//! The window controller: the decisions about windows, modules, focus and keys, as a synchronous state machine.
//! Facts come in as [`HostEvent`]s and verb requests; ordered batches of [`HostOp`]s come out, and the driver applies
//! them.
//!
//! Every action from a key, a Lua call or a panel tab verb goes through one runner, [`Decide::run`]: the
//! container selection's gate and the float's first, then the action's family. A family that has moved here (the
//! tiling verbs, [`super::verbs::tiling`], the focus verbs, [`super::verbs::focus`], and the keys verbs,
//! [`super::verbs::keys`]) is decided on the window's layout and answered with the batch that shows its change;
//! every other action goes back to the shell as the batch's [`ShellVerb`], and the controller counts how often the
//! shell changed the layout behind it.
//!
//! Who holds the keys is decided here too ([`super::focus`]): the requests that give them, and what follows the
//! host's report of where they are. So are the keys themselves ([`super::keys`]): each capture stage's answer, the
//! window's prefix and what its commands do, and the chrome the prefix shows. It also decides each window's module
//! lifecycle and layout persistence: a change derives the modules' service effects and arms the save's debounce on
//! the scheduler. The timer writes through the window's [`Saver`](super::persist::Saver), and the close's save is
//! [`Controller::persist_now`]. Each family that moves adds its state and its decisions here and deletes the shell
//! code that made them.

use super::chrome::ChromeModel;
use super::host::*;
use super::module::{Lifecycle, SurfaceDecl};
use super::verbs::{self, FloatGate, SelectionGate};
use super::window::{EditorPresence, Timers, WinId, Window, WindowParts, WindowSpec};
use crate::keymap::action::Action;
use crate::keymap::prefix::{Prefix, PrefixCommand};
use crate::keymap::Keymap;
use crate::layout::{Arrangement, Con, ModuleId, ModuleKeys, Size};
use crate::lua::layout::LayoutRequest;
use crate::tabs::ViewId;
use std::cell::Cell;
use std::collections::BTreeMap;
use std::rc::Rc;

/// The window controller; see the module documentation.
pub struct Controller {
    scheduler: Rc<dyn Scheduler>,
    /// What the shell answers at once: the grid's size and measures, the editor's cell.
    reads: Rc<dyn HostReads>,
    windows: BTreeMap<WinId, Window>,
    /// Each window's keymap and module keys, from its config phase on; the refusals name keys from them.
    keymaps: BTreeMap<WinId, (Rc<Keymap>, Rc<ModuleKeys>)>,
    /// How many `LayoutChangedByShell` reports each window had: a verb arm's, the grid's change hook's and a
    /// workspace rename's or delete's alike, so an arm that changes the grid is counted twice.
    shell_revisions: BTreeMap<WinId, u64>,
    /// Where every window's timers come from; the driver's own token is the largest there is.
    timers: Timers,
    /// Specs are retained here only until the live layout is attached.
    declared: BTreeMap<WinId, WindowSpec>,
    pub(super) starts: BTreeMap<WinId, super::start::StartState>,
    #[cfg(test)]
    start_notes: Vec<String>,
}

impl Controller {
    pub fn new(scheduler: Rc<dyn Scheduler>) -> Controller {
        Controller {
            timers: Timers {
                scheduler: scheduler.clone(),
                next: Rc::new(Cell::new(1)),
            },
            scheduler,
            reads: Rc::new(NoReads),
            windows: BTreeMap::new(),
            keymaps: BTreeMap::new(),
            shell_revisions: BTreeMap::new(),
            declared: BTreeMap::new(),
            starts: BTreeMap::new(),
            #[cfg(test)]
            start_notes: vec![],
        }
    }

    /// What the controller reads of the shell from now on; until it is given, [`NoReads`]' answers.
    pub fn set_reads(&mut self, reads: Rc<dyn HostReads>) {
        self.reads = reads;
    }

