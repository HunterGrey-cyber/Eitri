//! The root keys work with no prefix. `ROOT_ACCELS` holds the chords registered by the shell;
//! `Ctrl+h/j/k/l` use capture-phase navigation controllers and are listed for collision checks.
//!
//! No root chord holds `Ctrl+Shift`, which conflicts with input-method hotkeys. `plus` is omitted
//! because it requires Shift on a US keyboard.

use super::key::Chord;

/// A static root chord, independent of the host's accelerator spelling.
#[derive(Debug, Clone, Copy)]
pub struct RootChord {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub key: &'static str,
}

impl RootChord {
    pub fn chord(&self) -> Chord {
        Chord {
            ctrl: self.ctrl,
            alt: self.alt,
            shift: self.shift,
            key: self.key.into(),
        }
    }
}

/// One app action and the chords that fire it.
#[derive(Debug, Clone, Copy)]
pub struct RootAccel {
    /// The gio action name, without `app.`.
    pub action: &'static str,
    pub chords: &'static [RootChord],
    pub what: &'static str,
}

pub const ROOT_ACCELS: &[RootAccel] = &[
    RootAccel {
        action: "fullscreen",
        chords: &[RootChord {
            ctrl: false,
            alt: false,
            shift: false,
            key: "F11",
        }],
        what: "Fullscreen",
    },
    RootAccel {
        action: "text-larger",
        chords: &[
            RootChord {
                ctrl: true,
                alt: false,
                shift: false,
                key: "=",
            },
            RootChord {
                ctrl: true,
                alt: false,
                shift: false,
                key: "KP_Add",
            },
        ],
        what: "Text size larger (both panes)",
    },
    RootAccel {
        action: "text-smaller",
        chords: &[
            RootChord {
                ctrl: true,
                alt: false,
                shift: false,
                key: "-",
            },
            RootChord {
                ctrl: true,
                alt: false,
                shift: false,
                key: "KP_Subtract",
            },
        ],
        what: "Text size smaller (both panes)",
    },
    RootAccel {
        action: "text-reset",
        // Keypad 0 with NumLock off reports as KP_Insert, not KP_0.
        chords: &[
            RootChord {
                ctrl: true,
                alt: false,
                shift: false,
                key: "0",
            },
            RootChord {
                ctrl: true,
                alt: false,
                shift: false,
                key: "KP_0",
            },
            RootChord {
                ctrl: true,
                alt: false,
                shift: false,
                key: "KP_Insert",
            },
        ],
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

/// The chords `action` is registered with.
pub fn chords_of(action: &str) -> &'static [RootChord] {
    ROOT_ACCELS
        .iter()
        .find(|row| row.action == action)
        .unwrap_or_else(|| panic!("no root accelerator row for app action {action:?}"))
        .chords
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
        for root in row.chords {
            out.push((
                root.chord(),
                format!("{} ({})", spell_chord(root), row.what.to_lowercase()),
            ));
        }
    }
    out
}

/// Keysym names a person reads differently, and how.
const SPELLED: [(&str, &str); 4] = [
    ("KP_Add", "Keypad+"),
    ("KP_Subtract", "Keypad-"),
    ("KP_0", "Keypad0"),
    ("KP_Insert", "Keypad0 (NumLock off)"),
];

