//! Deterministic host services for testing decisions without a native window system.

use super::{host::*, window::*};
use crate::keymap::action::Action;
use crate::layout::{Direction, ModuleId, Size};
use crate::tabs::ViewId;
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
type ShellArm = Rc<dyn Fn(&FakeHost, WinId, &ShellCall, u32)>;
type RefuseKind = Rc<dyn Fn(&SurfaceKind) -> bool>;

#[derive(Debug, Clone, PartialEq)]
pub enum Logged {
    Op(HostOp, Applied),
    ShellVerb(ShellVerb),
    Event(HostEvent),
}

struct Surface {
    kind: SurfaceKind,
    mapped: bool,
}

pub struct FakeHost {
    sink: RefCell<Option<Weak<dyn HostSink>>>,
    model: Cell<bool>,
    surfaces: RefCell<BTreeMap<(WinId, ModuleId), Surface>>,
    focus: RefCell<BTreeMap<WinId, FocusOwner>>,
    closed: RefCell<BTreeSet<WinId>>,
    log: RefCell<Vec<Logged>>,
    shell_arm: RefCell<Option<ShellArm>>,
    apply_hook: RefCell<Option<ApplyHook>>,
    apply_index: Cell<usize>,
    refuse_kind: RefCell<Option<RefuseKind>>,
    reads: RefCell<Reads>,
    /// Each window's module hosts as its grid would stack them, bottom first: in the order they were made,
    /// until a restack moves its floats to the top.
    stacking: RefCell<BTreeMap<WinId, Vec<ModuleId>>>,
    /// How many allocations each window's grid was asked for.
    allocations: RefCell<BTreeMap<WinId, usize>>,
}

/// What the fake answers as [`HostReads`]: [`NoReads`]' values until a test sets one.
struct Reads {
    grid_size: Size,
    editor_cell: Option<(f64, f64)>,
    editor_running: bool,
    chat_claims: Vec<(ViewId, Direction)>,
    module_min: BTreeMap<ModuleId, Size>,
}

impl Default for Reads {
    fn default() -> Self {
        Reads {
            grid_size: super::grid::UNALLOCATED,
            editor_cell: None,
            editor_running: false,
            chat_claims: Vec::new(),
            module_min: BTreeMap::new(),
        }
    }
}

