//! The runner's rules around every action, from a key, a Lua call or a panel tab verb: which family an action
//! belongs to, the container selection's gate and the float's, and the words of their refusals.

pub mod chat;
pub mod chrome;
pub mod floats;
pub mod focus;
pub mod keys;
pub mod module_windows;
pub mod modules;
pub mod project;
pub mod tabs;
pub mod text_size;
pub mod tiling;
pub mod windows;
pub mod workspaces;

use crate::keymap::action::{Action, FloatRule, SelectionRule};
use crate::keymap::Keymap;
use crate::layout::Selection;

/// The families of verbs, each decided by its own part of the runner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    /// What changes the tiled tree, the mark, the zoom or the selection.
    Tiling,
    /// What moves the keys between modules.
    Focus,
    /// What sends keys on, numbers the modules or starts a hint.
    Keys,
    /// The window's own presentation.
    Chrome,
    /// Showing, hiding, killing and making modules.
    Modules,
    /// Switching, making and naming workspaces.
    Workspaces,
    /// Choosing another project.
    Project,
    /// The linked pair's windows.
    Windows,
    /// Floats, focus mode and the scratchpad.
    Floats,
    /// The chat panel's own verbs and its tabs.
    Panel,
    /// The editor's kill, which is the editor's quit.
    Close,
    /// Text size.
    TextSize,
}

/// The family `action` belongs to by its kind alone. A `move.<dir>` with the keys on a float is a float's verb
/// and the editor's kill is its quit, which only [`family_of`] can tell.
pub fn family(action: &Action) -> Family {
    match action {
        Action::ProjectChoose => Family::Project,
        Action::MoveFar(_)
        | Action::ToggleSplit
        | Action::Rotate { .. }
        | Action::Flip(_)
        | Action::Spread
        | Action::Even(_)
        | Action::MainHorizontal
        | Action::MainVertical
        | Action::Tiled
        | Action::CycleLayout { .. }
        | Action::LayoutUndo
        | Action::MarkToggle
        | Action::MarkClear
        | Action::SwapMarked
        | Action::Zoom
        | Action::Resize { .. }
        | Action::Swap(_)
        | Action::Move(_)
        | Action::ContainerTabbed
        | Action::ContainerStacked
        | Action::FocusParent
        | Action::FocusChild => Family::Tiling,
        Action::Select(_) | Action::SelectLast | Action::SelectNext => Family::Focus,
        Action::SelectNumber | Action::Hint | Action::SendPrefix | Action::SendKeys(_) | Action::CopyMode { .. } => {
            Family::Keys
        }
        Action::WindowImmersive | Action::WindowSetup => Family::Chrome,
        Action::Module(_)
        | Action::ModuleHide
        | Action::ModuleHideByKey
        | Action::ModuleKill
        | Action::InstanceNew { .. }
        | Action::Split { .. } => Family::Modules,
        Action::WorkspaceStep { .. }
        | Action::WorkspaceLast
        | Action::WorkspaceNew
        | Action::WorkspaceSwitch { .. }
        | Action::WorkspaceChoose
        | Action::WorkspaceRename => Family::Workspaces,
        Action::WindowNew
        | Action::WindowOther
        | Action::MoveToWindow { .. }
        | Action::WorkspaceToWindow { .. }
        | Action::WorkspaceSwapWindow => Family::Windows,
        Action::FloatToggle | Action::FocusModeToggle | Action::ScratchpadMove | Action::ScratchpadShow { .. } => {
            Family::Floats
        }
        Action::PanelReload | Action::PanelKeymap | Action::PanelCommandLine | Action::Tab(_) | Action::TabBreak => {
            Family::Panel
        }
        Action::Text(_) | Action::TextAll(_) => Family::TextSize,
    }
}

/// [`family`] where the window decides it: `move.<dir>` with the keys on a float moves the float, and
/// `module.kill` with the keys on the editor quits the editor.
pub fn family_of(action: &Action, keys_on_a_float: bool, keys_on_the_editor: bool) -> Family {
    match (action, family(action)) {
        (Action::Move(_), Family::Tiling) if keys_on_a_float => Family::Floats,
        (Action::ModuleKill, Family::Modules) if keys_on_the_editor => Family::Close,
        (_, family) => family,
    }
}

/// What a container selection does to an action, before and after it runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionGate {
    /// The action acts on one module, and a container is selected: it does not run.
    Refuse,
    /// It runs; with `reset`, the selection goes back to the module with the keys afterwards.
    Run { reset: bool },
}

/// What the keys being on a float does to an action, before it runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FloatGate {
    Run,
    /// The action needs a split around the module, which a float has not got.
    Refuse,
}

/// The selection's rule for `action` (`Action::selection_rule`) against what is selected now.
pub fn selection_gate(action: &Action, selection: Selection) -> SelectionGate {
    match (action.selection_rule(), selection) {
        (SelectionRule::Refuses, Selection::Con(_)) => SelectionGate::Refuse,
        (SelectionRule::Resets, _) => SelectionGate::Run { reset: true },
        _ => SelectionGate::Run { reset: false },
    }
}

/// The float's rule for `action` (`Action::on_a_float`) when the module with the keys floats: a verb on the
/// tiled tree runs and leaves the floats alone, a verb on the float acts on it, and a verb that needs a split
/// around the module is refused.
pub fn float_gate(action: &Action, keys_on_a_float: bool) -> FloatGate {
    match (action.on_a_float(), keys_on_a_float) {
        (FloatRule::Refuses, true) => FloatGate::Refuse,
        (FloatRule::Tiles | FloatRule::Acts | FloatRule::Refuses, _) => FloatGate::Run,
    }
}

/// What a refusal under a container selection says: how to get back to one module, as the keymap binds
/// `focus.child` (`acts on one module: C-b M-d`), or its name when no key reaches it.
pub fn one_module_text(keymap: &Keymap) -> String {
    format!("acts on one module: {}", key_text(keymap, &Action::FocusChild))
}

/// What a refusal with the keys on a float says: how to sink it.
pub fn no_split_text(keymap: &Keymap) -> String {
    format!(
        "a float has no split: {} sinks it",
        key_text(keymap, &Action::FloatToggle)
    )
}

/// How a refusal names the way to `action`: `<prefix> <key>` as the keymap binds it first, or the action's
/// name when no key reaches it.
pub fn key_text(keymap: &Keymap, action: &Action) -> String {
    keymap
        .keys_for(action)
        .first()
        .map(|key| format!("{} {}", keymap.prefix().human(), key.human()))
        .unwrap_or_else(|| action.name())
}

#[cfg(test)]
pub(crate) mod rig;

#[cfg(test)]
mod tests;
