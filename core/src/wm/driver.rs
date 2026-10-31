//! The one driver per process: it owns the decider, applies the batches it returns through the host, and keeps
//! every event that arrives while a batch applies in one queue, so nothing re-enters the decider.
//!
//! The rules, in the order a caller meets them:
//! - An event posted while no batch applies is handled at once. One posted while a batch applies joins the queue,
//!   which holds events and nested batches together and drains in arrival order after the batch. `NativeFocus` and
//!   `Chat(PageShown)` queued while op `i` applies are stamped `op_index: Some(i)`, so their handler can judge by what
//!   was true when they fired.
//! - `post_deferred` never runs inside the call that posted it: its events wait on their own queue for the reserved
//!   idle ([`TimerToken::DRIVER_IDLE`]).
//! - A batch applies its layout ops, then its effects, then its Lua ops. A `Focus` op's `on_grab` ops run right after
//!   it, and only when the grab took; a `HostOp::Lua` among them waits for the end of the batch's Lua ops, so Lua
//!   never runs in the middle of a batch.
//! - A `CreateSurface` op the host carried out is answered with `Created`, and a `PrepareSurface` with `Prepared`,
//!   each queued behind what the op itself caused: besides a `Focus` op's tail, the one place an op's answer is
//!   read.
//! - `key` and `close_requested` answer synchronously. A key that arrives while the driver is busy is answered from
//!   the decider as it stands, its batch queued, and counted; every close request prints the count, so a run's log
//!   shows whether a key ever arrived re-entrantly.
//! - The decider is never borrowed while the host runs an op, and `query` copies its answer out.
//! - Each time the queue empties the decider is asked what it judges against the state the batches left
//!   ([`Decide::after_drain`]); its batch applies like any other, and the queue drains again after it.
//! - The grid's allocation ([`Allocator`]) is answered at once from inside the toolkit's allocation, and may come
//!   while a batch applies: it borrows the decider for its own duration only, and what it causes (a pin settled)
//!   is posted deferred.

use super::controller::Controller;
use super::host::*;
use super::window::WinId;
use crate::keymap::action::Action;
use crate::layout::{Arrangement, ModuleId, Size};
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, VecDeque};
use std::rc::Rc;

/// One pending item: an event still to be decided, or a batch already decided that waits its turn.
enum Item {
    Event(HostEvent),
    Batch(Batch),
}

/// Applies a decider's batches through a host, one at a time, with every re-entrant arrival queued.
pub struct Driver<C: Decide = Controller> {
    decide: RefCell<C>,
    host: Rc<dyn Host>,
    scheduler: Rc<dyn Scheduler>,
    /// Something is being applied or drained: a batch, or the queue after it.
    busy: Cell<bool>,
    /// The index of the op being applied, while a batch's ops apply.
    op_index: Cell<Option<u32>>,
    queue: RefCell<VecDeque<Item>>,
    deferred: RefCell<VecDeque<HostEvent>>,
    idle_armed: Cell<bool>,
    nested_keys: Cell<u64>,
    /// Each window's last arrangement, answered again when an allocation finds the decider busy.
    arranged: RefCell<BTreeMap<WinId, Rc<Arrangement>>>,
    /// The lines the driver printed, kept so tests can read what a run would have logged.
    #[cfg(test)]
    notes: RefCell<Vec<String>>,
}

impl<C: Decide> Driver<C> {
    /// Builds a driver around a decider. Hosts and schedulers are attached to it afterwards, through a weak
    /// `HostSink`, so the driver owns them and not the other way round.
    pub fn new(decide: C, host: Rc<dyn Host>, scheduler: Rc<dyn Scheduler>) -> Rc<Self> {
        Rc::new(Self {
            decide: RefCell::new(decide),
            host,
            scheduler,
            busy: Cell::new(false),
            op_index: Cell::new(None),
            queue: RefCell::new(VecDeque::new()),
            deferred: RefCell::new(VecDeque::new()),
            idle_armed: Cell::new(false),
            nested_keys: Cell::new(0),
            arranged: RefCell::new(BTreeMap::new()),
            #[cfg(test)]
            notes: RefCell::new(vec![]),
        })
    }

    /// Hands an event to the decider: at once when nothing applies, otherwise after what is already queued.
    pub fn post(&self, event: HostEvent) {
        if event == HostEvent::Timer(TimerToken::DRIVER_IDLE) {
            self.drain_deferred();
            return;
        }
        let event = self.stamp(event);
        if self.busy.get() {
            self.queue.borrow_mut().push_back(Item::Event(event));
            return;
        }
        self.exclusive(|| {
            let batch = self.decide.borrow_mut().handle(event);
            self.apply(batch);
        });
    }

    /// Keeps an event for the driver's own idle. Events seen while the toolkit lays out or paints come this way, so
    /// no batch is ever computed inside an allocation.
    pub fn post_deferred(&self, event: HostEvent) {
        self.deferred.borrow_mut().push_back(event);
        if !self.idle_armed.replace(true) {
            self.scheduler.idle(TimerToken::DRIVER_IDLE);
        }
    }

    /// Decides a key at one capture stage and answers at once, as the toolkit's handler needs. A module's stage
    /// borrows the module's name, so no key on its way into a module copies it.
    pub fn key(&self, win: WinId, stage: &KeyStage<'_>, press: KeyPress) -> Verdict {
        if self.busy.get() {
            let count = self.nested_keys.get() + 1;
            self.nested_keys.set(count);
            if count == 1 {
                self.note("[wm] BUG: nested key: answered from the state as it stands, its batch queued".into());
            }
        }
        let (verdict, batch) = self.decide.borrow_mut().key(win, stage, &press);
        self.submit(batch);
        verdict
    }

