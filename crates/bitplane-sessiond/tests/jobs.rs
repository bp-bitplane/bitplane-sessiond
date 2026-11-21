mod support;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use bitplane_protocol::{ErrorCode, RpcError};
use bitplane_sessiond::DAEMON_VERSION;
use bitplane_sessiond::client::{Client, ClientError};
use bitplane_sessiond::env::EnvMap;
use bitplane_sessiond::job::{FINISHED_KEPT, LOG_LOST, LOG_MAX_LINES};
use bitplane_sessiond::plane::Plane;
use serde_json::{Value, json};
use support::{Host, Stop, TestDaemon, shell_env};

const WAIT: Duration = Duration::from_secs(10);

/// `bp list` answers no planes. `plane_create` writes `$FAKE_BP/progress`
/// to fd 3, `log` and `lines` (that many numbered lines) to stderr, waits
/// while `hold` exists (or, with `script`, runs a child script until SIGINT
/// reaches it), then writes `progress.after` and `log.after`; a SIGINT
/// answers `interrupted.stdout`. Then any action answers from
/// `<action>.{stderr,stdout,exit}`.
const FAKE_BP: &str = r#"#!/bin/sh
case " $* " in
*" --version "*)
	echo '{"version":"0.1.0","contract":1}'
	exit 0
	;;
esac
request=$(cat)
printf '%s\n' "$request" >> "$FAKE_BP/requests"
printf '%s\n' "$*" >> "$FAKE_BP/args"
action=$(printf '%s' "$request" | sed -n 's/.*"action":"\([a-z_]*\)".*/\1/p')
if [ "$action" = plane_create ]; then
	trap 'cat "$FAKE_BP/interrupted.stdout"; exit 130' INT
	[ -f "$FAKE_BP/log" ] && cat "$FAKE_BP/log" >&2
	[ -f "$FAKE_BP/lines" ] && seq 1 "$(cat "$FAKE_BP/lines")" >&2
	[ -f "$FAKE_BP/progress" ] && cat "$FAKE_BP/progress" >&3
	while [ -f "$FAKE_BP/hold" ]; do sleep 0.05; done
	[ -f "$FAKE_BP/script" ] && sh -c 'trap "touch \"$FAKE_BP/script.interrupted\"; exit 130" INT; while :; do sleep 0.05; done'
	[ -f "$FAKE_BP/progress.after" ] && cat "$FAKE_BP/progress.after" >&3
	[ -f "$FAKE_BP/log.after" ] && cat "$FAKE_BP/log.after" >&2
fi
[ -f "$FAKE_BP/$action.stderr" ] && cat "$FAKE_BP/$action.stderr" >&2
[ -f "$FAKE_BP/$action.stdout" ] && cat "$FAKE_BP/$action.stdout"
exit "$(cat "$FAKE_BP/$action.exit" 2>/dev/null || echo 0)"
"#;

struct Fake {
    _root: tempfile::TempDir,
    control: PathBuf,
    bin: PathBuf,
    home: PathBuf,
    planes: PathBuf,
}

impl Fake {
    fn new() -> Fake {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().canonicalize().unwrap();
        let fake = Fake {
            control: path.join("control"),
            bin: path.join("bin"),
            home: path.join("home"),
            planes: path.join("planes"),
            _root: root,
        };
        for dir in [&fake.control, &fake.bin, &fake.home] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let bp = fake.bin.join("bp");
        std::fs::write(&bp, FAKE_BP).unwrap();
        std::fs::set_permissions(&bp, std::fs::Permissions::from_mode(0o755)).unwrap();
        fake.answer(
            "plane_list",
            r#"{"action":"plane_list","planes":[]}"#,
            "",
            0,
        );
        fake.created("auth", false, false);
        fake
    }

    fn env(&self) -> EnvMap {
        let mut env = shell_env(&self.home);
        env.insert(
            "PATH".into(),
            format!("{}:/usr/bin:/bin", self.bin.display()),
        );
        env.insert("FAKE_BP".into(), self.control.display().to_string());
        env.insert(
            "BITPLANE_PLANES_DIR".into(),
            self.planes.display().to_string(),
        );
        env
    }

