//! Chooses the window's initial layout from the per-project state file, the default layout in
//! `init.lua`, or the built-in first launch. The core persistence module handles the file
//! format, reconciliation and failures.
//!
//! Arrangement changes are saved 500ms after the last change. At window close, an unwritten
//! change is saved synchronously.
//!
//! Only arrangement changes trigger a save: each workspace's tree, shares and pinned lengths,
//! floating rectangles and remembered rectangles, hidden modules, and the current workspace
//! ([`Arrangement`]). Each save writes all of this window's workspaces with keyboard focus as
//! it is when the save occurs.
//!
//! Moving keyboard focus by a click, `Ctrl+h/j/k/l` or a HINT label does not trigger a write,
//! even at close. Raising the float that receives focus does not trigger a write either. Focus
//! is saved along with the next arrangement change. Reopening therefore restores the focus at
//! the last arrangement save, which may differ from the focus at close.
//!
//! Launching, settling a pinned row's initial length (`ModuleGrid::connect_settled`) and
//! zooming do not count as changes. Zoom is not stored. Opening, zooming, moving focus and
//! closing without changing the arrangement leaves the file alone and keeps following the
//! default layout, including one added to `init.lua` later.
//!
//! With two windows on one project (`NON_UNIQUE`), each writes all its own workspaces and the
//! last write wins (`persist::save`). A zoom or drag can delay a save, so an earlier change can
//! be written after a later change in another window. The saved arrangement is not merged with
//! the file: this window's workspaces replace the other window's changes, including removals
//! and renames. A window that only moves focus writes nothing and cannot overwrite another
//! window's arrangement.
//!
//! A module absent from this window's layouts, such as an unregistered Lua panel or a later
//! build's `canvas`, leaves the file at the next arrangement save. Moving focus alone leaves it
//! in the file.
//!
//! Before every write, the file is read again. A file this build cannot use at all is set aside
//! whole as `.v2.json.unusable` (`persist::set_aside_v2`). A file it can carry only in part,
//! such as one containing an unknown workspace or container layout, is first copied to
//! `.v2.json.partial` (`persist::copy_aside_v2`).
//!
//! Keys preserved for a later build are taken from the current file
//! (`Workspaces::retake_kept`), so a window opened earlier does not erase newly written keys it
//! cannot interpret. If there is no file or the file is unusable, the window keeps the keys it
//! opened with. A preserved entry naming a module already placed in any of this window's
//! workspaces is excluded (`Kept::drop_placed`) to avoid placing that module twice. The file is
//! copied to `.v2.json.partial` in that case.
//!
//! A file read only in part at launch is copied aside before the first write only if it is
//! still byte-identical to the bytes read at launch. If another window has replaced it, this
//! window must not overwrite that window's backup with its older copy.
//!
//! Saving focus alone into the file's current arrangement can drop a module placed by another
//! window. Saving only after arrangement changes avoids that merge.
//!
//! The later build's preserved keys are the only entries taken from the current file at write
//! time; the arrangement comes from this window.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use eitri_core::layout::persist::{self, Loaded, V2State};
use eitri_core::layout::rules::{OpenPoint, NOMINAL_GRID};
use eitri_core::layout::workspace::MAIN;
use eitri_core::layout::{
    apply_launch_rule, first_launch_decls, reconcile_default_floating, Con, FloatRect, Layout, ModuleDecl, ModuleId,
    ModuleKind, Node, OpenRules, Placement, Workspaces,
};
use eitri_core::lua::layout::LuaTree;
use gtk4::glib;

/// The workspaces and the current layout a window opens with, the lines to log about how they were
/// chosen, and the text of a file this window opened only in part (copied aside before its first
/// write; `None` otherwise).
///
/// - `default`: `eitri.layout.default`'s tree, if `init.lua` gave one; an error there is the
///   caller's startup failure, not handled here.
/// - `loaded`: this project's state file, if the state directory is known.
///
/// The file wins whenever it can be used: a project reopens as it was left, a version 1 file
/// migrated into one workspace. Otherwise the Lua default, reconciled like a file (a module this
/// window lacks is left out, the terminal placed if missing); otherwise the built-in first launch
/// (`terminal::initial_layout`). Both of those are one workspace, `main`.
///
/// Whichever it is, a layout that hides the editor says so: nvim is started by the editor's first
/// frame (`neovide-editor`'s render callback builds it), so neither nvim nor the window's colours,
/// which follow nvim's colorscheme, come until the editor is shown. `prefix x` in the editor makes
/// that one keypress and one relaunch away. `show_editor` is the key that shows it, as the effective
/// keymap binds it (`Ctrl+b e` by default), so the note names it; `None` when nothing binds one, and
/// the note then names no key rather than a wrong one.
///
/// `lua` is the window's module declarations (the Lua panels', and where the open rules put modules
/// when nothing holds the keys). A first launch -- the Lua default included -- then applies `rules`'
/// floats, scratchpads and sizes to what it placed itself: the base terminal and each Lua panel, all
/// of them with no Lua default, else those the default's tree does not name, sized against
/// [`NOMINAL_GRID`] since the window has no size yet. A saved layout is never changed by a rule: it is
/// how the project was left, as it wins over `eitri.layout.default`.
pub(crate) fn choose_startup_layout(
    default: Option<&LuaTree>,
    loaded: Option<Loaded>,
    lua: &[ModuleDecl],
    rules: &OpenRules,
    show_editor: Option<&str>,
) -> Result<(Workspaces, Layout, Vec<String>, Option<String>), String> {
    let mut notes = Vec::new();
    let first_launch = |notes: &mut Vec<String>| -> Result<Layout, String> {
        let (mut layout, named) = match default {
            Some(tree) => {
                let r = reconcile_default_floating(tree.tiled.clone(), tree.floats.clone(), lua)
                    .map_err(|e| format!("eitri.layout.default: {e}"))?;
                notes.extend(r.notes.into_iter().map(|n| format!("eitri.layout.default: {n}")));
                let named: Vec<ModuleId> = tree
                    .tiled
                    .leaves()
                    .into_iter()
                    .chain(tree.floats.iter().map(|float| float.id.clone()))
                    .collect();
                (r.layout, named)
            }
            None => (
                crate::terminal::initial_layout(lua).map_err(|e| e.to_string())?,
                Vec::new(),
            ),
        };
        let mut placed: Vec<(ModuleId, Option<Placement>)> = vec![(ModuleId::terminal(), None)];
        placed.extend(
            first_launch_decls(lua)
                .into_iter()
                .filter(|d| d.id.kind() == ModuleKind::LuaWebview)
                .map(|d| (d.id, Some(d.placement))),
        );
        for (id, position) in placed.into_iter().filter(|(id, _)| !named.contains(id)) {
            let (rule, _) = rules.resolve(&id, OpenPoint::Launch, position);
            match apply_launch_rule(&mut layout, &id, &rule, NOMINAL_GRID) {
                Ok(applied) => notes.extend(applied),
                Err(err) => notes.push(format!("open rule for '{id}' not applied at the first launch: {err}")),
            }
        }
        Ok(layout)
    };
    let (workspaces, layout, keep_copy) = match loaded {
        Some(Loaded::Restored(mut opened)) | Some(Loaded::Migrated(mut opened)) => {
            notes.extend(std::mem::take(&mut opened.notes));
            // Every window has `main`, and the export writes it as `eitri.layout.default`: a file that
            // lost it gets it back as a first launch would build it, so the window never runs without it.
            let main_lost = !opened.workspaces.order().iter().any(|name| name == MAIN);
            if main_lost {
                let main = first_launch(&mut notes)?;
                opened.workspaces.restore_main(main);
                notes.push(format!(
                    "workspace '{MAIN}' could not be read; it is the default layout again"
                ));
            }
            notes.push("reopened as it was left".to_string());
            // A `main` put back is shaped by the Lua default, so saying it did not apply would be wrong.
            if default.is_some() && !main_lost {
                notes.push("eitri.layout.default shapes a project with no saved layout; this one has one".to_string());
            }
            if opened.keep_copy.is_some() {
                notes.push(
                    "what this build cannot carry of the file is kept: the file is copied as .v2.json.partial \
                     before this window first writes it"
                        .to_string(),
                );
            }
            (opened.workspaces, opened.current, opened.keep_copy)
        }
        Some(Loaded::Unusable(why)) => {
            notes.push(format!(
                "{why}; opening the default layout (a version 2 file is kept: it is set aside as .v2.json.unusable \
                 the first time this window saves; a version 1 file is never written)"
            ));
            (Workspaces::single(), first_launch(&mut notes)?, None)
        }
        Some(Loaded::Missing) | None => (Workspaces::single(), first_launch(&mut notes)?, None),
    };
    if !layout.is_shown(&ModuleId::editor()) {
        let way_back = show_editor.map(|key| format!(" ({key})")).unwrap_or_default();
        notes.push(format!(
            "the editor is hidden in this layout: nvim, and the window's colours that follow it, wait until it \
             is shown{way_back}"
        ));
    }
    Ok((workspaces, layout, notes, keep_copy))
}

/// Appends missing init.lua templates after saved workspaces, then shares every module across them.
pub(crate) fn add_defined_workspaces(
    workspaces: &mut Workspaces,
    live: &mut Layout,
    defined: &[(String, LuaTree)],
    lua: &[ModuleDecl],
) -> Result<Vec<String>, String> {
    let mut notes = Vec::new();
    for (name, tree) in defined {
        if workspaces.order().contains(name) {
            notes.push(format!(
                "workspace '{name}': the saved layout wins over eitri.layout.workspace"
            ));
            continue;
        }
        let reconciled = reconcile_default_floating(tree.tiled.clone(), tree.floats.clone(), lua)
            .map_err(|err| format!("eitri.layout.workspace('{name}'): {err}"))?;
        notes.extend(
            reconciled
                .notes
                .into_iter()
                .map(|note| format!("workspace '{name}': {note}")),
        );
        workspaces
            .create(name, reconciled.layout)
            .map_err(|err| format!("eitri.layout.workspace('{name}'): {err}"))?;
    }
    notes.extend(workspaces.fit_all_with(live, lua));
    Ok(notes)
}

/// What the state file keeps of the window besides the keys and a later build's kept keys
/// (`persist::encode_v2`): every workspace's tree with its shares and pinned lengths, its floats and
/// remembered float rectangles, and what it hides, in order, and which one is current. A change of
/// this, and only of this, writes the file (this module's doc); zoom and the MRU order are not stored
/// at all, and the keys ride along. Container ids are not compared (`Con`'s equality ignores them).
#[derive(Debug, Clone, PartialEq)]
struct Arrangement {
    current: String,
    workspaces: Vec<WorkspaceArrangement>,
}

#[derive(Debug, Clone, PartialEq)]
struct WorkspaceArrangement {
    name: String,
    root: Con,
    hidden: BTreeSet<ModuleId>,
    /// Sorted by module, not in z-order: giving a float the keys raises it, and a move of the keys
    /// must not write the file. The order is still written, as it is when something else is.
    floats: Vec<(ModuleId, FloatRect, bool)>,
    float_memory: BTreeMap<ModuleId, FloatRect>,
}

impl Arrangement {
    fn of(workspaces: &Workspaces, current: &Layout) -> Arrangement {
        Arrangement {
            current: workspaces.current().to_string(),
            workspaces: workspaces
                .each(current)
                .into_iter()
                .map(|(name, layout)| {
                    let mut floats: Vec<_> = layout
                        .floats()
                        .iter()
                        .map(|float| (float.id.clone(), float.rect, float.scratch))
                        .collect();
                    floats.sort_by(|a, b| a.0.cmp(&b.0));
                    WorkspaceArrangement {
                        name: name.to_string(),
                        root: layout.root().clone(),
                        hidden: layout.saved_hidden(),
                        floats,
                        float_memory: layout.float_memory().clone(),
                    }
                })
                .collect(),
        }
    }

