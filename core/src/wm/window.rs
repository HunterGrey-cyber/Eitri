//! Window identities and native window descriptions shared with the host, and the controller's own record of
//! each window.

use super::chrome::{ChromeModel, ChromeState};
use super::focus::FocusState;
use super::grid::drag::Drag;
use super::grid::{floats_need_restack, shown_floats};
use super::host::EditorSpawn;
use super::host::{Batch, FocusTo, HostOp, RestackMove, Scheduler, TimerToken};
use super::keys::KeyState;
use super::module::{Lifecycle, ModuleState};
use super::persist::{Fired, Saver};
use crate::layout::{Arrangement, Layout, ModuleId, Rect as LayoutRect, Size, Workspaces};
use crate::window_link::names::WindowName;
use crate::window_link::Summary;
use std::cell::{Cell, Ref, RefCell};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::rc::Rc;
use std::time::{Duration, Instant};

/// A stable identity for one application window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WinId(pub u32);

/// A stable identity for a linked process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PeerId(pub u32);

impl WinId {
    /// The initial main window.
    pub const MAIN: Self = Self(1);
}

/// Which end of a linked pair a window occupies.
#[derive(Debug, Clone, PartialEq)]
pub enum LinkSide {
    /// The first end.
    First,
    /// The second end.
    Second,
}

/// How a window takes part in the compositor's tiling. Set once, at the start plan's layout step, since the mode
/// is known only after `init.lua` and the plugins have run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Tiling {
    /// Everything lives inside the window; the compositor sees one window.
    #[default]
    Eitri,
    /// The window holds the editor and the chrome, and chats and pages are windows of the compositor beside it.
    Hub,
    /// A chat or page that is a window of the compositor.
    ModuleWindow,
}

/// A key action waiting for a module window that is starting: its name and the options that rebuild it.
pub type PendingRun = (String, Vec<(String, serde_json::Value)>);

/// The purpose of a native window.
#[derive(Debug, Clone, PartialEq)]
pub enum WindowRole {
    /// The main workspace.
    Main,
    /// A companion chat window.
    Companion,
    /// One end of a linked pair.
    Linked { side: LinkSide },
}

/// How the window reaches the editor.
#[derive(Debug, Clone, PartialEq)]
pub enum EditorPresence {
    /// Owns the editor process.
    Owned,
    /// Displays an editor owned elsewhere.
    Proxy,
    /// Has no editor.
    None,
}

/// The initial description of a window.
#[derive(Debug, Clone, PartialEq)]
pub struct WindowSpec {
    /// The window role.
    pub role: WindowRole,
    /// The editor relationship.
    pub presence: EditorPresence,
    /// The native title.
    pub title: String,
}

/// What the shell hands the controller when it opens a window: the layout it starts with, every workspace beside
/// it, and the saver of its state file (`None` for a window that keeps no layout file). The shell gives the two by
/// value and keeps only [`LayoutView`] and [`WorkspacesView`] on them: from then on the controller is the one
/// writer of both.
pub struct WindowParts {
    /// The current workspace's layout, the one on screen.
    pub(crate) layout: Rc<RefCell<Layout>>,
    /// Every workspace of the window; the current one's layout is `layout`.
    pub(crate) workspaces: Rc<RefCell<Workspaces>>,
    /// Writes the window's layouts to the project's state file.
    pub(crate) saver: Option<Saver>,
}

impl WindowParts {
    /// The parts of a window that starts with `layout` and `workspaces`.
    pub fn new(layout: Layout, workspaces: Workspaces, saver: Option<Saver>) -> WindowParts {
        WindowParts {
            layout: Rc::new(RefCell::new(layout)),
            workspaces: Rc::new(RefCell::new(workspaces)),
            saver,
        }
    }

    /// Read-only handles on the layout and the workspaces these parts carry, live once the controller has them.
    pub fn views(&self) -> (LayoutView, WorkspacesView) {
        (LayoutView(self.layout.clone()), WorkspacesView(self.workspaces.clone()))
    }
}

/// A read-only handle on a window's live layout: what the grid draws and every reader in the shell holds. The
/// controller writes the layout only while it decides, never while its ops are carried out, so a read from a
/// toolkit callback finds it free; [`LayoutView::try_read`] answers `None` in the moment it is not.
#[derive(Clone)]
pub struct LayoutView(Rc<RefCell<Layout>>);

