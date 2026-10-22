//! A record that moves a chat to the other window is built by destructuring the state it comes
//! from, without a rest pattern, so a field added to that state does not compile until it is placed
//! in the record or named as not moved. This reads the files that build those records as source text
//! and fails if any such destructure ends in `..`, or if one it guards is gone.

use eitri_core::source_scan::{braced, code_only, read_required, word_ends};

/// Each file that builds a move record, and each struct it destructures there.
const DESTRUCTURES: &[(&str, &str)] = &[
    ("agent/src/move_seed.rs", "AgentSessionProjection"),
    ("agent/src/move_seed.rs", "MoveSeed"),
    ("agent/src/conversation.rs", "AgentConversation"),
    ("agent/src/conversation.rs", "ConversationHandoff"),
    ("agent/src/ingestion.rs", "IngestState"),
    // A Claude chat's sidecar: the provider, its watch's state and steer gate, and the handoff taken up.
    ("agent/src/providers/claude_sidecar/moving.rs", "ClaudeSidecarProvider"),
    ("agent/src/providers/claude_sidecar/moving.rs", "ClaudeProviderHandoff"),
    ("agent/src/providers/claude_sidecar/moving.rs", "WatchMark"),
    ("agent/src/providers/claude_sidecar/mod.rs", "WatchState"),
    ("agent/src/providers/claude_sidecar/steer.rs", "SteerGate"),
];

/// Each file that builds a Codex or ACP provider's move record, and each struct it destructures there,
/// on the way out of the provider and on the way back in.
const PROVIDER_DESTRUCTURES: &[(&str, &str)] = &[
    ("agent/src/providers/codex/provider.rs", "CodexProvider"),
    ("agent/src/providers/codex/provider.rs", "State"),
    ("agent/src/providers/codex/provider.rs", "ActiveTurn"),
    ("agent/src/providers/codex/provider.rs", "SentSteer"),
    ("agent/src/providers/codex/provider.rs", "SentInterrupt"),
    ("agent/src/providers/codex/provider.rs", "CodexProviderHandoff"),
    ("agent/src/providers/codex/provider.rs", "MovedTurn"),
    ("agent/src/providers/codex/provider.rs", "MovedSteer"),
    ("agent/src/providers/codex/provider.rs", "MovedInterrupt"),
    ("agent/src/providers/codex/approvals.rs", "HeldItems"),
    ("agent/src/providers/codex/approvals.rs", "HeldItemsRecord"),
    ("agent/src/providers/acp/provider.rs", "AcpProvider"),
    ("agent/src/providers/acp/provider.rs", "State"),
    ("agent/src/providers/acp/provider.rs", "ActiveTurn"),
    ("agent/src/providers/acp/provider.rs", "AcpProviderHandoff"),
    ("agent/src/providers/acp/provider.rs", "MovedTurn"),
];

/// The braced pattern of every `let NAME {` in `code`.
fn let_patterns<'a>(code: &'a str, name: &str) -> Vec<&'a str> {
    word_ends(code, "let")
        .into_iter()
        .filter_map(|after_let| {
            let rest = &code[after_let..];
            let trimmed = rest.trim_start();
            let named = trimmed.strip_prefix(name)?;
            if named.chars().next().is_some_and(|c| c.is_alphanumeric() || c == '_') {
                return None;
            }
            let brace = named.trim_start();
            if !brace.starts_with('{') {
                return None;
            }
            Some(braced(code, code.len() - brace.len()))
        })
        .collect()
}

/// The patterns among `patterns` that end in a rest pattern.
fn with_rest<'a>(patterns: &[&'a str]) -> Vec<&'a str> {
    patterns.iter().copied().filter(|p| p.contains("..")).collect()
}

#[test]
fn the_handoff_destructures_name_every_field() {
    for (path, name) in DESTRUCTURES {
        let code = code_only(&read_required(path).text);
        let patterns = let_patterns(&code, name);
        assert!(
            !patterns.is_empty(),
            "{path} no longer destructures `{name}`: the move record it built is made somewhere this scan does not read"
        );
        let open = with_rest(&patterns);
        assert!(
            open.is_empty(),
            "{path} destructures `{name}` with a rest pattern, so a new field would move silently or not at all: {open:?}"
        );
    }
}

#[test]
fn the_provider_handoffs_name_every_field() {
    for (path, name) in PROVIDER_DESTRUCTURES {
        let code = code_only(&read_required(path).text);
        let patterns = let_patterns(&code, name);
        assert!(
            !patterns.is_empty(),
            "{path} no longer destructures `{name}`: the provider's move record is made somewhere this scan does not read"
        );
        let open = with_rest(&patterns);
        assert!(
            open.is_empty(),
            "{path} destructures `{name}` with a rest pattern, so a new field of the provider would move silently or not at all: {open:?}"
        );
    }
}

#[test]
fn a_destructure_with_a_rest_pattern_is_caught() {
    let code = "fn f(s: &S) { let S { a, b: _, .. } = s; let Sx { c } = t; let S {\n d,\n e } = s; }";
    let patterns = let_patterns(code, "S");
    assert_eq!(patterns.len(), 2, "`Sx` is another struct");
    assert_eq!(with_rest(&patterns).len(), 1);
}
