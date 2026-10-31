//! Deterministic host services for testing decisions without a native window system.

use super::{host::*, window::*};
use crate::keymap::action::Action;
use crate::layout::{Direction, ModuleId, Size};
use crate::tabs::{TabId, ViewId};
use crate::window_link::moves::MoveId;
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::os::fd::RawFd;
use std::rc::{Rc, Weak};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use std::time::{Duration, Instant};

type ApplyHook = Rc<dyn Fn(&FakeHost, usize, &HostOp)>;
type RefuseKind = Rc<dyn Fn(&SurfaceKind) -> bool>;
type RefuseOp = Rc<dyn Fn(&HostOp) -> bool>;

#[cfg(test)]
pub(crate) fn lua(driver: &super::Driver, win: WinId, request: crate::lua::layout::LayoutRequest) {
    driver.post(HostEvent::Lua(LuaEvent::LayoutRequests {
        win,
        requests: vec![request],
    }));
}

#[derive(Debug, Clone, PartialEq)]
pub enum Logged {
    Op(HostOp, Applied),
    Event(HostEvent),
    /// A read whose moment matters, among the ops and events: `plugin_keys_help(<module>)`.
    Read(String),
}

struct Surface {
    kind: SurfaceKind,
    mapped: bool,
}

/// A terminal pane's shell as the fake host runs it: which shell it is, the directory it is in and whether
/// anything else runs in its session. What a read reports and what a restart in place checks.
#[derive(Debug, Clone, PartialEq)]
pub struct FakeShell {
    pub shell: ShellStart,
    pub cwd: Result<std::path::PathBuf, String>,
    pub busy: bool,
}

pub struct FakeHost {
    sink: RefCell<Option<Weak<dyn HostSink>>>,
    model: Cell<bool>,
    surfaces: RefCell<BTreeMap<(WinId, ModuleId), Surface>>,
    focus: RefCell<BTreeMap<WinId, FocusOwner>>,
    closed: RefCell<BTreeSet<WinId>>,
    log: RefCell<Vec<Logged>>,
    apply_hook: RefCell<Option<ApplyHook>>,
    apply_index: Cell<usize>,
    refuse_kind: RefCell<Option<RefuseKind>>,
    refuse_op: RefCell<Option<RefuseOp>>,
    reads: RefCell<Reads>,
    /// Each window's module hosts as its grid would stack them, bottom first: in the order they were made,
    /// until a restack moves its floats to the top.
    stacking: RefCell<BTreeMap<WinId, Vec<ModuleId>>>,
    /// How many allocations each window's grid was asked for.
    allocations: RefCell<BTreeMap<WinId, usize>>,
    font_sizes: RefCell<BTreeMap<WinId, f32>>,
    /// The terminals whose shells the fake runs: only these answer a job read or a restart in place by themselves.
    shells: RefCell<BTreeMap<(WinId, ModuleId), FakeShell>>,
    page_zooms: RefCell<BTreeMap<(WinId, ModuleId), f32>>,
    terminal_zooms: RefCell<BTreeMap<WinId, f32>>,
}

/// What the fake answers as [`HostReads`]: [`NoReads`]' values until a test sets one.
struct Reads {
    grid_size: Size,
    editor_cell: Option<(f64, f64)>,
    editor_facts: EditorFacts,
    terminal_jobs: usize,
    running_chats: Vec<(ModuleId, Option<TabId>)>,
    terminal_job_modules: Vec<ModuleId>,
    pages_with_tasks: Vec<ModuleId>,
    base_tabs: Vec<TabId>,
    active_base_tab: Option<TabId>,
    tab_moves: BTreeMap<TabId, Result<ChatMoveFacts, String>>,
    arrived_tabs: BTreeMap<MoveId, TabId>,
    moved_tab_keys: BTreeMap<TabId, crate::window_link::base_tabs::MovedTabKey>,
    chat_claims: Vec<(ViewId, Direction)>,
    module_min: BTreeMap<ModuleId, Size>,
    attention: BTreeMap<ViewId, crate::attention::Attention>,
    oldest_card_view: Option<ViewId>,
    kill_counts: BTreeMap<ViewId, ChatKillCounts>,
    chat_summary: ChatSummary,
    first_tab_refused: Option<String>,
    module_size: BTreeMap<ModuleId, Size>,
    tab_steps: BTreeMap<i32, bool>,
    tab_last: bool,
    tab_places: BTreeMap<u16, crate::tab_set::TabPlace>,
    page_built: BTreeSet<ModuleId>,
    plugin_help: BTreeMap<ModuleId, crate::plugin::keys::PluginKeysHelp>,
    chat_views: Vec<ViewId>,
    break_out: Result<(), String>,
    chat_moves: BTreeMap<ViewId, Result<ChatMoveFacts, String>>,
    sessions: BTreeMap<String, ViewId>,
}

impl Default for Reads {
    fn default() -> Self {
        Reads {
            grid_size: super::grid::UNALLOCATED,
            editor_cell: None,
            editor_facts: EditorFacts::default(),
            terminal_jobs: 0,
            running_chats: Vec::new(),
            terminal_job_modules: Vec::new(),
            pages_with_tasks: Vec::new(),
            base_tabs: Vec::new(),
            active_base_tab: None,
            tab_moves: BTreeMap::new(),
            arrived_tabs: BTreeMap::new(),
            moved_tab_keys: BTreeMap::new(),
            chat_claims: Vec::new(),
            module_min: BTreeMap::new(),
            attention: BTreeMap::new(),
            oldest_card_view: None,
            kill_counts: BTreeMap::new(),
            chat_summary: ChatSummary::default(),
            first_tab_refused: None,
            module_size: BTreeMap::new(),
            tab_steps: BTreeMap::new(),
            tab_last: false,
            tab_places: BTreeMap::new(),
            page_built: BTreeSet::new(),
            plugin_help: BTreeMap::new(),
            chat_views: Vec::new(),
            break_out: Err(NOT_CARRIED_OUT.into()),
            chat_moves: BTreeMap::new(),
            sessions: BTreeMap::new(),
        }
    }
}