    /// The keymap and the module keys `win`'s config built, which its refusals name keys from and its prefix
    /// reads. A window already held gets a new prefix over them, waiting for nothing.
    pub fn set_keymap(&mut self, win: WinId, keymap: Rc<Keymap>, module_keys: Rc<ModuleKeys>) {
        if let Some(window) = self.windows.get_mut(&win) {
            window.keys.prefix = Some(Prefix::new(keymap.clone(), module_keys.clone()));
        }
        self.keymaps.insert(win, (keymap, module_keys));
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
        let mut window = Window::new(spec, parts, self.timers.clone());
        if let Some((keymap, module_keys)) = self.keymaps.get(&win) {
            window.keys.prefix = Some(Prefix::new(keymap.clone(), module_keys.clone()));
        }
        self.windows.insert(win, window);
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

    /// The window's first layout batch, with the hosts the shell placed declared: every visible hosted module
    /// mapped, the float band restacked, the grid allocated, then the modules' lifecycle effects and the shell's
    /// change hooks. Never the saver: a launch is not a change.
    pub fn first_apply(&mut self, win: WinId, hosted: &[SurfaceDecl]) -> Batch {
        let mut batch = Batch::default();
        let Some(window) = self.windows.get_mut(&win) else {
            return batch;
        };
        for decl in hosted {
            window.lifecycle.declare(decl, true);
        }
        window.layout_ops(win, None, &mut batch);
        let layout = window.layout.clone();
        match layout.try_borrow() {
            Ok(layout) => window.lifecycle.after_change(win, &layout, &mut batch),
            Err(_) => window.warn("[wm] BUG: the layout was borrowed at the first apply; no lifecycle effect".into()),
        }
        batch.push_effect(HostOp::ChromeHooks { win });
        batch
    }

    /// One queued `eitri.layout.*` request whose family has moved here: the container selection's gate by the
    /// action the request stands for (its refusal is logged, with no toast), then the request's own operation
    /// with its own ids, its error logged, then the gate's selection reset. `run` meets both gates through the
    /// runner, as a key does.
    pub fn lua_request(&mut self, win: WinId, request: LayoutRequest) -> Batch {
        let mut batch = Batch::default();
        if !super::lua::moved(&request) {
            self.verb_note(win, format!("[wm] BUG: lua_request for an unmoved {request:?}"));
            return batch;
        }
        if self.awaiting_layout(win, ("lua_request", &request)) {
            return batch;
        }
        let keymap = self.keymap(win);
        let Some(window) = self.windows.get_mut(&win) else {
            return batch;
        };
        window.say(format!("[lua] layout: {request:?}"));
        let id = match request {
            LayoutRequest::Run(action) => return self.run(win, action, Origin::Lua),
            LayoutRequest::Mark(ref id) => id.clone(),
            _ => unreachable!("only the moved requests get here"),
        };
        let Ok(selection) = window.layout.try_borrow().map(|layout| layout.selection()) else {
            window.warn(format!("[wm] BUG: the layout was borrowed when {request:?} arrived"));
            return batch;
        };
        let gate = super::lua::lua_request_action(&request).map(|action| verbs::selection_gate(&action, selection));
        if gate == Some(SelectionGate::Refuse) {
            let text = verbs::one_module_text(&keymap);
            window.warn(format!("[lua] layout: {request:?} refused: {text}"));
            return batch;
        }
        let marked = window.layout.try_borrow_mut().map(|mut layout| layout.set_mark(&id));
        match marked {
            // A mark changes only the chrome's text: nothing moves, and the state file does not keep it.
            Ok(Ok(())) => push_chrome_hooks(&mut batch, win),
            Ok(Err(err)) => window.warn(format!("[lua] layout: {request:?} refused: {err}")),
            Err(_) => window.warn(format!("[wm] BUG: the layout was borrowed when {request:?} ran")),
        }
        if gate == Some(SelectionGate::Run { reset: true }) && window.clear_selection() {
            push_chrome_hooks(&mut batch, win);
        }
        batch
    }

    /// The window's allocation at `size`: the pinned splits shown for the first time settled (with the tree as
    /// the settle left it, for the saver to hear later), then the layout arranged in the frame the grid's
    /// measures give. `None` for a window never opened or a layout borrowed right now.
    pub fn allocate(&mut self, win: WinId, size: Size) -> Option<(Rc<Arrangement>, Option<Con>)> {
        let reads = self.reads.clone();
        let window = self.windows.get_mut(&win)?;
        let layout = window.layout.clone();
        let mut settled = None;
        // A pin keeps the length its ratio gives at a real allocation, never at a stand-in size; asked first, with
        // no geometry, so a drag or a resize sweep arranges once per allocation.
        if size.w > 0 && size.h > 0 && layout.try_borrow().is_ok_and(|layout| layout.has_pin_to_settle()) {
            if let Ok(mut layout) = layout.try_borrow_mut() {
                if frame_with(&*reads, win, size, |frame| {
                    crate::layout::settle_pins(&mut layout, frame)
                }) {
                    settled = Some(layout.root().clone());
                }
            }
        }
        let Ok(layout) = layout.try_borrow() else {
            window.warn("[wm] BUG: the layout was borrowed when the grid allocated".into());
            return None;
        };
        let arrangement = frame_with(&*reads, win, size, |frame| crate::layout::arrange(&layout, frame));
        window.allocated = Some(size);
        Some((Rc::new(arrangement), settled))
    }

    /// What `win`'s chrome shows, as a copy; a window never opened shows nothing.
    pub fn chrome(&self, win: WinId) -> ChromeModel {
        self.windows
            .get(&win)
            .map(|window| window.chrome.clone())
            .unwrap_or_default()
    }

    /// Whether the release of `keycode` in `win` is to be reported as [`HostEvent::KeyReleased`]: only the key
    /// the window's prefix consumed and has not seen released is. Asked on every release, so it allocates nothing.
    pub fn watches_release(&self, win: WinId, keycode: u32) -> bool {
        self.windows
            .get(&win)
            .is_some_and(|window| super::keys::watches_release(window, keycode))
    }

    /// The workspace chooser was drawn: `win`'s prefix waits for its key. The chooser's own arm calls this while
    /// the workspace verbs are still the shell's.
    pub fn await_workspace(&mut self, win: WinId) -> Batch {
        let mut batch = Batch::default();
        if let Some(window) = self.windows.get_mut(&win) {
            super::keys::await_workspace(window, win, &mut batch);
        }
        batch
    }

    /// The `?` sheet opened in chat view `view`'s page from where `opened` says the keys were: kept as the
    /// window's way back unless the keys were in that view already, and the opening's number returned for the
    /// page to quote when the sheet closes ([`ChatEvent::KeymapClosed`]). The `prefix ?` arm calls this while the
    /// panel verbs are still the shell's; 0 for a window never opened, whose close then returns nothing.
    pub fn keymap_opened(&mut self, win: WinId, view: ViewId, opened: crate::keymap::keys_return::Opened) -> u64 {
        match self.windows.get_mut(&win) {
            Some(window) => window.focus.keys_return.opened(opened, &view.module()),
            None => 0,
        }
    }

    /// A request newer than an open `?` sheet (the command line, a workspace switch) took over: the sheet's way
    /// back is forgotten, so its close does not move the keys out from under the newer request.
    pub fn keymap_superseded(&mut self, win: WinId) {
        if let Some(window) = self.windows.get_mut(&win) {
            window.focus.keys_return.superseded();
        }
    }

    /// A command the window's prefix finished: an action through the runner as a key's, a digit landing in what
    /// `select.number` numbered, and a split's placement or a chooser's pick as the shell's verb, whose families
    /// are still the shell's.
    fn prefix_command(&mut self, win: WinId, command: PrefixCommand, keycode: u32) -> Batch {
        let mut batch = Batch::default();
        let call = match command {
            PrefixCommand::Run(action) => return self.run(win, action, Origin::Key { keycode }),
            PrefixCommand::Number(digit) => {
                if let Some(window) = self.windows.get_mut(&win) {
                    super::keys::land_digit(window, win, digit, &mut batch);
                }
                return batch;
            }
            PrefixCommand::Place { module, axis, cwd } => {
                // A split ends a container selection, as `split.*`'s rule says; it is put back after the arm's
                // re-read, as for every shell verb that resets it.
                if let Some(window) = self.windows.get_mut(&win) {
                    window.after_verb = Some(true);
                }
                ShellCall::Place { module, axis, cwd }
            }
            PrefixCommand::Workspace(pick) => ShellCall::WorkspacePick(pick),
        };
        batch.set_shell_verb(ShellVerb { win, call, keycode });
        batch
    }

    /// The keys' facts: a watched release, and the window losing activation or being clicked, which end a wait of
    /// the prefix's.
    fn keys_event(&mut self, event: &HostEvent, batch: &mut Batch) {
        match event {
            HostEvent::KeyReleased { win, keycode } => {
                if let Some(window) = self.windows.get_mut(win) {
                    super::keys::released(window, *keycode);
                }
            }
            HostEvent::Active { win, active: false }
            | HostEvent::Pointer {
                win,
                ev: PointerEv::Press { .. },
                ..
            } => {
                if let Some(window) = self.windows.get_mut(win) {
                    super::keys::cancel(window, *win, batch);
                }
            }
            _ => {}
        }
    }

    /// What `win`'s layout needs at the least in the frame the grid's measures give now.
    pub fn min_size(&self, win: WinId) -> Size {
        let Some(window) = self.windows.get(&win) else {
            return Size::default();
        };
        let Ok(layout) = window.layout.try_borrow() else {
            return Size::default();
        };
        let size = self.reads.grid_size(win);
        frame_with(&*self.reads, win, size, |frame| crate::layout::min_size(&layout, frame))
    }

    /// The size of `win`'s last allocation; `None` before its first.
    pub fn allocated(&self, win: WinId) -> Option<Size> {
        self.windows.get(&win).and_then(|window| window.allocated)
    }

    /// The lines `win`'s verbs printed, for tests.
    #[cfg(test)]
    pub(crate) fn notes(&self, win: WinId) -> Vec<String> {
        self.windows
            .get(&win)
            .map(|window| window.notes().to_vec())
            .unwrap_or_default()
    }

    fn verb_note(&mut self, win: WinId, line: String) {
        match self.windows.get_mut(&win) {
            Some(window) => window.warn(line),
            None => eprintln!("{line}"),
        }
    }

    /// The keymap `win`'s refusals name keys from: its config's, or the defaults before its config phase.
    fn keymap(&self, win: WinId) -> Rc<Keymap> {
        self.keymaps
            .get(&win)
            .map(|(keymap, _)| keymap.clone())
            .unwrap_or_else(|| Rc::new(Keymap::defaults()))
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

    /// The shell changed `win`'s layout or workspaces: what its apply left on screen is the grid's state now, a
    /// shell verb the runner handed over gets its selection reset (redrawn when it ended one), and the saver says
    /// whether the save's debounce is to be armed.
    fn layout_changed(&mut self, win: WinId, batch: &mut Batch) {
        let Some(window) = self.windows.get_mut(&win) else {
            return;
        };
        // The arm's own grid changes ran the hooks already; a selection put back here is a change they missed.
        if window.after_verb.take() == Some(true) && window.clear_selection() {
            batch.push_effect(HostOp::ChromeHooks { win });
        }
        window.reset_mapped();
        window.saver_changed();
    }

    /// A timer fired: the save's debounce of whichever window armed it, armed again or written, a window's
    /// restack after a float took the keys, or the end of a window's digit wait.
    fn timer_fired(&mut self, token: TimerToken, batch: &mut Batch) {
        if let Some(window) = self
            .windows
            .values_mut()
            .find(|window| window.save_timer == Some(token))
        {
            window.save_timer_fired();
            return;
        }
        for (win, window) in &mut self.windows {
            if super::focus::raise_timer_fired(window, *win, token, batch)
                || super::keys::number_timer_fired(window, *win, token, batch)
            {
                return;
            }
        }
    }

    /// The focus family's facts: where the host says the keys are, the window's activity, the `?` sheet
    /// closing, and every route that lands on a module or moves the keys by direction.
    fn focus_event(&mut self, event: &HostEvent, batch: &mut Batch) {
        let reads = self.reads.clone();
        let win = match event {
            HostEvent::NativeFocus { win, .. }
            | HostEvent::Active { win, .. }
            | HostEvent::Chat(ChatEvent::KeymapClosed { win, .. } | ChatEvent::NavFallthrough { win, .. })
            | HostEvent::Editor(EditorEvent::Navigate { win, .. })
            | HostEvent::Pointer { win, .. }
            | HostEvent::Hint(HintEvent::Landed { win, .. })
            | HostEvent::TopBarLeft { win } => *win,
            HostEvent::Refused {
                what: OpTag::Focus,
                surface,
                ..
            } => {
                // A refused grab never reports, so its request must not vouch for a later report of that module;
                // the other grabs of its batch still report, and their requests stand.
                if let Some((win, module)) = surface {
                    if let Some(window) = self.windows.get_mut(win) {
                        super::focus::grab_refused(window, module);
                    }
                }
                return;
            }
            _ => return,
        };
        let Some(w) = self.windows.get_mut(&win) else {
            return;
        };
        let strip_landing = |w: &mut Window, module: &ModuleId, batch: &mut Batch| {
            if let Err(err) = super::focus::focus_and_arrive(w, win, module, batch) {
                w.say(format!("[modules] {module}: its strip entry chose nothing: {err}"));
            }
        };
        match event {
            HostEvent::NativeFocus {
                owner,
                active,
                op_index,
                ..
            } => super::focus::on_native_focus(w, win, owner.clone(), *active, *op_index, batch),
            HostEvent::Active { active, .. } => super::focus::on_active(w, win, *active, batch),
            HostEvent::Chat(ChatEvent::KeymapClosed {
                view, opening, forced, ..
            }) => super::focus::keymap_closed(w, &*reads, win, *view, *opening, *forced, batch),
            HostEvent::Chat(ChatEvent::NavFallthrough { view, direction, .. }) => {
                // The page could not apply a navigation key against its own state, so it asks for what the key
                // would have done.
                super::focus::move_focus(w, &*reads, win, &view.module(), *direction, batch);
            }
            HostEvent::Editor(EditorEvent::Navigate { direction, .. }) => {
                super::focus::move_focus(w, &*reads, win, &ModuleId::editor(), *direction, batch);
            }
            HostEvent::Pointer {
                on: PointerTarget::StripEntry(module),
                ev: PointerEv::Press { .. },
                ..
            } => strip_landing(w, module, batch),
            // A strip entry chosen by HINT lands as a click would; a module chosen by HINT only gets the keys.
            HostEvent::Hint(HintEvent::Landed {
                target: HintTarget::StripEntry(module),
                ..
            }) => strip_landing(w, module, batch),
            HostEvent::Hint(HintEvent::Landed {
                target: HintTarget::Module(module),
                ..
            }) => {
                super::focus::give_keys(w, win, module, vec![], batch);
            }
            HostEvent::TopBarLeft { .. } => super::focus::return_to_pane(w, win, batch),
            _ => {}
        }
    }

    /// The window's record, for tests that drive one of its families directly.
    #[cfg(test)]
    pub(crate) fn window_mut(&mut self, win: WinId) -> Option<&mut Window> {
        self.windows.get_mut(&win)
    }

    /// The window's focus state, for tests.
    #[cfg(test)]
    pub(crate) fn focus_state(&self, win: WinId) -> Option<&super::focus::FocusState> {
        self.windows.get(&win).map(|window| &window.focus)
    }

    /// What the controller reads of the shell, for tests that call a family's functions directly.
    #[cfg(test)]
    pub(crate) fn reads(&self) -> Rc<dyn HostReads> {
        self.reads.clone()
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
                if let Some(window) = self.windows.get_mut(win) {
                    super::focus::modules_changed(window, *win, &mut batch);
                }
            }
            _ => {}
        }
        match event {
            HostEvent::LayoutChangedByShell { win } => {
                // A verb run before the window was declared still changed its layout, so the count starts there.
                *self.shell_revisions.entry(win).or_insert(0) += 1;
                self.layout_changed(win, &mut batch);
            }
            HostEvent::Timer(token) => self.timer_fired(token, &mut batch),
            HostEvent::Settled { win, root } => self.settled(win, &root),
            HostEvent::Wake => self.collect_saves(),
            event => {
                self.keys_event(&event, &mut batch);
                self.focus_event(&event, &mut batch);
            }
        }
        batch
    }

