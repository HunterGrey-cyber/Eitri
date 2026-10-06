//! What the window gives the review flow's editor overlay: who owns the review module in this
//! window's nvim, and what to do once a file was opened there.
//!
//! The overlay itself, its calls and its events are `eitri_core`'s (`ReviewFlow` drives them from
//! the panel's tick, never waiting). This holder only carries the two things that differ between
//! the integrated window and a companion panel, so neither window's code is known to the flow.

use std::rc::Rc;

use eitri_core::review_editor::Owner;
use eitri_core::turn_review::ReviewFlow;

/// Who owns the module in the editor now: asked again before every request, because a companion
/// panel's channel changes with each attach. `None`: there is no editor to show a review in.
pub type OwnerSource = Rc<dyn Fn() -> Option<Owner>>;

/// Brings the editor to the user once a file was opened in it.
pub type OpenedHook = Rc<dyn Fn()>;

/// The window's side of the editor overlay.
#[derive(Default)]
pub struct ReviewEditor {
    owner: Option<OwnerSource>,
    opened: Option<OpenedHook>,
}

impl ReviewEditor {
    pub fn set_owner(&mut self, owner: OwnerSource) {
        self.owner = Some(owner);
    }

    pub fn set_opened(&mut self, hook: OpenedHook) {
        self.opened = Some(hook);
    }

    /// Tells `flow` who owns the module now. A window that gave no owner has no editor overlay.
    pub fn sync(&self, flow: &mut ReviewFlow) {
        flow.set_editor_owner(self.owner.as_ref().and_then(|owner| owner()));
    }

    /// The hook to run after a file opened, if the window gave one. Returned rather than called so
    /// the caller can run it with no borrow of the panel's state held.
    pub fn opened_hook(&self) -> Option<OpenedHook> {
        self.opened.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn a_window_that_names_no_owner_has_no_editor_overlay() {
        let editor = ReviewEditor::default();
        let mut flow = ReviewFlow::new();
        flow.set_editor_owner(Some(Owner::Embedded));
        editor.sync(&mut flow);
        // The flow was told there is no owner: its own overlay is gone with it (the flow's tests
        // show what an open then answers).
        assert!(flow.is_idle());
    }

    #[test]
    fn the_owner_is_asked_on_every_sync() {
        let asked = Rc::new(Cell::new(0u32));
        let mut editor = ReviewEditor::default();
        let counter = asked.clone();
        editor.set_owner(Rc::new(move || {
            counter.set(counter.get() + 1);
            Some(Owner::Companion {
                channel: u64::from(counter.get()),
            })
        }));
        let mut flow = ReviewFlow::new();
        editor.sync(&mut flow);
        editor.sync(&mut flow);
        assert_eq!(asked.get(), 2);
    }

    #[test]
    fn the_opened_hook_is_handed_out_not_run() {
        let ran = Rc::new(Cell::new(false));
        let mut editor = ReviewEditor::default();
        assert!(editor.opened_hook().is_none());
        let flag = ran.clone();
        editor.set_opened(Rc::new(move || flag.set(true)));
        assert!(!ran.get());
        (editor.opened_hook().expect("a hook"))();
        assert!(ran.get());
    }
}
