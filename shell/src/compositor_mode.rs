//! `window.tiling = "auto"` in the shell: whether the compositor answers, and what the window says about the mode
//! it opened in.
//!
//! The compositor is asked on a thread of its own that `main` starts before GTK, from the environment alone. The GTK
//! thread only ever reads what has come in by then ([`Liveness::now`]): no answer yet counts as not live, so a slow
//! or dead compositor opens `eitri` mode for this run and the window says so, and nothing waits for it.

use eitri_core::compositor::mode::{
    first_start_toast, flip_note, liveness_argv, liveness_ok, read_mode_state, reason, socket_present,
    write_mode_state, Mode, ModeState, TilingChoice, Why, LIVENESS_WITHIN,
};
use eitri_core::compositor::Wm;
use std::ffi::OsString;
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

/// Runs a command and returns what it printed to stdout.
pub(crate) type Exec = Arc<dyn Fn(Vec<String>) -> Result<String, String> + Send + Sync>;

/// The compositor's answer to its version command, read without waiting.
pub(crate) struct Liveness {
    answer: Arc<Mutex<Option<bool>>>,
    #[cfg(test)]
    worker: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl Liveness {
    /// Asks `wm` its version on a new thread when its socket is there to be asked. The environment and the
    /// filesystem are the process's own.
    pub(crate) fn start(wm: Wm, exec: Exec) -> Liveness {
        let env = |name: &str| std::env::var_os(name);
        Liveness::start_with(wm, &env, &Path::exists, exec, LIVENESS_WITHIN)
    }

    /// [`Liveness::start`] with the environment, the filesystem and the bound given. A reply that comes in after
    /// `within` is thrown away: the window has opened without it by then.
    pub(crate) fn start_with(
        wm: Wm,
        env: &dyn Fn(&str) -> Option<OsString>,
        exists: &dyn Fn(&Path) -> bool,
        exec: Exec,
        within: Duration,
    ) -> Liveness {
        Self::start_with_clock(wm, env, exists, exec, within, Arc::new(Instant::now))
    }

    fn start_with_clock(
        wm: Wm,
        env: &dyn Fn(&str) -> Option<OsString>,
        exists: &dyn Fn(&Path) -> bool,
        exec: Exec,
        within: Duration,
        now: Arc<dyn Fn() -> Instant + Send + Sync>,
    ) -> Liveness {
        let answer = Arc::new(Mutex::new(None));
        let liveness = Liveness {
            answer: answer.clone(),
            #[cfg(test)]
            worker: Mutex::new(None),
        };
        let Some(argv) = liveness_argv(wm).filter(|_| socket_present(wm, env, exists)) else {
            return liveness;
        };
        let began = now();
        let spawned = std::thread::Builder::new()
            .name("compositor-liveness".into())
            .spawn(move || {
                let reply = exec(argv);
                if now().duration_since(began) > within {
                    println!("[mode] the compositor's reply came after {within:?}; it is not used");
                    return;
                }
                let live = reply.is_ok_and(|text| liveness_ok(wm, &text));
                *answer.lock().unwrap_or_else(PoisonError::into_inner) = Some(live);
            });
        #[cfg(test)]
        {
            *liveness.worker.lock().unwrap() = spawned.ok();
        }
        #[cfg(not(test))]
        if let Err(err) = spawned {
            println!("[mode] the compositor could not be asked: {err}");
        }
        liveness
    }

    /// What the compositor has answered so far: `Some(live)` once it has, within the bound; `None` while it has not,
    /// and when there was no socket to ask. Never waits.
    pub(crate) fn now(&self) -> Option<bool> {
        *self.answer.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Waits for the thread to end, so a test can read what it settled on.
    #[cfg(test)]
    fn finish(&self) {
        if let Some(worker) = self.worker.lock().unwrap().take() {
            worker.join().unwrap();
        }
    }
}

/// The real command: run to its end within `within`, else killed. The compositor's own tool can hang on a socket
/// nobody serves, and its thread must not outlive the window by that.
pub(crate) fn command_within(within: Duration) -> Exec {
    Arc::new(move |argv: Vec<String>| {
        let (program, args) = argv.split_first().ok_or_else(|| "empty command".to_string())?;
        let mut child = Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|err| err.to_string())?;
        let deadline = Instant::now() + within;
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
                Ok(None) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!("{program} did not answer in time"));
                }
                Err(err) => return Err(err.to_string()),
            }
        }
        let mut text = String::new();
        if let Some(mut out) = child.stdout.take() {
            out.read_to_string(&mut text).map_err(|err| err.to_string())?;
        }
        Ok(text)
    })
}

