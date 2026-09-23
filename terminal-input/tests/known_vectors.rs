//! HAND-WRITTEN EXPECTED BYTES.
//!
//! Every expectation in this file was written by reading upstream Alacritty at
//! commit 94e7c8874e526b1e67b349d9ba30ddf81669119e and working the algorithm
//! through by hand, or copied from a measurement against a real application. None
//! of it was generated from this crate. That is the point: `tests/golden.rs`
//! pins whatever the code does, this file pins what the code SHOULD do.
//!
//! Organised by the five confirmed defects of the earlier prototype plus the
//! regime gaps, so that a regression names the defect it resurrects.

use alacritty_terminal::term::TermMode;
use terminal_input::keys::{Key, KeyEvent, KeyLocation, ModifiersState as M, NamedKey as N};
use terminal_input::{encode_key, encode_paste};

fn base() -> TermMode {
    TermMode::default()
}

/// Kitty level 1: what most TUIs negotiate first.
fn kitty1() -> TermMode {
    base() | TermMode::DISAMBIGUATE_ESC_CODES
}

fn press(k: N) -> KeyEvent {
    KeyEvent::press(Key::Named(k))
}

fn release(k: N) -> KeyEvent {
    KeyEvent::release(Key::Named(k))
}

fn ch(c: &str) -> KeyEvent {
    KeyEvent::press(Key::Character(c.to_owned()))
}

#[track_caller]
fn check(label: &str, key: &KeyEvent, mods: M, mode: TermMode, want: &[u8]) {
    let got = encode_key(key, mods, mode);
    assert_eq!(
        got,
        want,
        "{label}: got {:?}, want {:?}",
        String::from_utf8_lossy(&got),
        String::from_utf8_lossy(want)
    );
}

// ===========================================================================
// D1 -- key_release. keyboard.rs:252-277.
//
// Measured consequence of omitting it, against real nvim: typing ABCDEF then ONE
// Backspace keystroke delivered as press+release left "ABCD" -- a double delete.
// One Enter keystroke inserted two lines.
// ===========================================================================

#[test]
fn d1_release_emits_nothing_without_report_event_types() {
    for key in [N::Backspace, N::Enter, N::Tab, N::ArrowUp, N::F1, N::Escape, N::Space] {
        check(
            &format!("release {key:?} in base mode"),
            &release(key),
            M::empty(),
            base(),
            b"",
        );
    }
    check(
        "release 'a' in base mode",
        &KeyEvent::release(Key::character("a")),
        M::empty(),
        base(),
        b"",
    );
    // Kitty level 1 alone does not turn on event reporting either.
    for key in [N::Backspace, N::Enter, N::Tab] {
        check(
            &format!("release {key:?} under kitty1"),
            &release(key),
            M::empty(),
            kitty1(),
            b"",
        );
    }
}

#[test]
fn d1_enter_tab_backspace_release_is_silent_even_with_event_types() {
    // THE anti-double-input rule. nvim negotiates REPORT_EVENT_TYPES; without
    // this arm one Backspace keystroke deletes twice.
    let mode = base() | TermMode::REPORT_EVENT_TYPES;
    check("Backspace release", &release(N::Backspace), M::empty(), mode, b"");
    check("Enter release", &release(N::Enter), M::empty(), mode, b"");
    check("Tab release", &release(N::Tab), M::empty(), mode, b"");
    // ... and with kitty1 also negotiated, which is the realistic nvim state.
    let mode = kitty1() | TermMode::REPORT_EVENT_TYPES;
    check(
        "Backspace release kitty1",
        &release(N::Backspace),
        M::empty(),
        mode,
        b"",
    );
    check("Enter release kitty1", &release(N::Enter), M::empty(), mode, b"");
    check("Tab release kitty1", &release(N::Tab), M::empty(), mode, b"");
}

#[test]
fn d1_other_releases_do_report_with_event_types() {
    // The exemption is exactly three keys. Everything else must still report, or
    // key-up handling in full-screen apps breaks.
    let mode = base() | TermMode::REPORT_EVENT_TYPES;
    check(
        "ArrowUp release",
        &release(N::ArrowUp),
        M::empty(),
        mode,
        b"\x1b[1;1:3A",
    );
    check("F5 release", &release(N::F5), M::empty(), mode, b"\x1b[15;1:3~");
    check("Escape release", &release(N::Escape), M::empty(), mode, b"\x1b[27;1:3u");
    check("Space release", &release(N::Space), M::empty(), mode, b"\x1b[32;1:3u");
    check(
        "'a' release",
        &KeyEvent::release(Key::character("a")),
        M::empty(),
        mode,
        b"\x1b[97;1:3u",
    );
}

