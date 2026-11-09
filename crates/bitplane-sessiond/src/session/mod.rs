//! The `session` module: shells on PTYs the daemon owns, and attachments
//! that show and type into them (ADRs 0025, 0026), persisted and restored
//! as fresh shells across daemon restarts (ADR 0027).

mod persist;
mod pty;
mod spool;
mod stream;

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak, mpsc};
use std::time::{Duration, Instant};

use bitplane_protocol::session::{
    AttachmentAckParams, AttachmentError, AttachmentId, AttachmentOpenParams, AttachmentOpenResult,
    AttachmentRef, AttachmentResizeParams, Session, SessionBusyResult, SessionId,
    SessionListResult, SessionOpenParams, SessionRef, SessionRenameParams, SessionResult,
    SessionStatus, Size,
};
use bitplane_protocol::{ErrorCode, Frame, Notification, RpcError, method};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::sync::oneshot;

use crate::env::{self, EnvMap};
use crate::module::{
    AgentLink, AgentLinkChanged, Availability, BoxFuture, Bus, Call, Connection, Events,
    Instruments, Module, ModuleContext, Resumed,
};

use self::persist::{Persist, Record};
use self::pty::{PARENT_SESSION_VARS, Pty, Spawn};
use self::stream::Command;

pub use self::spool::{GhosttySpool, SCROLLBACK_BYTES, Spool};

const NAMESPACE: &str = "session";
const DEFAULT_SIZE: Size = Size { cols: 80, rows: 24 };
/// How often roots and foreground processes are checked. Polling also sees
/// an ancestor renamed or unmounted, which a watch on the root misses.
const WATCH_INTERVAL: Duration = Duration::from_millis(500);

/// Published on the bus once a session is closed.
#[derive(Debug, Clone)]
pub struct SessionClosed {
    pub session: SessionId,
}

/// Published on the bus on every look at a session's shell and its
/// terminal's foreground process group, changed or not, so a short-lived
/// foreground is still followed by the shell's return; both are `None` once
/// the shell is gone.
#[derive(Debug, Clone)]
pub struct SessionForeground {
    pub session: SessionId,
    pub shell: Option<u32>,
    pub group: Option<u32>,
    pub name: Option<String>,
    /// When the session was looked at.
    pub at: Instant,
}

#[derive(Default)]
pub struct Sessions {
    shared: OnceLock<Arc<Shared>>,
}

struct Shared {
    env: Arc<EnvMap>,
    events: Events,
    bus: Bus,
    instruments: Instruments,
    state: Mutex<State>,
    persist: Persist,
}

#[derive(Default)]
struct State {
    next_seq: u32,
    next_attachment: AttachmentId,
    sessions: BTreeMap<SessionId, Entry>,
    attachments: HashMap<AttachmentId, Attached>,
}

struct Entry {
    info: Session,
    /// What every shell of the session starts with.
    env: EnvMap,
    /// The env `session.open` was given, which a restore starts from.
    client_env: Option<EnvMap>,
    /// The link the session was opened or restored with; an agent module's
    /// own decision overrides it (ADR 0031).
    agent: Option<AgentLink>,
    stream: mpsc::Sender<Command>,
    /// `None` once the shell exited or failed to start.
    pty: Option<Pty>,
    /// Counts the session's shells; a stale one's exit is ignored.
    generation: u64,
}

struct Attached {
    connection: u64,
    session: SessionId,
    size: Size,
    read_only: bool,
    ignore_size: bool,
}

impl Sessions {
    pub fn module() -> Arc<dyn Module> {
        Arc::new(Self::default())
    }

    fn shared(&self) -> &Arc<Shared> {
        self.shared.get().expect("started before any call")
    }
}

