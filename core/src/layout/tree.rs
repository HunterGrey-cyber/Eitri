//! The layout as data (modules spec §4.1): a binary tree of splits whose leaves are modules, plus
//! which modules are hidden, which one is zoomed, and which one last held the keys. Pure and
//! GTK-free: `shell`'s `ModuleGrid` allocates what [`super::geometry::arrange`] computes from it,
//! and never decides layout itself.
//!
//! **The invariants**, each enforced here and each held by a test below:
//! 1. each `ModuleId` appears at most once in the tree;
//! 2. `hidden` names only leaves that are in the tree;
//! 3. at least one leaf is visible (a hide that would leave none is refused);
//! 4. **zoom never touches `hidden`**: zoom is its own field, so ending a zoom can never resurrect a
//!    module the owner closed (today's `PaneLayout::unzoom` showed every child).
//!
//! Plus three this file adds because the rest of the model leans on them: every ratio is inside
//! `MIN_RATIO..=MAX_RATIO`; `focus` and every `mru` entry are leaves; and **the keys never sit on a
//! hidden module** -- [`Layout::set_focus`] refuses one, and [`super::geometry::hide`] moves them
//! before it hides the module that holds them (the spec's P1 row: "a test that a hidden module never
//! holds focus").
//!
//! **A split divides by its ratio, unless it is pinned** (modules P2). A [`Pin`] keeps one side's
//! length in pixels when the space the split divides changes, as an IDE's bottom panel keeps its
//! height when the window grows -- the owner's call on the spec's §14 question, "a bottom row keeps
//! its height when the window grows". A module placed below the whole root is pinned
//! ([`Placement::BelowRoot`]: a Lua `bottom` panel), and so, since v1 trial item 6 (2026-09-28), is
//! one placed below the editor's own leaf alone ([`Placement::BelowEditor`]: the terminal's default
//! place, "extended" from the same rule -- the owner's "a bottom row keeps its height", applied to a
//! row that is not full width). Either module's first show still divides by the ratio, a third of
//! the height at whatever size the window has then, as `main`'s `shown_position` did;
//! [`super::geometry::settle_pins`] then freezes that length.

use std::collections::BTreeSet;
use std::fmt;

use super::module::{ModuleDecl, ModuleId, Placement};

/// A split never gives either side less than this share (spec §4.1).
pub const MIN_RATIO: f32 = 0.05;
pub const MAX_RATIO: f32 = 0.95;

/// The editor's share of the default `Row(editor | agent)` (spec §4.4): exactly where the window it
/// replaces put the divider. `build_content_area` set its `GtkPaned` to 760px (`shell/src/layout.rs:32`
/// at `25724d1`, the last `main` before this phase deleted it) in the 1280px default window, whose 1px divider leaves 1279px to divide, and a `GtkPaned` whose children
/// both resize scales that position with the window after its first allocation -- which is what a
/// ratio does. So the default window is pixel-identical. A first allocation of another width (a
/// maximised or tiled launch) is not: `GtkPaned` kept the 760px it was given before it had a size,
/// and the checklist compares that case too. (The spec first said 0.6, which is 767px.)
pub const DEFAULT_EDITOR_SHARE: f32 = 760.0 / 1279.0;

/// The share the existing root keeps when a module is placed below it: the bottom slot it replaces
/// started with its divider 480px down (`build_vertical_split`, `shell/src/layout.rs:323` at
/// `25724d1`). The default
/// window's content is 721px tall if the top bar is the 39px its CSS asks for (`.topbar`'s
/// `min-height: 38px` and 1px border), 720 after the divider, and 480 of 720 is two thirds. **Read
/// off the CSS, not measured**: the checklist's first item measures where this divider lands.
pub const BELOW_ROOT_SHARE: f32 = 2.0 / 3.0;

/// The share the existing root keeps when a module is placed to its right. Nothing today does
/// this -- a Lua `side` panel used to REPLACE the agent -- so this is new: the new module gets
/// a third of the width.
pub const RIGHT_OF_ROOT_SHARE: f32 = 0.67;

/// The split a module placed in the editor's leaf makes with the (hidden) editor. Only seen once
/// the editor is shown again (P2): until then the split collapses and the module fills the leaf.
const IN_PLACE_SHARE: f32 = 0.5;

/// Puts module `id`, which `root` does not contain, where `placement` says (spec §4.5). `true` when
/// it took the editor's leaf, so the editor has to be hidden -- a Lua `main` panel. Shared by the first
/// launch and by `super::reconcile`, which places a module a saved layout has never seen the way a
/// first launch would. A tree with no editor leaf gets an `InPlaceOfEditor` module right of the root.
pub(super) fn place_new(root: Node, id: &ModuleId, placement: Placement) -> (Node, bool) {
    let leaf = Node::Leaf(id.clone());
    match placement {
        Placement::InPlaceOfEditor if root.leaves().contains(&ModuleId::editor()) => {
            let mut root = root;
            root.replace_leaf(&ModuleId::editor(), |editor| {
                Node::split(Axis::Row, IN_PLACE_SHARE, leaf, editor)
            });
            (root, true)
        }
        Placement::RightOfRoot | Placement::InPlaceOfEditor => {
            (Node::split(Axis::Row, RIGHT_OF_ROOT_SHARE, root, leaf), false)
        }
        Placement::BelowRoot => (
            Node::pinned(Axis::Column, BELOW_ROOT_SHARE, Branch::Second, root, leaf),
            false,
        ),
        Placement::BelowEditor if root.leaves().contains(&ModuleId::editor()) => {
            let mut root = root;
            root.replace_leaf(&ModuleId::editor(), |editor| {
                Node::pinned(Axis::Column, BELOW_ROOT_SHARE, Branch::Second, editor, leaf)
            });
            (root, false)
        }
        Placement::BelowEditor => (
            Node::pinned(Axis::Column, BELOW_ROOT_SHARE, Branch::Second, root, leaf),
            false,
        ),
    }
}

/// `Row`: side by side, with a vertical divider between them. `Column`: stacked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Axis {
    Row,
    Column,
}

/// Which child of a split, on the way down from the root.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Branch {
    First,
    Second,
}

/// A split, named by the branches that lead to it from the root (the root split is `[]`).
pub type SplitPath = Vec<Branch>;

