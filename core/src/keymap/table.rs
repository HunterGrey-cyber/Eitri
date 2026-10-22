//! The prefix table holds stock tmux defaults and applies `init.lua` keymap calls with
//! `apply_user`. Startup fails, naming both sides, for `set` on a bound key, a prefix equal
//! to a root chord, or a Lua command's `keybinding` equal to the prefix or a root chord.
//! Lua panel keys are checked in `crate::layout::keys`, where those keys are built.

use std::collections::BTreeMap;
use std::fmt;

use super::action::{self, Action, ActionError, OptValue, SwapTarget, TabAction, TextChange};
use super::key::{Chord, KeyName, KeyParseError, KeySpec};
use super::panel::PanelUserTable;
use super::root::{self, HelpRow};
use crate::layout::{Axis, Direction, ModuleId, ModuleKeys, ModuleKind};
use crate::plugin::keys::{self as plugin_keys, PluginKeyOp};
use crate::plugin::manifest::id_ok;

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

impl fmt::Display for KeymapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KeymapError::Recorded(message) => f.write_str(message),
            KeymapError::UnknownTable { call, table } => {
                write!(
                    f,
                    "{call}: {table:?} is not a table (there are \"prefix\", \"panel\" and \"plugin:<id>\")"
                )
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
            KeymapError::PluginKey { call, why } => write!(f, "{call}: {why}"),
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
    // tmux's select-layout, next-layout, rotate-window, select-pane -m/-M and display-panes keys; vim's <C-w>H/J/K and i3's $mod+e after the prefix.
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
    bind("s", Action::WorkspaceChoose, false);
    bind("$", Action::WorkspaceRename, false);
    bind("!", Action::TabBreak, false);

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
    // Stock tmux's `command-prompt`, which opens the panel's `:` line.
    bind(":", Action::PanelCommandLine, false);
    bind(
        "C-l",
        Action::SendKeys(KeySpec::parse("C-l").expect("C-l parses")),
        false,
    );
    // Like `C-l` above, send the literal chord to the pane with the keys.
    // Without the prefix, `Ctrl+h/j/k/l` still navigate between modules.
    for name in ["C-h", "C-j", "C-k"] {
        bind(
            name,
            Action::SendKeys(KeySpec::parse(name).expect("a literal chord parses")),
            false,
        );
    }
    bind("F11", Action::WindowImmersive, false);
    // The text size of the module with the keys alone: the prefix before the chord that sizes every pane
    // together (`Ctrl+=`/`Ctrl+-`/`Ctrl+0`, the root table). Larger and smaller repeat like a resize, so
    // one prefix takes a run of steps with Ctrl still held; a reset is one press.
    bind("C-=", Action::Text(TextChange::Larger), true);
    // The keypad's `+` with Ctrl reads as `C-+`; its `-` and `0` are already the two below.
    bind("C-+", Action::Text(TextChange::Larger), true);
    bind("C--", Action::Text(TextChange::Smaller), true);
    bind("C-0", Action::Text(TextChange::Reset), false);
    bind("e", Action::Module(ModuleId::editor()), false);
    bind("a", Action::Module(ModuleId::agent()), false);
    bind("t", Action::Module(ModuleId::terminal()), false);
    // `v` is reserved for the canvas (`crate::layout::keys::RESERVED_FOR_CANVAS`),
    // so it is not bound in this table.
    bind("[", Action::CopyMode { up: false }, false);
    bind("PPage", Action::CopyMode { up: true }, false);
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
                    } else if let Some((_, named)) = root::chords().into_iter().find(|(c, _)| *c == chord) {
                        skip(format!("the prefix would be a root key, {named}; {keeps}"));
                    } else if let Some((id, _)) = claims
                        .command_keybindings
                        .iter()
                        .find(|(_, accel)| Chord::from_accel(accel).is_ok_and(|c| c == chord))
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
        Ok((map, import))
    }

    /// The default bindings with the stock prefix `C-b`.
    pub fn defaults() -> Keymap {
        Keymap {
            prefix: KeySpec::parse(super::stock::STOCK_PREFIX).expect("C-b parses"),
            bindings: default_bindings(),
            panel_user: PanelUserTable::default(),
            plugin_user: BTreeMap::new(),
            removed_by_import: Vec::new(),
        }
    }

    /// The defaults with `ops` applied in order. `lua_panels`: the ids `module.<id>` may name.
    pub fn apply_user(ops: &[KeymapOp], lua_panels: &[String]) -> Result<Keymap, KeymapError> {
        Keymap::defaults().then_user(ops, lua_panels)
    }

    /// This keymap with `init.lua`'s `ops` applied in order. Every rule is strict, as for the
    /// defaults alone, except toward what the tmux import did: a `set` on a key the import bound
    /// replaces it, and a `del` of a key the import unbound is no error -- `init.lua` comes last
    /// and wins, and a snippet written before the import existed still applies on top of it.
    pub fn then_user(self, ops: &[KeymapOp], lua_panels: &[String]) -> Result<Keymap, KeymapError> {
        let mut map = self;
        let mut prefix_call = None;
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
                        match map.bindings.iter().position(|b| b.key == spec) {
                            Some(at) if map.bindings[at].source == Source::Tmux => map.bindings[at] = binding,
                            Some(at) => {
                                let existing = &map.bindings[at];
                                return Err(KeymapError::AlreadyBound {
                                    call: op.call(),
                                    key: spec,
                                    bound_to: existing.action.name(),
                                    source: existing.source,
                                });
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
                    } else {
                        prefix_table(op, table)?;
                        let spec = parse_key(op, key)?;
                        match map.bindings.iter().position(|b| b.key == spec) {
                            Some(at) => {
                                map.bindings.remove(at);
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
        let chord = Chord::of(&map.prefix);
        if let Some((_, named)) = root::chords().into_iter().find(|(c, _)| *c == chord) {
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

    pub fn prefix(&self) -> &KeySpec {
        &self.prefix
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
        rows.into_iter()
            .map(|(keys, what)| HelpRow {
                keys: format!("{prefix} {}", keys.join(" / ")),
                what,
            })
            .collect()
    }

    /// The verbs the top bar's strip lists after the module keys: the keys bound to
    /// kill, hide, split right, split below, even, swap and the keys sheet, in that order, in tmux spelling.
    /// A verb nothing is bound to is left out (`hide`, by default, since `x` kills).
    pub fn strip_verbs(&self) -> Vec<String> {
        type Is = fn(&Action) -> bool;
        let groups: [(&str, Is); 7] = [
            ("kill", |a| matches!(a, Action::ModuleKill)),
            ("hide", |a| matches!(a, Action::ModuleHide)),
            ("right", |a| matches!(a, Action::Split { axis: Axis::Row, .. })),
            ("below", |a| matches!(a, Action::Split { axis: Axis::Column, .. })),
            ("even", |a| matches!(a, Action::Even(_))),
            ("swap", |a| matches!(a, Action::Swap(_))),
            ("keys", |a| matches!(a, Action::PanelKeymap)),
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
    let Ok(chord) = Chord::from_accel(keybinding) else {
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

    /// Expected defaults, row by row, as `(key, action with its options, -r)`.
    /// This literal fixture checks the table's bindings, options and repeat flags.
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
        ("s", "workspace.choose", false),
        ("$", "workspace.rename", false),
        ("!", "tab.break", false),
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
        // tmux's `command-prompt`, here the panel's `:` line, which runs nothing.
        (":", "panel.command-line", false),
        ("C-l", "send-keys keys=C-l", false),
        ("C-h", "send-keys keys=C-h", false),
        ("C-j", "send-keys keys=C-j", false),
        ("C-k", "send-keys keys=C-k", false),
        ("F11", "window.immersive", false),
        ("C-=", "text.larger", true),
        ("C-+", "text.larger", true),
        ("C--", "text.smaller", true),
        ("C-0", "text.reset", false),
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
        let err = Keymap::apply_user(&[set("!", "zoom")], &[]).unwrap_err().to_string();
        assert!(
            err.contains("tab.break (default)") && err.contains("eitri.keymap.del(\"prefix\", \"!\")"),
            "{err}"
        );
    }

    #[test]
    fn the_window_verbs_have_no_default_key() {
        let map = Keymap::defaults();
        for action in [
            Action::WindowNew,
            Action::WindowOther,
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

    /// `prefix :` opens the panel's `:` line. Leaving it unbound would make the armed prefix swallow
    /// it and let subsequent letters run as panel keys. The binding is not repeatable, matching tmux's
    /// `command-prompt`, and the `?` overlay describes it in the prefix section.
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
        // `H J K` are vim's `<C-w>H/J/K`; the Alt chords are i3's `$mod` chords after the prefix.
        // The float and scratchpad chords are i3's `$mod+Shift+space`, `$mod+minus` and
        // `$mod+Shift+minus` after the prefix.
        const EITRI_ONLY: [&str; 27] = [
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

    /// Setting an already-bound default key fails with a message naming both bindings.
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
        let err = Keymap::apply_user(&[set("g", "zoom"), set("g", "module.hide")], &[])
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
        let listed: usize = rows.iter().map(|r| r.keys.split(" / ").count()).sum();
        assert_eq!(
            listed,
            Keymap::defaults().bindings().len(),
            "every binding in exactly one row"
        );
    }

    /// `;` and `o` sit right after the arrows, whose row means something else
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

    /// A user `set` on either key now fails naming the default, until
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
            ["x kill", "% right", "\" below", "M-1 M-2 even", "{ } swap", "? keys"]
        );
        let map = Keymap::apply_user(&[set("X", "module.hide")], &[]).unwrap();
        assert_eq!(map.strip_verbs()[..2], ["x kill".to_string(), "X hide".to_string()]);
        let map = Keymap::apply_user(&[del("%"), set("\\", "split.right")], &[]).unwrap();
        assert_eq!(map.strip_verbs()[1], "\\ right");
    }

    #[test]
    fn the_strip_ends_with_the_keys_sheet_key() {
        assert_eq!(Keymap::defaults().strip_verbs().last().unwrap(), "? keys");
        let rebound = Keymap::apply_user(&[del("?"), set("/", "panel.keymap")], &[]).unwrap();
        assert_eq!(rebound.strip_verbs().last().unwrap(), "/ keys");
        let unbound = Keymap::apply_user(&[del("?")], &[]).unwrap();
        assert_eq!(
            unbound.strip_verbs(),
            ["x kill", "% right", "\" below", "M-1 M-2 even", "{ } swap"]
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
        assert!(err(&[set("H", "tab.next")]).contains("tab.prev (default)"));
        assert!(err(&[set("j", "tab.next")]).contains("BROWSE"));
        assert!(err(&[set("gt", "tab.rename")]).contains("tab.rename"));
        assert!(err(&[set("<C-x>", "tab.new")]).contains("<C-x>"));
        assert!(err(&[del("zz")]).contains("nothing binds"));
        // `gt` is a default, so rebinding it is refused until it is deleted.
        assert!(err(&[set("gt", "tab.new")]).contains("tab.next (default)"));
        // `zq`, not `zb`: `zb` is reserved for BROWSE in panel.rs's TAKEN_PAIRS.
        let ok = Keymap::apply_user(&[del("H"), set("H", "tab.next"), set("zq", "tab.new")], &[]).unwrap();
        assert_eq!(ok.panel_user().sets.len(), 2);
        assert_eq!(ok.panel_user().dels.len(), 1);
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
}
