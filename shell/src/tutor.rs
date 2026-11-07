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

    /// The resolved mode at startup, before the first editor session.
    pub(crate) fn set_mode(&self, mode: eitri_core::compositor::mode::Mode) {
        self.installer.borrow_mut().set_mode(mode);
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

#[cfg(test)]
mod tests {
    #[test]
    fn tutor_mode_is_set_from_resolved_tiling_before_editor_sessions() {
        use eitri_core::source_scan::{function_in, read_required};
        let sources = [read_required("shell/src/main.rs")];
        let (_, body) = function_in(&sources, "build_ui");
        let body: String = body.chars().filter(|c| !c.is_whitespace()).collect();
        let mode = body
            .find("tutor.set_mode(")
            .expect("the tutor receives the window's mode");
        let resolved = body.find("let(tiling,resolved)=").expect("resolved window mode");
        let first_session = body.find("tutor.editor_session(").expect("first editor session");
        assert!(resolved < mode && mode < first_session);
        use eitri_core::compositor::mode::Mode;
        use eitri_core::editor_rpc::EditorRpc;
        use eitri_core::nvim_rpc::Pending;
        use eitri_core::tutor::{files, TutorDir, TutorInstaller, DEFAULT_PREFIX, INSTALL_TUTOR_LUA};
        use rmpv::Value;
        use std::cell::RefCell;
        use std::rc::Rc;
        use std::time::Instant;

        #[derive(Default)]
        struct Editor(RefCell<Option<Value>>);
        impl EditorRpc for Editor {
            fn target(&self) -> Option<u64> {
                Some(1)
            }
            fn exec_lua(&self, code: &'static str, args: Vec<Value>) -> Pending {
                assert_eq!(code, INSTALL_TUTOR_LUA);
                self.0.replace(Some(args[0].clone()));
                Pending::pair().1
            }
        }
        // The lessons' base: beside the test binary in the target dir, not `/tmp` (a build host's `/tmp` is a
        // tmpfs whose inodes every suite shares), removed when the test ends however it ends.
        struct Base(std::path::PathBuf);
        impl Drop for Base {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let exe = std::env::current_exe().unwrap();
        let base = Base(
            exe.parent()
                .and_then(std::path::Path::parent)
                .unwrap()
                .join("tutor-tests")
                .join(format!("shell-mode-{}", uuid::Uuid::new_v4().simple())),
        );
        std::fs::create_dir_all(&base.0).unwrap();
        for mode in [Mode::Eitri, Mode::Compositor] {
            let tutor = super::Tutor::new();
            let dir = base.0.clone();
            *tutor.installer.borrow_mut() = TutorInstaller::with_dir(Box::new(move || TutorDir::in_dir(&dir).ok()));
            let editor = Rc::new(Editor::default());
            tutor.set_mode(mode);
            tutor.set_editor(editor.clone());
            // Polling and GLib timers are independent of which lessons the first call carries.
            tutor.installer.borrow_mut().editor_session(Instant::now());
            let sent = editor.0.borrow();
            let opts = sent.as_ref().expect("the first session was asked").as_map().unwrap();
            let sent = opts
                .iter()
                .find(|(k, _)| k.as_str() == Some("files"))
                .unwrap()
                .1
                .as_map()
                .unwrap();
            for (path, text) in files(DEFAULT_PREFIX, Some(mode)) {
                assert_eq!(
                    sent.iter().find(|(k, _)| k.as_str() == Some(path)).unwrap().1.as_str(),
                    Some(text.as_str())
                );
            }
            tutor.cleanup();
        }

        assert!(
            body[mode..first_session].contains("Tiling::Hub"),
            "the tutor uses resolved tiling, including linked windows"
        );
    }
}
