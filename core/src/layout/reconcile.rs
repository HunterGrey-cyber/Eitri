//! A layout that came from outside this process -- the state file (modules spec §4.6) or `init.lua`'s
//! `neovibe.layout.default` (§4.4) -- rebuilt against the modules this window actually has.
//!
//! What it does, in order (spec §4.6, "On load, `reconcile`"):
//! 1. drops every leaf this window has no module for -- a Lua panel no longer registered, or the
//!    canvas before P3 builds it -- and its sibling takes the space, as a hidden module's does. A
//!    hidden editor that is left alone in its place by that is shown again: the Lua `main` panel
//!    that hid it (`Placement::InPlaceOfEditor`, which puts it beside the hidden editor) is gone,
//!    and a window opened with neither is a window with no nvim until `Ctrl+a e`. The one cost: an
//!    editor hidden by hand whose only neighbour was a panel since removed comes back too, and the
//!    note says so;
//! 2. refuses a tree without the editor or without the agent: every window has both, so a tree
//!    missing one is not a layout of this window, and the caller falls back to the default;
//! 3. **`init.lua`'s default only** ([`reconcile_default`]): pins a bottom row a first launch would
//!    have pinned -- a module placed below the root (the terminal, a Lua `bottom` panel) that is a
//!    full-width row at the bottom of the window. `init.lua` has no way to say "pin", and a bottom
//!    row that grew with the window would be the P1 behaviour the owner turned down (the plan
//!    review's finding 7). A file is not pinned here: it holds its pins as the window left them, and
//!    a row `Ctrl+a _` un-pinned (the owner's decision 8) must come back un-pinned;
//! 4. places the terminal below everything, hidden, if the tree does not have it -- where and how a
//!    first launch puts it -- and places each registered Lua panel the tree does not have by its
//!    `position`, as a first launch would;
//! 5. keeps `hidden` only for leaves still in the tree, and gives the keys to `focus` if it is
//!    shown -- else where a first launch gives them: the editor, else the first registered Lua panel
//!    placed in the editor's place, else the first shown module in tree order. `init.lua`'s default
//!    has no focus at all, and its first leaf is not where the keys belong: `{ 'row', {'agent', 0.4},
//!    {'editor'} }` would otherwise open every launch with nvim not taking keys (the plan review's
//!    second round, finding 2).
//!
//! Every change is said, as one note line each, so the caller can log why a window did not reopen
//! exactly as it was left.

use std::collections::BTreeSet;
use std::fmt;

use super::module::{ModuleDecl, ModuleId, Placement};
use super::tree::{place_new, Axis, Branch, Layout, LayoutError, Node, Pin};

/// Every window has these three (the terminal since modules P1 hosted it).
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
) -> Result<Reconciled, ReconcileError> {
    reconcile_with(root, hidden, focus, lua, false)
}

/// `init.lua`'s `neovibe.layout.default` tree, which has nothing hidden and no focus: [`reconcile`],
/// and its bottom rows pinned (the module doc's step 3).
pub fn reconcile_default(root: Node, lua: &[ModuleDecl]) -> Result<Reconciled, ReconcileError> {
    reconcile_with(root, &BTreeSet::new(), None, lua, true)
}

