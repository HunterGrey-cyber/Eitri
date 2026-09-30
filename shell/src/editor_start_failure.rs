//! Explains an embedded nvim startup failure in the editor's own area, such as nvim missing
//! from `PATH` or being older than the pinned fork supports.
//! `NeovideEditorPane::on_start_failed` delivers the message once, asynchronously, after the
//! pane's widget is already in the module grid. Install the hidden label when adding the
//! editor module so the callback can show the cause, minimum version, and where to get nvim.

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

/// Chooses where the keys go after nvim fails to start. Construction finishes asynchronously,
/// after the window is shown and initial focus is chosen, so the failed editor may hold
/// focus when the failure arrives and must give it up.
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

/// Routes `Ctrl+h/j/k/l` between modules at the shell level while nvim is absent: startup
/// may have failed, may still be pending, or nvim may have exited. Otherwise nvim's
/// navigator or the embedded fallback handles these keys and calls the shell only at a
/// window edge. Without shell navigation, revisiting a failed editor by shortcut or click
/// would leave the keys trapped in a pane that cannot handle them.
pub(crate) fn navigation(key: Key, state: ModifierType, nvim_running: bool) -> Option<Direction> {
    if nvim_running {
        return None;
    }
    crate::pane_switch::nav_direction(key, state)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A failed editor moves keyboard focus only if it held focus when it failed.
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
