mod support;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use bitplane_protocol::{ErrorCode, RpcError};
use bitplane_sessiond::client::{Client, ClientError};
use bitplane_sessiond::env::EnvMap;
use bitplane_sessiond::plane::{NO_BP, Plane};
use bitplane_sessiond::session::Sessions;
use serde_json::{Value, json};
use support::{Screen, TestDaemon, open, shell_env};

const WAIT: Duration = Duration::from_secs(10);

/// Answers `--version --json` from `$FAKE_BP/version`, and a request on
/// stdin from `$FAKE_BP/<action>.{sleep,stderr,stdout,exit}`, recording
/// each request and its arguments.
const FAKE_BP: &str = r#"#!/bin/sh
case " $* " in
*" --version "*)
	[ -f "$FAKE_BP/version" ] || exit 1
	cat "$FAKE_BP/version"
	exit 0
	;;
esac
request=$(cat)
printf '%s\n' "$request" >> "$FAKE_BP/requests"
printf '%s\n' "$*" >> "$FAKE_BP/args"
action=$(printf '%s' "$request" | sed -n 's/.*"action":"\([a-z_]*\)".*/\1/p')
[ -f "$FAKE_BP/$action.sleep" ] && sleep "$(cat "$FAKE_BP/$action.sleep")"
[ -f "$FAKE_BP/$action.stderr" ] && cat "$FAKE_BP/$action.stderr" >&2
[ -f "$FAKE_BP/$action.stdout" ] && cat "$FAKE_BP/$action.stdout"
exit "$(cat "$FAKE_BP/$action.exit" 2>/dev/null || echo 0)"
"#;

