//! `eitri open --nvim <addr>` over a real nvim whose terminal UI runs in a private tmux: the window shows that nvim,
//! the terminal shows the waiting room, and every way the window ends puts the same nvim back in the terminal.
//!
//! A plain `main` (`harness = false`): `own_display_scan` requires a test that starts the product binary to isolate its
//! display in `main`. Opt-in with `-- --ignored`: it needs nvim 0.12 or later and tmux on `PATH`, `Xvfb`, `xdotool`
//! and WebKitGTK. Each case runs its own tmux, nvim and window under a home of its own, with a stand-in `claude` first
//! on `PATH` and in `VERDANDI_CLAUDE_CLI_PATH`, and ends them by the pids it captured. Keys reach the window through
//! `xdotool` aimed at this test's own X server, never another display.

use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use eitri_core::nvim_rpc::NvimLink;
use rmpv::Value;

#[path = "support/own_x_server.rs"]
mod own_x_server;

#[path = "../../core/tests/support/nvim_snapshot.rs"]
mod nvim_snapshot;

/// How long one step may take on a loaded machine before the case calls it hung. Not a speed bound: every wait ends as
/// soon as what it waits for is there.
const HANG: Duration = Duration::from_secs(90);
/// How long the terminal may take to show nvim again once the window let go of it. The window's own bounds are 300 ms
/// for the teardown and 1 s for the room; the rest is for a loaded machine.
const BACK_WITHIN: Duration = Duration::from_secs(6);
/// The first line of the waiting room's text.
const ROOM_TEXT: &str = "This nvim is open in Eitri.";
const PROMPT: &str = "takeover-shell> ";

/// The person's nvim: an unsaved edit, a running job, and a record of any signal it is sent.
const PERSON_LUA: &str = r#"
for _, name in ipairs({ 'sigterm', 'sighup', 'sigint', 'sigusr1', 'sigusr2', 'sigquit' }) do
  local s = vim.uv.new_signal()
  s:start(name, function() vim.schedule(function() vim.g.eitri_test_signalled = name end) end)
end
vim.api.nvim_buf_set_lines(0, 0, -1, false, { 'unsaved line' })
vim.g.eitri_test_job = vim.fn.jobstart({ 'cat' })
"#;

struct Case {
    name: String,
    scratch: PathBuf,
    /// Short, under the temporary directory, as a socket path must be: tmux's, nvim's and the window's sockets.
    runtime: PathBuf,
    project: PathBuf,
    addr: PathBuf,
    tmux_socket: PathBuf,
    server: u32,
    server_start: Option<u64>,
    tui: u32,
    tui_start: Option<u64>,
    link: Option<NvimLink>,
    shell: Option<Child>,
    /// A server `:restart` left running, ended by the case.
    restarted: Option<PathBuf>,
}

fn field<'a>(value: &'a Value, name: &str) -> Option<&'a Value> {
    value
        .as_map()?
        .iter()
        .find(|(k, _)| k.as_str() == Some(name))
        .map(|(_, v)| v)
}

