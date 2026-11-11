//! The editor follows its own nvim's `:restart` and `:connect` (nvim 0.12) instead of ending.
//!
//! Each case starts an editor pane the way the window does, an `nvim --embed` of the pane's own, and types the command
//! into it:
//!
//! - `restart_in_an_owned_editor_follows_the_new_server_and_leaves_no_orphan`: the pane shows the server `:restart`
//!   started, `getpid()` answers that server's pid, which the pane learned, the old nvim is gone, and after the pane's
//!   shutdown no nvim this case started is left;
//! - `restart_refused_by_an_unsaved_buffer_keeps_the_session`: the same nvim, its edit, and nothing else running;
//! - `a_followed_restart_reports_a_new_session`: the session-ready callback fires again, with a new session serial;
//! - `a_followed_restart_keeps_the_shells_clipboard`: `"+y` in the new server still reaches the clipboard the host
//!   gave nvim (a provider set before the config), never the fork's own;
//! - `connect_in_an_owned_editor_follows_in_attach_mode`: the pane shows the server it moved to, as an nvim it did not
//!   start: that server's `ginit.vim` is not run, and once the pane let go of it (the fork's cleanup over its own
//!   channel, then the hang-up) it is as it was before the `:connect`; the editor's own nvim runs on, detached;
//! - `a_followed_connect_never_asks_getpid_and_refuses_end_nvim`: after the move the pane refuses to end, quit or
//!   restart, refuses every chunk until the host gives it a check, and its log has no pid question or signal;
//! - `connect_bang_stops_the_old_server`: `:connect!` follows too, and the nvim it left is gone;
//! - `connect_to_nowhere_keeps_the_session`: the pane goes back to its own nvim, which still takes keys.
//!
//! Each case runs in a child copy of this binary on its own `Xvfb`; the parent connects to nothing. Ignored unless asked
//! for, because it spawns real nvims (0.12 or later on `PATH`):
//!
//!     cargo test -p neovide-editor --features test-support --test owned_follow -- --ignored
use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gtk4::prelude::*;
use neovide::bridge::{
    session::{NeovimInstance, NeovimSession},
    NeovimWriter,
};
use neovide_editor::{ChunkCheck, NeovideEditorPane, NeovideEditorPaneOptions, PaneFollowed};
use rmpv::Value;

#[path = "../../shell/tests/support/own_x_server.rs"]
mod own_x_server;

const CASE_ENV: &str = "EITRI_OWNED_FOLLOW_CASE";
const WAIT: Duration = Duration::from_secs(15);

const CASES: &[&str] = &[
    "restart_in_an_owned_editor_follows_the_new_server_and_leaves_no_orphan",
    "restart_refused_by_an_unsaved_buffer_keeps_the_session",
    "a_followed_restart_reports_a_new_session",
    "a_followed_restart_keeps_the_shells_clipboard",
    "connect_in_an_owned_editor_follows_in_attach_mode",
    "a_followed_connect_never_asks_getpid_and_refuses_end_nvim",
    "connect_bang_stops_the_old_server",
    "connect_to_nowhere_keeps_the_session",
];

/// Lines the pane prints when it signals a process or asks nvim for its pid. After a followed `:connect` none may
/// appear: the server it moved to is not the pane's.
const FORBIDDEN_AFTER_A_CONNECT: &[&str] = &[
    "SIGTERM",
    "SIGKILL",
    "watching pid",
    "nvim is pid",
    "answered pid",
    "asking nvim for its own pid",
];

/// The line a case prints once the pane followed a `:connect`; the log after it is the attached session's.
const FOLLOWED_MARK: &str = "[case] followed the connect";

const GETPID_LUA: &str = "return vim.fn.getpid()";
const SERVERNAME_LUA: &str = "return vim.v.servername";
const LINE_LUA: &str = "return vim.fn.getline(1)";
const MODIFIED_LUA: &str = "return vim.bo.modified";
const CLIPBOARD_NAME_LUA: &str = "return type(vim.g.clipboard) == 'table' and vim.g.clipboard.name or vim.NIL";
const ONE_SHOT_LUA: &str = "return 'one shot'";

