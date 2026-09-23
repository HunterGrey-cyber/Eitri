//! The regime layer: everything upstream Alacritty does around `build_sequence`.
//!
//! PROVENANCE
//! ----------
//! Derived from Alacritty at commit 94e7c8874e526b1e67b349d9ba30ddf81669119e:
//!   * `alacritty/src/input/keyboard.rs` lines  22-102 (`Processor::key_input`)
//!   * `alacritty/src/input/keyboard.rs` lines 104-131 (`Processor::alt_send_esc`)
//!   * `alacritty/src/input/keyboard.rs` lines 252-277 (`Processor::key_release`)
//!   * `alacritty/src/config/bindings.rs` lines 444-460 (the `Action::Esc` rows)
//!   * `alacritty/src/event.rs`          lines 1369-1410 (`paste`)
//! Licence: Apache-2.0, see LICENSE-APACHE and NOTICE at the crate root.
//! MODIFIED (Apache-2.0 s.4(b)): rewritten, not copied. See NOTICE.
//! Reformatted by rustfmt (max_width = 120) when moved into neovibe on 2026-09-23: whitespace only.
//!
//! WHY THIS LAYER EXISTS AT ALL
//! ----------------------------
//! `build_sequence` is not the encoder. It returns an EMPTY vector for every
//! plain printable character, and it knows nothing about `APP_CURSOR`,
//! `APP_KEYPAD`, the legacy `Backspace`/`Tab`/`F1..F4` escapes, the `Alt`-as-ESC
//! prefix, or bracketed paste. Wiring a terminal to `build_sequence` alone yields
//! "typing does nothing".
//!
//! SCOPE: WHAT IS DELIBERATELY NOT MODELLED
//! ----------------------------------------
//! Upstream's `key_input` is a GUI event handler. The following upstream concerns
//! have no counterpart in a headless engine and are omitted:
//!   * IME preedit, hint selection, inline search, the search bar. All of these
//!     are UI state; none of them can be true here.
//!   * `TermMode::VI`. Vi mode is Alacritty's own scrollback navigation mode, set
//!     by the GUI, never by the PTY. This engine never sets it. Every binding in
//!     the vendored table carries `~VI, ~SEARCH`, and with both permanently false
//!     those two clauses are tautologies here.
//!   * Key bindings whose action is not `Action::Esc` (Copy, Paste, font size,
//!     `ToggleViMode`, ...). Those never reach the PTY. NOTE the consequence:
//!     upstream SUPPRESSES the character for `Ctrl+0`, `Ctrl+=`, `Ctrl+-`,
//!     `Ctrl++`, `Ctrl+Shift+{c,v,f,b}` and `Shift+Insert` because a GUI binding
//!     consumed them (config/bindings.rs:544-562). This engine has no such
//!     bindings, so those key combinations fall through to the normal encoder and
//!     DO reach the PTY. That is intentional -- a headless engine that swallowed
//!     `Ctrl+-` would be broken -- and it is modelled identically in the
//!     differential reference, so it is not a divergence.
//!
//! DEFERRED BY DECISION
//! --------------------
//! `modifyOtherKeys` (XTMODKEYS) is NOT implemented. Neither does upstream
//! Alacritty implement it. If you find a reproducible application failure that is
//! attributable to its absence, that is a finding worth escalating, not a bug to
//! quietly patch here.

use alacritty_terminal::term::TermMode;

use crate::keys::{ElementState, Key, KeyEvent, KeyLocation, ModifiersState, NamedKey};
use crate::vendored::{build_sequence, should_build_sequence};

/// One normalised input event, ready to be encoded for the PTY.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NormalizedInput {
    /// A key press or release together with the modifier state at that moment.
    Key { event: KeyEvent, mods: ModifiersState },
    /// A paste. `bracketed` is the *request*: whether the paste should be
    /// bracketed if the terminal has `BRACKETED_PASTE` enabled.
    Paste { text: String, bracketed: bool },
}

