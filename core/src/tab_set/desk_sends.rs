//! The desk's turns and interrupts in a remote tab. The provider's call crosses the network there and may take a
//! round trip or, when the link stalls, until its timeout: it runs on a thread of its own, and the tick reads what
//! became of it. A send's outcome comes as the [`Sent`] a send held for the presence comes as
//! ([`TabSet::poll_turn_sends`]); an interrupt's comes from [`TabSet::poll_interrupts`]. A local tab sends and
//! interrupts on the caller's thread, as it always did.

use super::*;
use crate::agent_backend::{SpawnedTurn, SubmittedPrompt, TurnRequest};

/// Why a remote tab's composer takes no images, and why a message with images is not sent there.
pub(super) const REMOTE_TAB_TAKES_NO_IMAGES: &str =
    "this tab cannot take images: its agent runs on a server, and messages go there as text only";

/// What a desk send carries until its provider call answers.
pub(super) enum DeskTurn {
    /// The tab's queue as one turn: its items are out while the call is out. On error they return to their
    /// original positions among surviving items in the same session, or to the head after a session change.
    Queue {
        items: Vec<Queued>,
        indices: Vec<usize>,
        kept: Vec<Arc<()>>,
        wire: String,
        typed: String,
        origin: PromptOrigin,
        after_turn: bool,
        trace: Option<crate::turn_trace::TurnTrace>,
    },
    /// A new session's first message, answered to the command `request_id`.
    First { request_id: String, turn: FirstTurn },
    /// A composer's message to an idle session, answered to the command `request_id` in `page`.
    Prompt {
        request_id: String,
        page: ViewId,
        turn: FirstTurn,
    },
}

/// A desk send whose provider call is out: at most one per tab.
pub(super) struct DeskSendWait {
    gate: SendGate,
    turn: DeskTurn,
    /// The tab's backend generation when the call was made. An old session's failed queue send returns
    /// its messages to the queue, but never changes the replacement backend's prompt or turn state.
    generation: u64,
    /// The last folded turn ending when the call went out; a busy reply waits only for a turn still open.
    ended_turn: Option<u64>,
    /// A flush was deferred while this send was out; its reply may release that flush.
    flush_owed: bool,
    /// The panel has already restored the text and withdrawn this send's row.
    restored_on_stop: bool,
    /// The person pressed Stop while the call was out, whether its interrupt went at once or waits for this send's
    /// turn to start.
    stopped_while_out: bool,
    spawned: SpawnedTurn,
    answer: mpsc::Receiver<Result<String, BackendError>>,
}

pub(super) struct DeskStart {
    tab: TabId,
    generation: u64,
    spawned: SpawnedTurn,
    turn_id: Option<String>,
    // A turn can finish before the provider reply identifies it.
    ended: BTreeSet<String>,
    lost_ending: bool,
    interrupts: Vec<Box<dyn FnOnce(Result<(), BackendError>) + Send>>,
}

impl DeskStart {
    pub(super) fn observe(&mut self, tab: TabId, generation: u64, delivery: &RevisedDelivery, backend: &AgentBackend) {
        if self.tab != tab || self.generation != generation {
            return;
        }
        match delivery {
            RevisedDelivery::Events(events) => {
                for (revision, event) in events {
                    if let AgentDomainEvent::TurnCompleted { turn_id, .. } = event {
                        if *revision > self.spawned.after_revision {
                            self.ended.insert(turn_id.clone());
                        }
                    }
                }
            }
            RevisedDelivery::Resync { .. } => {
                // A dropped ending cannot identify its turn. Only a matching active turn is safe to interrupt.
                self.lost_ending |= backend
                    .projection()
                    .last_turn_end_seq
                    .is_some_and(|s| s > self.spawned.after_revision);
            }
            RevisedDelivery::Nothing => {}
        }
    }
}

impl DeskTurn {
    fn has_prompt_row(&self) -> bool {
        !matches!(
            self,
            DeskTurn::Queue {
                origin: PromptOrigin::ShellLine,
                ..
            }
        )
    }

    fn request(&self) -> Option<(&str, Option<ViewId>)> {
        match self {
            DeskTurn::Queue { .. } => None,
            DeskTurn::First { request_id, .. } => Some((request_id, None)),
            DeskTurn::Prompt { request_id, page, .. } => Some((request_id, Some(*page))),
        }
    }
}

