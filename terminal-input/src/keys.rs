//! Minimal stand-in for the subset of `winit` types that the vendored Alacritty
//! encoder in [`crate::vendored`] refers to.
//!
//! PROVENANCE
//! ----------
//! This file is NOT a copy of winit. It is an independently written shim that
//! reproduces the *public API shape* of winit 0.30.13's `Key`, `NamedKey`,
//! `KeyLocation`, `ElementState`, `ModifiersState` and `KeyEvent` so that
//! `alacritty/src/input/keyboard.rs` compiles against it without edits.
//!
//! Two things are copied in substance rather than merely in shape, and both come
//! from winit 0.30.13 (`src/keyboard.rs`), Apache-2.0:
//!   * the `NamedKey` variant names, and
//!   * the body of [`NamedKey::to_text`] (winit src/keyboard.rs:1576-1585),
//!     which the encoder's control flow depends on exactly.
//!
//! WHY A SHIM AND NOT WINIT
//! ------------------------
//! `winit::event::KeyEvent` cannot be constructed outside of winit: its
//! `platform_specific` field is private and there is no public constructor. A
//! headless engine must be able to *synthesise* key events, so the event type has
//! to be ours. `Key` is generic over its string type (`Key<Str = String>`) with an
//! inherent `as_ref()` for exactly the same reason winit's is: the vendored code
//! calls `key.logical_key.as_ref()` and then pattern-matches `Key::Character("0")`
//! against `&str` literals. Dropping the generic would require editing the
//! vendored code, which is the thing we are trying not to do.

use std::fmt;

/// Where on the keyboard a key physically sits.
///
/// Mirrors `winit::keyboard::KeyLocation`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum KeyLocation {
    #[default]
    Standard,
    Left,
    Right,
    Numpad,
}

/// Whether a key was pressed or released.
///
/// Mirrors `winit::event::ElementState`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ElementState {
    #[default]
    Pressed,
    Released,
}

impl ElementState {
    #[inline]
    pub fn is_pressed(self) -> bool {
        matches!(self, ElementState::Pressed)
    }
}

bitflags::bitflags! {
    /// The state of the keyboard modifiers.
    ///
    /// Mirrors `winit::keyboard::ModifiersState`. The bit values are winit's.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
    pub struct ModifiersState: u32 {
        const SHIFT   = 0b0000_0100;
        const CONTROL = 0b0000_1000;
        const ALT     = 0b0001_0000;
        const SUPER   = 0b0010_0000;
    }
}

impl ModifiersState {
    #[inline]
    pub fn shift_key(&self) -> bool {
        self.intersects(Self::SHIFT)
    }

    #[inline]
    pub fn control_key(&self) -> bool {
        self.intersects(Self::CONTROL)
    }

    #[inline]
    pub fn alt_key(&self) -> bool {
        self.intersects(Self::ALT)
    }

    #[inline]
    pub fn super_key(&self) -> bool {
        self.intersects(Self::SUPER)
    }
}

/// A key with a name, as opposed to one that produces a character.
///
/// Variant names follow winit 0.30.13. Only the variants the encoder can act on
/// are declared; winit's full list is far longer and the rest would be dead
/// weight that the encoder maps to `None` anyway.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[allow(clippy::upper_case_acronyms)]
pub enum NamedKey {
    // Modifier keys.
    Alt,
    CapsLock,
    Control,
    Hyper,
    Meta,
    NumLock,
    ScrollLock,
    Shift,
    Super,
    // Whitespace / editing.
    Enter,
    Tab,
    Space,
    Backspace,
    Delete,
    Insert,
    Escape,
    // Navigation.
    ArrowDown,
    ArrowLeft,
    ArrowRight,
    ArrowUp,
    End,
    Home,
    PageDown,
    PageUp,
    // Misc.
    ContextMenu,
    Pause,
    PrintScreen,
    // Media.
    AudioVolumeDown,
    AudioVolumeMute,
    AudioVolumeUp,
    MediaFastForward,
    MediaPause,
    MediaPlay,
    MediaPlayPause,
    MediaRecord,
    MediaRewind,
    MediaStop,
    MediaTrackNext,
    MediaTrackPrevious,
    // Function keys.
    F1,
    F2,
    F3,
    F4,
    F5,
    F6,
    F7,
    F8,
    F9,
    F10,
    F11,
    F12,
    F13,
    F14,
    F15,
    F16,
    F17,
    F18,
    F19,
    F20,
    F21,
    F22,
    F23,
    F24,
    F25,
    F26,
    F27,
    F28,
    F29,
    F30,
    F31,
    F32,
    F33,
    F34,
    F35,
}

