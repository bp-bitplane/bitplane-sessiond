use std::future::Future;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use bitplane_protocol::DaemonInfo;
use tokio::net::UnixListener;
use tokio::sync::broadcast;

use crate::connection::{Shared, serve_connection};
use crate::env::{self, EnvMap};
use crate::module::{Events, Module, Registry};
use crate::paths::Dirs;
use crate::pidfile::{Pidfile, PidfileError};
use crate::{DAEMON_NAME, DAEMON_VERSION};

pub struct Config {
    pub dirs: Dirs,
    /// A socket handed over by systemd or launchd (`LISTEN_FDS`).
    pub listener: Option<std::os::unix::net::UnixListener>,
    /// Connections from any other uid are closed unread (ADR 0022).
    pub peer_uid: u32,
    pub modules: Vec<Arc<dyn Module>>,
    /// `None` captures the login-shell environment at startup.
    pub env: Option<EnvMap>,
}

#[derive(Debug)]
pub enum ServeError {
    AlreadyRunning,
    Io(std::io::Error),
}

/// Runs the daemon until `shutdown` resolves.
pub async fn serve(config: Config, shutdown: impl Future<Output = ()>) -> Result<(), ServeError> {
    let Config {
        dirs,
        listener,
        peer_uid,
        modules,
        env,
    } = config;
    create_private_dir(&dirs.runtime)?;
    create_private_dir(&dirs.state)?;
    let _pidfile = Pidfile::acquire(&dirs.pidfile()).map_err(|error| match error {
        PidfileError::Held => ServeError::AlreadyRunning,
        PidfileError::Io(error) => ServeError::Io(error),
    })?;

    let socket = dirs.socket();
    let bound_here = listener.is_none();
    let listener = match listener {
        Some(listener) => {
            listener.set_nonblocking(true)?;
            UnixListener::from_std(listener)?
        }
        None => {
            // Safe only while holding the pidfile lock: no live instance owns it.
            match std::fs::remove_file(&socket) {
                Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                    return Err(error.into());
                }
                _ => {}
            }
            let listener = UnixListener::bind(&socket)?;
            std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;
            listener
        }
    };

    // After bind, so a starter's dial succeeds at once while `hello` waits;
    // before the probes, so they see the user's `PATH`.
    let env = match env {
        Some(env) => env,
        None => {
            let (env, warning) = env::capture(&env::original(), env::CAPTURE_TIMEOUT).await;
            if let Some(warning) = warning {
                tracing::warn!(%warning, "using the daemon's own environment, not the login shell's");
            }
            env
        }
    };

    let events = broadcast::channel(1024).0;
    let registry = Registry::start(modules, &dirs.state, Arc::new(env), Events(events.clone()))?;
    let daemon = DaemonInfo {
        name: DAEMON_NAME.into(),
        version: DAEMON_VERSION.into(),
        instance: uuid::Uuid::new_v4().to_string(),
    };
    let shared = Arc::new(Shared {
        registry,
        daemon,
        events,
    });
    tracing::info!(socket = %socket.display(), instance = %shared.daemon.instance, "listening");

    let next_id = AtomicU64::new(1);
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let Ok((stream, _)) = accepted else { continue };
                match stream.peer_cred() {
                    Ok(cred) if cred.uid() == peer_uid => {
                        let id = next_id.fetch_add(1, Ordering::Relaxed);
                        tokio::spawn(serve_connection(stream, id, shared.clone()));
                    }
                    Ok(cred) => tracing::warn!(uid = cred.uid(), "refused a connection from another uid"),
                    Err(error) => tracing::warn!(%error, "refused a connection without peer credentials"),
                }
            }
            () = &mut shutdown => break,
        }
    }
    if bound_here {
        let _ = std::fs::remove_file(&socket);
    }
    Ok(())
}

fn create_private_dir(path: &Path) -> std::io::Result<()> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
}

impl From<std::io::Error> for ServeError {
    fn from(error: std::io::Error) -> Self {
        ServeError::Io(error)
    }
}

impl std::fmt::Display for ServeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ServeError::AlreadyRunning => {
                write!(
                    f,
                    "another daemon instance of this protocol version is running"
                )
            }
            ServeError::Io(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for ServeError {}
