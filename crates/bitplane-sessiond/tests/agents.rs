//! The `agent` module against a real daemon: Claude terminals launch a fake
//! `claude`, found on a `PATH` the test controls, that checks its argv and
//! runs the hook commands it is given, as Claude Code would.

mod support;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use bitplane_protocol::ErrorCode;
use bitplane_sessiond::agent::{Agents, Timing};
use bitplane_sessiond::client::{Client, ClientError};
use bitplane_sessiond::env::EnvMap;
use bitplane_sessiond::session::Sessions;
use serde_json::{Value, json};
use support::{Host, Screen, Stop, TestDaemon, next_event, open, shell_env};

const DAEMON_EXE: &str = env!("CARGO_BIN_EXE_bitplane-sessiond");
const POLL: Duration = Duration::from_millis(100);
const LIVE: Duration = Duration::from_millis(1000);
const GRACE: Duration = Duration::from_millis(500);

/// Launched as a Claude terminal's PTY child, it checks its argv is exactly
/// `--settings <absolute hooks file> [--resume <id>]` with the hook env set,
/// and exits 64 otherwise. With no arguments it is a `claude` typed into a
/// regular terminal. Its argv goes to `$FAKE_CLAUDE_LOG/calls` and the
/// settings it was given to `$FAKE_CLAUDE_LOG/settings.json`; then it runs
/// the hook command with the payload each typed line asks for, unless
/// `FAKE_NO_HOOKS` is set, as when settings block hooks. Lines end in `#<n>`
/// and are answered `done-<n>`, so every wait has its own marker.
///
/// It also keeps its entry in a fake `claude agents --json` registry, in the
/// verified format: updated before each state hook (after `SessionStart`'s,
/// which is when the id becomes known), and changed without any hook by the
/// `reg`, `regname`, `regsid` and `bg` lines; `fork` lists a child of its
/// own as the agent's pid and `vanish` ends that child. `agents --json`
/// calls are counted in `$FAKE_CLAUDE_LOG/agents`.
///
/// `--resume <id>` starts that agent session half a second in, as the real
/// one takes a while; while `$FAKE_CLAUDE_LOG/resume-fails` exists it
/// instead waits that many seconds and quits without a `SessionStart`.
/// `sessionend` runs the `SessionEnd` hook without quitting, as a hang-up does.
const FAKE_CLAUDE: &str = r#"#!/bin/sh
log="$FAKE_CLAUDE_LOG"
if [ "$1" = agents ] && [ "$2" = --json ]; then
  echo agents >> "$log/agents"
  printf '['
  sep=""
  for entry in "$log"/registry/*.json; do
    [ -f "$entry" ] || continue
    printf '%s' "$sep"; cat "$entry"; sep=","
  done
  printf ']\n'
  exit 0
fi
bad() {
  echo "fake-claude: unexpected launch: $*"
  echo "$*" >> "$log/bad"
  exit 64
}
settings=""
print=""
resume=""
case "$1" in
  "") echo "bare token=${BITPLANE_HOOK_TOKEN:-none} socket=${BITPLANE_HOOK_SOCKET:-none}" >> "$log/calls" ;;
  -p) print=1; echo "$*" >> "$log/calls" ;;
  --settings)
    echo "$*" >> "$log/calls"
    case "$2" in /*) ;; *) bad "$@" ;; esac
    [ -f "$2" ] || bad "$@"
    settings="$2"
    case "$#:$3" in
      2:) ;;
      4:--resume) resume="$4" ;;
      *) bad "$@" ;;
    esac
    [ -n "$BITPLANE_HOOK_TOKEN" ] && [ -n "$BITPLANE_HOOK_SOCKET" ] && [ -n "$BITPLANE_SESSION_ID" ] || bad "no hook env"
    ;;
  *) bad "$@" ;;
esac
json=""
[ -n "$settings" ] && json=$(cat "$settings")
[ -n "$json" ] && printf '%s' "$json" > "$log/settings.json"
hook=$(printf '%s' "$json" | tr ',{}[]' '\n\n\n\n\n' | sed -n 's/.*"command": *"\([^"]*\)".*/\1/p' | grep ' hook$' | head -n 1)
[ -n "$FAKE_NO_HOOKS" ] && hook=""
emit() {
  [ -n "$hook" ] || return 0
  printf '{"hook_event_name":"%s","session_id":"%s","cwd":"%s"%s}' "$1" "$sid" "$PWD" "$2" | sh -c "$hook"
}
entry="$log/registry/$$.json"
pid=$$
status=idle
waiting=""
name=""
reg() {
  [ -z "$FAKE_NO_REGISTRY" ] || return 0
  mkdir -p "$log/registry"
  fields="\"pid\":$pid,\"cwd\":\"$PWD\",\"kind\":\"interactive\",\"startedAt\":1"
  [ -n "$sid" ] && fields="$fields,\"sessionId\":\"$sid\""
  [ -n "$name" ] && fields="$fields,\"name\":\"$name\""
  fields="$fields,\"status\":\"$status\""
  [ -n "$waiting" ] && fields="$fields,\"waitingFor\":\"$waiting\""
  printf '{%s}' "$fields" > "$entry.tmp" && mv "$entry.tmp" "$entry"
}
set_status() { status="$1"; waiting="$2"; reg; }
if [ -n "$print" ]; then
  sid=nested
  emit SessionStart ',"source":"startup"'
  echo "printed"
  exit 0
fi
export CLAUDECODE=1
if [ -n "$resume" ] && [ -f "$log/resume-fails" ]; then
  sleep "$(cat "$log/resume-fails")"
  echo "No conversation found with session ID: $resume"
  exit 1
fi
trap 'rm -f "$entry"; [ "$pid" = $$ ] || kill "$pid" 2>/dev/null' EXIT
reg
if [ -n "$resume" ]; then
  sleep 0.5
  sid="$resume"
  emit SessionStart ',"source":"resume"'
  reg
