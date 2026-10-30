//! The closed set of things a key can do, named the way tmux names its commands. A new
//! action is a change to this enum and to the exhaustive `match`es over it
//! (`crate::wm::verbs::family` among them), never a plugin -- the same discipline as `ModuleKind`.

use std::fmt;

use super::key::KeySpec;
use crate::layout::{Axis, Direction, ModuleId, ModuleKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwapTarget {
    /// tmux `swap-pane -U`: the previous module on screen, in tree order, wrapping.
    Prev,
    /// tmux `swap-pane -D`.
    Next,
    /// The neighbour that way, as `Ctrl+h/j/k/l` would choose it (prefix `H/J/K/L`).
    Toward(Direction),
}

/// Session tabs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabAction {
    New,
    Next,
    Prev,
    Last,
    Select(u8),
    Rename,
    Close,
    /// Close every other tab (LazyVim "Delete Other Buffers", `keymaps.lua:43-45`). Panel-only for
    /// now: not bound after the prefix.
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SplitCwd {
    #[default]
    Start,
    Pane,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionRule {
    Takes,
    Refuses,
    Resets,
}

/// What an action does while the module with the keys is floating.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FloatRule {
    /// Acts on the tiled tree; every float stays where it is.
    Tiles,
    /// Refused: a float has no split.
    Refuses,
    /// Acts on the float itself.
    Acts,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// i3 `move left|down|up|right`.
    Move(Direction),
    ContainerTabbed,
    ContainerStacked,
    FocusParent,
    FocusChild,
    InstanceNew {
        kind: ModuleKind,
    },
    /// i3 `floating toggle`.
    FloatToggle,
    /// i3 `focus mode_toggle`: between the floating and the tiled layer.
    FocusModeToggle,
    /// i3 `scratchpad show`; with a module, that module only.
    ScratchpadShow {
        module: Option<ModuleId>,
    },
    /// i3 `move scratchpad`.
    ScratchpadMove,
    /// tmux `switch-client -n` (`forward: true`) / `-p`.
    WorkspaceStep {
        forward: bool,
    },
    /// tmux `switch-client -l`.
    WorkspaceLast,
    /// tmux `choose-tree -s`: the host draws the chooser, then starts the prefix wait.
    WorkspaceChoose,
    /// tmux's `rename-session` prompt.
    WorkspaceRename,
    WorkspaceSwitch {
        name: String,
    },
    WorkspaceNew,
    /// tmux `break-pane`: the base view's active tab out into a chat view of its own; pressed in that view,
    /// the tab joins the base view again.
    TabBreak,
    /// Open this project's second window, or raise it when it is open.
    WindowNew,
    /// Give the keys to the other window of the pair.
    WindowOther,
    /// Move the module with the keys, or `module`, to the other window (a module the other window holds comes
    /// here, as tmux's `join-pane -s`); with `follow`, the keys go with it.
    MoveToWindow {
        module: Option<ModuleId>,
        follow: bool,
    },
    /// Move the current workspace to the other window.
    WorkspaceToWindow {
        follow: bool,
    },
    /// Swap this window's current workspace with the other window's.
    WorkspaceSwapWindow,
    /// vim `<C-w>H/J/K/L`: move the module to the window's far edge.
    MoveFar(Direction),
    /// i3 `layout toggle split`: switch the containing split's axis.
    ToggleSplit,
    /// tmux `rotate-window` / `-D`: rotate the modules on screen.
    Rotate {
        down: bool,
    },
    /// Mirror the layout along an axis; tmux and vim have no equivalent command.
    Flip(Axis),
    /// tmux `select-layout main-horizontal`.
    MainHorizontal,
    /// tmux `select-layout main-vertical`.
    MainVertical,
    /// tmux `select-layout tiled`.
    Tiled,
    /// tmux `next-layout` / `previous-layout`.
    CycleLayout {
        forward: bool,
    },
    /// tmux `select-layout -E`.
    Spread,
    /// Undo the last layout change; tmux and vim have no layout undo command.
    LayoutUndo,
    /// tmux `select-pane -m`: toggle the mark on this module.
    MarkToggle,
    /// tmux `select-pane -M`: clear the mark.
    MarkClear,
    /// tmux `swap-pane` with no flags: swap with the marked module.
    SwapMarked,
    /// tmux `display-panes`: number the modules for a digit to select.
    SelectNumber,

    /// Hand the pane with the keys the prefix chord itself.
    SendPrefix,
    /// Hand the pane with the keys this key.
    SendKeys(KeySpec),
    /// Wait for a module key, then open it right of (`Row`) or below (`Column`) the focused module.
    Split {
        axis: Axis,
        cwd: SplitCwd,
    },
    Zoom,
    Resize {
        dir: Direction,
        cells: u16,
    },
    Select(Direction),
    /// tmux `last-pane`: give the keys back to the module that held them before the one holding them
    /// now, and again to return. From real history only
    /// (`crate::layout::FocusHistory`), never from the layout's MRU list, which starts in tree order.
    SelectLast,
    /// tmux `select-pane -t :.+`: the next module on screen in tree order, wrapping
    /// (`crate::layout::next_on_screen`).
    SelectNext,
    Swap(SwapTarget),
    /// `Row` is tmux's `even-horizontal` (every module side by side); `Column` is `even-vertical`.
    Even(Axis),
    ModuleHide,
    /// Wait for a module key, then hide that module wherever the keys are; on its own it hides nothing.
    ModuleHideByKey,
    /// tmux's `kill-pane`: close the module with the keys and end what runs in it, after a y/n,
    /// since what it ends cannot be brought back. `ModuleHide` stays an action of its own for a
    /// config that wants it.
    ModuleKill,
    Module(ModuleId),
    Tab(TabAction),
    Hint,
    PanelReload,
    PanelKeymap,
    /// tmux `command-prompt` (`prefix :`): show the chat with the keys and
    /// open the panel's `:` command line, which runs nothing. Without this binding, an armed prefix
    /// swallows the `:` and the letters typed after it run as panel keys.
    PanelCommandLine,
    WindowImmersive,
    /// The pane with the keys only (not bound by default).
    Text(TextChange),
    /// tmux `copy-mode` (`up: false`) / `copy-mode -u` (`up: true`): enter the terminal's read-only
    /// scrollback view, where the view is or scrolled up one page. Acts only when the
    /// terminal holds the keys; elsewhere it flashes the refusal (`shell`).
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
                "{action:?} names no module (editor, agent, terminal, terminal#2..#9, agent#2..#9, canvas, or a Lua panel's id)"
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
    follow: Option<bool>,
    cwd: Option<String>,
    kind: Option<String>,
    module: Option<String>,
    name: Option<String>,
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
            ("cwd", OptValue::Str(s)) => out.cwd = Some(s.clone()),
            ("kind", OptValue::Str(s)) => out.kind = Some(s.clone()),
            ("module", OptValue::Str(s)) => out.module = Some(s.clone()),
            ("name", OptValue::Str(s)) => out.name = Some(s.clone()),
            ("keys", OptValue::Str(s)) => out.keys = Some(s.clone()),
            ("repeatable", OptValue::Bool(b)) => out.repeatable = Some(*b),
            ("up", OptValue::Bool(b)) => out.up = Some(*b),
            ("follow", OptValue::Bool(b)) => out.follow = Some(*b),
            ("cells" | "n", other) => return Err(bad(name, format!("must be a whole number, not {}", other.kind()))),
            ("cwd" | "kind" | "module" | "name", other) => {
                return Err(bad(name, format!("must be a string, not {}", other.kind())))
            }
            ("keys", other) => return Err(bad(name, format!("must be one tmux key name, not {}", other.kind()))),
            ("repeatable" | "up" | "follow", other) => {
                return Err(bad(name, format!("must be true or false, not {}", other.kind())))
            }
            _ => {
                return Err(bad(
                    name,
                    "is not an option (cells, n, keys, repeatable, up, cwd, kind, module, name, follow)",
                ))
            }
        }
    }
    Ok(out)
}

