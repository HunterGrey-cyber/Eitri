//! What the oracle may print about a conversation, and how.
//!
//! The oracle runs on real sessions, so by default it prints only structure: counts, positions, words from
//! closed lists compiled in here, built-in tool names, the key names of a built-in tool's input, and keyed
//! digests. A digest is the first 10 hex characters of HMAC-SHA256 under a key drawn from `/dev/urandom`
//! for each run and never printed or stored: two equal values in one report have equal digests, digests
//! from two runs cannot be compared, and a short text such as "yes" cannot be recovered by hashing guesses.
//!
//! Never printed while redacting: the text of any message, tool input values, tool results, paths, model
//! ids, host and user names, URLs, names and bodies of peers, workflow names and scripts, error strings,
//! timestamps and the input files' paths. Every value that is not one of the closed words below prints as
//! `other`, a tool name not on the built-in list as `tool:<digest>`, and an input key that does not look
//! like a schema key as `key:<digest>`.
//!
//! With `--show-text` (the [`Redactor::showing_text`] form) text and values print as they are.

use serde_json::Value;
use sha2::{Digest, Sha256};
use std::io::Read;

/// HMAC-SHA256 (RFC 2104) over `message` under `key`, built on `sha2` alone.
pub fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut block_key = [0u8; BLOCK];
    if key.len() > BLOCK {
        block_key[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        block_key[..key.len()].copy_from_slice(key);
    }
    let mut inner = Sha256::new();
    inner.update(block_key.map(|b| b ^ 0x36));
    inner.update(message);
    let inner = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(block_key.map(|b| b ^ 0x5c));
    outer.update(inner);
    let mut mac = [0u8; 32];
    mac.copy_from_slice(&outer.finalize());
    mac
}

/// Tools the pinned CLI offers by name, and its other built-in tools: their names say nothing about the
/// person. Any other name, an MCP tool's included, prints as a digest.
pub const BUILT_IN_TOOLS: &[&str] = &[
    "Agent",
    "AskUserQuestion",
    "Bash",
    "BashOutput",
    "CronCreate",
    "CronDelete",
    "CronList",
    "Edit",
    "EnterPlanMode",
    "EnterWorktree",
    "ExitPlanMode",
    "ExitWorktree",
    "Glob",
    "Grep",
    "KillShell",
    "LSP",
    "ListAgents",
    "Monitor",
    "MultiEdit",
    "NotebookEdit",
    "NotebookRead",
    "Read",
    "ReportFindings",
    "ScheduleWakeup",
    "SendMessage",
    "Skill",
    "SubagentHandback",
    "Task",
    "TaskOutput",
    "TaskStop",
    "TodoWrite",
    "ToolSearch",
    "WebFetch",
    "WebSearch",
    "Workflow",
    "Write",
];

/// The words a report may print from its inputs: line types and subtypes, attachment types, origin kinds,
/// prompt sources, command modes, queue operations, task statuses, stop reasons, compaction triggers, and
/// the reader's own words for what it skipped. A value outside this list prints as `other`.
pub const ENUM_WORDS: &[&str] = &[
    // line types
    "user",
    "assistant",
    "system",
    "attachment",
    "queue-operation",
    "file-history-snapshot",
    "summary",
    "progress",
    "result",
    "api-request",
    "api-request-blob",
    "api-request-shape",
    "atis-latch",
    "cost-state",
    "last-prompt",
    "mode",
    "tools-used",
    "custom-title",
    // system subtypes
    "compact_boundary",
    "microcompact_boundary",
    "local_command",
    "turn_duration",
    "stop_hook_summary",
    "away_summary",
    "api_error",
    "informational",
    "init",
    "status",
    "task_notification",
    "task_started",
    "task_progress",
    "hook_response",
    // attachment types
    "agent_listing_delta",
    "auto_mode",
    "date",
    "deferred_tools_delta",
    "deferred_tools_record",
    "environment",
    "model",
    "prompt_snapshot",
    "queued_command",
    "remote_session_change",
    "session_context",
    "silent_turn_reminder",
    "skill_listing",
    "total_tokens_reminder",
    "todo_reminder",
    "plan_mode",
    "hook_success",
    "hook_additional_context",
    "edited_text_file",
    "file",
    "nested_memory",
    "diagnostics",
    "critical_system_reminder",
    "task_status",
    // origin kinds, prompt sources, command modes
    "human",
    "peer",
    "task-notification",
    "scheduled",
    "scheduled-trigger",
    "compact-continue",
    "typed",
    "suggestion_accepted",
    "queued",
    "sdk",
    "prompt",
    "bash",
    // queue operations
    "enqueue",
    "dequeue",
    "remove",
    "popAll",
    "absorbed_mid_turn",
    // task statuses and kinds
    "running",
    "completed",
    "failed",
    "killed",
    "stopped",
    "pending",
    "paused",
    "local_bash",
    "local_agent",
    "monitor",
    "workflow",
    // stop reasons and triggers
    "end_turn",
    "tool_use",
    "max_tokens",
    "stop_sequence",
    "pause_turn",
    "refusal",
    "manual",
    "auto",
    // what the reader says about lines it skipped, and the ends of turns it names
    "(no type)",
    "(malformed)",
    "-",
    "isMeta",
    "isCompactSummary",
    "isSidechain",
    "taskDelivery",
    "origin",
    "block",
    "no-content",
    "block-repeated",
    "local-command-output",
    "text_and_tool_result",
    "tool_result",
    "ended_to_deliver",
    "text",
    "thinking",
    "redacted_thinking",
    "image",
    "document",
    "limit_reached",
    "interrupted",
    "variant",
];

