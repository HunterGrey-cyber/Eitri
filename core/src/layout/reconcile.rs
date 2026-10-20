//! Rebuilds a saved or `init.lua` default layout against the modules this window has.
//!
//! 1. Removes unavailable leaves, such as unregistered Lua panels. Their siblings take the space.
//!    If this leaves a hidden editor alone in its place, it is shown again so a removed Lua `main`
//!    panel does not leave the window without an editor. An editor hidden by hand also returns if
//!    its only neighbour was removed; a note explains the change.
//! 2. Rejects a tree missing the editor or agent so the caller can fall back to the default.
//! 3. For `init.lua` defaults only, [`reconcile_default`] pins a full-width bottom terminal or Lua
//!    `bottom` panel to keep its height when the window grows. `init.lua` cannot express pins.
//!    Saved layouts retain their own pins, including rows unpinned with `Ctrl+a _`.
//! 4. Adds a missing terminal below the editor's leaf, hidden, and places missing registered Lua
//!    panels by their `position`, as on first launch.
//! 5. Keeps `hidden` entries only for leaves still in the tree. Restores focus if it is shown;
//!    otherwise chooses the editor, then the first registered Lua panel placed in its place,
//!    then the first shown module in tree order. Only the active children of tabbed and
//!    stacked containers are eligible, so focus does not switch to another tab.
//!    An `init.lua` default has no saved focus. Choosing its first leaf could give the agent the
//!    keys instead of the editor in `{ 'row', {'agent', 0.4}, {'editor'} }`.
//!
//! Each change produces a note so the caller can log why the window did not reopen as it was left.

use std::collections::BTreeSet;
use std::fmt;

use super::con::{ConLayout, Node, Pin};
use super::module::{ModuleDecl, ModuleId, Placement};
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
    reconcile_with(root, hidden, focus, lua, instances, false)
}

/// `init.lua`'s `eitri.layout.default` tree, which has nothing hidden and no focus: [`reconcile`],
/// and its bottom rows pinned (the module doc's step 3).
pub fn reconcile_default(root: Node, lua: &[ModuleDecl]) -> Result<Reconciled, ReconcileError> {
    let instances: Vec<_> = root
        .leaves()
        .into_iter()
        .filter(|id| id.kind() == super::module::ModuleKind::Terminal && id.number() >= 2)
        .collect();
    reconcile_with(root, &BTreeSet::new(), None, lua, &instances, true)
}

fn reconcile_with(
    root: Node,
    hidden: &BTreeSet<ModuleId>,
    focus: Option<&ModuleId>,
    lua: &[ModuleDecl],
    instances: &[ModuleId],
    pin: bool,
) -> Result<Reconciled, ReconcileError> {
    let known = |id: &ModuleId| {
        BUILT_INS.iter().any(|b| b() == *id) || lua.iter().any(|d| d.id == *id) || instances.contains(id)
    };
    let mut notes = Vec::new();
    let mut dropped = Vec::new();
    let mut uncovered = Vec::new();
    let root = prune(root, &known, &mut dropped, &mut uncovered);
    for id in &dropped {
        notes.push(format!("left out '{id}': this window has no such module"));
    }
    let mut root = root.ok_or(ReconcileError::MissingBuiltIn(ModuleId::editor()))?;
    for built_in in [ModuleId::editor(), ModuleId::agent()] {
        if !root.leaves().contains(&built_in) {
            return Err(ReconcileError::MissingBuiltIn(built_in));
        }
    }
    if pin {
        let below_root = |id: &ModuleId| {
            *id == ModuleId::terminal() || lua.iter().any(|d| d.id == *id && d.placement == Placement::BelowRoot)
        };
        pin_bottom_rows(&mut root, &below_root, &mut notes);
    }
    let mut hidden: BTreeSet<ModuleId> = hidden.iter().filter(|id| root.leaves().contains(id)).cloned().collect();
    let editor = ModuleId::editor();
    if uncovered.contains(&editor) && hidden.remove(&editor) {
        notes.push(format!(
            "showed '{editor}': the module that stood in its place (a Lua `main` panel) is left out, and \
             nothing else takes it"
        ));
    }
    let terminal = ModuleId::terminal();
    if !root.leaves().contains(&terminal) {
        root = place_new(root, &terminal, Placement::BelowEditor).0;
        hidden.insert(terminal.clone());
        notes.push(format!(
            "placed '{terminal}' below the editor, hidden, as a first launch does"
        ));
    }
    for id in instances {
        if !root.leaves().contains(id) {
            root = place_new(root, id, Placement::BelowEditor).0;
            hidden.insert(id.clone());
            notes.push(format!(
                "placed '{id}' below the editor, hidden: another workspace has it"
            ));
        }
    }
    for decl in lua {
        if root.leaves().contains(&decl.id) {
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
    let shown: Vec<ModuleId> = root.leaves().into_iter().filter(|id| !hidden.contains(id)).collect();
    let focus = match focus {
        Some(f) if shown.contains(f) => f.clone(),
        _ => {
            let mut in_view = Vec::new();
            drawn_leaves(&root, &|id| !hidden.contains(id), &mut in_view);
            first_focus(&in_view, lua).ok_or(ReconcileError::NothingVisible)?
        }
    };
    let layout = Layout::from_parts(root, hidden, focus).map_err(ReconcileError::Layout)?;
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

/// Step 5 of the module doc, when `focus` cannot be used: where `Layout::initial` gives the keys --
/// the editor, else the first registered Lua panel in the editor's place -- and otherwise the first
/// shown module in tree order. `None` only if nothing is shown.
fn first_focus(shown: &[ModuleId], lua: &[ModuleDecl]) -> Option<ModuleId> {
    let editor = ModuleId::editor();
    if shown.contains(&editor) {
        return Some(editor);
    }
    lua.iter()
        .filter(|decl| decl.placement == Placement::InPlaceOfEditor)
        .map(|decl| &decl.id)
        .find(|id| shown.contains(id))
        .or_else(|| shown.first())
        .cloned()
}

/// Step 3 of the module doc, from `node` down: each `below_root` module that is a full-width row at
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

/// `node` without the leaves `keep` refuses. A container left with one child is replaced by it, which
/// takes the container's place; one left with more has their shares scaled back up to 1. `None` if
/// nothing is left. The refused ids go into `dropped`, in tree order; a leaf left alone where a
/// container was, by refused siblings, goes into `uncovered` (step 1's hidden editor).
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
            let focus = c.focus;
            let mut kept_before_focus = 0;
            c.children = std::mem::take(&mut c.children)
                .into_iter()
                .enumerate()
                .filter_map(|(i, mut ch)| {
                    ch.node = prune(ch.node, keep, dropped, uncovered)?;
                    if i < focus {
                        kept_before_focus += 1;
                    }
                    Some(ch)
                })
                .collect();
            if !c.children.is_empty() && c.children.len() != before {
                // The remembered child (the drawn tab) stays the same child while it survives, and
                // passes to the one that followed it, wrapping, when it was left out.
                c.focus = kept_before_focus % c.children.len();
            }
            match c.children.len() {
                0 => None,
                1 if before > 1 => {
                    let only = c.children.pop().expect("one child").node;
                    if let Node::Leaf(id) = &only {
                        uncovered.push(id.clone());
                    }
                    Some(only)
                }
                n => {
                    if n != before {
                        let sum: f32 = c.children.iter().map(|ch| ch.share).sum();
                        for ch in &mut c.children {
                            ch.share /= sum;
                        }
                    }
                    Some(Node::Con(c))
                }
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
}
