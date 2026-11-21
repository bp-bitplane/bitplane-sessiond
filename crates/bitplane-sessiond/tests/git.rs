mod support;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use bitplane_protocol::{ErrorCode, Notification, RpcError};
use bitplane_sessiond::client::{Client, ClientError};
use bitplane_sessiond::env::EnvMap;
use bitplane_sessiond::git::{Git, NO_GIT};
use serde_json::{Value, json};
use support::TestDaemon;

const WAIT: Duration = Duration::from_secs(5);
const QUIET: Duration = Duration::from_millis(1500);
/// Lets a fresh OS watcher settle before the test changes anything.
const SETTLE: Duration = Duration::from_millis(400);

#[tokio::test]
async fn status_reports_each_path_in_order() {
    let dir = tempfile::tempdir().unwrap();
    let web = committed_repo(dir.path(), "web");
    write(&web.join("a.txt"), "one\nstaged\n");
    git(&web, &["add", "a.txt"]);
    write(&web.join("a.txt"), "one\nstaged\nunstaged\n");
    write(&web.join("new.txt"), "x");
    let api = committed_repo(dir.path(), "api");
    let plain = folder(dir.path(), "plain");
    let missing = dir.path().join("missing");
    let daemon = start().await;
    let client = daemon.connect_hello().await;

    let result = client
        .call(
            "git.status",
            Some(json!({ "paths": [&web, &api, &plain, &missing] })),
        )
        .await
        .unwrap();

    let repos = result["repos"].as_array().unwrap();
    assert_eq!(repos[0]["path"], json!(web));
    assert_eq!(
        repos[0]["status"],
        json!({
            "head": {"kind": "branch", "name": "main"},
            "staged": [{"path": "a.txt", "kind": "modified"}],
            "unstaged": [{"path": "a.txt", "kind": "modified"}],
            "untracked": ["new.txt"],
        })
    );
    assert_eq!(
        repos[1]["status"],
        json!({
            "head": {"kind": "branch", "name": "main"},
            "staged": [],
            "unstaged": [],
            "untracked": [],
        })
    );
    assert_eq!(repos[2], json!({ "path": plain, "gone": true }));
    assert_eq!(repos[3], json!({ "path": missing, "gone": true }));
}

#[tokio::test]
async fn show_returns_the_blob_at_head_and_in_the_index() {
    let dir = tempfile::tempdir().unwrap();
    let web = committed_repo(dir.path(), "web");
    write(&web.join("a.txt"), "staged\n");
    git(&web, &["add", "a.txt"]);
    write(&web.join("a.txt"), "worktree\n");
    let daemon = start().await;
    let client = daemon.connect_hello().await;

    assert_eq!(show(&client, &web, "a.txt", "head").await, b"one\ntwo\n");
    assert_eq!(show(&client, &web, "a.txt", "index").await, b"staged\n");
    let capped = client
        .call(
            "git.show",
            Some(json!({ "repo": web, "path": "a.txt", "rev": "head", "maxBytes": 2 })),
        )
        .await
        .unwrap();
    assert_eq!(capped, json!({ "size": 8 }));
}

#[tokio::test]
async fn show_of_a_missing_blob_or_a_non_repo_is_not_found() {
    let dir = tempfile::tempdir().unwrap();
    let web = committed_repo(dir.path(), "web");
    write(&web.join("new.txt"), "x");
    let plain = folder(dir.path(), "plain");
    let daemon = start().await;
    let client = daemon.connect_hello().await;

    for (repo, path, rev) in [
        (&web, "new.txt", "head"),
        (&web, "new.txt", "index"),
        (&web, "missing.txt", "head"),
        (&plain, "a.txt", "head"),
    ] {
        let error = rpc_error(
            client
                .call(
                    "git.show",
                    Some(json!({ "repo": repo, "path": path, "rev": rev })),
                )
                .await,
        );
        assert_eq!(error.protocol_code(), Some(ErrorCode::NotFound), "{path}");
    }
    let error = rpc_error(
        client
            .call(
                "git.show",
                Some(json!({ "repo": web, "path": "../x", "rev": "head" })),
            )
            .await,
    );
    assert_eq!(error.code, RpcError::INVALID_PARAMS);
}

