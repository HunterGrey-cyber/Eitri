//! The CLI's session files, read into the normal form.
//!
//! The rules, each from lines the pinned CLI (2.1.311) was recorded writing:
//!
//! - **A turn opens at a `user` line** whose content is a string or text blocks (not only `tool_result`
//!   blocks) and that is neither `isMeta` nor `isCompactSummary`. Its origin is `origin.kind` when the line
//!   has one, `task_notification` when the content is a `<task-notification>` envelope, `human` when
//!   `promptSource` is one a person's words carry, else `unknown`. A slash command's echo
//!   (`<command-name>`) opens a turn whose user text is the command as typed.
//! - **A turn runs to the next opener or the end of the file.** The CLI writes one `assistant` line per
//!   content block and fills `stop_reason` on each, so a turn's last block is known only by what follows
//!   it; `turn_duration` is not written in SDK sessions, and one that is followed by anything but an
//!   opener is reported as an inconsistency.
//! - **A message taken into a running turn is an `attachment` line of type `queued_command`**, an entry of
//!   that turn: a notification when its origin is a task notification, a peer's when its origin is a peer,
//!   a steer when it is a plain prompt the queue absorbed mid-turn (`queue-operation` `remove` with
//!   `absorbed_mid_turn`, which follows the attachment), queued otherwise, and `meta` when the CLI marked
//!   it as nobody's words.
//! - `assistant` lines of one `message.id` are one message; a block written twice (the same block index
//!   and content) is read once and counted as unrecorded.
//! - `[Request interrupted by user]` is the turn's interruption (another text with that start is too, and
//!   is counted as unrecorded); `compact_boundary` of trigger `manual` is
//!   a compaction, placed in the turn of the `/compact` command that follows it, and one of trigger `auto`
//!   is a compaction entry of the turn that was running, at the point the boundary was written (before the
//!   continued reply, or between a tool result and the next call).
//! - A subagent's transcript (`<session>/subagents/agent-<id>.jsonl`, named by its `.meta.json`) is nested
//!   under the Agent call that started it.
//!
//! Every other line is skipped and counted. A line of a kind the pinned CLI was never recorded writing is
//! never decoded, and is counted as unrecorded as well, so a new shape shows up in the counts instead of
//! being misread. Shapes of a recorded kind that no recording holds yet (the result of a tool call cut short
//! to deliver a message) are counted as unrecorded and give no entry of their own.

use super::{CliForm, Entry, Inconsistency, InputKind, Origin, Pos, Turn, Via};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::io::{self, BufRead, BufReader};
use std::path::{Path, PathBuf};

/// Line kinds the pinned CLI was recorded writing, in session and subagent files.
pub const RECORDED_LINE_KINDS: &[&str] = &[
    "api-request",
    "api-request-blob",
    "api-request-shape",
    "assistant",
    "atis-latch",
    "attachment/agent_listing_delta",
    "attachment/auto_mode",
    "attachment/date",
    "attachment/deferred_tools_delta",
    "attachment/deferred_tools_record",
    "attachment/environment",
    "attachment/model",
    "attachment/prompt_snapshot",
    "attachment/queued_command",
    "attachment/remote_session_change",
    "attachment/session_context",
    "attachment/silent_turn_reminder",
    "attachment/skill_listing",
    "attachment/total_tokens_reminder",
    "cost-state",
    "last-prompt",
    "mode",
    "queue-operation/dequeue",
    "queue-operation/enqueue",
    "queue-operation/remove",
    "system/compact_boundary",
    "system/local_command",
    "tools-used",
    "user",
];

/// `promptSource` values a person's own words carry; `sdk` is every prompt a host such as Eitri sends.
pub const HUMAN_PROMPT_SOURCES: &[&str] = &["typed", "suggestion_accepted", "queued", "sdk"];

/// The text the CLI writes as a user line when a reply was interrupted.
pub const INTERRUPTED_TEXT: &str = "[Request interrupted by user]";

/// How the result of a tool call the CLI cut short to deliver a message begins.
pub const ENDED_TO_DELIVER_PREFIX: &str = "[Tool call did not complete: the turn was ended to deliver";

