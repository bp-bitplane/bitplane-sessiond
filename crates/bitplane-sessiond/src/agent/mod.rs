//! The `agent.<kind>` modules: coding agents running inside daemon sessions,
//! tracked through hooks injected per session and the agent's own polled
//! registry (ADRs 0029–0031). Each agent kind is one [`AgentAdapter`]; this
//! module owns everything kind-neutral.

pub mod claude;
pub mod hook;
mod poll;
mod process;
pub mod wrapper;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

use bitplane_protocol::RpcError;
use bitplane_protocol::agent::{
    Agent, AgentError, AgentListResult, AgentRef, AgentResult, AgentSessionId, AgentState,
    Tracking, WaitingReason,
};
use bitplane_protocol::session::SessionId;
use serde_json::{Value, json};

use crate::env::EnvMap;
use crate::module::{
    Availability, BoxFuture, Bus, Call, Events, Instrumentation, Module, ModuleContext,
};
use crate::session::{SessionClosed, SessionForeground};

/// The env a hook needs to reach the daemon and speak for its session.
pub const HOOK_SOCKET_VAR: &str = "BITPLANE_HOOK_SOCKET";
pub const HOOK_TOKEN_VAR: &str = "BITPLANE_HOOK_TOKEN";
pub const SESSION_VAR: &str = "BITPLANE_SESSION_ID";
/// How many graces past its due a candidate the registry has not listed is
/// still asked about: an agent registers some time after it starts.
const CANDIDATE_GRACES: u32 = 4;

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

    /// The agents the agent's own registry lists as running, host-wide;
    /// `None` when the registry could not be read. `env` is the login env.
    fn probe(&self, env: &EnvMap) -> Option<Vec<LiveAgent>>;

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
        error: Option<AgentError>,
    },
    Ended {
        reason: Option<String>,
    },
}

/// One running agent as its registry lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveAgent {
    pub pid: u32,
    pub agent_session: Option<AgentSessionId>,
    pub name: Option<String>,
    pub cwd: String,
    /// `None` for a status this build does not know.
    pub state: Option<AgentState>,
    /// Moved out of its terminal into the agent's background supervisor.
    pub background: bool,
}

/// How often the registry is polled, and how long a session's foreground
/// may run the agent before missing hooks mean `tracking: poll` (ADR 0030).
#[derive(Debug, Clone, Copy)]
pub struct Timing {
    pub poll: Duration,
    pub grace: Duration,
}

pub struct Agents {
    adapter: Arc<dyn AgentAdapter>,
    exe: PathBuf,
    timing: Timing,
    shared: OnceLock<Arc<Shared>>,
}

struct Shared {
    adapter: Arc<dyn AgentAdapter>,
    events: Events,
    env: Arc<EnvMap>,
    timing: Timing,
    dir: PathBuf,
    hook_socket: PathBuf,
    state: Mutex<State>,
    /// Tells the poller its schedule may have changed.
    wake: Arc<tokio::sync::Notify>,
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
    agents: Vec<Tracked>,
    /// Each session's shell and terminal foreground, as last published.
    processes: HashMap<SessionId, Processes>,
    /// Sessions whose foreground left the shell with no live agent in them.
    candidates: HashMap<SessionId, Candidate>,
    /// When each session's last hook was handled.
    hooked: HashMap<SessionId, Instant>,
    last_probe: Option<Instant>,
}

struct Tracked {
    agent: Agent,
    /// Learnt from the registry.
    pid: Option<u32>,
    since: Instant,
}

/// A foreground that may be an agent whose hooks did not load: the registry
/// is asked from `due`, and given until `until` to list it there.
#[derive(Debug, Clone, Copy)]
struct Candidate {
    due: Instant,
    until: Instant,
}

#[derive(Debug, Clone, Default)]
struct Processes {
    shell: Option<u32>,
    group: Option<u32>,
    name: Option<String>,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            poll: Duration::from_secs(3),
            grace: Duration::from_secs(2),
        }
    }
}

impl Agents {
    /// The `agent.claude` module; its hooks run `exe hook`.
    pub fn claude(exe: PathBuf) -> Arc<dyn Module> {
        Self::claude_with(exe, Timing::default())
    }

