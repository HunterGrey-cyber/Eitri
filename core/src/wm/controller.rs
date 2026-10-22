//! The window controller: the decisions about windows, modules, focus and keys, as a synchronous state machine.
//! Facts come in as [`HostEvent`]s and verb requests; ordered batches of [`HostOp`]s come out, and the driver applies
//! them.
//!
//! No family of verbs has moved here yet, so every action it runs goes back to the shell as the batch's
//! [`ShellVerb`], and it counts how often the shell changed the layout behind it. What it decides itself is each
//! window's module lifecycle and layout persistence: a change derives the modules' service effects and arms the
//! save's debounce on the scheduler. The timer writes through the window's [`Saver`](super::persist::Saver),
//! and the close's save is
//! [`Controller::persist_now`]. Each family that moves adds its state and its decisions here and deletes the shell
//! code that made them.

use super::host::*;
use super::module::{Lifecycle, SurfaceDecl};
use super::persist::Fired;
use super::window::{EditorPresence, WinId, Window, WindowParts, WindowSpec};
use crate::keymap::action::Action;
use crate::layout::{Con, ModuleId};
use std::collections::BTreeMap;
use std::rc::Rc;
use std::time::Duration;

/// The window controller; see the module documentation.
pub struct Controller {
    scheduler: Rc<dyn Scheduler>,
    windows: BTreeMap<WinId, Window>,
    /// How many `LayoutChangedByShell` reports each window had: a verb arm's, the grid's change hook's and a
    /// workspace rename's or delete's alike, so an arm that changes the grid is counted twice.
    shell_revisions: BTreeMap<WinId, u64>,
    /// The next timer token the controller hands out; the driver's own token is the largest there is.
    next_token: u64,
    /// Specs are retained here only until the live layout is attached.
    declared: BTreeMap<WinId, WindowSpec>,
    pub(super) starts: BTreeMap<WinId, super::start::StartState>,
    #[cfg(test)]
    start_notes: Vec<String>,
}

impl Controller {
    pub fn new(scheduler: Rc<dyn Scheduler>) -> Controller {
        Controller {
            scheduler,
            windows: BTreeMap::new(),
            shell_revisions: BTreeMap::new(),
            next_token: 1,
            declared: BTreeMap::new(),
            starts: BTreeMap::new(),
            #[cfg(test)]
            start_notes: vec![],
        }
    }

    /// Records a window the shell opened, with the layouts it shares and its saver. Opening it again replaces
    /// its record, cancels the old record's save timer, and starts its count anew.
    pub fn open_window(&mut self, win: WinId, spec: WindowSpec, parts: WindowParts) {
        self.declare_window(win, spec);
        self.attach_parts(win, parts);
    }

    /// Records native presence before a window's layout exists.
    pub fn declare_window(&mut self, win: WinId, spec: WindowSpec) {
        if let Some(token) = self.windows.remove(&win).and_then(|old| old.save_timer) {
            self.scheduler.cancel(token);
        }
        self.declared.insert(win, spec);
        self.shell_revisions.insert(win, 0);
    }

    /// Attaches the shared model and saver to a previously declared window.
    pub fn attach_parts(&mut self, win: WinId, mut parts: WindowParts) {
        let Some(spec) = self.declared.remove(&win) else {
            self.start_note("[wm] BUG: attach_parts for a window without a declaration".into());
            return;
        };
        if let Some(saver) = parts.saver.as_mut() {
            saver.set_waker(self.scheduler.waker());
        }
        self.windows.insert(win, Window::new(spec, parts));
    }

    pub(super) fn start_note(&mut self, line: String) {
        eprintln!("{line}");
        #[cfg(test)]
        self.start_notes.push(line);
    }

    #[cfg(test)]
    pub(super) fn start_notes(&self) -> &[String] {
        &self.start_notes
    }

    fn awaiting_layout(&mut self, win: WinId, event: impl std::fmt::Debug) -> bool {
        if !self.declared.contains_key(&win) {
            return false;
        }
        self.start_note(format!("[wm] BUG: {event:?} for a window with no layout yet"));
        true
    }

    /// How the shell described `win`; `None` for a window never opened.
    pub fn window_spec(&self, win: WinId) -> Option<&WindowSpec> {
        self.windows
            .get(&win)
            .map(Window::spec)
            .or_else(|| self.declared.get(&win))
    }