/// Encode one normalised input against the terminal's current mode.
pub fn encode(input: &NormalizedInput, mode: TermMode) -> Vec<u8> {
    match input {
        NormalizedInput::Key { event, mods } => encode_key(event, *mods, mode),
        NormalizedInput::Paste { text, bracketed } => encode_paste(text, mode, *bracketed),
    }
}

/// Encode a single key event.
///
/// Mirrors upstream `Processor::key_input` (press) and `Processor::key_release`
/// (release), minus the GUI concerns listed in the module docs.
pub fn encode_key(key: &KeyEvent, mods: ModifiersState, mode: TermMode) -> Vec<u8> {
    if key.state == ElementState::Released {
        return key_release(key, mode, mods);
    }

    let text = key.text_with_all_modifiers().unwrap_or_default();

    // Upstream runs `process_key_bindings` BEFORE the Alt mask and before
    // `build_sequence`, and returns early when a binding fires. Order matters:
    // the legacy `Backspace` -> `\x7f` row only wins because it is checked first.
    //
    // Upstream's binding matcher requires `self.mods == mods` -- EXACT equality,
    // not `contains`. `Alt+Ctrl+Backspace` therefore does NOT match the
    // `ModifiersState::ALT` row and falls through to `build_sequence`.
    if let Some(bytes) = app_keypad_escape(key, mods, mode) {
        return bytes.to_vec();
    }
    if let Some(bytes) = binding_escape(key, mods, mode) {
        return bytes.to_vec();
    }

    // keyboard.rs:71-73. Vi mode has no PTY input of its own; the search input was
    // handled before. This engine never sets `TermMode::VI`, but the flag is real
    // and the check is kept so that the behaviour is upstream's if it ever is set.
    if mode.contains(TermMode::VI) {
        return Vec::new();
    }

    // Mask `Alt` modifier from input when we won't send esc.
    let mods = if alt_send_esc(key, text, mods) {
        mods
    } else {
        mods & !ModifiersState::ALT
    };

    let build_key_sequence = should_build_sequence(key, text, mode, mods);

    // CAREFUL. Upstream is an if/else EXPRESSION, not an early return:
    //
    //     let bytes = if build_key_sequence { build_sequence(..) } else { <text arm> };
    //
    // Writing it as `if build_key_sequence { let b = ..; if !b.is_empty() { return b } }`
    // and then falling through into the text arm injects a lone `\x1b` for every
    // `Alt+<key build_sequence does not know>` -- 68 such cases -- because the text
    // arm still pushes the Alt prefix onto empty text. Keep this an if/else.
    if build_key_sequence {
        build_sequence(key.clone(), mods, mode)
    } else {
        let mut bytes = Vec::with_capacity(text.len() + 1);
        if mods.alt_key() {
            bytes.push(b'\x1b');
        }
        bytes.extend_from_slice(text.as_bytes());
        bytes
    }
}

/// Handle key release.
///
/// Transcribed from upstream `keyboard.rs:252-277`.
///
/// Omitting this function is not a missing feature, it is a correctness bug: with
/// `REPORT_EVENT_TYPES` negotiated (nvim does negotiate it), routing a release
/// through the press path makes one physical keystroke act twice. One `Backspace`
/// keystroke deletes two characters; one `Enter` keystroke submits twice.
fn key_release(key: &KeyEvent, mode: TermMode, mods: ModifiersState) -> Vec<u8> {
    // Upstream also bails on `search_active()` and `hint_state.active()`; neither
    // exists headless. `TermMode::VI` is kept because the flag is real even though
    // this engine never sets it.
    if !mode.contains(TermMode::REPORT_EVENT_TYPES) || mode.contains(TermMode::VI) {
        return Vec::new();
    }

    // Mask `Alt` modifier from input when we won't send esc.
    let text = key.text_with_all_modifiers().unwrap_or_default();
    let mods = if alt_send_esc(key, text, mods) {
        mods
    } else {
        mods & !ModifiersState::ALT
    };

    match key.logical_key.as_ref() {
        // Releasing these three emits NOTHING unless every key is being reported
        // as an escape. This is the anti-double-input rule.
        Key::Named(NamedKey::Enter) | Key::Named(NamedKey::Tab) | Key::Named(NamedKey::Backspace)
            if !mode.contains(TermMode::REPORT_ALL_KEYS_AS_ESC) =>
        {
            Vec::new()
        }
        _ => build_sequence(key.clone(), mods, mode),
    }
}

