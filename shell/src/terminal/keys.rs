//! Moved from `terminal-pane/src/input.rs` on `freeze/terminal-stack` @ `1e715ab` (2026-09-23),
//! unchanged above the `neovibe additions` marker but for one function: [`location_for`] now says
//! which side a modifier is on (GUI pass 2026-09-23, defect 2). Where it says "the caller" or "a
//! session", read `neovibe-terminal`'s `TerminalSession`, which encodes on its own thread against
//! the live mode.
//!
//! GTK keyboard events, normalised into `terminal_input::NormalizedInput`.
//!
//! # Where this stops
//!
//! It stops one function call short of the PTY. Producing bytes is
//! [`terminal_input::encode`]'s job, and that call needs the authoritative terminal's current
//! `TermMode` -- which belongs to whoever owns the `Term`, not to a pane. So the whole of this
//! module's output is a `NormalizedInput`, and the caller writes:
//!
//! ```ignore
//! let bytes = terminal_input::encode(&normalized, session.term_mode());
//! ```
//!
//! That division is not bookkeeping. `terminal-input` is a rule-for-rule transcription of
//! Alacritty's own encoder, differentially tested against it upstream; the kitty protocol,
//! `APP_CURSOR`, the legacy escapes and bracketed paste all live there. Re-deciding any of it here
//! would be the second implementation the architecture exists to prevent.
//!
//! # Where this does not stop, and why that is still correct
//!
//! `terminal_input::keys::KeyEvent` is shaped like winit's, because the encoder it feeds was
//! written against winit's. GTK is not winit, so three of its fields have to be *reconstructed*
//! rather than copied, and each reconstruction is a real decision:
//!
//!   * **`text_with_all_modifiers`** must include the Control transformation -- `Ctrl+A` has to
//!     arrive as `\x01`, not `"a"`. winit gets this from libxkbcommon; GDK does not apply it, so
//!     [`control_transform`] reproduces libxkbcommon's own table. Get this wrong and a legacy
//!     terminal sends the letter instead of the control code: `Ctrl+C` stops interrupting.
//!   * **`key_without_modifiers`** comes from asking GDK to re-translate the hardware keycode with
//!     no modifiers held. It feeds one kitty-protocol branch (the `!` -> `1` base-key lookup).
//!   * **`repeat`** is not surfaced by GTK4 at all. [`RepeatTracker`] recovers it from the press /
//!     release sequence, because a repeat that reports itself as a fresh press is a real fidelity
//!     loss once `REPORT_EVENT_TYPES` is negotiated -- and nvim negotiates it.
//!
//! These are host-adaptation decisions, not terminal semantics: they describe the keyboard, not
//! what a terminal does with it. But they are decisions, they are not observed from a real
//! keyboard yet, and they are written down here so the next person knows where to look when a
//! chord misbehaves.
//!
//! # Testability
//!
//! [`normalize_key`] takes [`RawKey`] -- plain data -- rather than a GTK controller, so every rule
//! above is unit-testable with no display, no `gtk::init`, and no compositor. The only part that
//! genuinely needs a live GDK display is filling in `unmodified_keyval`, which is why that is a
//! caller-supplied field and not something this function goes and fetches.

use gtk4::gdk::{Key as GdkKey, ModifierType};
use gtk4::glib::translate::IntoGlib;
use terminal_input::keys::{ElementState, Key, KeyEvent, KeyLocation, ModifiersState, NamedKey};
use terminal_input::NormalizedInput;

/// One GTK key event, reduced to what the encoder can act on.
///
/// Built by the pane from `EventControllerKey`'s `key-pressed` / `key-released` arguments; built
/// directly by tests.
#[derive(Debug, Clone, Copy)]
pub struct RawKey {
    /// GDK's keyval, with the layout and shift already applied (`A`, not `a` + Shift).
    pub keyval: GdkKey,
    /// The same hardware key translated with no modifiers held, from
    /// `gdk::Display::translate_key(keycode, ModifierType::empty(), 0)`. `None` when the display
    /// could not translate it, in which case `keyval` is used unchanged.
    pub unmodified_keyval: Option<GdkKey>,
    /// The modifier state GTK reported with the event.
    pub state: ModifierType,
    /// Modifiers the layout itself consumed to produce `keyval`, from the same `translate_key`
    /// call. Used only to decide whether the Control transformation applies.
    pub consumed: ModifierType,
    /// Press or release.
    pub pressed: bool,
    /// Whether this press is an auto-repeat. See [`RepeatTracker`].
    pub repeat: bool,
}