/// A split that keeps one side's length when the length it divides changes (this file's doc). The
/// other side takes whatever is left, within both sides' minimum sizes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pin {
    /// The child whose length is kept.
    pub side: Branch,
    /// That child's length in pixels, once the split has been on screen with both sides; `None`
    /// until then, and the split divides by its ratio.
    pub px: Option<i32>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Node {
    Leaf(ModuleId),
    Split {
        axis: Axis,
        /// The first child's share of the length the split divides, `MIN_RATIO..=MAX_RATIO`. A
        /// pinned split uses it only until its pin has a length.
        ratio: f32,
        pin: Option<Pin>,
        first: Box<Node>,
        second: Box<Node>,
    },
}

impl Node {
    pub fn split(axis: Axis, ratio: f32, first: Node, second: Node) -> Node {
        Node::Split {
            axis,
            ratio,
            pin: None,
            first: Box::new(first),
            second: Box::new(second),
        }
    }

    /// A split whose `side` keeps its length once it has one ([`Pin`]); until then it divides by
    /// `ratio`.
    pub fn pinned(axis: Axis, ratio: f32, side: Branch, first: Node, second: Node) -> Node {
        Node::Split {
            axis,
            ratio,
            pin: Some(Pin { side, px: None }),
            first: Box::new(first),
            second: Box::new(second),
        }
    }

    /// Every leaf, in tree order (first before second, depth first): for a layout built from
    /// splits this is reading order, which is what HINT labels in.
    pub fn leaves(&self) -> Vec<ModuleId> {
        let mut out = Vec::new();
        self.collect_leaves(&mut out);
        out
    }

    fn collect_leaves(&self, out: &mut Vec<ModuleId>) {
        match self {
            Node::Leaf(id) => out.push(id.clone()),
            Node::Split { first, second, .. } => {
                first.collect_leaves(out);
                second.collect_leaves(out);
            }
        }
    }

    fn ratios(&self, out: &mut Vec<f32>) {
        if let Node::Split {
            ratio, first, second, ..
        } = self
        {
            out.push(*ratio);
            first.ratios(out);
            second.ratios(out);
        }
    }

    fn pins(&self, out: &mut Vec<Pin>) {
        if let Node::Split { pin, first, second, .. } = self {
            out.extend(*pin);
            first.pins(out);
            second.pins(out);
        }
    }

    fn any_leaf(&self, shown: &dyn Fn(&ModuleId) -> bool) -> bool {
        match self {
            Node::Leaf(id) => shown(id),
            Node::Split { first, second, .. } => first.any_leaf(shown) || second.any_leaf(shown),
        }
    }

    /// A pinned split with no length yet and a `shown` leaf on both sides, anywhere below.
    fn has_pin_to_settle(&self, shown: &dyn Fn(&ModuleId) -> bool) -> bool {
        match self {
            Node::Leaf(_) => false,
            Node::Split { pin, first, second, .. } => {
                (matches!(pin, Some(Pin { px: None, .. })) && first.any_leaf(shown) && second.any_leaf(shown))
                    || first.has_pin_to_settle(shown)
                    || second.has_pin_to_settle(shown)
            }
        }
    }

    fn at_path_mut(&mut self, path: &[Branch]) -> Option<&mut Node> {
        match (path.split_first(), self) {
            (None, node) => Some(node),
            (Some((branch, rest)), Node::Split { first, second, .. }) => match branch {
                Branch::First => first.at_path_mut(rest),
                Branch::Second => second.at_path_mut(rest),
            },
            (Some(_), Node::Leaf(_)) => None,
        }
    }

    /// This tree without the leaf `id`, whose sibling takes the split's place, as a hidden module's
    /// sibling takes its space. The split goes with its ratio and its pin. `None` if `id` was the
    /// whole tree; unchanged if `id` is not in it.
    pub(super) fn without(self, id: &ModuleId) -> Option<Node> {
        match self {
            Node::Leaf(leaf) if leaf == *id => None,
            Node::Leaf(leaf) => Some(Node::Leaf(leaf)),
            Node::Split {
                axis,
                ratio,
                pin,
                first,
                second,
            } => match (first.without(id), second.without(id)) {
                (Some(first), Some(second)) => Some(Node::Split {
                    axis,
                    ratio,
                    pin,
                    first: Box::new(first),
                    second: Box::new(second),
                }),
                (Some(only), None) | (None, Some(only)) => Some(only),
                (None, None) => None,
            },
        }
    }

    /// The leaves `a` and `b` exchange places; every split keeps its axis, ratio and pin.
    pub(super) fn swap_leaves(&mut self, a: &ModuleId, b: &ModuleId) {
        match self {
            Node::Leaf(id) if id == a => *id = b.clone(),
            Node::Leaf(id) if id == b => *id = a.clone(),
            Node::Leaf(_) => {}
            Node::Split { first, second, .. } => {
                first.swap_leaves(a, b);
                second.swap_leaves(a, b);
            }
        }
    }