fn wait(what: &str, within: Duration, mut done: impl FnMut() -> bool) -> Result<Duration, String> {
    let started = Instant::now();
    while started.elapsed() < within {
        if done() {
            return Ok(started.elapsed());
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    Err(format!("{what}: not within {within:?}"))
}

impl Case {
    fn new(name: &str) -> Case {
        Case::with_cwd(name, None)
    }

    /// A scratch home, a private tmux and, in its one pane, a shell that runs the person's nvim at `cwd` (the case's
    /// project directory when `None`).
    fn with_cwd(name: &str, cwd: Option<&str>) -> Case {
        let scratch =
            Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("takeover-start-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&scratch);
        let project = scratch.join(format!("tkproj-{name}-{}", std::process::id()));
        for dir in ["home", "config/eitri", "state", "cache", "data", "bin"] {
            std::fs::create_dir_all(scratch.join(dir)).unwrap();
        }
        std::fs::create_dir_all(&project).unwrap();
        let runtime = std::env::temp_dir().join(format!("tks-{}-{}", std::process::id(), &name[..name.len().min(8)]));
        let _ = std::fs::remove_dir_all(&runtime);
        std::fs::create_dir(&runtime).unwrap();
        std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700)).unwrap();
        // Anything that could start the agent's sidecar finds this first, and it answers nothing.
        let claude = scratch.join("bin/claude");
        std::fs::write(&claude, "#!/bin/sh\nexit 1\n").unwrap();
        std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(scratch.join("person.lua"), PERSON_LUA).unwrap();
        let tmux_socket = runtime.join("tm.sock");
        let addr = runtime.join("n.sock");
        let cwd = match cwd {
            Some("home") => scratch.join("home"),
            _ => project.clone(),
        };
        let quote = |p: &Path| format!("'{}'", p.display().to_string().replace('\'', "'\\''"));
        let available = Command::new("tmux").arg("-V").output();
        assert!(available.is_ok_and(|o| o.status.success()), "NOT RUN: tmux is required");
        let mut case = Case {
            name: name.to_owned(),
            scratch,
            runtime,
            project,
            addr,
            tmux_socket,
            server: 0,
            server_start: None,
            tui: 0,
            tui_start: None,
            link: None,
            shell: None,
            restarted: None,
        };
        case.tmux(&[
            "new-session",
            "-d",
            "-s",
            "tk",
            "-x",
            "150",
            "-y",
            "32",
            "/bin/sh",
            "-i",
        ]);
        case.tmux(&["set-option", "-g", "remain-on-exit", "on"]);
        let line = format!(
            "cd {}; PS1='{PROMPT}'; nvim --clean -n -i NONE --listen {} --cmd {}",
            quote(&cwd),
            quote(&case.addr),
            quote(Path::new(&format!(
                "luafile {}",
                case.scratch.join("person.lua").display()
            ))),
        );
        case.tmux(&["send-keys", "-t", "tk:0.0", "-l", &line]);
        case.tmux(&["send-keys", "-t", "tk:0.0", "Enter"]);
        wait("nvim listens", HANG, || case.addr.exists()).unwrap();
        case.link = Some(NvimLink::connect(&case.addr, Duration::from_secs(5)).unwrap().0);
        wait("the terminal UI attaches", HANG, || case.uis().len() == 1).unwrap();
        case.server = case.lua("return vim.fn.getpid()").as_u64().unwrap() as u32;
        let chan = field(&case.uis()[0], "chan").and_then(Value::as_u64).unwrap();
        let info = case.lua_with("return vim.api.nvim_get_chan_info(...)", vec![chan.into()]);
        case.tui = field(&info, "client")
            .and_then(|c| field(c, "attributes"))
            .and_then(|a| field(a, "pid"))
            .and_then(|p| p.as_u64().or_else(|| p.as_str().and_then(|s| s.parse().ok())))
            .expect("the terminal UI reports its pid") as u32;
        case.server_start = eitri_core::split::proc_start_time(case.server);
        case.tui_start = eitri_core::split::proc_start_time(case.tui);
        case
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
            .env("HOME", self.scratch.join("home"))
            .env("XDG_CONFIG_HOME", self.scratch.join("config"))
            .env("XDG_DATA_HOME", self.scratch.join("data"))
            .env("XDG_STATE_HOME", self.scratch.join("state"))
            .env("XDG_RUNTIME_DIR", &self.runtime)
            .env("SHELL", "/bin/sh")
            .env("TERM", "xterm-256color")
            .output()
            .unwrap();
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

    fn keys_to_terminal(&self, keys: &str) {
        self.tmux(&["send-keys", "-t", "tk:0.0", keys]);
    }

    fn lua(&self, code: &str) -> Value {
        self.lua_with(code, vec![])
    }

    fn lua_with(&self, code: &str, args: Vec<Value>) -> Value {
        self.link()
            .exec_lua(code, args)
            .wait(Duration::from_secs(10))
            .expect("nvim answered")
            .unwrap_or_else(|e| panic!("{code}: {e}"))
    }

    /// Lua in the person's nvim, or `None` when it does not answer within a second.
    fn try_lua(&self, code: &str) -> Option<Value> {
        self.link()
            .exec_lua(code, vec![])
            .wait(Duration::from_secs(1))
            .and_then(Result::ok)
    }

    fn link(&self) -> &NvimLink {
        self.link.as_ref().expect("connected")
    }

    fn uis(&self) -> Vec<Value> {
        self.lua("return vim.api.nvim_list_uis()")
            .as_array()
            .cloned()
            .unwrap_or_default()
    }

    /// The channel the window's editor holds in nvim, as the fork records it.
    fn fork_chan(&self) -> Option<u64> {
        self.lua("local a = rawget(_G, '__neovide_attach'); return type(a) == 'table' and a.chan or nil")
            .as_u64()
    }

    /// What Eitri may leave in nvim, compared before and after. A terminal UI attaching makes nvim ask the terminal
    /// about itself (`vim.tty`, the `nvim.tty` group), with `TermResponse` autocommands and timers of its own that
    /// come and go with the terminal's answers: those are the terminal's, never Eitri's, and are left out.
    fn snapshot(&self) -> nvim_snapshot::Snapshot {
        nvim_snapshot::snapshot(&|code| without_tty_responses(self.lua(code)))
    }

    /// The snapshot once the terminal's own queries have settled: two in a row, longer apart than the queries' own
    /// one-second timers, the same.
    fn settled_snapshot(&self) -> nvim_snapshot::Snapshot {
        let until = Instant::now() + Duration::from_secs(15);
        let mut last = self.snapshot();
        loop {
            std::thread::sleep(Duration::from_millis(1500));
            let now = self.snapshot();
            if now == last || Instant::now() > until {
                return now;
            }
            last = now;
        }
    }

    fn server_alive(&self) -> bool {
        self.server_start.is_some() && eitri_core::split::proc_start_time(self.server) == self.server_start
    }

    /// `eitri open --nvim <addr> [extra]`, its output in files of the case's own.
    fn open(&mut self, extra: &[&Path]) {
        let mut command = Command::new(env!("CARGO_BIN_EXE_shell"));
        command.args(["open", "--nvim"]).arg(&self.addr);
        if !extra.is_empty() {
            command.arg("--");
            command.args(extra);
        }
        let path = format!(
            "{}:{}",
            self.scratch.join("bin").display(),
            std::env::var("PATH").unwrap_or_default()
        );
        command
            .current_dir(&self.project)
            .env("HOME", self.scratch.join("home"))
            .env("XDG_CONFIG_HOME", self.scratch.join("config"))
            .env("XDG_DATA_HOME", self.scratch.join("data"))
            .env("XDG_STATE_HOME", self.scratch.join("state"))
            .env("XDG_CACHE_HOME", self.scratch.join("cache"))
            .env("XDG_RUNTIME_DIR", &self.runtime)
            .env("PATH", path)
            .env("VERDANDI_CLAUDE_CLI_PATH", self.scratch.join("bin/claude"))
            // The first-start screen would hold the keys this test types into the editor.
            .env("EITRI_FIRST_START", "off")
            .env_remove("NVIM")
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .env_remove("EITRI_CONFIG_DIR");
        let out = std::fs::File::create(self.scratch.join("shell.out")).unwrap();
        let err = std::fs::File::create(self.scratch.join("shell.err")).unwrap();
        self.shell = Some(
            command
                .stdin(Stdio::null())
                .stdout(out)
                .stderr(err)
                .spawn()
                .expect("the shell starts"),
        );
    }

    fn out(&self) -> String {
        let mut text = String::new();
        for name in ["shell.out", "shell.err"] {
            if let Ok(mut file) = std::fs::File::open(self.scratch.join(name)) {
                let _ = file.read_to_string(&mut text);
            }
        }
        text
    }

    fn wait_for_line(&self, needle: &str) -> Result<(), String> {
        wait(needle, HANG, || self.out().contains(needle))
            .map(drop)
            .map_err(|why| format!("{why}; the window's output:\n{}", self.out()))
    }

    /// The window is up with the person's nvim in it, and the terminal shows the room.
    fn open_and_wait(&mut self) -> Result<(), String> {
        self.open(&[]);
        self.wait_for_line("[takeover] attached")?;
        let chan = self.fork_chan().ok_or("the fork recorded no channel")?;
        let uis = self.uis();
        if uis.len() != 1 || field(&uis[0], "chan").and_then(Value::as_u64) != Some(chan) {
            return Err(format!("nvim's UIs are not the window's alone: {uis:?}"));
        }
        wait("the room's text in the terminal", HANG, || {
            self.capture().contains(ROOM_TEXT)
        })
        .map_err(|why| format!("{why}:\n{}", self.capture()))?;
        // The window takes its own closes once it is up, with the panel painted.
        self.wait_for_line("[agent_panel] view 1: shown after")
    }

    /// Runs `xdotool` on this case's window, on this test's own display (`%1` in `then` names it).
    fn xdo(&self, then: &[&str]) -> Result<(), String> {
        let display = std::env::var("DISPLAY").map_err(|_| "no DISPLAY: the case runs on its own server")?;
        let out = Command::new("timeout")
            // By the window's process: Xlib reads the title's dash only in a locale of its own database.
            .args(["20", "xdotool", "search", "--sync", "--onlyvisible", "--pid"])
            .arg(
                self.shell
                    .as_ref()
                    .ok_or("the window was not started")?
                    .id()
                    .to_string(),
            )
            .args(["--name", "."])
            .args(then)
            .env("DISPLAY", display)
            // The title has a non-Latin-1 dash: only a UTF-8 locale reads it.
            .env("LC_ALL", "C.UTF-8")
            .output()
            .map_err(|e| format!("running timeout/xdotool: {e}"))?;
        match out.status.code() {
            Some(0) => Ok(()),
            Some(127) => Err("NOT RUN: xdotool is not installed".into()),
            _ => {
                let run = |args: &[&str]| {
                    Command::new("timeout")
                        .arg("5")
                        .arg("xdotool")
                        .args(args)
                        .env("DISPLAY", std::env::var("DISPLAY").unwrap_or_default())
                        .env("LC_ALL", "C.UTF-8")
                        .output()
                        .map(|o| {
                            format!(
                                "[{}] {} {}",
                                o.status,
                                String::from_utf8_lossy(&o.stdout).trim(),
                                String::from_utf8_lossy(&o.stderr).trim()
                            )
                        })
                        .unwrap_or_default()
                };
                let pattern = format!(
                    "^{} ",
                    regex_quote(&self.project.file_name().unwrap().to_string_lossy())
                );
                let names = format!(
                    "{} / search: {} / visible: {} / focus: {}",
                    run(&["search", "--name", ".", "getwindowname", "%@"]),
                    run(&["search", "--name", &pattern]),
                    run(&["search", "--onlyvisible", "--name", &pattern]),
                    run(&["getwindowfocus"])
                );
                Err(format!(
                    "xdotool {then:?} failed ({}): {}; the windows: {names}",
                    out.status,
                    String::from_utf8_lossy(&out.stderr).trim()
                ))
            }
        }
    }

    /// The window takes the keyboard (with no window manager, X's own input focus), then `then` runs.
    fn focused(&self, then: &[&str]) -> Result<(), String> {
        self.xdo(&["windowfocus", "%1"])?;
        std::thread::sleep(Duration::from_millis(300));
        let display = std::env::var("DISPLAY").map_err(|_| "no DISPLAY: the case runs on its own server")?;
        let out = Command::new("timeout")
            .arg("20")
            .arg("xdotool")
            .args(then)
            .env("DISPLAY", display)
            // The title has a non-Latin-1 dash: only a UTF-8 locale reads it.
            .env("LC_ALL", "C.UTF-8")
            .output()
            .map_err(|e| format!("running timeout/xdotool: {e}"))?;
        if out.status.success() {
            Ok(())
        } else {
            Err(format!(
                "xdotool {then:?} failed ({}): {}",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            ))
        }
    }

    fn keys_to_window(&self, keys: &[&str]) -> Result<(), String> {
        let mut args = vec!["key", "--delay", "60"];
        args.extend_from_slice(keys);
        self.focused(&args)
    }

    /// Types `text`; a line break in it is the Return key, which xdotool's `type` does not send.
    fn type_in_window(&self, text: &str) -> Result<(), String> {
        let mut lines = text.split('\n').peekable();
        while let Some(line) = lines.next() {
            if !line.is_empty() {
                self.focused(&["type", "--delay", "40", line])?;
            }
            if lines.peek().is_some() {
                self.keys_to_window(&["Return"])?;
            }
        }
        Ok(())
    }

    /// What a compositor's close button sends: `WM_DELETE_WINDOW` to the window, on this test's own display. With no
    /// window manager on that display, xdotool's own close has nobody to ask, so the message is sent here.
    fn compositor_close(&self) -> Result<(), String> {
        let display = std::env::var("DISPLAY").map_err(|_| "no DISPLAY: the case runs on its own server")?;
        let pid = self
            .shell
            .as_ref()
            .ok_or("the window was not started")?
            .id()
            .to_string();
        let found = Command::new("timeout")
            .args([
                "20",
                "xdotool",
                "search",
                "--sync",
                "--onlyvisible",
                "--pid",
                &pid,
                "--name",
                ".",
            ])
            .env("DISPLAY", &display)
            .output()
            .map_err(|e| format!("running xdotool: {e}"))?;
        let window = String::from_utf8_lossy(&found.stdout)
            .lines()
            .next()
            .map(str::to_owned)
            .ok_or("no window of the shell's to close")?;
        let out = Command::new("python3")
            .args(["-I", "-c", DELETE_WINDOW_PY, &window])
            .env("DISPLAY", &display)
            .output()
            .map_err(|e| format!("running python3: {e}"))?;
        if out.status.success() {
            Ok(())
        } else {
            Err(format!("WM_DELETE_WINDOW: {}", String::from_utf8_lossy(&out.stderr)))
        }
    }

    fn signal_shell(&self, sig: libc::c_int) {
        let pid = self.shell.as_ref().expect("started").id() as libc::pid_t;
        // SAFETY: `kill` has no memory-safety preconditions; the pid is this case's own child, not yet reaped.
        unsafe { libc::kill(pid, sig) };
    }

    fn wait_shell(&mut self) -> Result<Option<i32>, String> {
        let shell = self.shell.as_mut().expect("started");
        let until = Instant::now() + HANG;
        while Instant::now() < until {
            if let Some(status) = shell.try_wait().unwrap() {
                return Ok(status.code());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        Err(format!("the window was still running:\n{}", self.out()))
    }

    /// The same nvim, alive and never signalled, has its terminal UI back and the window gone, with nothing of
    /// Eitri's left in it.
    fn assert_back(&mut self, before: &nvim_snapshot::Snapshot, since: Instant) -> Result<(), String> {
        let fork = self.fork_chan();
        wait("the terminal UI back", HANG, || {
            let uis = self.uis();
            uis.len() == 1
                && field(&uis[0], "chan").and_then(Value::as_u64) != fork
                && !self.capture().contains(ROOM_TEXT)
        })
        .map_err(|why| format!("{why}; terminal:\n{}\nwindow:\n{}", self.capture(), self.out()))?;
        let took = since.elapsed();
        if took > BACK_WITHIN {
            return Err(format!("the terminal UI came back after {took:?}"));
        }
        if !self.server_alive() {
            return Err("nvim's server process is gone".into());
        }
        if self.lua("return vim.v.dying").as_u64() != Some(0) {
            return Err("nvim is dying".into());
        }
        if !self.lua("return vim.g.eitri_test_signalled").is_nil() {
            return Err(format!(
                "nvim was sent a signal: {}",
                self.lua("return vim.g.eitri_test_signalled")
            ));
        }
        let code = self.wait_shell()?;
        if code != Some(0) {
            return Err(format!("the window exited {code:?}, wanted 0:\n{}", self.out()));
        }
        self.assert_nothing_left(before);
        Ok(())
    }

    /// Never signalled, and nothing of Eitri's left in nvim.
    fn assert_untouched(&self, before: &nvim_snapshot::Snapshot) -> Result<(), String> {
        if !self.server_alive() {
            return Err("nvim's server process is gone".into());
        }
        if !self.lua("return vim.g.eitri_test_signalled").is_nil() {
            return Err(format!(
                "nvim was sent a signal: {}",
                self.lua("return vim.g.eitri_test_signalled")
            ));
        }
        self.assert_nothing_left(before);
        Ok(())
    }

    /// The snapshot is the one taken before the window, once the glue's own timer has had its time.
    fn assert_nothing_left(&self, before: &nvim_snapshot::Snapshot) {
        // The glue's own timer may still be tearing down what a hang-up left; it has 500 ms.
        let mut last = None;
        let settled = wait("nothing of Eitri's left", Duration::from_secs(5), || {
            let now = self.snapshot();
            let same = now == *before;
            last = Some(now);
            same
        });
        if settled.is_err() {
            let modules =
                self.lua("local k = vim.tbl_keys(package.loaded); table.sort(k); return table.concat(k, ' ')");
            eprintln!("the Lua modules loaded now: {modules}");
            nvim_snapshot::assert_same(before, last.as_ref().unwrap());
        }
    }
}

impl Drop for Case {
    fn drop(&mut self) {
        if let Some(shell) = self.shell.as_mut() {
            if shell.try_wait().ok().flatten().is_none() {
                let _ = shell.kill();
                let _ = shell.wait();
            }
        }
        if let Some(addr) = &self.restarted {
            if let Ok((link, _)) = NvimLink::connect(addr, Duration::from_secs(2)) {
                let _ = link.exec_lua("vim.cmd('qall!')", vec![]).wait(Duration::from_secs(2));
            }
        }
        // Both processes were started by this case's own terminal. The server can outlive its UI.
        for (pid, start) in [(self.server, self.server_start), (self.tui, self.tui_start)] {
            if pid > 0 && start.is_some() && eitri_core::split::proc_start_time(pid) == start {
                // SAFETY: the pid and its unchanged start time identify a process this case started.
                unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
            }
        }
        let _ = Command::new("tmux")
            .arg("-S")
            .arg(&self.tmux_socket)
            .arg("kill-server")
            .output();
        let _ = std::fs::remove_dir_all(&self.scratch);
        let _ = std::fs::remove_dir_all(&self.runtime);
        let _ = &self.name;
    }
}

/// Sends `WM_DELETE_WINDOW` to the window whose id is the first argument, on `$DISPLAY`, through libX11.
const DELETE_WINDOW_PY: &str = r#"
import ctypes, sys
x = ctypes.cdll.LoadLibrary("libX11.so.6")
x.XOpenDisplay.restype = ctypes.c_void_p
x.XOpenDisplay.argtypes = [ctypes.c_char_p]
x.XInternAtom.restype = ctypes.c_ulong
x.XInternAtom.argtypes = [ctypes.c_void_p, ctypes.c_char_p, ctypes.c_int]
class ClientMessage(ctypes.Structure):
    _fields_ = [("type", ctypes.c_int), ("serial", ctypes.c_ulong), ("send_event", ctypes.c_int),
                ("display", ctypes.c_void_p), ("window", ctypes.c_ulong), ("message_type", ctypes.c_ulong),
                ("format", ctypes.c_int), ("l", ctypes.c_long * 5)]
class Event(ctypes.Union):
    _fields_ = [("xclient", ClientMessage), ("pad", ctypes.c_long * 24)]
x.XSendEvent.argtypes = [ctypes.c_void_p, ctypes.c_ulong, ctypes.c_int, ctypes.c_long, ctypes.POINTER(Event)]
x.XFlush.argtypes = [ctypes.c_void_p]
d = x.XOpenDisplay(None)
if not d:
    sys.exit("no display")
window = int(sys.argv[1])
ev = Event()
ev.xclient.type = 33
ev.xclient.window = window
ev.xclient.message_type = x.XInternAtom(d, b"WM_PROTOCOLS", 0)
ev.xclient.format = 32
ev.xclient.l[0] = x.XInternAtom(d, b"WM_DELETE_WINDOW", 0)
x.XSendEvent(d, window, 0, 0, ctypes.byref(ev))
x.XFlush(d)
"#;

fn without_tty_responses(mut value: Value) -> Value {
    let Value::Map(top) = &mut value else { return value };
    for (key, autocmds) in top.iter_mut() {
        if key.as_str() != Some("autocmds") {
            continue;
        }
        let Value::Map(events) = autocmds else { continue };
        for (event, list) in events.iter_mut() {
            if event.as_str() != Some("TermResponse") {
                continue;
            }
            if let Value::Array(items) = list {
                items.clear();
            }
        }
    }
    value
}

fn regex_quote(text: &str) -> String {
    text.chars()
        .map(|c| {
            if ".^$*+?()[]{}|\\".contains(c) {
                format!("\\{c}")
            } else {
                c.to_string()
            }
        })
        .collect()
}

fn a_takeover_window_shows_the_session_and_the_tui_is_in_the_room() -> Result<(), String> {
    let mut case = Case::new("shows");
    case.open_and_wait()?;
    case.wait_for_line("[takeover] the window's editor shows")?;
    if !case.out().contains(&format!("eitri open: {}", case.addr.display())) {
        return Err(format!(
            "the window did not name the nvim it took over:\n{}",
            case.out()
        ));
    }
    // Typed in the window, it reaches the person's nvim.
    case.type_in_window(":let g:eitri_test_typed = 7\n")?;
    wait("a command typed in the window", HANG, || {
        case.try_lua("return vim.g.eitri_test_typed").and_then(|v| v.as_u64()) == Some(7)
    })
    .map_err(|why| {
        let mode = case.link().call("nvim_get_mode", vec![]).wait(Duration::from_secs(2));
        format!("{why}; nvim's mode {mode:?}; window:\n{}", case.out())
    })?;
    Ok(())
}

fn leader_ae_reaches_the_composer_on_the_first_try() -> Result<(), String> {
    let mut case = Case::new("leader");
    case.open_and_wait()?;
    // The context part was installed with a socket that already listened: its mapping exists, and the selection it
    // sends reaches the window the first time.
    wait("the Visual <leader>ae mapping", HANG, || {
        case.lua("return vim.fn.maparg('\\\\ae', 'x')")
            .as_str()
            .is_some_and(|s| !s.is_empty())
    })?;
    case.type_in_window("V")?;
    case.keys_to_window(&["backslash", "a", "e"])?;
    case.wait_for_line(", for the edit key")
}

/// One way the window ends, and what it does to start it.
type Ending = fn(&mut Case) -> Result<(), String>;

fn top_bar_close(case: &mut Case) -> Result<(), String> {
    case.compositor_close()
}

fn prefix_x_yes(case: &mut Case) -> Result<(), String> {
    case.keys_to_window(&["ctrl+b", "x"])?;
    case.wait_for_line("give nvim back to the terminal and close this window? (y/n)")?;
    case.keys_to_window(&["y"])
}

fn sigterm(case: &mut Case) -> Result<(), String> {
    case.signal_shell(libc::SIGTERM);
    Ok(())
}

fn last_module_kill(case: &mut Case) -> Result<(), String> {
    // The agent goes first; the editor is then the last module, and killing it closes the window.
    case.lua("vim.cmd('Eitri focus agent')");
    case.keys_to_window(&["ctrl+b", "x"])?;
    std::thread::sleep(Duration::from_millis(500));
    case.keys_to_window(&["y"])?;
    case.lua("vim.cmd('Eitri focus editor')");
    case.keys_to_window(&["ctrl+b", "x"])?;
    std::thread::sleep(Duration::from_millis(500));
    case.keys_to_window(&["y"])
}

fn another_ui(case: &mut Case) -> Result<(), String> {
    case.tmux(&["new-window", "-d", "-t", "tk:1"]);
    let line = format!("nvim --remote-ui --server '{}'", case.addr.display());
    case.tmux(&["send-keys", "-t", "tk:1", "-l", &line]);
    case.tmux(&["send-keys", "-t", "tk:1", "Enter"]);
    // The other UI stays: whoever attached it chose that. The terminal's TUI is back beside it.
    Ok(())
}

fn enter_in_the_room(case: &mut Case) -> Result<(), String> {
    case.keys_to_terminal("Enter");
    Ok(())
}

fn replaced(case: &mut Case) -> Result<(), String> {
    case.lua("local c = rawget(_G, '__eitri_companion'); vim.rpcnotify(c.chan, 'eitri_replaced')");
    Ok(())
}

fn link_lost(case: &mut Case) -> Result<(), String> {
    case.lua("local c = rawget(_G, '__eitri_companion'); vim.fn.chanclose(c.chan)");
    Ok(())
}

fn connect_in_eitri(case: &mut Case) -> Result<(), String> {
    let other = case.runtime.join("o.sock");
    case.restarted = Some(other.clone());
    let started = Command::new("nvim")
        .args(["--clean", "--headless", "-n", "-i", "NONE", "--listen"])
        .arg(&other)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("the other nvim: {e}"))?;
    drop(started);
    wait("the other nvim listens", HANG, || other.exists())?;
    case.type_in_window(&format!(":connect {}\n", other.display()))
}

fn each_close_puts_the_same_nvim_back() -> Result<(), String> {
    let endings: [(&str, Ending); 9] = [
        ("top bar close", top_bar_close),
        ("prefix x and y", prefix_x_yes),
        ("SIGTERM", sigterm),
        ("the last module's kill", last_module_kill),
        ("another UI attaching", another_ui),
        ("Enter in the room", enter_in_the_room),
        ("eitri_replaced", replaced),
        ("a lost link", link_lost),
        (":connect typed in Eitri", connect_in_eitri),
    ];
    let mut failures = Vec::new();
    for (i, (name, ending)) in endings.into_iter().enumerate() {
        let mut case = Case::new(&format!("close{i}"));
        let before = case.settled_snapshot();
        let result = case.open_and_wait().and_then(|()| {
            let since = Instant::now();
            ending(&mut case)?;
            if name == "another UI attaching" {
                // Two UIs stay: the terminal's, back, and the one that attached.
                let took = wait("the terminal UI back beside the other", HANG, || {
                    case.uis().len() == 2 && !case.capture().contains(ROOM_TEXT)
                })?;
                if took > BACK_WITHIN {
                    return Err(format!("the terminal UI came back after {took:?}"));
                }
                let code = case.wait_shell()?;
                if code != Some(0) {
                    return Err(format!("exit {code:?}:\n{}", case.out()));
                }
                // The other UI's own state (its channel, its terminal's answers) goes with it; then nvim is as it was.
                case.tmux(&["kill-window", "-t", "tk:1"]);
                wait("the other UI gone", HANG, || case.uis().len() == 1)?;
                return case.assert_untouched(&before);
            }
            case.assert_back(&before, since)
        });
        if let Err(why) = result {
            failures.push(format!("{name}: {why}"));
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("\n\n"))
    }
}

fn sigkill_of_the_window_puts_the_nvim_back() -> Result<(), String> {
    let mut case = Case::new("sigkill");
    let before = case.settled_snapshot();
    case.open_and_wait()?;
    let since = Instant::now();
    case.signal_shell(libc::SIGKILL);
    let fork = case.fork_chan();
    wait("the terminal UI back", HANG, || {
        let uis = case.uis();
        uis.len() == 1 && field(&uis[0], "chan").and_then(Value::as_u64) != fork && !case.capture().contains(ROOM_TEXT)
    })
    .map_err(|why| format!("{why}:\n{}", case.capture()))?;
    let took = since.elapsed();
    if took > BACK_WITHIN {
        return Err(format!("back after {took:?}"));
    }
    if !case.server_alive() || !case.lua("return vim.g.eitri_test_signalled").is_nil() {
        return Err("the server was signalled or is gone".into());
    }
    let mut last = None;
    let settled = wait("the glue's timer took everything out", Duration::from_secs(5), || {
        let now = case.snapshot();
        let same = now == before;
        last = Some(now);
        same
    });
    if settled.is_err() {
        nvim_snapshot::assert_same(&before, last.as_ref().unwrap());
    }
    Ok(())
}

fn qa_and_restart_in_eitri_end_nvim_and_the_terminal_is_at_its_prompt() -> Result<(), String> {
    for (name, command) in [("qa", ":qa\n"), ("restart", ":restart\n")] {
        let mut case = Case::new(&format!("end-{name}"));
        case.lua("for _, b in ipairs(vim.api.nvim_list_bufs()) do vim.bo[b].modified = false end");
        case.open_and_wait()?;
        case.type_in_window(command)?;
        wait("nvim's server ended", HANG, || !case.server_alive()).map_err(|w| format!("{name}: {w}"))?;
        wait("the terminal at its prompt", HANG, || {
            case.capture().lines().any(|line| line.starts_with(PROMPT.trim_end()))
                && !case.capture().contains(ROOM_TEXT)
        })
        .map_err(|why| format!("{name}: {why}:\n{}", case.capture()))?;
        if name == "restart" {
            case.wait_for_line("nvim restarted at ")?;
            let out = case.out();
            let line = out
                .lines()
                .find(|l| l.contains("nvim restarted at "))
                .unwrap()
                .to_owned();
            let addr = line
                .split("nvim restarted at ")
                .nth(1)
                .unwrap()
                .split(':')
                .next()
                .unwrap();
            case.restarted = Some(PathBuf::from(addr));
        }
        let code = case.wait_shell()?;
        if code != Some(0) {
            return Err(format!("{name}: exit {code:?}:\n{}", case.out()));
        }
    }
    Ok(())
}

fn restart_in_eitri_logs_the_new_address() -> Result<(), String> {
    let mut case = Case::new("restart-log");
    case.lua("for _, b in ipairs(vim.api.nvim_list_bufs()) do vim.bo[b].modified = false end");
    case.open_and_wait()?;
    case.type_in_window(":restart\n")?;
    case.wait_for_line("nvim restarted at ")?;
    let out = case.out();
    let line = out
        .lines()
        .find(|l| l.contains("nvim restarted at "))
        .unwrap()
        .to_owned();
    let rest = line.split("nvim restarted at ").nth(1).unwrap();
    let addr = rest.split(": ").next().unwrap().to_owned();
    case.restarted = Some(PathBuf::from(&addr));
    let wanted = format!("nvim restarted at {addr}: nvim --remote-ui --server {addr}");
    if !line.ends_with(&wanted) {
        return Err(format!("the log line is {line:?}, wanted it to end {wanted:?}"));
    }
    Ok(())
}

fn the_terminal_closed_shows_the_band_and_the_close_names_remote_ui() -> Result<(), String> {
    let mut case = Case::new("termgone");
    case.open_and_wait()?;
    // The terminal's own UI process goes: the room has no UI left.
    // SAFETY: this case's own terminal started the process, and its start time still matches.
    if eitri_core::split::proc_start_time(case.tui) == case.tui_start {
        unsafe { libc::kill(case.tui as libc::pid_t, libc::SIGKILL) };
    }
    case.wait_for_line("[takeover] band: the terminal this nvim came from is closed")?;
    case.compositor_close()?;
    let line = format!(
        "closing leaves nvim running with no window. To see it again: nvim --remote-ui --server {}. Close? (y/n)",
        case.addr.display()
    );
    case.wait_for_line(&line)?;
    case.keys_to_window(&["y"])?;
    let code = case.wait_shell()?;
    if code != Some(0) || !case.server_alive() {
        return Err(format!(
            "exit {code:?}, server alive {}:\n{}",
            case.server_alive(),
            case.out()
        ));
    }
    // The nvim keeps every buffer; nobody but the person may end it.
    if case
        .lua("return vim.api.nvim_buf_get_lines(0, 0, -1, false)[1]")
        .as_str()
        != Some("unsaved line")
    {
        return Err("the unsaved edit is gone".into());
    }
    Ok(())
}

fn a_move_that_did_not_land_shows_the_band_from_the_start() -> Result<(), String> {
    let mut case = Case::new("notlanded");
    // The terminal's UI is stopped: it never takes the move, so the room gets no UI and the window shows the band.
    // SAFETY: this case's own terminal started the process, and its start time still matches.
    if eitri_core::split::proc_start_time(case.tui) == case.tui_start {
        unsafe { libc::kill(case.tui as libc::pid_t, libc::SIGSTOP) };
    }
    case.open(&[]);
    let result = (|| {
        case.wait_for_line("[takeover] attached")?;
        case.wait_for_line("[takeover] band: the terminal this nvim came from is closed")
    })();
    // SAFETY: as above.
    if eitri_core::split::proc_start_time(case.tui) == case.tui_start {
        unsafe { libc::kill(case.tui as libc::pid_t, libc::SIGCONT) };
    }
    result
}

fn a_terminal_ui_that_reaches_the_room_late_gets_nvim_back() -> Result<(), String> {
    let mut case = Case::new("latetui");
    let before = case.settled_snapshot();
    // Stopped, the terminal's UI misses the move's bound; brought back with the shell's `fg`, in the foreground where
    // it can read its terminal again, it reaches the room late.
    // SAFETY: this case's own terminal started the process, and its start time still matches.
    if eitri_core::split::proc_start_time(case.tui) == case.tui_start {
        unsafe { libc::kill(case.tui as libc::pid_t, libc::SIGSTOP) };
    }
    case.open(&[]);
    case.wait_for_line("[takeover] band: the terminal this nvim came from is closed")?;
    case.tmux(&["send-keys", "-t", "tk:0.0", "-l", "fg"]);
    case.keys_to_terminal("Enter");
    case.wait_for_line("[takeover] band cleared: the terminal's UI is in the room")?;
    // The text's last words: the stopped terminal's own answers may be printed over its first line.
    wait("the room's text in the terminal", HANG, || {
        case.capture().contains("<Enter> takes it back now.")
    })
    .map_err(|why| format!("{why}:\n{}", case.capture()))?;
    case.wait_for_line("[agent_panel] view 1: shown after")?;
    // The way back is there again: a close gives nvim back without the no-window question, and the terminal's UI is
    // the one that comes back.
    let tui = case.tui;
    let since = Instant::now();
    case.compositor_close()?;
    case.assert_back(&before, since)?;
    if eitri_core::split::proc_start_time(tui) != case.tui_start {
        return Err("the terminal's UI process is gone".into());
    }
    Ok(())
}

/// Holds the person's nvim busy for longer than the editor's attach may take, once the editor has named itself: the
/// attach fails after the terminal's UI is in the room. A flag of the callback's own keeps the snapshot unchanged.
const STALL_THE_EDITORS_ATTACH_LUA: &str = r#"
local stalled = false
vim.api.nvim_create_autocmd('ChanInfo', { callback = function()
  local client = vim.v.event.info and vim.v.event.info.client
  if stalled or not client or client.name ~= 'neovide' then return end
  stalled = true
  vim.schedule(function()
    local started = vim.uv.hrtime()
    while vim.uv.hrtime() - started < 7e9 do end
  end)
end })
"#;

fn an_attach_that_fails_after_the_move_gives_nvim_back_and_exits_1() -> Result<(), String> {
    let mut case = Case::new("attachfail");
    case.lua(STALL_THE_EDITORS_ATTACH_LUA);
    let before = case.settled_snapshot();
    case.open(&[]);
    wait("the room's text in the terminal", HANG, || {
        case.capture().contains(ROOM_TEXT)
    })
    .map_err(|why| format!("{why}:\n{}\nwindow:\n{}", case.capture(), case.out()))?;
    wait("the terminal UI back", HANG, || {
        let uis = case.uis();
        uis.len() == 1 && !case.capture().contains(ROOM_TEXT)
    })
    .map_err(|why| format!("{why}; terminal:\n{}\nwindow:\n{}", case.capture(), case.out()))?;
    let code = case.wait_shell()?;
    if code != Some(1) {
        return Err(format!("the window exited {code:?}, wanted 1:\n{}", case.out()));
    }
    let said = case
        .out()
        .lines()
        .filter(|line| line.starts_with("eitri open: the window's editor could not attach to nvim"))
        .count();
    if said == 0 {
        return Err(format!("no line names the failed attach:\n{}", case.out()));
    }
    case.assert_untouched(&before)
}

fn the_blocking_question_then_the_blank_screen_then_the_session() -> Result<(), String> {
    let mut case = Case::new("blocking");
    case.open_and_wait()?;
    // nvim waits for the character after `f`.
    case.type_in_window("f")?;
    wait("nvim waits for a key", HANG, || {
        let mode = case
            .link()
            .call("nvim_get_mode", vec![])
            .wait(Duration::from_secs(2))
            .and_then(Result::ok);
        mode.as_ref()
            .and_then(|m| field(m, "blocking"))
            .and_then(Value::as_bool)
            == Some(true)
    })?;
    case.compositor_close()?;
    case.wait_for_line("nvim is waiting for a key (or busy): press Esc in the editor, or close anyway? (y/n)")?;
    case.keys_to_window(&["y"])?;
    // Given back while waiting: the terminal shows no room and no session until a key.
    wait("the room's text gone", HANG, || !case.capture().contains(ROOM_TEXT))?;
    case.wait_shell()?;
    if case.capture().contains("unsaved line") {
        return Err(format!("the session showed before a key:\n{}", case.capture()));
    }
    case.keys_to_terminal("Escape");
    wait("the session after Esc", HANG, || {
        case.capture().contains("unsaved line")
    })
    .map_err(|why| format!("{why}:\n{}", case.capture()))?;
    Ok(())
}

fn the_window_root_is_the_nvims_cwd_and_goes_through_trust() -> Result<(), String> {
    let mut case = Case::new("root");
    case.open_and_wait()?;
    let root = std::fs::canonicalize(&case.project).unwrap();
    case.wait_for_line(&format!("eitri: project root {}", root.display()))?;
    drop(case);

    // A directory on the command line overrides the cwd.
    let mut case = Case::new("rootdir");
    let other = case.scratch.join("other");
    std::fs::create_dir_all(&other).unwrap();
    case.open(&[&other]);
    case.wait_for_line("[takeover] attached")?;
    case.wait_for_line(&format!(
        "eitri: project root {}",
        std::fs::canonicalize(&other).unwrap().display()
    ))?;
    drop(case);

    // nvim at $HOME: the window's root is $HOME, which the agent's boundary refuses as it does `eitri ~`.
    let mut case = Case::with_cwd("roothome", Some("home"));
    case.open(&[]);
    case.wait_for_line("[takeover] attached")?;
    let home = std::fs::canonicalize(case.scratch.join("home")).unwrap();
    case.wait_for_line(&format!("eitri: project root {}", home.display()))?;
    case.wait_for_line("[agent] project root note:")
}

fn handed_back_is_never_refused_while_a_question_is_up() -> Result<(), String> {
    let mut case = Case::new("question");
    let before = case.settled_snapshot();
    case.open_and_wait()?;
    case.keys_to_window(&["ctrl+b", "x"])?;
    case.wait_for_line("give nvim back to the terminal and close this window? (y/n)")?;
    let since = Instant::now();
    case.keys_to_terminal("Enter");
    case.assert_back(&before, since)
}

fn a_shell_chunk_the_table_refuses_never_reaches_the_taken_over_nvim() -> Result<(), String> {
    let mut case = Case::new("chunk");
    case.open_and_wait()?;
    case.wait_for_line("[takeover] the window's editor shows")?;
    // The window installs `:EitriTutor` in each editor session it sees, through the chunk the table refuses: the
    // editor's check answers it at once, and the person's nvim never runs it.
    case.wait_for_line("[tutor] :EitriTutor could not be installed")?;
    if case.lua("return vim.fn.exists(':EitriTutor')").as_u64() != Some(0) {
        return Err("the tutor's install ran in the person's nvim".into());
    }
    Ok(())
}

fn the_person_s_nvim_reaches_the_window_clipboard() -> Result<(), String> {
    let mut case = Case::new("clip");
    case.open_and_wait()?;
    // No clipboard tool on this PATH: nvim's provider is tmux, which the glue pointed at the window's shim.
    let path = case.lua("return vim.env.PATH").as_str().unwrap_or_default().to_owned();
    if !path.split(':').next().is_some_and(|first| first.ends_with("/bin")) {
        return Err(format!("the shim is not first on nvim's PATH: {path}"));
    }
    // The window serves its clipboard while it has the focus.
    case.xdo(&["windowfocus", "--sync", "%1"])?;
    case.lua("vim.g.clipboard = 'tmux'; vim.fn.setreg('+', 'from the person')");
    // The window's clipboard read back through the same shim, as a paste reads it. Not `getreg('+')`: nvim's tmux
    // provider caches what it copied and answers from that cache whatever the window did with the copy.
    wait("the copy back from the window's clipboard", HANG, || {
        case.lua("return vim.fn.system({ 'tmux', 'save-buffer', '-' })")
            .as_str()
            .map(|text| text.trim_end_matches('\n'))
            == Some("from the person")
    })
    .map(drop)
    .map_err(|why| format!("{why}; window:\n{}", case.out()))
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if !args.iter().any(|a| a == "--ignored" || a == "--include-ignored") {
        println!(
            "takeover_start: ignored (a real nvim and tmux, an X server of its own, an X input tool and WebKitGTK); \
             run with `-- --ignored`"
        );
        return;
    }
    let server = own_x_server::isolate("takeover_start", "1280x800x24");
    let filter = args.iter().skip(1).find(|a| !a.starts_with('-')).cloned();
    let cases: [(&str, fn() -> Result<(), String>); 15] = [
        (
            "a_takeover_window_shows_the_session_and_the_tui_is_in_the_room",
            a_takeover_window_shows_the_session_and_the_tui_is_in_the_room,
        ),
        (
            "leader_ae_reaches_the_composer_on_the_first_try",
            leader_ae_reaches_the_composer_on_the_first_try,
        ),
        ("each_close_puts_the_same_nvim_back", each_close_puts_the_same_nvim_back),
        (
            "sigkill_of_the_window_puts_the_nvim_back",
            sigkill_of_the_window_puts_the_nvim_back,
        ),
        (
            "qa_and_restart_in_eitri_end_nvim_and_the_terminal_is_at_its_prompt",
            qa_and_restart_in_eitri_end_nvim_and_the_terminal_is_at_its_prompt,
        ),
        (
            "the_terminal_closed_shows_the_band_and_the_close_names_remote_ui",
            the_terminal_closed_shows_the_band_and_the_close_names_remote_ui,
        ),
        (
            "a_move_that_did_not_land_shows_the_band_from_the_start",
            a_move_that_did_not_land_shows_the_band_from_the_start,
        ),
        (
            "a_terminal_ui_that_reaches_the_room_late_gets_nvim_back",
            a_terminal_ui_that_reaches_the_room_late_gets_nvim_back,
        ),
        (
            "an_attach_that_fails_after_the_move_gives_nvim_back_and_exits_1",
            an_attach_that_fails_after_the_move_gives_nvim_back_and_exits_1,
        ),
        (
            "the_blocking_question_then_the_blank_screen_then_the_session",
            the_blocking_question_then_the_blank_screen_then_the_session,
        ),
        (
            "the_window_root_is_the_nvims_cwd_and_goes_through_trust",
            the_window_root_is_the_nvims_cwd_and_goes_through_trust,
        ),
        (
            "handed_back_is_never_refused_while_a_question_is_up",
            handed_back_is_never_refused_while_a_question_is_up,
        ),
        (
            "a_shell_chunk_the_table_refuses_never_reaches_the_taken_over_nvim",
            a_shell_chunk_the_table_refuses_never_reaches_the_taken_over_nvim,
        ),
        (
            "restart_in_eitri_logs_the_new_address",
            restart_in_eitri_logs_the_new_address,
        ),
        (
            "the_person_s_nvim_reaches_the_window_clipboard",
            the_person_s_nvim_reaches_the_window_clipboard,
        ),
    ];
    let mut failed = 0;
    let mut ran = 0;
    for (name, case) in cases {
        if filter.as_deref().is_some_and(|f| !name.contains(f)) {
            continue;
        }
        ran += 1;
        let result = std::panic::catch_unwind(case).unwrap_or_else(|panic| {
            Err(panic
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| panic.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_else(|| "panicked".into()))
        });
        match result {
            Ok(()) => println!("test {name} ... ok"),
            Err(why) => {
                failed += 1;
                println!("test {name} ... FAILED\n{why}");
            }
        }
    }
    println!("takeover_start: {} passed; {failed} failed", ran - failed);
    server.exit(if failed == 0 && ran > 0 { 0 } else { 1 });
}