    /// The current workspace's tree: the one a pinned row settles in on screen.
    fn current_root_mut(&mut self) -> Option<&mut Con> {
        let current = &self.current;
        self.workspaces
            .iter_mut()
            .find(|ws| ws.name == *current)
            .map(|ws| &mut ws.root)
    }
}

/// How much longer the debounce waits, given how long ago the last change was: `None` once
/// [`persist::SAVE_DEBOUNCE_MS`] has passed. The timer is armed once for a burst of changes -- a
/// divider drag reports one per motion event -- and, when it fires early, armed again for what is
/// left, rather than removed and re-added for every change.
fn debounce_left(since_last_change: Duration) -> Option<Duration> {
    Duration::from_millis(persist::SAVE_DEBOUNCE_MS)
        .checked_sub(since_last_change)
        .filter(|left| !left.is_zero())
}

/// `base` with each pinned length it did not have yet taken from `now`: what `settle_pins` just gave
/// the layout. Containers are matched child by child where both have the same orientation and the
/// same number of children; where the two trees differ, `base` is left as it is.
fn take_settled_lengths(base: &mut Con, now: &Con) {
    if base.layout != now.layout || base.children.len() != now.children.len() {
        return;
    }
    for (base_child, now_child) in base.children.iter_mut().zip(&now.children) {
        if let (Some(base_pin), Some(now_pin)) = (base_child.pin.as_mut(), now_child.pin) {
            if base_pin.px.is_none() {
                base_pin.px = now_pin.px;
            }
        }
        if let (Node::Con(base_con), Node::Con(now_con)) = (&mut base_child.node, &now_child.node) {
            take_settled_lengths(base_con, now_con);
        }
    }
}

/// Writes the state file [`persist::SAVE_DEBOUNCE_MS`] after the last change of the arrangement, and
/// at close -- what, and when, is this module's doc.
pub(crate) struct LayoutSaver {
    dir: Option<PathBuf>,
    project_root: PathBuf,
    /// The current workspace's layout, the one on screen.
    layout: Rc<RefCell<Layout>>,
    /// Every workspace of the window; the current one's layout is `layout`.
    workspaces: Rc<RefCell<Workspaces>>,
    /// The registered Lua panels: the file read again before a write is decoded as `load` decodes
    /// it, so that "unusable" and "partly usable" mean what they meant when the window opened.
    lua: Vec<ModuleDecl>,
    /// The exact text of a file this window opened only in part, until its first write copies it
    /// aside (or finds the file is no longer those bytes).
    keep_copy: RefCell<Option<String>>,
    pending: RefCell<Option<glib::SourceId>>,
    /// When the grid last reported a change: what a debounce timer that fires checks.
    last_change: Cell<Option<Instant>>,
    /// What a change is measured from: the arrangement the window opened with (its pinned lengths as
    /// they settled), or the one it last wrote.
    base: RefCell<Arrangement>,
}

impl LayoutSaver {
    /// `dir` is `persist::state_dir`'s answer; `None` (no usable `XDG_STATE_HOME` or `HOME`) saves
    /// nothing, said once here. `layout` and `workspaces` are the startup ones, not yet changed: a
    /// launch is not a change, whether it opened a file, the Lua default or the first launch.
    /// `keep_copy` is `choose_startup_layout`'s.
    pub(crate) fn new(
        dir: Option<PathBuf>,
        project_root: &Path,
        layout: Rc<RefCell<Layout>>,
        workspaces: Rc<RefCell<Workspaces>>,
        lua: Vec<ModuleDecl>,
        keep_copy: Option<String>,
    ) -> Rc<LayoutSaver> {
        if dir.is_none() {
            eprintln!(
                "[layout] no usable state directory (XDG_STATE_HOME absolute, or HOME absolute); this window's \
                 layout will not be kept"
            );
        }
        let base = Arrangement::of(&workspaces.borrow(), &layout.borrow());
        Rc::new(LayoutSaver {
            dir,
            project_root: project_root.to_path_buf(),
            layout,
            workspaces,
            lua,
            keep_copy: RefCell::new(keep_copy),
            pending: RefCell::new(None),
            last_change: Cell::new(None),
            base: RefCell::new(base),
        })
    }

    /// The grid's change hook: the layout changed, so it is compared and written 500ms after the
    /// last change, or at close. A change that leaves the arrangement as it was -- a zoom, or its end,
    /// which a move of the keys out of a zoomed module also reports -- arms nothing, and a change
    /// while the timer already waits only moves the time it waits for ([`debounce_left`]). The grid's
    /// own startup `apply` never reaches this hook: `main` connects it after that `apply`.
    pub(crate) fn changed(self: &Rc<Self>) {
        if self.dir.is_none() {
            return;
        }
        self.last_change.set(Some(Instant::now()));
        if self.pending.borrow().is_some() || !self.arrangement_changed() {
            return;
        }
        self.arm(Duration::from_millis(persist::SAVE_DEBOUNCE_MS));
    }

    fn arm(self: &Rc<Self>, after: Duration) {
        let saver = Rc::downgrade(self);
        let source = glib::timeout_add_local_once(after, move || {
            let Some(saver) = saver.upgrade() else { return };
            saver.pending.borrow_mut().take();
            let since = saver.last_change.get().map_or(Duration::MAX, |at| at.elapsed());
            match debounce_left(since) {
                Some(left) => saver.arm(left),
                None => saver.save_now(),
            }
        });
        *self.pending.borrow_mut() = Some(source);
    }

    /// The arrangement now; `None` while the layout or the workspaces are borrowed.
    fn arrangement(&self) -> Option<Arrangement> {
        let layout = self.layout.try_borrow().ok()?;
        let workspaces = self.workspaces.try_borrow().ok()?;
        Some(Arrangement::of(&workspaces, &layout))
    }

    /// Whether the arrangement differs from `base`. `false` while either is borrowed: the timer is
    /// armed by the next change, and the close handler saves whatever it is.
    fn arrangement_changed(&self) -> bool {
        self.arrangement().is_some_and(|now| now != *self.base.borrow())
    }

    /// The grid's settle hook: a pinned split got its length on screen. That is not a change, so the
    /// length joins `base`.
    pub(crate) fn settled(&self) {
        if let Ok(layout) = self.layout.try_borrow() {
            if let Some(root) = self.base.borrow_mut().current_root_mut() {
                take_settled_lengths(root, layout.root());
            }
        }
    }

    /// Before the first write after opening a file only in part: its bytes copied aside, if the file
    /// is still those bytes. `false` when that could not be done and nothing may be written yet; the
    /// copy is tried again at the next save.
    fn copy_what_was_opened(&self, dir: &Path) -> bool {
        let Some(opened) = self.keep_copy.borrow().clone() else {
            return true;
        };
        match persist::read_v2_text(dir, &self.project_root) {
            Ok(Some(text)) if text == opened => match persist::copy_aside_v2(dir, &self.project_root, &opened) {
                Ok(aside) => println!(
                    "[layout] the file this window opened, of which this build could not carry everything, is \
                     kept as {}",
                    aside.display()
                ),
                Err(err) => {
                    eprintln!("[layout] could not copy aside the file this window opened ({err}); not saved");
                    return false;
                }
            },
            // Another window wrote since, or the file is gone: what is there is not the text this
            // window opened, and not this window's to keep.
            Ok(_) => {}
            Err(err) => {
                eprintln!("[layout] could not read the file to copy it aside ({err}); not saved");
                return false;
            }
        }
        *self.keep_copy.borrow_mut() = None;
        true
    }

    /// The file as it is now, made safe to write over: wholly unusable, set aside; partly usable, its
    /// text copied aside; usable or partly, its kept keys taken. A kept key naming a module this
    /// window places -- in any of its workspaces now, not only in the file's trees -- is not taken: a
    /// later build may have floated a module this window has since tiled, and writing both would put
    /// it in two places; the file's text is copied aside instead, as for a partly usable file.
    /// `false` when that could not be done and nothing may be written.
    fn make_way(&self, dir: &Path) -> bool {
        let (text, mut kept, mut copy) = match persist::v2_state(dir, &self.project_root, &self.lua) {
            V2State::Missing => return true,
            V2State::Usable(text, kept) => (text, kept, false),
            V2State::Partly(text, kept) => (text, kept, true),
            V2State::Unusable(_) => {
                return match persist::set_aside_v2(dir, &self.project_root) {
                    Ok(aside) => {
                        println!(
                            "[layout] the file this build could not use is kept as {}",
                            aside.display()
                        );
                        true
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => true,
                    Err(err) => {
                        eprintln!("[layout] could not set aside the file this build could not use ({err}); not saved");
                        false
                    }
                };
            }
        };
        let placed = match (self.layout.try_borrow(), self.workspaces.try_borrow()) {
            (Ok(layout), Ok(workspaces)) => workspaces.placed(&layout, &self.lua),
            _ => {
                eprintln!("[layout] the layout was busy; not saved this time");
                return false;
            }
        };
        let dropped = kept.drop_placed(&placed);
        copy |= !dropped.is_empty();
        if copy {
            match persist::copy_aside_v2(dir, &self.project_root, &text) {
                Ok(aside) => println!(
                    "[layout] the file, part of which this build cannot carry, is kept as {}",
                    aside.display()
                ),
                Err(err) => {
                    eprintln!(
                        "[layout] could not copy aside the file this build can only partly use ({err}); not saved"
                    );
                    return false;
                }
            }
        }
        for note in dropped {
            println!("[layout] {note}");
        }
        match self.workspaces.try_borrow_mut() {
            Ok(mut workspaces) => {
                workspaces.retake_kept(kept);
                true
            }
            Err(_) => {
                eprintln!("[layout] the workspaces were busy; not saved this time");
                false
            }
        }
    }

    /// Writes this window's workspaces now if their arrangement changed since `base`, dropping a
    /// pending timer: the close handler's call, and the timer's. The file is read again first, to
    /// know what the write replaces (this module's doc); nothing is written if what must be kept of
    /// it cannot be.
    pub(crate) fn save_now(&self) {
        if let Some(source) = self.pending.borrow_mut().take() {
            source.remove();
        }
        let Some(dir) = &self.dir else { return };
        let (now, current) = match (self.layout.try_borrow(), self.workspaces.try_borrow()) {
            (Ok(layout), Ok(workspaces)) => (Arrangement::of(&workspaces, &layout), layout.clone()),
            _ => {
                eprintln!("[layout] the layout was busy; not saved this time");
                return;
            }
        };
        if now == *self.base.borrow() {
            return;
        }
        // Not flags set at startup: the file may have changed since -- a hand-edit, another window,
        // another build -- and what was true of it when this window opened says nothing of it now.
        if !self.copy_what_was_opened(dir) || !self.make_way(dir) {
            return;
        }
        let Ok(workspaces) = self.workspaces.try_borrow() else {
            eprintln!("[layout] the workspaces were busy; not saved this time");
            return;
        };
        match persist::save(dir, &self.project_root, &workspaces, &current) {
            Ok(_) => *self.base.borrow_mut() = now,
            Err(err) => eprintln!("[layout] could not save {}: {err}", dir.display()),
        }
    }
}

/// Taken by every test in this crate that runs glib's default main context, so they take turns:
/// a source added there panics while another thread owns it.
#[cfg(test)]
pub(crate) static DEFAULT_CONTEXT_TURN: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use eitri_core::layout::{Float, FloatRect};

    fn template(first: ModuleId, second: ModuleId) -> LuaTree {
        LuaTree {
            tiled: Node::split(Axis::Row, 0.5, Node::Leaf(first), Node::Leaf(second)),
            floats: Vec::new(),
        }
    }