    /// One capture stage's answer for `press` ([`super::keys`]). A key that passes through, which is nearly
    /// every key, builds nothing on its way.
    fn key(&mut self, win: WinId, stage: &KeyStage, press: &KeyPress) -> (Verdict, Batch) {
        let mut batch = Batch::default();
        if self.awaiting_layout(win, ("key", stage, press)) {
            return (Verdict::PassThrough, batch);
        }
        let reads = self.reads.clone();
        let Some(window) = self.windows.get_mut(&win) else {
            return (Verdict::PassThrough, batch);
        };
        match stage {
            KeyStage::Window => {
                let (verdict, command) = super::keys::window_press(window, win, press, &mut batch);
                if let Some(command) = command {
                    let more = self.prefix_command(win, command, press.keycode);
                    batch.append(more);
                }
                (verdict, batch)
            }
            KeyStage::Module(module) => {
                let verdict = super::keys::module_press(window, &*reads, win, module, press, &mut batch);
                (verdict, batch)
            }
        }
    }

    fn close_requested(&mut self, win: WinId, by: CloseBy) -> (CloseVerdict, Batch) {
        self.awaiting_layout(win, ("close_requested", by));
        (CloseVerdict::Proceed, Batch::default())
    }

    /// The one runner: the selection's gate (a refusal says how to get back to one module), then the float's (a
    /// refusal says how to sink the float), then the action's family, decided here when it has moved and handed
    /// to the shell otherwise, then the gate's selection reset and the change hooks that redraw the selection.
    /// A shell verb's reset waits for its re-read, so it comes after the arm as it comes after a verb decided
    /// here.
    fn run(&mut self, win: WinId, action: Action, origin: Origin) -> Batch {
        let mut batch = Batch::default();
        if self.awaiting_layout(win, ("run", &action, &origin)) {
            return batch;
        }
        let keycode = match origin {
            Origin::Key { keycode } => keycode,
            _ => 0,
        };
        let shell_verb = |action: Action| ShellVerb {
            win,
            call: ShellCall::Action(action),
            keycode,
        };
        if !self.windows.contains_key(&win) {
            // A window the controller does not hold has only the shell's arms.
            batch.set_shell_verb(shell_verb(action));
            return batch;
        }
        let keymap = self.keymaps.get(&win).map(|(keymap, _)| keymap.clone());
        let keymap = || keymap.unwrap_or_else(|| Rc::new(Keymap::defaults()));
        let reads = self.reads.clone();
        let window = self.windows.get_mut(&win).expect("checked above");
        let facts = window.layout.try_borrow().map(|layout| {
            (
                layout.selection(),
                layout.is_floating(layout.focus()),
                *layout.focus() == ModuleId::editor(),
            )
        });
        let Ok((selection, keys_on_a_float, keys_on_the_editor)) = facts else {
            window.warn(format!("[wm] BUG: the layout was borrowed when {} ran", action.name()));
            return batch;
        };
        let gate = verbs::selection_gate(&action, selection);
        if gate == SelectionGate::Refuse {
            let text = verbs::one_module_text(&keymap());
            window.say(format!("[modules] {}: {text}", action.name()));
            batch.push_effect(HostOp::Refuse { win, text });
            return batch;
        }
        if verbs::float_gate(&action, keys_on_a_float) == FloatGate::Refuse {
            let text = verbs::no_split_text(&keymap());
            window.say(format!("[modules] {}: {text}", action.name()));
            batch.push_effect(HostOp::Refuse { win, text });
            return batch;
        }
        let reset = gate == SelectionGate::Run { reset: true };
        match verbs::family_of(&action, keys_on_a_float, keys_on_the_editor) {
            verbs::Family::Tiling => verbs::tiling::run(window, &*reads, win, &action, &mut batch),
            verbs::Family::Focus => verbs::focus::run(window, &*reads, win, &action, &mut batch),
            verbs::Family::Keys => verbs::keys::run(window, win, &action, keycode, &keymap(), &mut batch),
            family => {
                debug_assert!(!verbs::moved(family), "{family:?} has moved but has no runner arm");
                batch.set_shell_verb(shell_verb(action));
                window.after_verb = Some(reset);
                return batch;
            }
        }
        if reset && window.clear_selection() {
            push_chrome_hooks(&mut batch, win);
        }
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

/// The shell's change hooks once per batch: they read the layout as the whole batch leaves it.
fn push_chrome_hooks(batch: &mut Batch, win: WinId) {
    let hooks = HostOp::ChromeHooks { win };
    if !batch.ops().any(|op| *op == hooks) {
        batch.push_effect(hooks);
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
            key: Some(KeyName::Char('a')),
            mods: Mods::default(),
            super_or_hyper: false,
            super_or_meta: false,
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
