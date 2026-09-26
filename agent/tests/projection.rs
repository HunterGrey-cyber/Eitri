use agent::{
    AgentDomainEvent, AgentSessionProjection, ContentKind, PermissionOutcome, ProjectionStatus, TurnOutcome, UsageInfo,
};
use serde_json::json;

/// The assistant message texts, without their ordering keys.
///
/// Test-local on purpose. `AgentSessionProjection` carried a `transcript_text()` accessor for
/// exactly this until 2026-09-15, but its only callers were ever tests, and a `pub` method with
/// no production consumer is the thing this crate's own YAGNI rule (see `projection.rs`'s module
/// doc) exists to keep out -- it also reads as an invitation for a future renderer, which must
/// use `transcript` itself or it is back to guessing where the tool calls went.
fn texts(projection: &AgentSessionProjection) -> Vec<&str> {
    projection.transcript.iter().map(|m| m.text.as_str()).collect()
}

#[test]
fn session_opened_populates_identity_and_sets_running() {
    let mut projection = AgentSessionProjection::default();
    projection.apply(&AgentDomainEvent::SessionOpened {
        session_id: "sess-1".into(),
        provider_session_id: "prov-1".into(),
        model: "claude-sonnet-5".into(),
        cwd: "/tmp/project".into(),
    });
    assert_eq!(projection.session_id, Some("sess-1".into()));
    assert_eq!(projection.provider_session_id, Some("prov-1".into()));
    assert_eq!(projection.model, Some("claude-sonnet-5".into()));
    assert_eq!(projection.cwd, Some("/tmp/project".into()));
    assert_eq!(projection.status, ProjectionStatus::Running);
    assert_eq!(projection.last_revision, 1);
}

#[test]
fn turn_started_sets_active_turn_id() {
    let mut projection = AgentSessionProjection::default();
    projection.apply(&AgentDomainEvent::TurnStarted {
        turn_id: "turn-1".into(),
    });
    assert_eq!(projection.active_turn_id, Some("turn-1".into()));
}

#[test]
fn content_delta_text_accumulates_into_one_message_in_order() {
    // This asserted ["hello", "world"] until partial streaming landed. `transcript` holds assistant
    // MESSAGES, and two consecutive text deltas with nothing between them are one message -- under
    // StreamingPreference::Partial they are two fragments of one sentence, and one entry each would
    // render 400 separate bubbles for a single reply.
    let mut projection = AgentSessionProjection::default();
    projection.apply(&AgentDomainEvent::ContentDelta {
        turn_id: "turn-1".into(),
        kind: ContentKind::Text,
        text: "hello".into(),
    });
    projection.apply(&AgentDomainEvent::ContentDelta {
        turn_id: "turn-1".into(),
        kind: ContentKind::Text,
        text: "world".into(),
    });
    assert_eq!(texts(&projection), vec!["helloworld"]);
}

#[test]
fn content_delta_thinking_has_no_transcript_effect_but_still_bumps_revision() {
    let mut projection = AgentSessionProjection::default();
    projection.apply(&AgentDomainEvent::ContentDelta {
        turn_id: "turn-1".into(),
        kind: ContentKind::Thinking,
        text: "hmm".into(),
    });
    assert!(projection.transcript.is_empty());
    assert_eq!(projection.last_revision, 1);
}

#[test]
fn tool_call_started_then_completed_links_by_tool_use_id() {
    let mut projection = AgentSessionProjection::default();
    projection.apply(&AgentDomainEvent::ToolCallStarted {
        turn_id: "turn-1".into(),
        tool_use_id: "toolu_1".into(),
        name: "Bash".into(),
        input: json!({"command": "echo hi"}),
    });
    projection.apply(&AgentDomainEvent::ToolCallCompleted {
        turn_id: "turn-1".into(),
        tool_use_id: "toolu_1".into(),
        content: json!("hi\n"),
        is_error: false,
    });
    assert_eq!(projection.tool_calls.len(), 1);
    let call = &projection.tool_calls[0];
    assert_eq!(call.tool_use_id, "toolu_1");
    let result = call.result.as_ref().expect("result should be set");
    assert_eq!(result.content, json!("hi\n"));
    assert!(!result.is_error);
}

