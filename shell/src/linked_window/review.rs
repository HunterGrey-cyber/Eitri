//! Turn review across linked windows: the windows of a group are one presence, every linked window reviews in window
//! 1's editor, and window 1 decides every revert of the group.
//!
//! **One presence.** Once linked, a window names the window at the link's other end as its partner (pid and start
//! time) with the flag the link thread keeps for it ([`LinkDriver::presence_flag`]): the partner's `windows/` file is
//! skipped by this window's revert checks only while that flag is clear, and the link thread sets it from the
//! handshake (neither window has confirmed its presence when it says hello or welcome) and from every
//! `presence{held}` before it acknowledges, whatever the GTK side is doing. Each window says `presence{held: true}`
//! on every link once its presence thread has confirmed its files, and `presence{held: false}` when a prompt's turn
//! could not be confirmed, then `true` again once it is. A partner is cleared when its link ends and before this
//! window closes it, so a partner it cannot see counts as another window again. Prompts held for the presence then go
//! out when the last link ended, but stay unsent with a note when this window closes it: the partner skips this
//! window's file until it reads the close. The link thread counts the partner as another window from its `bye` or
//! the link's end on. A prompt that told its partners it could not be confirmed waits for every one of them to
//! acknowledge, or for its link to end.
//!
//! **Overlap notes.** Every turn start and end of this window goes to every partner as `turn_edge` with its monotonic
//! time (a window with turn review off still says when its turns run), and each partner's edges (with those its
//! `hello` or `welcome` named) go into this window's turn review as that partner's turns, the edges of one drain of
//! the link together. Module windows are linked to window 1 alone, so window 1 passes each one's edges on to the
//! others as they came ([`GroupReview::pass_on_edges`]), says which turns run to a window that links later, and ends
//! the running turns of a window whose link ended: chats in two module windows get their overlap notes as tabs do.
//!
//! **One editor.** Window 1's review driver is the only one that talks to nvim's review module. A linked window's
//! overlay calls arrive as editor operations and are queued on that driver as that window's; what the driver finds
//! for it (an open's answer, what became of a file it drew, the reverts made in nvim for a session it holds, nothing
//! drawn any more) goes back on its link as `editor_reply`, `review_drawn`, `review_events` and `review_idle`. Its
//! flow drives an overlay proxy that keeps what is drawn from those, and asks window 1's nvim about a buffer through
//! the same editor operations.
//!
//! **Live drawings.** A linked window's live notes (a running turn's edited files, and its end) go into window 1's
//! driver as that window's, and while the turn runs window 1's nvim answers which of the files a window shows, as the
//! note's reply. Its live draws are queued on the driver under their operation's id, which window 1 reports back with
//! what the draw came to. A turn a linked window noted as running ends in window 1's table when its link ends: nobody
//! else will say so.
//!
//! **Window 1 decides.** A module window is linked to window 1 alone, so it cannot tell another module window's
//! presence from a stranger's. Its revert's worker, after its own checks, asks window 1 with `revert_guard` and waits
//! for `guard` (`HubGuard`); window 1 answers from its own check, which skips every linked partner, on a thread of
//! its own, since that check reads the presence directories. Window 2 and window 1 check for themselves.
//!
//! **Moved chats.** What a job finishes here for a session whose chat moved to the other window goes there as
//! `review_result`, and one that arrives is recorded by the tab that holds the session here. Only a pair moves a live
//! chat, so these go between window 1 and window 2.
//!
//! Nothing here waits: answers are polled by a repeating step the window drives (a GLib timer; a test steps it by
//! hand), and every message is queued on a link.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use eitri_core::nvim_rpc::Pending;
use eitri_core::review_editor::{LinkedEvent, OverlayCall, RemoteLiveAsk, RemoteOverlay, Window};
use eitri_core::turn_review::{GroupGuard, Held, Partner, RemoteAsk, SOLE_PARTNER};
use eitri_core::window_link::link::LinkSender;
use eitri_core::window_link::{EdgeKind, EditorOp, Msg, ReplyResult, TurnEdge};
use eitri_core::wm::PeerId;
use eitri_panel::agent_panel::{AgentPanelHandle, PairOutbound, ReviewSettled, ReviewStep};
use serde_json::Value;

use super::driver::LinkDriver;
use super::editor::{EditorOps, Repeat, Window1Editor};
use super::spawn::{HUB_PEER, SECOND_PEER};

/// How often the answers this window waits for are looked at: an open's from window 1, its own presence
/// thread's confirmation, a guard check's for a module window.
const POLL_EVERY: Duration = Duration::from_millis(20);

/// How long after its presence thread said it could not hold a file the window asks it again.
const ASK_AGAIN_AFTER: Duration = Duration::from_secs(1);

/// What a window 1 with no review driver to queue a linked window's calls on answers them.
pub const NO_DRIVER: &str = "the editor window has no review overlay to draw in";

/// What a window 1 that holds no presence answers a module window's guard.
const NO_PRESENCE: &str = "the editor window holds no presence on this project";

/// The window's turn review as this file reaches it: the panel in a window, a bare tab set and flow in a test.
pub trait ReviewSide {
    /// Runs `step` on the window's review flow, its tab set and its editor; what it leaves goes to the pages.
    fn with(&self, step: ReviewStep<'_>);
}

impl ReviewSide for AgentPanelHandle {
    fn with(&self, step: ReviewStep<'_>) {
        self.with_turn_review(step);
    }
}

/// Which window this is.
pub enum Role {
    /// Window 1: the editor is here; every linked window's overlay calls are queued on this window's driver, and
    /// this window answers the module windows' guards.
    One(Rc<Window1Editor>),
    /// Window 2: its review reaches window 1's editor through these operations, and it checks its reverts itself.
    Two(Rc<EditorOps>),
    /// A module window: its review reaches window 1's editor as window 2's does, and its reverts ask window 1.
    Module(Rc<EditorOps>),
}

/// The presence thread's confirmation, asked for so the partners can be told this window holds its presence.
struct Confirming {
    answer: Option<mpsc::Receiver<Held>>,
    /// When to ask again after the thread could not hold the files.
    again_at: Option<Instant>,
}

/// What a module window's revert workers share with its GTK side: the way to window 1, and the workers waiting for
/// an answer, by job. A worker never reaches the GTK side: it sends on the link's own queue, and the answer is put
/// here when the link's messages are read.
#[derive(Default)]
struct HubGuard {
    /// The link to window 1, while it is up.
    link: Mutex<Option<LinkSender>>,
    waiting: Mutex<BTreeMap<u64, mpsc::Sender<Result<(), String>>>>,
    jobs: AtomicU64,
    #[cfg(test)]
    wait: Mutex<
        Option<
            Arc<
                dyn Fn(
                        &mpsc::Receiver<Result<(), String>>,
                        Duration,
                    ) -> Result<Result<(), String>, mpsc::RecvTimeoutError>
                    + Send
                    + Sync,
            >,
        >,
    >,
}

impl HubGuard {
    fn linked(&self, link: Option<LinkSender>) {
        *self.link.lock().unwrap_or_else(|e| e.into_inner()) = link;
    }

    /// The link ended: every worker waiting hears at once that no answer will come, as its sender goes.
    fn ended(&self) {
        let mut link = self.link.lock().unwrap_or_else(|e| e.into_inner());
        *link = None;
        self.waiting.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }

    /// Window 1's answer to `job`, for a worker that still waits. The link's messages are read behind its thread, so
    /// an answer read here may come from a link that has already ended: its worker hears the end instead, as the
    /// sender goes.
    fn answered(&self, job: u64, answer: Result<(), String>) {
        let waiting = self.waiting.lock().unwrap_or_else(|e| e.into_inner()).remove(&job);
        let up = self
            .link
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .is_some_and(|link| !link.is_closed());
        if let (Some(waiting), true) = (waiting, up) {
            let _ = waiting.send(answer);
        }
    }
}

impl GroupGuard for HubGuard {
    fn ask(&self) -> Option<(u64, mpsc::Receiver<Result<(), String>>)> {
        // Held while the worker is filed, so a link ending now finds it and lets it go.
        let link = self.link.lock().unwrap_or_else(|e| e.into_inner());
        let link = link.as_ref()?;
        let job = self.jobs.fetch_add(1, AtomicOrdering::SeqCst) + 1;
        let (answer, answered) = mpsc::channel();
        self.waiting
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(job, answer);
        if link.send(Msg::RevertGuard { job }) {
            Some((job, answered))
        } else {
            self.gave_up(job);
            None
        }
    }

    #[cfg(test)]
    fn wait_for_answer_for_tests(
        &self,
        answer: &mpsc::Receiver<Result<(), String>>,
        within: Duration,
    ) -> Result<Result<(), String>, mpsc::RecvTimeoutError> {
        let hook = self.wait.lock().unwrap().clone();
        match hook {
            Some(hook) => hook(answer, within),
            None => answer.recv_timeout(within),
        }
    }

    fn gave_up(&self, job: u64) {
        self.waiting.lock().unwrap_or_else(|e| e.into_inner()).remove(&job);
    }
}

/// A guard check running for a module window: its peer, its job and the answer to come.
struct GuardCheck {
    peer: PeerId,
    job: u64,
    answer: mpsc::Receiver<Result<(), String>>,
}

/// One link's part in the window's turn review.
struct LinkReview {
    peer: PeerId,
    link: LinkDriver,
    /// The partner at the other end, while the handshake named one.
    partner: Option<u32>,
    /// Whom this link's turn edges are filed under in the overlap scan: its pid for a module window, the sole partner
    /// for window 2 and for window 1 as a linked window sees it, since a chat that moved goes on as that partner's turn.
    /// In a module window the turns window 1 passes on go under that one partner too: a session is one chat's, so they
    /// stay apart.
    turns_of: u32,
    /// The partner's turn edges taken from the link in one drain, applied together once it is drained: a start
    /// and its end that arrived together are one interval, not a turn still running.
    edges_in: RefCell<Vec<TurnEdge>>,
    /// Window 1 only: the turns running at this link's other end, with their start times.
    turns_running: RefCell<BTreeMap<String, u64>>,
    /// This link has yet to hear `presence{held: true}`.
    owes_held: Cell<bool>,
}

impl LinkReview {
    fn send_edge(&self, edge: &TurnEdge) {
        self.send(Msg::TurnEdge {
            session: edge.session.clone(),
            edge: edge.edge,
            at_ns: edge.at_ns,
        });
    }

    fn send(&self, msg: Msg) {
        if !self.link.linked() {
            return;
        }
        let kind = msg.kind();
        if let Err(err) = self.link.send(msg, Vec::new()) {
            eprintln!("[review] {kind} could not be sent to the other window: {err}");
        }
    }
}

/// One window's turn review on its links: window 1's on every link it has, any other window's on its link to window 1.
pub struct GroupReview {
    me: Weak<GroupReview>,
    root: PathBuf,
    side: Rc<dyn ReviewSide>,
    role: Role,
    repeat: Repeat,
    links: RefCell<BTreeMap<PeerId, Rc<LinkReview>>>,
    /// A linked window's opens sent to window 1, each answered by the driver there once it has the call's outcome.
    opens: Rc<RefCell<Vec<(u64, Pending)>>>,
    /// The confirmation this window waits for before it says `presence{held: true}`; `None` once said.
    confirming: RefCell<Option<Confirming>>,
    /// The links told `presence{held: false}` that have not acknowledged: a prompt waits for them all, since until
    /// then any of them may skip this window's file.
    acks_owed: RefCell<BTreeSet<PeerId>>,
    polling: Cell<bool>,
    /// The `review_result`s sent, which number them.
    results_sent: Cell<u64>,
    /// In a module window: how its reverts reach window 1.
    hub: Option<Arc<HubGuard>>,
    /// In window 1: the module windows' guard checks not yet answered.
    guard_checks: RefCell<Vec<GuardCheck>>,
}

impl GroupReview {
    /// Window 1's (`Role::One`): every linked window's overlay calls go to this window's driver from now on. Any
    /// other's: its review flow reaches window 1's editor from now on, and while unlinked has none.
    pub fn new(root: &Path, side: Rc<dyn ReviewSide>, role: Role, repeat: Repeat) -> Rc<GroupReview> {
        let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
        let hub = matches!(role, Role::Module(_)).then(Arc::default);
        let review = Rc::new_cyclic(|me| GroupReview {
            me: me.clone(),
            root,
            side,
            role,
            repeat,
            links: RefCell::default(),
            opens: Rc::default(),
            confirming: RefCell::default(),
            acks_owed: RefCell::default(),
            polling: Cell::new(false),
            results_sent: Cell::new(0),
            hub,
            guard_checks: RefCell::default(),
        });
        match &review.role {
            Role::One(editor) => review.serve_linked_windows(editor),
            Role::Two(ops) | Role::Module(ops) => review.reach_window_1(ops),
        }
        if let Some(hub) = &review.hub {
            let hub: Arc<dyn GroupGuard> = hub.clone();
            review.side.with(&mut |_, tabs, _| {
                if let Some(guard) = tabs.presence_guard() {
                    guard.set_group(hub.clone());
                }
                ReviewSettled::default()
            });
        }
        review
    }

    /// Window 1: a linked window's overlay calls are queued on this window's driver, tagged as that window's, behind
    /// every call of its own. An open is answered by the driver once it has the outcome; the others at once.
    fn serve_linked_windows(&self, editor: &Window1Editor) {
        let side = Rc::downgrade(&self.side);
        editor.set_overlay(Rc::new(move |peer: PeerId, id: u64, calls: Vec<OverlayCall>| {
            let side = side.upgrade()?;
            let opens = calls.iter().any(|call| matches!(call, OverlayCall::OpenAndShow { .. }));
            let mut calls = Some(calls);
            let mut queued = false;
            side.with(&mut |flow, _, rpc| {
                if let (Some(overlay), Some(rpc), Some(calls)) = (flow.local_overlay(), rpc, calls.take()) {
                    let now = Instant::now();
                    for call in calls {
                        overlay.call_for_op(rpc, call, Window::Linked(peer), Some(id), now);
                    }
                    queued = true;
                }
                ReviewSettled::default()
            });
            if !queued {
                Some(ReplyResult::Err { why: NO_DRIVER.into() })
            } else if opens {
                None
            } else {
                Some(ReplyResult::Ok { value: Value::Null })
            }
        }));
        let side = Rc::downgrade(&self.side);
        editor.set_live_note(Rc::new(move |peer: PeerId, note| {
            let Some(side) = side.upgrade() else { return false };
            let mut taken = false;
            side.with(&mut |flow, _, _| {
                taken = flow.note_linked_live(peer, &note.key, note.started_ns, note.ended_ns, &note.paths);
                ReviewSettled::default()
            });
            taken
        }));
        let side = Rc::downgrade(&self.side);
        editor.set_drawn_for(Rc::new(move |peer: PeerId| {
            let mut drawn = Vec::new();
            if let Some(side) = side.upgrade() {
                side.with(&mut |flow, _, _| {
                    if let Some(overlay) = flow.local_overlay() {
                        drawn = overlay.linked_paths(peer).iter().cloned().collect();
                    }
                    ReviewSettled::default()
                });
            }
            drawn
        }));
    }

