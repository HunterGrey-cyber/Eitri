//! Shared spellings for configuration switches stored as text.

/// Read a stored switch, preserving the supplied default when it is unset.
pub fn parse_switch(key: &str, value: Option<&str>, default: bool) -> Result<bool, String> {
    match value.map(str::trim) {
        None => Ok(default),
        Some("true" | "on") => Ok(true),
        Some("false" | "off") => Ok(false),
        Some(_) => Err(format!(
            "eitri.config.set(\"{key}\", {:?}): must be true or \"on\" / false or \"off\"",
            value.unwrap_or_default()
        )),
    }
}

/// Recognize the disabled spellings for settings that also accept a numeric value.
pub fn is_off(value: &str) -> bool {
    matches!(value, "off" | "false")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_spellings_turn_a_switch_on_and_off() {
        for (value, expected) in [
            ("true", true),
            ("on", true),
            ("false", false),
            ("off", false),
            (" false ", false),
        ] {
            assert_eq!(parse_switch("example", Some(value), !expected), Ok(expected));
        }
        for default in [false, true] {
            assert_eq!(parse_switch("example", None, default), Ok(default));
        }
        for value in ["off", "false"] {
            assert!(is_off(value));
        }
        for value in ["on", "true", "False", "", "0"] {
            assert!(!is_off(value));
        }
    }

    #[test]
    fn a_bad_value_names_the_key_and_both_spellings() {
        for value in ["", "False", "TRUE", "yes", "0", "1", "nil"] {
            let err = parse_switch("example", Some(value), true).unwrap_err();
            for word in ["example", &format!("{value:?}"), "true", "false", "on", "off"] {
                assert!(err.contains(word), "{err}");
            }
        }
    }
}