#[test]
fn tool_call_completed_for_unknown_tool_use_id_is_a_harmless_no_op() {
    let mut projection = AgentSessionProjection::default();
    projection.apply(&AgentDomainEvent::ToolCallCompleted {
        turn_id: "turn-1".into(),
        tool_use_id: "does-not-exist".into(),
        content: json!("x"),
        is_error: false,
    });
    assert!(projection.tool_calls.is_empty());
}

#[test]
fn permission_requested_populates_pending_permissions_map() {
    let mut projection = AgentSessionProjection::default();
    projection.apply(&AgentDomainEvent::PermissionRequested {
        permission_id: "perm-1".into(),
        tool_use_id: Some("tu-1".into()),
        tool_name: "Bash".into(),
        input: json!({}),
    });
    assert!(projection.pending_permissions.contains_key("perm-1"));
    assert_eq!(projection.pending_permissions["perm-1"].tool_name, "Bash");
    assert_eq!(
        projection.pending_permissions["perm-1"].tool_use_id,
        Some("tu-1".into())
    );
}

/// The link itself, at the level that matters to a reader of the projection: the id a pending
/// permission carries is the SAME id a `ToolCallRecord` is keyed on, so "which call is this card
/// gating" is answerable by equality without either side knowing about the other. Two calls of
/// the same tool are in flight here on purpose -- that is the case where a card saying only
/// "Bash" identifies nothing.
#[test]
fn a_pending_permission_names_one_specific_tool_call_among_several_of_the_same_tool() {
    let mut projection = AgentSessionProjection::default();
    projection.apply(&AgentDomainEvent::ToolCallStarted {
        turn_id: "turn-1".into(),
        tool_use_id: "toolu_first".into(),
        name: "Bash".into(),
        input: json!({"command": "echo one"}),
    });
    projection.apply(&AgentDomainEvent::ToolCallStarted {
        turn_id: "turn-1".into(),
        tool_use_id: "toolu_second".into(),
        name: "Bash".into(),
        input: json!({"command": "rm -rf /"}),
    });
    projection.apply(&AgentDomainEvent::PermissionRequested {
        permission_id: "toolu_second".into(),
        tool_use_id: Some("toolu_second".into()),
        tool_name: "Bash".into(),
        input: json!({"command": "rm -rf /"}),
    });

    let request = &projection.pending_permissions["toolu_second"];
    let gated: Vec<&str> = projection
        .tool_calls
        .iter()
        .filter(|c| Some(&c.tool_use_id) == request.tool_use_id.as_ref())
        .map(|c| c.tool_use_id.as_str())
        .collect();
    assert_eq!(
        gated,
        vec!["toolu_second"],
        "exactly one call is gated, and it is the second one"
    );
}

/// The legacy hook-relay shape specifically: its `permission_id` and its `tool_use_id` are the
/// same string, because the `PreToolUse` payload's own `tool_use_id` is both the id of the gated
/// call and the key the relay connection is filed under. Nothing downstream may assume the two
/// differ -- the test above proves the link still resolves when they are equal.
#[test]
fn the_legacy_hook_relay_shape_where_both_ids_are_one_string_is_stored_intact() {
    let mut projection = AgentSessionProjection::default();
    projection.apply(&AgentDomainEvent::PermissionRequested {
        permission_id: "toolu_01CtdezhmhUCrBaswxW5HYmC".into(),
        tool_use_id: Some("toolu_01CtdezhmhUCrBaswxW5HYmC".into()),
        tool_name: "Bash".into(),
        input: json!({"command": "echo hello"}),
    });
    let record = &projection.pending_permissions["toolu_01CtdezhmhUCrBaswxW5HYmC"];
    assert_eq!(record.permission_id, "toolu_01CtdezhmhUCrBaswxW5HYmC");
    assert_eq!(record.tool_use_id.as_deref(), Some("toolu_01CtdezhmhUCrBaswxW5HYmC"));
}

