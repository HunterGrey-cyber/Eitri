//! `eitri.layout.*`: the Lua half of the layout.
//!
//! - `eitri.layout.default{ ... }`, in `init.lua`: the first window's tree, used whenever no saved
//!   layout can be (a first launch, or a state file that cannot be used). A malformed table is a
//!   startup failure naming the call: the kernel records refusals at the function boundary, including
//!   errors Lua catches, and a later default call replaces the earlier result.
//! - `eitri.layout.rule(target, opts)`, in `init.lua`: where automatically opened modules go.
//! - `eitri.layout.open(kind, opts?)`: queues a new terminal or chat view once the window exists.
//! - `eitri.layout.show(id)`, `.hide(id)`, `.focus(id)`, `.split(id, 'right'|'below')`,
//!   `.run(name, opts?)`, `.mark(id)` and `.float(id, rect?)`, for use
//!   inside `eitri.command.register` commands and event handlers: each queues a
//!   [`LayoutRequest`] that `shell` carries out when the handler returns. Before the window exists
//!   they refuse, naming `default` as the way to shape the first window.
//!
//! - `eitri.layout.get()` reads a fresh table of the live layout once the window exists, with `window`
//!   (`"1"`, `"2"`, or nil for a window whose pair another window holds) and `away`, the modules the
//!   other window holds.
//!
//! `run` takes the names and options `eitri.keymap.set` takes, for layout actions only, so a key
//! and a Lua call reach every verb the same way.
//!
//! **The grammar**, for example: `{ 'row', {'editor', 0.6}, {'column', {'agent'},
//! {'canvas', 0.5}} }`. A container starts with `'row'`/`'column'` (two or more children) or
//! `'tabbed'`/`'stacked'` (one or more children). Each table makes one flat container. Tabbed and
//! stacked containers accept `active = n`, the 1-based drawn child, defaulting to 1. A module id
//! starts a leaf with an optional second item: its share of the parent. A container takes its share
//! as `share = x`. Children without shares divide what remains equally; every share is at least
//! 0.05 and at most 1, and together they sum to 1. Unknown fields are refused so typos fail at startup.
//! Only the top table accepts `floating`, a sequence of module entries with `cx`, `cy`, `w`, `h`
//! and optional `scratch`. Centres range from 0 to 1, dimensions from 1 to 16384 whole pixels.
//! The editor stays in the tree; the agent may be tiled or floating. Every module appears once.
//! Modules this window does not have are left out during reconciliation, with a log line.
//!
//! **A bottom row keeps its height**, in either spelling: the terminal, or a
//! Lua `bottom` panel, that is a full-width row at the bottom of the window -- `{ 'column', {'row', ...},
//! {'terminal'} }` or the n-ary `{ 'column', {'editor'}, {'agent'}, {'terminal'} }` -- is pinned as a
//! first launch pins it, and the log says so. One in the middle of a column, or beside something in a
//! row, divides by its share.
//!
//! `show` puts its module on screen: during a zoom that would keep it off, the zoom ends first, as
//! `Ctrl+a e`/`a` end it. `focus` onto the zoomed module itself leaves the zoom on, as tmux's
//! `select-pane` onto the zoomed pane does.

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::rc::Rc;

use mlua::{Lua, Table, Value};

use crate::keymap::Action;
use crate::layout::rules::{OpenRule, OpenRules, RuleSize, RuleTarget};
pub use crate::layout::spec::LuaTree;
use crate::layout::spec::{self, SpecNode, SplitKind};
use crate::layout::{Axis, Con, Float, FloatRect, Layout, ModuleId, ModuleKind, Node, Workspaces, MAX_FLOAT_PX};

/// One queued `eitri.layout` call, for `shell` to carry out.
#[derive(Debug, Clone, PartialEq)]
pub enum LayoutRequest {
    /// `eitri.layout.open(kind, opts?)`: a new instance of `kind`; with `opts`, placed as they say,
    /// else as the rules say.
    Open(ModuleKind, Option<OpenRule>),
    Switch(String),
    Show(ModuleId),
    Hide(ModuleId),
    Focus(ModuleId),
    /// `Axis::Row` is `'right'`, `Axis::Column` is `'below'`, of the module with the keys.
    Split(ModuleId, Axis),
    /// A key action by its `eitri.keymap` name, run on the module with the keys;
    /// `shell` runs it exactly as the prefix does.
    Run(Action),
    Mark(ModuleId),
    Float(ModuleId, Option<FloatRect>),
}

/// How `get` and `export` read the window once it exists: a query its controller provides, answering a copy of
/// what it reads as it is when asked, or `None` while the controller is writing it, which the call refuses. A copy,
/// so building Lua's table holds no borrow of the window's layout and never meets a controller in the middle of a
/// decision.
pub struct Query<T>(Rc<dyn Fn() -> Option<T>>);

impl<T> Query<T> {
    pub fn new(read: impl Fn() -> Option<T> + 'static) -> Query<T> {
        Query(Rc::new(read))
    }

    fn read(&self) -> Option<T> {
        (self.0)()
    }
}

impl<T> std::fmt::Debug for Query<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Query")
    }
}

/// What `eitri.layout.*` recorded.
#[derive(Debug, Default)]
pub struct LayoutStore {
    default: Option<Result<LuaTree, String>>,
    defined: Vec<(String, Result<LuaTree, String>)>,
    rules: OpenRules,
    init_refusals: Option<Rc<RefCell<super::kernel::InitRefusals>>>,
    workspaces: Option<Query<Workspaces>>,
    requests: Vec<LayoutRequest>,
    accepting: bool,
    lua_panels: Vec<String>,
    layout: Option<Query<Layout>>,
    window: Option<&'static str>,
}

/// How `eitri.window` and `get().window` name a window: `"1"` for the one that holds the pair (a
/// window alone included), `"2"` for the second, and nothing for a window on a project whose pair
/// another window holds.
pub fn window_name(role: crate::window_link::pair::Role) -> Option<&'static str> {
    match role {
        crate::window_link::pair::Role::Main => Some("1"),
        crate::window_link::pair::Role::Second => Some("2"),
        crate::window_link::pair::Role::Unpaired => None,
    }
}

impl LayoutStore {
    /// `init.lua`'s open rules, or the first refusal among its `eitri.layout.rule` calls, which stops startup.
    pub fn rules(&self) -> Result<&OpenRules, String> {
        match self
            .init_refusals
            .as_ref()
            .and_then(|shared| shared.borrow().message("eitri.layout.rule"))
        {
            Some(error) => Err(error),
            None => Ok(&self.rules),
        }
    }

    /// Definitions in init.lua order, including errors that must stop startup.
    pub fn defined_workspaces(&self) -> &[(String, Result<LuaTree, String>)] {
        &self.defined
    }

    /// How the window's workspaces are read, so `get()` names them.
    pub fn attach_workspaces(&mut self, workspaces: Query<Workspaces>) {
        self.workspaces = Some(workspaces);
    }

    /// How the window's live layout is read, so `get` reads its current state.
    pub fn attach(&mut self, layout: Query<Layout>) {
        self.layout = Some(layout);
    }

    /// Which window of a pair this is, for `get().window` ([`window_name`]); `Kernel::set_window`
    /// sets it with `eitri.window`.
    pub fn set_window(&mut self, window: Option<&'static str>) {
        self.window = window;
    }

    /// The registered panel ids that `module.<id>` may name.
    pub fn set_lua_panels(&mut self, ids: Vec<String>) {
        self.lua_panels = ids;
    }

    /// `eitri.layout.default`'s tree, or why it is not one. `None` if `init.lua` never called it.
    pub fn default_tree(&self) -> Option<&Result<LuaTree, String>> {
        self.default.as_ref()
    }

    /// The window exists: runtime calls are accepted, and `default` refuses.
    pub fn accept_requests(&mut self) {
        self.accepting = true;
    }

    /// Everything queued since the last call, in the order it was asked for.
    pub fn take_requests(&mut self) -> Vec<LayoutRequest> {
        std::mem::take(&mut self.requests)
    }
}