    /// The lifecycle facts recorded for a window, without borrowing its layout.
    pub fn lifecycle(&self, win: WinId) -> Option<&Lifecycle> {
        self.windows.get(&win).map(|window| &window.lifecycle)
    }

    /// The lifecycle of a window, for tests that set up what it holds without its effects.
    #[cfg(test)]
    pub(crate) fn lifecycle_mut(&mut self, win: WinId) -> Option<&mut Lifecycle> {
        self.windows.get_mut(&win).map(|window| &mut window.lifecycle)
    }

    /// Declares surfaces in launch order; only an owned editor needs a local surface. The batch is decided as if
    /// every create applies, so each module counts as hosted from here; a create the host refuses comes back
    /// after the batch as `Refused` naming its module, which leaves that module declared and without effects.
    pub fn create_surfaces(&mut self, win: WinId, decls: &[SurfaceDecl]) -> Batch {
        let mut batch = Batch::default();
        let Some(window) = self.windows.get_mut(&win) else {
            return batch;
        };
        for decl in decls {
            if matches!(decl.kind, SurfaceKind::Editor { .. }) && *window.presence() != EditorPresence::Owned {
                continue;
            }
            window.lifecycle.declare(decl, true);
            batch.push_layout(HostOp::CreateSurface {
                win,
                module: decl.module.clone(),
                kind: decl.kind.clone(),
            });
        }
        batch
    }

    /// Records the shell's initial hosts and runs their effects without reporting a layout change. This entry
    /// stays until the controller owns the initial grid allocation and can derive these declarations itself.
    pub fn startup_applied(&mut self, win: WinId, hosted: &[SurfaceDecl]) -> Batch {
        let mut batch = Batch::default();
        let Some(window) = self.windows.get_mut(&win) else {
            return batch;
        };
        for decl in hosted {
            window.lifecycle.declare(decl, true);
        }
        match window.layout.try_borrow() {
            Ok(layout) => window.lifecycle.after_change(win, &layout, &mut batch),
            Err(_) => eprintln!("[wm] BUG: the layout was borrowed when startup_applied arrived; no lifecycle effect"),
        }
        batch
    }

    fn lifecycle_event(&mut self, win: WinId, event: &HostEvent, batch: &mut Batch) {
        let Some(window) = self.windows.get_mut(&win) else {
            return;
        };
        match window.layout.try_borrow() {
            Ok(layout) => {
                if matches!(event, HostEvent::LayoutChangedByShell { .. }) {
                    window.lifecycle.after_change(win, &layout, batch);
                } else {
                    window.lifecycle.on_event(win, event, &layout, batch);
                }
            }
            Err(_) => {
                eprintln!("[wm] BUG: the layout was borrowed when {event:?} arrived; no lifecycle effect");
                window.lifecycle.record_event(win, event);
            }
        }
    }

    /// How many `LayoutChangedByShell` reports the window had, from whichever hook posted them; 0 for a window
    /// never heard of.
    pub fn shell_revision(&self, win: WinId) -> u64 {
        self.shell_revisions.get(&win).copied().unwrap_or(0)
    }

    /// The close's save of `win`'s layouts: a pending debounce timer cancelled, a change still unwritten
    /// handed to the writer, and a wait for the writer to finish. Whether the file now holds the window's
    /// arrangement: `false` for a window with no saver or no state directory, a writer still writing when the wait
    /// ran out, a write that failed (or an earlier one that did and was not written since), or a layout that could
    /// not be read. A caller that forgets something only the file is to keep from now on needs that, not merely a
    /// finished writer.
    pub fn persist_now(&mut self, win: WinId) -> bool {
        let Some(window) = self.windows.get_mut(&win) else {
            return false;
        };
        if let Some(token) = window.save_timer.take() {
            self.scheduler.cancel(token);
        }
        let Some(saver) = window.saver.as_mut() else {
            return false;
        };
        saver.collect(window.workspaces.try_borrow_mut().ok().as_deref_mut());
        let finished = match (window.layout.try_borrow(), window.workspaces.try_borrow()) {
            (Ok(layout), Ok(workspaces)) => saver.save_now(&layout, &workspaces),
            _ => saver.save_now_unread(),
        };
        saver.collect(window.workspaces.try_borrow_mut().ok().as_deref_mut());
        let holds = match (window.layout.try_borrow(), window.workspaces.try_borrow()) {
            (Ok(layout), Ok(workspaces)) => saver.holds(&layout, &workspaces),
            _ => false,
        };
        finished && holds
    }

