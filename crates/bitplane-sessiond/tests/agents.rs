//! The `agent.claude` module against a real daemon, with a fake `claude` on
//! `PATH` that runs the hook commands it is given, as Claude Code would.

mod support;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use bitplane_protocol::ErrorCode;
use bitplane_sessiond::agent::Agents;
use bitplane_sessiond::client::{Client, ClientError};
use bitplane_sessiond::env::EnvMap;
use bitplane_sessiond::session::Sessions;
use serde_json::{Value, json};
use support::{Screen, TestDaemon, next_event, open, shell_env};

const DAEMON_EXE: &str = env!("CARGO_BIN_EXE_bitplane-sessiond");

/// Started through the wrapper, it reads the `--settings` it got, saves them
/// to `$FAKE_CLAUDE_LOG/settings.json`, and then runs the hook command with
/// the payload each typed line asks for. Lines end in `#<n>` and are
/// answered `done-<n>`, so every wait has its own marker.
const FAKE_CLAUDE: &str = r#"#!/bin/sh
log="$FAKE_CLAUDE_LOG"
echo "$*" >> "$log/calls"
settings=""
print=""
while [ $# -gt 0 ]; do
  case "$1" in
    --settings) settings="$2"; shift ;;
    -p) print=1 ;;
  esac
  shift
done
case "$settings" in
  "") json="" ;;
  "{"*) json="$settings" ;;
  *) json=$(cat "$settings") ;;
esac
[ -n "$json" ] && printf '%s' "$json" > "$log/settings.json"
hook=$(printf '%s' "$json" | tr ',{}[]' '\n\n\n\n\n' | sed -n 's/.*"command": *"\([^"]*\)".*/\1/p' | grep ' hook$' | head -n 1)
emit() {
  [ -n "$hook" ] || return 0
  printf '{"hook_event_name":"%s","session_id":"%s","cwd":"%s"%s}' "$1" "$sid" "$PWD" "$2" | sh -c "$hook"
}
if [ -n "$print" ]; then
  sid=nested
  emit SessionStart ',"source":"startup"'
  echo "printed"
  exit 0
fi
export CLAUDECODE=1
echo "fake-claude ready-$FAKE_RUN"
while IFS= read -r line; do
  set -- $line
  case "$1" in
    start) sid="$2"; emit SessionStart ",\"source\":\"$3\"" ;;
    prompt) emit UserPromptSubmit ',"prompt":"hi"' ;;
    permission) emit PermissionRequest ',"tool_name":"Bash"' ;;
    notify) emit Notification ",\"notification_type\":\"$2\",\"message\":\"m\"" ;;
    stop) emit Stop ',"stop_hook_active":false' ;;
    end) emit SessionEnd ",\"reason\":\"$2\""; exit 0 ;;
    nested) claude -p nested ;;
    quit) exit 0 ;;
  esac
  echo "done-${line##*#}"
done
"#;

#[tokio::test]
async fn hooks_drive_the_agent_through_its_states() {
    let mut fixture = Fixture::start().await;
    let (id, mut screen) = fixture.session().await;
    fixture.claude(&mut screen, "claude").await;

    fixture.say(&mut screen, "start a1 startup").await;
    let agent = fixture.changed().await;
    assert_eq!(
        agent,
        json!({
            "agentSession": "a1",
            "session": id,
            "cwd": fixture.root.path().canonicalize().unwrap(),
            "state": { "kind": "idle" },
            "tracking": "hooks",
        })
    );

    for (line, state) in [
        ("prompt", json!({ "kind": "busy" })),
        (
            "permission",
            json!({ "kind": "waiting", "reason": "permission" }),
        ),
        (
            "notify elicitation_dialog",
            json!({ "kind": "waiting", "reason": "input" }),
        ),
        ("notify idle_prompt", Value::Null),
        ("stop", json!({ "kind": "idle" })),
        (
            "notify permission_prompt",
            json!({ "kind": "waiting", "reason": "permission" }),
        ),
        ("end prompt_input_exit", json!({ "kind": "exited" })),
    ] {
        fixture.say(&mut screen, line).await;
        if !state.is_null() {
            assert_eq!(fixture.changed().await["state"], state, "after {line}");
        }
    }

    let listed = fixture.list().await;
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0]["state"], json!({ "kind": "exited" }));
    assert_eq!(listed[0]["session"], id);
    let settings = fixture.settings();
    for event in [
        "SessionStart",
        "UserPromptSubmit",
        "PermissionRequest",
        "Notification",
        "Stop",
        "StopFailure",
        "SessionEnd",
    ] {
        let hook = &settings["hooks"][event][0]["hooks"][0];
        assert_eq!(hook["type"], "command", "{event}");
        assert!(hook["timeout"].as_u64().unwrap() <= 5, "{event}");
    }
    assert!(!fixture.home.path().join(".claude").exists());
}

