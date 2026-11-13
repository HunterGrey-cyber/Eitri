//! Private, rotating copies of unloaded turns. Callers must handle every write failure before pruning memory.

use crate::private_fs::{append_private, PrivateLines};
use crate::projection::*;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

pub const SEGMENT_BYTES: u64 = 16 << 20;
pub const MAX_SEGMENTS: usize = 4;
pub const PAGE_MAX_CHARS: usize = super::HISTORY_MAX_CHARS;
/// Private archive records contain metadata only, so base64 transcript sizing does not apply.
pub(crate) const ARCHIVE_MAX_LINE_BYTES: usize = 4 * 1024 * 1024;
const CUT_KEEP_BYTES: usize = 4 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchiveId {
    pub conversation_id: String,
    pub provider_session_id: String,
    pub generation: String,
}

impl ArchiveId {
    pub fn new(conversation_id: &str, provider_session_id: &str) -> Result<Self, String> {
        let id = Self {
            conversation_id: conversation_id.into(),
            provider_session_id: provider_session_id.into(),
            generation: uuid::Uuid::new_v4().simple().to_string()[..16].into(),
        };
        id.validate().map_err(|err| err.to_string())?;
        Ok(id)
    }

    fn validate(&self) -> io::Result<()> {
        crate::persistence::validate_path_component("conversation_id", &self.conversation_id)?;
        crate::persistence::validate_path_component("provider_session_id", &self.provider_session_id)?;
        if self.generation.len() != 16
            || !self
                .generation
                .bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "archive generation must be 16 lowercase hex digits",
            ));
        }
        Ok(())
    }

    fn path(&self, dir: &Path, k: u64) -> PathBuf {
        dir.join(format!(
            "{}.evicted-{}.{k}.jsonl",
            self.provider_session_id, self.generation
        ))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArchivedTurn {
    pub turn: u64,
    pub records: Vec<ArchivedRecord>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "record", rename_all = "snake_case")]
pub enum ArchivedRecord {
    UserPrompt(UserPromptRecord),
    Transcript(TranscriptMessage),
    Thinking(ThinkingBlock),
    ToolCall(ToolCallRecord),
    TurnStart(TurnStartRecord),
    TurnEnding(TurnEndingRecord),
    Notice(NoticeRecord),
    Question(QuestionRecord),
    Plan(PlanRecord),
}

#[derive(Serialize, Deserialize)]
struct Line {
    v: u8,
    turn: u64,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    cut: bool,
    #[serde(flatten)]
    record: ArchivedRecord,
}

/// Parses only complete archive basenames, including the session id rather than the archive stem.
pub(super) fn archive_name(name: &str) -> Option<(&str, &str, u64)> {
    let (sid, suffix) = name.strip_suffix(".jsonl")?.rsplit_once(".evicted-")?;
    let (generation, k) = suffix.split_once('.')?;
    if crate::persistence::validate_path_component("provider_session_id", sid).is_err()
        || generation.len() != 16
        || !generation
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
        || k.is_empty()
        || !k.bytes().all(|c| c.is_ascii_digit())
    {
        return None;
    }
    Some((sid, generation, k.parse().ok()?))
}

fn segments(dir: &Path, id: &ArchiveId) -> io::Result<Vec<(u64, PathBuf)>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err),
    };
    let mut found = Vec::new();
    for entry in entries {
        let entry = entry?;
        if let Some((sid, generation, k)) = entry.file_name().to_str().and_then(archive_name) {
            if sid == id.provider_session_id && generation == id.generation {
                found.push((k, entry.path()));
            }
        }
    }
    found.sort_by_key(|(k, _)| *k);
    Ok(found)
}

#[cfg(test)]
thread_local! {
    static BEFORE_REMOVE: std::cell::RefCell<Option<Box<dyn FnOnce(&Path)>>> = const { std::cell::RefCell::new(None) };
}