/// What a server must be left as once an editor let go of it: variables, options, maps, autocommands and their
/// groups, user commands, functions, highlights, Lua globals and modules, live uv handles.
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

/// A directory of this case's own under the target's scratch directory, removed at the end. Its name is short: the
/// sockets in it must fit in `sun_path` (108 bytes).
struct Scratch(PathBuf);

impl Scratch {
    fn new(case: &str) -> Self {
        let index = CASES.iter().position(|c| *c == case).expect("a known case");
        let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("of-{}-{index}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("rt")).expect("scratch dir");
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir.join("rt"), std::fs::Permissions::from_mode(0o700)).expect("rt mode");
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

/// A `--cmd` every nvim of this case carries in its argv, so a server `:restart` started from one (it copies
/// `v:argv`) can be found among the machine's processes.
fn marker() -> String {
    format!(
        "let g:eitri_owned_follow_{}_{} = 1",
        std::process::id(),
        std::time::UNIX_EPOCH.elapsed().unwrap().as_nanos()
    )
}

/// Pids of live processes whose command line holds `needle`.
fn processes_with(needle: &str) -> Vec<u32> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir("/proc").into_iter().flatten().flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        if !runs(pid) {
            continue;
        }
        if let Ok(cmdline) = std::fs::read(entry.path().join("cmdline")) {
            if String::from_utf8_lossy(&cmdline).contains(needle) {
                found.push(pid);
            }
        }
    }
    found
}

/// Whether `pid` runs: it exists and is not a zombie.
fn runs(pid: u32) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    let state = stat
        .rfind(')')
        .and_then(|close| stat[close + 1..].split_whitespace().next().map(str::to_owned));
    state.is_some_and(|s| s != "Z" && s != "X")
}