    #[test]
    fn defined_workspaces_open_after_main_on_a_first_launch() {
        let (mut workspaces, mut layout, _, _) =
            choose_startup_layout(None, Some(Loaded::Missing), &[], &OpenRules::default(), None).unwrap();
        let notes = add_defined_workspaces(
            &mut workspaces,
            &mut layout,
            &[("review".to_string(), template(ModuleId::agent(), ModuleId::editor()))],
            &[],
        )
        .unwrap();
        assert_eq!(workspaces.order(), ["main", "review"]);
        assert_eq!(workspaces.current(), "main");
        let review = workspaces.stored("review").unwrap();
        assert_eq!(review.visible_leaves(), [ModuleId::agent(), ModuleId::editor()]);
        assert!(review.contains(&ModuleId::terminal()) && !review.is_shown(&ModuleId::terminal()));
        assert!(notes.iter().all(|n| n.starts_with("workspace 'review': ")), "{notes:?}");
    }

    #[test]
    fn a_saved_workspace_wins_over_its_template_and_a_new_one_joins_at_the_end() {
        let main = crate::terminal::initial_layout(&[]).unwrap();
        let mut study = crate::terminal::initial_layout(&[]).unwrap();
        study.set_focus(&ModuleId::agent()).unwrap();
        let saved = Workspaces::from_parts(
            vec!["main".into(), "study".into()],
            std::collections::BTreeMap::from([("study".to_string(), study.clone())]),
            "main".into(),
            Default::default(),
        )
        .unwrap();
        let loaded = Loaded::Restored(persist::Opened {
            workspaces: saved,
            current: main,
            notes: vec![],
            keep_copy: None,
        });
        let (mut workspaces, mut layout, _, _) =
            choose_startup_layout(None, Some(loaded), &[], &OpenRules::default(), None).unwrap();
        let notes = add_defined_workspaces(
            &mut workspaces,
            &mut layout,
            &[
                ("study".to_string(), template(ModuleId::agent(), ModuleId::editor())),
                ("review".to_string(), template(ModuleId::editor(), ModuleId::agent())),
            ],
            &[],
        )
        .unwrap();
        assert_eq!(workspaces.order(), ["main", "study", "review"]);
        assert_eq!(
            workspaces.stored("study").unwrap(),
            &study,
            "the saved study, not its template"
        );
        assert!(
            notes.contains(&"workspace 'study': the saved layout wins over eitri.layout.workspace".to_string()),
            "{notes:?}"
        );
    }

    /// A saved file whose `main` could not be read opens with `main` back, first and as the default
    /// layout, and the workspace that was current stays current; one that had `main` current opens
    /// the first readable workspace, as the file's reader chose.
    #[test]
    fn a_saved_file_without_main_gets_main_back_first() {
        let mut study = crate::terminal::initial_layout(&[]).unwrap();
        study.set_focus(&ModuleId::agent()).unwrap();
        let loaded = || {
            Loaded::Restored(persist::Opened {
                workspaces: Workspaces::from_parts(
                    vec!["study".into()],
                    Default::default(),
                    "study".into(),
                    Default::default(),
                )
                .unwrap(),
                current: study.clone(),
                notes: vec![],
                keep_copy: Some("the file".to_string()),
            })
        };
        let (workspaces, layout, notes, keep_copy) =
            choose_startup_layout(None, Some(loaded()), &[], &OpenRules::default(), None).unwrap();
        assert_eq!(workspaces.order(), ["main", "study"]);
        assert_eq!(workspaces.current(), "study");
        assert_eq!(layout, study);
        let (_, first_launch, _, _) =
            choose_startup_layout(None, Some(Loaded::Missing), &[], &OpenRules::default(), None).unwrap();
        assert_eq!(workspaces.stored("main"), Some(&first_launch));
        assert!(
            notes.contains(&"workspace 'main' could not be read; it is the default layout again".to_string()),
            "{notes:?}"
        );
        assert_eq!(keep_copy.as_deref(), Some("the file"));
        assert!(eitri_core::layout::export_lua(&layout, Some(&workspaces)).contains("eitri.layout.default("));

        // A file whose current workspace was the unreadable `main`: decode made the first readable
        // workspace current, and that holds.
        let text = persist::encode_v2(
            &Workspaces::from_parts(
                vec!["main".into(), "study".into()],
                std::collections::BTreeMap::from([("study".to_string(), study.clone())]),
                "main".into(),
                Default::default(),
            )
            .unwrap(),
            &first_launch,
            root(),
        );
        let mut file: serde_json::Value = serde_json::from_str(&text).unwrap();
        file["workspaces"][0]["tiled"]["children"][0]["share"] = serde_json::json!(0.99);
        let opened = persist::decode_v2(&file.to_string(), root(), &[]).unwrap();
        assert_eq!(opened.workspaces.order(), ["study"]);
        let (workspaces, layout, notes, keep_copy) =
            choose_startup_layout(None, Some(Loaded::Restored(opened)), &[], &OpenRules::default(), None).unwrap();
        assert_eq!(workspaces.order(), ["main", "study"]);
        assert_eq!(workspaces.current(), "study");
        assert_eq!(layout, study);
        assert_eq!(workspaces.stored("main"), Some(&first_launch));
        assert!(
            notes.contains(&"workspace 'main' was current and cannot be used; 'study' is current".to_string()),
            "{notes:?}"
        );
        assert!(
            notes.contains(&"workspace 'main' could not be read; it is the default layout again".to_string()),
            "{notes:?}"
        );
        assert!(keep_copy.is_some(), "the bytes read are kept");
    }

    /// With `init.lua`'s default, the `main` put back is that default.
    #[test]
    fn main_put_back_is_the_lua_default_when_there_is_one() {
        let tree = LuaTree {
            tiled: Node::split(
                Axis::Column,
                0.5,
                Node::Leaf(ModuleId::agent()),
                Node::Leaf(ModuleId::editor()),
            ),
            floats: Vec::new(),
        };
        let study = crate::terminal::initial_layout(&[]).unwrap();
        let loaded = Loaded::Restored(persist::Opened {
            workspaces: Workspaces::from_parts(
                vec!["study".into()],
                Default::default(),
                "study".into(),
                Default::default(),
            )
            .unwrap(),
            current: study,
            notes: vec![],
            keep_copy: None,
        });
        let (workspaces, _, notes, _) =
            choose_startup_layout(Some(&tree), Some(loaded), &[], &OpenRules::default(), None).unwrap();
        let (_, lua_default, _, _) =
            choose_startup_layout(Some(&tree), Some(Loaded::Missing), &[], &OpenRules::default(), None).unwrap();
        assert_eq!(workspaces.order(), ["main", "study"]);
        assert_eq!(workspaces.stored("main"), Some(&lua_default));
        assert!(
            !notes
                .iter()
                .any(|n| n.starts_with("eitri.layout.default shapes a project with no saved layout")),
            "the default shaped the main put back: {notes:?}"
        );
    }

    #[test]
    fn a_template_naming_an_instance_gives_it_to_every_workspace() {
        let t2 = ModuleId::parse("terminal#2").unwrap();
        let mut review = template(ModuleId::editor(), ModuleId::agent());
        review.floats.push(Float {
            id: t2.clone(),
            rect: FloatRect {
                cx: 0.5,
                cy: 0.5,
                w: 600,
                h: 300,
            },
            scratch: true,
        });
        let (mut workspaces, mut layout, _, _) =
            choose_startup_layout(None, Some(Loaded::Missing), &[], &OpenRules::default(), None).unwrap();
        add_defined_workspaces(&mut workspaces, &mut layout, &[("review".to_string(), review)], &[]).unwrap();
        assert!(workspaces.stored("review").unwrap().is_floating(&t2));
        assert!(layout.contains(&t2) && layout.hidden().contains(&t2) && !layout.is_floating(&t2));
    }

    #[test]
    fn a_template_without_the_chat_is_an_error_naming_the_call() {
        let (mut workspaces, mut layout, _, _) =
            choose_startup_layout(None, Some(Loaded::Missing), &[], &OpenRules::default(), None).unwrap();
        let err = add_defined_workspaces(
            &mut workspaces,
            &mut layout,
            &[("x".to_string(), template(ModuleId::editor(), ModuleId::terminal()))],
            &[],
        )
        .unwrap_err();
        assert!(
            err.starts_with("eitri.layout.workspace('x'): ") && err.contains("'agent'"),
            "{err}"
        );
    }

    use super::*;
    use eitri_core::layout::{Axis, Direction, Frame, ModuleId, Placement, Size};

    fn restored(layout: Layout) -> Loaded {
        Loaded::Restored(persist::Opened {
            workspaces: Workspaces::single(),
            current: layout,
            notes: vec!["a note".to_string()],
            keep_copy: None,
        })
    }

    /// A project reopens as it was left, whatever `init.lua`'s default says -- and the log says the
    /// default was not what shaped it, since that is the one surprise here.
    #[test]
    fn a_usable_file_wins_and_says_the_default_did_not_apply() {
        let mut left = crate::terminal::initial_layout(&[]).unwrap();
        left.show(&ModuleId::terminal()).unwrap();
        let tree = Node::split(
            Axis::Column,
            0.5,
            Node::Leaf(ModuleId::agent()),
            Node::Leaf(ModuleId::editor()),
        );
        let (_, layout, notes, _) = choose_startup_layout(
            Some(&LuaTree {
                tiled: tree.clone(),
                floats: Vec::new(),
            }),
            Some(restored(left.clone())),
            &[],
            &OpenRules::default(),
            None,
        )
        .unwrap();
        assert_eq!(layout, left);
        assert_eq!(
            notes,
            [
                "a note",
                "reopened as it was left",
                "eitri.layout.default shapes a project with no saved layout; this one has one"
            ]
        );
    }

    /// No file, or one that cannot be used: the Lua default, reconciled -- the terminal it did not
    /// name is placed hidden, as a first launch places it.
    #[test]
    fn without_a_usable_file_the_lua_default_shapes_the_window() {
        let tree = Node::split(
            Axis::Column,
            0.5,
            Node::Leaf(ModuleId::agent()),
            Node::Leaf(ModuleId::editor()),
        );
        for loaded in [None, Some(Loaded::Missing), Some(Loaded::Unusable("corrupt".into()))] {
            let unusable = matches!(loaded, Some(Loaded::Unusable(_)));
            let (_, layout, notes, _) = choose_startup_layout(
                Some(&LuaTree {
                    tiled: tree.clone(),
                    floats: Vec::new(),
                }),
                loaded,
                &[],
                &OpenRules::default(),
                None,
            )
            .unwrap();
            assert_eq!(layout.visible_leaves(), [ModuleId::agent(), ModuleId::editor()]);
            assert!(layout.contains(&ModuleId::terminal()) && !layout.is_shown(&ModuleId::terminal()));
            assert_eq!(notes.iter().any(|n| n.contains("corrupt")), unusable, "{notes:?}");
        }
    }

    /// With no saved file or Lua default, use the built-in initial layout.
    #[test]
    fn with_neither_the_window_opens_as_it_always_did() {
        let side = [ModuleDecl {
            id: ModuleId::lua("side"),
            placement: Placement::RightOfRoot,
        }];
        let (_, layout, notes, _) =
            choose_startup_layout(None, Some(Loaded::Missing), &side, &OpenRules::default(), None).unwrap();
        assert_eq!(layout, crate::terminal::initial_layout(&side).unwrap());
        assert!(notes.is_empty());
    }

