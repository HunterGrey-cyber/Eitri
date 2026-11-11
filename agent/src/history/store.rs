// agent/src/history/store.rs
//! **A**: Eitri's own copy of a conversation's history, written beside its conversation records.
//!
//! This is the FALLBACK
//! under [`super::transcript_jsonl`], which reads the real Claude CLI's own transcript. B wins
//! whenever it can be read at all; A exists for the resume where it cannot -- no
//! `CLAUDE_CONFIG_DIR`, an unreadable file, or a file whose format this build no longer recognises.
//!
//! Three things about this file are decisions with reasons, not preferences:
//!
//! - **It is not in `conversations/`.** See `state_dirs::history_dir`.
//! - **It carries its own `version`, and an unrecognised one is refused whole.** That field is the
//!   only defined "I do not know this format" signal anywhere in this feature: the CLI's transcript
//!   has no schema version at all (its `version` is the CLI's own release, which promises nothing
//!   about format), so B can only observe a format change as "nothing parsed". A does not have to
//!   guess, and [`load`] refuses rather than parsing what it can and inventing the rest.
//! - **Only the record types are `Deserialize`, never `AgentSessionProjection`.** See that
//!   type's own doc comment for why.
//!
//! Adding a field to `StoredHistory` or to any of the record types needs `#[serde(default)]`,
//! or every file written before it stops loading. The precedent is `ConversationRecord`'s
//! `provider_advertised_resume` and `title`, and the test below is shaped after the one that pins
//! them (`persistence::tests::a_record_written_before_provider_advertised_resume_existed_still_loads`).

use crate::projection::{
    AgentSessionProjection, NoticeRecord, QuestionRecord, QuestionState, ToolCallRecord, TranscriptMessage,
    TurnStartRecord, UserPromptRecord,
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// The format this build writes and the only one it reads.
///
/// Bump it when a change cannot be expressed as an added `#[serde(default)]` field -- i.e. when an
/// old file would be MISREAD rather than merely read with a default. Every file at any other
/// version is refused whole by [`load`]; that is the point of having the number.
pub const HISTORY_FORMAT_VERSION: u32 = 1;

/// One conversation's history as Eitri recorded it.
///
/// Deliberately not a serialized projection. It holds the collections that ARE history (prompts,
/// replies, tool calls, the starts of turns the user did not send, notice rows and question sets) and
/// nothing that is live state: no `status`, no `active_turn_id`, no `session_id`/
/// `provider_session_id` inside a projection, no `model`, no `cwd`, no `usage`, and above all no
/// `pending_permissions` -- a permission card restored from a file would be a request nobody can
/// answer, since the provider that raised it is gone. A question stored while it waited is kept as
/// it stood, and is read back as settled with `SessionClosed` for the same reason.
///
/// It also carries no `next_seq`. Loading this back is a REFOLD: each item's `seq` is a sorting key
/// on disk and the projection's own `apply` assigns the numbers again. A stored counter would be a
/// second numbering authority, and `apply` must stay the only one.
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
    /// The starts of turns the user did not send, with why they started. An older file has none,
    /// and an older build ignores them.
    #[serde(default)]
    pub turn_starts: Vec<TurnStartRecord>,
    /// The CLI's notice rows.
    #[serde(default)]
    pub notices: Vec<NoticeRecord>,
    /// Claude's question sets, as they stood when written: a waiting one restores as closed.
    #[serde(default)]
    pub questions: Vec<QuestionRecord>,
}

