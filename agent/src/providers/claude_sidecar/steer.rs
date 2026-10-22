// agent/src/providers/claude_sidecar/steer.rs
//! Messages put into a running turn with `SteerTurn`, from the moment the host hands one over until
//! the sidecar says what became of it: the order they are sent in, and the hold that keeps a turn's end
//! from reaching the host while one is still open.
//!
//! **Sending, in order, off the caller's thread.** `steer_turn` only records the message here and
//! returns; the provider's runtime sends it. One message is in flight at a time: Verdandi's kernel puts
//! a steered message on the CLI's input queue before it answers `SteerTurn`, so a message sent only
//! after every earlier one was answered reaches the CLI in the order the user typed. A later message
//! waits here, unsent, until the one before it is answered; when that one is refused, every message
//! waiting behind it is dropped unsent, so nothing overtakes a refusal. A refusal is reported as
//! `UserMessageDropped` with words the host shows after "held until the turn ends:".
//!
//! **Why the hold exists.** When the CLI cannot fold a message into the running turn (a plain-text
//! answer, an interrupt), it runs it after that turn as a turn of its own. The sidecar then sends the
//! running turn's `TurnCompleted`, at once a `TurnStarted` for a follow-up turn nobody started with
//! `SendTurn`, and the message's `UserMessageDelivered` inside it. The host decides what to do with
//! its queue from one fold of one `pump()` batch: had the `TurnCompleted` been folded on its own, it
//! would see no turn running and a message with no outcome, hold that message again, and send it once
//! the follow-up ended -- the user's words would run twice. So a `TurnCompleted` that arrives while
//! any message has no outcome yet is kept back, with everything after it, until the follow-up's
//! `TurnStarted` arrives; both then leave in one `pump()` batch.
//!
//! Kept back for a bounded time only ([`FOLLOW_UP_WAIT`]): the sidecar sends the two events back to
//! back, so a long wait means something broke, and the host must never be left with a message whose
//! fate is open once no turn runs. When the wait runs out, or the session ends first, every open
//! message is reported dropped BEFORE the held `TurnCompleted`, so the host holds it again and the
//! turn's end then finds nothing unreported. Giving up is not knowing: a stream that was only slow may
//! still bring the delivery, and it is passed on with the typed text under the same id, so the host
//! can take back the message it held again instead of sending it a second time.
//!
//! **The wait counts only while the stream is up.** A broken watch stream is re-opened from the last
//! sequence seen, and the sidecar replays what was missed, the follow-up's `TurnStarted` and the
//! delivery included. Giving up while it is down would release the turn's end on a guess: the host
//! would see no turn running and send the message as a turn of its own, and if the follow-up had
//! already finished by then, the sidecar would take that turn and the replayed delivery would come too
//! late to stop it, so the words would run twice. So the stream breaking stops the wait
//! ([`SteerGate::stream_lost`]), and its re-opening starts a whole new one
//! ([`SteerGate::stream_back`]), which the replay that follows has to beat. A stream that cannot be
//! re-opened ends the session, and that ends the hold.
//!
//! Pure state, no I/O and no clock: the caller (`EventQueue` in `mod.rs`) owns the lock, the timer and
//! the sending, so every rule here is tested without a sidecar or a runtime. `R` is the request the
//! caller sends; it is only kept and handed back.

use crate::provider::ProviderError;
#[cfg(test)]
use crate::steer_gate::BEHIND_A_REFUSAL;
pub(crate) use crate::steer_gate::MAX_OPEN_STEERS;
use crate::steer_gate::{self, SteerEntry};
use crate::AgentDomainEvent;
use std::collections::VecDeque;
use std::time::Duration;

/// How long a turn's end waits for the follow-up turn the sidecar opens for a message still to run,
/// counted only while the watch stream is up: a break stops it, and the re-opened stream gets the
/// whole wait again for its replay. The sidecar sends the follow-up's `TurnStarted` right after the
/// `TurnCompleted`, in the same write, so this only bounds a peer that stopped talking on a live
/// stream.
pub(crate) const FOLLOW_UP_WAIT: Duration = Duration::from_secs(5);

