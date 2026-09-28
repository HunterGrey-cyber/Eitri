//! Where a turn's latency actually goes, from the click to the pixels.
//!
//! Off unless `NEOVIBE_AGENT_TRACE=1`. One line per turn, to stderr.
//!
//! The point is to tell "the model is slow" apart from "our pipeline is slow", which the numbers in
//! `agent/tests/claude_sidecar_partial_streaming.rs` cannot do: those are measured at the provider
//! boundary, so everything after it -- the 33ms pump, the bridge hop, the WebView's own render --
//! is invisible to them. Partial streaming cut time-to-first-text from ~29s to ~11s, and the next
//! honest question is how much of the remainder this side is responsible for.
//!
//! The five marks:
//!
//! | mark | stamped where | means |
//! | --- | --- | --- |
//! | submitted | the `send_message` inbound message is handled | the user pressed Send |
//! | first provider event | the pump drains any event of this turn | the provider said anything at all |
//! | first presentation delta | the pump drains the first assistant text | the first characters exist here |
//! | first paint frame | reported back by the WebView | the first characters are on screen |
//! | completed | the pump drains `TurnCompleted` | the turn is over |
//!
//! **What these numbers do NOT bound.** They measure a request/response route: a turn goes out, a
//! reply streams back, and the span from "text exists in Rust" to "frame drawn" is ~20-26ms against
//! a 3-6s wait. That is a statement about LATENCY on this shape of traffic, and nothing else. It is
//! not evidence that transport is cheap in general, and specifically not for a continuous stream
//! that pushes whether or not anyone asked for anything -- there the question is whether the pipe
//! keeps up, which is throughput, which nothing here measures. (Raised by the terminal-runtime track,
//! which is measuring exactly that for its own route; the tempting bad inference is "the SDK route
//! proved transport is not the bottleneck, so it is not one anywhere".)
//!
//! The one transport property this route did establish is a correctness one, not a speed one: a
//! consumer that stalls for 20 seconds loses nothing, because grpc-js queues server-side rather than
//! dropping -- see `agent/MANUAL_VERIFICATION.md`'s 2026-09-12 backpressure section.
//!
//! **On "first paint frame", and what it is not.** The WebView reports how long it took from
//! *receiving* the payload to the animation frame that drew it, and that span is added to the
//! moment Rust dispatched it. Measured as a span rather than a wall-clock instant on purpose: JS
//! `performance.now()` and Rust `Instant` have unrelated epochs, so subtracting one from the other
//! would produce a confident, meaningless number. It is also a frame, not a photon -- the callback
//! runs before the compositor presents -- so read it as the frame the text was drawn into, and as a
//! floor on what the user perceives, never as a measured perceptual latency.

use agent::{AgentDomainEvent, ContentKind};
use std::time::{Duration, Instant};

/// True when `NEOVIBE_AGENT_TRACE=1`. Read once per turn rather than cached in a `static`, so it can
/// be flipped between runs of a long-lived process without a restart; a turn is far too coarse for
/// one `std::env::var` to matter.
fn enabled() -> bool {
    matches!(std::env::var("NEOVIBE_AGENT_TRACE").as_deref(), Ok("1"))
}

fn ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

/// One turn's marks. Created when the turn is submitted, emitted when it ends.
#[derive(Debug)]
pub struct TurnTrace {
    submitted_at: Instant,
    /// The provider's own id for this turn, learned from `TurnStarted`. Not known at submit time:
    /// `send_turn` returns one for the sidecar but not for the legacy backend, and this file must
    /// not care which backend it is looking at.
    turn_id: Option<String>,
    first_provider_event: Option<Duration>,
    first_presentation_delta: Option<Duration>,
    /// Dispatch time of the payload carrying the first assistant text, plus the WebView's own
    /// receive-to-frame span. `None` until the WebView reports, which it does exactly once per turn.
    first_paint_frame: Option<Duration>,
    /// When the payload carrying the first assistant text was handed to the WebView. Kept so the
    /// reported span can be added to it later.
    first_text_dispatched: Option<Duration>,
    completed: Option<Duration>,
    /// Set once the line has been printed, so a second terminal event cannot print a second line.
    emitted: bool,
    /// The first text arrived while this turn's tab was not the one on screen (phase 3 ruling 39):
    /// no paint will be reported, so the trace completes without one.
    background: bool,
    /// The turn's start or its first text reached the panel inside a snapshot, not as `events`
    /// (P1-A2 round 2, `observe_in_snapshot`): the panel measures a paint only for text it received
    /// as events, and resets that measurement only on a `turn_started` it received as events, so a
    /// paint report for this turn may never come and the trace completes without waiting for one.
    in_snapshot: bool,
}