    /// A layout that reopens with the editor hidden -- `prefix x` in the editor, then a relaunch --
    /// says what waits for it: nvim starts on the editor's first frame,
    /// and the window's colours follow nvim. A layout that shows the editor says nothing of the kind.
    #[test]
    fn a_layout_that_hides_the_editor_says_what_waits_for_it() {
        let mut left = crate::terminal::initial_layout(&[]).unwrap();
        left.set_focus(&ModuleId::agent()).unwrap();
        eitri_core::layout::hide(
            &mut left,
            &ModuleId::editor(),
            &eitri_core::layout::Frame::new(eitri_core::layout::Size { w: 1280, h: 721 }, 1),
        )
        .unwrap();
        let (_, layout, notes, _) = choose_startup_layout(
            None,
            Some(restored(left.clone())),
            &[],
            &OpenRules::default(),
            Some("Ctrl+b e"),
        )
        .unwrap();
        assert!(!layout.is_shown(&ModuleId::editor()));
        assert_eq!(
            notes.last().map(String::as_str),
            Some(
                "the editor is hidden in this layout: nvim, and the window's colours that follow it, wait until \
                 it is shown (Ctrl+b e)"
            )
        );
        // With nothing bound to show the editor, the note names no key rather than a wrong one.
        let (_, _, notes, _) =
            choose_startup_layout(None, Some(restored(left)), &[], &OpenRules::default(), None).unwrap();
        assert_eq!(
            notes.last().map(String::as_str),
            Some(
                "the editor is hidden in this layout: nvim, and the window's colours that follow it, wait until \
                 it is shown"
            )
        );
        let (_, _, notes, _) =
            choose_startup_layout(None, Some(Loaded::Missing), &[], &OpenRules::default(), None).unwrap();
        assert!(notes.iter().all(|n| !n.contains("editor is hidden")), "{notes:?}");
    }

    const ROOT: &str = "/home/user/project";

