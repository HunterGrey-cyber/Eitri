//! DIFFERENTIAL SWEEP.
//!
//! Walks the full cross product in `support::sweep` and compares
//! `terminal_input::regime` (the production encoder, written as match arms)
//! against `support::reference` (an independent, data-table transliteration of
//! upstream Alacritty). Both call the same vendored `build_sequence`, so this
//! test proves the REGIME layer is upstream-faithful; it says nothing about
//! `build_sequence` itself, which `tests/golden.rs` pins instead.
//!
//! Divergences are not merely counted, they are CLASSIFIED. A divergence is
//! allowed only if it is exactly the documented `APP_KEYPAD` extension, which
//! upstream does not implement at all. Anything else fails the test and is
//! printed with its full coordinates.

mod support;

use alacritty_terminal::term::TermMode;
use support::{reference, sweep};
use terminal_input::keys::{Key, KeyEvent, KeyLocation, ModifiersState, NamedKey};
use terminal_input::NormalizedInput;

/// The one declared extension over upstream: DECPAM application keypad.
/// Mirrors `regime::app_keypad_escape`'s *gate* (not its table) independently,
/// so that a divergence is only excused when the gate genuinely holds.
fn is_app_keypad_case(key: &KeyEvent, mods: ModifiersState, mode: TermMode) -> bool {
    if !mode.contains(TermMode::APP_KEYPAD) || mode.contains(TermMode::VI) {
        return false;
    }
    if key.location != KeyLocation::Numpad || !mods.is_empty() {
        return false;
    }
    if !key.state.is_pressed() {
        return false;
    }
    if mode
        .intersects(TermMode::REPORT_ALL_KEYS_AS_ESC | TermMode::DISAMBIGUATE_ESC_CODES | TermMode::REPORT_EVENT_TYPES)
    {
        return false;
    }
    match key.logical_key.as_ref() {
        Key::Character(c) => {
            matches!(
                c,
                "0" | "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "." | "/" | "*" | "-" | "+" | "="
            )
        }
        Key::Named(NamedKey::Enter) => true,
        _ => false,
    }
}

struct Counts {
    cases: usize,
    identical: usize,
    classified_app_keypad: usize,
    unexplained: usize,
}

fn run_sweep(verbose_failures: usize) -> Counts {
    let modes = sweep::modes();
    let mod_sets = sweep::mod_sets();
    let shapes = sweep::shapes();

    let mut c = Counts {
        cases: 0,
        identical: 0,
        classified_app_keypad: 0,
        unexplained: 0,
    };
    let mut shown = 0;

    for (mode_label, mode) in &modes {
        for (mods_label, mods) in &mod_sets {
            for shape in &shapes {
                for (state_label, state, repeat) in sweep::STATES {
                    let event = shape.event(state, repeat);
                    let got = terminal_input::encode_key(&event, *mods, *mode);
                    let want = reference::encode_key(&event, *mods, *mode);
                    c.cases += 1;

                    // The public `NormalizedInput` dispatcher must agree with the
                    // direct entry point on every single case. Without this the
                    // dispatcher is untested: a mutation sweep found that
                    // `encode` could be replaced with `vec![]` and every test
                    // still passed.
                    let via_enum = terminal_input::encode(
                        &NormalizedInput::Key {
                            event: event.clone(),
                            mods: *mods,
                        },
                        *mode,
                    );
                    assert_eq!(
                        via_enum, got,
                        "encode(NormalizedInput::Key) disagreed with encode_key for \
                         mode={mode_label} mods={mods_label} key={} state={state_label}",
                        shape.label
                    );

                    if got == want {
                        c.identical += 1;
                        continue;
                    }

                    if is_app_keypad_case(&event, *mods, *mode) {
                        c.classified_app_keypad += 1;
                        continue;
                    }

                    c.unexplained += 1;
                    if shown < verbose_failures {
                        shown += 1;
                        eprintln!(
                            "DIVERGENCE  mode={mode_label} mods={mods_label} key={} state={state_label}\n  \
                             production={:?}\n  reference ={:?}",
                            shape.label,
                            String::from_utf8_lossy(&got),
                            String::from_utf8_lossy(&want),
                        );
                    }
                }
            }
        }
    }
    c
}