impl Module for Sessions {
    fn namespace(&self) -> &'static str {
        NAMESPACE
    }

    fn probe(&self, _env: &EnvMap) -> Availability {
        Availability::Available { info: None }
    }

    /// Registers every persisted session before returning, so the first
    /// `session.list` is complete; their shells start in `ready`.
    fn start(&self, context: ModuleContext) {
        let restored = persist::restore(&context.dirs, &context.state_dir);
        let links = context.bus.subscribe();
        let shared = Arc::new(Shared {
            env: context.env,
            events: context.events,
            bus: context.bus,
            instruments: context.instruments,
            state: Mutex::new(State::default()),
            persist: Persist::new(restored.file),
        });
        shared.restore(restored.records);
        let weak = Arc::downgrade(&shared);
        if self.shared.set(shared).is_ok() {
            let saver = weak.clone();
            let relinked = weak.clone();
            std::thread::Builder::new()
                .name("session-links".into())
                .spawn(move || follow_links(links, &relinked))
                .expect("spawning a thread");
            std::thread::Builder::new()
                .name("session-watch".into())
                .spawn(move || watch(&weak))
                .expect("spawning a thread");
            std::thread::Builder::new()
                .name("session-persist".into())
                .spawn(move || save(&saver))
                .expect("spawning a thread");
        }
    }

    fn ready(&self) {
        if let Some(shared) = self.shared.get() {
            shared.spawn_restored();
        }
    }

    fn shutdown(&self, restore: bool) {
        if let Some(shared) = self.shared.get()
            && restore
        {
            shared.persist.close(Some(&shared.records()));
        }
    }

    fn terminate(&self, restore: bool) {
        if let Some(shared) = self.shared.get() {
            shared.terminate(restore);
        }
    }

    fn handle(&self, call: Call) -> BoxFuture<'_, Result<Value, RpcError>> {
        Box::pin(async move {
            let shared = self.shared();
            match call.method.as_str() {
                method::SESSION_LIST => to_value(shared.list()),
                method::SESSION_OPEN => to_value(shared.open(params(call.params)?)?),
                method::SESSION_RESTART => to_value(shared.restart(params(call.params)?)?),
                method::SESSION_BUSY => to_value(shared.busy(params(call.params)?)?),
                method::SESSION_RENAME => to_value(shared.rename(params(call.params)?)?),
                method::SESSION_CLOSE => {
                    shared.close(params(call.params)?)?;
                    Ok(json!({}))
                }
                method::SESSION_ATTACHMENT_OPEN => shared.attach(call).await,
                method::SESSION_ATTACHMENT_RESIZE => {
                    shared.resize(call.connection.id, params(call.params)?)?;
                    Ok(json!({}))
                }
                method::SESSION_ATTACHMENT_CLOSE => {
                    let AttachmentRef { attachment } = params(call.params)?;
                    let detached = shared.detach(call.connection.id, attachment)?;
                    let _ = detached.await;
                    Ok(json!({}))
                }
                method::SESSION_ATTACHMENT_ACK => {
                    shared.ack(call.connection.id, params(call.params)?);
                    Ok(Value::Null)
                }
                _ => Err(RpcError::new(
                    RpcError::METHOD_NOT_FOUND,
                    format!("no method {}", call.method),
                )),
            }
        })
    }

    fn input(&self, connection: &Connection, attachment: AttachmentId, data: Vec<u8>) {
        if let Err((code, message)) = self.shared().input(connection.id, attachment, data) {
            let error = AttachmentError {
                attachment,
                code,
                message,
            };
            let event = Notification::new(
                method::SESSION_ATTACHMENT_ERROR,
                Some(serde_json::to_value(error).expect("protocol types serialize")),
            );
            let bytes = serde_json::to_vec(&event).expect("protocol messages serialize");
            let _ = connection.frames.send(Frame::Json(bytes));
        }
    }

    fn disconnected(&self, connection: u64) {
        let Some(shared) = self.shared.get() else {
            return;
        };
        let gone: Vec<AttachmentId> = {
            let state = shared.state.lock().unwrap();
            state
                .attachments
                .iter()
                .filter(|(_, attached)| attached.connection == connection)
                .map(|(&attachment, _)| attachment)
                .collect()
        };
        for attachment in gone {
            let _ = shared.detach(connection, attachment);
        }
    }
}

impl Shared {
    fn list(&self) -> SessionListResult {
        let state = self.state.lock().unwrap();
        let mut sessions: Vec<Session> = state
            .sessions
            .values()
            .map(|entry| entry.info.clone())
            .collect();
        sessions.sort_by_key(|session| session.seq);
        SessionListResult { sessions }
    }

