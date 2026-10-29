//! The oracle's report: every difference between the CLI form and each of Eitri's records, what covers it,
//! the text a person reads and the JSON a machine reads.
//!
//! Each entry pair prints with a marker:
//!
//! - `=` the same;
//! - `~` the same after normalisation, with what normalisation did;
//! - `-` only in the CLI;
//! - `+` only in Eitri's record;
//! - `!` paired, with the fields that differ;
//!
//! followed by `(allowed: <id>, ...)` when allowances cover it, or `(allowed by kind: <word>)` when it is a
//! difference every run has by the nature of the record ([`ByKind`]). The summary line counts them, and
//! the exit code is 0 when every difference is allowed, 1 when one is not (or an allowance matched
//! nothing, or the reader met a line it could not square with its rules), 2 when an input could not be
//! read.
//!
//! Everything printed goes through a [`Redactor`]: the default prints structure and keyed digests only.

use super::align::{Alignment, EntryPair, Field, Mark, Note, TurnMatch};
use super::allowances::AllowanceSet;
use super::normalise::{result_text, text, user_text};
use super::redact::{utf16_len, Redactor};
use super::{CliForm, EndReason, Entry, Inconsistency, InputKind, Origin, Pos, QuestionKey, Source, Turn, Via};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::fmt::Write as _;

/// The exit code for an input that could not be read.
pub const EXIT_UNREADABLE: i32 = 2;

/// A difference every run against a record of this kind has, allowed in code with its reason.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ByKind {
    /// The record cut its oldest items to its caps and said so (or, for the stored history, the next older
    /// item could not have fitted them).
    RecordedTruncation,
    /// Eitri ends a turn that did not complete with a row of its own; the CLI writes a line for an
    /// interrupted reply only, and none for a failed turn or a reached limit.
    EitriTurnEnding,
    /// A command the CLI runs itself, without the model, is recorded by Eitri as a prompt; the CLI may write
    /// nothing of it into the session file.
    EitriLocalCommand,
}

impl ByKind {
    pub fn word(self) -> &'static str {
        match self {
            ByKind::RecordedTruncation => "recorded-truncation",
            ByKind::EitriTurnEnding => "eitri-turn-ending",
            ByKind::EitriLocalCommand => "eitri-local-command",
        }
    }
}

/// Why the CLI form loses entries of a kind before it is compared with a record: the record cannot hold
/// them, so they are never differences. Permission card records and the notices Eitri makes itself are
/// not read into Eitri's forms at all, and the CLI lines the reader skips are counted in the CLI summary.
pub fn projected_out_reason(source: Source, kind: &str) -> &'static str {
    match (source, kind) {
        (Source::A, "thinking") => "the stored history keeps no thinking",
        (Source::B, "thinking") => "the restore reader drops thinking",
        (_, "interrupted" | "ended") => "the record has no row for the end of a turn",
        (Source::B, "subagent") => {
            "the restore reader does not open subagent transcripts, as the CLI's resume does not"
        }
        _ => "the record cannot hold this kind",
    }
}

pub fn source_word(source: Source) -> &'static str {
    match source {
        Source::E => "E",
        Source::A => "A",
        Source::B => "B",
    }
}

fn source_title(source: Source) -> &'static str {
    match source {
        Source::E => "Eitri events E",
        Source::A => "Eitri history A",
        Source::B => "Eitri restore B",
    }
}

pub fn origin_word(origin: &Origin) -> &'static str {
    match origin {
        Origin::Human => "human",
        Origin::Steer => "steer",
        Origin::Queued => "queued",
        Origin::TaskNotification => "task_notification",
        Origin::Scheduled => "scheduled",
        Origin::Peer => "peer",
        Origin::CompactContinue => "compact_continue",
        Origin::Unknown => "unknown",
    }
}

pub fn via_word(via: &Via) -> &'static str {
    match via {
        Via::Prompt => "prompt",
        Via::Steer => "steer",
        Via::Queued => "queued",
        Via::TaskNotification => "task_notification",
        Via::Peer => "peer",
        Via::Meta => "meta",
        Via::Unknown => "unknown",
    }
}

pub fn turn_match_word(how: &TurnMatch) -> &'static str {
    match how {
        TurnMatch::Same => "same",
        TurnMatch::ByEntries => "by_entries",
        TurnMatch::OrderOnly => "order_only",
        TurnMatch::Merged => "merged",
        TurnMatch::Truncated => "truncated",
        TurnMatch::CliOnly => "cli_only",
        TurnMatch::EitriOnly => "eitri_only",
    }
}

pub fn field_word(field: &Field) -> &'static str {
    match field {
        Field::Text => "text",
        Field::Via => "via",
        Field::Name => "name",
        Field::Input => "input",
        Field::IsError => "is_error",
        Field::Result => "result",
        Field::Questions => "questions",
        Field::Answered => "answered",
        Field::Outcome => "outcome",
        Field::Turn => "turn",
        Field::Order => "order",
    }
}

fn note_word(note: &Note) -> &'static str {
    match note {
        Note::ComposedBlock => "composed_block",
        Note::Whitespace => "whitespace",
        Note::ResultFlattened => "result_flattened",
        Note::ChangedInput => "changed_input",
    }
}

