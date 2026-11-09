mod support;

use std::time::Duration;

use bitplane_sessiond::client::Client;
use serde_json::{Value, json};
use support::{Screen, next_event, open, paced_lines, sessions_daemon};

#[tokio::test]
async fn two_attachments_see_the_same_terminal_and_both_can_type() {
    let (_home, daemon) = sessions_daemon().await;
    let mut alice = daemon.connect_hello().await;
    let mut bob = daemon.connect_hello().await;
    let root = tempfile::tempdir().unwrap();
    let session = open(&alice, json!({ "root": root.path() })).await;
    let id = session["id"].as_str().unwrap();
    let mut left = Screen::attach(&mut alice, id).await;
    let mut right = Screen::attach(&mut bob, id).await;

    left.type_text(&alice, "echo from-$((1+1))-alice\r");
    right.wait_for(&mut bob, "from-2-alice").await;
    right.type_text(&bob, "echo from-$((2+2))-bob\r");
    left.wait_for(&mut alice, "from-4-bob").await;
    right.wait_for(&mut bob, "from-4-bob").await;
}

#[tokio::test]
async fn the_size_follows_the_attachment_that_last_typed_or_resized() {
    let (_home, daemon) = sessions_daemon().await;
    let mut alice = daemon.connect_hello().await;
    let mut bob = daemon.connect_hello().await;
    let mut carol = daemon.connect_hello().await;
    let root = tempfile::tempdir().unwrap();
    let session = open(&alice, json!({ "root": root.path() })).await;
    let id = session["id"].as_str().unwrap();
    let mut left = Screen::attach_with(&mut alice, sized(id, 100, 30)).await;
    let mut right = Screen::attach_with(&mut bob, sized(id, 120, 40)).await;
    assert_eq!(size_of(&alice, id).await, json!({ "cols": 80, "rows": 24 }));

    left.type_text(&alice, SHOW_SIZE);
    left.wait_for(&mut alice, "size-30x100").await;
    assert_eq!(
        size_of(&alice, id).await,
        json!({ "cols": 100, "rows": 30 })
    );

    resize(&bob, right.attachment, 132, 43).await;
    assert_eq!(
        size_of(&alice, id).await,
        json!({ "cols": 132, "rows": 43 })
    );
    right.type_text(&bob, SHOW_SIZE);
    right.wait_for(&mut bob, "size-43x132").await;

    let mut params = sized(id, 200, 50);
    params["ignoreSize"] = json!(true);
    let mut watcher = Screen::attach_with(&mut carol, params).await;
    resize(&carol, watcher.attachment, 210, 55).await;
    watcher.type_text(&carol, SHOW_SIZE);
    watcher.wait_for(&mut carol, "size-43x132").await;
    assert_eq!(
        size_of(&alice, id).await,
        json!({ "cols": 132, "rows": 43 })
    );
}

#[tokio::test]
async fn a_read_only_attachment_gets_read_only_when_it_types() {
    let (_home, daemon) = sessions_daemon().await;
    let mut typist = daemon.connect_hello().await;
    let mut observer = daemon.connect_hello().await;
    let root = tempfile::tempdir().unwrap();
    let session = open(&typist, json!({ "root": root.path() })).await;
    let id = session["id"].as_str().unwrap();
    let screen = Screen::attach(&mut typist, id).await;
    let mut params = sized(id, 80, 24);
    params["readOnly"] = json!(true);
    let mut watching = Screen::attach_with(&mut observer, params).await;

    watching.type_text(&observer, "touch observed\r");
    let error = next_event(&mut observer, "session.attachment.error").await;
    assert_eq!(error["attachment"], watching.attachment);
    assert_eq!(error["code"], "READ_ONLY");

    screen.type_text(&typist, "echo typed-$((6*7))\r");
    watching.wait_for(&mut observer, "typed-42").await;
    assert!(!root.path().join("observed").exists());
}

#[tokio::test]
async fn typing_into_an_exited_session_gets_session_exited() {
    let (_home, daemon) = sessions_daemon().await;
    let mut client = daemon.connect_hello().await;
    let root = tempfile::tempdir().unwrap();
    let session = open(&client, json!({ "root": root.path() })).await;
    let id = session["id"].as_str().unwrap();
    let screen = Screen::attach(&mut client, id).await;

    screen.type_text(&client, "exit 3\r");
    loop {
        let changed = next_event(&mut client, "session.changed").await;
        if changed["session"]["status"] == json!({ "state": "exited", "code": 3 }) {
            break;
        }
    }
    screen.type_text(&client, "echo too late\r");
    let error = next_event(&mut client, "session.attachment.error").await;
    assert_eq!(error["attachment"], screen.attachment);
    assert_eq!(error["code"], "SESSION_EXITED");
}

