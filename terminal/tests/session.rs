//! The session thread, end to end: bytes from a real child, through the `Term`, to a `PaintList`;
//! keys the other way; the answers a program waits for; and what idle costs.

mod common;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::{ctrl, plain_sh, size, spec, Harness, WAIT};
use eitri_terminal::mouse::{Button, MouseInput, MouseKind, MouseMods};
use eitri_terminal::pty::HANGUP_GRACE;
use eitri_terminal::session::SYNC_UPDATE_TIMEOUT;
use eitri_terminal::{CursorCell, SessionCommand, SessionConfig, TerminalColors, REPLY_CAP};
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

/// `Ctrl+a Ctrl+a` hands the program a literal Ctrl+a (spec, keys): 0x01 through the encoder, not a
/// raw byte, and it arrives -- `cat -v` shows it.
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

/// A program that asks the terminal where the cursor is must get an answer: the frozen driver's
/// `VoidListener` dropped it and the program waited out its timeout.
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

/// OSC 52: a program may put text on the clipboard (the event reaches the host), and may never
/// read it (nothing is written back).
#[test]
fn osc52_copy_reaches_the_host_and_osc52_paste_is_refused() {
    let script = r#"printf '\033]52;c;aGk=\007'; stty -icanon -echo min 0 time 10; printf '\033]52;c;?\007'; r=$(dd bs=1 count=1 2>/dev/null); printf 'load=[%s]\n' "$r""#;
    let mut h = Harness::start(spec("/bin/sh", &["-c", script]), size(80, 10));
    h.wait_for(WAIT, |h| h.exited.is_some());
    assert_eq!(h.events.clipboard.as_deref(), Some("hi"));
    assert!(h.has_line("load=[]"), "{:?}", h.text());
}

/// Bottom-terminal phase 2: an OSC 52 copy to the selection (`s`, nvim's `"*`) reaches the host as
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
/// clipboard at once, hidden or not). Bottom-terminal phase 2: a copy to the selection too. Before
/// it, `publish_events` rang only for `clipboard`, and a hidden terminal's `"*y` waited, unseen, for
/// the next show.
#[test]
fn a_hidden_selection_copy_rings_the_host_like_a_clipboard_copy() {
    let script = r#"stty -echo; printf ready; read _; printf '\033]52;s;aGk=\007'; exec sleep 5"#;
    let mut h = Harness::start(spec("/bin/sh", &["-c", script]), size(40, 5));
    h.wait_for(WAIT, |h| h.text().first().is_some_and(|l| l.starts_with("ready")));
    h.session.send(SessionCommand::Visible(false));
    std::thread::sleep(Duration::from_millis(50));
    h.wait_for(WAIT, |_| true);
    h.pending_wakes();
    h.type_str("\n");
    std::thread::sleep(Duration::from_millis(400));
    assert!(
        h.pending_wakes() > 0,
        "a hidden terminal's selection copy must ring the host"
    );
    h.wait_for(WAIT, |h| h.events.primary.is_some());
    assert_eq!(h.events.primary.as_deref(), Some("hi"));
}

