//! Drives the attach: the threads that connect and install, and the loop that feeds what they
//! learn to the [`Attacher`] and carries out what it answers. GTK-free and std-only, so the real
//! nvim tests reach it directly; the window owns one and calls [`LinkDriver::poll`] from a timer.
//!
//! Nothing here blocks the caller. A connect happens on a worker thread; the install and every
//! later call are queued on the link and answered through a [`Pending`] that `poll` only tries.
//! A slow install is a pending one: nvim queues requests while it waits for a key, so there is no
//! timeout on it and no timeout on a call made afterwards.

use std::collections::VecDeque;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rmpv::Value;

use super::attach::{AttachEvent, Attacher, BandLink, Effect, LinkState};
use super::{
    client_info_params, install_args, parse_install_report, Sockets, INSTALL_LUA, REPLACED_METHOD, TEARDOWN_LUA,
};
use crate::nvim_rpc::{LinkEvent, NvimLink, Pending, RpcError};

/// How long a connect may take before it is a failure. A stale socket file refuses at once; this
/// bounds an nvim that accepts and then does not answer the handshake.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);

/// How long an install must be pending before the panel asks whether nvim is waiting for a key.
const PROBE_AFTER: Duration = Duration::from_millis(300);

/// The least time between two questions about nvim's mode.
const PROBE_EVERY: Duration = Duration::from_secs(1);

/// How many of this panel's earlier channels are remembered, to recognise its own older attach
/// winning a race inside nvim.
const OWN_CHANNELS: usize = 8;

/// One earlier connection of this panel. A channel number means something only inside the nvim
/// that handed it out, so it is kept with the editor it belongs to: another panel's install into a
/// different nvim can be given the same number.
struct OwnChannel {
    addr: PathBuf,
    channel: u64,
    /// The nvim process, known once the install answered. An nvim restarted on the same address
    /// numbers its channels afresh, and has another pid.
    pid: Option<u32>,
}

/// What a worker thread reports. One channel for all of it, so a connection's `Linked` is always
/// read before anything the same worker says about that connection afterwards.
enum WorkerMsg {
    Event(AttachEvent),
    /// A connection is up and its install is queued.
    Linked {
        gen: u64,
        link: NvimLink,
        install: Pending,
    },
    /// nvim sent `eitri_replaced`: another install took the editor. `by` is that install's channel.
    ReplacedBy {
        gen: u64,
        by: Option<u64>,
    },
}

/// The two things a carried-out effect asks of a connection.
pub(crate) trait Wire {
    fn exec_lua(&self, code: &str, args: Vec<Value>);
    fn close_when_flushed(&self);
}

impl Wire for NvimLink {
    fn exec_lua(&self, code: &str, args: Vec<Value>) {
        // The answer is of no use to the caller; a link that has ended answers `Closed` to nobody.
        drop(NvimLink::exec_lua(self, code, args));
    }

    fn close_when_flushed(&self) {
        NvimLink::close_when_flushed(self);
    }
}

/// The connection of one generation.
pub(crate) struct Live<W> {
    pub(crate) gen: u64,
    pub(crate) link: W,
    pub(crate) channel: u64,
}

/// Carries out `effects` strictly in the order given. Returns whether the drafts waiting on the
/// editor must be ended.
///
/// A teardown is queued on the old link before that link is closed, and the close is the flushing
/// one: `close` would discard the teardown with everything else queued.
pub(crate) fn run_effects<W: Wire>(
    effects: Vec<Effect>,
    live: &mut Option<Live<W>>,
    current_gen: &AtomicU64,
    spawn: &mut dyn FnMut(u64, PathBuf),
) -> bool {
    let mut cancel = false;
    for effect in effects {
        match effect {
            Effect::Connect { gen, addr } => {
                current_gen.store(gen, Ordering::SeqCst);
                spawn(gen, addr);
            }
            Effect::ExplicitTeardown { gen } => {
                if let Some(held) = live.as_ref().filter(|held| held.gen == gen) {
                    held.link.exec_lua(TEARDOWN_LUA, vec![Value::from(held.channel)]);
                }
            }
            Effect::Close { gen } => {
                if live.as_ref().is_some_and(|held| held.gen == gen) {
                    if let Some(held) = live.take() {
                        held.link.close_when_flushed();
                    }
                }
            }
            Effect::CancelDraftEdits => cancel = true,
        }
    }
    cancel
}

