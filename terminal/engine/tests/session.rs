//! The session thread, end to end: bytes from a real child, through the `Term`, to a `PaintList`;
//! keys the other way; the answers a program waits for; and what idle costs.

mod common;
#[cfg(target_os = "linux")]
#[path = "common/process_identity.rs"]
mod process_identity;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::{ctrl, plain_sh, size, spec, ChildGate, Harness, WAIT};
use eitri_terminal::mouse::{Button, MouseInput, MouseKind, MouseMods};
use eitri_terminal::{CursorCell, PtySize, SessionCommand, SessionConfig, TerminalColors, TerminalMetrics, REPLY_CAP};
use terminal_input::NormalizedInput;
use terminal_render::RgbColor;

fn left_press(line: u16, col: u16) -> MouseInput {
    MouseInput {
        kind: MouseKind::Press(Button::Left),
        line,
        col,
        mods: MouseMods::default(),
    }
}

#[test]
fn typed_keys_are_echoed_by_the_shell() {
    let mut h = Harness::start(plain_sh(), size(80, 24));
    h.wait_for(WAIT, |h| h.text().first().is_some_and(|l| l.starts_with('$')));
    h.type_str("echo hi\n");
    h.wait_for(WAIT, |h| {
        let t = h.text();
        t.iter()
            .position(|l| l == "$ echo hi")
            .is_some_and(|i| t.get(i + 1).is_some_and(|l| l == "hi"))
    });
}

#[test]
fn keyboard_push_overflow_does_not_hang_up_the_shell() {
    let script = r#"i=0; while [ "$i" -lt 4097 ]; do printf '\033[>1u'; i=$((i + 1)); done
printf '\033[<4096uready\n'; read answer; printf 'received=%s\n' "$answer""#;
    let mut h = Harness::start(spec("/bin/sh", &["-c", script]), size(80, 10));
    h.wait_for(WAIT, |h| h.exited.is_some() || h.has_line("ready"));
    assert!(h.exited.is_none(), "the shell must still be waiting for input");
    h.type_str("still-here\n");
    h.wait_for(WAIT, |h| h.exited.is_some());
    assert!(h.has_line("received=still-here"), "{}", h.text().join("\n"));
}

/// `Ctrl+a Ctrl+a` hands the program a literal Ctrl+a, as a doubled tmux prefix does: 0x01 through
/// the encoder, not a raw byte, and it arrives -- `cat -v` shows it.
#[test]
fn ctrl_a_arrives_as_0x01() {
    assert_eq!(terminal_input::encode(&ctrl('a'), Default::default()), vec![0x01]);
    assert_eq!(terminal_input::encode(&ctrl('l'), Default::default()), vec![0x0c]);
    let mut h = Harness::start(plain_sh(), size(80, 24));
    h.wait_for(WAIT, |h| h.text().first().is_some_and(|l| l.starts_with('$')));
    h.type_str("cat -v\n");
    h.wait_for(WAIT, |h| h.has_line("$ cat -v"));
    h.send(ctrl('a'));
    h.type_str("\n");
    // The tty echoes ^A, then cat prints its own ^A.
    h.wait_for(WAIT, |h| h.text().iter().filter(|l| l.as_str() == "^A").count() >= 2);
}

#[test]
fn stty_size_follows_resize_both_ways() {
    let mut h = Harness::start(plain_sh(), size(80, 24));
    h.wait_for(WAIT, |h| h.text().first().is_some_and(|l| l.starts_with('$')));
    h.type_str("stty size\n");
    h.wait_for(WAIT, |h| h.has_line("24 80"));
    h.session.send(SessionCommand::Resize(size(100, 30)));
    h.type_str("stty size\n");
    h.wait_for(WAIT, |h| h.has_line("30 100"));
    assert_eq!(h.frame.as_ref().map(|f| (f.cols, f.rows)), Some((100, 30)));
    h.session.send(SessionCommand::Resize(size(50, 10)));
    h.type_str("clear; stty size\n");
    h.wait_for(WAIT, |h| h.has_line("10 50"));
    assert_eq!(h.frame.as_ref().map(|f| (f.cols, f.rows)), Some((50, 10)));
}