#[test]
fn d1_the_three_keys_do_report_under_report_all_keys_as_esc() {
    // The exemption is conditioned on `!REPORT_ALL_KEYS_AS_ESC`. With full
    // reporting on, the application asked for every event and gets them.
    let mode = base() | TermMode::REPORT_EVENT_TYPES | TermMode::REPORT_ALL_KEYS_AS_ESC;
    check(
        "Backspace release",
        &release(N::Backspace),
        M::empty(),
        mode,
        b"\x1b[127;1:3u",
    );
    check("Enter release", &release(N::Enter), M::empty(), mode, b"\x1b[13;1:3u");
    check("Tab release", &release(N::Tab), M::empty(), mode, b"\x1b[9;1:3u");
}

#[test]
fn d1_repeat_is_event_type_2_not_1() {
    // REPORT_EVENT_TYPES on its own does NOT route a printable key press into the
    // kitty encoder, because `should_build_sequence` (keyboard.rs:141-171)
    // consults only REPORT_ALL_KEYS_AS_ESC and DISAMBIGUATE_ESC_CODES -- verified
    // by reading those 31 lines; REPORT_EVENT_TYPES appears nowhere in them. So a
    // press and a repeat both come out as plain text...
    let mode = base() | TermMode::REPORT_EVENT_TYPES;
    let repeat = KeyEvent::press(Key::character("a")).with_repeat(true);
    check("'a' press", &ch("a"), M::empty(), mode, b"a");
    check("'a' repeat", &repeat, M::empty(), mode, b"a");

    // ... while the RELEASE is encoded, because `key_release` calls
    // `build_sequence` directly and never asks `should_build_sequence`. This
    // asymmetry is upstream's, not ours.
    check(
        "'a' release",
        &KeyEvent::release(Key::character("a")),
        M::empty(),
        mode,
        b"\x1b[97;1:3u",
    );

    // Under kitty level 1, an UNMODIFIED printable key is still plain text: the
    // `disambiguate` term (keyboard.rs:146-157) additionally requires a non-empty
    // modifier set, and even a lone SHIFT does not count unless the key is Tab,
    // Enter or Backspace. That is the point of "disambiguate" -- ordinary typing
    // is left alone.
    let mode = kitty1() | TermMode::REPORT_EVENT_TYPES;
    let repeat = KeyEvent::press(Key::character("a")).with_repeat(true);
    check("'a' press kitty1", &ch("a"), M::empty(), mode, b"a");
    check("'a' repeat kitty1", &repeat, M::empty(), mode, b"a");
    check("Shift+a repeat kitty1", &repeat, M::SHIFT, mode, b"a");

    // A modifier that is not SHIFT does disambiguate, and THERE the event type
    // shows up: 1 = press, 2 = repeat, 3 = release.
    let ctrl_a = KeyEvent::press(Key::character("a")).with_text(Some("\u{1}"));
    let ctrl_a_repeat = ctrl_a.clone().with_repeat(true);
    let ctrl_a_up = KeyEvent::release(Key::character("a")).with_text(Some("\u{1}"));
    check("Ctrl+a press kitty1", &ctrl_a, M::CONTROL, mode, b"\x1b[97;5u");
    check(
        "Ctrl+a repeat kitty1",
        &ctrl_a_repeat,
        M::CONTROL,
        mode,
        b"\x1b[97;5:2u",
    );
    check("Ctrl+a release kitty1", &ctrl_a_up, M::CONTROL, mode, b"\x1b[97;5:3u");
}

// ===========================================================================
// D2 -- the fall-through that injected a stray ESC.
//
// key_input's `let bytes = if build_key_sequence { .. } else { <text arm> }` is an
// EXPRESSION. Turning it into "try build_sequence, and if it came back empty fall
// into the text arm" emits a lone `\x1b` for every Alt+<key build_sequence does
// not know>. Demonstrated against real nvim: Alt+ContextMenu left INSERT mode.
// ===========================================================================

