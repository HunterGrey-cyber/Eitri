//! The root table (spec §2.2): the keys that work with no prefix. Two kinds. The GTK accelerators
//! (`ROOT_ACCELS`), registered by `shell` from this table and nowhere else -- `shell/src` holds no
//! accelerator literal (`shell/src/main.rs`'s scanner test). And `Ctrl+h/j/k/l`, which are not
//! accelerators but capture-phase controllers per module (`pane_switch`, `terminal::navigation`);
//! they are listed so the prefix and a Lua `keybinding` cannot take them.
//!
//! **No chord here holds `Ctrl+Shift`** (the owner's fcitx5 `Control+Shift_L` hotkey fires on them;
//! dated record, 2026-09-24, the terminal GUI-pass entry). `Ctrl+Shift+F`/`R`/`F11` became
//! `prefix f`/`r`/`F11`; `<Control>plus` is dropped, since on a US layout it is `Ctrl+Shift+=`.

use super::key::Chord;

/// One app action and the accelerators that fire it.
#[derive(Debug, Clone, Copy)]
pub struct RootAccel {
    /// The gio action name, without `app.`.
    pub action: &'static str,
    pub accels: &'static [&'static str],
    pub what: &'static str,
}

pub const ROOT_ACCELS: &[RootAccel] = &[
    RootAccel {
        action: "fullscreen",
        accels: &["F11"],
        what: "Fullscreen",
    },
    RootAccel {
        action: "text-larger",
        accels: &["<Control>equal", "<Control>KP_Add"],
        what: "Text size larger (both panes)",
    },
    RootAccel {
        action: "text-smaller",
        accels: &["<Control>minus", "<Control>KP_Subtract"],
        what: "Text size smaller (both panes)",
    },
    RootAccel {
        action: "text-reset",
        // Keypad 0 with NumLock off reports as KP_Insert, not KP_0 (2026-09-23).
        accels: &["<Control>0", "<Control>KP_0", "<Control>KP_Insert"],
        what: "Text size reset (both panes)",
    },
];

/// `Ctrl+<letter>`: move the keys to the module that way (vim-tmux-navigator's convention).
pub const NAV_KEYS: [(char, &str); 4] = [
    ('h', "move to the module left"),
    ('j', "move to the module below (the terminal when it is shown)"),
    ('k', "move to the module above; from the top module, the top bar"),
    ('l', "move to the module right"),
];

/// The accelerators `action` is registered with.
pub fn accels(action: &str) -> &'static [&'static str] {
    ROOT_ACCELS
        .iter()
        .find(|row| row.action == action)
        .unwrap_or_else(|| panic!("no root accelerator row for app action {action:?}"))
        .accels
}

/// Every root chord, with how a collision message names it.
pub fn chords() -> Vec<(Chord, String)> {
    let mut out: Vec<(Chord, String)> = NAV_KEYS
        .iter()
        .map(|(letter, what)| {
            let chord = Chord {
                ctrl: true,
                alt: false,
                shift: false,
                key: letter.to_string(),
            };
            let named = format!("Ctrl+{letter} ({what})");
            (chord, named)
        })
        .collect();
    for row in ROOT_ACCELS {
        for accel in row.accels {
            let chord = Chord::from_gtk(accel).expect("ROOT_ACCELS holds only accelerators Chord reads");
            out.push((chord, format!("{} ({})", spell(accel), row.what.to_lowercase())));
        }
    }
    out
}

/// Keysym names a person reads differently, and how.
const SPELLED: [(&str, &str); 7] = [
    ("equal", "="),
    ("plus", "+"),
    ("minus", "-"),
    ("KP_Add", "Keypad+"),
    ("KP_Subtract", "Keypad-"),
    ("KP_0", "Keypad0"),
    ("KP_Insert", "Keypad0 (NumLock off)"),
];

/// A GTK accelerator as the overlay prints it: `<Control>equal` is `Ctrl+=`.
pub fn spell(accel: &str) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut rest = accel;
    while let Some(end) = rest.strip_prefix('<').and_then(|r| r.find('>')) {
        parts.push(match &rest[1..end + 1] {
            "Control" | "Primary" => "Ctrl".to_string(),
            other => other.to_string(),
        });
        rest = &rest[end + 2..];
    }
    parts.push(match SPELLED.iter().find(|(keysym, _)| *keysym == rest) {
        Some((_, spelled)) => spelled.to_string(),
        None if rest.chars().count() == 1 => rest.to_uppercase(),
        None => rest.to_string(),
    });
    parts.join("+")
}

/// One line of the `?` overlay.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct HelpRow {
    pub keys: String,
    pub what: String,
}

