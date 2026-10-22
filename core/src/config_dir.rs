//! Resolve the Linux configuration directory without reading process-global state.

use std::ffi::OsString;
use std::path::PathBuf;

/// Prefer the explicit override, then an absolute XDG base, then the home fallback.
pub fn resolve(env: &dyn Fn(&str) -> Option<OsString>) -> Result<PathBuf, String> {
    if let Some(dir) = env("EITRI_CONFIG_DIR").filter(|dir| !dir.is_empty()) {
        let path = PathBuf::from(&dir);
        return if path.is_absolute() {
            Ok(path)
        } else {
            Err(format!("EITRI_CONFIG_DIR={dir:?} is not an absolute path"))
        };
    }
    if let Some(dir) = env("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute())
    {
        return Ok(dir.join("eitri"));
    }
    env("HOME")
        .map(|home| PathBuf::from(home).join(".config/eitri"))
        .ok_or_else(|| "HOME must be set".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolve_with(values: &[(&str, &str)]) -> Result<PathBuf, String> {
        resolve(&|key| {
            values
                .iter()
                .find(|(name, _)| *name == key)
                .map(|(_, value)| OsString::from(value))
        })
    }

    #[test]
    fn eitri_config_dir_wins() {
        assert_eq!(
            resolve_with(&[
                ("EITRI_CONFIG_DIR", "/custom"),
                ("XDG_CONFIG_HOME", "/xdg"),
                ("HOME", "/home/u")
            ]),
            Ok(PathBuf::from("/custom"))
        );
    }

    #[test]
    fn an_empty_eitri_config_dir_counts_as_unset() {
        assert_eq!(
            resolve_with(&[("EITRI_CONFIG_DIR", ""), ("XDG_CONFIG_HOME", "/xdg")]),
            Ok(PathBuf::from("/xdg/eitri"))
        );
    }

    #[test]
    fn a_relative_eitri_config_dir_is_refused() {
        let err = resolve_with(&[
            ("EITRI_CONFIG_DIR", "relative"),
            ("XDG_CONFIG_HOME", "/xdg"),
            ("HOME", "/home/u"),
        ])
        .unwrap_err();
        assert!(
            err.contains("EITRI_CONFIG_DIR") && err.contains("relative") && err.contains("is not an absolute path"),
            "{err}"
        );
    }

    #[test]
    fn an_absolute_xdg_config_home_comes_next() {
        assert_eq!(
            resolve_with(&[("XDG_CONFIG_HOME", "/xdg")]),
            Ok(PathBuf::from("/xdg/eitri"))
        );
    }

    #[test]
    fn a_relative_or_empty_xdg_config_home_is_ignored() {
        for value in ["relative", ""] {
            assert_eq!(
                resolve_with(&[("XDG_CONFIG_HOME", value), ("HOME", "/home/u")]),
                Ok(PathBuf::from("/home/u/.config/eitri"))
            );
            assert!(resolve_with(&[("XDG_CONFIG_HOME", value)]).is_err());
        }
    }

    #[test]
    fn home_dot_config_is_the_fallback() {
        assert_eq!(
            resolve_with(&[("HOME", "/home/u")]),
            Ok(PathBuf::from("/home/u/.config/eitri"))
        );
    }

    #[test]
    fn nothing_set_is_an_error() {
        assert!(resolve_with(&[]).unwrap_err().contains("HOME must be set"));
    }
}
