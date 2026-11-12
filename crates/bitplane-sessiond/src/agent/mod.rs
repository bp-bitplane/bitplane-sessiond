//! The `agent` module: agent terminals, whose PTY child is a coding agent
//! launched with per-session hooks, tracked through those hooks and the
//! agent's own polled registry (ADRs 0029–0031). Each agent kind is one
//! [`AgentAdapter`]; this module owns everything kind-neutral.

pub mod claude;
pub mod hook;
mod poll;
mod process;

use std::collections::HashMap;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

use bitplane_protocol::agent::{
    Agent, AgentError, AgentKindStatus, AgentListResult, AgentRef, AgentResult, AgentSessionId,
    AgentState, Tracking, WaitingReason,
};
use bitplane_protocol::session::SessionId;
use bitplane_protocol::{ErrorCode, RpcError, method};
use serde_json::{Value, json};

use crate::env::EnvMap;
use crate::module::{
    AgentLink, AgentLinkChanged, Availability, BoxFuture, Bus, Call, Events, Launch, LaunchError,
    Module, ModuleContext,
};
use crate::session::{SessionClosed, SessionForeground};

/// The env a hook needs to reach the daemon and speak for its session.
pub const HOOK_SOCKET_VAR: &str = "BITPLANE_HOOK_SOCKET";
pub const HOOK_TOKEN_VAR: &str = "BITPLANE_HOOK_TOKEN";
pub const SESSION_VAR: &str = "BITPLANE_SESSION_ID";
const NAMESPACE: &str = "agent";
/// Where each agent terminal's hooks file lives, in the runtime dir.
const HOOKS_DIR: &str = "agent-hooks";

/// One agent kind's side of the module: how its agent terminals are
/// launched and what its hook payloads mean.
pub trait AgentAdapter: Send + Sync + 'static {
    fn kind(&self) -> &'static str;

    /// The agent's binary on `env`'s `PATH`, or why it can't be launched.
    fn resolve(&self, env: &EnvMap) -> Result<PathBuf, String>;

    /// The settings that make every hook run `hook`, the forwarder command.
    fn hooks(&self, hook: &str) -> Value;

    /// How `binary` is launched with the session's `hooks` file, resuming
    /// `link`; the hook channel's env is added on top.
    fn launch(&self, binary: &Path, hooks: &Path, link: Option<&AgentLink>) -> AgentCommand;

    fn ingest(&self, payload: &Value) -> Option<Ingested>;

    /// The agents the agent's own registry lists as running, host-wide;
    /// `None` when the registry could not be read. `env` is the login env.
    fn probe(&self, env: &EnvMap) -> Option<Vec<LiveAgent>>;
}

/// What an adapter launches: the binary first, then its arguments.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentCommand {
    pub argv: Vec<String>,
    pub env: EnvMap,
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

/// How often the registry is polled, and how long a launched agent may run
/// without a `SessionStart` before it is tracked by poll (ADR 0030).
#[derive(Debug, Clone, Copy)]
pub struct Timing {
    /// While any agent is busy, waiting or tracked by poll.
    pub poll: Duration,
    /// While any agent is live at all, to see it moved to `/bg`.
    pub live: Duration,
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
    bus: Bus,
    env: Arc<EnvMap>,
    timing: Timing,
    exe: PathBuf,
    hooks_dir: PathBuf,
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
    /// Each kind's availability as last reported.
    kinds: HashMap<String, AgentKindStatus>,
    /// The secret each agent terminal's hooks must present; only agent
    /// terminals have one, and only they are tracked.
    tokens: HashMap<SessionId, String>,
    /// The agent session each session last started.
    current: HashMap<SessionId, AgentSessionId>,
    /// Each session's agent link, once this module set it (ADR 0031).
    links: HashMap<SessionId, AgentLink>,
    /// In start order.
    agents: Vec<Tracked>,
    /// Each agent terminal's PTY child and foreground, as last published.
    processes: HashMap<SessionId, Processes>,
    /// When each agent terminal was last launched, until its PTY child is
    /// seen or a `SessionStart` arrives.
    launches: HashMap<SessionId, Instant>,
    /// Agent terminals launched with no `SessionStart` from them yet.
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

/// A launch that may be an agent whose hooks did not load: the registry is
/// asked from `due` until a `SessionStart` or the agent's process ends it.
#[derive(Debug, Clone, Copy)]
struct Candidate {
    due: Instant,
}

#[derive(Debug, Clone, Default)]
struct Processes {
    shell: Option<u32>,
    group: Option<u32>,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            poll: Duration::from_secs(3),
            live: Duration::from_secs(30),
            grace: Duration::from_secs(2),
        }
    }
}

