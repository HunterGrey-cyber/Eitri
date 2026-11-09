//! A window's workspaces: named layouts, one of them current. The current one's `Layout` lives
//! outside this type (the shell keeps it in the `Rc<RefCell<Layout>>` every reader of the live layout
//! already holds), so a switch is a swap of two values and nothing that reads the live layout changes.
//! This type holds the order of the names, every other workspace's layout, and what a later build
//! wrote into the state file that this one cannot read but must write back unchanged.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde_json::{Map, Value};

use super::geometry::Frame;
use super::kill::{kill, Reopen};
use super::module::{declared_placement, ModuleDecl, ModuleId, ModuleKind, Placement};
use super::tree::{Layout, LayoutError};
use crate::window_link::base_tabs::MovedTabKey;

/// The workspace every window has from its first launch, and the one a file from before workspaces
/// is read into.
pub const MAIN: &str = "main";

/// The longest workspace name, in characters: a name is shown in the top bar and typed in a chooser.
pub const MAX_NAME_CHARS: usize = 32;

#[derive(Debug, Clone, PartialEq)]
pub struct Workspaces {
    order: Vec<String>,
    stored: BTreeMap<String, Layout>,
    current: String,
    last: Option<String>,
    /// What a later build wrote that this one does not read: top-level keys, and each workspace's own
    /// keys.
    kept: Kept,
    /// Per workspace, the base chat view's tabs that came into this window with that workspace, by agent and
    /// session: moving the workspace back takes exactly those. A workspace with none has no entry.
    moved_tabs: BTreeMap<String, Vec<MovedTabKey>>,
}

/// Keys of the state file this build does not know, carried verbatim so a save by this build does
/// not drop what a later build keeps there (a hypothetical `future_overlays` key, say). In [`Workspaces`] a workspace
/// with no kept keys has no entry; read from a file (`persist::V2State`), every workspace the file
/// holds has one, empty or not, which tells "the file has this workspace with no such keys" apart
/// from "the file has no such workspace".
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Kept {
    pub top: Map<String, Value>,
    /// Each workspace's unknown keys, such as a hypothetical `future_overlays`.
    pub per_workspace: BTreeMap<String, Map<String, Value>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkspaceError {
    /// A name that is empty, too long, or holds a control character.
    Name(String),
    /// A name given twice.
    Duplicate(String),
    /// The current workspace is not one of the names.
    NoCurrent(String),
    /// The stored layouts are not exactly the workspaces other than the current one.
    Stored(String),
    /// No workspace has the requested name.
    NoSuch(String),
    /// The permanent main workspace cannot be renamed or deleted.
    Main,
    /// The current workspace cannot be deleted until the window switches away.
    Current(String),
    /// A rename or creation requested a name another workspace already owns.
    Taken(String),
}

impl fmt::Display for WorkspaceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            // Escaped: a name refused for its control characters would otherwise carry them into
            // whatever line shows the error.
            WorkspaceError::Name(name) => write!(
                f,
                "'{}' is not a workspace name (1 to {MAX_NAME_CHARS} characters, no control characters)",
                printable(name)
            ),
            WorkspaceError::NoSuch(name) => write!(f, "no workspace '{}'", printable(name)),
            WorkspaceError::Main => write!(f, "'main' keeps its name and is never deleted"),
            WorkspaceError::Current(name) => write!(
                f,
                "'{}' is the current workspace; switch away before deleting it",
                printable(name)
            ),
            WorkspaceError::Taken(name) => write!(f, "a workspace named '{}' already exists", printable(name)),
            WorkspaceError::Duplicate(name) => write!(f, "workspace '{name}' is named twice"),
            WorkspaceError::NoCurrent(name) => write!(f, "the current workspace '{name}' is not one of the workspaces"),
            WorkspaceError::Stored(name) => write!(
                f,
                "workspace '{name}': the stored layouts must be every workspace but the current one"
            ),
        }
    }
}

impl std::error::Error for WorkspaceError {}

/// `text` with each control character -- a newline, an escape -- written as its Rust escape, the rest
/// as it is: for a name or key read from a file, shown on one line.
pub(crate) fn printable(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if c.is_control() {
            out.extend(c.escape_default());
        } else {
            out.push(c);
        }
    }
    out
}

/// Makes the layout hold the window's modules, removing absent instances before adding missing modules.
pub fn fit_to(layout: &mut Layout, modules: &[ModuleId]) -> Vec<String> {
    fit_to_with(layout, modules, &[])
}

/// Fits the window's modules, placing missing ones hidden at their declared placement or below the editor.
pub fn fit_to_with(layout: &mut Layout, modules: &[ModuleId], decls: &[ModuleDecl]) -> Vec<String> {
    let mut notes = Vec::new();
    for id in layout.modules() {
        if id.number() >= 2 && !modules.contains(&id) {
            layout.remove(&id).expect("a numbered instance can be removed");
            notes.push(format!("left out '{id}': this window has no such module now"));
        }
    }
    for id in modules {
        if !layout.contains(id) {
            let placement = declared_placement(decls, id, Placement::BelowEditor);
            let root = super::tree::place_new(layout.root_node(), id, placement).0;
            layout.replace_root(root);
            layout.hide_unfocused(id).expect("another module remains shown");
            notes.push(if placement == Placement::BelowEditor {
                format!("placed '{id}' below the editor, hidden: another workspace has it")
            } else {
                format!("placed '{id}' where its open rule says ({placement:?}), hidden: another workspace has it")
            });
        }
    }
    notes
}

impl Workspaces {
    /// Adds every module held by any workspace to all the others, keeping existing placements.
    pub fn fit_all(&mut self, live: &mut Layout) -> Vec<String> {
        self.fit_all_with(live, &[])
    }

    /// Fits every workspace to their combined modules, using declarations for missing modules only.
    /// A module away in any workspace is away in every one, including one it is added to here: the
    /// other window holds it, whichever workspace is current.
    pub fn fit_all_with(&mut self, live: &mut Layout, decls: &[ModuleDecl]) -> Vec<String> {
        let mut modules = Vec::new();
        let mut away = BTreeSet::new();
        for (_, layout) in self.each(live) {
            away.extend(layout.away());
            for id in layout.modules() {
                if !modules.contains(&id) {
                    modules.push(id);
                }
            }
        }
        let mut notes = Vec::new();
        for name in &self.order {
            let layout = if *name == self.current {
                &mut *live
            } else {
                self.stored
                    .get_mut(name)
                    .expect("every non-current workspace is stored")
            };
            notes.extend(
                fit_to_with(layout, &modules, decls)
                    .into_iter()
                    .map(|note| format!("workspace '{name}': {note}")),
            );
            for id in &away {
                // Another module stays shown: the chat is never away, and a layout without one is refused.
                let _ = layout.set_away(id, true);
            }
        }
        notes
    }

    /// One workspace, `main`, current: a first launch, and any window whose file could not be used.
    pub fn single() -> Workspaces {
        Workspaces {
            order: vec![MAIN.to_string()],
            stored: BTreeMap::new(),
            current: MAIN.to_string(),
            last: None,
            kept: Kept::default(),
            moved_tabs: BTreeMap::new(),
        }
    }

