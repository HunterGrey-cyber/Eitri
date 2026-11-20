// agent/src/providers/claude_sidecar/watch.rs
//! Continuity and recovery policy for one session's `WatchSessionEvents` stream.
//!
//! Everything here is pure and I/O-free so the decisions can be unit-tested without a sidecar --
//! the policy is the part that is easy to get wrong, and the part whose failure is invisible.
//!
//! ## The failure this module exists to prevent
//!
//! Before it, the watch loop logged a stream error to stderr and `break`ed. Measured against a real
//! sidecar (SIGKILL mid-reply, 2026-09-12): **zero** events reached the consumer afterwards. The
//! projection kept `active_turn_id = Some(..)` and 307 characters of a truncated assistant reply,
//! its status still `Running`. A UI rendering that shows an incomplete answer that looks finished,
//! with a spinner that never stops and no error anywhere. That is the one outcome this backend must
//! never produce -- a gap must be loud, or it is silent corruption.
//!
//! ## What the wire actually guarantees (read from the sidecar's own source, not assumed)
//!
//! - `SessionEntry.sequence` starts at 0 and is incremented by exactly 1 per event, before the
//!   event is pushed to the ring buffer and broadcast. Sequences are therefore dense: the client can
//!   tell "I missed something" from arithmetic alone.
//! - `RingBuffer` retains the newest 1000 events per session. `replayAfter(n)` reports a gap only
//!   when `n > 0 && n < oldest_retained - 1`. With partial streaming measured at ~403 events for one
//!   800-word reply, **a reconnect roughly 2.5 streamed turns behind the live edge is where replay
//!   stops being able to close the hole.**
//! - `after_sequence: 0` can never report a gap (the `n > 0` guard). Opening a watch at 0 against a
//!   session that has already overflowed silently skips the evicted prefix. This client only ever
//!   sends 0 for a session it created moments earlier, whose buffer is provably empty; every
//!   reconnect sends the real last-seen sequence precisely so a gap is reported rather than hidden.
//! - When a session terminates, the sidecar broadcasts `SessionClosed` and *then* gRPC-completes
//!   every subscriber's call. A clean end preceded by `SessionClosed` is normal. A clean end without
//!   one is not: `PumpDriver` reaches exactly that state when `pump()` itself throws, tearing the
//!   session down with no terminal event to broadcast.
//!
//! ## On not inventing provider events
//!
//! The loss signal raised here is `AgentDomainEvent::SessionUnavailable`, which is a statement about
//! *this client's ability to observe the session* -- the same thing the legacy backend already
//! reports when its child process dies. It is deliberately not a synthesized `TurnCompleted`,
//! `TurnStarted`, or `PermissionResolved`: those would claim the provider did something it never
//! did. Reporting "I can no longer see this session" is the honest alternative to guessing at the
//! events that went missing.

use crate::provider::{InteractionState, ProviderError, ProviderErrorCode, RemoteLinkState};
use crate::AgentDomainEvent;
use std::collections::VecDeque;
use std::time::Duration;

/// How long to wait before each reconnect attempt. Three tries inside ~1 second: long enough to ride
/// out a transient transport hiccup, short enough that a genuinely dead sidecar becomes a visible
/// error while the user is still looking at the reply it truncated.
pub(crate) const WATCH_RECONNECT_BACKOFF: [Duration; 3] = [
    Duration::from_millis(100),
    Duration::from_millis(250),
    Duration::from_millis(600),
];

/// How long a remote tab waits before each attempt to reach its session again: rising to a minute and
/// staying there. There is no last attempt. The agent lives on the server whatever the laptop's link
/// does, so only the helper's own word (the session or its sidecar is gone, or the journal has a gap)
/// ends the tab, or the person closing it.
pub use crate::remote_client::REMOTE_RECONNECT_BACKOFF;

/// The helper's refusal of a call naming a sidecar it does not run (any more, after a reboot).
pub(crate) const NO_SUCH_SIDECAR: &str = "no such sidecar on this server";

/// Why one attempt at a session's stream ended without the session closing itself.
#[derive(Debug)]
pub(crate) enum WatchFailure {
    /// Opening the stream failed with this status.
    Open(tonic::Status),
    /// The open stream failed with this status.
    Broke(tonic::Status),
    /// The stream ended cleanly with no `SessionClosed` before it.
    EndedEarly,
}

/// What a local tab does after a failed attempt: today's rule, three quick retries and then the loss.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LocalWatchVerdict {
    Retry {
        after: Duration,
    },
    /// The stream is lost; the detail goes into [`stream_ended_early_reason`].
    End(String),
}

/// A local tab's verdict after `consecutive_failures` failed attempts in a row. A typed refusal to
/// open ends at once (`classify_open_failure`); any other failure is retried along
/// [`WATCH_RECONNECT_BACKOFF`] and ends when it runs out.
pub(crate) fn local_watch_verdict(failure: &WatchFailure, consecutive_failures: usize) -> LocalWatchVerdict {
    let detail = match failure {
        WatchFailure::Open(status) => match classify_open_failure(&super::provider_error_of(status)) {
            OpenFailure::Fatal(detail) => return LocalWatchVerdict::End(detail),
            OpenFailure::Retry(detail) => detail,
        },
        WatchFailure::Broke(status) => status.to_string(),
        WatchFailure::EndedEarly => "the provider closed the event stream".to_string(),
    };
    match WATCH_RECONNECT_BACKOFF.get(consecutive_failures) {
        Some(after) => LocalWatchVerdict::Retry { after: *after },
        None => LocalWatchVerdict::End(detail),
    }
}

/// What a remote tab does after a failed attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RemoteWatchVerdict {
    /// Try again after this wait; the attempt counter goes on.
    Retry { after: Duration },
    /// The helper said the session cannot be followed any more; the detail goes into
    /// [`stream_ended_early_reason`].
    Gone(String),
}

