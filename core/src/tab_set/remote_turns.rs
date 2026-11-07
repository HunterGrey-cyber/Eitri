//! A phone's messages in a tab: who queued each one and in which mode it was accepted, so the tab sends it only
//! while its mode is no wider, and a phone's send, whose provider call runs off the tick's thread.
//!
//! A phone's text is sent as it came: never composed with the editor's context, and marked as the host's
//! (`PromptOrigin::Composed`), never as typed at the desk. What the phone may send, and to which tab, the panel
//! decides before anything reaches here.

use super::*;
use crate::agent_backend::{SpawnedTurn, TurnRequest};

/// The device a phone's message came from, kept with the message.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RemoteAsker {
    /// The device's id in the device store: a device's own items are found by it, never by its name.
    pub device_id: String,
    /// The device's name, which the desk is told.
    pub device: String,
    /// The id of the command the message came in.
    pub command_id: String,
    /// The connection it came on. A connection is this process's own, so a move does not carry it.
    #[serde(skip)]
    pub conn: Option<u64>,
}

impl RemoteAsker {
    pub(super) fn sent_from(&self) -> crate::agent_bridge::SentFrom {
        crate::agent_bridge::SentFrom {
            device_id: self.device_id.clone(),
            name: self.device.clone(),
        }
    }
}

/// Who put a queued item there.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueuedFrom {
    /// The desk: its composer, a review, a `!` line, a restore.
    #[default]
    Desk,
    /// A phone, in a tab whose mode was `accepted_mode` when the message was taken.
    Remote {
        asker: RemoteAsker,
        accepted_mode: SessionModeChoice,
    },
}

impl QueuedFrom {
    pub fn is_desk(&self) -> bool {
        matches!(self, QueuedFrom::Desk)
    }

    /// Whether an item from here may go out of a tab whose mode is `mode` now: a desk's always; a phone's never in
    /// Bypass, and never in a mode wider than the one it was accepted in (Manual, then Auto: an Auto tab runs what
    /// a Manual tab would have asked about).
    pub fn may_go_in(&self, mode: SessionModeChoice) -> bool {
        match self {
            QueuedFrom::Desk => true,
            QueuedFrom::Remote { accepted_mode, .. } => mode_allows(*accepted_mode, mode),
        }
    }

    /// Whether two items may go out in one turn: both the desk's, or both one device's taken in the same mode. A
    /// turn mixing them could not be refused for one sender alone, nor taken back by one device.
    pub(super) fn same_sender(&self, other: &QueuedFrom) -> bool {
        match (self, other) {
            (QueuedFrom::Desk, QueuedFrom::Desk) => true,
            (
                QueuedFrom::Remote {
                    asker: a,
                    accepted_mode: ma,
                },
                QueuedFrom::Remote {
                    asker: b,
                    accepted_mode: mb,
                },
            ) => a.device_id == b.device_id && ma == mb,
            _ => false,
        }
    }
}

/// Whether a phone's words taken while the tab was in `accepted_mode` may go out of it in `mode`: never in Bypass,
/// and never in a mode wider than the one they were taken in.
fn mode_allows(accepted_mode: SessionModeChoice, mode: SessionModeChoice) -> bool {
    mode != SessionModeChoice::Bypass && width(mode) <= width(accepted_mode)
}

/// How much a mode lets the agent do without asking.
fn width(mode: SessionModeChoice) -> u8 {
    match mode {
        SessionModeChoice::Manual => 0,
        SessionModeChoice::Auto => 1,
        SessionModeChoice::Bypass => 2,
    }
}

/// A phone's queued message the tab took out without sending: its mode had widened since the message was accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteRefused {
    pub tab: TabId,
    pub asker: RemoteAsker,
    pub accepted_mode: SessionModeChoice,
    pub mode: SessionModeChoice,
}