/// The scenario `agent/BACKEND_BASELINE.md` cites by name as pinning concurrent-permission
/// handling: two simultaneously-pending requests must both be retained and independently
/// resolvable in EITHER order, not just the order they arrived in.
#[test]
fn two_concurrent_permission_requests_are_both_retained_and_independently_resolvable_in_either_order() {
    let mut projection = AgentSessionProjection::default();
    projection.apply(&AgentDomainEvent::PermissionRequested {
        permission_id: "perm-1".into(),
        tool_use_id: None,
        tool_name: "Bash".into(),
        input: json!({}),
    });
    projection.apply(&AgentDomainEvent::PermissionRequested {
        permission_id: "perm-2".into(),
        tool_use_id: None,
        tool_name: "Write".into(),
        input: json!({}),
    });
    assert_eq!(projection.pending_permissions.len(), 2);

    // Resolve the SECOND request first -- proves order-independence, not just that two can coexist.
    projection.apply(&AgentDomainEvent::PermissionResolved {
        permission_id: "perm-2".into(),
        outcome: PermissionOutcome::Allowed,
    });
    assert!(!projection.pending_permissions.contains_key("perm-2"));
    assert!(projection.pending_permissions.contains_key("perm-1"));

    projection.apply(&AgentDomainEvent::PermissionResolved {
        permission_id: "perm-1".into(),
        outcome: PermissionOutcome::Denied,
    });
    assert!(projection.pending_permissions.is_empty());
}

#[test]
fn permission_resolved_for_unknown_id_is_a_harmless_no_op() {
    let mut projection = AgentSessionProjection::default();
    projection.apply(&AgentDomainEvent::PermissionResolved {
        permission_id: "does-not-exist".into(),
        outcome: PermissionOutcome::Allowed,
    });
    assert!(projection.pending_permissions.is_empty());
}

#[test]
fn turn_completed_clears_active_turn_id_and_updates_usage_for_every_outcome() {
    for outcome in [
        TurnOutcome::Completed,
        TurnOutcome::Interrupted,
        TurnOutcome::Failed,
        TurnOutcome::LimitReached,
    ] {
        let mut projection = AgentSessionProjection::default();
        projection.apply(&AgentDomainEvent::TurnStarted {
            turn_id: "turn-1".into(),
        });
        projection.apply(&AgentDomainEvent::TurnCompleted {
            turn_id: "turn-1".into(),
            outcome,
            result_text: "done".into(),
            stop_reason: None,
            usage: Some(UsageInfo {
                total_cost_usd: 0.01,
                num_turns: 1,
            }),
        });
        assert_eq!(
            projection.active_turn_id, None,
            "outcome {outcome:?} must clear active_turn_id"
        );
        assert_eq!(
            projection.usage,
            Some(UsageInfo {
                total_cost_usd: 0.01,
                num_turns: 1
            })
        );
    }
}

/// A provider that reports no usage must leave the projection saying so, and must not be able to
/// overwrite a figure another turn genuinely reported.
///
/// This is the whole point of `usage` being an `Option`. `ClaudeSidecarProvider` sends `None` on
/// every turn because its wire carries no usage fields; before this, its translation layer sent
/// `0.0`/`0` and this arm assigned unconditionally, so a UI reading `usage` would have shown a
/// confident zero for a session that had really spent money, with no way to tell that apart from a
/// turn that was genuinely free.
#[test]
fn a_turn_reporting_no_usage_neither_invents_a_zero_nor_erases_a_real_figure() {
    let mut projection = AgentSessionProjection::default();
    assert_eq!(projection.usage, None, "nothing has been reported yet");

    projection.apply(&AgentDomainEvent::TurnCompleted {
        turn_id: "turn-1".into(),
        outcome: TurnOutcome::Completed,
        result_text: String::new(),
        stop_reason: None,
        usage: None,
    });
    assert_eq!(
        projection.usage, None,
        "absence must stay absence, never become a measured zero"
    );

    projection.apply(&AgentDomainEvent::TurnCompleted {
        turn_id: "turn-2".into(),
        outcome: TurnOutcome::Completed,
        result_text: String::new(),
        stop_reason: None,
        usage: Some(UsageInfo {
            total_cost_usd: 0.25,
            num_turns: 2,
        }),
    });
    projection.apply(&AgentDomainEvent::TurnCompleted {
        turn_id: "turn-3".into(),
        outcome: TurnOutcome::Completed,
        result_text: String::new(),
        stop_reason: None,
        usage: None,
    });
    assert_eq!(
        projection.usage,
        Some(UsageInfo {
            total_cost_usd: 0.25,
            num_turns: 2
        }),
        "a silent turn must not zero out what an earlier turn actually reported"
    );
}

