//! Restoring what a conversation already said, so a resumed session does not open onto an empty
//! panel.
//!
//! Two sources: **B**, the real Claude CLI's own transcript, is primary, because it is the record
//! the CLI itself keeps and so the nearer to what the resumed model remembers; **A**, Eitri's own copy,
//! is the fallback under it. This module holds B's reader ([`transcript_jsonl`]) and the one piece
//! of text handling B needs that belongs to this project rather than to the CLI
//! ([`composed_block`]).
//!
//! [`store`] is A: Eitri's own copy, under `$XDG_STATE_HOME/eitri/history/`.
//!
//! **Nothing here ever writes under `$CLAUDE_CONFIG_DIR`**. B's half never writes,
//! renames, moves or deletes anything there; that directory belongs to the CLI. A's half writes
//! only into Eitri's own state directory. Nothing either half reads is evidence: history content
//! feeds the panel and nothing else -- it never decides whether a resume is safe, never reaches a
//! permission decision, and is never composed into a turn sent to the model.

pub mod archive;
pub mod composed_block;
/// Turning either stored record into the projection a resumed conversation starts from. Crate-internal except
/// [`seed_from_tail`] and [`seed_from_stored`]: a conversation resumed on another machine is seeded by a consumer that
/// holds the transcript's bytes (or none) and no local file.
pub(crate) mod load;
pub mod store;
pub mod transcript_jsonl;

pub use composed_block::strip_composed_block;
pub use load::{seed_from_stored, seed_from_tail};
pub use store::{HistoryLoadError, HistoryParts, StoredHistory, HISTORY_FORMAT_VERSION};
pub use transcript_jsonl::{
    read_transcript_history, HistoryCounts, TranscriptHistory, HISTORY_MAX_CHARS, HISTORY_MAX_ITEMS,
    HISTORY_MAX_LINE_BYTES, HISTORY_SCAN_MAX_BYTES,
};

pub(crate) static HISTORY_TURNS: std::sync::OnceLock<usize> = std::sync::OnceLock::new();

/// Pins the settled-turn budget before any session starts; later windows keep the first value.
pub fn configure_history_turns(turns: usize) {
    configure_turn_budget(&HISTORY_TURNS, turns, |line| eprintln!("{line}"));
}

fn configure_turn_budget(lock: &std::sync::OnceLock<usize>, turns: usize, log: impl FnOnce(String)) {
    assert!((10..=2000).contains(&turns));
    let kept = *lock.get_or_init(|| turns);
    if kept != turns {
        log(format!(
            "[history] keeping process history_turns={kept} (requested {turns})"
        ));
    }
}

/// Settled turns kept in memory and replayed by ACP, defaulting to 200 before configuration.
pub fn history_turns() -> usize {
    turn_budget(&HISTORY_TURNS)
}

pub(crate) fn turn_budget(lock: &std::sync::OnceLock<usize>) -> usize {
    lock.get()
        .copied()
        .unwrap_or(crate::providers::acp::history::HISTORY_TURN_LIMIT)
}

#[cfg(test)]
mod budget_tests {
    #[test]
    fn default_budget_and_first_configuration_are_stable() {
        let lock = std::sync::OnceLock::new();
        assert_eq!(super::turn_budget(&lock), 200);
        super::configure_turn_budget(&lock, 10, |_| {});
        super::configure_turn_budget(&lock, 20, |_| {});
        assert_eq!(super::turn_budget(&lock), 10);
    }

    #[test]
    fn only_a_different_later_budget_logs_the_kept_value() {
        let lock = std::sync::OnceLock::new();
        let mut logs = Vec::new();
        for turns in [10, 10, 20, 10] {
            super::configure_turn_budget(&lock, turns, |line| logs.push(line));
        }
        assert_eq!(super::turn_budget(&lock), 10);
        assert_eq!(logs, vec!["[history] keeping process history_turns=10 (requested 20)"]);
    }
}
