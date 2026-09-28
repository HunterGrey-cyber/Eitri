//! `editor_quit_lua`'s Lua, driven by a real `nvim` over msgpack-RPC and delivered as `shell` delivers
//! it: an `nvim_exec_lua` request (v1 hardening Task 6, fix round 1), never typed keys.
//!
//! The two older tests are `#[ignore]`d, as they were when this file was written; the v1-hardening
//! ones are not, because that plan ignores only a test that needs a display. So **`cargo test -p
//! neovibe-core` needs `nvim` on PATH** since Task 6. None of them spends tokens, touches the
//! network, opens a window or needs a display.
//!
//! Every other real-nvim test in this crate drives `nvim --headless` with no UI attached and reads
//! whatever the Lua side pushes to a socket -- that is not enough here, because **headless without a
//! UI answers `:confirm qall`'s dialog with its own default** (measured: typing `c` still wrote the
//! buffer and quit). Only with a UI attached (`nvim --embed` plus `nvim_ui_attach`) does the dialog
//! actually block, in mode `r?`, so this talks real msgpack-RPC instead: a small client over `rmpv`,
//! since nothing else in this crate needs one.
//!
//! Run the ignored ones too: `cargo test -p neovibe-core --test editor_quit_with_real_nvim -- --include-ignored`

use std::collections::HashMap;
use std::io::{ErrorKind, Read, Write};
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use neovibe_core::layout::kill::editor_quit_lua;
use rmpv::Value;

const DEADLINE: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(25);

/// A msgpack-RPC client over `nvim --embed`'s stdio: a request writer plus a reader thread that
/// forwards type-1 responses and drops type-2 (`redraw`) notifications, which would otherwise fill
/// the pipe and stall nvim. A request can be sent without waiting for its answer ([`Client::send`]),
/// as `shell` sends the quit: its response arrives only once the Lua returns, which a dialog holds up.
struct Client {
    stdin: std::process::ChildStdin,
    next_id: u64,
    responses: mpsc::Receiver<(u64, Value, Value)>,
    /// Responses read while waiting for a different one.
    early: HashMap<u64, (Value, Value)>,
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
            early: HashMap::new(),
        }
    }

    /// Writes `method(params)` and returns its id at once.
    fn send(&mut self, method: &str, params: Vec<Value>) -> u64 {
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
        id
    }

    /// Waits up to [`DEADLINE`] for request `id`'s `(error, result)`, panicking on silence.
    fn wait(&mut self, id: u64, what: &str) -> (Value, Value) {
        if let Some(answer) = self.early.remove(&id) {
            return answer;
        }
        let deadline = Instant::now() + DEADLINE;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let (got_id, err, result) = self
                .responses
                .recv_timeout(left)
                .unwrap_or_else(|_| panic!("nvim did not answer {what} within {DEADLINE:?}"));
            if got_id == id {
                return (err, result);
            }
            self.early.insert(got_id, (err, result));
        }
    }

    /// Whether request `id` has been answered yet, without waiting.
    fn answered(&mut self, id: u64) -> bool {
        while let Ok((got_id, err, result)) = self.responses.try_recv() {
            self.early.insert(got_id, (err, result));
        }
        self.early.contains_key(&id)
    }

    /// Sends `method(params)` and waits up to [`DEADLINE`] for its response, panicking on an nvim
    /// error or on silence.
    fn request(&mut self, method: &str, params: Vec<Value>) -> Value {
        let id = self.send(method, params);
        let (err, result) = self.wait(id, method);
        assert!(err.is_nil(), "nvim returned an error for {method}: {err:?}");
        result
    }

    /// The quit as `shell` sends it: an `nvim_exec_lua` request, not waited for. Its id.
    fn send_quit(&mut self, generation: u32) -> u64 {
        self.send(
            "nvim_exec_lua",
            vec![Value::from(editor_quit_lua(generation)), Value::Array(vec![])],
        )
    }

    /// `nvim_get_mode`'s `(mode, blocking)`. `nvim_get_mode` is one of nvim's fast calls: answered
    /// even while a dialog is up or a key is awaited, unlike every other request here.
    fn mode(&mut self) -> (String, bool) {
        let result = self.request("nvim_get_mode", vec![]);
        let field = |name: &str| {
            result
                .as_map()
                .and_then(|entries| entries.iter().find(|(k, _)| k.as_str() == Some(name)))
                .map(|(_, v)| v.clone())
        };
        (
            field("mode")
                .and_then(|v| v.as_str().map(str::to_owned))
                .unwrap_or_default(),
            field("blocking").and_then(|v| v.as_bool()).unwrap_or(false),
        )
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
    spawn_nvim_with(scratch, socket_path, &["-n"])
}