/// `pub`: `shell`'s `Kernel::new` calls this.
pub fn install(lua: &Lua, eitri: &Table, store: Rc<RefCell<LayoutStore>>) -> mlua::Result<()> {
    store.borrow_mut().init_refusals = Some(super::kernel::init_refusals(lua));
    let table = lua.create_table()?;

    let default_store = store.clone();
    table.set(
        "default",
        super::kernel::registering_function(lua, "eitri.layout.default", move |_, tree: Value| {
            if default_store.borrow().accepting {
                return Err(mlua::Error::RuntimeError(
                    "eitri.layout.default: only in init.lua, before the window exists".to_string(),
                ));
            }
            let parsed = parse_tree(&tree).map_err(|e| format!("eitri.layout.default: {e}"));
            let mut store = default_store.borrow_mut();
            if store.default.is_some() {
                eprintln!("[lua] eitri.layout.default called again -- the later call replaces the earlier");
            }
            store.default = Some(parsed.clone());
            parsed.map(|_| ()).map_err(mlua::Error::RuntimeError)
        })?,
    )?;

    let workspace_store = store.clone();
    table.set(
        "workspace",
        super::kernel::registering_function(lua, "eitri.layout.workspace", move |_, (name, tree): (Value, Value)| {
            if workspace_store.borrow().accepting {
                return Err(mlua::Error::RuntimeError(
                    "eitri.layout.workspace: only in init.lua, before the window exists".to_string(),
                ));
            }
            let name = match &name {
                Value::String(name) => name
                    .to_str()
                    .map(|name| name.to_string())
                    .map_err(|_| "eitri.layout.workspace: the name must be UTF-8".to_string()),
                other => Err(format!(
                    "eitri.layout.workspace: the name must be a string, not a {}",
                    other.type_name()
                )),
            };
            // A refused name is still recorded, so a definition called under pcall stops startup
            // instead of vanishing. Its empty name matches no valid workspace.
            let name = match name {
                Ok(name) => name,
                Err(message) => {
                    workspace_store
                        .borrow_mut()
                        .defined
                        .push((String::new(), Err(message.clone())));
                    return Err(mlua::Error::RuntimeError(message));
                }
            };
            let call = format!(
                "eitri.layout.workspace('{}')",
                crate::layout::workspace::printable(&name)
            );
            let parsed = if name == "main" {
                Err(format!(
                    "{call}: eitri.layout.default shapes 'main'; give this workspace another name"
                ))
            } else {
                Workspaces::valid_name(&name)
                    .map_err(|err| format!("{call}: {err}"))
                    .and_then(|()| parse_tree(&tree).map_err(|err| format!("{call}: {err}")))
            };
            let mut store = workspace_store.borrow_mut();
            if let Some((_, earlier)) = store.defined.iter_mut().find(|(existing, _)| existing == &name) {
                eprintln!("[lua] {call} called again -- the later call replaces the earlier");
                *earlier = parsed.clone();
            } else {
                store.defined.push((name, parsed.clone()));
            }
            parsed.map(|_| ()).map_err(mlua::Error::RuntimeError)
        })?,
    )?;

    let rule_store = store.clone();
    table.set(
        "rule",
        super::kernel::registering_function(lua, "eitri.layout.rule", move |_, (target, opts): (Value, Value)| {
            if rule_store.borrow().accepting {
                return Err(mlua::Error::RuntimeError(
                    "eitri.layout.rule: only in init.lua, before the window exists".to_string(),
                ));
            }
            let Value::String(target) = target else {
                let message = format!(
                    "eitri.layout.rule: the target must be a string, not a {}",
                    target.type_name()
                );
                return Err(mlua::Error::RuntimeError(message));
            };
            let target = match target.to_str() {
                Ok(target) => target.to_string(),
                Err(_) => {
                    let message = "eitri.layout.rule: the target must be UTF-8".to_string();
                    return Err(mlua::Error::RuntimeError(message));
                }
            };
            let call = format!("eitri.layout.rule('{}')", crate::layout::workspace::printable(&target));
            let parsed = RuleTarget::parse(&target)
                .map_err(|e| format!("{call}: {e}"))
                .and_then(|t| {
                    let editor = t == RuleTarget::Module(ModuleId::editor());
                    parse_rule(&call, &opts, editor).map(|rule| (t, rule))
                });
            let mut store = rule_store.borrow_mut();
            let added = parsed.and_then(|(t, rule)| store.rules.add(t, rule).map_err(|e| format!("{call}: {e}")));
            if let Err(message) = added {
                return Err(mlua::Error::RuntimeError(message));
            }
            Ok(())
        })?,
    )?;

    let open_store = store.clone();
    table.set(
        "open",
        super::kernel::registering_function(lua, "eitri.layout.open", move |_, (kind, opts): (Value, Value)| {
            let refuse = |what: String| {
                mlua::Error::RuntimeError(format!(
                    "eitri.layout.open: kind must be \"terminal\" or \"agent\", not {what}"
                ))
            };
            let kind = match &kind {
                Value::String(s) => s.to_str()?.to_string(),
                other => return Err(refuse(format!("a {}", other.type_name()))),
            };
            let module_kind = match kind.as_str() {
                "terminal" => ModuleKind::Terminal,
                "agent" => ModuleKind::Agent,
                other => return Err(refuse(format!("{other:?}"))),
            };
            let rule = match &opts {
                Value::Nil => None,
                opts => Some(
                    parse_rule(&format!("eitri.layout.open('{kind}')"), opts, false)
                        .map_err(mlua::Error::RuntimeError)?,
                ),
            };
            queue(&open_store, "open", LayoutRequest::Open(module_kind, rule))
        })?,
    )?;

    let switch_store = store.clone();
    table.set(
        "switch",
        super::kernel::registering_function(lua, "eitri.layout.switch", move |_, name: String| {
            Workspaces::valid_name(&name)
                .map_err(|err| mlua::Error::RuntimeError(format!("eitri.layout.switch: {err}")))?;
            queue(&switch_store, "switch", LayoutRequest::Switch(name))
        })?,
    )?;

    for name in ["show", "hide", "focus"] {
        let store = store.clone();
        table.set(
            name,
            super::kernel::registering_function(
                lua,
                match name {
                    "show" => "eitri.layout.show",
                    "hide" => "eitri.layout.hide",
                    _ => "eitri.layout.focus",
                },
                move |_, id: String| {
                    let id = module_arg(name, &id)?;
                    let request = match name {
                        "show" => LayoutRequest::Show(id),
                        "hide" => LayoutRequest::Hide(id),
                        _ => LayoutRequest::Focus(id),
                    };
                    queue(&store, name, request)
                },
            )?,
        )?;
    }

    let split_store = store.clone();
    table.set(
        "split",
        super::kernel::registering_function(
            lua,
            "eitri.layout.split",
            move |_, (id, direction): (String, String)| {
                let id = module_arg("split", &id)?;
                let axis = match direction.as_str() {
                    "right" => Axis::Row,
                    "below" => Axis::Column,
                    other => {
                        return Err(mlua::Error::RuntimeError(format!(
                            "eitri.layout.split: {other:?} is not 'right' or 'below'"
                        )))
                    }
                };
                queue(&split_store, "split", LayoutRequest::Split(id, axis))
            },
        )?,
    )?;

    let run_store = store.clone();
    table.set(
        "run",
        super::kernel::registering_function(lua, "eitri.layout.run", move |lua, (name, options): (Value, Value)| {
            let Value::String(name) = name else {
                return Err(mlua::Error::RuntimeError(format!(
                    "eitri.layout.run: the action name must be a string, not a {}",
                    name.type_name()
                )));
            };
            let name = name.to_str().map_err(|e| {
                mlua::Error::RuntimeError(format!("eitri.layout.run: {e}"))
            })?.to_string();
            let opts = crate::lua::keymap::opts(lua, &options)
                .map_err(|why| mlua::Error::RuntimeError(format!("eitri.layout.run({name:?}): {why}")))?;
            if opts.iter().any(|(key, _)| key == "repeatable") {
                return Err(mlua::Error::RuntimeError(format!(
                    "eitri.layout.run({name:?}): repeatable is a key binding's option"
                )));
            }
            let action = crate::keymap::action::parse(&name, &opts, &run_store.borrow().lua_panels)
                .map_err(|error| mlua::Error::RuntimeError(format!("eitri.layout.run: {error}")))?.action;
            if matches!(action, Action::Split { .. }) {
                return Err(mlua::Error::RuntimeError(format!(
                    "eitri.layout.run: {name} waits for a module key; eitri.layout.split(id, 'right'|'below') places one"
                )));
            }
            if matches!(action, Action::SelectNumber) {
                return Err(mlua::Error::RuntimeError(
                    "eitri.layout.run: select.number waits for a digit; eitri.layout.focus(id) gives a module the keys".to_string()
                ));
            }
            if matches!(action, Action::WorkspaceChoose | Action::WorkspaceRename) {
                return Err(mlua::Error::RuntimeError(format!(
                    "eitri.layout.run: {name} waits for a key; eitri.layout.switch(name) switches workspaces"
                )));
            }
            if !action.is_layout() {
                return Err(mlua::Error::RuntimeError(format!(
                    "eitri.layout.run: {name} is not a layout action"
                )));
            }
            queue(&run_store, "run", LayoutRequest::Run(action))
        })?,
    )?;

    let float_store = store.clone();
    table.set(
        "float",
        super::kernel::registering_function(lua, "eitri.layout.float", move |_, (id, rect): (String, Value)| {
            let id = module_arg("float", &id)?;
            if id == ModuleId::editor() {
                return Err(mlua::Error::RuntimeError(
                    "eitri.layout.float: the editor never floats".into(),
                ));
            }
            let rect = match rect {
                Value::Nil => None,
                Value::Table(t) => {
                    Some(parse_float_rect(&t, "eitri.layout.float", None).map_err(mlua::Error::RuntimeError)?)
                }
                other => {
                    return Err(mlua::Error::RuntimeError(format!(
                        "eitri.layout.float: expected a table, got {}",
                        float_type_name(&other)
                    )))
                }
            };
            queue(&float_store, "float", LayoutRequest::Float(id, rect))
        })?,
    )?;

    let mark_store = store.clone();
    table.set(
        "mark",
        super::kernel::registering_function(lua, "eitri.layout.mark", move |_, id: String| {
            let id = module_arg("mark", &id)?;
            queue(&mark_store, "mark", LayoutRequest::Mark(id))
        })?,
    )?;

    let export_store = store.clone();
    table.set(
        "export",
        super::kernel::registering_function(lua, "eitri.layout.export", move |_, ()| {
            let store = export_store
                .try_borrow()
                .map_err(|_| mlua::Error::RuntimeError("eitri.layout.export: the layout is being changed".into()))?;
            let live = store.layout.as_ref().filter(|_| store.accepting).ok_or_else(|| {
                mlua::Error::RuntimeError(
                    "eitri.layout.export: only once the window exists (in a command or an event handler)".into(),
                )
            })?;
            let layout = live
                .read()
                .ok_or_else(|| mlua::Error::RuntimeError("eitri.layout.export: the layout is being changed".into()))?;
            let workspaces = store
                .workspaces
                .as_ref()
                .map(|ws| ws.read().ok_or(()))
                .transpose()
                .map_err(|_| {
                    mlua::Error::RuntimeError("eitri.layout.export: the workspaces are being changed".into())
                })?;
            Ok(crate::layout::export_lua(&layout, workspaces.as_ref()))
        })?,
    )?;

    table.set(
        "get",
        super::kernel::registering_function(lua, "eitri.layout.get", move |lua, ()| {
            let store = store.borrow();
            let live = store.layout.as_ref().filter(|_| store.accepting).ok_or_else(|| {
                mlua::Error::RuntimeError(
                    "eitri.layout.get: only once the window exists (in a command or an event handler)".to_string(),
                )
            })?;
            let layout = live.read().ok_or_else(|| {
                mlua::Error::RuntimeError(
                    "eitri.layout.get: the layout is being changed; call it from a command or an event handler"
                        .to_string(),
                )
            })?;
            let table = match crate::layout::export::root_written_as_child(&layout) {
                Some(child) => node_table(lua, child, None)?,
                None => con_table(lua, layout.root(), None)?,
            };
            if !layout.floats().is_empty() {
                let floats = lua.create_table()?;
                for (i, float) in layout.floats().iter().enumerate() {
                    let entry = lua.create_table()?;
                    entry.raw_set(1, float.id.as_str())?;
                    entry.set("cx", float.rect.cx)?;
                    entry.set("cy", float.rect.cy)?;
                    entry.set("w", float.rect.w)?;
                    entry.set("h", float.rect.h)?;
                    if float.scratch {
                        entry.set("scratch", true)?;
                    }
                    floats.raw_set(i + 1, entry)?;
                }
                table.set("floating", floats)?;
            }
            table.set("focus", layout.focus().as_str())?;
            // An away module is listed once, under `away`: what the other window holds is not a hide.
            table.set(
                "hidden",
                lua.create_sequence_from(
                    layout
                        .hidden()
                        .iter()
                        .filter(|id| !layout.is_away(id))
                        .map(ModuleId::as_str),
                )?,
            )?;
            table.set(
                "away",
                lua.create_sequence_from(layout.away().iter().map(ModuleId::as_str))?,
            )?;
            table.set("window", store.window)?;
            let zoomed = match layout.zoom() {
                Some(crate::layout::Zoom::Module(id)) => Some(id.clone()),
                Some(crate::layout::Zoom::Con(id)) => layout
                    .root()
                    .find(*id)
                    .and_then(|c| c.focused_leaf(&|m| layout.is_shown(m))),
                None => None,
            };
            table.set("zoomed", zoomed.as_ref().map(ModuleId::as_str))?;
            table.set("marked", layout.mark().map(ModuleId::as_str))?;
            if let Some(workspaces) = &store.workspaces {
                let workspaces = workspaces.read().ok_or_else(|| {
                    mlua::Error::RuntimeError("eitri.layout.get: the workspaces are being changed".to_string())
                })?;
                table.set("workspace", workspaces.current())?;
                table.set(
                    "workspaces",
                    lua.create_sequence_from(workspaces.order().iter().map(String::as_str))?,
                )?;
            }
            Ok(table)
        })?,
    )?;

    eitri.set("layout", table)?;
    Ok(())
}