/// Transcribed from upstream `keyboard.rs:104-131`, non-macOS arm only.
///
/// On non-macOS both branches of the `Key::Named` arm reduce to `mods.alt_key()`;
/// they are kept apart here because upstream keeps them apart, and because the
/// macOS `option_as_alt` config is the only thing that makes them differ.
fn alt_send_esc(key: &KeyEvent, text: &str, mods: ModifiersState) -> bool {
    let alt_send_esc = mods.alt_key();

    match key.logical_key {
        Key::Named(named) => {
            if named.to_text().is_some() {
                alt_send_esc
            } else {
                // Treat `Alt` as modifier for named keys without text, like ArrowUp.
                mods.alt_key()
            }
        }
        _ => alt_send_esc && text.chars().count() == 1,
    }
}

/// The PTY-relevant subset of Alacritty's declarative key binding table,
/// `config/bindings.rs` lines 444-460, compiled into match arms.
///
/// Binding semantics reproduced from `Binding::is_triggered_by`
/// (config/bindings.rs:54-65):
///   * `self.mods == mods`               -- EXACT modifier equality.
///   * `mode.contains(self.mode)`        -- all required modes present.
///   * `!mode.intersects(self.notmode)`  -- no forbidden mode present.
/// And from the `trigger!` macro (config/bindings.rs:407-414): a row written
/// without `=> KeyLocation::..` gets `KeyLocation::Any`, whose `PartialEq`
/// (config/bindings.rs:657-666) matches EVERY location. Only the numpad `Enter`
/// row at line 460 constrains location.
///
/// NOTE, and this is not an oversight: the six `APP_CURSOR` rows carry ONLY
/// `~VI, ~SEARCH`. Unlike the `F1..F4`, `Tab` and `Backspace` rows immediately
/// below them, they are NOT gated on `~REPORT_ALL_KEYS_AS_ESC` or
/// `~DISAMBIGUATE_ESC_CODES`. Upstream therefore emits `\x1bOA` for an unmodified
/// `ArrowUp` even with the kitty protocol negotiated. That is faithfully
/// reproduced here. Adding a kitty pre-check to these six rows would be a
/// divergence from upstream, not a fix.
fn binding_escape(key: &KeyEvent, mods: ModifiersState, mode: TermMode) -> Option<&'static [u8]> {
    // Every row in the vendored table carries `~VI, ~SEARCH`. SEARCH is a GUI
    // concept that is permanently false here; VI is a real `TermMode` flag.
    if mode.contains(TermMode::VI) {
        return None;
    }

    let named = match key.logical_key {
        Key::Named(named) => named,
        _ => return None,
    };

    let app_cursor = mode.contains(TermMode::APP_CURSOR);
    let report_all = mode.contains(TermMode::REPORT_ALL_KEYS_AS_ESC);
    let disambiguate = mode.contains(TermMode::DISAMBIGUATE_ESC_CODES);
    let none = mods.is_empty();
    let numpad = key.location == KeyLocation::Numpad;

    // `legacy` == the `~REPORT_ALL_KEYS_AS_ESC, ~DISAMBIGUATE_ESC_CODES` pair that
    // rows 451-456 and 458-460 share.
    let legacy = !report_all && !disambiguate;

    let bytes: &'static [u8] = match named {
        // bindings.rs:444-449 -- `+APP_CURSOR`, no kitty gate. See the note above.
        NamedKey::Home if app_cursor && none => b"\x1bOH",
        NamedKey::End if app_cursor && none => b"\x1bOF",
        NamedKey::ArrowUp if app_cursor && none => b"\x1bOA",
        NamedKey::ArrowDown if app_cursor && none => b"\x1bOB",
        NamedKey::ArrowRight if app_cursor && none => b"\x1bOC",
        NamedKey::ArrowLeft if app_cursor && none => b"\x1bOD",

        // bindings.rs:451-454 -- legacy SS3 function keys.
        NamedKey::F1 if none && legacy => b"\x1bOP",
        NamedKey::F2 if none && legacy => b"\x1bOQ",
        NamedKey::F3 if none && legacy => b"\x1bOR",
        NamedKey::F4 if none && legacy => b"\x1bOS",

        // bindings.rs:455-456 -- back-tab.
        NamedKey::Tab if mods == ModifiersState::SHIFT && legacy => b"\x1b[Z",
        NamedKey::Tab if mods == ModifiersState::SHIFT | ModifiersState::ALT && legacy => b"\x1b\x1b[Z",

        // bindings.rs:457 -- the PLAIN backspace row is gated on
        // `~REPORT_ALL_KEYS_AS_ESC` ALONE. It still fires under
        // `DISAMBIGUATE_ESC_CODES`, which is why plain Backspace stays `\x7f` in
        // kitty level 1.
        NamedKey::Backspace if none && !report_all => b"\x7f",

        // bindings.rs:458-459 -- the ALT and SHIFT rows additionally require
        // `~DISAMBIGUATE_ESC_CODES`, so under kitty level 1 they fall through to
        // `build_sequence` and become `\x1b[127;3u` / `\x1b[127;2u`.
        //
        // The modifier test is `==`, not `contains`. `Ctrl+Alt+Backspace` does not
        // match row 458; it falls through and becomes `\x1b\x08`.
        NamedKey::Backspace if mods == ModifiersState::ALT && legacy => b"\x1b\x7f",
        NamedKey::Backspace if mods == ModifiersState::SHIFT && legacy => b"\x7f",

        // bindings.rs:460 -- numpad Enter. The only row with a location constraint.
        NamedKey::Enter if numpad && none && legacy => b"\n",

        _ => return None,
    };

    Some(bytes)
}

