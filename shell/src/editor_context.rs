//! The GTK-free feed is `eitri_core::editor_context::feed`, read by `eitri_core::editor_feeds` and
//! driven by `shell::editor_feeds::FeedPump`; what stays here is the gate a window puts in front of
//! the source it hands the panel.

use std::rc::Rc;

pub(crate) use eitri_core::editor_context::feed::EditorContextFeed;
use eitri_core::editor_context::ContextSource;

/// `inner` while `open` is set, nothing otherwise: a detached editor's file and selection must not
/// ride along on a turn.
pub(crate) fn gated(inner: ContextSource, open: Rc<std::cell::Cell<bool>>) -> ContextSource {
    Rc::new(move || if open.get() { inner() } else { None })
}

#[cfg(test)]
mod tests {
    use super::*;
    use eitri_core::editor_context::EditorContext;
    use std::cell::RefCell;

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