/// An interrupt whose provider call is out, by the number [`TabSet::interrupt_spawned`] gave it.
pub(super) struct InterruptWait {
    number: u64,
    tab: TabId,
    generation: u64,
    answer: mpsc::Receiver<Result<(), BackendError>>,
}

/// What became of an interrupt [`TabSet::interrupt_spawned`] made.
#[derive(Debug)]
pub struct InterruptDone {
    pub number: u64,
    pub tab: TabId,
    pub generation: u64,
    pub outcome: Result<(), BackendError>,
}

impl TabSet {
    /// Whether `id`'s turns and interrupts leave this thread: its agent runs on a server (its origin says so, or
    /// its provider reports a link). Another agent's tab never does; it cannot run in a remote project.
    pub fn sends_off_thread(&self, id: TabId) -> bool {
        self.get(id).is_some_and(|tab| {
            !tab.agent.is_external() && (tab.is_remote() || tab.live().is_some_and(|b| b.link_state().is_some()))
        })
    }

    /// Gives waiting messages back to the tab before a steer drop or a turn refusal can requeue them.
    pub fn cancel_sync_send(&mut self, id: TabId) -> Option<Vec<String>> {
        let tab = self.get(id)?;
        let generation = tab.backend_generation;
        let stopped = tab.send_sync.cancel_waits();
        if stopped.is_empty() {
            return None;
        }
        let withdraw = self.stop_desk_send(id, &stopped) && self.desk_send_generation(id) == Some(generation);
        let tab = self.get_mut(id)?;
        if withdraw {
            if let Some(backend) = tab.live_mut() {
                backend.withdraw_prompt();
            }
        }
        tab.session
            .queue
            .retain(|item| !stopped.iter().any(|(id, _)| item.carries(id)));
        let texts: Vec<_> = stopped.into_iter().map(|(_, prompt)| prompt).collect();
        put_before_draft(tab, &texts);
        Some(texts)
    }

    /// Marks the desk send of `id` among `stopped` as answered by its Stop, so whatever its call later answers
    /// never returns its text a second time. Whether that send folded a row of its own.
    pub(super) fn stop_desk_send(&mut self, id: TabId, stopped: &[(String, String)]) -> bool {
        self.desk_sends.get_mut(&id.0).is_some_and(|wait| {
            let ours = stopped.iter().any(|(id, _)| id == &wait.spawned.client_message_id);
            wait.restored_on_stop |= ours;
            ours && wait.turn.has_prompt_row()
        })
    }

    fn desk_send_generation(&self, id: TabId) -> Option<u64> {
        self.desk_sends.get(&id.0).map(|wait| wait.generation)
    }

    /// The texts of desk sends put back in their tabs' drafts since the last look (stopped after their call went out,
    /// or refused because their session ended during the wait): the page puts them in its box as it does a Stop's.
    pub fn take_stopped_sends(&mut self) -> Vec<(TabId, Vec<String>)> {
        std::mem::take(&mut self.desk_stopped)
    }

    /// Whether a desk send of `id` waits for its provider call.
    pub(super) fn desk_send_out(&self, id: TabId) -> bool {
        self.desk_sends.contains_key(&id.0)
    }

    /// Holds a flush until the outstanding send answers. A busy refusal still waits for the open turn.
    pub(super) fn defer_desk_flush(&mut self, id: TabId) -> bool {
        if let Some(wait) = self.desk_sends.get_mut(&id.0) {
            wait.flush_owed = true;
            true
        } else {
            false
        }
    }

    /// The command a desk send of `id` whose call is out owes its answer to, as
    /// [`TabSet::held_send_request`] names it.
    pub(super) fn desk_send_request(&self, id: TabId) -> Option<(&str, Option<ViewId>)> {
        self.desk_sends.get(&id.0)?.turn.request()
    }