#[tokio::test]
async fn a_stalled_attachment_is_dropped_and_resnapshotted_once_it_acks() {
    let (_home, daemon) = sessions_daemon().await;
    let mut alice = daemon.connect_hello().await;
    let mut bob = daemon.connect_hello().await;
    let root = tempfile::tempdir().unwrap();
    let session = open(&alice, json!({ "root": root.path() })).await;
    let id = session["id"].as_str().unwrap();
    let mut awake = Screen::attach(&mut alice, id).await;
    let mut asleep = Screen::attach(&mut bob, id).await;
    asleep.stall();

    awake.type_text(
        &alice,
        &format!("{}; echo end-$((6*7))\r", paced_lines("b", 40)),
    );
    awake.wait_for(&mut alice, "end-42").await;
    awake.drain(&mut alice).await;
    asleep.drain(&mut bob).await;
    assert_eq!(awake.resnapshots, 0, "the other attachment is unaffected");
    assert!(awake.text().contains("b40 1500\r\n"));
    assert!(
        asleep.output().len() <= 128 * 1024,
        "a stalled attachment gets no more than its window"
    );
    assert_eq!(asleep.resnapshots, 0);

    asleep.resume(&bob);
    asleep.wait_for(&mut bob, "end-42").await;
    assert_eq!(asleep.resnapshots, 1);
    assert_eq!(asleep.snapshot_offset, awake.next_offset);

    asleep.type_text(&bob, "echo back-$((6*7))\r");
    asleep.wait_for(&mut bob, "back-42").await;
    awake.wait_for(&mut alice, "back-42").await;
}

#[tokio::test]
async fn closing_or_dropping_an_attachment_frees_it() {
    let (_home, daemon) = sessions_daemon().await;
    let mut client = daemon.connect_hello().await;
    let mut other = daemon.connect_hello().await;
    let root = tempfile::tempdir().unwrap();
    let session = open(&client, json!({ "root": root.path() })).await;
    let id = session["id"].as_str().unwrap();
    let mut screen = Screen::attach(&mut client, id).await;
    Screen::attach(&mut other, id).await;
    assert_eq!(attachments_of(&client, id).await, 2);
    screen.type_text(&client, &format!("{}\r", paced_lines("c", 20)));
    screen.wait_for(&mut client, "c2 1\r\n").await;

    let closing = json!({ "attachment": screen.attachment });
    client
        .call("session.attachment.close", Some(closing.clone()))
        .await
        .unwrap();
    // What was sent before the reply is already read; nothing may follow it.
    while client.streams().1.try_recv().is_ok() {}
    let late = tokio::time::timeout(Duration::from_millis(300), client.next_frame()).await;
    assert!(late.is_err(), "a frame after the close: {late:?}");
    assert_eq!(attachments_of(&client, id).await, 1);
    let again = client
        .call("session.attachment.close", Some(closing))
        .await
        .unwrap_err();
    assert!(again.to_string().contains("UnknownAttachment"), "{again}");
    screen.type_text(&client, "echo ghost\r");
    let error = next_event(&mut client, "session.attachment.error").await;
    assert_eq!(error["code"], "UNKNOWN_ATTACHMENT");

    drop(other);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while attachments_of(&client, id).await != 0 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the dropped attachment stayed"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

const SHOW_SIZE: &str = "echo size-$(stty size | tr ' ' x)\r";

fn sized(session: &str, cols: u16, rows: u16) -> Value {
    json!({ "session": session, "cols": cols, "rows": rows })
}

async fn resize(client: &Client, attachment: u32, cols: u16, rows: u16) {
    client
        .call(
            "session.attachment.resize",
            Some(json!({ "attachment": attachment, "cols": cols, "rows": rows })),
        )
        .await
        .unwrap();
}

async fn size_of(client: &Client, session: &str) -> Value {
    let listed = client.call("session.list", None).await.unwrap();
    listed["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|listed| listed["id"] == session)
        .expect("the session is listed")["size"]
        .clone()
}

async fn attachments_of(client: &Client, session: &str) -> u64 {
    let listed = client.call("session.list", None).await.unwrap();
    listed["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|listed| listed["id"] == session)
        .expect("the session is listed")["attachments"]
        .as_u64()
        .unwrap()
}