    /// Replaces the leaf `id` with `with(leaf)`. `false` if there is no such leaf.
    pub(super) fn replace_leaf(&mut self, id: &ModuleId, with: impl FnOnce(Node) -> Node) -> bool {
        match self {
            Node::Leaf(leaf) if leaf == id => {
                let old = std::mem::replace(self, Node::Leaf(id.clone()));
                *self = with(old);
                true
            }
            Node::Leaf(_) => false,
            Node::Split { first, second, .. } => {
                if first.leaves().contains(id) {
                    first.replace_leaf(id, with)
                } else {
                    second.replace_leaf(id, with)
                }
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum LayoutError {
    /// Invariant 1.
    Duplicate(ModuleId),
    /// A module the tree does not contain.
    NotInTree(ModuleId),
    /// A hidden module cannot hold the keys.
    Hidden(ModuleId),
    /// Invariant 3: this hide would leave nothing on screen.
    LastVisible(ModuleId),
    /// A ratio outside `MIN_RATIO..=MAX_RATIO`, or not a number.
    Ratio(f32),
    /// A path that does not lead to a split.
    NotASplit(SplitPath),
    /// A pinned length below zero.
    Pin(i32),
    /// A module cannot be placed next to itself (tmux: "source and target panes must be different").
    SameModule(ModuleId),
    /// Killed for the rest of this window (`super::kill`, `Reopen::Never`).
    Gone(ModuleId),
}

impl fmt::Display for LayoutError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LayoutError::Duplicate(id) => write!(f, "module '{id}' appears more than once"),
            LayoutError::NotInTree(id) => write!(f, "module '{id}' is not in the layout"),
            LayoutError::Hidden(id) => write!(f, "module '{id}' is hidden and cannot hold the keys"),
            LayoutError::LastVisible(id) => write!(f, "'{id}' is the last module on screen"),
            LayoutError::Ratio(r) => write!(f, "ratio {r} is outside {MIN_RATIO}..={MAX_RATIO}"),
            LayoutError::NotASplit(path) => write!(f, "{path:?} does not lead to a split"),
            LayoutError::Pin(px) => write!(f, "a pinned length of {px}px is below zero"),
            LayoutError::SameModule(id) => write!(f, "'{id}' cannot be placed next to itself"),
            LayoutError::Gone(id) => write!(f, "'{id}' was closed and cannot be reopened in this window"),
        }
    }
}

impl std::error::Error for LayoutError {}

/// What `toggle_zoom` did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZoomChange {
    Zoomed,
    Unzoomed,
    /// Nothing to zoom: the target is not on screen, or it is the only module on screen (tmux
    /// refuses to zoom a window's only pane).
    Nothing,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Layout {
    root: Node,
    hidden: BTreeSet<ModuleId>,
    zoomed: Option<ModuleId>,
    /// The module that last held the keys (`pane_focus`'s "last pane"). What `Ctrl+a m` zooms,
    /// `Ctrl+a h/j/k/l` resizes around, and the top bar's `Ctrl+j` returns to.
    focus: ModuleId,
    /// Every leaf, most recently focused first. Breaks `neighbor()`'s ties (tmux's rule).
    mru: Vec<ModuleId>,
    /// Hidden modules that can never be shown again in this window (`super::kill`'s
    /// `Reopen::Never`). A subset of `hidden`; never read from or written to a state file.
    gone: BTreeSet<ModuleId>,
    /// The gone modules that were shown when they were retired: hidden now only because what ran in
    /// them ended, not because the user hid them, so the state file writes them shown
    /// ([`Layout::saved_hidden`]). A subset of `gone`.
    retired_shown: BTreeSet<ModuleId>,
}

impl Layout {
    /// A layout over `root` with nothing hidden or zoomed. Refuses a tree that breaks invariant 1,
    /// a ratio out of range, and a `focus` that is not a leaf.
    pub fn new(root: Node, focus: ModuleId) -> Result<Layout, LayoutError> {
        let leaves = root.leaves();
        let mut seen = BTreeSet::new();
        for id in &leaves {
            if !seen.insert(id.clone()) {
                return Err(LayoutError::Duplicate(id.clone()));
            }
        }
        let mut ratios = Vec::new();
        root.ratios(&mut ratios);
        if let Some(bad) = ratios.into_iter().find(|r| !(MIN_RATIO..=MAX_RATIO).contains(r)) {
            return Err(LayoutError::Ratio(bad));
        }
        let mut pins = Vec::new();
        root.pins(&mut pins);
        if let Some(bad) = pins.into_iter().filter_map(|p| p.px).find(|px| *px < 0) {
            return Err(LayoutError::Pin(bad));
        }
        if !seen.contains(&focus) {
            return Err(LayoutError::NotInTree(focus));
        }
        let mut mru = vec![focus.clone()];
        mru.extend(leaves.into_iter().filter(|id| *id != focus));
        Ok(Layout {
            root,
            hidden: BTreeSet::new(),
            zoomed: None,
            focus,
            mru,
            gone: BTreeSet::new(),
            retired_shown: BTreeSet::new(),
        })
    }

    /// The first-launch layout (spec §4.4, §4.5): `Row(editor | agent)` at [`DEFAULT_EDITOR_SHARE`],
    /// then each of `extra` placed in order by its [`Placement`]. Focus is on the editor, or, if a
    /// module took the editor's place, on that module. A duplicate id (including `editor`/`agent`
    /// themselves) is refused.
    pub fn initial(extra: &[ModuleDecl]) -> Result<Layout, LayoutError> {
        let mut root = Node::split(
            Axis::Row,
            DEFAULT_EDITOR_SHARE,
            Node::Leaf(ModuleId::editor()),
            Node::Leaf(ModuleId::agent()),
        );
        let mut hide_editor = false;
        let mut in_editors_place: Option<ModuleId> = None;
        for decl in extra {
            if root.leaves().contains(&decl.id) {
                return Err(LayoutError::Duplicate(decl.id.clone()));
            }
            let (placed, took_editors_place) = place_new(root, &decl.id, decl.placement);
            root = placed;
            if took_editors_place {
                hide_editor = true;
                in_editors_place.get_or_insert(decl.id.clone());
            }
        }
        let focus = in_editors_place.unwrap_or_else(ModuleId::editor);
        let mut layout = Layout::new(root, focus)?;
        if hide_editor {
            layout.hide_unfocused(&ModuleId::editor())?;
        }
        Ok(layout)
    }

    /// A layout rebuilt from what a state file (or `init.lua`) holds: [`Layout::new`]'s checks, then
    /// `hidden`, which must name leaves (invariant 2) and must not include `focus` -- which is also
    /// what keeps one module on screen (invariant 3), since `focus` is a leaf. The only way back into
    /// a `Layout` from outside the process, so a file cannot skip an invariant the constructors hold
    /// (the P1 plan's warning against deriving `Deserialize` on this type).
    pub fn from_parts(root: Node, hidden: BTreeSet<ModuleId>, focus: ModuleId) -> Result<Layout, LayoutError> {
        let mut layout = Layout::new(root, focus.clone())?;
        if let Some(stray) = hidden.iter().find(|id| !layout.contains(id)) {
            return Err(LayoutError::NotInTree(stray.clone()));
        }
        if hidden.contains(&focus) {
            return Err(LayoutError::Hidden(focus));
        }
        layout.hidden = hidden;
        Ok(layout)
    }

    pub fn root(&self) -> &Node {
        &self.root
    }

    pub fn hidden(&self) -> &BTreeSet<ModuleId> {
        &self.hidden
    }

