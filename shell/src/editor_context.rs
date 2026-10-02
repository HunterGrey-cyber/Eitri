//! The GTK half of wire 1: a `glib` timer that drains the editor-context socket
//! `eitri_core::editor_context::feed` owns, and a cache the agent panel reads at send time.
//!
//! Split exactly like `shell::theme::feed`: everything portable -- the socket, the snippet, the
//! parsing, the Lua<->Rust contract -- lives in `eitri-core`, and what stays here is the part
//! that names `gtk4`.

use std::cell::RefCell;
use std::rc::Rc;

use gtk4::glib;

pub(crate) use eitri_core::editor_context::feed::EditorContextFeed;
use eitri_core::editor_context::feed::{EditorContextReader, POLL_INTERVAL};
use eitri_core::editor_context::{ContextSource, EditorContext};

/// Starts draining the feed and returns the source the panel reads.
///
/// A tick with no line leaves the cache alone. The reader returning `None` means no update:
/// nvim writes only when something changes. Clearing the cache on an empty tick would lose
/// the selection roughly 150 ms later, just as the user moves to the panel to ask about it.
///
/// Takes the feed's listener. A second call logs and returns an always-empty source instead
/// of installing another timer that races the first for connections.
///
/// Reports naming buffers in the per-window `scratch_dir` are ignored. Drafts and scratch
/// views must leave the context on the file the user was in before opening them.
pub(crate) fn listen(feed: &mut EditorContextFeed, scratch_dir: Option<std::path::PathBuf>) -> ContextSource {
    listen_resettable(feed, scratch_dir).0
}

/// What the timer and the panel's source share: the last context that arrived.
#[derive(Clone, Default)]
struct ContextCache(Rc<RefCell<Option<EditorContext>>>);

impl ContextCache {
    /// Keeps `context` unless it names a scratch buffer of `scratch_dir`.
    fn store(&self, context: EditorContext, scratch_dir: Option<&std::path::Path>) {
        let scratch = scratch_dir.is_some_and(|dir| eitri_core::scratch::holds(dir, &context.file));
        if !scratch {
            *self.0.borrow_mut() = Some(context);
        }
    }

    fn reset(&self) {
        *self.0.borrow_mut() = None;
    }

    fn source(&self) -> ContextSource {
        let cache = self.0.clone();
        Rc::new(move || cache.borrow().clone())
    }
}

/// [`listen`], plus a way to forget what the cache holds and what the socket has not yet delivered.
/// A companion window attaches to editors that come and go, and the cache otherwise keeps the last
/// one's file and selection for good.
pub(crate) fn listen_resettable(
    feed: &mut EditorContextFeed,
    scratch_dir: Option<std::path::PathBuf>,
) -> (ContextSource, Rc<dyn Fn()>) {
    let cache = ContextCache::default();
    let Some(listener) = feed.take_listener() else {
        eprintln!("[editor-context] listen() called twice -- turns will carry no editor context");
        return (Rc::new(|| None), Rc::new(|| {}));
    };
    let reader = Rc::new(RefCell::new(EditorContextReader::new(listener)));
    let writer = cache.clone();
    let timer_reader = reader.clone();
    glib::timeout_add_local(POLL_INTERVAL, move || {
        if let Some(context) = timer_reader.borrow_mut().poll() {
            writer.store(context, scratch_dir.as_deref());
        }
        glib::ControlFlow::Continue
    });
    let reset = cache.clone();
    // The reader forgets what is queued or half read as well: a line the old editor wrote just
    // before it was left would otherwise land in the cache after the reset and ride on a turn.
    (
        cache.source(),
        Rc::new(move || {
            reader.borrow_mut().discard();
            reset.reset();
        }),
    )
}

/// `inner` while `open` is set, nothing otherwise: a detached editor's file and selection must not
/// ride along on a turn.
pub(crate) fn gated(inner: ContextSource, open: Rc<std::cell::Cell<bool>>) -> ContextSource {
    Rc::new(move || if open.get() { inner() } else { None })
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let cache = ContextCache::default();
        let open = Rc::new(std::cell::Cell::new(false));
        let source = gated(cache.source(), open.clone());

        cache.store(context("/p/a.rs"), None);
        assert_eq!(file(&source), None, "closed: nothing, though the cache holds A");
        open.set(true);
        assert_eq!(file(&source), Some("/p/a.rs".to_owned()));
        open.set(false);
        cache.reset();
        assert_eq!(file(&source), None);
        open.set(true);
        assert_eq!(file(&source), None, "the next editor starts empty");
        cache.store(context("/p/b.rs"), None);
        assert_eq!(file(&source), Some("/p/b.rs".to_owned()));
    }

    #[test]
    fn a_scratch_buffer_never_becomes_the_context() {
        let cache = ContextCache::default();
        cache.store(context("/p/a.rs"), Some(std::path::Path::new("/tmp/s")));
        cache.store(context("/tmp/s/1-draft.md"), Some(std::path::Path::new("/tmp/s")));
        assert_eq!(file(&cache.source()), Some("/p/a.rs".to_owned()));
    }
}
