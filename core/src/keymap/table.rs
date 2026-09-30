//! The prefix table: the defaults (spec §2.1, stock tmux), what `init.lua`'s `eitri.keymap` calls
//! make of it (`apply_user`), and the collision rules that are hard startup failures naming both
//! sides (spec §2.3: 1 -- `set` on a bound key; 2 -- the prefix equals a root chord; 4 -- a Lua
//! command's `keybinding` equals the prefix or a root chord). Rule 3, a Lua panel's key, is
//! `crate::layout::keys`'s, since it is checked where panel keys are built.

use std::fmt;

use super::action::{self, Action, ActionError, OptValue, SwapTarget, TabAction};
use super::key::{Chord, KeyName, KeyParseError, KeySpec};
use super::panel::PanelUserTable;
use super::root::{self, HelpRow};
use crate::layout::{Axis, Direction, ModuleId, ModuleKeys, ModuleKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Default,
    User,
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Source::Default => "default",
            Source::User => "set earlier in init.lua",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binding {
    pub key: KeySpec,
    pub action: Action,
    /// tmux's `-r`: runs again without the prefix within `repeat-time`.
    pub repeatable: bool,
    pub source: Source,
}

/// The effective prefix table: the prefix chord and what each key after it does, plus (Task 3)
/// `init.lua`'s recorded `"panel"` table ops -- unmerged with nvim's own mappings or the panel
/// defaults here; `eitri_core::keymap::panel::effective` does that merge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Keymap {
    prefix: KeySpec,
    bindings: Vec<Binding>,
    panel_user: PanelUserTable,
}

/// One `eitri.keymap` call, as `core::lua::keymap` recorded it -- raw, so every error is found
/// here, after `init.lua` has run, and becomes a hard startup failure rather than a Lua error
/// `shell` only logs.
#[derive(Debug, Clone, PartialEq)]
pub enum KeymapOp {
    Prefix {
        key: String,
    },
    Set {
        table: String,
        key: String,
        action: String,
        opts: Vec<(String, OptValue)>,
    },
    Del {
        table: String,
        key: String,
    },
    /// A call Lua could not record (an argument of the wrong type); the message names the call.
    Invalid(String),
}

impl KeymapOp {
    /// The call as the user wrote it, for an error message.
    pub fn call(&self) -> String {
        match self {
            KeymapOp::Prefix { key } => format!("eitri.keymap.prefix({key:?})"),
            KeymapOp::Set { table, key, .. } => format!("eitri.keymap.set({table:?}, {key:?}, …)"),
            KeymapOp::Del { table, key } => format!("eitri.keymap.del({table:?}, {key:?})"),
            KeymapOp::Invalid(message) => message.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeymapError {
    Recorded(String),
    UnknownTable {
        call: String,
        table: String,
    },
    BadKey {
        call: String,
        error: KeyParseError,
    },
    Action {
        call: String,
        error: ActionError,
    },
    AlreadyBound {
        call: String,
        key: KeySpec,
        bound_to: String,
        source: Source,
    },
    NotBound {
        call: String,
        key: KeySpec,
    },
    PrefixNotAChord {
        call: String,
    },
    PrefixIsRoot {
        call: String,
        root: String,
    },
    CommandKeybinding {
        command: String,
        keybinding: String,
        hits: String,
    },
    /// An `init.lua` `"panel"` table op `PanelUserTable::set`/`del` refused (Task 3): `why` is its
    /// message (already naming the key/action), `call` the op as the user wrote it.
    Panel {
        call: String,
        why: String,
    },
}

impl fmt::Display for KeymapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KeymapError::Recorded(message) => f.write_str(message),
            KeymapError::UnknownTable { call, table } => {
                write!(f, "{call}: {table:?} is not a table (v1 has \"prefix\" and \"panel\")")
            }
            KeymapError::BadKey { call, error } => write!(f, "{call}: {error}"),
            KeymapError::Action { call, error } => write!(f, "{call}: {error}"),
            KeymapError::AlreadyBound {
                call,
                key,
                bound_to,
                source,
            } => {
                let key = key.to_string();
                write!(
                    f,
                    "{call}: {key:?} is already bound to {bound_to} ({source}); eitri.keymap.del(\"prefix\", {key:?}) first"
                )
            }
            KeymapError::NotBound { call, key } => {
                write!(
                    f,
                    "{call}: {:?} is not bound, so there is nothing to delete",
                    key.to_string()
                )
            }
            KeymapError::PrefixNotAChord { call } => write!(
                f,
                "{call}: the prefix must be a chord (C- or M-) or a function key, or it would eat that key everywhere"
            ),
            KeymapError::PrefixIsRoot { call, root } => {
                write!(
                    f,
                    "{call}: that chord is already a root key, {root}; choose another prefix"
                )
            }
            KeymapError::CommandKeybinding {
                command,
                keybinding,
                hits,
            } => write!(
                f,
                "eitri.command.register{{ id = {command:?}, keybinding = {keybinding:?} }}: {keybinding} is {hits}"
            ),
            KeymapError::Panel { call, why } => write!(f, "{call}: {why}"),
        }
    }
}