fn remove_if_present(path: &Path) -> io::Result<()> {
    #[cfg(test)]
    BEFORE_REMOVE.with(|hook| {
        if let Some(hook) = hook.borrow_mut().take() {
            hook(path);
        }
    });
    match std::fs::remove_file(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}

pub struct ArchiveWriter {
    dir: PathBuf,
    id: ArchiveId,
    k: u64,
    file: File,
    bytes: u64,
    rollback_to: Option<(u64, u64, u64)>,
    recovered_rollback: Option<u64>,
    holds_lease: bool,
    #[cfg(test)]
    pub(crate) write_fault: Option<(usize, bool)>,
    #[cfg(test)]
    pub(crate) fail_rollback: bool,
}
impl ArchiveWriter {
    pub fn open(history_dir: &Path, id: &ArchiveId, holds_lease: bool) -> io::Result<Self> {
        id.validate()?;
        let dir = history_dir.join(&id.conversation_id);
        crate::private_fs::create_private_dir_all(&dir, history_dir)?;
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            if let Some((sid, generation, _)) = entry.file_name().to_str().and_then(archive_name) {
                if holds_lease && sid == id.provider_session_id && generation != id.generation {
                    remove_if_present(&entry.path())?;
                }
            }
        }
        let found = segments(&dir, id)?;
        let k = found.last().map_or(0, |(k, _)| *k);
        let path = id.path(&dir, k);
        let file = append_private(&path)?;
        let cancel = AtomicBool::new(false);
        let mut reader = PrivateLines::open(&path, ARCHIVE_MAX_LINE_BYTES, &cancel)?.into_file();
        let mut end = file.metadata()?.len();
        let mut buf = [0; 8192];
        // Scan backwards in fixed blocks, retaining only bytes through the last complete line.
        let bytes = loop {
            if end == 0 {
                break 0;
            }
            let start = end.saturating_sub(buf.len() as u64);
            reader.seek(SeekFrom::Start(start))?;
            let len = (end - start) as usize;
            reader.read_exact(&mut buf[..len])?;
            if let Some(pos) = buf[..len].iter().rposition(|b| *b == b'\n') {
                break start + pos as u64 + 1;
            }
            end = start;
        };
        file.set_len(bytes)?;
        let writer = Self {
            dir,
            id: id.clone(),
            k,
            file,
            bytes,
            rollback_to: None,
            recovered_rollback: None,
            holds_lease,
            #[cfg(test)]
            write_fault: None,
            #[cfg(test)]
            fail_rollback: false,
        };
        writer.trim(None)?;
        Ok(writer)
    }

    /// A failed chunk may still have partial records on disk until rollback succeeds.
    pub fn is_poisoned(&self) -> bool {
        self.rollback_to.is_some()
    }

    /// Earliest turn whose poisoned chunk this writer removed, beyond merely repairing a torn final line.
    pub(crate) fn recovered_rollback(&self) -> Option<u64> {
        self.recovered_rollback
    }

    fn trim(&self, rollback_segment: Option<u64>) -> io::Result<()> {
        let found = segments(&self.dir, &self.id)?;
        let mut obsolete: Vec<_> = found.iter().take(found.len().saturating_sub(MAX_SEGMENTS)).collect();
        // Keep the rollback segment until the last fallible removal, so an error can undo the chunk.
        obsolete.sort_by_key(|(k, _)| Some(*k) == rollback_segment);
        for (_, path) in obsolete {
            remove_if_present(&path)?;
        }
        Ok(())
    }

    /// Errors, including sync or rotation errors, reach the caller so it can defer unloading turns.
    pub fn append(&mut self, turns: &[ArchivedTurn]) -> io::Result<()> {
        if let Some((k, bytes, turn)) = self.rollback_to {
            self.rollback((k, bytes)).map_err(|err| {
                io::Error::new(err.kind(), format!("archive rollback incomplete; reopen writer: {err}"))
            })?;
            let recovered = self.recovered_rollback.map_or(turn, |old| old.min(turn));
            // Reopening also repairs any torn tail before a fresh chunk can append.
            *self = Self::open(
                self.dir.parent().expect("history directory"),
                &self.id,
                self.holds_lease,
            )?;
            self.recovered_rollback = Some(recovered);
        }
        let start = (self.k, self.bytes);
        let result = (|| {
            for turn in turns {
                for record in &turn.records {
                    let mut line = encode(turn.turn, record)?;
                    if self.bytes >= SEGMENT_BYTES || self.bytes + line.len() as u64 + 1 > SEGMENT_BYTES {
                        self.file.sync_all()?;
                        let k = self
                            .k
                            .checked_add(1)
                            .ok_or_else(|| io::Error::other("archive segment overflow"))?;
                        let file = append_private(&self.id.path(&self.dir, k))?;
                        self.k = k;
                        self.file = file;
                        self.bytes = self.file.metadata()?.len();
                    }
                    line.push(b'\n');
                    // A short append is a failure, never a second write interleaved with another record.
                    match self.write_line(&line) {
                        Ok(written) if written == line.len() => self.bytes += written as u64,
                        result => {
                            return Err(result
                                .err()
                                .unwrap_or_else(|| io::Error::new(io::ErrorKind::WriteZero, "short archive append")));
                        }
                    }
                }
            }
            self.file.sync_data()?;
            self.trim(Some(start.0))
        })();
        if result.is_err() && self.rollback(start).is_err() {
            // Preserve the write error and forbid appending until the whole failed chunk is removed.
            self.rollback_to = Some((start.0, start.1, turns.first().map_or(u64::MAX, |turn| turn.turn)));
        }
        result
    }

    fn rollback(&mut self, (k, bytes): (u64, u64)) -> io::Result<()> {
        if self.k != k {
            self.file = append_private(&self.id.path(&self.dir, k))?;
            self.k = k;
        }
        self.truncate(bytes)?;
        self.bytes = bytes;
        for (segment, path) in segments(&self.dir, &self.id)? {
            if segment > k {
                remove_if_present(&path)?;
            }
        }
        self.file.sync_data()
    }

    fn write_line(&mut self, line: &[u8]) -> io::Result<usize> {
        #[cfg(test)]
        if let Some((remaining, short)) = self.write_fault.as_mut() {
            *remaining -= 1;
            if *remaining == 0 {
                let short = *short;
                self.write_fault = None;
                return if short {
                    self.file.write(&line[..line.len() / 2])
                } else {
                    Err(io::Error::other("injected archive write failure"))
                };
            }
        }
        self.file.write(line)
    }

    fn truncate(&mut self, bytes: u64) -> io::Result<()> {
        #[cfg(test)]
        if std::mem::take(&mut self.fail_rollback) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "injected rollback failure",
            ));
        }
        self.file.set_len(bytes)
    }
}

