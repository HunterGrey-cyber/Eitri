//! `eitri.layout.*` (modules spec §4.4, §4.5): the Lua half of the layout.
//!
//! - `eitri.layout.default{ ... }`, in `init.lua`: the first window's tree, used whenever no saved
//!   layout can be (a first launch, or a state file that cannot be used). A malformed table is a
//!   startup failure naming the call: [`LayoutStore::default_tree`] holds the error for `shell`, which
//!   exits naming it once `init.lua` has run -- the discipline `agent.font_size` follows, since
//!   `init.lua`'s own errors are only logged.
//! - `eitri.layout.show(id)`, `.hide(id)`, `.focus(id)` and `.split(id, 'right'|'below')`, for use
//!   inside `eitri.command.register` commands and event handlers: each queues a
//!   [`LayoutRequest`] that `shell` carries out when the handler returns. Before the window exists
//!   they refuse, naming `default` as the way to shape the first window.
//!
//! **The grammar**, the spec's own example: `{ 'row', {'editor', 0.6}, {'column', {'agent'},
//! {'canvas', 0.5}} }`. A node is a table whose first item is either `'row'`/`'column'` -- a split,
//! whose remaining items are its children, two or more, like tmux's n cells -- or a module id, with
//! an optional second item: the share of its parent it takes. A split takes a share as `share = x`.
//! Children without a share divide what the others leave equally; shares are each inside `(0, 1)`
//! and must leave room for the rest (or, all given, add up to 1). Any other field is refused rather
//! than ignored -- `{'editor', share = 0.6}` (a module's share is its second item) or a split's
//! `shares = 0.7` -- since a typo that silently did nothing is the failure this call's hard-fail
//! discipline exists to prevent. The tree must hold `editor` and `agent`; anything else this window
//! does not have (the canvas before P3) is left out when it is reconciled (`layout::reconcile_default`),
//! with a log line.
//!
//! **A bottom row keeps its height** (the owner's decision), in either spelling: the terminal, or a
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

use crate::layout::tree::{MAX_RATIO, MIN_RATIO};
use crate::layout::{Axis, ModuleId, Node};

/// One `eitri.layout.show/hide/focus/split` call, for `shell` to carry out.
#[derive(Debug, Clone, PartialEq)]
pub enum LayoutRequest {
    Show(ModuleId),
    Hide(ModuleId),
    Focus(ModuleId),
    /// `Axis::Row` is `'right'`, `Axis::Column` is `'below'`, of the module with the keys.
    Split(ModuleId, Axis),
}

/// What `eitri.layout.*` recorded.
#[derive(Debug, Default)]
pub struct LayoutStore {
    default: Option<Result<Node, String>>,
    requests: Vec<LayoutRequest>,
    accepting: bool,
}

impl LayoutStore {
    /// `eitri.layout.default`'s tree, or why it is not one. `None` if `init.lua` never called it.
    pub fn default_tree(&self) -> Option<&Result<Node, String>> {
        self.default.as_ref()
    }

    /// The window exists: from now on `show`/`hide`/`focus`/`split` queue, and `default` refuses.
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

    let split_store = store;
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

    eitri.set("layout", table)?;
    Ok(())
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

    fn engine() -> (Lua, Rc<RefCell<LayoutStore>>) {
        let lua = Lua::new();
        let eitri = lua.create_table().unwrap();
        let store = Rc::new(RefCell::new(LayoutStore::default()));
        install(&lua, &eitri, store.clone()).unwrap();
        lua.globals().set("eitri", eitri).unwrap();
        (lua, store)
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

    /// Spec §4.4's own example: the editor at 0.6 beside a column of the agent over the canvas.
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

    /// The owner's "a bottom row keeps its height", spelled the n-ary way: the terminal ending a root
    /// `column` of three is pinned once the default is reconciled, as the two-level spelling's is
    /// (the whole-branch review's finding 5, which found it left to grow with the window).
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

    /// Spec §4.4: "A malformed table is a startup failure naming the call." Each message names it,
    /// and says what is wrong.
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
