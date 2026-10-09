//! What the session costs per keystroke and per frame, measured rather than argued.
//!
//! Release-only and `#[ignore]`d -- a debug build measures the debug build:
//!
//!     cargo test --release -p eitri-terminal --test perf -- --ignored --nocapture --test-threads=1

mod common;

use std::time::{Duration, Instant};

use common::{size, spec, Harness, WAIT};

fn median(mut v: Vec<Duration>) -> Duration {
    v.sort();
    v[v.len() / 2]
}

/// Echo latency after idle: a key typed into `cat` (the tty echoes it) until a frame that shows it
/// reaches the host, and how many frames the session rendered for it.
#[test]
#[ignore = "release-only measurement; see the module doc"]
fn echo_latency_after_idle() {
    let mut h = Harness::start(spec("/bin/sh", &["-c", "printf ready; exec cat"]), size(160, 50));
    h.wait_for(WAIT, |h| h.text().first().is_some_and(|l| l == "ready"));
    let mut latencies = Vec::new();
    let mut renders = Vec::new();
    for i in 1..=40usize {
        std::thread::sleep(Duration::from_millis(60));
        h.pending_wakes();
        let r0 = h.session.renders();
        let started = Instant::now();
        h.type_str("x");
        h.wait_for(WAIT, |h| h.text().first().is_some_and(|l| l.len() == 5 + i));
        latencies.push(started.elapsed());
        std::thread::sleep(Duration::from_millis(30));
        renders.push(h.session.renders() - r0);
    }
    let total: u64 = renders.iter().sum();
    eprintln!(
        "echo after idle: median {:?}, max {:?}; {total} renders for 40 keys",
        median(latencies.clone()),
        latencies.iter().max().unwrap()
    );
}

use eitri_terminal::{PtySize, Screen, ScrollRequest, SelectCommand, SelectKind, TerminalColors};

const WIDE: PtySize = PtySize {
    cols: 160,
    rows: 50,
    cell_width_px: 9,
    cell_height_px: 18,
};

/// A screen whose 10,000-line history is full of 160-column lines.
fn full_history() -> Screen {
    let mut screen = Screen::new(WIDE, TerminalColors::default());
    let mut chunk = Vec::new();
    for i in 0..10_100 {
        chunk.extend_from_slice(format!("{i:06} {}\r\n", "abcdefghij".repeat(15)).as_bytes());
        if chunk.len() > 60_000 {
            screen.feed(&chunk);
            chunk.clear();
        }
    }
    screen.feed(&chunk);
    screen
}

/// This thread's CPU time: unlike the wall clock, it does not count time the scheduler gave to
/// the other work this machine runs.
fn thread_cpu() -> Duration {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: `ts` is a valid timespec for the call to fill.
    assert_eq!(
        unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) },
        0
    );
    Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32)
}

/// The fastest of five batches of `n / 5` calls, per call, in thread CPU time: the machine runs
/// other work, and the minimum is the number least disturbed by it.
fn per_call(n: u32, mut f: impl FnMut(u32)) -> Duration {
    let batch = (n / 5).max(1);
    (0..5)
        .map(|b| {
            let started = thread_cpu();
            for i in 0..batch {
                f(b * batch + i);
            }
            (thread_cpu() - started) / batch
        })
        .min()
        .unwrap()
}

/// One PTY read's worth of output while the view is scrolled back over a full history: what
/// `Screen::feed` costs (parse plus re-aligning the pinned anchor), and what a render costs.
#[test]
#[ignore = "release-only measurement; see the module doc"]
fn pinned_streaming() {
    let mut screen = full_history();
    let follow_feed = per_call(200, |i| screen.feed(format!("tail {i}\r\n").as_bytes()));
    let follow_render = per_call(200, |_| drop(screen.render(true)));
    screen.scroll(ScrollRequest::Lines(500));
    assert!(screen.scrolled());
    let pinned_feed = per_call(200, |i| screen.feed(format!("more {i}\r\n").as_bytes()));
    assert!(screen.scrolled(), "still pinned");
    let pinned_render = per_call(200, |_| drop(screen.render(true)));
    eprintln!(
        "160x50, 10k history: following feed {follow_feed:?} render {follow_render:?}; \
         pinned feed {pinned_feed:?} render {pinned_render:?}"
    );
}

/// Repeated empty synchronized updates (`ESC[?2026h ESC[?2026l`), the redraw nvim does when nothing
/// changed, on a full screen: the cost per pair.
#[test]
#[ignore = "release-only measurement; see the module doc"]
fn empty_sync_pairs() {
    let mut screen = full_history();
    let pair = per_call(1000, |_| screen.feed(b"\x1b[?2026h\x1b[?2026l"));
    let mut many = Vec::new();
    for _ in 0..1000 {
        many.extend_from_slice(b"\x1b[?2026h\x1b[?2026l");
    }
    let started = thread_cpu();
    screen.feed(&many);
    eprintln!(
        "160x50: one empty BSU/ESU pair per read {pair:?}; 1000 pairs in one read {:?}",
        thread_cpu() - started
    );
}