#[tokio::test]
async fn only_newly_added_repos_get_initial_state() {
    let dir = tempfile::tempdir().unwrap();
    let web = committed_repo(dir.path(), "web");
    let api = committed_repo(dir.path(), "api");
    let daemon = start().await;
    let mut client = daemon.connect_hello().await;

    watch(&client, &[&web]).await;
    assert_eq!(next_change(&mut client).await["path"], json!(web));
    watch(&client, &[&web, &api]).await;
    let event = next_change(&mut client).await;
    assert_eq!(event["path"], json!(api));
    assert_eq!(event["status"]["head"]["name"], "main");
    assert_quiet(&mut client).await;
}

#[tokio::test]
async fn edit_stage_and_commit_each_send_the_new_status() {
    let dir = tempfile::tempdir().unwrap();
    let web = committed_repo(dir.path(), "web");
    let daemon = start().await;
    let mut client = daemon.connect_hello().await;
    watch_settled(&mut client, &[&web]).await;

    write(&web.join("a.txt"), "changed\n");
    let edited = next_status(&mut client, |status| {
        !status["unstaged"].as_array().unwrap().is_empty()
    })
    .await;
    assert_eq!(
        edited["unstaged"],
        json!([{"path": "a.txt", "kind": "modified"}])
    );

    git(&web, &["add", "a.txt"]);
    let staged = next_status(&mut client, |status| {
        !status["staged"].as_array().unwrap().is_empty()
    })
    .await;
    assert_eq!(staged["unstaged"], json!([]));

    git(&web, &["commit", "-q", "-m", "change"]);
    next_status(&mut client, |status| {
        status["staged"].as_array().unwrap().is_empty()
            && status["unstaged"].as_array().unwrap().is_empty()
    })
    .await;
}

#[tokio::test]
async fn changes_in_ignored_folders_send_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let web = committed_repo(dir.path(), "web");
    folder(&web, "node_modules");
    let daemon = start().await;
    let mut client = daemon.connect_hello().await;
    watch_settled(&mut client, &[&web]).await;

    write(&web.join("node_modules/x/index.js"), "x");

    assert_quiet(&mut client).await;
}

#[tokio::test]
async fn an_empty_watch_stops_events() {
    let dir = tempfile::tempdir().unwrap();
    let web = committed_repo(dir.path(), "web");
    let daemon = start().await;
    let mut client = daemon.connect_hello().await;
    watch_settled(&mut client, &[&web]).await;

    watch(&client, &[]).await;
    write(&web.join("a.txt"), "changed\n");

    assert_quiet(&mut client).await;
}

#[tokio::test]
async fn a_non_repo_path_is_gone_at_once() {
    let dir = tempfile::tempdir().unwrap();
    let plain = folder(dir.path(), "plain");
    let daemon = start().await;
    let mut client = daemon.connect_hello().await;

    watch(&client, &[&plain]).await;

    assert_eq!(
        next_change(&mut client).await,
        json!({ "path": plain, "gone": true })
    );
}

#[tokio::test]
async fn a_removed_repo_is_gone_once_then_silent() {
    let dir = tempfile::tempdir().unwrap();
    let web = committed_repo(dir.path(), "web");
    let daemon = start().await;
    let mut client = daemon.connect_hello().await;
    watch_settled(&mut client, &[&web]).await;

    std::fs::remove_dir_all(&web).unwrap();
    loop {
        let event = next_change(&mut client).await;
        if event.get("gone").is_some() {
            assert_eq!(event, json!({ "path": web, "gone": true }));
            break;
        }
    }
    committed_repo(dir.path(), "web");
    write(&web.join("a.txt"), "changed\n");

    assert_quiet(&mut client).await;
}

#[tokio::test]
async fn two_connections_each_get_events_under_their_own_spelling() {
    let dir = tempfile::tempdir().unwrap();
    let web = committed_repo(dir.path(), "web");
    let other_spelling = web.join("../web");
    let daemon = start().await;
    let mut first = daemon.connect_hello().await;
    let mut second = daemon.connect_hello().await;
    watch_settled(&mut first, &[&web]).await;
    watch_settled(&mut second, &[&other_spelling]).await;

    write(&web.join("a.txt"), "changed\n");

    assert_eq!(next_change(&mut first).await["path"], json!(web));
    assert_eq!(
        next_change(&mut second).await["path"],
        json!(other_spelling)
    );
}

