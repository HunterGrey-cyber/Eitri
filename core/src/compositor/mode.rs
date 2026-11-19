//! Whether a project's chats and pages open as separate compositor windows ("compositor" mode) or inside the
//! one Eitri window ("eitri" mode), decided from facts the shell gathers. Nothing here runs a command or touches
//! the desktop: the shell runs [`liveness_argv`] off the GTK thread and hands the reply to [`liveness_ok`].

use super::Wm;
use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The `init.lua` key that picks the mode.
pub const TILING_KEY: &str = "window.tiling";

/// How long the liveness command may take before the compositor counts as not answering.
pub const LIVENESS_WITHIN: Duration = Duration::from_millis(500);

/// What `window.tiling` says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TilingChoice {
    /// Compositor mode on a tiling compositor that answers, Eitri mode anywhere else.
    Auto,
    /// Everything stays inside the Eitri window.
    Eitri,
    /// Chats and pages are windows of the compositor, on any Wayland desktop.
    Compositor,
}

impl TilingChoice {
    /// The value as `init.lua` writes it.
    pub fn name(self) -> &'static str {
        match self {
            TilingChoice::Auto => "auto",
            TilingChoice::Eitri => "eitri",
            TilingChoice::Compositor => "compositor",
        }
    }
}

/// Reads the value of [`TILING_KEY`]; unset is `auto`. The error names the key and the values it takes.
pub fn parse_tiling(value: Option<&str>) -> Result<TilingChoice, String> {
    match value {
        None | Some("auto") => Ok(TilingChoice::Auto),
        Some("eitri") => Ok(TilingChoice::Eitri),
        Some("compositor") => Ok(TilingChoice::Compositor),
        Some(raw) => Err(format!(
            "eitri.config.set(\"{TILING_KEY}\", {raw:?}): expected \"auto\", \"eitri\" or \"compositor\""
        )),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Eitri,
    Compositor,
}

/// The rule that decided the mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Why {
    /// `window.tiling`, or `auto` on a tiling compositor that answered.
    Setting,
    /// `auto` and the desktop is not Hyprland, sway or niri.
    NoTilingCompositor,
    /// `auto` and a tiling compositor is named but did not answer.
    NotLive,
    NotWayland,
    MacOs,
    /// The project's page lives in a window of its own.
    OwnWindow,
    /// Window 2 of a pair was left behind without its partner.
    Window2Orphan,
    /// A full window on a project whose group another window holds.
    Unpaired,
    /// No pair lock: no module window could link to this window.
    Alone,
    /// Module windows from an earlier run are waiting to be linked again.
    ModuleOrphans,
}

/// Everything [`resolve_mode`] reads.
#[derive(Debug, Clone, Copy)]
pub struct ModeFacts {
    pub choice: TilingChoice,
    pub mac: bool,
    pub wayland: bool,
    pub wm: Wm,
    /// Whether the compositor answered its liveness command; `None` when it was not asked or there was no socket.
    pub live: Option<bool>,
    pub own_window: bool,
    pub window2_orphan: bool,
    pub module_orphans: bool,
    pub unpaired: bool,
    pub alone: bool,
}

/// The mode and the first rule that decided it.
pub fn resolve_mode(f: &ModeFacts) -> (Mode, Why) {
    if f.mac {
        return (Mode::Eitri, Why::MacOs);
    }
    if f.window2_orphan {
        return (Mode::Eitri, Why::Window2Orphan);
    }
    if f.unpaired {
        return (Mode::Eitri, Why::Unpaired);
    }
    if f.alone {
        return (Mode::Eitri, Why::Alone);
    }
    if f.own_window {
        return (Mode::Eitri, Why::OwnWindow);
    }
    if f.module_orphans {
        return (Mode::Compositor, Why::ModuleOrphans);
    }
    if !f.wayland {
        return (Mode::Eitri, Why::NotWayland);
    }
    match f.choice {
        TilingChoice::Eitri => (Mode::Eitri, Why::Setting),
        TilingChoice::Compositor => (Mode::Compositor, Why::Setting),
        TilingChoice::Auto => match f.wm {
            Wm::Hyprland | Wm::Sway | Wm::Niri if f.live == Some(true) => (Mode::Compositor, Why::Setting),
            Wm::Hyprland | Wm::Sway | Wm::Niri => (Mode::Eitri, Why::NotLive),
            Wm::Gnome | Wm::None => (Mode::Eitri, Why::NoTilingCompositor),
        },
    }
}

