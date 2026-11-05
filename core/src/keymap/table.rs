//! The prefix table: the defaults (stock tmux), what `init.lua`'s `eitri.keymap` calls
//! make of it (`apply_user`), the user's `"root"` table of keys that need no prefix, and the collision rules
//! that are hard startup failures naming both sides (`set` on a bound key; the prefix equals a root chord; a
//! root key equals the prefix, a built-in root chord other than the text size chords, or a prefix-table key; a Lua
//! command's `keybinding` equals the prefix or a root chord). A Lua panel's key is
//! `crate::layout::keys`'s, since it is checked where panel keys are built.

use std::collections::BTreeMap;
use std::fmt;

use super::action::{self, Action, ActionError, OptValue, SwapTarget, TabAction, TextChange};
use super::key::{Chord, KeyName, KeyParseError, KeySpec};
use super::panel::PanelUserTable;
use super::root::{self, HelpRow};
use crate::layout::{Axis, Direction, ModuleId, ModuleKeys, ModuleKind};
use crate::plugin::keys::{self as plugin_keys, PluginKeyOp};
use crate::plugin::manifest::id_ok;

/// How the sheet says `window.setup` is reached while no key is bound to it.
const SETUP_COMMAND: &str = ":Eitri setup";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Default,
    Tmux,
    User,
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Source::Default => "default",
            Source::Tmux => "from your tmux config",
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

/// The effective prefix table: the prefix chord and what each key after it does, plus
/// `init.lua`'s recorded `"panel"` table ops -- unmerged with nvim's own mappings or the panel
/// defaults here; `eitri_core::keymap::panel::effective` does that merge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Keymap {
    prefix: KeySpec,
    bindings: Vec<Binding>,
    panel_user: PanelUserTable,
    /// `init.lua`'s `plugin:<id>` tables, by plugin id: merged with what each page declares by the
    /// plugin's host, since only the page knows its actions.
    plugin_user: BTreeMap<String, Vec<PluginKeyOp>>,
    /// The keys the tmux import unbound and left unbound: `init.lua`'s `del` of one is not an
    /// error, since the user's own snippet may delete what their tmux config already deleted.
    removed_by_import: Vec<KeySpec>,
    /// `init.lua`'s `"root"` table: keys that run an action with no prefix. Always `Source::User`, never repeatable,
    /// and empty until `init.lua` binds one.
    root: Vec<Binding>,
    /// What the tmux import had set that a root key took over, one sentence each, for the launch log.
    dropped_for_root: Vec<String>,
    /// The defaults a prefix-table `set` of `init.lua` took over, `(key, action it replaced)`.
    overrides: Vec<(String, String)>,
}

/// What [`Keymap::for_startup_settled`] made: the keymap, the tmux import's record, and what `init.lua` asked of
/// the keymap that could not be done.
pub struct StartupKeymap {
    pub keymap: Keymap,
    pub import: Option<TmuxImport>,
    pub problems: Vec<String>,
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
    /// A root key that holds neither Ctrl nor Alt.
    RootNotAChord {
        call: String,
    },
    /// A root key given an action only a prefix key may run (`send-prefix`, `send-keys`).
    RootAction {
        call: String,
        action: String,
    },
    /// A root key given the `repeatable` option.
    RootRepeatable {
        call: String,
    },
    /// A root key that is something else already: the prefix, a built-in root chord, or a prefix-table binding.
    /// `what` finishes the sentence "<key> is ...".
    RootCollides {
        call: String,
        key: KeySpec,
        what: String,
    },
    CommandKeybinding {
        command: String,
        keybinding: String,
        hits: String,
    },
    /// An `init.lua` `"panel"` table op `PanelUserTable::set`/`del` refused: `why` is its
    /// message (already naming the key/action), `call` the op as the user wrote it.
    Panel {
        call: String,
        why: String,
    },
    /// A `plugin:<id>` table op refused: a key that is not a key, one Eitri keeps, an action name no
    /// page could declare, or options (the table takes none). `why` names the key or action.
    PluginKey {
        call: String,
        why: String,
    },
}

impl KeymapError {
    /// The `init.lua` call the error is about, as [`KeymapOp::call`] writes it, when it names one.
    fn call(&self) -> Option<String> {
        match self {
            KeymapError::Recorded(message) => Some(message.clone()),
            KeymapError::UnknownTable { call, .. }
            | KeymapError::BadKey { call, .. }
            | KeymapError::Action { call, .. }
            | KeymapError::NotBound { call, .. }
            | KeymapError::PrefixNotAChord { call }
            | KeymapError::PrefixIsRoot { call, .. }
            | KeymapError::RootNotAChord { call }
            | KeymapError::RootAction { call, .. }
            | KeymapError::RootRepeatable { call }
            | KeymapError::RootCollides { call, .. }
            | KeymapError::Panel { call, .. }
            | KeymapError::PluginKey { call, .. } => Some(call.clone()),
            KeymapError::CommandKeybinding { .. } => None,
        }
    }
}

impl fmt::Display for KeymapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KeymapError::Recorded(message) => f.write_str(message),
            KeymapError::UnknownTable { call, table } => {
                write!(
                    f,
                    "{call}: {table:?} is not a table (there are \"prefix\", \"root\", \"panel\" and \"plugin:<id>\")"
                )
            }
            KeymapError::BadKey { call, error } => write!(f, "{call}: {error}"),
            KeymapError::Action { call, error } => write!(f, "{call}: {error}"),
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
            KeymapError::RootNotAChord { call } => write!(
                f,
                "{call}: a root key must hold Ctrl or Alt (C- or M-), or it would take that key from every module"
            ),
            KeymapError::RootAction { call, action } => write!(
                f,
                "{call}: {action} is a prefix key's action; a root key cannot run it (<prefix> <key> sends a key through)"
            ),
            KeymapError::RootRepeatable { call } => {
                write!(f, "{call}: a root key repeats by being pressed again, so it takes no `repeatable`")
            }
            KeymapError::RootCollides { call, key, what } => write!(f, "{call}: {key} is {what}"),
            KeymapError::CommandKeybinding {
                command,
                keybinding,
                hits,
            } => write!(
                f,
                "eitri.command.register{{ id = {command:?}, keybinding = {keybinding:?} }}: {keybinding} is {hits}"
            ),
            KeymapError::Panel { call, why } => write!(f, "{call}: {why}"),
            KeymapError::PluginKey { call, why } => write!(f, "{call}: {why}"),
        }
    }
}

impl std::error::Error for KeymapError {}

fn parse_key(op: &KeymapOp, text: &str) -> Result<KeySpec, KeymapError> {
    KeySpec::parse(text).map_err(|error| KeymapError::BadKey { call: op.call(), error })
}

/// The `"prefix"` table. `"root"`, `"panel"` (`PanelUserTable::set`/`del`, whose key and action types differ from
/// the prefix table's) and `plugin:<id>` are answered by the `Set`/`Del` arms below before this is asked; anything
/// else is `UnknownTable`.
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

/// The plugin a `plugin:<id>` table names, when `table` is one and the id is one a manifest could have.
fn plugin_table(table: &str) -> Option<&str> {
    table.strip_prefix("plugin:").filter(|id| id_ok(id))
}

fn plugin_key(op: &KeymapOp, why: String) -> KeymapError {
    KeymapError::PluginKey { call: op.call(), why }
}

