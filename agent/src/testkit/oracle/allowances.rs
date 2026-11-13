//! Known gaps allowed by id until fixed.
//! Scenario allowances live in `allowances/<scenario>.json`; shared ones live in `_all.json`.
//! Each entry has `{id, scenario, source, matcher, reason}`. The scenario is the file's name
//! or `*` for shared entries. The source is `E`, `A` or `B`; the matcher selects differences,
//! and the reason explains the gap.
//! Ids must be canonical names from `allowances/_ids.json` ([`KnownIds`]) or harness-only ids
//! for CLI differences outside the oracle's inputs. Duplicate names are rejected with the
//! canonical id to use. Several allowances can match one difference.
//! An allowance must match a difference whenever its source is computed, so fixing a gap
//! requires removing its allowance. Sources not computed are not checked; replay computes E only.
//! Differences common to every entry of a kind are handled in code with explanations in
//! [`super::report::ByKind`] and [`super::report::projected_out_reason`].

use super::report::{field_word, origin_word, turn_match_word, via_word, Difference, Marker};
use super::report::{FIELD_WORDS, KIND_WORDS, ORIGIN_WORDS, TURN_MATCH_WORDS, VIA_WORDS};
use super::{Entry, Origin, Source, Turn, Via};
use serde::Deserialize;
use std::path::{Path, PathBuf};

/// The ids an allowance may carry, read from `allowances/_ids.json`: `{"canonical": [...], "duplicates":
/// {"<name>": "<canonical id>", ...}, "harness_only": {"<id>": "<what it covers>", ...}}`. The list lives beside
/// the allowances, with the gaps it names.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct KnownIds {
    /// What the file is, for its reader.
    #[serde(default)]
    pub about: String,
    pub canonical: Vec<String>,
    /// Names that repeat a canonical gap, with the id that stands for both.
    #[serde(default)]
    pub duplicates: std::collections::BTreeMap<String, String>,
    /// Ids for differences that are no gap of Eitri's, each with what it covers. They are kept apart from the
    /// gap ids, which other ledgers share, so that a ledger of Eitri's gaps never has to carry them.
    #[serde(default)]
    pub harness_only: std::collections::BTreeMap<String, String>,
}

/// The file listing the ids an allowance may carry.
pub const IDS_FILE: &str = "_ids.json";

impl KnownIds {
    pub fn parse(text: &str) -> Result<Self, AllowanceError> {
        let ids: KnownIds =
            serde_json::from_str(text).map_err(|e| AllowanceError(format!("{IDS_FILE}: not a list of ids: {e}")))?;
        for (dup, canonical) in &ids.duplicates {
            if !ids.canonical.contains(canonical) {
                return Err(AllowanceError(format!(
                    "{IDS_FILE}: {dup} stands for {canonical}, which is not a canonical id"
                )));
            }
            if ids.canonical.contains(dup) {
                return Err(AllowanceError(format!(
                    "{IDS_FILE}: {dup} is both canonical and a repeat"
                )));
            }
        }
        for (id, covers) in &ids.harness_only {
            if ids.canonical.contains(id) || ids.duplicates.contains_key(id) {
                return Err(AllowanceError(format!(
                    "{IDS_FILE}: {id} is a harness-only id and also a gap's"
                )));
            }
            if covers.trim().is_empty() {
                return Err(AllowanceError(format!(
                    "{IDS_FILE}: the harness-only id {id} does not say what it covers"
                )));
            }
        }
        Ok(ids)
    }

    /// Whether `id` may name an allowance, and if not, why.
    pub fn check(&self, id: &str) -> Result<(), String> {
        if let Some(canonical) = self.duplicates.get(id) {
            return Err(format!("allowance {id} repeats another gap: use {canonical}"));
        }
        if self.canonical.iter().any(|c| c == id) || self.harness_only.contains_key(id) {
            Ok(())
        } else {
            Err(format!("allowance {id} is not a known gap's canonical id"))
        }
    }
}