/// Whether this is a Wayland session: its display is named, or the session says so.
pub fn wayland_session(env: &dyn Fn(&str) -> Option<OsString>) -> bool {
    env("WAYLAND_DISPLAY").is_some_and(|display| !display.is_empty())
        || env("XDG_SESSION_TYPE").is_some_and(|kind| kind == "wayland")
}

/// Whether the compositor's socket is there to be asked: the environment names it and the path exists. A stale
/// Hyprland signature (the instance is gone) names a directory that has no socket, so it is absent.
pub fn socket_present(wm: Wm, env: &dyn Fn(&str) -> Option<OsString>, exists: &dyn Fn(&Path) -> bool) -> bool {
    let value = |name: &str| env(name).filter(|v| !v.is_empty());
    match wm {
        Wm::Hyprland => {
            let (Some(runtime), Some(signature)) = (value("XDG_RUNTIME_DIR"), value("HYPRLAND_INSTANCE_SIGNATURE"))
            else {
                return false;
            };
            let signature = signature.to_string_lossy().into_owned();
            if signature.contains('/') || signature == "." || signature == ".." {
                return false;
            }
            let dir = Path::new(&runtime).join("hypr").join(signature);
            // A path too long to bind cannot be a live socket.
            agent::socket_path::in_dir(&dir, ".socket.sock").is_ok_and(|path| exists(&path))
        }
        Wm::Sway => value("SWAYSOCK").is_some_and(|path| exists(Path::new(&path))),
        Wm::Niri => value("NIRI_SOCKET").is_some_and(|path| exists(Path::new(&path))),
        Wm::Gnome | Wm::None => false,
    }
}

/// The command that asks the compositor for its version, which only a live one answers; `None` for a desktop
/// with nothing to ask.
pub fn liveness_argv(wm: Wm) -> Option<Vec<String>> {
    let parts: &[&str] = match wm {
        Wm::Hyprland => &["hyprctl", "version", "-j"],
        Wm::Sway => &["swaymsg", "-t", "get_version"],
        Wm::Niri => &["niri", "msg", "--json", "version"],
        Wm::Gnome | Wm::None => return None,
    };
    Some(parts.iter().map(ToString::to_string).collect())
}

/// Whether `stdout` is that compositor's version reply: JSON of the shape its version command prints.
pub fn liveness_ok(wm: Wm, stdout: &str) -> bool {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(stdout.trim()) else {
        return false;
    };
    let text = |v: Option<&serde_json::Value>| v.and_then(|v| v.as_str()).is_some_and(|s| !s.is_empty());
    match wm {
        Wm::Hyprland => text(value.get("version")) || text(value.get("tag")),
        Wm::Sway => value.get("major").is_some_and(|v| v.is_u64()) || text(value.get("human_readable")),
        // The running compositor's version, printed beside the CLI's own, which needs no compositor to be known.
        Wm::Niri => text(value.get("compositor")),
        Wm::Gnome | Wm::None => false,
    }
}

/// What the last run of a project did, kept so a change of mode is said once.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ModeState {
    /// The mode the last run opened in.
    pub last: Option<Mode>,
    /// Whether the first-start toast was shown.
    pub toasted: bool,
}

/// The most a mode-state file can hold.
const STATE_FILE_LIMIT: u64 = 4096;

fn state_file(dir: &Path, root: &Path) -> PathBuf {
    dir.join(format!("{}.cm-mode.json", crate::state_paths::project_key(root)))
}

/// The project's mode state from `dir` (the layout directory); a missing, unreadable or malformed file reads as
/// the default, which says nothing was recorded.
pub fn read_mode_state(dir: &Path, root: &Path) -> ModeState {
    agent::private_fs::read_private_to_string_capped(&state_file(dir, root), STATE_FILE_LIMIT)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// Records `state` for the project: written 0600 beside the file and renamed over it.
pub fn write_mode_state(dir: &Path, root: &Path, state: &ModeState) -> io::Result<()> {
    agent::private_fs::create_private_dir_all(dir, dir)?;
    let file = state_file(dir, root);
    let text = serde_json::to_string(state).expect("a plain struct serializes");
    let tmp = crate::state_paths::temporary(&file);
    agent::private_fs::replace_private(&tmp, &file, text.as_bytes(), agent::private_fs::Flush::Nothing)
}

/// What the compositor is called in a notice.
pub fn compositor_name(wm: Wm) -> &'static str {
    match wm {
        Wm::Hyprland => "Hyprland",
        Wm::Sway => "sway",
        Wm::Niri => "niri",
        Wm::Gnome => "GNOME",
        Wm::None => "the compositor",
    }
}