/// The words of each matcher field, for checking allowance files.
pub const ORIGIN_WORDS: &[&str] = &[
    "human",
    "steer",
    "queued",
    "task_notification",
    "scheduled",
    "peer",
    "compact_continue",
    "unknown",
];
pub const VIA_WORDS: &[&str] = &[
    "prompt",
    "steer",
    "queued",
    "task_notification",
    "peer",
    "meta",
    "unknown",
];
pub const TURN_MATCH_WORDS: &[&str] = &[
    "same",
    "by_entries",
    "order_only",
    "merged",
    "truncated",
    "cli_only",
    "eitri_only",
];
pub const FIELD_WORDS: &[&str] = &[
    "text",
    "via",
    "name",
    "input",
    "is_error",
    "result",
    "questions",
    "answered",
    "outcome",
    "turn",
    "order",
];
/// Entry kinds, and `turn` for a turn the record merged into another.
pub const KIND_WORDS: &[&str] = &[
    "user",
    "assistant",
    "thinking",
    "tool_use",
    "tool_result",
    "subagent",
    "compaction",
    "interrupted",
    "ended",
    "question",
    "turn",
];

/// How a difference is marked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Marker {
    CliOnly,
    EitriOnly,
    Differs,
}

impl Marker {
    pub fn symbol(self) -> &'static str {
        match self {
            Marker::CliOnly => "-",
            Marker::EitriOnly => "+",
            Marker::Differs => "!",
        }
    }
}

/// One difference between the CLI form and a record.
#[derive(Clone, Debug)]
pub struct Difference {
    pub source: Source,
    /// Where it sits: the turn pair's index in [`Alignment::turns`], then the entry pair's index in it;
    /// inside a subagent, the subagent's own turn pair and entry pair follow. A turn the record merged into
    /// another is its turn pair's index alone.
    pub at: Vec<usize>,
    pub marker: Marker,
    /// The entry's kind, or `turn`.
    pub kind: &'static str,
    pub fields: Vec<Field>,
    pub turn_match: TurnMatch,
    /// The CLI turn's origin, when the CLI has the turn.
    pub origin: Option<Origin>,
    pub via: Option<Via>,
    pub tool: Option<String>,
    /// The entry's normalised text (a result's flattened text), for matchers.
    pub text: Option<String>,
    /// Inside a subagent's transcript.
    pub nested: bool,
    /// Whether the CLI's entry opened its turn (`Some(true)`) or was taken into a running turn
    /// (`Some(false)`); `None` for a difference with no CLI entry.
    pub opens_turn: Option<bool>,
    pub by_kind: Option<ByKind>,
    /// The ids of the allowances that cover it.
    pub allowed: Vec<String>,
}

/// The CLI form against one record.
#[derive(Clone, Debug)]
pub struct Section {
    pub alignment: Alignment,
    /// The first turn pair the report covers.
    pub start: usize,
    pub differences: Vec<Difference>,
}

/// What the report says about the CLI's files.
#[derive(Clone, Debug)]
pub struct CliSummary {
    pub inputs: Vec<InputKind>,
    pub file_names: Vec<String>,
    pub lines_read: usize,
    pub turns: usize,
    pub kinds_read: BTreeMap<String, usize>,
    pub skipped: BTreeMap<String, usize>,
    pub unrecorded: BTreeMap<String, usize>,
    pub inconsistencies: Vec<Inconsistency>,
}

/// The whole report.
#[derive(Clone, Debug)]
pub struct Report {
    pub cli: CliSummary,
    pub from_turn: Option<usize>,
    pub sections: Vec<Section>,
    /// One message per allowance that matched nothing for a source this run computed.
    pub unmatched_allowances: Vec<String>,
    /// Differences covered, by allowance id.
    pub allowance_matches: BTreeMap<String, usize>,
}

/// Counts of one section.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    pub missing_entries: usize,
    pub missing_turns: usize,
    pub extra_entries: usize,
    pub extra_turns: usize,
    pub text_differs: usize,
    pub order_differs: usize,
    pub turn_differs: usize,
    pub other_differs: usize,
    /// Every difference not allowed by kind.
    pub differences: usize,
    pub allowed: usize,
    pub by_kind: usize,
    pub normalised: usize,
}

impl Report {
    /// The report of `cli` against each alignment, from CLI turn `from_turn` (1-based) when given.
    pub fn build(cli: &CliForm, alignments: Vec<Alignment>, from_turn: Option<usize>) -> Report {
        let file_names = cli
            .inputs
            .iter()
            .map(|(_, path)| {
                path.file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default()
            })
            .collect();
        let summary = CliSummary {
            inputs: cli.inputs.iter().map(|(k, _)| k.clone()).collect(),
            file_names,
            lines_read: cli.lines_read,
            turns: cli.turns.len(),
            kinds_read: cli.kinds_read.clone(),
            skipped: cli.skipped.clone(),
            unrecorded: cli.unrecorded.clone(),
            inconsistencies: cli.inconsistencies.clone(),
        };
        let sections = alignments
            .into_iter()
            .map(|alignment| {
                let start = match from_turn {
                    None => 0,
                    Some(n) => alignment
                        .turns
                        .iter()
                        .position(|pair| pair.cli.is_some_and(|c| alignment.cli[c].index >= n))
                        .unwrap_or(alignment.turns.len()),
                };
                let mut differences = Vec::new();
                collect(&alignment, start, &mut Vec::new(), false, &mut differences);
                Section {
                    alignment,
                    start,
                    differences,
                }
            })
            .collect();
        Report {
            cli: summary,
            from_turn,
            sections,
            unmatched_allowances: Vec::new(),
            allowance_matches: BTreeMap::new(),
        }
    }