impl std::error::Error for KeymapError {}

fn parse_key(op: &KeymapOp, text: &str) -> Result<KeySpec, KeymapError> {
    KeySpec::parse(text).map_err(|error| KeymapError::BadKey { call: op.call(), error })
}

/// `v1`'s two tables: `"prefix"` (this file) and `"panel"` (`PanelUserTable::set`/`del`, called
/// directly from the `Set`/`Del` arms below, since its key/action types differ from the prefix
/// table's). Anything else is `UnknownTable`.
fn prefix_table(op: &KeymapOp, table: &str) -> Result<(), KeymapError> {
    if table == "prefix" {
        Ok(())
    } else {
        Err(KeymapError::UnknownTable {
            call: op.call(),
            table: table.to_string(),
        })
    }
}

fn default_bindings() -> Vec<Binding> {
    let mut out = Vec::new();
    let mut bind = |key: &str, action: Action, repeatable: bool| {
        out.push(Binding {
            key: KeySpec::parse(key).expect("a default key parses"),
            action,
            repeatable,
            source: Source::Default,
        })
    };
    let arrows = [
        ("Up", Direction::Up),
        ("Down", Direction::Down),
        ("Left", Direction::Left),
        ("Right", Direction::Right),
    ];
    bind("C-b", Action::SendPrefix, false);
    bind("%", Action::Split(Axis::Row), false);
    bind("\"", Action::Split(Axis::Column), false);
    bind("z", Action::Zoom, false);
    for (name, dir) in arrows {
        bind(&format!("C-{name}"), Action::Resize { dir, cells: 1 }, true);
    }
    for (name, dir) in arrows {
        bind(&format!("M-{name}"), Action::Resize { dir, cells: 5 }, true);
    }
    for (name, dir) in arrows {
        bind(name, Action::Select(dir), true);
    }
    // Stock tmux's `last-pane` and `select-pane -t :.+`, neither with `-r` (v1 picks, 2026-09-29).
    bind(";", Action::SelectLast, false);
    bind("o", Action::SelectNext, false);
    bind("{", Action::Swap(SwapTarget::Prev), false);
    bind("}", Action::Swap(SwapTarget::Next), false);
    bind("M-1", Action::Even(Axis::Row), false);
    bind("M-2", Action::Even(Axis::Column), false);
    bind("x", Action::ModuleKill, false);
    bind("c", Action::Tab(TabAction::New), false);
    bind("n", Action::Tab(TabAction::Next), false);
    bind("p", Action::Tab(TabAction::Prev), false);
    bind("l", Action::Tab(TabAction::Last), false);
    for n in 1..=9u8 {
        bind(&n.to_string(), Action::Tab(TabAction::Select(n)), false);
    }
    bind(",", Action::Tab(TabAction::Rename), false);
    bind("&", Action::Tab(TabAction::Close), false);
    bind("w", Action::Tab(TabAction::Choose), false);
    bind("i", Action::Tab(TabAction::Info), false);
    bind("f", Action::Hint, false);
    bind("r", Action::PanelReload, false);
    bind("?", Action::PanelKeymap, false);
    // Owner decision #28 (K16): stock tmux's `command-prompt`, which opens the panel's `:` line.
    bind(":", Action::PanelCommandLine, false);
    bind(
        "C-l",
        Action::SendKeys(KeySpec::parse("C-l").expect("C-l parses")),
        false,
    );
    // P4: bound like `C-l` above -- the literal chord, to whichever pane holds the keys
    // (vim-tmux-navigator's README: `bind C-l send-keys 'C-l'`). `Ctrl+h/j/k/l` themselves (with no
    // prefix) stay Eitri's own navigation (R38), unaffected by these.
    for name in ["C-h", "C-j", "C-k"] {
        bind(
            name,
            Action::SendKeys(KeySpec::parse(name).expect("a literal chord parses")),
            false,
        );
    }
    bind("F11", Action::WindowImmersive, false);
    bind("e", Action::Module(ModuleId::editor()), false);
    bind("a", Action::Module(ModuleId::agent()), false);
    bind("t", Action::Module(ModuleId::terminal()), false);
    // P5 (tmux `copy-mode` / `copy-mode -u`): `v` is deliberately not bound here any more (P6) --
    // reserved for the canvas (`crate::layout::keys::RESERVED_FOR_CANVAS`), not this table.
    bind("[", Action::CopyMode { up: false }, false);
    bind("PPage", Action::CopyMode { up: true }, false);
    out
}