#[test]
fn session_unavailable_and_session_closed_set_distinct_statuses() {
    let mut unavailable = AgentSessionProjection::default();
    unavailable.apply(&AgentDomainEvent::SessionUnavailable {
        reason: "crashed".into(),
    });
    assert_eq!(
        unavailable.status,
        ProjectionStatus::Unavailable {
            reason: "crashed".into()
        }
    );

    let mut closed = AgentSessionProjection::default();
    closed.apply(&AgentDomainEvent::SessionClosed {
        reason: "closed_by_host".into(),
    });
    assert_eq!(
        closed.status,
        ProjectionStatus::Closed {
            reason: "closed_by_host".into()
        }
    );
}

#[test]
fn every_apply_call_bumps_last_revision_by_exactly_one() {
    let mut projection = AgentSessionProjection::default();
    let events = vec![
        AgentDomainEvent::SessionOpened {
            session_id: "s".into(),
            provider_session_id: "p".into(),
            model: "m".into(),
            cwd: "/".into(),
        },
        AgentDomainEvent::TurnStarted { turn_id: "t".into() },
        AgentDomainEvent::ContentDelta {
            turn_id: "t".into(),
            kind: ContentKind::Thinking,
            text: "".into(),
        },
    ];
    for (i, event) in events.iter().enumerate() {
        projection.apply(event);
        assert_eq!(projection.last_revision, (i + 1) as u64);
    }
}

#[test]
fn permission_resolved_accepts_provider_failed_and_expired_outcomes() {
    let mut projection = AgentSessionProjection::default();
    projection.apply(&AgentDomainEvent::PermissionRequested {
        permission_id: "perm-1".into(),
        tool_use_id: None,
        tool_name: "Bash".into(),
        input: json!({}),
    });
    projection.apply(&AgentDomainEvent::PermissionResolved {
        permission_id: "perm-1".into(),
        outcome: PermissionOutcome::ProviderFailed,
    });
    assert!(!projection.pending_permissions.contains_key("perm-1"));

    projection.apply(&AgentDomainEvent::PermissionRequested {
        permission_id: "perm-2".into(),
        tool_use_id: None,
        tool_name: "Write".into(),
        input: json!({}),
    });
    projection.apply(&AgentDomainEvent::PermissionResolved {
        permission_id: "perm-2".into(),
        outcome: PermissionOutcome::Expired,
    });
    assert!(!projection.pending_permissions.contains_key("perm-2"));
}

/// The Rust half of the partial-streaming accumulation. Must stay behaviorally identical to
/// `agent-ui/web/src/reducer.ts`'s own `content_delta` case -- a snapshot from here has to be
/// indistinguishable from what that reducer accumulates from the same events.
#[test]
fn streamed_text_accumulates_into_one_transcript_entry() {
    let mut projection = AgentSessionProjection::default();
    projection.apply(&AgentDomainEvent::TurnStarted { turn_id: "t1".into() });
    for chunk in ["The ", "quick ", "**brown** ", "fox"] {
        projection.apply(&AgentDomainEvent::ContentDelta {
            turn_id: "t1".into(),
            kind: ContentKind::Text,
            text: chunk.into(),
        });
    }
    assert_eq!(texts(&projection), vec!["The quick **brown** fox"]);
    // Every delta is still its own revision: a consumer replaying from a revision must be able to
    // land between two chunks of one message.
    assert_eq!(projection.last_revision, 5);
}

#[test]
fn a_tool_call_or_a_turn_boundary_starts_a_new_transcript_entry() {
    let mut projection = AgentSessionProjection::default();
    projection.apply(&AgentDomainEvent::TurnStarted { turn_id: "t1".into() });
    projection.apply(&AgentDomainEvent::ContentDelta {
        turn_id: "t1".into(),
        kind: ContentKind::Text,
        text: "I'll check.".into(),
    });
    projection.apply(&AgentDomainEvent::ToolCallStarted {
        turn_id: "t1".into(),
        tool_use_id: "tu1".into(),
        name: "Bash".into(),
        input: serde_json::json!({}),
    });
    projection.apply(&AgentDomainEvent::ContentDelta {
        turn_id: "t1".into(),
        kind: ContentKind::Text,
        text: "It printed hi.".into(),
    });
    assert_eq!(texts(&projection), vec!["I'll check.", "It printed hi."]);

    projection.apply(&AgentDomainEvent::TurnCompleted {
        turn_id: "t1".into(),
        outcome: TurnOutcome::Completed,
        result_text: String::new(),
        stop_reason: None,
        usage: None,
    });
    projection.apply(&AgentDomainEvent::TurnStarted { turn_id: "t2".into() });
    projection.apply(&AgentDomainEvent::ContentDelta {
        turn_id: "t2".into(),
        kind: ContentKind::Text,
        text: "next turn".into(),
    });
    assert_eq!(projection.transcript.len(), 3);
}