/// Why a stored history could not be loaded. Three outcomes, kept apart on purpose.
#[derive(Debug)]
pub enum HistoryLoadError {
    /// No such file, no permission, a read that failed. `NotFound` is the ordinary case for a
    /// session that has never reached a turn boundary and is not an error worth reporting anywhere.
    Io(std::io::Error),
    /// The file is not JSON, or not JSON shaped like this version's `StoredHistory`.
    Malformed(serde_json::Error),
    /// The file declares a format this build does not know. A clean, defined
    /// refusal -- NOT a parse failure, and not a partial read: nothing is salvaged, no field is
    /// guessed at, and the caller is told the two numbers so a human can see which way the skew
    /// runs.
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
    /// Builds what should go on disk from the first three collections, TRUNCATED; the others empty.
    ///
    /// **This doc used to say:** "Truncation on the write path is not belt-and-braces: A
    /// accumulates ACROSS resumes. A resume loads history into the projection, the session then
    /// adds to that same projection, and the next write stores both -- which is the property that
    /// makes A survive a CLI transcript being rotated away, and is also what makes the file grow
    /// without bound if nothing cuts it here."
    ///
    /// **Both halves of that were wrong (found 2026-09-20),
    /// and the sentence is kept above rather than deleted so the next reader can see which claim
    /// was corrected.** `flush_history` (agent/src/ingestion.rs) writes the WHOLE projection; it
    /// does not append. After a resume the projection holds whatever was just restored -- when B
    /// (the CLI's own transcript) supplied it, that is B's view -- so one completed turn overwrites
    /// A with "what B restored, plus this session's new part", and anything A held beyond B is
    /// gone. The concrete bad case: a session whose transcript is past `HISTORY_SCAN_MAX_BYTES`
    /// resumes from B's last 32 MiB, runs one turn, and A now holds only that tail's worth.
    ///
    /// So **A is a snapshot of the last projection, not an accumulating archive**, and the file
    /// cannot grow without bound anyway: every write is a bounded projection. Making it really
    /// accumulate would need a write-time merge of A with the projection (by `seq`, deduplicated,
    /// then truncated), which carries its own correctness burden -- the same item can differ in
    /// text between the two records, which is how compaction works -- and nobody asked for it.
    /// Deliberately not done here.
    ///
    /// **Truncation on the write path stays, and its reason is the one below**, not the growth
    /// argument above. The caps and the direction are the reader's, deliberately:
    /// `HISTORY_MAX_ITEMS` items and `HISTORY_MAX_CHARS` characters, whole items only, taken from
    /// the NEWEST backwards, with the newest item always kept even when it alone is over budget. A
    /// file cut by one rule and read by another would drop a different set of items on each pass.
    pub fn from_collections(
        conversation_id: String,
        provider_session_id: String,
        user_prompts: Vec<UserPromptRecord>,
        transcript: Vec<TranscriptMessage>,
        tool_calls: Vec<ToolCallRecord>,
    ) -> Self {
        Self::from_parts(
            conversation_id,
            provider_session_id,
            HistoryParts {
                user_prompts,
                transcript,
                tool_calls,
                ..HistoryParts::default()
            },
        )
    }

    /// [`Self::from_collections`] with every collection a stored history keeps, truncated together
    /// by the same rule.
    pub fn from_parts(conversation_id: String, provider_session_id: String, parts: HistoryParts) -> Self {
        let HistoryParts {
            user_prompts,
            transcript,
            tool_calls,
            turn_starts,
            notices,
            questions,
        } = truncate(parts);
        Self {
            version: HISTORY_FORMAT_VERSION,
            conversation_id,
            provider_session_id,
            written_at: crate::conversation::epoch_millis(),
            user_prompts,
            transcript,
            tool_calls,
            turn_starts,
            notices,
            questions,
        }
    }

    /// The collections, moved out.
    pub fn into_parts(self) -> HistoryParts {
        HistoryParts {
            user_prompts: self.user_prompts,
            transcript: self.transcript,
            tool_calls: self.tool_calls,
            turn_starts: self.turn_starts,
            notices: self.notices,
            questions: self.questions,
        }
    }

    /// How many conversational items this holds -- the same things the reader counts as items (a
    /// tool result is not one; it belongs to the call it completes).
    pub fn item_count(&self) -> usize {
        self.user_prompts.len()
            + self.transcript.len()
            + self.tool_calls.len()
            + self.turn_starts.len()
            + self.notices.len()
            + self.questions.len()
    }

    /// How many characters this holds by the measure the caps cut by: every item's cost, summed.
    pub fn char_count(&self) -> usize {
        let view = HistoryView {
            user_prompts: &self.user_prompts,
            transcript: &self.transcript,
            tool_calls: &self.tool_calls,
            turn_starts: &self.turn_starts,
            notices: &self.notices,
            questions: &self.questions,
        };
        EVERY_WHICH
            .into_iter()
            .map(|which| (0..view.len(which)).map(|index| view.cost(which, index)).sum::<usize>())
            .sum()
    }
}

/// Every collection a stored history keeps, owned: what it is built from and what a load refolds.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HistoryParts {
    pub user_prompts: Vec<UserPromptRecord>,
    pub transcript: Vec<TranscriptMessage>,
    pub tool_calls: Vec<ToolCallRecord>,
    pub turn_starts: Vec<TurnStartRecord>,
    pub notices: Vec<NoticeRecord>,
    pub questions: Vec<QuestionRecord>,
}

