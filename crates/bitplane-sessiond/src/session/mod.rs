//! The `session` module: shells on PTYs the daemon owns, and attachments
//! that show and type into them (ADRs 0025, 0026).

mod pty;
mod spool;
mod stream;

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock, Weak, mpsc};

use bitplane_protocol::session::{
    AttachmentAckParams, AttachmentError, AttachmentId, AttachmentOpenParams, AttachmentOpenResult,
    AttachmentRef, AttachmentResizeParams, Session, SessionId, SessionListResult,
    SessionOpenParams, SessionRef, SessionResult, SessionStatus, Size,
};
use bitplane_protocol::{ErrorCode, Frame, Notification, RpcError, method};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::sync::oneshot;

use crate::env::{self, EnvMap};
use crate::module::{Availability, BoxFuture, Call, Connection, Events, Module, ModuleContext};

use self::pty::{PARENT_SESSION_VARS, Pty, Spawn};
use self::stream::Command;

pub use self::spool::{GhosttySpool, SCROLLBACK_BYTES, Spool};

const DEFAULT_SIZE: Size = Size { cols: 80, rows: 24 };

#[derive(Default)]
pub struct Sessions {
    shared: OnceLock<Arc<Shared>>,
}

struct Shared {
    env: Arc<EnvMap>,
    events: Events,
    state: Mutex<State>,
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
    stream: mpsc::Sender<Command>,
    /// `None` once the shell exited or failed to start.
    pty: Option<Pty>,
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
        "session"
    }

    fn probe(&self, _env: &EnvMap) -> Availability {
        Availability::Available { info: None }
    }

    fn start(&self, context: ModuleContext) {
        let _ = self.shared.set(Arc::new(Shared {
            env: context.env,
            events: context.events,
            state: Mutex::new(State::default()),
        }));
    }

    fn handle(&self, call: Call) -> BoxFuture<'_, Result<Value, RpcError>> {
        Box::pin(async move {
            let shared = self.shared();
            match call.method.as_str() {
                method::SESSION_LIST => to_value(shared.list()),
                method::SESSION_OPEN => to_value(shared.open(params(call.params)?)?),
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
        let env = session_env(&self.env, &params, &id);
        let shell = env::resolve_shell(env.get("SHELL").map(String::as_str));
        let stream = stream::spawn(DEFAULT_SIZE);

        let mut state = self.state.lock().unwrap();
        state.next_seq += 1;
        let weak = Arc::downgrade(self);
        let exited_id = id.clone();
        let spawned = Pty::spawn(
            Spawn {
                shell: &shell,
                root: &root,
                env,
                size: DEFAULT_SIZE,
                output: stream.clone(),
            },
            move |code| exited(&weak, &exited_id, code),
        );
        let (pty, status) = match spawned {
            Ok(pty) => (Some(pty), SessionStatus::Running),
            Err(error) => (None, SessionStatus::SpawnFailed { error }),
        };
        let info = Session {
            id: id.clone(),
            seq: state.next_seq,
            root: params.root,
            plane: params.plane,
            status,
            size: DEFAULT_SIZE,
            attachments: 0,
        };
        state.sessions.insert(
            id,
            Entry {
                info: info.clone(),
                stream,
                pty,
            },
        );
        self.changed(&info);
        Ok(SessionResult { session: info })
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
        self.events.emit(
            method::SESSION_REMOVED,
            serde_json::to_value(SessionRef { session }).expect("protocol types serialize"),
        );
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
        self.changed(&entry.info);
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

fn exited(shared: &Weak<Shared>, id: &str, code: i32) {
    let Some(shared) = shared.upgrade() else {
        return;
    };
    let mut state = shared.state.lock().unwrap();
    let Some(entry) = state.sessions.get_mut(id) else {
        return;
    };
    entry.pty = None;
    entry.info.status = SessionStatus::Exited { code };
    shared.changed(&entry.info);
}

/// The login-shell env, minus what a parent session or daemon leaked into
/// it, then the client's `env`, then the session's own variables.
fn session_env(login: &EnvMap, params: &SessionOpenParams, id: &str) -> EnvMap {
    let mut env: EnvMap = login
        .iter()
        .filter(|(key, _)| {
            !PARENT_SESSION_VARS.contains(&key.as_str()) && !key.starts_with("BITPLANE_")
        })
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    env.extend(params.env.clone().unwrap_or_default());
    env.insert("BITPLANE_SESSION_ID".into(), id.into());
    if let Some(plane) = &params.plane {
        env.insert("BITPLANE_PLANE_ID".into(), plane.clone());
        env.insert("BITPLANE_PLANE_DIR".into(), params.root.clone());
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