/// DECPAM / application keypad (`TermMode::APP_KEYPAD`, DECSET `\x1b=`).
///
/// EXTRAPOLATION WARNING -- READ BEFORE TRUSTING THIS TABLE.
///
/// Upstream Alacritty ships NOTHING for `APP_KEYPAD`. `BindingMode::APP_KEYPAD`
/// exists (config/bindings.rs:763) but not one default binding references it, and
/// `build_sequence`'s doc comment ("The key sequences for `APP_KEYPAD` and alike
/// are handled inside the bindings") points at bindings that do not exist. So
/// there is no upstream behaviour to be faithful to here and the differential
/// reference does not model this function.
///
/// What IS pinned by terminfo, measured with `infocmp -x` on this machine:
///
///   TERM=xterm     ka1=\EOw (kp7)  ka3=\EOy (kp9)  kb2=\EOu (kp5)
///                  kc1=\EOq (kp1)  kc3=\EOs (kp3)  kent=\EOM
///   TERM=alacritty kb2=\EOE        kent=\EOM       -- and NOTHING else;
///                  alacritty's own terminfo declares no ka1/ka3/kc1/kc3.
///
/// So exactly SIX of the seventeen rows below are corroborated by xterm's
/// terminfo (kp1, kp3, kp5, kp7, kp9, Enter), and alacritty's own terminfo
/// corroborates only two of those and CONTRADICTS one: it maps kb2 to `\EOE`
/// where xterm maps it to `\EOu`. The table below follows the DEC VT100/VT220
/// DECKPAM SS3 layout (`\EOp`..`\EOy` for digits 0..9), which is what xterm
/// implements and what applications actually expect.
///
/// Every row NOT in that corroborated set -- kp0, kp2, kp4, kp6, kp8, `.`, `/`,
/// `*`, `-`, `+`, `=` -- is EXTRAPOLATION from the DEC layout. It is not
/// validated by any terminfo entry on this machine and it is not validated by
/// upstream. Treat it as a considered guess.
///
/// Gating: suppressed entirely when the kitty protocol is active, using the SAME
/// three-flag test `build_sequence` uses for `kitty_seq`
/// (`REPORT_ALL_KEYS_AS_ESC | DISAMBIGUATE_ESC_CODES | REPORT_EVENT_TYPES`), NOT
/// `TermMode::KITTY_KEYBOARD_PROTOCOL`, which is a five-flag superset that also
/// covers `REPORT_ALTERNATE_KEYS` and `REPORT_ASSOCIATED_TEXT`. Using the superset
/// would silently break an application that negotiates `REPORT_ALTERNATE_KEYS`
/// alone, because `build_sequence`'s own numpad branch would then stay switched
/// off while this one also stayed switched off.
fn app_keypad_escape(key: &KeyEvent, mods: ModifiersState, mode: TermMode) -> Option<&'static [u8]> {
    if !mode.contains(TermMode::APP_KEYPAD) {
        return None;
    }
    // Matches the `~VI` every row of the vendored binding table carries.
    if mode.contains(TermMode::VI) {
        return None;
    }
    if key.location != KeyLocation::Numpad {
        return None;
    }
    if !mods.is_empty() {
        return None;
    }

    // Same expression as `build_sequence`'s `kitty_seq`.
    let kitty_seq = mode
        .intersects(TermMode::REPORT_ALL_KEYS_AS_ESC | TermMode::DISAMBIGUATE_ESC_CODES | TermMode::REPORT_EVENT_TYPES);
    if kitty_seq {
        return None;
    }

    let bytes: &'static [u8] = match key.logical_key.as_ref() {
        // Corroborated by xterm terminfo kc1/kc3/kb2/ka1/ka3.
        Key::Character("1") => b"\x1bOq",
        Key::Character("3") => b"\x1bOs",
        Key::Character("5") => b"\x1bOu",
        Key::Character("7") => b"\x1bOw",
        Key::Character("9") => b"\x1bOy",
        Key::Named(NamedKey::Enter) => b"\x1bOM",
        // EXTRAPOLATED from the DEC VT100 DECKPAM layout. Not corroborated.
        Key::Character("0") => b"\x1bOp",
        Key::Character("2") => b"\x1bOr",
        Key::Character("4") => b"\x1bOt",
        Key::Character("6") => b"\x1bOv",
        Key::Character("8") => b"\x1bOx",
        Key::Character(".") => b"\x1bOn",
        Key::Character("/") => b"\x1bOo",
        Key::Character("*") => b"\x1bOj",
        Key::Character("-") => b"\x1bOm",
        Key::Character("+") => b"\x1bOk",
        Key::Character("=") => b"\x1bOX",
        _ => return None,
    };

    Some(bytes)
}

