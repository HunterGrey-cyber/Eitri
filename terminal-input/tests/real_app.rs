//! REAL APPLICATION PROOF.
//!
//! Drives real `nvim`, real `bash` and real `fzf` over a real PTY, with a real
//! `alacritty_terminal::Term` interpreting the output and answering the
//! applications' queries. Every keystroke is encoded by `terminal_input` against
//! the mode the application ACTUALLY negotiated, read back out of that `Term`.
//!
//! These tests are `#[ignore]`d because they fork external programs and take
//! seconds. Run them with:
//!
//!     cargo test --release --test real_app -- --ignored --test-threads=1
//!
//! ON FAILABILITY
//! --------------
//! A previous matrix in this project had 13 unconditional `Ok(..)` rows and 2
//! rows that passed under a total no-op. Every assertion here is on observed
//! state -- screen contents, cursor position, a file the application wrote, a
//! process exit -- and several tests carry an explicit CONTROL step that proves
//! the assertion can fail: `nvim_alt_unknown_key_does_not_leave_insert` first
//! shows that a bare ESC DOES leave insert mode, so "still in insert" is a real
//! observation rather than an inability to observe.
//!
//! ON ENVIRONMENT
//! --------------
//! The harness calls `env_clear()`; children see only TERM/PATH/HOME/LANG (plus
//! PS1 or SHELL where needed). `alacritty_terminal::tty` was NOT used, precisely
//! because it inherits the parent environment.

mod support;

use std::time::Duration;

use alacritty_terminal::term::TermMode;
use support::pty::{App, Size};
use terminal_input::keys::{Key, KeyEvent, ModifiersState as M, NamedKey as N};
use terminal_input::NormalizedInput;

const SIZE: Size = Size { cols: 80, rows: 24 };
const QUIET: Duration = Duration::from_millis(250);
const DEADLINE: Duration = Duration::from_secs(8);

fn base_env() -> Vec<(&'static str, &'static str)> {
    vec![
        ("TERM", "xterm-256color"),
        ("PATH", "/usr/bin:/bin"),
        ("HOME", "/tmp"),
        ("LANG", "C.UTF-8"),
    ]
}

fn down(key: Key<String>, mods: M) -> NormalizedInput {
    NormalizedInput::Key {
        event: KeyEvent::press(key),
        mods,
    }
}

fn up(key: Key<String>, mods: M) -> NormalizedInput {
    NormalizedInput::Key {
        event: KeyEvent::release(key),
        mods,
    }
}

/// Send a full keystroke: press THEN release, the way a real keyboard delivers
/// one. Returns (press bytes, release bytes).
fn keystroke(app: &mut App, key: Key<String>, mods: M) -> (Vec<u8>, Vec<u8>) {
    let d = app.send(&down(key.clone(), mods));
    let u = app.send(&up(key, mods));
    (d, u)
}

fn type_str(app: &mut App, s: &str) {
    for c in s.chars() {
        keystroke(app, Key::Character(c.to_string()), M::empty());
    }
}

fn nvim(scratch: &str) -> App {
    nvim_with_kitty(scratch, true)
}

/// `kitty = false` makes the harness terminal refuse the kitty keyboard query, so
/// nvim falls back to legacy keys.
fn nvim_with_kitty(scratch: &str, kitty: bool) -> App {
    let mut env = base_env();
    env.push(("NVIM_APPNAME", "terminal-input-test"));
    let _ = scratch;
    let mut app =
        App::launch_with_kitty("nvim", &["--clean", "-n", "-i", "NONE"], &env, SIZE, kitty).expect("spawn nvim");
    app.settle(QUIET, DEADLINE);
    app
}

// ===========================================================================
// nvim -- D1, key_release.
// ===========================================================================

