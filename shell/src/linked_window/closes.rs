//! Window 1's side of [`CloseBurst`]: when a module window's close takes effect on what is kept for its next start. The
//! tabs file and the compositor record go on naming a closed window until the burst it belongs to has ended, so a logout
//! that closes every window leaves both as they were, and whatever ends window 1 meanwhile leaves them too.

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};
use std::time::Instant;

use eitri_core::window_link::close_burst::CloseBurst;
use eitri_core::window_link::names::WindowName;

use super::host_loop::PairClock;
use super::registry::ModuleWindows;

/// Told of each window that is closed for good, after its tabs have left the tabs file.
pub type OnForgotten = Rc<dyn Fn(&WindowName)>;

pub struct WindowCloses {
    burst: RefCell<CloseBurst>,
    /// A timer is running that will look at the burst.
    timing: Cell<bool>,
    modules: Weak<RefCell<ModuleWindows>>,
    forgotten: RefCell<Option<OnForgotten>>,
}

impl WindowCloses {
    pub fn new(modules: Weak<RefCell<ModuleWindows>>) -> Rc<WindowCloses> {
        Rc::new(WindowCloses {
            burst: RefCell::default(),
            timing: Cell::new(false),
            modules,
            forgotten: RefCell::new(None),
        })
    }

    pub fn on_forgotten(&self, hook: OnForgotten) {
        *self.forgotten.borrow_mut() = Some(hook);
    }

    /// A linked module window said it was closed.
    pub fn window_closed(self: &Rc<Self>, name: WindowName, clock: &Rc<dyn PairClock>) {
        let ended = self.burst.borrow_mut().window_closed(name, clock.now());
        self.forget_all(ended);
        self.watch(clock);
    }

    /// Window 1 was asked to close.
    pub fn group_asked(self: &Rc<Self>, clock: &Rc<dyn PairClock>) {
        let ended = self.burst.borrow_mut().group_asked(clock.now());
        self.forget_all(ended);
        self.watch(clock);
    }

    /// A window is about to start under `name`: it starts empty if the window of that name was closed.
    pub fn reopening(&self, name: &WindowName) {
        if self.burst.borrow_mut().reopened(name) {
            self.forget(name);
        }
    }

    fn watch(self: &Rc<Self>, clock: &Rc<dyn PairClock>) {
        if self.timing.get() {
            return;
        }
        let Some(due) = self.burst.borrow().due_in(clock.now()) else {
            return;
        };
        self.timing.set(true);
        let closes = self.clone();
        let again = clock.clone();
        clock.after(
            due,
            Box::new(move || {
                closes.timing.set(false);
                closes.settle(again.now());
                closes.watch(&again);
            }),
        );
    }

    pub(crate) fn settle(&self, now: Instant) {
        let closed = self.burst.borrow_mut().settle(now);
        self.forget_all(closed);
    }

    fn forget_all(&self, closed: Vec<WindowName>) {
        for name in closed {
            self.forget(&name);
        }
    }

    fn forget(&self, name: &WindowName) {
        if let Some(modules) = self.modules.upgrade() {
            modules.borrow_mut().tabs_window_closed(name);
        }
        let hook = self.forgotten.borrow().clone();
        if let Some(hook) = hook {
            hook(name);
        }
    }
}