/// A remote tab's verdict after its `attempt`-th failed attempt in a row (counting from 0). Only a
/// definite answer from the helper ends the tab: `NOT_FOUND` for the session (the runtime's
/// `SESSION_NOT_FOUND`) or for its sidecar ([`NO_SUCH_SIDECAR`]), or `OUT_OF_RANGE` with `EVENT_GAP`.
/// Everything else -- a broken channel, `UNAVAILABLE`, a timeout, a dead ssh master, any other refusal
/// -- is retried along [`REMOTE_RECONNECT_BACKOFF`], without end.
pub(crate) fn remote_watch_verdict(failure: &WatchFailure, attempt: u32) -> RemoteWatchVerdict {
    if let WatchFailure::Open(status) | WatchFailure::Broke(status) = failure {
        let error = super::provider_error_of(status);
        let code = |wanted| matches!(&error, ProviderError::Provider { code, .. } if *code == wanted);
        // The helper's own words follow, when it gave any: they name what it could not follow.
        let said = |what: &str| match status.message() {
            "" => what.to_string(),
            words => format!("{what}: {words}"),
        };
        if terminal_observation(status) {
            return RemoteWatchVerdict::Gone(said("the helper cannot continue observing this session"));
        }
        match status.code() {
            tonic::Code::NotFound if code(ProviderErrorCode::SessionNotFound) => {
                return RemoteWatchVerdict::Gone(said("the server no longer has this session"));
            }
            tonic::Code::NotFound if status.message() == NO_SUCH_SIDECAR => {
                return RemoteWatchVerdict::Gone(format!(
                    "the server no longer runs this tab's agent ({NO_SUCH_SIDECAR})"
                ));
            }
            tonic::Code::OutOfRange if code(ProviderErrorCode::EventGap) => {
                return RemoteWatchVerdict::Gone(said(
                    "the server's journal no longer holds the events that were missed",
                ));
            }
            _ => {}
        }
    }
    let at = (attempt as usize).min(REMOTE_RECONNECT_BACKOFF.len() - 1);
    RemoteWatchVerdict::Retry {
        after: REMOTE_RECONNECT_BACKOFF[at],
    }
}

fn terminal_observation(status: &tonic::Status) -> bool {
    use prost::Message;
    claude_runtime_protocol::v1::ErrorDetail::decode(status.details())
        .is_ok_and(|detail| detail.reason == "session_observation_ended")
}

/// Whether a failure [`remote_watch_verdict`] calls gone is the journal's gap (`OUT_OF_RANGE` with
/// `EVENT_GAP`), after which the agent may still run on the server, rather than the agent itself gone.
pub(crate) fn remote_gone_is_a_gap(failure: &WatchFailure) -> bool {
    match failure {
        WatchFailure::Open(status) | WatchFailure::Broke(status) => {
            terminal_observation(status)
                || (status.code() == tonic::Code::OutOfRange
                    && matches!(
                        super::provider_error_of(status),
                        ProviderError::Provider {
                            code: ProviderErrorCode::EventGap,
                            ..
                        }
                    ))
        }
        WatchFailure::EndedEarly => false,
    }
}

/// One line saying what a failed attempt was, for the log.
pub(crate) fn failure_text(failure: &WatchFailure) -> String {
    match failure {
        WatchFailure::Open(status) => format!("opening the stream: {:?}: {}", status.code(), status.message()),
        WatchFailure::Broke(status) => format!("the stream broke: {:?}: {}", status.code(), status.message()),
        WatchFailure::EndedEarly => "the stream ended without the session's close".to_string(),
    }
}

/// Where a remote tab's connection stands, read by the panel at its own pace. Only the watch writes it.
#[derive(Debug)]
pub(crate) struct LinkCell(std::sync::Mutex<RemoteLinkState>);

impl Default for LinkCell {
    fn default() -> Self {
        Self(std::sync::Mutex::new(RemoteLinkState::Live))
    }
}

impl LinkCell {
    pub(crate) fn get(&self) -> RemoteLinkState {
        self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone()
    }

    pub(crate) fn set(&self, state: RemoteLinkState) {
        *self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = state;
    }
}

/// A replay in progress after a watch opened: the journal held `head` when the watch opened after
/// `from`. `None` when there is nothing to catch up on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CatchUp {
    from: u64,
    head: u64,
}

impl CatchUp {
    pub(crate) fn opened(from: u64, head: Option<u64>) -> Option<Self> {
        head.filter(|head| *head > from).map(|head| Self { from, head })
    }

    /// The link state once the event at `sequence` was delivered, and whether the replay is over.
    pub(crate) fn at(&self, sequence: u64) -> (RemoteLinkState, bool) {
        if sequence >= self.head {
            return (RemoteLinkState::Live, true);
        }
        (
            RemoteLinkState::CatchingUp {
                done: sequence.saturating_sub(self.from),
                total: self.head - self.from,
            },
            false,
        )
    }

    pub(crate) fn start(&self) -> RemoteLinkState {
        RemoteLinkState::CatchingUp {
            done: 0,
            total: self.head - self.from,
        }
    }
}

/// What to do with one event, judged only by the sequence it carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SequenceVerdict {
    /// Exactly the next expected sequence. Translate and deliver it.
    Deliver,
    /// Already delivered. A reconnect replays from `after_sequence`, and the boundary is inclusive
    /// enough that the same event can legitimately arrive twice; dropping it is correct and silent.
    ///
    /// The ONLY silent path here, and safe for one specific reason: `last_delivered` can only ever
    /// have come from this same server's own sequence counter, so it cannot run ahead of what the
    /// server has assigned and mistake fresh events for replays. (Verdandi's claude ring accepts an
    /// impossible `after_sequence` -- one beyond anything ever assigned -- without complaint, per
    /// verdandi-37's own reading of the terminal sidecar's mirror of this code, so the client cannot
    /// lean on the server to catch it. Nothing here ever invents a cursor, which is what keeps that
    /// laxness out of reach.)
    Duplicate,
    /// Events between the last delivered one and this one were skipped, and replay will never bring
    /// them back. The assistant text this client holds is missing a piece in the middle.
    Lost { first: u64, last: u64 },
    /// Sequence 0 is reserved by the protocol for "nothing emitted yet" and must never appear on an
    /// event. Treated as loud rather than dropped: a silently ignored event is the exact failure
    /// this module exists to prevent, so an impossible sequence fails toward visible.
    Invalid,
}

/// Tracks one session's wire sequence across the whole watch, reconnects included.
#[derive(Debug, Default)]
pub(crate) struct SequenceTracker {
    last_delivered: u64,
}

