//! What the first-start screen's rows learn from this machine, on a worker thread: the tmux import this window already
//! ran, the desktop (its keys and `$mod`), the windows row and the input method. Nothing here touches the GTK thread,
//! and nothing reaches the machine except through the four things a [`DetectContext`] holds, so a test reads no
//! session bus, runs no command and opens no real file.
//!
//! Every row is sent once, when it is known. A compositor command that never answers sends no desktop row (and no
//! windows row, which follows it): the row stays `looking…`, and the worker still ends, because each command is killed
//! after [`COMMAND_TIME`](crate::root_conflicts) by `run_capped`. A command that cannot start is an answer: the
//! compositor is named and not live.

use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use eitri_core::compositor::Wm;
use eitri_core::keymap::root_conflicts as judge;
use eitri_core::keymap::{Chord, TmuxImport};
use eitri_core::onboarding::detect;
use eitri_core::onboarding::{Build, DesktopRow, Found, Tiling};

/// The longest a compositor command may run before it is killed.
const COMMAND_TIME: Duration = Duration::from_secs(3);
/// The most output read from a compositor command.
const OUTPUT_LIMIT: usize = 1 << 20;

type Env = Box<dyn Fn(&str) -> Option<OsString> + Send>;
type Run = Box<dyn Fn(&[&str]) -> Result<String, String> + Send>;
type BusOwners = Box<dyn Fn() -> Vec<&'static str> + Send>;
type ReadFile = Box<dyn Fn(&Path) -> Option<String> + Send>;

/// Everything detection reads, so a test can hand it fixed answers.
pub(crate) struct DetectContext {
    pub(crate) env: Env,
    /// Runs `argv` with no shell and gives its standard output.
    pub(crate) run: Run,
    /// The input-method names on the session bus that have an owner.
    pub(crate) bus_owners: BusOwners,
    /// A regular file's text, or `None`.
    pub(crate) read: ReadFile,
    /// What the window's own tmux import found, from the config phase.
    pub(crate) tmux: Option<TmuxImport>,
    pub(crate) build: Build,
}

impl DetectContext {
    /// The real machine: the process environment, `run_capped`, the session bus and the file system.
    pub(crate) fn live(tmux: Option<TmuxImport>, build: Build) -> DetectContext {
        DetectContext {
            env: Box::new(|name| std::env::var_os(name)),
            run: Box::new(|argv| crate::root_conflicts::run_capped(argv, COMMAND_TIME, OUTPUT_LIMIT)),
            bus_owners: Box::new(|| {
                let mut notes = Vec::new();
                let owners = crate::root_conflicts::bus_owners(&mut notes);
                for note in notes {
                    eprintln!("[onboarding] input method check: {}", note.escape_debug());
                }
                owners
            }),
            read: Box::new(read_regular_file),
            tmux,
            build,
        }
    }
}