    /// Applies the allowances of `scenario`: every allowance whose matcher fits a difference of its source
    /// covers it (several may cover one). An allowance of a source this report computed that covers
    /// nothing is recorded as a failure; one of a source not computed here is not checked.
    pub fn apply_allowances(&mut self, set: &AllowanceSet) {
        let computed: Vec<Source> = self.sections.iter().map(|s| s.alignment.source).collect();
        for allowance in set.entries() {
            if !computed.contains(&allowance.source) {
                continue;
            }
            let mut matched = 0;
            for section in self.sections.iter_mut() {
                if section.alignment.source != allowance.source {
                    continue;
                }
                for difference in section.differences.iter_mut() {
                    if difference.by_kind.is_none() && allowance.matcher.matches(difference) {
                        if !difference.allowed.contains(&allowance.id) {
                            difference.allowed.push(allowance.id.clone());
                        }
                        matched += 1;
                    }
                }
            }
            if matched == 0 {
                self.unmatched_allowances.push(format!(
                    "allowance {} matched nothing: delete it (scenario {}, source {}) matcher {}",
                    allowance.id,
                    set.scenario(),
                    source_word(allowance.source),
                    allowance.matcher.as_json()
                ));
            } else {
                *self.allowance_matches.entry(allowance.id.clone()).or_default() += matched;
            }
        }
    }

    pub fn counts(&self, section: &Section) -> Counts {
        let mut counts = Counts::default();
        for pair in &section.alignment.turns[section.start..] {
            match pair.how {
                TurnMatch::CliOnly => counts.missing_turns += 1,
                TurnMatch::EitriOnly => counts.extra_turns += 1,
                _ => {}
            }
            count_normalised(&pair.entries, &mut counts);
        }
        for d in &section.differences {
            if d.by_kind.is_some() {
                counts.by_kind += 1;
                continue;
            }
            counts.differences += 1;
            if !d.allowed.is_empty() {
                counts.allowed += 1;
            }
            match d.marker {
                Marker::CliOnly => counts.missing_entries += 1,
                Marker::EitriOnly => counts.extra_entries += 1,
                Marker::Differs => {
                    if d.fields.contains(&Field::Text) {
                        counts.text_differs += 1;
                    } else if d.fields.contains(&Field::Order) {
                        counts.order_differs += 1;
                    } else if d.fields.contains(&Field::Turn) {
                        counts.turn_differs += 1;
                    } else {
                        counts.other_differs += 1;
                    }
                }
            }
        }
        counts
    }

    /// 0 when every difference is allowed, every allowance matched and the reader met nothing it could not
    /// read by its rules; 1 otherwise.
    pub fn exit_code(&self) -> i32 {
        let unallowed = self
            .sections
            .iter()
            .flat_map(|s| s.differences.iter())
            .any(|d| d.by_kind.is_none() && d.allowed.is_empty());
        if unallowed || !self.unmatched_allowances.is_empty() || !self.cli.inconsistencies.is_empty() {
            1
        } else {
            0
        }
    }

    /// Differences no allowance covers, for a test's failure message.
    pub fn unallowed(&self) -> impl Iterator<Item = &Difference> {
        self.sections
            .iter()
            .flat_map(|s| s.differences.iter())
            .filter(|d| d.by_kind.is_none() && d.allowed.is_empty())
    }

    /// An input's name by its position: `--cli #k` for the k-th session file, `--subagents #k` for the k-th
    /// subagent transcript. With text shown, the file's name follows.
    fn input_name(&self, input: usize, r: &Redactor) -> String {
        let kind = self.cli.inputs.get(input);
        let nth = self.cli.inputs[..input.min(self.cli.inputs.len())]
            .iter()
            .filter(|k| Some(*k) == kind)
            .count()
            + 1;
        let flag = match kind {
            Some(InputKind::Subagent) => "--subagents",
            _ => "--cli",
        };
        if r.shows_text() {
            let name = self.cli.file_names.get(input).cloned().unwrap_or_default();
            format!("{flag} #{nth} ({name})")
        } else {
            format!("{flag} #{nth}")
        }
    }

    fn cli_inputs_line(&self, r: &Redactor) -> String {
        let names: Vec<String> = (0..self.cli.inputs.len())
            .filter(|&i| self.cli.inputs[i] == InputKind::Session)
            .map(|i| self.input_name(i, r))
            .collect();
        let subagents = self.cli.inputs.iter().filter(|k| **k == InputKind::Subagent).count();
        if subagents > 0 {
            format!("{} + {subagents} subagent transcripts", names.join(", "))
        } else {
            names.join(", ")
        }
    }