impl NamedKey {
    /// Convert a named key to its approximate textual equivalent.
    ///
    /// Body transcribed from winit 0.30.13 `src/keyboard.rs:1576-1585`. The
    /// encoder branches on `to_text().is_some()` in two places (`alt_send_esc`
    /// and `should_build_sequence`), so this table is load-bearing: adding a
    /// variant here changes which keys get an ESC prefix under Alt.
    pub fn to_text(&self) -> Option<&str> {
        match self {
            NamedKey::Enter => Some("\r"),
            NamedKey::Backspace => Some("\x08"),
            NamedKey::Tab => Some("\t"),
            NamedKey::Space => Some(" "),
            NamedKey::Escape => Some("\x1b"),
            _ => None,
        }
    }
}

/// A logical key.
///
/// Generic over the string type exactly as winit's is, so that
/// `Key<String>::as_ref()` yields a `Key<&str>` that the vendored code can match
/// against string literals.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Key<Str = String> {
    Named(NamedKey),
    Character(Str),
    Dead(Option<char>),
    Unidentified,
}

impl<Str: AsRef<str>> Key<Str> {
    pub fn as_ref(&self) -> Key<&str> {
        match self {
            Key::Named(named) => Key::Named(*named),
            Key::Character(ch) => Key::Character(ch.as_ref()),
            Key::Dead(ch) => Key::Dead(*ch),
            Key::Unidentified => Key::Unidentified,
        }
    }
}

impl Key<String> {
    /// Convenience constructor; `Key::Character("a".into())` gets verbose.
    pub fn character(s: &str) -> Self {
        Key::Character(s.to_owned())
    }
}

impl<Str: fmt::Display> fmt::Display for Key<Str> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Key::Named(named) => write!(f, "{named:?}"),
            Key::Character(ch) => write!(f, "Character({ch})"),
            Key::Dead(ch) => write!(f, "Dead({ch:?})"),
            Key::Unidentified => write!(f, "Unidentified"),
        }
    }
}