impl LayoutView {
    /// The layout as it is now. Panics if the controller is writing it, which no read from an op or a toolkit
    /// callback can meet; a reader that cannot rule it out asks [`LayoutView::try_read`].
    pub fn read(&self) -> Ref<'_, Layout> {
        self.0.borrow()
    }

    /// The layout as it is now, or `None` while the controller writes it.
    pub fn try_read(&self) -> Option<Ref<'_, Layout>> {
        self.0.try_borrow().ok()
    }

    /// A copy of the layout as it is now, or `None` while the controller writes it: what a reader takes that must
    /// hold no borrow while it works (Lua building `eitri.layout.get`'s table).
    pub fn copy(&self) -> Option<Layout> {
        self.try_read().map(|layout| layout.clone())
    }

    /// A view on a layout of its own, with the cell a test writes it through.
    #[cfg(any(test, feature = "test-support"))]
    pub fn for_test(layout: Layout) -> (LayoutView, Rc<RefCell<Layout>>) {
        let cell = Rc::new(RefCell::new(layout));
        (LayoutView(cell.clone()), cell)
    }
}

/// A read-only handle on a window's workspaces, as [`LayoutView`] is on its layout.
#[derive(Clone)]
pub struct WorkspacesView(Rc<RefCell<Workspaces>>);

impl WorkspacesView {
    /// The workspaces as they are now; panics only where [`LayoutView::read`] would.
    pub fn read(&self) -> Ref<'_, Workspaces> {
        self.0.borrow()
    }

    /// The workspaces as they are now, or `None` while the controller writes them.
    pub fn try_read(&self) -> Option<Ref<'_, Workspaces>> {
        self.0.try_borrow().ok()
    }

    /// A copy of the workspaces as they are now, or `None` while the controller writes them.
    pub fn copy(&self) -> Option<Workspaces> {
        self.try_read().map(|workspaces| workspaces.clone())
    }
}

/// Where a window's timers come from: the controller's scheduler, and the counter every window's tokens are
/// taken from, so no two timers of the process share one.
#[derive(Clone)]
pub struct Timers {
    pub(super) scheduler: Rc<dyn Scheduler>,
    pub(super) next: Rc<Cell<u64>>,
}

impl Timers {
    fn token(&self) -> TimerToken {
        let token = TimerToken(self.next.get());
        self.next.set(token.0 + 1);
        token
    }
}

/// Whether this window's editor can receive requests or needs its pane released and restarted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditorRun {
    Running,
    Ending,
    Ended,
    Restarting,
}