/// A phone's send whose provider call is out, by tab: at most one per tab.
pub(super) struct RemoteSendWait {
    gate: SendGate,
    asker: RemoteAsker,
    accepted_mode: SessionModeChoice,
    /// The tab's backend generation when the call was made: an answer for a session the tab no longer holds
    /// changes nothing in the tab.
    generation: u64,
    typed: String,
    spawned: SpawnedTurn,
    answer: mpsc::Receiver<Result<String, BackendError>>,
}

/// What became of a phone's send: taken by the agent, or why not. `typed` is what was sent.
#[derive(Debug)]
pub struct RemoteSent {
    pub tab: TabId,
    pub asker: RemoteAsker,
    pub typed: String,
    pub outcome: Result<(), BackendError>,
    /// Whether the tab still holds the session the send was for: false once the tab closed, or its session was
    /// replaced while the call was out, and then the outcome is the phone's alone.
    pub same_session: bool,
}

impl Tab {
    pub fn note_remote_stop(&mut self, id: String, from: crate::agent_bridge::SentFrom) {
        self.notes.stopped_from.insert(id, from);
        self.notes.remote_notes_dirty = true;
    }

    pub(super) fn note_remote_prompt(&mut self, text: &str, after: u64, from: crate::agent_bridge::SentFrom) {
        let seq = self.live().and_then(|backend| {
            backend
                .projection()
                .user_prompts
                .iter()
                .find(|p| p.seq >= after && p.text == text && !p.delivered)
                .map(|p| p.seq)
        });
        if let Some(seq) = seq {
            self.notes.sent_from.insert(seq, from);
            self.notes.remote_notes_dirty = true;
        }
    }

    /// Takes the phone's messages the tab's mode no longer lets go out of its queue, and says which. A message
    /// already put into the running turn stays unless `steered_too`: the queue is about to send again whatever
    /// the agent never said it read, and a phone's goes out again only as one held would.
    pub(super) fn refuse_widened(&mut self, steered_too: bool) -> Vec<RemoteRefused> {
        let mode = self.mode;
        if self.session.queue.iter().all(|item| item.from.may_go_in(mode)) {
            return Vec::new();
        }
        let (kept, refused): (Vec<Queued>, Vec<Queued>) = std::mem::take(&mut self.session.queue)
            .into_iter()
            .partition(|item| (!steered_too && !item.is_held()) || item.from.may_go_in(mode));
        self.session.queue = kept;
        refused
            .into_iter()
            .filter_map(|item| match item.from {
                QueuedFrom::Remote { asker, accepted_mode } => Some(RemoteRefused {
                    tab: self.id,
                    asker,
                    accepted_mode,
                    mode,
                }),
                QueuedFrom::Desk => None,
            })
            .collect()
    }
}

impl TabSet {
    /// A phone's message, queued behind the turn running in `id` as a held item that remembers the device and the
    /// tab's mode now. Refused as [`TabSet::queue_message`] refuses; the caller checked the rest.
    pub fn queue_remote(&mut self, id: TabId, text: &str, asker: RemoteAsker, now_ms: u64) -> Result<usize, String> {
        let accepted_mode = self.get(id).ok_or_else(|| format!("no tab {}", id.0))?.mode;
        let length = self.queue_message(id, text, text.to_owned(), now_ms, true, PromptOrigin::Composed)?;
        if let Some(item) = self.get_mut(id).and_then(|tab| tab.session.queue.last_mut()) {
            item.from = QueuedFrom::Remote { asker, accepted_mode };
        }
        Ok(length)
    }

