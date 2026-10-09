// agent/src/history/store.rs
//! Stores A, the application's copy of conversation history, beside conversation records.
//! It is the fallback for [`super::transcript_jsonl`], which reads the Claude CLI transcript,
//! B. The CLI transcript takes precedence when usable. The stored copy supports resumes
//! when the CLI configuration or transcript is unavailable or no conversation can be parsed.
//!
//! History lives outside `conversations/`; see `state_dirs::history_dir`. It carries its
//! own format `version`, and [`load`] rejects an unknown version in full. The CLI's version
//! field identifies a release rather than a schema, so it cannot provide this check.
//!
//! Only the six record types implement `Deserialize`; `AgentSessionProjection` does not.
//! Added fields in `StoredHistory` or those record types need `#[serde(default)]` so files
//! written before the fields existed still load.

use crate::projection::{ToolCallRecord, TranscriptMessage, UserPromptRecord};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// The format this build writes and the only one it reads.
///
/// Bump it when a change cannot be expressed as an added `#[serde(default)]` field -- i.e. when an
/// old file would be MISREAD rather than merely read with a default. Every file at any other
/// version is refused whole by [`load`]; that is the point of having the number.
pub const HISTORY_FORMAT_VERSION: u32 = 1;

/// One conversation's stored history.
///
/// Contains prompts, assistant messages, and tool calls, rather than a serialized projection.
/// Live state is excluded: no projection `status`, `active_turn_id`, `session_id`,
/// `provider_session_id`, `model`, `cwd`, `usage`, or `pending_permissions`. A restored permission
/// card would have no live provider request to answer.
///
/// There is no `next_seq`. Loading refolds the items: stored `seq` values determine their order,
/// then the projection's `apply` assigns new numbers. Persisting a counter would create a second
/// source of sequence numbers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredHistory {
    pub version: u32,
    pub conversation_id: String,
    pub provider_session_id: String,
    /// Milliseconds since the Unix epoch, as a string -- the same stamp and the same reason as
    /// `ConversationRecord::updated_at` (`conversation::epoch_millis`).
    pub written_at: String,
    #[serde(default)]
    pub user_prompts: Vec<UserPromptRecord>,
    #[serde(default)]
    pub transcript: Vec<TranscriptMessage>,
    #[serde(default)]
    pub tool_calls: Vec<ToolCallRecord>,
}

/// Why a stored history could not be loaded. Three outcomes, kept apart on purpose.
#[derive(Debug)]
pub enum HistoryLoadError {
    /// No such file, no permission, a read that failed. `NotFound` is the ordinary case for a
    /// session that has never reached a turn boundary and is not an error worth reporting anywhere.
    Io(std::io::Error),
    /// The file is not JSON, or not JSON shaped like this version's `StoredHistory`.
    Malformed(serde_json::Error),
    /// The file declares an unsupported format version. Refuse the whole file without salvaging
    /// fields or treating it as a parse failure. Return both the found and expected versions so the
    /// caller can explain the mismatch.
    UnknownVersion { found: u32, expected: u32 },
}

impl std::fmt::Display for HistoryLoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "{e}"),
            Self::Malformed(e) => write!(f, "stored history is malformed: {e}"),
            Self::UnknownVersion { found, expected } => {
                write!(
                    f,
                    "stored history is format version {found}, and this build reads {expected}"
                )
            }
        }
    }
}

impl std::error::Error for HistoryLoadError {}

impl StoredHistory {
    /// Builds a truncated snapshot from the three projection collections.
    ///
    /// `flush_history` writes the whole projection rather than appending to the stored copy.
    /// After a resume from the CLI transcript, the next completed turn replaces the stored
    /// copy with the restored view plus the new turn. Older content present only in the stored
    /// copy is lost. For example, when `HISTORY_SCAN_MAX_BYTES` limits the restored transcript
    /// to its tail, the next snapshot contains only that restored history and new content.
    ///
    /// Each snapshot is bounded. Truncation uses the same caps and direction as the reader:
    /// `HISTORY_MAX_ITEMS` items and `HISTORY_MAX_CHARS` characters, taking whole items from
    /// newest to oldest. The newest item is kept even if it alone exceeds the budget. Matching
    /// the reader prevents repeated writes and reads from discarding different sets of items.
    pub fn from_collections(
        conversation_id: String,
        provider_session_id: String,
        user_prompts: Vec<UserPromptRecord>,
        transcript: Vec<TranscriptMessage>,
        tool_calls: Vec<ToolCallRecord>,
    ) -> Self {
        let (user_prompts, transcript, tool_calls) = truncate(user_prompts, transcript, tool_calls);
        Self {
            version: HISTORY_FORMAT_VERSION,
            conversation_id,
            provider_session_id,
            written_at: crate::conversation::epoch_millis(),
            user_prompts,
            transcript,
            tool_calls,
        }
    }

