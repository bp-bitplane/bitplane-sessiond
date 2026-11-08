mod support;

use bitplane_sessiond::client::ClientError;
use serde_json::json;
use support::{Echo, TestDaemon};

#[tokio::test]
async fn a_split_handle_calls_from_many_tasks_over_one_connection() {
    let daemon = TestDaemon::start(vec![Echo::available("echo")]).await;
    let (handle, _streams) = daemon.connect_hello().await.split();

    let calls = (0..4).map(|n| {
        let handle = handle.clone();
        tokio::spawn(async move { handle.call("echo.echo", Some(json!(n))).await.unwrap() })
    });
    let replies: Vec<_> = join_all(calls).await;

    assert_eq!(replies, vec![json!(0), json!(1), json!(2), json!(3)]);
}

#[tokio::test]
async fn the_streams_end_and_calls_fail_when_the_daemon_stops() {
    let mut daemon = TestDaemon::start(vec![Echo::available("echo")]).await;
    let (handle, mut streams) = daemon.connect_hello().await.split();

    daemon.stop().await.unwrap();

    assert!(streams.next_event().await.is_none());
    assert!(matches!(
        handle.call("echo.echo", None).await,
        Err(ClientError::Closed)
    ));
}

async fn join_all<T>(tasks: impl Iterator<Item = tokio::task::JoinHandle<T>>) -> Vec<T> {
    let mut out = Vec::new();
    for task in tasks {
        out.push(task.await.unwrap());
    }
    out
}