/// Encode a paste.
///
/// Transcribed from upstream `event.rs:1369-1410`.
///
/// The `\x1b` / `\x03` stripping is a SECURITY control, not cosmetic. Without it,
/// pasted text containing `\x1b[201~` closes the bracket itself and everything
/// after it is delivered to the shell as if it had been typed -- the classic
/// "paste a command that runs itself" attack. `\x03` is stripped because some
/// shells wrongly treat it as a paste terminator.
pub fn encode_paste(text: &str, mode: TermMode, bracketed: bool) -> Vec<u8> {
    if bracketed && mode.contains(TermMode::BRACKETED_PASTE) {
        let mut out = Vec::with_capacity(text.len() + 12);
        out.extend_from_slice(b"\x1b[200~");
        // Filtered escape sequences. See the security note above.
        let filtered = text.replace(['\x1b', '\x03'], "");
        out.extend_from_slice(filtered.as_bytes());
        out.extend_from_slice(b"\x1b[201~");
        out
    } else if bracketed {
        // Bracketing was requested but the application has not enabled it, so it
        // cannot tell a paste from typing. Newlines collapse to a single `\r`,
        // which is what the Enter key produces. `\r\n` must be handled before the
        // bare `\n` or CRLF text turns into two carriage returns.
        text.replace("\r\n", "\r").replace('\n', "\r").into_bytes()
    } else {
        // Bracketing explicitly disabled by the caller: pass input through as-is.
        text.as_bytes().to_vec()
    }
}