impl TurnTrace {
    /// Starts a trace, or returns `None` when tracing is off -- so every call site is a cheap
    /// `Option` check rather than a flag test scattered through the panel.
    pub fn start() -> Option<Self> {
        enabled().then(Self::started_now)
    }

    /// A trace regardless of `NEOVIBE_AGENT_TRACE`, for this crate's tests.
    pub(crate) fn started_now() -> Self {
        Self {
            submitted_at: Instant::now(),
            turn_id: None,
            first_provider_event: None,
            first_presentation_delta: None,
            first_paint_frame: None,
            first_text_dispatched: None,
            completed: None,
            emitted: false,
            background: false,
            in_snapshot: false,
        }
    }

    /// The turn's first text reached this side while its tab was not the one on screen (phase 3
    /// ruling 39): nothing will paint it, so the trace completes without a paint mark.
    pub fn mark_background(&mut self) {
        self.background = true;
    }

    pub fn is_emitted(&self) -> bool {
        self.emitted
    }

    fn since_submit(&self) -> Duration {
        self.submitted_at.elapsed()
    }

    /// Folds one drained batch. Called with the same slice the bridge is about to serialize, so the
    /// marks describe the events the user is actually about to see.
    ///
    /// Returns true when this batch carried the first assistant text, which is the caller's cue to
    /// stamp the dispatch time.
    pub fn observe(&mut self, events: &[AgentDomainEvent]) -> bool {
        let mut carried_first_text = false;
        for event in events {
            if self.first_provider_event.is_none() {
                self.first_provider_event = Some(self.since_submit());
            }
            match event {
                AgentDomainEvent::TurnStarted { turn_id } => {
                    if self.turn_id.is_none() {
                        self.turn_id = Some(turn_id.clone());
                    }
                }
                AgentDomainEvent::ContentDelta {
                    kind: ContentKind::Text,
                    text,
                    ..
                } if self.first_presentation_delta.is_none() && !text.is_empty() => {
                    self.first_presentation_delta = Some(self.since_submit());
                    carried_first_text = true;
                }
                AgentDomainEvent::TurnCompleted { .. } => {
                    self.completed = Some(self.since_submit());
                }
                // A turn that ends because the session did still gets a line: a trace that only
                // prints for turns that finished cleanly hides exactly the slow, broken ones.
                AgentDomainEvent::SessionUnavailable { .. } | AgentDomainEvent::SessionClosed { .. }
                    if self.completed.is_none() =>
                {
                    self.completed = Some(self.since_submit());
                }
                _ => {}
            }
        }
        carried_first_text
    }

    /// Folds a batch the panel received inside a snapshot rather than as `events`: the part of a
    /// drained delivery the last switch/reload or resync snapshot already carried, which `TabSet::pump`
    /// leaves out of its payload. Marked as `observe` marks any batch -- the turn's end included, so
    /// its line still prints -- and, when the batch holds the turn's start or its first text, the
    /// trace stops waiting for a paint (`in_snapshot`).
    pub fn observe_in_snapshot(&mut self, events: &[AgentDomainEvent]) {
        let first_text = self.observe(events);
        let started = events
            .iter()
            .any(|event| matches!(event, AgentDomainEvent::TurnStarted { .. }));
        if first_text || started {
            self.in_snapshot = true;
        }
    }

    /// Records when the payload carrying the first assistant text was handed to the WebView.
    pub fn mark_first_text_dispatched(&mut self) {
        if self.first_text_dispatched.is_none() {
            self.first_text_dispatched = Some(self.since_submit());
        }
    }

    /// The WebView's own receive-to-animation-frame span for that payload. Ignored if it arrives
    /// twice, or before the dispatch it refers to.
    pub fn mark_painted(&mut self, receive_to_frame_ms: f64) {
        if self.first_paint_frame.is_some() || !receive_to_frame_ms.is_finite() || receive_to_frame_ms < 0.0 {
            return;
        }
        if let Some(dispatched) = self.first_text_dispatched {
            self.first_paint_frame = Some(dispatched + Duration::from_secs_f64(receive_to_frame_ms / 1000.0));
        }
    }