/// A text-size change is a metrics rebuild at the same pane size, and the grid it gives is what the
/// shell is told: twice the font is fewer cells, and `stty size` reports exactly the rebuilt grid, both
/// ways -- the same `PtySize::from_metrics` a pane resize sends, so the drawn grid and the PTY's agree.
#[test]
fn a_font_size_change_resizes_the_pty_to_the_rebuilt_grid() {
    let (width, height) = (1280.0, 480.0);
    let at = |px: f32| TerminalMetrics::with_families(&["monospace".to_string()], px, width, height, 1.0);
    let small = PtySize::from_metrics(&at(14.0));
    let large = PtySize::from_metrics(&at(28.0));
    assert!(
        large.cols < small.cols && large.rows < small.rows,
        "twice the font must be fewer cells: {small:?} -> {large:?}"
    );
    let mut h = Harness::start(plain_sh(), small);
    h.wait_for(WAIT, |h| h.text().first().is_some_and(|l| l.starts_with('$')));
    h.type_str("stty size\n");
    let says = |size: PtySize| format!("{} {}", size.rows, size.cols);
    h.wait_for(WAIT, |h| h.has_line(&says(small)));
    h.session.send(SessionCommand::Resize(large));
    h.type_str("clear; stty size\n");
    h.wait_for(WAIT, |h| h.has_line(&says(large)));
    assert_eq!(
        h.frame.as_ref().map(|f| (f.cols, f.rows)),
        Some((large.cols, large.rows))
    );
    h.session.send(SessionCommand::Resize(small));
    h.type_str("clear; stty size\n");
    h.wait_for(WAIT, |h| h.has_line(&says(small)));
    assert_eq!(
        h.frame.as_ref().map(|f| (f.cols, f.rows)),
        Some((small.cols, small.rows))
    );
}

/// A cursor-position query must get an answer: dropping it with a `VoidListener` makes the
/// program wait out its timeout.
#[test]
fn a_cursor_position_query_is_answered() {
    let script = r#"stty -icanon -echo min 0 time 20; printf '\033[6n'; r=$(dd bs=1 count=6 2>/dev/null); printf 'reply=%s\n' "$(printf %s "$r" | od -An -c | tr -s ' ')""#;
    let mut h = Harness::start(spec("/bin/sh", &["-c", script]), size(80, 10));
    h.wait_for(WAIT, |h| h.exited.is_some());
    let text = h.text().join("\n");
    assert!(text.contains("033 [ 1 ; 1 R"), "{text}");
}

/// OSC 11 ("what is your background?") is answered from the theme's background, so a program that
/// picks light or dark from it (nvim, delta) picks right.
#[test]
fn a_background_colour_query_is_answered_from_the_theme() {
    let colors = TerminalColors {
        background: RgbColor::new(0x12, 0x34, 0x56),
        foreground: RgbColor::new(0xee, 0xee, 0xee),
        cursor: None,
    };
    let script = r#"stty -icanon -echo min 0 time 20; printf '\033]11;?\007'; r=$(dd bs=1 count=24 2>/dev/null); printf 'reply=%s\n' "$(printf %s "$r" | tr -d '\033\007')""#;
    let mut h = Harness::start_with(SessionConfig {
        spawn: spec("/bin/sh", &["-c", script]),
        size: size(80, 10),
        colors,
        focused: true,
        tap: None,
    });
    h.wait_for(WAIT, |h| h.exited.is_some());
    let text = h.text().join("\n");
    assert!(text.contains("reply=]11;rgb:1212/3434/5656"), "{text}");
}

/// An OSC 52 copy to the selection (`s`, nvim's `"*`) reaches the host as
/// the primary selection, apart from a copy to the clipboard in the same stream.
#[test]
fn an_osc52_selection_copy_reaches_the_host_apart_from_the_clipboard() {
    let script = r#"printf '\033]52;s;aGk=\007\033]52;c;Ynll\007'"#;
    let mut h = Harness::start(spec("/bin/sh", &["-c", script]), size(80, 5));
    h.wait_for(WAIT, |h| h.exited.is_some());
    assert_eq!(h.events.primary.as_deref(), Some("hi"));
    assert_eq!(h.events.clipboard.as_deref(), Some("bye"));
}