/// Reads a chain of session files, oldest first, and the subagent transcripts in `subagents` if given.
pub fn read_chain(sessions: &[PathBuf], subagents: Option<&Path>) -> io::Result<CliForm> {
    let mut form = CliForm::default();
    let mut reader = Reader::new(false);
    for path in sessions {
        let input = form.inputs.len();
        form.inputs.push((InputKind::Session, path.clone()));
        let lines = read_lines(path)?;
        reader.file(&mut form, input, &lines);
    }
    form.turns = reader.finish();
    if let Some(dir) = subagents {
        read_subagents(&mut form, dir)?;
    }
    Ok(form)
}

/// Reads session files given as text, oldest first, with no subagents.
pub fn read_texts(sessions: &[&str]) -> CliForm {
    let mut form = CliForm::default();
    let mut reader = Reader::new(false);
    for (input, text) in sessions.iter().enumerate() {
        form.inputs
            .push((InputKind::Session, PathBuf::from(format!("#{}", input + 1))));
        let lines: Vec<String> = text.lines().map(str::to_owned).collect();
        reader.file(&mut form, input, &lines);
    }
    form.turns = reader.finish();
    form
}

fn read_lines(path: &Path) -> io::Result<Vec<String>> {
    BufReader::new(std::fs::File::open(path)?).lines().collect()
}

/// A line's kind: `type`, and `/subtype`, `/attachment.type` or `/operation` where it has one.
pub fn line_kind(line: &Value) -> String {
    let kind = line.get("type").and_then(Value::as_str).unwrap_or("(no type)");
    let sub = match kind {
        "system" => line.get("subtype").and_then(Value::as_str),
        "attachment" => line.pointer("/attachment/type").and_then(Value::as_str),
        "queue-operation" => line.get("operation").and_then(Value::as_str),
        _ => None,
    };
    match sub {
        Some(sub) => format!("{kind}/{sub}"),
        None => kind.to_owned(),
    }
}

fn flag(line: &Value, key: &str) -> bool {
    line.get(key).and_then(Value::as_bool) == Some(true)
}

fn bump(map: &mut std::collections::BTreeMap<String, usize>, key: impl Into<String>) {
    *map.entry(key.into()).or_default() += 1;
}

/// Reads session or subagent lines into turns, keeping the running turn across the files of a chain.
struct Reader {
    /// A subagent transcript, whose lines all carry `isSidechain: true`.
    sidechain_file: bool,
    turns: Vec<Turn>,
    current: Option<Turn>,
    /// A manual compaction waiting for the `/compact` command line that follows its boundary.
    pending_compaction: Option<(Entry, Pos)>,
    /// A `turn_duration` line whose next conversational line has not been seen.
    pending_turn_duration: Option<Pos>,
    /// Blocks already read per assistant `message.id`.
    seen_blocks: HashMap<String, HashSet<String>>,
}

impl Reader {
    fn new(sidechain_file: bool) -> Self {
        Self {
            sidechain_file,
            turns: Vec::new(),
            current: None,
            pending_compaction: None,
            pending_turn_duration: None,
            seen_blocks: HashMap::new(),
        }
    }

    fn file(&mut self, form: &mut CliForm, input: usize, lines: &[String]) {
        let parsed: Vec<Option<Value>> = lines
            .iter()
            .map(|line| {
                if line.trim().is_empty() {
                    return None;
                }
                serde_json::from_str::<Value>(line).ok().filter(Value::is_object)
            })
            .collect();
        // The queue says which plain prompts it absorbed into a running turn only after the attachment
        // that carries the prompt, so the whole file is read for it first.
        let absorbed: HashSet<String> = parsed
            .iter()
            .flatten()
            .filter(|line| {
                line.get("type").and_then(Value::as_str) == Some("queue-operation")
                    && line.get("operation").and_then(Value::as_str) == Some("remove")
                    && line.get("reason").and_then(Value::as_str) == Some("absorbed_mid_turn")
            })
            .filter_map(|line| line.get("commandUuid").and_then(Value::as_str).map(str::to_owned))
            .collect();
        for (index, (raw, line)) in lines.iter().zip(&parsed).enumerate() {
            let pos = Pos { input, line: index + 1 };
            if raw.trim().is_empty() {
                continue;
            }
            form.lines_read += 1;
            match line {
                Some(line) => self.line(form, line, pos, &absorbed),
                None => {
                    bump(&mut form.skipped, "(malformed)");
                    form.inconsistencies.push(Inconsistency::Malformed { pos });
                }
            }
        }
    }

