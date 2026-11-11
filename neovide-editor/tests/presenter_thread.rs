//! The presentation thread on a connection that another reader uses too, the way GDK reads its
//! own: a raw libwayland `wl_display` over a fake server (`support/fake_wl_server.rs`), read on this
//! process's main thread with `prepare_read`, `poll`, `read_events` or `cancel_read`, and
//! `dispatch_pending` on the default queue, while the thread under test serves its private queue on
//! the same display. Events reach the test's sinks through the thread's mailbox on the default GLib
//! main context, which the main thread owns. No display server is involved.
//!
//! A plain `main`, not libtest: each case runs in a fresh child copy of this binary, so an abort
//! there (libwayland aborts on an event for a destroyed queue) fails that case alone, and its stderr,
//! where libwayland and the thread write their warnings, is checked by the parent.

#[path = "support/fake_wl_server.rs"]
mod fake_wl_server;

use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use std::io::Read;
use std::os::raw::c_int;
use std::process::{Command, Stdio};
use std::rc::{Rc, Weak};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use fake_wl_server::{FakeServer, Feedback, Global};
use neovide_editor::subsurface_testing::{PaneId, PresentEvent, PresentSink, PresenterThread, TestCommand};
use wayland_sys::client::{wayland_client_handle, wl_display, wl_proxy};
use wayland_sys::common::{wl_argument, wl_interface, wl_message};
use wayland_sys::ffi_dispatch;

const CASE_ENV: &str = "EITRI_PRESENTER_THREAD_CASE";

/// Lines that libwayland writes when a queue is freed under a proxy still attached to it.
const QUEUE_WARNINGS: &[&str] = &["destroyed queue", "still attached"];

const LATE_STOP: &str = "[editor] subsurface thread did not stop within 100 ms";

// ---------------------------------------------------------------------------------------------
// The GDK stand-in: libwayland's own reader protocol on the default queue.

/// Static FFI data shared across threads only as read-only pointers.
struct Ffi<T>(T);
// SAFETY: the wrapped values are immutable statics; libwayland only reads them.
unsafe impl<T> Sync for Ffi<T> {}

static NO_TYPES: Ffi<[*const wl_interface; 1]> = Ffi([std::ptr::null()]);
static CALLBACK_EVENTS: Ffi<[wl_message; 1]> = Ffi([wl_message {
    name: c"done".as_ptr(),
    signature: c"u".as_ptr(),
    types: NO_TYPES.0.as_ptr(),
}]);
static CALLBACK_INTERFACE: Ffi<wl_interface> = Ffi(wl_interface {
    name: c"wl_callback".as_ptr(),
    version: 1,
    request_count: 0,
    requests: std::ptr::null(),
    event_count: 1,
    events: CALLBACK_EVENTS.0.as_ptr(),
});

unsafe extern "C" fn stand_in_done(data: *mut c_void, callback: *mut wl_proxy, _serial: u32) {
    // SAFETY: `data` is the stand-in's boxed counter, alive for the process (the stand-in is never
    // freed while its display can dispatch).
    let count = unsafe { &*(data as *const Cell<u64>) };
    count.set(count.get() + 1);
    // SAFETY: the callback is ours and gets no further event after `done`.
    unsafe { ffi_dispatch!(wayland_client_handle(), wl_proxy_destroy, callback) };
}

static CALLBACK_LISTENER: Ffi<[unsafe extern "C" fn(*mut c_void, *mut wl_proxy, u32); 1]> = Ffi([stand_in_done]);

struct StandIn {
    display: *mut wl_display,
    fd: c_int,
    done: Box<Cell<u64>>,
}

impl StandIn {
    fn connect(fd: c_int) -> Self {
        // SAFETY: `fd` is a connected socket whose ownership passes to libwayland.
        let display = unsafe { ffi_dispatch!(wayland_client_handle(), wl_display_connect_to_fd, fd) };
        assert!(!display.is_null(), "libwayland could not take the socket");
        // SAFETY: a live display.
        let fd = unsafe { ffi_dispatch!(wayland_client_handle(), wl_display_get_fd, display) };
        Self {
            display,
            fd,
            done: Box::new(Cell::new(0)),
        }
    }

