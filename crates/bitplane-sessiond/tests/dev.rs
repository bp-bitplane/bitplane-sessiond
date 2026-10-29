mod support;

use std::sync::{Arc, OnceLock};

use bitplane_protocol::{Frame, RpcError};
use bitplane_sessiond::dev::{self, DETACH};
use bitplane_sessiond::env::EnvMap;
use bitplane_sessiond::module::{Availability, BoxFuture, Call, Connection, Module, ModuleContext};
use serde_json::{Value, json};
use support::TestDaemon;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::test]
async fn attach_shows_the_snapshot_then_echoes_typed_input() {
    let daemon = TestDaemon::start(vec![Arc::new(StubSessions::default())]).await;
    let mut client = daemon.connect_hello().await;
    let (mut keyboard, input) = tokio::io::duplex(64);
    let (output, mut screen) = tokio::io::duplex(4096);

    let attach =
        tokio::spawn(async move { dev::attach(&mut client, "s1", (80, 24), input, output).await });
    let mut shown = Vec::new();
    read_until(&mut screen, &mut shown, b"$ ").await;
    keyboard.write_all(b"ls\r").await.unwrap();
    read_until(&mut screen, &mut shown, b"ls\r").await;
    keyboard.write_all(&[DETACH]).await.unwrap();

    attach.await.unwrap().unwrap();
    assert_eq!(shown, b"s1 at 80x24\r\n$ ls\r");
}

#[tokio::test]
async fn events_prints_one_json_line_per_event() {
    let daemon = TestDaemon::start(vec![Arc::new(StubSessions::default())]).await;
    let mut listener = daemon.connect_hello().await;
    let caller = daemon.connect_hello().await;
    let (out, mut lines) = tokio::io::duplex(4096);
    let printer = tokio::spawn(async move { dev::events(&mut listener, out).await });

    caller
        .call("session.rename", Some(json!({ "name": "build" })))
        .await
        .unwrap();

    let mut shown = Vec::new();
    read_until(&mut lines, &mut shown, b"\n").await;
    let event: Value = serde_json::from_slice(&shown).unwrap();
    assert_eq!(event["method"], "session.changed");
    assert_eq!(event["params"]["name"], "build");
    printer.abort();
}

async fn read_until(reader: &mut (impl AsyncReadExt + Unpin), seen: &mut Vec<u8>, end: &[u8]) {
    let mut buf = [0u8; 256];
    while !seen.ends_with(end) {
        let read = tokio::time::timeout(std::time::Duration::from_secs(2), reader.read(&mut buf))
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

/// Stands in for the sessions module until it exists: one fake terminal per
/// attachment that echoes what is typed.
#[derive(Default)]
struct StubSessions {
    context: OnceLock<ModuleContext>,
}

impl Module for StubSessions {
    fn namespace(&self) -> &'static str {
        "session"
    }

    fn probe(&self, _env: &EnvMap) -> Availability {
        Availability::Available { info: None }
    }

    fn start(&self, context: ModuleContext) {
        let _ = self.context.set(context);
    }

    fn handle(&self, call: Call) -> BoxFuture<'_, Result<Value, RpcError>> {
        Box::pin(async move {
            let params = call.params.unwrap_or_default();
            match call.method.as_str() {
                "session.attachment.open" => {
                    let banner = format!(
                        "{} at {}x{}\r\n$ ",
                        params["session"].as_str().unwrap(),
                        params["cols"],
                        params["rows"]
                    );
                    let _ = call.connection.frames.send(Frame::Snapshot {
                        attachment: 1,
                        offset: 0,
                        data: banner.into_bytes(),
                    });
                    Ok(json!({ "attachment": 1, "session": params["session"] }))
                }
                "session.attachment.close" => Ok(Value::Null),
                "session.rename" => {
                    self.context
                        .get()
                        .unwrap()
                        .events
                        .emit("session.changed", params);
                    Ok(Value::Null)
                }
                _ => Err(RpcError::new(RpcError::METHOD_NOT_FOUND, call.method)),
            }
        })
    }

    fn input(&self, connection: &Connection, attachment: u32, data: Vec<u8>) {
        let _ = connection.frames.send(Frame::Output {
            attachment,
            offset: 0,
            data,
        });
    }
}