/// The controller's record of one window. It is the one writer of the window's layout and workspaces; the shell
/// reads them through [`LayoutView`] and [`WorkspacesView`], and the controller still reads them with `try_borrow`,
/// so a read from inside one of its own writes is a logged mistake, never a panic.
pub struct Window {
    /// The plugin whose declared root is this window's project, absent in window 2.
    pub own_window: Option<String>,
    /// How the window takes part in the compositor's tiling.
    pub tiling: Tiling,
    pub(super) close: super::close::CloseFlow,
    pub(super) quit: super::quit::QuitFlow,
    pub(super) quit_timer: Option<(TimerToken, u32)>,
    pub(super) release_exited: Option<TimerToken>,
    /// The spawn a restart starts from; its `open` is always empty.
    pub(super) editor_spawn: Option<EditorSpawn>,
    /// The editor's open arguments from the launch, until the editor has been ready once: a restart before that
    /// carries them again, because nothing has opened them yet.
    pub(super) pending_open: Vec<String>,
    /// The editor has reported ready and has not exited, been released or begun a restart since.
    pub(super) editor_ready: bool,
    /// Files accepted while the editor was not ready, oldest first; they open when it next is.
    pub(super) queued_opens: VecDeque<QueuedOpen>,
    /// No nvim can take a file and nothing brings one back as things stand: the first start failed, which has no
    /// restart, or nvim went out of reach, which holds until it is ended. A queued open would wait for good.
    pub(super) editor_lost: bool,
    pub(super) editor_run: EditorRun,
    pub(super) restart_wanted: bool,
    pub(super) release_refused_logged: bool,
    /// Allocations are heard from an idle; a close must hear their pin lengths before saving.
    pub(super) unheard_settles: VecDeque<crate::layout::Con>,
    /// Decided one at a time, after the previous batch's events and judgments have applied.
    pub(super) lua_pending: VecDeque<crate::lua::layout::LayoutRequest>,
    pub(super) spec: WindowSpec,
    pub(super) lifecycle: Lifecycle,
    pub(super) layout: Rc<RefCell<Layout>>,
    pub(super) workspaces: Rc<RefCell<Workspaces>>,
    pub(super) saver: Option<Saver>,
    /// The save's debounce timer, while one is armed.
    pub(super) save_timer: Option<TimerToken>,
    timers: Timers,
    /// The hosted modules the grid shows, as the last layout batch left them.
    pub(super) mapped: BTreeSet<ModuleId>,
    /// The floats the grid's float band holds, as its last restack put them.
    pub(super) float_band: BTreeSet<ModuleId>,
    /// The size of the window's last allocation; `None` before its first.
    pub(super) allocated: Option<Size>,
    /// What the window's last allocation arranged: where each divider and float is on screen now. A copy of
    /// what the grid was given, read only by the drags, which move a divider or a float from where it is drawn.
    pub(super) arranged: Option<Rc<Arrangement>>,
    /// The divider or float a press began dragging (a float's only by the primary button), until its release or
    /// the next press.
    pub(super) drag: Option<Drag>,
    /// The sending page supplies the keys' view for one tab verb only.
    pub(super) tab_view: Option<crate::tabs::ViewId>,
    /// Who holds the keys as the host reports it, and what follows from it.
    pub(super) focus: FocusState,
    /// The window's prefix and the timer of its digit wait.
    pub(super) keys: KeyState,
    /// What the module verbs wait for: the y/n's module, the new instances whose surface was asked for, the
    /// splits waiting for a terminal's directory.
    pub(super) modules: super::verbs::modules::ModuleVerbs,
    /// The links to the windows this one is grouped with and the moves on each, by peer.
    pub(super) peers: BTreeMap<PeerId, super::moves::PeerMoves>,
    /// Whether this window already showed the hint that the compositor arranges windows.
    pub(super) hint_shown: bool,
    /// The chat window that last held the keyboard, while it is attached (a window 1 in compositor mode).
    pub(super) last_chat: Option<PeerId>,
    /// The module windows started and not yet linked, each with the actions waiting for it: they are sent when its
    /// peer attaches and dropped, with a log line, when the window ends or is refused first.
    pub(super) pending_runs: BTreeMap<WindowName, Vec<PendingRun>>,
    /// What this window is, when it is a module window.
    pub(super) module_name: Option<WindowName>,
    /// The tab whose `tab.break` waits for its turn's end.
    pub(super) tab_break_wait: Option<crate::tabs::TabId>,
    /// Output facts shared by navigation and the linked window.
    pub(super) monitors: Vec<Monitor>,
    pub(super) monitor: Option<MonitorId>,
    /// The output this window is to be put back on, asked for at most once, when it exists.
    pub(super) monitor_memory: crate::window_link::edge::MonitorMemory,
    pub(super) placement_role: PlacementRole,
    pub(super) placement_focus_pending: bool,
    /// Window 2 is started again after this window's start: the idle armed for it, until it fires.
    pub(super) restore_second: bool,
    pub(super) restore_idle: Option<TimerToken>,
    /// The idle that opens the module windows a hub's start owes, once `shell:ready` has gone out.
    pub(super) start_windows_idle: Option<TimerToken>,
    /// The first-start screen while one is up.
    pub(super) onboarding: Option<super::onboarding::OnboardingState>,
    /// Compositor mode's first-start toast, until the screen is ruled out or has closed.
    pub(super) first_toast: Option<String>,
    /// How many times the window asked for the screen's check: the generation the next screen opens under. An
    /// answer carrying another number is for a screen that is gone.
    pub(super) onboarding_generation: u64,
    /// What the window's chrome shows, as the controller decided it.
    pub(super) chrome: ChromeModel,
    /// Bumped by every change of `chrome`, so the shell can tell an announced model from one it drew.
    pub(super) chrome_rev: u64,
    /// What the chrome's fields are derived from, and when each was last derived.
    pub(super) chrome_state: ChromeState,
    /// The window's text size; its verbs clone it, step the clone and store the result, since
    /// (2026-09-23) `TextSize` holds a `Vec` (`burst`) and so is no longer `Copy`.
    pub(super) text_size: Option<super::text_size::TextSize>,
    pub(super) zoom_pages: BTreeSet<ModuleId>,
    /// The running base terminal's reads out for plugin pages' `terminal.at`, oldest first.
    pub(super) pending_at: Vec<super::terminal_at::PendingAt>,
    /// A restart in place of the base terminal waiting for its new shell.
    pub(super) pending_restart: Option<super::terminal_at::PendingRestart>,
    /// The next `terminal.at` read's own number, which its answer carries back.
    pub(super) next_job_read: u64,
    #[cfg(test)]
    notes: Vec<String>,
}