    /// Starts the provider's call for a desk send of `gate`'s tab on a thread of its own. `None` when it went out:
    /// its outcome comes from [`TabSet::poll_turn_sends`]. A refusal known before the call (no session, a turn
    /// already folded as running) is told at once.
    pub(super) fn spawn_desk_send(
        &mut self,
        gate: SendGate,
        wire: String,
        typed: String,
        origin: PromptOrigin,
        turn: DeskTurn,
    ) -> Option<Sent> {
        let id = gate.tab();
        // The call carries text alone: a message with images is refused whole, never sent without them. Its
        // composer takes none (`TabSet::image_input`), so only a message that came by another way can carry any.
        let carries_images = match &turn {
            DeskTurn::Queue { items, .. } => items.iter().any(|item| !item.attachments.is_empty()),
            DeskTurn::First { turn, .. } | DeskTurn::Prompt { turn, .. } => !turn.attachments.is_empty(),
        };
        if carries_images {
            let refused = BackendError {
                message: REMOTE_TAB_TAKES_NO_IMAGES.to_string(),
                benign: true,
            };
            return Some(self.desk_send_done(gate, turn, Err(refused)));
        }
        let generation = self.get(id).map_or(0, Tab::backend_generation);
        let ended_turn = self
            .get(id)
            .and_then(Tab::live)
            .and_then(|backend| backend.projection().last_turn_end_seq);
        let wake = self.ui_wake.clone();
        let (told, answer) = mpsc::channel();
        let answered = move |outcome: Result<String, BackendError>| {
            let _ = told.send(outcome);
            if let Some(wake) = wake {
                wake();
            }
        };
        let request = TurnRequest {
            sync: self.message_sync(id),
            wire,
            typed,
            origin,
        };
        let synced = request.sync.is_some();
        let started = match self.get_mut(id) {
            Some(tab) => match tab.live_mut() {
                Some(backend) => backend.send_turn_spawned(request, answered),
                None => Err(BackendError {
                    message: "no active session".to_string(),
                    benign: true,
                }),
            },
            None => Err(BackendError {
                message: format!("no tab {}", id.0),
                benign: true,
            }),
        };
        match started {
            Ok(spawned) => {
                if synced {
                    self.desk_starts.push(DeskStart {
                        tab: id,
                        generation,
                        spawned: spawned.clone(),
                        turn_id: None,
                        ended: BTreeSet::new(),
                        lost_ending: false,
                        interrupts: Vec::new(),
                    });
                }

                self.desk_sends.insert(
                    id.0,
                    DeskSendWait {
                        gate,
                        turn,
                        generation,
                        ended_turn,
                        flush_owed: false,
                        restored_on_stop: false,
                        stopped_while_out: false,
                        spawned,
                        answer,
                    },
                );
                None
            }
            // Nothing was folded for a refusal known here, so there is no row to take back.
            Err(error) => Some(self.desk_send_done(gate, turn, Err(error))),
        }
    }