/// `name` and its options, as `eitri.keymap.set` received them. `lua_panels` are the ids of the
/// Lua panels `init.lua` registered, which `module.<id>` may name.
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
        "split.right" | "split.below" => {
            let cwd = match o.cwd.as_deref().unwrap_or("start") {
                "start" => SplitCwd::Start,
                "pane" => SplitCwd::Pane,
                value => return Err(bad("cwd", format!("must be \"start\" or \"pane\", not {value:?}"))),
            };
            (
                Action::Split {
                    axis: if name == "split.right" { Axis::Row } else { Axis::Column },
                    cwd,
                },
                &["cwd"],
            )
        }
        "container.tabbed" => (Action::ContainerTabbed, &[]),
        "container.stacked" => (Action::ContainerStacked, &[]),
        "focus.parent" => (Action::FocusParent, &[]),
        "focus.child" => (Action::FocusChild, &[]),
        "instance.new" => {
            let value = o.kind.as_deref().ok_or_else(|| {
                bad(
                    "kind",
                    "is required: \"terminal\" or \"agent\", as { kind = \"terminal\" }",
                )
            })?;
            let kind = match value {
                "terminal" => ModuleKind::Terminal,
                "agent" => ModuleKind::Agent,
                _ => return Err(bad("kind", format!("must be \"terminal\" or \"agent\", not {value:?}"))),
            };
            (Action::InstanceNew { kind }, &["kind"])
        }
        "float.toggle" => (Action::FloatToggle, &[]),
        "focus.mode-toggle" => (Action::FocusModeToggle, &[]),
        "scratchpad.move" => (Action::ScratchpadMove, &[]),
        "workspace.prev" => (Action::WorkspaceStep { forward: false }, &[]),
        "workspace.next" => (Action::WorkspaceStep { forward: true }, &[]),
        "workspace.last" => (Action::WorkspaceLast, &[]),
        "workspace.choose" => (Action::WorkspaceChoose, &[]),
        "workspace.rename" => (Action::WorkspaceRename, &[]),
        "workspace.new" => (Action::WorkspaceNew, &[]),
        "scratchpad.show" => {
            let module = o
                .module
                .as_deref()
                .map(ModuleId::parse)
                .transpose()
                .map_err(|err| bad("module", format!("must be a module id: {err}")))?;
            (Action::ScratchpadShow { module }, &["module"])
        }
        "workspace.switch" => {
            let value = o
                .name
                .as_deref()
                .ok_or_else(|| bad("name", "is required: the workspace's name, as { name = \"study\" }"))?;
            crate::layout::Workspaces::valid_name(value).map_err(|_| {
                bad(
                    "name",
                    format!("must be 1 to 32 characters with no control characters, not {value:?}"),
                )
            })?;
            (
                Action::WorkspaceSwitch {
                    name: value.to_string(),
                },
                &["name"],
            )
        }
        "window.new" => (Action::WindowNew, &[]),
        "window.other" => (Action::WindowOther, &[]),
        "move.to-window" => {
            let module = o
                .module
                .as_deref()
                .map(ModuleId::parse)
                .transpose()
                .map_err(|err| bad("module", format!("must be a module id: {err}")))?;
            (
                Action::MoveToWindow {
                    module,
                    follow: o.follow.unwrap_or(false),
                },
                &["module", "follow"],
            )
        }
        "workspace.to-window" => (
            Action::WorkspaceToWindow {
                follow: o.follow.unwrap_or(false),
            },
            &["follow"],
        ),
        "workspace.swap-window" => (Action::WorkspaceSwapWindow, &[]),
        "zoom" => (Action::Zoom, &[]),
        "container.toggle-split" => (Action::ToggleSplit, &[]),
        "rotate.up" => (Action::Rotate { down: false }, &[]),
        "rotate.down" => (Action::Rotate { down: true }, &[]),
        "layout.flip-horizontal" => (Action::Flip(Axis::Row), &[]),
        "layout.flip-vertical" => (Action::Flip(Axis::Column), &[]),
        "layout.main-horizontal" => (Action::MainHorizontal, &[]),
        "layout.main-vertical" => (Action::MainVertical, &[]),
        "layout.tiled" => (Action::Tiled, &[]),
        "layout.next" => (Action::CycleLayout { forward: true }, &[]),
        "layout.previous" => (Action::CycleLayout { forward: false }, &[]),
        "layout.spread" => (Action::Spread, &[]),
        "layout.undo" => (Action::LayoutUndo, &[]),
        "mark.toggle" => (Action::MarkToggle, &[]),
        "mark.clear" => (Action::MarkClear, &[]),
        "swap.marked" => (Action::SwapMarked, &[]),
        "select.number" => (Action::SelectNumber, &[]),

        // Explicit, since the `select.` fallback below reads only a direction.
        "select.last" => (Action::SelectLast, &[]),
        "select.next" => (Action::SelectNext, &[]),
        "swap.prev" => (Action::Swap(SwapTarget::Prev), &[]),
        "swap.next" => (Action::Swap(SwapTarget::Next), &[]),
        "layout.even-horizontal" => (Action::Even(Axis::Row), &[]),
        "layout.even-vertical" => (Action::Even(Axis::Column), &[]),
        "module.hide" => (Action::ModuleHide, &[]),
        "module.hide-by-key" => (Action::ModuleHideByKey, &[]),
        "module.kill" => (Action::ModuleKill, &[]),
        "tab.new" => (Action::Tab(TabAction::New), &[]),
        "tab.next" => (Action::Tab(TabAction::Next), &[]),
        "tab.prev" => (Action::Tab(TabAction::Prev), &[]),
        "tab.last" => (Action::Tab(TabAction::Last), &[]),
        "tab.rename" => (Action::Tab(TabAction::Rename), &[]),
        "tab.close" => (Action::Tab(TabAction::Close), &[]),
        "tab.choose" => (Action::Tab(TabAction::Choose), &[]),
        "tab.info" => (Action::Tab(TabAction::Info), &[]),
        "tab.break" => (Action::TabBreak, &[]),
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
            if let Some(dir) = name.strip_prefix("move.far-") {
                (Action::MoveFar(direction(dir).ok_or_else(unknown)?), &[])
            } else if let Some(dir) = name.strip_prefix("move.") {
                (Action::Move(direction(dir).ok_or_else(unknown)?), &[])
            } else if let Some(dir) = name.strip_prefix("resize.") {
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
                    instance if ModuleId::parse(instance).is_ok_and(|id| id.kind().has_instances()) => {
                        ModuleId::parse(instance).expect("a canonical instance name")
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
        (o.cwd.is_some(), "cwd"),
        (o.kind.is_some(), "kind"),
        (o.module.is_some(), "module"),
        (o.name.is_some(), "name"),
        (o.follow.is_some(), "follow"),
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
    pub fn on_a_float(&self) -> FloatRule {
        match self {
            Action::Even(_)
            | Action::MainHorizontal
            | Action::MainVertical
            | Action::Tiled
            | Action::CycleLayout { .. }
            | Action::Rotate { .. }
            | Action::Flip(_)
            | Action::LayoutUndo => FloatRule::Tiles,
            Action::Spread
            | Action::ContainerTabbed
            | Action::ContainerStacked
            | Action::ToggleSplit
            | Action::Swap(_)
            | Action::FocusParent
            | Action::FocusChild => FloatRule::Refuses,
            Action::Move(_)
            | Action::MoveFar(_)
            | Action::Zoom
            | Action::FloatToggle
            | Action::ScratchpadMove
            | Action::ModuleKill
            | Action::ModuleHide
            | Action::ModuleHideByKey
            | Action::SwapMarked
            | Action::MarkToggle
            | Action::SendPrefix
            | Action::SendKeys(_)
            | Action::Split { .. }
            | Action::Resize { .. }
            | Action::Select(_)
            | Action::SelectLast
            | Action::SelectNext
            | Action::Module(_)
            | Action::InstanceNew { .. }
            | Action::Tab(_)
            | Action::Hint
            | Action::PanelReload
            | Action::PanelKeymap
            | Action::PanelCommandLine
            | Action::WindowImmersive
            | Action::Text(_)
            | Action::CopyMode { .. }
            | Action::MarkClear
            | Action::SelectNumber
            | Action::FocusModeToggle
            | Action::ScratchpadShow { .. }
            | Action::WorkspaceStep { .. }
            | Action::WorkspaceLast
            | Action::WorkspaceChoose
            | Action::WorkspaceRename
            | Action::WorkspaceSwitch { .. }
            | Action::WorkspaceNew
            | Action::TabBreak
            | Action::WindowNew
            | Action::WindowOther
            | Action::MoveToWindow { .. }
            | Action::WorkspaceToWindow { .. }
            | Action::WorkspaceSwapWindow => FloatRule::Acts,
        }
    }

    pub fn selection_rule(&self) -> SelectionRule {
        match self {
            Action::Move(_)
            | Action::MoveFar(_)
            | Action::Zoom
            | Action::ContainerTabbed
            | Action::ContainerStacked
            | Action::ToggleSplit
            | Action::Flip(_)
            | Action::Spread
            | Action::FocusParent
            | Action::FocusChild => SelectionRule::Takes,
            Action::FloatToggle
            | Action::ScratchpadMove
            | Action::ModuleKill
            | Action::ModuleHide
            | Action::ModuleHideByKey
            | Action::Swap(_)
            | Action::SwapMarked
            | Action::MarkToggle
            | Action::MoveToWindow { .. } => SelectionRule::Refuses,
            Action::SendPrefix
            | Action::SendKeys(_)
            | Action::Split { .. }
            | Action::Resize { .. }
            | Action::Select(_)
            | Action::SelectLast
            | Action::SelectNext
            | Action::Even(_)
            | Action::Module(_)
            | Action::InstanceNew { .. }
            | Action::Tab(_)
            | Action::Hint
            | Action::PanelReload
            | Action::PanelKeymap
            | Action::PanelCommandLine
            | Action::WindowImmersive
            | Action::Text(_)
            | Action::CopyMode { .. }
            | Action::Rotate { .. }
            | Action::MainHorizontal
            | Action::MainVertical
            | Action::Tiled
            | Action::CycleLayout { .. }
            | Action::LayoutUndo
            | Action::MarkClear
            | Action::SelectNumber
            | Action::FocusModeToggle
            | Action::ScratchpadShow { .. }
            | Action::WorkspaceStep { .. }
            | Action::WorkspaceLast
            | Action::WorkspaceChoose
            | Action::WorkspaceRename
            | Action::WorkspaceSwitch { .. }
            | Action::WorkspaceNew
            | Action::TabBreak
            | Action::WindowNew
            | Action::WindowOther
            | Action::WorkspaceToWindow { .. }
            | Action::WorkspaceSwapWindow => SelectionRule::Resets,
        }
    }

    /// What `eitri.layout.run` accepts; every new action must decide here whether it is layout.
    pub fn is_layout(&self) -> bool {
        match self {
            Action::Move(_)
            | Action::ContainerTabbed
            | Action::ContainerStacked
            | Action::FocusParent
            | Action::FocusChild
            | Action::InstanceNew { .. }
            | Action::Split { .. }
            | Action::Zoom
            | Action::Resize { .. }
            | Action::Select(_)
            | Action::SelectLast
            | Action::SelectNext
            | Action::Swap(_)
            | Action::Even(_)
            | Action::ModuleHide
            | Action::ModuleHideByKey
            | Action::ModuleKill
            | Action::Module(_)
            | Action::MoveFar(_)
            | Action::ToggleSplit
            | Action::Rotate { .. }
            | Action::Flip(_)
            | Action::MainHorizontal
            | Action::MainVertical
            | Action::Tiled
            | Action::CycleLayout { .. }
            | Action::Spread
            | Action::LayoutUndo
            | Action::MarkToggle
            | Action::MarkClear
            | Action::SwapMarked
            | Action::SelectNumber
            | Action::FloatToggle
            | Action::FocusModeToggle
            | Action::ScratchpadShow { .. }
            | Action::ScratchpadMove
            | Action::WorkspaceStep { .. }
            | Action::WorkspaceLast
            | Action::WorkspaceChoose
            | Action::WorkspaceRename
            | Action::WorkspaceSwitch { .. }
            | Action::WorkspaceNew
            | Action::TabBreak
            | Action::WindowNew
            | Action::WindowOther
            | Action::MoveToWindow { .. }
            | Action::WorkspaceToWindow { .. }
            | Action::WorkspaceSwapWindow => true,
            Action::SendPrefix
            | Action::SendKeys(_)
            | Action::Tab(_)
            | Action::Hint
            | Action::PanelReload
            | Action::PanelKeymap
            | Action::PanelCommandLine
            | Action::WindowImmersive
            | Action::Text(_)
            | Action::CopyMode { .. } => false,
        }
    }

    /// The Lua name `eitri.keymap.set` takes, without options.
    pub fn name(&self) -> String {
        match self {
            Action::SendPrefix => "send-prefix".into(),
            Action::SendKeys(_) => "send-keys".into(),
            Action::Split { axis: Axis::Row, .. } => "split.right".into(),
            Action::Split { axis: Axis::Column, .. } => "split.below".into(),
            Action::Move(dir) => format!("move.{}", direction_name(*dir)),
            Action::ContainerTabbed => "container.tabbed".into(),
            Action::ContainerStacked => "container.stacked".into(),
            Action::FocusParent => "focus.parent".into(),
            Action::FocusChild => "focus.child".into(),
            Action::InstanceNew { .. } => "instance.new".into(),
            Action::FloatToggle => "float.toggle".into(),
            Action::FocusModeToggle => "focus.mode-toggle".into(),
            Action::ScratchpadShow { .. } => "scratchpad.show".into(),
            Action::ScratchpadMove => "scratchpad.move".into(),
            Action::WorkspaceStep { forward: false } => "workspace.prev".into(),
            Action::WorkspaceStep { forward: true } => "workspace.next".into(),
            Action::WorkspaceLast => "workspace.last".into(),
            Action::WorkspaceChoose => "workspace.choose".into(),
            Action::WorkspaceRename => "workspace.rename".into(),
            Action::WorkspaceSwitch { .. } => "workspace.switch".into(),
            Action::WorkspaceNew => "workspace.new".into(),
            Action::TabBreak => "tab.break".into(),
            Action::WindowNew => "window.new".into(),
            Action::WindowOther => "window.other".into(),
            Action::MoveToWindow { .. } => "move.to-window".into(),
            Action::WorkspaceToWindow { .. } => "workspace.to-window".into(),
            Action::WorkspaceSwapWindow => "workspace.swap-window".into(),
            Action::Zoom => "zoom".into(),
            Action::MoveFar(dir) => format!("move.far-{}", direction_name(*dir)),
            Action::ToggleSplit => "container.toggle-split".into(),
            Action::Rotate { down: false } => "rotate.up".into(),
            Action::Rotate { down: true } => "rotate.down".into(),
            Action::Flip(Axis::Row) => "layout.flip-horizontal".into(),
            Action::Flip(Axis::Column) => "layout.flip-vertical".into(),
            Action::MainHorizontal => "layout.main-horizontal".into(),
            Action::MainVertical => "layout.main-vertical".into(),
            Action::Tiled => "layout.tiled".into(),
            Action::CycleLayout { forward: true } => "layout.next".into(),
            Action::CycleLayout { forward: false } => "layout.previous".into(),
            Action::Spread => "layout.spread".into(),
            Action::LayoutUndo => "layout.undo".into(),
            Action::MarkToggle => "mark.toggle".into(),
            Action::MarkClear => "mark.clear".into(),
            Action::SwapMarked => "swap.marked".into(),
            Action::SelectNumber => "select.number".into(),

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
            Action::ModuleHideByKey => "module.hide-by-key".into(),
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
            Action::ScratchpadShow { module: Some(id) } => vec![("module".into(), OptValue::Str(id.as_str().into()))],
            Action::WorkspaceSwitch { name } => vec![("name".into(), OptValue::Str(name.clone()))],
            Action::MoveToWindow { module, follow } => {
                let mut out = Vec::new();
                if let Some(id) = module {
                    out.push(("module".into(), OptValue::Str(id.as_str().into())));
                }
                if *follow {
                    out.push(("follow".into(), OptValue::Bool(true)));
                }
                out
            }
            Action::WorkspaceToWindow { follow: true } => vec![("follow".into(), OptValue::Bool(true))],
            Action::Split {
                cwd: SplitCwd::Pane, ..
            } => vec![("cwd".into(), OptValue::Str("pane".into()))],
            Action::InstanceNew { kind } => vec![(
                "kind".into(),
                OptValue::Str(
                    match kind {
                        ModuleKind::Terminal => "terminal",
                        ModuleKind::Agent => "agent",
                        ModuleKind::Editor => "editor",
                        ModuleKind::Canvas => "canvas",
                        ModuleKind::LuaWebview => "lua",
                    }
                    .into(),
                ),
            )],
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
            Action::TabBreak => {
                "Break this session tab out into a chat view of its own; in that view, join it back".into()
            }
            Action::WindowNew => "Open this project's second window, or raise it".into(),
            Action::WindowOther => "Give the keys to the other window".into(),
            Action::MoveToWindow { module: None, .. } => "Move this module to the other window".into(),
            Action::MoveToWindow { module: Some(id), .. } => {
                format!("Move {id} to the other window, or bring it here when the other window holds it")
            }
            Action::WorkspaceToWindow { .. } => "Move this workspace to the other window".into(),
            Action::WorkspaceSwapWindow => "Swap this workspace with the other window's".into(),
            Action::FloatToggle => "Float this module over the others, or put a floating one back in the layout".into(),
            Action::FocusModeToggle => {
                "Give the keys to the most recent module of the other layer, floating or tiled".into()
            }
            Action::ScratchpadShow { module: None } => {
                "Show the scratchpad's module, or put it away again; pressed again, the next".into()
            }
            Action::ScratchpadMove => "Send this module to the scratchpad".into(),
            Action::WorkspaceStep { forward: false } => "Switch to the previous workspace".into(),
            Action::WorkspaceStep { forward: true } => "Switch to the next workspace".into(),
            Action::WorkspaceLast => "Switch to the workspace used before this one".into(),
            Action::WorkspaceChoose => {
                "Choose a workspace: a digit switches, c makes a new one, x then a digit deletes one".into()
            }
            Action::WorkspaceRename => "Rename this workspace".into(),
            Action::WorkspaceNew => "Make a new workspace as a copy of this one, and switch to it".into(),
            Action::ScratchpadShow { module: Some(id) } => {
                format!("Show {id} from the scratchpad, or put it away again")
            }
            Action::WorkspaceSwitch { name } => format!("Switch to workspace {name}"),
            Action::SendPrefix => format!("Send {prefix} itself to the pane with the keys"),
            Action::SendKeys(key) => format!("Send {} to the pane with the keys", key.human()),
            Action::Split { axis: Axis::Row, .. } => {
                format!("Then a module key ({module_keys}): open it right of this one, or move it there")
            }
            Action::Split { axis: Axis::Column, .. } => {
                format!("Then a module key ({module_keys}): open it below this one, or move it there")
            }
            Action::Move(dir) => format!(
                "Move this module (or the selected container) one step {}",
                direction_name(*dir)
            ),
            Action::ContainerTabbed => "Make this module's container tabbed, or restore its split".into(),
            Action::ContainerStacked => "Make this module's container stacked, or restore its split".into(),
            Action::FocusParent => "Select the container around this module, then its parent".into(),
            Action::FocusChild => "Narrow the selection toward the module with the keys".into(),
            Action::InstanceNew {
                kind: ModuleKind::Terminal,
            } => "Open a new terminal".into(),
            Action::InstanceNew { .. } => "Open a new chat view".into(),
            Action::Zoom => "Zoom this module, or restore".into(),
            Action::MoveFar(Direction::Left) => "Move this module to the window's far left".into(),
            Action::MoveFar(Direction::Down) => "Move this module to the window's bottom".into(),
            Action::MoveFar(Direction::Up) => "Move this module to the window's top".into(),
            Action::MoveFar(Direction::Right) => "Move this module to the window's far right".into(),
            Action::ToggleSplit => "Turn this module's split between side by side and stacked".into(),
            Action::Rotate { down: false } => {
                "Rotate the modules on screen one place back (the first goes last)".into()
            }
            Action::Rotate { down: true } => "Rotate the modules on screen one place on (the last comes first)".into(),
            Action::Flip(Axis::Row) => "Mirror the layout left to right".into(),
            Action::Flip(Axis::Column) => "Mirror the layout top to bottom".into(),
            Action::MainHorizontal => "The first module on top, the rest in a row below".into(),
            Action::MainVertical => "The first module on the left, the rest in a column beside it".into(),
            Action::Tiled => "Every module in a grid of equal cells".into(),
            Action::CycleLayout { forward: true } => "The next of the five layouts (even, main, tiled)".into(),
            Action::CycleLayout { forward: false } => "The previous of the five layouts".into(),
            Action::Spread => "Even out the sizes in this module's row or column".into(),
            Action::LayoutUndo => "Undo the last layout change (one step; again: redo)".into(),
            Action::MarkToggle => "Mark this module, or clear the mark if it is this one".into(),
            Action::MarkClear => "Clear the mark".into(),
            Action::SwapMarked => "Swap this module with the marked one".into(),
            Action::SelectNumber => "Number the modules on screen; a digit moves the keys there".into(),

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
            Action::ModuleHideByKey => format!(
                "Then a module key ({module_keys}): hide that module; it keeps running, and the keys stay unless it \
                 had them"
            ),
            // Killing the last module closes the window (`KillScope::Window`), as tmux's `kill-pane`
            // on a window's last pane does; hiding the last module is refused.
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
    fn the_window_verbs_parse_by_name_round_trip_and_take_their_options() {
        let s = |v: &str| OptValue::Str(v.to_string());
        let b = OptValue::Bool;
        let cases: Vec<(&str, Vec<(String, OptValue)>, Action)> = vec![
            ("window.new", vec![], Action::WindowNew),
            ("window.other", vec![], Action::WindowOther),
            (
                "move.to-window",
                vec![],
                Action::MoveToWindow {
                    module: None,
                    follow: false,
                },
            ),
            (
                "move.to-window",
                vec![("module".into(), s("terminal#2")), ("follow".into(), b(true))],
                Action::MoveToWindow {
                    module: Some(ModuleId::parse("terminal#2").unwrap()),
                    follow: true,
                },
            ),
            (
                "workspace.to-window",
                vec![("follow".into(), b(true))],
                Action::WorkspaceToWindow { follow: true },
            ),
            (
                "workspace.to-window",
                vec![],
                Action::WorkspaceToWindow { follow: false },
            ),
            ("workspace.swap-window", vec![], Action::WorkspaceSwapWindow),
        ];
        for (name, opts, action) in cases {
            let got = parse(name, &opts, &[]).unwrap().action;
            assert_eq!(got, action, "{name}");
            assert_eq!(got.name(), name);
            assert_eq!(
                parse(&got.name(), &got.options(), &[]).unwrap().action,
                got,
                "{name} round trip"
            );
            assert!(got.is_layout(), "{name}");
            assert_eq!(got.on_a_float(), FloatRule::Acts, "{name}");
            assert!(!got.describe("Ctrl+b", "e / a / t").is_empty());
        }
        assert_eq!(
            Action::MoveToWindow {
                module: None,
                follow: false
            }
            .selection_rule(),
            SelectionRule::Refuses
        );
        assert_eq!(Action::WorkspaceSwapWindow.selection_rule(), SelectionRule::Resets);
        let err = parse("window.new", &[("follow".into(), b(true))], &[])
            .unwrap_err()
            .to_string();
        assert!(err.contains("is not an option of window.new"), "{err}");
        let err = parse("move.to-window", &[("follow".into(), s("yes"))], &[])
            .unwrap_err()
            .to_string();
        assert!(err.contains("true or false"), "{err}");
        let err = parse("move.to-window", &[("module".into(), s("chat"))], &[])
            .unwrap_err()
            .to_string();
        assert!(err.contains("must be a module id"), "{err}");
        assert!(parse("move.to-windows", &[], &[]).is_err());
    }

    #[test]
    fn tab_break_parses_by_name_and_is_a_layout_action() {
        let got = parse("tab.break", &[], &[]).unwrap();
        assert_eq!(got.action, Action::TabBreak);
        assert!(!got.repeatable);
        assert_eq!(Action::TabBreak.name(), "tab.break");
        assert!(Action::TabBreak.options().is_empty());
        assert!(Action::TabBreak.is_layout(), "eitri.layout.run reaches it");
        assert_eq!(Action::TabBreak.selection_rule(), SelectionRule::Resets);
        assert_eq!(Action::TabBreak.on_a_float(), FloatRule::Acts);
        assert_eq!(
            Action::TabBreak.describe("Ctrl+b", "e / a / t"),
            "Break this session tab out into a chat view of its own; in that view, join it back"
        );
        let err = parse("tab.break", &[("n".into(), OptValue::Int(2))], &[])
            .unwrap_err()
            .to_string();
        assert!(err.contains("is not an option of tab.break"), "{err}");
    }

    #[test]
    fn the_float_and_workspace_actions_parse_by_name_and_round_trip() {
        let cases: Vec<(&str, Action)> = vec![
            ("float.toggle", Action::FloatToggle),
            ("focus.mode-toggle", Action::FocusModeToggle),
            ("scratchpad.show", Action::ScratchpadShow { module: None }),
            ("scratchpad.move", Action::ScratchpadMove),
            ("workspace.prev", Action::WorkspaceStep { forward: false }),
            ("workspace.next", Action::WorkspaceStep { forward: true }),
            ("workspace.last", Action::WorkspaceLast),
            ("workspace.choose", Action::WorkspaceChoose),
            ("workspace.rename", Action::WorkspaceRename),
            ("workspace.new", Action::WorkspaceNew),
        ];
        for (name, action) in cases {
            assert_eq!(parse(name, &[], &[]).unwrap().action, action, "{name}");
            assert_eq!(action.name(), name);
            assert!(action.options().is_empty(), "{name}");
            assert!(action.is_layout(), "{name}");
            assert!(!action.describe("Ctrl+b", "e / a / t").is_empty(), "{name}");
            let err = parse(name, &[("cells".into(), OptValue::Int(2))], &[])
                .unwrap_err()
                .to_string();
            assert!(err.contains(&format!("is not an option of {name}")), "{err}");
        }
        for bad in [
            "float",
            "float.togle",
            "scratchpad",
            "workspace",
            "workspace.delete",
            "focus.mode",
        ] {
            assert!(parse(bad, &[], &[]).unwrap_err().to_string().contains(bad), "{bad}");
        }
    }

    #[test]
    fn scratchpad_show_takes_a_module_and_workspace_switch_a_name() {
        let s = |v: &str| OptValue::Str(v.to_string());
        let t2 = ModuleId::parse("terminal#2").unwrap();
        let got = parse("scratchpad.show", &[("module".into(), s("terminal#2"))], &[])
            .unwrap()
            .action;
        assert_eq!(got, Action::ScratchpadShow { module: Some(t2) });
        assert_eq!(got.options(), vec![("module".to_string(), s("terminal#2"))]);
        assert_eq!(parse(&got.name(), &got.options(), &[]).unwrap().action, got);
        let err = parse("scratchpad.show", &[("module".into(), s("chat"))], &[]).unwrap_err();
        assert!(
            matches!(&err, ActionError::BadOption { option, why } if option == "module" && why.starts_with("must be a module id: ")),
            "{err}"
        );
        assert_eq!(
            parse("scratchpad.show", &[("module".into(), OptValue::Int(2))], &[]).unwrap_err(),
            ActionError::BadOption {
                option: "module".into(),
                why: "must be a string, not a number".into()
            }
        );

        let got = parse("workspace.switch", &[("name".into(), s("study"))], &[])
            .unwrap()
            .action;
        assert_eq!(got, Action::WorkspaceSwitch { name: "study".into() });
        assert_eq!(got.options(), vec![("name".to_string(), s("study"))]);
        assert_eq!(parse(&got.name(), &got.options(), &[]).unwrap().action, got);
        assert!(got.is_layout());
        assert_eq!(
            parse("workspace.switch", &[], &[]).unwrap_err(),
            ActionError::BadOption {
                option: "name".into(),
                why: "is required: the workspace's name, as { name = \"study\" }".into()
            }
        );
        let long = "x".repeat(33);
        for bad in ["", "a\nb", long.as_str()] {
            assert_eq!(
                parse("workspace.switch", &[("name".into(), s(bad))], &[]).unwrap_err(),
                ActionError::BadOption {
                    option: "name".into(),
                    why: format!("must be 1 to 32 characters with no control characters, not {bad:?}")
                }
            );
        }
        let err = parse("zoom", &[("name".into(), s("x"))], &[]).unwrap_err().to_string();
        assert!(err.contains("is not an option of zoom"), "{err}");
        let err = parse("zoom", &[("speed".into(), OptValue::Int(1))], &[])
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("(cells, n, keys, repeatable, up, cwd, kind, module, name, follow)"),
            "{err}"
        );
    }

    #[test]
    fn the_float_and_workspace_actions_say_whether_they_take_a_selection() {
        assert_eq!(Action::FloatToggle.selection_rule(), SelectionRule::Refuses);
        assert_eq!(Action::ScratchpadMove.selection_rule(), SelectionRule::Refuses);
        for action in [
            Action::FocusModeToggle,
            Action::ScratchpadShow { module: None },
            Action::WorkspaceStep { forward: true },
            Action::WorkspaceLast,
            Action::WorkspaceChoose,
            Action::WorkspaceRename,
            Action::WorkspaceSwitch { name: "x".into() },
            Action::WorkspaceNew,
        ] {
            assert_eq!(action.selection_rule(), SelectionRule::Resets, "{action:?}");
        }
    }

    #[test]
    fn every_action_says_what_it_does_with_the_keys_on_a_float() {
        for action in [
            Action::Even(Axis::Row),
            Action::MainHorizontal,
            Action::MainVertical,
            Action::Tiled,
            Action::CycleLayout { forward: true },
            Action::Rotate { down: false },
            Action::Flip(Axis::Column),
            Action::LayoutUndo,
        ] {
            assert_eq!(action.on_a_float(), FloatRule::Tiles, "{action:?}");
        }
        for action in [
            Action::Spread,
            Action::ContainerTabbed,
            Action::ContainerStacked,
            Action::ToggleSplit,
            Action::Swap(SwapTarget::Prev),
            Action::Swap(SwapTarget::Toward(Direction::Left)),
            Action::FocusParent,
            Action::FocusChild,
        ] {
            assert_eq!(action.on_a_float(), FloatRule::Refuses, "{action:?}");
        }
        for action in [
            Action::FloatToggle,
            Action::Move(Direction::Left),
            Action::MoveFar(Direction::Up),
            Action::Resize {
                dir: Direction::Right,
                cells: 5,
            },
            Action::Zoom,
            Action::ModuleKill,
            Action::ModuleHide,
            Action::SwapMarked,
            Action::MarkToggle,
            Action::SelectNext,
            Action::ScratchpadMove,
            Action::ScratchpadShow { module: None },
            Action::WorkspaceNew,
            Action::Tab(TabAction::New),
        ] {
            assert_eq!(action.on_a_float(), FloatRule::Acts, "{action:?}");
        }
    }

    #[test]
    fn module_actions_name_instances_before_lua_panels() {
        let t2 = ModuleId::parse("terminal#2").unwrap();
        assert_eq!(
            parse("module.terminal#2", &[], &[]).unwrap().action,
            Action::Module(t2.clone())
        );
        assert_eq!(Action::Module(t2).name(), "module.terminal#2");
        assert_eq!(
            parse("module.agent#9", &[], &[]).unwrap().action,
            Action::Module(ModuleId::parse("agent#9").unwrap())
        );
        for bad in ["module.terminal#1", "module.terminal#10", "module.editor#2"] {
            assert!(
                matches!(parse(bad, &[], &[]), Err(ActionError::NoSuchModule { .. })),
                "{bad}"
            );
        }
        // A Lua panel whose id is not an instance spelling still resolves.
        assert_eq!(
            parse("module.notes#2", &[], &["notes#2".to_string()]).unwrap().action,
            Action::Module(ModuleId::lua("notes#2"))
        );
        // An invalid instance spelling can still name a registered Lua panel.
        assert_eq!(
            parse("module.terminal#1", &[], &["terminal#1".to_string()])
                .unwrap()
                .action,
            Action::Module(ModuleId::lua("terminal#1"))
        );
    }

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

    /// `module.hide-by-key` waits for a module key, so on its own it is ruled as `module.hide` is: a layout action
    /// that acts on a float and is refused under a container selection. Its `?` row names the module keys.
    #[test]
    fn hide_by_key_parses_and_is_ruled_as_module_hide() {
        let parsed = parse("module.hide-by-key", &[], &[]).unwrap();
        assert_eq!(parsed.action, Action::ModuleHideByKey);
        assert!(!parsed.repeatable);
        assert_eq!(Action::ModuleHideByKey.name(), "module.hide-by-key");
        assert_eq!(Action::ModuleHideByKey.on_a_float(), Action::ModuleHide.on_a_float());
        assert_eq!(Action::ModuleHideByKey.selection_rule(), SelectionRule::Refuses);
        assert!(Action::ModuleHideByKey.is_layout());
        let row = Action::ModuleHideByKey.describe("Ctrl+b", "e / a / t");
        assert!(row.contains("(e / a / t)") && row.contains("keeps running"), "{row}");
        assert!(row.contains("the keys stay unless it had them"), "{row}");
    }

    /// The `?` overlay must say that killing the last module on screen closes the window,
    /// while hiding it is refused.
    #[test]
    fn the_kill_row_says_the_last_module_closes_eitri() {
        let kill = Action::ModuleKill.describe("Ctrl+b", "e / a / t");
        assert!(kill.contains("the last one on screen closes Eitri"), "{kill}");
        assert!(!kill.contains("not the last one"), "{kill}");
        assert!(Action::ModuleHide
            .describe("Ctrl+b", "e / a / t")
            .contains("not the last one on screen"));
    }

    /// tmux's `last-pane` and `select-pane -t :.+` need explicit names: `parse`'s `select.` fallback
    /// reads only a direction and must keep reading the directions.
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

    #[test]
    fn the_layout_verbs_parse_by_name_and_round_trip() {
        let cases: Vec<(&str, Action)> = vec![
            ("move.far-left", Action::MoveFar(Direction::Left)),
            ("move.far-down", Action::MoveFar(Direction::Down)),
            ("move.far-up", Action::MoveFar(Direction::Up)),
            ("move.far-right", Action::MoveFar(Direction::Right)),
            ("container.toggle-split", Action::ToggleSplit),
            ("rotate.up", Action::Rotate { down: false }),
            ("rotate.down", Action::Rotate { down: true }),
            ("layout.flip-horizontal", Action::Flip(Axis::Row)),
            ("layout.flip-vertical", Action::Flip(Axis::Column)),
            ("layout.main-horizontal", Action::MainHorizontal),
            ("layout.main-vertical", Action::MainVertical),
            ("layout.tiled", Action::Tiled),
            ("layout.next", Action::CycleLayout { forward: true }),
            ("layout.previous", Action::CycleLayout { forward: false }),
            ("layout.spread", Action::Spread),
            ("layout.undo", Action::LayoutUndo),
            ("mark.toggle", Action::MarkToggle),
            ("mark.clear", Action::MarkClear),
            ("swap.marked", Action::SwapMarked),
            ("select.number", Action::SelectNumber),
        ];
        for (name, action) in cases {
            assert_eq!(parse(name, &[], &[]).unwrap().action, action, "{name}");
            assert_eq!(action.name(), name);
            assert!(action.options().is_empty(), "{name}");
            assert!(action.is_layout(), "{name}");
            assert!(!action.describe("Ctrl+b", "e / a / t").is_empty(), "{name}");
            let err = parse(name, &[("cells".into(), OptValue::Int(2))], &[])
                .unwrap_err()
                .to_string();
            assert!(err.contains(&format!("is not an option of {name}")), "{err}");
        }
        for bad in [
            "move.far-sideways",
            "move.far",
            "swap.markedd",
            "select.numbers",
            "rotate.left",
        ] {
            assert!(parse(bad, &[], &[]).unwrap_err().to_string().contains(bad), "{bad}");
        }
    }

    #[test]
    fn is_layout_holds_the_layout_actions_and_nothing_else() {
        let layout = [
            Action::FloatToggle,
            Action::FocusModeToggle,
            Action::ScratchpadShow { module: None },
            Action::ScratchpadMove,
            Action::WorkspaceStep { forward: false },
            Action::WorkspaceStep { forward: true },
            Action::WorkspaceLast,
            Action::WorkspaceChoose,
            Action::WorkspaceRename,
            Action::WorkspaceSwitch { name: "x".into() },
            Action::WorkspaceNew,
            Action::Split {
                axis: Axis::Row,
                cwd: crate::keymap::action::SplitCwd::Start,
            },
            Action::Zoom,
            Action::Resize {
                dir: Direction::Left,
                cells: 1,
            },
            Action::Select(Direction::Up),
            Action::SelectLast,
            Action::SelectNext,
            Action::Swap(SwapTarget::Prev),
            Action::Even(Axis::Row),
            Action::ModuleHide,
            Action::ModuleKill,
            Action::Module(ModuleId::terminal()),
        ];
        let other = [
            Action::SendPrefix,
            Action::SendKeys(KeySpec::parse("C-l").unwrap()),
            Action::Tab(TabAction::New),
            Action::Hint,
            Action::PanelReload,
            Action::PanelKeymap,
            Action::PanelCommandLine,
            Action::WindowImmersive,
            Action::Text(TextChange::Reset),
            Action::CopyMode { up: false },
        ];
        for action in layout {
            assert!(action.is_layout(), "{action:?}");
        }
        for action in other {
            assert!(!action.is_layout(), "{action:?}");
        }
    }

    #[test]
    fn the_container_and_instance_actions_parse_by_name_and_round_trip() {
        for (name, action) in [
            ("move.left", Action::Move(Direction::Left)),
            ("move.down", Action::Move(Direction::Down)),
            ("move.up", Action::Move(Direction::Up)),
            ("move.right", Action::Move(Direction::Right)),
            ("container.tabbed", Action::ContainerTabbed),
            ("container.stacked", Action::ContainerStacked),
            ("focus.parent", Action::FocusParent),
            ("focus.child", Action::FocusChild),
        ] {
            assert_eq!(parse(name, &[], &[]).unwrap().action, action);
            assert_eq!(action.name(), name);
            assert!(action.options().is_empty());
            assert!(action.is_layout());
            assert!(!action.describe("Ctrl+b", "e / a / t").is_empty());
            assert_eq!(
                parse(name, &[("cells".into(), OptValue::Int(2))], &[]),
                Err(bad("cells", format!("is not an option of {name}")))
            );
        }
        for name in ["move.sideways", "move", "focus.sibling", "container.scroll"] {
            assert_eq!(parse(name, &[], &[]), Err(ActionError::Unknown { action: name.into() }));
        }
    }

    #[test]
    fn instance_new_takes_a_kind() {
        for (name, kind) in [("terminal", ModuleKind::Terminal), ("agent", ModuleKind::Agent)] {
            let action = Action::InstanceNew { kind };
            let opts = vec![("kind".into(), OptValue::Str(name.into()))];
            assert_eq!(parse("instance.new", &opts, &[]).unwrap().action, action);
            assert_eq!(action.name(), "instance.new");
            assert_eq!(action.options(), opts);
            assert!(action.is_layout());
        }
        assert_eq!(
            parse("instance.new", &[], &[]),
            Err(bad(
                "kind",
                "is required: \"terminal\" or \"agent\", as { kind = \"terminal\" }"
            ))
        );
        for (value, why) in [
            (
                OptValue::Str("editor".into()),
                "must be \"terminal\" or \"agent\", not \"editor\"",
            ),
            (OptValue::Int(3), "must be a string, not a number"),
        ] {
            assert_eq!(
                parse("instance.new", &[("kind".into(), value)], &[]),
                Err(bad("kind", why))
            );
        }
    }

    #[test]
    fn a_split_takes_cwd_start_or_pane() {
        for cwd in [SplitCwd::Start, SplitCwd::Pane] {
            let action = Action::Split { axis: Axis::Row, cwd };
            let opts = if cwd == SplitCwd::Start {
                vec![]
            } else {
                vec![("cwd".into(), OptValue::Str("pane".into()))]
            };
            assert_eq!(action.options(), opts);
            assert_eq!(parse("split.right", &opts, &[]).unwrap().action, action);
        }
        assert_eq!(
            parse("split.right", &[("cwd".into(), OptValue::Str("start".into()))], &[])
                .unwrap()
                .action,
            Action::Split {
                axis: Axis::Row,
                cwd: SplitCwd::Start
            }
        );
        assert_eq!(
            parse("split.right", &[("cwd".into(), OptValue::Str("home".into()))], &[]),
            Err(bad("cwd", "must be \"start\" or \"pane\", not \"home\""))
        );
        assert_eq!(
            parse("zoom", &[("cwd".into(), OptValue::Str("pane".into()))], &[]),
            Err(bad("cwd", "is not an option of zoom"))
        );
        assert_eq!(
            parse("zoom", &[("wat".into(), OptValue::Int(1))], &[]),
            Err(bad(
                "wat",
                "is not an option (cells, n, keys, repeatable, up, cwd, kind, module, name, follow)"
            ))
        );
    }

    #[test]
    fn every_action_says_whether_it_takes_a_selection() {
        for action in [
            Action::Move(Direction::Left),
            Action::MoveFar(Direction::Up),
            Action::Zoom,
            Action::ContainerTabbed,
            Action::ContainerStacked,
            Action::ToggleSplit,
            Action::Flip(Axis::Row),
            Action::Spread,
            Action::FocusParent,
            Action::FocusChild,
        ] {
            assert_eq!(action.selection_rule(), SelectionRule::Takes, "{action:?}");
        }
        for action in [
            Action::ModuleKill,
            Action::ModuleHide,
            Action::Swap(SwapTarget::Next),
            Action::SwapMarked,
            Action::MarkToggle,
        ] {
            assert_eq!(action.selection_rule(), SelectionRule::Refuses, "{action:?}");
        }
        for action in [
            Action::SendPrefix,
            Action::SendKeys(KeySpec::parse("C-l").unwrap()),
            Action::Split {
                axis: Axis::Row,
                cwd: SplitCwd::Start,
            },
            Action::Resize {
                dir: Direction::Up,
                cells: 1,
            },
            Action::Select(Direction::Left),
            Action::SelectLast,
            Action::SelectNext,
            Action::Even(Axis::Row),
            Action::Rotate { down: false },
            Action::MainHorizontal,
            Action::MainVertical,
            Action::Tiled,
            Action::CycleLayout { forward: true },
            Action::LayoutUndo,
            Action::MarkClear,
            Action::SelectNumber,
            Action::Module(ModuleId::terminal()),
            Action::InstanceNew {
                kind: ModuleKind::Terminal,
            },
            Action::Tab(TabAction::New),
            Action::Hint,
            Action::PanelReload,
            Action::PanelKeymap,
            Action::PanelCommandLine,
            Action::WindowImmersive,
            Action::Text(TextChange::Reset),
            Action::CopyMode { up: false },
        ] {
            assert_eq!(action.selection_rule(), SelectionRule::Resets, "{action:?}");
        }
    }
}
