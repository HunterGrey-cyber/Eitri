//! The one watch that wakes every live editor pane on this thread.
//!
//! Every `LiveHarness` on a thread joins the same headless event loop, so its fd is one fd for all of
//! them and outlives each harness, and a pump through any harness applies every harness's pending
//! redraw batches. Two consequences shape this module:
//!
//! - **One watch, for every pane.** A watch per pane on the shared fd would wake every pane for any
//!   pane's batch, and leave it to the panes to agree on who drains it. Instead the first pane that
//!   registers adds one watch ([`crate::fd_watch::add_event_loop_watch`], below GDK's paint priority),
//!   kept while any pane is registered; each wake services every registered pane. A pane's service
//!   pumps, compares its harness's redraw count and exit flag with what it last saw, queues its
//!   render or reports the exit when either moved, and keeps its tick while it wants frames.
//! - **One pump per wake.** Within a wake (or a sweep) only the first pane's service pumps
//!   ([`pump_once_per_pass`]); every pane is inspected after that one pump. Were each pane to pump
//!   in turn, a batch arriving between two of those pumps would be applied by a later pane's pump
//!   behind an earlier pane that had already been inspected, and the fd it drained would never wake
//!   that pane again. With one pump, a batch that arrives after it stays queued on the loop and
//!   wakes the fd again.
//! - **A pump outside the watch wakes the others.** A pane's render, its tick and its shutdown pump
//!   too, and leave the fd drained: another pane whose batches that pump applied gets no wake from
//!   the fd and would show a stale frame until something else made it draw. So whoever pumps outside
//!   the watch calls [`after_outside_pump`], which, while another pane is registered, queues one
//!   sweep at the watch's own priority that services every registered pane as a wake would.
//!
//! Everything here runs on the thread that owns the default main context; a pane that registers from
//! anywhere else, or after the shared fd reported an error, keeps its tick on instead.

use std::cell::{Cell, RefCell};
use std::os::fd::RawFd;
use std::rc::{Rc, Weak};

use gtk4::glib;

use crate::fd_watch;
use crate::tick_driver::TickDriver;

/// One pane's registration, alive as long as its harness: the session holds the only strong
/// reference, so the registry forgets a pane whose session is gone even if nobody unregistered it.
pub(crate) struct PaneLive {
    /// The shared loop's fd, as this pane's harness reports it.
    fd: RawFd,
    /// What wakes the pane: its service, run as the fd watch's.
    driver: Weak<TickDriver>,
}

impl PaneLive {
    pub(crate) fn new(fd: RawFd, driver: &Rc<TickDriver>) -> Rc<Self> {
        Rc::new(Self {
            fd,
            driver: Rc::downgrade(driver),
        })
    }
}

#[derive(Default)]
struct Registry {
    panes: Vec<Weak<PaneLive>>,
    /// The one watch on the shared fd, while any pane is registered.
    watch: Option<glib::SourceId>,
    /// A sweep queued by [`after_outside_pump`] and not yet run.
    sweep: Option<glib::SourceId>,
    /// The shared fd reported an error or a hang-up: the loop it belongs to lives as long as the
    /// thread, so it is never watched again, and every pane falls back to its always-on tick.
    failed: bool,
}

impl Registry {
    fn prune(&mut self) {
        self.panes.retain(|pane| pane.strong_count() > 0);
    }

    /// The drivers of every live registered pane, so the caller can run them with the registry
    /// released: a pane's service may call back into [`register`] or [`unregister`].
    fn drivers(&mut self) -> Vec<Rc<TickDriver>> {
        self.prune();
        self.panes
            .iter()
            .filter_map(Weak::upgrade)
            .filter_map(|pane| pane.driver.upgrade())
            .collect()
    }

    /// Removes the watch and any queued sweep: nothing is left for them to wake.
    fn stand_down(&mut self) {
        if let Some(id) = self.watch.take() {
            id.remove();
        }
        if let Some(id) = self.sweep.take() {
            id.remove();
        }
    }
}

thread_local! {
    static REGISTRY: RefCell<Registry> = RefCell::new(Registry::default());
    /// While a wake or a sweep services the panes: whether a pane's service has pumped yet.
    static PASS: Cell<Option<bool>> = const { Cell::new(None) };
}