/// Every key upstream's `build_sequence` returns nothing for outside kitty mode.
const D2_UNKNOWN_KEYS: [N; 22] = [
    N::CapsLock,
    N::NumLock,
    N::ScrollLock,
    N::PrintScreen,
    N::Pause,
    N::ContextMenu,
    N::Hyper,
    N::Meta,
    N::MediaPlay,
    N::MediaPause,
    N::MediaPlayPause,
    N::MediaStop,
    N::MediaFastForward,
    N::MediaRewind,
    N::MediaTrackNext,
    N::MediaTrackPrevious,
    N::MediaRecord,
    N::AudioVolumeDown,
    N::AudioVolumeUp,
    N::AudioVolumeMute,
    N::F25,
    N::F35,
];

#[test]
fn d2_alt_plus_unknown_key_emits_nothing_not_a_stray_esc() {
    let mut asserted = 0;
    for key in D2_UNKNOWN_KEYS {
        for mods in [M::ALT, M::ALT | M::CONTROL] {
            check(&format!("{key:?} with {mods:?}"), &press(key), mods, base(), b"");
            asserted += 1;
        }
    }
    assert_eq!(asserted, 44, "expected 22 keys x 2 modifier sets");
}

#[test]
fn d2_the_same_keys_emit_nothing_unmodified_too() {
    for key in D2_UNKNOWN_KEYS {
        check(&format!("{key:?} bare"), &press(key), M::empty(), base(), b"");
    }
}

#[test]
fn d2_but_alt_still_prefixes_esc_for_keys_that_do_have_text() {
    // The fix must not over-correct: Alt+<printable> genuinely is ESC-prefixed.
    check("Alt+a", &ch("a"), M::ALT, base(), b"\x1ba");
    check("Alt+Enter", &press(N::Enter), M::ALT, base(), b"\x1b\r");
    check("Alt+Escape", &press(N::Escape), M::ALT, base(), b"\x1b\x1b");
    check("Alt+Space", &press(N::Space), M::ALT, base(), b"\x1b ");
}

// ===========================================================================
// D3 / D4 -- the Backspace legacy binding gates. config/bindings.rs:457-459.
//
//   :457  Backspace,        ~VI, ~SEARCH, ~REPORT_ALL_KEYS_AS_ESC                         -> "\x7f"
//   :458  Backspace, ALT,   ~VI, ~SEARCH, ~REPORT_ALL_KEYS_AS_ESC, ~DISAMBIGUATE_ESC_CODES -> "\x1b\x7f"
//   :459  Backspace, SHIFT, ~VI, ~SEARCH, ~REPORT_ALL_KEYS_AS_ESC, ~DISAMBIGUATE_ESC_CODES -> "\x7f"
//
// D3: the plain row is gated on ~REPORT_ALL_KEYS_AS_ESC ALONE; the ALT and SHIFT
//     rows also require ~DISAMBIGUATE_ESC_CODES.
// D4: the modifier test is `==`, not `contains`.
// ===========================================================================

#[test]
fn d3_plain_backspace_still_fires_under_disambiguate() {
    check("legacy", &press(N::Backspace), M::empty(), base(), b"\x7f");
    // The plain row has no ~DISAMBIGUATE gate, so kitty level 1 keeps `\x7f`.
    check("kitty1", &press(N::Backspace), M::empty(), kitty1(), b"\x7f");
    // But REPORT_ALL_KEYS_AS_ESC does gate it.
    check(
        "report-all",
        &press(N::Backspace),
        M::empty(),
        base() | TermMode::REPORT_ALL_KEYS_AS_ESC,
        b"\x1b[127u",
    );
}

#[test]
fn d3_alt_and_shift_backspace_are_additionally_gated_on_disambiguate() {
    check("legacy alt", &press(N::Backspace), M::ALT, base(), b"\x1b\x7f");
    check("legacy shift", &press(N::Backspace), M::SHIFT, base(), b"\x7f");
    // MEASURED divergence of the earlier prototype: it gave "\x7f" here.
    check("kitty1 shift", &press(N::Backspace), M::SHIFT, kitty1(), b"\x1b[127;2u");
    check("kitty1 alt", &press(N::Backspace), M::ALT, kitty1(), b"\x1b[127;3u");
}