    /// The held messages device `device_id` queued in `id`, taken out of its queue, oldest first. A message already
    /// sent into the running turn stays, as it does for the desk; nobody else's is touched.
    pub fn take_back_remote(&mut self, id: TabId, device_id: &str) -> Vec<String> {
        let Some(tab) = self.get_mut(id) else { return Vec::new() };
        tab.settle_steered();
        let mine = |item: &Queued| {
            item.is_held() && matches!(&item.from, QueuedFrom::Remote { asker, .. } if asker.device_id == device_id)
        };
        if !tab.session.queue.iter().any(mine) {
            return Vec::new();
        }
        let (taken, kept): (Vec<Queued>, Vec<Queued>) =
            std::mem::take(&mut tab.session.queue).into_iter().partition(mine);
        tab.session.queue = kept;
        if tab.session.queue.is_empty() {
            tab.session.queue_error = None;
            tab.session.queue_note = None;
        }
        taken.into_iter().map(|item| item.text).collect()
    }

    /// A phone's message as the next turn of `id`'s idle session, through the tab's own start of a turn
    /// ([`TabSet::begin_turn_send`]). The provider's call runs on a thread of its own: what became of it comes from
    /// [`TabSet::poll_remote_sends`]. Refused at once, with nothing sent, while the tab cannot start a turn now: it
    /// is moving or handed off, a start or a send of it is under way, a turn runs, a card, a question or a `!` line
    /// holds it, or its queue waits to go.
    pub fn send_remote(&mut self, id: TabId, text: &str, asker: RemoteAsker) -> Result<(), String> {
        if let Some(why) = self.why_no_turn_send(id) {
            return Err(why);
        }
        if self.starting.contains(&id.0) || self.remote_sends.contains_key(&id.0) {
            return Err("a turn is starting in this tab".to_string());
        }
        if self.card_waits(id) || self.shell_line_runs(id) {
            return Err("the tab waits for the desk".to_string());
        }
        let tab = self.get(id).ok_or_else(|| format!("no tab {}", id.0))?;
        if tab.pending_handoff.is_some() {
            return Err("this tab is being handed off to a terminal".to_string());
        }
        if tab.turn_running() {
            return Err("a turn is running".to_string());
        }
        if tab.session.queue.iter().any(|item| !item.refused) {
            return Err("the tab's queue waits to go first".to_string());
        }
        let accepted_mode = tab.mode;
        let gate = self.begin_turn_send(id);
        let turn = FirstTurn {
            wire: text.to_owned(),
            typed: text.to_owned(),
            typed_by_person: false,
            shell_line: false,
        };
        // Sent now, or held for the presence; either way its outcome comes through `poll_remote_sends`.
        let sent = self.send_when_allowed(
            gate,
            moving::TurnSend::Remote {
                turn,
                asker,
                accepted_mode,
            },
        );
        debug_assert!(sent.is_none(), "a phone's send is answered by poll_remote_sends");
        Ok(())
    }

    /// Lets a phone's send go: the row is folded and the provider's call starts on a thread of its own. What
    /// [`TabSet::send_remote`] checked is checked again, since the presence may have held the send meanwhile: a
    /// card, a question or a `!` line that came since holds it back, and a mode wider than the one it was taken in
    /// refuses it ([`TabSet::take_remote_refused`]).
    pub(super) fn release_remote_send(
        &mut self,
        gate: SendGate,
        turn: FirstTurn,
        asker: RemoteAsker,
        accepted_mode: SessionModeChoice,
    ) {
        let id = gate.tab();
        if self.card_waits(id) || self.shell_line_runs(id) {
            self.end_start_of(&gate);
            self.remote_not_sent(id, turn, asker, "the tab waits for the desk".to_string());
            return;
        }
        if let Some(mode) = self
            .get(id)
            .map(Tab::mode)
            .filter(|mode| !mode_allows(accepted_mode, *mode))
        {
            self.end_start_of(&gate);
            self.remote_refused.push(RemoteRefused {
                tab: id,
                asker,
                accepted_mode,
                mode,
            });
            return;
        }
        let generation = self.get(id).map_or(0, Tab::backend_generation);
        let wake = self.ui_wake.clone();
        let (told, answer) = mpsc::channel();
        let answered = move |outcome: Result<String, BackendError>| {
            let _ = told.send(outcome);
            if let Some(wake) = wake {
                wake();
            }
        };
        let request = TurnRequest {
            wire: turn.wire.clone(),
            typed: turn.typed.clone(),
            origin: PromptOrigin::Composed,
        };
        let started = match self.get_mut(id) {
            Some(tab) => {
                tab.session.turn_trace = crate::turn_trace::TurnTrace::start();
                match tab.live_mut() {
                    Some(backend) => backend.send_turn_spawned(request, answered),
                    None => Err(BackendError {
                        message: "no active session".to_string(),
                        benign: true,
                    }),
                }
            }
            None => Err(BackendError {
                message: format!("no tab {}", id.0),
                benign: true,
            }),
        };
        match started {
            Ok(spawned) => {
                self.remote_sends.insert(
                    id.0,
                    RemoteSendWait {
                        gate,
                        asker,
                        accepted_mode,
                        generation,
                        typed: turn.typed,
                        spawned,
                        answer,
                    },
                );
            }
            Err(outcome) => {
                self.end_start_of(&gate);
                self.remote_sent.push(RemoteSent {
                    tab: id,
                    asker,
                    typed: turn.typed,
                    outcome: Err(outcome),
                    same_session: true,
                });
            }
        }
    }

