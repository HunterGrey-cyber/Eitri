//! What a phone is shown of a tab before its snapshot, and the window's colours it follows.
//!
//! The greeting is about the one tab the phone follows ([`crate::tab_set::TabSet::remote_greeting`] builds it): the
//! tab as the strip shows it, a trust or bypass question it waits on (what the question says; nothing that answers it,
//! and the phone answers neither), `root_note`, the agent's live notices, whether a `!` line runs, the theme and why
//! the phone may not send to the tab. Nothing about the window or the project beyond that: no project path, no list of
//! sessions to resume, no account, no saved-rule offers, no drafts, no focus or editor state, no key map.
//!
//! The theme is the window's colour tokens (`--nv-*` colours, each a `#rrggbb` value) and its scheme (`light` or
//! `dark`); never a font name or a size, which describe the desk's screen and its user's fonts, not the look.

use serde_json::{json, Map, Value};

pub use crate::remote_rv::SendBlock;
use crate::remote_rv::{RvOutbound, RV_PROTOCOL};
use crate::tab_set::RowKey;
use crate::theme::ThemeTokens;

/// The fields of a greeting, in the order [`greeting_json`] writes them; nothing else is in it.
pub const GREETING_FIELDS: [&str; 7] = [
    "tab",
    "prompt",
    "root_note",
    "notices",
    "shell_running",
    "theme",
    "send_block",
];

/// What a phone is shown of a tab before its snapshot.
#[derive(Debug, Clone, PartialEq)]
pub struct Greeting {
    /// The tab's entry as the desk's tab strip shows it, without the desk's own question drafts.
    pub tab: Value,
    /// The question the tab waits on, for display.
    pub prompt: Option<PendingPrompt>,
    /// Why the window's project folder is no boundary, when it is not one.
    pub root_note: Option<String>,
    /// The agent's live notices (a retry, a rate-limit warning, a notification).
    pub notices: Vec<String>,
    /// Whether a `!` line the desk started runs in the tab.
    pub shell_running: bool,
    /// The window's colours and scheme ([`remote_theme`]).
    pub theme: Value,
    /// Why the phone may not send to the tab now, if it may not.
    pub send_block: Option<SendBlock>,
}

/// A question a tab waits on, as a phone shows it: never its nonce, its digest or anything an answer would echo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PendingPrompt {
    /// The tab waits for the desk to trust the project's configuration before its session starts.
    Trust,
    /// The desk asks whether the tab enters bypass; the lines the desk shows.
    Bypass { lines: Vec<String> },
}

/// The greeting as a JSON object with exactly [`GREETING_FIELDS`].
pub fn greeting_value(greeting: &Greeting) -> Value {
    let prompt = match &greeting.prompt {
        None => Value::Null,
        Some(PendingPrompt::Trust) => json!({ "kind": "trust" }),
        Some(PendingPrompt::Bypass { lines }) => json!({ "kind": "bypass", "lines": lines }),
    };
    json!({
        "tab": greeting.tab,
        "prompt": prompt,
        "root_note": greeting.root_note,
        "notices": greeting.notices,
        "shell_running": greeting.shell_running,
        "theme": greeting.theme,
        "send_block": greeting.send_block,
    })
}

/// The greeting's JSON object as text.
pub fn greeting_json(greeting: &Greeting) -> String {
    greeting_value(greeting).to_string()
}

/// The `greeting` line about the tab `key`, without its newline.
pub fn greeting_line(key: &RowKey, greeting: &Greeting) -> String {
    RvOutbound::Greeting {
        v: RV_PROTOCOL,
        key: key.as_str().to_owned(),
        greeting: greeting_value(greeting),
    }
    .line()
}

/// `{"vars":{"--nv-bg":"#1e1e2e",...},"scheme":"dark"}`: every `--nv-*` colour token of the window, and the scheme.
/// Only values that are colours are taken, so a font stack or a size never leaves the desk.
pub fn remote_theme(tokens: &ThemeTokens) -> Value {
    let vars: Map<String, Value> = tokens
        .appearance_vars()
        .into_iter()
        .filter(|(name, value)| name.starts_with("--nv-") && is_hex_colour(value))
        .map(|(name, value)| (name, Value::String(value)))
        .collect();
    json!({ "vars": vars, "scheme": tokens.color_scheme })
}

/// The `theme` line every connection is sent when the window's colours change, without its newline.
pub fn theme_line(theme: &Value) -> String {
    RvOutbound::Theme {
        v: RV_PROTOCOL,
        theme: theme.clone(),
    }
    .line()
}

fn is_hex_colour(value: &str) -> bool {
    let Some(digits) = value.strip_prefix('#') else {
        return false;
    };
    matches!(digits.len(), 3 | 4 | 6 | 8) && digits.bytes().all(|b| b.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_theme_is_colours_and_the_scheme_only() {
        let tokens = ThemeTokens::fallback();
        let theme = remote_theme(&tokens);
        let vars = theme["vars"].as_object().expect("the colours");
        assert!(vars.contains_key("--nv-bg") && vars.contains_key("--nv-fg"), "{theme}");
        assert!(vars.values().all(|v| v.as_str().is_some_and(is_hex_colour)), "{theme}");
        assert!(
            !vars
                .keys()
                .any(|k| k.starts_with("--nv-font") || k == "--nv-color-scheme"),
            "no font and no scheme among the colours: {theme}"
        );
        assert_eq!(theme["scheme"], tokens.color_scheme);
        assert_eq!(theme.as_object().unwrap().len(), 2, "{theme}");
        let line: Value = serde_json::from_str(&theme_line(&theme)).unwrap();
        assert_eq!((&line["kind"], &line["theme"]), (&json!("theme"), &theme));
    }
}
