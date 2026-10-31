//! The lessons say "your prefix" with the placeholder `{prefix}`, filled in when the lessons are installed: the
//! window's prefix may be `Ctrl+b`, a tmux user's `Ctrl+a`, or anything else the key table accepts, and the
//! tutorial should name the key the reader has to press.
//!
//! One function does it for every text that goes into the runtime directory: the `.tutor` text is substituted as
//! it is, and in each `.tutor.json` only the `expect` values are (the check numbers and `translated-from` are not
//! text for the reader), parsed and written again so a prefix holding `"` or `\` still leaves valid JSON. The
//! lessons are wrapped in the source: every line stays within 80 columns when `{prefix}` is a spelling of ten
//! columns (`Ctrl+Space`, `Ctrl+Alt+x`), and installing never wraps again, so a check's line number is the same for
//! every prefix. A longer spelling is substituted as it is and may run a line past 80 columns.
//!
//! A key the lessons name literally (`Ctrl+b` in the sample `init.lua` lines, nvim's own page-back key, the text
//! saying what the stock prefix is) is not a placeholder and is never touched.

/// What the lessons write wherever they mean the window's prefix.
pub const PLACEHOLDER: &str = "{prefix}";

/// `text` with every [`PLACEHOLDER`] replaced by `prefix`, the prefix as a person reads it (`KeySpec::human`).
/// Nothing else is rewritten.
pub fn substitute(text: &str, prefix: &str) -> String {
    text.replace(PLACEHOLDER, prefix)
}

/// A `.tutor.json` with the [`PLACEHOLDER`] of each `expect` value replaced by `prefix`. Text that is not JSON (the
/// lessons' own files always are) comes back unchanged.
pub fn substitute_checks(checks: &str, prefix: &str) -> String {
    let Ok(mut json) = serde_json::from_str::<serde_json::Value>(checks) else {
        return checks.to_string();
    };
    if let Some(expect) = json.get_mut("expect").and_then(serde_json::Value::as_object_mut) {
        for want in expect.values_mut() {
            if let Some(text) = want.as_str() {
                *want = serde_json::Value::String(substitute(text, prefix));
            }
        }
    }
    match serde_json::to_string_pretty(&json) {
        Ok(mut out) => {
            out.push('\n');
            out
        }
        Err(_) => checks.to_string(),
    }
}
