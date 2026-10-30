//! Explains an embedded nvim startup failure in the editor's own area, such as nvim missing
//! from `PATH` or being older than the pinned fork supports.
//! `NeovideEditorPane::on_start_failed` delivers the message once per failed launch,
//! asynchronously, after the pane's widget is already in the module grid. Install the hidden
//! label when adding the editor module so the callback can show the cause, minimum version,
//! and where to get nvim. A later successful launch hides the label again ([`hide`]).

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

/// Hides `label` again once a later launch of the pane gave it a running nvim, which the failure
/// message no longer describes.
pub(crate) fn hide(label: &gtk4::Label) {
    label.set_visible(false);
}