fn node_table(lua: &Lua, node: &Node, share: Option<f32>) -> mlua::Result<Table> {
    match node {
        Node::Leaf(id) => {
            let table = lua.create_table()?;
            table.raw_set(1, id.as_str())?;
            if let Some(share) = share {
                table.raw_set(2, f64::from(share))?;
            }
            Ok(table)
        }
        Node::Con(c) => con_table(lua, c, share),
    }
}

/// A container in the default grammar: its orientation, then every child, each but the last with its
/// share (the last takes what the others leave).
fn con_table(lua: &Lua, c: &Con, share: Option<f32>) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    table.raw_set(1, crate::layout::export::head_name(c.layout))?;
    if c.layout.axis().is_none() {
        table.raw_set("active", c.focus + 1)?;
    }
    let last = c.children.len().saturating_sub(1);
    for (i, child) in c.children.iter().enumerate() {
        let share = (i < last).then_some(child.share);
        table.raw_set(i + 2, node_table(lua, &child.node, share)?)?;
    }
    if let Some(share) = share {
        table.raw_set("share", f64::from(share))?;
    }
    Ok(table)
}

fn module_arg(call: &str, id: &str) -> mlua::Result<ModuleId> {
    ModuleId::parse(id).map_err(|e| mlua::Error::RuntimeError(format!("eitri.layout.{call}: {e}")))
}

fn queue(store: &RefCell<LayoutStore>, call: &str, request: LayoutRequest) -> mlua::Result<()> {
    let mut store = store.borrow_mut();
    if !store.accepting {
        return Err(mlua::Error::RuntimeError(format!(
            "eitri.layout.{call}: only once the window exists (in a command or an event handler); \
             eitri.layout.default shapes the first window"
        )));
    }
    store.requests.push(request);
    Ok(())
}

/// An open rule's options, as `eitri.layout.rule` and `eitri.layout.open` take them; `call` starts every refusal.
fn parse_rule(call: &str, opts: &Value, editor: bool) -> Result<OpenRule, String> {
    let Value::Table(table) = opts else {
        return Err(format!(
            "{call}: the options must be a table, not a {}",
            opts.type_name()
        ));
    };
    let mut rule = OpenRule::default();
    let mut size = None;
    for pair in table.clone().pairs::<Value, Value>() {
        let (key, value) = pair.map_err(|e| format!("{call}: {e}"))?;
        let key = match &key {
            Value::String(s) => s.to_str().map_err(|e| format!("{call}: {e}"))?.to_string(),
            _ => {
                return Err(format!(
                    "{call}: option names are strings (at, float, size, scratchpad, focus)"
                ))
            }
        };
        match key.as_str() {
            "at" => {
                let Value::String(s) = &value else {
                    return Err(format!("{call}: at must be a string, not a {}", value.type_name()));
                };
                let s = s.to_str().map_err(|e| format!("{call}: {e}"))?.to_string();
                rule.at = Some(crate::layout::rules::parse_at(&s).ok_or_else(|| {
                    format!(
                        "{call}: at '{s}' is not one of side, bottom, main, below-editor, right, below, left, \
                         above, tab"
                    )
                })?);
            }
            "float" | "scratchpad" | "focus" => {
                let Value::Boolean(on) = value else {
                    return Err(format!("{call}: {key} must be a boolean, not a {}", value.type_name()));
                };
                match key.as_str() {
                    "float" => rule.float = on,
                    "scratchpad" => rule.scratchpad = on,
                    _ => rule.focus = on,
                }
            }
            "size" => size = Some(value),
            other => {
                return Err(format!(
                    "{call}: unknown option '{other}' (at, float, size, scratchpad, focus)"
                ))
            }
        }
    }
    if rule.scratchpad {
        rule.float = true;
    }
    if rule.float && rule.at.is_some() {
        return Err(format!("{call}: a floating module has no 'at'; drop one of them"));
    }
    if rule.float && editor {
        return Err(format!("{call}: the editor never floats"));
    }
    if let Some(value) = size {
        rule.size = Some(parse_rule_size(call, &value, rule.float)?);
    }
    Ok(rule)
}

fn parse_rule_size(call: &str, value: &Value, floating: bool) -> Result<RuleSize, String> {
    let number = |v: &Value| match v {
        Value::Integer(i) => Some(*i as f64),
        Value::Number(n) => Some(*n),
        _ => None,
    };
    let fraction = |n: f64| n > 0.0 && n <= 1.0;
    match (value, floating) {
        (Value::Table(t), true) => {
            let (mut w, mut h) = (None, None);
            for pair in t.clone().pairs::<Value, Value>() {
                let (k, v) = pair.map_err(|e| format!("{call}: {e}"))?;
                let slot = match &k {
                    Value::Integer(1) => &mut w,
                    Value::Integer(2) => &mut h,
                    Value::String(s) if s.to_str().is_ok_and(|s| s == "w") => &mut w,
                    Value::String(s) if s.to_str().is_ok_and(|s| s == "h") => &mut h,
                    _ => {
                        return Err(format!(
                            "{call}: a floating size is {{ w, h }}, two fractions of the grid"
                        ))
                    }
                };
                *slot = number(&v);
            }
            match (w, h) {
                (Some(w), Some(h)) if fraction(w) && fraction(h) => Ok(RuleSize::Float {
                    w: w as f32,
                    h: h as f32,
                }),
                _ => Err(format!(
                    "{call}: a floating size needs w and h, each a fraction of the grid above 0 and at most 1"
                )),
            }
        }
        (Value::Table(_), false) => Err(format!(
            "{call}: a tiled size is one share between 0 and 1, not a table"
        )),
        (_, true) => Err(format!(
            "{call}: a floating size is {{ w, h }}, two fractions of the grid"
        )),
        (_, false) => match number(value) {
            Some(s) if s > 0.0 && s < 1.0 => Ok(RuleSize::Share(s as f32)),
            Some(s) => Err(format!("{call}: size must be between 0 and 1, not {s}")),
            None => Err(format!("{call}: size must be a number, not a {}", value.type_name())),
        },
    }
}

/// `eitri.layout.default`'s argument as a tree (the module doc's grammar).
pub fn parse_default(value: &Value) -> Result<Node, String> {
    parse_tree(value).map(|tree| tree.tiled)
}

/// Reads the tiled tree and floats together so every module appears only once.
pub fn parse_tree(value: &Value) -> Result<LuaTree, String> {
    let Value::Table(table) = value else {
        return Err(format!("expected a table, got {}", value.type_name()));
    };
    let root = parse_node(table, "the tree")?;
    let node = spec::build(&root, &|name| ModuleId::parse(name).map_err(|e| e.to_string()))?.tiled;
    let floats = parse_floats(table.raw_get::<Value>("floating").map_err(|e| e.to_string())?)?;
    let leaves = node.leaves();
    let mut seen = BTreeSet::new();
    for id in leaves.iter().chain(floats.iter().map(|f| &f.id)) {
        if !seen.insert(id.clone()) {
            return Err(format!("'{id}' appears twice"));
        }
    }
    for built_in in [ModuleId::editor(), ModuleId::agent()] {
        if !seen.contains(&built_in) {
            return Err(format!("the tree has no '{built_in}', which every window has"));
        }
    }
    Ok(LuaTree { tiled: node, floats })
}

fn float_type_name(value: &Value) -> &str {
    if matches!(value, Value::Integer(_) | Value::Number(_)) {
        "number"
    } else {
        value.type_name()
    }
}

fn parse_floats(value: Value) -> Result<Vec<Float>, String> {
    let table = match value {
        Value::Nil => return Ok(Vec::new()),
        Value::Table(table) => table,
        other => {
            return Err(format!(
                "floating: expected a table of floats, got {}",
                float_type_name(&other)
            ))
        }
    };
    let len = table.raw_len();
    for pair in table.clone().pairs::<Value, Value>() {
        let (key, _) = pair.map_err(|e| e.to_string())?;
        if !matches!(key, Value::Integer(i) if i >= 1 && i <= len as i64) {
            return Err("floating: expected a sequence of floats".into());
        }
    }
    (1..=len)
        .map(|i| {
            let at = format!("floating item {i}");
            let value: Value = table.raw_get(i).map_err(|e| e.to_string())?;
            let Value::Table(entry) = value else {
                return Err(format!("{at}: expected a table, got {}", float_type_name(&value)));
            };
            let head: Value = entry.raw_get(1).map_err(|e| e.to_string())?;
            let Value::String(head) = head else {
                return Err(format!("{at}: the first item must be a module id"));
            };
            let id = ModuleId::parse(&head.to_str().map_err(|e| e.to_string())?).map_err(|e| format!("{at}: {e}"))?;
            if id == ModuleId::editor() {
                return Err(format!("{at}: the editor never floats"));
            }
            let at = format!("{at} ('{id}')");
            let rect = parse_float_rect(&entry, &at, Some(&id))?;
            let scratch = match entry.raw_get::<Value>("scratch").map_err(|e| e.to_string())? {
                Value::Nil => false,
                Value::Boolean(b) => b,
                _ => return Err(format!("{at}: scratch is true or false")),
            };
            Ok(Float { id, rect, scratch })
        })
        .collect()
}