impl Window {
    pub fn new(spec: WindowSpec, parts: WindowParts, timers: Timers) -> Window {
        Window {
            own_window: None,
            tiling: Tiling::default(),
            close: Default::default(),
            quit: Default::default(),
            quit_timer: None,
            release_exited: None,
            editor_spawn: None,
            pending_open: Vec::new(),
            editor_ready: false,
            queued_opens: VecDeque::new(),
            editor_lost: false,
            editor_run: EditorRun::Running,
            restart_wanted: false,
            release_refused_logged: false,
            unheard_settles: VecDeque::new(),
            lua_pending: VecDeque::new(),
            lifecycle: Lifecycle::default(),
            spec,
            layout: parts.layout,
            workspaces: parts.workspaces,
            saver: parts.saver,
            save_timer: None,
            timers,
            mapped: BTreeSet::new(),
            float_band: BTreeSet::new(),
            allocated: None,
            arranged: None,
            drag: None,
            tab_view: None,
            focus: FocusState::default(),
            keys: KeyState::default(),
            modules: Default::default(),
            peers: BTreeMap::new(),
            hint_shown: false,
            last_chat: None,
            pending_runs: BTreeMap::new(),
            module_name: None,
            tab_break_wait: None,
            monitors: Vec::new(),
            monitor: None,
            monitor_memory: crate::window_link::edge::MonitorMemory::new(None),
            placement_role: PlacementRole::Main,
            placement_focus_pending: false,
            restore_second: false,
            restore_idle: None,
            start_windows_idle: None,
            onboarding: None,
            first_toast: None,
            onboarding_generation: 0,
            chrome: ChromeModel::default(),
            chrome_rev: 0,
            chrome_state: ChromeState::default(),
            text_size: None,
            zoom_pages: BTreeSet::new(),
            pending_at: Vec::new(),
            pending_restart: None,
            next_job_read: 0,
            #[cfg(test)]
            notes: vec![],
        }
    }

    /// Prints a verb's line on stdout.
    pub(super) fn say(&mut self, line: String) {
        println!("{line}");
        #[cfg(test)]
        self.notes.push(line);
    }

    /// The link to `id` and its moves, while that peer is attached.
    pub fn peer(&self, id: PeerId) -> Option<&super::moves::PeerMoves> {
        self.peers.get(&id)
    }

    pub fn peer_mut(&mut self, id: PeerId) -> Option<&mut super::moves::PeerMoves> {
        self.peers.get_mut(&id)
    }

    /// The attached peer that is the window called `name`.
    pub fn peer_named(&self, name: &WindowName) -> Option<PeerId> {
        self.peers.iter().find(|(_, p)| p.name() == name).map(|(id, _)| *id)
    }

    /// Every attached peer, by id.
    pub fn peers(&self) -> impl Iterator<Item = (&PeerId, &super::moves::PeerMoves)> {
        self.peers.iter()
    }

    /// Whether any peer is attached.
    pub fn has_peer(&self) -> bool {
        !self.peers.is_empty()
    }