    fn line(&mut self, form: &mut CliForm, line: &Value, pos: Pos, absorbed: &HashSet<String>) {
        let kind = line_kind(line);
        bump(&mut form.kinds_read, kind.clone());
        if kind == "system/turn_duration" {
            self.pending_turn_duration = Some(pos);
        }
        if !RECORDED_LINE_KINDS.contains(&kind.as_str()) {
            bump(&mut form.skipped, kind.clone());
            bump(&mut form.unrecorded, kind);
            return;
        }
        if line
            .get("isSidechain")
            .and_then(Value::as_bool)
            .is_some_and(|side| side != self.sidechain_file)
        {
            // A session file was recorded with no subagent line in it, and a subagent file with no line
            // of the session: a line on the wrong side is not read as either.
            bump(&mut form.skipped, format!("{kind}:isSidechain"));
            bump(&mut form.unrecorded, format!("{kind}:isSidechain"));
            return;
        }
        match kind.as_str() {
            "user" => self.user_line(form, line, pos),
            "assistant" => self.assistant_line(form, line, pos),
            "attachment/queued_command" => self.queued_command(form, line, pos, absorbed),
            "system/compact_boundary" => self.compact_boundary(form, line, pos),
            _ => bump(&mut form.skipped, kind),
        }
    }

    fn user_line(&mut self, form: &mut CliForm, line: &Value, pos: Pos) {
        if flag(line, "isMeta") {
            bump(&mut form.skipped, "user:isMeta");
            return;
        }
        if flag(line, "isCompactSummary") {
            bump(&mut form.skipped, "user:isCompactSummary");
            return;
        }
        let content = line.pointer("/message/content");
        let mut text: Option<String> = None;
        let mut results = Vec::new();
        match content {
            Some(Value::String(s)) => text = Some(s.clone()),
            Some(Value::Array(blocks)) => {
                for block in blocks {
                    match block.get("type").and_then(Value::as_str) {
                        Some("text") => {
                            let piece = block.get("text").and_then(Value::as_str).unwrap_or_default();
                            text.get_or_insert_with(String::new).push_str(piece);
                        }
                        Some("tool_result") => results.push(block),
                        other => {
                            let key = format!("user:block/{}", other.unwrap_or("(no type)"));
                            bump(&mut form.skipped, key.clone());
                            bump(&mut form.unrecorded, key);
                        }
                    }
                }
            }
            _ => {}
        }
        if !results.is_empty() && text.is_some() {
            bump(&mut form.unrecorded, "user:text_and_tool_result");
        }
        for block in results {
            self.tool_result(form, block, pos);
        }
        let Some(text) = text else {
            return;
        };
        if text.starts_with(INTERRUPTED_TEXT.trim_end_matches(']')) {
            // Only the plain form was recorded; another wording of it (an interruption during a tool
            // call) is still the end of the turn, never a new turn, and is counted.
            if text != INTERRUPTED_TEXT {
                bump(&mut form.unrecorded, "user:interrupted/variant");
            }
            self.mid_turn(form, Entry::Interrupted, pos);
            return;
        }
        if text.starts_with("<local-command-stdout>") || text.starts_with("<local-command-stderr>") {
            bump(&mut form.skipped, "user:local-command-output");
            return;
        }
        let command = command_text(&text);
        let origin = self.origin(form, line, &text);
        let via = match origin {
            Origin::TaskNotification | Origin::Scheduled => Via::TaskNotification,
            Origin::Peer => Via::Peer,
            _ => Via::Prompt,
        };
        let is_command = command.is_some();
        self.open(
            Entry::User {
                text: command.unwrap_or(text),
                via,
            },
            origin,
            pos,
            is_command,
        );
    }