/// Recovers the auto-repeat flag GTK4 does not expose.
///
/// The rule is the one the hardware already follows: a press of a key that is still held is a
/// repeat. Tracking the single most recent key is enough -- a keyboard repeats only the last key
/// pressed, and pressing a second key stops the first one repeating.
#[derive(Debug, Default)]
pub struct RepeatTracker {
    held: Option<GdkKey>,
}

impl RepeatTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Records a press and reports whether it is a repeat.
    pub fn press(&mut self, keyval: GdkKey) -> bool {
        let repeat = self.held == Some(keyval);
        self.held = Some(keyval);
        repeat
    }

    /// Records a release.
    pub fn release(&mut self, keyval: GdkKey) {
        if self.held == Some(keyval) {
            self.held = None;
        }
    }

    /// Forgets any held key. Call on focus-out: the release for a key held while focus moved away
    /// is delivered to the *other* widget, so without this the next press of that key would be
    /// reported as a repeat.
    pub fn reset(&mut self) {
        self.held = None;
    }
}

/// Which keyvals this pane has actually forwarded a press for -- so it forwards their release,
/// and only theirs (review 2026-09-23, window lens M2).
///
/// A `key-released` event reaches this pane's controller for a keyval whose PRESS never did, in
/// three real shapes: the key that moved focus in (`Ctrl+j`'s `j`, `Ctrl+a t`'s `t`, a HINT
/// label); the physical `a`/`l` release after `Ctrl+a Ctrl+a`/`Ctrl+a Ctrl+l` already sent a
/// synthesized press+release pair through [`control_letter`]; and `Ctrl+Shift+C`/`V` when Shift is
/// let go before the letter, so [`clipboard_chord`] no longer matches at release time even though
/// it correctly swallowed the press. Gating the release on "was the press actually delivered"
/// fixes all three at once, and does not depend on re-reading the modifier state at release time
/// the way re-checking `clipboard_chord` there did.
///
/// A small `Vec`, not a `HashSet`: at most a handful of keys are ever down on a keyboard at once.
#[derive(Debug, Default)]
pub struct DeliveredKeys {
    held: Vec<GdkKey>,
}

impl DeliveredKeys {
    pub fn new() -> Self {
        Self::default()
    }

    /// Records that this keyval's press was forwarded to the child.
    pub fn mark(&mut self, keyval: GdkKey) {
        if !self.held.contains(&keyval) {
            self.held.push(keyval);
        }
    }

    /// Whether this keyval's press was forwarded -- and forgets it either way, since a release
    /// (delivered or not) ends that keyval's story until its next press.
    pub fn take(&mut self, keyval: GdkKey) -> bool {
        match self.held.iter().position(|&held| held == keyval) {
            Some(index) => {
                self.held.remove(index);
                true
            }
            None => false,
        }
    }

    /// Forgets every held key. Call on focus-out, matching [`RepeatTracker::reset`]: a key held
    /// while focus moves away has its release delivered to whichever widget gets focus next, not
    /// to this pane.
    pub fn reset(&mut self) {
        self.held.clear();
    }
}

/// Normalises one GTK key event.
///
/// Returns `None` only when the key produces neither a name the encoder knows nor any text --
/// a key that cannot be represented, rather than one that has been judged uninteresting. Bare
/// modifier presses are deliberately NOT filtered here: under the kitty protocol they are real
/// input, and deciding otherwise is the encoder's call, not this function's.
pub fn normalize_key(raw: RawKey) -> Option<NormalizedInput> {
    let logical_key = logical_key_for(raw.keyval)?;
    let location = location_for(raw.keyval);
    let mods = modifiers_from_gdk(raw.state);

    let base_text = text_for(&logical_key);
    let text = match base_text {
        // The layout consumed Control to produce this keyval (some layouts put characters on a
        // Control level), so Control is not available to transform the result. libxkbcommon makes
        // the same check before its own transformation.
        Some(t) if mods.control_key() && !raw.consumed.contains(ModifierType::CONTROL_MASK) => {
            Some(control_transform(&t))
        }
        other => other,
    };

    let key_without_modifiers = raw
        .unmodified_keyval
        .and_then(logical_key_for)
        .unwrap_or_else(|| logical_key.clone());

    let event = KeyEvent {
        logical_key,
        location,
        state: if raw.pressed {
            ElementState::Pressed
        } else {
            ElementState::Released
        },
        repeat: raw.repeat,
        text_with_all_modifiers: text,
        key_without_modifiers,
    };
    Some(NormalizedInput::Key { event, mods })
}

/// A paste from the clipboard: bracketed if the application has asked for bracketing.
///
/// `bracketed: true` is a *request*, not a guarantee -- [`terminal_input::encode_paste`] honours it
/// only when the terminal has `BRACKETED_PASTE` set, and otherwise collapses newlines to `\r` so
/// the paste types like a human. Both behaviours belong to the encoder.
#[allow(dead_code)] // phase 2: Ctrl+Shift+V paste and the IME commit (bottom-terminal spec, phase 2)
pub fn normalize_paste(text: impl Into<String>) -> NormalizedInput {
    NormalizedInput::Paste {
        text: text.into(),
        bracketed: true,
    }
}

