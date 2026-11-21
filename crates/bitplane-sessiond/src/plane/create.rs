//! The `plane.create` job kind (ADR 0034): one `bp` with the request on
//! stdin and progress on fd 3. Its phase comes from the progress events,
//! its log is `bp`'s stderr, and how it ended is `bp`'s answer alone.

use std::os::fd::OwnedFd;
use std::sync::Arc;

use bitplane_protocol::RpcError;
use bitplane_protocol::job::{JobClient, JobKind, JobPhase, JobStatus};
use bitplane_protocol::plane::{PlaneCreateDetail, PlaneCreateParams, PlaneCreateResult};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Child;

use super::Shared;
use super::bp::{self, Failure};
use crate::job::JobHandle;
use crate::module::Call;

/// Enough of stderr's end to hold `bp`'s envelope, its last line.
const STDERR_TAIL: usize = 256 * 1024;
const CREATE_ACTION: &str = "plane_create";

/// Spawns `bp` and answers with the job; the job runs on after the reply.
pub(super) fn start(
    shared: &Arc<Shared>,
    call: &Call,
    params: PlaneCreateParams,
) -> Result<Value, RpcError> {
    let Value::Object(mut request) = params.args else {
        return Err(RpcError::new(
            RpcError::INVALID_PARAMS,
            "args must be bp's PlaneCreateRequest, an object",
        ));
    };
    request.insert("action".into(), json!(CREATE_ACTION));
    let detail = PlaneCreateDetail {
        plane: request.get("id").and_then(Value::as_str).map(str::to_owned),
        ..Default::default()
    };
    let (child, progress) = shared
        .bp
        .spawn_with_progress()
        .map_err(|error| Failure::NotRun(format!("couldn't start bp: {error}")).into_rpc())?;
    let pid = child.id();
    let started_by = call.connection.client.clone().unwrap_or(JobClient {
        name: "unknown".into(),
        version: String::new(),
    });
    let job = shared.jobs.start(
        JobKind::PlaneCreate,
        started_by,
        json!(detail),
        Box::new(move || {
            if let Some(pid) = pid {
                // bp is a session leader (setsid) and expects Ctrl-C's reach: its
                // scripts get the SIGINT too, or a cancel waits them out.
                // SAFETY: kill(2) on our own child's group, which root only cancels while running.
                unsafe {
                    libc::kill(-(pid as libc::pid_t), libc::SIGINT);
                }
            }
        }),
    );
    let result = PlaneCreateResult {
        job: job.id().to_owned(),
    };
    let shared = shared.clone();
    tokio::spawn(async move {
        let request = Value::Object(request);
        let (status, detail) = run(child, progress, &request, &job, detail).await;
        job.finish(status, json!(detail));
        shared.wake.notify_one();
    });
    Ok(json!(result))
}

async fn run(
    mut child: Child,
    progress: OwnedFd,
    request: &Value,
    job: &JobHandle,
    mut detail: PlaneCreateDetail,
) -> (JobStatus, PlaneCreateDetail) {
    if let Some(mut stdin) = child.stdin.take() {
        let bytes = serde_json::to_vec(request).expect("JSON values serialize");
        // A bp that exits without reading is judged by its answer.
        let _ = stdin.write_all(&bytes).await;
    }
    let mut stdout = child.stdout.take().expect("stdout is piped");
    let stderr = child.stderr.take().expect("stderr is piped");
    let read_stdout = async {
        let mut bytes = Vec::new();
        let _ = stdout.read_to_end(&mut bytes).await;
        bytes
    };
    let (stdout, stderr, ()) = tokio::join!(
        read_stdout,
        read_stderr(stderr, job),
        read_progress(progress, job)
    );
    let code = match child.wait().await {
        Ok(status) => status.code(),
        Err(error) => {
            let reason = format!("bp could not be run: couldn't wait for bp: {error}");
            return (JobStatus::Failed { reason }, detail);
        }
    };
    let status = outcome(
        code,
        &stdout,
        &String::from_utf8_lossy(&stderr),
        &mut detail,
    );
    (status, detail)
}

