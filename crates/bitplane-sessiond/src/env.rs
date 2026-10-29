//! The login-shell environment, captured once at startup.
//!
//! A daemon started from a GUI app, launchd or systemd doesn't inherit the
//! user's shell setup, so it asks a login + interactive shell for it.

use std::collections::BTreeMap;
use std::ffi::CStr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;

pub type EnvMap = BTreeMap<String, String>;

/// How long the shell may take to print its environment.
pub const CAPTURE_TIMEOUT: Duration = Duration::from_secs(10);

const MARKER: &str = "__BITPLANE_SESSIOND_ENV_7f3a__";

/// The process's own environment.
pub fn original() -> EnvMap {
    std::env::vars_os()
        .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
        .collect()
}

/// The login + interactive shell environment, merged over `base`.
///
/// On error or timeout it falls back to `base` and says why.
pub async fn capture(base: &EnvMap, timeout: Duration) -> (EnvMap, Option<String>) {
    let shell = resolve_shell(base.get("SHELL").map(String::as_str));
    match run_capture(&shell, base, timeout).await {
        Ok(captured) => {
            let mut vars = base.clone();
            vars.extend(captured);
            (vars, None)
        }
        Err(error) => (base.clone(), Some(format!("{}: {error}", shell.display()))),
    }
}

/// `$SHELL`, then the account database's login shell, then `/bin/sh`.
pub fn resolve_shell(shell_var: Option<&str>) -> PathBuf {
    if let Some(shell) = shell_var.filter(|s| !s.is_empty()) {
        return PathBuf::from(shell);
    }
    login_shell_from_passwd().unwrap_or_else(|| PathBuf::from("/bin/sh"))
}

/// `env -0` output printed between two markers. rc files may print banners
/// before or after, so only the text between the first two markers counts.
pub fn parse_env_output(output: &[u8]) -> Option<EnvMap> {
    let text = String::from_utf8_lossy(output);
    let start = text.find(MARKER)? + MARKER.len();
    let len = text[start..].find(MARKER)?;
    let body = &text[start..start + len];
    Some(
        body.split('\0')
            .filter_map(|entry| {
                let (key, value) = entry.split_once('=')?;
                (!key.is_empty() && !key.contains('\n')).then(|| (key.into(), value.into()))
            })
            .collect(),
    )
}

async fn run_capture(shell: &Path, base: &EnvMap, timeout: Duration) -> Result<EnvMap, String> {
    let script = format!("printf '%s' '{MARKER}'; env -0; printf '%s' '{MARKER}'");
    let mut command = Command::new(shell);
    command
        .args(["-l", "-i", "-c", &script])
        .env_clear()
        .envs(base)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    // No controlling terminal, so an rc file can never prompt on one.
    // SAFETY: setsid is async-signal-safe and touches no parent state.
    unsafe {
        command.pre_exec(|| rustix::process::setsid().map(drop).map_err(Into::into));
    }

    let output = tokio::time::timeout(timeout, command.output())
        .await
        .map_err(|_| format!("timed out after {}s", timeout.as_secs()))?
        .map_err(|e| e.to_string())?;
    parse_env_output(&output.stdout).ok_or_else(|| "no environment in shell output".into())
}

fn login_shell_from_passwd() -> Option<PathBuf> {
    // SAFETY: getpwuid returns a pointer into static storage or null; the
    // shell string is copied out before anything else can call it.
    unsafe {
        let entry = libc::getpwuid(libc::getuid());
        if entry.is_null() || (*entry).pw_shell.is_null() {
            return None;
        }
        let shell = CStr::from_ptr((*entry).pw_shell).to_str().ok()?;
        (!shell.is_empty()).then(|| PathBuf::from(shell))
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    #[test]
    fn parses_env_between_markers_ignoring_banners() {
        let output = format!("motd\n{MARKER}A=1\0PATH=/bin:/usr/bin\0MULTI=x\ny\0{MARKER}bye");
        let env = parse_env_output(output.as_bytes()).unwrap();
        assert_eq!(env["A"], "1");
        assert_eq!(env["PATH"], "/bin:/usr/bin");
        assert_eq!(env["MULTI"], "x\ny");
    }

    #[test]
    fn keeps_equals_signs_in_values() {
        let output = format!("{MARKER}OPTS=a=b=c\0{MARKER}");
        assert_eq!(
            parse_env_output(output.as_bytes()).unwrap()["OPTS"],
            "a=b=c"
        );
    }

    #[test]
    fn missing_markers_is_none() {
        assert!(parse_env_output(b"A=1\0").is_none());
        assert!(parse_env_output(format!("{MARKER}A=1\0").as_bytes()).is_none());
    }

    #[test]
    fn shell_falls_back_when_unset() {
        assert_eq!(resolve_shell(Some("/bin/zsh")), PathBuf::from("/bin/zsh"));
        let fallback = resolve_shell(Some(""));
        assert!(fallback.is_absolute());
        assert_eq!(resolve_shell(None), fallback);
    }

    #[tokio::test]
    async fn captures_what_the_login_shell_sets_up() {
        let dir = tempfile::tempdir().unwrap();
        let shell = dir.path().join("login-shell");
        std::fs::write(
            &shell,
            // Runs the `-c` script itself, so the host's profile files stay out.
            "#!/bin/sh\n[ \"$1 $2 $3\" = \"-l -i -c\" ] || exit 2\nexport FROM_PROFILE=yes\neval \"$4\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut base = original();
        base.insert("SHELL".into(), shell.display().to_string());
        base.insert("FROM_BASE".into(), "kept".into());

        let (vars, warning) = capture(&base, CAPTURE_TIMEOUT).await;

        assert_eq!(warning, None);
        assert_eq!(vars["FROM_PROFILE"], "yes");
        assert_eq!(vars["FROM_BASE"], "kept");
    }

    #[tokio::test]
    async fn falls_back_with_a_warning_when_the_shell_is_missing() {
        let mut base = EnvMap::new();
        base.insert("SHELL".into(), "/nonexistent/shell".into());

        let (vars, warning) = capture(&base, CAPTURE_TIMEOUT).await;

        assert!(warning.is_some());
        assert_eq!(vars, base);
    }
}
