//! How much of a long conversation an observer is sent at once: a snapshot holds the last turns that fit under a byte
//! ceiling, older turns come as pages on request, and an entry too large for any snapshot is sent cut there and comes
//! whole in a page.
//!
//! A turn, here, is the stretch of the conversation from one prompt (the user's, or a turn the CLI started by itself)
//! to the next. Every entry of the projection has a `seq`, one order over all its collections; turn `i` holds the
//! entries from the `i`th prompt's `seq` up to the next prompt's, and turn 0 also holds whatever came before the first
//! prompt (a conversation with no prompt is one turn). Prompts and CLI turn starts are only ever appended while a
//! conversation lasts, so a turn's number names the same entries later and a page asked for by number fits against the
//! snapshot without a gap; a new session (a new backend generation) numbers its own turns, and so does the conversation
//! a `/clear` starts in the same session (its observers are owed a snapshot of it first).
//!
//! Sizes are counted as the JSON will carry them, escapes included, so a text of quotes, backslashes or newlines is not
//! taken for half its size.
//!
//! What a snapshot or a page leaves out is cut from a copy of the projection, never from the projection itself, and
//! the copy is made only when something must go: a conversation of at most [`SNAPSHOT_TURNS`] turns that fits under
//! the ceiling is serialized from the live view, as the desk's snapshot is.

use agent::AgentSessionProjection;
use serde_json::Value;

use crate::agent_bridge::{CallNotes, TurnTime};
use crate::remote_rv::{json_escaped_len, CutEntry};

/// The turns a snapshot holds at most.
pub const SNAPSHOT_TURNS: usize = 100;

/// The bytes a snapshot's envelope stays under, as long as the last turn alone fits once its largest entries are cut.
pub const SNAPSHOT_CEILING_BYTES: usize = 1 << 20;

/// The turns a page holds at most.
pub const MORE_PAGE_TURNS: usize = 100;

/// How much of a cut entry's text a snapshot keeps, so the phone can show its start.
pub const CUT_KEEP_BYTES: usize = 4 * 1024;

/// What the session's state beside its turns (status, capabilities, cards, the envelope) is allowed of a ceiling.
const STATE_ALLOWANCE: usize = 64 * 1024;

/// What one cut entry costs a snapshot: its kept start and its marker.
const CUT_COST: usize = CUT_KEEP_BYTES + 256;

/// The collections a turn's entries are in, named as the desk's envelope names them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Kind {
    Prompt,
    Message,
    Call,
    Thought,
    Notice,
    Ending,
    Start,
    Question,
}

impl Kind {
    fn collection(self) -> &'static str {
        match self {
            Kind::Prompt => "userPrompts",
            Kind::Message => "transcript",
            Kind::Call => "toolCalls",
            Kind::Thought => "thinking",
            Kind::Notice => "notices",
            Kind::Ending => "turnEndings",
            Kind::Start => "turnStarts",
            Kind::Question => "questions",
        }
    }

    /// Whether an entry of this kind can be sent cut: its bulk is text or a payload the agent wrote.
    fn cuttable(self) -> bool {
        matches!(
            self,
            Kind::Prompt | Kind::Message | Kind::Call | Kind::Thought | Kind::Notice
        )
    }
}

/// One entry of the conversation: where it is and about how many bytes it takes in the envelope.
#[derive(Debug, Clone, Copy)]
struct Entry {
    kind: Kind,
    seq: u64,
    bytes: usize,
}

/// Where each turn of a session starts.
pub(super) struct Turns {
    /// `starts[0]` is 0; each later one is a prompt's `seq`, strictly increasing. Never empty.
    starts: Vec<u64>,
}

impl Turns {
    pub(super) fn of(projection: &AgentSessionProjection) -> Turns {
        let mut starts: Vec<u64> = projection
            .user_prompts
            .iter()
            .map(|p| p.seq)
            .chain(projection.turn_starts.iter().map(|s| s.seq))
            .collect();
        starts.sort_unstable();
        starts.dedup();
        match starts.first_mut() {
            Some(first) => *first = 0,
            None => starts.push(0),
        }
        Turns { starts }
    }

    /// How many turns the session has; always at least one.
    pub(super) fn count(&self) -> u64 {
        self.starts.len() as u64
    }

    /// The `seq` range of turns `from..to` (`to` excluded).
    fn seqs(&self, from: usize, to: usize) -> (u64, u64) {
        let lo = self.starts[from];
        let hi = self.starts.get(to).copied().unwrap_or(u64::MAX);
        (lo, hi)
    }

