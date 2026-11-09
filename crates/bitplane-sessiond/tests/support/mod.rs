//! A real daemon on a temp socket and state dir, served in-process so tests
//! can plug in their own modules.
#![allow(dead_code)]

use std::path::PathBuf;
use std::process::{Child, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bitplane_protocol::RpcError;
use bitplane_sessiond::client::Client;
use bitplane_sessiond::env::EnvMap;
use bitplane_sessiond::module::{Availability, BoxFuture, Call, Module};
use bitplane_sessiond::paths::Dirs;
use bitplane_sessiond::server::{Config, ServeError, serve};
use bitplane_sessiond::session::Sessions;
use serde_json::{Value, json};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

pub const DAEMON_EXE: &str = env!("CARGO_BIN_EXE_bitplane-sessiond");

pub struct TestDaemon {
    pub dirs: Dirs,
    _root: tempfile::TempDir,
    stop: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<Result<(), ServeError>>>,
}

impl TestDaemon {
    pub async fn start(modules: Vec<Arc<dyn Module>>) -> Self {
        Self::start_with(modules, own_uid()).await
    }

    pub async fn start_with(modules: Vec<Arc<dyn Module>>, peer_uid: u32) -> Self {
        Self::start_full(modules, peer_uid, EnvMap::new()).await
    }

    pub async fn start_with_env(modules: Vec<Arc<dyn Module>>, env: EnvMap) -> Self {
        Self::start_full(modules, own_uid(), env).await
    }

    async fn start_full(modules: Vec<Arc<dyn Module>>, peer_uid: u32, env: EnvMap) -> Self {
        // Short paths: macOS caps a socket path at 104 bytes.
        let root = tempfile::Builder::new()
            .prefix("sd")
            .tempdir_in("/tmp")
            .unwrap();
        let dirs = Dirs {
            runtime: root.path().join("run"),
            state: root.path().join("state"),
        };
        let daemon = Self::serve(dirs, root, modules, peer_uid, env);
        daemon.wait_for_socket().await;
        daemon
    }

    /// A second daemon on the same directories, which must refuse to start.
    pub async fn start_again(&self) -> Result<(), ServeError> {
        let config = Config {
            dirs: self.dirs.clone(),
            listener: None,
            peer_uid: own_uid(),
            modules: vec![],
            env: Some(EnvMap::new()),
        };
        serve(config, std::future::pending()).await
    }

    pub async fn connect(&self) -> Client {
        Client::connect(&self.dirs.socket()).await.unwrap()
    }

    pub async fn connect_hello(&self) -> Client {
        let client = self.connect().await;
        client.hello("test").await.unwrap();
        client
    }

    /// Serves the same directories again once this daemon has stopped, as
    /// the next instance after a restart.
    pub async fn restart(mut self, modules: Vec<Arc<dyn Module>>, env: EnvMap) -> Self {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(task) = self.task.take() {
            task.await.unwrap().unwrap();
        }
        let daemon = Self::serve(self.dirs, self._root, modules, own_uid(), env);
        daemon.wait_for_socket().await;
        daemon
    }

    pub async fn stop(&mut self) -> Result<(), ServeError> {
        let _ = self.stop.take().unwrap().send(());
        self.task.take().unwrap().await.unwrap()
    }

    fn serve(
        dirs: Dirs,
        root: tempfile::TempDir,
        modules: Vec<Arc<dyn Module>>,
        peer_uid: u32,
        env: EnvMap,
    ) -> Self {
        let (stop, stopped) = oneshot::channel::<()>();
        let config = Config {
            dirs: dirs.clone(),
            listener: None,
            peer_uid,
            modules,
            env: Some(env),
        };
        let task = tokio::spawn(serve(config, async move {
            let _ = stopped.await;
        }));
        Self {
            dirs,
            _root: root,
            stop: Some(stop),
            task: Some(task),
        }
    }

    async fn wait_for_socket(&self) {
        for _ in 0..200 {
            if self.dirs.socket().exists() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("daemon did not bind {}", self.dirs.socket().display());
    }
}

pub fn own_uid() -> u32 {
    rustix::process::getuid().as_raw()
}

pub fn short_tempdir() -> (tempfile::TempDir, PathBuf) {
    let root = tempfile::Builder::new()
        .prefix("sd")
        .tempdir_in("/tmp")
        .unwrap();
    let path = root.path().to_path_buf();
    (root, path)
}

/// A module that answers `<namespace>.echo` with its params, after
/// `<namespace>.sleep` waits the given milliseconds.
pub struct Echo {
    pub namespace: &'static str,
    pub availability: Availability,
    pub dependencies: &'static [&'static str],
}

impl Echo {
    pub fn available(namespace: &'static str) -> Arc<dyn Module> {
        Arc::new(Echo {
            namespace,
            availability: Availability::Available { info: None },
            dependencies: &[],
        })
    }
}

impl Module for Echo {
    fn namespace(&self) -> &'static str {
        self.namespace
    }

    fn dependencies(&self) -> &'static [&'static str] {
        self.dependencies
    }

    fn probe(&self, _env: &EnvMap) -> Availability {
        self.availability.clone()
    }

    fn handle(&self, call: Call) -> BoxFuture<'_, Result<Value, RpcError>> {
        Box::pin(async move {
            let params = call.params.unwrap_or(Value::Null);
            match call.method.strip_prefix(self.namespace).unwrap_or_default() {
                ".echo" => Ok(params),
                ".sleep" => {
                    let ms = params.as_u64().unwrap_or(0);
                    tokio::time::sleep(Duration::from_millis(ms)).await;
                    Ok(Value::from(ms))
                }
                _ => Err(RpcError::new(RpcError::METHOD_NOT_FOUND, call.method)),
            }
        })
    }
}

