//! Held headless: a watch on an fd that stays readable (a flood of nvim redraw batches keeps the
//! editors' shared event-loop fd readable) must not starve a source at `GDK_PRIORITY_REDRAW` (120,
//! what GDK paints at). The watch measured is one made by the function the editors' one event-loop
//! watch is made with, and its priority is read back from the source itself.
//!
//! Its own test binary, with a single test, because it owns the default main context for a while:
//! the library's unit tests assume nobody else does. No display is needed.

use std::cell::Cell;
use std::os::fd::RawFd;
use std::rc::Rc;
use std::time::{Duration, Instant};

use neovide_editor::fd_watch;

const GDK_PRIORITY_REDRAW: i32 = 120;

type Wake = Box<dyn FnMut(glib::IOCondition) -> glib::ControlFlow>;

/// Runs the default context for `span` with an always-readable pipe watched by the source `add`
/// makes, and a 5 ms timer at the paint priority; returns (paints, watch dispatches, the watch's
/// priority as the context holds it).
fn run(add: impl FnOnce(RawFd, Wake) -> glib::SourceId, span: Duration) -> (u32, u32, Option<i32>) {
    let mut fds = [0 as RawFd; 2];
    // SAFETY: a valid two-int array.
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
    // SAFETY: fds[1] is the write end just made; one byte is left unread for good.
    assert_eq!(unsafe { libc::write(fds[1], b"x".as_ptr().cast(), 1) }, 1);

    let dispatches = Rc::new(Cell::new(0u32));
    let paints = Rc::new(Cell::new(0u32));
    let watch = {
        let dispatches = dispatches.clone();
        add(
            fds[0],
            Box::new(move |_| {
                dispatches.set(dispatches.get() + 1);
                glib::ControlFlow::Continue
            }),
        )
    };
    let priority = fd_watch::source_priority(&watch);
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
    (paints.get(), dispatches.get(), priority)
}

fn at(priority: i32) -> impl FnOnce(RawFd, Wake) -> glib::SourceId {
    move |fd, wake| {
        fd_watch::add_local(fd, glib::IOCondition::IN, priority, wake).expect("this thread owns the default context")
    }
}

#[test]
fn a_readable_fd_watch_below_the_paint_priority_never_starves_a_paint() {
    let context = glib::MainContext::default();
    let _owner = context
        .acquire()
        .expect("nobody else in this binary uses the default context");

    // The control: at the default priority the flood wins, which is Codex's measurement (200
    // dispatches, 0 paints) -- if this stops holding the probe below proves nothing.
    let (paints, dispatches, _) = run(at(glib::ffi::G_PRIORITY_DEFAULT), Duration::from_millis(200));
    assert!(
        paints < 3,
        "control: a watch at G_PRIORITY_DEFAULT should starve a priority-120 source ({paints} paints, {dispatches} dispatches)"
    );

    // The watch the editors' event loop gets, made by the same function, its priority read back.
    let (paints, dispatches, priority) = run(
        |fd, wake| fd_watch::add_event_loop_watch(fd, wake).expect("this thread owns the default context"),
        Duration::from_millis(200),
    );
    assert_eq!(
        priority,
        Some(glib::ffi::G_PRIORITY_DEFAULT_IDLE),
        "the event-loop watch runs at G_PRIORITY_DEFAULT_IDLE (200), below the paint priority"
    );
    assert!(
        paints >= 10,
        "the event-loop watch must leave the paint priority its rate ({paints} paints, {dispatches} dispatches)"
    );
}