    /// The desk sends whose provider call answered since the last look, as the [`Sent`] a send made on this thread
    /// would have returned. An answer for a tab that has gone is dropped (its close answered its command); one for
    /// a session the tab no longer holds is told as not taken. Its queued messages are dropped only on
    /// success; an error restores them at the head, in order, with the reason for restoring them.
    /// Only a flush deferred while the call was out is owed again; a current busy reply also needs a new
    /// turn ending, since otherwise its messages still wait for the open turn. Never waits.
    pub(super) fn poll_desk_sends(&mut self) -> Vec<Sent> {
        let ready: Vec<(u64, Result<String, BackendError>)> = self
            .desk_sends
            .iter()
            .filter_map(|(id, wait)| match wait.answer.try_recv() {
                Ok(outcome) => Some((*id, outcome)),
                Err(mpsc::TryRecvError::Empty) => None,
                Err(mpsc::TryRecvError::Disconnected) => Some((
                    *id,
                    Err(BackendError {
                        message: "the send ended before the agent answered".to_string(),
                        benign: true,
                    }),
                )),
            })
            .collect();
        let mut sent = Vec::new();
        for (id, outcome) in ready {
            let Some(wait) = self.desk_sends.remove(&id) else {
                continue;
            };
            let DeskSendWait {
                gate,
                turn,
                generation,
                ended_turn,
                flush_owed,
                restored_on_stop,
                stopped_while_out,
                spawned,
                ..
            } = wait;
            let tab = TabId(id);
            let Some(now) = self.get(tab).map(Tab::backend_generation) else {
                continue;
            };
            let busy = outcome.as_ref().is_err_and(BackendError::is_turn_busy);
            let ended = outcome
                .as_ref()
                .is_err_and(|error| error.message == agent::SESSION_ENDED_BEFORE_SEND);
            if now != generation {
                self.end_start_of(&gate);
                let gone = Err(BackendError {
                    message: if restored_on_stop {
                        agent::STOPPED_BEFORE_SEND
                    } else if ended {
                        agent::SESSION_ENDED_BEFORE_SEND
                    } else {
                        "the tab's session changed before the agent answered"
                    }
                    .into(),
                    benign: true,
                });
                // The old backend owns its accepted turn; keep failed sends without installing its prompt here.
                // After an error that may hide delivery (timeout, transport loss or disconnect), a restored message
                // can reach a resumed conversation twice by choice, rather than not at all; never after success.
                sent.push(match turn {
                    DeskTurn::Queue { typed, items, .. } => {
                        if let Some(error) = outcome.as_ref().err().filter(|_| !restored_on_stop) {
                            if let Some(tab) = self.get_mut(tab) {
                                tab.session.queue.splice(0..0, items);
                                tab.session.queue_error = Some(if busy {
                                    "the tab's session changed; queued messages were not sent because the old session was busy"
                                        .to_string()
                                } else if ended {
                                    agent::SESSION_ENDED_BEFORE_SEND.to_string()
                                } else {
                                    format!(
                                        "the tab's session changed; queued messages may not have reached the agent: {}",
                                        error.message
                                    )
                                });
                            }
                        }
                        Sent::Flushed {
                            tab,
                            flush: Flush { typed, outcome: gone },
                        }
                    }
                    DeskTurn::First { request_id, turn } => {
                        if ended {
                            self.put_back(tab, &turn.typed);
                        }
                        Sent::First {
                            tab,
                            request_id,
                            typed: turn.typed,
                            outcome: gone,
                        }
                    }
                    DeskTurn::Prompt { request_id, page, turn } => {
                        if ended {
                            self.put_back(tab, &turn.typed);
                        }
                        Sent::Prompt {
                            tab,
                            page,
                            request_id,
                            typed: turn.typed,
                            outcome: gone,
                        }
                    }
                });
                if flush_owed {
                    self.owe_flush_if_idle(tab);
                }
                continue;
            }
            // A Stop made while the call was out, met by a busy reply: the agent was already running a turn this side
            // had not folded, which is what the Stop stops. The message, which the agent did not take, comes back to
            // the draft as a stopped send instead of waiting to steer. An interrupt still owed to this send goes to
            // that turn only while no turn has ended since the call went out; once one has, the turn the Stop meant
            // is over, and a later one is never stopped in its place.
            if busy && stopped_while_out {
                let start = self
                    .desk_starts
                    .iter()
                    .position(|s| s.spawned.client_message_id == spawned.client_message_id)
                    .map(|at| self.desk_starts.remove(at));
                let over = self.turn_ended_since(tab, ended_turn)
                    || start.as_ref().is_some_and(|s| !s.ended.is_empty() || s.lost_ending);
                let interrupts = start.map(|s| s.interrupts).unwrap_or_default();
                self.stop_into_busy_turn(tab, &turn, interrupts, !over);
                let stopped = Err(BackendError {
                    message: agent::STOPPED_BEFORE_SEND.into(),
                    benign: true,
                });
                sent.push(self.desk_send_done(gate, turn, stopped));
                if flush_owed && self.turn_ended_since(tab, ended_turn) {
                    self.owe_flush_if_idle(tab);
                }
                continue;
            }
            let outcome = match outcome {
                Ok(turn_id) => {
                    if let Some(start) = self
                        .desk_starts
                        .iter_mut()
                        .find(|s| s.spawned.client_message_id == spawned.client_message_id)
                    {
                        start.turn_id = Some(turn_id.clone());
                    }
                    let typed = match &turn {
                        DeskTurn::Queue { typed, .. } => typed.clone(),
                        DeskTurn::First { turn, .. } | DeskTurn::Prompt { turn, .. } => turn.typed.clone(),
                    };
                    if let Some(backend) = self.get_mut(tab).and_then(Tab::live_mut) {
                        backend.note_accepted_prompt(&typed);
                    }
                    Ok(spawned.submitted(turn_id))
                }
                Err(error) => {
                    // The row was folded when the call went out; a turn the agent already had open took nothing.
                    if error.is_turn_busy() && turn.has_prompt_row() {
                        if let Some(backend) = self.get_mut(tab).and_then(Tab::live_mut) {
                            backend.withdraw_prompt();
                        }
                    }
                    Err(error)
                }
            };
            sent.push(self.desk_send_done(gate, turn, outcome));
            if flush_owed && (!busy || self.turn_ended_since(tab, ended_turn)) {
                self.owe_flush_if_idle(tab);
            }
        }
        self.release_start_interrupts();
        sent
    }

