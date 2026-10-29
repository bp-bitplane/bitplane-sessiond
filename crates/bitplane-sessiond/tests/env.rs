mod support;

use std::sync::{Arc, OnceLock};

use bitplane_protocol::RpcError;
use bitplane_sessiond::env::EnvMap;
use bitplane_sessiond::module::{Availability, BoxFuture, Call, Module, ModuleContext};
use serde_json::Value;
use support::TestDaemon;

#[tokio::test]
async fn modules_probe_and_run_with_the_captured_env() {
    let env = EnvMap::from([("FROM_PROFILE".into(), "yes".into())]);
    let daemon = TestDaemon::start_with_env(vec![Arc::new(EnvReader::default())], env).await;
    let client = daemon.connect_hello().await;

    let value = client
        .call("env.get", Some(Value::from("FROM_PROFILE")))
        .await
        .unwrap();

    assert_eq!(value, "yes");
}

#[tokio::test]
async fn a_probe_that_finds_nothing_on_the_env_makes_the_module_unavailable() {
    let daemon =
        TestDaemon::start_with_env(vec![Arc::new(EnvReader::default())], EnvMap::new()).await;

    let hello = daemon.connect().await.hello("test").await.unwrap();

    assert!(!hello.capabilities[0].available);
}

/// Available only when the probe sees `FROM_PROFILE`; `env.get` reads the
/// env its context was started with.
#[derive(Default)]
struct EnvReader {
    env: OnceLock<Arc<EnvMap>>,
}

impl Module for EnvReader {
    fn namespace(&self) -> &'static str {
        "env"
    }

    fn probe(&self, env: &EnvMap) -> Availability {
        match env.contains_key("FROM_PROFILE") {
            true => Availability::Available { info: None },
            false => Availability::Unavailable {
                reason: "FROM_PROFILE is not set".into(),
            },
        }
    }

    fn start(&self, context: ModuleContext) {
        let _ = self.env.set(context.env);
    }

    fn handle(&self, call: Call) -> BoxFuture<'_, Result<Value, RpcError>> {
        Box::pin(async move {
            let key = call
                .params
                .as_ref()
                .and_then(Value::as_str)
                .unwrap_or_default();
            Ok(self.env.get().and_then(|env| env.get(key)).cloned().into())
        })
    }
}