    /// The peer named `Second`, if it is attached: window 2 seen from window 1, and window 1 seen from window 2,
    /// which attaches its only peer under the same name. The name is the pair's role, not a window number.
    pub fn second_peer(&self) -> Option<PeerId> {
        self.peers
            .iter()
            .find(|(_, p)| *p.name() == WindowName::Second)
            .map(|(id, _)| *id)
    }

    /// The `Second` peer when it is the window's only peer: the one case where window 2 is a neighbour on the same
    /// desktop and the window may offer to move work into it.
    pub fn lone_second_peer(&self) -> Option<PeerId> {
        if self.peers.len() == 1 {
            self.second_peer()
        } else {
            None
        }
    }

    /// The peer that holds `id`, a module of this window's group that lives in its window.
    pub fn held_by(&self, id: &ModuleId) -> Option<PeerId> {
        self.peers
            .iter()
            .find(|(_, p)| p.held.contains(id))
            .map(|(peer, _)| *peer)
    }

    /// Every module any peer holds, once each.
    pub(super) fn held_union(&self) -> Vec<ModuleId> {
        let mut all: Vec<ModuleId> = self.peers.values().flat_map(|p| p.held.iter().cloned()).collect();
        all.sort();
        all.dedup();
        all
    }

    /// The counts of every peer's last summary added up, with the workspace names of window 2 alone, since a module
    /// window has none; `None` while no peer has sent a summary.
    pub fn group_summary(&self) -> Option<Summary> {
        let mut total: Option<Summary> = None;
        for p in self.peers.values() {
            let Some(s) = p.summary() else {
                continue;
            };
            let sum = total.get_or_insert_with(Summary::default);
            sum.waiting += s.waiting;
            sum.unseen += s.unseen;
            sum.running += s.running;
            sum.queued += s.queued;
            if *p.name() == WindowName::Second {
                sum.workspaces = s.workspaces.clone();
            }
        }
        total
    }

    /// Prints a refusal or a mistake on stderr.
    pub(super) fn warn(&mut self, line: String) {
        eprintln!("{line}");
        #[cfg(test)]
        self.notes.push(line);
    }

    /// The lines the window's verbs printed, for tests.
    #[cfg(test)]
    pub(super) fn notes(&self) -> &[String] {
        &self.notes
    }

    /// The hosted modules the layout puts on screen now, tiles in tree order then floats bottom first: the
    /// hosts the grid makes visible. A module only declared, retired or away has no host here.
    fn hosted_visible(&self, layout: &Layout) -> Vec<ModuleId> {
        layout
            .visible_modules()
            .into_iter()
            .filter(|id| {
                self.lifecycle.state(id).is_some_and(|state| {
                    !matches!(state, ModuleState::Declared | ModuleState::Retired | ModuleState::Away)
                })
            })
            .collect()
    }

    /// The layout ops of a change, in the order that keeps the keys off a host leaving the screen: each hosted
    /// module the layout shows now and the grid does not is mapped (tree order), then `focus` gives the keys
    /// with its grab's tail, then what the grid shows and the layout no longer does is unmapped, the float band
    /// is restacked when its floats changed, and the grid is allocated. Both records follow what the ops do.
    pub fn layout_ops(&mut self, win: WinId, focus: Option<(ModuleId, Vec<HostOp>)>, batch: &mut Batch) {
        let layout = self.layout.clone();
        let Ok(layout) = layout.try_borrow() else {
            self.warn("[wm] BUG: the layout was borrowed when its change was laid out; nothing moved".into());
            return;
        };
        let visible = self.hosted_visible(&layout);
        for module in visible.iter().filter(|m| !self.mapped.contains(*m)) {
            batch.push_layout(HostOp::Map {
                win,
                module: module.clone(),
            });
        }
        if let Some((module, on_grab)) = focus {
            self.focus.ask(&module, batch);
            batch.push_layout(HostOp::Focus {
                win,
                to: FocusTo::Module(module),
                on_grab,
            });
        }
        for module in self.mapped.iter().filter(|m| !visible.contains(*m)) {
            batch.push_layout(HostOp::Unmap {
                win,
                module: module.clone(),
            });
        }
        let floats = shown_floats(&layout);
        if floats_need_restack(&self.float_band, &floats) {
            batch.push_layout(HostOp::Restack {
                win,
                moves: restack_order(&floats),
            });
            self.float_band = floats.into_iter().collect();
        }
        batch.push_layout(HostOp::Allocate { win });
        self.mapped = visible.into_iter().collect();
        self.layout_touched();
    }