    /// A linked window: the flow's overlay calls and buffer questions go to window 1 as editor operations.
    fn reach_window_1(&self, ops: &Rc<EditorOps>) {
        let sending = ops.clone();
        let opens = self.opens.clone();
        let me = self.me.clone();
        let overlay = RemoteOverlay::new(
            &self.root,
            Box::new(move |op: EditorOp| {
                let open = matches!(op, EditorOp::ReviewOpenAndShow { .. });
                let (id, pending) = sending.send_with_id(op);
                if open {
                    opens.borrow_mut().push((id, pending));
                    if let Some(me) = me.upgrade() {
                        me.poll_soon();
                    }
                }
                id
            }),
        );
        let asking = ops.clone();
        let ask = RemoteAsk::new(
            &self.root,
            Box::new(move |op: EditorOp| asking.is_linked().then(|| asking.send(op))),
        );
        let noting = ops.clone();
        let live_ask = RemoteLiveAsk::new(
            &self.root,
            Box::new(move |op: EditorOp| noting.is_linked().then(|| noting.send(op))),
        );
        let mut parts = Some((overlay, ask, live_ask));
        self.side.with(&mut |flow, _, _| {
            if let Some((overlay, ask, live_ask)) = parts.take() {
                flow.link_to_window_1(overlay, ask);
                flow.set_live_ask(Box::new(live_ask));
            }
            ReviewSettled::default()
        });
    }

    fn is_window_1(&self) -> bool {
        matches!(self.role, Role::One(_))
    }

    /// The live links, in peer order.
    fn up_links(&self) -> Vec<Rc<LinkReview>> {
        self.links
            .borrow()
            .values()
            .filter(|state| state.link.linked())
            .cloned()
            .collect()
    }

    /// Sends `msg` on the link to `peer`, if it is up.
    fn send(&self, peer: PeerId, msg: Msg) {
        let state = self.links.borrow().get(&peer).cloned();
        if let Some(state) = state {
            state.send(msg);
        }
    }

    /// The state of `link`, when it is still the link to `peer`: a late message of an ended link is not its
    /// successor's.
    fn state_of(&self, peer: PeerId, link: &LinkDriver) -> Option<Rc<LinkReview>> {
        self.links
            .borrow()
            .get(&peer)
            .filter(|state| state.link.same(link))
            .cloned()
    }

    /// A chat of `session` is moving between the windows: in window 1, which runs nvim, the editor's review events
    /// of that session wait (`hold`) until the move has ended, then go to the window holding the session, this
    /// one when `here`. Only a pair moves a live chat, so the other window is window 2. No other window holds
    /// anything: nvim's events reach it only through window 1.
    pub fn hold_session(&self, session: &str, hold: bool, here: bool) -> Result<(), String> {
        if !self.is_window_1() {
            return Err("only the window that runs the editor holds its review events".into());
        }
        let mut done = false;
        self.side.with(&mut |flow, _, _| {
            if let Some(overlay) = flow.local_overlay() {
                if hold {
                    overlay.hold_session(session);
                } else {
                    let to = if here { Window::One } else { Window::Linked(SECOND_PEER) };
                    overlay.release_session(session, to);
                }
                done = true;
            }
            ReviewSettled::default()
        });
        if done {
            Ok(())
        } else {
            Err(NO_DRIVER.into())
        }
    }

    /// The turns running here now, as started edges: what this window's `welcome` carries.
    pub fn running_turns(&self) -> Vec<TurnEdge> {
        let mut edges = Vec::new();
        self.side.with(&mut |_, tabs, _| {
            edges = match tabs.turn_review() {
                Some(review) => review.running_turn_edges(),
                None => tabs.untracked_running_turn_edges(),
            };
            ReviewSettled::default()
        });
        edges
    }

    /// The link to `peer` is up: the partner is named to this window's presence, its turns are taken into the overlap
    /// scan, this window's turn edges are kept for its partners, and the link's review messages are taken from here
    /// on.
    pub fn linked(&self, peer: PeerId, link: &LinkDriver) {
        let window_1 = self.is_window_1();
        let partner = link.partner();
        let turns_of = match partner {
            Some(partner) if window_1 && peer != SECOND_PEER => partner.pid,
            _ => SOLE_PARTNER,
        };
        let state = Rc::new(LinkReview {
            peer,
            link: link.clone(),
            partner: partner.map(|partner| partner.pid),
            turns_of,
            edges_in: RefCell::default(),
            turns_running: RefCell::default(),
            owes_held: Cell::new(true),
        });
        let first = !self.links.borrow().values().any(|state| state.partner.is_some());
        self.links.borrow_mut().insert(peer, state.clone());
        self.take_messages(peer, link);
        if let Some(hub) = &self.hub {
            hub.linked(link.sender());
        }
        let me = self.me.clone();
        link.on_closing(move |link| {
            if let Some(me) = me.upgrade().filter(|me| me.state_of(peer, link).is_some()) {
                me.end(peer, true);
            }
        });
        if let Some(partner) = partner {
            let mut partner = Some(Partner {
                pid: partner.pid,
                start: partner.start,
                counts_as_other: link.presence_flag(),
            });
            let named_running = link.take_partner_running();
            let mut running = Some(named_running.clone());
            self.side.with(&mut |_, tabs, _| {
                let sent = match partner.take() {
                    Some(partner) if window_1 => {
                        tabs.add_partner(partner);
                        Vec::new()
                    }
                    partner => tabs.set_partner(partner),
                };
                if let Some(review) = tabs.turn_review_mut() {
                    // Edges kept for the links already up stay theirs.
                    if first {
                        review.announce_turn_edges(true);
                    }
                    review.partner_turns_of(turns_of, running.take().unwrap_or_default());
                }
                ReviewSettled::sent(sent)
            });
            self.pass_on_edges(&state, &named_running);
        } else {
            eprintln!("[review] the link names no partner; the other window counts as another window");
        }
        self.tell_running_turns(&state);
        self.confirm_presence();
    }

    /// Window 1 passes `from`'s turn edges on to its other links as they came, and keeps which of `from`'s turns run.
    /// A module window hears no other module window directly, and its overlap notes need their turns.
    fn pass_on_edges(&self, from: &LinkReview, edges: &[TurnEdge]) {
        if !self.is_window_1() || edges.is_empty() {
            return;
        }
        {
            let mut running = from.turns_running.borrow_mut();
            for edge in edges {
                match edge.edge {
                    EdgeKind::Started => {
                        running.insert(edge.session.clone(), edge.at_ns);
                    }
                    EdgeKind::Ended => {
                        running.remove(&edge.session);
                    }
                }
            }
        }
        for state in self.up_links().iter().filter(|state| state.peer != from.peer) {
            for edge in edges {
                state.send_edge(edge);
            }
        }
    }

    /// Window 1 tells a window that has just linked which turns run in the windows linked before it.
    fn tell_running_turns(&self, to: &LinkReview) {
        if !self.is_window_1() {
            return;
        }
        for state in self.up_links().iter().filter(|state| state.peer != to.peer) {
            for (session, at_ns) in state.turns_running.borrow().iter() {
                to.send_edge(&TurnEdge {
                    session: session.clone(),
                    edge: EdgeKind::Started,
                    at_ns: *at_ns,
                });
            }
        }
    }

    fn take_messages(&self, peer: PeerId, link: &LinkDriver) {
        let me = self.me.clone();
        link.on("turn_edge", move |link, msg, _| {
            let (Some(me), Msg::TurnEdge { session, edge, at_ns }) = (me.upgrade(), msg) else {
                return;
            };
            if let Some(state) = me.state_of(peer, link) {
                state.edges_in.borrow_mut().push(TurnEdge { session, edge, at_ns });
            }
        });
        let me = self.me.clone();
        link.on_drained(move |link| {
            if let Some(me) = me.upgrade() {
                if let Some(state) = me.state_of(peer, link) {
                    me.apply_partner_edges(&state);
                }
            }
        });
        // The link thread has already set the partner's flag and acknowledged.
        link.on("presence", |_, msg, _| {
            if let Msg::Presence { held } = msg {
                println!("[review] the other window says its presence is held: {held}");
            }
        });
        let me = self.me.clone();
        link.on("presence_ack", move |_, msg, _| {
            let (Some(me), Msg::PresenceAck { held: false }) = (me.upgrade(), msg) else {
                return;
            };
            let all_heard = {
                let mut owed = me.acks_owed.borrow_mut();
                owed.remove(&peer);
                owed.is_empty()
            };
            if all_heard {
                me.side
                    .with(&mut |_, tabs, _| ReviewSettled::sent(tabs.on_presence_ack(Instant::now())));
            }
        });
        let me = self.me.clone();
        link.on("review_result", move |_, msg, _| {
            let (Some(me), Msg::ReviewResult { session, job, result }) = (me.upgrade(), msg) else {
                return;
            };
            let mut result = Some(result);
            me.side.with(&mut |flow, tabs, _| {
                let Some(result) = result.take() else {
                    return ReviewSettled::default();
                };
                match flow.review_result(tabs, &session, result) {
                    Ok(outs) => ReviewSettled::outs(outs),
                    Err(why) => {
                        eprintln!("[review] result {job} for {session} from the other window was dropped: {why}");
                        ReviewSettled::default()
                    }
                }
            });
        });
        match self.role {
            Role::One(_) => self.answer_guards(peer, link),
            Role::Two(_) => self.take_window_1_reports(link),
            Role::Module(_) => {
                self.take_window_1_reports(link);
                self.take_guard_answers(link);
            }
        }
    }

    /// A linked window: what window 1's driver reports about this window's overlays.
    fn take_window_1_reports(&self, link: &LinkDriver) {
        let me = self.me.clone();
        link.on("review_drawn", move |_, msg, _| {
            if let (Some(me), Msg::ReviewDrawn { id, path, state }) = (me.upgrade(), msg) {
                me.with_remote(|overlay| overlay.review_drawn_by(id, &path, state));
            }
        });
        let me = self.me.clone();
        link.on("review_idle", move |_, _, _| {
            if let Some(me) = me.upgrade() {
                me.with_remote(|overlay| overlay.review_idle());
            }
        });
        let me = self.me.clone();
        link.on("review_events", move |_, msg, _| {
            if let (Some(me), Msg::ReviewEvents { session, events }) = (me.upgrade(), msg) {
                let mut events = Some(events);
                me.with_remote(|overlay| overlay.review_events(&session, events.take().unwrap_or_default()));
            }
        });
    }

    /// A module window: window 1's answers go to the workers waiting for them.
    fn take_guard_answers(&self, link: &LinkDriver) {
        let me = self.me.clone();
        link.on("guard", move |_, msg, _| {
            let (Some(me), Msg::Guard { job, ok, why }) = (me.upgrade(), msg) else {
                return;
            };
            if let Some(hub) = &me.hub {
                let answer = if ok {
                    Ok(())
                } else {
                    Err(why.unwrap_or_else(|| "the editor window gave no reason".to_string()))
                };
                hub.answered(job, answer);
            }
        });
    }

    /// Window 1: a module window's `revert_guard` is answered from this window's check, which sees the whole group.
    fn answer_guards(&self, peer: PeerId, link: &LinkDriver) {
        let me = self.me.clone();
        link.on("revert_guard", move |_, msg, _| {
            if let (Some(me), Msg::RevertGuard { job }) = (me.upgrade(), msg) {
                me.check_for(peer, job);
            }
        });
    }

    /// Runs the guard check for the window at `peer` on a thread of its own, since it reads the presence directories;
    /// its answer goes on the link from the repeating step.
    fn check_for(&self, peer: PeerId, job: u64) {
        let mut guard = None;
        self.side.with(&mut |_, tabs, _| {
            guard = tabs.presence_guard();
            ReviewSettled::default()
        });
        let Some(guard) = guard else {
            self.send_guard(peer, job, Err(NO_PRESENCE.to_string()));
            return;
        };
        let (answer, answered) = mpsc::channel();
        let started = std::thread::Builder::new()
            .name("eitri-review-guard".into())
            .spawn(move || {
                let _ = answer.send(guard.check().map_err(|blocked| blocked.to_string()));
            });
        match started {
            Ok(_) => {
                self.guard_checks.borrow_mut().push(GuardCheck {
                    peer,
                    job,
                    answer: answered,
                });
                self.poll_soon();
            }
            Err(err) => self.send_guard(peer, job, Err(format!("the check could not start: {err}"))),
        }
    }

    fn send_guard(&self, peer: PeerId, job: u64, answer: Result<(), String>) {
        let (ok, why) = match answer {
            Ok(()) => (true, None),
            Err(why) => (false, Some(why)),
        };
        self.send(peer, Msg::Guard { job, ok, why });
    }

    /// The partner's turn edges taken since the last drain, into the overlap scan in one go.
    fn apply_partner_edges(&self, state: &LinkReview) {
        let edges = std::mem::take(&mut *state.edges_in.borrow_mut());
        if edges.is_empty() {
            return;
        }
        self.pass_on_edges(state, &edges);
        let mut edges = Some(edges);
        self.side.with(&mut |_, tabs, _| {
            if let (Some(review), Some(edges)) = (tabs.turn_review_mut(), edges.take()) {
                review.partner_turns_of(state.turns_of, edges);
            }
            ReviewSettled::default()
        });
    }

    fn with_remote(&self, mut step: impl FnMut(&mut RemoteOverlay)) {
        self.side.with(&mut |flow, _, _| {
            if let Some(overlay) = flow.remote_overlay() {
                step(overlay);
            }
            ReviewSettled::default()
        });
    }

    /// The link to `peer` ended: that partner counts as another window again (once the last is gone, the prompts held
    /// for the presence go out), its running turns end in the overlap scan, and nothing more is kept for it. Window 1
    /// clears every file drawn for that window; a linked window's overlays are known drawn no more, and what waited on
    /// window 1's editor is told it will not be answered.
    pub fn unlinked(&self, peer: PeerId) {
        self.end(peer, false);
    }