#[test]
fn a_thinking_delta_does_not_split_the_text_around_it() {
    let mut projection = AgentSessionProjection::default();
    projection.apply(&AgentDomainEvent::TurnStarted { turn_id: "t1".into() });
    projection.apply(&AgentDomainEvent::ContentDelta {
        turn_id: "t1".into(),
        kind: ContentKind::Text,
        text: "before ".into(),
    });
    projection.apply(&AgentDomainEvent::ContentDelta {
        turn_id: "t1".into(),
        kind: ContentKind::Thinking,
        text: "hmm".into(),
    });
    projection.apply(&AgentDomainEvent::ContentDelta {
        turn_id: "t1".into(),
        kind: ContentKind::Text,
        text: "after".into(),
    });
    assert_eq!(texts(&projection), vec!["before after"]);
}

/// The forbidden failure mode, at the projection level.
///
/// A session that dies mid-turn used to leave `active_turn_id` set forever. Three separate surfaces
/// read that field to mean "busy": the panel's composer (spinner, disabled input), the session
/// header's status word, and `supervisor_client::derive_status`, which returns `Working` from it
/// BEFORE it ever looks at the status. So a killed provider showed up as a working agent in every
/// one of them, above a truncated reply that looked merely short.
#[test]
fn a_lost_session_stops_looking_like_a_turn_in_progress() {
    let mut projection = AgentSessionProjection::default();
    projection.apply(&AgentDomainEvent::SessionOpened {
        session_id: "sess-1".into(),
        provider_session_id: "prov-1".into(),
        model: "claude-sonnet-5".into(),
        cwd: "/tmp/project".into(),
    });
    projection.apply(&AgentDomainEvent::TurnStarted { turn_id: "t1".into() });
    projection.apply(&AgentDomainEvent::ContentDelta {
        turn_id: "t1".into(),
        kind: ContentKind::Text,
        text: "half an ans".into(),
    });
    assert_eq!(projection.active_turn_id, Some("t1".into()));

    projection.apply(&AgentDomainEvent::SessionUnavailable {
        reason: "57 event(s) from the provider (sequence 41-97) were never delivered".into(),
    });

    assert_eq!(
        projection.active_turn_id, None,
        "a session that cannot report a turn is not running one"
    );
    assert!(matches!(projection.status, ProjectionStatus::Unavailable { .. }));
    // What DID arrive stays. Clearing the turn must not double as deleting the partial reply: the
    // reason string is what tells the reader it may be incomplete, not its absence.
    assert_eq!(texts(&projection), vec!["half an ans"]);
    // And no completion was invented on the way out. `None` is a stronger statement than the
    // zeroed struct this used to assert: nothing reported usage, as opposed to something reporting
    // nought.
    assert_eq!(projection.usage, None);
}

#[test]
fn a_session_closed_mid_turn_also_stops_looking_like_a_turn_in_progress() {
    let mut projection = AgentSessionProjection::default();
    projection.apply(&AgentDomainEvent::TurnStarted { turn_id: "t1".into() });
    projection.apply(&AgentDomainEvent::SessionClosed {
        reason: "closed_by_host".into(),
    });
    assert_eq!(projection.active_turn_id, None);
    assert!(matches!(projection.status, ProjectionStatus::Closed { .. }));
}

/* ------------------------------------------------------------------------------------------------
Interleaved ordering (2026-09-15).

The defect: `agent-ui`'s MessageList rendered `transcript`, then `toolCalls`, then
`pendingPermissions` as three sequential lists, so every tool card appeared below every assistant
message whatever the turn actually did.

Why the order is produced HERE rather than in the frontend: a snapshot is a complete replacement
of frontend state, and `serialize_snapshot_for_js` emits the three collections separately.
Nothing in that payload said how they interleave -- no per-item index, and no turn id (the
serializer does not even emit `ToolCallRecord::turn_id`, and a turn id could not order items
WITHIN a turn anyway). So a frontend that rebuilt the order from live event arrival alone would
lose it on every WebView reload and on every `UiDelivery::Resync`.

`seq` is read from the same counter as `last_revision`, before `apply` bumps it, so a snapshot's
`throughRevision` is strictly greater than every `seq` in that snapshot.
------------------------------------------------------------------------------------------------ */