/// Text committed by an input method.
///
/// Sent with `bracketed: false`, which the encoder passes through byte for byte. That is the
/// correct shape for a commit and not a shortcut: a committed string is what the user typed, so
/// wrapping it in paste brackets would tell the application a human did not type it.
#[allow(dead_code)] // phase 2: Ctrl+Shift+V paste and the IME commit (bottom-terminal spec, phase 2)
pub fn normalize_commit(text: impl Into<String>) -> NormalizedInput {
    NormalizedInput::Paste {
        text: text.into(),
        bracketed: false,
    }
}

fn modifiers_from_gdk(state: ModifierType) -> ModifiersState {
    let mut mods = ModifiersState::empty();
    if state.contains(ModifierType::SHIFT_MASK) {
        mods |= ModifiersState::SHIFT;
    }
    if state.contains(ModifierType::CONTROL_MASK) {
        mods |= ModifiersState::CONTROL;
    }
    if state.contains(ModifierType::ALT_MASK) {
        mods |= ModifiersState::ALT;
    }
    if state.contains(ModifierType::SUPER_MASK) {
        mods |= ModifiersState::SUPER;
    }
    mods
}

fn text_for(key: &Key<String>) -> Option<String> {
    match key {
        Key::Character(text) => Some(text.clone()),
        Key::Named(named) => named.to_text().map(str::to_owned),
        _ => None,
    }
}

/// libxkbcommon's Control transformation, applied per character.
///
/// Transcribed from libxkbcommon's `to_control` (`src/state.c`), which is itself X11's
/// `XLookupString` behaviour. The odd-looking digit rows are real and long-standing: `Ctrl+2` is
/// how a US keyboard types NUL, and `Ctrl+/` is how it types US (0x1f).
///
/// Characters with no control form pass through unchanged, which is what makes `Ctrl+ArrowUp`
/// and `Ctrl+漢` behave sanely.
fn control_transform(text: &str) -> String {
    text.chars().map(control_transform_char).collect()
}

fn control_transform_char(ch: char) -> char {
    match ch {
        '@'..='~' | ' ' => char::from_u32(ch as u32 & 0x1f).unwrap_or(ch),
        '2' => '\0',
        '3'..='7' => char::from_u32(ch as u32 - ('3' as u32 - 0x1b)).unwrap_or(ch),
        '8' => '\x7f',
        '/' => '\x1f',
        _ => ch,
    }
}

/// Numpad keyvals occupy one contiguous GDK block, `KP_Space` (0xff80) through `KP_Equal`
/// (0xffbd). The encoder needs the distinction for `APP_KEYPAD` and for the kitty numpad
/// codepoints, and this is the only place the information exists -- GTK has no key-location field.
///
/// A modifier's side is in its keyval (`Control_L`, `Control_R`), and the encoder needs that too:
/// under `REPORT_ALL_KEYS_AS_ESC` (kitty flag 8) a bare modifier is reported by its own code, and
/// the vendored table sends every modifier whose location is not `Left` as the RIGHT one (`(Control,
/// Left) => 57442`, `(Control, _) => 57448`). Reporting `Standard` for all of them, as the frozen
/// module did, turned every left Control and Shift into a right one (GUI pass 2026-09-23, defect 2:
/// 57448/57447 for Left Control/Shift). winit, which the encoder was written against, reports
/// `Left`/`Right` here too.
fn location_for(keyval: GdkKey) -> KeyLocation {
    match keyval {
        GdkKey::Shift_L | GdkKey::Control_L | GdkKey::Alt_L | GdkKey::Super_L | GdkKey::Meta_L | GdkKey::Hyper_L => {
            return KeyLocation::Left
        }
        GdkKey::Shift_R | GdkKey::Control_R | GdkKey::Alt_R | GdkKey::Super_R | GdkKey::Meta_R | GdkKey::Hyper_R => {
            return KeyLocation::Right
        }
        _ => {}
    }
    let raw = keyval.into_glib();
    if (GdkKey::KP_Space.into_glib()..=GdkKey::KP_Equal.into_glib()).contains(&raw) {
        KeyLocation::Numpad
    } else {
        KeyLocation::Standard
    }
}