    fn turn_ended_since(&self, tab: TabId, ended_turn: Option<u64>) -> bool {
        self.get(tab)
            .and_then(Tab::live)
            .and_then(|backend| backend.projection().last_turn_end_seq)
            > ended_turn
    }

    /// The Stop met by a busy reply: the send's row is withdrawn and its text put back in the draft, as a Stop during
    /// the wait does. Owed interrupts go to the turn the agent is running when `interrupt`, else are told there was
    /// nothing left to stop.
    fn stop_into_busy_turn(
        &mut self,
        id: TabId,
        turn: &DeskTurn,
        interrupts: Vec<Box<dyn FnOnce(Result<(), BackendError>) + Send>>,
        interrupt: bool,
    ) {
        let typed = match turn {
            DeskTurn::Queue { typed, .. } => typed.clone(),
            DeskTurn::First { turn, .. } | DeskTurn::Prompt { turn, .. } => turn.typed.clone(),
        };
        let Some(tab) = self.get_mut(id) else {
            for done in interrupts {
                done(Ok(()));
            }
            return;
        };
        match tab.live_mut() {
            Some(backend) => {
                if turn.has_prompt_row() {
                    backend.withdraw_prompt();
                }
                for done in interrupts {
                    if interrupt {
                        backend.interrupt_spawned(done);
                    } else {
                        done(Ok(()));
                    }
                }
            }
            None => {
                for done in interrupts {
                    done(Ok(()));
                }
            }
        }
        self.put_back(id, &typed);
    }

    /// Puts a send's text back in its tab's draft, ahead of what was typed since, for the page to merge into its box
    /// ([`TabSet::take_stopped_sends`]): the tab keeps it across a switch or a reload.
    fn put_back(&mut self, id: TabId, typed: &str) {
        if let Some(tab) = self.get_mut(id) {
            put_before_draft(tab, &[typed.to_string()]);
            self.desk_stopped.push((id, vec![typed.to_string()]));
        }
    }

    fn release_start_interrupts(&mut self) {
        self.desk_starts.retain_mut(|start| {
            let backend = self
                .tabs
                .iter_mut()
                .find(|tab| tab.id == start.tab && tab.backend_generation == start.generation)
                .and_then(Tab::live_mut);
            if let Some(backend) = backend {
                if let Some(turn_id) = &start.turn_id {
                    let projection = backend.projection();
                    if projection.active_turn_id.as_ref() == Some(turn_id) {
                        drop(projection);
                        for done in start.interrupts.drain(..) {
                            backend.interrupt_spawned(done);
                        }
                        return false;
                    }
                    // A whole turn can start and finish between polls, leaving no active turn to stop.
                    if !start.ended.contains(turn_id)
                        && !start.lost_ending
                        && projection.last_reset_revision <= start.spawned.after_revision
                        && !matches!(
                            projection.status,
                            agent::ProjectionStatus::Closed { .. } | agent::ProjectionStatus::Unavailable { .. }
                        )
                    {
                        return true;
                    }
                } else if self
                    .desk_sends
                    .get(&start.tab.0)
                    .is_some_and(|w| w.spawned.client_message_id == start.spawned.client_message_id)
                {
                    // The provider's answer identifies the turn this interrupt belongs to.
                    return true;
                }
            }
            // A refusal, session change or completed turn leaves nothing to interrupt.
            for done in start.interrupts.drain(..) {
                done(Ok(()));
            }
            false
        });
    }

