mod support;

use bitplane_sessiond::dev::{self, DETACH};
use bitplane_sessiond::session::Sessions;
use serde_json::{Value, json};
use support::{TestDaemon, contains, shell_env};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::test]
async fn attach_shows_a_real_session_and_types_into_it() {
    let home = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let daemon = TestDaemon::start_with_env(vec![Sessions::module()], shell_env(home.path())).await;
    let mut client = daemon.connect_hello().await;
    let opened = client
        .call("session.open", Some(json!({ "root": root.path() })))
        .await
        .unwrap();
    let session = opened["session"]["id"].as_str().unwrap().to_string();
    let (mut keyboard, input) = tokio::io::duplex(64);
    let (output, mut screen) = tokio::io::duplex(1 << 20);
    let (resize, resizes) = tokio::sync::mpsc::unbounded_channel();

    let attach = tokio::spawn(async move {
        dev::attach(&mut client, &session, (100, 30), resizes, input, output).await
    });
    let mut shown = Vec::new();
    keyboard.write_all(b"echo typed-$((6*7))\r").await.unwrap();
    read_until(&mut screen, &mut shown, b"typed-42").await;
    resize.send((132, 43)).unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    keyboard.write_all(b"stty size\r").await.unwrap();
    read_until(&mut screen, &mut shown, b"43 132").await;
    keyboard.write_all(&[DETACH]).await.unwrap();

    attach.await.unwrap().unwrap();
}

#[tokio::test]
async fn events_prints_one_json_line_per_event() {
    let home = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let daemon = TestDaemon::start_with_env(vec![Sessions::module()], shell_env(home.path())).await;
    let mut listener = daemon.connect_hello().await;
    let caller = daemon.connect_hello().await;
    let (out, mut lines) = tokio::io::duplex(4096);
    let printer = tokio::spawn(async move { dev::events(&mut listener, out).await });

    caller
        .call("session.open", Some(json!({ "root": root.path() })))
        .await
        .unwrap();

    let mut shown = Vec::new();
    read_until(&mut lines, &mut shown, b"\n").await;
    let line = shown.split(|&byte| byte == b'\n').next().unwrap();
    let event: Value = serde_json::from_slice(line).unwrap();
    assert_eq!(event["method"], "session.changed");
    assert_eq!(
        event["params"]["session"]["root"],
        root.path().display().to_string()
    );
    printer.abort();
}

async fn read_until(reader: &mut (impl AsyncReadExt + Unpin), seen: &mut Vec<u8>, end: &[u8]) {
    let mut buf = [0u8; 4096];
    while !contains(seen, end) {
        let read = tokio::time::timeout(std::time::Duration::from_secs(10), reader.read(&mut buf))
            .await
            .expect("output arrives")
            .unwrap();
        assert!(
            read > 0,
            "stream ended before {:?}",
            String::from_utf8_lossy(end)
        );
        seen.extend_from_slice(&buf[..read]);
    }
}
