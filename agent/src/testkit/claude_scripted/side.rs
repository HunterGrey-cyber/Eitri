//! Requests the CLI makes beside the conversation, and the policy for the betas and body fields it sends.
use serde_json::Value;

use super::matchers::View;

/// Opening of the auto-mode classifier's system prompt.
pub const AUTO_CLASSIFIER_OPENING: &str = "You are a security monitor for autonomous AI coding agents.";
/// The two compaction prompts, a full and a partial one.
pub const COMPACTION_PROMPTS: [&str; 2] = [
    "create a detailed summary of the conversation so far",
    "create a detailed summary of this conversation",
];
/// The small-model request that labels a group of tool calls. Whether a session driven through the SDK sends it is
/// not yet confirmed, so the row is listed but unconfirmed.
pub const TOOL_USE_SUMMARY_PROMPT: &str = "Write a short summary label describing what these tool calls accomplished";
pub const TOOL_USE_SUMMARY_CONFIRMED: bool = false;

/// The attribution header line of a subagent's requests.
pub const SUBAGENT_MARK: &str = "cc_is_subagent=true";

/// Top-level body fields the stand-in itself reads; they need no table entry.
pub const BASE_FIELDS: [&str; 5] = ["model", "messages", "system", "tools", "stream"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SideKind {
    CountTokens,
    CacheTouch,
    Compaction,
    ToolUseSummary,
    AutoClassifier,
    SmallFast,
}

impl SideKind {
    pub fn name(self) -> &'static str {
        match self {
            Self::CountTokens => "count_tokens",
            Self::CacheTouch => "cache_touch",
            Self::Compaction => "compaction",
            Self::ToolUseSummary => "tool_use_summary",
            Self::AutoClassifier => "auto_classifier",
            Self::SmallFast => "small_fast",
        }
    }
}