impl FakeHost {
    pub fn set_tab_can_step(&self, delta: i32, answer: bool) {
        self.reads.borrow_mut().tab_steps.insert(delta, answer);
    }
    pub fn set_tab_has_last(&self, answer: bool) {
        self.reads.borrow_mut().tab_last = answer;
    }
    pub fn set_tab_place(&self, n: u16, place: Option<crate::tab_set::TabPlace>) {
        let mut reads = self.reads.borrow_mut();
        if let Some(place) = place {
            reads.tab_places.insert(n, place);
        } else {
            reads.tab_places.remove(&n);
        }
    }
    /// The `?` sheet's section the plugin page `module` declares from now on; `None`, it declares none.
    pub fn set_plugin_keys_help(&self, module: ModuleId, help: Option<crate::plugin::keys::PluginKeysHelp>) {
        let mut reads = self.reads.borrow_mut();
        match help {
            Some(help) => reads.plugin_help.insert(module, help),
            None => reads.plugin_help.remove(&module),
        };
    }
    /// The chat views holding a tab from now on, as the panel would list them.
    pub fn set_chat_views(&self, views: Vec<ViewId>) {
        self.reads.borrow_mut().chat_views = views;
    }
    /// Whether the base view's tab can break out from now on, as the panel would answer.
    pub fn set_can_break_out(&self, answer: Result<(), String>) {
        self.reads.borrow_mut().break_out = answer;
    }
    /// Whether the plugin page `module` has been built from now on, as the window's plugin host would answer.
    pub fn set_page_built(&self, module: ModuleId, built: bool) {
        if built {
            self.reads.borrow_mut().page_built.insert(module);
        } else {
            self.reads.borrow_mut().page_built.remove(&module);
        }
    }
    /// What a move of view `view`'s tab would take from now on, as the panel would answer it.
    pub fn set_chat_move(&self, view: ViewId, answer: Result<ChatMoveFacts, String>) {
        self.reads.borrow_mut().chat_moves.insert(view, answer);
    }
    /// The view holding session `session` from now on.
    pub fn set_view_of_session(&self, session: &str, view: ViewId) {
        self.reads.borrow_mut().sessions.insert(session.to_owned(), view);
    }