    /// How many conversational items this holds -- the same three things the reader counts as items
    /// (a tool result is not one; it belongs to the call it completes).
    pub fn item_count(&self) -> usize {
        self.user_prompts.len() + self.transcript.len() + self.tool_calls.len()
    }
}

/// Which collection an item came from, so truncation can put the survivors back where they belong.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Which {
    Prompt,
    Message,
    ToolCall,
}

/// Keeps the newest items that fit both caps, and returns each collection with its survivors in
/// their original order.
///
/// Mirrors `transcript_jsonl::read_history_from`'s eviction exactly, stated the other way round:
/// that one appends and evicts from the front while `len > MAX_ITEMS || (chars > MAX_CHARS && len >
/// 1)`; this one walks newest-first and stops at the first item that does not fit. Both end with at
/// most `HISTORY_MAX_ITEMS` items and at most `HISTORY_MAX_CHARS` characters unless a single item
/// exceeds the budget by itself, in which case both keep it. "Stops" rather than "skips": an item
/// too large to fit ends the walk, so what survives is always a contiguous run of the newest items
/// and never a mix of old small ones with a hole where a big one was.
fn truncate(
    user_prompts: Vec<UserPromptRecord>,
    transcript: Vec<TranscriptMessage>,
    tool_calls: Vec<ToolCallRecord>,
) -> (Vec<UserPromptRecord>, Vec<TranscriptMessage>, Vec<ToolCallRecord>) {
    truncate_to(
        user_prompts,
        transcript,
        tool_calls,
        super::HISTORY_MAX_ITEMS,
        super::HISTORY_MAX_CHARS,
    )
}

/// [`truncate`] with the two caps as parameters.
///
/// Parameterised for ONE reason: a test that had to build `HISTORY_MAX_ITEMS * 2` real items to
/// observe the item cap, and a `HISTORY_MAX_CHARS`-sized string to observe the character one, would
/// be slow enough that nobody writes the interesting cases. The product path never passes anything
/// but the constants -- [`truncate`] is the only non-test caller.
fn truncate_to(
    mut user_prompts: Vec<UserPromptRecord>,
    mut transcript: Vec<TranscriptMessage>,
    mut tool_calls: Vec<ToolCallRecord>,
    max_items: usize,
    max_chars: usize,
) -> (Vec<UserPromptRecord>, Vec<TranscriptMessage>, Vec<ToolCallRecord>) {
    let mut items: Vec<(u64, Which, usize, usize)> = Vec::new();
    for (index, item) in user_prompts.iter().enumerate() {
        items.push((item.seq, Which::Prompt, index, item.text.chars().count()));
    }
    for (index, item) in transcript.iter().enumerate() {
        items.push((item.seq, Which::Message, index, item.text.chars().count()));
    }
    for (index, item) in tool_calls.iter().enumerate() {
        items.push((item.seq, Which::ToolCall, index, tool_call_chars(item)));
    }
    // Newest first. The tie-break on `Which` and the index keeps the order total, so two items that
    // somehow share a seq are cut deterministically rather than by sort luck.
    items.sort_by_key(|(seq, which, index, _)| std::cmp::Reverse((*seq, *which as u8, *index)));

    let mut keep_prompts = vec![false; user_prompts.len()];
    let mut keep_messages = vec![false; transcript.len()];
    let mut keep_tool_calls = vec![false; tool_calls.len()];
    let mut kept = 0usize;
    let mut chars = 0usize;
    for (_, which, index, cost) in items {
        if kept >= max_items {
            break;
        }
        // Keep at least one item even when it exceeds the character budget. Reporting history with
        // zero visible rows would be more confusing than showing one oversized row.
        if kept > 0 && chars + cost > max_chars {
            break;
        }
        match which {
            Which::Prompt => keep_prompts[index] = true,
            Which::Message => keep_messages[index] = true,
            Which::ToolCall => keep_tool_calls[index] = true,
        }
        kept += 1;
        chars += cost;
    }

    retain_by(&mut user_prompts, &keep_prompts);
    retain_by(&mut transcript, &keep_messages);
    retain_by(&mut tool_calls, &keep_tool_calls);
    (user_prompts, transcript, tool_calls)
}

