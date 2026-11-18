//! The `git` module: status per repo path, blobs at HEAD or the index, and
//! watching repos, all read through the git CLI (ADR 0032). A repo path is
//! a worktree: a folder holding `.git`. Watches cover the worktree and its
//! git dirs recursively.

mod repo;
mod status;

use std::collections::{BTreeSet, HashMap};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, OnceLock};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use bitplane_protocol::fs::{Blob, READ_LIMIT, WatchParams};
use bitplane_protocol::git::{
    GitRepo, GitRev, GitShowParams, GitStatus, GitStatusParams, GitStatusResult,
};
use bitplane_protocol::{ErrorCode, RpcError, method};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};

use crate::env::EnvMap;
use crate::git_cli::Git as Cli;
use crate::module::{Availability, BoxFuture, Call, Connection, Module, ModuleContext, Reply};
use crate::watch::{self, Hub, RecursiveMode, SWEEP, Subscriber, Target};

pub use repo::Repo;
pub use status::parse as parse_status;

const NAMESPACE: &str = "git";

pub const NO_GIT: &str = "git not found on PATH; install it and restart the daemon";

#[derive(Default)]
pub struct Git {
    cli: OnceLock<Arc<Cli>>,
    hub: OnceLock<Arc<Hub>>,
    jobs: OnceLock<mpsc::UnboundedSender<Watch>>,
}

/// One `git.watch`, applied in order with the OS events so each connection
/// sees its reply, then initial state, then changes.
struct Watch {
    connection: Connection,
    reply: Reply,
    paths: Vec<String>,
    done: oneshot::Sender<()>,
}

impl Git {
    pub fn module() -> Arc<dyn Module> {
        Arc::new(Self::default())
    }

    fn cli(&self) -> &Arc<Cli> {
        self.cli.get().expect("available only with git")
    }

    async fn status(&self, params: GitStatusParams) -> Result<Value, RpcError> {
        for path in &params.paths {
            absolute(path)?;
        }
        let repos = read_all(
            self.cli(),
            params.paths.into_iter().map(|path| {
                let repo = Repo::at(Path::new(&path));
                (path, repo)
            }),
        )
        .await;
        Ok(json!(GitStatusResult { repos }))
    }

    async fn show(&self, params: GitShowParams) -> Result<Value, RpcError> {
        let repo = absolute(&params.repo)?;
        let relative = Path::new(&params.path);
        if params.path.is_empty()
            || relative
                .components()
                .any(|c| !matches!(c, Component::Normal(_)))
        {
            return Err(RpcError::new(
                RpcError::INVALID_PARAMS,
                format!("{} is not a path inside the repo", params.path),
            ));
        }
        let spec = match params.rev {
            GitRev::Head => format!("HEAD:{}", params.path),
            GitRev::Index => format!(":{}", params.path),
            GitRev::Unknown => {
                return Err(RpcError::new(RpcError::INVALID_PARAMS, "unknown rev"));
            }
        };
        let not_found = || {
            RpcError::protocol(
                ErrorCode::NotFound,
                format!("{} is not in {} at {spec}", params.path, params.repo),
                Some(json!({ "repo": params.repo, "path": params.path })),
            )
        };
        if Repo::at(&repo).is_none() {
            return Err(not_found());
        }
        let limit = params.max_bytes.unwrap_or(READ_LIMIT).min(READ_LIMIT);
        let cli = self.cli();
        let size: u64 = cli
            .run(&repo, &["cat-file", "-s", &spec], &[])
            .await
            .ok()
            .and_then(|size| String::from_utf8_lossy(&size).trim().parse().ok())
            .ok_or_else(not_found)?;
        if size > limit {
            return Ok(json!(Blob { size, data: None }));
        }
        let bytes = cli
            .run(&repo, &["cat-file", "blob", &spec], &[])
            .await
            .map_err(|_| not_found())?;
        Ok(json!(Blob {
            size: bytes.len() as u64,
            data: Some(STANDARD.encode(bytes)),
        }))
    }

