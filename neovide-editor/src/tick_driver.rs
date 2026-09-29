//! Who runs the pane's "service" work (pump nvim, watch fullscreen/scale, resync the grid, report
//! an exit, queue a render), and when.
//!
//! Before 2026-09-29 a `GtkGLArea` tick callback stayed registered for the pane's whole life. With
//! nothing just drawn, GDK runs such a callback on its 16.667 ms fallback interval, so a redraw
//! batch that reached the fork's event loop waited 0-16.7 ms (8.5 ms at p50) before anything looked
//! at it: the whole of the insert-mode typing latency gap (`the private review notes`).
//!
//! Now: a GLib fd watch on the harness's event loop wakes the pane the moment a batch arrives, and
//! the tick callback is registered only while a frame is wanted. Three rules keep that safe, each
//! a finding of the plan review:
//!
//! - **F1:** the fd watch runs at `G_PRIORITY_DEFAULT_IDLE` (200), below `GDK_PRIORITY_REDRAW`
//!   (120), so a flood of batches can never starve a paint (of this pane, the terminal or the
//!   panel). It only services and kicks; `render_frame` pumps on every paint anyway.
//! - **F2:** a render that pumps may consume an exit, a fullscreen change or a scale change that
//!   only the service reports; [`service_pending_after_render`] asks for one more service run.
//! - **F3:** the tick stops in the committed render ([`TickDriver::stop_after_render`]) rather than
//!   one empty cycle later, so a key landing right after an animation does not wait a refresh.

use std::cell::{Cell, RefCell};
use std::os::fd::RawFd;
use std::rc::{Rc, Weak};

use gtk4::glib;
use gtk4::prelude::*;
use gtk4::{GLArea, TickCallbackId};

use crate::fd_watch;

/// What keeps the tick callback registered for a pane with a live harness. See
/// [`tick_still_wanted`]. Startup is not an input here: before a harness exists the service
/// decides by `sessionless_tick_should_render` (`NotStarted`/`Starting` keep the tick), and the
/// first `Ready` service run -- which adds the fd watch -- always runs before the first `Ready`
/// render can stop it, since `LiveSession` starts with `last_animating` true.
#[derive(Clone, Copy, Debug)]
pub(crate) struct TickInputs {
    pub animating: bool,
    pub wants_frame: bool,
    pub exit_pending: bool,
    /// An exit, fullscreen change or scale change the service has not yet reported (F2/F3).
    pub service_pending: bool,
}

pub(crate) fn tick_still_wanted(i: TickInputs) -> bool {
    i.animating || i.wants_frame || i.exit_pending || i.service_pending
}

/// What only the service reports, read after a render whose own pump may have consumed it.
#[derive(Clone, Copy, Debug)]
pub(crate) struct PendingInputs {
    pub exited: bool,
    pub close_requested: bool,
    pub fullscreen_changed: bool,
    pub scale_changed: bool,
}

pub(crate) fn service_pending_after_render(p: PendingInputs) -> bool {
    (p.exited && !p.close_requested) || p.fullscreen_changed || p.scale_changed
}

/// The state of the event-loop fd watch, tied to one harness.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FdState {
    /// A harness exists (or none yet) and no watch is registered for it: the next service run adds one.
    Idle,
    Watching,
    /// Removed (harness released or shut down, the watch failed or the fd reported an error): never
    /// re-added until [`TickDriver::session_started`] says a new harness exists.
    Released,
}

/// Which of the two drivers ran the service: the tick callback, or the event-loop fd watch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ServiceSource {
    Tick,
    FdWatch,
}

/// The pane's service work, told which driver runs it; returns whether another tick cycle is wanted.
pub(crate) type Service = Rc<dyn Fn(&GLArea, ServiceSource) -> bool>;

/// Owns the tick callback registration and the fd watch. Everything runs on the GTK main thread.
#[derive(Default)]
pub(crate) struct TickDriver {
    active: Cell<bool>,
    tick_id: RefCell<Option<TickCallbackId>>,
    area: RefCell<Option<glib::WeakRef<GLArea>>>,
    service: RefCell<Option<Service>>,
    fd_source: RefCell<Option<glib::SourceId>>,
    fd_state: Cell<Option<FdState>>,
    /// Set when the fd watch cannot be had: the tick then stays on, as it did before this driver,
    /// until that harness is released ([`TickDriver::unwatch_fd`]).
    always_on: Cell<bool>,
}

