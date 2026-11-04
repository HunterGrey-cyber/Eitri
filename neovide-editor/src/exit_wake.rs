//! One-shot wakes on the default main context for a pane whose nvim the fork reported gone while its process still
//! ran: one when that process exits, and one when it has been given long enough to.
//!
//! A [`Wake`] runs its `fire` at most once, its source taking itself out of the wake before `fire` runs, so `fire` may
//! drop the wake (and whatever owns it). A wake dropped before it fired removes its source; a pane drops its wakes
//! before the pidfd one may watch can close (`fd_watch`'s contract).

use std::cell::RefCell;
use std::os::fd::RawFd;
use std::rc::Rc;
use std::time::Duration;

use gtk4::glib;

use crate::fd_watch;

/// How often a wake with no file descriptor to watch asks whether the process is still alive.
pub const EXIT_POLL: Duration = Duration::from_millis(100);

/// A source on the default main context that fires once. Every constructor must run on the thread that owns that
/// context, which is the thread `fire` runs on.
pub struct Wake {
    slot: Rc<RefCell<Option<glib::SourceId>>>,
}

impl Wake {
    /// Fires once `exit_fd` polls readable -- a pidfd does once its process has exited -- or, with no fd or no watch to
    /// be had on it, once `alive` answers `false` at a poll every [`EXIT_POLL`].
    pub fn on_exit(exit_fd: Option<RawFd>, alive: impl Fn() -> bool + 'static, fire: impl Fn() + 'static) -> Wake {
        let slot = Rc::new(RefCell::new(None));
        let once = Once::new(&slot, fire);
        let watched = exit_fd.and_then(|fd| {
            let once = once.clone();
            fd_watch::add_local(
                fd,
                glib::IOCondition::IN | glib::IOCondition::HUP | glib::IOCondition::ERR,
                glib::ffi::G_PRIORITY_DEFAULT,
                move |_| {
                    once.fire();
                    glib::ControlFlow::Break
                },
            )
            .ok()
        });
        let id = watched.unwrap_or_else(|| {
            glib::timeout_add_local(EXIT_POLL, move || {
                if alive() {
                    return glib::ControlFlow::Continue;
                }
                once.fire();
                glib::ControlFlow::Break
            })
        });
        *slot.borrow_mut() = Some(id);
        Wake { slot }
    }

    /// Fires once, `delay` from now.
    pub fn after(delay: Duration, fire: impl Fn() + 'static) -> Wake {
        let slot = Rc::new(RefCell::new(None));
        let once = Once::new(&slot, fire);
        let id = glib::timeout_add_local(delay, move || {
            once.fire();
            glib::ControlFlow::Break
        });
        *slot.borrow_mut() = Some(id);
        Wake { slot }
    }

    /// Whether the wake has yet to fire.
    pub fn pending(&self) -> bool {
        self.slot.borrow().is_some()
    }
}

impl Drop for Wake {
    fn drop(&mut self) {
        if let Some(id) = self.slot.borrow_mut().take() {
            id.remove();
        }
    }
}

/// What a wake's source runs: the source leaves the wake first, since glib removes it on `Break` and the wake must
/// not remove it a second time, and since `fire` may drop the wake.
#[derive(Clone)]
struct Once {
    slot: std::rc::Weak<RefCell<Option<glib::SourceId>>>,
    fire: Rc<dyn Fn()>,
}

impl Once {
    fn new(slot: &Rc<RefCell<Option<glib::SourceId>>>, fire: impl Fn() + 'static) -> Once {
        Once {
            slot: Rc::downgrade(slot),
            fire: Rc::new(fire),
        }
    }

    fn fire(&self) {
        if let Some(slot) = self.slot.upgrade() {
            slot.borrow_mut().take();
        }
        (self.fire)();
    }
}
