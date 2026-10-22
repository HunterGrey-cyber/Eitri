//! The GLib side of the window contract's `Scheduler`: timers, idles and descriptor watches on the
//! default main context, each of which fires by posting an event to the attached sink rather than by
//! running a caller's callback, and the waker worker threads use to reach the UI thread.
//!
//! A fired timer or idle is forgotten before its event is posted, so a sink that re-arms the same
//! token from inside its handler arms a fresh source, and a later `cancel` never removes a source
//! GLib has already destroyed (which GLib reports as a critical warning).

use std::cell::RefCell;
use std::collections::HashMap;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::time::{Duration, Instant};

use eitri_core::wm::{HostEvent, HostSink, Interest, Priority, Scheduler, TimerToken, Waker, WatchToken};
use gtk4::glib::{self, translate::IntoGlib};

/// The GLib priority a contract priority runs at. Input and ordinary service work share GDK's own
/// event priority, `PRIORITY_DEFAULT`: under GTK input arrives as events on the same context, so
/// there is nothing to rank above it. Work that must yield to painting runs at
/// `PRIORITY_DEFAULT_IDLE` (200), below GDK's redraw (120), so a flood of it never starves a frame.
pub(crate) fn glib_priority(priority: Priority) -> glib::Priority {
    match priority {
        Priority::Input | Priority::Default => glib::Priority::DEFAULT,
        Priority::BelowPaint => glib::Priority::DEFAULT_IDLE,
    }
}

/// The contract's scheduler on GLib's default main context. Lives on the UI thread; only its waker
/// crosses threads.
pub(crate) struct GlibScheduler {
    me: Weak<GlibScheduler>,
    sink: RefCell<Option<Weak<dyn HostSink>>>,
    timers: RefCell<HashMap<TimerToken, glib::SourceId>>,
    watches: RefCell<HashMap<WatchToken, glib::SourceId>>,
    wake: RefCell<Option<WakeWatch>>,
}

/// The waker's eventfd and, once it could be added, the UI thread's watch on it.
struct WakeWatch {
    waker: Arc<EventFdWaker>,
    source: Option<glib::SourceId>,
}

impl GlibScheduler {
    pub(crate) fn new() -> Rc<GlibScheduler> {
        Rc::new_cyclic(|me| GlibScheduler {
            me: me.clone(),
            sink: RefCell::new(None),
            timers: RefCell::new(HashMap::new()),
            watches: RefCell::new(HashMap::new()),
            wake: RefCell::new(None),
        })
    }

    /// Where fired sources post. Held weakly: the sink (the driver) owns the scheduler, not the
    /// other way round.
    pub(crate) fn attach(&self, sink: Weak<dyn HostSink>) {
        *self.sink.borrow_mut() = Some(sink);
    }

    /// Posts with no borrow of this scheduler held, so the sink may arm or cancel from inside.
    fn post(&self, event: HostEvent) {
        let sink = self.sink.borrow().clone();
        match sink.and_then(|s| s.upgrade()) {
            Some(sink) => sink.post(event),
            None => eprintln!("[wm] a scheduled source fired with no sink attached: {event:?} dropped"),
        }
    }

    fn arm_timer(&self, token: TimerToken, arm: impl FnOnce(Box<dyn FnMut() -> glib::ControlFlow>) -> glib::SourceId) {
        self.cancel(token);
        let me = self.me.clone();
        let source = arm(Box::new(move || {
            if let Some(me) = me.upgrade() {
                // Forget the source before posting: GLib destroys it when this returns `Break`.
                me.timers.borrow_mut().remove(&token);
                me.post(HostEvent::Timer(token));
            }
            glib::ControlFlow::Break
        }));
        self.timers.borrow_mut().insert(token, source);
    }

    /// The UI thread's watch on the waker's eventfd, added on first use. A failed add (another thread
    /// owns the context) is retried on the next call, and the waker's writes still count until then.
    fn wake_watch(&self) -> Arc<EventFdWaker> {
        let mut slot = self.wake.borrow_mut();
        let entry = slot.get_or_insert_with(|| WakeWatch {
            waker: Arc::new(EventFdWaker::new()),
            source: None,
        });
        if entry.source.is_none() {
            let fd = entry.waker.fd.as_raw_fd();
            let me = self.me.clone();
            let added = add_on_default_context(
                fd,
                glib::IOCondition::IN | glib::IOCondition::ERR | glib::IOCondition::HUP,
                glib_priority(Priority::Default).into_glib(),
                move |_| {
                    drain_eventfd(fd);
                    if let Some(me) = me.upgrade() {
                        me.post(HostEvent::Wake);
                    }
                    glib::ControlFlow::Continue
                },
            );
            match added {
                Ok(source) => entry.source = Some(source),
                Err(neovide_editor::fd_watch::NotOwner) => {
                    eprintln!("[wm] the waker's watch was not added: another thread owns the main context")
                }
            }
        }
        entry.waker.clone()
    }
}