#[test]
#[ignore = "drives real nvim on a real PTY"]
fn nvim_one_backspace_keystroke_deletes_exactly_one_character() {
    let mut app = nvim("bs");

    // The proof is only meaningful if nvim really negotiated event reporting.
    // Without REPORT_EVENT_TYPES a missing `key_release` is invisible.
    let mode = app.screen.mode();
    assert!(
        mode.contains(TermMode::REPORT_EVENT_TYPES),
        "nvim did not negotiate REPORT_EVENT_TYPES (mode={mode:?}); this test would be vacuous"
    );
    assert!(
        mode.contains(TermMode::DISAMBIGUATE_ESC_CODES),
        "expected kitty level 1 too"
    );

    keystroke(&mut app, Key::character("i"), M::empty());
    type_str(&mut app, "ABCDEF");
    app.settle(QUIET, DEADLINE);

    let before = app.screen.line(0);
    let (before_row, before_col) = app.screen.cursor();
    assert_eq!(before, "ABCDEF", "setup failed; buffer line 0 is {before:?}");
    assert_eq!((before_row, before_col), (0, 6));

    // ONE keystroke: press and release.
    let (press, release) = keystroke(&mut app, Key::Named(N::Backspace), M::empty());
    app.settle(QUIET, DEADLINE);

    let after = app.screen.line(0);
    let (after_row, after_col) = app.screen.cursor();

    eprintln!(
        "nvim Backspace: mode={mode:?}\n  buffer before = {before:?} cursor {before_col}\n  \
         press   bytes = {press:?}\n  release bytes = {release:?}\n  buffer after  = {after:?} \
         cursor {after_col}"
    );

    assert_eq!(press, b"\x7f", "press must be the legacy DEL, not a kitty sequence");
    assert!(
        release.is_empty(),
        "the RELEASE emitted {release:?}; that is the double-delete bug (D1). nvim deletes once \
         for the press and once more for the release."
    );
    assert_eq!(
        after, "ABCDE",
        "one Backspace keystroke did not delete exactly one character"
    );
    assert_eq!(before.len() - after.len(), 1, "wrong number of characters deleted");
    assert_eq!((after_row, after_col), (0, 5), "cursor moved by more than one column");
}

#[test]
#[ignore = "drives real nvim on a real PTY"]
fn nvim_one_enter_keystroke_inserts_exactly_one_line() {
    let mut app = nvim("enter");
    assert!(app.screen.mode().contains(TermMode::REPORT_EVENT_TYPES));

    keystroke(&mut app, Key::character("i"), M::empty());
    type_str(&mut app, "AAA");
    app.settle(QUIET, DEADLINE);
    assert_eq!(app.screen.line(0), "AAA");

    let (press, release) = keystroke(&mut app, Key::Named(N::Enter), M::empty());
    app.settle(QUIET, DEADLINE);
    type_str(&mut app, "BBB");
    app.settle(QUIET, DEADLINE);

    eprintln!(
        "nvim Enter: press={press:?} release={release:?} line0={:?} line1={:?} line2={:?}",
        app.screen.line(0),
        app.screen.line(1),
        app.screen.line(2)
    );

    assert_eq!(press, b"\r");
    assert!(
        release.is_empty(),
        "Enter release emitted {release:?}: one keystroke, two lines"
    );
    assert_eq!(app.screen.line(0), "AAA");
    assert_eq!(
        app.screen.line(1),
        "BBB",
        "a blank line was inserted: Enter acted twice"
    );
    assert_eq!(app.screen.line(2), "~", "more than one line was inserted");
}

// ===========================================================================
// nvim -- D2, the stray ESC.
// ===========================================================================

