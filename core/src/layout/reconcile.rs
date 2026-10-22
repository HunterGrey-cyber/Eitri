//! A layout from the state file or `init.lua`'s `eitri.layout.default`, rebuilt against the
//! modules this window actually has.
//!
//! What it does, in order:
//! 1. Filters floats: the editor stays tiled, unknown modules and tiled duplicates leave,
//!    repeated floats keep their first entry, and invalid rectangles are clamped with a note.
//! 2. Drops every leaf this window has no module for, such as a Lua panel no longer registered,
//!    and its sibling takes the space as a hidden module's does. A hidden editor left alone
//!    after its replacement panel disappears is shown again: otherwise the window would open
//!    without nvim until the editor's module key was pressed. The cost is that an editor hidden
//!    by hand whose only neighbour was a panel since removed comes back too; the note says so.
//!    Refuses a layout without the tiled editor or without the chat in either layer, since every
//!    window has both and the caller must fall back to the default if either is missing.
//! 3. **`init.lua`'s default only** ([`reconcile_default`]): pins a full-width bottom row a first
//!    launch would have pinned, such as a Lua bottom panel or an explicitly placed terminal.
//!    `init.lua` has no way to say "pin", and these rows should keep their useful height when
//!    the window grows. A file keeps its own pins: a row the user unpinned must reopen unpinned.
//! 4. Places missing terminals, instances and registered Lua panels as a first launch would,
//!    skipping modules already floating. The base terminal goes hidden below the editor's own
//!    leaf; registered Lua panels follow their declared positions.
//! 5. Keeps hidden state only for modules still present and float memory only for remaining
//!    modules other than the editor.
//! 6. Gives the keys to a shown saved focus, otherwise where a first launch gives them: the
//!    editor, else the first registered Lua panel placed in its place, else the first module
//!    in view. Tabbed and stacked containers keep their drawn child, so choosing focus never
//!    switches tabs. A Lua default has no saved focus; its first leaf need not be the editor:
//!    `{ 'row', {'agent', 0.4}, {'editor'} }` should still open with nvim taking the keys.
//!
//! Every adjustment produces a note so the caller can explain why the window did not reopen
//! exactly as it was left.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use super::con::{remove_child, ConLayout, Node, Pin};
use super::float::{Float, FloatRect};
use super::module::{declared_placement, ModuleDecl, ModuleId, ModuleKind, Placement};
use super::tree::{place_new, Layout, LayoutError};

/// Every window has an editor, an agent and a terminal.
const BUILT_INS: [fn() -> ModuleId; 3] = [ModuleId::editor, ModuleId::agent, ModuleId::terminal];

/// A reconciled layout, and what was changed to get it.
#[derive(Debug, Clone, PartialEq)]
pub struct Reconciled {
    pub layout: Layout,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ReconcileError {
    /// The tree has no editor, or no agent.
    MissingBuiltIn(ModuleId),
    /// Nothing would be on screen.
    NothingVisible,
    /// The tree breaks an invariant `Layout::from_parts` holds (a duplicate, a ratio out of range, a
    /// pinned length below zero).
    Layout(LayoutError),
}

impl fmt::Display for ReconcileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReconcileError::MissingBuiltIn(id) => write!(f, "the layout has no '{id}', which every window has"),
            ReconcileError::NothingVisible => write!(f, "the layout hides every module"),
            ReconcileError::Layout(err) => err.fmt(f),
        }
    }
}

impl std::error::Error for ReconcileError {}

/// A state file's `root`, `hidden` and `focus`, against the built-ins and the registered Lua panels
/// `lua` (in registration order). See the module doc for what changes.
pub fn reconcile(
    root: Node,
    hidden: &BTreeSet<ModuleId>,
    focus: Option<&ModuleId>,
    lua: &[ModuleDecl],
    instances: &[ModuleId],
) -> Result<Reconciled, ReconcileError> {
    reconcile_floating(root, Vec::new(), BTreeMap::new(), hidden, focus, lua, instances)
}

/// `init.lua`'s `eitri.layout.default` tree, which has nothing hidden and no focus: [`reconcile`],
/// and its bottom rows pinned as a first launch would.
pub fn reconcile_default(root: Node, lua: &[ModuleDecl]) -> Result<Reconciled, ReconcileError> {
    reconcile_default_floating(root, Vec::new(), lua)
}

/// A file's or Lua default's tree, floats and remembered rectangles against this window's modules.
pub fn reconcile_floating(
    root: Node,
    floats: Vec<Float>,
    memory: BTreeMap<ModuleId, FloatRect>,
    hidden: &BTreeSet<ModuleId>,
    focus: Option<&ModuleId>,
    lua: &[ModuleDecl],
    instances: &[ModuleId],
) -> Result<Reconciled, ReconcileError> {
    reconcile_with(root, floats, memory, hidden, focus, lua, instances, false)
}

/// Reconciles a Lua default or a template, keeping the instances -- terminals and chat views -- named
/// in its tree or floats.
pub fn reconcile_default_floating(
    root: Node,
    floats: Vec<Float>,
    lua: &[ModuleDecl],
) -> Result<Reconciled, ReconcileError> {
    reconcile_default_floating_with(root, floats, lua, &[])
}

