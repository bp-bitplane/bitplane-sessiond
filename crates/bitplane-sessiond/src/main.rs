use std::io::IsTerminal;
use std::os::fd::{AsFd, FromRawFd};
use std::path::PathBuf;
use std::process::ExitCode;

use bitplane_sessiond::agent::{Agents, hook, wrapper};
use bitplane_sessiond::autostart::connect_or_start;
use bitplane_sessiond::client::Client;
use bitplane_sessiond::paths::Dirs;
use bitplane_sessiond::server::{Config, serve};
use bitplane_sessiond::session::Sessions;
use bitplane_sessiond::{DAEMON_NAME, dev};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = DAEMON_NAME, version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the daemon.
    Serve {
        /// Overrides the socket and pidfile directory.
        #[arg(long)]
        runtime_dir: Option<PathBuf>,
        /// Overrides the state directory.
        #[arg(long)]
        state_dir: Option<PathBuf>,
    },
    /// Forward a hook payload on stdin to the daemon; run by the agent hooks.
    Hook,
    /// Talk to a running daemon by hand.
    Dev {
        /// Connects to this socket instead, and never starts a daemon.
        #[arg(long)]
        socket: Option<PathBuf>,
        /// Fails instead of starting a daemon when none is running.
        #[arg(long)]
        no_start: bool,
        #[command(subcommand)]
        command: DevCommand,
    },
}

#[derive(Subcommand)]
enum DevCommand {
    /// Print the daemon's hello reply.
    Hello,
    /// Call one method and print its result.
    Call {
        method: String,
        params: Option<String>,
    },
    /// Print every event as a JSON line.
    Events,
    /// Attach this terminal to a session; Ctrl-] detaches.
    Attach { session: String },
}

fn main() -> ExitCode {
    if wrapper::invoked() {
        wrapper::run();
    }
    let cli = Cli::parse();
    if let Command::Hook = cli.command {
        hook::forward();
        return ExitCode::SUCCESS;
    }
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
        .with_ansi(std::io::stderr().is_terminal())
        .init();
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    let result = runtime.block_on(run(cli));
    // Stdin is read on a blocking thread that never returns on its own.
    runtime.shutdown_background();
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{DAEMON_NAME}: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    match cli.command {
        Command::Hook => hook::forward(),
        Command::Serve {
            runtime_dir,
            state_dir,
        } => {
            // Both given: the host's env may lack XDG_RUNTIME_DIR and TMPDIR.
            let dirs = match (runtime_dir, state_dir) {
                (Some(runtime), Some(state)) => Dirs { runtime, state },
                (runtime, state) => {
                    let host = Dirs::for_host()?;
                    Dirs {
                        runtime: runtime.unwrap_or(host.runtime),
                        state: state.unwrap_or(host.state),
                    }
                }
            };
            let config = Config {
                dirs,
                listener: activated_listener(),
                peer_uid: rustix::process::getuid().as_raw(),
                modules: vec![Sessions::module(), Agents::claude(std::env::current_exe()?)],
                env: None,
            };
            serve(config, shutdown_signal()).await?;
        }
        Command::Dev {
            socket,
            no_start,
            command,
        } => {
            let mut client = match socket {
                Some(socket) => Client::connect(&socket).await?,
                None if no_start => Client::connect(&Dirs::for_host()?.socket()).await?,
                None => connect_or_start(&Dirs::for_host()?, &std::env::current_exe()?).await?,
            };
            let hello = client.hello("bitplane-sessiond dev").await?;
            match command {
                DevCommand::Hello => println!("{}", serde_json::to_string_pretty(&hello)?),
                DevCommand::Call { method, params } => {
                    let params = params.map(|text| serde_json::from_str(&text)).transpose()?;
                    let result = client.call(&method, params).await?;
                    println!("{}", serde_json::to_string_pretty(&result)?);
                }
                DevCommand::Events => dev::events(&mut client, tokio::io::stdout()).await?,
                DevCommand::Attach { session } => {
                    let size = terminal_size().unwrap_or((80, 24));
                    let _raw = RawMode::enter();
                    dev::attach(
                        &mut client,
                        &session,
                        size,
                        window_changes()?,
                        tokio::io::stdin(),
                        tokio::io::stdout(),
                    )
                    .await?;
                }
            }
        }
    }
    Ok(())
}

/// Socket activation: systemd passes fd 3 with `LISTEN_PID` set to us.
fn activated_listener() -> Option<std::os::unix::net::UnixListener> {
    let pid: u32 = std::env::var("LISTEN_PID").ok()?.parse().ok()?;
    let fds: u32 = std::env::var("LISTEN_FDS").ok()?.parse().ok()?;
    (pid == std::process::id() && fds >= 1)
        // SAFETY: the service manager hands us fd 3 for exactly this use.
        .then(|| unsafe { std::os::unix::net::UnixListener::from_raw_fd(3) })
}

fn terminal_size() -> Option<(u16, u16)> {
    rustix::termios::tcgetwinsize(std::io::stdin().as_fd())
        .ok()
        .map(|size| (size.ws_col, size.ws_row))
}

/// The terminal's size after every SIGWINCH.
fn window_changes() -> std::io::Result<tokio::sync::mpsc::UnboundedReceiver<(u16, u16)>> {
    use tokio::signal::unix::{SignalKind, signal};
    let mut winch = signal(SignalKind::window_change())?;
    let (sizes, received) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while winch.recv().await.is_some() {
            if let Some(size) = terminal_size()
                && sizes.send(size).is_err()
            {
                return;
            }
        }
    });
    Ok(received)
}

async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
    tokio::select! {
        _ = term.recv() => {}
        _ = tokio::signal::ctrl_c() => {}
    }
}

struct RawMode(Option<rustix::termios::Termios>);

impl RawMode {
    fn enter() -> Self {
        let stdin = std::io::stdin();
        let Ok(original) = rustix::termios::tcgetattr(stdin.as_fd()) else {
            return Self(None);
        };
        let mut raw = original.clone();
        raw.make_raw();
        let _ =
            rustix::termios::tcsetattr(stdin.as_fd(), rustix::termios::OptionalActions::Now, &raw);
        Self(Some(original))
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        if let Some(original) = &self.0 {
            let _ = rustix::termios::tcsetattr(
                std::io::stdin().as_fd(),
                rustix::termios::OptionalActions::Now,
                original,
            );
        }
    }
}
