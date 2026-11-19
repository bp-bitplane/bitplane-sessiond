//! The root module's job table (ADR 0034): every job of every kind, their
//! capped logs and per-connection log scopes, and `jobs.json`, which brings
//! the table back after a restart. Modules contribute kinds through
//! [`Jobs::start`] and report on a job through its [`JobHandle`].

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use bitplane_protocol::job::{
    DAEMON_RESTARTED, Job, JobChanged, JobClient, JobId, JobKind, JobListResult, JobLog, JobPhase,
    JobRef, JobStatus, JobWatchParams,
};
use bitplane_protocol::{ErrorCode, Frame, RpcError, method};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::module::{Call, Events};
use crate::paths::Dirs;
use crate::pidfile;
use crate::watch;

pub const LOG_MAX_LINES: usize = 10_000;
pub const LOG_MAX_BYTES: usize = 1024 * 1024;
/// Finished jobs past this many are dismissed, oldest first.
pub const FINISHED_KEPT: usize = 50;
/// A restored job's whole backlog.
pub const LOG_LOST: &str = "The daemon restarted; this job's log was lost with it.";

pub const NAMESPACE: &str = "job";
pub const FILE_NAME: &str = "jobs.json";
/// Bumped on a change an older daemon would misread; it then ignores the file.
pub const SCHEMA_VERSION: u32 = 1;

/// Stops a running job's work; its kind still reports how it ended.
pub type Cancel = Box<dyn Fn() + Send + Sync>;

pub struct Jobs {
    events: Events,
    file: Option<PathBuf>,
    table: Mutex<Table>,
    /// `true` once closed: nothing is saved after the last save.
    writing: Mutex<bool>,
}

/// What a kind reports on one job through.
pub struct JobHandle {
    jobs: Arc<Jobs>,
    id: JobId,
}

#[derive(Default)]
struct Table {
    jobs: Vec<Entry>,
    scopes: HashMap<u64, Scope>,
}

struct Scope {
    frames: mpsc::UnboundedSender<Frame>,
    jobs: BTreeSet<JobId>,
}

struct Entry {
    job: Job,
    log: Log,
    cancel: Option<Cancel>,
}

#[derive(Default)]
struct Log {
    lines: VecDeque<String>,
    bytes: usize,
    dropped: u64,
}

impl Jobs {
    /// Reads this instance's table and adopts every other version's whose
    /// daemon is not running (ADR 0027); a job that was running comes back
    /// failed, and every restored job's log is one note line.
    pub fn load(dirs: &Dirs, events: Events) -> Arc<Jobs> {
        let state_dir = dirs.state.join(NAMESPACE);
        if let Err(error) = std::fs::create_dir_all(&state_dir) {
            tracing::warn!(dir = %state_dir.display(), %error, "no job state dir; jobs won't survive a restart");
        }
        let file = state_dir.join(FILE_NAME);
        let (restored, file) = match read(&file) {
            Read::Jobs(jobs) => (jobs, Some(file)),
            Read::Missing => (Vec::new(), Some(file)),
            Read::Newer(version) => {
                tracing::warn!(file = %file.display(), version, "a newer daemon's jobs; not restoring or saving any");
                (Vec::new(), None)
            }
            Read::Invalid(error) => {
                tracing::warn!(file = %file.display(), %error, "unreadable jobs file; starting without it");
                (Vec::new(), Some(file))
            }
        };
        let mut table = Table::default();
        for job in restored {
            table.restore(job);
        }

        let mut adopted = Vec::new();
        if file.is_some() {
            for other in dirs.other_versions() {
                let Ok(Some(lock)) = pidfile::try_lock(&other.pidfile()) else {
                    continue;
                };
                let path = other.state.join(NAMESPACE).join(FILE_NAME);
                let Read::Jobs(theirs) = read(&path) else {
                    continue;
                };
                for job in theirs {
                    if !table.jobs.iter().any(|entry| entry.job.id == job.id) {
                        table.restore(job);
                    }
                }
                tracing::info!(file = %path.display(), "adopting another version's jobs");
                adopted.push((path, lock));
            }
        }
        table.evict_finished();

        let jobs = Arc::new(Jobs {
            events,
            file,
            table: Mutex::new(table),
            writing: Mutex::new(false),
        });
        if jobs.save() && !adopted.is_empty() {
            for (path, _lock) in adopted {
                let _ = std::fs::remove_file(path);
            }
        }
        jobs
    }

