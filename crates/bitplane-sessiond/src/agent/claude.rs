//! The Claude Code adapter: a `claude` wrapper on each session's `PATH` that
//! adds a `--settings` with our hooks (ADR 0029), and what those hooks mean
//! (ADR 0030).

use std::ffi::OsStr;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use bitplane_protocol::agent::WaitingReason;
use serde_json::{Value, json};

use super::{AgentAdapter, AgentEvent, Ingested, LiveAgent};
use crate::env::EnvMap;
use crate::module::{Availability, Instrumentation};

/// The generated settings file the wrapper passes to `claude`.
pub const SETTINGS_VAR: &str = "BITPLANE_CLAUDE_SETTINGS";
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
const BIN_DIR: &str = "bin";
const SETTINGS_FILE: &str = "settings.json";

pub struct Claude;

impl AgentAdapter for Claude {
    fn kind(&self) -> &'static str {
        "claude"
    }

    fn namespace(&self) -> &'static str {
        "agent.claude"
    }

    fn available(&self, env: &EnvMap) -> Availability {
        let path = env.get("PATH").map(String::as_str).unwrap_or_default();
        match find(OsStr::new(path), None) {
            Some(claude) => Availability::Available {
                info: Some(json!({ "path": claude })),
            },
            None => Availability::Unavailable {
                reason: "claude not found on PATH; install Claude Code and restart the daemon"
                    .into(),
            },
        }
    }

    fn install(&self, dir: &Path, exe: &Path) -> std::io::Result<()> {
        let bin = dir.join(BIN_DIR);
        std::fs::create_dir_all(&bin)?;
        let link = bin.join("claude");
        match std::fs::remove_file(&link) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => return Err(error),
            _ => {}
        }
        std::os::unix::fs::symlink(exe, &link)?;
        let settings = serde_json::to_vec_pretty(&settings(exe)).expect("settings serialize");
        write_atomic(&dir.join(SETTINGS_FILE), &settings)
    }

    fn instrument(&self, dir: &Path) -> Instrumentation {
        Instrumentation {
            env: EnvMap::from([(
                SETTINGS_VAR.into(),
                dir.join(SETTINGS_FILE).display().to_string(),
            )]),
            path_prepend: vec![dir.join(BIN_DIR)],
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
                error: payload.get("error").map(|error| match error {
                    Value::String(text) => text.clone(),
                    other => other.to_string(),
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

    fn probe(&self) -> Vec<LiveAgent> {
        Vec::new()
    }

    fn resume_command(&self, agent_session: &str) -> String {
        format!("claude --resume {agent_session}\r")
    }
}

/// The first `claude` on `path` that is not a daemon's wrapper: neither
/// `own` (the running binary, canonical) nor any `bitplane-sessiond*` binary.
pub fn find(path: &OsStr, own: Option<&Path>) -> Option<PathBuf> {
    std::env::split_paths(path)
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(|dir| dir.join("claude"))
        .find(|candidate| {
            let Ok(target) = candidate.canonicalize() else {
                return false;
            };
            let executable = std::fs::metadata(&target)
                .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0);
            let wrapper = Some(target.as_path()) == own
                || target
                    .file_name()
                    .and_then(OsStr::to_str)
                    .is_some_and(|name| name.starts_with(crate::DAEMON_NAME));
            executable && !wrapper
        })
}

/// A `command` hook on every event in [`HOOK_EVENTS`], running `exe hook`.
pub fn settings(exe: &Path) -> Value {
    let command = format!("{} hook", shell_quote(&exe.display().to_string()));
    let group = json!([{
        "hooks": [{ "type": "command", "command": command, "timeout": HOOK_TIMEOUT_SECS }]
    }]);
    let hooks: serde_json::Map<String, Value> = HOOK_EVENTS
        .iter()
        .map(|event| (event.to_string(), group.clone()))
        .collect();
    json!({ "hooks": hooks })
}

/// `user`'s settings with `ours`' hook groups appended per event; `None`
/// when `user` is not an object or its `hooks` are malformed.
pub fn merge(user: Value, ours: &Value) -> Option<Value> {
    let Value::Object(mut merged) = user else {
        return None;
    };
    let hooks = merged
        .entry("hooks")
        .or_insert_with(|| json!({}))
        .as_object_mut()?;
    for (event, groups) in ours.get("hooks")?.as_object()? {
        hooks
            .entry(event.clone())
            .or_insert_with(|| json!([]))
            .as_array_mut()?
            .extend(groups.as_array()?.iter().cloned());
    }
    Some(Value::Object(merged))
}

fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("json.tmp");
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merging_keeps_the_users_settings_and_hooks_before_ours() {
        let user = json!({
            "model": "opus",
            "hooks": { "Stop": [{ "hooks": [{ "type": "command", "command": "notify" }] }] },
        });
        let ours = settings(Path::new("/opt/it's/bitplane-sessiond"));

        let merged = merge(user, &ours).unwrap();

        assert_eq!(merged["model"], "opus");
        let stop = merged["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 2);
        assert_eq!(stop[0]["hooks"][0]["command"], "notify");
        assert_eq!(
            stop[1]["hooks"][0]["command"],
            r"'/opt/it'\''s/bitplane-sessiond' hook"
        );
        assert_eq!(
            merged["hooks"]["SessionStart"],
            ours["hooks"]["SessionStart"]
        );
    }

    #[test]
    fn malformed_user_settings_are_not_merged() {
        let ours = settings(Path::new("/bin/x"));
        assert!(merge(json!([1]), &ours).is_none());
        assert!(merge(json!({ "hooks": [] }), &ours).is_none());
        assert!(merge(json!({ "hooks": { "Stop": {} } }), &ours).is_none());
    }
}
