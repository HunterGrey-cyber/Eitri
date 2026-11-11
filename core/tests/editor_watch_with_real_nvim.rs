//! What the editor watch tells the window, against a real nvim: which ends read as a quit and which as a
//! crash, and which swap files a start reports. `-- --ignored`; no token, no display.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use eitri_core::pane_switch::{PaneMessage, PaneSwitchChannel, PaneSwitchReader};

const DEADLINE: Duration = Duration::from_secs(10);

struct Rig {
    root: PathBuf,
    channel: PaneSwitchChannel,
    reader: PaneSwitchReader,
}

impl Rig {
    fn new(label: &str) -> Rig {
        agent::state_dirs::redirect_state_to_a_test_root();
        let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("editor-watch-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("project/src")).unwrap();
        std::fs::create_dir_all(root.join("other")).unwrap();
        std::fs::write(root.join("project/src/a.txt"), "one\ntwo\n").unwrap();
        std::fs::write(root.join("project/b.txt"), "b\n").unwrap();
        std::fs::write(root.join("other/c.txt"), "c\n").unwrap();
        let mut channel = PaneSwitchChannel::bind_without_shim().expect("the channel binds");
        let reader = PaneSwitchReader::new(channel.take_listener().expect("the listener"));
        Rig { root, channel, reader }
    }

    /// nvim as a window starts it, in `dir`, with `extra` after the window's own arguments.
    fn nvim(&self, dir: &str, extra: &[&str]) -> Child {
        let _ = std::fs::remove_file(self.root.join("quit"));
        let _ = std::fs::remove_file(self.root.join("started"));
        Command::new("nvim")
            .args(["--headless", "--clean", "--cmd", "set updatetime=50"])
            .args(self.channel.nvim_args())
            .args(extra)
            .args([
                "-c",
                &format!("call writefile(['started'], '{}')", self.root.join("started").display()),
            ])
            .current_dir(self.root.join(dir))
            .envs(self.channel.child_env())
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("nvim on PATH")
    }

    /// Everything read until `done` says so, or the deadline.
    fn read_until(&mut self, done: impl Fn(&[PaneMessage]) -> bool) -> Vec<PaneMessage> {
        let deadline = Instant::now() + DEADLINE;
        let mut seen = Vec::new();
        while Instant::now() < deadline && !done(&seen) {
            seen.extend(self.reader.poll());
            std::thread::sleep(Duration::from_millis(20));
        }
        seen
    }

    fn quit_command(&self, verb: &str) -> String {
        format!("autocmd VimEnter * ++nested lua assert(vim.wait(60000, function() return vim.fn.filereadable('{}') == 1 end, 1)); vim.cmd('{}')", self.root.join("quit").display(), verb)
    }

    /// A sentinel accepted after the child's exit puts every earlier connection before the final drain.
    fn after_exit(&mut self, child: &mut Child) -> Vec<PaneMessage> {
        std::fs::write(self.root.join("quit"), b"").unwrap();
        child.wait().expect("nvim ends");
        self.drained()
    }

    fn drained(&mut self) -> Vec<PaneMessage> {
        use std::io::Write;
        let marker = PaneMessage::QuitCancelled(u32::MAX);
        let mut stream = std::os::unix::net::UnixStream::connect(self.channel.socket_path()).unwrap();
        writeln!(stream, "Q {}", u32::MAX).unwrap();
        drop(stream);
        let mut seen = self.read_until(|seen| seen.contains(&marker));
        assert!(seen.contains(&marker), "the reader never reached the drain sentinel");
        let deadline = Instant::now() + DEADLINE;
        while self.reader.holds_connections() {
            assert!(Instant::now() < deadline, "earlier connections never finished");
            seen.extend(self.reader.poll());
            std::thread::yield_now();
        }
        seen.retain(|message| message != &marker);
        seen
    }

    fn started(&self) {
        let deadline = Instant::now() + DEADLINE;
        while !self.root.join("started").exists() {
            assert!(Instant::now() < deadline, "nvim never finished its startup commands");
            std::thread::yield_now();
        }
    }

    /// An nvim left dead by SIGKILL with an unsaved line in `file`, its swap file written.
    fn crash_with_unsaved(&mut self, dir: &str, file: &str) {
        let script = "call append(0, 'UNSAVED') | preserve";
        let mut child = self.nvim(dir, &[file, "-c", script]);
        self.read_until(|seen| seen.contains(&PaneMessage::EditorWatched));
        let swaps = self.root.join("state/nvim/swap");
        let wanted = file.rsplit('/').next().unwrap().to_string();
        let deadline = Instant::now() + DEADLINE;
        while Instant::now() < deadline {
            let found = std::fs::read_dir(&swaps).into_iter().flatten().flatten().any(|entry| {
                entry.file_name().to_string_lossy().contains(&wanted) && entry.metadata().is_ok_and(|m| m.len() > 0)
            });
            if found {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        self.started();
        child.kill().unwrap();
        child.wait().unwrap();
    }

    fn swap_count(&self) -> usize {
        std::fs::read_dir(self.root.join("state/nvim/swap"))
            .map(|dir| dir.flatten().count())
            .unwrap_or(0)
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        self.channel.cleanup();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn recoverable(seen: &[PaneMessage]) -> Option<(u32, String)> {
    seen.iter().find_map(|message| match message {
        PaneMessage::Recoverable { count, newest } => Some((*count, newest.clone())),
        _ => None,
    })
}

fn watched(seen: &[PaneMessage]) -> bool {
    seen.contains(&PaneMessage::EditorWatched)
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_quit_says_it_is_leaving_and_a_signal_does_not() {
    let mut rig = Rig::new("ends");
    let mut child = rig.nvim("project", &["-c", &rig.quit_command("qa!")]);
    rig.read_until(watched);
    assert_eq!(
        rig.after_exit(&mut child),
        [PaneMessage::EditorLeaving],
        ":qa! is a quit"
    );

    let mut child = rig.nvim("project", &["-c", &rig.quit_command("cquit")]);
    rig.read_until(watched);
    assert_eq!(
        rig.after_exit(&mut child),
        [PaneMessage::EditorLeaving],
        ":cq asks to quit"
    );

    let mut child = rig.nvim("project", &[]);
    rig.read_until(watched);
    child.kill().unwrap();
    assert_eq!(rig.after_exit(&mut child), [], "SIGKILL leaves no line");

    let mut child = rig.nvim("project", &[]);
    rig.read_until(watched);
    unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    assert_eq!(rig.after_exit(&mut child), [], "nobody in the window asked for SIGTERM");
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_config_that_clears_every_autocommand_leaves_the_exit_taken_as_a_quit() {
    let mut rig = Rig::new("cleared");
    let mut child = rig.nvim("project", &["-c", "autocmd!", "-c", "qa!"]);
    assert_eq!(child.wait().unwrap().code(), Some(0));
    assert!(rig.drained().is_empty(), "no line says it is watched");
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_start_names_the_projects_unsaved_files_and_recovers_nothing() {
    let mut rig = Rig::new("swaps");
    let mut clean = rig.nvim("project", &[]);
    let seen = rig.read_until(|seen| watched(seen));
    clean.kill().unwrap();
    clean.wait().unwrap();
    assert_eq!(recoverable(&seen), None, "no swap file, no line");

    rig.crash_with_unsaved("project", "src/a.txt");
    rig.crash_with_unsaved("other", "c.txt");
    let swaps = rig.swap_count();
    assert_eq!(swaps, 2);

    let mut child = rig.nvim("project", &["--cmd", &rig.quit_command("qa!")]);
    let seen = rig.read_until(|seen| recoverable(seen).is_some());
    assert_eq!(
        recoverable(&seen),
        Some((1, "src/a.txt".to_string())),
        "not the other project's"
    );
    rig.after_exit(&mut child);
    assert_eq!(rig.swap_count(), swaps, "the swap files stay");
    assert_eq!(
        std::fs::read_to_string(rig.root.join("project/src/a.txt")).unwrap(),
        "one\ntwo\n",
        "the file is not touched"
    );
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_file_another_nvim_is_editing_is_not_reported() {
    let mut rig = Rig::new("alive");
    let script = "call append(0, 'UNSAVED') | preserve";
    let mut editing = rig.nvim("project", &["b.txt", "-c", script]);
    rig.read_until(watched);
    rig.started();

    let mut second = rig.nvim("project", &["--cmd", &rig.quit_command("qa!")]);
    rig.started();
    std::fs::write(rig.root.join("quit"), b"").unwrap();
    let seen = rig.read_until(|seen| seen.contains(&PaneMessage::EditorLeaving));
    std::fs::write(rig.root.join("quit"), b"").unwrap();
    second.wait().unwrap();
    editing.kill().unwrap();
    editing.wait().unwrap();
    assert_eq!(recoverable(&seen), None);
}