impl Keymap {
    /// Stock tmux's prefix `C-b` and spec §2.1's table.
    pub fn defaults() -> Keymap {
        Keymap {
            prefix: KeySpec::parse(super::stock::STOCK_PREFIX).expect("C-b parses"),
            bindings: default_bindings(),
            panel_user: PanelUserTable::default(),
        }
    }

    /// The defaults with `ops` applied in order. `lua_panels`: the ids `module.<id>` may name.
    pub fn apply_user(ops: &[KeymapOp], lua_panels: &[String]) -> Result<Keymap, KeymapError> {
        let mut map = Keymap::defaults();
        let mut prefix_call = None;
        for op in ops {
            match op {
                KeymapOp::Invalid(message) => return Err(KeymapError::Recorded(message.clone())),
                KeymapOp::Prefix { key } => {
                    let spec = parse_key(op, key)?;
                    if !(spec.ctrl || spec.meta || matches!(spec.key, KeyName::F(_))) {
                        return Err(KeymapError::PrefixNotAChord { call: op.call() });
                    }
                    map.prefix = spec;
                    prefix_call = Some(op.call());
                }
                KeymapOp::Set {
                    table,
                    key,
                    action,
                    opts,
                } => {
                    if table == "panel" {
                        map.panel_user
                            .set(key, action)
                            .map_err(|why| KeymapError::Panel { call: op.call(), why })?;
                    } else {
                        prefix_table(op, table)?;
                        let spec = parse_key(op, key)?;
                        let parsed = action::parse(action, opts, lua_panels)
                            .map_err(|error| KeymapError::Action { call: op.call(), error })?;
                        if let Some(existing) = map.lookup(&spec) {
                            return Err(KeymapError::AlreadyBound {
                                call: op.call(),
                                key: spec,
                                bound_to: existing.action.name(),
                                source: existing.source,
                            });
                        }
                        map.bindings.push(Binding {
                            key: spec,
                            action: parsed.action,
                            repeatable: parsed.repeatable,
                            source: Source::User,
                        });
                    }
                }
                KeymapOp::Del { table, key } => {
                    if table == "panel" {
                        map.panel_user
                            .del(key)
                            .map_err(|why| KeymapError::Panel { call: op.call(), why })?;
                    } else {
                        prefix_table(op, table)?;
                        let spec = parse_key(op, key)?;
                        let at =
                            map.bindings
                                .iter()
                                .position(|b| b.key == spec)
                                .ok_or_else(|| KeymapError::NotBound {
                                    call: op.call(),
                                    key: spec,
                                })?;
                        map.bindings.remove(at);
                    }
                }
            }
        }
        let chord = Chord::of(&map.prefix);
        if let Some((_, named)) = root::chords().into_iter().find(|(c, _)| *c == chord) {
            return Err(KeymapError::PrefixIsRoot {
                call: prefix_call.unwrap_or_else(|| format!("the prefix {}", map.prefix)),
                root: named,
            });
        }
        Ok(map)
    }

    pub fn prefix(&self) -> &KeySpec {
        &self.prefix
    }

    pub fn lookup(&self, key: &KeySpec) -> Option<&Binding> {
        self.bindings.iter().find(|b| b.key == *key)
    }

    pub fn bindings(&self) -> &[Binding] {
        &self.bindings
    }

    /// `init.lua`'s recorded `"panel"` table ops (Task 3); `panel::effective` merges them with nvim's
    /// own mappings and the panel's defaults.
    pub fn panel_user(&self) -> &PanelUserTable {
        &self.panel_user
    }

