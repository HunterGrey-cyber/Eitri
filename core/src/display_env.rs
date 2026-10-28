use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Mirrors libwayland's own default, naming the display GTK would connect to anyway.
/// The shell sets this variable so the embedded editor (whose winit refuses to guess),
/// compositor detection and child processes all see the same display.
pub fn wayland_display_fallback(
    env: impl Fn(&str) -> Option<OsString>,
    socket_exists: impl Fn(&Path) -> bool,
) -> Option<&'static str> {
    for key in ["WAYLAND_DISPLAY", "WAYLAND_SOCKET", "DISPLAY"] {
        if env(key).is_some_and(|value| !value.is_empty()) {
            return None;
        }
    }

    let runtime_dir = PathBuf::from(env("XDG_RUNTIME_DIR")?);
    if !runtime_dir.is_absolute() {
        return None;
    }

    socket_exists(&runtime_dir.join("wayland-0")).then_some("wayland-0")
}

#[cfg(test)]
mod tests {
    use super::wayland_display_fallback;
    use std::ffi::OsString;
    use std::path::Path;

    #[test]
    fn display_env_fallback_follows_libwaylands_default() {
        let cases: &[(&str, &[(&str, Option<&str>)], bool, Option<&str>)] = &[
            ("all displays unset", &[], true, Some("wayland-0")),
            (
                "Wayland display set",
                &[("WAYLAND_DISPLAY", Some("wayland-1"))],
                true,
                None,
            ),
            (
                "empty Wayland display",
                &[("WAYLAND_DISPLAY", Some(""))],
                true,
                Some("wayland-0"),
            ),
            ("Wayland socket set", &[("WAYLAND_SOCKET", Some("3"))], true, None),
            (
                "empty Wayland socket",
                &[("WAYLAND_SOCKET", Some(""))],
                true,
                Some("wayland-0"),
            ),
            ("X display set", &[("DISPLAY", Some(":0"))], true, None),
            ("empty X display", &[("DISPLAY", Some(""))], true, Some("wayland-0")),
            (
                "all displays empty",
                &[
                    ("WAYLAND_DISPLAY", Some("")),
                    ("WAYLAND_SOCKET", Some("")),
                    ("DISPLAY", Some("")),
                ],
                true,
                Some("wayland-0"),
            ),
            ("socket absent", &[], false, None),
            ("runtime dir unset", &[("XDG_RUNTIME_DIR", None)], true, None),
            ("runtime dir empty", &[("XDG_RUNTIME_DIR", Some(""))], true, None),
            (
                "runtime dir relative",
                &[("XDG_RUNTIME_DIR", Some("runtime"))],
                true,
                None,
            ),
        ];

        for &(name, values, socket_present, expected) in cases {
            let result = wayland_display_fallback(
                |key| {
                    if let Some((_, value)) = values.iter().find(|(candidate, _)| *candidate == key) {
                        return value.map(OsString::from);
                    }
                    (key == "XDG_RUNTIME_DIR").then(|| OsString::from("/eitri-test-runtime"))
                },
                |path| {
                    assert_eq!(path, Path::new("/eitri-test-runtime/wayland-0"), "{name}");
                    socket_present
                },
            );
            assert_eq!(result, expected, "{name}");
        }
    }
}