    /// Workspaces from their parts, checked: every name valid and given once, `current` among them,
    /// and `stored` holding a layout for exactly the names other than `current`. Kept keys of a
    /// workspace not in `order` have nowhere to be written and are dropped, as is an empty entry.
    pub fn from_parts(
        order: Vec<String>,
        stored: BTreeMap<String, Layout>,
        current: String,
        mut kept: Kept,
    ) -> Result<Workspaces, WorkspaceError> {
        let mut seen = BTreeSet::new();
        for name in &order {
            Workspaces::valid_name(name)?;
            if !seen.insert(name.as_str()) {
                return Err(WorkspaceError::Duplicate(name.clone()));
            }
        }
        if !seen.contains(current.as_str()) {
            return Err(WorkspaceError::NoCurrent(current));
        }
        if let Some(name) = stored
            .keys()
            .find(|name| !seen.contains(name.as_str()) || **name == current)
        {
            return Err(WorkspaceError::Stored(name.clone()));
        }
        if let Some(name) = order
            .iter()
            .find(|name| **name != current && !stored.contains_key(*name))
        {
            return Err(WorkspaceError::Stored(name.clone()));
        }
        kept.per_workspace
            .retain(|name, map| seen.contains(name.as_str()) && !map.is_empty());
        Ok(Workspaces {
            order,
            stored,
            current,
            last: None,
            kept,
            moved_tabs: BTreeMap::new(),
        })
    }

    /// 1 to [`MAX_NAME_CHARS`] characters, none of them a control character (a newline or an escape
    /// would break the one line the name is shown on).
    pub fn valid_name(name: &str) -> Result<(), WorkspaceError> {
        let chars = name.chars().count();
        if chars == 0 || chars > MAX_NAME_CHARS || name.chars().any(char::is_control) {
            return Err(WorkspaceError::Name(name.to_string()));
        }
        Ok(())
    }

    /// Swaps the live layout with the target, carrying the window's mark and resetting selections.
    /// Returns the visible module that takes the keys, or `None` when already current.
    pub fn switch(
        &mut self,
        live: &mut Layout,
        to: &str,
        lua: &[ModuleDecl],
    ) -> Result<Option<ModuleId>, WorkspaceError> {
        if !self.order.iter().any(|name| name == to) {
            return Err(WorkspaceError::NoSuch(to.to_string()));
        }
        if self.current == to {
            return Ok(None);
        }
        let mark = live.mark().cloned();
        let mut outgoing = self.stored.remove(to).expect("every non-current workspace is stored");
        std::mem::swap(live, &mut outgoing);
        outgoing.clear_mark();
        outgoing.clear_selection();
        let old = std::mem::replace(&mut self.current, to.to_string());
        self.stored.insert(old.clone(), outgoing);
        self.last = Some(old);
        live.clear_mark();
        if let Some(id) = mark.filter(|id| live.contains(id) && !live.is_gone(id)) {
            let _ = live.set_mark(&id);
        }
        live.clear_selection();
        let focus = if live.is_visible(live.focus()) {
            Some(live.focus().clone())
        } else {
            super::reconcile::first_focus(&live.visible_modules(), lua)
        };
        if let Some(id) = &focus {
            let _ = live.set_focus(id);
        }
        Ok(focus)
    }

    /// The preceding or following workspace in order, wrapping; none when alone.
    pub fn step(&self, forward: bool) -> Option<String> {
        let count = self.order.len();
        if count <= 1 {
            return None;
        }
        let index = self.order.iter().position(|name| *name == self.current)?;
        let next = if forward {
            (index + 1) % count
        } else {
            (index + count - 1) % count
        };
        Some(self.order[next].clone())
    }

    /// The workspace left by the latest switch, while it still exists.
    pub fn last(&self) -> Option<&str> {
        self.last
            .as_deref()
            .filter(|last| self.order.iter().any(|name| name == last))
    }

    /// The lowest available whole number from 2, as text.
    pub fn next_free_name(&self) -> String {
        (2..)
            .map(|number| number.to_string())
            .find(|name| !self.order.contains(name))
            .unwrap()
    }

    /// Appends a copy with fresh container ids and no arrangement runtime state.
    pub fn create_copy(&mut self, live: &Layout) -> String {
        let name = self.next_free_name();
        self.create(&name, live.fresh_copy())
            .expect("a free numeric name is valid");
        name
    }

    /// Appends a layout containing every module of the window under a new name.
    pub fn create(&mut self, name: &str, layout: Layout) -> Result<(), WorkspaceError> {
        Self::valid_name(name)?;
        if self.order.iter().any(|existing| existing == name) {
            return Err(WorkspaceError::Taken(name.to_string()));
        }
        self.order.push(name.to_string());
        self.stored.insert(name.to_string(), layout);
        Ok(())
    }

    /// Removes a stored workspace and its preserved keys, keeping `main` and the current one.
    pub fn delete(&mut self, name: &str) -> Result<(), WorkspaceError> {
        if name == MAIN {
            return Err(WorkspaceError::Main);
        }
        if name == self.current {
            return Err(WorkspaceError::Current(name.to_string()));
        }
        if !self.order.iter().any(|existing| existing == name) {
            return Err(WorkspaceError::NoSuch(name.to_string()));
        }
        self.order.retain(|existing| existing != name);
        self.stored.remove(name);
        self.kept.per_workspace.remove(name);
        self.moved_tabs.remove(name);
        if self.last.as_deref() == Some(name) {
            self.last = None;
        }
        Ok(())
    }

    /// Renames the current workspace and carries its preserved keys; `main` keeps its name, and no
    /// workspace takes it.
    pub fn rename_current(&mut self, to: &str) -> Result<(), WorkspaceError> {
        if self.current == MAIN {
            return Err(WorkspaceError::Main);
        }
        if self.current == to {
            return Ok(());
        }
        Self::valid_name(to)?;
        if self.order.iter().any(|name| name == to) {
            return Err(WorkspaceError::Taken(to.to_string()));
        }
        // Only reachable while `main` is missing: a workspace renamed to it would become the one
        // that can never be renamed or deleted, and would stand in for the window's default layout.
        if to == MAIN {
            return Err(WorkspaceError::Main);
        }
        for name in &mut self.order {
            if *name == self.current {
                *name = to.to_string();
            }
        }
        if let Some(keys) = self.kept.per_workspace.remove(&self.current) {
            self.kept.per_workspace.insert(to.to_string(), keys);
        }
        if let Some(tabs) = self.moved_tabs.remove(&self.current) {
            self.moved_tabs.insert(to.to_string(), tabs);
        }
        self.current = to.to_string();
        Ok(())
    }

    /// Puts `main` back, first in order and stored (not current), as `layout`; `false`, changing
    /// nothing, when `main` is already there. A saved file that lost `main` would otherwise open
    /// without the workspace every window has.
    pub fn restore_main(&mut self, layout: Layout) -> bool {
        if self.order.iter().any(|name| name == MAIN) {
            return false;
        }
        self.order.insert(0, MAIN.to_string());
        self.stored.insert(MAIN.to_string(), layout);
        true
    }

    /// Retires a module in every stored layout, showing the chat when nothing else would remain. One
    /// away in a layout is left as it is there: the other window holds it.
    pub fn retire_everywhere(&mut self, id: &ModuleId, frame: &Frame) {
        self.kill_everywhere(id, Reopen::Never, frame);
    }

    /// Ends a module in place in every stored layout, showing the chat when nothing else would remain. One
    /// away in a layout is left as it is there: the other window holds it.
    pub fn end_everywhere(&mut self, id: &ModuleId, frame: &Frame) {
        self.kill_everywhere(id, Reopen::InPlace, frame);
    }

