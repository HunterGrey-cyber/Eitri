//! How the layer (a) host answers the permission requests of a gated session.
//!
//! Every Claude session is gated: the sidecar asks the host about each tool call (a hook request) and, for the
//! checks the CLI keeps to itself, relays the CLI's own prompt as a second request for the same call. Eitri's
//! real host answers from its permission policy; this crate cannot reach that policy, so the layer (a) host
//! answers by the scenario's mode alone:
//!
//! | mode   | hook request | CLI prompt |
//! |--------|--------------|------------|
//! | bypass | `Allow`      | `Allow`    |
//! | auto   | `Defer`      | the driver |
//! | manual | the driver   | the driver |
//!
//! Auto defers every hook request because a session in the sandbox has no saved rules: the CLI's classifier
//! then sees a superset of what Eitri would defer. A request that needs the driver waits for an
//! `answer_card` of the same tool; a request still waiting when the driver is on any other wait fails that
//! wait, naming the tool.

use super::script::Mode;
use crate::provider::PermissionDecision;
use serde_json::{json, Value};

/// Where a request came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    /// The sidecar's gate, which asks about every call.
    Hook,
    /// The CLI's own prompt, raised after the gate had answered.
    CliPrompt,
}

impl Origin {
    pub fn word(self) -> &'static str {
        match self {
            Self::Hook => "hook",
            Self::CliPrompt => "cli_prompt",
        }
    }
}

/// One permission request as the host saw it.
#[derive(Clone, Debug, PartialEq)]
pub struct Request {
    pub permission_id: String,
    pub tool_use_id: Option<String>,
    pub tool_name: String,
    pub input: Value,
    pub origin: Origin,
}

/// Who gave an answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// The mode's own rule.
    Automatic,
    /// The driver's `answer_card`.
    Driver,
}

/// A request and what became of it.
#[derive(Clone, Debug, PartialEq)]
pub struct Answered {
    pub request: Request,
    /// `None` while the request waits for the driver.
    pub decision: Option<PermissionDecision>,
}

impl Answered {
    pub fn tool(&self) -> &str {
        &self.request.tool_name
    }

    pub fn to_json(&self) -> Value {
        json!({
            "permission_id": self.request.permission_id,
            "tool_use_id": self.request.tool_use_id,
            "tool": self.request.tool_name,
            "origin": self.request.origin.word(),
            "decision": self.decision.as_ref().map(decision_word),
        })
    }
}

/// The word a decision is logged and compared by.
pub fn decision_word(decision: &PermissionDecision) -> &'static str {
    match decision {
        PermissionDecision::Allow => "allow",
        PermissionDecision::Deny { .. } => "deny",
        PermissionDecision::Defer => "defer",
    }
}

/// The answer a mode gives by itself, or `None` when only the driver can answer.
pub fn automatic(mode: Mode, origin: Origin) -> Option<PermissionDecision> {
    match (mode, origin) {
        (Mode::Bypass, _) => Some(PermissionDecision::Allow),
        (Mode::Auto, Origin::Hook) => Some(PermissionDecision::Defer),
        (Mode::Auto, Origin::CliPrompt) | (Mode::Manual, _) => None,
    }
}

/// An answer to send now.
#[derive(Clone, Debug, PartialEq)]
pub struct ToSend {
    pub request: Request,
    pub decision: PermissionDecision,
    pub source: Source,
}

/// The state of every request of one session: what was answered by rule, what by the driver, what waits.
#[derive(Debug)]
pub struct Gate {
    mode: Mode,
    /// Requests the driver answers, in arrival order, with the answer once it is given.
    cards: Vec<Answered>,
    /// Requests the mode answered, in arrival order.
    automatic: Vec<Answered>,
    /// Driver answers given before their request arrived: (tool, allow), oldest first.
    armed: Vec<(String, bool)>,
}