    fn origin(&self, form: &mut CliForm, line: &Value, text: &str) -> Origin {
        if let Some(kind) = line.pointer("/origin/kind").and_then(Value::as_str) {
            return match kind {
                "task-notification" => {
                    if line.pointer("/origin/subkind").and_then(Value::as_str) == Some("scheduled-trigger") {
                        bump(&mut form.unrecorded, "user:origin/task-notification/scheduled-trigger");
                        Origin::Scheduled
                    } else {
                        Origin::TaskNotification
                    }
                }
                other => {
                    bump(&mut form.unrecorded, format!("user:origin/{other}"));
                    match other {
                        "human" => Origin::Human,
                        "peer" => Origin::Peer,
                        _ => Origin::Unknown,
                    }
                }
            };
        }
        if text.starts_with("<task-notification>") {
            return Origin::TaskNotification;
        }
        match line.get("promptSource").and_then(Value::as_str) {
            Some(source) if HUMAN_PROMPT_SOURCES.contains(&source) => Origin::Human,
            _ => Origin::Unknown,
        }
    }

    fn tool_result(&mut self, form: &mut CliForm, block: &Value, pos: Pos) {
        let id = block
            .get("tool_use_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let content = block.get("content").cloned().unwrap_or(Value::Null);
        if super::normalise::result_text(&content).starts_with(ENDED_TO_DELIVER_PREFIX) {
            bump(&mut form.unrecorded, "user:tool_result/ended_to_deliver");
        }
        let is_error = block.get("is_error").and_then(Value::as_bool).unwrap_or(false);
        self.mid_turn(form, Entry::ToolResult { id, is_error, content }, pos);
    }

    fn assistant_line(&mut self, form: &mut CliForm, line: &Value, pos: Pos) {
        // The CLI answers an interrupted turn it finds unfinished when it resumes a session (`No response
        // requested.`) with a line of its own model name, not the model's words: no record of a live
        // session holds it, so it is not a reply to compare. An API error it writes under the same name
        // (`isApiErrorMessage`) is shown to the person as the turn's reply, and every record holds it, so
        // it is read like any reply.
        let synthetic = line.pointer("/message/model").and_then(Value::as_str) == Some("<synthetic>");
        let api_error = line.get("isApiErrorMessage").and_then(Value::as_bool).unwrap_or(false);
        if synthetic && !api_error {
            bump(&mut form.skipped, "assistant:synthetic");
            return;
        }
        let message_id = line
            .pointer("/message/id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let Some(Value::Array(blocks)) = line.pointer("/message/content") else {
            bump(&mut form.skipped, "assistant:no-content");
            return;
        };
        let mut produced = false;
        // A block is known by its place in the message: the line's `apiBlockIndex` (or, without one, the
        // line's own uuid) and its place on the line. Two blocks of one message with the same words are
        // two blocks; the same block written again is read once.
        let block_index = line.get("apiBlockIndex").and_then(Value::as_u64);
        let line_uuid = line.get("uuid").and_then(Value::as_str).unwrap_or_default();
        for (place, block) in blocks.iter().enumerate() {
            let identity = match block_index {
                Some(index) => format!("{index}/{place}/{block}"),
                None => format!("{line_uuid}/{place}/{block}"),
            };
            if !message_id.is_empty() && !self.seen_blocks.entry(message_id.clone()).or_default().insert(identity) {
                // No recording holds a block written twice.
                bump(&mut form.skipped, "assistant:block-repeated");
                bump(&mut form.unrecorded, "assistant:block-repeated");
                continue;
            }
            let entry = match block.get("type").and_then(Value::as_str) {
                Some("text") => Entry::Assistant {
                    text: block.get("text").and_then(Value::as_str).unwrap_or_default().to_owned(),
                },
                Some("tool_use") => Entry::ToolUse {
                    id: block.get("id").and_then(Value::as_str).unwrap_or_default().to_owned(),
                    name: block.get("name").and_then(Value::as_str).unwrap_or_default().to_owned(),
                    input: block.get("input").cloned().unwrap_or(Value::Null),
                },
                Some(kind @ ("thinking" | "redacted_thinking")) => {
                    bump(&mut form.unrecorded, format!("assistant:block/{kind}"));
                    Entry::Thinking {
                        text: block
                            .get("thinking")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned(),
                    }
                }
                other => {
                    let key = format!("assistant:block/{}", other.unwrap_or("(no type)"));
                    bump(&mut form.skipped, key.clone());
                    bump(&mut form.unrecorded, key);
                    continue;
                }
            };
            self.mid_turn(form, entry, pos);
            produced = true;
        }
        if !produced && blocks.is_empty() {
            bump(&mut form.skipped, "assistant:no-content");
        }
    }

    fn queued_command(&mut self, form: &mut CliForm, line: &Value, pos: Pos, absorbed: &HashSet<String>) {
        let attachment = &line["attachment"];
        let text = match attachment.get("prompt") {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Array(blocks)) => blocks
                .iter()
                .filter_map(|b| b.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join(""),
            _ => String::new(),
        };
        let mode = attachment.get("commandMode").and_then(Value::as_str).unwrap_or("");
        let origin = attachment.pointer("/origin/kind").and_then(Value::as_str);
        let meta = flag(attachment, "isMeta");
        let task_delivery = attachment.get("taskDelivery").is_some();
        let recorded = matches!(
            (mode, origin, meta, task_delivery),
            ("prompt", None, false, false)
                | ("prompt", Some("peer"), true, false)
                | ("task-notification", Some("task-notification"), false, false)
        );
        if !recorded {
            bump(
                &mut form.unrecorded,
                format!(
                    "attachment/queued_command:{mode}/{}{}{}",
                    origin.unwrap_or("-"),
                    if meta { "/isMeta" } else { "" },
                    if task_delivery { "/taskDelivery" } else { "" }
                ),
            );
        }
        let via = if origin == Some("task-notification") || task_delivery || mode == "task-notification" {
            Via::TaskNotification
        } else if origin == Some("peer") {
            Via::Peer
        } else if meta {
            Via::Meta
        } else if origin.is_some() {
            Via::Unknown
        } else if mode == "prompt" {
            let source = attachment
                .get("source_uuid")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if absorbed.contains(source) {
                Via::Steer
            } else {
                // The recorded queued prompt opened a turn of its own; one delivered as an attachment the
                // queue did not absorb mid-turn is a shape no recording holds.
                bump(&mut form.unrecorded, "attachment/queued_command:prompt/-/queued");
                Via::Queued
            }
        } else {
            Via::Unknown
        };
        self.mid_turn(form, Entry::User { text, via }, pos);
    }

    fn compact_boundary(&mut self, form: &mut CliForm, line: &Value, pos: Pos) {
        let trigger = line
            .pointer("/compactMetadata/trigger")
            .and_then(Value::as_str)
            .unwrap_or("");
        if trigger == "auto" {
            // The CLI compacts while it runs a turn, so the boundary falls inside that turn.
            self.mid_turn(
                form,
                Entry::Compaction {
                    trigger: trigger.to_owned(),
                },
                pos,
            );
            return;
        }
        if trigger != "manual" {
            let key = format!("system/compact_boundary:{trigger}");
            bump(&mut form.skipped, key.clone());
            bump(&mut form.unrecorded, key);
            return;
        }
        self.flush_compaction();
        self.pending_compaction = Some((
            Entry::Compaction {
                trigger: trigger.to_owned(),
            },
            pos,
        ));
    }

    /// Opens a turn; a pending compaction goes into it when the opener is the command that ran it, and
    /// into the turn before otherwise.
    fn open(&mut self, entry: Entry, origin: Origin, pos: Pos, is_command: bool) {
        self.pending_turn_duration = None;
        let compaction = if is_command {
            self.pending_compaction.take()
        } else {
            None
        };
        self.flush_compaction();
        if let Some(turn) = self.current.take() {
            self.turns.push(turn);
        }
        let mut turn = Turn::new(self.turns.len() + 1, origin);
        turn.push(entry, pos);
        if let Some((entry, pos)) = compaction {
            turn.push(entry, pos);
        }
        self.current = Some(turn);
    }

    /// Adds an entry to the running turn.
    fn mid_turn(&mut self, form: &mut CliForm, entry: Entry, pos: Pos) {
        if let Some(at) = self.pending_turn_duration.take() {
            form.inconsistencies
                .push(Inconsistency::TurnDurationMidTurn { pos: at });
        }
        self.flush_compaction();
        if self.current.is_none() {
            form.inconsistencies.push(Inconsistency::NoOpener { pos });
            self.current = Some(Turn::new(self.turns.len() + 1, Origin::Unknown));
        }
        self.current.as_mut().expect("set above").push(entry, pos);
    }

    fn flush_compaction(&mut self) {
        if let Some((entry, pos)) = self.pending_compaction.take() {
            match self.current.as_mut() {
                Some(turn) => turn.push(entry, pos),
                None => {
                    let mut turn = Turn::new(self.turns.len() + 1, Origin::Unknown);
                    turn.push(entry, pos);
                    self.current = Some(turn);
                }
            }
        }
    }

    fn finish(mut self) -> Vec<Turn> {
        self.flush_compaction();
        if let Some(turn) = self.current.take() {
            self.turns.push(turn);
        }
        self.turns
    }
}

/// A slash command's echo (`<command-name>/x</command-name> … <command-args>y</command-args>`) as typed:
/// `/x y`.
fn command_text(text: &str) -> Option<String> {
    let rest = text.strip_prefix("<command-name>")?;
    let name = &rest[..rest.find("</command-name>")?];
    let args = text
        .find("<command-args>")
        .and_then(|start| {
            let after = &text[start + "<command-args>".len()..];
            after.find("</command-args>").map(|end| after[..end].trim())
        })
        .unwrap_or("");
    Some(if args.is_empty() {
        name.to_owned()
    } else {
        format!("{name} {args}")
    })
}

fn read_subagents(form: &mut CliForm, dir: &Path) -> io::Result<()> {
    let mut found: Vec<(u64, String, usize, Vec<Turn>)> = Vec::new();
    let mut names: Vec<PathBuf> = std::fs::read_dir(dir)?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| {
            path.extension().is_some_and(|ext| ext == "jsonl")
                && path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("agent-"))
        })
        .collect();
    names.sort();
    for path in names {
        let input = form.inputs.len();
        form.inputs.push((InputKind::Subagent, path.clone()));
        let meta: Option<Value> = std::fs::read_to_string(path.with_extension("meta.json"))
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok());
        let parent = meta
            .as_ref()
            .and_then(|m| m.get("toolUseId"))
            .and_then(Value::as_str)
            .map(str::to_owned);
        let depth = meta
            .as_ref()
            .and_then(|m| m.get("spawnDepth"))
            .and_then(Value::as_u64)
            .unwrap_or(1);
        let mut reader = Reader::new(true);
        let lines = read_lines(&path)?;
        reader.file(form, input, &lines);
        let turns = reader.finish();
        match parent {
            Some(parent) => found.push((depth, parent, input, turns)),
            None => form.inconsistencies.push(Inconsistency::SubagentWithoutMeta { input }),
        }
    }
    // Parents before the subagents they started, so a nested Agent call is already in place.
    found.sort_by_key(|(depth, _, input, _)| (*depth, *input));
    for (_, parent, input, turns) in found {
        let entry = Entry::Subagent {
            parent_tool_use_id: parent.clone(),
            turns,
        };
        // The subagent sits where its transcript starts.
        if !nest(&mut form.turns, &parent, entry, Pos { input, line: 1 }) {
            form.inconsistencies.push(Inconsistency::SubagentWithoutParent {
                tool_use_id: parent,
                input,
            });
        }
    }
    Ok(())
}

