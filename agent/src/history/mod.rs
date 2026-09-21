//! Restores conversation history so a resumed session can display earlier messages.
//!
//! The Claude CLI transcript, B, is primary. The application's stored copy, A, is the
//! fallback. [`transcript_jsonl`] reads B, [`composed_block`] removes appended editor
//! context from prompts, and [`store`] manages A in the application's state directory.
//!
//! The CLI directory, `$CLAUDE_CONFIG_DIR`, is read-only here: nothing writes, renames,
//! moves or deletes files there. The stored copy is written only in application state.
//! Restored content feeds the panel; it does not determine resume safety or permissions,
//! and it is never composed into a turn sent to the model.

pub mod composed_block;
/// Builds the initial projection for a resumed conversation from either history source.
/// Crate-internal: only `AgentConversation::resume` calls this; crate consumers do not build seeds.
pub(crate) mod load;
pub mod store;
pub mod transcript_jsonl;

pub use composed_block::strip_composed_block;
pub use store::{HistoryLoadError, StoredHistory, HISTORY_FORMAT_VERSION};
pub use transcript_jsonl::{
    read_transcript_history, HistoryCounts, TranscriptHistory, HISTORY_MAX_CHARS, HISTORY_MAX_ITEMS,
    HISTORY_MAX_LINE_BYTES, HISTORY_SCAN_MAX_BYTES,
};
