//! A protocol client over the daemon's socket, used by the dev client and
//! the tests.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use bitplane_protocol::{
    ClientInfo, Frame, HelloParams, HelloResult, Id, MIN_PROTOCOL_VERSION, Message, Notification,
    PROTOCOL_VERSION, Request, Response, RpcError, decode_envelope, method, rpc::Outcome,
};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::{mpsc, oneshot};

/// `None` once the connection is gone, so later requests fail at once.
type Pending = Arc<Mutex<Option<HashMap<Id, oneshot::Sender<Response>>>>>;

pub struct Client {
    outgoing: mpsc::UnboundedSender<Frame>,
    pending: Pending,
    next_id: AtomicI64,
    events: mpsc::UnboundedReceiver<Notification>,
    frames: mpsc::UnboundedReceiver<Frame>,
}

#[derive(Debug)]
pub enum ClientError {
    Rpc(RpcError),
    /// The daemon closed the connection before answering.
    Closed,
    Decode(serde_json::Error),
}

impl Client {
    /// Connects without saying `hello`; see [`Client::hello`].
    pub async fn connect(socket: &Path) -> std::io::Result<Self> {
        let (reader, writer) = UnixStream::connect(socket).await?.into_split();
        let (outgoing, to_write) = mpsc::unbounded_channel();
        let (events_tx, events) = mpsc::unbounded_channel();
        let (frames_tx, frames) = mpsc::unbounded_channel();
        let pending = Pending::new(Mutex::new(Some(HashMap::new())));
        tokio::spawn(write_frames(writer, to_write));
        tokio::spawn(read_frames(reader, pending.clone(), events_tx, frames_tx));
        Ok(Self {
            outgoing,
            pending,
            next_id: AtomicI64::new(1),
            events,
            frames,
        })
    }

    pub async fn hello(&self, client: &str) -> Result<HelloResult, ClientError> {
        let params = HelloParams {
            protocol_version: PROTOCOL_VERSION,
            min_protocol_version: MIN_PROTOCOL_VERSION,
            client: ClientInfo {
                name: client.into(),
                version: crate::DAEMON_VERSION.into(),
                via: None,
            },
        };
        let result = self
            .call(method::HELLO, Some(serde_json::to_value(params).unwrap()))
            .await?;
        serde_json::from_value(result).map_err(ClientError::Decode)
    }

    pub async fn call(&self, method: &str, params: Option<Value>) -> Result<Value, ClientError> {
        let id = Id::Number(self.next_id.fetch_add(1, Ordering::Relaxed));
        let response = self.request(Request::new(id, method, params)).await?;
        match response.outcome {
            Outcome::Result(value) => Ok(value),
            Outcome::Error(error) => Err(ClientError::Rpc(error)),
        }
    }

    /// Sends any request, including malformed ones, and waits for its reply.
    pub async fn request(&self, request: Request) -> Result<Response, ClientError> {
        let (tx, rx) = oneshot::channel();
        match self.pending.lock().unwrap().as_mut() {
            Some(pending) => pending.insert(request.id.clone(), tx),
            None => return Err(ClientError::Closed),
        };
        self.send_json(&request);
        rx.await.map_err(|_| ClientError::Closed)
    }

    pub fn notify(&self, method: &str, params: Option<Value>) {
        self.send_json(&Notification::new(method, params));
    }

    pub fn send_frame(&self, frame: Frame) {
        let _ = self.outgoing.send(frame);
    }

    pub async fn next_event(&mut self) -> Option<Notification> {
        self.events.recv().await
    }

    pub async fn next_frame(&mut self) -> Option<Frame> {
        self.frames.recv().await
    }

    /// Both streams at once, for loops that wait on either.
    pub fn streams(
        &mut self,
    ) -> (
        &mut mpsc::UnboundedReceiver<Notification>,
        &mut mpsc::UnboundedReceiver<Frame>,
    ) {
        (&mut self.events, &mut self.frames)
    }

    fn send_json(&self, message: &impl serde::Serialize) {
        let bytes = serde_json::to_vec(message).expect("protocol messages serialize");
        let _ = self.outgoing.send(Frame::Json(bytes));
    }
}

async fn write_frames(
    mut writer: tokio::net::unix::OwnedWriteHalf,
    mut outgoing: mpsc::UnboundedReceiver<Frame>,
) {
    while let Some(frame) = outgoing.recv().await {
        if writer.write_all(&frame.encode_envelope()).await.is_err() {
            return;
        }
    }
}

async fn read_frames(
    mut reader: tokio::net::unix::OwnedReadHalf,
    pending: Pending,
    events: mpsc::UnboundedSender<Notification>,
    frames: mpsc::UnboundedSender<Frame>,
) {
    let mut buf = Vec::with_capacity(8192);
    loop {
        while let Ok(Some((frame, used))) = decode_envelope(&buf) {
            buf.drain(..used);
            let Frame::Json(bytes) = frame else {
                let _ = frames.send(frame);
                continue;
            };
            match serde_json::from_slice::<Message>(&bytes) {
                Ok(Message::Response(response)) => {
                    let waiter = response
                        .id
                        .as_ref()
                        .and_then(|id| pending.lock().unwrap().as_mut()?.remove(id));
                    match waiter {
                        Some(waiter) => {
                            let _ = waiter.send(response);
                        }
                        // An error the daemon could not tie to a request.
                        None => tracing::warn!(?response, "unmatched response"),
                    }
                }
                Ok(Message::Notification(event)) => {
                    let _ = events.send(event);
                }
                Ok(Message::Request(_)) | Err(_) => {}
            }
        }
        match reader.read_buf(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
    }
    // Dropping the senders wakes every waiter with `Closed`.
    pending.lock().unwrap().take();
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientError::Rpc(error) => match &error.data {
                Some(data) => write!(f, "{:?}: {}", data.code, error.message),
                None => write!(f, "{} ({})", error.message, error.code),
            },
            ClientError::Closed => write!(f, "the daemon closed the connection"),
            ClientError::Decode(error) => write!(f, "unexpected reply: {error}"),
        }
    }
}

impl std::error::Error for ClientError {}