/// Logs each line as it completes and returns stderr's tail.
async fn read_stderr(mut stderr: tokio::process::ChildStderr, job: &JobHandle) -> Vec<u8> {
    let mut tail = Vec::new();
    let mut pending = Vec::new();
    let mut buf = vec![0u8; 8192];
    loop {
        let read = match stderr.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(read) => read,
        };
        tail.extend_from_slice(&buf[..read]);
        if tail.len() > STDERR_TAIL {
            tail.drain(..tail.len() - STDERR_TAIL);
        }
        pending.extend_from_slice(&buf[..read]);
        let mut lines = Vec::new();
        while let Some(end) = pending.iter().position(|&byte| byte == b'\n') {
            let line: Vec<u8> = pending.drain(..=end).collect();
            lines.push(String::from_utf8_lossy(&line[..end]).into_owned());
        }
        job.log(lines);
    }
    if !pending.is_empty() {
        job.log(vec![String::from_utf8_lossy(&pending).into_owned()]);
    }
    tail
}

async fn read_progress(progress: OwnedFd, job: &JobHandle) {
    let Ok(receiver) = tokio::net::unix::pipe::Receiver::from_owned_fd(progress) else {
        tracing::warn!("couldn't read bp's progress; the job reports no phase");
        return;
    };
    let mut lines = BufReader::new(receiver).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if let Some(phase) = phase(&line) {
            job.phase(phase);
        }
    }
}

/// The phase a progress line says some member started, if it does.
fn phase(line: &str) -> Option<JobPhase> {
    #[derive(Deserialize)]
    struct Event {
        event: String,
        step: Option<String>,
        state: Option<String>,
    }
    let event: Event = serde_json::from_str(line).ok()?;
    if event.event != "member" || event.state.as_deref() != Some("started") {
        return None;
    }
    match event.step.as_deref()? {
        "fetch" => Some(JobPhase::Fetching),
        "worktree" => Some(JobPhase::AddingWorktrees),
        "script" => Some(JobPhase::RunningScripts),
        _ => None,
    }
}

/// How the run ended, from `bp`'s exit, result and envelope only; `detail`
/// gains what `bp` answered.
fn outcome(
    code: Option<i32>,
    stdout: &[u8],
    stderr: &str,
    detail: &mut PlaneCreateDetail,
) -> JobStatus {
    #[derive(Deserialize)]
    struct Created {
        id: String,
        directory: Option<String>,
        #[serde(default)]
        interrupted: bool,
        #[serde(default)]
        remnant: bool,
        #[serde(default)]
        members: Vec<Row>,
    }
    #[derive(Deserialize)]
    struct Row {
        outcome: String,
        #[serde(default)]
        detail: Option<Value>,
    }
    impl Row {
        fn kept(&self) -> bool {
            self.outcome == "ok"
                && self
                    .detail
                    .as_ref()
                    .and_then(|d| d.get("unwound"))
                    .and_then(Value::as_bool)
                    == Some(false)
        }
    }
    // An interrupted create answers on stdout with a non-zero exit.
    if let Ok(response) = serde_json::from_slice::<Value>(stdout)
        && let Ok(created) = Created::deserialize(&response)
    {
        detail.plane = Some(created.id);
        detail.directory = created.directory;
        detail.response = Some(response);
        // An interrupt past bp's point of no return (during scripts) keeps the
        // plane: interrupted, no remnant, and members that were never unwound.
        let kept = created.members.iter().any(Row::kept);
        return match (created.remnant, created.interrupted) {
            (true, _) => JobStatus::Partial,
            (false, true) if !kept => JobStatus::NothingCreated,
            (false, _) => JobStatus::Succeeded,
        };
    }
    let (_, envelope) = bp::split_envelope(stderr);
    if let Some(envelope) = envelope {
        let status = match envelope.get("error").and_then(Value::as_str) {
            Some("script_failed") => JobStatus::ScriptFailed,
            Some("create_aborted") if !left_nothing(&envelope) => JobStatus::Partial,
            _ => JobStatus::NothingCreated,
        };
        detail.envelope = Some(envelope);
        return status;
    }
    let reason = match bp::answer(code, stdout, stderr) {
        Err(Failure::NotRun(cause)) => format!("bp could not be run: {cause}"),
        _ => "bp could not be run: it answered with something other than a created plane".into(),
    };
    JobStatus::Failed { reason }
}

