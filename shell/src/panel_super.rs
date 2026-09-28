//! Filters Super and Hyper chords before they reach web content.
//!
//! WebKitGTK 2.52.6 does not expose these modifiers on the page's `KeyboardEvent`,
//! so `isPlainAnswerKey` cannot distinguish `Super+a` from a permission-card answer.
//! The page tracks Super keydown and keyup, but that misses a modifier held across
//! a focus change or a press consumed by GTK's HINT controller. GDK receives the
//! current modifiers when focus arrives and can catch these cases.
//! A capture-phase controller on the panel's host drops key presses carrying
//! `SUPER_MASK` or `HYPER_MASK` before WebKit gets them. The panel binds neither
//! modifier. Window shortcuts run earlier in the window's capture phase.
//!
//! When GDK does not report Super, this filter cannot detect it. The page-side
//! tracker remains necessary for that case.

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