    fn open(self: &Arc<Self>, params: SessionOpenParams) -> Result<SessionResult, RpcError> {
        let root = PathBuf::from(&params.root);
        if !root.is_absolute() {
            return Err(RpcError::new(
                RpcError::INVALID_PARAMS,
                format!("root {} is not an absolute path", params.root),
            ));
        }
        if !root.is_dir() {
            return Err(RpcError::protocol(
                ErrorCode::NotFound,
                format!("{} is not a directory", params.root),
                None,
            ));
        }
        let id = uuid::Uuid::new_v4().to_string();
        let link = params.resume.map(|agent_session| AgentLink {
            agent_session,
            cwd: params.root.clone(),
        });
        let resumed = link
            .as_ref()
            .and_then(|link| self.instruments.resume(&id, link))
            .transpose()?;
        let mut state = self.state.lock().unwrap();
        state.next_seq += 1;
        let mut entry = self.entry(Record {
            id,
            seq: state.next_seq,
            root: params.root,
            plane: params.plane,
            name: user_name(params.name),
            env: params.env,
            size: DEFAULT_SIZE,
            agent: link,
        });
        self.spawn_shell(&mut entry, resumed);
        let info = entry.info.clone();
        state.sessions.insert(info.id.clone(), entry);
        self.persist.touch();
        self.changed(&info);
        Ok(SessionResult { session: info })
    }

    /// Persisted sessions, without shells yet; where the root is gone they
    /// get none.
    fn restore(self: &Arc<Self>, records: Vec<Record>) {
        let mut state = self.state.lock().unwrap();
        for record in records {
            state.next_seq = state.next_seq.max(record.seq);
            let mut entry = self.entry(record);
            if !Path::new(&entry.info.root).is_dir() {
                entry.info.root_gone = true;
                entry.info.status = SessionStatus::Exited { code: -1 };
            }
            state.sessions.insert(entry.info.id.clone(), entry);
        }
    }

    /// Fresh shells for the restored sessions whose root is there, resuming
    /// their agent links.
    fn spawn_restored(self: &Arc<Self>) {
        let mut state = self.state.lock().unwrap();
        for entry in state.sessions.values_mut() {
            if entry.pty.is_none() && !entry.info.root_gone {
                let resumed = self.resumed(entry);
                self.spawn_shell(entry, resumed);
            }
        }
    }

    /// What the session's next shell resumes; nothing without a link or a
    /// module that runs agents, and the link stays for a later shell.
    fn resumed(&self, entry: &Entry) -> Option<Resumed> {
        let id = &entry.info.id;
        let link = self.instruments.agent_link(id, entry.agent.as_ref())?;
        match self.instruments.resume(id, &link)? {
            Ok(resumed) => Some(resumed),
            Err(error) => {
                tracing::warn!(session = %id, agent_session = %link.agent_session, error = %error.message, "not resuming the agent session");
                None
            }
        }
    }

    /// A session without a shell yet.
    fn entry(self: &Arc<Self>, record: Record) -> Entry {
        let Record {
            id,
            seq,
            root,
            plane,
            name,
            env: client_env,
            size,
            agent,
        } = record;
        let env = session_env(&self.env, client_env.as_ref(), &id, plane.as_deref(), &root);
        let weak = Arc::downgrade(self);
        let titled_id = id.clone();
        let stream = stream::spawn(size, move |title| retitled(&weak, &titled_id, title));
        Entry {
            info: Session {
                id,
                seq,
                root,
                plane,
                name,
                title: None,
                foreground: None,
                status: SessionStatus::Running,
                root_gone: false,
                size,
                attachments: 0,
            },
            env,
            client_env,
            agent,
            stream,
            pty: None,
            generation: 0,
        }
    }

    /// Hangs up every shell; without `restore` the sessions are then forgotten.
    fn terminate(&self, restore: bool) {
        let mut state = self.state.lock().unwrap();
        for entry in state.sessions.values_mut() {
            if let Some(pty) = entry.pty.take() {
                pty.hang_up();
            }
        }
        if !restore {
            self.persist.close(None);
        }
    }