/// A copy is the one event a hidden terminal rings the host for (the pump puts it on the desktop's
/// clipboard at once, hidden or not). Selection copies must ring it too: if `publish_events`
/// rings only for `clipboard`, a hidden terminal's `"*y` waits unseen until the next show.
#[test]
fn a_hidden_selection_copy_rings_the_host_like_a_clipboard_copy() {
    let script = r#"stty -echo; printf ready; read _; printf '\033]52;s;aGk=\007'; read _"#;
    let (arrived, arrival) = std::sync::mpsc::channel();
    let (release, released) = std::sync::mpsc::channel();
    let armed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let tap_armed = armed.clone();
    let mut h = Harness::start_with(SessionConfig {
        spawn: spec("/bin/sh", &["-c", script]),
        size: size(40, 5),
        colors: TerminalColors::default(),
        focused: true,
        tap: Some(Box::new(move |_| {
            if tap_armed.swap(false, std::sync::atomic::Ordering::SeqCst) {
                let _ = arrived.send(());
                released
                    .recv_timeout(Duration::from_secs(60))
                    .expect("release the copy read");
            }
        })),
    });
    h.wait_for(WAIT, |h| h.text().first().is_some_and(|l| l.starts_with("ready")));
    h.session.send(SessionCommand::Visible(false));
    armed.store(true, std::sync::atomic::Ordering::SeqCst);
    h.type_str("\n");
    arrival
        .recv_timeout(Duration::from_secs(60))
        .expect("the copy read arrived");
    h.pending_wakes();
    release.send(()).unwrap();
    h.wait_for_wake(Duration::from_secs(60));
    h.wait_for(WAIT, |h| h.events.primary.is_some());
    assert_eq!(h.events.primary.as_deref(), Some("hi"));
}

/// Every frame comes with the cell the cursor is in, even when the program
/// hid it and the frame paints no cursor -- the host draws an input method's preedit there.
#[test]
fn every_frame_says_where_the_cursor_is_even_when_it_is_hidden() {
    let script = r#"printf 'ab\033[?25l'; read x"#;
    let mut h = Harness::start(spec("/bin/sh", &["-c", script]), size(40, 5));
    h.wait_for(WAIT, |h| {
        h.text().first().is_some_and(|l| l == "ab") && h.cursor.is_some_and(|c| !c.visible)
    });
    assert!(
        h.frame.as_ref().unwrap().cursor().is_none(),
        "the frame paints no cursor"
    );
    assert_eq!(
        h.cursor,
        Some(CursorCell {
            row: 0,
            col: 2,
            visible: false
        })
    );
}

#[test]
fn the_title_reaches_the_host() {
    let mut h = Harness::start(
        spec("/bin/sh", &["-c", r"printf '\033]2;eitri-title\007'"]),
        size(80, 5),
    );
    h.wait_for(WAIT, |h| h.exited.is_some());
    assert_eq!(h.events.title, Some(Some("eitri-title".to_string())));
}

#[test]
fn an_exited_shell_keeps_its_screen_and_says_how_it_ended() {
    let mut h = Harness::start(spec("/bin/sh", &["-c", "echo last words; exit 3"]), size(80, 6));
    h.wait_for(WAIT, |h| h.exited.is_some());
    assert_eq!(h.exited.and_then(|e| e.code), Some(3));
    let text = h.text();
    assert!(text.iter().any(|l| l == "last words"), "{text:?}");
    let notice = "[process exited 3 \u{2014} Enter restarts]";
    let row = text.iter().position(|l| l == notice).expect("the notice");
    // The final frame carries its own cursor, just after the notice where `show_end` left it,
    // rather than omitting it or keeping the previous render's cursor.
    assert_eq!(
        h.cursor,
        Some(CursorCell {
            row: row as u16,
            col: notice.chars().count() as u16,
            visible: true
        })
    );
}

