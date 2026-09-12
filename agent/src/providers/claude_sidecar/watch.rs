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

use crate::provider::{ProviderError, ProviderErrorCode};
use std::time::Duration;

/// How long to wait before each reconnect attempt. Three tries inside ~1 second: long enough to ride
/// out a transient transport hiccup, short enough that a genuinely dead sidecar becomes a visible
/// error while the user is still looking at the reply it truncated.
pub(crate) const WATCH_RECONNECT_BACKOFF: [Duration; 3] =
    [Duration::from_millis(100), Duration::from_millis(250), Duration::from_millis(600)];

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
        Self { last_delivered: 0 }
    }

    /// What a reconnect must ask for. The server replays everything strictly greater than this, so
    /// resuming from the last *delivered* sequence (not the last *seen*) is what makes a hole
    /// impossible to paper over.
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
            return SequenceVerdict::Lost { first: self.last_delivered + 1, last: sequence - 1 };
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
        ProviderError::Provider { code: ProviderErrorCode::EventGap, .. } => OpenFailure::Fatal(
            "the provider's replay buffer no longer holds the events that were missed".to_string(),
        ),
        ProviderError::Provider { code: ProviderErrorCode::SessionNotFound, .. } => {
            OpenFailure::Fatal("the provider no longer has this session".to_string())
        }
        // Any other typed refusal is also unanswerable by a retry, but says something this client
        // did not anticipate -- carry its own words rather than paraphrasing them.
        ProviderError::Provider { code, message } => {
            OpenFailure::Fatal(format!("the provider refused to stream this session's events ({code:?}): {message}"))
        }
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

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(classify_open_failure(&error), OpenFailure::Retry("broken pipe".to_string()));
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