impl TickDriver {
    pub(crate) fn attach(&self, area: &GLArea, service: Service) {
        *self.area.borrow_mut() = Some(area.downgrade());
        *self.service.borrow_mut() = Some(service);
    }

    fn area(&self) -> Option<GLArea> {
        self.area.borrow().as_ref().and_then(|w| w.upgrade())
    }

    #[cfg(test)]
    pub(crate) fn is_active(&self) -> bool {
        self.active.get()
    }

    #[cfg(test)]
    pub(crate) fn is_always_on(&self) -> bool {
        self.always_on.get()
    }

    pub(crate) fn fd_state(&self) -> FdState {
        self.fd_state.get().unwrap_or(FdState::Idle)
    }

    /// A new harness exists: it gets a fresh watch on its own fd.
    pub(crate) fn session_started(&self) {
        self.unwatch_fd();
        self.fd_state.set(Some(FdState::Idle));
    }

    /// Registers the tick callback if it is not registered. Never runs the service synchronously.
    pub(crate) fn kick(self: &Rc<Self>) {
        if self.active.get() {
            return;
        }
        let (Some(area), Some(service)) = (self.area(), self.service.borrow().clone()) else {
            return;
        };
        self.active.set(true);
        let driver = Rc::downgrade(self);
        let id = area.add_tick_callback(move |widget, _clock| {
            let need = service(widget, ServiceSource::Tick);
            let Some(driver) = driver.upgrade() else {
                return glib::ControlFlow::Break;
            };
            if need || driver.always_on.get() {
                return glib::ControlFlow::Continue;
            }
            // GTK removes the callback itself on `Break`: forget the id so nothing removes it twice.
            driver.active.set(false);
            driver.tick_id.borrow_mut().take();
            glib::ControlFlow::Break
        });
        *self.tick_id.borrow_mut() = Some(id);
    }

    /// F3: called from the committed render when [`tick_still_wanted`] said no. Removes the tick
    /// there and then; a later `kick` registers it again.
    pub(crate) fn stop_after_render(&self) {
        if self.always_on.get() {
            return;
        }
        if let Some(id) = self.tick_id.borrow_mut().take() {
            id.remove();
        }
        self.active.set(false);
    }

    /// Watches the harness's event loop fd (winit's calloop epoll: readable while an event -- a
    /// redraw batch, a setting change, nvim's exit -- is queued), once per harness.
    pub(crate) fn watch_fd(self: &Rc<Self>, fd: RawFd) {
        if self.fd_state() != FdState::Idle {
            return;
        }
        let driver = Rc::downgrade(self);
        let added = fd_watch::add_local(
            fd,
            glib::IOCondition::IN | glib::IOCondition::ERR | glib::IOCondition::HUP,
            fd_watch::EVENT_LOOP_WATCH_PRIORITY,
            move |condition| {
                let Some(driver) = driver.upgrade() else {
                    return glib::ControlFlow::Break;
                };
                if condition.intersects(glib::IOCondition::ERR | glib::IOCondition::HUP | glib::IOCondition::NVAL) {
                    // Returning Break removes the source: forget it, and go back to ticking.
                    driver.fd_source.borrow_mut().take();
                    driver.fd_state.set(Some(FdState::Released));
                    driver.always_on.set(true);
                    println!("[tick] the event loop fd reported {condition:?}; falling back to the always-on tick");
                    driver.kick();
                    return glib::ControlFlow::Break;
                }
                let (Some(area), Some(service)) = (driver.area(), driver.service.borrow().clone()) else {
                    return glib::ControlFlow::Continue;
                };
                if service(&area, ServiceSource::FdWatch) {
                    driver.kick();
                }
                glib::ControlFlow::Continue
            },
        );
        match added {
            Ok(id) => {
                *self.fd_source.borrow_mut() = Some(id);
                self.fd_state.set(Some(FdState::Watching));
            }
            Err(fd_watch::NotOwner) => {
                println!("[tick] no fd watch (not on the main context's thread); keeping the always-on tick");
                self.fd_state.set(Some(FdState::Released));
                self.always_on.set(true);
                self.kick();
            }
        }
    }