    /// Answers a close the user or the compositor asked for. The driver's own closes never ask, so this is the
    /// one place every window's end passes through, and it prints how many keys arrived re-entrantly.
    pub fn close_requested(&self, win: WinId, by: CloseBy) -> CloseVerdict {
        self.note(format!("[wm] nested={}", self.nested_keys()));
        let (verdict, batch) = self.decide.borrow_mut().close_requested(win, by);
        self.submit(batch);
        verdict
    }

    /// Runs an action through the one runner shared by keys, Lua and panel tab verbs.
    pub fn run(&self, win: WinId, action: Action, origin: Origin) {
        let batch = self.decide.borrow_mut().run(win, action, origin);
        self.submit(batch);
    }

    /// Reads the decider. The answer is returned by value, so no borrow outlives the call.
    /// A query made during a controller borrow logs a bug and answers the type's default without calling `f`.
    pub fn query<T: Default + 'static>(&self, f: impl FnOnce(&C) -> T) -> T {
        self.query_or(T::default(), f)
    }

    /// Reads with an explicit fallback, so a gate whose answer is uncertain can refuse.
    pub fn query_or<T: 'static>(&self, fallback: T, f: impl FnOnce(&C) -> T) -> T {
        let Ok(controller) = self.decide.try_borrow() else {
            self.note("[wm] BUG: query while the controller is borrowed".into());
            return fallback;
        };
        f(&controller)
    }

    /// How many keys arrived while the driver was busy.
    pub fn nested_keys(&self) -> u64 {
        self.nested_keys.get()
    }

    #[cfg(test)]
    fn notes(&self) -> Vec<String> {
        self.notes.borrow().clone()
    }

    fn note(&self, line: String) {
        eprintln!("{line}");
        #[cfg(test)]
        self.notes.borrow_mut().push(line);
    }

    /// Fills in which op was applying when a fact fired; the host does not know where in a batch it is.
    fn stamp(&self, event: HostEvent) -> HostEvent {
        let Some(index) = self.op_index.get() else {
            return event;
        };
        match event {
            HostEvent::NativeFocus {
                win,
                owner,
                chain,
                active,
                op_index: None,
            } => HostEvent::NativeFocus {
                win,
                owner,
                chain,
                active,
                op_index: Some(index),
            },
            HostEvent::Chat(ChatEvent::PageShown {
                win,
                view,
                op_index: None,
            }) => HostEvent::Chat(ChatEvent::PageShown {
                win,
                view,
                op_index: Some(index),
            }),
            event => event,
        }
    }

    fn drain_deferred(&self) {
        self.idle_armed.set(false);
        let events: Vec<_> = self.deferred.borrow_mut().drain(..).collect();
        if events.is_empty() {
            return;
        }
        self.exclusive(|| {
            self.queue.borrow_mut().extend(events.into_iter().map(Item::Event));
        });
    }

    /// Applies a decided batch now when nothing else is, or queues it behind what is already waiting.
    fn submit(&self, batch: Batch) {
        if batch.is_empty() {
            return;
        }
        if self.busy.get() {
            self.queue.borrow_mut().push_back(Item::Batch(batch));
            return;
        }
        self.exclusive(|| {
            self.apply(batch);
        });
    }

    /// Runs `f` with the driver marked busy and drains the queue afterwards. Inside an outer call it only runs `f`:
    /// the outer call drains.
    fn exclusive<R>(&self, f: impl FnOnce() -> R) -> R {
        if self.busy.get() {
            return f();
        }
        self.busy.set(true);
        let result = f();
        loop {
            // The queue's borrow ends with this statement, so whatever the item causes may queue more.
            let next = self.queue.borrow_mut().pop_front();
            match next {
                None => {
                    // What is judged against the state every batch before it left; what that causes is queued
                    // and drained as anything else, and judged again once it has.
                    let batch = self.decide.borrow_mut().after_drain();
                    if batch.is_empty() {
                        break;
                    }
                    self.apply(batch);
                }
                Some(Item::Event(event)) => {
                    let batch = self.decide.borrow_mut().handle(event);
                    self.apply(batch);
                }
                Some(Item::Batch(batch)) => {
                    self.apply(batch);
                }
            }
        }
        self.busy.set(false);
        result
    }

    /// Applies one batch in its fixed order. Called only while the driver is busy.
    fn apply(&self, batch: Batch) {
        let outer = self.op_index.get();
        let mut index = 0;
        let mut grab_lua = Vec::new();
        for op in batch.ops() {
            self.apply_op(op, &mut index, &mut grab_lua);
        }
        for lua in batch.lua().iter().cloned().chain(grab_lua) {
            self.apply_op(&HostOp::Lua(lua), &mut index, &mut Vec::new());
        }
        self.op_index.set(outer);
    }

    /// Applies one op and, when it is a `Focus` that took, its `on_grab` tail. A Lua call in the tail is not run
    /// here but collected into `grab_lua`, which the batch runs after its own Lua calls: Lua may call back into the
    /// controller, so it waits until every native and service op of the batch is done.
    fn apply_op(&self, op: &HostOp, index: &mut u32, grab_lua: &mut Vec<LuaOp>) -> Applied {
        self.op_index.set(Some(*index));
        *index += 1;
        // A window ended by the controller's own teardown (a termination signal, the last terminal's exit) asks no
        // close request, so the count is printed at every `CloseWindow` and every window's end records it; a close
        // the user asked for prints it twice, once at its request and once here.
        if let HostOp::CloseWindow { .. } = op {
            self.note(format!("[wm] nested={}", self.nested_keys()));
        }
        let applied = self.host.apply(op);
        // A surface made at run time is added to the layout only once it exists: the batch that adds it is decided
        // after this one, from the event, which waits its turn behind whatever this batch causes.
        if let (HostOp::CreateSurface { win, module, .. }, Applied::Done) = (op, applied) {
            self.queue.borrow_mut().push_back(Item::Event(HostEvent::Created {
                win: *win,
                module: module.clone(),
            }));
        }
        // An arriving module's place is answered the same way: the other window may freeze the module only once
        // the place exists.
        if let (HostOp::PrepareSurface { win, module, .. }, Applied::Done) = (op, applied) {
            self.queue.borrow_mut().push_back(Item::Event(HostEvent::Prepared {
                win: *win,
                module: module.clone(),
            }));
        }
        if let HostOp::Focus { on_grab, .. } = op {
            if applied == Applied::Done {
                for tail in on_grab {
                    match tail {
                        HostOp::Lua(lua) => grab_lua.push(lua.clone()),
                        tail => {
                            self.apply_op(tail, index, grab_lua);
                        }
                    }
                }
            }
        }
        applied
    }
}

