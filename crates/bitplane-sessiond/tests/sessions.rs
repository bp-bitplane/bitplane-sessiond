mod support;

use bitplane_sessiond::client::Client;
use bitplane_sessiond::session::Sessions;
use serde_json::{Value, json};
use support::{Screen, TestDaemon, shell_env};

#[tokio::test]
async fn open_starts_a_shell_in_the_root_with_the_session_env() {
    let home = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let mut env = shell_env(home.path());
    env.insert("CLAUDECODE".into(), "1".into());
    env.insert("BITPLANE_SESSION_ID".into(), "inherited".into());
    env.insert("FROM_LOGIN".into(), "login".into());
    let daemon = TestDaemon::start_with_env(vec![Sessions::module()], env).await;
    let mut client = daemon.connect_hello().await;

    let session = open(
        &client,
        json!({ "root": root.path(), "plane": "web", "env": { "EXTRA": "x" } }),
    )
    .await;
    let id = session["id"].as_str().unwrap();
    let mut screen = Screen::attach(&mut client, id).await;
    screen.type_text(
        &client,
        "printf '<%s|%s|%s|%s|%s|%s|%s|%s>\\n' \"$(pwd -P)\" \"$BITPLANE_SESSION_ID\" \
         \"$BITPLANE_PLANE_ID\" \"$BITPLANE_PLANE_DIR\" \"$TERM\" \"${CLAUDECODE-unset}\" \
         \"$EXTRA\" \"$FROM_LOGIN\"\r",
    );

    let root = root.path().canonicalize().unwrap();
    let expected = format!(
        "<{root}|{id}|web|{plain}|xterm-256color|unset|x|login>",
        root = root.display(),
        plain = session["root"].as_str().unwrap(),
    );
    screen.wait_for(&mut client, &expected).await;
    assert!(uuid::Uuid::parse_str(id).is_ok(), "{id} is a UUID");
    assert_eq!(session["plane"], "web");
    assert_eq!(session["status"], json!({ "state": "running" }));
}

async fn open(client: &Client, params: Value) -> Value {
    let result = client.call("session.open", Some(params)).await.unwrap();
    result["session"].clone()
}

