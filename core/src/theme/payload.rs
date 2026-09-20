//! The one JSON line the embedded nvim writes to the theme socket (see `nvim_theme.lua`).

use std::collections::HashMap;

use serde::Deserialize;

/// Bumped whenever the Lua snippet and this parser change shape together. A mismatch rejects the
/// whole payload rather than guessing at a partial reading.
pub const PAYLOAD_VERSION: u32 = 1;

/// The subset of one `nvim_get_hl(0, {name = ..., link = false})` result that derivation reads.
/// Everything else nvim reports (`bold`, `ctermfg`, ...) is ignored by serde's default.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
pub struct HlAttrs {
    pub fg: Option<u32>,
    pub bg: Option<u32>,
    #[serde(default)]
    pub reverse: bool,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct NvimOptions {
    /// `"light"` or `"dark"`: nvim's `'background'`.
    pub background: String,
    /// nvim's `'guifont'`, verbatim. Empty when unset.
    pub guifont: String,
    /// `g:colors_name`, or empty. Only ever logged.
    pub colors_name: String,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct NvimThemePayload {
    pub v: u32,
    /// Only groups nvim reported as non-empty are present.
    pub groups: HashMap<String, HlAttrs>,
    pub options: NvimOptions,
}

impl NvimThemePayload {
    /// What derivation starts from when nvim has pushed nothing at all.
    pub fn empty() -> Self {
        NvimThemePayload {
            v: PAYLOAD_VERSION,
            groups: HashMap::new(),
            options: NvimOptions {
                background: "dark".to_string(),
                guifont: String::new(),
                colors_name: String::new(),
            },
        }
    }
}

pub(crate) fn parse_payload(line: &str) -> Result<NvimThemePayload, String> {
    let payload: NvimThemePayload =
        serde_json::from_str(line.trim()).map_err(|e| format!("malformed theme payload: {e}"))?;
    if payload.v != PAYLOAD_VERSION {
        return Err(format!(
            "theme payload version {} (this build reads {PAYLOAD_VERSION})",
            payload.v
        ));
    }
    Ok(payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAWN: &str = r#"{"v":1,"groups":{"Normal":{"fg":5722745,"bg":16446701,"reverse":false},"IncSearch":{"reverse":true}},"options":{"background":"light","guifont":"","colors_name":"rose-pine"}}"#;

    #[test]
    fn parses_a_real_shaped_line() {
        let payload = parse_payload(&format!("{DAWN}\n")).expect("valid");
        assert_eq!(payload.options.colors_name, "rose-pine");
        assert_eq!(payload.options.background, "light");
        assert_eq!(
            payload.groups["Normal"],
            HlAttrs {
                fg: Some(0x575279),
                bg: Some(0xfaf4ed),
                reverse: false
            }
        );
        assert_eq!(
            payload.groups["IncSearch"],
            HlAttrs {
                fg: None,
                bg: None,
                reverse: true
            }
        );
    }

    #[test]
    fn ignores_attributes_it_does_not_use() {
        let line = r#"{"v":1,"groups":{"@keyword":{"fg":1,"bold":true,"ctermfg":3,"cterm":{"bold":true}}},"options":{"background":"dark","guifont":"","colors_name":""}}"#;
        assert_eq!(parse_payload(line).expect("valid").groups["@keyword"].fg, Some(1));
    }

    #[test]
    fn rejects_another_version_whole() {
        let err = parse_payload(&DAWN.replace(r#""v":1"#, r#""v":2"#)).unwrap_err();
        assert!(err.contains("version 2"), "{err}");
    }

    #[test]
    fn rejects_malformed_or_incomplete_lines() {
        assert!(parse_payload("").is_err());
        assert!(parse_payload("not json").is_err());
        assert!(parse_payload(r#"{"v":1,"groups":{}}"#).is_err(), "options is required");
        assert!(
            parse_payload(r#"{"v":1,"groups":[],"options":{"background":"dark","guifont":"","colors_name":""}}"#)
                .is_err()
        );
    }

    #[test]
    fn the_empty_payload_is_dark_with_no_groups() {
        let empty = NvimThemePayload::empty();
        assert_eq!(empty.v, PAYLOAD_VERSION);
        assert!(empty.groups.is_empty());
        assert_eq!(empty.options.background, "dark");
    }
}