    fn set(&self, name: &str, contents: &str) {
        std::fs::write(self.control.join(name), contents).unwrap();
    }

    fn unset(&self, name: &str) {
        let _ = std::fs::remove_file(self.control.join(name));
    }

    fn answer(&self, action: &str, stdout: &str, stderr: &str, exit: i32) {
        self.set(&format!("{action}.stdout"), stdout);
        self.set(&format!("{action}.stderr"), stderr);
        self.set(&format!("{action}.exit"), &exit.to_string());
    }

    /// `bp create` answers with a plane, as `interrupted` and `remnant` say.
    fn created(&self, id: &str, interrupted: bool, remnant: bool) {
        let stdout = created(&self.planes, id, interrupted, remnant);
        self.answer(
            "plane_create",
            &stdout,
            "",
            if interrupted { 130 } else { 0 },
        );
    }

    /// `bp create` refuses with an envelope.
    fn refused(&self, error: &str, remedy: &str) {
        // bp writes `error` first, as its struct declares it.
        let envelope = format!(
            r#"{{"error":{},"code":1,"message":"it did not work","problems":[],"remedy":{}}}"#,
            json!(error),
            json!(remedy)
        );
        self.answer(
            "plane_create",
            "",
            &format!("doing things\n{envelope}\n"),
            1,
        );
    }

