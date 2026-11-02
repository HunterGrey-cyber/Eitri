//! No web module's page -- the agent panel, or a Lua or plugin page -- sees a key pressed with Super or
//! Hyper held.
//!
//! The 2026-09-28 sandbox pass of the v1 audit fixes found `Super+a` answering a permission card:
//! WebKitGTK 2.52.6 puts no modifier on the page's `KeyboardEvent` for Super or Hyper, so the
//! panel's own check (`agent-ui/web/src/keymap.ts`, `isPlainAnswerKey`) cannot see one. The panel
//! now also tracks Super's own keydown/keyup (`agent-ui/web/src/heldSuper.ts`), but a tracker made of
//! key events misses Super held across a focus change, or a Super press GTK consumes before the page
//! (the HINT capture controller). GDK's modifier state has neither gap: the compositor sends the
//! current modifiers whenever focus arrives. So a key press carrying `SUPER_MASK` or `HYPER_MASK` is
//! translated with `super_or_hyper` set (`gtk_host::keys::key_press`), and the window controller's
//! module stage (`eitri_core::wm::keys::module_press`, asked from each web module's host by
//! `gtk_host::keys::module_key`) consumes it before WebKit gets it; when the stage cannot decide (the
//! window torn down, or not yet laid out), `wire_host` still keeps the chord from the page, so the rule
//! fails toward the card rather than open. The panel binds nothing with Super
//! or Hyper, and a plugin page may not act on a Super or Hyper chord either (`keys.declare` refuses
//! such a key), so nothing any page binds is lost; window-level shortcuts run in the window's own
//! capture phase, before this, and are unaffected. A Lua or plugin page gets the same rule so a page
//! never sees half of a chord the window or the compositor owns.
//!
//! **Where GDK does not report Super, this does nothing** -- the headless sway sandbox is one such
//! place, which is why the page-side tracker stays too.
//! Whether a real GNOME session reports it is on the real-hardware checklist.

#[cfg(test)]
mod tests {
    use gtk4::gdk::{Key, ModifierType};

    /// What the module stage reads as a system modifier held, from GDK's state.
    fn withheld_from_panel(state: ModifierType) -> bool {
        crate::gtk_host::keys::key_press(Key::a, 38, state).super_or_hyper
    }

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