/// Puts `entry` right after the Agent call `parent`, searching nested subagents too.
fn nest(turns: &mut [Turn], parent: &str, entry: Entry, pos: Pos) -> bool {
    for turn in turns.iter_mut() {
        if let Some(at) = turn
            .entries
            .iter()
            .position(|e| matches!(e, Entry::ToolUse { id, .. } if id == parent))
        {
            turn.entries.insert(at + 1, entry);
            turn.positions.insert(at + 1, pos);
            return true;
        }
        for existing in turn.entries.iter_mut() {
            if let Entry::Subagent { turns, .. } = existing {
                if nest(turns, parent, entry.clone(), pos) {
                    return true;
                }
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(form: &CliForm) -> Vec<Vec<&'static str>> {
        form.turns
            .iter()
            .map(|t| t.entries.iter().map(Entry::kind).collect())
            .collect()
    }

    #[test]
    fn a_reply_the_cli_writes_under_its_own_model_name_is_skipped_and_counted() {
        let text = [
            r#"{"type":"user","isSidechain":false,"promptSource":"sdk","message":{"role":"user","content":"go"}}"#,
            r#"{"type":"assistant","isSidechain":false,"message":{"id":"m1","content":[{"type":"text","text":"x"}]}}"#,
            r#"{"type":"user","isSidechain":false,"message":{"role":"user","content":[{"type":"text","text":"[Request interrupted by user]"}]}}"#,
            r#"{"type":"assistant","isSidechain":false,"message":{"id":"m2","model":"<synthetic>","content":[{"type":"text","text":"No response requested."}]}}"#,
        ]
        .join("\n");
        let form = read_texts(&[&text]);
        assert_eq!(kinds(&form), vec![vec!["user", "assistant", "interrupted"]]);
        assert_eq!(form.skipped.get("assistant:synthetic"), Some(&1));
        assert!(form.unrecorded.is_empty(), "{:?}", form.unrecorded);
    }

    #[test]
    fn an_api_error_the_cli_writes_under_its_own_model_name_is_read_as_a_reply() {
        let text = [
            r#"{"type":"user","isSidechain":false,"promptSource":"sdk","message":{"role":"user","content":"go"}}"#,
            r#"{"type":"assistant","isSidechain":false,"isApiErrorMessage":true,"message":{"id":"m1","model":"<synthetic>","content":[{"type":"text","text":"API Error: Request rejected (429)"}]}}"#,
        ]
        .join("\n");
        let form = read_texts(&[&text]);
        assert_eq!(kinds(&form), vec![vec!["user", "assistant"]]);
        assert!(
            matches!(&form.turns[0].entries[1], Entry::Assistant { text } if text == "API Error: Request rejected (429)"),
            "{:?}",
            form.turns[0].entries
        );
        assert_eq!(form.skipped.get("assistant:synthetic"), None);
    }

    #[test]
    fn a_turn_duration_inside_a_turn_is_an_inconsistency_and_an_unrecorded_kind() {
        let text = [
            r#"{"type":"user","isSidechain":false,"promptSource":"sdk","message":{"role":"user","content":"go"}}"#,
            r#"{"type":"system","isSidechain":false,"subtype":"turn_duration","durationMs":5}"#,
            r#"{"type":"assistant","isSidechain":false,"message":{"id":"m1","content":[{"type":"text","text":"x"}]}}"#,
        ]
        .join("\n");
        let form = read_texts(&[&text]);
        assert_eq!(kinds(&form), vec![vec!["user", "assistant"]]);
        assert_eq!(form.unrecorded.get("system/turn_duration"), Some(&1));
        assert_eq!(form.skipped.get("system/turn_duration"), Some(&1));
        assert_eq!(
            form.inconsistencies,
            vec![Inconsistency::TurnDurationMidTurn {
                pos: Pos { input: 0, line: 2 }
            }]
        );
    }

    #[test]
    fn a_turn_duration_before_an_opener_or_the_end_is_no_inconsistency() {
        let text = [
            r#"{"type":"user","isSidechain":false,"promptSource":"sdk","message":{"role":"user","content":"go"}}"#,
            r#"{"type":"system","isSidechain":false,"subtype":"turn_duration","durationMs":5}"#,
            r#"{"type":"user","isSidechain":false,"promptSource":"sdk","message":{"role":"user","content":"again"}}"#,
            r#"{"type":"system","isSidechain":false,"subtype":"turn_duration","durationMs":5}"#,
        ]
        .join("\n");
        let form = read_texts(&[&text]);
        assert_eq!(form.turns.len(), 2);
        assert!(form.inconsistencies.is_empty(), "{:?}", form.inconsistencies);
    }

    #[test]
    fn a_line_of_an_unrecorded_kind_is_skipped_and_counted_never_decoded() {
        let text = [
            r#"{"type":"user","isSidechain":false,"promptSource":"sdk","message":{"role":"user","content":"go"}}"#,
            r#"{"type":"file-history-snapshot","snapshot":{}}"#,
            r#"{"type":"brand-new-kind","message":{"content":[{"type":"text","text":"not a reply"}]}}"#,
            "not json",
        ]
        .join("\n");
        let form = read_texts(&[&text]);
        assert_eq!(kinds(&form), vec![vec!["user"]]);
        assert_eq!(form.unrecorded.get("file-history-snapshot"), Some(&1));
        assert_eq!(form.unrecorded.get("brand-new-kind"), Some(&1));
        assert_eq!(form.skipped.get("(malformed)"), Some(&1));
        assert_eq!(form.lines_read, 4);
    }

    #[test]
    fn a_block_written_twice_under_one_message_id_is_read_once() {
        let text = [
            r#"{"type":"user","isSidechain":false,"promptSource":"sdk","message":{"role":"user","content":"go"}}"#,
            r#"{"type":"assistant","isSidechain":false,"apiBlockIndex":0,"uuid":"a","message":{"id":"m1","content":[{"type":"text","text":"x"}]}}"#,
            r#"{"type":"assistant","isSidechain":false,"apiBlockIndex":0,"uuid":"b","message":{"id":"m1","content":[{"type":"text","text":"x"}]}}"#,
        ]
        .join("\n");
        let form = read_texts(&[&text]);
        assert_eq!(kinds(&form), vec![vec!["user", "assistant"]]);
        assert_eq!(form.skipped.get("assistant:block-repeated"), Some(&1));
        assert_eq!(form.unrecorded.get("assistant:block-repeated"), Some(&1));
    }

    #[test]
    fn two_blocks_of_one_message_with_the_same_words_are_two_blocks() {
        let text = [
            r#"{"type":"user","isSidechain":false,"promptSource":"sdk","message":{"role":"user","content":"go"}}"#,
            r#"{"type":"assistant","isSidechain":false,"apiBlockIndex":0,"uuid":"a","message":{"id":"m1","content":[{"type":"text","text":"x"}]}}"#,
            r#"{"type":"assistant","isSidechain":false,"apiBlockIndex":1,"uuid":"b","message":{"id":"m1","content":[{"type":"text","text":"x"}]}}"#,
            r#"{"type":"assistant","isSidechain":false,"uuid":"c","message":{"id":"m2","content":[{"type":"text","text":"y"},{"type":"text","text":"y"}]}}"#,
        ]
        .join("\n");
        let form = read_texts(&[&text]);
        assert_eq!(
            kinds(&form),
            vec![vec!["user", "assistant", "assistant", "assistant", "assistant"]]
        );
        assert!(form.unrecorded.is_empty(), "{:?}", form.unrecorded);
    }

    #[test]
    fn an_interruption_in_other_words_ends_the_turn_and_is_counted() {
        let text = [
            r#"{"type":"user","isSidechain":false,"promptSource":"sdk","message":{"role":"user","content":"go"}}"#,
            r#"{"type":"user","isSidechain":false,"message":{"role":"user","content":[{"type":"text","text":"[Request interrupted by user for tool use]"}]}}"#,
        ]
        .join("\n");
        let form = read_texts(&[&text]);
        assert_eq!(kinds(&form), vec![vec!["user", "interrupted"]]);
        assert_eq!(form.unrecorded.get("user:interrupted/variant"), Some(&1));
    }

    #[test]
    fn a_queued_prompt_the_queue_did_not_absorb_is_an_unrecorded_shape() {
        let attachment = |uuid: &str| {
            format!(
                r#"{{"type":"attachment","isSidechain":false,"attachment":{{"type":"queued_command","commandMode":"prompt","prompt":"later","source_uuid":"{uuid}"}}}}"#
            )
        };
        let opener =
            r#"{"type":"user","isSidechain":false,"promptSource":"sdk","message":{"role":"user","content":"go"}}"#;
        let absorbed =
            r#"{"type":"queue-operation","operation":"remove","reason":"absorbed_mid_turn","commandUuid":"s1"}"#;
        let steer = read_texts(&[&[opener, &attachment("s1"), absorbed].join("\n")]);
        assert!(matches!(
            &steer.turns[0].entries[1],
            Entry::User { via: Via::Steer, .. }
        ));
        assert!(steer.unrecorded.is_empty(), "{:?}", steer.unrecorded);
        let queued = read_texts(&[&[opener, &attachment("s2")].join("\n")]);
        assert!(matches!(
            &queued.turns[0].entries[1],
            Entry::User { via: Via::Queued, .. }
        ));
        assert_eq!(
            queued.unrecorded.get("attachment/queued_command:prompt/-/queued"),
            Some(&1)
        );
    }

    #[test]
    fn a_command_echo_reads_as_the_command_typed() {
        assert_eq!(
            command_text(
                "<command-name>/compact</command-name>\n  <command-message>compact</command-message>\n  <command-args>keep it short</command-args>"
            )
            .as_deref(),
            Some("/compact keep it short")
        );
        assert_eq!(command_text("plain words"), None);
    }
}