    /// A running job of `kind`; `job.changed` is broadcast before this returns.
    pub fn start(
        self: &Arc<Self>,
        kind: JobKind,
        started_by: JobClient,
        detail: Value,
        cancel: Cancel,
    ) -> JobHandle {
        let job = Job {
            id: uuid::Uuid::new_v4().to_string(),
            kind,
            status: JobStatus::Running,
            phase: None,
            started_by,
            started_at: now_millis(),
            finished_at: None,
            detail,
        };
        let id = job.id.clone();
        {
            let mut table = self.table.lock().unwrap();
            self.changed(&job);
            table.jobs.push(Entry {
                job,
                log: Log::default(),
                cancel: Some(cancel),
            });
        }
        self.save();
        JobHandle {
            jobs: self.clone(),
            id,
        }
    }

    /// `job.*`. `job.watch` replies before it sends any backlog.
    pub fn handle(&self, call: &Call) -> Result<Value, RpcError> {
        match call.method.as_str() {
            method::JOB_LIST => Ok(json!(self.list())),
            method::JOB_WATCH => {
                self.watch(call, params(call)?);
                Ok(json!({}))
            }
            method::JOB_CANCEL => self.cancel(&params::<JobRef>(call)?.job),
            method::JOB_DISMISS => self.dismiss(&params::<JobRef>(call)?.job),
            _ => Err(RpcError::new(
                RpcError::METHOD_NOT_FOUND,
                format!("no method {}", call.method),
            )),
        }
    }

    /// The connection closed: its log scope goes.
    pub fn disconnected(&self, connection: u64) {
        self.table.lock().unwrap().scopes.remove(&connection);
    }

    /// The last save; the table is not saved after it.
    pub fn close(&self) {
        let mut closed = self.writing.lock().unwrap();
        if !*closed {
            self.write();
            *closed = true;
        }
    }

    fn list(&self) -> JobListResult {
        let table = self.table.lock().unwrap();
        JobListResult {
            jobs: table.jobs.iter().map(|entry| entry.job.clone()).collect(),
        }
    }

    fn watch(&self, call: &Call, params: JobWatchParams) {
        let connection = &call.connection;
        let mut table = self.table.lock().unwrap();
        let previous = table
            .scopes
            .remove(&connection.id)
            .map(|scope| scope.jobs)
            .unwrap_or_default();
        call.reply.send(Ok(json!({})));
        let mut scope = BTreeSet::new();
        for id in params.jobs.into_iter().collect::<BTreeSet<_>>() {
            let Some(entry) = table.find(&id) else {
                watch::send(&connection.frames, method::JOB_REMOVED, JobRef { job: id });
                continue;
            };
            if !previous.contains(&id) && (!entry.log.lines.is_empty() || entry.log.dropped > 0) {
                let backlog = JobLog {
                    job: id.clone(),
                    lines: entry.log.lines.iter().cloned().collect(),
                    dropped: (entry.log.dropped > 0).then_some(entry.log.dropped),
                };
                watch::send(&connection.frames, method::JOB_LOG, backlog);
            }
            scope.insert(id);
        }
        if !scope.is_empty() {
            table.scopes.insert(
                connection.id,
                Scope {
                    frames: connection.frames.clone(),
                    jobs: scope,
                },
            );
        }
    }

    fn cancel(&self, id: &str) -> Result<Value, RpcError> {
        let table = self.table.lock().unwrap();
        let entry = table.find(id).ok_or_else(|| unknown_job(id))?;
        match (&entry.cancel, entry.job.status.is_running()) {
            (Some(cancel), true) => {
                cancel();
                Ok(json!({}))
            }
            _ => Err(RpcError::protocol(
                ErrorCode::JobFinished,
                format!("job {id} already finished"),
                Some(json!({ "job": id, "status": entry.job.status })),
            )),
        }
    }