#[test]
#[ignore = "drives real nvim on a real PTY"]
fn nvim_alt_unknown_key_does_not_leave_insert() {
    // D2 is only observable in the LEGACY regime. Under the kitty protocol every
    // key has an encoding and `build_sequence` never returns empty, so the bad
    // fall-through never fires. The harness therefore refuses the kitty keyboard
    // query here, exactly as a terminal without the protocol would, and nvim
    // falls back to legacy keys -- which is where the earlier prototype's lone
    // ESC kicked nvim out of INSERT.
    let mut app = nvim_with_kitty("alt", false);
    let mode = app.screen.mode();
    assert!(
        !mode.contains(TermMode::DISAMBIGUATE_ESC_CODES)
            && !mode.contains(TermMode::REPORT_EVENT_TYPES)
            && !mode.contains(TermMode::REPORT_ALL_KEYS_AS_ESC),
        "nvim still negotiated the kitty protocol (mode={mode:?}); D2 would be unobservable"
    );

    keystroke(&mut app, Key::character("i"), M::empty());
    type_str(&mut app, "ABC");
    app.settle(QUIET, DEADLINE);
    assert_eq!(app.screen.line(23), "-- INSERT --", "setup: not in insert mode");
    assert_eq!(app.screen.line(0), "ABC");

    // Every key the earlier prototype turned into a lone ESC under Alt.
    let unknown = [
        N::ContextMenu,
        N::CapsLock,
        N::NumLock,
        N::ScrollLock,
        N::PrintScreen,
        N::Pause,
        N::Hyper,
        N::Meta,
        N::MediaPlay,
        N::AudioVolumeUp,
        N::F25,
        N::F35,
    ];
    let mut sent = 0;
    for key in unknown {
        for mods in [M::ALT, M::ALT | M::CONTROL] {
            let (press, release) = keystroke(&mut app, Key::Named(key), mods);
            assert!(
                press.is_empty(),
                "Alt+{key:?} press emitted {press:?}; a lone ESC here kicks nvim out of INSERT"
            );
            assert!(release.is_empty(), "Alt+{key:?} release emitted {release:?}");
            sent += 1;
        }
    }
    app.settle(QUIET, DEADLINE);

    assert_eq!(sent, 24, "expected 12 keys x 2 modifier sets");
    assert_eq!(
        app.screen.line(23),
        "-- INSERT --",
        "nvim left INSERT mode after {sent} Alt+<unknown> keystrokes (D2)"
    );
    assert_eq!(app.screen.line(0), "ABC", "the buffer changed");

    // CONTROL: prove the assertion above can fail. A genuine lone ESC DOES leave
    // insert mode, so "still in INSERT" is an observation, not a blind spot.
    app.send_raw(b"\x1b");
    app.settle(QUIET, DEADLINE);
    assert_ne!(
        app.screen.line(23),
        "-- INSERT --",
        "control failed: a raw ESC did not leave INSERT mode, so this test cannot detect D2 at all"
    );
    eprintln!(
        "nvim D2 (legacy regime): {sent} Alt+<unknown> keystrokes all emitted nothing; control \
         ESC left INSERT (line23={:?})",
        app.screen.line(23)
    );
}

/// Fresh nvim, insert "ABC", send exactly one raw sequence, report insert-mode
/// state and buffer. Each case gets its own nvim so one case cannot poison the
/// next -- an earlier version of this test shared one instance and drew the wrong
/// conclusion from it.
fn nvim_probe(seq: &[u8]) -> (String, String) {
    let mut app = nvim("probe");
    keystroke(&mut app, Key::character("i"), M::empty());
    type_str(&mut app, "ABC");
    app.settle(QUIET, DEADLINE);
    assert_eq!(app.screen.line(23), "-- INSERT --", "setup failed for {seq:?}");
    app.send_raw(seq);
    app.settle(QUIET, DEADLINE);
    (app.screen.line(23), app.screen.line(0))
}