impl SequenceTracker {
    pub(crate) fn new() -> Self {
        Self::after(0)
    }

    /// A tracker that goes on from `sequence`, the last event another watch of the same session
    /// delivered and its holder folded: the first event expected is the one after it.
    pub(crate) fn after(sequence: u64) -> Self {
        Self {
            last_delivered: sequence,
        }
    }

    /// The cursor to ask for, on a first open and on every reconnect alike.
    ///
    /// The server replays everything strictly greater than this, so resuming from the last
    /// *delivered* sequence -- not the last *seen* -- is what makes a hole impossible to paper over.
    ///
    /// On a first open this is 0, which the protocol now treats as an ordinary cursor meaning "I
    /// have seen nothing", checked for eviction like any other. That is the whole reason this client
    /// uses `AFTER_SEQUENCE` for every watch it ever opens, including the first, rather than
    /// `AVAILABLE_HISTORY`: the other mode cannot report a gap, because a caller choosing it has
    /// declared that a missing prefix is acceptable. It is not acceptable here. `FROM_NOW` is
    /// likewise never used -- a conversation that silently begins mid-history is the same defect
    /// wearing a different name.
    ///
    /// PROVENANCE. This value only ever comes from a sequence this stream delivered. It is never
    /// persisted, never carried across sessions, and never reconstructed -- see the proto's note on
    /// `after_sequence`. The server now refuses a cursor beyond anything it has assigned, so a
    /// future violation of that rule fails loudly instead of landing in `Duplicate` below.
    pub(crate) fn after_sequence(&self) -> u64 {
        self.last_delivered
    }

    pub(crate) fn observe(&mut self, sequence: u64) -> SequenceVerdict {
        if sequence == 0 {
            return SequenceVerdict::Invalid;
        }
        if sequence <= self.last_delivered {
            return SequenceVerdict::Duplicate;
        }
        if sequence > self.last_delivered + 1 {
            return SequenceVerdict::Lost {
                first: self.last_delivered + 1,
                last: sequence - 1,
            };
        }
        self.last_delivered = sequence;
        SequenceVerdict::Deliver
    }
}

/// Whether a failure to OPEN the watch stream is worth another attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum OpenFailure {
    /// Transport-level: the sidecar may still be there. Back off and try again.
    Retry(String),
    /// The server answered with a typed refusal that another identical attempt cannot change.
    Fatal(String),
}

/// Classifies a watch-open error. The two fatal codes are fatal for different reasons, and both
/// matter: `EVENT_GAP` means the events are already evicted (retrying replays the same refusal),
/// and `SESSION_NOT_FOUND` means the sidecar has already evicted the session itself.
pub(crate) fn classify_open_failure(error: &ProviderError) -> OpenFailure {
    match error {
        ProviderError::Provider {
            code: ProviderErrorCode::EventGap,
            ..
        } => OpenFailure::Fatal("the provider's replay buffer no longer holds the events that were missed".to_string()),
        ProviderError::Provider {
            code: ProviderErrorCode::SessionNotFound,
            ..
        } => OpenFailure::Fatal("the provider no longer has this session".to_string()),
        // The server refusing our cursor as impossible means this client sent one it could not have
        // received -- a bug on this side, not a transport problem, and retrying sends it again.
        // Surfaced with the provider's own words because they name the cursor and the latest
        // sequence, which is what a reader needs to see that the two are unrelated.
        ProviderError::Provider {
            code: ProviderErrorCode::InvalidConfiguration,
            message,
        } => OpenFailure::Fatal(format!("the provider refused this client's replay cursor: {message}")),
        // Any other typed refusal is also unanswerable by a retry, but says something this client
        // did not anticipate -- carry its own words rather than paraphrasing them.
        ProviderError::Provider { code, message } => OpenFailure::Fatal(format!(
            "the provider refused to stream this session's events ({code:?}): {message}"
        )),
        ProviderError::UnsupportedCapability(what) => {
            OpenFailure::Fatal(format!("the provider cannot stream this session's events: {what}"))
        }
        ProviderError::Timeout => OpenFailure::Retry("timed out opening the event stream".to_string()),
        ProviderError::Transport(message) => OpenFailure::Retry(message.clone()),
    }
}

/// The user-facing reason for a stream that stopped before its session did -- the single wording
/// every abnormal exit goes through, so no failure path can accidentally be quieter than another.
///
/// Deliberately says what it costs the reader ("anything after this point never arrived") rather
/// than only naming the transport fault, because the whole point of raising it is that the text
/// above it cannot be trusted to be complete.
pub(crate) fn stream_ended_early_reason(detail: &str) -> String {
    format!(
        "the connection to the provider ended before this session did, so anything after this point \
         never arrived and the reply above may be incomplete ({detail})"
    )
}

/// The user-facing reason for events that were skipped mid-stream.
pub(crate) fn events_lost_reason(first: u64, last: u64) -> String {
    let count = last - first + 1;
    format!(
        "{count} event(s) from the provider (sequence {first}-{last}) were never delivered, so the \
         reply above is missing a piece in the middle and cannot be trusted as complete"
    )
}

/// How many question ids one watch remembers. A session holds at most four questions waiting at once,
/// so this reaches back far past any question that could still be waiting.
const QUESTION_IDS_KEPT: usize = 256;

/// The question sets and plans one watch has delivered an ask or a settlement for, newest last, and the
/// asks a reconcile holds back until the replay reaches the sequence its list was read at. A plan is an
/// interaction like a question, listed beside them by `GetInteractionState`, and is handled the same way.
#[derive(Debug, Default)]
pub(crate) struct QuestionLedger {
    seen: VecDeque<String>,
    deferred: Option<DeferredAsks>,
}

/// Asks from the sidecar's list of waiting questions, read when the watch had delivered less than the
/// list is consistent with: they belong after event `through`, where the sidecar's own replay puts
/// them, not ahead of the replay that comes first.
#[derive(Debug)]
struct DeferredAsks {
    through: u64,
    asks: Vec<AgentDomainEvent>,
}