#[test]
fn d4_backspace_modifier_match_is_exact_not_contains() {
    // MEASURED divergence of the earlier prototype: it gave "\x1b\x7f" here,
    // because it tested `mods.contains(ALT)`. Row 458 requires `mods == ALT`.
    check(
        "legacy ctrl+alt",
        &press(N::Backspace),
        M::CONTROL | M::ALT,
        base(),
        b"\x1b\x08",
    );
    check("legacy ctrl", &press(N::Backspace), M::CONTROL, base(), b"\x08");
    check(
        "legacy shift+alt",
        &press(N::Backspace),
        M::SHIFT | M::ALT,
        base(),
        b"\x1b\x08",
    );
    check("legacy super", &press(N::Backspace), M::SUPER, base(), b"\x08");
}

#[test]
fn d4_shift_tab_modifier_match_is_exact_too() {
    check("Shift+Tab", &press(N::Tab), M::SHIFT, base(), b"\x1b[Z");
    check(
        "Shift+Alt+Tab",
        &press(N::Tab),
        M::SHIFT | M::ALT,
        base(),
        b"\x1b\x1b[Z",
    );
    // Ctrl+Shift+Tab matches neither row 455 nor 456.
    check("Ctrl+Shift+Tab", &press(N::Tab), M::SHIFT | M::CONTROL, base(), b"\t");
}

// ===========================================================================
// REGIME GAP -- APP_CURSOR (DECCKM). config/bindings.rs:444-449.
// Omitting this silently breaks arrow keys in every full-screen application.
// ===========================================================================

#[test]
fn app_cursor_switches_arrows_to_ss3() {
    let app = base() | TermMode::APP_CURSOR;
    for (key, normal, ss3) in [
        (N::ArrowUp, &b"\x1b[A"[..], &b"\x1bOA"[..]),
        (N::ArrowDown, b"\x1b[B", b"\x1bOB"),
        (N::ArrowRight, b"\x1b[C", b"\x1bOC"),
        (N::ArrowLeft, b"\x1b[D", b"\x1bOD"),
        (N::Home, b"\x1b[H", b"\x1bOH"),
        (N::End, b"\x1b[F", b"\x1bOF"),
    ] {
        check(&format!("{key:?} normal"), &press(key), M::empty(), base(), normal);
        check(&format!("{key:?} app_cursor"), &press(key), M::empty(), app, ss3);
    }
}

#[test]
fn app_cursor_rows_require_exactly_no_modifiers() {
    let app = base() | TermMode::APP_CURSOR;
    check("Shift+ArrowUp", &press(N::ArrowUp), M::SHIFT, app, b"\x1b[1;2A");
    check("Ctrl+ArrowUp", &press(N::ArrowUp), M::CONTROL, app, b"\x1b[1;5A");
    check("Alt+ArrowUp", &press(N::ArrowUp), M::ALT, app, b"\x1b[1;3A");
}

#[test]
fn app_cursor_rows_are_not_gated_on_the_kitty_flags() {
    // DELIBERATE and verified against config/bindings.rs:444-449: unlike rows
    // 451-459, the APP_CURSOR rows carry no `~REPORT_ALL_KEYS_AS_ESC` and no
    // `~DISAMBIGUATE_ESC_CODES`. Upstream emits SS3 even with kitty negotiated.
    // Adding a kitty pre-check here would diverge from upstream, not fix it.
    let app = base() | TermMode::APP_CURSOR;
    check(
        "kitty1",
        &press(N::ArrowUp),
        M::empty(),
        app | TermMode::DISAMBIGUATE_ESC_CODES,
        b"\x1bOA",
    );
    check(
        "report-all",
        &press(N::ArrowUp),
        M::empty(),
        app | TermMode::REPORT_ALL_KEYS_AS_ESC,
        b"\x1bOA",
    );
}

// ===========================================================================
// REGIME GAP -- APP_KEYPAD (DECPAM). Upstream ships NOTHING here; see the
// EXTRAPOLATION WARNING on `regime::app_keypad_escape`.
// ===========================================================================

#[test]
fn app_keypad_terminfo_corroborated_rows() {
    // The only six rows any terminfo entry on this machine pins:
    //   xterm  ka1=\EOw (kp7)  ka3=\EOy (kp9)  kb2=\EOu (kp5)
    //          kc1=\EOq (kp1)  kc3=\EOs (kp3)  kent=\EOM
    let app = base() | TermMode::APP_KEYPAD;
    let np = |c: &str| KeyEvent::press(Key::Character(c.to_owned())).with_location(KeyLocation::Numpad);
    check("kp1", &np("1"), M::empty(), app, b"\x1bOq");
    check("kp3", &np("3"), M::empty(), app, b"\x1bOs");
    check("kp5", &np("5"), M::empty(), app, b"\x1bOu");
    check("kp7", &np("7"), M::empty(), app, b"\x1bOw");
    check("kp9", &np("9"), M::empty(), app, b"\x1bOy");
    check(
        "kpEnter",
        &press(N::Enter).with_location(KeyLocation::Numpad),
        M::empty(),
        app,
        b"\x1bOM",
    );
}

