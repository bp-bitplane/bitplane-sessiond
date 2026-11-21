//! The `fs` module: listing, reading and watching any path the daemon's OS
//! account can read (ADR 0032). Watches are non-recursive: a path is one
//! folder.

mod list;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use bitplane_protocol::fs::{
    Blob, FsChanged, FsListResult, FsPathParams, FsReadParams, READ_LIMIT, WatchParams,
};
use bitplane_protocol::{ErrorCode, RpcError, method};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::env::EnvMap;
use crate::git_cli::Git;
use crate::module::{Availability, BoxFuture, Call, Module, ModuleContext};
use crate::watch::{self, Hub, RecursiveMode, SWEEP, Target};

pub use list::{GIT_DIR, containing_repo, in_git_dir};

const NAMESPACE: &str = "fs";

#[derive(Default)]
pub struct Fs {
    hub: OnceLock<Arc<Hub>>,
    git: OnceLock<Option<Git>>,
}

impl Fs {
    pub fn module() -> Arc<dyn Module> {
        Arc::new(Self::default())
    }

    fn hub(&self) -> &Arc<Hub> {
        self.hub.get().expect("probed before any call")
    }

    fn git(&self) -> Option<&Git> {
        self.git.get().and_then(Option::as_ref)
    }

    async fn list(&self, params: FsPathParams) -> Result<Value, RpcError> {
        let path = absolute(&params.path)?;
        let entries = list::list(&path, self.git())
            .await
            .ok_or_else(|| not_found(&params.path))?;
        Ok(json!(FsListResult { entries }))
    }

    async fn read(&self, params: FsReadParams) -> Result<Value, RpcError> {
        let path = absolute(&params.path)?;
        let limit = params.max_bytes.unwrap_or(READ_LIMIT).min(READ_LIMIT);
        let blob = tokio::task::spawn_blocking(move || read_blob(&path, limit))
            .await
            .map_err(|error| RpcError::new(RpcError::INTERNAL_ERROR, error.to_string()))?
            .ok_or_else(|| not_found(&params.path))?;
        Ok(json!(blob))
    }

    fn watch(&self, call: &Call, params: WatchParams) -> Result<(), RpcError> {
        let paths: BTreeSet<String> = params.paths.into_iter().collect();
        let mut targets = Vec::new();
        let mut gone = Vec::new();
        for path in paths {
            match absolute(&path)?.canonicalize() {
                Ok(key) if key.is_dir() => targets.push((
                    path,
                    Target {
                        watch: vec![(key.clone(), RecursiveMode::NonRecursive)],
                        key,
                    },
                )),
                _ => gone.push(path),
            }
        }
        let added = self.hub().replace(&call.connection, targets);
        call.reply.send(Ok(json!({})));
        for path in gone {
            watch::send(
                &call.connection.frames,
                method::FS_CHANGED,
                FsChanged { path, gone: true },
            );
        }
        for subscriber in added {
            let path = subscriber.path.clone();
            subscriber.send(method::FS_CHANGED, FsChanged { path, gone: false });
        }
        Ok(())
    }
}

impl Module for Fs {
    fn namespace(&self) -> &'static str {
        NAMESPACE
    }

    fn probe(&self, env: &EnvMap) -> Availability {
        let _ = self.git.set(Git::find(env));
        match Hub::new() {
            Ok(hub) => {
                let _ = self.hub.set(hub);
                Availability::Available { info: None }
            }
            Err(error) => Availability::Unavailable {
                reason: format!("couldn't start a file watcher: {error}"),
            },
        }
    }

    fn start(&self, _context: ModuleContext) {
        let hub = self.hub().clone();
        let batches = hub.batches().expect("started once");
        tokio::spawn(follow(hub, batches));
    }

    fn handle(&self, call: Call) -> BoxFuture<'_, Result<Value, RpcError>> {
        Box::pin(async move {
            match call.method.as_str() {
                method::FS_LIST => self.list(params(&call)?).await,
                method::FS_READ => self.read(params(&call)?).await,
                method::FS_WATCH => {
                    self.watch(&call, params(&call)?)?;
                    Ok(json!({}))
                }
                _ => Err(RpcError::new(
                    RpcError::METHOD_NOT_FOUND,
                    format!("no method {}", call.method),
                )),
            }
        })
    }

    fn disconnected(&self, connection: u64) {
        if let Some(hub) = self.hub.get() {
            hub.disconnect(connection);
        }
    }
}

/// Turns OS events into `fs.changed` for the folders they touch, and ends
/// the watch of a folder that is gone.
async fn follow(hub: Arc<Hub>, mut batches: mpsc::UnboundedReceiver<Vec<PathBuf>>) {
    let mut sweep = tokio::time::interval(SWEEP);
    loop {
        tokio::select! {
            batch = batches.recv() => {
                let Some(paths) = batch else { return };
                for key in hub.keys() {
                    let touched = paths
                        .iter()
                        .any(|path| path == &key || path.parent() == Some(key.as_path()));
                    if !touched {
                        continue;
                    }
                    if key.is_dir() {
                        for subscriber in hub.subscribers(&key) {
                            let path = subscriber.path.clone();
                            subscriber.send(method::FS_CHANGED, FsChanged { path, gone: false });
                        }
                    } else {
                        end(&hub, &key);
                    }
                }
            }
            _ = sweep.tick() => {
                for key in hub.keys().into_iter().filter(|key| !key.is_dir()) {
                    end(&hub, &key);
                }
            }
        }
    }
}

fn end(hub: &Hub, key: &Path) {
    for subscriber in hub.remove(key) {
        let path = subscriber.path.clone();
        subscriber.send(method::FS_CHANGED, FsChanged { path, gone: true });
    }
}

fn read_blob(path: &Path, limit: u64) -> Option<Blob> {
    let meta = std::fs::metadata(path).ok().filter(|meta| meta.is_file())?;
    let size = meta.len();
    if size > limit {
        return Some(Blob { size, data: None });
    }
    let bytes = std::fs::read(path).ok()?;
    Some(Blob {
        size: bytes.len() as u64,
        data: Some(STANDARD.encode(bytes)),
    })
}

fn params<T: DeserializeOwned>(call: &Call) -> Result<T, RpcError> {
    serde_json::from_value(call.params.clone().unwrap_or(Value::Null))
        .map_err(|error| RpcError::new(RpcError::INVALID_PARAMS, error.to_string()))
}

fn absolute(path: &str) -> Result<PathBuf, RpcError> {
    let path = PathBuf::from(path);
    if path.is_absolute() {
        Ok(path)
    } else {
        Err(RpcError::new(
            RpcError::INVALID_PARAMS,
            format!("{} is not an absolute path", path.display()),
        ))
    }
}

fn not_found(path: &str) -> RpcError {
    RpcError::protocol(
        ErrorCode::NotFound,
        format!("{path} does not exist"),
        Some(json!({ "path": path })),
    )
}
