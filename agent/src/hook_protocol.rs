//! The JSON contract for the `PreToolUse` hook this crate installs via `claude --settings`
//! (see `crate::settings`) and the `agent-hook` companion binary speaks on stdin/stdout. Real
//! shapes confirmed live against the actual CLI (2026-09-07) -- see
//! docs/superpowers/specs/2026-09-07-agent-v2-streaming-protocol-design.md and
//! agent/tests/fixtures/v2_hook_*.json.

use serde::Deserialize;
use serde_json::json;

/// Only the fields this crate actually needs from the hook's real stdin payload -- the real
/// shape also carries `transcript_path`, `cwd`, `prompt_id`, `effort`, and `hook_event_name`,
/// which are ignored here (serde ignores unknown fields by default).
#[derive(Debug, Deserialize)]
pub struct PreToolUseHookInput {
    pub session_id: String,
    pub tool_name: String,
    pub tool_input: serde_json::Value,
    pub tool_use_id: String,
    /// The CLI's own permission mode at the moment of this call (`"default"` in the captured
    /// fixture). Read since R07 by the D12 tripwire (`process::classify_cli_mode`): a mode less
    /// restrictive than the `default` this crate asks for denies the call and closes the session.
    /// Optional because nothing guarantees every CLI build sends it; an absent value is classified
    /// as unreported, never as `default`.
    #[serde(default)]
    pub permission_mode: Option<String>,
}

pub fn parse_pretooluse_input(json_str: &str) -> Result<PreToolUseHookInput, serde_json::Error> {
    serde_json::from_str(json_str)
}

/// Builds the exact `hookSpecificOutput` JSON the CLI expects back on the hook's stdout --
/// confirmed real and working (both allow and deny) via a live capture, see
/// `agent/tests/fixtures/v2_hook_decision_{allow,deny}.json`. `reason` is only ever attached
/// when `allow` is `false`; the confirmed real allow shape carries no reason field at all.
pub fn format_decision(allow: bool, reason: Option<&str>) -> String {
    let decision = if allow { "allow" } else { "deny" };
    let mut output = json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": decision,
        }
    });
    if let (false, Some(r)) = (allow, reason) {
        output["hookSpecificOutput"]["permissionDecisionReason"] = json!(r);
    }
    output.to_string()
}