impl Scheduler for GlibScheduler {
    fn after(&self, delay: Duration, token: TimerToken) {
        self.arm_timer(token, |fire| {
            glib::timeout_add_local_full(delay, glib::Priority::DEFAULT, fire)
        });
    }

    fn idle(&self, token: TimerToken) {
        self.arm_timer(token, |fire| {
            glib::idle_add_local_full(glib::Priority::DEFAULT_IDLE, fire)
        });
    }

    fn cancel(&self, token: TimerToken) {
        let source = self.timers.borrow_mut().remove(&token);
        if let Some(source) = source {
            source.remove();
        }
    }

    fn watch_fd(&self, fd: RawFd, interest: Interest, priority: Priority, token: WatchToken) {
        self.unwatch(token);
        let condition = match interest {
            Interest::Read => glib::IOCondition::IN,
            Interest::Write => glib::IOCondition::OUT,
        } | glib::IOCondition::ERR
            | glib::IOCondition::HUP;
        let me = self.me.clone();
        let ready = move |ready: glib::IOCondition| {
            let Some(me) = me.upgrade() else {
                return glib::ControlFlow::Break;
            };
            if ready.contains(glib::IOCondition::NVAL) {
                // The descriptor was closed under the watch: GLib would report it ready on every
                // iteration and spin the UI thread. Drop the watch and post once so its owner looks.
                eprintln!("[wm] watch {token:?}: its descriptor was closed while watched; the watch is dropped");
                me.watches.borrow_mut().remove(&token);
                me.post(HostEvent::Fd(token));
                return glib::ControlFlow::Break;
            }
            me.post(HostEvent::Fd(token));
            glib::ControlFlow::Continue
        };
        let added = add_on_default_context(fd, condition, glib_priority(priority).into_glib(), ready);
        match added {
            Ok(source) => {
                self.watches.borrow_mut().insert(token, source);
            }
            Err(neovide_editor::fd_watch::NotOwner) => {
                eprintln!("[wm] watch {token:?} was not added: another thread owns the main context")
            }
        }
    }

    fn unwatch(&self, token: WatchToken) {
        let source = self.watches.borrow_mut().remove(&token);
        if let Some(source) = source {
            source.remove();
        }
    }

    fn now(&self) -> Instant {
        Instant::now()
    }

    fn waker(&self) -> Arc<dyn Waker> {
        self.wake_watch()
    }
}

impl Drop for GlibScheduler {
    /// Removes every source still armed: their closures would only find a dead scheduler, and the
    /// waker's watch must go before the last owner of its eventfd can close it.
    fn drop(&mut self) {
        for (_, source) in self.timers.get_mut().drain() {
            source.remove();
        }
        for (_, source) in self.watches.get_mut().drain() {
            source.remove();
        }
        if let Some(source) = self.wake.get_mut().as_mut().and_then(|w| w.source.take()) {
            source.remove();
        }
    }
}

/// Adds a descriptor watch with the default main context held by this thread for the call.
/// `fd_watch::add_local` only checks ownership, and before the main loop runs no thread owns the
/// context, so a watch added during start-up would be refused although it is added from the very
/// thread that will run the loop. GLib's own `*_local` sources acquire the context the same way.
/// Acquiring nests on the owning thread and fails only when another thread holds the context,
/// which is the one case the closure must not be registered in.
fn add_on_default_context(
    fd: RawFd,
    condition: glib::IOCondition,
    priority: std::ffi::c_int,
    func: impl FnMut(glib::IOCondition) -> glib::ControlFlow + 'static,
) -> Result<glib::SourceId, neovide_editor::fd_watch::NotOwner> {
    let context = glib::MainContext::default();
    let Ok(_owner) = context.acquire() else {
        return Err(neovide_editor::fd_watch::NotOwner);
    };
    neovide_editor::fd_watch::add_local(fd, condition, priority, func)
}

/// A worker's route to the UI thread: one eventfd whose counter a wake raises and the UI thread's
/// watch drains, so any number of wakes before the UI thread runs post one `Wake`.
struct EventFdWaker {
    fd: OwnedFd,
}

impl EventFdWaker {
    fn new() -> EventFdWaker {
        // SAFETY: `eventfd` takes no pointers; a non-negative return is a new descriptor we own.
        let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if fd < 0 {
            panic!(
                "the UI thread's waker needs an eventfd: {}",
                std::io::Error::last_os_error()
            );
        }
        // SAFETY: `fd` was just returned by `eventfd` and nothing else owns it.
        EventFdWaker {
            fd: unsafe { OwnedFd::from_raw_fd(fd) },
        }
    }
}

impl Waker for EventFdWaker {
    fn wake(&self) {
        let one: u64 = 1;
        // SAFETY: writes the 8 bytes of a live `u64` to a descriptor this value owns. A full counter
        // (EAGAIN) means a wake is already pending, which is all a wake asks for.
        unsafe {
            libc::write(
                self.fd.as_raw_fd(),
                (&one as *const u64).cast(),
                std::mem::size_of::<u64>(),
            );
        }
    }
}