impl QuestionLedger {
    /// Notes the question an event this watch delivers asks or settles, a subagent's included.
    pub(crate) fn observe(&mut self, event: &AgentDomainEvent) {
        match event {
            AgentDomainEvent::QuestionAsked { interaction_id, .. }
            | AgentDomainEvent::QuestionSettled { interaction_id, .. }
            | AgentDomainEvent::PlanProposed { interaction_id, .. }
            | AgentDomainEvent::PlanSettled { interaction_id, .. } => self.note(interaction_id),
            AgentDomainEvent::SubagentFrame { frame, .. } => self.observe(frame),
            _ => {}
        }
    }

    fn note(&mut self, interaction_id: &str) {
        if self.knows(interaction_id) {
            return;
        }
        if self.seen.len() == QUESTION_IDS_KEPT {
            self.seen.pop_front();
        }
        self.seen.push_back(interaction_id.to_string());
    }

    fn knows(&self, interaction_id: &str) -> bool {
        self.seen.iter().any(|seen| seen == interaction_id)
    }

    /// The asks a reconcile held back for the event at `sequence` the watch just delivered: due once
    /// the replay reached the sequence their list was read at, minus every question the replay asked
    /// or settled on the way there.
    pub(crate) fn due(&mut self, sequence: u64) -> Vec<AgentDomainEvent> {
        match &self.deferred {
            Some(deferred) if deferred.through <= sequence => {}
            _ => return Vec::new(),
        }
        let deferred = self.deferred.take().expect("checked just above");
        self.unseen(deferred.asks)
    }

    /// Of `asks`, the ones this watch never delivered an ask or a settlement for, noted as delivered.
    fn unseen(&mut self, asks: Vec<AgentDomainEvent>) -> Vec<AgentDomainEvent> {
        let mut added = Vec::new();
        for event in asks {
            let (AgentDomainEvent::QuestionAsked { interaction_id, .. }
            | AgentDomainEvent::PlanProposed { interaction_id, .. }) = &event
            else {
                continue;
            };
            if interaction_id.is_empty() || self.knows(interaction_id) {
                continue;
            }
            self.observe(&event);
            added.push(event);
        }
        added
    }
}

/// What a re-opened watch adds from `GetInteractionState`, read when the watch had delivered every
/// event through `delivered_through`: the ask of every question still waiting that this watch never
/// delivered an ask or a settlement for, in the sidecar's order. Everything else the stream itself
/// carries -- the replay after the cursor brings every ask and settlement the break hid, and a hole it
/// cannot replay ends the session loudly -- so a question this watch saw is never asked again, and one
/// the sidecar no longer holds is never settled here: that would claim an outcome nobody reported. The
/// same ask arriving later in the stream replaces the record with itself.
///
/// The list is consistent with the sidecar's events through its `through_sequence`. When the watch has
/// delivered less than that, the asks wait for the replay to get there (`QuestionLedger::due`): put
/// ahead of the replay, an ask would end an assistant message the replay is still carrying and split
/// one reply in two. Nothing is returned then.
pub(crate) fn reconcile_questions(
    ledger: &mut QuestionLedger,
    state: InteractionState,
    delivered_through: u64,
) -> Vec<AgentDomainEvent> {
    let InteractionState {
        through_sequence,
        pending,
    } = state;
    // A newer list replaces one still held back: it is consistent with at least as much.
    ledger.deferred = None;
    if through_sequence > delivered_through {
        ledger.deferred = Some(DeferredAsks {
            through: through_sequence,
            asks: pending,
        });
        return Vec::new();
    }
    ledger.unseen(pending)
}

/// What a remote tab's watch says about a permission request the CLI gave up on while no window was
/// attached. The CLI denies a request nobody answers once its hook's time runs out (ten minutes) and
/// reports it expired; a remote tab learns of that only when it reattaches, from the replay. So an
/// expiry the caller judged unattended (sent as expired, read while catching up, past what a window had
/// folded while attached) is shown as the CLI's denial, with how long the request had waited -- but only
/// when that wait is the CLI's own time: the person's interrupt cancels a waiting request the same way,
/// at any moment, and an expiry of a request this watch never saw asked cannot be told from one.
#[derive(Debug, Default)]
pub(crate) struct UnattendedExpiry {
    /// When each request this watch saw was asked, on the events' own clock (milliseconds), at most
    /// [`ASKED_KEPT`] of them: the events come from the server, and a request never resolved stays here.
    asked_at: std::collections::HashMap<String, (i64, Option<String>)>,
    /// The keys of `asked_at`, oldest first.
    asked_order: VecDeque<String>,
}

/// How many asked requests one watch remembers. A session holds few requests waiting at once, so this
/// reaches far past any that could still be waiting; past it the oldest is forgotten, which costs only
/// the wait in the line an expiry of that request would have shown.
const ASKED_KEPT: usize = 256;

/// The shortest wait after which an expiry is the CLI's own: its hook gives a request ten minutes, and the
/// events' clock is the sidecar's.
pub(crate) const CLI_PERMISSION_TIMEOUT_FLOOR_MS: i64 = 590_000;

impl UnattendedExpiry {
    /// Looks at one translated event that arrived stamped `occurred_at`; `unattended` when the caller judged a
    /// resolution among it to be one read in a replay of what no window saw. Returns the row to show after
    /// such an expiry.
    pub(crate) fn observe(
        &mut self,
        event: &AgentDomainEvent,
        occurred_at: i64,
        unattended: bool,
    ) -> Option<AgentDomainEvent> {
        match event {
            AgentDomainEvent::PermissionRequested {
                permission_id,
                tool_use_id,
                ..
            } => {
                // Ids are the server's text: a request with one past an id's length is not remembered,
                // which costs only the wait in its expiry line.
                let plain = |id: &str| crate::remote_answers::checked_id("a permission id", id).is_ok();
                if !plain(permission_id) || !tool_use_id.as_deref().is_none_or(plain) {
                    return None;
                }
                if self
                    .asked_at
                    .insert(permission_id.clone(), (occurred_at, tool_use_id.clone()))
                    .is_none()
                {
                    self.asked_order.push_back(permission_id.clone());
                }
                while self.asked_order.len() > ASKED_KEPT {
                    if let Some(oldest) = self.asked_order.pop_front() {
                        self.asked_at.remove(&oldest);
                    }
                }
                None
            }
            AgentDomainEvent::PermissionResolved { permission_id, outcome } => {
                let asked = self.asked_at.remove(permission_id);
                if asked.is_some() {
                    self.asked_order.retain(|id| id != permission_id);
                }
                if !unattended || *outcome != crate::PermissionOutcome::Expired {
                    return None;
                }
                let (at, call) = asked.filter(|(at, _)| *at > 0)?;
                let wait = occurred_at.checked_sub(at)?;
                if wait < CLI_PERMISSION_TIMEOUT_FLOOR_MS {
                    return None;
                }
                Some(AgentDomainEvent::SessionNotice {
                    kind: crate::SessionNoticeKind::Informational,
                    level: crate::NoticeLevel::Notice,
                    content: Some(format!(
                        "the CLI denied this request after {} while no window was attached",
                        waited(wait as u64)
                    )),
                    scalars: Default::default(),
                    task_id: None,
                    tool_use_id: call,
                    task_status: None,
                })
            }
            _ => None,
        }
    }
}

