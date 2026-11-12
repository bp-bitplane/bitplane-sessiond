//! The Claude Code adapter: Claude terminals launch `claude --settings
//! <per-session hooks file>` (ADR 0029), and what those hooks mean (ADR 0030).

use std::ffi::OsStr;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use bitplane_protocol::agent::{AgentError, AgentState, WaitingReason};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{AgentAdapter, AgentCommand, AgentEvent, Ingested, LiveAgent};
use crate::env::EnvMap;
use crate::module::AgentLink;

/// The events whose hooks drive agent state.
pub const HOOK_EVENTS: &[&str] = &[
    "SessionStart",
    "UserPromptSubmit",
    "PermissionRequest",
    "Notification",
    "Stop",
    "StopFailure",
    "SessionEnd",
];
/// Claude Code's own cap on each hook, in seconds; the forwarder gives up
/// well before it.
const HOOK_TIMEOUT_SECS: u64 = 2;
/// `claude agents --json` takes ~0.1 s; a hung one is given up on.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

pub struct Claude;

impl AgentAdapter for Claude {
    fn kind(&self) -> &'static str {
        "claude"
    }

    fn resolve(&self, env: &EnvMap) -> Result<PathBuf, String> {
        let path = env.get("PATH").map(String::as_str).unwrap_or_default();
        find(OsStr::new(path)).ok_or_else(|| "claude not found on PATH; install Claude Code".into())
    }

    /// A `command` hook on every event in [`HOOK_EVENTS`].
    fn hooks(&self, hook: &str) -> Value {
        let group = json!([{
            "hooks": [{ "type": "command", "command": hook, "timeout": HOOK_TIMEOUT_SECS }]
        }]);
        let hooks: serde_json::Map<String, Value> = HOOK_EVENTS
            .iter()
            .map(|event| (event.to_string(), group.clone()))
            .collect();
        json!({ "hooks": hooks })
    }

    /// Claude Code merges hook lists across settings levels, so the user's
    /// own hooks still run next to these.
    fn launch(&self, binary: &Path, hooks: &Path, link: Option<&AgentLink>) -> AgentCommand {
        let mut argv = vec![
            binary.display().to_string(),
            "--settings".into(),
            hooks.display().to_string(),
        ];
        if let Some(link) = link {
            argv.extend(["--resume".into(), link.agent_session.clone()]);
        }
        AgentCommand {
            argv,
            env: EnvMap::new(),
        }
    }

    fn ingest(&self, payload: &Value) -> Option<Ingested> {
        let field = |name: &str| payload.get(name).and_then(Value::as_str);
        let agent_session = field("session_id")?.to_string();
        let event = match field("hook_event_name")? {
            "SessionStart" => AgentEvent::Started {
                cwd: field("cwd")?.to_string(),
                source: field("source").unwrap_or("startup").to_string(),
            },
            "UserPromptSubmit" => AgentEvent::Busy,
            "PermissionRequest" => AgentEvent::Waiting {
                reason: WaitingReason::Permission,
            },
            "Notification" => match field("notification_type")? {
                "permission_prompt" => AgentEvent::Waiting {
                    reason: WaitingReason::Permission,
                },
                "elicitation_dialog" | "agent_needs_input" => AgentEvent::Waiting {
                    reason: WaitingReason::Input,
                },
                _ => return None,
            },
            "Stop" => AgentEvent::Idle { error: None },
            "StopFailure" => AgentEvent::Idle {
                error: Some(AgentError {
                    error: match payload.get("error") {
                        Some(Value::String(text)) => text.clone(),
                        Some(other) => other.to_string(),
                        None => "unknown".into(),
                    },
                    message: field("last_assistant_message")
                        .or(field("error_details"))
                        .map(String::from),
                }),
            },
            "SessionEnd" => AgentEvent::Ended {
                reason: field("reason").map(String::from),
            },
            _ => return None,
        };
        Some(Ingested {
            agent_session,
            event,
        })
    }

    fn probe(&self, env: &EnvMap) -> Option<Vec<LiveAgent>> {
        let claude = self.resolve(env).ok()?;
        let mut command = Command::new(claude);
        command
            .args(["agents", "--json"])
            .env_clear()
            .envs(env.iter().filter(|(key, _)| *key != "CLAUDECODE"))
            .current_dir(env.get("HOME").map_or("/", String::as_str));
        parse_registry(&output(command, PROBE_TIMEOUT)?)
    }
}

/// The first executable `claude` on `path`, as a shell would find it.
pub fn find(path: &OsStr) -> Option<PathBuf> {
    std::env::split_paths(path)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join("claude"))
        .find(|candidate| {
            std::fs::metadata(candidate)
                .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        })
}