impl Driver<Controller> {
    /// Decides a startup phase before applying its batch, with no controller borrow held by the host.
    pub fn start(&self, win: WinId, step: super::start::StartStep) {
        let batch = self.decide.borrow_mut().start(win, step);
        self.submit(batch);
    }

    /// The generation of the first-start screen the window shows, `None` while none is up.
    pub fn onboarding_generation(&self, win: WinId) -> Option<u64> {
        self.query(|controller| controller.onboarding_generation(win))
    }

    /// Seeds the window's text size before the shell installs its reporting hooks.
    pub fn set_text_size(&self, win: WinId, base: f32, reported_s: f32) {
        self.decide.borrow_mut().set_text_size(win, base, reported_s);
    }

    /// The window's first layout batch over the hosts the shell placed ([`Controller::first_apply`]).
    pub fn first_apply(&self, win: WinId, hosted: Vec<super::module::SurfaceDecl>) {
        let batch = self.decide.borrow_mut().first_apply(win, &hosted);
        self.submit(batch);
    }

    /// One request from `:Eitri` in the window's nvim ([`Controller::nvim_request`]).
    pub fn nvim_request(&self, win: WinId, request: crate::lua::layout::LayoutRequest) {
        let batch = self.decide.borrow_mut().nvim_request(win, request);
        self.submit(batch);
    }

    /// An `:Eitri` line the parser refused ([`Controller::nvim_refused`]).
    pub fn nvim_refused(&self, win: WinId, why: &str) {
        let batch = self.decide.borrow_mut().nvim_refused(win, why);
        self.submit(batch);
    }

    /// A plugin's follow command for its module ([`Controller::follow`]): shown with the keys where they are, or
    /// hidden as any hide is, through the runner's gates.
    pub fn follow(&self, win: WinId, module: &ModuleId, show: bool) {
        let batch = self.decide.borrow_mut().follow(win, module, show);
        self.submit(batch);
    }

    /// Returns an editor request's outcome to its caller, so a linked panel receives refusals in its own window.
    /// A borrowed controller refuses because the request cannot be checked while another decision is in progress.
    pub fn editor_request(&self, win: WinId, keys: String) -> Result<(), String> {
        let Ok(mut controller) = self.decide.try_borrow_mut() else {
            self.note("[wm] BUG: an editor request while the controller decides; refused".into());
            return Err(super::feedback::LAYOUT_CHANGING_TEXT.to_string());
        };
        let (answer, batch) = controller.editor_request(win, keys);
        drop(controller);
        self.submit(batch);
        answer
    }

    /// A plugin page's `layout.focus` ([`Controller::plugin_focus`]): its batch carried out, and the answer the page
    /// is given. Asked while the controller decides, it is refused, as a gate whose answer cannot be read is.
    pub fn plugin_focus(&self, win: WinId, module: &ModuleId) -> Result<(), String> {
        let Ok(mut controller) = self.decide.try_borrow_mut() else {
            self.note("[wm] BUG: a plugin's focus while the controller decides; refused".into());
            return Err(super::feedback::LAYOUT_CHANGING_TEXT.to_string());
        };
        let (answer, batch) = controller.plugin_focus(win, module);
        drop(controller);
        self.submit(batch);
        answer
    }

    /// A plugin page's `terminal.at` ([`Controller::plugin_terminal_at`]): its batch carried out, and whether it was
    /// taken; the answer itself comes back as an op. Asked while the controller decides, it is refused, as
    /// `plugin_focus` is.
    pub fn plugin_terminal_at(
        &self,
        win: WinId,
        plugin: &str,
        dir: std::path::PathBuf,
        ticket: u64,
    ) -> Result<(), String> {
        let Ok(mut controller) = self.decide.try_borrow_mut() else {
            self.note("[wm] BUG: a plugin's terminal.at while the controller decides; refused".into());
            return Err(super::feedback::LAYOUT_CHANGING_TEXT.to_string());
        };
        let (answer, batch) = controller.plugin_terminal_at(win, plugin, dir, ticket);
        drop(controller);
        self.submit(batch);
        answer
    }

    /// How long `win`'s prefix waits before the hint panel shows ([`Controller::set_hint_delay`]).
    pub fn set_hint_delay(&self, win: WinId, delay: Option<std::time::Duration>) {
        self.decide.borrow_mut().set_hint_delay(win, delay);
    }

    /// Holds compositor mode's first-start toast for `win`: the first-start screen goes first, and what it writes can
    /// drop the toast ([`Controller::hold_first_toast`]).
    pub fn hold_first_toast(&self, win: WinId, text: String) {
        self.decide.borrow_mut().hold_first_toast(win, text);
    }