    /// The report as a person reads it.
    pub fn render(&self, r: &Redactor) -> String {
        let mut out = String::new();
        let inputs = self.cli_inputs_line(r);
        let _ = writeln!(
            out,
            "oracle: CLI {inputs} ({} turns, {} lines){}",
            self.cli.turns,
            self.cli.lines_read,
            match self.from_turn {
                Some(n) => format!("  from CLI turn {n}"),
                None => String::new(),
            }
        );
        let _ = writeln!(out, "  skipped: {}", count_list(&self.cli.skipped, r));
        let _ = writeln!(out, "  unrecorded: {}", count_list(&self.cli.unrecorded, r));
        if self.cli.inconsistencies.is_empty() {
            let _ = writeln!(out, "  reader inconsistencies: none");
        } else {
            for inconsistency in &self.cli.inconsistencies {
                let _ = writeln!(out, "  reader inconsistency: {}", self.inconsistency(inconsistency, r));
            }
        }
        for section in &self.sections {
            self.render_section(section, r, &mut out);
        }
        for message in &self.unmatched_allowances {
            let _ = writeln!(out, "{message}");
        }
        let _ = writeln!(out, "exit {}", self.exit_code());
        out
    }

    fn inconsistency(&self, inconsistency: &Inconsistency, r: &Redactor) -> String {
        match inconsistency {
            Inconsistency::Malformed { pos } => format!("malformed line @{}", self.cli_pos(*pos, r)),
            Inconsistency::TurnDurationMidTurn { pos } => {
                format!("turn_duration inside a turn @{}", self.cli_pos(*pos, r))
            }
            Inconsistency::NoOpener { pos } => format!("conversation before any opener @{}", self.cli_pos(*pos, r)),
            Inconsistency::SubagentWithoutParent { tool_use_id, input } => format!(
                "subagent transcript {} names a call {} the session lacks",
                self.input_name(*input, r),
                r.id(tool_use_id)
            ),
            Inconsistency::SubagentWithoutMeta { input } => {
                format!(
                    "subagent transcript {} has no readable meta file",
                    self.input_name(*input, r)
                )
            }
        }
    }

    fn inconsistency_json(&self, inconsistency: &Inconsistency, r: &Redactor) -> Value {
        match inconsistency {
            Inconsistency::Malformed { pos } => json!({"kind": "malformed", "at": self.cli_pos(*pos, r)}),
            Inconsistency::TurnDurationMidTurn { pos } => {
                json!({"kind": "turn_duration_mid_turn", "at": self.cli_pos(*pos, r)})
            }
            Inconsistency::NoOpener { pos } => json!({"kind": "no_opener", "at": self.cli_pos(*pos, r)}),
            Inconsistency::SubagentWithoutParent { tool_use_id, input } => json!({
                "kind": "subagent_without_parent",
                "input": self.input_name(*input, r),
                "tool_use_id": r.id(tool_use_id),
            }),
            Inconsistency::SubagentWithoutMeta { input } => {
                json!({"kind": "subagent_without_meta", "input": self.input_name(*input, r)})
            }
        }
    }

    fn cli_pos(&self, pos: Pos, r: &Redactor) -> String {
        format!("{}:{}", self.input_name(pos.input, r), pos.line)
    }