impl HistoryParts {
    fn view(&self) -> HistoryView<'_> {
        HistoryView {
            user_prompts: &self.user_prompts,
            transcript: &self.transcript,
            tool_calls: &self.tool_calls,
            turn_starts: &self.turn_starts,
            notices: &self.notices,
            questions: &self.questions,
        }
    }
}

/// The same collections, borrowed: a projection's, for the copy taken under its lock.
#[derive(Debug, Clone, Copy)]
pub(crate) struct HistoryView<'a> {
    pub user_prompts: &'a [UserPromptRecord],
    pub transcript: &'a [TranscriptMessage],
    pub tool_calls: &'a [ToolCallRecord],
    pub turn_starts: &'a [TurnStartRecord],
    pub notices: &'a [NoticeRecord],
    pub questions: &'a [QuestionRecord],
}

impl<'a> HistoryView<'a> {
    pub(crate) fn of(projection: &'a AgentSessionProjection) -> Self {
        Self {
            user_prompts: &projection.user_prompts,
            transcript: &projection.transcript,
            tool_calls: &projection.tool_calls,
            turn_starts: &projection.turn_starts,
            notices: &projection.notices,
            questions: &projection.questions,
        }
    }

    fn len(&self, which: Which) -> usize {
        match which {
            Which::Prompt => self.user_prompts.len(),
            Which::Message => self.transcript.len(),
            Which::ToolCall => self.tool_calls.len(),
            Which::TurnStart => self.turn_starts.len(),
            Which::Notice => self.notices.len(),
            Which::Question => self.questions.len(),
        }
    }

    fn seq(&self, which: Which, index: usize) -> u64 {
        match which {
            Which::Prompt => self.user_prompts[index].seq,
            Which::Message => self.transcript[index].seq,
            Which::ToolCall => self.tool_calls[index].seq,
            Which::TurnStart => self.turn_starts[index].seq,
            Which::Notice => self.notices[index].seq,
            Which::Question => self.questions[index].seq,
        }
    }

    /// What one item costs against the character budget.
    fn cost(&self, which: Which, index: usize) -> usize {
        match which {
            Which::Prompt => self.user_prompts[index].text.chars().count(),
            Which::Message => self.transcript[index].text.chars().count(),
            Which::ToolCall => tool_call_chars(&self.tool_calls[index]),
            Which::TurnStart => turn_start_chars(&self.turn_starts[index]),
            Which::Notice => notice_chars(&self.notices[index]),
            Which::Question => question_chars(&self.questions[index]),
        }
    }

    /// Copies of each collection from its start index on.
    fn tails(&self, from: [usize; WHICH_COUNT]) -> HistoryParts {
        HistoryParts {
            user_prompts: self.user_prompts[from[Which::Prompt as usize]..].to_vec(),
            transcript: self.transcript[from[Which::Message as usize]..].to_vec(),
            tool_calls: self.tool_calls[from[Which::ToolCall as usize]..].to_vec(),
            turn_starts: self.turn_starts[from[Which::TurnStart as usize]..].to_vec(),
            notices: self.notices[from[Which::Notice as usize]..].to_vec(),
            questions: self.questions[from[Which::Question as usize]..].to_vec(),
        }
    }
}

/// Which collection an item came from, so truncation can put the survivors back where they belong.
/// The order is the tie-break between items that share a `seq`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Which {
    Prompt,
    Message,
    ToolCall,
    TurnStart,
    Notice,
    Question,
}

const WHICH_COUNT: usize = 6;
const EVERY_WHICH: [Which; WHICH_COUNT] = [
    Which::Prompt,
    Which::Message,
    Which::ToolCall,
    Which::TurnStart,
    Which::Notice,
    Which::Question,
];

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
fn truncate(parts: HistoryParts) -> HistoryParts {
    truncate_to(parts, super::HISTORY_MAX_ITEMS, super::HISTORY_MAX_CHARS)
}

