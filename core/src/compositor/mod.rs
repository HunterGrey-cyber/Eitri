//! Window-manager adapters for companion mode: which compositor this is, the argv that moves
//! focus or raises a window, and how to read its window list. Nothing here runs a command or
//! touches the desktop; the caller does that off the GTK thread and feeds the output back in.
//!
//! Hyprland's commands are built in `hyprland_dispatch` from typed values only, in the form its config manager
//! takes ([`HyprForm`], picked by a probe the caller runs); its replies must be read, since a failed dispatch
//! still exits 0 ([`dispatch_reply_failure`]).
//!
//! Only sway is asked whether a move would wrap (it wraps around the edge of the last window);
//! Hyprland and niri have no such check. GNOME has no command line to ask: a client there cannot
//! list or focus windows, so `Wm::Gnome` is served by a Shell extension over D-Bus (the shell crate
//! makes those calls) and every argv function here has nothing to say for it.

use crate::layout::Direction;
use std::ffi::OsString;

pub use hyprland_dispatch::{
    failure_kind, first_line, names_other_form, probe_argv as hyprland_probe_argv, HyprAddress, HyprDispatch, HyprForm,
    OutputName,
};

mod hyprland;
mod hyprland_dispatch;
pub mod mode;
mod niri;
mod sway;

/// The `init.lua` key that picks the adapter.
pub const CONFIG_KEY: &str = "companion.wm";

pub const NO_ADAPTER_NOTICE: &str = "your desktop does not let Eitri move focus; use its own window keys";

