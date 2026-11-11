//! Observers and source controls for the display-isolated integration tests.

use super::*;

thread_local! {
    static ENDING_HELD: Cell<bool> = const { Cell::new(false) };
    static ENDING_SOURCE: RefCell<Option<glib::Source>> = const { RefCell::new(None) };
}

pub fn hold_ending_timer() {
    ENDING_HELD.with(|held| held.set(true));
}

pub(crate) fn ending_source(id: &glib::SourceId) {
    if ENDING_HELD.with(|held| held.get()) {
        let source = glib::MainContext::default().find_source_by_id(id).unwrap();
        set_ready(&source, -1);
        ENDING_SOURCE.with(|slot| *slot.borrow_mut() = Some(source));
    }
}

fn set_ready(source: &glib::Source, ready: i64) {
    // The context is owned by this test's thread; no dispatch can race the readiness change.
    unsafe { glib::ffi::g_source_set_ready_time(glib::translate::ToGlibPtr::to_glib_none(source).0, ready) };
}

pub fn fire_ending_timer() {
    ENDING_SOURCE.with(|slot| set_ready(slot.borrow().as_ref().expect("an ending timer was armed"), 0));
    ENDING_HELD.with(|held| held.set(false));
}

pub fn ending_timer_finished() -> bool {
    ENDING_SOURCE.with(|slot| slot.borrow().as_ref().is_some_and(glib::Source::is_destroyed))
}

impl NeovideEditorPane {
    pub fn testing_idle(&self) -> bool {
        let live = self.live_state.borrow();
        match &*live {
            LiveState::Ready(session) => {
                !session.last_animating.get()
                    && !session.wants_frame.get()
                    && !self.tick_driver.is_active()
                    && !self.testing_remap_pending.get()
            }
            LiveState::Exited | LiveState::Failed(_) => !self.tick_driver.is_active(),
            _ => false,
        }
    }

    pub fn testing_closed_observer(&self) -> impl Fn() -> bool {
        let live = Rc::downgrade(&self.live_state);
        move || {
            live.upgrade()
                .is_none_or(|live| matches!(&*live.borrow(), LiveState::Exited | LiveState::Failed(_)))
        }
    }

    pub fn testing_redraw_batches(&self) -> u64 {
        match &*self.live_state.borrow() {
            LiveState::Ready(session) => session.harness.redraw_batches_seen(),
            _ => 0,
        }
    }

    pub fn testing_event_fd(&self) -> Option<std::os::fd::RawFd> {
        match &*self.live_state.borrow() {
            LiveState::Ready(session) => Some(session.harness.event_loop_fd()),
            _ => None,
        }
    }
}

/// Temporarily removes fd dispatch so a queued paint can consume a batch first.
/// Calling the returned closure restores the watch without servicing any pane.
pub fn hold_event_loop_watch() -> impl FnOnce() {
    live_watch::hold_watch()
}