/// Why a message is reported dropped when its fate was never said and the wait for a follow-up turn
/// ran out.
pub(crate) const UNREPORTED: &str = "Claude did not say whether it read it";
/// Why a message is reported dropped when the session ended with its fate open.
pub(crate) const SESSION_ENDED: &str = "the session ended before Claude read it";
/// Why a message waiting behind one whose answer was lost is dropped unsent.
pub(crate) const BEHIND_AN_UNKNOWN: &str =
    "an earlier message may not have reached Claude, and this one waits behind it";

/// Where one message is in being sent.
#[derive(Debug)]
enum Sending<R> {
    /// Not sent: an earlier message has not been answered yet.
    Waiting(R),
    /// Sent, no answer yet.
    InFlight,
    /// The sidecar answered that the CLI has it, or the answer was lost and it may.
    Answered,
}

/// One message whose outcome has not been reported.
#[derive(Debug)]
struct OpenSteer<R> {
    client_id: String,
    /// What the user typed: the wire's delivery event carries no text, and the transcript row shows
    /// this.
    as_typed: String,
    sending: Sending<R>,
}

impl<R> SteerEntry for OpenSteer<R> {
    fn client_id(&self) -> &str {
        &self.client_id
    }

    fn waiting(&self) -> bool {
        matches!(self.sending, Sending::Waiting(_))
    }

    fn in_flight(&self) -> bool {
        matches!(self.sending, Sending::InFlight)
    }
}

/// Why a session's gate cannot be handed to another process now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SteerBusy {
    /// A message is open: waiting to be sent, sent and unanswered, or answered and not yet delivered.
    Open,
    /// A turn's end is held for the follow-up turn of an open message.
    HoldingTurnEnd,
}

/// What the stream's next event did to the hold.
#[derive(Debug, PartialEq)]
pub(crate) struct Passed {
    /// Ready for `pump()`, in order.
    pub(crate) ready: Vec<AgentDomainEvent>,
    /// A hold began with this event; the caller arms the timer for this generation.
    pub(crate) hold_started: Option<u64>,
}

#[derive(Debug)]
pub(crate) struct SteerGate<R> {
    /// In the order the host handed them over.
    open: steer_gate::SteerOrder<OpenSteer<R>>,
    /// The held `TurnCompleted` and everything after it, while `holding` is set.
    held: Vec<AgentDomainEvent>,
    /// The generation of the hold in force, so a timer armed for an earlier one does nothing.
    holding: Option<u64>,
    generations: u64,
    /// Messages that may have reached the CLI and were reported dropped because the wait for them ran
    /// out or the stream ended, with their typed text, newest last, at most [`MAX_OPEN_STEERS`]: a
    /// delivery that still comes for one is passed on.
    given_up: VecDeque<(String, String)>,
    /// The watch stream broke and has not been re-opened: no hold may give up meanwhile.
    stream_down: bool,
}

impl<R> Default for SteerGate<R> {
    fn default() -> Self {
        Self {
            open: steer_gate::SteerOrder::default(),
            held: Vec::new(),
            holding: None,
            generations: 0,
            given_up: VecDeque::new(),
            stream_down: false,
        }
    }
}

impl<R> SteerGate<R> {
    /// Takes a message from the host. `Some(request)` means send it now; `None` means it waits for the
    /// message before it to be answered, and [`Self::accepted`] hands it out then. Refused, nothing
    /// recorded, when the id is already open or too many are open; the host holds it for the turn's end.
    pub(crate) fn open(&mut self, client_id: &str, as_typed: String, request: R) -> Result<Option<R>, ProviderError> {
        let mut now = None;
        self.open.open(client_id, "Claude", |behind| {
            let sending = if behind {
                Sending::Waiting(request)
            } else {
                now = Some(request);
                Sending::InFlight
            };
            Ok(OpenSteer {
                client_id: client_id.to_string(),
                as_typed,
                sending,
            })
        })?;
        Ok(now)
    }

