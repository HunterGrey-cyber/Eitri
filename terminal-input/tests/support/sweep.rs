//! The sweep space: the cross product the differential and golden tests walk.
//!
//! Axes, and why each one is in:
//!
//! * MODE -- all 256 combinations of the eight `TermMode` flags the encoder can
//!   branch on: `APP_CURSOR`, `APP_KEYPAD`, `DISAMBIGUATE_ESC_CODES`,
//!   `REPORT_EVENT_TYPES`, `REPORT_ALTERNATE_KEYS`, `REPORT_ALL_KEYS_AS_ESC`,
//!   `REPORT_ASSOCIATED_TEXT`, `VI`. Exhaustive, not sampled: the defects this
//!   gate exists for (D3/D4) live in a two-flag interaction that a hand-picked
//!   mode list misses.
//! * MODS -- all 16 subsets of SHIFT/CONTROL/ALT/SUPER. Exhaustive. The `mods ==
//!   ModifiersState::ALT` vs `mods.contains(ALT)` defect is only visible at
//!   |mods| >= 2.
//! * KEY SHAPE -- every `NamedKey` variant at `Standard`; every numpad-reachable
//!   key at `Numpad`; every modifier key at `Left` and `Right`; a spread of
//!   characters including the control-char and multi-codepoint shapes; `Dead`
//!   and `Unidentified`.
//! * STATE -- Pressed, Pressed+repeat, Released. Releases are in the sweep
//!   because omitting `key_release` was the worst defect of the earlier attempt
//!   and a press-only sweep cannot see it.

#![allow(dead_code)]

use alacritty_terminal::term::TermMode;
use terminal_input::keys::{ElementState, Key, KeyEvent, KeyLocation, ModifiersState, NamedKey};

/// The eight mode flags the encoder branches on.
pub const MODE_FLAGS: [(&str, TermMode); 8] = [
    ("APP_CURSOR", TermMode::APP_CURSOR),
    ("APP_KEYPAD", TermMode::APP_KEYPAD),
    ("DISAMBIGUATE_ESC_CODES", TermMode::DISAMBIGUATE_ESC_CODES),
    ("REPORT_EVENT_TYPES", TermMode::REPORT_EVENT_TYPES),
    ("REPORT_ALTERNATE_KEYS", TermMode::REPORT_ALTERNATE_KEYS),
    ("REPORT_ALL_KEYS_AS_ESC", TermMode::REPORT_ALL_KEYS_AS_ESC),
    ("REPORT_ASSOCIATED_TEXT", TermMode::REPORT_ASSOCIATED_TEXT),
    ("VI", TermMode::VI),
];

pub const MOD_FLAGS: [(&str, ModifiersState); 4] = [
    ("shift", ModifiersState::SHIFT),
    ("ctrl", ModifiersState::CONTROL),
    ("alt", ModifiersState::ALT),
    ("super", ModifiersState::SUPER),
];

/// All 256 mode combinations, each on top of `TermMode::default()`.
pub fn modes() -> Vec<(String, TermMode)> {
    let mut out = Vec::with_capacity(256);
    for bits in 0u32..(1 << MODE_FLAGS.len()) {
        let mut mode = TermMode::default();
        let mut label = String::new();
        for (i, (name, flag)) in MODE_FLAGS.iter().enumerate() {
            if bits & (1 << i) != 0 {
                mode |= *flag;
                if !label.is_empty() {
                    label.push('+');
                }
                label.push_str(name);
            }
        }
        if label.is_empty() {
            label.push_str("base");
        }
        out.push((label, mode));
    }
    out
}

/// All 16 modifier combinations.
pub fn mod_sets() -> Vec<(String, ModifiersState)> {
    let mut out = Vec::with_capacity(16);
    for bits in 0u32..(1 << MOD_FLAGS.len()) {
        let mut mods = ModifiersState::empty();
        let mut label = String::new();
        for (i, (name, flag)) in MOD_FLAGS.iter().enumerate() {
            if bits & (1 << i) != 0 {
                mods |= *flag;
                if !label.is_empty() {
                    label.push('+');
                }
                label.push_str(name);
            }
        }
        if label.is_empty() {
            label.push_str("none");
        }
        out.push((label, mods));
    }
    out
}

