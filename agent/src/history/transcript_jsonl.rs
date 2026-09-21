//! Turning the real Claude CLI's own `<uuid>.jsonl` transcript into ordered `AgentDomainEvent`s.
//!
//! Design: `docs/superpowers/specs/2026-09-20-resume-history-design.md` §3 (what each line becomes)
//! and §5 (how much is read, and how it is truncated). Every number quoted in this file was
//! measured on 2026-09-20 over the 822 real `.jsonl` files (570 MB, 117,826 lines) under this
//! machine's own `$CLAUDE_CONFIG_DIR/projects`.
//!
//! Three properties this module exists to hold, none of which is convenience:
//!
//! 1. **Nothing here ever fails the load.** An unknown line type, an unknown content block, a
//!    malformed line and a half-written final line are each SKIPPED AND COUNTED. The only `Err`
//!    this module can return is a real I/O error opening or reading the file. A format change must
//!    degrade to "less history was shown", never to a failed resume.
//! 2. **`type:"user"` does not mean "the user typed this".** The decision is an ALLOW-list
//!    ([`user_line_is_the_users_own_words`]), never a deny-list, because a deny-list's failure
//!    direction is rendering someone else's text under the user's name -- and the corpus contains
//!    exactly such a line: a compaction summary is a `type:"user"` line whose `message.content` is
//!    a plain string, indistinguishable from a typed prompt by shape alone.
//! 3. **Memory is bounded by the constants below, not by the file.** The largest real transcript on
//!    this machine is 47,254,948 bytes.
//!
//! What this module does NOT do: fold anything into a projection, write anything anywhere, or read
//! anything under `$CLAUDE_CONFIG_DIR` other than the one file it is handed. That directory belongs
//! to the CLI and is read-only, forever.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::Path;

use serde_json::Value;

use super::composed_block::strip_composed_block;
use crate::projection::{AgentDomainEvent, ContentKind};

/// The most conversational items a restored history may carry.
///
/// 400 at the owner's ruling, 2026-09-20. Measured over this machine's 44 real session files by
/// replaying the exact take-from-the-newest rule below: at 200, 14 of 44 sessions (32%) were
/// truncated; at 400, 8 of 44 (18%). The cost of the larger number is the item ceiling and each
/// item's fixed overhead, not text volume -- the most text any session loads is identical under
/// both, because [`HISTORY_MAX_CHARS`] is what caps it.
///
/// **This number must not appear in an assertion.** Tests pin properties (newest-first, bounded,
/// announced); changing this constant is not supposed to turn anything red.
pub const HISTORY_MAX_ITEMS: usize = 400;

/// The most characters a restored history may carry, counting each tool call's serialized `input`
/// and `result`.
///
/// Not decoration: at 400 items, 5 of this machine's 44 sessions are governed by this limit rather
/// than by the item count (3 hit it outright, 2 more land within 5% of it). It is what stands
/// between the panel and a single tool result of 483,545 bytes multiplied by the item ceiling.
pub const HISTORY_MAX_CHARS: usize = 1_000_000;

/// How far back from the end of the file the scan may reach.
///
/// One of this machine's 44 sessions exceeds it (47,254,948 bytes) -- about 2%, which is the right
/// trigger rate for a fallback: neither dead code nor the common case. Lowering it to 4 MiB would
/// put 6 of 44 onto the "count unknown" notice. It is not a time budget: reading the largest file
/// whole takes 0.143 s in Python. It is a memory and worst-case bound. A deliberate non-choice is
/// a *timeout*: a read that stops after N seconds, in a forward scan, stops holding the OLDEST
/// records, which is the wrong end.
pub const HISTORY_SCAN_MAX_BYTES: u64 = 32 * 1024 * 1024;

/// A single line longer than this is skipped without ever being held in memory.
///
/// The longest real line in the whole 117,826-line corpus is 967,870 bytes, 23% of this. **It has
/// never triggered, and that is not a reason to delete it**: line length is the one quantity a
/// streaming parser cannot know in advance, so this is the only hard guarantee that one line cannot
/// exhaust memory. Its value is entirely in inputs nobody has seen.
pub const HISTORY_MAX_LINE_BYTES: usize = 4 * 1024 * 1024;

/// `promptSource` values that mean a person put these words there.
///
/// Measured: `typed` + `suggestion_accepted` + `queued` is 442 lines, exactly matching the 442
/// lines carrying the independent field `origin.kind == "human"` -- two fields corroborating each
/// other rather than one field asserting itself. **`sdk` is not optional**: every prompt in the six
/// sessions Neovibe itself drove carries `promptSource: "sdk"` and no `origin` field at all, so an
/// allow-list of "human" alone would return nothing in the only situation this feature exists for.
///
/// What it misses, knowingly: a bare `/compact` and other slash commands, which carry no
/// `promptSource` (2 lines here). A slash command is a command, not a message.
const HUMAN_PROMPT_SOURCES: [&str; 4] = ["typed", "suggestion_accepted", "queued", "sdk"];

/// Everything read out of one transcript, ready for a caller to fold into a projection.
///
/// It deliberately carries no `seq`, no source path and no projection: assigning order is
/// `AgentSessionProjection::apply`'s job alone, and this type must not become a second place where
/// history acquires an ordering.
#[derive(Debug, Clone)]
pub struct TranscriptHistory {
    /// In file order, ready to be handed to `apply` one at a time.
    ///
    /// Order is the order lines appear in the file, NOT a walk of the `parentUuid` chain. That
    /// chain is not a chain: 20 of this machine's 44 session files contain 127 positions where one
    /// `parentUuid` is referenced by several lines, and which child is "the main line" is not
    /// expressed on disk. Reconstructing it would mean guessing at those 127 positions, and a wrong
    /// guess silently drops a whole stretch of conversation. Flattening is uglier and loses nothing.
    pub events: Vec<AgentDomainEvent>,
    /// Conversational items this parse kept -- prompts, assistant text blocks and tool calls. A
    /// tool result is not an item; it belongs to the call it completes.
    ///
    /// **This is a PRE-FOLD count and must never be shown to a user.** It counts the slots this
    /// reader kept, and `AgentSessionProjection::apply` then coalesces consecutive assistant text
    /// into a single `TranscriptMessage` -- so N adjacent assistant slots become one row on screen.
    /// Measured (2026-09-20, whole-branch review, all 45 real top-level transcripts on this
    /// machine): 13 of 45 disagree with what the projection ends up holding, worst case 400 slots
    /// against 384 rows. The number the notice reports is counted in
    /// `history::load` AFTER the fold, from the projection's own collections; this one exists only
    /// so `read_b` can ask "did anything parse at all?" and so the truncation tests below can
    /// assert against the window the reader kept.
    pub parsed_items: usize,
    /// Items dropped by TRUNCATION. `None` means records were certainly omitted but cannot be
    /// counted, which happens exactly when the scan started partway into the file.
    ///
    /// **Never counts a skipped line type.** Attachments, side records and unknown lines are not
    /// omitted conversation, and mixing them in would make a user-facing number that cannot be
    /// explained. Those go to [`HistoryCounts`], which is for the log.
    pub omitted_items: Option<usize>,
    /// The CLI version that wrote this file, from the last line carrying one.
    ///
    /// It is **not** a schema version and nothing may branch on it. All four conversational line
    /// types carry it, 100% of the time (2.1.270 / 2.1.272 / 2.1.276 here), which makes it the one
    /// useful thing to report when a parse yields nothing at all.
    pub writer_version: Option<String>,
    /// Log-only tallies. Nothing here reaches the panel.
    pub counts: HistoryCounts,
}