/// What sessions are spawned with: `/bin/sh` and a `HOME` of its own, so no
/// profile of the host's runs.
pub fn shell_env(home: &std::path::Path) -> EnvMap {
    EnvMap::from([
        ("SHELL".into(), "/bin/sh".into()),
        ("HOME".into(), home.display().to_string()),
        ("PATH".into(), "/usr/bin:/bin".into()),
        ("PS1".into(), "$ ".into()),
    ])
}

/// One attachment's stream as a client sees it: the snapshot, then output
/// checked to continue exactly from it, acknowledged as it is drawn.
pub struct Screen {
    pub attachment: u32,
    pub snapshot_offset: u64,
    /// Snapshot and output bytes, in order.
    pub data: Vec<u8>,
    /// The session offset the next OUTPUT frame must start at.
    pub next_offset: u64,
    /// SNAPSHOT frames after the first: each one restarts the stream.
    pub resnapshots: u32,
    snapshot_len: usize,
    drawn: u64,
    stalled: bool,
}

impl Screen {
    /// Waits for the attachment's SNAPSHOT, which must be its first frame.
    pub async fn attach(client: &mut Client, session: &str) -> Self {
        Self::attach_with(
            client,
            serde_json::json!({ "session": session, "cols": 80, "rows": 24 }),
        )
        .await
    }

    /// Attaches with `session.attachment.open` params of the test's own.
    pub async fn attach_with(client: &mut Client, params: Value) -> Self {
        let opened = client
            .call("session.attachment.open", Some(params))
            .await
            .unwrap();
        let attachment = opened["attachment"].as_u64().unwrap() as u32;
        let frame = next_frame(client).await;
        let bitplane_protocol::Frame::Snapshot {
            attachment: a,
            offset,
            data,
        } = frame
        else {
            panic!("expected a SNAPSHOT first, got {frame:?}")
        };
        assert_eq!(a, attachment);
        let mut screen = Screen {
            attachment,
            snapshot_offset: offset,
            next_offset: offset,
            snapshot_len: data.len(),
            drawn: 0,
            data: Vec::new(),
            resnapshots: 0,
            stalled: false,
        };
        screen.draw(client, &data);
        screen
    }

    pub fn type_text(&self, client: &Client, text: &str) {
        client.send_frame(bitplane_protocol::Frame::Input {
            attachment: self.attachment,
            data: text.as_bytes().to_vec(),
        });
    }

    /// Closes the attachment and drops the frames it was sent before the reply,
    /// which the client has already read.
    pub async fn close(self, client: &mut Client) {
        client
            .call(
                "session.attachment.close",
                Some(serde_json::json!({ "attachment": self.attachment })),
            )
            .await
            .unwrap();
        while let Ok(frame) = client.streams().1.try_recv() {
            assert!(
                matches!(
                    frame,
                    bitplane_protocol::Frame::Output { attachment, .. }
                        | bitplane_protocol::Frame::Snapshot { attachment, .. }
                        if attachment == self.attachment
                ),
                "unexpected frame {frame:?}"
            );
        }
    }