fn keep_head(text: &mut String, cap: usize) {
    if text.len() <= cap {
        return;
    }
    let mut end = cap;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    text.push_str("… [cut]");
}

fn cut(value: &mut serde_json::Value, cap: usize) {
    use serde_json::Value;
    match value {
        Value::String(text) => keep_head(text, cap),
        Value::Array(items) => {
            items.truncate(cap);
            for item in items {
                cut(item, cap);
            }
        }
        Value::Object(fields) => {
            for (key, value) in fields.iter_mut() {
                if matches!(key.as_str(), "kind" | "origin" | "level" | "outcome") {
                    continue;
                }
                if matches!(key.as_str(), "input" | "content") && !value.is_null() {
                    let mut text = match &*value {
                        Value::String(s) => s.clone(),
                        other => other.to_string(),
                    };
                    if text.len() > cap {
                        keep_head(&mut text, cap);
                        *value = Value::String(text);
                    }
                } else if key == "scalars" && value.to_string().len() > cap {
                    let mut text = value.to_string();
                    keep_head(&mut text, cap);
                    *value = serde_json::json!({"cut": text});
                } else if key == "children" && value.to_string().len() > cap {
                    *value = Value::Null;
                } else {
                    cut(value, cap);
                }
            }
        }
        _ => {}
    }
}

fn encode(turn: u64, record: &ArchivedRecord) -> io::Result<Vec<u8>> {
    let line = Line {
        v: 1,
        turn,
        cut: false,
        record: record.clone(),
    };
    let bytes = serde_json::to_vec(&line)?;
    if bytes.len() <= ARCHIVE_MAX_LINE_BYTES {
        return Ok(bytes);
    }
    let mut value = serde_json::to_value(line)?;
    value["cut"] = true.into();
    let mut cap = CUT_KEEP_BYTES;
    loop {
        cut(&mut value["record"], cap);
        let bytes = serde_json::to_vec(&value)?;
        if bytes.len() <= ARCHIVE_MAX_LINE_BYTES {
            return Ok(bytes);
        }
        if cap == 1 {
            return Ok(serde_json::to_vec(&Line {
                v: 1,
                turn,
                cut: true,
                record: minimal_record(record),
            })?);
        }
        cap = (cap / 2).max(1);
    }
}