    /// Hangs up the shell if it still runs and starts a new one in the root,
    /// under the same id, at the session's size.
    fn restart(
        self: &Arc<Self>,
        SessionRef { session }: SessionRef,
    ) -> Result<SessionResult, RpcError> {
        let mut state = self.state.lock().unwrap();
        let entry = state
            .sessions
            .get_mut(&session)
            .ok_or_else(|| unknown_session(&session))?;
        if !Path::new(&entry.info.root).is_dir() {
            if !entry.info.root_gone {
                entry.info.root_gone = true;
                self.changed(&entry.info);
            }
            return Err(RpcError::protocol(
                ErrorCode::RootGone,
                format!(
                    "{} no longer exists; close the session instead",
                    entry.info.root
                ),
                None,
            ));
        }
        if let Some(pty) = entry.pty.take() {
            pty.hang_up();
        }
        entry.info.root_gone = false;
        entry.info.title = None;
        let resumed = self.resumed(entry);
        self.spawn_shell(entry, resumed);
        self.changed(&entry.info);
        Ok(SessionResult {
            session: entry.info.clone(),
        })
    }

    fn busy(&self, SessionRef { session }: SessionRef) -> Result<SessionBusyResult, RpcError> {
        let mut state = self.state.lock().unwrap();
        let entry = state
            .sessions
            .get_mut(&session)
            .ok_or_else(|| unknown_session(&session))?;
        let foreground = entry.pty.as_ref().and_then(Pty::foreground);
        let name = foreground.as_ref().and_then(|fg| fg.name.clone());
        if entry.pty.is_some() && entry.info.foreground != name {
            entry.info.foreground.clone_from(&name);
            self.changed(&entry.info);
        }
        let busy = foreground.is_some_and(|fg| fg.busy);
        Ok(SessionBusyResult {
            busy,
            process_name: name.filter(|_| busy),
        })
    }

    fn rename(&self, params: SessionRenameParams) -> Result<SessionResult, RpcError> {
        let mut state = self.state.lock().unwrap();
        let entry = state
            .sessions
            .get_mut(&params.session)
            .ok_or_else(|| unknown_session(&params.session))?;
        let name = user_name(params.name);
        if entry.info.name != name {
            entry.info.name = name;
            self.persist.touch();
            self.changed(&entry.info);
        }
        Ok(SessionResult {
            session: entry.info.clone(),
        })
    }

    /// The stream starts afresh first, so it drops what the old shell still
    /// writes. A resumed agent session starts the shell in the link's cwd,
    /// where the agent finds it, and is typed into it.
    fn spawn_shell(self: &Arc<Self>, entry: &mut Entry, resumed: Option<Resumed>) {
        entry.generation += 1;
        let generation = entry.generation;
        let size = entry.info.size;
        let _ = entry.stream.send(Command::Restart { generation, size });
        let shell = env::resolve_shell(entry.env.get("SHELL").map(String::as_str));
        let weak = Arc::downgrade(self);
        let id = entry.info.id.clone();
        let mut shell_env = entry.env.clone();
        self.instruments.apply(&id, &mut shell_env);
        let cwd = resumed
            .as_ref()
            .map(|resumed| Path::new(&resumed.link.cwd))
            .filter(|cwd| cwd.is_dir())
            .unwrap_or(Path::new(&entry.info.root));
        let spawned = Pty::spawn(
            Spawn {
                shell: &shell,
                cwd,
                env: shell_env,
                size,
                generation,
                output: entry.stream.clone(),
            },
            move |code| exited(&weak, &id, generation, code),
        );
        entry.info.foreground = None;
        (entry.pty, entry.info.status) = match spawned {
            Ok(pty) => (Some(pty), SessionStatus::Running),
            Err(error) => (None, SessionStatus::SpawnFailed { error }),
        };
        // Not left to the watch: an agent can start, work and quit within one
        // tick, and the registry is joined by the shell pid.
        let shell_pid = entry.pty.as_ref().and_then(Pty::shell_pid);
        self.foreground(&entry.info.id, entry, shell_pid, shell_pid, Instant::now());
        if let Some(Resumed { link, command }) = resumed {
            if let Some(pty) = &entry.pty {
                pty.write(command.into_bytes());
            }
            entry.agent = Some(link);
        }
    }

