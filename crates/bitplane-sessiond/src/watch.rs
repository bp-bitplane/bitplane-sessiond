//! Scoped watches (ADR 0032): one scope per connection, replaced whole by
//! each `*.watch`, over OS watchers shared and ref-counted across
//! connections. A module owns one [`Hub`] and decides what its paths mean.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bitplane_protocol::{Frame, Notification};
use notify_debouncer_mini::notify::RecommendedWatcher;
use notify_debouncer_mini::{DebounceEventResult, Debouncer, new_debouncer};
use serde::Serialize;
use tokio::sync::mpsc;

use crate::module::Connection;

pub use notify_debouncer_mini::notify::RecursiveMode;

pub const DEBOUNCE: Duration = Duration::from_millis(250);

/// How often watched targets are checked for having vanished, for the
/// removals an OS watcher does not report.
pub const SWEEP: Duration = Duration::from_millis(1500);

/// What a client path resolved to: `key` identifies it across connections
/// and spellings, `watch` is what the OS watches for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub key: PathBuf,
    pub watch: Vec<(PathBuf, RecursiveMode)>,
}

/// One connection's interest in a target, under the path it used.
#[derive(Clone)]
pub struct Subscriber {
    pub connection: u64,
    pub path: String,
    frames: mpsc::UnboundedSender<Frame>,
}

pub struct Hub {
    state: Mutex<State>,
    debouncer: Mutex<Debouncer<RecommendedWatcher>>,
    batches: Mutex<Option<mpsc::UnboundedReceiver<Vec<PathBuf>>>>,
}

#[derive(Default)]
struct State {
    connections: HashMap<u64, Scope>,
    targets: HashMap<PathBuf, Watched>,
    /// Each OS-watched path and how many targets need it.
    os: HashMap<PathBuf, usize>,
}

struct Scope {
    frames: mpsc::UnboundedSender<Frame>,
    /// Client path → target key.
    paths: BTreeMap<String, PathBuf>,
}

struct Watched {
    watch: Vec<(PathBuf, RecursiveMode)>,
    subscribers: BTreeSet<(u64, String)>,
}

impl Hub {
    pub fn new() -> Result<Arc<Hub>, String> {
        let (batches, receiver) = mpsc::unbounded_channel();
        let debouncer = new_debouncer(DEBOUNCE, move |result: DebounceEventResult| {
            if let Ok(events) = result {
                let _ = batches.send(events.into_iter().map(|event| event.path).collect());
            }
        })
        .map_err(|error| error.to_string())?;
        let hub = Hub {
            state: Mutex::default(),
            debouncer: Mutex::new(debouncer),
            batches: Mutex::new(Some(receiver)),
        };
        Ok(Arc::new(hub))
    }

    /// The paths of each debounced batch of OS events; `Some` only the
    /// first time.
    pub fn batches(&self) -> Option<mpsc::UnboundedReceiver<Vec<PathBuf>>> {
        self.batches.lock().unwrap().take()
    }

    /// Makes `targets` the connection's whole scope and returns the
    /// subscribers new to it, which get the initial event.
    pub fn replace(
        &self,
        connection: &Connection,
        targets: Vec<(String, Target)>,
    ) -> Vec<Subscriber> {
        let mut state = self.state.lock().unwrap();
        let previous = state
            .connections
            .remove(&connection.id)
            .map(|scope| scope.paths)
            .unwrap_or_default();
        let wanted: BTreeMap<String, Target> = targets.into_iter().collect();

        let mut released = Vec::new();
        for (path, key) in &previous {
            if wanted.get(path).is_none_or(|target| target.key != *key) {
                released.extend(state.unsubscribe(connection.id, path, key));
            }
        }
        let mut added = Vec::new();
        let mut acquired = Vec::new();
        let mut paths = BTreeMap::new();
        for (path, target) in wanted {
            if previous.get(&path) != Some(&target.key) {
                acquired.extend(state.subscribe(connection.id, &path, &target));
                added.push(Subscriber {
                    connection: connection.id,
                    path: path.clone(),
                    frames: connection.frames.clone(),
                });
            }
            paths.insert(path, target.key);
        }
        if !paths.is_empty() {
            state.connections.insert(
                connection.id,
                Scope {
                    frames: connection.frames.clone(),
                    paths,
                },
            );
        }
        drop(state);
        self.apply(acquired, released);
        added
    }