    pub fn new() -> Rc<Self> {
        Rc::new(Self {
            sink: RefCell::new(None),
            model: Cell::new(true),
            surfaces: RefCell::new(BTreeMap::new()),
            focus: RefCell::new(BTreeMap::new()),
            closed: RefCell::new(BTreeSet::new()),
            log: RefCell::new(vec![]),
            apply_hook: RefCell::new(None),
            apply_index: Cell::new(0),
            refuse_kind: RefCell::new(None),
            refuse_op: RefCell::new(None),
            reads: RefCell::new(Reads::default()),
            stacking: RefCell::new(BTreeMap::new()),
            allocations: RefCell::new(BTreeMap::new()),
            font_sizes: RefCell::new(BTreeMap::new()),
            shells: RefCell::new(BTreeMap::new()),
            page_zooms: RefCell::new(BTreeMap::new()),
            terminal_zooms: RefCell::new(BTreeMap::new()),
        })
    }
    pub fn font_size(&self, win: WinId) -> Option<f32> {
        self.font_sizes.borrow().get(&win).copied()
    }
    pub fn page_zoom(&self, win: WinId, module: &ModuleId) -> Option<f32> {
        self.page_zooms.borrow().get(&(win, module.clone())).copied()
    }
    pub fn terminal_zoom(&self, win: WinId) -> Option<f32> {
        self.terminal_zooms.borrow().get(&win).copied()
    }
    pub fn set_grid_size(&self, size: Size) {
        self.reads.borrow_mut().grid_size = size;
    }
    pub fn set_editor_cell(&self, cell: Option<(f64, f64)>) {
        self.reads.borrow_mut().editor_cell = cell;
    }
    pub fn set_editor_running(&self, running: bool) {
        let mut reads = self.reads.borrow_mut();
        reads.editor_facts.running = running;
    }
    pub fn set_running_chats(&self, chats: Vec<(ModuleId, Option<TabId>)>) {
        self.reads.borrow_mut().running_chats = chats;
    }
    pub fn set_terminal_job_modules(&self, modules: Vec<ModuleId>) {
        self.reads.borrow_mut().terminal_job_modules = modules;
    }
    pub fn set_pages_with_tasks(&self, modules: Vec<ModuleId>) {
        self.reads.borrow_mut().pages_with_tasks = modules;
    }
    pub fn set_base_tabs(&self, tabs: Vec<ChatMoveFacts>, active: Option<TabId>) {
        let mut reads = self.reads.borrow_mut();
        reads.base_tabs = tabs.iter().map(|t| t.tab).collect();
        reads.active_base_tab = active;
        reads.tab_moves = tabs.into_iter().map(|t| (t.tab, Ok(t))).collect();
    }
    pub fn set_terminal_jobs(&self, jobs: usize) {
        self.reads.borrow_mut().terminal_jobs = jobs;
    }
    pub fn set_editor_facts(&self, facts: EditorFacts) {
        let mut reads = self.reads.borrow_mut();
        reads.editor_facts = facts;
    }
    pub fn set_chat_claims(&self, view: ViewId, direction: Direction, claims: bool) {
        let mut reads = self.reads.borrow_mut();
        reads.chat_claims.retain(|claim| *claim != (view, direction));
        if claims {
            reads.chat_claims.push((view, direction));
        }
    }
    pub fn set_module_min(&self, module: ModuleId, min: Size) {
        self.reads.borrow_mut().module_min.insert(module, min);
    }
    /// What chat view `view` holds for the user from now on, as its panel would report it.
    pub fn set_attention(&self, view: ViewId, attention: crate::attention::Attention) {
        self.reads.borrow_mut().attention.insert(view, attention);
    }
    /// The chat view holding the oldest waiting card from now on, as the panel would report it.
    pub fn set_oldest_card_view(&self, view: Option<ViewId>) {
        self.reads.borrow_mut().oldest_card_view = view;
    }
    /// The window's chat counts from now on (waiting, unseen, running, queued), as the panel would sum them for the
    /// summary the other window is sent.
    pub fn set_chat_summary(&self, counts: ChatSummary) {
        self.reads.borrow_mut().chat_summary = counts;
    }
    /// What killing chat view `view` would end from now on, as the panel would count it.
    pub fn set_chat_kill_counts(&self, view: ViewId, counts: ChatKillCounts) {
        self.reads.borrow_mut().kill_counts.insert(view, counts);
    }
    /// Why no new chat view can be given its first tab from now on, as the panel would refuse it; `None`, it can.
    pub fn set_chat_first_tab_refused(&self, why: Option<String>) {
        self.reads.borrow_mut().first_tab_refused = why;
    }
    pub fn attach(&self, sink: Weak<dyn HostSink>) {
        *self.sink.borrow_mut() = Some(sink);
    }
    pub fn with_surface(&self, win: WinId, module: ModuleId, kind: SurfaceKind, mapped: bool) {
        let mut stacking = self.stacking.borrow_mut();
        let hosts = stacking.entry(win).or_default();
        if !hosts.contains(&module) {
            hosts.push(module.clone());
        }
        self.surfaces
            .borrow_mut()
            .insert((win, module), Surface { kind, mapped });
    }
    pub fn set_model(&self, on: bool) {
        self.model.set(on);
    }
    /// The shell `module`'s pane runs from now on (`None`: the fake answers nothing for it, and a test posts the
    /// answers itself). A terminal with one answers [`TermOp::ReadJob`] with what it holds, and carries out
    /// [`TermOp::Respawn`] only while it runs the shell the op expects, given nothing since: a new shell under the
    /// next start, in the op's directory and idle, reported as [`TermEvent::Started`]; otherwise refused.
    pub fn set_shell(&self, win: WinId, module: ModuleId, shell: Option<FakeShell>) {
        let mut shells = self.shells.borrow_mut();
        match shell {
            Some(shell) => shells.insert((win, module), shell),
            None => shells.remove(&(win, module)),
        };
    }
    /// The shell `module`'s pane runs now, as [`FakeHost::set_shell`] or a restart left it.
    pub fn shell(&self, win: WinId, module: &ModuleId) -> Option<FakeShell> {
        self.shells.borrow().get(&(win, module.clone())).cloned()
    }
    pub fn on_apply(&self, f: impl Fn(&FakeHost, usize, &HostOp) + 'static) {
        *self.apply_hook.borrow_mut() = Some(Rc::new(f));
        self.apply_index.set(0);
    }
    pub fn refuse_kind(&self, f: impl Fn(&SurfaceKind) -> bool + 'static) {
        *self.refuse_kind.borrow_mut() = Some(Rc::new(f));
    }
    /// Refuses, from now on, every op `f` picks, as a native host refuses what it cannot carry out.
    pub fn refuse_op(&self, f: impl Fn(&HostOp) -> bool + 'static) {
        *self.refuse_op.borrow_mut() = Some(Rc::new(f));
    }
    /// The size `module`'s host has on screen from now on.
    pub fn set_module_size(&self, module: ModuleId, size: Size) {
        self.reads.borrow_mut().module_size.insert(module, size);
    }
    pub fn log(&self) -> Vec<Logged> {
        self.log.borrow().clone()
    }
    pub fn focus_owner(&self, win: WinId) -> Option<ModuleId> {
        match self.focus.borrow().get(&win) {
            Some(FocusOwner::Module(m)) => Some(m.clone()),
            _ => None,
        }
    }
    pub fn is_closed(&self, win: WinId) -> bool {
        self.closed.borrow().contains(&win)
    }
    /// A primary press on `on`, as the grid's gesture posts the begin of a drag there.
    pub fn press_on(&self, win: WinId, on: PointerTarget) {
        self.post(HostEvent::Pointer {
            win,
            on,
            ev: PointerEv::Press {
                x: 0.0,
                y: 0.0,
                button: 1,
            },
        });
    }
    /// One step of the drag begun on `on`: the pointer `(dx, dy)` from where it was grabbed, in the dragged
    /// widget's coordinates as it is drawn now.
    pub fn drag_by(&self, win: WinId, on: PointerTarget, dx: f64, dy: f64) {
        self.post(HostEvent::Pointer {
            win,
            on,
            ev: PointerEv::Motion { x: dx, y: dy },
        });
    }
    /// The end of the drag begun on `on`.
    pub fn release_on(&self, win: WinId, on: PointerTarget) {
        self.post(HostEvent::Pointer {
            win,
            on,
            ev: PointerEv::Release { x: 0.0, y: 0.0 },
        });
    }
    pub fn post(&self, event: HostEvent) {
        let sink = attached(&self.sink, &event);
        self.log.borrow_mut().push(Logged::Event(event.clone()));
        sink.post(event);
    }
    fn native_focus(&self, win: WinId, owner: Option<FocusOwner>) {
        let chain = match &owner {
            Some(FocusOwner::Module(m)) => vec![m.clone()],
            _ => vec![],
        };
        let left_the_entry = {
            let mut focus = self.focus.borrow_mut();
            let before = focus.remove(&win);
            if let Some(owner) = &owner {
                focus.insert(win, owner.clone());
            }
            before == Some(FocusOwner::RenameEntry) && owner != Some(FocusOwner::RenameEntry)
        };
        self.post(HostEvent::NativeFocus {
            win,
            owner,
            chain: FocusChain(chain),
            active: true,
            op_index: None,
        });
        // A native rename entry hears the keys leave it whatever moved them, and says so as it would.
        if left_the_entry {
            self.post(HostEvent::TopBarEntry {
                win,
                ev: EntryEv::LostKeys,
            });
        }
    }
    fn refuse(&self, op: &HostOp) -> Applied {
        self.post(HostEvent::Refused {
            what: OpTag::of(op),
            why: "the native model cannot apply this operation".into(),
            surface: op.surface(),
        });
        Applied::Refused
    }
    fn model_apply(&self, op: &HostOp) -> Applied {
        let refuse = self.refuse_op.borrow().clone();
        if refuse.is_some_and(|f| f(op)) {
            return self.refuse(op);
        }
        match op {
            HostOp::Chat(ChatOp::Forget {
                view, tab: Some(tab), ..
            }) if view.is_base() => {
                let mut reads = self.reads.borrow_mut();
                reads.base_tabs.retain(|id| id != tab);
                reads.tab_moves.remove(tab);
                if reads.base_tabs.is_empty() {
                    reads.base_tabs.push(TabId(1));
                }
                if reads.active_base_tab == Some(*tab) {
                    reads.active_base_tab = reads.base_tabs.first().copied();
                }
            }
            HostOp::OrderBaseTabs { tabs, active, .. } => {
                let mut reads = self.reads.borrow_mut();
                reads.base_tabs = tabs.clone();
                if active.is_some() {
                    reads.active_base_tab = *active;
                }
            }
            HostOp::AdoptMoved {
                record,
                kind: super::moves::MoveKind::Chat,
                ..
            } => {
                if let Some(mv) = record["workspace_move_id"].as_u64() {
                    let mut reads = self.reads.borrow_mut();
                    let tab = TabId(1000 + mv);
                    if reads.base_tabs == [TabId(1)]
                        && reads
                            .tab_moves
                            .get(&TabId(1))
                            .is_none_or(|f| matches!(f, Ok(f) if f.session.is_none() && !f.running))
                    {
                        reads.base_tabs.clear();
                    }
                    reads.base_tabs.push(tab);
                    reads.tab_moves.insert(
                        tab,
                        Ok(ChatMoveFacts {
                            tab,
                            session: None,
                            live: true,
                            running: false,
                        }),
                    );
                    reads.arrived_tabs.insert(MoveId(mv), tab);
                }
            }

            HostOp::Terminal(TermOp::ReadJob { win, module, read }) => {
                let shell = self.shell(*win, module);
                if let Some(shell) = shell {
                    self.post(HostEvent::Terminal(TermEvent::Job {
                        win: *win,
                        module: module.clone(),
                        read: *read,
                        shell: shell.shell,
                        cwd: shell.cwd,
                        busy: shell.busy,
                    }));
                }
            }
            HostOp::Terminal(TermOp::Respawn {
                win,
                module,
                dir,
                expect,
            }) => {
                let shell = self.shell(*win, module);
                if let Some(shell) = shell {
                    if !expect.untouched(Some(shell.shell)) {
                        return self.refuse(op);
                    }
                    let pid = shell.shell.pid.map_or(1, |pid| pid + 1);
                    let fresh = FakeShell {
                        shell: ShellStart::of(shell.shell.generation + 1, Some(pid)),
                        cwd: Ok(dir.clone()),
                        busy: false,
                    };
                    self.set_shell(*win, module.clone(), Some(fresh));
                    self.post(HostEvent::Terminal(TermEvent::Started {
                        win: *win,
                        module: module.clone(),
                        pid,
                    }));
                }
            }
            HostOp::Chat(ChatOp::SetFontSize { win, px }) => {
                self.font_sizes.borrow_mut().insert(*win, *px);
            }
            HostOp::Plugin(PluginOp::Build { module, .. }) => {
                self.set_page_built(module.clone(), true);
            }
            HostOp::Plugin(PluginOp::SetZoom { win, module, level }) => {
                self.page_zooms.borrow_mut().insert((*win, module.clone()), *level);
            }
            HostOp::Terminal(TermOp::SetZoom { win, level }) => {
                self.terminal_zooms.borrow_mut().insert(*win, *level);
            }
            HostOp::Focus { win, to, .. } => {
                let owner = match to {
                    FocusTo::Module(module) => {
                        if !self
                            .surfaces
                            .borrow()
                            .get(&(*win, module.clone()))
                            .is_some_and(|s| s.mapped)
                        {
                            return self.refuse(op);
                        }
                        Some(FocusOwner::Module(module.clone()))
                    }
                    FocusTo::TopBarItem(_) => Some(FocusOwner::TopBar),
                    FocusTo::RenameEntry => Some(FocusOwner::RenameEntry),
                    // The host reports no owner for a screen that is not a module, as the toolkit's does.
                    FocusTo::Onboarding | FocusTo::Nothing => None,
                };
                self.native_focus(*win, owner);
            }
            HostOp::CreateSurface { win, module, kind } => {
                let refuse = self.refuse_kind.borrow().clone();
                if refuse.is_some_and(|f| f(kind)) {
                    return self.refuse(op);
                }
                self.with_surface(*win, module.clone(), kind.clone(), false);
            }
            // The arriving module's place, made off screen as a host of its kind.
            HostOp::PrepareSurface { win, module, .. } => {
                if !self.surfaces.borrow().contains_key(&(*win, module.clone())) {
                    let kind = match module.kind() {
                        crate::layout::ModuleKind::Agent => SurfaceKind::Chat {
                            view: ViewId::of_module(module).unwrap_or(ViewId::BASE),
                        },
                        crate::layout::ModuleKind::LuaWebview => SurfaceKind::PluginPage {
                            plugin: module.as_str().trim_start_matches("lua:").to_owned(),
                        },
                        _ => SurfaceKind::Terminal { cwd: ".".into() },
                    };
                    self.with_surface(*win, module.clone(), kind, false);
                }
            }
            HostOp::Map { win, module } => {
                let kind = {
                    let mut surfaces = self.surfaces.borrow_mut();
                    surfaces.get_mut(&(*win, module.clone())).map(|s| {
                        s.mapped = true;
                        s.kind.clone()
                    })
                };
                if let Some(SurfaceKind::Chat { view }) = kind {
                    self.post(HostEvent::Chat(ChatEvent::PageShown {
                        win: *win,
                        view,
                        op_index: None,
                    }));
                }
            }
            // A dropped surface keeps its widget, off screen, for the module's next instance, as a native host's
            // grid keeps a killed module's host.
            HostOp::Unmap { win, module } | HostOp::DropSurface { win, module, .. } => {
                {
                    let mut surfaces = self.surfaces.borrow_mut();
                    if let Some(s) = surfaces.get_mut(&(*win, module.clone())) {
                        s.mapped = false;
                    }
                }
                if self.focus_owner(*win).as_ref() == Some(module) {
                    self.native_focus(*win, None);
                }
            }
            HostOp::CloseWindow { win } => {
                self.closed.borrow_mut().insert(*win);
            }
            HostOp::Allocate { win } => {
                *self.allocations.borrow_mut().entry(*win).or_default() += 1;
            }
            // The floats named go to the top in the op's order; every other host keeps its place below them.
            HostOp::Restack { win, moves } => {
                let mut stacking = self.stacking.borrow_mut();
                let hosts = stacking.entry(*win).or_default();
                let floats: Vec<ModuleId> = moves
                    .iter()
                    .map(|step| step.module.clone())
                    .filter(|module| hosts.contains(module))
                    .collect();
                hosts.retain(|module| !floats.contains(module));
                hosts.extend(floats);
            }
            _ => {}
        }
        Applied::Done
    }
}

