//! One client connection: `hello` first, then requests run concurrently and
//! a single writer task owns the socket's write half.

use std::sync::Arc;
use std::time::Duration;

use bitplane_protocol::{
    DaemonInfo, ErrorCode, Frame, HelloParams, HelloResult, Id, MIN_PROTOCOL_VERSION, Message,
    Notification, PROTOCOL_VERSION, Request, Response, RpcError, decode_envelope, hello::negotiate,
    method,
};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::{broadcast, mpsc};

use crate::module::{Call, Connection, Registry};

pub struct Shared {
    pub registry: Registry,
    pub daemon: DaemonInfo,
    pub events: broadcast::Sender<Notification>,
}

pub async fn serve_connection(stream: UnixStream, id: u64, shared: Arc<Shared>) {
    let (mut reader, writer) = stream.into_split();
    let (frames, outgoing) = mpsc::unbounded_channel();
    let writer = tokio::spawn(write_frames(writer, outgoing));
    let connection = Connection { id, frames };

    let mut forwarder = None;
    let mut buf = Vec::with_capacity(8192);
    'read: loop {
        loop {
            let (frame, used) = match decode_envelope(&buf) {
                Ok(Some(decoded)) => decoded,
                Ok(None) => break,
                Err(error) => {
                    tracing::debug!(connection = id, %error, "closing on a bad frame");
                    break 'read;
                }
            };
            buf.drain(..used);
            let helloed = forwarder.is_some();
            match (frame, helloed) {
                (Frame::Json(bytes), false) => match hello(&bytes, &shared) {
                    Ok(response) => {
                        send(&connection, &response);
                        forwarder = Some(tokio::spawn(forward_events(
                            shared.events.subscribe(),
                            connection.clone(),
                        )));
                    }
                    Err(response) => {
                        send(&connection, &response);
                        break 'read;
                    }
                },
                (Frame::Json(bytes), true) => handle_message(&bytes, &connection, &shared),
                (Frame::Input { attachment, data }, true) => {
                    shared.registry.input(&connection, attachment, data)
                }
                (frame, _) => {
                    tracing::debug!(connection = id, kind = ?frame.kind(), "closing on an unexpected frame");
                    break 'read;
                }
            }
        }
        match reader.read_buf(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
    }

    if let Some(forwarder) = forwarder {
        forwarder.abort();
    }
    drop(connection);
    // Let a final error reply reach the client before the socket closes.
    let _ = tokio::time::timeout(Duration::from_secs(1), writer).await;
}

/// The reply to the connection's first message: `Ok` opens the connection,
/// `Err` is sent and then the connection closes.
fn hello(bytes: &[u8], shared: &Shared) -> Result<Response, Response> {
    let request = match serde_json::from_slice::<Message>(bytes) {
        Ok(Message::Request(request)) if request.method == method::HELLO => request,
        Ok(Message::Request(request)) => return Err(hello_required(Some(request.id))),
        Ok(_) => return Err(hello_required(None)),
        Err(error) => return Err(parse_error(error)),
    };
    let params: HelloParams = match request.params.map(serde_json::from_value).transpose() {
        Ok(Some(params)) => params,
        Ok(None) => return Err(invalid_params(request.id, "hello needs params")),
        Err(error) => return Err(invalid_params(request.id, &error.to_string())),
    };
    let Some(version) = negotiate(&params, MIN_PROTOCOL_VERSION, PROTOCOL_VERSION) else {
        let error = RpcError::protocol(
            ErrorCode::VersionMismatch,
            format!(
                "client speaks protocol {}..={}, this daemon {MIN_PROTOCOL_VERSION}..={PROTOCOL_VERSION}",
                params.min_protocol_version, params.protocol_version
            ),
            Some(json!({
                "minProtocolVersion": MIN_PROTOCOL_VERSION,
                "protocolVersion": PROTOCOL_VERSION,
            })),
        );
        return Err(Response::err(Some(request.id), error));
    };
    let result = HelloResult {
        protocol_version: version,
        daemon: shared.daemon.clone(),
        capabilities: shared.registry.capabilities(),
    };
    Ok(Response::ok(
        request.id,
        serde_json::to_value(result).expect("hello result serializes"),
    ))
}

fn handle_message(bytes: &[u8], connection: &Connection, shared: &Arc<Shared>) {
    let (id, method, params) = match serde_json::from_slice::<Message>(bytes) {
        Ok(Message::Request(Request {
            id, method, params, ..
        })) => (Some(id), method, params),
        Ok(Message::Notification(Notification { method, params, .. })) => (None, method, params),
        Ok(Message::Response(_)) => return,
        Err(error) => return send(connection, &parse_error(error)),
    };
    if method == method::HELLO {
        let error =
            RpcError::protocol(ErrorCode::HelloRepeated, "hello was already answered", None);
        if let Some(id) = id {
            send(connection, &Response::err(Some(id), error));
        }
        return;
    }
    let shared = shared.clone();
    let call = Call {
        method,
        params,
        connection: connection.clone(),
    };
    tokio::spawn(async move {
        let connection = call.connection.clone();
        let outcome = shared.registry.dispatch(call).await;
        if let Some(id) = id {
            let response = match outcome {
                Ok(result) => Response::ok(id, result),
                Err(error) => Response::err(Some(id), error),
            };
            send(&connection, &response);
        }
    });
}

async fn forward_events(mut events: broadcast::Receiver<Notification>, connection: Connection) {
    loop {
        match events.recv().await {
            Ok(event) => send(&connection, &event),
            Err(broadcast::error::RecvError::Lagged(missed)) => {
                tracing::warn!(
                    connection = connection.id,
                    missed,
                    "connection fell behind on events"
                );
            }
            Err(broadcast::error::RecvError::Closed) => return,
        }
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
    let _ = writer.shutdown().await;
}

fn send(connection: &Connection, message: &impl serde::Serialize) {
    let bytes = serde_json::to_vec(message).expect("protocol messages serialize");
    let _ = connection.frames.send(Frame::Json(bytes));
}

fn hello_required(id: Option<Id>) -> Response {
    let error = RpcError::protocol(
        ErrorCode::HelloRequired,
        "the first request on a connection must be hello",
        None,
    );
    Response::err(id, error)
}

fn parse_error(error: serde_json::Error) -> Response {
    Response::err(
        None,
        RpcError::new(RpcError::PARSE_ERROR, error.to_string()),
    )
}

fn invalid_params(id: Id, message: &str) -> Response {
    Response::err(Some(id), RpcError::new(RpcError::INVALID_PARAMS, message))
}
