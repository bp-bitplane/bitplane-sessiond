mod support;

use bitplane_protocol::{
    ClientInfo, ErrorCode, HelloParams, Id, PROTOCOL_VERSION, Request, RpcError, rpc::Outcome,
};
use bitplane_sessiond::client::ClientError;
use bitplane_sessiond::module::Availability;
use std::sync::Arc;
use support::{Echo, TestDaemon};

#[tokio::test]
async fn hello_negotiates_and_describes_the_daemon() {
    let missing = Arc::new(Echo {
        namespace: "git",
        availability: Availability::Unavailable {
            reason: "git not found on PATH; install it and restart the daemon".into(),
        },
        dependencies: &[],
    });
    let daemon = TestDaemon::start(vec![Echo::available("fs"), missing]).await;

    let hello = daemon.connect().await.hello("test").await.unwrap();

    assert_eq!(hello.protocol_version, PROTOCOL_VERSION);
    assert_eq!(hello.daemon.name, "bitplane-sessiond");
    assert!(!hello.daemon.version.is_empty());
    let fs = &hello.capabilities[0];
    assert_eq!(
        (fs.module.as_str(), fs.available, fs.reason.as_deref()),
        ("fs", true, None)
    );
    let git = &hello.capabilities[1];
    assert!(!git.available);
    assert!(git.reason.as_deref().unwrap().contains("install it"));
}

#[tokio::test]
async fn each_daemon_process_has_its_own_instance() {
    let first = TestDaemon::start(vec![]).await;
    let second = TestDaemon::start(vec![]).await;

    let a = first
        .connect()
        .await
        .hello("t")
        .await
        .unwrap()
        .daemon
        .instance;
    let b = first
        .connect()
        .await
        .hello("t")
        .await
        .unwrap()
        .daemon
        .instance;
    let c = second
        .connect()
        .await
        .hello("t")
        .await
        .unwrap()
        .daemon
        .instance;

    assert_eq!(a, b);
    assert_ne!(a, c);
}

#[tokio::test]
async fn an_incompatible_client_is_refused_and_disconnected() {
    let daemon = TestDaemon::start(vec![]).await;
    let client = daemon.connect().await;

    let params = HelloParams {
        protocol_version: PROTOCOL_VERSION + 2,
        min_protocol_version: PROTOCOL_VERSION + 1,
        client: ClientInfo {
            name: "future".into(),
            version: "9".into(),
            via: None,
        },
    };
    let error = client
        .call("hello", Some(serde_json::to_value(params).unwrap()))
        .await;

    let error = expect_rpc(error);
    assert_eq!(error.protocol_code(), Some(ErrorCode::VersionMismatch));
    assert_eq!(
        error.data.unwrap().detail.unwrap()["protocolVersion"],
        PROTOCOL_VERSION
    );
    assert!(matches!(
        client.call("fs.echo", None).await,
        Err(ClientError::Closed)
    ));
}

#[tokio::test]
async fn a_first_message_other_than_hello_is_refused() {
    let daemon = TestDaemon::start(vec![Echo::available("fs")]).await;
    let client = daemon.connect().await;

    let error = expect_rpc(client.call("fs.echo", None).await);

    assert_eq!(error.protocol_code(), Some(ErrorCode::HelloRequired));
    assert!(matches!(
        client.hello("late").await,
        Err(ClientError::Closed)
    ));
}

#[tokio::test]
async fn hello_is_answered_once() {
    let daemon = TestDaemon::start(vec![]).await;
    let client = daemon.connect_hello().await;

    let error = expect_rpc(client.hello("again").await.map(|_| ()));

    assert_eq!(error.protocol_code(), Some(ErrorCode::HelloRepeated));
}

#[tokio::test]
async fn unparseable_json_gets_a_parse_error_with_a_null_id() {
    let daemon = TestDaemon::start(vec![]).await;
    let client = daemon.connect_hello().await;
    client.send_frame(bitplane_protocol::Frame::Json(b"{not json".to_vec()));

    // The reply has no id to wait on; the next request still works.
    let response = client
        .request(Request::new(
            Id::String("after".into()),
            "nope.method",
            None,
        ))
        .await
        .unwrap();
    let Outcome::Error(error) = response.outcome else {
        panic!("expected an error")
    };
    assert_eq!(error.code, RpcError::METHOD_NOT_FOUND);
}

fn expect_rpc<T: std::fmt::Debug>(result: Result<T, ClientError>) -> RpcError {
    match result {
        Err(ClientError::Rpc(error)) => error,
        other => panic!("expected an RPC error, got {other:?}"),
    }
}
