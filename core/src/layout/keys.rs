//! The keys after `Ctrl+a` that name a module: `e` the editor, `a`
//! the agent, `t` the terminal, and each Lua panel's own `key`. `v` is the canvas's once it is
//! built, and is reserved until then ([`RESERVED_FOR_CANVAS`]): unbound in the
//! prefix table, but still refused to a Lua panel with its own `KeyError::Reserved`.
//!
//! **A Lua panel's key is checked against the effective keymap and stock tmux** (since
//! 2026-09-25, replacing a static `RESERVED` list): a key the prefix
//! table binds after `init.lua`, default or the user's, is refused naming that binding; so is any
//! key stock tmux binds after its prefix (`crate::keymap::stock`), so a panel never shadows a tmux
//! reflex a later phase might adopt. A clash is a startup failure naming the key (`KeyError`,
//! reported by `shell` after `init.lua` has run, the way `agent.font_size` is).
//!
//! **So is the capital of a built-in module's key** -- `E`, `A`, `T`, and `V` for the canvas's `v`
//! -- with its own message. After a split key the prefix takes `A` for `a` (`"` is Shift+' on the
//! US layout, and Shift is often still down), but only once an exact match has failed: a panel
//! keyed `A` would take that slip. Two Lua panels keyed `g`
//! and `G` are allowed.

use std::fmt;

use super::module::{ModuleId, ModuleKind};
use super::tree::Layout;
use crate::keymap::{stock, Action, KeySpec, Keymap, Source};

/// The built-in modules' keys. The canvas's `v` joins them once the canvas is built.
///
/// Their capitals are not a Lua panel's to take (this module's doc): [`BUILT_IN_CAPITALS`].
const BUILT_IN: [(char, fn() -> ModuleId); 3] = [
    ('e', ModuleId::editor),
    ('a', ModuleId::agent),
    ('t', ModuleId::terminal),
];

/// The Shift of each built-in module key, the canvas's `v` included: what the prefix reads as that
/// key after a split key (this module's doc).
pub const BUILT_IN_CAPITALS: [char; 4] = ['E', 'A', 'T', 'V'];

/// `v`, kept free of the prefix table and of every Lua panel, so the canvas can take it once it is
/// built (this module's doc) without a v1 config having to give it back up.
pub const RESERVED_FOR_CANVAS: char = 'v';

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
    /// Not bound by Eitri, but stock tmux binds it after its prefix: kept free.
    StockTmux {
        panel: String,
        key: char,
        command: &'static str,
    },
    /// Another panel's key already, or a built-in module's.
    Taken { panel: String, key: char, by: String },
    /// The capital of a built-in module's key ([`BUILT_IN_CAPITALS`]).
    ShiftOfBuiltIn { panel: String, key: char },
    /// [`RESERVED_FOR_CANVAS`]: not bound today, but kept free for the canvas.
    Reserved { panel: String, key: char },
}

