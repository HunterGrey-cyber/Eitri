//! The closed set of things a key can do (spec §2.3), named the way tmux names its commands. A new
//! action is a change to this enum and to `shell`'s own closed `match` (`shell/src/main.rs`), never
//! a plugin -- the same discipline as `ModuleKind`.

use std::fmt;

use super::key::KeySpec;
use crate::layout::{Axis, Direction, ModuleId, ModuleKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwapTarget {
    /// tmux `swap-pane -U`: the previous module on screen, in tree order, wrapping.
    Prev,
    /// tmux `swap-pane -D`.
    Next,
    /// The neighbour that way, as `Ctrl+h/j/k/l` would choose it (modules P2's `H/J/K/L`).
    Toward(Direction),
}

/// Session tabs (spec §3). Bound in phase 1, run in phase 2 (this plan's ruling 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabAction {
    New,
    Next,
    Prev,
    Last,
    Select(u8),
    Rename,
    Close,
    /// Close every other tab (LazyVim "Delete Other Buffers", `keymaps.lua:43-45`; panel round 2
    /// plan's Owner answers Q2). Panel-only for now: not bound after the prefix.
    CloseOthers,
    Choose,
    Info,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextChange {
    Larger,
    Smaller,
    Reset,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Hand the pane with the keys the prefix chord itself (spec §2.6).
    SendPrefix,
    /// Hand the pane with the keys this key (ruling 3).
    SendKeys(KeySpec),
    /// Wait for a module key, then open it right of (`Row`) or below (`Column`) the focused module.
    Split(Axis),
    Zoom,
    Resize {
        dir: Direction,
        cells: u16,
    },
    Select(Direction),
    /// tmux `last-pane`: give the keys back to the module that held them before the one holding them
    /// now, and again to return (v1 picks, 2026-09-29). From real history only
    /// (`crate::layout::FocusHistory`), never from the layout's MRU list, which starts in tree order.
    SelectLast,
    /// tmux `select-pane -t :.+`: the next module on screen in tree order, wrapping
    /// (`crate::layout::next_on_screen`).
    SelectNext,
    Swap(SwapTarget),
    /// `Row` is tmux's `even-horizontal` (every module side by side); `Column` is `even-vertical`.
    Even(Axis),
    ModuleHide,
    /// tmux's `kill-pane`: close the module with the keys and end what runs in it, after a y/n
    /// (owner, 2026-09-26). `ModuleHide` stays an action of its own for a config that wants it.
    ModuleKill,
    Module(ModuleId),
    Tab(TabAction),
    Hint,
    PanelReload,
    PanelKeymap,
    /// tmux `command-prompt` (`prefix :`, owner decision #28, K16): show the chat with the keys and
    /// open the panel's `:` command line, which runs nothing. Before it was bound, an armed prefix
    /// swallowed the `:` and the letters typed after it ran as panel keys.
    PanelCommandLine,
    WindowImmersive,
    /// The pane with the keys only (spec §2.3: not bound by default).
    Text(TextChange),
    /// tmux `copy-mode` (`up: false`) / `copy-mode -u` (`up: true`): enter the terminal's read-only
    /// scrollback view, where the view is or scrolled up one page (spec §6.2, P5). Acts only when the
    /// terminal holds the keys; elsewhere it flashes the refusal (`shell`, Task 9).
    CopyMode {
        up: bool,
    },
}

/// An option's value as Lua gave it.
#[derive(Debug, Clone, PartialEq)]
pub enum OptValue {
    Int(i64),
    Bool(bool),
    Str(String),
    /// Anything else, by its Lua type name.
    Other(&'static str),
}

impl OptValue {
    fn kind(&self) -> &'static str {
        match self {
            OptValue::Int(_) => "a number",
            OptValue::Bool(_) => "a boolean",
            OptValue::Str(_) => "a string",
            OptValue::Other(name) => name,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parsed {
    pub action: Action,
    /// tmux's `-r`.
    pub repeatable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionError {
    Unknown { action: String },
    NoSuchModule { action: String },
    BadOption { option: String, why: String },
}

impl fmt::Display for ActionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ActionError::Unknown { action } => write!(f, "{action:?} is not an action"),
            ActionError::NoSuchModule { action } => write!(
                f,
                "{action:?} names no module (editor, agent, terminal, canvas, or a Lua panel's id)"
            ),
            ActionError::BadOption { option, why } => write!(f, "option {option:?} {why}"),
        }
    }
}

impl std::error::Error for ActionError {}

fn direction(text: &str) -> Option<Direction> {
    Some(match text {
        "left" => Direction::Left,
        "down" => Direction::Down,
        "up" => Direction::Up,
        "right" => Direction::Right,
        _ => return None,
    })
}

fn direction_name(dir: Direction) -> &'static str {
    match dir {
        Direction::Left => "left",
        Direction::Down => "down",
        Direction::Up => "up",
        Direction::Right => "right",
    }
}

#[derive(Default)]
struct Opts {
    cells: Option<i64>,
    n: Option<i64>,
    keys: Option<String>,
    repeatable: Option<bool>,
    up: Option<bool>,
}

fn bad(option: &str, why: impl Into<String>) -> ActionError {
    ActionError::BadOption {
        option: option.to_string(),
        why: why.into(),
    }
}

fn read_opts(opts: &[(String, OptValue)]) -> Result<Opts, ActionError> {
    let mut out = Opts::default();
    for (name, value) in opts {
        match (name.as_str(), value) {
            ("cells", OptValue::Int(i)) => out.cells = Some(*i),
            ("n", OptValue::Int(i)) => out.n = Some(*i),
            ("keys", OptValue::Str(s)) => out.keys = Some(s.clone()),
            ("repeatable", OptValue::Bool(b)) => out.repeatable = Some(*b),
            ("up", OptValue::Bool(b)) => out.up = Some(*b),
            ("cells" | "n", other) => return Err(bad(name, format!("must be a whole number, not {}", other.kind()))),
            ("keys", other) => return Err(bad(name, format!("must be one tmux key name, not {}", other.kind()))),
            ("repeatable" | "up", other) => {
                return Err(bad(name, format!("must be true or false, not {}", other.kind())))
            }
            _ => return Err(bad(name, "is not an option (cells, n, keys, repeatable, up)")),
        }
    }
    Ok(out)
}

/// `name` and its options, as `eitri.keymap.set` received them. `lua_panels` are the ids of the
/// Lua panels `init.lua` registered, which `module.<id>` may name (ruling 7).
pub fn parse(name: &str, opts: &[(String, OptValue)], lua_panels: &[String]) -> Result<Parsed, ActionError> {
    let o = read_opts(opts)?;
    let unknown = || ActionError::Unknown {
        action: name.to_string(),
    };
    // Which of `cells`/`n`/`keys` this action takes; `repeatable` is every action's.
    let (action, takes): (Action, &[&str]) = match name {
        "send-prefix" => (Action::SendPrefix, &[]),
        "send-keys" => {
            let text = o
                .keys
                .as_deref()
                .ok_or_else(|| bad("keys", "is required: one tmux key name, as { keys = \"C-l\" }"))?;
            let key = KeySpec::parse(text).map_err(|e| bad("keys", e.to_string()))?;
            (Action::SendKeys(key), &["keys"])
        }
        "split.right" => (Action::Split(Axis::Row), &[]),
        "split.below" => (Action::Split(Axis::Column), &[]),
        "zoom" => (Action::Zoom, &[]),
        // Explicit, since the `select.` fallback below reads only a direction.
        "select.last" => (Action::SelectLast, &[]),
        "select.next" => (Action::SelectNext, &[]),
        "swap.prev" => (Action::Swap(SwapTarget::Prev), &[]),
        "swap.next" => (Action::Swap(SwapTarget::Next), &[]),
        "layout.even-horizontal" => (Action::Even(Axis::Row), &[]),
        "layout.even-vertical" => (Action::Even(Axis::Column), &[]),
        "module.hide" => (Action::ModuleHide, &[]),
        "module.kill" => (Action::ModuleKill, &[]),
        "tab.new" => (Action::Tab(TabAction::New), &[]),
        "tab.next" => (Action::Tab(TabAction::Next), &[]),
        "tab.prev" => (Action::Tab(TabAction::Prev), &[]),
        "tab.last" => (Action::Tab(TabAction::Last), &[]),
        "tab.rename" => (Action::Tab(TabAction::Rename), &[]),
        "tab.close" => (Action::Tab(TabAction::Close), &[]),
        "tab.choose" => (Action::Tab(TabAction::Choose), &[]),
        "tab.info" => (Action::Tab(TabAction::Info), &[]),
        "tab.select" => {
            let n =
                o.n.ok_or_else(|| bad("n", "is required: the tab's number, as { n = 3 }"))?;
            let n = u8::try_from(n)
                .ok()
                .filter(|n| *n >= 1)
                .ok_or_else(|| bad("n", "must be 1 to 255"))?;
            (Action::Tab(TabAction::Select(n)), &["n"])
        }
        "hint" => (Action::Hint, &[]),
        "panel.reload" => (Action::PanelReload, &[]),
        "panel.keymap" => (Action::PanelKeymap, &[]),
        "panel.command-line" => (Action::PanelCommandLine, &[]),
        "window.immersive" => (Action::WindowImmersive, &[]),
        "text.larger" => (Action::Text(TextChange::Larger), &[]),
        "text.smaller" => (Action::Text(TextChange::Smaller), &[]),
        "text.reset" => (Action::Text(TextChange::Reset), &[]),
        "copy-mode" => (
            Action::CopyMode {
                up: o.up.unwrap_or(false),
            },
            &["up"],
        ),
        _ => {
            if let Some(dir) = name.strip_prefix("resize.") {
                let dir = direction(dir).ok_or_else(unknown)?;
                let cells = o.cells.unwrap_or(1);
                let cells = u16::try_from(cells)
                    .ok()
                    .filter(|c| (1..=500).contains(c))
                    .ok_or_else(|| bad("cells", "must be 1 to 500"))?;
                (Action::Resize { dir, cells }, &["cells"])
            } else if let Some(dir) = name.strip_prefix("select.") {
                (Action::Select(direction(dir).ok_or_else(unknown)?), &[])
            } else if let Some(dir) = name.strip_prefix("swap.") {
                (
                    Action::Swap(SwapTarget::Toward(direction(dir).ok_or_else(unknown)?)),
                    &[],
                )
            } else if let Some(module) = name.strip_prefix("module.") {
                let id = match module {
                    "editor" | "agent" | "terminal" | "canvas" => {
                        ModuleId::parse(module).expect("a built-in module name")
                    }
                    panel if lua_panels.iter().any(|p| p == panel) => ModuleId::lua(panel),
                    _ => {
                        return Err(ActionError::NoSuchModule {
                            action: name.to_string(),
                        })
                    }
                };
                (Action::Module(id), &[])
            } else {
                return Err(unknown());
            }
        }
    };
    for (given, option) in [
        (o.cells.is_some(), "cells"),
        (o.n.is_some(), "n"),
        (o.keys.is_some(), "keys"),
        (o.up.is_some(), "up"),
    ] {
        if given && !takes.contains(&option) {
            return Err(bad(option, format!("is not an option of {name}")));
        }
    }
    Ok(Parsed {
        action,
        repeatable: o.repeatable.unwrap_or(false),
    })
}

impl Action {
    /// The Lua name `eitri.keymap.set` takes, without options.
    pub fn name(&self) -> String {
        match self {
            Action::SendPrefix => "send-prefix".into(),
            Action::SendKeys(_) => "send-keys".into(),
            Action::Split(Axis::Row) => "split.right".into(),
            Action::Split(Axis::Column) => "split.below".into(),
            Action::Zoom => "zoom".into(),
            Action::Resize { dir, .. } => format!("resize.{}", direction_name(*dir)),
            Action::Select(dir) => format!("select.{}", direction_name(*dir)),
            Action::SelectLast => "select.last".into(),
            Action::SelectNext => "select.next".into(),
            Action::Swap(SwapTarget::Prev) => "swap.prev".into(),
            Action::Swap(SwapTarget::Next) => "swap.next".into(),
            Action::Swap(SwapTarget::Toward(dir)) => format!("swap.{}", direction_name(*dir)),
            Action::Even(Axis::Row) => "layout.even-horizontal".into(),
            Action::Even(Axis::Column) => "layout.even-vertical".into(),
            Action::ModuleHide => "module.hide".into(),
            Action::ModuleKill => "module.kill".into(),
            Action::Module(id) => format!("module.{}", id.as_str().trim_start_matches("lua:")),
            Action::Tab(tab) => match tab {
                TabAction::New => "tab.new",
                TabAction::Next => "tab.next",
                TabAction::Prev => "tab.prev",
                TabAction::Last => "tab.last",
                TabAction::Select(_) => "tab.select",
                TabAction::Rename => "tab.rename",
                TabAction::Close => "tab.close",
                TabAction::CloseOthers => "tab.close-others",
                TabAction::Choose => "tab.choose",
                TabAction::Info => "tab.info",
            }
            .into(),
            Action::Hint => "hint".into(),
            Action::PanelReload => "panel.reload".into(),
            Action::PanelKeymap => "panel.keymap".into(),
            Action::PanelCommandLine => "panel.command-line".into(),
            Action::WindowImmersive => "window.immersive".into(),
            Action::Text(TextChange::Larger) => "text.larger".into(),
            Action::Text(TextChange::Smaller) => "text.smaller".into(),
            Action::Text(TextChange::Reset) => "text.reset".into(),
            Action::CopyMode { .. } => "copy-mode".into(),
        }
    }

    /// The options that, with [`Action::name`], parse back to this action.
    pub fn options(&self) -> Vec<(String, OptValue)> {
        match self {
            Action::SendKeys(key) => vec![("keys".into(), OptValue::Str(key.to_string()))],
            Action::Resize { cells, .. } => vec![("cells".into(), OptValue::Int(i64::from(*cells)))],
            Action::Tab(TabAction::Select(n)) => vec![("n".into(), OptValue::Int(i64::from(*n)))],
            Action::CopyMode { up: true } => vec![("up".into(), OptValue::Bool(true))],
            _ => vec![],
        }
    }

    /// The `?` overlay's description. `prefix` is the prefix as a person reads it (`Ctrl+b`);
    /// `module_keys` the module keys a split key takes (`e / a / t`).
    pub fn describe(&self, prefix: &str, module_keys: &str) -> String {
        match self {
            Action::SendPrefix => format!("Send {prefix} itself to the pane with the keys"),
            Action::SendKeys(key) => format!("Send {} to the pane with the keys", key.human()),
            Action::Split(Axis::Row) => {
                format!("Then a module key ({module_keys}): open it right of this one, or move it there")
            }
            Action::Split(Axis::Column) => {
                format!("Then a module key ({module_keys}): open it below this one, or move it there")
            }
            Action::Zoom => "Zoom this module, or restore".into(),
            Action::Resize { cells: 1, .. } => "Move the nearest divider 1 cell that way".into(),
            Action::Resize { cells, .. } => format!("Move the nearest divider {cells} cells that way"),
            Action::Select(_) => "Move the keys to the module that way".into(),
            Action::SelectLast => "Move the keys back to the module that had them before (again: return)".into(),
            Action::SelectNext => "Move the keys to the next module on screen, in tree order".into(),
            Action::Swap(SwapTarget::Prev | SwapTarget::Next) => {
                "Swap this module with the previous / next one on screen".into()
            }
            Action::Swap(SwapTarget::Toward(_)) => "Swap this module with the one that way".into(),
            Action::Even(Axis::Row) => "Every module in one row, at equal sizes".into(),
            Action::Even(Axis::Column) => "Every module in one column, at equal sizes".into(),
            Action::ModuleHide => "Hide this module; it keeps running (not the last one on screen)".into(),
            // Since 2026-09-26 the last module's kill closes the window (`KillScope::Window`), as
            // tmux's `kill-pane` on a window's last pane kills the window; the hide above still
            // refuses the last module, so only this row changed (v1 polish item 9).
            Action::ModuleKill => {
                "Close this module and end what runs in it, after y/n (the last one on screen closes Eitri)".into()
            }
            Action::Module(id) => match id.kind() {
                ModuleKind::Editor => "Editor: show and focus it, or hide it when it has the keys".into(),
                ModuleKind::Agent => "Agent: show and focus it, or hide it when it has the keys".into(),
                ModuleKind::Terminal => "Terminal: show and focus it, or hide it when it has the keys".into(),
                ModuleKind::Canvas => "Canvas (not yet)".into(),
                ModuleKind::LuaWebview => format!(
                    "{}: show and focus it, or hide it when it has the keys",
                    id.as_str().trim_start_matches("lua:")
                ),
            },
            Action::Tab(tab) => match tab {
                TabAction::New => "New session tab",
                TabAction::Next => "Next session tab",
                TabAction::Prev => "Previous session tab",
                TabAction::Last => "The session tab you were on last",
                TabAction::Select(_) => "Select that session tab",
                TabAction::Rename => "Rename this session tab",
                TabAction::Close => "Close this session tab, after y/n",
                TabAction::CloseOthers => "Close every other session tab, after y/n",
                TabAction::Choose => "Choose a session: open tabs, then ones to resume",
                TabAction::Info => "This session's details",
            }
            .into(),
            Action::Hint => "HINT: jump anywhere in the window".into(),
            Action::PanelReload => "Reload the agent panel (the session keeps running)".into(),
            Action::PanelKeymap => "Show the chat and this list".into(),
            Action::PanelCommandLine => "Show the chat and its : command line (runs nothing)".into(),
            Action::WindowImmersive => "Immersive: fullscreen without the top bar".into(),
            Action::Text(TextChange::Larger) => "Text size larger, the pane with the keys only".into(),
            Action::Text(TextChange::Smaller) => "Text size smaller, the pane with the keys only".into(),
            Action::Text(TextChange::Reset) => "Text size reset, the pane with the keys only".into(),
            Action::CopyMode { .. } => "Scroll the terminal back (copy mode)".into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keymap::table::Keymap;

    #[test]
    fn every_default_action_round_trips_through_its_lua_name() {
        for binding in Keymap::defaults().bindings() {
            let parsed = parse(&binding.action.name(), &binding.action.options(), &[])
                .unwrap_or_else(|e| panic!("{}: {e}", binding.action.name()));
            assert_eq!(parsed.action, binding.action);
            assert!(!parsed.repeatable, "repeatable comes only from the option");
        }
    }

    #[test]
    fn options_are_checked_by_name_type_and_action() {
        let opt = |k: &str, v: OptValue| vec![(k.to_string(), v)];
        assert_eq!(
            parse("resize.left", &opt("cells", OptValue::Int(5)), &[])
                .unwrap()
                .action,
            Action::Resize {
                dir: Direction::Left,
                cells: 5
            }
        );
        assert_eq!(
            parse("resize.left", &[], &[]).unwrap().action,
            Action::Resize {
                dir: Direction::Left,
                cells: 1
            },
            "tmux's resize-pane default is one cell"
        );
        assert!(
            parse("zoom", &opt("repeatable", OptValue::Bool(true)), &[])
                .unwrap()
                .repeatable
        );
        for (name, opts, needle) in [
            ("zoom", opt("cells", OptValue::Int(5)), "is not an option of zoom"),
            ("resize.left", opt("cellz", OptValue::Int(5)), "is not an option"),
            ("resize.left", opt("cells", OptValue::Str("5".into())), "whole number"),
            ("resize.left", opt("cells", OptValue::Int(0)), "1 to 500"),
            ("zoom", opt("repeatable", OptValue::Other("table")), "true or false"),
            ("send-keys", vec![], "is required"),
            (
                "send-keys",
                opt("keys", OptValue::Str("S-h".into())),
                "shifted character",
            ),
            ("tab.select", vec![], "is required"),
            ("tab.select", opt("n", OptValue::Int(0)), "1 to 255"),
        ] {
            let err = parse(name, &opts, &[]).expect_err(name).to_string();
            assert!(err.contains(needle), "{name} {opts:?}: {err}");
        }
    }

    #[test]
    fn an_unknown_action_and_an_unknown_module_are_named() {
        assert!(parse("tab.lastt", &[], &[])
            .unwrap_err()
            .to_string()
            .contains("tab.lastt"));
        assert!(parse("resize.sideways", &[], &[])
            .unwrap_err()
            .to_string()
            .contains("resize.sideways"));
        assert_eq!(
            parse("module.notes", &[], &["notes".to_string()]).unwrap().action,
            Action::Module(ModuleId::lua("notes"))
        );
        assert_eq!(
            parse("module.canvas", &[], &[]).unwrap().action,
            Action::Module(ModuleId::parse("canvas").unwrap())
        );
        let err = parse("module.nope", &[], &["notes".to_string()])
            .unwrap_err()
            .to_string();
        assert!(err.contains("module.nope") && err.contains("Lua panel"), "{err}");
    }

    /// v1 polish item 9: the `?` overlay said the kill refused the last module on screen; since
    /// 2026-09-26 that kill closes the window instead. The hide still refuses it.
    #[test]
    fn the_kill_row_says_the_last_module_closes_eitri() {
        let kill = Action::ModuleKill.describe("Ctrl+b", "e / a / t");
        assert!(kill.contains("the last one on screen closes Eitri"), "{kill}");
        assert!(!kill.contains("not the last one"), "{kill}");
        assert!(Action::ModuleHide
            .describe("Ctrl+b", "e / a / t")
            .contains("not the last one on screen"));
    }

    /// v1 picks (2026-09-29): tmux's `last-pane` and `select-pane -t :.+`. `parse`'s `select.`
    /// fallback reads only a direction, so both names need arms of their own -- and the fallback must
    /// keep reading the directions.
    #[test]
    fn select_last_and_next_parse_by_name_and_take_no_options() {
        assert_eq!(parse("select.last", &[], &[]).unwrap().action, Action::SelectLast);
        assert_eq!(parse("select.next", &[], &[]).unwrap().action, Action::SelectNext);
        assert_eq!(
            parse("select.left", &[], &[]).unwrap().action,
            Action::Select(Direction::Left)
        );
        for name in ["select.last", "select.next"] {
            let err = parse(name, &[("cells".into(), OptValue::Int(2))], &[])
                .unwrap_err()
                .to_string();
            assert!(err.contains(&format!("is not an option of {name}")), "{err}");
        }
    }

    #[test]
    fn send_keys_parses_its_key() {
        let parsed = parse("send-keys", &[("keys".into(), OptValue::Str("C-l".into()))], &[]).unwrap();
        assert_eq!(parsed.action, Action::SendKeys(KeySpec::parse("C-l").unwrap()));
    }

    #[test]
    fn copy_mode_defaults_up_to_false_and_round_trips_through_name_and_options() {
        assert_eq!(
            parse("copy-mode", &[], &[]).unwrap().action,
            Action::CopyMode { up: false }
        );
        for up in [false, true] {
            let action = Action::CopyMode { up };
            let parsed = parse(&action.name(), &action.options(), &[]).unwrap();
            assert_eq!(parsed.action, action);
        }
        assert_eq!(
            parse("copy-mode", &[("up".into(), OptValue::Bool(true))], &[])
                .unwrap()
                .action,
            Action::CopyMode { up: true }
        );
        assert_eq!(
            Action::CopyMode { up: false }.describe("Ctrl+b", "e / a / t"),
            "Scroll the terminal back (copy mode)"
        );
        assert_eq!(
            Action::CopyMode { up: true }.describe("Ctrl+b", "e / a / t"),
            "Scroll the terminal back (copy mode)"
        );
    }
}