#[test]
fn app_keypad_is_off_by_default_and_suppressed_under_kitty() {
    let np7 = KeyEvent::press(Key::character("7")).with_location(KeyLocation::Numpad);
    check("no APP_KEYPAD", &np7, M::empty(), base(), b"7");
    // The gate uses build_sequence's own three-flag `kitty_seq`, so kitty's
    // numpad encoding wins.
    let app = base() | TermMode::APP_KEYPAD;
    check(
        "kitty1",
        &np7,
        M::empty(),
        app | TermMode::DISAMBIGUATE_ESC_CODES,
        b"\x1b[57406u",
    );
    // KNOWN ROUGH EDGE, recorded rather than hidden. `REPORT_EVENT_TYPES` is part
    // of `kitty_seq`, so it switches the DECPAM table off -- but
    // `should_build_sequence` does NOT consult `REPORT_EVENT_TYPES`, so the press
    // never reaches `build_sequence`'s numpad branch either. An application that
    // negotiates REPORT_EVENT_TYPES *alone* and then sets DECPAM therefore gets a
    // bare "7" on press: it loses the SS3 form without gaining the kitty form.
    // Keeping the gate identical to `build_sequence`'s own `kitty_seq` is the
    // locked decision; this test exists so that the cost of it is visible and so
    // that any future change to the gate shows up here.
    check(
        "event types only -- DECPAM lost, kitty not gained",
        &np7,
        M::empty(),
        app | TermMode::REPORT_EVENT_TYPES,
        b"7",
    );
    // The release of the same key IS kitty-encoded, for the reason above.
    let np7_up = KeyEvent::release(Key::character("7")).with_location(KeyLocation::Numpad);
    check(
        "event types only, release",
        &np7_up,
        M::empty(),
        app | TermMode::REPORT_EVENT_TYPES,
        b"\x1b[57406;1:3u",
    );
    // ... but REPORT_ALTERNATE_KEYS alone is NOT part of `kitty_seq`, so
    // APP_KEYPAD must still apply. Using TermMode::KITTY_KEYBOARD_PROTOCOL (a
    // five-flag superset) for the gate would break exactly this case.
    check(
        "alternate keys only",
        &np7,
        M::empty(),
        app | TermMode::REPORT_ALTERNATE_KEYS,
        b"\x1bOw",
    );
}

#[test]
fn numpad_enter_is_newline_without_app_keypad() {
    // config/bindings.rs:460, the only row with a location constraint.
    check(
        "numpad Enter",
        &press(N::Enter).with_location(KeyLocation::Numpad),
        M::empty(),
        base(),
        b"\n",
    );
    check("standard Enter", &press(N::Enter), M::empty(), base(), b"\r");
}

// ===========================================================================
// REGIME GAP -- plain printable text and the Alt-ESC prefix.
// `build_sequence` returns "" for every plain character; reading only it yields
// "typing does nothing".
// ===========================================================================

#[test]
fn plain_typing_reaches_the_pty() {
    check("a", &ch("a"), M::empty(), base(), b"a");
    check("A", &ch("A"), M::SHIFT, base(), b"A");
    check("1", &ch("1"), M::empty(), base(), b"1");
    check("!", &ch("!"), M::SHIFT, base(), b"!");
    check("e-acute", &ch("\u{e9}"), M::empty(), base(), "\u{e9}".as_bytes());
    check("emoji", &ch("\u{1f600}"), M::empty(), base(), "\u{1f600}".as_bytes());
    // Control characters arrive as the layout's text, not as a rebuilt sequence.
    let ctrl_a = KeyEvent::press(Key::character("a")).with_text(Some("\u{1}"));
    check("Ctrl+a", &ctrl_a, M::CONTROL, base(), b"\x01");
}