    /// A `wl_display.sync` on the default queue, counted when its `done` is dispatched.
    fn sync(&self) {
        let mut args = [wl_argument { n: 0 }];
        // SAFETY: opcode 0 of wl_display is `sync(new_id wl_callback)`; the argument array holds the
        // new id's placeholder, and the interface describes wl_callback.
        let callback = unsafe {
            ffi_dispatch!(
                wayland_client_handle(),
                wl_proxy_marshal_array_constructor,
                self.display.cast::<wl_proxy>(),
                0,
                args.as_mut_ptr(),
                &CALLBACK_INTERFACE.0
            )
        };
        assert!(!callback.is_null(), "the stand-in's sync was not sent");
        // SAFETY: a fresh proxy; the listener matches wl_callback's one event and the counter
        // outlives the display's dispatching.
        let added = unsafe {
            ffi_dispatch!(
                wayland_client_handle(),
                wl_proxy_add_listener,
                callback,
                CALLBACK_LISTENER.0.as_ptr() as *mut extern "C" fn(),
                (&*self.done as *const Cell<u64>).cast_mut().cast::<c_void>()
            )
        };
        assert_eq!(added, 0);
    }

    fn done(&self) -> u64 {
        self.done.get()
    }

    /// One read cycle as GDK makes it, waiting at most `wait` for the fd, then one non-blocking
    /// iteration of the main context, where the thread's mailbox is drained.
    fn pump(&self, wait: Duration) {
        let display = self.display;
        // SAFETY: a live display, used only from this thread.
        unsafe {
            while ffi_dispatch!(wayland_client_handle(), wl_display_prepare_read, display) != 0 {
                ffi_dispatch!(wayland_client_handle(), wl_display_dispatch_pending, display);
            }
            ffi_dispatch!(wayland_client_handle(), wl_display_flush, display);
            let mut pollfd = libc::pollfd {
                fd: self.fd,
                events: libc::POLLIN,
                revents: 0,
            };
            let ready = libc::poll(&mut pollfd, 1, wait.as_millis() as c_int);
            if ready > 0 && pollfd.revents & libc::POLLIN != 0 {
                let read = ffi_dispatch!(wayland_client_handle(), wl_display_read_events, display);
                assert!(read >= 0, "the stand-in's read failed");
            } else {
                ffi_dispatch!(wayland_client_handle(), wl_display_cancel_read, display);
            }
            ffi_dispatch!(wayland_client_handle(), wl_display_dispatch_pending, display);
        }
        glib::MainContext::default().iteration(false);
    }

    /// Pumps until `done` holds; false if `limit` passed first.
    fn pump_until(&self, limit: Duration, done: impl Fn() -> bool) -> bool {
        let start = Instant::now();
        while !done() {
            if start.elapsed() > limit {
                return false;
            }
            self.pump(Duration::from_millis(1));
        }
        true
    }

    fn turns(&self) {
        for _ in 0..100 {
            self.pump(Duration::ZERO);
        }
    }
}

// ---------------------------------------------------------------------------------------------
// A sink that records what reaches it.

#[derive(Default)]
struct Recorder {
    frames: RefCell<Vec<(PaneId, u64, Instant)>>,
    unavailable: RefCell<Vec<String>>,
    failed: RefCell<Vec<String>>,
    ready: Cell<u32>,
    other: Cell<u32>,
}

impl PresentSink for Recorder {
    fn event(&self, event: PresentEvent) {
        match event {
            PresentEvent::FrameDone { pane, seq, at } => self.frames.borrow_mut().push((pane, seq, at)),
            PresentEvent::Unavailable(reason) => self.unavailable.borrow_mut().push(reason),
            PresentEvent::ThreadFailed(message) => self.failed.borrow_mut().push(message),
            _ => self.other.set(self.other.get() + 1),
        }
    }

    fn globals_ready(&self) {
        self.ready.set(self.ready.get() + 1);
    }
}

impl Recorder {
    fn seqs(&self) -> Vec<u64> {
        self.frames.borrow().iter().map(|frame| frame.1).collect()
    }