/// What a GNOME session without the Eitri Shell extension says in place of [`NO_ADAPTER_NOTICE`]:
/// the extension is what lets focus move there, so the notice names it.
pub const GNOME_NO_EXTENSION_NOTICE: &str = "your desktop does not let Eitri move focus; use its own window keys. \
Install the Eitri GNOME Shell extension to move focus with Ctrl+h/j/k/l.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wm {
    Hyprland,
    Sway,
    Niri,
    /// GNOME Shell, through the Eitri extension's D-Bus service. There is no argv for it.
    Gnome,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WmChoice {
    Auto,
    Fixed(Wm),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: i64,
    pub y: i64,
    pub w: i64,
    pub h: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WindowRef {
    pub pid: u32,
    pub id: Option<u64>,
    pub focused: bool,
    /// On screen now. A window that is not (another workspace, a hidden tab) is no neighbour for a
    /// directional move, but can still be raised.
    pub visible: bool,
    pub rect: Option<Rect>,
    pub app_id: Option<String>,
    pub title: Option<String>,
    pub workspace: Option<String>,
    pub output: Option<String>,
    pub address: Option<String>,
}

/// A different known workspace or output means the compositor placed this window deliberately. Missing facts
/// prove nothing, so an adapter that cannot report an output still compares workspaces.
pub fn placed_by_rule(before: &WindowRef, mine: &WindowRef) -> bool {
    let differs = |a: &Option<String>, b: &Option<String>| matches!((a, b), (Some(a), Some(b)) if a != b);
    differs(&before.workspace, &mine.workspace) || differs(&before.output, &mine.output)
}

pub fn parse_config(value: Option<&str>) -> Result<WmChoice, String> {
    match value {
        None | Some("auto") => Ok(WmChoice::Auto),
        Some("hyprland") => Ok(WmChoice::Fixed(Wm::Hyprland)),
        Some("sway") => Ok(WmChoice::Fixed(Wm::Sway)),
        Some("niri") => Ok(WmChoice::Fixed(Wm::Niri)),
        Some("gnome") => Ok(WmChoice::Fixed(Wm::Gnome)),
        Some("none") => Ok(WmChoice::Fixed(Wm::None)),
        Some(raw) => Err(format!(
            "eitri.config.set(\"companion.wm\", {raw:?}): expected \"auto\", \"hyprland\", \"sway\", \"niri\", \"gnome\" or \"none\""
        )),
    }
}

/// Which compositor the environment names: Hyprland, then sway, then niri, then GNOME (a
/// `:`-separated `XDG_CURRENT_DESKTOP` with a `GNOME` entry, as `ubuntu:GNOME` has) in a Wayland
/// session; an empty value is unset. The compositor-specific variables come first because a session
/// of another compositor can inherit a stale `XDG_CURRENT_DESKTOP`.
///
/// GNOME counts only under Wayland (`WAYLAND_DISPLAY` set, or `XDG_SESSION_TYPE` `wayland`): the
/// extension moves focus to windows that had fresh input, which no X11 window can show it, so on an
/// Xorg GNOME session every key would be claimed and then refused. There the usual notice is better.
pub fn detect(env: &dyn Fn(&str) -> Option<OsString>) -> Wm {
    let set = |name: &str| env(name).is_some_and(|v| !v.is_empty());
    if set("HYPRLAND_INSTANCE_SIGNATURE") {
        Wm::Hyprland
    } else if set("SWAYSOCK") {
        Wm::Sway
    } else if set("NIRI_SOCKET") {
        Wm::Niri
    } else if env("XDG_CURRENT_DESKTOP").is_some_and(|v| v.to_string_lossy().split(':').any(|part| part == "GNOME"))
        && (set("WAYLAND_DISPLAY") || env("XDG_SESSION_TYPE").is_some_and(|v| v == "wayland"))
    {
        Wm::Gnome
    } else {
        Wm::None
    }
}

pub fn resolve(choice: WmChoice, env: &dyn Fn(&str) -> Option<OsString>) -> Wm {
    match choice {
        WmChoice::Auto => detect(env),
        WmChoice::Fixed(wm) => wm,
    }
}

fn argv(parts: &[&str]) -> Option<Vec<String>> {
    Some(parts.iter().map(ToString::to_string).collect())
}

/// The `hyprctl` argv that runs `dispatches` in order, in `form`: one command as `hyprctl dispatch`, several as one
/// `--batch`. The only way a Hyprland dispatch is built.
pub fn hyprland_argv(form: HyprForm, dispatches: &[HyprDispatch]) -> Option<Vec<String>> {
    hyprland_dispatch::argv(form, dispatches)
}

/// What the compositor answered for a dispatch `argv`, when that was not success. Only Hyprland's dispatches are
/// judged: each command of one, every part of a `--batch` included, must answer `ok`, and anything else comes back
/// here as the failing part's text. `hyprctl` puts the reply on stdout and exits 0 for most failures (always for a
/// batch), so the reply is the only sign. `None` for success, and for every other compositor's commands.
pub fn dispatch_reply_failure(wm: Wm, argv: &[String], reply: &str) -> Option<String> {
    match wm {
        Wm::Hyprland => hyprland_dispatch::reply_failure(argv, reply),
        Wm::Sway | Wm::Niri | Wm::Gnome | Wm::None => None,
    }
}

/// The command that moves focus toward `dir`. `form` is read for Hyprland only.
pub fn move_focus_argv(wm: Wm, form: HyprForm, dir: Direction) -> Option<Vec<String>> {
    match wm {
        Wm::Hyprland => hyprland_argv(form, &[HyprDispatch::FocusDir(dir)]),
        Wm::Sway => {
            let d = match dir {
                Direction::Left => "left",
                Direction::Right => "right",
                Direction::Up => "up",
                Direction::Down => "down",
            };
            argv(&["swaymsg", "focus", d])
        }
        Wm::Niri => {
            let a = match dir {
                Direction::Left => "focus-column-left",
                Direction::Right => "focus-column-right",
                Direction::Up => "focus-window-up",
                Direction::Down => "focus-window-down",
            };
            argv(&["niri", "msg", "action", a])
        }
        Wm::Gnome | Wm::None => None,
    }
}

pub fn list_windows_argv(wm: Wm) -> Option<Vec<String>> {
    match wm {
        Wm::Hyprland => argv(&["hyprctl", "clients", "-j"]),
        Wm::Sway => argv(&["swaymsg", "-t", "get_tree"]),
        Wm::Niri => argv(&["niri", "msg", "--json", "windows"]),
        Wm::Gnome | Wm::None => None,
    }
}

/// Hyprland's client reply carries numeric monitor ids; this reply supplies their connector names.
pub fn list_outputs_argv(wm: Wm) -> Option<Vec<String>> {
    match wm {
        Wm::Hyprland => argv(&["hyprctl", "monitors", "-j"]),
        _ => None,
    }
}

/// The command that focuses `target`. niri needs the window id; without one there is no command. `form` is read
/// for Hyprland only.
pub fn focus_window_argv(wm: Wm, form: HyprForm, target: &WindowRef) -> Option<Vec<String>> {
    match wm {
        Wm::Hyprland => hyprland_argv(form, &[HyprDispatch::FocusPid(target.pid)]),
        // One argv element: swaymsg joins its arguments itself, and the criteria must lead the command.
        Wm::Sway => Some(vec!["swaymsg".to_string(), format!("[pid={}] focus", target.pid)]),
        Wm::Niri => target.id.map(|id| {
            ["niri", "msg", "action", "focus-window", "--id"]
                .iter()
                .map(ToString::to_string)
                .chain(std::iter::once(id.to_string()))
                .collect()
        }),
        Wm::Gnome | Wm::None => None,
    }
}

/// Output names as compositors give them (`DP-1`, `HDMI-A-2`, `HEADLESS-2`, `eDP-1`): letters, digits, `-`, `_`
/// and `.`. Anything else never reaches a command line.
pub fn valid_output_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// The command that moves `target`'s window to the output `output` (GDK's connector name, which is the
/// compositor's own output name). niri needs the window id. Hyprland's Lua `move` names the window, so it is one
/// command; its legacy `movewindow` acts on the focused window, so that batch focuses the window by pid first.
/// sway's move alone leaves focus on the previous output. `form` is read for Hyprland only.
pub fn move_to_output_argv(wm: Wm, form: HyprForm, target: &WindowRef, output: &str) -> Option<Vec<String>> {
    let output = OutputName::new(output)?;
    match wm {
        Wm::Sway => Some(vec![
            "swaymsg".to_string(),
            format!("[pid={}] move container to output \"{}\"", target.pid, output.as_str()),
        ]),
        Wm::Hyprland => hyprland_argv(
            form,
            &[HyprDispatch::MoveToOutput {
                pid: target.pid,
                output,
            }],
        ),
        Wm::Niri => target.id.map(|id| {
            ["niri", "msg", "action", "move-window-to-monitor", "--id"]
                .iter()
                .map(ToString::to_string)
                .chain([id.to_string(), output.as_str().to_string()])
                .collect()
        }),
        Wm::Gnome | Wm::None => None,
    }
}

/// Places a mapped window and leaves the keys with the window its launch role names. The focus follows the move
/// in the same command batch, so a restore cannot race a separate worker returning the keys to window 1.
/// `form` is read for Hyprland only.
pub fn placement_argv(
    wm: Wm,
    form: HyprForm,
    target: &WindowRef,
    output: Option<&str>,
    role: crate::wm::PlacementRole,
) -> Option<Vec<String>> {
    use crate::wm::PlacementRole;
    let focus_pid = match role {
        PlacementRole::NewSecond | PlacementRole::NewModule => Some(target.pid),
        PlacementRole::RestoredSecond { first_pid } => Some(first_pid),
        PlacementRole::Main => None,
    }
    .filter(|_| matches!(wm, Wm::Sway | Wm::Hyprland));
    let focus = focus_pid.and_then(|pid| focus_window_argv(wm, form, &WindowRef { pid, ..target.clone() }));
    let Some(output) = output else { return focus };
    if wm == Wm::Hyprland {
        let output = OutputName::new(output)?;
        let mut dispatches = vec![HyprDispatch::MoveToOutput {
            pid: target.pid,
            output,
        }];
        dispatches.extend(focus_pid.map(HyprDispatch::FocusPid));
        return hyprland_argv(form, &dispatches);
    }
    let mut command = move_to_output_argv(wm, form, target, output)?;
    if let (Wm::Sway, Some(focus)) = (wm, focus) {
        command[1].push_str(&format!("; {}", focus[1]));
    }
    Some(command)
}

pub fn parse_windows(wm: Wm, json: &str) -> Result<Vec<WindowRef>, String> {
    match wm {
        Wm::Hyprland => hyprland::parse_windows(json),
        Wm::Sway => sway::parse_windows(json),
        Wm::Niri => niri::parse_windows(json),
        Wm::Gnome => Err("GNOME has no window list a client can read".to_string()),
        Wm::None => Err("no window-manager adapter".to_string()),
    }
}

/// Whether another visible window lies past the focused window's edge in `dir` and shares part of
/// its span on the other axis. `None` when no focused window with a rectangle is known.
pub fn has_neighbour(windows: &[WindowRef], dir: Direction) -> Option<bool> {
    let (fi, f) = windows
        .iter()
        .enumerate()
        .find_map(|(i, w)| w.focused.then_some(w.rect.map(|r| (i, r)))?)?;
    let overlap = |a0: i64, a1: i64, b0: i64, b1: i64| a0 < b1 && b0 < a1;
    Some(
        windows
            .iter()
            .enumerate()
            .filter(|(i, w)| *i != fi && w.visible)
            .filter_map(|(_, w)| w.rect)
            .any(|r| match dir {
                Direction::Left => r.x + r.w <= f.x && overlap(f.y, f.y + f.h, r.y, r.y + r.h),
                Direction::Right => r.x >= f.x + f.w && overlap(f.y, f.y + f.h, r.y, r.y + r.h),
                Direction::Up => r.y + r.h <= f.y && overlap(f.x, f.x + f.w, r.x, r.x + r.w),
                Direction::Down => r.y >= f.y + f.h && overlap(f.x, f.x + f.w, r.x, r.x + r.w),
            }),
    )
}

/// Whether a window on screen overlaps the focused one, as floats do. Rectangles then show no edge: such a window
/// can lie in a direction [`has_neighbour`] cannot see.
pub fn focused_overlaps(windows: &[WindowRef]) -> bool {
    let Some((fi, f)) = windows
        .iter()
        .enumerate()
        .find_map(|(i, w)| w.focused.then_some(w.rect.map(|r| (i, r)))?)
    else {
        return false;
    };
    windows
        .iter()
        .enumerate()
        .filter(|(i, w)| *i != fi && w.visible)
        .filter_map(|(_, w)| w.rect)
        .any(|r| r.x < f.x + f.w && f.x < r.x + r.w && r.y < f.y + f.h && f.y < r.y + r.h)
}

/// The window owned by the nearest process in `chain`. When that process has several windows, the
/// focused one, else the first.
pub fn first_window_owner<'a>(chain: &[u32], windows: &'a [WindowRef]) -> Option<&'a WindowRef> {
    chain.iter().find_map(|pid| {
        let mut own = windows.iter().filter(|w| w.pid == *pid);
        let first = own.next()?;
        if first.focused {
            return Some(first);
        }
        Some(own.find(|w| w.focused).unwrap_or(first))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_known_change_of_workspace_or_output_counts_as_a_placement_rule() {
        let before = WindowRef {
            workspace: Some("1".into()),
            output: Some("DP-1".into()),
            ..Default::default()
        };
        assert!(!placed_by_rule(&before, &before));
        assert!(!placed_by_rule(&before, &WindowRef::default()));
        assert!(placed_by_rule(
            &before,
            &WindowRef {
                workspace: Some("2".into()),
                ..before.clone()
            }
        ));
        assert!(placed_by_rule(
            &before,
            &WindowRef {
                output: Some("DP-2".into()),
                ..before.clone()
            }
        ));
        assert!(!placed_by_rule(&WindowRef::default(), &before));
    }

    #[test]
    fn parse_windows_keeps_class_title_address_and_workspace() {
        for version in ["0.56.2", "0.57.0"] {
            let clients: serde_json::Value =
                serde_json::from_str(&fixture(&format!("hyprland_clients_{version}.json"))).unwrap();
            let reply = serde_json::json!({"clients": clients, "monitors": [{"id": 0, "name": "DP-1"}]});
            let windows = parse_windows(Wm::Hyprland, &reply.to_string()).unwrap();
            let panel = windows
                .iter()
                .find(|w| w.app_id.as_deref() == Some("cn.huntergrey.eitri.Panel"))
                .unwrap();
            assert_eq!(panel.workspace.as_deref(), Some("1"));
            assert_eq!(panel.output.as_deref(), Some("DP-1"));
            assert!(panel.address.as_deref().unwrap().starts_with("0x"));
            assert!(panel.title.as_deref().unwrap().starts_with("Eitri"));
        }
        let sway = parse_windows(Wm::Sway, &fixture("sway_tree_real_two_windows.json")).unwrap();
        assert!(sway
            .iter()
            .all(|w| w.workspace.as_deref() == Some("1") && w.output.as_deref() == Some("HEADLESS-1")));
        assert!(sway
            .iter()
            .all(|w| w.app_id.is_some() && w.title.is_some() && w.address.is_none()));
        let niri = parse_windows(Wm::Niri, &fixture("niri_windows.json")).unwrap();
        assert!(niri
            .iter()
            .all(|w| w.workspace.as_deref() == Some("1") && w.output.is_none()));
        assert_eq!(niri[1].app_id.as_deref(), Some("cn.huntergrey.eitri.Panel"));
        assert_eq!(niri[1].title.as_deref(), Some("Eitri · demo"));
    }

    #[test]
    fn a_title_never_reaches_a_compositor_command() {
        for title in ["x'}) os.execute('y') --", "a;b"] {
            let reply = fixture("hostile_window_titles.json").replace("TITLE", title);
            for wm in [Wm::Hyprland, Wm::Sway, Wm::Niri] {
                let json: serde_json::Value = serde_json::from_str(&reply).unwrap();
                let key = match wm {
                    Wm::Hyprland => "hyprland",
                    Wm::Sway => "sway",
                    _ => "niri",
                };
                let window = parse_windows(wm, &json[key].to_string()).unwrap().remove(0);
                assert_eq!(window.title.as_deref(), Some(title));
                for form in [HyprForm::Legacy, HyprForm::Lua] {
                    for argv in [
                        focus_window_argv(wm, form, &window),
                        move_to_output_argv(wm, form, &window, "DP-1"),
                        placement_argv(wm, form, &window, Some("DP-1"), crate::wm::PlacementRole::NewModule),
                    ] {
                        let argv = argv.unwrap().join(" ");
                        assert!(!argv.contains(title) && !argv.contains("hostile-class"));
                        assert!(argv.contains("4242") || argv.contains("42"));
                    }
                    assert!(move_to_output_argv(wm, form, &window, title).is_none());
                }
            }
        }
    }

    #[test]
    fn the_compositor_adapters_keep_their_config_key_and_notices() {
        assert_eq!(CONFIG_KEY, "companion.wm");
        assert_eq!(
            NO_ADAPTER_NOTICE,
            "your desktop does not let Eitri move focus; use its own window keys"
        );
        assert_eq!(
            GNOME_NO_EXTENSION_NOTICE,
            "your desktop does not let Eitri move focus; use its own window keys. \
             Install the Eitri GNOME Shell extension to move focus with Ctrl+h/j/k/l."
        );
    }

    fn fixture(name: &str) -> String {
        std::fs::read_to_string(format!("{}/tests/fixtures/wm/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
    }

    fn env_of<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<OsString> + 'a {
        move |name| pairs.iter().find(|(k, _)| *k == name).map(|(_, v)| OsString::from(v))
    }

    #[test]
    fn parse_config_accepts_the_six_values_and_names_the_key_otherwise() {
        assert_eq!(parse_config(None), Ok(WmChoice::Auto));
        assert_eq!(parse_config(Some("auto")), Ok(WmChoice::Auto));
        assert_eq!(parse_config(Some("hyprland")), Ok(WmChoice::Fixed(Wm::Hyprland)));
        assert_eq!(parse_config(Some("sway")), Ok(WmChoice::Fixed(Wm::Sway)));
        assert_eq!(parse_config(Some("niri")), Ok(WmChoice::Fixed(Wm::Niri)));
        assert_eq!(parse_config(Some("gnome")), Ok(WmChoice::Fixed(Wm::Gnome)));
        assert_eq!(parse_config(Some("none")), Ok(WmChoice::Fixed(Wm::None)));
        let err = parse_config(Some("i3")).unwrap_err();
        assert_eq!(
            err,
            "eitri.config.set(\"companion.wm\", \"i3\"): expected \"auto\", \"hyprland\", \"sway\", \"niri\", \"gnome\" or \"none\""
        );
        assert!(err.contains(CONFIG_KEY));
    }

    #[test]
    fn gnome_is_detected_last_from_a_colon_separated_desktop_name() {
        let desktop = |value: &'static str| [("XDG_CURRENT_DESKTOP", value), ("WAYLAND_DISPLAY", "wayland-0")];
        assert_eq!(detect(&env_of(&desktop("GNOME"))), Wm::Gnome);
        assert_eq!(detect(&env_of(&desktop("ubuntu:GNOME"))), Wm::Gnome);
        assert_eq!(detect(&env_of(&desktop("GNOME-Classic:GNOME"))), Wm::Gnome);
        // A whole entry, in the variable's own case: neither a substring nor another spelling.
        assert_eq!(detect(&env_of(&desktop("GNOME-Flashback:Unity"))), Wm::None);
        assert_eq!(detect(&env_of(&desktop("gnome"))), Wm::None);
        assert_eq!(detect(&env_of(&desktop("KDE"))), Wm::None);
        assert_eq!(detect(&env_of(&desktop(""))), Wm::None);
        // The compositors' own variables win over a desktop name an environment may have inherited.
        let with = |name: &'static str| [(name, "x"), ("XDG_CURRENT_DESKTOP", "GNOME")];
        assert_eq!(detect(&env_of(&with("HYPRLAND_INSTANCE_SIGNATURE"))), Wm::Hyprland);
        assert_eq!(detect(&env_of(&with("SWAYSOCK"))), Wm::Sway);
        assert_eq!(detect(&env_of(&with("NIRI_SOCKET"))), Wm::Niri);
        assert_eq!(resolve(WmChoice::Auto, &env_of(&desktop("ubuntu:GNOME"))), Wm::Gnome);
        assert_eq!(resolve(WmChoice::Fixed(Wm::None), &env_of(&desktop("GNOME"))), Wm::None);
    }

    #[test]
    fn gnome_has_no_argv_and_no_window_list_and_a_notice_that_names_the_extension() {
        let w = WindowRef {
            pid: 1,
            id: Some(1),
            focused: false,
            visible: true,
            rect: None,
            ..Default::default()
        };
        for dir in [Direction::Left, Direction::Right, Direction::Up, Direction::Down] {
            assert_eq!(move_focus_argv(Wm::Gnome, HyprForm::Legacy, dir), None);
        }
        assert_eq!(list_windows_argv(Wm::Gnome), None);
        assert_eq!(focus_window_argv(Wm::Gnome, HyprForm::Legacy, &w), None);
        assert!(parse_windows(Wm::Gnome, "[]").is_err());
        assert_eq!(
            GNOME_NO_EXTENSION_NOTICE,
            "your desktop does not let Eitri move focus; use its own window keys. \
             Install the Eitri GNOME Shell extension to move focus with Ctrl+h/j/k/l."
        );
        assert!(GNOME_NO_EXTENSION_NOTICE.starts_with(NO_ADAPTER_NOTICE));
    }

    #[test]
    fn gnome_is_detected_only_in_a_wayland_session() {
        let gnome = ("XDG_CURRENT_DESKTOP", "ubuntu:GNOME");
        // Wayland by the display variable, or by the session type alone.
        assert_eq!(detect(&env_of(&[gnome, ("WAYLAND_DISPLAY", "wayland-0")])), Wm::Gnome);
        assert_eq!(detect(&env_of(&[gnome, ("XDG_SESSION_TYPE", "wayland")])), Wm::Gnome);
        // An Xorg session: no window passes the extension's fresh-input rule, so it is no adapter.
        assert_eq!(detect(&env_of(&[gnome])), Wm::None);
        assert_eq!(detect(&env_of(&[gnome, ("XDG_SESSION_TYPE", "x11")])), Wm::None);
        assert_eq!(
            detect(&env_of(&[gnome, ("XDG_SESSION_TYPE", "x11"), ("DISPLAY", ":0")])),
            Wm::None
        );
        assert_eq!(detect(&env_of(&[gnome, ("WAYLAND_DISPLAY", "")])), Wm::None);
        assert_eq!(
            detect(&env_of(&[gnome, ("WAYLAND_DISPLAY", ""), ("XDG_SESSION_TYPE", "tty")])),
            Wm::None
        );
        // A wayland session type with the display variable empty still counts (a stripped environment).
        assert_eq!(
            detect(&env_of(&[
                gnome,
                ("WAYLAND_DISPLAY", ""),
                ("XDG_SESSION_TYPE", "wayland")
            ])),
            Wm::Gnome
        );
        // `companion.wm = "gnome"` still forces it on an X11 session.
        assert_eq!(resolve(WmChoice::Fixed(Wm::Gnome), &env_of(&[gnome])), Wm::Gnome);
    }

    #[test]
    fn detect_follows_the_documented_order_and_ignores_empty_values() {
        let all = [
            ("HYPRLAND_INSTANCE_SIGNATURE", "h"),
            ("SWAYSOCK", "/s"),
            ("NIRI_SOCKET", "/n"),
        ];
        assert_eq!(detect(&env_of(&all)), Wm::Hyprland);
        assert_eq!(detect(&env_of(&all[1..])), Wm::Sway);
        assert_eq!(detect(&env_of(&all[2..])), Wm::Niri);
        assert_eq!(detect(&env_of(&[])), Wm::None);
        let empties = [
            ("HYPRLAND_INSTANCE_SIGNATURE", ""),
            ("SWAYSOCK", ""),
            ("NIRI_SOCKET", "/n"),
        ];
        assert_eq!(detect(&env_of(&empties)), Wm::Niri);
        let all_empty = [
            ("HYPRLAND_INSTANCE_SIGNATURE", ""),
            ("SWAYSOCK", ""),
            ("NIRI_SOCKET", ""),
        ];
        assert_eq!(detect(&env_of(&all_empty)), Wm::None);
        assert_eq!(resolve(WmChoice::Auto, &env_of(&all)), Wm::Hyprland);
        assert_eq!(resolve(WmChoice::Fixed(Wm::Niri), &env_of(&all)), Wm::Niri);
        assert_eq!(resolve(WmChoice::Fixed(Wm::None), &env_of(&all)), Wm::None);
    }

    #[test]
    fn every_direction_builds_the_documented_argv_per_wm() {
        let v = |parts: &[&str]| Some(parts.iter().map(ToString::to_string).collect::<Vec<_>>());
        let dirs = [Direction::Left, Direction::Right, Direction::Up, Direction::Down];
        let hypr = ["l", "r", "u", "d"];
        let sway = ["left", "right", "up", "down"];
        let niri = [
            "focus-column-left",
            "focus-column-right",
            "focus-window-up",
            "focus-window-down",
        ];
        for (i, dir) in dirs.into_iter().enumerate() {
            assert_eq!(
                move_focus_argv(Wm::Hyprland, HyprForm::Legacy, dir),
                v(&["hyprctl", "dispatch", "movefocus", hypr[i]])
            );
            assert_eq!(
                move_focus_argv(Wm::Sway, HyprForm::Legacy, dir),
                v(&["swaymsg", "focus", sway[i]])
            );
            assert_eq!(
                move_focus_argv(Wm::Niri, HyprForm::Legacy, dir),
                v(&["niri", "msg", "action", niri[i]])
            );
            assert_eq!(move_focus_argv(Wm::None, HyprForm::Legacy, dir), None);
        }
        assert_eq!(list_windows_argv(Wm::Hyprland), v(&["hyprctl", "clients", "-j"]));
        assert_eq!(list_windows_argv(Wm::Sway), v(&["swaymsg", "-t", "get_tree"]));
        assert_eq!(list_windows_argv(Wm::Niri), v(&["niri", "msg", "--json", "windows"]));
        assert_eq!(list_windows_argv(Wm::None), None);

        let w = WindowRef {
            pid: 42,
            id: Some(7),
            focused: false,
            visible: true,
            rect: None,
            ..Default::default()
        };
        assert_eq!(
            focus_window_argv(Wm::Hyprland, HyprForm::Legacy, &w),
            v(&["hyprctl", "dispatch", "focuswindow", "pid:42"])
        );
        assert_eq!(
            focus_window_argv(Wm::Sway, HyprForm::Legacy, &w),
            v(&["swaymsg", "[pid=42] focus"])
        );
        assert_eq!(
            focus_window_argv(Wm::Niri, HyprForm::Legacy, &w),
            v(&["niri", "msg", "action", "focus-window", "--id", "7"])
        );
        assert_eq!(focus_window_argv(Wm::None, HyprForm::Legacy, &w), None);
        let no_id = WindowRef { id: None, ..w };
        assert_eq!(focus_window_argv(Wm::Niri, HyprForm::Legacy, &no_id), None);
        assert!(focus_window_argv(Wm::Sway, HyprForm::Legacy, &no_id).is_some());
    }

    #[test]
    fn moving_a_window_to_an_output_builds_the_documented_argv_per_wm() {
        let v = |parts: &[&str]| Some(parts.iter().map(ToString::to_string).collect::<Vec<_>>());
        let w = WindowRef {
            pid: 42,
            id: Some(7),
            focused: false,
            visible: true,
            rect: None,
            ..Default::default()
        };
        assert_eq!(
            move_to_output_argv(Wm::Sway, HyprForm::Legacy, &w, "HEADLESS-2"),
            v(&["swaymsg", "[pid=42] move container to output \"HEADLESS-2\""])
        );
        assert_eq!(
            move_to_output_argv(Wm::Hyprland, HyprForm::Legacy, &w, "DP-1"),
            v(&[
                "hyprctl",
                "--batch",
                "dispatch focuswindow pid:42 ; dispatch movewindow mon:DP-1"
            ])
        );
        assert_eq!(
            move_to_output_argv(Wm::Niri, HyprForm::Legacy, &w, "eDP-1"),
            v(&["niri", "msg", "action", "move-window-to-monitor", "--id", "7", "eDP-1"])
        );
        assert_eq!(
            move_to_output_argv(
                Wm::Niri,
                HyprForm::Legacy,
                &WindowRef { id: None, ..w.clone() },
                "eDP-1"
            ),
            None
        );
        assert_eq!(move_to_output_argv(Wm::Gnome, HyprForm::Legacy, &w, "DP-1"), None);
        assert_eq!(move_to_output_argv(Wm::None, HyprForm::Legacy, &w, "DP-1"), None);
    }

    #[test]
    fn hyprland_takes_the_probed_form_and_the_others_ignore_it() {
        let v = |parts: &[&str]| Some(parts.iter().map(ToString::to_string).collect::<Vec<_>>());
        let w = WindowRef {
            pid: 42,
            id: Some(7),
            focused: false,
            visible: true,
            rect: None,
            ..Default::default()
        };
        assert_eq!(
            move_focus_argv(Wm::Hyprland, HyprForm::Lua, Direction::Left),
            v(&["hyprctl", "dispatch", "hl.dsp.focus({ direction = 'l' })"])
        );
        assert_eq!(
            focus_window_argv(Wm::Hyprland, HyprForm::Lua, &w),
            v(&["hyprctl", "dispatch", "hl.dsp.focus({ window = 'pid:42' })"])
        );
        assert_eq!(
            move_to_output_argv(Wm::Hyprland, HyprForm::Lua, &w, "DP-1"),
            v(&[
                "hyprctl",
                "dispatch",
                "hl.dsp.window.move({ window = 'pid:42', monitor = 'DP-1' })"
            ])
        );
        for wm in [Wm::Sway, Wm::Niri] {
            for dir in [Direction::Left, Direction::Down] {
                assert_eq!(
                    move_focus_argv(wm, HyprForm::Lua, dir),
                    move_focus_argv(wm, HyprForm::Legacy, dir)
                );
            }
            assert_eq!(
                focus_window_argv(wm, HyprForm::Lua, &w),
                focus_window_argv(wm, HyprForm::Legacy, &w)
            );
            assert_eq!(
                move_to_output_argv(wm, HyprForm::Lua, &w, "DP-1"),
                move_to_output_argv(wm, HyprForm::Legacy, &w, "DP-1")
            );
        }
        // Replies are judged for Hyprland's dispatches only.
        let hypr = move_focus_argv(Wm::Hyprland, HyprForm::Legacy, Direction::Left).unwrap();
        assert_eq!(dispatch_reply_failure(Wm::Hyprland, &hypr, "ok\n"), None);
        assert_eq!(
            dispatch_reply_failure(Wm::Hyprland, &hypr, "Invalid dispatcher"),
            Some("Invalid dispatcher".to_string())
        );
        let sway = move_focus_argv(Wm::Sway, HyprForm::Legacy, Direction::Left).unwrap();
        assert_eq!(dispatch_reply_failure(Wm::Sway, &sway, r#"[{"success": true}]"#), None);
    }

    #[test]
    fn a_new_module_window_is_focused_by_its_own_pid_and_moved_nowhere() {
        use crate::wm::PlacementRole;
        let w = WindowRef {
            pid: 4242,
            id: None,
            focused: false,
            visible: true,
            rect: None,
            ..Default::default()
        };
        for (wm, form) in [
            (Wm::Sway, HyprForm::Legacy),
            (Wm::Hyprland, HyprForm::Legacy),
            (Wm::Hyprland, HyprForm::Lua),
        ] {
            let focus = focus_window_argv(wm, form, &w);
            assert!(focus.is_some(), "{wm:?}");
            assert_eq!(
                placement_argv(wm, form, &w, None, PlacementRole::NewModule),
                focus,
                "{wm:?} {form:?}"
            );
        }
        // niri and GNOME focus by the token alone.
        for wm in [Wm::Niri, Wm::Gnome, Wm::None] {
            assert_eq!(
                placement_argv(wm, HyprForm::Legacy, &w, None, PlacementRole::NewModule),
                None
            );
        }
    }

    #[test]
    fn a_hyprland_placement_ends_with_the_focus_its_role_names_in_either_form() {
        use crate::wm::PlacementRole;
        let v = |parts: &[&str]| Some(parts.iter().map(ToString::to_string).collect::<Vec<_>>());
        let w = WindowRef {
            pid: 4242,
            id: None,
            focused: false,
            visible: true,
            rect: None,
            ..Default::default()
        };
        let restored = PlacementRole::RestoredSecond { first_pid: 101 };
        for (role, focus_pid) in [(PlacementRole::NewSecond, 4242), (restored, 101)] {
            assert_eq!(
                placement_argv(Wm::Hyprland, HyprForm::Legacy, &w, Some("DP-2"), role),
                Some(vec![
                    "hyprctl".to_string(),
                    "--batch".to_string(),
                    format!(
                        "dispatch focuswindow pid:4242 ; dispatch movewindow mon:DP-2 ; dispatch focuswindow pid:{focus_pid}"
                    )
                ])
            );
            assert_eq!(
                placement_argv(Wm::Hyprland, HyprForm::Lua, &w, Some("DP-2"), role),
                Some(vec![
                    "hyprctl".to_string(),
                    "--batch".to_string(),
                    format!(
                        "dispatch hl.dsp.window.move({{ window = 'pid:4242', monitor = 'DP-2' }}) ; dispatch hl.dsp.focus({{ window = 'pid:{focus_pid}' }})"
                    )
                ])
            );
        }
        assert_eq!(
            placement_argv(Wm::Hyprland, HyprForm::Legacy, &w, Some("DP-2"), PlacementRole::Main),
            v(&[
                "hyprctl",
                "--batch",
                "dispatch focuswindow pid:4242 ; dispatch movewindow mon:DP-2"
            ])
        );
        assert_eq!(
            placement_argv(Wm::Hyprland, HyprForm::Lua, &w, Some("DP-2"), PlacementRole::Main),
            move_to_output_argv(Wm::Hyprland, HyprForm::Lua, &w, "DP-2")
        );
        assert_eq!(
            placement_argv(Wm::Hyprland, HyprForm::Legacy, &w, None, restored),
            v(&["hyprctl", "dispatch", "focuswindow", "pid:101"])
        );
        assert_eq!(
            placement_argv(Wm::Hyprland, HyprForm::Lua, &w, None, restored),
            v(&["hyprctl", "dispatch", "hl.dsp.focus({ window = 'pid:101' })"])
        );
        for form in [HyprForm::Legacy, HyprForm::Lua] {
            assert_eq!(placement_argv(Wm::Hyprland, form, &w, None, PlacementRole::Main), None);
            assert_eq!(placement_argv(Wm::Hyprland, form, &w, Some("DP-2; x"), restored), None);
        }
    }

    #[test]
    fn an_output_name_with_anything_but_a_plain_word_is_never_put_on_a_command_line() {
        let long = "x".repeat(65);
        for bad in ["", "DP 1", "DP-1\"; exec x", "a;b", "x\ny", long.as_str()] {
            assert!(!valid_output_name(bad), "{bad:?}");
            let w = WindowRef {
                pid: 1,
                id: Some(1),
                focused: false,
                visible: true,
                rect: None,
                ..Default::default()
            };
            assert_eq!(move_to_output_argv(Wm::Sway, HyprForm::Legacy, &w, bad), None);
        }
        for good in ["DP-1", "HDMI-A-2", "HEADLESS-2", "eDP-1", "Virtual-1", "DVI_I.1"] {
            assert!(valid_output_name(good), "{good}");
        }
    }

    #[test]
    fn sway_tree_parses_leaf_windows_with_pids_and_marks_the_hidden_one() {
        let ws = parse_windows(Wm::Sway, &fixture("sway_tree_two_windows.json")).unwrap();
        // The window on the other workspace is kept, marked not visible, so it can still be raised.
        assert_eq!(ws.len(), 3);
        assert_eq!((ws[2].pid, ws[2].visible, ws[2].focused), (4003, false, false));
        assert!(ws[0].visible && ws[1].visible);
        assert_eq!(ws[0].pid, 4001);
        assert!(!ws[0].focused);
        assert_eq!(
            ws[0].rect,
            Some(Rect {
                x: 0,
                y: 0,
                w: 960,
                h: 1080
            })
        );
        assert_eq!(ws[1].pid, 4002);
        assert!(ws[1].focused);
        assert_eq!(
            ws[1].rect,
            Some(Rect {
                x: 960,
                y: 0,
                w: 960,
                h: 1080
            })
        );
        assert!(parse_windows(Wm::Sway, "[1]").is_err());
        assert!(parse_windows(Wm::Sway, "not json").is_err());
    }

    #[test]
    fn has_neighbour_is_false_at_the_left_edge_so_a_wrapping_sway_is_never_asked() {
        let mut ws = parse_windows(Wm::Sway, &fixture("sway_tree_two_windows.json")).unwrap();
        // The panel is focused on the right half: something lies to its left, nothing beyond the other edges.
        assert_eq!(has_neighbour(&ws, Direction::Left), Some(true));
        assert_eq!(has_neighbour(&ws, Direction::Right), Some(false));
        assert_eq!(has_neighbour(&ws, Direction::Up), Some(false));
        assert_eq!(has_neighbour(&ws, Direction::Down), Some(false));
        // Focus the leftmost window: nothing to its left.
        ws[0].focused = true;
        ws[1].focused = false;
        assert_eq!(has_neighbour(&ws, Direction::Left), Some(false));
        assert_eq!(has_neighbour(&ws, Direction::Right), Some(true));
        // No focused window with a rectangle: unknown.
        ws[0].focused = false;
        assert_eq!(has_neighbour(&ws, Direction::Left), None);
        assert_eq!(has_neighbour(&[], Direction::Left), None);
    }

    #[test]
    fn only_a_window_on_screen_over_the_focused_one_counts_as_overlapping() {
        let r = |x, y, w, h| Some(Rect { x, y, w, h });
        let window = |focused, visible, rect| WindowRef {
            focused,
            visible,
            rect,
            ..Default::default()
        };
        let float = window(true, true, r(100, 100, 800, 600));
        assert!(focused_overlaps(&[
            float.clone(),
            window(false, true, r(700, 100, 800, 600))
        ]));
        assert!(!focused_overlaps(&[
            float.clone(),
            window(false, false, r(700, 100, 800, 600))
        ]));
        assert!(!focused_overlaps(&[
            float.clone(),
            window(false, true, r(900, 100, 800, 600))
        ]));
        assert!(!focused_overlaps(&[
            window(false, true, r(0, 0, 9, 9)),
            window(false, true, r(0, 0, 9, 9))
        ]));
    }

    #[test]
    fn a_hidden_window_is_no_neighbour_but_can_be_raised() {
        let r = |x, y, w, h| Some(Rect { x, y, w, h });
        let panel = WindowRef {
            pid: 2,
            id: Some(2),
            focused: true,
            visible: true,
            rect: r(960, 0, 960, 1080),
            ..Default::default()
        };
        // The editor sits to the panel's left, but on a workspace that is not shown (sway keeps its
        // last rectangle there), or behind another tab.
        let editor = WindowRef {
            pid: 1,
            id: Some(1),
            focused: false,
            visible: false,
            rect: r(0, 0, 960, 1080),
            ..Default::default()
        };
        let ws = [editor, panel];
        assert_eq!(has_neighbour(&ws, Direction::Left), Some(false));
        assert_eq!(first_window_owner(&[300, 1], &ws).map(|w| w.pid), Some(1));
        assert_eq!(
            first_window_owner(&[300, 1], &ws).and_then(|w| focus_window_argv(Wm::Sway, HyprForm::Legacy, w)),
            Some(vec!["swaymsg".to_string(), "[pid=1] focus".to_string()])
        );
        // The same from the fixture's window on workspace 2.
        let ws = parse_windows(Wm::Sway, &fixture("sway_tree_two_windows.json")).unwrap();
        assert_eq!(first_window_owner(&[4003], &ws).map(|w| w.pid), Some(4003));
        let h = parse_windows(
            Wm::Hyprland,
            r#"[{"mapped":true,"hidden":true,"pid":8,"focusHistoryID":3},{"mapped":false,"pid":9}]"#,
        )
        .unwrap();
        assert_eq!(h.len(), 1);
        assert_eq!((h[0].pid, h[0].visible), (8, false));
    }

    #[test]
    fn has_neighbour_needs_the_other_axis_to_overlap() {
        let r = |x, y, w, h| Some(Rect { x, y, w, h });
        let w = |focused, rect| WindowRef {
            pid: 1,
            id: None,
            focused,
            visible: true,
            rect,
            ..Default::default()
        };
        // Beside the focused window's edge but entirely below its span: not a left neighbour.
        let ws = [w(true, r(500, 0, 500, 400)), w(false, r(0, 400, 500, 400))];
        assert_eq!(has_neighbour(&ws, Direction::Left), Some(false));
        assert_eq!(has_neighbour(&ws, Direction::Down), Some(false));
        // Touching edges count as beyond.
        let ws = [w(true, r(500, 0, 500, 400)), w(false, r(0, 100, 500, 100))];
        assert_eq!(has_neighbour(&ws, Direction::Left), Some(true));
    }

    #[test]
    fn a_real_sway_tree_parses_and_the_left_edge_has_no_neighbour() {
        // `swaymsg -t get_tree` recorded from a headless sway holding a terminal (pid 1955365, focused, left half)
        // and a companion panel (pid 1955517, right half): every field the real thing sends, not a hand-cut subset.
        let mut ws = parse_windows(Wm::Sway, &fixture("sway_tree_real_two_windows.json")).unwrap();
        let pids: Vec<u32> = ws.iter().map(|w| w.pid).collect();
        assert_eq!(pids, [1955365, 1955517]);
        assert_eq!((ws[0].focused, ws[1].focused), (true, false));
        assert_eq!(
            ws[1].rect,
            Some(Rect {
                x: 800,
                y: 0,
                w: 800,
                h: 900
            })
        );
        assert_eq!(has_neighbour(&ws, Direction::Left), Some(false));
        assert_eq!(has_neighbour(&ws, Direction::Right), Some(true));
        ws[0].focused = false;
        ws[1].focused = true;
        assert_eq!(has_neighbour(&ws, Direction::Left), Some(true));
    }

    #[test]
    fn has_neighbour_crosses_outputs() {
        let ws = parse_windows(Wm::Sway, &fixture("sway_tree_two_outputs.json")).unwrap();
        let pids: Vec<u32> = ws.iter().map(|w| w.pid).collect();
        assert_eq!(pids, [5001, 5002, 5003]);
        // Focused window is on the right output: the left output's window is a left neighbour.
        assert_eq!(has_neighbour(&ws, Direction::Left), Some(true));
        assert_eq!(has_neighbour(&ws, Direction::Right), Some(false));
    }

    /// `hyprctl clients -j` on Hyprland 0.57 with a compositor-mode group in one workspace: window 1 on the left, the
    /// chat and page windows stacked beside it, each with its own app id and the fixed title it mapped with. Every
    /// Eitri window has the one app id its kind has, so a list of them tells the windows apart.
    #[test]
    fn a_compositor_mode_group_on_hyprland_parses() {
        let w = parse_windows(Wm::Hyprland, &fixture("hyprland_clients_cm_0.57.0.json")).unwrap();
        let of = |app: &str| w.iter().find(|w| w.app_id.as_deref() == Some(app)).unwrap();
        let (editor, chat, page) = (
            of("cn.huntergrey.eitri"),
            of("cn.huntergrey.eitri.Panel"),
            of("cn.huntergrey.eitri.Page"),
        );
        assert_eq!(w.len(), 3);
        assert_eq!(editor.title.as_deref(), Some("project — Eitri"));
        assert_eq!(chat.title.as_deref(), Some("project — Eitri chat"));
        assert_eq!(page.title.as_deref(), Some("project — Notes · Eitri"));
        assert_eq!(
            w.iter().filter(|w| w.focused).map(|w| w.pid).collect::<Vec<_>>(),
            [page.pid]
        );
        assert!(w.iter().all(|w| w.workspace.as_deref() == Some("1") && w.visible));
        let (e, c) = (editor.rect.unwrap(), chat.rect.unwrap());
        assert!(e.x + e.w <= c.x, "the chat window sits beside the editor");
    }

    /// The page window of that group has the chat above it and window 1 to its left, and nothing below or to its right.
    /// A client list holds every workspace's windows, so only the monitors' reply takes the others out.
    #[test]
    fn hyprland_neighbours_are_the_windows_its_monitors_show() {
        let clients: serde_json::Value = serde_json::from_str(&fixture("hyprland_clients_cm_0.57.0.json")).unwrap();
        let parsed = |active: i64| {
            let monitors = serde_json::json!([{"id": 0, "name": "DP-1", "activeWorkspace": {"id": active}}]);
            let reply = serde_json::json!({"clients": clients, "monitors": monitors});
            parse_windows(Wm::Hyprland, &reply.to_string()).unwrap()
        };
        let on_screen = parsed(1);
        assert_eq!(has_neighbour(&on_screen, Direction::Up), Some(true));
        assert_eq!(has_neighbour(&on_screen, Direction::Left), Some(true));
        assert_eq!(has_neighbour(&on_screen, Direction::Down), Some(false));
        assert_eq!(has_neighbour(&on_screen, Direction::Right), Some(false));
        assert_eq!(has_neighbour(&parsed(2), Direction::Up), Some(false));
    }

    /// `hyprctl clients -j` as Hyprland 0.56.2 and 0.57.0 print it, with nvim's foot on the left and the panel on the
    /// right, the panel focused: each carries many more fields than the parser reads, and the ones it reads are there.
    #[test]
    fn real_hyprland_window_lists_parse() {
        for (name, foot, panel) in [
            ("hyprland_clients_0.56.2.json", 4144, 4498),
            ("hyprland_clients_0.57.0.json", 5114, 5128),
        ] {
            let windows = parse_windows(Wm::Hyprland, &fixture(name)).unwrap();
            let seen: Vec<(u32, bool, bool, Option<Rect>)> =
                windows.iter().map(|w| (w.pid, w.focused, w.visible, w.rect)).collect();
            let rect = |x| {
                Some(Rect {
                    x,
                    y: 22,
                    w: 931,
                    h: 1036,
                })
            };
            assert_eq!(
                seen,
                [(foot, false, true, rect(22)), (panel, true, true, rect(967))],
                "{name}"
            );
            assert_eq!(
                first_window_owner(&[panel], &windows).map(|w| w.pid),
                Some(panel),
                "{name}"
            );
            assert_eq!(has_neighbour(&windows, Direction::Left), Some(true), "{name}");
            assert_eq!(has_neighbour(&windows, Direction::Right), Some(false), "{name}");
        }
    }

    #[test]
    fn hyprland_and_niri_parse_their_window_lists() {
        let h = parse_windows(Wm::Hyprland, &fixture("hyprland_clients.json")).unwrap();
        assert_eq!(h.len(), 2);
        assert_eq!((h[0].pid, h[0].focused), (6001, false));
        assert_eq!(
            h[0].rect,
            Some(Rect {
                x: 0,
                y: 0,
                w: 960,
                h: 1080
            })
        );
        assert_eq!((h[1].pid, h[1].focused), (6002, true));
        assert_eq!(
            h[1].rect,
            Some(Rect {
                x: 960,
                y: 0,
                w: 960,
                h: 1080
            })
        );
        assert_eq!(h[0].id, None);

        let n = parse_windows(Wm::Niri, &fixture("niri_windows.json")).unwrap();
        assert_eq!(n.len(), 2);
        assert_eq!(
            (n[0].pid, n[0].id, n[0].focused, n[0].rect),
            (7001, Some(12), false, None)
        );
        assert_eq!((n[1].pid, n[1].id, n[1].focused), (7002, Some(15), true));
        // niri has no rectangles, so the wrap check is unknown rather than wrong.
        assert_eq!(has_neighbour(&n, Direction::Left), None);

        assert!(parse_windows(Wm::Hyprland, "{}").is_err());
        assert!(parse_windows(Wm::Niri, "{}").is_err());
        assert!(parse_windows(Wm::None, "[]").is_err());
        // A niri window without a pid cannot be matched to a process and is left out.
        let n = parse_windows(
            Wm::Niri,
            r#"[{"id":1,"is_focused":true},{"id":2,"pid":9,"is_focused":false}]"#,
        )
        .unwrap();
        assert_eq!(n.len(), 1);
        assert_eq!(n[0].pid, 9);
    }

    fn win(pid: u32, focused: bool) -> WindowRef {
        WindowRef {
            pid,
            id: Some(pid as u64),
            focused,
            visible: true,
            rect: None,
            ..Default::default()
        }
    }

    #[test]
    fn a_chain_through_tmux_server_reaches_no_window() {
        // nvim under tmux: the chain is nvim, the tmux server, systemd -- no terminal window in it.
        let chain = [300, 200, 100];
        let windows = [win(4001, false), win(4002, true)];
        assert_eq!(first_window_owner(&chain, &windows), None);
        assert_eq!(first_window_owner(&[], &windows), None);
    }

    #[test]
    fn neovide_as_parent_is_found() {
        let windows = [win(4001, false), win(4002, true)];
        assert_eq!(
            first_window_owner(&[300, 4001, 100], &windows).map(|w| w.pid),
            Some(4001)
        );
        // The nearest process wins over a farther one even when the farther one is focused.
        assert_eq!(first_window_owner(&[4001, 4002], &windows).map(|w| w.pid), Some(4001));
        // Several windows for one pid: the focused one, else the first.
        let two = [
            WindowRef {
                id: Some(1),
                ..win(50, false)
            },
            WindowRef {
                id: Some(2),
                ..win(50, true)
            },
            WindowRef {
                id: Some(3),
                ..win(50, false)
            },
        ];
        assert_eq!(first_window_owner(&[50], &two).and_then(|w| w.id), Some(2));
        let unfocused = [
            WindowRef {
                id: Some(4),
                ..win(60, false)
            },
            WindowRef {
                id: Some(5),
                ..win(60, false)
            },
        ];
        assert_eq!(first_window_owner(&[60], &unfocused).and_then(|w| w.id), Some(4));
    }
}