/// The processes carrying `marker` once they have had `wait` to exit on their own.
fn left_running(marker: &str, wait: Duration) -> Vec<u32> {
    let deadline = Instant::now() + wait;
    loop {
        let left = processes_with(marker);
        if left.is_empty() || Instant::now() >= deadline {
            return left;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Ends, on scope exit (a failed assertion included), every process that carries this case's marker: only nvims this
/// case started, or servers they started.
struct Reaper(String);

impl Drop for Reaper {
    fn drop(&mut self) {
        for pid in processes_with(&self.0) {
            // SAFETY: a plain syscall; the pid carries this case's own marker.
            unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
        }
    }
}

/// A headless `nvim --listen <dir>/<name>.sock`, standing in for a server of the person's own, with a `ginit.vim` on
/// its runtime path that marks that it ran.
struct Server {
    child: Child,
    socket: PathBuf,
}

impl Server {
    fn start(dir: &Path, name: &str, marker: &str) -> Server {
        let socket = dir.join(format!("{name}.sock"));
        assert!(
            socket.as_os_str().len() < 100,
            "socket path too long: {}",
            socket.display()
        );
        let runtime = dir.join(format!("{name}-rtp"));
        std::fs::create_dir_all(&runtime).unwrap();
        std::fs::write(runtime.join("ginit.vim"), "let g:eitri_test_ginit_ran = 1\n").unwrap();
        let state = dir.join(format!("{name}-state"));
        let child = Command::new("nvim")
            .args(["--clean", "--headless", "-n", "-i", "NONE", "--cmd", marker, "--cmd"])
            .arg(format!("set rtp^={}", runtime.display()))
            .arg("--listen")
            .arg(&socket)
            .current_dir(dir)
            .env("XDG_STATE_HOME", &state)
            .env("XDG_CONFIG_HOME", &state)
            .env("XDG_DATA_HOME", &state)
            .env("XDG_CACHE_HOME", &state)
            .env("XDG_RUNTIME_DIR", dir.join("rt"))
            .stdin(Stdio::null())
            .spawn()
            .expect("nvim must be on PATH for this test");
        let mut server = Server { child, socket };
        let deadline = Instant::now() + WAIT;
        while !server.socket.exists() {
            if let Ok(Some(status)) = server.child.try_wait() {
                panic!("the server exited before it listened ({status})");
            }
            assert!(Instant::now() < deadline, "the server never listened");
            std::thread::sleep(Duration::from_millis(20));
        }
        server
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

#[derive(Clone)]
struct Quiet;

impl nvim_rs::Handler for Quiet {
    type Writer = NeovimWriter;
}

/// A second RPC client on a server, to read what the editor did there.
struct Observer {
    runtime: tokio::runtime::Runtime,
    session: NeovimSession,
}

impl Observer {
    fn on(address: &str) -> Observer {
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
            .unwrap_or_else(|e| panic!("no server at {address}: {e:#}"));
        Observer { runtime, session }
    }

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

    fn uis(&self) -> i64 {
        self.int("return #vim.api.nvim_list_uis()")
    }

    fn snapshot(&self) -> String {
        match self.lua(SNAPSHOT_LUA) {
            Value::String(s) => String::from_utf8_lossy(s.as_bytes()).into_owned(),
            other => panic!("the snapshot is not a string: {other:?}"),
        }
    }
}

impl Drop for Observer {
    fn drop(&mut self) {
        self.session.hang_up.hang_up();
    }
}

/// Runs the default main context without blocking until `done` holds; `false` at `limit`.
fn run_until(limit: Duration, mut done: impl FnMut() -> bool) -> bool {
    let context = gtk4::glib::MainContext::default();
    let deadline = Instant::now() + limit;
    loop {
        context.iteration(false);
        if done() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn sample(pane: &NeovideEditorPane) {
    assert!(
        run_until(Duration::from_secs(60), || pane.testing_idle()),
        "pane did not become idle"
    );
    for _ in 0..100 {
        glib::MainContext::default().iteration(false);
        assert!(pane.is_running());
    }
}

/// An editor of its own in a shown window, with what its callbacks reported.
struct Editor {
    _window: gtk4::Window,
    pane: Rc<NeovideEditorPane>,
    followed: Rc<RefCell<Vec<PaneFollowed>>>,
    ready: Rc<Cell<u32>>,
    exits: Rc<Cell<u32>>,
}

impl Editor {
    /// A pane starting its own `nvim --embed` (`--clean`, this case's marker, `extra`), its state and sockets under
    /// `dir`.
    fn start(dir: &Path, marker: &str, extra: &[String]) -> Editor {
        let state = dir.join("editor-state");
        let mut extra_nvim_args = vec!["-n".to_owned(), "-i".to_owned(), "NONE".to_owned()];
        extra_nvim_args.extend(["--cmd".to_owned(), marker.to_owned()]);
        extra_nvim_args.extend(extra.iter().cloned());
        let env = |name: &str, value: &Path| (name.to_owned(), value.to_string_lossy().into_owned());
        let pane = Rc::new(NeovideEditorPane::with_options(NeovideEditorPaneOptions {
            clean: true,
            cwd: Some(dir.to_owned()),
            extra_nvim_args,
            child_env: vec![
                env("XDG_STATE_HOME", &state),
                env("XDG_CONFIG_HOME", &state),
                env("XDG_DATA_HOME", &state),
                env("XDG_CACHE_HOME", &state),
                env("XDG_RUNTIME_DIR", &dir.join("rt")),
            ],
            ..Default::default()
        }));
        let followed = Rc::new(RefCell::new(Vec::new()));
        {
            let followed = followed.clone();
            pane.on_followed(move |how| followed.borrow_mut().push(how));
        }
        let ready = Rc::new(Cell::new(0));
        {
            let ready = ready.clone();
            pane.on_session_ready(move || ready.set(ready.get() + 1));
        }
        let exits = Rc::new(Cell::new(0));
        {
            let exits = exits.clone();
            pane.on_exited_unrequested(move || exits.set(exits.get() + 1));
        }
        pane.widget().set_size_request(800, 400);
        let window = gtk4::Window::new();
        window.set_default_size(800, 400);
        window.set_child(Some(pane.widget()));
        window.present();
        assert!(
            run_until(Duration::from_secs(30), || pane.is_running()),
            "the editor's nvim should start"
        );
        assert!(!pane.is_attached(), "an editor of its own is not attached");
        Editor {
            _window: window,
            pane,
            followed,
            ready,
            exits,
        }
    }

    /// `code`'s answer from the editor's nvim, through the pane.
    fn lua(&self, code: &'static str) -> Value {
        let slot: Arc<Mutex<Option<Result<Value, String>>>> = Arc::new(Mutex::new(None));
        let theirs = slot.clone();
        assert!(
            self.pane.exec_lua(
                code,
                vec![],
                Box::new(move |answer| *theirs.lock().unwrap() = Some(answer))
            ),
            "the pane took no request: {code}"
        );
        assert!(
            run_until(WAIT, || slot.lock().unwrap().is_some()),
            "no answer from the editor's nvim: {code}"
        );
        let answer = slot.lock().unwrap().take().unwrap();
        answer.unwrap_or_else(|e| panic!("{code}: {e}"))
    }

    fn pid(&self) -> u32 {
        self.lua(GETPID_LUA).as_u64().expect("a pid") as u32
    }

    fn string(&self, code: &'static str) -> String {
        match self.lua(code) {
            Value::String(s) => s.into_str().unwrap_or_default(),
            other => panic!("not a string: {code} -> {other:?}"),
        }
    }

    /// Types `keys` and runs the main loop until `done` or 15 s.
    fn keys_until(&self, keys: &str, what: &str, done: impl FnMut() -> bool) {
        self.pane.send_keys(keys);
        assert!(run_until(WAIT, done), "timed out waiting for: {what}");
    }

    fn followed(&self) -> Vec<PaneFollowed> {
        self.followed.borrow().clone()
    }
}

fn restart_in_an_owned_editor_follows_the_new_server_and_leaves_no_orphan(scratch: &Scratch) {
    let marker = marker();
    let _reaper = Reaper(marker.clone());
    let editor = Editor::start(scratch.path(), &marker, &[]);
    let old = editor.pid();
    let old_address = editor.string(SERVERNAME_LUA);
    editor.keys_until(":restart<CR>", "the editor followed the restart", || {
        !editor.followed().is_empty() && editor.pane.is_running()
    });
    assert_eq!(editor.followed(), vec![PaneFollowed::Restart]);
    let new = editor.pid();
    assert_ne!(new, old, "the editor still shows the nvim that restarted");
    assert_ne!(editor.string(SERVERNAME_LUA), old_address, "the same server answers");
    assert!(
        run_until(WAIT, || !runs(old)),
        "the nvim that restarted (pid {old}) is still running"
    );
    assert!(
        run_until(WAIT, || editor.pane.learned_nvim().get().map(|(pid, _)| pid)
            == Some(new)),
        "the pane did not learn the new server's pid {new}: {:?}",
        editor.pane.learned_nvim().get()
    );
    assert_eq!(editor.exits.get(), 0, "the restart was reported as an exit");
    println!("[case] followed :restart from pid {old} to pid {new}");

    editor.pane.shutdown();
    let left = left_running(&marker, Duration::from_secs(8));
    assert!(
        left.is_empty(),
        "the editor's shutdown left nvims of this case running: {left:?}"
    );
}

fn wait_for_blocking_mode(
    within: Duration,
    mut query: impl FnMut(Box<dyn FnOnce(Option<neovide_editor::NvimMode>) + Send>) -> bool,
) {
    let mode = Arc::new(Mutex::new(None::<Option<neovide_editor::NvimMode>>));
    let mut pending = false;
    assert!(
        run_until(within, || {
            if pending {
                let Some(answer) = mode.lock().unwrap().take() else {
                    return false;
                };
                if answer.is_some_and(|mode| mode.blocking) {
                    return true;
                }
            }
            let reply = mode.clone();
            pending = query(Box::new(move |answer| *reply.lock().unwrap() = Some(answer)));
            assert!(pending, "the session stopped accepting mode queries");
            false
        }),
        "nvim never reached the restart refusal prompt"
    );
}

fn mode_retry_self_test() {
    let mut queries = 0;
    wait_for_blocking_mode(WAIT, |reply| {
        queries += 1;
        assert!(
            queries <= 2,
            "more than one request outstanding or no prompt recognized"
        );
        reply(Some(neovide_editor::NvimMode {
            at: Instant::now(),
            mode: "n".into(),
            blocking: queries == 2,
        }));
        true
    });
    assert_eq!(queries, 2, "the first nonblocking answer must trigger a fresh query");
    println!("test mode_retry_self_test ... ok");
}

fn restart_refused_by_an_unsaved_buffer_keeps_the_session(scratch: &Scratch) {
    let marker = marker();
    let _reaper = Reaper(marker.clone());
    let editor = Editor::start(scratch.path(), &marker, &[]);
    let old = editor.pid();
    editor.keys_until("ikept<Esc>", "the edit", || editor.string(LINE_LUA) == "kept");
    editor.pane.send_keys(":restart<CR>");
    wait_for_blocking_mode(WAIT, |reply| editor.pane.ask_mode(WAIT, reply));
    // A refusal on a short grid waits at a hit-enter prompt; Esc ends it, and nvim answers again.
    editor.pane.send_keys("<Esc>");
    assert_eq!(editor.pid(), old);
    sample(&editor.pane);
    assert!(editor.followed().is_empty(), "a refused restart was followed");
    assert!(editor.pane.is_running(), "the session ended");
    assert_eq!(editor.exits.get(), 0, "the refused restart ended the session");
    assert_eq!(editor.pid(), old, "another nvim answers");
    assert_eq!(editor.string(LINE_LUA), "kept");
    assert_eq!(
        editor.lua(MODIFIED_LUA),
        Value::Boolean(true),
        "the edit is no longer unsaved"
    );
    let running = processes_with(&marker);
    assert_eq!(running, vec![old], "a refused restart left another server running");
    editor.pane.shutdown();
}

fn a_followed_restart_reports_a_new_session(scratch: &Scratch) {
    let marker = marker();
    let _reaper = Reaper(marker.clone());
    let editor = Editor::start(scratch.path(), &marker, &[]);
    assert!(
        run_until(WAIT, || editor.ready.get() == 1),
        "the first session was not reported"
    );
    let first = editor.pane.session_serial().expect("a running session has a serial");
    editor.keys_until(":restart<CR>", "the restart followed", || {
        !editor.followed().is_empty() && editor.pane.is_running()
    });
    assert!(
        run_until(WAIT, || editor.ready.get() == 2),
        "the followed restart was not reported as a new session (ready fired {} times)",
        editor.ready.get()
    );
    let second = editor.pane.session_serial().expect("the followed session has a serial");
    assert_ne!(first, second, "the followed session kept the old serial");
    editor.pane.shutdown();
}

fn a_followed_restart_keeps_the_shells_clipboard(scratch: &Scratch) {
    let marker = marker();
    let _reaper = Reaper(marker.clone());
    let clip = scratch.path().join("clip.txt");
    let clip_path = clip.display().to_string();
    // The host's clipboard: a provider set before the config, as the window's `--cmd` does.
    let provider = format!(
        "let g:clipboard = {{'name': 'shell', \
          'copy': {{'+': ['sh', '-c', 'cat > {clip_path}'], '*': ['sh', '-c', 'cat > {clip_path}']}}, \
          'paste': {{'+': ['cat', '{clip_path}'], '*': ['cat', '{clip_path}']}}, 'cache_enabled': 0}}"
    );
    let editor = Editor::start(scratch.path(), &marker, &["--cmd".to_owned(), provider]);
    editor.keys_until(":restart<CR>", "the restart followed", || {
        !editor.followed().is_empty() && editor.pane.is_running()
    });
    assert!(
        run_until(WAIT, || editor.ready.get() == 2),
        "the followed session was not reported"
    );
    assert_eq!(
        editor.string(CLIPBOARD_NAME_LUA),
        "shell",
        "the followed server's clipboard is not the host's"
    );
    editor.keys_until(
        "ifollowed clip<Esc>\"+yy",
        "the yank reached the host's clipboard",
        || std::fs::read_to_string(&clip).is_ok_and(|text| text == "followed clip\n"),
    );
    assert_eq!(
        editor.string(CLIPBOARD_NAME_LUA),
        "shell",
        "the fork's clipboard took over"
    );
    editor.pane.shutdown();
}

fn connect_in_an_owned_editor_follows_in_attach_mode(scratch: &Scratch) {
    let marker = marker();
    let _reaper = Reaper(marker.clone());
    let mut target = Server::start(scratch.path(), "t", &marker);
    let observer = Observer::on(&target.address());
    let before = observer.snapshot();
    assert!(before.len() > 1000, "the snapshot is empty: {before:?}");
    let size = (observer.int("return vim.o.columns"), observer.int("return vim.o.lines"));
    let editor = Editor::start(scratch.path(), &marker, &[]);
    let own = editor.pid();
    let own_address = editor.string(SERVERNAME_LUA);
    editor.pane.send_keys(&format!(":connect {}<CR>", target.address()));
    assert!(
        run_until(WAIT, || !editor.followed().is_empty() && editor.pane.is_running()),
        "the editor did not follow the connect"
    );
    println!("{FOLLOWED_MARK}");
    assert_eq!(
        editor.followed(),
        vec![PaneFollowed::Connect {
            left: Some(own_address.clone())
        }]
    );
    assert!(editor.pane.is_attached(), "a followed connect is not an attach");
    assert_eq!(observer.uis(), 1, "the editor is not the target's UI");
    assert!(
        observer.lua("return vim.g.eitri_test_ginit_ran").is_nil(),
        "the target's ginit.vim ran"
    );
    assert_eq!(
        observer.lua("return rawget(_G, '__neovide_attach') ~= nil"),
        Value::Boolean(true),
        "the fork kept no record of what it put into the target"
    );
    assert!(
        runs(own),
        "the editor's own nvim (pid {own}) ended on a :connect without !"
    );
    assert_eq!(editor.pane.left_running(), Some(own_address.clone()));

    let cleaned: Arc<Mutex<Option<bool>>> = Arc::new(Mutex::new(None));
    {
        let cleaned = cleaned.clone();
        assert!(
            editor
                .pane
                .detach_followed(move |done| *cleaned.lock().unwrap() = Some(done)),
            "the pane would not let go of the server it followed"
        );
    }
    assert!(
        run_until(WAIT, || cleaned.lock().unwrap().is_some()),
        "the detach never finished"
    );
    assert_eq!(*cleaned.lock().unwrap(), Some(true), "the fork's cleanup did not act");
    assert!(
        run_until(WAIT, || observer.uis() == 0),
        "the editor is still the target's UI"
    );
    // nvim keeps the last UI's size once none is left, whichever UI it was: the size it had is put back, and everything
    // else is compared.
    observer.lua(&format!("vim.o.columns = {} vim.o.lines = {}", size.0, size.1));
    let after = observer.snapshot();
    if let Some((was, is)) = before.lines().zip(after.lines()).find(|(a, b)| a != b) {
        panic!("the followed connect left a change in the target: was {was:?}, is {is:?}");
    }
    assert_eq!(
        before.lines().count(),
        after.lines().count(),
        "the followed connect left a change"
    );
    assert!(target.is_running(), "the target ended");
    assert!(runs(own), "the editor's own nvim ended");
    editor.pane.shutdown();
}

fn a_followed_connect_never_asks_getpid_and_refuses_end_nvim(scratch: &Scratch) {
    let marker = marker();
    let _reaper = Reaper(marker.clone());
    let mut target = Server::start(scratch.path(), "t", &marker);
    let observer = Observer::on(&target.address());
    let editor = Editor::start(scratch.path(), &marker, &[]);
    editor.pane.send_keys(&format!(":connect {}<CR>", target.address()));
    assert!(
        run_until(WAIT, || !editor.followed().is_empty() && editor.pane.is_running()),
        "the editor did not follow the connect"
    );
    println!("{FOLLOWED_MARK}");
    assert!(
        !editor.pane.end_nvim(),
        "end_nvim was taken for a server the editor did not start"
    );
    assert!(
        !editor.pane.exec_lua_watched("vim.cmd('confirm qall')"),
        "a quit was taken for a server the editor did not start"
    );
    assert!(
        editor.pane.restart(NeovideEditorPaneOptions::default()).is_err(),
        "a restart was taken"
    );
    // No check yet: every chunk is refused at once.
    let slot: Arc<Mutex<Option<Result<Value, String>>>> = Arc::new(Mutex::new(None));
    {
        let theirs = slot.clone();
        assert!(editor.pane.exec_lua(
            ONE_SHOT_LUA,
            vec![],
            Box::new(move |answer| *theirs.lock().unwrap() = Some(answer))
        ));
    }
    assert!(
        matches!(&*slot.lock().unwrap(), Some(Err(_))),
        "a chunk went to the server before the host gave a check"
    );
    // The host's check lets this one through, and only this one.
    editor.pane.set_chunk_check(Some(ChunkCheck::new(|code, args| {
        if code == ONE_SHOT_LUA {
            Ok(args)
        } else {
            Err(neovide_editor::ChunkRefused {
                reason: "not a one-shot chunk".into(),
            })
        }
    })));
    assert_eq!(editor.string(ONE_SHOT_LUA), "one shot");
    sample(&editor.pane);
    assert_eq!(observer.uis(), 1, "the editor left the target");
    assert!(target.is_running(), "the target ended");
    editor.pane.shutdown();
    assert!(run_until(WAIT, || observer.uis() == 0), "the shutdown did not let go");
    assert!(
        target.is_running(),
        "the editor's shutdown ended a server it did not start"
    );
}

fn connect_bang_stops_the_old_server(scratch: &Scratch) {
    let marker = marker();
    let _reaper = Reaper(marker.clone());
    let mut target = Server::start(scratch.path(), "t", &marker);
    let observer = Observer::on(&target.address());
    let editor = Editor::start(scratch.path(), &marker, &[]);
    let own = editor.pid();
    editor.pane.send_keys(&format!(":connect! {}<CR>", target.address()));
    assert!(
        run_until(WAIT, || !editor.followed().is_empty() && editor.pane.is_running()),
        "the editor did not follow the connect!"
    );
    assert!(matches!(editor.followed()[..], [PaneFollowed::Connect { .. }]));
    assert!(editor.pane.is_attached());
    assert!(
        run_until(WAIT, || !runs(own)),
        "`:connect!` left the editor's own nvim running"
    );
    assert_eq!(editor.pane.left_running(), None, "a stopped nvim is named as running");
    assert_eq!(observer.uis(), 1);
    editor.pane.shutdown();
    assert!(target.is_running(), "the target ended");
}

fn connect_to_nowhere_keeps_the_session(scratch: &Scratch) {
    let marker = marker();
    let _reaper = Reaper(marker.clone());
    let editor = Editor::start(scratch.path(), &marker, &[]);
    let own = editor.pid();
    let nowhere = scratch.path().join("nowhere.sock");
    editor.pane.send_keys(&format!(":connect {}<CR>", nowhere.display()));
    editor.keys_until("ikept<Esc>", "the failed connect left nvim taking keys", || {
        editor.string(LINE_LUA) == "kept"
    });
    sample(&editor.pane);
    assert!(
        editor.followed().is_empty(),
        "a connect that went nowhere was reported as followed"
    );
    assert!(editor.pane.is_running(), "the session ended");
    assert!(!editor.pane.is_attached(), "the editor's own nvim is attached to");
    assert_eq!(editor.exits.get(), 0, "the failed connect was reported as an exit");
    assert_eq!(editor.pid(), own, "another nvim answers");

    editor.pane.shutdown();
    let left = left_running(&marker, Duration::from_secs(8));
    assert!(
        left.is_empty(),
        "the editor's shutdown left nvims of this case running: {left:?}"
    );
}

fn run_case(case: &str) {
    let scratch = Scratch::new(case);
    match case {
        "restart_in_an_owned_editor_follows_the_new_server_and_leaves_no_orphan" => {
            restart_in_an_owned_editor_follows_the_new_server_and_leaves_no_orphan(&scratch)
        }
        "restart_refused_by_an_unsaved_buffer_keeps_the_session" => {
            restart_refused_by_an_unsaved_buffer_keeps_the_session(&scratch)
        }
        "a_followed_restart_reports_a_new_session" => a_followed_restart_reports_a_new_session(&scratch),
        "a_followed_restart_keeps_the_shells_clipboard" => a_followed_restart_keeps_the_shells_clipboard(&scratch),
        "connect_in_an_owned_editor_follows_in_attach_mode" => {
            connect_in_an_owned_editor_follows_in_attach_mode(&scratch)
        }
        "a_followed_connect_never_asks_getpid_and_refuses_end_nvim" => {
            a_followed_connect_never_asks_getpid_and_refuses_end_nvim(&scratch)
        }
        "connect_bang_stops_the_old_server" => connect_bang_stops_the_old_server(&scratch),
        "connect_to_nowhere_keeps_the_session" => connect_to_nowhere_keeps_the_session(&scratch),
        other => panic!("unknown case {other}"),
    }
}

fn main() {
    mode_retry_self_test();
    if let Some(case) = std::env::var_os(CASE_ENV) {
        let case = case.to_string_lossy().into_owned();
        if let Ok(Some(kind)) = neovide_editor::detach_stdin_from_nvim() {
            println!("[stdin] a {kind} on stdin would reach nvim as a buffer; stdin is /dev/null now");
        }
        let _server = own_x_server::init_gtk("owned_follow", "1024x768x24");
        run_case(&case);
        println!("RESULT pass {case}");
        return;
    }
    let args: Vec<String> = std::env::args().collect();
    if !args.iter().any(|a| a == "--ignored" || a == "--include-ignored") {
        println!("owned_follow: ignored (spawns real nvims); run with `-- --ignored`");
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
        let after_connect: Vec<&str> = stdout
            .lines()
            .skip_while(|line| *line != FOLLOWED_MARK)
            .filter(|line| !line.starts_with("RESULT ") && !line.starts_with("[case] "))
            .filter(|line| FORBIDDEN_AFTER_A_CONNECT.iter().any(|f| line.contains(f)))
            .collect();
        if passed && after_connect.is_empty() {
            for line in stdout.lines().filter(|line| line.starts_with("[case] ")) {
                println!("{line}");
            }
            println!("test {case} ... ok");
        } else {
            println!("{stdout}");
            if !after_connect.is_empty() {
                println!("--- {case}: the pane signalled or asked a pid after a followed connect: {after_connect:#?}");
            }
            eprintln!("--- {case} stderr ---\n{}", String::from_utf8_lossy(&out.stderr));
            println!("test {case} ... FAILED ({})", out.status);
            failed.push(*case);
        }
    }
    assert!(failed.is_empty(), "owned_follow: failed cases: {failed:?}");
}