    /// A change the controller made to the layout: its layout ops, then as effects the modules' lifecycle
    /// effects, the chrome derived again at the batch's end, and the save's debounce armed when the arrangement
    /// differs from what was last written.
    pub fn changed_by_controller(&mut self, win: WinId, batch: &mut Batch) {
        self.changed_with_focus(win, None, batch);
    }

    /// [`Window::changed_by_controller`] for a change that also gives the keys: `focus` gets them, with its
    /// grab's tail, between the maps and the unmaps. The chrome is derived once however many changes the batch
    /// holds, since it reads the layout as the whole batch leaves it.
    pub fn changed_with_focus(&mut self, win: WinId, focus: Option<(ModuleId, Vec<HostOp>)>, batch: &mut Batch) {
        self.layout_ops(win, focus, batch);
        let layout = self.layout.clone();
        match layout.try_borrow() {
            Ok(layout) => self.lifecycle.after_change(win, &layout, batch),
            Err(_) => self.warn("[wm] BUG: the layout was borrowed after a change; no lifecycle effect".into()),
        }
        self.layout_touched();
        self.saver_changed();
    }

    /// The layout may have changed in a way the chrome shows (the selection, the mark, the tray, the strips): its
    /// fields that read the layout are derived again once, at the end of the batch, however many changes the
    /// batch holds, since they read the layout as the whole batch leaves it.
    pub(super) fn layout_touched(&mut self) {
        self.chrome_state.dirty.layout += 1;
    }

    /// A refusal in `batch`: the app name flashes and the toast gives the reason (an empty `text` flashes alone),
    /// both in the chrome model, which the batch announces at its end. Nothing else goes into the batch for it.
    pub(super) fn refuse(&mut self, _win: WinId, text: String, _batch: &mut Batch) {
        super::chrome::refused(self, &text);
    }

    /// The float band put in the layout's z-order now, as one restack.
    pub(super) fn restack(&mut self, win: WinId, batch: &mut Batch) {
        let Ok(floats) = self.layout.try_borrow().map(|layout| shown_floats(&layout)) else {
            self.warn("[wm] BUG: the layout was borrowed when the floats were restacked".into());
            return;
        };
        batch.push_layout(HostOp::Restack {
            win,
            moves: restack_order(&floats),
        });
        self.float_band = floats.into_iter().collect();
        // A float raised over another changes what each covers and the order of their bars.
        self.layout_touched();
    }

    /// Takes the window's start placement: the output it is put back on, and whether it starts window 2 again.
    pub(super) fn set_placement(&mut self, placement: WindowPlacement) {
        self.monitor_memory = crate::window_link::edge::MonitorMemory::new(placement.remembered);
        self.restore_second = placement.restore_second;
        self.placement_role = placement.role;
        self.placement_focus_pending = matches!(
            placement.role,
            PlacementRole::RestoredSecond { .. } | PlacementRole::NewModule
        );
    }

    /// Asks the window's remembered output whether it is to move there now: only once it is on an output (so it is
    /// shown and the window manager can move it), and at most once, when the remembered output exists and the window
    /// is not already on it. A restored window also returns the keys after its first mapped output, even without
    /// a move, because the compositor can focus a newly mapped window by itself.
    pub(super) fn put_back_on_its_monitor(&mut self, win: WinId, batch: &mut Batch) {
        let Some(current) = self.monitor.clone() else {
            return;
        };
        let available: Vec<String> = self.monitors.iter().map(|m| m.id.0.clone()).collect();
        let monitor = self.monitor_memory.decide(Some(&current.0), &available).map(MonitorId);
        if let Some(monitor) = &monitor {
            self.say(format!("[window] moving to {}, the monitor it was last on", monitor.0));
        }
        // Mapping can give a restored window the keys even when no output move is needed.
        if monitor.is_some() || self.placement_focus_pending {
            self.placement_focus_pending = false;
            batch.push_effect(HostOp::PlaceWindow {
                win,
                monitor,
                role: self.placement_role,
            });
        }
        // Only the first mapped placement decision may focus; an output appearing later must not take the keys.
        self.placement_role = PlacementRole::Main;
    }