fn reconcile_with(
    root: Node,
    hidden: &BTreeSet<ModuleId>,
    focus: Option<&ModuleId>,
    lua: &[ModuleDecl],
    pin: bool,
) -> Result<Reconciled, ReconcileError> {
    let known = |id: &ModuleId| BUILT_INS.iter().any(|b| b() == *id) || lua.iter().any(|d| d.id == *id);
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
        root = place_new(root, &terminal, Placement::BelowRoot).0;
        hidden.insert(terminal.clone());
        notes.push(format!(
            "placed '{terminal}' below everything, hidden, as a first launch does"
        ));
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
        _ => first_focus(&shown, lua).ok_or(ReconcileError::NothingVisible)?,
    };
    let layout = Layout::from_parts(root, hidden, focus).map_err(ReconcileError::Layout)?;
    Ok(Reconciled { layout, notes })
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
/// length yet -- so it divides by its ratio until it has been on screen, then keeps that height.
///
/// Two shapes reach the bottom. A `Column` split whose second child is a `below_root` module is
/// pinned, and the walk goes on into its first child: the first-child spine, where `Layout::initial`
/// puts every below-root module, each wrapping the tree built so far -- and `{ 'column', {'row', ...},
/// {'terminal'} }` from Lua. A `Column` split whose second child is another `Column` split goes on
/// into that second child: the right-leaning chain an n-ary Lua `column` builds, so the terminal
/// ending `{ 'column', {'editor'}, {'agent'}, {'terminal'} }` is the bottom row too (the whole-branch
/// review, finding 5). A below-root module anywhere else -- the middle of a column, beside something
/// in a row -- is not a bottom row, and stays a ratio.
fn pin_bottom_rows(node: &mut Node, below_root: &dyn Fn(&ModuleId) -> bool, notes: &mut Vec<String>) {
    let Node::Split {
        axis: Axis::Column,
        pin,
        first,
        second,
        ..
    } = node
    else {
        return;
    };
    match second.as_ref() {
        Node::Leaf(id) if below_root(id) => {
            if pin.is_none() {
                *pin = Some(Pin {
                    side: Branch::Second,
                    px: None,
                });
                notes.push(format!(
                    "pinned '{id}' below everything, as a first launch does: it keeps its height when the window grows"
                ));
            }
            pin_bottom_rows(first, below_root, notes);
        }
        Node::Split { axis: Axis::Column, .. } => pin_bottom_rows(second, below_root, notes),
        _ => {}
    }
}

