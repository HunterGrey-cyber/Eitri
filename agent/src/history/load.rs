// agent/src/history/load.rs
//! Turning one of the two stored records into the projection a resumed conversation starts from.
//!
//! Design: `docs/superpowers/specs/2026-09-20-resume-history-design.md` §4 (one allocator, where
//! the load happens), §7 (B failing to A) and §8 (B wins, always).
//!
//! Four properties this module holds, each of which is a ruling with a reason:
//!
//! 1. **There is exactly one `seq` allocator, and it is `AgentSessionProjection::apply`.** Both
//!    sources are REFOLDED: B's parser already produces `AgentDomainEvent`s, and A's three stored
//!    collections are merged by their on-disk `seq` and turned back into events here. A stored
//!    `seq` is a sort key and is never written back into the projection, so "no item has a number
//!    that something other than `apply` chose" holds by construction rather than by convention.
//! 2. **The seed is complete before the first live event exists.** `ConversationIngest::start`
//!    takes it as a parameter and spawns its thread afterwards, so every live `seq` is greater than
//!    every historical one with no renumbering, no offset, and nothing for the frontend to change.
//! 3. **`assistant_message_open` is false when this module returns.** Otherwise the first live
//!    reply is appended to the last historical assistant message and the two render as one.
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
    AgentDomainEvent, AgentSessionProjection, ContentKind, HistoryNotice, HistorySource, ToolCallRecord,
    TranscriptMessage, UserPromptRecord,
};

use super::store::{self, HistoryLoadError, StoredHistory};
use super::transcript_jsonl::{read_transcript_history, TranscriptHistory};

/// The projection a resumed conversation starts from.
///
/// Called from `AgentConversation::resume` after the lease is held and before the provider is asked
/// for anything (§4.2): the stability probe has just confirmed nothing else is writing the
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
/// multi-threaded test binary, which this crate has a whole lock (`CLAUDE_CONFIG_DIR_TEST_LOCK`)
/// to contain. Passing the path makes every branch reachable with an ordinary temporary file.
pub(crate) fn seed_from(
    transcript_path: std::io::Result<PathBuf>,
    conversation_id: &str,
    provider_session_id: &str,
) -> AgentSessionProjection {
    let mut projection = AgentSessionProjection::default();
    let attempt = read_b(transcript_path);
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
    // §4.4, and the last thing this module does on every path. A history whose final item is
    // assistant text leaves this flag true, and the first live delta would then be appended to that
    // message -- last session's reply and this one's rendered as a single bubble.
    projection.assistant_message_open = false;
    projection
}

/// How many conversational items a projection is holding -- the one basis both paths report.
///
/// **Counted AFTER the fold, and that is the whole point.** Both sources hand `apply` a stream of
/// events, and `apply` coalesces consecutive assistant text into a single `TranscriptMessage`
/// (`projection.rs`), so the number of events, slots or stored records that went in is not the
/// number of rows that come out. Reporting the input count was the defect the whole-branch review
/// found on 2026-09-20: over all 45 real top-level transcripts on this machine, 13 disagreed with
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
    projection.user_prompts.len() + projection.transcript.len() + projection.tool_calls.len()
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

