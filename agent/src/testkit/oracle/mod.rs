//! The transcript oracle: the Claude CLI's own session files and Eitri's records of the same conversation,
//! both read into one normal form and aligned.
//!
//! - [`cli_jsonl`] reads the CLI's session `.jsonl` files (a chain of them when `/clear` started a new
//!   session) and the subagent transcripts beside them. It is the oracle's own reader, written from the
//!   line shapes the pinned CLI was recorded writing; it shares no code with Eitri's restore reader, which
//!   is one of the things compared.
//! - [`eitri`] reads Eitri's three records: E (the domain event log and the final projection), A (the
//!   stored history file) and B (Eitri's restore reader run on the same `.jsonl`, folded).
//! - [`normalise`] states what counts as the same text, input and result, and projects the CLI form
//!   onto what each Eitri record can hold.
//! - [`align`] pairs the two forms turn by turn and entry by entry, at the resolution each record has.
//! - [`report`] lists the differences of each alignment, prints them and writes them as JSON; [`allowances`]
//!   says which known gaps cover which differences; [`redact`] decides what of a conversation a report may
//!   print.
//!
//! The binary `eitri-transcript-oracle` runs the whole of it on files given by path.

pub mod align;
pub mod allowances;
pub mod cli_jsonl;
pub mod eitri;
pub mod normalise;
pub mod redact;
pub mod report;

use serde_json::Value;
use std::collections::BTreeMap;

/// Who or what started a turn.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Origin {
    Human,
    Steer,
    Queued,
    TaskNotification,
    Scheduled,
    Peer,
    CompactContinue,
    Unknown,
}

/// How a user entry reached the conversation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Via {
    /// The words that opened a turn.
    Prompt,
    /// A person's message taken into the running turn.
    Steer,
    /// A person's message that waited for the running turn and was then taken into it.
    Queued,
    /// A background task's notification: the CLI's words, not a person's.
    TaskNotification,
    /// Another session or a subagent of this one.
    Peer,
    /// A message the CLI marked as not a person's, from no known producer.
    Meta,
    /// An origin this reader has no name for.
    Unknown,
}

/// Why a turn stopped other than by completing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EndReason {
    /// The CLI cut the turn short to deliver a message that arrived during a tool call.
    EndedToDeliver,
    /// Any other end a record names, by its own word.
    Other(String),
}

/// One thing in a turn.
#[derive(Clone, Debug, PartialEq)]
pub enum Entry {
    User {
        text: String,
        via: Via,
        /// The images the message carried, in order, as (media type, SHA-256 in hex): what each was, never the
        /// bytes. A digest is empty for an image too large for the record to have hashed.
        images: Vec<(String, String)>,
    },
    Assistant {
        text: String,
    },
    Thinking {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    ToolResult {
        id: String,
        is_error: bool,
        content: Value,
    },
    Subagent {
        parent_tool_use_id: String,
        turns: Vec<Turn>,
    },
    Compaction {
        trigger: String,
    },
    Interrupted,
    Ended {
        reason: EndReason,
    },
    /// A question set Claude asked (`AskUserQuestion`) and how it ended. Only its shape is kept: the words
    /// asked and answered never enter the normal form, so no report can print them.
    Question {
        /// The call's tool-use id (a record without one gives its interaction id), for pairing only.
        id: String,
        key: QuestionKey,
    },
    /// A plan Claude proposed (`ExitPlanMode`) and how it ended. Only its digest and end are kept: the plan's
    /// words never enter the normal form, so no report can print them.
    Plan {
        /// The call's tool-use id (a record without one gives its interaction id), for pairing only.
        id: String,
        key: PlanKey,
    },
}

/// How a question set ended, in the oracle's own closed list.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum QuestionEnd {
    /// The person's answers went back to Claude.
    Answered,
    /// The person turned the questions down.
    Declined,
    /// The person chose to answer in the chat instead.
    Chat,
    /// Something other than the person ended it: an interrupt, the session closing, an expiry.
    Cancelled,
    /// Any other end: a result the reader has no rule for, a host that could not show the form, a failed
    /// provider.
    Other,
}

impl QuestionEnd {
    pub fn word(self) -> &'static str {
        match self {
            QuestionEnd::Answered => "answered",
            QuestionEnd::Declined => "declined",
            QuestionEnd::Chat => "chat",
            QuestionEnd::Cancelled => "cancelled",
            QuestionEnd::Other => "other",
        }
    }
}

/// All the oracle keeps of a question set, and all a report prints of it: how many questions it held,
/// which of them (0-based, ascending) carry an answer, and how it ended.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct QuestionKey {
    pub count: usize,
    pub answered: Vec<usize>,
    pub end: QuestionEnd,
}

impl QuestionKey {
    /// The key with `answered` sorted and without repeats, whatever order the record gave.
    pub fn new(count: usize, mut answered: Vec<usize>, end: QuestionEnd) -> Self {
        answered.sort_unstable();
        answered.dedup();
        Self { count, answered, end }
    }
}

/// How a plan ended, in the oracle's own closed list.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PlanEnd {
    /// The person approved it and the model went on.
    Approved,
    /// The person kept planning, with or without words.
    KeptPlanning,
    /// Any other end: an interrupt, the session closing, an expiry, a plan the client could not carry, a
    /// result the reader has no rule for.
    Other,
}

