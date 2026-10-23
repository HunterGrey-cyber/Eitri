//! The window controller: the decisions about windows, modules, focus and keys, as a synchronous state machine.
//! Facts come in as [`HostEvent`]s and verb requests; ordered batches of [`HostOp`]s come out, and the driver applies
//! them.
//!
//! Every action from a key, a Lua call or a panel tab verb goes through one runner, [`Decide::run`]: the
//! container selection's gate and the float's first (a tray chip meets neither), then the action's family. A
//! family that has moved here (the tiling verbs, [`super::verbs::tiling`], the focus verbs,
//! [`super::verbs::focus`], the keys verbs, [`super::verbs::keys`], and the module verbs,
//! [`super::verbs::modules`]) is decided on the window's layout and answered with the batch that shows its change;
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
//!
//! What each window's chrome shows is derived here too ([`super::chrome`]): every entry that returns a batch ends
//! by deriving the fields whose inputs moved, and announces the model once when a value changed.

use super::chrome::{self, ChromeModel, ChromeTitles, Derivations};
use super::host::*;
use super::module::{Lifecycle, SurfaceDecl};
use super::verbs::modules::{self as module_verbs, ModuleSetup};
use super::verbs::{self, FloatGate, SelectionGate};
use super::window::{EditorPresence, PeerId, Timers, WinId, Window, WindowParts, WindowSpec};
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
    /// Each window's open rules, module declarations, start-hidden plugins and project root, which its module
    /// verbs place and kill by.
    setups: BTreeMap<WinId, Rc<ModuleSetup>>,
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
            setups: BTreeMap::new(),
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
            // The prefix strip lists the keys this keymap binds.
            window.chrome_state.dirty.prefix += 1;
        }
        self.keymaps.insert(win, (keymap, module_keys));
    }

    /// What `win`'s module verbs place and kill by, from its config on; before it, no rules, no declarations and
    /// the current directory as the project root.
    pub fn set_module_setup(&mut self, win: WinId, setup: ModuleSetup) {
        self.setups.insert(win, Rc::new(setup));
    }

    /// A chooser pick, by the key `keycode` that made it, uses the same workspace decisions as the action runner.
    pub fn workspace_pick(&mut self, win: WinId, choice: crate::keymap::prefix::WorkspacePick, keycode: u32) -> Batch {
        let mut batch = Batch::default();
        if self.awaiting_layout(win, ("workspace_pick", choice)) {
            return batch;
        }
        if choice == crate::keymap::prefix::WorkspacePick::New {
            return self.run(win, Action::WorkspaceNew, Origin::Key { keycode });
        }
        let (reads, _, setup) = self.module_parts(win);
        if let Some(window) = self.windows.get_mut(&win) {
            verbs::workspaces::pick(window, &*reads, win, choice, &setup, &mut batch);
        }
        self.settle_chrome(&mut batch);
        batch
    }

    /// `win` is linked to the other window of the pair, `peer`, reached through `port`; its moves start from here.
    /// A window already linked keeps its old link's moves only as far as the link's end leaves them.
    pub fn attach_peer(&mut self, win: WinId, peer: PeerId, port: Rc<dyn PeerPort>) -> Batch {
        let mut batch = Batch::default();
        if self.windows.get(&win).is_some_and(|w| w.peer.is_some()) {
            batch = self.detach_peer(win);
        }
        let Some(window) = self.windows.get_mut(&win) else {
            eprintln!("[wm] BUG: a peer attached to {win:?}, which is not held");
            return batch;
        };
        let side = super::moves::side_of(window.role());
        window.peer = Some(super::moves::PeerMoves::new(peer, port, side));
        window.say(format!("[window] linked to the other window ({peer:?})"));
        batch
    }

    /// `win`'s link has gone: every move on it ends as the link's end ends it (a module not yet sent stays here,
    /// an arrival not yet committed is dropped), and the window verbs refuse until a peer is attached again.
    pub fn detach_peer(&mut self, win: WinId) -> Batch {
        let mut batch = Batch::default();
        let (reads, keymap, setup) = self.module_parts(win);
        if let Some(window) = self.windows.get_mut(&win) {
            let cx = module_verbs::Cx {
                reads: &*reads,
                keymap: &keymap,
                setup: &setup,
            };
            super::moves::detach(window, &cx, win, &mut batch);
        }
        self.settle_chrome(&mut batch);
        batch
    }

    /// The window a peer is attached to.
    fn window_of_peer(&self, peer: PeerId) -> Option<WinId> {
        self.windows
            .iter()
            .find(|(_, window)| window.peer.as_ref().is_some_and(|p| p.peer() == peer))
            .map(|(win, _)| *win)
    }

    /// A fact from the other window or the link, decided on the window it is attached to.
    fn peer_event(&mut self, event: PeerEvent, batch: &mut Batch) {
        let Some(win) = self.window_of_peer(event.peer()) else {
            eprintln!("[wm] {event:?}: no window is linked to that peer");
            return;
        };
        let (reads, keymap, setup) = self.module_parts(win);
        if let Some(window) = self.windows.get_mut(&win) {
            let cx = module_verbs::Cx {
                reads: &*reads,
                keymap: &keymap,
                setup: &setup,
            };
            super::moves::peer_event(window, &cx, win, event, batch);
        }
    }

    /// The host could not start or raise the other window. The refusal names no window: a raise's goes to the
    /// window that asked for one, a start's to every window that can have a second one (one per process).
    fn window_op_refused(&mut self, what: OpTag, why: &str, batch: &mut Batch) {
        for (win, window) in &mut self.windows {
            let asked = match what {
                OpTag::RaisePeer => super::moves::raise_refused(window),
                _ => !matches!(window.role(), super::window::WindowRole::Companion),
            };
            if asked {
                verbs::windows::refused(window, *win, &what, why, batch);
                if what == OpTag::RaisePeer {
                    return;
                }
            }
        }
    }

    /// What `win`'s module verbs read: the shell's answers, the keymap and the module setup.
    pub(super) fn module_parts(&self, win: WinId) -> (Rc<dyn HostReads>, Rc<Keymap>, Rc<ModuleSetup>) {
        (
            self.reads.clone(),
            self.keymap(win),
            self.setups.get(&win).cloned().unwrap_or_default(),
        )
    }

    /// Whether a card answer from chat view `view` may be taken, read and nothing else: `Ok` only when the
    /// controller can tell nothing covers the view. A layout being written right now, a window it does not hold, or
    /// a view the window's layout does not hold answer `Err` with the refusal's text, which the caller posts as
    /// [`HostEvent::AnswerRefused`] for the toast.
    pub fn answer_gate(&self, win: WinId, view: ViewId) -> Result<(), String> {
        let (reads, keymap, setup) = self.module_parts(win);
        let cx = module_verbs::Cx {
            reads: &*reads,
            keymap: &keymap,
            setup: &setup,
        };
        module_verbs::answer_gate(self.windows.get(&win), &cx, win, view)
    }

    /// The covered chat view `view` revealed before something opens in it with the keys: the scratch floats over it
    /// that do not hold the keys stowed, the keys staying where they are; with the refusal's text when a float still
    /// covers it.
    pub fn reveal_chat(&mut self, win: WinId, view: ViewId) -> (Batch, Option<String>) {
        let mut batch = Batch::default();
        if self.awaiting_layout(win, ("reveal_chat", view)) {
            return (batch, Some(super::feedback::LAYOUT_CHANGING_TEXT.into()));
        }
        let (reads, keymap, setup) = self.module_parts(win);
        let Some(window) = self.windows.get_mut(&win) else {
            // A window it does not hold is one it cannot judge, so the card waits as the answer gate's would.
            return (batch, Some(super::feedback::LAYOUT_CHANGING_TEXT.into()));
        };
        let cx = module_verbs::Cx {
            reads: &*reads,
            keymap: &keymap,
            setup: &setup,
        };
        let still = module_verbs::reveal_chat(window, &cx, win, view, &mut batch);
        self.settle_chrome(&mut batch);
        (batch, still)
    }

    /// A plugin's follow command for `module`, through the runner's gates as the module key (a show) or
    /// `module.hide` (a hide) meets them, then shown with the keys where they are or hidden as any hide is.
    pub fn follow(&mut self, win: WinId, module: &ModuleId, show: bool) -> Batch {
        let action = if show {
            Action::Module(module.clone())
        } else {
            Action::ModuleHide
        };
        let mut batch = self.run_gated(win, action, Origin::Follow, Some(module));
        self.settle_chrome(&mut batch);
        batch
    }

    /// Whether `module` is shown in `win`'s layout, and whether it is on screen too (not behind a zoom, not on a tab
    /// its container does not draw): what a plugin's follow commands read before they show or hide it. Neither for
    /// a window it does not hold, or a layout being written right now.
    pub fn module_shown(&self, win: WinId, module: &ModuleId) -> (bool, bool) {
        self.windows
            .get(&win)
            .and_then(|window| window.layout.try_borrow().ok())
            .map_or((false, false), |layout| {
                (layout.is_shown(module), layout.is_visible(module))
            })
    }

    /// `scratchpad.show` named the terminal or chat view `module`, which does not exist yet: it is made under that
    /// number and put straight into the scratchpad, shown and given the keys.
    pub fn scratch_new(&mut self, win: WinId, module: &ModuleId) -> Batch {
        let mut batch = Batch::default();
        if self.awaiting_layout(win, ("scratch_new", module)) {
            return batch;
        }
        self.modules_event(win, &mut batch, |window, cx, batch| {
            module_verbs::scratch_new(window, cx, win, module, batch)
        });
        self.settle_chrome(&mut batch);
        batch
    }

    /// `tab.break` from the base view: a new chat view, where the open rules put a broken-out tab, holding the
    /// base view's active tab.
    pub fn tab_break(&mut self, win: WinId) -> Batch {
        let mut batch = Batch::default();
        if self.awaiting_layout(win, "tab_break") {
            return batch;
        }
        self.modules_event(win, &mut batch, |window, cx, batch| {
            module_verbs::tab_break(window, cx, win, batch)
        });
        self.settle_chrome(&mut batch);
        batch
    }

    /// Kills `module` with no y/n, as a `y` to its kill's question would: a terminal whose shell ended by itself.
    /// The last module on screen is the window close, which is the caller's, and is left alone here.
    pub fn kill_confirmed(&mut self, win: WinId, module: &ModuleId) -> Batch {
        let mut batch = Batch::default();
        if self.awaiting_layout(win, ("kill_confirmed", module)) {
            return batch;
        }
        self.modules_event(win, &mut batch, |window, cx, batch| {
            if module_verbs::kill_confirmed(window, cx, win, module, batch) == module_verbs::Ran::Shell {
                window.say(format!(
                    "[modules] {module}: the last module on screen; its close is the window's"
                ));
            }
        });
        self.settle_chrome(&mut batch);
        batch
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
    /// mapped, the float band restacked, the grid allocated, then the modules' lifecycle effects and the chrome
    /// derived from it all. Never the saver: a launch is not a change.
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
        window.layout_touched();
        self.settle_chrome(&mut batch);
        batch
    }

    /// One queued `eitri.layout.*` request whose family has moved here: the container selection's gate by the
    /// action the request stands for (its refusal is logged, with no toast), then the request's own operation
    /// with its own ids, its error logged, then the gate's selection reset. `run` meets both gates through the
    /// runner, as a key does.
    pub fn lua_request(&mut self, win: WinId, request: LayoutRequest) -> Batch {
        let mut batch = Batch::default();
        if !super::lua::decided(&request) {
            self.verb_note(win, format!("[wm] BUG: lua_request for the shell's {request:?}"));
            return batch;
        }
        if self.awaiting_layout(win, ("lua_request", &request)) {
            return batch;
        }
        let (reads, keymap, setup) = self.module_parts(win);
        let Some(window) = self.windows.get_mut(&win) else {
            return batch;
        };
        window.say(format!("[lua] layout: {request:?}"));
        if let LayoutRequest::Run(action) = request {
            return self.run(win, action, Origin::Lua);
        }
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
        let cx = module_verbs::Cx {
            reads: &*reads,
            keymap: &keymap,
            setup: &setup,
        };
        let refused = |err: &dyn std::fmt::Display| format!("[lua] layout: {request:?} refused: {err}");
        let result: Result<(), String> = match &request {
            LayoutRequest::Mark(id) => match window.layout.try_borrow_mut().map(|mut layout| layout.set_mark(id)) {
                // A mark changes only the chrome's text: nothing moves, and the state file does not keep it.
                Ok(Ok(())) => {
                    window.layout_touched();
                    Ok(())
                }
                Ok(Err(err)) => Err(err.to_string()),
                Err(_) => {
                    window.warn(format!("[wm] BUG: the layout was borrowed when {request:?} ran"));
                    Ok(())
                }
            },
            LayoutRequest::Show(id) => {
                module_verbs::lua_show(window, &cx, win, id, &mut batch).map_err(|err| err.to_string())
            }
            LayoutRequest::Hide(id) => {
                module_verbs::hide(window, &cx, win, id, &mut batch).map_err(|err| err.to_string())
            }
            LayoutRequest::Focus(id) => {
                super::focus::focus_and_arrive(window, win, id, &mut batch).map_err(|err| err.to_string())
            }
            LayoutRequest::Split(id, axis) => {
                module_verbs::place_and_arrive(window, &cx, win, id, *axis, &mut batch).map_err(|err| err.to_string())
            }
            // Its refusals, now or once its surface is asked for, are logged under the request's own words.
            LayoutRequest::Open(kind, explicit) => {
                module_verbs::lua_open(
                    window,
                    &cx,
                    win,
                    *kind,
                    explicit.clone(),
                    format!("{request:?}"),
                    &mut batch,
                );
                Ok(())
            }
            LayoutRequest::Switch(name) => {
                verbs::workspaces::switch_or_make(window, win, name, &setup, &*reads, &mut batch).map(|_| ())
            }
            LayoutRequest::Run(_) | LayoutRequest::Float(..) => {
                unreachable!("only the requests decided here get here")
            }
        };
        if let Err(err) = result {
            window.warn(refused(&err));
        }
        if gate == Some(SelectionGate::Run { reset: true }) && window.clear_selection() {
            window.layout_touched();
        }
        self.settle_chrome(&mut batch);
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
        drop(layout);
        if settled.is_some() {
            // The settle wrote the pins' lengths: its `Settled` event, heard from the idle, derives what they move.
            window.layout_touched();
        }
        if window.allocated != Some(size) {
            window.allocated = Some(size);
            chrome::resized(window);
        }
        Some((Rc::new(arrangement), settled))
    }

    /// What `win`'s chrome shows, as a copy; a window never opened shows nothing.
    pub fn chrome(&self, win: WinId) -> ChromeModel {
        self.windows
            .get(&win)
            .map(|window| window.chrome.clone())
            .unwrap_or_default()
    }

    /// What `win`'s chrome names things by: the project, the Lua panels' titles, the tray's modules. The fields
    /// that read them are derived again, and the batch announces what changed.
    pub fn set_titles(&mut self, win: WinId, titles: ChromeTitles) -> Batch {
        let mut batch = Batch::default();
        if let Some(window) = self.windows.get_mut(&win) {
            chrome::set_titles(window, titles);
        }
        self.settle_chrome(&mut batch);
        batch
    }

    /// How many times each of `win`'s chrome fields was derived; nothing for a window never opened.
    pub fn chrome_derivations(&self, win: WinId) -> Derivations {
        self.windows
            .get(&win)
            .map(|window| window.chrome_state.derivations)
            .unwrap_or_default()
    }

    /// Every window's chrome fields whose inputs moved, derived, and the model announced once per window whose
    /// values changed. Every entry that returns a batch ends here; a window with nothing moved allocates nothing.
    fn settle_chrome(&mut self, batch: &mut Batch) {
        let reads = self.reads.clone();
        for (win, window) in &mut self.windows {
            chrome::settle(window, *win, &*reads, self.keymaps.get(win), batch);
        }
    }

    /// The shell's host refused a window mode the controller asked for: a shell that does not carry out
    /// `SetMode` yet toggles its own mode through its arm, as before the chrome's verb moved here.
    fn mode_refused(&mut self, batch: &mut Batch) {
        let Some((win, window)) = self
            .windows
            .iter_mut()
            .find(|(_, window)| window.chrome_state.mode_asked)
        else {
            return;
        };
        window.chrome_state.mode_asked = false;
        window.say("[window_mode] the host refused the mode; the shell's arm sets it".into());
        batch.set_shell_verb(ShellVerb {
            win: *win,
            call: ShellCall::Action(Action::WindowImmersive),
            keycode: 0,
        });
    }

    /// The chat facts the chrome shows: a view's attention or busy state, a page that came on screen (judged once
    /// the queue has drained), and a toast a chat asked for.
    fn chat_chrome(&mut self, event: &HostEvent) {
        let HostEvent::Chat(chat) = event else {
            return;
        };
        let win = match chat {
            ChatEvent::Attention { win, .. }
            | ChatEvent::Busy { win, .. }
            | ChatEvent::PageShown { win, .. }
            | ChatEvent::Toast { win, .. } => *win,
            _ => return,
        };
        let Some(window) = self.windows.get_mut(&win) else {
            return;
        };
        match chat {
            ChatEvent::Attention { view, .. } | ChatEvent::Busy { view, .. } => {
                chrome::attention_changed(window, *view)
            }
            ChatEvent::PageShown { view, .. } => chrome::page_shown(window, *view),
            ChatEvent::Toast { text, kind, .. } => chrome::show_toast(window, text.clone(), *kind),
            _ => {}
        }
    }

    /// Whether the release of `keycode` in `win` is to be reported as [`HostEvent::KeyReleased`]: only the key
    /// the window's prefix consumed and has not seen released is. Asked on every release, so it allocates nothing.
    pub fn watches_release(&self, win: WinId, keycode: u32) -> bool {
        self.windows
            .get(&win)
            .is_some_and(|window| super::keys::watches_release(window, keycode))
    }

    /// The workspace chooser was drawn: `win`'s prefix waits for its key, including a top-bar click.
    pub fn await_workspace(&mut self, win: WinId) -> Batch {
        let mut batch = Batch::default();
        if let Some(window) = self.windows.get_mut(&win) {
            super::keys::await_workspace(window, &mut batch);
        }
        self.settle_chrome(&mut batch);
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
    /// `select.number` numbered, a split's placement, and a chooser's pick against the workspace order.
    fn prefix_command(&mut self, win: WinId, command: PrefixCommand, keycode: u32) -> Batch {
        let mut batch = Batch::default();
        match command {
            PrefixCommand::Run(action) => return self.run(win, action, Origin::Key { keycode }),
            PrefixCommand::Number(digit) => {
                if let Some(window) = self.windows.get_mut(&win) {
                    super::keys::land_digit(window, win, digit, &mut batch);
                }
                batch
            }
            PrefixCommand::Place { module, axis, cwd } => {
                let (reads, keymap, setup) = self.module_parts(win);
                if let Some(window) = self.windows.get_mut(&win) {
                    let cx = module_verbs::Cx {
                        reads: &*reads,
                        keymap: &keymap,
                        setup: &setup,
                    };
                    module_verbs::place(window, &cx, win, &module, axis, cwd, &mut batch);
                }
                batch
            }
            PrefixCommand::Workspace(pick) => self.workspace_pick(win, pick, keycode),
        }
    }

    /// A module fact `f` decides on `win`'s record with the module verbs' reads; nothing for a window never opened.
    fn modules_event(
        &mut self,
        win: WinId,
        batch: &mut Batch,
        f: impl FnOnce(&mut Window, &module_verbs::Cx<'_>, &mut Batch),
    ) {
        let (reads, keymap, setup) = self.module_parts(win);
        let Some(window) = self.windows.get_mut(&win) else {
            return;
        };
        let cx = module_verbs::Cx {
            reads: &*reads,
            keymap: &keymap,
            setup: &setup,
        };
        f(window, &cx, batch);
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
                    super::keys::cancel(window, batch);
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
    /// shell verb the runner handed over gets its selection reset, the chrome is derived again from it all, and the
    /// saver says whether the save's debounce is to be armed.
    fn layout_changed(&mut self, win: WinId) {
        let Some(window) = self.windows.get_mut(&win) else {
            return;
        };
        // The arm may have changed the layout and the workspaces in any way the chrome shows.
        window.layout_touched();
        window.chrome_state.dirty.workspaces += 1;
        // A selection the verb ended is put back here, after its arm; the chrome derived below reads it.
        if window.after_verb.take() == Some(true) {
            window.clear_selection();
        }
        window.reset_mapped();
        window.saver_changed();
    }

    /// A timer fired: the save's debounce of whichever window armed it, armed again or written, a window's
    /// restack after a float took the keys, the end of a window's digit wait, a toast's time up, or the idle that
    /// derives what a resize changed.
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
                || super::moves::deadline_fired(window, *win, token, batch)
                || super::keys::number_timer_fired(window, token, batch)
                || chrome::toast_timer_fired(window, token)
                || chrome::resize_idle_fired(window, token)
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

    /// What the window's chrome was last told the prefix waits for, and the digits it drew, for tests.
    #[cfg(test)]
    pub(crate) fn prefix_shown(&self, win: WinId) -> Option<(crate::keymap::prefix::Waiting, Vec<(char, ModuleId)>)> {
        self.windows
            .get(&win)
            .map(|window| (window.keys.waiting, window.keys.plan.clone()))
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
                self.layout_changed(win);
            }
            HostEvent::Timer(token) => self.timer_fired(token, &mut batch),
            HostEvent::Settled { win, root } => self.settled(win, &root),
            HostEvent::Wake => self.collect_saves(),
            HostEvent::Refused {
                what: OpTag::SetMode, ..
            } => self.mode_refused(&mut batch),
            HostEvent::Created { win, module } => self.modules_event(win, &mut batch, |window, cx, batch| {
                module_verbs::created(window, cx, win, &module, batch);
            }),
            HostEvent::TopBarEntry { win, ev } => {
                if let Some(window) = self.windows.get_mut(&win) {
                    verbs::workspaces::entry(window, win, ev, &mut batch);
                }
            }
            HostEvent::AnswerRefused { win, text } => {
                if let Some(window) = self.windows.get_mut(&win) {
                    window.say(format!("[modules] refused: {text}"));
                    window.refuse(win, text, &mut batch);
                }
            }
            HostEvent::PromptWithdrawn { win } => self.modules_event(win, &mut batch, |window, cx, batch| {
                // A `no` kills nothing, so nothing is left for the shell.
                module_verbs::prompt_answer(window, cx, win, super::close::PromptKey::No, batch);
            }),
            HostEvent::Terminal(TermEvent::Cwd { win, module, read, dir }) => {
                self.modules_event(win, &mut batch, |window, cx, batch| {
                    module_verbs::cwd_read(window, cx, win, &module, read, dir, batch)
                })
            }
            HostEvent::Peer(event) => self.peer_event(event, &mut batch),
            HostEvent::Terminal(TermEvent::Detached {
                win,
                module,
                mv,
                record,
                handoff,
            }) => {
                if let Some(window) = self.windows.get_mut(&win) {
                    super::moves::detached(window, win, &module, mv, record, handoff, &mut batch);
                }
            }
            HostEvent::Prepared { win, module } => {
                if let Some(window) = self.windows.get_mut(&win) {
                    super::moves::prepared(window, &module);
                }
            }
            HostEvent::Refused {
                what: OpTag::PrepareSurface,
                why,
                surface: Some((win, module)),
            } => {
                if let Some(window) = self.windows.get_mut(&win) {
                    super::moves::prepare_refused(window, &module, why);
                }
            }
            // Of the terminal's ops only a freeze names its module when refused.
            HostEvent::Refused {
                what: OpTag::Terminal,
                why,
                surface: Some((win, module)),
            } => {
                if let Some(window) = self.windows.get_mut(&win) {
                    super::moves::detach_refused(window, win, &module, why, &mut batch);
                }
            }
            HostEvent::Refused {
                what: OpTag::AdoptMoved,
                why,
                surface: Some((win, module)),
            } => {
                if let Some(window) = self.windows.get_mut(&win) {
                    super::moves::adopt_refused(window, win, &module, why, &mut batch);
                }
            }
            HostEvent::Refused {
                what: what @ (OpTag::OpenLinkedWindow | OpTag::RaisePeer),
                why,
                ..
            } => self.window_op_refused(what, &why, &mut batch),
            // A refusal from an arm still in the shell flashes and toasts through the model, as the runner's do.
            HostEvent::ShellRefused { win, text } => {
                if let Some(window) = self.windows.get_mut(&win) {
                    chrome::refused(window, &text);
                }
            }
            event => {
                if let HostEvent::Refused {
                    what: OpTag::CreateSurface,
                    why,
                    surface: Some((win, module)),
                } = &event
                {
                    if let Some(window) = self.windows.get_mut(win) {
                        module_verbs::create_refused(window, *win, module, why, &mut batch);
                    }
                }
                self.keys_event(&event, &mut batch);
                self.focus_event(&event, &mut batch);
                self.chat_chrome(&event);
            }
        }
        self.settle_chrome(&mut batch);
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
        if *stage == KeyStage::Window {
            let answer = self
                .windows
                .get(&win)
                .and_then(|window| super::keys::prompt_press(window, press));
            if let Some(answer) = answer {
                let (reads, keymap, setup) = self.module_parts(win);
                if let Some(window) = self.windows.get_mut(&win) {
                    let cx = module_verbs::Cx {
                        reads: &*reads,
                        keymap: &keymap,
                        setup: &setup,
                    };
                    // The kill the y/n asked about turned out to be the last module's: the window close's own
                    // question is the shell's.
                    if module_verbs::prompt_answer(window, &cx, win, answer, &mut batch) == module_verbs::Ran::Shell {
                        window.after_verb = Some(false);
                        batch.set_shell_verb(ShellVerb {
                            win,
                            call: ShellCall::Action(Action::ModuleKill),
                            keycode: press.keycode,
                        });
                    }
                }
                self.settle_chrome(&mut batch);
                return (Verdict::Consumed, batch);
            }
        }
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
                self.settle_chrome(&mut batch);
                (verdict, batch)
            }
            KeyStage::Module(module) => {
                let verdict = super::keys::module_press(window, &*reads, win, module, press, &mut batch);
                self.settle_chrome(&mut batch);
                (verdict, batch)
            }
        }
    }

    fn close_requested(&mut self, win: WinId, by: CloseBy) -> (CloseVerdict, Batch) {
        self.awaiting_layout(win, ("close_requested", by));
        (CloseVerdict::Proceed, Batch::default())
    }

    /// The one runner ([`Controller::run_action`]), and the chrome its batch leaves.
    fn run(&mut self, win: WinId, action: Action, origin: Origin) -> Batch {
        let mut batch = self.run_action(win, action, origin);
        self.settle_chrome(&mut batch);
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

    /// The coverage the batches the queue held left, judged when the layout or the size moved or a chat page came
    /// on screen; a window's answer from the host to a mode it asked for has come by now, if the host gave one.
    fn after_drain(&mut self) -> Batch {
        let mut batch = Batch::default();
        let reads = self.reads.clone();
        for (win, window) in &mut self.windows {
            window.chrome_state.mode_asked = false;
            if chrome::coverage_due(window) {
                chrome::reconcile_coverage(window, *win, &*reads, &mut batch);
            }
        }
        batch
    }
}

