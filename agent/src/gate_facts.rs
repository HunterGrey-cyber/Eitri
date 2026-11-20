//! A remote tab's copy of the server's path facts for its pending gate requests.
//!
//! The helper computes a fact (`permission_policy::gate_fact`, the disk half) for each gate request its session's
//! Pending holds and streams them (`Gate.WatchFacts`); before any automatic answer the laptop asks for a fresh one
//! (`Gate.CheckFact`). What arrives is a server's word: every fact is decoded here against this build's own reasons
//! and bounds, and kept only for a request whose digest, computed here from the event's own bytes, it names. A fact
//! that cannot be read, or names another request, is a refusal, which the gate turns into a card.
//!
//! The cache holds the facts the current stream sent and nothing from before a reconnect: a link that drops clears
//! them, and the next stream sends every fact the helper holds again.

use crate::permission_policy::{
    known_reason, request_digest, Classification, GateFact, PermissionVerdict, REASON_EITRI_OWN_PLACES,
    REASON_EITRI_PLACE_UNKNOWN, REASON_SERVER_FACT_UNKNOWN,
};
use crate::project_boundary::NotABoundary;
use claude_runtime_protocol::v1::{session_event::Event, SessionEvent};
use eitri_server_proto::{
    CheckFactResponse, GateFactMsg, GateRequestFact, GateRootFact, GateVerdict, GATE_FACTS_HELD_MAX, GATE_REASON_MAX,
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How long a gate request whose decision reads the server's fact waits for it after delivery; then it is a card.
pub const GATE_FACT_WAIT: Duration = Duration::from_secs(2);

/// How long the fresh check before an automatic answer may take; past it the check is unavailable.
pub const GATE_FRESH_WAIT: Duration = Duration::from_secs(3);

/// The largest message the laptop decodes from the `Gate` service. A fact names one permission id, three short
/// reasons, a digest and a refusal; a fresh check two facts. This leaves room for every field at its bound.
pub const GATE_MESSAGE_MAX: usize = 16 * 1024;

/// Why a fact is refused when it names another request than the one held under its id.
pub const REFUSED_FOR_ANOTHER_REQUEST: &str = "the server's fact is for another request";
/// Why a fact is refused when it could not be read: a field missing or past its bound.
pub const REFUSED_UNREADABLE: &str = "the server's fact could not be read";
/// Why a fact is refused when more facts arrived than the laptop keeps for one session.
pub const REFUSED_TOO_MANY: &str = "more facts arrived than this Eitri keeps for one session";

/// A fact the server sent for a request, decoded, with the digest it names and when it came.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceivedFact {
    pub fact: GateFact,
    pub request_digest: [u8; 32],
    pub received: Instant,
}

/// What a remote tab knows of the server's fact for one pending request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateFactState {
    /// The tab's agent runs on this machine: the local classifier judges.
    NotRemote,
    /// The tab's helper does not serve facts this build can use (no `gate-facts`, or a helper of another build).
    NoCapability,
    /// No fact for the request yet.
    Waiting,
    /// The fact, for exactly this request.
    Have(Arc<ReceivedFact>),
    /// No usable fact, and why, in this build's words (with any server text made plain).
    Refused(String),
}

/// The answer to a fresh check before an automatic answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FreshFact {
    /// The fact the helper held and one it computed for this check, both for the request asked about.
    Both { first: GateFact, fresh: GateFact },
    /// The helper refused, or sent something that cannot be used; why.
    Refused(String),
    /// No answer: the link, the time limit, or a tab without facts.
    Unavailable,
}

/// The server's word on the tab's root: whether it is a boundary, and when not, which refusal, when this build
/// knows its name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RootFact {
    pub is_boundary: bool,
    pub not_a_boundary: Option<NotABoundary>,
}

/// Server text as a plain line: control and bidirectional formatting characters replaced, at most `max` chars.
fn plain(text: &str, max: usize) -> String {
    let mut out: String = text
        .chars()
        .take(max)
        .map(|ch| {
            let bidi = matches!(ch, '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}');
            if ch.is_control() || bidi {
                '?'
            } else {
                ch
            }
        })
        .collect();
    if text.chars().nth(max).is_some() {
        out.push_str("...");
    }
    out
}