    /// Arms an idle below painting on the window's scheduler, with a token no other timer of the process has.
    pub(super) fn arm_idle(&mut self) -> TimerToken {
        let token = self.timers.token();
        self.timers.scheduler.idle(token);
        token
    }

    /// Arms a timer after `delay` on the window's scheduler, with a token no other timer of the process has.
    pub(super) fn arm_after(&mut self, delay: Duration) -> TimerToken {
        let token = self.timers.token();
        self.timers.scheduler.after(delay, token);
        token
    }

    /// Cancels a timer this window armed.
    pub(super) fn cancel_timer(&self, token: TimerToken) {
        self.timers.scheduler.cancel(token);
    }

    /// The scheduler's clock.
    pub(super) fn now(&self) -> Instant {
        self.timers.scheduler.now()
    }

    /// The prefix's part of the chrome changed: its fields that read the prefix are derived again, and the batch
    /// announces the model once at its end.
    pub(super) fn prefix_changed(&mut self) {
        self.chrome_state.dirty.prefix += 1;
        super::chrome::touched(self);
    }

    /// The chrome changed: its revision bumped and announced with the new revision, once per batch.
    pub(super) fn chrome_changed(&mut self, win: WinId, batch: &mut Batch) {
        self.chrome_rev += 1;
        batch.push_effect(HostOp::Chrome {
            win,
            rev: self.chrome_rev,
        });
    }

    /// A test's own write of the layout or the workspaces, taken as already drawn: the modules' lifecycle effects
    /// follow it, the grid is taken to show what the layout shows (the test asks for no layout op), the chrome is
    /// derived again from it all, and the saver says whether the save's debounce is to be armed. What a test sets up
    /// by hand behind the controller reaches it this way; nothing outside a test writes the layout.
    #[cfg(test)]
    pub(super) fn written_in_test(&mut self, win: WinId, batch: &mut Batch) {
        let layout = self.layout.clone();
        let Ok(layout) = layout.try_borrow() else {
            self.warn("[wm] BUG: the layout was borrowed when a test's write was heard".into());
            return;
        };
        self.lifecycle.after_change(win, &layout, batch);
        self.layout_touched();
        self.chrome_state.dirty.workspaces += 1;
        self.mapped = self.hosted_visible(&layout).into_iter().collect();
        self.float_band = shown_floats(&layout).into_iter().collect();
        drop(layout);
        self.saver_changed();
    }

    /// Puts the selection back on the module with the keys; `true` if a container was selected. A layout
    /// borrowed right now leaves the selection as it is.
    pub(super) fn clear_selection(&mut self) -> bool {
        self.layout
            .try_borrow_mut()
            .is_ok_and(|mut layout| layout.clear_selection())
    }

    /// The layout or the workspaces changed: the saver says whether the save's debounce is to be armed.
    pub(super) fn saver_changed(&mut self) {
        let now = self.timers.scheduler.now();
        let Some(saver) = self.saver.as_mut() else {
            return;
        };
        saver.collect(self.workspaces.try_borrow_mut().ok().as_deref_mut());
        let arm = match (self.layout.try_borrow(), self.workspaces.try_borrow()) {
            (Ok(layout), Ok(workspaces)) => saver.changed(now, &layout, &workspaces),
            _ => {
                saver.changed_unread(now);
                None
            }
        };
        if let Some(delay) = arm {
            self.arm_save(delay);
        }
    }

    /// The save's debounce fired: armed again for what is left, or written.
    pub(super) fn save_timer_fired(&mut self) {
        self.save_timer = None;
        let now = self.timers.scheduler.now();
        let Some(saver) = self.saver.as_mut() else {
            return;
        };
        saver.collect(self.workspaces.try_borrow_mut().ok().as_deref_mut());
        let rearm = match (self.layout.try_borrow(), self.workspaces.try_borrow()) {
            (Ok(layout), Ok(workspaces)) => match saver.timer_fired(now, &layout, &workspaces) {
                Fired::Rearm(left) => Some(left),
                Fired::Saved | Fired::NothingToSave => None,
            },
            _ => saver.timer_fired_unread(now),
        };
        if let Some(left) = rearm {
            self.arm_save(left);
        }
    }