/// GDK keyval -> the encoder's logical key.
///
/// Named keys are matched first, because several of them (`Return`, `Tab`, `BackSpace`, `space`)
/// also have a Unicode value, and the encoder's behaviour differs sharply between the two: a
/// `Key::Character("\r")` misses every legacy-`Enter` rule in the binding table.
///
/// Numpad keys map to the *same* logical key as their main-keyboard twin, with the numpad-ness
/// carried by `location` instead -- that is winit's split too, and the encoder reads both fields.
fn logical_key_for(keyval: GdkKey) -> Option<Key<String>> {
    if let Some(named) = named_key_for(keyval) {
        return Some(Key::Named(named));
    }
    // Numpad digits and operators are characters whose GDK keyvals sit outside the Latin-1 block,
    // so `to_unicode` cannot be relied on for them.
    let numpad_char = match keyval {
        GdkKey::KP_0 => Some('0'),
        GdkKey::KP_1 => Some('1'),
        GdkKey::KP_2 => Some('2'),
        GdkKey::KP_3 => Some('3'),
        GdkKey::KP_4 => Some('4'),
        GdkKey::KP_5 => Some('5'),
        GdkKey::KP_6 => Some('6'),
        GdkKey::KP_7 => Some('7'),
        GdkKey::KP_8 => Some('8'),
        GdkKey::KP_9 => Some('9'),
        GdkKey::KP_Decimal | GdkKey::KP_Separator => Some('.'),
        GdkKey::KP_Divide => Some('/'),
        GdkKey::KP_Multiply => Some('*'),
        GdkKey::KP_Subtract => Some('-'),
        GdkKey::KP_Add => Some('+'),
        GdkKey::KP_Equal => Some('='),
        _ => None,
    };
    if let Some(ch) = numpad_char {
        return Some(Key::Character(ch.to_string()));
    }
    // `to_unicode` already reports `None` (not NUL) for a keyval with no character.
    keyval.to_unicode().map(|ch| Key::Character(ch.to_string()))
}

