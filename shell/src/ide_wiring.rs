//! The window's half of the terminal `claude`: `eitri_core::ide_server` and what only a window can give it.
//!
//! One [`IdeWiring`] lives in a window that holds an editor, with `terminal.claude_ide` on. It decides what the
//! terminal's shells get ([`ShellPort`]), hands the listener's events to the [`IdeHost`] on the main context and the
//! host's answers back, and keeps the host's timer.
//!
//! The host says how soon it is owed a tick, and the timer exists only while that is `Some`, so an idle window is woken
//! by nothing. A cadence changes without a client frame (an open answered moves 10 ms to 100 ms), so it is compared
//! after every tick as well as after every wake, and only a change touches the timer.

use std::cell::{Cell, RefCell};
use std::ffi::OsString;
use std::os::fd::RawFd;
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::time::{Duration, Instant};

use eitri_core::editor_context::feed::PushFacts;
use eitri_core::editor_context::EditorContext;
use eitri_core::editor_rpc::EditorRpc;
use eitri_core::ide_server::host::IdeHost;
use eitri_core::ide_server::server::IdeServer;
use eitri_core::ide_server::{Command, ContextUpdate, Event, ServerConfig, Started, PORT_VAR};
use gtk4::glib;

use crate::linked_window::editor::Repeat;

/// How long the window waits for the lock to be written before it decides the first shell's environment. A shell
/// started later than this has no variable.
const FIRST_ANSWER_WITHIN: Duration = Duration::from_millis(200);

/// How long the window's end waits for the lock to go and the port to close.
const STOP_WITHIN: Duration = Duration::from_millis(500);

const LISTENER_STOPPED: &str = "terminal claude: the listener stopped; not attached";

/// What the bottom terminal's shells get. Set only for a port with a lock behind it.
#[derive(Clone, Default)]
pub(crate) struct ShellPort(Rc<Cell<Option<u16>>>);

impl ShellPort {
    /// On `cell`, which the window's pair holds too: in window 2 the port comes from there.
    pub(crate) fn shared(cell: Rc<Cell<Option<u16>>>) -> ShellPort {
        ShellPort(cell)
    }

    pub(crate) fn set(&self, port: Option<u16>) {
        self.0.set(port);
    }

    pub(crate) fn get(&self) -> Option<u16> {
        self.0.get()
    }

    /// With no port the list is empty, so an inherited value passes through.
    pub(crate) fn env(&self) -> Vec<(OsString, OsString)> {
        self.get()
            .map(|port| (OsString::from(PORT_VAR), OsString::from(port.to_string())))
            .into_iter()
            .collect()
    }

    /// `base`, what the window gives every shell, with the port as it stands when a shell starts.
    pub(crate) fn over(&self, base: Vec<(OsString, OsString)>) -> Rc<dyn Fn() -> Vec<(OsString, OsString)>> {
        let port = self.clone();
        Rc::new(move || base.iter().cloned().chain(port.env()).collect())
    }
}

/// Told the port a shell should get, each time that changes.
type PortHook = Rc<dyn Fn(Option<u16>)>;

/// What the wiring needs of the listener.
trait Listener {
    fn wake_fd(&self) -> RawFd;
    fn drain(&self) -> Vec<Event>;
    fn send(&self, commands: Vec<Command>);
    fn stop(self: Box<Self>, within: Duration);
}

impl Listener for IdeServer {
    fn wake_fd(&self) -> RawFd {
        IdeServer::wake_fd(self)
    }

    fn drain(&self) -> Vec<Event> {
        IdeServer::drain(self)
    }

    fn send(&self, commands: Vec<Command>) {
        IdeServer::send(self, commands);
    }

    fn stop(self: Box<Self>, within: Duration) {
        IdeServer::stop(*self, within);
    }
}

/// What the window hands over once it has an editor and a way to speak.
struct Window {
    rpc: Rc<dyn EditorRpc>,
    repeat: Repeat,
    note: Rc<dyn Fn(&str)>,
}

