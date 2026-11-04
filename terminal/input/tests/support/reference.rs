//! An INDEPENDENT transcription of upstream Alacritty's regime layer.
//!
//! This exists to be differentially compared against `terminal_input::regime`.
//! For that comparison to mean anything the two must be *structurally* different
//! while being *behaviourally* identical, so this file is written as a literal
//! transliteration of upstream:
//!
//!   * the key bindings are a DATA TABLE of `Binding` values, matched by a
//!     line-for-line port of `Binding::is_triggered_by` and of the `KeyLocation`
//!     `PartialEq` -- where `regime.rs` compiles them into match arms;
//!   * `key_input` follows upstream's statement order literally, including the
//!     `BindingMode` intermediate type -- where `regime.rs` inlines it.
//!
//! What it deliberately does NOT model, because upstream does not have it:
//!   * `APP_KEYPAD`. Upstream ships zero `APP_KEYPAD` bindings. The differential
//!     test classifies every divergence caused by that extension explicitly.
//!
//! Scope matches `regime.rs`: the PTY-relevant `Action::Esc` rows only, no vi
//! mode, no search, no IME, no GUI bindings. See `regime.rs` module docs.
//!
//! Source: alacritty @ 94e7c8874e526b1e67b349d9ba30ddf81669119e
//!   alacritty/src/input/keyboard.rs:22-102, 104-131, 252-277
//!   alacritty/src/config/bindings.rs:28-65, 407-414, 444-460, 636-666, 758-789
//! Licence: Apache-2.0 (see LICENSE-APACHE at the crate root); this is a
//! rewritten transcription, not a copy.

#![allow(dead_code)]

use alacritty_terminal::term::TermMode;
use terminal_input::keys::{ElementState, Key, KeyEvent, KeyLocation, ModifiersState, NamedKey};
use terminal_input::vendored::{build_sequence, should_build_sequence};

// ---------------------------------------------------------------------------
// config/bindings.rs:758-789 -- BindingMode
// ---------------------------------------------------------------------------

bitflags::bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub struct BindingMode: u8 {
        const APP_CURSOR             = 0b0000_0001;
        const APP_KEYPAD             = 0b0000_0010;
        const ALT_SCREEN             = 0b0000_0100;
        const VI                     = 0b0000_1000;
        const SEARCH                 = 0b0001_0000;
        const DISAMBIGUATE_ESC_CODES = 0b0010_0000;
        const REPORT_ALL_KEYS_AS_ESC = 0b0100_0000;
    }
}

impl BindingMode {
    pub fn new(mode: &TermMode, search: bool) -> BindingMode {
        let mut binding_mode = BindingMode::empty();
        binding_mode.set(BindingMode::APP_CURSOR, mode.contains(TermMode::APP_CURSOR));
        binding_mode.set(BindingMode::APP_KEYPAD, mode.contains(TermMode::APP_KEYPAD));
        binding_mode.set(BindingMode::ALT_SCREEN, mode.contains(TermMode::ALT_SCREEN));
        binding_mode.set(BindingMode::VI, mode.contains(TermMode::VI));
        binding_mode.set(BindingMode::SEARCH, search);
        binding_mode.set(
            BindingMode::DISAMBIGUATE_ESC_CODES,
            mode.contains(TermMode::DISAMBIGUATE_ESC_CODES),
        );
        binding_mode.set(
            BindingMode::REPORT_ALL_KEYS_AS_ESC,
            mode.contains(TermMode::REPORT_ALL_KEYS_AS_ESC),
        );
        binding_mode
    }
}

// ---------------------------------------------------------------------------
// config/bindings.rs:636-666 -- the binding-matching KeyLocation, whose PartialEq
// is deliberately non-reflexive-looking: `Any` equals everything.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Eq)]
pub enum BindLocation {
    Standard,
    Numpad,
    Any,
}

impl PartialEq for BindLocation {
    fn eq(&self, other: &Self) -> bool {
        matches!(
            (self, other),
            (_, BindLocation::Any)
                | (BindLocation::Any, _)
                | (BindLocation::Standard, BindLocation::Standard)
                | (BindLocation::Numpad, BindLocation::Numpad)
        )
    }
}

