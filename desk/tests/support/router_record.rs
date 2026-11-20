//! The host op router's record: every call a host makes, while it applies an op, on the toolkit-free collaborators
//! a shell gives it (the surface maker, the services, the grid, Lua, the pair, HINT, the page beside nvim, the base
//! page, the project chooser, the first-start screen), written as one line of a shared trace between the op and its
//! answer, with every event the router posts. What the toolkit itself does (which widget holds the keys, whether the
//! window is active) is the toolkit half's own, so `NativeFocus` and `Active` are left out.
//!
//! `router_conformance.trace` is the GTK host's run of these scripts with these recorders (only that run may write it);
//! any host that carries the ops out through the same collaborators must write it again, line for line.
//!
//! Included by `#[path]` from the suites that drive a host; each includer names, in a module `traits` beside this
//! one, where the collaborator traits live for it.

#![allow(dead_code)]

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use eitri_core::layout::{Direction, ModuleId};
use eitri_core::tabs::{TabId, ViewId};
use eitri_core::window_link::names::WindowName;
use eitri_core::window_link::{HubAsk, Msg, Shape};
use eitri_core::wm::conformance::{self, Fixture, Script};
use eitri_core::wm::moves::MoveKind;
use eitri_core::wm::{
    ActivationToken, Applied, BesideOp, ChatOp, ChooserEnv, CleanupOp, CloseBy, CloseVerdict, Descriptors, DropWhy,
    EditorCall, EditorPresence, EditorSpawn, FocusTo, HintOp, Host, HostEvent, HostOp, HostSink, LuaOp, MonitorId,
    OnboardingOp, OpTag, PairOp, PeerId, PlacementRole, PluginOp, ProjectOp, SurfaceKind, TermOp, WinId, WindowMode,
    WindowRole, WindowSpec,
};

use super::traits::{
    BasePageMaker, BesidePort, GridPort, HintPort, LinkedWindows, LuaRunner, ProjectHost, RestartRefused, ServiceSet,
    SurfaceMaker,
};

/// One line per op, call, event and answer, in the order they happened.
pub type Trace = Rc<RefCell<Vec<String>>>;

fn say(trace: &Trace, line: String) {
    trace.borrow_mut().push(line);
}

/// The trace file the GTK host's run wrote.
pub const TRACE_FILE: &str = "router_conformance.trace";

/// Whether the router, not the toolkit, posts `event`: where the keys are and whether the window is active are
/// reported by the toolkit's own signals.
fn router_event(event: &HostEvent) -> bool {
    !matches!(event, HostEvent::NativeFocus { .. } | HostEvent::Active { .. })
}

/// The sink the record runs post to: each event the router posts becomes a line of the trace.
pub struct TraceSink {
    pub trace: Trace,
    close_requests: Cell<usize>,
}

impl TraceSink {
    pub fn new(trace: &Trace) -> Rc<TraceSink> {
        Rc::new(TraceSink {
            trace: trace.clone(),
            close_requests: Cell::new(0),
        })
    }
}

impl HostSink for TraceSink {
    fn post(&self, event: HostEvent) {
        if router_event(&event) {
            say(&self.trace, format!("  event {event:?}"));
        }
    }
    fn post_deferred(&self, event: HostEvent) {
        self.post(event);
    }
    fn close_requested(&self, win: WinId, by: CloseBy) -> CloseVerdict {
        self.close_requests.set(self.close_requests.get() + 1);
        say(&self.trace, format!("  close requested {win:?} {by:?}"));
        CloseVerdict::Proceed
    }
}

/// Applies `script`'s ops to `host` in order, each between its line and its answer.
pub fn record(host: &dyn Host, trace: &Trace, config: &str, script: &Script) {
    say(trace, format!("== {config} {}", script.name));
    for (i, op) in script.ops.iter().enumerate() {
        say(trace, format!("op {i} {:?}", OpTag::of(op)));
        let applied = host.apply(op);
        say(trace, format!("  = {applied:?}"));
    }
}

/// Whether a token came: a toolkit's activation tokens differ from run to run, the router's handling of one does not.
fn token(token: &Option<String>) -> &'static str {
    if token.is_some() {
        "token"
    } else {
        "no token"
    }
}