    /// [`unlinked`](Self::unlinked), or, with `closing`, this window is about to close the link itself: then the
    /// prompts held for the presence are not sent but wait in their queues with a note, since the partner skips
    /// this window's file until it reads the close, and that file was not confirmed to mark their turns.
    fn end(&self, peer: PeerId, closing: bool) {
        let Some(state) = self.links.borrow_mut().remove(&peer) else {
            return;
        };
        // Edges already read are still the partner's turns: they go into the scan before it is told the partner
        // is gone.
        self.apply_partner_edges(&state);
        // Whatever it still ran ends now for the windows that heard it started: nobody else will say so.
        #[cfg(not(test))]
        let at_ns = eitri_core::turn_review::monotonic_ns();
        #[cfg(test)]
        let at_ns = tests::mono_now();
        let ended: Vec<TurnEdge> = std::mem::take(&mut *state.turns_running.borrow_mut())
            .into_keys()
            .map(|session| TurnEdge {
                session,
                edge: EdgeKind::Ended,
                at_ns,
            })
            .collect();
        self.pass_on_edges(&state, &ended);
        let last = self.links.borrow().is_empty();
        if last {
            self.confirming.borrow_mut().take();
        }
        // A prompt that waited only for this link's acknowledgement waits for nothing now, unless this window is the
        // one closing it: the partner goes on skipping this window's file until it reads the close.
        let all_heard = {
            let mut owed = self.acks_owed.borrow_mut();
            owed.remove(&peer) && owed.is_empty() && !closing
        };
        let window_1 = self.is_window_1();
        self.side.with(&mut |flow, tabs, _| {
            let mut sent = match (window_1, state.partner) {
                (true, Some(pid)) => tabs.remove_partner(pid, closing),
                (true, None) => Vec::new(),
                (false, _) if closing => tabs.close_link(),
                (false, _) => tabs.set_partner(None),
            };
            if all_heard && !last {
                sent.extend(tabs.on_presence_ack(Instant::now()));
            }
            if let Some(review) = tabs.turn_review_mut() {
                if last {
                    review.announce_turn_edges(false);
                }
                review.partner_gone_of(state.turns_of);
            }
            let outs = if window_1 {
                if let Some(overlay) = flow.local_overlay() {
                    overlay.linked_gone(peer);
                }
                flow.linked_live_gone(peer, at_ns);
                Vec::new()
            } else {
                flow.editor_lost()
            };
            ReviewSettled { outs, sent }
        });
        if let Some(hub) = &self.hub {
            hub.ended();
        }
        if !window_1 {
            self.opens.borrow_mut().clear();
        }
    }

    /// What the window's panel left for the other windows after a tick.
    pub fn outbound(&self, out: PairOutbound) {
        let PairOutbound {
            linked,
            edges,
            presence_false,
            results,
        } = out;
        let links = self.up_links();
        if links.is_empty() {
            return;
        }
        for (peer, events) in linked {
            for event in events {
                if let Some(msg) = self.linked_msg(event) {
                    self.send(peer, msg);
                }
            }
        }
        for edge in edges {
            for state in &links {
                state.send_edge(&edge);
            }
        }
        let holder = if self.is_window_1() { SECOND_PEER } else { HUB_PEER };
        for (session, result) in results {
            let job = self.results_sent.get() + 1;
            self.results_sent.set(job);
            self.send(holder, Msg::ReviewResult { session, job, result });
        }
        if presence_false {
            for state in &links {
                state.owes_held.set(true);
                state.send(Msg::Presence { held: false });
            }
            *self.acks_owed.borrow_mut() = links.iter().map(|state| state.peer).collect();
            // Asked afresh: an answer already on its way was for a set before this turn's.
            self.confirming.borrow_mut().take();
            self.confirm_presence();
        }
    }

    /// One of window 1's driver's reports for a linked window, as its message; paths go relative to the project root.
    fn linked_msg(&self, event: LinkedEvent) -> Option<Msg> {
        Some(match event {
            LinkedEvent::Answered { op_id, result } => Msg::EditorReply {
                id: op_id,
                result: match result {
                    Ok(message) => ReplyResult::Ok {
                        value: Value::String(message),
                    },
                    Err(why) => ReplyResult::Err { why },
                },
            },
            LinkedEvent::Drawn { op_id, path, state } => {
                let Some(rel) = path.strip_prefix(&self.root).ok().and_then(Path::to_str) else {
                    eprintln!(
                        "[review] a linked window drew {}, outside the project; not reported",
                        path.display()
                    );
                    return None;
                };
                Msg::ReviewDrawn {
                    id: op_id,
                    path: rel.to_owned(),
                    state,
                }
            }
            LinkedEvent::Events { session, events } => Msg::ReviewEvents { session, events },
            LinkedEvent::Idle => Msg::ReviewIdle,
        })
    }

    /// Asks the presence thread to confirm this window's files, and says `presence{held: true}` once it has. A
    /// window that holds no presence never says it: its partners go on counting it as another window.
    fn confirm_presence(&self) {
        let Some(answer) = self.ask_confirmation() else {
            eprintln!("[review] this window holds no presence; the other window counts it as another window");
            return;
        };
        *self.confirming.borrow_mut() = Some(Confirming {
            answer: Some(answer),
            again_at: None,
        });
        self.poll_soon();
    }

    fn ask_confirmation(&self) -> Option<mpsc::Receiver<Held>> {
        let mut answer = None;
        self.side.with(&mut |_, tabs, _| {
            answer = tabs.presence_confirmation();
            ReviewSettled::default()
        });
        answer
    }

    fn poll_soon(&self) {
        if self.polling.replace(true) {
            return;
        }
        let me = self.me.clone();
        (self.repeat)(
            POLL_EVERY,
            Box::new(move || {
                let Some(me) = me.upgrade() else { return false };
                let more = me.poll(Instant::now());
                if !more {
                    me.polling.set(false);
                }
                more
            }),
        );
    }

    /// Looks at the answers this window waits for; whether any is still out.
    pub fn poll(&self, now: Instant) -> bool {
        self.poll_opens();
        self.poll_confirmation(now);
        self.poll_guard_checks();
        !self.opens.borrow().is_empty() || self.confirming.borrow().is_some() || !self.guard_checks.borrow().is_empty()
    }

    fn poll_guard_checks(&self) {
        let mut finished = Vec::new();
        self.guard_checks
            .borrow_mut()
            .retain(|check| match check.answer.try_recv() {
                Err(mpsc::TryRecvError::Empty) => true,
                Ok(answer) => {
                    finished.push((check.peer, check.job, answer));
                    false
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    finished.push((check.peer, check.job, Err("the check stopped".to_string())));
                    false
                }
            });
        for (peer, job, answer) in finished {
            self.send_guard(peer, job, answer);
        }
    }

    /// The revert workers waiting for window 1.
    #[cfg(test)]
    fn guards_waiting(&self) -> usize {
        self.hub
            .as_ref()
            .map_or(0, |hub| hub.waiting.lock().unwrap_or_else(|e| e.into_inner()).len())
    }

    /// The newest job waiting for window 1.
    #[cfg(test)]
    fn guard_job(&self) -> Option<u64> {
        let hub = self.hub.as_ref()?;
        let waiting = hub.waiting.lock().unwrap_or_else(|e| e.into_inner());
        waiting.keys().next_back().copied()
    }

    fn poll_opens(&self) {
        let answered: Vec<(u64, Result<String, String>)> = {
            let mut opens = self.opens.borrow_mut();
            let mut answered = Vec::new();
            opens.retain(|(id, pending)| match pending.try_take() {
                None => true,
                Some(reply) => {
                    answered.push((
                        *id,
                        match reply {
                            Ok(rmpv::Value::String(text)) => Ok(text.into_str().unwrap_or_default()),
                            Ok(other) => Ok(other.to_string()),
                            Err(err) => Err(err.to_string()),
                        },
                    ));
                    false
                }
            });
            answered
        };
        if answered.is_empty() {
            return;
        }
        let mut answered = Some(answered);
        self.with_remote(|overlay| {
            for (id, result) in answered.take().unwrap_or_default() {
                overlay.answered(id, result);
            }
        });
    }