    fn dismiss(&self, id: &str) -> Result<Value, RpcError> {
        {
            let mut table = self.table.lock().unwrap();
            let entry = table.find(id).ok_or_else(|| unknown_job(id))?;
            if entry.job.status.is_running() {
                return Err(RpcError::new(
                    RpcError::INVALID_REQUEST,
                    format!("job {id} is still running; cancel it first"),
                ));
            }
            table.remove(id);
            self.events
                .emit(method::JOB_REMOVED, json!(JobRef { job: id.into() }));
        }
        self.save();
        Ok(json!({}))
    }

    fn changed(&self, job: &Job) {
        self.events
            .emit(method::JOB_CHANGED, json!(JobChanged { job: job.clone() }));
    }

    /// Whether the table is on disk as it is now.
    fn save(&self) -> bool {
        let closed = self.writing.lock().unwrap();
        !*closed && self.write()
    }

    /// Called with `writing` held, so saves land in the order they were taken.
    fn write(&self) -> bool {
        let Some(file) = &self.file else {
            return false;
        };
        let jobs: Vec<Job> = {
            let table = self.table.lock().unwrap();
            table.jobs.iter().map(|entry| entry.job.clone()).collect()
        };
        match write(file, &jobs) {
            Ok(()) => true,
            Err(error) => {
                tracing::warn!(file = %file.display(), %error, "couldn't save jobs");
                false
            }
        }
    }
}

impl JobHandle {
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Phases only move forward; a step behind the current one is ignored.
    pub fn phase(&self, phase: JobPhase) {
        {
            let mut table = self.jobs.table.lock().unwrap();
            let Some(entry) = table.find_mut(&self.id) else {
                return;
            };
            if !entry.job.status.is_running()
                || entry
                    .job
                    .phase
                    .is_some_and(|current| rank(current) >= rank(phase))
            {
                return;
            }
            entry.job.phase = Some(phase);
            let job = entry.job.clone();
            self.jobs.changed(&job);
        }
        self.jobs.save();
    }

    /// Appends to the job's log and sends the lines to every connection
    /// watching it.
    pub fn log(&self, lines: Vec<String>) {
        if lines.is_empty() {
            return;
        }
        let mut table = self.jobs.table.lock().unwrap();
        let Some(entry) = table.find_mut(&self.id) else {
            return;
        };
        for line in &lines {
            entry.log.push(line.clone());
        }
        let event = JobLog {
            job: self.id.clone(),
            lines,
            dropped: None,
        };
        for scope in table.scopes.values() {
            if scope.jobs.contains(&self.id) {
                watch::send(&scope.frames, method::JOB_LOG, &event);
            }
        }
    }

    /// The job's final status and detail; finished jobs past
    /// [`FINISHED_KEPT`] are dismissed.
    pub fn finish(&self, status: JobStatus, detail: Value) {
        {
            let mut table = self.jobs.table.lock().unwrap();
            let Some(entry) = table.find_mut(&self.id) else {
                return;
            };
            entry.job.status = status;
            entry.job.finished_at = Some(now_millis());
            entry.job.detail = detail;
            entry.cancel = None;
            let job = entry.job.clone();
            self.jobs.changed(&job);
            for id in table.evict_finished() {
                self.jobs
                    .events
                    .emit(method::JOB_REMOVED, json!(JobRef { job: id }));
            }
        }
        self.jobs.save();
    }
}

impl Table {
    fn restore(&mut self, mut job: Job) {
        if job.status.is_running() {
            job.status = JobStatus::Failed {
                reason: DAEMON_RESTARTED.into(),
            };
            job.finished_at = Some(now_millis());
        }
        let mut log = Log::default();
        log.push(LOG_LOST.into());
        self.jobs.push(Entry {
            job,
            log,
            cancel: None,
        });
    }

    fn find(&self, id: &str) -> Option<&Entry> {
        self.jobs.iter().find(|entry| entry.job.id == id)
    }

