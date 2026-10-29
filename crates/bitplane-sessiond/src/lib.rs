pub mod autostart;
pub mod client;
pub mod connection;
pub mod dev;
pub mod env;
pub mod module;
pub mod paths;
pub mod pidfile;
pub mod server;

pub const DAEMON_NAME: &str = "bitplane-sessiond";
pub const DAEMON_VERSION: &str = env!("CARGO_PKG_VERSION");