    /// `SteerTurn` answered: the CLI has the message. Returns the next message to send, if one waits.
    pub(crate) fn accepted(&mut self, client_id: &str) -> Option<(String, R)> {
        if let Some(steer) = self.open.iter_mut().find(|s| s.client_id == client_id) {
            steer.sending = Sending::Answered;
        }
        self.next_to_send()
    }

    /// `SteerTurn` was refused: nothing reached the CLI. The message is reported dropped with `reason`,
    /// and every message waiting behind it is dropped unsent.
    pub(crate) fn refused(&mut self, client_id: &str, reason: &str) -> Vec<AgentDomainEvent> {
        let Some(at) = self.open.iter().position(|s| s.client_id == client_id) else {
            return Vec::new();
        };
        let out = self.open.refused(at, reason);
        self.settled(out)
    }

    /// `SteerTurn` got no answer from the sidecar (the transport failed, or no answer in time): the
    /// CLI may have the message, so its outcome is awaited like any other. Whatever waits behind it is
    /// dropped unsent, since it might otherwise reach the CLI first.
    pub(crate) fn unknown(&mut self, client_id: &str) -> Vec<AgentDomainEvent> {
        if let Some(steer) = self.open.iter_mut().find(|s| s.client_id == client_id) {
            steer.sending = Sending::Answered;
        }
        let out = self.drop_waiting(BEHIND_AN_UNKNOWN);
        self.settled(out)
    }

    /// One translated event off the stream.
    pub(crate) fn pass(&mut self, mut event: AgentDomainEvent) -> Passed {
        let mut passed = Passed {
            ready: Vec::new(),
            hold_started: None,
        };
        match &mut event {
            AgentDomainEvent::UserMessageDelivered {
                client_message_id,
                text,
                ..
            } => {
                if let Some(at) = self.open.iter().position(|s| &s.client_id == client_message_id) {
                    *text = self.open.remove(at).as_typed;
                } else if let Some(at) = self.given_up.iter().position(|(id, _)| id == client_message_id) {
                    // Reported dropped when the wait ran out, and read after all: passed on, so the
                    // host takes back the message it held again rather than sending it twice.
                    let (_, typed) = self.given_up.remove(at).expect("found above");
                    eprintln!(
                        "agent: ClaudeSidecarProvider: a message reported dropped was read after all ({client_message_id})"
                    );
                    *text = typed;
                } else {
                    // Not a message this provider sent, or one already reported: folding it would add
                    // a prompt row nobody typed.
                    eprintln!(
                        "agent: ClaudeSidecarProvider: a delivery for a message this client has no record of ({client_message_id}); ignored"
                    );
                    return passed;
                }
            }
            AgentDomainEvent::UserMessageDropped { client_message_id, .. } => {
                self.open.retain(|s| &s.client_id != client_message_id);
                self.given_up.retain(|(id, _)| id != client_message_id);
            }
            _ => {}
        }
        let terminal = matches!(
            event,
            AgentDomainEvent::SessionClosed { .. } | AgentDomainEvent::SessionUnavailable { .. }
        );
        if self.holding.is_some() {
            let started = matches!(event, AgentDomainEvent::TurnStarted { .. });
            self.held.push(event);
            if terminal {
                passed.ready = self.give_up_then_release(SESSION_ENDED);
            } else if started || self.open.is_empty() {
                passed.ready = self.release();
            }
            return passed;
        }
        if terminal {
            passed.ready = self.give_up(SESSION_ENDED);
            passed.ready.push(event);
            return passed;
        }
        if matches!(event, AgentDomainEvent::TurnCompleted { .. }) && !self.open.is_empty() {
            self.generations += 1;
            self.holding = Some(self.generations);
            self.held.push(event);
            passed.hold_started = self.holding;
            return passed;
        }
        passed.ready.push(event);
        passed
    }

    /// The wait armed for `generation` ran out. If that hold is still in force, every open message is
    /// dropped and the held events go, in that order.
    pub(crate) fn expire(&mut self, generation: u64) -> Vec<AgentDomainEvent> {
        if self.holding != Some(generation) || self.stream_down {
            return Vec::new();
        }
        eprintln!(
            "agent: ClaudeSidecarProvider: a turn ended with {} message(s) still to run and no follow-up turn came in {:?}; they are reported dropped",
            self.open.len(),
            FOLLOW_UP_WAIT
        );
        self.give_up_then_release(UNREPORTED)
    }