    /// Every session as the next instance restores it, with its agent link
    /// as it stands now.
    fn records(&self) -> Vec<Record> {
        let state = self.state.lock().unwrap();
        let mut records: Vec<Record> = state
            .sessions
            .values()
            .map(|entry| Record {
                id: entry.info.id.clone(),
                seq: entry.info.seq,
                root: entry.info.root.clone(),
                plane: entry.info.plane.clone(),
                name: entry.info.name.clone(),
                env: entry.client_env.clone(),
                size: entry.info.size,
                agent: self
                    .instruments
                    .agent_link(&entry.info.id, entry.agent.as_ref()),
            })
            .collect();
        records.sort_by_key(|record| record.seq);
        records
    }

    fn close(&self, SessionRef { session }: SessionRef) -> Result<(), RpcError> {
        let mut state = self.state.lock().unwrap();
        let entry = state
            .sessions
            .remove(&session)
            .ok_or_else(|| unknown_session(&session))?;
        state
            .attachments
            .retain(|_, attached| attached.session != session);
        let _ = entry.stream.send(Command::Close);
        if let Some(pty) = entry.pty {
            pty.hang_up();
        }
        self.persist.touch();
        self.events.emit(
            method::SESSION_REMOVED,
            serde_json::to_value(SessionRef {
                session: session.clone(),
            })
            .expect("protocol types serialize"),
        );
        self.bus.publish(SessionClosed { session });
        Ok(())
    }

    /// Replies, then the stream sends the SNAPSHOT: both happen on the
    /// stream's thread, after the output before them and before the output after.
    async fn attach(&self, call: Call) -> Result<Value, RpcError> {
        let AttachmentOpenParams {
            session,
            cols,
            rows,
            read_only,
            ignore_size,
        } = params(call.params)?;
        let (done, attached) = oneshot::channel();
        {
            let mut state = self.state.lock().unwrap();
            let State {
                next_attachment,
                sessions,
                attachments,
                ..
            } = &mut *state;
            let entry = sessions
                .get_mut(&session)
                .ok_or_else(|| unknown_session(&session))?;
            *next_attachment += 1;
            let attachment = *next_attachment;
            attachments.insert(
                attachment,
                Attached {
                    connection: call.connection.id,
                    session: session.clone(),
                    size: Size { cols, rows },
                    read_only,
                    ignore_size,
                },
            );
            entry.info.attachments += 1;
            let result = to_value(AttachmentOpenResult {
                attachment,
                session: entry.info.clone(),
            })?;
            let _ = entry.stream.send(Command::Attach {
                attachment,
                connection: call.connection.clone(),
                reply: call.reply.clone(),
                result,
                done,
            });
            if let Some(pty) = &entry.pty {
                pty.nudge();
            }
            self.changed(&entry.info);
        }
        let _ = attached.await;
        Ok(Value::Null)
    }

    fn resize(&self, connection: u64, params: AttachmentResizeParams) -> Result<(), RpcError> {
        let mut state = self.state.lock().unwrap();
        let attached = state
            .attachments
            .get_mut(&params.attachment)
            .filter(|attached| attached.connection == connection)
            .ok_or_else(|| unknown_attachment(params.attachment))?;
        attached.size = Size {
            cols: params.cols,
            rows: params.rows,
        };
        self.activity(&mut state, params.attachment);
        Ok(())
    }

    /// Resolves once no more frames are sent to the attachment.
    fn detach(
        &self,
        connection: u64,
        attachment: AttachmentId,
    ) -> Result<oneshot::Receiver<()>, RpcError> {
        let mut state = self.state.lock().unwrap();
        if state
            .attachments
            .get(&attachment)
            .is_none_or(|attached| attached.connection != connection)
        {
            return Err(unknown_attachment(attachment));
        }
        let attached = state.attachments.remove(&attachment).expect("checked");
        let (done, detached) = oneshot::channel();
        if let Some(entry) = state.sessions.get_mut(&attached.session) {
            let _ = entry.stream.send(Command::Detach { attachment, done });
            entry.info.attachments -= 1;
            self.changed(&entry.info);
        }
        Ok(detached)
    }

