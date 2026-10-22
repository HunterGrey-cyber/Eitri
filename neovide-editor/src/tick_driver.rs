//! Who runs the pane's "service" work (pump nvim, watch fullscreen/scale, resync the grid, report
//! an exit, queue a render), and when.
//!
//! Before 2026-09-29 a `GtkGLArea` tick callback stayed registered for the pane's whole life. With
//! nothing just drawn, GDK runs such a callback on its 16.667 ms fallback interval, so a redraw
//! batch that reached the fork's event loop waited 0-16.7 ms (8.5 ms at p50) before anything looked
//! at it: the whole of the insert-mode typing latency gap measured against native Neovide that day.
//!
//! Now: the one watch on the editors' shared event loop (`live_watch`) runs the pane's service the
//! moment a batch arrives, and the tick callback is registered only while a frame is wanted. Three
//! rules keep that safe:
//!
//! - **F1:** the fd watch runs at `G_PRIORITY_DEFAULT_IDLE` (200), below `GDK_PRIORITY_REDRAW`
//!   (120), so a flood of batches can never starve a paint (of this pane, the terminal or the
//!   panel). It only services and kicks; `render_frame` pumps on every paint anyway.
//! - **F2:** a render that pumps may consume an exit, a fullscreen change or a scale change that
//!   only the service reports; [`service_pending_after_render`] asks for one more service run.
//! - **F3:** the tick stops in the committed render ([`TickDriver::stop_after_render`]) rather than
//!   one empty cycle later, so a key landing right after an animation does not wait a refresh.

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};

use gtk4::glib;
use gtk4::prelude::*;
use gtk4::{GLArea, TickCallbackId};

/// What keeps the tick callback registered for a pane with a live harness. See
/// [`tick_still_wanted`]. Startup is not an input here: before a harness exists the service
/// decides by `sessionless_tick_should_render` (`NotStarted`/`Starting` keep the tick), and the
/// first `Ready` render cannot stop it before the pane's harness is registered with the watch
/// (`LiveSession::new` registers it), since `LiveSession` starts with `last_animating` true.
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

/// Which of the two drivers ran the service: the tick callback, or the event-loop fd watch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ServiceSource {
    Tick,
    FdWatch,
}

/// The pane's service work, told which driver runs it; returns whether another tick cycle is wanted.
pub(crate) type Service = Rc<dyn Fn(&GLArea, ServiceSource) -> bool>;

/// Owns the tick callback registration, and runs the pane's service when the event-loop watch
/// (`live_watch`) wakes it. Everything runs on the GTK main thread.
#[derive(Default)]
pub(crate) struct TickDriver {
    active: Cell<bool>,
    tick_id: RefCell<Option<TickCallbackId>>,
    area: RefCell<Option<glib::WeakRef<GLArea>>>,
    service: RefCell<Option<Service>>,
    /// Set when the watch cannot be had: the tick then stays on, as it did before this driver,
    /// until that harness is released ([`TickDriver::session_ended`]).
    always_on: Cell<bool>,
}

impl TickDriver {
    pub(crate) fn attach(&self, area: &GLArea, service: Service) {
        *self.area.borrow_mut() = Some(area.downgrade());
        *self.service.borrow_mut() = Some(service);
    }

    fn area(&self) -> Option<GLArea> {
        self.area.borrow().as_ref().and_then(glib::WeakRef::upgrade)
    }

    #[cfg(test)]
    pub(crate) fn is_active(&self) -> bool {
        self.active.get()
    }

    #[cfg(test)]
    pub(crate) fn is_always_on(&self) -> bool {
        self.always_on.get()
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

    /// The event-loop watch woke this pane (or a sweep after a pump made outside it): runs the
    /// service as the watch's, and keeps the tick if the service wants more frames.
    pub(crate) fn service_from_fd(self: &Rc<Self>) {
        let (Some(area), Some(service)) = (self.area(), self.service.borrow().clone()) else {
            return;
        };
        if service(&area, ServiceSource::FdWatch) {
            self.kick();
        }
    }

    /// No watch can wake this pane (not on the main context's thread, or the shared fd failed):
    /// the tick stays registered, polling the harness every frame, until its session ends.
    pub(crate) fn keep_ticking(self: &Rc<Self>) {
        self.always_on.set(true);
        self.kick();
    }

    /// The harness this driver served is shut down or released. Ends the always-on fallback: it
    /// polled that harness in the watch's place, and with the harness gone there is nothing left to
    /// poll, so the tick stops once the service stops wanting it (an exited or failed pane). Twice
    /// is harmless.
    pub(crate) fn session_ended(&self) {
        self.always_on.set(false);
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

    /// The harness this flag belongs to is going away: its driver stops any always-on fallback.
    pub(crate) fn session_ended(&self) {
        if let Some(driver) = self.driver.upgrade() {
            driver.session_ended();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fd_watch;

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
    fn a_driver_with_no_area_never_registers_a_tick_and_ending_twice_is_harmless() {
        let driver = Rc::new(TickDriver::default());
        driver.kick();
        assert!(!driver.is_active(), "nothing to tick without an area and a service");
        driver.stop_after_render();
        assert!(!driver.is_active());
        driver.service_from_fd();
        assert!(!driver.is_active(), "a wake with nothing to service registers no tick");
        driver.session_ended();
        driver.session_ended();
        assert!(!driver.is_always_on());
    }

    /// The always-on fallback exists to poll a harness no watch could cover. Once that harness is
    /// released there is nothing to poll, so the fallback must end with it, or an exited pane wakes
    /// the frame clock at the refresh rate for good.
    #[test]
    fn the_always_on_fallback_ends_when_its_harness_is_released() {
        let driver = Rc::new(TickDriver::default());
        driver.keep_ticking();
        assert!(driver.is_always_on(), "no watch, so the tick stays on");
        driver.session_ended();
        assert!(!driver.is_always_on(), "a released harness has nothing left to poll");
    }

    #[test]
    fn asking_for_a_frame_with_no_driver_left_is_harmless() {
        let driver = Rc::new(TickDriver::default());
        let wants = WantsFrame::new(&driver);
        drop(driver);
        wants.set(true);
        assert!(wants.get());
        assert!(wants.replace(false));
        wants.session_ended();
    }
}