/// Decides when to ask nvim whether it is waiting for a key.
#[derive(Debug, Default)]
pub(crate) struct ModeProbe {
    last_ask: Option<Instant>,
}

impl ModeProbe {
    /// True once the install has been pending for 300 ms, no question is still unanswered and the
    /// last one was a second ago. The unanswered rule matters: while nvim is busy for another
    /// reason, a question queues, and one more every second would grow its queue for as long as
    /// that lasts.
    pub(crate) fn due(&self, sent_at: Instant, now: Instant, outstanding: bool) -> bool {
        now.saturating_duration_since(sent_at) >= PROBE_AFTER
            && !outstanding
            && self
                .last_ask
                .is_none_or(|asked| now.saturating_duration_since(asked) >= PROBE_EVERY)
    }

    pub(crate) fn asked(&mut self, now: Instant) {
        self.last_ask = Some(now);
    }
}

/// Whether nvim's answer to `nvim_get_mode` says it is waiting for input.
pub(crate) fn get_mode_blocking(answer: &Value) -> Option<bool> {
    answer
        .as_map()?
        .iter()
        .find(|(key, _)| key.as_str() == Some("blocking"))
        .and_then(|(_, value)| value.as_bool())
}

/// `live_peer` (the pid of the live connection's peer) as the editor's pid, only in the attached
/// state: in any other the connection is not yet, or no longer, the editor the panel follows.
fn attached_peer(state: &LinkState, live_peer: Option<u32>) -> Option<u32> {
    match state {
        LinkState::Attached { .. } => live_peer,
        _ => None,
    }
}

/// The log line for an editor whose own report of its pid is not the pid of the process holding its
/// socket; `None` when they agree or when the platform gave no peer pid to compare with.
fn pid_mismatch_line(reported: u32, peer: Option<u32>) -> Option<String> {
    let peer = peer.filter(|&peer| peer != reported)?;
    Some(format!(
        "[companion] the editor reported pid {reported}, its socket is held by {peer}; using {peer}"
    ))
}

/// Why a call cannot go to the editor now; `None` when it can.
pub fn not_attached_why(state: &LinkState) -> Option<&'static str> {
    match state {
        LinkState::Attached { .. } => None,
        LinkState::Connecting { .. } | LinkState::Attaching { .. } => Some("the editor is still attaching"),
        LinkState::NoEditor | LinkState::Detached { .. } | LinkState::Failed { .. } => {
            Some("the editor is not connected to this panel")
        }
    }
}

/// The install of the current connection, until it answers.
struct InstallWatch {
    gen: u64,
    pending: Pending,
    sent_at: Instant,
    probe: ModeProbe,
    /// The `nvim_get_mode` question that has no answer yet.
    mode: Option<Pending>,
}

/// What one [`LinkDriver::poll`] changed.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Tick {
    /// The state is not the one the last tick left.
    pub state_changed: bool,
    /// The editor went away or was swapped: the drafts waiting on it must end.
    pub cancel_drafts: bool,
    /// Lines for the log, in order.
    pub logs: Vec<String>,
}

pub struct LinkDriver {
    attacher: Attacher,
    tx: Sender<WorkerMsg>,
    rx: Receiver<WorkerMsg>,
    /// The generation of the last `Connect`: a worker whose own is older drops its link instead of
    /// installing into an nvim the panel has already left.
    current_gen: Arc<AtomicU64>,
    live: Option<Live<NvimLink>>,
    install: Option<InstallWatch>,
    /// The parts the last install loaded.
    installed: Vec<String>,
    own_channels: VecDeque<OwnChannel>,
    sockets: Sockets,
    /// What `new` already asked of the machine, carried out by the first `poll`.
    first_effects: Vec<Effect>,
    last_state: LinkState,
    shut: bool,
    /// Starts a connect's worker thread. A field so a test can make it fail.
    spawn_thread: fn(Job) -> io::Result<()>,
}