#[tokio::test]
async fn sessions_are_listed_in_open_order_and_broadcast_when_they_change() {
    let (_home, daemon) = sessions_daemon().await;
    let mut watcher = daemon.connect_hello().await;
    let client = daemon.connect_hello().await;
    let root = tempfile::tempdir().unwrap();

    let first = open(&client, json!({ "root": root.path() })).await;
    let second = open(&client, json!({ "root": root.path(), "plane": "web" })).await;

    let changed = next_event(&mut watcher, "session.changed").await;
    assert_eq!(changed["session"]["id"], first["id"]);
    let listed = client.call("session.list", None).await.unwrap();
    let ids: Vec<&Value> = listed["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|session| &session["id"])
        .collect();
    assert_eq!(ids, [&first["id"], &second["id"]]);
    assert!(first["seq"].as_u64() < second["seq"].as_u64());
}

#[tokio::test]
async fn close_hangs_up_the_shell_and_removes_the_session() {
    let (_home, daemon) = sessions_daemon().await;
    let mut watcher = daemon.connect_hello().await;
    let mut client = daemon.connect_hello().await;
    let root = tempfile::tempdir().unwrap();
    let marker = root.path().join("hup");
    let session = open(&client, json!({ "root": root.path() })).await;
    let id = session["id"].as_str().unwrap();
    let mut screen = Screen::attach(&mut client, id).await;
    screen.type_text(
        &client,
        "trap 'echo got > hup; exit 0' HUP; echo trap-$((6*7))\r",
    );
    screen.wait_for(&mut client, "trap-42").await;

    client
        .call("session.close", Some(json!({ "session": id })))
        .await
        .unwrap();

    let removed = next_event(&mut watcher, "session.removed").await;
    assert_eq!(removed["session"], id);
    let listed = client.call("session.list", None).await.unwrap();
    assert_eq!(listed["sessions"], json!([]));
    wait_until(|| std::fs::read_to_string(&marker).is_ok_and(|text| text == "got\n")).await;
}

#[tokio::test]
async fn attach_during_output_continues_the_stream_with_no_byte_lost_or_repeated() {
    let (_home, daemon) = sessions_daemon().await;
    let mut early = daemon.connect_hello().await;
    let mut late = daemon.connect_hello().await;
    let root = tempfile::tempdir().unwrap();
    let session = open(&early, json!({ "root": root.path() })).await;
    let id = session["id"].as_str().unwrap();
    let mut whole = Screen::attach(&mut early, id).await;
    whole.type_text(
        &early,
        "sleep 0.3; seq 1 60000 | sed 's/^/line /'; echo done-$((6*7))\r",
    );
    whole.wait_for(&mut early, "line 2000\r\n").await;

    let mut joined = Screen::attach(&mut late, id).await;
    let (_, _) = tokio::join!(
        whole.wait_for(&mut early, "done-42"),
        joined.wait_for(&mut late, "done-42"),
    );
    // Both streams end in the same place once the shell's prompt is back.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    whole.drain(&mut early).await;
    joined.drain(&mut late).await;

    assert!(joined.snapshot_offset > whole.snapshot_offset);
    assert!(!joined.text().starts_with("line 1\r\n"));
    let from = (joined.snapshot_offset - whole.snapshot_offset) as usize;
    assert_eq!(joined.next_offset, whole.next_offset);
    assert!(
        whole.output()[from..] == *joined.output(),
        "the late attachment's OUTPUT is the early one's from the snapshot offset on"
    );
}

#[tokio::test]
async fn a_paste_sent_as_one_input_frame_arrives_whole() {
    let (_home, daemon) = sessions_daemon().await;
    let mut client = daemon.connect_hello().await;
    let mut other = daemon.connect_hello().await;
    let root = tempfile::tempdir().unwrap();
    let session = open(&client, json!({ "root": root.path() })).await;
    let id = session["id"].as_str().unwrap();
    let mut screen = Screen::attach(&mut client, id).await;
    let typist = Screen::attach(&mut other, id).await;
    let paste: String = (0..3000).map(|i| format!("pasted line {i}\n")).collect();
    let typed = "x".repeat(20_000);
    // Raw, as a line editor or TUI reads a paste.
    screen.type_text(
        &client,
        &format!(
            "stty raw -echo; echo ready-$((6*7)); head -c {} > pasted; stty sane; echo saved-$((6*7))\r",
            paste.len() + typed.len()
        ),
    );
    screen.wait_for(&mut client, "ready-42").await;

    screen.type_text(&client, &paste);
    typist.type_text(&other, &typed);

    screen.wait_for(&mut client, "saved-42").await;
    let pasted = std::fs::read_to_string(root.path().join("pasted")).unwrap();
    assert!(
        pasted == format!("{paste}{typed}") || pasted == format!("{typed}{paste}"),
        "the paste and the other attachment's input interleaved"
    );
}

#[tokio::test]
async fn output_keeps_flowing_with_no_attachment_and_a_reattach_shows_it() {
    let (_home, daemon) = sessions_daemon().await;
    let mut client = daemon.connect_hello().await;
    let root = tempfile::tempdir().unwrap();
    let session = open(&client, json!({ "root": root.path() })).await;
    let id = session["id"].as_str().unwrap();
    let screen = Screen::attach(&mut client, id).await;
    screen.type_text(
        &client,
        "sleep 0.3; seq 1 30000 | sed 's/^/row /'; echo end-$((6*7))\r",
    );
    screen.close(&mut client).await;

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let again = Screen::attach(&mut client, id).await;
        let text = again.text();
        again.close(&mut client).await;
        if text.contains("row 30000") && text.contains("end-42") {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "never saw the end in {text:?}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

async fn sessions_daemon() -> (tempfile::TempDir, TestDaemon) {
    let home = tempfile::tempdir().unwrap();
    let daemon = TestDaemon::start_with_env(vec![Sessions::module()], shell_env(home.path())).await;
    (home, daemon)
}

async fn next_event(client: &mut Client, method: &str) -> Value {
    loop {
        let event = tokio::time::timeout(std::time::Duration::from_secs(10), client.next_event())
            .await
            .unwrap_or_else(|_| panic!("no {method} event"))
            .expect("connection open");
        if event.method == method {
            return event.params.unwrap_or_default();
        }
    }
}

async fn wait_until(done: impl Fn() -> bool) {
    for _ in 0..500 {
        if done() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("timed out");
}
