//! Keys after `Ctrl+a` select modules: `e` the editor, `a` the agent, `t` the terminal,
//! and each Lua panel's own `key`. `c` is reserved for the canvas until it is available.
//!
//! A Lua panel's key is checked against [`RESERVED`], which includes bound keys and keys
//! kept for future bindings so an accepted panel key need not be taken back. A clash is
//! a startup failure naming the key, reported by `shell` as a `KeyError` after `init.lua`.
//!
//! Capitals of built-in module keys are also refused: `E`, `A`, `T` and `C`.
//! After `Ctrl+a "`, the prefix accepts `A` for `a` only after an exact match fails.
//! Shift can still be held from typing `"`, so a panel keyed `A` would open below instead
//! of the chat. Two Lua panels keyed `g` and `G` are allowed; after `"`, the capital
//! opens its own panel.

use std::fmt;

use super::module::ModuleId;
use super::tree::Layout;

/// Keys unavailable to Lua panels, including `n p w < >` reserved for future bindings
/// so an accepted panel key need not be taken back. The prefix `Ctrl+a` is a chord,
/// so it cannot appear in this character table.
pub const RESERVED: [char; 28] = [
    'm', 'z', 'h', 'j', 'k', 'l', 'x', 'q', 'H', 'J', 'K', 'L', '\\', '"', '|', '_', '-', '=', '0', 'e', 'a', 'c', 't',
    'n', 'p', 'w', '<', '>',
];

/// The built-in modules' keys.
/// Their capitals, and the canvas key's `C`, cannot be taken by a Lua panel:
/// [`BUILT_IN_CAPITALS`].
const BUILT_IN: [(char, fn() -> ModuleId); 3] = [
    ('e', ModuleId::editor),
    ('a', ModuleId::agent),
    ('t', ModuleId::terminal),
];

/// The Shift of each built-in module key, the canvas's included: what the prefix reads as that key
/// after `\` or `"` (this module's doc).
pub const BUILT_IN_CAPITALS: [char; 4] = ['E', 'A', 'T', 'C'];

/// Every module key this window answers to, in the order the prefix strip shows them: the built-ins,
/// then each Lua panel's in registration order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleKeys {
    keys: Vec<(char, ModuleId)>,
}

/// A Lua panel's `key` that cannot be one (`ModuleKeys::build`). `panel` is the panel's own `id`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyError {
    /// Not exactly one printable, non-space character.
    Malformed { panel: String, key: String },
    /// One of [`RESERVED`].
    Reserved { panel: String, key: char },
    /// Another panel's key already.
    Taken { panel: String, key: char, by: String },
    /// The capital of a built-in module's key ([`BUILT_IN_CAPITALS`]), which the prefix reads as
    /// that key after `\` or `"`.
    ShiftOfBuiltIn { panel: String, key: char },
}

impl fmt::Display for KeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let reserved: String = RESERVED.iter().map(|c| format!("{c} ")).collect();
        match self {
            KeyError::Malformed { panel, key } => write!(
                f,
                "neovibe.panel.register{{ id = {panel:?}, key = {key:?} }}: a key is one character, not a space"
            ),
            KeyError::Reserved { panel, key } => write!(
                f,
                "neovibe.panel.register{{ id = {panel:?}, key = \"{key}\" }}: '{key}' is neovibe's after Ctrl+a \
                 (reserved: {})",
                reserved.trim_end()
            ),
            KeyError::Taken { panel, key, by } => write!(
                f,
                "neovibe.panel.register{{ id = {panel:?}, key = \"{key}\" }}: '{key}' is already panel {by:?}'s key"
            ),
            KeyError::ShiftOfBuiltIn { panel, key } => {
                let lower = key.to_ascii_lowercase();
                let whose = match lower {
                    'e' => "the editor's",
                    'a' => "the agent's",
                    't' => "the terminal's",
                    _ => "the canvas's, from P3,",
                };
                write!(
                    f,
                    "neovibe.panel.register{{ id = {panel:?}, key = \"{key}\" }}: '{key}' is Shift+'{lower}', and \
                     after Ctrl+a \\ or Ctrl+a \" neovibe reads it as '{lower}', {whose} key"
                )
            }
        }
    }
}

impl std::error::Error for KeyError {}

impl ModuleKeys {
    /// The built-in keys alone: a window with no Lua panel keys.
    pub fn built_in() -> ModuleKeys {
        ModuleKeys {
            keys: BUILT_IN.iter().map(|(key, id)| (*key, id())).collect(),
        }
    }