    fn poll_confirmation(&self, now: Instant) {
        let held = {
            let mut confirming = self.confirming.borrow_mut();
            let Some(wait) = confirming.as_mut() else { return };
            match &wait.answer {
                Some(answer) => match answer.try_recv() {
                    Ok(held) => Some(held),
                    Err(mpsc::TryRecvError::Empty) => None,
                    Err(mpsc::TryRecvError::Disconnected) => Some(Held::No("the presence thread stopped".into())),
                },
                None => None,
            }
        };
        match held {
            Some(Held::Yes) => {
                self.confirming.borrow_mut().take();
                for state in self.up_links() {
                    if state.owes_held.replace(false) {
                        state.send(Msg::Presence { held: true });
                    }
                }
            }
            Some(Held::No(why)) => {
                eprintln!("[review] this window's presence is not confirmed yet ({why}); asked again shortly");
                if let Some(wait) = self.confirming.borrow_mut().as_mut() {
                    wait.answer = None;
                    wait.again_at = Some(now + ASK_AGAIN_AFTER);
                }
            }
            Some(Held::NotYet) | None => {}
        }
        let again = self
            .confirming
            .borrow()
            .as_ref()
            .is_some_and(|wait| wait.answer.is_none() && wait.again_at.is_some_and(|at| now >= at));
        if again {
            match self.ask_confirmation() {
                Some(answer) => {
                    if let Some(wait) = self.confirming.borrow_mut().as_mut() {
                        wait.again_at = None;
                        wait.answer = Some(answer);
                    }
                }
                None => {
                    self.confirming.borrow_mut().take();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_guard_without_a_hook_uses_the_supplied_deadline() {
        let guard = HubGuard::default();
        let (_send, answer) = mpsc::channel();
        assert_eq!(
            guard.wait_for_answer_for_tests(&answer, Duration::ZERO),
            Err(mpsc::RecvTimeoutError::Timeout)
        );
    }

    use crate::linked_window::driver::Partner as LinkPartner;
    use crate::linked_window::editor::Window2Editor;
    use crate::linked_window::tests::Scratch;
    use agent::{AgentConversation, AgentDomainEvent, TurnOutcome};
    use eitri_core::agent_backend::{AgentBackend, AgentId, BackendKind};
    use eitri_core::agent_bridge::{parse_inbound_message, InboundMessage, SessionModeChoice};
    use eitri_core::editor_rpc::EditorRpc;
    use eitri_core::review_editor::{
        Owner, CLEAR_LUA, LIVE_CLEAR_LUA, LIVE_SHOWN_LUA, LIVE_SHOW_LUA, OPEN_AND_SHOW_LUA, REVIEW_LUA, SHOW_LUA,
        TAKE_EVENTS_LUA,
    };
    use eitri_core::tab_set::{FirstTurn, Sent, TabBackend, TabSet, ViewId};
    use eitri_core::tabs::TabId;
    use eitri_core::test_providers::RecordingProvider;
    use eitri_core::turn_review::live_contest::TurnKey;
    use eitri_core::turn_review::revert::BUFFER_STATE_LUA;
    use eitri_core::turn_review::{
        review_dir_for, Out, PresenceHolder, ReviewFlow, ReviewOptions, Scope, SlotBusy, Snap, TurnReview, WriteSlot,
    };
    use eitri_core::window_link::fdpass;
    use eitri_core::window_link::link::{LinkHandle, PresenceFlag};
    use eitri_core::window_link::names::WindowName;
    use eitri_core::window_link::EdgeKind;
    use eitri_panel::agent_panel::take_pair_outbound;
    use rmpv::Value as M;
    use serde_json::{json, Value as Json};
    use std::collections::BTreeSet;
    use std::sync::atomic::{AtomicBool, Ordering};

    const BASE: &[u8] = b"one\ntwo\nthree\n";
    const CHANGED: &[u8] = b"one\nTWO\nthree\n";
    const AGAIN: &[u8] = b"one\nTWO\nTHREE\n";
    const BUSY_TURN: &str = "an agent turn is running";
    const OTHER_WINDOW: &str = "another Eitri window has this project open";

    /// Which side of a [`Pair`] a step of a test acts on.
    #[derive(Debug, Clone, Copy)]
    enum Window {
        One,
        Two,
    }

    // ---- the processes the windows' presence files name -------------------------------------------------------

    /// A process standing for a window other than this test's own: its pid and start time are what that window's
    /// presence files and its partner's skip name. Killed and reaped when dropped.
    struct Stand(std::process::Child);

    impl Stand {
        fn start() -> Stand {
            Stand(
                std::process::Command::new("sleep")
                    .arg("600")
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .spawn()
                    .expect("sleep starts"),
            )
        }

        fn pid(&self) -> u32 {
            self.0.id()
        }

        fn start_time(&self) -> u64 {
            agent::child::start_time(self.pid()).unwrap()
        }
    }

    impl Drop for Stand {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    // ---- the editor ---------------------------------------------------------------------------------------------

    /// Window 1's nvim: the review module's answers as the module gives them, the events it queued for the next
    /// drain, and what a buffer holds when asked.
    struct FakeNvim {
        calls: RefCell<Vec<(&'static str, Vec<M>)>>,
        drawn: RefCell<BTreeSet<Vec<u8>>>,
        events: RefCell<Vec<M>>,
        buffer: RefCell<M>,
    }

    impl FakeNvim {
        fn new() -> FakeNvim {
            FakeNvim {
                calls: RefCell::default(),
                drawn: RefCell::default(),
                events: RefCell::default(),
                buffer: RefCell::new(clear_buffer()),
            }
        }

        fn active(&self) -> M {
            M::from(self.drawn.borrow().len() as u64)
        }

        fn reply(&self, code: &'static str, args: &[M]) -> M {
            let path = || match &args[0] {
                M::Binary(bytes) => bytes.clone(),
                other => panic!("a path travels as bytes: {other}"),
            };
            if code == REVIEW_LUA {
                vmap(vec![("installed", M::from(true))])
            } else if code == LIVE_SHOWN_LUA {
                // Every file asked about is shown in a window.
                let asked = args[0].as_array().map_or(0, Vec::len);
                vmap(vec![("shown", M::Array(vec![M::from(true); asked]))])
            } else if code == LIVE_SHOW_LUA {
                let hunks = args[2].as_array().map_or(0, Vec::len) as u64;
                self.drawn.borrow_mut().insert(path());
                vmap(vec![
                    ("drawn", M::from(hunks)),
                    ("shown", M::from(true)),
                    ("left", M::from(false)),
                    ("skipped", M::from(0)),
                    ("active", self.active()),
                    ("pending", M::from(0)),
                    ("live_only", M::from(true)),
                ])
            } else if code == LIVE_CLEAR_LUA {
                let cleared = self.drawn.borrow_mut().remove(&path());
                vmap(vec![
                    ("cleared", M::from(cleared)),
                    ("active", self.active()),
                    ("pending", M::from(0)),
                    ("live_only", M::from(false)),
                ])
            } else if code == SHOW_LUA {
                self.drawn.borrow_mut().insert(path());
                vmap(vec![
                    ("drawn", M::from(1)),
                    ("kept", M::from(1)),
                    ("active", self.active()),
                ])
            } else if code == OPEN_AND_SHOW_LUA {
                if args[3] != M::Nil {
                    self.drawn.borrow_mut().insert(path());
                }
                vmap(vec![
                    ("opened", M::from(true)),
                    ("drawn", M::from(1)),
                    ("kept", M::from(1)),
                    ("active", self.active()),
                ])
            } else if code == CLEAR_LUA {
                self.drawn.borrow_mut().remove(&path());
                vmap(vec![("active", self.active())])
            } else if code == TAKE_EVENTS_LUA {
                let events: Vec<M> = self.events.borrow_mut().drain(..).collect();
                vmap(vec![("events", M::Array(events)), ("active", self.active())])
            } else if code == BUFFER_STATE_LUA {
                self.buffer.borrow().clone()
            } else {
                M::Nil
            }
        }

        fn asked(&self, code: &str) -> Vec<Vec<M>> {
            self.calls
                .borrow()
                .iter()
                .filter(|(c, _)| *c == code)
                .map(|(_, args)| args.clone())
                .collect()
        }
    }

    impl EditorRpc for FakeNvim {
        fn exec_lua(&self, code: &'static str, args: Vec<M>) -> Pending {
            let reply = self.reply(code, &args);
            self.calls.borrow_mut().push((code, args));
            let (answer, pending) = Pending::pair();
            answer.send(Ok(reply));
            pending
        }

        fn target(&self) -> Option<u64> {
            Some(1)
        }
    }

    fn vmap(pairs: Vec<(&str, M)>) -> M {
        M::Map(pairs.into_iter().map(|(k, v)| (M::from(k), v)).collect())
    }

    fn clear_buffer() -> M {
        vmap(vec![("found", M::from(false)), ("modified", M::from(false))])
    }

    fn modified_buffer() -> M {
        vmap(vec![("found", M::from(true)), ("modified", M::from(true))])
    }

    /// A buffer whose region asked about holds `lines`, unsaved.
    fn unsaved_buffer(lines: &[&str]) -> M {
        vmap(vec![
            ("found", M::from(true)),
            ("modified", M::from(true)),
            ("fileformat", M::from("unix")),
            ("eol", M::from(true)),
            ("line_count", M::from(lines.len() as u64)),
            ("lines", M::Array(lines.iter().map(|l| M::from(*l)).collect())),
        ])
    }

    // ---- a window's turn review ---------------------------------------------------------------------------------

    /// A window's tab set and review flow, as the panel holds them, with what the pages were sent.
    struct Side {
        tabs: RefCell<TabSet>,
        flow: RefCell<ReviewFlow>,
        rpc: Option<Rc<dyn EditorRpc>>,
        owner: Option<Owner>,
        outs: RefCell<Vec<Out>>,
        sent: RefCell<Vec<Sent>>,
    }

    impl ReviewSide for Side {
        fn with(&self, step: ReviewStep<'_>) {
            let settled = {
                let mut tabs = self.tabs.borrow_mut();
                let mut flow = self.flow.borrow_mut();
                flow.set_editor_owner(self.owner);
                step(&mut flow, &mut tabs, self.rpc.as_deref())
            };
            self.outs.borrow_mut().extend(settled.outs);
            self.sent.borrow_mut().extend(settled.sent);
        }
    }

    struct Win {
        side: Rc<Side>,
        project: PathBuf,
        provider: Arc<RecordingProvider>,
        tab: TabId,
        session: String,
        turns: u32,
        /// Set in a window whose presence thread waits for it.
        release: Option<mpsc::Sender<()>>,
    }

    fn live_backend(project: &Path) -> (Arc<RecordingProvider>, AgentBackend) {
        let provider = Arc::new(RecordingProvider::default());
        let conversation = AgentConversation::create(
            provider.clone(),
            project,
            agent::setting_sources::ProjectTrust::Untrusted,
        )
        .unwrap();
        (
            provider,
            AgentBackend::Conversation {
                agent: AgentId::Claude,
                conversation: Box::new(conversation),
            },
        )
    }

    thread_local! {
        static MONOTONIC: Arc<AtomicU64> = Arc::new(AtomicU64::new(0));
    }

    pub(super) fn mono_now() -> u64 {
        MONOTONIC.with(|clock| eitri_core::turn_review::monotonic_ns() + clock.load(Ordering::SeqCst))
    }

    fn advance_mono() {
        MONOTONIC.with(|clock| {
            clock.fetch_add(500_000_000, Ordering::SeqCst);
        });
    }

    fn until(what: &str, mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !done() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    impl Win {
        /// A window of `project` whose presence files are under `pid`, with one tab holding `session`.
        fn new(
            project: &Path,
            review_dir: &Path,
            pid: u32,
            session: &str,
            rpc: Option<Rc<dyn EditorRpc>>,
            owner: Option<Owner>,
            parked: bool,
        ) -> Win {
            let mut tabs = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
            let options = ReviewOptions {
                excludes: Some(None),
                trim_every: Duration::MAX,
                monotonic: Some(MONOTONIC.with(|clock| {
                    let clock = clock.clone();
                    Arc::new(move || eitri_core::turn_review::monotonic_ns() + clock.load(Ordering::SeqCst))
                        as eitri_core::turn_review::MonotonicClock
                })),
                ..ReviewOptions::default()
            };
            tabs.install_turn_review(TurnReview::with_options(
                Some(review_dir.to_path_buf()),
                project,
                options,
            ))
            .unwrap();
            let release = if parked {
                let (holder, release) = PresenceHolder::start_parked(review_dir.to_path_buf(), pid);
                tabs.hold_presence(holder);
                Some(release)
            } else {
                tabs.hold_presence(PresenceHolder::start(review_dir.to_path_buf(), pid));
                let answer = tabs.presence_confirmation().expect("a presence is held");
                assert_eq!(answer.recv_timeout(Duration::from_secs(10)), Ok(Held::Yes));
                None
            };
            let tab = tabs.active();
            let (provider, backend) = live_backend(project);
            tabs.get_mut(tab).unwrap().set_backend(TabBackend::Live(backend));
            provider.open_session(session, project);
            until("the session", || {
                tabs.pump(project, true);
                tabs.get(tab).unwrap().provider_session_id().as_deref() == Some(session)
            });
            Win {
                side: Rc::new(Side {
                    tabs: RefCell::new(tabs),
                    flow: RefCell::new(ReviewFlow::new()),
                    rpc,
                    owner,
                    outs: RefCell::default(),
                    sent: RefCell::default(),
                }),
                project: project.to_path_buf(),
                provider,
                tab,
                session: session.to_owned(),
                turns: 0,
                release,
            }
        }

        /// One tick of the panel: the pump, the review flow, the prompts the presence lets go, and what the link
        /// owes the other window, handed to `review`.
        fn tick(&self, review: Option<&GroupReview>) {
            let outbound = {
                let mut tabs = self.side.tabs.borrow_mut();
                let mut flow = self.side.flow.borrow_mut();
                tabs.pump(&self.project, true);
                flow.set_editor_owner(self.side.owner);
                let now = Instant::now();
                let outs = flow.tick(&mut tabs, self.side.rpc.as_deref(), now);
                self.side.outs.borrow_mut().extend(outs);
                let sent = tabs.poll_turn_sends(now);
                self.side.sent.borrow_mut().extend(sent);
                take_pair_outbound(&mut flow, &mut tabs)
            };
            if let Some(review) = review {
                review.outbound(outbound);
            }
        }

        fn handle(&self, message: InboundMessage) {
            let outs = {
                let mut tabs = self.side.tabs.borrow_mut();
                let mut flow = self.side.flow.borrow_mut();
                flow.set_editor_owner(self.side.owner);
                flow.handle(message, &mut tabs, self.side.rpc.as_deref(), Instant::now())
            };
            self.side.outs.borrow_mut().extend(outs);
        }

        fn file(&self, rel: &str) -> PathBuf {
            self.project.join(rel)
        }

        fn records(&self) -> Vec<eitri_core::turn_review::TurnRecord> {
            self.side.tabs.borrow().turn_review().unwrap().turns(&self.session)
        }

        fn draft_reverts(&self) -> usize {
            self.side.tabs.borrow().review_draft(self.tab).unwrap().reverts().len()
        }

        /// Whether this window's presence thread holds every file of its running set now.
        fn confirmed(&self) -> bool {
            let answer = self.side.tabs.borrow().presence_confirmation().unwrap();
            answer.recv_timeout(Duration::from_secs(10)) == Ok(Held::Yes)
        }

        fn queue_turn_start(&mut self) -> String {
            self.turns += 1;
            let id = format!("{}-t{}", self.session, self.turns);
            self.provider
                .queue(AgentDomainEvent::TurnStarted { turn_id: id.clone() });
            id
        }

        fn queue_turn_end(&self) -> String {
            let id = format!("{}-t{}", self.session, self.turns);
            self.provider.queue(AgentDomainEvent::TurnCompleted {
                turn_id: id.clone(),
                outcome: TurnOutcome::Completed,
                result_text: String::new(),
                stop_reason: None,
                usage: None,
                detail: Default::default(),
            });
            id
        }

        fn base_taken(&self, id: &str) -> Option<u32> {
            self.records()
                .into_iter()
                .find(|r| r.turn_id == id && matches!(r.base, Snap::Taken { .. }))
                .map(|r| r.n)
        }

        fn end_taken(&self, id: &str) -> bool {
            self.side.tabs.borrow().running_count() == 0
                && self
                    .records()
                    .iter()
                    .any(|r| r.turn_id == id && matches!(r.end, Snap::Taken { .. }))
        }

        fn shut_down(self) {
            let tabs = std::mem::replace(
                &mut *self.side.tabs.borrow_mut(),
                TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto),
            );
            let mut tabs = tabs;
            for tab in tabs.take_all() {
                if let Some(retired) = tab.retired {
                    retired.into_backend().shutdown();
                }
            }
        }
    }

    // ---- messages -----------------------------------------------------------------------------------------------

    fn msg(v: Json) -> InboundMessage {
        parse_inbound_message(&v.to_string()).expect("the message parses")
    }

    fn revert_file(id: &str, tab: TabId, turn: u32, path: &str) -> InboundMessage {
        msg(
            json!({"type":"review_revert","request_id":id,"tab":tab.0,"turn":turn,"scope":"turn","path":path,
            "target":"file"}),
        )
    }

    fn comment(id: &str, tab: TabId, turn: u32, path: &str, text: &str) -> InboundMessage {
        msg(
            json!({"type":"review_comment_add","request_id":id,"tab":tab.0,"turn":turn,"scope":"turn","path":path,
            "from":1,"to":1,"text":text}),
        )
    }

    fn send(id: &str, tab: TabId) -> InboundMessage {
        msg(json!({"type":"review_send","request_id":id,"tab":tab.0,"confirm":null}))
    }

    fn open_in_editor(id: &str, tab: TabId, turn: u32, path: &str) -> InboundMessage {
        msg(
            json!({"type":"open_in_editor","request_id":id,"tab":tab.0,"turn":turn,"scope":"turn","path":path,
            "line":2}),
        )
    }

    fn envelopes(outs: &[Out]) -> Vec<Json> {
        outs.iter()
            .filter_map(|out| match out {
                Out::Envelope(text) | Out::Answer { payload: text, .. } | Out::Draft(_, text) => {
                    Some(serde_json::from_str(text).expect("an envelope is JSON"))
                }
                Out::Flushed(..) | Out::QueueChanged(..) | Out::EditorOpened => None,
            })
            .collect()
    }

    /// The command result for `id`: `Ok(message)` or `Err(error)`; `None` while it is not answered.
    fn result_of(outs: &[Out], id: &str) -> Option<Result<Option<String>, String>> {
        envelopes(outs)
            .into_iter()
            .find(|v| v["kind"] == "command_result" && v["requestId"] == id)
            .map(|found| {
                if found["ok"] == true {
                    Ok(found["message"].as_str().map(str::to_owned))
                } else {
                    Err(found["error"].as_str().unwrap().to_owned())
                }
            })
    }

    fn preview_of(outs: &[Out], id: &str) -> Option<Json> {
        envelopes(outs)
            .into_iter()
            .find(|v| v["kind"] == "review_send_preview" && v["requestId"] == id)
    }

    /// The editor's revert event for a one-line hunk of `rel`, for `win`'s session.
    fn revert_event(win: &Win, n: u32, rel: &str, at_line: u32, old: &str, new: &str) -> M {
        vmap(vec![
            ("kind", M::from("revert")),
            ("tab", M::from(win.tab.0)),
            ("session", M::from(win.session.as_str())),
            ("turn", M::from(n)),
            ("scope", M::from("turn")),
            ("path", M::from(win.file(rel).to_str().unwrap())),
            ("hunk_id", M::from(1)),
            ("old_start", M::from(at_line)),
            ("old_len", M::from(1)),
            ("new_start", M::from(at_line)),
            ("new_len", M::from(1)),
            ("at_line", M::from(at_line)),
            ("old_lines", M::Array(vec![M::from(old)])),
            ("new_lines", M::Array(vec![M::from(new)])),
            ("old_eols", M::Array(vec![M::from("lf")])),
            ("new_eols", M::Array(vec![M::from("lf")])),
        ])
    }

    // ---- the pair -----------------------------------------------------------------------------------------------

    type Step = Box<dyn Fn() -> bool>;

    /// Steps the windows' repeating work by hand.
    #[derive(Clone, Default)]
    struct Steps(Rc<RefCell<Vec<Step>>>);

    impl Steps {
        fn repeat(&self) -> Repeat {
            let steps = self.0.clone();
            Rc::new(move |_: Duration, step: Box<dyn Fn() -> bool>| steps.borrow_mut().push(step))
        }

        fn run(&self) {
            let steps = std::mem::take(&mut *self.0.borrow_mut());
            let kept: Vec<_> = steps.into_iter().filter(|step| step()).collect();
            self.0.borrow_mut().extend(kept);
        }
    }

    /// Two windows of one project linked over a real socketpair: window 1 is this process, with the editor; window
    /// 2's presence files are named by a process standing for it. Neither has said its presence is held yet.
    struct Pair {
        _scratch: Scratch,
        project: PathBuf,
        review_dir: PathBuf,
        steps: Steps,
        nvim: Rc<FakeNvim>,
        one: Win,
        two: Win,
        one_link: LinkDriver,
        two_link: LinkDriver,
        one_review: Rc<GroupReview>,
        two_review: Rc<GroupReview>,
        _one_editor: Rc<Window1Editor>,
        _two_editor: Rc<Window2Editor>,
        stand: Stand,
    }

    impl Pair {
        fn new(label: &str, park_two: bool) -> Pair {
            agent::state_dirs::redirect_state_to_a_test_root();
            let scratch = Scratch::new(label);
            let base = std::fs::canonicalize(&scratch.1).unwrap();
            let project = base.join("project");
            std::fs::create_dir_all(&project).unwrap();
            let review_dir = review_dir_for(Some(base.join("state").as_os_str()), None, &project).unwrap();
            for rel in ["a.txt", "b.txt"] {
                std::fs::write(project.join(rel), BASE).unwrap();
            }
            let stand = Stand::start();
            let nvim = Rc::new(FakeNvim::new());
            let one = Win::new(
                &project,
                &review_dir,
                std::process::id(),
                "s-one",
                Some(nvim.clone() as Rc<dyn EditorRpc>),
                Some(Owner::Embedded),
                false,
            );
            let two = Win::new(&project, &review_dir, stand.pid(), "s-two", None, None, park_two);

            let (a, b) = fdpass::seqpacket_pair().unwrap();
            let flag = || PresenceFlag(Arc::new(AtomicBool::new(true)));
            let (one_flag, two_flag) = (flag(), flag());
            let (one_handle, one_in) = LinkHandle::start(a, one_flag.clone()).unwrap();
            let (two_handle, two_in) = LinkHandle::start(b, two_flag.clone()).unwrap();
            let (one_link, two_link) = (LinkDriver::new(one_handle, one_in), LinkDriver::new(two_handle, two_in));
            one_link.set_presence(one_flag);
            two_link.set_presence(two_flag);
            one_link.set_partner(LinkPartner {
                pid: stand.pid(),
                start: stand.start_time(),
            });
            two_link.set_partner(LinkPartner {
                pid: std::process::id(),
                start: agent::child::start_time(std::process::id()).unwrap(),
            });
            one_link.set_linked();
            two_link.set_linked();

            let steps = Steps::default();
            let one_editor = Window1Editor::new(&project, None, steps.repeat());
            one_editor.set_rpc(nvim.clone());
            {
                let side = one.side.clone();
                one_editor.set_own_session(Rc::new(move |session: &str| {
                    side.tabs
                        .borrow()
                        .tabs()
                        .iter()
                        .any(|t| t.provider_session_id().as_deref() == Some(session))
                }));
            }
            let two_editor = Window2Editor::new(&project, None, steps.repeat());
            one_editor.linked(
                crate::linked_window::spawn::SECOND_PEER,
                eitri_core::window_link::names::WindowName::Second,
                &one_link,
            );
            two_editor.linked(&two_link);
            let one_review = GroupReview::new(
                &project,
                one.side.clone(),
                Role::One(one_editor.clone()),
                steps.repeat(),
            );
            let two_review = GroupReview::new(&project, two.side.clone(), Role::Two(two_editor.ops()), steps.repeat());
            one_review.linked(SECOND_PEER, &one_link);
            two_review.linked(HUB_PEER, &two_link);
            Pair {
                _scratch: scratch,
                project,
                review_dir,
                steps,
                nvim,
                one,
                two,
                one_link,
                two_link,
                one_review,
                two_review,
                _one_editor: one_editor,
                _two_editor: two_editor,
                stand,
            }
        }

        /// One round of both windows: the link's messages, each panel's tick, the timers.
        fn step(&self) {
            self.one_link.pump();
            self.two_link.pump();
            self.one.tick(Some(&self.one_review));
            self.two.tick(Some(&self.two_review));
            self.steps.run();
        }

        fn settle(&self, what: &str, done: impl Fn(&Pair) -> bool) {
            let deadline = Instant::now() + Duration::from_secs(20);
            loop {
                self.step();
                if done(self) {
                    return;
                }
                assert!(Instant::now() < deadline, "timed out waiting for {what}");
                std::thread::sleep(Duration::from_millis(5));
            }
        }

        /// Both windows have said their presence is held, so each skips the other's window file.
        fn one_presence(&self) {
            self.settle("both windows to confirm their presence", |p| {
                !p.one_link.presence_flag().load(Ordering::SeqCst) && !p.two_link.presence_flag().load(Ordering::SeqCst)
            });
        }

        fn win(&self, which: Window) -> &Win {
            match which {
                Window::One => &self.one,
                Window::Two => &self.two,
            }
        }

        fn win_mut(&mut self, which: Window) -> &mut Win {
            match which {
                Window::One => &mut self.one,
                Window::Two => &mut self.two,
            }
        }

        /// Starts a turn in `which` and waits for its base; returns its id and number.
        fn start_turn(&mut self, which: Window) -> (String, u32) {
            let id = self.win_mut(which).queue_turn_start();
            let mut n = None;
            let deadline = Instant::now() + Duration::from_secs(20);
            while n.is_none() {
                assert!(Instant::now() < deadline, "the base of {id}");
                self.step();
                n = self.win(which).base_taken(&id);
                std::thread::sleep(Duration::from_millis(5));
            }
            (id, n.unwrap())
        }

        fn end_turn(&mut self, which: Window) {
            let id = self.win(which).queue_turn_end();
            self.settle("the turn's end", |p| p.win(which).end_taken(&id));
        }

        /// One whole turn in `which` that writes `bytes` to `rel`; returns its number.
        fn turn(&mut self, which: Window, rel: &str, bytes: &[u8]) -> u32 {
            let (_, n) = self.start_turn(which);
            std::fs::write(self.project.join(rel), bytes).unwrap();
            self.end_turn(which);
            n
        }

        /// `message` handed to `which`'s flow, then both windows stepped until it is answered.
        fn request(&self, which: Window, id: &str, message: InboundMessage) -> Result<Option<String>, String> {
            self.win(which).handle(message);
            self.settle(id, |p| result_of(&p.win(which).side.outs.borrow(), id).is_some());
            result_of(&self.win(which).side.outs.borrow(), id).unwrap()
        }

        /// `message` handed to `which`'s flow, then both windows stepped until anything answers it.
        fn ask(&self, which: Window, id: &str, message: InboundMessage) {
            self.win(which).handle(message);
            self.settle(id, |p| {
                envelopes(&p.win(which).side.outs.borrow())
                    .iter()
                    .any(|v| v["requestId"] == id)
            });
        }

        fn read(&self, rel: &str) -> Vec<u8> {
            std::fs::read(self.project.join(rel)).unwrap()
        }

        fn shut_down(self) {
            let Pair { one, two, stand, .. } = self;
            if let Some(release) = &two.release {
                let _ = release.send(());
            }
            one.shut_down();
            two.shut_down();
            drop(stand);
        }
    }

    // ---- reverts and the presence ---------------------------------------------------------------------------------

    #[test]
    fn a_revert_in_window_2_and_one_in_window_1_both_succeed_when_both_are_idle() {
        let mut p = Pair::new("both-idle", false);
        p.one_presence();
        let n1 = p.turn(Window::One, "a.txt", CHANGED);
        let n2 = p.turn(Window::Two, "b.txt", CHANGED);

        let result = p.request(Window::Two, "r2", revert_file("r2", p.two.tab, n2, "b.txt"));
        assert_eq!(result, Ok(None), "window 2 reverts its own turn's file");
        assert_eq!(p.read("b.txt"), BASE);
        assert_eq!(
            p.nvim.asked(BUFFER_STATE_LUA).len(),
            1,
            "the editor asked about the file is window 1's nvim"
        );

        let result = p.request(Window::One, "r1", revert_file("r1", p.one.tab, n1, "a.txt"));
        assert_eq!(result, Ok(None), "and window 1 its own");
        assert_eq!(p.read("a.txt"), BASE);
        p.shut_down();
    }

    #[test]
    fn a_turn_running_in_either_window_blocks_the_others_revert() {
        let mut p = Pair::new("turn-blocks", false);
        p.one_presence();
        let n1 = p.turn(Window::One, "a.txt", CHANGED);
        let n2 = p.turn(Window::Two, "b.txt", CHANGED);

        p.start_turn(Window::Two);
        assert!(p.two.confirmed(), "window 2's turn holds its running file");
        let refused = p.request(Window::One, "r1", revert_file("r1", p.one.tab, n1, "a.txt"));
        assert!(
            refused.as_ref().is_err_and(|why| why.contains(BUSY_TURN)),
            "window 2's turn blocks window 1's revert: {refused:?}"
        );
        assert_eq!(p.read("a.txt"), CHANGED);
        p.end_turn(Window::Two);

        p.start_turn(Window::One);
        assert!(p.one.confirmed());
        let refused = p.request(Window::Two, "r2", revert_file("r2", p.two.tab, n2, "b.txt"));
        assert!(
            refused.as_ref().is_err_and(|why| why.contains(BUSY_TURN)),
            "window 1's turn blocks window 2's revert: {refused:?}"
        );
        assert_eq!(p.read("b.txt"), CHANGED);
        p.end_turn(Window::One);

        let result = p.request(Window::One, "r3", revert_file("r3", p.one.tab, n1, "a.txt"));
        assert_eq!(result, Ok(None), "with no turn running anywhere it goes through");
        p.shut_down();
    }

    #[test]
    fn a_revert_and_a_review_send_at_the_same_moment_never_both_go_ahead() {
        let mut p = Pair::new("slot", false);
        p.one_presence();
        let n1 = p.turn(Window::One, "a.txt", CHANGED);
        let n2 = p.turn(Window::Two, "b.txt", CHANGED);
        p.ask(Window::Two, "c2", comment("c2", p.two.tab, n2, "b.txt", "why?"));
        assert_eq!(
            p.two
                .side
                .tabs
                .borrow()
                .review_draft(p.two.tab)
                .unwrap()
                .comments()
                .len(),
            1
        );

        // Window 1's revert holds the pair's write slot, as its worker does from its first act until after its
        // write: window 2's send, which reads the disk, does not go ahead meanwhile.
        let writing = p
            .one
            .side
            .tabs
            .borrow()
            .presence_guard()
            .unwrap()
            .hold_slot_for_write()
            .expect("the slot is free");
        let refused = p.request(Window::Two, "s1", send("s1", p.two.tab));
        assert_eq!(refused, Err(SlotBusy::Writing.to_string()));
        drop(writing);
        p.two.handle(send("s2", p.two.tab));
        p.settle("window 2's preview", |p| {
            preview_of(&p.two.side.outs.borrow(), "s2").is_some()
        });

        // Window 2's send holds it shared while it reads the disk: window 1's revert does not go ahead meanwhile.
        let reading = WriteSlot::open(&p.review_dir)
            .unwrap()
            .try_shared()
            .expect("no write is out");
        let refused = p.request(Window::One, "r1", revert_file("r1", p.one.tab, n1, "a.txt"));
        assert!(refused.is_err(), "{refused:?}");
        assert_eq!(p.read("a.txt"), CHANGED, "nothing was written");
        drop(reading);
        assert_eq!(
            p.request(Window::One, "r2", revert_file("r2", p.one.tab, n1, "a.txt")),
            Ok(None)
        );
        p.shut_down();
    }

    #[test]
    fn overlapping_turns_across_the_windows_each_carry_the_overlap_note() {
        let mut p = Pair::new("overlap", false);
        p.one_presence();
        let (one_turn, _) = p.start_turn(Window::One);
        let (two_turn, _) = p.start_turn(Window::Two);
        p.end_turn(Window::Two);
        p.end_turn(Window::One);
        let overlapped = |win: &Win, id: &str| win.records().iter().any(|r| r.turn_id == id && r.overlapped_tab);
        p.settle("both records to carry the note", |p| {
            overlapped(&p.one, &one_turn) && overlapped(&p.two, &two_turn)
        });

        // A turn that ran alone carries none: it starts once window 2 has heard window 1's turn end, and later
        // than the start's scan looks back.
        for _ in 0..20 {
            p.one_link.read_through_for_test(&p.two_link);
            p.two_link.read_through_for_test(&p.one_link);
            p.step();
        }
        advance_mono();
        let (alone, _) = p.start_turn(Window::Two);
        p.end_turn(Window::Two);
        for _ in 0..20 {
            p.step();
        }
        assert!(!overlapped(&p.two, &alone), "{:?}", p.two.records());
        p.shut_down();
    }

    #[test]
    fn a_third_unpaired_window_blocks_both_and_is_blocked() {
        let mut p = Pair::new("third", false);
        p.one_presence();
        let n1 = p.turn(Window::One, "a.txt", CHANGED);
        let n2 = p.turn(Window::Two, "b.txt", CHANGED);
        let third_stand = Stand::start();
        let third_nvim: Rc<dyn EditorRpc> = Rc::new(FakeNvim::new());
        let mut third = Win::new(
            &p.project,
            &p.review_dir,
            third_stand.pid(),
            "s-three",
            Some(third_nvim),
            Some(Owner::Embedded),
            false,
        );
        std::fs::write(p.project.join("c.txt"), BASE).unwrap();
        let id = third.queue_turn_start();
        until("the third window's base", || {
            third.tick(None);
            third.base_taken(&id).is_some()
        });
        let n3 = third.base_taken(&id).unwrap();
        std::fs::write(p.project.join("c.txt"), CHANGED).unwrap();
        let end = third.queue_turn_end();
        until("the third window's end", || {
            third.tick(None);
            third.end_taken(&end)
        });

        for (which, id, tab, n, rel) in [
            (Window::One, "r1", p.one.tab, n1, "a.txt"),
            (Window::Two, "r2", p.two.tab, n2, "b.txt"),
        ] {
            let refused = p.request(which, id, revert_file(id, tab, n, rel));
            assert!(
                refused.as_ref().is_err_and(|why| why.contains(OTHER_WINDOW)),
                "{which:?}: {refused:?}"
            );
            assert_eq!(p.read(rel), CHANGED);
        }
        third.handle(revert_file("r3", third.tab, n3, "c.txt"));
        until("the third window's answer", || {
            third.tick(None);
            result_of(&third.side.outs.borrow(), "r3").is_some()
        });
        let refused = result_of(&third.side.outs.borrow(), "r3").unwrap();
        assert!(
            refused.as_ref().is_err_and(|why| why.contains(OTHER_WINDOW)),
            "the pair is two windows to it: {refused:?}"
        );
        assert_eq!(p.read("c.txt"), CHANGED);
        third.shut_down();
        drop(third_stand);
        p.shut_down();
    }

    #[test]
    fn a_parked_presence_thread_in_window_2_sends_after_window_1_acknowledges() {
        let p = Pair::new("parked", true);
        // As if window 2 had confirmed its presence before its thread stalled.
        p.one_link.presence_flag().store(false, Ordering::SeqCst);
        let held = p.two.side.tabs.borrow_mut().send_prompt(
            p.two.tab,
            "p1",
            ViewId::BASE,
            FirstTurn {
                typed_by_person: true,
                wire: "hello".into(),
                typed: "hello".into(),
                shell_line: false,
                attachments: Vec::new(),
            },
        );
        assert!(held.is_none(), "held for the presence");
        // Window 2's panel ticks on, its link's messages not yet read: past the confirmation's wait it tells
        // window 1, whose link thread counts it as another window and acknowledges.
        let deadline = Instant::now() + Duration::from_secs(20);
        while !p.one_link.presence_flag().load(Ordering::SeqCst) {
            assert!(
                Instant::now() < deadline,
                "window 1 never heard presence{{held: false}}"
            );
            p.two.tick(Some(&p.two_review));
            p.steps.run();
            std::thread::sleep(Duration::from_millis(5));
        }
        for _ in 0..20 {
            p.two.tick(Some(&p.two_review));
        }
        assert!(
            p.two.provider.turns().is_empty(),
            "nothing goes out before the acknowledgement is read"
        );
        assert!(p.two.side.tabs.borrow().send_is_held(p.two.tab));
        p.settle("the prompt", |p| !p.two.provider.turns().is_empty());
        assert_eq!(p.two.provider.turns(), vec!["hello".to_string()]);
        assert!(
            p.one_link.presence_flag().load(Ordering::SeqCst),
            "window 1 counts window 2 as another window until it says its presence is held"
        );
        p.shut_down();
    }

    #[test]
    fn a_prompt_waiting_for_its_presence_when_window_2_closes_its_link_is_not_sent() {
        let p = Pair::new("closing", true);
        // As if window 2 had confirmed its presence before its thread stalled: window 1 skips its file.
        p.one_link.presence_flag().store(false, Ordering::SeqCst);
        let held = p.two.side.tabs.borrow_mut().send_prompt(
            p.two.tab,
            "p1",
            ViewId::BASE,
            FirstTurn {
                typed_by_person: true,
                wire: "hello".into(),
                typed: "hello".into(),
                shell_line: false,
                attachments: Vec::new(),
            },
        );
        assert!(held.is_none(), "held for the presence");

        p.two_link.close("window 2 closed");
        assert!(p.two.provider.turns().is_empty(), "nothing went to the agent");
        assert!(
            p.two.side.sent.borrow().iter().any(
                |sent| matches!(sent, Sent::NotSent { note, .. } if note == eitri_core::turn_review::CLOSING_NOTE)
            ),
            "the prompt waits in its queue with the reason"
        );
        // Window 1's link thread counts window 2 as another window as soon as it reads the close, before window
        // 1's own side has heard of it.
        until("window 1's link thread to read the close", || {
            p.one_link.presence_flag().load(Ordering::SeqCst)
        });
        for _ in 0..20 {
            p.two.tick(None);
        }
        assert!(p.two.provider.turns().is_empty(), "and it is not sent later either");
        p.shut_down();
    }

    #[test]
    fn a_partner_turn_whose_start_and_end_arrive_together_marks_only_the_turns_it_overlapped() {
        let mut p = Pair::new("edges-together", false);
        p.one_presence();
        // Window 2's turn ran and ended before window 1's turn began; both its edges reach window 1 only later,
        // together.
        let partner_end = mono_now();
        advance_mono();
        let (later, _) = p.start_turn(Window::One);
        for (edge, at_ns) in [
            (EdgeKind::Started, partner_end - 1_000_000),
            (EdgeKind::Ended, partner_end),
        ] {
            p.two_link
                .send(
                    Msg::TurnEdge {
                        session: "s-two".into(),
                        edge,
                        at_ns,
                    },
                    Vec::new(),
                )
                .unwrap();
        }
        p.one_link.read_through_for_test(&p.two_link);
        p.end_turn(Window::One);
        for _ in 0..20 {
            p.step();
        }
        assert!(
            p.one.records().iter().any(|r| r.turn_id == later && !r.overlapped_tab),
            "{:?}",
            p.one.records()
        );
        p.shut_down();
    }

    #[test]
    fn a_window_with_turn_review_off_still_counts_in_the_others_overlap_note() {
        let mut p = Pair::new("review-off", false);
        p.one_presence();
        p.two.side.tabs.borrow_mut().turn_review_off();
        let (one_turn, _) = p.start_turn(Window::One);
        let tab = p.two.tab;
        p.two.queue_turn_start();
        p.settle("window 2's turn", |p| {
            p.two.side.tabs.borrow().get(tab).unwrap().turn_running()
        });
        p.two.queue_turn_end();
        p.settle("its end", |p| {
            !p.two.side.tabs.borrow().get(tab).unwrap().turn_running()
        });
        p.end_turn(Window::One);
        p.settle("window 1's record to carry the note", |p| {
            p.one
                .records()
                .iter()
                .any(|r| r.turn_id == one_turn && r.overlapped_tab)
        });
        p.shut_down();
    }

    #[test]
    fn a_turn_between_two_ticks_of_a_window_with_turn_review_off_still_counts_in_the_others_overlap_note() {
        let mut p = Pair::new("review-off-brief", false);
        p.one_presence();
        p.two.side.tabs.borrow_mut().turn_review_off();
        let (one_turn, _) = p.start_turn(Window::One);
        let tab = p.two.tab;
        // Window 2's ingestion thread folds the whole turn while its tick does not run: no tick sees it running.
        p.two.queue_turn_start();
        until("window 2's turn", || {
            p.two.side.tabs.borrow().get(tab).unwrap().turn_running()
        });
        p.two.queue_turn_end();
        until("its end", || !p.two.side.tabs.borrow().get(tab).unwrap().turn_running());
        p.step();
        p.end_turn(Window::One);
        p.settle("window 1's record to carry the note", |p| {
            p.one
                .records()
                .iter()
                .any(|r| r.turn_id == one_turn && r.overlapped_tab)
        });
        p.shut_down();
    }

    // ---- the editor across the pair ----------------------------------------------------------------------------------

    #[test]
    fn a_revert_in_window_2_asks_window_1s_nvim_and_is_refused_on_a_modified_buffer() {
        let mut p = Pair::new("modified", false);
        p.one_presence();
        let n2 = p.turn(Window::Two, "b.txt", CHANGED);
        *p.nvim.buffer.borrow_mut() = modified_buffer();
        let refused = p.request(Window::Two, "r2", revert_file("r2", p.two.tab, n2, "b.txt"));
        assert!(
            refused.as_ref().is_err_and(|why| why.contains("unsaved")),
            "window 1's nvim holds unsaved changes of the file: {refused:?}"
        );
        assert_eq!(p.read("b.txt"), CHANGED);
        let asked = p.nvim.asked(BUFFER_STATE_LUA);
        assert_eq!(asked.len(), 1);
        assert_eq!(
            asked[0][0],
            M::from(p.project.join("b.txt").to_str().unwrap()),
            "about that very file, by window 1's own path"
        );
        p.shut_down();
    }

    #[test]
    fn a_review_send_in_window_2_names_an_unsaved_editor_revert_as_window_1_would() {
        let mut p = Pair::new("send-unsaved", false);
        p.one_presence();
        let n1 = p.turn(Window::One, "a.txt", CHANGED);
        let n2 = p.turn(Window::Two, "b.txt", CHANGED);
        // Each window opens its file in the editor, and the hunk is reverted there and left unsaved.
        assert!(p
            .request(Window::One, "o1", open_in_editor("o1", p.one.tab, n1, "a.txt"))
            .is_ok());
        assert!(p
            .request(Window::Two, "o2", open_in_editor("o2", p.two.tab, n2, "b.txt"))
            .is_ok());
        p.nvim.events.borrow_mut().extend([
            revert_event(&p.one, n1, "a.txt", 2, "two", "TWO"),
            revert_event(&p.two, n2, "b.txt", 2, "two", "TWO"),
        ]);
        p.settle("both editor reverts", |p| {
            p.one.draft_reverts() == 1 && p.two.draft_reverts() == 1
        });
        // The reverted line, as the region asked about holds it, unsaved.
        *p.nvim.buffer.borrow_mut() = unsaved_buffer(&["two"]);

        p.one.handle(send("s1", p.one.tab));
        p.two.handle(send("s2", p.two.tab));
        p.settle("both previews", |p| {
            preview_of(&p.one.side.outs.borrow(), "s1").is_some()
                && preview_of(&p.two.side.outs.borrow(), "s2").is_some()
        });
        let one = preview_of(&p.one.side.outs.borrow(), "s1").unwrap();
        let two = preview_of(&p.two.side.outs.borrow(), "s2").unwrap();
        assert_eq!(
            two["notOnDisk"][0]["why"], one["notOnDisk"][0]["why"],
            "window 2's message says what window 1's says: {one} / {two}"
        );
        assert_eq!(two["notOnDisk"][0]["why"], "only in the editor, not saved");
        assert_eq!(two["notOnDisk"][0]["lines"], one["notOnDisk"][0]["lines"]);
        p.shut_down();
    }

    #[test]
    fn window_2s_hunk_reverted_in_nvim_reaches_window_2s_review_only() {
        let mut p = Pair::new("events", false);
        p.one_presence();
        let n1 = p.turn(Window::One, "a.txt", CHANGED);
        let n2 = p.turn(Window::Two, "b.txt", CHANGED);
        assert!(p
            .request(Window::One, "o1", open_in_editor("o1", p.one.tab, n1, "a.txt"))
            .is_ok());
        let opened = p.request(Window::Two, "o2", open_in_editor("o2", p.two.tab, n2, "b.txt"));
        assert_eq!(opened, Ok(Some("opened".to_string())), "answered by window 1's driver");
        assert!(
            p.two
                .side
                .outs
                .borrow()
                .iter()
                .any(|out| matches!(out, Out::EditorOpened)),
            "window 2 brings the editor forward"
        );
        assert_eq!(
            p.nvim.asked(REVIEW_LUA).len(),
            1,
            "one module, installed once, by window 1"
        );

        p.nvim
            .events
            .borrow_mut()
            .push(revert_event(&p.two, n2, "b.txt", 2, "two", "TWO"));
        p.settle("window 2's draft", |p| p.two.draft_reverts() == 1);
        for _ in 0..40 {
            p.step();
        }
        assert_eq!(p.one.draft_reverts(), 0, "window 1's review never sees window 2's hunk");
        assert_eq!(p.two.draft_reverts(), 1, "and window 2 records it once");
        p.shut_down();
    }

    #[test]
    fn window_2s_panel_moving_to_another_turn_of_a_drawn_file_redraws_nvim() {
        let mut p = Pair::new("redraw", false);
        p.one_presence();
        let first = p.turn(Window::Two, "b.txt", CHANGED);
        let second = p.turn(Window::Two, "b.txt", AGAIN);
        assert!(p
            .request(Window::Two, "o2", open_in_editor("o2", p.two.tab, first, "b.txt"))
            .is_ok());
        let drawn = p.project.join("b.txt");
        p.settle("window 2 to hear the file is drawn", |p| {
            p.two
                .side
                .flow
                .borrow_mut()
                .remote_overlay()
                .is_some_and(|overlay| overlay.active_paths().contains(&drawn))
        });
        let shows = p.nvim.asked(SHOW_LUA).len();
        {
            let tabs = p.two.side.tabs.borrow();
            p.two
                .side
                .flow
                .borrow_mut()
                .panel_shows(&tabs, p.two.tab, second, Scope::Turn, "b.txt");
        }
        p.settle("the redraw in nvim", |p| p.nvim.asked(SHOW_LUA).len() > shows);
        let show = p.nvim.asked(SHOW_LUA).pop().unwrap();
        assert_eq!(show[0], M::Binary(drawn.into_os_string().into_encoded_bytes()));
        let turn = match &show[1] {
            M::Map(fields) => fields
                .iter()
                .find(|(key, _)| key.as_str() == Some("turn"))
                .and_then(|(_, value)| value.as_u64()),
            _ => None,
        };
        assert_eq!(
            turn,
            Some(u64::from(second)),
            "the turn the panel moved to: {:?}",
            show[1]
        );
        p.shut_down();
    }

    #[test]
    fn a_hunk_reverted_in_nvim_during_the_freeze_reaches_the_destinations_draft() {
        let mut p = Pair::new("held", false);
        p.one_presence();
        let n2 = p.turn(Window::Two, "b.txt", CHANGED);
        let opened = p.request(Window::Two, "o2", open_in_editor("o2", p.two.tab, n2, "b.txt"));
        assert_eq!(opened, Ok(Some("opened".to_string())));
        // The session's chat is moving: the window that runs nvim holds its review events.
        p.one_review
            .hold_session(&p.two.session, true, true)
            .expect("window 1 holds them");
        p.nvim
            .events
            .borrow_mut()
            .push(revert_event(&p.two, n2, "b.txt", 2, "two", "TWO"));
        for _ in 0..40 {
            p.step();
        }
        assert_eq!(p.two.draft_reverts(), 0, "held while the move is under way");
        assert_eq!(p.one.draft_reverts(), 0, "and recorded by no window meanwhile");
        // The move ends with the other window holding the session: what waited goes there, once.
        p.one_review
            .hold_session(&p.two.session, false, false)
            .expect("window 1 releases them");
        p.settle("the held revert", |p| p.two.draft_reverts() == 1);
        for _ in 0..40 {
            p.step();
        }
        assert_eq!(p.two.draft_reverts(), 1, "recorded once, by the session's holder");
        assert_eq!(p.one.draft_reverts(), 0);
        assert!(
            p.two_review.hold_session(&p.two.session, true, true).is_err(),
            "a window without the editor holds nothing"
        );
        p.shut_down();
    }

    // ---- a group of windows -------------------------------------------------------------------------------------

    const C1: PeerId = PeerId(16);
    const C2: PeerId = PeerId(17);

    /// Both ends of a link between the windows of the processes `a` and `b`, each end counting the other as another
    /// window until it says its presence is held.
    fn ends(a: LinkPartner, b: LinkPartner) -> (LinkDriver, LinkDriver) {
        let (x, y) = fdpass::seqpacket_pair().unwrap();
        let flag = || PresenceFlag(Arc::new(AtomicBool::new(true)));
        let (a_flag, b_flag) = (flag(), flag());
        let (a_handle, a_in) = LinkHandle::start(x, a_flag.clone()).unwrap();
        let (b_handle, b_in) = LinkHandle::start(y, b_flag.clone()).unwrap();
        let (a_link, b_link) = (LinkDriver::new(a_handle, a_in), LinkDriver::new(b_handle, b_in));
        a_link.set_presence(a_flag);
        b_link.set_presence(b_flag);
        a_link.set_partner(b);
        b_link.set_partner(a);
        a_link.set_linked();
        b_link.set_linked();
        (a_link, b_link)
    }

    /// A module window of a [`Group`]: its presence files are named by a process standing for it.
    struct Member {
        win: Win,
        /// Window 1's end of its link.
        hub_link: LinkDriver,
        link: LinkDriver,
        review: Rc<GroupReview>,
        _editor: Rc<Window2Editor>,
        stand: Stand,
    }

    /// Which window of a [`Group`] a step of a test acts on.
    #[derive(Debug, Clone, Copy)]
    enum Seat {
        One,
        First,
        Second,
    }

    /// Window 1, with the editor, and two module windows, linked to it and to nothing else, over real socketpairs.
    /// Nobody has said its presence is held yet.
    struct Group {
        _scratch: Scratch,
        project: PathBuf,
        review_dir: PathBuf,
        steps: Steps,
        nvim: Rc<FakeNvim>,
        one: Win,
        review: Rc<GroupReview>,
        _editor: Rc<Window1Editor>,
        c1: Member,
        c2: Member,
    }

    impl Group {
        /// With `park_one`, window 1's presence thread waits for its release, and with `park_c2`, the second module
        /// window's.
        fn new(label: &str, park_one: bool, park_c2: bool) -> Group {
            agent::state_dirs::redirect_state_to_a_test_root();
            let scratch = Scratch::new(label);
            let base = std::fs::canonicalize(&scratch.1).unwrap();
            let project = base.join("project");
            std::fs::create_dir_all(&project).unwrap();
            let review_dir = review_dir_for(Some(base.join("state").as_os_str()), None, &project).unwrap();
            for rel in ["a.txt", "b.txt", "c.txt"] {
                std::fs::write(project.join(rel), BASE).unwrap();
            }
            let steps = Steps::default();
            let nvim = Rc::new(FakeNvim::new());
            let one = Win::new(
                &project,
                &review_dir,
                std::process::id(),
                "s-one",
                Some(nvim.clone() as Rc<dyn EditorRpc>),
                Some(Owner::Embedded),
                park_one,
            );
            let editor = Window1Editor::new(&project, None, steps.repeat());
            editor.set_rpc(nvim.clone());
            {
                let side = one.side.clone();
                editor.set_own_session(Rc::new(move |session: &str| {
                    side.tabs
                        .borrow()
                        .tabs()
                        .iter()
                        .any(|t| t.provider_session_id().as_deref() == Some(session))
                }));
            }
            let review = GroupReview::new(&project, one.side.clone(), Role::One(editor.clone()), steps.repeat());
            let this_process = LinkPartner {
                pid: std::process::id(),
                start: agent::child::start_time(std::process::id()).unwrap(),
            };
            let member = |peer: PeerId, name: WindowName, session: &str, parked: bool| {
                let stand = Stand::start();
                let win = Win::new(&project, &review_dir, stand.pid(), session, None, None, parked);
                let (hub_link, link) = ends(
                    this_process,
                    LinkPartner {
                        pid: stand.pid(),
                        start: stand.start_time(),
                    },
                );
                let own_editor = Window2Editor::new(&project, None, steps.repeat());
                editor.linked(peer, name, &hub_link);
                own_editor.linked(&link);
                let own = GroupReview::new(
                    &project,
                    win.side.clone(),
                    Role::Module(own_editor.ops()),
                    steps.repeat(),
                );
                review.linked(peer, &hub_link);
                own.linked(HUB_PEER, &link);
                let ended = Rc::downgrade(&own);
                link.on_end(move |_, _| {
                    if let Some(review) = ended.upgrade() {
                        review.unlinked(HUB_PEER);
                    }
                });
                Member {
                    win,
                    hub_link,
                    link,
                    review: own,
                    _editor: own_editor,
                    stand,
                }
            };
            let c1 = member(C1, WindowName::Chat(1), "s-c1", false);
            let c2 = member(C2, WindowName::Chat(2), "s-c2", park_c2);
            Group {
                _scratch: scratch,
                project,
                review_dir,
                steps,
                nvim,
                one,
                review,
                _editor: editor,
                c1,
                c2,
            }
        }

        fn members(&self) -> [&Member; 2] {
            [&self.c1, &self.c2]
        }

        /// One round of every window: the links' messages, each panel's tick, the timers.
        fn step(&self) {
            for member in self.members() {
                member.hub_link.pump();
                member.link.pump();
            }
            self.one.tick(Some(&self.review));
            for member in self.members() {
                member.win.tick(Some(&member.review));
            }
            self.steps.run();
        }

        fn settle(&self, what: &str, done: impl Fn(&Group) -> bool) {
            let deadline = Instant::now() + Duration::from_secs(20);
            loop {
                self.step();
                if done(self) {
                    return;
                }
                assert!(Instant::now() < deadline, "timed out waiting for {what}");
                std::thread::sleep(Duration::from_millis(5));
            }
        }

        /// Window 1 has heard every module window say its presence is held, and each of them window 1's.
        fn all_present(&self) {
            self.settle("every window to confirm its presence", |g| {
                g.members().iter().all(|member| {
                    !member.hub_link.presence_flag().load(Ordering::SeqCst)
                        && !member.link.presence_flag().load(Ordering::SeqCst)
                })
            });
        }

        fn win(&self, seat: Seat) -> &Win {
            match seat {
                Seat::One => &self.one,
                Seat::First => &self.c1.win,
                Seat::Second => &self.c2.win,
            }
        }

        fn win_mut(&mut self, seat: Seat) -> &mut Win {
            match seat {
                Seat::One => &mut self.one,
                Seat::First => &mut self.c1.win,
                Seat::Second => &mut self.c2.win,
            }
        }

        fn start_turn(&mut self, seat: Seat) -> (String, u32) {
            let id = self.win_mut(seat).queue_turn_start();
            let mut n = None;
            let deadline = Instant::now() + Duration::from_secs(20);
            while n.is_none() {
                assert!(Instant::now() < deadline, "the base of {id}");
                self.step();
                n = self.win(seat).base_taken(&id);
                std::thread::sleep(Duration::from_millis(5));
            }
            (id, n.unwrap())
        }

        fn end_turn(&mut self, seat: Seat) {
            let id = self.win(seat).queue_turn_end();
            self.settle("the turn's end", |g| g.win(seat).end_taken(&id));
        }

        /// One whole turn in `seat` that writes `bytes` to `rel`; returns its number. Its `running/` file is gone by
        /// the time this returns.
        fn turn(&mut self, seat: Seat, rel: &str, bytes: &[u8]) -> u32 {
            let (_, n) = self.start_turn(seat);
            std::fs::write(self.project.join(rel), bytes).unwrap();
            self.end_turn(seat);
            assert!(self.win(seat).confirmed());
            n
        }

        fn request(&self, seat: Seat, id: &str, message: InboundMessage) -> Result<Option<String>, String> {
            self.win(seat).handle(message);
            self.settle(id, |g| result_of(&g.win(seat).side.outs.borrow(), id).is_some());
            result_of(&self.win(seat).side.outs.borrow(), id).unwrap()
        }

        fn read(&self, rel: &str) -> Vec<u8> {
            std::fs::read(self.project.join(rel)).unwrap()
        }

        fn drawn(&self, rel: &str) -> bool {
            let path = self.project.join(rel).into_os_string().into_encoded_bytes();
            self.nvim.drawn.borrow().contains(&path)
        }

        fn shut_down(self) {
            let Group { one, c1, c2, .. } = self;
            let (first, second) = (c1.stand, c2.stand);
            for win in [one, c1.win, c2.win] {
                if let Some(release) = &win.release {
                    let _ = release.send(());
                }
                win.shut_down();
            }
            drop((first, second));
        }
    }

    fn overlapped(win: &Win, id: &str) -> bool {
        win.records().iter().any(|r| r.turn_id == id && r.overlapped_tab)
    }

    #[test]
    fn overlapping_turns_in_two_module_windows_each_carry_the_overlap_note() {
        let mut g = Group::new("group-overlap", false, false);
        g.all_present();
        let (first, _) = g.start_turn(Seat::First);
        let (second, _) = g.start_turn(Seat::Second);
        g.end_turn(Seat::Second);
        g.end_turn(Seat::First);
        g.settle("both records to carry the note", |g| {
            overlapped(&g.c1.win, &first) && overlapped(&g.c2.win, &second)
        });
        g.shut_down();
    }

    /// Lets every edge on its way arrive, and takes a turn that starts afterwards past what its start's scan looks at.
    fn let_edges_settle(g: &Group) {
        for _ in 0..20 {
            for member in g.members() {
                member.hub_link.read_through_for_test(&member.link);
                member.link.read_through_for_test(&member.hub_link);
            }
            g.step();
        }
        advance_mono();
    }

    #[test]
    fn a_turn_passed_on_ends_when_its_window_goes() {
        let mut g = Group::new("group-overlap-gone", false, false);
        g.all_present();
        let (running, _) = g.start_turn(Seat::Second);
        let_edges_settle(&g);
        g.c2.hub_link.close("the window went away");
        let_edges_settle(&g);

        let (alone, _) = g.start_turn(Seat::First);
        g.end_turn(Seat::First);
        for _ in 0..20 {
            g.step();
        }
        assert!(!overlapped(&g.c1.win, &alone), "{:?}", g.c1.win.records());
        assert!(!running.is_empty());
        g.shut_down();
    }

    #[test]
    fn a_window_that_links_later_hears_the_turns_already_running() {
        let mut g = Group::new("group-overlap-late", false, false);
        g.all_present();
        let (_, n) = g.start_turn(Seat::First);
        assert!(n > 0);
        let_edges_settle(&g);

        let stand = Stand::start();
        let this_process = LinkPartner {
            pid: std::process::id(),
            start: agent::child::start_time(std::process::id()).unwrap(),
        };
        let (hub_link, far) = ends(
            this_process,
            LinkPartner {
                pid: stand.pid(),
                start: stand.start_time(),
            },
        );
        let heard: Rc<RefCell<Vec<Msg>>> = Rc::default();
        {
            let heard = heard.clone();
            far.on("turn_edge", move |_, msg, _| heard.borrow_mut().push(msg));
        }
        g.review.linked(PeerId(18), &hub_link);
        let deadline = Instant::now() + Duration::from_secs(5);
        while heard.borrow().is_empty() {
            assert!(Instant::now() < deadline, "the running turn was not told");
            hub_link.pump();
            far.pump();
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            matches!(
                &heard.borrow()[0],
                Msg::TurnEdge {
                    session,
                    edge: EdgeKind::Started,
                    ..
                } if session == "s-c1"
            ),
            "{:?}",
            heard.borrow()
        );
        g.review.unlinked(PeerId(18));
        g.end_turn(Seat::First);
        g.shut_down();
    }

    /// One finished turn in each window of the group, each writing its own file; what reverting each one takes.
    fn one_turn_each(g: &mut Group) -> [(Seat, &'static str, &'static str, TabId, u32); 3] {
        let n1 = g.turn(Seat::One, "a.txt", CHANGED);
        let n2 = g.turn(Seat::First, "b.txt", CHANGED);
        let n3 = g.turn(Seat::Second, "c.txt", CHANGED);
        [
            (Seat::One, "r1", "a.txt", g.one.tab, n1),
            (Seat::First, "r2", "b.txt", g.c1.win.tab, n2),
            (Seat::Second, "r3", "c.txt", g.c2.win.tab, n3),
        ]
    }

    #[test]
    fn a_turn_running_in_c2_blocks_a_revert_in_c1() {
        let mut g = Group::new("c2-turn", false, false);
        g.all_present();
        let n = g.turn(Seat::First, "a.txt", CHANGED);

        g.start_turn(Seat::Second);
        assert!(g.c2.win.confirmed(), "c2's turn holds its running file");
        let refused = g.request(Seat::First, "r1", revert_file("r1", g.c1.win.tab, n, "a.txt"));
        assert!(
            refused.as_ref().is_err_and(|why| why.contains(BUSY_TURN)),
            "window 1 names the turn it sees: {refused:?}"
        );
        assert_eq!(g.read("a.txt"), CHANGED);

        g.end_turn(Seat::Second);
        assert!(g.c2.win.confirmed());
        let result = g.request(Seat::First, "r2", revert_file("r2", g.c1.win.tab, n, "a.txt"));
        assert_eq!(result, Ok(None), "with no turn running anywhere it goes through");
        assert_eq!(g.read("a.txt"), BASE);
        g.shut_down();
    }

    #[test]
    fn an_unlinked_third_window_blocks_a_revert_anywhere_in_the_group() {
        let mut g = Group::new("third", false, false);
        g.all_present();
        let reverts = one_turn_each(&mut g);
        let third = Stand::start();
        let holder = PresenceHolder::start(g.review_dir.clone(), third.pid());
        assert_eq!(
            holder
                .sync_confirmed(&BTreeSet::new())
                .recv_timeout(Duration::from_secs(10)),
            Ok(Held::Yes)
        );

        for (seat, id, rel, tab, n) in reverts {
            let refused = g.request(seat, id, revert_file(id, tab, n, rel));
            assert!(
                refused.as_ref().is_err_and(|why| why.contains(OTHER_WINDOW)),
                "{seat:?}: {refused:?}"
            );
            assert_eq!(g.read(rel), CHANGED);
        }
        drop(holder);
        drop(third);
        g.shut_down();
    }

    #[test]
    fn c2_counting_as_other_blocks_until_its_presence_held_arrives() {
        let mut g = Group::new("c2-counted", false, true);
        g.settle("c1 to confirm its presence", |g| {
            !g.c1.hub_link.presence_flag().load(Ordering::SeqCst)
        });
        let n = g.turn(Seat::First, "a.txt", CHANGED);

        let refused = g.request(Seat::First, "r1", revert_file("r1", g.c1.win.tab, n, "a.txt"));
        assert!(
            refused.as_ref().is_err_and(|why| why.contains(OTHER_WINDOW)),
            "c2 has not said its presence is held: {refused:?}"
        );
        assert_eq!(g.read("a.txt"), CHANGED);

        g.c2.win.release.as_ref().unwrap().send(()).unwrap();
        g.settle("c2 to confirm its presence", |g| {
            !g.c2.hub_link.presence_flag().load(Ordering::SeqCst)
        });
        let result = g.request(Seat::First, "r2", revert_file("r2", g.c1.win.tab, n, "a.txt"));
        assert_eq!(result, Ok(None));
        assert_eq!(g.read("a.txt"), BASE);
        g.shut_down();
    }

    /// c1's revert of a turn of its own is on its worker, waiting for a window 1 that takes `revert_guard` and never
    /// answers; returns the request's id.
    fn waiting_for_the_guard(g: &mut Group) -> (&'static str, Instant) {
        g.all_present();
        let n = g.turn(Seat::First, "a.txt", CHANGED);
        g.c1.hub_link.on("revert_guard", |_, _, _| {});
        let asked = Instant::now();
        g.c1.win.handle(revert_file("r1", g.c1.win.tab, n, "a.txt"));
        g.settle("the revert to ask window 1", |g| g.c1.review.guards_waiting() == 1);
        ("r1", asked)
    }

    #[test]
    fn the_link_dropping_while_c1_waits_for_the_guard_refuses_the_revert() {
        let mut g = Group::new("link-drops", false, false);
        let (id, _) = waiting_for_the_guard(&mut g);
        // Every window goes on being stepped while the worker waits: nothing on this thread waits with it.
        for _ in 0..10 {
            g.step();
        }
        assert_eq!(result_of(&g.c1.win.side.outs.borrow(), id), None, "still waiting");

        g.c1.hub_link.close("window 1 went away");
        g.settle("the revert's answer", |g| {
            result_of(&g.c1.win.side.outs.borrow(), id).is_some()
        });
        let refused = result_of(&g.c1.win.side.outs.borrow(), id).unwrap();
        assert!(
            refused
                .as_ref()
                .is_err_and(|why| why.contains(eitri_core::turn_review::NOT_LINKED)),
            "{refused:?}"
        );
        assert_eq!(g.read("a.txt"), CHANGED);
        assert_eq!(g.c1.review.guards_waiting(), 0);
        g.shut_down();
    }

    #[test]
    fn a_window_1_that_does_not_answer_refuses_the_revert_at_the_end_of_the_wait() {
        let mut g = Group::new("no-answer", false, false);
        let (release, held) = mpsc::channel();
        let held = Mutex::new(held);
        let (observed, wait_started) = mpsc::channel();
        *g.c1.review.hub.as_ref().unwrap().wait.lock().unwrap() = Some(Arc::new(move |_, within| {
            observed.send(within).unwrap();
            held.lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(60))
                .expect("release the simulated deadline");
            Err(mpsc::RecvTimeoutError::Timeout)
        }));
        let (id, _) = waiting_for_the_guard(&mut g);
        assert_eq!(
            wait_started.recv_timeout(Duration::from_secs(60)).unwrap(),
            eitri_core::turn_review::GUARD_WAIT
        );
        assert!(
            result_of(&g.c1.win.side.outs.borrow(), id).is_none(),
            "still inside the guard wait"
        );
        release.send(()).unwrap();
        g.settle("the revert's answer", |g| {
            result_of(&g.c1.win.side.outs.borrow(), id).is_some()
        });
        let refused = result_of(&g.c1.win.side.outs.borrow(), id).unwrap();
        assert!(
            refused
                .as_ref()
                .is_err_and(|why| why.contains(eitri_core::turn_review::NOT_LINKED)),
            "{refused:?}"
        );
        assert_eq!(g.read("a.txt"), CHANGED);
        assert_eq!(
            g.c1.review.guards_waiting(),
            0,
            "a worker that gave up leaves nothing behind"
        );
        g.shut_down();
    }

    #[test]
    fn an_approval_read_after_the_link_ended_lets_no_write_through() {
        let mut g = Group::new("late-approval", false, false);
        let (id, _) = waiting_for_the_guard(&mut g);
        // A write asks twice; the first approval takes the job on to its last look before it writes.
        let first = g.c1.review.guard_job().unwrap();
        let approval = |job| Msg::Guard {
            job,
            ok: true,
            why: None,
        };
        g.c1.hub_link.send(approval(first), Vec::new()).unwrap();
        g.settle("the last look to ask window 1", |g| {
            g.c1.review.guard_job().is_some_and(|job| job != first)
        });
        let last = g.c1.review.guard_job().unwrap();

        // The approval and the end both reach c1's link thread before its GTK side reads either.
        let link = g.c1.link.sender().unwrap();
        g.c1.hub_link.send(approval(last), Vec::new()).unwrap();
        g.c1.hub_link.close("window 1 went away");
        let deadline = Instant::now() + Duration::from_secs(20);
        while !link.is_closed() {
            assert!(Instant::now() < deadline, "timed out waiting for the link thread");
            std::thread::sleep(Duration::from_millis(1));
        }

        g.settle("the revert's answer", |g| {
            result_of(&g.c1.win.side.outs.borrow(), id).is_some()
        });
        let refused = result_of(&g.c1.win.side.outs.borrow(), id).unwrap();
        assert!(
            refused
                .as_ref()
                .is_err_and(|why| why.contains(eitri_core::turn_review::NOT_LINKED)),
            "{refused:?}"
        );
        assert_eq!(g.read("a.txt"), CHANGED);
        g.shut_down();
    }

    #[test]
    fn window_1_reverts_with_c1_and_c2_linked_and_idle() {
        let mut g = Group::new("idle", false, false);
        g.all_present();
        for (seat, id, rel, tab, n) in one_turn_each(&mut g) {
            assert_eq!(g.request(seat, id, revert_file(id, tab, n, rel)), Ok(None), "{seat:?}");
            assert_eq!(g.read(rel), BASE);
        }
        g.shut_down();
    }

    #[test]
    fn review_calls_from_two_module_windows_route_back_to_their_own_link() {
        let mut g = Group::new("routing", false, false);
        g.all_present();
        let n1 = g.turn(Seat::First, "a.txt", CHANGED);
        let n2 = g.turn(Seat::Second, "b.txt", CHANGED);
        for (seat, id, tab, n, rel) in [
            (Seat::First, "o1", g.c1.win.tab, n1, "a.txt"),
            (Seat::Second, "o2", g.c2.win.tab, n2, "b.txt"),
        ] {
            assert_eq!(
                g.request(seat, id, open_in_editor(id, tab, n, rel)),
                Ok(Some("opened".to_string())),
                "answered on its own link"
            );
        }
        let shows = |win: &Win, rel: &str| {
            let path = win.file(rel);
            win.side
                .flow
                .borrow_mut()
                .remote_overlay()
                .is_some_and(|overlay| overlay.active_paths().contains(&path))
        };
        g.settle("each window to hear its file is drawn", |g| {
            shows(&g.c1.win, "a.txt") && shows(&g.c2.win, "b.txt")
        });
        assert!(!shows(&g.c1.win, "b.txt") && !shows(&g.c2.win, "a.txt"));

        g.nvim
            .events
            .borrow_mut()
            .push(revert_event(&g.c2.win, n2, "b.txt", 2, "two", "TWO"));
        g.settle("c2's draft", |g| g.c2.win.draft_reverts() == 1);
        for _ in 0..40 {
            g.step();
        }
        assert_eq!(
            g.c1.win.draft_reverts(),
            0,
            "the event goes to the window holding its session"
        );
        assert_eq!(g.one.draft_reverts(), 0);

        g.review.unlinked(C1);
        g.settle("c1's file to be cleared", |g| !g.drawn("a.txt"));
        for _ in 0..20 {
            g.step();
        }
        assert!(g.drawn("b.txt"), "c2 keeps what it drew");
        g.shut_down();
    }

    #[test]
    fn a_turn_in_a_module_window_marks_window_1s_overlapping_turn() {
        let mut g = Group::new("overlap", false, false);
        g.all_present();
        let (one_turn, _) = g.start_turn(Seat::One);
        g.start_turn(Seat::First);
        g.end_turn(Seat::One);
        g.settle("window 1's record to carry the note", |g| {
            g.one
                .records()
                .iter()
                .any(|r| r.turn_id == one_turn && r.overlapped_tab)
        });
        g.shut_down();
    }

    /// A prompt of window 1's, whose presence thread stalled, past its wait: both links are told, and window 1 has
    /// read neither acknowledgement yet.
    fn prompt_waiting_for_acks() -> Group {
        let g = Group::new("acks", true, false);
        let held = g.one.side.tabs.borrow_mut().send_prompt(
            g.one.tab,
            "p1",
            ViewId::BASE,
            FirstTurn {
                typed_by_person: true,
                wire: "hello".into(),
                typed: "hello".into(),
                shell_line: false,
                attachments: Vec::new(),
            },
        );
        assert!(held.is_none(), "held for the presence");
        until("both links to be told", || {
            g.one.tick(Some(&g.review));
            g.steps.run();
            g.review.acks_owed.borrow().len() == 2
        });
        g
    }

    #[test]
    fn a_prompt_waiting_for_acknowledgements_waits_for_every_link() {
        let g = prompt_waiting_for_acks();
        until("c1's acknowledgement", || {
            g.c1.hub_link.pump();
            g.review.acks_owed.borrow().len() == 1
        });
        for _ in 0..5 {
            g.one.tick(Some(&g.review));
        }
        assert!(g.one.provider.turns().is_empty(), "c2 may still skip window 1's file");
        until("c2's acknowledgement", || {
            g.c2.hub_link.pump();
            g.one.tick(Some(&g.review));
            !g.one.provider.turns().is_empty()
        });
        assert_eq!(g.one.provider.turns(), vec!["hello".to_string()]);
        g.shut_down();
    }

    #[test]
    fn a_link_ending_stands_for_its_acknowledgement() {
        let g = prompt_waiting_for_acks();
        until("c1's acknowledgement", || {
            g.c1.hub_link.pump();
            g.review.acks_owed.borrow().len() == 1
        });
        g.review.unlinked(C2);
        until("the prompt", || {
            g.one.tick(Some(&g.review));
            !g.one.provider.turns().is_empty()
        });
        g.shut_down();
    }

    // ---- live drawings ------------------------------------------------------------------------------------------

    /// Both windows' live rounds (as the panel's tick runs them before its own), then a step of the pair, until `done`.
    fn live_settle(p: &Pair, what: &str, done: impl Fn(&Pair) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            for win in [&p.one, &p.two] {
                let mut tabs = win.side.tabs.borrow_mut();
                let mut flow = win.side.flow.borrow_mut();
                tabs.pump(&win.project, true);
                flow.set_editor_owner(win.side.owner);
                flow.live_tick(&mut tabs, win.side.rpc.as_deref(), Instant::now());
            }
            p.step();
            if done(p) {
                return;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn two_live_drawings(p: &Pair) -> BTreeMap<PathBuf, (String, u32)> {
        let mut flow = p.two.side.flow.borrow_mut();
        flow.remote_overlay()
            .map(|overlay| overlay.live_drawings().clone())
            .unwrap_or_default()
    }

    #[test]
    fn a_live_edit_in_window_2_is_drawn_in_window_1s_nvim_and_its_turn_ends_with_the_link() {
        let mut p = Pair::new("live-linked", false);
        p.two.side.tabs.borrow_mut().set_live_edits(true);
        let (turn_id, n) = p.start_turn(Window::Two);
        let abs = p.project.join("a.txt");
        std::fs::write(&abs, CHANGED).unwrap();
        p.two.provider.queue_all(vec![
            AgentDomainEvent::ToolCallStarted {
                turn_id: turn_id.clone(),
                tool_use_id: "edit-1".into(),
                name: "Edit".into(),
                input: json!({ "file_path": abs.to_str().unwrap() }),
            },
            AgentDomainEvent::ToolCallCompleted {
                turn_id: turn_id.clone(),
                tool_use_id: "edit-1".into(),
                content: json!("ok"),
                is_error: false,
            },
        ]);
        live_settle(&p, "window 2's live drawing, known in both windows", |p| {
            two_live_drawings(p).contains_key(&abs)
        });
        let bytes = M::Binary(abs.as_os_str().as_encoded_bytes().to_vec());
        assert!(
            p.nvim
                .asked(LIVE_SHOWN_LUA)
                .iter()
                .all(|args| args[0] == M::Array(vec![bytes.clone()])),
            "window 1's nvim is asked by window 1's own path"
        );
        let shows = p.nvim.asked(LIVE_SHOW_LUA);
        assert_eq!(shows.len(), 1);
        assert_eq!(shows[0][0], bytes);
        assert_eq!(
            two_live_drawings(&p),
            BTreeMap::from([(abs.clone(), (p.two.session.clone(), n))]),
            "window 2 knows its drawing is live, for its turn"
        );

        // The link ends while window 2's turn still runs: its turn ends in window 1's table then, so a turn of
        // window 1's that starts later and edits the file does not contest it.
        p.one_review.unlinked(SECOND_PEER);
        let later = TurnKey {
            session: "s-one".into(),
            turn_id: "s-one-later".into(),
        };
        let mut flow = p.one.side.flow.borrow_mut();
        let driver = flow.local_overlay().expect("window 1 has a driver");
        driver.note_live(
            eitri_core::review_editor::Window::One,
            &later,
            eitri_core::turn_review::monotonic_ns() + 1,
            None,
            std::slice::from_ref(&abs),
        );
        assert!(!driver.is_contested(&abs), "window 2's turn ended with its link");
        drop(flow);
        p.shut_down();
    }
}