    /// Removes the watch, if any, and keeps it removed for this harness. Call before the harness
    /// (and its fd) goes away; twice is harmless. Also ends the always-on fallback: it polled this
    /// harness in the watch's place, and with the harness gone there is nothing left to poll, so
    /// the tick stops once the service stops wanting it (an exited or failed pane).
    pub(crate) fn unwatch_fd(&self) {
        if let Some(id) = self.fd_source.borrow_mut().take() {
            id.remove();
        }
        self.fd_state.set(Some(FdState::Released));
        self.always_on.set(false);
    }
}

impl Drop for TickDriver {
    /// `LiveSession`'s own drop reaches the driver through a `Weak`, which is already dead when the
    /// driver itself is what is being dropped (the driver's service owns the session): the watch
    /// must go here too, or it outlives the harness's fd.
    fn drop(&mut self) {
        if let Some(id) = self.fd_source.get_mut().take() {
            id.remove();
        }
    }
}

/// `wants_frame` with a side effect: asking for a frame makes sure the tick callback runs. Same
/// `set`/`replace`/`get` as the `Cell<bool>` it replaces.
pub(crate) struct WantsFrame {
    flag: Cell<bool>,
    driver: Weak<TickDriver>,
}

impl WantsFrame {
    pub(crate) fn new(driver: &Rc<TickDriver>) -> Self {
        Self {
            flag: Cell::new(false),
            driver: Rc::downgrade(driver),
        }
    }

    pub(crate) fn set(&self, wanted: bool) {
        self.flag.set(wanted);
        if wanted {
            if let Some(driver) = self.driver.upgrade() {
                driver.kick();
            }
        }
    }

    pub(crate) fn replace(&self, wanted: bool) -> bool {
        self.flag.replace(wanted)
    }

    pub(crate) fn get(&self) -> bool {
        self.flag.get()
    }

