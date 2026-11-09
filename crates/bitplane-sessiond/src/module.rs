//! The `Module` seam (ADR 0023): every built-in module owns one namespace,
//! is probed once at startup, and is reached through the root router.

use std::any::Any;
use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use bitplane_protocol::{Capability, ErrorCode, Frame, Id, Notification, Response, RpcError};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::{broadcast, mpsc};

use crate::env::EnvMap;
use crate::paths::Dirs;

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[derive(Debug, Clone)]
pub enum Availability {
    Available { info: Option<Value> },
    Unavailable { reason: String },
}

pub trait Module: Send + Sync + 'static {
    fn namespace(&self) -> &'static str;

    /// Namespaces this module needs; it is unavailable when any of them is.
    fn dependencies(&self) -> &'static [&'static str] {
        &[]
    }

    /// `env` is the login-shell environment, so tools are looked up on the
    /// user's own `PATH`.
    fn probe(&self, env: &EnvMap) -> Availability;

    /// Called once, after probing, on available modules only.
    fn start(&self, _context: ModuleContext) {}

    /// Called once every available module has started; work that reaches
    /// other modules, such as spawning instrumented shells, starts here.
    fn ready(&self) {}

    /// What every shell a session starts gets from this module (ADR 0029).
    fn instrument(&self, _session: &str) -> Option<Instrumentation> {
        None
    }

    /// Takes over resuming `link` in the session's next shell (ADR 0031):
    /// what to type, and the link as it now stands. `None` from a module
    /// that runs no agents.
    fn resume(&self, _session: &str, _link: &AgentLink) -> Option<Result<Resumed, RpcError>> {
        None
    }

    /// The session's agent link as this module last decided it; `None` when
    /// it has not, and the link the session was restored with stands.
    fn agent_link(&self, _session: &str) -> Option<Option<AgentLink>> {
        None
    }

    fn handle(&self, call: Call) -> BoxFuture<'_, Result<Value, RpcError>>;

    /// An INPUT frame for one of this module's attachments.
    fn input(&self, _connection: &Connection, _attachment: u32, _data: Vec<u8>) {}

    /// The connection closed; frames sent to it go nowhere from now on.
    fn disconnected(&self, _connection: u64) {}

    /// The daemon is about to exit: save what the next instance restores
    /// (when `restore`, ADR 0027) and stop taking changes. Called at most
    /// once, on every module before any [`Module::terminate`].
    fn shutdown(&self, _restore: bool) {}

    /// Then end the processes the module owns.
    fn terminate(&self, _restore: bool) {}
}

pub struct Call {
    pub method: String,
    pub params: Option<Value>,
    pub connection: Connection,
    pub reply: Reply,
}

/// The reply to one request. The connection sends the handler's result
/// unless the handler sent it first, so frames sent after it follow it.
#[derive(Clone)]
pub struct Reply {
    id: Arc<Mutex<Option<Id>>>,
    frames: mpsc::UnboundedSender<Frame>,
}

/// The connection a call or frame came from. Frames sent here reach only it.
#[derive(Clone)]
pub struct Connection {
    pub id: u64,
    pub frames: mpsc::UnboundedSender<Frame>,
}

/// The agent session a session's shell resumes, and where it started
/// (ADR 0031); persisted with the session (ADR 0027).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentLink {
    pub agent_session: String,
    pub cwd: String,
}

/// What a session's shell is typed to resume its agent session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resumed {
    pub link: AgentLink,
    pub command: String,
}

/// Published on the bus whenever a module changed a session's agent link.
#[derive(Debug, Clone)]
pub struct AgentLinkChanged {
    pub session: String,
}

/// Env set and `PATH` entries prepended for a session's shell.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Instrumentation {
    pub env: EnvMap,
    pub path_prepend: Vec<PathBuf>,
}

pub struct ModuleContext {
    /// This instance's directories.
    pub dirs: Dirs,
    /// `<state>/<namespace>/`, created before `start`.
    pub state_dir: std::path::PathBuf,
    /// The login-shell environment captured at startup; what sessions and
    /// tools are spawned with.
    pub env: Arc<EnvMap>,
    pub events: Events,
    pub bus: Bus,
    pub instruments: Instruments,
}

/// Broadcast events to every connection that completed `hello`.
#[derive(Clone)]
pub struct Events(pub(crate) broadcast::Sender<Notification>);

