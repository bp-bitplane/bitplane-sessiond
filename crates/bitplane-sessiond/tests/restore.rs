mod support;

use std::time::{Duration, Instant};

use bitplane_sessiond::client::Client;
use bitplane_sessiond::pidfile::Pidfile;
use serde_json::{Value, json};
use support::{Host, Screen, Stop, open, write_sessions};

#[tokio::test]
async fn shutdown_with_restore_brings_every_session_back() {
    sessions_come_back_after(Stop::ShutdownWithRestore).await;
}

#[tokio::test]
async fn a_sigterm_brings_every_session_back() {
    sessions_come_back_after(Stop::Sigterm).await;
}

#[tokio::test]
async fn a_sigkill_brings_every_session_back() {
    sessions_come_back_after(Stop::Sigkill).await;
}

async fn sessions_come_back_after(stop: Stop) {
    let host = Host::new();
    let mut first = host.serve();
    let mut client = first.connect().await;
    let before = client.hello("test").await.unwrap().daemon.instance;
    let roots: Vec<tempfile::TempDir> = (0..4).map(|_| tempfile::tempdir().unwrap()).collect();
    let mut opened = Vec::new();
    for (i, root) in roots.iter().enumerate() {
        let session = open(
            &client,
            json!({
                "root": root.path(),
                "name": format!("tab-{i}"),
                "plane": format!("plane-{i}"),
                "env": { "RESTORE_MARK": format!("mark-{i}") },
            }),
        )
        .await;
        opened.push(session["id"].as_str().unwrap().to_owned());
    }
    client
        .call(
            "session.rename",
            Some(json!({ "session": opened[1], "name": "renamed" })),
        )
        .await
        .unwrap();
    client
        .call("session.close", Some(json!({ "session": opened[3] })))
        .await
        .unwrap();
    let mut screen = Screen::attach_with(
        &mut client,
        json!({ "session": opened[2], "cols": 120, "rows": 40 }),
    )
    .await;
    screen.type_text(&client, "echo sized-$((6*7))\r");
    screen.wait_for(&mut client, "sized-42").await;
    let listed = client.call("session.list", None).await.unwrap()["sessions"].clone();
    assert_eq!(listed[2]["size"], json!({ "cols": 120, "rows": 40 }));

    first.stop(&client, stop).await;
    drop(client);

    let mut second = host.serve();
    let mut client = second.connect().await;
    let after = client.hello("test").await.unwrap().daemon.instance;
    assert_ne!(before, after, "a new instance");
    let restored = client.call("session.list", None).await.unwrap()["sessions"].clone();
    let summary = |sessions: &Value| -> Vec<Value> {
        sessions
            .as_array()
            .unwrap()
            .iter()
            .map(|s| {
                json!([
                    s["id"], s["seq"], s["name"], s["plane"], s["root"], s["size"]
                ])
            })
            .collect()
    };
    assert_eq!(summary(&restored), summary(&listed), "after {stop:?}");
    assert_eq!(restored[1]["name"], "renamed");
    for session in restored.as_array().unwrap() {
        assert_eq!(session["status"], json!({ "state": "running" }));
        assert!(session.get("title").is_none());
    }
    let mut screen = Screen::attach(&mut client, &opened[2]).await;
    screen.type_text(&client, "echo \"got-$RESTORE_MARK-$BITPLANE_SESSION_ID\"\r");
    screen
        .wait_for(&mut client, &format!("got-mark-2-{}", opened[2]))
        .await;
}