    /// What `win`'s module verbs place and kill by ([`Controller::set_module_setup`]).
    pub fn set_module_setup(&self, win: WinId, setup: super::verbs::modules::ModuleSetup) {
        self.decide.borrow_mut().set_module_setup(win, setup);
    }

    /// Which chat or page window `win` is ([`Controller::set_module_name`]).
    pub fn set_module_name(&self, win: WinId, name: crate::window_link::names::WindowName) {
        self.decide.borrow_mut().set_module_name(win, name);
    }

    /// What `win`'s start knows so far ([`Controller::set_early_facts`]).
    pub fn set_early_facts(&self, win: WinId, early: super::start::EarlyFacts) {
        self.decide.borrow_mut().set_early_facts(win, early);
    }

    /// What `win`'s start opens beside it as a hub ([`Controller::set_start_windows`]).
    pub fn set_start_windows(&self, win: WinId, windows: super::start::StartWindows) {
        self.decide.borrow_mut().set_start_windows(win, windows);
    }

    /// Where `win` goes at its start ([`Controller::set_placement`]).
    pub fn set_placement(&self, win: WinId, placement: super::window::WindowPlacement) {
        let batch = self.decide.borrow_mut().set_placement(win, placement);
        self.submit(batch);
    }

    /// `win` is linked to the window `peer`, called `name` ([`Controller::attach_peer`]).
    pub fn attach_peer(
        &self,
        win: WinId,
        peer: super::window::PeerId,
        name: crate::window_link::names::WindowName,
        port: Rc<dyn PeerPort>,
    ) {
        let batch = self.decide.borrow_mut().attach_peer(win, peer, name, port);
        self.submit(batch);
    }

    /// `win`'s link to `peer` has gone ([`Controller::detach_peer`]).
    pub fn detach_peer(&self, win: WinId, peer: super::window::PeerId) {
        let batch = self.decide.borrow_mut().detach_peer(win, peer);
        self.submit(batch);
    }

    /// A kill with no y/n ([`Controller::kill_confirmed`]). The grid probe's (`--features probe`), which kills a chat
    /// view it opened without asking; a terminal whose shell ended posts its end instead, which the controller judges.
    pub fn kill_confirmed(&self, win: WinId, module: &ModuleId) {
        let batch = self.decide.borrow_mut().kill_confirmed(win, module);
        self.submit(batch);
    }

    /// What the window's chrome names things by ([`Controller::set_titles`]).
    pub fn set_titles(&self, win: WinId, titles: super::chrome::ChromeTitles) {
        let batch = self.decide.borrow_mut().set_titles(win, titles);
        self.submit(batch);
    }

    /// The workspace chooser opened from the top bar's workspace name: the window's prefix waits for its key
    /// ([`Controller::await_workspace`]).
    pub fn await_workspace(&self, win: WinId) {
        let batch = self.decide.borrow_mut().await_workspace(win);
        self.submit(batch);
    }

    #[cfg(test)]
    pub(super) fn test_batch(&self, decide: impl FnOnce(&mut Controller) -> Batch) {
        let batch = decide(&mut self.decide.borrow_mut());
        self.submit(batch);
    }
}

impl Allocator for Driver<Controller> {
    fn allocate(&self, win: WinId, size: Size) -> Rc<Arrangement> {
        let answer = match self.decide.try_borrow_mut() {
            Ok(mut controller) => controller.allocate(win, size),
            Err(_) => {
                self.note(
                    "[wm] BUG: the grid allocated while the controller decides; its last arrangement kept".into(),
                );
                None
            }
        };
        let Some((arrangement, settled)) = answer else {
            return self.arranged.borrow().get(&win).cloned().unwrap_or_default();
        };
        if settled.is_some() {
            // Heard from the idle: no batch is computed inside an allocation.
            self.post_deferred(HostEvent::Settled { win });
        }
        self.arranged.borrow_mut().insert(win, arrangement.clone());
        arrangement
    }

    fn min_size(&self, win: WinId) -> Size {
        match self.decide.try_borrow() {
            Ok(controller) => controller.min_size(win),
            Err(_) => Size::default(),
        }
    }
}