#[test]
fn shutdown_hangs_up_and_reaps() {
    let mut h = Harness::start(plain_sh(), size(80, 24));
    h.wait_for(WAIT, |h| h.text().first().is_some_and(|l| l.starts_with('$')));
    let pid = h.session.pid();
    let exit = h.session.shutdown_and_wait().expect("reaped");
    assert_eq!(exit.signal, Some(libc::SIGHUP));
    let mut status = 0;
    let rc = unsafe { libc::waitpid(pid as libc::pid_t, &mut status, libc::WNOHANG) };
    assert!(rc == -1, "no zombie left behind");
}

/// The byte tap sees every byte the child wrote, before parsing.
#[test]
fn the_tap_sees_the_raw_stream() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = seen.clone();
    let mut h = Harness::start_with(SessionConfig {
        spawn: spec("/bin/sh", &["-c", r"printf 'a\033[31mb'"]),
        size: size(80, 5),
        colors: TerminalColors::default(),
        focused: true,
        tap: Some(Box::new(move |bytes: &[u8]| {
            sink.lock().unwrap().extend_from_slice(bytes);
        })),
    });
    h.wait_for(WAIT, |h| h.exited.is_some());
    assert_eq!(seen.lock().unwrap().as_slice(), b"a\x1b[31mb");
}

/// A shell that ignores SIGHUP must not outlive a closed window: after the grace period it is
/// killed, by the pid captured at spawn. That pid is the shell's. A job the shell
/// runs in a process group of its own gets the ordinary hangup -- from the shell, and from the
/// kernel when the master closes -- and one that ignores it outlives the window, as it would
/// outlive foot or alacritty.
#[test]
fn a_shell_that_ignores_sighup_is_killed_after_the_grace_period() {
    let mut h = Harness::start(
        spec("/bin/sh", &["-c", "trap '' HUP; printf ready; exec sleep 100"]),
        size(80, 5),
    );
    h.wait_for(WAIT, |h| h.text().first().is_some_and(|l| l == "ready"));
    let exit = h.session.shutdown_and_wait().expect("reaped");
    assert_eq!(exit.signal, Some(libc::SIGKILL));
}

#[test]
fn a_large_input_is_delivered_whole_without_stalling() {
    let (first, second) = (ChildGate::new(), ChildGate::new());
    let script = format!(
        "stty raw -echo; printf ready; read x < {}; printf mid; read x < {}; head -c 200000 | wc -c",
        first.path(),
        second.path()
    );
    let mut h = Harness::start(spec("/bin/sh", &["-c", &script]), size(80, 5));
    h.wait_for(WAIT, |h| h.text().first().is_some_and(|l| l.starts_with("ready")));
    h.send(NormalizedInput::Paste {
        text: "x".repeat(200_000),
        bracketed: false,
    });
    // The child cannot drain the paste until the host has seen mid.
    first.release();
    h.wait_for(WAIT, |h| h.text().iter().any(|l| l.contains("mid")));
    assert!(h.session.outbox_high_water() > 0, "the paste waited in the outbox");
    second.release();
    h.wait_for(WAIT, |h| h.exited.is_some());
    assert!(h.text().iter().any(|l| l.contains("200000")), "{:?}", h.text());
}

/// Keys, resizes and colours sent after shell exit must be dropped without panic because
/// the pane can race the exit. A zero-sized resize is clamped to one cell before forwarding.
#[test]
fn commands_after_exit_and_a_zero_resize_are_harmless() {
    let mut h = Harness::start(plain_sh(), size(80, 24));
    h.wait_for(WAIT, |h| h.text().first().is_some_and(|l| l.starts_with('$')));
    h.session.send(SessionCommand::Resize(size(0, 0)));
    h.wait_for(WAIT, |h| h.frame.as_ref().is_some_and(|f| (f.cols, f.rows) == (1, 1)));
    h.session.send(SessionCommand::Resize(size(80, 24)));
    h.type_str("clear; stty size\n");
    h.wait_for(WAIT, |h| h.has_line("24 80"));
    h.type_str("exit\n");
    h.wait_for(WAIT, |h| h.exited.is_some());
    h.type_str("still typing\n");
    h.session.send(SessionCommand::Resize(size(100, 30)));
    h.session.send(SessionCommand::SetColors(TerminalColors::default()));
    // The thread already ended with the child's own exit, which is what joining it returns.
    assert_eq!(h.session.shutdown_and_wait().and_then(|e| e.code), Some(0));
}