#[test]
fn differential_sweep_has_no_unexplained_divergence() {
    let modes = sweep::modes().len();
    let mod_sets = sweep::mod_sets().len();
    let shapes = sweep::shapes().len();
    let states = sweep::STATES.len();

    let c = run_sweep(40);

    eprintln!(
        "\nDIFFERENTIAL SWEEP\n  axes: {modes} modes x {mod_sets} modifier sets x {shapes} key \
         shapes x {states} states\n  cases                : {}\n  identical            : {}\n  \
         classified APP_KEYPAD: {}\n  UNEXPLAINED          : {}",
        c.cases, c.identical, c.classified_app_keypad, c.unexplained
    );

    assert_eq!(
        c.cases,
        modes * mod_sets * shapes * states,
        "sweep did not visit every case"
    );
    assert!(c.cases > 1_000_000, "sweep is suspiciously small: {}", c.cases);
    assert_eq!(
        c.unexplained, 0,
        "{} unexplained divergences from upstream",
        c.unexplained
    );
    assert!(
        c.classified_app_keypad > 0,
        "the APP_KEYPAD extension produced no divergence at all, which means the sweep never \
         reached it and the classification arm is dead code"
    );
}

/// Guards the guard. If `run_sweep` ever became a no-op -- or if the two
/// implementations were accidentally aliased to the same function -- the test
/// above would pass vacuously. This proves the comparison has teeth by feeding
/// the reference an input the production encoder is known to treat differently.
#[test]
fn the_comparison_can_actually_fail() {
    // `regime` and `reference` must be distinct code: numpad `7` under
    // APP_KEYPAD is the documented extension, so they MUST disagree here.
    let mode = TermMode::default() | TermMode::APP_KEYPAD;
    let event = KeyEvent::press(Key::Character("7".into())).with_location(KeyLocation::Numpad);
    let got = terminal_input::encode_key(&event, ModifiersState::empty(), mode);
    let want = reference::encode_key(&event, ModifiersState::empty(), mode);
    assert_ne!(
        got, want,
        "production and reference are not independent implementations"
    );
    assert_eq!(got, b"\x1bOw");
    assert_eq!(want, b"7");
}

#[test]
fn paste_matches_the_reference() {
    let samples = [
        "",
        "hello",
        "line one\nline two\nline three",
        "crlf\r\nended",
        "trailing\n",
        "\x1b[201~echo pwned\n",
        "mixed \x03 and \x1b escapes",
        "unicode \u{4f60}\u{597d}\n\u{1f600}",
    ];
    let mut cases = 0;
    for (_, mode) in sweep::modes() {
        for extra in [TermMode::empty(), TermMode::BRACKETED_PASTE] {
            let mode = mode | extra;
            for text in samples {
                for bracketed in [true, false] {
                    let got = terminal_input::encode_paste(text, mode, bracketed);
                    let want = reference::encode_paste(text, mode, bracketed);
                    assert_eq!(got, want, "paste diverged: {text:?} bracketed={bracketed}");
                    let via_enum = terminal_input::encode(
                        &NormalizedInput::Paste {
                            text: text.to_owned(),
                            bracketed,
                        },
                        mode,
                    );
                    assert_eq!(
                        via_enum, got,
                        "encode(NormalizedInput::Paste) disagreed with encode_paste for {text:?}"
                    );
                    cases += 1;
                }
            }
        }
    }
    eprintln!("PASTE DIFFERENTIAL: {cases} cases, 0 divergences");
    assert_eq!(cases, 256 * 2 * 8 * 2);
}
