//! Which layout a window opens with, and keeping the per-project state file current (modules spec
//! §4.4, §4.6). The file's format, its reconciling and its failure modes are
//! `neovibe_core::layout::persist`'s; this is the window's half: choosing between the file,
//! `init.lua`'s `neovibe.layout.default` and the built-in first launch, and writing the file 500ms
//! after the last change and once more, synchronously, when the window closes.
//!
//! **What is written, and when.** The file is written only when this window's *arrangement* has
//! changed since the window opened or last wrote it -- the tree with its ratios and pinned lengths,
//! and what is hidden ([`Arrangement`]): 500ms after the grid reports the last such change, and at
//! close if one is still unwritten. What is written is this window's whole layout, with the keys
//! wherever they are at that moment:
//!
//! - **Moving the keys is not a change.** A click, `Ctrl+h/j/k/l`, a HINT label -- anything that only
//!   moves the keys -- writes nothing, at close included; the keys go into the file with the next
//!   arrangement change, wherever they are when that change is written. So a project reopens with the
//!   keys where they were when its last arrangement change was written -- a click inside the debounce
//!   rides along, and so does one made before a close that writes a change still unwritten -- not
//!   where they were when it closed. That is the cost of this rule, and it is accepted.
//! - **A launch is not a change** (the plan review's finding 1), nor is a pinned row getting its
//!   length on the first frame it is shown (`ModuleGrid::connect_settled`), nor a zoom (zoom is not
//!   stored): a project opened, zoomed, clicked around in and closed keeps following
//!   `neovibe.layout.default` -- including one added to `init.lua` later -- and a file this build
//!   could not use is left alone.
//! - **Two windows on one project** (`NON_UNIQUE`) each write their own whole layout at their own
//!   arrangement changes, and the last write wins (`persist::save`) -- usually the later change, but a
//!   window whose wait a zoom or a drag extended can write an earlier change after another window's
//!   later one. Nothing of the file is merged into what is written. A window that only clicks writes
//!   nothing, so it can never put the arrangement it opened with back over the other's.
//! - **A module this window does not have** -- a Lua panel this launch's `init.lua` no longer
//!   registers, or a later build's `canvas` -- is not in this window's layout, so it leaves the file
//!   the next time this window changes the arrangement (a click leaves it where it is). Accepted too:
//!   what is written is this window's layout.
//!
//! **The file is read again before every write, and only to know whether it can be used.** One this
//! build cannot use -- found so when the window opened, or made so since by a hand-edit from the nvim
//! inside this very window or by another build (the whole-branch review's finding 7) -- is set aside
//! whole as `.json.unusable` (`persist::set_aside`) and never written over. A usable one is replaced,
//! whatever it holds.
//!
//! **This rule replaced a merge** (the controller's ruling of 2026-09-24). Three rounds -- the
//! whole-branch review's window finding 1, then its re-reviews' N2 and N6 -- tried to write only the
//! keys when only the keys had moved, into whatever arrangement the file then held; each round's fix
//! opened the next hole, and the last one dropped a module another window had placed. The dated record
//! (`docs/canonical/dated_record.md`, 2026-09-24) keeps that history.

use std::cell::{Cell, RefCell};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use gtk4::glib;
use neovibe_core::layout::persist::{self, Loaded};
use neovibe_core::layout::{reconcile_default, Layout, ModuleDecl, ModuleId, Node};