/// bp's `create_aborted` envelope does not carry `remnant`; only its remedy
/// tells a clean unwind from a remnant, so anything else counts as partial.
fn left_nothing(envelope: &Value) -> bool {
    envelope
        .get("remedy")
        .and_then(Value::as_str)
        .is_some_and(|remedy| remedy.starts_with("Nothing was left behind"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn judged(code: Option<i32>, stdout: &str, stderr: &str) -> (JobStatus, PlaneCreateDetail) {
        let mut detail = PlaneCreateDetail::default();
        let status = outcome(code, stdout.as_bytes(), stderr, &mut detail);
        (status, detail)
    }

    fn envelope(error: &str, remedy: &str) -> String {
        format!(
            r#"{{"error":{},"code":1,"message":"m","problems":[],"remedy":{}}}"#,
            json!(error),
            json!(remedy)
        )
    }

    #[test]
    fn a_result_on_stdout_decides_by_interrupted_and_remnant() {
        let created = |interrupted: bool, remnant: bool| {
            json!({"action": "plane_create", "id": "auth", "directory": "/p/auth",
                   "members": [], "interrupted": interrupted, "remnant": remnant, "scripts": []})
            .to_string()
        };
        let member = |unwound: bool| {
            json!({"member": "@api", "outcome": "ok",
                   "detail": {"branch": "auth", "created_branch": true, "path": "api", "unwound": unwound}})
        };
        let interrupted_with = |members: Vec<Value>| {
            json!({"action": "plane_create", "id": "auth", "directory": "/p/auth",
                   "members": members, "interrupted": true, "remnant": false, "scripts": []})
            .to_string()
        };

        let (status, detail) = judged(Some(0), &created(false, false), "");
        assert_eq!(status, JobStatus::Succeeded);
        assert_eq!(detail.plane.as_deref(), Some("auth"));
        assert_eq!(detail.directory.as_deref(), Some("/p/auth"));
        assert!(detail.response.is_some());
        assert_eq!(
            judged(Some(130), &created(true, false), "").0,
            JobStatus::NothingCreated
        );
        assert_eq!(
            judged(Some(130), &created(true, true), "").0,
            JobStatus::Partial
        );
        assert_eq!(
            judged(Some(130), &interrupted_with(vec![member(true)]), "").0,
            JobStatus::NothingCreated
        );
        let skipped = json!({"member": "@web", "outcome": "skipped", "detail": "interrupted"});
        assert_eq!(
            judged(
                Some(130),
                &interrupted_with(vec![member(false), skipped]),
                ""
            )
            .0,
            JobStatus::Succeeded
        );
    }

    #[test]
    fn an_envelope_decides_by_its_tag_and_an_aborts_remedy() {
        let script = envelope("script_failed", "Re-run it.");
        let clean = envelope(
            "create_aborted",
            "Nothing was left behind. Fix what the rows report, then run bp create again.",
        );
        let remnant = envelope(
            "create_aborted",
            "Run bp destroy -p auth to clear the remnant.",
        );
        let refused = envelope("lock_timeout", "Try again.");

        let (status, detail) = judged(Some(1), "", &format!("$ pnpm i\nERR\n{script}\n"));
        assert_eq!(status, JobStatus::ScriptFailed);
        assert_eq!(detail.envelope.unwrap()["error"], "script_failed");
        assert_eq!(judged(Some(1), "", &clean).0, JobStatus::NothingCreated);
        assert_eq!(judged(Some(1), "", &remnant).0, JobStatus::Partial);
        assert_eq!(judged(Some(5), "", &refused).0, JobStatus::NothingCreated);
    }

    #[test]
    fn no_answer_is_failed_with_the_cause() {
        let JobStatus::Failed { reason } = judged(None, "", "").0 else {
            panic!()
        };
        assert!(reason.contains("killed by a signal"), "{reason}");

        let JobStatus::Failed { reason } = judged(Some(101), "", "panicked\n").0 else {
            panic!()
        };
        assert!(
            reason.contains("exited 101") && reason.ends_with("panicked"),
            "{reason}"
        );
    }

    #[test]
    fn only_a_started_member_step_names_a_phase() {
        assert_eq!(
            phase(r#"{"event":"member","member":"@api","step":"worktree","state":"started"}"#),
            Some(JobPhase::AddingWorktrees)
        );
        assert_eq!(
            phase(
                r#"{"event":"member","member":"@api","step":"script","state":"started","name":"i","log":"/l"}"#
            ),
            Some(JobPhase::RunningScripts)
        );
        assert_eq!(
            phase(r#"{"event":"member","member":"@api","step":"fetch","state":"done"}"#),
            None
        );
        assert_eq!(phase(r#"{"event":"waiting","object":"/p"}"#), None);
        assert_eq!(phase("not json"), None);
    }
}