/// Bottom-terminal phase 2: every frame comes with the cell the cursor is in, even when the program
/// hid it and the frame paints no cursor -- the host draws an input method's preedit there.
#[test]
fn every_frame_says_where_the_cursor_is_even_when_it_is_hidden() {
    let script = r#"printf 'ab\033[?25l'; exec sleep 5"#;
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

/// After the initial frames, a child that prints nothing causes no render, host wake-up, or
/// session-thread wake-up. The thread counter catches a timed `poll` that would keep waking
/// without rendering.
#[test]
fn idle_renders_nothing() {
    let mut h = Harness::start(spec("/bin/sh", &["-c", "printf ready; exec sleep 5"]), size(80, 5));
    h.wait_for(WAIT, |h| h.text().first().is_some_and(|l| l == "ready"));
    std::thread::sleep(Duration::from_millis(100));
    // Drain wake-ups received on the way to "ready" so the idle count starts at zero.
    // `Harness::wakes` only grows inside `wait_for`, so comparing that field without
    // receiving more wake-ups would miss notifications sent during the idle interval.
    h.pending_wakes();
    // Reopen the wake-pending gate as well as draining the channel. A closed gate would hide
    // repeated publication during the idle interval and let `pending_wakes() == 0` pass.
    // `wait_for` with an always-true predicate calls `take_update` once without blocking,
    // so the gate starts open.
    h.wait_for(WAIT, |_| true);
    let (renders, turns) = (h.session.renders(), h.session.thread_wakeups());
    std::thread::sleep(Duration::from_secs(1));
    assert_eq!(h.session.renders(), renders, "a render with nothing new");
    assert_eq!(h.pending_wakes(), 0, "no host wake-up while idle");
    assert_eq!(
        h.session.thread_wakeups(),
        turns,
        "the session thread woke with nothing to do"
    );
}

/// A noisy stream is folded into about one render per frame, not one per read.
#[test]
fn a_flood_is_coalesced_to_about_one_render_per_frame() {
    let started = std::time::Instant::now();
    let mut h = Harness::start(spec("/bin/sh", &["-c", "yes | head -n 200000"]), size(120, 30));
    h.wait_for(Duration::from_secs(30), |h| h.exited.is_some());
    let elapsed = started.elapsed();
    let frames = elapsed.as_millis() as u64 / 16 + 3;
    assert!(
        h.session.renders() <= frames,
        "{} renders in {elapsed:?}",
        h.session.renders()
    );
    assert!(h.text().iter().any(|l| l == "y"), "{:?}", h.text());
}

/// A bell on most reads must ride the render clock like a pixel
/// change does, not ring the host once per loop turn. Before the fix, `publish_events` rang
/// unconditionally on every read that saw an event, which for a bell-heavy stream is far more often
/// than once per frame (measured: 1118 host wakes/s against 58 renders/s for the same volume).
///
/// A byte-volume flood (`yes | head -c N`, as the render-coalescing test above uses) turned out not
/// to pin this: how many separate read turns 4 MB of bells takes depends on how fast THIS build
/// parses -- a slow debug build lets the pty buffer refill between reads and coalesces turns by
/// accident, which is what made an earlier version of this test pass unmodified against the unfixed
/// code (13 wake-ups over 2s, nowhere near a per-turn rate). Forty bells explicitly spaced 5ms
/// apart are forty genuinely separate turns regardless of build speed, so this reproduces the bug's
/// shape without depending on it.
#[test]
fn a_bell_flood_is_coalesced_to_about_one_host_wake_per_frame() {
    let started = std::time::Instant::now();
    let script = "i=0; while [ $i -lt 40 ]; do printf '\\a'; i=$((i+1)); sleep 0.005; done";
    let mut h = Harness::start(spec("/bin/sh", &["-c", script]), size(80, 5));
    h.wait_for(Duration::from_secs(10), |h| h.exited.is_some());
    let elapsed = started.elapsed();
    // Generous: the leading edge plus one wake per frame interval, with slack for scheduling --
    // the same shape of bound as `a_flood_is_coalesced_to_about_one_render_per_frame` above, but on
    // host wakes rather than renders. 40, the bell count, is what a broken per-turn ring would hit.
    let frames = elapsed.as_millis() as usize / 16 + 5;
    assert!(
        h.wakes <= frames,
        "{} host wake-ups for 40 spaced bells in {elapsed:?}",
        h.wakes
    );
    assert!(h.events.bell, "the bell itself must still be reported");
}

/// A hidden terminal must not wake the host for bells. Hidden events should cause no host work,
/// just like hidden frames.
#[test]
fn a_hidden_bell_flood_rings_the_host_not_at_all() {
    let mut h = Harness::start(
        spec("/bin/sh", &["-c", "printf ready; sleep 0.1; exec yes '\u{7}'"]),
        size(80, 5),
    );
    h.wait_for(WAIT, |h| h.text().first().is_some_and(|l| l == "ready"));
    h.session.send(SessionCommand::Visible(false));
    // Drains anything already in flight (there should be none) before the hidden window starts.
    h.pending_wakes();
    std::thread::sleep(Duration::from_millis(400));
    assert_eq!(h.pending_wakes(), 0, "a bell must not ring a hidden pane");
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
    // The final frame carries the cursor placed by `show_end` just after the exit notice,
    // so the host has cursor coordinates for that frame.
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
/// outlive foot or alacritty; the GUI checklist records it.
#[test]
fn a_shell_that_ignores_sighup_is_killed_after_the_grace_period() {
    let mut h = Harness::start(
        spec("/bin/sh", &["-c", "trap '' HUP; printf ready; exec sleep 100"]),
        size(80, 5),
    );
    h.wait_for(WAIT, |h| h.text().first().is_some_and(|l| l == "ready"));
    let started = std::time::Instant::now();
    let exit = h.session.shutdown_and_wait().expect("reaped");
    assert_eq!(exit.signal, Some(libc::SIGKILL));
    assert!(started.elapsed() < Duration::from_secs(2), "{:?}", started.elapsed());
}

/// A paste larger than the tty buffer must wait in the outbox for POLLOUT without blocking
/// output reads or losing bytes. Send it after the child enters raw mode, since cooked mode's
/// line discipline discards input past 4095 bytes.
///
/// The child prints "mid" before reading the paste. Receiving "mid" while the paste is still
/// queued proves that reading and rendering continue during a pending write. Checking only
/// the final byte count would also pass if `write` blocked until the child began reading.
#[test]
fn a_large_input_is_delivered_whole_without_stalling() {
    let script = "stty raw -echo; printf ready; sleep 0.5; printf mid; sleep 0.5; head -c 200000 | wc -c; stty sane";
    let mut h = Harness::start(spec("/bin/sh", &["-c", script]), size(80, 5));
    h.wait_for(WAIT, |h| h.text().first().is_some_and(|l| l.starts_with("ready")));
    h.send(NormalizedInput::Paste {
        text: "x".repeat(200_000),
        bracketed: false,
    });
    h.wait_for(Duration::from_millis(900), |h| {
        h.text().iter().any(|l| l.contains("mid"))
    });
    h.wait_for(Duration::from_secs(10), |h| h.exited.is_some());
    assert!(h.text().iter().any(|l| l.contains("200000")), "{:?}", h.text());
}

/// Keys, resizes and colours sent after the shell has exited -- the pane can race the exit -- are
/// dropped without a panic, and a zero-sized resize is clamped to one cell rather than passed on
///.
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

/// An unterminated synchronized update holds publication only until its deadline. This covers a
/// program killed mid-frame, `printf '\e[?2026h'`, or replayed output lacking an end marker.
#[test]
fn an_unterminated_synchronized_update_is_published_after_the_deadline() {
    let mut h = Harness::start(
        spec(
            "/bin/sh",
            &["-c", r"printf '\033[?2026h'; printf visible; exec sleep 30"],
        ),
        size(80, 5),
    );
    let started = Instant::now();
    h.wait_for(Duration::from_secs(2), |h| h.has_line("visible"));
    assert!(h.exited.is_none(), "published by the deadline, not by the exit");
    assert!(
        started.elapsed() >= SYNC_UPDATE_TIMEOUT / 2,
        "{:?}: too soon for the deadline",
        started.elapsed()
    );
}

/// A reset does not end an open update (`terminal-sync` counts a RIS as one more dispatch), so what
/// a `reset` draws after a killed nvim is published by the deadline too.
#[test]
fn a_reset_inside_an_open_update_is_published_after_the_deadline() {
    let mut h = Harness::start(
        spec(
            "/bin/sh",
            &["-c", r"printf 'before\033[?2026h\033cafter'; exec sleep 30"],
        ),
        size(80, 5),
    );
    h.wait_for(Duration::from_secs(2), |h| h.has_line("after"));
}

/// A child exiting during a synchronized update must publish both its content and the exit
/// notice. `Screen::render` uses the snapshot from the update's start while `open_update()` is
/// `Some`, so the exit path must close the update before rendering. Otherwise the pane remains
/// on the pre-update frame even after `exited` is set.
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

/// A program that closes its terminal while still running has detached rather than exited.
/// Waiting for it would prevent the thread from receiving `Shutdown`. Report the detachment to
/// the pane and still hang up the child on shutdown.
#[test]
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
    let started = Instant::now();
    let exit = h.session.shutdown_and_wait().expect("reaped");
    assert_eq!(exit.signal, Some(libc::SIGHUP));
    assert!(started.elapsed() < HANGUP_GRACE, "{:?}", started.elapsed());
}

/// A child that left the terminal is still reaped when it ends on its own, without a `Shutdown`.
#[test]
fn a_child_that_left_the_terminal_is_reaped_when_it_exits() {
    let mut h = Harness::start(
        spec("/bin/sh", &["-c", "exec </dev/null >/dev/null 2>&1; sleep 0.6; exit 4"]),
        size(80, 5),
    );
    h.wait_for(Duration::from_secs(2), |h| h.exited.is_some());
    assert!(h.exited.is_some_and(|e| e.detached));
    std::thread::sleep(Duration::from_millis(1500));
    let started = Instant::now();
    assert_eq!(h.session.shutdown_and_wait().and_then(|e| e.code), Some(4));
    assert!(
        started.elapsed() < Duration::from_millis(100),
        "the thread had already ended"
    );
}

/// The host must learn of a detached child's own exit too, not just get it back from
/// `shutdown_and_wait` when it happens to ask. Without this, the
/// pane keeps "[process left the terminal, still running ...]" under its screen forever about a
/// process that is gone, until something calls `shutdown_and_wait`.
#[test]
fn a_detached_childs_own_exit_reaches_the_host() {
    let mut h = Harness::start(
        // Keep the child alive for 0.6s after closing its terminal. A 0.3s delay leaves only about
        // 100ms beyond `EXIT_AFTER_EOF` (200ms); late scheduling could then report the real exit
        // before `detached`, causing the first `wait_for` to time out.
        spec("/bin/sh", &["-c", "exec </dev/null >/dev/null 2>&1; sleep 0.6; exit 4"]),
        size(80, 5),
    );
    h.wait_for(Duration::from_secs(2), |h| h.exited.is_some_and(|e| e.detached));
    h.wait_for(Duration::from_secs(2), |h| {
        h.exited.is_some_and(|e| !e.detached && e.code == Some(4))
    });
    assert!(
        h.text()
            .iter()
            .any(|l| l == "[process exited 4 \u{2014} Enter restarts]"),
        "{:?}",
        h.text()
    );
}

/// A session-thread panic must notify the host through `exited` so Enter can restart the pane.
/// `PtyChild`'s `Drop` must kill and reap the child even if it ignores SIGHUP. With
/// `--nocapture`, the test's panic message is expected.
#[cfg(target_os = "linux")]
#[test]
fn a_panic_on_the_session_thread_ends_the_session_and_reaps_the_child() {
    let mut h = Harness::start_with(SessionConfig {
        spawn: spec("/bin/sh", &["-c", "trap '' HUP; printf x; exec sleep 30"]),
        size: size(80, 5),
        colors: TerminalColors::default(),
        focused: true,
        tap: Some(Box::new(|_: &[u8]| panic!("a tap that panics"))),
    });
    let pid = h.session.pid();
    h.wait_for(WAIT, |h| h.exited.is_some());
    assert!(
        h.text().iter().any(|l| l.contains("the terminal failed")),
        "{:?}",
        h.text()
    );
    let deadline = Instant::now() + Duration::from_secs(2);
    while std::path::Path::new(&format!("/proc/{pid}")).exists() {
        assert!(Instant::now() < deadline, "pid {pid} still exists 2 s after the panic");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// A program that asks questions without reading replies (`yes $'\e[6n'`) must not grow session
/// memory at its output rate. Drop replies beyond `REPLY_CAP`.
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

/// A hidden terminal renders nothing while its program prints. When shown again, it renders the
/// latest screen immediately.
#[test]
fn a_hidden_terminal_renders_nothing_until_it_is_shown() {
    let script = "stty -echo; printf ready; read _; seq 1 2000; exec sleep 30";
    let mut h = Harness::start(spec("/bin/sh", &["-c", script]), size(40, 5));
    h.wait_for(WAIT, |h| h.text().first().is_some_and(|l| l.starts_with("ready")));
    h.session.send(SessionCommand::Visible(false));
    std::thread::sleep(Duration::from_millis(50));
    let before = h.session.renders();
    h.type_str("\n");
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(h.session.renders(), before, "2000 lines went by while hidden");
    h.session.send(SessionCommand::Visible(true));
    h.wait_for(WAIT, |h| h.has_line("2000"));
}

/// A key the program does not echo changes nothing on screen, so it renders nothing. A render for
/// an unchanged screen is not only wasted work: it takes the render clock's leading edge, and the
/// echo of the next key that does change something then waits out the rest of the frame (16 ms
/// after idle, measured, before keys at the live bottom stopped marking the screen dirty).
#[test]
fn a_key_that_changes_nothing_on_screen_renders_nothing() {
    let mut h = Harness::start(
        spec("/bin/sh", &["-c", "stty -echo; printf ready; exec cat"]),
        size(40, 5),
    );
    h.wait_for(WAIT, |h| h.text().first().is_some_and(|l| l == "ready"));
    std::thread::sleep(Duration::from_millis(50));
    let before = h.session.renders();
    h.type_str("abc");
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(h.session.renders(), before, "keys with no echo rendered a frame");
}

/// A program that changes only its title, or rings the bell, changes nothing a frame shows: the
/// events reach the host, riding the render clock as before, and no frame is built for them (34
/// frames for 100 titles 5 ms apart before, measured).
#[test]
fn titles_and_bells_reach_the_host_without_a_frame() {
    let script = "printf ready; read _; i=0; while [ $i -lt 20 ]; do printf '\\033]2;t%d\\007\\a' $i; i=$((i+1)); sleep 0.005; done; exec sleep 5";
    let mut h = Harness::start(spec("/bin/sh", &["-c", script]), size(40, 5));
    h.wait_for(WAIT, |h| h.text().first().is_some_and(|l| l == "ready"));
    std::thread::sleep(Duration::from_millis(50));
    h.wait_for(WAIT, |_| true);
    let before = h.session.renders();
    // The tty echoes the Enter that starts the loop: one frame for that.
    h.type_str("\n");
    h.wait_for(WAIT, |h| h.events.title == Some(Some("t19".to_string())));
    assert!(h.events.bell, "the bells reached the host");
    assert!(
        h.session.renders() <= before + 1,
        "{} frames for titles and bells",
        h.session.renders() - before
    );
}

/// A mouse report reaches the program only while its own mode asks for it, and a dropped report
/// (the mode having just gone off) never gets read as something else's byte (bottom-terminal phase
/// 3c). `ready` proves the enabling escape sequence was parsed before anything is sent; `off` proves
/// the disabling one was too, before the second (dropped) report is sent; `next=x` proves the
/// dropped report did not leave a stray byte for the child's next read to pick up instead of the key
/// that follows it.
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