#[test]
fn legacy_named_keys() {
    check("Enter", &press(N::Enter), M::empty(), base(), b"\r");
    check("Tab", &press(N::Tab), M::empty(), base(), b"\t");
    check("Escape", &press(N::Escape), M::empty(), base(), b"\x1b");
    check("Space", &press(N::Space), M::empty(), base(), b" ");
    check("Delete", &press(N::Delete), M::empty(), base(), b"\x1b[3~");
    check("Insert", &press(N::Insert), M::empty(), base(), b"\x1b[2~");
    check("PageUp", &press(N::PageUp), M::empty(), base(), b"\x1b[5~");
    check("PageDown", &press(N::PageDown), M::empty(), base(), b"\x1b[6~");
    check("F1", &press(N::F1), M::empty(), base(), b"\x1bOP");
    check("F2", &press(N::F2), M::empty(), base(), b"\x1bOQ");
    check("F3", &press(N::F3), M::empty(), base(), b"\x1bOR");
    check("F4", &press(N::F4), M::empty(), base(), b"\x1bOS");
    check("F5", &press(N::F5), M::empty(), base(), b"\x1b[15~");
    check("F12", &press(N::F12), M::empty(), base(), b"\x1b[24~");
}

#[test]
fn f1_to_f4_lose_their_ss3_form_under_kitty() {
    // Rows 451-454 carry both `~REPORT_ALL_KEYS_AS_ESC` and
    // `~DISAMBIGUATE_ESC_CODES`, so under kitty1 they fall through.
    check("F1 kitty1", &press(N::F1), M::empty(), kitty1(), b"\x1b[P");
    // F3 in the kitty protocol diverges from alacritty's terminfo -- upstream
    // encodes it as CSI 13 ~, not CSI R.
    check("F3 kitty1", &press(N::F3), M::empty(), kitty1(), b"\x1b[13~");
}

#[test]
fn vi_mode_swallows_everything() {
    let vi = base() | TermMode::VI;
    check("a", &ch("a"), M::empty(), vi, b"");
    check("Backspace", &press(N::Backspace), M::empty(), vi, b"");
    check(
        "ArrowUp",
        &press(N::ArrowUp),
        M::empty(),
        vi | TermMode::APP_CURSOR,
        b"",
    );
}

// ===========================================================================
// REGIME GAP -- bracketed paste. The \x1b / \x03 stripping is a SECURITY control.
// ===========================================================================

#[test]
fn bracketed_paste_wraps_and_filters() {
    let mode = base() | TermMode::BRACKETED_PASTE;
    assert_eq!(encode_paste("hello", mode, true), b"\x1b[200~hello\x1b[201~");
    assert_eq!(encode_paste("a\nb", mode, true), b"\x1b[200~a\nb\x1b[201~");
}

#[test]
fn bracketed_paste_strips_the_escape_that_would_close_the_bracket() {
    let mode = base() | TermMode::BRACKETED_PASTE;
    // Without the filter this delivers `rm -rf ~` to the shell as typed input.
    let attack = "safe\x1b[201~rm -rf ~\ncontinued";
    let out = encode_paste(attack, mode, true);
    let s = String::from_utf8(out).unwrap();
    assert!(
        !s[6..s.len() - 6].contains('\x1b'),
        "an ESC survived into the payload: {s:?}"
    );
    assert_eq!(s, "\x1b[200~safe[201~rm -rf ~\ncontinued\x1b[201~");
    assert_eq!(s.matches("\x1b[201~").count(), 1, "payload closed the bracket early");
}

#[test]
fn bracketed_paste_strips_etx() {
    let mode = base() | TermMode::BRACKETED_PASTE;
    assert_eq!(encode_paste("a\x03b", mode, true), b"\x1b[200~ab\x1b[201~");
}

#[test]
fn unbracketed_paste_collapses_newlines_to_one_cr_each() {
    // A multi-line paste into an application that has not enabled bracketed paste
    // must not act as several Enters beyond the newlines the text genuinely has --
    // and CRLF must collapse to ONE \r, not two.
    let mode = base();
    assert_eq!(encode_paste("a\nb\nc", mode, true), b"a\rb\rc");
    assert_eq!(encode_paste("a\r\nb", mode, true), b"a\rb");
    assert_eq!(
        encode_paste("a\r\nb", mode, true)
            .iter()
            .filter(|c| **c == b'\r')
            .count(),
        1
    );
    // Explicitly-unbracketed paste passes through untouched.
    assert_eq!(encode_paste("a\r\nb", mode, false), b"a\r\nb");
}