#[tokio::test]
async fn a_new_agent_session_replaces_the_old_one_but_compact_keeps_it() {
    let mut fixture = Fixture::start().await;
    let (id, mut screen) = fixture.session().await;
    fixture.claude(&mut screen, "claude").await;
    fixture.say(&mut screen, "start a1 startup").await;
    fixture.changed().await;

    fixture.say(&mut screen, "start a1 compact").await;
    assert_eq!(fixture.changed().await["agentSession"], "a1");

    for (previous, next, source) in [
        ("a1", "a2", "clear"),
        ("a2", "a3", "fork"),
        ("a3", "a4", "resume"),
    ] {
        fixture
            .say(&mut screen, &format!("start {next} {source}"))
            .await;
        assert_eq!(fixture.removed().await, previous, "{source}");
        let agent = fixture.changed().await;
        assert_eq!(
            (&agent["agentSession"], &agent["session"]),
            (&json!(next), &json!(id))
        );
    }
    let listed = fixture.list().await;
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0]["agentSession"], "a4");
}

#[tokio::test]
async fn a_users_settings_are_merged_and_a_nested_claude_is_ignored() {
    let mut fixture = Fixture::start().await;
    let (_id, mut screen) = fixture.session().await;
    fixture
        .claude(
            &mut screen,
            r#"claude --settings '{"model":"opus","hooks":{"Stop":[{"hooks":[{"type":"command","command":"true"}]}]}}'"#,
        )
        .await;

    let settings = fixture.settings();
    assert_eq!(settings["model"], "opus");
    let stop = settings["hooks"]["Stop"].as_array().unwrap();
    assert_eq!(stop.len(), 2);
    assert_eq!(stop[0]["hooks"][0]["command"], "true");
    assert!(settings["hooks"]["SessionStart"].is_array());
    fixture.say(&mut screen, "start a1 startup").await;
    assert_eq!(fixture.changed().await["agentSession"], "a1");

    fixture.say(&mut screen, "nested").await;
    fixture.say(&mut screen, "prompt").await;
    let agent = fixture.changed().await;
    assert_eq!(
        (&agent["agentSession"], &agent["state"]),
        (&json!("a1"), &json!({ "kind": "busy" }))
    );
    let calls = std::fs::read_to_string(fixture.log.join("calls")).unwrap();
    let nested = calls.lines().find(|call| call.starts_with("-p")).unwrap();
    assert!(!nested.contains("--settings"), "{calls}");

    fixture.say(&mut screen, "quit").await;
    let file = fixture.home.path().join("user.json");
    std::fs::write(&file, r#"{"env":{"FROM_FILE":"1"}}"#).unwrap();
    fixture
        .claude(
            &mut screen,
            &format!("claude --settings={}", file.display()),
        )
        .await;
    let settings = fixture.settings();
    assert_eq!(settings["env"]["FROM_FILE"], "1");
    assert!(settings["hooks"]["Stop"].is_array());
}

#[tokio::test]
async fn hooks_without_their_sessions_token_are_ignored() {
    let mut fixture = Fixture::start().await;
    let (id, mut screen) = fixture.session().await;

    for claude in [
        "BITPLANE_HOOK_TOKEN=forged claude",
        &format!("BITPLANE_SESSION_ID={} claude", uuid::Uuid::new_v4()),
    ] {
        fixture.claude(&mut screen, claude).await;
        fixture.say(&mut screen, "start forged startup").await;
        fixture.say(&mut screen, "quit").await;
    }
    let outside = Command::new(DAEMON_EXE)
        .arg("hook")
        .env_clear()
        .env("BITPLANE_HOOK_SOCKET", fixture.daemon.dirs.runtime.join("hook.sock"))
        .stdin(Stdio::piped())
        .spawn()
        .and_then(|mut hook| {
            use std::io::Write;
            hook.stdin
                .take()
                .unwrap()
                .write_all(br#"{"hook_event_name":"SessionStart","session_id":"outside","cwd":"/","source":"startup"}"#)?;
            hook.wait()
        })
        .unwrap();
    assert!(outside.success());

    fixture.claude(&mut screen, "claude").await;
    fixture.say(&mut screen, "start real startup").await;
    let agent = fixture.changed().await;
    assert_eq!(
        (&agent["agentSession"], &agent["session"]),
        (&json!("real"), &json!(id))
    );
    assert_eq!(fixture.list().await.len(), 1);
}

#[tokio::test]
async fn the_hook_forwarder_never_stalls_the_agent() {
    let dir = tempfile::Builder::new()
        .prefix("sd")
        .tempdir_in("/tmp")
        .unwrap();
    let silent = dir.path().join("silent.sock");
    let _never_accepts = std::os::unix::net::UnixListener::bind(&silent).unwrap();

    for (socket, close_stdin) in [
        (silent.clone(), false),
        (silent, true),
        (dir.path().join("missing.sock"), true),
    ] {
        let started = Instant::now();
        let mut hook = Command::new(DAEMON_EXE)
            .arg("hook")
            .env("BITPLANE_HOOK_SOCKET", &socket)
            .env("BITPLANE_SESSION_ID", "s")
            .env("BITPLANE_HOOK_TOKEN", "t")
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        let stdin = hook.stdin.take().unwrap();
        if close_stdin {
            use std::io::Write;
            let mut stdin = stdin;
            stdin.write_all(br#"{"hook_event_name":"Stop"}"#).unwrap();
            drop(stdin);
            assert!(hook.wait().unwrap().success());
        } else {
            assert!(hook.wait().unwrap().success());
            drop(stdin);
        }
        assert!(
            started.elapsed() < Duration::from_millis(1500),
            "{socket:?} took {:?}",
            started.elapsed()
        );
    }
}

#[tokio::test]
async fn closing_a_session_leaves_its_agent_listed_without_it() {
    let mut fixture = Fixture::start().await;
    let (id, mut screen) = fixture.session().await;
    fixture.claude(&mut screen, "claude").await;
    fixture.say(&mut screen, "start a1 startup").await;
    fixture.changed().await;

    fixture
        .client
        .call("session.close", Some(json!({ "session": id })))
        .await
        .unwrap();

    let agent = fixture.changed().await;
    assert_eq!(agent["session"], Value::Null);
    assert_eq!(agent["state"], json!({ "kind": "exited" }));
    assert_eq!(fixture.list().await[0]["session"], Value::Null);
}

#[tokio::test]
async fn without_claude_on_path_the_module_is_unavailable() {
    let home = tempfile::tempdir().unwrap();
    let daemon = TestDaemon::start_with_env(modules(), shell_env(home.path())).await;
    let client = daemon.connect().await;

    let hello = client.hello("test").await.unwrap();

    let agent = hello
        .capabilities
        .iter()
        .find(|capability| capability.module == "agent.claude")
        .unwrap();
    assert!(!agent.available);
    assert!(
        agent
            .reason
            .as_deref()
            .unwrap()
            .contains("claude not found on PATH"),
        "{:?}",
        agent.reason
    );
    let Err(ClientError::Rpc(error)) = client.call("agent.claude.list", None).await else {
        panic!("expected an RPC error")
    };
    assert_eq!(error.protocol_code(), Some(ErrorCode::ModuleUnavailable));
}

/// A daemon with the session and agent modules, whose sessions run a
/// non-login `/bin/sh` with the fake `claude` on `PATH`: a login shell would
/// let macOS's `path_helper` put the host's real `claude` first.
struct Fixture {
    home: tempfile::TempDir,
    root: tempfile::TempDir,
    log: PathBuf,
    daemon: TestDaemon,
    client: Client,
    watcher: Client,
    said: u32,
}

impl Fixture {
    async fn start() -> Self {
        let home = tempfile::tempdir().unwrap();
        let bin = home.path().join("bin");
        let log = home.path().join("log");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(&log).unwrap();
        executable(&bin.join("claude"), FAKE_CLAUDE);
        executable(&bin.join("sh-nologin"), "#!/bin/sh\nexec /bin/sh\n");
        let mut env: EnvMap = shell_env(home.path());
        env.insert("SHELL".into(), bin.join("sh-nologin").display().to_string());
        env.insert("PATH".into(), format!("{}:/usr/bin:/bin", bin.display()));
        env.insert("FAKE_CLAUDE_LOG".into(), log.display().to_string());
        let daemon = TestDaemon::start_with_env(modules(), env).await;
        let watcher = daemon.connect_hello().await;
        let client = daemon.connect_hello().await;
        Self {
            home,
            root: tempfile::tempdir().unwrap(),
            log,
            daemon,
            client,
            watcher,
            said: 0,
        }
    }

    async fn session(&mut self) -> (String, Screen) {
        let session = open(&self.client, json!({ "root": self.root.path() })).await;
        let id = session["id"].as_str().unwrap().to_string();
        let screen = Screen::attach(&mut self.client, &id).await;
        (id, screen)
    }

    async fn claude(&mut self, screen: &mut Screen, command: &str) {
        self.said += 1;
        let n = self.said;
        screen.type_text(&self.client, &format!("FAKE_RUN={n} {command}\r"));
        screen
            .wait_for(&mut self.client, &format!("fake-claude ready-{n}"))
            .await;
    }

    /// Types a line to the fake claude and waits until it ran the hook.
    async fn say(&mut self, screen: &mut Screen, line: &str) {
        self.said += 1;
        let n = self.said;
        screen.type_text(&self.client, &format!("{line} #{n}\r"));
        let verb = line.split_whitespace().next().unwrap();
        if verb != "quit" && verb != "end" {
            screen
                .wait_for(&mut self.client, &format!("done-{n}"))
                .await;
        }
    }

    async fn changed(&mut self) -> Value {
        next_event(&mut self.watcher, "agent.claude.changed").await["agent"].clone()
    }

    async fn removed(&mut self) -> Value {
        next_event(&mut self.watcher, "agent.claude.removed").await["agentSession"].clone()
    }

    async fn list(&self) -> Vec<Value> {
        let listed = self.client.call("agent.claude.list", None).await.unwrap();
        listed["agents"].as_array().unwrap().clone()
    }

    /// What the fake claude was last given as `--settings`.
    fn settings(&self) -> Value {
        serde_json::from_slice(&std::fs::read(self.log.join("settings.json")).unwrap()).unwrap()
    }
}

fn modules() -> Vec<std::sync::Arc<dyn bitplane_sessiond::module::Module>> {
    vec![
        Sessions::module(),
        Agents::claude(PathBuf::from(DAEMON_EXE)),
    ]
}

fn executable(path: &Path, script: &str) {
    std::fs::write(path, script).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}
