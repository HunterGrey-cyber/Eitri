//! Window-manager adapters for companion mode: which compositor this is, the argv that moves
//! focus or raises a window, and how to read its window list. Nothing here runs a command or
//! touches the desktop; the caller does that off the GTK thread and feeds the output back in.
//!
//! Only sway is asked whether a move would wrap (it wraps around the edge of the last window);
//! Hyprland and niri have no such check. GNOME has no command line to ask: a client there cannot
//! list or focus windows, so `Wm::Gnome` is served by a Shell extension over D-Bus (the shell crate
//! makes those calls) and every argv function here has nothing to say for it.

use crate::layout::Direction;
use std::ffi::OsString;

mod hyprland;
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowRef {
    pub pid: u32,
    pub id: Option<u64>,
    pub focused: bool,
    /// On screen now. A window that is not (another workspace, a hidden tab) is no neighbour for a
    /// directional move, but can still be raised.
    pub visible: bool,
    pub rect: Option<Rect>,
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
    Some(parts.iter().map(|s| s.to_string()).collect())
}

pub fn move_focus_argv(wm: Wm, dir: Direction) -> Option<Vec<String>> {
    match wm {
        Wm::Hyprland => {
            let d = match dir {
                Direction::Left => "l",
                Direction::Right => "r",
                Direction::Up => "u",
                Direction::Down => "d",
            };
            argv(&["hyprctl", "dispatch", "movefocus", d])
        }
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

/// The command that focuses `target`. niri needs the window id; without one there is no command.
pub fn focus_window_argv(wm: Wm, target: &WindowRef) -> Option<Vec<String>> {
    match wm {
        Wm::Hyprland => Some(vec![
            "hyprctl".to_string(),
            "dispatch".to_string(),
            "focuswindow".to_string(),
            format!("pid:{}", target.pid),
        ]),
        // One argv element: swaymsg joins its arguments itself, and the criteria must lead the command.
        Wm::Sway => Some(vec!["swaymsg".to_string(), format!("[pid={}] focus", target.pid)]),
        Wm::Niri => target.id.map(|id| {
            ["niri", "msg", "action", "focus-window", "--id"]
                .iter()
                .map(|s| s.to_string())
                .chain(std::iter::once(id.to_string()))
                .collect()
        }),
        Wm::Gnome | Wm::None => None,
    }
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

/// `start` and its ancestors, nearest first, read from `/proc/<pid>/stat` text. The parent pid is
/// the second field after the last `)`, because the command name may itself hold spaces and
/// parentheses. Stops before pid 1 (init owns no window), on a read or parse failure, on a cycle,
/// and at `max` entries.
pub fn ppid_chain(start: u32, read_stat: &dyn Fn(u32) -> Option<String>, max: usize) -> Vec<u32> {
    let mut chain = Vec::new();
    let mut pid = start;
    while chain.len() < max && !chain.contains(&pid) {
        chain.push(pid);
        let Some(stat) = read_stat(pid) else { break };
        let Some(ppid) = stat
            .rsplit_once(')')
            .and_then(|(_, rest)| rest.split_whitespace().nth(1))
            .and_then(|f| f.parse::<u32>().ok())
        else {
            break;
        };
        if ppid <= 1 {
            break;
        }
        pid = ppid;
    }
    chain
}

/// The linux reader for [`ppid_chain`]; other platforms have no `/proc`.
#[cfg(target_os = "linux")]
pub fn proc_stat(pid: u32) -> Option<String> {
    std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()
}

#[cfg(not(target_os = "linux"))]
pub fn proc_stat(_pid: u32) -> Option<String> {
    None
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
        };
        for dir in [Direction::Left, Direction::Right, Direction::Up, Direction::Down] {
            assert_eq!(move_focus_argv(Wm::Gnome, dir), None);
        }
        assert_eq!(list_windows_argv(Wm::Gnome), None);
        assert_eq!(focus_window_argv(Wm::Gnome, &w), None);
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
        let v = |parts: &[&str]| Some(parts.iter().map(|s| s.to_string()).collect::<Vec<_>>());
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
                move_focus_argv(Wm::Hyprland, dir),
                v(&["hyprctl", "dispatch", "movefocus", hypr[i]])
            );
            assert_eq!(move_focus_argv(Wm::Sway, dir), v(&["swaymsg", "focus", sway[i]]));
            assert_eq!(move_focus_argv(Wm::Niri, dir), v(&["niri", "msg", "action", niri[i]]));
            assert_eq!(move_focus_argv(Wm::None, dir), None);
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
        };
        assert_eq!(
            focus_window_argv(Wm::Hyprland, &w),
            v(&["hyprctl", "dispatch", "focuswindow", "pid:42"])
        );
        assert_eq!(focus_window_argv(Wm::Sway, &w), v(&["swaymsg", "[pid=42] focus"]));
        assert_eq!(
            focus_window_argv(Wm::Niri, &w),
            v(&["niri", "msg", "action", "focus-window", "--id", "7"])
        );
        assert_eq!(focus_window_argv(Wm::None, &w), None);
        let no_id = WindowRef { id: None, ..w };
        assert_eq!(focus_window_argv(Wm::Niri, &no_id), None);
        assert!(focus_window_argv(Wm::Sway, &no_id).is_some());
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
    fn a_hidden_window_is_no_neighbour_but_can_be_raised() {
        let r = |x, y, w, h| Some(Rect { x, y, w, h });
        let panel = WindowRef {
            pid: 2,
            id: Some(2),
            focused: true,
            visible: true,
            rect: r(960, 0, 960, 1080),
        };
        // The editor sits to the panel's left, but on a workspace that is not shown (sway keeps its
        // last rectangle there), or behind another tab.
        let editor = WindowRef {
            pid: 1,
            id: Some(1),
            focused: false,
            visible: false,
            rect: r(0, 0, 960, 1080),
        };
        let ws = [editor, panel];
        assert_eq!(has_neighbour(&ws, Direction::Left), Some(false));
        assert_eq!(first_window_owner(&[300, 1], &ws).map(|w| w.pid), Some(1));
        assert_eq!(
            first_window_owner(&[300, 1], &ws).and_then(|w| focus_window_argv(Wm::Sway, w)),
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

    fn stat_table(rows: &'static [(u32, &'static str)]) -> impl Fn(u32) -> Option<String> {
        move |pid| rows.iter().find(|(p, _)| *p == pid).map(|(_, s)| s.to_string())
    }

    #[test]
    fn ppid_chain_parses_a_comm_with_spaces_and_parens_and_stops_at_init() {
        static ROWS: &[(u32, &str)] = &[
            (300, "300 (nvim) S 200 300 300 0 -1 4194560 1 0 0 0"),
            (200, "200 (tmux: server) (x) S 100 200 200 0 -1 4194560"),
            (100, "100 (a b) c) S 1 100 100 0 -1 4194560"),
        ];
        assert_eq!(ppid_chain(300, &stat_table(ROWS), 32), [300, 200, 100]);
        // A read failure ends the chain; the start is always there.
        assert_eq!(ppid_chain(999, &stat_table(ROWS), 32), [999]);
        // The cap bounds it.
        assert_eq!(ppid_chain(300, &stat_table(ROWS), 2), [300, 200]);
        // A malformed stat ends it.
        static BAD: &[(u32, &str)] = &[(5, "5 nvim S 1")];
        assert_eq!(ppid_chain(5, &stat_table(BAD), 32), [5]);
        // A loop does not spin.
        static LOOP: &[(u32, &str)] = &[(7, "7 (a) S 8 0"), (8, "8 (b) S 7 0")];
        assert_eq!(ppid_chain(7, &stat_table(LOOP), 32), [7, 8]);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn proc_stat_reads_this_process_and_its_chain_is_bounded() {
        let me = std::process::id();
        let chain = ppid_chain(me, &proc_stat, 32);
        assert_eq!(chain[0], me);
        assert!(chain.len() <= 32);
        assert!(!chain.contains(&1));
    }

    fn win(pid: u32, focused: bool) -> WindowRef {
        WindowRef {
            pid,
            id: Some(pid as u64),
            focused,
            visible: true,
            rect: None,
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