fn named_key_for(keyval: GdkKey) -> Option<NamedKey> {
    let named = match keyval {
        GdkKey::Escape => NamedKey::Escape,
        GdkKey::Return | GdkKey::ISO_Enter | GdkKey::KP_Enter => NamedKey::Enter,
        GdkKey::BackSpace => NamedKey::Backspace,
        GdkKey::Tab | GdkKey::ISO_Left_Tab | GdkKey::KP_Tab => NamedKey::Tab,
        GdkKey::space | GdkKey::KP_Space => NamedKey::Space,
        GdkKey::Delete | GdkKey::KP_Delete => NamedKey::Delete,
        GdkKey::Insert | GdkKey::KP_Insert => NamedKey::Insert,
        GdkKey::Up | GdkKey::KP_Up => NamedKey::ArrowUp,
        GdkKey::Down | GdkKey::KP_Down => NamedKey::ArrowDown,
        GdkKey::Left | GdkKey::KP_Left => NamedKey::ArrowLeft,
        GdkKey::Right | GdkKey::KP_Right => NamedKey::ArrowRight,
        GdkKey::Home | GdkKey::KP_Home => NamedKey::Home,
        GdkKey::End | GdkKey::KP_End => NamedKey::End,
        GdkKey::Page_Up | GdkKey::KP_Page_Up => NamedKey::PageUp,
        GdkKey::Page_Down | GdkKey::KP_Page_Down => NamedKey::PageDown,
        GdkKey::Menu => NamedKey::ContextMenu,
        GdkKey::Pause => NamedKey::Pause,
        GdkKey::Print => NamedKey::PrintScreen,
        GdkKey::Caps_Lock => NamedKey::CapsLock,
        GdkKey::Num_Lock => NamedKey::NumLock,
        GdkKey::Scroll_Lock => NamedKey::ScrollLock,
        GdkKey::Shift_L | GdkKey::Shift_R => NamedKey::Shift,
        GdkKey::Control_L | GdkKey::Control_R => NamedKey::Control,
        GdkKey::Alt_L | GdkKey::Alt_R => NamedKey::Alt,
        GdkKey::Super_L | GdkKey::Super_R => NamedKey::Super,
        GdkKey::Meta_L | GdkKey::Meta_R => NamedKey::Meta,
        GdkKey::Hyper_L | GdkKey::Hyper_R => NamedKey::Hyper,
        GdkKey::F1 => NamedKey::F1,
        GdkKey::F2 => NamedKey::F2,
        GdkKey::F3 => NamedKey::F3,
        GdkKey::F4 => NamedKey::F4,
        GdkKey::F5 => NamedKey::F5,
        GdkKey::F6 => NamedKey::F6,
        GdkKey::F7 => NamedKey::F7,
        GdkKey::F8 => NamedKey::F8,
        GdkKey::F9 => NamedKey::F9,
        GdkKey::F10 => NamedKey::F10,
        GdkKey::F11 => NamedKey::F11,
        GdkKey::F12 => NamedKey::F12,
        _ => return None,
    };
    Some(named)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(keyval: GdkKey, state: ModifierType) -> RawKey {
        RawKey {
            keyval,
            unmodified_keyval: None,
            state,
            consumed: ModifierType::empty(),
            pressed: true,
            repeat: false,
        }
    }

    fn key_of(input: &NormalizedInput) -> &KeyEvent {
        match input {
            NormalizedInput::Key { event, .. } => event,
            other => panic!("expected a key event, got {other:?}"),
        }
    }

    fn mods_of(input: &NormalizedInput) -> ModifiersState {
        match input {
            NormalizedInput::Key { mods, .. } => *mods,
            other => panic!("expected a key event, got {other:?}"),
        }
    }

    #[test]
    fn a_plain_letter_carries_its_own_text() {
        let out = normalize_key(raw(GdkKey::a, ModifierType::empty())).unwrap();
        assert_eq!(key_of(&out).logical_key, Key::character("a"));
        assert_eq!(key_of(&out).text_with_all_modifiers(), Some("a"));
        assert_eq!(mods_of(&out), ModifiersState::empty());
    }

    /// The one that matters most. A legacy terminal never calls `build_sequence` for a plain
    /// character, so if this text were `"a"` rather than `\x01`, `Ctrl+C` would type a `c` and
    /// nothing would ever be interruptible.
    #[test]
    fn control_letters_become_control_codes() {
        let out = normalize_key(raw(GdkKey::a, ModifierType::CONTROL_MASK)).unwrap();
        assert_eq!(key_of(&out).text_with_all_modifiers(), Some("\u{1}"));
        assert!(mods_of(&out).control_key());

        let out = normalize_key(raw(GdkKey::c, ModifierType::CONTROL_MASK)).unwrap();
        assert_eq!(key_of(&out).text_with_all_modifiers(), Some("\u{3}"));
    }

    #[test]
    fn control_transform_covers_the_non_letter_rows() {
        assert_eq!(control_transform("2"), "\0");
        assert_eq!(control_transform("3"), "\x1b");
        assert_eq!(control_transform("7"), "\x1f");
        assert_eq!(control_transform("8"), "\x7f");
        assert_eq!(control_transform("/"), "\x1f");
        assert_eq!(control_transform(" "), "\0");
        assert_eq!(control_transform("["), "\x1b");
        // No control form: passes through, rather than being mangled into one.
        assert_eq!(control_transform("漢"), "漢");
        assert_eq!(control_transform("1"), "1");
    }

    /// A layout that already used Control to reach this character has none left to transform with.
    #[test]
    fn a_consumed_control_does_not_transform() {
        let mut r = raw(GdkKey::a, ModifierType::CONTROL_MASK);
        r.consumed = ModifierType::CONTROL_MASK;
        let out = normalize_key(r).unwrap();
        assert_eq!(key_of(&out).text_with_all_modifiers(), Some("a"));
    }

    /// Enter must not arrive as `Character("\r")`: every legacy-Enter rule in the encoder's
    /// binding table matches on `Key::Named(NamedKey::Enter)` and would miss it.
    #[test]
    fn named_keys_win_over_their_unicode_values() {
        for (keyval, expected) in [
            (GdkKey::Return, NamedKey::Enter),
            (GdkKey::Tab, NamedKey::Tab),
            (GdkKey::BackSpace, NamedKey::Backspace),
            (GdkKey::space, NamedKey::Space),
            (GdkKey::Escape, NamedKey::Escape),
        ] {
            let out = normalize_key(raw(keyval, ModifierType::empty())).unwrap();
            assert_eq!(key_of(&out).logical_key, Key::Named(expected), "{keyval:?}");
        }
    }

    #[test]
    fn arrows_and_function_keys_are_named_and_textless() {
        let out = normalize_key(raw(GdkKey::Up, ModifierType::empty())).unwrap();
        assert_eq!(key_of(&out).logical_key, Key::Named(NamedKey::ArrowUp));
        assert_eq!(key_of(&out).text_with_all_modifiers(), None);

        let out = normalize_key(raw(GdkKey::F5, ModifierType::empty())).unwrap();
        assert_eq!(key_of(&out).logical_key, Key::Named(NamedKey::F5));
        assert_eq!(key_of(&out).text_with_all_modifiers(), None);
    }

    /// Numpad-ness rides on `location`, not on a different logical key -- and the digits must
    /// still be characters, because the kitty numpad table matches `Key::Character("4")`.
    #[test]
    fn numpad_keys_are_located_not_renamed() {
        let out = normalize_key(raw(GdkKey::KP_Left, ModifierType::empty())).unwrap();
        assert_eq!(key_of(&out).logical_key, Key::Named(NamedKey::ArrowLeft));
        assert_eq!(key_of(&out).location, KeyLocation::Numpad);

        let out = normalize_key(raw(GdkKey::KP_4, ModifierType::empty())).unwrap();
        assert_eq!(key_of(&out).logical_key, Key::character("4"));
        assert_eq!(key_of(&out).location, KeyLocation::Numpad);

        let out = normalize_key(raw(GdkKey::KP_Enter, ModifierType::empty())).unwrap();
        assert_eq!(key_of(&out).logical_key, Key::Named(NamedKey::Enter));
        assert_eq!(key_of(&out).location, KeyLocation::Numpad);

        // And an ordinary key is not swept into the numpad block by an over-wide range check.
        let out = normalize_key(raw(GdkKey::Left, ModifierType::empty())).unwrap();
        assert_eq!(key_of(&out).location, KeyLocation::Standard);
    }

    /// Bare modifiers reach the encoder. Under `REPORT_ALL_KEYS_AS_ESC` they are real input, and
    /// the decision to ignore them is the encoder's to make, not this layer's.
    #[test]
    fn bare_modifier_presses_are_not_swallowed_here() {
        let out = normalize_key(raw(GdkKey::Control_L, ModifierType::empty())).unwrap();
        assert_eq!(key_of(&out).logical_key, Key::Named(NamedKey::Control));
        assert!(terminal_input::is_modifier_key(key_of(&out)));
    }

    #[test]
    fn an_unrepresentable_key_is_dropped_rather_than_guessed_at() {
        // ISO_Level3_Shift produces no text and has no NamedKey counterpart.
        assert!(normalize_key(raw(GdkKey::ISO_Level3_Shift, ModifierType::empty())).is_none());
    }

    #[test]
    fn releases_are_marked_as_releases() {
        let mut r = raw(GdkKey::a, ModifierType::empty());
        r.pressed = false;
        let out = normalize_key(r).unwrap();
        assert_eq!(key_of(&out).state, ElementState::Released);
    }

    /// The shifted character carries the shift, and `key_without_modifiers` carries the base key --
    /// the pair the kitty protocol needs to report `!` as "shift plus the `1` key".
    #[test]
    fn the_unmodified_keyval_supplies_the_base_key() {
        let mut r = raw(GdkKey::exclam, ModifierType::SHIFT_MASK);
        r.unmodified_keyval = Some(GdkKey::_1);
        let out = normalize_key(r).unwrap();
        assert_eq!(key_of(&out).logical_key, Key::character("!"));
        assert_eq!(key_of(&out).key_without_modifiers, Key::character("1"));
    }

    /// Without a display to translate with, the base key falls back to the logical key rather than
    /// to something invented.
    #[test]
    fn a_missing_unmodified_keyval_falls_back_to_the_logical_key() {
        let out = normalize_key(raw(GdkKey::exclam, ModifierType::SHIFT_MASK)).unwrap();
        assert_eq!(key_of(&out).key_without_modifiers, Key::character("!"));
    }

    #[test]
    fn modifiers_map_across_all_four() {
        let state =
            ModifierType::SHIFT_MASK | ModifierType::CONTROL_MASK | ModifierType::ALT_MASK | ModifierType::SUPER_MASK;
        let mods = mods_of(&normalize_key(raw(GdkKey::Up, state)).unwrap());
        assert_eq!(
            mods,
            ModifiersState::SHIFT | ModifiersState::CONTROL | ModifiersState::ALT | ModifiersState::SUPER
        );
    }

    #[test]
    fn a_paste_requests_bracketing_and_a_commit_does_not() {
        assert_eq!(
            normalize_paste("hi"),
            NormalizedInput::Paste {
                text: "hi".into(),
                bracketed: true
            }
        );
        assert_eq!(
            normalize_commit("你好"),
            NormalizedInput::Paste {
                text: "你好".into(),
                bracketed: false
            }
        );
    }

    #[test]
    fn repeat_is_the_second_press_of_a_still_held_key() {
        let mut tracker = RepeatTracker::new();
        assert!(!tracker.press(GdkKey::a));
        assert!(tracker.press(GdkKey::a));
        assert!(tracker.press(GdkKey::a));
        tracker.release(GdkKey::a);
        assert!(!tracker.press(GdkKey::a), "a released key repeats nothing");
    }

    #[test]
    fn a_different_key_interrupts_the_repeat() {
        let mut tracker = RepeatTracker::new();
        tracker.press(GdkKey::a);
        assert!(!tracker.press(GdkKey::b));
        assert!(!tracker.press(GdkKey::a), "the a-repeat ended when b was pressed");
    }

    /// Focus-out eats the release, so without a reset the next press of that key would be reported
    /// as a repeat -- which under the kitty protocol is a visibly different event.
    #[test]
    fn focus_out_clears_the_held_key() {
        let mut tracker = RepeatTracker::new();
        tracker.press(GdkKey::a);
        tracker.reset();
        assert!(!tracker.press(GdkKey::a));
    }

    /// Review 2026-09-23, M2. A release must be forwarded only for a keyval whose press this pane
    /// actually forwarded.
    #[test]
    fn a_release_is_forwarded_only_for_a_keyval_whose_press_was() {
        let mut delivered = DeliveredKeys::new();
        // The ordinary case: press marked, its release takes it.
        delivered.mark(GdkKey::a);
        assert!(delivered.take(GdkKey::a));
        // The key that moved focus in, or any other press this pane never saw: no press was ever
        // marked, so the release is not forwarded.
        assert!(!delivered.take(GdkKey::j));
        // A release, delivered or not, ends that keyval's story -- a second release of the same
        // keyval (the real one after `control_letter`'s synthesized pair) finds nothing.
        delivered.mark(GdkKey::l);
        assert!(delivered.take(GdkKey::l));
        assert!(!delivered.take(GdkKey::l));
        // Two keys held at once are tracked independently.
        delivered.mark(GdkKey::a);
        delivered.mark(GdkKey::b);
        assert!(delivered.take(GdkKey::a));
        assert!(delivered.take(GdkKey::b));
    }

    #[test]
    fn focus_out_clears_every_delivered_keyval() {
        let mut delivered = DeliveredKeys::new();
        delivered.mark(GdkKey::a);
        delivered.mark(GdkKey::b);
        delivered.reset();
        assert!(!delivered.take(GdkKey::a));
        assert!(!delivered.take(GdkKey::b));
    }
}

