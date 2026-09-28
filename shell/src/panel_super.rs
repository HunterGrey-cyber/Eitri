//! The agent panel never sees a key pressed with Super or Hyper held.
//!
//! The 2026-09-28 sandbox pass of the v1 audit fixes found `Super+a` answering a permission card:
//! WebKitGTK 2.52.6 puts no modifier on the page's `KeyboardEvent` for Super or Hyper, so the
//! panel's own check (`agent-ui/web/src/keymap.ts`, `isPlainAnswerKey`) cannot see one. The panel
//! now also tracks Super's own keydown/keyup (`agent-ui/web/src/heldSuper.ts`), but a tracker made of
//! key events misses Super held across a focus change, or a Super press GTK consumes before the page
//! (the HINT capture controller). GDK's modifier state has neither gap: the compositor sends the
//! current modifiers whenever focus arrives. So a capture-phase controller on the panel's host drops
//! any key press carrying `SUPER_MASK` or `HYPER_MASK` before WebKit gets it. The panel binds nothing
//! with Super or Hyper, so nothing is lost; window-level shortcuts run in the window's own capture
//! phase, before this, and are unaffected.
//!
//! **Where GDK does not report Super, this does nothing** -- the headless sway sandbox is one such
//! place (dated record, 2026-09-28, the Super+q note), which is why the page-side tracker stays too.
//! Whether the owner's GNOME session reports it is on the real-hardware checklist.

use gtk4::gdk::ModifierType;
use gtk4::glib;
use gtk4::prelude::*;

/// Whether a key press with this modifier state must not reach the agent panel's page.
pub fn withheld_from_panel(state: ModifierType) -> bool {
    state.intersects(ModifierType::SUPER_MASK | ModifierType::HYPER_MASK)
}

/// Installs the filter on the agent panel's module host.
pub fn install(host: &gtk4::Widget) {
    let controller = gtk4::EventControllerKey::new();
    controller.set_propagation_phase(gtk4::PropagationPhase::Capture);
    controller.connect_key_pressed(|_controller, _key, _keycode, state| {
        if withheld_from_panel(state) {
            glib::Propagation::Stop
        } else {
            glib::Propagation::Proceed
        }
    });
    host.add_controller(controller);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn super_or_hyper_held_is_withheld_and_nothing_else_is() {
        assert!(withheld_from_panel(ModifierType::SUPER_MASK));
        assert!(withheld_from_panel(ModifierType::HYPER_MASK));
        assert!(withheld_from_panel(ModifierType::SUPER_MASK | ModifierType::SHIFT_MASK));
        for plain in [
            ModifierType::empty(),
            ModifierType::SHIFT_MASK,
            ModifierType::CONTROL_MASK,
            ModifierType::ALT_MASK,
            ModifierType::META_MASK,
            ModifierType::LOCK_MASK,
        ] {
            assert!(!withheld_from_panel(plain), "{plain:?}");
        }
    }
}