/// Change notifications between modules; never direct calls.
#[derive(Clone)]
pub struct Bus(pub(crate) broadcast::Sender<Arc<dyn Any + Send + Sync>>);

/// Every available module, for what a session's shells get from them:
/// [`Module::instrument`], [`Module::resume`] and [`Module::agent_link`].
/// Set before any `ready`; weak, as the modules hold it.
#[derive(Clone, Default)]
pub struct Instruments(Arc<OnceLock<Vec<Weak<dyn Module>>>>);

/// The namespace that owns INPUT frames.
const FRAME_OWNER: &str = "session";

pub struct Registry {
    entries: Vec<Entry>,
    shut_down: AtomicBool,
}

struct Entry {
    module: Arc<dyn Module>,
    availability: Availability,
}

impl Reply {
    /// `id` is `None` for a notification, which gets no reply.
    pub fn new(id: Option<Id>, frames: mpsc::UnboundedSender<Frame>) -> Self {
        Self {
            id: Arc::new(Mutex::new(id)),
            frames,
        }
    }

    /// Sends the reply unless it was sent already.
    pub fn send(&self, outcome: Result<Value, RpcError>) {
        let Some(id) = self.id.lock().unwrap().take() else {
            return;
        };
        let response = match outcome {
            Ok(result) => Response::ok(id, result),
            Err(error) => Response::err(Some(id), error),
        };
        let bytes = serde_json::to_vec(&response).expect("protocol messages serialize");
        let _ = self.frames.send(Frame::Json(bytes));
    }
}

impl Events {
    pub fn emit(&self, method: &str, params: Value) {
        let _ = self.0.send(Notification::new(method, Some(params)));
    }
}

impl Bus {
    pub fn publish<T: Any + Send + Sync>(&self, message: T) {
        let _ = self.0.send(Arc::new(message));
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Arc<dyn Any + Send + Sync>> {
        self.0.subscribe()
    }
}

impl Instruments {
    /// Adds what every module instruments a session's shell with to its env,
    /// in module order.
    pub fn apply(&self, session: &str, env: &mut EnvMap) {
        let mut path = Vec::new();
        for module in self.modules() {
            let Some(Instrumentation {
                env: added,
                path_prepend,
            }) = module.instrument(session)
            else {
                continue;
            };
            env.extend(added);
            path.extend(path_prepend.iter().map(|dir| dir.display().to_string()));
        }
        if path.is_empty() {
            return;
        }
        path.extend(env.get("PATH").filter(|path| !path.is_empty()).cloned());
        env.insert("PATH".into(), path.join(":"));
    }

    /// The first module's [`Module::resume`]; `None` when no module runs agents.
    pub fn resume(&self, session: &str, link: &AgentLink) -> Option<Result<Resumed, RpcError>> {
        self.modules()
            .find_map(|module| module.resume(session, link))
    }

    /// The session's agent link: the first module's decision, else `restored`.
    pub fn agent_link(&self, session: &str, restored: Option<&AgentLink>) -> Option<AgentLink> {
        self.modules()
            .find_map(|module| module.agent_link(session))
            .unwrap_or_else(|| restored.cloned())
    }

    fn modules(&self) -> impl Iterator<Item = Arc<dyn Module>> + '_ {
        self.0.get().into_iter().flatten().filter_map(Weak::upgrade)
    }
}

impl Registry {
    pub fn start(
        modules: Vec<Arc<dyn Module>>,
        dirs: &Dirs,
        env: Arc<EnvMap>,
        events: Events,
    ) -> std::io::Result<Self> {
        let bus = Bus(broadcast::channel(256).0);
        let instruments = Instruments::default();
        let mut availability: HashMap<&'static str, Availability> = modules
            .iter()
            .map(|module| (module.namespace(), module.probe(&env)))
            .collect();
        resolve_dependencies(&modules, &mut availability);

        let mut entries = Vec::with_capacity(modules.len());
        for module in modules {
            let availability = availability
                .remove(module.namespace())
                .expect("probed above");
            if let Availability::Available { .. } = availability {
                let state_dir = dirs.state.join(module.namespace());
                std::fs::create_dir_all(&state_dir)?;
                module.start(ModuleContext {
                    dirs: dirs.clone(),
                    state_dir,
                    env: env.clone(),
                    events: events.clone(),
                    bus: bus.clone(),
                    instruments: instruments.clone(),
                });
            }
            entries.push(Entry {
                module,
                availability,
            });
        }
        let available: Vec<Arc<dyn Module>> = entries
            .iter()
            .filter(|entry| matches!(entry.availability, Availability::Available { .. }))
            .map(|entry| entry.module.clone())
            .collect();
        let _ = instruments
            .0
            .set(available.iter().map(Arc::downgrade).collect());
        for module in available {
            module.ready();
        }
        Ok(Self {
            entries,
            shut_down: Default::default(),
        })
    }