// ---- neovibe additions (2026-09-23): what the bottom terminal needs beyond the frozen module ----

/// `Ctrl+Shift+C` or `Ctrl+Shift+V`: foot's copy and paste, which the owner's `foot.ini` keeps, so
/// his hands will press them here. Until phase 2 fills them in, the pane swallows both: passed on,
/// the encoder turns `Ctrl+Shift+C` into 0x03 -- SIGINT to whatever is running -- and
/// `Ctrl+Shift+V` into 0x16, his zsh's `quoted-insert` (review 2026-09-23, finding 8).
pub(crate) fn clipboard_chord(keyval: GdkKey, state: ModifierType) -> bool {
    let others = ModifierType::ALT_MASK | ModifierType::SUPER_MASK | ModifierType::META_MASK;
    state.contains(ModifierType::CONTROL_MASK | ModifierType::SHIFT_MASK)
        && !state.intersects(others)
        && matches!(keyval.to_lower(), GdkKey::c | GdkKey::v)
}

/// `Ctrl+letter` pressed then released, exactly as [`normalize_key`] turns a real one. What
/// `Ctrl+a Ctrl+a` and `Ctrl+a Ctrl+l` hand the terminal. It goes through `terminal_input::encode`
/// like a typed key -- never as a raw byte -- so a program that has switched on the kitty keyboard
/// protocol gets the encoding it asked for.
pub(crate) fn control_letter(letter: char) -> [NormalizedInput; 2] {
    let text = control_transform(&letter.to_string());
    let press = KeyEvent::press(Key::Character(letter.to_string())).with_text(Some(&text));
    let release = KeyEvent {
        state: ElementState::Released,
        ..press.clone()
    };
    [
        NormalizedInput::Key {
            event: press,
            mods: ModifiersState::CONTROL,
        },
        NormalizedInput::Key {
            event: release,
            mods: ModifiersState::CONTROL,
        },
    ]
}