    fn ack(&self, connection: u64, params: AttachmentAckParams) {
        let state = self.state.lock().unwrap();
        let entry = state
            .attachments
            .get(&params.attachment)
            .filter(|attached| attached.connection == connection)
            .and_then(|attached| state.sessions.get(&attached.session));
        if let Some(entry) = entry {
            let _ = entry.stream.send(Command::Ack {
                attachment: params.attachment,
                bytes: params.bytes,
            });
        }
    }

    fn input(
        &self,
        connection: u64,
        attachment: AttachmentId,
        data: Vec<u8>,
    ) -> Result<(), (ErrorCode, String)> {
        let mut state = self.state.lock().unwrap();
        let attached = state
            .attachments
            .get(&attachment)
            .filter(|attached| attached.connection == connection)
            .ok_or_else(|| {
                (
                    ErrorCode::UnknownAttachment,
                    format!("no attachment {attachment} on this connection"),
                )
            })?;
        if attached.read_only {
            return Err((ErrorCode::ReadOnly, "the attachment is read-only".into()));
        }
        let session = attached.session.clone();
        if state.sessions.get(&session).is_none_or(|e| e.pty.is_none()) {
            return Err((ErrorCode::SessionExited, "the shell has exited".into()));
        }
        // Resized first, so the shell reads this input at the new size.
        self.activity(&mut state, attachment);
        if let Some(pty) = state.sessions.get(&session).and_then(|e| e.pty.as_ref()) {
            pty.write(data);
        }
        Ok(())
    }

    /// The latest active attachment owns the size (ADR 0026).
    fn activity(&self, state: &mut State, attachment: AttachmentId) {
        let Some(attached) = state
            .attachments
            .get(&attachment)
            .filter(|attached| !attached.ignore_size)
        else {
            return;
        };
        let size = attached.size;
        let Some(entry) = state.sessions.get_mut(&attached.session) else {
            return;
        };
        if entry.info.size == size {
            return;
        }
        entry.info.size = size;
        if let Some(pty) = &entry.pty {
            pty.resize(size);
        }
        let _ = entry.stream.send(Command::Resize(size));
        self.persist.touch();
        self.changed(&entry.info);
    }

    fn foreground(
        &self,
        id: &str,
        entry: &Entry,
        shell: Option<u32>,
        group: Option<u32>,
        at: Instant,
    ) {
        self.bus.publish(SessionForeground {
            session: id.to_string(),
            shell,
            group,
            name: entry.info.foreground.clone(),
            at,
        });
    }

    fn changed(&self, session: &Session) {
        self.events.emit(
            method::SESSION_CHANGED,
            serde_json::to_value(SessionResult {
                session: session.clone(),
            })
            .expect("protocol types serialize"),
        );
    }
}

fn exited(shared: &Weak<Shared>, id: &str, generation: u64, code: i32) {
    let Some(shared) = shared.upgrade() else {
        return;
    };
    let mut state = shared.state.lock().unwrap();
    let Some(entry) = state
        .sessions
        .get_mut(id)
        .filter(|entry| entry.generation == generation)
    else {
        return;
    };
    entry.pty = None;
    entry.info.foreground = None;
    entry.info.status = SessionStatus::Exited { code };
    shared.changed(&entry.info);
    shared.foreground(id, entry, None, None, Instant::now());
}

fn retitled(shared: &Weak<Shared>, id: &str, title: Option<String>) {
    let Some(shared) = shared.upgrade() else {
        return;
    };
    let mut state = shared.state.lock().unwrap();
    let Some(entry) = state.sessions.get_mut(id) else {
        return;
    };
    if entry.info.title != title {
        entry.info.title = title;
        shared.changed(&entry.info);
    }
}

/// Saves the sessions [`persist::DEBOUNCE`] after a change, until shutdown.
fn save(shared: &Weak<Shared>) {
    while let Some(shared) = shared.upgrade() {
        if !shared.persist.due() {
            return;
        }
        shared.persist.save(&shared.records());
    }
}