fi
echo "fake-claude ready-$FAKE_RUN"
while IFS= read -r line; do
  n="${line##*#}"
  set -- ${line% #*}
  case "$1" in
    start) sid="$2"; emit SessionStart ",\"source\":\"$3\""; reg ;;
    prompt) set_status busy; emit UserPromptSubmit ',"prompt":"hi"' ;;
    permission) set_status waiting "permission prompt"; emit PermissionRequest ',"tool_name":"Bash"' ;;
    notify)
      case "$2" in
        permission_prompt) set_status waiting "permission prompt" ;;
        elicitation_dialog|agent_needs_input) set_status waiting "input needed" ;;
      esac
      emit Notification ",\"notification_type\":\"$2\",\"message\":\"m\"" ;;
    stop) set_status idle; emit Stop ',"stop_hook_active":false' ;;
    fail) set_status idle; emit StopFailure ',"error":"rate_limit","error_details":"429 Too Many Requests","last_assistant_message":"API Error: Rate limit reached"' ;;
    end) rm -f "$entry"; emit SessionEnd ",\"reason\":\"$2\""; exit 0 ;;
    sessionend) emit SessionEnd ",\"reason\":\"$2\"" ;;
    reg) s="$2"; shift 2; set_status "$s" "$*" ;;
    regname) name="$2"; reg ;;
    regsid) sid="$2"; reg ;;
    fork) sleep 60 & pid=$!; reg ;;
    vanish) kill "$pid"; rm -f "$entry" ;;
    bg)
      rm -f "$entry"
      printf '{"pid":1,"id":"bg1","cwd":"%s","kind":"background","startedAt":1,"sessionId":"%s","status":"idle","state":"running"}' "$PWD" "$sid" > "$log/registry/bg.json" ;;
    nested) claude -p nested ;;
    quit) exit 0 ;;
  esac
  echo "done-$n"
done
"#;