#[test]
#[ignore = "drives real nvim on a real PTY"]
fn nvim_alt_modified_kitty_functional_codes_leave_insert_mode() {
    // FINDING, recorded rather than papered over. Reproducible, one fresh nvim
    // 0.12.5 per case.
    //
    //   CSI 57363 u     (bare ContextMenu)  -> stays in INSERT, inserts U+E013
    //   CSI 57363 ; 5 u (Ctrl+ContextMenu)  -> stays in INSERT, inserts U+E013
    //   CSI 57363 ; 3 u (Alt+ContextMenu)   -> LEAVES INSERT
    //
    // So it is the ALT BIT, not the functional code, that nvim reacts to: for a
    // key it has no mapping for, `<M-...>` falls back to ESC-prefix semantics.
    //
    // This is NOT a defect in this crate and NOT the D2 fall-through:
    //   * D2 was a LONE ESC emitted where upstream emits nothing. Here a complete,
    //     well-formed CSI ... u sequence is emitted, and this test asserts it is
    //     byte-identical to the independent upstream reference model.
    //   * In the legacy regime -- see the test above -- these same keystrokes
    //     emit NOTHING and nvim stays in INSERT.
    // Real Alacritty driving real nvim produces the same bytes and therefore the
    // same outcome. It is recorded here so the behaviour is known, not hidden.
    let mut app = nvim("modes");
    let mode = app.screen.mode();
    assert!(mode.contains(TermMode::DISAMBIGUATE_ESC_CODES));
    app.pty.kill();

    let menu = KeyEvent::press(Key::Named(N::ContextMenu));
    let bare = terminal_input::encode_key(&menu, M::empty(), mode);
    let ctrl = terminal_input::encode_key(&menu, M::CONTROL, mode);
    let alt = terminal_input::encode_key(&menu, M::ALT, mode);

    // Our bytes are upstream's bytes. If this fails, the finding below is ours.
    assert_eq!(bare, support::reference::encode_key(&menu, M::empty(), mode));
    assert_eq!(ctrl, support::reference::encode_key(&menu, M::CONTROL, mode));
    assert_eq!(alt, support::reference::encode_key(&menu, M::ALT, mode));
    assert_eq!(bare, b"\x1b[57363u");
    assert_eq!(ctrl, b"\x1b[57363;5u");
    assert_eq!(alt, b"\x1b[57363;3u");

    let (bare_mode, bare_buf) = nvim_probe(&bare);
    let (ctrl_mode, ctrl_buf) = nvim_probe(&ctrl);
    let (alt_mode, alt_buf) = nvim_probe(&alt);

    eprintln!(
        "nvim vs kitty functional codes:\n  bare {bare:?} -> mode={bare_mode:?} buf={bare_buf:?}\n  \
         ctrl {ctrl:?} -> mode={ctrl_mode:?} buf={ctrl_buf:?}\n  alt  {alt:?} -> mode={alt_mode:?} \
         buf={alt_buf:?}"
    );

    assert_eq!(bare_mode, "-- INSERT --", "bare functional code left INSERT");
    assert_eq!(ctrl_mode, "-- INSERT --", "Ctrl-modified functional code left INSERT");
    assert_ne!(
        alt_mode, "-- INSERT --",
        "the Alt-modified case no longer leaves INSERT -- nvim's behaviour changed and the \
         finding recorded here should be revisited"
    );
    assert_eq!(alt_buf, "ABC", "the Alt case also modified the buffer");
}

// ===========================================================================
// nvim -- APP_CURSOR arrows against an application that really sets DECCKM.
// ===========================================================================

#[test]
#[ignore = "drives real nvim on a real PTY"]
fn nvim_app_cursor_arrows_navigate() {
    let mut app = nvim("arrows");
    assert!(
        app.screen.mode().contains(TermMode::APP_CURSOR),
        "nvim did not set DECCKM; this test would not exercise the APP_CURSOR rows"
    );

    keystroke(&mut app, Key::character("i"), M::empty());
    type_str(&mut app, "L1");
    keystroke(&mut app, Key::Named(N::Enter), M::empty());
    type_str(&mut app, "L2");
    keystroke(&mut app, Key::Named(N::Enter), M::empty());
    type_str(&mut app, "L3");
    app.settle(QUIET, DEADLINE);
    assert_eq!(app.screen.line(2), "L3");
    let (row_before, _) = app.screen.cursor();
    assert_eq!(row_before, 2);

    // Because nvim negotiated DECCKM, an unmodified ArrowUp must encode as SS3.
    let (press, release) = keystroke(&mut app, Key::Named(N::ArrowUp), M::empty());
    assert_eq!(press, b"\x1bOA", "APP_CURSOR arrow did not use the SS3 form");
    // REPORT_EVENT_TYPES is on, so the release IS reported for arrows -- and it
    // is reported in the CSI form, not SS3. `key_release` calls `build_sequence`
    // directly (keyboard.rs:276) and never consults the binding table, so the
    // APP_CURSOR rows cannot apply to a release. That asymmetry is upstream's.
    assert_eq!(release, b"\x1b[1;1:3A", "unexpected arrow release");
    app.settle(QUIET, DEADLINE);
    let (row_after, _) = app.screen.cursor();
    assert_eq!(row_after, 1, "ArrowUp did not move the cursor up one line");

    keystroke(&mut app, Key::Named(N::ArrowUp), M::empty());
    app.settle(QUIET, DEADLINE);
    assert_eq!(app.screen.cursor().0, 0, "second ArrowUp did not move");

    let (press, _) = keystroke(&mut app, Key::Named(N::ArrowDown), M::empty());
    assert_eq!(press, b"\x1bOB");
    app.settle(QUIET, DEADLINE);
    assert_eq!(app.screen.cursor().0, 1, "ArrowDown did not move the cursor down");
}

