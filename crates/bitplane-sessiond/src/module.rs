//! The `Module` seam (ADR 0023): every built-in module owns one namespace,
//! is probed once at startup, and is reached through the root router.

use std::any::Any;
use std::collections::HashMap;
use std::future::Future;
use std::path::Path;
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
    /// other modules, such as launching restored agent terminals, starts here.
    fn ready(&self) {}

    /// What the agent terminal `session` runs as its PTY child in `cwd`, an
    /// agent of `kind` resuming `link` (ADR 0029); `env` is the session's
    /// own. `None` from a module that runs no agents of that kind.
    fn launch(
        &self,
        _session: &str,
        _kind: &str,
        _env: &EnvMap,
        _link: Option<&AgentLink>,
        _cwd: &Path,
    ) -> Option<Result<Launch, LaunchError>> {
        None
    }

    /// The session's agent link as this module last set it; `None` when it
    /// has not, and the link the session was opened or restored with stands.
    fn agent_link(&self, _session: &str) -> Option<AgentLink> {
        None
    }

    /// The restored agent terminal `session` came back with `link` and no
    /// process (ADR 0027); its agent is listed as it was, exited.
    fn restored_exited(&self, _session: &str, _kind: &str, _link: &AgentLink) {}

    /// Where an agent session of `kind` this module knows started.
    fn agent_cwd(&self, _kind: &str, _agent_session: &str) -> Option<String> {
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

/// The agent session an agent terminal resumes, and where it started
/// (ADR 0031); persisted with the session (ADR 0027).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentLink {
    pub agent_session: String,
    pub cwd: String,
}

/// An agent terminal's PTY child: `argv[0]` is the agent's binary, and
/// `env` goes on top of the session's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Launch {
    pub argv: Vec<String>,
    pub env: EnvMap,
}

#[derive(Debug, Clone)]
pub enum LaunchError {
    /// The request itself is wrong; a `session.open` fails with it.
    Refused(RpcError),
    /// The agent can't be launched now; the session is `spawnFailed`.
    Failed(String),
}

/// Published on the bus whenever a module changed a session's agent link.
#[derive(Debug, Clone)]
pub struct AgentLinkChanged {
    pub session: String,
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
    pub launchers: Launchers,
}

/// Broadcast events to every connection that completed `hello`.
#[derive(Clone)]
pub struct Events(pub(crate) broadcast::Sender<Notification>);

/// Change notifications between modules; never direct calls.
#[derive(Clone)]
pub struct Bus(pub(crate) broadcast::Sender<Arc<dyn Any + Send + Sync>>);

/// Every available module, for what sessions get from them:
/// [`Module::launch`] and [`Module::agent_link`]. Set before any `ready`;
/// weak, as the modules hold it.
#[derive(Clone, Default)]
pub struct Launchers(Arc<OnceLock<Vec<Weak<dyn Module>>>>);

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

impl Launchers {
    /// The first module's [`Module::launch`]; with none running `kind`,
    /// the session can't be launched.
    pub fn launch(
        &self,
        session: &str,
        kind: &str,
        env: &EnvMap,
        link: Option<&AgentLink>,
        cwd: &Path,
    ) -> Result<Launch, LaunchError> {
        self.modules()
            .find_map(|module| module.launch(session, kind, env, link, cwd))
            .unwrap_or_else(|| Err(LaunchError::Failed(format!("unknown agent kind {kind}"))))
    }

    /// The session's agent link: the first module's, else `restored`.
    pub fn agent_link(&self, session: &str, restored: Option<&AgentLink>) -> Option<AgentLink> {
        self.modules()
            .find_map(|module| module.agent_link(session))
            .or_else(|| restored.cloned())
    }

    /// Every module's [`Module::restored_exited`].
    pub fn restored_exited(&self, session: &str, kind: &str, link: &AgentLink) {
        for module in self.modules() {
            module.restored_exited(session, kind, link);
        }
    }

    /// Where a known agent session of `kind` started, if any module knows it.
    pub fn agent_cwd(&self, kind: &str, agent_session: &str) -> Option<String> {
        self.modules()
            .find_map(|module| module.agent_cwd(kind, agent_session))
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
        let launchers = Launchers::default();
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
                    launchers: launchers.clone(),
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
        let _ = launchers
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
    /// then are the PTYs hung up (ADR 0031).
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