/// Whether a key press restarts a shell that has exited (spec §4.6: the pane holds the last screen,
/// Enter restarts). Plain Enter only: a chord is never a restart.
pub(crate) fn restarts(keyval: GdkKey, state: ModifierType) -> bool {
    let chords =
        ModifierType::CONTROL_MASK | ModifierType::ALT_MASK | ModifierType::SUPER_MASK | ModifierType::SHIFT_MASK;
    matches!(keyval, GdkKey::Return | GdkKey::KP_Enter | GdkKey::ISO_Enter) && !state.intersects(chords)
}

#[cfg(test)]
mod neovibe_tests {
    use super::*;

    fn press(keyval: GdkKey, state: ModifierType) -> RawKey {
        RawKey {
            keyval,
            unmodified_keyval: None,
            state,
            consumed: ModifierType::empty(),
            pressed: true,
            repeat: false,
        }
    }

    #[test]
    fn a_literal_control_letter_encodes_as_a_typed_one_does() {
        let [down, up] = control_letter('a');
        assert_eq!(terminal_input::encode(&down, Default::default()), vec![0x01]);
        assert_eq!(terminal_input::encode(&up, Default::default()), Vec::<u8>::new());
        let [down, _] = control_letter('l');
        assert_eq!(terminal_input::encode(&down, Default::default()), vec![0x0c]);
        let typed = normalize_key(press(GdkKey::a, ModifierType::CONTROL_MASK)).unwrap();
        assert_eq!(
            control_letter('a')[0],
            typed,
            "the literal is the typed key, byte for byte and field for field"
        );
    }