    /// The built-ins plus each Lua panel's `key`, given as `(its module id, the key as written)` in
    /// registration order; a panel without one has `None`. The first key that cannot be one is the
    /// error.
    pub fn build(lua: &[(ModuleId, Option<String>)]) -> Result<ModuleKeys, KeyError> {
        let mut keys = ModuleKeys::built_in();
        for (id, key) in lua {
            let Some(text) = key else { continue };
            let panel = id.as_str().trim_start_matches("lua:").to_string();
            let mut chars = text.chars();
            let key = match (chars.next(), chars.next()) {
                (Some(c), None) if !c.is_whitespace() && !c.is_control() => c,
                _ => {
                    return Err(KeyError::Malformed {
                        panel,
                        key: text.clone(),
                    })
                }
            };
            if RESERVED.contains(&key) {
                return Err(KeyError::Reserved { panel, key });
            }
            if BUILT_IN_CAPITALS.contains(&key) {
                return Err(KeyError::ShiftOfBuiltIn { panel, key });
            }
            if let Some(by) = keys.module(key) {
                return Err(KeyError::Taken {
                    panel,
                    key,
                    by: by.as_str().trim_start_matches("lua:").to_string(),
                });
            }
            keys.keys.push((key, id.clone()));
        }
        Ok(keys)
    }

    /// The module `key` names.
    pub fn module(&self, key: char) -> Option<&ModuleId> {
        self.keys.iter().find(|(k, _)| *k == key).map(|(_, id)| id)
    }

    /// The key that names `id`.
    pub fn key_of(&self, id: &ModuleId) -> Option<char> {
        self.keys.iter().find(|(_, m)| m == id).map(|(k, _)| *k)
    }

    /// The Lua panels' keys, which the prefix answers to beyond its own table.
    pub fn lua_keys(&self) -> Vec<char> {
        self.keys
            .iter()
            .filter(|(_, id)| id.kind() == super::module::ModuleKind::LuaWebview)
            .map(|(k, _)| *k)
            .collect()
    }

    pub fn entries(&self) -> &[(char, ModuleId)] {
        &self.keys
    }
}

/// What `Ctrl+a <key>` does to module `id`, given whether it holds the keys now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyAction {
    /// Hidden: show it back where it was hidden from, and give it the keys.
    ShowAndFocus,
    /// Never placed: open it right of the module with the keys and give it focus.
    Place,
    /// Shown -- on screen, or zoomed away -- without the keys: give it them, ending a zoom that keeps
    /// it off screen first, as a move does (`Layout::zoom_hides`). The zoomed module itself keeps its
    /// zoom, as tmux's `select-pane` onto the zoomed pane does.
    Focus,
    /// It holds the keys: hide it. The keys go to its neighbour first (`geometry::hide`).
    Hide,
}

/// `Ctrl+a <key>` on `id`. The terminal's own `Ctrl+a t` is the same rule
/// (`shell::terminal::toggle_action`), with its shell started on the way.
pub fn key_action(layout: &Layout, id: &ModuleId, has_keys: bool) -> KeyAction {
    if !layout.contains(id) {
        KeyAction::Place
    } else if !layout.is_shown(id) {
        KeyAction::ShowAndFocus
    } else if has_keys {
        KeyAction::Hide
    } else {
        KeyAction::Focus
    }
}

/// One module key in the prefix strip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StripEntry {
    pub key: char,
    pub module: ModuleId,
    /// Hidden, or never placed: the strip dims it.
    pub dimmed: bool,
}