    /// The shell changed `win`'s layout or workspaces: the saver says whether the save's debounce is to be
    /// armed.
    fn layout_changed(&mut self, win: WinId) {
        let now = self.scheduler.now();
        let Some(window) = self.windows.get_mut(&win) else {
            return;
        };
        let Some(saver) = window.saver.as_mut() else {
            return;
        };
        saver.collect(window.workspaces.try_borrow_mut().ok().as_deref_mut());
        let arm = match (window.layout.try_borrow(), window.workspaces.try_borrow()) {
            (Ok(layout), Ok(workspaces)) => saver.changed(now, &layout, &workspaces),
            _ => {
                saver.changed_unread(now);
                None
            }
        };
        if let Some(delay) = arm {
            self.arm_save(win, delay);
        }
    }

    /// A timer fired: the save's debounce of whichever window armed it, armed again or written.
    fn timer_fired(&mut self, token: TimerToken) {
        let now = self.scheduler.now();
        let Some((&win, window)) = self
            .windows
            .iter_mut()
            .find(|(_, window)| window.save_timer == Some(token))
        else {
            return;
        };
        window.save_timer = None;
        let Some(saver) = window.saver.as_mut() else {
            return;
        };
        saver.collect(window.workspaces.try_borrow_mut().ok().as_deref_mut());
        let rearm = match (window.layout.try_borrow(), window.workspaces.try_borrow()) {
            (Ok(layout), Ok(workspaces)) => match saver.timer_fired(now, &layout, &workspaces) {
                Fired::Rearm(left) => Some(left),
                Fired::Saved | Fired::NothingToSave => None,
            },
            _ => saver.timer_fired_unread(now),
        };
        if let Some(left) = rearm {
            self.arm_save(win, left);
        }
    }

    /// A pinned split of `win` got its length on screen, which is not a change. `root` is the tree as the
    /// settle left it, not the layout as it is now: a change made between the allocation and this idle is
    /// still a change.
    fn settled(&mut self, win: WinId, root: &Con) {
        if let Some(saver) = self.windows.get_mut(&win).and_then(|window| window.saver.as_mut()) {
            saver.settled(root);
        }
    }

    /// The layout writer finished a write: every window takes what its writer finished.
    fn collect_saves(&mut self) {
        for window in self.windows.values_mut() {
            if let Some(saver) = window.saver.as_mut() {
                saver.collect(window.workspaces.try_borrow_mut().ok().as_deref_mut());
            }
        }
    }

    fn arm_save(&mut self, win: WinId, delay: Duration) {
        let token = TimerToken(self.next_token);
        self.next_token += 1;
        let Some(window) = self.windows.get_mut(&win) else {
            return;
        };
        if let Some(old) = window.save_timer.replace(token) {
            self.scheduler.cancel(old);
        }
        self.scheduler.after(delay, token);
    }

    /// The saver of `win`, for the tests that wait for its writer.
    #[cfg(test)]
    pub(crate) fn saver(&self, win: WinId) -> Option<&super::persist::Saver> {
        self.windows.get(&win).and_then(|window| window.saver.as_ref())
    }
}

impl Decide for Controller {
    fn handle(&mut self, event: HostEvent) -> Batch {
        let mut batch = Batch::default();
        if super::start::event_window(&event).is_some_and(|win| self.awaiting_layout(win, &event)) {
            return batch;
        }
        match &event {
            HostEvent::LayoutChangedByShell { win }
            | HostEvent::Declared { win, .. }
            | HostEvent::SurfaceEnded { win, .. }
            | HostEvent::Terminal(TermEvent::Started { win, .. } | TermEvent::Exited { win, .. })
            | HostEvent::Refused {
                what: OpTag::CreateSurface,
                surface: Some((win, _)),
                ..
            } => {
                self.lifecycle_event(*win, &event, &mut batch);
            }
            _ => {}
        }
        match event {
            HostEvent::LayoutChangedByShell { win } => {
                // A verb run before the window was declared still changed its layout, so the count starts there.
                *self.shell_revisions.entry(win).or_insert(0) += 1;
                self.layout_changed(win);
            }
            HostEvent::Timer(token) => self.timer_fired(token),
            HostEvent::Settled { win, root } => self.settled(win, &root),
            HostEvent::Wake => self.collect_saves(),
            _ => {}
        }
        batch
    }

