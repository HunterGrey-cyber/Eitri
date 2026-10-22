//! Converts the keymap's chords to the accelerator spelling GTK registers.

use eitri_core::keymap::{key::keysym_name, Chord};

pub(crate) fn gtk_accel(chord: &Chord) -> String {
    let mut out = String::new();
    if chord.ctrl {
        out.push_str("<Control>");
    }
    if chord.alt {
        out.push_str("<Alt>");
    }
    if chord.shift {
        out.push_str("<Shift>");
    }
    let mut chars = chord.key.chars();
    let key = match (chars.next(), chars.next()) {
        (Some(c), None) => keysym_name(c).unwrap_or(&chord.key),
        _ => &chord.key,
    };
    out.push_str(key);
    out
}

pub(crate) fn command_accel(keybinding: &str) -> Result<String, String> {
    Chord::from_accel(keybinding).map(|chord| gtk_accel(&chord))
}

/// What `set_accels_for_action` gets for a command's `keybinding`. A modifier the keymap does not
/// read (`<Super>`, `<Meta>`, `<Hyper>`) is one Eitri never binds, so nothing can collide with it,
/// and GTK still reads it: such a keybinding goes to GTK as written and keeps working.
pub(crate) fn registered_accel(keybinding: &str) -> String {
    command_accel(keybinding).unwrap_or_else(|_| keybinding.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use eitri_core::keymap::{root::ROOT_ACCELS, Chord};

    #[test]
    fn gtk_accel_round_trips_every_root_chord() {
        let old = [
            "F11",
            "<Control>equal",
            "<Control>KP_Add",
            "<Control>minus",
            "<Control>KP_Subtract",
            "<Control>0",
            "<Control>KP_0",
            "<Control>KP_Insert",
        ];
        let chords: Vec<_> = ROOT_ACCELS.iter().flat_map(|r| r.chords).collect();
        assert_eq!(chords.len(), old.len());
        for (root, old) in chords.into_iter().zip(old) {
            let c = root.chord();
            assert_eq!(gtk_accel(&c), old);
            assert_eq!(Chord::from_accel(&gtk_accel(&c)).unwrap(), c);
        }
    }

    #[test]
    fn a_primary_or_uppercase_keybinding_registers_the_accelerator_it_did_before() {
        for (input, expected) in [
            ("<Primary>h", "<Control>h"),
            ("<Control>h", "<Control>h"),
            ("<Control>H", "<Control>h"),
            ("<Control><Alt>r", "<Control><Alt>r"),
            ("<Shift><Mod1><Ctrl>question", "<Control><Alt><Shift>question"),
            ("<primary>t", "<Control>t"),
            ("<control><alt>r", "<Control><Alt>r"),
        ] {
            assert_eq!(command_accel(input).unwrap(), expected);
            assert_eq!(registered_accel(input), expected);
        }
        assert!(command_accel("<Super>x").is_err());
        for unread in ["<Super>x", "<super>x", "<Control><Meta>x"] {
            assert_eq!(
                registered_accel(unread),
                unread,
                "a modifier the keymap does not read still reaches GTK as written"
            );
        }
    }
}