impl Host for FakeHost {
    fn apply(&self, op: &HostOp) -> Applied {
        let index = self.apply_index.get();
        self.apply_index.set(index + 1);
        // Reserve the log position before signals or hooks can apply another operation.
        let position = self.log.borrow().len();
        self.log.borrow_mut().push(Logged::Op(op.clone(), Applied::Done));
        let applied = if self.model.get() {
            self.model_apply(op)
        } else {
            Applied::Done
        };
        self.log.borrow_mut()[position] = Logged::Op(op.clone(), applied);
        let hook = self.apply_hook.borrow().clone();
        if let Some(hook) = hook {
            hook(self, index, op);
        }
        applied
    }
    fn monitors(&self) -> Vec<Monitor> {
        vec![]
    }
}

impl super::conformance::GridObserve for FakeHost {
    fn allocations_asked(&self, win: WinId) -> usize {
        self.allocations.borrow().get(&win).copied().unwrap_or(0)
    }
    fn stacking(&self, win: WinId) -> Vec<ModuleId> {
        self.stacking.borrow().get(&win).cloned().unwrap_or_default()
    }
    fn window_open(&self, win: WinId) -> bool {
        !self.is_closed(win)
    }
}

impl HostReads for FakeHost {
    fn running_chats(&self, _: WinId) -> Vec<(ModuleId, Option<TabId>)> {
        self.reads.borrow().running_chats.clone()
    }
    fn terminal_job_modules(&self, _: WinId) -> Vec<ModuleId> {
        self.reads.borrow().terminal_job_modules.clone()
    }
    fn pages_with_tasks(&self, _: WinId) -> Vec<ModuleId> {
        self.reads.borrow().pages_with_tasks.clone()
    }
    fn base_tabs(&self, _: WinId) -> Vec<TabId> {
        self.reads.borrow().base_tabs.clone()
    }
    fn active_base_tab(&self, _: WinId) -> Option<TabId> {
        self.reads.borrow().active_base_tab
    }
    fn tab_move(&self, win: WinId, tab: TabId) -> Result<ChatMoveFacts, String> {
        self.reads
            .borrow()
            .tab_moves
            .get(&tab)
            .cloned()
            .unwrap_or_else(|| self.chat_move(win, ViewId::BASE, None))
    }
    fn arrived_tab(&self, _: WinId, mv: MoveId) -> Option<TabId> {
        self.reads.borrow().arrived_tabs.get(&mv).copied()
    }
    fn moved_tab_key(&self, _: WinId, tab: TabId) -> Option<crate::window_link::base_tabs::MovedTabKey> {
        self.reads.borrow().moved_tab_keys.get(&tab).cloned()
    }
    fn terminal_jobs(&self, _: WinId) -> usize {
        self.reads.borrow().terminal_jobs
    }
    fn tab_can_step(&self, _: WinId, delta: i32) -> bool {
        self.reads.borrow().tab_steps.get(&delta).copied().unwrap_or(false)
    }
    fn tab_has_last(&self, _: WinId) -> bool {
        self.reads.borrow().tab_last
    }
    fn tab_place(&self, _: WinId, n: u16) -> Option<crate::tab_set::TabPlace> {
        self.reads.borrow().tab_places.get(&n).copied()
    }

