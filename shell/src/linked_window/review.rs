//! Turn review across a linked pair: the two windows are one presence, and window 2 reviews in window 1's editor.
//!
//! **One presence.** Once linked, each window names the other as its partner (pid and start time) with the flag
//! the link thread keeps for it ([`LinkDriver::presence_flag`]): the partner's `windows/` file is skipped by this
//! window's revert checks only while that flag is clear, and the link thread sets it from the handshake (neither
//! window has confirmed its presence when it says hello or welcome) and from every `presence{held}` before it
//! acknowledges, whatever the GTK side is doing. Each window says `presence{held: true}` once its presence thread
//! has confirmed its files, and `presence{held: false}` when a prompt's turn could not be confirmed, then
//! `true` again once it is. The partner is cleared when the link ends and before this window closes it, so a
//! partner it cannot see counts as another window again. Prompts held for the presence then go out when the link
//! ended, but stay unsent with a note when this window closes it: the partner skips this window's file until it
//! reads the close. The link thread counts the partner as another window from its `bye` or the link's end on.
//!
//! **Overlap notes.** Every turn start and end of this window goes to the partner as `turn_edge` with its
//! monotonic time (a window with turn review off still says when its turns run), and the partner's edges (with
//! those its `hello` or `welcome` named) go into this window's turn review as a partner's turns, the edges of one
//! drain of the link together.
//!
//! **One editor.** Window 1's review driver is the only one that talks to nvim's review module. Window 2's overlay
//! calls arrive as editor operations and are queued on that driver as window 2's; what the driver finds for window
//! 2 (an open's answer, what became of a file it drew, the reverts made in nvim for a session it holds, nothing
//! drawn any more) goes back as `editor_reply`, `review_drawn`, `review_events` and `review_idle`. Window 2's
//! flow drives an overlay proxy that keeps what is drawn from those, and asks window 1's nvim about a buffer
//! through the same editor operations.
//!
//! **Moved chats.** What a job finishes here for a session whose chat moved to the other window goes there as
//! `review_result`, and one that arrives is recorded by the tab that holds the session here.
//!
//! Nothing here waits: answers are polled by a repeating step the window drives (a GLib timer; a test steps it
//! by hand), and every message is queued on the link.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use eitri_core::nvim_rpc::Pending;
use eitri_core::review_editor::{OverlayCall, RemoteOverlay, Window, Window2Event};
use eitri_core::turn_review::{Held, Partner, RemoteAsk};
use eitri_core::window_link::{EditorOp, Msg, ReplyResult};
use eitri_panel::agent_panel::{AgentPanelHandle, PairOutbound, ReviewSettled, ReviewStep};
use serde_json::Value;

use super::driver::LinkDriver;
use super::editor::{EditorOps, Repeat, Window1Editor};

/// How often the answers this window waits for are looked at: an open's from window 1, its own presence
/// thread's confirmation.
const POLL_EVERY: Duration = Duration::from_millis(20);

/// How long after its presence thread said it could not hold a file the window asks it again.
const ASK_AGAIN_AFTER: Duration = Duration::from_secs(1);

/// What a window 1 with no review driver to queue window 2's calls on answers them.
pub const NO_DRIVER: &str = "the editor window has no review overlay to draw in";

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
    /// Window 1: the editor is here; window 2's overlay calls are queued on this window's driver.
    One(Rc<Window1Editor>),
    /// Window 2: its review reaches window 1's editor through these operations.
    Two(Rc<EditorOps>),
}

/// The presence thread's confirmation, asked for so the partner can be told this window holds its presence.
struct Confirming {
    answer: Option<mpsc::Receiver<Held>>,
    /// When to ask again after the thread could not hold the files.
    again_at: Option<Instant>,
}

/// One window's turn review on the link.
pub struct PairReview {
    me: Weak<PairReview>,
    root: PathBuf,
    side: Rc<dyn ReviewSide>,
    role: Role,
    repeat: Repeat,
    link: RefCell<Option<LinkDriver>>,
    /// Window 2's opens sent to window 1, each answered by the driver there once it has the call's outcome.
    opens: Rc<RefCell<Vec<(u64, Pending)>>>,
    /// The confirmation this window waits for before it says `presence{held: true}`; `None` once said.
    confirming: RefCell<Option<Confirming>>,
    polling: Cell<bool>,
    /// The `review_result`s sent, which number them.
    results_sent: Cell<u64>,
    /// The partner's turn edges taken from the link in one drain, applied together once it is drained: a start
    /// and its end that arrived together are one interval, not a turn still running.
    edges_in: RefCell<Vec<eitri_core::window_link::TurnEdge>>,
}

