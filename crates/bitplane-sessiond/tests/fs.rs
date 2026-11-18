mod support;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use bitplane_protocol::{ErrorCode, Notification, RpcError};
use bitplane_sessiond::client::{Client, ClientError};
use bitplane_sessiond::env::EnvMap;
use bitplane_sessiond::fs::Fs;
use serde_json::{Value, json};
use support::TestDaemon;

const WAIT: Duration = Duration::from_secs(5);
const QUIET: Duration = Duration::from_millis(1500);
/// Lets a fresh OS watcher settle before the test changes anything.
const SETTLE: Duration = Duration::from_millis(400);

#[tokio::test]
async fn a_listing_flags_ignored_entries_and_everything_in_git() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("web");
    git_init(&repo);
    write(&repo.join(".gitignore"), "target/\n*.log\n");
    write(&repo.join("target/app"), "");
    write(&repo.join("src/main.rs"), "");
    write(&repo.join("src/debug.log"), "");
    let daemon = TestDaemon::start_with_env(vec![Fs::module()], git_env()).await;
    let client = daemon.connect_hello().await;

    assert_eq!(
        list(&client, &repo).await,
        [
            (".git", "directory", true),
            (".gitignore", "file", false),
            ("src", "directory", false),
            ("target", "directory", true),
        ]
    );
    assert_eq!(
        list(&client, &repo.join("src")).await,
        [("debug.log", "file", true), ("main.rs", "file", false)]
    );
    let git = list(&client, &repo.join(".git")).await;
    assert!(!git.is_empty());
    assert!(git.iter().all(|(_, _, ignored)| *ignored), "{git:?}");
}

#[tokio::test]
async fn a_symlinked_folder_inherits_the_links_ignore_status() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("web");
    git_init(&repo);
    write(&repo.join(".gitignore"), ".notes\n");
    let outside = tempfile::tempdir().unwrap();
    write(&outside.path().join("plans/a.md"), "");
    for link in [".notes", "docs"] {
        std::os::unix::fs::symlink(outside.path(), repo.join(link)).unwrap();
    }
    let daemon = TestDaemon::start_with_env(vec![Fs::module()], git_env()).await;
    let client = daemon.connect_hello().await;

    assert_eq!(
        list(&client, &repo.join(".notes/plans")).await,
        [("a.md", "file", true)]
    );
    assert_eq!(
        list(&client, &repo.join("docs/plans")).await,
        [("a.md", "file", false)]
    );
}

#[tokio::test]
async fn without_git_nothing_is_ignored_but_dot_git() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("web");
    git_init(&repo);
    write(&repo.join(".gitignore"), "a.txt\n");
    write(&repo.join("a.txt"), "");
    let env = EnvMap::from([("PATH".into(), "/nonexistent".into())]);
    let daemon = TestDaemon::start_with_env(vec![Fs::module()], env).await;
    let client = daemon.connect_hello().await;

    assert_eq!(
        list(&client, &repo).await,
        [
            (".git", "directory", true),
            (".gitignore", "file", false),
            ("a.txt", "file", false),
        ]
    );
}

#[tokio::test]
async fn read_returns_the_raw_blob_and_only_its_size_past_max_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("a.bin");
    std::fs::write(&file, b"one\0two").unwrap();
    let daemon = TestDaemon::start(vec![Fs::module()]).await;
    let client = daemon.connect_hello().await;

    let blob = client
        .call("fs.read", Some(json!({ "path": file })))
        .await
        .unwrap();
    let capped = client
        .call("fs.read", Some(json!({ "path": file, "maxBytes": 3 })))
        .await
        .unwrap();

    assert_eq!(blob["size"], 7);
    assert_eq!(
        STANDARD.decode(blob["data"].as_str().unwrap()).unwrap(),
        b"one\0two"
    );
    assert_eq!(capped, json!({ "size": 7 }));
}

#[tokio::test]
async fn missing_paths_are_not_found_and_relative_ones_invalid() {
    let dir = tempfile::tempdir().unwrap();
    let daemon = TestDaemon::start(vec![Fs::module()]).await;
    let client = daemon.connect_hello().await;
    let missing = dir.path().join("missing");

    for (method, path) in [
        ("fs.list", missing.clone()),
        ("fs.read", missing),
        ("fs.read", dir.path().to_path_buf()),
    ] {
        let error = rpc_error(client.call(method, Some(json!({ "path": path }))).await);
        assert_eq!(error.protocol_code(), Some(ErrorCode::NotFound), "{method}");
    }
    let error = rpc_error(client.call("fs.list", Some(json!({ "path": "rel" }))).await);
    assert_eq!(error.code, RpcError::INVALID_PARAMS);
}

#[tokio::test]
async fn only_newly_added_paths_get_initial_state() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = (folder(dir.path(), "a"), folder(dir.path(), "b"));
    let daemon = TestDaemon::start(vec![Fs::module()]).await;
    let mut client = daemon.connect_hello().await;

    watch(&client, &[&a]).await;
    assert_eq!(next_change(&mut client).await, changed(&a));
    watch(&client, &[&a, &b]).await;
    assert_eq!(next_change(&mut client).await, changed(&b));
    assert_quiet(&mut client).await;
}

#[tokio::test]
async fn changes_in_a_watched_folder_arrive_but_not_from_its_subfolders() {
    let dir = tempfile::tempdir().unwrap();
    let a = folder(dir.path(), "a");
    folder(&a, "sub");
    let daemon = TestDaemon::start(vec![Fs::module()]).await;
    let mut client = daemon.connect_hello().await;
    watch_settled(&mut client, &[&a]).await;

    write(&a.join("sub/deep.txt"), "x");
    assert_quiet(&mut client).await;
    write(&a.join("new.txt"), "x");
    assert_eq!(next_change(&mut client).await, changed(&a));
}