/// Resets the eventfd's counter, so the watch stops being ready until the next wake.
fn drain_eventfd(fd: RawFd) {
    let mut count: u64 = 0;
    // SAFETY: reads at most 8 bytes into a live `u64`. The descriptor is non-blocking: with nothing
    // to read it answers EAGAIN, which leaves nothing to drain.
    unsafe {
        libc::read(fd, (&mut count as *mut u64).cast(), std::mem::size_of::<u64>());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eitri_core::wm::conformance::RecordingSink;
    use eitri_core::wm::{HostEvent, HostSink, Interest, Priority, Scheduler, TimerToken, WatchToken};
    use gtk4::glib;
    use std::rc::Rc;
    use std::time::Duration;

    /// Runs every ready source on the default context, bounded so a source that stays ready (a watch
    /// whose counter was never drained) fails the test instead of hanging it.
    fn drain_default_context() {
        let context = glib::MainContext::default();
        for _ in 0..1000 {
            if !context.iteration(false) {
                return;
            }
        }
        panic!("the default context still had ready sources after 1000 iterations");
    }

    fn attached() -> (Rc<GlibScheduler>, Rc<RecordingSink>) {
        let scheduler = GlibScheduler::new();
        let sink = Rc::new(RecordingSink::default());
        let erased: Rc<dyn HostSink> = sink.clone();
        scheduler.attach(Rc::downgrade(&erased));
        (scheduler, sink)
    }

    fn posted(sink: &RecordingSink) -> Vec<HostEvent> {
        sink.events().into_iter().map(|(_, event)| event).collect()
    }

    #[test]
    fn glib_scheduler_maps_idle_to_default_idle_and_timers_to_default() {
        let _turn = crate::layout_state::DEFAULT_CONTEXT_TURN
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let context = glib::MainContext::default();
        let _owner = context
            .acquire()
            .expect("glib's default main context, which the other tests on it leave to this one");
        assert_eq!(glib_priority(Priority::BelowPaint), glib::Priority::DEFAULT_IDLE);
        assert_eq!(glib_priority(Priority::Default), glib::Priority::DEFAULT);
        assert_eq!(glib_priority(Priority::Input), glib::Priority::DEFAULT);

        let (scheduler, sink) = attached();
        // The idle is armed first; a timer due now still runs before it, because the idle sits below
        // the timer's priority and GLib dispatches only the most urgent ready sources per iteration.
        scheduler.idle(TimerToken(1));
        scheduler.after(Duration::ZERO, TimerToken(2));
        scheduler.after(Duration::ZERO, TimerToken(3));
        scheduler.cancel(TimerToken(3));
        drain_default_context();
        assert_eq!(
            posted(&sink),
            vec![HostEvent::Timer(TimerToken(2)), HostEvent::Timer(TimerToken(1))],
            "the due timer first, then the idle, and nothing from the cancelled timer"
        );
        // A fired source is gone: cancelling its token afterwards is harmless.
        scheduler.cancel(TimerToken(1));
        scheduler.cancel(TimerToken(2));
    }

    #[test]
    fn a_wake_from_another_thread_posts_wake_on_the_ui_thread() {
        let _turn = crate::layout_state::DEFAULT_CONTEXT_TURN
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let context = glib::MainContext::default();
        let _owner = context
            .acquire()
            .expect("glib's default main context, which the other tests on it leave to this one");
        let (scheduler, sink) = attached();
        let waker = scheduler.waker();
        std::thread::spawn(move || waker.wake()).join().unwrap();
        drain_default_context();
        assert_eq!(posted(&sink), vec![HostEvent::Wake], "one wake, posted once");
        // Nothing is left ready: the watch drained the counter when it posted.
        drain_default_context();
        assert_eq!(posted(&sink), vec![HostEvent::Wake]);
    }

    #[test]
    fn a_waker_and_a_watch_taken_before_the_loop_owns_the_context_still_post() {
        let _turn = crate::layout_state::DEFAULT_CONTEXT_TURN
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let context = glib::MainContext::default();
        assert!(
            !context.is_owner(),
            "start-up code runs before the main loop holds the default context"
        );
        let (scheduler, sink) = attached();
        let waker = scheduler.waker();
        let watched = EventFdWaker::new();
        scheduler.watch_fd(watched.fd.as_raw_fd(), Interest::Read, Priority::Default, WatchToken(7));
        std::thread::spawn(move || waker.wake()).join().unwrap();
        watched.wake();
        let _owner = context
            .acquire()
            .expect("glib's default main context, which the other tests on it leave to this one");
        // The watch on `watched` is never drained, so it stays ready: one iteration, not a drain.
        for _ in 0..4 {
            context.iteration(false);
        }
        scheduler.unwatch(WatchToken(7));
        let events = posted(&sink);
        assert!(
            events.contains(&HostEvent::Wake),
            "a waker taken before the loop ran still posts a wake: {events:?}"
        );
        assert!(
            events.contains(&HostEvent::Fd(WatchToken(7))),
            "a watch added before the loop ran still posts when its descriptor is ready: {events:?}"
        );
    }
}