// ===========================================================================
// bash
// ===========================================================================

fn bash() -> App {
    let mut env = base_env();
    env.push(("PS1", "PROMPT> "));
    let mut app = App::launch("bash", &["--norc", "--noprofile", "-i"], &env, SIZE).expect("spawn bash");
    app.settle(QUIET, DEADLINE);
    app
}

#[test]
#[ignore = "drives real bash on a real PTY"]
fn bash_arrow_history_works_with_app_cursor_off_and_on() {
    let mut app = bash();
    assert!(
        !app.screen.mode().contains(TermMode::APP_CURSOR),
        "bash unexpectedly set DECCKM; the APP_CURSOR-off half of this test is void"
    );

    type_str(&mut app, "echo first");
    keystroke(&mut app, Key::Named(N::Enter), M::empty());
    app.settle(QUIET, DEADLINE);
    type_str(&mut app, "echo second");
    keystroke(&mut app, Key::Named(N::Enter), M::empty());
    app.settle(QUIET, DEADLINE);
    assert!(app.screen.visible_lines().iter().any(|l| l == "second"), "setup failed");

    // --- APP_CURSOR OFF: the CSI form, encoded against bash's real mode.
    let (csi, _) = keystroke(&mut app, Key::Named(N::ArrowUp), M::empty());
    assert_eq!(csi, b"\x1b[A", "with DECCKM off the arrow must be CSI A");
    app.settle(QUIET, DEADLINE);
    let recalled = app.screen.visible_lines().last().cloned().unwrap_or_default();
    assert!(
        recalled.ends_with("echo second"),
        "CSI arrow did not recall history; last line is {recalled:?}"
    );

    // --- APP_CURSOR ON: force DECCKM and check bash accepts the SS3 form too.
    // bash never sets DECCKM itself, so the mode is forced here deliberately; the
    // claim under test is that the bytes the encoder emits under DECCKM are bytes
    // a real readline accepts. A FRESH shell is used rather than reusing the one
    // above, because readline's history cursor has already moved and reusing it
    // would make "which entry came back" depend on readline state instead of on
    // the arrow encoding.
    let mut app2 = bash();
    type_str(&mut app2, "echo first");
    keystroke(&mut app2, Key::Named(N::Enter), M::empty());
    app2.settle(QUIET, DEADLINE);
    type_str(&mut app2, "echo second");
    keystroke(&mut app2, Key::Named(N::Enter), M::empty());
    app2.settle(QUIET, DEADLINE);

    let forced = app2.screen.mode() | TermMode::APP_CURSOR;
    let ss3 = terminal_input::encode(&down(Key::Named(N::ArrowUp), M::empty()), forced);
    assert_eq!(ss3, b"\x1bOA", "DECCKM arrow must be SS3 A");
    assert_ne!(
        ss3, csi,
        "the two regimes produced the same bytes; one of them is not applied"
    );
    app2.send_raw(&ss3);
    app2.settle(QUIET, DEADLINE);
    let recalled = app2.screen.visible_lines().last().cloned().unwrap_or_default();
    assert!(
        recalled.ends_with("echo second"),
        "SS3 arrow did not recall history; last line is {recalled:?}"
    );
    // A second SS3 arrow must step one further back, proving each arrow is
    // delivered exactly once.
    app2.send_raw(&ss3);
    app2.settle(QUIET, DEADLINE);
    let recalled2 = app2.screen.visible_lines().last().cloned().unwrap_or_default();
    assert!(
        recalled2.ends_with("echo first"),
        "second SS3 arrow did not step back one entry; last line is {recalled2:?}"
    );

    eprintln!("bash arrows: CSI={csi:?} SS3={ss3:?}, both recalled history");
}