    fn turn_of(&self, seq: u64) -> usize {
        self.starts.partition_point(|start| *start <= seq).saturating_sub(1)
    }
}

/// About how many bytes `value` takes as JSON, without serializing it.
fn value_bytes(value: &Value) -> usize {
    match value {
        Value::Null | Value::Bool(_) => 5,
        Value::Number(_) => 20,
        Value::String(s) => text_bytes(s),
        Value::Array(items) => 2 + items.iter().map(|v| value_bytes(v) + 1).sum::<usize>(),
        Value::Object(map) => {
            2 + map
                .iter()
                .map(|(k, v)| text_bytes(k) + 2 + value_bytes(v))
                .sum::<usize>()
        }
    }
}

/// How many bytes `text` takes as a JSON string, its quotes and escapes counted.
fn text_bytes(text: &str) -> usize {
    json_escaped_len(text) + 2
}

/// How many bytes `item` takes as JSON, counted while it serializes into nothing.
fn serialized_bytes(item: &impl serde::Serialize) -> usize {
    struct Count(usize);
    impl std::io::Write for Count {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0 += buf.len();
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut count = Count(0);
    let _ = serde_json::to_writer(&mut count, item);
    count.0
}

fn call_bytes(call: &agent::ToolCallRecord) -> usize {
    150 + text_bytes(&call.name)
        + text_bytes(&call.turn_id)
        + text_bytes(&call.tool_use_id)
        + value_bytes(&call.input)
        + call.result.as_ref().map_or(5, |r| 30 + value_bytes(&r.content))
        + call.children.as_ref().map_or(0, serialized_bytes)
}

/// The entries whose `seq` is in `lo..hi`, with their sizes.
fn entries(projection: &AgentSessionProjection, lo: u64, hi: u64) -> Vec<Entry> {
    let within = |seq: u64| (lo..hi).contains(&seq);
    let mut out = Vec::new();
    let mut add = |kind, seq, bytes: &dyn Fn() -> usize| {
        if within(seq) {
            out.push(Entry {
                kind,
                seq,
                bytes: bytes(),
            });
        }
    };
    for p in &projection.user_prompts {
        add(Kind::Prompt, p.seq, &|| text_bytes(&p.text) + 40);
    }
    for m in &projection.transcript {
        add(Kind::Message, m.seq, &|| text_bytes(&m.text) + 30);
    }
    for c in &projection.tool_calls {
        add(Kind::Call, c.seq, &|| call_bytes(c));
    }
    for t in &projection.thinking {
        add(Kind::Thought, t.seq, &|| {
            text_bytes(&t.text) + text_bytes(&t.turn_id) + 50
        });
    }
    for n in &projection.notices {
        add(Kind::Notice, n.seq, &|| serialized_bytes(n) + 40);
    }
    for e in &projection.turn_endings {
        add(Kind::Ending, e.seq, &|| serialized_bytes(e) + 40);
    }
    for s in &projection.turn_starts {
        add(Kind::Start, s.seq, &|| serialized_bytes(s) + 40);
    }
    for q in &projection.questions {
        add(Kind::Question, q.seq, &|| serialized_bytes(q) + 40);
    }
    out
}

/// What a snapshot holds: turns `first_turn..` from `seq` `from_seq` on, with the entries in `cut` sent cut.
#[derive(Clone)]
pub(super) struct Tail {
    pub(super) first_turn: usize,
    from_seq: u64,
    cut: Vec<Entry>,
}

impl Tail {
    /// Whether the snapshot is the whole conversation, serialized as it stands.
    pub(super) fn is_whole(&self) -> bool {
        self.first_turn == 0 && self.cut.is_empty()
    }

    /// The cut entries as the frame names them.
    pub(super) fn cut_entries(&self) -> Vec<CutEntry> {
        self.cut
            .iter()
            .map(|e| CutEntry {
                entry_id: format!("{}:{}", e.kind.collection(), e.seq),
                bytes: e.bytes as u64,
            })
            .collect()
    }
}

/// The turns a snapshot of at most `ceiling` bytes starts from: the last [`SNAPSHOT_TURNS`] at most, then the earliest
/// whole turn that keeps their entries under the ceiling. An entry larger than the ceiling is cut; when the last turn
/// alone does not fit, its largest entries are cut until it does (or none is left to cut).
pub(super) fn choose_tail(projection: &AgentSessionProjection, turns: &Turns, ceiling: usize) -> Tail {
    let count = turns.starts.len();
    let earliest = count.saturating_sub(SNAPSHOT_TURNS);
    let (lo, _) = turns.seqs(earliest, count);
    let budget = ceiling.saturating_sub(STATE_ALLOWANCE);
    let mut all = entries(projection, lo, u64::MAX);
    let mut cut: Vec<Entry> = Vec::new();
    let mut sizes = vec![0usize; count - earliest];
    for entry in &all {
        let turn = turns.turn_of(entry.seq);
        let bytes = if entry.kind.cuttable() && entry.bytes > budget {
            cut.push(*entry);
            CUT_COST
        } else {
            entry.bytes
        };
        sizes[turn - earliest] += bytes;
    }
    let mut first = count - 1;
    let mut used = sizes[first - earliest];
    while first > earliest && used + sizes[first - 1 - earliest] <= budget {
        first -= 1;
        used += sizes[first - earliest];
    }
    if used > budget {
        // The last turn alone is over the ceiling: its largest entries go cut until it fits.
        let (last_lo, _) = turns.seqs(first, count);
        all.retain(|e| e.seq >= last_lo && e.kind.cuttable() && e.bytes > CUT_COST);
        all.sort_by(|a, b| b.bytes.cmp(&a.bytes));
        for entry in all {
            if used <= budget {
                break;
            }
            if cut.iter().any(|c| c.kind == entry.kind && c.seq == entry.seq) {
                continue;
            }
            used = used - entry.bytes + CUT_COST;
            cut.push(entry);
        }
    }
    let from_seq = turns.seqs(first, count).0;
    cut.retain(|e| e.seq >= from_seq);
    cut.sort_by_key(|e| (e.seq, e.kind));
    Tail {
        first_turn: first,
        from_seq: if first == 0 { 0 } else { from_seq },
        cut,
    }
}

/// The same tail with `more` of its earliest turns dropped too (at least one turn is kept).
pub(super) fn shorter(tail: Tail, turns: &Turns, more: usize) -> Tail {
    let count = turns.starts.len();
    let first = (tail.first_turn + more).min(count - 1);
    let from_seq = turns.seqs(first, count).0;
    let mut cut = tail.cut;
    cut.retain(|e| e.seq >= from_seq);
    Tail {
        first_turn: first,
        from_seq,
        cut,
    }
}

/// The same tail with more of its entries cut, the largest first, until about `over` bytes are saved; `None` when no
/// entry it holds whole is left to cut. Read from `projection` (or a copy of it holding the tail), whose entries the
/// tail does not cut yet are whole there.
pub(super) fn cut_largest(mut tail: Tail, projection: &AgentSessionProjection, over: usize) -> Option<Tail> {
    let mut whole: Vec<Entry> = entries(projection, tail.from_seq, u64::MAX)
        .into_iter()
        .filter(|e| e.kind.cuttable() && e.bytes > CUT_COST)
        .filter(|e| !tail.cut.iter().any(|c| c.kind == e.kind && c.seq == e.seq))
        .collect();
    if whole.is_empty() {
        return None;
    }
    whole.sort_by(|a, b| b.bytes.cmp(&a.bytes));
    let mut saved = 0;
    for entry in whole {
        if saved >= over {
            break;
        }
        saved += entry.bytes - CUT_COST;
        tail.cut.push(entry);
    }
    tail.cut.sort_by_key(|e| (e.seq, e.kind));
    Some(tail)
}

/// The turns a page answering `more` before `before_turn` holds: back from `before_turn - 1`, at most
/// [`MORE_PAGE_TURNS`], whole, while their entries stay under `limit`; always the one turn before `before_turn`, however
/// large. `None` when `before_turn` names no turn.
pub(super) fn choose_page(
    projection: &AgentSessionProjection,
    turns: &Turns,
    before_turn: u64,
    limit: usize,
) -> Option<(usize, usize)> {
    let count = turns.starts.len();
    let before = usize::try_from(before_turn).ok().filter(|b| (1..=count).contains(b))?;
    let earliest = before.saturating_sub(MORE_PAGE_TURNS);
    let (lo, hi) = turns.seqs(earliest, before);
    let mut sizes = vec![0usize; before - earliest];
    for entry in entries(projection, lo, hi) {
        sizes[turns.turn_of(entry.seq) - earliest] += entry.bytes;
    }
    let budget = limit.saturating_sub(STATE_ALLOWANCE);
    let mut from = before - 1;
    let mut used = sizes[from - earliest];
    while from > earliest && used + sizes[from - 1 - earliest] <= budget {
        from -= 1;
        used += sizes[from - earliest];
    }
    Some((from, before - 1))
}

/// The start of `text` within [`CUT_KEEP_BYTES`], cut between characters.
fn keep_start(text: &mut String) {
    if text.len() <= CUT_KEEP_BYTES {
        return;
    }
    let mut end = CUT_KEEP_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
}

/// A payload cut to its start: a string keeps its own start, anything else the start of its JSON, as a string.
fn cut_value(value: &mut Value) {
    if value_bytes(value) <= CUT_KEEP_BYTES {
        return;
    }
    let mut text = match &mut *value {
        Value::String(s) => std::mem::take(s),
        other => other.to_string(),
    };
    keep_start(&mut text);
    *value = Value::String(text);
}

/// The entries of `items` with a `seq` in `lo..hi`, cloned one by one.
fn kept<T: Clone>(items: &[T], seq: impl Fn(&T) -> u64, lo: u64, hi: u64) -> Vec<T> {
    items.iter().filter(|e| (lo..hi).contains(&seq(e))).cloned().collect()
}

/// A copy of `projection` holding only the entries with a `seq` in `lo..hi`: the conversation outside that range is
/// never cloned, so a copy costs what it keeps, not the whole transcript. The live cards are the session's state, not
/// one turn's: a snapshot (`keep_cards`) keeps them all. Every field is named, without a rest pattern, so a field added
/// to the projection does not compile here until it says whether it is the conversation's (kept by `seq`) or the
/// session's (cloned).
fn copy_between(projection: &AgentSessionProjection, lo: u64, hi: u64, keep_cards: bool) -> AgentSessionProjection {
    let AgentSessionProjection {
        session_id,
        provider_session_id,
        model,
        cwd,
        status,
        active_turn_id,
        user_prompts,
        transcript,
        tool_calls,
        pending_permissions,
        turn_endings,
        last_turn_ending,
        turn_starts,
        notices,
        api_retry,
        compacting,
        rate_limit_warning,
        fallback_model,
        effort,
        cli_left_auto,
        notification,
        background_tasks,
        questions,
        last_turn_end_seq,
        usage,
        assistant_message_open,
        thinking,
        thinking_open,
        last_revision,
        history,
        ungated_cli_mode,
        cli_mode_reported,
        policy_mismatch,
        account_limits,
        background_calls,
        available_commands,
        background_at_last_turn_end,
        denials_before_their_call,
        gate_inputs,
        changed_inputs_before_their_call,
        // Positions in the whole `tool_calls`, which the copy does not hold; nothing serialized reads it.
        call_index: _,
        open_client_calls,
        init_servers,
        steer_outcomes,
        move_seeded,
        cli_unseen_busy,
        prompt_before_turn,
        running_turn_prompt_is_slash,
        running_turn_said_text,
        opening_prompt,
        running_turn_cli,
    } = projection;
    AgentSessionProjection {
        session_id: session_id.clone(),
        provider_session_id: provider_session_id.clone(),
        model: model.clone(),
        cwd: cwd.clone(),
        status: status.clone(),
        active_turn_id: active_turn_id.clone(),
        user_prompts: kept(user_prompts, |e| e.seq, lo, hi),
        transcript: kept(transcript, |e| e.seq, lo, hi),
        tool_calls: kept(tool_calls, |e| e.seq, lo, hi),
        pending_permissions: pending_permissions
            .iter()
            .filter(|(_, p)| keep_cards || (lo..hi).contains(&p.seq))
            .map(|(k, p)| (k.clone(), p.clone()))
            .collect(),
        turn_endings: kept(turn_endings, |e| e.seq, lo, hi),
        last_turn_ending: last_turn_ending.clone(),
        turn_starts: kept(turn_starts, |e| e.seq, lo, hi),
        notices: kept(notices, |e| e.seq, lo, hi),
        api_retry: api_retry.clone(),
        compacting: *compacting,
        rate_limit_warning: rate_limit_warning.clone(),
        fallback_model: fallback_model.clone(),
        effort: effort.clone(),
        cli_left_auto: *cli_left_auto,
        notification: notification.clone(),
        background_tasks: background_tasks.clone(),
        questions: kept(questions, |e| e.seq, lo, hi),
        last_turn_end_seq: *last_turn_end_seq,
        usage: usage.clone(),
        assistant_message_open: *assistant_message_open,
        thinking: kept(thinking, |e| e.seq, lo, hi),
        thinking_open: *thinking_open,
        last_revision: *last_revision,
        history: history.clone(),
        ungated_cli_mode: ungated_cli_mode.clone(),
        cli_mode_reported: cli_mode_reported.clone(),
        policy_mismatch: policy_mismatch.clone(),
        account_limits: account_limits.clone(),
        background_calls: *background_calls,
        available_commands: available_commands.clone(),
        background_at_last_turn_end: *background_at_last_turn_end,
        denials_before_their_call: denials_before_their_call.clone(),
        gate_inputs: gate_inputs.clone(),
        changed_inputs_before_their_call: changed_inputs_before_their_call.clone(),
        call_index: Default::default(),
        open_client_calls: open_client_calls.clone(),
        init_servers: init_servers.clone(),
        steer_outcomes: steer_outcomes.clone(),
        move_seeded: *move_seeded,
        cli_unseen_busy: *cli_unseen_busy,
        prompt_before_turn: *prompt_before_turn,
        running_turn_prompt_is_slash: *running_turn_prompt_is_slash,
        running_turn_said_text: *running_turn_said_text,
        opening_prompt: *opening_prompt,
        running_turn_cli: *running_turn_cli,
    }
}

/// A copy of the projection holding only the snapshot's turns, its cut entries cut.
pub(super) fn tail_copy(projection: &AgentSessionProjection, tail: &Tail) -> AgentSessionProjection {
    let mut copy = copy_between(projection, tail.from_seq, u64::MAX, true);
    for entry in &tail.cut {
        let seq = entry.seq;
        match entry.kind {
            Kind::Prompt => copy
                .user_prompts
                .iter_mut()
                .filter(|e| e.seq == seq)
                .for_each(|e| keep_start(&mut e.text)),
            Kind::Message => copy
                .transcript
                .iter_mut()
                .filter(|e| e.seq == seq)
                .for_each(|e| keep_start(&mut e.text)),
            Kind::Thought => copy
                .thinking
                .iter_mut()
                .filter(|e| e.seq == seq)
                .for_each(|e| keep_start(&mut e.text)),
            Kind::Call => {
                for call in copy.tool_calls.iter_mut().filter(|e| e.seq == seq) {
                    cut_value(&mut call.input);
                    if let Some(result) = call.result.as_mut() {
                        cut_value(&mut result.content);
                    }
                    if call
                        .children
                        .as_ref()
                        .is_some_and(|c| serialized_bytes(c) > CUT_KEEP_BYTES)
                    {
                        call.children = None;
                    }
                }
            }
            Kind::Notice => {
                for notice in copy.notices.iter_mut().filter(|e| e.seq == seq) {
                    if let Some(content) = notice.content.as_mut() {
                        keep_start(content);
                    }
                }
            }
            Kind::Ending | Kind::Start | Kind::Question => {}
        }
    }
    copy
}

/// A copy of the projection holding only turns `from..=through`, every entry whole.
pub(super) fn page_copy(
    projection: &AgentSessionProjection,
    turns: &Turns,
    from: usize,
    through: usize,
) -> AgentSessionProjection {
    let (lo, hi) = turns.seqs(from, through + 1);
    copy_between(projection, lo, hi, false)
}

/// `notes` with only the turn times whose anchor row is at `lo` or after it and before `hi`: those of the turns a
/// snapshot or a page holds (a time with no anchor has no row to stand under, so the page draws none for it).
pub(super) fn notes_between(notes: &CallNotes, lo: u64, hi: u64) -> CallNotes {
    let mut kept = notes.clone();
    kept.turn_times
        .retain(|time: &TurnTime| time.anchor_seq.is_some_and(|seq| (lo..hi).contains(&seq)));
    kept
}

/// The `seq` range of turns `from..=through`.
pub(super) fn page_seqs(turns: &Turns, from: usize, through: usize) -> (u64, u64) {
    turns.seqs(from, through + 1)
}

impl Tail {
    /// Where the snapshot's entries start.
    pub(super) fn from_seq(&self) -> u64 {
        self.from_seq
    }
}