    /// The watch stream broke. The wait armed for a hold in force no longer counts: what it waits for
    /// may be in the replay, and that comes only once the stream is open again.
    pub(crate) fn stream_lost(&mut self) {
        self.stream_down = true;
        if self.holding.is_some() {
            self.generations += 1;
            self.holding = Some(self.generations);
        }
    }

    /// The watch stream is open again and its replay follows. `Some(generation)` when a hold is in
    /// force: the caller arms a whole new wait for it.
    pub(crate) fn stream_back(&mut self) -> Option<u64> {
        self.stream_down = false;
        if self.holding.is_some() {
            self.generations += 1;
            self.holding = Some(self.generations);
        }
        self.holding
    }

    /// Events held back, for the queue depth a caller reports.
    pub(crate) fn held_len(&self) -> usize {
        self.held.len()
    }

    /// Whether a turn's end is held back for its follow-up.
    pub(crate) fn holding(&self) -> bool {
        self.holding.is_some()
    }

    /// What another process needs to carry this gate on for the same session: the messages reported
    /// dropped whose delivery may still come. Refused while a message is open (sent and not yet
    /// delivered or dropped, or waiting to be sent) or a turn's end is held for a follow-up: the
    /// sending, the answer and the hold all live in this process and cannot be handed over.
    ///
    /// Destructures the gate without a rest pattern, so a field added to it has to say whether it moves.
    pub(crate) fn hand_over(&self) -> Result<Vec<(String, String)>, SteerBusy> {
        let SteerGate {
            open,
            held,
            holding,
            // Rebuilt: a gate taken over holds nothing, so it starts counting its holds afresh.
            generations: _,
            given_up,
            // Rebuilt: the taking side's watch is not open yet, and opening it is what says it is up.
            stream_down: _,
        } = self;
        if holding.is_some() || !held.is_empty() {
            return Err(SteerBusy::HoldingTurnEnd);
        }
        if !open.is_empty() {
            return Err(SteerBusy::Open);
        }
        Ok(given_up.iter().cloned().collect())
    }

    /// The gate of a session taken over from another process: nothing open or held, and the messages
    /// that process had given up on, so a delivery that still comes for one is passed on there too.
    pub(crate) fn taken_over(given_up: Vec<(String, String)>) -> Self {
        let mut gate = Self::default();
        let skip = given_up.len().saturating_sub(MAX_OPEN_STEERS);
        gate.given_up = given_up.into_iter().skip(skip).collect();
        gate
    }

    /// The first waiting message, once nothing is in flight.
    fn next_to_send(&mut self) -> Option<(String, R)> {
        let at = self.open.next_to_send()?;
        let steer = &mut self.open[at];
        let Sending::Waiting(request) = std::mem::replace(&mut steer.sending, Sending::InFlight) else {
            unreachable!("found waiting above");
        };
        Some((steer.client_id.clone(), request))
    }

    /// Drops every message not sent yet. Nothing of them reached the CLI, so nothing is remembered.
    fn drop_waiting(&mut self, reason: &str) -> Vec<AgentDomainEvent> {
        let (waiting, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut self.open)
            .into_iter()
            .partition(|s| matches!(s.sending, Sending::Waiting(_)));
        self.open = kept.into_iter().collect();
        waiting.into_iter().map(|s| dropped(s.client_id, reason)).collect()
    }

    /// Outcomes reported off the stream's path (a refusal, a lost answer): if they settle the last
    /// open message during a hold, the hold ends with them ahead of the held end; during a hold that
    /// goes on they wait behind the held events.
    fn settled(&mut self, mut out: Vec<AgentDomainEvent>) -> Vec<AgentDomainEvent> {
        if self.holding.is_none() {
            return out;
        }
        if self.open.is_empty() {
            out.extend(self.release());
            out
        } else {
            self.held.append(&mut out);
            Vec::new()
        }
    }