#[tokio::test]
async fn an_empty_watch_stops_events() {
    let dir = tempfile::tempdir().unwrap();
    let a = folder(dir.path(), "a");
    let daemon = TestDaemon::start(vec![Fs::module()]).await;
    let mut client = daemon.connect_hello().await;
    watch_settled(&mut client, &[&a]).await;

    watch(&client, &[]).await;
    write(&a.join("new.txt"), "x");

    assert_quiet(&mut client).await;
}

#[tokio::test]
async fn unwatchable_paths_are_gone_at_once() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("a.txt");
    write(&file, "x");
    let missing = dir.path().join("missing");
    let daemon = TestDaemon::start(vec![Fs::module()]).await;
    let mut client = daemon.connect_hello().await;

    watch(&client, &[&file, &missing]).await;

    let mut events = vec![
        next_change(&mut client).await,
        next_change(&mut client).await,
    ];
    events.sort_by_key(|event| event["path"].as_str().unwrap().to_string());
    assert_eq!(events, [gone(&file), gone(&missing)]);
}

#[tokio::test]
async fn a_watched_folder_that_vanishes_is_gone_once_and_never_rearmed() {
    let dir = tempfile::tempdir().unwrap();
    let a = folder(dir.path(), "a");
    let daemon = TestDaemon::start(vec![Fs::module()]).await;
    let mut client = daemon.connect_hello().await;
    watch_settled(&mut client, &[&a]).await;

    std::fs::remove_dir_all(&a).unwrap();
    assert_eq!(next_change(&mut client).await, gone(&a));
    folder(dir.path(), "a");
    write(&a.join("new.txt"), "x");

    assert_quiet(&mut client).await;
}

#[tokio::test]
async fn two_connections_share_a_folder_each_under_its_own_spelling() {
    let dir = tempfile::tempdir().unwrap();
    let a = folder(dir.path(), "a");
    let other_spelling = a.join("../a");
    let daemon = TestDaemon::start(vec![Fs::module()]).await;
    let mut first = daemon.connect_hello().await;
    let mut second = daemon.connect_hello().await;
    watch_settled(&mut first, &[&a]).await;
    watch_settled(&mut second, &[&other_spelling]).await;

    write(&a.join("one.txt"), "x");
    assert_eq!(next_change(&mut first).await, changed(&a));
    assert_eq!(next_change(&mut second).await, changed(&other_spelling));

    watch(&first, &[]).await;
    write(&a.join("two.txt"), "x");
    assert_eq!(next_change(&mut second).await, changed(&other_spelling));
    assert_quiet(&mut first).await;
}

#[tokio::test]
async fn a_closed_connection_leaves_the_others_watching() {
    let dir = tempfile::tempdir().unwrap();
    let a = folder(dir.path(), "a");
    let daemon = TestDaemon::start(vec![Fs::module()]).await;
    let mut first = daemon.connect_hello().await;
    let mut second = daemon.connect_hello().await;
    watch_settled(&mut first, &[&a]).await;
    watch_settled(&mut second, &[&a]).await;

    drop(first);
    tokio::time::sleep(SETTLE).await;
    write(&a.join("new.txt"), "x");

    assert_eq!(next_change(&mut second).await, changed(&a));
}

async fn list(client: &Client, path: &Path) -> Vec<(&'static str, &'static str, bool)> {
    let result = client
        .call("fs.list", Some(json!({ "path": path })))
        .await
        .unwrap();
    result["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| {
            (
                leak(entry["name"].as_str().unwrap()),
                leak(entry["kind"].as_str().unwrap()),
                entry["ignored"].as_bool().unwrap(),
            )
        })
        .collect()
}

async fn watch(client: &Client, paths: &[&Path]) {
    let reply = client
        .call("fs.watch", Some(json!({ "paths": paths })))
        .await
        .unwrap();
    assert_eq!(reply, json!({}));
}

/// Watches `paths`, takes their initial events and lets the watcher settle.
async fn watch_settled(client: &mut Client, paths: &[&Path]) {
    watch(client, paths).await;
    for _ in paths {
        let event = next_change(client).await;
        assert!(event.get("gone").is_none(), "{event}");
    }
    tokio::time::sleep(SETTLE).await;
}

async fn next_change(client: &mut Client) -> Value {
    let event: Notification = tokio::time::timeout(WAIT, client.next_event())
        .await
        .expect("an fs.changed event")
        .expect("the connection is open");
    assert_eq!(event.method, "fs.changed");
    event.params.unwrap()
}

async fn assert_quiet(client: &mut Client) {
    if let Ok(event) = tokio::time::timeout(QUIET, client.next_event()).await {
        panic!("expected no event, got {event:?}");
    }
}

fn changed(path: &Path) -> Value {
    json!({ "path": path })
}

fn gone(path: &Path) -> Value {
    json!({ "path": path, "gone": true })
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

fn leak(text: &str) -> &'static str {
    Box::leak(text.to_string().into_boxed_str())
}

/// The host's `PATH` for git, with no user or system config.
fn git_env() -> EnvMap {
    EnvMap::from([
        ("PATH".into(), std::env::var("PATH").unwrap_or_default()),
        ("GIT_CONFIG_GLOBAL".into(), "/dev/null".into()),
        ("GIT_CONFIG_NOSYSTEM".into(), "1".into()),
    ])
}

fn git_init(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    let output = Command::new("git")
        .args(["init", "-q", "-b", "main"])
        .current_dir(dir)
        .env_clear()
        .envs(git_env())
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
}