    async fn watch(&self, call: &Call, params: WatchParams) -> Result<(), RpcError> {
        for path in &params.paths {
            absolute(path)?;
        }
        let (done, finished) = oneshot::channel();
        let job = Watch {
            connection: call.connection.clone(),
            reply: call.reply.clone(),
            paths: params.paths,
            done,
        };
        let jobs = self.jobs.get().expect("started");
        if jobs.send(job).is_ok() {
            let _ = finished.await;
        }
        Ok(())
    }
}

impl Module for Git {
    fn namespace(&self) -> &'static str {
        NAMESPACE
    }

    fn probe(&self, env: &EnvMap) -> Availability {
        let Some(cli) = Cli::find(env) else {
            return Availability::Unavailable {
                reason: NO_GIT.into(),
            };
        };
        let _ = self.cli.set(Arc::new(cli));
        match Hub::new() {
            Ok(hub) => {
                let _ = self.hub.set(hub);
                Availability::Available { info: None }
            }
            Err(error) => Availability::Unavailable {
                reason: format!("couldn't start a file watcher: {error}"),
            },
        }
    }

    fn start(&self, _context: ModuleContext) {
        let hub = self.hub.get().expect("probed").clone();
        let batches = hub.batches().expect("started once");
        let (jobs, receiver) = mpsc::unbounded_channel();
        let _ = self.jobs.set(jobs);
        let follower = Follower {
            cli: self.cli().clone(),
            hub,
            repos: HashMap::new(),
        };
        tokio::spawn(follower.run(receiver, batches));
    }

    fn handle(&self, call: Call) -> BoxFuture<'_, Result<Value, RpcError>> {
        Box::pin(async move {
            match call.method.as_str() {
                method::GIT_STATUS => self.status(params(&call)?).await,
                method::GIT_SHOW => self.show(params(&call)?).await,
                method::GIT_WATCH => {
                    self.watch(&call, params(&call)?).await?;
                    Ok(json!({}))
                }
                _ => Err(RpcError::new(
                    RpcError::METHOD_NOT_FOUND,
                    format!("no method {}", call.method),
                )),
            }
        })
    }

    fn disconnected(&self, connection: u64) {
        if let Some(hub) = self.hub.get() {
            hub.disconnect(connection);
        }
    }
}

/// Applies watches and OS events one at a time, so a connection's events
/// keep the order they were produced in.
struct Follower {
    cli: Arc<Cli>,
    hub: Arc<Hub>,
    /// Each watched target's repo, by key.
    repos: HashMap<PathBuf, Repo>,
}

impl Follower {
    async fn run(
        mut self,
        mut watches: mpsc::UnboundedReceiver<Watch>,
        mut batches: mpsc::UnboundedReceiver<Vec<PathBuf>>,
    ) {
        let mut sweep = tokio::time::interval(SWEEP);
        loop {
            tokio::select! {
                watch = watches.recv() => {
                    let Some(watch) = watch else { return };
                    self.watch(watch).await;
                }
                batch = batches.recv() => {
                    let Some(paths) = batch else { return };
                    self.changed(&paths).await;
                }
                _ = sweep.tick() => self.sweep(),
            }
        }
    }

    async fn watch(&mut self, watch: Watch) {
        let paths: BTreeSet<String> = watch.paths.into_iter().collect();
        let mut targets = Vec::new();
        let mut keys = HashMap::new();
        let mut gone = Vec::new();
        for path in paths {
            match Repo::at(Path::new(&path)) {
                Some(repo) => {
                    keys.insert(path.clone(), repo.worktree.clone());
                    let target = Target {
                        key: repo.worktree.clone(),
                        watch: repo
                            .watched()
                            .into_iter()
                            .map(|path| (path, RecursiveMode::Recursive))
                            .collect(),
                    };
                    self.repos.insert(repo.worktree.clone(), repo);
                    targets.push((path, target));
                }
                None => gone.push(path),
            }
        }
        let added = self.hub.replace(&watch.connection, targets);
        self.forget_unwatched();
        watch.reply.send(Ok(json!({})));
        let _ = watch.done.send(());
        for path in gone {
            watch::send(
                &watch.connection.frames,
                method::GIT_CHANGED,
                gone_repo(path),
            );
        }
        let mut keyed: Vec<(PathBuf, Vec<Subscriber>)> = Vec::new();
        for subscriber in added {
            let key = keys[&subscriber.path].clone();
            match keyed.iter_mut().find(|(existing, _)| *existing == key) {
                Some((_, group)) => group.push(subscriber),
                None => keyed.push((key, vec![subscriber])),
            }
        }
        self.send_status(keyed).await;
    }

