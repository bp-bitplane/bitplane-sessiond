//! A session's output stream: one thread that owns its [`Spool`] and its
//! attachments. Commands are handled in order, so an attach takes its
//! snapshot and subscribes between two output chunks (ADR 0024), and the PTY
//! reader never waits on it or on any client (ADR 0026).

use std::collections::HashMap;
use std::sync::mpsc;

use bitplane_protocol::Frame;
use bitplane_protocol::session::{AttachmentId, Size};
use serde_json::Value;
use tokio::sync::oneshot;

use crate::module::{Connection, Reply};

use super::spool::{GhosttySpool, Spool};

/// Unacknowledged bytes allowed in flight per attachment (cc ADR 0003).
pub const WINDOW: u64 = 128 * 1024;
/// Once the window is full, sending resumes when unacked bytes drop to this.
pub const LOW_WATER: u64 = 32 * 1024;
/// Largest OUTPUT frame.
pub const MAX_FRAME: usize = 16 * 1024;

pub enum Command {
    Output(Vec<u8>),
    /// Sends `result` as the reply, then the SNAPSHOT, then subscribes.
    Attach {
        attachment: AttachmentId,
        connection: Connection,
        reply: Reply,
        result: Value,
        done: oneshot::Sender<()>,
    },
    Ack {
        attachment: AttachmentId,
        bytes: u64,
    },
    Detach(AttachmentId),
    Resize(Size),
    /// The session is closed: stop sending to anyone.
    Close,
}

/// Starts the stream's thread; it ends once every sender is dropped.
pub fn spawn(size: Size) -> mpsc::Sender<Command> {
    let (commands, received) = mpsc::channel();
    std::thread::Builder::new()
        .name("session-stream".into())
        .spawn(move || run(size, received))
        .expect("spawning a thread");
    commands
}

struct Stream {
    spool: GhosttySpool,
    /// Session output bytes so far; the offset of the next one.
    offset: u64,
    attachments: HashMap<AttachmentId, Flow>,
}

/// One attachment's byte-ack window. `sent` and `acked` count snapshot and
/// output bytes; `queue` holds output not yet sent, ending at the stream's offset.
struct Flow {
    connection: Connection,
    sent: u64,
    acked: u64,
    paused: bool,
    queue: Vec<u8>,
}

fn run(size: Size, commands: mpsc::Receiver<Command>) {
    let mut stream = Stream {
        spool: GhosttySpool::new(size),
        offset: 0,
        attachments: HashMap::new(),
    };
    while let Ok(command) = commands.recv() {
        stream.handle(command);
        // Coalesce whatever else is waiting into the same frames.
        while let Ok(command) = commands.try_recv() {
            stream.handle(command);
        }
        stream.flush();
    }
}

impl Stream {
    fn handle(&mut self, command: Command) {
        match command {
            Command::Output(data) => {
                self.spool.process(&data);
                self.offset += data.len() as u64;
                for flow in self.attachments.values_mut() {
                    flow.queue.extend_from_slice(&data);
                }
            }
            Command::Attach {
                attachment,
                connection,
                reply,
                result,
                done,
            } => {
                reply.send(Ok(result));
                let snapshot = self.spool.snapshot();
                let sent = snapshot.len() as u64;
                let _ = connection.frames.send(Frame::Snapshot {
                    attachment,
                    offset: self.offset,
                    data: snapshot,
                });
                self.attachments.insert(
                    attachment,
                    Flow {
                        connection,
                        sent,
                        acked: 0,
                        paused: false,
                        queue: Vec::new(),
                    },
                );
                let _ = done.send(());
            }
            Command::Ack { attachment, bytes } => {
                if let Some(flow) = self.attachments.get_mut(&attachment) {
                    flow.acked = flow.acked.max(bytes.min(flow.sent));
                }
            }
            Command::Detach(attachment) => {
                self.attachments.remove(&attachment);
            }
            Command::Resize(size) => self.spool.resize(size),
            Command::Close => self.attachments.clear(),
        }
    }

    fn flush(&mut self) {
        let offset = self.offset;
        self.attachments
            .retain(|&attachment, flow| flow.flush(attachment, offset));
    }
}

impl Flow {
    /// Sends what the window allows; false once the connection is gone.
    fn flush(&mut self, attachment: AttachmentId, offset: u64) -> bool {
        if self.paused {
            if self.sent - self.acked > LOW_WATER {
                return true;
            }
            self.paused = false;
        }
        let mut start = 0;
        while start < self.queue.len() {
            let unacked = self.sent - self.acked;
            if unacked >= WINDOW {
                self.paused = true;
                break;
            }
            let take = (self.queue.len() - start)
                .min((WINDOW - unacked) as usize)
                .min(MAX_FRAME);
            let frame = Frame::Output {
                attachment,
                offset: offset - (self.queue.len() - start) as u64,
                data: self.queue[start..start + take].to_vec(),
            };
            if self.connection.frames.send(frame).is_err() {
                return false;
            }
            self.sent += take as u64;
            start += take;
        }
        self.queue.drain(..start);
        true
    }
}
