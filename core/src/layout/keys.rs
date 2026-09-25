//! Keys after `Ctrl+a` select modules: `e` the editor, `a` the agent, `t` the terminal,
//! and each Lua panel's own `key`. `v` is reserved for the canvas until it is available.
//!
//! A Lua panel's key is checked against the effective keymap after `init.lua` and against
//! stock tmux's prefix bindings (`crate::keymap::stock`). This prevents a panel from
//! shadowing an active binding or a tmux key that may be supported later. A clash is a
//! startup failure naming the key and binding, reported by `shell` as a `KeyError`.
//!
//! Capitals of built-in module keys are also refused: `E`, `A`, `T` and `V`.
//! After a split key, the prefix accepts `A` for `a` only after an exact match fails.
//! Shift can still be held from typing `"`, so a panel keyed `A` would intercept that
//! shortcut. Two Lua panels keyed `g` and `G` are allowed.

use std::fmt;

use super::module::{ModuleId, ModuleKind};
use super::tree::Layout;
use crate::keymap::{stock, Action, KeySpec, Keymap, Source};

/// The built-in modules' keys.
/// Their capitals, including the reserved canvas key's `V`, cannot be taken by a Lua panel:
/// [`BUILT_IN_CAPITALS`].
const BUILT_IN: [(char, fn() -> ModuleId); 3] = [
    ('e', ModuleId::editor),
    ('a', ModuleId::agent),
    ('t', ModuleId::terminal),
];

/// The Shift of each built-in module key, the canvas's `v` included: what the prefix reads as that
/// key after a split key (this module's doc).
pub const BUILT_IN_CAPITALS: [char; 4] = ['E', 'A', 'T', 'V'];

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
    /// Bound after the prefix in the effective table.
    Bound {
        panel: String,
        key: char,
        prefix: String,
        action: String,
        source: Source,
    },
    /// Not bound by neovibe, but stock tmux binds it after its prefix: kept free.
    StockTmux {
        panel: String,
        key: char,
        command: &'static str,
    },
    /// Another panel's key already, or a built-in module's.
    Taken { panel: String, key: char, by: String },
    /// The capital of a built-in module's key ([`BUILT_IN_CAPITALS`]).
    ShiftOfBuiltIn { panel: String, key: char },
}