impl<C: Decide> HostSink for Driver<C> {
    fn post(&self, event: HostEvent) {
        Driver::post(self, event);
    }
    fn post_deferred(&self, event: HostEvent) {
        Driver::post_deferred(self, event);
    }
    fn close_requested(&self, win: WinId, by: CloseBy) -> CloseVerdict {
        Driver::close_requested(self, win, by)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keymap::{key::KeyName, prefix::Press};
    use crate::layout::Size;
    use crate::tabs::ViewId;
    use crate::wm::fake::{Call, FakeHost, FakeScheduler, Logged, ScriptedDecide};
    use crate::wm::window::FrameMetrics;
    use std::rc::Weak;
    use std::time::Duration;

    const WIN: WinId = WinId::MAIN;

    /// One entry of a run's merged record: what the host did and what the decider was asked, in the order they
    /// happened.
    #[derive(Debug, Clone, PartialEq)]
    enum Seen {
        Host(Logged),
        Asked(Call),
    }

    /// Places the decider's calls among the host's log by the log's length when they happened.
    struct Trace {
        host: Rc<FakeHost>,
        marks: RefCell<Vec<(usize, Seen)>>,
    }

    impl Trace {
        fn mark(&self, seen: Seen) {
            let at = self.host.log().len();
            self.marks.borrow_mut().push((at, seen));
        }
        fn timeline(&self) -> Vec<Seen> {
            let marks = self.marks.borrow();
            let mut marks = marks.iter().peekable();
            let mut out = vec![];
            for (i, entry) in self.host.log().into_iter().enumerate() {
                while let Some((_, seen)) = marks.next_if(|(at, _)| *at <= i) {
                    out.push(seen.clone());
                }
                out.push(Seen::Host(entry));
            }
            out.extend(marks.map(|(_, seen)| seen.clone()));
            out
        }
    }

    /// A scripted decider that also writes each call into the shared record.
    struct Traced {
        inner: ScriptedDecide,
        trace: Rc<Trace>,
    }

    impl Decide for Traced {
        fn handle(&mut self, event: HostEvent) -> Batch {
            self.trace.mark(Seen::Asked(Call::Handle(event.clone())));
            self.inner.handle(event)
        }
        fn key(&mut self, win: WinId, stage: &KeyStage, press: &KeyPress) -> (Verdict, Batch) {
            self.trace
                .mark(Seen::Asked(Call::Key(win, stage.module().cloned(), press.clone())));
            self.inner.key(win, stage, press)
        }
        fn close_requested(&mut self, win: WinId, by: CloseBy) -> (CloseVerdict, Batch) {
            self.trace.mark(Seen::Asked(Call::Close(win, by.clone())));
            self.inner.close_requested(win, by)
        }
        fn run(&mut self, win: WinId, action: Action, origin: Origin) -> Batch {
            self.trace
                .mark(Seen::Asked(Call::Run(win, action.clone(), origin.clone())));
            self.inner.run(win, action, origin)
        }
    }

    struct Rig {
        host: Rc<FakeHost>,
        scheduler: Rc<FakeScheduler>,
        trace: Rc<Trace>,
        driver: Rc<Driver<Traced>>,
    }

    impl Rig {
        fn weak(&self) -> Weak<Driver<Traced>> {
            Rc::downgrade(&self.driver)
        }
        fn asked(&self) -> Vec<Call> {
            self.driver.query(|d| d.inner.calls())
        }
        fn host_ops(&self) -> Vec<HostOp> {
            self.host
                .log()
                .into_iter()
                .filter_map(|l| match l {
                    Logged::Op(op, _) => Some(op),
                    _ => None,
                })
                .collect()
        }
    }

    /// The host and the scheduler first, the driver from them, then both attached to the driver.
    fn rig(script: impl FnOnce(&mut ScriptedDecide)) -> Rig {
        let host = FakeHost::new();
        let scheduler = FakeScheduler::new();
        let trace = Rc::new(Trace {
            host: host.clone(),
            marks: RefCell::new(vec![]),
        });
        let mut inner = ScriptedDecide::default();
        script(&mut inner);
        let driver = Driver::new(
            Traced {
                inner,
                trace: trace.clone(),
            },
            host.clone(),
            scheduler.clone(),
        );
        let sink: Rc<dyn HostSink> = driver.clone();
        host.attach(Rc::downgrade(&sink));
        scheduler.attach(Rc::downgrade(&sink));
        Rig {
            host,
            scheduler,
            trace,
            driver,
        }
    }

    #[test]
    fn a_query_while_the_controller_is_borrowed_logs_a_bug_and_returns_the_default() {
        let r = rig(|_| {});
        let _borrow = r.driver.decide.borrow_mut();
        let answer: Option<f32> = r.driver.query(|_| panic!("a borrowed controller must not be queried"));
        assert_eq!(answer, None);
        assert_eq!(r.driver.notes(), ["[wm] BUG: query while the controller is borrowed"]);
    }

    #[test]
    fn a_gate_query_while_borrowed_refuses_without_calling_the_controller() {
        let r = rig(|_| {});
        let _borrow = r.driver.decide.borrow_mut();
        let answer: Result<(), String> = r.driver.query_or(Err("layout is changing".into()), |_| {
            panic!("a borrowed controller must not be queried")
        });
        assert_eq!(answer, Err("layout is changing".into()));
        assert_eq!(r.driver.notes(), ["[wm] BUG: query while the controller is borrowed"]);
    }

    #[test]
    fn a_scale_echo_during_set_scale_waits_for_the_whole_batch() {
        let scale = HostOp::Editor(EditorCall::SetScale { win: WIN, scale: 1.1 });
        let font = HostOp::Chat(ChatOp::SetFontSize { win: WIN, px: 15.4 });
        let zoom = HostOp::Plugin(PluginOp::SetZoom {
            win: WIN,
            module: x(),
            level: 1.1,
        });
        let echo = HostEvent::Editor(EditorEvent::ScaleReported { win: WIN, scale: 1.1 });
        let start = HostEvent::TextSize(TextSizeEvent::Both {
            win: WIN,
            step: super::super::text_size::TextStep::Larger,
        });
        let r = rig(|d| {
            let mut batch = Batch::default();
            for op in [&scale, &font, &zoom] {
                batch.push_effect(op.clone());
            }
            d.then_handle(batch);
        });
        let weak = r.weak();
        let posted = echo.clone();
        r.host.on_apply(move |_, _, op| {
            let driver = weak.upgrade().unwrap();
            assert!(!driver
                .query(|d| d.inner.calls())
                .contains(&Call::Handle(posted.clone())));
            if matches!(op, HostOp::Editor(EditorCall::SetScale { .. })) {
                driver.post(posted.clone());
            }
        });
        r.driver.post(start.clone());
        assert_eq!(r.asked(), [Call::Handle(start), Call::Handle(echo.clone())]);
        let timeline = r.trace.timeline();
        let last_write = position(&timeline, |seen| {
            *seen == Seen::Host(Logged::Op(zoom.clone(), Applied::Done))
        });
        let heard = position(&timeline, |seen| *seen == Seen::Asked(Call::Handle(echo.clone())));
        assert!(
            last_write < heard,
            "the echo is decided after every write: {timeline:?}"
        );
        assert!(
            r.driver.notes().is_empty(),
            "queries during apply find no controller borrow"
        );
    }

    fn a() -> ModuleId {
        ModuleId::agent()
    }
    fn x() -> ModuleId {
        ModuleId::lua("x")
    }
    fn y() -> ModuleId {
        ModuleId::lua("y")
    }
    fn hidden() -> ModuleId {
        ModuleId::editor()
    }

    fn page(name: &str) -> SurfaceKind {
        SurfaceKind::LuaPanel {
            url: format!("eitri://{name}"),
            title: name.into(),
        }
    }

    fn focus(module: ModuleId) -> HostOp {
        HostOp::Focus {
            win: WIN,
            to: FocusTo::Module(module),
            on_grab: vec![],
        }
    }
    fn map(module: ModuleId) -> HostOp {
        HostOp::Map { win: WIN, module }
    }
    fn native_focus(module: ModuleId, op_index: Option<u32>) -> HostEvent {
        HostEvent::NativeFocus {
            win: WIN,
            owner: Some(FocusOwner::Module(module.clone())),
            chain: FocusChain(vec![module]),
            active: true,
            op_index,
        }
    }
    fn is_native_focus_on(event: &HostEvent, module: &ModuleId) -> bool {
        matches!(event, HostEvent::NativeFocus { owner: Some(FocusOwner::Module(m)), .. } if m == module)
    }
    fn layout(ops: Vec<HostOp>) -> Batch {
        let mut batch = Batch::default();
        for op in ops {
            batch.push_layout(op);
        }
        batch
    }
    fn press() -> KeyPress {
        KeyPress {
            keycode: 38,
            press: Press::Other,
            key: Some(KeyName::Char('a')),
            mods: Mods::default(),
            super_or_hyper: false,
            super_or_meta: false,
        }
    }
    fn position(timeline: &[Seen], what: impl Fn(&Seen) -> bool) -> usize {
        timeline
            .iter()
            .position(what)
            .unwrap_or_else(|| panic!("not in the record: {timeline:#?}"))
    }

    #[test]
    fn the_driver_queues_an_event_posted_while_a_batch_applies() {
        let r = rig(|d| d.then_run(layout(vec![focus(a())])));
        r.host.with_surface(WIN, a(), page("a"), true);
        r.driver.run(WIN, Action::FocusParent, Origin::Lua);
        assert_eq!(
            r.trace.timeline(),
            vec![
                Seen::Asked(Call::Run(WIN, Action::FocusParent, Origin::Lua)),
                Seen::Host(Logged::Op(focus(a()), Applied::Done)),
                Seen::Host(Logged::Event(native_focus(a(), None))),
                Seen::Asked(Call::Handle(native_focus(a(), Some(0)))),
            ],
            "the focus fact posted during the grab is decided after the batch, stamped with the grab's index"
        );
    }

    #[test]
    fn a_deferred_event_never_runs_inside_the_call_that_posted_it() {
        let r = rig(|_| {});
        let resized = |w| HostEvent::Resized {
            win: WIN,
            metrics: FrameMetrics {
                size: Size { w, h: 10 },
                scale: 1.0,
            },
        };
        r.driver.post_deferred(resized(1));
        r.driver.post_deferred(resized(2));
        assert!(r.asked().is_empty(), "a deferred event waits for the idle");
        assert_eq!(
            r.scheduler.armed().len(),
            1,
            "one idle serves every deferred event posted before it fires"
        );
        r.scheduler.advance(Duration::ZERO);
        assert_eq!(
            r.asked(),
            vec![Call::Handle(resized(1)), Call::Handle(resized(2))],
            "the idle hands the deferred events over in order, and its own timer is the driver's"
        );
        r.driver.post_deferred(resized(3));
        assert_eq!(
            r.scheduler.armed().len(),
            1,
            "a later deferred event arms the idle again"
        );
    }

    #[test]
    fn a_nested_key_batch_runs_after_the_events_queued_before_it() {
        let r = rig(|d| {
            d.then_run(layout(vec![focus(a())]));
            d.then_key(Verdict::Consumed, layout(vec![map(x())]));
        });
        r.host.with_surface(WIN, a(), page("a"), true);
        let weak = r.weak();
        r.host.on_apply(move |_, i, _| {
            if i == 0 {
                weak.upgrade().unwrap().key(WIN, &KeyStage::Window, press());
            }
        });
        r.driver.run(WIN, Action::FocusParent, Origin::Lua);
        let timeline = r.trace.timeline();
        let handled = position(
            &timeline,
            |s| matches!(s, Seen::Asked(Call::Handle(e)) if is_native_focus_on(e, &a())),
        );
        let mapped = position(&timeline, |s| *s == Seen::Host(Logged::Op(map(x()), Applied::Done)));
        assert!(
            handled < mapped,
            "the focus fact queued first is decided before the nested key's batch applies: {timeline:#?}"
        );
    }

    #[test]
    fn a_key_posted_while_a_batch_applies_is_answered_and_logged() {
        let r = rig(|d| {
            d.then_run(layout(vec![HostOp::Allocate { win: WIN }]));
            d.then_key(Verdict::Consumed, Batch::default());
            d.then_key(Verdict::LetThrough, Batch::default());
            d.then_key(Verdict::PassThrough, Batch::default());
        });
        let seen = Rc::new(RefCell::new(vec![]));
        let (weak, out) = (r.weak(), seen.clone());
        r.host.on_apply(move |_, i, _| {
            if i == 0 {
                let driver = weak.upgrade().unwrap();
                for _ in 0..2 {
                    let verdict = driver.key(WIN, &KeyStage::Window, press());
                    out.borrow_mut()
                        .push((verdict, driver.nested_keys(), driver.notes().len()));
                }
            }
        });
        r.driver.run(WIN, Action::FocusParent, Origin::Lua);
        assert_eq!(
            *seen.borrow(),
            vec![(Verdict::Consumed, 1, 1), (Verdict::LetThrough, 2, 1)],
            "each nested key is answered at once and counted; only the first is logged"
        );
        assert!(r.driver.notes()[0].starts_with("[wm] BUG: nested key"));
        assert_eq!(r.driver.key(WIN, &KeyStage::Window, press()), Verdict::PassThrough);
        assert_eq!(r.driver.nested_keys(), 2, "a key outside any batch is not nested");
    }

    #[test]
    fn a_close_request_logs_the_nested_count() {
        let r = rig(|d| {
            d.then_close(CloseVerdict::Stop, Batch::default());
            d.then_run(layout(vec![HostOp::Allocate { win: WIN }]));
            d.then_close(CloseVerdict::Proceed, Batch::default());
        });
        assert_eq!(r.driver.close_requested(WIN, CloseBy::User), CloseVerdict::Stop);
        assert_eq!(r.driver.notes(), vec!["[wm] nested=0".to_string()]);
        let weak = r.weak();
        r.host.on_apply(move |_, i, _| {
            if i == 0 {
                weak.upgrade().unwrap().key(WIN, &KeyStage::Window, press());
            }
        });
        r.driver.run(WIN, Action::FocusParent, Origin::Lua);
        assert_eq!(
            r.driver.close_requested(WIN, CloseBy::Compositor),
            CloseVerdict::Proceed
        );
        assert_eq!(r.driver.notes().last().map(String::as_str), Some("[wm] nested=1"));
    }

    #[test]
    fn a_window_closed_by_its_teardown_logs_the_nested_count() {
        let r = rig(|d| {
            d.then_run(layout(vec![HostOp::CloseWindow { win: WIN }]));
        });
        r.driver.run(WIN, Action::FocusParent, Origin::Lua);
        assert_eq!(r.driver.notes(), vec!["[wm] nested=0".to_string()]);
    }

    #[test]
    fn a_refused_focus_runs_no_arrive() {
        let arrive = HostOp::Chat(ChatOp::Arrive {
            win: WIN,
            view: ViewId::BASE,
            card_first: true,
        });
        let r = rig(|d| {
            d.then_run(layout(vec![
                HostOp::Focus {
                    win: WIN,
                    to: FocusTo::Module(hidden()),
                    on_grab: vec![arrive.clone()],
                },
                map(y()),
            ]))
        });
        r.host.with_surface(WIN, hidden(), page("hidden"), false);
        r.driver.run(WIN, Action::FocusParent, Origin::Lua);
        assert!(!r.host_ops().contains(&arrive), "the tail of a refused grab never runs");
        let timeline = r.trace.timeline();
        let mapped = position(&timeline, |s| *s == Seen::Host(Logged::Op(map(y()), Applied::Done)));
        let refused = position(&timeline, |s| {
            matches!(
                s,
                Seen::Asked(Call::Handle(HostEvent::Refused { what: OpTag::Focus, .. }))
            )
        });
        assert!(
            mapped < refused,
            "the batch carries on past the refusal, which is decided after it: {timeline:#?}"
        );
    }

    #[test]
    fn lua_in_a_grab_tail_waits_for_the_end_of_the_batch() {
        let arrive = HostOp::Chat(ChatOp::Arrive {
            win: WIN,
            view: ViewId::BASE,
            card_first: true,
        });
        let from_tail = HostOp::Lua(LuaOp::Emit {
            event: "from_tail".into(),
        });
        let from_batch = HostOp::Lua(LuaOp::Emit {
            event: "from_batch".into(),
        });
        let r = rig(|d| {
            let mut batch = layout(vec![
                HostOp::Focus {
                    win: WIN,
                    to: FocusTo::Module(a()),
                    on_grab: vec![from_tail.clone(), arrive.clone()],
                },
                map(y()),
            ]);
            batch.push_effect(from_batch.clone());
            d.then_run(batch);
        });
        r.host.with_surface(WIN, a(), page("a"), true);
        r.driver.run(WIN, Action::FocusParent, Origin::Lua);
        assert_eq!(
            r.host_ops(),
            vec![
                HostOp::Focus {
                    win: WIN,
                    to: FocusTo::Module(a()),
                    on_grab: vec![from_tail.clone(), arrive.clone()],
                },
                arrive,
                map(y()),
                from_batch,
                from_tail,
            ],
            "the tail's native op follows the grab; its Lua call runs after every native op and the batch's own Lua"
        );
    }

    #[test]
    fn lua_ops_run_after_every_other_op_of_their_batch() {
        let lua = |n: &str| LuaOp::Emit { event: n.into() };
        let invoke = LuaOp::Invoke { command: "c".into() };
        let r = rig(|d| {
            let mut batch = Batch::default();
            batch.push_lua(lua("1"));
            batch.push_layout(HostOp::Lua(lua("2")));
            batch.push_effect(HostOp::Lua(invoke.clone()));
            batch.push_layout(map(x()));
            batch.push_effect(HostOp::Allocate { win: WIN });
            d.then_run(batch);
        });
        r.driver.run(WIN, Action::FocusParent, Origin::Lua);
        assert_eq!(
            r.host_ops(),
            vec![
                map(x()),
                HostOp::Allocate { win: WIN },
                HostOp::Lua(lua("1")),
                HostOp::Lua(lua("2")),
                HostOp::Lua(invoke),
            ]
        );
    }

    /// A driver over the controller holding `layout` as the main window, every module of it hosted and its first
    /// layout batch applied, the fake host's reads given to the controller.
    fn driven(
        layout: crate::layout::Layout,
    ) -> (
        Rc<Driver>,
        Rc<FakeHost>,
        Rc<FakeScheduler>,
        Rc<RefCell<crate::layout::Layout>>,
    ) {
        use crate::wm::window::{EditorPresence, WindowParts, WindowRole, WindowSpec};
        let host = FakeHost::new();
        let scheduler = FakeScheduler::new();
        let mut controller = Controller::new(scheduler.clone());
        controller.set_reads(host.clone());
        let hosted: Vec<_> = layout.modules().iter().map(crate::wm::verbs::rig::decl).collect();
        let layout = Rc::new(RefCell::new(layout));
        controller.open_window(
            WIN,
            WindowSpec {
                role: WindowRole::Main,
                presence: EditorPresence::Owned,
                title: "project".into(),
            },
            WindowParts {
                layout: layout.clone(),
                workspaces: Rc::new(RefCell::new(crate::layout::Workspaces::single())),
                saver: None,
            },
        );
        let driver = Driver::new(controller, host.clone(), scheduler.clone());
        let sink: Rc<dyn HostSink> = driver.clone();
        host.attach(Rc::downgrade(&sink));
        scheduler.attach(Rc::downgrade(&sink));
        driver.first_apply(WIN, hosted);
        (driver, host, scheduler, layout)
    }

    /// `editor | agent` tiled under the float `x`, which holds the keys.
    fn under_a_float() -> crate::layout::Layout {
        use crate::layout::{Axis, Float, FloatRect, Node};
        crate::layout::Layout::from_parts_floating(
            Node::split(Axis::Row, 0.5, Node::Leaf(ModuleId::editor()), Node::Leaf(a())),
            vec![Float {
                id: x(),
                rect: FloatRect {
                    cx: 0.5,
                    cy: 0.5,
                    w: 300,
                    h: 200,
                },
                scratch: false,
            }],
            Default::default(),
            x(),
            Default::default(),
        )
        .unwrap()
    }

    fn frame(size: Size) -> crate::layout::Frame<'static> {
        crate::layout::Frame::new(size, 1)
    }

