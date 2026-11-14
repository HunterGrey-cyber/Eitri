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
    spawned: SpawnedTurn,
    answer: mpsc::Receiver<Result<String, BackendError>>,
}

impl DeskTurn {
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
        let request = TurnRequest { wire, typed, origin };
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
                self.desk_sends.insert(
                    id.0,
                    DeskSendWait {
                        gate,
                        turn,
                        generation,
                        ended_turn,
                        flush_owed: false,
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
                spawned,
                ..
            } = wait;
            let tab = TabId(id);
            let Some(now) = self.get(tab).map(Tab::backend_generation) else {
                continue;
            };
            let busy = outcome.as_ref().is_err_and(BackendError::is_turn_busy);
            if now != generation {
                self.end_start_of(&gate);
                let gone = Err(BackendError {
                    message: "the tab's session changed before the agent answered".to_string(),
                    benign: true,
                });
                // The old backend owns its accepted turn; keep failed sends without installing its prompt here.
                // After an error that may hide delivery (timeout, transport loss or disconnect), a restored message
                // can reach a resumed conversation twice by choice, rather than not at all; never after success.
                sent.push(match turn {
                    DeskTurn::Queue { typed, items, .. } => {
                        if let Err(error) = &outcome {
                            if let Some(tab) = self.get_mut(tab) {
                                tab.session.queue.splice(0..0, items);
                                tab.session.queue_error = Some(if busy {
                                    "the tab's session changed; queued messages were not sent because the old session was busy"
                                        .to_string()
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
                    DeskTurn::First { request_id, turn } => Sent::First {
                        tab,
                        request_id,
                        typed: turn.typed,
                        outcome: gone,
                    },
                    DeskTurn::Prompt { request_id, page, turn } => Sent::Prompt {
                        tab,
                        page,
                        request_id,
                        typed: turn.typed,
                        outcome: gone,
                    },
                });
                if flush_owed {
                    self.owe_flush_if_idle(tab);
                }
                continue;
            }
            let outcome = match outcome {
                Ok(turn_id) => {
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
                    if error.is_turn_busy() {
                        if let Some(backend) = self.get_mut(tab).and_then(Tab::live_mut) {
                            backend.withdraw_prompt();
                        }
                    }
                    Err(error)
                }
            };
            sent.push(self.desk_send_done(gate, turn, outcome));
            let turn_ended = self
                .get(tab)
                .and_then(Tab::live)
                .and_then(|backend| backend.projection().last_turn_end_seq)
                > ended_turn;
            if flush_owed && (!busy || turn_ended) {
                self.owe_flush_if_idle(tab);
            }
        }
        sent
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
                Sent::First {
                    tab: id,
                    request_id,
                    typed: turn.typed,
                    outcome,
                }
            }
            DeskTurn::Prompt { request_id, page, turn } => {
                let mut outcome = self.opening_prompt_from(id, &turn, outcome);
                if outcome.as_ref().is_err_and(BackendError::is_turn_busy) && self.hold_as_steer(id, &turn) {
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
            .get_mut(id)
            .ok_or_else(|| BackendError {
                message: format!("no tab {}", id.0),
                benign: true,
            })?
            .live_mut()
            .ok_or_else(|| BackendError {
                message: "no active session".to_string(),
                benign: true,
            })?;
        backend.interrupt_spawned(done);
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