/// A synthesised keyboard event.
///
/// Every field is public and plain data, which is the whole point: winit's
/// `KeyEvent` has a private `platform_specific` field and therefore cannot be
/// built outside winit. `text_with_all_modifiers` and `key_without_modifiers`
/// are fields here where winit exposes them as trait methods on
/// `KeyEventExtModifierSupplement`; the accessor methods below preserve the
/// call syntax the vendored code uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyEvent {
    /// The key as interpreted by the active layout and modifiers.
    pub logical_key: Key<String>,
    /// Physical location of the key. `Numpad` drives a whole encoder branch.
    pub location: KeyLocation,
    /// Press or release.
    pub state: ElementState,
    /// Whether this is an auto-repeat.
    pub repeat: bool,
    /// Text the key produces with every modifier applied (winit's
    /// `KeyEventExtModifierSupplement::text_with_all_modifiers`).
    ///
    /// **This field IS the encoding for text-producing keys** -- including Enter, whose byte is
    /// `\r` from here, not from `logical_key`. `encode_key` reads it; it does not re-derive the
    /// text from the key. So a hand-built `KeyEvent` that leaves this `None` encodes to ZERO
    /// BYTES, silently, while compiling perfectly and looking correct at the call site:
    ///
    /// ```text
    /// KeyEvent::press(Key::Named(NamedKey::Enter))     -> text Some("\r") -> [13]
    /// KeyEvent { logical_key: Enter, text_with_all_modifiers: None, .. } -> [] // nothing is sent
    /// ```
    ///
    /// Measured downstream, 2026-09-14: a consumer hand-filled this as `None` for a synthetic
    /// Enter, so its submit keystroke wrote nothing, the prompt sat in the composer, and no test
    /// caught it because every test double accepted whatever bytes it was handed.
    ///
    /// **Use [`KeyEvent::press`] for synthetic keys** -- it derives this field from the key. The
    /// fields stay public because this type mirrors winit's `KeyEvent` for the vendored encoder,
    /// so a real winit event can be adapted field-for-field; that is exactly why hand-construction
    /// is available and exactly why it needs this warning. If you do build one by hand, assert the
    /// bytes it encodes to, not just that encoding happened.
    pub text_with_all_modifiers: Option<String>,
    /// The key as it would be without any modifiers applied (winit's
    /// `KeyEventExtModifierSupplement::key_without_modifiers`).
    pub key_without_modifiers: Key<String>,
}

impl KeyEvent {
    /// Press event for a named key with no modifiers applied to its text.
    ///
    /// The only correct way to build a SYNTHETIC key event. It derives
    /// `text_with_all_modifiers` from `logical_key`, which is what `encode_key` actually reads --
    /// see that field's warning for what hand-construction silently costs you.
    pub fn press(logical_key: Key<String>) -> Self {
        let text = match &logical_key {
            Key::Character(ch) => Some(ch.clone()),
            Key::Named(named) => named.to_text().map(str::to_owned),
            _ => None,
        };
        Self {
            key_without_modifiers: logical_key.clone(),
            logical_key,
            location: KeyLocation::Standard,
            state: ElementState::Pressed,
            repeat: false,
            text_with_all_modifiers: text,
        }
    }

    /// Same key, but a release event.
    pub fn release(logical_key: Key<String>) -> Self {
        Self {
            state: ElementState::Released,
            ..Self::press(logical_key)
        }
    }

    pub fn with_location(mut self, location: KeyLocation) -> Self {
        self.location = location;
        self
    }

    pub fn with_repeat(mut self, repeat: bool) -> Self {
        self.repeat = repeat;
        self
    }

    pub fn with_text(mut self, text: Option<&str>) -> Self {
        self.text_with_all_modifiers = text.map(str::to_owned);
        self
    }

    pub fn with_key_without_modifiers(mut self, key: Key<String>) -> Self {
        self.key_without_modifiers = key;
        self
    }

    /// Mirrors `KeyEventExtModifierSupplement::text_with_all_modifiers`.
    #[inline]
    pub fn text_with_all_modifiers(&self) -> Option<&str> {
        self.text_with_all_modifiers.as_deref()
    }