/// The maker a host builds surfaces through, written down around the maker that builds them.
pub struct RecMaker<M> {
    pub inner: M,
    pub trace: Trace,
}

impl<M: SurfaceMaker> SurfaceMaker for RecMaker<M> {
    fn mapped(&self, module: &ModuleId, mapped: bool) {
        say(&self.trace, format!("  maker.mapped {module} {mapped}"));
        self.inner.mapped(module, mapped);
    }
    fn create(&self, win: WinId, module: &ModuleId, kind: &SurfaceKind) -> Result<(), String> {
        say(&self.trace, format!("  maker.create {win:?} {module} {kind:?}"));
        self.inner.create(win, module, kind)
    }
    fn drop_surface(&self, win: WinId, module: &ModuleId, why: &DropWhy) -> Result<(), String> {
        say(&self.trace, format!("  maker.drop {win:?} {module} {why:?}"));
        self.inner.drop_surface(win, module, why)
    }
    fn prepare(&self, win: WinId, module: &ModuleId, shape: &Shape, size: (u32, u32)) -> Result<(), String> {
        say(
            &self.trace,
            format!("  maker.prepare {win:?} {module} {shape:?} {size:?}"),
        );
        self.inner.prepare(win, module, shape, size)
    }
    fn adopt(
        &self,
        win: WinId,
        module: &ModuleId,
        kind: MoveKind,
        record: &serde_json::Value,
        fds: Vec<std::os::fd::OwnedFd>,
    ) -> Result<(), String> {
        say(
            &self.trace,
            format!("  maker.adopt {win:?} {module} {kind:?} {record} fds={}", fds.len()),
        );
        self.inner.adopt(win, module, kind, record, fds)
    }
}

/// The window's services, written down around the set that carries them out.
pub struct RecServices<S> {
    pub inner: S,
    pub trace: Trace,
}

impl<S: ServiceSet> ServiceSet for RecServices<S> {
    fn editor(&self, op: &EditorCall) -> Result<(), String> {
        say(&self.trace, format!("  services.editor {op:?}"));
        self.inner.editor(op)
    }
    fn restart_editor(&self, spawn: &EditorSpawn) -> Result<(), RestartRefused> {
        say(&self.trace, format!("  services.restart_editor {spawn:?}"));
        self.inner.restart_editor(spawn)
    }
    fn terminal(&self, op: &TermOp) -> Result<(), String> {
        say(&self.trace, format!("  services.terminal {op:?}"));
        self.inner.terminal(op)
    }
    fn plugin(&self, op: &PluginOp) -> Result<(), String> {
        say(&self.trace, format!("  services.plugin {op:?}"));
        self.inner.plugin(op)
    }
    fn chat(&self, op: &ChatOp) -> Result<(), String> {
        say(&self.trace, format!("  services.chat {op:?}"));
        self.inner.chat(op)
    }
    fn agents_scan(
        &self,
        win: WinId,
        step: eitri_core::wm::agents_round::RoundKey,
        at: Option<&ModuleId>,
    ) -> Result<(), String> {
        say(&self.trace, format!("  services.agents_scan {win:?} {step:?} {at:?}"));
        self.inner.agents_scan(win, step, at)
    }
    fn cleanup(&self, op: &CleanupOp) -> Result<(), String> {
        say(&self.trace, format!("  services.cleanup {op:?}"));
        self.inner.cleanup(op)
    }
    fn open_answer(&self, id: u64, accepted: bool) -> Result<(), String> {
        say(&self.trace, format!("  services.open_answer {id} {accepted}"));
        self.inner.open_answer(id, accepted)
    }
    fn pair(&self, op: &PairOp) -> Result<(), String> {
        say(&self.trace, format!("  services.pair {op:?}"));
        self.inner.pair(op)
    }
    fn order_base_tabs(&self, tabs: &[TabId], active: Option<TabId>) -> Result<(), String> {
        say(&self.trace, format!("  services.order_base_tabs {tabs:?} {active:?}"));
        self.inner.order_base_tabs(tabs, active)
    }
}