    #[test]
    fn a_pinned_row_is_settled_in_the_allocation_that_first_shows_it() {
        let (driver, _host, scheduler, layout) = driven(under_a_float());
        // Docked along the bottom edge, the float becomes a pinned row that has never been on screen.
        driver.run(WIN, Action::MoveFar(crate::layout::Direction::Down), Origin::Lua);
        assert!(
            layout.borrow().has_pin_to_settle(),
            "the premise: a pin waits for its first length"
        );
        let size = Size { w: 1920, h: 1040 };
        let want = {
            let mut settled = layout.borrow().clone();
            assert!(crate::layout::settle_pins(&mut settled, &frame(size)));
            crate::layout::arrange(&settled, &frame(size))
        };
        let arrangement = driver.allocate(WIN, size);
        assert_eq!(
            *arrangement, want,
            "the pinned row has the length its ratio gives at this size"
        );
        assert!(!layout.borrow().has_pin_to_settle());
        let settles = |driver: &Driver| {
            driver
                .deferred
                .borrow()
                .iter()
                .filter(|event| matches!(event, HostEvent::Settled { win, .. } if *win == WIN))
                .count()
        };
        assert_eq!(
            settles(&driver),
            1,
            "the settle is posted for the idle, not handled inside the allocation"
        );
        scheduler.advance(Duration::ZERO);
        assert_eq!(settles(&driver), 0, "the idle hands it to the controller");
        driver.allocate(WIN, size);
        assert_eq!(settles(&driver), 0, "a pin settles once");
    }

