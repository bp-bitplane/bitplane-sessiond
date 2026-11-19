//! The `plane` module: cc's sidebar model from `bp list`, the planes
//! directory and its drafts, re-read when the planes directory changes, and
//! any `bp` request passed through (ADR 0033), and the `plane.create` job
//! kind (ADR 0034). Every call is its own `bp`;
//! only the model's re-reads are coalesced, so they land in order.

mod bp;
mod create;
mod model;

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use bitplane_protocol::plane::{
    PlaneCreateDraftParams, PlaneDispatchParams, PlaneDispatchResult, PlaneInfo, PlaneListError,
    PlaneProjectsResult, PlanesModel,
};
use bitplane_protocol::{ErrorCode, RpcError, method};
use notify_debouncer_mini::notify::{RecommendedWatcher, RecursiveMode};
use notify_debouncer_mini::{DebounceEventResult, Debouncer, new_debouncer};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::sync::Notify;

pub use bp::NO_BP;

use bp::{Bp, Failure};
use model::{resolve_planes_dir, validate_plane_id};

use crate::env::EnvMap;
use crate::job::{Jobs, now_millis};
use crate::module::{Availability, BoxFuture, Call, Events, Module, ModuleContext};
use crate::watch::DEBOUNCE;

const NAMESPACE: &str = "plane";

const LIST_REQUEST: &str = r#"{"action":"plane_list","health":"cheap"}"#;
const PROJECTS_REQUEST: &str = r#"{"action":"project_list"}"#;

#[derive(Default)]
pub struct Plane {
    bp: OnceLock<Arc<Bp>>,
    shared: OnceLock<Arc<Shared>>,
}

struct Shared {
    bp: Arc<Bp>,
    events: Events,
    jobs: Arc<Jobs>,
    /// One permit means "list again"; notifications while a list runs
    /// coalesce into one more run.
    wake: Notify,
    state: Mutex<State>,
    watcher: Mutex<Option<Watcher>>,
}

struct State {
    model: PlanesModel,
    rows: model::Rows,
}

struct Watcher {
    debouncer: Debouncer<RecommendedWatcher>,
    watched: BTreeSet<PathBuf>,
}

impl Plane {
    pub fn module() -> Arc<dyn Module> {
        Arc::new(Self::default())
    }

    fn shared(&self) -> &Arc<Shared> {
        self.shared.get().expect("started before any call")
    }

    async fn projects(&self) -> Result<Value, RpcError> {
        let response = self.run(PROJECTS_REQUEST).await?;
        let projects =
            model::projects(&response).map_err(|cause| Failure::NotRun(cause).into_rpc())?;
        Ok(json!(PlaneProjectsResult { projects }))
    }

    async fn create_draft(&self, params: PlaneCreateDraftParams) -> Result<Value, RpcError> {
        validate_plane_id(&params.id)
            .map_err(|message| RpcError::new(RpcError::INVALID_PARAMS, message))?;
        let shared = self.shared().clone();
        let planes_dir = shared.bp.planes_dir().to_path_buf();
        let id = params.id.clone();
        tokio::task::spawn_blocking(move || make_draft(&planes_dir, &id))
            .await
            .map_err(|error| RpcError::new(RpcError::INTERNAL_ERROR, error.to_string()))??;
        shared.wake.notify_one();
        Ok(json!({}))
    }

    async fn dispatch(&self, params: PlaneDispatchParams) -> Result<Value, RpcError> {
        let response = self
            .shared()
            .bp
            .run(&params.request)
            .await
            .map_err(Failure::into_rpc)?;
        Ok(json!(PlaneDispatchResult { response }))
    }

    async fn run(&self, request: &str) -> Result<Value, RpcError> {
        let request: Value = serde_json::from_str(request).expect("a valid request");
        self.shared()
            .bp
            .run(&request)
            .await
            .map_err(Failure::into_rpc)
    }
}