/// What [`truncate`] keeps of copies of the three collections, copying only the survivors: this runs
/// under the lock the UI's reads of the conversation take, and a long session's collections hold
/// far more than the caps keep. Collections in `seq` order (as the projection keeps them) are walked
/// newest first from their ends and stop at the caps, so the work is what survives; anything else
/// is copied whole and truncated as before.
pub(crate) fn newest_that_fit(
    user_prompts: &[UserPromptRecord],
    transcript: &[TranscriptMessage],
    tool_calls: &[ToolCallRecord],
) -> (Vec<UserPromptRecord>, Vec<TranscriptMessage>, Vec<ToolCallRecord>) {
    newest_that_fit_to(
        user_prompts,
        transcript,
        tool_calls,
        super::HISTORY_MAX_ITEMS,
        super::HISTORY_MAX_CHARS,
    )
}

fn newest_that_fit_to(
    user_prompts: &[UserPromptRecord],
    transcript: &[TranscriptMessage],
    tool_calls: &[ToolCallRecord],
    max_items: usize,
    max_chars: usize,
) -> (Vec<UserPromptRecord>, Vec<TranscriptMessage>, Vec<ToolCallRecord>) {
    fn in_order(seqs: impl Iterator<Item = u64>) -> bool {
        let mut last = None;
        seqs.into_iter().all(|seq| {
            let ok = last.is_none_or(|last| last <= seq);
            last = Some(seq);
            ok
        })
    }
    if !in_order(user_prompts.iter().map(|p| p.seq))
        || !in_order(transcript.iter().map(|m| m.seq))
        || !in_order(tool_calls.iter().map(|c| c.seq))
    {
        return truncate_to(
            user_prompts.to_vec(),
            transcript.to_vec(),
            tool_calls.to_vec(),
            max_items,
            max_chars,
        );
    }
    // Each collection's survivors are a run at its end (the walk stops, never skips), so a cursor per
    // collection says where its run starts. The next item is the newest of the three ends, by the
    // same key `truncate_to` sorts on.
    let (mut prompts, mut messages, mut calls) = (user_prompts.len(), transcript.len(), tool_calls.len());
    let mut kept = 0usize;
    let mut chars = 0usize;
    while kept < max_items {
        let ends = [
            prompts
                .checked_sub(1)
                .map(|i| ((user_prompts[i].seq, Which::Prompt as u8, i), Which::Prompt)),
            messages
                .checked_sub(1)
                .map(|i| ((transcript[i].seq, Which::Message as u8, i), Which::Message)),
            calls
                .checked_sub(1)
                .map(|i| ((tool_calls[i].seq, Which::ToolCall as u8, i), Which::ToolCall)),
        ];
        let Some((_, which)) = ends.into_iter().flatten().max_by_key(|(key, _)| *key) else {
            break;
        };
        let cost = match which {
            Which::Prompt => user_prompts[prompts - 1].text.chars().count(),
            Which::Message => transcript[messages - 1].text.chars().count(),
            Which::ToolCall => tool_call_chars(&tool_calls[calls - 1]),
        };
        if kept > 0 && chars + cost > max_chars {
            break;
        }
        match which {
            Which::Prompt => prompts -= 1,
            Which::Message => messages -= 1,
            Which::ToolCall => calls -= 1,
        }
        kept += 1;
        chars += cost;
    }
    (
        user_prompts[prompts..].to_vec(),
        transcript[messages..].to_vec(),
        tool_calls[calls..].to_vec(),
    )
}

fn retain_by<T>(items: &mut Vec<T>, keep: &[bool]) {
    let mut index = 0;
    items.retain(|_| {
        let keep_this = keep[index];
        index += 1;
        keep_this
    });
}