/// Which differences an allowance covers: every field given must hold.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Matcher {
    /// `-` (only in the CLI), `+` (only in the record) or `!` (paired, differing).
    pub marker: Option<String>,
    /// The entry's kind, or `turn` for a turn the record kept inside another.
    pub kind: Option<String>,
    /// The origin of the CLI turn it sits in.
    pub origin: Option<String>,
    /// How a user entry reached the conversation.
    pub via: Option<String>,
    /// How its turn paired.
    pub turn_match: Option<String>,
    /// A field that differs.
    pub field: Option<String>,
    /// The tool of a call or of the call a result answers.
    pub tool: Option<String>,
    /// A piece of the entry's normalised text (a call's canonical input, a result's text).
    pub text_contains: Option<String>,
    /// Inside a subagent's transcript, or not.
    pub nested: Option<bool>,
    /// Whether the CLI's entry is the first of its turn (the message that opened the turn) or one taken into a
    /// turn already running. A difference that names no CLI entry holds neither.
    pub opens_turn: Option<bool>,
}

impl Matcher {
    /// The fields the matcher states, as the JSON an allowance file holds them in: what a report names when an
    /// allowance matched nothing, so the entry to delete can be told from the others of its id.
    pub fn as_json(&self) -> String {
        let mut fields = serde_json::Map::new();
        let words = [
            ("marker", &self.marker),
            ("kind", &self.kind),
            ("origin", &self.origin),
            ("via", &self.via),
            ("turn_match", &self.turn_match),
            ("field", &self.field),
            ("tool", &self.tool),
            ("text_contains", &self.text_contains),
        ];
        for (name, value) in words {
            if let Some(value) = value {
                fields.insert(name.to_owned(), serde_json::Value::String(value.clone()));
            }
        }
        if let Some(nested) = self.nested {
            fields.insert("nested".to_owned(), serde_json::Value::Bool(nested));
        }
        if let Some(opens_turn) = self.opens_turn {
            fields.insert("opens_turn".to_owned(), serde_json::Value::Bool(opens_turn));
        }
        serde_json::Value::Object(fields).to_string()
    }

    fn validate(&self) -> Result<(), String> {
        let check = |name: &str, value: &Option<String>, words: &[&str]| -> Result<(), String> {
            match value {
                Some(v) if !words.contains(&v.as_str()) => {
                    Err(format!("matcher {name} {v:?} is not one of {}", words.join(", ")))
                }
                _ => Ok(()),
            }
        };
        if *self == Matcher::default() {
            return Err("matcher is empty: it would cover every difference".to_owned());
        }
        check("marker", &self.marker, &["-", "+", "!"])?;
        check("kind", &self.kind, KIND_WORDS)?;
        check("origin", &self.origin, ORIGIN_WORDS)?;
        check("via", &self.via, VIA_WORDS)?;
        check("turn_match", &self.turn_match, TURN_MATCH_WORDS)?;
        check("field", &self.field, FIELD_WORDS)?;
        if self.text_contains.as_deref() == Some("") {
            return Err("matcher text_contains is empty".to_owned());
        }
        Ok(())
    }