    /// The connection closed: its scope goes.
    pub fn disconnect(&self, connection: u64) {
        let mut state = self.state.lock().unwrap();
        let Some(scope) = state.connections.remove(&connection) else {
            return;
        };
        let released: Vec<_> = scope
            .paths
            .iter()
            .flat_map(|(path, key)| state.unsubscribe(connection, path, key))
            .collect();
        drop(state);
        self.apply(Vec::new(), released);
    }

    /// Every watched target's key.
    pub fn keys(&self) -> Vec<PathBuf> {
        self.state.lock().unwrap().targets.keys().cloned().collect()
    }

    pub fn subscribers(&self, key: &Path) -> Vec<Subscriber> {
        let state = self.state.lock().unwrap();
        state.subscribers_of(key)
    }

    /// Takes `key` out of every scope, as when it is gone, and returns who
    /// was watching it.
    pub fn remove(&self, key: &Path) -> Vec<Subscriber> {
        let mut state = self.state.lock().unwrap();
        let subscribers = state.subscribers_of(key);
        let mut released = Vec::new();
        for subscriber in &subscribers {
            if let Some(scope) = state.connections.get_mut(&subscriber.connection) {
                scope.paths.remove(&subscriber.path);
                if scope.paths.is_empty() {
                    state.connections.remove(&subscriber.connection);
                }
            }
            released.extend(state.unsubscribe(subscriber.connection, &subscriber.path, key));
        }
        drop(state);
        self.apply(Vec::new(), released);
        subscribers
    }

    fn apply(&self, acquired: Vec<(PathBuf, RecursiveMode)>, released: Vec<PathBuf>) {
        let mut debouncer = self.debouncer.lock().unwrap();
        let watcher = debouncer.watcher();
        for path in released {
            let _ = watcher.unwatch(&path);
        }
        for (path, depth) in acquired {
            if let Err(error) = watcher.watch(&path, depth) {
                tracing::debug!(path = %path.display(), %error, "couldn't watch");
            }
        }
    }
}

impl Subscriber {
    /// Sends `method` to this subscriber's connection only.
    pub fn send(&self, method: &str, params: impl Serialize) {
        send(&self.frames, method, params);
    }
}

/// Sends an event to one connection.
pub fn send(frames: &mpsc::UnboundedSender<Frame>, method: &str, params: impl Serialize) {
    let params = serde_json::to_value(params).expect("protocol events serialize");
    let bytes = serde_json::to_vec(&Notification::new(method, Some(params)))
        .expect("protocol events serialize");
    let _ = frames.send(Frame::Json(bytes));
}

impl State {
    /// The OS paths that became watched.
    fn subscribe(
        &mut self,
        connection: u64,
        path: &str,
        target: &Target,
    ) -> Vec<(PathBuf, RecursiveMode)> {
        let watched = self
            .targets
            .entry(target.key.clone())
            .or_insert_with(|| Watched {
                watch: target.watch.clone(),
                subscribers: BTreeSet::new(),
            });
        let first = watched.subscribers.is_empty();
        watched.subscribers.insert((connection, path.to_string()));
        if !first {
            return Vec::new();
        }
        let watch = watched.watch.clone();
        watch
            .into_iter()
            .filter(|(os_path, _)| {
                let count = self.os.entry(os_path.clone()).or_default();
                *count += 1;
                *count == 1
            })
            .collect()
    }

    /// The OS paths no longer needed.
    fn unsubscribe(&mut self, connection: u64, path: &str, key: &Path) -> Vec<PathBuf> {
        let Some(watched) = self.targets.get_mut(key) else {
            return Vec::new();
        };
        watched.subscribers.remove(&(connection, path.to_string()));
        if !watched.subscribers.is_empty() {
            return Vec::new();
        }
        let watched = self.targets.remove(key).expect("looked up above");
        watched
            .watch
            .into_iter()
            .filter_map(|(os_path, _)| {
                let count = self.os.get_mut(&os_path)?;
                *count -= 1;
                (*count == 0).then(|| {
                    self.os.remove(&os_path);
                    os_path
                })
            })
            .collect()
    }

    fn subscribers_of(&self, key: &Path) -> Vec<Subscriber> {
        let Some(watched) = self.targets.get(key) else {
            return Vec::new();
        };
        watched
            .subscribers
            .iter()
            .filter_map(|(connection, path)| {
                let scope = self.connections.get(connection)?;
                Some(Subscriber {
                    connection: *connection,
                    path: path.clone(),
                    frames: scope.frames.clone(),
                })
            })
            .collect()
    }
}