/// `claude agents --json`: every Claude Code on the host. `pid` and `status`
/// are there only while the process lives, `sessionId` and `name` only once
/// set; entries without a pid are left out.
pub fn parse_registry(json: &[u8]) -> Option<Vec<LiveAgent>> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Entry {
        pid: Option<u32>,
        session_id: Option<String>,
        name: Option<String>,
        #[serde(default)]
        cwd: String,
        kind: Option<String>,
        status: Option<String>,
        waiting_for: Option<String>,
    }
    let entries: Vec<Value> = serde_json::from_slice(json).ok()?;
    Some(
        entries
            .into_iter()
            .filter_map(|entry| serde_json::from_value::<Entry>(entry).ok())
            .filter_map(|entry| {
                Some(LiveAgent {
                    pid: entry.pid?,
                    agent_session: entry.session_id,
                    name: entry.name.filter(|name| !name.is_empty()),
                    cwd: entry.cwd,
                    state: match entry.status?.as_str() {
                        "busy" => Some(AgentState::Busy),
                        "idle" => Some(AgentState::Idle),
                        "waiting" => Some(AgentState::Waiting {
                            reason: match entry.waiting_for.as_deref() {
                                Some("permission prompt") => WaitingReason::Permission,
                                Some("input needed") => WaitingReason::Input,
                                _ => WaitingReason::Other,
                            },
                        }),
                        _ => None,
                    },
                    background: entry.kind.as_deref() == Some("background"),
                })
            })
            .collect(),
    )
}

/// `command`'s stdout when it succeeds within `timeout`; it is killed otherwise.
fn output(mut command: Command, timeout: Duration) -> Option<Vec<u8>> {
    let deadline = Instant::now() + timeout;
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let (done, read) = mpsc::channel();
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = done.send(stdout.read_to_end(&mut bytes).map(|_| bytes));
    });
    let bytes = read
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .ok()
        .and_then(Result::ok);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return bytes.filter(|_| status.success()),
            Ok(None) if bytes.is_some() && Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(5));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_is_launched_with_its_hooks_file_and_resumes_by_argument() {
        let hooks = Claude.hooks("'/opt/it'\\''s/bitplane-sessiond' hook");
        for event in HOOK_EVENTS {
            assert_eq!(
                hooks["hooks"][event][0]["hooks"][0]["command"],
                r"'/opt/it'\''s/bitplane-sessiond' hook"
            );
        }

        let fresh = Claude.launch(Path::new("/bin/claude"), Path::new("/run/s1.json"), None);
        let resumed = Claude.launch(
            Path::new("/bin/claude"),
            Path::new("/run/s1.json"),
            Some(&AgentLink {
                agent_session: "a1".into(),
                cwd: "/r".into(),
            }),
        );

        assert_eq!(fresh.argv, ["/bin/claude", "--settings", "/run/s1.json"]);
        assert_eq!(
            resumed.argv,
            [
                "/bin/claude",
                "--settings",
                "/run/s1.json",
                "--resume",
                "a1"
            ]
        );
    }

    #[test]
    fn the_registry_lists_live_agents_only() {
        let json = br#"[
            {"id":"b1","cwd":"/old","kind":"background","startedAt":1,"sessionId":"s0","state":"done"},
            {"pid":11,"cwd":"/a","kind":"interactive","startedAt":1,"sessionId":"s1","name":"web-fix","status":"busy"},
            {"pid":12,"cwd":"/b","kind":"interactive","startedAt":1,"status":"waiting","waitingFor":"input needed"},
            {"pid":13,"cwd":"/c","kind":"interactive","startedAt":1,"status":"waiting","waitingFor":"sandbox request"},
            {"pid":14,"cwd":"/d","kind":"background","startedAt":1,"sessionId":"s4","status":"idle","state":"running"},
            {"pid":15,"cwd":"/e","kind":"interactive","startedAt":1,"status":"pondering"},
            {"pid":"bad"}
        ]"#;

        let live = parse_registry(json).unwrap();

        let summary: Vec<_> = live
            .iter()
            .map(|agent| {
                (
                    agent.pid,
                    agent.agent_session.as_deref(),
                    agent.state.clone(),
                    agent.background,
                )
            })
            .collect();
        assert_eq!(
            summary,
            [
                (11, Some("s1"), Some(AgentState::Busy), false),
                (
                    12,
                    None,
                    Some(AgentState::Waiting {
                        reason: WaitingReason::Input
                    }),
                    false
                ),
                (
                    13,
                    None,
                    Some(AgentState::Waiting {
                        reason: WaitingReason::Other
                    }),
                    false
                ),
                (14, Some("s4"), Some(AgentState::Idle), true),
                (15, None, None, false),
            ]
        );
        assert_eq!(live[0].name.as_deref(), Some("web-fix"));
        assert!(parse_registry(b"not json").is_none());
    }

    #[test]
    fn a_stop_failure_carries_the_error_and_what_claude_showed() {
        let ingested = Claude
            .ingest(&json!({
                "hook_event_name": "StopFailure",
                "session_id": "a1",
                "error": "rate_limit",
                "error_details": "429 Too Many Requests",
                "last_assistant_message": "API Error: Rate limit reached",
            }))
            .unwrap();

        assert_eq!(
            ingested.event,
            AgentEvent::Idle {
                error: Some(AgentError {
                    error: "rate_limit".into(),
                    message: Some("API Error: Rate limit reached".into()),
                })
            }
        );
    }
}
