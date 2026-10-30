//! Presentation preferences and the capability decision, independent of GTK objects.

use std::ffi::OsStr;
use std::sync::OnceLock;

/// The editor's presentation preference; unavailable subsurfaces fall back to GTK.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Presentation {
    Auto,
    Subsurface,
    Gtk,
}

impl Presentation {
    /// The preference of a pane whose host has not supplied one.
    pub const DEFAULT: Presentation = Presentation::Auto;

    /// Checks the process-wide override before the host shows its first editor. The cached
    /// result is also used when a pane chooses its presentation at realize.
    pub fn validate_environment() -> Result<(), String> {
        effective(Self::DEFAULT).map(|_| ())
    }
}

pub(crate) fn from_env(value: Option<&OsStr>) -> Result<Option<Presentation>, String> {
    match value {
        None => Ok(None),
        Some(value) if value.is_empty() => Ok(None),
        Some(value) => match value.to_str() {
            Some("auto") => Ok(Some(Presentation::Auto)),
            Some("subsurface") => Ok(Some(Presentation::Subsurface)),
            Some("gtk") => Ok(Some(Presentation::Gtk)),
            _ => Err(format!(
                "EITRI_EDITOR_PRESENTATION must be auto, subsurface or gtk (got {value:?})"
            )),
        },
    }
}

pub(crate) struct Facts {
    pub wayland: bool,
    pub gtk: (u32, u32, u32),
    pub egl_vendor: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Decision {
    Gtk,
    Subsurface,
}

pub(crate) const SUBSURFACE_MIN_GTK: (u32, u32, u32) = (4, 24, 0);

pub(crate) fn decide(setting: Presentation, facts: &Facts) -> Decision {
    if !facts.wayland {
        return Decision::Gtk;
    }
    match setting {
        Presentation::Gtk => Decision::Gtk,
        Presentation::Subsurface => Decision::Subsurface,
        Presentation::Auto => {
            let nvidia = facts
                .egl_vendor
                .as_deref()
                .is_some_and(|vendor| vendor.contains("NVIDIA"));
            if facts.gtk >= SUBSURFACE_MIN_GTK && !nvidia {
                Decision::Subsurface
            } else {
                Decision::Gtk
            }
        }
    }
}

pub(crate) fn effective(stored: Presentation) -> Result<Presentation, String> {
    static ENVIRONMENT: OnceLock<Result<Option<Presentation>, String>> = OnceLock::new();
    ENVIRONMENT
        .get_or_init(|| from_env(std::env::var_os("EITRI_EDITOR_PRESENTATION").as_deref()))
        .clone()
        .map(|value| value.unwrap_or(stored))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;
    #[test]
    fn env_takes_three_words_and_empty_is_unset() {
        assert_eq!(from_env(Some(OsStr::new("auto"))), Ok(Some(Presentation::Auto)));
        assert_eq!(
            from_env(Some(OsStr::new("subsurface"))),
            Ok(Some(Presentation::Subsurface))
        );
        assert_eq!(from_env(Some(OsStr::new("gtk"))), Ok(Some(Presentation::Gtk)));
        assert_eq!(from_env(Some(OsStr::new(""))), Ok(None));
        assert_eq!(from_env(None), Ok(None));
        for bad in ["1", "on", "Auto", " gtk", "subsurface\n"] {
            assert!(from_env(Some(OsStr::new(bad))).is_err(), "{bad:?}");
        }
    }
    #[test]
    fn a_non_utf8_environment_value_names_the_variable_and_choices() {
        use std::os::unix::ffi::OsStrExt;

        let error = from_env(Some(OsStr::from_bytes(b"gtk\xff"))).unwrap_err();
        for word in ["EITRI_EDITOR_PRESENTATION", "auto", "subsurface", "gtk"] {
            assert!(error.contains(word), "{error}");
        }
    }

    #[test]
    fn auto_checks_the_vendor_substring_and_allows_an_unknown_vendor() {
        let mut facts = Facts {
            wayland: true,
            gtk: (4, 24, 0),
            egl_vendor: Some("EGL NVIDIA Corporation".into()),
        };
        assert_eq!(decide(Presentation::Auto, &facts), Decision::Gtk);
        facts.egl_vendor = None;
        assert_eq!(decide(Presentation::Auto, &facts), Decision::Subsurface);
    }

    #[test]
    fn auto_needs_wayland_gtk_4_24_and_no_nvidia() {
        let f = |wayland, gtk, vendor: Option<&str>| Facts {
            wayland,
            gtk,
            egl_vendor: vendor.map(str::to_owned),
        };
        assert_eq!(
            decide(Presentation::Auto, &f(true, (4, 24, 0), Some("Mesa Project"))),
            Decision::Subsurface
        );
        assert_eq!(
            decide(Presentation::Auto, &f(true, (4, 24, 1), Some("Mesa Project"))),
            Decision::Subsurface
        );
        assert_eq!(
            decide(Presentation::Auto, &f(true, (4, 23, 9), Some("Mesa Project"))),
            Decision::Gtk
        );
        assert_eq!(
            decide(Presentation::Auto, &f(true, (4, 22, 5), Some("Mesa Project"))),
            Decision::Gtk
        );
        assert_eq!(
            decide(Presentation::Auto, &f(true, (4, 24, 1), Some("NVIDIA"))),
            Decision::Gtk
        );
        assert_eq!(decide(Presentation::Auto, &f(false, (4, 24, 1), None)), Decision::Gtk);
    }
    #[test]
    fn forced_skips_version_and_vendor_but_not_wayland() {
        let f = |wayland, gtk, vendor: &str| Facts {
            wayland,
            gtk,
            egl_vendor: Some(vendor.into()),
        };
        assert_eq!(
            decide(Presentation::Subsurface, &f(true, (4, 14, 5), "NVIDIA")),
            Decision::Subsurface
        );
        assert_eq!(
            decide(Presentation::Subsurface, &f(true, (4, 22, 5), "Mesa")),
            Decision::Subsurface
        );
        assert_eq!(
            decide(Presentation::Subsurface, &f(false, (4, 24, 1), "Mesa")),
            Decision::Gtk
        );
        assert_eq!(decide(Presentation::Gtk, &f(true, (4, 24, 1), "Mesa")), Decision::Gtk);
    }
    #[test]
    fn the_default_is_auto() {
        assert_eq!(Presentation::DEFAULT, Presentation::Auto);
    }
}
