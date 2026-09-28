//! Exercise `HangUp` through the fork's `NeovimSession` against real processes without a display
//! or `LiveHarness`. Closing stdin ends nvim while its `Neovim` handle remains alive.
//! A launcher with a failed handshake is released, and hanging up a `--listen` connection
//! shuts the socket so the server sees the client leave.

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use neovide::bridge::session::{NeovimInstance, NeovimSession};
use neovide::bridge::NeovimWriter;

/// The fork's session wants a handler whose writer is its own `NeovimWriter`; this one answers
/// nothing (nvim-rs's defaults), which is all a hang-up needs.
#[derive(Clone)]
struct Quiet;

impl nvim_rs::Handler for Quiet {
    type Writer = NeovimWriter;
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime")
}

fn scratch(case: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("nvhang-{}-{case}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

fn embedded(program: &Path, args: &[&str], dir: &Path) -> NeovimInstance {
    let mut command = tokio::process::Command::new(program);
    command
        .args(args)
        .current_dir(dir)
        .env("XDG_STATE_HOME", dir.join("state"))
        .env("XDG_CONFIG_HOME", dir.join("config"))
        .env("XDG_DATA_HOME", dir.join("data"));
    NeovimInstance::Embedded(command)
}

/// The session's `Neovim` handle stays alive, as it does in the fork's handler.
/// Hanging up the session must still end nvim.
#[test]
fn a_hang_up_ends_nvim_while_the_connection_lives() {
    let dir = scratch("pipe");
    let runtime = runtime();
    let nvim = PathBuf::from("nvim");
    let mut session = runtime
        .block_on(NeovimSession::new(
            embedded(&nvim, &["--clean", "--embed", "-n"], &dir),
            Quiet,
        ))
        .expect("a session on nvim");
    let mut child = session.neovim_process.take().expect("the child");
    let handle = session.neovim.clone();
    session.hang_up.hang_up();
    let exited = runtime.block_on(async { tokio::time::timeout(Duration::from_secs(5), child.wait()).await });
    if exited.is_err() {
        let _ = runtime.block_on(child.kill());
    }
    drop(handle);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(exited.is_ok(), "nvim exited on the hang-up alone");
}

/// The launcher closes stdout before the handshake, then waits for EOF on stdin.
/// After the handshake fails, startup waits for stderr to close. Release the session's stdin
/// writer first so the launcher can exit and startup can report the error.
#[test]
fn a_launcher_that_fails_the_handshake_is_let_go() {
    use std::os::unix::fs::PermissionsExt;
    let dir = scratch("handshake");
    let launcher = dir.join("launcher");
    std::fs::write(&launcher, "#!/bin/sh\nexec 1>&-\ncat >/dev/null\n").expect("write the launcher");
    std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    let (tx, rx) = mpsc::channel();
    let instance = embedded(&launcher, &[], &dir);
    std::thread::spawn(move || {
        let runtime = runtime();
        let result = runtime.block_on(NeovimSession::new(instance, Quiet));
        let _ = tx.send(result.is_err());
        // The runtime is dropped here, on this thread; a launcher still blocked is abandoned to it.
    });
    let reported = rx.recv_timeout(Duration::from_secs(10));
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(
        reported,
        Ok(true),
        "startup reports the failed handshake instead of waiting forever"
    );
}

/// A `NEOVIDE_SERVER` connection splits a socket into read and write halves.
/// Dropping the write half alone leaves the socket open. Hanging up shuts its write side
/// so the server sees the client leave and removes its channel.
#[test]
fn a_hang_up_on_a_server_session_closes_the_servers_channel() {
    use nvim_rs::rpc::handler::Dummy;
    let dir = scratch("server");
    let socket = dir.join("s");
    let mut server = std::process::Command::new("nvim")
        .args(["--clean", "--headless", "-n", "--listen"])
        .arg(&socket)
        .current_dir(&dir)
        .env("XDG_STATE_HOME", dir.join("state"))
        .spawn()
        .expect("nvim must be on PATH for this test");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !socket.exists() {
        assert!(Instant::now() < deadline, "the server never listened");
        std::thread::sleep(Duration::from_millis(20));
    }
    let runtime = runtime();
    let session = runtime
        .block_on(NeovimSession::new(
            NeovimInstance::Server {
                address: socket.to_string_lossy().into_owned(),
            },
            Quiet,
        ))
        .expect("a session on the server");
    let (observer, _io) = runtime
        .block_on(nvim_rs::create::tokio::new_path(&socket, Dummy::new()))
        .expect("an observer");
    let sockets = || -> usize {
        runtime
            .block_on(observer.list_chans())
            .map(|chans| {
                chans
                    .iter()
                    .filter(|c| {
                        c.as_map().is_some_and(|m| {
                            m.iter()
                                .any(|(k, v)| k.as_str() == Some("stream") && v.as_str() == Some("socket"))
                        })
                    })
                    .count()
            })
            .unwrap_or(0)
    };
    let before = sockets();
    session.hang_up.hang_up();
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut after = sockets();
    while after >= before && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
        after = sockets();
    }
    let _ = server.kill();
    let _ = server.wait();
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(before, 2, "the case: the session and the observer");
    assert_eq!(
        after, 1,
        "the session's channel closed: the server saw the client leave"
    );
}
