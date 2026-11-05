//! `:connect` typed in a window's own editor: the editor follows to the other nvim, and the window's close lets go of it
//! and leaves both nvims running, the one it moved to as it was before.
//!
//! A plain `main` (`harness = false`): `own_display_scan` requires a test that starts the product binary to isolate its
//! display in `main`. Opt-in with `-- --ignored`: it needs nvim 0.12 or later on `PATH`, `Xvfb`, `xdotool` and
//! WebKitGTK. The case runs a headless nvim and a window of its own under a home of its own, with a stand-in `claude`
//! first on `PATH` and in `VERDANDI_CLAUDE_CLI_PATH`. Keys reach the window through `xdotool` aimed at this test's own
//! X server, never another display.

use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
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

struct Case {
    scratch: PathBuf,
    /// Short, under the temporary directory, as a socket path must be: the nvims' and the window's sockets.
    runtime: PathBuf,
    project: PathBuf,
    target: Child,
    target_addr: PathBuf,
    target_link: NvimLink,
    shell: Option<Child>,
    /// The window's own nvim, once found: its pid and start time, and its address.
    own: Option<(u32, Option<u64>, PathBuf)>,
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
        let scratch =
            Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("followed-connect-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&scratch);
        let project = scratch.join(format!("fcproj-{}", std::process::id()));
        for dir in ["home", "config/eitri", "state", "cache", "data", "bin"] {
            std::fs::create_dir_all(scratch.join(dir)).unwrap();
        }
        std::fs::create_dir_all(&project).unwrap();
        let runtime = std::env::temp_dir().join(format!("fcs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&runtime);
        std::fs::create_dir(&runtime).unwrap();
        std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700)).unwrap();
        // Anything that could start the agent's sidecar finds this first, and it answers nothing.
        let claude = scratch.join("bin/claude");
        std::fs::write(&claude, "#!/bin/sh\nexit 1\n").unwrap();
        std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o755)).unwrap();
        let target_addr = runtime.join("t.sock");
        let state = scratch.join("target-state");
        let target = Command::new("nvim")
            .args(["--clean", "--headless", "-n", "-i", "NONE", "--listen"])
            .arg(&target_addr)
            .current_dir(&project)
            .env("XDG_STATE_HOME", &state)
            .env("XDG_CONFIG_HOME", &state)
            .env("XDG_DATA_HOME", &state)
            .env("XDG_CACHE_HOME", &state)
            .env("XDG_RUNTIME_DIR", &runtime)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("nvim on PATH");
        wait("the other nvim listens", HANG, || target_addr.exists()).unwrap();
        let target_link = NvimLink::connect(&target_addr, Duration::from_secs(5)).unwrap().0;
        Case {
            scratch,
            runtime,
            project,
            target,
            target_addr,
            target_link,
            shell: None,
            own: None,
        }
    }

    fn lua(&self, code: &str) -> Value {
        self.target_link
            .exec_lua(code, vec![])
            .wait(Duration::from_secs(10))
            .expect("nvim answered")
            .unwrap_or_else(|e| panic!("{code}: {e}"))
    }

    fn uis(&self) -> usize {
        self.lua("return #vim.api.nvim_list_uis()").as_u64().unwrap_or(0) as usize
    }

    fn snapshot(&self) -> nvim_snapshot::Snapshot {
        nvim_snapshot::snapshot(&|code| without_termfeatures(self.lua(code)))
    }

    /// The window over the case's project, its output in files of the case's own.
    fn start_window(&mut self) {
        let mut command = Command::new(env!("CARGO_BIN_EXE_shell"));
        command.arg(&self.project);
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

    /// The window's own nvim: the one nvim socket in the runtime directory that is not the other nvim's.
    fn find_own(&mut self) -> Result<(), String> {
        let mut found = None;
        wait("the editor's own nvim listens", HANG, || {
            found = std::fs::read_dir(&self.runtime)
                .into_iter()
                .flatten()
                .flatten()
                .map(|entry| entry.path())
                .find(|path| {
                    path != &self.target_addr
                        && path
                            .file_name()
                            .and_then(|name| name.to_str())
                            .is_some_and(|name| name.starts_with("nvim.") && name.ends_with(".0"))
                });
            found.is_some()
        })?;
        let addr = found.unwrap();
        let link = NvimLink::connect(&addr, Duration::from_secs(5))
            .map_err(|e| format!("the editor's own nvim at {}: {e}", addr.display()))?
            .0;
        let pid = link
            .exec_lua("return vim.fn.getpid()", vec![])
            .wait(Duration::from_secs(10))
            .and_then(Result::ok)
            .and_then(|v| v.as_u64())
            .ok_or("the editor's own nvim did not say its pid")? as u32;
        self.own = Some((pid, eitri_core::split::proc_start_time(pid), addr));
        Ok(())
    }

    fn own_alive(&self) -> bool {
        self.own
            .as_ref()
            .is_some_and(|(pid, start, _)| start.is_some() && eitri_core::split::proc_start_time(*pid) == *start)
    }

    /// Runs `xdotool` with `then` after giving this case's window the keyboard, on this test's own display.
    fn focused(&self, then: &[&str]) -> Result<(), String> {
        let display = std::env::var("DISPLAY").map_err(|_| "no DISPLAY: the case runs on its own server")?;
        let pid = self
            .shell
            .as_ref()
            .ok_or("the window was not started")?
            .id()
            .to_string();
        let focus = Command::new("timeout")
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
                "windowfocus",
                "%1",
            ])
            .env("DISPLAY", &display)
            .env("LC_ALL", "C.UTF-8")
            .output()
            .map_err(|e| format!("running timeout/xdotool: {e}"))?;
        match focus.status.code() {
            Some(0) => {}
            Some(127) => return Err("NOT RUN: xdotool is not installed".into()),
            _ => {
                return Err(format!(
                    "xdotool focus failed: {}",
                    String::from_utf8_lossy(&focus.stderr)
                ))
            }
        }
        std::thread::sleep(Duration::from_millis(300));
        let out = Command::new("timeout")
            .arg("20")
            .arg("xdotool")
            .args(then)
            .env("DISPLAY", display)
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

    /// Types `text`, then the Return key.
    fn line_to_window(&self, text: &str) -> Result<(), String> {
        self.focused(&["type", "--delay", "40", text])?;
        self.keys_to_window(&["Return"])
    }

    /// What a compositor's close button sends: `WM_DELETE_WINDOW` to the window, on this test's own display.
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

    fn wait_shell(&mut self) -> Result<Option<i32>, String> {
        let until = Instant::now() + HANG;
        while Instant::now() < until {
            let shell = self.shell.as_mut().expect("started");
            if let Some(status) = shell.try_wait().unwrap() {
                return Ok(status.code());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        Err(format!("the window was still running:\n{}", self.out()))
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
        // The window's own nvim runs on with no window by design; this case started it, and ends it.
        if let Some((_, _, addr)) = &self.own {
            if let Ok((link, _)) = NvimLink::connect(addr, Duration::from_secs(2)) {
                let _ = link.exec_lua("vim.cmd('qall!')", vec![]).wait(Duration::from_secs(2));
            }
        }
        let _ = self.target.kill();
        let _ = self.target.wait();
        let _ = std::fs::remove_dir_all(&self.scratch);
        let _ = std::fs::remove_dir_all(&self.runtime);
    }
}

/// nvim's own `UILeave` handler (runtime `plugin/osc52.lua`) writes `g:termfeatures` whenever a UI leaves and no
/// terminal UI is left, whichever UI it was: not something the window put there.
fn without_termfeatures(mut value: Value) -> Value {
    let Value::Map(top) = &mut value else { return value };
    for (key, part) in top.iter_mut() {
        if key.as_str() != Some("globals") {
            continue;
        }
        if let Value::Map(globals) = part {
            globals.retain(|(name, _)| name.as_str() != Some("termfeatures"));
        }
    }
    value
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

fn closing_a_followed_connect_window_leaves_both_servers_running_and_clean() -> Result<(), String> {
    let mut case = Case::new("close");
    let before = case.snapshot();
    let size = (case.lua("return vim.o.columns"), case.lua("return vim.o.lines"));
    let groups = case.lua("local g = vim.fn.getcompletion('', 'augroup'); table.sort(g); return table.concat(g, ' ')");
    println!("the other nvim's augroups before: {groups}");
    case.start_window();
    case.wait_for_line("[agent_panel] view 1: shown after")?;
    case.wait_for_line("[live] LiveHarness reports is_ready()=true")?;
    case.find_own()?;
    let own_addr = case.own.as_ref().map(|(_, _, addr)| addr.clone()).unwrap();
    case.line_to_window(&format!(":connect {}", case.target_addr.display()))?;
    wait("the editor is the other nvim's UI", HANG, || case.uis() == 1)
        .map_err(|why| format!("{why}; the window's output:\n{}", case.out()))?;
    case.wait_for_line("[editor] followed :connect")?;
    if !case.own_alive() {
        return Err("the window's own nvim ended on a :connect without !".into());
    }
    if case.lua("return rawget(_G, '__neovide_attach') ~= nil").as_bool() != Some(true) {
        return Err("the fork kept no record of what it put into the other nvim".into());
    }

    case.compositor_close()?;
    let question = format!(
        "closing leaves nvim running with no window. To see it again: nvim --remote-ui --server {}. Close? (y/n)",
        own_addr.display()
    );
    case.wait_for_line(&question)?;
    if case.uis() != 1 {
        return Err("the window let go of the other nvim before the question was answered".into());
    }
    case.keys_to_window(&["y"])?;
    let code = case.wait_shell()?;
    if code != Some(0) {
        return Err(format!("the window exited {code:?}, wanted 0:\n{}", case.out()));
    }
    wait("the other nvim has no UI", HANG, || case.uis() == 0)?;
    if !matches!(case.target.try_wait(), Ok(None)) {
        return Err("the other nvim ended".into());
    }
    if !case.own_alive() {
        return Err("the window's own nvim ended with the window".into());
    }
    // nvim keeps the last UI's size once none is left, whichever UI it was: the size it had is put back, and everything
    // else is compared.
    case.lua(&format!("vim.o.columns = {} vim.o.lines = {}", size.0, size.1));
    let mut last = None;
    let settled = wait(
        "nothing of Eitri's left in the other nvim",
        Duration::from_secs(5),
        || {
            let now = case.snapshot();
            let same = now == before;
            last = Some(now);
            same
        },
    );
    if settled.is_err() {
        let groups =
            case.lua("local g = vim.fn.getcompletion('', 'augroup'); table.sort(g); return table.concat(g, ' ')");
        eprintln!(
            "the other nvim's augroups now: {groups}\nthe window's output:\n{}",
            case.out()
        );
        nvim_snapshot::assert_same(&before, last.as_ref().unwrap());
    }
    Ok(())
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if !args.iter().any(|a| a == "--ignored" || a == "--include-ignored") {
        println!(
            "followed_connect: ignored (a real nvim, an X server of its own, an X input tool and WebKitGTK); run with \
             `-- --ignored`"
        );
        return;
    }
    let server = own_x_server::isolate("followed_connect", "1280x800x24");
    let cases: [(&str, fn() -> Result<(), String>); 1] = [(
        "closing_a_followed_connect_window_leaves_both_servers_running_and_clean",
        closing_a_followed_connect_window_leaves_both_servers_running_and_clean,
    )];
    let mut failed = 0;
    for (name, case) in cases {
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
    println!("followed_connect: {} passed; {failed} failed", cases.len() - failed);
    server.exit(if failed == 0 { 0 } else { 1 });
}
