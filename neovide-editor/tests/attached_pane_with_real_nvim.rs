//! An editor pane attached to a person's running nvim (`NeovideEditorPaneOptions::attach`) never signals it, never
//! asks it to quit and never asks it for its pid, whichever way the pane lets go of it.
//!
//! Every case starts the person's nvim the way a person does: `nvim --listen <addr>` in a terminal (a private tmux
//! server, `-S <scratch>/tm.sock -f /dev/null`), so the server is the TUI's `nvim --embed` child, a process whose argv
//! carries `--embed` exactly like an nvim the pane would start itself. The server holds an unsaved edit, a job and a
//! `:terminal`. A second RPC client on the server (the observer, through the fork's own session code) sets that up and
//! reads what is left after the pane let go:
//!
//! - `a_shutdown_leaves_the_persons_nvim_as_it_was`: the pane's `shutdown`;
//! - `a_drop_without_shutdown_leaves_the_persons_nvim_as_it_was`: the pane and its window dropped;
//! - `a_pane_dropped_before_it_attached_never_attaches`: the pane dropped while its window still shows its widget,
//!   before its first frames attached it;
//! - `the_hang_up_handle_leaves_the_persons_nvim_as_it_was`: the handle a host's worker keeps, used from another
//!   thread;
//! - `a_hang_up_while_nvim_waits_for_a_key_leaves_the_persons_nvim_as_it_was`: the same with `f` pending;
//! - `the_forks_quit_leaves_the_persons_nvim_as_it_was`: the fork's own quit (`neovide.exec_detach_handler`), which in
//!   attach mode closes the UI's channel from nvim's side.
//!
//! After each: the server is the same process and alive, `v:dying` is 0, the edit is still unsaved, the job and the
//! terminal's job still run, and the pane's log (this child's stdout, read by the parent) has no signal line and no pid
//! question. Also:
//!
//! - `end_nvim_restart_and_exec_lua_watched_refuse`, and an attached pane with no chunk check answers every
//!   `exec_lua` with a refusal at once;
//! - `getpid_is_never_asked_of_an_attached_pane`;
//! - `a_refused_chunk_never_reaches_the_attached_nvim`: a chunk that would rewrite the person's buffer options,
//!   refused by the pane's chunk check, is answered at once and nvim never runs it;
//! - `a_registered_chunk_carries_the_arguments_the_check_returns`.
//!
//! Each case runs in a child copy of this binary on its own `Xvfb`; the parent connects to nothing. Without `tmux` on
//! `PATH` a case prints `NOT RUN` and fails. Ignored unless asked for, because it spawns real nvims:
//!
//!     cargo test -p neovide-editor --features test-support --test attached_pane_with_real_nvim -- --ignored
use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gtk4::prelude::*;
use neovide::bridge::{
    session::{NeovimInstance, NeovimSession},
    NeovimWriter,
};
use neovide_editor::{AttachSpec, ChunkCheck, ChunkRefused, NeovideEditorPane, NeovideEditorPaneOptions, PaneEnded};
use rmpv::Value;

#[path = "../../shell/tests/support/own_x_server.rs"]
mod own_x_server;

const CASE_ENV: &str = "EITRI_ATTACHED_PANE_CASE";
const WAIT: Duration = Duration::from_secs(15);

const CASES: &[&str] = &[
    "a_shutdown_leaves_the_persons_nvim_as_it_was",
    "a_drop_without_shutdown_leaves_the_persons_nvim_as_it_was",
    "a_pane_dropped_before_it_attached_never_attaches",
    "the_hang_up_handle_leaves_the_persons_nvim_as_it_was",
    "a_hang_up_while_nvim_waits_for_a_key_leaves_the_persons_nvim_as_it_was",
    "the_forks_quit_leaves_the_persons_nvim_as_it_was",
    "end_nvim_restart_and_exec_lua_watched_refuse",
    "getpid_is_never_asked_of_an_attached_pane",
    "a_refused_chunk_never_reaches_the_attached_nvim",
    "a_registered_chunk_carries_the_arguments_the_check_returns",
];

/// Lines the pane prints when it signals a process, ends one by its pid or asks nvim for its pid. None may appear in
/// a case's output: an attached pane does none of these.
const FORBIDDEN_IN_THE_LOG: &[&str] = &[
    "SIGTERM",
    "SIGKILL",
    "watching pid",
    "nvim is pid",
    "answered pid",
    "getpid",
    "asking nvim for its own pid",
    "the fork started pid",
];