    /// Every key bound to `action`, in table order.
    pub fn keys_for(&self, action: &Action) -> Vec<KeySpec> {
        self.bindings
            .iter()
            .filter(|b| b.action == *action)
            .map(|b| b.key)
            .collect()
    }

    /// The overlay's "After <prefix>" rows: one per binding, adjacent bindings that mean the same
    /// thing joined with ` / ` under one prefix (`Ctrl+b Up / Down / Left / Right`), then one row per
    /// Lua panel key. A panel's key is no binding -- the prefix answers it when the table does not
    /// (`ModuleKeys::build` refuses one the table binds) -- so without its own row the overlay would
    /// name it only inside the split rows' list of module keys.
    pub fn help(&self, keys: &ModuleKeys) -> Vec<HelpRow> {
        let prefix = self.prefix.human();
        let listed: Vec<String> = keys.entries().iter().map(|(k, _)| k.to_string()).collect();
        let module_keys = listed.join(" / ");
        let mut rows: Vec<(Vec<String>, String)> = Vec::new();
        for binding in &self.bindings {
            let mut what = binding.action.describe(&prefix, &module_keys);
            if binding.repeatable {
                what.push_str(" (repeats within 500 ms)");
            }
            let key = binding.key.human();
            match rows.last_mut() {
                Some((keys, last)) if *last == what => keys.push(key),
                _ => rows.push((vec![key], what)),
            }
        }
        for (key, id) in keys.entries() {
            if id.kind() == ModuleKind::LuaWebview {
                rows.push((
                    vec![key.to_string()],
                    Action::Module(id.clone()).describe(&prefix, &module_keys),
                ));
            }
        }
        rows.into_iter()
            .map(|(keys, what)| HelpRow {
                keys: format!("{prefix} {}", keys.join(" / ")),
                what,
            })
            .collect()
    }

    /// The verbs the top bar's strip lists after the module keys (ruling 13): the keys bound to
    /// kill, hide, split right, split below, even and swap, in that order, in tmux spelling. A verb
    /// nothing is bound to is left out (`hide`, by default, since `x` kills).
    pub fn strip_verbs(&self) -> Vec<String> {
        type Is = fn(&Action) -> bool;
        let groups: [(&str, Is); 6] = [
            ("kill", |a| matches!(a, Action::ModuleKill)),
            ("hide", |a| matches!(a, Action::ModuleHide)),
            ("right", |a| matches!(a, Action::Split(Axis::Row))),
            ("below", |a| matches!(a, Action::Split(Axis::Column))),
            ("even", |a| matches!(a, Action::Even(_))),
            ("swap", |a| matches!(a, Action::Swap(_))),
        ];
        groups
            .iter()
            .filter_map(|(label, is)| {
                let keys: Vec<String> = self
                    .bindings
                    .iter()
                    .filter(|b| is(&b.action))
                    .map(|b| b.key.to_string())
                    .collect();
                (!keys.is_empty()).then(|| format!("{} {label}", keys.join(" ")))
            })
            .collect()
    }
}