impl Gate {
    pub fn new(mode: Mode) -> Self {
        Self {
            mode,
            cards: Vec::new(),
            automatic: Vec::new(),
            armed: Vec::new(),
        }
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// A request arrived. Returns the answer to send now, if there is one: the mode's rule, or an armed
    /// driver answer for the same tool.
    pub fn on_request(&mut self, request: Request) -> Option<ToSend> {
        if let Some(decision) = automatic(self.mode, request.origin) {
            self.automatic.push(Answered {
                request: request.clone(),
                decision: Some(decision.clone()),
            });
            return Some(ToSend {
                request,
                decision,
                source: Source::Automatic,
            });
        }
        let armed = self.armed.iter().position(|(tool, _)| *tool == request.tool_name);
        let decision = armed.map(|i| decide(self.armed.remove(i).1));
        self.cards.push(Answered {
            request: request.clone(),
            decision: decision.clone(),
        });
        decision.map(|decision| ToSend {
            request,
            decision,
            source: Source::Driver,
        })
    }

    /// The driver answers a card of `tool`. The oldest waiting request of that tool is answered now; with none
    /// waiting the answer is kept for the next one.
    pub fn driver_answer(&mut self, tool: &str, allow: bool) -> Option<ToSend> {
        let waiting = self
            .cards
            .iter_mut()
            .find(|c| c.decision.is_none() && c.request.tool_name == tool);
        match waiting {
            Some(card) => {
                let decision = decide(allow);
                card.decision = Some(decision.clone());
                Some(ToSend {
                    request: card.request.clone(),
                    decision,
                    source: Source::Driver,
                })
            }
            None => {
                self.armed.push((tool.to_owned(), allow));
                None
            }
        }
    }

    /// Answers every waiting request `Allow` (the "approve all pending" path).
    pub fn approve_all(&mut self) -> Vec<ToSend> {
        self.cards
            .iter_mut()
            .filter(|c| c.decision.is_none())
            .map(|card| {
                card.decision = Some(PermissionDecision::Allow);
                ToSend {
                    request: card.request.clone(),
                    decision: PermissionDecision::Allow,
                    source: Source::Driver,
                }
            })
            .collect()
    }

    /// The oldest request that waits for a driver answer, if any.
    pub fn unanswered(&self) -> Option<&Request> {
        self.cards.iter().find(|c| c.decision.is_none()).map(|c| &c.request)
    }

    /// Every request that needed the driver, answered or not.
    pub fn cards_shown(&self) -> Vec<Answered> {
        self.cards.clone()
    }

    /// Every request the mode answered by itself.
    pub fn automatic_answers(&self) -> Vec<Answered> {
        self.automatic.clone()
    }
}

fn decide(allow: bool) -> PermissionDecision {
    if allow {
        PermissionDecision::Allow
    } else {
        PermissionDecision::Deny {
            reason: Some("denied by the harness driver".into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(id: &str, tool: &str, origin: Origin) -> Request {
        Request {
            permission_id: id.into(),
            tool_use_id: Some(format!("toolu_{id}")),
            tool_name: tool.into(),
            input: json!({}),
            origin,
        }
    }

    #[test]
    fn bypass_allows_hook_requests_and_cli_prompts() {
        for origin in [Origin::Hook, Origin::CliPrompt] {
            let mut gate = Gate::new(Mode::Bypass);
            let sent = gate
                .on_request(request("p1", "Bash", origin))
                .expect("answered at once");
            assert_eq!(sent.decision, PermissionDecision::Allow);
            assert_eq!(sent.source, Source::Automatic);
            assert_eq!(gate.automatic_answers().len(), 1);
            assert!(gate.cards_shown().is_empty());
            assert!(gate.unanswered().is_none());
        }
    }

    #[test]
    fn auto_defers_hook_requests_and_leaves_cli_prompts_to_the_driver() {
        let mut gate = Gate::new(Mode::Auto);
        let sent = gate
            .on_request(request("p1", "Bash", Origin::Hook))
            .expect("deferred at once");
        assert_eq!(sent.decision, PermissionDecision::Defer);
        assert_eq!(sent.source, Source::Automatic);
        assert!(gate.on_request(request("p2", "Bash", Origin::CliPrompt)).is_none());
        assert_eq!(gate.unanswered().map(|r| r.permission_id.as_str()), Some("p2"));
        assert_eq!(gate.automatic_answers().len(), 1);
        assert_eq!(gate.cards_shown().len(), 1);
        let sent = gate
            .driver_answer("Bash", true)
            .expect("the waiting prompt is answered");
        assert_eq!(sent.request.permission_id, "p2");
        assert_eq!(sent.source, Source::Driver);
        assert!(gate.unanswered().is_none());
    }

    #[test]
    fn manual_leaves_both_origins_to_the_driver() {
        for origin in [Origin::Hook, Origin::CliPrompt] {
            let mut gate = Gate::new(Mode::Manual);
            assert!(gate.on_request(request("p1", "Write", origin)).is_none());
            assert_eq!(gate.unanswered().map(|r| r.tool_name.as_str()), Some("Write"));
            assert!(gate.automatic_answers().is_empty());
            let sent = gate.driver_answer("Write", false).expect("answered");
            assert!(matches!(sent.decision, PermissionDecision::Deny { .. }));
            assert_eq!(gate.cards_shown()[0].decision, Some(sent.decision));
        }
    }

    #[test]
    fn a_manual_request_with_no_driver_answer_stays_unanswered_and_names_its_tool() {
        let mut gate = Gate::new(Mode::Manual);
        assert!(gate.on_request(request("p1", "Bash", Origin::Hook)).is_none());
        assert!(
            gate.driver_answer("Edit", true).is_none(),
            "an answer for another tool does not apply"
        );
        assert_eq!(gate.unanswered().map(|r| r.tool_name.as_str()), Some("Bash"));
    }

    #[test]
    fn an_answer_given_first_is_kept_for_the_next_request_of_its_tool() {
        let mut gate = Gate::new(Mode::Manual);
        assert!(gate.driver_answer("Bash", true).is_none());
        let sent = gate
            .on_request(request("p1", "Bash", Origin::Hook))
            .expect("armed answer applies");
        assert_eq!(sent.decision, PermissionDecision::Allow);
        assert_eq!(sent.source, Source::Driver);
        assert_eq!(gate.cards_shown().len(), 1);
        assert!(
            gate.on_request(request("p2", "Bash", Origin::Hook)).is_none(),
            "used once"
        );
    }

    #[test]
    fn approve_all_answers_every_waiting_request() {
        let mut gate = Gate::new(Mode::Manual);
        gate.on_request(request("p1", "Bash", Origin::Hook));
        gate.on_request(request("p2", "Edit", Origin::Hook));
        let sent = gate.approve_all();
        assert_eq!(sent.len(), 2);
        assert!(gate.unanswered().is_none());
        assert!(gate.approve_all().is_empty());
    }

    #[test]
    fn answered_requests_are_logged_with_their_decision_word() {
        let mut gate = Gate::new(Mode::Manual);
        gate.on_request(request("p1", "Bash", Origin::CliPrompt));
        assert_eq!(gate.cards_shown()[0].to_json()["decision"], Value::Null);
        gate.driver_answer("Bash", true);
        let line = gate.cards_shown()[0].to_json();
        assert_eq!(line["decision"], "allow");
        assert_eq!(line["origin"], "cli_prompt");
        assert_eq!(line["tool"], "Bash");
    }
}