/// Every `NamedKey` this crate declares. Kept as an explicit list so that adding
/// a variant to the shim without adding it here is visible in the count.
pub const ALL_NAMED: [(&str, NamedKey); 74] = [
    ("Alt", NamedKey::Alt),
    ("CapsLock", NamedKey::CapsLock),
    ("Control", NamedKey::Control),
    ("Hyper", NamedKey::Hyper),
    ("Meta", NamedKey::Meta),
    ("NumLock", NamedKey::NumLock),
    ("ScrollLock", NamedKey::ScrollLock),
    ("Shift", NamedKey::Shift),
    ("Super", NamedKey::Super),
    ("Enter", NamedKey::Enter),
    ("Tab", NamedKey::Tab),
    ("Space", NamedKey::Space),
    ("Backspace", NamedKey::Backspace),
    ("Delete", NamedKey::Delete),
    ("Insert", NamedKey::Insert),
    ("Escape", NamedKey::Escape),
    ("ArrowDown", NamedKey::ArrowDown),
    ("ArrowLeft", NamedKey::ArrowLeft),
    ("ArrowRight", NamedKey::ArrowRight),
    ("ArrowUp", NamedKey::ArrowUp),
    ("End", NamedKey::End),
    ("Home", NamedKey::Home),
    ("PageDown", NamedKey::PageDown),
    ("PageUp", NamedKey::PageUp),
    ("ContextMenu", NamedKey::ContextMenu),
    ("Pause", NamedKey::Pause),
    ("PrintScreen", NamedKey::PrintScreen),
    ("AudioVolumeDown", NamedKey::AudioVolumeDown),
    ("AudioVolumeMute", NamedKey::AudioVolumeMute),
    ("AudioVolumeUp", NamedKey::AudioVolumeUp),
    ("MediaFastForward", NamedKey::MediaFastForward),
    ("MediaPause", NamedKey::MediaPause),
    ("MediaPlay", NamedKey::MediaPlay),
    ("MediaPlayPause", NamedKey::MediaPlayPause),
    ("MediaRecord", NamedKey::MediaRecord),
    ("MediaRewind", NamedKey::MediaRewind),
    ("MediaStop", NamedKey::MediaStop),
    ("MediaTrackNext", NamedKey::MediaTrackNext),
    ("MediaTrackPrevious", NamedKey::MediaTrackPrevious),
    ("F1", NamedKey::F1),
    ("F2", NamedKey::F2),
    ("F3", NamedKey::F3),
    ("F4", NamedKey::F4),
    ("F5", NamedKey::F5),
    ("F6", NamedKey::F6),
    ("F7", NamedKey::F7),
    ("F8", NamedKey::F8),
    ("F9", NamedKey::F9),
    ("F10", NamedKey::F10),
    ("F11", NamedKey::F11),
    ("F12", NamedKey::F12),
    ("F13", NamedKey::F13),
    ("F14", NamedKey::F14),
    ("F15", NamedKey::F15),
    ("F16", NamedKey::F16),
    ("F17", NamedKey::F17),
    ("F18", NamedKey::F18),
    ("F19", NamedKey::F19),
    ("F20", NamedKey::F20),
    ("F21", NamedKey::F21),
    ("F22", NamedKey::F22),
    ("F23", NamedKey::F23),
    ("F24", NamedKey::F24),
    ("F25", NamedKey::F25),
    ("F26", NamedKey::F26),
    ("F27", NamedKey::F27),
    ("F28", NamedKey::F28),
    ("F29", NamedKey::F29),
    ("F30", NamedKey::F30),
    ("F31", NamedKey::F31),
    ("F32", NamedKey::F32),
    ("F33", NamedKey::F33),
    ("F34", NamedKey::F34),
    ("F35", NamedKey::F35),
];

/// Keys a physical numeric keypad can produce.
pub const NUMPAD_CHARS: [&str; 16] = [
    "0", "1", "2", "3", "4", "5", "6", "7", "8", "9", ".", "/", "*", "-", "+", "=",
];

pub const NUMPAD_NAMED: [(&str, NamedKey); 11] = [
    ("Enter", NamedKey::Enter),
    ("ArrowLeft", NamedKey::ArrowLeft),
    ("ArrowRight", NamedKey::ArrowRight),
    ("ArrowUp", NamedKey::ArrowUp),
    ("ArrowDown", NamedKey::ArrowDown),
    ("PageUp", NamedKey::PageUp),
    ("PageDown", NamedKey::PageDown),
    ("Home", NamedKey::Home),
    ("End", NamedKey::End),
    ("Insert", NamedKey::Insert),
    ("Delete", NamedKey::Delete),
];

pub const SIDED_MODIFIERS: [(&str, NamedKey); 6] = [
    ("Shift", NamedKey::Shift),
    ("Control", NamedKey::Control),
    ("Alt", NamedKey::Alt),
    ("Super", NamedKey::Super),
    ("Hyper", NamedKey::Hyper),
    ("Meta", NamedKey::Meta),
];

/// One key "shape": a fully built event modulo press/release/repeat.
pub struct Shape {
    pub label: String,
    pub key: Key<String>,
    pub location: KeyLocation,
    pub text: Option<String>,
    pub key_without_modifiers: Key<String>,
}