/// The probe chunk that makes a buffer one nothing asks about on exit. Sent to a person's nvim it would turn their
/// unsaved buffer into a `nofile` one, so a pane attached to it must never let it through.
const PROBE_SETUP_LUA: &str = "vim.o.guicursor = 'a:block-blinkon0' \
    vim.o.shortmess = vim.o.shortmess .. 'I' \
    vim.o.showmode = false \
    vim.o.ruler = false \
    vim.o.showcmd = false \
    vim.o.number = false \
    vim.o.wrap = false \
    vim.bo.buftype = 'nofile' \
    vim.bo.bufhidden = 'hide' \
    vim.bo.swapfile = false";

/// A chunk the stand-in check lets through: it returns the arguments it was called with.
const ECHO_ARGS_LUA: &str = "return { ... }";

/// The person's session: an unsaved edit in the first buffer, a job, and a `:terminal` in a split below it, with the
/// edit's window current again.
const PERSONS_SESSION_LUA: &str = "\
vim.api.nvim_buf_set_lines(0, 0, -1, false, { 'unsaved text' })
vim.g.tk_buf = vim.api.nvim_get_current_buf()
vim.g.tk_job = vim.fn.jobstart({ 'sleep', '600' })
vim.cmd('botright split')
vim.cmd('terminal sleep 600')
vim.g.tk_term = vim.b.terminal_job_id
vim.g.tk_term_buf = vim.api.nvim_get_current_buf()
vim.cmd('wincmd p')
return vim.g.tk_job > 0 and vim.g.tk_term ~= nil and vim.api.nvim_get_current_buf() == vim.g.tk_buf";

/// A directory of this case's own under the target's scratch directory, removed at the end. Its name is short: a
/// socket's path must fit in `sun_path` (108 bytes).
struct Scratch(PathBuf);

