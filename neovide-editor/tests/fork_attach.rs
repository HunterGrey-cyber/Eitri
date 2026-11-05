//! The pinned fork's `LiveHarness` attached to an nvim that is already running (`LiveHarnessOptions::server`).
//!
//! Every case starts its own `nvim --headless --listen` server, attaches a harness to it and watches the server
//! through a second RPC client (the observer): the harness reaches the server as its one UI and starts no nvim of its
//! own; a socket held by another uid is refused before any request; a server that does not answer the setup within
//! 4 s fails the launch and is left with nothing of the attempt; a shutdown ends at the channel's EOF and leaves the
//! server running with its unsaved text; `remote_clipboard` off leaves the server's `g:clipboard` as it was; the
//! harness's hang-up handle, used from another thread, closes the connection; and a `:connect` or `:restart` typed in
//! the attached nvim is reported as the session leaving (with the restarted server's address), never followed, while a
//! restart an unsaved buffer refused keeps the session.
//!
//! Each case runs in a child copy of this binary on its own `Xvfb` (winit and the clipboard connect to a display even
//! though no window is made); the parent connects to nothing. Ignored unless asked for, because it spawns real nvims:
//!
//!     cargo test -p neovide-editor --test fork_attach -- --ignored
//!
//! The other-uid case holds its server as a uid of this user's subordinate range (`unshare --map-auto`, which needs
//! `newuidmap` and an `/etc/subuid` entry); where that cannot be had it prints `NOT RUN` with the reason, and the
//! fork's own unit test of the owner check (a stand-in uid) still holds the refusal.
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use neovide::{
    bridge::{
        session::{NeovimInstance, NeovimSession},
        NeovimWriter,
    },
    live_harness::{Left, LiveEvent, LiveHarness, LiveHarnessOptions},
    units::{GridSize, PixelRect},
};
use rmpv::Value;
use skia_safe::{surfaces, Surface};

#[path = "../../shell/tests/support/own_x_server.rs"]
mod own_x_server;

const CANVAS: (i32, i32) = (640, 320);
const FRAME_DT: f32 = 1.0 / 60.0;
const CASE_ENV: &str = "EITRI_FORK_ATTACH_CASE";
const WAIT: Duration = Duration::from_secs(15);

const CASES: &[&str] = &[
    "attach_reaches_a_listening_server_as_its_only_ui",
    "attach_refuses_a_socket_of_another_uid",
    "attach_setup_past_four_seconds_fails_the_launch",
    "shutdown_returns_at_eof",
    "no_remote_clipboard_leaves_g_clipboard_alone",
    "the_editor_attaches_through_options_and_hangs_up",
    "connect_in_attach_mode_reports_session_left_connect",
    "restart_in_attach_mode_reports_session_left_restart_with_the_address",
    "restart_refused_by_an_unsaved_buffer_keeps_the_session",
    "a_hang_up_while_a_refused_restart_waits_at_a_prompt_reports_nothing",
    "attach_then_cleanup_leaves_the_nvim_as_it_was",
    "a_channel_that_ends_without_the_cleanup_leaves_the_nvim_as_it_was",
];

/// A directory of this case's own under the target's scratch directory, removed at the end. Its name is short: a
/// socket's path must fit in `sun_path` (108 bytes), or nvim refuses `--listen`.
struct Scratch(PathBuf);

impl Scratch {
    fn new(case: &str) -> Self {
        let index = CASES.iter().position(|c| *c == case).expect("a known case");
        let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("fa-{}-{index}", std::process::id()));
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

/// An nvim server the case started, ended by its own pid when the case is done.
struct Server {
    child: Child,
    socket: PathBuf,
}

impl Server {
    /// `nvim --clean --headless --listen <dir>/<name>.sock`, with its state and log inside `dir`.
    fn start(dir: &Path, name: &str) -> Server {
        let socket = dir.join(format!("{name}.sock"));
        assert!(
            socket.as_os_str().len() < 100,
            "the socket path is too long for a unix socket: {}",
            socket.display()
        );
        let state = dir.join(format!("{name}-state"));
        let child = Command::new("nvim")
            .args(["--clean", "--headless", "-n", "--listen"])
            .arg(&socket)
            .current_dir(dir)
            .env("XDG_STATE_HOME", &state)
            .env("XDG_CONFIG_HOME", &state)
            .env("XDG_DATA_HOME", &state)
            .env("XDG_CACHE_HOME", &state)
            .env("NVIM_LOG_FILE", dir.join(format!("{name}.log")))
            .stdin(Stdio::null())
            .spawn()
            .expect("nvim must be on PATH for this test");
        let mut server = Server { child, socket };
        server.wait_listening();
        server
    }