fn default_bindings() -> Vec<Binding> {
    let mut out = Vec::new();
    let mut bind = |key: &str, action: Action, repeatable: bool| {
        out.push(Binding {
            key: KeySpec::parse(key).expect("a default key parses"),
            action,
            repeatable,
            source: Source::Default,
        });
    };
    let arrows = [
        ("Up", Direction::Up),
        ("Down", Direction::Down),
        ("Left", Direction::Left),
        ("Right", Direction::Right),
    ];
    bind("C-b", Action::SendPrefix, false);
    for (key, dir) in [
        ("M-H", Direction::Left),
        ("M-J", Direction::Down),
        ("M-K", Direction::Up),
        ("M-L", Direction::Right),
    ] {
        bind(key, Action::Move(dir), true);
    }
    bind("M-w", Action::ContainerTabbed, false);
    bind("M-s", Action::ContainerStacked, false);
    bind("M-a", Action::FocusParent, false);
    bind("M-d", Action::FocusChild, false);
    bind(
        "M-Enter",
        Action::InstanceNew {
            kind: ModuleKind::Terminal,
        },
        false,
    );
    bind("M-S-Space", Action::FloatToggle, false);
    bind("M--", Action::ScratchpadShow { module: None }, false);
    bind("M-_", Action::ScratchpadMove, false);
    bind(
        "%",
        Action::Split {
            axis: Axis::Row,
            cwd: crate::keymap::action::SplitCwd::Start,
        },
        false,
    );
    bind(
        "\"",
        Action::Split {
            axis: Axis::Column,
            cwd: crate::keymap::action::SplitCwd::Start,
        },
        false,
    );
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
    // Stock tmux's `last-pane` and `select-pane -t :.+`, neither with `-r`.
    bind(";", Action::SelectLast, false);
    bind("o", Action::SelectNext, false);
    bind("{", Action::Swap(SwapTarget::Prev), false);
    bind("}", Action::Swap(SwapTarget::Next), false);
    bind("M-1", Action::Even(Axis::Row), false);
    bind("M-2", Action::Even(Axis::Column), false);
    // tmux's select-layout, next-layout, rotate-window, select-pane -m/-M and display-panes keys;
    // vim's <C-w>H/J/K and i3's $mod+e after the prefix.
    bind("M-3", Action::MainHorizontal, false);
    bind("M-4", Action::MainVertical, false);
    bind("M-5", Action::Tiled, false);
    bind("Space", Action::CycleLayout { forward: true }, false);
    bind("E", Action::Spread, false);
    bind("C-o", Action::Rotate { down: false }, false);
    bind("M-o", Action::Rotate { down: true }, false);
    bind("M-e", Action::ToggleSplit, false);
    bind("H", Action::MoveFar(Direction::Left), false);
    bind("J", Action::MoveFar(Direction::Down), false);
    bind("K", Action::MoveFar(Direction::Up), false);
    bind("m", Action::MarkToggle, false);
    bind("M", Action::MarkClear, false);
    bind("q", Action::SelectNumber, false);
    bind("(", Action::WorkspaceStep { forward: false }, false);
    bind(")", Action::WorkspaceStep { forward: true }, false);
    bind("L", Action::WorkspaceLast, false);
    bind("O", Action::ProjectChoose, false);
    bind("s", Action::WorkspaceChoose, false);
    bind("$", Action::WorkspaceRename, false);
    bind("!", Action::TabBreak, false);

    bind("x", Action::ModuleKill, false);
    // Stock tmux's `d` detaches the client: it goes, and what ran in it keeps running.
    bind("d", Action::ModuleHideByKey, false);
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
    // Stock tmux's `command-prompt`, which opens the panel's `:` line.
    bind(":", Action::PanelCommandLine, false);
    bind(
        "C-l",
        Action::SendKeys(KeySpec::parse("C-l").expect("C-l parses")),
        false,
    );
    // Bound like `C-l` above -- the literal chord, to whichever pane holds the keys
    // (vim-tmux-navigator's README: `bind C-l send-keys 'C-l'`). `Ctrl+h/j/k/l` themselves (with no
    // prefix) stay Eitri's own navigation, unaffected by these.
    for name in ["C-h", "C-j", "C-k"] {
        bind(
            name,
            Action::SendKeys(KeySpec::parse(name).expect("a literal chord parses")),
            false,
        );
    }
    bind("F11", Action::WindowImmersive, false);
    // The text size of the module with the keys alone: the prefix before the chord that sizes every module
    // (`Ctrl+=`/`Ctrl+-`/`Ctrl+0`, the root table). One prefix takes a run of steps, a reset among them, for as long
    // as Ctrl stays down (`prefix.rs`).
    bind("C-=", Action::Text(TextChange::Larger), true);
    // The keypad's `+` with Ctrl reads as `C-+`; its `-` and `0` are already the two below.
    bind("C-+", Action::Text(TextChange::Larger), true);
    bind("C--", Action::Text(TextChange::Smaller), true);
    bind("C-0", Action::Text(TextChange::Reset), true);
    bind("e", Action::Module(ModuleId::editor()), false);
    bind("a", Action::Module(ModuleId::agent()), false);
    bind("t", Action::Module(ModuleId::terminal()), false);
    // tmux `copy-mode` / `copy-mode -u`: `v` is deliberately not bound here --
    // reserved for the canvas (`crate::layout::keys::RESERVED_FOR_CANVAS`), not this table.
    bind("[", Action::CopyMode { up: false }, false);
    bind("PPage", Action::CopyMode { up: true }, false);
    // The round over the agents that wait, across windows: keys stock tmux leaves free after its prefix.
    bind("u", Action::AgentsNext, false);
    bind("B", Action::AgentsBack, false);
    out
}

/// What `init.lua` registered that the tmux import must leave alone, so that a configuration that
/// starts without the import still starts with it: each Lua panel's `(id, key)` (a panel whose key
/// the prefix table binds is refused at startup) and each Lua command's `(id, keybinding)` (one
/// that is the prefix chord is refused at startup).
#[derive(Debug, Clone, Default)]
pub struct LuaClaims {
    pub panel_keys: Vec<(String, String)>,
    pub command_keybindings: Vec<(String, String)>,
}

/// What the tmux import did: the files tmux would load that were read, the prefix it set (`None`
/// when the config sets none Eitri could take), its bindings as they stood once the import was
/// done, the keys it unbound and left unbound, and every line that did not come across.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TmuxImport {
    pub files: Vec<std::path::PathBuf>,
    pub prefix: Option<KeySpec>,
    pub bindings: Vec<Binding>,
    pub removed: Vec<KeySpec>,
    pub skipped: Vec<super::tmux::Skipped>,
}

impl TmuxImport {
    /// Whether the window's keys differ from the defaults because of it -- not merely whether a
    /// config was read: one that only sets options, or rebinds a default key to what it already
    /// does, changes nothing a notice would need to announce.
    pub fn changes_anything(&self) -> bool {
        let defaults = Keymap::defaults();
        self.prefix.is_some_and(|p| p != defaults.prefix)
            || !self.removed.is_empty()
            || self.bindings.iter().any(|b| {
                defaults
                    .lookup(&b.key)
                    .is_none_or(|d| d.action != b.action || d.repeatable != b.repeatable)
            })
    }
}

/// tmux's own actions, the ones the import maps commands to: what `unbind -a` removes from the
/// defaults. The rest -- HINT, the module keys, the keymap overlay, the panel reload, text size,
/// Immersive, and the tab keys tmux has no command for -- are Eitri's and stay.
fn is_tmux_action(action: &Action) -> bool {
    matches!(
        action,
        Action::WorkspaceStep { .. }
            | Action::WorkspaceLast
            | Action::WorkspaceChoose
            | Action::TabBreak
            | Action::SendPrefix
            | Action::SendKeys(_)
            | Action::Split { .. }
            | Action::Zoom
            | Action::Resize { .. }
            | Action::Select(_)
            | Action::SelectLast
            | Action::SelectNext
            | Action::Swap(_)
            | Action::Even(_)
            | Action::MainHorizontal
            | Action::MainVertical
            | Action::Tiled
            | Action::CycleLayout { .. }
            | Action::Spread
            | Action::Rotate { .. }
            | Action::MarkToggle
            | Action::MarkClear
            | Action::SelectNumber
            | Action::SwapMarked
            | Action::ModuleKill
            | Action::PanelCommandLine
            | Action::CopyMode { .. }
            | Action::Tab(
                TabAction::New
                    | TabAction::Next
                    | TabAction::Prev
                    | TabAction::Last
                    | TabAction::Rename
                    | TabAction::Close
                    | TabAction::Choose
            )
    )
}

impl Keymap {
    /// The defaults with the user's tmux config applied on top, in tmux's order, and what that did.
    /// Never fails: a key or prefix Eitri cannot take, or one `init.lua` already claimed, is
    /// skipped with the reason and the rest still comes across.
    pub fn with_tmux(read: &super::tmux::TmuxRead, claims: &LuaClaims) -> (Keymap, TmuxImport) {
        use super::tmux::{Skipped, Step};
        let mut map = Keymap::defaults();
        let mut import = TmuxImport {
            files: read.files.clone(),
            skipped: read.skipped.clone(),
            ..TmuxImport::default()
        };
        let mut removed: Vec<KeySpec> = Vec::new();
        for (step, origin) in &read.steps {
            let mut skip = |reason: String| {
                import.skipped.push(Skipped {
                    origin: origin.clone(),
                    reason,
                });
            };
            match step {
                Step::Prefix(key) => {
                    let keeps = format!("the prefix stays {}", map.prefix.human());
                    let chord = Chord::of(key);
                    if !(key.ctrl || key.meta || matches!(key.key, KeyName::F(_))) {
                        skip(format!(
                            "a prefix must be a chord (C- or M-) or a function key, or it would eat that key \
                             everywhere; {keeps}"
                        ));
                    } else if let Some((_, named)) = root::pressed_chords().into_iter().find(|(c, _)| *c == chord) {
                        skip(format!("the prefix would be a root key, {named}; {keeps}"));
                    } else if let Some((id, _)) = claims
                        .command_keybindings
                        .iter()
                        .find(|(_, accel)| Chord::from_accel(accel).is_ok_and(|c| c.folded() == chord.folded()))
                    {
                        skip(format!("taken by init.lua: command {id}'s keybinding; {keeps}"));
                    } else {
                        map.prefix = *key;
                        import.prefix = Some(*key);
                    }
                }
                Step::Bind {
                    key,
                    action,
                    repeatable,
                } => {
                    if key.as_char() == Some(crate::layout::keys::RESERVED_FOR_CANVAS) {
                        skip("reserved for the canvas".to_string());
                        continue;
                    }
                    if let Some((id, _)) = claims
                        .panel_keys
                        .iter()
                        .find(|(_, k)| KeySpec::parse(k).is_ok_and(|k| k == *key))
                    {
                        skip(format!("taken by init.lua: the {id} panel's key"));
                        continue;
                    }
                    let binding = Binding {
                        key: *key,
                        action: action.clone(),
                        repeatable: *repeatable,
                        source: Source::Tmux,
                    };
                    match map.bindings.iter().position(|b| b.key == *key) {
                        Some(at) => map.bindings[at] = binding,
                        None => map.bindings.push(binding),
                    }
                    removed.retain(|k| k != key);
                }
                Step::Unbind(key) => {
                    if let Some(at) = map.bindings.iter().position(|b| b.key == *key) {
                        map.bindings.remove(at);
                        if !removed.contains(key) {
                            removed.push(*key);
                        }
                    }
                }
                Step::UnbindAll => {
                    map.bindings.retain(|b| {
                        let goes = b.source == Source::Tmux || is_tmux_action(&b.action);
                        if goes && !removed.contains(&b.key) {
                            removed.push(b.key);
                        }
                        !goes
                    });
                }
            }
        }
        import.bindings = map
            .bindings
            .iter()
            .filter(|b| b.source == Source::Tmux)
            .cloned()
            .collect();
        import.removed = removed.clone();
        map.removed_by_import = removed;
        (map, import)
    }