    pub fn is_finished(&self) -> bool {
        self.completed.is_some()
    }

    /// True once every mark that is still coming has arrived. A turn whose text was never painted
    /// (an interrupt before any text, a session that died first) is finished without it, so this
    /// also reports true when there is nothing left to wait for. Nor is a paint coming for a turn
    /// whose text arrived while its tab was in the background (`mark_background`).
    pub fn is_complete(&self) -> bool {
        self.is_finished()
            && (self.first_paint_frame.is_some()
                || self.first_presentation_delta.is_none()
                || self.background
                || self.in_snapshot)
    }

    /// Prints the line. Idempotent: the caller emits on completion and again on a deadline, and only
    /// the first one produces output.
    pub fn emit(&mut self) {
        if self.emitted {
            return;
        }
        self.emitted = true;
        eprintln!("{}", self.line());
    }

    /// The line `emit` prints. A background turn with no paint reads `first_paint_frame=bg`, and one
    /// whose start or first text the panel got inside a snapshot `first_paint_frame=snapshot`.
    pub fn line(&self) -> String {
        let mark = |d: Option<Duration>| match d {
            Some(d) => format!("{:.0}ms", ms(d)),
            None => "--".to_string(),
        };
        let paint = if self.background && self.first_paint_frame.is_none() {
            "bg".to_string()
        } else if self.in_snapshot && self.first_paint_frame.is_none() {
            "snapshot".to_string()
        } else {
            mark(self.first_paint_frame)
        };
        format!(
            "[turn-trace] turn={} submitted=0ms first_provider_event={} first_presentation_delta={} \
             first_paint_frame={} completed={}",
            self.turn_id.as_deref().unwrap_or("?"),
            mark(self.first_provider_event),
            mark(self.first_presentation_delta),
            paint,
            mark(self.completed),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent::TurnOutcome;

    /// Built directly rather than through `start()`, which is env-gated: these tests are about the
    /// folding, not about whether the flag is set.
    fn trace() -> TurnTrace {
        TurnTrace::started_now()
    }

    fn text(s: &str) -> AgentDomainEvent {
        AgentDomainEvent::ContentDelta {
            turn_id: "t1".into(),
            kind: ContentKind::Text,
            text: s.into(),
        }
    }

    #[test]
    fn the_first_provider_event_can_precede_the_first_text() {
        let mut trace = trace();
        // Thinking arrives first on a real turn, and often by many seconds. Counting it as the first
        // presentation delta would make the pipeline look faster than a reader could possibly see.
        let carried = trace.observe(&[
            AgentDomainEvent::TurnStarted { turn_id: "t1".into() },
            AgentDomainEvent::ContentDelta {
                turn_id: "t1".into(),
                kind: ContentKind::Thinking,
                text: "hmm".into(),
            },
        ]);
        assert!(!carried);
        assert!(trace.first_provider_event.is_some());
        assert!(trace.first_presentation_delta.is_none());
        assert_eq!(trace.turn_id.as_deref(), Some("t1"));

        assert!(
            trace.observe(&[text("hello")]),
            "the batch carrying the first text must say so"
        );
        assert!(trace.first_presentation_delta.is_some());
    }

    #[test]
    fn an_empty_text_delta_is_not_the_first_visible_text() {
        let mut trace = trace();
        assert!(!trace.observe(&[text("")]));
        assert!(trace.first_presentation_delta.is_none());
    }

    #[test]
    fn only_the_first_text_batch_claims_the_mark() {
        let mut trace = trace();
        assert!(trace.observe(&[text("a")]));
        let first = trace.first_presentation_delta;
        assert!(!trace.observe(&[text("b")]));
        assert_eq!(trace.first_presentation_delta, first);
    }

    #[test]
    fn a_paint_report_is_added_to_the_dispatch_it_refers_to() {
        let mut trace = trace();
        trace.observe(&[text("a")]);
        trace.mark_first_text_dispatched();
        let dispatched = trace.first_text_dispatched.unwrap();
        trace.mark_painted(40.0);
        let painted = trace.first_paint_frame.unwrap();
        assert!(
            painted >= dispatched + Duration::from_millis(39),
            "{painted:?} vs {dispatched:?}"
        );
        // A second report is ignored -- the mark is "first", and a later frame is not it.
        trace.mark_painted(500.0);
        assert_eq!(trace.first_paint_frame, Some(painted));
    }

    /// A report that arrives with no dispatch to attach it to is dropped rather than measured from
    /// submit -- that would silently hand the WebView credit for the model's own thinking time.
    #[test]
    fn a_paint_report_without_a_dispatch_is_ignored() {
        let mut trace = trace();
        trace.mark_painted(40.0);
        assert!(trace.first_paint_frame.is_none());
    }

    #[test]
    fn a_nonsense_paint_report_is_ignored() {
        let mut trace = trace();
        trace.observe(&[text("a")]);
        trace.mark_first_text_dispatched();
        trace.mark_painted(-5.0);
        trace.mark_painted(f64::NAN);
        assert!(trace.first_paint_frame.is_none());
    }

    #[test]
    fn a_turn_that_produced_no_text_is_complete_without_a_paint() {
        let mut trace = trace();
        trace.observe(&[AgentDomainEvent::TurnCompleted {
            turn_id: "t1".into(),
            outcome: TurnOutcome::Interrupted,
            result_text: String::new(),
            stop_reason: None,
            usage: None,
        }]);
        assert!(trace.is_finished());
        assert!(
            trace.is_complete(),
            "nothing is still coming, so waiting for a paint would hang the line"
        );
    }

    /// The slow and broken turns are the ones worth tracing, so a session that dies mid-turn still
    /// produces a line rather than being the one case that silently never reports.
    #[test]
    fn a_session_that_dies_mid_turn_still_finishes_its_trace() {
        let mut trace = trace();
        trace.observe(&[text("half an ans")]);
        trace.mark_first_text_dispatched();
        trace.observe(&[AgentDomainEvent::SessionUnavailable { reason: "gone".into() }]);
        assert!(trace.is_finished());
    }

    #[test]
    fn a_background_turn_is_complete_without_a_paint_and_says_so() {
        let mut trace = trace();
        trace.observe(&[AgentDomainEvent::TurnStarted { turn_id: "t1".into() }, text("hi")]);
        trace.mark_background();
        trace.observe(&[AgentDomainEvent::TurnCompleted {
            turn_id: "t1".into(),
            outcome: TurnOutcome::Completed,
            result_text: String::new(),
            stop_reason: None,
            usage: None,
        }]);
        assert!(
            trace.is_complete(),
            "no paint is coming for a tab that was not on screen"
        );
        assert!(trace.line().contains("first_paint_frame=bg"), "{}", trace.line());
        assert!(!trace.is_emitted());
        trace.emit();
        assert!(trace.is_emitted());
    }

    /// P1-A2 round 2: a turn whose start or first text reached the panel inside a snapshot gets no
    /// paint report (the panel measures only text it received as events), so its line must not wait
    /// for one -- and a batch seen only through a snapshot still marks the turn's end.
    #[test]
    fn a_turn_the_panel_got_inside_a_snapshot_is_complete_without_a_paint() {
        let mut trace = trace();
        trace.observe_in_snapshot(&[AgentDomainEvent::TurnStarted { turn_id: "t1".into() }]);
        assert!(trace.observe(&[text("hi")]), "the first text still reports itself");
        trace.mark_first_text_dispatched();
        trace.observe_in_snapshot(&[AgentDomainEvent::TurnCompleted {
            turn_id: "t1".into(),
            outcome: TurnOutcome::Completed,
            result_text: String::new(),
            stop_reason: None,
            usage: None,
        }]);
        assert!(
            trace.is_finished(),
            "an end seen only in a snapshot still ends the trace"
        );
        assert!(trace.is_complete(), "no paint report is coming for this turn");
        assert!(trace.line().contains("first_paint_frame=snapshot"), "{}", trace.line());

        // A batch with neither the start nor the first text changes nothing about the paint.
        let mut trace = self::trace();
        trace.observe(&[AgentDomainEvent::TurnStarted { turn_id: "t1".into() }, text("hi")]);
        trace.mark_first_text_dispatched();
        trace.observe_in_snapshot(&[text(" more")]);
        trace.observe(&[AgentDomainEvent::TurnCompleted {
            turn_id: "t1".into(),
            outcome: TurnOutcome::Completed,
            result_text: String::new(),
            stop_reason: None,
            usage: None,
        }]);
        assert!(!trace.is_complete(), "the paint of text sent as events is still owed");
    }

    #[test]
    fn emit_prints_once() {
        let mut trace = trace();
        trace.emit();
        assert!(trace.emitted);
        trace.emit(); // no panic, no second line
    }
}
