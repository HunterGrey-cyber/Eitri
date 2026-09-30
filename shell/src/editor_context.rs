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
    let cache: Rc<RefCell<Option<EditorContext>>> = Rc::new(RefCell::new(None));
    let Some(listener) = feed.take_listener() else {
        eprintln!("[editor-context] listen() called twice -- turns will carry no editor context");
        return Rc::new(|| None);
    };
    let mut reader = EditorContextReader::new(listener);
    let writer = Rc::clone(&cache);
    glib::timeout_add_local(POLL_INTERVAL, move || {
        if let Some(context) = reader.poll() {
            let scratch = scratch_dir
                .as_deref()
                .is_some_and(|dir| eitri_core::scratch::holds(dir, &context.file));
            if !scratch {
                *writer.borrow_mut() = Some(context);
            }
        }
        glib::ControlFlow::Continue
    });
    Rc::new(move || cache.borrow().clone())
}
