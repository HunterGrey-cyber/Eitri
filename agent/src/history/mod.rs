//! Restoring what a conversation already said, so a resumed session does not open onto an empty
//! panel.
//!
//! Design: `docs/superpowers/specs/2026-09-20-resume-history-design.md`. Two sources, ruled on by
//! the owner: **B**, the real Claude CLI's own transcript, is primary; **A**, Neovibe's own copy,
//! is the fallback under it. This module holds B's reader ([`transcript_jsonl`]) and the one piece
//! of text handling B needs that belongs to this project rather than to the CLI
//! ([`composed_block`]).
//!
//! [`store`] is A: Neovibe's own copy, under `$XDG_STATE_HOME/neovibe/history/`.
//!
//! **Nothing here ever writes under `$CLAUDE_CONFIG_DIR`** (invariant 11). B's half never writes,
//! renames, moves or deletes anything there; that directory belongs to the CLI. A's half writes
//! only into Neovibe's own state directory. Nothing either half reads is evidence: history content
//! feeds the panel and nothing else -- it never decides whether a resume is safe, never reaches a
//! permission decision, and is never composed into a turn sent to the model.

pub mod composed_block;
/// Turning either stored record into the projection a resumed conversation starts from (§4, §7, §8).
/// Crate-internal: the only caller is `AgentConversation::resume`, and a seed is not something a
/// consumer of this crate builds for itself.
pub(crate) mod load;
pub mod store;
pub mod transcript_jsonl;

pub use composed_block::strip_composed_block;
pub use store::{HistoryLoadError, StoredHistory, HISTORY_FORMAT_VERSION};
pub use transcript_jsonl::{
    read_transcript_history, HistoryCounts, TranscriptHistory, HISTORY_MAX_CHARS, HISTORY_MAX_ITEMS,
    HISTORY_MAX_LINE_BYTES, HISTORY_SCAN_MAX_BYTES,
};