/// A regular file's text, at most one config's worth; anything else (a missing file, a directory, a FIFO, text that is
/// not UTF-8, a file that is too large) is `None`. The file is opened without waiting for a writer, so a FIFO with
/// none is refused by the check on the handle and never holds the worker in `open`.
fn read_regular_file(path: &Path) -> Option<String> {
    use std::os::unix::fs::OpenOptionsExt;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
        .ok()?;
    if !file.metadata().ok()?.is_file() {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(detect::MAX_CONFIG_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() > detect::MAX_CONFIG_BYTES {
        return None;
    }
    String::from_utf8(bytes).ok()
}

/// Runs the detection rows, sending each as it is known. Blocks until the last is sent or given up, so it belongs on a
/// worker thread.
pub(crate) fn run(ctx: DetectContext, send: impl Fn(Found)) {
    let home = (ctx.env)("HOME").filter(|h| !h.is_empty()).map(PathBuf::from);
    send(Found::Tmux(detect::tmux_row(ctx.tmux.as_ref(), home.as_deref())));
    let ims = judge::im_evidence(&*ctx.env, &(ctx.bus_owners)());
    let locale = ["LC_ALL", "LC_MESSAGES", "LANG"]
        .iter()
        .find_map(|name| (ctx.env)(name).filter(|v| !v.is_empty()))
        .map(|v| v.to_string_lossy().into_owned())
        .unwrap_or_default();
    send(Found::Input(detect::input_row(&ims, &locale)));
    let Some(row) = desktop(&ctx) else {
        return;
    };
    let tiling = windows(&row, ctx.build);
    send(Found::Desktop(row));
    send(Found::Windows(tiling));
}

/// What `window.tiling "auto"` would pick: the compositor's own windows under a live tiling compositor in a build
/// that has compositor mode, else nothing.
fn windows(row: &DesktopRow, build: Build) -> Option<Tiling> {
    let tiling = row.live && matches!(row.wm, Some("Hyprland" | "sway" | "niri"));
    (build.compositor_mode && tiling).then_some(Tiling::Compositor)
}

/// Whether `why` says the command was still running when its time was up, as opposed to not starting or failing.
fn never_answered(why: &str) -> bool {
    why.contains("in time")
}

/// The desktop row, or `None` when the compositor's command never answered.
fn desktop(ctx: &DetectContext) -> Option<DesktopRow> {
    let env = &*ctx.env;
    let name = detect::wm_name(env);
    let wm = eitri_core::compositor::detect(env);
    match name {
        Some(name @ ("sway" | "i3")) => {
            let argv: &[&str] = if name == "sway" {
                &["swaymsg", "-r", "-t", "get_config"]
            } else {
                &["i3-msg", "-t", "get_config"]
            };
            let answer = match (ctx.run)(argv) {
                Ok(text) => Some(text),
                Err(why) if never_answered(&why) => return None,
                Err(_) => None,
            };
            let live = answer.is_some();
            let json = answer.map(|text| {
                if name == "sway" {
                    text
                } else {
                    // i3 answers the config as plain text; the chord reader takes sway's reply shape.
                    serde_json::json!({ "config": text }).to_string()
                }
            });
            // What the running compositor loaded says what `$mod` is first; its config file is read when it does not
            // (no answer, or the `set` lives in a file the main one includes, which the answer does not expand).
            let from_answer = json
                .as_deref()
                .and_then(|json| serde_json::from_str::<serde_json::Value>(json).ok())
                .and_then(|value| value.get("config").and_then(|c| c.as_str()).and_then(detect::sway_mod));
            let binds: Vec<Chord> = json
                .as_deref()
                .and_then(|json| judge::sway_chords(json).ok())
                .map(|chords| chords.into_iter().map(|(chord, _)| chord).collect())
                .unwrap_or_default();
            let wm_mod = from_answer.or_else(|| {
                detect::wm_config_paths(name, env)
                    .iter()
                    .find_map(|path| detect::read_config(path, &*ctx.read))
                    .and_then(|text| detect::sway_mod(&text))
            });
            let mut row = detect::desktop_row(Wm::Sway, live, wm_mod, binds, false);
            row.wm = Some(if name == "sway" { "sway" } else { "i3" });
            Some(row)
        }
        Some("Hyprland") => {
            let answer = match (ctx.run)(&["hyprctl", "binds", "-j"]) {
                Ok(text) => Some(text),
                Err(why) if never_answered(&why) => return None,
                Err(_) => None,
            };
            let binds: Vec<Chord> = answer
                .as_deref()
                .and_then(|json| judge::hyprland_chords(json).ok())
                .map(|chords| chords.into_iter().map(|(chord, _)| chord).collect())
                .unwrap_or_default();
            let dir = hypr_dir(env);
            let wm_mod = dir
                .as_ref()
                .and_then(|dir| detect::read_config(&dir.join("hyprland.conf"), &*ctx.read))
                .and_then(|text| detect::hypr_main_mod(&text));
            let lua_config = dir.is_some_and(|dir| (ctx.read)(&dir.join("hyprland.lua")).is_some());
            Some(detect::desktop_row(
                Wm::Hyprland,
                answer.is_some(),
                wm_mod,
                binds,
                lua_config,
            ))
        }
        // niri's binds are not read; its socket in the environment is the whole of what is known.
        Some("niri") => Some(detect::desktop_row(Wm::Niri, true, None, Vec::new(), false)),
        _ => Some(detect::desktop_row(wm, false, None, Vec::new(), false)),
    }
}

/// Hyprland's configuration directory: `$XDG_CONFIG_HOME/hypr` when that is absolute, else `~/.config/hypr`.
fn hypr_dir(env: &dyn Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    let config_home = env("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| {
            env("HOME")
                .filter(|h| !h.is_empty())
                .map(|h| PathBuf::from(h).join(".config"))
        })?;
    Some(config_home.join("hypr"))
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::time::Instant;

    use eitri_core::keymap::KeySpec;

    use super::*;

    fn fake(pairs: &'static [(&'static str, &'static str)], run: Run) -> DetectContext {
        DetectContext {
            env: Box::new(move |name| pairs.iter().find(|(k, _)| *k == name).map(|(_, v)| OsString::from(*v))),
            run,
            bus_owners: Box::new(Vec::new),
            read: Box::new(|_| None),
            tmux: None,
            build: Build::THIS_BINARY,
        }
    }

    fn collect(ctx: DetectContext) -> Vec<Found> {
        let found = RefCell::new(Vec::new());
        run(ctx, |row| found.borrow_mut().push(row));
        found.into_inner()
    }

    #[test]
    fn the_detect_worker_sends_each_row_once() {
        let mut ctx = fake(
            &[
                ("HOME", "/home/u"),
                ("SWAYSOCK", "/run/sway.sock"),
                ("LANG", "en_US.UTF-8"),
            ],
            Box::new(|_| Err("swaymsg could not start (NotFound)".to_string())),
        );
        ctx.tmux = Some(TmuxImport {
            files: vec![PathBuf::from("/home/u/.tmux.conf")],
            prefix: KeySpec::parse("C-a").ok(),
            ..TmuxImport::default()
        });
        let rows = collect(ctx);
        let kinds: Vec<&str> = rows
            .iter()
            .map(|row| match row {
                Found::Tmux(_) => "tmux",
                Found::Desktop(_) => "desktop",
                Found::Windows(_) => "windows",
                Found::Neovim(_) => "neovim",
                Found::Input(_) => "input",
                Found::TrustOpen(_) => "trust",
            })
            .collect();
        assert_eq!(kinds, ["tmux", "input", "desktop", "windows"], "{rows:?}");
        let Found::Tmux(tmux) = &rows[0] else { unreachable!() };
        assert_eq!(tmux.path.as_deref(), Some("~/.tmux.conf"));
        assert_eq!(tmux.prefix, KeySpec::parse("C-a").ok());
        let Found::Desktop(desktop) = &rows[2] else {
            unreachable!()
        };
        assert_eq!(desktop.wm, Some("sway"), "named by its socket");
        assert!(!desktop.live, "sway is not there to answer");
        assert_eq!(rows[3], Found::Windows(None), "no compositor mode in this binary");
    }

    fn sway_row(answer: &'static str, file: Option<&'static str>) -> DesktopRow {
        let mut ctx = fake(
            &[("HOME", "/home/u"), ("SWAYSOCK", "/run/sway.sock")],
            Box::new(move |argv| {
                assert_eq!(argv, ["swaymsg", "-r", "-t", "get_config"]);
                if answer.is_empty() {
                    return Err("swaymsg could not start (NotFound)".to_string());
                }
                Ok(serde_json::json!({ "config": answer }).to_string())
            }),
        );
        ctx.read = Box::new(move |path| {
            (path == Path::new("/home/u/.config/sway/config"))
                .then(|| file.map(str::to_string))
                .flatten()
        });
        let rows = collect(ctx);
        let Some(Found::Desktop(desktop)) = rows.into_iter().find(|r| matches!(r, Found::Desktop(_))) else {
            panic!("no desktop row")
        };
        desktop
    }

    #[test]
    fn a_sway_that_answers_gives_its_binds_and_its_mod_from_its_own_answer() {
        let desktop = sway_row("set $mod Mod4\nbindsym Ctrl+a exec foo\n", Some("set $mod Mod1\n"));
        assert!(desktop.live);
        assert_eq!(
            desktop.wm_mod,
            Some(eitri_core::onboarding::WmMod::Super),
            "what the running sway loaded decides, whatever the default path holds"
        );
        assert_eq!(desktop.binds.len(), 1, "{:?}", desktop.binds);
    }

    #[test]
    fn the_mod_comes_from_the_file_when_the_answer_has_none() {
        // The `set` is in an included file, which the running sway's answer does not expand.
        let desktop = sway_row("bindsym Ctrl+a exec foo\n", Some("set $mod Mod1\n"));
        assert_eq!(desktop.wm_mod, Some(eitri_core::onboarding::WmMod::Alt));
        // No answer at all: the file is all there is.
        let desktop = sway_row("", Some("set $mod Mod1\n"));
        assert!(!desktop.live);
        assert_eq!(desktop.wm_mod, Some(eitri_core::onboarding::WmMod::Alt));
    }

    #[test]
    fn a_command_that_never_answers_leaves_its_row_looking() {
        let ctx = fake(
            &[("SWAYSOCK", "/run/sway.sock")],
            Box::new(|_| crate::root_conflicts::run_capped(&["sleep", "30"], Duration::from_millis(200), 1024)),
        );
        let started = Instant::now();
        let rows = collect(ctx);
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the worker ended after {:?}",
            started.elapsed()
        );
        assert!(
            !rows
                .iter()
                .any(|row| matches!(row, Found::Desktop(_) | Found::Windows(_))),
            "{rows:?}"
        );
        assert!(
            rows.iter().any(|row| matches!(row, Found::Tmux(_))),
            "the other rows still come"
        );
        assert!(rows.iter().any(|row| matches!(row, Found::Input(_))));
    }

    #[test]
    fn no_compositor_is_a_row_that_says_so() {
        let rows = collect(fake(&[], Box::new(|_| panic!("no command runs without a compositor"))));
        let Some(Found::Desktop(desktop)) = rows.iter().find(|r| matches!(r, Found::Desktop(_))) else {
            panic!("{rows:?}")
        };
        assert_eq!(desktop.wm, None);
        assert!(!desktop.live);
    }

    #[test]
    fn a_config_that_is_not_a_regular_file_is_not_read() {
        assert_eq!(read_regular_file(Path::new("/nonexistent/eitri-detect")), None);
        assert_eq!(read_regular_file(Path::new("/")), None, "a directory");
    }

    #[test]
    fn a_fifo_with_no_writer_is_refused_and_does_not_hold_the_reader() {
        use std::os::unix::ffi::OsStrExt;
        let dir = eitri_core::plugin::test_support::Scratch::new("onboarding-detect-fifo");
        let fifo = dir.0.join("config");
        let c_path = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
        let regular = dir.0.join("regular");
        std::fs::write(&regular, "set $mod Mod4\n").unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        // A reader that blocked would never answer: the test gives it a second, on a thread of its own, so a failure
        // is a failed assertion and not a hung suite.
        let path = fifo.clone();
        std::thread::spawn(move || {
            let _ = tx.send(read_regular_file(&path));
        });
        assert_eq!(rx.recv_timeout(Duration::from_secs(5)), Ok(None));
        assert_eq!(read_regular_file(&regular).as_deref(), Some("set $mod Mod4\n"));
    }
}
