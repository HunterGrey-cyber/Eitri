//! `eitri.layout.*`: the Lua half of the layout.
//!
//! - `eitri.layout.default{ ... }`, in `init.lua`: the first window's tree, used whenever no saved
//!   layout can be (a first launch, or a state file that cannot be used). A malformed table is a
//!   startup failure naming the call: [`LayoutStore::default_tree`] holds the error for `shell`, which
//!   exits naming it once `init.lua` has run -- the discipline `agent.font_size` follows, since
//!   `init.lua`'s own errors are only logged.
//! - `eitri.layout.show(id)`, `.hide(id)`, `.focus(id)`, `.split(id, 'right'|'below')`,
//!   `.run(name, opts?)` and `.mark(id)`, for use
//!   inside `eitri.command.register` commands and event handlers: each queues a
//!   [`LayoutRequest`] that `shell` carries out when the handler returns. Before the window exists
//!   they refuse, naming `default` as the way to shape the first window.
//!
//! - `eitri.layout.get()` reads a fresh table of the live layout once the window exists.
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
//! The tree must hold `editor` and `agent`; anything else this window
//! does not have is left out when it is reconciled (`layout::reconcile_default`),
//! with a log line.
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
use crate::layout::tree::MIN_RATIO;
use crate::layout::{Axis, Child, Con, ConLayout, Layout, ModuleId, Node};

/// One queued `eitri.layout` call, for `shell` to carry out.
#[derive(Debug, Clone, PartialEq)]
pub enum LayoutRequest {
    Show(ModuleId),
    Hide(ModuleId),
    Focus(ModuleId),
    /// `Axis::Row` is `'right'`, `Axis::Column` is `'below'`, of the module with the keys.
    Split(ModuleId, Axis),
    /// A key action by its `eitri.keymap` name, run on the module with the keys;
    /// `shell` runs it exactly as the prefix does.
    Run(Action),
    Mark(ModuleId),
}

/// What `eitri.layout.*` recorded.
#[derive(Debug, Default)]
pub struct LayoutStore {
    default: Option<Result<Node, String>>,
    requests: Vec<LayoutRequest>,
    accepting: bool,
    lua_panels: Vec<String>,
    layout: Option<Rc<RefCell<Layout>>>,
}

impl LayoutStore {
    /// The window's live layout, so `get` reads its current state.
    pub fn attach(&mut self, layout: Rc<RefCell<Layout>>) {
        self.layout = Some(layout);
    }

    /// The registered panel ids that `module.<id>` may name.
    pub fn set_lua_panels(&mut self, ids: Vec<String>) {
        self.lua_panels = ids;
    }