/// [`reconcile_default_floating`] for a window some of whose modules are deferred (never placed by the window
/// itself): one the tree or its floats name is kept as an instance is, and one they do not name is not placed.
pub fn reconcile_default_floating_with(
    root: Node,
    floats: Vec<Float>,
    lua: &[ModuleDecl],
    deferred: &[ModuleId],
) -> Result<Reconciled, ReconcileError> {
    let mut instances: Vec<ModuleId> = Vec::new();
    for id in root
        .leaves()
        .into_iter()
        .chain(floats.iter().map(|float| float.id.clone()))
    {
        if (deferred.contains(&id) || id.kind().has_instances() && id.number() >= 2) && !instances.contains(&id) {
            instances.push(id);
        }
    }
    reconcile_with(
        root,
        floats,
        BTreeMap::new(),
        &BTreeSet::new(),
        None,
        lua,
        &instances,
        true,
    )
}

fn reconcile_with(
    root: Node,
    floats: Vec<Float>,
    memory: BTreeMap<ModuleId, FloatRect>,
    hidden: &BTreeSet<ModuleId>,
    focus: Option<&ModuleId>,
    lua: &[ModuleDecl],
    instances: &[ModuleId],
    pin: bool,
) -> Result<Reconciled, ReconcileError> {
    let known = |id: &ModuleId| {
        BUILT_INS.iter().any(|b| b() == *id)
            || lua.iter().any(|d| d.id == *id && d.id.kind() == ModuleKind::LuaWebview)
            || instances.contains(id)
    };
    let mut notes = Vec::new();
    let tiled = root.leaves();
    let mut floating = BTreeSet::new();
    let mut kept = Vec::new();
    for mut float in floats {
        let id = &float.id;
        if *id == ModuleId::editor() {
            notes.push("'editor' never floats; it stays tiled".into());
        } else if !known(id) {
            notes.push(format!("left out floating '{id}': this window has no such module"));
        } else if tiled.contains(id) {
            notes.push(format!("floating '{id}' is also tiled; it stays tiled"));
        } else if !floating.insert(id.clone()) {
            notes.push(format!("floating '{id}' appears twice; the first is kept"));
        } else {
            if !float.rect.is_valid() {
                notes.push(format!(
                    "floating '{id}': its rectangle was out of range and is clamped"
                ));
                float.rect = float.rect.clamped();
            }
            kept.push(float);
        }
    }
    let mut dropped = Vec::new();
    let mut uncovered = Vec::new();
    let root = prune(root, &known, &mut dropped, &mut uncovered);
    for id in &dropped {
        notes.push(format!("left out '{id}': this window has no such module"));
    }
    let mut root = root.ok_or(ReconcileError::MissingBuiltIn(ModuleId::editor()))?;
    for built_in in [ModuleId::editor(), ModuleId::agent()] {
        if !root.leaves().contains(&built_in) && (built_in == ModuleId::editor() || !floating.contains(&built_in)) {
            return Err(ReconcileError::MissingBuiltIn(built_in));
        }
    }
    if pin {
        let below_root = |id: &ModuleId| {
            *id == ModuleId::terminal()
                || lua.iter().any(|d| {
                    d.id == *id && d.id.kind() == ModuleKind::LuaWebview && d.placement == Placement::BelowRoot
                })
        };
        pin_bottom_rows(&mut root, &below_root, &mut notes);
    }
    let mut hidden: BTreeSet<ModuleId> = hidden
        .iter()
        .filter(|id| root.leaves().contains(id) || floating.contains(id))
        .cloned()
        .collect();
    let editor = ModuleId::editor();
    if uncovered.contains(&editor) && hidden.remove(&editor) {
        notes.push(format!(
            "showed '{editor}': the module that stood in its place (a Lua `main` panel) is left out, and \
             nothing else takes it"
        ));
    }
    let terminal = ModuleId::terminal();
    if !root.leaves().contains(&terminal) && !floating.contains(&terminal) {
        let placement = declared_placement(lua, &terminal, Placement::BelowEditor);
        root = place_new(root, &terminal, placement).0;
        hidden.insert(terminal.clone());
        notes.push(if placement == Placement::BelowEditor {
            format!("placed '{terminal}' below the editor, hidden, as a first launch does")
        } else {
            format!("placed '{terminal}' where its open rule says ({placement:?}), hidden")
        });
    }
    for id in instances {
        if !root.leaves().contains(id) && !floating.contains(id) {
            let placement = declared_placement(lua, id, Placement::BelowEditor);
            root = place_new(root, id, placement).0;
            hidden.insert(id.clone());
            notes.push(if placement == Placement::BelowEditor {
                format!("placed '{id}' below the editor, hidden: another workspace has it")
            } else {
                format!("placed '{id}' where its open rule says ({placement:?}), hidden: another workspace has it")
            });
        }
    }
    for decl in lua.iter().filter(|decl| decl.id.kind() == ModuleKind::LuaWebview) {
        if root.leaves().contains(&decl.id) || floating.contains(&decl.id) {
            continue;
        }
        let (placed, took_editors_place) = place_new(root, &decl.id, decl.placement);
        root = placed;
        if took_editors_place {
            hidden.insert(ModuleId::editor());
        }
        notes.push(format!(
            "placed '{}' where a first launch does ({:?})",
            decl.id, decl.placement
        ));
    }
    let shown: Vec<ModuleId> = root
        .leaves()
        .into_iter()
        .chain(kept.iter().map(|float| float.id.clone()))
        .filter(|id| !hidden.contains(id))
        .collect();
    let mut kept_memory = BTreeMap::new();
    for (id, rect) in memory {
        if id == ModuleId::editor() {
            notes.push("float memory of 'editor' left out: the editor never floats".into());
        } else if !root.leaves().contains(&id) && !floating.contains(&id) {
            notes.push(format!(
                "float memory of '{id}' left out: this window has no such module"
            ));
        } else {
            kept_memory.insert(id, rect.clamped());
        }
    }
    let focus = match focus {
        Some(f) if shown.contains(f) => f.clone(),
        _ => {
            let mut in_view = Vec::new();
            drawn_leaves(&root, &|id| !hidden.contains(id), &mut in_view);
            in_view.extend(
                kept.iter()
                    .filter(|float| !hidden.contains(&float.id))
                    .map(|float| float.id.clone()),
            );
            first_focus(&in_view, lua).ok_or(ReconcileError::NothingVisible)?
        }
    };
    let layout = Layout::from_parts_floating(root, kept, hidden, focus, kept_memory).map_err(ReconcileError::Layout)?;
    Ok(Reconciled { layout, notes })
}