    /// Whether this matcher covers `d`.
    pub fn matches(&self, d: &Difference) -> bool {
        let is = |want: &Option<String>, have: Option<&str>| want.as_deref().is_none_or(|w| Some(w) == have);
        is(&self.marker, Some(d.marker.symbol()))
            && is(&self.kind, Some(d.kind))
            && is(&self.origin, d.origin.as_ref().map(origin_word))
            && is(&self.via, d.via.as_ref().map(via_word))
            && is(&self.turn_match, Some(turn_match_word(&d.turn_match)))
            && self
                .field
                .as_deref()
                .is_none_or(|f| d.marker == Marker::Differs && d.fields.iter().any(|x| field_word(x) == f))
            && is(&self.tool, d.tool.as_deref())
            && self
                .text_contains
                .as_deref()
                .is_none_or(|piece| d.text.as_deref().is_some_and(|t| t.contains(piece)))
            && self.nested.is_none_or(|n| n == d.nested)
            && self.opens_turn.is_none_or(|o| d.opens_turn == Some(o))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAllowance {
    id: String,
    scenario: String,
    source: String,
    matcher: Matcher,
    reason: String,
    #[serde(default)]
    shape: Option<String>,
}

/// One allowance, checked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Allowance {
    pub id: String,
    /// The scenario it names, or `*`.
    pub scenario: String,
    pub source: Source,
    pub matcher: Matcher,
    pub reason: String,
    /// One of [`SHAPES`]: the allowance covers a difference that exists only when the CLI's turns have that shape
    /// (the CLI decides how it takes two notifications that arrive together). A run whose turns do not have the
    /// shape does not have the difference; a run whose turns have it must match it.
    pub shape: Option<String>,
}

/// A refused allowance file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AllowanceError(pub String);

impl std::fmt::Display for AllowanceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for AllowanceError {}

/// The allowances that apply to one scenario.
#[derive(Clone, Debug, Default)]
pub struct AllowanceSet {
    scenario: String,
    entries: Vec<Allowance>,
}

/// Where the allowance files live.
pub fn allowances_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/claude_scenarios/allowances")
}

/// The shapes of the CLI's turns an allowance may be conditioned on (see [`shapes_of`]).
pub const SHAPES: &[&str] = &[
    "replies-adjacent-across-turns",
    "notification-turn",
    "peer-message-mid-turn",
];

/// Which of [`SHAPES`] the CLI's turns have. `replies-adjacent-across-turns`: two replies of the CLI that no
/// entry but a task notification separates, the second in a later turn than the first. A restore of the files
/// reads them as one reply, since no turn start falls between them. `notification-turn`: a turn a task
/// notification opened. A recording that ends before the CLI hands its last notification over has none, and
/// the same scenario resumed has one. `peer-message-mid-turn`: another session's message the CLI
/// delivered into a running turn (a peer's entry that does not open its turn), which no record of Eitri's
/// holds as a row.
pub fn shapes_of(cli: &[Turn]) -> Vec<&'static str> {
    let mut previous: Option<(usize, bool)> = None;
    let mut shapes = Vec::new();
    if cli.iter().any(|t| t.origin == Origin::TaskNotification) {
        shapes.push(SHAPES[1]);
    }
    if cli.iter().any(|t| {
        t.entries
            .iter()
            .skip(1)
            .any(|e| matches!(e, Entry::User { via: Via::Peer, .. }))
    }) {
        shapes.push(SHAPES[2]);
    }
    for (at, turn) in cli.iter().enumerate() {
        for entry in &turn.entries {
            match entry {
                Entry::User {
                    via: Via::TaskNotification,
                    ..
                } => continue,
                Entry::Assistant { .. } => {
                    if previous.is_some_and(|(turn_of_reply, is_reply)| is_reply && turn_of_reply != at) {
                        shapes.push(SHAPES[0]);
                    }
                    previous = Some((at, true));
                }
                _ => previous = Some((at, false)),
            }
        }
    }
    shapes.sort_unstable();
    shapes.dedup();
    shapes
}

/// The file holding the allowances for every scenario.
pub const ALL_SCENARIOS_FILE: &str = "_all.json";

