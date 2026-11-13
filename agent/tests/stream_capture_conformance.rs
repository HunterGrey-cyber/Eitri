//! Reads `stream.jsonl` in `EITRI_CAPTURE_DIR` through `agent::translate_line`, the parser
//! for the `--output-format stream-json` stdout the legacy backend reads live. A CLI upgrade
//! can add or reshape a line kind without any compile error; this check pins the conversational
//! records the current build must recognize.
//!
//! **`#[ignore]`d, and it spends no tokens itself**: it reads stdout a capture already produced
//! under the TEST profile (the test-account wrapper): a plain turn, a turn that runs `echo`, a turn that
//! thinks. Run it with
//!
//!     EITRI_CAPTURE_DIR=<dir> cargo test -p agent --features legacy-backend \
//!         --test stream_capture_conformance -- --ignored --nocapture
//!
//! Read the printed tallies: no conversational line may remain in `Unknown`, every result must
//! be counted, and every line must be JSON. New non-conversational kinds are printed for inspection.

use std::collections::BTreeMap;
use std::path::PathBuf;

use agent::{translate_line, AgentEvent};

/// `None` (with a line on stderr) when no capture is named, so a blanket `-- --ignored` run skips
/// this test instead of failing it for a missing input.
fn capture_dir() -> Option<PathBuf> {
    match std::env::var("EITRI_CAPTURE_DIR") {
        Ok(dir) if !dir.is_empty() => Some(PathBuf::from(dir)),
        _ => {
            eprintln!("SKIPPED: EITRI_CAPTURE_DIR names no directory holding stream.jsonl and transcript.jsonl");
            None
        }
    }
}

#[test]
#[ignore = "reads a capture made under the TEST profile; see this file's header"]
fn the_captured_stream_json_translates_without_a_parse_failure() {
    let Some(dir) = capture_dir() else { return };
    let text = std::fs::read_to_string(dir.join("stream.jsonl")).expect("stream.jsonl");
    let mut kinds: BTreeMap<String, usize> = BTreeMap::new();
    let mut unknown: BTreeMap<(String, Option<String>), usize> = BTreeMap::new();
    let mut result_lines = 0;
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let value: serde_json::Value = serde_json::from_str(line).expect("every captured line is JSON");
        if value["type"] == "result" {
            result_lines += 1;
        }
        for event in translate_line(line) {
            let name = match &event {
                AgentEvent::SessionStarted { .. } => "SessionStarted",
                AgentEvent::AssistantText { .. } => "AssistantText",
                AgentEvent::Thinking { .. } => "Thinking",
                AgentEvent::ToolStarted { .. } => "ToolStarted",
                AgentEvent::ToolResult { .. } => "ToolResult",
                AgentEvent::TurnFinished { .. } => "TurnFinished",
                AgentEvent::RateLimit { .. } => "RateLimit",
                AgentEvent::Unknown { kind, subtype, .. } => {
                    *unknown.entry((kind.clone(), subtype.clone())).or_default() += 1;
                    "Unknown"
                }
                _ => "other",
            };
            *kinds.entry(name.to_string()).or_default() += 1;
        }
    }
    eprintln!("stream events by kind: {kinds:#?}");
    eprintln!("stream Unknown by (type, subtype): {unknown:#?}");

    for (kind, subtype) in unknown.keys() {
        // A conversational envelope that failed its typed parse comes back as `Unknown` under its own
        // `type`, so the three kinds below are the ones that must never land there; lifecycle
        // notices (`system` with another subtype) and new top-level types are expected to.
        let conversational = matches!(
            kind.as_str(),
            "assistant" | "user" | "result" | "assistant_content_block"
        ) || (kind == "system" && subtype.as_deref() == Some("init"));
        assert!(
            !conversational,
            "a conversational line this parser could not model: {kind} {subtype:?}"
        );
    }
    assert!(result_lines >= 1, "the capture holds no result line");
    assert_eq!(
        kinds.get("TurnFinished").copied().unwrap_or(0),
        result_lines,
        "every `result` line must become a TurnFinished"
    );
    assert!(kinds.contains_key("AssistantText"), "no assistant text came out");
    assert!(kinds.contains_key("ToolStarted"), "no tool call came out");
    assert!(kinds.contains_key("ToolResult"), "no tool result came out");
}
