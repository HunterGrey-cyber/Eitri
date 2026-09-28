//! Partial assistant streaming, measured and pinned against a real sidecar and a real Claude CLI.
//!
//! `#[ignore]`d and billed. Run with
//! `cargo test -p agent --test claude_sidecar_partial_streaming -- --ignored --test-threads=1 --nocapture`.
//!
//! **Terminology.** The SDK's `includePartialMessages` emits `stream_event` messages carrying
//! Anthropic Messages API streaming events. Those are PRESENTATION deltas -- nothing in the SDK
//! contract guarantees a token boundary -- so nothing here calls them tokens. "Partial assistant
//! update" and "streaming content" are the accurate terms.
//!
//! The correctness risk this file exists for is not "does text appear sooner". It is that with
//! partials on, the provider sends the incremental deltas AND, at the end, the completed assistant
//! message carrying the same characters. Translating both doubles every reply; translating neither
//! loses it. `final text == accumulated stream` is the assertion that catches both directions, and
//! it is checked against the turn's own authoritative `result_text`, not against a second copy of
//! the same accumulation.
//!
//! **Since R07 (2026-09-27) every session here is gated** (`INTERACTIVE`, the CLI in `default`);
//! they used BYPASS so a tool call ran unasked. A turn that uses a tool now raises a
//! `PermissionRequested`, which `run_timed_turn` answers `Allow` -- neovibe's own bypass answer,
//! given at the provider level because these tests drive the provider directly.

use agent::{
    AgentDomainEvent, AgentProvider, ClaudeSidecarProvider, CloseSessionRequest, ContentKind, CreateSessionRequest,
    InterruptTurnRequest, PermissionDecision, ResolvePermissionRequest, SendTurnRequest, StreamingPreference,
    TurnOutcome,
};
use std::time::{Duration, Instant};

/// A long-but-bounded prompt. Long enough that streaming has something to stream (the
/// complete-mode measurement is meaningless on a two-word reply), bounded so the bill stays sane.
const LONG_PROMPT: &str = "Write about 600 words on why text editors converged on modal and non-modal designs. \
     Continuous prose, no headings, no bullet points.";

struct Timeline {
    submitted: Instant,
    first_visible: Option<Instant>,
    last_visible: Option<Instant>,
    completed: Option<Instant>,
    /// Every ContentDelta text in arrival order, so ordering and duplication are inspectable.
    chunks: Vec<String>,
    events: Vec<AgentDomainEvent>,
}

impl Timeline {
    fn accumulated(&self) -> String {
        self.chunks.concat()
    }
    fn ms(&self, at: Option<Instant>) -> String {
        match at {
            Some(t) => format!("{}ms", t.duration_since(self.submitted).as_millis()),
            None => "never".to_string(),
        }
    }
    fn report(&self, label: &str) {
        eprintln!(
            "---- {label} ----\n  turn submitted:        0ms\n  first visible text:    {}\n  \
             final visible text:    {}\n  TurnCompleted:         {}\n  content events:        {}\n  \
             accumulated chars:     {}",
            self.ms(self.first_visible),
            self.ms(self.last_visible),
            self.ms(self.completed),
            self.chunks.len(),
            self.accumulated().len(),
        );
    }
}

fn connect() -> ClaudeSidecarProvider {
    ClaudeSidecarProvider::connect(&uuid::Uuid::new_v4().to_string())
        .expect("connecting to a real sidecar should succeed")
}

fn open(provider: &ClaudeSidecarProvider, streaming: StreamingPreference) -> String {
    provider
        .create_session(CreateSessionRequest {
            cwd: std::env::temp_dir().to_string_lossy().to_string(),
            streaming,
        })
        .expect("create_session should succeed")
}

