//! Filters Super and Hyper chords before they reach web content.
//!
//! WebKitGTK 2.52.6 does not expose these modifiers on the page's `KeyboardEvent`,
//! so `isPlainAnswerKey` cannot distinguish `Super+a` from a permission-card answer.
//! The page tracks Super keydown and keyup, but that misses a modifier held across
//! a focus change or a press consumed by GTK's HINT controller. GDK receives the
//! current modifiers when focus arrives and can catch these cases.
//! `gtk_host::keys::key_press` sets `super_or_hyper` for `SUPER_MASK` or `HYPER_MASK`.
//! The window controller's module stage consumes the chord before WebKit receives it,
//! via `gtk_host::keys::module_key`. If the window is gone or has no layout yet,
//! `wire_host` still keeps the chord from the page.
//! The panel binds neither modifier, and `keys.declare` refuses them for plugin pages.
//! Window shortcuts run earlier in the window's capture phase. Lua and plugin pages
//! use the same filter to avoid seeing only part of a chord.
//!
//! When GDK does not report Super, this filter cannot detect it. The page-side
//! tracker remains necessary for that case.

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