/// Why a window opened in this mode, as a clause that follows a colon.
pub fn reason(why: Why) -> &'static str {
    match why {
        Why::Setting => "window.tiling = \"eitri\"",
        Why::NoTilingCompositor => "this desktop is not Hyprland, sway or niri (window.tiling = \"auto\")",
        Why::NotLive => "the compositor did not answer (window.tiling = \"auto\")",
        Why::NotWayland => "this is not a Wayland session",
        Why::MacOs => "macOS has no compositor mode",
        Why::OwnWindow => "this project's page has a window of its own",
        Why::Window2Orphan => "window 2 was left without its partner",
        Why::Unpaired => "this project is open in another Eitri window",
        Why::Alone => "this window has no pair lock for module windows to link to",
        Why::ModuleOrphans => "chat windows were waiting to be relinked",
    }
}

/// The line that says the mode changed since the last run, once; `None` when it did not, or there was no last run.
/// `choice` is what `window.tiling` says, the setting a change to compositor mode is put down to.
pub fn flip_note(prev: Option<Mode>, now: Mode, why: Why, wm: Wm, choice: TilingChoice) -> Option<String> {
    let prev = prev?;
    if prev == now {
        return None;
    }
    Some(match now {
        Mode::Eitri => match why {
            Why::NotLive => format!(
                "opened as tiles this time: {} did not answer (window.tiling = \"auto\")",
                compositor_name(wm)
            ),
            other => format!("opened as tiles this time: {}", reason(other)),
        },
        Mode::Compositor => match why {
            Why::ModuleOrphans => {
                "opened as separate windows this time: chat windows were waiting to be relinked".to_string()
            }
            _ => format!(
                "opened as separate windows this time (window.tiling = \"{}\")",
                choice.name()
            ),
        },
    })
}