    fn wait_listening(&mut self) {
        let deadline = Instant::now() + WAIT;
        while !self.socket.exists() {
            if let Ok(Some(status)) = self.child.try_wait() {
                panic!("the server exited before it listened ({status})");
            }
            assert!(
                Instant::now() < deadline,
                "the server never listened at {}",
                self.socket.display()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn address(&self) -> String {
        self.socket.to_string_lossy().into_owned()
    }

    fn is_running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The observer's handler answers nothing; it only makes requests.
#[derive(Clone)]
struct Quiet;

impl nvim_rs::Handler for Quiet {
    type Writer = NeovimWriter;
}

/// A second RPC client on the server, through the fork's own session code (no owner check), to read what the
/// harness did there.
struct Observer {
    runtime: tokio::runtime::Runtime,
    session: NeovimSession,
}

impl Observer {
    fn on(address: &str) -> Observer {
        Observer::try_on(address).expect("the observer reaches the server")
    }

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

    /// The first observer that reaches `address` within `WAIT`, for a server that is still starting.
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

    fn lua(&self, code: &str) -> Value {
        self.runtime
            .block_on(self.session.neovim.exec_lua(code, vec![]))
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

    /// Typed keys, through `nvim_input`, which nvim answers at once even while it runs a command.
    fn input(&self, keys: &str) {
        self.runtime
            .block_on(self.session.neovim.input(keys))
            .expect("nvim_input");
    }

    fn uis(&self) -> i64 {
        self.int("return #vim.api.nvim_list_uis()")
    }

    /// The server's socket channels: the observer's own and any other client's.
    fn socket_channels(&self) -> i64 {
        self.int("local n = 0 for _, c in ipairs(vim.api.nvim_list_chans()) do if c.stream == 'socket' then n = n + 1 end end return n")
    }

    /// Waits until `done` holds, failing after `WAIT` with `what`.
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

fn options(address: &str) -> LiveHarnessOptions {
    LiveHarnessOptions {
        grid_size: Some(GridSize::new(40u32, 8u32)),
        server: Some(address.to_owned()),
        startup_message_capture: false,
        minimum_nvim_version: Some(neovide_editor::NVIM_FLOOR),
        remote_clipboard: false,
        ..Default::default()
    }
}

struct Editor {
    harness: LiveHarness,
    surface: Surface,
}

impl Editor {
    fn attach(options: LiveHarnessOptions) -> Editor {
        let harness = LiveHarness::with_options(options).unwrap_or_else(|e| panic!("the attach failed: {e:#}"));
        let surface = surfaces::raster_n32_premul(CANVAS).expect("raster surface");
        let mut editor = Editor { harness, surface };
        editor.render_until("the attached editor is ready", |h| h.is_ready());
        editor
    }

    fn frame(&mut self) {
        let region = PixelRect::from_min_max((0.0, 0.0), (CANVAS.0 as f32, CANVAS.1 as f32));
        self.harness
            .render_frame(self.surface.canvas(), Some(&region), FRAME_DT);
    }

    fn render_until(&mut self, what: &str, done: impl Fn(&LiveHarness) -> bool) {
        let deadline = Instant::now() + WAIT;
        loop {
            self.frame();
            if done(&self.harness) {
                return;
            }
            assert!(Instant::now() < deadline, "timed out waiting for: {what}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

/// Pids of live processes whose command line or environment holds `needle`.
fn processes_with(needle: &str) -> Vec<u32> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir("/proc").into_iter().flatten().flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        for file in ["cmdline", "environ"] {
            if let Ok(bytes) = std::fs::read(entry.path().join(file)) {
                if String::from_utf8_lossy(&bytes).contains(needle) {
                    found.push(pid);
                    break;
                }
            }
        }
    }
    found
}

fn attach_reaches_a_listening_server_as_its_only_ui(scratch: &Scratch) {
    let mut server = Server::start(scratch.path(), "s");
    let observer = Observer::on(&server.address());
    assert_eq!(observer.uis(), 0, "a headless server starts with no UI");
    let cwd_before = observer.string("return vim.fn.getcwd()");

    // What would start an nvim of the harness's own: none of it may be used, since nothing is started.
    let marker = format!(
        "fork_attach_marker_{}_{}",
        std::process::id(),
        std::time::UNIX_EPOCH.elapsed().unwrap().as_nanos()
    );
    let elsewhere = scratch.path().join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    let mut editor = Editor::attach(LiveHarnessOptions {
        extra_nvim_args: vec!["--cmd".to_owned(), format!("let g:{marker} = 1")],
        child_env: vec![("FORK_ATTACH_MARKER".to_owned(), marker.clone())],
        cwd: Some(elsewhere),
        ..options(&server.address())
    });

    assert_eq!(observer.uis(), 1, "the harness is the server's one UI");
    assert_eq!(
        observer.string("return vim.api.nvim_get_chan_info(vim.api.nvim_list_uis()[1].chan).client.name"),
        "neovide",
        "the UI is the harness's"
    );
    assert!(
        processes_with(&marker).is_empty(),
        "an nvim was started with the harness's own arguments or environment"
    );
    assert_eq!(observer.string(&format!("return vim.g.{marker}")), "<nil>");
    assert_eq!(
        observer.string("return vim.fn.getcwd()"),
        cwd_before,
        "the server's cwd moved"
    );

    editor.harness.send_text_input("ihello<Esc>");
    observer.wait("the keys reached the server", |o| {
        o.string("return vim.fn.getline(1)") == "hello"
    });

    assert!(editor.harness.shutdown(), "the shutdown did not see the channel end");
    observer.wait("the server has no UI after the shutdown", |o| o.uis() == 0);
    assert!(server.is_running(), "the server ended with the harness");
}

/// Whether this machine can run a process as a uid of this user's subordinate range, or why not.
fn second_uid_available() -> Result<(), String> {
    let runs = |program: &str, args: &[&str]| {
        Command::new(program)
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    };
    for tool in ["unshare", "setpriv"] {
        if !runs(tool, &["--version"]) {
            return Err(format!("no `{tool}`"));
        }
    }
    if !runs("unshare", &["--user", "--map-root-user", "--map-auto", "--", "true"]) {
        return Err("`unshare --map-auto` fails (no `newuidmap`, or no `/etc/subuid` range for this user)".to_owned());
    }
    Ok(())
}

/// Starts `nvim --listen` as a uid of this user's subordinate range. Only called once
/// [`second_uid_available`] said it can be done, so any failure here is the test's.
fn start_as_another_uid(dir: &Path) -> Server {
    // The other uid creates its socket in this directory, under the target's private (0700) directories: it keeps
    // `CAP_DAC_OVERRIDE` in its namespace, which reaches files whose owner is mapped there (this user, the namespace's
    // root), so nothing here is opened up to other users.
    let socket = dir.join("other.sock");
    let child = Command::new("unshare")
        .args(["--user", "--map-root-user", "--map-auto", "--"])
        .args(["setpriv", "--reuid=1", "--regid=1", "--clear-groups"])
        .args(["--inh-caps=+dac_override", "--ambient-caps=+dac_override", "--"])
        .args(["sh", "-c", "umask 0; exec \"$0\" \"$@\""])
        .args(["nvim", "--clean", "--headless", "-n", "--listen"])
        .arg(&socket)
        .current_dir(dir)
        .env("NVIM_LOG_FILE", dir.join("other.log"))
        .env("XDG_STATE_HOME", dir)
        .env("XDG_CONFIG_HOME", dir)
        .env("XDG_DATA_HOME", dir)
        .env("XDG_CACHE_HOME", dir)
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("unshare");
    let mut server = Server { child, socket };
    let deadline = Instant::now() + WAIT;
    while !server.socket.exists() {
        if let Ok(Some(status)) = server.child.try_wait() {
            let mut stderr = String::new();
            if let Some(mut pipe) = server.child.stderr.take() {
                use std::io::Read;
                let _ = pipe.read_to_string(&mut stderr);
            }
            panic!("the server as another uid did not start ({status}): {}", stderr.trim());
        }
        assert!(Instant::now() < deadline, "the server as another uid never listened");
        std::thread::sleep(Duration::from_millis(20));
    }
    let owner = std::fs::metadata(&server.socket).expect("the other uid's socket").uid();
    let own = unsafe { libc::geteuid() };
    assert_ne!(owner, own, "the socket is held by this uid after all");
    server
}

/// Removes what the other uid wrote when dropped, as the namespace's root, which may remove files of its mapped uids.
/// Declared before the server, so the server has ended first.
struct ClearAsNamespaceRoot(PathBuf);

impl Drop for ClearAsNamespaceRoot {
    fn drop(&mut self) {
        let _ = Command::new("unshare")
            .args(["--user", "--map-root-user", "--map-auto", "--", "find"])
            .arg(&self.0)
            .args(["-mindepth", "1", "-delete"])
            .stderr(Stdio::null())
            .status();
    }
}

fn attach_refuses_a_socket_of_another_uid(scratch: &Scratch) {
    let dir = scratch.path().join("other");
    std::fs::create_dir_all(&dir).unwrap();
    if let Err(why) = second_uid_available() {
        println!("fork_attach: NOT RUN attach_refuses_a_socket_of_another_uid: no second uid ({why})");
        return;
    }
    let _clear = ClearAsNamespaceRoot(dir.clone());
    let mut server = start_as_another_uid(&dir);
    let owner = std::fs::metadata(&server.socket).unwrap().uid();
    println!("fork_attach: the server's socket is held by uid {owner}");
    let observer = Observer::on(&server.address());

    let result = LiveHarness::with_options(options(&server.address()));
    let error = match result {
        Ok(mut harness) => {
            harness.shutdown();
            panic!("a socket held by uid {owner} was attached to");
        }
        Err(e) => format!("{e:#}"),
    };
    println!("fork_attach: refused: {error}");
    assert!(
        error.contains(&format!("uid {owner}")),
        "the refusal names the socket's owner: {error}"
    );

    observer.wait("only the observer is left on the server", |o| o.socket_channels() == 1);
    assert_eq!(observer.uis(), 0, "a refused attach left a UI");
    assert_eq!(
        observer.string("return vim.g.neovide and 'set' or nil"),
        "<nil>",
        "a request reached the server"
    );
    assert!(server.is_running());
}

fn attach_setup_past_four_seconds_fails_the_launch(scratch: &Scratch) {
    let server = Server::start(scratch.path(), "s");
    let observer = Observer::on(&server.address());
    let done = scratch.path().join("held-done");
    // The server runs a shell command for 6 s and answers nothing else meanwhile; the file marks its end.
    observer.input(&format!(":silent !sleep 6; touch {}<CR>", done.display()));
    std::thread::sleep(Duration::from_millis(500));
    assert!(!done.exists());

    let started = Instant::now();
    let result = LiveHarness::with_options(options(&server.address()));
    let took = started.elapsed();
    let held = !done.exists();
    let error = match result {
        Ok(mut harness) => {
            harness.shutdown();
            panic!("the attach succeeded on a server held for 6 s (after {took:?})");
        }
        Err(e) => format!("{e:#}"),
    };
    println!("fork_attach: the launch failed after {took:?}: {error}");
    assert!(held, "the launch ended only after the server came free ({took:?})");
    assert!(error.contains("4 s"), "the failure names the setup's bound: {error}");

    let deadline = Instant::now() + WAIT;
    while !done.exists() {
        assert!(Instant::now() < deadline, "the server's command never finished");
        std::thread::sleep(Duration::from_millis(50));
    }
    observer.wait("only the observer is left on the server", |o| o.socket_channels() == 1);
    assert_eq!(observer.uis(), 0, "the failed attach left a UI");
    assert_eq!(
        observer.string("return vim.g.neovide and 'set' or nil"),
        "<nil>",
        "the failed attach changed the server"
    );
}

fn shutdown_returns_at_eof(scratch: &Scratch) {
    let mut server = Server::start(scratch.path(), "s");
    let observer = Observer::on(&server.address());
    let mut editor = Editor::attach(options(&server.address()));
    editor.harness.send_text_input("ikept<Esc>");
    observer.wait("the text reached the server", |o| {
        o.string("return vim.fn.getline(1)") == "kept"
    });

    let started = Instant::now();
    let saw_the_end = editor.harness.shutdown();
    println!("fork_attach: the shutdown returned after {:?}", started.elapsed());
    assert!(saw_the_end, "the shutdown did not see the channel's EOF");
    assert!(editor.harness.has_neovim_exited());

    observer.wait("the server lost its UI", |o| o.uis() == 0);
    observer.wait("only the observer is left on the server", |o| o.socket_channels() == 1);
    assert!(server.is_running(), "the server ended with the harness");
    assert_eq!(
        observer.string("return vim.fn.getline(1)"),
        "kept",
        "the unsaved text is gone"
    );
    assert_eq!(
        observer.string("return vim.bo.modified and 'modified' or 'saved'"),
        "modified"
    );
}

const PERSONS_CLIPBOARD: &str = "vim.g.clipboard = { name = 'the-persons-own', \
     copy = { ['+'] = { 'true' }, ['*'] = { 'true' } }, paste = { ['+'] = { 'true' }, ['*'] = { 'true' } } }";

fn clipboard_after_attach(server: &Server, observer: &Observer, remote_clipboard: bool) -> String {
    let mut editor = Editor::attach(LiveHarnessOptions {
        remote_clipboard,
        ..options(&server.address())
    });
    // The fork's setup runs before the UI attaches, so it has run in full once the harness is ready.
    let name = observer.string("return vim.g.clipboard and vim.g.clipboard.name or nil");
    assert!(editor.harness.shutdown());
    name
}

fn no_remote_clipboard_leaves_g_clipboard_alone(scratch: &Scratch) {
    // A person with a provider of their own keeps it.
    let own = Server::start(scratch.path(), "own");
    let own_observer = Observer::on(&own.address());
    own_observer.lua(PERSONS_CLIPBOARD);
    assert_eq!(clipboard_after_attach(&own, &own_observer, false), "the-persons-own");

    // A person with none is left with none.
    let none = Server::start(scratch.path(), "none");
    let none_observer = Observer::on(&none.address());
    assert_eq!(clipboard_after_attach(&none, &none_observer, false), "<nil>");

    // With the setting on, the fork does put its own in, so the two above are the setting's doing.
    let on = Server::start(scratch.path(), "on");
    let on_observer = Observer::on(&on.address());
    on_observer.lua(PERSONS_CLIPBOARD);
    assert_eq!(clipboard_after_attach(&on, &on_observer, true), "neovide");
}

fn the_editor_attaches_through_options_and_hangs_up(scratch: &Scratch) {
    let mut server = Server::start(scratch.path(), "s");
    let observer = Observer::on(&server.address());
    let mut editor = Editor::attach(options(&server.address()));
    assert_eq!(observer.uis(), 1);

    let hang_up = editor
        .harness
        .hang_up_handle()
        .expect("an attached harness has a hang-up handle");
    // The handle is used from another thread, as a worker does while the harness stays on its own.
    std::thread::spawn(move || hang_up.hang_up())
        .join()
        .expect("the hang-up thread");
    editor.render_until("the harness saw its connection end", |h| h.has_neovim_exited());

    observer.wait("the server lost its UI", |o| o.uis() == 0);
    observer.wait("only the observer is left on the server", |o| o.socket_channels() == 1);
    assert!(server.is_running(), "the hang-up ended the server");
    assert!(
        editor.harness.shutdown(),
        "a shutdown after the hang-up sees the end at once"
    );
}

/// Whether a process with this pid is alive (a zombie counts as gone).
fn pid_alive(pid: u32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => !stat
            .rsplit_once(") ")
            .is_some_and(|(_, rest)| rest.starts_with('Z') || rest.starts_with('X')),
        Err(_) => false,
    }
}

/// The NUL-separated command line of a process, as words.
fn cmdline(pid: u32) -> Vec<String> {
    std::fs::read(format!("/proc/{pid}/cmdline"))
        .map(|bytes| {
            bytes
                .split(|b| *b == 0)
                .filter(|w| !w.is_empty())
                .map(|w| String::from_utf8_lossy(w).into_owned())
                .collect()
        })
        .unwrap_or_default()
}

fn connect_in_attach_mode_reports_session_left_connect(scratch: &Scratch) {
    let mut origin = Server::start(scratch.path(), "o");
    let other = Server::start(scratch.path(), "t");
    let origin_observer = Observer::on(&origin.address());
    let other_observer = Observer::on(&other.address());
    let mut editor = Editor::attach(options(&origin.address()));
    assert_eq!(editor.harness.take_event(), None, "an event before anything was typed");

    editor
        .harness
        .send_text_input(&format!(":connect {}<CR>", other.address()));
    editor.render_until("the harness saw the session end", |h| h.has_neovim_exited());
    let event = editor.harness.take_event();
    println!("fork_attach: after :connect the harness reported {event:?}");
    assert_eq!(event, Some(LiveEvent::SessionLeft(Left::Connect)));
    assert_eq!(
        editor.harness.take_event(),
        None,
        "the session's end was reported twice"
    );

    origin_observer.wait("the origin lost its UI", |o| o.uis() == 0);
    assert!(origin.is_running(), "the origin ended on :connect");
    // Not followed: the harness never reached the other server.
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        other_observer.uis(),
        0,
        "the harness followed :connect to the other server"
    );
    assert_eq!(
        other_observer.string("return vim.g.neovide and 'set' or nil"),
        "<nil>",
        "the harness set the other server up"
    );
    assert!(editor.harness.shutdown());
}

fn restart_in_attach_mode_reports_session_left_restart_with_the_address(scratch: &Scratch) {
    let mut origin = Server::start(scratch.path(), "o");
    let address = origin.address();
    let mut editor = Editor::attach(options(&address));

    editor.harness.send_text_input(":restart<CR>");
    editor.render_until("the harness saw the session end", |h| h.has_neovim_exited());
    let event = editor.harness.take_event();
    println!("fork_attach: after :restart the harness reported {event:?}");
    assert_eq!(
        event,
        Some(LiveEvent::SessionLeft(Left::Restart {
            addr: Some(address.clone())
        }))
    );
    assert_eq!(
        editor.harness.take_event(),
        None,
        "the session's end was reported twice"
    );

    // The origin quit as `:qa` does; nvim started the new server at the same address, and nothing attached to it.
    let deadline = Instant::now() + WAIT;
    while origin.is_running() {
        assert!(Instant::now() < deadline, "the origin did not quit on :restart");
        std::thread::sleep(Duration::from_millis(20));
    }
    let restarted = Observer::when_listening(&address);
    let pid = u32::try_from(restarted.int("return vim.fn.getpid()")).expect("a pid");
    let argv = cmdline(pid);
    println!("fork_attach: the restarted server is pid {pid}: {argv:?}");
    assert!(
        argv.iter().any(|a| a == "--embed") && argv.iter().any(|a| *a == address),
        "the server at the address is not the one :restart started: {argv:?}"
    );
    assert_eq!(restarted.uis(), 0, "the harness followed :restart");
    drop(restarted);
    // The restarted server waits for a UI nobody brings; it is this test's leftover, ended by the pid it answered.
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGTERM);
    }
    let deadline = Instant::now() + WAIT;
    while pid_alive(pid) {
        assert!(Instant::now() < deadline, "the restarted server {pid} did not end");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(editor.harness.shutdown());
}

fn restart_refused_by_an_unsaved_buffer_keeps_the_session(scratch: &Scratch) {
    let mut origin = Server::start(scratch.path(), "o");
    let observer = Observer::on(&origin.address());
    // Wide enough that nvim's refusal fits on one line rather than waiting at a hit-enter prompt.
    let mut editor = Editor::attach(LiveHarnessOptions {
        grid_size: Some(GridSize::new(120u32, 8u32)),
        ..options(&origin.address())
    });
    editor.harness.send_text_input("ikept<Esc>");
    observer.wait("the text reached the server", |o| {
        o.string("return vim.fn.getline(1)") == "kept"
    });

    editor.harness.send_text_input(":restart<CR>");
    observer.wait("nvim refused the restart", |o| {
        o.string("return vim.v.exitreason") == "" && o.string("return vim.v.errmsg").contains("E37")
    });
    // Long enough for the harness's check of the refusal to have come back.
    let until = Instant::now() + Duration::from_millis(1500);
    while Instant::now() < until {
        editor.frame();
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        !editor.harness.has_neovim_exited(),
        "the session ended on a refused restart"
    );
    assert_eq!(editor.harness.take_event(), None, "a refused restart was reported");
    assert!(origin.is_running(), "the origin ended on a refused restart");
    let new_servers: Vec<u32> = processes_with(&origin.address())
        .into_iter()
        .filter(|pid| cmdline(*pid).iter().any(|a| a == "--embed"))
        .collect();
    assert!(
        new_servers.is_empty(),
        "a refused restart left a new server: {new_servers:?}"
    );

    editor.harness.send_text_input("aagain<Esc>");
    observer.wait("the session still takes keys", |o| {
        o.string("return vim.fn.getline(1)") == "keptagain"
    });
    // The restart was dropped, so the session's own end is no restart.
    assert!(editor.harness.shutdown());
    assert_eq!(
        editor.harness.take_event(),
        None,
        "the dropped restart was reported at the end"
    );
    observer.wait("the origin lost its UI", |o| o.uis() == 0);
    assert!(origin.is_running(), "the origin ended with the harness");
    assert_eq!(
        observer.string("return vim.bo.modified and 'modified' or 'saved'"),
        "modified"
    );
}

/// In 40 columns nvim's refusal of `:restart` waits at a hit-enter prompt, where the harness's check of the
/// refusal gets no answer, so the restart is still announced when the host hangs up. That end is the host's own,
/// not a restart, and the person's nvim keeps its unsaved buffer.
fn a_hang_up_while_a_refused_restart_waits_at_a_prompt_reports_nothing(scratch: &Scratch) {
    let mut origin = Server::start(scratch.path(), "o");
    let observer = Observer::on(&origin.address());
    let mut editor = Editor::attach(options(&origin.address()));
    editor.harness.send_text_input("ikept<Esc>");
    observer.wait("the text reached the server", |o| {
        o.string("return vim.fn.getline(1)") == "kept"
    });

    editor.harness.send_text_input(":restart<CR>");
    let until = Instant::now() + Duration::from_millis(1500);
    while Instant::now() < until {
        editor.frame();
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        !editor.harness.has_neovim_exited(),
        "the session ended on a refused restart"
    );
    assert!(
        editor.harness.shutdown(),
        "the harness did not see its own hang-up end the session"
    );
    assert_eq!(
        editor.harness.take_event(),
        None,
        "the host's own hang-up was reported as nvim's"
    );

    observer.input("<CR>");
    observer.wait("the origin lost its UI", |o| o.uis() == 0);
    assert!(origin.is_running(), "the origin ended");
    assert_eq!(observer.string("return vim.fn.getline(1)"), "kept");
    assert_eq!(
        observer.string("return vim.bo.modified and 'modified' or 'saved'"),
        "modified"
    );
}

/// What an attach must leave as it found it once its cleanup ran and it hung up: variables, options, maps,
/// autocommands and their groups, user commands, functions, highlights, Lua globals and modules, live uv handles.
const SNAPSHOT_LUA: &str = r#"
local function plain(x)
  if type(x) == "function" then return tostring(x) end
  if type(x) ~= "table" then return x end
  local out = {}
  for k, v in pairs(x) do out[k] = plain(v) end
  return out
end
local function keys(t) local out = {} for k in pairs(t) do out[#out + 1] = k end table.sort(out) return out end
local options = {}
for name in pairs(vim.api.nvim_get_all_options_info()) do
  for scope, o in pairs({ global = { scope = "global" }, window = { win = 0 }, buffer = { buf = 0 } }) do
    local ok, value = pcall(vim.api.nvim_get_option_value, name, o)
    if ok then options[scope .. ":" .. name] = value end
  end
end
local maps = {}
for _, mode in ipairs({ "n", "v", "x", "s", "o", "i", "l", "c", "t" }) do maps[mode] = vim.api.nvim_get_keymap(mode) end
local groups = {}
for _, group in ipairs(vim.fn.getcompletion("", "augroup")) do
  if pcall(vim.api.nvim_get_autocmds, { group = group }) then groups[#groups + 1] = group end
end
local handles = 0
vim.uv.walk(function(h) if not h:is_closing() then handles = handles + 1 end end)
-- nvim's own UILeave handler (runtime plugin/osc52.lua) writes g:termfeatures whenever a UI leaves and no TUI is
-- left, whichever UI it was.
local globals = vim.fn.eval("g:")
globals.termfeatures = nil
return vim.inspect(plain({
  globals = globals, options = options, maps = maps, autocmds = vim.api.nvim_get_autocmds({}),
  augroups = groups, commands = vim.api.nvim_get_commands({ builtin = false }),
  functions = vim.fn.execute("function"), highlights = vim.api.nvim_get_hl(0, {}), global_keys = keys(_G),
  modules = keys(package.loaded), handles = handles,
}))
"#;

/// The snapshot, read byte for byte (option values may hold bytes that are not UTF-8).
fn snapshot(observer: &Observer) -> String {
    match observer.lua(SNAPSHOT_LUA) {
        Value::String(s) => String::from_utf8_lossy(s.as_bytes()).into_owned(),
        other => panic!("the snapshot is not a string: {other:?}"),
    }
}

fn attach_then_cleanup_leaves_the_nvim_as_it_was(scratch: &Scratch) {
    let mut server = Server::start(scratch.path(), "s");
    let observer = Observer::on(&server.address());
    observer.lua("vim.o.lazyredraw = true vim.o.termguicolors = false vim.g.neovide_scale_factor = 1.5");
    let before = snapshot(&observer);
    assert!(before.len() > 1000, "the snapshot is empty: {before:?}");
    // nvim keeps the last UI's size once none is left, so the editor attaches at the size a headless nvim has.
    let mut editor = Editor::attach(LiveHarnessOptions {
        grid_size: Some(GridSize::new(80u32, 24u32)),
        ..options(&server.address())
    });
    assert_eq!(
        observer.string("return tostring(vim.g.neovide)"),
        "true",
        "the attach set nothing"
    );
    assert_ne!(snapshot(&observer), before, "the snapshot does not see the attach");

    let cleaned = observer.lua("local r = rawget(_G, '__neovide_attach') return r.cleanup(r.chan)");
    assert_eq!(
        cleaned,
        Value::Boolean(true),
        "the cleanup for the editor's channel did not act"
    );
    assert!(editor.harness.shutdown(), "the hang-up was not seen");
    observer.wait("only the observer is left on the server", |o| o.socket_channels() == 1);
    let after = snapshot(&observer);
    if let Some((was, is)) = before.lines().zip(after.lines()).find(|(a, b)| a != b) {
        panic!("the attach left a change: was {was:?}, is {is:?}");
    }
    assert_eq!(
        before.lines().count(),
        after.lines().count(),
        "the attach left a change at the end"
    );
    assert!(server.is_running(), "the server ended");
}

/// The editor's channel can end with nobody running the cleanup first (a hang-up as here; a `:connect` typed in the
/// editor or a lost connection end it the same way): what the attach put in goes with the channel all the same.
fn a_channel_that_ends_without_the_cleanup_leaves_the_nvim_as_it_was(scratch: &Scratch) {
    let mut server = Server::start(scratch.path(), "s");
    let observer = Observer::on(&server.address());
    observer.lua("vim.o.lazyredraw = true vim.o.termguicolors = false vim.g.neovide_scale_factor = 1.5");
    let before = snapshot(&observer);
    let mut editor = Editor::attach(LiveHarnessOptions {
        grid_size: Some(GridSize::new(80u32, 24u32)),
        ..options(&server.address())
    });
    assert_eq!(
        observer.string("return tostring(vim.g.neovide)"),
        "true",
        "the attach set nothing"
    );
    assert!(editor.harness.shutdown(), "the hang-up was not seen");
    observer.wait("only the observer is left on the server", |o| o.socket_channels() == 1);
    observer.wait("the attach's record goes with its channel", |o| {
        o.lua("return rawget(_G, '__neovide_attach') == nil") == Value::Boolean(true)
    });
    let after = snapshot(&observer);
    if let Some((was, is)) = before.lines().zip(after.lines()).find(|(a, b)| a != b) {
        panic!("the attach left a change: was {was:?}, is {is:?}");
    }
    assert_eq!(
        before.lines().count(),
        after.lines().count(),
        "the attach left a change at the end"
    );
    assert!(server.is_running(), "the server ended");
}

fn run_case(case: &str) {
    let scratch = Scratch::new(case);
    match case {
        "attach_reaches_a_listening_server_as_its_only_ui" => {
            attach_reaches_a_listening_server_as_its_only_ui(&scratch)
        }
        "attach_refuses_a_socket_of_another_uid" => attach_refuses_a_socket_of_another_uid(&scratch),
        "attach_setup_past_four_seconds_fails_the_launch" => attach_setup_past_four_seconds_fails_the_launch(&scratch),
        "shutdown_returns_at_eof" => shutdown_returns_at_eof(&scratch),
        "no_remote_clipboard_leaves_g_clipboard_alone" => no_remote_clipboard_leaves_g_clipboard_alone(&scratch),
        "the_editor_attaches_through_options_and_hangs_up" => {
            the_editor_attaches_through_options_and_hangs_up(&scratch)
        }
        "connect_in_attach_mode_reports_session_left_connect" => {
            connect_in_attach_mode_reports_session_left_connect(&scratch)
        }
        "restart_in_attach_mode_reports_session_left_restart_with_the_address" => {
            restart_in_attach_mode_reports_session_left_restart_with_the_address(&scratch)
        }
        "restart_refused_by_an_unsaved_buffer_keeps_the_session" => {
            restart_refused_by_an_unsaved_buffer_keeps_the_session(&scratch)
        }
        "a_hang_up_while_a_refused_restart_waits_at_a_prompt_reports_nothing" => {
            a_hang_up_while_a_refused_restart_waits_at_a_prompt_reports_nothing(&scratch)
        }
        "attach_then_cleanup_leaves_the_nvim_as_it_was" => attach_then_cleanup_leaves_the_nvim_as_it_was(&scratch),
        "a_channel_that_ends_without_the_cleanup_leaves_the_nvim_as_it_was" => {
            a_channel_that_ends_without_the_cleanup_leaves_the_nvim_as_it_was(&scratch)
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
        let _server = own_x_server::isolate("fork_attach", "640x480x24");
        run_case(&case);
        println!("fork_attach: ok {case}");
        return;
    }
    let args: Vec<String> = std::env::args().collect();
    if !args.iter().any(|a| a == "--ignored" || a == "--include-ignored") {
        println!("fork_attach: ignored (spawns real nvims); run with `-- --ignored`");
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
        print!("{stdout}");
        if stdout.lines().any(|l| l == format!("fork_attach: ok {case}")) {
            println!("test {case} ... ok");
        } else {
            println!("test {case} ... FAILED ({})", out.status);
            eprintln!("--- {case} stderr ---\n{}", String::from_utf8_lossy(&out.stderr));
            failed.push(*case);
        }
    }
    if !failed.is_empty() {
        panic!("fork_attach: failed cases: {failed:?}");
    }
}