impl LinkDriver {
    /// A driver for a window started with an editor address, or without one. Nothing connects
    /// until the first [`LinkDriver::poll`]; the state already says `Connecting` for an address.
    pub fn new(initial: Option<PathBuf>, sockets: Sockets) -> LinkDriver {
        let (attacher, first_effects) = Attacher::new(initial);
        let (tx, rx) = mpsc::channel();
        let last_state = attacher.state().clone();
        LinkDriver {
            attacher,
            tx,
            rx,
            current_gen: Arc::new(AtomicU64::new(0)),
            live: None,
            install: None,
            installed: Vec::new(),
            own_channels: VecDeque::new(),
            sockets,
            first_effects,
            last_state,
            shut: false,
            spawn_thread: spawn_attach_thread,
        }
    }

    /// Aim the panel at this nvim; the one it already follows is left as it is (asking from it is how
    /// the panel is brought forward). Takes effect at the next `poll`.
    pub fn attach(&self, addr: PathBuf) {
        let _ = self.tx.send(WorkerMsg::Event(AttachEvent::Requested(addr)));
    }

    pub fn state(&self) -> &LinkState {
        self.attacher.state()
    }

    pub fn band(&self) -> BandLink {
        self.attacher.band()
    }

    pub fn nvim_pid(&self) -> Option<u32> {
        match self.attacher.state() {
            LinkState::Attached { nvim_pid, .. } => Some(*nvim_pid),
            _ => None,
        }
    }

    /// The pid of the process holding the editor's socket, while the link is attached: what the
    /// kernel said when the connection was made, not the pid the editor reported about itself, which
    /// any process that answers the install can make up. The editor's window is looked for from this
    /// pid. `None` when not attached or when the platform gives no pid.
    pub fn peer_pid(&self) -> Option<u32> {
        let live_peer = self.live.as_ref().and_then(|live| live.link.peer_pid());
        attached_peer(self.attacher.state(), live_peer)
    }

    /// Whether the last install loaded the part called `name`.
    pub fn has_part(&self, name: &str) -> bool {
        self.installed.iter().any(|part| part == name)
    }

    pub fn is_shut(&self) -> bool {
        self.shut
    }

    /// Run Lua in the attached nvim. The answer is not waited for. `Err` says why not.
    pub fn exec_lua(&mut self, code: &str, args: Vec<Value>) -> Result<(), String> {
        self.exec_lua_for(None, code, args)
    }

    /// [`LinkDriver::exec_lua`] for code that needs the install's part `part` to be loaded: without
    /// it the call would answer `false` to nobody, so it is refused with a reason instead.
    pub fn exec_lua_for(&mut self, part: Option<&str>, code: &str, args: Vec<Value>) -> Result<(), String> {
        if let Some(why) = not_attached_why(self.attacher.state()) {
            return Err(why.to_owned());
        }
        if let Some(part) = part.filter(|part| !self.has_part(part)) {
            return Err(format!("the editor could not load Eitri's {part} support"));
        }
        match &self.live {
            Some(live) if live.link.is_alive() => {
                Wire::exec_lua(&live.link, code, args);
                Ok(())
            }
            _ => Err("the editor is not connected to this panel".to_owned()),
        }
    }

    /// Lets go of the editor: the teardown is queued, then the connection closes once it is
    /// written. Never waits: an nvim sitting in a prompt answers when the user does, and the
    /// glue's own liveness timer removes what a dropped teardown leaves.
    pub fn shutdown(&mut self) {
        if let (LinkState::Attached { channel, .. }, Some(live)) = (self.attacher.state(), &self.live) {
            Wire::exec_lua(&live.link, TEARDOWN_LUA, vec![Value::from(*channel)]);
        }
        if let Some(live) = self.live.take() {
            live.link.close_when_flushed();
        }
        self.install = None;
        // A worker still connecting drops its link when it sees this.
        self.current_gen.store(u64::MAX, Ordering::SeqCst);
        self.shut = true;
    }

    /// One step: read what the workers said, read the install's answer, ask nvim whether it is
    /// waiting for a key when the install is slow, and carry out what the machine answered.
    pub fn poll(&mut self, now: Instant) -> Tick {
        let mut tick = Tick::default();
        if self.shut {
            return tick;
        }
        let first = std::mem::take(&mut self.first_effects);
        self.carry_out(first, &mut tick);
        while let Ok(message) = self.rx.try_recv() {
            self.on_message(message, now, &mut tick);
        }
        self.poll_install(&mut tick);
        self.ask_mode(now);
        self.poll_mode();
        // `Attached` plus `Closed` answers no `Close`, and a failed or detached panel holds no link.
        if !matches!(
            self.attacher.state(),
            LinkState::Connecting { .. } | LinkState::Attaching { .. } | LinkState::Attached { .. }
        ) {
            if let Some(live) = self.live.take() {
                live.link.close_when_flushed();
            }
            self.install = None;
        }
        if *self.attacher.state() != self.last_state {
            self.last_state = self.attacher.state().clone();
            tick.state_changed = true;
        }
        tick
    }