    pub fn claude_with(exe: PathBuf, timing: Timing) -> Arc<dyn Module> {
        Arc::new(Self {
            adapter: Arc::new(claude::Claude),
            exe,
            timing,
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
            env: context.env,
            timing: self.timing,
            dir: context.state_dir,
            hook_socket: context.dirs.runtime.join("hook.sock"),
            state: Mutex::new(State::default()),
            wake: Arc::default(),
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
        tasks.push(tokio::spawn(follow_sessions(context.bus, weak.clone())));
        tasks.push(tokio::spawn(poll::run(weak, shared.wake.clone())));
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
                let agents = shared
                    .state
                    .lock()
                    .unwrap()
                    .agents
                    .iter()
                    .map(|tracked| tracked.agent.clone())
                    .collect();
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
        state.hooked.insert(message.session.clone(), Instant::now());
        let Some(Ingested {
            agent_session,
            event,
        }) = self.adapter.ingest(&message.payload)
        else {
            return;
        };
        let session = message.session.as_str();
        match event {
            AgentEvent::Started { cwd, .. } => {
                let agent = Agent {
                    agent_session,
                    session: Some(session.to_string()),
                    cwd,
                    name: None,
                    state: AgentState::Idle,
                    last_error: None,
                    tracking: Tracking::Hooks,
                };
                self.start_agent(&mut state, agent, None);
            }
            AgentEvent::Busy => self.update(&mut state, session, &agent_session, |agent| {
                agent.state = AgentState::Busy;
                agent.last_error = None;
            }),
            AgentEvent::Waiting { reason } => {
                self.update(&mut state, session, &agent_session, |agent| {
                    agent.state = AgentState::Waiting { reason }
                })
            }
            AgentEvent::Idle { error } => {
                self.update(&mut state, session, &agent_session, |agent| {
                    agent.state = AgentState::Idle;
                    if error.is_some() {
                        agent.last_error = error;
                    }
                })
            }
            AgentEvent::Ended { .. } => self.update(&mut state, session, &agent_session, |agent| {
                agent.state = AgentState::Exited
            }),
        }
        self.wake.notify_one();
    }

    /// `agent` is now its session's agent; the one it replaces, if any, is
    /// removed (ADR 0031). The same agent session started again keeps what
    /// the registry and the last failed turn said about it.
    fn start_agent(&self, state: &mut State, mut agent: Agent, pid: Option<u32>) {
        let session = agent.session.clone().expect("started in a session");
        let id = agent.agent_session.clone();
        state.candidates.remove(&session);
        if let Some(previous) = state
            .current
            .insert(session.clone(), id.clone())
            .filter(|previous| *previous != id)
        {
            state
                .agents
                .retain(|tracked| tracked.agent.agent_session != previous);
            self.emit_removed(previous);
        }
        state
            .current
            .retain(|other, current| *other == session || *current != id);
        let existing = state
            .agents
            .iter_mut()
            .find(|tracked| tracked.agent.agent_session == id);
        let tracked = Tracked {
            pid: pid.or(existing.as_ref().and_then(|tracked| tracked.pid)),
            since: Instant::now(),
            agent: {
                if let Some(existing) = &existing {
                    agent.name = agent.name.or(existing.agent.name.clone());
                    agent.last_error = agent.last_error.or(existing.agent.last_error.clone());
                }
                agent
            },
        };
        self.emit_changed(tracked.agent.clone());
        match existing {
            Some(existing) => *existing = tracked,
            None => state.agents.push(tracked),
        }
    }

    /// Changes the session's own agent; a stray hook for another is dropped.
    fn update(
        &self,
        state: &mut State,
        session: &str,
        agent_session: &str,
        change: impl FnOnce(&mut Agent),
    ) {
        if state.current.get(session).map(String::as_str) != Some(agent_session) {
            return;
        }
        if let Some(tracked) = state
            .agents
            .iter_mut()
            .find(|tracked| tracked.agent.agent_session == agent_session)
        {
            self.change(tracked, change);
        }
    }

    /// Emits `changed` when `change` changed anything.
    fn change(&self, tracked: &mut Tracked, change: impl FnOnce(&mut Agent)) {
        let before = tracked.agent.clone();
        change(&mut tracked.agent);
        if tracked.agent != before {
            self.emit_changed(tracked.agent.clone());
        }
    }

    /// The session's shell or foreground changed. The shell back in front,
    /// or gone, ends every agent that started before (ADR 0030); anything
    /// else in front with no live agent may be an agent without hooks.
    fn foreground(&self, message: &SessionForeground) {
        let mut state = self.state.lock().unwrap();
        state.processes.insert(
            message.session.clone(),
            Processes {
                shell: message.shell,
                group: message.group,
                name: message.name.clone(),
            },
        );
        if message.group.is_none() || message.group == message.shell {
            state.candidates.remove(&message.session);
            for tracked in state.agents.iter_mut().filter(|tracked| {
                tracked.agent.session.as_ref() == Some(&message.session)
                    && tracked.since < message.at
            }) {
                self.change(tracked, |agent| agent.state = AgentState::Exited);
            }
        } else if !has_live_agent(&state, &message.session) {
            let due = message.at + self.timing.grace;
            let candidate = Candidate {
                due,
                until: due + self.timing.grace * CANDIDATE_GRACES,
            };
            state.candidates.insert(message.session.clone(), candidate);
        }
        self.wake.notify_one();
    }

    /// Its agents stay listed with no session, exited: the hang-up ended them.
    fn session_closed(&self, session: &str) {
        let mut state = self.state.lock().unwrap();
        state.tokens.remove(session);
        state.current.remove(session);
        state.processes.remove(session);
        state.candidates.remove(session);
        state.hooked.remove(session);
        for tracked in state
            .agents
            .iter_mut()
            .filter(|tracked| tracked.agent.session.as_deref() == Some(session))
        {
            self.change(tracked, |agent| {
                agent.session = None;
                agent.state = AgentState::Exited;
            });
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
                let Some(shared) = shared.upgrade() else {
                    return;
                };
                if let Some(closed) = message.downcast_ref::<SessionClosed>() {
                    shared.session_closed(&closed.session);
                } else if let Some(foreground) = message.downcast_ref::<SessionForeground>() {
                    shared.foreground(foreground);
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(missed)) => {
                tracing::warn!(missed, "the agent module fell behind on session changes");
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
        }
    }
}

/// The session's agent is running, as far as anyone knows.
fn has_live_agent(state: &State, session: &str) -> bool {
    state.current.get(session).is_some_and(|id| {
        state.agents.iter().any(|tracked| {
            tracked.agent.agent_session == *id && tracked.agent.state != AgentState::Exited
        })
    })
}