impl fmt::Display for KeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KeyError::Malformed { panel, key } => write!(
                f,
                "neovibe.panel.register{{ id = {panel:?}, key = {key:?} }}: a key is one character, not a space"
            ),
            KeyError::Bound {
                panel,
                key,
                prefix,
                action,
                source,
            } => write!(
                f,
                "neovibe.panel.register{{ id = {panel:?}, key = \"{key}\" }}: '{key}' is bound after {prefix} to \
                 {action} ({source})"
            ),
            KeyError::StockTmux { panel, key, command } => write!(
                f,
                "neovibe.panel.register{{ id = {panel:?}, key = \"{key}\" }}: '{key}' is stock tmux's {command} \
                 after its prefix, kept free for neovibe"
            ),
            KeyError::Taken { panel, key, by } => write!(
                f,
                "neovibe.panel.register{{ id = {panel:?}, key = \"{key}\" }}: '{key}' is already {by:?}'s key"
            ),
            KeyError::ShiftOfBuiltIn { panel, key } => {
                let lower = key.to_ascii_lowercase();
                let whose = match lower {
                    'e' => "the editor's",
                    'a' => "the agent's",
                    't' => "the terminal's",
                    _ => "the canvas's, once it exists,",
                };
                write!(
                    f,
                    "neovibe.panel.register{{ id = {panel:?}, key = \"{key}\" }}: '{key}' is Shift+'{lower}', and \
                     after a split key neovibe reads it as '{lower}', {whose} key"
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
    /// registration order; a panel without one has `None`. `keymap` is the effective table after
    /// `init.lua`. The first key that cannot be one is the error.
    pub fn build(lua: &[(ModuleId, Option<String>)], keymap: &Keymap) -> Result<ModuleKeys, KeyError> {
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
            let spec = KeySpec::char(key, false, false);
            if let Some(binding) = keymap.lookup(&spec) {
                return Err(KeyError::Bound {
                    panel,
                    key,
                    prefix: keymap.prefix().human(),
                    action: binding.action.name(),
                    source: binding.source,
                });
            }
            if BUILT_IN_CAPITALS.contains(&key) {
                return Err(KeyError::ShiftOfBuiltIn { panel, key });
            }
            if let Some(command) = stock::stock_command(&spec) {
                return Err(KeyError::StockTmux { panel, key, command });
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
    /// In tmux spelling; several keys space-separated (`e M-e`), as the strip's verbs are.
    pub key: String,
    pub module: ModuleId,
    /// Hidden, or never placed: the strip dims it.
    pub dimmed: bool,
}

/// What the top bar lists after a split key: every module key, the built-ins first, a hidden module
/// dimmed. These are fixed whatever the user rebinds.
pub fn strip(keys: &ModuleKeys, layout: &Layout) -> Vec<StripEntry> {
    keys.entries()
        .iter()
        .map(|(key, module)| StripEntry {
            key: key.to_string(),
            module: module.clone(),
            dimmed: !layout.is_shown(module),
        })
        .collect()
}

/// What the top bar lists while the prefix is armed: for each module, in [`strip`]'s order, the keys
/// that really reach it now -- the effective table's `module.<id>` bindings, and a Lua panel's own
/// key, which the prefix answers when the table does not. A built-in whose key the user `del`ed or
/// rebound is listed under whatever key reaches it, or not at all; its after-a-split key (`strip`)
/// is unaffected.
pub fn strip_direct(keys: &ModuleKeys, keymap: &Keymap, layout: &Layout) -> Vec<StripEntry> {
    keys.entries()
        .iter()
        .filter_map(|(key, module)| {
            let mut direct: Vec<String> = keymap
                .keys_for(&Action::Module(module.clone()))
                .iter()
                .map(|k| k.to_string())
                .collect();
            if module.kind() == ModuleKind::LuaWebview && !direct.contains(&key.to_string()) {
                direct.push(key.to_string());
            }
            (!direct.is_empty()).then(|| StripEntry {
                key: direct.join(" "),
                module: module.clone(),
                dimmed: !layout.is_shown(module),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::super::geometry::{hide, Frame, Size};
    use super::super::module::{ModuleDecl, Placement};
    use super::*;
    use crate::keymap::{Keymap, KeymapOp};

    fn lua(id: &str, key: Option<&str>) -> (ModuleId, Option<String>) {
        (ModuleId::lua(id), key.map(str::to_string))
    }

    fn build_one(key: &str, keymap: &Keymap) -> Result<ModuleKeys, KeyError> {
        ModuleKeys::build(&[lua("notes", Some(key))], keymap)
    }

    #[test]
    fn the_built_ins_answer_to_e_a_and_t() {
        let keys = ModuleKeys::built_in();
        assert_eq!(keys.module('e'), Some(&ModuleId::editor()));
        assert_eq!(keys.module('a'), Some(&ModuleId::agent()));
        assert_eq!(keys.module('t'), Some(&ModuleId::terminal()));
        assert_eq!(keys.module('v'), None, "the canvas's key arrives with the canvas (P3)");
        assert!(keys.lua_keys().is_empty());
    }

    #[test]
    fn a_lua_panel_key_is_added_in_registration_order() {
        let keys = ModuleKeys::build(
            &[lua("notes", Some("N")), lua("plain", None), lua("todo", Some("g"))],
            &Keymap::defaults(),
        )
        .unwrap();
        assert_eq!(keys.module('N'), Some(&ModuleId::lua("notes")));
        assert_eq!(keys.key_of(&ModuleId::lua("todo")), Some('g'));
        assert_eq!(keys.key_of(&ModuleId::lua("plain")), None);
        assert_eq!(keys.lua_keys(), ['N', 'g']);
    }

    /// A Lua panel keyed `f`, `w` or `c` fails with a message naming the existing binding.
    #[test]
    fn a_lua_panel_keyed_f_w_or_c_fails_naming_the_binding() {
        for (key, action) in [('f', "hint"), ('w', "tab.choose"), ('c', "tab.new")] {
            let err = build_one(&key.to_string(), &Keymap::defaults()).unwrap_err();
            assert!(matches!(err, KeyError::Bound { key: k, .. } if k == key), "{err:?}");
            let text = err.to_string();
            assert!(text.contains("notes") && text.contains(&format!("'{key}'")), "{text}");
            assert!(text.contains(&format!("after Ctrl+b to {action} (default)")), "{text}");
        }
    }

    /// Every character the default table binds refuses a Lua panel, so a panel's key can never
    /// shadow one of neovibe's own (this replaces `shell`'s `every_bound_key_is_reserved_against_lua_panels`).
    #[test]
    fn every_character_the_effective_table_binds_refuses_a_lua_panel() {
        let keymap = Keymap::defaults();
        for binding in keymap.bindings() {
            let Some(c) = binding.key.as_char() else { continue };
            assert!(
                build_one(&c.to_string(), &keymap).is_err(),
                "{c} is bound but a panel could take it"
            );
        }
    }

    #[test]
    fn a_key_the_user_bound_names_the_users_binding() {
        let keymap = Keymap::apply_user(
            &[KeymapOp::Set {
                table: "prefix".into(),
                key: "g".into(),
                action: "zoom".into(),
                opts: vec![],
            }],
            &[],
        )
        .unwrap();
        let text = build_one("g", &keymap).unwrap_err().to_string();
        assert!(text.contains("to zoom (set earlier in init.lua)"), "{text}");
        assert!(build_one("g", &Keymap::defaults()).is_ok(), "free by default");
    }

    #[test]
    fn a_stock_tmux_key_neovibe_does_not_bind_stays_reserved() {
        for (key, command) in [
            ('s', "choose-tree"),
            ('o', "select-pane"),
            ('[', "copy-mode"),
            ('C', "customize-mode"),
        ] {
            let err = build_one(&key.to_string(), &Keymap::defaults()).unwrap_err();
            assert_eq!(
                err,
                KeyError::StockTmux {
                    panel: "notes".into(),
                    key,
                    command
                }
            );
            assert!(err.to_string().contains(command), "{err}");
        }
    }

    /// A key the user freed from neovibe's table is still stock tmux's (`f`), or a module key (`e`).
    #[test]
    fn a_key_deleted_from_the_table_is_still_refused_for_the_other_reasons() {
        let del = |k: &str| KeymapOp::Del {
            table: "prefix".into(),
            key: k.into(),
        };
        let keymap = Keymap::apply_user(&[del("f"), del("e")], &[]).unwrap();
        assert!(matches!(build_one("f", &keymap), Err(KeyError::StockTmux { .. })));
        assert!(matches!(build_one("e", &keymap), Err(KeyError::Taken { by, .. }) if by == "editor"));
    }

    /// A panel keyed `A` would intercept `Ctrl+a " a` with Shift still held from `"`,
    /// which should open the chat below. Capitals of built-in keys are refused,
    /// including `V` for the reserved canvas key `v`.
    #[test]
    fn the_capital_of_a_built_in_key_is_refused_naming_the_key() {
        for key in ['E', 'A', 'T', 'V'] {
            let err = build_one(&key.to_string(), &Keymap::defaults()).unwrap_err();
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
            ModuleKeys::build(&[lua("one", Some("g")), lua("two", Some("G"))], &Keymap::defaults()).is_ok(),
            "a Lua pair is the user's to choose"
        );
    }

    #[test]
    fn a_key_two_panels_want_is_refused_naming_both() {
        let err = ModuleKeys::build(&[lua("a1", Some("g")), lua("b2", Some("g"))], &Keymap::defaults()).unwrap_err();
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
                    ModuleKeys::build(&[lua("notes", Some(bad))], &Keymap::defaults()),
                    Err(KeyError::Malformed { .. })
                ),
                "{bad:?}"
            );
        }
        assert!(
            ModuleKeys::build(&[lua("notes", Some("é"))], &Keymap::defaults()).is_ok(),
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
        let keys = ModuleKeys::build(&[lua("notes", Some("N"))], &Keymap::defaults()).unwrap();
        let shown: Vec<(String, bool)> = strip(&keys, &layout).into_iter().map(|e| (e.key, e.dimmed)).collect();
        let want = [("e", false), ("a", false), ("t", true), ("N", false)].map(|(k, d)| (k.to_string(), d));
        assert_eq!(shown, want);
        // Armed, the defaults reach every module by the same key.
        let armed: Vec<(String, bool)> = strip_direct(&keys, &Keymap::defaults(), &layout)
            .into_iter()
            .map(|e| (e.key, e.dimmed))
            .collect();
        let after_split: Vec<(String, bool)> = strip(&keys, &layout).into_iter().map(|e| (e.key, e.dimmed)).collect();
        assert_eq!(armed, after_split);
    }

    /// A built-in whose direct key is `del`ed or moved is listed, armed, under the key that really
    /// reaches it, or not at all -- while its after-a-split key stays.
    #[test]
    fn the_armed_strip_follows_a_deleted_or_rebound_module_key() {
        let layout = Layout::initial(&[ModuleDecl {
            id: ModuleId::terminal(),
            placement: Placement::BelowRoot,
        }])
        .unwrap();
        let map = Keymap::apply_user(
            &[
                KeymapOp::Del {
                    table: "prefix".into(),
                    key: "e".into(),
                },
                KeymapOp::Del {
                    table: "prefix".into(),
                    key: "t".into(),
                },
                KeymapOp::Set {
                    table: "prefix".into(),
                    key: "M-t".into(),
                    action: "module.terminal".into(),
                    opts: Vec::new(),
                },
            ],
            &[],
        )
        .unwrap();
        let keys = ModuleKeys::build(&[], &map).unwrap();
        let armed: Vec<(String, ModuleId)> = strip_direct(&keys, &map, &layout)
            .into_iter()
            .map(|e| (e.key, e.module))
            .collect();
        assert_eq!(
            armed,
            [
                ("a".to_string(), ModuleId::agent()),
                ("M-t".to_string(), ModuleId::terminal())
            ]
        );
        let after_split: Vec<String> = strip(&keys, &layout).into_iter().map(|e| e.key).collect();
        assert_eq!(after_split, ["e", "a", "t"]);
    }
}
