//! `bitplane-sessiond` invoked as `claude` (ADR 0029): runs the next `claude`
//! on `PATH`, with the session's hooks added through `--settings`.

use std::ffi::{OsStr, OsString};
use std::os::unix::process::CommandExt;
use std::path::Path;

use serde_json::Value;

use super::claude;

const SETTINGS_FLAG: &str = "--settings";

/// Whether the process was started through the `claude` symlink.
pub fn invoked() -> bool {
    std::env::args_os()
        .next()
        .is_some_and(|arg0| Path::new(&arg0).file_name() == Some(OsStr::new("claude")))
}

/// Never returns: it becomes the real `claude`, or exits as a shell would
/// when it can't.
pub fn run() -> ! {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let own = std::env::current_exe()
        .and_then(|exe| exe.canonicalize())
        .ok();
    let path = std::env::var_os("PATH").unwrap_or_default();
    let Some(real) = claude::find(&path, own.as_deref()) else {
        eprintln!("claude: command not found");
        std::process::exit(127);
    };
    let args = injected(&args).unwrap_or(args);
    let error = std::process::Command::new(&real).args(&args).exec();
    eprintln!("claude: couldn't run {}: {error}", real.display());
    std::process::exit(126);
}

/// `args` with the session's hooks added, or `None` to run them unchanged:
/// inside a Claude Code (`CLAUDECODE=1`), outside a session, or when the
/// user's own `--settings` can't be read (claude then reports it).
pub fn injected(args: &[OsString]) -> Option<Vec<OsString>> {
    if std::env::var_os("CLAUDECODE").is_some_and(|value| value == "1") {
        return None;
    }
    let ours = std::env::var_os(claude::SETTINGS_VAR).filter(|path| !path.is_empty())?;
    let (rest, user) = split_settings(args)?;
    let settings = match user {
        None => ours,
        Some(user) => {
            let ours: Value = serde_json::from_slice(&std::fs::read(&ours).ok()?).ok()?;
            claude::merge(read_user_settings(&user)?, &ours)?
                .to_string()
                .into()
        }
    };
    let mut injected = vec![OsString::from(SETTINGS_FLAG), settings];
    injected.extend(rest);
    Some(injected)
}

/// The args without any `--settings`, and the last one's value, as claude
/// takes only the last. `None` when a `--settings` has no value.
fn split_settings(args: &[OsString]) -> Option<(Vec<OsString>, Option<OsString>)> {
    let mut rest = Vec::with_capacity(args.len());
    let mut user = None;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        if arg == "--" {
            rest.push(arg.clone());
            rest.extend(args.by_ref().cloned());
            break;
        }
        if arg == SETTINGS_FLAG {
            user = Some(args.next()?.clone());
            continue;
        }
        if let Some(value) = arg.to_str().and_then(|arg| arg.strip_prefix("--settings=")) {
            user = Some(value.into());
            continue;
        }
        rest.push(arg.clone());
    }
    Some((rest, user))
}

/// A `--settings` value: inline JSON, or a file path relative to the cwd.
fn read_user_settings(value: &OsStr) -> Option<Value> {
    let text = value.to_str()?;
    if text.trim_start().starts_with('{') {
        return serde_json::from_str(text).ok();
    }
    serde_json::from_slice(&std::fs::read(text).ok()?).ok()
}