impl Module for Plane {
    fn namespace(&self) -> &'static str {
        NAMESPACE
    }

    fn probe(&self, env: &EnvMap) -> Availability {
        let Some(program) = Bp::find(env) else {
            return Availability::Unavailable {
                reason: NO_BP.into(),
            };
        };
        let bp = Bp::new(program, env, resolve_planes_dir(env));
        match bp.version() {
            Ok(version) => {
                let _ = self.bp.set(Arc::new(bp));
                Availability::Available {
                    info: Some(json!(PlaneInfo { bp: version })),
                }
            }
            Err(reason) => Availability::Unavailable { reason },
        }
    }

    fn start(&self, context: ModuleContext) {
        let bp = self.bp.get().expect("probed available").clone();
        let planes_dir = bp.planes_dir().to_path_buf();
        let shared = Arc::new(Shared {
            bp,
            events: context.events,
            jobs: context.jobs,
            wake: Notify::new(),
            state: Mutex::new(State {
                model: PlanesModel {
                    planes_dir: planes_dir.to_string_lossy().into(),
                    planes_dir_exists: planes_dir.is_dir(),
                    refreshed_at: None,
                    list_error: None,
                    planes: Vec::new(),
                },
                rows: model::Rows::default(),
            }),
            watcher: Mutex::new(None),
        });
        let weak = Arc::downgrade(&shared);
        match new_debouncer(DEBOUNCE, move |result: DebounceEventResult| {
            if let (Ok(events), Some(shared)) = (result, weak.upgrade())
                && !events.is_empty()
            {
                shared.wake.notify_one();
            }
        }) {
            Ok(debouncer) => {
                *shared.watcher.lock().unwrap() = Some(Watcher {
                    debouncer,
                    watched: BTreeSet::new(),
                });
            }
            Err(error) => {
                tracing::warn!(%error, "no planes-dir watcher; plane.refresh still works")
            }
        }
        let _ = self.shared.set(shared.clone());
        shared.wake.notify_one();
        tokio::spawn(async move {
            loop {
                shared.wake.notified().await;
                shared.list().await;
            }
        });
    }

    fn handle(&self, call: Call) -> BoxFuture<'_, Result<Value, RpcError>> {
        Box::pin(async move {
            match call.method.as_str() {
                method::PLANE_LIST => Ok(json!(self.shared().state.lock().unwrap().model)),
                method::PLANE_REFRESH => {
                    self.shared().wake.notify_one();
                    Ok(json!({}))
                }
                method::PLANE_PROJECTS => self.projects().await,
                method::PLANE_CREATE_DRAFT => self.create_draft(params(&call)?).await,
                method::PLANE_DISPATCH => self.dispatch(params(&call)?).await,
                method::PLANE_CREATE => create::start(self.shared(), &call, params(&call)?),
                _ => Err(RpcError::new(
                    RpcError::METHOD_NOT_FOUND,
                    format!("no method {}", call.method),
                )),
            }
        })
    }
}

impl Shared {
    /// Runs `bp list` and scans for drafts, then broadcasts the whole model.
    async fn list(&self) {
        let request: Value = serde_json::from_str(LIST_REQUEST).expect("a valid request");
        let listed = match self.bp.run(&request).await {
            Ok(response) => model::listed(&response).map_err(|message| PlaneListError {
                message,
                envelope: None,
            }),
            Err(Failure::NotRun(cause)) => Err(PlaneListError {
                message: format!("bp could not be run: {cause}"),
                envelope: None,
            }),
            Err(Failure::Refused(envelope)) => Err(PlaneListError {
                message: envelope
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("bp list was refused")
                    .into(),
                envelope: Some(envelope),
            }),
        };
        let planes_dir = self.bp.planes_dir().to_path_buf();
        let scan_dir = planes_dir.clone();
        let drafts = tokio::task::spawn_blocking(move || model::scan_drafts(&scan_dir))
            .await
            .unwrap_or_default();

        let (model, watch) = {
            let mut state = self.state.lock().unwrap();
            let list_error = match listed {
                Ok(rows) => {
                    state.rows.listed = rows;
                    None
                }
                Err(error) => Some(error),
            };
            let planes = state.rows.merge(drafts);
            let watch = model::watch_paths(&planes_dir, &planes);
            state.model = PlanesModel {
                planes_dir: planes_dir.to_string_lossy().into(),
                planes_dir_exists: planes_dir.is_dir(),
                refreshed_at: Some(now_millis()),
                list_error,
                planes,
            };
            (state.model.clone(), watch)
        };
        if let Some(watcher) = self.watcher.lock().unwrap().as_mut() {
            watcher.set_paths(watch);
        }
        self.events.emit(method::PLANE_CHANGED, json!(model));
    }
}

impl Watcher {
    /// Watches exactly `paths`, adding and removing the difference.
    fn set_paths(&mut self, paths: BTreeSet<PathBuf>) {
        let watcher = self.debouncer.watcher();
        for gone in self.watched.difference(&paths) {
            let _ = watcher.unwatch(gone);
        }
        let mut watched = BTreeSet::new();
        for path in paths {
            if self.watched.contains(&path)
                || watcher.watch(&path, RecursiveMode::NonRecursive).is_ok()
            {
                watched.insert(path);
            }
        }
        self.watched = watched;
    }
}

fn make_draft(planes_dir: &std::path::Path, id: &str) -> Result<(), RpcError> {
    let failed = |error: std::io::Error, path: &std::path::Path| {
        RpcError::new(
            RpcError::INTERNAL_ERROR,
            format!("couldn't create {}: {error}", path.display()),
        )
    };
    std::fs::create_dir_all(planes_dir).map_err(|error| failed(error, planes_dir))?;
    let dir = planes_dir.join(id);
    std::fs::create_dir(&dir).map_err(|error| match error.kind() {
        std::io::ErrorKind::AlreadyExists => RpcError::protocol(
            ErrorCode::PlaneExists,
            format!("{id} already exists"),
            Some(json!({ "id": id })),
        ),
        _ => failed(error, &dir),
    })
}

fn params<T: DeserializeOwned>(call: &Call) -> Result<T, RpcError> {
    serde_json::from_value(call.params.clone().unwrap_or(Value::Null))
        .map_err(|error| RpcError::new(RpcError::INVALID_PARAMS, error.to_string()))
}
