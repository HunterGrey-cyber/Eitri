//! `NvimLink` against a real `nvim --headless --listen`: values and errors, a connection that stays
//! answerable while nvim waits for a key, notifications, and nvim dying under a pending call.
//!
//! Every case is `#[ignore]`d: it needs `nvim` on `PATH`, spends no tokens and opens no window.

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use eitri_core::nvim_rpc::{LinkEvent, NvimLink, Pending, RpcError};
use rmpv::Value;

/// Under `std::env::temp_dir()`, not `CARGO_TARGET_TMPDIR`: the socket bound inside it is capped at
/// 103 bytes (`agent::socket_path`), and a target directory in a git worktree already uses most of
/// that. Case names stay short for the same reason.
fn scratch_dir(case: &str) -> PathBuf {
    std::env::temp_dir().join(format!("rpc-{}-{case}", std::process::id()))
}

/// A headless nvim and the scratch directory it lives in; killed by its own handle and removed on drop.
struct Nvim {
    child: Child,
    scratch: PathBuf,
    socket: PathBuf,
}

impl Nvim {
    fn start(case: &str) -> Nvim {
        let scratch = scratch_dir(case);
        let _ = std::fs::remove_dir_all(&scratch);
        std::fs::create_dir_all(scratch.join("state")).unwrap();
        let socket = agent::socket_path::in_dir(&scratch, "n.sock").unwrap();
        let child = Command::new("nvim")
            .args(["--headless", "--clean", "-n", "-i", "NONE", "--listen"])
            .arg(&socket)
            .current_dir(&scratch)
            .env("XDG_STATE_HOME", scratch.join("state"))
            .env("XDG_DATA_HOME", scratch.join("data"))
            .env("XDG_CONFIG_HOME", scratch.join("config"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("nvim must be on PATH for this test");
        let nvim = Nvim { child, scratch, socket };
        let deadline = Instant::now() + Duration::from_secs(5);
        while !nvim.socket.exists() {
            assert!(Instant::now() < deadline, "nvim never created its socket");
            std::thread::sleep(Duration::from_millis(20));
        }
        nvim
    }

    /// The socket file appears when nvim binds, a moment before it listens; until then a connect is
    /// refused.
    fn connect(&self) -> (NvimLink, Receiver<LinkEvent>) {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match NvimLink::connect(&self.socket, Duration::from_secs(1)) {
                Ok(connected) => return connected,
                Err(e) => {
                    assert!(Instant::now() < deadline, "could not connect: {e}");
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
        }
    }
}

impl Drop for Nvim {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.scratch);
    }
}

fn answer(pending: &Pending, within: Duration) -> Result<Value, RpcError> {
    pending.wait(within).expect("an answer in time")
}

fn field(map: &Value, name: &str) -> Option<Value> {
    map.as_map()?
        .iter()
        .find(|(k, _)| k.as_str() == Some(name))
        .map(|(_, v)| v.clone())
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn exec_lua_returns_values_and_errors() {
    let nvim = Nvim::start("lua");
    let (link, _events) = nvim.connect();
    let two = answer(&link.exec_lua("return 1 + 1", vec![]), Duration::from_secs(5)).unwrap();
    assert_eq!(two.as_i64(), Some(2));
    let echoed = answer(
        &link.exec_lua("return ...", vec![Value::from("a")]),
        Duration::from_secs(5),
    )
    .unwrap();
    assert_eq!(echoed.as_str(), Some("a"));
    match answer(&link.exec_lua("error('boom')", vec![]), Duration::from_secs(5)) {
        Err(RpcError::Nvim(message)) => assert!(message.contains("boom"), "{message}"),
        other => panic!("expected an nvim error, got {other:?}"),
    }
    assert!(link.is_alive());
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn get_api_info_answers_while_nvim_waits_for_a_key_and_exec_lua_waits() {
    let nvim = Nvim::start("wait");
    let (ui, _ui_events) = nvim.connect();
    // A UI makes nvim treat `f` as the start of a command that waits for its character. The
    // redraws pile up unread in `_ui_events`, which is fine: it is unbounded.
    let attach = ui.call(
        "nvim_ui_attach",
        vec![
            Value::from(80),
            Value::from(24),
            Value::Map(vec![(Value::from("rgb"), Value::from(true))]),
        ],
    );
    answer(&attach, Duration::from_secs(5)).unwrap();
    answer(&ui.call("nvim_input", vec![Value::from("f")]), Duration::from_secs(5)).unwrap();
    std::thread::sleep(Duration::from_millis(200));

    // nvim is now inside a char-wait. `nvim_get_api_info` is api-fast, so a fresh connect works...
    let started = Instant::now();
    let (second, _events) = NvimLink::connect(&nvim.socket, Duration::from_secs(1)).expect("connect while waiting");
    assert!(started.elapsed() < Duration::from_secs(1));
    // ...but `nvim_exec_lua` is queued until the key arrives.
    let queued = second.exec_lua("return 1", vec![]);
    let until = Instant::now() + Duration::from_secs(1);
    while Instant::now() < until {
        assert!(queued.try_take().is_none(), "exec_lua answered during the char-wait");
        std::thread::sleep(Duration::from_millis(50));
    }
    // `nvim_get_mode` is api-fast too, and says nvim is blocked.
    let mode = answer(&ui.call("nvim_get_mode", vec![]), Duration::from_secs(1)).unwrap();
    assert_eq!(
        field(&mode, "blocking").and_then(|v| v.as_bool()),
        Some(true),
        "{mode:?}"
    );

    answer(&ui.call("nvim_input", vec![Value::from("x")]), Duration::from_secs(1)).unwrap();
    assert_eq!(answer(&queued, Duration::from_secs(1)).unwrap().as_i64(), Some(1));
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn rpcnotify_reaches_the_link() {
    let nvim = Nvim::start("note");
    let (link, events) = nvim.connect();
    let sent = link.exec_lua(
        "local chan = ...; vim.rpcnotify(chan, 't', 1, 'a')",
        vec![Value::from(link.channel_id())],
    );
    // The notification may arrive before the call's own answer, so the two are not ordered here.
    match events.recv_timeout(Duration::from_secs(1)).expect("the notification") {
        LinkEvent::Notification { method, params } => {
            assert_eq!(method, "t");
            assert_eq!(params, vec![Value::from(1), Value::from("a")]);
        }
        LinkEvent::Closed => panic!("closed"),
    }
    assert_eq!(answer(&sent, Duration::from_secs(1)), Ok(Value::Nil));
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn killing_nvim_closes_the_link_within_a_second() {
    let mut nvim = Nvim::start("kill");
    let (link, events) = nvim.connect();
    // Busy in nvim for ten seconds, with no UI needed: deterministic, unlike a char-wait.
    let stuck = link.exec_lua("vim.uv.sleep(10000)", vec![]);
    std::thread::sleep(Duration::from_millis(100));
    assert!(stuck.try_take().is_none());
    nvim.child.kill().unwrap();
    assert_eq!(events.recv_timeout(Duration::from_secs(1)), Ok(LinkEvent::Closed));
    assert_eq!(stuck.wait(Duration::from_secs(1)), Some(Err(RpcError::Closed)));
    assert!(!link.is_alive());
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn connect_to_a_dead_socket_fails_fast() {
    let dir = scratch_dir("dead");
    std::fs::create_dir_all(&dir).unwrap();
    let socket = agent::socket_path::in_dir(&dir, "n.sock").unwrap();
    // Binding creates the file; dropping the listener leaves it with nobody behind it.
    drop(std::os::unix::net::UnixListener::bind(&socket).unwrap());
    let started = Instant::now();
    let error = NvimLink::connect(&socket, Duration::from_secs(2))
        .err()
        .expect("a refused connect");
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "took {:?}",
        started.elapsed()
    );
    assert!(error.starts_with("connect "), "{error}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn the_peer_pid_is_the_nvim_that_holds_the_socket() {
    let mut nvim = Nvim::start("peer");
    let (link, _events) = nvim.connect();
    assert_eq!(link.peer_pid(), Some(nvim.child.id()));
    // What nvim says about itself agrees here; the point is that the first does not rest on it.
    let own = answer(
        &link.call("nvim_eval", vec![Value::from("getpid()")]),
        Duration::from_secs(2),
    )
    .unwrap();
    assert_eq!(own.as_u64(), Some(u64::from(nvim.child.id())));
    // Once the process that was connected to is gone and reaped its number may name a stranger, so
    // the link stops answering with it (this needs SO_PEERPIDFD, Linux 6.5 and later).
    nvim.child.kill().unwrap();
    nvim.child.wait().unwrap();
    assert_eq!(link.peer_pid(), None);
}