/// A refusal the server sent, as this build shows it.
fn server_refusal(text: &str) -> String {
    format!("the server's path check gave no fact: {}", plain(text, 200))
}

fn verdict(sent: &GateVerdict) -> Result<Classification, String> {
    if sent.why.len() > GATE_REASON_MAX {
        return Err(REFUSED_UNREADABLE.to_owned());
    }
    let reason = known_reason(&sent.why).ok_or_else(|| REASON_SERVER_FACT_UNKNOWN.to_owned())?;
    Ok(Classification {
        verdict: if sent.passes {
            PermissionVerdict::AllowWithoutAsking
        } else {
            PermissionVerdict::AskTheUser
        },
        reason,
    })
}

/// A fact as the server sent it, decoded against this build's own reasons and bounds, with the digest it names.
pub fn decode_fact(sent: &GateFactMsg) -> Result<(GateFact, [u8; 32]), String> {
    let digest: [u8; 32] = sent
        .request_digest
        .as_slice()
        .try_into()
        .map_err(|_| REFUSED_UNREADABLE.to_owned())?;
    let classification = verdict(
        sent.classification
            .as_ref()
            .ok_or_else(|| REFUSED_UNREADABLE.to_owned())?,
    )?;
    let places_card = match &sent.places_card {
        None => None,
        Some(card) => {
            let card = verdict(card)?;
            // The places' check gives only these two.
            if ![REASON_EITRI_OWN_PLACES, REASON_EITRI_PLACE_UNKNOWN].contains(&card.reason) {
                return Err(REFUSED_UNREADABLE.to_owned());
            }
            Some(card)
        }
    };
    let fact = GateFact {
        root_is_boundary: sent.root_is_boundary,
        places_card,
        classification,
    };
    Ok((fact, digest))
}

/// The server's root fact, decoded; a refusal name this build does not know names none.
pub fn decode_root(sent: &GateRootFact) -> RootFact {
    RootFact {
        is_boundary: sent.is_boundary,
        not_a_boundary: (!sent.is_boundary)
            .then(|| NotABoundary::from_name(&sent.not_a_boundary))
            .flatten(),
    }
}

/// A fresh check's answer for the request `digest` names.
pub fn decode_fresh(answer: &CheckFactResponse, digest: &[u8; 32]) -> FreshFact {
    let decoded = |sent: &Option<GateFactMsg>| -> Result<GateFact, String> {
        let (fact, named) = decode_fact(sent.as_ref().ok_or_else(|| REFUSED_UNREADABLE.to_owned())?)?;
        if named != *digest {
            return Err(REFUSED_FOR_ANOTHER_REQUEST.to_owned());
        }
        Ok(fact)
    };
    if !answer.refusal.is_empty() {
        return FreshFact::Refused(server_refusal(&answer.refusal));
    }
    match (decoded(&answer.first), decoded(&answer.fresh)) {
        (Ok(first), Ok(fresh)) => FreshFact::Both { first, fresh },
        (Err(why), _) | (_, Err(why)) => FreshFact::Refused(why),
    }
}

/// What the stream said about one request.
#[derive(Debug, Clone)]
enum Held {
    Fact { sequence: u64, fact: Arc<ReceivedFact> },
    Refused { sequence: u64, why: String },
}

/// One session's facts as the laptop holds them.
#[derive(Debug)]
pub struct FactCache {
    session_id: String,
    /// The root the helper names requests by: the sidecar's recorded root.
    root: PathBuf,
    /// Each pending gate request's sequence and digest, computed from its event's own bytes.
    expected: HashMap<String, (u64, [u8; 32])>,
    held: HashMap<String, Held>,
    root_fact: Option<RootFact>,
}