/// Why lines did not become conversation. For the log, never for the UI.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HistoryCounts {
    /// Total lines read that produced no item and no result -- the sum of every field below that
    /// counts a whole line.
    pub skipped_lines: u64,
    /// Lines that were not valid JSON, or were valid JSON but not an object. Includes a
    /// half-written final line. Measured 0 in 117,826 real lines; a partial line can only appear in
    /// a file that is being written, which `is_transcript_stable` exists to avoid reading.
    pub malformed_lines: u64,
    /// Lines longer than [`HISTORY_MAX_LINE_BYTES`], discarded without being buffered.
    pub oversized_lines: u64,
    /// `attachment` lines. The single biggest thing this module throws away: 21% of the bytes in a
    /// session file, and 13-24 of the 19-69 lines in a Neovibe-driven one.
    pub attachment_lines: u64,
    /// `system` lines, all ten subtypes, `compact_boundary` included.
    pub system_lines: u64,
    /// The eleven side-record types (`last-prompt`, `ai-title`, `cost-state`, ...).
    pub side_record_lines: u64,
    /// The five types that occur only in `agent-*.jsonl` subagent files. Defence, not a live path:
    /// zero of them appear in any top-level session file.
    pub subagent_only_lines: u64,
    /// Line types this module has never seen. The reason there is no "everything else" bucket in
    /// the classifier: a future type lands here and is counted, not misread.
    pub unknown_type_lines: u64,
    /// `isSidechain: true`. Zero in every top-level session file measured -- subagent turns live in
    /// separate files whose names `transcript_path` cannot produce. Kept because the split is the
    /// CLI's implementation detail rather than a promise, and because what it stops (a subagent's
    /// internal round trips mixed into the main timeline) fails silently.
    pub sidechain_lines: u64,
    /// `type:"user"` lines that produced neither a prompt nor a tool result.
    ///
    /// Mostly the allow-list's rejections: compaction summaries, `isMeta` injections, slash-command
    /// echoes, `<local-command-stdout>`, bash-mode lines, interrupt markers. But **not only** those
    /// -- a line that never reached the allow-list lands here too, because "nothing came out of it"
    /// is the same observation from this counter's point of view: no `message` object, an empty
    /// `content` array, or a line whose only block was a `tool_result` dropped for an empty or
    /// absent `tool_use_id`. Reading a non-zero value as "this many prompts were deliberately
    /// filtered" would therefore overstate it. Splitting the two would mean a second counter for a
    /// number nothing acts on; the honest doc is the cheaper fix.
    pub rejected_user_lines: u64,
    /// Prompts that were nothing but an editor-context block, which `compose_turn_text` sends when
    /// the user typed no words at all.
    pub empty_prompt_lines: u64,
    /// Content blocks dropped inside an otherwise usable line: `thinking`, `image`, an unknown
    /// block type, a `tool_use` with no usable id.
    pub dropped_blocks: u64,
    /// `tool_result` blocks naming a call that is not in the loaded window -- either genuinely
    /// unmatched, or matched to a call that truncation removed. Measured 0 unmatched in the whole
    /// corpus (23,592 of 23,594 `tool_use` ids pair, 0 orphans, 0 empty ids), so this is a safety
    /// net; what it prevents is two empty ids comparing equal and cross-linking two unrelated
    /// records, which is silent.
    pub orphan_tool_results: u64,
}

/// Reads one transcript file and returns the history it holds.
///
/// `Err` only for a real I/O failure (no such file, no permission, a read that fails partway).
/// Nothing about the file's *contents* can produce one.
pub fn read_transcript_history(path: &Path) -> std::io::Result<TranscriptHistory> {
    let file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    let mut reader = BufReader::with_capacity(64 * 1024, file);
    let mut started_mid_file = false;
    if len > HISTORY_SCAN_MAX_BYTES {
        reader.seek(SeekFrom::Start(len - HISTORY_SCAN_MAX_BYTES))?;
        started_mid_file = true;
        // The seek almost certainly landed inside a line; that partial line is not JSON and is
        // dropped here rather than counted as malformed, because it is an artefact of where this
        // function chose to start, not of the file.
        let _ = read_bounded_line(&mut reader)?;
    }
    read_history_from(&mut reader, started_mid_file)
}

/// The whole fold, over any reader. Split out so tests can drive it from a `&[u8]` without a
/// temporary file, and so [`read_transcript_history`] holds nothing but the seek decision.
pub(crate) fn read_history_from(
    reader: &mut impl BufRead,
    started_mid_file: bool,
) -> std::io::Result<TranscriptHistory> {
    let mut counts = HistoryCounts::default();
    let mut writer_version: Option<String> = None;
    let mut slots: VecDeque<Slot> = VecDeque::new();
    let mut total_chars: usize = 0;
    let mut omitted: usize = 0;

    while let Some(line) = read_bounded_line(reader)? {
        let raw = match line {
            Line::Complete(bytes) => bytes,
            Line::TooLong => {
                counts.oversized_lines += 1;
                counts.skipped_lines += 1;
                continue;
            }
        };
        if raw.iter().all(|b| b.is_ascii_whitespace()) {
            continue;
        }
        let parsed = match std::str::from_utf8(&raw) {
            Ok(text) => classify_line(text),
            // Invalid UTF-8 cannot be JSON either; it is counted the same way and for the same
            // reason, rather than being a separate user-visible category.
            Err(_) => ParsedLine::skipped(SkipReason::Malformed),
        };
        if let Some(version) = parsed.version {
            writer_version = Some(version);
        }
        counts.dropped_blocks += u64::from(parsed.dropped_blocks);
        if let Some(reason) = parsed.skip {
            counts.skipped_lines += 1;
            match reason {
                SkipReason::Malformed => counts.malformed_lines += 1,
                SkipReason::Attachment => counts.attachment_lines += 1,
                SkipReason::System => counts.system_lines += 1,
                SkipReason::SideRecord => counts.side_record_lines += 1,
                SkipReason::SubagentOnly => counts.subagent_only_lines += 1,
                SkipReason::UnknownType => counts.unknown_type_lines += 1,
                SkipReason::Sidechain => counts.sidechain_lines += 1,
                SkipReason::RejectedUser => counts.rejected_user_lines += 1,
                SkipReason::EmptyPrompt => counts.empty_prompt_lines += 1,
            }
        }
        for outcome in parsed.outcomes {
            match outcome {
                Outcome::Item {
                    chars,
                    tool_use_id,
                    event,
                } => {
                    slots.push_back(Slot {
                        chars,
                        tool_use_id,
                        events: vec![event],
                    });
                    total_chars += chars;
                }
                Outcome::ToolResult {
                    tool_use_id,
                    chars,
                    event,
                } => {
                    let call = slots
                        .iter()
                        .position(|s| s.tool_use_id.as_deref() == Some(tool_use_id.as_str()));
                    match call {
                        Some(index) => {
                            // The result's size belongs to the call it completes, so that
                            // truncation weighs a tool call by what it will actually render.
                            slots[index].chars += chars;
                            total_chars += chars;
                            // ...but the event itself stays where the file put it.
                            slots
                                .back_mut()
                                .expect("a matching call means at least one slot exists")
                                .events
                                .push(event);
                        }
                        None => counts.orphan_tool_results += 1,
                    }
                }
            }
            // Eviction is from the FRONT, always: history is taken from the newest backwards.
            // Whole items only -- half a tool result would mean writing a sentence into `content`,
            // which is the provider's own value and must not be forged.
            while slots.len() > HISTORY_MAX_ITEMS || (total_chars > HISTORY_MAX_CHARS && slots.len() > 1) {
                let dropped = slots.pop_front().expect("the loop condition implies a front");
                total_chars -= dropped.chars;
                omitted += 1;
            }
        }
    }

    let parsed_items = slots.len();
    let mut events: Vec<AgentDomainEvent> = Vec::new();
    let mut kept_tool_use_ids: Vec<&str> = Vec::new();
    for slot in &slots {
        if let Some(id) = &slot.tool_use_id {
            kept_tool_use_ids.push(id.as_str());
        }
    }
    for slot in &slots {
        for event in &slot.events {
            // A completion whose call was evicted after the two were matched. Dropping it here
            // rather than letting `apply` find nothing keeps "an event in this list always does
            // something" true, and keeps the orphan count honest.
            if let AgentDomainEvent::ToolCallCompleted { tool_use_id, .. } = event {
                if !kept_tool_use_ids.contains(&tool_use_id.as_str()) {
                    counts.orphan_tool_results += 1;
                    continue;
                }
            }
            events.push(event.clone());
        }
    }

    Ok(TranscriptHistory {
        events,
        parsed_items,
        // A scan that began partway into the file omitted an unknown number of records. Reporting
        // `Some(omitted)` there would be a precise-looking count of only the part that was seen.
        omitted_items: if started_mid_file { None } else { Some(omitted) },
        writer_version,
        counts,
    })
}

/// One conversational item, plus the tool results that followed it in the file.
///
/// Eviction removes a whole slot. Because eviction only ever takes from the front, a slot's
/// trailing results can only refer to calls in this slot or in an earlier one -- and an earlier one
/// is always evicted first -- so keeping a call never loses its result.
struct Slot {
    chars: usize,
    /// Set only for a tool-call item; this is how a later `tool_result` finds the call it belongs
    /// to. Never `Some("")`: an empty id compares equal to every other empty id, which would
    /// cross-link two unrelated records, so such a block is dropped at parse time instead.
    tool_use_id: Option<String>,
    events: Vec<AgentDomainEvent>,
}