/// A root chord as the overlay prints it: `=` with Control is `Ctrl+=`.
fn spell_chord(root: &RootChord) -> String {
    let mut chord = root.chord();
    chord.key = match SPELLED.iter().find(|(key, _)| *key == root.key) {
        Some((_, spelled)) => spelled.to_string(),
        None if root.key.chars().count() == 1 => root.key.to_uppercase(),
        None => root.key.to_string(),
    };
    chord.human()
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
            keys: row.chords.iter().map(spell_chord).collect::<Vec<_>>().join(" / "),
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

    #[test]
    fn root_chords_are_chords_not_accelerator_strings() {
        for row in ROOT_ACCELS {
            let RootAccel { action, chords, what } = *row;
            assert!(!action.is_empty());
            assert!(!what.is_empty());
            assert!(!chords.is_empty());
            for root in chords {
                let _: Chord = root.chord();
            }
        }
    }

    #[test]
    fn the_accelerator_notation_still_reads() {
        for (action, old) in [
            ("fullscreen", vec!["F11"]),
            ("text-larger", vec!["<Control>equal", "<Control>KP_Add"]),
            ("text-smaller", vec!["<Control>minus", "<Control>KP_Subtract"]),
            ("text-reset", vec!["<Control>0", "<Control>KP_0", "<Control>KP_Insert"]),
        ] {
            let roots = chords_of(action);
            assert_eq!(roots.len(), old.len());
            for (root, accel) in roots.iter().zip(old) {
                assert_eq!(Chord::from_accel(accel).unwrap(), root.chord());
            }
        }
        for (accel, key) in [("<Control><Alt>r", "r"), ("<Control><Alt>e", "e")] {
            assert_eq!(
                Chord::from_accel(accel).unwrap(),
                Chord {
                    ctrl: true,
                    alt: true,
                    shift: false,
                    key: key.into(),
                }
            );
        }
        assert!(Chord::from_accel("<Super>x").is_err());
    }

    /// Control and Shift together conflict with input-method hotkeys. `plus` is Shift+= on a US
    /// layout, and an uppercase letter needs Shift, so both count.
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
            for root in row.chords {
                assert!(!root.shift, "{} binds {root:?}", row.action);
            }
        }
    }

    /// The agent panel reads the key that brings the keyboard into it off this row (`panelEntryKey` in
    /// `agent-ui/web/src/keymap.ts`): the second of its two keys, found by these words.
    #[test]
    fn the_module_move_row_is_the_one_the_panel_reads_its_entry_key_from() {
        let rows = help_rows();
        assert_eq!(rows[0].keys, "Ctrl+h / Ctrl+l");
        assert_eq!(rows[0].what, "Module left / right");
    }

    #[test]
    fn the_root_accelerators_are_the_specs_table() {
        let got: Vec<(&str, Vec<Chord>)> = ROOT_ACCELS
            .iter()
            .map(|r| (r.action, r.chords.iter().map(RootChord::chord).collect()))
            .collect();
        assert_eq!(
            got,
            vec![
                ("fullscreen", vec!["F11"]),
                ("text-larger", vec!["<Control>equal", "<Control>KP_Add"]),
                ("text-smaller", vec!["<Control>minus", "<Control>KP_Subtract"]),
                ("text-reset", vec!["<Control>0", "<Control>KP_0", "<Control>KP_Insert"]),
            ]
            .into_iter()
            .map(|(action, accels)| (
                action,
                accels.into_iter().map(|a| Chord::from_accel(a).unwrap()).collect()
            ))
            .collect::<Vec<_>>()
        );
        assert_eq!(
            chords_of("text-reset").iter().map(RootChord::chord).collect::<Vec<_>>(),
            ["<Control>0", "<Control>KP_0", "<Control>KP_Insert"].map(|a| Chord::from_accel(a).unwrap())
        );
    }

    #[test]
    #[should_panic(expected = "hint")]
    fn an_action_with_no_row_is_a_programming_error() {
        chords_of("hint");
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

    /// In panel INPUT, Ctrl+y approves the oldest waiting card through the page, like Ctrl+Enter.
    /// Default root accelerators, capture navigation and prefix bindings must leave both chords
    /// to the WebView; otherwise the advertised key would do nothing. Explicit user bindings
    /// may still claim the chord.
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
            assert_ne!(chord, ctrl_y, "{what} would take the agent panel's INPUT Ctrl+y");
        }
        let prefix = Chord::of(
            &super::super::key::KeySpec::parse(super::super::stock::STOCK_PREFIX).expect("the stock prefix parses"),
        );
        assert_ne!(
            prefix, ctrl_y,
            "the default prefix would take the agent panel's INPUT Ctrl+y"
        );
    }

    #[test]
    fn spell_chord_reads_a_root_chord_as_a_person_types_it() {
        let spelled = |action: &str| chords_of(action).iter().map(spell_chord).collect::<Vec<_>>();
        assert_eq!(spelled("fullscreen"), ["F11"]);
        assert_eq!(spelled("text-larger"), ["Ctrl+=", "Ctrl+Keypad+"]);
        assert_eq!(spelled("text-smaller"), ["Ctrl+-", "Ctrl+Keypad-"]);
        assert_eq!(
            spelled("text-reset"),
            ["Ctrl+0", "Ctrl+Keypad0", "Ctrl+Keypad0 (NumLock off)"]
        );
    }

    #[test]
    fn every_root_accelerator_and_every_nav_key_is_in_the_help() {
        let rows = help_rows();
        let keys: Vec<&str> = rows.iter().flat_map(|r| r.keys.split(" / ")).collect();
        for row in ROOT_ACCELS {
            for root in row.chords {
                assert!(
                    keys.contains(&spell_chord(root).as_str()),
                    "{root:?} missing from {keys:?}"
                );
            }
        }
        for (letter, _) in NAV_KEYS {
            assert!(keys.contains(&format!("Ctrl+{letter}").as_str()), "Ctrl+{letter}");
        }
        assert!(rows.iter().any(|r| r.keys == "Ctrl+wheel"), "the per-pane wheel row");
        assert!(rows.iter().all(|r| !r.keys.contains("Shift")), "{rows:?}");
    }
}
