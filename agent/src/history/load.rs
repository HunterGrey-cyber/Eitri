// agent/src/history/load.rs
//! Turning one of the two stored records into the projection a resumed conversation starts from.
//!
//! The load happens in one place, through one allocator; when B, Claude's own transcript, cannot be
//! read it falls to A, Eitri's copy; and B wins whenever it can be read.
//!
//! Four properties this module holds, each with its reason:
//!
//! 1. **There is exactly one `seq` allocator, and it is `AgentSessionProjection::apply`.** Both
//!    sources are REFOLDED: B's parser already produces `AgentDomainEvent`s, and A's stored
//!    collections are merged by their on-disk `seq` and turned back into events here. A stored
//!    `seq` is a sort key and is never written back into the projection, so "no item has a number
//!    that something other than `apply` chose" holds by construction rather than by convention.
//! 2. **The seed is complete before the first live event exists.** `ConversationIngest::start`
//!    takes it as a parameter and spawns its thread afterwards, so every live `seq` is greater than
//!    every historical one with no renumbering, no offset, and nothing for the frontend to change.
//! 3. **`assistant_message_open` is false and no turn is active when this module returns.**
//!    Otherwise the first live reply is appended to the last historical assistant message and the
//!    two render as one, or the first prompt is refused as a turn already running: a restored turn
//!    is a closed turn, and both sources close every turn they start.
//! 4. **B wins whenever it can be read at all, and a fall to A is always said out loud.** No
//!    merging, no difference detection, no silent degradation: the two records can genuinely
//!    differ (the CLI compacts its own context and Eitri's copy does not), and a user with
//!    `claude --resume` open in a terminal beside this panel must be able to see which one they are
//!    looking at.
//!
//! Nothing read here is evidence: a restored item feeds the panel and nothing else. It never
//! reaches a permission decision, is never composed into a turn, and is never written back to disk.

use std::path::{Path, PathBuf};

use crate::projection::{
    AgentDomainEvent, AgentSessionProjection, ContentKind, HistoryNotice, HistorySource, NoticeRecord, PlanOutcome,
    PlanRecord, PlanState, QuestionOutcome, QuestionRecord, QuestionState, ToolCallRecord, TranscriptMessage,
    TurnEndDetail, TurnOutcome, TurnStartRecord, UserPromptRecord,
};

use super::store::{self, HistoryLoadError, StoredHistory};
use super::transcript_jsonl::{read_history_from_tail, read_transcript_history, TranscriptHistory};

/// The projection a resumed conversation starts from.
///
/// Called from `AgentConversation::resume` after the lease is held and before the provider is asked
/// for anything: the stability probe has just confirmed nothing else is writing the
/// transcript, the lease means no other Eitri window is driving this session, and the provider has
/// not yet produced a single byte.
///
/// Never fails. Every way of not getting history -- no transcript, no permission, a format nothing
/// here recognises, no stored copy either -- ends in a projection that is merely emptier, because a
/// resume that works is worth more than a history that is complete.
pub(crate) fn seed_for_resume(cwd: &str, conversation_id: &str, provider_session_id: &str) -> AgentSessionProjection {
    seed_from(
        crate::transcript::transcript_path(cwd, provider_session_id),
        conversation_id,
        provider_session_id,
    )
}

/// [`seed_for_resume`] with the transcript path handed in.
///
/// Split out for one reason: a test that drives the B-fails-to-A decision by setting
/// `CLAUDE_CONFIG_DIR` would be mutating a process-wide environment variable inside a
/// multi-threaded test binary. Environment-dependent tests run in a child through
/// `crate::testkit::environment::in_child`; passing the path makes every branch reachable with an
/// ordinary temporary file.
pub(crate) fn seed_from(
    transcript_path: std::io::Result<PathBuf>,
    conversation_id: &str,
    provider_session_id: &str,
) -> AgentSessionProjection {
    seed_with(read_b(transcript_path), conversation_id, provider_session_id)
}

/// The projection a conversation resumed on another machine starts from, built from the transcript's bytes: the last
/// bytes of the file (the whole file when `started_mid_file` is false). The same fold, the same notes and the same
/// fall to Eitri's own copy as [`seed_from`]; a tail that starts mid file drops its first line, which is partial.
/// `source_label` names the transcript in the notice (`source_path`, and `attempted_transcript_path` when the fall
/// happens) as there is no local path: the caller says where the bytes came from, host included.
pub fn seed_from_tail(
    tail: &[u8],
    started_mid_file: bool,
    conversation_id: &str,
    provider_session_id: &str,
    source_label: &str,
) -> AgentSessionProjection {
    let path = PathBuf::from(source_label);
    let attempt = if tail.is_empty() {
        BOutcome::Loaded {
            path,
            history: empty_transcript_history(),
        }
    } else {
        judge(path, read_history_from_tail(tail, started_mid_file))
    };
    seed_with(attempt, conversation_id, provider_session_id)
}

/// The projection a conversation resumed on another machine starts from when that machine's transcript could not be
/// had (it is missing, was refused or did not arrive): Eitri's own copy, with `reason` and `source_label` in the
/// notice as the fall in [`seed_from`] says them. The same fall as a transcript that holds nothing usable.
pub fn seed_from_stored(
    conversation_id: &str,
    provider_session_id: &str,
    source_label: &str,
    reason: &str,
) -> AgentSessionProjection {
    let attempt = BOutcome::Failed {
        path: Some(PathBuf::from(source_label)),
        reason: reason.to_string(),
    };
    seed_with(attempt, conversation_id, provider_session_id)
}

/// What [`seed_from`] and [`seed_from_tail`] share once B has been read or has failed.
fn seed_with(attempt: BOutcome, conversation_id: &str, provider_session_id: &str) -> AgentSessionProjection {
    let mut projection = AgentSessionProjection::default();
    match attempt {
        BOutcome::Loaded { path, history } => {
            load_transcript(&mut projection, &path, history);
        }
        BOutcome::Failed { path, reason } => {
            eprintln!(
                "agent: Claude's own transcript for session {provider_session_id} was not used \
                 ({reason}); falling back to Eitri's own copy"
            );
            load_stored(&mut projection, conversation_id, provider_session_id, path, reason);
        }
    }
    // The last thing this module does on every path. A history whose final item is
    // assistant text leaves this flag true, and the first live delta would then be appended to that
    // message -- last session's reply and this one's rendered as a single bubble.
    projection.assistant_message_open = false;
    // Neither source restores thinking today, so this is already false; stated so a later source
    // that does cannot leave a restored thought open for the first live one to be appended to.
    projection.thinking_open = false;
    // Every turn a source restores is over: the process that ran it is gone. Both sources close the
    // turns they start; this holds even for one that did not, so the first prompt sent after a
    // resume is never refused as a turn already running.
    projection.active_turn_id = None;
    projection
}

/// How many conversational items a projection is holding -- the one basis both paths report.
///
/// **Counted AFTER the fold, and that is the whole point.** Both sources hand `apply` a stream of
/// events, and `apply` coalesces consecutive assistant text into a single `TranscriptMessage`
/// (`projection.rs`), so the number of events, slots or stored records that went in is not the
/// number of rows that come out. Reporting the input count was a real defect, found on
/// 2026-09-20: over all 45 real top-level transcripts in the measured corpus, 13 disagreed with
/// what the projection actually held (worst case 400 against 384), and the two paths were reporting
/// two different quantities under one field name -- B its pre-fold slot count, A its post-truncation
/// stored-record count. Taking both from here means the number the panel prints is the number of
/// rows the panel draws, by construction rather than by coincidence.
///
/// Called as a before/after pair rather than once at the end: a seed is built on a fresh projection
/// today, and a difference stays right if that ever stops being true.
///
/// **`omitted_items` is deliberately NOT in this unit**, and the notice's sentence therefore mixes
/// two: restored is counted in rows, omitted in items that were dropped before any fold could see
/// them. There is no honest way to fold what was discarded -- whether two evicted assistant slots
/// would have merged depends on neighbours that were also evicted -- so the choice is between a
/// dropped-item count that is exact in its own unit and a row estimate that is not exact in any.
fn restorable_item_count(projection: &AgentSessionProjection) -> usize {
    projection.user_prompts.len()
        + projection.transcript.len()
        + projection.tool_calls.len()
        + projection.turn_starts.len()
        + projection.notices.len()
        + projection.questions.len()
        + projection.plans.len()
}

/// What came of trying to read the Claude CLI's own transcript.
enum BOutcome {
    /// The file was read. `history` may still be empty, which is a real answer: a session that
    /// never said anything has nothing to restore and is NOT a reason to reach for the fallback.
    Loaded { path: PathBuf, history: TranscriptHistory },
    /// B is not usable. `reason` is written for the user, not for the log -- it ends up in the
    /// notice row. `path` is `None` only when no path could be built at all.
    Failed { path: Option<PathBuf>, reason: String },
}

/// Which way of not reading B counts as unavailable, as failure or as success, and which says so.
///
/// The one case worth reading twice is the middle: a file that exists, holds bytes, and yields no
/// items at all is treated as a FAILURE rather than as an empty conversation. The transcript
/// carries no schema version anywhere (measured: none of the 15 line types has one), so "this build
/// no longer understands this format" has no direct expression on disk and can only be observed as
/// "nothing parsed". An empty file is different and is not failure: zero bytes is zero
/// conversation, unambiguously.
fn read_b(transcript_path: std::io::Result<PathBuf>) -> BOutcome {
    let path = match transcript_path {
        Ok(path) => path,
        Err(e) => {
            return BOutcome::Failed {
                path: None,
                reason: format!("Claude's transcript directory could not be located ({e})"),
            }
        }
    };
    let metadata = match std::fs::metadata(&path) {
        Ok(metadata) => metadata,
        Err(e) => {
            let reason = describe_io_error(&e);
            return BOutcome::Failed {
                path: Some(path),
                reason,
            };
        }
    };
    // **Refused before it is opened, and this is a hang fix rather than a tidiness rule.**
    // `File::open` on a FIFO blocks in `open(2)` until a writer appears, and this call sits on the
    // backend-start worker, which `collect_pending_starts` polls with `try_recv` and no timeout
    // while the window is open (`panel/src/starts.rs`) -- the 3-second bound exists only on
    // the close path. So a FIFO, a device node or a directory at this path would leave the panel in
    // its connecting state for the life of the window, with nothing printed and no way back to the
    // start screen. A `metadata` call does not block (it is a `stat`), so asking first turns a hang
    // into the ordinary reported fallback every other unusable transcript already gets.
    if !metadata.is_file() {
        return BOutcome::Failed {
            path: Some(path),
            reason: "the transcript path is not a regular file".to_string(),
        };
    }
    if metadata.len() == 0 {
        return BOutcome::Loaded {
            path,
            history: empty_transcript_history(),
        };
    }
    judge(path.clone(), read_transcript_history(&path))
}

/// Whether what was read of the transcript at `path` is usable: see [`read_b`] for the cases.
fn judge(path: PathBuf, read: std::io::Result<TranscriptHistory>) -> BOutcome {
    match read {
        Ok(history) if history.parsed_items == 0 => {
            // **Says what was observed, not what it might mean.** This arm is the "the format
            // may have changed" signal, but it is reachable with no format change at all: a
            // session whose `type:"user"` lines are all slash-command echoes, `isMeta` injections
            // or a compaction summary is correctly rejected by the allow-list and yields nothing
            // from a perfectly healthy file. The earlier wording ("no records could be parsed")
            // rode a sentence beginning "Claude's transcript could not be read", which sent a user
            // to inspect a file that is fine. The version stays: it is still the first thing
            // somebody debugging a real format change wants, and it costs nothing when there was
            // no format change.
            let reason = match &history.writer_version {
                Some(version) => format!("nothing in it could be restored (written by claude {version})"),
                None => "nothing in it could be restored".to_string(),
            };
            BOutcome::Failed {
                path: Some(path),
                reason,
            }
        }
        Ok(history) => BOutcome::Loaded { path, history },
        Err(e) => {
            let reason = describe_io_error(&e);
            BOutcome::Failed {
                path: Some(path),
                reason,
            }
        }
    }
}

