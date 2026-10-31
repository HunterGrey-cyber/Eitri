//! The window's `:EitriTutor` (`eitri_core::tutor`): asked for at each editor session the feeds report, and
//! polled from a GLib timer only while a call to nvim is out, so an idle window is never woken by it.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Instant;

use eitri_core::editor_rpc::EditorRpc;
use eitri_core::tutor::TutorInstaller;
use gtk4::glib;

pub(crate) struct Tutor {
    installer: RefCell<TutorInstaller>,
    timer: Cell<Option<glib::SourceId>>,
}

impl Tutor {
    pub(crate) fn new() -> Rc<Tutor> {
        Rc::new(Tutor {
            installer: RefCell::new(TutorInstaller::new()),
            timer: Cell::new(None),
        })
    }

    /// The window's prefix, as the lessons name it; set once the config is read, before the first editor session.
    pub(crate) fn set_prefix(&self, human: String) {
        self.installer.borrow_mut().set_prefix(human);
    }

    /// The window's one editor handle.
    pub(crate) fn set_editor(&self, editor: Rc<dyn EditorRpc>) {
        self.installer.borrow_mut().set_editor(editor);
    }

    /// The feeds reported: the editor may have a new nvim session ready.
    pub(crate) fn editor_session(self: &Rc<Self>) {
        let next = self.installer.borrow_mut().editor_session(Instant::now());
        self.arm(next);
    }

    /// The window's close: the lessons' directory goes with it.
    pub(crate) fn cleanup(&self) {
        if let Some(id) = self.timer.take() {
            id.remove();
        }
        self.installer.borrow().cleanup();
    }

    fn arm(self: &Rc<Self>, next: Option<Instant>) {
        if let Some(id) = self.timer.take() {
            id.remove();
        }
        let Some(at) = next else { return };
        let wait = at.saturating_duration_since(Instant::now());
        let weak = Rc::downgrade(self);
        let id = glib::timeout_add_local_once(wait, move || {
            let Some(tutor) = weak.upgrade() else { return };
            // This source has run and is gone; only a newer one may be removed from here on.
            tutor.timer.set(None);
            let next = tutor.installer.borrow_mut().poll(Instant::now());
            tutor.arm(next);
        });
        self.timer.set(Some(id));
    }
}