    fn grid_size(&self, _: WinId) -> Size {
        self.reads.borrow().grid_size
    }
    fn handle_px(&self, _: WinId) -> i32 {
        1
    }
    fn strip_px(&self, _: WinId) -> i32 {
        super::grid::DEFAULT_STRIP_PX
    }
    fn module_min(&self, _: WinId, module: &ModuleId) -> Size {
        self.reads.borrow().module_min.get(module).copied().unwrap_or_default()
    }
    fn editor_cell(&self, _: WinId) -> Option<(f64, f64)> {
        self.reads.borrow().editor_cell
    }
    fn editor_facts(&self, _: WinId) -> EditorFacts {
        self.reads.borrow().editor_facts.clone()
    }
    fn editor_running(&self, _: WinId) -> bool {
        self.reads.borrow().editor_facts.running
    }
    fn chat_claims(&self, view: ViewId, direction: Direction) -> bool {
        self.reads.borrow().chat_claims.contains(&(view, direction))
    }
    fn view_attention(&self, view: ViewId) -> crate::attention::Attention {
        self.reads.borrow().attention.get(&view).copied().unwrap_or_default()
    }
    fn oldest_card_view(&self, _: WinId) -> Option<ViewId> {
        self.reads.borrow().oldest_card_view
    }
    fn chat_summary(&self, _: WinId) -> ChatSummary {
        self.reads.borrow().chat_summary
    }
    fn chat_kill_counts(&self, view: ViewId) -> ChatKillCounts {
        self.reads.borrow().kill_counts.get(&view).cloned().unwrap_or_default()
    }
    fn chat_first_tab(&self, _: ViewId, _: bool) -> Result<(), String> {
        self.reads.borrow().first_tab_refused.clone().map_or(Ok(()), Err)
    }
    fn module_size(&self, _: WinId, module: &ModuleId) -> Option<Size> {
        self.reads.borrow().module_size.get(module).copied()
    }
    fn page_built(&self, _: WinId, module: &ModuleId) -> bool {
        self.reads.borrow().page_built.contains(module)
    }
    fn plugin_keys_help(&self, _: WinId, module: &ModuleId) -> Option<crate::plugin::keys::PluginKeysHelp> {
        self.log
            .borrow_mut()
            .push(Logged::Read(format!("plugin_keys_help({module})")));
        self.reads.borrow().plugin_help.get(module).cloned()
    }
    fn chat_views(&self) -> Vec<ViewId> {
        self.reads.borrow().chat_views.clone()
    }
    fn can_break_out(&self, _: WinId) -> Result<(), String> {
        self.reads.borrow().break_out.clone()
    }
    fn chat_move(&self, _: WinId, view: ViewId, _: Option<&str>) -> Result<ChatMoveFacts, String> {
        self.reads
            .borrow()
            .chat_moves
            .get(&view)
            .cloned()
            .unwrap_or_else(|| Err(NOT_CARRIED_OUT.into()))
    }
    fn view_of_session(&self, _: WinId, session: &str) -> Option<ViewId> {
        self.reads.borrow().sessions.get(session).copied()
    }
}

/// One message the controller handed the fake link, in the order it handed them.
#[derive(Debug, Clone, PartialEq)]
pub struct SentMsg {
    pub msg: crate::window_link::Msg,
    pub handoff: Option<Handoff>,
    /// Sent with `send_confirmed`, whose answer the test gives.
    pub confirmed: bool,
}

/// The link to the other window as a recording: every message kept, a confirmed send answered when the test
/// says, and a send refused while the test says the link refuses.
#[derive(Default)]
pub struct FakePeerPort {
    sent: RefCell<Vec<SentMsg>>,
    answers: RefCell<Vec<std::sync::mpsc::Sender<std::io::Result<()>>>>,
    refuse: RefCell<Option<String>>,
    refuse_confirmed: RefCell<Option<String>>,
}