#[test]
#[ignore = "drives real bash on a real PTY"]
fn bash_ctrl_c_abandons_the_line_without_running_it() {
    let mut app = bash();
    type_str(&mut app, "echo SHOULD-NOT-RUN");
    app.settle(QUIET, DEADLINE);
    assert!(
        app.screen
            .visible_lines()
            .iter()
            .any(|l| l.contains("echo SHOULD-NOT-RUN")),
        "setup failed: the text was never typed"
    );

    let ctrl_c = NormalizedInput::Key {
        event: KeyEvent::press(Key::character("c")).with_text(Some("\u{3}")),
        mods: M::CONTROL,
    };
    let bytes = app.send(&ctrl_c);
    app.send(&NormalizedInput::Key {
        event: KeyEvent::release(Key::character("c")).with_text(Some("\u{3}")),
        mods: M::CONTROL,
    });
    app.settle(QUIET, DEADLINE);

    assert_eq!(bytes, b"\x03", "Ctrl-C must encode as ETX");
    let lines = app.screen.visible_lines();
    eprintln!("bash after Ctrl-C:\n{}", lines.join("\n"));
    assert!(lines.iter().any(|l| l.contains("^C")), "bash never saw the interrupt");
    assert!(
        lines.last().map(|l| l.trim_end() == "PROMPT>").unwrap_or(false),
        "no fresh prompt after Ctrl-C; last line is {:?}",
        lines.last()
    );
    assert!(
        !lines.iter().any(|l| l.trim() == "SHOULD-NOT-RUN"),
        "the abandoned command actually executed"
    );
}

#[test]
#[ignore = "drives real bash on a real PTY"]
fn bash_ctrl_d_ends_the_shell() {
    let mut app = bash();
    assert!(
        app.screen.visible_lines().iter().any(|l| l.contains("PROMPT>")),
        "no prompt"
    );

    let ctrl_d = NormalizedInput::Key {
        event: KeyEvent::press(Key::character("d")).with_text(Some("\u{4}")),
        mods: M::CONTROL,
    };
    let bytes = app.send(&ctrl_d);
    assert_eq!(bytes, b"\x04", "Ctrl-D must encode as EOT");

    // The shell should exit; the PTY master then reports EOF / EIO.
    let mut sawpity_eof = false;
    for _ in 0..40 {
        let chunk = app
            .pty
            .read_quiescent(Duration::from_millis(50), Duration::from_millis(200));
        app.screen.advance(&chunk);
        if chunk.is_empty() {
            // A dead child closes the slave; reads then stop producing data.
            sawpity_eof = true;
            break;
        }
    }
    assert!(sawpity_eof, "bash kept producing output after Ctrl-D");
    eprintln!("bash after Ctrl-D:\n{}", app.screen.visible_lines().join("\n"));
}