/// A reset does not end an open update (`terminal-sync` counts a RIS as one more dispatch), so what
/// a `reset` draws after a killed nvim is published by the deadline too.
#[test]
fn a_reset_inside_an_open_update_is_published_after_the_deadline() {
    let mut h = Harness::start(
        spec("/bin/sh", &["-c", r"printf 'before\033[?2026h\033cafter'; read x"]),
        size(80, 5),
    );
    h.wait_for(Duration::from_secs(2), |h| h.has_line("after"));
}

/// A child that exits while a synchronized update is still open must not freeze the pane on the
/// stale pre-update frame: its own content and the exit notice must both reach the screen.
/// `Screen::render` returns the snapshot taken at the moment the update opened for as long as
/// `open_update()` is `Some`. Feeding the exit notice and rendering without closing the update
/// would leave the pane on `["before", "", ...]` despite `exited`, hiding both "inside" and the notice.
#[test]
fn an_exit_inside_an_open_synchronized_update_still_shows_its_content_and_the_notice() {
    let mut h = Harness::start(
        spec("/bin/sh", &["-c", r"printf 'before\r\n\033[?2026hinside'; exit 3"]),
        size(80, 6),
    );
    h.wait_for(WAIT, |h| h.exited.is_some());
    assert_eq!(h.exited.and_then(|e| e.code), Some(3));
    let text = h.text();
    assert!(text.iter().any(|l| l == "inside"), "{text:?}");
    assert!(
        text.iter().any(|l| l == "[process exited 3 \u{2014} Enter restarts]"),
        "{text:?}"
    );
}

/// A program that closes the terminal and keeps running (`exec nohup cmd` does this) is not an
/// exit: waiting for it would stop the thread hearing `Shutdown`. The pane is told it left and
/// still runs, and shutting down still hangs it up.
#[test]
#[cfg_attr(
    target_os = "macos",
    ignore = "macOS: the controller sees no EOF while a process keeps the tty as its controlling terminal, so leaving it is not seen"
)]
fn a_child_that_leaves_the_terminal_is_reported_and_still_shut_down() {
    let mut h = Harness::start(
        spec("/bin/sh", &["-c", "exec </dev/null >/dev/null 2>&1; exec sleep 30"]),
        size(80, 5),
    );
    h.wait_for(Duration::from_secs(2), |h| h.exited.is_some());
    let left = h.exited.expect("reported");
    assert!(
        left.detached && left.code.is_none() && left.signal.is_none(),
        "{left:?}"
    );
    assert!(h.text().iter().any(|l| l.contains("still running")), "{:?}", h.text());
    let exit = h.session.shutdown_and_wait().expect("reaped");
    assert_eq!(exit.signal, Some(libc::SIGHUP));
}

/// A child that left the terminal is still reaped when it ends on its own, without a `Shutdown`.
#[test]
#[cfg_attr(
    target_os = "macos",
    ignore = "macOS: the controller sees no EOF while a process keeps the tty as its controlling terminal, so leaving it is not seen"
)]
fn a_child_that_left_the_terminal_is_reaped_when_it_exits() {
    let gate = ChildGate::new();
    let script = format!("exec </dev/null >/dev/null 2>&1; read x < {}; exit 4", gate.path());
    let mut h = Harness::start(spec("/bin/sh", &["-c", &script]), size(80, 5));
    h.wait_for(Duration::from_secs(2), |h| h.exited.is_some());
    assert!(h.exited.is_some_and(|e| e.detached));
    gate.release();
    h.wait_for(WAIT, |h| h.exited.is_some_and(|e| e.code == Some(4)));
    assert_eq!(h.session.shutdown_and_wait().and_then(|e| e.code), Some(4));
}

