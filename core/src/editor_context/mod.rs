//! What the editor is showing, sent to the agent.
//!
//! [`compose`] is pure and decides what the model reads. The transport follows
//! `crate::theme::feed`: nvim pushes over one Unix socket and the host polls.

pub mod compose;

pub use compose::{compose_turn_text, EditorContext, Selection, CONTENT_LIMIT, TRUNCATION_MARKER};