fn parse_file(
    text: &str,
    file: &str,
    scenario: Option<&str>,
    ids: &KnownIds,
) -> Result<Vec<Allowance>, AllowanceError> {
    let fail = |message: String| AllowanceError(format!("{file}: {message}"));
    let raw: Vec<RawAllowance> =
        serde_json::from_str(text).map_err(|e| fail(format!("not a list of allowances: {e}")))?;
    let mut out = Vec::new();
    for entry in raw {
        let id = entry.id;
        ids.check(&id).map_err(fail)?;
        match scenario {
            Some(s) if entry.scenario != s => {
                return Err(fail(format!(
                    "allowance {id} names scenario {:?}; this file holds scenario {s}'s",
                    entry.scenario
                )));
            }
            None if entry.scenario != "*" => {
                return Err(fail(format!(
                    "allowance {id} names scenario {:?}; this file holds the allowances of every scenario (\"*\")",
                    entry.scenario
                )));
            }
            _ => {}
        }
        let source = match entry.source.as_str() {
            "E" => Source::E,
            "A" => Source::A,
            "B" => Source::B,
            other => return Err(fail(format!("allowance {id}: source {other:?} is not E, A or B"))),
        };
        entry
            .matcher
            .validate()
            .map_err(|m| fail(format!("allowance {id}: {m}")))?;
        if let Some(shape) = entry.shape.as_deref().filter(|s| !SHAPES.contains(s)) {
            return Err(fail(format!(
                "allowance {id}: shape {shape:?} is not one of {}",
                SHAPES.join(", ")
            )));
        }
        if entry.reason.trim().is_empty() {
            return Err(fail(format!("allowance {id} gives no reason")));
        }
        out.push(Allowance {
            id,
            scenario: entry.scenario,
            source,
            matcher: entry.matcher,
            reason: entry.reason,
            shape: entry.shape,
        });
    }
    Ok(out)
}

impl AllowanceSet {
    /// No allowances, for a run that is held to the records as they are.
    pub fn none(scenario: &str) -> Self {
        Self {
            scenario: scenario.to_owned(),
            entries: Vec::new(),
        }
    }

    /// The allowances of `scenario` from the texts of its file and of `_all.json` (either may be absent),
    /// their ids checked against `ids`.
    pub fn from_texts(
        scenario: &str,
        ids: &KnownIds,
        scenario_file: Option<&str>,
        all_file: Option<&str>,
    ) -> Result<Self, AllowanceError> {
        let mut entries = Vec::new();
        if let Some(text) = scenario_file {
            entries.extend(parse_file(text, &format!("{scenario}.json"), Some(scenario), ids)?);
        }
        if let Some(text) = all_file {
            entries.extend(parse_file(text, ALL_SCENARIOS_FILE, None, ids)?);
        }
        Ok(Self {
            scenario: scenario.to_owned(),
            entries,
        })
    }

    /// The allowances of `scenario` from `dir`.
    pub fn load(dir: &Path, scenario: &str) -> Result<Self, AllowanceError> {
        let read = |name: &str| -> Result<Option<String>, AllowanceError> {
            match std::fs::read_to_string(dir.join(name)) {
                Ok(text) => Ok(Some(text)),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(AllowanceError(format!("{name}: {e}"))),
            }
        };
        let ids = KnownIds::parse(&read(IDS_FILE)?.ok_or_else(|| AllowanceError(format!("{IDS_FILE}: missing")))?)?;
        let own = read(&format!("{scenario}.json"))?;
        let all = read(ALL_SCENARIOS_FILE)?;
        Self::from_texts(scenario, &ids, own.as_deref(), all.as_deref())
    }

    /// The allowances of one run of `scenario`: a variant or a numbered run that differs from the scenario's
    /// base run has a file of its own, `<scenario>_<run>.json`, whose entries name `<scenario>_<run>`; a run
    /// without one is held to the scenario's file. A file's allowances must all match in the run it is read for,
    /// so a difference only one run has cannot sit in the file the others read.
    pub fn for_run(scenario: &str, run: Option<&str>) -> Result<Self, AllowanceError> {
        let dir = allowances_dir();
        let name = run
            .map(|run| format!("{scenario}_{run}"))
            .filter(|name| dir.join(format!("{name}.json")).exists())
            .unwrap_or_else(|| scenario.to_owned());
        Self::load(&dir, &name)
    }

