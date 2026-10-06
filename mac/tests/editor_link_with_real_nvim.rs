//! The editor half of the Mac host against a real `nvim --headless --listen`: it attaches, installs the
//! glue, turns nvim's own navigator into a pane switch at nvim's edge, follows the colourscheme, and lets
//! go cleanly.
//!
//! `#[ignore]`d: needs `nvim` on PATH. No tokens, no network, no display.
//!
//! Run: `cargo test -p eitri-mac --test editor_link_with_real_nvim -- --ignored`

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::rc::Rc;
use std::time::{Duration, Instant};

use eitri_core::layout::Direction;
use eitri_core::nvim_rpc::NvimLink;
use eitri_mac::editor::{Editor, EditorEvent, TICK};
use eitri_mac::startup;
use rmpv::Value;

/// Under `std::env::temp_dir()`, not the target directory: the socket is capped at 103 bytes and a
/// worktree's target directory already uses most of that. `case` stays short for the same reason.
fn run_dir(case: &str) -> PathBuf {
    std::env::temp_dir().join(format!("em-{}-{case}", std::process::id()))
}

/// A headless nvim listening where the Mac host would have Neovide's nvim listen, killed by its own handle
/// and removed on drop.
struct Nvim {
    child: Child,
    run_dir: PathBuf,
    listen: PathBuf,
}

/// The socket nvim will listen on, with nothing started yet.
struct Reserved {
    run_dir: PathBuf,
    listen: PathBuf,
}

impl Reserved {
    fn new(case: &str) -> Reserved {
        let run_dir = run_dir(case);
        let _ = std::fs::remove_dir_all(&run_dir);
        std::fs::create_dir_all(run_dir.join("state")).unwrap();
        let home = run_dir.join("home");
        std::fs::create_dir_all(&home).unwrap();
        let env = |name: &str| -> Option<OsString> { (name == "XDG_STATE_HOME").then(|| run_dir.join("state").into()) };
        let paths = startup::paths(&env, &home, &run_dir).expect("the paths of a start");
        Reserved {
            run_dir,
            listen: paths.nvim_listen,
        }
    }

    fn spawn(self) -> Nvim {
        let Reserved { run_dir, listen } = self;
        std::fs::write(run_dir.join("s.txt"), "a b c x\nline two\n").unwrap();
        let mut command = Command::new("nvim");
        command
            .args(["--headless", "--clean", "-n", "-i", "NONE", "--listen"])
            .arg(&listen)
            .arg(run_dir.join("s.txt"))
            .current_dir(&run_dir)
            .env("XDG_STATE_HOME", run_dir.join("state"))
            .env("XDG_DATA_HOME", run_dir.join("data"))
            .env("XDG_CONFIG_HOME", run_dir.join("config"))
            // An inherited `$TMUX` would turn the navigator rule off.
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .env_remove("NVIM")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        for (name, _) in std::env::vars_os() {
            if name.to_str().is_some_and(|n| n.starts_with("EITRI_")) {
                command.env_remove(name);
            }
        }
        let child = command.spawn().expect("nvim must be on PATH for this test");
        Nvim { child, run_dir, listen }
    }
}

impl Nvim {
    fn start(case: &str) -> Nvim {
        Reserved::new(case).spawn()
    }

    fn wait_for_socket_file(&self) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !self.listen.exists() {
            assert!(Instant::now() < deadline, "nvim never created its socket");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// An oracle connection of the test's own. The socket file appears a moment before nvim listens.
    fn probe(&self) -> Probe {
        self.wait_for_socket_file();
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            match NvimLink::connect(&self.listen, Duration::from_secs(1)) {
                Ok((link, events)) => return Probe { link, _events: events },
                Err(e) => {
                    assert!(Instant::now() < deadline, "could not connect: {e}");
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
        }
    }

    /// Past `VimEnter`: an install before it takes the late-install branch, which most tests are not about.
    fn started(case: &str) -> (Nvim, Probe) {
        let nvim = Nvim::start(case);
        let probe = nvim.probe();
        until("nvim finishing its start", Duration::from_secs(5), || {
            (probe.eval("v:vim_did_enter").as_i64() == Some(1)).then_some(())
        });
        (nvim, probe)
    }
}

impl Drop for Nvim {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.run_dir);
    }
}

struct Probe {
    link: NvimLink,
    _events: std::sync::mpsc::Receiver<eitri_core::nvim_rpc::LinkEvent>,
}