/// Saves the sessions whenever another module changed an agent link.
fn follow_links(
    mut messages: tokio::sync::broadcast::Receiver<Arc<dyn std::any::Any + Send + Sync>>,
    shared: &Weak<Shared>,
) {
    loop {
        let changed = match messages.blocking_recv() {
            Ok(message) => message.is::<AgentLinkChanged>(),
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => true,
            Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
        };
        let Some(shared) = shared.upgrade() else {
            return;
        };
        if changed {
            shared.persist.touch();
        }
    }
}

/// Polls every session's root and foreground process until the module is gone.
fn watch(shared: &Weak<Shared>) {
    loop {
        std::thread::sleep(WATCH_INTERVAL);
        let Some(shared) = shared.upgrade() else {
            return;
        };
        let roots: Vec<(SessionId, String)> = {
            let state = shared.state.lock().unwrap();
            state
                .sessions
                .iter()
                .map(|(id, entry)| (id.clone(), entry.info.root.clone()))
                .collect()
        };
        // Stat outside the lock: a hung mount must not stall the module.
        let gone: Vec<(SessionId, bool)> = roots
            .into_iter()
            .map(|(id, root)| (id, !Path::new(&root).is_dir()))
            .collect();
        let mut state = shared.state.lock().unwrap();
        for (id, gone) in gone {
            let Some(entry) = state.sessions.get_mut(&id) else {
                continue;
            };
            let seen = Instant::now();
            let shell = entry.pty.as_ref().and_then(Pty::shell_pid);
            let (foreground, group) = entry
                .pty
                .as_ref()
                .and_then(Pty::foreground)
                .map_or((None, None), |fg| (fg.name, Some(fg.group)));
            if entry.info.root_gone != gone || entry.info.foreground != foreground {
                entry.info.root_gone = gone;
                entry.info.foreground = foreground;
                shared.changed(&entry.info);
            }
            shared.foreground(&id, entry, shell, group, seen);
        }
    }
}

/// A name of only whitespace is no name, as in cc.
fn user_name(name: Option<String>) -> Option<String> {
    name.filter(|name| !name.trim().is_empty())
}

/// The login-shell env, minus what a parent session or daemon leaked into
/// it, then the client's `env`, then the session's own variables.
fn session_env(
    login: &EnvMap,
    client: Option<&EnvMap>,
    id: &str,
    plane: Option<&str>,
    root: &str,
) -> EnvMap {
    let mut env: EnvMap = login
        .iter()
        .filter(|(key, _)| {
            !PARENT_SESSION_VARS.contains(&key.as_str()) && !key.starts_with("BITPLANE_")
        })
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    env.extend(client.cloned().unwrap_or_default());
    env.insert("BITPLANE_SESSION_ID".into(), id.into());
    if let Some(plane) = plane {
        env.insert("BITPLANE_PLANE_ID".into(), plane.into());
        env.insert("BITPLANE_PLANE_DIR".into(), root.into());
    }
    env.insert("TERM".into(), "xterm-256color".into());
    env.insert("COLORTERM".into(), "truecolor".into());
    env.insert("TERM_PROGRAM".into(), crate::DAEMON_NAME.into());
    env.insert("TERM_PROGRAM_VERSION".into(), crate::DAEMON_VERSION.into());
    env.remove("PWD");
    env
}

fn params<T: DeserializeOwned>(params: Option<Value>) -> Result<T, RpcError> {
    serde_json::from_value(params.unwrap_or(Value::Null))
        .map_err(|error| RpcError::new(RpcError::INVALID_PARAMS, error.to_string()))
}

fn to_value(value: impl Serialize) -> Result<Value, RpcError> {
    Ok(serde_json::to_value(value).expect("protocol types serialize"))
}

fn unknown_session(session: &str) -> RpcError {
    RpcError::protocol(
        ErrorCode::UnknownSession,
        format!("no session {session}"),
        None,
    )
}

fn unknown_attachment(attachment: AttachmentId) -> RpcError {
    RpcError::protocol(
        ErrorCode::UnknownAttachment,
        format!("no attachment {attachment} on this connection"),
        None,
    )
}