    /// `eitri.layout.default`'s tree, or why it is not one. `None` if `init.lua` never called it.
    pub fn default_tree(&self) -> Option<&Result<Node, String>> {
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
    let table = lua.create_table()?;

    let default_store = store.clone();
    table.set(
        "default",
        lua.create_function(move |_, tree: Value| {
            if default_store.borrow().accepting {
                return Err(mlua::Error::RuntimeError(
                    "eitri.layout.default: only in init.lua, before the window exists".to_string(),
                ));
            }
            let parsed = parse_default(&tree).map_err(|e| format!("eitri.layout.default: {e}"));
            let mut store = default_store.borrow_mut();
            if store.default.is_some() {
                eprintln!("[lua] eitri.layout.default called again -- the later call replaces the earlier");
            }
            store.default = Some(parsed.clone());
            parsed.map(|_| ()).map_err(mlua::Error::RuntimeError)
        })?,
    )?;

    for name in ["show", "hide", "focus"] {
        let store = store.clone();
        table.set(
            name,
            lua.create_function(move |_, id: String| {
                let id = module_arg(name, &id)?;
                let request = match name {
                    "show" => LayoutRequest::Show(id),
                    "hide" => LayoutRequest::Hide(id),
                    _ => LayoutRequest::Focus(id),
                };
                queue(&store, name, request)
            })?,
        )?;
    }

    let split_store = store.clone();
    table.set(
        "split",
        lua.create_function(move |_, (id, direction): (String, String)| {
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
        })?,
    )?;

    let run_store = store.clone();
    table.set(
        "run",
        lua.create_function(move |lua, (name, options): (Value, Value)| {
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
            if !action.is_layout() {
                return Err(mlua::Error::RuntimeError(format!(
                    "eitri.layout.run: {name} is not a layout action"
                )));
            }
            queue(&run_store, "run", LayoutRequest::Run(action))
        })?,
    )?;

    let mark_store = store.clone();
    table.set(
        "mark",
        lua.create_function(move |_, id: String| {
            let id = module_arg("mark", &id)?;
            queue(&mark_store, "mark", LayoutRequest::Mark(id))
        })?,
    )?;

    table.set(
        "get",
        lua.create_function(move |lua, ()| {
            let store = store.borrow();
            let live = store.layout.as_ref().filter(|_| store.accepting).ok_or_else(|| {
                mlua::Error::RuntimeError(
                    "eitri.layout.get: only once the window exists (in a command or an event handler)".to_string(),
                )
            })?;
            let layout = live.try_borrow().map_err(|_| {
                mlua::Error::RuntimeError(
                    "eitri.layout.get: the layout is being changed; call it from a command or an event handler"
                        .to_string(),
                )
            })?;
            let table = match layout.root().children.as_slice() {
                [only] => node_table(lua, &only.node, None)?,
                _ => con_table(lua, layout.root(), None)?,
            };
            table.set("focus", layout.focus().as_str())?;
            table.set(
                "hidden",
                lua.create_sequence_from(layout.hidden().iter().map(ModuleId::as_str))?,
            )?;
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
    table.raw_set(
        1,
        match c.layout {
            ConLayout::SplitH => "row",
            ConLayout::SplitV => "column",
            ConLayout::Tabbed => "tabbed",
            ConLayout::Stacked => "stacked",
        },
    )?;
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

/// `eitri.layout.default`'s argument as a tree (the module doc's grammar).
pub fn parse_default(value: &Value) -> Result<Node, String> {
    let Value::Table(table) = value else {
        return Err(format!("expected a table, got {}", value.type_name()));
    };
    let (node, share) = parse_node(table, "the tree")?;
    if share.is_some() {
        return Err("the whole tree takes no share".to_string());
    }
    let leaves = node.leaves();
    let mut seen = BTreeSet::new();
    for id in &leaves {
        if !seen.insert(id.clone()) {
            return Err(format!("'{id}' appears twice"));
        }
    }
    for built_in in [ModuleId::editor(), ModuleId::agent()] {
        if !seen.contains(&built_in) {
            return Err(format!("the tree has no '{built_in}', which every window has"));
        }
    }
    Ok(node)
}

/// A node and the share of its parent it asked for.
fn parse_node(table: &Table, at: &str) -> Result<(Node, Option<f32>), String> {
    let head: Value = table.raw_get(1).map_err(|e| e.to_string())?;
    let Value::String(head) = head else {
        return Err(format!(
            "{at}: the first item must be 'row', 'column', 'tabbed', 'stacked' or a module id"
        ));
    };
    let head = head.to_str().map_err(|e| e.to_string())?.to_string();
    let len = table.raw_len();
    let layout = match head.as_str() {
        "row" => Some(ConLayout::SplitH),
        "column" => Some(ConLayout::SplitV),
        "tabbed" => Some(ConLayout::Tabbed),
        "stacked" => Some(ConLayout::Stacked),
        _ => None,
    };
    refuse_other_fields(table, len, layout.is_some(), &head, at)?;
    let Some(layout) = layout else {
        let id = ModuleId::parse(&head).map_err(|e| format!("{at}: {e}"))?;
        let share = match len {
            1 => None,
            2 => Some(share_of(
                table.raw_get(2).map_err(|e| e.to_string())?,
                &format!("{at} ('{id}')"),
            )?),
            _ => return Err(format!("{at}: a module is {{'{id}'}} or {{'{id}', share}}")),
        };
        return Ok((Node::Leaf(id), share));
    };
    let children: Vec<(Node, Option<f32>)> = (2..=len)
        .map(|i| match table.raw_get::<Value>(i) {
            Ok(Value::Table(child)) => parse_node(&child, &format!("item {} of the {head}", i - 1)),
            Ok(other) => Err(format!(
                "item {} of the {head}: expected a table, got {}",
                i - 1,
                other.type_name()
            )),
            Err(e) => Err(e.to_string()),
        })
        .collect::<Result<_, _>>()?;
    let tabbed = layout.axis().is_none();
    if children.len() < if tabbed { 1 } else { 2 } {
        let count = if tabbed { "one" } else { "two" };
        return Err(format!("{at}: a {head} needs {count} or more items"));
    }
    let active = if tabbed {
        match table.raw_get::<Value>("active").map_err(|e| e.to_string())? {
            Value::Nil => 1,
            Value::Integer(n) if n >= 1 && n as usize <= children.len() => n as usize,
            Value::Number(n) if n.fract() == 0.0 && n >= 1.0 && n <= children.len() as f64 => n as usize,
            value => {
                let n = match value {
                    Value::Integer(n) => n.to_string(),
                    Value::Number(n) => n.to_string(),
                    other => format!("{other:?}"),
                };
                return Err(format!("{at}: active = {n}: the {head} has {} items", children.len()));
            }
        }
    } else {
        1
    };
    let share = match table.raw_get::<Value>("share").map_err(|e| e.to_string())? {
        Value::Nil => None,
        other => Some(share_of(other, &format!("{at}'s share"))?),
    };
    let mut c = container(layout, children, &head)?;
    c.focus = active - 1;
    Ok((Node::Con(c), share))
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

fn share_of(value: Value, at: &str) -> Result<f32, String> {
    let share = match value {
        Value::Number(n) => n as f32,
        Value::Integer(n) => n as f32,
        other => return Err(format!("{at}: a share is a number, got {}", other.type_name())),
    };
    Ok(share)
}

/// Each child keeps its share directly, so parsing never introduces another container.
fn container(layout: ConLayout, children: Vec<(Node, Option<f32>)>, head: &str) -> Result<Con, String> {
    for (i, (_, share)) in children.iter().enumerate() {
        if let Some(share) = share {
            if !(MIN_RATIO..=1.0).contains(share) {
                return Err(format!(
                    "item {} of the {head} gets {share:.3}; a share is 0.05 to 1",
                    i + 1
                ));
            }
        }
    }
    let given: f32 = children.iter().filter_map(|(_, s)| *s).sum();
    let unset = children.iter().filter(|(_, s)| s.is_none()).count();
    if unset == 0 && (given - 1.0).abs() > 0.001 {
        return Err(format!("the {head}'s shares add up to {given}, not 1"));
    }
    if unset > 0 && given >= 1.0 {
        return Err(format!(
            "the {head}'s shares add up to {given}, leaving nothing for the rest"
        ));
    }
    let each = if unset > 0 { (1.0 - given) / unset as f32 } else { 0.0 };
    let shares: Vec<f32> = children.iter().map(|(_, s)| s.unwrap_or(each)).collect();
    for (i, share) in shares.iter().enumerate() {
        if !(MIN_RATIO..=1.0).contains(share) {
            return Err(format!(
                "item {} of the {head} gets {share:.3}; a share is 0.05 to 1",
                i + 1
            ));
        }
    }
    Ok(Con::new(
        layout,
        children
            .into_iter()
            .zip(shares)
            .map(|((node, _), share)| Child::new(node, share))
            .collect(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keymap::Action;
    use crate::layout::{Direction, Layout};

    fn engine() -> (Lua, Rc<RefCell<LayoutStore>>) {
        let lua = Lua::new();
        let eitri = lua.create_table().unwrap();
        let store = Rc::new(RefCell::new(LayoutStore::default()));
        install(&lua, &eitri, store.clone()).unwrap();
        lua.globals().set("eitri", eitri).unwrap();
        (lua, store)
    }

    fn attached(layout: Layout) -> (Lua, Rc<RefCell<LayoutStore>>, Rc<RefCell<Layout>>) {
        let (lua, store) = engine();
        let live = Rc::new(RefCell::new(layout));
        store.borrow_mut().attach(live.clone());
        store.borrow_mut().set_lua_panels(vec!["notes".to_string()]);
        store.borrow_mut().accept_requests();
        (lua, store, live)
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
                "local t = eitri.layout.get(); t.focus = nil; t.hidden = nil; t.zoomed = nil; t.marked = nil; return t",
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
                for key in ["focus", "hidden", "zoomed", "marked"] {
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

    fn default_of(source: &str) -> Result<Node, String> {
        let (lua, store) = engine();
        let _ = lua.load(source).exec();
        let result = store.borrow().default_tree().cloned().expect("default was called");
        result
    }

    fn leaf(id: ModuleId) -> Node {
        Node::Leaf(id)
    }

    /// The editor takes a 0.6 share beside a column with the agent above the canvas.
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

    /// A bottom row keeps its height, spelled the n-ary way: the terminal ending a root
    /// `column` of three is pinned once the default is reconciled, as the two-level spelling's is
    ///.
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

    /// A malformed layout table produces an error naming the call and the problem.
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
}