/// A wait as a person says it: seconds under two minutes, whole minutes after.
fn waited(ms: u64) -> String {
    let seconds = (ms + 500) / 1000;
    if seconds < 120 {
        format!("{seconds} s")
    } else {
        format!("{} min", (seconds + 30) / 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn observation_refusal_ends_retries_without_claiming_the_agent_has_stopped() {
        use prost::Message;
        let status = tonic::Status::with_details(
            tonic::Code::FailedPrecondition,
            "cannot observe",
            claude_runtime_protocol::v1::ErrorDetail {
                reason: "session_observation_ended".into(),
                ..Default::default()
            }
            .encode_to_vec()
            .into(),
        );
        let failure = WatchFailure::Open(status);
        assert!(matches!(
            remote_watch_verdict(&failure, 100),
            RemoteWatchVerdict::Gone(_)
        ));
        assert!(remote_gone_is_a_gap(&failure));
        assert!(matches!(
            remote_watch_verdict(
                &WatchFailure::Open(tonic::Status::unavailable("cannot follow yet")),
                100
            ),
            RemoteWatchVerdict::Retry { .. }
        ));
    }

    fn requested(id: &str) -> AgentDomainEvent {
        AgentDomainEvent::PermissionRequested {
            permission_id: id.into(),
            tool_use_id: Some(format!("tool-{id}")),
            tool_name: "Bash".into(),
            input: serde_json::json!({ "command": "true" }),
            provider_prompt: None,
        }
    }

    fn resolved(id: &str, outcome: crate::PermissionOutcome) -> AgentDomainEvent {
        AgentDomainEvent::PermissionResolved {
            permission_id: id.into(),
            outcome,
        }
    }

    /// An expiry judged unattended is the CLI's denial, with how long the request waited; one seen live, any
    /// other outcome, one sooner than the CLI's own time (the person's interrupt cancels a request the same way)
    /// and one of a request never seen asked say nothing more than they do today.
    #[test]
    fn an_unattended_expiry_is_the_clis_denial_with_its_wait() {
        let mut expiry = UnattendedExpiry::default();
        assert_eq!(expiry.observe(&requested("p1"), 1_000, true), None);
        let note = expiry.observe(
            &resolved("p1", crate::PermissionOutcome::Expired),
            1_000 + 598_800,
            true,
        );
        let Some(AgentDomainEvent::SessionNotice {
            content, tool_use_id, ..
        }) = note
        else {
            panic!("{note:?}");
        };
        assert_eq!(
            content.as_deref(),
            Some("the CLI denied this request after 10 min while no window was attached")
        );
        assert_eq!(tool_use_id.as_deref(), Some("tool-p1"));

        expiry.observe(&requested("p2"), 5_000, false);
        assert_eq!(
            expiry.observe(&resolved("p2", crate::PermissionOutcome::Expired), 605_000, false),
            None,
            "an expiry seen live was on screen"
        );
        expiry.observe(&requested("p3"), 5_000, true);
        assert_eq!(
            expiry.observe(&resolved("p3", crate::PermissionOutcome::Denied), 700_000, true),
            None
        );
        expiry.observe(&requested("p5"), 5_000, true);
        assert_eq!(
            expiry.observe(&resolved("p5", crate::PermissionOutcome::Expired), 9_000, true),
            None,
            "four seconds is the person's interrupt, not the CLI's time"
        );
        assert_eq!(
            expiry.observe(&resolved("p4", crate::PermissionOutcome::Expired), 900_000, true),
            None,
            "a request never seen asked cannot be told from an interrupt"
        );
        assert_eq!(waited(42_400), "42 s");
    }

    /// Requests the server never resolves do not grow the watch's memory: past `ASKED_KEPT` the oldest is
    /// forgotten, and a newer one still gets its denial line.
    #[test]
    fn asked_requests_never_resolved_are_kept_to_a_bound() {
        let mut expiry = UnattendedExpiry::default();
        for n in 0..ASKED_KEPT * 4 {
            expiry.observe(&requested(&format!("p{n}")), 1_000, true);
        }
        assert_eq!(expiry.asked_at.len(), ASKED_KEPT);
        assert_eq!(expiry.asked_order.len(), ASKED_KEPT);
        assert_eq!(
            expiry.observe(&resolved("p0", crate::PermissionOutcome::Expired), 700_000, true),
            None,
            "the oldest was forgotten"
        );
        let last = format!("p{}", ASKED_KEPT * 4 - 1);
        assert!(expiry
            .observe(&resolved(&last, crate::PermissionOutcome::Expired), 700_000, true)
            .is_some());
        assert_eq!(expiry.asked_order.len(), ASKED_KEPT - 1);
        let long = "p".repeat(crate::remote_answers::ID_BYTES + 1);
        expiry.observe(&requested(&long), 1_000, true);
        assert!(
            !expiry.asked_at.contains_key(&long),
            "an id past its length is not kept"
        );
    }

    fn asked(id: &str) -> AgentDomainEvent {
        AgentDomainEvent::QuestionAsked {
            interaction_id: id.into(),
            tool_use_id: Some(format!("toolu_{id}")),
            questions: vec![crate::QuestionSpec {
                question: format!("question {id}?"),
                header: "Pick".into(),
                multi_select: false,
                options: ["A", "B"]
                    .into_iter()
                    .map(|label| crate::QuestionOptionSpec {
                        label: label.into(),
                        description: String::new(),
                        preview: None,
                    })
                    .collect(),
            }],
            unsupported_reason: None,
        }
    }

    fn settled(id: &str) -> AgentDomainEvent {
        AgentDomainEvent::QuestionSettled {
            interaction_id: id.into(),
            outcome: crate::QuestionOutcome::Answered,
            answers: Vec::new(),
            reason: None,
        }
    }

    fn waiting_ids(projection: &crate::AgentSessionProjection) -> Vec<String> {
        projection
            .waiting_questions()
            .map(|record| record.interaction_id.clone())
            .collect()
    }

    /// After the stream broke and was opened again, the questions still waiting are the ones that
    /// waited before: the reconcile adds nothing this watch already delivered, and never revives one it
    /// saw settled.
    #[test]
    fn the_reconcile_after_a_re_watch_restores_the_same_pending_set() {
        let mut ledger = QuestionLedger::default();
        let mut projection = crate::AgentSessionProjection::default();
        for event in [asked("q0"), asked("q1"), asked("q2"), settled("q0")] {
            ledger.observe(&event);
            projection.apply(&event);
        }
        let before = waiting_ids(&projection);
        assert_eq!(before, vec!["q1".to_string(), "q2".to_string()]);
        let added = reconcile_questions(
            &mut ledger,
            InteractionState {
                through_sequence: 9,
                pending: vec![asked("q1"), asked("q2")],
            },
            9,
        );
        assert!(
            added.is_empty(),
            "every question still waiting was delivered: {added:?}"
        );
        assert_eq!(waiting_ids(&projection), before);

        // A sidecar that still listed one this watch saw settled does not bring it back.
        assert!(reconcile_questions(
            &mut ledger,
            InteractionState {
                through_sequence: 9,
                pending: vec![asked("q0"), asked("q1"), asked("q2")],
            },
            9,
        )
        .is_empty());
    }

    /// A question this watch never delivered -- asked before a move brought the watch here -- comes back
    /// from the sidecar's list in its order, and folding it again leaves the same set waiting; one
    /// added is not added twice.
    #[test]
    fn a_question_the_watch_never_saw_is_added_from_the_sidecars_list() {
        let mut ledger = QuestionLedger::default();
        let mut projection = crate::AgentSessionProjection::default();
        for event in [asked("q1"), asked("q2")] {
            projection.apply(&event);
        }
        ledger.observe(&asked("q3"));
        projection.apply(&asked("q3"));
        let added = reconcile_questions(
            &mut ledger,
            InteractionState {
                through_sequence: 3,
                pending: vec![asked("q1"), asked("q2"), asked("q3")],
            },
            3,
        );
        assert_eq!(added, vec![asked("q1"), asked("q2")]);
        for event in &added {
            projection.apply(event);
        }
        let mut waiting = waiting_ids(&projection);
        waiting.sort();
        assert_eq!(waiting, vec!["q1".to_string(), "q2".to_string(), "q3".to_string()]);
        assert!(reconcile_questions(
            &mut ledger,
            InteractionState {
                through_sequence: 3,
                pending: vec![asked("q1")],
            },
            3,
        )
        .is_empty());
    }

    /// A list read while the replay still has events before the list's sequence waits for them: the
    /// question is added right after the event it is consistent with, never ahead of the reply text the
    /// replay is still carrying.
    #[test]
    fn a_list_newer_than_the_watch_is_added_where_the_replay_reaches_it() {
        let mut ledger = QuestionLedger::default();
        let added = reconcile_questions(
            &mut ledger,
            InteractionState {
                through_sequence: 8,
                pending: vec![asked("q1")],
            },
            5,
        );
        assert!(added.is_empty(), "nothing goes ahead of the replay: {added:?}");
        for sequence in 6..8 {
            assert!(ledger.due(sequence).is_empty(), "not due before event 8, at {sequence}");
        }
        assert_eq!(ledger.due(8), vec![asked("q1")]);
        assert!(ledger.due(9).is_empty(), "added once");
    }

    /// A question the replay asks or settles on its way to the list's sequence is the replay's: the
    /// held-back ask is dropped, so a settled question is never brought back.
    #[test]
    fn a_held_back_ask_the_replay_carried_is_not_added_again() {
        let mut ledger = QuestionLedger::default();
        assert!(reconcile_questions(
            &mut ledger,
            InteractionState {
                through_sequence: 8,
                pending: vec![asked("q1"), asked("q2")],
            },
            5,
        )
        .is_empty());
        ledger.observe(&asked("q1"));
        ledger.observe(&settled("q1"));
        ledger.observe(&asked("q2"));
        assert!(ledger.due(8).is_empty());
    }

    fn refusal(code: tonic::Code, detail: Option<ProtoCode>, message: &str) -> tonic::Status {
        match detail {
            Some(detail) => {
                let bytes = prost::Message::encode_to_vec(&claude_runtime_protocol::v1::ErrorDetail {
                    code: detail as i32,
                    message: message.to_string(),
                    ..Default::default()
                });
                tonic::Status::with_details(code, message, bytes.into())
            }
            None => tonic::Status::new(code, message),
        }
    }

    use claude_runtime_protocol::v1::ErrorCode as ProtoCode;

    /// The three answers that say the session is gone for good end a remote tab, at any attempt.
    #[test]
    fn a_remote_watch_ends_only_on_the_helpers_definite_answers() {
        let gone = [
            WatchFailure::Open(refusal(tonic::Code::NotFound, Some(ProtoCode::SessionNotFound), "")),
            WatchFailure::Broke(refusal(tonic::Code::NotFound, Some(ProtoCode::SessionNotFound), "")),
            WatchFailure::Open(refusal(tonic::Code::NotFound, None, NO_SUCH_SIDECAR)),
            WatchFailure::Open(refusal(tonic::Code::OutOfRange, Some(ProtoCode::EventGap), "")),
            WatchFailure::Broke(refusal(tonic::Code::OutOfRange, Some(ProtoCode::EventGap), "")),
        ];
        for failure in &gone {
            for attempt in [0, 3, 500] {
                assert!(
                    matches!(remote_watch_verdict(failure, attempt), RemoteWatchVerdict::Gone(_)),
                    "{failure:?} at {attempt}"
                );
            }
        }
    }

    /// Of the definite answers, only the journal's gap leaves the agent possibly running on the server;
    /// the session or sidecar not found means the agent is gone.
    #[test]
    fn only_the_journal_gap_is_a_gap() {
        for failure in [
            WatchFailure::Open(refusal(tonic::Code::OutOfRange, Some(ProtoCode::EventGap), "")),
            WatchFailure::Broke(refusal(tonic::Code::OutOfRange, Some(ProtoCode::EventGap), "")),
        ] {
            assert!(remote_gone_is_a_gap(&failure), "{failure:?}");
        }
        for failure in [
            WatchFailure::Open(refusal(tonic::Code::NotFound, Some(ProtoCode::SessionNotFound), "")),
            WatchFailure::Open(refusal(tonic::Code::NotFound, None, NO_SUCH_SIDECAR)),
            WatchFailure::Open(refusal(tonic::Code::FailedPrecondition, Some(ProtoCode::EventGap), "")),
            WatchFailure::EndedEarly,
        ] {
            assert!(!remote_gone_is_a_gap(&failure), "{failure:?}");
        }
    }

    /// Transport failures, `UNAVAILABLE`, timeouts and every other refusal retry without end, the wait
    /// rising to a minute and staying there.
    #[test]
    fn a_remote_watch_retries_forever_and_settles_at_a_minute() {
        let retried = [
            WatchFailure::Open(tonic::Status::unavailable("transport error")),
            WatchFailure::Broke(tonic::Status::unknown("h2 protocol error: broken pipe")),
            WatchFailure::Open(tonic::Status::deadline_exceeded("timed out")),
            WatchFailure::EndedEarly,
            // A NOT_FOUND that names neither the session nor the sidecar is not the helper's word.
            WatchFailure::Open(refusal(tonic::Code::NotFound, None, "something else")),
            // A gap reported under another code, and a cursor refused as being in the future.
            WatchFailure::Open(refusal(tonic::Code::FailedPrecondition, Some(ProtoCode::EventGap), "")),
            WatchFailure::Open(refusal(
                tonic::Code::InvalidArgument,
                Some(ProtoCode::InvalidConfiguration),
                "replay_cursor_in_future",
            )),
        ];
        for failure in &retried {
            let waits: Vec<Duration> = (0..12)
                .map(|attempt| match remote_watch_verdict(failure, attempt) {
                    RemoteWatchVerdict::Retry { after } => after,
                    other => panic!("{failure:?} at {attempt}: {other:?}"),
                })
                .collect();
            assert_eq!(waits[..8], REMOTE_RECONNECT_BACKOFF, "{failure:?}");
            assert!(
                waits[8..].iter().all(|wait| *wait == Duration::from_secs(60)),
                "{failure:?}"
            );
            assert_eq!(
                remote_watch_verdict(failure, u32::MAX),
                RemoteWatchVerdict::Retry {
                    after: Duration::from_secs(60)
                }
            );
        }
        let total: Duration = (0..200)
            .map(|attempt| match remote_watch_verdict(&retried[0], attempt) {
                RemoteWatchVerdict::Retry { after } => after,
                RemoteWatchVerdict::Gone(why) => panic!("{why}"),
            })
            .sum();
        assert!(
            total > Duration::from_secs(2 * 3600),
            "two hours of failures still retry"
        );
    }

    /// A local tab keeps its rule exactly: three retries 100, 250 and 600 ms apart, then the loss; a typed
    /// refusal to open ends it at once.
    #[test]
    fn a_local_watch_keeps_three_quick_retries_and_then_ends() {
        assert_eq!(
            WATCH_RECONNECT_BACKOFF,
            [
                Duration::from_millis(100),
                Duration::from_millis(250),
                Duration::from_millis(600)
            ]
        );
        for failure in [
            WatchFailure::Open(tonic::Status::unavailable("transport error")),
            WatchFailure::Broke(tonic::Status::unavailable("transport error")),
            WatchFailure::EndedEarly,
        ] {
            for (attempt, wait) in WATCH_RECONNECT_BACKOFF.iter().enumerate() {
                assert_eq!(
                    local_watch_verdict(&failure, attempt),
                    LocalWatchVerdict::Retry { after: *wait }
                );
            }
            assert!(matches!(local_watch_verdict(&failure, 3), LocalWatchVerdict::End(_)));
        }
        assert_eq!(
            local_watch_verdict(&WatchFailure::EndedEarly, 3),
            LocalWatchVerdict::End("the provider closed the event stream".to_string())
        );
        for failure in [
            WatchFailure::Open(refusal(tonic::Code::NotFound, Some(ProtoCode::SessionNotFound), "")),
            WatchFailure::Open(refusal(tonic::Code::OutOfRange, Some(ProtoCode::EventGap), "")),
        ] {
            assert!(matches!(local_watch_verdict(&failure, 0), LocalWatchVerdict::End(_)));
        }
        // The helper's word for a missing sidecar is a transport failure to a local tab, as before.
        assert_eq!(
            local_watch_verdict(
                &WatchFailure::Open(refusal(tonic::Code::NotFound, None, NO_SUCH_SIDECAR)),
                0
            ),
            LocalWatchVerdict::Retry {
                after: Duration::from_millis(100)
            }
        );
    }

    /// A definite answer keeps the helper's own words after the reason.
    #[test]
    fn a_gone_verdict_keeps_the_helpers_words() {
        let gone = |failure| match remote_watch_verdict(&failure, 0) {
            RemoteWatchVerdict::Gone(why) => why,
            other => panic!("{other:?}"),
        };
        let why = gone(WatchFailure::Open(refusal(
            tonic::Code::NotFound,
            Some(ProtoCode::SessionNotFound),
            "the helper cannot follow this session: disk full",
        )));
        assert!(
            why.ends_with(": the helper cannot follow this session: disk full"),
            "{why}"
        );
        let why = gone(WatchFailure::Broke(refusal(
            tonic::Code::OutOfRange,
            Some(ProtoCode::EventGap),
            "journal gap 5..=9: WriteFailed",
        )));
        assert!(why.ends_with(": journal gap 5..=9: WriteFailed"), "{why}");
        assert_eq!(
            gone(WatchFailure::Open(refusal(
                tonic::Code::NotFound,
                Some(ProtoCode::SessionNotFound),
                ""
            ))),
            "the server no longer has this session"
        );
    }

    /// A replay reports how far it has come out of how much the journal held when it opened, and is
    /// live once it reaches that head.
    #[test]
    fn catching_up_counts_against_the_head_and_ends_live() {
        assert_eq!(CatchUp::opened(10, Some(10)), None);
        assert_eq!(CatchUp::opened(10, None), None);
        let catch_up = CatchUp::opened(10, Some(30)).unwrap();
        assert_eq!(catch_up.start(), RemoteLinkState::CatchingUp { done: 0, total: 20 });
        assert_eq!(
            catch_up.at(15),
            (RemoteLinkState::CatchingUp { done: 5, total: 20 }, false)
        );
        assert_eq!(catch_up.at(30), (RemoteLinkState::Live, true));
    }

    /// A plan waits in the sidecar's list beside the questions: one this watch never delivered comes back
    /// from it, and one it saw settled never does.
    #[test]
    fn the_reconcile_takes_plans_as_it_takes_questions() {
        let plan = |id: &str| AgentDomainEvent::PlanProposed {
            interaction_id: id.into(),
            tool_use_id: Some(format!("toolu_{id}")),
            plan: "# Plan".into(),
            plan_sha256: String::new(),
            allowed_prompts: Vec::new(),
            unsupported_reason: None,
        };
        let mut ledger = QuestionLedger::default();
        ledger.observe(&plan("p0"));
        ledger.observe(&AgentDomainEvent::PlanSettled {
            interaction_id: "p0".into(),
            outcome: crate::PlanOutcome::KeptPlanning,
            mode_after: None,
            feedback: Some(String::new()),
            reason: None,
        });
        let added = reconcile_questions(
            &mut ledger,
            InteractionState {
                through_sequence: 4,
                pending: vec![plan("p0"), asked("q1"), plan("p1")],
            },
            4,
        );
        assert_eq!(added, vec![asked("q1"), plan("p1")]);
        assert!(reconcile_questions(
            &mut ledger,
            InteractionState {
                through_sequence: 4,
                pending: vec![plan("p1")],
            },
            4,
        )
        .is_empty());
    }

    #[test]
    fn a_dense_stream_delivers_every_event() {
        let mut tracker = SequenceTracker::new();
        for sequence in 1..=100 {
            assert_eq!(tracker.observe(sequence), SequenceVerdict::Deliver, "at {sequence}");
        }
        assert_eq!(tracker.after_sequence(), 100);
    }

    /// The case the client was previously blind to: it read no sequence at all, so a hole in the
    /// middle of an assistant reply was indistinguishable from a reply that was simply shorter.
    #[test]
    fn a_hole_in_the_middle_is_reported_with_its_exact_extent() {
        let mut tracker = SequenceTracker::new();
        for sequence in 1..=40 {
            assert_eq!(tracker.observe(sequence), SequenceVerdict::Deliver);
        }
        assert_eq!(tracker.observe(98), SequenceVerdict::Lost { first: 41, last: 97 });
        // And the tracker does NOT advance past a hole: nothing may be delivered on the far side of
        // a gap as though the stream were still intact.
        assert_eq!(tracker.after_sequence(), 40);
    }

    #[test]
    fn a_replayed_event_after_a_reconnect_is_dropped_silently() {
        let mut tracker = SequenceTracker::new();
        for sequence in 1..=10 {
            tracker.observe(sequence);
        }
        // A reconnect asks for everything after 10; a server that re-sends 10 itself is harmless.
        assert_eq!(tracker.observe(10), SequenceVerdict::Duplicate);
        assert_eq!(tracker.observe(9), SequenceVerdict::Duplicate);
        assert_eq!(tracker.observe(11), SequenceVerdict::Deliver);
    }

    #[test]
    fn sequence_zero_is_loud_rather_than_ignored() {
        let mut tracker = SequenceTracker::new();
        assert_eq!(tracker.observe(0), SequenceVerdict::Invalid);
    }

    #[test]
    fn the_first_event_of_a_session_is_sequence_one() {
        let mut tracker = SequenceTracker::new();
        assert_eq!(tracker.observe(1), SequenceVerdict::Deliver);
    }

    /// A fresh watch opens at 0, so an initial stream that starts at anything but 1 has already lost
    /// the session's opening events -- including `SessionReady`, without which no session id, model,
    /// or cwd is ever known.
    #[test]
    fn a_stream_that_starts_past_one_is_a_loss_not_a_fresh_start() {
        let mut tracker = SequenceTracker::new();
        assert_eq!(tracker.observe(7), SequenceVerdict::Lost { first: 1, last: 6 });
    }

    #[test]
    fn an_event_gap_refusal_is_fatal_because_a_retry_replays_the_same_refusal() {
        let error = ProviderError::Provider {
            code: ProviderErrorCode::EventGap,
            message: "sequence 12 is no longer in the replay buffer".to_string(),
        };
        assert!(matches!(classify_open_failure(&error), OpenFailure::Fatal(_)));
    }

    #[test]
    fn a_missing_session_is_fatal() {
        let error = ProviderError::Provider {
            code: ProviderErrorCode::SessionNotFound,
            message: "no such session: abc".to_string(),
        };
        assert!(matches!(classify_open_failure(&error), OpenFailure::Fatal(_)));
    }

    #[test]
    fn a_transport_failure_is_worth_retrying() {
        let error = ProviderError::Transport("broken pipe".to_string());
        assert_eq!(
            classify_open_failure(&error),
            OpenFailure::Retry("broken pipe".to_string())
        );
    }

    /// Every reason string is shown to a human in the panel's error banner, so none of them may be
    /// empty and all of them must say what it costs the reader, not just name a fault.
    #[test]
    fn every_reason_says_what_was_lost() {
        let lost = events_lost_reason(41, 97);
        assert!(lost.contains("41-97"), "{lost}");
        assert!(lost.contains("57 event"), "{lost}");
        assert!(lost.contains("incomplete") || lost.contains("missing"), "{lost}");

        let ended = stream_ended_early_reason("broken pipe");
        assert!(ended.contains("broken pipe"), "{ended}");
        assert!(ended.contains("incomplete"), "{ended}");
    }
}