    fn on_message(&mut self, message: WorkerMsg, now: Instant, tick: &mut Tick) {
        match message {
            WorkerMsg::Event(event) => self.feed(event, tick),
            WorkerMsg::Linked { gen, link, install } => {
                let wanted = gen == self.current_gen.load(Ordering::SeqCst)
                    && gen == self.attacher.gen()
                    && matches!(self.attacher.state(), LinkState::Connecting { .. });
                if !wanted {
                    // A connection for an editor the panel has left, or one that already failed.
                    link.close();
                    return;
                }
                let channel = link.channel_id();
                if let Some(addr) = self.current_addr() {
                    self.own_channels.push_back(OwnChannel {
                        addr,
                        channel,
                        pid: None,
                    });
                }
                while self.own_channels.len() > OWN_CHANNELS {
                    self.own_channels.pop_front();
                }
                self.installed.clear();
                self.live = Some(Live { gen, link, channel });
                self.install = Some(InstallWatch {
                    gen,
                    pending: install,
                    sent_at: now,
                    probe: ModeProbe::default(),
                    mode: None,
                });
                self.feed(AttachEvent::Connected { gen, channel }, tick);
            }
            WorkerMsg::ReplacedBy { gen, by } => {
                let ours = by.is_some_and(|by| self.is_own_earlier_channel(by));
                if ours && gen == self.attacher.gen() {
                    // An older attach of this very panel reached nvim after the newer one and took
                    // its place, so nothing is installed for the current connection any more.
                    tick.logs.push(
                        "[companion] an earlier attach of this panel replaced the current one; attaching again"
                            .to_owned(),
                    );
                    if let Some(addr) = self.current_addr() {
                        self.feed(AttachEvent::Attach(addr), tick);
                    }
                } else {
                    self.feed(AttachEvent::Replaced { gen }, tick);
                }
            }
        }
    }

    /// Whether `by` is the channel of an earlier connection of this panel to the editor it is
    /// aimed at now, and not the live one.
    fn is_own_earlier_channel(&self, by: u64) -> bool {
        if self.live.as_ref().is_some_and(|live| live.channel == by) {
            return false;
        }
        let Some(addr) = self.current_addr() else {
            return false;
        };
        let pid = self.nvim_pid();
        self.own_channels.iter().any(|own| {
            own.channel == by && own.addr == addr && !matches!((own.pid, pid), (Some(then), Some(now)) if then != now)
        })
    }

    fn current_addr(&self) -> Option<PathBuf> {
        match self.attacher.state() {
            LinkState::Connecting { addr }
            | LinkState::Attaching { addr, .. }
            | LinkState::Attached { addr, .. }
            | LinkState::Failed { addr, .. } => Some(addr.clone()),
            LinkState::NoEditor | LinkState::Detached { .. } => None,
        }
    }

    /// Hands one event to the machine and carries out its answer at once, so an effect that sets
    /// the generation is in place before the next message is judged against it.
    fn feed(&mut self, event: AttachEvent, tick: &mut Tick) {
        let effects = self.attacher.handle(event);
        self.carry_out(effects, tick);
    }

    fn carry_out(&mut self, effects: Vec<Effect>, tick: &mut Tick) {
        let tx = self.tx.clone();
        let current = Arc::clone(&self.current_gen);
        let sockets = self.sockets.clone();
        let spawn_thread = self.spawn_thread;
        let mut spawn = move |gen: u64, addr: PathBuf| {
            let (tx, current, sockets) = (tx.clone(), Arc::clone(&current), sockets.clone());
            let report = tx.clone();
            let shown = addr.display().to_string();
            // Out of threads is a failed connect like any other, reported through the channel the
            // worker would have used, so this same poll reads it; a panic here would end the panel.
            if let Err(e) = spawn_thread(Box::new(move || worker(gen, addr, tx, current, sockets))) {
                let why = format!("could not start a thread to connect to {shown}: {e}");
                let _ = report.send(WorkerMsg::Event(AttachEvent::ConnectFailed { gen, why }));
            }
        };
        if run_effects(effects, &mut self.live, &self.current_gen, &mut spawn) {
            tick.cancel_drafts = true;
        }
    }