// Metadata maps and optional payloads can outweigh every text head; retain the record's identity.
fn minimal_record(record: &ArchivedRecord) -> ArchivedRecord {
    let mut record = record.clone();
    match &mut record {
        ArchivedRecord::UserPrompt(r) => {
            r.text.clear();
            r.delivered = false;
        }
        ArchivedRecord::Transcript(r) => r.text.clear(),
        ArchivedRecord::Thinking(r) => r.text.clear(),
        ArchivedRecord::ToolCall(r) => {
            r.children = None;
            r.background = false;
            r.name.clear();
            r.input = serde_json::Value::Null;
            r.result = None;
            r.denied = None;
            r.changed_input = None;
        }
        ArchivedRecord::TurnStart(r) => r.cause = None,
        ArchivedRecord::TurnEnding(r) => {
            r.detail = TurnEndDetail::default();
            r.resets_at = None;
        }
        ArchivedRecord::Notice(r) => {
            r.content = None;
            r.scalars.clear();
            r.task_id = None;
            r.tool_use_id = None;
            r.task_status = None;
        }
        ArchivedRecord::Question(r) => {
            r.tool_use_id = None;
            r.turn_id = None;
            r.questions.clear();
            r.unsupported_reason = None;
            if let QuestionState::Settled { answers, reason, .. } = &mut r.state {
                answers.clear();
                *reason = None;
            }
        }
        ArchivedRecord::Plan(r) => {
            r.tool_use_id = None;
            r.turn_id = None;
            r.plan.clear();
            r.plan_sha256.clear();
            r.allowed_prompts.clear();
            r.unsupported_reason = None;
            if let PlanState::Settled { feedback, reason, .. } = &mut r.state {
                *feedback = None;
                *reason = None;
            }
        }
    }
    record
}

