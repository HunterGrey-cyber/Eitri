//! A card the broker raises itself, with no provider permission request behind it. Its lines are plain text:
//! everything on a card may come from a repository or the agent, so the page shows each through
//! `revealHidden`, never as Markdown.

use serde::{Deserialize, Serialize};

use super::broker::AnswerSeal;

/// One line of a card, by what it is. Serialised as `{kind, text}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "text", rename_all = "lowercase")]
pub enum CardLine {
    Plain(String),
    Code(String),
    #[serde(rename = "agent")]
    AgentWords(String),
    Warning(String),
}

#[derive(Debug, Default)]
pub struct CardRequest {
    pub title: String,
    pub body: Vec<CardLine>,
    /// A digest of what the card shows, kept with the parked card and handed back on its answer.
    pub binding: Option<[u8; 32]>,
    /// The id of the tab's proposal this card would apply, for the card's view of each file.
    pub proposal: Option<String>,
}

impl CardRequest {
    pub fn new(title: impl Into<String>, body: Vec<CardLine>) -> CardRequest {
        CardRequest {
            title: title.into(),
            body,
            binding: None,
            proposal: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardDecision {
    Allow,
    Deny,
}

/// How the person answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardOrigin {
    Key,
    Click,
}

/// Made only by the broker's `answer_card`: no public constructor and no public field, so a value is proof of a
/// person's answer on that card's own id.
#[derive(Debug)]
pub struct CardAnswer {
    decision: CardDecision,
    origin: CardOrigin,
    card_id: String,
    call_id: String,
    answered_ms: u64,
    binding: Option<[u8; 32]>,
}

impl CardAnswer {
    /// The broker's alone: nothing else in the crate can make an [`AnswerSeal`].
    pub(super) fn sealed(
        _seal: AnswerSeal,
        decision: CardDecision,
        origin: CardOrigin,
        card_id: String,
        call_id: String,
        answered_ms: u64,
        binding: Option<[u8; 32]>,
    ) -> CardAnswer {
        CardAnswer {
            decision,
            origin,
            card_id,
            call_id,
            answered_ms,
            binding,
        }
    }

    /// An answer to `request` for a run driven with no broker, carrying the request's `binding` as the broker
    /// carries the parked card's. No product build has it.
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn for_tests(
        request: &CardRequest,
        decision: CardDecision,
        origin: CardOrigin,
        call_id: &str,
    ) -> CardAnswer {
        CardAnswer {
            decision,
            origin,
            card_id: "ec-driven-1".to_owned(),
            call_id: call_id.to_owned(),
            answered_ms: super::stamp::wall_ms(),
            binding: request.binding,
        }
    }

    pub fn allowed(&self) -> bool {
        self.decision == CardDecision::Allow
    }

    pub fn origin(&self) -> CardOrigin {
        self.origin
    }

    pub fn card_id(&self) -> &str {
        &self.card_id
    }

    pub fn call_id(&self) -> &str {
        &self.call_id
    }

    /// The wall clock taken in the broker's `answer_card`: when the person answered, not when the queue admitted the run.
    pub fn answered_ms(&self) -> u64 {
        self.answered_ms
    }

    /// The `binding` of the card this answers, from the parked card, never from the page.
    pub fn binding(&self) -> Option<&[u8; 32]> {
        self.binding.as_ref()
    }
}

/// A `CardAnswer` cannot be written out, so holding one means the broker made it. The paths are the ones
/// [`a_card_answer_is_read_through_these_paths`] proves good, so this fails for the private fields and nothing else.
///
/// ```compile_fail
/// use eitri_core::editor_tools::cards::{CardAnswer, CardDecision, CardOrigin};
/// let _forged = CardAnswer {
///     decision: CardDecision::Allow,
///     origin: CardOrigin::Key,
///     card_id: String::new(),
///     call_id: String::new(),
///     answered_ms: 0,
///     binding: None,
/// };
/// ```
#[cfg(doctest)]
fn a_card_answer_exists_only_through_answer_card() {}

/// Neither constructor is reachable from outside the crate, with `test-support` on or off.
///
/// ```compile_fail
/// use eitri_core::editor_tools::cards::{CardAnswer, CardDecision, CardOrigin, CardRequest};
/// let request = CardRequest::new("title", vec![]);
/// let _forged = CardAnswer::for_tests(&request, CardDecision::Allow, CardOrigin::Key, "call");
/// ```
#[cfg(doctest)]
fn a_card_answer_has_no_constructor_outside_the_crate() {}

/// ```
/// use eitri_core::editor_tools::cards::{CardAnswer, CardDecision, CardOrigin, CardRequest};
/// fn read(answer: &CardAnswer) -> bool {
///     answer.allowed()
/// }
/// let _ = (read, CardDecision::Allow, CardOrigin::Key, CardRequest::new("title", vec![]));
/// ```
#[cfg(doctest)]
fn a_card_answer_is_read_through_these_paths() {}

/// The proposal a card would apply, as the page needs it to show each file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CardProposal {
    pub id: String,
    pub files: Vec<String>,
}

/// A card as it reaches the page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EditorCardWire {
    /// `ec-<16 hex of the broker's random value>-<n>`, one per raised card, never a call id.
    pub id: String,
    /// The tab's `last_revision` at raise time, the number the next event would take, so the page's numbering
    /// stays Rust's.
    pub seq: u64,
    pub tool: String,
    pub tool_use_id: Option<String>,
    pub title: String,
    pub body: Vec<CardLine>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proposal: Option<CardProposal>,
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../../../agent-ui/web/src/fixtures/editor-cards.json");

