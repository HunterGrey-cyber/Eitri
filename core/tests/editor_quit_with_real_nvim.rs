//! `editor_quit_keys`'s Lua, driven by a real `nvim` over msgpack-RPC.
//!
//! `#[ignore]`d: it needs `nvim` on PATH. It spends no tokens, touches no network, opens no window
//! and needs no display.
//!
//! Every other real-nvim test in this crate drives `nvim --headless` with no UI attached and reads
//! whatever the Lua side pushes to a socket -- that is not enough here, because **headless without a
//! UI answers `:confirm qall`'s dialog with its own default** (measured: typing `c` still wrote the
//! buffer and quit). Only with a UI attached (`nvim --embed` plus `nvim_ui_attach`) does the dialog
//! actually block, in mode `r?`, so this talks real msgpack-RPC instead: a ~60-line client over
//! `rmpv`, since nothing else in this crate needs one.
//!
//! Run: `cargo test -p neovibe-core --test editor_quit_with_real_nvim -- --ignored`

use std::io::{ErrorKind, Read, Write};
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use neovibe_core::layout::kill::editor_quit_keys;
use rmpv::Value;

const DEADLINE: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(25);

/// A msgpack-RPC client over `nvim --embed`'s stdio: a request writer plus a reader thread that
/// forwards type-1 responses and drops type-2 (`redraw`) notifications, which would otherwise fill
/// the pipe and stall nvim.
struct Client {
    stdin: std::process::ChildStdin,
    next_id: u64,
    responses: mpsc::Receiver<(u64, Value, Value)>,
}

impl Client {
    fn attach(child: &mut Child) -> Self {
        let stdin = child.stdin.take().expect("nvim's stdin must be piped");
        let mut stdout = child.stdout.take().expect("nvim's stdout must be piped");
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || loop {
            match rmpv::decode::read_value(&mut stdout) {
                Ok(Value::Array(items)) if items.len() == 4 && items[0].as_u64() == Some(1) => {
                    let id = items[1].as_u64().unwrap_or(u64::MAX);
                    if tx.send((id, items[2].clone(), items[3].clone())).is_err() {
                        break;
                    }
                }
                // A request from nvim to us (type 0) or a notification (type 2, `redraw`): neither
                // is expected here; drop it rather than stall on an unread response.
                Ok(_) => {}
                Err(_) => break,
            }
        });
        Self {
            stdin,
            next_id: 1,
            responses: rx,
        }
    }

    /// Sends `method(params)` and waits up to [`DEADLINE`] for its response, panicking on an nvim
    /// error or on silence.
    fn request(&mut self, method: &str, params: Vec<Value>) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let message = Value::Array(vec![
            Value::from(0),
            Value::from(id),
            Value::from(method),
            Value::Array(params),
        ]);
        rmpv::encode::write_value(&mut self.stdin, &message).expect("write to nvim's stdin");
        self.stdin.flush().expect("flush nvim's stdin");
        let (got_id, err, result) = self
            .responses
            .recv_timeout(DEADLINE)
            .unwrap_or_else(|_| panic!("nvim did not answer {method} within {DEADLINE:?}"));
        assert_eq!(got_id, id, "a response for a different request arrived");
        assert!(err.is_nil(), "nvim returned an error for {method}: {err:?}");
        result
    }
}

/// Under `std::env::temp_dir()`, as every other real-nvim test in this crate does, not
/// `CARGO_TARGET_TMPDIR`: the socket bound inside it is capped at 103 bytes
/// (`agent::socket_path`), and a target directory in a git worktree
/// (`…/.worktrees/<name>/target/tmp/eq-<pid>-quit-unmodified/s.sock`) ran past it.
fn scratch_dir(case: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("eq-{}-{case}", std::process::id()))
}

fn spawn_nvim(scratch: &Path, socket_path: &Path) -> Child {
    Command::new("nvim")
        .args(["--clean", "--embed", "-n"])
        .current_dir(scratch)
        .env("XDG_STATE_HOME", scratch.join("state"))
        .env("XDG_DATA_HOME", scratch.join("data"))
        .env("XDG_CONFIG_HOME", scratch.join("config"))
        .env("NEOVIBE_PANE_SWITCH_SOCKET", socket_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("nvim must be on PATH for this test")
}

fn wait_for_mode(client: &mut Client, expected: &str) {
    let deadline = Instant::now() + DEADLINE;
    loop {
        let result = client.request("nvim_get_mode", vec![]);
        let mode = result
            .as_map()
            .and_then(|entries| entries.iter().find(|(k, _)| k.as_str() == Some("mode")))
            .and_then(|(_, v)| v.as_str());
        if mode == Some(expected) {
            return;
        }
        if Instant::now() >= deadline {
            panic!("nvim never reached mode {expected:?}; last saw {result:?}");
        }
        std::thread::sleep(POLL);
    }
}

/// Blocks (up to [`DEADLINE`]) for one accepted connection, reads it to EOF and returns the line.
fn wait_for_letter(listener: &UnixListener) -> String {
    let deadline = Instant::now() + DEADLINE;
    loop {
        match listener.accept() {
            Ok((mut stream, _addr)) => {
                stream.set_nonblocking(false).expect("blocking mode");
                let mut line = String::new();
                stream.read_to_string(&mut line).expect("read the letter");
                return line;
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    panic!("no letter arrived within {DEADLINE:?}");
                }
                std::thread::sleep(POLL);
            }
            Err(e) => panic!("accept failed: {e}"),
        }
    }
}