    fn kill_everywhere(&mut self, id: &ModuleId, reopen: Reopen, frame: &Frame) {
        for layout in self.stored.values_mut() {
            if layout.contains(id) && !layout.is_gone(id) && !layout.is_away(id) {
                if let Err(LayoutError::LastVisible(_)) = kill(layout, id, reopen, frame) {
                    let _ = layout.show(&ModuleId::agent());
                    let _ = kill(layout, id, reopen, frame);
                }
            }
        }
    }

    /// Marks `id` away, or back, in every stored layout that holds it ([`Layout::set_away`]): its leaf
    /// stays where it is in each, so coming back puts it where it was in every workspace. The live
    /// layout is the caller's, as for [`Workspaces::retire_everywhere`].
    pub fn set_away_everywhere(&mut self, id: &ModuleId, away: bool) {
        for layout in self.stored.values_mut() {
            if layout.contains(id) {
                // Refused only when nothing else could be shown, and every layout holds the chat.
                let _ = layout.set_away(id, away);
            }
        }
    }

    /// Takes `id` back hidden in every stored layout that holds it, keeping its place in each. The live layout
    /// remains the caller's.
    pub fn set_back_hidden_everywhere(&mut self, id: &ModuleId) {
        for layout in self.stored.values_mut() {
            if layout.contains(id) {
                let _ = layout.set_back_hidden(id);
            }
        }
    }

    pub fn current(&self) -> &str {
        &self.current
    }

    pub fn order(&self) -> &[String] {
        &self.order
    }

    /// A workspace other than the current one; `None` for the current one, whose layout is the live
    /// one, and for a name that is not a workspace.
    pub fn stored(&self, name: &str) -> Option<&Layout> {
        self.stored.get(name)
    }

    /// [`Workspaces::stored`], to change.
    pub fn stored_mut_of(&mut self, name: &str) -> Option<&mut Layout> {
        self.stored.get_mut(name)
    }

    pub fn kept(&self) -> &Kept {
        &self.kept
    }

    /// The kept keys of one workspace; `None` when it has none.
    pub fn kept_for(&self, name: &str) -> Option<&Map<String, Value>> {
        self.kept.per_workspace.get(name)
    }

    /// The base chat view's tabs that came in with workspace `name`, as the state file names them.
    pub fn moved_tabs_of(&self, name: &str) -> &[MovedTabKey] {
        self.moved_tabs.get(name).map_or(&[], Vec::as_slice)
    }

    /// Every workspace's record of the tabs that came in with it, as the file keeps them.
    pub fn moved_tabs(&self) -> &BTreeMap<String, Vec<MovedTabKey>> {
        &self.moved_tabs
    }

    /// Replaces the record of the tabs that came in with each workspace. A workspace this window does not have
    /// has nowhere to keep one, and an empty list is no record, so neither is kept.
    pub fn set_moved_tabs(&mut self, mut tabs: BTreeMap<String, Vec<MovedTabKey>>) {
        tabs.retain(|name, list| self.order.contains(name) && !list.is_empty());
        self.moved_tabs = tabs;
    }

    /// Every module id a string anywhere in the kept keys names, by `ModuleId::parse`, each once: what
    /// a later build placed where this one cannot see it, so this build must never give the same name
    /// to a module of its own.
    pub fn kept_modules(&self) -> Vec<ModuleId> {
        let mut found = BTreeSet::new();
        for value in self.kept.top.values() {
            modules_named(value, &mut found);
        }
        for map in self.kept.per_workspace.values() {
            for value in map.values() {
                modules_named(value, &mut found);
            }
        }
        found.into_iter().collect()
    }

    /// Numbered modules named by preserved data cannot be allocated again.
    pub fn reserved(&self) -> Vec<ModuleId> {
        self.kept_modules().into_iter().filter(|id| id.number() >= 2).collect()
    }

    /// Adds a hidden instance below the editor in every stored workspace.
    pub fn add_everywhere(&mut self, id: &ModuleId) {
        self.add_everywhere_at(id, Placement::BelowEditor);
    }

    /// Adds a hidden module at `placement` in each stored workspace that lacks it.
    pub fn add_everywhere_at(&mut self, id: &ModuleId, placement: Placement) {
        self.add_everywhere_sized(id, placement, None);
    }

    /// [`Workspaces::add_everywhere_at`], each copy given `size` (a rule's tiled size) as the current workspace's
    /// copy was: a copy placed without it takes a default share there, so a one-line strip shown in another
    /// workspace would come up a third of the window tall.
    pub fn add_everywhere_sized(&mut self, id: &ModuleId, placement: Placement, size: Option<super::rules::RuleSize>) {
        for layout in self.stored_mut() {
            if !layout.contains(id) {
                let root = super::tree::place_new(layout.root_node(), id, placement).0;
                layout.replace_root(root);
                layout.hide_unfocused(id).expect("another module remains shown");
                // A refused share leaves the copy where `place_new` put it; nothing here can report it.
                let _ = super::rules::apply_tiled_size(layout, id, size);
            }
        }
    }

    pub fn remove_everywhere(&mut self, id: &ModuleId) {
        for layout in self.stored_mut() {
            if layout.contains(id) {
                let _ = layout.remove(id);
            }
        }
    }

    /// Every module this window places itself: the built-ins, each registered Lua panel, and every
    /// module any of its workspaces holds, `current` being the current one's layout. A kept key naming
    /// one of these would, once written back, put that module in two places.
    pub fn placed(&self, current: &Layout, lua: &[ModuleDecl]) -> BTreeSet<ModuleId> {
        let mut placed = built_in_and_registered(lua);
        for (_, layout) in self.each(current) {
            placed.extend(layout.modules());
        }
        placed
    }

    /// Replaces the kept keys with `now`'s, the file's as it is when this window writes: the kept keys
    /// are not this window's arrangement (it can neither read nor change them), so the file's newest
    /// copy of them wins. The top-level keys are taken whole. A workspace of this window takes the
    /// file's keys for it when the file has that workspace, and keeps its own when the file does not
    /// (another window removed it, or the file's copy of it could not be read): this window writes the
    /// workspace back, and its keys with it.
    pub fn retake_kept(&mut self, now: Kept) {
        self.kept.top = now.top;
        for (name, map) in now.per_workspace {
            if !self.order.contains(&name) {
                continue;
            }
            if map.is_empty() {
                self.kept.per_workspace.remove(&name);
            } else {
                self.kept.per_workspace.insert(name, map);
            }
        }
    }

    /// Every stored layout, for a change that must reach every workspace (a module that ends, or a new
    /// instance, changes the set of modules each one holds).
    pub fn stored_mut(&mut self) -> impl Iterator<Item = &mut Layout> {
        self.stored.values_mut()
    }

    /// Every workspace in order, as `(name, layout)`, the current one's layout being `current`.
    pub fn each<'a>(&'a self, current: &'a Layout) -> Vec<(&'a str, &'a Layout)> {
        self.order
            .iter()
            .map(|name| {
                let layout = if *name == self.current {
                    current
                } else {
                    self.stored
                        .get(name)
                        .expect("every workspace but the current one is stored")
                };
                (name.as_str(), layout)
            })
            .collect()
    }
}

impl Kept {
    /// Drops every workspace-level key that names a module in `placed` ([`Workspaces::placed`]),
    /// with one note per key dropped. Each note says a copy is kept: a caller that gets any back keeps
    /// a copy of the text these keys came from before it writes without them.
    pub fn drop_placed(&mut self, placed: &BTreeSet<ModuleId>) -> Vec<String> {
        let mut notes = Vec::new();
        for (name, map) in &mut self.per_workspace {
            drop_keys_naming(name, map, placed, &mut notes);
        }
        notes
    }
}