    fn poll_install(&mut self, tick: &mut Tick) {
        let Some(watch) = &self.install else { return };
        let gen = watch.gen;
        let Some(answer) = watch.pending.try_take() else { return };
        self.install = None;
        match answer {
            Ok(value) => match parse_install_report(&value) {
                Ok(report) => {
                    for (part, why) in &report.failed {
                        tick.logs.push(format!("[companion] {part} did not install: {why}"));
                    }
                    self.installed = report.installed.clone();
                    let peer = self.live.as_ref().and_then(|live| live.link.peer_pid());
                    tick.logs.extend(pid_mismatch_line(report.nvim_pid, peer));
                    if let (Some(live), Some(addr)) = (&self.live, self.current_addr()) {
                        for own in self.own_channels.iter_mut() {
                            if own.channel == live.channel && own.addr == addr {
                                own.pid = Some(report.nvim_pid);
                            }
                        }
                    }
                    self.feed(AttachEvent::Installed { gen, report }, tick);
                }
                Err(why) => self.feed(AttachEvent::InstallFailed { gen, why }, tick),
            },
            Err(RpcError::Nvim(why)) => self.feed(AttachEvent::InstallFailed { gen, why }, tick),
            // The connection ended first; its own `Closed` says what the band says.
            Err(RpcError::Closed | RpcError::Encode(_)) => {}
        }
    }

    fn ask_mode(&mut self, now: Instant) {
        let (Some(watch), Some(live)) = (&mut self.install, &self.live) else {
            return;
        };
        if watch.probe.due(watch.sent_at, now, watch.mode.is_some()) {
            watch.probe.asked(now);
            watch.mode = Some(live.link.call("nvim_get_mode", vec![]));
        }
    }

    fn poll_mode(&mut self) {
        let Some(watch) = &mut self.install else { return };
        let Some(answer) = watch.mode.as_ref().and_then(Pending::try_take) else {
            return;
        };
        watch.mode = None;
        let gen = watch.gen;
        if let Some(blocking) = answer.ok().as_ref().and_then(get_mode_blocking) {
            let effects = self.attacher.handle(AttachEvent::Blocking { gen, blocking });
            debug_assert!(effects.is_empty());
        }
    }
}

/// What a connect's worker thread runs.
type Job = Box<dyn FnOnce() + Send + 'static>;

fn spawn_attach_thread(job: Job) -> io::Result<()> {
    std::thread::Builder::new()
        .name("eitri-attach".to_owned())
        .spawn(job)
        .map(drop)
}

