//! Running `bp` (ADR 0033): every run is `bp --stdin --json` with one
//! `Request` on stdin, in the captured environment.

use std::ffi::OsStr;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use bitplane_protocol::plane::BpVersion;
use bitplane_protocol::{ErrorCode, RpcError};
use serde_json::Value;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::env::EnvMap;

pub const NO_BP: &str = "bp not found on PATH; install it and restart the daemon";

const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const TAIL_LINES: usize = 20;
const TAIL_BYTES: usize = 2000;

pub struct Bp {
    program: PathBuf,
    env: EnvMap,
    planes_dir: PathBuf,
}

/// Why a run gave no `Response`.
#[derive(Debug, Clone, PartialEq)]
pub enum Failure {
    /// `bp` ran and said no: its error envelope, verbatim.
    Refused(Value),
    /// `bp` did not run, died, or answered nothing parseable; the cause and
    /// the tail of its stderr.
    NotRun(String),
}

impl Bp {
    /// The first executable `bp` on `env`'s `PATH`.
    pub fn find(env: &EnvMap) -> Option<PathBuf> {
        let path = env.get("PATH").map(String::as_str).unwrap_or_default();
        std::env::split_paths(OsStr::new(path))
            .filter(|dir| dir.is_absolute())
            .map(|dir| dir.join("bp"))
            .find(|candidate| {
                std::fs::metadata(candidate)
                    .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
            })
    }

    pub fn new(program: PathBuf, env: &EnvMap, planes_dir: PathBuf) -> Bp {
        Bp {
            program,
            env: crate::git_cli::no_prompts(env),
            planes_dir,
        }
    }

    pub fn planes_dir(&self) -> &Path {
        &self.planes_dir
    }

    /// `bp --version --json`, once at startup; the error is the reason the
    /// module is unavailable.
    pub fn version(&self) -> Result<BpVersion, String> {
        let mut command = std::process::Command::new(&self.program);
        command
            .args(["--version", "--json"])
            .env_clear()
            .envs(&self.env);
        let fix = "update bp to one that takes requests on stdin and restart the daemon";
        let output = crate::agent::claude::output(command, PROBE_TIMEOUT).ok_or_else(|| {
            format!(
                "`{} --version --json` failed; {fix}",
                self.program.display()
            )
        })?;
        serde_json::from_slice(&output).map_err(|error| {
            format!(
                "`{} --version --json` printed no version ({error}); {fix}",
                self.program.display()
            )
        })
    }

    /// One request. A run is never killed: a dropped caller only loses the
    /// answer, as a `bp` stopped mid-mutation would leave work behind.
    pub async fn run(&self, request: &Value) -> Result<Value, Failure> {
        let mut command = Command::new(&self.program);
        command
            .arg("--stdin")
            .arg("--json")
            .arg("--planes-dir")
            .arg(&self.planes_dir)
            .env_clear()
            .envs(&self.env)
            .current_dir(self.env.get("HOME").map_or("/", String::as_str))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // SAFETY: setsid is async-signal-safe and touches no parent state.
        unsafe {
            command.pre_exec(|| rustix::process::setsid().map(drop).map_err(Into::into));
        }
        let mut child = command
            .spawn()
            .map_err(|error| Failure::NotRun(format!("couldn't start bp: {error}")))?;
        if let Some(mut stdin) = child.stdin.take() {
            let bytes = serde_json::to_vec(request).expect("JSON values serialize");
            // A bp that exits without reading is judged by its answer below.
            let _ = stdin.write_all(&bytes).await;
        }
        let output = child
            .wait_with_output()
            .await
            .map_err(|error| Failure::NotRun(format!("couldn't wait for bp: {error}")))?;
        answer(
            output.status.code(),
            &output.stdout,
            &String::from_utf8_lossy(&output.stderr),
        )
    }
}

impl Failure {
    pub fn into_rpc(self) -> RpcError {
        match self {
            Failure::Refused(envelope) => {
                let message = envelope
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("bp refused")
                    .to_string();
                RpcError::protocol(ErrorCode::BpRefused, message, Some(envelope))
            }
            Failure::NotRun(cause) => RpcError::new(
                RpcError::INTERNAL_ERROR,
                format!("bp could not be run: {cause}"),
            ),
        }
    }
}