/// The overlay's "Anywhere in the window" section, generated from this table.
pub fn help_rows() -> Vec<HelpRow> {
    let mut rows = vec![
        HelpRow {
            keys: "Ctrl+h / Ctrl+l".into(),
            what: "Module left / right".into(),
        },
        HelpRow {
            keys: "Ctrl+k".into(),
            what: "Module above; from the top one, the top bar (h / l move, Ctrl+j or Esc go back)".into(),
        },
        HelpRow {
            keys: "Ctrl+j".into(),
            what: "Module below (the terminal when it is shown)".into(),
        },
    ];
    for row in ROOT_ACCELS {
        rows.push(HelpRow {
            keys: row.accels.iter().map(|a| spell(a)).collect::<Vec<_>>().join(" / "),
            what: row.what.to_string(),
        });
    }
    rows.push(HelpRow {
        keys: "Ctrl+wheel".into(),
        what: "Text size of the pane under the pointer only".into(),
    });
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Spec §2.2: no root chord holds Control and Shift together -- the owner's fcitx5
    /// `Control+Shift_L` hotkey fires on them. `plus` is Shift+= on a US layout, and an uppercase
    /// letter needs Shift, so both count.
    #[test]
    fn no_root_chord_holds_control_and_shift() {
        for (chord, what) in chords() {
            assert!(!(chord.ctrl && chord.shift), "{what} holds Control+Shift");
            assert_ne!(chord.key, "+", "{what}: `plus` is a Shift chord on a US layout");
            assert!(
                !(chord.key.chars().count() == 1 && chord.key.chars().all(|c| c.is_ascii_uppercase())),
                "{what}: an uppercase letter needs Shift"
            );
        }
        for row in ROOT_ACCELS {
            for accel in row.accels {
                assert!(!accel.contains("<Shift>"), "{} binds {accel}", row.action);
            }
        }
    }

    #[test]
    fn the_root_accelerators_are_the_specs_table() {
        let got: Vec<(&str, Vec<&str>)> = ROOT_ACCELS.iter().map(|r| (r.action, r.accels.to_vec())).collect();
        assert_eq!(
            got,
            vec![
                ("fullscreen", vec!["F11"]),
                ("text-larger", vec!["<Control>equal", "<Control>KP_Add"]),
                ("text-smaller", vec!["<Control>minus", "<Control>KP_Subtract"]),
                ("text-reset", vec!["<Control>0", "<Control>KP_0", "<Control>KP_Insert"]),
            ]
        );
        assert_eq!(
            accels("text-reset"),
            ["<Control>0", "<Control>KP_0", "<Control>KP_Insert"]
        );
    }

    #[test]
    #[should_panic(expected = "hint")]
    fn an_action_with_no_row_is_a_programming_error() {
        accels("hint");
    }

    #[test]
    fn the_navigation_chords_are_ctrl_h_j_k_l() {
        let nav: Vec<Chord> = chords()
            .into_iter()
            .map(|(c, _)| c)
            .filter(|c| c.key.len() == 1)
            .collect();
        for letter in ["h", "j", "k", "l"] {
            assert!(
                nav.contains(&Chord {
                    ctrl: true,
                    alt: false,
                    shift: false,
                    key: letter.into()
                }),
                "{letter}"
            );
        }
        let descriptions: Vec<String> = chords().into_iter().map(|(_, d)| d).collect();
        assert!(
            descriptions.iter().any(|d| d.starts_with("Ctrl+h (")),
            "{descriptions:?}"
        );
        assert!(
            descriptions.iter().any(|d| d.starts_with("Ctrl+= (")),
            "{descriptions:?}"
        );
    }

    /// Owner decision #39 (2026-09-30): in the agent panel's INPUT, `Ctrl+y` approves the oldest
    /// waiting card. The page handles it, the way it handles `Ctrl+Enter`: nothing in `shell`
    /// claims either chord, so the WebView's own key event decides. A root chord (an accelerator or
    /// a capture-phase navigation key) or the default prefix on `Ctrl+y` would take it before the
    /// page ever saw it, and the band would advertise a key that does nothing. A user's own
    /// `eitri.keymap` can still take it; that is theirs to choose.
    #[test]
    fn ctrl_y_stays_the_agent_panels() {
        let ctrl_y = Chord::of(&super::super::key::KeySpec::parse("C-y").expect("C-y parses"));
        assert_eq!(
            ctrl_y,
            Chord {
                ctrl: true,
                alt: false,
                shift: false,
                key: "y".into()
            }
        );
        for (chord, what) in chords() {
            assert_ne!(chord, ctrl_y, "{what} would take the agent panel's INPUT Ctrl+y (#39)");
        }
        let prefix = Chord::of(
            &super::super::key::KeySpec::parse(super::super::stock::STOCK_PREFIX).expect("the stock prefix parses"),
        );
        assert_ne!(
            prefix, ctrl_y,
            "the default prefix would take the agent panel's INPUT Ctrl+y (#39)"
        );
    }

    #[test]
    fn spell_reads_an_accelerator_as_a_person_types_it() {
        assert_eq!(spell("F11"), "F11");
        assert_eq!(spell("<Control>equal"), "Ctrl+=");
        assert_eq!(spell("<Control>KP_Add"), "Ctrl+Keypad+");
        assert_eq!(spell("<Control>KP_Insert"), "Ctrl+Keypad0 (NumLock off)");
        assert_eq!(spell("<Control>0"), "Ctrl+0");
    }

    #[test]
    fn every_root_accelerator_and_every_nav_key_is_in_the_help() {
        let rows = help_rows();
        let keys: Vec<&str> = rows.iter().flat_map(|r| r.keys.split(" / ")).collect();
        for row in ROOT_ACCELS {
            for accel in row.accels {
                assert!(keys.contains(&spell(accel).as_str()), "{accel} missing from {keys:?}");
            }
        }
        for (letter, _) in NAV_KEYS {
            assert!(keys.contains(&format!("Ctrl+{letter}").as_str()), "Ctrl+{letter}");
        }
        assert!(rows.iter().any(|r| r.keys == "Ctrl+wheel"), "the per-pane wheel row");
        assert!(rows.iter().all(|r| !r.keys.contains("Shift")), "{rows:?}");
    }
}
