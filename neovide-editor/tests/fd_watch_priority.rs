//! F1 of the typing-latency plan review, held headless: a watch on an fd that stays readable (a
//! flood of nvim redraw batches keeps the harness's event loop fd readable) must not starve a
//! source at `GDK_PRIORITY_REDRAW` (120, what GDK paints at).
//!
//! Its own test binary, with a single test, because it owns the default main context for a while:
//! the library's unit tests assume nobody else does. No display is needed.

use std::cell::Cell;
use std::os::fd::RawFd;
use std::rc::Rc;
use std::time::{Duration, Instant};

use neovide_editor::fd_watch;

const GDK_PRIORITY_REDRAW: i32 = 120;

/// Runs the default context for `span` with an always-readable pipe watched at `priority`, and a
/// 5 ms timer at the paint priority; returns (paints, watch dispatches).
fn run(priority: i32, span: Duration) -> (u32, u32) {
    let mut fds = [0 as RawFd; 2];
    // SAFETY: a valid two-int array.
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
    // SAFETY: fds[1] is the write end just made; one byte is left unread for good.
    assert_eq!(unsafe { libc::write(fds[1], b"x".as_ptr().cast(), 1) }, 1);

    let dispatches = Rc::new(Cell::new(0u32));
    let paints = Rc::new(Cell::new(0u32));
    let watch = {
        let dispatches = dispatches.clone();
        fd_watch::add_local(fds[0], glib::IOCondition::IN, priority, move |_| {
            dispatches.set(dispatches.get() + 1);
            glib::ControlFlow::Continue
        })
        .expect("this thread owns the default context")
    };
    let paint = {
        let paints = paints.clone();
        glib::timeout_add_local_full(
            Duration::from_millis(5),
            glib::Priority::from(GDK_PRIORITY_REDRAW),
            move || {
                paints.set(paints.get() + 1);
                glib::ControlFlow::Continue
            },
        )
    };

    let context = glib::MainContext::default();
    let start = Instant::now();
    while start.elapsed() < span {
        // Never blocks for long: the watch is always ready when it is allowed to run.
        context.iteration(false);
        std::thread::sleep(Duration::from_micros(200));
    }

    // The contract: the source is removed before its fd closes.
    watch.remove();
    paint.remove();
    // SAFETY: both ends were opened above and are closed once.
    unsafe {
        libc::close(fds[0]);
        libc::close(fds[1]);
    }
    (paints.get(), dispatches.get())
}

#[test]
fn a_readable_fd_watch_below_the_paint_priority_never_starves_a_paint() {
    let context = glib::MainContext::default();
    let _owner = context
        .acquire()
        .expect("nobody else in this binary uses the default context");

    // The control: at the default priority the flood wins, which is Codex's measurement (200
    // dispatches, 0 paints) -- if this stops holding the probe below proves nothing.
    let (paints, dispatches) = run(glib::ffi::G_PRIORITY_DEFAULT, Duration::from_millis(200));
    assert!(
        paints < 3,
        "control: a watch at G_PRIORITY_DEFAULT should starve a priority-120 source ({paints} paints, {dispatches} dispatches)"
    );

    // The priority the pane's watch actually uses, not a copy of it.
    let (paints, dispatches) = run(fd_watch::EVENT_LOOP_WATCH_PRIORITY, Duration::from_millis(200));
    assert!(
        paints >= 10,
        "a watch at EVENT_LOOP_WATCH_PRIORITY must leave the paint priority its rate ({paints} paints, {dispatches} dispatches)"
    );
}