/// Adds a pane whose harness is ready. The first registration adds the watch on its fd.
pub(crate) fn register(pane: Weak<PaneLive>) {
    let Some(live) = pane.upgrade() else {
        return;
    };
    let Some(driver) = live.driver.upgrade() else {
        return;
    };
    let fall_back = REGISTRY.with(|registry| {
        let mut registry = registry.borrow_mut();
        registry.prune();
        registry.panes.push(pane);
        if registry.failed {
            return true;
        }
        if registry.watch.is_some() {
            return false;
        }
        match fd_watch::add_event_loop_watch(live.fd, on_wake) {
            Ok(id) => {
                registry.watch = Some(id);
                false
            }
            Err(fd_watch::NotOwner) => {
                println!("[tick] no fd watch (not on the main context's thread); keeping the always-on tick");
                true
            }
        }
    });
    if fall_back {
        driver.keep_ticking();
    }
}

/// Removes a pane (by its address, so a pane already dropped can still be named). The last one
/// out removes the watch: the fd stays open with the loop, but nothing is left to wake.
pub(crate) fn unregister(pane: *const PaneLive) {
    REGISTRY.with(|registry| {
        let mut registry = registry.borrow_mut();
        registry.panes.retain(|entry| entry.as_ptr() != pane);
        registry.prune();
        if registry.panes.is_empty() {
            registry.stand_down();
        }
    });
}

/// Whoever pumped a harness outside the watch calls this afterwards (`by` is its own pane, if it
/// is still registered). While another pane is registered, one sweep is queued at the watch's
/// priority, so a pane whose batches that pump applied is serviced as if the fd had woken it.
pub(crate) fn after_outside_pump(by: Option<*const PaneLive>) {
    if !glib::MainContext::default().is_owner() {
        return;
    }
    REGISTRY.with(|registry| {
        let mut registry = registry.borrow_mut();
        registry.prune();
        if registry.sweep.is_some() || registry.failed {
            return;
        }
        let others = registry
            .panes
            .iter()
            .any(|pane| by.is_none_or(|by| pane.as_ptr() != by));
        if !others {
            return;
        }
        let id = glib::idle_add_local_full(glib::Priority::from(fd_watch::EVENT_LOOP_WATCH_PRIORITY), || {
            let drivers = REGISTRY.with(|registry| {
                let mut registry = registry.borrow_mut();
                // Returning `Break` removes this source: forget it first.
                registry.sweep = None;
                registry.drivers()
            });
            service_all(&drivers);
            glib::ControlFlow::Break
        });
        registry.sweep = Some(id);
    });
}

/// A pane's service pumps the shared loop through this. Inside a wake or a sweep only the first call
/// pumps: that pump applies every pane's batches, and every pane is inspected after it. Outside one
/// (the pane's tick) it always pumps.
pub(crate) fn pump_once_per_pass(pump: impl FnOnce()) {
    let first = PASS.with(|pass| match pass.get() {
        None => true,
        Some(pumped) => {
            pass.set(Some(true));
            !pumped
        }
    });
    if first {
        pump();
    }
}

/// Runs `body` as one pass: the pumps [`pump_once_per_pass`] gates inside it come to one. A pass
/// begun inside another (a pane's service that ends up servicing panes again) is a pass of its own,
/// and the outer one's state comes back when it ends.
fn in_pass<T>(body: impl FnOnce() -> T) -> T {
    let outer = PASS.with(|pass| pass.replace(Some(false)));
    let result = body();
    PASS.with(|pass| pass.set(outer));
    result
}

/// The priority of the watch on the shared fd, `None` while no pane is registered.
pub(crate) fn watch_priority() -> Option<std::ffi::c_int> {
    REGISTRY.with(|registry| registry.borrow().watch.as_ref().and_then(fd_watch::source_priority))
}

#[cfg(feature = "test-support")]
pub(crate) fn hold_watch() -> impl FnOnce() {
    let fd = REGISTRY.with(|registry| {
        let mut registry = registry.borrow_mut();
        assert!(registry.watch.is_some(), "a live shared watch");
        // The last settling render may have queued a sweep even though every pane is idle.
        // Cancel that old work too; a pump after the hold must still be able to queue a new sweep.
        registry.stand_down();
        registry
            .panes
            .iter()
            .find_map(Weak::upgrade)
            .expect("a registered pane")
            .fd
    });
    move || {
        let context = glib::MainContext::default();
        let _held = context.acquire().expect("the watch's thread owns the context");
        REGISTRY.with(|registry| {
            let mut registry = registry.borrow_mut();
            assert!(registry.watch.is_none());
            registry.watch = Some(fd_watch::add_event_loop_watch(fd, on_wake).expect("the owning thread"));
        });
    }
}

