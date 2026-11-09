//! The `agent.<kind>` modules: coding agents running inside daemon sessions,
//! tracked through hooks injected per session (ADRs 0029–0031). Each agent
//! kind is one [`AgentAdapter`]; this module owns everything kind-neutral.

pub mod claude;
pub mod hook;
pub mod wrapper;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use bitplane_protocol::RpcError;
use bitplane_protocol::agent::{
    Agent, AgentListResult, AgentRef, AgentResult, AgentSessionId, AgentState, Tracking,
    WaitingReason,
};
use bitplane_protocol::session::SessionId;
use serde_json::{Value, json};

use crate::env::EnvMap;
use crate::module::{
    Availability, BoxFuture, Bus, Call, Events, Instrumentation, Module, ModuleContext,
};
use crate::session::SessionClosed;

/// The env a hook needs to reach the daemon and speak for its session.
pub const HOOK_SOCKET_VAR: &str = "BITPLANE_HOOK_SOCKET";
pub const HOOK_TOKEN_VAR: &str = "BITPLANE_HOOK_TOKEN";
pub const SESSION_VAR: &str = "BITPLANE_SESSION_ID";

/// One agent kind's side of the module: how its sessions are wired up and
/// what its hook payloads mean.
pub trait AgentAdapter: Send + Sync + 'static {
    fn kind(&self) -> &'static str;

    /// `agent.<kind>`.
    fn namespace(&self) -> &'static str;

    /// Whether the agent is installed, looked up on the login-shell `PATH`.
    fn available(&self, env: &EnvMap) -> Availability;

    /// Prepares `dir`, the module's own state dir, for [`Self::instrument`];
    /// `exe` is the daemon binary the hooks run.
    fn install(&self, dir: &Path, exe: &Path) -> std::io::Result<()>;

    /// What a session's shell gets on top of the hook channel's env.
    fn instrument(&self, dir: &Path) -> Instrumentation;

    fn ingest(&self, payload: &Value) -> Option<Ingested>;

    /// The agents the agent's own registry lists as running.
    fn probe(&self) -> Vec<LiveAgent>;

    /// What a shell is typed to resume an agent session.
    fn resume_command(&self, agent_session: &str) -> String;
}

/// A hook payload, read: the agent session it is about and what happened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ingested {
    pub agent_session: AgentSessionId,
    pub event: AgentEvent,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentEvent {
    /// `source` is the agent's own word for why: startup, resume, clear,
    /// compact or fork for Claude Code.
    Started {
        cwd: String,
        source: String,
    },
    Busy,
    Waiting {
        reason: WaitingReason,
    },
    Idle {
        error: Option<String>,
    },
    Ended {
        reason: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveAgent {
    pub pid: u32,
    pub agent_session: Option<AgentSessionId>,
    pub cwd: String,
    pub state: AgentState,
}

pub struct Agents {
    adapter: Arc<dyn AgentAdapter>,
    exe: PathBuf,
    shared: OnceLock<Arc<Shared>>,
}

struct Shared {
    adapter: Arc<dyn AgentAdapter>,
    events: Events,
    dir: PathBuf,
    hook_socket: PathBuf,
    state: Mutex<State>,
    /// Set by shutdown; hooks arriving after it are dropped.
    stopped: AtomicBool,
    tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

#[derive(Default)]
struct State {
    /// The secret each session's hooks must present.
    tokens: HashMap<SessionId, String>,
    /// The agent session each session last started.
    current: HashMap<SessionId, AgentSessionId>,
    /// In start order.
    agents: Vec<Agent>,
}

impl Agents {
    /// The `agent.claude` module; its hooks run `exe hook`.
    pub fn claude(exe: PathBuf) -> Arc<dyn Module> {
        Arc::new(Self {
            adapter: Arc::new(claude::Claude),
            exe,
            shared: OnceLock::new(),
        })
    }

    fn shared(&self) -> &Arc<Shared> {
        self.shared.get().expect("started before any call")
    }
}

impl Module for Agents {
    fn namespace(&self) -> &'static str {
        self.adapter.namespace()
    }

    fn dependencies(&self) -> &'static [&'static str] {
        &["session"]
    }

    fn probe(&self, env: &EnvMap) -> Availability {
        self.adapter.available(env)
    }

    fn start(&self, context: ModuleContext) {
        if let Err(error) = self.adapter.install(&context.state_dir, &self.exe) {
            tracing::warn!(%error, dir = %context.state_dir.display(), "couldn't install the agent wrapper");
        }
        let shared = Arc::new(Shared {
            adapter: self.adapter.clone(),
            events: context.events,
            dir: context.state_dir,
            hook_socket: context.dirs.runtime.join("hook.sock"),
            state: Mutex::new(State::default()),
            stopped: AtomicBool::new(false),
            tasks: Mutex::new(Vec::new()),
        });
        let weak = Arc::downgrade(&shared);
        let mut tasks = Vec::new();
        match hook::bind(&shared.hook_socket) {
            Ok(listener) => {
                let weak = weak.clone();
                tasks.push(tokio::spawn(hook::serve(listener, move |message| {
                    if let Some(shared) = weak.upgrade() {
                        shared.hook(message);
                    }
                })));
            }
            Err(error) => {
                tracing::warn!(%error, socket = %shared.hook_socket.display(), "couldn't open the hook channel");
            }
        }
        tasks.push(tokio::spawn(follow_sessions(context.bus, weak)));
        *shared.tasks.lock().unwrap() = tasks;
        let _ = self.shared.set(shared);
    }

    fn instrument(&self, session: &str) -> Option<Instrumentation> {
        let shared = self.shared.get()?;
        let token = shared
            .state
            .lock()
            .unwrap()
            .tokens
            .entry(session.to_string())
            .or_insert_with(|| uuid::Uuid::new_v4().simple().to_string())
            .clone();
        let mut instrumentation = shared.adapter.instrument(&shared.dir);
        instrumentation.env.insert(
            HOOK_SOCKET_VAR.into(),
            shared.hook_socket.display().to_string(),
        );
        instrumentation.env.insert(HOOK_TOKEN_VAR.into(), token);
        Some(instrumentation)
    }

    fn handle(&self, call: Call) -> BoxFuture<'_, Result<Value, RpcError>> {
        Box::pin(async move {
            let shared = self.shared();
            let list = format!("{}.list", self.adapter.namespace());
            if call.method == list {
                let agents = shared.state.lock().unwrap().agents.clone();
                return Ok(json!(AgentListResult { agents }));
            }
            Err(RpcError::new(
                RpcError::METHOD_NOT_FOUND,
                format!("no method {}", call.method),
            ))
        })
    }

    fn shutdown(&self, _restore: bool) {
        let Some(shared) = self.shared.get() else {
            return;
        };
        shared.stopped.store(true, Ordering::SeqCst);
        for task in shared.tasks.lock().unwrap().drain(..) {
            task.abort();
        }
        let _ = std::fs::remove_file(&shared.hook_socket);
    }
}

