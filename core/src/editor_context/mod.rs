//! What the editor is showing, sent to the agent.
//!
//! [`compose`] is pure and decides what the model reads. The transport follows
//! `crate::theme::feed`: nvim pushes over one Unix socket and the host polls.

pub mod compose;
pub mod feed;

pub use compose::{compose_turn_text, EditorContext, Selection, CONTENT_LIMIT, TRUNCATION_MARKER};

/// The inverse of [`compose_turn_text`], re-exported here so both directions are available
/// together. Defined in `agent::history::composed_block` because this crate depends on
/// `agent`, whose transcript reader needs the inverse. Defining it here would create a
/// dependency cycle. [`compose`] tests `strip(compose(t, ctx)) == t` beside the forward
/// cases.
pub use agent::history::strip_composed_block;

/// Provides the editor context at send time through a closure, keeping the socket out of the panel.
/// Tests can return a constant; production reads the cache filled by `shell::editor_context`.
/// The call must return immediately because it runs on the GTK main loop between a keypress and
/// sending a turn. RPC or a whole-buffer read would delay input; the latter can take 33 ms.
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
