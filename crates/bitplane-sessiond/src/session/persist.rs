//! `sessions.json`: what a restored session comes back with (ADR 0027).

use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Condvar, Mutex};
use std::time::Duration;

use bitplane_protocol::session::{SessionId, Size};
use serde::{Deserialize, Serialize};

use crate::env::EnvMap;
use crate::module::AgentLink;
use crate::paths::Dirs;
use crate::pidfile;

/// Bumped on a change an older daemon would misread; it then ignores the file.
pub const SCHEMA_VERSION: u32 = 1;
pub const FILE_NAME: &str = "sessions.json";
/// A crash loses at most this much of the latest changes.
pub const DEBOUNCE: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Record {
    pub id: SessionId,
    pub seq: u32,
    pub root: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plane: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The client's `session.open` env; the login env is captured afresh.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<EnvMap>,
    pub size: Size,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<AgentLink>,
}

/// What a daemon starts with: its own sessions, then other versions' after
/// them. `file` is `None` when its own file is a newer schema, which it
/// must neither read nor overwrite.
pub struct Restored {
    pub records: Vec<Record>,
    pub file: Option<PathBuf>,
}

/// Saves at most once per [`DEBOUNCE`], and never after [`Persist::close`].
pub struct Persist {
    file: Option<PathBuf>,
    pending: Mutex<Pending>,
    wake: Condvar,
    writing: Mutex<()>,
}

#[derive(Default)]
struct Pending {
    dirty: bool,
    closed: bool,
}

/// Reads this instance's file and adopts every other version's whose
/// daemon is not running; adopted files are deleted once their sessions
/// are saved in this one.
pub fn restore(dirs: &Dirs, state_dir: &Path) -> Restored {
    let file = state_dir.join(FILE_NAME);
    let mut records = match read(&file) {
        Read::Records(records) => records,
        Read::Missing => Vec::new(),
        Read::Newer(version) => {
            tracing::warn!(file = %file.display(), version, "a newer daemon's sessions; not restoring or saving any");
            return Restored {
                records: Vec::new(),
                file: None,
            };
        }
        Read::Invalid(error) => {
            tracing::warn!(file = %file.display(), %error, "unreadable sessions file; starting without it");
            Vec::new()
        }
    };
    records.sort_by_key(|record| record.seq);

    let mut adopted = Vec::new();
    for other in dirs.other_versions() {
        let Ok(Some(lock)) = pidfile::try_lock(&other.pidfile()) else {
            continue;
        };
        let path = other.state.join(super::NAMESPACE).join(FILE_NAME);
        let Read::Records(mut theirs) = read(&path) else {
            continue;
        };
        theirs.sort_by_key(|record| record.seq);
        let mut next = records.iter().map(|record| record.seq).max().unwrap_or(0);
        for mut record in theirs {
            if records.iter().any(|ours| ours.id == record.id) {
                continue;
            }
            next += 1;
            record.seq = next;
            records.push(record);
        }
        tracing::info!(file = %path.display(), "adopting another version's sessions");
        adopted.push((path, lock));
    }
    if !adopted.is_empty() {
        if let Err(error) = write(&file, &records) {
            tracing::warn!(%error, "couldn't save adopted sessions; leaving their files");
            return Restored {
                records,
                file: Some(file),
            };
        }
        for (path, _lock) in adopted {
            let _ = std::fs::remove_file(path);
        }
    }
    Restored {
        records,
        file: Some(file),
    }
}

impl Persist {
    pub fn new(file: Option<PathBuf>) -> Self {
        Self {
            file,
            pending: Mutex::new(Pending::default()),
            wake: Condvar::new(),
            writing: Mutex::new(()),
        }
    }

    /// A persisted field changed.
    pub fn touch(&self) {
        let mut pending = self.pending.lock().unwrap();
        if !pending.dirty {
            pending.dirty = true;
            self.wake.notify_one();
        }
    }

    /// Blocks until a change is due to be saved; `false` once closed.
    pub fn due(&self) -> bool {
        let mut pending = self.pending.lock().unwrap();
        while !pending.dirty && !pending.closed {
            pending = self.wake.wait(pending).unwrap();
        }
        if pending.closed {
            return false;
        }
        drop(pending);
        std::thread::sleep(DEBOUNCE);
        let mut pending = self.pending.lock().unwrap();
        pending.dirty = false;
        !pending.closed
    }

    pub fn save(&self, records: &[Record]) {
        let _writing = self.writing.lock().unwrap();
        if self.pending.lock().unwrap().closed {
            return;
        }
        self.write(records);
    }

    /// The last save, or with `None` the file's removal; nothing is saved after it.
    pub fn close(&self, records: Option<&[Record]>) {
        let _writing = self.writing.lock().unwrap();
        {
            let mut pending = self.pending.lock().unwrap();
            if pending.closed {
                return;
            }
            pending.closed = true;
            self.wake.notify_one();
        }
        match records {
            Some(records) => self.write(records),
            None => {
                if let Some(file) = &self.file {
                    let _ = std::fs::remove_file(file);
                }
            }
        }
    }

    fn write(&self, records: &[Record]) {
        if let Some(file) = &self.file
            && let Err(error) = write(file, records)
        {
            tracing::warn!(file = %file.display(), %error, "couldn't save sessions");
        }
    }
}

enum Read {
    Records(Vec<Record>),
    Missing,
    Newer(u32),
    Invalid(String),
}

#[derive(Serialize)]
struct FileOut<'a> {
    version: u32,
    sessions: &'a [Record],
}

#[derive(Deserialize)]
struct Header {
    version: u32,
}

#[derive(Deserialize)]
struct FileIn {
    #[serde(default)]
    sessions: Vec<Record>,
}

fn read(path: &Path) -> Read {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Read::Missing,
        Err(error) => return Read::Invalid(error.to_string()),
    };
    match serde_json::from_slice::<Header>(&bytes) {
        Ok(Header { version }) if version > SCHEMA_VERSION => return Read::Newer(version),
        Ok(_) => {}
        Err(error) => return Read::Invalid(error.to_string()),
    }
    match serde_json::from_slice::<FileIn>(&bytes) {
        Ok(file) => Read::Records(file.sessions),
        Err(error) => Read::Invalid(error.to_string()),
    }
}

/// tmp + fsync + rename, so a crash leaves the old file or the new one.
fn write(path: &Path, records: &[Record]) -> std::io::Result<()> {
    let bytes = serde_json::to_vec_pretty(&FileOut {
        version: SCHEMA_VERSION,
        sessions: records,
    })
    .expect("records serialize");
    let tmp = path.with_extension("json.tmp");
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    std::fs::rename(&tmp, path)
}