    fn frame(&self, seq: u64) -> Option<Instant> {
        self.frames
            .borrow()
            .iter()
            .find(|frame| frame.1 == seq)
            .map(|frame| frame.2)
    }
}

fn subscribe(thread: &PresenterThread, pane: PaneId, recorder: &Rc<Recorder>) {
    let weak: Weak<dyn PresentSink> = Rc::downgrade(recorder) as Weak<dyn PresentSink>;
    thread.subscribe(pane, weak);
}

struct Setup {
    server: FakeServer,
    stand_in: StandIn,
    thread: Rc<PresenterThread>,
}

const LIMIT: Duration = Duration::from_secs(5);

impl Setup {
    fn start(globals: Vec<Global>, feedback: Option<Feedback>) -> Self {
        Self::start_with(globals, feedback, |_| {})
    }

    fn start_with(globals: Vec<Global>, feedback: Option<Feedback>, before: impl FnOnce(&FakeServer)) -> Self {
        let mut server = FakeServer::start(globals, feedback);
        before(&server);
        let stand_in = StandIn::connect(server.take_client_fd());
        // SAFETY: the stand-in's display stays connected until the process exits: it is never
        // disconnected.
        let thread = unsafe { PresenterThread::start_on_foreign_display(stand_in.display.cast()) }
            .expect("the presentation thread starts");
        Self {
            server,
            stand_in,
            thread,
        }
    }

    /// Subscribes a new pane and waits until the thread has finished its startup (with no
    /// `wl_subcompositor` on the fake server, that ends in `Unavailable`).
    fn pane_after_startup(&self) -> (PaneId, Rc<Recorder>) {
        let pane = self.thread.new_pane_id();
        let recorder = Rc::new(Recorder::default());
        subscribe(&self.thread, pane, &recorder);
        let start = Instant::now();
        if !self
            .stand_in
            .pump_until(LIMIT, || !recorder.unavailable.borrow().is_empty())
        {
            let stuck = self.diagnose();
            let late = self
                .stand_in
                .pump_until(Duration::from_secs(20), || !recorder.unavailable.borrow().is_empty());
            panic!(
                "the thread's startup had not ended after {LIMIT:?} ({}); {stuck}",
                if late {
                    format!("it ended after {:?}", start.elapsed())
                } else {
                    "nor 20 s later".to_string()
                }
            );
        }
        (pane, recorder)
    }

    /// What every party was doing: the server's request count, the thread's mailbox, and each
    /// thread's state, kernel wait channel and current syscall.
    fn diagnose(&self) -> String {
        let mut report = format!(
            "server requests {}, mailbox wakes {}, thread exited {}, stand-in dones {}",
            self.server.requests(),
            self.thread.mailbox_wakes(),
            self.thread.thread_exited(),
            self.stand_in.done()
        );
        if let Ok(tasks) = std::fs::read_dir("/proc/self/task") {
            for task in tasks.flatten() {
                let path = task.path();
                let read = |name: &str| std::fs::read_to_string(path.join(name)).unwrap_or_default();
                let stat = read("stat");
                let state = stat
                    .rsplit(')')
                    .next()
                    .unwrap_or("")
                    .split_whitespace()
                    .next()
                    .unwrap_or("?")
                    .to_string();
                report.push_str(&format!(
                    "\n  task {} {:?} state {state} wchan {:?} syscall {:?}",
                    task.file_name().to_string_lossy(),
                    read("comm").trim(),
                    read("wchan").trim(),
                    read("syscall").trim()
                ));
            }
        }
        report
    }

    fn sync(&self, pane: PaneId, seq: u64) -> u32 {
        self.thread
            .test_sync(pane, seq)
            .expect("the thread's connection is open")
    }
}

// ---------------------------------------------------------------------------------------------
// The cases.