    /// Reads OUTPUT until the stream contains `needle`.
    pub async fn wait_for(&mut self, client: &mut Client, needle: &str) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        // Only the new bytes are searched, so a long stream stays cheap to follow.
        let mut from = 0;
        while !contains(&self.data[from..], needle.as_bytes()) {
            from = self.data.len().saturating_sub(needle.len());
            let frame = tokio::time::timeout_at(deadline, client.next_frame())
                .await
                .unwrap_or_else(|_| {
                    panic!("no {needle:?} in {:?}", String::from_utf8_lossy(&self.data))
                })
                .expect("connection open");
            let resnapshots = self.resnapshots;
            self.take(client, frame);
            if self.resnapshots != resnapshots {
                from = 0;
            }
        }
    }

    pub fn take(&mut self, client: &Client, frame: bitplane_protocol::Frame) {
        match frame {
            bitplane_protocol::Frame::Output {
                attachment,
                offset,
                data,
            } if attachment == self.attachment => {
                assert_eq!(offset, self.next_offset, "OUTPUT must continue the stream");
                self.next_offset += data.len() as u64;
                self.draw(client, &data);
            }
            bitplane_protocol::Frame::Snapshot {
                attachment,
                offset,
                data,
            } if attachment == self.attachment => {
                self.resnapshots += 1;
                self.snapshot_offset = offset;
                self.next_offset = offset;
                self.snapshot_len = data.len();
                self.data.clear();
                self.draw(client, &data);
            }
            other => panic!("unexpected frame {other:?}"),
        }
    }

    /// Reads frames until none arrives for a while.
    pub async fn drain(&mut self, client: &mut Client) {
        while let Ok(Some(frame)) =
            tokio::time::timeout(Duration::from_millis(300), client.next_frame()).await
        {
            self.take(client, frame);
        }
    }

    /// Stops acknowledging what is drawn, as a client that went to sleep.
    pub fn stall(&mut self) {
        self.stalled = true;
    }

    /// Acknowledges everything drawn so far and keeps acknowledging.
    pub fn resume(&mut self, client: &Client) {
        self.stalled = false;
        self.ack(client);
    }

    /// The OUTPUT bytes, without the snapshot.
    pub fn output(&self) -> &[u8] {
        &self.data[self.snapshot_len..]
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.data).into_owned()
    }

    fn draw(&mut self, client: &Client, data: &[u8]) {
        self.data.extend_from_slice(data);
        self.drawn += data.len() as u64;
        if !self.stalled {
            self.ack(client);
        }
    }

    fn ack(&self, client: &Client) {
        client.notify(
            "session.attachment.ack",
            Some(serde_json::json!({ "attachment": self.attachment, "bytes": self.drawn })),
        );
    }
}

pub async fn next_frame(client: &mut Client) -> bitplane_protocol::Frame {
    tokio::time::timeout(Duration::from_secs(10), client.next_frame())
        .await
        .expect("a frame arrives")
        .expect("connection open")
}

/// Shell commands for `chunks` bursts of 1500 numbered lines, 20 ms apart:
/// `<prefix><chunk> <line>`. Each burst is written in large pieces, and a
/// client that acks what it draws keeps up with it.
pub fn paced_lines(prefix: &str, chunks: u32) -> String {
    format!(
        "for i in $(seq 1 {chunks}); do seq 1 1500 | sed \"s/^/{prefix}$i /\" | cat; sleep 0.02; done"
    )
}

pub fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

pub async fn open(client: &Client, params: Value) -> Value {
    let result = client.call("session.open", Some(params)).await.unwrap();
    result["session"].clone()
}

pub async fn sessions_daemon() -> (tempfile::TempDir, TestDaemon) {
    let home = tempfile::tempdir().unwrap();
    let daemon = TestDaemon::start_with_env(vec![Sessions::module()], shell_env(home.path())).await;
    (home, daemon)
}

pub async fn next_event(client: &mut Client, method: &str) -> Value {
    loop {
        let event = tokio::time::timeout(std::time::Duration::from_secs(10), client.next_event())
            .await
            .unwrap_or_else(|_| panic!("no {method} event"))
            .expect("connection open");
        if event.method == method {
            return event.params.unwrap_or_default();
        }
    }
}