    /// A phone's send that did not go out (the presence could not mark it as running, something came to hold the
    /// tab, the tab left): the phone is told.
    pub(super) fn remote_not_sent(&mut self, tab: TabId, turn: FirstTurn, asker: RemoteAsker, note: String) {
        self.remote_sent.push(RemoteSent {
            tab,
            asker,
            typed: turn.typed,
            outcome: Err(BackendError {
                message: note,
                benign: true,
            }),
            same_session: true,
        });
    }

    /// How many of the phone's sends are out or held for the presence: each becomes a provider call.
    pub fn remote_sends_out(&self) -> usize {
        self.remote_sends.len()
            + self
                .pending_sends
                .values()
                .filter(|pending| pending.is_remote())
                .count()
    }

    /// What became of the phone's sends since the last call: those whose provider call answered, and those refused
    /// before it was made. An accepted one is the tab's opening prompt, as a desk's send is, and stays the phone's
    /// (its device and the mode it was taken in) should the agent hand it back to the queue; one the agent turned
    /// down because it already had a turn open has its row taken back. An answer for a session the tab no longer
    /// holds (it closed, or another session took its place) changes nothing in the tab and is the phone's alone.
    /// Never waits.
    pub fn poll_remote_sends(&mut self) -> Vec<RemoteSent> {
        let mut done = std::mem::take(&mut self.remote_sent);
        let ready: Vec<(u64, Result<String, BackendError>)> = self
            .remote_sends
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
        for (id, outcome) in ready {
            let Some(wait) = self.remote_sends.remove(&id) else {
                continue;
            };
            let RemoteSendWait {
                gate,
                asker,
                accepted_mode,
                generation,
                typed,
                spawned,
                ..
            } = wait;
            let tab = TabId(id);
            let same_session = self.get(tab).is_some_and(|t| t.backend_generation() == generation);
            if !same_session {
                // The start was the old session's; nothing of the tab's now waits on it.
                if self.get(tab).is_some() {
                    self.end_start_of(&gate);
                }
                done.push(RemoteSent {
                    tab,
                    asker,
                    typed,
                    outcome: outcome.map(|_| ()),
                    same_session,
                });
                continue;
            }
            let outcome = match outcome {
                Ok(turn_id) => {
                    let t = self.get_mut(tab).expect("checked above");
                    if let Some(backend) = t.live_mut() {
                        backend.note_accepted_prompt(&typed);
                    }
                    t.note_remote_prompt(&typed, spawned.after_revision, asker.sent_from());
                    t.notes
                        .note_remote_sender(spawned.client_message_id.clone(), asker.sent_from());
                    let mut message = Queued::held(&typed, typed.clone(), wall_clock_ms());
                    message.from = QueuedFrom::Remote {
                        asker: asker.clone(),
                        accepted_mode,
                    };
                    t.session.opening_prompt = Some(OpeningPrompt {
                        submitted: spawned.submitted(turn_id),
                        typed_by_person: false,
                        message,
                    });
                    Ok(())
                }
                Err(error) => {
                    if error.is_turn_busy() {
                        if let Some(backend) = self.get_mut(tab).and_then(Tab::live_mut) {
                            backend.withdraw_prompt();
                        }
                    }
                    self.end_start_of(&gate);
                    Err(error)
                }
            };
            done.push(RemoteSent {
                tab,
                asker,
                typed,
                outcome,
                same_session,
            });
        }
        done
    }