fn no_deadlock_and_no_lost_event_over_20000_syncs() {
    let setup = Setup::start(Vec::new(), None);
    let (pane, recorder) = setup.pane_after_startup();
    let mut seq = 0;
    for _ in 0..100 {
        // Both queues' requests interleaved, both readers active while the replies arrive.
        for _ in 0..100 {
            setup.sync(pane, seq);
            setup.stand_in.sync();
            seq += 1;
        }
        let want = seq;
        assert!(
            setup.stand_in.pump_until(Duration::from_secs(10), || {
                recorder.frames.borrow().len() as u64 == want && setup.stand_in.done() == want
            }),
            "stalled at {} of our dones and {} of the stand-in's, of {want} each",
            recorder.frames.borrow().len(),
            setup.stand_in.done()
        );
    }
    assert_eq!(
        recorder.seqs(),
        (0..10_000).collect::<Vec<_>>(),
        "every done, once, in order"
    );
    assert_eq!(setup.stand_in.done(), 10_000);
}

fn our_queue_is_dispatched_when_only_gdk_reads() {
    let setup = Setup::start(Vec::new(), None);
    let (pane, recorder) = setup.pane_after_startup();

    for seq in 0..20 {
        setup.sync(pane, seq);
        assert!(setup.stand_in.pump_until(LIMIT, || recorder.frame(seq).is_some()));
    }
    let (report, hold) = mpsc::channel();
    let (release, released) = mpsc::channel();
    setup.thread.send_test_command(TestCommand::Hold {
        release: released,
        report,
    });
    let held_from = hold.recv_timeout(LIMIT).expect("the hold started");
    setup.sync(pane, 100);
    setup.stand_in.sync();
    assert!(setup.stand_in.pump_until(LIMIT, || setup.stand_in.done() == 1));
    assert!(
        recorder.frame(100).is_none(),
        "the worker is held with the reply queued"
    );
    release.send(()).unwrap();
    let held_until = hold.recv_timeout(LIMIT).expect("the hold ended");
    assert!(held_from <= held_until);
    assert!(setup.stand_in.pump_until(LIMIT, || recorder.frame(100).is_some()));
    assert!(recorder.frame(100).unwrap() >= held_until, "dispatch follows release");
}

fn a_roundtrip_whose_reply_gdk_read_first_still_ends() {
    let setup = Setup::start(Vec::new(), None);
    let _ = setup.pane_after_startup();
    // The thread holds no read intention while it is busy, so the stand-in reads the reply into
    // the thread's queue; the thread must find it there rather than wait in its poll for more.
    for _ in 0..5 {
        let (done, ended) = mpsc::channel();
        let (entered, entry) = mpsc::channel();
        let (release, released) = mpsc::channel();
        setup.thread.send_test_command(TestCommand::Roundtrip {
            entered,
            release: released,
            done,
        });
        entry.recv_timeout(LIMIT).expect("inside the roundtrip wait");
        let before = setup.stand_in.done();
        setup.stand_in.sync();
        assert!(setup.stand_in.pump_until(LIMIT, || setup.stand_in.done() > before));
        release.send(()).unwrap();
        assert!(
            setup.stand_in.pump_until(LIMIT, || ended.try_recv().is_ok()),
            "the thread is still waiting for a reply that is already in its queue; {}",
            setup.diagnose()
        );
    }
}

fn shutdown_returns_while_the_server_holds_its_reply() {
    // Idle in its poll.
    let setup = Setup::start(Vec::new(), None);
    let _ = setup.pane_after_startup();
    assert!(setup.thread.connection_probe().is_some());
    setup.thread.shutdown();
    assert!(
        setup.thread.connection_probe().is_none(),
        "shutdown relinquishes the host's connection, even if stopping needed the leak fallback"
    );

    // The startup reply stays held until shutdown has returned.
    let setup = Setup::start_with(Vec::new(), None, |server| server.hold_next());
    assert!(
        setup.stand_in.pump_until(LIMIT, || !setup.server.held().is_empty()),
        "the thread sent nothing"
    );
    assert!(setup.thread.connection_probe().is_some());
    setup.thread.shutdown();
    assert!(!setup.server.held().is_empty(), "the reply is still held");
    assert!(
        setup.thread.connection_probe().is_none(),
        "shutdown relinquishes the host's connection, even if stopping needed the leak fallback"
    );
    // The late replies reach objects that are gone with the queue; the stand-in reads them.
    for callback in setup.server.held() {
        setup.server.release(callback);
    }
    setup.stand_in.sync();
    assert!(setup.stand_in.pump_until(LIMIT, || setup.stand_in.done() == 1));
}