/// Judges a finished run: `0` and `3` with a `Response` are the answer, any
/// other exit with an envelope a refusal, everything else `NotRun`.
pub fn answer(code: Option<i32>, stdout: &[u8], stderr: &str) -> Result<Value, Failure> {
    let (log, envelope) = split_envelope(stderr);
    let not_run = |cause: String| {
        let tail = tail(if envelope.is_some() { stderr } else { &log });
        Failure::NotRun(if tail.is_empty() {
            cause
        } else {
            format!("{cause}; stderr: {tail}")
        })
    };
    match code {
        Some(0 | 3) => serde_json::from_slice(stdout).map_err(|error| {
            not_run(format!(
                "bp exited {} without a response it could parse: {error}",
                code.unwrap_or_default()
            ))
        }),
        Some(code) => match envelope {
            Some(envelope) => Err(Failure::Refused(envelope)),
            None => Err(not_run(format!(
                "bp exited {code} without an error envelope"
            ))),
        },
        None => Err(not_run("bp was killed by a signal".into())),
    }
}

/// stderr without the envelope, and the envelope: the last line, from its
/// last `{"error":`, since script output without a newline can precede it.
pub fn split_envelope(stderr: &str) -> (String, Option<Value>) {
    let trimmed = stderr.trim_end_matches(['\n', '\r']);
    let line_start = trimmed.rfind('\n').map_or(0, |i| i + 1);
    let last_line = &trimmed[line_start..];
    if let Some(offset) = last_line.rfind("{\"error\":")
        && let Ok(envelope) = serde_json::from_str::<Value>(&last_line[offset..])
        && envelope.get("error").is_some_and(Value::is_string)
    {
        return (trimmed[..line_start + offset].to_string(), Some(envelope));
    }
    (stderr.to_string(), None)
}

fn tail(text: &str) -> String {
    let text = text.trim();
    let lines: Vec<&str> = text.lines().collect();
    let joined = lines[lines.len().saturating_sub(TAIL_LINES)..].join("\n");
    match joined.char_indices().rev().nth(TAIL_BYTES) {
        Some((cut, _)) => format!("…{}", &joined[cut + 1..]),
        None => joined,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const ENVELOPE: &str =
        r#"{"error":"lock_timeout","code":5,"message":"busy","problems":[],"remedy":null}"#;

    #[test]
    fn exits_zero_and_three_answer_with_the_response() {
        for code in [0, 3] {
            let answer = answer(Some(code), br#"{"action":"plane_list","planes":[]}"#, "");
            assert_eq!(answer, Ok(json!({"action": "plane_list", "planes": []})));
        }
    }

    #[test]
    fn any_other_exit_with_an_envelope_is_refused_verbatim() {
        let stderr = format!("waiting for /p…\nscript output{ENVELOPE}\n");

        let answer = answer(Some(5), b"", &stderr);

        assert_eq!(
            answer,
            Err(Failure::Refused(serde_json::from_str(ENVELOPE).unwrap()))
        );
    }

    #[test]
    fn no_envelope_garbage_or_a_signal_is_not_run_with_the_stderr_tail() {
        let Err(Failure::NotRun(cause)) = answer(Some(1), b"", "panicked at x\n") else {
            panic!()
        };
        assert!(cause.contains("exited 1") && cause.ends_with("stderr: panicked at x"));

        let Err(Failure::NotRun(cause)) = answer(Some(0), b"not json", "") else {
            panic!()
        };
        assert!(cause.contains("without a response"), "{cause}");

        assert!(matches!(answer(None, b"", ""), Err(Failure::NotRun(_))));
    }

    #[test]
    fn script_output_that_looks_like_an_envelope_earlier_is_log() {
        let stderr = format!("{{\"error\": fake\n{ENVELOPE}");

        let (log, envelope) = split_envelope(&stderr);

        assert_eq!(log, "{\"error\": fake\n");
        assert!(envelope.is_some());
    }

    #[test]
    fn the_tail_keeps_the_last_lines() {
        let text: String = (0..50).map(|i| format!("line {i}\n")).collect();

        let tail = tail(&text);

        assert!(tail.starts_with("line 30\n") && tail.ends_with("line 49"));
    }
}