/// §7.1's table, as code.
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
    // backend-start worker, which `collect_pending_start` polls with `try_recv` and no timeout
    // while the window is open (`shell/src/agent_panel.rs`) -- the 3-second bound exists only on
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
    match read_transcript_history(&path) {
        Ok(history) if history.parsed_items == 0 => {
            // **Says what was observed, not what it might mean.** This arm is §7.1's "the format
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

/// The three wordings §7.2 names, and a fall-through that carries the real error rather than
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
/// explain, and a row saying "no earlier messages" on a session that genuinely had none is noise
/// (§7.3).
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
/// The caps are applied HERE as well as at write time, through the same `from_collections` the
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
        // stored copy. §7.3 -- no notice, no row, the panel is simply the one it is today.
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
    let capped = StoredHistory::from_collections(
        stored.conversation_id,
        stored.provider_session_id,
        stored.user_prompts,
        stored.transcript,
        stored.tool_calls,
    );
    let kept = capped.item_count();
    if kept == 0 {
        return;
    }
    let items_before = restorable_item_count(projection);
    for event in stored_events(&capped) {
        projection.apply(&event);
        // §4.4's second half, and the reason A cannot simply be handed to `apply` as a stream: each
        // stored `TranscriptMessage` is an already-COALESCED assistant message, and `apply` closes
        // a run only on a prompt or a turn boundary -- neither of which A stores. Two adjacent
        // stored messages would merge back into one. Closing after every one keeps them apart.
        if matches!(
            event,
            AgentDomainEvent::ContentDelta {
                kind: ContentKind::Text,
                ..
            }
        ) {
            projection.assistant_message_open = false;
        }
    }
    projection.history = Some(HistoryNotice {
        source: HistorySource::EitriCopy,
        restored_items: restorable_item_count(projection) - items_before,
        // Counted in STORED items, not in rows: it is what this load's own truncation dropped,
        // and a dropped item has no row to be counted as. See `restorable_item_count`'s note on
        // why the two numbers in one sentence are in different units and why that is the honest
        // pair rather than a mistake.
        omitted_items: Some(before - kept),
        upto_seq: projection.last_revision,
        source_path: path.to_string_lossy().to_string(),
        attempted_transcript_path: attempted_transcript_path.map(|p| p.to_string_lossy().to_string()),
        fallback_reason: Some(fallback_reason),
        // A has no writer version: it is this project's own file, and its format is pinned by
        // `HISTORY_FORMAT_VERSION`, which `store::load` has already checked.
        writer_version: None,
    });
}

/// A's three collections, merged by their stored `seq` and turned back into the events that
/// produced them.
///
/// The stored `seq` is used HERE and only here: as the sort key that recovers the one order the
/// three collections were written in. The numbers themselves are then thrown away -- `apply`
/// assigns new ones, contiguous and this process's own (§4.1, invariant 2).
fn stored_events(stored: &StoredHistory) -> Vec<AgentDomainEvent> {
    enum Item<'a> {
        Prompt(&'a UserPromptRecord),
        Message(&'a TranscriptMessage),
        Call(&'a ToolCallRecord),
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
    // A stable sort on (seq, kind): two items cannot share a `seq` in a file this build wrote, but
    // a corrupt or hand-edited one could, and "whatever order the vectors happened to be in" is not
    // an answer a reader can reproduce.
    items.sort_by_key(|(seq, kind, _)| (*seq, *kind));

    let mut events = Vec::new();
    for (_, _, item) in items {
        match item {
            Item::Prompt(prompt) => events.push(AgentDomainEvent::UserPromptSubmitted {
                text: prompt.text.clone(),
            }),
            Item::Message(message) => events.push(AgentDomainEvent::ContentDelta {
                // A stores no turn ids, and nothing renders one: `serialize_snapshot_for_js` does
                // not send `turn_id` to the frontend at all.
                turn_id: String::new(),
                kind: ContentKind::Text,
                text: message.text.clone(),
            }),
            Item::Call(call) => {
                events.push(AgentDomainEvent::ToolCallStarted {
                    turn_id: call.turn_id.clone(),
                    tool_use_id: call.tool_use_id.clone(),
                    name: call.name.clone(),
                    input: call.input.clone(),
                });
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
    events
}

/// One line, to stderr, with everything the parse threw away.
///
/// These numbers are deliberately not in the notice (§3.4): "1,431 records were not loaded" next to
/// a number that also counts attachments and side records would be a figure nobody can explain. In
/// the log they are the first thing a person debugging a format change wants.
///
/// **`parsed` and `restored` are two different numbers and this line names both** (re-review,
/// 2026-09-20). It used to print the parser's own slot count under the word "restored", which is
/// the panel's word for the rows it draws: a transcript of six adjacent assistant lines logged
/// "restored 6 items" while the notice beside it said 1, because consecutive assistant text folds
/// into one row. A debugger reads this line first, so the two must not disagree about a word they
/// share. `restored` is `None` only where nothing was folded at all -- a transcript that parsed to
/// zero items, which leaves no notice either (§7.3).
fn log_counts(path: &Path, history: &TranscriptHistory, restored: Option<usize>) {
    let counts = history.counts;
    eprintln!(
        "agent: parsed {items} {item_word} from {path} (restored {restored} {row_word}, omitted by truncation: {omitted}, \
         skipped lines: {skipped} [malformed {malformed}, oversized {oversized}, attachment \
         {attachment}, system {system}, side record {side}, subagent {subagent}, unknown type \
         {unknown}, sidechain {sidechain}, rejected user {rejected}, empty prompt {empty}], \
         dropped blocks: {blocks}, orphan tool results: {orphans}, written by claude {version})",
        items = history.parsed_items,
        // Agreement, for the same reason the panel's own notice has it (§5.5 Amendment 3): this
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

    // -------------------------------------------------------------------------------------
    // Ordering: history below, live above, one allocator (§4.1, §4.2, invariants 1 and 2)
    // -------------------------------------------------------------------------------------

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

    // -------------------------------------------------------------------------------------
    // `assistant_message_open` (§4.4, invariant 5)
    // -------------------------------------------------------------------------------------

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

    /// §4.4's second half, which is specific to A: a stored `TranscriptMessage` is an ALREADY
    /// coalesced message, and `apply` closes a run only on a prompt or a turn boundary -- neither
    /// of which A stores. Without the explicit close after each one, two stored messages merge back
    /// into a single row.
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
    // B failing to A (§7), and B winning (§8)
    // -------------------------------------------------------------------------------------

    /// §7.1's middle row: a transcript with bytes in it that yields no conversational item at all
    /// falls back, because the file carries no schema version and "nothing parsed" is the only
    /// signal a format change has. The user is told, and the reason names the CLI that wrote the
    /// file so the next person can date the change.
    ///
    /// **The reason says what was observed, not what it was taken to mean** (whole-branch review,
    /// 2026-09-20). This exact input -- a file of nothing but attachment lines -- is a file that
    /// read perfectly; a real session whose user lines are all slash-command echoes or `isMeta`
    /// reaches this arm the same way, with no format change anywhere. The wording is therefore
    /// pinned here, and it is pinned as a whole sentence rather than as a substring: the panel's
    /// own line embeds it, and the two drifting apart is how a user ends up inspecting a healthy
    /// file.
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

    /// §8: B wins even when A is longer. No merging, no difference detection -- the CLI's own file
    /// is closer to what a resumed session will actually behave like, and showing the longer record
    /// would suggest the agent remembers things it does not.
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

    /// §7.1's last row, and the negative control for the test above it: zero bytes is zero
    /// conversation, which is a real answer rather than a failure. The stored copy below is
    /// deliberately non-empty, so falling back would be visible.
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

    /// §7.3: neither record exists. No notice, no row, no invented explanation -- for a session
    /// that never said anything, "no history" is noise.
    #[test]
    fn neither_source_available_restores_nothing_and_says_nothing() {
        let conversation_id = setup();
        let projection = seed_from(Ok(missing_transcript()), &conversation_id, "sess-none");
        assert!(projection.history.is_none());
        assert!(all_seqs(&projection).is_empty());
        assert_eq!(projection.last_revision, 0);
    }

    /// The first row of §7.1: no `CLAUDE_CONFIG_DIR` and no `HOME`, so no path could be built at
    /// all. A still loads, and the notice carries no attempted path because there is none to name.
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

    // -------------------------------------------------------------------------------------
    // What history may never carry (§3.2, §4.3, invariants 3 and 4)
    // -------------------------------------------------------------------------------------

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

    // -------------------------------------------------------------------------------------
    // Tool calls through A (§3.3)
    // -------------------------------------------------------------------------------------

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
    // What the notice's count MEANS (whole-branch review, 2026-09-20)
    // -------------------------------------------------------------------------------------

    /// **`restored_items` is what the panel draws, not what the parser kept.**
    ///
    /// `apply` coalesces consecutive assistant text into ONE `TranscriptMessage`, so a transcript
    /// of four adjacent assistant lines is four slots to the reader and one row to the panel. The
    /// notice used to carry the reader's number, which on this machine's own corpus over-stated
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
    /// backend-start worker, which `collect_pending_start` polls with `try_recv` and no timeout --
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

    /// The reviewer's own method, against the real corpus: fold every real top-level transcript on
    /// this machine and check the notice against the projection it produced.
    ///
    /// The hermetic test above pins the property on one hand-built input. This one is what says the
    /// property survives real files -- the same 45 files the whole-branch review folded to find the
    /// defect. It prints how many of them fold at all, because a run where NONE folded would make
    /// the equality vacuous and the reader deserves to see that rather than a green tick.
    ///
    /// Shapes and counts only. Nothing here reads, prints or compares conversation content.
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
            assert!(
                notice.restored_items <= parsed.parsed_items,
                "{}: folding cannot produce MORE rows than slots",
                path.display()
            );
            if notice.restored_items < parsed.parsed_items {
                folded += 1;
            }
        }
        assert!(restored_any > 0, "no real file restored anything");
        eprintln!("{folded} of {restored_any} real transcripts fold at least one pair of adjacent assistant slots");
    }
}