/// The modules every window places whatever its file says: the built-ins and each registered Lua
/// panel.
pub(crate) fn built_in_and_registered(lua: &[ModuleDecl]) -> BTreeSet<ModuleId> {
    let mut placed: BTreeSet<ModuleId> = [ModuleId::editor(), ModuleId::agent(), ModuleId::terminal()].into();
    placed.extend(
        lua.iter()
            .filter(|decl| decl.id.kind() == ModuleKind::LuaWebview)
            .map(|decl| decl.id.clone()),
    );
    placed
}

/// Drops each of workspace `name`'s kept keys that names a module in `placed`, anywhere in its value:
/// written back, it would put that module in two places, in this window's tree and wherever the
/// later build keeps it. One note per key, its text escaped for one log line.
pub(crate) fn drop_keys_naming(
    name: &str,
    map: &mut Map<String, Value>,
    placed: &BTreeSet<ModuleId>,
    notes: &mut Vec<String>,
) {
    map.retain(|key, value| {
        let mut named = BTreeSet::new();
        modules_named(value, &mut named);
        match named.iter().find(|id| placed.contains(id)) {
            Some(id) => {
                notes.push(printable(&format!(
                    "{key} of workspace '{name}' names '{id}', which this version places itself; a copy is kept"
                )));
                false
            }
            None => true,
        }
    });
}

