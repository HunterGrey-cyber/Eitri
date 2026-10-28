//! Known gaps allowed by id until fixed.
//! Scenario allowances live in `allowances/<scenario>.json`; shared ones live in `_all.json`.
//! Each entry has `{id, scenario, source, matcher, reason}`. The scenario is the file's name
//! or `*` for shared entries. The source is `E`, `A` or `B`; the matcher selects differences,
//! and the reason explains the gap.
//! Ids must be canonical names from `allowances/_ids.json` ([`KnownIds`]); duplicate names
//! are rejected with the canonical id to use. Several allowances can match one difference.
//! An allowance must match a difference whenever its source is computed, so fixing a gap
//! requires removing its allowance. Sources not computed are not checked; replay computes E only.
//! Differences common to every entry of a kind are handled in code with explanations in
//! [`super::report::ByKind`] and [`super::report::projected_out_reason`].

use super::report::{field_word, origin_word, turn_match_word, via_word, Difference, Marker};
use super::report::{FIELD_WORDS, KIND_WORDS, ORIGIN_WORDS, TURN_MATCH_WORDS, VIA_WORDS};
use super::Source;
use serde::Deserialize;
use std::path::{Path, PathBuf};

/// The ids an allowance may carry, read from `allowances/_ids.json`: `{"canonical": [...], "duplicates":
/// {"<name>": "<canonical id>", ...}}`. The list lives beside the allowances, with the gaps it names.
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
        Ok(ids)
    }

    /// Whether `id` may name an allowance, and if not, why.
    pub fn check(&self, id: &str) -> Result<(), String> {
        if let Some(canonical) = self.duplicates.get(id) {
            return Err(format!("allowance {id} repeats another gap: use {canonical}"));
        }
        if self.canonical.iter().any(|c| c == id) {
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
}

impl Matcher {
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
                )))
            }
            None if entry.scenario != "*" => {
                return Err(fail(format!(
                    "allowance {id} names scenario {:?}; this file holds the allowances of every scenario (\"*\")",
                    entry.scenario
                )))
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
        if entry.reason.trim().is_empty() {
            return Err(fail(format!("allowance {id} gives no reason")));
        }
        out.push(Allowance {
            id,
            scenario: entry.scenario,
            source,
            matcher: entry.matcher,
            reason: entry.reason,
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

    /// The allowances of `scenario` from the tree's allowance directory.
    pub fn for_scenario(scenario: &str) -> Result<Self, AllowanceError> {
        Self::load(&allowances_dir(), scenario)
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