/// Decides how each value of a report is printed.
#[derive(Clone)]
pub struct Redactor {
    key: [u8; 32],
    show_text: bool,
}

impl std::fmt::Debug for Redactor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The key never leaves the process.
        f.debug_struct("Redactor")
            .field("show_text", &self.show_text)
            .finish_non_exhaustive()
    }
}

impl Redactor {
    /// Redacting, under a key drawn from `/dev/urandom` for this run.
    pub fn from_urandom() -> std::io::Result<Self> {
        let mut key = [0u8; 32];
        std::fs::File::open("/dev/urandom")?.read_exact(&mut key)?;
        Ok(Self::with_key(key))
    }

    /// Redacting under `key`: for tests that compare a report with a stored one.
    pub fn with_key(key: [u8; 32]) -> Self {
        Self { key, show_text: false }
    }

    /// The same digests, with text and values printed as they are.
    pub fn showing_text(self) -> Self {
        Self {
            show_text: true,
            ..self
        }
    }

    pub fn shows_text(&self) -> bool {
        self.show_text
    }

    /// The first 10 hex characters of the value's keyed digest.
    pub fn digest(&self, value: &str) -> String {
        let mac = hmac_sha256(&self.key, value.as_bytes());
        mac[..5].iter().map(|b| format!("{b:02x}")).collect()
    }

    /// An id (tool use, message, task, session): `id:<digest>`, or the id itself when showing text.
    pub fn id(&self, id: &str) -> String {
        if self.show_text {
            id.to_owned()
        } else {
            format!("id:{}", self.digest(id))
        }
    }

    /// A text: `N chars sha:<digest>`, N in UTF-16 units; when showing text, the first 60 characters quoted
    /// and the length.
    pub fn text(&self, text: &str) -> String {
        let chars = utf16_len(text);
        if self.show_text {
            format!("{} ({chars} chars)", quote_cut(text, 60))
        } else {
            format!("{chars} chars sha:{}", self.digest(text))
        }
    }

    /// A word read from an input: itself when it is on the closed list (or when showing text), else
    /// `other`.
    pub fn word<'a>(&self, word: &'a str) -> &'a str {
        if self.show_text || ENUM_WORDS.contains(&word) {
            word
        } else {
            "other"
        }
    }

    /// A count's key built from input words joined by `/` and `:`: each word through [`Self::word`].
    pub fn count_key(&self, key: &str) -> String {
        if self.show_text {
            return key.to_owned();
        }
        let mut out = String::new();
        let mut word = String::new();
        for c in key.chars() {
            if c == '/' || c == ':' {
                out.push_str(self.word(&word));
                out.push(c);
                word.clear();
            } else {
                word.push(c);
            }
        }
        out.push_str(self.word(&word));
        out
    }

    /// A tool's name: itself when built in (or when showing text), else `tool:<digest>`.
    pub fn tool(&self, name: &str) -> String {
        if self.show_text || BUILT_IN_TOOLS.contains(&name) {
            name.to_owned()
        } else {
            format!("tool:{}", self.digest(name))
        }
    }

    /// A tool's input: a built-in tool's top-level key names, sorted; any other tool's key count only. When
    /// showing text, the input as canonical JSON, cut to 80 characters.
    pub fn input(&self, tool: &str, input: &Value) -> String {
        if self.show_text {
            let json = super::normalise::canonical_json(input);
            return cut(&json, 80);
        }
        let Value::Object(map) = input else {
            return "(not an object)".to_owned();
        };
        if !BUILT_IN_TOOLS.contains(&tool) {
            return format!("{{{} keys}}", map.len());
        }
        let mut keys: Vec<String> = map
            .keys()
            .map(|k| {
                if schema_key(k) {
                    k.clone()
                } else {
                    format!("key:{}", self.digest(k))
                }
            })
            .collect();
        keys.sort();
        format!("{{{}}}", keys.join(", "))
    }

    /// A tool input's key names as a list, for the JSON report: `None` where only a count may be printed.
    pub fn input_keys(&self, tool: &str, input: &Value) -> Option<Vec<String>> {
        let Value::Object(map) = input else {
            return Some(Vec::new());
        };
        if !self.show_text && !BUILT_IN_TOOLS.contains(&tool) {
            return None;
        }
        let mut keys: Vec<String> = map
            .keys()
            .map(|k| {
                if self.show_text || schema_key(k) {
                    k.clone()
                } else {
                    format!("key:{}", self.digest(k))
                }
            })
            .collect();
        keys.sort();
        Some(keys)
    }
}

