//! wire 1 -- what the editor is showing, told to the agent.
//!
//! Split the way `theme` is: [`compose`] is pure and decides what the model reads; the transport is
//! a push protocol over one Unix socket, nvim writing and the host polling, following
//! `crate::theme::feed`'s shape. See
//! `docs/superpowers/specs/2026-09-18-wire1-editor-context-design.md`.

pub mod compose;
pub mod feed;

pub use compose::{compose_turn_text, EditorContext, Selection, CONTENT_LIMIT, TRUNCATION_MARKER};

/// What a turn composer asks, at send time, for where the user is.
///
/// A closure rather than the feed itself, so the agent panel never learns that a socket exists: it
/// asks a question and gets an answer or `None`. A test hands it a constant; production hands it a
/// read of the cache `shell::editor_context`'s poller fills. Reading it must be instant -- the call
/// happens on the GTK main loop between a keypress and a turn going out, which is not a place to
/// perform RPC. See the spec's cost measurement: a whole-buffer read there is 33ms.
pub type ContextSource = std::rc::Rc<dyn Fn() -> Option<EditorContext>>;
