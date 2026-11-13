//! Reads `transcript.jsonl` in `EITRI_CAPTURE_DIR` through
//! `agent::history::read_transcript_history`, the transcript parser used by sidecar resume.
//! The input is a capture already made under the TEST profile (the test-account wrapper);
//! this ignored test reads files and spends no tokens itself. Run with
//!
//!     EITRI_CAPTURE_DIR=<dir> cargo test -p agent \
//!         --test transcript_capture_conformance -- --ignored --nocapture
//!
//! The capture contains a plain turn, a tool-using turn and a thinking turn.
//! Printed counts describe the parsed conversation and any unrecognized records.

use std::path::PathBuf;

/// `None` with a skip diagnostic when no transcript capture directory is named, so this test can
/// join a blanket `-- --ignored` run without requiring captured input.
fn capture_dir() -> Option<PathBuf> {
    match std::env::var("EITRI_CAPTURE_DIR") {
        Ok(dir) if !dir.is_empty() => Some(PathBuf::from(dir)),
        _ => {
            eprintln!("SKIPPED: EITRI_CAPTURE_DIR names no directory holding transcript.jsonl");
            None
        }
    }
}

#[test]
#[ignore = "reads a capture made under the TEST profile; see this file's header"]
fn the_captured_transcript_reads_back_as_the_conversation() {
    let Some(dir) = capture_dir() else { return };
    let path = dir.join("transcript.jsonl");
    let history = agent::history::read_transcript_history(&path).expect("transcript.jsonl is readable");
    eprintln!(
        "transcript: writer {:?}, {} parsed items, {} events, counts {:#?}",
        history.writer_version,
        history.parsed_items,
        history.events.len(),
        history.counts
    );
    assert_eq!(
        history.counts.malformed_lines, 0,
        "a captured transcript line is not JSON"
    );
    assert_eq!(history.counts.oversized_lines, 0);
    assert_eq!(
        history.counts.unknown_type_lines, 0,
        "a line type this reader has never seen: see the printed counts, then the reader's classifier"
    );
    assert_eq!(history.counts.orphan_tool_results, 0, "a tool result found no call");
    assert!(
        history.parsed_items > 0,
        "nothing in the transcript became conversation"
    );
    assert!(history.writer_version.is_some(), "no line carried the writer's version");
}