    /// The allowances that apply to a run whose CLI turns have the shapes `seen`: those naming no shape, and those
    /// naming one of them.
    pub fn observing(mut self, seen: &[&str]) -> Self {
        self.entries
            .retain(|a| a.shape.as_deref().is_none_or(|shape| seen.contains(&shape)));
        self
    }

    pub fn scenario(&self) -> &str {
        &self.scenario
    }

    pub fn entries(&self) -> &[Allowance] {
        &self.entries
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids() -> KnownIds {
        KnownIds::parse(r#"{"canonical": ["gap-one", "gap-two"], "duplicates": {"gap-again": "gap-one"}}"#).unwrap()
    }

    #[test]
    fn ids_are_canonical_only() {
        let ids = ids();
        assert!(ids.check("gap-one").is_ok());
        assert!(ids.check("made-up").is_err());
        let dup = ids.check("gap-again").unwrap_err();
        assert!(dup.contains("use gap-one"), "{dup}");
        assert!(KnownIds::parse(r#"{"canonical": ["a"], "duplicates": {"b": "c"}}"#).is_err());
    }

    #[test]
    fn a_harness_only_id_is_accepted_apart_from_the_gaps_and_says_what_it_covers() {
        let ids = KnownIds::parse(r#"{"canonical": ["gap-one"], "harness_only": {"cli-files-it-so": "why"}}"#).unwrap();
        assert!(ids.check("cli-files-it-so").is_ok());
        assert!(!ids.canonical.iter().any(|c| c == "cli-files-it-so"));
        assert!(KnownIds::parse(r#"{"canonical": ["a"], "harness_only": {"a": "why"}}"#).is_err());
        assert!(KnownIds::parse(r#"{"canonical": ["a"], "harness_only": {"b": " "}}"#).is_err());
    }

    #[test]
    fn an_allowance_that_names_a_shape_applies_only_to_a_run_whose_turns_have_it() {
        let ids = ids();
        let entry = |extra: &str| {
            format!(
                r#"[{{"id": "gap-one", "scenario": "c01c", "source": "B",
                      "matcher": {{"marker": "-"}}, "reason": "a race the CLI decides"{extra}}}]"#
            )
        };
        let load = |extra: &str| AllowanceSet::from_texts("c01c", &ids, Some(&entry(extra)), None).unwrap();
        assert_eq!(load("").entries()[0].shape, None);
        assert_eq!(load("").observing(&[]).entries().len(), 1);
        let named = load(r#", "shape": "replies-adjacent-across-turns""#);
        assert_eq!(
            named.entries()[0].shape.as_deref(),
            Some("replies-adjacent-across-turns")
        );
        assert_eq!(
            named
                .clone()
                .observing(&["replies-adjacent-across-turns"])
                .entries()
                .len(),
            1
        );
        assert!(named.observing(&[]).entries().is_empty());
        let unknown = entry(r#", "shape": "made-up""#);
        assert!(AllowanceSet::from_texts("c01c", &ids, Some(&unknown), None).is_err());
    }

    #[test]
    fn replies_with_only_notifications_between_them_are_adjacent_across_turns() {
        let reply = |text: &str| Entry::Assistant { text: text.to_owned() };
        let notified = || Entry::User {
            text: "<task-notification/>".to_owned(),
            via: Via::TaskNotification,
        };
        let call = || Entry::ToolUse {
            id: "toolu_1".to_owned(),
            name: "Bash".to_owned(),
            input: serde_json::Value::Null,
        };
        let turns = |shape: Vec<Vec<Entry>>| -> Vec<Turn> {
            shape
                .into_iter()
                .enumerate()
                .map(|(n, entries)| {
                    let mut turn = Turn::new(n + 1, Origin::Unknown);
                    turn.entries = entries;
                    turn
                })
                .collect()
        };
        // A notification turn with no reply of its own, then one that answers: the restore joins the replies.
        let empty_first = turns(vec![
            vec![reply("one")],
            vec![notified()],
            vec![notified(), reply("two")],
        ]);
        assert_eq!(shapes_of(&empty_first), ["replies-adjacent-across-turns"]);
        // A call between the replies keeps them apart.
        let call_between = turns(vec![vec![reply("one")], vec![notified(), call(), reply("two")]]);
        assert!(shapes_of(&call_between).is_empty());
        // Two replies of one turn are one turn's own business.
        let one_turn = turns(vec![vec![reply("one"), reply("two")]]);
        assert!(shapes_of(&one_turn).is_empty());
    }

    #[test]
    fn a_turn_a_notification_opened_is_its_own_shape() {
        let turn = |origin: Origin, entries: Vec<Entry>| {
            let mut turn = Turn::new(1, origin);
            turn.entries = entries;
            turn
        };
        let prompt = || Entry::User {
            text: "go".to_owned(),
            via: Via::Prompt,
        };
        let notified = || Entry::User {
            text: "<task-notification/>".to_owned(),
            via: Via::TaskNotification,
        };
        assert!(shapes_of(&[turn(Origin::Human, vec![prompt()])]).is_empty());
        assert_eq!(
            shapes_of(&[
                turn(Origin::Human, vec![prompt()]),
                turn(Origin::TaskNotification, vec![notified()])
            ]),
            ["notification-turn"]
        );
    }

    #[test]
    fn a_peer_message_delivered_into_a_running_turn_is_its_own_shape() {
        let turn = |origin: Origin, entries: Vec<Entry>| {
            let mut turn = Turn::new(1, origin);
            turn.entries = entries;
            turn
        };
        let user = |via: Via| Entry::User {
            text: "words".to_owned(),
            via,
        };
        let reply = || Entry::Assistant { text: "ok".to_owned() };
        // A turn another session's message opened is not a delivery into a running turn.
        assert!(shapes_of(&[turn(Origin::Peer, vec![user(Via::Peer), reply()])]).is_empty());
        assert_eq!(
            shapes_of(&[turn(Origin::Human, vec![user(Via::Prompt), user(Via::Peer), reply()])]),
            ["peer-message-mid-turn"]
        );
    }

    #[test]
    fn files_are_checked_entry_by_entry() {
        let ids = ids();
        let ok = r#"[{"id": "gap-two", "scenario": "c16w", "source": "E",
                      "matcher": {"marker": "-", "kind": "tool_use", "tool": "Workflow"}, "reason": "invisible"}]"#;
        assert_eq!(
            AllowanceSet::from_texts("c16w", &ids, Some(ok), None)
                .unwrap()
                .entries()
                .len(),
            1
        );
        let wrong_scenario = ok.replace("\"c16w\"", "\"c01\"");
        assert!(AllowanceSet::from_texts("c16w", &ids, Some(&wrong_scenario), None).is_err());
        assert!(
            AllowanceSet::from_texts("c16w", &ids, None, Some(ok)).is_err(),
            "the shared file holds only allowances for every scenario"
        );
        for bad in [
            ok.replace("\"gap-two\"", "\"gap-again\""),
            ok.replace("\"E\"", "\"X\""),
            ok.replace("\"invisible\"", "\" \""),
            ok.replace("\"kind\": \"tool_use\"", "\"kind\": \"tool\""),
            ok.replace("\"tool\": \"Workflow\"", "\"tool\": \"Workflow\", \"colour\": \"red\""),
            ok.replace(
                "{\"marker\": \"-\", \"kind\": \"tool_use\", \"tool\": \"Workflow\"}",
                "{}",
            ),
        ] {
            assert!(
                AllowanceSet::from_texts("c16w", &ids, Some(&bad), None).is_err(),
                "{bad}"
            );
        }
    }
}
