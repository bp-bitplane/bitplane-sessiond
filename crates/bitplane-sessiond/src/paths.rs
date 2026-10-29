use std::path::{Path, PathBuf};

use bitplane_protocol::PROTOCOL_VERSION;

/// One daemon instance's directories (ADR 0020): `runtime` holds its socket
/// and pidfile, `state` what it persists. Both are keyed by protocol version.
#[derive(Debug, Clone)]
pub struct Dirs {
    pub runtime: PathBuf,
    pub state: PathBuf,
}

impl Dirs {
    /// The directories every client and daemon of this version agree on.
    pub fn for_host() -> Result<Self, String> {
        let env = |name| {
            std::env::var_os(name)
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
        };
        let runtime = env("XDG_RUNTIME_DIR")
            .or_else(|| env("TMPDIR"))
            .or_else(darwin_user_temp_dir)
            .ok_or("neither XDG_RUNTIME_DIR nor TMPDIR is set")?;
        let state = env("XDG_STATE_HOME")
            .or_else(|| env("HOME").map(|home| home.join(".local/state")))
            .ok_or("neither XDG_STATE_HOME nor HOME is set")?;
        Ok(Self::for_version(&runtime, &state))
    }

    /// `runtime_base` is `$XDG_RUNTIME_DIR` (`$TMPDIR` on macOS), `state_base`
    /// is `$XDG_STATE_HOME`.
    pub fn for_version(runtime_base: &Path, state_base: &Path) -> Self {
        let version = PROTOCOL_VERSION.to_string();
        let relative = Path::new("bitplane").join("sessiond").join(version);
        Self {
            runtime: runtime_base.join(&relative),
            state: state_base.join(relative),
        }
    }

    pub fn socket(&self) -> PathBuf {
        self.runtime.join("sessiond.sock")
    }

    pub fn pidfile(&self) -> PathBuf {
        self.runtime.join("sessiond.pid")
    }

    /// Where an auto-started daemon's stdout and stderr go. Under `state`,
    /// so it outlives a reboot that clears the runtime dir.
    pub fn log(&self) -> PathBuf {
        self.state.join("sessiond.log")
    }
}

/// launchd agents start without `TMPDIR`; this is the dir it would name.
#[cfg(target_os = "macos")]
fn darwin_user_temp_dir() -> Option<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    let mut buf = vec![0u8; 1024];
    // SAFETY: confstr writes at most `buf.len()` bytes, NUL included.
    let len = unsafe {
        libc::confstr(
            libc::_CS_DARWIN_USER_TEMP_DIR,
            buf.as_mut_ptr().cast(),
            buf.len(),
        )
    };
    (len > 1 && len <= buf.len())
        .then(|| PathBuf::from(std::ffi::OsStr::from_bytes(&buf[..len - 1])))
}

#[cfg(not(target_os = "macos"))]
fn darwin_user_temp_dir() -> Option<PathBuf> {
    None
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    #[test]
    fn launchd_agents_find_the_same_temp_dir_as_a_login_session() {
        let dir = super::darwin_user_temp_dir().unwrap();
        assert!(dir.is_dir());
        if let Some(tmpdir) = std::env::var_os("TMPDIR") {
            assert_eq!(
                dir.canonicalize().unwrap(),
                std::path::Path::new(&tmpdir).canonicalize().unwrap()
            );
        }
    }
}