enum Outcome {
    Item {
        chars: usize,
        tool_use_id: Option<String>,
        event: AgentDomainEvent,
    },
    ToolResult {
        tool_use_id: String,
        chars: usize,
        event: AgentDomainEvent,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SkipReason {
    Malformed,
    Attachment,
    System,
    SideRecord,
    SubagentOnly,
    UnknownType,
    Sidechain,
    RejectedUser,
    EmptyPrompt,
}

pub(crate) struct ParsedLine {
    outcomes: Vec<Outcome>,
    skip: Option<SkipReason>,
    dropped_blocks: u32,
    version: Option<String>,
}

impl ParsedLine {
    fn skipped(reason: SkipReason) -> Self {
        Self {
            outcomes: Vec::new(),
            skip: Some(reason),
            dropped_blocks: 0,
            version: None,
        }
    }
}

/// Every `type` value this module knows about, so that the classifier is a total match and a new
/// one has to be added deliberately rather than falling into a catch-all.
///
/// The first fifteen are the complete set found in top-level session files. The next five occur
/// only in `agent-*.jsonl` subagent files, which `transcript_path` can never name; they are listed
/// because the file split is the CLI's implementation detail, not a promise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LineType {
    User,
    Assistant,
    Attachment,
    System,
    LastPrompt,
    AtisLatch,
    PermissionMode,
    Mode,
    AiTitle,
    QueueOperation,
    FileHistorySnapshot,
    FileHistoryDelta,
    Relocated,
    WorktreeState,
    CostState,
    Started,
    Result,
    Failed,
    Launched,
    ForkContextRef,
    Unknown,
}

fn line_type(name: &str) -> LineType {
    match name {
        "user" => LineType::User,
        "assistant" => LineType::Assistant,
        "attachment" => LineType::Attachment,
        "system" => LineType::System,
        "last-prompt" => LineType::LastPrompt,
        "atis-latch" => LineType::AtisLatch,
        "permission-mode" => LineType::PermissionMode,
        "mode" => LineType::Mode,
        "ai-title" => LineType::AiTitle,
        "queue-operation" => LineType::QueueOperation,
        "file-history-snapshot" => LineType::FileHistorySnapshot,
        "file-history-delta" => LineType::FileHistoryDelta,
        "relocated" => LineType::Relocated,
        "worktree-state" => LineType::WorktreeState,
        "cost-state" => LineType::CostState,
        "started" => LineType::Started,
        "result" => LineType::Result,
        "failed" => LineType::Failed,
        "launched" => LineType::Launched,
        "fork-context-ref" => LineType::ForkContextRef,
        _ => LineType::Unknown,
    }
}

/// One line of the file to zero or more outcomes. Pure: no I/O, no clock, no state.
pub(crate) fn classify_line(raw: &str) -> ParsedLine {
    let Ok(value) = serde_json::from_str::<Value>(raw) else {
        return ParsedLine::skipped(SkipReason::Malformed);
    };
    if !value.is_object() {
        return ParsedLine::skipped(SkipReason::Malformed);
    }
    let version = value.get("version").and_then(Value::as_str).map(str::to_owned);
    let with_version = |mut parsed: ParsedLine| {
        parsed.version = version.clone();
        parsed
    };

    // Checked before the type, so it covers every type at once.
    if flag_is_true(&value, "isSidechain") {
        return with_version(ParsedLine::skipped(SkipReason::Sidechain));
    }

    let Some(kind) = value.get("type").and_then(Value::as_str) else {
        return with_version(ParsedLine::skipped(SkipReason::UnknownType));
    };
    match line_type(kind) {
        LineType::User => with_version(classify_user_line(&value)),
        LineType::Assistant => with_version(classify_assistant_line(&value)),
        LineType::Attachment => with_version(ParsedLine::skipped(SkipReason::Attachment)),
        // Every subtype, `compact_boundary` included. Session identity comes only from a live
        // `SessionOpened`, never from history, and drawing "a compaction happened here" line is a
        // separate product decision; what matters is already done, which is that the summary
        // itself cannot become the user's words.
        LineType::System => with_version(ParsedLine::skipped(SkipReason::System)),
        LineType::LastPrompt
        | LineType::AtisLatch
        | LineType::PermissionMode
        | LineType::Mode
        | LineType::AiTitle
        | LineType::QueueOperation
        | LineType::FileHistorySnapshot
        | LineType::FileHistoryDelta
        | LineType::Relocated
        | LineType::WorktreeState
        // `cost-state` carries `totalCostUSD` and `modelUsage`. History still does not fill
        // `usage`: a restored figure would read as this session's live running cost.
        | LineType::CostState => with_version(ParsedLine::skipped(SkipReason::SideRecord)),
        LineType::Started
        | LineType::Result
        | LineType::Failed
        | LineType::Launched
        | LineType::ForkContextRef => with_version(ParsedLine::skipped(SkipReason::SubagentOnly)),
        LineType::Unknown => with_version(ParsedLine::skipped(SkipReason::UnknownType)),
    }
}

fn flag_is_true(value: &Value, key: &str) -> bool {
    value.get(key).and_then(Value::as_bool) == Some(true)
}

/// The allow-list of §3.1.1, in full. Condition 1 ("no `tool_result` block") is applied at the
/// block level by the caller, because one real line carries both a `text` and a `tool_result` and
/// the two halves have different answers.
fn user_line_is_the_users_own_words(value: &Value) -> bool {
    !flag_is_true(value, "isMeta")
        && !flag_is_true(value, "isCompactSummary")
        && !flag_is_true(value, "isVisibleInTranscriptOnly")
        && !flag_is_true(value, "isSidechain")
        && value
            .get("promptSource")
            .and_then(Value::as_str)
            .is_some_and(|source| HUMAN_PROMPT_SOURCES.contains(&source))
}

fn classify_user_line(value: &Value) -> ParsedLine {
    let turn_id = line_uuid(value);
    let content = value.get("message").and_then(|m| m.get("content"));
    let mut outcomes = Vec::new();
    let mut dropped_blocks = 0u32;
    let mut saw_tool_result = false;
    let mut prompt_text = String::new();

    match content {
        Some(Value::String(text)) => prompt_text.push_str(text),
        Some(Value::Array(blocks)) => {
            for block in blocks {
                match block.get("type").and_then(Value::as_str) {
                    Some("tool_result") => {
                        saw_tool_result = true;
                        match tool_result_outcome(block, &turn_id) {
                            Some(outcome) => outcomes.push(outcome),
                            None => dropped_blocks += 1,
                        }
                    }
                    Some("text") => match block.get("text").and_then(Value::as_str) {
                        Some(text) => prompt_text.push_str(text),
                        None => dropped_blocks += 1,
                    },
                    // `image` has never appeared on a `user`/`assistant` line here; the panel has
                    // no image renderer either way.
                    _ => dropped_blocks += 1,
                }
            }
        }
        _ => {}
    }

    let mut skip = None;
    // A line carrying a tool result is a transport frame, never a message -- so its text, if any,
    // is not the user's. Dropping that half is the safe direction. One such mixed line exists in
    // the whole corpus, and the text on it is not a person's words.
    if !prompt_text.is_empty() && saw_tool_result {
        dropped_blocks += 1;
    }
    if !prompt_text.is_empty() && !saw_tool_result {
        if user_line_is_the_users_own_words(value) {
            let text = strip_composed_block(&prompt_text).to_owned();
            if text.is_empty() {
                skip = Some(SkipReason::EmptyPrompt);
            } else {
                outcomes.push(Outcome::Item {
                    chars: text.chars().count(),
                    tool_use_id: None,
                    event: AgentDomainEvent::UserPromptSubmitted { text },
                });
            }
        } else {
            skip = Some(SkipReason::RejectedUser);
        }
    } else if outcomes.is_empty() {
        skip = Some(SkipReason::RejectedUser);
    }

    ParsedLine {
        outcomes,
        skip,
        dropped_blocks,
        version: None,
    }
}

fn tool_result_outcome(block: &Value, turn_id: &str) -> Option<Outcome> {
    let tool_use_id = block.get("tool_use_id").and_then(Value::as_str)?;
    if tool_use_id.is_empty() {
        return None;
    }
    let content = block.get("content").cloned().unwrap_or(Value::Null);
    let is_error = block.get("is_error").and_then(Value::as_bool).unwrap_or(false);
    Some(Outcome::ToolResult {
        tool_use_id: tool_use_id.to_owned(),
        chars: value_chars(&content),
        event: AgentDomainEvent::ToolCallCompleted {
            turn_id: turn_id.to_owned(),
            tool_use_id: tool_use_id.to_owned(),
            content,
            is_error,
        },
    })
}

fn classify_assistant_line(value: &Value) -> ParsedLine {
    let turn_id = line_uuid(value);
    let mut outcomes = Vec::new();
    let mut dropped_blocks = 0u32;
    if let Some(Value::Array(blocks)) = value.get("message").and_then(|m| m.get("content")) {
        for block in blocks {
            match block.get("type").and_then(Value::as_str) {
                Some("text") => match block.get("text").and_then(Value::as_str) {
                    Some(text) => outcomes.push(Outcome::Item {
                        chars: text.chars().count(),
                        tool_use_id: None,
                        event: AgentDomainEvent::ContentDelta {
                            turn_id: turn_id.clone(),
                            kind: ContentKind::Text,
                            text: text.to_owned(),
                        },
                    }),
                    None => dropped_blocks += 1,
                },
                Some("tool_use") => match tool_use_outcome(block, &turn_id) {
                    Some(outcome) => outcomes.push(outcome),
                    None => dropped_blocks += 1,
                },
                // `thinking` has no projection effect on the live path either, so restoring it
                // would make history show something a live turn does not. 13,588 of them here.
                _ => dropped_blocks += 1,
            }
        }
    }
    let skip = if outcomes.is_empty() && dropped_blocks == 0 {
        Some(SkipReason::Malformed)
    } else {
        None
    };
    ParsedLine {
        outcomes,
        skip,
        dropped_blocks,
        version: None,
    }
}

fn tool_use_outcome(block: &Value, turn_id: &str) -> Option<Outcome> {
    let id = block.get("id").and_then(Value::as_str)?;
    if id.is_empty() {
        return None;
    }
    let name = block.get("name").and_then(Value::as_str)?;
    let input = block.get("input").cloned().unwrap_or(Value::Null);
    Some(Outcome::Item {
        chars: name.chars().count() + value_chars(&input),
        tool_use_id: Some(id.to_owned()),
        event: AgentDomainEvent::ToolCallStarted {
            turn_id: turn_id.to_owned(),
            tool_use_id: id.to_owned(),
            name: name.to_owned(),
            input,
        },
    })
}

/// `ToolCallRecord::turn_id` is a required `String` and history has no notion of a turn, so the
/// line's own `uuid` stands in. It never reaches a screen -- `serialize_snapshot_for_js` does not
/// send it -- so it is a debugging aid, not a claim. The four conversational line types carry a
/// `uuid` 100% of the time; a line without one gets the empty string rather than a fabricated id.
fn line_uuid(value: &Value) -> String {
    value.get("uuid").and_then(Value::as_str).unwrap_or_default().to_owned()
}

/// Size of a JSON value as it will be rendered, for the character budget. A serialization failure
/// (only reachable for a value this crate did not build) counts as zero rather than aborting a
/// load; the consequence is a slightly under-counted budget, not a lost conversation.
pub(crate) fn value_chars(value: &Value) -> usize {
    match value {
        Value::Null => 0,
        Value::String(text) => text.chars().count(),
        other => serde_json::to_string(other).map(|s| s.chars().count()).unwrap_or(0),
    }
}

enum Line {
    Complete(Vec<u8>),
    TooLong,
}

/// One line, without ever holding more than [`HISTORY_MAX_LINE_BYTES`] of it.
///
/// `BufRead::read_line`/`read_until` would allocate the whole line first and discover its length
/// afterwards, which is exactly the guarantee this function exists to provide. An over-long line's
/// bytes are consumed and thrown away as they arrive, so the peak is the buffer, not the line.
fn read_bounded_line(reader: &mut impl BufRead) -> std::io::Result<Option<Line>> {
    let mut buf: Vec<u8> = Vec::new();
    let mut over_long = false;
    loop {
        let (finished, consumed) = {
            let available = match reader.fill_buf() {
                Ok(bytes) => bytes,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            };
            if available.is_empty() {
                if buf.is_empty() && !over_long {
                    return Ok(None);
                }
                return Ok(Some(if over_long { Line::TooLong } else { Line::Complete(buf) }));
            }
            match available.iter().position(|byte| *byte == b'\n') {
                Some(index) => {
                    if !over_long {
                        buf.extend_from_slice(&available[..index]);
                    }
                    (true, index + 1)
                }
                None => {
                    if !over_long {
                        buf.extend_from_slice(available);
                    }
                    (false, available.len())
                }
            }
        };
        reader.consume(consumed);
        if buf.len() > HISTORY_MAX_LINE_BYTES {
            over_long = true;
            buf = Vec::new();
        }
        if finished {
            return Ok(Some(if over_long { Line::TooLong } else { Line::Complete(buf) }));
        }
    }
}

/// Every real top-level session transcript under this machine's `$CLAUDE_CONFIG_DIR/projects`.
///
/// Shared by the `#[ignore]`d real-corpus tests in this module and in `history::load`, so the two
/// walk the SAME set of files: a bucket holds directories as well as files, and `agent-<hex>.jsonl`
/// subagent logs as well as `<uuid>.jsonl` sessions, and only the latter is what `transcript_path`
/// can ever name. Two hand-written copies of that filter would be two definitions of "the corpus".
#[cfg(test)]
pub(crate) fn real_session_files() -> Vec<std::path::PathBuf> {
    let Ok(root) = crate::transcript::claude_projects_dir() else {
        panic!("neither CLAUDE_CONFIG_DIR nor HOME is set");
    };
    let mut files = Vec::new();
    let buckets = std::fs::read_dir(&root).expect("the projects directory exists");
    for bucket in buckets {
        let bucket = bucket.expect("readable bucket entry").path();
        if !bucket.is_dir() {
            continue;
        }
        for entry in std::fs::read_dir(&bucket).expect("readable bucket") {
            let path = entry.expect("readable entry").path();
            if !path.is_file() {
                continue;
            }
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if !name.ends_with(".jsonl") || name.len() != 36 + ".jsonl".len() {
                continue;
            }
            if !name[..36].chars().all(|c| c.is_ascii_hexdigit() || c == '-') {
                continue;
            }
            files.push(path);
        }
    }
    assert!(!files.is_empty(), "no session files found under {}", root.display());
    files
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn fold(lines: &str) -> TranscriptHistory {
        read_history_from(&mut Cursor::new(lines.as_bytes()), false).expect("a cursor cannot fail")
    }

    fn skip_reason(line: &str) -> Option<SkipReason> {
        classify_line(line).skip
    }

    fn prompts(history: &TranscriptHistory) -> Vec<String> {
        history
            .events
            .iter()
            .filter_map(|event| match event {
                AgentDomainEvent::UserPromptSubmitted { text } => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    fn assistant_text(history: &TranscriptHistory) -> Vec<String> {
        history
            .events
            .iter()
            .filter_map(|event| match event {
                AgentDomainEvent::ContentDelta { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    fn user_line(extra: &str, content: &str) -> String {
        format!(
            r#"{{"type":"user","uuid":"u-1","version":"2.1.272",{extra}"message":{{"role":"user","content":{content}}}}}"#
        )
    }

    fn typed_prompt(text: &str) -> String {
        user_line(
            r#""promptSource":"typed","#,
            &serde_json::to_string(text).expect("a string serializes"),
        )
    }

    // ---------------------------------------------------------------------------------------
    // The fifteen line types a top-level session file can hold, plus the five that only occur in
    // subagent files. Every one has a stated destination; there is no "everything else" bucket, so
    // each is checked rather than assumed.
    // ---------------------------------------------------------------------------------------

    #[test]
    fn a_users_own_prompt_becomes_a_prompt_event() {
        let history = fold(&typed_prompt("why is this slow?"));
        assert_eq!(prompts(&history), vec!["why is this slow?".to_string()]);
        assert_eq!(history.parsed_items, 1);
        assert_eq!(history.omitted_items, Some(0));
        assert_eq!(history.counts.skipped_lines, 0);
    }

    #[test]
    fn an_assistant_text_block_becomes_a_text_delta() {
        let line = r#"{"type":"assistant","uuid":"a-1","version":"2.1.272","message":{"role":"assistant","content":[{"type":"text","text":"here is why"}]}}"#;
        let history = fold(line);
        assert_eq!(assistant_text(&history), vec!["here is why".to_string()]);
        assert!(matches!(
            history.events[0],
            AgentDomainEvent::ContentDelta {
                kind: ContentKind::Text,
                ..
            }
        ));
    }

    #[test]
    fn an_attachment_line_is_skipped_and_counted() {
        let line = r#"{"type":"attachment","uuid":"x","version":"2.1.272","attachment":{"type":"file"}}"#;
        assert_eq!(skip_reason(line), Some(SkipReason::Attachment));
        let history = fold(line);
        assert!(history.events.is_empty());
        assert_eq!(history.counts.attachment_lines, 1);
        assert_eq!(history.counts.skipped_lines, 1);
    }

    /// All ten subtypes, `compact_boundary` included. Session identity never comes from history.
    #[test]
    fn every_system_subtype_is_skipped_and_counted() {
        for subtype in [
            "turn_duration",
            "stop_hook_summary",
            "away_summary",
            "informational",
            "api_error",
            "compact_boundary",
            "local_command",
            "model_refusal_fallback",
            "scheduled_task_fire",
            "model_refusal_no_fallback",
        ] {
            let line = format!(
                r#"{{"type":"system","subtype":"{subtype}","uuid":"s","version":"2.1.272","content":"Conversation compacted"}}"#
            );
            assert_eq!(skip_reason(&line), Some(SkipReason::System), "subtype {subtype}");
        }
    }

    #[test]
    fn all_eleven_side_record_types_are_skipped_and_counted() {
        let lines = [
            r#"{"type":"last-prompt","leafUuid":"l","sessionId":"s"}"#,
            r#"{"type":"atis-latch","atis":{},"sessionId":"s"}"#,
            r#"{"type":"permission-mode","permissionMode":"auto","sessionId":"s"}"#,
            r#"{"type":"mode","mode":"default","sessionId":"s"}"#,
            r#"{"type":"ai-title","aiTitle":"A title","sessionId":"s"}"#,
            r#"{"type":"queue-operation","operation":"add","sessionId":"s"}"#,
            r#"{"type":"file-history-snapshot","messageId":"m","snapshot":{},"isSnapshotUpdate":false}"#,
            r#"{"type":"file-history-delta","messageId":"m","snapshotMessageId":"s","trackingPath":"/p"}"#,
            r#"{"type":"relocated","sessionId":"s"}"#,
            r#"{"type":"worktree-state","sessionId":"s"}"#,
            r#"{"type":"cost-state","totalCostUSD":1.5,"modelUsage":{}}"#,
        ];
        for line in lines {
            assert_eq!(skip_reason(line), Some(SkipReason::SideRecord), "line {line}");
        }
        let history = fold(&lines.join("\n"));
        assert!(history.events.is_empty());
        assert_eq!(history.counts.side_record_lines, 11);
        assert_eq!(history.counts.skipped_lines, 11);
    }

    /// `cost-state` is the one side record carrying something the projection has a field for.
    /// History still does not fill `usage`: a restored figure would read as this session's live
    /// running cost.
    #[test]
    fn a_cost_state_line_contributes_no_usage() {
        let history = fold(r#"{"type":"cost-state","totalCostUSD":12.5,"modelUsage":{"m":1}}"#);
        assert!(history.events.is_empty());
    }

    #[test]
    fn the_five_subagent_only_types_are_skipped_and_counted() {
        for kind in ["started", "result", "failed", "launched", "fork-context-ref"] {
            let line = format!(r#"{{"type":"{kind}","agentId":"a","key":"k"}}"#);
            assert_eq!(skip_reason(&line), Some(SkipReason::SubagentOnly), "type {kind}");
        }
    }

    #[test]
    fn an_unknown_line_type_is_skipped_and_counted_never_an_error() {
        let history = fold(
            r#"{"type":"a-type-invented-after-this-was-written","payload":{"anything":1}}
{"noTypeFieldAtAll":true}"#,
        );
        assert!(history.events.is_empty());
        assert_eq!(history.counts.unknown_type_lines, 2);
    }

    #[test]
    fn a_malformed_line_is_skipped_and_counted_never_an_error() {
        let history = fold(
            r#"not json at all
[1,2,3]
{"type":"user","promptSource":"typed","message":{"role":"user","content":"kept"}}
{"type":"user","promptSource":"typed","message":{"role":"user","conte"#,
        );
        assert_eq!(prompts(&history), vec!["kept".to_string()]);
        // The bare array is valid JSON but not an object; the half-written final line is neither.
        assert_eq!(history.counts.malformed_lines, 3);
    }

    // ---------------------------------------------------------------------------------------
    // The allow-list. Every branch, because the failure direction of getting this wrong is
    // showing someone else's text under the user's name.
    // ---------------------------------------------------------------------------------------

    #[test]
    fn each_allowed_prompt_source_produces_a_prompt() {
        for source in HUMAN_PROMPT_SOURCES {
            let line = user_line(&format!(r#""promptSource":"{source}","#), r#""hello""#);
            let history = fold(&line);
            assert_eq!(prompts(&history), vec!["hello".to_string()], "source {source}");
        }
    }

    #[test]
    fn a_prompt_source_outside_the_allow_list_is_rejected() {
        for source in ["system", "hook", "", "TYPED"] {
            let line = user_line(&format!(r#""promptSource":"{source}","#), r#""not the user""#);
            assert_eq!(skip_reason(&line), Some(SkipReason::RejectedUser), "source {source}");
        }
    }

    /// The 131 measured lines with no `promptSource` and no `isMeta` are interrupt markers, slash
    /// command echoes, `<local-command-stdout>` and bash-mode lines. Not one is a person's prose.
    #[test]
    fn a_user_line_with_no_prompt_source_is_rejected() {
        for content in [
            "[Request interrupted by user]",
            "[Request interrupted by user for tool use]",
            "<command-name>/clear</command-name>",
            "<local-command-stdout>ok</local-command-stdout>",
            "<bash-input>ls</bash-input>",
            "/compact",
        ] {
            let line = user_line("", &serde_json::to_string(content).unwrap());
            assert_eq!(skip_reason(&line), Some(SkipReason::RejectedUser), "content {content}");
        }
    }

    #[test]
    fn each_exclusion_flag_alone_rejects_a_line_that_would_otherwise_pass() {
        for flag in ["isMeta", "isCompactSummary", "isVisibleInTranscriptOnly", "isSidechain"] {
            let line = user_line(
                &format!(r#""promptSource":"typed","{flag}":true,"#),
                r#""would otherwise pass""#,
            );
            let reason = skip_reason(&line);
            // `isSidechain` is caught earlier, by the whole-line guard that applies to every type.
            let expected = if flag == "isSidechain" {
                SkipReason::Sidechain
            } else {
                SkipReason::RejectedUser
            };
            assert_eq!(reason, Some(expected), "flag {flag}");
        }
    }

    /// `false` is not `true`: a line that spells its flags out must still pass.
    #[test]
    fn exclusion_flags_set_to_false_do_not_reject() {
        let line = user_line(
            r#""promptSource":"typed","isMeta":false,"isCompactSummary":false,"isVisibleInTranscriptOnly":false,"isSidechain":false,"#,
            r#""real words""#,
        );
        assert_eq!(prompts(&fold(&line)), vec!["real words".to_string()]);
    }

    /// The blocker this whole allow-list exists for. A compaction summary is a `type:"user"` line
    /// whose content is a plain string -- shape-identical to a typed prompt -- and it is the model's
    /// own summary of the conversation, several KB of it. Rendering it as the user's words would
    /// both invent a prompt and breach the panel's own "never say what the agent remembers" rule.
    #[test]
    fn a_compaction_summary_never_becomes_the_users_words_and_its_body_reaches_nothing() {
        const BODY: &str = "SUMMARY-BODY-SENTINEL";
        let line = user_line(
            r#""isCompactSummary":true,"isVisibleInTranscriptOnly":true,"#,
            &serde_json::to_string(&format!(
                "This session is being continued from a previous conversation that ran out of context. The summary below covers the earlier portion of the conversation.\n\nSummary:\n{BODY}"
            ))
            .unwrap(),
        );
        let history = fold(&line);
        assert!(history.events.is_empty(), "{:?}", history.events);
        assert_eq!(history.counts.rejected_user_lines, 1);
        let rendered = format!("{:?}", history.events);
        assert!(!rendered.contains(BODY), "the summary body reached the events");
    }

    /// `isSidechain` never appears in a top-level session file, so this is a safety net rather than
    /// a live path. It is checked for every type at once, before the type is even looked at.
    #[test]
    fn a_sidechain_line_of_any_type_is_skipped_and_counted() {
        let lines = [
            r#"{"type":"user","isSidechain":true,"promptSource":"typed","message":{"role":"user","content":"sub"}}"#,
            r#"{"type":"assistant","isSidechain":true,"message":{"role":"assistant","content":[{"type":"text","text":"sub"}]}}"#,
        ];
        for line in lines {
            assert_eq!(skip_reason(line), Some(SkipReason::Sidechain), "line {line}");
        }
        let history = fold(&lines.join("\n"));
        assert!(history.events.is_empty());
        assert_eq!(history.counts.sidechain_lines, 2);
    }

    // ---------------------------------------------------------------------------------------
    // Blocks
    // ---------------------------------------------------------------------------------------

    #[test]
    fn an_allowed_user_line_whose_content_is_an_array_of_text_blocks_is_joined() {
        let line = user_line(
            r#""promptSource":"typed","#,
            r#"[{"type":"text","text":"first "},{"type":"text","text":"second"}]"#,
        );
        assert_eq!(prompts(&fold(&line)), vec!["first second".to_string()]);
    }

    #[test]
    fn a_tool_use_and_its_result_become_a_call_and_its_completion() {
        let lines = concat!(
            r#"{"type":"assistant","uuid":"a-1","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"ls"}}]}}"#,
            "\n",
            r#"{"type":"user","uuid":"u-1","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"a\nb","is_error":false}]}}"#,
        );
        let history = fold(lines);
        assert_eq!(history.events.len(), 2);
        // A result is not an item: it belongs to the call it completes.
        assert_eq!(history.parsed_items, 1);
        match &history.events[0] {
            AgentDomainEvent::ToolCallStarted {
                tool_use_id,
                name,
                input,
                ..
            } => {
                assert_eq!(tool_use_id, "toolu_1");
                assert_eq!(name, "Bash");
                assert_eq!(input["command"], "ls");
            }
            other => panic!("expected a tool call, got {other:?}"),
        }
        match &history.events[1] {
            AgentDomainEvent::ToolCallCompleted {
                tool_use_id,
                content,
                is_error,
                ..
            } => {
                assert_eq!(tool_use_id, "toolu_1");
                assert_eq!(content, "a\nb");
                assert!(!is_error);
            }
            other => panic!("expected a completion, got {other:?}"),
        }
    }

    /// `is_error` is absent on 917 of the measured results. Absent means not an error, and the
    /// value must not be invented some other way.
    #[test]
    fn a_tool_result_with_no_is_error_field_is_not_an_error() {
        let lines = concat!(
            r#"{"type":"assistant","uuid":"a","message":{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Read","input":{}}]}}"#,
            "\n",
            r#"{"type":"user","uuid":"u","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":[{"type":"text","text":"x"}]}]}}"#,
        );
        let history = fold(lines);
        match &history.events[1] {
            AgentDomainEvent::ToolCallCompleted { is_error, content, .. } => {
                assert!(!is_error);
                // The provider's own value, passed through byte for byte rather than flattened.
                assert!(content.is_array());
            }
            other => panic!("expected a completion, got {other:?}"),
        }
    }

    /// The one measured line carrying both a `text` and a `tool_result`. The two halves get
    /// different answers: the result is consumed, and the text is dropped because a line carrying a
    /// tool result is a transport frame, not a message.
    #[test]
    fn a_line_carrying_both_text_and_a_tool_result_keeps_only_the_result() {
        let lines = concat!(
            r#"{"type":"assistant","uuid":"a","message":{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Read","input":{}}]}}"#,
            "\n",
            r#"{"type":"user","uuid":"u","promptSource":"typed","message":{"role":"user","content":[{"type":"text","text":"NOT-THE-USERS-WORDS"},{"type":"tool_result","tool_use_id":"t1","content":"out"}]}}"#,
        );
        let history = fold(lines);
        assert!(prompts(&history).is_empty());
        assert_eq!(history.events.len(), 2);
        assert_eq!(history.counts.dropped_blocks, 1);
    }

    #[test]
    fn a_thinking_block_is_dropped_and_counted() {
        let line = r#"{"type":"assistant","uuid":"a","message":{"role":"assistant","content":[{"type":"thinking","thinking":"THINKING-SENTINEL","signature":"s"}]}}"#;
        let history = fold(line);
        assert!(history.events.is_empty());
        assert_eq!(history.counts.dropped_blocks, 1);
        assert!(!format!("{:?}", history.events).contains("THINKING-SENTINEL"));
    }

    /// Defensive: no `image` block has ever appeared on a `user`/`assistant` line here, and the
    /// panel has no image renderer.
    #[test]
    fn an_image_block_is_dropped_and_counted() {
        let line = r#"{"type":"assistant","uuid":"a","message":{"role":"assistant","content":[{"type":"image","source":{"data":"AAAA"}},{"type":"text","text":"caption"}]}}"#;
        let history = fold(line);
        assert_eq!(assistant_text(&history), vec!["caption".to_string()]);
        assert_eq!(history.counts.dropped_blocks, 1);
    }

    /// Never produce `Some("")`: an empty id compares equal to every other empty id, which
    /// cross-links two unrelated records -- silently.
    #[test]
    fn a_tool_use_with_an_empty_or_missing_id_is_dropped_and_counted() {
        for block in [
            r#"{"type":"tool_use","id":"","name":"Bash","input":{}}"#,
            r#"{"type":"tool_use","name":"Bash","input":{}}"#,
            r#"{"type":"tool_use","id":"t1","input":{}}"#,
        ] {
            let line =
                format!(r#"{{"type":"assistant","uuid":"a","message":{{"role":"assistant","content":[{block}]}}}}"#);
            let history = fold(&line);
            assert!(history.events.is_empty(), "block {block}");
            assert_eq!(history.counts.dropped_blocks, 1, "block {block}");
        }
    }

    #[test]
    fn a_tool_result_matching_no_call_is_dropped_and_counted() {
        for block in [
            r#"{"type":"tool_result","tool_use_id":"nothing-has-this-id","content":"x"}"#,
            r#"{"type":"tool_result","tool_use_id":"","content":"x"}"#,
            r#"{"type":"tool_result","content":"x"}"#,
        ] {
            let line = format!(r#"{{"type":"user","uuid":"u","message":{{"role":"user","content":[{block}]}}}}"#);
            let history = fold(&line);
            assert!(history.events.is_empty(), "block {block}");
        }
        let unmatched = r#"{"type":"user","uuid":"u","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"absent","content":"x"}]}}"#;
        assert_eq!(fold(unmatched).counts.orphan_tool_results, 1);
    }

    // ---------------------------------------------------------------------------------------
    // The editor-context block
    // ---------------------------------------------------------------------------------------

    /// The prompt on disk is the WIRE turn. Two of the six Neovibe-driven sessions on this machine
    /// carry a block this project's own `compose_turn_text` appended, and
    /// `UserPromptSubmitted::text` is documented as what the user typed, not what went on the wire.
    #[test]
    fn a_stored_prompt_loses_the_editor_context_block_this_project_composed_into_it() {
        let wire = "why is this slow?\n\nThe user selected the lines 12 to 14 from /p/src/main.rs:\nfn a() {}\n\nThis may or may not be related to the current task.";
        let line = user_line(r#""promptSource":"sdk","#, &serde_json::to_string(wire).unwrap());
        assert_eq!(prompts(&fold(&line)), vec!["why is this slow?".to_string()]);
    }

    /// `compose_turn_text` sends the block alone when the user typed nothing. What is left after
    /// stripping is not a prompt, and rendering an empty bubble would be worse than rendering
    /// nothing.
    #[test]
    fn a_prompt_that_was_nothing_but_a_context_block_is_dropped_and_counted() {
        let wire =
            "The user opened the file /p/src/main.rs in the IDE. This may or may not be related to the current task.";
        let line = user_line(r#""promptSource":"sdk","#, &serde_json::to_string(wire).unwrap());
        let history = fold(&line);
        assert!(history.events.is_empty());
        assert_eq!(history.counts.empty_prompt_lines, 1);
    }

    // ---------------------------------------------------------------------------------------
    // Order, truncation and bounds
    // ---------------------------------------------------------------------------------------

    /// File order, not a walk of `parentUuid`: 20 of this machine's 44 session files hold 127
    /// positions where one parent has several children, and which child continues the main line is
    /// not expressed on disk.
    #[test]
    fn items_come_out_in_file_order_even_when_parent_uuids_branch() {
        let lines = [
            r#"{"type":"user","uuid":"u1","parentUuid":"p","promptSource":"typed","message":{"role":"user","content":"first"}}"#,
            r#"{"type":"assistant","uuid":"a1","parentUuid":"p","message":{"role":"assistant","content":[{"type":"text","text":"reply"}]}}"#,
            r#"{"type":"user","uuid":"u2","parentUuid":"p","promptSource":"typed","message":{"role":"user","content":"second"}}"#,
        ];
        let history = fold(&lines.join("\n"));
        let order: Vec<&str> = history
            .events
            .iter()
            .map(|event| match event {
                AgentDomainEvent::UserPromptSubmitted { text } => text.as_str(),
                AgentDomainEvent::ContentDelta { text, .. } => text.as_str(),
                other => panic!("unexpected {other:?}"),
            })
            .collect();
        assert_eq!(order, ["first", "reply", "second"]);
    }

    /// The newest items, not the oldest. Expressed against the constant, never against its value,
    /// so that retuning the constant is not supposed to turn anything red.
    #[test]
    fn truncation_keeps_the_newest_items_and_counts_what_it_dropped() {
        let extra = 37usize;
        let total = HISTORY_MAX_ITEMS + extra;
        let lines: String = (0..total)
            .map(|i| typed_prompt(&format!("prompt-{i}")))
            .collect::<Vec<_>>()
            .join("\n");
        let history = fold(&lines);
        assert_eq!(history.parsed_items, HISTORY_MAX_ITEMS);
        assert_eq!(history.omitted_items, Some(extra));
        let kept = prompts(&history);
        assert_eq!(
            kept.first().map(String::as_str),
            Some(format!("prompt-{extra}").as_str())
        );
        assert_eq!(
            kept.last().map(String::as_str),
            Some(format!("prompt-{}", total - 1).as_str())
        );
    }

    /// `omitted_items` is what the panel puts in front of the user, so it counts omitted
    /// CONVERSATION and nothing else. Attachments, side records and unknown lines are not
    /// conversation, and a number mixing them in could not be explained to anyone.
    #[test]
    fn skipped_line_types_never_move_the_omitted_count() {
        let noise = concat!(
            r#"{"type":"attachment","attachment":{}}"#,
            "\n",
            r#"{"type":"cost-state","totalCostUSD":1}"#,
            "\n",
            r#"{"type":"a-future-type"}"#,
        );
        let plain: String = (0..3)
            .map(|i| typed_prompt(&format!("p{i}")))
            .collect::<Vec<_>>()
            .join("\n");
        let noisy: String = (0..3)
            .map(|i| format!("{noise}\n{}", typed_prompt(&format!("p{i}"))))
            .collect::<Vec<_>>()
            .join("\n");
        let without = fold(&plain);
        let with = fold(&noisy);
        assert_eq!(with.omitted_items, without.omitted_items);
        assert_eq!(with.parsed_items, without.parsed_items);
        assert_eq!(with.counts.skipped_lines, 9);
    }

    #[test]
    fn the_character_budget_can_bind_before_the_item_count_does() {
        // Two items, each over half the budget: only the newer can fit, well short of the item
        // ceiling. Measured, this is the binding limit for 5 of this machine's 44 sessions.
        let older = "z".repeat(HISTORY_MAX_CHARS * 2 / 3);
        let newer = "y".repeat(HISTORY_MAX_CHARS * 2 / 3);
        let history = fold(&format!("{}\n{}", typed_prompt(&older), typed_prompt(&newer)));
        assert_eq!(history.parsed_items, 1);
        assert_eq!(prompts(&history), vec![newer]);
        assert_eq!(history.omitted_items, Some(1));
    }

    /// A single real item has been measured at 377,284 characters, 38% of the budget, so "one item
    /// is bigger than everything we are allowed to load" is not hypothetical. Showing zero records
    /// while announcing that history was restored is harder to understand than showing one
    /// oversized one.
    #[test]
    fn at_least_one_item_survives_even_when_it_alone_exceeds_the_budget() {
        let huge = "z".repeat(HISTORY_MAX_CHARS + 1_000);
        let history = fold(&format!("{}\n{}", typed_prompt("older"), typed_prompt(&huge)));
        assert_eq!(history.parsed_items, 1);
        assert_eq!(prompts(&history).len(), 1);
        assert_eq!(prompts(&history)[0].chars().count(), huge.chars().count());
    }

    /// A tool call is weighed by what it will render, which includes the result that arrives later
    /// in the file. Without that, a pair of 400 KB tool results would sail past a budget meant to
    /// stop exactly them.
    #[test]
    fn a_tool_calls_result_counts_toward_the_character_budget() {
        let payload = "z".repeat(HISTORY_MAX_CHARS * 2 / 3);
        let lines = format!(
            concat!(
                r#"{{"type":"assistant","uuid":"a1","message":{{"role":"assistant","content":[{{"type":"tool_use","id":"t1","name":"Bash","input":{{}}}}]}}}}"#,
                "\n",
                r#"{{"type":"user","uuid":"u1","message":{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"t1","content":"{payload}"}}]}}}}"#,
                "\n{newest}",
            ),
            payload = payload,
            newest = typed_prompt(&"y".repeat(HISTORY_MAX_CHARS * 2 / 3)),
        );
        let history = fold(&lines);
        // Without the result's size on the call, the two would total two thirds of the budget and
        // nothing would be dropped at all.
        assert_eq!(history.parsed_items, 1);
        // The completion was evicted along with the call it belonged to, so nothing dangles and
        // nothing had to be filtered out afterwards.
        assert!(
            history
                .events
                .iter()
                .all(|e| matches!(e, AgentDomainEvent::UserPromptSubmitted { .. })),
            "{:?}",
            history.events
        );
        assert_eq!(history.counts.orphan_tool_results, 0);
    }

    /// The one case where a completion can outlive its call: the result arrived after a later item,
    /// so the event sits in that later item's slot while its size was charged to the earlier one --
    /// and only the earlier one is evicted. Rendering a completion for a call that is not on screen
    /// would attach a result to nothing, so it is dropped and counted here instead.
    #[test]
    fn a_completion_whose_call_was_evicted_is_dropped_rather_than_left_dangling() {
        let payload = "z".repeat(HISTORY_MAX_CHARS * 2 / 3);
        let lines = format!(
            concat!(
                r#"{{"type":"assistant","uuid":"a1","message":{{"role":"assistant","content":[{{"type":"tool_use","id":"t1","name":"Bash","input":{{}}}}]}}}}"#,
                "\n{middle}\n",
                r#"{{"type":"user","uuid":"u1","message":{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"t1","content":"{payload}"}}]}}}}"#,
                "\n{newest}",
            ),
            middle = typed_prompt("middle"),
            payload = payload,
            newest = typed_prompt(&"y".repeat(HISTORY_MAX_CHARS * 2 / 3)),
        );
        let history = fold(&lines);
        assert_eq!(history.parsed_items, 2, "{:?}", history.events);
        assert!(
            !history
                .events
                .iter()
                .any(|e| matches!(e, AgentDomainEvent::ToolCallCompleted { .. })),
            "{:?}",
            history.events
        );
        assert_eq!(history.counts.orphan_tool_results, 1);
    }

    #[test]
    fn an_over_long_line_is_skipped_without_being_held_in_memory() {
        let huge = "z".repeat(HISTORY_MAX_LINE_BYTES + 16);
        let lines = format!("{}\n{}", typed_prompt(&huge), typed_prompt("after"));
        let history = fold(&lines);
        assert_eq!(prompts(&history), vec!["after".to_string()]);
        assert_eq!(history.counts.oversized_lines, 1);
        assert_eq!(history.counts.skipped_lines, 1);
    }

    #[test]
    fn a_file_with_no_final_newline_still_yields_its_last_line() {
        let history = fold(&format!("{}\n{}", typed_prompt("one"), typed_prompt("two")));
        assert_eq!(prompts(&history), vec!["one".to_string(), "two".to_string()]);
    }

    #[test]
    fn a_completely_unreadable_file_yields_an_empty_history_rather_than_an_error() {
        let history = fold("garbage\n{\"type\":\"nope\"}\n\n");
        assert!(history.events.is_empty());
        assert_eq!(history.parsed_items, 0);
        assert_eq!(history.omitted_items, Some(0));
    }

    #[test]
    fn the_writer_version_comes_from_the_last_line_that_carries_one() {
        let history = fold(&format!(
            "{}\n{}\n{}",
            typed_prompt("a"),
            r#"{"type":"ai-title","aiTitle":"t"}"#,
            user_line(r#""promptSource":"typed","version":"2.1.276","#, r#""b""#),
        ));
        assert_eq!(history.writer_version.as_deref(), Some("2.1.276"));
    }

    #[test]
    fn the_skipped_line_categories_always_sum_to_the_skipped_line_total() {
        let history = fold(&format!(
            "{}\n{}\n{}\n{}\n{}\n{}\n{}",
            r#"{"type":"attachment","attachment":{}}"#,
            r#"{"type":"system","subtype":"turn_duration"}"#,
            r#"{"type":"ai-title","aiTitle":"t"}"#,
            r#"{"type":"launched","agentId":"a"}"#,
            r#"{"type":"unheard-of"}"#,
            r#"{"type":"user","isSidechain":true,"message":{"role":"user","content":"x"}}"#,
            user_line("", r#""no prompt source""#),
        ));
        let c = history.counts;
        assert_eq!(
            c.skipped_lines,
            c.malformed_lines
                + c.oversized_lines
                + c.attachment_lines
                + c.system_lines
                + c.side_record_lines
                + c.subagent_only_lines
                + c.unknown_type_lines
                + c.sidechain_lines
                + c.rejected_user_lines
                + c.empty_prompt_lines
        );
        assert_eq!(c.skipped_lines, 7);
    }

    // ---------------------------------------------------------------------------------------
    // Reading a file, including the bounded tail scan
    // ---------------------------------------------------------------------------------------

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("agent-history-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    #[test]
    fn a_missing_file_is_an_io_error_not_an_empty_history() {
        // The caller has a different, more specific problem than "this session said nothing", and
        // the panel's fallback notice has to be able to say which.
        let error = read_transcript_history(Path::new("/nonexistent/definitely-not-here.jsonl"));
        assert!(error.is_err());
    }

    #[test]
    fn a_whole_small_file_is_read_and_its_omitted_count_is_known() {
        let dir = temp_dir("small");
        let path = dir.join("session.jsonl");
        std::fs::write(&path, format!("{}\n{}\n", typed_prompt("one"), typed_prompt("two"))).unwrap();
        let history = read_transcript_history(&path).unwrap();
        assert_eq!(prompts(&history), vec!["one".to_string(), "two".to_string()]);
        assert_eq!(history.omitted_items, Some(0));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// One padding line of the tail-scan fixture: valid JSON that contributes no item, so anything
    /// in the result came from the tail.
    fn filler_line(pad: usize) -> String {
        format!(
            r#"{{"type":"attachment","attachment":{{"pad":"{}"}}}}"#,
            "p".repeat(pad)
        )
    }

    /// That line's length on disk, newline included.
    fn filler_len(pad: usize) -> usize {
        filler_line(pad).len() + 1
    }

    /// The tail scan. A file past [`HISTORY_SCAN_MAX_BYTES`] is read from
    /// `len - HISTORY_SCAN_MAX_BYTES`, the partial line at that offset is dropped, and
    /// `omitted_items` becomes `None` -- because the number of records in the part that was never
    /// read is genuinely unknown, and reporting the part that was seen would be a precise-looking
    /// count of the wrong thing. One of this machine's 44 sessions is past the limit.
    #[test]
    fn a_file_past_the_scan_limit_is_read_from_its_tail_and_says_the_count_is_unknown() {
        let dir = temp_dir("tail");
        let path = dir.join("big.jsonl");
        {
            use std::io::Write;
            let file = std::fs::File::create(&path).unwrap();
            let mut writer = std::io::BufWriter::with_capacity(1 << 20, file);
            // Padding that is valid JSON but contributes nothing, so anything that shows up in the
            // result came from the tail.
            let filler = filler_line(8192);
            let mut written = 0u64;
            while written < HISTORY_SCAN_MAX_BYTES + (1 << 20) {
                writeln!(writer, "{filler}").unwrap();
                written += filler.len() as u64 + 1;
            }
            writeln!(writer, "{}", typed_prompt("inside the tail")).unwrap();
            writer.flush().unwrap();
        }
        let len = std::fs::metadata(&path).unwrap().len();
        assert!(len > HISTORY_SCAN_MAX_BYTES);
        // Every filler line is the same length, so this arithmetic settles whether the seek lands
        // inside one. It has to, or the assertion below about the discarded partial line is
        // vacuous -- and it would go vacuous silently if someone changed the padding's size.
        let filler_line = filler_len(8192) as u64;
        assert_ne!(
            (len - HISTORY_SCAN_MAX_BYTES) % filler_line,
            0,
            "the seek must land inside a line for this test to mean anything"
        );

        let history = read_transcript_history(&path).unwrap();
        assert_eq!(prompts(&history), vec!["inside the tail".to_string()]);
        assert_eq!(history.omitted_items, None);
        // The partial line the seek cut in half is DISCARDED, not counted as malformed: it is an
        // artefact of where this function chose to start, not of the file. Without this assertion
        // deleting that discard leaves the whole suite green -- the half line is simply tallied as
        // malformed instead, and nothing else here looks at the tally.
        assert_eq!(
            history.counts.malformed_lines, 0,
            "a file the CLI wrote correctly has no malformed lines, however this function seeks"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---------------------------------------------------------------------------------------
    // Real lines
    // ---------------------------------------------------------------------------------------

    /// Twelve real lines, redacted; see `agent/tests/fixtures/transcript_lines.README.md`.
    ///
    /// What synthetic lines cannot prove and this does: that the envelope the CLI actually writes
    /// -- its field names, its nesting, its flags -- reaches the destinations this module's table
    /// assigns them. Every synthetic line above was written by the same person who wrote the
    /// parser, so agreement between them proves only self-consistency.
    #[test]
    fn the_redacted_real_lines_land_where_the_mapping_table_says() {
        let raw = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/transcript_lines.jsonl"
        ))
        .expect("the fixture is checked in");
        let history = fold(&raw);

        // Two prompts survive (one `typed`, one `sdk`); the third `sdk` prompt in the file carries
        // an editor-context block and is the one the strip has to handle.
        let prompts = prompts(&history);
        assert_eq!(prompts.len(), 3, "{prompts:?}");
        assert!(
            prompts.iter().all(|p| !p.contains("The user opened the file")),
            "an editor-context block survived into a prompt: {prompts:?}"
        );

        // The tool call and its result paired inside the file.
        let started = history
            .events
            .iter()
            .filter(|e| matches!(e, AgentDomainEvent::ToolCallStarted { .. }))
            .count();
        let completed = history
            .events
            .iter()
            .filter(|e| matches!(e, AgentDomainEvent::ToolCallCompleted { .. }))
            .count();
        assert_eq!((started, completed), (1, 1));
        assert_eq!(history.counts.orphan_tool_results, 0);

        // The compaction summary was refused, its paired boundary line was dropped as a `system`
        // line, and neither reached the events.
        assert_eq!(history.counts.rejected_user_lines, 2, "the summary and the isMeta line");
        assert_eq!(history.counts.system_lines, 1);
        let rendered = format!("{:?}", history.events);
        assert!(!rendered.contains("This session is being continued"));

        assert_eq!(history.counts.attachment_lines, 1);
        assert_eq!(history.counts.side_record_lines, 2, "ai-title and file-history-delta");
        assert_eq!(history.counts.dropped_blocks, 1, "the thinking block");
        assert_eq!(history.writer_version.as_deref(), Some("2.1.272"));
    }

    /// Runs the reader over every real top-level session file under `$CLAUDE_CONFIG_DIR/projects`
    /// on this machine. `#[ignore]`d because it needs that corpus to exist; it reads files and
    /// bills nothing.
    ///
    /// **What this proves that no synthetic or fixture line can.** The fixtures are twelve lines
    /// somebody chose; this is every line the CLI has actually written here (44 sessions, ~46,000
    /// lines, 130 MB as of 2026-09-20), including whatever shapes nobody thought to look for. Four
    /// properties, none of which a hand-made input can establish:
    ///
    /// 1. **No real file makes the reader fail.** Not one `Err`, on any of them.
    /// 2. **The bounds hold against real sizes**, including the one 47 MB session that is the only
    ///    file on this machine past `HISTORY_SCAN_MAX_BYTES`.
    /// 3. **The allow-list is not vacuous on real data.** Real prompts come back -- the failure
    ///    this asserts against is a parser that is merely *safe*, refusing everything, which every
    ///    negative test above would happily pass.
    /// 4. **Nothing in the corpus produces an unmatched completion or a surviving context block.**
    ///
    /// It deliberately asserts only shapes and counts. Nothing here reads, prints or compares
    /// conversation content.
    #[test]
    #[ignore = "needs this machine's real $CLAUDE_CONFIG_DIR/projects corpus"]
    fn every_real_session_file_on_this_machine_parses_within_its_bounds() {
        let files = real_session_files();

        let mut sessions_with_a_prompt = 0usize;
        let mut sessions_with_items = 0usize;
        for path in &files {
            let history = read_transcript_history(path).unwrap_or_else(|error| {
                panic!("{} failed to parse: {error}", path.display());
            });

            assert!(
                history.parsed_items <= HISTORY_MAX_ITEMS,
                "{} restored {} items",
                path.display(),
                history.parsed_items
            );

            let mut prompt_count = 0usize;
            let mut started: Vec<&str> = Vec::new();
            for event in &history.events {
                match event {
                    AgentDomainEvent::UserPromptSubmitted { text } => {
                        prompt_count += 1;
                        assert_eq!(
                            strip_composed_block(text).len(),
                            text.len(),
                            "{}: a composed editor-context block survived into a prompt",
                            path.display()
                        );
                    }
                    AgentDomainEvent::ToolCallStarted { tool_use_id, .. } => {
                        assert!(!tool_use_id.is_empty());
                        started.push(tool_use_id);
                    }
                    AgentDomainEvent::ToolCallCompleted { tool_use_id, .. } => {
                        assert!(
                            started.contains(&tool_use_id.as_str()),
                            "{}: a completion with no call in the loaded window",
                            path.display()
                        );
                    }
                    AgentDomainEvent::ContentDelta { .. } => {}
                    other => panic!("{}: history produced {other:?}", path.display()),
                }
            }
            if prompt_count > 0 {
                sessions_with_a_prompt += 1;
            }
            if history.parsed_items > 0 {
                sessions_with_items += 1;
            }

            let counts = history.counts;
            assert_eq!(
                counts.skipped_lines,
                counts.malformed_lines
                    + counts.oversized_lines
                    + counts.attachment_lines
                    + counts.system_lines
                    + counts.side_record_lines
                    + counts.subagent_only_lines
                    + counts.unknown_type_lines
                    + counts.sidechain_lines
                    + counts.rejected_user_lines
                    + counts.empty_prompt_lines,
                "{}: the skipped-line categories do not sum to the total",
                path.display()
            );
        }

        assert!(
            sessions_with_items * 2 >= files.len(),
            "only {sessions_with_items} of {} real sessions produced any history at all",
            files.len()
        );
        assert!(
            sessions_with_a_prompt * 2 >= files.len(),
            "only {sessions_with_a_prompt} of {} real sessions produced a single user prompt -- the \
             allow-list is refusing real data",
            files.len()
        );
    }
}