/// The window's grid, written down around the grid that lays it out.
pub struct RecGrid<G> {
    pub inner: Rc<G>,
    pub trace: Trace,
}

impl<G: GridPort> GridPort for RecGrid<G> {
    fn size(&self) -> eitri_core::layout::Size {
        self.inner.size()
    }
    fn handle_px(&self) -> i32 {
        self.inner.handle_px()
    }
    fn strip_px(&self) -> i32 {
        self.inner.strip_px()
    }
    fn module_min(&self, module: &ModuleId) -> eitri_core::layout::Size {
        self.inner.module_min(module)
    }
    fn queue_allocation(&self) {
        say(&self.trace, "  grid.queue_allocation".into());
        self.inner.queue_allocation();
    }
    fn restack_floats(&self, order: &[ModuleId]) {
        let names: Vec<&str> = order.iter().map(ModuleId::as_str).collect();
        say(&self.trace, format!("  grid.restack_floats {names:?}"));
        self.inner.restack_floats(order);
    }
    fn probe_places(&self, module: &ModuleId) -> bool {
        say(&self.trace, format!("  grid.probe_places {module}"));
        self.inner.probe_places(module)
    }
}

/// The project chooser's work, written down around the chooser that does it.
pub struct RecProjects<P> {
    pub inner: P,
    pub trace: Trace,
}

impl<P: ProjectHost> ProjectHost for RecProjects<P> {
    fn apply(&self, op: &ProjectOp) -> Applied {
        say(&self.trace, format!("  projects.apply {op:?}"));
        self.inner.apply(op)
    }
    fn open(&self, win: WinId, target: &std::path::Path, tok: Option<String>) -> Applied {
        say(
            &self.trace,
            format!("  projects.open {win:?} {} {}", target.display(), token(&tok)),
        );
        self.inner.open(win, target, tok)
    }
    fn focus(&self, win: WinId) -> bool {
        say(&self.trace, format!("  projects.focus {win:?}"));
        self.inner.focus(win)
    }
    fn env(&self, win: WinId) -> ChooserEnv {
        self.inner.env(win)
    }
}

/// The window's Lua kernel: every op runs.
pub struct RecLua(pub Trace);

impl LuaRunner for RecLua {
    fn run(&self, op: &LuaOp) -> Result<(), String> {
        say(&self.0, format!("  lua.run {op:?}"));
        Ok(())
    }
}

/// The pair's code: every start, send and hand-over goes.
pub struct RecLinked(pub Trace);

impl LinkedWindows for RecLinked {
    fn open(&self, win: WinId, name: &WindowName, tok: Option<String>) -> Result<(), String> {
        say(&self.0, format!("  linked.open {win:?} {name:?} {}", token(&tok)));
        Ok(())
    }
    fn end_peer(&self, win: WinId, peer: PeerId) -> Result<(), String> {
        say(&self.0, format!("  linked.end_peer {win:?} {peer:?}"));
        Ok(())
    }
    fn restore(&self, win: WinId, name: &WindowName) -> Result<(), String> {
        say(&self.0, format!("  linked.restore {win:?} {name:?}"));
        Ok(())
    }
    fn module_window_open(&self, _name: &WindowName) -> bool {
        false
    }
    fn open_module(&self, win: WinId, name: &WindowName, tok: Option<String>, keep_keys: bool) -> Result<(), String> {
        say(
            &self.0,
            format!(
                "  linked.open_module {win:?} {name:?} {} keep_keys={keep_keys}",
                token(&tok)
            ),
        );
        Ok(())
    }
    fn send(&self, win: WinId, peer: PeerId, msg: Msg) -> Result<(), String> {
        say(&self.0, format!("  linked.send {win:?} {peer:?} {}", msg_line(&msg)));
        Ok(())
    }
    fn send_to_hub(&self, win: WinId, msg: Msg) -> Result<(), String> {
        say(&self.0, format!("  linked.send_to_hub {win:?} {}", msg_line(&msg)));
        Ok(())
    }
    fn hand_over(
        &self,
        win: WinId,
        from: PeerId,
        agent: &eitri_core::agent_backend::AgentId,
        session: &str,
        to: &WindowName,
        tok: Option<String>,
    ) -> Result<(), String> {
        say(
            &self.0,
            format!(
                "  linked.hand_over {win:?} {from:?} {agent:?} {session} {to:?} {}",
                token(&tok)
            ),
        );
        Ok(())
    }
    fn start_editor(&self, win: WinId) -> Result<(), String> {
        say(&self.0, format!("  linked.start_editor {win:?}"));
        Ok(())
    }
}