/// The three wordings a user is shown, and a fall-through that carries the real error rather than
/// flattening every other failure into one of them.
fn describe_io_error(e: &std::io::Error) -> String {
    match e.kind() {
        std::io::ErrorKind::NotFound => "transcript file not found".to_string(),
        std::io::ErrorKind::PermissionDenied => "permission denied".to_string(),
        _ => format!("the transcript could not be read ({e})"),
    }
}

fn empty_transcript_history() -> TranscriptHistory {
    TranscriptHistory {
        events: Vec::new(),
        parsed_items: 0,
        omitted_items: Some(0),
        writer_version: None,
        counts: super::transcript_jsonl::HistoryCounts::default(),
    }
}

/// Folds B in, and records where it came from.
///
/// A transcript that parsed cleanly but holds no conversation leaves no notice: there is nothing to
/// explain, and a row saying "no earlier messages" on a session that genuinely had none is noise.
fn load_transcript(projection: &mut AgentSessionProjection, path: &Path, history: TranscriptHistory) {
    if history.parsed_items == 0 {
        // Logged from here rather than before the check, so the one line can carry the folded row
        // count as well -- there is none in this branch, and that is what it says.
        log_counts(path, &history, None);
        return;
    }
    let before = restorable_item_count(projection);
    for event in &history.events {
        projection.apply(event);
    }
    let restored_items = restorable_item_count(projection) - before;
    log_counts(path, &history, Some(restored_items));
    projection.history = Some(HistoryNotice {
        source: HistorySource::ClaudeTranscript,
        restored_items,
        omitted_items: history.omitted_items,
        omitted_turns: None,
        upto_seq: projection.last_revision,
        source_path: path.to_string_lossy().to_string(),
        // Would repeat `source_path`: the transcript is what was read.
        attempted_transcript_path: None,
        fallback_reason: None,
        writer_version: history.writer_version,
    });
}