impl FakePeerPort {
    pub fn new() -> Rc<Self> {
        Rc::new(Self::default())
    }
    /// Every message handed over so far.
    pub fn sent(&self) -> Vec<SentMsg> {
        self.sent.borrow().clone()
    }
    /// The kinds of the messages handed over so far, in order.
    pub fn kinds(&self) -> Vec<&'static str> {
        self.sent.borrow().iter().map(|s| s.msg.kind()).collect()
    }
    /// Plain sends fail with `why` from now on (`None`: they go through again).
    pub fn refuse_sends(&self, why: Option<&str>) {
        *self.refuse.borrow_mut() = why.map(str::to_owned);
    }
    /// Confirmed sends fail with `why` from now on.
    pub fn refuse_confirmed_sends(&self, why: Option<&str>) {
        *self.refuse_confirmed.borrow_mut() = why.map(str::to_owned);
    }
    /// Answers the `n`th confirmed send (from 0), as the link thread does once the packet is on the socket or
    /// will never be.
    pub fn answer(&self, n: usize, answer: std::io::Result<()>) {
        let _ = self.answers.borrow()[n].send(answer);
    }
}

impl PeerPort for FakePeerPort {
    fn send(&self, msg: crate::window_link::Msg, handoff: Option<Handoff>) -> Result<(), String> {
        if let Some(why) = self.refuse.borrow().clone() {
            return Err(why);
        }
        self.sent.borrow_mut().push(SentMsg {
            msg,
            handoff,
            confirmed: false,
        });
        Ok(())
    }
    fn send_confirmed(
        &self,
        msg: crate::window_link::Msg,
    ) -> Result<std::sync::mpsc::Receiver<std::io::Result<()>>, String> {
        if let Some(why) = self.refuse_confirmed.borrow().clone() {
            return Err(why);
        }
        let (tx, rx) = std::sync::mpsc::channel();
        self.answers.borrow_mut().push(tx);
        self.sent.borrow_mut().push(SentMsg {
            msg,
            handoff: None,
            confirmed: true,
        });
        Ok(rx)
    }
}

fn attached(sink: &RefCell<Option<Weak<dyn HostSink>>>, event: &HostEvent) -> Rc<dyn HostSink> {
    sink.borrow()
        .as_ref()
        .and_then(Weak::upgrade)
        .unwrap_or_else(|| panic!("cannot post {event:?}: no attached live sink"))
}

struct Timer {
    token: TimerToken,
    deadline: Instant,
    idle: bool,
    order: u64,
}

#[derive(Default)]
struct WakeFlag(AtomicUsize);
impl Waker for WakeFlag {
    fn wake(&self) {
        self.0.fetch_add(1, Ordering::Release);
    }
}

pub struct FakeScheduler {
    sink: RefCell<Option<Weak<dyn HostSink>>>,
    clock: Cell<Instant>,
    timers: RefCell<Vec<Timer>>,
    order: Cell<u64>,
    watches: RefCell<BTreeMap<WatchToken, (RawFd, Interest, Priority)>>,
    wake: Arc<WakeFlag>,
}

impl FakeScheduler {
    pub fn new() -> Rc<Self> {
        Rc::new(Self {
            sink: RefCell::new(None),
            clock: Cell::new(Instant::now()),
            timers: RefCell::new(vec![]),
            order: Cell::new(0),
            watches: RefCell::new(BTreeMap::new()),
            wake: Arc::new(WakeFlag::default()),
        })
    }
    pub fn attach(&self, sink: Weak<dyn HostSink>) {
        *self.sink.borrow_mut() = Some(sink);
    }
    fn post(&self, event: HostEvent) {
        attached(&self.sink, &event).post(event);
    }
    fn arm(&self, delay: Duration, token: TimerToken, idle: bool) {
        self.cancel(token);
        let order = self.order.get();
        self.order.set(order + 1);
        self.timers.borrow_mut().push(Timer {
            token,
            deadline: self.now() + delay,
            idle,
            order,
        });
    }
    pub fn advance(&self, by: Duration) {
        let end = self.now() + by;
        for _ in 0..self.wake.0.swap(0, Ordering::AcqRel) {
            self.post(HostEvent::Wake);
        }
        loop {
            let next = {
                let mut timers = self.timers.borrow_mut();
                let index = timers
                    .iter()
                    .enumerate()
                    .filter(|(_, t)| t.deadline <= end)
                    .min_by_key(|(_, t)| (t.deadline, t.idle, t.order))
                    .map(|(i, _)| i);
                index.map(|i| timers.remove(i))
            };
            let Some(timer) = next else { break };
            self.clock.set(timer.deadline);
            self.post(HostEvent::Timer(timer.token));
        }
        self.clock.set(end);
    }
    pub fn fire_fd(&self, token: WatchToken) {
        let watched = self.watches.borrow().contains_key(&token);
        if watched {
            self.post(HostEvent::Fd(token));
        }
    }
    pub fn armed(&self) -> Vec<(TimerToken, Instant)> {
        let timers = self.timers.borrow();
        let mut ordered: Vec<_> = timers.iter().collect();
        ordered.sort_by_key(|t| (t.deadline, t.idle, t.order));
        ordered.into_iter().map(|t| (t.token, t.deadline)).collect()
    }
}