impl Scratch {
    fn new(case: &str) -> Self {
        let index = CASES.iter().position(|c| *c == case).expect("a known case");
        let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("ap-{}-{index}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        Scratch(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The observer's handler answers nothing; it only makes requests.
#[derive(Clone)]
struct Quiet;

impl nvim_rs::Handler for Quiet {
    type Writer = NeovimWriter;
}

/// A second RPC client on the person's server, to set the session up and read what the pane left there.
struct Observer {
    runtime: tokio::runtime::Runtime,
    session: NeovimSession,
}

impl Observer {
    fn try_on(address: &str) -> Option<Observer> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("runtime");
        let session = runtime
            .block_on(NeovimSession::new(
                NeovimInstance::Server {
                    address: address.to_owned(),
                },
                Quiet,
                None,
            ))
            .ok()?;
        Some(Observer { runtime, session })
    }

    fn when_listening(address: &str) -> Observer {
        let deadline = Instant::now() + WAIT;
        loop {
            if let Some(observer) = Observer::try_on(address) {
                return observer;
            }
            assert!(Instant::now() < deadline, "nothing listened at {address}");
            std::thread::sleep(Duration::from_millis(30));
        }
    }

    /// `code`'s answer. nvim queues a request while it waits for a key, so a case that leaves it waiting sends a key
    /// first; the bound turns a forgotten key into a failure rather than a hang.
    fn lua(&self, code: &str) -> Value {
        let call = self.session.neovim.exec_lua(code, vec![]);
        self.runtime
            .block_on(async { tokio::time::timeout(WAIT, call).await })
            .unwrap_or_else(|_| panic!("nvim did not answer within {WAIT:?}:\n{code}"))
            .unwrap_or_else(|e| panic!("the observer's request failed: {e}\n{code}"))
    }

    fn int(&self, code: &str) -> i64 {
        self.lua(code)
            .as_i64()
            .unwrap_or_else(|| panic!("not a number: {code}"))
    }

    fn string(&self, code: &str) -> String {
        match self.lua(code) {
            Value::String(s) => s.into_str().unwrap_or_default(),
            Value::Nil => "<nil>".to_owned(),
            other => panic!("not a string: {code} -> {other:?}"),
        }
    }

    fn truth(&self, code: &str) -> bool {
        self.lua(code)
            .as_bool()
            .unwrap_or_else(|| panic!("not a boolean: {code}"))
    }

    /// Typed keys, through `nvim_input`, which nvim answers at once even while it waits for a key.
    fn input(&self, keys: &str) {
        self.runtime
            .block_on(self.session.neovim.input(keys))
            .expect("nvim_input");
    }

    /// Whether nvim waits for a key, from `nvim_get_mode`, which nvim answers at once even then.
    fn blocking(&self) -> bool {
        let pairs = self
            .runtime
            .block_on(self.session.neovim.get_mode())
            .expect("nvim_get_mode");
        pairs
            .iter()
            .find(|(k, _)| k.as_str() == Some("blocking"))
            .and_then(|(_, v)| v.as_bool())
            .unwrap_or(false)
    }

    fn uis(&self) -> i64 {
        self.int("return #vim.api.nvim_list_uis()")
    }

    fn wait(&self, what: &str, done: impl Fn(&Observer) -> bool) {
        let deadline = Instant::now() + WAIT;
        while !done(self) {
            assert!(Instant::now() < deadline, "timed out waiting for: {what}");
            std::thread::sleep(Duration::from_millis(30));
        }
    }
}

impl Drop for Observer {
    fn drop(&mut self) {
        self.session.hang_up.hang_up();
    }
}

/// The start time of `pid` (`/proc/<pid>/stat`'s 22nd field), which tells the process apart from a later one that
/// reused its number.
fn start_time(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_whitespace().nth(19)?.parse().ok()
}

/// Whether `pid` runs: it exists and is not a zombie.
fn runs(pid: u32) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    let state = stat
        .rfind(')')
        .and_then(|close| stat[close + 1..].split_whitespace().next().map(str::to_owned));
    state.is_some_and(|s| s != "Z")
}

/// The person's terminal: a private tmux server running `nvim --clean -n -i NONE --listen <addr>`, and the TUI's
/// server child, which is the nvim the pane attaches to.
struct Terminal {
    dir: PathBuf,
    tmux_socket: PathBuf,
    addr: PathBuf,
    server: u32,
    server_start: Option<u64>,
}

impl Terminal {
    fn start(dir: &Path) -> Terminal {
        let available = Command::new("tmux").arg("-V").output();
        if !available.is_ok_and(|o| o.status.success()) {
            println!("NOT RUN: tmux is not on PATH, and the person's nvim runs in a terminal");
            panic!("NOT RUN: tmux is required");
        }
        let tmux_socket = dir.join("tm.sock");
        let addr = dir.join("n.sock");
        assert!(
            addr.as_os_str().len() < 100,
            "the socket path is too long for a unix socket: {}",
            addr.display()
        );
        let terminal = Terminal {
            dir: dir.to_owned(),
            tmux_socket,
            addr,
            server: 0,
            server_start: None,
        };
        let addr = terminal.address();
        terminal.tmux(&[
            "new-session",
            "-d",
            "-s",
            "tk",
            "-x",
            "120",
            "-y",
            "30",
            "nvim",
            "--clean",
            "-n",
            "-i",
            "NONE",
            "--listen",
            &addr,
        ]);
        let deadline = Instant::now() + WAIT;
        while !terminal.addr.exists() {
            assert!(
                Instant::now() < deadline,
                "the person's nvim never listened at {addr}; the terminal shows:\n{}",
                terminal.capture()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        terminal
    }

    fn tmux(&self, args: &[&str]) -> Output {
        let output = Command::new("tmux")
            .arg("-S")
            .arg(&self.tmux_socket)
            .args(["-f", "/dev/null"])
            .args(args)
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .env_remove("NVIM")
            .env_remove("NVIM_LISTEN_ADDRESS")
            .env("XDG_CONFIG_HOME", self.dir.join("config"))
            .env("XDG_DATA_HOME", self.dir.join("data"))
            .env("XDG_STATE_HOME", self.dir.join("state"))
            .env("XDG_CACHE_HOME", self.dir.join("cache"))
            .env("NVIM_LOG_FILE", self.dir.join("nvim.log"))
            .env("TERM", "xterm-256color")
            .output()
            .expect("tmux runs");
        assert!(
            output.status.success(),
            "tmux {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    fn capture(&self) -> String {
        String::from_utf8_lossy(&self.tmux(&["capture-pane", "-p", "-t", "tk:0.0"]).stdout).into_owned()
    }

    fn address(&self) -> String {
        self.addr.to_string_lossy().into_owned()
    }

    /// Holds the server's pid, as nvim itself answers it, and checks it is the TUI's `--embed` child.
    fn learn_server(&mut self, observer: &Observer) {
        self.server = u32::try_from(observer.int("return vim.fn.getpid()")).expect("a pid");
        self.server_start = start_time(self.server);
        let argv = std::fs::read(format!("/proc/{}/cmdline", self.server)).expect("the server's argv");
        assert!(
            argv.split(|b| *b == 0).any(|arg| arg == b"--embed"),
            "the person's server should be the TUI's `nvim --embed` child; its argv is {:?}",
            String::from_utf8_lossy(&argv)
        );
    }

    /// The same server process as at the start, still running.
    fn server_alive(&self) -> bool {
        self.server > 0 && runs(self.server) && start_time(self.server) == self.server_start
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        // The session's jobs first, so nothing outlives the case; a key first, since nvim may wait for one.
        if let Some(observer) = Observer::try_on(&self.address()) {
            observer.input("<Esc>");
            let _ = observer.runtime.block_on(async {
                tokio::time::timeout(
                    Duration::from_secs(3),
                    observer.session.neovim.exec_lua(
                        "pcall(vim.fn.jobstop, vim.g.tk_job) pcall(vim.fn.jobstop, vim.g.tk_term)",
                        vec![],
                    ),
                )
                .await
            });
        }
        let _ = Command::new("tmux")
            .arg("-S")
            .arg(&self.tmux_socket)
            .arg("kill-server")
            .output();
        // The TUI's end closes the server's stdin, on which it exits; a server this case started and that is still
        // there after that is ended by its own pid, checked by its start time.
        let deadline = Instant::now() + Duration::from_secs(3);
        while self.server_alive() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        if self.server_alive() {
            unsafe {
                libc::kill(self.server as libc::pid_t, libc::SIGKILL);
            }
        }
    }
}

/// Runs the default main context without blocking until `done` holds; `false` at `limit`.
fn run_until(limit: Duration, mut done: impl FnMut() -> bool) -> bool {
    let context = gtk4::glib::MainContext::default();
    let deadline = Instant::now() + limit;
    loop {
        while context.iteration(false) {}
        if done() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// Runs the default main context for `span`.
fn turns() {
    for _ in 0..100 {
        glib::MainContext::default().iteration(false);
    }
}

/// The person's terminal, its server holding the session, and the observer on it.
struct Person {
    observer: Observer,
    terminal: Terminal,
    _scratch: Scratch,
}

impl Person {
    fn start(case: &str) -> Person {
        let scratch = Scratch::new(case);
        let mut terminal = Terminal::start(scratch.path());
        let observer = Observer::when_listening(&terminal.address());
        terminal.learn_server(&observer);
        observer.wait("the terminal's UI is attached", |o| o.uis() == 1);
        assert!(
            observer.truth(PERSONS_SESSION_LUA),
            "the person's session was not set up"
        );
        println!(
            "[case] the person's server is process {} (the TUI's --embed child), at {}",
            terminal.server,
            terminal.address()
        );
        Person {
            observer,
            terminal,
            _scratch: scratch,
        }
    }

    fn address(&self) -> String {
        self.terminal.address()
    }

    /// What must hold after any way the pane lets go: the same server alive and not dying, the edit unsaved, the job
    /// and the terminal's job running, and only the terminal's UI left.
    fn assert_intact(&self, after: &str) {
        assert!(
            self.terminal.server_alive(),
            "{after}: the person's nvim (pid {}) is gone",
            self.terminal.server
        );
        let o = &self.observer;
        o.wait("only the terminal's UI is left", |o| o.uis() == 1);
        assert_eq!(
            o.int("return vim.fn.getpid()"),
            i64::from(self.terminal.server),
            "{after}: another nvim answers at the address"
        );
        assert_eq!(o.int("return vim.v.dying"), 0, "{after}: nvim is dying");
        assert_eq!(
            o.string("return vim.api.nvim_buf_get_lines(vim.g.tk_buf, 0, -1, false)[1]"),
            "unsaved text",
            "{after}: the edit is gone"
        );
        assert!(
            o.truth("return vim.bo[vim.g.tk_buf].modified"),
            "{after}: the edit is no longer unsaved"
        );
        assert_eq!(
            o.string("return vim.bo[vim.g.tk_buf].buftype"),
            "",
            "{after}: the person's buffer changed type"
        );
        assert_eq!(
            o.int("return vim.fn.jobwait({ vim.g.tk_job }, 0)[1]"),
            -1,
            "{after}: the job ended"
        );
        assert_eq!(
            o.int("return vim.fn.jobwait({ vim.g.tk_term }, 0)[1]"),
            -1,
            "{after}: the terminal's job ended"
        );
        assert_eq!(
            o.string("return vim.bo[vim.g.tk_term_buf].buftype"),
            "terminal",
            "{after}: the terminal buffer is gone"
        );
        println!("[case] {after}: the person's nvim is as it was");
    }
}

/// A shown window holding one pane attached to `address`, running, and a count of its exit reports.
fn attached_pane(
    person: &Person,
    chunk_check: Option<ChunkCheck>,
) -> (gtk4::Window, Rc<NeovideEditorPane>, Rc<Cell<u32>>) {
    let pane = Rc::new(NeovideEditorPane::with_options(NeovideEditorPaneOptions {
        attach: Some(AttachSpec {
            server: person.address(),
        }),
        chunk_check,
        ..Default::default()
    }));
    assert!(pane.is_attached(), "a pane built to attach says so before it starts");
    pane.widget().set_size_request(640, 360);
    let window = gtk4::Window::new();
    window.set_default_size(640, 360);
    window.set_child(Some(pane.widget()));
    window.present();
    assert!(
        run_until(Duration::from_secs(30), || pane.is_running()),
        "the pane should attach to the person's nvim"
    );
    assert!(pane.is_attached());
    let exits = Rc::new(Cell::new(0));
    {
        let exits = exits.clone();
        pane.on_exited_unrequested(move || exits.set(exits.get() + 1));
    }
    person
        .observer
        .wait("the pane is the server's second UI", |o| o.uis() == 2);
    assert_eq!(
        person.observer.string(
            "for _, u in ipairs(vim.api.nvim_list_uis()) do \
               local c = vim.api.nvim_get_chan_info(u.chan).client \
               if c and c.name == 'neovide' then return c.name end \
             end"
        ),
        "neovide",
        "one of the server's UIs is the pane's"
    );
    (window, pane, exits)
}

type Answer = Arc<Mutex<Option<Result<Value, String>>>>;

/// `exec_lua` through the pane, with the slot its reply fills.
fn exec(pane: &NeovideEditorPane, code: &'static str, args: Vec<Value>) -> (bool, Answer) {
    let slot: Answer = Arc::new(Mutex::new(None));
    let theirs = slot.clone();
    let accepted = pane.exec_lua(
        code,
        args,
        Box::new(move |answer| *theirs.lock().unwrap() = Some(answer)),
    );
    (accepted, slot)
}

fn a_shutdown_leaves_the_persons_nvim_as_it_was() {
    let person = Person::start("a_shutdown_leaves_the_persons_nvim_as_it_was");
    let (_window, pane, exits) = attached_pane(&person, None);
    let started = Instant::now();
    assert!(pane.shutdown(), "the shutdown should see the channel end");
    let took = started.elapsed();
    println!("[case] the shutdown returned after {took:?}");

    assert_eq!(pane.ended(), Some(PaneEnded::Detached), "the pane let go of the nvim");
    turns();
    assert_eq!(
        exits.get(),
        0,
        "a shutdown the host asked for is not reported as an exit"
    );
    person.assert_intact("a shutdown");
}

fn a_drop_without_shutdown_leaves_the_persons_nvim_as_it_was() {
    let person = Person::start("a_drop_without_shutdown_leaves_the_persons_nvim_as_it_was");
    let (window, pane, _exits) = attached_pane(&person, None);
    window.destroy();
    drop(window);
    drop(pane);
    let left = run_until(Duration::from_secs(10), || person.observer.uis() == 1);
    assert!(left, "the dropped pane is still a UI of the person's nvim");
    person.assert_intact("a drop without shutdown");
}

fn a_pane_dropped_before_it_attached_never_attaches() {
    let person = Person::start("a_pane_dropped_before_it_attached_never_attaches");
    let pane = NeovideEditorPane::with_options(NeovideEditorPaneOptions {
        attach: Some(AttachSpec {
            server: person.address(),
        }),
        ..Default::default()
    });
    pane.widget().set_size_request(640, 360);
    let window = gtk4::Window::new();
    window.set_default_size(640, 360);
    window.set_child(Some(pane.widget()));
    window.present();
    // The window keeps the widget, and with it the frames that would attach; the pane itself is gone.
    let closed = pane.testing_closed_observer();
    let widget = pane.widget().clone();
    let paints = Rc::new(Cell::new(0u32));
    let seen = paints.clone();
    let clock = widget.frame_clock().expect("the presented widget has a frame clock");
    // The pane handles render with Stop; after-paint still observes the completed frame.
    let paint_event = clock.connect_after_paint(move |_| {
        seen.set(seen.get() + 1);
    });
    drop(pane);
    assert!(closed(), "Drop cancelled the unstarted launch");
    for _ in 0..100 {
        let before = paints.get();
        widget.queue_render();
        assert!(
            run_until(Duration::from_secs(60), || paints.get() > before),
            "widget did not paint"
        );
        assert!(closed(), "a paint reopened the dropped pane");
        assert_eq!(person.observer.uis(), 1, "a dropped pane attached");
    }
    clock.disconnect(paint_event);
    window.destroy();
    drop(window);
    turns();
    person.assert_intact("a pane dropped before it attached");
}

fn the_hang_up_handle_leaves_the_persons_nvim_as_it_was() {
    let person = Person::start("the_hang_up_handle_leaves_the_persons_nvim_as_it_was");
    let (_window, pane, exits) = attached_pane(&person, None);
    let handle = pane.hang_up_handle().expect("an attached pane offers its hang-up");
    std::thread::spawn(move || handle.hang_up())
        .join()
        .expect("the hang-up thread");
    assert!(
        run_until(Duration::from_secs(10), || exits.get() == 1),
        "the pane should report the session's end once"
    );
    assert_eq!(pane.ended(), Some(PaneEnded::Detached), "the pane let go of the nvim");
    assert!(!pane.is_running());
    turns();
    assert_eq!(exits.get(), 1, "the end is reported once");
    person.assert_intact("the hang-up handle");
    assert!(
        pane.shutdown(),
        "a shutdown after the hang-up has nothing left to wait for"
    );
    assert!(pane.release_exited(), "the ended pane is released");
}

fn a_hang_up_while_nvim_waits_for_a_key_leaves_the_persons_nvim_as_it_was() {
    let person = Person::start("a_hang_up_while_nvim_waits_for_a_key_leaves_the_persons_nvim_as_it_was");
    let (_window, pane, exits) = attached_pane(&person, None);
    person.observer.input("f");
    person
        .observer
        .wait("nvim waits for the character after `f`", Observer::blocking);
    let handle = pane.hang_up_handle().expect("an attached pane offers its hang-up");
    std::thread::spawn(move || handle.hang_up())
        .join()
        .expect("the hang-up thread");
    turns();
    let ended_while_waiting = exits.get() == 1;
    let still_waiting = person.observer.blocking();
    println!(
        "[case] measured: the session's end {} reported while nvim waited for a key; nvim {} waiting",
        if ended_while_waiting { "was" } else { "was not" },
        if still_waiting { "is still" } else { "is no longer" },
    );
    assert!(still_waiting, "the hang-up answered the key nvim was waiting for");
    // The person's key, as they would press it in the terminal.
    person.observer.input("<Esc>");
    assert!(
        run_until(Duration::from_secs(10), || exits.get() == 1),
        "the pane should report the session's end once nvim took the key"
    );
    assert_eq!(pane.ended(), Some(PaneEnded::Detached));
    person.assert_intact("a hang-up while nvim waited for a key");
}

fn the_forks_quit_leaves_the_persons_nvim_as_it_was() {
    let person = Person::start("the_forks_quit_leaves_the_persons_nvim_as_it_was");
    let (_window, pane, exits) = attached_pane(&person, None);
    let chan = person.observer.int("return vim.g.neovide_channel_id");
    assert!(chan > 0, "the fork's channel id is set");
    person
        .observer
        .lua(&format!("vim.rpcnotify({chan}, 'neovide.exec_detach_handler')"));
    assert!(
        run_until(Duration::from_secs(10), || exits.get() == 1),
        "the pane should report the session's end once"
    );
    assert_eq!(
        pane.ended(),
        Some(PaneEnded::Exited),
        "nvim closed the channel from its side; the pane did not hang up"
    );
    person.assert_intact("the fork's quit");
}

fn end_nvim_restart_and_exec_lua_watched_refuse() {
    let person = Person::start("end_nvim_restart_and_exec_lua_watched_refuse");
    let (_window, pane, exits) = attached_pane(&person, None);

    assert!(!pane.end_nvim(), "end_nvim is refused on an attached pane");
    assert!(
        !pane.exec_lua_watched("vim.g.tk_watched = 1"),
        "exec_lua_watched, the quit's carrier, is refused on an attached pane"
    );
    assert!(
        pane.watched_call().is_none(),
        "a refused watched request leaves no watch"
    );
    let refused = pane
        .restart(NeovideEditorPaneOptions::default())
        .expect_err("restart is refused on an attached pane");
    assert_eq!(refused.state, "Attached", "the refusal names the attach");
    assert!(
        refused.to_string().contains("attached"),
        "the refusal says why: {refused}"
    );

    // With no chunk check, nothing reaches the person's nvim through exec_lua either: the refusal is answered at once.
    let (accepted, answer) = exec(&pane, "vim.g.tk_plain = 1", vec![]);
    assert!(accepted, "the call is answered, with its refusal");
    assert!(
        matches!(&*answer.lock().unwrap(), Some(Err(_))),
        "an attached pane with no chunk check answers a refusal at once: {:?}",
        answer.lock().unwrap()
    );

    turns();
    assert!(pane.is_running(), "the refusals left the session alone");
    assert_eq!(exits.get(), 0);
    let o = &person.observer;
    assert_eq!(o.uis(), 2, "the pane is still attached");
    assert_eq!(o.string("return vim.g.tk_watched"), "<nil>", "the watched request ran");
    assert_eq!(o.string("return vim.g.tk_plain"), "<nil>", "the unchecked chunk ran");

    assert!(pane.shutdown());
    assert!(pane.release_exited(), "the ended pane is released");
    let refused = pane
        .restart(NeovideEditorPaneOptions::default())
        .expect_err("an attached pane that ended starts no nvim of its own either");
    assert_eq!(refused.state, "Attached");
    assert!(!pane.is_starting(), "the refused restart left the pane as it was");
    assert!(!pane.end_nvim(), "end_nvim after the end still does nothing");
    person.assert_intact("the refusals and a shutdown");
}

fn getpid_is_never_asked_of_an_attached_pane() {
    let person = Person::start("getpid_is_never_asked_of_an_attached_pane");
    let (_window, pane, _exits) = attached_pane(&person, None);
    // An owned pane asks at once, on a thread of its own, and logs the answer; give that every chance to happen.
    turns();
    assert!(pane.is_running());
    assert!(pane.shutdown());
    turns();
    person.assert_intact("a pane that ran attached");
    // The parent reads this child's whole output: no pid question, no answer, no signal.
}

/// The stand-in for the window's chunk rule: `ECHO_ARGS_LUA` is registered and gets one more argument; every other
/// chunk is refused. Records each chunk it was asked about.
fn stand_in_check(seen: Rc<RefCell<Vec<&'static str>>>) -> ChunkCheck {
    ChunkCheck::new(move |code, mut args| {
        seen.borrow_mut().push(code);
        if code == ECHO_ARGS_LUA {
            args.push(Value::Map(vec![(Value::from("takeover_chan"), Value::from(42))]));
            Ok(args)
        } else {
            Err(ChunkRefused {
                reason: "the Lua chunk is refused in a takeover".to_owned(),
            })
        }
    })
}

fn a_refused_chunk_never_reaches_the_attached_nvim() {
    let person = Person::start("a_refused_chunk_never_reaches_the_attached_nvim");
    let o = &person.observer;
    let guicursor_before = o.string("return vim.o.guicursor");
    let seen = Rc::new(RefCell::new(Vec::new()));
    let (_window, pane, _exits) = attached_pane(&person, Some(stand_in_check(seen.clone())));

    let (accepted, answer) = exec(&pane, PROBE_SETUP_LUA, vec![]);
    assert!(accepted, "the call is answered, with its refusal");
    match &*answer.lock().unwrap() {
        Some(Err(reason)) => assert_eq!(reason, "the Lua chunk is refused in a takeover"),
        other => panic!("the refusal should be answered at once, before anything ran; got {other:?}"),
    }
    assert_eq!(*seen.borrow(), vec![PROBE_SETUP_LUA], "the check saw the chunk");

    // Anything sent would have arrived by now: a chunk sent after it through the same pane is answered first.
    let (accepted, echoed) = exec(&pane, ECHO_ARGS_LUA, vec![]);
    assert!(accepted);
    assert!(
        run_until(WAIT, || echoed.lock().unwrap().is_some()),
        "the registered chunk was answered"
    );
    assert_eq!(
        o.string("return vim.bo[vim.g.tk_buf].buftype"),
        "",
        "the refused chunk ran"
    );
    assert_eq!(
        o.string("return vim.o.guicursor"),
        guicursor_before,
        "the refused chunk ran"
    );
    assert!(o.truth("return vim.o.showmode"), "the refused chunk ran");

    assert!(pane.shutdown());
    person.assert_intact("a refused chunk");
}

fn a_registered_chunk_carries_the_arguments_the_check_returns() {
    let person = Person::start("a_registered_chunk_carries_the_arguments_the_check_returns");
    let seen = Rc::new(RefCell::new(Vec::new()));
    let (_window, pane, _exits) = attached_pane(&person, Some(stand_in_check(seen.clone())));

    let (accepted, answer) = exec(&pane, ECHO_ARGS_LUA, vec![Value::from("first")]);
    assert!(accepted, "a registered chunk is sent");
    assert!(
        run_until(WAIT, || answer.lock().unwrap().is_some()),
        "nvim answered the registered chunk"
    );
    let answer = answer.lock().unwrap().take().expect("an answer");
    let values = answer.expect("nvim ran the registered chunk");
    let values = values.as_array().expect("the chunk returns its arguments as a list");
    assert_eq!(values.len(), 2, "the caller's argument and the check's: {values:?}");
    assert_eq!(values[0].as_str(), Some("first"));
    let options = values[1].as_map().expect("the check's argument is a table");
    assert_eq!(options.len(), 1);
    assert_eq!(options[0].0.as_str(), Some("takeover_chan"));
    assert_eq!(options[0].1.as_i64(), Some(42));
    assert_eq!(*seen.borrow(), vec![ECHO_ARGS_LUA]);

    assert!(pane.shutdown());
    person.assert_intact("a registered chunk");
}

fn run_case(case: &str) {
    match case {
        "a_shutdown_leaves_the_persons_nvim_as_it_was" => a_shutdown_leaves_the_persons_nvim_as_it_was(),
        "a_drop_without_shutdown_leaves_the_persons_nvim_as_it_was" => {
            a_drop_without_shutdown_leaves_the_persons_nvim_as_it_was()
        }
        "a_pane_dropped_before_it_attached_never_attaches" => a_pane_dropped_before_it_attached_never_attaches(),
        "the_hang_up_handle_leaves_the_persons_nvim_as_it_was" => {
            the_hang_up_handle_leaves_the_persons_nvim_as_it_was()
        }
        "a_hang_up_while_nvim_waits_for_a_key_leaves_the_persons_nvim_as_it_was" => {
            a_hang_up_while_nvim_waits_for_a_key_leaves_the_persons_nvim_as_it_was()
        }
        "the_forks_quit_leaves_the_persons_nvim_as_it_was" => the_forks_quit_leaves_the_persons_nvim_as_it_was(),
        "end_nvim_restart_and_exec_lua_watched_refuse" => end_nvim_restart_and_exec_lua_watched_refuse(),
        "getpid_is_never_asked_of_an_attached_pane" => getpid_is_never_asked_of_an_attached_pane(),
        "a_refused_chunk_never_reaches_the_attached_nvim" => a_refused_chunk_never_reaches_the_attached_nvim(),
        "a_registered_chunk_carries_the_arguments_the_check_returns" => {
            a_registered_chunk_carries_the_arguments_the_check_returns()
        }
        other => panic!("unknown case {other}"),
    }
}

fn main() {
    if let Some(case) = std::env::var_os(CASE_ENV) {
        let case = case.to_string_lossy().into_owned();
        if let Ok(Some(kind)) = neovide_editor::detach_stdin_from_nvim() {
            println!("[stdin] a {kind} on stdin would reach nvim as a buffer; stdin is /dev/null now");
        }
        let _server = own_x_server::init_gtk("attached_pane_with_real_nvim", "1024x768x24");
        run_case(&case);
        println!("RESULT pass {case}");
        return;
    }
    let args: Vec<String> = std::env::args().collect();
    if !args.iter().any(|a| a == "--ignored" || a == "--include-ignored") {
        println!("attached_pane_with_real_nvim: ignored (spawns real nvims and tmux); run with `-- --ignored`");
        return;
    }
    let filter: Vec<&String> = args.iter().skip(1).filter(|a| !a.starts_with("--")).collect();
    let mut failed = Vec::new();
    for case in CASES {
        if !filter.is_empty() && !filter.iter().any(|f| case.contains(f.as_str())) {
            continue;
        }
        let out = Command::new(std::env::current_exe().unwrap())
            .env(CASE_ENV, case)
            .output()
            .expect("re-exec of the test binary failed");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let passed = stdout.lines().any(|l| l == format!("RESULT pass {case}"));
        // The case's own lines (its markers and result, which name the case) are not the pane's log.
        let forbidden: Vec<&str> = stdout
            .lines()
            .filter(|line| !line.starts_with("RESULT ") && !line.starts_with("[case] "))
            .filter(|line| FORBIDDEN_IN_THE_LOG.iter().any(|f| line.contains(f)))
            .collect();
        if passed && forbidden.is_empty() {
            for line in stdout.lines().filter(|line| line.starts_with("[case] ")) {
                println!("{line}");
            }
            println!("test {case} ... ok");
        } else {
            println!("{stdout}");
            if !forbidden.is_empty() {
                println!("--- {case}: the pane's log signalled or asked for a pid: {forbidden:#?}");
            }
            eprintln!("--- {case} stderr ---\n{}", String::from_utf8_lossy(&out.stderr));
            println!("test {case} ... FAILED ({})", out.status);
            failed.push(*case);
        }
    }
    assert!(
        failed.is_empty(),
        "attached_pane_with_real_nvim: failed cases: {failed:?}"
    );
}
