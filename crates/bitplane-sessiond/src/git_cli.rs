//! Running the git CLI (ADR 0032), the one way the daemon reads git.

use std::ffi::OsStr;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};

use tokio::process::Command;

use crate::env::EnvMap;

pub struct Git {
    program: PathBuf,
    env: EnvMap,
}

impl Git {
    /// `None` when git isn't on `env`'s `PATH`.
    pub fn find(env: &EnvMap) -> Option<Git> {
        let path = env.get("PATH").map(String::as_str).unwrap_or_default();
        let program = std::env::split_paths(OsStr::new(path))
            .filter(|dir| dir.is_absolute())
            .map(|dir| dir.join("git"))
            .find(|candidate| {
                std::fs::metadata(candidate)
                    .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
            })?;
        Some(Git {
            program,
            env: no_prompts(env),
        })
    }

    /// Only the captured environment, no stdin, output piped, killed when
    /// dropped so an abandoned read doesn't linger.
    pub fn command(&self, dir: &Path, args: &[&str]) -> Command {
        let mut command = Command::new(&self.program);
        command
            .args(args)
            .current_dir(dir)
            .env_clear()
            .envs(&self.env)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        command
    }

    /// Stdout of a run that exits 0 or with one of `also_ok`.
    pub async fn run(&self, dir: &Path, args: &[&str], also_ok: &[i32]) -> Result<Vec<u8>, String> {
        let output = self
            .command(dir, args)
            .output()
            .await
            .map_err(|error| format!("couldn't run git: {error}"))?;
        let ok = output.status.success()
            || output
                .status
                .code()
                .is_some_and(|code| also_ok.contains(&code));
        if ok {
            Ok(output.stdout)
        } else {
            Err(failure(&output))
        }
    }
}

/// Git's own stderr, or the exit status when it said nothing.
pub fn failure(output: &Output) -> String {
    match String::from_utf8_lossy(&output.stderr).trim() {
        "" => format!("git exited with {}", output.status),
        message => message.to_string(),
    }
}

pub(crate) fn no_prompts(env: &EnvMap) -> EnvMap {
    let mut env = env.clone();
    env.insert("GIT_TERMINAL_PROMPT".into(), "0".into());
    let ssh = match env.get("GIT_SSH_COMMAND").filter(|s| !s.trim().is_empty()) {
        Some(theirs) => format!("{theirs} -o BatchMode=yes"),
        None => "ssh -o BatchMode=yes".into(),
    };
    env.insert("GIT_SSH_COMMAND".into(), ssh);
    env
}
