//! `bitplane-sessiond dev`: a small client for poking a running daemon.

use bitplane_protocol::Frame;
use serde_json::json;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

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
/// user types [`DETACH`], the daemon reports an error, or it hangs up.
pub async fn attach(
    client: &mut Client,
    session: &str,
    (cols, rows): (u16, u16),
    mut input: impl AsyncRead + Unpin,
    mut output: impl AsyncWrite + Unpin,
) -> Result<(), ClientError> {
    let opened = client
        .call(
            "session.attachment.open",
            Some(json!({ "session": session, "cols": cols, "rows": rows })),
        )
        .await?;
    let attachment = opened["attachment"].as_u64().ok_or(ClientError::Closed)? as u32;

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
            frame = frames.recv() => match frame {
                Some(Frame::Snapshot { attachment: a, data, .. } | Frame::Output { attachment: a, data, .. })
                    if a == attachment =>
                {
                    if output.write_all(&data).await.is_err() {
                        break;
                    }
                    let _ = output.flush().await;
                }
                Some(_) => {}
                None => return Err(ClientError::Closed),
            },
            event = events.recv() => match event {
                Some(event) if event.method == "session.attachment.error"
                    && event.params.as_ref().and_then(|p| p["attachment"].as_u64()) == Some(attachment.into()) =>
                {
                    let message = event.params.as_ref().and_then(|p| p["message"].as_str()).unwrap_or("");
                    tracing::warn!(message, "attachment error");
                    break;
                }
                Some(_) => {}
                None => return Err(ClientError::Closed),
            },
        }
    }
    let _ = client
        .call(
            "session.attachment.close",
            Some(json!({ "attachment": attachment })),
        )
        .await;
    Ok(())
}