impl fmt::Display for KeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KeyError::Malformed { panel, key } => write!(
                f,
                "eitri.panel.register{{ id = {panel:?}, key = {key:?} }}: a key is one character, not a space"
            ),
            KeyError::Bound {
                panel,
                key,
                prefix,
                action,
                source,
            } => write!(
                f,
                "eitri.panel.register{{ id = {panel:?}, key = \"{key}\" }}: '{key}' is bound after {prefix} to \
                 {action} ({source})"
            ),
            KeyError::StockTmux { panel, key, command } => write!(
                f,
                "eitri.panel.register{{ id = {panel:?}, key = \"{key}\" }}: '{key}' is stock tmux's {command} \
                 after its prefix, kept free for Eitri"
            ),
            KeyError::Taken { panel, key, by } => write!(
                f,
                "eitri.panel.register{{ id = {panel:?}, key = \"{key}\" }}: '{key}' is already {by:?}'s key"
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
                    "eitri.panel.register{{ id = {panel:?}, key = \"{key}\" }}: '{key}' is Shift+'{lower}', and \
                     after a split key Eitri reads it as '{lower}', {whose} key"
                )
            }
            KeyError::Reserved { panel, key } => write!(
                f,
                "eitri.panel.register{{ id = {panel:?}, key = \"{key}\" }}: '{key}' is reserved for the canvas"
            ),
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
            keys.add_lua(id, key.as_deref(), keymap)?;
        }
        Ok(keys)
    }

    /// [`ModuleKeys::build`] for a window that opens whatever `init.lua` did: a panel whose key cannot be one has no
    /// key, and its error is returned beside the keys, in the order of the panels.
    pub fn build_settled(lua: &[(ModuleId, Option<String>)], keymap: &Keymap) -> (ModuleKeys, Vec<KeyError>) {
        let mut keys = ModuleKeys::built_in();
        let mut refused = Vec::new();
        for (id, key) in lua {
            if let Err(error) = keys.add_lua(id, key.as_deref(), keymap) {
                refused.push(error);
            }
        }
        (keys, refused)
    }

    /// Adds the Lua panel `id`'s key `text`, if it has one and can have it.
    fn add_lua(&mut self, id: &ModuleId, text: Option<&str>, keymap: &Keymap) -> Result<(), KeyError> {
        let Some(text) = text else { return Ok(()) };
        let panel = id.as_str().trim_start_matches("lua:").to_string();
        let mut chars = text.chars();
        let key = match (chars.next(), chars.next()) {
            (Some(c), None) if !c.is_whitespace() && !c.is_control() => c,
            _ => {
                return Err(KeyError::Malformed {
                    panel,
                    key: text.to_string(),
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
        if key == RESERVED_FOR_CANVAS {
            return Err(KeyError::Reserved { panel, key });
        }
        if BUILT_IN_CAPITALS.contains(&key) {
            return Err(KeyError::ShiftOfBuiltIn { panel, key });
        }
        if let Some(command) = stock::stock_command(&spec) {
            return Err(KeyError::StockTmux { panel, key, command });
        }
        if let Some(by) = self.module(key) {
            return Err(KeyError::Taken {
                panel,
                key,
                by: by.as_str().trim_start_matches("lua:").to_string(),
            });
        }
        self.keys.push((key, id.clone()));
        Ok(())
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
    /// Never placed (the canvas): open it right of the module with the keys, which gives it them.
    Place,
    /// Shown -- on screen, or zoomed away -- without the keys: give it them, ending a zoom that keeps
    /// it off screen first, as a move does (`Layout::zoom_hides`). The zoomed module itself keeps its
    /// zoom, as tmux's `select-pane` onto the zoomed pane does.
    Focus,
    /// It holds the keys: hide it. The keys go to its neighbour first (`geometry::hide`).
    Hide,
}

/// `Ctrl+a <key>` on `id` (its four cases). The terminal's own `Ctrl+a t` is the same rule
/// (`wm::verbs::modules`' terminal key), with its shell started on the way.
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

/// A kind's focused instance, then its most recently used instance, then its base id.
pub fn choose_instance(layout: &Layout, kind: ModuleKind) -> ModuleId {
    if layout.focus().kind() == kind {
        return layout.focus().clone();
    }
    if let Some(id) = layout.mru().iter().find(|id| id.kind() == kind) {
        return id.clone();
    }
    match kind {
        ModuleKind::Editor => ModuleId::editor(),
        ModuleKind::Agent => ModuleId::agent(),
        ModuleKind::Terminal => ModuleId::terminal(),
        ModuleKind::Canvas => ModuleId::parse("canvas").expect("a built-in id"),
        ModuleKind::LuaWebview => unreachable!("a Lua panel is addressed by its own id"),
    }
}

/// Base ids of kinds with instances follow their most recently used instance.
pub fn module_target(layout: &Layout, id: &ModuleId) -> ModuleId {
    if id.number() == 1 && id.kind().has_instances() {
        choose_instance(layout, id.kind())
    } else {
        id.clone()
    }
}

/// The module a hide key's module key hides: for a base id of a kind with instances, the instance with the keys when
/// the keys are in that kind, else the most recently used instance still shown, else [`module_target`]'s; any other
/// id is itself. [`module_target`]'s most recent instance may be hidden already, where a hide would do nothing while
/// another instance of the kind is on screen.
pub fn hide_target(layout: &Layout, id: &ModuleId) -> ModuleId {
    if id.number() == 1 && id.kind().has_instances() {
        if layout.focus().kind() == id.kind() {
            return layout.focus().clone();
        }
        if let Some(shown) = layout
            .mru()
            .iter()
            .find(|m| m.kind() == id.kind() && layout.is_shown(m))
        {
            return shown.clone();
        }
    }
    module_target(layout, id)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SplitChoice {
    NewInstance(ModuleKind),
    Existing(ModuleId),
}

pub fn split_choice(layout: &Layout, named: &ModuleId) -> SplitChoice {
    if named.kind().has_instances() && layout.focus().kind() == named.kind() {
        SplitChoice::NewInstance(named.kind())
    } else {
        SplitChoice::Existing(module_target(layout, named))
    }
}

fn instance_count(layout: &Layout, id: &ModuleId) -> usize {
    if id.kind().has_instances() {
        layout.modules().iter().filter(|m| m.kind() == id.kind()).count()
    } else {
        1
    }
}

fn new_terminal(layout: &Layout, id: &ModuleId) -> bool {
    id.kind() == ModuleKind::Terminal && matches!(split_choice(layout, id), SplitChoice::NewInstance(_))
}

/// One module key in the prefix strip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StripEntry {
    /// In tmux spelling; several keys space-separated (`e M-e`), as the strip's verbs are.
    pub key: String,
    pub module: ModuleId,
    /// Hidden, or never placed: the strip dims it.
    pub dimmed: bool,
    pub count: usize,
    pub new_instance: bool,
}

/// What the top bar lists after a split key: every module key, the built-ins first, a hidden module
/// dimmed. These are fixed whatever the user rebinds, so the strip always names the modules.
pub fn strip(keys: &ModuleKeys, layout: &Layout) -> Vec<StripEntry> {
    keys.entries()
        .iter()
        .map(|(key, module)| StripEntry {
            key: key.to_string(),
            module: module.clone(),
            dimmed: !layout.is_shown(module),
            count: instance_count(layout, module),
            new_instance: new_terminal(layout, module),
        })
        .collect()
}

/// What the top bar lists while the hide key waits: [`strip`]'s entries, each dimmed by whether the module its key
/// would hide ([`hide_target`]) is hidden already, not the base id: with the base terminal hidden and another
/// terminal shown, `t` still hides one, so it is not dimmed. A hide makes nothing, so no entry offers a new instance.
pub fn strip_for_hide(keys: &ModuleKeys, layout: &Layout) -> Vec<StripEntry> {
    strip(keys, layout)
        .into_iter()
        .map(|entry| StripEntry {
            dimmed: !layout.is_shown(&hide_target(layout, &entry.module)),
            new_instance: false,
            ..entry
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
                .map(ToString::to_string)
                .collect();
            if module.kind() == ModuleKind::LuaWebview && !direct.contains(&key.to_string()) {
                direct.push(key.to_string());
            }
            (!direct.is_empty()).then(|| StripEntry {
                key: direct.join(" "),
                module: module.clone(),
                dimmed: !layout.is_shown(module),
                count: instance_count(layout, module),
                new_instance: new_terminal(layout, module),
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
        assert_eq!(keys.module('v'), None, "the canvas's key arrives with the canvas");
        assert!(keys.lua_keys().is_empty());
    }

    /// Every module key the user guide's Lua examples give a page (`eitri.plugin.load(..., { key = ... })` and a
    /// `key = ...` field of `eitri.panel.register`) is one a window takes with the default table, so a reader who
    /// copies an example gets the key it names instead of a startup problem.
    #[test]
    fn every_module_key_in_the_guides_lua_examples_is_free_by_default() {
        let guide = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../publish/files/docs/guide");
        let keymap = Keymap::defaults();
        let mut seen = 0;
        let mut refused = Vec::new();
        for entry in std::fs::read_dir(&guide).expect("the guide directory") {
            let path = entry.expect("a guide entry").path();
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            let text = std::fs::read_to_string(&path).expect("a guide page");
            for (n, line) in text.lines().enumerate() {
                let Some(at) = line.find("key = \"") else { continue };
                if !(line.contains("eitri.plugin.load(") || line.trim_start().starts_with("key = \"")) {
                    continue;
                }
                let rest = &line[at + "key = \"".len()..];
                let Some(end) = rest.find('"') else { continue };
                let key = &rest[..end];
                seen += 1;
                if let Err(e) = build_one(key, &keymap) {
                    refused.push(format!("{}:{}: {e}", path.display(), n + 1));
                }
            }
        }
        assert!(seen >= 5, "the guide's examples were not found ({seen})");
        assert!(refused.is_empty(), "{refused:#?}");
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

    /// A Lua panel cannot claim reserved `f`, `w` or `c`; failure must name the conflicting binding.
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
    /// shadow one of Eitri's own (this replaces `shell`'s `every_bound_key_is_reserved_against_lua_panels`).
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
    fn a_stock_tmux_key_eitri_does_not_bind_stays_reserved() {
        // Bound keys fail `Bound` before the stock-key reservation check: `[` and `PPage`
        // enter copy mode, `;` and `o` select modules, `q` is `select.number`, `s` is
        // `workspace.choose` and `d` is `module.hide-by-key`. `D` (`choose-client`) remains an
        // unbound stock key.
        for (key, command) in [('D', "choose-client"), (']', "paste-buffer"), ('C', "customize-mode")] {
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

    /// A key the user freed from Eitri's table is still stock tmux's (`f`), or a module key (`e`).
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

    /// A panel keyed `A` must not take `Ctrl+a " a` with Shift still held from `"`, which opens
    /// chat below everywhere else. `V` is likewise reserved for the canvas's `v`, so a later canvas
    /// cannot take a key already assigned to a Lua panel.
    #[test]
    fn the_capital_of_a_built_in_key_is_refused_naming_the_key() {
        // `E` is also a built-in capital, but it is bound itself now (`layout.spread`), and a bound key is refused as
        // bound before the capital rule is read.
        assert!(matches!(
            build_one("E", &Keymap::defaults()),
            Err(KeyError::Bound { action, .. }) if action == "layout.spread"
        ));
        for key in ['A', 'T', 'V'] {
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

    /// `v` is unbound by default but reserved for the canvas, so a Lua panel cannot claim it.
    #[test]
    fn a_lua_panel_keyed_v_is_refused_reserved_for_the_canvas() {
        let keymap = Keymap::defaults();
        assert!(
            keymap.lookup(&KeySpec::parse("v").unwrap()).is_none(),
            "v is unbound by default"
        );
        let err = build_one("v", &keymap).unwrap_err();
        assert_eq!(
            err,
            KeyError::Reserved {
                panel: "notes".into(),
                key: 'v'
            }
        );
        assert!(err.to_string().contains("reserved for the canvas"), "{err}");
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

    /// A module key shows a hidden module, places an absent one, focuses a visible one, or hides the focused one.
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

    /// Deleting or moving a built-in direct key changes its listed, armed key to the one that
    /// actually reaches it, or removes it from the list; its after-a-split key stays available.
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

    fn instances() -> Layout {
        Layout::initial(&["terminal", "terminal#2", "terminal#3"].map(|name| ModuleDecl {
            id: ModuleId::parse(name).unwrap(),
            placement: Placement::BelowEditor,
        }))
        .unwrap()
    }

    #[test]
    fn the_key_of_a_kind_with_instances_picks_the_instance() {
        let mut layout = instances();
        let t2 = ModuleId::parse("terminal#2").unwrap();
        let t3 = ModuleId::parse("terminal#3").unwrap();
        layout.set_focus(&t2).unwrap();
        assert_eq!(choose_instance(&layout, ModuleKind::Terminal), t2);
        layout.set_focus(&t3).unwrap();
        layout.set_focus(&ModuleId::editor()).unwrap();
        assert_eq!(choose_instance(&layout, ModuleKind::Terminal), t3);
        assert_eq!(
            choose_instance(&Layout::initial(&[]).unwrap(), ModuleKind::Terminal),
            ModuleId::terminal()
        );
    }

    #[test]
    fn a_numbered_module_action_addresses_its_own_instance() {
        let mut layout = instances();
        let t2 = ModuleId::parse("terminal#2").unwrap();
        let t3 = ModuleId::parse("terminal#3").unwrap();
        layout.set_focus(&t3).unwrap();
        assert_eq!(module_target(&layout, &t2), t2);
        assert_eq!(module_target(&layout, &ModuleId::terminal()), t3);
        assert_eq!(module_target(&layout, &ModuleId::editor()), ModuleId::editor());
    }

    /// A hide key's module key hides the instance with the keys, else the most recently used one still shown, which
    /// may differ from the module key's target: that one may be hidden already, and hiding it would do nothing.
    #[test]
    fn a_hide_key_hides_the_shown_instance_most_recently_used() {
        let mut layout = instances();
        let t2 = ModuleId::parse("terminal#2").unwrap();
        let t3 = ModuleId::parse("terminal#3").unwrap();
        let frame = Frame::new(Size { w: 1280, h: 721 }, 1);
        layout.set_focus(&t2).unwrap();
        assert_eq!(
            hide_target(&layout, &ModuleId::terminal()),
            t2,
            "the instance with the keys"
        );
        layout.set_focus(&t3).unwrap();
        layout.set_focus(&ModuleId::editor()).unwrap();
        assert_eq!(hide_target(&layout, &ModuleId::terminal()), t3);
        hide(&mut layout, &t3, &frame).unwrap();
        assert_eq!(
            module_target(&layout, &ModuleId::terminal()),
            t3,
            "the module key would show it again"
        );
        assert_eq!(
            hide_target(&layout, &ModuleId::terminal()),
            t2,
            "the hide key hides one still shown"
        );
        assert_eq!(hide_target(&layout, &t3), t3, "a numbered id is its own instance");
        assert_eq!(hide_target(&layout, &ModuleId::editor()), ModuleId::editor());
    }

    /// The hide key's strip dims a kind by the instance its key would hide: with the base terminal and the base chat
    /// hidden and `terminal#2` and `agent#2` shown, `t` and `a` hide those, so neither is dimmed (the split key's strip,
    /// which names the base ids, dims both); once no instance of a kind is shown, its key is dimmed.
    #[test]
    fn the_hide_strip_dims_a_kind_by_the_instance_its_key_would_hide() {
        let decl = |name: &str, placement| ModuleDecl {
            id: ModuleId::parse(name).unwrap(),
            placement,
        };
        let mut layout = Layout::initial(&[
            decl("terminal", Placement::BelowEditor),
            decl("terminal#2", Placement::BelowEditor),
            decl("agent#2", Placement::RightOfRoot),
        ])
        .unwrap();
        let frame = Frame::new(Size { w: 1280, h: 721 }, 1);
        layout.set_focus(&ModuleId::editor()).unwrap();
        hide(&mut layout, &ModuleId::terminal(), &frame).unwrap();
        hide(&mut layout, &ModuleId::agent(), &frame).unwrap();
        let dimmed = |entries: Vec<StripEntry>| -> Vec<(String, bool)> {
            entries.into_iter().map(|e| (e.key, e.dimmed)).collect()
        };
        let keys = ModuleKeys::built_in();
        let (e, a, t) = ("e".to_string(), "a".to_string(), "t".to_string());
        assert_eq!(
            dimmed(strip(&keys, &layout)),
            [(e.clone(), false), (a.clone(), true), (t.clone(), true)],
            "the premise: the split key's strip dims the base ids"
        );
        assert_eq!(
            dimmed(strip_for_hide(&keys, &layout)),
            [(e.clone(), false), (a.clone(), false), (t.clone(), false)]
        );
        assert!(strip_for_hide(&keys, &layout).iter().all(|entry| !entry.new_instance));
        hide(&mut layout, &ModuleId::parse("terminal#2").unwrap(), &frame).unwrap();
        hide(&mut layout, &ModuleId::parse("agent#2").unwrap(), &frame).unwrap();
        assert_eq!(
            dimmed(strip_for_hide(&keys, &layout)),
            [(e, false), (a, true), (t, true)]
        );
    }

    #[test]
    fn a_split_key_from_inside_a_terminal_opens_a_new_one() {
        let mut layout = instances();
        layout.set_focus(&ModuleId::terminal()).unwrap();
        assert_eq!(
            split_choice(&layout, &ModuleId::terminal()),
            SplitChoice::NewInstance(ModuleKind::Terminal)
        );
        layout.set_focus(&ModuleId::editor()).unwrap();
        assert_eq!(
            split_choice(&layout, &ModuleId::terminal()),
            SplitChoice::Existing(choose_instance(&layout, ModuleKind::Terminal))
        );
        assert_eq!(
            split_choice(&layout, &ModuleId::editor()),
            SplitChoice::Existing(ModuleId::editor())
        );
    }

    #[test]
    fn the_prefix_strip_counts_instances_and_offers_a_new_terminal() {
        let mut layout = instances();
        let map = Keymap::defaults();
        let keys = ModuleKeys::build(&[], &map).unwrap();
        for focused in [ModuleId::terminal(), ModuleId::editor()] {
            layout.set_focus(&focused).unwrap();
            for entries in [strip(&keys, &layout), strip_direct(&keys, &map, &layout)] {
                let terminal = entries.iter().find(|e| e.module == ModuleId::terminal()).unwrap();
                assert_eq!(terminal.count, 3);
                assert_eq!(terminal.new_instance, focused == ModuleId::terminal());
                let editor = entries.iter().find(|e| e.module == ModuleId::editor()).unwrap();
                assert_eq!(editor.count, 1);
                assert!(!editor.new_instance);
            }
        }
    }

    #[test]
    fn a_lua_panel_with_module_key_o_is_reported_and_loses_it() {
        let map = Keymap::defaults();
        let (keys, errors) = ModuleKeys::build_settled(&[lua("notes", Some("O"))], &map);
        assert!(keys.module('O').is_none());
        assert_eq!(
            errors,
            [KeyError::Bound {
                panel: "notes".into(),
                key: 'O',
                prefix: map.prefix().human(),
                action: "project.choose".into(),
                source: crate::keymap::Source::Default,
            }]
        );
        let text = errors[0].to_string();
        assert!(text.contains("notes") && text.contains("project.choose"), "{text}");
    }
}