/// Collision rule 4: an `eitri.command.register{ keybinding }` accelerator that is the prefix chord
/// or a root chord would never fire (the prefix and the root controllers see the key first). An
/// accelerator `Chord` cannot read uses a modifier Eitri never binds, so it cannot collide.
pub fn check_command_keybinding(command: &str, keybinding: &str, keymap: &Keymap) -> Result<(), KeymapError> {
    let Ok(chord) = Chord::from_gtk(keybinding) else {
        return Ok(());
    };
    let hits = if chord == Chord::of(keymap.prefix()) {
        Some(format!("the prefix, {} (eitri.keymap)", keymap.prefix().human()))
    } else {
        root::chords()
            .into_iter()
            .find(|(c, _)| *c == chord)
            .map(|(_, named)| named)
    };
    match hits {
        Some(hits) => Err(KeymapError::CommandKeybinding {
            command: command.to_string(),
            keybinding: keybinding.to_string(),
            hits,
        }),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keymap::stock::{STOCK_PREFIX, STOCK_TMUX_PREFIX};

    fn set(key: &str, action: &str) -> KeymapOp {
        KeymapOp::Set {
            table: "prefix".into(),
            key: key.into(),
            action: action.into(),
            opts: vec![],
        }
    }
    fn del(key: &str) -> KeymapOp {
        KeymapOp::Del {
            table: "prefix".into(),
            key: key.into(),
        }
    }
    fn prefix(key: &str) -> KeymapOp {
        KeymapOp::Prefix { key: key.into() }
    }
    fn k(name: &str) -> KeySpec {
        KeySpec::parse(name).unwrap()
    }

    /// Spec §2.1's table, row by row, as `(key, action with its options, -r)`. Copied from the spec,
    /// not from the code: a change to either is a change to both.
    const DEFAULTS_FIXTURE: &[(&str, &str, bool)] = &[
        ("C-b", "send-prefix", false),
        ("%", "split.right", false),
        ("\"", "split.below", false),
        ("z", "zoom", false),
        ("C-Up", "resize.up cells=1", true),
        ("C-Down", "resize.down cells=1", true),
        ("C-Left", "resize.left cells=1", true),
        ("C-Right", "resize.right cells=1", true),
        ("M-Up", "resize.up cells=5", true),
        ("M-Down", "resize.down cells=5", true),
        ("M-Left", "resize.left cells=5", true),
        ("M-Right", "resize.right cells=5", true),
        ("Up", "select.up", true),
        ("Down", "select.down", true),
        ("Left", "select.left", true),
        ("Right", "select.right", true),
        (";", "select.last", false),
        ("o", "select.next", false),
        ("{", "swap.prev", false),
        ("}", "swap.next", false),
        ("M-1", "layout.even-horizontal", false),
        ("M-2", "layout.even-vertical", false),
        ("x", "module.kill", false),
        ("c", "tab.new", false),
        ("n", "tab.next", false),
        ("p", "tab.prev", false),
        ("l", "tab.last", false),
        ("1", "tab.select n=1", false),
        ("2", "tab.select n=2", false),
        ("3", "tab.select n=3", false),
        ("4", "tab.select n=4", false),
        ("5", "tab.select n=5", false),
        ("6", "tab.select n=6", false),
        ("7", "tab.select n=7", false),
        ("8", "tab.select n=8", false),
        ("9", "tab.select n=9", false),
        (",", "tab.rename", false),
        ("&", "tab.close", false),
        ("w", "tab.choose", false),
        ("i", "tab.info", false),
        ("f", "hint", false),
        ("r", "panel.reload", false),
        ("?", "panel.keymap", false),
        // Owner decision #28 (K16): tmux's `command-prompt`, here the panel's `:` line, which runs nothing.
        (":", "panel.command-line", false),
        ("C-l", "send-keys keys=C-l", false),
        ("C-h", "send-keys keys=C-h", false),
        ("C-j", "send-keys keys=C-j", false),
        ("C-k", "send-keys keys=C-k", false),
        ("F11", "window.immersive", false),
        ("e", "module.editor", false),
        ("a", "module.agent", false),
        ("t", "module.terminal", false),
        ("[", "copy-mode", false),
        ("PPage", "copy-mode up=true", false),
    ];

    fn spelled(action: &Action) -> String {
        let mut out = action.name();
        for (name, value) in action.options() {
            let value = match value {
                OptValue::Int(i) => i.to_string(),
                OptValue::Str(s) => s,
                OptValue::Bool(b) => b.to_string(),
                other => format!("{other:?}"),
            };
            out.push_str(&format!(" {name}={value}"));
        }
        out
    }

    #[test]
    fn the_default_table_is_the_specs_table() {
        let map = Keymap::defaults();
        assert_eq!(map.prefix().to_string(), STOCK_PREFIX);
        let got: Vec<(String, String, bool)> = map
            .bindings()
            .iter()
            .map(|b| (b.key.to_string(), spelled(&b.action), b.repeatable))
            .collect();
        let want: Vec<(String, String, bool)> = DEFAULTS_FIXTURE
            .iter()
            .map(|(k, a, r)| (k.to_string(), a.to_string(), *r))
            .collect();
        assert_eq!(got, want);
        assert!(map.bindings().iter().all(|b| b.source == Source::Default));
    }

    /// Owner decision #28 (K16): `prefix :` (tmux `command-prompt`) used to be swallowed -- an armed
    /// prefix reads the next key from its own table and eats one it leaves unbound -- and the letters
    /// typed after it ran as panel keys. It is bound now, to the panel's `:` line, never repeatable
    /// (stock tmux binds it without `-r`), and the `?` overlay's prefix section says what it does.
    #[test]
    fn prefix_colon_opens_the_panels_command_line() {
        let map = Keymap::defaults();
        let bound = map
            .lookup(&k(":"))
            .unwrap_or_else(|| panic!("prefix : is unbound, so the armed prefix swallows it"));
        assert_eq!(spelled(&bound.action), "panel.command-line");
        assert!(!bound.repeatable);
        assert_eq!(map.keys_for(&bound.action), vec![k(":")]);
    }

    /// Every default key is stock tmux's, with stock's `-r`, unless the spec marks it eitri-only.
    #[test]
    fn every_default_is_a_stock_tmux_key_or_marked_eitri_only() {
        // `[` and `PPage` are stock tmux's own keys too (both non-repeatable, like our `copy-mode`
        // bindings), so they need no entry here even though Eitri's action differs from tmux's.
        const EITRI_ONLY: [&str; 7] = ["C-l", "C-h", "C-j", "C-k", "F11", "e", "a"];
        for binding in Keymap::defaults().bindings() {
            let name = binding.key.to_string();
            if EITRI_ONLY.contains(&name.as_str()) {
                continue;
            }
            let (_, repeat, _) = STOCK_TMUX_PREFIX
                .iter()
                .find(|(key, _, _)| *key == name)
                .unwrap_or_else(|| panic!("{name} is neither stock tmux's nor marked eitri-only"));
            assert_eq!(binding.repeatable, *repeat, "{name}: tmux binds it with -r = {repeat}");
        }
    }

    /// Collision rule 1, the spec's own example message.
    #[test]
    fn set_on_a_default_key_fails_naming_both_bindings() {
        let err = Keymap::apply_user(&[set("l", "resize.right")], &[]).unwrap_err();
        assert_eq!(
            err.to_string(),
            "eitri.keymap.set(\"prefix\", \"l\", …): \"l\" is already bound to tab.last (default); \
             eitri.keymap.del(\"prefix\", \"l\") first"
        );
    }

    #[test]
    fn a_second_set_on_the_same_key_names_the_first_user_binding() {
        let err = Keymap::apply_user(&[set("m", "zoom"), set("m", "module.hide")], &[])
            .unwrap_err()
            .to_string();
        assert!(err.contains("already bound to zoom (set earlier in init.lua)"), "{err}");
    }

    #[test]
    fn del_then_set_succeeds_and_the_new_binding_is_the_users() {
        let map = Keymap::apply_user(&[del("l"), set("l", "resize.right")], &[]).unwrap();
        let binding = map.lookup(&k("l")).unwrap();
        assert_eq!(
            binding.action,
            Action::Resize {
                dir: Direction::Right,
                cells: 1
            }
        );
        assert_eq!(binding.source, Source::User);
    }

    #[test]
    fn del_of_an_unbound_key_fails_naming_it() {
        let err = Keymap::apply_user(&[del("q")], &[]).unwrap_err().to_string();
        assert!(
            err.contains("eitri.keymap.del(\"prefix\", \"q\")") && err.contains("not bound"),
            "{err}"
        );
    }

    /// Collision rule 2.
    #[test]
    fn a_prefix_that_is_a_root_chord_fails_naming_both() {
        let err = Keymap::apply_user(&[prefix("C-h")], &[]).unwrap_err().to_string();
        assert!(
            err.contains("eitri.keymap.prefix(\"C-h\")") && err.contains("Ctrl+h (move to the module left)"),
            "{err}"
        );
        let err = Keymap::apply_user(&[prefix("C-=")], &[]).unwrap_err().to_string();
        assert!(err.contains("Ctrl+= (text size larger"), "{err}");
    }

    #[test]
    fn the_prefix_must_be_a_chord_and_the_last_call_wins() {
        assert!(Keymap::apply_user(&[prefix("a")], &[])
            .unwrap_err()
            .to_string()
            .contains("chord"));
        assert_eq!(Keymap::apply_user(&[prefix("F12")], &[]).unwrap().prefix(), &k("F12"));
        assert_eq!(Keymap::apply_user(&[prefix("M-a")], &[]).unwrap().prefix(), &k("M-a"));
        assert_eq!(
            Keymap::apply_user(&[prefix("C-x"), prefix("C-a")], &[])
                .unwrap()
                .prefix(),
            &k("C-a")
        );
    }

    #[test]
    fn a_bad_table_key_action_or_option_is_named() {
        let root = KeymapOp::Set {
            table: "root".into(),
            key: "x".into(),
            action: "zoom".into(),
            opts: vec![],
        };
        assert!(Keymap::apply_user(&[root], &[])
            .unwrap_err()
            .to_string()
            .contains("\"root\""));
        assert!(Keymap::apply_user(&[set("Foo", "zoom")], &[])
            .unwrap_err()
            .to_string()
            .contains("\"Foo\""));
        assert!(Keymap::apply_user(&[set("g", "zoomm")], &[])
            .unwrap_err()
            .to_string()
            .contains("zoomm"));
        let with_opt = KeymapOp::Set {
            table: "prefix".into(),
            key: "g".into(),
            action: "zoom".into(),
            opts: vec![("cells".into(), OptValue::Int(2))],
        };
        assert!(Keymap::apply_user(&[with_opt], &[])
            .unwrap_err()
            .to_string()
            .contains("cells"));
    }

    #[test]
    fn a_call_lua_could_not_record_is_the_error() {
        let err = Keymap::apply_user(&[KeymapOp::Invalid("eitri.keymap.set(...): boom".into())], &[]).unwrap_err();
        assert_eq!(err.to_string(), "eitri.keymap.set(...): boom");
    }

    /// Collision rule 4.
    #[test]
    fn a_command_keybinding_on_the_prefix_or_a_root_chord_fails_naming_both() {
        let map = Keymap::defaults();
        let err = check_command_keybinding("notes.open", "<Control>b", &map)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("notes.open") && err.contains("<Control>b") && err.contains("the prefix, Ctrl+b"),
            "{err}"
        );
        let err = check_command_keybinding("x", "<Control>h", &map)
            .unwrap_err()
            .to_string();
        assert!(err.contains("Ctrl+h (move to the module left)"), "{err}");
        assert!(check_command_keybinding("x", "<Control>equal", &map).is_err());
        assert!(check_command_keybinding("x", "<Control>g", &map).is_ok());
        assert!(
            check_command_keybinding("x", "<Super>x", &map).is_ok(),
            "a chord Eitri never binds"
        );
        let custom = Keymap::apply_user(&[prefix("C-a")], &[]).unwrap();
        assert!(check_command_keybinding("x", "<Control>a", &custom).is_err());
        assert!(check_command_keybinding("x", "<Control>b", &custom).is_ok());
    }

    #[test]
    fn help_gives_a_lua_panel_key_a_row_of_its_own() {
        let map = Keymap::defaults();
        let keys = ModuleKeys::build(&[(ModuleId::lua("notes"), Some("g".to_string()))], &map).unwrap();
        let rows = map.help(&keys);
        let row = rows
            .iter()
            .find(|r| r.keys == "Ctrl+b g")
            .unwrap_or_else(|| panic!("no row for the panel's key: {rows:?}"));
        assert_eq!(row.what, "notes: show and focus it, or hide it when it has the keys");
        assert!(rows
            .iter()
            .find(|r| r.keys == "Ctrl+b %")
            .unwrap()
            .what
            .contains("(e / a / t / g)"));
        // The built-ins are table bindings and keep their one row each.
        assert_eq!(rows.iter().filter(|r| r.keys == "Ctrl+b e").count(), 1);
    }

    #[test]
    fn help_groups_adjacent_rows_with_the_same_meaning_under_the_prefix() {
        let rows = Keymap::defaults().help(&ModuleKeys::built_in());
        let find = |keys: &str| {
            rows.iter()
                .find(|r| r.keys == keys)
                .unwrap_or_else(|| panic!("{keys}: {rows:?}"))
        };
        assert!(find("Ctrl+b Up / Down / Left / Right")
            .what
            .contains("repeats within 500 ms"));
        assert_eq!(
            find("Ctrl+b 1 / 2 / 3 / 4 / 5 / 6 / 7 / 8 / 9").what,
            "Select that session tab"
        );
        assert!(find("Ctrl+b %").what.contains("(e / a / t)"));
        assert_eq!(
            find("Ctrl+b Ctrl+b").what,
            "Send Ctrl+b itself to the pane with the keys"
        );
        assert_eq!(
            find("Ctrl+b { / }").what,
            "Swap this module with the previous / next one on screen"
        );
        let listed: usize = rows.iter().map(|r| r.keys.split(" / ").count()).sum();
        assert_eq!(
            listed,
            Keymap::defaults().bindings().len(),
            "every binding in exactly one row"
        );
    }

    /// v1 picks (2026-09-29): `;` and `o` sit right after the arrows, whose row means something else
    /// ("that way"); each says what it does and neither joins it.
    #[test]
    fn help_gives_the_last_and_next_module_keys_rows_of_their_own() {
        let rows = Keymap::defaults().help(&ModuleKeys::built_in());
        let find = |keys: &str| {
            rows.iter()
                .find(|r| r.keys == keys)
                .unwrap_or_else(|| panic!("{keys}: {rows:?}"))
        };
        assert_eq!(
            find("Ctrl+b ;").what,
            "Move the keys back to the module that had them before (again: return)"
        );
        assert_eq!(
            find("Ctrl+b o").what,
            "Move the keys to the next module on screen, in tree order"
        );
        assert_eq!(
            find("Ctrl+b Up / Down / Left / Right").what,
            "Move the keys to the module that way (repeats within 500 ms)"
        );
    }

    /// The v1 picks freeze table: a user `set` on either key now fails naming the default, until
    /// the user frees the key with `del`, as for every other default.
    #[test]
    fn set_on_semicolon_or_o_names_the_default_it_would_shadow() {
        for (key, action) in [(";", "select.last"), ("o", "select.next")] {
            let err = Keymap::apply_user(&[set(key, "zoom")], &[]).unwrap_err().to_string();
            assert!(err.contains(&format!("already bound to {action} (default)")), "{err}");
            let map = Keymap::apply_user(&[del(key), set(key, "zoom")], &[]).unwrap();
            assert_eq!(map.lookup(&k(key)).unwrap().action, Action::Zoom);
        }
    }

    #[test]
    fn the_strip_verbs_come_from_the_table() {
        assert_eq!(
            Keymap::defaults().strip_verbs(),
            ["x kill", "% right", "\" below", "M-1 M-2 even", "{ } swap"]
        );
        let map = Keymap::apply_user(&[set("X", "module.hide")], &[]).unwrap();
        assert_eq!(map.strip_verbs()[..2], ["x kill".to_string(), "X hide".to_string()]);
        let map = Keymap::apply_user(&[del("%"), set("\\", "split.right")], &[]).unwrap();
        assert_eq!(map.strip_verbs()[1], "\\ right");
    }

    #[test]
    fn keys_for_finds_every_key_bound_to_an_action() {
        assert_eq!(
            Keymap::defaults().keys_for(&Action::Module(ModuleId::agent())),
            [k("a")]
        );
        assert!(Keymap::defaults().keys_for(&Action::Zoom).contains(&k("z")));
    }

    #[test]
    fn panel_ops_fail_on_defaults_reservations_and_unknowns() {
        let set = |key: &str, action: &str| KeymapOp::Set {
            table: "panel".into(),
            key: key.into(),
            action: action.into(),
            opts: vec![],
        };
        let del = |key: &str| KeymapOp::Del {
            table: "panel".into(),
            key: key.into(),
        };
        let err = |ops: &[KeymapOp]| Keymap::apply_user(ops, &[]).unwrap_err().to_string();
        assert!(err(&[set("H", "tab.next")]).contains("tab.prev (default)"));
        assert!(err(&[set("j", "tab.next")]).contains("BROWSE"));
        assert!(err(&[set("gt", "tab.rename")]).contains("tab.rename"));
        assert!(err(&[set("<C-x>", "tab.new")]).contains("<C-x>"));
        assert!(err(&[del("zz")]).contains("nothing binds"));
        // `gt` is a default since v1 polish F16, so rebinding it is refused until it is deleted.
        assert!(err(&[set("gt", "tab.new")]).contains("tab.next (default)"));
        // `zq`, not `zb`: `zb` is reserved for BROWSE since the v1 picks (panel.rs's TAKEN_PAIRS).
        let ok = Keymap::apply_user(&[del("H"), set("H", "tab.next"), set("zq", "tab.new")], &[]).unwrap();
        assert_eq!(ok.panel_user().sets.len(), 2);
        assert_eq!(ok.panel_user().dels.len(), 1);
    }
}