    /// Every open message is reported dropped with `reason`; those that may have reached the CLI are
    /// remembered, so a delivery that still comes is honoured.
    fn give_up(&mut self, reason: &str) -> Vec<AgentDomainEvent> {
        let open = std::mem::take(&mut self.open);
        let mut out = Vec::with_capacity(open.len());
        for steer in open {
            if !matches!(steer.sending, Sending::Waiting(_)) {
                if self.given_up.len() >= MAX_OPEN_STEERS {
                    self.given_up.pop_front();
                }
                self.given_up.push_back((steer.client_id.clone(), steer.as_typed));
            }
            out.push(dropped(steer.client_id, reason));
        }
        out
    }

    fn give_up_then_release(&mut self, reason: &str) -> Vec<AgentDomainEvent> {
        let mut out = self.give_up(reason);
        out.extend(self.release());
        out
    }

    fn release(&mut self) -> Vec<AgentDomainEvent> {
        self.holding = None;
        std::mem::take(&mut self.held)
    }
}

fn dropped(client_message_id: String, reason: &str) -> AgentDomainEvent {
    AgentDomainEvent::UserMessageDropped {
        client_message_id,
        reason: reason.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{TurnEndDetail, TurnOutcome};

    type Gate = SteerGate<&'static str>;

    fn completed(turn: &str) -> AgentDomainEvent {
        AgentDomainEvent::TurnCompleted {
            turn_id: turn.into(),
            outcome: TurnOutcome::Completed,
            result_text: String::new(),
            stop_reason: None,
            usage: None,
            detail: TurnEndDetail::default(),
        }
    }

    fn started(turn: &str) -> AgentDomainEvent {
        AgentDomainEvent::TurnStarted { turn_id: turn.into() }
    }

    fn text(turn: &str, words: &str) -> AgentDomainEvent {
        AgentDomainEvent::ContentDelta {
            turn_id: turn.into(),
            kind: crate::ContentKind::Text,
            text: words.into(),
        }
    }

    /// What the translator makes of the wire's delivery: no text, which the gate fills in.
    fn delivered(id: &str, turn: &str) -> AgentDomainEvent {
        AgentDomainEvent::UserMessageDelivered {
            client_message_id: id.into(),
            turn_id: turn.into(),
            text: String::new(),
        }
    }

    fn delivered_as(id: &str, turn: &str, typed: &str) -> AgentDomainEvent {
        AgentDomainEvent::UserMessageDelivered {
            client_message_id: id.into(),
            turn_id: turn.into(),
            text: typed.into(),
        }
    }

    fn drop_of(id: &str, reason: &str) -> AgentDomainEvent {
        dropped(id.into(), reason)
    }

    /// A gate with one message sent and answered.
    fn one_sent() -> Gate {
        let mut gate = Gate::default();
        assert_eq!(gate.open("m1", "typed one".into(), "req1").unwrap(), Some("req1"));
        assert!(gate.accepted("m1").is_none());
        gate
    }

    #[test]
    fn a_delivery_carries_the_typed_text_and_closes_the_message() {
        let mut gate = one_sent();
        let passed = gate.pass(delivered("m1", "t1"));
        assert_eq!(passed.ready, [delivered_as("m1", "t1", "typed one")]);
        // Nothing open: the turn's end goes straight through.
        let passed = gate.pass(completed("t1"));
        assert_eq!(passed.ready, [completed("t1")]);
        assert_eq!(passed.hold_started, None);
    }

    #[test]
    fn a_delivery_for_a_message_never_sent_is_not_folded() {
        let mut gate = Gate::default();
        assert!(gate.pass(delivered("stranger", "t1")).ready.is_empty());
    }

    /// The case the hold exists for: a turn ends with a message still to run, and the sidecar opens a
    /// follow-up turn for it. The end and the follow-up's start leave in ONE batch, so the host never
    /// sees "no turn running" with the message unreported.
    #[test]
    fn a_turn_end_with_a_message_open_waits_for_the_follow_up_and_leaves_with_it() {
        let mut gate = one_sent();
        let passed = gate.pass(completed("t1"));
        assert!(passed.ready.is_empty(), "the end is held");
        let generation = passed.hold_started.expect("a hold began");
        assert_eq!(gate.held_len(), 1);

        let passed = gate.pass(started("t2"));
        assert_eq!(
            passed.ready,
            [completed("t1"), started("t2")],
            "released together, in order"
        );
        assert_eq!(passed.hold_started, None);
        // The follow-up runs the message.
        assert_eq!(gate.pass(delivered("m1", "t2")).ready.len(), 1);
        assert_eq!(gate.pass(text("t2", "ok")).ready, [text("t2", "ok")]);
        assert_eq!(
            gate.pass(completed("t2")).ready,
            [completed("t2")],
            "nothing open, no hold"
        );
        // The timer armed for the first hold finds nothing to do.
        assert!(gate.expire(generation).is_empty());
    }

    #[test]
    fn whatever_arrives_between_the_end_and_the_follow_up_stays_in_order() {
        let mut gate = one_sent();
        gate.pass(completed("t1"));
        let commands = AgentDomainEvent::CommandsAvailable { commands: Vec::new() };
        assert!(gate.pass(commands.clone()).ready.is_empty());
        let passed = gate.pass(started("t2"));
        assert_eq!(passed.ready, [completed("t1"), commands, started("t2")]);
    }

    #[test]
    fn the_wait_running_out_drops_the_open_messages_before_the_held_end() {
        let mut gate = one_sent();
        assert_eq!(gate.open("m2", "typed two".into(), "req2").unwrap(), Some("req2"));
        gate.accepted("m2");
        let generation = gate.pass(completed("t1")).hold_started.unwrap();
        assert_eq!(
            gate.expire(generation),
            [drop_of("m1", UNREPORTED), drop_of("m2", UNREPORTED), completed("t1")]
        );
        // Nothing open and nothing held afterwards: a late follow-up start passes as it is.
        assert_eq!(gate.pass(started("t2")).ready, [started("t2")]);
        assert!(gate.expire(generation).is_empty(), "a hold expires once");
    }

    /// The stream was slow, not broken: the wait ran out and the message was reported dropped, then the
    /// follow-up and the delivery came after all. The delivery is passed on with the typed text, so the
    /// host can take back the message it held again instead of sending it a second time.
    #[test]
    fn a_delivery_after_the_wait_ran_out_is_still_passed_on() {
        let mut gate = one_sent();
        let generation = gate.pass(completed("t1")).hold_started.unwrap();
        assert_eq!(gate.expire(generation), [drop_of("m1", UNREPORTED), completed("t1")]);
        assert_eq!(gate.pass(started("t2")).ready, [started("t2")]);
        assert_eq!(
            gate.pass(delivered("m1", "t2")).ready,
            [delivered_as("m1", "t2", "typed one")]
        );
        // Once.
        assert!(gate.pass(delivered("m1", "t2")).ready.is_empty());
    }

    /// The stream broke between the turn's end and the follow-up's start, and re-opening it took longer
    /// than the wait. Giving up meanwhile would have released the end with the message reported
    /// dropped, and the host would have sent it as a turn of its own while the replay still carried its
    /// delivery. The wait stops while the stream is down and starts again when it is back, and the
    /// replay releases the end with the follow-up.
    #[test]
    fn the_wait_does_not_run_out_while_the_stream_is_down() {
        let mut gate = one_sent();
        let armed = gate.pass(completed("t1")).hold_started.unwrap();
        gate.stream_lost();
        assert!(gate.expire(armed).is_empty(), "the stream is down: nothing is given up");
        assert_eq!(gate.held_len(), 1);
        let rearmed = gate.stream_back().expect("the hold is still in force");
        assert_ne!(rearmed, armed);
        assert!(gate.expire(armed).is_empty(), "the wait armed before the break is void");
        // The replay brings the follow-up and the delivery.
        assert_eq!(gate.pass(started("t2")).ready, [completed("t1"), started("t2")]);
        assert_eq!(
            gate.pass(delivered("m1", "t2")).ready,
            [delivered_as("m1", "t2", "typed one")]
        );
        assert!(gate.expire(rearmed).is_empty(), "nothing is held any more");
    }

    /// Once the stream is back, a follow-up that still does not come is given up on as before.
    #[test]
    fn the_wait_armed_when_the_stream_is_back_still_runs_out() {
        let mut gate = one_sent();
        gate.pass(completed("t1"));
        gate.stream_lost();
        let rearmed = gate.stream_back().expect("the hold is still in force");
        assert_eq!(gate.expire(rearmed), [drop_of("m1", UNREPORTED), completed("t1")]);
        // A break with no hold in force arms nothing.
        gate.stream_lost();
        assert_eq!(gate.stream_back(), None);
    }

    #[test]
    fn an_old_timer_does_not_end_a_newer_hold() {
        let mut gate = one_sent();
        let first = gate.pass(completed("t1")).hold_started.unwrap();
        gate.pass(started("t2"));
        // The follow-up is plain text too, and the message is still open at its end.
        let second = gate.pass(completed("t2")).hold_started.unwrap();
        assert_ne!(first, second);
        assert!(gate.expire(first).is_empty());
        assert_eq!(gate.held_len(), 1);
        assert_eq!(gate.expire(second), [drop_of("m1", UNREPORTED), completed("t2")]);
    }

    #[test]
    fn the_session_ending_during_a_hold_drops_the_open_messages_before_the_held_end() {
        let mut gate = one_sent();
        gate.pass(completed("t1"));
        let closed = AgentDomainEvent::SessionClosed {
            reason: "closed".into(),
        };
        assert_eq!(
            gate.pass(closed.clone()).ready,
            [drop_of("m1", SESSION_ENDED), completed("t1"), closed]
        );
    }

    #[test]
    fn the_stream_failing_with_a_message_open_drops_it_first() {
        let mut gate = one_sent();
        let lost = AgentDomainEvent::SessionUnavailable { reason: "gone".into() };
        assert_eq!(gate.pass(lost.clone()).ready, [drop_of("m1", SESSION_ENDED), lost]);
    }

    /// The sidecar's own drop arriving during a hold (no follow-up coming) ends it at once, without
    /// waiting out the timer.
    #[test]
    fn the_last_open_message_settled_during_a_hold_releases_it() {
        let mut gate = one_sent();
        gate.pass(completed("t1"));
        let passed = gate.pass(drop_of("m1", "Claude refused it"));
        assert_eq!(passed.ready, [completed("t1"), drop_of("m1", "Claude refused it")]);
    }

    /// A message whose `SteerTurn` was still in flight when the turn ended: the sidecar refuses it
    /// (no turn runs) and opens no follow-up, so the hold ends on the refusal, not on the timer.
    #[test]
    fn a_refusal_of_the_last_open_message_releases_the_hold_with_the_drop_first() {
        let mut gate = Gate::default();
        gate.open("m1", "typed".into(), "req1").unwrap();
        assert!(
            gate.pass(completed("t1")).hold_started.is_some(),
            "in flight counts as open"
        );
        assert_eq!(
            gate.refused("m1", "the turn had ended when it reached Claude"),
            [
                drop_of("m1", "the turn had ended when it reached Claude"),
                completed("t1")
            ]
        );
        assert_eq!(gate.pass(started("t2")).ready, [started("t2")]);
    }

    /// The host is never made to wait for an answer: a later message is taken at once and sent only
    /// when the one before it is answered.
    #[test]
    fn a_later_message_waits_unsent_until_the_earlier_one_is_answered() {
        let mut gate = Gate::default();
        assert_eq!(gate.open("m1", "one".into(), "req1").unwrap(), Some("req1"));
        assert_eq!(gate.open("m2", "two".into(), "req2").unwrap(), None, "waits");
        assert_eq!(gate.open("m3", "three".into(), "req3").unwrap(), None, "waits");
        assert_eq!(gate.accepted("m1"), Some(("m2".to_string(), "req2")));
        assert_eq!(gate.accepted("m2"), Some(("m3".to_string(), "req3")));
        assert_eq!(gate.accepted("m3"), None);
        // With all answered, a new one goes at once.
        assert_eq!(gate.open("m4", "four".into(), "req4").unwrap(), Some("req4"));
    }

    #[test]
    fn a_refusal_drops_the_messages_waiting_behind_it_unsent() {
        let mut gate = Gate::default();
        gate.open("m1", "one".into(), "req1").unwrap();
        gate.open("m2", "two".into(), "req2").unwrap();
        gate.open("m3", "three".into(), "req3").unwrap();
        assert_eq!(
            gate.refused("m1", "why"),
            [
                drop_of("m1", "why"),
                drop_of("m2", BEHIND_A_REFUSAL),
                drop_of("m3", BEHIND_A_REFUSAL)
            ]
        );
        assert_eq!(gate.open("m4", "four".into(), "req4").unwrap(), Some("req4"));
    }

    /// A lost answer: the message may be with the CLI, so it stays open for its outcome; what waits
    /// behind it is dropped unsent, since it might reach the CLI first.
    #[test]
    fn a_lost_answer_keeps_the_message_open_and_drops_what_waits_behind_it() {
        let mut gate = Gate::default();
        gate.open("m1", "one".into(), "req1").unwrap();
        gate.open("m2", "two".into(), "req2").unwrap();
        assert_eq!(gate.unknown("m1"), [drop_of("m2", BEHIND_AN_UNKNOWN)]);
        assert_eq!(
            gate.pass(delivered("m1", "t1")).ready,
            [delivered_as("m1", "t1", "one")]
        );
    }

    #[test]
    fn a_message_never_sent_is_not_remembered_when_the_wait_runs_out() {
        let mut gate = Gate::default();
        gate.open("m1", "one".into(), "req1").unwrap();
        gate.open("m2", "two".into(), "req2").unwrap();
        let generation = gate.pass(completed("t1")).hold_started.unwrap();
        gate.expire(generation);
        assert!(gate.pass(delivered("m2", "t2")).ready.is_empty(), "m2 never left");
        assert_eq!(gate.pass(delivered("m1", "t2")).ready.len(), 1, "m1 was in flight");
    }

    #[test]
    fn an_id_already_open_and_too_many_open_are_refused() {
        let mut gate = one_sent();
        assert!(gate.open("m1", "again".into(), "x").is_err());
        for n in 2..=MAX_OPEN_STEERS {
            let id = format!("m{n}");
            gate.open(&id, String::new(), "x").unwrap();
            gate.accepted(&id);
        }
        assert!(gate.open("one-more", String::new(), "x").is_err());
    }

    #[test]
    fn a_gate_with_a_message_open_or_a_held_end_is_not_handed_over() {
        let mut gate: Gate = SteerGate::default();
        assert_eq!(gate.hand_over(), Ok(Vec::new()));
        assert_eq!(gate.open("a", "typed".into(), "req").unwrap(), Some("req"));
        assert_eq!(gate.hand_over(), Err(SteerBusy::Open), "sent, not answered");
        gate.accepted("a");
        assert_eq!(gate.hand_over(), Err(SteerBusy::Open), "answered, not delivered");
        assert!(gate.pass(completed("t1")).hold_started.is_some());
        assert_eq!(gate.hand_over(), Err(SteerBusy::HoldingTurnEnd));
        gate.pass(started("t2"));
        gate.pass(delivered("a", "t2"));
        assert_eq!(gate.hand_over(), Ok(Vec::new()));
    }

    #[test]
    fn a_message_given_up_on_is_handed_over_and_a_late_delivery_is_honoured_by_the_taker() {
        let mut gate: Gate = SteerGate::default();
        gate.open("a", "typed".into(), "req").unwrap();
        gate.accepted("a");
        let generation = gate.pass(completed("t1")).hold_started.unwrap();
        gate.expire(generation);
        let given_up = gate.hand_over().unwrap();
        assert_eq!(given_up, [("a".to_string(), "typed".to_string())]);

        let mut taker: Gate = SteerGate::taken_over(given_up);
        assert_eq!(
            taker.pass(delivered("a", "t2")).ready,
            [delivered_as("a", "t2", "typed")]
        );
    }
}