/// The whole point, at the projection level: three collections, one order.
#[test]
fn every_item_carries_a_sequence_number_ordering_it_against_the_other_two_collections() {
    let mut projection = AgentSessionProjection::default();
    projection.apply(&AgentDomainEvent::TurnStarted { turn_id: "t1".into() });
    projection.apply(&AgentDomainEvent::ContentDelta {
        turn_id: "t1".into(),
        kind: ContentKind::Text,
        text: "I'll check.".into(),
    });
    projection.apply(&AgentDomainEvent::ToolCallStarted {
        turn_id: "t1".into(),
        tool_use_id: "toolu_1".into(),
        name: "Bash".into(),
        input: json!({}),
    });
    projection.apply(&AgentDomainEvent::ContentDelta {
        turn_id: "t1".into(),
        kind: ContentKind::Text,
        text: "Now the other one.".into(),
    });
    projection.apply(&AgentDomainEvent::ToolCallStarted {
        turn_id: "t1".into(),
        tool_use_id: "toolu_2".into(),
        name: "Read".into(),
        input: json!({}),
    });
    projection.apply(&AgentDomainEvent::PermissionRequested {
        permission_id: "perm-1".into(),
        tool_use_id: Some("toolu_2".into()),
        tool_name: "Read".into(),
        input: json!({}),
    });
    projection.apply(&AgentDomainEvent::ContentDelta {
        turn_id: "t1".into(),
        kind: ContentKind::Text,
        text: "Done.".into(),
    });

    // Merge all three by `seq` and check the result is the real turn, not messages-then-tools.
    let mut merged: Vec<(u64, String)> = Vec::new();
    merged.extend(
        projection
            .transcript
            .iter()
            .map(|m| (m.seq, format!("text:{}", m.text))),
    );
    merged.extend(
        projection
            .tool_calls
            .iter()
            .map(|c| (c.seq, format!("tool:{}", c.tool_use_id))),
    );
    merged.extend(
        projection
            .pending_permissions
            .values()
            .map(|p| (p.seq, format!("perm:{}", p.permission_id))),
    );
    merged.sort_by_key(|(seq, _)| *seq);

    assert_eq!(
        merged.into_iter().map(|(_, label)| label).collect::<Vec<_>>(),
        vec![
            "text:I'll check.".to_string(),
            "tool:toolu_1".to_string(),
            "text:Now the other one.".to_string(),
            "tool:toolu_2".to_string(),
            "perm:perm-1".to_string(),
            "text:Done.".to_string(),
        ],
    );
}

/// A message's `seq` is where it STARTED, not where it was last appended to. Under partial
/// streaming a reply arrives as hundreds of deltas; if `seq` tracked the latest one, a message that
/// was still streaming would keep jumping below the tool call that already interrupted it.
#[test]
fn a_streamed_message_keeps_the_seq_of_its_first_delta() {
    let mut projection = AgentSessionProjection::default();
    projection.apply(&AgentDomainEvent::ContentDelta {
        turn_id: "t1".into(),
        kind: ContentKind::Text,
        text: "The ".into(),
    });
    let first = projection.transcript[0].seq;
    for chunk in ["quick ", "brown ", "fox"] {
        projection.apply(&AgentDomainEvent::ContentDelta {
            turn_id: "t1".into(),
            kind: ContentKind::Text,
            text: chunk.into(),
        });
    }
    assert_eq!(projection.transcript.len(), 1);
    assert_eq!(projection.transcript[0].seq, first);
    assert_eq!(projection.transcript[0].text, "The quick brown fox");
}