/// [`truncate`] with the two caps as parameters.
///
/// Parameterised for ONE reason: a test that had to build `HISTORY_MAX_ITEMS * 2` real items to
/// observe the item cap, and a `HISTORY_MAX_CHARS`-sized string to observe the character one, would
/// be slow enough that nobody writes the interesting cases. The product path never passes anything
/// but the constants -- [`truncate`] is the only non-test caller.
fn truncate_to(mut parts: HistoryParts, max_items: usize, max_chars: usize) -> HistoryParts {
    let view = parts.view();
    let mut items: Vec<(u64, Which, usize, usize)> = Vec::new();
    for which in EVERY_WHICH {
        for index in 0..view.len(which) {
            items.push((view.seq(which, index), which, index, view.cost(which, index)));
        }
    }
    // Newest first. The tie-break on `Which` and the index keeps the order total, so two items that
    // somehow share a seq are cut deterministically rather than by sort luck.
    items.sort_by_key(|(seq, which, index, _)| std::cmp::Reverse((*seq, *which as u8, *index)));

    let mut keep: [Vec<bool>; WHICH_COUNT] = EVERY_WHICH.map(|which| vec![false; view.len(which)]);
    let mut chars = 0usize;
    for (kept, (_, which, index, cost)) in items.into_iter().enumerate() {
        if kept >= max_items {
            break;
        }
        // At least one item always survives, however large it is: "there is history here" followed
        // by zero rows is harder to understand than one oversized row.
        if kept > 0 && chars + cost > max_chars {
            break;
        }
        keep[which as usize][index] = true;
        chars += cost;
    }

    retain_by(&mut parts.user_prompts, &keep[Which::Prompt as usize]);
    retain_by(&mut parts.transcript, &keep[Which::Message as usize]);
    retain_by(&mut parts.tool_calls, &keep[Which::ToolCall as usize]);
    retain_by(&mut parts.turn_starts, &keep[Which::TurnStart as usize]);
    retain_by(&mut parts.notices, &keep[Which::Notice as usize]);
    retain_by(&mut parts.questions, &keep[Which::Question as usize]);
    parts
}

/// What [`truncate`] keeps of copies of the collections, copying only the survivors: this runs
/// under the lock the UI's reads of the conversation take, and a long session's collections hold
/// far more than the caps keep. Collections in `seq` order (as the projection keeps them) are walked
/// newest first from their ends and stop at the caps, so the work is what survives; anything else
/// is copied whole and truncated as before.
pub(crate) fn newest_that_fit(view: HistoryView<'_>) -> HistoryParts {
    newest_that_fit_to(view, super::HISTORY_MAX_ITEMS, super::HISTORY_MAX_CHARS)
}