const ENVELOPE: &str = r#"{"error":"plane_not_found","code":1,"message":"there is no plane called nope","problems":[],"remedy":"Run bp list."}"#;

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
        fake.set("version", r#"{"version":"0.1.0","contract":1}"#);
        fake.planes_listed(&[]);
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

    fn answer(&self, action: &str, stdout: &str, stderr: &str, exit: i32) {
        self.set(&format!("{action}.stdout"), stdout);
        self.set(&format!("{action}.stderr"), stderr);
        self.set(&format!("{action}.exit"), &exit.to_string());
    }

    /// `bp list` reports these planes, each with an `@api` member.
    fn planes_listed(&self, ids: &[&str]) {
        let planes: Vec<Value> = ids
            .iter()
            .map(|id| {
                json!({
                    "id": id,
                    "directory": self.planes.join(id),
                    "created_at": null,
                    "members": [{"member": "@api", "path": "api", "head": {"head": "branch", "branch": "main"}}],
                    "health": {"checked": "cheap", "findings": [
                        {"finding": "member_worktree_missing", "member": "@api", "path": "api"}
                    ]},
                })
            })
            .collect();
        let response = json!({"action": "plane_list", "planes": planes});
        self.answer("plane_list", &response.to_string(), "", 3);
    }

    fn plane_dir(&self, id: &str) -> PathBuf {
        let dir = self.planes.join(id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("plane.toml"), "version = 1\n").unwrap();
        dir
    }

    fn requests(&self) -> Vec<Value> {
        read_lines(&self.control.join("requests"))
            .iter()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    async fn start(&self) -> TestDaemon {
        TestDaemon::start_with_env(vec![Plane::module()], self.env()).await
    }
}

#[tokio::test]
async fn hello_reports_bps_version_and_contract() {
    let fake = Fake::new();
    let daemon = fake.start().await;

    let hello = daemon.connect().await.hello("test").await.unwrap();

    let plane = &hello.capabilities[0];
    assert_eq!(plane.module, "plane");
    assert!(plane.available);
    assert_eq!(
        plane.info,
        Some(json!({"bp": {"version": "0.1.0", "contract": 1}}))
    );
}

#[tokio::test]
async fn without_bp_the_module_is_unavailable_with_a_reason_naming_the_fix() {
    let env = EnvMap::from([("PATH".into(), "/nonexistent".into())]);
    let daemon = TestDaemon::start_with_env(vec![Plane::module()], env).await;
    let client = daemon.connect().await;

    let hello = client.hello("test").await.unwrap();
    let error = rpc_error(client.call("plane.list", None).await);

    assert!(!hello.capabilities[0].available);
    assert_eq!(hello.capabilities[0].reason.as_deref(), Some(NO_BP));
    assert_eq!(error.protocol_code(), Some(ErrorCode::ModuleUnavailable));
}

#[tokio::test]
async fn a_bp_that_answers_no_version_makes_the_module_unavailable() {
    let fake = Fake::new();
    std::fs::remove_file(fake.control.join("version")).unwrap();
    let daemon = fake.start().await;

    let hello = daemon.connect().await.hello("test").await.unwrap();

    let plane = &hello.capabilities[0];
    assert!(!plane.available);
    let reason = plane.reason.as_deref().unwrap();
    assert!(reason.contains("--version --json") && reason.contains("restart the daemon"));
}

#[tokio::test]
async fn list_returns_planes_with_members_and_findings_and_drafts() {
    let fake = Fake::new();
    let auth = fake.plane_dir("auth");
    std::fs::create_dir_all(fake.planes.join("scratch")).unwrap();
    std::fs::create_dir_all(fake.planes.join(".hidden")).unwrap();
    fake.planes_listed(&["auth"]);
    let daemon = fake.start().await;
    let client = daemon.connect_hello().await;

    let model = loaded(&client).await;

    assert_eq!(model["planesDir"], json!(fake.planes));
    assert_eq!(model["planesDirExists"], true);
    assert!(model.get("listError").is_none(), "{model}");
    assert_eq!(
        model["planes"],
        json!([
            {"id": "auth", "directory": auth, "kind": "plane",
             "members": [{"member": "@api", "path": auth.join("api")}],
             "findings": ["member_worktree_missing"]},
            {"id": "scratch", "directory": fake.planes.join("scratch"), "kind": "draft",
             "members": [], "findings": []},
        ])
    );
    assert_eq!(
        fake.requests()[0],
        json!({"action": "plane_list", "health": "cheap"})
    );
    let args = read_lines(&fake.control.join("args"));
    assert_eq!(
        args[0],
        format!("--stdin --json --planes-dir {}", fake.planes.display())
    );
}

#[tokio::test]
async fn a_planes_dir_change_broadcasts_the_model_and_a_vanished_plane_is_gone() {
    let fake = Fake::new();
    fake.plane_dir("auth");
    fake.planes_listed(&["auth"]);
    let daemon = fake.start().await;
    let mut client = daemon.connect_hello().await;
    loaded(&client).await;
    tokio::time::sleep(Duration::from_millis(400)).await;

    fake.planes_listed(&["auth", "web"]);
    fake.plane_dir("web");
    let model = changed_until(&mut client, |model| kind(model, "web") == Some("plane")).await;
    assert_eq!(kind(&model, "auth"), Some("plane"));

    fake.planes_listed(&["auth"]);
    std::fs::remove_dir_all(fake.planes.join("web")).unwrap();
    let model = changed_until(&mut client, |model| kind(model, "web") == Some("gone")).await;
    let web = row(&model, "web").unwrap();
    assert_eq!(web["directory"], json!(fake.planes.join("web")));
    assert_eq!(web["members"], json!([]));
}

#[tokio::test]
async fn refresh_reruns_bp_list_and_answers_with_plane_changed() {
    let fake = Fake::new();
    let daemon = fake.start().await;
    let mut client = daemon.connect_hello().await;
    loaded(&client).await;

    fake.planes_listed(&["elsewhere"]);
    let reply = client.call("plane.refresh", None).await.unwrap();

    assert_eq!(reply, json!({}));
    let model = changed_until(&mut client, |model| row(model, "elsewhere").is_some()).await;
    assert_eq!(kind(&model, "elsewhere"), Some("plane"));
}

#[tokio::test]
async fn a_failed_list_keeps_the_rows_and_carries_bps_envelope() {
    let fake = Fake::new();
    fake.plane_dir("auth");
    fake.planes_listed(&["auth"]);
    let daemon = fake.start().await;
    let mut client = daemon.connect_hello().await;
    loaded(&client).await;
    let git_missing = r#"{"error":"git_missing","code":4,"message":"git was not found on PATH","problems":[],"remedy":null}"#;

    fake.answer("plane_list", "", &format!("{git_missing}\n"), 4);
    client.call("plane.refresh", None).await.unwrap();

    let model = changed_until(&mut client, |model| model.get("listError").is_some()).await;
    assert_eq!(
        model["listError"],
        json!({
            "message": "git was not found on PATH",
            "envelope": serde_json::from_str::<Value>(git_missing).unwrap(),
        })
    );
    assert_eq!(kind(&model, "auth"), Some("plane"));
}

#[tokio::test]
async fn projects_lists_bps_projects_by_name() {
    let fake = Fake::new();
    fake.answer(
        "project_list",
        &json!({"action": "project_list", "projects": [
            {"project": "web", "outcome": "ok", "detail": {"source": {"type": "owned", "url": "u"}, "source_repo_present": false}},
            {"project": "api", "outcome": "ok", "detail": {"source": {"type": "adopted", "path": "/x"}, "source_repo_present": true}},
            {"project": "broken", "outcome": "failed", "detail": {"error": "parse_error"}},
        ]})
        .to_string(),
        "",
        3,
    );
    let daemon = fake.start().await;
    let client = daemon.connect_hello().await;

    let result = client.call("plane.projects", None).await.unwrap();

    assert_eq!(
        result,
        json!({"projects": [
            {"name": "api", "sourceRepoPresent": true},
            {"name": "broken", "sourceRepoPresent": false},
            {"name": "web", "sourceRepoPresent": false},
        ]})
    );
}

#[tokio::test]
async fn create_draft_makes_the_folder_and_refuses_a_clash() {
    let fake = Fake::new();
    let daemon = fake.start().await;
    let mut client = daemon.connect_hello().await;
    loaded(&client).await;

    let reply = client
        .call("plane.createDraft", Some(json!({ "id": "scratch" })))
        .await
        .unwrap();

    assert_eq!(reply, json!({}));
    assert!(fake.planes.join("scratch").is_dir());
    let model = changed_until(&mut client, |model| kind(model, "scratch") == Some("draft")).await;
    assert_eq!(model["planesDirExists"], true);
    let clash = rpc_error(
        client
            .call("plane.createDraft", Some(json!({ "id": "scratch" })))
            .await,
    );
    assert_eq!(clash.protocol_code(), Some(ErrorCode::PlaneExists));
    let invalid = rpc_error(
        client
            .call("plane.createDraft", Some(json!({ "id": "Bad Id" })))
            .await,
    );
    assert_eq!(invalid.code, RpcError::INVALID_PARAMS);
}

#[tokio::test]
async fn dispatch_passes_the_request_through_and_returns_bps_response() {
    let fake = Fake::new();
    let response = json!({"action": "plane_status", "id": "auth", "future_field": [1, 2]});
    fake.answer("plane_status", &response.to_string(), "", 3);
    let daemon = fake.start().await;
    let client = daemon.connect_hello().await;
    let request =
        json!({"action": "plane_status", "plane": {"plane": "id", "id": "auth"}, "unknown": true});

    let result = client
        .call("plane.dispatch", Some(json!({ "request": request })))
        .await
        .unwrap();

    assert_eq!(result, json!({ "response": response }));
    assert!(fake.requests().contains(&request));
}

#[tokio::test]
async fn a_refusal_is_bp_refused_with_the_envelope_as_data() {
    let fake = Fake::new();
    fake.answer(
        "plane_show",
        "",
        &format!("waiting for /p…\n{ENVELOPE}\n"),
        1,
    );
    let daemon = fake.start().await;
    let client = daemon.connect_hello().await;

    let error = rpc_error(
        client
            .call(
                "plane.dispatch",
                Some(json!({"request": {"action": "plane_show"}})),
            )
            .await,
    );

    assert_eq!(error.protocol_code(), Some(ErrorCode::BpRefused));
    assert_eq!(error.message, "there is no plane called nope");
    assert_eq!(
        error.data.unwrap().detail,
        Some(serde_json::from_str(ENVELOPE).unwrap())
    );
}

#[tokio::test]
async fn bp_that_cannot_answer_is_an_internal_error_naming_the_cause() {
    let fake = Fake::new();
    fake.answer("plane_show", "", "thread 'main' panicked\n", 101);
    fake.answer("plane_status", "not json", "", 0);
    let daemon = fake.start().await;
    let client = daemon.connect_hello().await;
    let dispatch = |action: &str| {
        client.call(
            "plane.dispatch",
            Some(json!({"request": {"action": action}})),
        )
    };

    let crashed = rpc_error(dispatch("plane_show").await);
    let garbled = rpc_error(dispatch("plane_status").await);
    std::fs::remove_file(fake.bin.join("bp")).unwrap();
    let missing = rpc_error(dispatch("plane_show").await);

    for error in [&crashed, &garbled, &missing] {
        assert_eq!(error.code, RpcError::INTERNAL_ERROR);
        assert!(
            error.message.starts_with("bp could not be run"),
            "{error:?}"
        );
        assert!(error.data.is_none());
    }
    assert!(crashed.message.contains("exited 101"), "{crashed:?}");
    assert!(crashed.message.ends_with("thread 'main' panicked"));
    assert!(
        garbled.message.contains("without a response"),
        "{garbled:?}"
    );
    assert!(missing.message.contains("couldn't start bp"), "{missing:?}");
}

#[tokio::test]
async fn a_slow_bp_call_does_not_delay_sessions_or_typing() {
    let fake = Fake::new();
    fake.set("plane_status.sleep", "3");
    fake.answer("plane_status", r#"{"action":"plane_status"}"#, "", 0);
    let daemon =
        TestDaemon::start_with_env(vec![Sessions::module(), Plane::module()], fake.env()).await;
    let slow = daemon.connect_hello().await;
    let mut typing = daemon.connect_hello().await;
    let root = tempfile::tempdir().unwrap();
    let session = open(&typing, json!({ "root": root.path() })).await;
    let mut screen = Screen::attach(&mut typing, session["id"].as_str().unwrap()).await;
    let started = Instant::now();

    let dispatch = slow.call(
        "plane.dispatch",
        Some(json!({"request": {"action": "plane_status"}})),
    );
    let others = async {
        tokio::time::sleep(Duration::from_millis(200)).await;
        slow.call("session.list", None).await.unwrap();
        let listed = started.elapsed();
        screen.type_text(&typing, "echo typed-$((40 + 2))\r");
        screen.wait_for(&mut typing, "typed-42").await;
        (listed, started.elapsed())
    };
    let (dispatched, (listed, typed)) = tokio::join!(dispatch, others);

    assert!(
        listed < Duration::from_secs(2),
        "session.list took {listed:?}"
    );
    assert!(typed < Duration::from_secs(2), "typing took {typed:?}");
    assert_eq!(
        dispatched.unwrap(),
        json!({"response": {"action": "plane_status"}})
    );
    assert!(started.elapsed() >= Duration::from_secs(3));
}

/// The model once `bp list` has answered.
async fn loaded(client: &Client) -> Value {
    let deadline = Instant::now() + WAIT;
    loop {
        let model = client.call("plane.list", None).await.unwrap();
        if model.get("refreshedAt").is_some() {
            return model;
        }
        assert!(Instant::now() < deadline, "bp list never answered");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// The first `plane.changed` whose model satisfies `done`.
async fn changed_until(client: &mut Client, done: impl Fn(&Value) -> bool) -> Value {
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let event = tokio::time::timeout_at(deadline, client.next_event())
            .await
            .expect("a matching plane.changed")
            .expect("the connection is open");
        assert_eq!(event.method, "plane.changed");
        let model = event.params.unwrap();
        if done(&model) {
            return model;
        }
    }
}

fn row<'a>(model: &'a Value, id: &str) -> Option<&'a Value> {
    model["planes"]
        .as_array()?
        .iter()
        .find(|row| row["id"] == id)
}

fn kind<'a>(model: &'a Value, id: &str) -> Option<&'a str> {
    row(model, id)?["kind"].as_str()
}

fn rpc_error(result: Result<Value, ClientError>) -> RpcError {
    match result {
        Err(ClientError::Rpc(error)) => error,
        other => panic!("expected an RPC error, got {other:?}"),
    }
}

fn read_lines(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}