impl Scheduler for FakeScheduler {
    fn after(&self, delay: Duration, token: TimerToken) {
        self.arm(delay, token, false);
    }
    fn idle(&self, token: TimerToken) {
        self.arm(Duration::ZERO, token, true);
    }
    fn cancel(&self, token: TimerToken) {
        self.timers.borrow_mut().retain(|t| t.token != token);
    }
    fn watch_fd(&self, fd: RawFd, interest: Interest, priority: Priority, token: WatchToken) {
        self.watches.borrow_mut().insert(token, (fd, interest, priority));
    }
    fn unwatch(&self, token: WatchToken) {
        self.watches.borrow_mut().remove(&token);
    }
    fn now(&self) -> Instant {
        self.clock.get()
    }
    fn waker(&self) -> Arc<dyn Waker> {
        self.wake.clone()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Call {
    Handle(HostEvent),
    /// A key at a capture stage: the module whose stage it was, `None` for the window's.
    Key(WinId, Option<ModuleId>, KeyPress),
    Run(WinId, Action, Origin),
    Close(WinId, CloseBy),
}

#[derive(Default)]
pub struct ScriptedDecide {
    calls: Vec<Call>,
    handles: VecDeque<Batch>,
    keys: VecDeque<(Verdict, Batch)>,
    runs: VecDeque<Batch>,
    closes: VecDeque<(CloseVerdict, Batch)>,
}

impl ScriptedDecide {
    pub fn then_handle(&mut self, batch: Batch) {
        self.handles.push_back(batch);
    }
    pub fn then_key(&mut self, verdict: Verdict, batch: Batch) {
        self.keys.push_back((verdict, batch));
    }
    pub fn then_run(&mut self, batch: Batch) {
        self.runs.push_back(batch);
    }
    pub fn then_close(&mut self, verdict: CloseVerdict, batch: Batch) {
        self.closes.push_back((verdict, batch));
    }
    pub fn calls(&self) -> Vec<Call> {
        self.calls.clone()
    }
}

impl Decide for ScriptedDecide {
    fn handle(&mut self, event: HostEvent) -> Batch {
        self.calls.push(Call::Handle(event));
        self.handles.pop_front().unwrap_or_default()
    }
    fn key(&mut self, win: WinId, stage: &KeyStage, press: &KeyPress) -> (Verdict, Batch) {
        self.calls.push(Call::Key(win, stage.module().cloned(), press.clone()));
        self.keys
            .pop_front()
            .unwrap_or((Verdict::PassThrough, Batch::default()))
    }
    fn run(&mut self, win: WinId, action: Action, origin: Origin) -> Batch {
        self.calls.push(Call::Run(win, action, origin));
        self.runs.pop_front().unwrap_or_default()
    }
    fn close_requested(&mut self, win: WinId, by: CloseBy) -> (CloseVerdict, Batch) {
        self.calls.push(Call::Close(win, by));
        self.closes
            .pop_front()
            .unwrap_or((CloseVerdict::Proceed, Batch::default()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keymap::{key::KeyName, prefix::Press};
    use crate::wm::conformance::RecordingSink;

    fn attached_scheduler() -> (Rc<FakeScheduler>, Rc<RecordingSink>) {
        let scheduler = FakeScheduler::new();
        let sink = Rc::new(RecordingSink::default());
        let erased: Rc<dyn HostSink> = sink.clone();
        scheduler.attach(Rc::downgrade(&erased));
        (scheduler, sink)
    }

    #[test]
    fn wake_and_fd_readiness_wait_for_explicit_delivery() {
        let (s, sink) = attached_scheduler();
        s.watch_fd(42, Interest::Read, Priority::Input, WatchToken(1));
        let wake = s.waker();
        std::thread::spawn(move || wake.wake()).join().unwrap();
        assert!(sink.events().is_empty());
        s.fire_fd(WatchToken(2));
        s.fire_fd(WatchToken(1));
        s.unwatch(WatchToken(1));
        s.fire_fd(WatchToken(1));
        s.advance(Duration::ZERO);
        assert_eq!(
            sink.events(),
            vec![(None, HostEvent::Fd(WatchToken(1))), (None, HostEvent::Wake)]
        );
        s.advance(Duration::ZERO);
        assert_eq!(sink.events().len(), 2);
    }

    #[test]
    fn timers_precede_idles_at_the_same_instant_and_callbacks_can_rearm() {
        struct RearmingSink {
            scheduler: Rc<FakeScheduler>,
            events: RefCell<Vec<HostEvent>>,
        }
        impl HostSink for RearmingSink {
            fn post(&self, e: HostEvent) {
                if e == HostEvent::Timer(TimerToken(1)) {
                    self.scheduler.idle(TimerToken(3));
                    self.scheduler.after(Duration::ZERO, TimerToken(4));
                }
                self.events.borrow_mut().push(e);
            }
            fn post_deferred(&self, e: HostEvent) {
                self.post(e);
            }
            fn close_requested(&self, _: WinId, _: CloseBy) -> CloseVerdict {
                CloseVerdict::Proceed
            }
        }
        let s = FakeScheduler::new();
        let sink = Rc::new(RearmingSink {
            scheduler: s.clone(),
            events: RefCell::new(vec![]),
        });
        let erased: Rc<dyn HostSink> = sink.clone();
        s.attach(Rc::downgrade(&erased));
        s.idle(TimerToken(2));
        s.after(Duration::ZERO, TimerToken(1));
        s.advance(Duration::ZERO);
        assert_eq!(
            *sink.events.borrow(),
            vec![
                HostEvent::Timer(TimerToken(1)),
                HostEvent::Timer(TimerToken(4)),
                HostEvent::Timer(TimerToken(2)),
                HostEvent::Timer(TimerToken(3))
            ]
        );
    }

    #[test]
    fn native_posts_precede_hooks_and_hooks_can_reenter_the_host() {
        let h = FakeHost::new();
        let sink = Rc::new(RecordingSink::default());
        let erased: Rc<dyn HostSink> = sink.clone();
        h.attach(Rc::downgrade(&erased));
        let m = ModuleId::agent();
        h.with_surface(
            WinId::MAIN,
            m.clone(),
            SurfaceKind::Chat {
                view: crate::tabs::ViewId::BASE,
            },
            false,
        );
        h.on_apply(|host, i, _| {
            if i == 0 {
                assert!(matches!(
                    host.log().last(),
                    Some(Logged::Event(HostEvent::Chat(ChatEvent::PageShown {
                        op_index: None,
                        ..
                    })))
                ));
                host.apply(&HostOp::Allocate { win: WinId::MAIN });
                host.on_apply(|_, _, _| {});
            }
        });
        h.apply(&HostOp::Map {
            win: WinId::MAIN,
            module: m.clone(),
        });
        h.apply(&HostOp::Focus {
            win: WinId::MAIN,
            to: FocusTo::Module(m.clone()),
            on_grab: vec![],
        });
        assert_eq!(h.focus_owner(WinId::MAIN), Some(m));
        h.refuse_kind(|_| true);
        let op = HostOp::CreateSurface {
            win: WinId::MAIN,
            module: ModuleId::terminal(),
            kind: SurfaceKind::Terminal { cwd: ".".into() },
        };
        assert_eq!(h.apply(&op), Applied::Refused);
        assert!(
            h.log().iter().any(|entry| matches!(
                entry,
                Logged::Event(HostEvent::Refused { what: OpTag::CreateSurface, surface: Some((WinId::MAIN, module)), .. })
                    if *module == ModuleId::terminal()
            )),
            "a refused create names the surface it was for"
        );
        h.apply(&HostOp::CloseWindow { win: WinId::MAIN });
        assert!(h.is_closed(WinId::MAIN));
        assert_eq!(sink.close_requests(), 0, "a close by op asks no close request");
    }

    #[test]
    #[should_panic(expected = "cannot post Wake")]
    fn a_post_before_attachment_names_the_event() {
        FakeHost::new().post(HostEvent::Wake);
    }

    #[test]
    fn the_fake_scheduler_fires_timers_in_deadline_order_and_never_synchronously() {
        let (s, sink) = attached_scheduler();
        s.after(Duration::from_millis(30), TimerToken(1));
        s.after(Duration::from_millis(10), TimerToken(2));
        s.after(Duration::from_millis(10), TimerToken(3));
        s.idle(TimerToken(4));
        s.after(Duration::from_millis(8), TimerToken(5));
        s.cancel(TimerToken(5));
        assert!(sink.events().is_empty());
        s.advance(Duration::from_millis(5));
        assert_eq!(sink.events(), vec![(None, HostEvent::Timer(TimerToken(4)))]);
        s.advance(Duration::from_millis(10));
        assert_eq!(
            sink.events().iter().map(|(_, e)| e.clone()).collect::<Vec<_>>(),
            vec![
                HostEvent::Timer(TimerToken(4)),
                HostEvent::Timer(TimerToken(2)),
                HostEvent::Timer(TimerToken(3))
            ]
        );
        s.advance(Duration::from_millis(20));
        assert_eq!(sink.events().last(), Some(&(None, HostEvent::Timer(TimerToken(1)))));
        assert!(s.armed().is_empty());
    }

    #[test]
    fn the_fake_hosts_model_can_be_switched_off() {
        let h = FakeHost::new();
        let sink = Rc::new(RecordingSink::default());
        let erased: Rc<dyn HostSink> = sink.clone();
        h.attach(Rc::downgrade(&erased));
        h.set_model(false);
        h.with_surface(
            WinId::MAIN,
            ModuleId::terminal(),
            SurfaceKind::Terminal { cwd: ".".into() },
            false,
        );
        let op = HostOp::Focus {
            win: WinId::MAIN,
            to: FocusTo::Module(ModuleId::terminal()),
            on_grab: vec![],
        };
        assert_eq!(h.apply(&op), Applied::Done);
        assert!(sink.events().is_empty());
        assert_eq!(h.log(), vec![Logged::Op(op, Applied::Done)]);
    }

    #[test]
    fn the_fake_schedulers_clock_moves_only_with_advance() {
        struct ClockSink {
            scheduler: Rc<FakeScheduler>,
            seen: RefCell<Vec<Instant>>,
        }
        impl HostSink for ClockSink {
            fn post(&self, event: HostEvent) {
                if matches!(event, HostEvent::Timer(_)) {
                    self.seen.borrow_mut().push(self.scheduler.now());
                }
            }
            fn post_deferred(&self, event: HostEvent) {
                self.post(event);
            }
            fn close_requested(&self, _: WinId, _: CloseBy) -> CloseVerdict {
                CloseVerdict::Proceed
            }
        }
        let s = FakeScheduler::new();
        let sink = Rc::new(ClockSink {
            scheduler: s.clone(),
            seen: RefCell::new(vec![]),
        });
        let erased: Rc<dyn HostSink> = sink.clone();
        s.attach(Rc::downgrade(&erased));
        let start = s.now();
        s.after(Duration::from_millis(7), TimerToken(1));
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(s.now(), start);
        s.advance(Duration::from_millis(10));
        assert_eq!(s.now(), start + Duration::from_millis(10));
        assert_eq!(*sink.seen.borrow(), vec![start + Duration::from_millis(7)]);
    }

    #[test]
    fn the_fake_host_calls_on_apply_during_each_op_and_is_attached_after_it_is_built() {
        let h = FakeHost::new();
        let sink = Rc::new(RecordingSink::default());
        let erased: Rc<dyn HostSink> = sink.clone();
        h.attach(Rc::downgrade(&erased));
        let calls = Rc::new(RefCell::new(vec![]));
        let seen = calls.clone();
        h.on_apply(move |host, i, op| {
            seen.borrow_mut().push((i, OpTag::of(op)));
            if i == 1 {
                host.post(HostEvent::Wake);
            }
        });
        for i in 0..2 {
            sink.applying(Some(i));
            h.apply(&HostOp::Allocate { win: WinId::MAIN });
            sink.applying(None);
        }
        assert_eq!(*calls.borrow(), vec![(0, OpTag::Allocate), (1, OpTag::Allocate)]);
        assert_eq!(sink.events(), vec![(Some(1), HostEvent::Wake)]);
    }

    #[test]
    fn a_scripted_decider_answers_in_queue_order_and_records_its_calls() {
        let mut d = ScriptedDecide::default();
        let mut batch = Batch::default();
        batch.push_effect(HostOp::Quit);
        d.then_handle(batch.clone());
        d.then_handle(Batch::default());
        d.then_key(Verdict::Consumed, batch.clone());
        d.then_run(batch.clone());
        d.then_close(CloseVerdict::Stop, batch.clone());
        let w = WinId::MAIN;
        let m = ModuleId::agent();
        let key = KeyPress {
            keycode: 42,
            press: Press::Other,
            key: Some(KeyName::Char('x')),
            mods: Mods::default(),
            super_or_hyper: false,
            super_or_meta: false,
        };
        assert_eq!(d.handle(HostEvent::Wake), batch);
        assert!(d.handle(HostEvent::MonitorsChanged { monitors: vec![] }).is_empty());
        assert_eq!(d.key(w, &KeyStage::Window, &key), (Verdict::Consumed, batch.clone()));
        assert_eq!(d.run(w, Action::FocusParent, Origin::Lua), batch);
        assert_eq!(d.close_requested(w, CloseBy::User), (CloseVerdict::Stop, batch.clone()));
        assert_eq!(
            d.calls(),
            vec![
                Call::Handle(HostEvent::Wake),
                Call::Handle(HostEvent::MonitorsChanged { monitors: vec![] }),
                Call::Key(w, None, key.clone()),
                Call::Run(w, Action::FocusParent, Origin::Lua),
                Call::Close(w, CloseBy::User)
            ]
        );
        assert!(d.handle(HostEvent::Wake).is_empty());
        assert_eq!(
            d.key(w, &KeyStage::Module(&m), &key),
            (Verdict::PassThrough, Batch::default())
        );
        assert!(d.run(w, Action::FocusChild, Origin::Chip).is_empty());
        assert_eq!(
            d.close_requested(w, CloseBy::Compositor),
            (CloseVerdict::Proceed, Batch::default())
        );
    }
}