/// `extra` after `--clean --embed`: `-n` (no swap file) for every case but the one about swap files.
fn spawn_nvim_with(scratch: &Path, socket_path: &Path, extra: &[&str]) -> Child {
    Command::new("nvim")
        .args(["--clean", "--embed"])
        .args(extra)
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
        let (mode, blocking) = client.mode();
        if mode == expected {
            return;
        }
        if Instant::now() >= deadline {
            panic!("nvim never reached mode {expected:?}; last saw {mode:?} (blocking: {blocking})");
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

    let quit = client.send_quit(7);
    // The check that makes this non-vacuous: headless-without-a-UI answers the dialog with its
    // default instead of blocking, so seeing mode `r?` is proof the dialog is really up.
    wait_for_mode(&mut client, "r?");

    client.request("nvim_input", vec![Value::from("c")]);
    let line = wait_for_letter(&listener);
    assert_eq!(line, "Q 7\n");
    let (err, _) = client.wait(quit, "the quit");
    assert!(err.is_nil(), "the chunk returned, and its request says so: {err:?}");

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
        client.send_quit(1);
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
        client.send_quit(2);
        wait_for_mode(&mut client, "r?");
        client.request("nvim_input", vec![Value::from("n")]);
        let status = wait_for_exit(&mut child);
        assert!(status.success(), "nvim exited with {status}");
        assert_no_letter(&listener);
        assert!(!draft.exists(), "answering No means nothing was written");
        let _ = std::fs::remove_dir_all(&scratch);
    }
}

/// v1 hardening Task 6 (R3; review finding R1-1): every window close now has nvim run exactly this
/// Lua (`shell`'s `ask_nvim_to_quit`, `kill_pane::EditorQuit::Window`) where it used to run the
/// fork's `:qa!`. A close cancelled in nvim's prompt keeps the unsaved buffer, writes nothing, and
/// leaves its swap file where it was -- the last step is the old close's `:qa!`, which deleted it,
/// as the control that the swap-file check can see a deletion at all.
///
/// Not `#[ignore]`d, unlike the rest of this file: the v1-hardening plan ignores only a test that
/// needs a display, and this one needs only `nvim` on PATH.
#[test]
fn a_cancelled_window_close_keeps_the_unsaved_buffer_and_its_swap_file() {
    let scratch = scratch_dir("close-swap");
    let _ = std::fs::remove_dir_all(&scratch);
    std::fs::create_dir_all(&scratch).expect("scratch dir");
    let swap = scratch.join("swap");
    std::fs::create_dir_all(&swap).expect("swap dir");
    let socket_path = agent::socket_path::in_dir(&scratch, "s.sock").expect("under the cap");
    let listener = UnixListener::bind(&socket_path).expect("bind");
    listener.set_nonblocking(true).expect("non-blocking");

    let mut child = spawn_nvim_with(&scratch, &socket_path, &[]);
    let mut client = Client::attach(&mut child);
    client.request(
        "nvim_ui_attach",
        vec![
            Value::from(80),
            Value::from(24),
            Value::Map(vec![(Value::from("ext_linegrid"), Value::from(true))]),
        ],
    );
    client.request(
        "nvim_command",
        vec![Value::from(format!("set directory={}//", swap.display()))],
    );
    let draft = scratch.join("draft.txt");
    client.request("nvim_command", vec![Value::from(format!("edit {}", draft.display()))]);
    client.request(
        "nvim_call_function",
        vec![
            Value::from("setline"),
            Value::Array(vec![Value::from(1), Value::from("unsaved text")]),
        ],
    );
    client.request("nvim_command", vec![Value::from("preserve")]);
    let swap_files = || std::fs::read_dir(&swap).map(|dir| dir.count()).unwrap_or(0);
    assert_eq!(swap_files(), 1, "the control: the modified buffer has a swap file");

    client.send_quit(9);
    wait_for_mode(&mut client, "r?");
    client.request("nvim_input", vec![Value::from("c")]);
    assert_eq!(wait_for_letter(&listener), "Q 9\n", "the cancel reaches the shell");

    let modified = client.request("nvim_eval", vec![Value::from("&modified")]);
    assert_eq!(modified.as_i64(), Some(1), "the buffer is still modified: {modified:?}");
    let line = client.request("nvim_get_current_line", vec![]);
    assert_eq!(line.as_str(), Some("unsaved text"), "and still holds the edit");
    assert!(!draft.exists(), "nothing was written");
    assert_eq!(swap_files(), 1, "the swap file is not deleted");

    // What a window close did before: the fork's quit runs `:qa!`.
    client.request("nvim_input", vec![Value::from("<Cmd>qa!<CR>")]);
    let status = wait_for_exit(&mut child);
    assert!(status.success(), "nvim exited with {status}");
    assert_eq!(swap_files(), 0, "the old close's :qa! deleted it");
    assert!(!draft.exists(), "and the edit was never written anywhere");

    let _ = std::fs::remove_dir_all(&scratch);
}

