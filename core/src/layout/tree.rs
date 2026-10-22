//! The layout as data: a tree of containers whose leaves are modules ([`super::con`]), plus
//! which modules are hidden, which one is zoomed, and which one last held the keys.
//! The root is always a container, even for a single module. A root holding only another container
//! is replaced by it. Pure and GTK-free: `shell`'s `ModuleGrid` allocates what
//! [`super::geometry::arrange`] computes from it.
//!
//! The invariants enforced here and tested below are:
//! 1. each `ModuleId` appears at most once in the tree;
//! 2. `hidden` names only leaves in the tree;
//! 3. at least one leaf is visible; hiding the last visible leaf is refused;
//! 4. zoom never changes `hidden`, so ending a zoom cannot show a module that was hidden.
//!
//! Every container below the root has at least two children. Each share is inside `MIN_RATIO..=1`,
//! and a container's shares sum to 1 ([`super::con::Con::check`]). `focus` and every `mru` entry are
//! leaves. Focus never rests on a hidden module: [`Layout::set_focus`] refuses one, and
//! [`super::geometry::hide`] moves focus before hiding the module that holds it.
//!
//! A container divides by its children's shares unless a child is pinned. A [`super::con::Pin`]
//! keeps that child's length in pixels when the available space changes. Modules placed below the
//! whole root ([`Placement::BelowRoot`], a Lua `bottom` panel) or below the editor's leaf
//! ([`Placement::BelowEditor`], the default terminal) are pinned, including rows that are not full
//! width. On first show, the share gives either module a third of the available height;
//! [`super::geometry::settle_pins`] then freezes that length.
//!
//! The mark, last preset and undo slot are runtime state; no state file reads or writes them.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use super::arrange::{con_has_pin_to_settle, has_step, set_step_ratio, Divider};
use super::con::{Child, Con, ConFault, ConId, ConLayout, Node, MAX_CHILDREN};
use super::float::{Float, FloatRect};
use super::module::{ModuleDecl, ModuleId, Placement};
use super::verbs::Preset;

/// A split never gives either side less than this share.
pub const MIN_RATIO: f32 = 0.05;
pub const MAX_RATIO: f32 = 0.95;

/// The editor's share of the default `Row(editor | agent)` preserves a 760px editor in a
/// 1280px window with a 1px divider, leaving 1279px to divide. The ratio scales with window width.
/// The previous `GtkPaned` also scaled after its first allocation, but kept its initial 760px
/// position on a first allocation at another width, such as a maximised or tiled launch.
/// A share of 0.6 would give 767px in the default window.
pub const DEFAULT_EDITOR_SHARE: f32 = 760.0 / 1279.0;

/// The share the existing root keeps when a module is placed below it.
/// The previous bottom split started 480px down. With a 39px top bar (`min-height: 38px` plus
/// its 1px border), the default content height is 721px, or 720px after the divider.
/// 480 of 720 is two thirds. These dimensions come from the CSS rather than measurement.
pub const BELOW_ROOT_SHARE: f32 = 2.0 / 3.0;

/// The share the existing root keeps when a module is placed to its right. Nothing today does
/// this -- a Lua `side` panel used to REPLACE the agent -- so this is new: the new module gets
/// a third of the width.
pub const RIGHT_OF_ROOT_SHARE: f32 = 0.67;

/// The split between a module placed in the editor's leaf and the hidden editor.
/// It collapses until the editor is shown again, letting the module fill the leaf.
const IN_PLACE_SHARE: f32 = 0.5;

/// Puts module `id`, which `root` does not contain, where `placement` says. `true` when
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

#[derive(Debug, Clone, PartialEq)]
pub enum LayoutError {
    /// Invariant 1.
    Duplicate(ModuleId),
    /// A module the tree does not contain.
    NotInTree(ModuleId),
    NotAnInstance(ModuleId),
    /// A hidden module cannot hold the keys.
    Hidden(ModuleId),
    /// Invariant 3: this hide would leave nothing on screen.
    LastVisible(ModuleId),
    /// A ratio outside `MIN_RATIO..=MAX_RATIO`, or a share below `MIN_RATIO`, or not a number.
    Ratio(f32),
    /// A divider that is no longer on screen: its container is gone, or the children on either side
    /// of it are not both shown any more.
    NoSuchDivider,
    /// A container whose shares do not add up to 1.
    ShareSum(f32),
    /// A container with no children, or one below the root with a single child.
    Unnormalised,
    /// A pinned length below zero.
    Pin(i32),
    /// A module cannot be placed next to itself (tmux: "source and target panes must be different").
    SameModule(ModuleId),
    /// Killed for the rest of this window (`super::kill`, `Reopen::Never`).
    Gone(ModuleId),
    /// The other window of the pair holds it now ([`Layout::is_away`]).
    Away(ModuleId),
    NoMark,
    MarkedIsTarget(ModuleId),
    MarkHidden(ModuleId),
    TooMany {
        limit: usize,
    },
    NothingToUndo,
    EditorNeverFloats,
    Floating(ModuleId),
    NotFloating(ModuleId),
    NoScratch,
    LastTiled(ModuleId),
    FloatRect(ModuleId),
}

