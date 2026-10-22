//! Startup settings for the page's keyboard kit: scroll distance and animation, prefix waiting,
//! which-key delay, hint letters and find case handling. They reach the page only in `host.hello`
//! because they cannot change while the window runs.

use serde::{Deserialize, Serialize};

pub const SCROLL_STEP_KEY: &str = "plugins.keys.scroll_step";
pub const SMOOTH_KEY: &str = "plugins.keys.smooth";
pub const PREFIX_WAIT_KEY: &str = "plugins.keys.prefix_wait";
pub const WHICH_KEY_DELAY_KEY: &str = "plugins.keys.which_key_delay";
pub const HINT_CHARS_KEY: &str = "plugins.keys.hint_chars";
pub const FIND_CASE_KEY: &str = "plugins.keys.find_case";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FindCase {
    #[default]
    Smart,
    Ignore,
    Match,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeysSettings {
    pub scroll_step: u32,
    pub smooth: bool,
    pub prefix_wait: bool,
    pub which_key_delay: Option<u32>,
    pub hint_chars: String,
    pub find_case: FindCase,
}

impl Default for KeysSettings {
    fn default() -> Self {
        Self {
            scroll_step: 60,
            smooth: true,
            prefix_wait: true,
            which_key_delay: Some(200),
            hint_chars: "sadfjklewcmpgh".into(),
            find_case: FindCase::Smart,
        }
    }
}

pub fn from_config(get: impl Fn(&str) -> Option<String>) -> Result<KeysSettings, String> {
    let mut settings = KeysSettings::default();
    for key in [
        SCROLL_STEP_KEY,
        SMOOTH_KEY,
        PREFIX_WAIT_KEY,
        WHICH_KEY_DELAY_KEY,
        HINT_CHARS_KEY,
        FIND_CASE_KEY,
    ] {
        let Some(raw) = get(key) else { continue };
        let value = raw.trim();
        if value.is_empty() {
            continue;
        }
        let error = |what| format!("eitri.config.set(\"{key}\", {value:?}): {what}");
        match key {
            SCROLL_STEP_KEY => {
                settings.scroll_step = value
                    .parse::<u32>()
                    .ok()
                    .filter(|n| (10..=400).contains(n))
                    .ok_or_else(|| error("must be a whole number from 10 to 400"))?;
            }
            SMOOTH_KEY | PREFIX_WAIT_KEY => {
                let on = match value {
                    "true" => true,
                    "false" => false,
                    _ => return Err(error("must be true or false")),
                };
                if key == SMOOTH_KEY {
                    settings.smooth = on;
                } else {
                    settings.prefix_wait = on;
                }
            }
            WHICH_KEY_DELAY_KEY => {
                settings.which_key_delay = if value == "false" {
                    None
                } else {
                    Some(
                        value
                            .parse::<u32>()
                            .ok()
                            .filter(|n| *n <= 5000)
                            .ok_or_else(|| error("must be a whole number from 0 to 5000, or false"))?,
                    )
                };
            }
            HINT_CHARS_KEY => {
                let mut seen = [false; 26];
                if !(2..=26).contains(&value.len())
                    || !value.bytes().all(|b| {
                        if !b.is_ascii_lowercase() {
                            return false;
                        }
                        let slot = &mut seen[(b - b'a') as usize];
                        let fresh = !*slot;
                        *slot = true;
                        fresh
                    })
                {
                    return Err(error("must be 2 to 26 different lowercase letters a-z"));
                }
                settings.hint_chars = value.to_owned();
            }
            FIND_CASE_KEY => {
                settings.find_case = match value {
                    "smart" => FindCase::Smart,
                    "ignore" => FindCase::Ignore,
                    "match" => FindCase::Match,
                    _ => return Err(error("must be \"smart\", \"ignore\" or \"match\"")),
                };
            }
            _ => unreachable!(),
        }
    }
    Ok(settings)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn parse(pairs: &[(&str, &str)]) -> Result<KeysSettings, String> {
        let map: HashMap<String, String> = pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        from_config(|key| map.get(key).cloned())
    }

    fn shared_table() -> serde_json::Value {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/plugin/fixtures/kit_settings.json");
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    #[test]
    fn kit_settings_match_the_shared_table() {
        let table = shared_table();
        assert_eq!(
            serde_json::to_value(KeysSettings::default()).unwrap(),
            table["defaults"]
        );
        for group in ["accepted", "refused"] {
            for example in table[group].as_array().unwrap() {
                let field = example["field"].as_str().unwrap();
                let key = format!("plugins.keys.{field}");
                let value = &example["value"];
                let raw = match value {
                    serde_json::Value::String(s) => s.clone(),
                    serde_json::Value::Null => "false".into(),
                    _ => value.to_string(),
                };
                let result = parse(&[(&key, &raw)]);
                if group == "accepted" {
                    assert_eq!(serde_json::to_value(result.unwrap()).unwrap()[field], *value);
                } else {
                    assert!(result.is_err(), "{field} = {value} must be refused");
                }
            }
        }
        for (field, range) in table["ranges"].as_object().unwrap() {
            let key = format!("plugins.keys.{field}");
            let min = range[0].as_i64().unwrap();
            let max = range[1].as_i64().unwrap();
            for (n, accepted) in [(min - 1, false), (min, true), (max, true), (max + 1, false)] {
                let raw = if field == "hint_chars" {
                    "abcdefghijklmnopqrstuvwxyz".repeat(2)[..n as usize].to_owned()
                } else {
                    n.to_string()
                };
                assert_eq!(parse(&[(&key, &raw)]).is_ok(), accepted, "{field} = {raw}");
            }
        }
    }

    #[test]
    fn every_prefix_the_keymap_accepts_round_trips_through_the_kit() {
        let table = shared_table();
        let prefixes = table["prefixes"].as_array().unwrap();
        assert_eq!(prefixes.len(), 26);
        for (letter, prefix) in ('a'..='z').zip(prefixes) {
            for input in [letter, letter.to_ascii_uppercase()] {
                let key = crate::keymap::KeySpec::parse(&format!("C-{input}")).unwrap();
                assert_eq!(key.human(), prefix["human"].as_str().unwrap());
                assert_eq!(key.to_vim(), prefix["kit"].as_str().unwrap());
            }
        }
    }

    #[test]
    fn unset_keys_give_the_defaults() {
        assert_eq!(
            parse(&[]).unwrap(),
            KeysSettings {
                scroll_step: 60,
                smooth: true,
                prefix_wait: true,
                which_key_delay: Some(200),
                hint_chars: "sadfjklewcmpgh".into(),
                find_case: FindCase::Smart,
            }
        );
        assert_eq!(parse(&[]).unwrap(), KeysSettings::default());
        // An empty value means unset, as for every other key Eitri reads.
        assert_eq!(
            parse(&[
                (SCROLL_STEP_KEY, ""),
                (SMOOTH_KEY, " "),
                (WHICH_KEY_DELAY_KEY, ""),
                (FIND_CASE_KEY, "  ")
            ])
            .unwrap(),
            KeysSettings::default()
        );
    }

    #[test]
    fn every_key_reads_its_values() {
        let s = parse(&[
            (SCROLL_STEP_KEY, " 40 "),
            (SMOOTH_KEY, "false"),
            (PREFIX_WAIT_KEY, "false"),
            (WHICH_KEY_DELAY_KEY, "0"),
            (HINT_CHARS_KEY, "asdfghjkl"),
            (FIND_CASE_KEY, "match"),
        ])
        .unwrap();
        assert_eq!(s.scroll_step, 40);
        assert!(!s.smooth);
        assert!(!s.prefix_wait);
        assert_eq!(s.which_key_delay, Some(0));
        assert_eq!(s.hint_chars, "asdfghjkl");
        assert_eq!(s.find_case, FindCase::Match);
        assert_eq!(parse(&[(WHICH_KEY_DELAY_KEY, "false")]).unwrap().which_key_delay, None);
        assert_eq!(
            parse(&[(WHICH_KEY_DELAY_KEY, "5000")]).unwrap().which_key_delay,
            Some(5000)
        );
        assert_eq!(parse(&[(SCROLL_STEP_KEY, "10")]).unwrap().scroll_step, 10);
        assert_eq!(parse(&[(SCROLL_STEP_KEY, "400")]).unwrap().scroll_step, 400);
        assert_eq!(parse(&[(FIND_CASE_KEY, "ignore")]).unwrap().find_case, FindCase::Ignore);
        assert_eq!(parse(&[(SMOOTH_KEY, "true")]).unwrap().smooth, true);
        assert_eq!(
            parse(&[(HINT_CHARS_KEY, "abcdefghijklmnopqrstuvwxyz")])
                .unwrap()
                .hint_chars
                .len(),
            26
        );
    }

    #[test]
    fn a_bad_value_is_an_error_naming_the_key_and_the_value() {
        for (key, value, want) in [
            (SCROLL_STEP_KEY, "9", "must be a whole number from 10 to 400"),
            (SCROLL_STEP_KEY, "401", "must be a whole number from 10 to 400"),
            (SCROLL_STEP_KEY, "60.5", "must be a whole number from 10 to 400"),
            (SCROLL_STEP_KEY, "fast", "must be a whole number from 10 to 400"),
            (SMOOTH_KEY, "yes", "must be true or false"),
            (PREFIX_WAIT_KEY, "1", "must be true or false"),
            (
                WHICH_KEY_DELAY_KEY,
                "5001",
                "must be a whole number from 0 to 5000, or false",
            ),
            (
                WHICH_KEY_DELAY_KEY,
                "true",
                "must be a whole number from 0 to 5000, or false",
            ),
            (
                WHICH_KEY_DELAY_KEY,
                "-1",
                "must be a whole number from 0 to 5000, or false",
            ),
            (HINT_CHARS_KEY, "a", "must be 2 to 26 different lowercase letters a-z"),
            (HINT_CHARS_KEY, "aa", "must be 2 to 26 different lowercase letters a-z"),
            (HINT_CHARS_KEY, "abC", "must be 2 to 26 different lowercase letters a-z"),
            (HINT_CHARS_KEY, "ab1", "must be 2 to 26 different lowercase letters a-z"),
            (FIND_CASE_KEY, "Smart", "must be \"smart\", \"ignore\" or \"match\""),
        ] {
            let err = parse(&[(key, value)]).unwrap_err();
            assert_eq!(
                err,
                format!("eitri.config.set(\"{key}\", {value:?}): {want}"),
                "{key} = {value:?}"
            );
        }
    }

    #[test]
    fn the_wire_shape_is_six_snake_case_members() {
        assert_eq!(
            serde_json::to_value(KeysSettings::default()).unwrap(),
            serde_json::json!({"scroll_step": 60, "smooth": true, "prefix_wait": true, "which_key_delay": 200,
                               "hint_chars": "sadfjklewcmpgh", "find_case": "smart"})
        );
        let off = KeysSettings {
            which_key_delay: None,
            ..KeysSettings::default()
        };
        assert_eq!(
            serde_json::to_value(off).unwrap()["which_key_delay"],
            serde_json::Value::Null
        );
    }
}