    fn lines(&self, name: &str) -> Vec<String> {
        std::fs::read_to_string(self.control.join(name))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    async fn start(&self) -> TestDaemon {
        TestDaemon::start_with_env(vec![Plane::module()], self.env()).await
    }
}

fn created(planes: &Path, id: &str, interrupted: bool, remnant: bool) -> String {
    json!({"action": "plane_create", "id": id, "directory": planes.join(id), "members": [],
           "interrupted": interrupted, "remnant": remnant, "scripts": []})
    .to_string()
}

fn progress(steps: &[(&str, &str, &str)]) -> String {
    steps
        .iter()
        .map(|(member, step, state)| {
            format!(
                "{}\n",
                json!({"event": "member", "member": member, "step": step, "state": state})
            )
        })
        .collect()
}

fn args(id: &str) -> Value {
    json!({"members": ["@api", "@web"], "branch": "feat", "id": id, "here": null,
           "intent": "resolve", "fetch": true, "run_scripts": true})
}

#[tokio::test]
async fn create_runs_bp_with_the_request_and_reports_the_furthest_phase_then_bps_result() {
    let fake = Fake::new();
    fake.set(
        "progress",
        &progress(&[
            ("@api", "fetch", "started"),
            ("@api", "fetch", "done"),
            ("@api", "worktree", "started"),
            ("@web", "fetch", "started"),
        ]),
    );
    fake.set(
        "progress.after",
        &progress(&[("@api", "script", "started")]),
    );
    fake.set("hold", "");
    let daemon = fake.start().await;
    let mut starter = daemon.connect().await;
    starter.hello("cc").await.unwrap();
    let mut other = daemon.connect_hello().await;

    let reply = starter
        .call("plane.create", Some(json!({ "args": args("auth") })))
        .await
        .unwrap();

    let id = reply["job"].as_str().unwrap().to_owned();
    let running = changed_until(&mut other, &id, |job| job["phase"] == "addingWorktrees").await;
    assert_eq!(running["kind"], "plane.create");
    assert_eq!(running["status"], json!({"state": "running"}));
    assert_eq!(
        running["startedBy"],
        json!({"name": "cc", "version": DAEMON_VERSION})
    );
    assert_eq!(running["detail"], json!({"plane": "auth"}));
    tokio::time::sleep(Duration::from_millis(300)).await;
    let listed = list(&starter).await;
    assert_eq!(listed.len(), 1);
    assert_eq!(
        listed[0]["phase"], "addingWorktrees",
        "a member that starts fetching later does not move the phase back"
    );

    fake.unset("hold");
    let done = finished(&mut starter, &id).await;
    assert_eq!(done["status"], json!({"state": "succeeded"}));
    assert_eq!(done["phase"], "runningScripts");
    assert!(done["finishedAt"].is_u64());
    assert_eq!(done["detail"]["plane"], "auth");
    assert_eq!(done["detail"]["directory"], json!(fake.planes.join("auth")));
    assert_eq!(done["detail"]["response"]["interrupted"], false);
    let mut request = args("auth");
    request["action"] = json!("plane_create");
    let sent: Value = serde_json::from_str(fake.lines("requests").last().unwrap()).unwrap();
    assert_eq!(sent, request);
    assert!(
        fake.lines("args")
            .last()
            .unwrap()
            .ends_with("--progress-fd 3"),
        "{:?}",
        fake.lines("args")
    );
}

#[tokio::test]
async fn the_final_status_is_bps_answer() {
    let fake = Fake::new();
    let daemon = fake.start().await;
    let mut client = daemon.connect_hello().await;
    let cases: Vec<(Box<dyn Fn()>, Value)> = vec![
        (
            Box::new(|| fake.refused("script_failed", "Re-run the script.")),
            json!({"state": "scriptFailed"}),
        ),
        (
            Box::new(|| {
                fake.refused(
                    "create_aborted",
                    "Nothing was left behind. Fix what the rows report, then run bp create again.",
                )
            }),
            json!({"state": "nothingCreated"}),
        ),
        (
            Box::new(|| {
                fake.refused(
                    "create_aborted",
                    "Run bp destroy -p auth to clear the remnant.",
                )
            }),
            json!({"state": "partial"}),
        ),
        (
            Box::new(|| fake.refused("lock_timeout", "Try again.")),
            json!({"state": "nothingCreated"}),
        ),
        (
            Box::new(|| fake.created("auth", true, true)),
            json!({"state": "partial"}),
        ),
    ];

    for (answer, status) in cases {
        answer();
        let id = create(&client, "auth").await;
        let done = finished(&mut client, &id).await;
        assert_eq!(done["status"], status, "{done}");
    }

    fake.answer("plane_create", "", "thread 'main' panicked\n", 101);
    let id = create(&client, "auth").await;
    let done = finished(&mut client, &id).await;
    let reason = done["status"]["reason"].as_str().unwrap();
    assert_eq!(done["status"]["state"], "failed");
    assert!(
        reason.starts_with("bp could not be run") && reason.contains("exited 101"),
        "{reason}"
    );
}

#[tokio::test]
async fn a_refusal_keeps_bps_envelope_in_the_detail() {
    let fake = Fake::new();
    fake.refused("script_failed", "Re-run the script.");
    let daemon = fake.start().await;
    let mut client = daemon.connect_hello().await;

    let id = create(&client, "auth").await;

    let done = finished(&mut client, &id).await;
    assert_eq!(done["detail"]["envelope"]["error"], "script_failed");
    assert_eq!(done["detail"]["envelope"]["remedy"], "Re-run the script.");
}

#[tokio::test]
async fn a_watch_sends_the_backlog_then_live_lines_and_only_to_watchers() {
    let fake = Fake::new();
    fake.set("log", "one\ntwo\n");
    fake.set("progress", &progress(&[("@api", "worktree", "started")]));
    fake.set("log.after", "three\n");
    fake.set("hold", "");
    let daemon = fake.start().await;
    let mut watcher = daemon.connect_hello().await;
    let mut bystander = daemon.connect_hello().await;
    let id = create(&watcher, "auth").await;
    changed_until(&mut watcher, &id, |job| job["phase"] == "addingWorktrees").await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    let reply = watcher
        .call(
            "job.watch",
            Some(json!({ "jobs": [id.clone(), "no-such-job"] })),
        )
        .await
        .unwrap();

    assert_eq!(reply, json!({}));
    let mut seen = Vec::new();
    while seen.len() < 2 {
        let (method, params) = event(&mut watcher).await;
        if method.starts_with("job.") && method != "job.changed" {
            seen.push((method, params));
        }
    }
    seen.sort_by_key(|(method, _)| method.clone());
    assert_eq!(
        seen,
        [
            (
                "job.log".to_owned(),
                json!({"job": id, "lines": ["one", "two"]})
            ),
            ("job.removed".to_owned(), json!({"job": "no-such-job"})),
        ]
    );
    fake.unset("hold");
    let live = next(&mut watcher, "job.log").await;
    assert_eq!(live, json!({"job": id, "lines": ["three"]}));
    finished(&mut watcher, &id).await;

    let bystander_saw = drain(&mut bystander).await;
    assert!(
        bystander_saw
            .iter()
            .any(|(method, _)| method == "job.changed")
            && !bystander_saw.iter().any(|(method, _)| method == "job.log"),
        "{bystander_saw:?}"
    );

    let mut late = daemon.connect_hello().await;
    late.call("job.watch", Some(json!({ "jobs": [id.clone()] })))
        .await
        .unwrap();
    let backlog = next(&mut late, "job.log").await;
    assert_eq!(
        backlog,
        json!({"job": id, "lines": ["one", "two", "three"]}),
        "a finished job sends its whole backlog"
    );
    let rewatched = late
        .call("job.watch", Some(json!({ "jobs": [id.clone()] })))
        .await;
    assert_eq!(rewatched.unwrap(), json!({}));
    let after = drain(&mut late).await;
    assert!(
        !after.iter().any(|(method, _)| method == "job.log"),
        "a job already watched gets no backlog again: {after:?}"
    );
}

#[tokio::test]
async fn a_long_log_keeps_its_tail_and_the_backlog_counts_the_dropped_lines() {
    let fake = Fake::new();
    let extra = 50;
    fake.set("lines", &(LOG_MAX_LINES + extra).to_string());
    let daemon = fake.start().await;
    let mut client = daemon.connect_hello().await;
    let id = create(&client, "auth").await;
    finished(&mut client, &id).await;

    client
        .call("job.watch", Some(json!({ "jobs": [id.clone()] })))
        .await
        .unwrap();

    let backlog = next(&mut client, "job.log").await;
    let lines = backlog["lines"].as_array().unwrap();
    assert_eq!(lines.len(), LOG_MAX_LINES);
    assert_eq!(lines[0], (extra + 1).to_string());
    assert_eq!(
        lines.last().unwrap(),
        &json!((LOG_MAX_LINES + extra).to_string())
    );
    assert_eq!(backlog["dropped"], extra);
}

#[tokio::test]
async fn any_client_cancels_and_dismisses_and_the_errors_say_why_not() {
    let fake = Fake::new();
    fake.set("hold", "");
    fake.set("progress", &progress(&[("@api", "worktree", "started")]));
    fake.set(
        "interrupted.stdout",
        &created(&fake.planes, "auth", true, false),
    );
    let daemon = fake.start().await;
    let mut starter = daemon.connect_hello().await;
    let other = daemon.connect_hello().await;
    let id = create(&starter, "auth").await;
    changed_until(&mut starter, &id, |job| job["phase"] == "addingWorktrees").await;
    let job = json!({ "job": id });

    let unknown = rpc_error(other.call("job.cancel", Some(json!({"job": "nope"}))).await);
    assert_eq!(unknown.protocol_code(), Some(ErrorCode::UnknownJob));
    let running = rpc_error(other.call("job.dismiss", Some(job.clone())).await);
    assert_eq!(running.code, RpcError::INVALID_REQUEST);

    assert_eq!(
        other.call("job.cancel", Some(job.clone())).await.unwrap(),
        json!({})
    );
    let done = finished(&mut starter, &id).await;
    assert_eq!(done["status"], json!({"state": "nothingCreated"}));
    assert_eq!(done["detail"]["response"]["interrupted"], true);
    let again = rpc_error(starter.call("job.cancel", Some(job.clone())).await);
    assert_eq!(again.protocol_code(), Some(ErrorCode::JobFinished));

    assert_eq!(
        other.call("job.dismiss", Some(job.clone())).await.unwrap(),
        json!({})
    );
    assert_eq!(next(&mut starter, "job.removed").await, job);
    assert!(list(&starter).await.is_empty());
    let gone = rpc_error(starter.call("job.dismiss", Some(job.clone())).await);
    assert_eq!(gone.protocol_code(), Some(ErrorCode::UnknownJob));
    starter
        .call("job.watch", Some(json!({ "jobs": [id.clone()] })))
        .await
        .unwrap();
    assert_eq!(
        next(&mut starter, "job.removed").await,
        job,
        "a dismissed job gets job.removed at once"
    );
}

#[tokio::test]
async fn a_cancel_during_scripts_reaches_the_script_and_a_kept_plane_succeeded() {
    let fake = Fake::new();
    fake.set("script", "");
    fake.set("progress", &progress(&[("@api", "script", "started")]));
    let kept = json!({"member": "@api", "outcome": "ok",
        "detail": {"branch": "feat", "created_branch": true, "path": "api", "unwound": false}});
    fake.set(
        "interrupted.stdout",
        &json!({"action": "plane_create", "id": "auth", "directory": fake.planes.join("auth"),
                "members": [kept], "interrupted": true, "remnant": false, "scripts": []})
        .to_string(),
    );
    let daemon = fake.start().await;
    let mut client = daemon.connect_hello().await;
    let id = create(&client, "auth").await;
    changed_until(&mut client, &id, |job| job["phase"] == "runningScripts").await;

    client
        .call("job.cancel", Some(json!({ "job": id })))
        .await
        .unwrap();
    let done = finished(&mut client, &id).await;

    assert!(fake.control.join("script.interrupted").exists());
    assert_eq!(done["status"], json!({"state": "succeeded"}));
    assert_eq!(done["detail"]["response"]["interrupted"], true);
}

#[tokio::test]
async fn only_the_last_finished_jobs_are_kept() {
    let fake = Fake::new();
    let daemon = fake.start().await;
    let mut client = daemon.connect_hello().await;
    let extra = 3;
    let mut ids = Vec::new();
    let mut removed = Vec::new();

    for _ in 0..FINISHED_KEPT + extra {
        let id = create(&client, "auth").await;
        loop {
            let (method, params) = event(&mut client).await;
            match method.as_str() {
                "job.removed" => removed.push(params["job"].as_str().unwrap().to_owned()),
                "job.changed"
                    if params["job"]["id"] == id
                        && params["job"]["status"]["state"] != "running" =>
                {
                    break;
                }
                _ => {}
            }
        }
        ids.push(id);
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    for (method, params) in drain(&mut client).await {
        if method == "job.removed" {
            removed.push(params["job"].as_str().unwrap().to_owned());
        }
    }

    assert_eq!(removed, ids[..extra]);
    let listed: Vec<Value> = list(&client)
        .await
        .iter()
        .map(|job| job["id"].clone())
        .collect();
    assert_eq!(listed, ids[extra..]);
}

#[tokio::test]
async fn after_a_restart_the_table_is_back_and_an_interrupted_job_failed() {
    let fake = Fake::new();
    fake.set("log", "fetching @api\n");
    let daemon = fake.start().await;
    let mut client = daemon.connect_hello().await;
    let done = create(&client, "auth").await;
    finished(&mut client, &done).await;
    fake.set("hold", "");
    fake.set("progress", &progress(&[("@api", "worktree", "started")]));
    let interrupted = create(&client, "web").await;
    changed_until(&mut client, &interrupted, |job| {
        job["phase"] == "addingWorktrees"
    })
    .await;
    drop(client);

    let daemon = daemon.restart(vec![Plane::module()], fake.env()).await;
    let mut client = daemon.connect_hello().await;

    let listed = list(&client).await;
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0]["id"], json!(done));
    assert_eq!(listed[0]["status"], json!({"state": "succeeded"}));
    assert_eq!(listed[1]["id"], json!(interrupted));
    assert_eq!(
        listed[1]["status"],
        json!({"state": "failed", "reason": "daemon restarted"})
    );
    assert_eq!(listed[1]["phase"], "addingWorktrees");
    assert!(listed[1]["finishedAt"].is_u64());
    client
        .call("job.watch", Some(json!({ "jobs": [interrupted.clone()] })))
        .await
        .unwrap();
    assert_eq!(
        next(&mut client, "job.log").await,
        json!({"job": interrupted, "lines": [LOG_LOST]})
    );
    let error = rpc_error(
        client
            .call("job.cancel", Some(json!({ "job": interrupted })))
            .await,
    );
    assert_eq!(error.protocol_code(), Some(ErrorCode::JobFinished));
}