fn newest_that_fit_to(view: HistoryView<'_>, max_items: usize, max_chars: usize) -> HistoryParts {
    let in_order = |which: Which| (1..view.len(which)).all(|i| view.seq(which, i - 1) <= view.seq(which, i));
    if !EVERY_WHICH.into_iter().all(in_order) {
        let whole = view.tails([0; WHICH_COUNT]);
        return truncate_to(whole, max_items, max_chars);
    }
    // Each collection's survivors are a run at its end (the walk stops, never skips), so a cursor per
    // collection says where its run starts. The next item is the newest of the ends, by the same key
    // `truncate_to` sorts on.
    let mut cursors: [usize; WHICH_COUNT] = EVERY_WHICH.map(|which| view.len(which));
    let mut kept = 0usize;
    let mut chars = 0usize;
    while kept < max_items {
        let newest = EVERY_WHICH
            .into_iter()
            .filter_map(|which| {
                let index = cursors[which as usize].checked_sub(1)?;
                Some(((view.seq(which, index), which as u8, index), which, index))
            })
            .max_by_key(|(key, _, _)| *key);
        let Some((_, which, index)) = newest else {
            break;
        };
        let cost = view.cost(which, index);
        if kept > 0 && chars + cost > max_chars {
            break;
        }
        cursors[which as usize] = index;
        kept += 1;
        chars += cost;
    }
    view.tails(cursors)
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
pub(super) fn tool_call_chars(call: &ToolCallRecord) -> usize {
    let result = call
        .result
        .as_ref()
        .map(|r| super::transcript_jsonl::value_chars(&r.content))
        .unwrap_or(0);
    let children = call
        .children
        .as_ref()
        .map(|stream| {
            stream
                .transcript
                .iter()
                .map(|row| row.text.chars().count())
                .sum::<usize>()
                + stream
                    .thinking
                    .iter()
                    .map(|row| row.text.chars().count())
                    .sum::<usize>()
                + stream.tool_calls.iter().map(tool_call_chars).sum::<usize>()
        })
        .unwrap_or(0);
    call.name.chars().count() + super::transcript_jsonl::value_chars(&call.input) + result + children
}

/// A turn start costs the text of its cause: what its row can show.
pub(super) fn turn_start_chars(start: &TurnStartRecord) -> usize {
    start
        .cause
        .as_ref()
        .map(|cause| cause.texts().map(|text| text.chars().count()).sum())
        .unwrap_or(0)
}

/// A notice costs its words and the values it may be worded from.
pub(super) fn notice_chars(notice: &NoticeRecord) -> usize {
    let content = notice.content.as_deref().map(|c| c.chars().count()).unwrap_or(0);
    content
        + notice
            .scalars
            .values()
            .map(super::transcript_jsonl::value_chars)
            .sum::<usize>()
}

/// A question costs every text it holds: the questions, their choices and previews, and the
/// answers and reasons it settled with.
pub(super) fn question_chars(question: &QuestionRecord) -> usize {
    let count = |text: &str| text.chars().count();
    let opt = |text: &Option<String>| text.as_deref().map(count).unwrap_or(0);
    let asked: usize = question
        .questions
        .iter()
        .map(|q| {
            count(&q.question)
                + count(&q.header)
                + q.options
                    .iter()
                    .map(|o| count(&o.label) + count(&o.description) + opt(&o.preview))
                    .sum::<usize>()
        })
        .sum();
    let settled = match &question.state {
        QuestionState::Settled { answers, reason, .. } => {
            answers.iter().map(|a| count(&a.answer) + opt(&a.notes)).sum::<usize>() + opt(reason)
        }
        QuestionState::Waiting | QuestionState::Answering => 0,
    };
    asked + settled + opt(&question.unsupported_reason)
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
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        // Temporary writes and unrelated files are never housekeeping candidates.
        let session_id = match super::archive::archive_name(name) {
            Some((sid, _, _)) => sid,
            None => match name.strip_suffix(".json") {
                Some(sid) => sid,
                None => continue,
            },
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
            delivered: false,
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
            children: None,
            background: false,
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

    fn three(
        user_prompts: Vec<UserPromptRecord>,
        transcript: Vec<TranscriptMessage>,
        tool_calls: Vec<ToolCallRecord>,
    ) -> HistoryParts {
        HistoryParts {
            user_prompts,
            transcript,
            tool_calls,
            ..HistoryParts::default()
        }
    }

    fn turn_start(seq: u64, summary: &str) -> TurnStartRecord {
        let mut cause = crate::projection::TurnCause::of_kind(crate::projection::TurnCauseKind::Task);
        cause.summary = Some(summary.to_string());
        TurnStartRecord {
            seq,
            turn_id: format!("cli-{seq}"),
            origin: crate::projection::TurnOrigin::Cli,
            cause: Some(cause),
        }
    }

    fn notice(seq: u64, content: &str) -> NoticeRecord {
        NoticeRecord {
            seq,
            kind: crate::projection::SessionNoticeKind::Informational,
            level: crate::projection::NoticeLevel::Notice,
            content: Some(content.to_string()),
            scalars: Default::default(),
            task_id: None,
            tool_use_id: None,
            task_status: None,
        }
    }

    fn question(seq: u64, text: &str) -> QuestionRecord {
        QuestionRecord {
            seq,
            interaction_id: format!("q-{seq}"),
            tool_use_id: None,
            turn_id: None,
            questions: vec![crate::projection::QuestionSpec {
                question: text.to_string(),
                header: String::new(),
                multi_select: false,
                options: vec![],
            }],
            state: QuestionState::Waiting,
            unsupported_reason: None,
        }
    }

    /// The three collections added after the first three are bounded with them, by the same rule:
    /// the newest items of the whole conversation within both caps, a turn start, a notice and a
    /// question weighed by their text like any other item.
    #[test]
    fn newest_that_fit_bounds_the_new_collections() {
        let parts = HistoryParts {
            user_prompts: vec![prompt(1, "oldest")],
            transcript: vec![message(2, "reply")],
            tool_calls: vec![],
            turn_starts: vec![turn_start(3, "a task ended"), turn_start(7, "tick")],
            notices: vec![notice(4, "compacted"), notice(8, "fell back")],
            questions: vec![question(5, "which one?"), question(9, "and now?")],
        };
        // Item cap: the newest three of the conversation, whichever collections they are in.
        let kept = newest_that_fit_to(parts.view(), 3, usize::MAX);
        assert_eq!(kept.turn_starts.iter().map(|r| r.seq).collect::<Vec<_>>(), vec![7]);
        assert_eq!(kept.notices.iter().map(|r| r.seq).collect::<Vec<_>>(), vec![8]);
        assert_eq!(kept.questions.iter().map(|r| r.seq).collect::<Vec<_>>(), vec![9]);
        assert!(kept.user_prompts.is_empty() && kept.transcript.is_empty());
        assert_eq!(kept, truncate_to(parts.clone(), 3, usize::MAX));

        // Character cap: "and now?" (8) + "fell back" (9) fit in 20; "tick" (4) would pass it.
        let kept = newest_that_fit_to(parts.view(), usize::MAX, 20);
        assert!(kept.turn_starts.is_empty());
        assert_eq!(kept.notices.len(), 1);
        assert_eq!(kept.questions.len(), 1);
        assert_eq!(kept, truncate_to(parts.clone(), usize::MAX, 20));

        // And what is stored is what the history keeps: the write path bounds them too.
        let stored = StoredHistory::from_parts("c".into(), "s".into(), parts.clone());
        assert_eq!(stored.item_count(), 8);
        assert_eq!(stored.into_parts(), parts);
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

    /// Stored history contains prompts, replies and tool calls, so its files must be 0600
    /// in 0700 directories even when the umask would otherwise produce 0644 files in 0755 directories.
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
    fn orphan_cleanup_uses_the_archives_session_id_and_preserves_an_empty_record_list() {
        let conversation_id = setup();
        let dir = conversation_history_dir(&conversation_id).unwrap();
        crate::persistence::create_private_state_dir(&dir, crate::state_dirs::history_dir().unwrap()).unwrap();
        let kept = dir.join("recorded.evicted-0123456789abcdef.0.jsonl");
        let gone = dir.join("orphan.evicted-0123456789abcdef.0.jsonl");
        let unrelated = dir.join("orphan.evicted-not-an-archive.jsonl");
        for path in [&kept, &gone, &unrelated] {
            crate::private_fs::write_private(path, b"{}\n").unwrap();
        }
        remove_orphans(&conversation_id);
        assert!(gone.exists(), "no records means no authority to sweep");
        record_for(&conversation_id, "recorded");
        remove_orphans(&conversation_id);
        assert!(kept.exists());
        assert!(!gone.exists());
        assert!(unrelated.exists());
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
        crate::test_tmp::write_private(&path, serde_json::to_string(&written).unwrap()).unwrap();

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

        let kept = truncate_to(three(prompts, messages, calls), 2, usize::MAX);

        assert!(
            kept.user_prompts.is_empty(),
            "the two oldest items are prompts and must go"
        );
        assert_eq!(kept.transcript.len(), 1);
        assert_eq!(kept.tool_calls.len(), 1);
    }

    /// The character cap binds independently of the item cap, and the same way: newest back, whole
    /// items, stop at the first that does not fit.
    #[test]
    fn the_character_cap_cuts_even_when_the_item_cap_is_not_reached() {
        let messages: Vec<TranscriptMessage> = (0..10u64).map(|i| message(i, &"z".repeat(10))).collect();

        let kept = truncate_to(three(Vec::new(), messages, Vec::new()), usize::MAX, 35).transcript;

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
        crate::test_tmp::write_private(
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
            let n = next(4);
            let starts: Vec<_> = seqs(&mut next, n)
                .into_iter()
                .map(|q| turn_start(q, &"c".repeat(next(40) as usize)))
                .collect();
            let n = next(4);
            let notices: Vec<_> = seqs(&mut next, n)
                .into_iter()
                .map(|q| notice(q, &"n".repeat(next(40) as usize)))
                .collect();
            let n = next(4);
            let questions: Vec<_> = seqs(&mut next, n)
                .into_iter()
                .map(|q| question(q, &"q".repeat(next(40) as usize)))
                .collect();
            let parts = HistoryParts {
                user_prompts: prompts,
                transcript: messages,
                tool_calls: calls,
                turn_starts: starts,
                notices,
                questions,
            };
            let max_items = next(25) as usize;
            let max_chars = next(400) as usize;
            let expected = truncate_to(parts.clone(), max_items, max_chars);
            let got = newest_that_fit_to(parts.view(), max_items, max_chars);
            assert_eq!(got, expected, "round {round}");
        }
    }
}
