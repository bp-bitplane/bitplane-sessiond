//! The `Module` seam (ADR 0023): every built-in module owns one namespace,
//! is probed once at startup, and is reached through the root router.

use std::any::Any;
use std::collections::HashMap;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;

use bitplane_protocol::{Capability, ErrorCode, Frame, Notification, RpcError};
use serde_json::Value;
use tokio::sync::{broadcast, mpsc};

use crate::env::EnvMap;

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

    fn handle(&self, call: Call) -> BoxFuture<'_, Result<Value, RpcError>>;

    /// An INPUT frame for one of this module's attachments.
    fn input(&self, _connection: &Connection, _attachment: u32, _data: Vec<u8>) {}
}

pub struct Call {
    pub method: String,
    pub params: Option<Value>,
    pub connection: Connection,
}

/// The connection a call or frame came from. Frames sent here reach only it.
#[derive(Clone)]
pub struct Connection {
    pub id: u64,
    pub frames: mpsc::UnboundedSender<Frame>,
}

pub struct ModuleContext {
    /// `<state>/<namespace>/`, created before `start`.
    pub state_dir: std::path::PathBuf,
    /// The login-shell environment captured at startup; what sessions and
    /// tools are spawned with.
    pub env: Arc<EnvMap>,
    pub events: Events,
    pub bus: Bus,
}

/// Broadcast events to every connection that completed `hello`.
#[derive(Clone)]
pub struct Events(pub(crate) broadcast::Sender<Notification>);

/// Change notifications between modules; never direct calls.
#[derive(Clone)]
pub struct Bus(pub(crate) broadcast::Sender<Arc<dyn Any + Send + Sync>>);

/// The namespace that owns INPUT frames.
const FRAME_OWNER: &str = "session";

pub struct Registry {
    entries: Vec<Entry>,
}

struct Entry {
    module: Arc<dyn Module>,
    availability: Availability,
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

impl Registry {
    pub fn start(
        modules: Vec<Arc<dyn Module>>,
        state_dir: &Path,
        env: Arc<EnvMap>,
        events: Events,
    ) -> std::io::Result<Self> {
        let bus = Bus(broadcast::channel(256).0);
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
                let state_dir = state_dir.join(module.namespace());
                std::fs::create_dir_all(&state_dir)?;
                module.start(ModuleContext {
                    state_dir,
                    env: env.clone(),
                    events: events.clone(),
                    bus: bus.clone(),
                });
            }
            entries.push(Entry {
                module,
                availability,
            });
        }
        Ok(Self { entries })
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