fn on_wake(condition: glib::IOCondition) -> glib::ControlFlow {
    if condition.intersects(glib::IOCondition::ERR | glib::IOCondition::HUP | glib::IOCondition::NVAL) {
        let drivers = REGISTRY.with(|registry| {
            let mut registry = registry.borrow_mut();
            // Returning `Break` removes the source: forget it, so nothing removes it twice.
            registry.watch = None;
            registry.failed = true;
            registry.drivers()
        });
        println!("[tick] the event loop fd reported {condition:?}; falling back to the always-on tick");
        for driver in &drivers {
            driver.keep_ticking();
        }
        return glib::ControlFlow::Break;
    }
    let drivers = REGISTRY.with(|registry| registry.borrow_mut().drivers());
    service_all(&drivers);
    glib::ControlFlow::Continue
}

/// Services each pane in turn, as one pass: the first pane's pump applies every pane's batches, so
/// the panes after it find theirs already in their renderer and present them in the same wake.
fn service_all(drivers: &[Rc<TickDriver>]) {
    in_pass(|| {
        for driver in drivers {
            driver.service_from_fd();
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registered() -> usize {
        REGISTRY.with(|registry| {
            let mut registry = registry.borrow_mut();
            registry.prune();
            registry.panes.len()
        })
    }

    fn watching() -> bool {
        REGISTRY.with(|registry| registry.borrow().watch.is_some())
    }

    /// The registry is per thread: each test runs on its own libtest thread, so it starts empty.
    fn owning_the_default_context<T>(body: impl FnOnce() -> T) -> T {
        let _serial = crate::DEFAULT_CONTEXT_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let context = glib::MainContext::default();
        let _owner = context
            .acquire()
            .expect("tests that run the default main context take DEFAULT_CONTEXT_TEST_LOCK first");
        body()
    }

    #[cfg(feature = "test-support")]
    #[test]
    fn a_held_watch_is_restored_between_main_context_dispatches() {
        use std::os::fd::AsRawFd;

        let _serial = crate::DEFAULT_CONTEXT_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (read, _write) = std::io::pipe().unwrap();
        let driver = Rc::new(TickDriver::default());
        let pane = PaneLive::new(read.as_raw_fd(), &driver);
        let context = glib::MainContext::default();
        {
            let _held = context.acquire().unwrap();
            register(Rc::downgrade(&pane));
        }
        assert!(!context.is_owner(), "the caller is between dispatches");
        let release = hold_watch();
        assert!(!watching(), "no fd dispatch can precede the paint");
        release();
        assert_eq!(watch_priority(), Some(fd_watch::EVENT_LOOP_WATCH_PRIORITY));
        unregister(Rc::as_ptr(&pane));
        assert!(!watching());
    }

    #[cfg(feature = "test-support")]
    #[test]
    fn holding_the_watch_cancels_an_old_sweep_but_allows_the_next_pumps_sweep() {
        use std::os::fd::AsRawFd;

        owning_the_default_context(|| {
            let (read, _write) = std::io::pipe().unwrap();
            let driver = Rc::new(TickDriver::default());
            let pane = PaneLive::new(read.as_raw_fd(), &driver);
            register(Rc::downgrade(&pane));
            after_outside_pump(None);
            let context = glib::MainContext::default();
            let old_source = REGISTRY.with(|r| context.find_source_by_id(r.borrow().sweep.as_ref().unwrap()).unwrap());
            let release = hold_watch();
            assert!(
                old_source.is_destroyed(),
                "the settling sweep could dispatch before paint"
            );
            assert!(REGISTRY.with(|r| r.borrow().sweep.is_none()));
            after_outside_pump(None);
            let new_source = REGISTRY.with(|r| context.find_source_by_id(r.borrow().sweep.as_ref().unwrap()).unwrap());
            for _ in 0..100 {
                if new_source.is_destroyed() {
                    break;
                }
                context.iteration(false);
            }
            assert!(new_source.is_destroyed(), "the new pump's sweep must still dispatch");
            release();
            unregister(Rc::as_ptr(&pane));
        });
    }

    #[test]
    fn the_first_pane_adds_the_one_watch_and_the_last_one_out_removes_it() {
        owning_the_default_context(|| {
            let (read, _write) = std::os::unix::net::UnixStream::pair().expect("a socket pair");
            let fd = std::os::fd::AsRawFd::as_raw_fd(&read);
            let (da, db) = (Rc::new(TickDriver::default()), Rc::new(TickDriver::default()));
            let (a, b) = (PaneLive::new(fd, &da), PaneLive::new(fd, &db));
            register(Rc::downgrade(&a));
            let first = REGISTRY.with(|r| r.borrow().watch.as_ref().map(glib::SourceId::as_raw));
            register(Rc::downgrade(&b));
            let second = REGISTRY.with(|r| r.borrow().watch.as_ref().map(glib::SourceId::as_raw));
            assert!(first.is_some(), "the first registration watches the shared fd");
            assert_eq!(first, second, "a second pane shares the watch rather than adding one");
            assert_eq!(watch_priority(), Some(fd_watch::EVENT_LOOP_WATCH_PRIORITY));
            assert_eq!(registered(), 2);

            unregister(Rc::as_ptr(&a));
            assert!(watching(), "the other pane still needs the watch");
            unregister(Rc::as_ptr(&b));
            assert!(!watching(), "nothing is left to wake");
            assert_eq!(watch_priority(), None);
            // SAFETY: a lookup by id on the default context; the pointer is only compared.
            let found = unsafe { glib::ffi::g_main_context_find_source_by_id(std::ptr::null_mut(), first.unwrap()) };
            assert!(found.is_null(), "the watch's source is gone from the context");
        });
    }

    #[test]
    fn a_dropped_pane_no_longer_counts_and_unregistering_its_address_removes_the_watch() {
        owning_the_default_context(|| {
            let (read, _write) = std::os::unix::net::UnixStream::pair().expect("a socket pair");
            let fd = std::os::fd::AsRawFd::as_raw_fd(&read);
            let driver = Rc::new(TickDriver::default());
            let pane = PaneLive::new(fd, &driver);
            let address = Rc::as_ptr(&pane);
            register(Rc::downgrade(&pane));
            drop(pane);
            assert_eq!(registered(), 0, "a dead registration does not count");
            unregister(address);
            assert!(!watching());
        });
    }

    #[test]
    fn an_outside_pump_queues_one_sweep_only_while_another_pane_is_registered() {
        owning_the_default_context(|| {
            let (read, _write) = std::os::unix::net::UnixStream::pair().expect("a socket pair");
            let fd = std::os::fd::AsRawFd::as_raw_fd(&read);
            let (da, db) = (Rc::new(TickDriver::default()), Rc::new(TickDriver::default()));
            let a = PaneLive::new(fd, &da);
            register(Rc::downgrade(&a));
            let sweeping = || REGISTRY.with(|r| r.borrow().sweep.is_some());

            after_outside_pump(Some(Rc::as_ptr(&a)));
            assert!(!sweeping(), "a lone pane's own pump left nobody else stale");

            let b = PaneLive::new(fd, &db);
            register(Rc::downgrade(&b));
            after_outside_pump(Some(Rc::as_ptr(&a)));
            assert!(sweeping(), "B may hold batches A's pump applied");
            after_outside_pump(Some(Rc::as_ptr(&b)));

            let context = glib::MainContext::default();
            while context.iteration(false) {}
            assert!(!sweeping(), "the sweep ran once and is gone");

            unregister(Rc::as_ptr(&a));
            after_outside_pump(None);
            assert!(
                sweeping(),
                "a released pane's shutdown pump may have applied B's batches"
            );
            unregister(Rc::as_ptr(&b));
            assert!(!sweeping(), "the last one out removes a queued sweep too");
        });
    }

    #[test]
    fn a_wake_pumps_once_for_every_pane_and_a_tick_always_pumps() {
        let pumps = Cell::new(0);
        let pump = || pumps.set(pumps.get() + 1);
        pump_once_per_pass(pump);
        pump_once_per_pass(pump);
        assert_eq!(pumps.get(), 2, "outside a wake each service pumps for itself");

        pumps.set(0);
        in_pass(|| {
            pump_once_per_pass(pump);
            pump_once_per_pass(pump);
            pump_once_per_pass(pump);
        });
        assert_eq!(
            pumps.get(),
            1,
            "a later pane's pump could apply a batch behind an inspected pane"
        );

        pumps.set(0);
        in_pass(|| {
            pump_once_per_pass(pump);
            in_pass(|| pump_once_per_pass(pump));
            pump_once_per_pass(pump);
        });
        assert_eq!(
            pumps.get(),
            2,
            "a nested pass pumps once, and the outer one stays pumped"
        );

        pumps.set(0);
        pump_once_per_pass(pump);
        assert_eq!(pumps.get(), 1, "a finished pass leaves the tick's pump alone");
    }

    #[test]
    fn a_pane_registered_off_the_main_context_keeps_its_tick() {
        // A libtest worker thread never owns the default context here.
        let driver = Rc::new(TickDriver::default());
        let pane = PaneLive::new(0, &driver);
        register(Rc::downgrade(&pane));
        assert!(driver.is_always_on(), "no watch, so the tick stays on");
        assert!(!watching());
        unregister(Rc::as_ptr(&pane));
        driver.session_ended();
        assert!(!driver.is_always_on(), "a released harness has nothing left to poll");
    }
}