/// One nvim with a UI attached, a file and one edit made by typing it (so `u` would undo it), the
/// pane-switch socket listening: what the pending-key cases start from.
struct Editing {
    scratch: std::path::PathBuf,
    listener: UnixListener,
    child: Child,
    client: Client,
}

fn editing(case: &str) -> Editing {
    let scratch = scratch_dir(case);
    let _ = std::fs::remove_dir_all(&scratch);
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
        "nvim_buf_set_lines",
        vec![
            Value::from(0),
            Value::from(0),
            Value::from(-1),
            Value::from(false),
            Value::Array(vec![Value::from("hello world")]),
        ],
    );
    client.request("nvim_input", vec![Value::from("A user edit<Esc>")]);
    wait_for_mode(&mut client, "n");
    Editing {
        scratch,
        listener,
        child,
        client,
    }
}

fn buffer_text(client: &mut Client) -> String {
    let lines = client.request(
        "nvim_buf_get_lines",
        vec![Value::from(0), Value::from(0), Value::from(-1), Value::from(false)],
    );
    lines
        .as_array()
        .map(|lines| lines.iter().filter_map(|l| l.as_str()).collect::<Vec<_>>().join("\n"))
        .unwrap_or_default()
}

/// v1 hardening Task 6, fix round 1 (review finding I1): a window close while nvim waits for a key
/// -- here the character after `f`; `t`, `r`, `m`, `q`, `Ctrl-W`, `getchar()` and an insert-mode
/// `Ctrl-V` behave alike -- must not type anything into the user's buffer.
///
/// The control is the delivery Task 6 first shipped, the same Lua typed as `<Cmd>lua …<CR>` through
/// `nvim_input`: `<Cmd>` becomes `f`'s character and the rest runs as Normal-mode commands, so `u`
/// undoes the user's edit and `a` inserts the Lua. Sent as an `nvim_exec_lua` request instead, nvim
/// holds it until it has the key it is waiting for (it answers `nvim_get_mode` meanwhile: `n`,
/// blocking), then asks; a cancel keeps the edit and sends the generation back.
#[test]
fn a_quit_while_nvim_waits_for_a_key_types_nothing_into_the_buffer() {
    // --- the control: typed, it destroys the edit ------------------------------------------------
    {
        let Editing {
            scratch,
            listener: _listener,
            mut child,
            mut client,
        } = editing("pending-typed");
        client.request("nvim_input", vec![Value::from("f")]);
        assert_eq!(client.mode(), ("n".to_string(), true), "f waits for its character");
        client.request(
            "nvim_input",
            vec![Value::from(format!("<Cmd>lua {}<CR>", editor_quit_lua(1)))],
        );
        std::thread::sleep(Duration::from_millis(300));
        let text = buffer_text(&mut client);
        assert!(!text.contains("user edit"), "typed, `u` undid the edit: {text:?}");
        assert!(
            text.contains("confirm qall"),
            "and the Lua was typed into the buffer: {text:?}"
        );
        client.request("nvim_input", vec![Value::from("<Esc><Cmd>qa!<CR>")]);
        wait_for_exit(&mut child);
        let _ = std::fs::remove_dir_all(&scratch);
    }

    // --- as `shell` sends it now: a request, which types nothing ---------------------------------
    let Editing {
        scratch,
        listener,
        mut child,
        mut client,
    } = editing("pending-rpc");
    client.request("nvim_input", vec![Value::from("f")]);
    let quit = client.send_quit(2);
    let held_until = Instant::now() + Duration::from_millis(500);
    while Instant::now() < held_until {
        assert_eq!(
            client.mode(),
            ("n".to_string(), true),
            "nvim holds the request while it waits for f's character"
        );
        std::thread::sleep(POLL);
    }
    assert!(!client.answered(quit));

    client.request("nvim_input", vec![Value::from("x")]);
    wait_for_mode(&mut client, "r?");
    client.request("nvim_input", vec![Value::from("c")]);
    assert_eq!(wait_for_letter(&listener), "Q 2\n", "the cancel reaches the shell");
    let (err, _) = client.wait(quit, "the quit");
    assert!(err.is_nil(), "{err:?}");

    assert_eq!(buffer_text(&mut client), "hello world user edit", "the edit is intact");
    let modified = client.request("nvim_eval", vec![Value::from("&modified")]);
    assert_eq!(modified.as_i64(), Some(1));
    assert_eq!(client.mode().0, "n", "and nvim is back in Normal mode");

    client.request("nvim_input", vec![Value::from("<Cmd>qa!<CR>")]);
    wait_for_exit(&mut child);
    let _ = std::fs::remove_dir_all(&scratch);
}