/// The layout a window opens with, and the lines to log about how it was chosen.
///
/// - `default`: `neovibe.layout.default`'s tree, if `init.lua` gave one; an error there is the
///   caller's startup failure, not handled here.
/// - `loaded`: this project's state file, if the state directory is known.
///
/// The file wins whenever it can be used: a project reopens as it was left. Otherwise the Lua
/// default, reconciled like a file (a module this window lacks is left out, the terminal placed if
/// missing); otherwise the built-in first launch (`terminal::initial_layout`).
///
/// Whichever it is, a layout that hides the editor says so: nvim is started by the editor's first
/// frame (`neovide-editor`'s render callback builds it), so neither nvim nor the window's colours,
/// which follow nvim's colorscheme, come until the editor is shown. `prefix x` in the editor makes
/// that one keypress and one relaunch away (the plan review's finding 5). `show_editor` is the key
/// that shows it, as the effective keymap binds it (`Ctrl+b e` by default), so the note names it;
/// `None` when nothing binds one, and the note then names no key rather than a wrong one.
pub(crate) fn choose_startup_layout(
    default: Option<&Node>,
    loaded: Option<Loaded>,
    lua: &[ModuleDecl],
    show_editor: Option<&str>,
) -> Result<(Layout, Vec<String>), String> {
    let mut notes = Vec::new();
    let first_launch = |notes: &mut Vec<String>| -> Result<Layout, String> {
        match default {
            Some(tree) => {
                let r = reconcile_default(tree.clone(), lua).map_err(|e| format!("neovibe.layout.default: {e}"))?;
                notes.extend(r.notes.into_iter().map(|n| format!("neovibe.layout.default: {n}")));
                Ok(r.layout)
            }
            None => crate::terminal::initial_layout(lua).map_err(|e| e.to_string()),
        }
    };
    let layout = match loaded {
        Some(Loaded::Restored(r)) => {
            notes.extend(r.notes);
            notes.push("reopened as it was left".to_string());
            if default.is_some() {
                notes
                    .push("neovibe.layout.default shapes a project with no saved layout; this one has one".to_string());
            }
            r.layout
        }
        Some(Loaded::Unusable(why)) => {
            notes.push(format!(
                "{why}; opening the default layout (the file is kept: it is set aside as .json.unusable \
                 the first time this window saves)"
            ));
            first_launch(&mut notes)?
        }
        Some(Loaded::Missing) | None => first_launch(&mut notes)?,
    };
    if !layout.is_shown(&ModuleId::editor()) {
        let way_back = show_editor.map(|key| format!(" ({key})")).unwrap_or_default();
        notes.push(format!(
            "the editor is hidden in this layout: nvim, and the window's colours that follow it, wait until it \
             is shown{way_back}"
        ));
    }
    Ok((layout, notes))
}

/// What the state file keeps of a layout besides the keys (`persist::encode`): the tree with its
/// ratios and pinned lengths, and what is hidden. A change of this, and only of this, writes the file
/// (this module's doc); zoom and the MRU order are not stored at all, and the keys ride along.
#[derive(Debug, Clone, PartialEq)]
struct Arrangement {
    root: Node,
    hidden: BTreeSet<ModuleId>,
}

impl Arrangement {
    fn of(layout: &Layout) -> Arrangement {
        Arrangement {
            root: layout.root().clone(),
            hidden: layout.saved_hidden(),
        }
    }
}

/// How much longer the debounce waits, given how long ago the last change was: `None` once
/// [`persist::SAVE_DEBOUNCE_MS`] has passed. The timer is armed once for a burst of changes -- a
/// divider drag reports one per motion event -- and, when it fires early, armed again for what is
/// left, rather than removed and re-added for every change (the whole-branch review's window
/// finding 5).
fn debounce_left(since_last_change: Duration) -> Option<Duration> {
    Duration::from_millis(persist::SAVE_DEBOUNCE_MS)
        .checked_sub(since_last_change)
        .filter(|left| !left.is_zero())
}

/// `base` with each pinned length it did not have yet taken from `now`'s split at the same place:
/// what `settle_pins` just gave the layout. Where the two trees differ, `base` is left as it is.
fn take_settled_lengths(base: &mut Node, now: &Node) {
    if let (
        Node::Split {
            pin: base_pin,
            first: base_first,
            second: base_second,
            ..
        },
        Node::Split {
            pin: now_pin,
            first: now_first,
            second: now_second,
            ..
        },
    ) = (base, now)
    {
        if let (Some(base_pin), Some(now_pin)) = (base_pin.as_mut(), now_pin) {
            if base_pin.px.is_none() && base_pin.side == now_pin.side {
                base_pin.px = now_pin.px;
            }
        }
        take_settled_lengths(base_first, now_first);
        take_settled_lengths(base_second, now_second);
    }
}

