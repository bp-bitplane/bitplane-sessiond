mod support;

use std::os::fd::AsRawFd;
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use bitplane_sessiond::autostart::{START_TIMEOUT, connect_or_start};
use bitplane_sessiond::client::Client;
use bitplane_sessiond::paths::Dirs;
use bitplane_sessiond::pidfile::Pidfile;
use support::short_tempdir;

const EXE: &str = env!("CARGO_BIN_EXE_bitplane-sessiond");

#[tokio::test]
async fn a_client_with_no_daemon_starts_one() {
    let host = Host::new();

    let started = Instant::now();
    let client = connect_or_start(&host.dirs, Path::new(EXE)).await.unwrap();
    assert!(started.elapsed() < START_TIMEOUT);

    let hello = client.hello("test").await.unwrap();
    assert_eq!(hello.daemon.name, "bitplane-sessiond");
    assert!(
        host.dirs.log().exists(),
        "the daemon's output goes to its log"
    );
    assert!(host.dirs.pidfile().starts_with(&host.dirs.runtime));
    assert!(host.dirs.log().starts_with(&host.dirs.state));
}

#[tokio::test]
async fn racing_starters_end_up_on_one_daemon() {
    let host = Host::new();

    let (a, b) = tokio::join!(
        connect_or_start(&host.dirs, Path::new(EXE)),
        connect_or_start(&host.dirs, Path::new(EXE)),
    );
    let (a, b) = (a.unwrap(), b.unwrap());

    let (a, b) = tokio::join!(a.hello("a"), b.hello("b"));
    assert_eq!(a.unwrap().daemon.instance, b.unwrap().daemon.instance);
}

#[tokio::test]
async fn a_stale_socket_is_replaced() {
    let host = Host::new();
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&host.dirs.runtime)
        .unwrap();
    drop(std::os::unix::net::UnixListener::bind(host.dirs.socket()).unwrap());
    assert!(Client::connect(&host.dirs.socket()).await.is_err());

    let client = connect_or_start(&host.dirs, Path::new(EXE)).await.unwrap();

    client.hello("test").await.unwrap();
}

#[tokio::test]
async fn serve_accepts_a_socket_activated_listener() {
    let host = Host::new();
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&host.dirs.runtime)
        .unwrap();
    let listener = std::os::unix::net::UnixListener::bind(host.dirs.socket()).unwrap();
    let fd = listener.as_raw_fd();

    let mut command = std::process::Command::new("/bin/sh");
    command
        .args([
            "-c",
            r#"LISTEN_PID=$$ LISTEN_FDS=1 exec "$0" "$@""#,
            EXE,
            "serve",
        ])
        .arg("--runtime-dir")
        .arg(&host.dirs.runtime)
        .arg("--state-dir")
        .arg(&host.dirs.state)
        .env("SHELL", "/bin/sh")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // SAFETY: dup2 is async-signal-safe; fd 3 is where LISTEN_FDS starts.
    unsafe {
        command.pre_exec(move || {
            if libc::dup2(fd, 3) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn().unwrap();
    drop(listener);

    let hello = Client::connect(&host.dirs.socket())
        .await
        .unwrap()
        .hello("test")
        .await
        .unwrap();

    assert_eq!(hello.daemon.name, "bitplane-sessiond");
    child.kill().unwrap();
    child.wait().unwrap();
}

/// A host's directories in a temp dir. The daemon an auto-start leaves
/// behind is sent SIGTERM through its pidfile when the test ends, but only
/// while it still holds the lock, so a recycled pid is never signalled.
struct Host {
    dirs: Dirs,
    _root: tempfile::TempDir,
}

impl Host {
    fn new() -> Self {
        let (root, path) = short_tempdir();
        Self {
            dirs: Dirs {
                runtime: path.join("run"),
                state: path.join("state"),
            },
            _root: root,
        }
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        if Pidfile::acquire(&self.dirs.pidfile()).is_ok() {
            return;
        }
        let Ok(pid) = std::fs::read_to_string(self.dirs.pidfile()) else {
            return;
        };
        let Some(pid) = pid
            .trim()
            .parse()
            .ok()
            .and_then(rustix::process::Pid::from_raw)
        else {
            return;
        };
        let _ = rustix::process::kill_process(pid, rustix::process::Signal::TERM);
        let deadline = Instant::now() + Duration::from_secs(2);
        while self.dirs.socket().exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