/// What one tool call costs against the character budget: its name, its input, and its result if it
/// has one. The result is weighed with the call rather than on its own because that is how it
/// renders and how the reader budgets it -- a call and its result are one row.
fn tool_call_chars(call: &ToolCallRecord) -> usize {
    let result = call
        .result
        .as_ref()
        .map(|r| super::transcript_jsonl::value_chars(&r.content))
        .unwrap_or(0);
    call.name.chars().count() + super::transcript_jsonl::value_chars(&call.input) + result
}

/// `$XDG_STATE_HOME/eitri/history/<conversation_id>/`.
fn conversation_history_dir(conversation_id: &str) -> std::io::Result<PathBuf> {
    crate::persistence::validate_path_component("conversation_id", conversation_id)?;
    Ok(crate::state_dirs::history_dir()?.join(conversation_id))
}

/// `$XDG_STATE_HOME/eitri/history/<conversation_id>/<provider_session_id>.json`.
///
/// Both components go through `persistence`'s own validator: they are joined into a path with no
/// other checking, so a value shaped like `../../../etc` would otherwise escape the history
/// directory entirely. A 32-hex conversation id and a UUID session id both pass.
pub fn history_path(conversation_id: &str, provider_session_id: &str) -> std::io::Result<PathBuf> {
    crate::persistence::validate_path_component("provider_session_id", provider_session_id)?;
    Ok(conversation_history_dir(conversation_id)?.join(format!("{provider_session_id}.json")))
}

/// Writes one session's history, atomically.
///
/// Through `persistence::write_record`, which is the same temp-file → `sync_all` → `rename` →
/// fsync-the-directory sequence a conversation record gets, and for the same reason: a reader
/// arriving mid-write must see the whole previous file or the whole new one, never a truncated one
/// that parses as nothing. Reusing that function rather than copying it also means the durability
/// argument stays in one place.
///
/// On the FIRST write for a session -- the one that creates the file -- this also sweeps orphans
/// (see [`remove_orphans`]). Only then: a sweep is a `read_dir` plus up to 17 record parses, and
/// paying that on every turn boundary to catch something that can only change when a record is
/// pruned would be wasteful. The sweep runs BEFORE the write, so the file this call is about to
/// create can never be its own victim.
pub fn save(history: &StoredHistory) -> std::io::Result<()> {
    let path = history_path(&history.conversation_id, &history.provider_session_id)?;
    let dir = path.parent().expect("a history path always has a parent directory");
    if !path.exists() {
        remove_orphans(&history.conversation_id);
    }
    crate::persistence::create_private_state_dir(dir, crate::state_dirs::history_dir()?)?;
    let json = serde_json::to_string(history).map_err(std::io::Error::other)?;
    crate::persistence::write_record(&path, json.as_bytes())
}

/// Reads one session's history back.
///
/// The version is checked BEFORE the rest of the document is interpreted, which is what makes an
/// unknown version a refusal rather than a parse error: a future format may legitimately have
/// fields this build would choke on, and "it did not deserialize" would report that as corruption.
pub fn load(conversation_id: &str, provider_session_id: &str) -> Result<StoredHistory, HistoryLoadError> {
    let path = history_path(conversation_id, provider_session_id).map_err(HistoryLoadError::Io)?;
    let text = crate::private_fs::read_private_to_string(&path).map_err(HistoryLoadError::Io)?;
    load_str(&text)
}

/// The version gate and the parse, over text. Split out so a test can drive both without a file.
pub(crate) fn load_str(text: &str) -> Result<StoredHistory, HistoryLoadError> {
    let value: serde_json::Value = serde_json::from_str(text).map_err(HistoryLoadError::Malformed)?;
    // A document with no `version` at all is not "version 0 and therefore unknown" in any useful
    // sense -- it is not this format. `found: 0` says what was read, and the refusal is the same.
    let found = value.get("version").and_then(serde_json::Value::as_u64).unwrap_or(0);
    if found != u64::from(HISTORY_FORMAT_VERSION) {
        return Err(HistoryLoadError::UnknownVersion {
            found: u32::try_from(found).unwrap_or(u32::MAX),
            expected: HISTORY_FORMAT_VERSION,
        });
    }
    serde_json::from_value(value).map_err(HistoryLoadError::Malformed)
}

