//! The GTK half of wire 1: a `glib` timer that drains the editor-context socket
//! `neovibe_core::editor_context::feed` owns, and a cache the agent panel reads at send time.
//!
//! Split exactly like `shell::theme::feed`: everything portable -- the socket, the snippet, the
//! parsing, the Lua<->Rust contract -- lives in `neovibe-core`, and what stays here is the part
//! that names `gtk4`.

use std::cell::RefCell;
use std::rc::Rc;

use gtk4::glib;

pub(crate) use neovibe_core::editor_context::feed::EditorContextFeed;
use neovibe_core::editor_context::feed::{EditorContextReader, POLL_INTERVAL};
use neovibe_core::editor_context::{ContextSource, EditorContext};

/// Starts draining the feed and returns the source the panel reads.
///
/// **A tick that brings no line leaves the cache alone.** The reader returning `None` means
/// "no news", not "no context": nvim only writes when something changed, so forgetting on an empty
/// tick would drop the user's selection roughly 150ms after they made it -- which is exactly when
/// they are moving their hand to the panel to ask about it.
///
/// Takes the feed's listener, so a second call logs and returns a source that is always empty,
/// rather than installing a second timer racing the first for every connection.
pub(crate) fn listen(feed: &mut EditorContextFeed) -> ContextSource {
    let cache: Rc<RefCell<Option<EditorContext>>> = Rc::new(RefCell::new(None));
    let Some(listener) = feed.take_listener() else {
        eprintln!("[editor-context] listen() called twice -- turns will carry no editor context");
        return Rc::new(|| None);
    };
    let mut reader = EditorContextReader::new(listener);
    let writer = Rc::clone(&cache);
    glib::timeout_add_local(POLL_INTERVAL, move || {
        if let Some(context) = reader.poll() {
            *writer.borrow_mut() = Some(context);
        }
        glib::ControlFlow::Continue
    });
    Rc::new(move || cache.borrow().clone())
}