    fn card(n: u64, tool_use_id: Option<&str>, title: &str, line: CardLine) -> EditorCardWire {
        EditorCardWire {
            id: format!("ec-0123456789abcdef-{n}"),
            seq: 40 + n,
            tool: "open_file".to_owned(),
            tool_use_id: tool_use_id.map(str::to_owned),
            title: title.to_owned(),
            body: vec![line],
            proposal: None,
        }
    }

    #[test]
    fn the_card_wire_matches_the_shared_fixture() {
        let cards = vec![
            card(
                1,
                Some("toolu_01"),
                "Load src/a.rs in your editor?",
                CardLine::Plain("It is not open there.".to_owned()),
            ),
            card(
                2,
                Some("toolu_02"),
                "Show /etc/hosts in your editor?",
                CardLine::Code("/etc/hosts".to_owned()),
            ),
            card(
                3,
                None,
                "Read /etc/hosts from your editor?",
                CardLine::AgentWords("I need the host table.".to_owned()),
            ),
            card(
                4,
                None,
                "Run this in your editor?",
                CardLine::Warning("It is outside the project.".to_owned()),
            ),
            EditorCardWire {
                tool: "apply_proposal".to_owned(),
                proposal: Some(CardProposal {
                    id: "p-3".to_owned(),
                    files: vec!["src/a.rs".to_owned(), "src/b.rs".to_owned()],
                }),
                ..card(
                    5,
                    Some("toolu_05"),
                    "Apply rename foo to bar to 2 files?",
                    CardLine::Plain("Files: src/a.rs, src/b.rs".to_owned()),
                )
            },
        ];
        let written = serde_json::to_string_pretty(&cards).unwrap() + "\n";
        assert_eq!(written, FIXTURE);
        let read: Vec<EditorCardWire> = serde_json::from_str(FIXTURE).unwrap();
        assert_eq!(read, cards);
    }

    #[test]
    fn a_request_binds_nothing_unless_it_says_so() {
        let request = CardRequest::new("title", vec![]);
        assert_eq!(request.title, "title");
        assert!(request.binding.is_none());
        assert!(CardRequest::default().binding.is_none());
    }

    #[test]
    fn a_request_names_no_proposal_unless_it_says_so() {
        assert!(CardRequest::new("title", vec![]).proposal.is_none());
        assert!(CardRequest::default().proposal.is_none());
    }
}