    fn key(&mut self, win: WinId, stage: &KeyStage, press: &KeyPress) -> (Verdict, Batch) {
        self.awaiting_layout(win, ("key", stage, press));
        (Verdict::PassThrough, Batch::default())
    }

    fn close_requested(&mut self, win: WinId, by: CloseBy) -> (CloseVerdict, Batch) {
        self.awaiting_layout(win, ("close_requested", by));
        (CloseVerdict::Proceed, Batch::default())
    }

    fn run(&mut self, win: WinId, action: Action, origin: Origin) -> Batch {
        if self.awaiting_layout(win, ("run", &action, &origin)) {
            return Batch::default();
        }
        let keycode = match origin {
            Origin::Key { keycode } => keycode,
            _ => 0,
        };
        let mut batch = Batch::default();
        batch.set_shell_verb(ShellVerb { win, action, keycode });
        batch
    }

    fn focus(&mut self, win: WinId, module: &ModuleId) -> Batch {
        let mut batch = Batch::default();
        if self.awaiting_layout(win, ("focus", module)) {
            return batch;
        }
        batch.push_layout(HostOp::Focus {
            win,
            to: FocusTo::Module(module.clone()),
            on_grab: vec![],
        });
        batch
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keymap::{key::KeyName, prefix::Press};
    use crate::layout::Workspaces;
    use crate::wm::fake::FakeScheduler;
    use crate::wm::persist::first_launch_layout;
    use crate::wm::window::{EditorPresence, WindowRole};
    use std::cell::RefCell;

    fn controller() -> Controller {
        Controller::new(FakeScheduler::new())
    }

    #[test]
    fn a_layout_changed_by_the_shell_counts_a_revision_and_asks_for_nothing() {
        let mut c = controller();
        let win = WinId::MAIN;
        c.open_window(
            win,
            WindowSpec {
                role: WindowRole::Main,
                presence: EditorPresence::Owned,
                title: "main".into(),
            },
            WindowParts {
                layout: Rc::new(RefCell::new(first_launch_layout(&[]).unwrap())),
                workspaces: Rc::new(RefCell::new(Workspaces::single())),
                saver: None,
            },
        );
        assert_eq!(c.window_spec(win).map(|spec| spec.title.as_str()), Some("main"));
        assert_eq!(c.shell_revision(win), 0);
        assert!(c.handle(HostEvent::LayoutChangedByShell { win }).is_empty());
        assert!(c.handle(HostEvent::LayoutChangedByShell { win }).is_empty());
        assert_eq!(c.shell_revision(win), 2);
        assert!(c.handle(HostEvent::MonitorsChanged).is_empty());
        assert_eq!(c.shell_revision(win), 2, "other facts change nothing yet");
        assert_eq!(c.shell_revision(WinId(9)), 0);
    }

    #[test]
    fn the_first_controller_focuses_passes_keys_through_and_lets_closes_proceed() {
        let mut c = controller();
        let win = WinId::MAIN;
        let focus = c.focus(win, &ModuleId::terminal());
        assert_eq!(
            focus.ops().cloned().collect::<Vec<_>>(),
            vec![HostOp::Focus {
                win,
                to: FocusTo::Module(ModuleId::terminal()),
                on_grab: vec![],
            }]
        );
        assert!(focus.shell_verb().is_none());
        let press = KeyPress {
            keycode: 38,
            press: Press::Other,
            key: KeyName::Char('a'),
            mods: Mods::default(),
            super_or_hyper: false,
        };
        assert_eq!(
            c.key(win, &KeyStage::Window, &press),
            (Verdict::PassThrough, Batch::default())
        );
        assert_eq!(
            c.close_requested(win, CloseBy::User),
            (CloseVerdict::Proceed, Batch::default())
        );
    }
}