/// Every module id a string in `value` (at any depth) names.
pub(crate) fn modules_named(value: &Value, out: &mut BTreeSet<ModuleId>) {
    match value {
        Value::String(text) => {
            if let Ok(id) = ModuleId::parse(text) {
                out.insert(id);
            }
        }
        Value::Array(items) => items.iter().for_each(|item| modules_named(item, out)),
        Value::Object(map) => map.values().for_each(|item| modules_named(item, out)),
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_switch_fallback_counts_visible_floats() {
        let t2 = ModuleId::parse("terminal#2").unwrap();
        let mut layout = layout();
        layout
            .add_float(
                &t2,
                crate::layout::FloatRect {
                    cx: 0.5,
                    cy: 0.5,
                    w: 600,
                    h: 300,
                },
                false,
            )
            .unwrap();
        let frame = Frame::new(crate::layout::Size { w: 1280, h: 720 }, 1);
        for id in [ModuleId::agent(), ModuleId::editor()] {
            if let Some(next) = crate::layout::hide(&mut layout, &id, &frame).unwrap() {
                layout.set_focus(&next).unwrap();
            }
        }
        assert_eq!(layout.visible_modules(), [t2.clone()]);
        assert_eq!(
            super::super::reconcile::first_focus(&layout.visible_modules(), &[]),
            Some(t2)
        );
    }

    #[test]
    fn fit_to_drops_instances_the_window_has_not_and_adds_what_it_has() {
        let t2 = ModuleId::parse("terminal#2").unwrap();
        let mut layout = crate::layout::fixture::fixture("row(editor 0.6, column(agent, terminal#3))");
        let notes = fit_to(
            &mut layout,
            &[ModuleId::editor(), ModuleId::agent(), ModuleId::terminal(), t2.clone()],
        );
        assert!(layout.contains(&ModuleId::terminal()) && layout.hidden().contains(&ModuleId::terminal()));
        assert!(layout.contains(&t2) && layout.hidden().contains(&t2));
        assert!(!layout.contains(&ModuleId::parse("terminal#3").unwrap()));
        assert_eq!(
            notes,
            vec![
                "left out 'terminal#3': this window has no such module now".to_string(),
                "placed 'terminal' below the editor, hidden: another workspace has it".to_string(),
                "placed 'terminal#2' below the editor, hidden: another workspace has it".to_string(),
            ]
        );
    }

    #[test]
    fn fit_to_counts_a_floating_module_as_present() {
        let t2 = ModuleId::parse("terminal#2").unwrap();
        let mut layout = layout();
        layout
            .add_float(
                &t2,
                crate::layout::FloatRect {
                    cx: 0.5,
                    cy: 0.5,
                    w: 600,
                    h: 300,
                },
                true,
            )
            .unwrap();
        assert!(fit_to(&mut layout, &[ModuleId::editor(), ModuleId::agent(), t2.clone()]).is_empty());
        assert!(layout.is_floating(&t2));
    }

    #[test]
    fn fit_all_gives_every_workspace_every_module_any_of_them_holds() {
        let t2 = ModuleId::parse("terminal#2").unwrap();
        let mut other = layout();
        other
            .add_float(
                &t2,
                crate::layout::FloatRect {
                    cx: 0.5,
                    cy: 0.5,
                    w: 600,
                    h: 300,
                },
                true,
            )
            .unwrap();
        let mut ws = Workspaces::from_parts(
            names(&["main", "2"]),
            BTreeMap::from([("2".to_string(), other)]),
            "main".into(),
            Kept::default(),
        )
        .unwrap();
        let mut live = layout();
        let notes = ws.fit_all(&mut live);
        assert!(live.contains(&t2) && live.hidden().contains(&t2) && !live.is_floating(&t2));
        assert_eq!(
            notes,
            vec![
                "workspace 'main': placed 'terminal#2' below the editor, hidden: another workspace has it".to_string()
            ]
        );
        assert!(
            ws.stored("2").unwrap().is_floating(&t2),
            "the workspace that had it keeps it as it was"
        );
    }

    use super::*;
    use serde_json::json;

    use crate::layout::fixture::fixture;

    #[test]
    fn a_switch_swaps_the_live_layout_and_switching_back_restores_it_exactly() {
        let mut live = fixture("row(editor 0.6, agent)");
        let mut ws = Workspaces::single();
        assert_eq!(ws.create_copy(&live), "2");
        assert_eq!(ws.order(), ["main", "2"]);
        live.toggle_zoom(&ModuleId::editor());
        live.set_mark(&ModuleId::agent()).unwrap();
        let main_before = live.clone();
        assert_eq!(ws.switch(&mut live, "2", &[]), Ok(Some(ModuleId::editor())));
        assert_eq!(ws.current(), "2");
        assert!(!live.is_zoomed(), "the copy was made before the zoom");
        assert_eq!(live.mark(), Some(&ModuleId::agent()), "the mark is the window's");
        assert_eq!(ws.stored("main").unwrap().mark(), None);
        assert_eq!(ws.last(), Some("main"));
        assert_eq!(ws.switch(&mut live, "main", &[]), Ok(Some(ModuleId::editor())));
        assert!(live.is_zoomed(), "a workspace comes back zoomed as it was left");
        assert_eq!(live.root(), main_before.root());
        assert_eq!(live.mark(), Some(&ModuleId::agent()));
        assert_eq!(ws.switch(&mut live, "main", &[]), Ok(None), "already current");
        assert_eq!(
            ws.switch(&mut live, "nope", &[]),
            Err(WorkspaceError::NoSuch("nope".into()))
        );
    }

    #[test]
    fn a_switch_gives_the_keys_to_a_visible_module_of_the_target() {
        let mut live = layout();
        let mut ws = Workspaces::single();
        ws.create_copy(&live);
        ws.switch(&mut live, "2", &[]).unwrap();
        live.set_focus(&ModuleId::agent()).unwrap();
        live.toggle_zoom(&ModuleId::agent());
        live.set_focus(&ModuleId::editor()).unwrap();
        assert!(!live.is_visible(&ModuleId::editor()), "the keys' module is zoomed away");
        ws.switch(&mut live, "main", &[]).unwrap();
        assert_eq!(
            ws.switch(&mut live, "2", &[]),
            Ok(Some(ModuleId::agent())),
            "the first visible module instead"
        );
        assert_eq!(live.focus(), &ModuleId::agent());
    }

    #[test]
    fn a_switch_without_a_visible_saved_focus_prefers_a_lua_panel_in_the_editors_place() {
        let notes = ModuleId::lua("notes");
        let lua = [crate::layout::ModuleDecl {
            id: notes.clone(),
            placement: crate::layout::Placement::InPlaceOfEditor,
        }];
        let mut two = fixture("row(editor 0.5, column(agent, lua:notes))");
        two.set_focus(&ModuleId::agent()).unwrap();
        assert!(crate::layout::focus_parent(&mut two), "the column is selected");
        two.toggle_zoom(&ModuleId::agent());
        two.set_focus(&ModuleId::editor()).unwrap();
        assert!(
            !two.is_visible(&ModuleId::editor()),
            "the keys' module is outside the zoomed column"
        );
        assert!(two.is_visible(&ModuleId::agent()) && two.is_visible(&notes));
        let mut ws = Workspaces::from_parts(
            names(&["main", "2"]),
            BTreeMap::from([("2".to_string(), two)]),
            "main".into(),
            Kept::default(),
        )
        .unwrap();
        let mut live = layout();
        assert_eq!(
            ws.switch(&mut live, "2", &lua),
            Ok(Some(notes)),
            "not the first visible module, the agent"
        );
    }

    #[test]
    fn prev_and_next_wrap_and_last_is_the_workspace_left_most_recently() {
        let mut live = layout();
        let mut ws = Workspaces::single();
        assert_eq!(ws.step(true), None);
        assert_eq!(ws.last(), None);
        ws.create_copy(&live);
        ws.create_copy(&live);
        assert_eq!(ws.order(), ["main", "2", "3"]);
        assert_eq!(ws.step(true).as_deref(), Some("2"));
        assert_eq!(ws.step(false).as_deref(), Some("3"));
        ws.switch(&mut live, "3", &[]).unwrap();
        assert_eq!(ws.step(true).as_deref(), Some("main"));
        assert_eq!(ws.last(), Some("main"));
        ws.switch(&mut live, "2", &[]).unwrap();
        assert_eq!(ws.last(), Some("3"));
    }

    #[test]
    fn delete_refuses_main_and_the_current_workspace_and_last_forgets_a_deleted_one() {
        let mut live = layout();
        let mut ws = Workspaces::single();
        ws.create_copy(&live);
        ws.create_copy(&live);
        ws.switch(&mut live, "2", &[]).unwrap();
        ws.switch(&mut live, "3", &[]).unwrap();
        assert_eq!(ws.last(), Some("2"));
        assert_eq!(ws.delete("main"), Err(WorkspaceError::Main));
        assert_eq!(ws.delete("3"), Err(WorkspaceError::Current("3".into())));
        assert_eq!(ws.delete("9"), Err(WorkspaceError::NoSuch("9".into())));
        assert_eq!(ws.delete("2"), Ok(()));
        assert_eq!(ws.order(), ["main", "3"]);
        assert_eq!(ws.last(), None);
        assert_eq!(ws.create_copy(&live), "2", "the lowest free number again");
    }

    #[test]
    fn rename_moves_the_current_workspaces_name_and_its_kept_keys() {
        let live = layout();
        let mut kept = Kept::default();
        kept.per_workspace.insert(
            "2".into(),
            serde_json::Map::from_iter([("future".to_string(), json!(1))]),
        );
        let mut ws = Workspaces::from_parts(
            names(&["main", "2"]),
            BTreeMap::from([("main".to_string(), live.clone())]),
            "2".into(),
            kept,
        )
        .unwrap();
        assert_eq!(ws.rename_current("study"), Ok(()));
        assert_eq!(ws.current(), "study");
        assert_eq!(ws.order(), ["main", "study"]);
        assert_eq!(ws.kept_for("study").and_then(|m| m.get("future")), Some(&json!(1)));
        assert_eq!(ws.kept_for("2"), None);
        assert_eq!(ws.rename_current("main"), Err(WorkspaceError::Taken("main".into())));
        assert_eq!(
            ws.rename_current("main").unwrap_err().to_string(),
            "a workspace named 'main' already exists"
        );
        assert_eq!(ws.rename_current("a\nb"), Err(WorkspaceError::Name("a\nb".into())));
        assert_eq!(ws.rename_current("study"), Ok(()), "its own name again changes nothing");
        assert_eq!(Workspaces::single().rename_current("x"), Err(WorkspaceError::Main));
        assert_eq!(
            ws.create("study", live.clone()),
            Err(WorkspaceError::Taken("study".into()))
        );
        assert_eq!(ws.create("review", live), Ok(()));
        assert_eq!(ws.order(), ["main", "study", "review"]);
    }

    #[test]
    fn main_can_never_be_a_rename_target() {
        let mut ws =
            Workspaces::from_parts(vec!["study".into()], BTreeMap::new(), "study".into(), Kept::default()).unwrap();
        assert_eq!(ws.rename_current("main"), Err(WorkspaceError::Main));
        assert_eq!(ws.current(), "study");
        assert_eq!(ws.order(), ["study"]);
    }

    #[test]
    fn main_is_restored_first_and_not_current_only_when_missing() {
        let mut ws = Workspaces::from_parts(
            vec!["study".into(), "review".into()],
            BTreeMap::from([("review".to_string(), layout())]),
            "study".into(),
            Kept::default(),
        )
        .unwrap();
        assert!(ws.restore_main(layout()));
        assert_eq!(ws.order(), ["main", "study", "review"]);
        assert_eq!(ws.current(), "study");
        assert_eq!(ws.stored("main"), Some(&layout()));
        let before = ws.clone();
        assert!(!ws.restore_main(layout()));
        assert_eq!(ws, before);
        assert_eq!(ws.delete("main"), Err(WorkspaceError::Main));
    }

    #[test]
    fn taken_names_at_runtime_and_duplicate_names_in_input_have_distinct_text() {
        let mut live = layout();
        let mut ws = Workspaces::single();
        ws.create("study", live.clone()).unwrap();
        ws.create("other", live.clone()).unwrap();
        ws.switch(&mut live, "other", &[]).unwrap();
        assert_eq!(
            ws.rename_current("study").unwrap_err().to_string(),
            "a workspace named 'study' already exists"
        );
        assert_eq!(
            ws.create("study", live.clone()).unwrap_err().to_string(),
            "a workspace named 'study' already exists"
        );
        let error = Workspaces::from_parts(
            names(&["main", "study", "study"]),
            BTreeMap::from([("study".to_string(), live)]),
            "main".into(),
            Kept::default(),
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "workspace 'study' is named twice");
    }

    #[test]
    fn a_copy_gets_fresh_container_ids_and_no_runtime_state() {
        let mut live = fixture("row(editor 0.6, column(agent, terminal))");
        live.set_mark(&ModuleId::agent()).unwrap();
        live.toggle_zoom(&ModuleId::editor());
        let mut ws = Workspaces::single();
        let name = ws.create_copy(&live);
        let copy = ws.stored(&name).unwrap();
        assert_eq!(copy.root(), live.root(), "the same arrangement");
        let ids: BTreeSet<_> = live.root().ids().into_iter().collect();
        assert!(
            copy.root().ids().iter().all(|id| !ids.contains(id)),
            "no container id shared"
        );
        assert_eq!(copy.mark(), None);
        assert!(!copy.is_zoomed());
        assert_eq!(copy.selection(), crate::layout::Selection::Module);
    }

    #[test]
    fn retiring_a_module_retires_it_in_every_stored_workspace() {
        let frame = crate::layout::Frame::new(crate::layout::Size { w: 1280, h: 721 }, 1);
        let mut lone = layout();
        crate::layout::hide(&mut lone, &ModuleId::agent(), &frame).unwrap();
        let mut ws = Workspaces::from_parts(
            names(&["main", "2"]),
            BTreeMap::from([("2".to_string(), lone)]),
            "main".into(),
            Kept::default(),
        )
        .unwrap();
        ws.retire_everywhere(&ModuleId::editor(), &frame);
        let two = ws.stored("2").unwrap();
        assert!(two.is_gone(&ModuleId::editor()));
        assert!(two.is_shown(&ModuleId::agent()), "something stays on screen");
        assert_eq!(two.focus(), &ModuleId::agent());
    }

    fn layout() -> Layout {
        Layout::initial(&[]).unwrap()
    }

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn workspace_names_are_1_to_32_characters_without_control_characters() {
        for good in [
            "main",
            "a",
            "study",
            "ворк",
            "x y",
            &"é".repeat(32),
            "日本語のワークスペース",
        ] {
            assert_eq!(Workspaces::valid_name(good), Ok(()), "{good:?}");
        }
        for bad in [
            "",
            &"a".repeat(33),
            &"é".repeat(33),
            "a\nb",
            "tab\there",
            "\u{1b}[31m",
            "nul\0",
        ] {
            assert_eq!(
                Workspaces::valid_name(bad),
                Err(WorkspaceError::Name(bad.to_string())),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn single_is_main_alone_and_current() {
        let one = Workspaces::single();
        assert_eq!(one.order(), ["main"]);
        assert_eq!(one.current(), MAIN);
        assert_eq!(one.stored(MAIN), None);
        assert_eq!(one.kept(), &Kept::default());
        let live = layout();
        assert_eq!(one.each(&live), vec![("main", &live)]);
    }

    #[test]
    fn from_parts_refuses_what_does_not_fit() {
        let stored = |list: &[&str]| -> BTreeMap<String, Layout> {
            list.iter().map(|name| (name.to_string(), layout())).collect()
        };
        let ok = Workspaces::from_parts(
            names(&["main", "study"]),
            stored(&["study"]),
            "main".into(),
            Kept::default(),
        )
        .unwrap();
        assert_eq!(ok.order(), ["main", "study"]);
        assert!(ok.stored("study").is_some());

        let cases: Vec<(Vec<String>, BTreeMap<String, Layout>, &str, WorkspaceError)> = vec![
            (
                names(&["main", ""]),
                stored(&[""]),
                "main",
                WorkspaceError::Name("".into()),
            ),
            (
                names(&["main", "a\u{7}"]),
                stored(&["a\u{7}"]),
                "main",
                WorkspaceError::Name("a\u{7}".into()),
            ),
            (
                names(&["main", "main"]),
                stored(&[]),
                "main",
                WorkspaceError::Duplicate("main".into()),
            ),
            (
                names(&["main"]),
                stored(&[]),
                "study",
                WorkspaceError::NoCurrent("study".into()),
            ),
            (
                names(&["main", "study"]),
                stored(&[]),
                "main",
                WorkspaceError::Stored("study".into()),
            ),
            (
                names(&["main"]),
                stored(&["main"]),
                "main",
                WorkspaceError::Stored("main".into()),
            ),
            (
                names(&["main"]),
                stored(&["other"]),
                "main",
                WorkspaceError::Stored("other".into()),
            ),
        ];
        for (order, stored, current, want) in cases {
            assert_eq!(
                Workspaces::from_parts(order.clone(), stored, current.into(), Kept::default()),
                Err(want.clone()),
                "{order:?} current {current}"
            );
        }
    }

    #[test]
    fn kept_keys_of_a_workspace_that_is_not_there_are_dropped() {
        let mut some = Map::new();
        some.insert("floating".into(), json!([]));
        let mut kept = Kept::default();
        kept.per_workspace.insert("main".into(), some.clone());
        kept.per_workspace.insert("gone".into(), some);
        kept.per_workspace.insert("study".into(), Map::new());
        let mut stored = BTreeMap::new();
        stored.insert("study".to_string(), layout());
        let ws = Workspaces::from_parts(names(&["main", "study"]), stored, "main".into(), kept).unwrap();
        assert_eq!(
            ws.kept().per_workspace.keys().collect::<Vec<_>>(),
            ["main"],
            "'gone' is not a workspace, and 'study' keeps nothing"
        );
    }

    #[test]
    fn each_lists_every_workspace_in_order_with_the_live_one_current() {
        let mut stored = BTreeMap::new();
        let mut study = layout();
        study.set_focus(&ModuleId::agent()).unwrap();
        stored.insert("a-first".to_string(), layout());
        stored.insert("study".to_string(), study.clone());
        let ws = Workspaces::from_parts(
            names(&["study", "main", "a-first"]),
            stored,
            "main".into(),
            Kept::default(),
        )
        .unwrap();
        let live = Layout::companion();
        let each = ws.each(&live);
        assert_eq!(
            each.iter().map(|(n, _)| *n).collect::<Vec<_>>(),
            ["study", "main", "a-first"]
        );
        assert_eq!(each[1].1, &live);
        assert_eq!(each[0].1, &study);
    }

    #[test]
    fn kept_modules_lists_every_string_that_parses_once() {
        let mut kept = Kept::default();
        kept.top
            .insert("future".into(), json!({"deep": [["lua:x", "not a module"]]}));
        let mut main = Map::new();
        main.insert(
            "floating".into(),
            json!([{"module": "lua:x", "w": 900}, {"module": "terminal"}]),
        );
        kept.per_workspace.insert("main".into(), main);
        let ws = Workspaces::from_parts(names(&["main"]), BTreeMap::new(), "main".into(), kept).unwrap();
        assert_eq!(ws.kept_modules(), vec![ModuleId::lua("x"), ModuleId::terminal()]);
    }

    #[test]
    fn retake_kept_takes_the_files_keys_and_keeps_a_workspace_the_file_lacks() {
        let mut stored = BTreeMap::new();
        stored.insert("study".to_string(), layout());
        stored.insert("review".to_string(), layout());
        let mut kept = Kept::default();
        kept.top.insert("old".into(), json!(1));
        let mut floating = Map::new();
        floating.insert("floating".into(), json!([]));
        kept.per_workspace.insert("study".into(), floating.clone());
        kept.per_workspace.insert("review".into(), floating.clone());
        let mut ws = Workspaces::from_parts(names(&["main", "study", "review"]), stored, "main".into(), kept).unwrap();

        let mut now = Kept::default();
        now.top.insert("future".into(), json!(2));
        let mut memory = Map::new();
        memory.insert("float_memory".into(), json!([]));
        now.per_workspace.insert("main".into(), memory.clone());
        now.per_workspace.insert("review".into(), Map::new());
        now.per_workspace.insert("elsewhere".into(), memory.clone());
        ws.retake_kept(now);
        assert_eq!(ws.kept().top.keys().collect::<Vec<_>>(), ["future"]);
        assert_eq!(ws.kept_for("main"), Some(&memory), "the file's keys for 'main'");
        assert_eq!(ws.kept_for("review"), None, "the file's 'review' keeps nothing now");
        assert_eq!(
            ws.kept_for("study"),
            Some(&floating),
            "the file has no 'study': this window's copy stays"
        );
        assert_eq!(ws.kept_for("elsewhere"), None, "not a workspace of this window");
    }

    #[test]
    fn every_workspace_gains_and_loses_an_instance() {
        let id = ModuleId::parse("terminal#2").unwrap();
        let mut ws = Workspaces::from_parts(
            names(&["main", "study"]),
            [("study".into(), layout())].into(),
            "main".into(),
            Kept::default(),
        )
        .unwrap();
        ws.add_everywhere(&id);
        let study = ws.stored("study").unwrap();
        assert!(study.contains(&id));
        assert!(study.hidden().contains(&id));
        let mut expected = layout();
        expected.replace_root(
            super::super::tree::place_new(expected.root_node(), &id, super::super::Placement::BelowEditor).0,
        );
        assert_eq!(study.root(), expected.root());
        ws.add_everywhere(&id);
        let study = ws.stored_mut().next().unwrap();
        study.show(&id).unwrap();
        study.set_focus(&id).unwrap();
        ws.remove_everywhere(&id);
        let study = ws.stored("study").unwrap();
        assert!(!study.contains(&id));
        assert_eq!(study.focus(), &ModuleId::editor());
    }

    #[test]
    fn a_kept_key_naming_a_module_any_workspace_now_holds_is_dropped_with_a_note() {
        use super::super::ModuleDecl;
        let id = ModuleId::parse("terminal#2").unwrap();
        let mut study = layout();
        study.replace_root(
            super::super::tree::place_new(study.root_node(), &id, super::super::Placement::BelowEditor).0,
        );
        let ws = Workspaces::from_parts(
            names(&["main", "study"]),
            [("study".to_string(), study)].into(),
            MAIN.to_string(),
            Kept::default(),
        )
        .unwrap();
        let panel = ModuleDecl {
            id: ModuleId::lua("side"),
            placement: super::super::Placement::RightOfRoot,
        };
        let placed = ws.placed(&layout(), std::slice::from_ref(&panel));
        assert!(placed.contains(&id), "held by a workspace not on screen");
        assert!(placed.contains(&panel.id));
        assert!(placed.contains(&ModuleId::terminal()));
        assert!(!placed.contains(&ModuleId::parse("terminal#3").unwrap()));

        let mut kept = Kept::default();
        let mut main = Map::new();
        main.insert("floating".to_string(), json!([{"module": "terminal#2"}]));
        main.insert("float_memory".to_string(), json!([{"module": "terminal#3"}]));
        kept.per_workspace.insert("main".to_string(), main);
        kept.top.insert("floating".to_string(), json!(["terminal#2"]));
        let notes = kept.drop_placed(&placed);
        assert_eq!(
            notes,
            ["floating of workspace 'main' names 'terminal#2', which this version places itself; a copy is kept"]
        );
        assert_eq!(
            kept.per_workspace["main"].keys().collect::<Vec<_>>(),
            ["float_memory"],
            "a key naming only modules this window never places stays"
        );
        assert!(
            kept.top.contains_key("floating"),
            "only workspace-level keys are checked"
        );
    }

    #[test]
    fn an_instance_named_only_in_kept_data_keeps_its_number() {
        use super::super::persist::{decode_v2, encode_v2};
        let root = std::path::Path::new("/project");
        let mut file: Value = serde_json::from_str(&encode_v2(&Workspaces::single(), &layout(), root)).unwrap();
        file["workspaces"][0]["overlays"] = json!([{"module": "terminal#2"}]);
        let restored = decode_v2(&file.to_string(), root, &[]).unwrap();
        let id = ModuleId::parse("terminal#2").unwrap();
        assert_eq!(restored.workspaces.reserved(), [id.clone()]);
        assert!(restored.workspaces.kept_for("main").unwrap().contains_key("overlays"));
        let mut live = layout();
        live.replace_root(super::super::tree::place_new(live.root_node(), &id, super::super::Placement::BelowEditor).0);
        file["workspaces"][0]["tiled"] = serde_json::from_str::<Value>(&encode_v2(&Workspaces::single(), &live, root))
            .unwrap()["workspaces"][0]["tiled"]
            .clone();
        let restored = decode_v2(&file.to_string(), root, &[]).unwrap();
        assert!(restored.workspaces.reserved().is_empty());
        assert!(restored.workspaces.kept_for("main").is_none());
        assert!(restored.keep_copy.is_some());
    }

    #[test]
    fn a_module_added_everywhere_goes_where_it_is_declared() {
        use crate::layout::module::{ModuleDecl, Placement};
        let t2 = ModuleId::parse("terminal#2").unwrap();
        let mut live = layout();
        let mut ws = Workspaces::single();
        ws.create_copy(&live);
        ws.add_everywhere_at(&t2, Placement::RightOfRoot);
        let stored = ws
            .each(&live)
            .into_iter()
            .find(|(name, _)| *name != ws.current())
            .unwrap()
            .1
            .clone();
        assert!(stored.hidden().contains(&t2));
        assert_eq!(stored.leaves().last(), Some(&t2), "right of the root");

        let t3 = ModuleId::parse("terminal#3").unwrap();
        let decls = [ModuleDecl {
            id: ModuleId::terminal(),
            placement: Placement::RightOfRoot,
        }];
        let notes = fit_to_with(&mut live, &[t3.clone()], &decls);
        assert_eq!(live.leaves().last(), Some(&t3), "its kind's declaration");
        assert!(live.hidden().contains(&t3));
        assert!(notes.iter().any(|n| n.contains("'terminal#3'")), "{notes:?}");
        assert!(
            built_in_and_registered(&[
                ModuleDecl {
                    id: ModuleId::parse("terminal#4").unwrap(),
                    placement: Placement::BelowEditor
                },
                ModuleDecl {
                    id: ModuleId::lua("notes"),
                    placement: Placement::RightOfRoot
                },
            ])
            .into_iter()
            .eq([
                ModuleId::editor(),
                ModuleId::agent(),
                ModuleId::terminal(),
                ModuleId::lua("notes")
            ]
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>()),
            "a built-in's or an instance's declaration does not make it placed by this window"
        );
    }

    fn away_frame() -> Frame<'static> {
        Frame::new(crate::layout::Size { w: 1280, h: 721 }, 1)
    }

    fn rects_of(layout: &Layout) -> BTreeMap<ModuleId, crate::layout::Rect> {
        crate::layout::arrange(layout, &away_frame())
            .modules
            .into_iter()
            .collect()
    }

    #[test]
    fn an_away_module_keeps_its_leaf_in_every_workspace_and_takes_no_space() {
        let term = ModuleId::terminal();
        let mut live = fixture("row(editor 0.6, column(agent, terminal))");
        let mut ws = Workspaces::from_parts(
            names(&["main", "2", "3"]),
            BTreeMap::from([
                ("2".to_string(), fixture("column(row(editor, agent), terminal)")),
                (
                    "3".to_string(),
                    fixture("row(editor 0.6, column(agent, terminal hidden))"),
                ),
            ]),
            "main".into(),
            Kept::default(),
        )
        .unwrap();
        let before: Vec<Layout> = ws.each(&live).into_iter().map(|(_, l)| l.clone()).collect();
        assert_eq!(live.set_away(&term, true), Ok(true));
        assert_eq!(live.set_away(&term, true), Ok(false), "a second call changes nothing");
        ws.set_away_everywhere(&term, true);
        for ((name, layout), before) in ws.each(&live).into_iter().zip(&before) {
            assert!(layout.is_away(&term), "{name}: away");
            assert!(layout.leaves().contains(&term), "{name}: its leaf stays in the tree");
            assert!(layout.modules().contains(&term), "{name}: its number stays counted");
            assert_eq!(layout.away(), BTreeSet::from([term.clone()]), "{name}");
            assert_eq!(layout.root(), before.root(), "{name}: the tree is untouched");
            assert!(!rects_of(layout).contains_key(&term), "{name}: it gets no space");
            let mut hidden = before.clone();
            if hidden.is_shown(&term) {
                crate::layout::hide(&mut hidden, &term, &away_frame()).unwrap();
            }
            assert_eq!(
                rects_of(layout),
                rects_of(&hidden),
                "{name}: the others share the space as for a hide"
            );
            assert!(
                !layout.saved_hidden().contains(&term) || before.hidden().contains(&term),
                "{name}: the state file keeps it as the user left it"
            );
        }
        assert_eq!(live.show(&term), Err(LayoutError::Away(term.clone())));
        assert_eq!(live.set_mark(&term), Err(LayoutError::Away(term.clone())));
        assert_eq!(
            kill(&mut live, &term, Reopen::Never, &away_frame()),
            Err(LayoutError::Away(term.clone()))
        );
        assert_eq!(LayoutError::Away(term).to_string(), "'terminal' is in the other window");
    }

    #[test]
    fn a_module_back_from_away_is_where_it_was_in_every_workspace() {
        use crate::layout::{Child, Con, ConLayout, FloatRect, Node};
        let t2 = ModuleId::parse("terminal#2").unwrap();
        // Window one's main workspace: the instance is the drawn tab beside the chat, the editor has the keys.
        let mut tabs = Con::new(
            ConLayout::Tabbed,
            vec![
                Child::new(Node::Leaf(ModuleId::agent()), 0.5),
                Child::new(Node::Leaf(t2.clone()), 0.5),
            ],
        );
        tabs.focus = 1;
        let mut live = Layout::new(
            Node::Con(Con::new(
                ConLayout::SplitH,
                vec![
                    Child::new(Node::Leaf(ModuleId::editor()), 0.6),
                    Child::new(Node::Con(tabs), 0.4),
                ],
            )),
            ModuleId::editor(),
        )
        .unwrap();
        // Workspace 2: it floats, and has the keys.
        let mut floating = fixture("row(editor 0.6, agent)");
        let rect = FloatRect {
            cx: 0.3,
            cy: 0.6,
            w: 500,
            h: 280,
        };
        floating.add_float(&t2, rect, false).unwrap();
        floating.set_focus(&t2).unwrap();
        // Workspace 3: hidden below the chat.
        let hidden = fixture("row(editor 0.6, column(agent, terminal#2 hidden))");
        let mut ws = Workspaces::from_parts(
            names(&["main", "2", "3"]),
            BTreeMap::from([("2".to_string(), floating), ("3".to_string(), hidden)]),
            "main".into(),
            Kept::default(),
        )
        .unwrap();
        let before: Vec<Layout> = ws.each(&live).into_iter().map(|(_, l)| l.clone()).collect();
        assert!(live.is_visible(&t2));

        live.set_away(&t2, true).unwrap();
        ws.set_away_everywhere(&t2, true);
        assert!(
            live.is_visible(&ModuleId::agent()),
            "the tab beside it is drawn meanwhile"
        );
        let two = ws.stored("2").unwrap();
        assert_ne!(two.focus(), &t2, "the keys leave with it");
        assert!(two.is_shown(two.focus()));
        assert!(rects_of(two).len() == 2 && two.float_of(&t2).unwrap().rect == rect);

        live.set_away(&t2, false).unwrap();
        ws.set_away_everywhere(&t2, false);
        for ((name, layout), before) in ws.each(&live).into_iter().zip(&before) {
            assert!(!layout.is_away(&t2) && layout.away().is_empty(), "{name}");
            assert_eq!(layout.root(), before.root(), "{name}: the same tree, tabs and all");
            assert_eq!(
                layout.floats(),
                before.floats(),
                "{name}: the same floats and rectangles"
            );
            assert_eq!(layout.hidden(), before.hidden(), "{name}: shown or hidden as before");
            assert_eq!(rects_of(layout), rects_of(before), "{name}: on screen where it was");
        }
        assert!(live.is_visible(&t2), "the drawn tab again");
        assert_ne!(
            ws.stored("2").unwrap().focus(),
            &t2,
            "coming back does not take the keys"
        );
    }

    #[test]
    fn fit_all_and_retire_everywhere_leave_away_modules_alone() {
        let t2 = ModuleId::parse("terminal#2").unwrap();
        let mut live = fixture("row(editor 0.6, column(agent, terminal#2))");
        live.set_away(&t2, true).unwrap();
        let live_before = live.clone();
        let mut ws = Workspaces::from_parts(
            names(&["main", "2"]),
            BTreeMap::from([("2".to_string(), layout())]),
            "main".into(),
            Kept::default(),
        )
        .unwrap();
        ws.fit_all(&mut live);
        assert_eq!(live, live_before, "the live layout already held it");
        let two = ws.stored("2").unwrap();
        assert!(two.contains(&t2), "a workspace that lacked it holds its leaf");
        assert!(two.is_away(&t2), "and holds it away: the other window has it");
        assert!(!two.is_shown(&t2));
        let mut back = two.clone();
        back.set_away(&t2, false).unwrap();
        assert!(
            back.hidden().contains(&t2),
            "it was added hidden, and comes back hidden"
        );

        let two_before = ws.stored("2").unwrap().clone();
        ws.retire_everywhere(&t2, &away_frame());
        assert_eq!(
            ws.stored("2").unwrap(),
            &two_before,
            "an away module is not retired here"
        );
        ws.retire_everywhere(&ModuleId::editor(), &away_frame());
        let two = ws.stored("2").unwrap();
        assert!(two.is_gone(&ModuleId::editor()));
        assert!(
            two.is_away(&t2) && two.contains(&t2) && !two.is_gone(&t2),
            "the editor's retirement leaves it be"
        );
        assert!(two.is_shown(&ModuleId::agent()));
    }

    #[test]
    fn the_last_module_on_screen_going_away_leaves_the_chat_in_its_place() {
        let t2 = ModuleId::parse("terminal#2").unwrap();
        let mut live = fixture("row(editor hidden, column(agent hidden, terminal#2 focus))");
        live.set_away(&t2, true).unwrap();
        assert!(live.is_shown(&ModuleId::agent()), "a window always shows something");
        assert_eq!(live.focus(), &ModuleId::agent());
        let mut lone = Layout::new(crate::layout::Node::Leaf(ModuleId::agent()), ModuleId::agent()).unwrap();
        assert_eq!(
            lone.set_away(&ModuleId::agent(), true),
            Err(LayoutError::LastVisible(ModuleId::agent()))
        );
        assert_eq!(
            lone.set_away(&t2, true),
            Err(LayoutError::NotInTree(t2.clone())),
            "only a module the layout holds"
        );
    }
}