#[tokio::test]
async fn another_versions_jobs_are_adopted_when_its_daemon_is_not_running() {
    let host = Host::new();
    let older = host.other_version("0");
    let dir = older.state.join("job");
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("jobs.json");
    let job = json!({"id": "old-1", "kind": "plane.create", "status": {"state": "running"},
                     "startedBy": {"name": "cc", "version": "0.0.1"}, "startedAt": 1,
                     "detail": {"plane": "auth"}});
    std::fs::write(&file, json!({"version": 1, "jobs": [job]}).to_string()).unwrap();

    let mut daemon = host.serve();
    let client = daemon.connect().await;
    client.hello("test").await.unwrap();

    let listed = list(&client).await;
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0]["id"], "old-1");
    assert_eq!(
        listed[0]["status"],
        json!({"state": "failed", "reason": "daemon restarted"})
    );
    assert!(!file.exists(), "the adopted file is deleted");
    daemon.stop(&client, Stop::Sigterm).await;
    let saved: Value = serde_json::from_slice(
        &std::fs::read(host.dirs.state.join("job").join("jobs.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(saved["jobs"][0]["id"], "old-1");
}

async fn create(client: &Client, id: &str) -> String {
    let reply = client
        .call("plane.create", Some(json!({ "args": args(id) })))
        .await
        .unwrap();
    reply["job"].as_str().unwrap().to_owned()
}

async fn list(client: &Client) -> Vec<Value> {
    let result = client.call("job.list", None).await.unwrap();
    result["jobs"].as_array().unwrap().clone()
}

async fn event(client: &mut Client) -> (String, Value) {
    let event = tokio::time::timeout(WAIT, client.next_event())
        .await
        .expect("an event")
        .expect("the connection is open");
    (event.method, event.params.unwrap_or_default())
}

async fn next(client: &mut Client, method: &str) -> Value {
    loop {
        let (got, params) = event(client).await;
        if got == method {
            return params;
        }
    }
}

/// The first `job.changed` for `id` whose job satisfies `done`.
async fn changed_until(client: &mut Client, id: &str, done: impl Fn(&Value) -> bool) -> Value {
    loop {
        let params = next(client, "job.changed").await;
        let job = &params["job"];
        if job["id"] == id && done(job) {
            return job.clone();
        }
    }
}

async fn finished(client: &mut Client, id: &str) -> Value {
    changed_until(client, id, |job| job["status"]["state"] != "running").await
}

/// Every event that arrives within a short quiet period.
async fn drain(client: &mut Client) -> Vec<(String, Value)> {
    let mut events = Vec::new();
    while let Ok(Some(event)) =
        tokio::time::timeout(Duration::from_millis(300), client.next_event()).await
    {
        events.push((event.method, event.params.unwrap_or_default()));
    }
    events
}

fn rpc_error(result: Result<Value, ClientError>) -> RpcError {
    match result {
        Err(ClientError::Rpc(error)) => error,
        other => panic!("expected an RPC error, got {other:?}"),
    }
}