/// A message as the trace writes it: a hub ask's token is the toolkit's, so only whether one came.
fn msg_line(msg: &Msg) -> String {
    match msg {
        Msg::HubAsk { ask, token: tok } => format!("HubAsk {ask:?} {}", token(tok)),
        msg => format!("{msg:?}"),
    }
}

/// HINT's digits and session.
pub struct RecHints(pub Trace);

impl HintPort for RecHints {
    fn show_digits(&self, win: WinId, digits: &[(ModuleId, u8)]) {
        say(&self.0, format!("  hints.show_digits {win:?} {digits:?}"));
    }
    fn hide_digits(&self, win: WinId) {
        say(&self.0, format!("  hints.hide_digits {win:?}"));
    }
    fn toggle(&self, win: WinId, keycode: u32) {
        say(&self.0, format!("  hints.toggle {win:?} {keycode}"));
    }
}

/// The page window's carrier: each op is taken, its later refusal never comes.
pub struct RecBeside(pub Trace);

impl BesidePort for RecBeside {
    fn carry_out(&self, op: &BesideOp, _later: Box<dyn FnOnce(String)>) -> Result<(), String> {
        say(&self.0, format!("  beside.carry_out {op:?}"));
        Ok(())
    }
}

/// The base page's builder.
pub struct RecBasePage(pub Trace);

impl BasePageMaker for RecBasePage {
    fn build(&self) {
        say(&self.0, "  base_page.build".into());
    }
}

/// The first-start screen's work, as the shell's function carries it out: started.
pub fn rec_onboarding(trace: &Trace) -> Rc<dyn Fn(&OnboardingOp) -> Applied> {
    let trace = trace.clone();
    Rc::new(move |op| {
        say(&trace, format!("  onboarding {op:?}"));
        Applied::Done
    })
}

/// The stage's own script, beside the shared ones: the keys given twice to the module that holds them.
pub fn focus_twice(f: &Fixture) -> Script {
    let focus = || HostOp::Focus {
        win: f.win,
        to: FocusTo::Module(f.a.clone()),
        on_grab: vec![],
    };
    Script {
        name: "focus_on_the_current_owner_still_reports_native_focus",
        ops: vec![focus(), focus()],
        expect: vec![],
    }
}

/// The scripts a host runs on its stage, in the record's order: the shared ones, `own` (the stage's
/// [`focus_twice`] with what that host expects of it), the refusals a host with no agents layer gives, and the two
/// teardowns.
pub fn scripts(f: &Fixture, own: Script) -> Vec<Script> {
    conformance::scripts(f)
        .into_iter()
        .chain(conformance::pair_refusal_scripts(f))
        .chain([
            own,
            conformance::join_back_refusal_script(f.win, "the tab could not join the base view"),
            conformance::agents_scan_refusal_script(f.win),
            conformance::teardown_script(f.win),
            conformance::teardown_script_for(
                f.win,
                Some(CleanupOp::EditorRecord {
                    token: Some("0123456789abcdef".into()),
                    seq: 1,
                }),
            ),
        ])
        .collect()
}

/// What the record runs: [`scripts`] and then [`every_op_once`], each on a fresh stage, first with the stage's own
/// collaborators (`bare`), then with every collaborator a host can be given (`full`).
pub fn record_runs(f: &Fixture) -> Vec<(bool, Script)> {
    [false, true]
        .into_iter()
        .flat_map(|full| {
            scripts(f, focus_twice(f))
                .into_iter()
                .chain([every_op_once(f)])
                .map(move |script| (full, script))
        })
        .collect()
}