impl FakeHost {
    pub fn new() -> Rc<Self> {
        Rc::new(Self {
            sink: RefCell::new(None),
            model: Cell::new(true),
            surfaces: RefCell::new(BTreeMap::new()),
            focus: RefCell::new(BTreeMap::new()),
            closed: RefCell::new(BTreeSet::new()),
            log: RefCell::new(vec![]),
            shell_arm: RefCell::new(None),
            apply_hook: RefCell::new(None),
            apply_index: Cell::new(0),
            refuse_kind: RefCell::new(None),
            reads: RefCell::new(Reads::default()),
            stacking: RefCell::new(BTreeMap::new()),
            allocations: RefCell::new(BTreeMap::new()),
        })
    }
    pub fn set_grid_size(&self, size: Size) {
        self.reads.borrow_mut().grid_size = size;
    }
    pub fn set_editor_cell(&self, cell: Option<(f64, f64)>) {
        self.reads.borrow_mut().editor_cell = cell;
    }
    pub fn set_editor_running(&self, running: bool) {
        self.reads.borrow_mut().editor_running = running;
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
    pub fn on_shell_verb(&self, arm: impl Fn(&FakeHost, WinId, &ShellCall, u32) + 'static) {
        *self.shell_arm.borrow_mut() = Some(Rc::new(arm));
    }
    pub fn on_apply(&self, f: impl Fn(&FakeHost, usize, &HostOp) + 'static) {
        *self.apply_hook.borrow_mut() = Some(Rc::new(f));
        self.apply_index.set(0);
    }
    pub fn refuse_kind(&self, f: impl Fn(&SurfaceKind) -> bool + 'static) {
        *self.refuse_kind.borrow_mut() = Some(Rc::new(f));
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
        {
            let mut focus = self.focus.borrow_mut();
            focus.remove(&win);
            if let Some(owner) = &owner {
                focus.insert(win, owner.clone());
            }
        }
        self.post(HostEvent::NativeFocus {
            win,
            owner,
            chain: FocusChain(chain),
            active: true,
            op_index: None,
        });
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
        match op {
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
                    FocusTo::Nothing => None,
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
    fn run_shell_verb(&self, win: WinId, call: &ShellCall, keycode: u32) {
        self.log.borrow_mut().push(Logged::ShellVerb(ShellVerb {
            win,
            call: call.clone(),
            keycode,
        }));
        let arm = self.shell_arm.borrow().clone();
        if let Some(arm) = arm {
            arm(self, win, call, keycode);
        }
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
}

impl HostReads for FakeHost {
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
    fn editor_running(&self, _: WinId) -> bool {
        self.reads.borrow().editor_running
    }
    fn chat_claims(&self, view: ViewId, direction: Direction) -> bool {
        self.reads.borrow().chat_claims.contains(&(view, direction))
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
    Key(WinId, KeyStage, KeyPress),
    Run(WinId, Action, Origin),
    Focus(WinId, ModuleId),
    Close(WinId, CloseBy),
}

#[derive(Default)]
pub struct ScriptedDecide {
    calls: Vec<Call>,
    handles: VecDeque<Batch>,
    keys: VecDeque<(Verdict, Batch)>,
    runs: VecDeque<Batch>,
    focuses: VecDeque<Batch>,
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
    pub fn then_focus(&mut self, batch: Batch) {
        self.focuses.push_back(batch);
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
        self.calls.push(Call::Key(win, stage.clone(), press.clone()));
        self.keys
            .pop_front()
            .unwrap_or((Verdict::PassThrough, Batch::default()))
    }
    fn run(&mut self, win: WinId, action: Action, origin: Origin) -> Batch {
        self.calls.push(Call::Run(win, action, origin));
        self.runs.pop_front().unwrap_or_default()
    }
    fn focus(&mut self, win: WinId, module: &ModuleId) -> Batch {
        self.calls.push(Call::Focus(win, module.clone()));
        self.focuses.pop_front().unwrap_or_default()
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
        h.on_shell_verb(|host, win, _, _| {
            host.apply(&HostOp::CloseWindow { win });
        });
        let call = ShellCall::Action(Action::FloatToggle);
        h.run_shell_verb(WinId::MAIN, &call, 42);
        assert!(h.is_closed(WinId::MAIN));
        assert_eq!(sink.close_requests(), 0);
        assert!(h.log().contains(&Logged::ShellVerb(ShellVerb {
            win: WinId::MAIN,
            call,
            keycode: 42
        })));
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
        d.then_focus(batch.clone());
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
        assert!(d.handle(HostEvent::MonitorsChanged).is_empty());
        assert_eq!(d.key(w, &KeyStage::Window, &key), (Verdict::Consumed, batch.clone()));
        assert_eq!(d.run(w, Action::FocusParent, Origin::Lua), batch);
        assert_eq!(d.focus(w, &m), batch);
        assert_eq!(d.close_requested(w, CloseBy::User), (CloseVerdict::Stop, batch.clone()));
        assert_eq!(
            d.calls(),
            vec![
                Call::Handle(HostEvent::Wake),
                Call::Handle(HostEvent::MonitorsChanged),
                Call::Key(w, KeyStage::Window, key.clone()),
                Call::Run(w, Action::FocusParent, Origin::Lua),
                Call::Focus(w, m.clone()),
                Call::Close(w, CloseBy::User)
            ]
        );
        assert!(d.handle(HostEvent::Wake).is_empty());
        assert_eq!(
            d.key(w, &KeyStage::Module(m.clone()), &key),
            (Verdict::PassThrough, Batch::default())
        );
        assert!(d.run(w, Action::FocusChild, Origin::Chip).is_empty());
        assert!(d.focus(w, &m).is_empty());
        assert_eq!(
            d.close_requested(w, CloseBy::Compositor),
            (CloseVerdict::Proceed, Batch::default())
        );
    }
}