#[tokio::test]
async fn without_git_the_module_is_unavailable_with_a_reason() {
    let env = EnvMap::from([("PATH".into(), "/nonexistent".into())]);
    let daemon = TestDaemon::start_with_env(vec![Git::module()], env).await;
    let client = daemon.connect().await;

    let hello = client.hello("test").await.unwrap();
    let error = rpc_error(
        client
            .call("git.status", Some(json!({ "paths": [] })))
            .await,
    );

    let git = &hello.capabilities[0];
    assert_eq!(git.module, "git");
    assert!(!git.available);
    assert_eq!(git.reason.as_deref(), Some(NO_GIT));
    assert_eq!(error.protocol_code(), Some(ErrorCode::ModuleUnavailable));
}

async fn start() -> TestDaemon {
    TestDaemon::start_with_env(vec![Git::module()], git_env()).await
}

async fn show(client: &Client, repo: &Path, path: &str, rev: &str) -> Vec<u8> {
    let blob = client
        .call(
            "git.show",
            Some(json!({ "repo": repo, "path": path, "rev": rev })),
        )
        .await
        .unwrap();
    STANDARD.decode(blob["data"].as_str().unwrap()).unwrap()
}

async fn watch(client: &Client, paths: &[&Path]) {
    let reply = client
        .call("git.watch", Some(json!({ "paths": paths })))
        .await
        .unwrap();
    assert_eq!(reply, json!({}));
}

/// Watches `paths`, takes their initial events and lets the watcher settle.
async fn watch_settled(client: &mut Client, paths: &[&Path]) {
    watch(client, paths).await;
    for _ in paths {
        let event = next_change(client).await;
        assert!(event.get("status").is_some(), "{event}");
    }
    tokio::time::sleep(SETTLE).await;
    while let Ok(Some(_)) =
        tokio::time::timeout(Duration::from_millis(300), client.next_event()).await
    {}
}

/// The status of the first `git.changed` whose status satisfies `done`.
async fn next_status(client: &mut Client, done: impl Fn(&Value) -> bool) -> Value {
    loop {
        let event = next_change(client).await;
        let status = &event["status"];
        assert!(status.is_object(), "{event}");
        if done(status) {
            return status.clone();
        }
    }
}

async fn next_change(client: &mut Client) -> Value {
    let event: Notification = tokio::time::timeout(WAIT, client.next_event())
        .await
        .expect("a git.changed event")
        .expect("the connection is open");
    assert_eq!(event.method, "git.changed");
    event.params.unwrap()
}

async fn assert_quiet(client: &mut Client) {
    if let Ok(event) = tokio::time::timeout(QUIET, client.next_event()).await {
        panic!("expected no event, got {event:?}");
    }
}

fn rpc_error(result: Result<Value, ClientError>) -> RpcError {
    match result {
        Err(ClientError::Rpc(error)) => error,
        other => panic!("expected an RPC error, got {other:?}"),
    }
}

fn folder(parent: &Path, name: &str) -> PathBuf {
    let path = parent.join(name);
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn write(path: &Path, contents: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}

/// A repo holding a committed `a.txt`.
fn committed_repo(parent: &Path, name: &str) -> PathBuf {
    let dir = folder(parent, name);
    git(&dir, &["init", "-q", "-b", "main"]);
    write(&dir.join("a.txt"), "one\ntwo\n");
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-q", "-m", "init"]);
    dir
}

fn git(dir: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env_clear()
        .envs(git_env())
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
}

/// The host's `PATH` for git, with no user or system config.
fn git_env() -> EnvMap {
    EnvMap::from([
        ("PATH".into(), std::env::var("PATH").unwrap_or_default()),
        ("GIT_CONFIG_GLOBAL".into(), "/dev/null".into()),
        ("GIT_CONFIG_NOSYSTEM".into(), "1".into()),
        ("GIT_AUTHOR_NAME".into(), "Test".into()),
        ("GIT_AUTHOR_EMAIL".into(), "test@example.com".into()),
        ("GIT_COMMITTER_NAME".into(), "Test".into()),
        ("GIT_COMMITTER_EMAIL".into(), "test@example.com".into()),
    ])
}