    /// The window's keymap at startup: the defaults, the tmux import unless `keymap.from_tmux` is
    /// `"off"` (`from_tmux` is that setting as `init.lua` left it), then `init.lua`'s own calls.
    /// `env` is asked for only when the import is on, so nothing about the environment can matter
    /// to a window that turned the import off.
    /// The import never fails; a bad setting or an `init.lua` mistake does, naming it.
    pub fn for_startup(
        ops: &[KeymapOp],
        lua_panels: &[String],
        from_tmux: Option<&str>,
        env: impl FnOnce() -> super::tmux::TmuxEnv,
        claims: &LuaClaims,
    ) -> Result<(Keymap, Option<TmuxImport>), String> {
        let (base, import) = if super::tmux::enabled(from_tmux)? {
            let (map, import) = Keymap::with_tmux(&super::tmux::read(&env()), claims);
            (map, Some(import))
        } else {
            (Keymap::defaults(), None)
        };
        let map = base.then_user(ops, lua_panels).map_err(|e| e.to_string())?;
        // What a root key took over is no longer what the import set, so the import (and the notice made from it)
        // does not claim it.
        let import = import.map(|mut import| {
            if import.prefix.is_some_and(|p| *map.prefix() != p) {
                import.prefix = None;
            }
            import.bindings.retain(|b| map.root_lookup(&b.key).is_none());
            import
        });
        Ok((map, import))
    }

    /// [`Keymap::for_startup`] for a window that opens whatever `init.lua` did: a call that cannot be applied (a bad key,
    /// an unknown action, a key that collides with one Eitri keeps) is left out, and what went wrong with it is
    /// returned beside the keymap, one line each. `from_tmux` is `keymap.from_tmux` as the config validation left
    /// it, so it is never a bad value here.
    pub fn for_startup_settled(
        ops: &[KeymapOp],
        origins: &[Option<String>],
        lua_panels: &[String],
        from_tmux: Option<&str>,
        env: impl FnOnce() -> super::tmux::TmuxEnv,
        claims: &LuaClaims,
    ) -> StartupKeymap {
        let (base, import) = if super::tmux::enabled(from_tmux).unwrap_or(true) {
            let (map, import) = Keymap::with_tmux(&super::tmux::read(&env()), claims);
            (map, Some(import))
        } else {
            (Keymap::defaults(), None)
        };
        let (keymap, problems) = base.then_user_settled(ops, origins, lua_panels);
        let import = import.map(|mut import| {
            if import.prefix.is_some_and(|p| *keymap.prefix() != p) {
                import.prefix = None;
            }
            import.bindings.retain(|b| keymap.root_lookup(&b.key).is_none());
            import
        });
        StartupKeymap {
            keymap,
            import,
            problems,
        }
    }

    /// [`Keymap::then_user`] that leaves out the call an error names and tries again, until what is left applies:
    /// the keymap, and one line for each call left out, led by `origins[i]` (where the call was made) when it is known.
    /// An error that names no call of `ops` leaves all of them out.
    pub fn then_user_settled(
        &self,
        ops: &[KeymapOp],
        origins: &[Option<String>],
        lua_panels: &[String],
    ) -> (Keymap, Vec<String>) {
        // The calls still in play, as their places in `ops`.
        let mut kept: Vec<usize> = (0..ops.len()).collect();
        let mut left_out = Vec::new();
        let applied = |kept: &[usize]| {
            let calls: Vec<KeymapOp> = kept.iter().map(|&at| ops[at].clone()).collect();
            self.clone().then_user(&calls, lua_panels)
        };
        loop {
            match applied(&kept) {
                Ok(map) => return (map, left_out),
                Err(error) => {
                    // What the base alone cannot hold is not for a call to fix; no call left is the end.
                    if kept.is_empty() {
                        left_out.push(format!("{error} -- left out"));
                        return (self.clone(), left_out);
                    }
                    // Two calls can read alike (`set("prefix", "g", ...)` twice, one of them bad): the one whose
                    // removal changes the error is the culprit, the latest of them when none does.
                    let named = error.call();
                    let candidates: Vec<usize> = kept
                        .iter()
                        .copied()
                        .filter(|&at| named.as_deref() == Some(ops[at].call().as_str()))
                        .collect();
                    let culprit = candidates
                        .iter()
                        .rev()
                        .copied()
                        .find(|&candidate| {
                            let rest: Vec<usize> = kept.iter().copied().filter(|&at| at != candidate).collect();
                            applied(&rest)
                                .err()
                                .is_none_or(|next| next.to_string() != error.to_string())
                        })
                        .or(candidates.last().copied());
                    let line = match culprit.and_then(|at| origins.get(at)).and_then(Option::as_ref) {
                        Some(origin) => format!("{origin}: {error} -- left out"),
                        None => format!("{error} -- left out"),
                    };
                    left_out.push(line);
                    match culprit {
                        Some(at) => kept.retain(|&k| k != at),
                        None => kept.clear(),
                    }
                }
            }
        }
    }

    /// Stock tmux's prefix `C-b` and its default table.
    pub fn defaults() -> Keymap {
        Keymap {
            prefix: KeySpec::parse(super::stock::STOCK_PREFIX).expect("C-b parses"),
            bindings: default_bindings(),
            panel_user: PanelUserTable::default(),
            plugin_user: BTreeMap::new(),
            removed_by_import: Vec::new(),
            root: Vec::new(),
            dropped_for_root: Vec::new(),
            overrides: Vec::new(),
        }
    }

    /// The defaults with `ops` applied in order. `lua_panels`: the ids `module.<id>` may name.
    pub fn apply_user(ops: &[KeymapOp], lua_panels: &[String]) -> Result<Keymap, KeymapError> {
        Keymap::defaults().then_user(ops, lua_panels)
    }