/// What the stand-in does with a beta or a body field.
#[derive(Clone, Copy, Debug)]
pub enum Policy {
    /// Handled: ignored when it changes nothing the stand-in answers, parsed where it must match on it.
    Accept { reason: &'static str },
    /// Answered with the status and JSON body the CLI's healing ladder reacts to, so that the CLI drops the beta for
    /// the rest of the conversation. A refused beta is listed in the run manifest.
    Refuse {
        status: u16,
        body: &'static str,
        reason: &'static str,
    },
}

/// The answer that makes the CLI drop the server-side classifier beta: the CLI looks for its header name in a 400's
/// message, then resends without the beta and the `safeguards` field, and auto mode decides with the local
/// classifier for the rest of the conversation.
const SERVER_CLASSIFIER_REFUSAL: &str = r#"{"type":"error","error":{"type":"invalid_request_error","message":"Unexpected value(s) `dangerous-tool-use-2026-09-03` for the `anthropic-beta` header. Please consult our documentation at docs.anthropic.com or try again without the header."}}"#;

/// Betas the CLI 2.1.311 sends to a custom base URL, each with the stand-in's decision. Every accepted beta only
/// changes the shape of what the CLI sends or asks for; the stand-in's plain answers are valid under all of them.
pub const BETA_POLICY: &[(&str, Policy)] = &[
    (
        "claude-code-20250219",
        Policy::Accept {
            reason: "identifies the client; changes no answer",
        },
    ),
    (
        "interleaved-thinking-2025-05-14",
        Policy::Accept {
            reason: "lets thinking blocks interleave with tool use; the stand-in streams thinking only when scripted",
        },
    ),
    (
        "context-management-2025-06-27",
        Policy::Accept {
            reason: "adds the context_management body field (a clear-thinking edit); the stand-in ignores it",
        },
    ),
    (
        "per-turn-control-2026-07-01",
        Policy::Accept {
            reason: "per-turn effort control; the stand-in reads no effort",
        },
    ),
    (
        "mid-conversation-tool-changes-2026-07-01",
        Policy::Accept {
            reason: "tool changes and the environment text travel as mid-conversation system messages; the matchers read the whole tail in any role",
        },
    ),
    (
        "thinking-token-count-2026-05-13",
        Policy::Accept {
            reason: "asks for thinking token counts in usage; the stand-in's fixed usage is accepted",
        },
    ),
    (
        "advanced-tool-use-2025-11-20",
        Policy::Accept {
            reason: "tool search and deferred tool loading; the declared-tool check reads defer_loading and the name-only announcement",
        },
    ),
    (
        "effort-2025-11-24",
        Policy::Accept {
            reason: "adds output_config.effort; the stand-in ignores it",
        },
    ),
    (
        "dangerous-tool-use-2026-09-03",
        Policy::Refuse {
            status: 400,
            body: SERVER_CLASSIFIER_REFUSAL,
            reason: "the server classifier would judge tool use inside the main request and the stand-in has no classifier; refused, the CLI uses its local classifier, whose side requests the scenario scripts and counts",
        },
    ),
    (
        "thinking-display-updates-2026-08-18",
        Policy::Accept {
            reason: "asks for thinking display updates (thinking.display); the stand-in streams plain thinking deltas",
        },
    ),
    (
        "fallback-credit-2026-06-01",
        Policy::Accept {
            reason: "sent on a compaction request; asks the server to credit a fallback, which the stand-in never takes",
        },
    ),
    (
        "afk-mode-2026-01-31",
        Policy::Accept {
            reason: "marks the session as unattended for the server; changes no answer",
        },
    ),
];

/// Top-level body fields the CLI 2.1.311 sends beside the ones the stand-in reads, each with the stand-in's decision.
pub const FIELD_POLICY: &[(&str, Policy)] = &[
    (
        "context_management",
        Policy::Accept {
            reason: "a clear-thinking edit list; no effect on a scripted answer",
        },
    ),
    (
        "max_tokens",
        Policy::Accept {
            reason: "the output limit; scripted replies are far shorter",
        },
    ),
    (
        "metadata",
        Policy::Accept {
            reason: "device and session ids; the stand-in does not read them",
        },
    ),
    (
        "output_config",
        Policy::Accept {
            reason: "effort setting; no effect on a scripted answer",
        },
    ),
    (
        "thinking",
        Policy::Accept {
            reason: "adaptive thinking with display updates; the stand-in streams thinking only when scripted",
        },
    ),
    (
        "stop_sequences",
        Policy::Accept {
            reason: "the classifier's requests stop at the severity tag; the stand-in answers the whole text and the CLI reads it the same",
        },
    ),
    (
        "safeguards",
        Policy::Refuse {
            status: 400,
            body: SERVER_CLASSIFIER_REFUSAL,
            reason: "carries the server classifier's context and goes with the dangerous-tool-use beta, which is refused",
        },
    ),
];

fn lookup(table: &'static [(&'static str, Policy)], name: &str) -> Option<&'static Policy> {
    table.iter().find(|(n, _)| *n == name).map(|(_, p)| p)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Finding {
    /// A beta or field no table entry covers.
    Unlisted { kind: &'static str, value: String },
    /// A beta or field the table refuses.
    Refused {
        kind: &'static str,
        value: String,
        status: u16,
        body: Value,
    },
}

/// Every beta in the `anthropic-beta` header and every top-level body field, checked against the tables.
pub fn check(beta_header: &str, body: &Value) -> Vec<Finding> {
    let mut findings = Vec::new();
    let mut one = |kind: &'static str, value: &str, policy: Option<&'static Policy>| match policy {
        Some(Policy::Accept { .. }) => {}
        Some(Policy::Refuse { status, body, .. }) => findings.push(Finding::Refused {
            kind,
            value: value.to_owned(),
            status: *status,
            body: serde_json::from_str(body).unwrap_or(Value::Null),
        }),
        None => findings.push(Finding::Unlisted {
            kind,
            value: value.to_owned(),
        }),
    };
    for beta in beta_header.split(',').map(str::trim).filter(|b| !b.is_empty()) {
        one("beta", beta, lookup(BETA_POLICY, beta));
    }
    if let Some(fields) = body.as_object() {
        for field in fields.keys().filter(|f| !BASE_FIELDS.contains(&f.as_str())) {
            one("field", field, lookup(FIELD_POLICY, field));
        }
    }
    findings
}

/// The side row that claims a `/v1/messages` request, if any. The two path rows are decided by the caller.
/// `small_model` is the run's small model: a request for it that no other row fits is `SmallFast`.
pub fn classify(view: &View, small_model: Option<&str>) -> Option<SideKind> {
    let system = view.system_text();
    let user = view.tail_user_text();
    if COMPACTION_PROMPTS
        .iter()
        .any(|p| system.contains(p) || user.contains(p))
    {
        return Some(SideKind::Compaction);
    }
    if system.contains(TOOL_USE_SUMMARY_PROMPT) || user.contains(TOOL_USE_SUMMARY_PROMPT) {
        return Some(SideKind::ToolUseSummary);
    }
    if system.contains(AUTO_CLASSIFIER_OPENING) {
        return Some(SideKind::AutoClassifier);
    }
    // A subagent started with the haiku alias asks the small model too; the billing header marks it as a subagent
    // and its requests go to the scenario's steps.
    if small_model == Some(view.model()) && !system.contains(SUBAGENT_MARK) {
        return Some(SideKind::SmallFast);
    }
    None
}

pub fn compaction_text(n: usize) -> String {
    format!("<analysis>scripted</analysis>\n<summary>SCRIPTED SUMMARY {n}</summary>")
}

pub fn label_text(n: usize) -> String {
    format!("SCRIPTED LABEL {n}")
}

/// Which question a classifier request asks. The CLI asks the severity first and, when it reads one above its
/// threshold, asks again for the decision that blocks or lets the call through.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClassifierStage {
    /// "Respond with <severity>N</severity> ONLY".
    Severity,
    /// The second question: reasoning in `<thinking>` first, then `<severity>N</severity>` (above 50 blocks), plus
    /// `<category>` naming the rule when it blocks.
    Block,
}