    #[test]
    fn a_resize_allocates_with_the_new_size_in_the_same_pass() {
        let (driver, _host, _scheduler, layout) = driven(under_a_float());
        let small = Size { w: 1280, h: 721 };
        let large = Size { w: 1920, h: 1040 };
        let first = driver.allocate(WIN, small);
        assert_eq!(*first, crate::layout::arrange(&layout.borrow(), &frame(small)));
        let second = driver.allocate(WIN, large);
        assert_eq!(*second, crate::layout::arrange(&layout.borrow(), &frame(large)));
        assert_ne!(first, second);
        assert_eq!(driver.query(|c| c.allocated(WIN)), Some(large));
        assert_eq!(
            driver.min_size(WIN),
            crate::layout::min_size(&layout.borrow(), &frame(crate::wm::grid::UNALLOCATED)),
            "the minimum is measured in the frame the grid reports"
        );
    }

    #[test]
    fn an_allocation_while_the_controller_decides_keeps_the_last_arrangement() {
        let (driver, _host, _scheduler, _layout) = driven(under_a_float());
        let size = Size { w: 1280, h: 721 };
        let first = driver.allocate(WIN, size);
        let held = driver.decide.borrow_mut();
        let again = driver.allocate(WIN, Size { w: 1920, h: 1040 });
        drop(held);
        assert!(Rc::ptr_eq(&first, &again));
        assert!(driver
            .notes()
            .iter()
            .any(|line| line.starts_with("[wm] BUG: the grid allocated")));
    }

    #[test]
    fn the_driver_drives_the_controller_by_default() {
        let host = FakeHost::new();
        let scheduler = FakeScheduler::new();
        let driver = Driver::new(Controller::new(scheduler.clone()), host.clone(), scheduler.clone());
        let sink: Rc<dyn HostSink> = driver.clone();
        host.attach(Rc::downgrade(&sink));
        scheduler.attach(Rc::downgrade(&sink));
        driver.run(WIN, Action::PanelReload, Origin::Lua);
        assert!(
            host.log().is_empty(),
            "a window the controller does not hold has nothing to apply"
        );
        driver.post(HostEvent::Pair(PairEvent::RestoreFailed { win: WIN }));
        assert!(
            host.log().is_empty(),
            "a fact for a window the controller does not hold changes nothing"
        );
    }
}
