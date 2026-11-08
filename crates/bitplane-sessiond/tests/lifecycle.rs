mod support;

use serde_json::json;
use support::{Screen, next_event, open, sessions_daemon};

#[tokio::test]
async fn restart_spawns_a_new_shell_under_the_same_id_and_offsets_keep_counting() {
    let (_home, daemon) = sessions_daemon().await;
    let mut client = daemon.connect_hello().await;
    let root = tempfile::tempdir().unwrap();
    let session = open(&client, json!({ "root": root.path() })).await;
    let id = session["id"].as_str().unwrap();
    let mut screen = Screen::attach(&mut client, id).await;
    screen.type_text(&client, "echo $$ > first; echo old-$((6*7))\r");
    screen.wait_for(&mut client, "old-42").await;
    screen.drain(&mut client).await;
    let before = screen.next_offset;

    let restarted = client
        .call("session.restart", Some(json!({ "session": id })))
        .await
        .unwrap();

    assert_eq!(restarted["session"]["id"], id);
    assert_eq!(restarted["session"]["seq"], session["seq"]);
    assert_eq!(
        restarted["session"]["status"],
        json!({ "state": "running" })
    );
    screen.type_text(&client, "echo $$ > second; echo new-$((6*7))\r");
    screen.wait_for(&mut client, "new-42").await;
    assert_eq!(screen.resnapshots, 1, "the restart resets the terminal");
    assert!(screen.snapshot_offset >= before, "offsets keep counting");
    assert!(!screen.text().contains("old-42"));
    let first = std::fs::read_to_string(root.path().join("first")).unwrap();
    let second = std::fs::read_to_string(root.path().join("second")).unwrap();
    assert_ne!(first, second, "a new shell process");
}

#[tokio::test]
async fn an_exited_shell_stays_listed_with_its_code_until_restarted() {
    let (_home, daemon) = sessions_daemon().await;
    let mut client = daemon.connect_hello().await;
    let root = tempfile::tempdir().unwrap();
    let session = open(&client, json!({ "root": root.path() })).await;
    let id = session["id"].as_str().unwrap();
    let screen = Screen::attach(&mut client, id).await;
    screen.type_text(&client, "exit 0\r");
    changed_until(&mut client, |s| {
        s["status"] == json!({ "state": "exited", "code": 0 })
    })
    .await;

    let listed = client.call("session.list", None).await.unwrap();
    assert_eq!(listed["sessions"][0]["id"], id);
    assert_eq!(
        listed["sessions"][0]["status"],
        json!({ "state": "exited", "code": 0 })
    );
    let busy = client
        .call("session.busy", Some(json!({ "session": id })))
        .await
        .unwrap();
    assert_eq!(busy, json!({ "busy": false }));

    let restarted = client
        .call("session.restart", Some(json!({ "session": id })))
        .await
        .unwrap();
    assert_eq!(
        restarted["session"]["status"],
        json!({ "state": "running" })
    );
}

#[tokio::test]
async fn a_shell_that_cannot_start_is_reported_on_open_and_on_restart() {
    let (_home, daemon) = sessions_daemon().await;
    let client = daemon.connect_hello().await;
    let root = tempfile::tempdir().unwrap();
    let session = open(
        &client,
        json!({ "root": root.path(), "env": { "SHELL": "/nonexistent/shell" } }),
    )
    .await;
    assert_eq!(session["status"]["state"], "spawnFailed");
    assert!(
        session["status"]["error"]
            .as_str()
            .unwrap()
            .contains("/nonexistent/shell")
    );

    let restarted = client
        .call("session.restart", Some(json!({ "session": session["id"] })))
        .await
        .unwrap();
    assert_eq!(restarted["session"]["status"]["state"], "spawnFailed");
}

