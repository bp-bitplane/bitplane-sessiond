mod support;

use std::os::unix::fs::PermissionsExt;
use std::process::Stdio;
use std::time::Duration;

use bitplane_sessiond::client::{Client, ClientError};
use bitplane_sessiond::server::ServeError;
use support::{TestDaemon, own_uid, short_tempdir};

#[tokio::test]
async fn a_second_instance_of_the_same_version_refuses_to_start() {
    let daemon = TestDaemon::start(vec![]).await;

    let second = tokio::time::timeout(Duration::from_secs(2), daemon.start_again())
        .await
        .unwrap();

    assert!(matches!(second, Err(ServeError::AlreadyRunning)));
    daemon.connect_hello().await;
}

#[tokio::test]
async fn the_socket_lives_in_a_private_dir() {
    let daemon = TestDaemon::start(vec![]).await;

    let mode = std::fs::metadata(&daemon.dirs.runtime)
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o700);
    let pid = std::fs::read_to_string(daemon.dirs.pidfile()).unwrap();
    assert_eq!(pid.trim(), std::process::id().to_string());
}

#[tokio::test]
async fn a_connection_from_another_uid_is_closed_unread() {
    let daemon = TestDaemon::start_with(vec![], own_uid() + 1).await;

    let result = daemon.connect().await.hello("intruder").await;

    assert!(matches!(result, Err(ClientError::Closed)));
}

#[tokio::test]
async fn a_stopped_daemon_frees_its_socket_and_lock() {
    let mut daemon = TestDaemon::start(vec![]).await;

    daemon.stop().await.unwrap();

    assert!(!daemon.dirs.socket().exists());
    assert!(
        daemon.dirs.pidfile().exists(),
        "the pidfile is never unlinked"
    );
    let restarted = tokio::time::timeout(Duration::from_millis(200), daemon.start_again()).await;
    assert!(
        restarted.is_err(),
        "a new instance takes the freed lock and keeps serving"
    );
}

#[tokio::test]
async fn the_serve_binary_answers_hello() {
    let (_root, root) = short_tempdir();
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_bitplane-sessiond"))
        .args(["serve", "--runtime-dir"])
        .arg(root.join("run"))
        .arg("--state-dir")
        .arg(root.join("state"))
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let socket = root.join("run/sessiond.sock");
    for _ in 0..300 {
        if socket.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let hello = Client::connect(&socket)
        .await
        .unwrap()
        .hello("test")
        .await
        .unwrap();

    assert_eq!(hello.daemon.name, "bitplane-sessiond");
    assert!(hello.capabilities.is_empty());
    child.start_kill().unwrap();
}
