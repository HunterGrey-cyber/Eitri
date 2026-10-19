//! `eitri.layout.*`: the Lua half of the layout.
//!
//! `eitri.layout.default{ ... }` in `init.lua` supplies the first window's tree when no saved
//! layout is usable. A malformed table is a startup failure: [`LayoutStore::default_tree`]
//! holds the error for `shell` to report after `init.lua` runs, since Lua errors are only logged.
//! `agent.font_size` uses the same error handling.
//!
//! `eitri.layout.show(id)`, `.hide(id)`, `.focus(id)`, `.split(id, 'right'|'below')`,
//! `.run(name, opts?)` and `.mark(id)` queue a [`LayoutRequest`] from a command or event handler.
//! `shell` applies it after the handler returns. Before a window exists, these calls refuse
//! and name `default` as the way to shape the first window. `eitri.layout.get()` returns a
//! fresh table of the live layout once the window exists. For layout actions, `run` takes the
//! same names and options as `eitri.keymap.set`, so keys and Lua calls reach the same verbs.
//!
//! A tree looks like `{ 'row', {'editor', 0.6}, {'column', {'agent'}, {'canvas', 0.5}} }`.
//! A node starts with `'row'` or `'column'` followed by at least two children, or with a module
//! id and an optional second item giving its share. Splits take their share as `share = x`.
//! Shares are inside `(0, 1)`. Unspecified children divide the remainder equally; explicit
//! shares must leave them room, or sum to 1 if all are given. Other fields are refused so a
//! typo cannot silently do nothing. A module's share is its second item, not `share = x`.
//! The tree must contain `editor` and `agent`. Modules unavailable in this window are removed
//! by `layout::reconcile_default`, with a log line.
//!
//! A terminal or Lua `bottom` panel that forms a full-width bottom row keeps its pinned
//! height, with a log line. This applies both to `{ 'column', {'row', ...}, {'terminal'} }`
//! and to `{ 'column', {'editor'}, {'agent'}, {'terminal'} }`. A panel in the middle of a
//! column or beside another module in a row divides space by its share.
//!
//! `show` ends a zoom if needed to expose its module, as `Ctrl+a e`/`a` do. Focusing the
//! zoomed module itself keeps the zoom.

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::rc::Rc;

use mlua::{Lua, Table, Value};

use crate::keymap::Action;
use crate::layout::tree::{MAX_RATIO, MIN_RATIO};
use crate::layout::{Axis, Layout, ModuleId, Node};

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
            if matches!(action, Action::Split(_)) {
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
            let table = node_table(lua, layout.root(), None)?;
            table.set("focus", layout.focus().as_str())?;
            table.set(
                "hidden",
                lua.create_sequence_from(layout.hidden().iter().map(ModuleId::as_str))?,
            )?;
            table.set("zoomed", layout.zoomed().map(ModuleId::as_str))?;
            table.set("marked", layout.mark().map(ModuleId::as_str))?;
            Ok(table)
        })?,
    )?;

    eitri.set("layout", table)?;
    Ok(())
}

fn node_table(lua: &Lua, node: &Node, share: Option<f32>) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    match node {
        Node::Leaf(id) => {
            table.raw_set(1, id.as_str())?;
            if let Some(share) = share {
                table.raw_set(2, f64::from(share))?;
            }
        }
        Node::Split {
            axis,
            ratio,
            first,
            second,
            ..
        } => {
            table.raw_set(
                1,
                match axis {
                    Axis::Row => "row",
                    Axis::Column => "column",
                },
            )?;
            table.raw_set(2, node_table(lua, first, Some(*ratio))?)?;
            table.raw_set(3, node_table(lua, second, None)?)?;
            if let Some(share) = share {
                table.raw_set("share", f64::from(share))?;
            }
        }
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
        return Err(format!("{at}: the first item must be 'row', 'column' or a module id"));
    };
    let head = head.to_str().map_err(|e| e.to_string())?.to_string();
    let len = table.raw_len();
    let axis = match head.as_str() {
        "row" => Some(Axis::Row),
        "column" => Some(Axis::Column),
        _ => None,
    };
    refuse_other_fields(table, len, axis.is_some(), &head, at)?;
    let Some(axis) = axis else {
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
    if children.len() < 2 {
        return Err(format!("{at}: a {head} needs two or more items"));
    }
    let share = match table.raw_get::<Value>("share").map_err(|e| e.to_string())? {
        Value::Nil => None,
        other => Some(share_of(other, &format!("{at}'s share"))?),
    };
    Ok((chain(axis, children, &head)?, share))
}

/// Everything in a node's table but its `len` items and, on a split, `share`: refused, naming it.
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
    if !(share > 0.0 && share < 1.0) {
        return Err(format!("{at}: a share is between 0 and 1, got {share}"));
    }
    Ok(share)
}

/// `children` as a right-leaning chain of splits along `axis`, each child at its share.
fn chain(axis: Axis, children: Vec<(Node, Option<f32>)>, head: &str) -> Result<Node, String> {
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
    let mut nodes: Vec<Node> = children.into_iter().map(|(n, _)| n).collect();
    let mut node = nodes.pop().expect("two or more children");
    for i in (0..nodes.len()).rev() {
        let rest: f32 = shares[i..].iter().sum();
        let ratio = shares[i] / rest;
        if !(MIN_RATIO..=MAX_RATIO).contains(&ratio) {
            return Err(format!(
                "item {} of the {head} would get {:.3} of what is left; a split gives each side 0.05 to 0.95",
                i + 1,
                ratio
            ));
        }
        node = Node::split(axis, ratio, nodes.pop().expect("one per index"), node);
    }
    Ok(node)
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
            let mut pins = Vec::new();
            fn collect(node: &Node, out: &mut Vec<(ModuleId, bool)>) {
                if let Node::Split { pin, first, second, .. } = node {
                    if let Node::Leaf(id) = second.as_ref() {
                        out.push((id.clone(), pin.is_some()));
                    }
                    collect(first, out);
                    collect(second, out);
                }
            }
            collect(r.layout.root(), &mut pins);
            pins.into_iter()
                .find(|(id, _)| *id == ModuleId::terminal())
                .is_some_and(|(_, p)| p)
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

    /// tmux's n cells: three children with no shares are a third each, as a chain of two splits.
    #[test]
    fn children_without_a_share_divide_what_is_left_equally() {
        let tree = default_of("eitri.layout.default{ 'row', {'editor'}, {'agent'}, {'lua:notes'} }").unwrap();
        let Node::Split { ratio, second, .. } = &tree else {
            panic!("a leaf")
        };
        assert!((ratio - 1.0 / 3.0).abs() < 1e-6, "{ratio}");
        let Node::Split { ratio, .. } = second.as_ref() else {
            panic!("a leaf")
        };
        assert!((ratio - 0.5).abs() < 1e-6, "{ratio}");
        let tree =
            default_of("eitri.layout.default{ 'column', {'row', {'editor'}, {'agent'}, share = 0.7}, {'terminal'} }")
                .unwrap();
        let Node::Split { axis, ratio, .. } = &tree else {
            panic!("a leaf")
        };
        assert_eq!((*axis, *ratio), (Axis::Column, 0.7));
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
                "between 0 and 1, got 1.2",
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
                "0.05 to 0.95",
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
}