static PROBE: OnceLock<Liveness> = OnceLock::new();

/// Starts the process's one liveness probe for a desktop `detect` named; `main` calls it before the application is
/// built, from the environment alone.
pub(crate) fn start_probe(wm: Wm) {
    PROBE.get_or_init(|| Liveness::start(wm, command_within(LIVENESS_WITHIN)));
}

/// What the probe has answered so far; `None` when it was not started, has not answered or had no socket.
pub(crate) fn probe_now() -> Option<bool> {
    PROBE.get().and_then(Liveness::now)
}

/// The mode a window opened in, with what it says about it.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Settled {
    /// The one log line.
    pub log: String,
    /// What the user is told, once: the first-start toast, a change of mode, a rule that forced `eitri`.
    pub notes: Vec<String>,
    /// The first start's toast, apart from the notes: it waits for the first-start screen.
    pub first_toast: Option<String>,
    /// What to keep for the next run.
    pub state: ModeState,
}

fn mode_name(mode: Mode) -> &'static str {
    match mode {
        Mode::Eitri => "eitri",
        Mode::Compositor => "compositor",
    }
}

fn why_name(why: Why) -> &'static str {
    match why {
        Why::Setting => "setting",
        Why::NoTilingCompositor => "no tiling compositor",
        Why::NotLive => "compositor did not answer",
        Why::NotWayland => "not Wayland",
        Why::MacOs => "macOS",
        Why::OwnWindow => "own window",
        Why::Window2Orphan => "window 2 orphan",
        Why::Unpaired => "unpaired",
        Why::Alone => "no pair lock",
        Why::ModuleOrphans => "chat windows to relink",
    }
}

/// Records that the first start's toast went up, so it is said once. It is recorded then and not at the start: a window
/// closed before it showed says it next time.
fn remember_the_toast(dir: &Path, root: &Path) -> std::io::Result<()> {
    let state = ModeState {
        toasted: true,
        ..read_mode_state(dir, root)
    };
    write_mode_state(dir, root, &state)
}

/// [`remember_the_toast`] for this project, on a worker: the toast goes up on the GTK thread.
pub(crate) fn toast_went_up(project_root: &Path) {
    let Some(dir) = eitri_core::layout::persist::state_dir(
        std::env::var_os("XDG_STATE_HOME").as_deref(),
        std::env::var_os("HOME").as_deref(),
    ) else {
        return;
    };
    let root = project_root.to_path_buf();
    let worker = std::thread::Builder::new().name("mode-toast".into()).spawn(move || {
        if let Err(err) = remember_the_toast(&dir, &root) {
            println!("[mode] could not record the toast in {}: {err}", dir.display());
        }
    });
    if let Err(err) = worker {
        println!("[mode] could not record the toast ({})", err.kind());
    }
}