    /// This keymap with `init.lua`'s `ops` applied in order. Every rule is strict, as for the
    /// defaults alone, except toward what the tmux import did: a `set` on a key the import bound
    /// replaces it, and a `del` of a key the import unbound is no error -- `init.lua` comes last
    /// and wins. Importing a deletion must not break a snippet that also deletes that key.
    pub fn then_user(self, ops: &[KeymapOp], lua_panels: &[String]) -> Result<Keymap, KeymapError> {
        let mut map = self;
        let mut prefix_call = None;
        // The call that bound each root key, for the collision messages.
        let mut root_calls: Vec<(KeySpec, String)> = Vec::new();
        // A plugin key is checked against the prefix only once every call has run, since a later
        // `prefix` call moves it.
        let mut plugin_sets: Vec<(&KeymapOp, &str)> = Vec::new();
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
                    if let Some(id) = plugin_table(table) {
                        if !opts.is_empty() {
                            return Err(plugin_key(op, "a plugin table's set takes no options".into()));
                        }
                        if !plugin_keys::action_ok(action) {
                            return Err(plugin_key(
                                op,
                                format!("{action:?} is not an action a page could declare ([a-z0-9_.-], at most 40)"),
                            ));
                        }
                        plugin_keys::parse(key).map_err(|why| plugin_key(op, why))?;
                        plugin_sets.push((op, key));
                        map.plugin_user
                            .entry(id.to_string())
                            .or_default()
                            .push(PluginKeyOp::Set {
                                key: key.clone(),
                                action: action.clone(),
                            });
                    } else if table == "panel" {
                        map.panel_user
                            .set(key, action)
                            .map_err(|why| KeymapError::Panel { call: op.call(), why })?;
                    } else if table == "root" {
                        let spec = parse_key(op, key)?;
                        if !(spec.ctrl || spec.meta) {
                            return Err(KeymapError::RootNotAChord { call: op.call() });
                        }
                        let parsed = action::parse(action, opts, lua_panels)
                            .map_err(|error| KeymapError::Action { call: op.call(), error })?;
                        if matches!(parsed.action, Action::SendPrefix | Action::SendKeys(_)) {
                            return Err(KeymapError::RootAction {
                                call: op.call(),
                                action: parsed.action.name(),
                            });
                        }
                        if parsed.repeatable {
                            return Err(KeymapError::RootRepeatable { call: op.call() });
                        }
                        // The table holds only `init.lua`'s own keys, so a key bound already was bound by an
                        // earlier call: the last one wins.
                        map.root.retain(|b| b.key != spec);
                        root_calls.retain(|(k, _)| *k != spec);
                        map.root.push(Binding {
                            key: spec,
                            action: parsed.action,
                            repeatable: false,
                            source: Source::User,
                        });
                        root_calls.push((spec, op.call()));
                    } else {
                        prefix_table(op, table)?;
                        let spec = parse_key(op, key)?;
                        let parsed = action::parse(action, opts, lua_panels)
                            .map_err(|error| KeymapError::Action { call: op.call(), error })?;
                        let binding = Binding {
                            key: spec,
                            action: parsed.action,
                            repeatable: parsed.repeatable,
                            source: Source::User,
                        };
                        // What the import or an earlier call bound is replaced without a word; a default is
                        // replaced too, and recorded, since the person may not know the key was taken.
                        match map.bindings.iter().position(|b| b.key == spec) {
                            Some(at) => {
                                let replaced = std::mem::replace(&mut map.bindings[at], binding);
                                if replaced.source == Source::Default {
                                    map.overrides.push((spec.to_string(), replaced.action.name()));
                                }
                            }
                            None => map.bindings.push(binding),
                        }
                    }
                }
                KeymapOp::Del { table, key } => {
                    if let Some(id) = plugin_table(table) {
                        plugin_keys::parse(key).map_err(|why| plugin_key(op, why))?;
                        map.plugin_user
                            .entry(id.to_string())
                            .or_default()
                            .push(PluginKeyOp::Del { key: key.clone() });
                    } else if table == "panel" {
                        map.panel_user
                            .del(key)
                            .map_err(|why| KeymapError::Panel { call: op.call(), why })?;
                    } else if table == "root" {
                        let spec = parse_key(op, key)?;
                        match map.root.iter().position(|b| b.key == spec) {
                            Some(at) => {
                                map.root.remove(at);
                                root_calls.retain(|(k, _)| *k != spec);
                            }
                            None => {
                                return Err(KeymapError::NotBound {
                                    call: op.call(),
                                    key: spec,
                                })
                            }
                        }
                    } else {
                        prefix_table(op, table)?;
                        let spec = parse_key(op, key)?;
                        match map.bindings.iter().position(|b| b.key == spec) {
                            Some(at) => {
                                map.bindings.remove(at);
                                map.overrides.retain(|(taken, _)| *taken != spec.to_string());
                            }
                            None if map.removed_by_import.contains(&spec) => {}
                            None => {
                                return Err(KeymapError::NotBound {
                                    call: op.call(),
                                    key: spec,
                                })
                            }
                        }
                    }
                }
            }
        }
        map.settle_root(prefix_call.is_some(), &root_calls)?;
        let chord = Chord::of(&map.prefix);
        if let Some((_, named)) = root::pressed_chords().into_iter().find(|(c, _)| *c == chord) {
            return Err(KeymapError::PrefixIsRoot {
                call: prefix_call.unwrap_or_else(|| format!("the prefix {}", map.prefix)),
                root: named,
            });
        }
        for (op, key) in plugin_sets {
            plugin_keys::check_key(key, &chord).map_err(|why| plugin_key(op, why))?;
        }
        Ok(map)
    }

    /// Checks every root key against what else the key could be, once every op has run (a later op can
    /// create the collision), in the order prefix, built-in root chord, prefix table, so the first message is the
    /// most basic one. What the tmux import set -- a prefix, a prefix-table binding -- gives way to a root key and
    /// is recorded in `dropped_for_root`; what `init.lua` set is an error naming both sides.
    fn settle_root(&mut self, prefix_set_by_init_lua: bool, calls: &[(KeySpec, String)]) -> Result<(), KeymapError> {
        if self.root.is_empty() {
            return Ok(());
        }
        let stock = KeySpec::parse(super::stock::STOCK_PREFIX).expect("the stock prefix parses");
        if !prefix_set_by_init_lua
            && self.prefix != stock
            && self.root.iter().any(|b| Chord::of(&b.key) == Chord::of(&self.prefix))
        {
            self.dropped_for_root.push(format!(
                "the prefix {} from your tmux config is also a root key set in init.lua, so the prefix stays {}",
                self.prefix.human(),
                stock.human()
            ));
            self.prefix = stock;
        }
        let prefix_chord = Chord::of(&self.prefix);
        let builtin = root::pressed_chords();
        let mut dropped = Vec::new();
        for binding in &self.root {
            let key = binding.key;
            let call = calls
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, call)| call.clone())
                .unwrap_or_else(|| format!("the root key {key}"));
            let collides = |what: String| KeymapError::RootCollides {
                call: call.clone(),
                key,
                what,
            };
            let chord = Chord::of(&key);
            if chord == prefix_chord {
                return Err(collides(
                    "the prefix; choose another root key, or another prefix with eitri.keymap.prefix".to_string(),
                ));
            }
            if root::rebindable(&chord) {
                continue;
            }
            if let Some((_, named)) = builtin.iter().find(|(c, _)| *c == chord) {
                return Err(collides(format!("{named}, which Eitri keeps")));
            }
            if let Some(at) = self.bindings.iter().position(|b| b.key == key) {
                let existing = &self.bindings[at];
                if existing.source == Source::Tmux {
                    dropped.push(at);
                    self.dropped_for_root.push(format!(
                        "{} after the prefix ({}) from your tmux config is also a root key set in init.lua, so it was left out of the prefix table",
                        key.human(),
                        existing.action.name()
                    ));
                } else {
                    return Err(collides(format!(
                        "also {} after the prefix ({}); eitri.keymap.del(\"prefix\", \"{key}\") first, so that {} {key} can send {key} itself",
                        existing.action.name(),
                        existing.source,
                        self.prefix
                    )));
                }
            }
        }
        dropped.sort_unstable_by(|a, b| b.cmp(a));
        for at in dropped {
            self.bindings.remove(at);
        }
        Ok(())
    }

    pub fn prefix(&self) -> &KeySpec {
        &self.prefix
    }

    /// `init.lua`'s root keys: bound with no prefix. Empty unless `init.lua` set some.
    pub fn root_bindings(&self) -> &[Binding] {
        &self.root
    }

    pub fn root_lookup(&self, key: &KeySpec) -> Option<&Binding> {
        self.root.iter().find(|b| b.key == *key)
    }

    /// What the tmux import set that a root key took over, one sentence each.
    pub fn dropped_for_root(&self) -> &[String] {
        &self.dropped_for_root
    }

    /// The defaults `init.lua`'s `set` calls took over, one `table key (was action)` each, in the order of the
    /// calls: the prefix table's, then the panel's.
    pub fn user_overrides(&self) -> Vec<String> {
        let prefix = self
            .overrides
            .iter()
            .map(|(key, was)| format!("prefix {key} (was {was})"));
        let panel = self
            .panel_user
            .overrides
            .iter()
            .map(|(key, was)| format!("panel {key} (was {was})"));
        prefix.chain(panel).collect()
    }

    pub fn lookup(&self, key: &KeySpec) -> Option<&Binding> {
        self.bindings.iter().find(|b| b.key == *key)
    }

    pub fn bindings(&self) -> &[Binding] {
        &self.bindings
    }

    /// `init.lua`'s recorded `"panel"` table ops; `panel::effective` merges them with nvim's
    /// own mappings and the panel's defaults.
    pub fn panel_user(&self) -> &PanelUserTable {
        &self.panel_user
    }

    /// `init.lua`'s recorded `plugin:<id>` table ops, in order; empty for a plugin it never named.
    pub fn plugin_user(&self, id: &str) -> &[PluginKeyOp] {
        self.plugin_user.get(id).map(Vec::as_slice).unwrap_or(&[])
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
        self.help_where(keys, |_| true)
    }

    /// [`Keymap::help`] limited to the actions `keep` accepts: a window that runs only some of the
    /// table's actions (a companion window has no layout) lists only those. A Lua panel's key row is
    /// `Action::Module`'s, so `keep` decides it too.
    pub fn help_where(&self, keys: &ModuleKeys, keep: impl Fn(&Action) -> bool) -> Vec<HelpRow> {
        let prefix = self.prefix.human();
        let listed: Vec<String> = keys.entries().iter().map(|(k, _)| k.to_string()).collect();
        let module_keys = listed.join(" / ");
        let mut rows: Vec<(Vec<String>, String)> = Vec::new();
        for binding in self.bindings.iter().filter(|b| keep(&b.action)) {
            let mut what = binding.action.describe(&prefix, &module_keys);
            if binding.repeatable {
                what.push_str(" (repeats within 500 ms)");
            }
            if binding.source == Source::Tmux {
                what.push_str(" (tmux)");
            }
            let key = binding.key.human();
            match rows.last_mut() {
                Some((keys, last)) if *last == what => keys.push(key),
                _ => rows.push((vec![key], what)),
            }
        }
        for (key, id) in keys.entries() {
            if id.kind() == ModuleKind::LuaWebview && keep(&Action::Module(id.clone())) {
                rows.push((
                    vec![key.to_string()],
                    Action::Module(id.clone()).describe(&prefix, &module_keys),
                ));
            }
        }
        let mut sheet: Vec<HelpRow> = rows
            .into_iter()
            .map(|(keys, what)| HelpRow {
                keys: format!("{prefix} {}", keys.join(" / ")),
                what,
            })
            .collect();
        // No default key, and the one way back to a screen that was dismissed: the row says how it is reached until
        // a key is bound to it.
        let setup = Action::WindowSetup;
        if keep(&setup) && !self.bindings.iter().any(|b| b.action == setup) {
            sheet.push(HelpRow {
                keys: SETUP_COMMAND.to_string(),
                what: setup.describe(&prefix, &module_keys),
            });
        }
        sheet
    }

    /// The verbs the top bar's strip lists after the module keys: the keys bound to
    /// kill, hide (`module.hide` and the hide key), split right, split below, even, swap and the keys sheet, in that
    /// order, in tmux spelling. A verb nothing is bound to is left out.
    pub fn strip_verbs(&self) -> Vec<String> {
        self.strip_verbs_but(&[])
    }

    /// [`Keymap::strip_verbs`] for a window of compositor mode: the compositor arranges the windows, so the verbs that
    /// lay modules out are not listed, nor `hide` where the window has no module of its own to hide.
    pub fn compositor_strip_verbs(&self, hides: bool) -> Vec<String> {
        let off = ["right", "below", "even", "swap"];
        match hides {
            true => self.strip_verbs_but(&off),
            false => self.strip_verbs_but(&[&off[..], &["hide"]].concat()),
        }
    }

    fn strip_verbs_but(&self, off: &[&str]) -> Vec<String> {
        type Is = fn(&Action) -> bool;
        let groups: [(&str, Is); 7] = [
            ("kill", |a| matches!(a, Action::ModuleKill)),
            ("hide", |a| matches!(a, Action::ModuleHide | Action::ModuleHideByKey)),
            ("right", |a| matches!(a, Action::Split { axis: Axis::Row, .. })),
            ("below", |a| matches!(a, Action::Split { axis: Axis::Column, .. })),
            ("even", |a| matches!(a, Action::Even(_))),
            ("swap", |a| matches!(a, Action::Swap(_))),
            ("keys", |a| matches!(a, Action::PanelKeymap)),
        ];
        groups
            .iter()
            .filter(|(label, _)| !off.contains(label))
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
    let Ok(chord) = Chord::from_accel(keybinding).map(|chord| chord.folded()) else {
        return Ok(());
    };
    let hits = if chord == Chord::of(keymap.prefix()).folded() {
        Some(format!("the prefix, {} (eitri.keymap)", keymap.prefix().human()))
    } else if let Some(binding) = keymap
        .root_bindings()
        .iter()
        .find(|b| Chord::of(&b.key).folded() == chord)
    {
        Some(format!(
            "a root key, {} ({}, eitri.keymap)",
            binding.key.human(),
            binding.action.name()
        ))
    } else {
        root::pressed_chords()
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

    /// Pin default keys row by row as `(key, action with its options, -r)`. An independent
    /// expected table catches changed actions, options or repeatability in the defaults.
    const DEFAULTS_FIXTURE: &[(&str, &str, bool)] = &[
        ("C-b", "send-prefix", false),
        ("M-H", "move.left", true),
        ("M-J", "move.down", true),
        ("M-K", "move.up", true),
        ("M-L", "move.right", true),
        ("M-w", "container.tabbed", false),
        ("M-s", "container.stacked", false),
        ("M-a", "focus.parent", false),
        ("M-d", "focus.child", false),
        ("M-Enter", "instance.new kind=terminal", false),
        ("M-S-Space", "float.toggle", false),
        ("M--", "scratchpad.show", false),
        ("M-_", "scratchpad.move", false),
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
        ("M-3", "layout.main-horizontal", false),
        ("M-4", "layout.main-vertical", false),
        ("M-5", "layout.tiled", false),
        ("Space", "layout.next", false),
        ("E", "layout.spread", false),
        ("C-o", "rotate.up", false),
        ("M-o", "rotate.down", false),
        ("M-e", "container.toggle-split", false),
        ("H", "move.far-left", false),
        ("J", "move.far-down", false),
        ("K", "move.far-up", false),
        ("m", "mark.toggle", false),
        ("M", "mark.clear", false),
        ("q", "select.number", false),
        ("(", "workspace.prev", false),
        (")", "workspace.next", false),
        ("L", "workspace.last", false),
        ("O", "project.choose", false),
        ("s", "workspace.choose", false),
        ("$", "workspace.rename", false),
        ("!", "tab.break", false),
        ("x", "module.kill", false),
        ("d", "module.hide-by-key", false),
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
        // tmux's `command-prompt` opens the panel's `:` line without running anything.
        (":", "panel.command-line", false),
        ("C-l", "send-keys keys=C-l", false),
        ("C-h", "send-keys keys=C-h", false),
        ("C-j", "send-keys keys=C-j", false),
        ("C-k", "send-keys keys=C-k", false),
        ("F11", "window.immersive", false),
        ("C-=", "text.larger", true),
        ("C-+", "text.larger", true),
        ("C--", "text.smaller", true),
        ("C-0", "text.reset", true),
        ("e", "module.editor", false),
        ("a", "module.agent", false),
        ("t", "module.terminal", false),
        ("[", "copy-mode", false),
        ("PPage", "copy-mode up=true", false),
        ("u", "agents.next", false),
        ("B", "agents.back", false),
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
    fn bang_breaks_a_tab_out_as_stock_tmux_breaks_a_pane() {
        let map = Keymap::defaults();
        let binding = map.lookup(&k("!")).expect("! is bound");
        assert_eq!(binding.action, Action::TabBreak);
        assert!(!binding.repeatable, "stock tmux binds it without -r");
        assert_eq!(crate::keymap::stock::stock_command(&k("!")), Some("break-pane"));
        assert!(
            is_tmux_action(&Action::TabBreak),
            "unbind -a removes it as it removes the other tmux defaults"
        );
        assert_eq!(map.keys_for(&Action::TabBreak).len(), 1, "one default key");
        let taken = Keymap::apply_user(&[set("!", "zoom")], &[]).unwrap();
        assert_eq!(taken.lookup(&k("!")).unwrap().action, Action::Zoom);
        assert_eq!(taken.user_overrides(), ["prefix ! (was tab.break)"]);
    }

    #[test]
    #[allow(non_snake_case)]
    fn u_and_B_are_bound_in_eitris_table_and_unbound_in_the_stock_tmux_fixture() {
        let map = Keymap::defaults();
        for (key, action) in [("u", Action::AgentsNext), ("B", Action::AgentsBack)] {
            let binding = map.lookup(&k(key)).unwrap_or_else(|| panic!("prefix {key} is unbound"));
            assert_eq!(binding.action, action);
            assert!(!binding.repeatable);
            assert_eq!(map.keys_for(&action), vec![k(key)], "one default key");
            assert_eq!(
                crate::keymap::stock::stock_command(&k(key)),
                None,
                "stock tmux binds nothing at prefix {key}"
            );
            assert!(
                !is_tmux_action(&action),
                "Eitri's own: `unbind -a` in a tmux config leaves it"
            );
        }
        let sheet = map.help(&ModuleKeys::built_in());
        for action in [Action::AgentsNext, Action::AgentsBack] {
            let what = action.describe("Ctrl+b", "");
            assert!(
                sheet.iter().any(|row| row.what == what),
                "the ? sheet lists {}: {sheet:?}",
                action.name()
            );
        }
    }

    #[test]
    fn the_sheet_says_how_window_setup_is_reached_until_a_key_is_bound() {
        let keys = ModuleKeys::built_in();
        let map = Keymap::defaults();
        let rows = map.help(&keys);
        let row = rows
            .iter()
            .find(|r| r.keys == ":Eitri setup")
            .expect("the sheet lists window.setup with no key");
        assert_eq!(row.what, Action::WindowSetup.describe("Ctrl+b", ""));
        assert!(
            map.help_where(&keys, |action| *action != Action::WindowSetup)
                .iter()
                .all(|r| r.keys != ":Eitri setup"),
            "a window that does not run it does not list it"
        );
        let bound = Keymap::apply_user(&[set("F5", "window.setup")], &[]).unwrap();
        let rows = bound.help(&keys);
        assert!(rows.iter().all(|r| r.keys != ":Eitri setup"), "{rows:?}");
        assert!(rows.iter().any(|r| r.keys == "Ctrl+b F5"), "{rows:?}");
    }

    #[test]
    fn the_window_verbs_have_no_default_key() {
        let map = Keymap::defaults();
        for action in [
            Action::WindowNew,
            Action::WindowOther,
            Action::WindowSetup,
            Action::MoveToWindow {
                module: None,
                follow: false,
            },
            Action::WorkspaceToWindow { follow: false },
            Action::WorkspaceSwapWindow,
        ] {
            assert!(map.keys_for(&action).is_empty(), "{action:?} has no default key");
        }
    }

    #[test]
    fn the_float_and_workspace_defaults() {
        let map = Keymap::defaults();
        let bound = |key: &str| {
            map.lookup(&KeySpec::parse(key).unwrap())
                .map(|b| (b.action.clone(), b.repeatable))
        };
        assert_eq!(bound("M-S-Space"), Some((Action::FloatToggle, false)));
        assert_eq!(bound("M--"), Some((Action::ScratchpadShow { module: None }, false)));
        assert_eq!(bound("M-_"), Some((Action::ScratchpadMove, false)));
        assert_eq!(bound("("), Some((Action::WorkspaceStep { forward: false }, false)));
        assert_eq!(bound(")"), Some((Action::WorkspaceStep { forward: true }, false)));
        assert_eq!(bound("L"), Some((Action::WorkspaceLast, false)));
        assert_eq!(bound("s"), Some((Action::WorkspaceChoose, false)));
        assert_eq!(bound("$"), Some((Action::WorkspaceRename, false)));
        for (key, stock) in [
            ("(", "switch-client"),
            (")", "switch-client"),
            ("L", "switch-client"),
            ("s", "choose-tree"),
            ("$", "command-prompt"),
        ] {
            assert_eq!(
                crate::keymap::stock::stock_command(&KeySpec::parse(key).unwrap()),
                Some(stock),
                "{key} is stock tmux's {stock}"
            );
        }
        for unbound in [
            Action::FocusModeToggle,
            Action::WorkspaceNew,
            Action::WorkspaceSwitch { name: "main".into() },
        ] {
            assert!(map.keys_for(&unbound).is_empty(), "{unbound:?} has no default key");
        }
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

    /// `prefix :` must open the panel command line, matching tmux command-prompt. An unbound
    /// colon would be swallowed by the prefix table and leave subsequent letters running as panel
    /// keys. The binding is non-repeatable like stock tmux, and the prefix help lists its action.
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

    /// Every default key matches stock tmux and its `-r`, except explicitly listed Eitri-only actions.
    #[test]
    fn every_default_is_a_stock_tmux_key_or_marked_eitri_only() {
        // `[` and `PPage` are stock tmux's own keys too (both non-repeatable, like our `copy-mode`
        // bindings), so they need no entry here even though Eitri's action differs from tmux's.
        // `H J K` are vim's `<C-w>H/J/K`; the Alt chords are i3's `$mod` chords after the prefix.
        // The float and scratchpad chords are i3's `$mod+Shift+space`, `$mod+minus` and
        // `$mod+Shift+minus` after the prefix.
        const EITRI_ONLY: [&str; 30] = [
            "O",
            "C-l",
            "C-h",
            "C-j",
            "C-k",
            "F11",
            "e",
            "a",
            "C-=",
            "C-+",
            "C--",
            "C-0",
            "M-e",
            "H",
            "J",
            "K",
            "M-H",
            "M-J",
            "M-K",
            "M-L",
            "M-w",
            "M-s",
            "M-a",
            "M-d",
            "M-Enter",
            "M-S-Space",
            "M--",
            "M-_",
            "u",
            "B",
        ];
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

    /// A user `set` on a default key takes it, and the takeover is recorded for the startup notice.
    #[test]
    fn set_on_a_default_key_takes_it_and_is_recorded() {
        let map = Keymap::apply_user(&[set("l", "resize.right")], &[]).unwrap();
        assert_eq!(
            map.lookup(&k("l")).unwrap().action,
            Action::Resize {
                dir: Direction::Right,
                cells: 1
            }
        );
        assert_eq!(map.lookup(&k("l")).unwrap().source, Source::User);
        assert_eq!(map.user_overrides(), ["prefix l (was tab.last)"]);
    }

    #[test]
    fn a_second_set_on_the_same_key_wins_and_says_nothing_of_the_first() {
        let map = Keymap::apply_user(&[set("g", "zoom"), set("g", "module.hide")], &[]).unwrap();
        assert_eq!(map.lookup(&k("g")).unwrap().action.name(), "module.hide");
        assert!(map.user_overrides().is_empty(), "g is no default");
        let map = Keymap::apply_user(&[set("l", "zoom"), set("l", "module.hide")], &[]).unwrap();
        assert_eq!(map.lookup(&k("l")).unwrap().action.name(), "module.hide");
        assert_eq!(
            map.user_overrides(),
            ["prefix l (was tab.last)"],
            "one record, the default's"
        );
    }

    #[test]
    fn a_del_after_a_takeover_takes_the_record_with_it() {
        let map = Keymap::apply_user(&[set("l", "zoom"), del("l")], &[]).unwrap();
        assert!(map.lookup(&k("l")).is_none());
        assert!(map.user_overrides().is_empty());
        let map = Keymap::apply_user(&[del("l"), set("l", "zoom")], &[]).unwrap();
        assert!(
            map.user_overrides().is_empty(),
            "freed by the user first: no takeover to say"
        );
    }

    #[test]
    fn settling_leaves_out_the_call_that_failed_and_not_the_one_that_reads_like_it() {
        let origins = [Some("/c/init.lua:1".to_string()), Some("/c/init.lua:2".to_string())];
        for (ops, left) in [
            ([set("g", "zoom"), set("g", "no.such.action")], "/c/init.lua:2: "),
            ([set("g", "no.such.action"), set("g", "zoom")], "/c/init.lua:1: "),
        ] {
            let (map, left_out) = Keymap::defaults().then_user_settled(&ops, &origins, &[]);
            assert_eq!(map.lookup(&k("g")).unwrap().action, Action::Zoom);
            assert_eq!(left_out.len(), 1, "{left_out:?}");
            assert!(
                left_out[0].starts_with(left) && left_out[0].ends_with("-- left out"),
                "{left_out:?}"
            );
        }
    }

    #[test]
    fn settling_a_keymap_with_nothing_wrong_leaves_nothing_out_and_matches_the_strict_one() {
        let ops = [set("g", "zoom"), del("l"), set("l", "resize.right")];
        let (map, left_out) = Keymap::defaults().then_user_settled(&ops, &[], &[]);
        assert!(left_out.is_empty());
        assert_eq!(map, Keymap::apply_user(&ops, &[]).unwrap());
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
        let err = Keymap::apply_user(&[del("g")], &[]).unwrap_err().to_string();
        assert!(
            err.contains("eitri.keymap.del(\"prefix\", \"g\")") && err.contains("not bound"),
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
            check_command_keybinding("x", "<primary>b", &map).is_err(),
            "a lowercase modifier name is the same chord in GTK, so it collides too"
        );
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
        let keyed = rows.iter().filter(|r| r.keys != SETUP_COMMAND);
        let listed: usize = keyed.map(|r| r.keys.split(" / ").count()).sum();
        assert_eq!(
            listed,
            Keymap::defaults().bindings().len(),
            "every binding in exactly one row"
        );
    }

    /// `;` and `o` sit right after the arrows, whose row means something else ("that way"); each
    /// says what it does and neither joins it.
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

    /// Those two keys are frozen defaults; a user `set` on either takes the key, as for every other default.
    #[test]
    fn set_on_semicolon_or_o_takes_the_default_it_would_shadow() {
        for (key, action) in [(";", "select.last"), ("o", "select.next")] {
            let map = Keymap::apply_user(&[set(key, "zoom")], &[]).unwrap();
            assert_eq!(map.lookup(&k(key)).unwrap().action, Action::Zoom);
            assert_eq!(map.user_overrides(), [format!("prefix {key} (was {action})")]);
            let map = Keymap::apply_user(&[del(key), set(key, "zoom")], &[]).unwrap();
            assert_eq!(map.lookup(&k(key)).unwrap().action, Action::Zoom);
        }
    }

    #[test]
    fn compositor_mode_strips_the_verbs_that_lay_modules_out() {
        let map = Keymap::defaults();
        assert_eq!(map.compositor_strip_verbs(true), ["x kill", "d hide", "? keys"]);
        assert_eq!(map.compositor_strip_verbs(false), ["x kill", "? keys"]);
    }

    #[test]
    fn the_strip_verbs_come_from_the_table() {
        assert_eq!(
            Keymap::defaults().strip_verbs(),
            [
                "x kill",
                "d hide",
                "% right",
                "\" below",
                "M-1 M-2 even",
                "{ } swap",
                "? keys"
            ]
        );
        let map = Keymap::apply_user(&[set("X", "module.hide")], &[]).unwrap();
        assert_eq!(
            map.strip_verbs()[..2],
            ["x kill".to_string(), "d X hide".to_string()],
            "both hides share the group, the default key first"
        );
        let map = Keymap::apply_user(&[del("%"), set("\\", "split.right")], &[]).unwrap();
        assert_eq!(map.strip_verbs()[2], "\\ right");
    }

    #[test]
    fn the_strip_ends_with_the_keys_sheet_key() {
        assert_eq!(Keymap::defaults().strip_verbs().last().unwrap(), "? keys");
        let rebound = Keymap::apply_user(&[del("?"), set("/", "panel.keymap")], &[]).unwrap();
        assert_eq!(rebound.strip_verbs().last().unwrap(), "/ keys");
        let unbound = Keymap::apply_user(&[del("?")], &[]).unwrap();
        assert_eq!(
            unbound.strip_verbs(),
            ["x kill", "d hide", "% right", "\" below", "M-1 M-2 even", "{ } swap"]
        );
    }

    #[test]
    fn keys_for_finds_every_key_bound_to_an_action() {
        assert_eq!(
            Keymap::defaults().keys_for(&Action::Module(ModuleId::agent())),
            [k("a")]
        );
        assert!(Keymap::defaults().keys_for(&Action::Zoom).contains(&k("z")));
    }

    fn plugin_set(table: &str, key: &str, action: &str) -> KeymapOp {
        KeymapOp::Set {
            table: table.into(),
            key: key.into(),
            action: action.into(),
            opts: vec![],
        }
    }

    #[test]
    fn a_plugin_table_records_its_ops_in_order_per_plugin() {
        let ops = [
            plugin_set("plugin:snotra", "<C-n>", "item.next"),
            KeymapOp::Del {
                table: "plugin:snotra".into(),
                key: "]q".into(),
            },
            plugin_set("plugin:other-1", "x", "a.b"),
        ];
        let map = Keymap::apply_user(&ops, &[]).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(
            map.plugin_user("snotra"),
            [
                PluginKeyOp::Set {
                    key: "<C-n>".into(),
                    action: "item.next".into()
                },
                PluginKeyOp::Del { key: "]q".into() }
            ]
        );
        assert_eq!(map.plugin_user("other-1").len(), 1);
        assert!(map.plugin_user("nobody").is_empty());
        // The plugin tables touch neither the prefix table nor the panel's.
        assert_eq!(map.bindings(), Keymap::defaults().bindings());
    }

    #[test]
    fn a_plugin_table_refuses_a_key_eitri_keeps() {
        for key in ["<C-h>", "<F11>", "<C-=>", "<C-b>", "<C-S-a>", "<D-j>"] {
            let err = Keymap::apply_user(&[plugin_set("plugin:snotra", key, "x")], &[]).unwrap_err();
            let KeymapError::PluginKey { call, why } = &err else {
                panic!("{key}: {err:?}")
            };
            assert!(call.contains("plugin:snotra"), "{call}");
            assert!(why.contains(&format!("{key:?}")), "{key}: {why}");
        }
        // The prefix checked is the one init.lua leaves, whichever order it was set in.
        let moved = [
            plugin_set("plugin:snotra", "<C-a>", "x"),
            KeymapOp::Prefix { key: "C-a".into() },
        ];
        assert!(matches!(
            Keymap::apply_user(&moved, &[]),
            Err(KeymapError::PluginKey { .. })
        ));
        let freed = [
            KeymapOp::Prefix { key: "C-a".into() },
            plugin_set("plugin:snotra", "<C-b>", "x"),
        ];
        assert!(Keymap::apply_user(&freed, &[]).is_ok());
    }

    #[test]
    fn a_plugin_table_refuses_bad_keys_actions_and_options() {
        let err = |op: KeymapOp| match Keymap::apply_user(&[op], &[]) {
            Err(KeymapError::PluginKey { why, .. }) => why,
            other => panic!("{other:?}"),
        };
        assert!(err(plugin_set("plugin:snotra", "<Foo>", "x")).contains("\"<Foo>\""));
        assert!(err(plugin_set("plugin:snotra", "j", "Item.Next")).contains("\"Item.Next\""));
        assert!(err(KeymapOp::Set {
            table: "plugin:snotra".into(),
            key: "j".into(),
            action: "x".into(),
            opts: vec![("cells".into(), OptValue::Int(1))],
        })
        .contains("option"));
        assert!(err(KeymapOp::Del {
            table: "plugin:snotra".into(),
            key: "<Foo>".into(),
        })
        .contains("\"<Foo>\""));
        // A key the page's kit cannot bind would leave the action with a key that never fires.
        for key in ["<M-n>", "<A-x>", "<F2>", "<PageDown>", "<Insert>"] {
            assert!(
                err(plugin_set("plugin:snotra", key, "item.next")).contains(&format!("{key:?}")),
                "{key}"
            );
            assert!(
                err(KeymapOp::Del {
                    table: "plugin:snotra".into(),
                    key: key.into(),
                })
                .contains(&format!("{key:?}")),
                "{key}"
            );
        }
    }

    #[test]
    fn a_plugin_table_must_name_a_plugin_id() {
        for table in ["plugin:Bad_Id", "plugin:", "plugin:-x", "plugin", "plugins:snotra"] {
            let err = Keymap::apply_user(&[plugin_set(table, "j", "x")], &[]).unwrap_err();
            assert!(matches!(err, KeymapError::UnknownTable { .. }), "{table}: {err:?}");
            assert!(err.to_string().contains("plugin:<id>"), "{err}");
        }
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
        assert!(err(&[set("j", "tab.next")]).contains("BROWSE"));
        assert!(err(&[set("gt", "tab.rename")]).contains("tab.rename"));
        assert!(err(&[set("<C-x>", "tab.new")]).contains("<C-x>"));
        assert!(err(&[del("zz")]).contains("nothing binds"));
        // `gt` and `H` are default bindings: a `set` takes them, and the takeover is recorded.
        let taken = Keymap::apply_user(&[set("gt", "tab.new"), set("H", "tab.next")], &[]).unwrap();
        assert_eq!(taken.panel_user().sets.len(), 2);
        assert_eq!(
            taken.user_overrides(),
            ["panel gt (was tab.next)", "panel H (was tab.prev)"]
        );
        // `zq`, not `zb`: `zb` is reserved for BROWSE (panel.rs's TAKEN_PAIRS).
        let ok = Keymap::apply_user(&[del("H"), set("H", "tab.next"), set("zq", "tab.new")], &[]).unwrap();
        assert_eq!(ok.panel_user().sets.len(), 2);
        assert_eq!(ok.panel_user().dels.len(), 1);
        assert!(ok.user_overrides().is_empty());
    }

    #[test]
    fn the_tiling_defaults_are_stock_tmux_keys_or_vims_and_i3s() {
        let map = Keymap::defaults();
        for (key, action, stock) in [
            ("M-3", "layout.main-horizontal", "select-layout"),
            ("M-4", "layout.main-vertical", "select-layout"),
            ("M-5", "layout.tiled", "select-layout"),
            ("Space", "layout.next", "next-layout"),
            ("E", "layout.spread", "select-layout"),
            ("C-o", "rotate.up", "rotate-window"),
            ("M-o", "rotate.down", "rotate-window"),
            ("m", "mark.toggle", "select-pane"),
            ("M", "mark.clear", "select-pane"),
            ("q", "select.number", "display-panes"),
        ] {
            let binding = map.lookup(&k(key)).unwrap_or_else(|| panic!("{key} is unbound"));
            assert_eq!(binding.action.name(), action, "{key}");
            assert!(!binding.repeatable, "{key}: stock tmux binds it without -r");
            assert_eq!(crate::keymap::stock::stock_command(&k(key)), Some(stock), "{key}");
        }
        for (key, action) in [
            ("H", "move.far-left"),
            ("J", "move.far-down"),
            ("K", "move.far-up"),
            ("M-e", "container.toggle-split"),
        ] {
            assert_eq!(map.lookup(&k(key)).unwrap().action.name(), action, "{key}");
            assert_eq!(
                crate::keymap::stock::stock_command(&k(key)),
                None,
                "{key}: never a stock tmux key"
            );
        }
        for name in [
            "swap.marked",
            "layout.flip-horizontal",
            "layout.flip-vertical",
            "layout.previous",
            "move.far-right",
            "layout.undo",
        ] {
            assert!(
                map.bindings().iter().all(|b| b.action.name() != name),
                "{name} has no default key"
            );
        }
        assert_eq!(
            map.lookup(&k("L")).unwrap().action,
            Action::WorkspaceLast,
            "L keeps tmux's switch-client -l"
        );
    }

    #[test]
    fn a_lua_panel_keyed_h_j_or_k_is_refused_naming_the_default() {
        for (key, action) in [("H", "move.far-left"), ("J", "move.far-down"), ("K", "move.far-up")] {
            let err =
                ModuleKeys::build(&[(ModuleId::lua("notes"), Some(key.to_string()))], &Keymap::defaults()).unwrap_err();
            assert!(
                matches!(&err, crate::layout::KeyError::Bound { action: a, .. } if a == action),
                "{key}: {err:?}"
            );
        }
    }

    #[test]
    fn the_container_and_instance_defaults() {
        let map = Keymap::defaults();
        for (key, action, repeatable) in [
            ("M-H", Action::Move(Direction::Left), true),
            ("M-J", Action::Move(Direction::Down), true),
            ("M-K", Action::Move(Direction::Up), true),
            ("M-L", Action::Move(Direction::Right), true),
            ("M-w", Action::ContainerTabbed, false),
            ("M-s", Action::ContainerStacked, false),
            ("M-a", Action::FocusParent, false),
            ("M-d", Action::FocusChild, false),
            (
                "M-Enter",
                Action::InstanceNew {
                    kind: ModuleKind::Terminal,
                },
                false,
            ),
        ] {
            let binding = map.lookup(&k(key)).unwrap();
            assert_eq!(binding.action, action, "{key}");
            assert_eq!(binding.repeatable, repeatable, "{key}");
        }
    }

    fn root_set(key: &str, action: &str) -> KeymapOp {
        KeymapOp::Set {
            table: "root".into(),
            key: key.into(),
            action: action.into(),
            opts: vec![],
        }
    }
    fn root_del(key: &str) -> KeymapOp {
        KeymapOp::Del {
            table: "root".into(),
            key: key.into(),
        }
    }
    fn root_err(ops: &[KeymapOp]) -> KeymapError {
        Keymap::apply_user(ops, &[]).unwrap_err()
    }

    #[test]
    fn the_defaults_have_no_root_keys() {
        let map = Keymap::defaults();
        assert!(map.root_bindings().is_empty());
        assert!(map.dropped_for_root().is_empty());
        assert!(Keymap::apply_user(&[set("g", "zoom")], &[])
            .unwrap()
            .root_bindings()
            .is_empty());
    }

    #[test]
    fn a_root_key_is_set_looked_up_and_deleted() {
        let map = Keymap::apply_user(&[root_set("M-y", "tab.next")], &[]).unwrap();
        let binding = map.root_lookup(&k("M-y")).expect("bound");
        assert_eq!(binding.action, Action::Tab(TabAction::Next));
        assert_eq!(binding.source, Source::User);
        assert!(!binding.repeatable);
        assert_eq!(map.root_bindings().len(), 1);
        assert!(map.lookup(&k("M-y")).is_none(), "the prefix table is untouched");
        let map = Keymap::apply_user(&[root_set("M-y", "tab.next"), root_del("M-y")], &[]).unwrap();
        assert!(map.root_bindings().is_empty());
        let err = root_err(&[root_del("M-y")]);
        assert!(matches!(err, KeymapError::NotBound { .. }), "{err:?}");
    }

    #[test]
    fn a_root_key_must_hold_ctrl_or_alt() {
        for key in ["a", "F5", "S-Up", "Space", "Up", "%"] {
            let err = root_err(&[root_set(key, "tab.next")]);
            assert!(matches!(err, KeymapError::RootNotAChord { .. }), "{key}: {err:?}");
            assert!(err.to_string().contains("must hold Ctrl or Alt"), "{err}");
        }
        for key in ["C-y", "M-h", "C-M-t", "M-S-Up", "C-F5"] {
            assert!(Keymap::apply_user(&[root_set(key, "tab.next")], &[]).is_ok(), "{key}");
        }
    }

    #[test]
    fn a_root_key_takes_no_prefix_only_action_and_no_repeat() {
        for action in ["send-prefix", "send-keys keys=C-l"] {
            let (name, opts) = match action.split_once(' ') {
                Some((name, _)) => (name, vec![("keys".to_string(), OptValue::Str("C-l".into()))]),
                None => (action, vec![]),
            };
            let op = KeymapOp::Set {
                table: "root".into(),
                key: "M-y".into(),
                action: name.into(),
                opts,
            };
            let err = root_err(&[op]);
            assert!(matches!(err, KeymapError::RootAction { .. }), "{action}: {err:?}");
            assert!(err.to_string().contains("is a prefix key's action"), "{err}");
        }
        let op = KeymapOp::Set {
            table: "root".into(),
            key: "M-y".into(),
            action: "tab.next".into(),
            opts: vec![("repeatable".into(), OptValue::Bool(true))],
        };
        let err = root_err(&[op]);
        assert!(matches!(err, KeymapError::RootRepeatable { .. }), "{err:?}");
        assert!(err.to_string().contains("repeats by being pressed again"), "{err}");
        let split = Keymap::apply_user(&[root_set("M-y", "split.right")], &[]).unwrap();
        assert!(matches!(
            split.root_lookup(&k("M-y")).unwrap().action,
            Action::Split { .. }
        ));
    }

    #[test]
    fn a_root_key_that_is_the_prefix_or_a_built_in_root_chord_is_refused_naming_both() {
        let err = root_err(&[root_set("C-b", "tab.next")]);
        assert!(matches!(err, KeymapError::RootCollides { .. }), "{err:?}");
        assert!(err.to_string().contains("the prefix"), "{err}");
        let err = root_err(&[prefix("M-y"), root_set("M-y", "tab.next")]);
        assert!(err.to_string().contains("the prefix"), "{err}");
        for (key, named) in [("C-h", "Ctrl+h"), ("C-l", "Ctrl+l")] {
            let err = root_err(&[root_set(key, "tab.next")]);
            assert!(matches!(err, KeymapError::RootCollides { .. }), "{key}: {err:?}");
            assert!(err.to_string().contains(named), "{key}: {err}");
            assert!(err.to_string().contains("Eitri keeps"), "{err}");
        }
    }

    /// The text size chords are the built-in chords `init.lua` may take. They stay bound after the prefix by default,
    /// and that is no collision: the prefix before the chord is how the two scopes are told apart.
    #[test]
    fn the_text_size_chords_can_be_root_keys_beside_their_prefix_bindings() {
        for key in ["C-=", "C-+", "C--", "C-0"] {
            let map = Keymap::apply_user(&[root_set(key, "text.larger")], &[]).unwrap();
            assert_eq!(
                map.root_lookup(&k(key)).unwrap().action,
                Action::Text(TextChange::Larger)
            );
            assert_eq!(
                map.lookup(&k(key)).unwrap().action,
                match key {
                    "C--" => Action::Text(TextChange::Smaller),
                    "C-0" => Action::Text(TextChange::Reset),
                    _ => Action::Text(TextChange::Larger),
                },
                "{key} after the prefix stays"
            );
        }
        let swap = Keymap::apply_user(
            &[
                root_set("C-=", "text.larger"),
                del("C-="),
                KeymapOp::Set {
                    table: "prefix".into(),
                    key: "C-=".into(),
                    action: "text.all.larger".into(),
                    opts: vec![("repeatable".into(), OptValue::Bool(true))],
                },
            ],
            &[],
        )
        .unwrap();
        let bound = swap.lookup(&k("C-=")).unwrap();
        assert_eq!(bound.action, Action::TextAll(TextChange::Larger));
        assert!(bound.repeatable);
    }

    #[test]
    fn a_text_size_chord_is_still_no_prefix() {
        for key in ["C-=", "C--", "C-0", "C-+"] {
            let err = Keymap::apply_user(&[prefix(key)], &[]).unwrap_err();
            assert!(matches!(err, KeymapError::PrefixIsRoot { .. }), "{key}: {err:?}");
        }
    }

    #[test]
    fn a_root_key_in_the_prefix_table_is_refused_with_the_del_hint_until_that_is_deleted() {
        for (key, shown) in [
            ("M-w", "container.tabbed after the prefix (default)"),
            ("C-o", "rotate.up after the prefix (default)"),
        ] {
            let err = root_err(&[root_set(key, "tab.next")]);
            assert!(matches!(err, KeymapError::RootCollides { .. }), "{key}: {err:?}");
            let text = err.to_string();
            assert!(text.contains(shown), "{text}");
            assert!(
                text.contains(&format!("eitri.keymap.del(\"prefix\", \"{key}\")")),
                "{text}"
            );
        }
        let map = Keymap::apply_user(&[root_set("M-w", "tab.next"), del("M-w")], &[]).unwrap();
        assert!(map.lookup(&k("M-w")).is_none());
        assert!(map.root_lookup(&k("M-w")).is_some());
        // A key the user bound in the prefix table after the root key is the same collision.
        let err = root_err(&[root_set("M-z", "tab.next"), set("M-z", "zoom")]);
        assert!(matches!(err, KeymapError::RootCollides { .. }), "{err:?}");
        assert!(err.to_string().contains("set earlier in init.lua"), "{err}");
    }

    #[test]
    fn a_root_key_set_twice_is_the_last_calls() {
        let map = Keymap::apply_user(&[root_set("M-y", "tab.next"), root_set("M-y", "tab.prev")], &[]).unwrap();
        assert_eq!(map.root_bindings().len(), 1);
        assert_eq!(map.root_lookup(&k("M-y")).unwrap().action.name(), "tab.prev");
    }

    #[test]
    fn the_unknown_table_message_names_the_root_table() {
        let err = root_err(&[KeymapOp::Set {
            table: "global".into(),
            key: "M-y".into(),
            action: "tab.next".into(),
            opts: vec![],
        }]);
        assert!(err.to_string().contains("\"root\""), "{err}");
    }

    #[test]
    fn a_lua_command_keybinding_on_a_root_key_is_refused() {
        let map = Keymap::apply_user(&[root_set("M-h", "tab.next")], &[]).unwrap();
        let err = check_command_keybinding("cmd", "<Alt>h", &map).unwrap_err();
        assert!(matches!(err, KeymapError::CommandKeybinding { .. }), "{err:?}");
        assert!(err.to_string().contains("root key"), "{err}");
        assert!(check_command_keybinding("cmd", "<Alt>j", &map).is_ok());
    }

    /// A key press and an accelerator spell one chord two ways: `Alt+Shift+y` is the character `Y` with no Shift, and
    /// the keypad's plus is the character `+`; a collision check that compares the spellings as written misses both.
    #[test]
    fn a_root_key_or_prefix_collides_with_a_chord_however_the_press_spells_it() {
        let err = root_err(&[prefix("C-+")]);
        assert!(matches!(err, KeymapError::PrefixIsRoot { .. }), "{err:?}");
        assert!(err.to_string().contains("Keypad+"), "{err}");

        let map = Keymap::apply_user(&[root_set("M-Y", "tab.next")], &[]).unwrap();
        let err = check_command_keybinding("cmd", "<Alt><Shift>y", &map).unwrap_err();
        assert!(matches!(err, KeymapError::CommandKeybinding { .. }), "{err:?}");
        assert!(check_command_keybinding("cmd", "<Alt>y", &map).is_ok());
        let map = Keymap::apply_user(&[root_set("M-y", "tab.next")], &[]).unwrap();
        assert!(check_command_keybinding("cmd", "<Alt><Shift>y", &map).is_ok());
    }

    #[test]
    fn the_prefix_table_collision_message_names_the_prefix_chord() {
        let err = root_err(&[root_set("M-w", "tab.next")]);
        assert!(err.to_string().contains("so that C-b M-w can send M-w itself"), "{err}");
    }

    #[test]
    fn root_help_rows_follow_the_built_in_ones_and_honour_the_hosts_keep() {
        let map = Keymap::apply_user(&[root_set("M-y", "tab.next"), root_set("C-M-t", "zoom")], &[]).unwrap();
        let rows = root::user_help_rows(&map, |_| true);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].keys, "Alt+y");
        assert!(rows[0].what.ends_with(" (init.lua)"), "{:?}", rows[0]);
        assert_eq!(rows[1].keys, "Ctrl+Alt+t");
        let rows = root::user_help_rows(&map, |action| !matches!(action, Action::Zoom));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].keys, "Alt+y");
    }

    #[test]
    fn o_opens_a_project_and_survives_tmux_unbind_all() {
        let map = Keymap::defaults();
        let binding = map.lookup(&k("O")).unwrap();
        assert_eq!(binding.action, Action::ProjectChoose);
        assert!(!binding.repeatable);
        assert!(!is_tmux_action(&binding.action));
        let home = crate::test_scratch_dir::ScratchDir::new("eitri-project-key", "tmux");
        std::fs::write(home.join(".tmux.conf"), "unbind -a\n").unwrap();
        let read = crate::keymap::tmux::read(&crate::keymap::tmux::tests::env_for(&home));
        let (map, _) = Keymap::with_tmux(&read, &crate::keymap::LuaClaims::default());
        assert_eq!(map.lookup(&k("O")).unwrap().action, Action::ProjectChoose);
        let taken = Keymap::apply_user(&[set("O", "zoom")], &[]).unwrap();
        assert_eq!(taken.lookup(&k("O")).unwrap().action, Action::Zoom);
        assert_eq!(taken.user_overrides(), ["prefix O (was project.choose)"]);
    }

    #[test]
    fn o_is_no_stock_tmux_3_8_key() {
        let probed = "Tab Space ! \" # $ % & ' ( ) * , - . / 0 1 2 3 4 5 6 7 8 9 : ; < = > ? @ \
                      C D E L M T [ ] c d f g i l m n o p q r s t w x z { } ~ DC PPage BTab \
                      Up Down Left Right M-1 M-2 M-3 M-4 M-5 M-6 M-7 M-n M-o M-p \
                      M-Up M-Down M-Left M-Right C-b C-o C-z C-Up C-Down C-Left C-Right \
                      S-Up S-Down S-Left S-Right";
        assert!(!probed.split_whitespace().any(|key| key == "O"));
    }

    #[test]
    fn o_is_no_root_chord() {
        let map = Keymap::defaults();
        assert!(map.root_lookup(&k("O")).is_none());
        assert!(root::pressed_chords()
            .iter()
            .all(|(chord, _)| *chord != Chord::of(&k("O"))));
    }
}