pub(crate) struct IdeWiring {
    /// `None` once the window has ended.
    listener: RefCell<Option<Box<dyn Listener>>>,
    host: RefCell<IdeHost>,
    shell_port: ShellPort,
    window: RefCell<Option<Window>>,
    /// A refusal heard before the window could say it.
    unsaid: RefCell<Option<String>>,
    port_hook: RefCell<Option<PortHook>>,
    /// The cadence the current timer runs at, `None` with no timer.
    armed: Cell<Option<Duration>>,
    /// Moves on each time the timer changes: a step of any other number has been replaced, and ends at its next run.
    timer: Cell<u64>,
    watch: Cell<Option<glib::SourceId>>,
    me: Weak<IdeWiring>,
}

impl IdeWiring {
    /// The listener's bind, then the wait of at most 200 ms for the worker's first answer. Called only with the key on,
    /// in a window that holds an editor.
    pub(crate) fn start(root: &Path, shell_port: ShellPort) -> Option<Rc<IdeWiring>> {
        let server = IdeServer::start(ServerConfig {
            root: root.to_path_buf(),
            home: std::env::var_os("HOME").map(PathBuf::from),
        })?;
        let first = server.first_answer(FIRST_ANSWER_WITHIN);
        Some(IdeWiring::with_listener(Box::new(server), root, shell_port, first))
    }

    fn with_listener(
        listener: Box<dyn Listener>,
        root: &Path,
        shell_port: ShellPort,
        first: Option<Started>,
    ) -> Rc<IdeWiring> {
        let wiring = Rc::new_cyclic(|me| IdeWiring {
            listener: RefCell::new(Some(listener)),
            host: RefCell::new(IdeHost::new(root.to_path_buf())),
            shell_port,
            window: RefCell::new(None),
            unsaid: RefCell::new(None),
            port_hook: RefCell::new(None),
            armed: Cell::new(None),
            timer: Cell::new(0),
            watch: Cell::new(None),
            me: me.clone(),
        });
        if let Some(started) = first {
            wiring.started(started);
        }
        wiring
    }

    /// Once the window has an editor handle and a way to speak: wakes watched, a pending refusal said.
    pub(crate) fn connect(&self, rpc: Rc<dyn EditorRpc>, repeat: Repeat, note: Rc<dyn Fn(&str)>) {
        self.attach(rpc, repeat, note);
        self.watch_wakes();
    }

    fn attach(&self, rpc: Rc<dyn EditorRpc>, repeat: Repeat, note: Rc<dyn Fn(&str)>) {
        *self.window.borrow_mut() = Some(Window {
            rpc,
            repeat,
            note: note.clone(),
        });
        let unsaid = self.unsaid.borrow_mut().take();
        if let Some(why) = unsaid {
            note(&why);
        }
    }

    /// The wake stays readable until a drain, so whatever came before this is seen at the next main-loop turn.
    fn watch_wakes(&self) {
        let Some(fd) = self.listener.borrow().as_ref().map(|listener| listener.wake_fd()) else {
            return;
        };
        let me = self.me.clone();
        let watched =
            neovide_editor::fd_watch::add_local(fd, glib::IOCondition::IN, glib::ffi::G_PRIORITY_DEFAULT, move |_| {
                let Some(wiring) = me.upgrade() else {
                    return glib::ControlFlow::Break;
                };
                wiring.drain();
                glib::ControlFlow::Continue
            });
        match watched {
            Ok(watch) => self.watch.set(Some(watch)),
            Err(neovide_editor::fd_watch::NotOwner) => {
                eprintln!("[ide-server] BUG: the listener is watched off the main context's thread; nothing arrives");
            }
        }
    }

    /// Each time the port a shell should get changes. The first answer came before there was a hook: a window 2 hears
    /// that one on its command line or from its link's `linked` hook.
    pub(crate) fn on_port(&self, hook: PortHook) {
        *self.port_hook.borrow_mut() = Some(hook);
    }

