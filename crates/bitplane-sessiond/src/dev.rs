//! `bitplane-sessiond dev`: a small client for poking a running daemon.

use bitplane_protocol::{Frame, Notification, method};
use serde_json::json;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;

use crate::client::{Client, ClientError};

/// Ctrl-] ends an attach, as in telnet.
pub const DETACH: u8 = 0x1d;

/// Prints every event as one JSON line until the connection closes.
pub async fn events(client: &mut Client, mut out: impl AsyncWrite + Unpin) -> std::io::Result<()> {
    while let Some(event) = client.next_event().await {
        let mut line = serde_json::to_vec(&event)?;
        line.push(b'\n');
        out.write_all(&line).await?;
    }
    Ok(())
}

/// Bridges `input` and `output` to one attachment of `session` until the
/// user types [`DETACH`], the session goes away, or the daemon hangs up.
/// Each size from `resizes` resizes the attachment.
pub async fn attach(
    client: &mut Client,
    session: &str,
    (cols, rows): (u16, u16),
    mut resizes: mpsc::UnboundedReceiver<(u16, u16)>,
    mut input: impl AsyncRead + Unpin,
    mut output: impl AsyncWrite + Unpin,
) -> Result<(), ClientError> {
    let opened = client
        .call(
            method::SESSION_ATTACHMENT_OPEN,
            Some(json!({ "session": session, "cols": cols, "rows": rows })),
        )
        .await?;
    let attachment = opened["attachment"].as_u64().ok_or(ClientError::Closed)? as u32;

    let mut drawn = 0u64;
    let mut buf = [0u8; 4096];
    loop {
        let (events, frames) = client.streams();
        tokio::select! {
            read = input.read(&mut buf) => {
                let read = match read {
                    Ok(0) | Err(_) => break,
                    Ok(read) => read,
                };
                let chunk = &buf[..read];
                let detach = chunk.iter().position(|&byte| byte == DETACH);
                let data = &chunk[..detach.unwrap_or(read)];
                if !data.is_empty() {
                    client.send_frame(Frame::Input { attachment, data: data.to_vec() });
                }
                if detach.is_some() {
                    break;
                }
            }
            Some((cols, rows)) = resizes.recv() => {
                let _ = client
                    .call(
                        method::SESSION_ATTACHMENT_RESIZE,
                        Some(json!({ "attachment": attachment, "cols": cols, "rows": rows })),
                    )
                    .await;
            }
            frame = frames.recv() => {
                let (data, snapshot) = match frame {
                    Some(Frame::Snapshot { attachment: a, data, .. }) if a == attachment => (data, true),
                    Some(Frame::Output { attachment: a, data, .. }) if a == attachment => (data, false),
                    Some(_) => continue,
                    None => return Err(ClientError::Closed),
                };
                if snapshot && output.write_all(CLEAR).await.is_err() {
                    break;
                }
                if output.write_all(&data).await.is_err() {
                    break;
                }
                let _ = output.flush().await;
                drawn += data.len() as u64;
                client.notify(
                    method::SESSION_ATTACHMENT_ACK,
                    Some(json!({ "attachment": attachment, "bytes": drawn })),
                );
            }
            event = events.recv() => match event {
                Some(event) if ends(&event, session, attachment) => {
                    let message = event.params.as_ref().and_then(|p| p["message"].as_str()).unwrap_or("");
                    tracing::warn!(method = event.method, message, "attachment ended");
                    break;
                }
                Some(_) => {}
                None => return Err(ClientError::Closed),
            },
        }
    }
    let _ = client
        .call(
            method::SESSION_ATTACHMENT_CLOSE,
            Some(json!({ "attachment": attachment })),
        )
        .await;
    Ok(())
}

/// Clears the screen and its scrollback before a snapshot redraws it.
const CLEAR: &[u8] = b"\x1b[H\x1b[2J\x1b[3J";

fn ends(event: &Notification, session: &str, attachment: u32) -> bool {
    let params = event.params.as_ref();
    match event.method.as_str() {
        method::SESSION_ATTACHMENT_ERROR => {
            params.and_then(|p| p["attachment"].as_u64()) == Some(attachment.into())
        }
        method::SESSION_REMOVED => params.and_then(|p| p["session"].as_str()) == Some(session),
        _ => false,
    }
}
