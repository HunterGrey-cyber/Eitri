//! R1-4: when the embedded nvim fails to start (missing from `PATH`, or older than the pinned
//! fork's floor), say why in the editor's own area instead of leaving an unexplained dark-red block
//! (`neovide_editor`'s `FAILED_COLOR`). `NeovideEditorPane::on_start_failed` hands this the built
//! message ("what failed, the version floor, where to get one") only once, asynchronously, well
//! after the pane already has a widget sitting in the module grid -- so the label has to already be
//! in place, hidden, from the moment the editor module is added.

use eitri_core::layout::{Direction, ModuleId};
use gtk4::gdk::{Key, ModifierType};
use gtk4::glib;
use gtk4::prelude::*;

/// Wraps `editor` (the editor pane's own `GtkGLArea`, [`neovide_editor::NeovideEditorPane::widget`])
/// in an `Overlay` carrying a hidden `Label` for the failure message. Return the overlay to register
/// with the module grid *instead of* `editor` -- the grid never reparents (`module_grid`'s own doc),
/// so this must happen once, before `ModuleGrid::add`, not by inserting an overlay under an
/// already-parented widget. The overlay's main child keeps the exact rect the grid allocates the
/// editor module, precisely because it *is* what the grid now allocates to; no coordinate tracking
/// of the editor's own rect is needed. The label paints no background of its own, so
/// `FAILED_COLOR`'s Skia fill (painted by the GLArea underneath) shows through behind it -- only the
/// text needs to read against that dark red, which [`show`] handles.
pub(crate) fn install(editor: &gtk4::GLArea) -> (gtk4::Overlay, gtk4::Label) {
    let overlay = gtk4::Overlay::new();
    overlay.set_child(Some(editor));

    let label = gtk4::Label::new(None);
    label.add_css_class("editor-start-failure");
    label.set_wrap(true);
    label.set_wrap_mode(gtk4::pango::WrapMode::WordChar);
    label.set_justify(gtk4::Justification::Center);
    label.set_max_width_chars(56);
    label.set_halign(gtk4::Align::Center);
    label.set_valign(gtk4::Align::Center);
    label.set_margin_start(24);
    label.set_margin_end(24);
    // Never takes a click or the keys -- the same rule the toast and the HINT labels follow
    // (`shell::toast`, `shell::hint`): this is a message, not a control.
    label.set_can_target(false);
    label.set_can_focus(false);
    label.set_visible(false);
    overlay.add_overlay(&label);

    (overlay, label)
}

/// Shows `message` on `label` -- [`neovide_editor::NeovideEditorPane::on_start_failed`]'s callback.
/// White text, since the backdrop it has to read against (`FAILED_COLOR`) is a fixed dark red set
/// long before any nvim colorscheme could apply, so there is no theme token to follow here the way
/// `shell::theme` drives everything else.
pub(crate) fn show(label: &gtk4::Label, message: &str) {
    label.set_markup(&format!(
        "<span foreground=\"#ffffff\">{}</span>",
        glib::markup_escape_text(message)
    ));
    label.set_visible(true);
}

/// Where the keys go when nvim fails to start (R4: a pane that cannot take input does not keep
/// them). Construction is asynchronous, after the window is shown and the first focus chosen, so
/// the editor can hold the keys when the failure lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KeysAfterFailure {
    /// The keys were elsewhere, or nowhere: they stay.
    Stay,
    /// The editor held them: they move to the chat, shown if it was hidden.
    ToChat,
}

pub(crate) fn keys_after_failure(focused: Option<&ModuleId>) -> KeysAfterFailure {
    if focused == Some(&ModuleId::editor()) {
        KeysAfterFailure::ToChat
    } else {
        KeysAfterFailure::Stay
    }
}

/// `Ctrl+h/j/k/l` on the editor while no nvim runs in it -- it failed to start, it is still
/// starting, or it exited -- move between modules at the shell's level, as they do from every other
/// module (the Opus review's T7-1). Otherwise they belong to nvim, whose own navigator (or Eitri's
/// fallback, `EITRI_NAV_LUA`) resolves them and calls out to the shell only at a window edge; with
/// no nvim there, the pane swallowed them and a failed editor, once reached again by `Ctrl+h`,
/// `prefix e`, a HINT label or a click, kept the keys for good. The precedent is
/// vim-tmux-navigator's own tmux half: `bind -n C-h if-shell "$is_vim" "send-keys C-h" "select-pane
/// -L"` -- a pane not running vim moves at the multiplexer's level.
pub(crate) fn navigation(key: Key, state: ModifierType, nvim_running: bool) -> Option<Direction> {
    if nvim_running {
        return None;
    }
    crate::pane_switch::nav_direction(key, state)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Task 7's focus move (the Opus review's T6-4, its sixth mutation): the keys leave only an
    /// editor that held them.
    #[test]
    fn the_keys_leave_a_failed_editor_only_if_it_held_them() {
        assert_eq!(keys_after_failure(Some(&ModuleId::editor())), KeysAfterFailure::ToChat);
        assert_eq!(keys_after_failure(Some(&ModuleId::agent())), KeysAfterFailure::Stay);
        assert_eq!(keys_after_failure(Some(&ModuleId::terminal())), KeysAfterFailure::Stay);
        assert_eq!(keys_after_failure(None), KeysAfterFailure::Stay);
    }

    /// T7-1: with no nvim, the four chords move at the shell's level; with nvim running they are
    /// nvim's; anything else is never claimed.
    #[test]
    fn an_editor_without_nvim_lets_ctrl_hjkl_out() {
        let ctrl = ModifierType::CONTROL_MASK;
        for (key, direction) in [
            (Key::h, Direction::Left),
            (Key::j, Direction::Down),
            (Key::k, Direction::Up),
            (Key::l, Direction::Right),
        ] {
            assert_eq!(navigation(key, ctrl, false), Some(direction), "{key:?}");
            assert_eq!(navigation(key, ctrl, true), None, "{key:?} is nvim's while it runs");
        }
        assert_eq!(navigation(Key::h, ModifierType::empty(), false), None, "a plain h");
        assert_eq!(navigation(Key::h, ctrl | ModifierType::SHIFT_MASK, false), None);
        assert_eq!(navigation(Key::a, ctrl, false), None);
    }
}