/// The shown modules of `node` that its tab containers draw, in tree order: a tabbed or stacked
/// container contributes only its drawn child's, so the keys never start on a tab the tree does not
/// draw and giving them never switches the tab it chose.
fn drawn_leaves(node: &Node, shown: &dyn Fn(&ModuleId) -> bool, out: &mut Vec<ModuleId>) {
    match node {
        Node::Leaf(id) => {
            if shown(id) {
                out.push(id.clone());
            }
        }
        Node::Con(c) if c.layout.axis().is_none() => {
            if let Some(i) = c.drawn(shown) {
                drawn_leaves(&c.children[i].node, shown, out);
            }
        }
        Node::Con(c) => c.children.iter().for_each(|ch| drawn_leaves(&ch.node, shown, out)),
    }
}

/// When `focus` cannot be used: where `Layout::initial` gives the keys --
/// the editor, else the first registered Lua panel in the editor's place -- and otherwise the first
/// shown module in tree order. `None` only if nothing is shown.
pub(super) fn first_focus(shown: &[ModuleId], lua: &[ModuleDecl]) -> Option<ModuleId> {
    let editor = ModuleId::editor();
    if shown.contains(&editor) {
        return Some(editor);
    }
    lua.iter()
        .filter(|decl| decl.id.kind() == ModuleKind::LuaWebview && decl.placement == Placement::InPlaceOfEditor)
        .map(|decl| &decl.id)
        .find(|id| shown.contains(id))
        .or_else(|| shown.first())
        .cloned()
}

/// From `node` down: each `below_root` module that is a full-width row at
/// the bottom of the window gets the pin a first launch gives it ([`Placement::BelowRoot`]), with no
/// length yet -- so it divides by its share until it has been on screen, then keeps that height.
///
/// The last child of a vertical container on the first-child spine spans the window's bottom.
/// Only a below-root leaf there gets a pin; a module in the middle or beside another stays a share.
/// The walk goes on into the first child only below such a leaf: with anything else last, the
/// first child is not at the bottom, and a below-root module inside it stays a share. Existing pins
/// keep their lengths when the window grows.
fn pin_bottom_rows(node: &mut Node, below_root: &dyn Fn(&ModuleId) -> bool, notes: &mut Vec<String>) {
    let Node::Con(c) = node else {
        return;
    };
    if c.layout != ConLayout::SplitV || c.children.len() < 2 {
        return;
    }
    let unpinned = c.children.iter().all(|ch| ch.pin.is_none());
    let last = c.children.len() - 1;
    let Node::Leaf(id) = &c.children[last].node else {
        return;
    };
    if !below_root(id) {
        return;
    }
    if unpinned {
        notes.push(format!(
            "pinned '{id}' below everything, as a first launch does: it keeps its height when the window grows"
        ));
        c.children[last].pin = Some(Pin { px: None });
    }
    pin_bottom_rows(&mut c.children[0].node, below_root, notes);
}

