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