impl fmt::Display for LayoutError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LayoutError::EditorNeverFloats => write!(f, "the editor never floats"),
            LayoutError::Floating(id) => write!(f, "'{id}' is floating; it has no split"),
            LayoutError::NotFloating(id) => write!(f, "'{id}' is not floating"),
            LayoutError::NoScratch => write!(f, "the scratchpad is empty"),
            LayoutError::LastTiled(id) => write!(f, "'{id}' is the last module tiled on screen"),
            LayoutError::FloatRect(id) => write!(f, "the rectangle of floating '{id}' is out of range"),
            LayoutError::NotAnInstance(id) => write!(f, "'{id}' is not an instance; it cannot leave the layout"),
            LayoutError::NoMark => write!(f, "no module is marked"),
            LayoutError::MarkedIsTarget(id) => write!(f, "'{id}' is the marked module itself"),
            LayoutError::MarkHidden(id) => write!(f, "the marked module '{id}' is hidden"),
            LayoutError::TooMany { limit } => write!(f, "more than {limit} modules on screen"),
            LayoutError::NothingToUndo => write!(f, "nothing to undo"),
            LayoutError::Duplicate(id) => write!(f, "module '{id}' appears more than once"),
            LayoutError::NotInTree(id) => write!(f, "module '{id}' is not in the layout"),
            LayoutError::Hidden(id) => write!(f, "module '{id}' is hidden and cannot hold the keys"),
            LayoutError::LastVisible(id) => write!(f, "'{id}' is the last module on screen"),
            LayoutError::Ratio(r) => write!(f, "ratio {r} is outside {MIN_RATIO}..={MAX_RATIO}"),
            LayoutError::NoSuchDivider => write!(f, "that divider is no longer on screen"),
            LayoutError::ShareSum(sum) => write!(f, "a container's shares add up to {sum}, not 1"),
            LayoutError::Unnormalised => write!(f, "a container holds no children, or only one below the root"),
            LayoutError::Pin(px) => write!(f, "a pinned length of {px}px is below zero"),
            LayoutError::SameModule(id) => write!(f, "'{id}' cannot be placed next to itself"),
            LayoutError::Gone(id) => write!(f, "'{id}' was closed and cannot be reopened in this window"),
            LayoutError::Away(id) => write!(f, "'{id}' is in the other window"),
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Selection {
    Module,
    Con(ConId),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Zoom {
    Module(ModuleId),
    Con(ConId),
}

/// Detaching a tile would leave no tiled tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Emptied;

#[derive(Debug, Clone, PartialEq)]
pub struct Layout {
    root: Con,
    floats: Vec<Float>,
    float_memory: BTreeMap<ModuleId, FloatRect>,
    mark: Option<ModuleId>,
    last_preset: Option<Preset>,
    undo: Option<Con>,
    hidden: BTreeSet<ModuleId>,
    zoomed: Option<Zoom>,
    selection: Selection,
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
    /// Modules the other window of the pair holds now: hidden here, each leaf (or float) left where
    /// it was, so one that comes back is in its old place. Like a gone module they cannot be shown,
    /// placed, marked or killed here, and unlike one they come back ([`Layout::set_away`]). A subset
    /// of `hidden`.
    away: BTreeSet<ModuleId>,
    /// The away modules that were shown when they left: coming back shows them again, and the state
    /// file writes them shown ([`Layout::saved_hidden`]). A subset of `away`.
    away_shown: BTreeSet<ModuleId>,
}

impl Layout {
    /// A layout over `root` with nothing hidden or zoomed. A module alone becomes a root of one child,
    /// and a root holding only another container is that container. Refuses a tree that breaks
    /// invariant 1, a container [`Con::check`] refuses, and a `focus` that is not a leaf.
    pub fn new(root: Node, focus: ModuleId) -> Result<Layout, LayoutError> {
        let mut root = into_root(root, None);
        let leaves = root.leaves();
        let mut seen = BTreeSet::new();
        for id in &leaves {
            if !seen.insert(id.clone()) {
                return Err(LayoutError::Duplicate(id.clone()));
            }
        }
        root.check(true).map_err(|fault| match fault {
            ConFault::Share(share) => LayoutError::Ratio(share),
            ConFault::ShareSum(sum) => LayoutError::ShareSum(sum),
            ConFault::Empty | ConFault::OneChild | ConFault::Focus(_) => LayoutError::Unnormalised,
            ConFault::Pin(px) => LayoutError::Pin(px),
            ConFault::TooMany(_) => LayoutError::TooMany { limit: MAX_CHILDREN },
        })?;
        if !seen.contains(&focus) {
            return Err(LayoutError::NotInTree(focus));
        }
        root.focus_leaf(&focus);
        let mut mru = vec![focus.clone()];
        mru.extend(leaves.into_iter().filter(|id| *id != focus));
        Ok(Layout {
            root,
            floats: Vec::new(),
            float_memory: BTreeMap::new(),
            mark: None,
            last_preset: None,
            undo: None,
            hidden: BTreeSet::new(),
            zoomed: None,
            selection: Selection::Module,
            focus,
            mru,
            gone: BTreeSet::new(),
            retired_shown: BTreeSet::new(),
            away: BTreeSet::new(),
            away_shown: BTreeSet::new(),
        })
    }

    /// The companion panel's layout: the agent as the only leaf, focused. There is no editor module
    /// here -- the user's own nvim is the editor -- so this is built with [`Layout::new`], which accepts
    /// a one-leaf tree, and never with [`Layout::initial`] or a reconcile against the saved state.
    pub fn companion() -> Layout {
        Layout::new(Node::Leaf(ModuleId::agent()), ModuleId::agent()).expect("a one-leaf layout is valid")
    }

    /// The first-launch layout: `Row(editor | agent)` at [`DEFAULT_EDITOR_SHARE`],
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

    /// A layout rebuilt from what a state file or `init.lua` holds: [`Layout::new`]'s checks,
    /// then `hidden`, which must name layout modules and must not include `focus`. Since focus
    /// belongs to the layout, this also keeps at least one module shown. This and its floating
    /// counterpart are the only way back into a `Layout` from outside the process, so a file
    /// cannot skip an invariant the constructors hold by deserialising the model directly.
    /// This tiled entry point delegates with empty floats and float memory.
    pub fn from_parts(root: Node, hidden: BTreeSet<ModuleId>, focus: ModuleId) -> Result<Layout, LayoutError> {
        Self::from_parts_floating(root, Vec::new(), hidden, focus, BTreeMap::new())
    }

    /// Rebuilds a layout with floats in the given order, without raising the focused float.
    /// Extends [`Layout::from_parts`]'s checks to float membership, rectangles and memory;
    /// hidden modules and focus may belong to either layer.
    pub fn from_parts_floating(
        root: Node,
        floats: Vec<Float>,
        hidden: BTreeSet<ModuleId>,
        focus: ModuleId,
        memory: BTreeMap<ModuleId, FloatRect>,
    ) -> Result<Layout, LayoutError> {
        let root = into_root(root, None);
        let leaves = root.leaves();
        let first = leaves.first().cloned().unwrap_or_else(|| focus.clone());
        let mut layout = Layout::new(Node::Con(root.clone()), first)?;
        layout.root = root;
        let mut modules: BTreeSet<_> = leaves.iter().cloned().collect();
        for float in &floats {
            if float.id == ModuleId::editor() {
                return Err(LayoutError::EditorNeverFloats);
            }
            if !modules.insert(float.id.clone()) {
                return Err(LayoutError::Duplicate(float.id.clone()));
            }
            if !float.rect.is_valid() {
                return Err(LayoutError::FloatRect(float.id.clone()));
            }
        }
        for id in memory.keys() {
            if *id == ModuleId::editor() {
                return Err(LayoutError::EditorNeverFloats);
            }
            if !modules.contains(id) {
                return Err(LayoutError::NotInTree(id.clone()));
            }
        }
        if let Some(stray) = hidden.iter().find(|id| !modules.contains(id)) {
            return Err(LayoutError::NotInTree(stray.clone()));
        }
        if !modules.contains(&focus) {
            return Err(LayoutError::NotInTree(focus));
        }
        if hidden.contains(&focus) {
            return Err(LayoutError::Hidden(focus));
        }
        layout.floats = floats;
        layout.float_memory = memory;
        layout.hidden = hidden;
        layout.mru = layout.modules();
        layout.mru.retain(|id| *id != focus);
        layout.mru.insert(0, focus.clone());
        layout.root.focus_leaf(&focus);
        layout.focus = focus;
        Ok(layout)
    }

    pub fn root(&self) -> &Con {
        &self.root
    }

    /// The tree as a node, for the verbs that rebuild it: the root container, or the one module a root
    /// of one child holds (so a split around it never nests a container of one child).
    pub(super) fn root_node(&self) -> Node {
        match self.root.children.as_slice() {
            [only] if self.root.layout.axis().is_some() => only.node.clone(),
            _ => Node::Con(self.root.clone()),
        }
    }

    /// Returns the tree without a tile, leaving this layout unchanged until its caller accepts it.
    pub(crate) fn detach_tile(&mut self, id: &ModuleId) -> Result<Node, Emptied> {
        let rest = Node::Con(self.root.clone()).without(id).ok_or(Emptied)?;
        match rest {
            Node::Con(mut c) if c.children.len() == 1 && c.layout.axis().is_some() => Ok(c.children.remove(0).node),
            rest => Ok(rest),
        }
    }

    /// The root container, for the verbs that change shares, pins and layouts in place.
    pub(super) fn root_mut(&mut self) -> &mut Con {
        &mut self.root
    }

    /// Every module not hidden: what a change made through [`Layout::root_mut`] reads while it holds
    /// the tree.
    pub(super) fn shown_set(&self) -> BTreeSet<ModuleId> {
        self.leaves().into_iter().filter(|id| self.is_shown(id)).collect()
    }

    pub fn hidden(&self) -> &BTreeSet<ModuleId> {
        &self.hidden
    }

    /// The saved hidden set excludes modules that were shown when retired, such as the editor
    /// after nvim ended (`super::kill`'s `Reopen::Never`). Retirement ends the process without
    /// changing the saved arrangement, so relaunch shows those modules where they were.
    /// A module hidden by the user stays hidden. A module moved to another window is likewise
    /// saved as it was when it left; its absence does not change this window's saved arrangement.
    pub fn saved_hidden(&self) -> BTreeSet<ModuleId> {
        self.hidden
            .iter()
            .filter(|id| !self.retired_shown.contains(*id) && !self.away_shown.contains(*id))
            .cloned()
            .collect()
    }

    pub fn zoomed(&self) -> Option<&ModuleId> {
        match self.zoom() {
            Some(Zoom::Module(id)) => Some(id),
            _ => None,
        }
    }

    pub fn zoom(&self) -> Option<&Zoom> {
        self.zoomed.as_ref()
    }

    pub fn is_zoomed(&self) -> bool {
        self.zoomed.is_some()
    }

    pub fn selection(&self) -> Selection {
        self.selection
    }

    pub fn clear_selection(&mut self) -> bool {
        let selected = self.selection != Selection::Module;
        self.selection = Selection::Module;
        selected
    }

    pub(super) fn select(&mut self, selection: Selection) {
        self.selection = selection;
    }

    pub(super) fn selected_container(&self, target: &ModuleId) -> Option<ConId> {
        match self.selection {
            Selection::Module => self.container_of(target),
            Selection::Con(id) => Some(id),
        }
    }

    fn retain_selection(&mut self) {
        if let Selection::Con(id) = self.selection {
            if self.root.find(id).is_none() {
                self.clear_selection();
            }
        }
        if let Some(Zoom::Con(id)) = self.zoom() {
            if self
                .root
                .find(*id)
                .is_none_or(|c| !c.leaves().iter().any(|m| self.is_shown(m)))
            {
                self.unzoom();
            }
        }
    }

    pub fn focus(&self) -> &ModuleId {
        &self.focus
    }

    pub fn mark(&self) -> Option<&ModuleId> {
        self.mark.as_ref()
    }

    pub fn set_mark(&mut self, id: &ModuleId) -> Result<(), LayoutError> {
        if !self.contains(id) {
            return Err(LayoutError::NotInTree(id.clone()));
        }
        if self.is_gone(id) {
            return Err(LayoutError::Gone(id.clone()));
        }
        if self.is_away(id) {
            return Err(LayoutError::Away(id.clone()));
        }
        self.mark = Some(id.clone());
        Ok(())
    }

    pub fn clear_mark(&mut self) -> bool {
        self.mark.take().is_some()
    }

    pub fn toggle_mark(&mut self, id: &ModuleId) -> Result<bool, LayoutError> {
        if self.mark.as_ref() == Some(id) {
            self.clear_mark();
            Ok(false)
        } else {
            self.set_mark(id)?;
            Ok(true)
        }
    }

    pub fn last_preset(&self) -> Option<Preset> {
        self.last_preset
    }

    pub(super) fn set_last_preset(&mut self, preset: Preset) {
        self.last_preset = Some(preset);
    }

    pub(super) fn clear_last_preset(&mut self) {
        self.last_preset = None;
    }

    pub fn undo(&mut self) -> Result<(), LayoutError> {
        let Some(mut old) = self.undo.take() else {
            return Err(LayoutError::NothingToUndo);
        };
        let mut before = old.leaves();
        let mut now = self.leaves();
        before.sort();
        now.sort();
        if before != now {
            return Err(LayoutError::NothingToUndo);
        }
        self.unzoom();
        std::mem::swap(&mut old, &mut self.root);
        self.undo = Some(old);
        self.retain_selection();
        self.set_focus(&self.focus.clone())?;
        self.last_preset = None;
        Ok(())
    }

    pub fn mru(&self) -> &[ModuleId] {
        &self.mru
    }

    /// Killed for the rest of this window: hidden, and refused by [`Layout::show`] and
    /// `super::ops::place` (`super::kill`'s module doc says why only the editor ever is).
    pub fn is_gone(&self, id: &ModuleId) -> bool {
        self.gone.contains(id)
    }

    /// Held by the other window of the pair: hidden here, its place kept ([`Layout::set_away`]).
    pub fn is_away(&self, id: &ModuleId) -> bool {
        self.away.contains(id)
    }

    /// Every module away in the other window. [`Layout::modules`] still lists them: their leaves are
    /// here, and so are their numbers.
    pub fn away(&self) -> BTreeSet<ModuleId> {
        self.away.clone()
    }

    /// Marks `id` away (`true`) or back (`false`); `Ok(false)` when it already was. Away, it stays in
    /// the tree or among the floats exactly where it is, hidden, and nothing here can show it; back,
    /// it is shown again if it was shown when it left. A module leaving takes no keys and no mark
    /// with it: the keys go to the most recent module on screen. When it was the only module on
    /// screen the chat is shown in its place (the editor if the chat is away too), as a retirement
    /// does, since a window always shows something; `LastVisible` if there is nothing else at all.
    pub fn set_away(&mut self, id: &ModuleId, away: bool) -> Result<bool, LayoutError> {
        if !self.contains(id) {
            return Err(LayoutError::NotInTree(id.clone()));
        }
        if !away {
            if !self.away.remove(id) {
                return Ok(false);
            }
            if self.away_shown.remove(id) {
                self.hidden.remove(id);
            }
            if self.zoomed().is_some_and(|z| self.is_tabbed_away(z)) {
                self.unzoom();
            }
            return Ok(true);
        }
        if self.away.contains(id) {
            return Ok(false);
        }
        let was_shown = !self.hidden.contains(id);
        if was_shown && !self.modules().iter().any(|m| m != id && self.is_shown(m)) {
            let stand_in = [ModuleId::agent(), ModuleId::editor()]
                .into_iter()
                .chain(self.modules())
                .find(|m| m != id && self.contains(m) && !self.is_gone(m) && !self.is_away(m))
                .ok_or_else(|| LayoutError::LastVisible(id.clone()))?;
            self.hidden.remove(&stand_in);
        }
        if self.zoomed() == Some(id) {
            self.zoomed = None;
        }
        self.hidden.insert(id.clone());
        self.away.insert(id.clone());
        if was_shown {
            self.away_shown.insert(id.clone());
        }
        if self.mark.as_ref() == Some(id) {
            self.mark = None;
        }
        self.retain_selection();
        if &self.focus == id {
            // On screen first: a module merely not tabbed away may sit outside a container zoom that
            // outlived the leaving module, and the keys would then land where nothing is drawn.
            let next = self
                .mru
                .iter()
                .find(|m| self.is_visible(m))
                .or_else(|| self.mru.iter().find(|m| self.is_in_view(m)))
                .or_else(|| self.mru.iter().find(|m| self.is_shown(m)))
                .cloned()
                .expect("a module stays shown");
            if self.zoom_hides(&next) {
                self.unzoom();
            }
            self.set_focus(&next)?;
        }
        Ok(true)
    }

    /// The tiled tree's leaves, in tree order; floats are not leaves ([`Layout::modules`]).
    pub fn leaves(&self) -> Vec<ModuleId> {
        self.root.leaves()
    }

    pub fn floats(&self) -> &[Float] {
        &self.floats
    }

    pub fn float_of(&self, id: &ModuleId) -> Option<&Float> {
        self.floats.iter().find(|float| &float.id == id)
    }

    pub fn is_floating(&self, id: &ModuleId) -> bool {
        self.float_of(id).is_some()
    }

    /// A hidden scratch float is stowed in the scratchpad.
    /// An away float is not: the scratchpad cannot bring up what the other window holds.
    pub fn is_stowed(&self, id: &ModuleId) -> bool {
        self.hidden.contains(id) && !self.away.contains(id) && self.float_of(id).is_some_and(|float| float.scratch)
    }

    /// Every module: tiled leaves in tree order, then floats bottom first.
    pub fn modules(&self) -> Vec<ModuleId> {
        let mut modules = self.leaves();
        modules.extend(self.floats.iter().map(|float| float.id.clone()));
        modules
    }

    /// Visible tiled leaves, then visible floats bottom first.
    pub fn visible_modules(&self) -> Vec<ModuleId> {
        self.modules().into_iter().filter(|id| self.is_visible(id)).collect()
    }

    pub fn float_memory(&self) -> &BTreeMap<ModuleId, FloatRect> {
        &self.float_memory
    }

    /// Remembers the rectangle `id` floats at when it next floats. Refused for the editor, which never
    /// floats, for a module this layout does not hold, and for a rectangle out of range.
    pub(crate) fn remember_float(&mut self, id: &ModuleId, rect: FloatRect) -> Result<(), LayoutError> {
        if *id == ModuleId::editor() {
            return Err(LayoutError::EditorNeverFloats);
        }
        if !self.contains(id) {
            return Err(LayoutError::NotInTree(id.clone()));
        }
        if !rect.is_valid() {
            return Err(LayoutError::FloatRect(id.clone()));
        }
        self.float_memory.insert(id.clone(), rect);
        Ok(())
    }

    /// The most recently used tiled module that is in view.
    pub fn tiled_anchor(&self) -> Option<ModuleId> {
        self.mru
            .iter()
            .find(|id| !self.is_floating(id) && self.is_in_view(id))
            .cloned()
    }

    /// Adds a new, shown float on top, joining the end of the MRU list.
    pub fn add_float(&mut self, id: &ModuleId, rect: FloatRect, scratch: bool) -> Result<(), LayoutError> {
        if *id == ModuleId::editor() {
            return Err(LayoutError::EditorNeverFloats);
        }
        if self.contains(id) {
            return Err(LayoutError::Duplicate(id.clone()));
        }
        if !rect.is_valid() {
            return Err(LayoutError::FloatRect(id.clone()));
        }
        self.floats.push(Float {
            id: id.clone(),
            rect,
            scratch,
        });
        self.mru.push(id.clone());
        Ok(())
    }

    /// Removes a float, remembering its rectangle and leaving its hidden state alone.
    pub(crate) fn take_float(&mut self, id: &ModuleId) -> Option<Float> {
        let index = self.floats.iter().position(|float| &float.id == id)?;
        let float = self.floats.remove(index);
        self.float_memory.insert(id.clone(), float.rect);
        Some(float)
    }

    /// Puts an existing module above the other floats after it has left the tree.
    pub(crate) fn push_float(&mut self, float: Float) {
        debug_assert!(self.contains(&float.id));
        debug_assert!(!self.is_floating(&float.id) && float.id != ModuleId::editor() && float.rect.is_valid());
        self.floats.push(float);
    }

    /// Puts the float `id` at the top of the z-order: `false` when it is not a float or is on top already.
    /// Public because a press on a float's bar raises it before anything gives it the keys.
    pub fn raise(&mut self, id: &ModuleId) -> bool {
        let Some(index) = self.floats.iter().position(|float| &float.id == id) else {
            return false;
        };
        if index + 1 == self.floats.len() {
            return false;
        }
        let float = self.floats.remove(index);
        self.floats.push(float);
        true
    }

    pub(crate) fn lower(&mut self, id: &ModuleId) -> bool {
        let Some(index) = self.floats.iter().position(|float| &float.id == id) else {
            return false;
        };
        if index == 0 {
            return false;
        }
        let float = self.floats.remove(index);
        self.floats.insert(0, float);
        true
    }

    pub(crate) fn store_float_rect(&mut self, id: &ModuleId, rect: FloatRect) -> bool {
        let Some(float) = self.floats.iter_mut().find(|float| &float.id == id) else {
            return false;
        };
        if float.rect == rect {
            return false;
        }
        float.rect = rect;
        true
    }

    pub(crate) fn set_scratch(&mut self, id: &ModuleId, scratch: bool) -> bool {
        let Some(float) = self.floats.iter_mut().find(|float| &float.id == id) else {
            return false;
        };
        if float.scratch == scratch {
            return false;
        }
        float.scratch = scratch;
        true
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
        if !self.is_shown(id) || self.zoom_hides(id) {
            return false;
        }
        let ancestors = self.ancestors(id);
        let start = match self.zoom() {
            Some(Zoom::Con(z)) => ancestors
                .iter()
                .position(|(c, _)| c.id == *z)
                .unwrap_or(ancestors.len()),
            Some(Zoom::Module(_)) => return true,
            None => 0,
        };
        ancestors[start..]
            .iter()
            .all(|(c, i)| c.layout.axis().is_some() || c.drawn(&|m| self.is_shown(m)) == Some(*i))
    }

    pub(crate) fn ancestors(&self, id: &ModuleId) -> Vec<(&Con, usize)> {
        let mut out = Vec::new();
        let mut c = &self.root;
        while let Some(i) = c.children.iter().position(|ch| ch.node.leaves().contains(id)) {
            out.push((c, i));
            match &c.children[i].node {
                Node::Con(inner) => c = inner,
                Node::Leaf(_) => break,
            }
        }
        out
    }

    pub fn is_tabbed_away(&self, id: &ModuleId) -> bool {
        self.is_shown(id)
            && self
                .ancestors(id)
                .iter()
                .any(|(c, i)| c.layout.axis().is_none() && c.drawn(&|m| self.is_shown(m)) != Some(*i))
    }

    /// Whether `id` has its own entry on a tab strip that is on screen now: a tabbed or stacked
    /// container above it is drawn, and the entry for the child holding `id` names `id` (a child that
    /// is itself a container is named by the module it would draw). A module on an inactive tab of a
    /// container that is itself on an inactive tab has no such entry, and neither does anything
    /// under a zoom that leaves its strip off screen.
    pub fn is_titled_on_strip(&self, id: &ModuleId) -> bool {
        if !self.is_shown(id) {
            return false;
        }
        let shown = |m: &ModuleId| self.is_shown(m);
        let ancestors = self.ancestors(id);
        let start = match self.zoom() {
            Some(Zoom::Con(z)) => match ancestors.iter().position(|(c, _)| c.id == *z) {
                Some(at) => at,
                None => return false,
            },
            Some(Zoom::Module(_)) => return false,
            None => 0,
        };
        for (c, i) in &ancestors[start..] {
            if c.layout.axis().is_some() {
                continue;
            }
            if c.children[*i].node.focused_leaf(&shown).as_ref() == Some(id) {
                return true;
            }
            if c.drawn(&shown) != Some(*i) {
                return false;
            }
        }
        false
    }

    pub fn is_in_view(&self, id: &ModuleId) -> bool {
        self.is_shown(id) && !self.is_tabbed_away(id)
    }

    pub fn container_of(&self, id: &ModuleId) -> Option<super::con::ConId> {
        self.ancestors(id).into_iter().rev().find_map(|(c, i)| {
            c.children
                .iter()
                .enumerate()
                .any(|(j, ch)| j != i && ch.node.leaves().iter().any(|m| self.is_shown(m)))
                .then_some(c.id)
        })
    }

    pub fn bring_forward(&mut self, id: &ModuleId) -> bool {
        if !self.is_shown(id) {
            return false;
        }
        let old = self.root.clone();
        self.root.focus_leaf(id);
        if !self.is_visible(&self.focus) {
            self.root = old;
            return false;
        }
        true
    }

    pub fn tab_neighbour(&self, id: &ModuleId, forward: bool) -> Option<ModuleId> {
        let (c, i) = self
            .ancestors(id)
            .into_iter()
            .rev()
            .find(|(c, _)| c.layout.axis().is_none())?;
        let n = c.children.len();
        (1..n)
            .map(|offset| (i + if forward { offset } else { n - offset }) % n)
            .find_map(|j| c.children[j].node.focused_leaf(&|m| self.is_shown(m)))
    }

    /// Whether [`super::geometry::settle_pins`] has anything to do: nothing zoomed, and a pinned
    /// split with no length yet and a shown module on each side. Cheap -- no geometry -- so the grid
    /// asks it on every allocation and arranges twice only on the one where a pin settles.
    pub fn has_pin_to_settle(&self) -> bool {
        self.zoomed.is_none() && con_has_pin_to_settle(&self.root, &|id| self.is_shown(id))
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
        self.raise(id);
        self.root.focus_leaf(id);
        if self.zoomed().is_some_and(|z| self.is_tabbed_away(z)) {
            self.unzoom();
        }
        if self.focus != *id {
            self.clear_selection();
        }
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

    /// Whether a zoom hides `id`: showing or focusing another module must end the zoom first.
    /// Returns `false` for the zoomed module itself, so focusing it with `Ctrl+a a` leaves the zoom on.
    pub fn zoom_hides(&self, id: &ModuleId) -> bool {
        match self.zoom() {
            Some(Zoom::Module(z)) => z != id,
            Some(Zoom::Con(c)) => self.root.find(*c).is_none_or(|c| !c.leaves().contains(id)),
            None => false,
        }
    }

    /// `Ctrl+a m`/`z`: zoom `target`, or end the zoom if one is on (whatever `target` is, as
    /// today's `PaneLayout::toggle_zoom` does).
    pub fn toggle_zoom(&mut self, target: &ModuleId) -> ZoomChange {
        if self.unzoom() {
            return ZoomChange::Unzoomed;
        }
        let shown = self.modules().into_iter().filter(|id| self.is_shown(id)).count();
        if !self.is_in_view(target) || shown < 2 {
            return ZoomChange::Nothing;
        }
        self.zoomed = Some(match self.selection {
            Selection::Module => Zoom::Module(target.clone()),
            Selection::Con(id) if id != self.root.id => Zoom::Con(id),
            Selection::Con(_) => return ZoomChange::Nothing,
        });
        ZoomChange::Zoomed
    }

    /// Puts back a zoom that something else ended, whichever kind it was, around `keeps`, the module
    /// that holds the keys. Unlike `toggle_zoom` it never reads the selection, so a container
    /// selected meanwhile cannot turn a module's zoom into a container's. `false`, changing nothing,
    /// when a zoom is on already, `keeps` is not in view, fewer than two modules are shown (floats
    /// count, as they do for `toggle_zoom`: a zoomed float beside one tile is a zoom it could make), or
    /// the zoom would hide `keeps` (a module zoom of another module; a container that is gone, the
    /// root, or does not hold `keeps`).
    pub fn restore_zoom(&mut self, zoom: &Zoom, keeps: &ModuleId) -> bool {
        if self.zoomed.is_some() || !self.is_in_view(keeps) {
            return false;
        }
        let shown = self.modules().into_iter().filter(|id| self.is_shown(id)).count();
        if shown < 2 {
            return false;
        }
        let around_keeps = match zoom {
            Zoom::Module(id) => id == keeps,
            Zoom::Con(id) => *id != self.root.id && self.root.find(*id).is_some_and(|c| c.leaves().contains(keeps)),
        };
        if around_keeps {
            self.zoomed = Some(zoom.clone());
        }
        around_keeps
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
        if self.away.contains(id) {
            return Err(LayoutError::Away(id.clone()));
        }
        let changed = self.hidden.remove(id);
        if self.zoomed().is_some_and(|z| self.is_tabbed_away(z)) {
            self.unzoom();
        }
        Ok(changed)
    }

    /// Sets the share of the step `divider` draws (a divider [`super::geometry::arrange`] returned for
    /// this layout), clamped into `MIN_RATIO..=MAX_RATIO`: the child before it gets that part of what
    /// the two children on either side of it hold. A NaN changes nothing (a divider dragged by a broken
    /// pointer event must not wedge the tree). If a pinned length governs that step it is forgotten, so
    /// the share is what the next allocation shows and is then frozen again. A drag and
    /// `Ctrl+a h/j/k/l` go through [`super::geometry::move_divider`], which keeps a pin in pixels.
    pub fn set_ratio(&mut self, divider: &Divider, ratio: f32) -> Result<(), LayoutError> {
        let shown = self.shown_set();
        let shown = |id: &ModuleId| shown.contains(id);
        if !has_step(&self.root, divider, &shown) {
            return Err(LayoutError::NoSuchDivider);
        }
        set_step_ratio(&mut self.root, divider, ratio, &shown);
        Ok(())
    }

    /// Replaces the tree with `root`, for `super::ops`, which rearranges the tree and keeps the
    /// other invariants itself. `root` must hold every module this layout holds; it may hold one
    /// more (a module placed for the first time), which joins the end of the MRU list. A module alone
    /// becomes a root of one child that keeps the old root's id; a root holding only another container
    /// is that container.
    pub(super) fn replace_root(&mut self, root: Node) {
        let root = into_root(root, Some(self.root.id));
        let leaves = root.leaves();
        debug_assert!(
            self.mru.iter().all(|id| leaves.contains(id) || self.is_floating(id)),
            "a rearrangement lost a module"
        );
        for id in leaves {
            if !self.mru.contains(&id) {
                self.mru.push(id);
            }
        }
        let old = std::mem::replace(&mut self.root, root);
        if old != self.root {
            self.undo = Some(old);
        }
        self.root.focus_leaf(&self.focus);
        self.retain_selection();
        self.last_preset = None;
    }

    /// Marks the hidden module `id` gone, for `super::kill`, which has just hidden it; `was_shown`
    /// says whether it was shown before that hide ([`Layout::saved_hidden`]).
    pub(super) fn retire(&mut self, id: &ModuleId, was_shown: bool) {
        debug_assert!(self.hidden.contains(id), "only a hidden module can be gone");
        self.gone.insert(id.clone());
        if self.mark.as_ref() == Some(id) {
            self.mark = None;
        }
        if was_shown {
            self.retired_shown.insert(id.clone());
        }
    }

    /// Takes `id` out of `hidden` without the checks [`Layout::show`] makes, for `super::ops`,
    /// which has just placed it. `true` if it was hidden.
    pub(super) fn unhide(&mut self, id: &ModuleId) -> bool {
        self.hidden.remove(id)
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
        let others_shown = self.modules().iter().any(|m| m != id && self.is_shown(m));
        if !others_shown {
            return Err(LayoutError::LastVisible(id.clone()));
        }
        if self.zoomed() == Some(id) {
            self.zoomed = None;
        }
        self.hidden.insert(id.clone());
        self.retain_selection();
        Ok(true)
    }

    /// Removes a numbered instance, giving its keys to the editor or the first shown leaf. An away
    /// instance can be removed: what ran in it may end in the other window.
    pub fn remove(&mut self, id: &ModuleId) -> Result<(), LayoutError> {
        if id.number() == 1 {
            return Err(LayoutError::NotAnInstance(id.clone()));
        }
        if !self.contains(id) {
            return Err(LayoutError::NotInTree(id.clone()));
        }
        let root = if self.is_floating(id) {
            self.take_float(id);
            None
        } else {
            Some(self.detach_tile(id).map_err(|_| LayoutError::LastVisible(id.clone()))?)
        };
        self.float_memory.remove(id);
        if self.zoomed() == Some(id) {
            self.unzoom();
        }
        self.mru.retain(|m| m != id);
        self.hidden.remove(id);
        self.gone.remove(id);
        self.retired_shown.remove(id);
        self.away.remove(id);
        self.away_shown.remove(id);
        if self.mark.as_ref() == Some(id) {
            self.clear_mark();
        }
        if let Some(root) = root {
            self.replace_root(root);
        }
        let modules = self.modules();
        if !modules.iter().any(|m| self.is_shown(m)) {
            let first = modules
                .iter()
                .find(|m| **m == ModuleId::editor() && !self.is_gone(m) && !self.is_away(m))
                .or_else(|| modules.iter().find(|m| !self.is_gone(m) && !self.is_away(m)))
                .ok_or_else(|| LayoutError::LastVisible(id.clone()))?;
            self.hidden.remove(first);
        }
        if &self.focus == id {
            let next = modules
                .iter()
                .find(|m| **m == ModuleId::editor() && self.is_shown(m))
                .or_else(|| modules.iter().find(|m| self.is_shown(m)))
                .expect("a remaining leaf is shown");
            self.set_focus(next)?;
        }
        Ok(())
    }
}
/// `node` as a root container: a module alone is a root of one child (with `keep` as its id when
/// given, so the root a change leaves with one module is still the same container), and a root holding
/// only another container is replaced by that container, as often as that holds.
fn into_root(node: Node, keep: Option<super::con::ConId>) -> Con {
    match node {
        Node::Leaf(_) => {
            let mut root = Con::new(ConLayout::SplitH, vec![Child::new(node, 1.0)]);
            if let Some(id) = keep {
                root.id = id;
            }
            root
        }
        Node::Con(mut root) => {
            while root.children.len() == 1 && matches!(root.children[0].node, Node::Con(_)) {
                let Some(Child {
                    node: Node::Con(inner), ..
                }) = root.children.pop()
                else {
                    unreachable!()
                };
                root = inner;
            }
            root
        }
    }
}

impl Layout {
    /// Copies the arrangement for a new workspace with fresh container ids and no undo,
    /// selection, zoom, mark or last preset.
    pub fn fresh_copy(&self) -> Layout {
        fn renumber(con: &mut Con) {
            con.id = ConId::fresh();
            for child in &mut con.children {
                if let Node::Con(nested) = &mut child.node {
                    renumber(nested);
                }
            }
        }

        let mut copy = self.clone();
        renumber(&mut copy.root);
        copy.undo = None;
        copy.zoomed = None;
        copy.mark = None;
        copy.last_preset = None;
        copy.selection = Selection::Module;
        copy
    }
}

#[cfg(test)]
mod tests {
    use super::super::con::Pin;
    use super::super::geometry::{arrange, Frame, Size};
    use super::*;

    fn lua(id: &str) -> ModuleId {
        ModuleId::lua(id)
    }

    /// The container a split of two builds, for comparing with a layout's root.
    fn con(node: Node) -> Con {
        match node {
            Node::Con(c) => c,
            Node::Leaf(id) => panic!("{id} is a module, not a container"),
        }
    }

    fn decl(id: &str, placement: Placement) -> ModuleDecl {
        ModuleDecl { id: lua(id), placement }
    }

    #[test]
    fn detaching_the_last_tile_says_so_instead_of_panicking() {
        use super::super::geometry::Direction;
        use super::super::kill::{kill, Reopen};
        use super::super::ops::{place, place_as_tab};
        use super::super::verbs::{float_at, move_far};

        let tile = ModuleId::parse("terminal#2").unwrap();
        let floating = ModuleId::agent();
        let frame = Frame::new(Size { w: 1280, h: 721 }, 1);
        for mode in [ConLayout::SplitH, ConLayout::Tabbed, ConLayout::Stacked] {
            let mut layout = Layout::from_parts_floating(
                Node::Con(Con::new(mode, vec![Child::new(Node::Leaf(tile.clone()), 1.0)])),
                vec![Float {
                    id: floating.clone(),
                    rect: FloatRect::centred(frame.size, 0.6),
                    scratch: false,
                }],
                BTreeSet::new(),
                tile.clone(),
                BTreeMap::new(),
            )
            .unwrap();
            let before = layout.clone();
            assert_eq!(layout.detach_tile(&tile), Err(Emptied));
            assert_eq!(layout, before);
            assert_eq!(layout.remove(&tile), Err(LayoutError::LastVisible(tile.clone())));
            assert_eq!(
                float_at(&mut layout, &tile, None, &frame),
                Err(LayoutError::LastTiled(tile.clone()))
            );
            assert_eq!(
                move_far(&mut layout, &tile, Direction::Right, &frame),
                Err(LayoutError::LastVisible(tile.clone()))
            );
            assert_eq!(
                place(&mut layout, &tile, &floating, Axis::Row),
                Err(LayoutError::SameModule(tile.clone()))
            );
            assert_eq!(
                place_as_tab(&mut layout, &tile, &floating, true),
                Err(LayoutError::SameModule(tile.clone()))
            );
            assert_eq!(layout, before);

            kill(&mut layout, &tile, Reopen::At(Placement::RightOfRoot), &frame).unwrap();
            assert_eq!(layout.root(), before.root());
            assert!(!layout.is_shown(&tile));
            assert_eq!(layout.focus(), &floating);

            for tab in [false, true] {
                let mut layout = before.clone();
                if tab {
                    place_as_tab(&mut layout, &floating, &tile, true).unwrap();
                } else {
                    place(&mut layout, &floating, &tile, Axis::Row).unwrap();
                }
                assert_eq!(layout.leaves(), [tile.clone(), floating.clone()]);
                assert!(!layout.is_floating(&floating));
                layout.root().check(true).unwrap();
            }
        }
    }

    #[test]
    fn companion_layout_is_one_agent_leaf() {
        let layout = Layout::companion();
        assert_eq!(layout.root().leaves(), vec![ModuleId::agent()]);
        assert_eq!(layout.focus(), &ModuleId::agent());
        assert!(layout.is_shown(&ModuleId::agent()));
        assert!(!layout.is_shown(&ModuleId::editor()));
    }

    /// The default split leaves 760px for the editor out of the 1279px remaining
    /// after the default window's 1px divider, matching `main`'s `GtkPaned`.
    #[test]
    fn the_default_is_editor_then_agent_where_main_put_the_divider() {
        assert_eq!((1279.0 * DEFAULT_EDITOR_SHARE).round(), 760.0);
        let layout = Layout::initial(&[]).unwrap();
        assert_eq!(
            layout.root(),
            &con(Node::split(
                Axis::Row,
                DEFAULT_EDITOR_SHARE,
                Node::Leaf(ModuleId::editor()),
                Node::Leaf(ModuleId::agent())
            ))
        );
        assert_eq!(layout.focus(), &ModuleId::editor());
        assert_eq!(layout.mru(), &[ModuleId::editor(), ModuleId::agent()]);
        assert!(layout.hidden().is_empty());
        assert_eq!(layout.zoomed(), None);
    }

    /// `side` goes right of the whole root and `bottom` below it, in registration order.
    /// A side panel preserves the agent.
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
            &con(Node::pinned(
                Axis::Column,
                BELOW_ROOT_SHARE,
                Branch::Second,
                Node::split(Axis::Row, RIGHT_OF_ROOT_SHARE, row, Node::Leaf(lua("right"))),
                Node::Leaf(lua("below"))
            ))
        );
        assert_eq!(
            layout.visible_leaves(),
            vec![ModuleId::editor(), ModuleId::agent(), lua("right"), lua("below")]
        );
    }

    /// `main` takes the editor's leaf and hides the editor, which remains in the tree
    /// so it can be shown again. Focus starts on the panel.
    #[test]
    fn main_takes_the_editors_leaf_and_hides_the_editor() {
        let layout = Layout::initial(&[decl("m", Placement::InPlaceOfEditor)]).unwrap();
        assert!(layout.contains(&ModuleId::editor()));
        assert!(layout.hidden().contains(&ModuleId::editor()));
        assert_eq!(layout.visible_leaves(), vec![lua("m"), ModuleId::agent()]);
        assert_eq!(layout.focus(), &lua("m"));
    }

    /// `Placement::BelowEditor` wraps only the editor's own leaf, even with the agent on its right
    /// and a side panel around the root. Neither the agent nor the side panel moves or loses height.
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

    /// In the default `[editor | agent]` window, the module goes below the editor's own leaf.
    /// The agent keeps its full height, unlike placement below the whole root.
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

    /// `LastVisible` describes the last module on screen rather than saying it cannot be hidden.
    /// The message also reaches callers of `kill()` through its inner `hide`.
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
    /// ending would resurrect a module the user closed.
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

    /// A hidden module cannot take focus through `set_focus`, which receives GTK focus changes.
    /// `hide` moves focus away first; this checks that a later focus update cannot give it back.
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
    fn set_ratio_clamps_ignores_nan_and_refuses_a_divider_no_longer_on_screen() {
        let frame = Frame::new(Size { w: 1280, h: 721 }, 1);
        let mut layout = Layout::initial(&[decl("below", Placement::BelowRoot)]).unwrap();
        let divider = |layout: &Layout, axis: Axis| {
            arrange(layout, &frame)
                .dividers
                .into_iter()
                .find(|d| d.axis == axis)
                .unwrap()
        };
        let row = divider(&layout, Axis::Row);
        layout.set_ratio(&row, 0.99).unwrap();
        // MAX_RATIO of the row's 1279px.
        assert_eq!(divider(&layout, Axis::Row).first_px, 1215);
        let column = divider(&layout, Axis::Column);
        layout.set_ratio(&column, 0.0).unwrap();
        // MIN_RATIO of the column's 720px.
        assert_eq!(divider(&layout, Axis::Column).first_px, 36);
        let before = layout.clone();
        layout.set_ratio(&column, f32::NAN).unwrap();
        assert_eq!(layout, before, "NaN changes nothing");
        let mut stale = column.clone();
        stale.con = super::super::con::ConId::fresh();
        assert_eq!(layout.set_ratio(&stale, 0.5), Err(LayoutError::NoSuchDivider));
        super::super::geometry::hide(&mut layout, &lua("below"), &frame).unwrap();
        assert_eq!(
            layout.set_ratio(&column, 0.5),
            Err(LayoutError::NoSuchDivider),
            "the bottom module is hidden: the column draws no divider"
        );
    }

    /// A module placed below the root keeps its height. Until the pin has a length, the split
    /// uses `BELOW_ROOT_SHARE`. A module placed right of the root or in the editor's leaf
    /// is not pinned.
    #[test]
    fn a_module_placed_below_the_root_is_pinned_on_its_own_side_and_nothing_else_is() {
        let layout = Layout::initial(&[decl("below", Placement::BelowRoot)]).unwrap();
        let root = layout.root();
        assert_eq!(
            (root.layout, root.children[0].share),
            (ConLayout::SplitV, BELOW_ROOT_SHARE)
        );
        assert_eq!(
            (root.children[0].pin, root.children[1].pin),
            (None, Some(Pin { px: None }))
        );
        fn pins(c: &Con, out: &mut Vec<Pin>) {
            for child in &c.children {
                out.extend(child.pin);
                if let Node::Con(inner) = &child.node {
                    pins(inner, out);
                }
            }
        }
        for placement in [Placement::RightOfRoot, Placement::InPlaceOfEditor] {
            let layout = Layout::initial(&[decl("x", placement)]).unwrap();
            let mut found = Vec::new();
            pins(layout.root(), &mut found);
            assert!(found.is_empty(), "{placement:?}: {found:?}");
        }
    }

    /// `set_ratio` on a step a pin governs forgets the pinned length: the share is what shows next.
    #[test]
    fn set_ratio_forgets_a_pinned_length() {
        let frame = Frame::new(Size { w: 1280, h: 721 }, 1);
        let mut layout = Layout::initial(&[decl("below", Placement::BelowRoot)]).unwrap();
        layout.root_mut().children[1].pin = Some(Pin { px: Some(240) });
        let column = arrange(&layout, &frame).dividers[0].clone();
        assert_eq!(column.first_px, 480, "720 less the pinned 240");
        layout.set_ratio(&column, 0.5).unwrap();
        assert_eq!(layout.root().children[1].pin, Some(Pin { px: None }));
        assert_eq!(arrange(&layout, &frame).rect_of(&lua("below")).unwrap().h, 360);
    }

    /// Rebuilding from a state file applies every `Layout::new` check
    /// and rejects invalid `hidden` sets.
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
        if let Node::Con(c) = &mut bad_pin {
            c.children[1].pin = Some(Pin { px: Some(-1) });
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

    /// The editor beside a container of two plugins, `a` with the keys.
    fn beside_a_container() -> Layout {
        let pair = Node::Con(Con::new(
            ConLayout::SplitV,
            vec![
                Child::new(Node::Leaf(lua("a")), 0.5),
                Child::new(Node::Leaf(lua("b")), 0.5),
            ],
        ));
        let root = Node::Con(Con::new(
            ConLayout::SplitH,
            vec![Child::new(Node::Leaf(ModuleId::editor()), 0.5), Child::new(pair, 0.5)],
        ));
        Layout::new(root, lua("a")).unwrap()
    }

    #[test]
    fn restore_zoom_puts_back_either_kind_and_ignores_the_selection() {
        let mut layout = beside_a_container();
        let pair = layout.container_of(&lua("a")).unwrap();
        assert!(layout.restore_zoom(&Zoom::Con(pair), &lua("a")));
        assert_eq!(layout.zoom(), Some(&Zoom::Con(pair)));
        assert_eq!(layout.visible_leaves(), vec![lua("a"), lua("b")]);
        assert!(
            !layout.restore_zoom(&Zoom::Module(lua("a")), &lua("a")),
            "a zoom is on already"
        );

        // A container selected meanwhile: `toggle_zoom` would zoom it, the restore keeps the module's.
        layout.unzoom();
        layout.select(Selection::Con(pair));
        assert!(layout.restore_zoom(&Zoom::Module(lua("a")), &lua("a")));
        assert_eq!(layout.zoom(), Some(&Zoom::Module(lua("a"))));
    }

    #[test]
    fn restore_zoom_refuses_a_zoom_that_would_hide_the_module_with_the_keys() {
        let mut layout = beside_a_container();
        let pair = layout.container_of(&lua("a")).unwrap();
        let root = layout.root().id;
        assert!(!layout.restore_zoom(&Zoom::Module(lua("b")), &lua("a")));
        assert!(!layout.restore_zoom(&Zoom::Con(pair), &ModuleId::editor()));
        assert!(
            !layout.restore_zoom(&Zoom::Con(root), &lua("a")),
            "the root is never zoomed"
        );
        layout.hide_unfocused(&ModuleId::editor()).unwrap();
        layout.hide_unfocused(&lua("b")).unwrap();
        assert!(
            !layout.restore_zoom(&Zoom::Module(lua("a")), &lua("a")),
            "the only module shown"
        );
        assert_eq!(layout.zoom(), None);
    }

    #[test]
    fn the_keys_leaving_with_an_away_module_stay_inside_the_container_zoom() {
        let mut layout = beside_a_container();
        let pair = layout.container_of(&lua("a")).unwrap();
        layout.set_focus(&ModuleId::editor()).unwrap();
        layout.set_focus(&lua("b")).unwrap();
        layout.select(Selection::Con(pair));
        assert_eq!(layout.toggle_zoom(&lua("b")), ZoomChange::Zoomed);
        assert_eq!(layout.zoom(), Some(&Zoom::Con(pair)));

        layout.set_away(&lua("b"), true).unwrap();
        assert_eq!(
            layout.zoom(),
            Some(&Zoom::Con(pair)),
            "the zoomed container still shows a"
        );
        assert_eq!(
            layout.focus(),
            &lua("a"),
            "the editor is more recent but the zoom leaves it off screen"
        );
        assert!(
            layout.is_visible(layout.focus()),
            "the keys sit on a module that is drawn"
        );
    }

    #[test]
    fn an_away_float_is_not_drawn_and_keeps_its_rectangle() {
        use super::super::verbs::{scratchpad_show, ScratchShow};
        let frame = Frame::new(Size { w: 1280, h: 721 }, 1);
        let notes = ModuleId::parse("terminal#2").unwrap();
        let scratch = ModuleId::parse("terminal#3").unwrap();
        let rect = FloatRect {
            cx: 0.7,
            cy: 0.4,
            w: 420,
            h: 260,
        };
        let mut layout = Layout::initial(&[]).unwrap();
        layout.add_float(&notes, rect, false).unwrap();
        layout.add_float(&scratch, rect, true).unwrap();
        crate::layout::scratchpad_move(&mut layout, &scratch, &frame).unwrap();
        assert!(layout.is_stowed(&scratch));
        let drawn = |layout: &Layout| -> BTreeMap<ModuleId, super::super::geometry::Rect> {
            let arrangement = arrange(layout, &frame);
            let floats = arrangement.floats.into_iter().map(|spot| (spot.id, spot.rect));
            arrangement.modules.into_iter().chain(floats).collect()
        };
        let before = drawn(&layout);
        assert!(before.contains_key(&notes));

        layout.set_away(&notes, true).unwrap();
        layout.set_away(&scratch, true).unwrap();
        assert!(!drawn(&layout).contains_key(&notes), "an away float is not drawn");
        assert!(arrange(&layout, &frame).floats.is_empty());
        assert_eq!(
            layout.float_of(&notes).map(|f| f.rect),
            Some(rect),
            "its rectangle is kept"
        );
        assert!(
            !layout.is_stowed(&scratch),
            "the scratchpad does not hold what the other window has"
        );
        assert_eq!(scratchpad_show(&mut layout, None, &frame), Err(LayoutError::NoScratch));
        assert_eq!(
            scratchpad_show(&mut layout, Some(&scratch), &frame),
            Err(LayoutError::Away(scratch.clone()))
        );

        layout.set_away(&notes, false).unwrap();
        layout.set_away(&scratch, false).unwrap();
        assert_eq!(drawn(&layout), before, "back where it floated");
        assert!(layout.is_stowed(&scratch), "and the stowed one is stowed again");
        assert!(matches!(
            scratchpad_show(&mut layout, None, &frame),
            Ok(ScratchShow::Shown(id)) if id == scratch
        ));
    }
}