    /// Whether a phone's send of `id` is out, or something about the phone's messages waits to be told.
    pub(super) fn remote_needs_polling(&self) -> bool {
        !self.remote_sends.is_empty() || !self.remote_sent.is_empty() || !self.remote_refused.is_empty()
    }

    /// Whether a phone's send of `id` waits for its provider call.
    pub fn remote_send_waits(&self, id: TabId) -> bool {
        self.remote_sends.contains_key(&id.0)
    }

    /// The phone's queued messages taken out unsent since the last call (their tab's mode had widened).
    pub fn take_remote_refused(&mut self) -> Vec<RemoteRefused> {
        std::mem::take(&mut self.remote_refused)
    }

    /// [`Tab::refuse_widened`] in `id`, kept for [`TabSet::take_remote_refused`]; whether its queue changed.
    pub(super) fn refuse_widened_in(&mut self, id: TabId) -> bool {
        let refused = self
            .get_mut(id)
            .map(|tab| tab.refuse_widened(false))
            .unwrap_or_default();
        let changed = !refused.is_empty();
        self.remote_refused.extend(refused);
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn asker() -> RemoteAsker {
        RemoteAsker {
            device_id: "d1".into(),
            device: "Pixel 8".into(),
            command_id: "q1".into(),
            conn: Some(7),
        }
    }

    #[test]
    fn a_phone_item_goes_only_in_a_mode_no_wider_than_it_was_taken_in() {
        use SessionModeChoice::{Auto, Bypass, Manual};
        assert!([Manual, Auto, Bypass]
            .into_iter()
            .all(|mode| QueuedFrom::Desk.may_go_in(mode)));
        let taken_in = |accepted_mode| QueuedFrom::Remote {
            asker: asker(),
            accepted_mode,
        };
        for (accepted, now, goes) in [
            (Manual, Manual, true),
            (Manual, Auto, false),
            (Manual, Bypass, false),
            (Auto, Manual, true),
            (Auto, Auto, true),
            (Auto, Bypass, false),
            (Bypass, Bypass, false),
        ] {
            assert_eq!(taken_in(accepted).may_go_in(now), goes, "{accepted:?} -> {now:?}");
        }
    }

    #[test]
    fn a_phone_item_keeps_its_device_across_a_move_but_not_its_connection() {
        let mut item = Queued::held("x", "x".into(), 1);
        let desk = serde_json::to_value(&item).unwrap();
        assert!(desk.get("from").is_none(), "a desk item is written as before");
        item.from = QueuedFrom::Remote {
            asker: asker(),
            accepted_mode: SessionModeChoice::Manual,
        };
        let moved: Queued = serde_json::from_str(&serde_json::to_string(&item).unwrap()).unwrap();
        let QueuedFrom::Remote {
            asker: moved,
            accepted_mode,
        } = moved.from
        else {
            panic!("the origin was lost: {moved:?}");
        };
        assert_eq!(accepted_mode, SessionModeChoice::Manual);
        assert_eq!(
            moved,
            RemoteAsker { conn: None, ..asker() },
            "a connection is this process's own"
        );
        let old: Queued = serde_json::from_value(desk).unwrap();
        assert_eq!(
            old.from,
            QueuedFrom::Desk,
            "an item written before origins is the desk's"
        );
    }
}