impl PlanEnd {
    pub fn word(self) -> &'static str {
        match self {
            PlanEnd::Approved => "approved",
            PlanEnd::KeptPlanning => "kept_planning",
            PlanEnd::Other => "other",
        }
    }
}

/// All the oracle keeps of a plan, and all a report prints of it: the SHA-256 of its text (lower-case hex,
/// empty for a plan the client could not carry, which holds no text) and how it ended. The feedback a person
/// gave and the mode they approved into are not in it: the CLI's file holds neither.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PlanKey {
    pub plan_sha256: String,
    pub end: PlanEnd,
}

impl Entry {
    /// The entry's kind as one word, for counts and tests.
    pub fn kind(&self) -> &'static str {
        match self {
            Entry::User { .. } => "user",
            Entry::Assistant { .. } => "assistant",
            Entry::Thinking { .. } => "thinking",
            Entry::ToolUse { .. } => "tool_use",
            Entry::ToolResult { .. } => "tool_result",
            Entry::Subagent { .. } => "subagent",
            Entry::Compaction { .. } => "compaction",
            Entry::Interrupted => "interrupted",
            Entry::Ended { .. } => "ended",
            Entry::Question { .. } => "question",
            Entry::Plan { .. } => "plan",
        }
    }
}

/// Where an entry came from: the input's position among the files read (or 0 for an Eitri record) and the
/// 1-based line in it (for an Eitri record, the event index or the item's `seq`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Pos {
    pub input: usize,
    pub line: usize,
}

/// One turn: its 1-based index, who started it, and what happened in it, in order.
#[derive(Clone, Debug, PartialEq)]
pub struct Turn {
    pub index: usize,
    pub origin: Origin,
    pub entries: Vec<Entry>,
    /// Where each entry came from, one per entry.
    pub positions: Vec<Pos>,
}

impl Turn {
    pub(crate) fn new(index: usize, origin: Origin) -> Self {
        Self {
            index,
            origin,
            entries: Vec::new(),
            positions: Vec::new(),
        }
    }

    pub(crate) fn push(&mut self, entry: Entry, pos: Pos) {
        self.entries.push(entry);
        self.positions.push(pos);
    }
}

/// Something the reader saw that contradicts the rules it reads by. Never fatal: the form is still built,
/// and the report lists these beside the differences.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Inconsistency {
    /// A line that is not a JSON object.
    Malformed { pos: Pos },
    /// A `turn_duration` line followed by something other than a turn opener or the end of the file: the
    /// turn-end rule (a turn runs to the next opener) and the CLI's own marker disagree.
    TurnDurationMidTurn { pos: Pos },
    /// Conversation before any turn opener; it is kept in a turn of unknown origin.
    NoOpener { pos: Pos },
    /// A subagent transcript whose Agent call is not in the session.
    SubagentWithoutParent { tool_use_id: String, input: usize },
    /// A subagent transcript with no readable `.meta.json` naming its Agent call.
    SubagentWithoutMeta { input: usize },
}

/// Which file each input position names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InputKind {
    /// A session file of the chain, in the order given.
    Session,
    /// A subagent transcript.
    Subagent,
}

/// The CLI's side, read.
#[derive(Clone, Debug, Default)]
pub struct CliForm {
    pub turns: Vec<Turn>,
    /// Each input's kind and path, by position.
    pub inputs: Vec<(InputKind, std::path::PathBuf)>,
    pub lines_read: usize,
    /// Every line by kind: `type`, with `/subtype`, `/attachment.type` or `/operation` where the line has one.
    pub kinds_read: BTreeMap<String, usize>,
    /// Lines that gave no entry, by why (a kind, or a kind and the flag that excluded it).
    pub skipped: BTreeMap<String, usize>,
    /// Line kinds and shapes outside what the pinned CLI was recorded writing. A line of an unrecorded
    /// kind is never decoded and is counted in `skipped` as well.
    pub unrecorded: BTreeMap<String, usize>,
    pub inconsistencies: Vec<Inconsistency>,
}

/// Which of Eitri's records a form was read from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Source {
    /// The domain event log and the final projection.
    E,
    /// The stored history file.
    A,
    /// Eitri's restore reader on the CLI's own file, folded.
    B,
}

/// Which entries' turns a record can be held to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Attribution {
    /// Every entry carries its turn.
    Full,
    /// Tool calls (by their turn id) and messages put into a running turn; text rows are placed by order.
    ToolCallsAndMidTurnMessages,
    /// None: the record's turn ids do not name turns.
    None,
}

/// What a record says about history it left out.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Omission {
    /// Nothing was left out, or the record cannot leave anything out.
    None,
    /// The record says it left items out: how many, or `None` when it could not count them.
    Recorded(Option<usize>),
    /// The record is cut to its caps without saying so; these are its size by the caps' own measure, so
    /// the alignment can check that the next older item would not have fitted.
    Bounds { items: usize, chars: usize },
}

/// One of Eitri's records, read.
#[derive(Clone, Debug)]
pub struct EitriForm {
    pub source: Source,
    pub turns: Vec<Turn>,
    pub attribution: Attribution,
    pub omission: Omission,
    /// Tool calls whose input Eitri saw change between the gate and the CLI, by tool-use id: reported
    /// beside a matched call, never as a difference.
    pub changed_input: BTreeMap<String, Value>,
}