/// The name a run's lines carry.
pub fn config(full: bool) -> &'static str {
    if full {
        "full"
    } else {
        "bare"
    }
}

/// Holds `trace` to the kept file at `path`, line for line. With `EITRI_WRITE_ROUTER_TRACE=1` and `may_write` (only
/// the GTK host's run, whose record the file is) it writes the file there instead.
pub fn check_trace(trace: &Trace, path: &std::path::Path, may_write: bool) -> Result<(), String> {
    let written: String = trace.borrow().iter().map(|line| format!("{line}\n")).collect();
    if may_write && std::env::var_os("EITRI_WRITE_ROUTER_TRACE").is_some_and(|v| v == "1") {
        std::fs::write(path, &written).map_err(|e| format!("writing {}: {e}", path.display()))?;
        println!(
            "router record: wrote {} ({} lines)",
            path.display(),
            trace.borrow().len()
        );
        return Ok(());
    }
    let kept = std::fs::read_to_string(path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    if kept == written {
        return Ok(());
    }
    let first = kept
        .lines()
        .zip(written.lines())
        .position(|(a, b)| a != b)
        .unwrap_or_else(|| kept.lines().count().min(written.lines().count()));
    Err(format!(
        "the router's record differs from {} at line {}: kept {:?}, now {:?}",
        path.display(),
        first + 1,
        kept.lines().nth(first),
        written.lines().nth(first)
    ))
}

/// A window the stage never opens.
pub const ELSEWHERE: WinId = WinId(9);

/// One op of every kind the shared scripts leave out, and the ones they hold carried out once more on the stage's
/// window, each refusal path a host decides alone taken once (a window it does not hold, a module with no surface).
/// The window's close comes last.
pub fn every_op_once(f: &Fixture) -> Script {
    let win = f.win;
    let terminal2 = ModuleId::instance(eitri_core::layout::ModuleKind::Terminal, 2).expect("a terminal's number");
    let spawn = EditorSpawn {
        env: vec![],
        args: vec![],
        cwd: None,
        clean: true,
        background: None,
        open: vec![],
    };
    let focus = |win: WinId, to: FocusTo| HostOp::Focus {
        win,
        to,
        on_grab: vec![],
    };
    let ops = vec![
        focus(win, FocusTo::TopBarItem(0)),
        focus(win, FocusTo::RenameEntry),
        focus(win, FocusTo::Onboarding),
        focus(win, FocusTo::ProjectChooser),
        focus(win, FocusTo::Nothing),
        focus(ELSEWHERE, FocusTo::Module(f.a.clone())),
        focus(win, FocusTo::Module(terminal2.clone())),
        HostOp::Map {
            win,
            module: terminal2.clone(),
        },
        HostOp::Unmap {
            win,
            module: f.b.clone(),
        },
        HostOp::Map {
            win,
            module: f.b.clone(),
        },
        HostOp::Allocate { win: ELSEWHERE },
        HostOp::Restack {
            win: ELSEWHERE,
            moves: vec![],
        },
        HostOp::DropSurface {
            win,
            module: terminal2.clone(),
            why: DropWhy::Retired,
        },
        HostOp::DropSurface {
            win,
            module: f.b.clone(),
            why: DropWhy::Retired,
        },
        HostOp::Chrome { win: ELSEWHERE, rev: 1 },
        HostOp::Chrome { win, rev: 2 },
        HostOp::Hint(HintOp::ShowDigits(vec![(f.a.clone(), 1), (f.b.clone(), 2)])),
        HostOp::Hint(HintOp::Hide),
        HostOp::Hint(HintOp::Toggle { keycode: 42 }),
        HostOp::Onboarding(OnboardingOp::Detect { win, generation: 1 }),
        HostOp::Project(ProjectOp::List {
            win: ELSEWHERE,
            serial: 1,
        }),
        HostOp::Project(ProjectOp::Open {
            win,
            target: "/work/other".into(),
        }),
        HostOp::Editor(EditorCall::SetFocused { win, focused: true }),
        HostOp::Editor(EditorCall::Quit {
            win,
            lua: "quit".into(),
            generation: 3,
            editor: eitri_core::wm::owned_editor_for_tests(),
        }),
        HostOp::Editor(EditorCall::ReleaseExited { win }),
        HostOp::Editor(EditorCall::Restart {
            win,
            spawn: spawn.clone(),
            editor: eitri_core::wm::owned_editor_for_tests(),
        }),
        HostOp::OpenWindow {
            win,
            spec: WindowSpec {
                role: WindowRole::Main,
                presence: EditorPresence::Owned,
                title: "again".into(),
            },
        },
        HostOp::OpenWindow {
            win: ELSEWHERE,
            spec: WindowSpec {
                role: WindowRole::Main,
                presence: EditorPresence::None,
                title: "other".into(),
            },
        },
        HostOp::Present {
            win: ELSEWHERE,
            activation: Some(ActivationToken("launcher".into())),
        },
        HostOp::OpenAnswer { id: 3, accepted: false },
        HostOp::SetMode {
            win,
            mode: WindowMode {
                immersive: false,
                fullscreen: true,
                top_bar_revealed: false,
            },
        },
        HostOp::PlaceWindow {
            win,
            monitor: Some(MonitorId("DP-1".into())),
            role: PlacementRole::Main,
        },
        HostOp::Lua(LuaOp::Emit { event: "x".into() }),
        HostOp::OpenLinkedWindow { win },
        HostOp::OpenLinkedWindow { win: ELSEWHERE },
        HostOp::RestoreLinkedWindow { win },
        HostOp::StartEditorWindow { win },
        HostOp::RaisePeer { win, peer: PeerId(2) },
        HostOp::RaisePeer {
            win: ELSEWHERE,
            peer: PeerId(2),
        },
        HostOp::ForwardRaise {
            win,
            peer: PeerId(3),
            token: Some("theirs".into()),
        },
        HostOp::OpenModuleWindow {
            win,
            name: WindowName::Chat(2),
            token: None,
            keep_keys: false,
        },
        HostOp::OpenModuleWindow {
            win,
            name: WindowName::Chat(3),
            token: Some("key".into()),
            keep_keys: true,
        },
        HostOp::PeerRun {
            win,
            peer: PeerId(16),
            name: "tab.select".into(),
            opts: vec![("n".into(), 3.into())],
        },
        HostOp::HubAsk {
            win,
            ask: HubAsk::InstanceNew,
        },
        HostOp::CompositorFocus {
            win,
            direction: Direction::Left,
        },
        HostOp::HandOverTab {
            win,
            from: PeerId(16),
            agent: eitri_core::agent_backend::AgentId::Claude,
            session: "s".into(),
            to: WindowName::Chat(4),
            token: Some("theirs".into()),
        },
        HostOp::KeepTab {
            win,
            peer: PeerId(16),
            session: "s".into(),
            why: "no room".into(),
        },
        HostOp::EndPeer { win, peer: PeerId(2) },
        HostOp::Beside(BesideOp::RaiseSelf),
        HostOp::BuildBasePage { win },
        HostOp::PrepareSurface {
            win,
            module: terminal2.clone(),
            shape: Shape::Tiled,
            size: (640, 480),
        },
        HostOp::AdoptMoved {
            win,
            module: terminal2,
            kind: MoveKind::Terminal,
            record: serde_json::json!({"cwd": "/"}),
            fds: Descriptors::default(),
        },
        HostOp::AdoptMoved {
            win,
            module: ModuleId::lua("nowhere"),
            kind: MoveKind::LuaPanel,
            record: serde_json::json!({}),
            fds: Descriptors::default(),
        },
        HostOp::OrderBaseTabs {
            win,
            tabs: vec![TabId(1), TabId(2)],
            active: Some(TabId(2)),
        },
        HostOp::Pair(PairOp::Unrestored { win }),
        HostOp::Chat(ChatOp::PageShown {
            win,
            view: ViewId::BASE,
        }),
        HostOp::Quit,
        HostOp::CloseWindow { win: ELSEWHERE },
        HostOp::CloseWindow { win },
    ];
    Script {
        name: "every_op_once",
        ops,
        expect: vec![],
    }
}