    fn find_mut(&mut self, id: &str) -> Option<&mut Entry> {
        self.jobs.iter_mut().find(|entry| entry.job.id == id)
    }

    /// Takes the job out of the table and out of every scope, silently.
    fn remove(&mut self, id: &str) {
        self.jobs.retain(|entry| entry.job.id != id);
        self.scopes.retain(|_, scope| {
            scope.jobs.remove(id);
            !scope.jobs.is_empty()
        });
    }

    /// Removes the oldest finished jobs past [`FINISHED_KEPT`] and returns them.
    fn evict_finished(&mut self) -> Vec<JobId> {
        let mut finished: Vec<(u64, usize)> = self
            .jobs
            .iter()
            .enumerate()
            .filter(|(_, entry)| !entry.job.status.is_running())
            .map(|(index, entry)| (entry.job.finished_at.unwrap_or_default(), index))
            .collect();
        let excess = finished.len().saturating_sub(FINISHED_KEPT);
        finished.sort();
        let evicted: Vec<JobId> = finished[..excess]
            .iter()
            .map(|&(_, index)| self.jobs[index].job.id.clone())
            .collect();
        for id in &evicted {
            self.remove(id);
        }
        evicted
    }
}

impl Log {
    fn push(&mut self, line: String) {
        self.bytes += line.len();
        self.lines.push_back(line);
        while self.lines.len() > LOG_MAX_LINES || self.bytes > LOG_MAX_BYTES {
            let Some(evicted) = self.lines.pop_front() else {
                break;
            };
            self.bytes -= evicted.len();
            self.dropped += 1;
        }
    }
}

pub fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}

fn rank(phase: JobPhase) -> u8 {
    match phase {
        JobPhase::Fetching => 0,
        JobPhase::AddingWorktrees => 1,
        JobPhase::RunningScripts => 2,
        JobPhase::Unknown => 3,
    }
}

fn unknown_job(id: &str) -> RpcError {
    RpcError::protocol(
        ErrorCode::UnknownJob,
        format!("no job {id}"),
        Some(json!({ "job": id })),
    )
}

fn params<T: serde::de::DeserializeOwned>(call: &Call) -> Result<T, RpcError> {
    serde_json::from_value(call.params.clone().unwrap_or(Value::Null))
        .map_err(|error| RpcError::new(RpcError::INVALID_PARAMS, error.to_string()))
}

enum Read {
    Jobs(Vec<Job>),
    Missing,
    Newer(u32),
    Invalid(String),
}

#[derive(Serialize)]
struct FileOut<'a> {
    version: u32,
    jobs: &'a [Job],
}

#[derive(Deserialize)]
struct Header {
    version: u32,
}

#[derive(Deserialize)]
struct FileIn {
    #[serde(default)]
    jobs: Vec<Job>,
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
        Ok(file) => Read::Jobs(file.jobs),
        Err(error) => Read::Invalid(error.to_string()),
    }
}

/// tmp + fsync + rename, so a crash leaves the old file or the new one.
fn write(path: &Path, jobs: &[Job]) -> std::io::Result<()> {
    let bytes = serde_json::to_vec_pretty(&FileOut {
        version: SCHEMA_VERSION,
        jobs,
    })
    .expect("jobs serialize");
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_log_keeps_the_tail_and_counts_what_it_dropped() {
        let mut log = Log::default();

        for i in 0..LOG_MAX_LINES + 7 {
            log.push(format!("line {i}"));
        }

        assert_eq!(log.lines.len(), LOG_MAX_LINES);
        assert_eq!(log.dropped, 7);
        assert_eq!(log.lines.front().unwrap(), "line 7");
    }

    #[test]
    fn the_log_is_capped_in_bytes_too() {
        let mut log = Log::default();
        let line = "x".repeat(LOG_MAX_BYTES / 4);

        for _ in 0..6 {
            log.push(line.clone());
        }

        assert_eq!(log.lines.len(), 4);
        assert_eq!(log.dropped, 2);
        assert!(log.bytes <= LOG_MAX_BYTES);
    }
}