    /// What a desk send's outcome does to its tab, as the send on this thread does it, and the [`Sent`] it is told
    /// as: a taken one is the tab's opening prompt; a queue the agent did not take returns to its original
    /// order with the reason beside it; a composer's message refused because a turn was open waits to steer into it.
    fn desk_send_done(
        &mut self,
        gate: SendGate,
        turn: DeskTurn,
        outcome: Result<SubmittedPrompt, BackendError>,
    ) -> Sent {
        let id = gate.tab();
        if outcome.is_err() {
            self.end_start_of(&gate);
        }
        match turn {
            DeskTurn::Queue {
                items,
                indices,
                kept,
                wire,
                typed,
                origin,
                after_turn,
                trace,
            } => {
                if let Some(tab) = self.get_mut(id) {
                    match &outcome {
                        Ok(submitted) => {
                            let from = items.first().map(|item| item.from.clone()).unwrap_or_default();
                            if let QueuedFrom::Remote { asker, .. } = &from {
                                let sender = asker.sent_from();
                                tab.notes
                                    .note_remote_sender(submitted.client_message_id.clone(), sender.clone());
                                tab.note_remote_prompt(&typed, submitted.after_revision, sender);
                            }
                            let mut message = Queued::held(&typed, wire, wall_clock_ms());
                            message.from = from;
                            message.typed_by_person = origin == PromptOrigin::Typed;
                            message.shell_report = origin == PromptOrigin::ShellLine;
                            message.steerable = !message.shell_report;
                            message.after_turn = after_turn;
                            tab.session.opening_prompt = Some(OpeningPrompt {
                                submitted: submitted.clone(),
                                typed_by_person: message.typed_by_person,
                                message,
                            });
                            tab.session.queue_error = None;
                            tab.session.queue_note = tab
                                .session
                                .queue
                                .iter()
                                .any(|item| item.refused)
                                .then(|| refused_note(agent::CLI_REFUSED_DROP));
                            tab.session.turn_trace = trace;
                        }
                        Err(error) if error.message == agent::STOPPED_BEFORE_SEND => {}
                        Err(error) => {
                            // Items still in the queue may have been taken back or read while the call was
                            // out. Count only surviving original neighbours; later messages stay behind them.
                            let queue = &mut tab.session.queue;
                            let mut after = 0;
                            let surviving: Vec<bool> = kept
                                .iter()
                                .map(|old| {
                                    let found = queue[after..].iter().position(|now| Arc::ptr_eq(&now.identity, old));
                                    if let Some(found) = found {
                                        after += found + 1;
                                    }
                                    found.is_some()
                                })
                                .collect();
                            for (removed, (index, item)) in indices.into_iter().zip(items).enumerate() {
                                let before = surviving[..index - removed].iter().filter(|kept| **kept).count();
                                queue.insert(before + removed, item);
                            }
                            tab.session.queue_error = Some(error.message.clone());
                        }
                    }
                    tab.session.was_running = tab.turn_running();
                }
                Sent::Flushed {
                    tab: id,
                    flush: Flush {
                        typed,
                        outcome: outcome.map(|_| ()),
                    },
                }
            }
            DeskTurn::First { request_id, turn } => {
                let outcome = self.opening_prompt_from(id, &turn, outcome);
                self.put_back_if_ended(id, &turn.typed, &outcome);
                Sent::First {
                    tab: id,
                    request_id,
                    typed: turn.typed,
                    outcome,
                }
            }
            DeskTurn::Prompt { request_id, page, turn } => {
                let mut outcome = self.opening_prompt_from(id, &turn, outcome);
                self.put_back_if_ended(id, &turn.typed, &outcome);
                if outcome.as_ref().is_err_and(BackendError::is_turn_busy)
                    && self.get(id).and_then(Tab::live).is_some()
                    && self.hold_as_steer(id, &turn)
                {
                    outcome = Ok(());
                }
                Sent::Prompt {
                    tab: id,
                    page,
                    request_id,
                    typed: turn.typed,
                    outcome,
                }
            }
        }
    }