#[tokio::test]
async fn shutdown_without_restore_hangs_up_every_session_and_forgets_them() {
    let host = Host::new();
    let mut first = host.serve();
    let mut client = first.connect().await;
    client.hello("test").await.unwrap();
    let root = tempfile::tempdir().unwrap();
    let session = open(&client, json!({ "root": root.path() })).await;
    let mut screen = Screen::attach(&mut client, session["id"].as_str().unwrap()).await;
    screen.type_text(&client, "echo $$ > pid; echo pid-$((6*7))\r");
    screen.wait_for(&mut client, "pid-42").await;
    let pid: i32 = std::fs::read_to_string(root.path().join("pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();

    first.shutdown(&client, false).await;

    let deadline = Instant::now() + Duration::from_secs(5);
    let pid = rustix::process::Pid::from_raw(pid).unwrap();
    while rustix::process::test_kill_process(pid).is_ok() {
        assert!(Instant::now() < deadline, "the shell outlived the shutdown");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let mut second = host.serve();
    let client = second.connect().await;
    client.hello("test").await.unwrap();
    let listed = client.call("session.list", None).await.unwrap();
    assert_eq!(listed["sessions"], json!([]));
}

#[tokio::test]
async fn exited_and_spawn_failed_sessions_come_back_running() {
    let host = Host::new();
    let mut first = host.serve();
    let mut client = first.connect().await;
    client.hello("test").await.unwrap();
    let root = tempfile::tempdir().unwrap();
    let exited = open(&client, json!({ "root": root.path() })).await;
    let screen = Screen::attach(&mut client, exited["id"].as_str().unwrap()).await;
    screen.type_text(&client, "exit 3\r");
    wait_for_status(&client, 0, json!({ "state": "exited", "code": 3 })).await;
    let shell = root.path().join("shell");
    let failed = open(
        &client,
        json!({ "root": root.path(), "env": { "SHELL": shell } }),
    )
    .await;
    assert_eq!(failed["status"]["state"], "spawnFailed");
    std::os::unix::fs::symlink("/bin/sh", &shell).unwrap();

    first.stop(&client, Stop::Sigterm).await;

    let mut second = host.serve();
    let client = second.connect().await;
    client.hello("test").await.unwrap();
    let listed = client.call("session.list", None).await.unwrap()["sessions"].clone();
    assert_eq!(listed[0]["id"], exited["id"]);
    assert_eq!(listed[1]["id"], failed["id"]);
    for session in listed.as_array().unwrap() {
        assert_eq!(session["status"], json!({ "state": "running" }));
    }
}

#[tokio::test]
async fn a_session_whose_root_is_gone_comes_back_exited_until_the_root_returns() {
    let host = Host::new();
    let mut first = host.serve();
    let client = first.connect().await;
    client.hello("test").await.unwrap();
    let parent = tempfile::tempdir().unwrap();
    let root = parent.path().join("plane");
    std::fs::create_dir(&root).unwrap();
    let session = open(&client, json!({ "root": root })).await;
    let id = session["id"].as_str().unwrap();
    first.stop(&client, Stop::Sigterm).await;
    std::fs::remove_dir(&root).unwrap();

    let mut second = host.serve();
    let client = second.connect().await;
    client.hello("test").await.unwrap();
    let listed = client.call("session.list", None).await.unwrap()["sessions"].clone();
    assert_eq!(listed[0]["id"], id);
    assert_eq!(listed[0]["status"]["state"], "exited");
    assert_eq!(listed[0]["rootGone"], true);
    assert!(
        client
            .call("session.restart", Some(json!({ "session": id })))
            .await
            .is_err()
    );

    std::fs::create_dir(&root).unwrap();
    let restarted = client
        .call("session.restart", Some(json!({ "session": id })))
        .await
        .unwrap();
    assert_eq!(
        restarted["session"]["status"],
        json!({ "state": "running" })
    );
    assert_eq!(restarted["session"]["rootGone"], false);
}

#[tokio::test]
async fn another_versions_sessions_are_adopted_when_its_daemon_is_not_running() {
    let host = Host::new();
    let mut first = host.serve();
    let client = first.connect().await;
    client.hello("test").await.unwrap();
    let root = tempfile::tempdir().unwrap();
    let ours: Vec<Value> = [
        open(&client, json!({ "root": root.path(), "name": "ours-1" })).await,
        open(&client, json!({ "root": root.path(), "name": "ours-2" })).await,
    ]
    .into();
    first.stop(&client, Stop::Sigterm).await;
    let older = host.other_version("0");
    let older_file = write_sessions(
        &older,
        json!({ "version": 1, "sessions": [
            { "id": "old-b", "seq": 9, "root": root.path(), "name": "old-b",
              "size": { "cols": 100, "rows": 30 },
              "agent": { "agentSession": "a-1", "cwd": root.path() } },
            { "id": "old-a", "seq": 4, "root": root.path(), "name": "old-a",
              "size": { "cols": 80, "rows": 24 } },
        ]}),
    );
    let running = host.other_version("7");
    let running_file = write_sessions(
        &running,
        json!({ "version": 1, "sessions": [
            { "id": "live", "seq": 1, "root": root.path(), "size": { "cols": 80, "rows": 24 } },
        ]}),
    );
    std::fs::create_dir_all(&running.runtime).unwrap();
    let _running = Pidfile::acquire(&running.pidfile()).unwrap();

    let mut second = host.serve();
    let client = second.connect().await;
    client.hello("test").await.unwrap();

    let listed = client.call("session.list", None).await.unwrap()["sessions"].clone();
    let order: Vec<Value> = listed
        .as_array()
        .unwrap()
        .iter()
        .map(|s| json!([s["id"], s["seq"]]))
        .collect();
    assert_eq!(
        order,
        vec![
            json!([ours[0]["id"], 1]),
            json!([ours[1]["id"], 2]),
            json!(["old-a", 3]),
            json!(["old-b", 4]),
        ]
    );
    assert_eq!(listed[3]["size"], json!({ "cols": 100, "rows": 30 }));
    assert!(!older_file.exists(), "the adopted file is deleted");
    assert!(
        running_file.exists(),
        "a running version keeps its sessions"
    );

    second.stop(&client, Stop::Sigterm).await;
    let saved: Value =
        serde_json::from_slice(&std::fs::read(host.sessions_file()).unwrap()).unwrap();
    assert_eq!(
        saved["sessions"][3]["agent"],
        json!({ "agentSession": "a-1", "cwd": root.path() }),
        "the agent link is kept for the agent adapter"
    );
}

#[tokio::test]
async fn a_newer_schema_is_left_alone_and_an_older_one_is_read() {
    let host = Host::new();
    let root = tempfile::tempdir().unwrap();
    let newer = json!({ "version": 999, "sessions": [], "tabs": [{ "id": "x" }] });
    let file = write_sessions(&host.dirs, newer.clone());

    let mut first = host.serve();
    let client = first.connect().await;
    client.hello("test").await.unwrap();
    assert_eq!(
        client.call("session.list", None).await.unwrap()["sessions"],
        json!([])
    );
    open(&client, json!({ "root": root.path() })).await;
    first.stop(&client, Stop::Sigterm).await;
    let kept: Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
    assert_eq!(kept, newer, "an older daemon never rewrites a newer file");

    // Version 1 as the first daemon to persist wrote it: only required fields.
    write_sessions(
        &host.dirs,
        json!({ "version": 1, "sessions": [
            { "id": "v1", "seq": 1, "root": root.path(), "size": { "cols": 80, "rows": 24 } },
        ]}),
    );
    let mut second = host.serve();
    let client = second.connect().await;
    client.hello("test").await.unwrap();
    let listed = client.call("session.list", None).await.unwrap()["sessions"].clone();
    assert_eq!(listed[0]["id"], "v1");
    assert_eq!(listed[0]["status"], json!({ "state": "running" }));
}

async fn wait_for_status(client: &Client, index: usize, status: Value) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let listed = client.call("session.list", None).await.unwrap();
        if listed["sessions"][index]["status"] == status {
            return;
        }
        assert!(Instant::now() < deadline, "no {status} in {listed}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
