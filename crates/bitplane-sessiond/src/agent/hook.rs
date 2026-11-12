//! The hook channel (ADR 0029): `bitplane-sessiond hook` forwards one hook
//! payload per connection to the daemon, outside the client protocol.

use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::AsyncReadExt;
use tokio::net::UnixListener;

use super::{HOOK_SOCKET_VAR, HOOK_TOKEN_VAR, SESSION_VAR, process, read_within};

/// The forwarder's whole budget, well inside the 1.5 s Claude Code gives
/// every `SessionEnd` hook together.
pub const FORWARD_TIMEOUT: Duration = Duration::from_millis(800);
/// How long the daemon waits for one hook's bytes.
const READ_TIMEOUT: Duration = Duration::from_secs(1);
const MAX_MESSAGE: u64 = 8 << 20;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub session: String,
    pub token: String,
    /// The process that ran the hook and its parent, as the forwarder saw
    /// them: which `claude` sent it, even after a shell between them exited.
    #[serde(default)]
    pub parent: Option<u32>,
    #[serde(default)]
    pub grandparent: Option<u32>,
    pub payload: Value,
}

/// `bitplane-sessiond hook`: never fails and never outlives
/// [`FORWARD_TIMEOUT`], so it cannot stall the agent.
pub fn forward() {
    let deadline = Instant::now() + FORWARD_TIMEOUT;
    let parent = std::os::unix::process::parent_id();
    let grandparent = process::parent(parent);
    // The reader thread of a stdin that never closes dies with the process.
    let Some(stdin) = read_within(std::io::stdin().take(MAX_MESSAGE), deadline) else {
        return;
    };
    let (Some(socket), Ok(session), Ok(token)) = (
        std::env::var_os(HOOK_SOCKET_VAR),
        std::env::var(SESSION_VAR),
        std::env::var(HOOK_TOKEN_VAR),
    ) else {
        return;
    };
    let Ok(payload) = serde_json::from_slice(&stdin) else {
        return;
    };
    let message = serde_json::to_vec(&Message {
        session,
        token,
        parent: Some(parent),
        grandparent,
        payload,
    })
    .expect("hook messages serialize");
    let Ok(mut stream) = std::os::unix::net::UnixStream::connect(socket) else {
        return;
    };
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() || stream.set_write_timeout(Some(left)).is_err() {
        return;
    }
    let _ = stream.write_all(&message);
}

/// The channel's socket, private to the daemon's user. Replacing a stale one
/// is safe only while holding the pidfile lock.
pub fn bind(path: &Path) -> std::io::Result<UnixListener> {
    match std::fs::remove_file(path) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => return Err(error),
        _ => {}
    }
    let listener = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

/// Hands every message to `deliver`, one connection at a time: Claude Code
/// waits for each hook before the next, so accept order is event order.
/// No peer-uid check: macOS can't tell the uid of a hook that already
/// exited; the socket's mode and the session token keep others out.
pub async fn serve(listener: UnixListener, deliver: impl Fn(Message)) {
    loop {
        let Ok((mut stream, _)) = listener.accept().await else {
            tokio::time::sleep(Duration::from_millis(50)).await;
            continue;
        };
        let mut bytes = Vec::new();
        let mut limited = (&mut stream).take(MAX_MESSAGE);
        let read = limited.read_to_end(&mut bytes);
        if !matches!(tokio::time::timeout(READ_TIMEOUT, read).await, Ok(Ok(_))) {
            continue;
        }
        match serde_json::from_slice(&bytes) {
            Ok(message) => deliver(message),
            Err(error) => tracing::debug!(%error, "dropped an unreadable hook message"),
        }
    }
}