fn a_thread_that_does_not_confirm_stop_is_leaked_not_dropped() {
    let setup = Setup::start(Vec::new(), None);
    let _ = setup.pane_after_startup();
    let probe = setup.thread.connection_probe().expect("a live connection");
    let (report, hold) = mpsc::channel();
    let (release, released) = mpsc::channel();
    setup.thread.send_test_command(TestCommand::Hold {
        release: released,
        report,
    });
    hold.recv_timeout(LIMIT).expect("the hold started");
    setup.thread.shutdown();
    assert!(
        !setup.thread.thread_exited(),
        "shutdown returned while the worker was held"
    );
    release.send(()).unwrap();
    hold.recv_timeout(LIMIT).expect("the hold ended");
    assert!(
        setup.stand_in.pump_until(LIMIT, || setup.thread.thread_exited()),
        "the thread never stopped"
    );
    assert!(probe.is_alive(), "the connection was dropped, not leaked");
    // The stand-in still reads and dispatches.
    setup.stand_in.sync();
    assert!(setup.stand_in.pump_until(LIMIT, || setup.stand_in.done() == 1));
}

fn unsubscribing_the_only_pane_with_a_callback_pending_neither_aborts_nor_warns() {
    let setup = Setup::start(Vec::new(), None);
    let (pane, recorder) = setup.pane_after_startup();
    setup.server.hold_next();
    let callback = setup.sync(pane, 1);
    assert!(setup
        .stand_in
        .pump_until(LIMIT, || setup.server.held().contains(&callback)));
    setup.thread.unsubscribe(pane);
    setup.server.release(callback);
    assert!(
        setup.stand_in.pump_until(LIMIT, || setup.thread.dropped_events() == 1),
        "the released done never reached the thread"
    );
    assert!(
        recorder.frames.borrow().is_empty(),
        "an unsubscribed pane heard its done"
    );
    subscribe(&setup.thread, pane, &recorder);
    setup.sync(pane, 2);
    assert!(
        setup.stand_in.pump_until(LIMIT, || recorder.frame(2).is_some()),
        "the thread stopped serving"
    );
    assert_eq!(recorder.seqs(), vec![2]);
}

fn a_panic_on_the_thread_becomes_thread_failed() {
    let setup = Setup::start(Vec::new(), None);
    let (pane, recorder) = setup.pane_after_startup();
    let probe = setup.thread.connection_probe().expect("a live connection");
    setup.server.hold_next();
    let callback = setup.sync(pane, 1);
    assert!(setup
        .stand_in
        .pump_until(LIMIT, || setup.server.held().contains(&callback)));
    setup.thread.send_test_command(TestCommand::Panic);
    assert!(
        setup
            .stand_in
            .pump_until(LIMIT, || !recorder.failed.borrow().is_empty()),
        "no ThreadFailed"
    );
    assert!(recorder.failed.borrow()[0].contains("a test asked the presentation thread to panic"));
    assert!(setup.stand_in.pump_until(LIMIT, || setup.thread.thread_exited()));
    // A pane subscribing after the failure hears it at once.
    let late = Rc::new(Recorder::default());
    subscribe(&setup.thread, setup.thread.new_pane_id(), &late);
    assert_eq!(
        late.failed.borrow().len(),
        1,
        "a late subscriber was not told of the failure"
    );
    // The GTK side's own reference goes with a confirmed stop; the thread's leaked one remains.
    setup.thread.shutdown();
    assert!(
        probe.is_alive(),
        "the failed thread's connection was dropped, not leaked"
    );
    // Its pending callback's done lands on the leaked queue, unread, and aborts nothing.
    setup.server.release(callback);
    setup.stand_in.turns();
    // GDK's reader is not held by a read intention the thread left behind.
    setup.stand_in.sync();
    assert!(
        setup.stand_in.pump_until(LIMIT, || setup.stand_in.done() == 1),
        "the stand-in's reader is held"
    );
    assert!(
        recorder.frames.borrow().is_empty(),
        "a done was delivered after the failure"
    );
}