#[tokio::test]
async fn hooks_drive_the_agent_through_its_states() {
    let mut fixture = Fixture::start().await;
    let (id, mut screen) = fixture.claude_terminal(json!({})).await;

    let session = fixture.session_info(&id).await;
    assert_eq!(session["agent"], json!({ "kind": "claude" }));
    let hooks = fixture
        .daemon
        .dirs
        .runtime
        .join("agent-hooks")
        .join(format!("{id}.json"));
    assert_eq!(fixture.calls(), format!("--settings {}\n", hooks.display()));
    fixture.say(&mut screen, "start a1 startup").await;
    let agent = fixture.changed().await;
    assert_eq!(
        agent,
        json!({
            "kind": "claude",
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
    let (id, mut screen) = fixture.claude_terminal(json!({})).await;
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
async fn a_nested_claude_gets_no_hooks_and_never_takes_the_agent() {
    let mut fixture = Fixture::start().await;
    let (_id, mut screen) = fixture.claude_terminal(json!({})).await;
    fixture.say(&mut screen, "start a1 startup").await;
    assert_eq!(fixture.changed().await["agentSession"], "a1");

    fixture.say(&mut screen, "nested").await;
    fixture.say(&mut screen, "prompt").await;

    let agent = fixture.changed().await;
    assert_eq!(
        (&agent["agentSession"], &agent["state"]),
        (&json!("a1"), &json!({ "kind": "busy" }))
    );
    let calls = fixture.calls();
    let nested = calls.lines().find(|call| call.starts_with("-p")).unwrap();
    assert!(!nested.contains("--settings"), "{calls}");
}

#[tokio::test]
async fn a_claude_terminal_starts_in_the_cwd_it_was_opened_with() {
    let mut fixture = Fixture::start().await;
    let web = fixture.root.path().join("web");
    std::fs::create_dir(&web).unwrap();

    let (_id, mut screen) = fixture.claude_terminal(json!({ "cwd": web })).await;
    fixture.say(&mut screen, "start a1 startup").await;

    assert_eq!(
        fixture.changed().await["cwd"],
        json!(web.canonicalize().unwrap())
    );
}

#[tokio::test]
async fn hooks_without_their_sessions_token_are_ignored() {
    let mut fixture = Fixture::start().await;
    let (id, mut screen) = fixture.claude_terminal(json!({})).await;
    let socket = fixture.daemon.dirs.runtime.join("hook.sock");
    let start = |agent_session: &str| {
        format!(
            r#"{{"hook_event_name":"SessionStart","session_id":"{agent_session}","cwd":"/","source":"startup"}}"#
        )
    };

    for (session, token) in [
        (Some(id.as_str()), Some("forged")),
        (Some(&*uuid::Uuid::new_v4().to_string()), None),
        (None, None),
    ] {
        let mut hook = Command::new(DAEMON_EXE);
        hook.arg("hook")
            .env_clear()
            .env("BITPLANE_HOOK_SOCKET", &socket)
            .stdin(Stdio::piped());
        if let Some(session) = session {
            hook.env("BITPLANE_SESSION_ID", session);
        }
        if let Some(token) = token {
            hook.env("BITPLANE_HOOK_TOKEN", token);
        }
        let status = hook
            .spawn()
            .and_then(|mut hook| {
                use std::io::Write;
                hook.stdin
                    .take()
                    .unwrap()
                    .write_all(start("forged").as_bytes())?;
                hook.wait()
            })
            .unwrap();
        assert!(status.success());
    }

    fixture.say(&mut screen, "start real startup").await;
    let agent = fixture.changed().await;
    assert_eq!(
        (&agent["agentSession"], &agent["session"]),
        (&json!("real"), &json!(id))
    );
    assert_eq!(fixture.list().await.len(), 1);
}

#[tokio::test]
async fn a_claude_in_a_regular_terminal_is_not_tracked() {
    let mut fixture = Fixture::start().await;
    let session = open(&fixture.client, json!({ "root": fixture.root.path() })).await;
    let id = session["id"].as_str().unwrap().to_string();
    assert!(session.get("agent").is_none(), "{session}");
    let mut screen = Screen::attach(&mut fixture.client, &id).await;

    screen.type_text(&fixture.client, "echo \"path=$PATH\"\r");
    let path = format!("path={}", fixture.env["PATH"]);
    screen.wait_for(&mut fixture.client, &path).await;
    screen.type_text(&fixture.client, "export FAKE_RUN=1; claude\r");
    screen
        .wait_for(&mut fixture.client, "fake-claude ready-1")
        .await;
    for line in ["start a1 startup", "prompt"] {
        fixture.say(&mut screen, line).await;
    }

    let quiet = tokio::time::timeout(GRACE * 6, fixture.changed()).await;
    assert!(quiet.is_err(), "tracked: {quiet:?}");
    assert_eq!(fixture.list().await, Vec::<Value>::new());
    assert_eq!(fixture.calls(), "bare token=none socket=none\n");
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
    let (id, mut screen) = fixture.claude_terminal(json!({})).await;
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
async fn the_registry_corrects_what_the_hooks_missed() {
    let mut fixture = Fixture::start().await;
    let (_id, mut screen) = fixture.claude_terminal(json!({})).await;
    fixture.say(&mut screen, "start a1 startup").await;
    fixture.changed().await;
    fixture.say(&mut screen, "prompt").await;
    assert_eq!(fixture.changed().await["state"], json!({ "kind": "busy" }));

    fixture.say(&mut screen, "reg idle").await;
    let agent = fixture.changed_within(Duration::from_secs(3)).await;
    assert_eq!(agent["state"], json!({ "kind": "idle" }), "Esc interrupt");

    fixture.say(&mut screen, "prompt").await;
    fixture.changed().await;
    fixture.say(&mut screen, "permission").await;
    assert_eq!(fixture.changed().await["state"]["kind"], "waiting");
    fixture.say(&mut screen, "reg busy").await;
    let agent = fixture.changed_within(Duration::from_secs(3)).await;
    assert_eq!(
        agent["state"],
        json!({ "kind": "busy" }),
        "permission granted"
    );

    fixture.say(&mut screen, "regname web-fix").await;
    let agent = fixture.changed_within(Duration::from_secs(3)).await;
    assert_eq!(
        (&agent["name"], &agent["tracking"]),
        (&json!("web-fix"), &json!("hooks"))
    );
    fixture.say(&mut screen, "stop").await;
    let agent = fixture.changed().await;
    assert_eq!(
        (&agent["state"], &agent["name"]),
        (&json!({ "kind": "idle" }), &json!("web-fix"))
    );
}

#[tokio::test]
async fn a_failed_turn_sets_last_error_until_the_next_prompt() {
    let mut fixture = Fixture::start().await;
    let (_id, mut screen) = fixture.claude_terminal(json!({})).await;
    fixture.say(&mut screen, "start a1 startup").await;
    fixture.changed().await;
    fixture.say(&mut screen, "prompt").await;
    fixture.changed().await;

    fixture.say(&mut screen, "fail").await;
    let mut agent = fixture.changed().await;
    if agent.get("lastError").is_none() {
        assert_eq!(
            agent["state"],
            json!({ "kind": "idle" }),
            "from the registry"
        );
        agent = fixture.changed().await;
    }
    assert_eq!(agent["state"], json!({ "kind": "idle" }));
    assert_eq!(
        agent["lastError"],
        json!({ "error": "rate_limit", "message": "API Error: Rate limit reached" })
    );
    assert_eq!(fixture.list().await[0]["lastError"]["error"], "rate_limit");

    fixture.say(&mut screen, "prompt").await;
    let agent = fixture.changed().await;
    assert_eq!(agent["state"], json!({ "kind": "busy" }));
    assert!(agent.get("lastError").is_none(), "{agent}");
}

#[tokio::test]
async fn the_registry_is_polled_often_while_an_agent_works_and_seldom_while_one_is_idle() {
    let mut fixture = Fixture::start().await;
    tokio::time::sleep(POLL * 5).await;
    assert_eq!(fixture.probes(), 0, "probed with no agent");
    let (id, mut screen) = fixture.claude_terminal(json!({})).await;
    fixture.say(&mut screen, "start a1 startup").await;
    fixture.changed().await;
    fixture.say(&mut screen, "prompt").await;
    fixture.changed().await;
    let before = fixture.probes();
    tokio::time::sleep(POLL * 5).await;
    assert!(fixture.probes() > before, "no probe while busy");

    fixture.say(&mut screen, "stop").await;
    fixture.changed().await;
    tokio::time::sleep(POLL * 3).await;
    let settled = fixture.probes();
    tokio::time::sleep(LIVE * 5 / 2).await;
    let idle = fixture.probes() - settled;
    assert!(
        (1..=3).contains(&idle),
        "{idle} probes in 2.5 live intervals while every agent is idle"
    );

    fixture
        .client
        .call("session.close", Some(json!({ "session": id })))
        .await
        .unwrap();
    tokio::time::sleep(LIVE / 2).await;
    let closed = fixture.probes();
    tokio::time::sleep(LIVE * 2).await;
    assert_eq!(fixture.probes(), closed, "probed with no live agent");
}

#[tokio::test]
async fn without_hooks_the_agent_is_tracked_from_the_registry() {
    let mut fixture = Fixture::start().await;
    let (id, mut screen) = fixture
        .claude_terminal_with(json!({}), json!({ "FAKE_NO_HOOKS": "1" }))
        .await;

    fixture.say(&mut screen, "start a1 startup").await;
    let agent = fixture.changed_within(Duration::from_secs(5)).await;
    assert_eq!(
        agent,
        json!({
            "kind": "claude",
            "agentSession": "a1",
            "session": id,
            "cwd": fixture.root.path().canonicalize().unwrap(),
            "state": { "kind": "idle" },
            "tracking": "poll",
        })
    );

    for (line, state) in [
        ("reg busy", json!({ "kind": "busy" })),
        (
            "reg waiting worker request",
            json!({ "kind": "waiting", "reason": "other" }),
        ),
        (
            "reg waiting permission prompt",
            json!({ "kind": "waiting", "reason": "permission" }),
        ),
        ("reg idle", json!({ "kind": "idle" })),
    ] {
        fixture.say(&mut screen, line).await;
        let agent = fixture.changed_within(Duration::from_secs(3)).await;
        assert_eq!(agent["state"], state, "after {line}");
        assert_eq!(agent["tracking"], "poll");
    }

    fixture.say(&mut screen, "regsid a2").await;
    assert_eq!(fixture.removed().await, "a1", "/clear");
    let agent = fixture.changed().await;
    assert_eq!(
        (&agent["agentSession"], &agent["tracking"]),
        (&json!("a2"), &json!("poll"))
    );

    fixture.say(&mut screen, "quit").await;
    let agent = fixture.changed().await;
    assert_eq!(
        (&agent["agentSession"], &agent["state"]),
        (&json!("a2"), &json!({ "kind": "exited" }))
    );
}

#[tokio::test]
async fn without_hooks_a_resumed_agent_is_tracked_from_the_registry() {
    let mut fixture = Fixture::start().await;
    let (id, mut screen) = fixture
        .claude_terminal_with(json!({ "resume": "a1" }), json!({ "FAKE_NO_HOOKS": "1" }))
        .await;

    assert_eq!(fixture.changed().await["state"]["kind"], "resuming");
    let agent = fixture.changed_within(Duration::from_secs(5)).await;
    assert_eq!(
        (
            &agent["agentSession"],
            &agent["session"],
            &agent["state"],
            &agent["tracking"]
        ),
        (
            &json!("a1"),
            &json!(id),
            &json!({ "kind": "idle" }),
            &json!("poll")
        )
    );

    fixture.say(&mut screen, "reg busy").await;
    let agent = fixture.changed_within(Duration::from_secs(3)).await;
    assert_eq!(
        (&agent["state"], &agent["tracking"]),
        (&json!({ "kind": "busy" }), &json!("poll"))
    );
}

#[tokio::test]
async fn claude_exiting_exits_the_session_and_its_agent() {
    let mut fixture = Fixture::start().await;
    let (id, mut screen) = fixture.claude_terminal(json!({})).await;
    fixture.say(&mut screen, "start a1 startup").await;
    fixture.changed().await;

    fixture.say(&mut screen, "quit").await;

    let agent = fixture.changed_within(Duration::from_secs(3)).await;
    assert_eq!(agent["state"], json!({ "kind": "exited" }));
    fixture
        .status_becomes(&id, json!({ "state": "exited", "code": 0 }))
        .await;
}

#[tokio::test]
async fn an_agent_whose_pid_vanishes_exits() {
    let mut fixture = Fixture::start().await;
    let (_id, mut screen) = fixture.claude_terminal(json!({})).await;
    fixture.say(&mut screen, "start a1 startup").await;
    fixture.changed().await;
    fixture.say(&mut screen, "fork").await;
    fixture.say(&mut screen, "prompt").await;
    fixture.changed().await;
    tokio::time::sleep(POLL * 3).await;

    fixture.say(&mut screen, "vanish").await;

    let agent = fixture.changed_within(Duration::from_secs(3)).await;
    assert_eq!(agent["state"], json!({ "kind": "exited" }));
    fixture.daemon.stop().await.unwrap();
}

#[tokio::test]
async fn an_agent_moved_to_bg_exits_even_while_every_agent_is_idle() {
    let mut fixture = Fixture::start().await;
    let (id, mut screen) = fixture.claude_terminal(json!({})).await;
    fixture.say(&mut screen, "start a1 startup").await;
    fixture.changed().await;

    fixture.say(&mut screen, "bg").await;

    let agent = fixture.changed_within(LIVE * 3).await;
    assert_eq!(
        (&agent["agentSession"], &agent["state"]),
        (&json!("a1"), &json!({ "kind": "exited" }))
    );
    let fixture = fixture.restart().await;
    let listed = fixture.list().await;
    assert_eq!(
        (&listed[0]["agentSession"], &listed[0]["session"]),
        (&json!("a1"), &json!(id)),
        "/bg keeps the link: {listed:?}"
    );
}

#[tokio::test]
async fn without_claude_a_claude_terminal_fails_to_spawn_until_it_is_installed() {
    let fixture = Fixture::start().await;
    let later = fixture.home.path().join("later");
    std::fs::create_dir(&later).unwrap();
    let mut env = fixture.env.clone();
    env.insert("PATH".into(), format!("{}:/usr/bin:/bin", later.display()));
    let mut fixture = fixture.restart_with(env).await;

    let hello = fixture.daemon.connect().await.hello("test").await.unwrap();
    let agent = hello
        .capabilities
        .iter()
        .find(|capability| capability.module == "agent")
        .unwrap();
    assert!(agent.available, "{agent:?}");
    let listed = fixture.client.call("agent.list", None).await.unwrap();
    assert_eq!(listed["kinds"][0]["kind"], "claude");
    assert_eq!(listed["kinds"][0]["available"], false);
    let error = listed["kinds"][0]["error"].as_str().unwrap();
    assert!(error.contains("claude not found on PATH"), "{error}");

    let mut ids = Vec::new();
    for kind in ["claude", "codex"] {
        let session = open(
            &fixture.client,
            json!({ "root": fixture.root.path(), "agent": { "kind": kind } }),
        )
        .await;
        assert_eq!(session["agent"], json!({ "kind": kind }));
        assert_eq!(session["status"]["state"], "spawnFailed", "{session}");
        ids.push(session["id"].as_str().unwrap().to_string());
    }
    let sessions = fixture.client.call("session.list", None).await.unwrap()["sessions"].clone();
    assert!(
        sessions[0]["status"]["error"]
            .as_str()
            .unwrap()
            .contains("claude not found on PATH")
    );
    assert_eq!(sessions[1]["status"]["error"], "unknown agent kind codex");

    executable(&later.join("claude"), FAKE_CLAUDE);
    fixture.client.call("agent.list", None).await.unwrap();
    let changed = next_event(&mut fixture.watcher, "agent.kind.changed").await;
    assert_eq!(changed, json!({ "kind": "claude", "available": true }));
    let restarted = fixture
        .client
        .call("session.restart", Some(json!({ "session": ids[0] })))
        .await
        .unwrap();
    assert_eq!(
        restarted["session"]["status"],
        json!({ "state": "running" })
    );
    let mut screen = Screen::attach(&mut fixture.client, &ids[0]).await;
    screen
        .wait_for(&mut fixture.client, "fake-claude ready-")
        .await;
}

#[tokio::test]
async fn opening_a_claude_terminal_that_resumes_launches_it_and_refuses_a_live_one() {
    let mut fixture = Fixture::start().await;
    let root = fixture.root.path().to_path_buf();
    let resume = |id: &str| json!({ "root": root, "agent": { "kind": "claude", "resume": id } });

    let session = open(&fixture.client, resume("a1")).await;

    let id = session["id"].as_str().unwrap().to_string();
    assert_eq!(
        fixture.changed().await,
        json!({
            "kind": "claude",
            "agentSession": "a1",
            "session": id,
            "cwd": root,
            "state": { "kind": "resuming" },
            "tracking": "hooks",
        })
    );
    let agent = fixture.changed().await;
    assert_eq!(
        (&agent["agentSession"], &agent["state"]),
        (&json!("a1"), &json!({ "kind": "idle" }))
    );
    assert_eq!(agent["cwd"], json!(root.canonicalize().unwrap()));
    let resumed = fixture.calls();
    assert!(resumed.starts_with("--settings /"), "{resumed}");
    assert!(resumed.ends_with(" --resume a1\n"), "{resumed}");

    for (id, code) in [
        ("a1", Some(ErrorCode::AgentLive)),
        ("a1; touch pwned", None),
        ("--dangerously-skip-permissions", None),
    ] {
        let Err(ClientError::Rpc(error)) =
            fixture.client.call("session.open", Some(resume(id))).await
        else {
            panic!("resuming {id:?} must fail")
        };
        assert_eq!(error.protocol_code(), code, "{id}");
    }
    let listed = fixture.client.call("session.list", None).await.unwrap();
    assert_eq!(listed["sessions"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn a_resumed_agent_that_never_starts_exits() {
    let mut fixture = Fixture::start().await;
    let root = fixture.root.path().to_path_buf();

    for (id, fails_after) in [("a1", "1"), ("a2", "0")] {
        std::fs::write(fixture.log.join("resume-fails"), fails_after).unwrap();
        let session = open(
            &fixture.client,
            json!({ "root": root, "agent": { "kind": "claude", "resume": id } }),
        )
        .await;

        assert_eq!(fixture.changed().await["state"]["kind"], "resuming");
        let agent = fixture.changed_within(Duration::from_secs(10)).await;
        assert_eq!(
            (&agent["agentSession"], &agent["state"]),
            (&json!(id), &json!({ "kind": "exited" })),
            "failing after {fails_after}s"
        );
        fixture
            .status_becomes(
                session["id"].as_str().unwrap(),
                json!({ "state": "exited", "code": 1 }),
            )
            .await;
    }
}

#[tokio::test]
async fn restore_resumes_the_agent_session_each_session_last_started() {
    let mut fixture = Fixture::start().await;
    let (first, mut screen) = fixture.claude_terminal(json!({})).await;
    for line in ["start a1 startup", "start a1 compact", "start a2 clear"] {
        fixture.say(&mut screen, line).await;
    }
    fixture.say(&mut screen, "sessionend other").await;
    let (second, mut screen) = fixture.claude_terminal(json!({})).await;
    fixture.say(&mut screen, "start b1 startup").await;

    let mut fixture = fixture.restart().await;

    let mut listed = fixture.list().await;
    listed.sort_by_key(|agent| agent["agentSession"].as_str().unwrap().to_string());
    let summary: Vec<Value> = listed
        .iter()
        .map(|agent| json!([agent["agentSession"], agent["session"], agent["state"]]))
        .collect();
    assert_eq!(
        summary,
        [
            json!(["a2", first, { "kind": "resuming" }]),
            json!(["b1", second, { "kind": "resuming" }]),
        ]
    );
    for _ in 0..2 {
        assert_eq!(fixture.changed().await["state"], json!({ "kind": "idle" }));
    }
    let calls = fixture.calls();
    assert!(!calls.contains("--resume a1"), "{calls}");
}

#[tokio::test]
async fn claude_exiting_keeps_the_link_for_a_restart() {
    let mut fixture = Fixture::start().await;
    let (id, mut screen) = fixture.claude_terminal(json!({})).await;
    fixture.say(&mut screen, "start a1 startup").await;
    fixture.changed().await;
    fixture.say(&mut screen, "end prompt_input_exit").await;
    assert_eq!(
        fixture.changed().await["state"],
        json!({ "kind": "exited" })
    );
    fixture
        .status_becomes(&id, json!({ "state": "exited", "code": 0 }))
        .await;

    fixture
        .client
        .call("session.restart", Some(json!({ "session": id })))
        .await
        .unwrap();

    assert_eq!(
        fixture.changed().await["state"],
        json!({ "kind": "resuming" })
    );
    assert_eq!(fixture.changed().await["state"], json!({ "kind": "idle" }));
    assert!(
        fixture.calls().contains("--resume a1"),
        "{}",
        fixture.calls()
    );
}

#[tokio::test]
async fn an_exited_claude_terminal_comes_back_exited_with_its_link() {
    let mut fixture = Fixture::start().await;
    let (id, mut screen) = fixture.claude_terminal(json!({})).await;
    fixture.say(&mut screen, "start a1 startup").await;
    fixture.say(&mut screen, "end prompt_input_exit").await;
    fixture
        .status_becomes(&id, json!({ "state": "exited", "code": 0 }))
        .await;
    let before = fixture.list().await;
    assert_eq!(
        before[0]["state"],
        json!({ "kind": "exited" }),
        "{before:?}"
    );

    let mut fixture = fixture.restart().await;

    assert_eq!(
        fixture.session_info(&id).await["status"],
        json!({ "state": "exited", "code": 0 })
    );
    assert_eq!(fixture.list().await, before, "listed as before the restart");
    assert!(!fixture.calls().contains("--resume"), "{}", fixture.calls());
    fixture
        .client
        .call("session.restart", Some(json!({ "session": id })))
        .await
        .unwrap();
    assert_eq!(
        fixture.changed().await["state"],
        json!({ "kind": "resuming" })
    );
    assert_eq!(fixture.changed().await["state"], json!({ "kind": "idle" }));
    assert!(
        fixture.calls().contains("--resume a1"),
        "{}",
        fixture.calls()
    );
}

#[tokio::test]
async fn without_claude_at_restore_the_session_fails_to_spawn_and_keeps_the_link() {
    let mut fixture = Fixture::start().await;
    let (id, mut screen) = fixture.claude_terminal(json!({})).await;
    fixture.say(&mut screen, "start a1 startup").await;
    let mut agent = fixture.changed().await;
    agent["state"] = json!({ "kind": "exited" });
    let later = fixture.home.path().join("later");
    std::fs::create_dir(&later).unwrap();
    let mut without = fixture.env.clone();
    without.insert("PATH".into(), format!("{}:/usr/bin:/bin", later.display()));

    let mut fixture = fixture.restart_with(without).await;

    let listed = fixture.client.call("session.list", None).await.unwrap();
    assert_eq!(listed["sessions"][0]["id"], json!(id));
    assert_eq!(
        listed["sessions"][0]["status"]["state"],
        json!("spawnFailed")
    );
    assert!(!fixture.calls().contains("--resume"));
    assert_eq!(fixture.list().await, [agent]);

    executable(&later.join("claude"), FAKE_CLAUDE);
    fixture
        .client
        .call("session.restart", Some(json!({ "session": id })))
        .await
        .unwrap();
    let agent = fixture.changed().await;
    assert_eq!(
        (&agent["agentSession"], &agent["session"], &agent["state"]),
        (&json!("a1"), &json!(id), &json!({ "kind": "resuming" }))
    );
    assert_eq!(fixture.changed().await["state"], json!({ "kind": "idle" }));
    assert!(fixture.calls().contains("--resume a1"));
}

#[tokio::test]
async fn resuming_starts_in_the_given_cwd_else_the_known_agents() {
    let mut fixture = Fixture::start().await;
    let web = fixture.root.path().join("web");
    std::fs::create_dir(&web).unwrap();
    let outside = fixture.home.path().join("outside");
    std::fs::create_dir(&outside).unwrap();
    let (_first, mut screen) = fixture.claude_terminal(json!({ "cwd": web })).await;
    fixture.say(&mut screen, "start a1 startup").await;
    fixture.changed().await;
    fixture.say(&mut screen, "quit").await;
    screen.wait_for(&mut fixture.client, "quit #").await;
    fixture.changed().await;

    let (_known, mut screen) = fixture.claude_terminal(json!({ "resume": "a1" })).await;
    let resuming = fixture.changed().await;
    assert_eq!(
        (&resuming["state"], &resuming["cwd"]),
        (
            &json!({ "kind": "resuming" }),
            &json!(web.canonicalize().unwrap())
        )
    );
    assert_eq!(
        fixture.changed().await["cwd"],
        json!(web.canonicalize().unwrap()),
        "the known agent's cwd"
    );
    fixture.say(&mut screen, "quit").await;
    screen.wait_for(&mut fixture.client, "quit #").await;
    fixture.changed().await;

    let (given, _screen) = fixture
        .claude_terminal(json!({ "resume": "a1", "cwd": outside }))
        .await;
    assert_eq!(fixture.changed().await["cwd"], json!(outside));
    let outside = outside.canonicalize().unwrap();
    assert_eq!(
        fixture.changed().await["cwd"],
        json!(outside),
        "the given cwd wins, even outside the root"
    );

    let mut fixture = fixture.restart().await;
    let listed = fixture.list().await;
    assert_eq!(
        (&listed[0]["session"], &listed[0]["cwd"]),
        (&json!(given), &json!(outside)),
        "the link records the cwd used: {listed:?}"
    );
    assert_eq!(fixture.changed().await["cwd"], json!(outside));
}

#[tokio::test]
async fn a_given_cwd_is_kept_across_a_restore() {
    let mut fixture = Fixture::start().await;
    let web = fixture.root.path().join("web");
    std::fs::create_dir(&web).unwrap();
    let (id, _screen) = fixture.claude_terminal(json!({ "cwd": web })).await;

    let mut fixture = fixture.restart().await;

    let mut screen = Screen::attach(&mut fixture.client, &id).await;
    screen
        .wait_for(&mut fixture.client, "fake-claude ready-")
        .await;
    fixture.say(&mut screen, "start a1 startup").await;
    assert_eq!(
        fixture.changed().await["cwd"],
        json!(web.canonicalize().unwrap())
    );
}

#[tokio::test]
async fn a_missing_cwd_fails_to_spawn_and_keeps_the_link() {
    let fixture = Fixture::start().await;
    let gone = fixture.root.path().join("gone");
    let session = open(
        &fixture.client,
        json!({ "root": fixture.root.path(),
                "agent": { "kind": "claude", "resume": "a1", "cwd": gone } }),
    )
    .await;
    let id = session["id"].as_str().unwrap().to_string();
    assert_eq!(session["status"]["state"], "spawnFailed", "{session}");
    let error = session["status"]["error"].as_str().unwrap();
    assert!(error.contains(&*gone.to_string_lossy()), "{error}");
    assert_eq!(fixture.list().await, Vec::<Value>::new());

    let mut fixture = fixture.restart().await;
    assert_eq!(
        fixture.session_info(&id).await["status"]["state"],
        "spawnFailed"
    );
    assert_eq!(
        fixture.list().await,
        [
            json!({ "kind": "claude", "agentSession": "a1", "session": id, "cwd": gone,
                 "state": { "kind": "exited" }, "tracking": "hooks" })
        ]
    );
    std::fs::create_dir(&gone).unwrap();
    fixture
        .client
        .call("session.restart", Some(json!({ "session": id })))
        .await
        .unwrap();

    assert_eq!(
        fixture.changed().await["state"],
        json!({ "kind": "resuming" })
    );
    let agent = fixture.changed().await;
    assert_eq!(
        (&agent["agentSession"], &agent["cwd"]),
        (&json!("a1"), &json!(gone.canonicalize().unwrap()))
    );
    assert!(
        fixture.calls().contains("--resume a1"),
        "{}",
        fixture.calls()
    );
}

#[tokio::test]
async fn shutdown_with_claude_running_keeps_the_link() {
    the_link_survives(Stop::ShutdownWithRestore).await;
}

#[tokio::test]
async fn a_sigterm_with_claude_running_keeps_the_link() {
    the_link_survives(Stop::Sigterm).await;
}

#[tokio::test]
async fn a_sigkill_with_claude_running_keeps_the_link() {
    the_link_survives(Stop::Sigkill).await;
}

#[tokio::test]
async fn an_exit_before_a_sigkill_comes_back_exited() {
    let host = Host::new();
    let env = fake_claude_env(host.home.path());
    let log = host.home.path().join("log");
    let root = tempfile::tempdir().unwrap();
    let mut first = host.serve_with(&env);
    let mut client = first.connect().await;
    client.hello("test").await.unwrap();
    let session = open(
        &client,
        json!({ "root": root.path(), "agent": { "kind": "claude" } }),
    )
    .await;
    let id = session["id"].as_str().unwrap().to_string();
    let mut screen = Screen::attach(&mut client, &id).await;
    screen.wait_for(&mut client, "fake-claude ready-").await;
    screen.type_text(&client, "start a1 startup #1\r");
    screen.wait_for(&mut client, "done-1").await;
    screen.type_text(&client, "end prompt_input_exit #2\r");
    let deadline = Instant::now() + Duration::from_secs(10);
    while client.call("session.list", None).await.unwrap()["sessions"][0]["status"]["state"]
        != "exited"
    {
        assert!(Instant::now() < deadline, "claude never exited");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // Past the save debounce, so the exit is on disk before the crash.
    tokio::time::sleep(Duration::from_secs(1)).await;

    first.stop(&client, Stop::Sigkill).await;
    drop(client);

    let mut second = host.serve_with(&env);
    let client = second.connect().await;
    client.hello("test").await.unwrap();
    let listed = client.call("session.list", None).await.unwrap()["sessions"].clone();
    assert_eq!(
        listed[0]["status"],
        json!({ "state": "exited", "code": 0 }),
        "{listed}"
    );
    let calls = std::fs::read_to_string(log.join("calls")).unwrap();
    assert!(!calls.contains("--resume"), "{calls}");
    second.stop(&client, Stop::Sigterm).await;
}

/// The regression behind cc#43: every way the daemon stops, with the agent
/// still running in its Claude terminal, resumes it in the next instance.
async fn the_link_survives(stop: Stop) {
    let host = Host::new();
    let env = fake_claude_env(host.home.path());
    let log = host.home.path().join("log");
    let root = tempfile::tempdir().unwrap();
    let mut first = host.serve_with(&env);
    let mut client = first.connect().await;
    client.hello("test").await.unwrap();
    let session = open(
        &client,
        json!({ "root": root.path(), "agent": { "kind": "claude" } }),
    )
    .await;
    let id = session["id"].as_str().unwrap().to_string();
    let mut screen = Screen::attach(&mut client, &id).await;
    screen.wait_for(&mut client, "fake-claude ready-").await;
    screen.type_text(&client, "start a1 startup #1\r");
    screen.wait_for(&mut client, "done-1").await;
    let agent = next_event(&mut client, "agent.changed").await["agent"].clone();
    assert_eq!(agent["agentSession"], "a1");

    first.stop(&client, stop).await;
    drop(client);

    let mut second = host.serve_with(&env);
    let client = second.connect().await;
    client.hello("test").await.unwrap();
    let listed = client.call("agent.list", None).await.unwrap()["agents"].clone();
    assert_eq!(
        (&listed[0]["agentSession"], &listed[0]["session"]),
        (&json!("a1"), &json!(id)),
        "after {stop:?}: {listed}"
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    while client.call("agent.list", None).await.unwrap()["agents"][0]["state"]
        != json!({ "kind": "idle" })
    {
        assert!(
            Instant::now() < deadline,
            "a1 never came back after {stop:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let calls = std::fs::read_to_string(log.join("calls")).unwrap();
    assert!(calls.contains("--resume a1"), "{calls}");
    second.stop(&client, Stop::Sigterm).await;
}

/// A daemon with the session and agent modules whose login env finds only
/// the fake `claude`; regular terminals run a non-login `/bin/sh`, so
/// macOS's `path_helper` can't put the host's real `claude` first there.
struct Fixture {
    home: tempfile::TempDir,
    root: tempfile::TempDir,
    log: PathBuf,
    env: EnvMap,
    daemon: TestDaemon,
    client: Client,
    watcher: Client,
    said: u32,
}

impl Fixture {
    async fn start() -> Self {
        let home = tempfile::tempdir().unwrap();
        let env = fake_claude_env(home.path());
        let daemon = TestDaemon::start_with_env(modules(), env.clone()).await;
        let watcher = daemon.connect_hello().await;
        let client = daemon.connect_hello().await;
        Self {
            log: home.path().join("log"),
            home,
            root: tempfile::tempdir().unwrap(),
            env,
            daemon,
            client,
            watcher,
            said: 0,
        }
    }

    /// `daemon.shutdown { restore: true }`, then the next instance on the
    /// same directories with `env`.
    async fn restart_with(self, env: EnvMap) -> Self {
        let reply = self
            .client
            .call("daemon.shutdown", Some(json!({ "restore": true })))
            .await
            .unwrap();
        assert_eq!(reply, json!({}));
        let Self {
            home,
            root,
            log,
            env: fixture_env,
            daemon,
            ..
        } = self;
        let daemon = daemon.restart(modules(), env).await;
        let watcher = daemon.connect_hello().await;
        let client = daemon.connect_hello().await;
        Self {
            home,
            root,
            log,
            env: fixture_env,
            daemon,
            client,
            watcher,
            said: 0,
        }
    }

    async fn restart(self) -> Self {
        let env = self.env.clone();
        self.restart_with(env).await
    }

    /// The `claude` calls the fake was started with.
    fn calls(&self) -> String {
        std::fs::read_to_string(self.log.join("calls")).unwrap_or_default()
    }

    /// Opens a Claude terminal with the `agent` params given and waits for
    /// its fake `claude`.
    async fn claude_terminal(&mut self, agent: Value) -> (String, Screen) {
        self.claude_terminal_with(agent, json!({})).await
    }

    async fn claude_terminal_with(&mut self, agent: Value, env: Value) -> (String, Screen) {
        self.said += 1;
        let n = self.said;
        let mut agent = agent;
        agent["kind"] = json!("claude");
        let mut env = env;
        env["FAKE_RUN"] = json!(n.to_string());
        let session = open(
            &self.client,
            json!({ "root": self.root.path(), "env": env, "agent": agent }),
        )
        .await;
        let id = session["id"].as_str().unwrap().to_string();
        let mut screen = Screen::attach(&mut self.client, &id).await;
        screen
            .wait_for(&mut self.client, &format!("fake-claude ready-{n}"))
            .await;
        (id, screen)
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

    async fn session_info(&self, id: &str) -> Value {
        let listed = self.client.call("session.list", None).await.unwrap();
        listed["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|session| session["id"] == id)
            .unwrap()
            .clone()
    }

    async fn status_becomes(&self, id: &str, status: Value) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while self.session_info(id).await["status"] != status {
            assert!(Instant::now() < deadline, "{id} never became {status}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    async fn changed(&mut self) -> Value {
        next_event(&mut self.watcher, "agent.changed").await["agent"].clone()
    }

    async fn changed_within(&mut self, limit: Duration) -> Value {
        tokio::time::timeout(limit, self.changed())
            .await
            .unwrap_or_else(|_| panic!("no agent.changed within {limit:?}"))
    }

    async fn removed(&mut self) -> Value {
        next_event(&mut self.watcher, "agent.removed").await["agentSession"].clone()
    }

    async fn list(&self) -> Vec<Value> {
        let listed = self.client.call("agent.list", None).await.unwrap();
        listed["agents"].as_array().unwrap().clone()
    }

    /// How many times the registry was read.
    fn probes(&self) -> usize {
        std::fs::read_to_string(self.log.join("agents"))
            .map(|calls| calls.lines().count())
            .unwrap_or(0)
    }

    /// What the fake claude was last given as `--settings`.
    fn settings(&self) -> Value {
        serde_json::from_slice(&std::fs::read(self.log.join("settings.json")).unwrap()).unwrap()
    }
}

fn modules() -> Vec<std::sync::Arc<dyn bitplane_sessiond::module::Module>> {
    vec![
        Sessions::module(),
        Agents::claude_with(
            PathBuf::from(DAEMON_EXE),
            Timing {
                poll: POLL,
                live: LIVE,
                grace: GRACE,
            },
        ),
    ]
}

/// A login env whose only `claude` is the fake, in `home/bin`, logging to
/// `home/log`; its `PATH` names nothing else but `/usr/bin` and `/bin`, so
/// a host's real `claude` can never be launched. Its shell is never a login
/// shell, but still runs the `-l -i -c` script a daemon captures its
/// environment with.
fn fake_claude_env(home: &Path) -> EnvMap {
    let bin = home.join("bin");
    let log = home.join("log");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(&log).unwrap();
    executable(&bin.join("claude"), FAKE_CLAUDE);
    executable(
        &bin.join("sh-nologin"),
        "#!/bin/sh\nif [ \"$1 $3\" = \"-l -c\" ]; then eval \"$4\"; exit; fi\nexec /bin/sh\n",
    );
    let mut env: EnvMap = shell_env(home);
    env.insert("SHELL".into(), bin.join("sh-nologin").display().to_string());
    env.insert("PATH".into(), format!("{}:/usr/bin:/bin", bin.display()));
    env.insert("FAKE_CLAUDE_LOG".into(), log.display().to_string());
    env
}

fn executable(path: &Path, script: &str) {
    std::fs::write(path, script).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}