    /// What the state file keeps as hidden: [`Layout::hidden`] without the modules that were on
    /// screen when they were retired (`super::kill`'s `Reopen::Never` -- the editor after nvim
    /// ended). Their retirement is about the process, not the arrangement, so a relaunch shows them
    /// where they were (the Opus review's T6-5); one the user had hidden stays hidden.
    pub fn saved_hidden(&self) -> BTreeSet<ModuleId> {
        self.hidden.difference(&self.retired_shown).cloned().collect()
    }

    pub fn zoomed(&self) -> Option<&ModuleId> {
        self.zoomed.as_ref()
    }

    pub fn focus(&self) -> &ModuleId {
        &self.focus
    }

    pub fn mru(&self) -> &[ModuleId] {
        &self.mru
    }

    /// Killed for the rest of this window: hidden, and refused by [`Layout::show`] and
    /// `super::ops::place` (`super::kill`'s module doc says why only the editor ever is).
    pub fn is_gone(&self, id: &ModuleId) -> bool {
        self.gone.contains(id)
    }

    pub fn leaves(&self) -> Vec<ModuleId> {
        self.root.leaves()
    }

    pub fn contains(&self, id: &ModuleId) -> bool {
        self.mru.contains(id)
    }

    /// Not hidden. Ignores zoom: a module zoomed away is still "shown" in this sense, and comes back
    /// when the zoom ends.
    pub fn is_shown(&self, id: &ModuleId) -> bool {
        self.contains(id) && !self.hidden.contains(id)
    }

    /// On screen now: shown, and not zoomed away.
    pub fn is_visible(&self, id: &ModuleId) -> bool {
        self.is_shown(id) && self.zoomed.as_ref().is_none_or(|z| z == id)
    }

    /// Whether [`super::geometry::settle_pins`] has anything to do: nothing zoomed, and a pinned
    /// split with no length yet and a shown module on each side. Cheap -- no geometry -- so the grid
    /// asks it on every allocation and arranges twice only on the one where a pin settles.
    pub fn has_pin_to_settle(&self) -> bool {
        self.zoomed.is_none() && self.root.has_pin_to_settle(&|id| self.is_shown(id))
    }

    /// Every module on screen now, in tree order.
    pub fn visible_leaves(&self) -> Vec<ModuleId> {
        self.leaves().into_iter().filter(|id| self.is_visible(id)).collect()
    }

    /// `id` now holds the keys. Moves it to the front of the MRU list.
    ///
    /// **Refuses a hidden module**, so the keys never sit on one whoever reports them: `shell` writes
    /// this from whatever GTK says has focus (`pane_focus`), and GTK4's `grab_focus` does not refuse
    /// an unmapped widget -- the editor under a Lua `main` panel is one. Without this the top bar's
    /// `Ctrl+j` and the `Ctrl+a` prefix would then act on a module nobody can see. A module zoomed
    /// away is still shown and may take the keys; the move that gives them to it unzooms.
    pub fn set_focus(&mut self, id: &ModuleId) -> Result<(), LayoutError> {
        self.can_focus(id)?;
        self.focus = id.clone();
        self.mru.retain(|m| m != id);
        self.mru.insert(0, id.clone());
        Ok(())
    }

    /// Whether [`Layout::set_focus`] would take `id`: in the tree, and not hidden. What `shell` asks
    /// before it moves anything -- a refused focus must not end a zoom on its way to being refused.
    pub fn can_focus(&self, id: &ModuleId) -> Result<(), LayoutError> {
        if !self.contains(id) {
            return Err(LayoutError::NotInTree(id.clone()));
        }
        if self.hidden.contains(id) {
            return Err(LayoutError::Hidden(id.clone()));
        }
        Ok(())
    }

    /// A zoom is on, and it is not `id`'s: putting `id` on screen -- giving it the keys, showing it
    /// -- has to end it first. `false` for the zoomed module itself: tmux's `select-pane` onto the
    /// zoomed pane leaves the zoom on, and so does `Ctrl+a a` onto a zoomed chat (the whole-branch
    /// review's finding 3).
    pub fn zoom_hides(&self, id: &ModuleId) -> bool {
        self.zoomed.as_ref().is_some_and(|z| z != id)
    }

    /// `Ctrl+a m`/`z`: zoom `target`, or end the zoom if one is on (whatever `target` is, as
    /// today's `PaneLayout::toggle_zoom` does).
    pub fn toggle_zoom(&mut self, target: &ModuleId) -> ZoomChange {
        if self.unzoom() {
            return ZoomChange::Unzoomed;
        }
        let shown = self.leaves().into_iter().filter(|id| self.is_shown(id)).count();
        if !self.is_shown(target) || shown < 2 {
            return ZoomChange::Nothing;
        }
        self.zoomed = Some(target.clone());
        ZoomChange::Zoomed
    }

    /// Ends a zoom. `false` if nothing was zoomed. Never touches `hidden` (invariant 4).
    pub fn unzoom(&mut self) -> bool {
        self.zoomed.take().is_some()
    }

    /// Shows a hidden module where it was hidden from. `Ok(false)` if it was already shown.
    pub fn show(&mut self, id: &ModuleId) -> Result<bool, LayoutError> {
        if !self.contains(id) {
            return Err(LayoutError::NotInTree(id.clone()));
        }
        if self.gone.contains(id) {
            return Err(LayoutError::Gone(id.clone()));
        }
        Ok(self.hidden.remove(id))
    }

    /// Sets the ratio of the split at `path`, clamped into `MIN_RATIO..=MAX_RATIO`. A NaN changes
    /// nothing (a divider dragged by a broken pointer event must not wedge the tree). On a pinned
    /// split it also forgets the pinned length, so the ratio is what the next allocation shows and
    /// is then frozen again. A drag and `Ctrl+a h/j/k/l` go through
    /// [`super::geometry::move_divider`], which keeps a pin in pixels.
    pub fn set_ratio(&mut self, path: &[Branch], ratio: f32) -> Result<(), LayoutError> {
        let (r, pin) = self.split_at_mut(path)?;
        if !ratio.is_nan() {
            *r = ratio.clamp(MIN_RATIO, MAX_RATIO);
            if let Some(pin) = pin {
                pin.px = None;
            }
        }
        Ok(())
    }