// ===========================================================================
// The public surface: the NormalizedInput dispatcher and is_modifier_key.
// Both were found UNTESTED by the mutation sweep -- `encode` could be replaced
// with `vec![]` and `is_modifier_key` with a constant, and every test still
// passed.
// ===========================================================================

#[test]
fn the_normalized_input_dispatcher_routes_both_arms() {
    use terminal_input::{encode, NormalizedInput};

    let key = NormalizedInput::Key {
        event: press(N::Backspace),
        mods: M::empty(),
    };
    assert_eq!(encode(&key, base()), b"\x7f");

    let release = NormalizedInput::Key {
        event: KeyEvent::release(Key::Named(N::Backspace)),
        mods: M::empty(),
    };
    assert_eq!(encode(&release, base() | TermMode::REPORT_EVENT_TYPES), b"");

    let typing = NormalizedInput::Key {
        event: ch("a"),
        mods: M::empty(),
    };
    assert_eq!(encode(&typing, base()), b"a");

    let paste = NormalizedInput::Paste {
        text: "hi".into(),
        bracketed: true,
    };
    assert_eq!(
        encode(&paste, base() | TermMode::BRACKETED_PASTE),
        b"\x1b[200~hi\x1b[201~"
    );
    assert_eq!(encode(&paste, base()), b"hi");

    let unbracketed = NormalizedInput::Paste {
        text: "a\nb".into(),
        bracketed: false,
    };
    assert_eq!(encode(&unbracketed, base() | TermMode::BRACKETED_PASTE), b"a\nb");

    // The two arms must not be interchangeable.
    assert_ne!(encode(&key, base()), encode(&paste, base()));
}

#[test]
fn is_modifier_key_is_exactly_the_four_modifiers() {
    use terminal_input::is_modifier_key;

    for key in [N::Shift, N::Control, N::Alt, N::Super] {
        for loc in [KeyLocation::Standard, KeyLocation::Left, KeyLocation::Right] {
            let ev = press(key).with_location(loc);
            assert!(is_modifier_key(&ev), "{key:?} at {loc:?} should be a modifier key");
        }
    }
    // Hyper and Meta are NOT in upstream's list, even though they are modifiers
    // in the kitty encoding table.
    for key in [
        N::Hyper,
        N::Meta,
        N::CapsLock,
        N::NumLock,
        N::ScrollLock,
        N::Enter,
        N::Tab,
        N::Backspace,
        N::Escape,
        N::Space,
        N::ArrowUp,
        N::F1,
    ] {
        assert!(!is_modifier_key(&press(key)), "{key:?} must not be a modifier key");
    }
    assert!(!is_modifier_key(&ch("a")));
    assert!(!is_modifier_key(&KeyEvent::press(Key::Dead(Some('\u{b4}')))));
    assert!(!is_modifier_key(&KeyEvent::press(Key::Unidentified)));
}

/// `text_with_all_modifiers` IS the encoding for text-producing keys, and a hand-built `KeyEvent`
/// that leaves it `None` encodes to nothing -- silently, while compiling and reading correctly.
///
/// Pinned because it cost a real downstream consumer a broken submit path: it hand-filled a
/// synthetic Enter with `None`, so the keystroke wrote zero bytes, the prompt sat unsent in the
/// composer, and no test caught it because every test double accepted whatever bytes it was given.
/// This asserts both halves so the trap is visible here rather than rediscovered there, and so a
/// future change to which field drives the encoding shows up as a failure.
#[test]
fn enter_encodes_from_text_with_all_modifiers_and_a_hand_built_none_encodes_to_nothing() {
    use terminal_input::keys::{ElementState, KeyLocation};
    let mode = Default::default();

    let constructed = KeyEvent::press(Key::Named(N::Enter));
    assert_eq!(constructed.text_with_all_modifiers.as_deref(), Some("\r"));
    assert_eq!(
        encode_key(&constructed, M::empty(), mode),
        b"\r".to_vec(),
        "press() must produce CR"
    );

    let hand_built = KeyEvent {
        logical_key: Key::Named(N::Enter),
        location: KeyLocation::Standard,
        state: ElementState::Pressed,
        repeat: false,
        text_with_all_modifiers: None,
        key_without_modifiers: Key::Named(N::Enter),
    };
    assert!(
        encode_key(&hand_built, M::empty(), mode).is_empty(),
        "documented trap: the encoding comes from text_with_all_modifiers, so None yields no bytes",
    );
}