/// Folds A in, when B could not be used.
///
/// The caps are applied HERE as well as at write time, through the same `from_parts` the
/// writer uses. Not belt-and-braces: a file written by a build with different constants, or by a
/// future one, would otherwise decide this process's memory and this panel's length. Re-truncating
/// through the writer's own function is also what makes `omitted_items` a real number on this path
/// rather than a guess -- it is what this load dropped, counted, and nothing else.
fn load_stored(
    projection: &mut AgentSessionProjection,
    conversation_id: &str,
    provider_session_id: &str,
    attempted_transcript_path: Option<PathBuf>,
    fallback_reason: String,
) {
    let stored = match store::load(conversation_id, provider_session_id) {
        Ok(stored) => stored,
        // The ordinary case, not an error: a session that never reached a turn boundary has no
        // stored copy: no notice, no row, the panel is simply the one it is today.
        Err(HistoryLoadError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => return,
        Err(e) => {
            eprintln!("agent: Eitri's own history for session {provider_session_id} could not be read: {e}");
            return;
        }
    };
    let Ok(path) = store::history_path(conversation_id, provider_session_id) else {
        return;
    };
    let before = stored.item_count();
    let (conversation_id, provider_session_id) = (stored.conversation_id.clone(), stored.provider_session_id.clone());
    let capped = StoredHistory::from_parts(conversation_id, provider_session_id, stored.into_parts());
    let kept = capped.item_count();
    if kept == 0 {
        return;
    }
    let items_before = restorable_item_count(projection);
    let refold = stored_events(&capped);
    let mut question_turns = refold.question_turns.into_iter();
    let mut plan_turns = refold.plan_turns.into_iter();
    for event in refold.events {
        projection.apply(&event);
        match event {
            // The reason A cannot simply be handed to `apply` as a stream: each
            // stored `TranscriptMessage` is an already-COALESCED assistant message, and `apply` closes
            // a run only on a prompt or a turn boundary -- neither of which A stores. Two adjacent
            // stored messages would merge back into one. Closing after every one keeps them apart.
            AgentDomainEvent::ContentDelta {
                kind: ContentKind::Text,
                ..
            } => projection.assistant_message_open = false,
            // `QuestionAsked` names no turn: a live record takes the turn running when it was asked.
            // The refold cannot stand in for that turn -- A stores no start for a turn of the user's
            // own, and keeps a turn of the CLI's open until the next start or prompt -- so the
            // record takes back the turn it was stored with. The fold has just pushed it.
            AgentDomainEvent::QuestionAsked { .. } => {
                if let Some(record) = projection.questions.last_mut() {
                    record.turn_id = question_turns.next().flatten();
                }
            }
            // Likewise a plan: `PlanProposed` names no turn either.
            AgentDomainEvent::PlanProposed { .. } => {
                if let Some(record) = projection.plans.last_mut() {
                    record.turn_id = plan_turns.next().flatten();
                }
            }
            _ => {}
        }
    }
    crate::projection::restore_children(projection, &capped.tool_calls);
    projection.history = Some(HistoryNotice {
        source: HistorySource::EitriCopy,
        restored_items: restorable_item_count(projection) - items_before,
        // Counted in STORED items, not in rows: it is what this load's own truncation dropped,
        // and a dropped item has no row to be counted as. See `restorable_item_count`'s note on
        // why the two numbers in one sentence are in different units and why that is the honest
        // pair rather than a mistake.
        omitted_items: Some(before - kept),
        omitted_turns: None,
        upto_seq: projection.last_revision,
        source_path: path.to_string_lossy().to_string(),
        attempted_transcript_path: attempted_transcript_path.map(|p| p.to_string_lossy().to_string()),
        fallback_reason: Some(fallback_reason),
        // A has no writer version: it is this project's own file, and its format is pinned by
        // `HISTORY_FORMAT_VERSION`, which `store::load` has already checked.
        writer_version: None,
    });
}

/// A's collections, merged by their stored `seq` and turned back into the events that produced
/// them.
///
/// The stored `seq` is used HERE and only here: as the sort key that recovers the one order the
/// collections were written in. The numbers themselves are then thrown away -- `apply` assigns new
/// ones, contiguous and this process's own (one allocator).
///
/// Restored turns are closed turns: a stored turn start becomes its `TurnStarted` and attribution,
/// and the turn ends, completed, before the next turn start or the next prompt of the user's own
/// (a delivered prompt was read inside the running turn and does not end it), or after the last
/// item. A question or a plan stored while it waited is settled `SessionClosed`: nothing can answer it now.
///
/// `question_turns` and `plan_turns` hold each refolded question's and plan's stored turn, in the order of
/// its `QuestionAsked` or `PlanProposed` in `events`: the event carries none, and the folder puts it back on
/// the record.
fn stored_events(stored: &StoredHistory) -> Refold {
    enum Item<'a> {
        Prompt(&'a UserPromptRecord),
        Message(&'a TranscriptMessage),
        Call(&'a ToolCallRecord),
        TurnStart(&'a TurnStartRecord),
        Notice(&'a NoticeRecord),
        Question(&'a QuestionRecord),
        Plan(&'a PlanRecord),
    }
    let mut items: Vec<(u64, u8, Item<'_>)> = Vec::new();
    for prompt in &stored.user_prompts {
        items.push((prompt.seq, 0, Item::Prompt(prompt)));
    }
    for message in &stored.transcript {
        items.push((message.seq, 1, Item::Message(message)));
    }
    for call in &stored.tool_calls {
        items.push((call.seq, 2, Item::Call(call)));
    }
    for start in &stored.turn_starts {
        items.push((start.seq, 3, Item::TurnStart(start)));
    }
    for notice in &stored.notices {
        items.push((notice.seq, 4, Item::Notice(notice)));
    }
    for question in &stored.questions {
        items.push((question.seq, 5, Item::Question(question)));
    }
    for plan in &stored.plans {
        items.push((plan.seq, 6, Item::Plan(plan)));
    }
    // A stable sort on (seq, kind). Items of different kinds can share a `seq`: a question the CLI
    // asked before announcing the turn it woke for takes the `seq` of the call it asks about once that
    // call starts, so the call (kind 2) is folded first and its question (kind 5) after it. Two items
    // of one kind do not share one in a file this build wrote, but a corrupt or hand-edited file could
    // have it, and "whatever order the vectors happened to be in" is not an answer a reader can
    // reproduce. Code that keys or dedupes stored items by `seq` alone would lose the question.
    items.sort_by_key(|(seq, kind, _)| (*seq, *kind));

    let mut events = Vec::new();
    let mut question_turns = Vec::new();
    let mut plan_turns = Vec::new();
    let mut open_turn: Option<String> = None;
    for (_, _, item) in items {
        match item {
            Item::Prompt(prompt) if prompt.delivered => events.push(AgentDomainEvent::UserMessageDelivered {
                // A stored prompt keeps no client id; this one names its place and matches no message
                // a host is waiting on.
                client_message_id: format!("history-{}", prompt.seq),
                turn_id: String::new(),
                text: prompt.text.clone(),
            }),
            Item::Prompt(prompt) => {
                close_turn(&mut events, &mut open_turn);
                events.push(AgentDomainEvent::UserPromptSubmitted {
                    text: prompt.text.clone(),
                });
                // What the images were (type, size, digest) comes back with the words they went with; their
                // bytes were never stored.
                if !prompt.attachments.is_empty() {
                    events.push(AgentDomainEvent::UserPromptAttachments {
                        attachments: prompt.attachments.clone(),
                    });
                }
            }
            Item::Message(message) => events.push(AgentDomainEvent::ContentDelta {
                // A stores no turn ids for replies; its stored message boundaries keep replies apart.
                // B instead reconstructs turn ids from the transcript's segment openers.
                turn_id: String::new(),
                kind: ContentKind::Text,
                text: message.text.clone(),
            }),
            Item::TurnStart(start) => {
                close_turn(&mut events, &mut open_turn);
                events.push(AgentDomainEvent::TurnStarted {
                    turn_id: start.turn_id.clone(),
                });
                events.push(AgentDomainEvent::TurnAttributed {
                    turn_id: start.turn_id.clone(),
                    origin: start.origin,
                    cause: start.cause.clone(),
                });
                open_turn = Some(start.turn_id.clone());
            }
            Item::Notice(notice) => events.push(AgentDomainEvent::SessionNotice {
                kind: notice.kind,
                level: notice.level,
                content: notice.content.clone(),
                scalars: notice.scalars.clone(),
                task_id: notice.task_id.clone(),
                tool_use_id: notice.tool_use_id.clone(),
                task_status: notice.task_status.clone(),
            }),
            Item::Question(question) => {
                question_turns.push(question.turn_id.clone());
                events.push(AgentDomainEvent::QuestionAsked {
                    interaction_id: question.interaction_id.clone(),
                    tool_use_id: question.tool_use_id.clone(),
                    questions: question.questions.clone(),
                    unsupported_reason: question.unsupported_reason.clone(),
                });
                let (outcome, answers, reason) = match &question.state {
                    QuestionState::Settled {
                        outcome,
                        answers,
                        reason,
                    } => (*outcome, answers.clone(), reason.clone()),
                    QuestionState::Waiting | QuestionState::Answering => {
                        (QuestionOutcome::SessionClosed, Vec::new(), None)
                    }
                };
                events.push(AgentDomainEvent::QuestionSettled {
                    interaction_id: question.interaction_id.clone(),
                    outcome,
                    answers,
                    reason,
                });
            }
            Item::Plan(plan) => {
                plan_turns.push(plan.turn_id.clone());
                events.push(AgentDomainEvent::PlanProposed {
                    interaction_id: plan.interaction_id.clone(),
                    tool_use_id: plan.tool_use_id.clone(),
                    plan: plan.plan.clone(),
                    plan_sha256: plan.plan_sha256.clone(),
                    allowed_prompts: plan.allowed_prompts.clone(),
                    unsupported_reason: plan.unsupported_reason.clone(),
                });
                // Never re-armed: a plan the stored copy held as waiting is closed, and a settled one reads as
                // it was settled. Its approval's `mode_after` stays the record's own; nothing here moves a mode.
                let (outcome, mode_after, feedback, reason) = match &plan.state {
                    PlanState::Settled {
                        outcome,
                        mode_after,
                        feedback,
                        reason,
                    } => (*outcome, *mode_after, feedback.clone(), reason.clone()),
                    PlanState::Waiting | PlanState::Answering => (PlanOutcome::SessionClosed, None, None, None),
                };
                events.push(AgentDomainEvent::PlanSettled {
                    interaction_id: plan.interaction_id.clone(),
                    outcome,
                    mode_after,
                    feedback,
                    reason,
                });
            }
            Item::Call(call) => {
                events.push(AgentDomainEvent::ToolCallStarted {
                    turn_id: call.turn_id.clone(),
                    tool_use_id: call.tool_use_id.clone(),
                    name: call.name.clone(),
                    input: call.input.clone(),
                });
                // Before the completion-skip below: the mark belongs to the call, not to its result,
                // and an empty id is no link (a held change for it could never be attached).
                if let Some(change) = call.changed_input.as_ref().filter(|_| !call.tool_use_id.is_empty()) {
                    events.push(AgentDomainEvent::ToolInputChanged {
                        tool_use_id: call.tool_use_id.clone(),
                        change: change.clone(),
                    });
                }
                // The CLI's refusal, on the call it refused; an empty id is no link here either.
                if let Some(denied) = call.denied.as_ref().filter(|_| !call.tool_use_id.is_empty()) {
                    events.push(AgentDomainEvent::PermissionDenied {
                        tool_use_id: Some(call.tool_use_id.clone()),
                        tool_name: call.name.clone(),
                        reason_type: denied.reason_type.clone(),
                        reason: denied.reason.clone(),
                    });
                }
                if let Some(result) = &call.result {
                    // An empty id is not a link (`projection::tool_use_link`'s whole subject): a
                    // completion carrying one would be matched by `apply` against the FIRST call
                    // whose id is also empty, attaching a result to an unrelated call. Dropping the
                    // completion loses a result; keeping it would invent one somewhere else.
                    if call.tool_use_id.is_empty() {
                        continue;
                    }
                    events.push(AgentDomainEvent::ToolCallCompleted {
                        turn_id: call.turn_id.clone(),
                        tool_use_id: call.tool_use_id.clone(),
                        content: result.content.clone(),
                        is_error: result.is_error,
                    });
                }
            }
        }
    }
    close_turn(&mut events, &mut open_turn);
    Refold {
        events,
        question_turns,
        plan_turns,
    }
}

/// What [`stored_events`] makes of A: the events, and the stored turn of each question and plan among them.
struct Refold {
    events: Vec<AgentDomainEvent>,
    question_turns: Vec<Option<String>>,
    plan_turns: Vec<Option<String>>,
}

/// Ends the restored turn that is open, if one is, as completed: it is over, and how it ended
/// beyond that is not stored.
fn close_turn(events: &mut Vec<AgentDomainEvent>, open_turn: &mut Option<String>) {
    if let Some(turn_id) = open_turn.take() {
        events.push(AgentDomainEvent::TurnCompleted {
            turn_id,
            outcome: TurnOutcome::Completed,
            result_text: String::new(),
            stop_reason: None,
            usage: None,
            detail: TurnEndDetail::default(),
        });
    }
}

/// One line, to stderr, with everything the parse threw away.
///
/// These numbers are deliberately not in the notice: "1,431 records were not loaded" next to
/// a number that also counts attachments and side records would be a figure nobody can explain. In
/// the log they are the first thing a person debugging a format change wants.
///
/// **`parsed` and `restored` are two different numbers and this line names both.** Until
/// 2026-09-20 it printed the parser's own slot count under the word "restored", which is
/// the panel's word for the rows it draws: a transcript of six adjacent assistant lines logged
/// "restored 6 items" while the notice beside it said 1, because consecutive assistant text folds
/// into one row. A debugger reads this line first, so the two must not disagree about a word they
/// share. `restored` is `None` only where nothing was folded at all -- a transcript that parsed to
/// zero items, which leaves no notice either.
fn log_counts(path: &Path, history: &TranscriptHistory, restored: Option<usize>) {
    let counts = history.counts;
    eprintln!(
        "agent: parsed {items} {item_word} from {path} (restored {restored} {row_word}, omitted by truncation: {omitted}, \
         skipped lines: {skipped} [malformed {malformed}, oversized {oversized}, attachment \
         {attachment}, system {system}, side record {side}, subagent {subagent}, unknown type \
         {unknown}, sidechain {sidechain}, rejected user {rejected}, empty prompt {empty}, synthetic {synthetic}], \
         dropped blocks: {blocks}, orphan tool results: {orphans}, written by claude {version})",
        items = history.parsed_items,
        // Agreement, for the same reason the panel's own notice has it: this
        // line is read beside that sentence and "restored 1 rows" in the sentence written to make
        // the numbers honest reads as a line nobody proofread.
        item_word = if history.parsed_items == 1 { "item" } else { "items" },
        path = path.display(),
        restored = match restored {
            Some(n) => n.to_string(),
            None => "no".to_string(),
        },
        // "no rows", not "no row": the plural is right for zero, and `None` means zero here.
        row_word = if restored == Some(1) { "row" } else { "rows" },
        omitted = match history.omitted_items {
            Some(n) => n.to_string(),
            None => "unknown".to_string(),
        },
        skipped = counts.skipped_lines,
        malformed = counts.malformed_lines,
        oversized = counts.oversized_lines,
        attachment = counts.attachment_lines,
        system = counts.system_lines,
        side = counts.side_record_lines,
        subagent = counts.subagent_only_lines,
        unknown = counts.unknown_type_lines,
        sidechain = counts.sidechain_lines,
        rejected = counts.rejected_user_lines,
        empty = counts.empty_prompt_lines,
        synthetic = counts.synthetic_lines,
        blocks = counts.dropped_blocks,
        orphans = counts.orphan_tool_results,
        version = history.writer_version.as_deref().unwrap_or("<unreported>"),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::projection::{ProjectionStatus, ToolCallResult};

    fn setup() -> String {
        crate::state_dirs::redirect_state_to_a_test_root();
        // A fresh conversation id per test: they all share one process-wide state root.
        uuid::Uuid::new_v4().simple().to_string()
    }

    /// A real file under the test root, so nothing lands in `$TMPDIR` and nothing needs
    /// `CLAUDE_CONFIG_DIR`.
    fn write_transcript(lines: &str) -> PathBuf {
        let dir = crate::state_dirs::test_workspace_dir("transcript");
        let path = dir.join("session.jsonl");
        std::fs::write(&path, lines).expect("writing a test transcript should succeed");
        path
    }

    fn missing_transcript() -> PathBuf {
        crate::state_dirs::test_workspace_dir("transcript").join("nothing-here.jsonl")
    }

    fn typed_prompt(text: &str) -> String {
        format!(
            r#"{{"type":"user","uuid":"u-1","version":"2.1.272","promptSource":"typed","message":{{"role":"user","content":{}}}}}"#,
            serde_json::to_string(text).expect("a string serializes")
        )
    }

    fn assistant_text(text: &str) -> String {
        format!(
            r#"{{"type":"assistant","uuid":"a-1","version":"2.1.272","message":{{"role":"assistant","content":[{{"type":"text","text":{}}}]}}}}"#,
            serde_json::to_string(text).expect("a string serializes")
        )
    }

    fn attachment() -> String {
        r#"{"type":"attachment","uuid":"x-1","version":"2.1.272","attachment":{"type":"file"}}"#.to_string()
    }

    fn prompt(seq: u64, text: &str) -> UserPromptRecord {
        UserPromptRecord {
            seq,
            text: text.to_string(),
            delivered: false,
            attachments: Vec::new(),
        }
    }

    fn message(seq: u64, text: &str) -> TranscriptMessage {
        TranscriptMessage {
            seq,
            text: text.to_string(),
        }
    }

    fn tool_call(seq: u64, id: &str, result: Option<&str>) -> ToolCallRecord {
        ToolCallRecord {
            children: None,
            background: false,
            seq,
            turn_id: "t-1".into(),
            tool_use_id: id.into(),
            name: "Read".into(),
            input: serde_json::json!({"file_path": "/p/a.rs"}),
            result: result.map(|text| ToolCallResult {
                content: serde_json::json!(text),
                is_error: false,
            }),
            denied: None,
            changed_input: None,
        }
    }

    /// Puts a stored copy (A) on disk for `conversation_id`/`session_id`.
    fn store_copy(
        conversation_id: &str,
        session_id: &str,
        user_prompts: Vec<UserPromptRecord>,
        transcript: Vec<TranscriptMessage>,
        tool_calls: Vec<ToolCallRecord>,
    ) {
        store::save(&StoredHistory::from_collections(
            conversation_id.to_string(),
            session_id.to_string(),
            user_prompts,
            transcript,
            tool_calls,
        ))
        .expect("writing a test stored history should succeed");
    }

    /// Every `seq` in the projection, in one sorted list -- the total order the panel renders.
    fn all_seqs(projection: &AgentSessionProjection) -> Vec<u64> {
        let mut seqs: Vec<u64> = projection
            .user_prompts
            .iter()
            .map(|p| p.seq)
            .chain(projection.transcript.iter().map(|m| m.seq))
            .chain(projection.tool_calls.iter().map(|c| c.seq))
            .collect();
        seqs.sort_unstable();
        seqs
    }

    // History sorts before live events; both use one sequence allocator.

    /// Invariant 1, end to end: what the session says next sorts after everything it said before,
    /// with no renumbering anywhere. The point of loading BEFORE the ingestion thread starts is
    /// that this needs no arithmetic -- `apply` simply carries on counting.
    #[test]
    fn every_live_seq_sits_above_every_restored_one() {
        let conversation_id = setup();
        let path = write_transcript(&format!(
            "{}\n{}\n{}\n",
            typed_prompt("first question"),
            assistant_text("first answer"),
            typed_prompt("second question")
        ));

        let mut projection = seed_from(Ok(path), &conversation_id, "sess-b");
        let notice = projection.history.clone().expect("history was restored");
        assert_eq!(notice.restored_items, 3);
        let restored = all_seqs(&projection);
        assert!(
            restored.iter().all(|seq| *seq < notice.upto_seq),
            "restored seqs {restored:?} must all sit below uptoSeq {}",
            notice.upto_seq
        );

        projection.apply(&AgentDomainEvent::ContentDelta {
            turn_id: "t-live".into(),
            kind: ContentKind::Text,
            text: "a live answer".into(),
        });
        let live_seq = projection.transcript.last().expect("a live message").seq;
        assert!(
            live_seq >= notice.upto_seq,
            "a live seq {live_seq} must not fall inside the restored range (< {})",
            notice.upto_seq
        );
        // And the whole order is still strictly increasing with no duplicates, which is what the
        // frontend's merge sorts on.
        let everything = all_seqs(&projection);
        assert!(
            everything.windows(2).all(|w| w[0] < w[1]),
            "seqs must be unique: {everything:?}"
        );
    }

    /// Invariant 2: the numbers on disk are a sort key and nothing else. These stored items carry
    /// seqs from a previous session's counter; the restored ones start at 0 because `apply` -- the
    /// only allocator -- assigned them here.
    #[test]
    fn a_stored_seq_is_a_sort_key_and_is_never_reinstalled() {
        let conversation_id = setup();
        store_copy(
            &conversation_id,
            "sess-a",
            vec![prompt(904, "asked last"), prompt(900, "asked first")],
            vec![message(902, "answered")],
            vec![],
        );

        let projection = seed_from(Ok(missing_transcript()), &conversation_id, "sess-a");
        assert_eq!(all_seqs(&projection), vec![0, 1, 2]);
        // Sorted by the stored seq, not by the order the collections happened to be in.
        assert_eq!(
            projection
                .user_prompts
                .iter()
                .map(|p| p.text.as_str())
                .collect::<Vec<_>>(),
            vec!["asked first", "asked last"]
        );
        assert_eq!(projection.user_prompts[0].seq, 0);
        assert_eq!(projection.transcript[0].seq, 1);
        assert_eq!(projection.user_prompts[1].seq, 2);
    }

    /// A call's changed-input mark was stored with it, and a restore from Eitri's own copy (the
    /// transcript unavailable) brings it back; a call stored without one stays unmarked.
    #[test]
    fn a_changed_input_mark_survives_a_restore_from_the_stored_copy() {
        let conversation_id = setup();
        let change = crate::InputChange {
            fields: serde_json::json!({ "command": "rtk ls" }).as_object().unwrap().clone(),
            removed: vec!["timeout".into()],
            clipped: true,
        };
        let mut marked = tool_call(1, "toolu_marked", Some("ok"));
        marked.changed_input = Some(change.clone());
        store_copy(
            &conversation_id,
            "sess-mark",
            vec![prompt(0, "go")],
            vec![],
            vec![marked, tool_call(2, "toolu_plain", Some("ok")), {
                // An empty id is no link: it neither gets a result nor a held mark.
                let mut anonymous = tool_call(3, "", None);
                anonymous.changed_input = Some(change.clone());
                anonymous
            }],
        );

        let projection = seed_from(Ok(missing_transcript()), &conversation_id, "sess-mark");
        assert_eq!(projection.tool_calls.len(), 3);
        assert_eq!(projection.tool_calls[0].changed_input, Some(change));
        assert_eq!(projection.tool_calls[1].changed_input, None);
        assert_eq!(projection.tool_calls[2].changed_input, None);
        assert!(projection.changed_inputs_before_their_call.is_empty());
        // And it went to disk in the shape the loader reads (a round trip through the real store).
        let again = store::load(&conversation_id, "sess-mark").expect("stored history");
        assert!(again.tool_calls[0].changed_input.is_some());
    }

    // Closing `assistant_message_open` keeps restored and live replies separate.

    /// The defect this exists to stop: a restored conversation ending in assistant text leaves the
    /// run open, and the first live delta is appended to it -- last session's reply and this one's
    /// rendered as one bubble.
    #[test]
    fn the_first_live_reply_is_not_glued_onto_the_last_restored_one() {
        let conversation_id = setup();
        let path = write_transcript(&format!(
            "{}\n{}\n",
            typed_prompt("why is this slow?"),
            assistant_text("because of the loop")
        ));

        let mut projection = seed_from(Ok(path), &conversation_id, "sess-b");
        assert!(!projection.assistant_message_open);

        projection.apply(&AgentDomainEvent::ContentDelta {
            turn_id: "t-live".into(),
            kind: ContentKind::Text,
            text: "a new reply".into(),
        });
        assert_eq!(
            projection
                .transcript
                .iter()
                .map(|m| m.text.as_str())
                .collect::<Vec<_>>(),
            vec!["because of the loop", "a new reply"]
        );
    }

    /// Neither record a resume reads carries thinking back: Claude's transcript has its thinking
    /// blocks dropped by the reader, and Eitri's own copy never stored any. The seed has no block, none
    /// is open, and the first live thought is a block of its own.
    #[test]
    fn a_resumed_conversation_restores_no_thinking() {
        let conversation_id = setup();
        let thinking = r#"{"type":"assistant","uuid":"th","message":{"role":"assistant","content":[{"type":"thinking","thinking":"THINKING-SENTINEL","signature":"s"}]}}"#;
        let path = write_transcript(&format!(
            "{}\n{}\n{}\n",
            typed_prompt("why is this slow?"),
            thinking,
            assistant_text("because of the loop")
        ));

        let mut projection = seed_from(Ok(path), &conversation_id, "sess-th");
        assert!(projection.thinking.is_empty(), "{:?}", projection.thinking);
        assert!(!projection.thinking_open);
        assert_eq!(projection.transcript.len(), 1);

        projection.apply(&AgentDomainEvent::ContentDelta {
            turn_id: "t-live".into(),
            kind: ContentKind::Thinking,
            text: "a live thought".into(),
        });
        assert_eq!(projection.thinking.len(), 1);
        assert_eq!(projection.thinking[0].text, "a live thought");
        assert!(projection.thinking[0].seq >= projection.history.as_ref().unwrap().upto_seq);
    }

    /// Each stored `TranscriptMessage` is already coalesced. `apply` closes a run only on a prompt
    /// or turn boundary, and the stored events contain no turn boundaries. Explicitly close each
    /// assistant message so adjacent stored messages do not merge into one row.
    #[test]
    fn two_adjacent_stored_messages_stay_two_messages() {
        let conversation_id = setup();
        store_copy(
            &conversation_id,
            "sess-a",
            vec![],
            vec![message(1, "first message"), message(2, "second message")],
            vec![],
        );

        let projection = seed_from(Ok(missing_transcript()), &conversation_id, "sess-a");
        assert_eq!(
            projection
                .transcript
                .iter()
                .map(|m| m.text.as_str())
                .collect::<Vec<_>>(),
            vec!["first message", "second message"]
        );
    }

    // -------------------------------------------------------------------------------------
    // The transcript failing over to the stored copy, and the transcript winning
    // -------------------------------------------------------------------------------------

    /// A nonempty transcript with no conversational items falls back because the file has no
    /// schema version and nothing parsed is the only available signal of a format change.
    /// The notice names the CLI that wrote it so the format can be dated.
    ///
    /// The reason must describe the observation rather than assume a format change: attachment-only
    /// files read perfectly, and real sessions with only slash-command echoes or `isMeta` lines can
    /// reach the same branch. Pin the whole sentence because the panel embeds it; wording that
    /// drifts can send a user to inspect a healthy file.
    #[test]
    fn a_transcript_that_parses_nothing_falls_back_to_the_stored_copy() {
        let conversation_id = setup();
        store_copy(
            &conversation_id,
            "sess-x",
            vec![prompt(1, "what eitri kept")],
            vec![],
            vec![],
        );
        let path = write_transcript(&format!("{}\n{}\n", attachment(), attachment()));

        let projection = seed_from(Ok(path.clone()), &conversation_id, "sess-x");
        let notice = projection.history.clone().expect("the stored copy was restored");
        assert_eq!(notice.source, HistorySource::EitriCopy);
        assert_eq!(
            notice.fallback_reason.as_deref(),
            Some("nothing in it could be restored (written by claude 2.1.272)")
        );
        assert_eq!(
            notice.attempted_transcript_path.as_deref(),
            Some(path.to_string_lossy().as_ref())
        );
        assert_eq!(
            projection
                .user_prompts
                .iter()
                .map(|p| p.text.as_str())
                .collect::<Vec<_>>(),
            vec!["what eitri kept"]
        );
    }

    /// The transcript wins even when the stored copy is longer. The CLI's file reflects what the
    /// resumed session can remember. Merging in extra stored messages would imply it remembers more.
    #[test]
    fn the_transcript_wins_over_a_longer_stored_copy() {
        let conversation_id = setup();
        store_copy(
            &conversation_id,
            "sess-both",
            vec![prompt(1, "old one"), prompt(2, "old two"), prompt(3, "old three")],
            vec![],
            vec![],
        );
        let path = write_transcript(&format!("{}\n", typed_prompt("the transcript's only prompt")));

        let projection = seed_from(Ok(path), &conversation_id, "sess-both");
        let notice = projection.history.clone().expect("history was restored");
        assert_eq!(notice.source, HistorySource::ClaudeTranscript);
        assert_eq!(notice.fallback_reason, None);
        assert_eq!(notice.attempted_transcript_path, None);
        assert_eq!(notice.writer_version.as_deref(), Some("2.1.272"));
        assert_eq!(
            projection
                .user_prompts
                .iter()
                .map(|p| p.text.as_str())
                .collect::<Vec<_>>(),
            vec!["the transcript's only prompt"]
        );
    }

    /// An empty transcript represents an empty conversation and does not trigger fallback.
    /// The stored copy is deliberately non-empty so an incorrect fallback would be visible.
    #[test]
    fn an_empty_transcript_is_an_empty_history_and_not_a_fallback() {
        let conversation_id = setup();
        store_copy(
            &conversation_id,
            "sess-empty",
            vec![prompt(1, "never shown")],
            vec![],
            vec![],
        );
        let path = write_transcript("");

        let projection = seed_from(Ok(path), &conversation_id, "sess-empty");
        assert!(projection.history.is_none(), "an empty session gets no notice");
        assert!(
            projection.user_prompts.is_empty(),
            "the stored copy must not be consulted"
        );
    }

    /// Neither history source exists, so there are no restored rows or notice. A session with no
    /// earlier messages needs no explanation row.
    #[test]
    fn neither_source_available_restores_nothing_and_says_nothing() {
        let conversation_id = setup();
        let projection = seed_from(Ok(missing_transcript()), &conversation_id, "sess-none");
        assert!(projection.history.is_none());
        assert!(all_seqs(&projection).is_empty());
        assert_eq!(projection.last_revision, 0);
    }

    /// Without `CLAUDE_CONFIG_DIR` or `HOME`, no transcript path can be built. The stored copy still
    /// loads, and the notice has no attempted path because none was available.
    #[test]
    fn a_transcript_path_that_cannot_be_built_still_reaches_the_stored_copy() {
        let conversation_id = setup();
        store_copy(
            &conversation_id,
            "sess-nopath",
            vec![prompt(1, "kept anyway")],
            vec![],
            vec![],
        );

        let projection = seed_from(
            Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "neither CLAUDE_CONFIG_DIR nor HOME is set",
            )),
            &conversation_id,
            "sess-nopath",
        );
        let notice = projection.history.clone().expect("the stored copy was restored");
        assert_eq!(notice.source, HistorySource::EitriCopy);
        assert_eq!(notice.attempted_transcript_path, None);
        assert!(notice
            .fallback_reason
            .as_deref()
            .expect("a reason")
            .contains("could not be located"));
    }

    // Restored history must carry no pending permissions or live session state.

    /// Invariant 3. A restored permission card would be a request addressed to a process that no
    /// longer exists -- at best the provider errors, at worst the id matches something else. On the
    /// transcript path it is unreachable by construction (permissions never reach that file at
    /// all); on the stored path `StoredHistory` has no such collection. Both are asserted, because
    /// "unreachable" is a property of today's types and this is the test that would notice it
    /// changing.
    #[test]
    fn no_restored_history_ever_carries_a_pending_permission() {
        let conversation_id = setup();
        let path = write_transcript(&format!(
            "{}\n{}\n",
            typed_prompt("run the tests"),
            assistant_text("running them")
        ));
        let from_transcript = seed_from(Ok(path), &conversation_id, "sess-b");
        assert!(from_transcript.pending_permissions.is_empty());

        store_copy(
            &conversation_id,
            "sess-a",
            vec![prompt(1, "run the tests")],
            vec![],
            vec![tool_call(2, "toolu_1", None)],
        );
        let from_store = seed_from(Ok(missing_transcript()), &conversation_id, "sess-a");
        assert!(from_store.pending_permissions.is_empty());
    }

    /// Invariant 4: everything that describes a LIVE session stays unset. Restoring `status:
    /// Running` would claim a provider is up before one has been asked for, and restoring
    /// `active_turn_id` spins the panel forever on a turn that ended days ago.
    #[test]
    fn loading_history_declares_nothing_about_a_live_session() {
        let conversation_id = setup();
        let path = write_transcript(&format!(
            "{}\n{}\n",
            typed_prompt("still here?"),
            assistant_text("still here")
        ));

        let projection = seed_from(Ok(path), &conversation_id, "sess-b");
        assert!(matches!(projection.status, ProjectionStatus::Starting));
        assert_eq!(projection.active_turn_id, None);
        assert_eq!(projection.session_id, None);
        assert_eq!(projection.provider_session_id, None);
        assert_eq!(projection.model, None);
        assert_eq!(projection.cwd, None);
        assert_eq!(projection.usage, None);
    }

    // Stored tool calls preserve both recorded results and their absence.

    /// A stored call with a result keeps it, and one without stays `None` -- which is what tells
    /// the panel to render "no result recorded" rather than a spinner. The two must not be
    /// collapsed: below `uptoSeq` there is no such thing as a call still running.
    #[test]
    fn a_stored_tool_call_keeps_its_result_and_its_absence() {
        let conversation_id = setup();
        store_copy(
            &conversation_id,
            "sess-a",
            vec![],
            vec![],
            vec![
                tool_call(1, "toolu_done", Some("file contents")),
                tool_call(2, "toolu_never", None),
            ],
        );

        let projection = seed_from(Ok(missing_transcript()), &conversation_id, "sess-a");
        assert_eq!(projection.tool_calls.len(), 2);
        assert_eq!(
            projection.tool_calls[0].result.as_ref().map(|r| r.content.clone()),
            Some(serde_json::json!("file contents"))
        );
        assert!(projection.tool_calls[1].result.is_none());
        let notice = projection.history.expect("history was restored");
        assert!(projection.tool_calls.iter().all(|c| c.seq < notice.upto_seq));
    }

    /// A completion whose id is the empty string is dropped rather than folded: `apply` matches
    /// completions to calls by id equality, so an empty one would attach this result to the FIRST
    /// call that also has an empty id -- a result shown against a call that never produced it.
    /// Losing a result is recoverable; inventing one somewhere else is not.
    #[test]
    fn a_stored_result_with_no_tool_use_id_is_dropped_rather_than_cross_linked() {
        let conversation_id = setup();
        store_copy(
            &conversation_id,
            "sess-a",
            vec![],
            vec![],
            vec![
                tool_call(1, "", Some("first output")),
                tool_call(2, "", Some("second output")),
            ],
        );

        let projection = seed_from(Ok(missing_transcript()), &conversation_id, "sess-a");
        assert_eq!(projection.tool_calls.len(), 2);
        assert!(projection.tool_calls.iter().all(|c| c.result.is_none()));
    }

    // -------------------------------------------------------------------------------------
    // What the notice's count means
    // -------------------------------------------------------------------------------------

    /// **`restored_items` is what the panel draws, not what the parser kept.**
    ///
    /// `apply` coalesces consecutive assistant text into ONE `TranscriptMessage`, so a transcript
    /// of four adjacent assistant lines is four slots to the reader and one row to the panel. The
    /// notice used to carry the reader's number, which on the measured corpus over-stated
    /// the drawn count on 13 of 45 real files (worst case 400 against 384) -- and the panel's
    /// sentence prints this number, so an over-statement is a sentence a user can disprove by
    /// scrolling.
    ///
    /// The assertion is deliberately in two parts. The equality is the property; the strict
    /// inequality against `parsed_items` is what makes the test able to FAIL, since an
    /// implementation that reported the pre-fold count would satisfy the equality on any input
    /// where nothing folds.
    #[test]
    fn the_notice_counts_the_rows_the_panel_draws_not_the_slots_the_parser_kept() {
        let conversation_id = setup();
        let lines = format!(
            "{}\n{}\n{}\n{}\n{}\n",
            typed_prompt("one question"),
            assistant_text("first "),
            assistant_text("second "),
            assistant_text("third "),
            assistant_text("fourth")
        );
        let path = write_transcript(&lines);

        // What the reader kept, before anything folded.
        let parsed = read_transcript_history(&path).expect("the test transcript parses");
        assert_eq!(parsed.parsed_items, 5, "one prompt plus four assistant slots");

        let projection = seed_from(Ok(path), &conversation_id, "sess-fold");
        let notice = projection.history.clone().expect("history was restored");
        assert_eq!(
            notice.restored_items,
            restorable_item_count(&projection),
            "the notice must report the projection's own contents"
        );
        assert_eq!(
            notice.restored_items, 2,
            "one prompt plus one coalesced assistant message"
        );
        assert!(
            notice.restored_items < parsed.parsed_items,
            "this input has to fold, or the equality above proves nothing"
        );
    }

    /// The A path reports the SAME quantity, computed the same way.
    ///
    /// It used to report `capped.item_count()` -- post-truncation STORED records -- so one field
    /// name carried two different meanings depending on which source answered. A stored copy
    /// holding two adjacent assistant messages is the case that separates them: `load_stored`
    /// closes the run after every stored message precisely so the two stay apart on screen.
    ///
    /// **Said plainly: this test cannot go red against the code it replaced.** Because that run is
    /// closed after every stored message, nothing on the A path ever folds, so the old
    /// `capped.item_count()` and the new row count agree on every input that exists today. What
    /// was wrong was the BASIS, not the number, and a basis is what a test can pin: if the closing
    /// rule ever changes, or a prompt or tool call ever starts merging, this assertion moves with
    /// the projection while a record count would silently stop describing the screen. The red
    /// check for the fold itself is the test above, on the B path.
    #[test]
    fn the_fallback_path_counts_rows_the_same_way_the_transcript_path_does() {
        let conversation_id = setup();
        store_copy(
            &conversation_id,
            "sess-a",
            vec![prompt(1, "a question")],
            vec![message(2, "first reply"), message(3, "second reply")],
            vec![tool_call(4, "toolu_1", Some("output"))],
        );

        let projection = seed_from(Ok(missing_transcript()), &conversation_id, "sess-a");
        let notice = projection.history.clone().expect("history was restored");
        assert_eq!(notice.restored_items, restorable_item_count(&projection));
        assert_eq!(notice.restored_items, 4);
        assert_eq!(
            projection.transcript.len(),
            2,
            "two stored messages must stay two rows, or the count above is measuring the wrong thing"
        );
    }

    /// A path that is not a regular file is refused BEFORE it is opened, and the refusal is an
    /// ordinary reported fallback.
    ///
    /// `File::open` on a FIFO blocks in `open(2)` until a writer appears, and this call sits on the
    /// backend-start worker, which `collect_pending_starts` polls with `try_recv` and no timeout --
    /// so the panel would sit in its connecting state for the life of the window. A directory
    /// stands in for the FIFO here because it needs no `mkfifo` and no second thread, and it
    /// exercises the same guard on the same line; the guard is a `metadata` check, which cannot
    /// tell the two apart in any way this code branches on.
    #[test]
    fn a_transcript_path_that_is_not_a_regular_file_is_refused_rather_than_opened() {
        let conversation_id = setup();
        store_copy(
            &conversation_id,
            "sess-a",
            vec![prompt(1, "kept anyway")],
            vec![],
            vec![],
        );
        let not_a_file = crate::state_dirs::test_workspace_dir("transcript-dir");

        let projection = seed_from(Ok(not_a_file.clone()), &conversation_id, "sess-a");
        let notice = projection.history.clone().expect("the stored copy was restored");
        assert_eq!(notice.source, HistorySource::EitriCopy);
        assert_eq!(
            notice.fallback_reason.as_deref(),
            Some("the transcript path is not a regular file")
        );
        assert_eq!(
            notice.attempted_transcript_path.as_deref(),
            Some(not_a_file.to_string_lossy().as_ref())
        );
    }

    /// Fold every real top-level transcript in the corpus and compare the notice with the
    /// projection it produced. The hermetic case above pins one constructed input; this checks
    /// that the property survives a real corpus, including the 45 files that exposed a mismatch.
    /// Print how many files fold so an empty corpus cannot masquerade as meaningful equality.
    ///
    /// Shapes and counts only: nothing reads, prints or compares conversation content.
    #[test]
    #[ignore = "needs this machine's real $CLAUDE_CONFIG_DIR/projects corpus"]
    fn every_real_transcript_reports_the_count_its_projection_holds() {
        let conversation_id = setup();
        let files = super::super::transcript_jsonl::real_session_files();
        assert!(!files.is_empty(), "no real session files found");

        let mut folded = 0usize;
        let mut restored_any = 0usize;
        for path in &files {
            let parsed = read_transcript_history(path).unwrap_or_else(|e| panic!("{} failed: {e}", path.display()));
            let projection = seed_from(Ok(path.clone()), &conversation_id, "sess-corpus");
            let Some(notice) = projection.history.clone() else {
                assert_eq!(parsed.parsed_items, 0, "{}: items but no notice", path.display());
                continue;
            };
            restored_any += 1;
            assert_eq!(
                notice.restored_items,
                restorable_item_count(&projection),
                "{}: the notice disagrees with the projection it describes",
                path.display()
            );
            // A restored question keeps its tool row and adds its form: one slot, two rows.
            assert!(
                notice.restored_items <= parsed.parsed_items + projection.questions.len(),
                "{}: folding cannot produce MORE rows than slots and restored questions",
                path.display()
            );
            if notice.restored_items < parsed.parsed_items {
                folded += 1;
            }
        }
        assert!(restored_any > 0, "no real file restored anything");
        eprintln!("{folded} of {restored_any} real transcripts fold at least one pair of adjacent assistant slots");
    }

    // -------------------------------------------------------------------------------------
    // Turn starts, notices and questions through A; restored turns are closed
    // -------------------------------------------------------------------------------------

    use crate::projection::{
        NoticeLevel, QuestionOptionSpec, QuestionSpec, SessionNoticeKind, ShownAnswer, ToolCallDenial, TurnCause,
        TurnCauseKind, TurnOrigin,
    };

    fn store_parts(conversation_id: &str, session_id: &str, parts: store::HistoryParts) {
        store::save(&StoredHistory::from_parts(
            conversation_id.to_string(),
            session_id.to_string(),
            parts,
        ))
        .expect("writing a test stored history should succeed");
    }

    fn task_cause() -> TurnCause {
        let mut cause = TurnCause::of_kind(TurnCauseKind::Task);
        cause.task_id = Some("task-7".into());
        cause.task_status = Some("completed".into());
        cause.summary = Some("the build finished".into());
        cause
    }

    fn one_question(text: &str) -> Vec<QuestionSpec> {
        vec![QuestionSpec {
            question: text.into(),
            header: "Pick".into(),
            multi_select: false,
            options: vec![
                QuestionOptionSpec {
                    label: "left".into(),
                    description: "the left one".into(),
                    preview: None,
                },
                QuestionOptionSpec {
                    label: "right".into(),
                    description: "the right one".into(),
                    preview: Some("`right`".into()),
                },
            ],
        }]
    }

    fn task_notice() -> AgentDomainEvent {
        AgentDomainEvent::SessionNotice {
            kind: SessionNoticeKind::Task,
            level: NoticeLevel::Notice,
            content: Some("Background command finished".into()),
            scalars: [("exit_code".to_string(), serde_json::json!(0))].into_iter().collect(),
            task_id: Some("task-7".into()),
            tool_use_id: Some("toolu_bg".into()),
            task_status: Some("completed".into()),
        }
    }

    /// A conversation as a live session folds it: a turn of the user's own with a message read
    /// inside it and a call the CLI refused, then a turn the CLI started because a task ended, with
    /// its notice, an answered question and one still waiting when the session stopped.
    fn live_conversation() -> AgentSessionProjection {
        let mut live = AgentSessionProjection::default();
        let events = vec![
            AgentDomainEvent::UserPromptSubmitted {
                text: "build it".into(),
            },
            AgentDomainEvent::TurnStarted { turn_id: "t1".into() },
            AgentDomainEvent::ContentDelta {
                turn_id: "t1".into(),
                kind: ContentKind::Text,
                text: "Building.".into(),
            },
            AgentDomainEvent::ToolCallStarted {
                turn_id: "t1".into(),
                tool_use_id: "toolu_rm".into(),
                name: "Bash".into(),
                input: serde_json::json!({ "command": "rm -rf build" }),
            },
            AgentDomainEvent::PermissionDenied {
                tool_use_id: Some("toolu_rm".into()),
                tool_name: "Bash".into(),
                reason_type: Some("classifier".into()),
                reason: Some("[Destructive]".into()),
            },
            AgentDomainEvent::ToolCallCompleted {
                turn_id: "t1".into(),
                tool_use_id: "toolu_rm".into(),
                content: serde_json::json!("refused"),
                is_error: true,
            },
            AgentDomainEvent::UserMessageDelivered {
                client_message_id: "eitri-1-1".into(),
                turn_id: "t1".into(),
                text: "in the background".into(),
            },
            AgentDomainEvent::TurnCompleted {
                turn_id: "t1".into(),
                outcome: TurnOutcome::Completed,
                result_text: String::new(),
                stop_reason: None,
                usage: None,
                detail: TurnEndDetail::default(),
            },
            task_notice(),
            AgentDomainEvent::TurnStarted {
                turn_id: "cli-1".into(),
            },
            AgentDomainEvent::TurnAttributed {
                turn_id: "cli-1".into(),
                origin: TurnOrigin::Cli,
                cause: Some(task_cause()),
            },
            AgentDomainEvent::ContentDelta {
                turn_id: "cli-1".into(),
                kind: ContentKind::Text,
                text: "The build finished.".into(),
            },
            AgentDomainEvent::QuestionAsked {
                interaction_id: "q-1".into(),
                tool_use_id: Some("toolu_q1".into()),
                questions: one_question("Which target?"),
                unsupported_reason: None,
            },
            AgentDomainEvent::QuestionSettled {
                interaction_id: "q-1".into(),
                outcome: QuestionOutcome::Answered,
                answers: vec![ShownAnswer {
                    question_index: 0,
                    answer: "right".into(),
                    notes: Some("the fast one".into()),
                }],
                reason: None,
            },
            AgentDomainEvent::QuestionAsked {
                interaction_id: "q-2".into(),
                tool_use_id: Some("toolu_q2".into()),
                questions: one_question("Deploy now?"),
                unsupported_reason: None,
            },
        ];
        for event in &events {
            live.apply(event);
        }
        live
    }

    /// What the history writer stores of a projection: the same copy and the same caps ingestion uses.
    fn store_projection(conversation_id: &str, session_id: &str, live: &AgentSessionProjection) {
        let parts = store::newest_that_fit(store::HistoryView::of(live));
        store_parts(conversation_id, session_id, parts);
    }

    fn without_seq<T: Clone>(items: &[T], clear: impl Fn(&mut T)) -> Vec<T> {
        items
            .iter()
            .cloned()
            .map(|mut item| {
                clear(&mut item);
                item
            })
            .collect()
    }

    /// Everything the three new collections hold comes back from Eitri's own copy as it was, but for
    /// the numbers `apply` gives again and a waiting question, which comes back closed.
    #[test]
    fn a_round_trip_keeps_turn_starts_notices_and_questions() {
        let conversation_id = setup();
        let live = live_conversation();
        assert_eq!(
            live.turn_starts.len(),
            1,
            "the CLI's turn has a start row; the user's has none"
        );
        store_projection(&conversation_id, "sess-new", &live);

        let restored = seed_from(Ok(missing_transcript()), &conversation_id, "sess-new");

        assert_eq!(
            without_seq(&restored.turn_starts, |r| r.seq = 0),
            without_seq(&live.turn_starts, |r| r.seq = 0)
        );
        assert_eq!(restored.turn_starts[0].cause, Some(task_cause()));
        assert_eq!(
            without_seq(&restored.notices, |r| r.seq = 0),
            without_seq(&live.notices, |r| r.seq = 0)
        );
        assert_eq!(
            without_seq(&restored.questions[..1], |r| r.seq = 0),
            without_seq(&live.questions[..1], |r| r.seq = 0),
            "a settled question comes back as it was settled, in the turn it was asked in"
        );
        assert_eq!(live.questions[1].state, QuestionState::Waiting);
        assert_eq!(
            restored.questions[1].state,
            QuestionState::Settled {
                outcome: QuestionOutcome::SessionClosed,
                answers: vec![],
                reason: None
            }
        );
        // The rows keep the order they were drawn in.
        let order = |p: &AgentSessionProjection| {
            let mut rows: Vec<(u64, &str)> = p
                .user_prompts
                .iter()
                .map(|r| (r.seq, "prompt"))
                .chain(p.transcript.iter().map(|r| (r.seq, "reply")))
                .chain(p.tool_calls.iter().map(|r| (r.seq, "call")))
                .chain(p.turn_starts.iter().map(|r| (r.seq, "start")))
                .chain(p.notices.iter().map(|r| (r.seq, "notice")))
                .chain(p.questions.iter().map(|r| (r.seq, "question")))
                .collect();
            rows.sort_unstable();
            rows.into_iter().map(|(_, kind)| kind).collect::<Vec<_>>()
        };
        assert_eq!(order(&restored), order(&live));
        let notice = restored.history.as_ref().expect("history was restored");
        assert_eq!(notice.restored_items, 9, "every row the panel draws is counted");
        assert_eq!(restored.active_turn_id, None);
    }

    /// A question keeps the turn it was asked in across a restore and the write after it: one asked in
    /// a turn of the user's own (A stores no start for that turn), and one asked between turns right
    /// after a turn of the CLI's (which the refold keeps open until the next start or prompt).
    #[test]
    fn a_restored_question_keeps_the_turn_it_was_asked_in() {
        let conversation_id = setup();
        let ask = |id: &str| AgentDomainEvent::QuestionAsked {
            interaction_id: id.into(),
            tool_use_id: Some(format!("toolu_{id}")),
            questions: one_question("Which?"),
            unsupported_reason: None,
        };
        let settle = |id: &str| AgentDomainEvent::QuestionSettled {
            interaction_id: id.into(),
            outcome: QuestionOutcome::Declined,
            answers: vec![],
            reason: None,
        };
        let completed = |id: &str| AgentDomainEvent::TurnCompleted {
            turn_id: id.into(),
            outcome: TurnOutcome::Completed,
            result_text: String::new(),
            stop_reason: None,
            usage: None,
            detail: TurnEndDetail::default(),
        };
        let mut live = AgentSessionProjection::default();
        for event in [
            AgentDomainEvent::UserPromptSubmitted { text: "ask me".into() },
            AgentDomainEvent::TurnStarted { turn_id: "t1".into() },
            ask("in-mine"),
            settle("in-mine"),
            completed("t1"),
            AgentDomainEvent::TurnStarted {
                turn_id: "cli-1".into(),
            },
            AgentDomainEvent::TurnAttributed {
                turn_id: "cli-1".into(),
                origin: TurnOrigin::Cli,
                cause: None,
            },
            completed("cli-1"),
            ask("between"),
            settle("between"),
        ] {
            live.apply(&event);
        }
        let turns = |p: &AgentSessionProjection| p.questions.iter().map(|q| q.turn_id.clone()).collect::<Vec<_>>();
        assert_eq!(turns(&live), vec![Some("t1".to_string()), None]);
        store_projection(&conversation_id, "sess-turns", &live);

        let restored = seed_from(Ok(missing_transcript()), &conversation_id, "sess-turns");
        assert_eq!(turns(&restored), turns(&live));
        let again = store::newest_that_fit(store::HistoryView::of(&restored));
        assert_eq!(
            again.questions.iter().map(|q| q.turn_id.clone()).collect::<Vec<_>>(),
            turns(&live),
            "the next write stores the same turns"
        );
    }

    /// A prompt's images come back as what they were: type, size and digest, in the order they went, on the
    /// prompt they went with, and the next write stores them again. A prompt without images has none.
    #[test]
    fn a_prompt_with_images_round_trips_metadata() {
        let conversation_id = setup();
        let meta = |media_type: &str, bytes: u64, digit: char| crate::provider::AttachmentMeta {
            media_type: media_type.into(),
            bytes,
            width: 640,
            height: 480,
            sha256: digit.to_string().repeat(64),
        };
        let mut with_images = prompt(1, "what is in these?");
        with_images.attachments = vec![meta("image/png", 1200, 'a'), meta("image/jpeg", 800, 'b')];
        store_copy(
            &conversation_id,
            "sess-images",
            vec![with_images.clone(), prompt(3, "and now?")],
            vec![message(2, "Two pictures.")],
            vec![],
        );

        let restored = seed_from(Ok(missing_transcript()), &conversation_id, "sess-images");

        assert_eq!(restored.user_prompts.len(), 2);
        assert_eq!(restored.user_prompts[0].attachments, with_images.attachments);
        assert!(restored.user_prompts[1].attachments.is_empty());
        let stored = serde_json::to_string(&restored.user_prompts).unwrap();
        assert!(
            !stored.contains("\"data\""),
            "no bytes ride with the metadata: {stored}"
        );

        store_projection(&conversation_id, "sess-images-2", &restored);
        let again = seed_from(Ok(missing_transcript()), &conversation_id, "sess-images-2");
        assert_eq!(
            again
                .user_prompts
                .iter()
                .map(|p| p.attachments.clone())
                .collect::<Vec<_>>(),
            restored
                .user_prompts
                .iter()
                .map(|p| p.attachments.clone())
                .collect::<Vec<_>>()
        );
    }

    /// The two marks A always stored but did not restore: a message read inside a running turn stays
    /// one (not the start of a new turn), and a call the CLI refused keeps its refusal.
    #[test]
    fn a_refold_restores_delivered_prompts_and_denied_calls() {
        let conversation_id = setup();
        let mut refused = tool_call(3, "toolu_rm", Some("refused"));
        refused.denied = Some(ToolCallDenial {
            reason_type: Some("classifier".into()),
            reason: Some("[Destructive]".into()),
        });
        let mut unlinked = tool_call(5, "", None);
        unlinked.denied = Some(ToolCallDenial {
            reason_type: Some("rule".into()),
            reason: None,
        });
        let delivered = UserPromptRecord {
            seq: 4,
            text: "in the background".into(),
            delivered: true,
            attachments: Vec::new(),
        };
        store_copy(
            &conversation_id,
            "sess-marks",
            vec![prompt(1, "build it"), delivered],
            vec![message(2, "Building.")],
            vec![refused.clone(), unlinked],
        );

        let restored = seed_from(Ok(missing_transcript()), &conversation_id, "sess-marks");

        assert_eq!(
            restored
                .user_prompts
                .iter()
                .map(|p| (p.text.as_str(), p.delivered))
                .collect::<Vec<_>>(),
            vec![("build it", false), ("in the background", true)]
        );
        assert_eq!(restored.tool_calls[0].denied, refused.denied);
        assert_eq!(restored.tool_calls[1].denied, None, "an empty id is no link to refuse");
        assert!(restored.denials_before_their_call.is_empty());
        // And the same projection written again stores the same marks.
        let again = store::newest_that_fit(store::HistoryView::of(&restored));
        assert!(again.user_prompts[1].delivered);
        assert_eq!(again.tool_calls[0].denied, refused.denied);
    }

    /// A file an older build wrote, with none of the new collections, still restores: the
    /// collections are `#[serde(default)]` and the format version is unchanged.
    #[test]
    fn an_older_history_file_without_new_collections_loads() {
        let conversation_id = setup();
        let path = store::history_path(&conversation_id, "sess-old").unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let text = format!(
            r#"{{"version":{},"conversation_id":"{conversation_id}","provider_session_id":"sess-old",
                 "written_at":"1","user_prompts":[{{"seq":1,"text":"hi"}}],
                 "transcript":[{{"seq":2,"text":"hello"}}],"tool_calls":[]}}"#,
            store::HISTORY_FORMAT_VERSION
        );
        crate::test_tmp::write_private(&path, text).unwrap();
        let loaded = store::load(&conversation_id, "sess-old").expect("an older file loads");
        assert!(loaded.turn_starts.is_empty() && loaded.notices.is_empty() && loaded.questions.is_empty());

        let restored = seed_from(Ok(missing_transcript()), &conversation_id, "sess-old");
        assert_eq!(restored.user_prompts.len(), 1);
        assert_eq!(restored.transcript.len(), 1);
        assert!(restored.turn_starts.is_empty() && restored.notices.is_empty() && restored.questions.is_empty());
    }

    /// A question stored while it waited, or while an answer was on its way, comes back closed: the
    /// provider that asked it is gone and nothing can answer it. A settled one keeps its outcome.
    #[test]
    fn a_waiting_question_restores_as_closed() {
        let conversation_id = setup();
        let record = |seq: u64, id: &str, state: QuestionState| QuestionRecord {
            seq,
            interaction_id: id.into(),
            tool_use_id: Some(format!("toolu_{id}")),
            turn_id: None,
            questions: one_question("Which?"),
            state,
            unsupported_reason: None,
        };
        let declined = QuestionState::Settled {
            outcome: QuestionOutcome::Declined,
            answers: vec![],
            reason: Some("the person cancelled".into()),
        };
        store_parts(
            &conversation_id,
            "sess-q",
            store::HistoryParts {
                questions: vec![
                    record(1, "waiting", QuestionState::Waiting),
                    record(2, "answering", QuestionState::Answering),
                    record(3, "declined", declined.clone()),
                ],
                ..store::HistoryParts::default()
            },
        );

        let restored = seed_from(Ok(missing_transcript()), &conversation_id, "sess-q");

        let closed = QuestionState::Settled {
            outcome: QuestionOutcome::SessionClosed,
            answers: vec![],
            reason: None,
        };
        assert_eq!(
            restored.questions.iter().map(|q| q.state.clone()).collect::<Vec<_>>(),
            vec![closed.clone(), closed, declined]
        );
        assert!(restored.pending_permissions.is_empty());
    }

    fn proposed(id: &str, plan: &str) -> AgentDomainEvent {
        AgentDomainEvent::PlanProposed {
            interaction_id: id.into(),
            tool_use_id: Some(format!("toolu_{id}")),
            plan: plan.into(),
            plan_sha256: format!("sha-of-{plan}"),
            allowed_prompts: vec![crate::PlanAllowedPrompt {
                tool: "Bash".into(),
                prompt: "run the tests".into(),
            }],
            unsupported_reason: None,
        }
    }

    fn plan_settled(id: &str, outcome: PlanOutcome) -> AgentDomainEvent {
        AgentDomainEvent::PlanSettled {
            interaction_id: id.into(),
            outcome,
            mode_after: (outcome == PlanOutcome::Approved).then_some(crate::PlanExecutionMode::Auto),
            feedback: (outcome == PlanOutcome::KeptPlanning).then(|| "also the docs".to_string()),
            reason: None,
        }
    }

    /// Plans come back from Eitri's own copy as they were stored, in the turn they were proposed in and
    /// in their place among the other rows, but for the numbers `apply` gives again and a waiting one,
    /// which comes back closed. Nothing about the restore starts a transition or leaves a plan waiting.
    #[test]
    fn a_round_trip_keeps_plans() {
        let conversation_id = setup();
        let mut live = AgentSessionProjection::default();
        for event in [
            AgentDomainEvent::UserPromptSubmitted { text: "plan it".into() },
            AgentDomainEvent::TurnStarted { turn_id: "t1".into() },
            proposed("p1", "# One\n1. do it\n"),
            plan_settled("p1", PlanOutcome::KeptPlanning),
            proposed("p2", "# Two\n1. do it better\n"),
            plan_settled("p2", PlanOutcome::Approved),
            AgentDomainEvent::ContentDelta {
                turn_id: "t1".into(),
                kind: ContentKind::Text,
                text: "Going ahead.".into(),
            },
            proposed("p3", "# Three\n"),
        ] {
            live.apply(&event);
        }
        assert_eq!(live.plans.len(), 3);
        assert_eq!(live.plans[2].state, PlanState::Waiting);
        store_projection(&conversation_id, "sess-plans", &live);

        let restored = seed_from(Ok(missing_transcript()), &conversation_id, "sess-plans");

        assert_eq!(
            without_seq(&restored.plans[..2], |r| r.seq = 0),
            without_seq(&live.plans[..2], |r| r.seq = 0),
            "a settled plan comes back as it was settled, with its text, digest, feedback, mode and turn"
        );
        assert_eq!(restored.plans[0].turn_id.as_deref(), Some("t1"));
        assert_eq!(
            restored.plans[2].plan, live.plans[2].plan,
            "a closed plan keeps its words"
        );
        assert_eq!(
            restored.plans[2].state,
            PlanState::Settled {
                outcome: PlanOutcome::SessionClosed,
                mode_after: None,
                feedback: None,
                reason: None
            }
        );
        assert!(
            restored.waiting_plan().is_none(),
            "nothing restored waits for a decision"
        );
        assert!(!restored.is_planning() && restored.pending_permissions.is_empty());
        let order = |p: &AgentSessionProjection| {
            let mut rows: Vec<(u64, &str)> = p
                .user_prompts
                .iter()
                .map(|r| (r.seq, "prompt"))
                .chain(p.transcript.iter().map(|r| (r.seq, "reply")))
                .chain(p.plans.iter().map(|r| (r.seq, "plan")))
                .collect();
            rows.sort_unstable();
            rows.into_iter().map(|(_, kind)| kind).collect::<Vec<_>>()
        };
        assert_eq!(order(&restored), order(&live));
        // The write after a restore keeps the turn each plan was proposed in.
        let again = store::newest_that_fit(store::HistoryView::of(&restored));
        assert_eq!(
            again.plans.iter().map(|r| r.turn_id.clone()).collect::<Vec<_>>(),
            live.plans.iter().map(|r| r.turn_id.clone()).collect::<Vec<_>>()
        );
    }

    /// A plan stored while it waited, or while a decision was on its way, comes back closed: the provider
    /// that proposed it is gone and nothing can decide it. A settled one keeps its outcome.
    #[test]
    fn a_waiting_plan_restores_as_closed() {
        let conversation_id = setup();
        let record = |seq: u64, id: &str, state: PlanState| PlanRecord {
            seq,
            interaction_id: id.into(),
            tool_use_id: Some(format!("toolu_{id}")),
            turn_id: None,
            plan: "# Plan\n".into(),
            plan_sha256: "abc".into(),
            allowed_prompts: vec![],
            state,
            unsupported_reason: None,
        };
        let kept = PlanState::Settled {
            outcome: PlanOutcome::KeptPlanning,
            mode_after: None,
            feedback: Some("less".into()),
            reason: None,
        };
        store_parts(
            &conversation_id,
            "sess-p",
            store::HistoryParts {
                plans: vec![
                    record(1, "waiting", PlanState::Waiting),
                    record(2, "answering", PlanState::Answering),
                    record(3, "kept", kept.clone()),
                ],
                ..store::HistoryParts::default()
            },
        );

        let restored = seed_from(Ok(missing_transcript()), &conversation_id, "sess-p");

        let closed = PlanState::Settled {
            outcome: PlanOutcome::SessionClosed,
            mode_after: None,
            feedback: None,
            reason: None,
        };
        assert_eq!(
            restored.plans.iter().map(|p| p.state.clone()).collect::<Vec<_>>(),
            vec![closed.clone(), closed, kept]
        );
        assert!(restored.waiting_plan().is_none());
        assert!(restored.pending_permissions.is_empty());
    }

    /// The projection as JSON with the notice's source path blanked: a tail has no local path to name.
    fn comparable(mut projection: AgentSessionProjection) -> serde_json::Value {
        if let Some(history) = projection.history.as_mut() {
            history.source_path.clear();
        }
        serde_json::to_value(&projection).expect("a projection serializes")
    }

    #[test]
    fn seed_from_tail_equals_seed_from_the_same_file() {
        let conversation_id = setup();
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/history_b");
        let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
            .expect("the history corpus is there")
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
            .collect();
        files.sort();
        assert!(files.len() > 20, "the corpus must not be empty");
        for file in files {
            let bytes = std::fs::read(&file).unwrap();
            let from_file = seed_from(
                Ok(write_transcript(&String::from_utf8(bytes.clone()).unwrap())),
                &conversation_id,
                "sess-eq",
            );
            let from_tail = seed_from_tail(&bytes, false, &conversation_id, "sess-eq", "sess-eq.jsonl");
            assert_eq!(comparable(from_file), comparable(from_tail), "{}", file.display());
        }
    }

    #[test]
    fn a_tail_that_starts_mid_file_drops_its_first_line() {
        let conversation_id = setup();
        let whole = format!("{}\n{}\n", typed_prompt("first"), assistant_text("reply"));
        // The tail begins inside the first line: that partial line is dropped, the reply after it is kept.
        let cut = whole.find("\"type\":\"user\"").unwrap() + 10;
        let mid = seed_from_tail(
            &whole.as_bytes()[cut..],
            true,
            &conversation_id,
            "sess-mid",
            "sess-mid.jsonl",
        );
        assert!(mid.user_prompts.is_empty(), "the partial first line is not a prompt");
        assert_eq!(mid.transcript.len(), 1);
        let history = mid.history.as_ref().expect("the tail has a notice");
        assert_eq!(history.restored_items, 1);
        // The reader cannot tell a tail that starts on a line boundary from one that starts inside a line, so the
        // flag alone decides: set, the boundary tail loses its real first line; clear, it keeps it.
        let boundary = whole.find('\n').unwrap() + 1;
        let tail = &whole.as_bytes()[boundary..];
        let dropped = seed_from_tail(tail, true, &conversation_id, "sess-mid", "sess-mid.jsonl");
        assert!(
            dropped.transcript.is_empty(),
            "the first line of a tail is always dropped"
        );
        let kept = seed_from_tail(tail, false, &conversation_id, "sess-mid", "sess-mid.jsonl");
        assert_eq!(
            kept.transcript.len(),
            1,
            "a tail that is a whole file keeps its first line"
        );
    }

    #[test]
    fn a_tail_names_its_source_by_the_label_the_caller_gives() {
        let conversation_id = setup();
        let label = "example-host:/home/user/.claude/projects/-srv-project/sess-label.jsonl";
        let read = seed_from_tail(
            format!("{}\n", typed_prompt("kept")).as_bytes(),
            false,
            &conversation_id,
            "sess-label",
            label,
        );
        let notice = read.history.as_ref().expect("the tail has a notice");
        assert_eq!(notice.source, HistorySource::ClaudeTranscript);
        assert_eq!(notice.source_path, label);
        assert_eq!(notice.attempted_transcript_path, None);

        // A tail with nothing to restore falls to Eitri's own copy and names the label as what was tried.
        store_copy(
            &conversation_id,
            "sess-label",
            vec![prompt(1, "what eitri kept")],
            vec![],
            vec![],
        );
        let nothing = format!("{}\n{}\n", attachment(), attachment());
        let fell = seed_from_tail(nothing.as_bytes(), false, &conversation_id, "sess-label", label);
        let notice = fell.history.as_ref().expect("the stored copy was restored");
        assert_eq!(notice.source, HistorySource::EitriCopy);
        assert_eq!(notice.attempted_transcript_path.as_deref(), Some(label));
        assert_eq!(
            notice.fallback_reason.as_deref(),
            Some("nothing in it could be restored (written by claude 2.1.272)")
        );
    }

    /// The tail a source hands over, read in memory, gives what the file reader gives for a file larger than the scan
    /// bound: its last bytes, read as a tail that starts mid file.
    #[test]
    fn a_tail_in_memory_reads_as_the_file_reader_reads_a_file_past_the_scan_bound() {
        let bound = super::super::transcript_jsonl::HISTORY_SCAN_MAX_BYTES as usize;
        let mut big = Vec::with_capacity(bound + 8192);
        let mut n = 0u64;
        while big.len() < bound + 4099 {
            big.extend_from_slice(assistant_text(&format!("line {n} {}", "x".repeat(37))).as_bytes());
            big.push(b'\n');
            n += 1;
        }
        let path = crate::state_dirs::test_workspace_dir("transcript-big").join("session.jsonl");
        std::fs::write(&path, &big).unwrap();
        let summary = |history: TranscriptHistory| {
            (
                format!("{:?}", history.events),
                history.parsed_items,
                history.omitted_items,
                format!("{:?}", history.counts),
            )
        };
        let from_file = summary(read_transcript_history(&path).unwrap());
        let _ = std::fs::remove_file(&path);
        let from_last_bytes = summary(read_history_from_tail(&big[big.len() - bound..], true).unwrap());
        let from_everything = summary(read_history_from_tail(&big, false).unwrap());
        assert!(from_file.1 > 0, "the file holds something to compare");
        assert_eq!(from_file, from_last_bytes);
        assert_eq!(from_file, from_everything);
    }

    #[test]
    fn a_tail_past_the_scan_bound_is_cut_to_its_last_32_mib_and_read_as_mid_file() {
        let conversation_id = setup();
        let bound = super::super::transcript_jsonl::HISTORY_SCAN_MAX_BYTES as usize;
        let line = format!("{}\n", assistant_text("filler"));
        let mut whole = Vec::with_capacity(bound + 8192);
        while whole.len() < bound + 4096 {
            whole.extend_from_slice(line.as_bytes());
        }
        let excess = whole.len() - bound;
        // Whatever the caller says about where the bytes start, more than the bound is a tail that starts mid file.
        let long = seed_from_tail(&whole, false, &conversation_id, "sess-long", "sess-long.jsonl");
        let cut = seed_from_tail(&whole[excess..], true, &conversation_id, "sess-long", "sess-long.jsonl");
        assert_eq!(comparable(long), comparable(cut));
    }

    #[test]
    fn a_cli_segment_at_the_end_of_b_is_closed_before_the_first_live_prompt() {
        let conversation_id = setup();
        let path = write_transcript(include_str!("../../tests/fixtures/history_b/task_notification.jsonl"));
        let mut restored = seed_from(Ok(path), &conversation_id, "sess-b-cli");
        assert_eq!(restored.active_turn_id, None);
        assert!(!restored.assistant_message_open);
        assert_eq!(restored.turn_starts.len(), 1);
        assert_eq!(restored.transcript.len(), 2);
        assert_eq!(restored.user_prompts.len(), 1);
        assert!(restored.turn_endings.is_empty());
        restored.apply(&AgentDomainEvent::UserPromptSubmitted {
            text: "Next step".into(),
        });
        restored.apply(&AgentDomainEvent::TurnStarted { turn_id: "live".into() });
        assert_eq!(restored.active_turn_id.as_deref(), Some("live"));
    }

    /// A stored turn of the CLI's own as the last thing in the file: the restored turn is closed, so
    /// the first prompt after the resume is taken rather than refused as a turn already running
    /// (`AgentConversation::send_turn` refuses exactly when `active_turn_id` is set).
    #[test]
    fn a_restored_projection_accepts_the_first_prompt() {
        let conversation_id = setup();
        let start = TurnStartRecord {
            seq: 3,
            turn_id: "cli-9".into(),
            origin: TurnOrigin::Cli,
            cause: Some(TurnCause::of_kind(TurnCauseKind::Scheduled)),
        };
        let parts = store::HistoryParts {
            user_prompts: vec![prompt(1, "check every hour")],
            transcript: vec![message(2, "Scheduled."), message(4, "Checked; all green.")],
            turn_starts: vec![start],
            ..store::HistoryParts::default()
        };
        let stored = StoredHistory::from_parts(conversation_id.clone(), "sess-cli".into(), parts.clone());
        let events = stored_events(&stored).events;
        assert!(
            matches!(events.last(), Some(AgentDomainEvent::TurnCompleted { turn_id, outcome: TurnOutcome::Completed, .. }) if turn_id == "cli-9"),
            "the last restored turn is closed: {events:?}"
        );
        store_parts(&conversation_id, "sess-cli", parts);

        let mut restored = seed_from(Ok(missing_transcript()), &conversation_id, "sess-cli");
        assert_eq!(restored.active_turn_id, None);
        assert_eq!(restored.turn_starts.len(), 1);
        assert!(
            restored.turn_endings.is_empty(),
            "a restored turn ends completed, with no ending row"
        );

        // The user's first prompt after the resume starts a turn of its own.
        restored.apply(&AgentDomainEvent::UserPromptSubmitted {
            text: "and now?".into(),
        });
        restored.apply(&AgentDomainEvent::TurnStarted {
            turn_id: "t-live".into(),
        });
        assert_eq!(restored.active_turn_id.as_deref(), Some("t-live"));
    }

    /// Every turn a refold starts it also ends, before the next start or prompt of the user's own,
    /// and a message read inside a turn does not end it.
    #[test]
    fn every_restored_turn_start_is_closed_before_the_next_start_or_prompt() {
        let start = |seq: u64, id: &str| TurnStartRecord {
            seq,
            turn_id: id.into(),
            origin: TurnOrigin::Cli,
            cause: None,
        };
        let stored = StoredHistory::from_parts(
            "c".into(),
            "s".into(),
            store::HistoryParts {
                user_prompts: vec![
                    UserPromptRecord {
                        seq: 2,
                        text: "read inside".into(),
                        delivered: true,
                        attachments: Vec::new(),
                    },
                    prompt(5, "mine"),
                ],
                turn_starts: vec![start(1, "cli-a"), start(3, "cli-b")],
                ..store::HistoryParts::default()
            },
        );
        let shape: Vec<String> = stored_events(&stored)
            .events
            .iter()
            .map(|event| match event {
                AgentDomainEvent::TurnStarted { turn_id } => format!("start {turn_id}"),
                AgentDomainEvent::TurnAttributed { .. } => "attributed".into(),
                AgentDomainEvent::TurnCompleted { turn_id, .. } => format!("end {turn_id}"),
                AgentDomainEvent::UserMessageDelivered { client_message_id, .. } => {
                    format!("delivered {client_message_id}")
                }
                AgentDomainEvent::UserPromptSubmitted { text } => format!("prompt {text}"),
                other => panic!("unexpected {other:?}"),
            })
            .collect();
        assert_eq!(
            shape,
            vec![
                "start cli-a",
                "attributed",
                "delivered history-2",
                "end cli-a",
                "start cli-b",
                "attributed",
                "end cli-b",
                "prompt mine"
            ]
        );
    }
    #[test]
    fn a_refold_restores_nested_children_without_main_rows() {
        let conversation_id = setup();
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../../../agent-ui/web/src/fixtures/subagent-frames.json")).unwrap();
        for case in fixture["cases"].as_array().unwrap() {
            let mut live = AgentSessionProjection::default();
            let events: Vec<AgentDomainEvent> = serde_json::from_value(case["events"].clone()).unwrap();
            for event in events {
                live.apply(&event);
            }
            store_copy(&conversation_id, "children", vec![], vec![], live.tool_calls.clone());
            let restored = seed_from(Ok(missing_transcript()), &conversation_id, "children");
            assert_eq!(restored.tool_calls.len(), live.tool_calls.len());
            assert!(restored.transcript.is_empty());
            assert!(restored.thinking.is_empty());
            assert!(restored.active_turn_id.is_none());
            let mut marks = 0;
            fn check(saved: &[ToolCallRecord], loaded: &[ToolCallRecord], marks: &mut usize) {
                for (saved, loaded) in saved.iter().zip(loaded) {
                    assert_eq!(saved.tool_use_id, loaded.tool_use_id);
                    assert_eq!(saved.turn_id, loaded.turn_id);
                    assert_eq!(saved.result, loaded.result);
                    assert_eq!(saved.background, loaded.background);
                    assert_eq!(saved.denied, loaded.denied);
                    assert_eq!(saved.changed_input, loaded.changed_input);
                    if let Some(stream) = &saved.children {
                        *marks += 1;
                        let restored = loaded.children.as_ref().unwrap();
                        assert_eq!(
                            stream.transcript.iter().map(|row| &row.text).collect::<Vec<_>>(),
                            restored.transcript.iter().map(|row| &row.text).collect::<Vec<_>>()
                        );
                        assert_eq!(
                            stream.thinking.iter().map(|row| &row.text).collect::<Vec<_>>(),
                            restored.thinking.iter().map(|row| &row.text).collect::<Vec<_>>()
                        );
                        assert_eq!(stream.tool_calls.len(), restored.tool_calls.len());
                        check(&stream.tool_calls, &restored.tool_calls, marks);
                    }
                }
            }
            check(&live.tool_calls, &restored.tool_calls, &mut marks);
            assert!(marks > 0);
            let parts = store::newest_that_fit(store::HistoryView::of(&restored));
            assert_eq!(parts.tool_calls, restored.tool_calls);
        }
    }
}
