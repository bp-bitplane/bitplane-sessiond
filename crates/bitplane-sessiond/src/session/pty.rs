//! A session's process on a PTY, with its reader, writer and waiter threads:
//! the login shell, or the agent of an agent terminal.

use std::io::{Read, Write};
use std::path::Path;
use std::sync::mpsc;

use bitplane_protocol::session::Size;
use portable_pty::{ChildKiller, CommandBuilder, MasterPty, NativePtySystem, PtySize, PtySystem};

use crate::env::EnvMap;

use super::stream::Command;

/// Set by a Claude Code session for its children. A shell that inherits them
/// makes the claude started in it a child session, which saves nothing to
/// resume (cc's `PARENT_SESSION_VARS`). User settings such as
/// `CLAUDE_CONFIG_DIR` are deliberately not listed.
pub const PARENT_SESSION_VARS: &[&str] = &[
    "AI_AGENT",
    "CLAUDECODE",
    "CLAUDE_AGENT_SDK_VERSION",
    "CLAUDE_CODE_CHILD_SESSION",
    "CLAUDE_CODE_ENTRYPOINT",
    "CLAUDE_CODE_EXECPATH",
    "CLAUDE_CODE_MESSAGING_SOCKET",
    "CLAUDE_CODE_MESSAGING_TOKEN",
    "CLAUDE_CODE_SESSION_ATTENDED",
    "CLAUDE_CODE_SESSION_ID",
    "CLAUDE_EFFORT",
    "CLAUDE_PID",
];

pub struct Pty {
    master: Box<dyn MasterPty + Send>,
    input: mpsc::Sender<Vec<u8>>,
    killer: Box<dyn ChildKiller + Send + Sync>,
    shell_pid: Option<u32>,
}

/// The terminal's foreground process group, as cc's busy check sees it.
pub struct Foreground {
    pub name: Option<String>,
    pub group: u32,
    /// The group is not the shell's.
    pub busy: bool,
}

/// What the PTY child is started with.
pub struct Spawn<'a> {
    /// The program, then its arguments.
    pub argv: &'a [String],
    /// The session root, or where its agent starts.
    pub cwd: &'a Path,
    pub env: EnvMap,
    pub size: Size,
    /// Tags this PTY's output, so a restarted session's stream drops the old one's.
    pub generation: u64,
    /// Gets the shell's output, then nothing once the PTY closes.
    pub output: mpsc::Sender<Command>,
}

impl Pty {
    /// Starts the child; `exited` runs on its own thread with the exit code.
    pub fn spawn(
        spawn: Spawn<'_>,
        exited: impl FnOnce(i32) + Send + 'static,
    ) -> Result<Self, String> {
        let pair = NativePtySystem::default()
            .openpty(pty_size(spawn.size))
            .map_err(|e| format!("couldn't open a terminal: {e}"))?;
        let (program, args) = spawn.argv.split_first().ok_or("nothing to start")?;
        let mut command = CommandBuilder::new(program);
        command.args(args);
        command.cwd(spawn.cwd);
        command.env_clear();
        for (key, value) in &spawn.env {
            command.env(key, value);
        }
        warm_up_fork();
        let mut child = pair
            .slave
            .spawn_command(command)
            .map_err(|e| format!("couldn't start {program}: {e}"))?;
        drop(pair.slave);
        let reader = pair.master.try_clone_reader().map_err(|e| e.to_string())?;
        let writer = pair.master.take_writer().map_err(|e| e.to_string())?;
        let killer = child.clone_killer();
        let shell_pid = child.process_id();

        let (input, inputs) = mpsc::channel();
        let generation = spawn.generation;
        spawn_thread("pty-read", move || read(reader, generation, spawn.output));
        spawn_thread("pty-write", move || write(writer, inputs));
        spawn_thread("pty-wait", move || {
            let code = child
                .wait()
                .map(|status| status.exit_code() as i32)
                .unwrap_or(-1);
            exited(code);
        });
        Ok(Self {
            master: pair.master,
            input,
            killer,
            shell_pid,
        })
    }

    pub fn foreground(&self) -> Option<Foreground> {
        let leader = self.master.process_group_leader()?;
        Some(Foreground {
            name: process_name(leader),
            group: leader as u32,
            busy: self
                .shell_pid
                .is_some_and(|shell| shell as libc::pid_t != leader),
        })
    }

    /// The PTY child's: the shell, or an agent terminal's agent.
    pub fn shell_pid(&self) -> Option<u32> {
        self.shell_pid
    }

    /// Queues one INPUT frame; the writer thread writes it whole.
    pub fn write(&self, data: Vec<u8>) {
        let _ = self.input.send(data);
    }

    pub fn resize(&self, size: Size) {
        let _ = self.master.resize(pty_size(size));
    }

    /// Resizes away and back, so the program gets SIGWINCH and redraws.
    pub fn nudge(&self) {
        if let Ok(size) = self.master.get_size() {
            let taller = PtySize {
                rows: size.rows.saturating_add(1),
                ..size
            };
            let _ = self.master.resize(taller);
            let _ = self.master.resize(size);
        }
    }

    /// SIGHUPs the shell.
    pub fn hang_up(mut self) {
        let _ = self.killer.kill();
        std::thread::spawn(move || drop(self));
    }
}

fn read(mut reader: Box<dyn Read + Send>, generation: u64, output: mpsc::Sender<Command>) {
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        match reader.read(&mut buf) {
            Ok(0) | Err(_) => return,
            Ok(n) => {
                let data = buf[..n].to_vec();
                if output.send(Command::Output { generation, data }).is_err() {
                    return;
                }
            }
        }
    }
}

fn write(mut writer: Box<dyn Write + Send>, inputs: mpsc::Receiver<Vec<u8>>) {
    while let Ok(data) = inputs.recv() {
        if writer
            .write_all(&data)
            .and_then(|()| writer.flush())
            .is_err()
        {
            return;
        }
    }
}

#[cfg(target_os = "macos")]
fn process_name(pid: libc::pid_t) -> Option<String> {
    let mut buf = [0u8; 256];
    // SAFETY: proc_name writes at most `buf.len()` bytes into `buf`.
    let len = unsafe { libc::proc_name(pid, buf.as_mut_ptr().cast(), buf.len() as u32) };
    (len > 0).then(|| String::from_utf8_lossy(&buf[..len as usize]).into_owned())
}

#[cfg(not(target_os = "macos"))]
fn process_name(pid: libc::pid_t) -> Option<String> {
    let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).ok()?;
    Some(comm.trim_end().to_string()).filter(|name| !name.is_empty())
}

fn pty_size(size: Size) -> PtySize {
    PtySize {
        rows: size.rows.max(1),
        cols: size.cols.max(1),
        pixel_width: 0,
        pixel_height: 0,
    }
}

/// macOS SIGKILLs a child forked while another thread makes the process's
/// first `getpwuid` call: `_notify_fork_child` finds libnotify's init half done.
fn warm_up_fork() {
    static WARM: std::sync::Once = std::sync::Once::new();
    // SAFETY: the returned entry is not read.
    WARM.call_once(|| unsafe {
        libc::getpwuid(libc::getuid());
    });
}

fn spawn_thread(name: &str, run: impl FnOnce() + Send + 'static) {
    std::thread::Builder::new()
        .name(name.into())
        .spawn(run)
        .expect("spawning a thread");
}