    /// The harness this flag belongs to is going away: its fd watch must not outlive it.
    pub(crate) fn release_watch(&self) {
        if let Some(driver) = self.driver.upgrade() {
            driver.unwatch_fd();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inputs() -> TickInputs {
        TickInputs {
            animating: false,
            wants_frame: false,
            exit_pending: false,
            service_pending: false,
        }
    }

    #[test]
    fn the_tick_is_wanted_for_each_reason_alone_and_for_none_it_is_not() {
        assert!(!tick_still_wanted(inputs()));
        assert!(tick_still_wanted(TickInputs {
            animating: true,
            ..inputs()
        }));
        assert!(tick_still_wanted(TickInputs {
            wants_frame: true,
            ..inputs()
        }));
        assert!(tick_still_wanted(TickInputs {
            exit_pending: true,
            ..inputs()
        }));
        assert!(
            tick_still_wanted(TickInputs {
                service_pending: true,
                ..inputs()
            }),
            "pending service work alone keeps the tick, so the service runs once more (plan review finding 1)"
        );
    }

    fn pending() -> PendingInputs {
        PendingInputs {
            exited: false,
            close_requested: false,
            fullscreen_changed: false,
            scale_changed: false,
        }
    }

    #[test]
    fn a_render_that_consumed_an_exit_fullscreen_or_scale_change_asks_for_the_service() {
        assert!(!service_pending_after_render(pending()));
        assert!(service_pending_after_render(PendingInputs {
            exited: true,
            ..pending()
        }));
        assert!(service_pending_after_render(PendingInputs {
            fullscreen_changed: true,
            ..pending()
        }));
        assert!(service_pending_after_render(PendingInputs {
            scale_changed: true,
            ..pending()
        }));
        assert!(!service_pending_after_render(PendingInputs {
            close_requested: true,
            ..pending()
        }));
        assert!(
            !service_pending_after_render(PendingInputs {
                exited: true,
                close_requested: true,
                ..pending()
            }),
            "an exit already reported (or asked for by the host) is not pending"
        );
        assert!(
            service_pending_after_render(PendingInputs {
                exited: true,
                close_requested: true,
                scale_changed: true,
                ..pending()
            }),
            "close_requested only cancels the exit term"
        );
    }

    #[test]
    fn a_pending_exit_keeps_the_tick_through_the_same_chain_the_render_uses() {
        let service_pending = service_pending_after_render(PendingInputs {
            exited: true,
            ..pending()
        });
        assert!(tick_still_wanted(TickInputs {
            service_pending,
            ..inputs()
        }));
        let service_pending = service_pending_after_render(pending());
        assert!(!tick_still_wanted(TickInputs {
            service_pending,
            ..inputs()
        }));
    }

    #[test]
    fn add_local_refuses_a_thread_that_does_not_own_the_default_context() {
        // A libtest worker thread never acquires the default context (the one lib test that does
        // holds it on its own thread), so it is not the owner here either way.
        let result = fd_watch::add_local(0, glib::IOCondition::IN, glib::ffi::G_PRIORITY_DEFAULT_IDLE, |_| {
            glib::ControlFlow::Break
        });
        assert_eq!(result.err(), Some(fd_watch::NotOwner));
    }

    #[test]
    fn a_driver_with_no_area_never_registers_a_tick_and_unwatch_twice_is_harmless() {
        let driver = Rc::new(TickDriver::default());
        driver.kick();
        assert!(!driver.is_active(), "nothing to tick without an area and a service");
        driver.stop_after_render();
        assert!(!driver.is_active());
        driver.unwatch_fd();
        driver.unwatch_fd();
        assert_eq!(driver.fd_state(), FdState::Released);
    }

    #[test]
    fn a_released_watch_stays_released_until_a_new_harness_exists() {
        let driver = Rc::new(TickDriver::default());
        assert_eq!(driver.fd_state(), FdState::Idle);
        driver.unwatch_fd();
        // Not owner in this thread, so watch_fd would fail if it tried; state must not change.
        driver.watch_fd(0);
        assert_eq!(
            driver.fd_state(),
            FdState::Released,
            "no watch on a released harness's fd"
        );
        driver.session_started();
        assert_eq!(driver.fd_state(), FdState::Idle, "a new harness gets a fresh watch");
    }

    /// Review finding (fix round 1): the pane dropped without `shutdown()`/`release_exited()`. The
    /// driver's service owns the `LiveSession`, so when the driver drops, `LiveSession::drop`'s
    /// `Weak` is already dead and removes nothing; the driver must remove the watch itself.
    #[test]
    fn dropping_the_driver_removes_its_fd_watch() {
        let _serial = crate::DEFAULT_CONTEXT_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let context = glib::MainContext::default();
        let _owner = context
            .acquire()
            .expect("tests that run the default main context take DEFAULT_CONTEXT_TEST_LOCK first");
        let (read, _write) = std::os::unix::net::UnixStream::pair().expect("a socket pair");
        let driver = Rc::new(TickDriver::default());
        // The production shape: the driver's service owns what holds the `WantsFrame`.
        let wants = WantsFrame::new(&driver);
        let service: Service = Rc::new(move |_, _| wants.get());
        *driver.service.borrow_mut() = Some(service);
        driver.watch_fd(std::os::fd::AsRawFd::as_raw_fd(&read));
        assert_eq!(driver.fd_state(), FdState::Watching);
        let raw = driver.fd_source.borrow().as_ref().expect("a watch").as_raw();
        drop(driver);
        // SAFETY: a lookup by id on the default context; the returned pointer is only compared.
        let found = unsafe { glib::ffi::g_main_context_find_source_by_id(std::ptr::null_mut(), raw) };
        assert!(
            found.is_null(),
            "the watch outlived its driver (and so the harness's fd)"
        );
    }

    /// Review finding (fix round 2): the always-on fallback exists to poll a harness the watch
    /// could not cover. Once that harness is released there is nothing to poll, so the fallback
    /// must end with it, or an exited pane wakes the frame clock at the refresh rate for good.
    #[test]
    fn the_always_on_fallback_ends_when_its_harness_is_released() {
        let driver = Rc::new(TickDriver::default());
        // Not the default context's owner on a libtest thread: the fallback trips.
        driver.watch_fd(0);
        assert!(driver.is_always_on(), "no watch, so the tick stays on");
        driver.unwatch_fd();
        assert!(!driver.is_always_on(), "a released harness has nothing left to poll");
        driver.watch_fd(0);
        assert!(!driver.is_always_on(), "a released harness is not retried");
        driver.session_started();
        driver.watch_fd(0);
        assert!(
            driver.is_always_on(),
            "a new harness that cannot be watched falls back again"
        );
    }

    #[test]
    fn asking_for_a_frame_with_no_driver_left_is_harmless() {
        let driver = Rc::new(TickDriver::default());
        let wants = WantsFrame::new(&driver);
        drop(driver);
        wants.set(true);
        assert!(wants.get());
        assert!(wants.replace(false));
        wants.release_watch();
    }
}
