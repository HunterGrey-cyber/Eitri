//! GNOME's shortcut settings as chords. The shell reads the `as` keys of the shortcut schemas through GSettings and
//! hands over each key's schema name and accelerator strings; this turns them into the chords they bind.

use crate::keymap::Chord;

/// The schemas GNOME keeps its live shortcuts in. A schema that is not installed is skipped by the caller.
pub const SCHEMAS: &[&str] = &[
    "org.gnome.desktop.wm.keybindings",
    "org.gnome.shell.keybindings",
    "org.gnome.mutter.keybindings",
    "org.gnome.mutter.wayland.keybindings",
    "org.gnome.settings-daemon.plugins.media-keys",
];

/// The chords the accelerators bind, with the schema key each came from. An accelerator holding Super (or any
/// modifier Eitri has no key for), an empty one (`['']` is how GNOME writes "unbound") and one that is not
/// accelerator text are not candidates.
pub fn gnome_chords(accels: &[(String, String)]) -> Vec<(Chord, String)> {
    accels
        .iter()
        .filter_map(|(schema_key, accel)| {
            let parsed = Chord::from_accel(accel.trim()).ok()?;
            super::checked(parsed).map(|chord| (chord, schema_key.clone()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair(key: &str, accel: &str) -> (String, String) {
        (key.to_string(), accel.to_string())
    }

    #[test]
    fn alt_primary_and_function_accelerators_are_chords_and_super_and_empty_ones_are_not() {
        let accels = [
            pair("close", "<Alt>F4"),
            pair("terminal", "<Primary><Alt>t"),
            pair("activities", "<Super>space"),
            pair("unbound", ""),
            pair("switch-input-source", "<Control>space"),
        ];
        let found: Vec<(String, String)> = gnome_chords(&accels)
            .into_iter()
            .map(|(chord, key)| (chord.human(), key))
            .collect();
        assert_eq!(
            found,
            [
                ("Alt+F4".to_string(), "close".to_string()),
                ("Ctrl+Alt+t".to_string(), "terminal".to_string()),
                ("Ctrl+space".to_string(), "switch-input-source".to_string()),
            ]
        );
    }

    #[test]
    fn text_that_is_not_an_accelerator_is_left_out() {
        let accels = [
            pair("a", "<Alt>"),
            pair("b", "<Alt>\u{1b}[31mx"),
            pair("c", "<Hyper>x"),
            pair("d", &"x".repeat(5000)),
        ];
        assert!(gnome_chords(&accels).is_empty());
    }
}