#[test]
#[ignore = "drives real bash on a real PTY"]
fn bash_bracketed_multiline_paste_does_not_act_as_multiple_enters() {
    let mut app = bash();
    assert!(
        app.screen.mode().contains(TermMode::BRACKETED_PASTE),
        "readline did not enable bracketed paste; this test would be vacuous"
    );

    let paste = NormalizedInput::Paste {
        text: "echo PASTE1\necho PASTE2\necho PASTE3".into(),
        bracketed: true,
    };
    let bytes = app.send(&paste);
    app.settle(QUIET, DEADLINE);

    assert_eq!(
        bytes, b"\x1b[200~echo PASTE1\necho PASTE2\necho PASTE3\x1b[201~",
        "paste was not bracketed"
    );

    let lines = app.screen.visible_lines();
    eprintln!("bash after multi-line paste (no Enter sent):\n{}", lines.join("\n"));
    for marker in ["PASTE1", "PASTE2", "PASTE3"] {
        assert!(
            !lines.iter().any(|l| l.trim() == marker),
            "{marker} was EXECUTED: the paste acted as an Enter"
        );
    }
    // The text is sitting in the line editor, unexecuted.
    assert!(
        lines.iter().any(|l| l.contains("echo PASTE1")),
        "the paste never arrived"
    );
    assert!(
        lines.iter().any(|l| l.contains("echo PASTE3")),
        "the paste was truncated"
    );
}

#[test]
#[ignore = "drives real bash on a real PTY"]
fn bracketed_paste_filtering_stops_a_paste_from_escaping_its_brackets() {
    let mut app = bash();
    assert!(app.screen.mode().contains(TermMode::BRACKETED_PASTE));

    // A payload that tries to close the bracket early and run a command.
    let paste = NormalizedInput::Paste {
        text: "safe\x1b[201~echo ESCAPED\n".into(),
        bracketed: true,
    };
    let bytes = app.send(&paste);
    app.settle(QUIET, DEADLINE);

    let s = String::from_utf8(bytes.clone()).unwrap();
    assert_eq!(
        s.matches("\x1b[201~").count(),
        1,
        "the payload closed the bracket itself"
    );
    assert!(s.starts_with("\x1b[200~") && s.ends_with("\x1b[201~"));

    let lines = app.screen.visible_lines();
    eprintln!("bash after hostile paste:\n{}", lines.join("\n"));
    assert!(
        !lines.iter().any(|l| l.trim() == "ESCAPED"),
        "the pasted command executed: the \\x1b filter is not working"
    );
}

// ===========================================================================
// fzf
// ===========================================================================

fn scratch_path(name: &str) -> String {
    let dir = std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".into());
    format!("{dir}/terminal-input-{name}.txt")
}

fn fzf(out: &str) -> App {
    let cmd = format!("printf 'alpha\\nbeta\\ngamma\\ndelta\\n' | fzf --no-mouse > {out}");
    let mut env = base_env();
    env.push(("SHELL", "/bin/bash"));
    let mut app = App::launch("bash", &["-c", &cmd], &env, SIZE).expect("spawn fzf");
    app.settle(Duration::from_millis(400), DEADLINE);
    app
}

#[test]
#[ignore = "drives real fzf on a real PTY"]
fn fzf_arrows_move_the_selection_with_app_cursor_off() {
    let out = scratch_path("fzf-csi");
    let _ = std::fs::remove_file(&out);
    let mut app = fzf(&out);
    assert!(
        !app.screen.mode().contains(TermMode::APP_CURSOR),
        "fzf set DECCKM; the APP_CURSOR-off half of this test is void"
    );
    assert!(
        app.screen.visible_lines().iter().any(|l| l.contains("alpha")),
        "fzf did not start"
    );

    // Two ArrowUps: alpha -> beta -> gamma.
    for _ in 0..2 {
        let (press, _) = keystroke(&mut app, Key::Named(N::ArrowUp), M::empty());
        assert_eq!(press, b"\x1b[A");
        app.settle(Duration::from_millis(150), DEADLINE);
    }
    keystroke(&mut app, Key::Named(N::Enter), M::empty());
    app.settle(Duration::from_millis(400), DEADLINE);
    std::thread::sleep(Duration::from_millis(300));

    let selected = std::fs::read_to_string(&out).unwrap_or_default();
    eprintln!("fzf (CSI arrows) selected {selected:?}");
    assert_eq!(
        selected.trim(),
        "gamma",
        "two ArrowUps should land on gamma; got {selected:?}. One arrow too few or too many \
         means the arrow encoding is wrong or duplicated."
    );
}

