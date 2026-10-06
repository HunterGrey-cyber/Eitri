//! wire 1 -- what the editor is showing, told to the agent.
//!
//! Split the way `theme` is: [`compose`] is pure and decides what the model reads; the transport is
//! a push protocol over one Unix socket, nvim writing and the host polling, following
//! `crate::theme::feed`'s shape. See
//! `docs/superpowers/specs/2026-09-18-wire1-editor-context-design.md`.

pub mod compose;
pub mod feed;

pub use compose::{compose_turn_text, EditorContext, Selection, CONTENT_LIMIT, TRUNCATION_MARKER};

/// The inverse of [`compose_turn_text`], re-exported here so that the two directions are reachable
/// from one place, as the resume-history design (§3.1.2) specifies.
///
/// It is *defined* in `agent::history::composed_block` rather than in [`compose`] for a reason the
/// crate graph forces: `eitri-core` depends on `agent`, and the transcript reader that needs the
/// inverse lives in `agent`, so defining it here would require `agent -> eitri-core -> agent`.
/// The property that ties the two together -- `strip(compose(t, ctx)) == t` -- is asserted in
/// [`compose`]'s own tests, beside the cases for the forward direction.
pub use agent::history::strip_composed_block;

/// What a turn composer asks, at send time, for where the user is.
///
/// A closure rather than the feed itself, so the agent panel never learns that a socket exists: it
/// asks a question and gets an answer or `None`. A test hands it a constant; production hands it a
/// read of the cache `shell::editor_context`'s poller fills. Reading it must be instant -- the call
/// happens on the GTK main loop between a keypress and a turn going out, which is not a place to
/// perform RPC. See the spec's cost measurement: a whole-buffer read there is 33ms.
pub type ContextSource = std::rc::Rc<dyn Fn() -> Option<EditorContext>>;

/// `inner` while `open` is set, nothing otherwise: a detached editor's file and selection must not
/// ride along on a turn.
pub fn gated(inner: ContextSource, open: std::rc::Rc<std::cell::Cell<bool>>) -> ContextSource {
    std::rc::Rc::new(move || if open.get() { inner() } else { None })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    fn context(file: &str) -> EditorContext {
        EditorContext {
            file: file.to_owned(),
            selection: None,
        }
    }

    fn file(source: &ContextSource) -> Option<String> {
        source().map(|context| context.file)
    }

    #[test]
    fn a_detached_editor_sends_no_context_and_a_new_one_starts_empty() {
        let held: Rc<RefCell<Option<EditorContext>>> = Rc::default();
        let inner: ContextSource = {
            let held = held.clone();
            Rc::new(move || held.borrow().clone())
        };
        let open = Rc::new(std::cell::Cell::new(false));
        let source = gated(inner, open.clone());

        *held.borrow_mut() = Some(context("/p/a.rs"));
        assert_eq!(file(&source), None, "closed: nothing, though the source holds A");
        open.set(true);
        assert_eq!(file(&source), Some("/p/a.rs".to_owned()));
        open.set(false);
        *held.borrow_mut() = None;
        open.set(true);
        assert_eq!(file(&source), None, "the next editor starts empty");
        *held.borrow_mut() = Some(context("/p/b.rs"));
        assert_eq!(file(&source), Some("/p/b.rs".to_owned()));
    }
}