impl From<KeyLocation> for BindLocation {
    fn from(value: KeyLocation) -> Self {
        match value {
            KeyLocation::Standard => BindLocation::Standard,
            KeyLocation::Left => BindLocation::Any,
            KeyLocation::Right => BindLocation::Any,
            KeyLocation::Numpad => BindLocation::Numpad,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindingKey {
    pub key: Key<String>,
    pub location: BindLocation,
}

// ---------------------------------------------------------------------------
// config/bindings.rs:28-65 -- Binding and is_triggered_by
// ---------------------------------------------------------------------------

pub struct Binding {
    pub trigger: BindingKey,
    pub mods: ModifiersState,
    pub mode: BindingMode,
    pub notmode: BindingMode,
    /// Upstream `Action::Esc(..)` payload. This reference only carries rows whose
    /// action writes to the PTY.
    pub esc: &'static str,
    /// Upstream source line, for failure messages.
    pub line: u32,
}

impl Binding {
    pub fn is_triggered_by(&self, mode: BindingMode, mods: ModifiersState, input: &BindingKey) -> bool {
        self.trigger == *input && self.mods == mods && mode.contains(self.mode) && !mode.intersects(self.notmode)
    }
}

fn named(line: u32, key: NamedKey, mods: ModifiersState, m: BindingMode, n: BindingMode, esc: &'static str) -> Binding {
    Binding {
        trigger: BindingKey {
            key: Key::Named(key),
            location: BindLocation::Any,
        },
        mods,
        mode: m,
        notmode: n,
        esc,
        line,
    }
}

/// config/bindings.rs:444-460, the rows whose action is `Action::Esc`.
///
/// Every row's `~VI, ~SEARCH` is transcribed even though this engine can never
/// set either; dropping them would make the transcription less faithful, not
/// more.
pub fn pty_bindings() -> Vec<Binding> {
    use BindingMode as B;
    use ModifiersState as M;
    let novi = B::VI | B::SEARCH;
    let legacy = B::VI | B::SEARCH | B::REPORT_ALL_KEYS_AS_ESC | B::DISAMBIGUATE_ESC_CODES;

    let mut v = vec![
        named(444, NamedKey::Home, M::empty(), B::APP_CURSOR, novi, "\x1bOH"),
        named(445, NamedKey::End, M::empty(), B::APP_CURSOR, novi, "\x1bOF"),
        named(446, NamedKey::ArrowUp, M::empty(), B::APP_CURSOR, novi, "\x1bOA"),
        named(447, NamedKey::ArrowDown, M::empty(), B::APP_CURSOR, novi, "\x1bOB"),
        named(448, NamedKey::ArrowRight, M::empty(), B::APP_CURSOR, novi, "\x1bOC"),
        named(449, NamedKey::ArrowLeft, M::empty(), B::APP_CURSOR, novi, "\x1bOD"),
        named(451, NamedKey::F1, M::empty(), B::empty(), legacy, "\x1bOP"),
        named(452, NamedKey::F2, M::empty(), B::empty(), legacy, "\x1bOQ"),
        named(453, NamedKey::F3, M::empty(), B::empty(), legacy, "\x1bOR"),
        named(454, NamedKey::F4, M::empty(), B::empty(), legacy, "\x1bOS"),
        named(455, NamedKey::Tab, M::SHIFT, B::empty(), legacy, "\x1b[Z"),
        named(456, NamedKey::Tab, M::SHIFT | M::ALT, B::empty(), legacy, "\x1b\x1b[Z"),
        // NOTE row 457: notmode is `~VI, ~SEARCH, ~REPORT_ALL_KEYS_AS_ESC` -- there
        // is NO `~DISAMBIGUATE_ESC_CODES` here, unlike 458 and 459.
        named(
            457,
            NamedKey::Backspace,
            M::empty(),
            B::empty(),
            novi | B::REPORT_ALL_KEYS_AS_ESC,
            "\x7f",
        ),
        named(458, NamedKey::Backspace, M::ALT, B::empty(), legacy, "\x1b\x7f"),
        named(459, NamedKey::Backspace, M::SHIFT, B::empty(), legacy, "\x7f"),
    ];
    // Row 460 is the only one with an explicit location.
    v.push(Binding {
        trigger: BindingKey {
            key: Key::Named(NamedKey::Enter),
            location: BindLocation::Numpad,
        },
        mods: ModifiersState::empty(),
        mode: BindingMode::empty(),
        notmode: legacy,
        esc: "\n",
        line: 460,
    });
    v
}

// ---------------------------------------------------------------------------
// keyboard.rs:104-131 -- alt_send_esc (non-macOS arm)
// ---------------------------------------------------------------------------

fn alt_send_esc(key: &KeyEvent, text: &str, mods: ModifiersState) -> bool {
    let alt_send_esc = mods.alt_key();

    match key.logical_key {
        Key::Named(n) => {
            if n.to_text().is_some() {
                alt_send_esc
            } else {
                mods.alt_key()
            }
        }
        _ => alt_send_esc && text.chars().count() == 1,
    }
}

// ---------------------------------------------------------------------------
// keyboard.rs:174-248 -- process_key_bindings, reduced to the Action::Esc rows.
// ---------------------------------------------------------------------------

/// Returns `Some(bytes)` when at least one binding fired. Upstream runs EVERY
/// matching binding (it does not stop at the first) and then suppresses the
/// character if any of them fired, so this collects all of them.
fn process_key_bindings(key: &KeyEvent, mods: ModifiersState, mode: TermMode) -> Option<Vec<u8>> {
    let binding_mode = BindingMode::new(&mode, false);

    // keyboard.rs:188-205, non-macOS non-Windows arm: characters are lowercased.
    let logical_key = if let Key::Character(ch) = key.logical_key.as_ref() {
        Key::Character(ch.to_lowercase())
    } else {
        key.logical_key.clone()
    };

    let input = BindingKey {
        key: logical_key,
        location: key.location.into(),
    };

    let mut fired = false;
    let mut out = Vec::new();
    for binding in pty_bindings() {
        if binding.is_triggered_by(binding_mode, mods, &input) {
            fired = true;
            out.extend_from_slice(binding.esc.as_bytes());
        }
    }

    if fired {
        Some(out)
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// keyboard.rs:22-102 and 252-277 -- key_input / key_release
// ---------------------------------------------------------------------------

pub fn encode_key(key: &KeyEvent, mods: ModifiersState, mode: TermMode) -> Vec<u8> {
    if key.state == ElementState::Released {
        return key_release(key, mode, mods);
    }

    let text = key.text_with_all_modifiers().unwrap_or_default();

    if let Some(bytes) = process_key_bindings(key, mods, mode) {
        return bytes;
    }

    if mode.contains(TermMode::VI) {
        return Vec::new();
    }

    let mods = if alt_send_esc(key, text, mods) {
        mods
    } else {
        mods & !ModifiersState::ALT
    };

    let build_key_sequence = should_build_sequence(key, text, mode, mods);

    let bytes = if build_key_sequence {
        build_sequence(key.clone(), mods, mode)
    } else {
        let mut bytes = Vec::with_capacity(text.len() + 1);
        if mods.alt_key() {
            bytes.push(b'\x1b');
        }

        bytes.extend_from_slice(text.as_bytes());
        bytes
    };

    bytes
}

fn key_release(key: &KeyEvent, mode: TermMode, mods: ModifiersState) -> Vec<u8> {
    if !mode.contains(TermMode::REPORT_EVENT_TYPES) || mode.contains(TermMode::VI) {
        return Vec::new();
    }

    let text = key.text_with_all_modifiers().unwrap_or_default();
    let mods = if alt_send_esc(key, text, mods) {
        mods
    } else {
        mods & !ModifiersState::ALT
    };

    match key.logical_key.as_ref() {
        Key::Named(NamedKey::Enter) | Key::Named(NamedKey::Tab) | Key::Named(NamedKey::Backspace)
            if !mode.contains(TermMode::REPORT_ALL_KEYS_AS_ESC) =>
        {
            Vec::new()
        }
        _ => build_sequence(key.clone(), mods, mode),
    }
}

// ---------------------------------------------------------------------------
// event.rs:1369-1410 -- paste
// ---------------------------------------------------------------------------

pub fn encode_paste(text: &str, mode: TermMode, bracketed: bool) -> Vec<u8> {
    if bracketed && mode.contains(TermMode::BRACKETED_PASTE) {
        let mut out = Vec::new();
        out.extend_from_slice(b"\x1b[200~");
        let filtered = text.replace(['\x1b', '\x03'], "");
        out.extend_from_slice(filtered.as_bytes());
        out.extend_from_slice(b"\x1b[201~");
        out
    } else if bracketed {
        text.replace("\r\n", "\r").replace('\n', "\r").into_bytes()
    } else {
        text.to_owned().into_bytes()
    }
}