    /// Replaces the tree with `root`, for `super::ops`, which rearranges the tree and keeps the
    /// other invariants itself. `root` must hold every module this layout holds; it may hold one
    /// more (a module placed for the first time), which joins the end of the MRU list.
    pub(super) fn replace_root(&mut self, root: Node) {
        let leaves = root.leaves();
        debug_assert!(
            self.mru.iter().all(|id| leaves.contains(id)),
            "a rearrangement lost a module"
        );
        for id in leaves {
            if !self.mru.contains(&id) {
                self.mru.push(id);
            }
        }
        self.root = root;
    }

    /// Marks the hidden module `id` gone, for `super::kill`, which has just hidden it; `was_shown`
    /// says whether it was shown before that hide ([`Layout::saved_hidden`]).
    pub(super) fn retire(&mut self, id: &ModuleId, was_shown: bool) {
        debug_assert!(self.hidden.contains(id), "only a hidden module can be gone");
        self.gone.insert(id.clone());
        if was_shown {
            self.retired_shown.insert(id.clone());
        }
    }

    /// Takes `id` out of `hidden` without the checks [`Layout::show`] makes, for `super::ops`,
    /// which has just placed it. `true` if it was hidden.
    pub(super) fn unhide(&mut self, id: &ModuleId) -> bool {
        self.hidden.remove(id)
    }

    /// The ratio and the pin of the split at `path`: what the geometry turns a divider position back
    /// into (`move_divider`, `settle_pins`).
    pub(super) fn split_at_mut(&mut self, path: &[Branch]) -> Result<(&mut f32, &mut Option<Pin>), LayoutError> {
        match self.root.at_path_mut(path) {
            Some(Node::Split { ratio, pin, .. }) => Ok((ratio, pin)),
            _ => Err(LayoutError::NotASplit(path.to_vec())),
        }
    }