fn parse_float_rect(table: &Table, at: &str, id: Option<&ModuleId>) -> Result<FloatRect, String> {
    for pair in table.clone().pairs::<Value, Value>() {
        let (key, _) = pair.map_err(|e| e.to_string())?;
        match key {
            Value::Integer(1) if id.is_some() => {}
            Value::String(s) if matches!(s.to_str().map_err(|e| e.to_string())?.as_ref(), "cx" | "cy" | "w" | "h") => {}
            Value::String(s) if id.is_some() && s.to_str().map_err(|e| e.to_string())?.as_ref() == "scratch" => {}
            Value::String(s) => return Err(format!("{at}: unknown field {}", s.to_string_lossy())),
            _ => {
                return Err(match id {
                    Some(id) => format!("{at}: a float is {{'{id}', cx = .., cy = .., w = .., h = ..}}"),
                    None => format!("{at}: a rectangle is {{cx = .., cy = .., w = .., h = ..}}"),
                });
            }
        }
    }
    let fields = ["cx", "cy", "w", "h"].map(|key| table.raw_get::<Value>(key).map_err(|e| e.to_string()));
    let [cx, cy, w, h] = fields;
    let values = [cx?, cy?, w?, h?];
    if values.iter().any(Value::is_nil) {
        return Err(format!("{at}: cx, cy, w and h are required"));
    }
    let mut numbers = [0.0; 4];
    for (i, value) in values.into_iter().enumerate() {
        let n = match value {
            Value::Integer(n) => Some(n as f64),
            Value::Number(n) => Some(n),
            _ => None,
        };
        let field = ["cx", "cy", "w", "h"][i];
        let valid = n.is_some_and(|n| {
            n.is_finite()
                && if i < 2 {
                    (0.0..=1.0).contains(&n)
                } else {
                    n.fract() == 0.0 && (1.0..=f64::from(MAX_FLOAT_PX)).contains(&n)
                }
        });
        if !valid {
            return Err(if i < 2 {
                format!("{at}: {field} is a number from 0 to 1")
            } else {
                format!("{at}: {field} is a whole number of pixels from 1 to {MAX_FLOAT_PX}")
            });
        }
        numbers[i] = n.unwrap();
    }
    Ok(FloatRect {
        cx: numbers[0] as f32,
        cy: numbers[1] as f32,
        w: numbers[2] as i32,
        h: numbers[3] as i32,
    })
}

/// Reads Lua's representation without deciding container shares or module uniqueness.
fn parse_node(table: &Table, at: &str) -> Result<SpecNode, String> {
    let head: Value = table.raw_get(1).map_err(|e| e.to_string())?;
    let Value::String(head) = head else {
        return Err(format!(
            "{at}: the first item must be 'row', 'column', 'tabbed', 'stacked' or a module id"
        ));
    };
    let head = head.to_str().map_err(|e| e.to_string())?.to_string();
    let len = table.raw_len();
    let kind = SplitKind::parse(&head);
    refuse_other_fields(table, len, kind.is_some(), &head, at)?;
    let Some(kind) = kind else {
        ModuleId::parse(&head).map_err(|e| format!("{at}: {e}"))?;
        let share = match len {
            1 => None,
            2 => Some(share_of(
                table.raw_get(2).map_err(|e| e.to_string())?,
                &format!("{at} ('{head}')"),
            )?),
            _ => return Err(format!("{at}: a module is {{'{head}'}} or {{'{head}', share}}")),
        };
        return Ok(SpecNode::Module { name: head, share });
    };
    let children = (2..=len)
        .map(|i| match table.raw_get::<Value>(i) {
            Ok(Value::Table(child)) => parse_node(&child, &format!("item {} of the {head}", i - 1)),
            Ok(other) => Err(format!(
                "item {} of the {head}: expected a table, got {}",
                i - 1,
                other.type_name()
            )),
            Err(e) => Err(e.to_string()),
        })
        .collect::<Result<Vec<_>, _>>()?;
    let active = match table.raw_get::<Value>("active").map_err(|e| e.to_string())? {
        Value::Nil => None,
        Value::Integer(n) => Some(n),
        Value::Number(n) if n.fract() == 0.0 && n >= 1.0 && n <= children.len() as f64 => Some(n as i64),
        value => {
            let n = match value {
                Value::Number(n) => n.to_string(),
                other => format!("{other:?}"),
            };
            return Err(format!("{at}: active = {n}: the {head} has {} items", children.len()));
        }
    };
    let share = match table.raw_get::<Value>("share").map_err(|e| e.to_string())? {
        Value::Nil => None,
        other => Some(share_of(other, &format!("{at}'s share"))?),
    };
    Ok(SpecNode::Split {
        kind,
        children,
        share,
        active,
    })
}

/// Refuse unknown fields so misspelled shares and active indices cannot silently change a layout.
fn refuse_other_fields(table: &Table, len: usize, split: bool, head: &str, at: &str) -> Result<(), String> {
    for pair in table.pairs::<Value, Value>() {
        let (key, _) = pair.map_err(|e| e.to_string())?;
        let name = match &key {
            Value::Integer(i) if usize::try_from(*i).is_ok_and(|i| (1..=len).contains(&i)) => continue,
            Value::String(s) => s.to_string_lossy(),
            Value::Integer(i) => format!("[{i}]"),
            other => format!("a {} key", other.type_name()),
        };
        let message = match name.as_str() {
            "floating" if at == "the tree" => continue,
            "share" if split => continue,
            "active" if matches!(head, "tabbed" | "stacked") => continue,
            "share" => format!(
                "{at}: a module's share is its second item, {{'{head}', 0.6}}; share = is for a row or a column"
            ),
            _ => format!("{at}: unknown field {name}"),
        };
        return Err(message);
    }
    Ok(())
}

fn share_of(value: Value, at: &str) -> Result<f64, String> {
    let share = match value {
        Value::Number(n) => n,
        Value::Integer(n) => n as f64,
        other => return Err(format!("{at}: a share is a number, got {}", other.type_name())),
    };
    Ok(share)
}

#[cfg(test)]
mod tests {
    #[test]
    fn workspace_defines_a_template_in_init_lua_only() {
        let (lua, store) = engine();
        lua.load(
            "eitri.layout.workspace('review', { 'row', {'agent'}, {'editor'}, floating = { \
             {'terminal#2', cx = 0.5, cy = 0.5, w = 600, h = 400, scratch = true} } })",
        )
        .exec()
        .unwrap();
        {
            let store = store.borrow();
            let defined = store.defined_workspaces();
            assert_eq!(defined.len(), 1);
            assert_eq!(defined[0].0, "review");
            let tree = defined[0].1.as_ref().unwrap();
            assert_eq!(tree.tiled.leaves(), vec![ModuleId::agent(), ModuleId::editor()]);
            assert_eq!(tree.floats.len(), 1);
        }
        for (source, why) in [
            (
                "eitri.layout.workspace('main', {'row', {'editor'}, {'agent'}})",
                "eitri.layout.default shapes 'main'",
            ),
            (
                "eitri.layout.workspace('', {'row', {'editor'}, {'agent'}})",
                "is not a workspace name",
            ),
            (
                "eitri.layout.workspace('x', {'row', {'editor'}})",
                "a row needs two or more items",
            ),
            (
                "eitri.layout.workspace(3, {'row', {'editor'}, {'agent'}})",
                "the name must be a string",
            ),
        ] {
            let err = lua.load(source).exec().unwrap_err().to_string();
            assert!(
                err.contains("eitri.layout.workspace") && err.contains(why),
                "{source}: {err}"
            );
        }
        assert!(
            store
                .borrow()
                .defined_workspaces()
                .iter()
                .any(|(name, tree)| name == "x" && tree.is_err()),
            "a broken template is kept as its error, for the shell to stop on"
        );
        store.borrow_mut().accept_requests();
        let err = lua
            .load("eitri.layout.workspace('late', {'row', {'editor'}, {'agent'}})")
            .exec()
            .unwrap_err()
            .to_string();
        assert!(err.contains("only in init.lua"), "{err}");
    }

    #[test]
    fn switch_queues_a_workspace_name_once_the_window_exists() {
        let (lua, store) = engine();
        let err = lua.load("eitri.layout.switch('study')").exec().unwrap_err().to_string();
        assert!(err.contains("only once the window exists"), "{err}");
        store.borrow_mut().accept_requests();
        lua.load("eitri.layout.switch('study')").exec().unwrap();
        let err = lua.load("eitri.layout.switch('a\\nb')").exec().unwrap_err().to_string();
        assert!(
            err.contains("eitri.layout.switch") && err.contains("is not a workspace name"),
            "{err}"
        );
        assert_eq!(
            store.borrow_mut().take_requests(),
            [LayoutRequest::Switch("study".to_string())]
        );
    }

    #[test]
    fn get_names_the_current_workspace_and_every_workspace_in_order() {
        let (lua, store, live) = attached(Layout::initial(&[]).unwrap());
        let mut ws = crate::layout::Workspaces::single();
        ws.create_copy(&live.borrow());
        store.borrow_mut().attach_workspaces(copied(&Rc::new(RefCell::new(ws))));
        let got: String = lua
            .load("local t = eitri.layout.get(); return t.workspace .. ':' .. table.concat(t.workspaces, ',')")
            .eval()
            .unwrap();
        assert_eq!(got, "main:main,2");
    }

    #[test]
    fn get_fed_to_workspace_gives_the_same_arrangement() {
        use crate::layout::{arrange, reconcile_default_floating, Frame, Size};
        let t2 = ModuleId::parse("terminal#2").unwrap();
        let tabs = Con::new(
            ConLayout::Tabbed,
            vec![
                Child::new(Node::Leaf(ModuleId::agent()), 0.5),
                Child::new(Node::Leaf(t2.clone()), 0.5),
            ],
        );
        let root = Node::split(
            crate::layout::Axis::Row,
            0.6,
            Node::Leaf(ModuleId::editor()),
            Node::Con(tabs),
        );
        let mut live = Layout::new(root, ModuleId::editor()).unwrap();
        live.add_float(
            &ModuleId::terminal(),
            crate::layout::FloatRect {
                cx: 0.5,
                cy: 0.45,
                w: 700,
                h: 400,
            },
            true,
        )
        .unwrap();
        let (lua, store, live) = attached(live);
        lua.load(
            "t = eitri.layout.get(); t.focus = nil; t.hidden = nil; t.away = nil; t.zoomed = nil; t.marked = nil; \
             t.workspace = nil; t.workspaces = nil",
        )
        .exec()
        .unwrap();
        // Definitions are accepted only before the window takes requests.
        store.borrow_mut().accepting = false;
        lua.load("eitri.layout.workspace('w', t)").exec().unwrap();
        let tree = store.borrow().defined_workspaces()[0].1.clone().unwrap();
        let back = reconcile_default_floating(tree.tiled, tree.floats, &[]).unwrap().layout;
        for size in [
            Size { w: 1280, h: 721 },
            Size { w: 1920, h: 1040 },
            Size { w: 800, h: 600 },
        ] {
            let frame = Frame::new(size, 1);
            let (a, b) = (arrange(&back, &frame), arrange(&live.borrow(), &frame));
            assert_eq!(a.modules, b.modules, "{size:?}");
            assert_eq!(a.floats, b.floats, "{size:?}");
        }
        assert!(back.float_of(&ModuleId::terminal()).unwrap().scratch);
    }

    use super::*;
    use crate::keymap::Action;
    use crate::layout::tree::MIN_RATIO;
    use crate::layout::{Child, ConLayout, Direction, Float, FloatRect, Layout};

    fn engine() -> (Lua, Rc<RefCell<LayoutStore>>) {
        let lua = Lua::new();
        let eitri = lua.create_table().unwrap();
        let store = Rc::new(RefCell::new(LayoutStore::default()));
        install(&lua, &eitri, store.clone()).unwrap();
        lua.globals().set("eitri", eitri).unwrap();
        (lua, store)
    }