    pub fn capabilities(&self) -> Vec<Capability> {
        self.entries
            .iter()
            .map(|entry| {
                let module = entry.module.namespace().to_string();
                match &entry.availability {
                    Availability::Available { info } => Capability {
                        module,
                        available: true,
                        reason: None,
                        info: info.clone(),
                    },
                    Availability::Unavailable { reason } => Capability {
                        module,
                        available: false,
                        reason: Some(reason.clone()),
                        info: None,
                    },
                }
            })
            .collect()
    }

    pub async fn dispatch(&self, call: Call) -> Result<Value, RpcError> {
        let entry = self.route(&call.method).ok_or_else(|| {
            RpcError::new(
                RpcError::METHOD_NOT_FOUND,
                format!("no method {}", call.method),
            )
        })?;
        match &entry.availability {
            Availability::Available { .. } => entry.module.handle(call).await,
            Availability::Unavailable { reason } => Err(RpcError::protocol(
                ErrorCode::ModuleUnavailable,
                format!(
                    "module {} is unavailable: {reason}",
                    entry.module.namespace()
                ),
                Some(serde_json::json!({ "module": entry.module.namespace(), "reason": reason })),
            )),
        }
    }

    pub fn input(&self, connection: &Connection, attachment: u32, data: Vec<u8>) {
        if let Some(entry) = self.entries.iter().find(|entry| {
            entry.module.namespace() == FRAME_OWNER
                && matches!(entry.availability, Availability::Available { .. })
        }) {
            entry.module.input(connection, attachment, data);
        }
    }

    pub fn disconnected(&self, connection: u64) {
        for entry in &self.entries {
            if let Availability::Available { .. } = entry.availability {
                entry.module.disconnected(connection);
            }
        }
    }

    /// Every available module's [`Module::shutdown`], then every one's
    /// [`Module::terminate`], once; later calls do nothing. Sessions persist
    /// the agent links before the agent module stops taking hooks, and only
    /// then are the shells hung up (ADR 0031).
    pub fn shutdown(&self, restore: bool) {
        if self.shut_down.swap(true, Ordering::SeqCst) {
            return;
        }
        let available = || {
            self.entries
                .iter()
                .filter(|entry| matches!(entry.availability, Availability::Available { .. }))
        };
        for entry in available() {
            entry.module.shutdown(restore);
        }
        for entry in available() {
            entry.module.terminate(restore);
        }
    }

    /// The module with the longest namespace that prefixes `method`.
    fn route(&self, method: &str) -> Option<&Entry> {
        self.entries
            .iter()
            .filter(|entry| {
                method
                    .strip_prefix(entry.module.namespace())
                    .is_some_and(|rest| rest.starts_with('.'))
            })
            .max_by_key(|entry| entry.module.namespace().len())
    }
}

fn resolve_dependencies(
    modules: &[Arc<dyn Module>],
    availability: &mut HashMap<&'static str, Availability>,
) {
    loop {
        let mut changed = false;
        for module in modules {
            if !matches!(
                availability[module.namespace()],
                Availability::Available { .. }
            ) {
                continue;
            }
            let missing = module.dependencies().iter().find_map(|dependency| {
                match availability.get(dependency) {
                    Some(Availability::Available { .. }) => None,
                    Some(Availability::Unavailable { reason }) => Some(format!(
                        "needs {dependency}, which is unavailable: {reason}"
                    )),
                    None => Some(format!("needs {dependency}, which this daemon lacks")),
                }
            });
            if let Some(reason) = missing {
                availability.insert(module.namespace(), Availability::Unavailable { reason });
                changed = true;
            }
        }
        if !changed {
            return;
        }
    }
}