    /// Hides `id`. Refuses the last shown module (invariant 3). A zoom on `id` ends with it. Does
    /// not move focus: [`super::geometry::hide`] is the entry point that does, and the only one
    /// `shell` calls. `pub(super)` so nothing outside this module can hide the focused module
    /// without choosing where the keys go first.
    pub(super) fn hide_unfocused(&mut self, id: &ModuleId) -> Result<bool, LayoutError> {
        if !self.contains(id) {
            return Err(LayoutError::NotInTree(id.clone()));
        }
        if self.hidden.contains(id) {
            return Ok(false);
        }
        let others_shown = self.leaves().iter().any(|m| m != id && self.is_shown(m));
        if !others_shown {
            return Err(LayoutError::LastVisible(id.clone()));
        }
        if self.zoomed.as_ref() == Some(id) {
            self.zoomed = None;
        }
        self.hidden.insert(id.clone());
        Ok(true)
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    fn lua(id: &str) -> ModuleId {
        ModuleId::lua(id)
    }

    fn decl(id: &str, placement: Placement) -> ModuleDecl {
        ModuleDecl { id: lua(id), placement }
    }

    /// P1's acceptance test is sameness: the default split puts the divider where `main`'s
    /// `GtkPaned` did, 760px of the 1279 the default window's 1px divider leaves.
    #[test]
    fn the_default_is_editor_then_agent_where_main_put_the_divider() {
        assert_eq!((1279.0 * DEFAULT_EDITOR_SHARE).round(), 760.0);
        let layout = Layout::initial(&[]).unwrap();
        assert_eq!(
            layout.root(),
            &Node::split(
                Axis::Row,
                DEFAULT_EDITOR_SHARE,
                Node::Leaf(ModuleId::editor()),
                Node::Leaf(ModuleId::agent())
            )
        );
        assert_eq!(layout.focus(), &ModuleId::editor());
        assert_eq!(layout.mru(), &[ModuleId::editor(), ModuleId::agent()]);
        assert!(layout.hidden().is_empty());
        assert_eq!(layout.zoomed(), None);
    }

    /// `side` goes right of the whole root and `bottom` below it, in registration order; the agent
    /// is no longer replaced by a side panel (decision 1).
    #[test]
    fn lua_positions_become_placements_in_registration_order() {
        let layout = Layout::initial(&[
            decl("right", Placement::RightOfRoot),
            decl("below", Placement::BelowRoot),
        ])
        .unwrap();
        let row = Node::split(
            Axis::Row,
            DEFAULT_EDITOR_SHARE,
            Node::Leaf(ModuleId::editor()),
            Node::Leaf(ModuleId::agent()),
        );
        assert_eq!(
            layout.root(),
            &Node::pinned(
                Axis::Column,
                BELOW_ROOT_SHARE,
                Branch::Second,
                Node::split(Axis::Row, RIGHT_OF_ROOT_SHARE, row, Node::Leaf(lua("right"))),
                Node::Leaf(lua("below"))
            )
        );
        assert_eq!(
            layout.visible_leaves(),
            vec![ModuleId::editor(), ModuleId::agent(), lua("right"), lua("below")]
        );
    }

    /// `main` keeps its visual meaning: the panel takes the editor's leaf and the editor is hidden,
    /// not gone -- it is still a leaf, so it can be shown again (P2), and focus starts on the panel.
    #[test]
    fn main_takes_the_editors_leaf_and_hides_the_editor() {
        let layout = Layout::initial(&[decl("m", Placement::InPlaceOfEditor)]).unwrap();
        assert!(layout.contains(&ModuleId::editor()));
        assert!(layout.hidden().contains(&ModuleId::editor()));
        assert_eq!(layout.visible_leaves(), vec![lua("m"), ModuleId::agent()]);
        assert_eq!(layout.focus(), &lua("m"));
    }

    /// `Placement::BelowEditor` (v1 trial item 6, 2026-09-28): with the agent right of the editor and
    /// a side panel wrapping the whole root, a module placed this way wraps the editor's own leaf
    /// only -- neither the agent nor the side panel move or lose any height.
    #[test]
    fn below_editor_wraps_only_the_editors_own_leaf() {
        let root = Node::split(
            Axis::Row,
            RIGHT_OF_ROOT_SHARE,
            Node::split(
                Axis::Row,
                DEFAULT_EDITOR_SHARE,
                Node::Leaf(ModuleId::editor()),
                Node::Leaf(ModuleId::agent()),
            ),
            Node::Leaf(lua("side")),
        );
        let (placed, took_editors_place) = place_new(root, &lua("term"), Placement::BelowEditor);
        assert!(!took_editors_place);
        assert_eq!(
            placed,
            Node::split(
                Axis::Row,
                RIGHT_OF_ROOT_SHARE,
                Node::split(
                    Axis::Row,
                    DEFAULT_EDITOR_SHARE,
                    Node::pinned(
                        Axis::Column,
                        BELOW_ROOT_SHARE,
                        Branch::Second,
                        Node::Leaf(ModuleId::editor()),
                        Node::Leaf(lua("term"))
                    ),
                    Node::Leaf(ModuleId::agent())
                ),
                Node::Leaf(lua("side")),
            )
        );
    }

    /// The default window, `[editor | agent]`: the module goes below the editor's own leaf, and the
    /// agent keeps the whole column's height -- not the tree `Placement::BelowRoot` would build,
    /// which is exactly the shape v1 trial item 6 moved the terminal away from.
    #[test]
    fn below_editor_keeps_the_agent_at_full_height_in_the_default_window() {
        let root = Node::split(
            Axis::Row,
            DEFAULT_EDITOR_SHARE,
            Node::Leaf(ModuleId::editor()),
            Node::Leaf(ModuleId::agent()),
        );
        let (placed, _) = place_new(root.clone(), &lua("term"), Placement::BelowEditor);
        assert_eq!(
            placed,
            Node::split(
                Axis::Row,
                DEFAULT_EDITOR_SHARE,
                Node::pinned(
                    Axis::Column,
                    BELOW_ROOT_SHARE,
                    Branch::Second,
                    Node::Leaf(ModuleId::editor()),
                    Node::Leaf(lua("term"))
                ),
                Node::Leaf(ModuleId::agent())
            )
        );
        let (with_below, _) = place_new(root, &lua("term"), Placement::BelowRoot);
        assert_ne!(placed, with_below, "below-editor, not below-root, is the new default");
    }

    /// The editor is gone (`super::kill::Reopen::Never`): falls back to `Placement::BelowRoot`, below
    /// the whole root -- there is no editor column left to go under.
    #[test]
    fn below_editor_falls_back_to_below_root_when_the_editor_is_not_in_the_tree() {
        let root = Node::split(
            Axis::Row,
            RIGHT_OF_ROOT_SHARE,
            Node::Leaf(ModuleId::agent()),
            Node::Leaf(lua("side")),
        );
        let (placed, _) = place_new(root.clone(), &lua("term"), Placement::BelowEditor);
        let (expected, _) = place_new(root, &lua("term"), Placement::BelowRoot);
        assert_eq!(placed, expected);
    }

    #[test]
    fn invariant_1_a_module_appears_once() {
        assert_eq!(
            Layout::initial(&[decl("x", Placement::BelowRoot), decl("x", Placement::RightOfRoot)]),
            Err(LayoutError::Duplicate(lua("x")))
        );
        let agent_again = ModuleDecl {
            id: ModuleId::agent(),
            placement: Placement::BelowRoot,
        };
        assert_eq!(
            Layout::initial(&[agent_again]),
            Err(LayoutError::Duplicate(ModuleId::agent()))
        );
        let twice = Node::split(
            Axis::Row,
            0.5,
            Node::Leaf(ModuleId::editor()),
            Node::Leaf(ModuleId::editor()),
        );
        assert_eq!(
            Layout::new(twice, ModuleId::editor()),
            Err(LayoutError::Duplicate(ModuleId::editor()))
        );
    }

    #[test]
    fn a_ratio_out_of_range_or_a_focus_outside_the_tree_is_refused() {
        let tree = |ratio| {
            Node::split(
                Axis::Row,
                ratio,
                Node::Leaf(ModuleId::editor()),
                Node::Leaf(ModuleId::agent()),
            )
        };
        for bad in [0.0, 0.049, 0.951, 1.0, f32::NAN] {
            assert!(matches!(
                Layout::new(tree(bad), ModuleId::editor()),
                Err(LayoutError::Ratio(_))
            ));
        }
        assert!(Layout::new(tree(0.05), ModuleId::editor()).is_ok());
        assert!(Layout::new(tree(0.95), ModuleId::editor()).is_ok());
        assert_eq!(
            Layout::new(tree(0.5), lua("nope")),
            Err(LayoutError::NotInTree(lua("nope")))
        );
    }

    /// Invariant 2: only a leaf can be hidden, and showing an unknown module is an error rather
    /// than a silent insert.
    #[test]
    fn invariant_2_only_leaves_are_hidden() {
        let mut layout = Layout::initial(&[]).unwrap();
        assert_eq!(
            layout.hide_unfocused(&lua("ghost")),
            Err(LayoutError::NotInTree(lua("ghost")))
        );
        assert_eq!(layout.show(&lua("ghost")), Err(LayoutError::NotInTree(lua("ghost"))));
        assert!(layout.hidden().is_empty());
    }

    /// Task 5: `LastVisible`'s wording no longer says "cannot be hidden" -- since `x` is a kill, the
    /// same text reaches `kill()`'s inner `hide` too (`on_exited_unrequested`, and until Task 6
    /// `on_shell_exit`), where a kill was never a hide.
    #[test]
    fn last_visible_reads_the_last_module_on_screen_not_cannot_be_hidden() {
        assert_eq!(
            LayoutError::LastVisible(ModuleId::editor()).to_string(),
            "'editor' is the last module on screen"
        );
    }

    #[test]
    fn invariant_3_the_last_visible_module_cannot_be_hidden() {
        let mut layout = Layout::initial(&[]).unwrap();
        assert_eq!(layout.hide_unfocused(&ModuleId::agent()), Ok(true));
        assert_eq!(
            layout.hide_unfocused(&ModuleId::editor()),
            Err(LayoutError::LastVisible(ModuleId::editor()))
        );
        assert_eq!(layout.visible_leaves(), vec![ModuleId::editor()]);
        assert_eq!(layout.hide_unfocused(&ModuleId::agent()), Ok(false), "already hidden");
    }

    /// Invariant 4, and the defect it exists for: today's `unzoom` shows every child, so a zoom
    /// ending would resurrect a module the owner closed.
    #[test]
    fn invariant_4_a_zoom_never_touches_the_hidden_set() {
        let mut layout = Layout::initial(&[decl("below", Placement::BelowRoot)]).unwrap();
        layout.hide_unfocused(&lua("below")).unwrap();
        assert_eq!(layout.toggle_zoom(&ModuleId::editor()), ZoomChange::Zoomed);
        assert_eq!(layout.visible_leaves(), vec![ModuleId::editor()]);
        assert_eq!(layout.toggle_zoom(&ModuleId::editor()), ZoomChange::Unzoomed);
        assert!(layout.hidden().contains(&lua("below")));
        assert_eq!(layout.visible_leaves(), vec![ModuleId::editor(), ModuleId::agent()]);
    }

    #[test]
    fn zoom_shows_one_module_and_any_toggle_ends_it() {
        let mut layout = Layout::initial(&[]).unwrap();
        assert_eq!(layout.toggle_zoom(&ModuleId::agent()), ZoomChange::Zoomed);
        assert_eq!(layout.zoomed(), Some(&ModuleId::agent()));
        assert_eq!(layout.visible_leaves(), vec![ModuleId::agent()]);
        assert!(layout.is_shown(&ModuleId::editor()), "zoomed away is still shown");
        assert!(!layout.is_visible(&ModuleId::editor()));
        // Today's toggle ends the zoom whichever pane it names.
        assert_eq!(layout.toggle_zoom(&ModuleId::editor()), ZoomChange::Unzoomed);
        assert_eq!(layout.zoomed(), None);
        assert!(!layout.unzoom(), "nothing left to unzoom");
    }

    #[test]
    fn nothing_zooms_a_hidden_module_or_the_only_one_on_screen() {
        let mut layout = Layout::initial(&[]).unwrap();
        layout.hide_unfocused(&ModuleId::agent()).unwrap();
        assert_eq!(layout.toggle_zoom(&ModuleId::agent()), ZoomChange::Nothing);
        assert_eq!(layout.toggle_zoom(&ModuleId::editor()), ZoomChange::Nothing);
        assert_eq!(layout.zoomed(), None);
    }

    #[test]
    fn hiding_the_zoomed_module_ends_the_zoom() {
        let mut layout = Layout::initial(&[decl("below", Placement::BelowRoot)]).unwrap();
        layout.toggle_zoom(&lua("below"));
        layout.hide_unfocused(&lua("below")).unwrap();
        assert_eq!(layout.zoomed(), None);
        assert_eq!(layout.visible_leaves(), vec![ModuleId::editor(), ModuleId::agent()]);
    }

    #[test]
    fn show_puts_a_module_back_where_it_was() {
        let mut layout = Layout::initial(&[decl("below", Placement::BelowRoot)]).unwrap();
        let before = layout.root().clone();
        layout.hide_unfocused(&lua("below")).unwrap();
        assert_eq!(layout.root(), &before, "hiding leaves the tree alone");
        assert_eq!(layout.show(&lua("below")), Ok(true));
        assert_eq!(layout.show(&lua("below")), Ok(false));
        assert_eq!(layout.visible_leaves().last(), Some(&lua("below")));
    }

    #[test]
    fn focus_moves_to_the_front_of_the_mru_list() {
        let mut layout = Layout::initial(&[decl("below", Placement::BelowRoot)]).unwrap();
        assert_eq!(layout.mru(), &[ModuleId::editor(), ModuleId::agent(), lua("below")]);
        layout.set_focus(&lua("below")).unwrap();
        layout.set_focus(&ModuleId::agent()).unwrap();
        assert_eq!(layout.focus(), &ModuleId::agent());
        assert_eq!(layout.mru(), &[ModuleId::agent(), lua("below"), ModuleId::editor()]);
        assert_eq!(
            layout.set_focus(&lua("ghost")),
            Err(LayoutError::NotInTree(lua("ghost")))
        );
        assert_eq!(layout.focus(), &ModuleId::agent());
    }

    /// The spec's P1 row: "a test that a hidden module never holds focus". `hide` moves the keys
    /// first (`geometry.rs`'s tests); this is the other door, the one `shell` writes through from
    /// whatever GTK reports.
    #[test]
    fn a_hidden_module_never_takes_the_keys() {
        let mut layout = Layout::initial(&[decl("below", Placement::BelowRoot)]).unwrap();
        layout.hide_unfocused(&lua("below")).unwrap();
        assert_eq!(layout.set_focus(&lua("below")), Err(LayoutError::Hidden(lua("below"))));
        assert_eq!(layout.focus(), &ModuleId::editor(), "the keys stay where they were");
        assert_eq!(
            layout.mru(),
            &[ModuleId::editor(), ModuleId::agent(), lua("below")],
            "a refused focus is not a use"
        );
        // Zoomed away is still shown: the move that gives it the keys unzooms.
        assert_eq!(layout.toggle_zoom(&ModuleId::editor()), ZoomChange::Zoomed);
        assert_eq!(layout.set_focus(&ModuleId::agent()), Ok(()));
        // Shown again, it can.
        layout.show(&lua("below")).unwrap();
        assert_eq!(layout.set_focus(&lua("below")), Ok(()));

        // The editor under a Lua `main` panel: hidden from the first frame.
        let mut layout = Layout::initial(&[decl("m", Placement::InPlaceOfEditor)]).unwrap();
        assert_eq!(
            layout.set_focus(&ModuleId::editor()),
            Err(LayoutError::Hidden(ModuleId::editor()))
        );
        assert_eq!(layout.focus(), &lua("m"));
    }

    /// Two `main` panels both take the editor's leaf, in registration order, and the first one
    /// registered starts with the keys: it is first in reading order, and the one a config that
    /// registers one `main` panel has always started in.
    #[test]
    fn two_main_panels_both_show_and_the_first_takes_the_keys() {
        let layout = Layout::initial(&[
            decl("m1", Placement::InPlaceOfEditor),
            decl("m2", Placement::InPlaceOfEditor),
        ])
        .unwrap();
        assert_eq!(layout.focus(), &lua("m1"));
        assert_eq!(layout.visible_leaves(), vec![lua("m1"), lua("m2"), ModuleId::agent()]);
        assert!(layout.hidden().contains(&ModuleId::editor()));
    }

    #[test]
    fn set_ratio_clamps_ignores_nan_and_refuses_a_leaf_path() {
        let mut layout = Layout::initial(&[decl("below", Placement::BelowRoot)]).unwrap();
        let ratio_at = |layout: &Layout, path: &[Branch]| {
            let mut node = layout.root();
            for branch in path {
                node = match (node, branch) {
                    (Node::Split { first, .. }, Branch::First) => first,
                    (Node::Split { second, .. }, Branch::Second) => second,
                    _ => panic!("not a split"),
                };
            }
            match node {
                Node::Split { ratio, .. } => *ratio,
                Node::Leaf(_) => panic!("a leaf"),
            }
        };
        layout.set_ratio(&[Branch::First], 0.99).unwrap();
        assert_eq!(ratio_at(&layout, &[Branch::First]), MAX_RATIO);
        layout.set_ratio(&[], 0.0).unwrap();
        assert_eq!(ratio_at(&layout, &[]), MIN_RATIO);
        layout.set_ratio(&[], f32::NAN).unwrap();
        assert_eq!(ratio_at(&layout, &[]), MIN_RATIO, "NaN changes nothing");
        assert_eq!(
            layout.set_ratio(&[Branch::Second], 0.5),
            Err(LayoutError::NotASplit(vec![Branch::Second]))
        );
    }

    /// Modules P2: a module placed below the root keeps its height (this file's doc). The pin has no
    /// length yet -- the split divides by `BELOW_ROOT_SHARE` until it has been on screen -- and a
    /// module placed right of the root, or in the editor's leaf, is not pinned.
    #[test]
    fn a_module_placed_below_the_root_is_pinned_on_its_own_side_and_nothing_else_is() {
        let layout = Layout::initial(&[decl("below", Placement::BelowRoot)]).unwrap();
        match layout.root() {
            Node::Split { axis, ratio, pin, .. } => {
                assert_eq!((*axis, *ratio), (Axis::Column, BELOW_ROOT_SHARE));
                assert_eq!(
                    *pin,
                    Some(Pin {
                        side: Branch::Second,
                        px: None
                    })
                );
            }
            Node::Leaf(_) => panic!("a leaf"),
        }
        for placement in [Placement::RightOfRoot, Placement::InPlaceOfEditor] {
            let layout = Layout::initial(&[decl("x", placement)]).unwrap();
            let mut pins = Vec::new();
            layout.root().pins(&mut pins);
            assert!(pins.is_empty(), "{placement:?}: {pins:?}");
        }
    }

    /// `set_ratio` on a pinned split forgets the pinned length: the ratio is what shows next.
    #[test]
    fn set_ratio_forgets_a_pinned_length() {
        let mut layout = Layout::initial(&[decl("below", Placement::BelowRoot)]).unwrap();
        *layout.split_at_mut(&[]).unwrap().1 = Some(Pin {
            side: Branch::Second,
            px: Some(240),
        });
        layout.set_ratio(&[], 0.5).unwrap();
        assert_eq!(
            layout.split_at_mut(&[]).unwrap(),
            (
                &mut 0.5,
                &mut Some(Pin {
                    side: Branch::Second,
                    px: None
                })
            )
        );
    }

    /// The door a state file comes back through (P1 plan, deferred item 11): every check
    /// `Layout::new` makes, and the three `hidden` can break.
    #[test]
    fn from_parts_rebuilds_a_layout_and_refuses_what_breaks_an_invariant() {
        let root = || {
            Node::pinned(
                Axis::Column,
                BELOW_ROOT_SHARE,
                Branch::Second,
                Node::split(
                    Axis::Row,
                    0.5,
                    Node::Leaf(ModuleId::editor()),
                    Node::Leaf(ModuleId::agent()),
                ),
                Node::Leaf(ModuleId::terminal()),
            )
        };
        let hidden = |ids: &[ModuleId]| ids.iter().cloned().collect::<BTreeSet<_>>();

        let layout = Layout::from_parts(root(), hidden(&[ModuleId::terminal()]), ModuleId::agent()).unwrap();
        assert_eq!(layout.visible_leaves(), [ModuleId::editor(), ModuleId::agent()]);
        assert_eq!(layout.focus(), &ModuleId::agent());
        assert_eq!(layout.zoomed(), None);

        assert_eq!(
            Layout::from_parts(root(), hidden(&[lua("gone")]), ModuleId::agent()),
            Err(LayoutError::NotInTree(lua("gone")))
        );
        assert_eq!(
            Layout::from_parts(root(), hidden(&[ModuleId::agent()]), ModuleId::agent()),
            Err(LayoutError::Hidden(ModuleId::agent()))
        );
        assert_eq!(
            Layout::from_parts(
                root(),
                hidden(&[ModuleId::editor(), ModuleId::agent(), ModuleId::terminal()]),
                ModuleId::editor()
            ),
            Err(LayoutError::Hidden(ModuleId::editor())),
            "everything hidden is refused through the focus: it is always a leaf"
        );
        let mut bad_pin = root();
        if let Node::Split { pin, .. } = &mut bad_pin {
            *pin = Some(Pin {
                side: Branch::Second,
                px: Some(-1),
            });
        }
        assert_eq!(
            Layout::from_parts(bad_pin, BTreeSet::new(), ModuleId::editor()),
            Err(LayoutError::Pin(-1))
        );
    }

    /// What `shell` asks before a focus or a show moves anything: a module the keys cannot go to is
    /// refused first, and a zoom ends only for a module it keeps off screen -- never for the zoomed
    /// module itself.
    #[test]
    fn a_zoom_ends_only_for_a_module_it_keeps_off_screen() {
        let mut layout = Layout::initial(&[decl("side", Placement::RightOfRoot)]).unwrap();
        assert!(!layout.zoom_hides(&ModuleId::agent()), "nothing zoomed");
        layout.toggle_zoom(&ModuleId::agent());
        assert!(!layout.zoom_hides(&ModuleId::agent()), "the zoomed module itself");
        assert!(layout.zoom_hides(&ModuleId::editor()));
        layout.hide_unfocused(&lua("side")).unwrap();
        assert!(
            layout.zoom_hides(&lua("side")),
            "shown later, it would still be off screen"
        );
        assert_eq!(layout.can_focus(&lua("side")), Err(LayoutError::Hidden(lua("side"))));
        assert_eq!(layout.can_focus(&lua("gone")), Err(LayoutError::NotInTree(lua("gone"))));
        assert_eq!(layout.can_focus(&ModuleId::editor()), Ok(()));
    }
}