    /// A query over `cell` as a window's controller answers one: a copy when the cell is free, else none.
    fn copied<T: Clone + 'static>(cell: &Rc<RefCell<T>>) -> Query<T> {
        let cell = cell.clone();
        Query::new(move || cell.try_borrow().ok().map(|value| value.clone()))
    }

    fn attached(layout: Layout) -> (Lua, Rc<RefCell<LayoutStore>>, Rc<RefCell<Layout>>) {
        let (lua, store) = engine();
        let live = Rc::new(RefCell::new(layout));
        store.borrow_mut().attach(copied(&live));
        store.borrow_mut().set_lua_panels(vec!["notes".to_string()]);
        store.borrow_mut().accept_requests();
        (lua, store, live)
    }

    #[test]
    fn rule_records_an_open_rule_in_init_lua_only() {
        use crate::layout::rules::{At, OpenPoint, OpenRule, RuleSize, RuleSource};
        use crate::layout::{Direction, Placement};
        let (lua, store) = engine();
        lua.load(
            "eitri.layout.rule('terminal', { at = 'below-editor' })\n\
             eitri.layout.rule('terminal#2', { float = true, size = { 0.6, 0.5 }, scratchpad = true })\n\
             eitri.layout.rule('agent', { at = 'right' })\n\
             eitri.layout.rule('lua:notes', { at = 'tab', focus = false })\n\
             eitri.layout.rule('agent#3', { size = 0.3 })\n\
             eitri.layout.rule('terminal#3', { float = true, size = { w = 0.5, h = 0.4 } })",
        )
        .exec()
        .unwrap();
        let store = store.borrow();
        let rules = store.rules().unwrap();
        let t2 = ModuleId::parse("terminal#2").unwrap();
        let (rule, source) = rules.resolve(&t2, OpenPoint::NewInstance, None);
        assert_eq!(source, RuleSource::Module);
        assert_eq!(
            rule,
            OpenRule {
                at: None,
                float: true,
                size: Some(RuleSize::Float { w: 0.6, h: 0.5 }),
                scratchpad: true,
                focus: true
            }
        );
        let (rule, _) = rules.resolve(&ModuleId::parse("terminal#3").unwrap(), OpenPoint::NewInstance, None);
        assert_eq!(rule.size, Some(RuleSize::Float { w: 0.5, h: 0.4 }));
        let (rule, _) = rules.resolve(&ModuleId::lua("notes"), OpenPoint::Launch, None);
        assert_eq!((rule.at, rule.focus), (Some(At::Tab), false));
        let (rule, _) = rules.resolve(&ModuleId::parse("agent#2").unwrap(), OpenPoint::Break, None);
        assert_eq!(rule.at, Some(At::Beside(Direction::Right)));
        let (rule, _) = rules.resolve(&ModuleId::parse("agent#3").unwrap(), OpenPoint::Break, None);
        assert_eq!((rule.at, rule.size), (None, Some(RuleSize::Share(0.3))));
        let (rule, _) = rules.resolve(&ModuleId::terminal(), OpenPoint::Launch, None);
        assert_eq!(rule.at, Some(At::Placement(Placement::BelowEditor)));
    }

    #[test]
    fn a_malformed_rule_is_a_startup_failure_naming_the_call() {
        for (source, why) in [
            (
                "eitri.layout.rule('terminal', { at = 'top' })",
                "at 'top' is not one of side, bottom, main, below-editor, right, below, left, above, tab",
            ),
            (
                "eitri.layout.rule('terminal', { at = 3 })",
                "at must be a string, not a integer",
            ),
            (
                "eitri.layout.rule('terminal', { float = 'yes' })",
                "float must be a boolean, not a string",
            ),
            (
                "eitri.layout.rule('terminal', { focus = 1 })",
                "focus must be a boolean, not a integer",
            ),
            (
                "eitri.layout.rule('terminal', { width = 3 })",
                "unknown option 'width' (at, float, size, scratchpad, focus)",
            ),
            (
                "eitri.layout.rule('terminal', { float = true, at = 'right' })",
                "a floating module has no 'at'; drop one of them",
            ),
            (
                "eitri.layout.rule('terminal', { scratchpad = true, at = 'right' })",
                "a floating module has no 'at'; drop one of them",
            ),
            (
                "eitri.layout.rule('terminal', { size = 1 })",
                "size must be between 0 and 1, not 1",
            ),
            (
                "eitri.layout.rule('terminal', { size = 0 })",
                "size must be between 0 and 1, not 0",
            ),
            (
                "eitri.layout.rule('terminal', { size = 'big' })",
                "size must be a number, not a string",
            ),
            (
                "eitri.layout.rule('terminal', { size = { 0.5, 0.5 } })",
                "a tiled size is one share between 0 and 1, not a table",
            ),
            (
                "eitri.layout.rule('terminal', { float = true, size = 0.5 })",
                "a floating size is { w, h }, two fractions of the grid",
            ),
            (
                "eitri.layout.rule('terminal', { float = true, size = { 0.5 } })",
                "a floating size needs w and h, each a fraction of the grid above 0 and at most 1",
            ),
            (
                "eitri.layout.rule('terminal', { float = true, size = { 0.5, 1.5 } })",
                "a floating size needs w and h, each a fraction of the grid above 0 and at most 1",
            ),
            (
                "eitri.layout.rule('terminal', { float = true, size = { w = 0.5, d = 0.5 } })",
                "a floating size is { w, h }, two fractions of the grid",
            ),
            (
                "eitri.layout.rule('editor', { float = true })",
                "the editor never floats",
            ),
            (
                "eitri.layout.rule('editor', { scratchpad = true })",
                "the editor never floats",
            ),
            (
                "eitri.layout.rule('terminal#1', { at = 'right' })",
                "instance 1 is 'terminal'",
            ),
            (
                "eitri.layout.rule('terminal', 'right')",
                "the options must be a table, not a string",
            ),
            ("eitri.layout.rule(3, {})", "the target must be a string, not a integer"),
        ] {
            let (lua, store) = engine();
            let err = lua.load(source).exec().unwrap_err().to_string();
            assert!(
                err.contains("eitri.layout.rule") && err.contains(why),
                "{source}: {err}"
            );
            let kept = store.borrow().rules().unwrap_err();
            assert!(
                kept.contains(why),
                "{source}: the store keeps the error for the shell to stop on: {kept}"
            );
        }
        let (lua, store) = engine();
        lua.load("eitri.layout.rule('agent', {})").exec().unwrap();
        let err = lua
            .load("eitri.layout.rule('agent', { at = 'left' })")
            .exec()
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("eitri.layout.rule('agent'): a rule for it is already set"),
            "{err}"
        );
        assert!(store.borrow().rules().is_err());
        let (lua, store) = engine();
        store.borrow_mut().accept_requests();
        let err = lua
            .load("eitri.layout.rule('agent', {})")
            .exec()
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("eitri.layout.rule: only in init.lua, before the window exists"),
            "{err}"
        );
    }

    #[test]
    fn open_queues_a_kind_and_its_options_once_the_window_exists() {
        use crate::layout::rules::{At, OpenRule};
        use crate::layout::{Direction, ModuleKind};
        let (lua, store) = engine();
        let err = lua
            .load("eitri.layout.open('terminal')")
            .exec()
            .unwrap_err()
            .to_string();
        assert!(err.contains("only once the window exists"), "{err}");
        store.borrow_mut().accept_requests();
        lua.load("eitri.layout.open('terminal'); eitri.layout.open('agent', { at = 'below', focus = false })")
            .exec()
            .unwrap();
        assert_eq!(
            store.borrow_mut().take_requests(),
            [
                LayoutRequest::Open(ModuleKind::Terminal, None),
                LayoutRequest::Open(
                    ModuleKind::Agent,
                    Some(OpenRule {
                        at: Some(At::Beside(Direction::Down)),
                        focus: false,
                        ..OpenRule::default()
                    })
                ),
            ]
        );
        for (source, why) in [
            (
                "eitri.layout.open('editor')",
                "kind must be \"terminal\" or \"agent\", not \"editor\"",
            ),
            (
                "eitri.layout.open(2)",
                "kind must be \"terminal\" or \"agent\", not a integer",
            ),
            (
                "eitri.layout.open('terminal', { at = 'nowhere' })",
                "eitri.layout.open('terminal'): at 'nowhere' is not one of",
            ),
        ] {
            let err = lua.load(source).exec().unwrap_err().to_string();
            assert!(err.contains(why), "{source}: {err}");
        }
    }

    #[test]
    fn run_queues_a_layout_action_by_its_key_name_with_its_options() {
        let (lua, store, _) = attached(Layout::initial(&[]).unwrap());
        lua.load(
            "eitri.layout.run('move.far-left'); eitri.layout.run('resize.left', { cells = 5 }); \
             eitri.layout.run('layout.tiled'); eitri.layout.run('module.notes'); eitri.layout.run('module.kill')",
        )
        .exec()
        .unwrap();
        assert_eq!(
            store.borrow_mut().take_requests(),
            [
                LayoutRequest::Run(Action::MoveFar(Direction::Left)),
                LayoutRequest::Run(Action::Resize {
                    dir: Direction::Left,
                    cells: 5
                }),
                LayoutRequest::Run(Action::Tiled),
                LayoutRequest::Run(Action::Module(ModuleId::lua("notes"))),
                LayoutRequest::Run(Action::ModuleKill),
            ]
        );
    }

    #[test]
    fn run_refuses_what_is_not_a_layout_action_or_waits_for_a_key() {
        let (lua, store, _) = attached(Layout::initial(&[]).unwrap());
        for (source, why) in [
            ("eitri.layout.run('tab.new')", "tab.new is not a layout action"),
            ("eitri.layout.run('hint')", "hint is not a layout action"),
            ("eitri.layout.run('zoomm')", "\"zoomm\" is not an action"),
            ("eitri.layout.run('resize.left', { cells = 0 })", "1 to 500"),
            (
                "eitri.layout.run('zoom', { repeatable = true })",
                "repeatable is a key binding's option",
            ),
            ("eitri.layout.run('workspace.choose')", "eitri.layout.switch"),
            ("eitri.layout.run('workspace.rename')", "eitri.layout.switch"),
            ("eitri.layout.run('split.right')", "eitri.layout.split"),
            ("eitri.layout.run('select.number')", "eitri.layout.focus"),
            ("eitri.layout.run('module.nope')", "module.nope"),
            ("eitri.layout.run(3)", "must be a string"),
            ("eitri.layout.run('zoom', 'fast')", "options must be a table"),
        ] {
            let err = lua.load(source).exec().unwrap_err().to_string();
            assert!(err.contains("eitri.layout.run") && err.contains(why), "{source}: {err}");
        }
        assert!(store.borrow_mut().take_requests().is_empty());
    }

    #[test]
    fn run_mark_and_get_wait_for_the_window() {
        let (lua, store) = engine();
        for source in [
            "eitri.layout.run('zoom')",
            "eitri.layout.mark('agent')",
            "eitri.layout.get()",
            "eitri.layout.float('terminal')",
        ] {
            let err = lua.load(source).exec().unwrap_err().to_string();
            assert!(err.contains("only once the window exists"), "{source}: {err}");
        }
        assert!(store.borrow_mut().take_requests().is_empty());
    }

    #[test]
    fn mark_queues_its_module() {
        let (lua, store, _) = attached(Layout::initial(&[]).unwrap());
        lua.load("eitri.layout.mark('agent')").exec().unwrap();
        assert_eq!(
            store.borrow_mut().take_requests(),
            [LayoutRequest::Mark(ModuleId::agent())]
        );
        let err = lua.load("eitri.layout.mark('chat')").exec().unwrap_err().to_string();
        assert!(
            err.contains("eitri.layout.mark") && err.contains("unknown module 'chat'"),
            "{err}"
        );
    }

    #[test]
    fn get_names_the_focused_leaf_of_a_zoomed_container() {
        use crate::layout::{container_mode, focus_parent, ConLayout};
        let mut layout = Layout::new(
            Node::split(
                Axis::Row,
                0.5,
                Node::Leaf(ModuleId::editor()),
                Node::split(
                    Axis::Column,
                    0.5,
                    Node::Leaf(ModuleId::agent()),
                    Node::Leaf(ModuleId::terminal()),
                ),
            ),
            ModuleId::agent(),
        )
        .unwrap();
        container_mode(&mut layout, &ModuleId::agent(), ConLayout::Tabbed).unwrap();
        focus_parent(&mut layout);
        layout.toggle_zoom(&ModuleId::agent());
        assert!(layout.is_zoomed());
        assert_eq!(layout.zoomed(), None);
        let (lua, _, _) = attached(layout);
        assert_eq!(
            lua.load("return eitri.layout.get().zoomed").eval::<String>().unwrap(),
            "agent"
        );
    }

    #[test]
    fn get_reads_the_live_layout_in_the_default_grammar() {
        let mut layout = Layout::new(
            Node::split(
                Axis::Row,
                0.6,
                Node::Leaf(ModuleId::editor()),
                Node::split(
                    Axis::Column,
                    0.5,
                    Node::Leaf(ModuleId::agent()),
                    Node::Leaf(ModuleId::terminal()),
                ),
            ),
            ModuleId::editor(),
        )
        .unwrap();
        let frame = crate::layout::Frame::new(crate::layout::Size { w: 1280, h: 721 }, 1);
        crate::layout::hide(&mut layout, &ModuleId::terminal(), &frame).unwrap();
        layout.set_mark(&ModuleId::agent()).unwrap();
        layout.toggle_zoom(&ModuleId::agent());
        let (lua, _, _) = attached(layout);
        let got: String = lua
            .load(
                r#"
            local t = eitri.layout.get()
            return string.format("%s %s %.1f | %s %s %.1f %s | %s | focus=%s zoomed=%s marked=%s",
                t[1], t[2][1], t[2][2], t[3][1], t[3][2][1], t[3][2][2], t[3][3][1],
                table.concat(t.hidden, ","), t.focus, t.zoomed, t.marked)
            "#,
            )
            .eval()
            .unwrap();
        assert_eq!(
            got,
            "row editor 0.6 | column agent 0.5 terminal | terminal | focus=editor zoomed=agent marked=agent"
        );
    }

    #[test]
    fn get_fed_back_to_the_default_grammar_gives_the_same_rectangles() {
        use crate::layout::{arrange, Frame, Size};
        let layout = Layout::new(
            Node::split(
                Axis::Row,
                crate::layout::tree::DEFAULT_EDITOR_SHARE,
                Node::Leaf(ModuleId::editor()),
                Node::split(
                    Axis::Column,
                    0.6,
                    Node::Leaf(ModuleId::agent()),
                    Node::Leaf(ModuleId::terminal()),
                ),
            ),
            ModuleId::editor(),
        )
        .unwrap();
        let (lua, _, live) = attached(layout);
        let table: Value = lua
            .load(
                "local t = eitri.layout.get(); t.focus = nil; t.hidden = nil; t.away = nil; t.zoomed = nil; t.marked = nil; return t",
            )
            .eval()
            .unwrap();
        let back = Layout::new(parse_default(&table).unwrap(), ModuleId::editor()).unwrap();
        for size in [
            Size { w: 1280, h: 721 },
            Size { w: 1920, h: 1040 },
            Size { w: 800, h: 600 },
        ] {
            let frame = Frame::new(size, 1);
            assert_eq!(
                arrange(&back, &frame).modules,
                arrange(&live.borrow(), &frame).modules,
                "{size:?}"
            );
        }
    }

    #[test]
    fn get_keeps_the_head_of_a_one_tile_tabbed_root() {
        let tree = default_of("eitri.layout.default{ 'tabbed', {'editor'}, {'agent'} }").unwrap();
        let mut layout = Layout::new(tree, ModuleId::editor()).unwrap();
        let frame = crate::layout::Frame::new(crate::layout::Size { w: 1280, h: 721 }, 1);
        crate::layout::float_toggle(&mut layout, &ModuleId::agent(), &frame).unwrap();
        let (lua, _, _) = attached(layout);
        let table: Table = lua.load("return eitri.layout.get()").eval().unwrap();
        assert_eq!(table.get::<String>(1).unwrap(), "tabbed");
        assert_eq!(table.get::<Table>(2).unwrap().get::<String>(1).unwrap(), "editor");
    }

    #[test]
    fn a_stripped_get_reads_back_as_the_same_root() {
        for (head, mode) in [("tabbed", ConLayout::Tabbed), ("stacked", ConLayout::Stacked)] {
            let tree = default_of(&format!(
                "eitri.layout.default{{ '{head}', {{'editor'}}, {{'agent'}} }}"
            ))
            .unwrap();
            let mut layout = Layout::new(tree, ModuleId::editor()).unwrap();
            let frame = crate::layout::Frame::new(crate::layout::Size { w: 1280, h: 721 }, 1);
            crate::layout::float_toggle(&mut layout, &ModuleId::agent(), &frame).unwrap();
            let (lua, store, _) = attached(layout);
            store
                .borrow_mut()
                .attach_workspaces(copied(&Rc::new(RefCell::new(Workspaces::single()))));
            let table: Table = lua.load("return eitri.layout.get()").eval().unwrap();
            for key in ["focus", "hidden", "away", "zoomed", "marked", "workspace", "workspaces"] {
                table.raw_set(key, Value::Nil).unwrap();
            }
            let tree = parse_tree(&Value::Table(table)).unwrap();
            let root = tree.tiled.as_con().unwrap();
            assert_eq!(root.layout, mode);
            assert_eq!(root.children.len(), 1);
            assert_eq!(root.children[0].node, Node::Leaf(ModuleId::editor()));
            assert_eq!(tree.floats.len(), 1);
            assert_eq!(tree.floats[0].id, ModuleId::agent());
        }
    }

    #[test]
    fn the_grammar_says_tabbed_and_stacked_and_get_round_trips_them() {
        use crate::layout::{arrange, Frame, Size};
        for head in ["tabbed", "stacked"] {
            for source in [
                format!("{{ 'row', {{'editor', 0.6}}, {{'{head}', {{'agent'}}, {{'terminal'}}, active = 2}} }}"),
                format!("{{'{head}', {{'editor'}}, {{'agent'}}, active = 2}}"),
                format!("{{'row', {{'editor'}}, {{'{head}', {{'agent'}}, active = 1}} }}"),
            ] {
                let lua = Lua::new();
                let value = lua.load(&format!("return {source}")).eval::<Value>().unwrap();
                let tree = parse_default(&value).unwrap();
                let c = tree.as_con().unwrap();
                let tabs = if source.contains("'row'") {
                    assert_eq!(c.layout, ConLayout::SplitH);
                    c.children[1].node.as_con().unwrap()
                } else {
                    c
                };
                assert_eq!(
                    tabs.layout,
                    if head == "tabbed" {
                        ConLayout::Tabbed
                    } else {
                        ConLayout::Stacked
                    }
                );
                assert_eq!(tabs.focus, if source.contains("active = 1") { 0 } else { 1 });
                let focus = if source.contains("0.6") || source.contains("active = 1") {
                    ModuleId::editor()
                } else {
                    ModuleId::agent()
                };
                let layout = Layout::new(tree, focus.clone()).unwrap();
                let (lua, _, live) = attached(layout);
                let table: Table = lua.load("return eitri.layout.get()").eval().unwrap();
                assert_eq!(table.get::<String>("focus").unwrap(), focus.as_str());
                if table.get::<String>(1).unwrap() == head {
                    assert_eq!(table.get::<usize>("active").unwrap(), 2);
                }
                for key in ["focus", "hidden", "away", "zoomed", "marked"] {
                    table.raw_set(key, Value::Nil).unwrap();
                }
                let back = Layout::new(parse_default(&Value::Table(table)).unwrap(), focus).unwrap();
                for size in [
                    Size { w: 1280, h: 721 },
                    Size { w: 1920, h: 1040 },
                    Size { w: 800, h: 600 },
                ] {
                    let frame = Frame::new(size, 1);
                    let a = arrange(&back, &frame);
                    let b = arrange(&live.borrow(), &frame);
                    assert_eq!(a.modules, b.modules, "{source} {size:?}");
                    assert_eq!(
                        a.strips
                            .iter()
                            .map(|s| (s.layout, s.rect, &s.entries))
                            .collect::<Vec<_>>(),
                        b.strips
                            .iter()
                            .map(|s| (s.layout, s.rect, &s.entries))
                            .collect::<Vec<_>>(),
                        "{source} {size:?}",
                    );
                }
            }
        }
        for (source, message) in [
            ("{'tabbed'}", "a tabbed needs one or more items"),
            (
                "{'tabbed', {'editor'}, {'agent'}, active = 3}",
                "the tree: active = 3: the tabbed has 2 items",
            ),
            (
                "{'stacked', {'editor'}, {'agent'}, active = 0}",
                "active = 0: the stacked has 2 items",
            ),
            ("{'row', {'editor'}, {'agent'}, active = 1}", "unknown field active"),
        ] {
            let lua = Lua::new();
            let value = lua.load(&format!("return {source}")).eval::<Value>().unwrap();
            assert!(parse_default(&value).unwrap_err().contains(message), "{source}");
        }
    }

    #[test]
    fn a_row_of_three_is_one_container() {
        let tree = default_of("eitri.layout.default{ 'row', {'editor'}, {'agent'}, {'terminal'} }").unwrap();
        let c = tree.as_con().unwrap();
        assert_eq!(c.layout, crate::layout::ConLayout::SplitH);
        assert_eq!(c.children.len(), 3);
        assert!(c
            .children
            .iter()
            .all(|ch| matches!(ch.node, Node::Leaf(_)) && (ch.share - 1.0 / 3.0).abs() < 1e-6));
    }

    #[test]
    fn get_while_the_layout_is_being_changed_is_refused_not_a_panic() {
        let (lua, _, live) = attached(Layout::initial(&[]).unwrap());
        let _held = live.borrow_mut();
        let err = lua.load("eitri.layout.get()").exec().unwrap_err().to_string();
        assert!(
            err.contains("eitri.layout.get") && err.contains("being changed"),
            "{err}"
        );
    }

    #[test]
    fn export_waits_for_the_window_and_reads_the_live_layout() {
        let (lua, _) = engine();
        let err = lua.load("eitri.layout.export()").exec().unwrap_err().to_string();
        assert!(
            err.contains("eitri.layout.export") && err.contains("only once the window exists"),
            "{err}"
        );
        let (lua, store, live) = attached(Layout::initial(&[]).unwrap());
        assert_eq!(
            lua.load("return eitri.layout.export()").eval::<String>().unwrap(),
            crate::layout::export_lua(&live.borrow(), None),
        );
        let held = live.borrow_mut();
        let err = lua.load("eitri.layout.export()").exec().unwrap_err().to_string();
        assert!(
            err.contains("eitri.layout.export") && err.contains("being changed"),
            "{err}"
        );
        drop(held);
        let ws = Rc::new(RefCell::new(crate::layout::Workspaces::single()));
        store.borrow_mut().attach_workspaces(copied(&ws));
        let held = ws.borrow_mut();
        let err = lua.load("eitri.layout.export()").exec().unwrap_err().to_string();
        assert!(
            err.contains("eitri.layout.export") && err.contains("workspaces are being changed"),
            "{err}"
        );
        drop(held);
        assert_eq!(
            lua.load("return eitri.layout.export()").eval::<String>().unwrap(),
            crate::layout::export_lua(&live.borrow(), Some(&ws.borrow())),
        );
    }

    fn default_of(source: &str) -> Result<Node, String> {
        default_tree_of(source).map(|tree| tree.tiled)
    }

    fn default_tree_of(source: &str) -> Result<LuaTree, String> {
        let (lua, store) = engine();
        let _ = lua.load(source).exec();
        let result = store.borrow().default_tree().cloned().expect("default was called");
        result
    }

    fn leaf(id: ModuleId) -> Node {
        Node::Leaf(id)
    }

    /// The editor takes share 0.6 beside a column with the agent above the canvas.
    #[test]
    fn the_specs_example_is_a_tree() {
        let tree = default_of("eitri.layout.default{ 'row', {'editor', 0.6}, {'column', {'agent'}, {'canvas', 0.5}} }")
            .unwrap();
        assert_eq!(
            tree,
            Node::split(
                Axis::Row,
                0.6,
                leaf(ModuleId::editor()),
                Node::split(
                    Axis::Column,
                    0.5,
                    leaf(ModuleId::agent()),
                    leaf(ModuleId::parse("canvas").unwrap())
                )
            )
        );
    }

    /// A bottom terminal in an n-ary root `column` is pinned when defaults are reconciled, just
    /// like the two-level spelling, so it retains its height when the window grows.
    #[test]
    fn an_n_ary_columns_bottom_terminal_is_pinned_as_a_first_launch_pins_it() {
        let pinned = |source: &str| {
            let tree = default_of(source).unwrap();
            let r = crate::layout::reconcile_default(tree, &[]).unwrap();
            // Pinned, the terminal keeps the height it settled at when the window grows.
            let mut layout = r.layout;
            let height_at = |layout: &Layout, h: i32| {
                crate::layout::arrange(
                    layout,
                    &crate::layout::Frame::new(crate::layout::Size { w: 1280, h }, 1),
                )
                .rect_of(&ModuleId::terminal())
                .unwrap()
                .h
            };
            crate::layout::settle_pins(
                &mut layout,
                &crate::layout::Frame::new(crate::layout::Size { w: 1280, h: 721 }, 1),
            );
            height_at(&layout, 721) == height_at(&layout, 1041)
        };
        assert!(pinned(
            "eitri.layout.default{ 'column', {'editor'}, {'agent'}, {'terminal'} }"
        ));
        assert!(pinned(
            "eitri.layout.default{ 'column', {'row', {'editor'}, {'agent'}, share = 0.7}, {'terminal'} }"
        ));
        assert!(
            !pinned("eitri.layout.default{ 'column', {'editor'}, {'terminal'}, {'agent'} }"),
            "a terminal in the middle is not a bottom row"
        );
    }

    /// Children without shares divide the remaining space equally in one container.
    #[test]
    fn children_without_a_share_divide_what_is_left_equally() {
        let tree = default_of("eitri.layout.default{ 'row', {'editor'}, {'agent'}, {'lua:notes'} }").unwrap();
        let outer = tree.as_con().expect("a container");
        let ratio = outer.children[0].share;
        assert!((ratio - 1.0 / 3.0).abs() < 1e-6, "{ratio}");
        assert_eq!(outer.children.len(), 3);
        assert!(outer.children.iter().all(|ch| (ch.share - 1.0 / 3.0).abs() < 1e-6));
        let tree =
            default_of("eitri.layout.default{ 'column', {'row', {'editor'}, {'agent'}, share = 0.7}, {'terminal'} }")
                .unwrap();
        let outer = tree.as_con().expect("a container");
        assert_eq!(
            (outer.layout.axis(), outer.children[0].share),
            (Some(Axis::Column), 0.7)
        );
    }

    /// Malformed layout tables must fail startup with a message naming the call and the error.
    #[test]
    fn a_malformed_tree_is_an_error_naming_the_call_and_the_problem() {
        let cases = [
            ("eitri.layout.default('row')", "expected a table, got string"),
            (
                "eitri.layout.default{ 'row', {'editor'} }",
                "a row needs two or more items",
            ),
            (
                "eitri.layout.default{ 'rows', {'editor'}, {'agent'} }",
                "unknown module 'rows'",
            ),
            (
                "eitri.layout.default{ 'row', {'editor', 1.2}, {'agent'} }",
                "item 1 of the row gets 1.200; a share is 0.05 to 1",
            ),
            (
                "eitri.layout.default{ 'row', {'editor', 0.7}, {'agent', 0.5} }",
                "add up to 1.2",
            ),
            (
                "eitri.layout.default{ 'row', {'editor', 0.6}, {'agent', 0.4}, {'terminal'} }",
                "leaving nothing",
            ),
            (
                "eitri.layout.default{ 'row', {'editor', 0.97}, {'agent', 0.03} }",
                "a share is 0.05 to 1",
            ),
            (
                "eitri.layout.default{ 'row', {'editor'}, {'editor'} }",
                "'editor' appears twice",
            ),
            ("eitri.layout.default{ 'row', {'editor'}, {'terminal'} }", "no 'agent'"),
            (
                "eitri.layout.default{ 'row', {'editor', 0.5, 'x'}, {'agent'} }",
                "{'editor'} or {'editor', share}",
            ),
            (
                "eitri.layout.default{ 'row', {'editor'}, 'agent' }",
                "expected a table, got string",
            ),
            (
                "eitri.layout.default{ 'row', {'editor', 'wide'}, {'agent'} }",
                "a share is a number",
            ),
            (
                "eitri.layout.default{ 'row', {'editor', share = 0.6}, {'agent'} }",
                "a module's share is its second item, {'editor', 0.6}",
            ),
            (
                "eitri.layout.default{ 'row', {'editor'}, {'agent'}, shares = 0.7 }",
                "unknown field shares",
            ),
        ];
        for (source, why) in cases {
            let err = default_of(source).unwrap_err();
            assert!(err.starts_with("eitri.layout.default: "), "{source}: {err}");
            assert!(err.contains(why), "{source}: expected {why:?} in {err:?}");
        }
    }

    /// The error reaches Lua as well, so the rest of `init.lua` does not run on a broken layout.
    #[test]
    fn a_malformed_tree_raises_in_lua_too() {
        let (lua, _) = engine();
        let err = lua.load("eitri.layout.default{ 'row' }").exec().unwrap_err();
        assert!(err.to_string().contains("eitri.layout.default"), "{err}");
    }

    #[test]
    fn requests_wait_for_the_window_then_queue_in_order() {
        let (lua, store) = engine();
        let err = lua.load("eitri.layout.hide('agent')").exec().unwrap_err();
        assert!(err.to_string().contains("only once the window exists"), "{err}");
        assert!(store.borrow_mut().take_requests().is_empty());

        store.borrow_mut().accept_requests();
        lua.load(
            "eitri.layout.hide('agent'); eitri.layout.show('terminal'); \
             eitri.layout.focus('lua:notes'); eitri.layout.split('agent', 'below')",
        )
        .exec()
        .unwrap();
        assert_eq!(
            store.borrow_mut().take_requests(),
            [
                LayoutRequest::Hide(ModuleId::agent()),
                LayoutRequest::Show(ModuleId::terminal()),
                LayoutRequest::Focus(ModuleId::lua("notes")),
                LayoutRequest::Split(ModuleId::agent(), Axis::Column),
            ]
        );
        assert!(store.borrow_mut().take_requests().is_empty(), "taken once");

        for (source, why) in [
            ("eitri.layout.show('chat')", "unknown module 'chat'"),
            ("eitri.layout.split('agent', 'left')", "not 'right' or 'below'"),
            (
                "eitri.layout.default{ 'row', {'editor'}, {'agent'} }",
                "only in init.lua",
            ),
        ] {
            let err = lua.load(source).exec().unwrap_err();
            assert!(err.to_string().contains(why), "{source}: {err}");
        }
    }

    #[test]
    fn instance_ids_are_part_of_the_grammar() {
        let tree = default_of("eitri.layout.default{ 'row', {'editor'}, {'agent'}, {'terminal#2'} }").unwrap();
        let layout = crate::layout::reconcile_default(tree, &[]).unwrap().layout;
        let id = ModuleId::parse("terminal#2").unwrap();
        assert!(layout.contains(&id));
        let (lua, _, _) = attached(layout);
        assert!(lua.load("return eitri.layout.get()[4][1]").eval::<String>().unwrap() == "terminal#2");
        let error = default_of("eitri.layout.default{ 'row', {'editor'}, {'agent'}, {'terminal#1'} }").unwrap_err();
        assert!(error.contains("instance 1 is 'terminal'"), "{error}");
    }

    #[test]
    fn the_grammar_says_floating_at_the_top_of_the_tree() {
        let (lua, store) = engine();
        lua.load(
            "eitri.layout.default{ 'row', {'editor'}, {'agent'}, floating = { \
         {'terminal#2', cx = 0.5, cy = 0.45, w = 900, h = 420, scratch = true}, \
         {'terminal', cx = 0.2, cy = 0.8, w = 300, h = 200} } }",
        )
        .exec()
        .unwrap();
        let tree = store.borrow().default_tree().cloned().unwrap().unwrap();
        assert_eq!(tree.tiled.leaves(), vec![ModuleId::editor(), ModuleId::agent()]);
        assert_eq!(
            tree.floats,
            vec![
                Float {
                    id: ModuleId::parse("terminal#2").unwrap(),
                    rect: FloatRect {
                        cx: 0.5,
                        cy: 0.45,
                        w: 900,
                        h: 420
                    },
                    scratch: true
                },
                Float {
                    id: ModuleId::terminal(),
                    rect: FloatRect {
                        cx: 0.2,
                        cy: 0.8,
                        w: 300,
                        h: 200
                    },
                    scratch: false
                },
            ]
        );
    }

    #[test]
    fn a_malformed_float_is_an_error_naming_it() {
        for (field, message) in [
            ("floating = 3", "floating: expected a table of floats, got number"),
            ("floating = { 3 }", "floating item 1: expected a table, got number"),
            (
                "floating = { {cx = 0.5} }",
                "floating item 1: the first item must be a module id",
            ),
            (
                "floating = { {'chat', cx = 0.5, cy = 0.5, w = 10, h = 10} }",
                "floating item 1: unknown module 'chat'",
            ),
            (
                "floating = { {'editor', cx = 0.5, cy = 0.5, w = 10, h = 10} }",
                "floating item 1: the editor never floats",
            ),
            (
                "floating = { {'terminal', cx = 0.5, cy = 0.5, w = 10} }",
                "floating item 1 ('terminal'): cx, cy, w and h are required",
            ),
            (
                "floating = { {'terminal', cx = 1.5, cy = 0.5, w = 10, h = 10} }",
                "floating item 1 ('terminal'): cx is a number from 0 to 1",
            ),
            (
                "floating = { {'terminal', cx = 0.5, cy = 0.5, w = 0.5, h = 10} }",
                "floating item 1 ('terminal'): w is a whole number of pixels from 1 to 16384",
            ),
            (
                "floating = { {'terminal', cx = 0.5, cy = 0.5, w = 10, h = 99999} }",
                "floating item 1 ('terminal'): h is a whole number of pixels from 1 to 16384",
            ),
            (
                "floating = { {'terminal', cx = 0.5, cy = 0.5, w = 10, h = 10, scratch = 1} }",
                "floating item 1 ('terminal'): scratch is true or false",
            ),
            (
                "floating = { {'terminal', cx = 0.5, cy = 0.5, w = 10, h = 10, size = 3} }",
                "floating item 1 ('terminal'): unknown field size",
            ),
            (
                "floating = { {'terminal', 0.5, cx = 0.5, cy = 0.5, w = 10, h = 10} }",
                "floating item 1 ('terminal'): a float is {'terminal', cx = .., cy = .., w = .., h = ..}",
            ),
            (
                "floating = { {'agent', cx = 0.5, cy = 0.5, w = 10, h = 10} }",
                "'agent' appears twice",
            ),
        ] {
            let source = format!("eitri.layout.default{{ 'row', {{'editor'}}, {{'agent'}}, {field} }}");
            let err = default_of(&source).unwrap_err();
            assert!(
                err.starts_with("eitri.layout.default: ") && err.contains(message),
                "{source}: {err}"
            );
        }
        let nested = default_of(
            "eitri.layout.default{ 'row', {'editor'}, {'column', {'agent'}, {'terminal'}, floating = {} } }",
        )
        .unwrap_err();
        assert!(nested.contains("unknown field floating"), "{nested}");
    }

    #[test]
    fn get_writes_the_floats_and_reading_them_back_gives_the_same_arrangement() {
        use crate::layout::{arrange, reconcile_default_floating, Frame, Size};
        let t2 = ModuleId::parse("terminal#2").unwrap();
        let mut layout = Layout::initial(&[]).unwrap();
        layout
            .add_float(
                &t2,
                FloatRect {
                    cx: 0.5,
                    cy: 0.45,
                    w: 900,
                    h: 420,
                },
                true,
            )
            .unwrap();
        let (lua, _, live) = attached(layout);
        let got: String = lua
            .load(
                "local f = eitri.layout.get().floating[1]; \
             return string.format('%s %.2f %.2f %d %d %s', f[1], f.cx, f.cy, f.w, f.h, tostring(f.scratch))",
            )
            .eval()
            .unwrap();
        assert_eq!(got, "terminal#2 0.50 0.45 900 420 true");
        let table: Value = lua
            .load(
                "local t = eitri.layout.get(); t.focus = nil; t.hidden = nil; t.away = nil; t.zoomed = nil; t.marked = nil; return t",
            )
            .eval()
            .unwrap();
        let tree = parse_tree(&table).unwrap();
        let back = reconcile_default_floating(tree.tiled, tree.floats, &[]).unwrap().layout;
        for size in [
            Size { w: 1280, h: 721 },
            Size { w: 1920, h: 1040 },
            Size { w: 800, h: 600 },
        ] {
            let frame = Frame::new(size, 1);
            assert_eq!(
                arrange(&back, &frame).floats,
                arrange(&live.borrow(), &frame).floats,
                "{size:?}"
            );
            assert_eq!(
                arrange(&back, &frame).modules,
                arrange(&live.borrow(), &frame).modules,
                "{size:?}"
            );
        }
        let (empty, _, _) = attached(Layout::initial(&[]).unwrap());
        let none: Value = empty.load("return eitri.layout.get().floating").eval().unwrap();
        assert!(none.is_nil(), "no floating key without a float");
    }

    #[test]
    fn float_queues_a_module_and_an_optional_rectangle() {
        let (lua, store, _) = attached(Layout::initial(&[]).unwrap());
        lua.load("eitri.layout.float('terminal'); eitri.layout.float('terminal#2', { cx = 0.5, cy = 0.4, w = 600, h = 300 })")
        .exec()
        .unwrap();
        assert_eq!(
            store.borrow_mut().take_requests(),
            [
                LayoutRequest::Float(ModuleId::terminal(), None),
                LayoutRequest::Float(
                    ModuleId::parse("terminal#2").unwrap(),
                    Some(FloatRect {
                        cx: 0.5,
                        cy: 0.4,
                        w: 600,
                        h: 300
                    })
                ),
            ]
        );
        for (source, why) in [
            ("eitri.layout.float('editor')", "the editor never floats"),
            (
                "eitri.layout.float('terminal', { cx = 0.5 })",
                "cx, cy, w and h are required",
            ),
            ("eitri.layout.float('terminal', 3)", "expected a table, got number"),
        ] {
            let err = lua.load(source).exec().unwrap_err().to_string();
            assert!(
                err.contains("eitri.layout.float") && err.contains(why),
                "{source}: {err}"
            );
        }
    }

    #[test]
    fn the_chat_may_float_and_leave_the_tiled_tree() {
        let tree = default_tree_of(
            "eitri.layout.default{ 'row', {'editor'}, {'terminal'}, floating = { \
         {'agent', cx = 0.5, cy = 0.5, w = 600, h = 400} } }",
        )
        .unwrap();
        assert_eq!(tree.tiled.leaves(), vec![ModuleId::editor(), ModuleId::terminal()]);
        assert_eq!(tree.floats[0].id, ModuleId::agent());
        let r = crate::layout::reconcile_default_floating(tree.tiled, tree.floats, &[]).unwrap();
        assert!(r.layout.is_floating(&ModuleId::agent()));
    }

    #[test]
    fn a_workspace_named_by_a_non_string_or_non_utf8_value_stops_startup() {
        let (lua, store) = engine();
        lua.load("assert(not pcall(eitri.layout.workspace, 42, {'editor'}))")
            .exec()
            .unwrap();
        lua.load(r"assert(not pcall(eitri.layout.workspace, '\255', {'editor'}))")
            .exec()
            .unwrap();
        let store = store.borrow();
        let defined = store.defined_workspaces();
        assert_eq!(defined.len(), 2);
        let errors: Vec<_> = defined.iter().map(|(_, tree)| tree.clone().unwrap_err()).collect();
        assert!(errors[0].contains("must be a string, not a"), "{errors:?}");
        assert!(errors[1].contains("must be UTF-8"), "{errors:?}");
    }

    #[test]
    fn a_rule_target_that_is_not_utf8_stops_startup() {
        let (lua, store) = engine();
        lua.load(r"assert(not pcall(eitri.layout.rule, '\255', {at = 'side'}))")
            .exec()
            .unwrap();
        let error = store.borrow().rules().map(|_| ()).unwrap_err();
        assert!(error.contains("must be UTF-8"), "{error}");
    }

    #[test]
    fn a_left_out_share_at_the_minimum_is_computed_in_double_precision() {
        let lua = Lua::new();
        let value = lua
            .load(r#"return { "row", { "editor", 0.6 }, { "agent", 0.2 }, { "terminal", 0.15 }, { "terminal#2" } }"#)
            .eval::<Value>()
            .unwrap();
        let Node::Con(row) = parse_default(&value).unwrap() else {
            panic!("a row parses as a container");
        };
        row.check(true).unwrap();
        assert!(row.children[3].share < MIN_RATIO, "{}", row.children[3].share);
    }

    #[test]
    fn lua_get_reports_the_window_and_the_away_ids() {
        let t2 = ModuleId::parse("terminal#2").unwrap();
        let root = Node::split(
            crate::layout::Axis::Column,
            0.6,
            Node::split(
                crate::layout::Axis::Row,
                0.6,
                Node::Leaf(ModuleId::editor()),
                Node::Leaf(ModuleId::agent()),
            ),
            Node::split(
                crate::layout::Axis::Row,
                0.5,
                Node::Leaf(ModuleId::terminal()),
                Node::Leaf(t2.clone()),
            ),
        );
        let mut layout = Layout::from_parts(root, BTreeSet::from([ModuleId::terminal()]), ModuleId::editor()).unwrap();
        layout.set_away(&t2, true).unwrap();
        let (lua, store, live) = attached(layout);
        let read = |lua: &Lua| -> (Option<String>, Vec<String>, Vec<String>) {
            lua.load("local t = eitri.layout.get(); return t.window, t.away, t.hidden")
                .eval::<(Option<String>, Vec<String>, Vec<String>)>()
                .unwrap()
        };
        assert_eq!(
            read(&lua),
            (None, vec!["terminal#2".to_string()], vec!["terminal".to_string()]),
            "a window with no pair is neither; an away module is listed once, as away"
        );
        for (role, name) in [
            (crate::window_link::pair::Role::Main, Some("1")),
            (crate::window_link::pair::Role::Second, Some("2")),
            (crate::window_link::pair::Role::Unpaired, None),
        ] {
            store.borrow_mut().set_window(window_name(role));
            assert_eq!(read(&lua).0.as_deref(), name, "{role:?}");
        }
        live.borrow_mut().set_away(&t2, false).unwrap();
        assert_eq!(read(&lua).1, Vec::<String>::new(), "back, it is no longer away");
    }
}