/// Sends one turn and records when each thing first became visible, polling fast enough that the
/// measurement reflects the provider rather than the poll interval. 25ms is well under the 33ms the
/// real UI pump uses, so these numbers are if anything conservative about how soon a user sees text.
fn run_timed_turn(provider: &ClaudeSidecarProvider, session_id: &str, prompt: &str, deadline_secs: u64) -> Timeline {
    let submitted = Instant::now();
    provider
        .send_turn(SendTurnRequest {
            session_id: session_id.to_string(),
            text: prompt.to_string(),
        })
        .expect("send_turn should be accepted");

    let mut timeline = Timeline {
        submitted,
        first_visible: None,
        last_visible: None,
        completed: None,
        chunks: Vec::new(),
        events: Vec::new(),
    };
    let deadline = submitted + Duration::from_secs(deadline_secs);
    while Instant::now() < deadline && timeline.completed.is_none() {
        for event in provider.pump() {
            let now = Instant::now();
            match &event {
                AgentDomainEvent::ContentDelta {
                    kind: ContentKind::Text,
                    text,
                    ..
                } => {
                    if timeline.first_visible.is_none() {
                        timeline.first_visible = Some(now);
                    }
                    timeline.last_visible = Some(now);
                    timeline.chunks.push(text.clone());
                }
                AgentDomainEvent::TurnCompleted { .. } => timeline.completed = Some(now),
                AgentDomainEvent::PermissionRequested { permission_id, .. } => provider
                    .resolve_permission(ResolvePermissionRequest {
                        session_id: session_id.to_string(),
                        permission_id: permission_id.clone(),
                        decision: PermissionDecision::Allow,
                    })
                    .expect("answering a pending request should succeed"),
                _ => {}
            }
            timeline.events.push(event);
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    timeline
}

fn result_text(events: &[AgentDomainEvent]) -> Option<String> {
    events.iter().find_map(|e| match e {
        AgentDomainEvent::TurnCompleted { result_text, .. } => Some(result_text.clone()),
        _ => None,
    })
}

/// The headline measurement, and the acceptance checks that make it meaningful.
///
/// Both modes run in one test, against one sidecar build, minutes apart, on the same prompt. Two
/// separate tests would invite comparing numbers taken under different conditions, which is exactly
/// the mistake this measurement exists to prevent someone else from making.
#[test]
#[ignore]
fn partial_streaming_shows_content_sooner_and_loses_or_duplicates_nothing() {
    let provider = connect();

    // --- baseline: complete-message mode (the behavior before this change) ----------------------
    let complete_session = open(&provider, StreamingPreference::Complete);
    let complete = run_timed_turn(&provider, &complete_session, LONG_PROMPT, 180);
    complete.report("STREAMING_MODE_COMPLETE (baseline)");
    provider
        .close_session(CloseSessionRequest {
            session_id: complete_session,
        })
        .unwrap();

    assert!(complete.completed.is_some(), "the baseline turn did not finish");
    let complete_final = result_text(&complete.events).expect("a completed turn reports its result text");
    // The baseline reconciles too. Checking it here is what makes the same assertion on the partial
    // side meaningful: if reconciliation only held in one mode, the difference would be the mode,
    // not the streaming.
    assert_eq!(
        complete.accumulated(),
        complete_final,
        "even in complete mode the content events must add up to the turn's final text"
    );

    // --- partial mode ----------------------------------------------------------------------------
    let partial_session = open(&provider, StreamingPreference::Partial);
    let partial = run_timed_turn(&provider, &partial_session, LONG_PROMPT, 180);
    partial.report("STREAMING_MODE_PARTIAL");

    assert!(partial.completed.is_some(), "the partial-mode turn did not finish");
    let partial_final = result_text(&partial.events).expect("a completed turn reports its result text");

    // 1. Incremental at all. One content event is what complete mode produces; partial mode must
    //    produce many, or nothing about this change worked.
    assert!(
        partial.chunks.len() > complete.chunks.len(),
        "partial mode produced {} content events vs the baseline's {} -- it is not incremental",
        partial.chunks.len(),
        complete.chunks.len()
    );
    assert!(
        partial.chunks.len() > 5,
        "expected many partial updates, got {}",
        partial.chunks.len()
    );

    // 2. NO DUPLICATE TEXT and NO LOST TEXT, in one assertion, against the turn's own authoritative
    //    final text rather than a second copy of the same accumulation. Both failure directions are
    //    live: the provider sends the deltas AND the completed message, so translating both doubles
    //    the reply and translating neither empties it.
    // Equality holds here because this prompt produces exactly ONE assistant message. On a turn with
    // several (text, tool call, more text) `result_text` carries only the LAST one -- see
    // `tool_activity_interleaves_with_partial_assistant_updates`, which asserts the suffix
    // relationship that generalizes this.
    assert_eq!(
        partial.accumulated(),
        partial_final,
        "the accumulated partial stream does not equal the turn's final text -- text was duplicated or lost"
    );

    // 3. Ordering. The stream must read as the reply, in order: the accumulation's prefix is the
    //    final text's prefix at every chunk boundary. A reordered stream would still satisfy (2) if
    //    it happened to contain the same characters.
    let mut prefix = String::new();
    for chunk in &partial.chunks {
        prefix.push_str(chunk);
        assert!(
            partial_final.starts_with(&prefix),
            "partial updates arrived out of order: {:?} is not a prefix of the final text",
            prefix.chars().take(80).collect::<String>()
        );
    }

    // 4. First visible content arrives meaningfully sooner. Asserted as a real inequality, not just
    //    reported -- a regression that silently returns to complete-mode behavior must fail here.
    let complete_first = complete.first_visible.expect("baseline produced no text at all");
    let partial_first = partial.first_visible.expect("partial mode produced no text at all");
    let complete_ttfb = complete_first.duration_since(complete.submitted);
    let partial_ttfb = partial_first.duration_since(partial.submitted);
    eprintln!(
        "time to first visible content: baseline {}ms -> partial {}ms",
        complete_ttfb.as_millis(),
        partial_ttfb.as_millis()
    );
    assert!(
        partial_ttfb < complete_ttfb,
        "partial mode was not faster to first visible content ({}ms vs {}ms)",
        partial_ttfb.as_millis(),
        complete_ttfb.as_millis()
    );

    // 5. TurnCompleted reconciles: it arrives after the last visible text, and its outcome is clean.
    assert_eq!(
        partial
            .events
            .iter()
            .filter(|e| matches!(e, AgentDomainEvent::TurnCompleted { .. }))
            .count(),
        1,
        "exactly one terminal event per turn"
    );
    let completed_at = partial.completed.unwrap();
    assert!(
        completed_at >= partial.last_visible.unwrap(),
        "TurnCompleted preceded the last content it completes"
    );
    assert!(matches!(
        partial.events.iter().find_map(|e| match e {
            AgentDomainEvent::TurnCompleted { outcome, .. } => Some(*outcome),
            _ => None,
        }),
        Some(TurnOutcome::Completed)
    ));

    // 6. The session survives a partial-stream turn: a second turn runs on it normally.
    let second = run_timed_turn(&provider, &partial_session, "Reply with exactly the word: pong", 60);
    second.report("second turn after a partial-stream turn");
    assert!(
        second.accumulated().to_lowercase().contains("pong"),
        "got: {:?}",
        second.accumulated()
    );
    assert_eq!(
        result_text(&second.events).as_deref().map(str::trim),
        Some(second.accumulated().trim()),
        "the second turn's stream and final text disagree"
    );

    provider
        .close_session(CloseSessionRequest {
            session_id: partial_session,
        })
        .unwrap();
}

/// Interrupting mid-stream must terminate the turn cleanly and leave the accumulated text coherent.
///
/// Distinct from the complete-mode interrupt test: there, nothing had been delivered yet when Stop
/// was pressed, so there was no partial state to be inconsistent. Here there is, and the question is
/// whether what was already shown remains a valid prefix of what the provider says the turn produced.
#[test]
#[ignore]
fn interrupting_a_partial_stream_terminates_cleanly_and_leaves_a_coherent_prefix() {
    let provider = connect();
    let session_id = open(&provider, StreamingPreference::Partial);

    let submitted = Instant::now();
    provider
        .send_turn(SendTurnRequest {
            session_id: session_id.clone(),
            text: LONG_PROMPT.to_string(),
        })
        .unwrap();

    // Interrupt only once real content has arrived -- otherwise this tests an interrupt of nothing,
    // which the complete-mode suite already covers.
    let mut before_interrupt = String::new();
    let deadline = submitted + Duration::from_secs(60);
    while Instant::now() < deadline && before_interrupt.len() < 200 {
        for event in provider.pump() {
            if let AgentDomainEvent::ContentDelta {
                kind: ContentKind::Text,
                text,
                ..
            } = event
            {
                before_interrupt.push_str(&text);
            }
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(
        before_interrupt.len() >= 200,
        "no partial content arrived within 60s, so there was nothing to interrupt mid-stream"
    );
    eprintln!("interrupting after {} streamed chars", before_interrupt.len());

    provider
        .interrupt_turn(InterruptTurnRequest {
            session_id: session_id.clone(),
        })
        .unwrap();

    let mut after = before_interrupt.clone();
    let mut outcome = None;
    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline && outcome.is_none() {
        for event in provider.pump() {
            match event {
                AgentDomainEvent::ContentDelta {
                    kind: ContentKind::Text,
                    text,
                    ..
                } => after.push_str(&text),
                AgentDomainEvent::TurnCompleted { outcome: o, .. } => outcome = Some(o),
                _ => {}
            }
        }
        std::thread::sleep(Duration::from_millis(25));
    }

    assert_eq!(
        outcome,
        Some(TurnOutcome::Interrupted),
        "an interrupted partial stream must terminate as Interrupted"
    );
    // What was already on screen stays on screen: the pre-interrupt text is still a prefix of
    // everything received. A stream that rewrote or dropped already-shown text would break a UI
    // that has appended it.
    assert!(
        after.starts_with(&before_interrupt),
        "text already shown was not a prefix of the final stream"
    );

    // And the session is still usable.
    let second = run_timed_turn(&provider, &session_id, "Reply with exactly the word: pong", 60);
    assert!(
        second.accumulated().to_lowercase().contains("pong"),
        "got: {:?}",
        second.accumulated()
    );

    provider.close_session(CloseSessionRequest { session_id }).unwrap();
}

/// Tool activity must interleave with assistant partials rather than being starved by them, and the
/// accumulated text must still reconcile when a turn contains both.
#[test]
#[ignore]
fn tool_activity_interleaves_with_partial_assistant_updates() {
    let provider = connect();
    let session_id = open(&provider, StreamingPreference::Partial);

    let timeline = run_timed_turn(
        &provider,
        &session_id,
        "First say a sentence about what you are about to do. Then run the bash command \
         `echo neovibe_partial_marker_4d1c`. Then say a sentence about its output.",
        120,
    );
    timeline.report("tool + partial assistant updates");

    let tool_started = timeline
        .events
        .iter()
        .position(|e| matches!(e, AgentDomainEvent::ToolCallStarted { .. }));
    let tool_started = tool_started.expect("no tool call ran; this turn does not test interleaving");
    assert!(
        timeline
            .events
            .iter()
            .any(|e| matches!(e, AgentDomainEvent::ToolCallCompleted { is_error: false, .. })),
        "the tool call never completed successfully"
    );

    // Assistant text both BEFORE and AFTER the tool call, in the same event sequence. Text only
    // after would mean partial updates were being withheld until the turn was nearly done.
    let content_positions: Vec<usize> = timeline
        .events
        .iter()
        .enumerate()
        .filter(|(_, e)| {
            matches!(
                e,
                AgentDomainEvent::ContentDelta {
                    kind: ContentKind::Text,
                    ..
                }
            )
        })
        .map(|(i, _)| i)
        .collect();
    assert!(
        content_positions.iter().any(|i| *i < tool_started),
        "no assistant text arrived before the tool call"
    );
    assert!(
        content_positions.iter().any(|i| *i > tool_started),
        "no assistant text arrived after the tool call"
    );

    // The marker only exists in the tool's real output, so this proves the result reached the model.
    assert!(
        timeline.accumulated().contains("neovibe_partial_marker_4d1c"),
        "the tool's output never reached the assistant text. got: {:?}",
        timeline.accumulated()
    );
    // Reconciliation on a MULTI-MESSAGE turn is a different property, and getting this assertion
    // wrong the first time is what revealed why.
    //
    // MEASURED: `TurnCompleted.result_text` is the LAST assistant message, not the turn's whole
    // assistant output. This turn produced two ("I'll run a quick echo command to check its
    // output." then "The command printed exactly the marker string ..."), and result_text carried
    // only the second. So the equality that holds for a single-message turn becomes a SUFFIX
    // relationship here, and a consumer that reconstructs a transcript from `result_text` alone
    // silently loses everything a turn said before its last tool call.
    //
    // Not introduced by partial streaming: `result_text` comes from the SDK's own `result` message
    // regardless of mode, and complete mode emits one text_delta per assistant message with the
    // same final-message-only result_text. Partial streaming only made it visible.
    let final_text = result_text(&timeline.events).expect("a completed turn reports its result text");
    assert!(
        timeline.accumulated().ends_with(&final_text),
        "the turn's final message must be the tail of the accumulated stream.\n  accumulated: {:?}\n  result_text: {:?}",
        timeline.accumulated(),
        final_text
    );
    assert!(
        timeline.accumulated().len() > final_text.len(),
        "this turn should have said something BEFORE its tool call, so the stream must exceed result_text"
    );

    provider.close_session(CloseSessionRequest { session_id }).unwrap();
}