/// Writes the state file [`persist::SAVE_DEBOUNCE_MS`] after the last change of the arrangement, and
/// at close -- what, and when, is this module's doc.
pub(crate) struct LayoutSaver {
    dir: Option<PathBuf>,
    project_root: PathBuf,
    layout: Rc<RefCell<Layout>>,
    /// The registered Lua panels: the file read again before a write is decoded as `load` decodes
    /// it, so that "unusable" means what it meant when the window opened.
    lua: Vec<ModuleDecl>,
    pending: RefCell<Option<glib::SourceId>>,
    /// When the grid last reported a change: what a debounce timer that fires checks.
    last_change: Cell<Option<Instant>>,
    /// What a change is measured from: the arrangement the window opened with (its pinned lengths as
    /// they settled), or the one it last wrote.
    base: RefCell<Arrangement>,
}

impl LayoutSaver {
    /// `dir` is `persist::state_dir`'s answer; `None` (no usable `XDG_STATE_HOME` or `HOME`) saves
    /// nothing, said once here. `layout` is the startup layout, not yet changed: a launch is not a
    /// change, whether it opened a file, the Lua default or the first launch.
    pub(crate) fn new(
        dir: Option<PathBuf>,
        project_root: &Path,
        layout: Rc<RefCell<Layout>>,
        lua: Vec<ModuleDecl>,
    ) -> Rc<LayoutSaver> {
        if dir.is_none() {
            eprintln!(
                "[layout] no usable state directory (XDG_STATE_HOME absolute, or HOME absolute); this window's \
                 layout will not be kept"
            );
        }
        let base = Arrangement::of(&layout.borrow());
        Rc::new(LayoutSaver {
            dir,
            project_root: project_root.to_path_buf(),
            layout,
            lua,
            pending: RefCell::new(None),
            last_change: Cell::new(None),
            base: RefCell::new(base),
        })
    }

    /// The grid's change hook: the layout changed, so it is compared and written 500ms after the
    /// last change, or at close. A change that leaves the arrangement as it was -- a zoom, or its end,
    /// which a move of the keys out of a zoomed module also reports -- arms nothing, and a change
    /// while the timer already waits only
    /// moves the time it waits for ([`debounce_left`]). The grid's own startup `apply` never reaches
    /// this hook: `main` connects it after that `apply`.
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

    /// Whether the arrangement differs from `base`. `false` while the layout is borrowed: the timer
    /// is armed by the next change, and the close handler saves whatever it is.
    fn arrangement_changed(&self) -> bool {
        match self.layout.try_borrow() {
            Ok(layout) => Arrangement::of(&layout) != *self.base.borrow(),
            Err(_) => false,
        }
    }

    /// The grid's settle hook: a pinned split got its length on screen. That is not a change, so the
    /// length joins `base`.
    pub(crate) fn settled(&self) {
        if let Ok(layout) = self.layout.try_borrow() {
            take_settled_lengths(&mut self.base.borrow_mut().root, layout.root());
        }
    }