    async fn changed(&mut self, paths: &[PathBuf]) {
        let touched: Vec<PathBuf> = self
            .repos
            .iter()
            .filter(|(_, repo)| paths.iter().any(|path| repo.is_relevant(path)))
            .map(|(key, _)| key.clone())
            .collect();
        let mut keyed = Vec::new();
        for key in touched {
            if self.repos[&key].is_gone() {
                self.end(&key);
            } else {
                keyed.push((key.clone(), self.hub.subscribers(&key)));
            }
        }
        self.send_status(keyed).await;
    }

    fn sweep(&mut self) {
        let gone: Vec<PathBuf> = self
            .repos
            .iter()
            .filter(|(_, repo)| repo.is_gone())
            .map(|(key, _)| key.clone())
            .collect();
        for key in gone {
            self.end(&key);
        }
    }

    /// One status read per repo, sent to each subscriber under its own path.
    async fn send_status(&self, keyed: Vec<(PathBuf, Vec<Subscriber>)>) {
        let reads: Vec<_> = keyed
            .into_iter()
            .filter(|(_, subscribers)| !subscribers.is_empty())
            .map(|(key, subscribers)| {
                let cli = self.cli.clone();
                let read = tokio::spawn(async move { status::read(&cli, &key).await });
                (read, subscribers)
            })
            .collect();
        for (read, subscribers) in reads {
            let outcome = read
                .await
                .unwrap_or_else(|error| Err(format!("status read failed: {error}")));
            for subscriber in subscribers {
                let repo = repo_state(subscriber.path.clone(), outcome.clone());
                subscriber.send(method::GIT_CHANGED, repo);
            }
        }
    }

    fn end(&mut self, key: &Path) {
        for subscriber in self.hub.remove(key) {
            subscriber.send(method::GIT_CHANGED, gone_repo(subscriber.path.clone()));
        }
        self.repos.remove(key);
    }

    fn forget_unwatched(&mut self) {
        let watched: BTreeSet<PathBuf> = self.hub.keys().into_iter().collect();
        self.repos.retain(|key, _| watched.contains(key));
    }
}

/// Every path's state, read in parallel and returned in order.
async fn read_all(
    cli: &Arc<Cli>,
    paths: impl Iterator<Item = (String, Option<Repo>)>,
) -> Vec<GitRepo> {
    let reads: Vec<_> = paths
        .map(|(path, repo)| {
            let cli = cli.clone();
            let read = repo
                .map(|repo| tokio::spawn(async move { status::read(&cli, &repo.worktree).await }));
            (path, read)
        })
        .collect();
    let mut repos = Vec::with_capacity(reads.len());
    for (path, read) in reads {
        repos.push(match read {
            None => gone_repo(path),
            Some(read) => repo_state(
                path,
                read.await
                    .unwrap_or_else(|error| Err(format!("status read failed: {error}"))),
            ),
        });
    }
    repos
}

fn repo_state(path: String, outcome: Result<GitStatus, String>) -> GitRepo {
    match outcome {
        Ok(status) => GitRepo {
            path,
            status: Some(status),
            gone: false,
            error: None,
        },
        Err(error) => GitRepo {
            path,
            status: None,
            gone: false,
            error: Some(error),
        },
    }
}

fn gone_repo(path: String) -> GitRepo {
    GitRepo {
        path,
        status: None,
        gone: true,
        error: None,
    }
}

fn params<T: DeserializeOwned>(call: &Call) -> Result<T, RpcError> {
    serde_json::from_value(call.params.clone().unwrap_or(Value::Null))
        .map_err(|error| RpcError::new(RpcError::INVALID_PARAMS, error.to_string()))
}

fn absolute(path: &str) -> Result<PathBuf, RpcError> {
    let path = PathBuf::from(path);
    if path.is_absolute() {
        Ok(path)
    } else {
        Err(RpcError::new(
            RpcError::INVALID_PARAMS,
            format!("{} is not an absolute path", path.display()),
        ))
    }
}
