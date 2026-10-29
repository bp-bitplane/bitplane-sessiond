//! A real daemon on a temp socket and state dir, served in-process so tests
//! can plug in their own modules.
#![allow(dead_code)]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use bitplane_protocol::RpcError;
use bitplane_sessiond::client::Client;
use bitplane_sessiond::env::EnvMap;
use bitplane_sessiond::module::{Availability, BoxFuture, Call, Module};
use bitplane_sessiond::paths::Dirs;
use bitplane_sessiond::server::{Config, ServeError, serve};
use serde_json::Value;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

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
