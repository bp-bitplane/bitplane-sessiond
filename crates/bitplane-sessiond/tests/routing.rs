mod support;

use std::sync::Arc;
use std::time::Instant;

use bitplane_protocol::{ErrorCode, RpcError};
use bitplane_sessiond::client::ClientError;
use bitplane_sessiond::module::{Availability, BoxFuture, Call, Module, ModuleContext};
use serde_json::{Value, json};
use std::sync::OnceLock;
use support::{Echo, TestDaemon};

#[tokio::test]
async fn calls_reach_the_module_owning_the_namespace() {
    let daemon = TestDaemon::start(vec![
        Echo::available("agent"),
        Echo::available("agent.claude"),
    ])
    .await;
    let client = daemon.connect_hello().await;

    assert_eq!(
        client.call("agent.echo", Some(json!(1))).await.unwrap(),
        json!(1)
    );
    assert_eq!(
        client
            .call("agent.claude.echo", Some(json!(2)))
            .await
            .unwrap(),
        json!(2)
    );
}

#[tokio::test]
async fn an_unknown_namespace_is_method_not_found() {
    let daemon = TestDaemon::start(vec![]).await;
    let client = daemon.connect_hello().await;

    let Err(ClientError::Rpc(error)) = client.call("nothing.here", None).await else {
        panic!("expected an RPC error")
    };
    assert_eq!(error.code, RpcError::METHOD_NOT_FOUND);
}

#[tokio::test]
async fn an_unavailable_module_answers_module_unavailable() {
    let git = Arc::new(Echo {
        namespace: "git",
        availability: Availability::Unavailable {
            reason: "git not found on PATH".into(),
        },
        dependencies: &[],
    });
    let daemon = TestDaemon::start(vec![git]).await;
    let client = daemon.connect_hello().await;

    let Err(ClientError::Rpc(error)) = client.call("git.echo", None).await else {
        panic!("expected an RPC error")
    };
    assert_eq!(error.protocol_code(), Some(ErrorCode::ModuleUnavailable));
    assert!(error.message.contains("git not found on PATH"));
}

#[tokio::test]
async fn a_module_whose_dependency_is_unavailable_is_unavailable() {
    let sessions = Arc::new(Echo {
        namespace: "session",
        availability: Availability::Unavailable {
            reason: "no PTYs".into(),
        },
        dependencies: &[],
    });
    let agent = Arc::new(Echo {
        namespace: "agent.claude",
        availability: Availability::Available { info: None },
        dependencies: &["session"],
    });
    let orphan = Arc::new(Echo {
        namespace: "orphan",
        availability: Availability::Available { info: None },
        dependencies: &["missing"],
    });
    let daemon = TestDaemon::start(vec![sessions, agent, orphan]).await;

    let hello = daemon.connect().await.hello("t").await.unwrap();

    let reason = |module: &str| {
        let capability = hello
            .capabilities
            .iter()
            .find(|c| c.module == module)
            .unwrap();
        assert!(!capability.available, "{module} should be unavailable");
        capability.reason.clone().unwrap()
    };
    assert_eq!(
        reason("agent.claude"),
        "needs session, which is unavailable: no PTYs"
    );
    assert_eq!(reason("orphan"), "needs missing, which this daemon lacks");
}

#[tokio::test]
async fn requests_run_concurrently_and_replies_match_by_id() {
    let daemon = TestDaemon::start(vec![Echo::available("slow")]).await;
    let client = daemon.connect_hello().await;
    let started = Instant::now();

    let slow = client.call("slow.sleep", Some(json!(400)));
    let fast = async {
        let reply = client.call("slow.echo", Some(json!("fast"))).await;
        (reply, started.elapsed())
    };
    let (slow, (fast, fast_elapsed)) = tokio::join!(slow, fast);

    assert_eq!(slow.unwrap(), json!(400));
    assert_eq!(fast.unwrap(), json!("fast"));
    assert!(
        fast_elapsed.as_millis() < 300,
        "the fast reply waited behind the slow one"
    );
}

#[tokio::test]
async fn module_events_reach_every_helloed_connection() {
    let daemon = TestDaemon::start(vec![Arc::new(Announcer::default())]).await;
    let caller = daemon.connect_hello().await;
    let mut listener = daemon.connect_hello().await;

    caller
        .call("announce.now", Some(json!({ "n": 7 })))
        .await
        .unwrap();

    let event = listener.next_event().await.unwrap();
    assert_eq!(event.method, "announce.changed");
    assert_eq!(event.params, Some(json!({ "n": 7 })));
}

#[tokio::test]
async fn each_module_gets_its_own_state_dir() {
    let announcer = Arc::new(Announcer::default());
    let daemon = TestDaemon::start(vec![announcer.clone()]).await;

    let dir = announcer.context.get().unwrap().state_dir.clone();
    assert_eq!(dir, daemon.dirs.state.join("announce"));
    assert!(dir.is_dir());
}

#[derive(Default)]
struct Announcer {
    context: OnceLock<ModuleContext>,
}

impl Module for Announcer {
    fn namespace(&self) -> &'static str {
        "announce"
    }

    fn probe(&self) -> Availability {
        Availability::Available { info: None }
    }

    fn start(&self, context: ModuleContext) {
        let _ = self.context.set(context);
    }

    fn handle(&self, call: Call) -> BoxFuture<'_, Result<Value, RpcError>> {
        Box::pin(async move {
            let context = self.context.get().unwrap();
            context
                .events
                .emit("announce.changed", call.params.unwrap_or_default());
            Ok(Value::Null)
        })
    }
}
