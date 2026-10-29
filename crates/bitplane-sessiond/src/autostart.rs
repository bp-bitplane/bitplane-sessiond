//! Starting the daemon on demand (ADR 0022): dial, and on no answer run
//! `serve` detached and dial again until it answers.

use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::client::Client;
use crate::paths::Dirs;

/// How long a started daemon may take to accept a connection.
pub const START_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug)]
pub enum StartError {
    Spawn(std::io::Error),
    /// Nothing answered within [`START_TIMEOUT`]; the log says why.
    Timeout {
        log: PathBuf,
    },
}

/// Connects to this version's daemon, starting `exe serve` when none answers.
///
/// The socket is never unlinked here: a racing starter's daemon may have just
/// bound it. `serve` removes a stale one while holding the pidfile lock, and
/// every starter but one fails that lock and exits.
pub async fn connect_or_start(dirs: &Dirs, exe: &Path) -> Result<Client, StartError> {
    if let Ok(client) = Client::connect(&dirs.socket()).await {
        return Ok(client);
    }
    spawn_detached(dirs, exe).map_err(StartError::Spawn)?;

    let deadline = Instant::now() + START_TIMEOUT;
    let mut delay = Duration::from_millis(10);
    loop {
        tokio::time::sleep(delay).await;
        if let Ok(client) = Client::connect(&dirs.socket()).await {
            return Ok(client);
        }
        if Instant::now() >= deadline {
            return Err(StartError::Timeout { log: dirs.log() });
        }
        delay = (delay * 2).min(Duration::from_millis(200));
    }
}

fn spawn_detached(dirs: &Dirs, exe: &Path) -> std::io::Result<()> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dirs.state)?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(dirs.log())?;
    let mut command = Command::new(exe);
    command
        .arg("serve")
        .arg("--runtime-dir")
        .arg(&dirs.runtime)
        .arg("--state-dir")
        .arg(&dirs.state)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    // SAFETY: setsid is async-signal-safe and touches no parent state.
    unsafe {
        command.pre_exec(|| rustix::process::setsid().map(drop).map_err(Into::into));
    }
    let mut child = command.spawn()?;
    // Reaps the daemon if it exits while this process still runs.
    std::thread::spawn(move || child.wait());
    Ok(())
}

impl std::fmt::Display for StartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StartError::Spawn(error) => write!(f, "could not start the daemon: {error}"),
            StartError::Timeout { log } => write!(
                f,
                "the daemon did not answer within {}s; see {}",
                START_TIMEOUT.as_secs(),
                log.display()
            ),
        }
    }
}

impl std::error::Error for StartError {}