    fn render_section(&self, section: &Section, r: &Redactor, out: &mut String) {
        let a = &section.alignment;
        let s = source_word(a.source);
        let counts = self.counts(section);
        let _ = writeln!(
            out,
            "oracle: CLI {} ({} turns)  vs  {} ({} turns)          RESULT: {} differences, {} allowed",
            self.cli_inputs_line(r),
            a.cli.len(),
            source_title(a.source),
            a.eitri.len(),
            counts.differences,
            counts.allowed
        );
        let mut index: BTreeMap<Vec<usize>, usize> = BTreeMap::new();
        for (i, d) in section.differences.iter().enumerate() {
            index.insert(d.at.clone(), i);
        }
        self.render_turns(section, a, section.start, &mut Vec::new(), "", r, &index, out);
        let _ = writeln!(
            out,
            "summary: turns CLI {} / {s} {}; missing-in-{s} {} entries ({} turns); extra-in-{s} {} entries ({} \
             turns); text differs {}; order differs {}; turn differs {}; other differs {}; allowed {}; by kind {}; \
             normalised {}",
            a.cli.len(),
            a.eitri.len(),
            counts.missing_entries,
            counts.missing_turns,
            counts.extra_entries,
            counts.extra_turns,
            counts.text_differs,
            counts.order_differs,
            counts.turn_differs,
            counts.other_differs,
            counts.allowed,
            counts.by_kind,
            counts.normalised
        );
        if !a.projected_out.is_empty() {
            let parts: Vec<String> = a
                .projected_out
                .iter()
                .map(|(kind, n)| format!("{kind} {n} ({})", projected_out_reason(a.source, kind)))
                .collect();
            let _ = writeln!(out, "by kind, not compared: {}", parts.join("; "));
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn render_turns(
        &self,
        section: &Section,
        a: &Alignment,
        start: usize,
        prefix: &mut Vec<usize>,
        indent: &str,
        r: &Redactor,
        index: &BTreeMap<Vec<usize>, usize>,
        out: &mut String,
    ) {
        let s = source_word(a.source);
        for (t, pair) in a.turns.iter().enumerate().skip(start) {
            prefix.push(t);
            let cli_side = match pair.cli {
                Some(c) => format!("turn {}  CLI origin={}", a.cli[c].index, origin_word(&a.cli[c].origin)),
                None => "turn -  CLI: -- missing --".to_owned(),
            };
            let record_side = match (pair.eitri, &pair.how) {
                (Some(e), how) => {
                    let suffix = match how {
                        TurnMatch::ByEntries => " (placed by entries)",
                        TurnMatch::OrderOnly => " (order only)",
                        TurnMatch::Merged => " (merged)",
                        _ => "",
                    };
                    format!(
                        "{s} turn {} origin={}{suffix}",
                        a.eitri[e].index,
                        origin_word(&a.eitri[e].origin)
                    )
                }
                (None, TurnMatch::Truncated) => format!("{s}: -- truncated --"),
                (None, _) => format!("{s}: -- missing --"),
            };
            let _ = writeln!(out, "{indent}{cli_side}        {record_side}");
            if let Some(&i) = index.get(prefix.as_slice()) {
                let d = &section.differences[i];
                let _ = writeln!(
                    out,
                    "{indent}  !  turn kept in another turn of {s}{}",
                    allowed_suffix(d)
                );
            }
            for (e, entry) in pair.entries.iter().enumerate() {
                prefix.push(e);
                let line = self.entry_line(
                    a,
                    entry,
                    index.get(prefix.as_slice()).map(|&i| &section.differences[i]),
                    r,
                );
                let _ = writeln!(out, "{indent}  {line}");
                if let Some(nested) = &entry.nested {
                    let deeper = format!("{indent}      ");
                    self.render_turns(section, nested, 0, prefix, &deeper, r, index, out);
                }
                prefix.pop();
            }
            prefix.pop();
        }
    }

    fn entry_line(&self, a: &Alignment, pair: &EntryPair, difference: Option<&Difference>, r: &Redactor) -> String {
        let s = source_word(a.source);
        let symbol = match &pair.mark {
            Mark::Same => "=",
            Mark::Normalised(_) => "~",
            Mark::CliOnly | Mark::Truncated => "-",
            Mark::EitriOnly => "+",
            Mark::Differs(_) => "!",
        };
        let mut line = String::from(symbol);
        line.push_str("  ");
        let cli_desc = pair.cli.map(|c| describe(a.cli_entry(c), r));
        let record_desc = pair.eitri.map(|e| describe(a.eitri_entry(e), r));
        match (&cli_desc, &record_desc) {
            (Some(c), Some(e)) if c != e => {
                let _ = write!(line, "{c}  |  {s}: {e}");
            }
            (Some(c), _) => line.push_str(c),
            (None, Some(e)) => line.push_str(e),
            (None, None) => {}
        }
        match &pair.mark {
            Mark::Normalised(notes) => {
                let words: Vec<&str> = notes.iter().map(note_word).collect();
                let _ = write!(line, "  normalised: {}", words.join(", "));
            }
            Mark::Differs(fields) => {
                let words: Vec<&str> = fields.iter().map(field_word).collect();
                let _ = write!(line, "  differs: {}", words.join(", "));
                if fields.contains(&Field::Turn) {
                    if let (Some(c), Some(e)) = (pair.cli, pair.eitri) {
                        let _ = write!(
                            line,
                            " (CLI turn {}, {s} turn {})",
                            a.cli[c.turn].index, a.eitri[e.turn].index
                        );
                    }
                }
            }
            _ => {}
        }
        if let Some(d) = difference {
            line.push_str(&allowed_suffix(d));
        }
        let mut at = Vec::new();
        if let Some(c) = pair.cli {
            at.push(self.cli_pos(a.cli[c.turn].positions[c.entry], r));
        }
        if let Some(e) = pair.eitri {
            at.push(format!("{s}:{}", a.eitri[e.turn].positions[e.entry].line));
        }
        if !at.is_empty() {
            let _ = write!(line, "  @{}", at.join(" "));
        }
        line
    }

    /// The report as a machine reads it, redacted the same way as [`Self::render`].
    pub fn to_json(&self, r: &Redactor) -> Value {
        let sections: Vec<Value> = self
            .sections
            .iter()
            .map(|section| {
                let a = &section.alignment;
                let counts = self.counts(section);
                let mut index: BTreeMap<Vec<usize>, usize> = BTreeMap::new();
                for (i, d) in section.differences.iter().enumerate() {
                    index.insert(d.at.clone(), i);
                }
                let turns = self.turns_json(section, a, section.start, &mut Vec::new(), r, &index);
                let projected_out: Map<String, Value> =
                    a.projected_out.iter().map(|(k, n)| (k.to_string(), json!(n))).collect();
                json!({
                    "source": source_word(a.source),
                    "turns_cli": a.cli.len(),
                    "turns_record": a.eitri.len(),
                    "counts": {
                        "differences": counts.differences,
                        "allowed": counts.allowed,
                        "by_kind": counts.by_kind,
                        "missing_entries": counts.missing_entries,
                        "missing_turns": counts.missing_turns,
                        "extra_entries": counts.extra_entries,
                        "extra_turns": counts.extra_turns,
                        "text_differs": counts.text_differs,
                        "order_differs": counts.order_differs,
                        "turn_differs": counts.turn_differs,
                        "other_differs": counts.other_differs,
                        "normalised": counts.normalised,
                    },
                    "projected_out": projected_out,
                    "turns": turns,
                })
            })
            .collect();
        json!({
            "exit": self.exit_code(),
            "redacted": !r.shows_text(),
            "from_turn": self.from_turn,
            "cli": {
                "inputs": (0..self.cli.inputs.len()).map(|i| self.input_name(i, r)).collect::<Vec<_>>(),
                "lines_read": self.cli.lines_read,
                "turns": self.cli.turns,
                "kinds_read": count_map(&self.cli.kinds_read, r),
                "skipped": count_map(&self.cli.skipped, r),
                "unrecorded": count_map(&self.cli.unrecorded, r),
                "inconsistencies": self.cli.inconsistencies.iter().map(|i| self.inconsistency_json(i, r)).collect::<Vec<_>>(),
            },
            "sections": sections,
            "allowances": {
                "matched": self.allowance_matches,
                "unmatched": self.unmatched_allowances,
            },
        })
    }

    fn turns_json(
        &self,
        section: &Section,
        a: &Alignment,
        start: usize,
        prefix: &mut Vec<usize>,
        r: &Redactor,
        index: &BTreeMap<Vec<usize>, usize>,
    ) -> Vec<Value> {
        let mut turns = Vec::new();
        for (t, pair) in a.turns.iter().enumerate().skip(start) {
            prefix.push(t);
            let mut entries = Vec::new();
            for (e, entry) in pair.entries.iter().enumerate() {
                prefix.push(e);
                let difference = index.get(prefix.as_slice()).map(|&i| &section.differences[i]);
                let mut value = json!({
                    "marker": match &entry.mark {
                        Mark::Same => "=",
                        Mark::Normalised(_) => "~",
                        Mark::CliOnly | Mark::Truncated => "-",
                        Mark::EitriOnly => "+",
                        Mark::Differs(_) => "!",
                    },
                    "cli": entry.cli.map(|c| describe_json(a.cli_entry(c), r)),
                    "record": entry.eitri.map(|e| describe_json(a.eitri_entry(e), r)),
                    "cli_at": entry.cli.map(|c| self.cli_pos(a.cli[c.turn].positions[c.entry], r)),
                    "record_at": entry.eitri.map(|e| a.eitri[e.turn].positions[e.entry].line),
                });
                match &entry.mark {
                    Mark::Differs(fields) => {
                        value["fields"] = json!(fields.iter().map(field_word).collect::<Vec<_>>());
                    }
                    Mark::Normalised(notes) => {
                        value["normalised"] = json!(notes.iter().map(note_word).collect::<Vec<_>>());
                    }
                    _ => {}
                }
                if let Some(d) = difference {
                    value["allowed"] = json!(d.allowed);
                    value["allowed_by_kind"] = json!(d.by_kind.map(ByKind::word));
                }
                if let Some(nested) = &entry.nested {
                    value["nested"] = json!(self.turns_json(section, nested, 0, prefix, r, index));
                }
                entries.push(value);
                prefix.pop();
            }
            let merged = index.get(prefix.as_slice()).map(|&i| &section.differences[i]);
            turns.push(json!({
                "cli_turn": pair.cli.map(|c| a.cli[c].index),
                "cli_origin": pair.cli.map(|c| origin_word(&a.cli[c].origin)),
                "record_turn": pair.eitri.map(|e| a.eitri[e].index),
                "how": turn_match_word(&pair.how),
                "merged_allowed": merged.map(|d| d.allowed.clone()),
                "entries": entries,
            }));
            prefix.pop();
        }
        turns
    }
}

fn allowed_suffix(d: &Difference) -> String {
    if let Some(kind) = d.by_kind {
        format!("  (allowed by kind: {})", kind.word())
    } else if d.allowed.is_empty() {
        String::new()
    } else {
        format!("  (allowed: {})", d.allowed.join(", "))
    }
}

fn count_normalised(entries: &[EntryPair], counts: &mut Counts) {
    for entry in entries {
        if matches!(entry.mark, Mark::Normalised(_)) {
            counts.normalised += 1;
        }
        if let Some(nested) = &entry.nested {
            for pair in &nested.turns {
                count_normalised(&pair.entries, counts);
            }
        }
    }
}

/// Counts by key, keys redacted; two keys that redact alike are summed.
fn count_map(counts: &BTreeMap<String, usize>, r: &Redactor) -> BTreeMap<String, usize> {
    let mut out: BTreeMap<String, usize> = BTreeMap::new();
    for (key, n) in counts {
        *out.entry(r.count_key(key)).or_default() += n;
    }
    out
}

fn count_list(counts: &BTreeMap<String, usize>, r: &Redactor) -> String {
    let map = count_map(counts, r);
    if map.is_empty() {
        return "none".to_owned();
    }
    map.iter()
        .map(|(k, n)| format!("{k} {n}"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn end_word(reason: &EndReason, r: &Redactor) -> String {
    match reason {
        EndReason::EndedToDeliver => "ended_to_deliver".to_owned(),
        EndReason::Other(word) => r.word(word).to_owned(),
    }
}

/// One entry as the report prints it.
pub fn describe(entry: &Entry, r: &Redactor) -> String {
    match entry {
        Entry::User { text: t, via } => format!("user[{}] {}", via_word(via), r.text(&user_text(t))),
        Entry::Assistant { text: t } => format!("assistant {}", r.text(&text(t))),
        Entry::Thinking { text: t } => format!("thinking {}", r.text(&text(t))),
        Entry::ToolUse { id, name, input } => {
            format!("tool_use {} {} {}", r.id(id), r.tool(name), r.input(name, input))
        }
        Entry::ToolResult { id, is_error, content } => format!(
            "tool_result {} {} {}",
            r.id(id),
            if *is_error { "error" } else { "ok" },
            r.text(&text(&result_text(content)))
        ),
        Entry::Subagent {
            parent_tool_use_id,
            turns,
        } => format!("subagent {} {} turns", r.id(parent_tool_use_id), turns.len()),
        Entry::Compaction { trigger } => format!("compaction {}", r.word(trigger)),
        Entry::Interrupted => "interrupted".to_owned(),
        Entry::Ended { reason } => format!("ended {}", end_word(reason, r)),
        Entry::Question { key, .. } => question_line(key),
    }
}

/// A question set as the report prints it, with or without text shown: its key, nothing it asked or was
/// answered, and no id.
fn question_line(key: &QuestionKey) -> String {
    let answered: Vec<String> = key.answered.iter().map(usize::to_string).collect();
    format!(
        "question count={} answered=[{}] outcome={}",
        key.count,
        answered.join(","),
        key.end.word()
    )
}

fn text_json(t: &str, r: &Redactor) -> Value {
    if r.shows_text() {
        json!({"chars": utf16_len(t), "text": t})
    } else {
        json!({"chars": utf16_len(t), "sha": r.digest(t)})
    }
}

/// One entry as the JSON report holds it.
pub fn describe_json(entry: &Entry, r: &Redactor) -> Value {
    let mut value = match entry {
        Entry::User { text: t, via } => {
            let mut v = text_json(&user_text(t), r);
            v["via"] = json!(via_word(via));
            v
        }
        Entry::Assistant { text: t } | Entry::Thinking { text: t } => text_json(&text(t), r),
        Entry::ToolUse { id, name, input } => {
            let mut v = json!({"id": r.id(id), "tool": r.tool(name)});
            match r.input_keys(name, input) {
                Some(keys) => v["keys"] = json!(keys),
                None => {
                    v["key_count"] = json!(input.as_object().map(|m| m.len()).unwrap_or(0));
                }
            }
            if r.shows_text() {
                v["input"] = input.clone();
            }
            v
        }
        Entry::ToolResult { id, is_error, content } => {
            let mut v = text_json(&text(&result_text(content)), r);
            v["id"] = json!(r.id(id));
            v["is_error"] = json!(is_error);
            v
        }
        Entry::Subagent {
            parent_tool_use_id,
            turns,
        } => json!({"id": r.id(parent_tool_use_id), "turns": turns.len()}),
        Entry::Compaction { trigger } => json!({"trigger": r.word(trigger)}),
        Entry::Interrupted => json!({}),
        Entry::Ended { reason } => json!({"reason": end_word(reason, r)}),
        Entry::Question { key, .. } => json!({
            "count": key.count,
            "answered": key.answered,
            "outcome": key.end.word(),
        }),
    };
    value["kind"] = json!(entry.kind());
    value
}

/// Gathers the differences of `a` from turn pair `start`, at `prefix`.
fn collect(a: &Alignment, start: usize, prefix: &mut Vec<usize>, nested: bool, out: &mut Vec<Difference>) {
    for (t, pair) in a.turns.iter().enumerate().skip(start) {
        prefix.push(t);
        let origin = pair.cli.map(|c| a.cli[c].origin.clone());
        if pair.how == TurnMatch::Merged {
            out.push(Difference {
                source: a.source,
                at: prefix.clone(),
                marker: Marker::Differs,
                kind: "turn",
                fields: vec![Field::Turn],
                turn_match: pair.how.clone(),
                origin: origin.clone(),
                via: None,
                tool: None,
                text: None,
                nested,
                opens_turn: None,
                by_kind: None,
                allowed: Vec::new(),
            });
        }
        for (e, entry) in pair.entries.iter().enumerate() {
            prefix.push(e);
            let (marker, fields, by_kind) = match &entry.mark {
                Mark::Same | Mark::Normalised(_) => (None, Vec::new(), None),
                Mark::CliOnly => (Some(Marker::CliOnly), Vec::new(), None),
                Mark::Truncated => (Some(Marker::CliOnly), Vec::new(), Some(ByKind::RecordedTruncation)),
                Mark::EitriOnly => {
                    let record = a.eitri_entry(entry.eitri.expect("a record-only entry has a record side"));
                    (Some(Marker::EitriOnly), Vec::new(), record_only_by_kind(record, pair))
                }
                Mark::Differs(fields) => (Some(Marker::Differs), fields.clone(), None),
            };
            if let Some(marker) = marker {
                let shown = match (entry.cli, entry.eitri) {
                    (Some(c), _) => a.cli_entry(c),
                    (None, Some(e)) => a.eitri_entry(e),
                    (None, None) => unreachable!("every entry pair holds an entry"),
                };
                out.push(Difference {
                    source: a.source,
                    at: prefix.clone(),
                    marker,
                    kind: shown.kind(),
                    fields,
                    turn_match: pair.how.clone(),
                    origin: origin.clone(),
                    via: match shown {
                        Entry::User { via, .. } => Some(via.clone()),
                        _ => None,
                    },
                    tool: match shown {
                        Entry::ToolUse { name, .. } => Some(name.clone()),
                        Entry::ToolResult { id, .. } => tool_name_of(a, entry, id),
                        _ => None,
                    },
                    text: entry_text(shown),
                    nested,
                    opens_turn: entry.cli.map(|at| at.entry == 0),
                    by_kind,
                    allowed: Vec::new(),
                });
            }
            if let Some(sub) = &entry.nested {
                collect(sub, 0, prefix, true, out);
            }
            prefix.pop();
        }
        prefix.pop();
    }
}

/// Whether a row only the record has is one every record of its kind has.
fn record_only_by_kind(record: &Entry, pair: &super::align::TurnPair) -> Option<ByKind> {
    match record {
        Entry::Interrupted | Entry::Ended { .. } => Some(ByKind::EitriTurnEnding),
        Entry::User {
            text: t,
            via: Via::Prompt,
        } if pair.how == TurnMatch::EitriOnly && pair.entries.len() == 1 && is_cli_local_command(t) => {
            Some(ByKind::EitriLocalCommand)
        }
        _ => None,
    }
}

/// The commands the pinned CLI runs itself, without the model, and their aliases: the list Eitri's
/// composer keeps as `CLI_LOCAL_COMMANDS` in `eitri-core` (which this crate cannot depend on), less the
/// ones the CLI was recorded writing into the session file (`/clear` with its aliases, `/compact`), whose
/// absence there is a real difference. A command missing here shows as a difference, never hidden.
const CLI_LOCAL_COMMANDS: &[&str] = &[
    "__remote-workflow",
    "add-dir",
    "advisor",
    "agents",
    "auto-mode-setup",
    "autocompact",
    "claim-credit",
    "color",
    "config",
    "settings",
    "context",
    "design-consent",
    "design-revoke",
    "effort",
    "exit",
    "extra-usage",
    "fast",
    "focus",
    "goal",
    "heapdump",
    "import",
    "install-slack-app",
    "keybindings",
    "list-agents",
    "low-priority",
    "peers",
    "mcp",
    "model",
    "output-style",
    "pause-memory",
    "memory-pause",
    "toggle-memory",
    "plugin-types",
    "radio",
    "recap",
    "reload-plugins",
    "reload-skills",
    "rewind",
    "checkpoint",
    "undo",
    "rename",
    "name",
    "skill-doctor",
    "stickers",
    "stop",
    "ultrareview",
    "update",
    "restart",
    "usage",
    "cost",
    "stats",
    "usage-credits",
    "version",
    "voice",
    "workflow-launch-exec",
];

/// Whether `t` runs one of [`CLI_LOCAL_COMMANDS`], argument or not, by the CLI's own parse: the text
/// trimmed, a leading `/`, the name up to the first whitespace, matched exactly. A prompt command or a
/// skill (`/review`, `/init`) goes to the model, so its prompt reaches the CLI and is never allowed here.
fn is_cli_local_command(t: &str) -> bool {
    let Some(rest) = t.trim().strip_prefix('/') else {
        return false;
    };
    let name = rest.split(char::is_whitespace).next().unwrap_or_default();
    CLI_LOCAL_COMMANDS.contains(&name)
}

/// The name of the call a result answers, from whichever side holds the call.
fn tool_name_of(a: &Alignment, pair: &EntryPair, id: &str) -> Option<String> {
    let find = |turns: &[Turn]| {
        turns.iter().flat_map(|t| t.entries.iter()).find_map(|e| match e {
            Entry::ToolUse { id: call, name, .. } if call == id => Some(name.clone()),
            _ => None,
        })
    };
    match pair.cli {
        Some(_) => find(&a.cli),
        None => find(&a.eitri),
    }
}

fn entry_text(entry: &Entry) -> Option<String> {
    match entry {
        Entry::User { text: t, .. } => Some(user_text(t)),
        Entry::Assistant { text: t } | Entry::Thinking { text: t } => Some(text(t)),
        Entry::ToolResult { content, .. } => Some(text(&result_text(content))),
        Entry::ToolUse { input, .. } => Some(super::normalise::canonical_json(input)),
        _ => None,
    }
}

impl Difference {
    /// The entry this difference names, for a test's message.
    pub fn describe(&self) -> String {
        format!(
            "{} {} {} at {:?}{}",
            source_word(self.source),
            self.marker.symbol(),
            self.kind,
            self.at,
            if self.fields.is_empty() {
                String::new()
            } else {
                format!(
                    " ({})",
                    self.fields.iter().map(field_word).collect::<Vec<_>>().join(", ")
                )
            }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_commands_the_cli_runs_itself_are_local_commands() {
        assert!(is_cli_local_command("/model"));
        assert!(is_cli_local_command("/effort high"));
        assert!(
            !is_cli_local_command("/clear"),
            "the CLI writes a /clear into its session file"
        );
        assert!(
            !is_cli_local_command("/compact keep the plan"),
            "the CLI writes a /compact into its session file"
        );
        assert!(!is_cli_local_command("/review"), "a prompt command reaches the model");
        assert!(!is_cli_local_command("/my-skill do it"), "a skill reaches the model");
        assert!(!is_cli_local_command("/"));
        assert!(!is_cli_local_command("/home/x is a path"));
        assert!(!is_cli_local_command("not a command"));
    }
}