fn shape(label: &str, key: Key<String>, location: KeyLocation, text: Option<&str>) -> Shape {
    Shape {
        label: label.to_owned(),
        key_without_modifiers: key.clone(),
        key,
        location,
        text: text.map(str::to_owned),
    }
}

/// Every key shape in the sweep.
pub fn shapes() -> Vec<Shape> {
    let mut v = Vec::new();

    // Every named key at the standard location, with winit's own text mapping.
    for (name, nk) in ALL_NAMED {
        v.push(shape(
            &format!("Named/{name}"),
            Key::Named(nk),
            KeyLocation::Standard,
            nk.to_text(),
        ));
    }

    // Numpad.
    for ch in NUMPAD_CHARS {
        v.push(shape(
            &format!("Numpad/char {ch}"),
            Key::Character(ch.to_owned()),
            KeyLocation::Numpad,
            Some(ch),
        ));
    }
    for (name, nk) in NUMPAD_NAMED {
        v.push(shape(
            &format!("Numpad/Named {name}"),
            Key::Named(nk),
            KeyLocation::Numpad,
            nk.to_text(),
        ));
    }

    // Left/right-sided modifier keys: `try_build_control_char_or_mod` keys off
    // location, so Left and Right encode differently.
    for (name, nk) in SIDED_MODIFIERS {
        v.push(shape(
            &format!("Left/{name}"),
            Key::Named(nk),
            KeyLocation::Left,
            nk.to_text(),
        ));
        v.push(shape(
            &format!("Right/{name}"),
            Key::Named(nk),
            KeyLocation::Right,
            nk.to_text(),
        ));
    }

    // Characters. `text` and `key_without_modifiers` are set to what a real
    // layout would report, because both feed real branches:
    // `try_build_textual` consults `key_without_modifiers` only when SHIFT is set
    // and the shifted/unshifted codepoints agree, and `REPORT_ASSOCIATED_TEXT`
    // consults `is_control_character(text)`.
    let chars: &[(&str, &str, Option<&str>, &str)] = &[
        // (label, logical, text, key_without_modifiers)
        ("char/a", "a", Some("a"), "a"),
        ("char/A", "A", Some("A"), "a"),
        ("char/z", "z", Some("z"), "z"),
        ("char/1", "1", Some("1"), "1"),
        ("char/!", "!", Some("!"), "1"),
        ("char/0", "0", Some("0"), "0"),
        ("char/space-as-char", " ", Some(" "), " "),
        ("char/dash", "-", Some("-"), "-"),
        ("char/equals", "=", Some("="), "="),
        ("char/plus", "+", Some("+"), "="),
        ("char/eacute", "\u{e9}", Some("\u{e9}"), "\u{e9}"),
        ("char/cyrillic", "\u{444}", Some("\u{444}"), "\u{444}"),
        ("char/emoji", "\u{1f600}", Some("\u{1f600}"), "\u{1f600}"),
        ("char/multi-codepoint", "ab", Some("ab"), "ab"),
        // Ctrl+A as a real layout reports it: logical key stays `a`, text is the
        // C0 control byte. `is_control_character` must reject it for
        // REPORT_ASSOCIATED_TEXT.
        ("char/ctrl-a-shape", "a", Some("\u{1}"), "a"),
        ("char/ctrl-space-shape", " ", Some("\u{0}"), " "),
        // A key that produces no text at all.
        ("char/no-text", "a", None, "a"),
    ];
    for (label, logical, text, kwm) in chars {
        let mut s = shape(
            label,
            Key::Character((*logical).to_owned()),
            KeyLocation::Standard,
            *text,
        );
        s.key_without_modifiers = Key::Character((*kwm).to_owned());
        v.push(s);
    }

    // Non-character, non-named keys.
    v.push(shape(
        "dead/acute",
        Key::Dead(Some('\u{b4}')),
        KeyLocation::Standard,
        None,
    ));
    v.push(shape("dead/none", Key::Dead(None), KeyLocation::Standard, None));
    v.push(shape("unidentified", Key::Unidentified, KeyLocation::Standard, None));

    v
}

/// (label, state, repeat).
pub const STATES: [(&str, ElementState, bool); 3] = [
    ("press", ElementState::Pressed, false),
    ("repeat", ElementState::Pressed, true),
    ("release", ElementState::Released, false),
];

impl Shape {
    pub fn event(&self, state: ElementState, repeat: bool) -> KeyEvent {
        KeyEvent {
            logical_key: self.key.clone(),
            location: self.location,
            state,
            repeat,
            text_with_all_modifiers: self.text.clone(),
            key_without_modifiers: self.key_without_modifiers.clone(),
        }
    }
}

/// Total number of cases the sweep visits.
pub fn case_count() -> usize {
    modes().len() * mod_sets().len() * shapes().len() * STATES.len()
}