/// `seq` is read from `last_revision` BEFORE the bump, so a snapshot's `throughRevision` is a
/// strict upper bound on every `seq` it carries. The frontend seeds its own counter from exactly
/// that number, so an item it folds after a snapshot cannot collide with one from inside it.
#[test]
fn every_seq_is_strictly_below_the_revision_a_snapshot_would_report() {
    let mut projection = AgentSessionProjection::default();
    projection.apply(&AgentDomainEvent::ContentDelta {
        turn_id: "t1".into(),
        kind: ContentKind::Text,
        text: "hi".into(),
    });
    projection.apply(&AgentDomainEvent::ToolCallStarted {
        turn_id: "t1".into(),
        tool_use_id: "toolu_1".into(),
        name: "Bash".into(),
        input: json!({}),
    });
    projection.apply(&AgentDomainEvent::PermissionRequested {
        permission_id: "perm-1".into(),
        tool_use_id: None,
        tool_name: "Bash".into(),
        input: json!({}),
    });

    let highest = projection
        .transcript
        .iter()
        .map(|m| m.seq)
        .chain(projection.tool_calls.iter().map(|c| c.seq))
        .chain(projection.pending_permissions.values().map(|p| p.seq))
        .max()
        .unwrap();
    assert!(
        highest < projection.last_revision,
        "seq {highest} must be below throughRevision {}",
        projection.last_revision,
    );
}

/// The half of `apply`'s stated ordering invariant that nothing pinned: "no single event creates
/// more than one item".
///
/// Its sibling -- every `seq` below `last_revision` -- has the test above. This one matters more,
/// because breaking it fails QUIETLY: `seq` is read once per `apply` call, so an arm that pushed
/// two items would hand both the same number, and every consumer that sorts on `seq`
/// (`agent-ui`'s `buildTimeline`, and the snapshot merge in `core/src/agent_bridge.rs`'s tests)
/// would then tie them in whatever order the arrays happened to be in. A mis-ordered pair, not a
/// failure -- the exact class of thing that survives one refactor and dies in the next.
///
/// Checked after EVERY call rather than once at the end, because a later `PermissionResolved`
/// removes an item and could carry the collision away with it.
#[test]
fn no_single_event_ever_creates_more_than_one_item() {
    let mut projection = AgentSessionProjection::default();
    let mut item_count = 0usize;

    for event in every_event_variant() {
        projection.apply(&event);

        let mut seqs: Vec<u64> = projection
            .user_prompts
            .iter()
            .map(|p| p.seq)
            .chain(projection.transcript.iter().map(|m| m.seq))
            .chain(projection.tool_calls.iter().map(|c| c.seq))
            .chain(projection.pending_permissions.values().map(|p| p.seq))
            .collect();
        let before_dedup = seqs.len();
        seqs.sort_unstable();
        seqs.dedup();
        assert_eq!(
            seqs.len(),
            before_dedup,
            "two items share a seq after applying {}; `seq` is assigned once per apply call, so \
             this means one event created more than one item",
            label(&event),
        );

        assert!(
            before_dedup <= item_count + 1,
            "applying {} grew the three collections by {}; at most one item per event is what makes \
             seq a total order",
            label(&event),
            before_dedup - item_count,
        );
        item_count = before_dedup;
    }
}

/// The panel currently has no way to show what the user asked -- `transcript` is assistant text
/// only. This pins the projection half of the fix: a submitted prompt is its own item, ordered
/// against `transcript` by the same `seq`, and (just as important) it closes whatever assistant
/// message was open so the next turn's reply does not get appended to the previous one's.
#[test]
fn a_user_prompt_is_an_item_of_its_own_and_closes_an_open_assistant_message() {
    let mut projection = AgentSessionProjection::default();
    projection.apply(&AgentDomainEvent::ContentDelta {
        turn_id: "t1".into(),
        kind: ContentKind::Text,
        text: "first".into(),
    });
    projection.apply(&AgentDomainEvent::UserPromptSubmitted {
        text: "and now this".into(),
    });
    projection.apply(&AgentDomainEvent::ContentDelta {
        turn_id: "t2".into(),
        kind: ContentKind::Text,
        text: "second".into(),
    });

    assert_eq!(projection.user_prompts.len(), 1);
    assert_eq!(projection.user_prompts[0].text, "and now this");
    // Two SEPARATE assistant messages: a prompt between them must break the run, or the reply to
    // turn 2 is appended to the reply to turn 1 and rendered as one bubble.
    assert_eq!(projection.transcript.len(), 2, "{:?}", projection.transcript);
    assert!(projection.user_prompts[0].seq > projection.transcript[0].seq);
    assert!(projection.user_prompts[0].seq < projection.transcript[1].seq);
}