impl Agents {
    /// The `agent` module with the Claude Code adapter; its hooks run `exe hook`.
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
        NAMESPACE
    }

    fn dependencies(&self) -> &'static [&'static str] {
        &["session"]
    }

    /// Always available: a missing agent binary makes its kind unavailable
    /// and its terminals `spawnFailed`, not the module (ADR 0029).
    fn probe(&self, _env: &EnvMap) -> Availability {
        Availability::Available { info: None }
    }

    fn start(&self, context: ModuleContext) {
        let shared = Arc::new(Shared {
            adapter: self.adapter.clone(),
            events: context.events,
            bus: context.bus.clone(),
            env: context.env,
            timing: self.timing,
            exe: self.exe.clone(),
            hooks_dir: context.dirs.runtime.join(HOOKS_DIR),
            hook_socket: context.dirs.runtime.join("hook.sock"),
            state: Mutex::new(State::default()),
            wake: Arc::default(),
            stopped: AtomicBool::new(false),
            tasks: Mutex::new(Vec::new()),
        });
        let initial = kind_status(shared.adapter.kind(), &shared.adapter.resolve(&shared.env));
        let mut state = shared.state.lock().unwrap();
        state.kinds.insert(initial.kind.clone(), initial);
        drop(state);
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

    fn launch(
        &self,
        session: &str,
        kind: &str,
        env: &EnvMap,
        link: Option<&AgentLink>,
        cwd: &Path,
    ) -> Option<Result<Launch, LaunchError>> {
        let shared = self.shared.get()?;
        if kind != shared.adapter.kind() {
            return None;
        }
        Some(shared.launch(session, env, link, cwd))
    }

    fn agent_link(&self, session: &str) -> Option<AgentLink> {
        let shared = self.shared.get()?;
        shared.state.lock().unwrap().links.get(session).cloned()
    }

    fn restored_exited(&self, session: &str, kind: &str, link: &AgentLink) {
        let Some(shared) = self.shared.get() else {
            return;
        };
        if kind == shared.adapter.kind() {
            shared.restored_exited(session, link);
        }
    }

    fn agent_cwd(&self, kind: &str, agent_session: &str) -> Option<String> {
        let shared = self.shared.get()?;
        if kind != shared.adapter.kind() {
            return None;
        }
        let state = shared.state.lock().unwrap();
        state
            .agents
            .iter()
            .find(|tracked| tracked.agent.agent_session == agent_session)
            .map(|tracked| tracked.agent.cwd.clone())
    }

    fn handle(&self, call: Call) -> BoxFuture<'_, Result<Value, RpcError>> {
        Box::pin(async move {
            let shared = self.shared();
            if call.method == method::AGENT_LIST {
                let kinds = vec![shared.resolve(&shared.env).1];
                let agents = shared
                    .state
                    .lock()
                    .unwrap()
                    .agents
                    .iter()
                    .map(|tracked| tracked.agent.clone())
                    .collect();
                return Ok(json!(AgentListResult { kinds, agents }));
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
    /// Launches the agent terminal `session` in `cwd`: its binary resolved
    /// on `env`'s `PATH`, its own hooks file and token, and `link` resumed. A
    /// launch replaces the session's previous agent process, so its agents
    /// end. Resuming an agent session live in another session is refused.
    fn launch(
        self: &Arc<Self>,
        session: &str,
        env: &EnvMap,
        link: Option<&AgentLink>,
        cwd: &Path,
    ) -> Result<Launch, LaunchError> {
        if let Some(link) = link {
            self.check_resume(session, link)
                .map_err(LaunchError::Refused)?;
        }
        let launched = Instant::now();
        {
            let mut state = self.state.lock().unwrap();
            for tracked in state.agents.iter_mut().filter(|tracked| {
                tracked.agent.session.as_deref() == Some(session) && tracked.since < launched
            }) {
                self.change(tracked, |agent| agent.state = AgentState::Exited);
            }
        }
        let binary = match self.resolve(env) {
            (Some(binary), _) => binary,
            (None, status) => {
                return Err(LaunchError::Failed(
                    status.error.unwrap_or_else(|| "unavailable".into()),
                ));
            }
        };
        if !cwd.is_dir() {
            return Err(LaunchError::Failed(format!(
                "{} is not a directory",
                cwd.display()
            )));
        }
        let hooks = self.hooks_dir.join(format!("{session}.json"));
        let command = format!("{} hook", shell_quote(&self.exe.display().to_string()));
        let settings =
            serde_json::to_vec_pretty(&self.adapter.hooks(&command)).expect("settings serialize");
        std::fs::create_dir_all(&self.hooks_dir)
            .and_then(|()| write_private(&hooks, &settings))
            .map_err(|error| {
                LaunchError::Failed(format!("couldn't write {}: {error}", hooks.display()))
            })?;
        let mut state = self.state.lock().unwrap();
        let token = state
            .tokens
            .entry(session.to_string())
            .or_insert_with(|| uuid::Uuid::new_v4().simple().to_string())
            .clone();
        state.launches.insert(session.to_string(), launched);
        state.candidates.remove(session);
        if let Some(link) = link {
            self.resuming(&mut state, session, link);
        }
        drop(state);
        let AgentCommand { argv, mut env } = self.adapter.launch(&binary, &hooks, link);
        env.insert(
            HOOK_SOCKET_VAR.into(),
            self.hook_socket.display().to_string(),
        );
        env.insert(HOOK_TOKEN_VAR.into(), token);
        env.insert(SESSION_VAR.into(), session.to_string());
        Ok(Launch { argv, env })
    }

    /// The kind's binary on `env`'s `PATH`, if any, and its status; a
    /// status that changed is broadcast.
    fn resolve(&self, env: &EnvMap) -> (Option<PathBuf>, AgentKindStatus) {
        let resolved = self.adapter.resolve(env);
        let status = kind_status(self.adapter.kind(), &resolved);
        let mut state = self.state.lock().unwrap();
        if state.kinds.get(&status.kind) != Some(&status) {
            state.kinds.insert(status.kind.clone(), status.clone());
            self.events
                .emit(method::AGENT_KIND_CHANGED, json!(status.clone()));
        }
        (resolved.ok(), status)
    }

    /// Refuses resuming `link` in `session`: an id that isn't one, or an
    /// agent session running or resuming in another session.
    fn check_resume(&self, session: &str, link: &AgentLink) -> Result<(), RpcError> {
        let id = &link.agent_session;
        // An argument after `--resume`; a leading `-` would read as an option.
        if id.is_empty()
            || id.starts_with('-')
            || !id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return Err(RpcError::new(
                RpcError::INVALID_PARAMS,
                format!("{id:?} is not an agent session id"),
            ));
        }
        let state = self.state.lock().unwrap();
        if let Some(other) = state
            .agents
            .iter()
            .find(|tracked| tracked.agent.agent_session == *id)
            .filter(|tracked| tracked.agent.state != AgentState::Exited)
            .and_then(|tracked| tracked.agent.session.clone())
            .filter(|other| other != session)
        {
            return Err(RpcError::protocol(
                ErrorCode::AgentLive,
                format!("agent session {id} is live in session {other}"),
                Some(json!({ "session": other })),
            ));
        }
        Ok(())
    }

    /// Tracks `link`'s agent session as `resuming` in `session` (ADR 0030)
    /// and makes it the session's link.
    fn resuming(&self, state: &mut State, session: &str, link: &AgentLink) {
        let id = &link.agent_session;
        let agent = Agent {
            kind: self.adapter.kind().into(),
            agent_session: id.clone(),
            session: Some(session.to_string()),
            cwd: link.cwd.clone(),
            name: None,
            state: AgentState::Resuming,
            last_error: None,
            tracking: Tracking::Hooks,
        };
        self.start_agent(state, agent, None);
        if let Some(tracked) = state
            .agents
            .iter_mut()
            .find(|tracked| tracked.agent.agent_session == *id)
        {
            tracked.pid = None;
        }
        self.link(state, session, link.clone());
    }

    /// Lists `link`'s agent session exited in `session`, as it was when it
    /// exited live; one already tracked elsewhere is left alone.
    fn restored_exited(&self, session: &str, link: &AgentLink) {
        let mut state = self.state.lock().unwrap();
        if state
            .agents
            .iter()
            .any(|tracked| tracked.agent.agent_session == link.agent_session)
        {
            return;
        }
        let agent = Agent {
            kind: self.adapter.kind().into(),
            agent_session: link.agent_session.clone(),
            session: Some(session.to_string()),
            cwd: link.cwd.clone(),
            name: None,
            state: AgentState::Exited,
            last_error: None,
            tracking: Tracking::Hooks,
        };
        self.start_agent(&mut state, agent, None);
    }

    /// Sets the session's agent link and has the sessions saved.
    fn link(&self, state: &mut State, session: &str, link: AgentLink) {
        if state.links.get(session) == Some(&link) {
            return;
        }
        state.links.insert(session.to_string(), link);
        self.bus.publish(AgentLinkChanged {
            session: session.to_string(),
        });
    }

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
            AgentEvent::Started { cwd, source } => {
                let link = AgentLink {
                    agent_session: agent_session.clone(),
                    cwd: cwd.clone(),
                };
                let agent = Agent {
                    kind: self.adapter.kind().into(),
                    agent_session,
                    session: Some(session.to_string()),
                    cwd,
                    name: None,
                    state: AgentState::Idle,
                    last_error: None,
                    tracking: Tracking::Hooks,
                };
                state.launches.remove(session);
                state.candidates.remove(session);
                self.start_agent(&mut state, agent, None);
                if source != "compact" {
                    self.link(&mut state, session, link);
                }
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

    /// A look at an agent terminal's PTY child, which is the agent itself
    /// (ADR 0029); regular terminals are not tracked. The child gone ends
    /// every agent that started before (ADR 0030). A launched child that
    /// sent no `SessionStart` yet may be an agent whose hooks are blocked.
    fn foreground(&self, message: &SessionForeground) {
        let mut state = self.state.lock().unwrap();
        if !state.tokens.contains_key(&message.session) {
            return;
        }
        let previous = state.processes.insert(
            message.session.clone(),
            Processes {
                shell: message.shell,
                group: message.group,
            },
        );
        if message.shell.is_none() {
            state.candidates.remove(&message.session);
            for tracked in state.agents.iter_mut().filter(|tracked| {
                tracked.agent.session.as_ref() == Some(&message.session)
                    && tracked.since < message.at
            }) {
                self.change(tracked, |agent| agent.state = AgentState::Exited);
            }
            self.wake.notify_one();
            return;
        }
        let new_child = previous.is_none_or(|previous| previous.shell != message.shell);
        if let Some(launched) = new_child
            .then(|| state.launches.remove(&message.session))
            .flatten()
        {
            let due = launched + self.timing.grace;
            state
                .candidates
                .insert(message.session.clone(), Candidate { due });
        }
        self.wake.notify_one();
    }

    /// Its agents stay listed with no session, exited: the hang-up ended them.
    fn session_closed(&self, session: &str) {
        let _ = std::fs::remove_file(self.hooks_dir.join(format!("{session}.json")));
        let mut state = self.state.lock().unwrap();
        state.tokens.remove(session);
        state.current.remove(session);
        state.links.remove(session);
        state.processes.remove(session);
        state.launches.remove(session);
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
        self.events
            .emit(method::AGENT_CHANGED, json!(AgentResult { agent }));
    }

    fn emit_removed(&self, agent_session: AgentSessionId) {
        self.events
            .emit(method::AGENT_REMOVED, json!(AgentRef { agent_session }));
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

fn kind_status(kind: &str, resolved: &Result<PathBuf, String>) -> AgentKindStatus {
    AgentKindStatus {
        kind: kind.into(),
        available: resolved.is_ok(),
        error: resolved.as_ref().err().cloned(),
    }
}

fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// tmp + rename, owner-only.
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("json.tmp");
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)?;
    file.write_all(bytes)?;
    std::fs::rename(&tmp, path)
}