/// What a window that resolved `mode` for `why` logs and says, given what the last run recorded in `prev`.
pub(crate) fn settle(mode: Mode, why: Why, wm: Wm, choice: TilingChoice, prev: ModeState) -> Settled {
    let log = format!(
        "[mode] {} ({}, window.tiling = \"{}\")",
        mode_name(mode),
        why_name(why),
        choice.name()
    );
    let mut notes = Vec::new();
    let first = choice == TilingChoice::Auto && mode == Mode::Compositor && why == Why::Setting && !prev.toasted;
    let flip = flip_note(prev.last, mode, why, wm, choice);
    let forced = matches!(why, Why::OwnWindow | Why::Window2Orphan | Why::Unpaired);
    // Said only where the mode was in play: on a desktop `auto` leaves in `eitri` mode anyway, the rule changed nothing.
    let in_play = choice == TilingChoice::Compositor
        || (choice == TilingChoice::Auto && matches!(wm, Wm::Hyprland | Wm::Sway | Wm::Niri));
    let forced_note = (mode == Mode::Eitri && in_play && forced && flip.is_none())
        .then(|| format!("Chats and pages stay inside this window: {}", reason(why)));
    notes.extend(flip);
    notes.extend(forced_note);
    Settled {
        log,
        first_toast: first.then(|| first_start_toast(wm)),
        notes,
        state: ModeState {
            last: Some(mode),
            toasted: prev.toasted,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eitri_core::compositor::mode::resolve_mode;
    use eitri_core::plugin::test_support::Scratch;
    use eitri_core::wm::start::EarlyFacts;
    use eitri_core::wm::window::Tiling;
    use std::sync::mpsc;

    const SWAY_VERSION: &str = "{\"human_readable\":\"1.10\",\"major\":1,\"minor\":10,\"patch\":0}";
    const NIRI_VERSION: &str = "{\"compositor\":\"25.08 (abc)\",\"cli\":\"25.08 (abc)\"}\n";

    fn early(wm: Wm) -> EarlyFacts {
        EarlyFacts {
            started: false,
            shows_chat: true,
            mac: false,
            wayland: true,
            wm,
            window2_orphan: false,
            module_orphans: false,
            unpaired: false,
            alone: false,
            choice: Some(TilingChoice::Auto),
        }
    }

    fn env_of<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<OsString> + 'a {
        move |name| pairs.iter().find(|(k, _)| *k == name).map(|(_, v)| OsString::from(v))
    }

    fn answering(text: &'static str, asked: Arc<Mutex<Vec<Vec<String>>>>) -> Exec {
        Arc::new(move |argv| {
            asked.lock().unwrap().push(argv);
            Ok(text.to_string())
        })
    }

    fn start_liveness(
        wm: Wm,
        env: &dyn Fn(&str) -> Option<OsString>,
        exists: &dyn Fn(&Path) -> bool,
        exec: Exec,
        within: Duration,
    ) -> Liveness {
        let now = Instant::now();
        Liveness::start_with_clock(wm, env, exists, exec, within, Arc::new(move || now))
    }

    #[test]
    fn the_liveness_answer_is_read_without_waiting() {
        let (release, released) = mpsc::channel::<()>();
        let released = Mutex::new(released);
        let (began, started) = mpsc::channel::<Vec<String>>();
        let began = Mutex::new(began);
        let exec: Exec = Arc::new(move |argv| {
            began.lock().unwrap().send(argv).unwrap();
            released.lock().unwrap().recv().unwrap();
            Ok(SWAY_VERSION.to_string())
        });
        let env = env_of(&[("SWAYSOCK", "/run/user/1000/sway-ipc")]);
        let within = Duration::from_millis(20);
        let clock = Arc::new(Mutex::new(Instant::now()));
        let reading = clock.clone();
        let live = Liveness::start_with_clock(
            Wm::Sway,
            &env,
            &|_| true,
            exec,
            within,
            Arc::new(move || *reading.lock().unwrap()),
        );
        // The command is held, so a read that waited for it would never return.
        assert_eq!(live.now(), None);
        let argv = started
            .recv_timeout(Duration::from_secs(30))
            .expect("the command runs on its own thread");
        assert_eq!(argv.join(" "), "swaymsg -t get_version");
        assert_eq!(live.now(), None);
        // The answer comes after the bound: it is thrown away.
        *clock.lock().unwrap() += within * 3;
        release.send(()).unwrap();
        live.finish();
        assert_eq!(live.now(), None, "a reply past the bound counts for nothing");
    }

    #[test]
    fn a_late_answer_opens_eitri_mode_with_the_note() {
        let (release, released) = mpsc::channel::<()>();
        let released = Mutex::new(released);
        let exec: Exec = Arc::new(move |_| {
            released.lock().unwrap().recv().unwrap();
            Ok(SWAY_VERSION.to_string())
        });
        let env = env_of(&[("SWAYSOCK", "/run/user/1000/sway-ipc")]);
        let within = Duration::from_millis(20);
        let clock = Arc::new(Mutex::new(Instant::now()));
        let reading = clock.clone();
        let live = Liveness::start_with_clock(
            Wm::Sway,
            &env,
            &|_| true,
            exec,
            within,
            Arc::new(move || *reading.lock().unwrap()),
        );
        *clock.lock().unwrap() += within * 3;
        release.send(()).unwrap();
        live.finish();
        let (tiling, mode, why) = early(Wm::Sway).tiling(false, live.now());
        assert_eq!((tiling, mode, why), (Tiling::Eitri, Mode::Eitri, Why::NotLive));
        let last_run = ModeState {
            last: Some(Mode::Compositor),
            toasted: true,
        };
        let settled = settle(mode, why, Wm::Sway, TilingChoice::Auto, last_run);
        assert_eq!(
            settled.notes,
            ["opened as tiles this time: sway did not answer (window.tiling = \"auto\")"]
        );
        assert_eq!(
            settled.log,
            "[mode] eitri (compositor did not answer, window.tiling = \"auto\")"
        );
    }

    #[test]
    fn auto_on_sway_with_a_live_socket_opens_compositor_mode() {
        let scratch = Scratch::new("cm-live-sway");
        let socket = scratch.0.join("sway-ipc");
        std::fs::write(&socket, "").unwrap();
        let socket = socket.to_string_lossy().into_owned();
        let pairs = [("SWAYSOCK", socket.as_str())];
        let env = env_of(&pairs);
        let asked = Arc::default();
        let live = start_liveness(
            Wm::Sway,
            &env,
            &Path::exists,
            answering(SWAY_VERSION, Arc::clone(&asked)),
            LIVENESS_WITHIN,
        );
        live.finish();
        assert_eq!(asked.lock().unwrap().concat().join(" "), "swaymsg -t get_version");
        assert_eq!(live.now(), Some(true));
        let (tiling, mode, why) = early(Wm::Sway).tiling(false, live.now());
        assert_eq!((tiling, mode, why), (Tiling::Hub, Mode::Compositor, Why::Setting));
    }

    #[test]
    fn auto_on_niri_with_its_real_version_reply_opens_compositor_mode() {
        let scratch = Scratch::new("cm-live-niri");
        let socket = scratch.0.join("niri-ipc");
        std::fs::write(&socket, "").unwrap();
        let socket = socket.to_string_lossy().into_owned();
        let pairs = [("NIRI_SOCKET", socket.as_str())];
        let env = env_of(&pairs);
        let asked = Arc::default();
        let live = start_liveness(
            Wm::Niri,
            &env,
            &Path::exists,
            answering(NIRI_VERSION, Arc::clone(&asked)),
            LIVENESS_WITHIN,
        );
        live.finish();
        assert_eq!(asked.lock().unwrap().concat().join(" "), "niri msg --json version");
        assert_eq!(live.now(), Some(true));
        assert_eq!(
            early(Wm::Niri).tiling(false, live.now()),
            (Tiling::Hub, Mode::Compositor, Why::Setting)
        );
    }

    #[test]
    fn no_socket_asks_nothing_and_a_dead_one_is_not_live() {
        let asked = Arc::default();
        let gone = start_liveness(
            Wm::Sway,
            &env_of(&[("SWAYSOCK", "/run/user/1000/sway-ipc")]),
            &|_| false,
            answering(SWAY_VERSION, Arc::clone(&asked)),
            LIVENESS_WITHIN,
        );
        gone.finish();
        assert!(asked.lock().unwrap().is_empty());
        assert_eq!(gone.now(), None);
        // A desktop with nothing to ask is never asked.
        let gnome = start_liveness(
            Wm::Gnome,
            &env_of(&[]),
            &|_| true,
            answering("{}", Arc::clone(&asked)),
            LIVENESS_WITHIN,
        );
        gnome.finish();
        assert!(asked.lock().unwrap().is_empty());
        // A reply that is no version, or a command that fails, says not live.
        let socket = env_of(&[("SWAYSOCK", "/s")]);
        let wrong = start_liveness(
            Wm::Sway,
            &socket,
            &|_| true,
            answering("nope", Arc::default()),
            LIVENESS_WITHIN,
        );
        wrong.finish();
        assert_eq!(wrong.now(), Some(false));
        let failing: Exec = Arc::new(|_| Err("no such program".into()));
        let failed = start_liveness(Wm::Sway, &socket, &|_| true, failing, LIVENESS_WITHIN);
        failed.finish();
        assert_eq!(failed.now(), Some(false));
        assert_eq!(
            resolve_mode(&early(Wm::Sway).mode_facts(false, failed.now())),
            (Mode::Eitri, Why::NotLive)
        );
    }

    #[test]
    fn the_real_command_is_cut_off_at_its_bound_and_reads_what_a_quick_one_prints() {
        let quick = command_within(Duration::from_secs(30));
        assert_eq!(quick(vec!["echo".into(), "hello".into()]).unwrap().trim(), "hello");
        assert!(quick(vec!["no-such-program-for-the-liveness-test".into()]).is_err());
        assert!(quick(vec![]).is_err());
        let slow = command_within(Duration::from_millis(50));
        let err = slow(vec!["sleep".into(), "30".into()]).unwrap_err();
        assert!(err.contains("did not answer"), "{err}");
    }

    #[test]
    fn snotras_own_window_keeps_its_grant_under_auto_on_sway() {
        let scratch = Scratch::new("cm-own-window");
        let socket = scratch.0.join("sway-ipc");
        std::fs::write(&socket, "").unwrap();
        let socket = socket.to_string_lossy().into_owned();
        let pairs = [("SWAYSOCK", socket.as_str())];
        let env = env_of(&pairs);
        let live = start_liveness(
            Wm::Sway,
            &env,
            &Path::exists,
            answering(SWAY_VERSION, Arc::default()),
            LIVENESS_WITHIN,
        );
        live.finish();
        assert_eq!(live.now(), Some(true));
        let (tiling, mode, why) = early(Wm::Sway).tiling(true, live.now());
        assert_eq!((tiling, mode, why), (Tiling::Eitri, Mode::Eitri, Why::OwnWindow));
        let settled = settle(mode, why, Wm::Sway, TilingChoice::Auto, ModeState::default());
        assert_eq!(
            settled.notes,
            ["Chats and pages stay inside this window: this project's page has a window of its own"]
        );
        // The id the start plan carries reaches the plugin hosts whatever the mode: the grant is set from the plan's
        // own window before any page is built, and never inside a branch on the mode.
        let code = include_str!("main.rs");
        let call = code
            .find("plugin_panels.set_own_window(own_window.as_deref());")
            .expect("the hosts learn which plugin owns the window");
        let resolved = code.find("let hub = tiling ==").expect("the mode is resolved");
        assert!(resolved < call);
        assert!(
            !code[resolved..call].contains("if hub {\n        plugin_panels"),
            "the grant does not depend on the mode"
        );
    }

    #[test]
    fn a_hub_starts_the_review_pump_and_no_other_window_does() {
        use eitri_core::source_scan as scan;
        let sources = [scan::read_required("shell/src/main.rs")];
        let (_, build) = scan::function_in(&sources, "build_ui");
        let hub = build
            .find("if hub")
            .and_then(|at| build[at..].find('{').map(|open| at + open))
            .expect("the window's start branches on the hub");
        let branch = scan::braced(&build, hub);
        assert_eq!(scan::calls(branch, "start_review_pump"), 1, "{branch}");
        assert_eq!(scan::calls(&build, "start_review_pump"), 1);
    }

    #[test]
    fn a_rule_that_forces_eitri_says_nothing_where_auto_would_not_have_handed_over() {
        for why in [Why::OwnWindow, Why::Window2Orphan, Why::Unpaired] {
            for wm in [Wm::Gnome, Wm::None] {
                let settled = settle(Mode::Eitri, why, wm, TilingChoice::Auto, ModeState::default());
                assert!(settled.notes.is_empty(), "{why:?} on {wm:?}: {:?}", settled.notes);
                assert!(
                    settled.log.starts_with("[mode] eitri ("),
                    "the log line stays: {}",
                    settled.log
                );
            }
            // Asked for by name, the mode was in play on any desktop.
            let forced = settle(
                Mode::Eitri,
                why,
                Wm::Gnome,
                TilingChoice::Compositor,
                ModeState::default(),
            );
            assert_eq!(
                forced.notes,
                [format!("Chats and pages stay inside this window: {}", reason(why))]
            );
        }
    }

    #[test]
    fn the_first_auto_start_toasts_once() {
        let first = settle(
            Mode::Compositor,
            Why::Setting,
            Wm::Hyprland,
            TilingChoice::Auto,
            ModeState::default(),
        );
        assert_eq!(first.first_toast, Some(first_start_toast(Wm::Hyprland)));
        assert!(first.notes.is_empty());
        assert_eq!(
            first.state,
            ModeState {
                last: Some(Mode::Compositor),
                toasted: false
            },
            "it counts once it has gone up"
        );
        assert_eq!(first.log, "[mode] compositor (setting, window.tiling = \"auto\")");
        let unshown = settle(
            Mode::Compositor,
            Why::Setting,
            Wm::Hyprland,
            TilingChoice::Auto,
            first.state,
        );
        assert_eq!(
            unshown.first_toast, first.first_toast,
            "a toast that never showed comes back"
        );
        let again = settle(
            Mode::Compositor,
            Why::Setting,
            Wm::Hyprland,
            TilingChoice::Auto,
            ModeState {
                toasted: true,
                ..first.state
            },
        );
        assert_eq!(again.first_toast, None);
        assert!(again.notes.is_empty());
        // Forcing compositor mode is no detection to announce, and neither is a relink.
        let forced = settle(
            Mode::Compositor,
            Why::Setting,
            Wm::Gnome,
            TilingChoice::Compositor,
            ModeState::default(),
        );
        assert!(forced.notes.is_empty());
        assert!(!forced.state.toasted);
        let orphans = settle(
            Mode::Compositor,
            Why::ModuleOrphans,
            Wm::Hyprland,
            TilingChoice::Auto,
            ModeState::default(),
        );
        assert!(orphans.notes.is_empty());
        // The toast and a change of mode on the same start both come, the toast first.
        let both = settle(
            Mode::Compositor,
            Why::Setting,
            Wm::Niri,
            TilingChoice::Auto,
            ModeState {
                last: Some(Mode::Eitri),
                toasted: false,
            },
        );
        assert_eq!(both.first_toast, Some(first_start_toast(Wm::Niri)));
        assert_eq!(
            both.notes,
            ["opened as separate windows this time (window.tiling = \"auto\")".to_string()]
        );
    }

    #[test]
    fn a_flip_writes_one_note_and_the_next_run_none() {
        let scratch = Scratch::new("cm-flip");
        let dir = scratch.0.join("layout");
        let root = Path::new("/project");
        let run = |mode, why| {
            let prev = read_mode_state(&dir, root);
            let settled = settle(mode, why, Wm::Hyprland, TilingChoice::Auto, prev);
            write_mode_state(&dir, root, &settled.state).unwrap();
            settled.first_toast.into_iter().chain(settled.notes).collect::<Vec<_>>()
        };
        assert_eq!(run(Mode::Compositor, Why::Setting).len(), 1, "the first start's toast");
        remember_the_toast(&dir, root).unwrap();
        assert_eq!(
            read_mode_state(&dir, root).last,
            Some(Mode::Compositor),
            "what the start wrote stays"
        );
        assert!(run(Mode::Compositor, Why::Setting).is_empty());
        assert_eq!(
            run(Mode::Eitri, Why::NotLive),
            ["opened as tiles this time: Hyprland did not answer (window.tiling = \"auto\")"]
        );
        assert!(
            run(Mode::Eitri, Why::NotLive).is_empty(),
            "the same mode again says nothing"
        );
        assert_eq!(
            run(Mode::Compositor, Why::Setting),
            ["opened as separate windows this time (window.tiling = \"auto\")"]
        );
    }

    #[test]
    fn a_window_2_orphan_makes_this_run_eitri_and_says_so() {
        for choice in [TilingChoice::Auto, TilingChoice::Compositor] {
            let facts = EarlyFacts {
                window2_orphan: true,
                choice: Some(choice),
                ..early(Wm::Hyprland)
            };
            let (tiling, mode, why) = facts.tiling(false, Some(true));
            assert_eq!((tiling, mode, why), (Tiling::Eitri, Mode::Eitri, Why::Window2Orphan));
            let settled = settle(mode, why, Wm::Hyprland, choice, ModeState::default());
            assert_eq!(
                settled.notes,
                ["Chats and pages stay inside this window: window 2 was left without its partner"]
            );
            assert!(
                settled
                    .log
                    .starts_with("[mode] eitri (window 2 orphan, window.tiling = "),
                "{}",
                settled.log
            );
        }
        // Under `eitri` there is nothing to explain, and when the mode flipped the flip note carries the reason.
        let quiet = settle(
            Mode::Eitri,
            Why::Window2Orphan,
            Wm::Hyprland,
            TilingChoice::Eitri,
            ModeState::default(),
        );
        assert!(quiet.notes.is_empty());
        let flipped = settle(
            Mode::Eitri,
            Why::Unpaired,
            Wm::Hyprland,
            TilingChoice::Auto,
            ModeState {
                last: Some(Mode::Compositor),
                toasted: true,
            },
        );
        assert_eq!(
            flipped.notes,
            ["opened as tiles this time: this project is open in another Eitri window"]
        );
    }

    #[test]
    fn module_orphans_make_this_run_compositor() {
        for wm in [Wm::Hyprland, Wm::Gnome] {
            let facts = EarlyFacts {
                module_orphans: true,
                choice: Some(TilingChoice::Eitri),
                ..early(wm)
            };
            let (tiling, mode, why) = facts.tiling(false, None);
            assert_eq!((tiling, mode, why), (Tiling::Hub, Mode::Compositor, Why::ModuleOrphans));
            let settled = settle(
                mode,
                why,
                wm,
                TilingChoice::Eitri,
                ModeState {
                    last: Some(Mode::Eitri),
                    toasted: false,
                },
            );
            assert_eq!(
                settled.notes,
                ["opened as separate windows this time: chat windows were waiting to be relinked"]
            );
            assert!(!settled.state.toasted, "a relink is no first start under auto");
        }
    }
}