/// The host must learn of a detached child's own exit too, not just get it back from
/// `shutdown_and_wait` when it happens to ask. Without this, the
/// pane keeps "[process left the terminal, still running ...]" under its screen forever about a
/// process that is gone, until something calls `shutdown_and_wait`.
#[test]
#[cfg_attr(
    target_os = "macos",
    ignore = "macOS: the controller sees no EOF while a process keeps the tty as its controlling terminal, so leaving it is not seen"
)]
fn a_detached_childs_own_exit_reaches_the_host() {
    let gate = ChildGate::new();
    let script = format!("exec </dev/null >/dev/null 2>&1; read x < {}; exit 4", gate.path());
    let mut h = Harness::start(spec("/bin/sh", &["-c", &script]), size(80, 5));
    h.wait_for(Duration::from_secs(2), |h| h.exited.is_some_and(|e| e.detached));
    gate.release();
    h.wait_for(WAIT, |h| h.exited.is_some_and(|e| !e.detached && e.code == Some(4)));
    assert!(
        h.text()
            .iter()
            .any(|l| l == "[process exited 4 \u{2014} Enter restarts]"),
        "{:?}",
        h.text()
    );
}

/// A panic on the session thread -- here a host's tap; in life the emulator on hostile input -- must
/// neither wedge the pane nor leave the child behind: the host hears `exited` (so Enter restarts),
/// and the child, which ignores SIGHUP, is killed and reaped by `PtyChild`'s `Drop`.
/// Run with `--nocapture`, the panic's own message is printed; that is expected.
#[cfg(target_os = "linux")]
#[test]
fn a_panic_on_the_session_thread_ends_the_session_and_reaps_the_child() {
    let (release, released) = std::sync::mpsc::channel();
    let mut h = Harness::start_with(SessionConfig {
        spawn: spec("/bin/sh", &["-c", "trap '' HUP; printf x; exec sleep 30"]),
        size: size(80, 5),
        colors: TerminalColors::default(),
        focused: true,
        tap: Some(Box::new(move |_: &[u8]| {
            released.recv_timeout(Duration::from_secs(60)).expect("release the tap");
            panic!("a tap that panics");
        })),
    });
    let identity = process_identity::ProcessIdentity::capture(h.session.pid());
    release.send(()).unwrap();
    h.wait_for(WAIT, |h| h.exited.is_some());
    assert!(
        h.text().iter().any(|l| l.contains("the terminal failed")),
        "{:?}",
        h.text()
    );
    let deadline = Instant::now() + Duration::from_secs(60);
    while !identity.is_reaped() {
        assert!(Instant::now() < deadline, "the child was not reaped after the panic");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// A program that asks questions and never reads the answers (`yes $'\e[6n'`) must not grow the
/// session's memory at its output rate: answers past `REPLY_CAP` are dropped.
#[test]
fn answers_to_a_program_that_never_reads_them_are_capped() {
    let script = r#"stty raw -echo; yes "$(printf '\033[6n')" | head -c 600000; printf done; exec sleep 30"#;
    let mut h = Harness::start(spec("/bin/sh", &["-c", script]), size(80, 5));
    h.wait_for(Duration::from_secs(60), |h| h.text().iter().any(|l| l.contains("done")));
    assert!(
        h.session.replies_dropped() > 0,
        "the cap was never reached: the test proves nothing"
    );
    assert!(
        h.session.outbox_high_water() <= REPLY_CAP,
        "{} bytes waited for the PTY",
        h.session.outbox_high_water()
    );
}

/// A hidden terminal whose program keeps printing renders nothing; shown again, it renders the
/// latest screen at once.
#[test]
fn a_hidden_terminal_renders_nothing_until_it_is_shown() {
    let script = "stty -echo; printf ready; read _; seq 1 2000; printf '\\033]0;consumed\\007'; read _";
    let mut h = Harness::start(spec("/bin/sh", &["-c", script]), size(40, 5));
    h.wait_for(WAIT, |h| h.text().first().is_some_and(|l| l.starts_with("ready")));
    let before = h.session.renders();
    h.session.send(SessionCommand::Visible(false));
    h.type_str("\n");
    let deadline = Instant::now() + Duration::from_secs(60);
    while h.session.take_update().events.title.flatten().as_deref() != Some("consumed") {
        assert!(Instant::now() < deadline, "the hidden session never consumed the flood");
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(h.session.renders(), before, "2000 lines went by while hidden");
    h.session.send(SessionCommand::Visible(true));
    h.wait_for(WAIT, |h| h.has_line("2000"));
}

/// A mouse report reaches the program only while its own mode asks for it, and a dropped report
/// (the mode having just gone off) never gets read as something else's byte. `ready` proves the
/// enabling escape sequence was parsed before anything is sent; `off` proves the disabling one was
/// too, before the second (dropped) report is sent; `next=x` proves the dropped report did not leave
/// a stray byte for the child's next read to pick up instead of the key that follows it.
#[test]
fn mouse_reports_reach_the_program_only_while_it_asks() {
    let script = r#"stty raw -echo; printf '\033[?1000;1006hready'; r=$(dd bs=1 count=9 2>/dev/null); printf '\033[?1000lgot=%s off' "$(printf %s "$r" | od -An -c | tr -s ' ')"; r=$(dd bs=1 count=1 2>/dev/null); printf ' next=%s' "$r""#;
    let mut h = Harness::start(spec("/bin/sh", &["-c", script]), size(80, 10));
    h.wait_for(WAIT, |h| h.text().iter().any(|l| l.contains("ready")));
    h.session.send(SessionCommand::Mouse(left_press(2, 4)));
    h.wait_for(WAIT, |h| h.text().iter().any(|l| l.contains("off")));
    // Dropped: the mode just went off, so this must reach nobody.
    h.session.send(SessionCommand::Mouse(left_press(2, 4)));
    h.type_str("x");
    h.wait_for(WAIT, |h| h.text().iter().any(|l| l.contains("next=x")));
    let text = h.text().join(" ");
    assert!(
        text.contains("033 [ < 0 ; 5 ; 3 M"),
        "the SGR press report; screen:\n{text}"
    );
}

/// Focus reporting (DECSET 1004) reaches the program through the same door, `CSI I` for gaining the
/// keys.
#[test]
fn a_focus_report_reaches_the_program_with_1004() {
    let script = r#"stty raw -echo; printf '\033[?1004hready'; r=$(dd bs=1 count=3 2>/dev/null); printf 'got=%s' "$(printf %s "$r" | od -An -c | tr -s ' ')""#;
    let mut h = Harness::start(spec("/bin/sh", &["-c", script]), size(80, 10));
    h.wait_for(WAIT, |h| h.text().iter().any(|l| l.contains("ready")));
    h.session.send(SessionCommand::FocusReport(true));
    h.wait_for(WAIT, |h| h.text().iter().any(|l| l.contains("got=")));
    let text = h.text().join(" ");
    assert!(text.contains("033 [ I"), "the CSI I focus-in report; screen:\n{text}");
}

/// `Update::mouse` is published (and the host wakes) the moment the live mode changes, not only
/// with the next frame -- what lets the host route a click correctly even mid-synchronized-update.
#[test]
fn mouse_modes_are_published_when_they_change() {
    let mut h = Harness::start(plain_sh(), size(80, 24));
    h.wait_for(WAIT, |h| h.text().first().is_some_and(|l| l.starts_with('$')));
    assert_eq!(h.mouse.map(|m| m.report), Some(false), "no mode asked for yet");
    h.type_str("printf '\\033[?1000h'\n");
    h.wait_for(WAIT, |h| h.mouse.is_some_and(|m| m.report));
    h.type_str("printf '\\033[?1000l'\n");
    h.wait_for(WAIT, |h| h.mouse.is_some_and(|m| !m.report));
}