    /// Writes this window's layout now if its arrangement changed since `base`, dropping a pending
    /// timer: the close handler's call, and the timer's. The file is read again first, only to know
    /// whether it can be used: one this build cannot use is set aside, and nothing is written if it
    /// cannot be (this module's doc).
    pub(crate) fn save_now(&self) {
        if let Some(source) = self.pending.borrow_mut().take() {
            source.remove();
        }
        let Some(dir) = &self.dir else { return };
        let Ok(layout) = self.layout.try_borrow() else {
            eprintln!("[layout] the layout was busy; not saved this time");
            return;
        };
        let now = Arrangement::of(&layout);
        if now == *self.base.borrow() {
            return;
        }
        let to_write = layout.clone();
        drop(layout);
        // Not a flag set at startup: the file may have become unusable since (a hand-edit), and
        // another window on this project that found the same unusable file may have set it aside
        // and written a good one since, which is not this window's to set aside over the bytes it
        // kept (the second round's finding 8).
        if let Loaded::Unusable(_) = persist::load(dir, &self.project_root, &self.lua) {
            match persist::set_aside(dir, &self.project_root) {
                Ok(aside) => println!(
                    "[layout] the file this build could not use is kept as {}",
                    aside.display()
                ),
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => {
                    eprintln!("[layout] could not set aside the file this build could not use ({err}); not saved");
                    return;
                }
            }
        }
        match persist::save(dir, &self.project_root, &to_write) {
            Ok(_) => *self.base.borrow_mut() = now,
            Err(err) => eprintln!("[layout] could not save {}: {err}", dir.display()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use neovibe_core::layout::{Axis, Direction, Frame, ModuleId, Placement, Size};

    fn restored(layout: Layout) -> Loaded {
        Loaded::Restored(neovibe_core::layout::Reconciled {
            layout,
            notes: vec!["a note".to_string()],
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
        let (layout, notes) = choose_startup_layout(Some(&tree), Some(restored(left.clone())), &[], None).unwrap();
        assert_eq!(layout, left);
        assert_eq!(
            notes,
            [
                "a note",
                "reopened as it was left",
                "neovibe.layout.default shapes a project with no saved layout; this one has one"
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
            let (layout, notes) = choose_startup_layout(Some(&tree), loaded, &[], None).unwrap();
            assert_eq!(layout.visible_leaves(), [ModuleId::agent(), ModuleId::editor()]);
            assert!(layout.contains(&ModuleId::terminal()) && !layout.is_shown(&ModuleId::terminal()));
            assert_eq!(notes.iter().any(|n| n.contains("corrupt")), unusable, "{notes:?}");
        }
    }

    /// No file and no Lua default: the first launch every build before P2 opened with.
    #[test]
    fn with_neither_the_window_opens_as_it_always_did() {
        let side = [ModuleDecl {
            id: ModuleId::lua("side"),
            placement: Placement::RightOfRoot,
        }];
        let (layout, notes) = choose_startup_layout(None, Some(Loaded::Missing), &side, None).unwrap();
        assert_eq!(layout, crate::terminal::initial_layout(&side).unwrap());
        assert!(notes.is_empty());
    }

    /// A layout that reopens with the editor hidden -- `prefix x` in the editor, then a relaunch --
    /// says what waits for it (the plan review's finding 5): nvim starts on the editor's first frame,
    /// and the window's colours follow nvim. A layout that shows the editor says nothing of the kind.
    #[test]
    fn a_layout_that_hides_the_editor_says_what_waits_for_it() {
        let mut left = crate::terminal::initial_layout(&[]).unwrap();
        left.set_focus(&ModuleId::agent()).unwrap();
        neovibe_core::layout::hide(
            &mut left,
            &ModuleId::editor(),
            &neovibe_core::layout::Frame::new(neovibe_core::layout::Size { w: 1280, h: 721 }, 1),
        )
        .unwrap();
        let (layout, notes) = choose_startup_layout(None, Some(restored(left.clone())), &[], Some("Ctrl+b e")).unwrap();
        assert!(!layout.is_shown(&ModuleId::editor()));
        assert_eq!(
            notes.last().map(String::as_str),
            Some(
                "the editor is hidden in this layout: nvim, and the window's colours that follow it, wait until \
                 it is shown (Ctrl+b e)"
            )
        );
        // With nothing bound to show the editor, the note names no key rather than a wrong one.
        let (_, notes) = choose_startup_layout(None, Some(restored(left)), &[], None).unwrap();
        assert_eq!(
            notes.last().map(String::as_str),
            Some(
                "the editor is hidden in this layout: nvim, and the window's colours that follow it, wait until \
                 it is shown"
            )
        );
        let (_, notes) = choose_startup_layout(None, Some(Loaded::Missing), &[], None).unwrap();
        assert!(notes.iter().all(|n| !n.contains("editor is hidden")), "{notes:?}");
    }

    const ROOT: &str = "/home/user/project";

    fn root() -> &'static Path {
        Path::new(ROOT)
    }

    fn saver_for(dir: &Path, layout: &Rc<RefCell<Layout>>) -> Rc<LayoutSaver> {
        LayoutSaver::new(Some(dir.to_path_buf()), root(), layout.clone(), Vec::new())
    }

    /// A window opened on this project's file, as `main` opens one.
    fn opened(dir: &Path, lua: &[ModuleDecl]) -> Rc<RefCell<Layout>> {
        let Loaded::Restored(r) = persist::load(dir, root(), lua) else {
            panic!("a good file")
        };
        Rc::new(RefCell::new(r.layout))
    }

    fn file_text(dir: &Path) -> String {
        std::fs::read_to_string(dir.join(persist::file_name(root()))).unwrap()
    }

    /// What the file holds after a write of `layout`, byte for byte: this window's whole layout.
    fn written_by(layout: &Rc<RefCell<Layout>>) -> String {
        persist::encode(&layout.borrow(), root())
    }

    fn frame() -> Frame<'static> {
        Frame::new(Size { w: 1280, h: 721 }, 1)
    }

    /// A launch is not a change (the plan review's finding 1): the saver writes nothing until the
    /// arrangement changes, so a file `load` could not use survives a window opened and closed
    /// untouched -- a click into the chat included -- and the first write that does replace it sets
    /// it aside whole, as `.json.unusable`.
    #[test]
    fn nothing_is_written_until_the_layout_changes_and_an_unusable_file_is_kept() {
        let dir = agent::state_dirs::test_workspace_dir("layout-saver");
        let path = dir.join(persist::file_name(root()));
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
        let aside = dir.join(format!("{}.unusable", persist::file_name(root())));
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

    /// Moving the keys is not a change (the controller's ruling of 2026-09-24): a click, and a zoom
    /// and its end, write nothing in a project with no file, and nothing in one with a file, at
    /// close included. The keys go into the file with the next arrangement change, so a project
    /// reopens with them where they were when that change was written -- here the terminal, not
    /// nvim, where they were at close. (Where they are when the debounce fires, not when the change
    /// was made: `the_debounce_writes_an_arrangement_change_and_a_click_arms_nothing`.)
    #[test]
    fn moving_the_keys_writes_nothing_with_or_without_a_file() {
        let dir = agent::state_dirs::test_workspace_dir("layout-saver-keys");
        let path = dir.join(persist::file_name(root()));
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
        persist::save(&dir, root(), &crate::terminal::initial_layout(&[]).unwrap()).unwrap();
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

    /// Two windows on one project (`NON_UNIQUE`): each arrangement change writes that window's whole
    /// layout, and the last write wins -- here the later change, since neither window's wait is
    /// extended; a click in the other window writes nothing, before or after,
    /// so it never puts the arrangement it opened with back over the other's (the whole-branch
    /// review's window finding 1, which this rule settles without reading the other's file).
    #[test]
    fn two_windows_the_later_arrangement_change_wins_and_a_click_writes_nothing() {
        let dir = agent::state_dirs::test_workspace_dir("layout-saver-two-arrangements");
        persist::save(&dir, root(), &crate::terminal::initial_layout(&[]).unwrap()).unwrap();
        let (first, second) = (opened(&dir, &[]), opened(&dir, &[]));
        let first_saver = saver_for(&dir, &first);
        let second_saver = saver_for(&dir, &second);

        first.borrow_mut().show(&ModuleId::terminal()).unwrap();
        neovibe_core::layout::swap(&mut first.borrow_mut(), &ModuleId::editor(), Direction::Right, &frame()).unwrap();
        first_saver.save_now();
        let firsts = written_by(&first);
        assert_eq!(file_text(&dir), firsts);
        second.borrow_mut().set_focus(&ModuleId::agent()).unwrap();
        second_saver.save_now();
        assert_eq!(file_text(&dir), firsts, "the second window's click: nothing written");

        neovibe_core::layout::hide(&mut second.borrow_mut(), &ModuleId::agent(), &frame()).unwrap();
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
        persist::save(&dir, root(), &crate::terminal::initial_layout(&[]).unwrap()).unwrap();
        let lua = vec![ModuleDecl {
            id: ModuleId::lua("notes"),
            placement: Placement::RightOfRoot,
        }];
        let (first, second) = (opened(&dir, &lua), opened(&dir, &[]));
        let first_saver = LayoutSaver::new(Some(dir.clone()), root(), first.clone(), lua.clone());
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
        let path = dir.join(persist::file_name(root()));
        let (startup, _) =
            choose_startup_layout(Some(&bottom_row_default()), Some(Loaded::Missing), &[], None).unwrap();
        let layout = Rc::new(RefCell::new(startup));
        let saver = saver_for(&dir, &layout);
        assert!(neovibe_core::layout::settle_pins(&mut layout.borrow_mut(), &frame()));
        saver.settled();
        layout.borrow_mut().toggle_zoom(&ModuleId::editor());
        saver.save_now();
        layout.borrow_mut().toggle_zoom(&ModuleId::editor());
        saver.save_now();
        assert!(!path.exists(), "the first frame and a zoom: nothing to keep");

        assert!(neovibe_core::layout::resize(
            &mut layout.borrow_mut(),
            &ModuleId::editor(),
            Direction::Down,
            5,
            &frame()
        ));
        saver.save_now();
        assert!(path.exists(), "Ctrl+a j moved the bottom row");
    }

    /// `neovibe.layout.default{ 'column', {'row', {'editor'}, {'agent'}, share = 0.7}, {'terminal'} }`:
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
        let (unsettled, _) =
            choose_startup_layout(Some(&bottom_row_default()), Some(Loaded::Missing), &[], None).unwrap();
        persist::save(&dir, root(), &unsettled).unwrap();
        let before = file_text(&dir);
        assert!(before.contains("\"px\": null"), "{before}");
        let layout = opened(&dir, &[]);
        let saver = saver_for(&dir, &layout);
        assert!(neovibe_core::layout::settle_pins(&mut layout.borrow_mut(), &frame()));
        saver.settled();
        layout.borrow_mut().set_focus(&ModuleId::terminal()).unwrap();
        saver.save_now();
        assert_eq!(file_text(&dir), before, "a settled length and a click: untouched");

        assert!(neovibe_core::layout::resize(
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

    /// Two windows on one project (`NON_UNIQUE`), both opened on a file this build could not use:
    /// the first to save sets it aside and writes a good one; the second must not then set that good
    /// file aside over the bytes the first one kept (the plan review's second round, finding 8).
    #[test]
    fn a_second_window_does_not_set_aside_the_file_the_first_one_wrote() {
        let dir = agent::state_dirs::test_workspace_dir("layout-saver-two-windows");
        std::fs::write(dir.join(persist::file_name(root())), "{ a typo").unwrap();
        let first = Rc::new(RefCell::new(crate::terminal::initial_layout(&[]).unwrap()));
        let second = Rc::new(RefCell::new(crate::terminal::initial_layout(&[]).unwrap()));
        let first_saver = saver_for(&dir, &first);
        let second_saver = saver_for(&dir, &second);
        first.borrow_mut().show(&ModuleId::terminal()).unwrap();
        first_saver.save_now();
        neovibe_core::layout::even(&mut second.borrow_mut(), Axis::Column);
        second_saver.save_now();
        let aside = dir.join(format!("{}.unusable", persist::file_name(root())));
        assert_eq!(std::fs::read_to_string(&aside).unwrap(), "{ a typo");
        let Loaded::Restored(saved) = persist::load(&dir, root(), &[]) else {
            panic!("the second window's layout")
        };
        assert_eq!(saved.layout.root(), second.borrow().root(), "the last to save wins");
    }

    /// A file that becomes unusable while the window is open -- here a hand-edit with a typo, made
    /// after the window opened on a good file -- is set aside at the next write, never written over
    /// (the whole-branch review's finding 7: the guard covered only a file unusable at startup). A
    /// click is not a write, so it leaves such a file where it is, as it would leave a good one.
    #[test]
    fn a_file_made_unusable_while_the_window_is_open_is_set_aside_not_overwritten() {
        let dir = agent::state_dirs::test_workspace_dir("layout-saver-edited");
        let path = dir.join(persist::file_name(root()));
        let aside = dir.join(format!("{}.unusable", persist::file_name(root())));
        persist::save(&dir, root(), &crate::terminal::initial_layout(&[]).unwrap()).unwrap();
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
        neovibe_core::layout::hide(&mut layout.borrow_mut(), &ModuleId::terminal(), &frame()).unwrap();
        saver.save_now();
        assert_eq!(std::fs::read_to_string(&aside).unwrap(), "garbage");
        assert_eq!(file_text(&dir), written_by(&layout));
    }

    /// A file deleted while the window is open -- the owner resetting a project's layout -- is not
    /// written again for a click; the next arrangement change writes one.
    #[test]
    fn a_deleted_file_is_not_written_again_for_a_click() {
        let dir = agent::state_dirs::test_workspace_dir("layout-saver-deleted");
        let path = dir.join(persist::file_name(root()));
        persist::save(&dir, root(), &crate::terminal::initial_layout(&[]).unwrap()).unwrap();
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
    /// binary, since the closure belongs to the thread that armed it (the save rule's re-review, O1:
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
    /// loss, which the close cannot (the save rule's re-review, O1: no test called `changed()`, so
    /// `changed()` never arming the timer, or arming it on every change, left every test green).
    ///
    /// This is the one test in the crate on glib's default main context. It owns the context for its
    /// whole run (`acquire`), and [`Disarm`] takes a pending timer off it if an assertion fails. A
    /// second test on that context would have to take turns with this one: `timeout_add_local_once`
    /// panics while another thread owns it.
    #[test]
    fn the_debounce_writes_an_arrangement_change_and_a_click_arms_nothing() {
        let context = glib::MainContext::default();
        let _owner = context
            .acquire()
            .expect("glib's default main context, which no other test in this crate uses");
        let dir = agent::state_dirs::test_workspace_dir("layout-saver-debounce");
        let path = dir.join(persist::file_name(root()));
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
    /// `.json.unusable` goes, which fails the rename for root too. The change is not lost: `base`
    /// has not moved, so the next save, the close's included, tries again, and once the way is clear
    /// the file is set aside and this window's layout written (the save rule's re-review, O2).
    #[test]
    fn a_set_aside_that_fails_writes_nothing_over_the_unusable_file() {
        let dir = agent::state_dirs::test_workspace_dir("layout-saver-set-aside-fails");
        let path = dir.join(persist::file_name(root()));
        let aside = dir.join(format!("{}.unusable", persist::file_name(root())));
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
    /// save, the close's included, writes it (the save rule's re-review, O2). The write is failed
    /// by a directory where this process's `.tmp` goes, which fails for root too, where a
    /// read-only directory would not.
    #[test]
    fn a_failed_write_is_tried_again() {
        let dir = agent::state_dirs::test_workspace_dir("layout-saver-failed-write");
        let path = dir.join(persist::file_name(root()));
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
    /// (the save rule's re-review, O2: with the settle copying every length, it was never written).
    #[test]
    fn a_settle_does_not_swallow_a_length_the_window_changed() {
        let dir = agent::state_dirs::test_workspace_dir("layout-saver-settle-after-resize");
        let path = dir.join(persist::file_name(root()));
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
            neovibe_core::layout::hide(&mut start, &logs, &frame()).unwrap();
        }
        neovibe_core::layout::settle_pins(&mut start, &frame());
        let layout = Rc::new(RefCell::new(start));
        let saver = LayoutSaver::new(Some(dir.clone()), root(), layout.clone(), lua);

        layout.borrow_mut().set_focus(&ModuleId::terminal()).unwrap();
        assert!(neovibe_core::layout::resize(
            &mut layout.borrow_mut(),
            &ModuleId::terminal(),
            Direction::Up,
            5,
            &frame()
        ));
        layout.borrow_mut().show(&logs).unwrap();
        assert!(
            neovibe_core::layout::settle_pins(&mut layout.borrow_mut(), &frame()),
            "the panel's pin settled: the hook runs"
        );
        saver.settled();
        neovibe_core::layout::hide(&mut layout.borrow_mut(), &logs, &frame()).unwrap();
        saver.save_now();
        assert!(path.exists(), "the terminal's new height is a change, and is written");
        assert_eq!(file_text(&dir), written_by(&layout));
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
        let base = Arrangement::of(&layout);
        let mut keys = layout.clone();
        keys.set_focus(&ModuleId::agent()).unwrap();
        let mut zoomed = layout.clone();
        zoomed.toggle_zoom(&ModuleId::agent());
        let mut shown = layout.clone();
        shown.show(&ModuleId::terminal()).unwrap();
        let mut even = layout.clone();
        neovibe_core::layout::even(&mut even, Axis::Row);
        assert_eq!(Arrangement::of(&keys), base, "a click");
        assert_eq!(Arrangement::of(&zoomed), base, "a zoom");
        assert_ne!(Arrangement::of(&shown), base, "a module shown");
        assert_ne!(Arrangement::of(&even), base, "a ratio");
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
        let (layout, notes) = choose_startup_layout(Some(&tree), None, &[], None).unwrap();
        assert!(!layout.contains(&ModuleId::lua("gone")));
        assert_eq!(
            notes,
            [
                "neovibe.layout.default: left out 'lua:gone': this window has no such module",
                "neovibe.layout.default: placed 'terminal' below the editor, hidden, as a first launch does"
            ]
        );
    }
}