impl Probe {
    fn call(&self, method: &str, params: Vec<Value>) -> Value {
        match self.link.call(method, params).wait(Duration::from_secs(5)) {
            Some(Ok(value)) => value,
            Some(Err(e)) => panic!("{method}: {e}"),
            None => panic!("{method}: no answer"),
        }
    }
    fn eval(&self, expr: &str) -> Value {
        self.call("nvim_eval", vec![Value::from(expr)])
    }
    fn lua(&self, code: &'static str) -> Value {
        match self.link.exec_lua(code, vec![]).wait(Duration::from_secs(5)) {
            Some(Ok(value)) => value,
            Some(Err(e)) => panic!("{code}: {e}"),
            None => panic!("{code}: no answer"),
        }
    }
    fn command(&self, command: &str) {
        self.call("nvim_command", vec![Value::from(command)]);
    }
    fn winnr(&self) -> i64 {
        self.eval("winnr()").as_i64().expect("a window number")
    }
}

fn until<T>(what: &str, within: Duration, mut check: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + within;
    loop {
        if let Some(found) = check() {
            return found;
        }
        assert!(Instant::now() < deadline, "timed out after {within:?}: {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn editor_at(path: &Path) -> Rc<Editor> {
    Rc::new(Editor::new(path.to_path_buf()).expect("an editor"))
}

/// Polls the editor every tick until `check` accepts an event. Every event seen is kept in `seen`.
fn pump<T>(
    what: &str,
    editor: &Editor,
    within: Duration,
    seen: &mut Vec<String>,
    mut check: impl FnMut(&EditorEvent) -> Option<T>,
) -> T {
    let deadline = Instant::now() + within;
    loop {
        for event in editor.poll(Instant::now()) {
            seen.push(describe(&event));
            if let Some(found) = check(&event) {
                return found;
            }
        }
        assert!(
            Instant::now() < deadline,
            "timed out after {within:?}: {what}; saw {seen:?}"
        );
        std::thread::sleep(TICK);
    }
}

fn describe(event: &EditorEvent) -> String {
    match event {
        EditorEvent::PaneSwitch(direction) => format!("pane switch {direction:?}"),
        EditorEvent::Theme(payload) => format!("theme {}", payload.options.colors_name),
        EditorEvent::Keys(_) => "keys".to_string(),
        EditorEvent::LinkChanged(band) => format!("link {} {}", band.state, band.text),
        EditorEvent::CancelDrafts => "cancel drafts".to_string(),
    }
}

fn attached(editor: &Editor, seen: &mut Vec<String>) {
    editor.attach();
    pump("the panel attached", editor, Duration::from_secs(5), seen, |event| {
        matches!(event, EditorEvent::LinkChanged(band) if band.state == "attached").then_some(())
    });
}

/// Nothing arrives within `quiet`.
fn nothing_for(editor: &Editor, quiet: Duration, what: &str) {
    let until = Instant::now() + quiet;
    while Instant::now() < until {
        for event in editor.poll(Instant::now()) {
            if let EditorEvent::PaneSwitch(_) = event {
                panic!("{what}: {}", describe(&event));
            }
        }
        std::thread::sleep(TICK);
    }
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn attaches_and_switches_panes_at_nvims_right_edge() {
    let (nvim, probe) = Nvim::started("edge");
    let editor = editor_at(&nvim.listen);
    let mut seen = Vec::new();
    attached(&editor, &mut seen);
    // The nav fallback maps `<C-l>` from a scheduled check once the install ran.
    until("the nav fallback's map", Duration::from_secs(3), || {
        (probe
            .eval("maparg('<C-l>', 'n')")
            .as_str()
            .is_some_and(|map| !map.is_empty()))
        .then_some(())
    });
    // `:vsplit` leaves the cursor in the left window.
    probe.command("vsplit");
    assert_eq!(probe.winnr(), 1);

    editor.send_keys("<C-l>").expect("the keys went");
    until("nvim moving to the right window", Duration::from_secs(2), || {
        let _ = editor.poll(Instant::now());
        (probe.winnr() == 2).then_some(())
    });
    nothing_for(
        &editor,
        Duration::from_millis(300),
        "a move inside nvim must not switch panes",
    );

    editor.send_keys("<C-l>").expect("the keys went");
    pump(
        "the switch at nvim's edge",
        &editor,
        Duration::from_secs(2),
        &mut seen,
        |event| matches!(event, EditorEvent::PaneSwitch(Direction::Right)).then_some(()),
    );
    editor.shutdown();
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn keys_sent_while_nvim_waits_for_a_character_complete_it() {
    let (nvim, probe) = Nvim::started("pending");
    let editor = editor_at(&nvim.listen);
    let mut seen = Vec::new();
    attached(&editor, &mut seen);
    probe.command(&format!("edit {}", nvim.run_dir.join("s.txt").display()));
    // `f` leaves nvim waiting for a character. A Lua call made then would be held until the user typed
    // another key, and `x` would then delete a character instead of finishing the motion.
    editor.send_keys("f").expect("the keys went");
    editor.send_keys("x").expect("the keys went");
    // Not api-fast: this answers only once nvim is done with the keys above, so it cannot pass early.
    until("`fx` moving to the x", Duration::from_secs(3), || {
        (probe.eval("col('.')").as_i64() == Some(7)).then_some(())
    });
    assert_eq!(
        probe.eval("getline(1)").as_str(),
        Some("a b c x"),
        "nothing was deleted"
    );
    editor.shutdown();
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_colorscheme_change_is_a_theme_event() {
    let (nvim, probe) = Nvim::started("theme");
    let editor = editor_at(&nvim.listen);
    let mut seen = Vec::new();
    attached(&editor, &mut seen);
    // The install's first theme, if it comes.
    let quiet = Instant::now() + Duration::from_millis(500);
    while Instant::now() < quiet {
        let _ = editor.poll(Instant::now());
        std::thread::sleep(TICK);
    }
    probe.command("colorscheme blue");
    pump(
        "a theme event",
        &editor,
        Duration::from_secs(3),
        &mut seen,
        |event| match event {
            EditorEvent::Theme(payload) => (payload.options.colors_name == "blue").then_some(()),
            _ => None,
        },
    );
    editor.shutdown();
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn shutdown_leaves_no_glue_behind() {
    let (nvim, probe) = Nvim::started("glue");
    let editor = editor_at(&nvim.listen);
    let mut seen = Vec::new();
    attached(&editor, &mut seen);
    assert_eq!(
        probe.lua("return rawget(_G, '__eitri_companion') ~= nil").as_bool(),
        Some(true)
    );
    editor.shutdown();
    until("the glue gone", Duration::from_secs(1), || {
        (probe.lua("return rawget(_G, '__eitri_companion') == nil").as_bool() == Some(true)).then_some(())
    });
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn nvim_exiting_detaches_and_ends_drafts() {
    let (mut nvim, _probe) = Nvim::started("quit");
    let editor = editor_at(&nvim.listen);
    let mut seen = Vec::new();
    attached(&editor, &mut seen);
    nvim.child.kill().expect("kill nvim by its own handle");
    nvim.child.wait().unwrap();
    let (mut detached, mut cancelled) = (false, false);
    pump(
        "the link noticing nvim went",
        &editor,
        Duration::from_secs(3),
        &mut seen,
        |event| {
            match event {
                EditorEvent::LinkChanged(band) if band.state != "attached" => detached = true,
                EditorEvent::CancelDrafts => cancelled = true,
                _ => {}
            }
            (detached && cancelled).then_some(())
        },
    );
    assert!((editor.context())().is_none(), "a gone editor leaves no context behind");
    editor.shutdown();
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn an_attach_as_the_socket_appears_still_attaches() {
    let nvim = Nvim::start("race");
    nvim.wait_for_socket_file();
    // No wait for the listen, and none for `VimEnter`: attach the moment the file exists.
    let editor = editor_at(&nvim.listen);
    let mut seen = Vec::new();
    attached(&editor, &mut seen);
    editor.shutdown();
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn an_attach_before_nvim_listens_is_tried_again_and_not_reported_failed() {
    let reserved = Reserved::new("late");
    let editor = editor_at(&reserved.listen);
    let mut seen = Vec::new();
    editor.attach();
    // Nothing listens yet: every connect is refused, and the band must not say "failed" meanwhile.
    let quiet = Instant::now() + Duration::from_millis(600);
    while Instant::now() < quiet {
        for event in editor.poll(Instant::now()) {
            seen.push(describe(&event));
            assert!(
                !matches!(&event, EditorEvent::LinkChanged(band) if band.state == "failed"),
                "reported failed inside the retry window: {seen:?}"
            );
        }
        std::thread::sleep(TICK);
    }
    let nvim = reserved.spawn();
    pump(
        "the panel attached once nvim listened",
        &editor,
        Duration::from_secs(5),
        &mut seen,
        |event| matches!(event, EditorEvent::LinkChanged(band) if band.state == "attached").then_some(()),
    );
    editor.shutdown();
    drop(nvim);
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn an_attach_that_never_succeeds_is_reported_failed_after_the_retry_window() {
    let reserved = Reserved::new("never");
    let editor = editor_at(&reserved.listen);
    let mut seen = Vec::new();
    let started = Instant::now();
    editor.attach();
    pump("a failed band", &editor, Duration::from_secs(5), &mut seen, |event| {
        matches!(event, EditorEvent::LinkChanged(band) if band.state == "failed").then_some(())
    });
    assert!(
        started.elapsed() >= eitri_mac::editor::ATTACH_RETRY_FOR,
        "failed was reported after {:?}",
        started.elapsed()
    );
    editor.shutdown();
    let _ = std::fs::remove_dir_all(&reserved.run_dir);
}
