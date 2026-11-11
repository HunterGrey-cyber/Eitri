//! `eitri page` beside a real nvim: the product binary's page window, its link to that nvim and its follow commands,
//! end to end, on the test's own Xvfb.
//!
//! A plain `main` (`harness = false`): `own_display_scan` requires a test that starts the product binary to isolate its
//! display in `main`. Opt-in with `-- --ignored`: it needs `nvim` (>= 0.11) on `PATH`, `Xvfb` and WebKitGTK. Each case
//! runs its own headless nvim and its own window under a home of its own, with a stand-in `claude` first on `PATH`, and
//! ends both by the pids it captured.

use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

#[path = "support/own_x_server.rs"]
mod own_x_server;

/// How long one step may take on a loaded machine before the case calls it hung. Not a speed bound: every wait ends as
/// soon as what it waits for is there.
const HANG: Duration = Duration::from_secs(90);
/// The stand-in plugin's id: it follows Markdown and defines a show and a hide command.
const ID: &str = "pwtest";

struct Case {
    scratch: PathBuf,
    /// The runtime directory, the sockets' home: short, as a socket path must be, so under the temporary directory.
    runtime: PathBuf,
    nvim: Child,
    sock: PathBuf,
    shell: Option<Child>,
}

impl Case {
    /// A scratch home with the stand-in plugin and `claude`, and a headless nvim on `doc.md` listening on its socket.
    /// `nvim_cmd` runs in that nvim before its first file.
    fn new(name: &str, nvim_cmd: Option<&str>) -> Case {
        let scratch =
            Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("page-window-start-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&scratch);
        for dir in ["home", "config/eitri", "state", "cache", "bin", "project"] {
            std::fs::create_dir_all(scratch.join(dir)).unwrap();
        }
        let runtime = std::env::temp_dir().join(format!("pwt-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&runtime);
        std::fs::create_dir(&runtime).unwrap();
        std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700)).unwrap();
        let plugin = scratch.join("data/eitri/plugins").join(ID);
        std::fs::create_dir_all(plugin.join("site")).unwrap();
        std::fs::write(
            plugin.join("eitri-plugin.toml"),
            format!(
                "id = \"{ID}\"\ntitle = \"Page test\"\nprotocol = 1\nsite = \"site\"\nentry = \"index.html\"\n\
                 capabilities = [\"editor.follow\", \"editor.commands\"]\n\n\
                 [follow]\nfiletypes = [\"markdown\"]\nauto_show = \"auto\"\n\n\
                 [commands.EitriPwtest]\ndoes = \"show\"\n[commands.EitriPwtestHide]\ndoes = \"hide\"\n\n\
                 [settings.auto]\nkind = \"bool\"\ndefault = true\n"
            ),
        )
        .unwrap();
        std::fs::write(
            plugin.join("site/index.html"),
            "<!doctype html><title>t</title><p>page</p>\n",
        )
        .unwrap();
        // Anything that could start the agent's sidecar finds this first, and it answers nothing.
        let claude = scratch.join("bin/claude");
        script_fixture::write_executable_script(&claude, "#!/bin/sh\nexit 1\n", 0o755).unwrap();
        std::fs::write(scratch.join("project/doc.md"), "# Doc\n\nline\n").unwrap();
        std::fs::write(scratch.join("project/other.md"), "# Other\n").unwrap();

        let sock = runtime.join("nvim.sock");
        let mut command = Command::new("nvim");
        command.args(["--clean", "--headless", "--listen"]).arg(&sock);
        if let Some(cmd) = nvim_cmd {
            command.args(["--cmd", cmd]);
        }
        command.arg(scratch.join("project/doc.md"));
        isolate_env(&mut command, &scratch, &runtime);
        let nvim = command
            .current_dir(scratch.join("project"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("nvim starts");
        let case = Case {
            scratch,
            runtime,
            nvim,
            sock,
            shell: None,
        };
        let until = Instant::now() + HANG;
        while !case.sock.exists() {
            assert!(Instant::now() < until, "nvim never listened");
            std::thread::sleep(Duration::from_millis(20));
        }
        case
    }

    /// `eitri page pwtest --nvim <sock> -- <project>`, its output in files of the case's own.
    fn start_page(&mut self) {
        let mut command = Command::new(env!("CARGO_BIN_EXE_shell"));
        command
            .args(["page", ID, "--nvim"])
            .arg(&self.sock)
            .arg("--")
            .arg(self.scratch.join("project"));
        isolate_env(&mut command, &self.scratch, &self.runtime);
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
        let until = Instant::now() + HANG;
        while Instant::now() < until {
            if self.out().contains(needle) {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        Err(format!("no {needle:?} in the window's output:\n{}", self.out()))
    }

    /// The window is up and attached, and the plugin's commands are in nvim.
    fn wait_attached(&self) -> Result<(), String> {
        self.wait_for_line("[page] attached")?;
        let until = Instant::now() + HANG;
        while Instant::now() < until {
            if self.expr("exists(':EitriPwtest')") == "2" {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        Err(format!(
            "the follow snippet never defined its commands (page slots {}, follow states {}):\n{}",
            self.expr(
                "luaeval('vim.inspect({ pages = vim.tbl_map(function(s) return { chan = s.chan, torn = s.torn, \
                 adopt = type(s.adopt), installed = s.installed, failed = s.failed } end, \
                 rawget(_G, \"__eitri_pages\") or {}), chans = vim.tbl_map(function(c) return c.id end, \
                 vim.api.nvim_list_chans()) })')"
            ),
            self.expr("luaeval('vim.inspect(vim.tbl_keys(rawget(_G, \"__eitri_plugin_follow\") or {}))')"),
            self.out()
        ))
    }

    fn expr(&self, expr: &str) -> String {
        let output = Command::new("nvim")
            .args(["--clean", "--headless", "--server"])
            .arg(&self.sock)
            .args(["--remote-expr", expr])
            .stdin(Stdio::null())
            .output()
            .expect("nvim answers");
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn send(&self, keys: &str) {
        let status = Command::new("nvim")
            .args(["--clean", "--headless", "--server"])
            .arg(&self.sock)
            .args(["--remote-send", keys])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .status()
            .expect("nvim takes the keys");
        assert!(status.success());
    }

    /// The window's exit, or `Err` when it is still running after [`HANG`].
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

    fn signal_shell(&self, sig: libc::c_int) {
        let pid = self.shell.as_ref().expect("started").id() as libc::pid_t;
        // SAFETY: `kill` has no memory-safety preconditions; the pid is this case's own child, not yet reaped.
        unsafe { libc::kill(pid, sig) };
    }
}

impl Drop for Case {
    fn drop(&mut self) {
        for child in self.shell.iter_mut().chain(std::iter::once(&mut self.nvim)) {
            if child.try_wait().ok().flatten().is_none() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
        let _ = std::fs::remove_dir_all(&self.scratch);
        let _ = std::fs::remove_dir_all(&self.runtime);
    }
}

/// Every place the window or nvim could read a person's own files or reach a session points into the scratch.
fn isolate_env(command: &mut Command, scratch: &Path, runtime: &Path) {
    let path = format!(
        "{}:{}",
        scratch.join("bin").display(),
        std::env::var("PATH").unwrap_or_default()
    );
    command
        .env("HOME", scratch.join("home"))
        .env("XDG_CONFIG_HOME", scratch.join("config"))
        .env("XDG_DATA_HOME", scratch.join("data"))
        .env("XDG_STATE_HOME", scratch.join("state"))
        .env("XDG_CACHE_HOME", scratch.join("cache"))
        .env("XDG_RUNTIME_DIR", runtime)
        .env("PATH", path)
        .env("VERDANDI_CLAUDE_CLI_PATH", scratch.join("bin/claude"))
        .env_remove("NVIM")
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .env_remove("EITRI_CONFIG_DIR");
}

/// Entering another Markdown buffer sends the page an `Auto`: it is logged, and the window neither comes forward nor
/// takes the keyboard.
fn auto_through_listen_follow_pushes_no_raise_self() -> Result<(), String> {
    let mut case = Case::new("auto", None);
    case.start_page();
    case.wait_attached()?;
    case.send(":edit other.md<CR>");
    case.wait_for_line("[modules] lua:pwtest: a followed buffer was entered; this window stays where it is")?;
    let out = case.out();
    if out.contains("[page] raise this window") || out.contains("shown, so this window comes forward") {
        return Err(format!("an Auto raised the window:\n{out}"));
    }
    Ok(())
}

/// The plugin's show command reaches the controller whole, even with the page on screen, and the window comes forward.
fn show_through_listen_follow_raises_an_on_screen_window() -> Result<(), String> {
    let mut case = Case::new("show", None);
    case.start_page();
    case.wait_attached()?;
    case.send(":EitriPwtest<CR>");
    case.wait_for_line("[modules] lua:pwtest: shown, so this window comes forward")?;
    case.wait_for_line("[page] raise this window")
}

/// `:qa!` in nvim ends the link, and the window closes with exit 0 and its log line; the plugin's hide closes it too.
fn a_beside_window_closes_on_the_links_end() -> Result<(), String> {
    let mut case = Case::new("end", None);
    case.start_page();
    case.wait_attached()?;
    case.send(":qa!<CR>");
    let code = case.wait_shell()?;
    case.wait_for_line("[page] the editor went away; closing")?;
    if code != Some(0) {
        return Err(format!("exit {code:?}, wanted 0:\n{}", case.out()));
    }

    let mut case = Case::new("hide", None);
    case.start_page();
    case.wait_attached()?;
    case.send(":EitriPwtestHide<CR>");
    let code = case.wait_shell()?;
    if code != Some(0) {
        return Err(format!("the hide command: exit {code:?}, wanted 0:\n{}", case.out()));
    }
    Ok(())
}

/// A slot whose channel is not the window's own refuses the follow snippet: the window closes with exit 1 and its log
/// line, and does not try again.
fn a_refused_follow_snippet_closes_the_window_with_exit_1() -> Result<(), String> {
    // Every page slot this nvim makes answers another channel than its own.
    let foreign = "lua _G.__eitri_pages = setmetatable({}, { __newindex = function(t, k, v) \
                   rawset(t, k, setmetatable({}, { __index = function(_, key) if key == 'chan' then return -1 end \
                   return v[key] end, __newindex = v })) end })";
    let mut case = Case::new("refused", Some(foreign));
    case.start_page();
    let code = case.wait_shell()?;
    case.wait_for_line("[page] the editor refused the page's follow snippet; closing")?;
    if code != Some(1) {
        return Err(format!("exit {code:?}, wanted 1:\n{}", case.out()));
    }
    Ok(())
}

/// Eitri's own nvim refuses the page's slot: `eitri page` ends with exit 64 and the refusal before it builds anything,
/// so no window opens for a page that cannot follow.
fn a_refused_install_exits_64_before_the_window_is_built() -> Result<(), String> {
    let mut case = Case::new("eitri-window", Some("lua _G.__eitri_window = {}"));
    case.start_page();
    let code = case.wait_shell()?;
    case.wait_for_line("this window opens pages itself: press the prefix and the page's key")?;
    if code != Some(64) {
        return Err(format!("exit {code:?}, wanted 64:\n{}", case.out()));
    }
    let out = case.out();
    if out.contains(&format!("[plugin {ID}] loaded")) {
        return Err(format!("the window was built before the refusal:\n{out}"));
    }
    Ok(())
}

/// nvim waiting for the character after an `f`: an end signal closes the window at once, and the slot is gone from
/// nvim once it can run its timer again.
fn closing_a_beside_window_does_not_wait_for_a_blocked_nvim() -> Result<(), String> {
    let mut case = Case::new("blocked", None);
    case.start_page();
    case.wait_attached()?;
    case.send("f");
    case.signal_shell(libc::SIGTERM);
    let code = case.wait_shell()?;
    if code != Some(0) {
        return Err(format!("exit {code:?} with nvim blocked, wanted 0:\n{}", case.out()));
    }
    case.send("x");
    let until = Instant::now() + HANG;
    while Instant::now() < until {
        if case.expr("luaeval('vim.tbl_count(rawget(_G, \"__eitri_pages\") or {})')") == "0" {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Err(format!(
        "the page slot outlived its window: {}",
        case.expr("luaeval('vim.inspect(vim.tbl_keys(rawget(_G, \"__eitri_pages\") or {}))')")
    ))
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if !args.iter().any(|a| a == "--ignored" || a == "--include-ignored") {
        println!(
            "page_window_start: ignored (a real nvim, an X server of its own and WebKitGTK); run with `-- --ignored`"
        );
        return;
    }
    let server = own_x_server::isolate("page_window_start", "1024x768x24");
    let filter = args.iter().skip(1).find(|a| !a.starts_with('-')).cloned();
    let cases: [(&str, fn() -> Result<(), String>); 6] = [
        (
            "auto_through_listen_follow_pushes_no_raise_self",
            auto_through_listen_follow_pushes_no_raise_self,
        ),
        (
            "show_through_listen_follow_raises_an_on_screen_window",
            show_through_listen_follow_raises_an_on_screen_window,
        ),
        (
            "a_beside_window_closes_on_the_links_end",
            a_beside_window_closes_on_the_links_end,
        ),
        (
            "a_refused_follow_snippet_closes_the_window_with_exit_1",
            a_refused_follow_snippet_closes_the_window_with_exit_1,
        ),
        (
            "a_refused_install_exits_64_before_the_window_is_built",
            a_refused_install_exits_64_before_the_window_is_built,
        ),
        (
            "closing_a_beside_window_does_not_wait_for_a_blocked_nvim",
            closing_a_beside_window_does_not_wait_for_a_blocked_nvim,
        ),
    ];
    let mut failed = 0;
    let mut ran = 0;
    for (name, case) in cases {
        if filter.as_deref().is_some_and(|f| !name.contains(f)) {
            continue;
        }
        ran += 1;
        match case() {
            Ok(()) => println!("test {name} ... ok"),
            Err(why) => {
                failed += 1;
                println!("test {name} ... FAILED\n{why}");
            }
        }
    }
    println!("page_window_start: {} passed; {failed} failed", ran - failed);
    server.exit(if failed == 0 && ran > 0 { 0 } else { 1 });
}

#[cfg(test)]
mod script_fixture {
    include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/support/executable.rs"));
}