fn the_mailbox_wakes_once_per_drain() {
    let setup = Setup::start(Vec::new(), None);
    let (pane, recorder) = setup.pane_after_startup();
    setup.stand_in.turns();
    let before = setup.thread.mailbox_wakes();
    let (done, burst) = mpsc::channel();
    setup
        .thread
        .send_test_command(TestCommand::Burst { pane, count: 100, done });
    burst.recv_timeout(LIMIT).expect("the burst was posted");
    assert!(setup
        .stand_in
        .pump_until(LIMIT, || recorder.frames.borrow().len() == 100));
    assert_eq!(
        setup.thread.mailbox_wakes() - before,
        1,
        "one wake-up for the whole burst"
    );
    assert_eq!(recorder.seqs(), (0..100).collect::<Vec<_>>(), "delivered in order");
}

fn events_for_an_unsubscribed_pane_are_dropped() {
    let setup = Setup::start(Vec::new(), None);
    let (kept, kept_sink) = setup.pane_after_startup();
    let (gone, gone_sink) = setup.pane_after_startup();
    setup.thread.unsubscribe(gone);
    setup.sync(gone, 1);
    setup.sync(kept, 1);
    assert!(setup.stand_in.pump_until(LIMIT, || kept_sink.frame(1).is_some()));
    let (done, burst) = mpsc::channel();
    setup.thread.send_test_command(TestCommand::Burst {
        pane: gone,
        count: 5,
        done,
    });
    burst.recv_timeout(LIMIT).expect("the burst was posted");
    let (done, burst) = mpsc::channel();
    setup.thread.send_test_command(TestCommand::Burst {
        pane: kept,
        count: 5,
        done,
    });
    burst.recv_timeout(LIMIT).expect("the burst was posted");
    assert!(setup
        .stand_in
        .pump_until(LIMIT, || kept_sink.frames.borrow().len() == 6));
    assert!(
        gone_sink.frames.borrow().is_empty(),
        "an unsubscribed pane heard its events"
    );
    assert_eq!(setup.thread.dropped_events(), 6);
}