    /// Also `host.check_target`: a restarted nvim's feed reports at its load, and an idle host has no timer to see it.
    pub(crate) fn context_changed(&self, context: Option<EditorContext>, facts: PushFacts) {
        if self.listener.borrow().is_none() {
            return;
        }
        let mut out = vec![Command::Context(ContextUpdate { context, facts })];
        if let Some(rpc) = self.rpc() {
            self.host.borrow_mut().check_target(&*rpc, &mut out);
        }
        self.send(out);
        self.settle_timer();
    }

    /// The window's end: every view closes, then the lock and the port go. Waits, within its bound.
    pub(crate) fn end(&self) {
        let Some(listener) = self.listener.borrow_mut().take() else {
            return;
        };
        self.timer.set(self.timer.get() + 1);
        self.armed.set(None);
        if let Some(rpc) = self.rpc() {
            self.host.borrow_mut().shutdown(&*rpc);
        }
        if let Some(watch) = self.watch.take() {
            watch.remove();
        }
        listener.stop(STOP_WITHIN);
    }

    fn rpc(&self) -> Option<Rc<dyn EditorRpc>> {
        self.window.borrow().as_ref().map(|window| window.rpc.clone())
    }

    fn drain(&self) {
        let Some(rpc) = self.rpc() else {
            return;
        };
        let Some(events) = self.listener.borrow().as_ref().map(|listener| listener.drain()) else {
            return;
        };
        let mut out = Vec::new();
        let now = Instant::now();
        for event in events {
            match event {
                Event::Started(started) => self.started(started),
                Event::Lost => {
                    self.port_changed(None);
                    self.host.borrow_mut().on_event(Event::Lost, &*rpc, now, &mut out);
                    self.say(LISTENER_STOPPED);
                }
                event => self.host.borrow_mut().on_event(event, &*rpc, now, &mut out),
            }
        }
        self.send(out);
        self.settle_timer();
    }

    fn started(&self, started: Started) {
        match started {
            Started::Attached { port } => self.port_changed(Some(port)),
            Started::NotAttached { why } => self.say(&why),
        }
    }

    fn port_changed(&self, port: Option<u16>) {
        self.shell_port.set(port);
        let hook = self.port_hook.borrow().clone();
        if let Some(hook) = hook {
            hook(port);
        }
    }

    fn say(&self, text: &str) {
        let note = self.window.borrow().as_ref().map(|window| window.note.clone());
        match note {
            Some(note) => note(text),
            None => *self.unsaid.borrow_mut() = Some(text.to_owned()),
        }
    }

    fn send(&self, commands: Vec<Command>) {
        if commands.is_empty() {
            return;
        }
        if let Some(listener) = self.listener.borrow().as_ref() {
            listener.send(commands);
        }
    }

    /// One run of the timer numbered `number`. `false` ends that timer.
    fn on_timer(&self, number: u64) -> bool {
        if self.timer.get() != number {
            return false;
        }
        let Some(rpc) = self.rpc() else {
            return false;
        };
        let mut out = Vec::new();
        self.host.borrow_mut().tick(&*rpc, Instant::now(), &mut out);
        self.send(out);
        self.settle_timer();
        self.timer.get() == number
    }