#[derive(Debug, PartialEq)]
pub struct Page {
    pub from_turn: u64,
    pub turns: Vec<ArchivedTurn>,
    /// Last-version records whose payloads were cut, keyed by (turn, kind, seq).
    pub cut: Vec<(u64, &'static str, u64)>,
}

#[cfg(test)]
thread_local! {
    static AFTER_READ_LINE: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = std::cell::RefCell::new(None);
    static RETAINED_RECORDS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static SEGMENT_OPENS: std::cell::RefCell<BTreeMap<PathBuf, usize>> = const { std::cell::RefCell::new(BTreeMap::new()) };
}

pub fn read_page(
    history_dir: &Path,
    id: &ArchiveId,
    before_turn: u64,
    max_turns: usize,
    max_chars: usize,
    cancel: &AtomicBool,
) -> io::Result<Option<Page>> {
    id.validate()?;
    #[cfg(test)]
    RETAINED_RECORDS.with(|count| count.set(0));
    #[cfg(test)]
    SEGMENT_OPENS.with(|counts| counts.borrow_mut().clear());
    let files = segments(&history_dir.join(&id.conversation_id), id)?;
    let window_start = before_turn.saturating_sub(max_turns.max(1) as u64);
    let mut costs = BTreeMap::<u64, BTreeMap<RecordKey, usize>>::new();
    // Retried records can arrive out of order and shrink. Keep only their last character costs
    // inside the requested window, then collect payloads only for the turns that fit.
    let partial = scan_records(&files, window_start..before_turn, cancel, |turn, key, record, _cut| {
        costs.entry(turn).or_default().insert(key, record_chars(&record));
    })?;
    // Rotation may have removed the beginning of the oldest surviving segment's lowest turn.
    if let Some(turn) = partial {
        costs.remove(&turn);
    }
    let mut chars = 0usize;
    let mut from_turn = before_turn;
    for turn in (window_start..before_turn).rev() {
        if cancel.load(Ordering::Acquire) {
            return Ok(None);
        }
        let Some(records) = costs.get(&turn) else {
            break;
        };
        let cost = records.values().fold(0usize, |sum, cost| sum.saturating_add(*cost));
        if from_turn != before_turn && chars.saturating_add(cost) > max_chars.min(PAGE_MAX_CHARS) {
            break;
        }
        chars = chars.saturating_add(cost);
        from_turn = turn;
    }
    drop(costs);
    if cancel.load(Ordering::Acquire) || from_turn == before_turn {
        return Ok(None);
    }
    let mut turns = BTreeMap::<u64, BTreeMap<RecordKey, ArchivedRecord>>::new();
    let mut cut = BTreeSet::new();
    scan_records(&files, from_turn..before_turn, cancel, |turn, key, record, is_cut| {
        let cut_key = (turn, key.1, key.0);
        if is_cut {
            cut.insert(cut_key);
        } else {
            cut.remove(&cut_key);
        }
        retain_record(turns.entry(turn).or_default(), key, record)
    })?;
    if cancel.load(Ordering::Acquire) {
        return Ok(None);
    }
    Ok(Some(Page {
        from_turn,
        cut: cut.into_iter().collect(),
        turns: turns
            .into_iter()
            .map(|(turn, records)| ArchivedTurn {
                turn,
                records: records.into_values().collect(),
            })
            .collect(),
    }))
}

type RecordKey = (u64, &'static str);

fn retain_record(records: &mut BTreeMap<RecordKey, ArchivedRecord>, key: RecordKey, record: ArchivedRecord) {
    #[cfg(test)]
    RETAINED_RECORDS.with(|count| count.set(count.get() + 1));
    records.insert(key, record);
}

fn scan_records(
    files: &[(u64, PathBuf)],
    turns: std::ops::Range<u64>,
    cancel: &AtomicBool,
    mut visit: impl FnMut(u64, RecordKey, ArchivedRecord, bool),
) -> io::Result<Option<u64>> {
    let oldest = files.first().map(|(k, _)| *k);
    let mut partial = None;
    for (k, path) in files {
        if cancel.load(Ordering::Acquire) {
            break;
        }
        let lines = PrivateLines::open(path, ARCHIVE_MAX_LINE_BYTES, cancel)?;
        #[cfg(test)]
        SEGMENT_OPENS.with(|counts| *counts.borrow_mut().entry(path.clone()).or_default() += 1);
        for bytes in lines {
            #[cfg(test)]
            AFTER_READ_LINE.with(|hook| {
                if let Some(hook) = hook.borrow_mut().take() {
                    hook();
                }
            });
            if cancel.load(Ordering::Acquire) {
                return Ok(partial);
            }
            let Ok(line) = serde_json::from_slice::<Line>(&bytes?) else {
                continue;
            };
            if line.v != 1 {
                continue;
            }
            if Some(*k) == oldest && *k != 0 {
                partial = Some(partial.map_or(line.turn, |turn: u64| turn.min(line.turn)));
            }
            if !turns.contains(&line.turn) {
                continue;
            }
            let key = match &line.record {
                ArchivedRecord::UserPrompt(r) => (r.seq, "user_prompt"),
                ArchivedRecord::Transcript(r) => (r.seq, "transcript"),
                ArchivedRecord::Thinking(r) => (r.seq, "thinking"),
                ArchivedRecord::ToolCall(r) => (r.seq, "tool_call"),
                ArchivedRecord::TurnStart(r) => (r.seq, "turn_start"),
                ArchivedRecord::TurnEnding(r) => (r.seq, "turn_ending"),
                ArchivedRecord::Notice(r) => (r.seq, "notice"),
                ArchivedRecord::Question(r) => (r.seq, "question"),
                ArchivedRecord::Plan(r) => (r.seq, "plan"),
            };
            visit(line.turn, key, line.record, line.cut);
        }
    }
    Ok(partial)
}

fn record_chars(record: &ArchivedRecord) -> usize {
    use super::store;
    match record {
        ArchivedRecord::UserPrompt(r) => r.text.chars().count(),
        ArchivedRecord::Transcript(r) => r.text.chars().count(),
        ArchivedRecord::Thinking(r) => r.text.chars().count(),
        ArchivedRecord::ToolCall(r) => store::tool_call_chars(r),
        ArchivedRecord::TurnStart(r) => store::turn_start_chars(r),
        ArchivedRecord::TurnEnding(r) => [&r.detail.origin_kind, &r.detail.reason, &r.detail.message]
            .into_iter()
            .flatten()
            .map(|s| s.chars().count())
            .sum(),
        ArchivedRecord::Notice(r) => store::notice_chars(r),
        ArchivedRecord::Question(r) => store::question_chars(r),
        ArchivedRecord::Plan(r) => store::plan_chars(r),
    }
}

#[cfg(test)]
mod tests;