/// Asserts nothing is queued on the socket. Only meaningful once nvim has already exited (or is
/// known not to write): a connection the writer already made is buffered by the kernel and would
/// answer this immediately, so no retry loop is needed here.
fn assert_no_letter(listener: &UnixListener) {
    match listener.accept() {
        Err(e) if e.kind() == ErrorKind::WouldBlock => {}
        other => panic!("expected no letter, got {other:?}"),
    }
}

fn wait_for_exit(child: &mut Child) -> std::process::ExitStatus {
    let deadline = Instant::now() + DEADLINE;
    loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            return status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            panic!("nvim did not exit within {DEADLINE:?}");
        }
        std::thread::sleep(POLL);
    }
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_cancelled_confirm_qall_sends_its_generation_and_nvim_stays() {
    let scratch = scratch_dir("cancel");
    std::fs::create_dir_all(&scratch).expect("scratch dir");
    let socket_path = agent::socket_path::in_dir(&scratch, "s.sock").expect("under the cap");
    let listener = UnixListener::bind(&socket_path).expect("bind");
    listener.set_nonblocking(true).expect("non-blocking");

    let mut child = spawn_nvim(&scratch, &socket_path);
    let mut client = Client::attach(&mut child);

    client.request(
        "nvim_ui_attach",
        vec![
            Value::from(80),
            Value::from(24),
            Value::Map(vec![(Value::from("ext_linegrid"), Value::from(true))]),
        ],
    );
    let draft = scratch.join("draft.txt");
    client.request("nvim_command", vec![Value::from(format!("file {}", draft.display()))]);
    client.request(
        "nvim_call_function",
        vec![
            Value::from("setline"),
            Value::Array(vec![Value::from(1), Value::from("x")]),
        ],
    );

    client.request("nvim_input", vec![Value::from(editor_quit_keys(7))]);
    // The check that makes this non-vacuous: headless-without-a-UI answers the dialog with its
    // default instead of blocking, so seeing mode `r?` is proof the dialog is really up.
    wait_for_mode(&mut client, "r?");

    client.request("nvim_input", vec![Value::from("c")]);
    let line = wait_for_letter(&listener);
    assert_eq!(line, "Q 7\n");

    let modified = client.request("nvim_eval", vec![Value::from("&modified")]);
    assert_eq!(modified.as_i64(), Some(1), "the buffer is still modified: {modified:?}");
    assert!(!draft.exists(), "cancelled means nothing was written");

    client.request("nvim_input", vec![Value::from("<Cmd>qa!<CR>")]);
    let status = wait_for_exit(&mut child);
    assert!(status.success(), "nvim exited with {status}");

    let _ = std::fs::remove_dir_all(&scratch);
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_confirm_qall_that_quits_sends_nothing() {
    // --- an unmodified buffer: `:confirm qall` has nothing to confirm and just quits ------------
    {
        let scratch = scratch_dir("quit-unmodified");
        std::fs::create_dir_all(&scratch).expect("scratch dir");
        let socket_path = agent::socket_path::in_dir(&scratch, "s.sock").expect("under the cap");
        let listener = UnixListener::bind(&socket_path).expect("bind");
        listener.set_nonblocking(true).expect("non-blocking");

        let mut child = spawn_nvim(&scratch, &socket_path);
        let mut client = Client::attach(&mut child);
        client.request(
            "nvim_ui_attach",
            vec![
                Value::from(80),
                Value::from(24),
                Value::Map(vec![(Value::from("ext_linegrid"), Value::from(true))]),
            ],
        );
        client.request("nvim_input", vec![Value::from(editor_quit_keys(1))]);
        let status = wait_for_exit(&mut child);
        assert!(status.success(), "nvim exited with {status}");
        assert_no_letter(&listener);
        assert!(!scratch.join("draft.txt").exists());
        let _ = std::fs::remove_dir_all(&scratch);
    }

    // --- a modified buffer answered "n" (No, don't save it): quits, still sends nothing --------
    // Measured (2026-09-26): `:confirm qall` with exactly one modified buffer asks per-buffer
    // Yes/No/Cancel ("Save changes to \"<file>\"?"), not a "Save All/Discard All/Cancel" choice --
    // "n" is what answers it without writing and lets the quit proceed.
    {
        let scratch = scratch_dir("quit-discard");
        std::fs::create_dir_all(&scratch).expect("scratch dir");
        let socket_path = agent::socket_path::in_dir(&scratch, "s.sock").expect("under the cap");
        let listener = UnixListener::bind(&socket_path).expect("bind");
        listener.set_nonblocking(true).expect("non-blocking");

        let mut child = spawn_nvim(&scratch, &socket_path);
        let mut client = Client::attach(&mut child);
        client.request(
            "nvim_ui_attach",
            vec![
                Value::from(80),
                Value::from(24),
                Value::Map(vec![(Value::from("ext_linegrid"), Value::from(true))]),
            ],
        );
        let draft = scratch.join("draft.txt");
        client.request("nvim_command", vec![Value::from(format!("file {}", draft.display()))]);
        client.request(
            "nvim_call_function",
            vec![
                Value::from("setline"),
                Value::Array(vec![Value::from(1), Value::from("x")]),
            ],
        );
        client.request("nvim_input", vec![Value::from(editor_quit_keys(2))]);
        wait_for_mode(&mut client, "r?");
        client.request("nvim_input", vec![Value::from("n")]);
        let status = wait_for_exit(&mut child);
        assert!(status.success(), "nvim exited with {status}");
        assert_no_letter(&listener);
        assert!(!draft.exists(), "answering No means nothing was written");
        let _ = std::fs::remove_dir_all(&scratch);
    }
}