/// `build_paint_list` with the whole screen selected, against none.
#[test]
#[ignore = "release-only measurement; see the module doc"]
fn render_with_a_whole_screen_selection() {
    let mut screen = full_history();
    let plain = per_call(200, |_| drop(screen.render(true)));
    screen.select(SelectCommand::Start {
        line: 0,
        col: 0,
        right_half: false,
        kind: SelectKind::Simple,
    });
    screen.select(SelectCommand::Extend {
        line: 49,
        col: 159,
        right_half: true,
    });
    let selected = per_call(200, |_| drop(screen.render(true)));
    eprintln!("160x50 render: no selection {plain:?}, whole screen selected {selected:?}");
}

/// Painting a dense 160x50 frame onto a CPU raster with the product's own `paint`, and on its own the
/// part of that which builds a `TextBlob` per glyph -- the part that runs on the CPU whatever
/// surface GTK hands the painter.
#[test]
#[ignore = "release-only measurement; see the module doc"]
fn paint_a_dense_frame() {
    use eitri_terminal::{paint, TerminalMetrics};
    use skia_safe::{surfaces, ISize, TextBlob};
    use terminal_render::PaintOp;

    let mut screen = full_history();
    let list = screen.render(true);
    let metrics = TerminalMetrics::with_font("monospace", 13.0, 160.0 * 8.0, 50.0 * 17.0, 1.0);
    let width = (list.cols as f32 * metrics.cell_width()).ceil() as i32;
    let height = (list.rows as f32 * metrics.cell_height()).ceil() as i32;
    let mut surface = surfaces::raster_n32_premul(ISize::new(width, height)).expect("raster");
    let fills = list
        .ops
        .iter()
        .filter(|op| matches!(op, PaintOp::FillCells { .. }))
        .count();
    let texts: Vec<&str> = list
        .ops
        .iter()
        .filter_map(|op| match op {
            PaintOp::DrawText { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    let whole = per_call(100, |_| paint(surface.canvas(), &list, &metrics));
    let blobs = per_call(100, |_| {
        for text in &texts {
            let font = metrics.font_for(text.chars().next().unwrap_or(' '), false, false);
            drop(TextBlob::new(text, &font));
        }
    });
    eprintln!(
        "160x50 dense frame, {fills} fills and {} glyphs: paint {whole:?}; building the glyphs' blobs alone {blobs:?}",
        texts.len()
    );
}

/// A visible terminal whose program changes only its title (a shell prompt's, a progress counter in
/// it) or rings the bell, 100 times 5 ms apart: how many frames the session renders for nothing on
/// screen changing, and how much CPU the session thread spends.
#[test]
#[ignore = "release-only measurement; see the module doc"]
fn title_and_bell_streams() {
    for (what, script) in [
        (
            "title",
            "printf ready; i=0; while [ $i -lt 100 ]; do printf '\\033]2;t%d\\007' $i; i=$((i+1)); sleep 0.005; done; printf done",
        ),
        (
            "bell",
            "printf ready; i=0; while [ $i -lt 100 ]; do printf '\\a'; i=$((i+1)); sleep 0.005; done; printf done",
        ),
    ] {
        let mut h = Harness::start(spec("/bin/sh", &["-c", script]), size(160, 50));
        h.wait_for(WAIT, |h| h.text().first().is_some_and(|l| l.starts_with("ready")));
        let r0 = h.session.renders();
        h.wait_for(Duration::from_secs(10), |h| h.text().first().is_some_and(|l| l.ends_with("done")));
        eprintln!(
            "{what} stream, 100 events 5 ms apart at 160x50: {} renders (one of them the final \"done\")",
            h.session.renders() - r0
        );
    }
}

/// What starting a terminal costs the thread that asks (the GTK thread, in the product): the passwd
/// lookup a stale `$SHELL` falls back to, and `TerminalSession::spawn` up to its return (openpty,
/// fork, and the exec handshake std waits for), in wall time.
#[test]
#[ignore = "release-only measurement; see the module doc"]
fn starting_a_terminal() {
    use eitri_terminal::pty::passwd_shell;
    use eitri_terminal::{SessionConfig, TerminalColors, TerminalSession};
    let lookups: Vec<Duration> = (0..20)
        .map(|_| {
            let t = Instant::now();
            let _ = passwd_shell();
            t.elapsed()
        })
        .collect();
    let spawns: Vec<Duration> = (0..20)
        .map(|_| {
            let t = Instant::now();
            let session = TerminalSession::spawn(
                SessionConfig {
                    spawn: spec("/bin/sh", &["-c", "exec sleep 5"]),
                    size: size(160, 50),
                    colors: TerminalColors::default(),
                    focused: true,
                    tap: None,
                },
                || {},
            )
            .expect("spawn");
            let elapsed = t.elapsed();
            drop(session);
            elapsed
        })
        .collect();
    eprintln!(
        "passwd lookup: median {:?}, max {:?}; TerminalSession::spawn: median {:?}, max {:?}",
        median(lookups.clone()),
        lookups.iter().max().unwrap(),
        median(spawns.clone()),
        spawns.iter().max().unwrap()
    );
}