fn globals(list: &[(&'static str, u32)]) -> Vec<Global> {
    list.iter()
        .map(|&(interface, version)| Global { interface, version })
        .collect()
}

fn unavailable_when_a_required_global_is_missing() {
    let setup = Setup::start(
        globals(&[
            ("wl_compositor", 4),
            ("zwp_linux_dmabuf_v1", 4),
            ("wp_viewporter", 1),
            ("wp_presentation", 1),
        ]),
        None,
    );
    let (_, recorder) = setup.pane_after_startup();
    let reason = recorder.unavailable.borrow()[0].clone();
    assert!(
        reason.contains("wl_subcompositor"),
        "the reason does not name the missing global: {reason:?}"
    );
    assert_eq!(recorder.ready.get(), 0);
    assert!(setup.thread.globals().is_none());
    assert!(
        setup.server.bound().is_empty(),
        "bound {:?} with nothing to use them for",
        setup.server.bound()
    );
}

fn unavailable_on_dmabuf_v3() {
    let setup = Setup::start(
        globals(&[
            ("wl_compositor", 5),
            ("wl_subcompositor", 1),
            ("zwp_linux_dmabuf_v1", 3),
        ]),
        None,
    );
    let (_, recorder) = setup.pane_after_startup();
    let reason = recorder.unavailable.borrow()[0].clone();
    assert!(
        reason.contains("zwp_linux_dmabuf_v1 is version 3"),
        "unexpected reason {reason:?}"
    );
    assert_eq!(recorder.ready.get(), 0);
    assert!(setup.thread.globals().is_none());
    assert!(
        setup.server.bound().is_empty(),
        "bound {:?} with nothing to use them for",
        setup.server.bound()
    );
}

const XR24: u32 = 0x3432_5258;
const AR24: u32 = 0x3432_5241;

fn ready_after_the_globals_and_the_first_feedback() {
    let main_device = 0xe280_u64;
    let setup = Setup::start(
        globals(&[
            ("wl_compositor", 6),
            ("wl_subcompositor", 1),
            ("zwp_linux_dmabuf_v1", 5),
            ("wp_viewporter", 1),
            ("wp_presentation", 2),
        ]),
        Some(Feedback {
            table: vec![(XR24, 0), (AR24, 0), (XR24, 0x0100_0000_0000_0001)],
            main_device,
            tranches: vec![(main_device, vec![2, 0]), (0xe281, vec![1])],
        }),
    );
    let pane = setup.thread.new_pane_id();
    let early = Rc::new(Recorder::default());
    subscribe(&setup.thread, pane, &early);
    assert!(
        setup.stand_in.pump_until(LIMIT, || early.ready.get() == 1),
        "no Ready: {:?}",
        early.unavailable.borrow()
    );
    {
        let globals = setup.thread.globals().expect("the stored globals");
        assert_eq!(globals.feedback.main_device, main_device);
        assert_eq!(globals.feedback.formats, vec![(XR24, 0x0100_0000_0000_0001), (XR24, 0)]);
        assert!(globals.viewporter.is_some() && globals.presentation.is_some());
    }
    assert_eq!(
        setup.server.bound(),
        vec![
            ("wl_compositor".to_string(), 4),
            ("wl_subcompositor".to_string(), 1),
            ("zwp_linux_dmabuf_v1".to_string(), 4),
            ("wp_viewporter".to_string(), 1),
            ("wp_presentation".to_string(), 1),
        ]
    );
    // A pane subscribing after `Ready` is told at once, without a wake-up.
    let late = Rc::new(Recorder::default());
    subscribe(&setup.thread, setup.thread.new_pane_id(), &late);
    assert_eq!(late.ready.get(), 1);
    assert!(early.unavailable.borrow().is_empty() && early.other.get() == 0);
}

// ---------------------------------------------------------------------------------------------
// The harness.

struct Case {
    name: &'static str,
    run: fn(),
    /// Lines its stderr must hold.
    stderr_has: &'static [&'static str],
    /// Lines its stderr must not hold, beside libwayland's queue warnings.
    stderr_lacks: &'static [&'static str],
}

const CASES: &[Case] = &[
    Case {
        name: "no_deadlock_and_no_lost_event_over_20000_syncs",
        run: no_deadlock_and_no_lost_event_over_20000_syncs,
        stderr_has: &[],
        stderr_lacks: &[LATE_STOP],
    },
    Case {
        name: "our_queue_is_dispatched_when_only_gdk_reads",
        run: our_queue_is_dispatched_when_only_gdk_reads,
        stderr_has: &[],
        stderr_lacks: &[LATE_STOP],
    },
    Case {
        name: "a_roundtrip_whose_reply_gdk_read_first_still_ends",
        run: a_roundtrip_whose_reply_gdk_read_first_still_ends,
        stderr_has: &[],
        stderr_lacks: &[LATE_STOP],
    },
    Case {
        name: "shutdown_returns_while_the_server_holds_its_reply",
        run: shutdown_returns_while_the_server_holds_its_reply,
        stderr_has: &[],
        stderr_lacks: &[],
    },
    Case {
        name: "a_thread_that_does_not_confirm_stop_is_leaked_not_dropped",
        run: a_thread_that_does_not_confirm_stop_is_leaked_not_dropped,
        stderr_has: &[LATE_STOP],
        stderr_lacks: &[],
    },
    Case {
        name: "unsubscribing_the_only_pane_with_a_callback_pending_neither_aborts_nor_warns",
        run: unsubscribing_the_only_pane_with_a_callback_pending_neither_aborts_nor_warns,
        stderr_has: &[],
        stderr_lacks: &[LATE_STOP],
    },
    Case {
        name: "a_panic_on_the_thread_becomes_thread_failed",
        run: a_panic_on_the_thread_becomes_thread_failed,
        stderr_has: &["[editor] subsurface thread failed: the presentation thread panicked"],
        stderr_lacks: &[LATE_STOP],
    },
    Case {
        name: "the_mailbox_wakes_once_per_drain",
        run: the_mailbox_wakes_once_per_drain,
        stderr_has: &[],
        stderr_lacks: &[LATE_STOP],
    },
    Case {
        name: "events_for_an_unsubscribed_pane_are_dropped",
        run: events_for_an_unsubscribed_pane_are_dropped,
        stderr_has: &[],
        stderr_lacks: &[LATE_STOP],
    },
    Case {
        name: "unavailable_when_a_required_global_is_missing",
        run: unavailable_when_a_required_global_is_missing,
        stderr_has: &[],
        stderr_lacks: &[LATE_STOP],
    },
    Case {
        name: "unavailable_on_dmabuf_v3",
        run: unavailable_on_dmabuf_v3,
        stderr_has: &[],
        stderr_lacks: &[LATE_STOP],
    },
    Case {
        name: "ready_after_the_globals_and_the_first_feedback",
        run: ready_after_the_globals_and_the_first_feedback,
        stderr_has: &[],
        stderr_lacks: &[LATE_STOP],
    },
];

