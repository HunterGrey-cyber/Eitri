// agent/src/history/load.rs
//! Builds the initial projection for a resumed conversation from one stored source.
//!
//! `AgentSessionProjection::apply` is the only `seq` allocator. Both sources become events:
//! B supplies parsed events, while A's three collections are merged by stored `seq` and
//! converted to events. Stored sequence numbers determine order but are not copied into
//! the projection.
//!
//! `ConversationIngest::start` receives the complete seed before spawning its thread.
//! Live events therefore receive sequence numbers after all historical events. The loader
//! clears `assistant_message_open` so the first live reply cannot join the last stored reply.
//!
//! The CLI transcript, B, takes precedence when usable. Falling back to the stored copy,
//! A, is announced to the user. The sources are not merged because CLI compaction can
//! make their contents differ.
//!
//! Restored content is for display. It does not determine permissions or resume safety,
//! and this loader neither composes it into model input nor writes it back to disk.

use std::path::{Path, PathBuf};

use crate::projection::{
    AgentDomainEvent, AgentSessionProjection, ContentKind, HistoryNotice, HistorySource, ToolCallRecord,
    TranscriptMessage, UserPromptRecord,
};

use super::store::{self, HistoryLoadError, StoredHistory};
use super::transcript_jsonl::{read_transcript_history, TranscriptHistory};

/// The projection a resumed conversation starts from.
///
/// Called from `AgentConversation::resume` after acquiring the lease and before asking the provider
/// to resume. The stability probe has found no transcript writer; the lease excludes other
/// cooperating clients, and this provider has not yet emitted live events.
///
/// Never fails. Missing files, denied access, unsupported formats, or an unavailable stored copy
/// leave a projection with less history so history loading does not prevent a working resume.
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
    // Close the assistant message on every return path. Restored assistant text leaves this flag
    // true; the first live delta would otherwise append to the previous session's reply and render
    // both replies as one bubble.
    projection.assistant_message_open = false;
    // Neither source restores thinking today, so this is already false; stated so a later source
    // that does cannot leave a restored thought open for the first live one to be appended to.
    projection.thinking_open = false;
    projection
}

/// Counts the conversational rows held by the projection after events have been folded.
/// Both loading paths use this count so the notice matches the rows the panel draws.
/// `apply` coalesces consecutive assistant text into one `TranscriptMessage`, so input
/// event counts and stored item counts can differ from the displayed row count.
///
/// Called before and after loading, so the difference remains correct even if the
/// projection already contains items.
///
/// `omitted_items` counts items discarded before folding, a different unit from restored
/// rows. Their row count cannot be recovered because merging would depend on discarded
/// neighbours.
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

/// Reads the transcript and determines whether to use it or fall back to stored history.
///
/// A non-empty file yielding no conversational items triggers fallback. The transcript has no
/// schema version, so an unrecognized format can appear as nothing parsed. An empty file is
/// different: zero bytes represents an empty conversation and does not trigger fallback.
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
    match read_transcript_history(&path) {
        Ok(history) if history.parsed_items == 0 => {
            // Report that nothing could be restored without implying the file could not be read.
            // A healthy transcript containing only slash-command echoes, `isMeta` injections, or a compaction
            // summary can yield no items because the allow-list rejects them. Keep the writer version in
            // the reason to help diagnose a format change when there is one.
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

/// Describes missing files and denied access directly, preserving the underlying error for
/// other read failures.
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

/// Folds transcript events into the projection and records their source.
///
/// An empty conversation leaves no notice. A session with no earlier messages needs no
/// explanation row.
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
        // A session that never reached a turn boundary has no stored copy. Leave the panel empty
        // without adding a notice row.
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
        // Each stored `TranscriptMessage` is already coalesced. `apply` closes a run only on a prompt
        // or turn boundary, and the stored events contain no turn boundaries. Explicitly close each
        // assistant message so adjacent stored messages do not merge into one row.
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

/// Merges the three stored collections by `seq` and reconstructs their events.
///
/// Stored sequence numbers recover the order across collections, then are discarded. `apply`
/// assigns new contiguous numbers from the current projection's allocator.
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
                // Before the completion-skip below: the mark belongs to the call, not to its result,
                // and an empty id is no link (a held change for it could never be attached).
                if let Some(change) = call.changed_input.as_ref().filter(|_| !call.tool_use_id.is_empty()) {
                    events.push(AgentDomainEvent::ToolInputChanged {
                        tool_use_id: call.tool_use_id.clone(),
                        change: change.clone(),
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
    events
}

/// Logs parser counts and discarded data to stderr.
///
/// Keep parser diagnostics out of the notice: they include attachments and side records, so they
/// cannot be presented as counts of conversation rows. They help diagnose transcript format changes.
///
/// Log `parsed` and `restored` separately. Consecutive assistant text can fold into one row, so the
/// parser's item count can exceed the panel's restored row count. `restored` uses the panel's
/// meaning of a row. It is `None` when zero items were parsed and nothing was folded; that case
/// also leaves no notice.
fn log_counts(path: &Path, history: &TranscriptHistory, restored: Option<usize>) {
    let counts = history.counts;
    eprintln!(
        "agent: parsed {items} {item_word} from {path} (restored {restored} {row_word}, omitted by truncation: {omitted}, \
         skipped lines: {skipped} [malformed {malformed}, oversized {oversized}, attachment \
         {attachment}, system {system}, side record {side}, subagent {subagent}, unknown type \
         {unknown}, sidechain {sidechain}, rejected user {rejected}, empty prompt {empty}], \
         dropped blocks: {blocks}, orphan tool results: {orphans}, written by claude {version})",
        items = history.parsed_items,
        // Use singular nouns for a count of one, matching the panel notice. These diagnostics are
        // read beside that notice and should describe the same counts consistently.
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
            delivered: false,
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

    /// A nonempty transcript with no parsed conversational items falls back to the stored copy.
    /// The file has no schema version, so this can indicate an unsupported format. It can also
    /// happen with valid attachment-only files, slash-command echoes or `isMeta` user lines.
    /// The notice must describe what was observed without claiming the file is broken.
    /// The test checks the full reason sentence because it is embedded in the panel's notice.
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
    // What the notice's count MEANS
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

    /// Folds real top-level transcripts and checks each notice against the resulting projection.
    /// This complements the hand-built case above. The test reports how many files produced
    /// history so equality is not mistaken for useful coverage when none did.
    /// Assertions and diagnostics use shapes and counts, without printing or comparing
    /// conversation content.
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