/// One connect, for one generation. Ends when the connection does or when the driver is gone.
fn worker(gen: u64, addr: PathBuf, tx: Sender<WorkerMsg>, current: Arc<AtomicU64>, sockets: Sockets) {
    let (link, events) = match NvimLink::connect(&addr, CONNECT_TIMEOUT) {
        Ok(connected) => connected,
        Err(why) => {
            let _ = tx.send(WorkerMsg::Event(AttachEvent::ConnectFailed { gen, why }));
            return;
        }
    };
    // Two attaches inside one poll would otherwise put two installs into one nvim.
    if current.load(Ordering::SeqCst) != gen {
        link.close();
        return;
    }
    drop(link.call("nvim_set_client_info", client_info_params()));
    let install = link.exec_lua(INSTALL_LUA, install_args(link.channel_id(), &sockets));
    if tx.send(WorkerMsg::Linked { gen, link, install }).is_err() {
        return;
    }
    // The same thread now watches the connection, so `Linked` is always read before anything below.
    loop {
        let message = match events.recv() {
            Ok(LinkEvent::Notification { method, params }) if method == REPLACED_METHOD => WorkerMsg::ReplacedBy {
                gen,
                by: params.first().and_then(Value::as_u64),
            },
            Ok(LinkEvent::Notification { .. }) => continue,
            Ok(LinkEvent::Closed) | Err(_) => {
                let _ = tx.send(WorkerMsg::Event(AttachEvent::Closed { gen }));
                return;
            }
        };
        if tx.send(message).is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::companion::attach::AttachEvent as Ev;
    use crate::companion::InstallReport;
    use std::cell::RefCell;
    use std::rc::Rc;

    #[test]
    fn an_install_pending_past_300ms_asks_get_mode_once_a_second() {
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let mut probe = ModeProbe::default();
        assert!(!probe.due(t0, at(0), false));
        assert!(!probe.due(t0, at(299), false));
        assert!(probe.due(t0, at(300), false));
        probe.asked(at(300));
        assert!(
            !probe.due(t0, at(800), false),
            "less than a second after the last question"
        );
        assert!(probe.due(t0, at(1300), false));
        assert!(!probe.due(t0, at(1300), true), "a question is still unanswered");
    }

    /// Records, in one list, what the effect runner asked of a connection and of the spawner.
    struct Recorder(Rc<RefCell<Vec<String>>>);

    impl Wire for Recorder {
        fn exec_lua(&self, code: &str, args: Vec<Value>) {
            let which = if code == TEARDOWN_LUA { "TEARDOWN_LUA" } else { "other" };
            self.0.borrow_mut().push(format!("exec_lua({which},{args:?})"));
        }

        fn close_when_flushed(&self) {
            self.0.borrow_mut().push("close_when_flushed".to_owned());
        }
    }

    fn report() -> InstallReport {
        InstallReport {
            installed: vec!["scratch".into()],
            failed: vec![],
            nvim_pid: 9,
            in_tmux: false,
        }
    }

    #[test]
    fn the_first_retarget_effect_is_the_teardown_of_the_old_channel() {
        let (mut attacher, _) = Attacher::new(Some(PathBuf::from("/r/a")));
        attacher.handle(Ev::Connected { gen: 1, channel: 5 });
        attacher.handle(Ev::Installed {
            gen: 1,
            report: report(),
        });
        let effects = attacher.handle(Ev::Attach(PathBuf::from("/r/b")));

        let log = Rc::new(RefCell::new(Vec::new()));
        let mut live = Some(Live {
            gen: 1,
            link: Recorder(log.clone()),
            channel: 5,
        });
        let current = AtomicU64::new(1);
        let spawned = log.clone();
        let cancel = run_effects(effects, &mut live, &current, &mut |gen, addr| {
            spawned.borrow_mut().push(format!("spawn({gen},{})", addr.display()));
        });
        assert_eq!(
            *log.borrow(),
            [
                "exec_lua(TEARDOWN_LUA,[Integer(PosInt(5))])",
                "close_when_flushed",
                "spawn(2,/r/b)"
            ]
        );
        assert!(cancel, "the drafts waiting on the old editor end");
        assert!(live.is_none());
        assert_eq!(current.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn an_effect_for_another_generation_touches_no_link() {
        let log = Rc::new(RefCell::new(Vec::new()));
        let mut live = Some(Live {
            gen: 2,
            link: Recorder(log.clone()),
            channel: 6,
        });
        let current = AtomicU64::new(2);
        run_effects(
            vec![Effect::ExplicitTeardown { gen: 1 }, Effect::Close { gen: 1 }],
            &mut live,
            &current,
            &mut |_, _| panic!("no connect was asked for"),
        );
        assert!(log.borrow().is_empty());
        assert!(live.is_some());
    }

    #[test]
    fn the_editors_pid_is_the_peers_and_only_while_attached() {
        let addr = PathBuf::from("/r/a");
        let attached = LinkState::Attached {
            addr: addr.clone(),
            channel: 1,
            nvim_pid: 2,
            in_tmux: false,
        };
        // The pid the editor reported (2) plays no part: the peer's does.
        assert_eq!(attached_peer(&attached, Some(77)), Some(77));
        // No peer pid (a platform that gives none): no editor pid, never the reported one.
        assert_eq!(attached_peer(&attached, None), None);
        for state in [
            LinkState::NoEditor,
            LinkState::Connecting { addr: addr.clone() },
            LinkState::Attaching {
                addr: addr.clone(),
                waiting_for_key: false,
            },
            LinkState::Failed {
                addr,
                why: "x".to_owned(),
            },
        ] {
            assert_eq!(attached_peer(&state, Some(77)), None);
        }
    }

    #[test]
    fn a_reported_pid_that_is_not_the_peers_is_logged_once() {
        assert_eq!(
            pid_mismatch_line(10, Some(20)).as_deref(),
            Some("[companion] the editor reported pid 10, its socket is held by 20; using 20")
        );
        assert_eq!(pid_mismatch_line(20, Some(20)), None);
        assert_eq!(pid_mismatch_line(10, None), None);
    }

    #[test]
    fn not_attached_why_texts() {
        let addr = PathBuf::from("/r/a");
        let attached = LinkState::Attached {
            addr: addr.clone(),
            channel: 1,
            nvim_pid: 2,
            in_tmux: false,
        };
        assert_eq!(not_attached_why(&attached), None);
        for state in [
            LinkState::Connecting { addr: addr.clone() },
            LinkState::Attaching {
                addr: addr.clone(),
                waiting_for_key: true,
            },
        ] {
            assert_eq!(not_attached_why(&state), Some("the editor is still attaching"));
        }
        for state in [
            LinkState::NoEditor,
            LinkState::Detached {
                why: crate::companion::attach::Detach::Replaced,
            },
            LinkState::Failed {
                addr,
                why: "x".to_owned(),
            },
        ] {
            assert_eq!(
                not_attached_why(&state),
                Some("the editor is not connected to this panel")
            );
        }
    }

    #[test]
    fn get_mode_reply_reads_blocking() {
        let answer = |blocking: Value| {
            Value::Map(vec![
                (Value::from("mode"), Value::from("n")),
                (Value::from("blocking"), blocking),
            ])
        };
        assert_eq!(get_mode_blocking(&answer(Value::from(true))), Some(true));
        assert_eq!(get_mode_blocking(&answer(Value::from(false))), Some(false));
        assert_eq!(get_mode_blocking(&Value::Map(vec![])), None);
        assert_eq!(get_mode_blocking(&Value::from("n")), None);
    }

    fn own(addr: &str, channel: u64, pid: Option<u32>) -> OwnChannel {
        OwnChannel {
            addr: PathBuf::from(addr),
            channel,
            pid,
        }
    }

    #[test]
    fn a_channel_number_counts_as_ours_only_in_the_nvim_it_was_handed_out_by() {
        // Attached to A on channel 5, then to B on channel 3; another panel installs into B and is
        // given channel 5 there.
        let mut driver = LinkDriver::new(Some(PathBuf::from("/r/b")), Sockets::default());
        driver.own_channels.push_back(own("/r/a", 5, Some(100)));
        driver.own_channels.push_back(own("/r/b", 3, None));
        assert!(!driver.is_own_earlier_channel(5), "channel 5 was A's, not B's");
        assert!(driver.is_own_earlier_channel(3));
        assert!(!driver.is_own_earlier_channel(4));
    }

    #[test]
    fn an_nvim_restarted_on_the_same_address_does_not_inherit_the_old_channels() {
        let mut driver = LinkDriver::new(Some(PathBuf::from("/r/a")), Sockets::default());
        driver.own_channels.push_back(own("/r/a", 5, Some(100)));
        assert!(
            driver.is_own_earlier_channel(5),
            "pid unknown for the current connection yet"
        );
        driver.attacher.handle(Ev::Connected { gen: 1, channel: 7 });
        driver.attacher.handle(Ev::Installed {
            gen: 1,
            report: InstallReport {
                nvim_pid: 200,
                ..report()
            },
        });
        assert!(!driver.is_own_earlier_channel(5), "pid 100 is not pid 200");
    }

    #[test]
    fn a_thread_that_cannot_start_fails_the_connect() {
        let mut driver = LinkDriver::new(Some(PathBuf::from("/r/a")), Sockets::default());
        driver.spawn_thread = |_| Err(std::io::Error::other("no threads left"));
        let tick = driver.poll(Instant::now());
        assert!(tick.state_changed);
        assert!(
            matches!(driver.state(), LinkState::Failed { why, .. } if why.contains("no threads left") && why.contains("/r/a")),
            "{:?}",
            driver.state()
        );
    }

    #[test]
    fn a_driver_for_an_address_says_connecting_before_it_connects() {
        let driver = LinkDriver::new(Some(PathBuf::from("/r/a")), Sockets::default());
        assert!(matches!(driver.state(), LinkState::Connecting { .. }));
        let driver = LinkDriver::new(None, Sockets::default());
        assert_eq!(*driver.state(), LinkState::NoEditor);
        assert_eq!(driver.band().state, "none");
    }
}