    fn root() -> &'static Path {
        Path::new(ROOT)
    }

    fn saver_for(dir: &Path, layout: &Rc<RefCell<Layout>>) -> Rc<LayoutSaver> {
        LayoutSaver::new(
            Some(dir.to_path_buf()),
            root(),
            layout.clone(),
            one_workspace(),
            Vec::new(),
            None,
        )
    }

    fn one_workspace() -> Rc<RefCell<Workspaces>> {
        Rc::new(RefCell::new(Workspaces::single()))
    }

    /// A window opened on this project's file, as `main` opens one.
    fn opened(dir: &Path, lua: &[ModuleDecl]) -> Rc<RefCell<Layout>> {
        let Loaded::Restored(r) = persist::load(dir, root(), lua) else {
            panic!("a good file")
        };
        Rc::new(RefCell::new(r.current))
    }

    fn file_text(dir: &Path) -> String {
        std::fs::read_to_string(dir.join(persist::file_name_v2(root()))).unwrap()
    }

    /// What the file holds after a write of `layout`, byte for byte: this window's whole layout.
    fn written_by(layout: &Rc<RefCell<Layout>>) -> String {
        persist::encode_v2(&Workspaces::single(), &layout.borrow(), root())
    }

    fn frame() -> Frame<'static> {
        Frame::new(Size { w: 1280, h: 721 }, 1)
    }

    /// A launch is not a change: the saver writes nothing until the
    /// arrangement changes, so a file `load` could not use survives a window opened and closed
    /// untouched -- a click into the chat included -- and the first write that does replace it sets
    /// it aside whole, as `.v2.json.unusable`.
    #[test]
    fn nothing_is_written_until_the_layout_changes_and_an_unusable_file_is_kept() {
        let dir = agent::state_dirs::test_workspace_dir("layout-saver");
        let path = dir.join(persist::file_name_v2(root()));
        std::fs::write(&path, "{ a typo").unwrap();
        let layout = Rc::new(RefCell::new(crate::terminal::initial_layout(&[]).unwrap()));
        let saver = saver_for(&dir, &layout);
        layout.borrow_mut().set_focus(&ModuleId::agent()).unwrap();
        saver.save_now();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{ a typo",
            "opened, clicked into the chat, closed: untouched"
        );

        layout.borrow_mut().show(&ModuleId::terminal()).unwrap();
        saver.save_now();
        let aside = dir.join(format!("{}.unusable", persist::file_name_v2(root())));
        assert_eq!(std::fs::read_to_string(&aside).unwrap(), "{ a typo");
        assert!(matches!(persist::load(&dir, root(), &[]), Loaded::Restored(_)));

        std::fs::write(&path, "written by someone else").unwrap();
        saver.save_now();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "written by someone else",
            "nothing changed since the last write, so nothing is written"
        );
    }

    /// Moving the keys is not a change: a click, and a zoom
    /// and its end, write nothing in a project with no file, and nothing in one with a file, at
    /// close included. The keys go into the file with the next arrangement change, so a project
    /// reopens with them where they were when that change was written -- here the terminal, not
    /// nvim, where they were at close. (Where they are when the debounce fires, not when the change
    /// was made: `the_debounce_writes_an_arrangement_change_and_a_click_arms_nothing`.)
    #[test]
    fn moving_the_keys_writes_nothing_with_or_without_a_file() {
        let dir = agent::state_dirs::test_workspace_dir("layout-saver-keys");
        let path = dir.join(persist::file_name_v2(root()));
        let layout = Rc::new(RefCell::new(crate::terminal::initial_layout(&[]).unwrap()));
        let saver = saver_for(&dir, &layout);
        layout.borrow_mut().set_focus(&ModuleId::agent()).unwrap();
        saver.save_now();
        layout.borrow_mut().toggle_zoom(&ModuleId::agent());
        saver.save_now();
        layout.borrow_mut().toggle_zoom(&ModuleId::agent());
        saver.save_now();
        assert!(!path.exists(), "a click and a zoom: no file");

        layout.borrow_mut().show(&ModuleId::terminal()).unwrap();
        layout.borrow_mut().set_focus(&ModuleId::terminal()).unwrap();
        saver.save_now();
        let at_the_change = written_by(&layout);
        assert_eq!(
            file_text(&dir),
            at_the_change,
            "`Ctrl+a t`: written, the keys in the terminal"
        );
        layout.borrow_mut().set_focus(&ModuleId::editor()).unwrap();
        saver.save_now();
        saver.save_now();
        assert_eq!(
            file_text(&dir),
            at_the_change,
            "a click into nvim, then close: nothing written"
        );

        let reopened = opened(&dir, &[]);
        assert_eq!(
            reopened.borrow().focus(),
            &ModuleId::terminal(),
            "the keys where they were when the last arrangement change was written, not at close"
        );
        let saver = saver_for(&dir, &reopened);
        reopened.borrow_mut().set_focus(&ModuleId::agent()).unwrap();
        saver.save_now();
        assert_eq!(
            file_text(&dir),
            at_the_change,
            "a restored file does not follow a click either"
        );
    }

    /// An arrangement change writes this window's whole layout, with the keys where they are at
    /// that moment -- a click made before it included.
    #[test]
    fn an_arrangement_change_writes_this_windows_layout_with_the_keys_where_they_are() {
        let dir = agent::state_dirs::test_workspace_dir("layout-saver-arrangement");
        persist::save(
            &dir,
            root(),
            &Workspaces::single(),
            &crate::terminal::initial_layout(&[]).unwrap(),
        )
        .unwrap();
        let layout = opened(&dir, &[]);
        let saver = saver_for(&dir, &layout);
        let before = file_text(&dir);
        layout.borrow_mut().set_focus(&ModuleId::agent()).unwrap();
        saver.save_now();
        assert_eq!(file_text(&dir), before, "the click alone");
        layout.borrow_mut().show(&ModuleId::terminal()).unwrap();
        saver.save_now();
        assert_eq!(file_text(&dir), written_by(&layout));
        let reopened = opened(&dir, &[]);
        assert_eq!(*reopened.borrow().root(), *layout.borrow().root());
        assert!(reopened.borrow().is_shown(&ModuleId::terminal()));
        assert_eq!(reopened.borrow().focus(), &ModuleId::agent(), "the click rode along");
    }

    /// Two windows on one project (`NON_UNIQUE`) each save their whole layout when their
    /// arrangement changes. The last write wins; neither window's wait is extended here.
    /// A focus click writes nothing, so it cannot overwrite another window's arrangement
    /// with the layout this window opened with.
    #[test]
    fn two_windows_the_later_arrangement_change_wins_and_a_click_writes_nothing() {
        let dir = agent::state_dirs::test_workspace_dir("layout-saver-two-arrangements");
        persist::save(
            &dir,
            root(),
            &Workspaces::single(),
            &crate::terminal::initial_layout(&[]).unwrap(),
        )
        .unwrap();
        let (first, second) = (opened(&dir, &[]), opened(&dir, &[]));
        let first_saver = saver_for(&dir, &first);
        let second_saver = saver_for(&dir, &second);

        first.borrow_mut().show(&ModuleId::terminal()).unwrap();
        eitri_core::layout::swap(&mut first.borrow_mut(), &ModuleId::editor(), Direction::Right, &frame()).unwrap();
        first_saver.save_now();
        let firsts = written_by(&first);
        assert_eq!(file_text(&dir), firsts);
        second.borrow_mut().set_focus(&ModuleId::agent()).unwrap();
        second_saver.save_now();
        assert_eq!(file_text(&dir), firsts, "the second window's click: nothing written");

        eitri_core::layout::hide(&mut second.borrow_mut(), &ModuleId::agent(), &frame()).unwrap();
        second_saver.save_now();
        let seconds = written_by(&second);
        assert_ne!(seconds, firsts);
        assert_eq!(file_text(&dir), seconds, "the later write wins, whole");

        first.borrow_mut().set_focus(&ModuleId::terminal()).unwrap();
        first_saver.save_now();
        first_saver.save_now();
        assert_eq!(
            file_text(&dir),
            seconds,
            "the first window's click, then close: nothing written"
        );
    }

    /// A module this window does not have -- here a Lua panel only the other window registered --
    /// stays in the file through a click here, and leaves it with this window's next arrangement
    /// change: what is written is this window's layout (the accepted cost in this module's doc).
    #[test]
    fn a_module_this_window_lacks_leaves_the_file_only_with_its_arrangement_change() {
        let dir = agent::state_dirs::test_workspace_dir("layout-saver-foreign-module");
        persist::save(
            &dir,
            root(),
            &Workspaces::single(),
            &crate::terminal::initial_layout(&[]).unwrap(),
        )
        .unwrap();
        let lua = vec![ModuleDecl {
            id: ModuleId::lua("notes"),
            placement: Placement::RightOfRoot,
        }];
        let (first, second) = (opened(&dir, &lua), opened(&dir, &[]));
        let first_saver = LayoutSaver::new(
            Some(dir.clone()),
            root(),
            first.clone(),
            one_workspace(),
            lua.clone(),
            None,
        );
        let second_saver = saver_for(&dir, &second);
        first.borrow_mut().show(&ModuleId::terminal()).unwrap();
        first_saver.save_now();
        let firsts = file_text(&dir);
        assert!(firsts.contains("lua:notes"), "{firsts}");

        second.borrow_mut().set_focus(&ModuleId::agent()).unwrap();
        second_saver.save_now();
        assert_eq!(file_text(&dir), firsts, "a click: the panel stays in the file");

        second.borrow_mut().show(&ModuleId::terminal()).unwrap();
        second_saver.save_now();
        assert_eq!(file_text(&dir), written_by(&second));
        assert!(
            !file_text(&dir).contains("lua:notes"),
            "the second window's layout has no such panel"
        );
    }

    /// A pinned row getting its length on the first frame it is shown is not a change either
    /// (`ModuleGrid::connect_settled`): a Lua default whose bottom terminal is on screen from the
    /// start, then a zoom and its end, writes nothing. Moving that row afterwards is a change.
    #[test]
    fn a_length_settled_on_screen_is_not_a_change_and_moving_it_is() {
        let dir = agent::state_dirs::test_workspace_dir("layout-saver-settle");
        let path = dir.join(persist::file_name_v2(root()));
        let (_, startup, _, _) = choose_startup_layout(
            Some(&LuaTree {
                tiled: bottom_row_default(),
                floats: Vec::new(),
            }),
            Some(Loaded::Missing),
            &[],
            &OpenRules::default(),
            None,
        )
        .unwrap();
        let layout = Rc::new(RefCell::new(startup));
        let saver = saver_for(&dir, &layout);
        assert!(eitri_core::layout::settle_pins(&mut layout.borrow_mut(), &frame()));
        saver.settled();
        layout.borrow_mut().toggle_zoom(&ModuleId::editor());
        saver.save_now();
        layout.borrow_mut().toggle_zoom(&ModuleId::editor());
        saver.save_now();
        assert!(!path.exists(), "the first frame and a zoom: nothing to keep");

        assert!(eitri_core::layout::resize(
            &mut layout.borrow_mut(),
            &ModuleId::editor(),
            Direction::Down,
            5,
            &frame()
        ));
        saver.save_now();
        assert!(path.exists(), "Ctrl+a j moved the bottom row");
    }

    /// `eitri.layout.default{ 'column', {'row', {'editor'}, {'agent'}, share = 0.7}, {'terminal'} }`:
    /// a bottom row pinned with no length yet.
    fn bottom_row_default() -> Node {
        Node::split(
            Axis::Column,
            0.7,
            Node::split(
                Axis::Row,
                0.5,
                Node::Leaf(ModuleId::editor()),
                Node::Leaf(ModuleId::agent()),
            ),
            Node::Leaf(ModuleId::terminal()),
        )
    }

    /// The same for a file whose bottom row is pinned but has no length yet -- written by a window
    /// where it never settled: the row settling here, then a click, writes nothing, so a project
    /// opened and closed untouched keeps its file byte for byte. The next arrangement change writes
    /// this window's layout, with the row's length.
    #[test]
    fn a_length_settled_on_a_restored_file_is_not_a_change_either() {
        let dir = agent::state_dirs::test_workspace_dir("layout-saver-settled-file");
        let (_, unsettled, _, _) = choose_startup_layout(
            Some(&LuaTree {
                tiled: bottom_row_default(),
                floats: Vec::new(),
            }),
            Some(Loaded::Missing),
            &[],
            &OpenRules::default(),
            None,
        )
        .unwrap();
        persist::save(&dir, root(), &Workspaces::single(), &unsettled).unwrap();
        let before = file_text(&dir);
        assert!(before.contains("\"px\": null"), "{before}");
        let layout = opened(&dir, &[]);
        let saver = saver_for(&dir, &layout);
        assert!(eitri_core::layout::settle_pins(&mut layout.borrow_mut(), &frame()));
        saver.settled();
        layout.borrow_mut().set_focus(&ModuleId::terminal()).unwrap();
        saver.save_now();
        assert_eq!(file_text(&dir), before, "a settled length and a click: untouched");

        assert!(eitri_core::layout::resize(
            &mut layout.borrow_mut(),
            &ModuleId::editor(),
            Direction::Down,
            5,
            &frame()
        ));
        saver.save_now();
        assert_eq!(file_text(&dir), written_by(&layout));
        assert!(!file_text(&dir).contains("\"px\": null"), "the moved length is written");
    }

    /// Two windows open the same unusable layout file. The first save sets it aside and
    /// writes a valid file. The second save must not set that valid file aside over the
    /// unusable bytes already kept by the first window.
    #[test]
    fn a_second_window_does_not_set_aside_the_file_the_first_one_wrote() {
        let dir = agent::state_dirs::test_workspace_dir("layout-saver-two-windows");
        std::fs::write(dir.join(persist::file_name_v2(root())), "{ a typo").unwrap();
        let first = Rc::new(RefCell::new(crate::terminal::initial_layout(&[]).unwrap()));
        let second = Rc::new(RefCell::new(crate::terminal::initial_layout(&[]).unwrap()));
        let first_saver = saver_for(&dir, &first);
        let second_saver = saver_for(&dir, &second);
        first.borrow_mut().show(&ModuleId::terminal()).unwrap();
        first_saver.save_now();
        eitri_core::layout::even(&mut second.borrow_mut(), Axis::Column);
        second_saver.save_now();
        let aside = dir.join(format!("{}.unusable", persist::file_name_v2(root())));
        assert_eq!(std::fs::read_to_string(&aside).unwrap(), "{ a typo");
        let Loaded::Restored(saved) = persist::load(&dir, root(), &[]) else {
            panic!("the second window's layout")
        };
        assert_eq!(saved.current.root(), second.borrow().root(), "the last to save wins");
    }

    /// A file that becomes unusable while the window is open -- here a hand-edit with a typo, made
    /// after the window opened on a good file -- is set aside at the next write, never written over
    ///. A
    /// click is not a write, so it leaves such a file where it is, as it would leave a good one.
    #[test]
    fn a_file_made_unusable_while_the_window_is_open_is_set_aside_not_overwritten() {
        let dir = agent::state_dirs::test_workspace_dir("layout-saver-edited");
        let path = dir.join(persist::file_name_v2(root()));
        let aside = dir.join(format!("{}.unusable", persist::file_name_v2(root())));
        persist::save(
            &dir,
            root(),
            &Workspaces::single(),
            &crate::terminal::initial_layout(&[]).unwrap(),
        )
        .unwrap();
        let layout = opened(&dir, &[]);
        let saver = saver_for(&dir, &layout);
        std::fs::write(&path, "{ \"version\": 1, a typo made in nvim").unwrap();
        layout.borrow_mut().show(&ModuleId::terminal()).unwrap();
        saver.save_now();
        assert_eq!(
            std::fs::read_to_string(&aside).unwrap(),
            "{ \"version\": 1, a typo made in nvim"
        );
        assert_eq!(file_text(&dir), written_by(&layout), "written after the edit was kept");

        // Edited into garbage again: a click leaves it alone, and the next arrangement change sets
        // it aside too.
        std::fs::write(&path, "garbage").unwrap();
        layout.borrow_mut().set_focus(&ModuleId::agent()).unwrap();
        saver.save_now();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "garbage",
            "a click writes nothing"
        );
        assert_eq!(
            std::fs::read_to_string(&aside).unwrap(),
            "{ \"version\": 1, a typo made in nvim"
        );
        eitri_core::layout::hide(&mut layout.borrow_mut(), &ModuleId::terminal(), &frame()).unwrap();
        saver.save_now();
        assert_eq!(std::fs::read_to_string(&aside).unwrap(), "garbage");
        assert_eq!(file_text(&dir), written_by(&layout));
    }

    /// A file deleted while the window is open -- resetting a project's layout -- is not
    /// written again for a click; the next arrangement change writes one.
    #[test]
    fn a_deleted_file_is_not_written_again_for_a_click() {
        let dir = agent::state_dirs::test_workspace_dir("layout-saver-deleted");
        let path = dir.join(persist::file_name_v2(root()));
        persist::save(
            &dir,
            root(),
            &Workspaces::single(),
            &crate::terminal::initial_layout(&[]).unwrap(),
        )
        .unwrap();
        let layout = opened(&dir, &[]);
        let saver = saver_for(&dir, &layout);
        std::fs::remove_file(&path).unwrap();
        layout.borrow_mut().set_focus(&ModuleId::agent()).unwrap();
        saver.save_now();
        assert!(!path.exists());
        layout.borrow_mut().set_focus(&ModuleId::editor()).unwrap();
        saver.save_now();
        assert!(!path.exists(), "and not at the next save either");
        layout.borrow_mut().show(&ModuleId::terminal()).unwrap();
        saver.save_now();
        assert!(path.exists(), "a change of the arrangement is written");
    }

    /// Takes the saver's pending timer off glib's default main context if a test unwinds with one
    /// armed. `timeout_add_local_once` puts it on that process-global context, and a source left
    /// there is dispatched by whichever test thread next iterates it -- which aborts the whole test
    /// binary, since the closure belongs to the thread that armed it (a test on that context with
    /// its own probe did exactly that until it had this guard).
    struct Disarm(Rc<LayoutSaver>);

    impl Drop for Disarm {
        fn drop(&mut self) {
            if let Some(source) = self.0.pending.borrow_mut().take() {
                source.remove();
            }
        }
    }

    /// Iterates glib's default main context -- where the saver's timer runs -- until `done` holds or
    /// `ms` have passed, and says which.
    fn pump_until(ms: u64, done: impl Fn() -> bool) -> bool {
        let context = glib::MainContext::default();
        let deadline = Instant::now() + Duration::from_millis(ms);
        loop {
            while context.iteration(false) {}
            if done() {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// The debounce end to end, with no display: `changed()`, glib's timer, `save_now`. A change that
    /// leaves the arrangement as it was -- a click, a zoom -- arms nothing, and nothing is written
    /// however long the window then waits. An arrangement change arms the timer, and the timer, not
    /// only the close, writes it, with the keys where they are when it fires: a click inside the
    /// debounce rides along. The timer is what keeps a change through a SIGKILL, a crash or a power
    /// loss, which the close cannot (before this test, no test called `changed()`, so
    /// `changed()` never arming the timer, or arming it on every change, left every test green).
    ///
    /// It runs on glib's default main context and owns it for its whole run (`acquire`), taking turns
    /// with the crate's other test there through [`DEFAULT_CONTEXT_TURN`]: `timeout_add_local_once`
    /// panics while another thread owns it. [`Disarm`] takes a pending timer off it if an assertion
    /// fails.
    #[test]
    fn the_debounce_writes_an_arrangement_change_and_a_click_arms_nothing() {
        let _turn = DEFAULT_CONTEXT_TURN.lock().unwrap_or_else(|e| e.into_inner());
        let context = glib::MainContext::default();
        let _owner = context
            .acquire()
            .expect("glib's default main context, which no other test in this crate uses");
        let dir = agent::state_dirs::test_workspace_dir("layout-saver-debounce");
        let path = dir.join(persist::file_name_v2(root()));
        let layout = Rc::new(RefCell::new(crate::terminal::initial_layout(&[]).unwrap()));
        let saver = saver_for(&dir, &layout);
        let _disarm = Disarm(saver.clone());
        let past_the_debounce = persist::SAVE_DEBOUNCE_MS + 300;

        layout.borrow_mut().set_focus(&ModuleId::agent()).unwrap();
        saver.changed();
        layout.borrow_mut().toggle_zoom(&ModuleId::agent());
        saver.changed();
        assert!(saver.pending.borrow().is_none(), "a click and a zoom arm nothing");
        assert!(
            !pump_until(past_the_debounce, || path.exists()),
            "a click and a zoom: no file, however long the window waits"
        );
        layout.borrow_mut().toggle_zoom(&ModuleId::agent());
        saver.changed();

        layout.borrow_mut().show(&ModuleId::terminal()).unwrap();
        saver.changed();
        assert!(saver.pending.borrow().is_some(), "an arrangement change arms the timer");
        assert!(!path.exists(), "not written at once: after the debounce");
        layout.borrow_mut().set_focus(&ModuleId::terminal()).unwrap();
        assert!(
            pump_until(10_000, || path.exists()),
            "the timer writes it, not only the close"
        );
        assert!(saver.pending.borrow().is_none(), "the timer is spent");
        assert_eq!(file_text(&dir), written_by(&layout));
        assert_eq!(
            opened(&dir, &[]).borrow().focus(),
            &ModuleId::terminal(),
            "the keys where they were when the timer fired: the click inside the debounce rode along"
        );

        let written = file_text(&dir);
        layout.borrow_mut().set_focus(&ModuleId::editor()).unwrap();
        saver.changed();
        assert!(
            saver.pending.borrow().is_none(),
            "written, so a click arms nothing again"
        );
        assert!(!pump_until(past_the_debounce, || file_text(&dir) != written));
    }

    /// A set-aside that fails for any reason but `NotFound` writes nothing, so an unusable file is
    /// never written over even when it cannot be moved -- here because a directory sits where
    /// `.v2.json.unusable` goes, which fails the rename for root too. The change is not lost: `base`
    /// has not moved, so the next save, the close's included, tries again, and once the way is clear
    /// the file is set aside and this window's layout written.
    #[test]
    fn a_set_aside_that_fails_writes_nothing_over_the_unusable_file() {
        let dir = agent::state_dirs::test_workspace_dir("layout-saver-set-aside-fails");
        let path = dir.join(persist::file_name_v2(root()));
        let aside = dir.join(format!("{}.unusable", persist::file_name_v2(root())));
        std::fs::write(&path, "{ a typo").unwrap();
        std::fs::create_dir_all(aside.join("in the way")).unwrap();
        let layout = Rc::new(RefCell::new(crate::terminal::initial_layout(&[]).unwrap()));
        let saver = saver_for(&dir, &layout);
        layout.borrow_mut().show(&ModuleId::terminal()).unwrap();
        saver.save_now();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{ a typo",
            "could not be set aside, so not written over"
        );
        assert!(!persist::temporary(&path).exists(), "not even half-written");

        std::fs::remove_dir_all(&aside).unwrap();
        saver.save_now();
        assert_eq!(std::fs::read_to_string(&aside).unwrap(), "{ a typo");
        assert_eq!(file_text(&dir), written_by(&layout), "the close tries again");
    }

    /// A write that fails does not move `base`, so the change is not taken for written: the next
    /// save, the close's included, writes it. The write is failed
    /// by a directory where this process's `.tmp` goes, which fails for root too, where a
    /// read-only directory would not.
    #[test]
    fn a_failed_write_is_tried_again() {
        let dir = agent::state_dirs::test_workspace_dir("layout-saver-failed-write");
        let path = dir.join(persist::file_name_v2(root()));
        std::fs::create_dir_all(persist::temporary(&path).join("in the way")).unwrap();
        let layout = Rc::new(RefCell::new(crate::terminal::initial_layout(&[]).unwrap()));
        let saver = saver_for(&dir, &layout);
        layout.borrow_mut().show(&ModuleId::terminal()).unwrap();
        saver.save_now();
        assert!(!path.exists(), "the write failed");

        std::fs::remove_dir_all(persist::temporary(&path)).unwrap();
        saver.save_now();
        assert_eq!(file_text(&dir), written_by(&layout), "the close writes it");
    }

    /// A settle copies into `base` only a length `base` did not have (`take_settled_lengths`), so a
    /// real change of a pinned length is not taken for a settle. Here `Ctrl+a k` grows the bottom
    /// terminal, and before the debounce fires a pinned Lua bottom panel is shown -- it settles, so
    /// the grid's settle hook runs -- and hidden again. The terminal's new height is still written
    /// because the settle preserves lengths already held in `base`.
    #[test]
    fn a_settle_does_not_swallow_a_length_the_window_changed() {
        let dir = agent::state_dirs::test_workspace_dir("layout-saver-settle-after-resize");
        let path = dir.join(persist::file_name_v2(root()));
        let logs = ModuleId::lua("logs");
        let lua = vec![ModuleDecl {
            id: logs.clone(),
            placement: Placement::BelowRoot,
        }];
        // What a launch would find: the terminal shown and settled, the Lua bottom panel hidden and
        // never on screen, so its pin has no length yet.
        let mut start = crate::terminal::initial_layout(&lua).unwrap();
        start.show(&ModuleId::terminal()).unwrap();
        if start.is_shown(&logs) {
            eitri_core::layout::hide(&mut start, &logs, &frame()).unwrap();
        }
        eitri_core::layout::settle_pins(&mut start, &frame());
        let layout = Rc::new(RefCell::new(start));
        let saver = LayoutSaver::new(Some(dir.clone()), root(), layout.clone(), one_workspace(), lua, None);

        layout.borrow_mut().set_focus(&ModuleId::terminal()).unwrap();
        assert!(eitri_core::layout::resize(
            &mut layout.borrow_mut(),
            &ModuleId::terminal(),
            Direction::Up,
            5,
            &frame()
        ));
        layout.borrow_mut().show(&logs).unwrap();
        assert!(
            eitri_core::layout::settle_pins(&mut layout.borrow_mut(), &frame()),
            "the panel's pin settled: the hook runs"
        );
        saver.settled();
        eitri_core::layout::hide(&mut layout.borrow_mut(), &logs, &frame()).unwrap();
        saver.save_now();
        assert!(path.exists(), "the terminal's new height is a change, and is written");
        assert_eq!(file_text(&dir), written_by(&layout));
    }

    /// A pinned child of a container with three children, one pin settled in `now` and not in `base`.
    fn three_children(settled: Option<i32>, layout: eitri_core::layout::ConLayout) -> Con {
        use eitri_core::layout::{Child, Pin};
        let leaf = |name: &str| Node::Leaf(ModuleId::lua(name));
        let mut bottom = Child::new(leaf("c"), 0.2);
        bottom.pin = Some(Pin { px: settled });
        Con::new(
            layout,
            vec![Child::new(leaf("a"), 0.5), Child::new(leaf("b"), 0.3), bottom],
        )
    }

    /// The settle hook copies a pin's length into `base` where `base` had none, in a container of any
    /// size; a base whose shape differs from the layout's keeps its own pins as they are.
    #[test]
    fn take_settled_lengths_copies_a_settled_pin_into_the_base() {
        use eitri_core::layout::ConLayout::{SplitH, SplitV};
        let pin_of = |con: &Con| con.children[2].pin.unwrap().px;

        let mut base = three_children(None, SplitV);
        take_settled_lengths(&mut base, &three_children(Some(240), SplitV));
        assert_eq!(pin_of(&base), Some(240), "the unsettled pin took the settled length");

        let mut kept = three_children(Some(100), SplitV);
        take_settled_lengths(&mut kept, &three_children(Some(240), SplitV));
        assert_eq!(
            pin_of(&kept),
            Some(100),
            "a length the base already had is not replaced"
        );

        let mut other_axis = three_children(None, SplitV);
        take_settled_lengths(&mut other_axis, &three_children(Some(240), SplitH));
        assert_eq!(
            pin_of(&other_axis),
            None,
            "a container of another orientation is left alone"
        );

        let mut other_count = three_children(None, SplitV);
        let mut shorter = three_children(Some(240), SplitV);
        shorter.children.remove(1);
        take_settled_lengths(&mut other_count, &shorter);
        assert_eq!(
            pin_of(&other_count),
            None,
            "a container with another child count is left alone"
        );

        // The same walk reaches a pin below the first level.
        let nested = |settled| {
            Con::new(
                SplitH,
                vec![
                    eitri_core::layout::Child::new(Node::Con(three_children(settled, SplitV)), 0.5),
                    eitri_core::layout::Child::new(Node::Leaf(ModuleId::lua("d")), 0.5),
                ],
            )
        };
        let mut base = nested(None);
        take_settled_lengths(&mut base, &nested(Some(77)));
        let Node::Con(inner) = &base.children[0].node else {
            unreachable!()
        };
        assert_eq!(pin_of(inner), Some(77));
    }

    /// The debounce's one timer: armed once, and when it fires before 500ms have passed since the
    /// last change, armed again for what is left.
    #[test]
    fn the_debounce_waits_for_what_is_left_of_500ms() {
        assert_eq!(debounce_left(Duration::ZERO), Some(Duration::from_millis(500)));
        assert_eq!(
            debounce_left(Duration::from_millis(120)),
            Some(Duration::from_millis(380))
        );
        assert_eq!(debounce_left(Duration::from_millis(500)), None);
        assert_eq!(debounce_left(Duration::from_secs(3)), None);
        assert_eq!(debounce_left(Duration::MAX), None, "no change was ever reported");
    }

    /// What arms the timer and what writes: the tree, its ratios and what is hidden; never a zoom or
    /// the keys.
    #[test]
    fn only_the_arrangement_is_a_change() {
        let layout = crate::terminal::initial_layout(&[]).unwrap();
        let base = Arrangement::of(&Workspaces::single(), &layout);
        let mut keys = layout.clone();
        keys.set_focus(&ModuleId::agent()).unwrap();
        let mut zoomed = layout.clone();
        zoomed.toggle_zoom(&ModuleId::agent());
        let mut shown = layout.clone();
        shown.show(&ModuleId::terminal()).unwrap();
        let mut even = layout.clone();
        eitri_core::layout::even(&mut even, Axis::Row);
        assert_eq!(Arrangement::of(&Workspaces::single(), &keys), base, "a click");
        assert_eq!(Arrangement::of(&Workspaces::single(), &zoomed), base, "a zoom");
        assert_ne!(Arrangement::of(&Workspaces::single(), &shown), base, "a module shown");
        assert_ne!(Arrangement::of(&Workspaces::single(), &even), base, "a ratio");

        // With two workspaces: a change in the stored one is a change, and so is which one is current.
        let two = |current: &str, study: &Layout| {
            let mut stored = std::collections::BTreeMap::new();
            stored.insert(
                if current == "main" { "study" } else { "main" }.to_string(),
                study.clone(),
            );
            Workspaces::from_parts(
                vec!["main".to_string(), "study".to_string()],
                stored,
                current.to_string(),
                Default::default(),
            )
            .unwrap()
        };
        let base = Arrangement::of(&two("main", &layout), &layout);
        assert_eq!(Arrangement::of(&two("main", &layout), &keys), base, "a click");
        assert_ne!(
            Arrangement::of(&two("main", &shown), &layout),
            base,
            "a module shown in the other workspace"
        );
        assert_ne!(
            Arrangement::of(&two("study", &layout), &layout),
            base,
            "the other workspace current"
        );
    }

    /// A Lua default naming a Lua panel that did not register is not a failure: the panel is left
    /// out and the log says so, as for a state file.
    #[test]
    fn a_lua_default_naming_a_missing_panel_opens_without_it() {
        let tree = Node::split(
            Axis::Row,
            0.5,
            Node::Leaf(ModuleId::editor()),
            Node::split(
                Axis::Column,
                0.5,
                Node::Leaf(ModuleId::agent()),
                Node::Leaf(ModuleId::lua("gone")),
            ),
        );
        let (_, layout, notes, _) = choose_startup_layout(
            Some(&LuaTree {
                tiled: tree.clone(),
                floats: Vec::new(),
            }),
            None,
            &[],
            &OpenRules::default(),
            None,
        )
        .unwrap();
        assert!(!layout.contains(&ModuleId::lua("gone")));
        assert_eq!(
            notes,
            [
                "eitri.layout.default: left out 'lua:gone': this window has no such module",
                "eitri.layout.default: placed 'terminal' below the editor, hidden, as a first launch does"
            ]
        );
    }

    /// A window opened on this project's file as `main` opens one: the current layout, every
    /// workspace, and a saver given what `choose_startup_layout` gives it.
    struct Window {
        layout: Rc<RefCell<Layout>>,
        workspaces: Rc<RefCell<Workspaces>>,
        saver: Rc<LayoutSaver>,
    }

    impl Window {
        fn open(dir: &Path) -> Window {
            let loaded = persist::load(dir, root(), &[]);
            let (workspaces, layout, _, keep_copy) =
                choose_startup_layout(None, Some(loaded), &[], &OpenRules::default(), None).unwrap();
            Window::with(dir, workspaces, layout, keep_copy)
        }

        fn with(dir: &Path, workspaces: Workspaces, layout: Layout, keep_copy: Option<String>) -> Window {
            let layout = Rc::new(RefCell::new(layout));
            let workspaces = Rc::new(RefCell::new(workspaces));
            let saver = LayoutSaver::new(
                Some(dir.to_path_buf()),
                root(),
                layout.clone(),
                workspaces.clone(),
                Vec::new(),
                keep_copy,
            );
            Window {
                layout,
                workspaces,
                saver,
            }
        }

        /// What a write of this window puts in the file, byte for byte.
        fn written(&self) -> String {
            persist::encode_v2(&self.workspaces.borrow(), &self.layout.borrow(), root())
        }

        /// A change of the arrangement: the terminal shown, or hidden if it is shown.
        fn change(&self) {
            let mut layout = self.layout.borrow_mut();
            if layout.is_shown(&ModuleId::terminal()) {
                eitri_core::layout::hide(&mut layout, &ModuleId::terminal(), &frame()).unwrap();
            } else {
                layout.show(&ModuleId::terminal()).unwrap();
            }
        }
    }

    fn partial(dir: &Path) -> PathBuf {
        dir.join(format!("{}.partial", persist::file_name_v2(root())))
    }

    fn two_workspaces(study: &Layout) -> Workspaces {
        let mut stored = std::collections::BTreeMap::new();
        stored.insert("study".to_string(), study.clone());
        Workspaces::from_parts(
            vec!["main".to_string(), "study".to_string()],
            stored,
            "main".to_string(),
            Default::default(),
        )
        .unwrap()
    }

    /// `main` and `study`, the second naming a module no build has: a file this build opens only in
    /// part.
    fn partly_usable_text() -> String {
        let main = crate::terminal::initial_layout(&[]).unwrap();
        let text = persist::encode_v2(&two_workspaces(&main), &main, root());
        let mut value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(
            value["workspaces"][1]["tiled"]["children"][1]["node"]["module"],
            "agent"
        );
        value["workspaces"][1]["tiled"]["children"][1]["node"]["module"] = serde_json::json!("chat");
        serde_json::to_string_pretty(&value).unwrap()
    }

    /// A project last left by a build before workspaces opens from its version 1 file, says so, and
    /// writes only the version 2 file.
    #[test]
    fn a_migrated_file_says_so() {
        let dir = agent::state_dirs::test_workspace_dir("layout-saver-migrated");
        let v1 = dir.join(persist::file_name(root()));
        let text = r#"{"version": 1, "project_root": "/home/user/project",
            "root": {"type": "split", "axis": "row", "ratio": 0.5942142,
                "first": {"type": "split", "axis": "column", "ratio": 0.6666667,
                    "pin": {"side": "second", "px": 240},
                    "first": {"type": "leaf", "module": "editor"},
                    "second": {"type": "leaf", "module": "terminal"}},
                "second": {"type": "leaf", "module": "agent"}},
            "hidden": [], "focus": "editor"}"#;
        std::fs::write(&v1, text).unwrap();
        let (workspaces, layout, notes, keep_copy) = choose_startup_layout(
            None,
            Some(persist::load(&dir, root(), &[])),
            &[],
            &OpenRules::default(),
            None,
        )
        .unwrap();
        assert_eq!(workspaces, Workspaces::single());
        assert!(layout.is_shown(&ModuleId::terminal()));
        assert_eq!(
            notes,
            ["migrated from the version 1 layout file", "reopened as it was left"]
        );
        assert_eq!(keep_copy, None);

        let window = Window::with(&dir, workspaces, layout, keep_copy);
        window.change();
        window.saver.save_now();
        assert_eq!(file_text(&dir), window.written());
        assert_eq!(
            std::fs::read_to_string(&v1).unwrap(),
            text,
            "the version 1 file is never written"
        );
    }

    /// A file this window opened only in part has the bytes it read copied aside before its first
    /// write, and then the window's own workspaces are written.
    #[test]
    fn the_saver_copies_a_partly_unusable_file_before_its_first_write() {
        let dir = agent::state_dirs::test_workspace_dir("layout-saver-partial");
        let text = partly_usable_text();
        std::fs::write(dir.join(persist::file_name_v2(root())), &text).unwrap();
        let window = Window::open(&dir);
        assert_eq!(window.workspaces.borrow().order(), ["main"]);
        window.layout.borrow_mut().set_focus(&ModuleId::agent()).unwrap();
        window.saver.save_now();
        assert!(!partial(&dir).exists(), "a click writes nothing, and copies nothing");
        assert_eq!(file_text(&dir), text);

        window.change();
        window.saver.save_now();
        assert_eq!(std::fs::read_to_string(partial(&dir)).unwrap(), text);
        assert_eq!(file_text(&dir), window.written());
    }

    /// Two windows that opened the same partly usable file: the first to write copies it aside; the
    /// second finds the file is the first one's now, not the text it opened, and copies nothing over
    /// the copy -- here a later copy, of another partly usable text the first window found and kept,
    /// which a second window copying what it opened would replace.
    #[test]
    fn a_second_window_does_not_copy_aside_the_file_the_first_one_wrote() {
        let dir = agent::state_dirs::test_workspace_dir("layout-saver-partial-two");
        let text = partly_usable_text();
        std::fs::write(dir.join(persist::file_name_v2(root())), &text).unwrap();
        let (first, second) = (Window::open(&dir), Window::open(&dir));
        first.change();
        first.saver.save_now();
        assert_eq!(std::fs::read_to_string(partial(&dir)).unwrap(), text);
        assert_eq!(file_text(&dir), first.written());

        // The file made partly unusable again, differently, and the first window writing over it:
        // the copy is of that text now.
        let later = text.replace("\"chat\"", "\"pane\"");
        assert_ne!(later, text);
        std::fs::write(dir.join(persist::file_name_v2(root())), &later).unwrap();
        first.change();
        first.saver.save_now();
        assert_eq!(std::fs::read_to_string(partial(&dir)).unwrap(), later);
        assert_eq!(file_text(&dir), first.written());

        eitri_core::layout::even(&mut second.layout.borrow_mut(), Axis::Column);
        second.saver.save_now();
        assert_eq!(
            std::fs::read_to_string(partial(&dir)).unwrap(),
            later,
            "the file is not the text the second window opened, so that text is not copied over the copy"
        );
        assert_eq!(file_text(&dir), second.written(), "the later writer wins, whole");
        assert_ne!(second.written(), first.written());
    }

    /// A file made partly unusable after this window opened it -- a hand edit, another build -- has its
    /// text copied aside before this window writes over it.
    #[test]
    fn a_file_made_partly_unusable_while_open_is_copied_before_the_write() {
        let dir = agent::state_dirs::test_workspace_dir("layout-saver-partial-later");
        persist::save(
            &dir,
            root(),
            &Workspaces::single(),
            &crate::terminal::initial_layout(&[]).unwrap(),
        )
        .unwrap();
        let window = Window::open(&dir);
        let text = partly_usable_text();
        std::fs::write(dir.join(persist::file_name_v2(root())), &text).unwrap();
        window.change();
        window.saver.save_now();
        assert_eq!(std::fs::read_to_string(partial(&dir)).unwrap(), text);
        assert_eq!(file_text(&dir), window.written());
    }

    /// Keys a later build wrote into the file after this window opened it -- a workspace-level key, a
    /// top-level key -- are taken from the file when this window writes, so its write does not erase
    /// them; nothing is copied aside for them.
    #[test]
    fn unknown_keys_written_after_this_window_loaded_survive_its_save() {
        let dir = agent::state_dirs::test_workspace_dir("layout-saver-kept-later");
        persist::save(
            &dir,
            root(),
            &Workspaces::single(),
            &crate::terminal::initial_layout(&[]).unwrap(),
        )
        .unwrap();
        let window = Window::open(&dir);
        let mut value: serde_json::Value = serde_json::from_str(&file_text(&dir)).unwrap();
        let overlays = serde_json::json!([{"module": "terminal#2", "opacity": 0.8}]);
        value["workspaces"][0]["overlays"] = overlays.clone();
        value["future"] = serde_json::json!({"a": 1});
        std::fs::write(
            dir.join(persist::file_name_v2(root())),
            serde_json::to_string_pretty(&value).unwrap(),
        )
        .unwrap();

        window.change();
        window.saver.save_now();
        let again: serde_json::Value = serde_json::from_str(&file_text(&dir)).unwrap();
        assert_eq!(again["workspaces"][0]["overlays"], overlays);
        assert_eq!(again["future"], serde_json::json!({"a": 1}));
        assert_eq!(file_text(&dir), window.written());
        assert!(!partial(&dir).exists(), "carried, so nothing is copied");
    }

    /// Two builds on one project: this window tiles `terminal#2` after it opened the file, and a later
    /// build then puts a `terminal#2` of its own in a key this build does not know. The file's tree does not hold it, so the file
    /// alone does not show the clash; this window's own layout does. Its write keeps `terminal#2` in
    /// one place, its tree, and the later build's text survives as `.v2.json.partial`.
    #[test]
    fn a_kept_key_naming_a_module_this_window_placed_since_it_opened_is_not_written_back() {
        let dir = agent::state_dirs::test_workspace_dir("layout-saver-kept-placed-since");
        persist::save(
            &dir,
            root(),
            &Workspaces::single(),
            &crate::terminal::initial_layout(&[]).unwrap(),
        )
        .unwrap();
        let window = Window::open(&dir);
        let second = ModuleId::parse("terminal#2").unwrap();
        eitri_core::layout::place(&mut window.layout.borrow_mut(), &second, &ModuleId::editor(), Axis::Row).unwrap();

        let mut value: serde_json::Value = serde_json::from_str(&file_text(&dir)).unwrap();
        value["workspaces"][0]["overlays"] = serde_json::json!([{"module": "terminal#2", "w": 900, "h": 420}]);
        value["workspaces"][0]["overlay_memory"] = serde_json::json!({"x": 1});
        let later = serde_json::to_string_pretty(&value).unwrap();
        std::fs::write(dir.join(persist::file_name_v2(root())), &later).unwrap();

        window.saver.save_now();
        let again: serde_json::Value = serde_json::from_str(&file_text(&dir)).unwrap();
        assert_eq!(again["workspaces"][0].get("overlays"), None, "terminal#2 is tiled here");
        assert_eq!(
            again["workspaces"][0]["overlay_memory"],
            serde_json::json!({"x": 1}),
            "a key naming no module of this window is still carried"
        );
        assert!(file_text(&dir).contains("\"terminal#2\""), "in the tree");
        assert_eq!(file_text(&dir), window.written());
        assert_eq!(std::fs::read_to_string(partial(&dir)).unwrap(), later);
        assert!(window.workspaces.borrow().reserved().is_empty());
        let Loaded::Restored(back) = persist::load(&dir, root(), &[]) else {
            panic!("the file")
        };
        assert!(back.keep_copy.is_none(), "what was written holds terminal#2 once");
    }

    #[test]
    fn the_saver_writes_every_workspace() {
        let dir = agent::state_dirs::test_workspace_dir("layout-saver-every-workspace");
        let mut study = crate::terminal::initial_layout(&[]).unwrap();
        eitri_core::layout::even(&mut study, Axis::Row);
        let window = Window::with(
            &dir,
            two_workspaces(&study),
            crate::terminal::initial_layout(&[]).unwrap(),
            None,
        );
        window.change();
        window.saver.save_now();
        assert_eq!(file_text(&dir), window.written());
        let Loaded::Restored(back) = persist::load(&dir, root(), &[]) else {
            panic!("the file")
        };
        assert_eq!(back.workspaces.order(), ["main", "study"]);
        assert_eq!(back.workspaces.stored("study").unwrap().root(), study.root());
    }

    /// With several workspaces too, a move of the keys writes nothing; a change in any workspace,
    /// the one not on screen included, is written.
    #[test]
    fn a_move_of_the_keys_writes_nothing() {
        let dir = agent::state_dirs::test_workspace_dir("layout-saver-keys-workspaces");
        let path = dir.join(persist::file_name_v2(root()));
        let layout = crate::terminal::initial_layout(&[]).unwrap();
        let window = Window::with(&dir, two_workspaces(&layout), layout, None);
        window.layout.borrow_mut().set_focus(&ModuleId::agent()).unwrap();
        window.saver.save_now();
        assert!(!path.exists(), "a click");

        for stored in window.workspaces.borrow_mut().stored_mut() {
            stored.show(&ModuleId::terminal()).unwrap();
        }
        window.saver.save_now();
        assert_eq!(
            file_text(&dir),
            window.written(),
            "a change in the workspace not on screen"
        );
    }

    #[test]
    fn a_lua_default_with_a_float_opens_with_it() {
        use eitri_core::layout::{Float, FloatRect};
        let tree = LuaTree {
            tiled: Node::split(
                Axis::Row,
                0.5,
                Node::Leaf(ModuleId::agent()),
                Node::Leaf(ModuleId::editor()),
            ),
            floats: vec![Float {
                id: ModuleId::terminal(),
                rect: FloatRect {
                    cx: 0.5,
                    cy: 0.5,
                    w: 600,
                    h: 300,
                },
                scratch: false,
            }],
        };
        let (_, layout, notes, copy) =
            choose_startup_layout(Some(&tree), Some(Loaded::Missing), &[], &OpenRules::default(), None).unwrap();
        assert!(layout.is_floating(&ModuleId::terminal()));
        assert!(!layout.leaves().contains(&ModuleId::terminal()));
        assert!(notes.is_empty(), "{notes:?}");
        assert!(copy.is_none());
    }

    #[test]
    fn a_lua_default_whose_chat_floats_opens_with_it() {
        use eitri_core::layout::{Float, FloatRect};
        let tree = LuaTree {
            tiled: Node::split(
                Axis::Row,
                0.5,
                Node::Leaf(ModuleId::editor()),
                Node::Leaf(ModuleId::terminal()),
            ),
            floats: vec![Float {
                id: ModuleId::agent(),
                rect: FloatRect {
                    cx: 0.5,
                    cy: 0.5,
                    w: 600,
                    h: 300,
                },
                scratch: false,
            }],
        };
        let (_, layout, notes, copy) =
            choose_startup_layout(Some(&tree), Some(Loaded::Missing), &[], &OpenRules::default(), None).unwrap();
        assert!(layout.is_floating(&ModuleId::agent()));
        assert!(!layout.leaves().contains(&ModuleId::agent()));
        assert!(notes.is_empty(), "{notes:?}");
        assert!(copy.is_none());
    }

    /// `editor | agent` tiled, with the terminal floating under `terminal#2` and the keys in nvim.
    fn two_floats() -> Layout {
        use eitri_core::layout::Float;
        let float = |id: ModuleId, cx: f32| Float {
            id,
            rect: FloatRect {
                cx,
                cy: 0.5,
                w: 600,
                h: 300,
            },
            scratch: false,
        };
        Layout::from_parts_floating(
            Node::split(
                Axis::Row,
                0.5,
                Node::Leaf(ModuleId::editor()),
                Node::Leaf(ModuleId::agent()),
            ),
            vec![
                float(ModuleId::terminal(), 0.4),
                float(ModuleId::parse("terminal#2").unwrap(), 0.6),
            ],
            BTreeSet::new(),
            ModuleId::editor(),
            BTreeMap::new(),
        )
        .unwrap()
    }

    fn reopened_floats(dir: &Path) -> Layout {
        let Loaded::Restored(back) = persist::load(dir, root(), &[]) else {
            panic!("the file")
        };
        back.current
    }

    /// Giving the lower float the keys raises it, and that is a move of the keys: nothing is written.
    /// The order goes into the file with the next change, as it is then.
    #[test]
    fn moving_the_keys_between_floats_writes_nothing() {
        let dir = agent::state_dirs::test_workspace_dir("layout-saver-float-keys");
        let path = dir.join(persist::file_name_v2(root()));
        let window = Window::with(&dir, Workspaces::single(), two_floats(), None);
        window.layout.borrow_mut().set_focus(&ModuleId::terminal()).unwrap();
        assert_eq!(
            window.layout.borrow().floats().last().map(|f| f.id.clone()),
            Some(ModuleId::terminal()),
            "raised"
        );
        window.saver.save_now();
        assert!(!path.exists(), "a raise is not a change");

        let second = ModuleId::parse("terminal#2").unwrap();
        assert!(eitri_core::layout::move_float(
            &mut window.layout.borrow_mut(),
            &second,
            Direction::Down,
            40,
            &frame()
        ));
        window.saver.save_now();
        assert_eq!(file_text(&dir), window.written());
        let ids: Vec<ModuleId> = reopened_floats(&dir).floats().iter().map(|f| f.id.clone()).collect();
        assert_eq!(
            ids,
            [second, ModuleId::terminal()],
            "bottom first, as it was at the write"
        );
    }

    #[test]
    fn a_moved_float_is_written() {
        let dir = agent::state_dirs::test_workspace_dir("layout-saver-float-moved");
        let window = Window::with(&dir, Workspaces::single(), two_floats(), None);
        let before = window.layout.borrow().float_of(&ModuleId::terminal()).unwrap().rect;
        assert!(eitri_core::layout::move_float(
            &mut window.layout.borrow_mut(),
            &ModuleId::terminal(),
            Direction::Right,
            40,
            &frame()
        ));
        window.saver.save_now();
        let moved = window.layout.borrow().float_of(&ModuleId::terminal()).unwrap().rect;
        assert_ne!(moved, before);
        assert_eq!(
            reopened_floats(&dir).float_of(&ModuleId::terminal()).map(|f| f.rect),
            Some(moved)
        );
    }

    #[test]
    fn a_stowed_float_is_written_hidden() {
        let dir = agent::state_dirs::test_workspace_dir("layout-saver-float-stowed");
        let window = Window::with(&dir, Workspaces::single(), two_floats(), None);
        eitri_core::layout::scratchpad_move(&mut window.layout.borrow_mut(), &ModuleId::terminal(), &frame()).unwrap();
        window.saver.save_now();
        let back = reopened_floats(&dir);
        assert!(back.is_stowed(&ModuleId::terminal()));
        assert!(back.hidden().contains(&ModuleId::terminal()));
        assert!(back.float_of(&ModuleId::terminal()).unwrap().scratch);
    }

    fn rules_of(entries: &[(&str, eitri_core::layout::OpenRule)]) -> OpenRules {
        let mut rules = OpenRules::default();
        for (target, rule) in entries {
            rules
                .add(
                    eitri_core::layout::rules::RuleTarget::parse(target).unwrap(),
                    rule.clone(),
                )
                .unwrap();
        }
        rules
    }

    fn at(name: &str) -> Option<eitri_core::layout::rules::At> {
        Some(eitri_core::layout::rules::parse_at(name).unwrap())
    }

    #[test]
    fn a_first_launch_stows_the_terminal_a_scratchpad_rule_names() {
        use eitri_core::layout::OpenRule;
        let rules = rules_of(&[(
            "terminal",
            OpenRule {
                scratchpad: true,
                ..OpenRule::default()
            },
        )]);
        let (decls, _) = rules.launch_decls(&[]);
        let (_, layout, notes, _) = choose_startup_layout(None, Some(Loaded::Missing), &decls, &rules, None).unwrap();
        let terminal = ModuleId::terminal();
        assert!(layout.is_floating(&terminal), "{layout:?}");
        assert!(layout.float_of(&terminal).unwrap().scratch);
        assert!(!layout.leaves().contains(&terminal));
        assert!(!layout.is_shown(&terminal));
        assert_eq!(layout.focus(), &ModuleId::editor());
        assert_eq!(layout.visible_leaves(), [ModuleId::editor(), ModuleId::agent()]);
        assert!(
            notes.contains(&"open rule for 'terminal': a first launch floats it, stowed in the scratchpad".to_string()),
            "{notes:?}"
        );
    }

    /// A chat view's or a numbered instance's declaration only says where that module goes when
    /// something else places it: a first launch places neither, and its tree is today's.
    #[test]
    fn a_first_launch_with_rules_for_chat_views_and_instances_places_only_its_own() {
        use eitri_core::layout::OpenRule;
        let rules = rules_of(&[
            (
                "agent",
                OpenRule {
                    at: at("below"),
                    ..OpenRule::default()
                },
            ),
            (
                "terminal#3",
                OpenRule {
                    at: at("side"),
                    ..OpenRule::default()
                },
            ),
        ]);
        let (decls, _) = rules.launch_decls(&[]);
        assert!(decls.iter().any(|d| d.id == ModuleId::agent()));
        assert!(decls.iter().any(|d| d.id == ModuleId::parse("terminal#3").unwrap()));
        let (_, layout, _, _) = choose_startup_layout(None, Some(Loaded::Missing), &decls, &rules, None).unwrap();
        let today = crate::terminal::initial_layout(&[]).unwrap();
        assert_eq!(layout.leaves(), today.leaves());
        assert_eq!(layout, today);
    }

    #[test]
    fn a_first_launch_puts_the_terminal_where_its_rule_says() {
        use eitri_core::layout::OpenRule;
        let rules = rules_of(&[(
            "terminal",
            OpenRule {
                at: at("bottom"),
                ..OpenRule::default()
            },
        )]);
        let (decls, _) = rules.launch_decls(&[]);
        let (_, layout, _, _) = choose_startup_layout(None, Some(Loaded::Missing), &decls, &rules, None).unwrap();
        assert_eq!(
            layout.leaves(),
            [ModuleId::editor(), ModuleId::agent(), ModuleId::terminal()],
            "below everything, not below the editor"
        );
        assert!(!layout.is_shown(&ModuleId::terminal()));
    }

    #[test]
    fn a_saved_layout_is_not_moved_by_a_rule() {
        use eitri_core::layout::OpenRule;
        let mut left = crate::terminal::initial_layout(&[]).unwrap();
        left.show(&ModuleId::terminal()).unwrap();
        let rules = rules_of(&[(
            "terminal",
            OpenRule {
                at: at("bottom"),
                float: true,
                ..OpenRule::default()
            },
        )]);
        let (decls, _) = rules.launch_decls(&[]);
        let (_, layout, notes, _) =
            choose_startup_layout(None, Some(restored(left.clone())), &decls, &rules, None).unwrap();
        assert_eq!(layout, left);
        assert!(notes.iter().all(|n| !n.starts_with("open rule")), "{notes:?}");
    }

    /// A Lua default's tree is the user's own arrangement: what it names stays as it says, and only what
    /// the first launch adds to it -- here a Lua panel and the terminal it leaves out -- follows a rule.
    #[test]
    fn a_lua_default_places_what_it_names_and_the_rules_place_the_rest() {
        use eitri_core::layout::OpenRule;
        let named = ModuleId::lua("named");
        let other = ModuleId::lua("other");
        let floats = OpenRule {
            float: true,
            ..OpenRule::default()
        };
        let rules = rules_of(&[
            ("lua:named", floats.clone()),
            ("lua:other", floats.clone()),
            ("terminal", floats),
        ]);
        let panels = [
            ModuleDecl {
                id: named.clone(),
                placement: Placement::RightOfRoot,
            },
            ModuleDecl {
                id: other.clone(),
                placement: Placement::RightOfRoot,
            },
        ];
        let (decls, _) = rules.launch_decls(&panels);
        let tree = LuaTree {
            tiled: Node::split(
                Axis::Row,
                0.5,
                Node::Leaf(ModuleId::editor()),
                Node::split(
                    Axis::Column,
                    0.5,
                    Node::Leaf(ModuleId::agent()),
                    Node::Leaf(named.clone()),
                ),
            ),
            floats: Vec::new(),
        };
        let (_, layout, notes, _) =
            choose_startup_layout(Some(&tree), Some(Loaded::Missing), &decls, &rules, None).unwrap();
        assert!(layout.leaves().contains(&named) && !layout.is_floating(&named));
        assert!(layout.is_floating(&other) && !layout.leaves().contains(&other));
        assert!(layout.is_floating(&ModuleId::terminal()));
        assert!(
            !layout.is_shown(&ModuleId::terminal()),
            "a first launch's terminal stays hidden"
        );
        for floated in [&other, &ModuleId::terminal()] {
            let note = format!("open rule for '{floated}': a first launch floats it");
            assert!(notes.contains(&note), "{note} in {notes:?}");
        }
        assert!(
            notes
                .iter()
                .all(|n| !n.starts_with(&format!("open rule for '{named}'"))),
            "{notes:?}"
        );
    }
}