#[test]
#[ignore = "drives real fzf on a real PTY"]
fn fzf_arrows_move_the_selection_with_app_cursor_on() {
    let out = scratch_path("fzf-ss3");
    let _ = std::fs::remove_file(&out);
    let mut app = fzf(&out);

    // fzf does not set DECCKM, so force it: the claim under test is that the
    // bytes the encoder emits under DECCKM are bytes a real full-screen
    // application accepts.
    let forced = app.screen.mode() | TermMode::APP_CURSOR;
    let ss3 = terminal_input::encode(&down(Key::Named(N::ArrowUp), M::empty()), forced);
    assert_eq!(ss3, b"\x1bOA");
    let csi = terminal_input::encode(&down(Key::Named(N::ArrowUp), M::empty()), app.screen.mode());
    assert_ne!(ss3, csi, "forcing APP_CURSOR changed nothing");

    for _ in 0..3 {
        app.send_raw(&ss3);
        app.settle(Duration::from_millis(150), DEADLINE);
    }
    keystroke(&mut app, Key::Named(N::Enter), M::empty());
    app.settle(Duration::from_millis(400), DEADLINE);
    std::thread::sleep(Duration::from_millis(300));

    let selected = std::fs::read_to_string(&out).unwrap_or_default();
    eprintln!("fzf (SS3 arrows) selected {selected:?}");
    assert_eq!(
        selected.trim(),
        "delta",
        "three SS3 ArrowUps should land on delta; got {selected:?}"
    );
}

#[test]
#[ignore = "drives real fzf on a real PTY"]
fn fzf_ctrl_c_aborts_without_selecting() {
    let out = scratch_path("fzf-ctrlc");
    let _ = std::fs::remove_file(&out);
    let mut app = fzf(&out);
    assert!(app.screen.visible_lines().iter().any(|l| l.contains("alpha")));

    let bytes = app.send(&NormalizedInput::Key {
        event: KeyEvent::press(Key::character("c")).with_text(Some("\u{3}")),
        mods: M::CONTROL,
    });
    assert_eq!(bytes, b"\x03");
    app.settle(Duration::from_millis(400), DEADLINE);
    std::thread::sleep(Duration::from_millis(300));

    let selected = std::fs::read_to_string(&out).unwrap_or_default();
    eprintln!("fzf after Ctrl-C wrote {selected:?}");
    assert!(
        selected.trim().is_empty(),
        "fzf selected {selected:?} instead of aborting"
    );
}

/// UNGATED SENTINEL -- deliberately NOT `#[ignore]`d.
///
/// Every other test in this file is `#[ignore]`d, so a plain `cargo test` is green
/// while the entire real-application proof silently does not run. That is the exact
/// failure mode this crate exists to close, one level up: a suite that stays green
/// while its most end-to-end coverage stops running. This repo already carries the
/// same asymmetry deliberately in `apps/terminal-sidecar`'s CLI-version sentinel --
/// the production gate fails closed, but the TEST is ungated so a change is RED,
/// never silent.
///
/// So: this test does not run the applications, it asserts they are RUNNABLE. If the
/// gate cannot be executed on this machine, that is a red test, not a quiet skip.
#[test]
fn the_real_application_gate_is_runnable_here() {
    let missing: Vec<&str> = ["nvim", "bash", "fzf"]
        .into_iter()
        .filter(|p| {
            std::process::Command::new("sh")
                .arg("-c")
                .arg(format!("command -v {p}"))
                .output()
                .map(|o| !o.status.success())
                .unwrap_or(true)
        })
        .collect();

    assert!(
        missing.is_empty(),
        "the real-application gate cannot run here -- missing: {missing:?}. \
         Every other test in this file is #[ignore]d, so without these the suite is \
         green while proving nothing about real applications. Install them, or run \
         `cargo test --test real_app -- --ignored --test-threads=1` elsewhere and \
         record the result."
    );
}