    /// A message refused because its session ended during the wait is put back by the host, so it survives the page
    /// that sent it switching tabs or reloading; the page leaves that refusal's text to it.
    fn put_back_if_ended(&mut self, id: TabId, typed: &str, outcome: &Result<(), BackendError>) {
        if outcome
            .as_ref()
            .is_err_and(|error| error.message == agent::SESSION_ENDED_BEFORE_SEND)
        {
            self.put_back(id, typed);
        }
    }

    /// A single message the agent took is the tab's opening prompt, as [`TabSet::send_one`] records it.
    fn opening_prompt_from(
        &mut self,
        id: TabId,
        turn: &FirstTurn,
        outcome: Result<SubmittedPrompt, BackendError>,
    ) -> Result<(), BackendError> {
        let submitted = outcome?;
        if let Some(tab) = self.get_mut(id) {
            let mut message = Queued::held(&turn.typed, turn.wire.clone(), wall_clock_ms());
            message.typed_by_person = turn.typed_by_person;
            tab.session.opening_prompt = Some(OpeningPrompt {
                submitted,
                typed_by_person: message.typed_by_person,
                message,
            });
        }
        Ok(())
    }

    /// Asks `id`'s agent to stop its turn with the provider's call on a thread of its own, for a remote tab, whose
    /// call crosses the network. The number its outcome comes back under from [`TabSet::poll_interrupts`]. Refused
    /// while the tab is frozen for a move: its turn moves as it runs.
    pub fn interrupt_spawned(&mut self, id: TabId) -> Result<u64, BackendError> {
        if self.moving.contains(&id.0) {
            return Err(BackendError {
                message: MOVING.to_string(),
                benign: true,
            });
        }
        let generation = self.get(id).map_or(0, Tab::backend_generation);
        let wake = self.ui_wake.clone();
        let (told, answer) = mpsc::channel();
        let done = move |outcome: Result<(), BackendError>| {
            let _ = told.send(outcome);
            if let Some(wake) = wake {
                wake();
            }
        };
        let backend = self
            .tabs
            .iter_mut()
            .find(|tab| tab.id == id)
            .ok_or_else(|| BackendError {
                message: format!("no tab {}", id.0),
                benign: true,
            })?
            .live_mut()
            .ok_or_else(|| BackendError {
                message: "no active session".to_string(),
                benign: true,
            })?;
        if let Some(wait) = self
            .desk_sends
            .get_mut(&id.0)
            .filter(|wait| wait.generation == generation)
        {
            wait.stopped_while_out = true;
        }
        let pending_start = self
            .desk_starts
            .iter_mut()
            .rev()
            .find(|s| s.tab == id && s.generation == generation);
        match pending_start {
            Some(start) if backend.projection().active_turn_id.is_none() => start.interrupts.push(Box::new(done)),
            _ => backend.interrupt_spawned(done),
        }
        let number = self.next_interrupt;
        self.next_interrupt += 1;
        self.interrupts_out.push(InterruptWait {
            number,
            tab: id,
            generation,
            answer,
        });
        Ok(number)
    }

    /// The interrupts [`TabSet::interrupt_spawned`] made whose provider call answered since the last look. Never
    /// waits.
    pub fn poll_interrupts(&mut self) -> Vec<InterruptDone> {
        let mut done = Vec::new();
        self.interrupts_out.retain(|wait| {
            let outcome = match wait.answer.try_recv() {
                Ok(outcome) => outcome,
                Err(mpsc::TryRecvError::Empty) => return true,
                Err(mpsc::TryRecvError::Disconnected) => Err(BackendError {
                    message: "the interrupt ended before the agent answered".to_string(),
                    benign: true,
                }),
            };
            done.push(InterruptDone {
                number: wait.number,
                tab: wait.tab,
                generation: wait.generation,
                outcome,
            });
            false
        });
        done
    }

    /// Whether an interrupt or a desk send waits for its provider call.
    pub(super) fn desk_calls_out(&self) -> bool {
        !self.desk_sends.is_empty() || !self.interrupts_out.is_empty()
    }
}

/// Texts taken back from a send go into the draft ahead of what was typed since, so neither is lost.
fn put_before_draft(tab: &mut Tab, texts: &[String]) {
    let mut draft = texts.join("\n\n");
    if !tab.draft().is_empty() {
        draft.push_str("\n\n");
        draft.push_str(tab.draft());
    }
    tab.replace_draft(draft);
}