pub async fn wait_until(done: impl Fn() -> bool) {
    for _ in 0..500 {
        if done() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("timed out");
}

#[derive(Clone, Copy, Debug)]
pub enum Stop {
    ShutdownWithRestore,
    Sigterm,
    Sigkill,
}

/// A host's directories in a temp dir, keyed by protocol version like the
/// real ones, so other versions' dirs sit next to them.
pub struct Host {
    pub dirs: Dirs,
    pub base: PathBuf,
    pub home: tempfile::TempDir,
    _root: tempfile::TempDir,
}

impl Host {
    pub fn new() -> Self {
        let (root, base) = short_tempdir();
        Self {
            dirs: Dirs::for_version(&base.join("run"), &base.join("state")),
            base,
            home: tempfile::tempdir().unwrap(),
            _root: root,
        }
    }

    /// `bitplane-sessiond serve` with a bare `/bin/sh` environment.
    pub fn serve(&self) -> Daemon {
        self.serve_with(&shell_env(self.home.path()))
    }

    /// `bitplane-sessiond serve` with only `env`.
    pub fn serve_with(&self, env: &EnvMap) -> Daemon {
        let log = self.base.join("daemon.log");
        let stderr = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log)
            .unwrap();
        let child = std::process::Command::new(DAEMON_EXE)
            .arg("serve")
            .arg("--runtime-dir")
            .arg(&self.dirs.runtime)
            .arg("--state-dir")
            .arg(&self.dirs.state)
            .env_clear()
            .envs(env)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(stderr)
            .spawn()
            .unwrap();
        Daemon {
            child,
            socket: self.dirs.socket(),
            log,
        }
    }

    pub fn sessions_file(&self) -> PathBuf {
        self.dirs.state.join("session").join("sessions.json")
    }

    /// Where another protocol version's daemon keeps its socket and state.
    pub fn other_version(&self, version: &str) -> Dirs {
        let versions = |base: &str| self.base.join(base).join("bitplane").join("sessiond");
        Dirs {
            runtime: versions("run").join(version),
            state: versions("state").join(version),
        }
    }
}

/// The daemon this test spawned; only this child is ever signalled.
pub struct Daemon {
    child: Child,
    socket: PathBuf,
    log: PathBuf,
}

impl Daemon {
    /// Retries until the daemon accepts, past a socket a killed one left.
    pub async fn connect(&mut self) -> Client {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Ok(client) = Client::connect(&self.socket).await {
                return client;
            }
            let exited = self.child.try_wait().unwrap();
            if exited.is_some() || Instant::now() >= deadline {
                let log = std::fs::read_to_string(&self.log).unwrap_or_default();
                panic!("no daemon on {:?} ({exited:?}):\n{log}", self.socket);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    pub async fn stop(&mut self, client: &Client, stop: Stop) {
        match stop {
            Stop::ShutdownWithRestore => {
                let reply = client
                    .call("daemon.shutdown", Some(json!({ "restore": true })))
                    .await
                    .unwrap();
                assert_eq!(reply, json!({}));
            }
            Stop::Sigterm => self.signal(rustix::process::Signal::TERM),
            Stop::Sigkill => {
                // Past the 500 ms debounce: a crash loses at most that.
                tokio::time::sleep(Duration::from_secs(1)).await;
                self.signal(rustix::process::Signal::KILL);
            }
        }
        self.wait().await;
    }

    pub async fn shutdown(&mut self, client: &Client, restore: bool) {
        let reply = client
            .call("daemon.shutdown", Some(json!({ "restore": restore })))
            .await
            .unwrap();
        assert_eq!(reply, json!({}));
        self.wait().await;
    }

    pub fn signal(&self, signal: rustix::process::Signal) {
        let pid = rustix::process::Pid::from_child(&self.child);
        rustix::process::kill_process(pid, signal).unwrap();
    }

    pub async fn wait(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while self.child.try_wait().unwrap().is_none() {
            assert!(Instant::now() < deadline, "the daemon did not exit");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if let Ok(None) = self.child.try_wait() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

pub fn write_sessions(dirs: &Dirs, file: Value) -> PathBuf {
    let dir = dirs.state.join("session");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("sessions.json");
    std::fs::write(&path, serde_json::to_vec(&file).unwrap()).unwrap();
    path
}
