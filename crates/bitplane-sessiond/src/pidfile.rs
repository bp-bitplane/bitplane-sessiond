use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use rustix::fs::{FlockOperation, flock};

/// The instance's claim (ADR 0022): held for the daemon's lifetime and
/// released by the kernel when the process dies. The file is never deleted,
/// since unlinking it would let a second starter lock a fresh inode.
pub struct Pidfile {
    _file: File,
}

#[derive(Debug)]
pub enum PidfileError {
    /// Another instance of this version holds the lock.
    Held,
    Io(io::Error),
}

impl Pidfile {
    pub fn acquire(path: &Path) -> Result<Self, PidfileError> {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(path)
            .map_err(PidfileError::Io)?;
        match flock(&file, FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => {}
            Err(rustix::io::Errno::WOULDBLOCK) => return Err(PidfileError::Held),
            Err(errno) => return Err(PidfileError::Io(errno.into())),
        }
        file.set_len(0).map_err(PidfileError::Io)?;
        writeln!(file, "{}", std::process::id()).map_err(PidfileError::Io)?;
        Ok(Self { _file: file })
    }
}

impl std::fmt::Display for PidfileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PidfileError::Held => write!(f, "another daemon instance of this version is running"),
            PidfileError::Io(error) => write!(f, "pidfile: {error}"),
        }
    }
}

impl std::error::Error for PidfileError {}