/// Deletes stored histories for sessions this conversation no longer has a record for.
///
/// A conversation keeps at most `MAX_RECORDS_PER_CONVERSATION` records and drops the oldest when it
/// goes over; the history of a dropped session is then unreachable forever, because the only route
/// to a resume is a record in the picker. This does not re-implement that ranking -- it borrows its
/// result. A session with no record is not offerable, so its history has no reader.
///
/// Best-effort and silent throughout: this is housekeeping running inside a write whose failure
/// must not cost the caller its history. **An empty record list means "do nothing"**, because
/// `recorded_session_ids` cannot distinguish "this conversation has no records" from "the
/// conversation directory could not be read", and the second reading would delete every history
/// file this conversation has.
fn remove_orphans(conversation_id: &str) {
    let Ok(dir) = conversation_history_dir(conversation_id) else {
        return;
    };
    let recorded = crate::persistence::recorded_session_ids(conversation_id);
    if recorded.is_empty() {
        return;
    }
    let Ok(entries) = std::fs::read_dir(&dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        // `.json` only. `write_record`'s temp files are `.tmp`, and deleting one would be deleting
        // a write in flight.
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Some(session_id) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if !recorded.iter().any(|id| id == session_id) {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// Deletes one session's stored history, if it has one. Used by tests; a product caller would be
/// the deletion of a conversation, which nothing does yet.
#[cfg(test)]
fn remove(conversation_id: &str, provider_session_id: &str) -> std::io::Result<()> {
    let path = history_path(conversation_id, provider_session_id)?;
    match std::fs::remove_file(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::projection::ToolCallResult;

    fn setup() -> String {
        crate::state_dirs::redirect_state_to_a_test_root();
        // A fresh conversation id per test: these all write into one process-wide root.
        uuid::Uuid::new_v4().simple().to_string()
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

    fn tool_call(seq: u64, id: &str) -> ToolCallRecord {
        ToolCallRecord {
            seq,
            turn_id: "t1".into(),
            tool_use_id: id.into(),
            name: "Read".into(),
            input: serde_json::json!({"file_path": "/p/a.rs"}),
            result: None,
            denied: None,
            changed_input: None,
        }
    }

    fn history(conversation_id: &str, session_id: &str) -> StoredHistory {
        StoredHistory::from_collections(
            conversation_id.to_string(),
            session_id.to_string(),
            vec![prompt(1, "what does this do?")],
            vec![message(2, "it reads a file")],
            vec![tool_call(3, "toolu_1")],
        )
    }

    /// Stored prompts, replies, and tool calls use a `0600` file in `0700` directories so other
    /// users cannot read them, regardless of the umask.
    #[test]
    fn a_saved_history_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &std::path::Path| std::fs::symlink_metadata(p).unwrap().permissions().mode() & 0o777;
        let conv = setup();
        save(&history(&conv, "prov-1")).unwrap();
        let path = history_path(&conv, "prov-1").unwrap();
        assert_eq!(mode(&path), 0o600);
        let history_dir = crate::state_dirs::history_dir().unwrap();
        for dir in path.ancestors().skip(1).take_while(|d| d.starts_with(&history_dir)) {
            assert_eq!(mode(dir), 0o700, "{}", dir.display());
        }
        assert_eq!(mode(history_dir.parent().unwrap()), 0o700, "the state root");
    }

    /// Writes a record so the session is one `remove_orphans` must keep.
    fn record_for(conversation_id: &str, session_id: &str) {
        crate::persistence::save_conversation_record(&crate::persistence::ConversationRecord {
            conversation_id: conversation_id.to_string(),
            provider: "claude".into(),
            provider_session_id: session_id.to_string(),
            canonical_cwd: "/tmp/ws".into(),
            created_at: "1".into(),
            updated_at: "1".into(),
            provider_advertised_resume: true,
            title: None,
            name: None,
        })
        .unwrap();
    }

    #[test]
    fn a_history_round_trips_through_the_disk() {
        let conversation_id = setup();
        let written = history(&conversation_id, "sess-a");
        save(&written).unwrap();
        assert_eq!(load(&conversation_id, "sess-a").unwrap(), written);
    }

    /// Invariant 13: an unrecognised version is refused WHOLE -- not parsed as far as it goes, not
    /// reported as corruption. The document below is otherwise perfectly well-formed for this
    /// build, so nothing but the version can be what refuses it.
    #[test]
    fn a_version_this_build_does_not_know_is_a_clean_refusal_not_a_parse_error() {
        let conversation_id = setup();
        let mut written = history(&conversation_id, "sess-future");
        written.version = HISTORY_FORMAT_VERSION + 7;
        let path = history_path(&conversation_id, "sess-future").unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, serde_json::to_string(&written).unwrap()).unwrap();

        match load(&conversation_id, "sess-future") {
            Err(HistoryLoadError::UnknownVersion { found, expected }) => {
                assert_eq!(found, HISTORY_FORMAT_VERSION + 7);
                assert_eq!(expected, HISTORY_FORMAT_VERSION);
            }
            other => panic!("expected a version refusal, got {other:?}"),
        }
    }

    /// The other half of the gate: a document with no `version` key is refused the same way. It is
    /// specifically NOT allowed through as "the format before versions existed" -- there is no such
    /// format, and guessing would be the partial parse invariant 13 forbids.
    #[test]
    fn a_document_without_a_version_is_refused_as_an_unknown_version() {
        let text = r#"{"conversation_id":"c","provider_session_id":"s","written_at":"1",
                       "user_prompts":[],"transcript":[],"tool_calls":[]}"#;
        match load_str(text) {
            Err(HistoryLoadError::UnknownVersion { found, .. }) => assert_eq!(found, 0),
            other => panic!("expected a version refusal, got {other:?}"),
        }
    }

    /// A version this build DOES know, with a body it cannot read, is corruption and says so. This
    /// is the negative control for the two tests above: without it, "refuses" would not distinguish
    /// the version gate from any other failure to parse.
    #[test]
    fn a_known_version_with_an_unreadable_body_is_malformed_not_a_version_refusal() {
        let text = format!(r#"{{"version":{HISTORY_FORMAT_VERSION},"conversation_id":42}}"#);
        assert!(matches!(load_str(&text), Err(HistoryLoadError::Malformed(_))));
    }

    /// Files written before a collection existed still load because collections use `#[serde(default)]`.
    /// Without defaults, adding a collection would make older files unreadable and leave resumed
    /// conversations without their stored history.
    #[test]
    fn a_file_written_before_a_collection_existed_still_loads() {
        let text = format!(
            r#"{{"version":{HISTORY_FORMAT_VERSION},"conversation_id":"c",
                 "provider_session_id":"s","written_at":"1","user_prompts":[{{"seq":1,"text":"hi"}}]}}"#
        );
        let loaded = load_str(&text).unwrap();
        assert_eq!(loaded.user_prompts.len(), 1);
        assert!(loaded.transcript.is_empty() && loaded.tool_calls.is_empty());
    }

    /// The atomicity property `write_record` buys, stated as what a reader can observe: a second
    /// write over an existing file leaves no window in which the path holds anything but a complete
    /// document, and leaves no temp file behind.
    #[test]
    fn a_second_write_replaces_the_file_atomically_and_leaves_no_temp_file() {
        let conversation_id = setup();
        save(&history(&conversation_id, "sess-b")).unwrap();
        let mut second = history(&conversation_id, "sess-b");
        second.transcript.push(message(9, "and then some more"));
        save(&second).unwrap();

        assert_eq!(load(&conversation_id, "sess-b").unwrap(), second);
        let dir = conversation_history_dir(&conversation_id).unwrap();
        let strays: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| !name.ends_with(".json"))
            .collect();
        assert!(strays.is_empty(), "a temp file survived the write: {strays:?}");
    }

    /// The write path keeps the newest items within the cap. Stored history spans resumes, so
    /// uncapped writes would let the file grow indefinitely.
    ///
    /// Assertions use the constants so changing a cap does not invalidate this test.
    #[test]
    fn the_write_path_keeps_the_newest_items_up_to_the_item_cap() {
        let conversation_id = setup();
        let total = crate::history::HISTORY_MAX_ITEMS * 2;
        let messages: Vec<TranscriptMessage> = (0..total as u64).map(|i| message(i, &format!("m{i}"))).collect();
        let stored =
            StoredHistory::from_collections(conversation_id, "sess-c".into(), Vec::new(), messages, Vec::new());

        assert_eq!(stored.item_count(), crate::history::HISTORY_MAX_ITEMS);
        let first_kept = total - crate::history::HISTORY_MAX_ITEMS;
        assert_eq!(stored.transcript.first().unwrap().text, format!("m{first_kept}"));
        assert_eq!(stored.transcript.last().unwrap().text, format!("m{}", total - 1));
    }

    /// Truncation weighs the three collections together, by `seq`: what survives is the newest
    /// items of the CONVERSATION, not the newest of each collection independently. Interleaving
    /// them and capping at two items is what makes the difference observable -- a per-collection
    /// cut would keep the newest prompt, and there must be none.
    #[test]
    fn truncation_ranks_the_three_collections_against_each_other_by_seq() {
        let prompts = vec![prompt(5, "older still"), prompt(10, "oldest kept prompt")];
        let messages = vec![message(30, "newest")];
        let calls = vec![tool_call(20, "toolu_mid")];

        let (kept_prompts, kept_messages, kept_calls) = truncate_to(prompts, messages, calls, 2, usize::MAX);

        assert!(kept_prompts.is_empty(), "the two oldest items are prompts and must go");
        assert_eq!(kept_messages.len(), 1);
        assert_eq!(kept_calls.len(), 1);
    }

    /// The character cap binds independently of the item cap, and the same way: newest back, whole
    /// items, stop at the first that does not fit.
    #[test]
    fn the_character_cap_cuts_even_when_the_item_cap_is_not_reached() {
        let messages: Vec<TranscriptMessage> = (0..10u64).map(|i| message(i, &"z".repeat(10))).collect();

        let (_, kept, _) = truncate_to(Vec::new(), messages, Vec::new(), usize::MAX, 35);

        assert_eq!(kept.len(), 3, "three ten-character messages fit in 35 characters");
        assert_eq!(kept.first().unwrap().seq, 7, "and they must be the NEWEST three");
        assert_eq!(kept.last().unwrap().seq, 9);
    }

    /// Keep at least one item even when it exceeds the character budget. Reporting history with
    /// zero visible rows would be more confusing than showing one oversized row.
    #[test]
    fn a_single_item_over_the_character_budget_is_still_kept() {
        let huge = "x".repeat(crate::history::HISTORY_MAX_CHARS + 1);
        let stored = StoredHistory::from_collections(
            "c".into(),
            "s".into(),
            Vec::new(),
            vec![message(1, "small and old"), message(2, &huge)],
            Vec::new(),
        );
        assert_eq!(stored.item_count(), 1);
        assert_eq!(stored.transcript[0].text.len(), huge.len());
    }

    /// A tool call is weighed with its result, the same way the reader weighs it: they render as
    /// one row, so budgeting the call alone would let a 400 KB result through unaccounted.
    #[test]
    fn a_tool_calls_result_counts_against_the_character_budget() {
        let mut call = tool_call(1, "toolu_big");
        let bare = tool_call_chars(&call);
        call.result = Some(ToolCallResult {
            content: serde_json::Value::String("y".repeat(1000)),
            is_error: false,
        });
        assert_eq!(tool_call_chars(&call), bare + 1000);
    }

    /// History whose session entry has been pruned cannot be offered for resume. The first write
    /// for a new session sweeps these unreachable history files.
    #[test]
    fn the_first_write_for_a_session_deletes_histories_whose_record_is_gone() {
        let conversation_id = setup();
        record_for(&conversation_id, "sess-live");
        record_for(&conversation_id, "sess-doomed");
        save(&history(&conversation_id, "sess-live")).unwrap();
        save(&history(&conversation_id, "sess-doomed")).unwrap();
        // The record goes, as `prune` would take it once this conversation passes its cap.
        std::fs::remove_file(
            crate::persistence::conversations_dir()
                .unwrap()
                .join(&conversation_id)
                .join("sess-doomed.json"),
        )
        .unwrap();

        record_for(&conversation_id, "sess-new");
        save(&history(&conversation_id, "sess-new")).unwrap();

        assert!(
            load(&conversation_id, "sess-live").is_ok(),
            "a recorded session's history must survive"
        );
        assert!(
            !history_path(&conversation_id, "sess-doomed").unwrap().exists(),
            "an orphaned history must be swept"
        );
        assert!(load(&conversation_id, "sess-new").is_ok());
    }

    /// The other half of the sweep, and the one that matters more: a LATER write for a session that
    /// already has a file does not sweep, so a history file created between two turns of another
    /// window is not deleted on a whim -- and, more to the point, the sweep's cost is not paid on
    /// every turn boundary.
    #[test]
    fn a_later_write_for_the_same_session_does_not_sweep() {
        let conversation_id = setup();
        record_for(&conversation_id, "sess-keeper");
        save(&history(&conversation_id, "sess-keeper")).unwrap();
        // An orphan with no record at all, created after the first write.
        save(&history(&conversation_id, "sess-orphan")).unwrap();
        remove(&conversation_id, "sess-orphan").unwrap();
        let orphan = history_path(&conversation_id, "sess-orphan").unwrap();
        std::fs::write(
            &orphan,
            serde_json::to_string(&history(&conversation_id, "sess-orphan")).unwrap(),
        )
        .unwrap();

        save(&history(&conversation_id, "sess-keeper")).unwrap();

        assert!(orphan.exists(), "a write that creates no file must not sweep");
    }

    /// The ambiguity guard in `remove_orphans`: a conversation whose record directory cannot be
    /// read at all reports zero recorded sessions, and treating that as "everything is an orphan"
    /// would delete every history this conversation has.
    #[test]
    fn a_conversation_with_no_readable_records_loses_no_history() {
        let conversation_id = setup();
        save(&history(&conversation_id, "sess-x")).unwrap();
        // No record was ever written for this conversation, so `recorded_session_ids` is empty.
        assert!(crate::persistence::recorded_session_ids(&conversation_id).is_empty());
        save(&history(&conversation_id, "sess-y")).unwrap();
        assert!(
            load(&conversation_id, "sess-x").is_ok(),
            "an ambiguous sweep must delete nothing"
        );
    }

    /// Path components are validated, so neither id can walk out of the history directory.
    #[test]
    fn a_path_shaped_id_is_refused_rather_than_escaping_the_history_directory() {
        assert!(history_path("../../etc", "s").is_err());
        assert!(history_path("c", "../../etc/passwd").is_err());
        assert!(history_path("", "s").is_err());
    }

    /// A real conversation id and a real Claude session UUID both pass, which is what makes the
    /// validator a guard rather than an obstacle.
    #[test]
    fn a_real_conversation_id_and_session_uuid_are_valid_path_components() {
        let path = history_path(
            &uuid::Uuid::new_v4().simple().to_string(),
            "7d4e9b2a-1c3f-4a5b-8e6d-0f1a2b3c4d5e",
        )
        .unwrap();
        assert!(path.extension().unwrap() == "json");
    }

    /// The survivors copied from borrowed collections are exactly what truncating copies of them
    /// keeps, for collections in `seq` order (with ties), out of order, and every cap from none to
    /// all: a small generator, many seeds.
    #[test]
    fn copying_only_the_survivors_keeps_what_truncating_keeps() {
        let mut state = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = move |below: u64| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state % below
        };
        for round in 0..400 {
            let sorted = round % 5 != 0;
            let mut seq = 0u64;
            let mut seqs = |next: &mut dyn FnMut(u64) -> u64, n: u64| -> Vec<u64> {
                let mut out: Vec<u64> = (0..n)
                    .map(|_| {
                        seq += next(3);
                        seq
                    })
                    .collect();
                if !sorted {
                    out.reverse();
                }
                out
            };
            let n = next(6);
            let prompts: Vec<_> = seqs(&mut next, n)
                .into_iter()
                .map(|q| prompt(q, &"p".repeat(next(30) as usize)))
                .collect();
            let n = next(8);
            let messages: Vec<_> = seqs(&mut next, n)
                .into_iter()
                .map(|q| message(q, &"m".repeat(next(60) as usize)))
                .collect();
            let n = next(8);
            let calls: Vec<_> = seqs(&mut next, n)
                .into_iter()
                .map(|q| tool_call(q, &format!("t{q}")))
                .collect();
            let max_items = next(25) as usize;
            let max_chars = next(400) as usize;
            let expected = truncate_to(prompts.clone(), messages.clone(), calls.clone(), max_items, max_chars);
            let got = newest_that_fit_to(&prompts, &messages, &calls, max_items, max_chars);
            assert_eq!(got, expected, "round {round}");
        }
    }
}