/// `node` without the leaves `keep` refuses, each collapsed into its sibling. `None` if nothing is
/// left. The refused ids go into `dropped`, in tree order; a leaf left alone where a split was, by a
/// refused sibling, goes into `uncovered` (step 1's hidden editor).
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
        Node::Split {
            axis,
            ratio,
            pin,
            first,
            second,
        } => match (
            prune(*first, keep, dropped, uncovered),
            prune(*second, keep, dropped, uncovered),
        ) {
            (Some(first), Some(second)) => Some(Node::Split {
                axis,
                ratio,
                pin,
                first: Box::new(first),
                second: Box::new(second),
            }),
            (Some(only), None) | (None, Some(only)) => {
                if let Node::Leaf(id) = &only {
                    uncovered.push(id.clone());
                }
                Some(only)
            }
            (None, None) => None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::super::tree::{Axis, Branch, BELOW_ROOT_SHARE, DEFAULT_EDITOR_SHARE, RIGHT_OF_ROOT_SHARE};
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
        let r = reconcile(saved(), &set(&[term()]), Some(&agent()), &[]).unwrap();
        assert_eq!(r.layout.root(), &saved());
        assert_eq!(r.layout.hidden(), &set(&[term()]));
        assert_eq!(r.layout.focus(), &agent());
        assert!(r.notes.is_empty(), "{:?}", r.notes);
    }

    /// Spec §4.6: "drops ids it does not know (a Lua panel no longer registered)". Its sibling takes
    /// the space, and a `hidden` entry for it goes with it.
    #[test]
    fn a_module_this_window_does_not_have_is_left_out_and_its_sibling_takes_the_space() {
        let with_gone = Node::split(
            Axis::Row,
            0.7,
            saved(),
            row(leaf(ModuleId::lua("gone")), leaf(ModuleId::parse("canvas").unwrap())),
        );
        let r = reconcile(with_gone, &set(&[ModuleId::lua("gone")]), Some(&editor()), &[]).unwrap();
        assert_eq!(r.layout.root(), &saved());
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
        let r = reconcile(kept.clone(), &BTreeSet::new(), Some(&editor()), &[side.clone()]).unwrap();
        assert_eq!(r.layout.root(), &kept);
        assert!(r.notes.is_empty());

        let r = reconcile(saved(), &BTreeSet::new(), Some(&editor()), &[side.clone()]).unwrap();
        assert_eq!(
            r.layout.root(),
            &Node::split(Axis::Row, RIGHT_OF_ROOT_SHARE, saved(), leaf(side.id.clone()))
        );
        assert_eq!(r.notes, ["placed 'lua:side' where a first launch does (RightOfRoot)"]);

        let main = ModuleDecl {
            id: ModuleId::lua("main"),
            placement: Placement::InPlaceOfEditor,
        };
        let r = reconcile(saved(), &BTreeSet::new(), Some(&editor()), &[main.clone()]).unwrap();
        assert!(r.layout.hidden().contains(&editor()));
        assert_eq!(
            r.layout.focus(),
            &main.id,
            "the editor was hidden; the panel in its place is the first shown module"
        );
    }

    /// A file written before the terminal existed, or `init.lua`'s default tree without one: it goes
    /// where a first launch puts it, hidden until `Ctrl+a t`.
    #[test]
    fn a_missing_terminal_is_placed_below_everything_and_hidden() {
        let r = reconcile(row(leaf(editor()), leaf(agent())), &BTreeSet::new(), None, &[]).unwrap();
        assert_eq!(
            r.layout.root(),
            &Node::pinned(
                Axis::Column,
                BELOW_ROOT_SHARE,
                Branch::Second,
                row(leaf(editor()), leaf(agent())),
                leaf(term())
            )
        );
        assert_eq!(r.layout.hidden(), &set(&[term()]));
        assert_eq!(r.layout.focus(), &editor());
    }

    /// `init.lua`'s `neovibe.layout.default{ 'column', {'row', {'editor'}, {'agent'}, share = 0.7},
    /// {'terminal'} }` -- the natural way to ask for a bottom terminal -- is a plain `Column` split;
    /// pinned here, its terminal keeps its height through `F11` as a first launch's does (the plan
    /// review's finding 7). A module below the root that a first launch does not place there -- the
    /// editor under the agent -- stays a ratio.
    #[test]
    fn a_bottom_row_from_outside_is_pinned_as_a_first_launch_pins_it() {
        let upper = || Node::split(Axis::Row, 0.6, leaf(editor()), leaf(agent()));
        let lua_default = Node::split(Axis::Column, 0.7, upper(), leaf(term()));
        let r = reconcile_default(lua_default, &[]).unwrap();
        assert_eq!(
            r.layout.root(),
            &Node::pinned(Axis::Column, 0.7, Branch::Second, upper(), leaf(term()))
        );
        assert_eq!(
            r.notes,
            ["pinned 'terminal' below everything, as a first launch does: it keeps its height when the window grows"]
        );

        let editor_below = Node::split(Axis::Column, 0.5, row(leaf(agent()), leaf(term())), leaf(editor()));
        let r = reconcile_default(editor_below.clone(), &[]).unwrap();
        assert_eq!(r.layout.root(), &editor_below);
        assert!(r.notes.is_empty(), "{:?}", r.notes);
    }

    /// The owner's decision "a bottom row keeps its height", for the n-ary spelling: `{ 'column',
    /// {'editor'}, {'agent'}, {'terminal'} }` is `Column(editor, Column(agent, terminal))`, and its
    /// terminal is full width at the bottom (the whole-branch review's finding 5: unpinned, with no
    /// note). A terminal in the middle of such a column is not a bottom row.
    #[test]
    fn the_last_row_of_an_n_ary_lua_column_is_pinned() {
        let stack =
            |a: Node, b: Node, c: Node| Node::split(Axis::Column, 1.0 / 3.0, a, Node::split(Axis::Column, 0.5, b, c));
        let r = reconcile_default(stack(leaf(editor()), leaf(agent()), leaf(term())), &[]).unwrap();
        assert_eq!(
            r.layout.root(),
            &Node::split(
                Axis::Column,
                1.0 / 3.0,
                leaf(editor()),
                Node::pinned(Axis::Column, 0.5, Branch::Second, leaf(agent()), leaf(term()))
            )
        );
        assert_eq!(r.notes.len(), 1, "{:?}", r.notes);

        let middle = stack(leaf(editor()), leaf(term()), leaf(agent()));
        let r = reconcile_default(middle.clone(), &[]).unwrap();
        assert_eq!(r.layout.root(), &middle);
    }

    /// A file keeps what the window left: `Ctrl+a _` with the terminal shown builds that same
    /// `Column(editor, Column(agent, terminal))` and un-pins the terminal on purpose (the owner's
    /// decision 8), so reopening it must not pin it again.
    #[test]
    fn a_file_is_never_pinned_by_reconcile() {
        let evened = Node::split(
            Axis::Column,
            1.0 / 3.0,
            leaf(editor()),
            Node::split(Axis::Column, 0.5, leaf(agent()), leaf(term())),
        );
        let r = reconcile(evened.clone(), &BTreeSet::new(), Some(&editor()), &[]).unwrap();
        assert_eq!(r.layout.root(), &evened);
        assert!(r.notes.is_empty(), "{:?}", r.notes);
        let two_level = Node::split(Axis::Column, 0.7, row(leaf(editor()), leaf(agent())), leaf(term()));
        let r = reconcile(two_level.clone(), &BTreeSet::new(), Some(&editor()), &[]).unwrap();
        assert_eq!(r.layout.root(), &two_level);
    }

    /// A layout saved with a Lua `main` panel -- `Row(lua:main, editor)`, the editor hidden -- and
    /// reopened after the panel left `init.lua`: the editor is shown where the panel was, rather than
    /// the window opening with no nvim (the whole-branch review's finding 8). An editor hidden beside
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
        let r = reconcile(saved.clone(), &set(&[editor(), term()]), Some(&main), &[]).unwrap();
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
        let r = reconcile(saved, &set(&[editor(), term()]), Some(&main), &[decl]).unwrap();
        assert!(!r.layout.is_shown(&editor()), "the panel is still there");
        assert!(r.notes.is_empty(), "{:?}", r.notes);

        let r = reconcile(saved_with_side(), &set(&[editor(), term()]), Some(&agent()), &[]).unwrap();
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
            reconcile(no_agent, &BTreeSet::new(), None, &[]),
            Err(ReconcileError::MissingBuiltIn(agent()))
        );
        let no_editor = row(leaf(agent()), leaf(ModuleId::lua("gone")));
        assert_eq!(
            reconcile(no_editor, &BTreeSet::new(), None, &[]),
            Err(ReconcileError::MissingBuiltIn(editor()))
        );
        assert_eq!(
            reconcile(leaf(ModuleId::lua("gone")), &BTreeSet::new(), None, &[]),
            Err(ReconcileError::MissingBuiltIn(editor())),
            "nothing left at all"
        );
    }

    /// The keys go to `focus` if it is shown, else where a first launch gives them (the next test);
    /// a file that hides everything is refused rather than opened blank.
    #[test]
    fn the_keys_land_on_a_shown_module_and_a_layout_with_none_is_refused() {
        let r = reconcile(saved(), &set(&[editor(), term()]), Some(&editor()), &[]).unwrap();
        assert_eq!(r.layout.focus(), &agent());
        let r = reconcile(saved(), &set(&[term()]), Some(&ModuleId::lua("gone")), &[]).unwrap();
        assert_eq!(r.layout.focus(), &editor());
        assert_eq!(
            reconcile(saved(), &set(&[editor(), agent(), term()]), None, &[]),
            Err(ReconcileError::NothingVisible)
        );
    }

    /// `init.lua`'s `neovibe.layout.default{ 'row', {'agent', 0.4}, {'editor'} }` -- the chat on the
    /// left, the natural way to ask for it -- carries no focus: the keys go to the editor, as every
    /// first launch gives them, not to the first leaf (the plan review's second round, finding 2). A
    /// focus the tree does not have is no better. With the editor hidden, a Lua `main` panel in its
    /// place takes them before a module earlier in the tree.
    #[test]
    fn without_a_usable_focus_the_keys_go_where_a_first_launch_gives_them() {
        let chat_left = Node::split(Axis::Row, 0.4, leaf(agent()), leaf(editor()));
        let r = reconcile(chat_left.clone(), &BTreeSet::new(), None, &[]).unwrap();
        assert_eq!(r.layout.focus(), &editor());
        let r = reconcile(chat_left, &BTreeSet::new(), Some(&ModuleId::lua("gone")), &[]).unwrap();
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
        let r = reconcile(tree, &set(&[editor()]), None, &[side, main.clone()]).unwrap();
        assert_eq!(r.layout.focus(), &main.id);
    }

    #[test]
    fn a_tree_that_breaks_an_invariant_is_refused() {
        let twice = row(saved(), leaf(editor()));
        assert_eq!(
            reconcile(twice, &BTreeSet::new(), None, &[]),
            Err(ReconcileError::Layout(LayoutError::Duplicate(editor())))
        );
        let bad_ratio = Node::split(Axis::Row, 0.99, leaf(editor()), leaf(agent()));
        assert_eq!(
            reconcile(bad_ratio, &BTreeSet::new(), None, &[]),
            Err(ReconcileError::Layout(LayoutError::Ratio(0.99)))
        );
    }
}