/// The line that closes a severity request's user text.
const SEVERITY_ASK: &str = "Respond with <severity>N</severity> ONLY";

pub fn classifier_stage(view: &View) -> ClassifierStage {
    if view.tail_user_text().contains(SEVERITY_ASK) {
        ClassifierStage::Severity
    } else {
        ClassifierStage::Block
    }
}

/// A classifier answer in the format of the stage's own prompt. A severity of 0 allows and one of 90 sends the CLI
/// on to the second question, which is answered with the reason as the reasoning, the same severity, and, to block,
/// a rule category. A second answer in any other form is one the CLI cannot read: it blocks the call as
/// unevaluated and asks again.
pub fn verdict_text(stage: ClassifierStage, allow: bool, reason: &str) -> String {
    match (stage, allow) {
        (ClassifierStage::Severity, true) => "<severity>0</severity>".to_owned(),
        (ClassifierStage::Severity, false) => "<severity>90</severity><category>Scripted Block</category>".to_owned(),
        (ClassifierStage::Block, true) => format!("<thinking>{reason}</thinking><severity>0</severity>"),
        (ClassifierStage::Block, false) => {
            format!("<thinking>{reason}</thinking><severity>90</severity><category>Scripted Block</category>")
        }
    }
}

/// The first 120 characters of the system text, for a side request no row fits.
pub fn quote(view: &View) -> String {
    view.system_text().chars().take(120).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn view(body: Value) -> View {
        View::new("/v1/messages", &body)
    }

    #[test]
    fn rows_are_recognised_by_system_or_user_text_and_model() {
        let compaction = view(json!({"model":"m-main","messages":[
            {"role":"user","content":"hi"},
            {"role":"user","content":"Your task is to create a detailed summary of the conversation so far."}]}));
        assert_eq!(classify(&compaction, Some("m-small")), Some(SideKind::Compaction));
        let partial = view(
            json!({"model":"m-main","system":"Your task is to create a detailed summary of this conversation",
            "messages":[{"role":"user","content":"hi"}]}),
        );
        assert_eq!(classify(&partial, None), Some(SideKind::Compaction));
        let label = view(json!({"model":"m-small","messages":[
            {"role":"user","content":"Write a short summary label describing what these tool calls accomplished: x"}]}));
        assert_eq!(classify(&label, Some("m-small")), Some(SideKind::ToolUseSummary));
        let classifier = view(json!({"model":"m-main","system":[
            {"type":"text","text":"x-anthropic-billing-header: cc_version=1;"},
            {"type":"text","text":"You are a security monitor for autonomous AI coding agents. Decide."}],
            "messages":[{"role":"user","content":"x"}]}));
        assert_eq!(classify(&classifier, Some("m-small")), Some(SideKind::AutoClassifier));
        let small = view(json!({"model":"m-small","messages":[{"role":"user","content":"x"}]}));
        assert_eq!(classify(&small, Some("m-small")), Some(SideKind::SmallFast));
        assert_eq!(classify(&small, None), None);
        let main = view(json!({"model":"m-main","messages":[{"role":"user","content":"x"}]}));
        assert_eq!(classify(&main, Some("m-small")), None);
        let small_subagent = view(json!({"model":"m-small","system":[
            {"type":"text","text":"x-anthropic-billing-header: cc_version=1; cc_is_subagent=true;"}],
            "messages":[{"role":"user","content":"x"}]}));
        assert_eq!(classify(&small_subagent, Some("m-small")), None);
    }

    #[test]
    fn a_compaction_prompt_in_old_history_is_not_a_compaction_request() {
        let v = view(json!({"model":"m-main","messages":[
            {"role":"user","content":"create a detailed summary of the conversation so far"},
            {"role":"assistant","content":"<summary>x</summary>"},
            {"role":"user","content":"carry on"}]}));
        assert_eq!(classify(&v, None), None);
    }

    #[test]
    fn a_beta_or_field_no_table_entry_covers_is_listed_and_the_base_fields_never_are() {
        let body = json!({"model":"m","messages":[],"system":"s","tools":[],"stream":true,"max_tokens":5,
            "metadata":{},"surprise_field":1});
        let found = check("claude-code-20250219, surprise-beta-2030-01-01", &body);
        let values: Vec<_> = found
            .iter()
            .map(|f| match f {
                Finding::Unlisted { kind, value } => format!("{kind}:{value}"),
                Finding::Refused { .. } => unreachable!(),
            })
            .collect();
        assert_eq!(values, ["beta:surprise-beta-2030-01-01", "field:surprise_field"]);
        assert!(check("", &json!({"model":"m","messages":[],"stream":true})).is_empty());
    }

    #[test]
    fn the_server_classifier_beta_and_field_are_refused_with_a_message_naming_the_header() {
        let body = json!({"model":"m","messages":[],"stream":true,"safeguards":[]});
        let found = check("claude-code-20250219, dangerous-tool-use-2026-09-03", &body);
        assert_eq!(found.len(), 2, "{found:?}");
        for f in found {
            let Finding::Refused { status, body, .. } = f else {
                panic!("{f:?}")
            };
            assert_eq!(status, 400);
            assert!(body["error"]["message"]
                .as_str()
                .unwrap()
                .contains("dangerous-tool-use-2026-09-03"));
        }
    }

    #[test]
    fn every_policy_entry_states_its_reason_and_names_are_unique() {
        for table in [BETA_POLICY, FIELD_POLICY] {
            let mut names: Vec<&str> = table.iter().map(|(n, _)| *n).collect();
            names.sort_unstable();
            names.dedup();
            assert_eq!(names.len(), table.len(), "a name is listed twice");
            for (name, policy) in table {
                let reason = match policy {
                    Policy::Accept { reason } | Policy::Refuse { reason, .. } => reason,
                };
                assert!(reason.len() > 12, "{name} has no reason");
            }
        }
    }

    #[test]
    fn answer_texts_carry_their_counters() {
        assert!(compaction_text(3).contains("SCRIPTED SUMMARY 3"));
        assert_eq!(label_text(2), "SCRIPTED LABEL 2");
        assert_eq!(
            verdict_text(ClassifierStage::Block, true, "ok"),
            "<thinking>ok</thinking><severity>0</severity>"
        );
        assert_eq!(
            verdict_text(ClassifierStage::Block, false, "outside"),
            "<thinking>outside</thinking><severity>90</severity><category>Scripted Block</category>"
        );
        assert_eq!(
            verdict_text(ClassifierStage::Severity, true, "ok"),
            "<severity>0</severity>"
        );
        assert!(verdict_text(ClassifierStage::Severity, false, "no").starts_with("<severity>90</severity>"));
    }
}