/// `node` without the leaves `keep` refuses. A split left with one child is replaced by it;
/// tabbed and stacked containers keep their strip. Surviving shares scale back up to 1.
/// `None` if nothing is left. The refused ids go into `dropped`, in tree order; a leaf left
/// alone by refused siblings goes into `uncovered` so a hidden editor can be shown again.
fn prune(
    node: Node,
    keep: &dyn Fn(&ModuleId) -> bool,
    dropped: &mut Vec<ModuleId>,
    uncovered: &mut Vec<ModuleId>,
) -> Option<Node> {
    match node {
        Node::Leaf(id) => {
            if keep(&id) {
                Some(Node::Leaf(id))
            } else {
                dropped.push(id);
                None
            }
        }
        Node::Con(mut c) => {
            let before = c.children.len();
            let mut at = 0;
            while at < c.children.len() {
                let node = std::mem::replace(&mut c.children[at].node, Node::Leaf(ModuleId::editor()));
                if let Some(node) = prune(node, keep, dropped, uncovered) {
                    c.children[at].node = node;
                    at += 1;
                } else {
                    remove_child(&mut c, at);
                }
            }
            if c.children.len() == 1 && before > 1 {
                if let Node::Leaf(id) = &c.children[0].node {
                    uncovered.push(id.clone());
                }
            }
            match c.children.len() {
                0 => None,
                1 if c.layout.axis().is_some() => Some(c.children.pop().expect("one child").node),
                _ => Some(Node::Con(c)),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::same_rects_opened;
    use super::super::tree::{Axis, Branch, BELOW_ROOT_SHARE, DEFAULT_EDITOR_SHARE, RIGHT_OF_ROOT_SHARE};
    use super::super::{Child, Con};
    use super::*;

    fn editor() -> ModuleId {
        ModuleId::editor()
    }
    fn agent() -> ModuleId {
        ModuleId::agent()
    }
    fn term() -> ModuleId {
        ModuleId::terminal()
    }
    fn leaf(id: ModuleId) -> Node {
        Node::Leaf(id)
    }
    fn row(a: Node, b: Node) -> Node {
        Node::split(Axis::Row, 0.5, a, b)
    }
    fn set(ids: &[ModuleId]) -> BTreeSet<ModuleId> {
        ids.iter().cloned().collect()
    }
    /// `r`'s layout puts every module where `want` does, with `r`'s own hidden modules and keys: as
    /// it is, and with everything shown and every pin settled.
    fn same_as(r: &Reconciled, want: Node) {
        let want = Layout::from_parts(want, r.layout.hidden().clone(), r.layout.focus().clone()).unwrap();
        same_rects_opened(&r.layout, &want);
    }
    /// What a window with a terminal saves: `[editor | agent]` over the (pinned) terminal.
    fn saved() -> Node {
        Node::pinned(
            Axis::Column,
            BELOW_ROOT_SHARE,
            Branch::Second,
            row(leaf(editor()), leaf(agent())),
            leaf(term()),
        )
    }

    /// [`saved`] with a Lua `side` panel right of everything.
    fn saved_with_side() -> Node {
        row(saved(), leaf(ModuleId::lua("side")))
    }

    #[test]
    fn a_chat_views_declaration_never_pins_the_agent_of_a_lua_default() {
        fn row_pin(node: &Node, id: &ModuleId) -> Option<Pin> {
            let Node::Con(c) = node else { return None };
            c.children.iter().find_map(|child| {
                if child.node.leaves() == vec![id.clone()] {
                    child.pin
                } else {
                    row_pin(&child.node, id)
                }
            })
        }
        let decls = [ModuleDecl {
            id: ModuleId::agent(),
            placement: Placement::BelowRoot,
        }];
        for (beside, bottom, pinned) in [
            (ModuleId::terminal(), ModuleId::agent(), false),
            (ModuleId::agent(), ModuleId::terminal(), true),
        ] {
            let root = Node::split(
                Axis::Column,
                0.7,
                Node::split(Axis::Row, 0.6, Node::Leaf(ModuleId::editor()), Node::Leaf(beside)),
                Node::Leaf(bottom.clone()),
            );
            let layout = reconcile_default_floating(root, Vec::new(), &decls).unwrap().layout;
            assert_eq!(row_pin(&layout.root_node(), &bottom).is_some(), pinned, "{bottom}");
        }
    }

    #[test]
    fn a_default_tree_keeps_a_deferred_module_it_names_and_places_none_it_does_not() {
        let preview = ModuleId::lua("markdown");
        let deferred = [preview.clone()];
        let named = row(row(leaf(editor()), leaf(agent())), leaf(preview.clone()));
        let r = reconcile_default_floating_with(named.clone(), Vec::new(), &[], &deferred).unwrap();
        assert!(r.layout.contains(&preview), "{:?}", r.notes);
        assert!(!r.layout.hidden().contains(&preview));
        assert!(r.notes.iter().all(|n| !n.contains("lua:markdown")), "{:?}", r.notes);

        let unnamed = row(leaf(editor()), leaf(agent()));
        let r = reconcile_default_floating_with(unnamed, Vec::new(), &[], &deferred).unwrap();
        assert!(!r.layout.contains(&preview));
        assert!(r.notes.iter().all(|n| !n.contains("lua:markdown")), "{:?}", r.notes);

        let r = reconcile_default_floating(named, Vec::new(), &[]).unwrap();
        assert!(!r.layout.contains(&preview));
        assert!(
            r.notes
                .contains(&"left out 'lua:markdown': this window has no such module".to_string()),
            "{:?}",
            r.notes
        );
    }

    #[test]
    fn a_layout_this_window_can_show_comes_back_unchanged() {
        let r = reconcile(saved(), &set(&[term()]), Some(&agent()), &[], &[]).unwrap();
        same_as(&r, saved());
        assert_eq!(r.layout.hidden(), &set(&[term()]));
        assert_eq!(r.layout.focus(), &agent());
        assert!(r.notes.is_empty(), "{:?}", r.notes);
    }

    /// Removes modules this window does not have, including unregistered Lua panels.
    /// Their siblings take the space, and their `hidden` entries are removed.
    #[test]
    fn a_module_this_window_does_not_have_is_left_out_and_its_sibling_takes_the_space() {
        let with_gone = Node::split(
            Axis::Row,
            0.7,
            saved(),
            row(leaf(ModuleId::lua("gone")), leaf(ModuleId::parse("canvas").unwrap())),
        );
        let r = reconcile(with_gone, &set(&[ModuleId::lua("gone")]), Some(&editor()), &[], &[]).unwrap();
        same_as(&r, saved());
        assert!(r.layout.hidden().is_empty());
        assert_eq!(
            r.notes,
            [
                "left out 'lua:gone': this window has no such module",
                "left out 'canvas': this window has no such module",
            ]
        );
    }

    /// A registered panel keeps its place; one the file has never seen is placed where a first
    /// launch places it -- a `main` panel hides the editor there too.
    #[test]
    fn a_registered_panel_keeps_its_place_and_a_new_one_goes_where_a_first_launch_puts_it() {
        let side = ModuleDecl {
            id: ModuleId::lua("side"),
            placement: Placement::RightOfRoot,
        };
        let kept = row(saved(), leaf(side.id.clone()));
        let r = reconcile(
            kept.clone(),
            &BTreeSet::new(),
            Some(&editor()),
            std::slice::from_ref(&side),
            &[],
        )
        .unwrap();
        same_as(&r, kept);
        assert!(r.notes.is_empty());

        let r = reconcile(
            saved(),
            &BTreeSet::new(),
            Some(&editor()),
            std::slice::from_ref(&side),
            &[],
        )
        .unwrap();
        same_as(
            &r,
            Node::split(Axis::Row, RIGHT_OF_ROOT_SHARE, saved(), leaf(side.id.clone())),
        );
        assert_eq!(r.notes, ["placed 'lua:side' where a first launch does (RightOfRoot)"]);

        let main = ModuleDecl {
            id: ModuleId::lua("main"),
            placement: Placement::InPlaceOfEditor,
        };
        let r = reconcile(
            saved(),
            &BTreeSet::new(),
            Some(&editor()),
            std::slice::from_ref(&main),
            &[],
        )
        .unwrap();
        assert!(r.layout.hidden().contains(&editor()));
        assert_eq!(
            r.layout.focus(),
            &main.id,
            "the editor was hidden; the panel in its place is the first shown module"
        );
    }

    /// A state file or Lua default tree without a terminal receives one below the editor's own
    /// leaf, hidden until `Ctrl+a t`, leaving the agent untouched.
    #[test]
    fn a_missing_terminal_is_placed_below_the_editor_and_hidden() {
        let r = reconcile(row(leaf(editor()), leaf(agent())), &BTreeSet::new(), None, &[], &[]).unwrap();
        same_as(
            &r,
            row(
                Node::pinned(
                    Axis::Column,
                    BELOW_ROOT_SHARE,
                    Branch::Second,
                    leaf(editor()),
                    leaf(term()),
                ),
                leaf(agent()),
            ),
        );
        assert_eq!(r.layout.hidden(), &set(&[term()]));
        assert_eq!(r.layout.focus(), &editor());
    }

    /// A Lua default or a template that names chat views, one tiled and one floating, opens with
    /// both, as it does with terminal instances: nothing is left out and nothing is noted about them.
    #[test]
    fn a_lua_default_keeps_the_chat_views_it_names() {
        let chat2 = ModuleId::parse("agent#2").unwrap();
        let chat3 = ModuleId::parse("agent#3").unwrap();
        let tiled = row(row(leaf(editor()), leaf(agent())), leaf(chat2.clone()));
        let floats = vec![Float {
            id: chat3.clone(),
            rect: FloatRect {
                cx: 0.5,
                cy: 0.5,
                w: 600,
                h: 400,
            },
            scratch: false,
        }];
        let r = reconcile_default_floating(tiled, floats, &[]).unwrap();
        assert!(r.layout.leaves().contains(&chat2), "the tiled chat view stays tiled");
        assert!(r.layout.is_floating(&chat3), "the floating chat view stays a float");
        assert!(!r.layout.hidden().contains(&chat2) && !r.layout.hidden().contains(&chat3));
        assert!(
            r.notes.iter().all(|n| !n.contains("agent#")),
            "no note about a chat view: {:?}",
            r.notes
        );
    }

    /// A default layout with a terminal below a row is a plain `Column` split.
    /// Pinning the terminal here keeps its height through `F11`.
    /// An editor placed under the agent keeps its ratio instead.
    #[test]
    fn a_bottom_row_from_outside_is_pinned_as_a_first_launch_pins_it() {
        let upper = || Node::split(Axis::Row, 0.6, leaf(editor()), leaf(agent()));
        let lua_default = Node::split(Axis::Column, 0.7, upper(), leaf(term()));
        let r = reconcile_default(lua_default, &[]).unwrap();
        same_as(
            &r,
            Node::pinned(Axis::Column, 0.7, Branch::Second, upper(), leaf(term())),
        );
        assert_eq!(
            r.notes,
            ["pinned 'terminal' below everything, as a first launch does: it keeps its height when the window grows"]
        );

        let editor_below = Node::split(Axis::Column, 0.5, row(leaf(agent()), leaf(term())), leaf(editor()));
        let r = reconcile_default(editor_below.clone(), &[]).unwrap();
        same_as(&r, editor_below);
        assert!(r.notes.is_empty(), "{:?}", r.notes);
    }

    /// The final leaf of a flat column spans its bottom and keeps its height when the window grows.
    /// A terminal in the middle is not a bottom row and keeps its share.
    #[test]
    fn the_last_row_of_an_n_ary_lua_column_is_pinned() {
        let stack = |a: Node, b: Node, c: Node| {
            Node::Con(Con::new(
                ConLayout::SplitV,
                vec![
                    Child::new(a, 1.0 / 3.0),
                    Child::new(b, 1.0 / 3.0),
                    Child::new(c, 1.0 / 3.0),
                ],
            ))
        };
        let r = reconcile_default(stack(leaf(editor()), leaf(agent()), leaf(term())), &[]).unwrap();
        let mut expected = stack(leaf(editor()), leaf(agent()), leaf(term()));
        let Node::Con(c) = &mut expected else { unreachable!() };
        c.children[2].pin = Some(Pin { px: None });
        same_as(&r, expected);
        assert_eq!(r.notes.len(), 1, "{:?}", r.notes);

        let middle = stack(leaf(editor()), leaf(term()), leaf(agent()));
        let r = reconcile_default(middle.clone(), &[]).unwrap();
        same_as(&r, middle);
    }

    /// A below-root module inside the first child of a column is not at the window's bottom when
    /// something else is below it, so it stays a share.
    #[test]
    fn a_bottom_row_inside_the_upper_part_of_a_column_is_not_pinned() {
        let upper = Node::split(Axis::Column, 0.5, leaf(editor()), leaf(term()));
        let nested = Node::split(Axis::Column, 0.7, upper, leaf(agent()));
        let r = reconcile_default(nested.clone(), &[]).unwrap();
        same_as(&r, nested);
        assert!(r.notes.is_empty(), "{:?}", r.notes);
    }

    #[test]
    fn a_reopened_tab_strip_keeps_its_last_tab() {
        for mode in [ConLayout::Tabbed, ConLayout::Stacked] {
            let mut strip = Con::new(
                mode,
                vec![
                    Child::new(leaf(agent()), 0.5),
                    Child::new(leaf(ModuleId::lua("gone")), 0.5),
                ],
            );
            strip.focus = 1;
            strip.last_split = Axis::Column;
            let strip_id = strip.id;
            let r = reconcile(
                row(leaf(editor()), Node::Con(strip)),
                &BTreeSet::new(),
                Some(&editor()),
                &[],
                &[],
            )
            .unwrap();
            let strip = r.layout.root().children[1]
                .node
                .as_con()
                .expect("the remaining tab keeps its strip");
            assert_eq!(strip.id, strip_id);
            assert_eq!(strip.layout, mode);
            assert_eq!(strip.last_split, Axis::Column);
            assert_eq!(strip.focus, 0);
            assert_eq!(strip.children, vec![Child::new(leaf(agent()), 1.0)]);
            assert!(r.layout.is_titled_on_strip(&agent()));
        }
    }

    #[test]
    fn the_editor_under_a_gone_main_panel_in_a_kept_strip_is_shown() {
        for mode in [ConLayout::Tabbed, ConLayout::Stacked] {
            let main = ModuleId::lua("main");
            let strip = Con::new(
                mode,
                vec![Child::new(leaf(main.clone()), 0.5), Child::new(leaf(editor()), 0.5)],
            );
            let strip_id = strip.id;
            let r = reconcile(
                row(Node::Con(strip), leaf(agent())),
                &set(&[editor()]),
                Some(&main),
                &[],
                &[],
            )
            .unwrap();
            assert!(
                r.layout.is_shown(&editor()),
                "the vanished panel no longer covers the editor"
            );
            assert!(r.layout.is_visible(&editor()));
            assert_eq!(r.layout.focus(), &editor());
            let strip = r.layout.root().find(strip_id).expect("the editor keeps its strip");
            assert_eq!(strip.layout, mode);
            assert_eq!(strip.children.len(), 1);
            assert_eq!(strip.children[0].share, 1.0);
            assert_eq!(strip.children[0].node.leaves(), vec![editor(), term()]);
            assert!(r.notes.iter().any(|note| note.starts_with("showed 'editor':")));
        }
    }

    /// Leaving a module out of a tabbed container keeps the drawn tab on the same child, or on the
    /// one that followed it, instead of leaving a focus that points past the end.
    #[test]
    fn leaving_out_a_tab_keeps_the_drawn_tab_in_range() {
        let tabs = |ids: [ModuleId; 3], focus: usize| {
            let mut c = Con::new(
                ConLayout::Tabbed,
                ids.into_iter().map(|id| Child::new(leaf(id), 1.0 / 3.0)).collect(),
            );
            c.focus = focus;
            row(leaf(editor()), Node::Con(c))
        };
        let gone = ModuleId::lua("gone");
        let drawn = |r: &Reconciled| {
            let root = r.layout.root();
            let c = root.children[1].node.as_con().expect("the tabbed container").clone();
            (c.children.len(), c.focus, c.children[c.focus].node.leaves())
        };
        // The last tab is the drawn one and is left out: the first tab follows it.
        let r = reconcile(
            tabs([agent(), term(), gone.clone()], 2),
            &BTreeSet::new(),
            Some(&editor()),
            &[],
            &[],
        )
        .unwrap();
        assert_eq!(drawn(&r), (2, 0, vec![agent()]));
        // A tab before the drawn one is left out: the drawn tab is still the terminal.
        let r = reconcile(
            tabs([gone.clone(), agent(), term()], 2),
            &BTreeSet::new(),
            Some(&editor()),
            &[],
            &[],
        )
        .unwrap();
        assert_eq!(drawn(&r), (2, 1, vec![term()]));
        // The drawn tab is in the middle and is left out: the one after it is drawn.
        let r = reconcile(
            tabs([agent(), gone, term()], 1),
            &BTreeSet::new(),
            Some(&editor()),
            &[],
            &[],
        )
        .unwrap();
        assert_eq!(drawn(&r), (2, 1, vec![term()]));
    }

    /// `eitri.layout.default{ 'tabbed', {'editor'}, {'agent'}, {'terminal'}, active = 2 }`: the
    /// chat is drawn as asked and takes the keys, rather than the editor taking them and switching the
    /// tab back. Nested inside a split, the editor beside a stacked container keeps the keys and the
    /// stacked container keeps its own choice.
    #[test]
    fn the_keys_start_on_the_tab_the_tree_draws() {
        let tabs = |layout: ConLayout, ids: Vec<ModuleId>, active: usize| {
            let n = ids.len() as f32;
            let mut c = Con::new(
                layout,
                ids.into_iter().map(|id| Child::new(leaf(id), 1.0 / n)).collect(),
            );
            c.focus = active;
            Node::Con(c)
        };
        let r = reconcile_default(tabs(ConLayout::Tabbed, vec![editor(), agent(), term()], 1), &[]).unwrap();
        assert_eq!(r.layout.focus(), &agent());
        assert_eq!(r.layout.root().focus, 1, "the tab asked for is drawn");
        assert!(r.layout.is_visible(&agent()));
        assert!(r.layout.is_tabbed_away(&editor()));

        let inner = tabs(ConLayout::Stacked, vec![editor(), agent()], 1);
        let r = reconcile_default(row(inner, leaf(term())), &[]).unwrap();
        assert_eq!(r.layout.focus(), &agent(), "the editor's stack draws the chat");
        let stack = r.layout.root().children[0].node.as_con().unwrap();
        assert_eq!(stack.focus, 1);

        let r = reconcile_default(
            row(leaf(editor()), tabs(ConLayout::Tabbed, vec![agent(), term()], 1)),
            &[],
        )
        .unwrap();
        assert_eq!(r.layout.focus(), &editor());
        let tabbed = r.layout.root().children[1].node.as_con().unwrap();
        assert_eq!(tabbed.focus, 1, "the terminal is still the drawn tab");
    }

    /// A saved file keeps the arrangement the window left. `Ctrl+a _` with the terminal shown
    /// builds `Column(editor, Column(agent, terminal))` and removes its pin,
    /// so reopening must not pin it again.
    #[test]
    fn a_file_is_never_pinned_by_reconcile() {
        let evened = Node::split(
            Axis::Column,
            1.0 / 3.0,
            leaf(editor()),
            Node::split(Axis::Column, 0.5, leaf(agent()), leaf(term())),
        );
        let r = reconcile(evened.clone(), &BTreeSet::new(), Some(&editor()), &[], &[]).unwrap();
        same_as(&r, evened);
        assert!(r.notes.is_empty(), "{:?}", r.notes);
        let two_level = Node::split(Axis::Column, 0.7, row(leaf(editor()), leaf(agent())), leaf(term()));
        let r = reconcile(two_level.clone(), &BTreeSet::new(), Some(&editor()), &[], &[]).unwrap();
        same_as(&r, two_level);
    }

    /// A layout saved with a Lua `main` panel -- `Row(lua:main, editor)`, the editor hidden -- and
    /// reopened after the panel left `init.lua`: the editor is shown where the panel was, rather than
    /// the window opening with no nvim. An editor hidden beside
    /// a module that is still there stays hidden.
    #[test]
    fn an_editor_hidden_by_a_panel_that_is_gone_is_shown() {
        let main = ModuleId::lua("main");
        let saved = Node::pinned(
            Axis::Column,
            BELOW_ROOT_SHARE,
            Branch::Second,
            Node::split(
                Axis::Row,
                DEFAULT_EDITOR_SHARE,
                Node::split(Axis::Row, 0.5, leaf(main.clone()), leaf(editor())),
                leaf(agent()),
            ),
            leaf(term()),
        );
        let r = reconcile(saved.clone(), &set(&[editor(), term()]), Some(&main), &[], &[]).unwrap();
        assert!(r.layout.is_shown(&editor()), "{:?}", r.notes);
        assert_eq!(r.layout.focus(), &editor());
        assert_eq!(
            r.notes,
            [
                "left out 'lua:main': this window has no such module",
                "showed 'editor': the module that stood in its place (a Lua `main` panel) is left out, and nothing \
                 else takes it"
            ]
        );

        let decl = ModuleDecl {
            id: main.clone(),
            placement: Placement::InPlaceOfEditor,
        };
        let r = reconcile(saved, &set(&[editor(), term()]), Some(&main), &[decl], &[]).unwrap();
        assert!(!r.layout.is_shown(&editor()), "the panel is still there");
        assert!(r.notes.is_empty(), "{:?}", r.notes);

        let r = reconcile(saved_with_side(), &set(&[editor(), term()]), Some(&agent()), &[], &[]).unwrap();
        assert!(
            !r.layout.is_shown(&editor()),
            "hidden beside the chat, which is still there"
        );
        assert_eq!(r.notes, ["left out 'lua:side': this window has no such module"]);
    }

    #[test]
    fn a_tree_without_the_editor_or_the_agent_is_refused() {
        let no_agent = row(leaf(editor()), leaf(term()));
        assert_eq!(
            reconcile(no_agent, &BTreeSet::new(), None, &[], &[]),
            Err(ReconcileError::MissingBuiltIn(agent()))
        );
        let no_editor = row(leaf(agent()), leaf(ModuleId::lua("gone")));
        assert_eq!(
            reconcile(no_editor, &BTreeSet::new(), None, &[], &[]),
            Err(ReconcileError::MissingBuiltIn(editor()))
        );
        assert_eq!(
            reconcile(leaf(ModuleId::lua("gone")), &BTreeSet::new(), None, &[], &[]),
            Err(ReconcileError::MissingBuiltIn(editor())),
            "nothing left at all"
        );
    }

    /// The keys go to `focus` if it is shown, else where a first launch gives them (the next test);
    /// a file that hides everything is refused rather than opened blank.
    #[test]
    fn the_keys_land_on_a_shown_module_and_a_layout_with_none_is_refused() {
        let r = reconcile(saved(), &set(&[editor(), term()]), Some(&editor()), &[], &[]).unwrap();
        assert_eq!(r.layout.focus(), &agent());
        let r = reconcile(saved(), &set(&[term()]), Some(&ModuleId::lua("gone")), &[], &[]).unwrap();
        assert_eq!(r.layout.focus(), &editor());
        assert_eq!(
            reconcile(saved(), &set(&[editor(), agent(), term()]), None, &[], &[]),
            Err(ReconcileError::NothingVisible)
        );
    }

    /// A Lua default tree with the chat on the left carries no focus. The keys go to the editor,
    /// as on first launch, rather than to the first leaf. A focus absent from the tree uses the
    /// same fallback. With the editor hidden, a Lua `main` panel in its place takes focus before
    /// a module earlier in the tree.
    #[test]
    fn without_a_usable_focus_the_keys_go_where_a_first_launch_gives_them() {
        let chat_left = Node::split(Axis::Row, 0.4, leaf(agent()), leaf(editor()));
        let r = reconcile(chat_left.clone(), &BTreeSet::new(), None, &[], &[]).unwrap();
        assert_eq!(r.layout.focus(), &editor());
        let r = reconcile(chat_left, &BTreeSet::new(), Some(&ModuleId::lua("gone")), &[], &[]).unwrap();
        assert_eq!(r.layout.focus(), &editor());

        let side = ModuleDecl {
            id: ModuleId::lua("side"),
            placement: Placement::RightOfRoot,
        };
        let main = ModuleDecl {
            id: ModuleId::lua("main"),
            placement: Placement::InPlaceOfEditor,
        };
        let tree = row(
            leaf(side.id.clone()),
            row(leaf(agent()), row(leaf(main.id.clone()), leaf(editor()))),
        );
        let r = reconcile(tree, &set(&[editor()]), None, &[side, main.clone()], &[]).unwrap();
        assert_eq!(r.layout.focus(), &main.id);
    }

    #[test]
    fn a_tree_that_breaks_an_invariant_is_refused() {
        let twice = row(saved(), leaf(editor()));
        assert_eq!(
            reconcile(twice, &BTreeSet::new(), None, &[], &[]),
            Err(ReconcileError::Layout(LayoutError::Duplicate(editor())))
        );
        // The agent's share, 1 - 0.99, is the one below the minimum.
        let bad_ratio = Node::split(Axis::Row, 0.99, leaf(editor()), leaf(agent()));
        assert_eq!(
            reconcile(bad_ratio, &BTreeSet::new(), None, &[], &[]),
            Err(ReconcileError::Layout(LayoutError::Ratio(1.0 - 0.99)))
        );
    }

    #[test]
    fn reconcile_places_the_terminal_and_instances_where_their_declarations_say() {
        use crate::layout::geometry::{arrange, Frame, Size};
        let t2 = ModuleId::parse("terminal#2").unwrap();
        let root = Node::split(
            Axis::Row,
            0.6,
            Node::Leaf(ModuleId::editor()),
            Node::Leaf(ModuleId::agent()),
        );
        let decls = [ModuleDecl {
            id: ModuleId::terminal(),
            placement: Placement::BelowRoot,
        }];
        let mut layout = reconcile_floating(
            root.clone(),
            Vec::new(),
            BTreeMap::new(),
            &BTreeSet::new(),
            None,
            &decls,
            &[t2.clone()],
        )
        .unwrap()
        .layout;
        layout.show(&ModuleId::terminal()).unwrap();
        layout.show(&t2).unwrap();
        let size = Size { w: 1280, h: 721 };
        let r: BTreeMap<ModuleId, _> = arrange(&layout, &Frame::new(size, 1)).modules.into_iter().collect();
        assert_eq!(
            r[&ModuleId::terminal()].w,
            1280,
            "its declaration: a full-width bottom row"
        );
        assert_eq!(
            r[&t2].w, 1280,
            "an instance with no declaration of its own follows its kind's"
        );

        let gone = ModuleId::parse("terminal#5").unwrap();
        let with_gone = Node::split(Axis::Row, 0.5, root, Node::Leaf(gone.clone()));
        let decls = [ModuleDecl {
            id: gone.clone(),
            placement: Placement::RightOfRoot,
        }];
        let reconciled = reconcile_floating(
            with_gone,
            Vec::new(),
            BTreeMap::new(),
            &BTreeSet::new(),
            None,
            &decls,
            &[],
        )
        .unwrap();
        assert!(
            !reconciled.layout.leaves().contains(&gone),
            "a declaration never makes a module known"
        );
        assert!(
            reconciled.notes.iter().any(|n| n.contains("left out 'terminal#5'")),
            "{:?}",
            reconciled.notes
        );

        let only_decl = [ModuleDecl {
            id: ModuleId::parse("agent#3").unwrap(),
            placement: Placement::RightOfRoot,
        }];
        let root = Node::split(
            Axis::Row,
            0.6,
            Node::Leaf(ModuleId::editor()),
            Node::Leaf(ModuleId::agent()),
        );
        let layout = reconcile_floating(
            root,
            Vec::new(),
            BTreeMap::new(),
            &BTreeSet::new(),
            None,
            &only_decl,
            &[],
        )
        .unwrap()
        .layout;
        assert!(
            !layout.modules().contains(&ModuleId::parse("agent#3").unwrap()),
            "nor places it"
        );
    }
}