impl FactCache {
    pub fn new(session_id: &str, root: &Path) -> FactCache {
        FactCache {
            session_id: session_id.to_owned(),
            root: root.to_path_buf(),
            expected: HashMap::new(),
            held: HashMap::new(),
            root_fact: None,
        }
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Follows a session event as it is delivered: a permission request is named by its digest, an answer drops
    /// its request's fact, a turn's end or the session's close drops them all. Changes nothing for other events.
    pub fn observe(&mut self, event: &SessionEvent) {
        match &event.event {
            // The CLI's own prompt has no fact.
            Some(Event::PermissionRequested(request)) if crate::permission_policy::is_gate_request(request.origin) => {
                if self.expected.len() >= GATE_FACTS_HELD_MAX && !self.expected.contains_key(&request.permission_id) {
                    return;
                }
                let digest = request_digest(
                    &self.session_id,
                    &request.permission_id,
                    &request.tool_name,
                    request.input_json.as_bytes(),
                    &self.root,
                );
                self.expected
                    .insert(request.permission_id.clone(), (event.sequence, digest));
            }
            Some(Event::PermissionResolved(resolved)) => {
                self.expected.remove(&resolved.permission_id);
                self.held.remove(&resolved.permission_id);
            }
            Some(Event::TurnCompleted(_) | Event::SessionClosed(_)) => {
                self.expected.clear();
                self.held.clear();
            }
            _ => {}
        }
    }

    /// Keeps a fact the stream sent. A fact for a request not yet delivered is kept until it is; past the bound a
    /// fact for a new id is a refusal for nothing but that id's later lookup, which then finds none and waits.
    pub fn receive(&mut self, sent: &GateRequestFact) {
        if crate::remote_answers::checked_id("a fact's permission id", &sent.permission_id).is_err() {
            return;
        }
        if self.held.len() >= GATE_FACTS_HELD_MAX && !self.held.contains_key(&sent.permission_id) {
            return;
        }
        let held = match &sent.fact {
            None => Held::Refused {
                sequence: sent.sequence,
                why: server_refusal(&sent.refusal),
            },
            Some(message) => match decode_fact(message) {
                Ok((fact, request_digest)) => Held::Fact {
                    sequence: sent.sequence,
                    fact: Arc::new(ReceivedFact {
                        fact,
                        request_digest,
                        received: Instant::now(),
                    }),
                },
                Err(why) => Held::Refused {
                    sequence: sent.sequence,
                    why,
                },
            },
        };
        self.held.insert(sent.permission_id.clone(), held);
    }

    pub fn receive_root(&mut self, sent: &GateRootFact) {
        self.root_fact = Some(decode_root(sent));
    }

    /// The link dropped: nothing the old stream sent is used again. The requests' digests stay, since their
    /// events are not delivered twice.
    pub fn disconnected(&mut self) {
        self.held.clear();
        self.root_fact = None;
    }

    pub fn root_fact(&self) -> Option<RootFact> {
        self.root_fact
    }

    /// The fact for the pending request `permission_id`, for exactly that request.
    pub fn state(&self, permission_id: &str) -> GateFactState {
        let Some((sequence, digest)) = self.expected.get(permission_id) else {
            return match self.expected.len() >= GATE_FACTS_HELD_MAX {
                true => GateFactState::Refused(REFUSED_TOO_MANY.to_owned()),
                false => GateFactState::Waiting,
            };
        };
        match self.held.get(permission_id) {
            None => GateFactState::Waiting,
            // A fact about the same id at another sequence is about an earlier or later request; the one for
            // this request is still to come.
            Some(Held::Fact { sequence: at, .. } | Held::Refused { sequence: at, .. }) if at != sequence => {
                GateFactState::Waiting
            }
            Some(Held::Fact { fact, .. }) if fact.request_digest == *digest => GateFactState::Have(fact.clone()),
            Some(Held::Fact { .. }) => GateFactState::Refused(REFUSED_FOR_ANOTHER_REQUEST.to_owned()),
            Some(Held::Refused { why, .. }) => GateFactState::Refused(why.clone()),
        }
    }

    /// The digest of the pending request `permission_id`, as this machine computed it from its event.
    pub fn expected_digest(&self, permission_id: &str) -> Option<[u8; 32]> {
        self.expected.get(permission_id).map(|(_, digest)| *digest)
    }

    #[cfg(test)]
    fn held_len(&self) -> usize {
        self.held.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permission_policy::{
        classify_permission_request, REASON_ALLOWED_BY_A_PROJECT_RULE, REASON_NOT_ON_READ_ONLY_LIST,
    };
    use claude_runtime_protocol::v1::{PermissionRequested, PermissionResolved, TurnCompleted};

    const ROOT: &str = "/srv/project";

    fn requested(sequence: u64, id: &str, input: &str) -> SessionEvent {
        SessionEvent {
            sequence,
            event: Some(Event::PermissionRequested(PermissionRequested {
                permission_id: id.into(),
                tool_name: "Read".into(),
                input_json: input.into(),
                ..Default::default()
            })),
            ..Default::default()
        }
    }

    fn sent_verdict(classification: Classification) -> GateVerdict {
        GateVerdict {
            passes: !classification.needs_a_human(),
            why: classification.reason.to_owned(),
        }
    }

    fn message(digest: [u8; 32], classification: Classification) -> GateFactMsg {
        GateFactMsg {
            root_is_boundary: true,
            places_card: None,
            classification: Some(sent_verdict(classification)),
            request_digest: digest.to_vec(),
            computed_unix_ms: 1,
        }
    }

    fn fact_for(sequence: u64, id: &str, message: GateFactMsg) -> GateRequestFact {
        GateRequestFact {
            permission_id: id.into(),
            sequence,
            fact: Some(message),
            refusal: String::new(),
        }
    }

    /// An allow the disk half gives (a tool allowed by name).
    fn inside() -> Classification {
        let allowed = classify_permission_request("TodoWrite", &serde_json::json!({}), Path::new(ROOT));
        assert!(!allowed.needs_a_human());
        allowed
    }

    fn ask(reason: &'static str) -> Classification {
        Classification {
            verdict: PermissionVerdict::AskTheUser,
            reason,
        }
    }

    fn digest(id: &str, input: &str) -> [u8; 32] {
        request_digest("s1", id, "Read", input.as_bytes(), Path::new(ROOT))
    }

    #[test]
    fn facts_arrive_with_their_requests_and_leave_with_them() {
        let mut cache = FactCache::new("s1", Path::new(ROOT));
        let input = r#"{"file_path":"a.txt"}"#;
        // A fact may come before its request is delivered: it waits for it.
        cache.receive(&fact_for(3, "p1", message(digest("p1", input), inside())));
        assert_eq!(cache.state("p1"), GateFactState::Waiting);
        cache.observe(&requested(3, "p1", input));
        let GateFactState::Have(have) = cache.state("p1") else {
            panic!("{:?}", cache.state("p1"));
        };
        assert_eq!(have.fact.classification, inside());
        assert_eq!(cache.expected_digest("p1"), Some(digest("p1", input)));
        // An answer drops it.
        cache.observe(&SessionEvent {
            sequence: 4,
            event: Some(Event::PermissionResolved(PermissionResolved {
                permission_id: "p1".into(),
                ..Default::default()
            })),
            ..Default::default()
        });
        assert_eq!(cache.state("p1"), GateFactState::Waiting);
        assert_eq!(cache.held_len(), 0);
        // A turn's end drops them all.
        cache.observe(&requested(5, "p2", input));
        cache.receive(&fact_for(5, "p2", message(digest("p2", input), inside())));
        assert!(matches!(cache.state("p2"), GateFactState::Have(_)));
        cache.observe(&SessionEvent {
            sequence: 6,
            event: Some(Event::TurnCompleted(TurnCompleted::default())),
            ..Default::default()
        });
        assert_eq!(cache.held_len(), 0);
        assert_eq!(cache.expected_digest("p2"), None);
    }

    #[test]
    fn the_clis_own_prompt_is_not_named_or_counted() {
        let mut cache = FactCache::new("s1", Path::new(ROOT));
        let mut prompt = requested(1, "p1", "{}");
        if let Some(Event::PermissionRequested(request)) = &mut prompt.event {
            request.origin = claude_runtime_protocol::v1::PermissionOrigin::ProviderPrompt as i32;
        }
        cache.observe(&prompt);
        assert_eq!(cache.expected_digest("p1"), None);
        assert!(cache.expected.is_empty());
    }

    #[test]
    fn a_fact_whose_digest_differs_is_refused_and_one_for_another_sequence_waits() {
        let mut cache = FactCache::new("s1", Path::new(ROOT));
        cache.observe(&requested(3, "p1", r#"{"file_path":"a.txt"}"#));
        cache.receive(&fact_for(
            3,
            "p1",
            message(digest("p1", r#"{"file_path":"b.txt"}"#), inside()),
        ));
        assert_eq!(
            cache.state("p1"),
            GateFactState::Refused(REFUSED_FOR_ANOTHER_REQUEST.into())
        );
        cache.receive(&fact_for(
            2,
            "p1",
            message(digest("p1", r#"{"file_path":"a.txt"}"#), inside()),
        ));
        assert_eq!(cache.state("p1"), GateFactState::Waiting);
        // The same request named under another session or root is another request.
        let other = request_digest("s2", "p1", "Read", br#"{"file_path":"a.txt"}"#, Path::new(ROOT));
        cache.receive(&fact_for(3, "p1", message(other, inside())));
        assert_eq!(
            cache.state("p1"),
            GateFactState::Refused(REFUSED_FOR_ANOTHER_REQUEST.into())
        );
    }

    #[test]
    fn a_reconnect_clears_the_facts_and_keeps_the_requests() {
        let mut cache = FactCache::new("s1", Path::new(ROOT));
        let input = r#"{"file_path":"a.txt"}"#;
        cache.observe(&requested(3, "p1", input));
        cache.receive(&fact_for(3, "p1", message(digest("p1", input), inside())));
        cache.receive_root(&GateRootFact {
            is_boundary: true,
            not_a_boundary: String::new(),
        });
        cache.disconnected();
        assert_eq!(cache.state("p1"), GateFactState::Waiting);
        assert_eq!(cache.root_fact(), None);
        cache.receive(&fact_for(3, "p1", message(digest("p1", input), inside())));
        assert!(matches!(cache.state("p1"), GateFactState::Have(_)));
    }

    #[test]
    fn an_unknown_reason_or_a_long_one_is_refused() {
        let d = digest("p1", "{}");
        let unknown = GateFactMsg {
            classification: Some(GateVerdict {
                passes: true,
                why: "a reason from a newer helper".into(),
            }),
            ..message(d, inside())
        };
        assert_eq!(decode_fact(&unknown), Err(REASON_SERVER_FACT_UNKNOWN.to_owned()));
        let long = GateFactMsg {
            classification: Some(GateVerdict {
                passes: false,
                why: "x".repeat(GATE_REASON_MAX + 1),
            }),
            ..message(d, inside())
        };
        assert_eq!(decode_fact(&long), Err(REFUSED_UNREADABLE.to_owned()));
        // The two the policy half adds are never the disk half's.
        let rule = GateFactMsg {
            classification: Some(sent_verdict(Classification {
                verdict: PermissionVerdict::AllowWithoutAsking,
                reason: REASON_ALLOWED_BY_A_PROJECT_RULE,
            })),
            ..message(d, inside())
        };
        assert_eq!(decode_fact(&rule), Err(REASON_SERVER_FACT_UNKNOWN.to_owned()));
        let short_digest = GateFactMsg {
            request_digest: vec![0; 31],
            ..message(d, inside())
        };
        assert_eq!(decode_fact(&short_digest), Err(REFUSED_UNREADABLE.to_owned()));
        let no_classification = GateFactMsg {
            classification: None,
            ..message(d, inside())
        };
        assert_eq!(decode_fact(&no_classification), Err(REFUSED_UNREADABLE.to_owned()));
        // A places card only with one of the places' own reasons.
        let places = GateFactMsg {
            places_card: Some(sent_verdict(ask(REASON_EITRI_OWN_PLACES))),
            ..message(d, inside())
        };
        assert_eq!(
            decode_fact(&places).unwrap().0.places_card,
            Some(ask(REASON_EITRI_OWN_PLACES))
        );
        let odd_places = GateFactMsg {
            places_card: Some(sent_verdict(inside())),
            ..message(d, inside())
        };
        assert_eq!(decode_fact(&odd_places), Err(REFUSED_UNREADABLE.to_owned()));
        // Through the cache, each is a refusal for its request.
        let mut cache = FactCache::new("s1", Path::new(ROOT));
        cache.observe(&requested(1, "p1", "{}"));
        cache.receive(&fact_for(1, "p1", unknown));
        assert_eq!(
            cache.state("p1"),
            GateFactState::Refused(REASON_SERVER_FACT_UNKNOWN.into())
        );
    }

    #[test]
    fn a_server_refusal_is_shown_plain_and_bounded() {
        let mut cache = FactCache::new("s1", Path::new(ROOT));
        cache.observe(&requested(1, "p1", "{}"));
        cache.receive(&GateRequestFact {
            permission_id: "p1".into(),
            sequence: 1,
            fact: None,
            refusal: format!("slow\n\u{1b}[31m\u{202e}{}", "y".repeat(1000)),
        });
        let GateFactState::Refused(why) = cache.state("p1") else {
            panic!("{:?}", cache.state("p1"));
        };
        assert!(
            why.starts_with("the server's path check gave no fact: slow??[31m?"),
            "{why}"
        );
        assert!(why.len() < 300, "{}", why.len());
        assert!(!why.chars().any(char::is_control));
        // A permission id that is not an identifier is not kept.
        cache.receive(&fact_for(1, "p 1\n", message(digest("p1", "{}"), inside())));
        assert_eq!(cache.held_len(), 1);
    }

    #[test]
    fn the_cache_is_bounded() {
        let mut cache = FactCache::new("s1", Path::new(ROOT));
        for n in 0..GATE_FACTS_HELD_MAX + 10 {
            let id = format!("p{n}");
            cache.receive(&fact_for(1, &id, message(digest(&id, "{}"), inside())));
            cache.observe(&requested(n as u64 + 1, &id, "{}"));
        }
        assert_eq!(cache.held_len(), GATE_FACTS_HELD_MAX);
        assert_eq!(cache.expected.len(), GATE_FACTS_HELD_MAX);
        let past = format!("p{}", GATE_FACTS_HELD_MAX + 5);
        assert_eq!(cache.state(&past), GateFactState::Refused(REFUSED_TOO_MANY.into()));
    }

    #[test]
    fn a_fresh_check_is_both_facts_for_the_request_or_a_refusal() {
        let d = digest("p1", "{}");
        let answer = CheckFactResponse {
            first: Some(message(d, inside())),
            fresh: Some(message(d, ask(REASON_NOT_ON_READ_ONLY_LIST))),
            refusal: String::new(),
        };
        assert_eq!(
            decode_fresh(&answer, &d),
            FreshFact::Both {
                first: decode_fact(answer.first.as_ref().unwrap()).unwrap().0,
                fresh: decode_fact(answer.fresh.as_ref().unwrap()).unwrap().0,
            }
        );
        assert_eq!(
            decode_fresh(&answer, &digest("p2", "{}")),
            FreshFact::Refused(REFUSED_FOR_ANOTHER_REQUEST.into())
        );
        let half = CheckFactResponse {
            fresh: None,
            ..answer.clone()
        };
        assert_eq!(decode_fresh(&half, &d), FreshFact::Refused(REFUSED_UNREADABLE.into()));
        let refused = CheckFactResponse {
            refusal: "no longer pending".into(),
            ..answer
        };
        assert_eq!(
            decode_fresh(&refused, &d),
            FreshFact::Refused("the server's path check gave no fact: no longer pending".into())
        );
    }

    #[test]
    fn a_root_fact_names_only_a_known_refusal() {
        let root = |is_boundary: bool, name: &str| {
            decode_root(&GateRootFact {
                is_boundary,
                not_a_boundary: name.into(),
            })
        };
        assert_eq!(root(true, "").not_a_boundary, None);
        assert_eq!(
            root(false, "HomeOrAbove").not_a_boundary,
            Some(NotABoundary::HomeOrAbove)
        );
        assert_eq!(root(false, "Newer").not_a_boundary, None);
        assert_eq!(root(true, "HomeOrAbove").not_a_boundary, None);
    }
}