impl Controller {
    /// The one runner: the selection's gate (a refusal says how to get back to one module), then the float's (a
    /// refusal says how to sink the float), then the action's family, decided here when it has moved and handed
    /// to the shell otherwise, then the gate's selection reset, whose text the chrome derives again.
    /// A shell verb's reset waits for its re-read, so it comes after the arm as it comes after a verb decided
    /// here. [`Decide::run`] derives the chrome once the whole batch is built.
    fn run_action(&mut self, win: WinId, action: Action, origin: Origin) -> Batch {
        self.run_gated(win, action, origin, None)
    }

    /// [`Controller::run_action`], with `subject` the module a follow command names: its gates are the action's,
    /// and its operation is the follow's own on `subject`.
    fn run_gated(&mut self, win: WinId, action: Action, origin: Origin, subject: Option<&ModuleId>) -> Batch {
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
        let setup = self.setups.get(&win).cloned().unwrap_or_default();
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
        // A chip is a module key from the top bar, which never holds the keys: neither gate is about it.
        let gated = origin != Origin::Chip;
        let gate = if gated {
            verbs::selection_gate(&action, selection)
        } else {
            SelectionGate::Run { reset: false }
        };
        if gate == SelectionGate::Refuse {
            let text = verbs::one_module_text(&keymap());
            window.say(format!("[modules] {}: {text}", action.name()));
            window.refuse(win, text, &mut batch);
            return batch;
        }
        if gated && verbs::float_gate(&action, keys_on_a_float) == FloatGate::Refuse {
            let text = verbs::no_split_text(&keymap());
            window.say(format!("[modules] {}: {text}", action.name()));
            window.refuse(win, text, &mut batch);
            return batch;
        }
        let reset = gate == SelectionGate::Run { reset: true };
        match verbs::family_of(&action, keys_on_a_float, keys_on_the_editor) {
            verbs::Family::Tiling => verbs::tiling::run(window, &*reads, win, &action, &mut batch),
            verbs::Family::Focus => verbs::focus::run(window, &*reads, win, &action, &mut batch),
            verbs::Family::Keys => verbs::keys::run(window, win, &action, keycode, &keymap(), &mut batch),
            verbs::Family::Chrome => verbs::chrome::run(window, win, &action, &mut batch),
            verbs::Family::Workspaces => verbs::workspaces::run(window, &*reads, win, &action, &setup, &mut batch),
            verbs::Family::Windows => {
                let keymap = keymap();
                let cx = module_verbs::Cx {
                    reads: &*reads,
                    keymap: &keymap,
                    setup: &setup,
                };
                verbs::windows::run(window, &cx, win, &action, &mut batch);
            }
            verbs::Family::Modules => {
                let keymap = keymap();
                let cx = module_verbs::Cx {
                    reads: &*reads,
                    keymap: &keymap,
                    setup: &setup,
                };
                let ran = match subject {
                    Some(module) => {
                        let show = matches!(action, Action::Module(_));
                        module_verbs::follow(window, &cx, win, module, show, &mut batch);
                        module_verbs::Ran::Done
                    }
                    None => module_verbs::run(window, &cx, win, &action, &origin, &mut batch),
                };
                if ran == module_verbs::Ran::Shell {
                    // The last module's kill is the window close, whose question the shell asks.
                    batch.set_shell_verb(shell_verb(action));
                    window.after_verb = Some(reset);
                    return batch;
                }
            }
            family => {
                debug_assert!(!verbs::moved(family), "{family:?} has moved but has no runner arm");
                batch.set_shell_verb(shell_verb(action));
                window.after_verb = Some(reset);
                return batch;
            }
        }
        if reset && window.clear_selection() {
            window.layout_touched();
        }
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
        // The shell's change may change what the chrome shows, so its batch may announce the model; nothing else.
        let announce_only = |batch: Batch| batch.ops().all(|op| matches!(op, HostOp::Chrome { .. }));
        assert!(announce_only(c.handle(HostEvent::LayoutChangedByShell { win })));
        assert!(
            c.handle(HostEvent::LayoutChangedByShell { win }).is_empty(),
            "the same layout again changes no chrome value"
        );
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