#[tokio::test]
async fn busy_reports_the_foreground_process_when_it_is_not_the_shell() {
    let (_home, daemon) = sessions_daemon().await;
    let mut client = daemon.connect_hello().await;
    let root = tempfile::tempdir().unwrap();
    let session = open(&client, json!({ "root": root.path() })).await;
    let id = session["id"].as_str().unwrap();
    let mut screen = Screen::attach(&mut client, id).await;
    screen.type_text(&client, "echo idle-$((6*7))\r");
    screen.wait_for(&mut client, "idle-42").await;
    let idle = client
        .call("session.busy", Some(json!({ "session": id })))
        .await
        .unwrap();
    assert_eq!(idle, json!({ "busy": false }));
    let shell = changed_until(&mut client, |s| s["foreground"].is_string()).await;
    let shell = shell["foreground"].as_str().unwrap().to_owned();
    assert_ne!(shell, "sleep");

    screen.type_text(&client, "sleep 30\r");
    let changed = changed_until(&mut client, |s| s["foreground"] == "sleep").await;
    assert_eq!(changed["id"], id);
    let busy = client
        .call("session.busy", Some(json!({ "session": id })))
        .await
        .unwrap();
    assert_eq!(busy, json!({ "busy": true, "processName": "sleep" }));

    screen.type_text(&client, "\x03");
    changed_until(&mut client, |s| s["foreground"] == shell.as_str()).await;
}

#[tokio::test]
async fn rename_sets_a_user_name_apart_from_the_programs_title() {
    let (_home, daemon) = sessions_daemon().await;
    let mut watcher = daemon.connect_hello().await;
    let mut client = daemon.connect_hello().await;
    let root = tempfile::tempdir().unwrap();
    let session = open(&client, json!({ "root": root.path(), "name": "api" })).await;
    let id = session["id"].as_str().unwrap();
    assert_eq!(session["name"], "api");

    let renamed = client
        .call(
            "session.rename",
            Some(json!({ "session": id, "name": "web" })),
        )
        .await
        .unwrap();
    assert_eq!(renamed["session"]["name"], "web");
    changed_until(&mut watcher, |s| s["name"] == "web").await;

    let screen = Screen::attach(&mut client, id).await;
    screen.type_text(&client, "printf '\\033]2;%s\\007' \"building $((6*7))\"\r");
    let titled = changed_until(&mut watcher, |s| s["title"] == "building 42").await;
    assert_eq!(
        titled["name"], "web",
        "a program's title never replaces the name"
    );

    let cleared = client
        .call(
            "session.rename",
            Some(json!({ "session": id, "name": "  " })),
        )
        .await
        .unwrap();
    assert!(cleared["session"].get("name").is_none());
    assert_eq!(cleared["session"]["title"], "building 42");
}

#[tokio::test]
async fn a_gone_root_keeps_the_shell_running_and_refuses_restart_until_it_is_back() {
    let (_home, daemon) = sessions_daemon().await;
    let mut client = daemon.connect_hello().await;
    let parent = tempfile::tempdir().unwrap();
    let root = parent.path().join("plane");
    std::fs::create_dir(&root).unwrap();
    let session = open(&client, json!({ "root": root })).await;
    let id = session["id"].as_str().unwrap();
    assert_eq!(session["rootGone"], false);
    let mut screen = Screen::attach(&mut client, id).await;

    std::fs::rename(&root, parent.path().join("moved")).unwrap();
    let gone = changed_until(&mut client, |s| s["rootGone"] == true).await;
    assert_eq!(gone["status"], json!({ "state": "running" }));
    screen.type_text(&client, "echo alive-$((6*7))\r");
    screen.wait_for(&mut client, "alive-42").await;
    let refused = client
        .call("session.restart", Some(json!({ "session": id })))
        .await
        .unwrap_err();
    assert!(refused.to_string().contains("RootGone"), "{refused}");

    std::fs::create_dir(&root).unwrap();
    changed_until(&mut client, |s| s["rootGone"] == false).await;
    let restarted = client
        .call("session.restart", Some(json!({ "session": id })))
        .await
        .unwrap();
    assert_eq!(
        restarted["session"]["status"],
        json!({ "state": "running" })
    );
}

#[tokio::test]
async fn restart_of_an_unknown_session_is_refused() {
    let (_home, daemon) = sessions_daemon().await;
    let client = daemon.connect_hello().await;
    for method in ["session.restart", "session.busy", "session.rename"] {
        let error = client
            .call(method, Some(json!({ "session": "nope" })))
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("UnknownSession"),
            "{method}: {error}"
        );
    }
}

/// The session of the next `session.changed` that matches.
async fn changed_until(
    client: &mut bitplane_sessiond::client::Client,
    done: impl Fn(&serde_json::Value) -> bool,
) -> serde_json::Value {
    loop {
        let changed = next_event(client, "session.changed").await;
        if done(&changed["session"]) {
            return changed["session"].clone();
        }
    }
}