    /// Mirrors `KeyEventExtModifierSupplement::key_without_modifiers`.
    #[inline]
    pub fn key_without_modifiers(&self) -> Key<String> {
        self.key_without_modifiers.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_names_every_variant() {
        assert_eq!(Key::<String>::Named(NamedKey::Backspace).to_string(), "Backspace");
        assert_eq!(Key::character("a").to_string(), "Character(a)");
        assert_eq!(Key::<String>::Dead(Some('\u{b4}')).to_string(), "Dead(Some('\u{b4}'))");
        assert_eq!(Key::<String>::Unidentified.to_string(), "Unidentified");
    }

    #[test]
    fn to_text_matches_winit_0_30_13() {
        // winit src/keyboard.rs:1576-1585. The encoder branches on
        // `to_text().is_some()`, so this table decides which keys get an ESC
        // prefix under Alt.
        assert_eq!(NamedKey::Enter.to_text(), Some("\r"));
        assert_eq!(NamedKey::Backspace.to_text(), Some("\x08"));
        assert_eq!(NamedKey::Tab.to_text(), Some("\t"));
        assert_eq!(NamedKey::Space.to_text(), Some(" "));
        assert_eq!(NamedKey::Escape.to_text(), Some("\x1b"));
        for other in [
            NamedKey::ArrowUp,
            NamedKey::F1,
            NamedKey::Delete,
            NamedKey::Insert,
            NamedKey::ContextMenu,
            NamedKey::Shift,
            NamedKey::Control,
            NamedKey::Alt,
            NamedKey::Super,
            NamedKey::CapsLock,
            NamedKey::Home,
            NamedKey::End,
        ] {
            assert_eq!(other.to_text(), None, "{other:?} must not have text");
        }
    }

    /// Three mutants in `regime.rs` replace `|` with `^` in a bitflag union and
    /// SURVIVE the mutation sweep. They are EQUIVALENT MUTANTS -- no test can
    /// kill them -- because the flags involved occupy disjoint bits, and for
    /// disjoint bit sets `a | b` and `a ^ b` are the same value.
    ///
    /// That is an assumption about specific flag values in two different crates,
    /// so it is demonstrated here rather than asserted in a comment. The three
    /// surviving mutants are, verbatim from `cargo mutants`:
    ///
    ///   src/regime.rs:248:46: replace | with ^ in binding_escape
    ///   src/regime.rs:337:13: replace | with ^ in app_keypad_escape
    ///   src/regime.rs:338:13: replace | with ^ in app_keypad_escape
    ///
    /// If a future change ever makes two of these flags share a bit, `|` and `^`
    /// stop agreeing, the mutants stop being equivalent, and this test goes red
    /// before anyone has to rediscover why.
    #[test]
    fn the_three_surviving_mutants_are_provably_equivalent() {
        use alacritty_terminal::term::TermMode;

        // regime.rs:248 -- `mods == ModifiersState::SHIFT | ModifiersState::ALT`
        assert_eq!(
            ModifiersState::SHIFT.bits() & ModifiersState::ALT.bits(),
            0,
            "SHIFT and ALT now overlap"
        );
        assert_eq!(
            (ModifiersState::SHIFT | ModifiersState::ALT).bits(),
            ModifiersState::SHIFT.bits() ^ ModifiersState::ALT.bits(),
            "the regime.rs:248 mutant is no longer equivalent; it must now be killed by a test"
        );

        // regime.rs:337-338 -- the three-flag `kitty_seq` union.
        let kitty = [
            TermMode::REPORT_ALL_KEYS_AS_ESC,
            TermMode::DISAMBIGUATE_ESC_CODES,
            TermMode::REPORT_EVENT_TYPES,
        ];
        for i in 0..kitty.len() {
            for j in (i + 1)..kitty.len() {
                assert_eq!(
                    kitty[i].bits() & kitty[j].bits(),
                    0,
                    "{:?} and {:?} now overlap",
                    kitty[i],
                    kitty[j]
                );
            }
        }
        assert_eq!(
            (kitty[0] | kitty[1] | kitty[2]).bits(),
            kitty[0].bits() ^ kitty[1].bits() ^ kitty[2].bits(),
            "the regime.rs:337/338 mutants are no longer equivalent; they must now be killed"
        );
    }

    #[test]
    fn element_state_is_pressed() {
        assert!(ElementState::Pressed.is_pressed());
        assert!(!ElementState::Released.is_pressed());
    }

    #[test]
    fn modifier_accessors_read_the_right_bit() {
        let m = ModifiersState::SHIFT | ModifiersState::ALT;
        assert!(m.shift_key() && m.alt_key());
        assert!(!m.control_key() && !m.super_key());
        let m = ModifiersState::CONTROL | ModifiersState::SUPER;
        assert!(m.control_key() && m.super_key());
        assert!(!m.shift_key() && !m.alt_key());
    }
}
