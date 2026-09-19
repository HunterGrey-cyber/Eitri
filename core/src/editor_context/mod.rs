//! wire 1 -- what the editor is showing, told to the agent.
//!
//! Split the way `theme` is: [`compose`] is pure and decides what the model reads; the transport is
//! a push protocol over one Unix socket, nvim writing and the host polling, following
//! `crate::theme::feed`'s shape. See
//! `docs/superpowers/specs/2026-09-18-wire1-editor-context-design.md`.

pub mod compose;

pub use compose::{compose_turn_text, EditorContext, Selection, CONTENT_LIMIT, TRUNCATION_MARKER};