    #[test]
    fn ctrl_shift_c_and_v_are_held_back_and_ctrl_c_is_not() {
        let ctrl_shift = ModifierType::CONTROL_MASK | ModifierType::SHIFT_MASK;
        assert!(clipboard_chord(GdkKey::C, ctrl_shift));
        assert!(clipboard_chord(GdkKey::V, ctrl_shift));
        assert!(
            !clipboard_chord(GdkKey::c, ModifierType::CONTROL_MASK),
            "Ctrl+C is the shell's interrupt"
        );
        assert!(!clipboard_chord(GdkKey::C, ctrl_shift | ModifierType::ALT_MASK));
        assert!(!clipboard_chord(GdkKey::X, ctrl_shift));
        // What reaches the shell if the pane passed it on: the interrupt character.
        let raw = RawKey {
            keyval: GdkKey::C,
            unmodified_keyval: Some(GdkKey::c),
            state: ctrl_shift,
            consumed: ModifierType::SHIFT_MASK,
            pressed: true,
            repeat: false,
        };
        let input = normalize_key(raw).unwrap();
        assert_eq!(terminal_input::encode(&input, Default::default()), vec![0x03]);
    }

    /// GUI pass 2026-09-23, defect 2: under kitty flag 8 (`REPORT_ALL_KEYS_AS_ESC`), a bare left
    /// modifier was reported with the RIGHT modifier's code, because every modifier went to the
    /// encoder as `KeyLocation::Standard`. Checked through the real encoder, not just the field: the
    /// codes are the kitty protocol's (57441-57452), and a wrong side is a wrong byte.
    #[test]
    fn a_bare_modifier_is_reported_with_its_own_side_under_report_all_keys() {
        use alacritty_terminal::term::TermMode;
        let report_all = TermMode::DISAMBIGUATE_ESC_CODES | TermMode::REPORT_ALL_KEYS_AS_ESC;
        for (keyval, location, code) in [
            (GdkKey::Shift_L, KeyLocation::Left, 57441),
            (GdkKey::Control_L, KeyLocation::Left, 57442),
            (GdkKey::Alt_L, KeyLocation::Left, 57443),
            (GdkKey::Super_L, KeyLocation::Left, 57444),
            (GdkKey::Shift_R, KeyLocation::Right, 57447),
            (GdkKey::Control_R, KeyLocation::Right, 57448),
            (GdkKey::Alt_R, KeyLocation::Right, 57449),
            (GdkKey::Super_R, KeyLocation::Right, 57450),
        ] {
            let input = normalize_key(press(keyval, ModifierType::empty())).unwrap();
            let NormalizedInput::Key { event, .. } = &input else {
                panic!("{keyval:?} is a key")
            };
            assert_eq!(event.location, location, "{keyval:?}");
            let bytes = String::from_utf8(terminal_input::encode(&input, report_all)).unwrap();
            assert!(
                bytes.starts_with(&format!("\x1b[{code};")) || bytes == format!("\x1b[{code}u"),
                "{keyval:?} encoded as {bytes:?}, not as kitty code {code}"
            );
        }
        // Nothing else moved: an ordinary key is still `Standard`, a numpad key still `Numpad`.
        let letter = normalize_key(press(GdkKey::a, ModifierType::empty())).unwrap();
        let keypad = normalize_key(press(GdkKey::KP_4, ModifierType::empty())).unwrap();
        for (input, location) in [(letter, KeyLocation::Standard), (keypad, KeyLocation::Numpad)] {
            let NormalizedInput::Key { event, .. } = input else {
                panic!("a key")
            };
            assert_eq!(event.location, location);
        }
    }

    #[test]
    fn only_a_plain_enter_restarts_an_exited_shell() {
        assert!(restarts(GdkKey::Return, ModifierType::empty()));
        assert!(restarts(GdkKey::KP_Enter, ModifierType::empty()));
        assert!(!restarts(GdkKey::Return, ModifierType::CONTROL_MASK));
        assert!(!restarts(GdkKey::space, ModifierType::empty()));
        assert!(
            !restarts(GdkKey::j, ModifierType::CONTROL_MASK),
            "Ctrl+j is LF, not Enter"
        );
    }
}