/// The toast on the first start in compositor mode.
pub fn first_start_toast(wm: Wm) -> String {
    format!(
        "{} detected: chats and pages open as their own windows; window.tiling = \"eitri\" keeps them inside",
        compositor_name(wm)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compositor::detect;

    fn facts() -> ModeFacts {
        ModeFacts {
            choice: TilingChoice::Auto,
            mac: false,
            wayland: true,
            wm: Wm::Hyprland,
            live: Some(true),
            own_window: false,
            window2_orphan: false,
            module_orphans: false,
            unpaired: false,
            alone: false,
        }
    }

    fn env_of<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<OsString> + 'a {
        move |name| pairs.iter().find(|(k, _)| *k == name).map(|(_, v)| OsString::from(v))
    }

    #[test]
    fn a_wayland_session_is_named_by_its_display_or_its_session_type() {
        assert!(wayland_session(&env_of(&[("WAYLAND_DISPLAY", "wayland-1")])));
        assert!(wayland_session(&env_of(&[("XDG_SESSION_TYPE", "wayland")])));
        assert!(!wayland_session(&env_of(&[("XDG_SESSION_TYPE", "x11")])));
        assert!(!wayland_session(&env_of(&[("WAYLAND_DISPLAY", "")])));
        assert!(!wayland_session(&env_of(&[])));
    }

    #[test]
    fn every_mode_case_resolves_as_the_table_says() {
        use Mode::{Compositor, Eitri};
        let forced = [TilingChoice::Auto, TilingChoice::Compositor];
        for choice in forced {
            let with = |edit: fn(&mut ModeFacts)| {
                let mut f = ModeFacts { choice, ..facts() };
                edit(&mut f);
                resolve_mode(&f)
            };
            assert_eq!(with(|f| f.mac = true), (Eitri, Why::MacOs));
            assert_eq!(with(|f| f.window2_orphan = true), (Eitri, Why::Window2Orphan));
            assert_eq!(with(|f| f.unpaired = true), (Eitri, Why::Unpaired));
            assert_eq!(with(|f| f.alone = true), (Eitri, Why::Alone));
            assert_eq!(with(|f| f.own_window = true), (Eitri, Why::OwnWindow));
            assert_eq!(with(|f| f.module_orphans = true), (Compositor, Why::ModuleOrphans));
            assert_eq!(with(|f| f.wayland = false), (Eitri, Why::NotWayland));
        }
        // An earlier rule wins over a later one.
        let mut f = facts();
        f.mac = true;
        f.window2_orphan = true;
        f.module_orphans = true;
        assert_eq!(resolve_mode(&f), (Eitri, Why::MacOs));
        f.mac = false;
        assert_eq!(resolve_mode(&f), (Eitri, Why::Window2Orphan));
        f.window2_orphan = false;
        f.own_window = true;
        assert_eq!(resolve_mode(&f), (Eitri, Why::OwnWindow));
        // Module orphans beat a non-Wayland session and the setting.
        let orphans = ModeFacts {
            choice: TilingChoice::Eitri,
            wayland: false,
            module_orphans: true,
            ..facts()
        };
        assert_eq!(resolve_mode(&orphans), (Compositor, Why::ModuleOrphans));
        // The setting.
        let with_choice = |choice| resolve_mode(&ModeFacts { choice, ..facts() });
        assert_eq!(with_choice(TilingChoice::Eitri), (Eitri, Why::Setting));
        assert_eq!(with_choice(TilingChoice::Compositor), (Compositor, Why::Setting));
        // Forced compositor mode takes any Wayland desktop.
        for wm in [Wm::Gnome, Wm::None] {
            let f = ModeFacts {
                choice: TilingChoice::Compositor,
                wm,
                live: None,
                ..facts()
            };
            assert_eq!(resolve_mode(&f), (Compositor, Why::Setting));
        }
        // Auto.
        for wm in [Wm::Hyprland, Wm::Sway, Wm::Niri] {
            let auto = |live| resolve_mode(&ModeFacts { wm, live, ..facts() });
            assert_eq!(auto(Some(true)), (Compositor, Why::Setting));
            assert_eq!(auto(Some(false)), (Eitri, Why::NotLive));
            assert_eq!(auto(None), (Eitri, Why::NotLive));
        }
        for wm in [Wm::Gnome, Wm::None] {
            assert_eq!(
                resolve_mode(&ModeFacts {
                    wm,
                    live: None,
                    ..facts()
                }),
                (Eitri, Why::NoTilingCompositor)
            );
        }
    }

    #[test]
    fn a_stale_hyprland_signature_resolves_to_eitri() {
        let env = env_of(&[
            ("XDG_RUNTIME_DIR", "/run/user/1000"),
            ("HYPRLAND_INSTANCE_SIGNATURE", "abc_123"),
        ]);
        let dir = Path::new("/run/user/1000/hypr/abc_123");
        let in_dir = |p: &Path| {
            p.parent() == Some(dir)
                && p.file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with(".socket"))
        };
        assert!(socket_present(Wm::Hyprland, &env, &in_dir));
        assert!(!socket_present(Wm::Hyprland, &env, &|_| false));
        for bad in ["", "../x", "a/b", "..", "."] {
            let pairs = [
                ("XDG_RUNTIME_DIR", "/run/user/1000"),
                ("HYPRLAND_INSTANCE_SIGNATURE", bad),
            ];
            let env = env_of(&pairs);
            assert!(!socket_present(Wm::Hyprland, &env, &|_| true), "{bad:?}");
        }
        // A path too long to bind cannot be a live socket.
        let long = "x".repeat(100);
        let pairs = [
            ("XDG_RUNTIME_DIR", "/run/user/1000"),
            ("HYPRLAND_INSTANCE_SIGNATURE", long.as_str()),
        ];
        assert!(!socket_present(Wm::Hyprland, &env_of(&pairs), &|_| true));
        let no_runtime = env_of(&[("HYPRLAND_INSTANCE_SIGNATURE", "abc")]);
        assert!(!socket_present(Wm::Hyprland, &no_runtime, &|_| true));
        // No socket means nothing was asked, so liveness is unknown and auto stays in Eitri mode.
        let f = ModeFacts { live: None, ..facts() };
        assert_eq!(resolve_mode(&f), (Mode::Eitri, Why::NotLive));
    }

    #[test]
    fn socket_present_reads_the_sway_and_niri_paths() {
        let sway = env_of(&[("SWAYSOCK", "/run/user/1000/sway-ipc")]);
        assert!(socket_present(Wm::Sway, &sway, &|p| p == Path::new("/run/user/1000/sway-ipc")));
        assert!(!socket_present(Wm::Sway, &sway, &|_| false));
        assert!(!socket_present(Wm::Sway, &env_of(&[("SWAYSOCK", "")]), &|_| true));
        let niri = env_of(&[("NIRI_SOCKET", "/run/user/1000/niri-ipc")]);
        assert!(socket_present(Wm::Niri, &niri, &|_| true));
        assert!(!socket_present(Wm::Niri, &env_of(&[]), &|_| true));
        assert!(!socket_present(Wm::Gnome, &niri, &|_| true));
        assert!(!socket_present(Wm::None, &niri, &|_| true));
    }

    #[test]
    fn sway_beside_i3sock_is_sway() {
        let env = env_of(&[
            ("SWAYSOCK", "/run/user/1000/sway-ipc"),
            ("I3SOCK", "/run/user/1000/sway-ipc"),
        ]);
        assert_eq!(detect(&env), Wm::Sway);
    }

    #[test]
    fn x11_never_picks_compositor_mode() {
        let env = env_of(&[
            ("XDG_SESSION_TYPE", "x11"),
            ("XDG_CURRENT_DESKTOP", "GNOME"),
            ("DISPLAY", ":0"),
        ]);
        let wm = detect(&env);
        assert_eq!(wm, Wm::None);
        for choice in [TilingChoice::Auto, TilingChoice::Eitri, TilingChoice::Compositor] {
            let f = ModeFacts {
                choice,
                wayland: false,
                wm,
                live: None,
                ..facts()
            };
            assert_eq!(resolve_mode(&f).0, Mode::Eitri);
        }
    }

    #[test]
    fn parse_tiling_names_the_key_and_its_values() {
        assert_eq!(parse_tiling(None), Ok(TilingChoice::Auto));
        assert_eq!(parse_tiling(Some("auto")), Ok(TilingChoice::Auto));
        assert_eq!(parse_tiling(Some("eitri")), Ok(TilingChoice::Eitri));
        assert_eq!(parse_tiling(Some("compositor")), Ok(TilingChoice::Compositor));
        let err = parse_tiling(Some("tiles")).unwrap_err();
        for needle in ["window.tiling", "\"auto\"", "\"eitri\"", "\"compositor\"", "tiles"] {
            assert!(err.contains(needle), "{err} lacks {needle}");
        }
        assert_eq!(TILING_KEY, "window.tiling");
    }

    #[test]
    fn the_mode_state_round_trips_and_a_bad_file_reads_as_none() {
        let tmp = crate::test_scratch_dir::ScratchDir::new("nv-cm-mode", "state");
        let dir = tmp.join("layout");
        let root = Path::new("/project");
        assert_eq!(read_mode_state(&dir, root), ModeState::default());
        let state = ModeState {
            last: Some(Mode::Compositor),
            toasted: true,
        };
        write_mode_state(&dir, root, &state).unwrap();
        assert_eq!(read_mode_state(&dir, root), state);
        // Another project has its own record.
        assert_eq!(read_mode_state(&dir, Path::new("/other")), ModeState::default());
        let file = state_file(&dir, root);
        assert!(file.to_string_lossy().ends_with(".cm-mode.json"));
        crate::test_scratch_dir::write_private(&file, "{not json").unwrap();
        assert_eq!(read_mode_state(&dir, root), ModeState::default());
        crate::test_scratch_dir::write_private(&file, vec![b' '; 10_000]).unwrap();
        assert_eq!(read_mode_state(&dir, root), ModeState::default());
    }

    #[test]
    fn a_flip_says_why_once() {
        use Mode::{Compositor, Eitri};
        use TilingChoice::{Auto, Compositor as Forced, Eitri as Eitri_};
        // No change, or no last run, says nothing.
        assert_eq!(flip_note(Some(Eitri), Eitri, Why::Setting, Wm::Hyprland, Auto), None);
        assert_eq!(
            flip_note(Some(Compositor), Compositor, Why::Setting, Wm::Hyprland, Auto),
            None
        );
        assert_eq!(flip_note(None, Compositor, Why::Setting, Wm::Hyprland, Auto), None);
        assert_eq!(flip_note(None, Eitri, Why::NotLive, Wm::Hyprland, Auto), None);
        // Back to Eitri mode.
        assert_eq!(
            flip_note(Some(Compositor), Eitri, Why::NotLive, Wm::Hyprland, Auto).as_deref(),
            Some("opened as tiles this time: Hyprland did not answer (window.tiling = \"auto\")")
        );
        assert_eq!(
            flip_note(Some(Compositor), Eitri, Why::NotLive, Wm::Sway, Auto).as_deref(),
            Some("opened as tiles this time: sway did not answer (window.tiling = \"auto\")")
        );
        for why in [
            Why::Setting,
            Why::NoTilingCompositor,
            Why::NotWayland,
            Why::MacOs,
            Why::OwnWindow,
            Why::Window2Orphan,
            Why::Unpaired,
            Why::Alone,
        ] {
            let note = flip_note(Some(Compositor), Eitri, why, Wm::Hyprland, Eitri_).unwrap();
            assert_eq!(note, format!("opened as tiles this time: {}", reason(why)));
        }
        assert_eq!(
            flip_note(Some(Compositor), Eitri, Why::Unpaired, Wm::Hyprland, Auto).as_deref(),
            Some("opened as tiles this time: this project is open in another Eitri window")
        );
        // Into compositor mode.
        assert_eq!(
            flip_note(Some(Eitri), Compositor, Why::Setting, Wm::Niri, Auto).as_deref(),
            Some("opened as separate windows this time (window.tiling = \"auto\")")
        );
        assert_eq!(
            flip_note(Some(Eitri), Compositor, Why::Setting, Wm::Gnome, Forced).as_deref(),
            Some("opened as separate windows this time (window.tiling = \"compositor\")")
        );
        // The setting that was changed is the one named, on a tiling compositor too.
        assert_eq!(
            flip_note(Some(Eitri), Compositor, Why::Setting, Wm::Sway, Forced).as_deref(),
            Some("opened as separate windows this time (window.tiling = \"compositor\")")
        );
        assert_eq!(
            flip_note(Some(Eitri), Compositor, Why::ModuleOrphans, Wm::Hyprland, Auto).as_deref(),
            Some("opened as separate windows this time: chat windows were waiting to be relinked")
        );
    }

    #[test]
    fn the_first_start_toast_names_the_compositor() {
        assert_eq!(
            first_start_toast(Wm::Hyprland),
            "Hyprland detected: chats and pages open as their own windows; window.tiling = \"eitri\" keeps them inside"
        );
        assert!(first_start_toast(Wm::Sway).starts_with("sway detected:"));
        assert_eq!(compositor_name(Wm::Niri), "niri");
        assert_eq!(compositor_name(Wm::Gnome), "GNOME");
        assert_eq!(compositor_name(Wm::None), "the compositor");
    }

    #[test]
    fn liveness_argv_per_compositor() {
        let argv = |wm| liveness_argv(wm).unwrap().join(" ");
        assert_eq!(argv(Wm::Hyprland), "hyprctl version -j");
        assert_eq!(argv(Wm::Sway), "swaymsg -t get_version");
        assert_eq!(argv(Wm::Niri), "niri msg --json version");
        assert_eq!(liveness_argv(Wm::Gnome), None);
        assert_eq!(liveness_argv(Wm::None), None);
        assert_eq!(LIVENESS_WITHIN, Duration::from_millis(500));
    }

    #[test]
    fn liveness_ok_reads_each_version_reply() {
        assert!(liveness_ok(
            Wm::Hyprland,
            "{\"branch\":\"main\",\"version\":\"0.50.1\",\"tag\":\"v0.50.1\"}\n"
        ));
        assert!(!liveness_ok(Wm::Hyprland, "{\"branch\":\"main\"}"));
        assert!(!liveness_ok(Wm::Hyprland, ""));
        assert!(!liveness_ok(Wm::Hyprland, "Couldn't connect to socket"));
        assert!(liveness_ok(
            Wm::Sway,
            "{\"human_readable\":\"1.10\",\"major\":1,\"minor\":10,\"patch\":0}"
        ));
        assert!(!liveness_ok(Wm::Sway, "{\"error\":\"no\"}"));
        assert!(liveness_ok(
            Wm::Niri,
            "{\"compositor\":\"25.08 (abc)\",\"cli\":\"25.08 (abc)\"}\n"
        ));
        assert!(
            !liveness_ok(Wm::Niri, "{\"cli\":\"25.08 (abc)\"}"),
            "the CLI's version alone says nothing of a running compositor"
        );
        assert!(!liveness_ok(Wm::Niri, "{\"compositor\":\"\"}"));
        assert!(!liveness_ok(Wm::Niri, "[]"));
        assert!(!liveness_ok(Wm::Gnome, "{\"version\":\"1\"}"));
    }
}