    fn arm_save(&mut self, delay: Duration) {
        let token = self.timers.token();
        if let Some(old) = self.save_timer.replace(token) {
            self.timers.scheduler.cancel(old);
        }
        self.timers.scheduler.after(delay, token);
    }

    /// Read-only handles on the window's layout and workspaces.
    pub fn views(&self) -> (LayoutView, WorkspacesView) {
        (LayoutView(self.layout.clone()), WorkspacesView(self.workspaces.clone()))
    }

    pub fn role(&self) -> &WindowRole {
        &self.spec.role
    }

    pub fn presence(&self) -> &EditorPresence {
        &self.spec.presence
    }

    /// Keeps `spawn` as what a restart starts from, and its open arguments apart as the ones still to be opened.
    pub(super) fn keep_spawn(&mut self, mut spawn: EditorSpawn) {
        self.pending_open = std::mem::take(&mut spawn.open);
        self.editor_spawn = Some(spawn);
    }

    /// How the shell described the window when it opened it.
    pub fn spec(&self) -> &WindowSpec {
        &self.spec
    }
}

/// The float band's order as restack moves: each shown float after the one below it, the lowest first. Floats
/// only: the editor never floats, and a move naming it would reorder the editor's widget among the grid's.
fn restack_order(floats: &[ModuleId]) -> Vec<RestackMove> {
    let mut after = None;
    floats
        .iter()
        .map(|module| {
            debug_assert!(*module != ModuleId::editor(), "the editor never floats");
            let step = RestackMove {
                module: module.clone(),
                after: after.clone(),
            };
            after = Some(module.clone());
            step
        })
        .collect()
}

/// The independently controlled window presentation flags.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct WindowMode {
    /// Whether surrounding chrome is suppressed.
    pub immersive: bool,
    /// Whether the window fills its output.
    pub fullscreen: bool,
    /// Whether the top bar is temporarily exposed.
    pub top_bar_revealed: bool,
}

/// An opaque output identity supplied by the host.
#[derive(Debug, Clone, PartialEq)]
pub struct MonitorId(pub String);

/// One output in desktop layout coordinates.
#[derive(Debug, Clone, PartialEq)]
pub struct Monitor {
    /// The stable output identity.
    pub id: MonitorId,
    /// The desktop rectangle.
    pub rect: LayoutRect,
    /// The output scale.
    pub scale: f64,
    /// The displayed output name.
    pub name: String,
}

/// Which window receives the keys after startup placement.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PlacementRole {
    /// An ordinary placement adds no focus beyond the compositor's move.
    #[default]
    Main,
    /// A window opened explicitly receives the keys after moving.
    NewSecond,
    /// A chat or page window opened by a key or a hub ask receives the keys once it is mapped, wherever the
    /// compositor put it: it is never moved to an output.
    NewModule,
    /// A window reopened at launch returns the keys to window 1 after mapping or moving.
    RestoredSecond { first_pid: u32 },
}

/// Where a window goes at its start, from what the pair file kept of the last run: the output it was on, and,
/// for the window that holds the pair, whether it starts the second window again.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WindowPlacement {
    /// Whether placement keeps the keys here or returns them to window 1.
    pub role: PlacementRole,
    /// The connector of the output this window was last on, if the pair file names one.
    pub remembered: Option<String>,
    /// Whether this window starts window 2 again once its own start is done.
    pub restore_second: bool,
}

/// The current content area and scale of a window.
#[derive(Debug, Clone, PartialEq)]
pub struct FrameMetrics {
    /// The available content size.
    pub size: Size,
    /// The native scale.
    pub scale: f64,
}

/// A file waiting to open in the editor. `line` and `col` count from one, `col` in bytes.
#[derive(Debug, Clone, PartialEq)]
pub struct QueuedOpen {
    pub path: std::path::PathBuf,
    pub line: Option<u32>,
    pub col: Option<u32>,
}

/// An opaque native token authorizing presentation.
#[derive(Debug, Clone, PartialEq)]
pub struct ActivationToken(pub String);