/// What the top bar lists while the prefix is armed: every module key, the built-ins first, a hidden
/// module dimmed.
pub fn strip(keys: &ModuleKeys, layout: &Layout) -> Vec<StripEntry> {
    keys.entries()
        .iter()
        .map(|(key, module)| StripEntry {
            key: *key,
            module: module.clone(),
            dimmed: !layout.is_shown(module),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::super::geometry::{hide, Frame, Size};
    use super::super::module::{ModuleDecl, Placement};
    use super::*;

    fn lua(id: &str, key: Option<&str>) -> (ModuleId, Option<String>) {
        (ModuleId::lua(id), key.map(str::to_string))
    }

    #[test]
    fn the_reserved_set_is_the_specs_list_verbatim() {
        let spec = "m z h j k l x q H J K L \\ \" | _ - = 0 e a c t n p w < >";
        let listed: Vec<char> = spec.split(' ').map(|t| t.chars().next().unwrap()).collect();
        assert_eq!(RESERVED.to_vec(), listed);
    }

    #[test]
    fn the_built_ins_answer_to_e_a_and_t() {
        let keys = ModuleKeys::built_in();
        assert_eq!(keys.module('e'), Some(&ModuleId::editor()));
        assert_eq!(keys.module('a'), Some(&ModuleId::agent()));
        assert_eq!(keys.module('t'), Some(&ModuleId::terminal()));
        assert_eq!(keys.module('c'), None, "the canvas's key arrives with the canvas (P3)");
        assert!(keys.lua_keys().is_empty());
    }

    #[test]
    fn a_lua_panel_key_is_added_in_registration_order() {
        let keys = ModuleKeys::build(&[lua("notes", Some("N")), lua("plain", None), lua("todo", Some("g"))]).unwrap();
        assert_eq!(keys.module('N'), Some(&ModuleId::lua("notes")));
        assert_eq!(keys.key_of(&ModuleId::lua("todo")), Some('g'));
        assert_eq!(keys.key_of(&ModuleId::lua("plain")), None);
        assert_eq!(keys.lua_keys(), ['N', 'g']);
    }

    /// A key collision fails startup with a message naming the key and the panel.
    #[test]
    fn every_reserved_key_is_refused_naming_the_key_and_the_panel() {
        for key in RESERVED {
            let err = ModuleKeys::build(&[lua("notes", Some(&key.to_string()))]).unwrap_err();
            assert_eq!(
                err,
                KeyError::Reserved {
                    panel: "notes".into(),
                    key
                }
            );
            let text = err.to_string();
            assert!(text.contains(&format!("'{key}'")) && text.contains("notes"), "{text}");
        }
    }

    /// A panel keyed `A` would intercept `Ctrl+a " a` with Shift still held from `"`,
    /// which should open the chat below. Capitals of built-in keys are refused,
    /// including `C` for the reserved canvas key `c`.
    #[test]
    fn the_capital_of_a_built_in_key_is_refused_naming_the_key() {
        for key in ['E', 'A', 'T', 'C'] {
            let err = ModuleKeys::build(&[lua("notes", Some(&key.to_string()))]).unwrap_err();
            assert_eq!(
                err,
                KeyError::ShiftOfBuiltIn {
                    panel: "notes".into(),
                    key
                }
            );
            let text = err.to_string();
            assert!(text.contains(&format!("'{key}'")) && text.contains("notes"), "{text}");
        }
        for (built_in, _) in BUILT_IN {
            assert!(BUILT_IN_CAPITALS.contains(&built_in.to_ascii_uppercase()), "{built_in}");
        }
        assert!(
            ModuleKeys::build(&[lua("one", Some("g")), lua("two", Some("G"))]).is_ok(),
            "a Lua pair is the user's to choose"
        );
    }

    #[test]
    fn a_key_two_panels_want_is_refused_naming_both() {
        let err = ModuleKeys::build(&[lua("a1", Some("g")), lua("b2", Some("g"))]).unwrap_err();
        assert_eq!(
            err,
            KeyError::Taken {
                panel: "b2".into(),
                key: 'g',
                by: "a1".into()
            }
        );
        assert!(err.to_string().contains("\"a1\""), "{err}");
    }

    #[test]
    fn a_key_that_is_not_one_printable_character_is_refused() {
        for bad in ["", "gg", " ", "\t", "\u{7}"] {
            assert!(
                matches!(
                    ModuleKeys::build(&[lua("notes", Some(bad))]),
                    Err(KeyError::Malformed { .. })
                ),
                "{bad:?}"
            );
        }
        assert!(
            ModuleKeys::build(&[lua("notes", Some("é"))]).is_ok(),
            "any one character"
        );
    }

    /// A module key shows, places, focuses or hides its module according to its current state.
    #[test]
    fn a_module_key_shows_places_focuses_or_hides() {
        let frame = Frame::new(Size { w: 1280, h: 721 }, 1);
        let mut layout = Layout::initial(&[ModuleDecl {
            id: ModuleId::lua("below"),
            placement: Placement::BelowRoot,
        }])
        .unwrap();
        hide(&mut layout, &ModuleId::lua("below"), &frame).unwrap();
        assert_eq!(
            key_action(&layout, &ModuleId::lua("below"), false),
            KeyAction::ShowAndFocus
        );
        assert_eq!(
            key_action(&layout, &ModuleId::parse("canvas").unwrap(), false),
            KeyAction::Place
        );
        assert_eq!(key_action(&layout, &ModuleId::agent(), false), KeyAction::Focus);
        assert_eq!(key_action(&layout, &ModuleId::editor(), true), KeyAction::Hide);
        layout.toggle_zoom(&ModuleId::editor());
        assert_eq!(
            key_action(&layout, &ModuleId::agent(), false),
            KeyAction::Focus,
            "zoomed away is shown"
        );
    }

    #[test]
    fn the_strip_lists_every_key_built_ins_first_and_dims_a_hidden_module() {
        let frame = Frame::new(Size { w: 1280, h: 721 }, 1);
        let mut layout = Layout::initial(&[
            ModuleDecl {
                id: ModuleId::terminal(),
                placement: Placement::BelowRoot,
            },
            ModuleDecl {
                id: ModuleId::lua("notes"),
                placement: Placement::RightOfRoot,
            },
        ])
        .unwrap();
        hide(&mut layout, &ModuleId::terminal(), &frame).unwrap();
        let keys = ModuleKeys::build(&[lua("notes", Some("N"))]).unwrap();
        let shown: Vec<(char, bool)> = strip(&keys, &layout).iter().map(|e| (e.key, e.dimmed)).collect();
        assert_eq!(shown, [('e', false), ('a', false), ('t', true), ('N', false)]);
    }
}