/// Whether a key looks like a schema's key name and not like a value: ASCII letters, digits and `_`,
/// starting with a letter or `_`, at most 40 long.
fn schema_key(key: &str) -> bool {
    let mut chars = key.chars();
    key.len() <= 40
        && chars.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// A text's length in UTF-16 units, the unit an editor and a browser count in.
pub fn utf16_len(text: &str) -> usize {
    text.encode_utf16().count()
}

/// `text` cut to `limit` characters, with `…` where it was cut.
fn cut(text: &str, limit: usize) -> String {
    let mut out: String = text.chars().take(limit).collect();
    if text.chars().count() > limit {
        out.push('…');
    }
    out
}

/// `text` cut to `limit` characters and quoted as a JSON string, with `…` inside the quotes where it was
/// cut.
fn quote_cut(text: &str, limit: usize) -> String {
    let shown: String = text.chars().take(limit).collect();
    let mut quoted = Value::String(shown).to_string();
    if text.chars().count() > limit {
        quoted.insert(quoted.len() - 1, '…');
    }
    quoted
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn hmac_matches_the_rfc_4231_vectors() {
        let long_key = [0xaau8; 131];
        let cases: Vec<(Vec<u8>, Vec<u8>, &str)> = vec![
            (
                vec![0x0b; 20],
                b"Hi There".to_vec(),
                "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7",
            ),
            (
                b"Jefe".to_vec(),
                b"what do ya want for nothing?".to_vec(),
                "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843",
            ),
            (
                vec![0xaa; 20],
                vec![0xdd; 50],
                "773ea91e36800e46854db8ebd09181a72959098b3ef8c122d9635514ced565fe",
            ),
            (
                (1u8..=25).collect(),
                vec![0xcd; 50],
                "82558a389a443c0ea4cc819899f2083a85f0faa3e578f8077a2e3ff46729665b",
            ),
            (
                long_key.to_vec(),
                b"Test Using Larger Than Block-Size Key - Hash Key First".to_vec(),
                "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54",
            ),
            (
                long_key.to_vec(),
                b"This is a test using a larger than block-size key and a larger than block-size data. The key \
                  needs to be hashed before being used by the HMAC algorithm."
                    .to_vec(),
                "9b09ffa71b942fcb27635fbcd5b0e944bfdc63644f0713938a7f51535c3a35e2",
            ),
        ];
        for (key, message, expected) in cases {
            assert_eq!(hex(&hmac_sha256(&key, &message)), expected);
        }
        // The vector whose output is truncated to 128 bits.
        let truncated = hmac_sha256(&[0x0c; 20], b"Test With Truncation");
        assert_eq!(hex(&truncated[..16]), "a3b6167473100ee06e0c796c2955552b");
    }

    #[test]
    fn a_digest_is_ten_hex_characters_and_depends_on_the_key() {
        let a = Redactor::with_key([1; 32]);
        let b = Redactor::with_key([2; 32]);
        assert_eq!(a.digest("yes").len(), 10);
        assert!(a.digest("yes").chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(a.digest("yes"), a.digest("yes"));
        assert_ne!(a.digest("yes"), b.digest("yes"));
    }

    #[test]
    fn words_off_the_closed_list_are_other() {
        let r = Redactor::with_key([0; 32]);
        assert_eq!(r.word("queued_command"), "queued_command");
        assert_eq!(r.word("my-secret-host"), "other");
        assert_eq!(
            r.count_key("user:origin/a secret word"),
            "user:origin/other",
            "each part of a compound key is checked on its own"
        );
        assert_eq!(
            r.count_key("attachment/queued_command:prompt/-/isMeta"),
            "attachment/queued_command:prompt/-/isMeta"
        );
        assert_eq!(r.clone().showing_text().word("my-secret-host"), "my-secret-host");
    }

    #[test]
    fn texts_print_as_utf16_length_and_digest() {
        let r = Redactor::with_key([0; 32]);
        let printed = r.text("héllo 🙂");
        assert!(printed.starts_with("8 chars sha:"), "{printed}");
        assert!(!printed.contains("llo"));
        let shown = r.showing_text().text("héllo 🙂");
        assert_eq!(shown, "\"héllo 🙂\" (8 chars)");
    }

    #[test]
    fn inputs_print_built_in_keys_sorted_and_others_as_a_count() {
        let r = Redactor::with_key([0; 32]);
        let input = json!({"run_in_background": true, "command": "ls ~/secret", "description": "x"});
        assert_eq!(r.input("Bash", &input), "{command, description, run_in_background}");
        assert_eq!(r.input("mcp__server__tool", &input), "{3 keys}");
        assert!(r.tool("mcp__server__tool").starts_with("tool:"));
        let odd = json!({"/home/someone/file": 1});
        assert!(
            r.input("Read", &odd).starts_with("{key:"),
            "a key shaped like a value is a digest"
        );
    }
}
