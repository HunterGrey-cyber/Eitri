//! The JSON contract for the `PreToolUse` hook installed via `claude --settings` by
//! `crate::settings` and spoken on stdin/stdout by the `agent-hook` companion binary.
//! CLI payload examples are in `agent/tests/fixtures/v2_hook_*.json`.

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
    /// The CLI's permission mode at the time of this call, `"default"` in the fixture.
    /// Enforcement uses `process::classify_cli_mode` to deny the call and close the session if this
    /// mode is less restrictive than the requested `default`. Optional because some CLI builds may omit it;
    /// an absent value is classified as unreported, never as `default`.
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