/// Each case gets this long before the parent kills its child.
const CASE_LIMIT: Duration = Duration::from_secs(60);

fn run_child(name: &str) -> Result<(), String> {
    let mut child = Command::new(std::env::current_exe().map_err(|error| error.to_string())?)
        .env(CASE_ENV, name)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("the child did not start: {error}"))?;
    let mut stdout_pipe = child.stdout.take().expect("piped stdout");
    let mut stderr_pipe = child.stderr.take().expect("piped stderr");
    let stdout_reader = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = stdout_pipe.read_to_string(&mut text);
        text
    });
    let stderr_reader = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = stderr_pipe.read_to_string(&mut text);
        text
    });
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if start.elapsed() < CASE_LIMIT => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                // The child's own handle: its pid, nothing found by name.
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    let stdout = stdout_reader.join().unwrap_or_default();
    let stderr = stderr_reader.join().unwrap_or_default();
    let case = CASES.iter().find(|case| case.name == name).expect("a known case");
    let mut problems = Vec::new();
    match status {
        None => problems.push(format!("killed after {CASE_LIMIT:?}")),
        Some(status) if !status.success() => problems.push(format!("exited with {status}")),
        Some(_) => {}
    }
    if !stdout.lines().any(|line| line == "RESULT pass") {
        problems.push("no RESULT pass".to_string());
    }
    for line in QUEUE_WARNINGS.iter().chain(case.stderr_lacks) {
        if stderr.contains(line) {
            problems.push(format!("stderr has {line:?}"));
        }
    }
    for line in case.stderr_has {
        if !stderr.contains(line) {
            problems.push(format!("stderr lacks {line:?}"));
        }
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "{}\n--- stdout\n{stdout}\n--- stderr\n{stderr}",
            problems.join("; ")
        ))
    }
}

fn main() {
    if let Ok(name) = std::env::var(CASE_ENV) {
        let case = CASES
            .iter()
            .find(|case| case.name == name)
            .unwrap_or_else(|| panic!("unknown case {name:?}"));
        let context = glib::MainContext::default();
        let _owner = context
            .acquire()
            .expect("the main thread owns the default main context");
        (case.run)();
        println!("RESULT pass");
        return;
    }
    let filters: Vec<String> = std::env::args().skip(1).filter(|arg| !arg.starts_with('-')).collect();
    let mut failed = Vec::new();
    let mut ran = 0;
    for case in CASES {
        if !filters.is_empty() && !filters.iter().any(|filter| case.name.contains(filter.as_str())) {
            continue;
        }
        ran += 1;
        match run_child(case.name) {
            Ok(()) => println!("test {} ... ok", case.name),
            Err(report) => {
                println!("test {} ... FAILED\n{report}", case.name);
                failed.push(case.name);
            }
        }
    }
    println!(
        "presenter_thread: {} passed, {} failed",
        ran - failed.len(),
        failed.len()
    );
    if !failed.is_empty() {
        std::process::exit(1);
    }
}