impl Shared {
    /// A hook that presents its session's token; any other is dropped.
    fn hook(&self, message: hook::Message) {
        if self.stopped.load(Ordering::SeqCst) {
            return;
        }
        let mut state = self.state.lock().unwrap();
        if state.tokens.get(&message.session) != Some(&message.token) {
            tracing::debug!(session = %message.session, "dropped a hook with a wrong session or token");
            return;
        }
        let Some(Ingested {
            agent_session,
            event,
        }) = self.adapter.ingest(&message.payload)
        else {
            return;
        };
        match event {
            AgentEvent::Started { cwd, .. } => {
                self.started(&mut state, &message.session, agent_session, cwd)
            }
            AgentEvent::Busy => self.set_state(
                &mut state,
                &message.session,
                &agent_session,
                AgentState::Busy,
            ),
            AgentEvent::Waiting { reason } => self.set_state(
                &mut state,
                &message.session,
                &agent_session,
                AgentState::Waiting { reason },
            ),
            AgentEvent::Idle { .. } => self.set_state(
                &mut state,
                &message.session,
                &agent_session,
                AgentState::Idle,
            ),
            AgentEvent::Ended { .. } => self.set_state(
                &mut state,
                &message.session,
                &agent_session,
                AgentState::Exited,
            ),
        }
    }

    /// The session's agent is now `agent_session`; the one it replaces, if
    /// any, is removed (ADR 0031).
    fn started(&self, state: &mut State, session: &str, agent_session: String, cwd: String) {
        if let Some(previous) = state
            .current
            .insert(session.to_string(), agent_session.clone())
            .filter(|previous| *previous != agent_session)
        {
            state.agents.retain(|agent| agent.agent_session != previous);
            self.emit_removed(previous);
        }
        state
            .current
            .retain(|other, current| other == session || *current != agent_session);
        let agent = Agent {
            agent_session: agent_session.clone(),
            session: Some(session.to_string()),
            cwd,
            name: None,
            state: AgentState::Idle,
            tracking: Tracking::Hooks,
        };
        match state
            .agents
            .iter_mut()
            .find(|agent| agent.agent_session == agent_session)
        {
            Some(existing) => *existing = agent.clone(),
            None => state.agents.push(agent.clone()),
        }
        self.emit_changed(agent);
    }

    /// Only the session's own agent moves; a stray hook for another is dropped.
    fn set_state(&self, state: &mut State, session: &str, agent_session: &str, new: AgentState) {
        if state.current.get(session).map(String::as_str) != Some(agent_session) {
            return;
        }
        if let Some(agent) = state
            .agents
            .iter_mut()
            .find(|agent| agent.agent_session == agent_session)
            .filter(|agent| agent.state != new)
        {
            agent.state = new;
            self.emit_changed(agent.clone());
        }
    }

    /// Its agents stay listed with no session, exited: the hang-up ended them.
    fn session_closed(&self, session: &str) {
        let mut state = self.state.lock().unwrap();
        state.tokens.remove(session);
        state.current.remove(session);
        let closed: Vec<Agent> = state
            .agents
            .iter_mut()
            .filter(|agent| agent.session.as_deref() == Some(session))
            .map(|agent| {
                agent.session = None;
                agent.state = AgentState::Exited;
                agent.clone()
            })
            .collect();
        for agent in closed {
            self.emit_changed(agent);
        }
    }

    fn emit_changed(&self, agent: Agent) {
        self.events.emit(
            &format!("{}.changed", self.adapter.namespace()),
            json!(AgentResult { agent }),
        );
    }

    fn emit_removed(&self, agent_session: AgentSessionId) {
        self.events.emit(
            &format!("{}.removed", self.adapter.namespace()),
            json!(AgentRef { agent_session }),
        );
    }
}

async fn follow_sessions(bus: Bus, shared: Weak<Shared>) {
    let mut messages = bus.subscribe();
    loop {
        match messages.recv().await {
            Ok(message) => {
                let Some(closed) = message.downcast_ref::<SessionClosed>() else {
                    continue;
                };
                let Some(shared) = shared.upgrade() else {
                    return;
                };
                shared.session_closed(&closed.session);
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(missed)) => {
                tracing::warn!(missed, "the agent module fell behind on session changes");
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
        }
    }
}