impl PairReview {
    /// Window 1's (`Role::One`): window 2's overlay calls go to this window's driver from now on. Window 2's
    /// (`Role::Two`): its review flow reaches window 1's editor from now on, and while unlinked has none.
    pub fn new(root: &Path, side: Rc<dyn ReviewSide>, role: Role, repeat: Repeat) -> Rc<PairReview> {
        let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
        let review = Rc::new_cyclic(|me| PairReview {
            me: me.clone(),
            root,
            side,
            role,
            repeat,
            link: RefCell::default(),
            opens: Rc::default(),
            confirming: RefCell::default(),
            polling: Cell::new(false),
            results_sent: Cell::new(0),
            edges_in: RefCell::default(),
        });
        match &review.role {
            Role::One(editor) => review.serve_window_2(editor),
            Role::Two(ops) => review.reach_window_1(ops),
        }
        review
    }

    /// Window 1: window 2's overlay calls are queued on this window's driver, tagged as window 2's, behind every
    /// call of its own. An open is answered by the driver once it has the outcome; the others at once.
    fn serve_window_2(&self, editor: &Window1Editor) {
        let side = Rc::downgrade(&self.side);
        editor.set_overlay(Rc::new(move |_id: u64, calls: Vec<OverlayCall>| {
            let side = side.upgrade()?;
            let opens = calls.iter().any(|call| matches!(call, OverlayCall::OpenAndShow { .. }));
            let mut calls = Some(calls);
            let mut queued = false;
            side.with(&mut |flow, _, rpc| {
                if let (Some(overlay), Some(rpc), Some(calls)) = (flow.local_overlay(), rpc, calls.take()) {
                    let now = Instant::now();
                    for call in calls {
                        overlay.call_for(rpc, call, Window::Two, now);
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
        editor.set_drawn_for_window2(Rc::new(move || {
            let mut drawn = Vec::new();
            if let Some(side) = side.upgrade() {
                side.with(&mut |flow, _, _| {
                    if let Some(overlay) = flow.local_overlay() {
                        drawn = overlay.window2_paths().iter().cloned().collect();
                    }
                    ReviewSettled::default()
                });
            }
            drawn
        }));
    }

    /// Window 2: the flow's overlay calls and buffer questions go to window 1 as editor operations.
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
        let mut parts = Some((overlay, ask));
        self.side.with(&mut |flow, _, _| {
            if let Some((overlay, ask)) = parts.take() {
                flow.link_to_window_1(overlay, ask);
            }
            ReviewSettled::default()
        });
    }

    fn send(&self, msg: Msg) {
        let link = self.link.borrow().clone();
        if let Some(link) = link.filter(LinkDriver::linked) {
            let kind = msg.kind();
            if let Err(err) = link.send(msg, Vec::new()) {
                eprintln!("[review] {kind} could not be sent to the other window: {err}");
            }
        }
    }

    /// The turns running here now, as started edges: what this window's `welcome` carries.
    pub fn running_turns(&self) -> Vec<eitri_core::window_link::TurnEdge> {
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

    /// The link is up: the partner is named to this window's presence, its turns are taken into the overlap scan,
    /// this window's turn edges are kept for it, and the link's review messages are taken from here on.
    pub fn linked(&self, link: &LinkDriver) {
        *self.link.borrow_mut() = Some(link.clone());
        self.take_messages(link);
        let me = self.me.clone();
        link.on_closing(move |_| {
            if let Some(me) = me.upgrade() {
                me.end(true);
            }
        });
        if let Some(partner) = link.partner() {
            let mut partner = Some(Partner {
                pid: partner.pid,
                start: partner.start,
                counts_as_other: link.presence_flag(),
            });
            let mut running = Some(link.take_partner_running());
            self.side.with(&mut |_, tabs, _| {
                let sent = tabs.set_partner(partner.take());
                if let Some(review) = tabs.turn_review_mut() {
                    review.announce_turn_edges(true);
                    review.partner_turns(running.take().unwrap_or_default());
                }
                ReviewSettled::sent(sent)
            });
        } else {
            eprintln!("[review] the link names no partner; the other window counts as another window");
        }
        self.confirm_presence();
    }

    fn take_messages(&self, link: &LinkDriver) {
        let me = self.me.clone();
        link.on("turn_edge", move |_, msg, _| {
            let (Some(me), Msg::TurnEdge { session, edge, at_ns }) = (me.upgrade(), msg) else {
                return;
            };
            me.edges_in
                .borrow_mut()
                .push(eitri_core::window_link::TurnEdge { session, edge, at_ns });
        });
        let me = self.me.clone();
        link.on_drained(move |_| {
            if let Some(me) = me.upgrade() {
                me.apply_partner_edges();
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
            me.side
                .with(&mut |_, tabs, _| ReviewSettled::sent(tabs.on_presence_ack(Instant::now())));
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
        if let Role::Two(_) = self.role {
            self.take_window_1_reports(link);
        }
    }

    /// Window 2: what window 1's driver reports about this window's overlays.
    fn take_window_1_reports(&self, link: &LinkDriver) {
        let me = self.me.clone();
        link.on("review_drawn", move |_, msg, _| {
            if let (Some(me), Msg::ReviewDrawn { path, state, .. }) = (me.upgrade(), msg) {
                me.with_remote(|overlay| overlay.review_drawn(&path, state));
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

    /// The partner's turn edges taken since the last drain, into the overlap scan in one go.
    fn apply_partner_edges(&self) {
        let edges = std::mem::take(&mut *self.edges_in.borrow_mut());
        if edges.is_empty() {
            return;
        }
        let mut edges = Some(edges);
        self.side.with(&mut |_, tabs, _| {
            if let (Some(review), Some(edges)) = (tabs.turn_review_mut(), edges.take()) {
                review.partner_turns(edges);
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

    /// The link ended: the partner counts as another window again (the prompts held for the presence go out), its
    /// running turns end in the overlap scan, and nothing more is kept for it. Window 1 clears every file drawn for
    /// window 2; window 2's overlays are known drawn no more, and what waited on window 1's editor is told it will
    /// not be answered.
    pub fn unlinked(&self) {
        self.end(false);
    }

    /// [`unlinked`](Self::unlinked), or, with `closing`, this window is about to close the link itself: then the
    /// prompts held for the presence are not sent but wait in their queues with a note, since the partner skips
    /// this window's file until it reads the close, and that file was not confirmed to mark their turns.
    fn end(&self, closing: bool) {
        if self.link.borrow_mut().take().is_none() {
            return;
        }
        // Edges already read are still the partner's turns: they go into the scan before it is told the partner
        // is gone.
        self.apply_partner_edges();
        self.confirming.borrow_mut().take();
        let window_1 = matches!(self.role, Role::One(_));
        self.side.with(&mut |flow, tabs, _| {
            let sent = if closing {
                tabs.close_link()
            } else {
                tabs.set_partner(None)
            };
            if let Some(review) = tabs.turn_review_mut() {
                review.announce_turn_edges(false);
                review.partner_gone();
            }
            let outs = if window_1 {
                if let Some(overlay) = flow.local_overlay() {
                    overlay.window2_gone();
                }
                Vec::new()
            } else {
                flow.editor_lost()
            };
            ReviewSettled { outs, sent }
        });
        self.opens.borrow_mut().clear();
    }

    /// What the window's panel left for the other window after a tick.
    pub fn outbound(&self, out: PairOutbound) {
        let PairOutbound {
            window2,
            edges,
            presence_false,
            results,
        } = out;
        if self.link.borrow().as_ref().is_none_or(|link| !link.linked()) {
            return;
        }
        for event in window2 {
            if let Some(msg) = self.window_2_msg(event) {
                self.send(msg);
            }
        }
        for edge in edges {
            self.send(Msg::TurnEdge {
                session: edge.session,
                edge: edge.edge,
                at_ns: edge.at_ns,
            });
        }
        for (session, result) in results {
            let job = self.results_sent.get() + 1;
            self.results_sent.set(job);
            self.send(Msg::ReviewResult { session, job, result });
        }
        if presence_false {
            self.send(Msg::Presence { held: false });
            // Asked afresh: an answer already on its way was for a set before this turn's.
            self.confirming.borrow_mut().take();
            self.confirm_presence();
        }
    }

    /// One of window 1's driver's reports for window 2, as its message; paths go relative to the project root.
    fn window_2_msg(&self, event: Window2Event) -> Option<Msg> {
        Some(match event {
            Window2Event::Answered { op_id, result } => Msg::EditorReply {
                id: op_id,
                result: match result {
                    Ok(message) => ReplyResult::Ok {
                        value: Value::String(message),
                    },
                    Err(why) => ReplyResult::Err { why },
                },
            },
            Window2Event::Drawn { op_id, path, state } => {
                let Some(rel) = path.strip_prefix(&self.root).ok().and_then(Path::to_str) else {
                    eprintln!(
                        "[review] window 2 drew {}, outside the project; not reported",
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
            Window2Event::Events { session, events } => Msg::ReviewEvents { session, events },
            Window2Event::Idle => Msg::ReviewIdle,
        })
    }

    /// Asks the presence thread to confirm this window's files, and says `presence{held: true}` once it has. A
    /// window that holds no presence never says it: the partner goes on counting it as another window.
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
        !self.opens.borrow().is_empty() || self.confirming.borrow().is_some()
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
                self.send(Msg::Presence { held: true });
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
    use crate::linked_window::driver::Partner as LinkPartner;
    use crate::linked_window::editor::Window2Editor;
    use crate::linked_window::tests::Scratch;
    use agent::{AgentConversation, AgentDomainEvent, TurnOutcome};
    use eitri_core::agent_backend::{AgentBackend, AgentId, BackendKind};
    use eitri_core::agent_bridge::{parse_inbound_message, InboundMessage, SessionModeChoice};
    use eitri_core::editor_rpc::EditorRpc;
    use eitri_core::review_editor::{Owner, CLEAR_LUA, OPEN_AND_SHOW_LUA, REVIEW_LUA, SHOW_LUA, TAKE_EVENTS_LUA};
    use eitri_core::tab_set::{FirstTurn, Sent, TabBackend, TabSet, ViewId};
    use eitri_core::tabs::TabId;
    use eitri_core::test_providers::RecordingProvider;
    use eitri_core::turn_review::revert::BUFFER_STATE_LUA;
    use eitri_core::turn_review::{
        review_dir_for, Out, PresenceHolder, ReviewFlow, ReviewOptions, Scope, SlotBusy, Snap, TurnReview, WriteSlot,
    };
    use eitri_core::window_link::fdpass;
    use eitri_core::window_link::link::{LinkHandle, PresenceFlag};
    use eitri_core::window_link::EdgeKind;
    use eitri_panel::agent_panel::take_pair_outbound;
    use rmpv::Value as M;
    use serde_json::{json, Value as Json};
    use std::collections::BTreeSet;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    const BASE: &[u8] = b"one\ntwo\nthree\n";
    const CHANGED: &[u8] = b"one\nTWO\nthree\n";
    const AGAIN: &[u8] = b"one\nTWO\nTHREE\n";
    const BUSY_TURN: &str = "an agent turn is running";
    const OTHER_WINDOW: &str = "another Eitri window has this project open";

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
        fn tick(&self, review: Option<&PairReview>) {
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
        one_review: Rc<PairReview>,
        two_review: Rc<PairReview>,
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
            one_editor.linked(&one_link);
            two_editor.linked(&two_link);
            let one_review = PairReview::new(
                &project,
                one.side.clone(),
                Role::One(one_editor.clone()),
                steps.repeat(),
            );
            let two_review = PairReview::new(&project, two.side.clone(), Role::Two(two_editor.ops()), steps.repeat());
            one_review.linked(&one_link);
            two_review.linked(&two_link);
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
        let settled_at = Instant::now();
        while settled_at.elapsed() < Duration::from_millis(300) {
            p.step();
            std::thread::sleep(Duration::from_millis(5));
        }
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
                wire: "hello".into(),
                typed: "hello".into(),
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
            std::thread::sleep(Duration::from_millis(5));
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
                wire: "hello".into(),
                typed: "hello".into(),
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
            std::thread::sleep(Duration::from_millis(5));
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
        let partner_end = eitri_core::turn_review::monotonic_ns();
        std::thread::sleep(Duration::from_millis(20));
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
        // Long enough for both to be read off the socket before window 1 next drains it.
        std::thread::sleep(Duration::from_millis(300));
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
}