    /// Unchanged, a running timer goes on. Changed, it is out of date and the new cadence is armed.
    fn settle_timer(&self) {
        let wanted = self.host.borrow().next_tick();
        if wanted == self.armed.get() {
            return;
        }
        let Some(repeat) = self.window.borrow().as_ref().map(|window| window.repeat.clone()) else {
            return;
        };
        let number = self.timer.get() + 1;
        self.timer.set(number);
        self.armed.set(wanted);
        if let Some(every) = wanted {
            let me = self.me.clone();
            repeat(
                every,
                Box::new(move || me.upgrade().is_some_and(|wiring| wiring.on_timer(number))),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use eitri_core::ide_server::lines::LineShape;
    use eitri_core::ide_server::mention::IDE_MENTION_DEFINE;
    use eitri_core::ide_server::view::{IDE_DIFF_CLOSE, IDE_DIFF_OPEN, IDE_TAKE_EVENTS};
    use eitri_core::ide_server::{DiffRequest, RpcId};
    use eitri_core::nvim_rpc::{Answer, Pending, RpcError};
    use rmpv::Value;

    use super::*;

    const ROOT: &str = "/project";
    const CALL_CADENCE: Duration = Duration::from_millis(10);
    const VIEW_CADENCE: Duration = Duration::from_millis(100);

    #[derive(Default)]
    struct Queue {
        events: RefCell<Vec<Event>>,
        sent: RefCell<Vec<Command>>,
        stops: Cell<u32>,
    }

    impl Queue {
        fn push(&self, event: Event) {
            self.events.borrow_mut().push(event);
        }
    }

    struct FakeListener(Rc<Queue>);

    impl Listener for FakeListener {
        fn wake_fd(&self) -> RawFd {
            -1
        }

        fn drain(&self) -> Vec<Event> {
            std::mem::take(&mut *self.0.events.borrow_mut())
        }

        fn send(&self, commands: Vec<Command>) {
            self.0.sent.borrow_mut().extend(commands);
        }

        fn stop(self: Box<Self>, _within: Duration) {
            self.0.stops.set(self.0.stops.get() + 1);
        }
    }

    /// Records every call and holds its answer until a test gives it.
    #[derive(Default)]
    struct FakeEditor {
        target: Cell<Option<u64>>,
        codes: RefCell<Vec<&'static str>>,
        answers: RefCell<Vec<Option<Answer>>>,
    }

    impl FakeEditor {
        fn count(&self, code: &str) -> usize {
            self.codes.borrow().iter().filter(|sent| **sent == code).count()
        }

        fn answer(&self, code: &str, reply: Result<Value, RpcError>) {
            let codes = self.codes.borrow();
            let mut answers = self.answers.borrow_mut();
            let at = (0..codes.len())
                .find(|at| codes[*at] == code && answers[*at].is_some())
                .expect("a call of that chunk is out");
            answers[at].take().expect("just found").send(reply);
        }
    }

    impl EditorRpc for FakeEditor {
        fn exec_lua(&self, code: &'static str, _args: Vec<Value>) -> Pending {
            let (answer, pending) = Pending::pair();
            self.codes.borrow_mut().push(code);
            self.answers.borrow_mut().push(Some(answer));
            pending
        }

        fn target(&self) -> Option<u64> {
            self.target.get()
        }
    }

    type Step = (Duration, Box<dyn Fn() -> bool>);

    /// A `Repeat` stepped by hand.
    #[derive(Clone, Default)]
    struct Timers(Rc<RefCell<Vec<Step>>>);

    impl Timers {
        fn repeat(&self) -> Repeat {
            let armed = self.0.clone();
            Rc::new(move |every: Duration, step: Box<dyn Fn() -> bool>| armed.borrow_mut().push((every, step)))
        }

        fn cadences(&self) -> Vec<Duration> {
            self.0.borrow().iter().map(|(every, _)| *every).collect()
        }

        /// Each armed step once. What a step arms waits for the next call.
        fn step(&self) {
            let running = std::mem::take(&mut *self.0.borrow_mut());
            for (every, step) in running {
                if step() {
                    self.0.borrow_mut().push((every, step));
                }
            }
        }
    }

    struct Rig {
        queue: Rc<Queue>,
        editor: Rc<FakeEditor>,
        timers: Timers,
        notes: Rc<RefCell<Vec<String>>>,
        port: ShellPort,
        wiring: Rc<IdeWiring>,
    }

    impl Rig {
        fn new(first: Option<Started>) -> Rig {
            let rig = Rig::unconnected(first);
            let notes = rig.notes.clone();
            rig.wiring.attach(
                rig.editor.clone(),
                rig.timers.repeat(),
                Rc::new(move |text: &str| notes.borrow_mut().push(text.to_owned())),
            );
            rig
        }

        fn unconnected(first: Option<Started>) -> Rig {
            let queue = Rc::new(Queue::default());
            let port = ShellPort::default();
            let wiring = IdeWiring::with_listener(
                Box::new(FakeListener(queue.clone())),
                Path::new(ROOT),
                port.clone(),
                first,
            );
            let editor = Rc::new(FakeEditor::default());
            editor.target.set(Some(1));
            Rig {
                queue,
                editor,
                timers: Timers::default(),
                notes: Rc::default(),
                port,
                wiring,
            }
        }

        /// What a wake of the listener does.
        fn wake_with(&self, event: Event) {
            self.queue.push(event);
            self.wiring.drain();
        }
    }

    fn attached() -> Option<Started> {
        Some(Started::Attached { port: 23456 })
    }

    fn open_diff() -> Event {
        Event::OpenDiff {
            conn: 1,
            id: RpcId::Num(7),
            diff: Box::new(DiffRequest {
                tab_name: "a tab".to_owned(),
                real_path: PathBuf::from("/project/a.rs"),
                rel: "a.rs".to_owned(),
                file_text: Vec::new(),
                file_lines: Vec::new(),
                proposal_lines: Vec::new(),
                shape: LineShape { crlf: false, eol: true },
            }),
        }
    }

    fn opened() -> Result<Value, RpcError> {
        Ok(Value::Map(vec![(Value::from("opened"), Value::from(true))]))
    }

    fn the_view_was_closed() -> Result<Value, RpcError> {
        Ok(Value::Array(vec![Value::Map(vec![
            (Value::from("view"), Value::from(1)),
            (Value::from("kind"), Value::from("closed")),
        ])]))
    }

    fn os(key: &str, value: &str) -> (OsString, OsString) {
        (OsString::from(key), OsString::from(value))
    }

    #[test]
    fn a_shell_port_names_the_variable() {
        let port = ShellPort::default();
        port.set(Some(23456));
        assert_eq!(port.env(), vec![os("CLAUDE_CODE_SSE_PORT", "23456")]);
    }

    #[test]
    fn no_port_gives_an_empty_list() {
        assert_eq!(ShellPort::default().env(), Vec::new());
    }

    #[test]
    fn the_port_joins_what_the_window_gives_every_shell_when_a_shell_starts() {
        let port = ShellPort::default();
        let env = port.over(vec![os("EITRI_OPEN", "/run/a.sock")]);
        assert_eq!(env(), vec![os("EITRI_OPEN", "/run/a.sock")]);
        port.set(Some(23456));
        assert_eq!(
            env(),
            vec![os("EITRI_OPEN", "/run/a.sock"), os("CLAUDE_CODE_SSE_PORT", "23456")]
        );
    }

    #[test]
    fn no_lock_means_no_variable() {
        let rig = Rig::new(Some(Started::NotAttached {
            why: "no lock; not attached".to_owned(),
        }));
        assert_eq!(rig.port.get(), None);
        let host = [os("CLAUDE_CODE_SSE_PORT", "1"), os("PATH", "/usr/bin")];
        let env = eitri_terminal::child_environment_with(host, &rig.port.env());
        assert!(env.contains(&os("CLAUDE_CODE_SSE_PORT", "1")), "{env:?}");
    }

    #[test]
    fn a_late_first_answer_reaches_later_shells_only() {
        let rig = Rig::new(None);
        let told = Rc::new(RefCell::new(Vec::new()));
        let heard = told.clone();
        rig.wiring
            .on_port(Rc::new(move |port: Option<u16>| heard.borrow_mut().push(port)));
        let first_shell = rig.port.env();

        rig.wake_with(Event::Started(Started::Attached { port: 23456 }));

        assert_eq!(first_shell, Vec::new(), "a shell keeps what it started with");
        assert_eq!(rig.port.env(), vec![os("CLAUDE_CODE_SSE_PORT", "23456")]);
        assert_eq!(*told.borrow(), vec![Some(23456)]);
    }

    #[test]
    fn a_refusal_is_said_once() {
        let rig = Rig::unconnected(Some(Started::NotAttached {
            why: "HOME is not absolute; not attached".to_owned(),
        }));
        assert!(rig.notes.borrow().is_empty());
        let notes = rig.notes.clone();
        rig.wiring.attach(
            rig.editor.clone(),
            rig.timers.repeat(),
            Rc::new(move |text: &str| notes.borrow_mut().push(text.to_owned())),
        );
        rig.wiring.drain();
        assert_eq!(*rig.notes.borrow(), vec!["HOME is not absolute; not attached"]);
        rig.wiring.context_changed(None, PushFacts::default());
        assert_eq!(rig.notes.borrow().len(), 1);
    }

    #[test]
    fn a_refusal_heard_late_is_said_at_once() {
        let rig = Rig::new(None);
        rig.wake_with(Event::Started(Started::NotAttached {
            why: "no lock; not attached".to_owned(),
        }));
        assert_eq!(*rig.notes.borrow(), vec!["no lock; not attached"]);
        assert_eq!(rig.port.get(), None);
    }

    #[test]
    fn an_idle_host_keeps_no_timer() {
        let rig = Rig::new(attached());
        rig.wake_with(Event::ClientGone { conn: 1 });
        rig.wiring.context_changed(None, PushFacts::default());
        assert_eq!(rig.timers.cadences(), Vec::<Duration>::new());
    }

    #[test]
    fn the_cadence_moves_from_a_call_to_a_view_and_back_to_none() {
        let rig = Rig::new(attached());
        rig.wake_with(open_diff());
        assert_eq!(rig.editor.count(IDE_DIFF_OPEN), 1);
        assert_eq!(rig.timers.cadences(), vec![CALL_CADENCE]);

        rig.timers.step();
        assert_eq!(rig.timers.cadences(), vec![CALL_CADENCE], "nothing has changed");

        // No frame from any client from here on: only the timer moves things.
        rig.editor.answer(IDE_DIFF_OPEN, opened());
        rig.timers.step();
        assert_eq!(rig.timers.cadences(), vec![VIEW_CADENCE]);
        assert_eq!(rig.editor.count(IDE_TAKE_EVENTS), 1, "polled with no wake");

        rig.editor.answer(IDE_TAKE_EVENTS, the_view_was_closed());
        rig.timers.step();
        assert_eq!(rig.timers.cadences(), Vec::<Duration>::new());
    }

    #[test]
    fn a_wake_that_changes_the_cadence_ends_the_running_step() {
        let rig = Rig::new(attached());
        rig.wake_with(open_diff());
        rig.editor.answer(IDE_DIFF_OPEN, opened());
        rig.timers.step();
        assert_eq!(rig.timers.cadences(), vec![VIEW_CADENCE]);

        rig.wake_with(Event::Diagnostics {
            conn: 2,
            id: RpcId::Num(8),
            query: eitri_core::ide_server::DiagnosticsQuery::All,
        });
        assert_eq!(rig.timers.cadences(), vec![VIEW_CADENCE, CALL_CADENCE]);
        rig.timers.step();
        assert_eq!(rig.timers.cadences(), vec![CALL_CADENCE], "the older step ended itself");
    }

    #[test]
    fn a_step_after_end_returns_false() {
        let rig = Rig::new(attached());
        rig.wake_with(open_diff());
        rig.wiring.end();
        let calls = rig.editor.codes.borrow().len();
        rig.timers.step();
        assert_eq!(rig.timers.cadences(), Vec::<Duration>::new());
        assert_eq!(
            rig.editor.codes.borrow().len(),
            calls,
            "a step after the end asks nothing"
        );
    }

    #[test]
    fn the_end_closes_the_views_and_stops_the_listener_once() {
        let rig = Rig::new(attached());
        rig.wake_with(open_diff());
        rig.wiring.end();
        rig.wiring.end();
        assert_eq!(rig.editor.count(IDE_DIFF_CLOSE), 1);
        assert_eq!(rig.queue.stops.get(), 1);
        rig.wiring.context_changed(None, PushFacts::default());
        assert!(rig.queue.sent.borrow().is_empty());
    }

    #[test]
    fn a_context_update_reaches_check_target() {
        let rig = Rig::new(attached());
        rig.wake_with(Event::MentionUp {
            socket: PathBuf::from("/run/m.sock"),
        });
        assert_eq!(rig.editor.count(IDE_MENTION_DEFINE), 1);

        rig.editor.target.set(Some(2));
        let facts = PushFacts {
            cursor_line: Some(4),
            start_col: None,
            end_col: None,
            read_from: None,
        };
        rig.wiring.context_changed(None, facts.clone());

        assert_eq!(
            rig.editor.count(IDE_MENTION_DEFINE),
            2,
            "the new editor has the command"
        );
        let sent = rig.queue.sent.borrow();
        assert!(
            matches!(sent.as_slice(), [Command::Context(update)] if update.facts == facts),
            "{sent:?}"
        );
    }

    /// A window 2 started at a window 1's launch has the port on its command line only if the wait is over by then,
    /// and only the layout's work can start one.
    #[test]
    fn the_first_answer_comes_before_window_2_can_start() {
        use eitri_core::source_scan as scan;
        let read = scan::rust_sources(&["shell/src"]);
        let (_, ui) = scan::function_in(&read, "build_ui");
        let at = |what: &str| ui.find(what).unwrap_or_else(|| panic!("`{what}` in build_ui"));
        assert!(at("IdeWiring::start") < at("on_port"));
        assert!(at("on_port") < at("StartStep::Layout"));
    }

    /// A link to window 2 hears the port from the window's `linked` hook; a module window's returns before that.
    #[test]
    fn the_linked_hook_says_the_port_on_window_2s_link_only() {
        use eitri_core::source_scan as scan;
        let read = scan::rust_sources(&["shell/src"]);
        let (_, ui) = scan::function_in(&read, "build_ui");
        let hook = ui.find("let linked: linked_window::spawn::OnLinked").expect("the hook");
        let hook = &ui[hook..ui[hook..].find("let post:").expect("what follows it") + hook];
        let module_branch_ends = hook.find("return;").expect("the module window's branch");
        assert!(hook.find("say_ide_port").expect("the call") > module_branch_ends);
    }

    #[test]
    fn an_unpaired_window_has_its_own_listener() {
        use crate::linked_window::driver::tests::pair;
        use crate::linked_window::WindowRole;
        use eitri_core::window_link::names::WindowName;
        let (link, _theirs, _inbound) = pair();
        let table = [
            (WindowRole::Main { link: None }, true, "window 1"),
            (WindowRole::Alone, true, "a window alone"),
            (WindowRole::Unpaired, true, "a second full window of a project"),
            (WindowRole::Second { link: link.clone() }, false, "window 2"),
            (
                WindowRole::Module {
                    name: WindowName::Chat(1),
                    link,
                },
                false,
                "a chat window",
            ),
        ];
        for (role, listens, who) in table {
            assert_eq!(role.owns_editor(), listens, "{who}");
        }
    }

    #[test]
    fn a_lost_listener_clears_the_port_and_says_so_once() {
        let rig = Rig::new(attached());
        assert_eq!(rig.port.get(), Some(23456));
        let told = Rc::new(RefCell::new(Vec::new()));
        let heard = told.clone();
        rig.wiring
            .on_port(Rc::new(move |port: Option<u16>| heard.borrow_mut().push(port)));
        rig.wake_with(Event::MentionUp {
            socket: PathBuf::from("/run/m.sock"),
        });
        rig.wake_with(open_diff());

        rig.wake_with(Event::Lost);
        rig.wiring.drain();

        assert_eq!(rig.port.get(), None);
        assert_eq!(*told.borrow(), vec![None]);
        assert_eq!(*rig.notes.borrow(), vec![LISTENER_STOPPED]);
        assert_eq!(rig.editor.count(IDE_DIFF_CLOSE), 1, "the host heard it too");
    }
}