/// One of every `AgentDomainEvent` variant, in an order that exercises the arms that interact
/// (a tool call and its completion; a permission and its resolution; text on both sides of the
/// break a tool call forces).
fn every_event_variant() -> Vec<AgentDomainEvent> {
    vec![
        AgentDomainEvent::SessionOpened {
            session_id: "sess-1".into(),
            provider_session_id: "prov-1".into(),
            model: "claude-sonnet-5".into(),
            cwd: "/tmp/project".into(),
        },
        AgentDomainEvent::TurnStarted { turn_id: "t1".into() },
        AgentDomainEvent::UserPromptSubmitted {
            text: "what does this do?".into(),
        },
        AgentDomainEvent::ContentDelta {
            turn_id: "t1".into(),
            kind: ContentKind::Text,
            text: "I'll check.".into(),
        },
        AgentDomainEvent::ContentDelta {
            turn_id: "t1".into(),
            kind: ContentKind::Thinking,
            text: "hmm".into(),
        },
        AgentDomainEvent::AssistantMessageBoundary { turn_id: "t1".into() },
        AgentDomainEvent::ToolCallStarted {
            turn_id: "t1".into(),
            tool_use_id: "toolu_1".into(),
            name: "Bash".into(),
            input: json!({}),
        },
        AgentDomainEvent::ToolCallCompleted {
            turn_id: "t1".into(),
            tool_use_id: "toolu_1".into(),
            content: json!("ok"),
            is_error: false,
        },
        AgentDomainEvent::PermissionRequested {
            permission_id: "perm-1".into(),
            tool_use_id: Some("toolu_1".into()),
            tool_name: "Bash".into(),
            input: json!({}),
        },
        AgentDomainEvent::ContentDelta {
            turn_id: "t1".into(),
            kind: ContentKind::Text,
            text: "Done.".into(),
        },
        AgentDomainEvent::PermissionResolved {
            permission_id: "perm-1".into(),
            outcome: PermissionOutcome::Allowed,
        },
        AgentDomainEvent::TurnCompleted {
            turn_id: "t1".into(),
            outcome: TurnOutcome::Completed,
            result_text: "Done.".into(),
            stop_reason: None,
            usage: Some(UsageInfo {
                total_cost_usd: 0.01,
                num_turns: 1,
            }),
        },
        AgentDomainEvent::ResumeOutcome {
            requested_provider_session_id: "prov-1".into(),
            status: agent::ResumeStatus::Attached,
            attached_provider_session_id: Some("prov-1".into()),
            forked: false,
            detail: None,
        },
        AgentDomainEvent::SessionUnavailable {
            reason: "provider exited".into(),
        },
        AgentDomainEvent::SessionClosed {
            reason: "closed_by_host".into(),
        },
    ]
}

/// A name per variant, for the assertion messages -- and, more usefully, an exhaustive `match` with
/// no `_` arm, so a new `AgentDomainEvent` variant stops this test COMPILING. Whoever adds one then
/// has to decide whether it creates an item and add it to `every_event_variant` above; the compiler
/// cannot force the second half, so this comment is the reminder.
fn label(event: &AgentDomainEvent) -> &'static str {
    match event {
        AgentDomainEvent::SessionOpened { .. } => "SessionOpened",
        AgentDomainEvent::TurnStarted { .. } => "TurnStarted",
        AgentDomainEvent::UserPromptSubmitted { .. } => "UserPromptSubmitted",
        AgentDomainEvent::ContentDelta {
            kind: ContentKind::Text,
            ..
        } => "ContentDelta(Text)",
        AgentDomainEvent::ContentDelta {
            kind: ContentKind::Thinking,
            ..
        } => "ContentDelta(Thinking)",
        AgentDomainEvent::AssistantMessageBoundary { .. } => "AssistantMessageBoundary",
        AgentDomainEvent::ToolCallStarted { .. } => "ToolCallStarted",
        AgentDomainEvent::ToolCallCompleted { .. } => "ToolCallCompleted",
        AgentDomainEvent::PermissionRequested { .. } => "PermissionRequested",
        AgentDomainEvent::PermissionResolved { .. } => "PermissionResolved",
        AgentDomainEvent::TurnCompleted { .. } => "TurnCompleted",
        AgentDomainEvent::ResumeOutcome { .. } => "ResumeOutcome",
        AgentDomainEvent::SessionUnavailable { .. } => "SessionUnavailable",
        AgentDomainEvent::SessionClosed { .. } => "SessionClosed",
    }
}
