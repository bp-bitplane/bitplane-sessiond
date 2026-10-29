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
}
