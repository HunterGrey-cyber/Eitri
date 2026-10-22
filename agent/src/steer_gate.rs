//! Ordering shared by providers whose steer requests can be refused asynchronously: the Claude
//! sidecar's gate and the Codex provider keep their open messages in one [`SteerOrder`], so both send
//! them one at a time in typing order and drop the unsent ones behind a refusal.

use crate::provider::{ProviderError, ProviderErrorCode};
use crate::AgentDomainEvent;
use std::ops::{Deref, DerefMut};

/// The most messages that may be open at once in one session; a further one is refused and the host
/// holds it for the turn's end.
pub(crate) const MAX_OPEN_STEERS: usize = 64;
/// Why a message waiting unsent behind a refused one is dropped too.
pub(crate) const BEHIND_A_REFUSAL: &str = "an earlier message was not taken, and this one waits behind it";

/// What the gate needs to know about one open message; the provider keeps the rest.
pub(crate) trait SteerEntry {
    /// The host's id for the message, unique among the open ones.
    fn client_id(&self) -> &str;
    /// Not sent yet: it waits for every message before it to be answered.
    fn waiting(&self) -> bool;
    /// Sent, and its answer has not come yet.
    fn in_flight(&self) -> bool;
}

/// The open messages in the order the host handed them over. An entry that is neither waiting nor
/// in flight was accepted and is kept until its delivery or drop settles it.
#[derive(Debug)]
pub(crate) struct SteerOrder<T>(Vec<T>);

impl<T> Default for SteerOrder<T> {
    fn default() -> Self {
        Self(Vec::new())
    }
}

impl<T: SteerEntry> SteerOrder<T> {
    /// Builds a message only after checking its id and capacity; `agent` names who reads it in the
    /// refusals. The builder is told whether an earlier message is waiting or unanswered, and sends it
    /// immediately only when none is.
    pub(crate) fn open(
        &mut self,
        client_id: &str,
        agent: &str,
        build: impl FnOnce(bool) -> Result<T, ProviderError>,
    ) -> Result<(), ProviderError> {
        let refuse = |code, message: String| ProviderError::Provider { code, message };
        if self.iter().any(|s| s.client_id() == client_id) {
            return Err(refuse(
                ProviderErrorCode::IdempotencyConflict,
                format!("this message was already sent to {agent}"),
            ));
        }
        if self.len() >= MAX_OPEN_STEERS {
            return Err(refuse(
                ProviderErrorCode::TurnAlreadyActive,
                format!("too many messages are waiting for {agent} to read them"),
            ));
        }
        let behind = self.iter().any(|s| s.in_flight() || s.waiting());
        self.0.push(build(behind)?);
        Ok(())
    }

    /// An unanswered request prevents every waiting message from being sent.
    pub(crate) fn next_to_send(&self) -> Option<usize> {
        if self.iter().any(SteerEntry::in_flight) {
            return None;
        }
        self.iter().position(SteerEntry::waiting)
    }

    /// A refusal drops the refused message and every unsent message in typing order.
    pub(crate) fn refused(&mut self, at: usize, reason: &str) -> Vec<AgentDomainEvent> {
        let refused = self.0.remove(at);
        let mut events = vec![AgentDomainEvent::UserMessageDropped {
            client_message_id: refused.client_id().into(),
            reason: reason.into(),
        }];
        self.0.retain(|entry| {
            if !entry.waiting() {
                return true;
            }
            events.push(AgentDomainEvent::UserMessageDropped {
                client_message_id: entry.client_id().into(),
                reason: BEHIND_A_REFUSAL.into(),
            });
            false
        });
        events
    }
}

// Providers also settle deliveries and carry their own transport state across process moves.
impl<T> Deref for SteerOrder<T> {
    type Target = Vec<T>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<T> DerefMut for SteerOrder<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl<T> IntoIterator for SteerOrder<T> {
    type Item = T;
    type IntoIter = std::vec::IntoIter<T>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

impl<T> FromIterator<T> for SteerOrder<T> {
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Self {
        Self(iter.into_iter().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ProviderErrorCode;

    #[derive(Debug)]
    struct Entry {
        id: &'static str,
        waiting: bool,
        in_flight: bool,
    }

    impl SteerEntry for Entry {
        fn client_id(&self) -> &str {
            self.id
        }

        fn waiting(&self) -> bool {
            self.waiting
        }

        fn in_flight(&self) -> bool {
            self.in_flight
        }
    }

    fn waiting(id: &'static str) -> Entry {
        Entry {
            id,
            waiting: true,
            in_flight: false,
        }
    }

    #[test]
    fn a_duplicate_id_is_refused_and_leaves_the_open_message_alone() {
        let mut gate = SteerOrder::default();
        gate.open("same", "Codex", |_| Ok(waiting("same"))).unwrap();
        let error = gate.open("same", "Codex", |_| Ok(waiting("same"))).unwrap_err();
        assert!(
            matches!(
                &error,
                ProviderError::Provider { code: ProviderErrorCode::IdempotencyConflict, message }
                    if message == "this message was already sent to Codex"
            ),
            "a second message with an open id must be refused as a conflict naming the agent: {error:?}"
        );
        assert_eq!(gate.len(), 1, "the refused duplicate must not be recorded");
    }

    #[test]
    fn a_message_past_the_cap_is_refused_naming_the_agent() {
        let mut gate: SteerOrder<_> = (0..MAX_OPEN_STEERS).map(|_| waiting("open")).collect();
        let error = gate
            .open("one more", "Claude", |_| Ok(waiting("one more")))
            .unwrap_err();
        assert!(
            matches!(
                &error,
                ProviderError::Provider { code: ProviderErrorCode::TurnAlreadyActive, message }
                    if message == "too many messages are waiting for Claude to read them"
            ),
            "a full gate must refuse the next message: {error:?}"
        );
        assert_eq!(gate.len(), MAX_OPEN_STEERS);
    }

    #[test]
    fn a_message_behind_an_unanswered_one_is_built_to_wait() {
        let mut gate = SteerOrder::default();
        gate.open("first", "Codex", |behind| {
            assert!(!behind, "nothing is open, so the first message is sent at once");
            Ok(Entry {
                id: "first",
                waiting: false,
                in_flight: true,
            })
        })
        .unwrap();
        gate.open("second", "Codex", |behind| {
            assert!(behind, "the first message is unanswered, so the second must wait");
            Ok(waiting("second"))
        })
        .unwrap();
        assert_eq!(
            gate.next_to_send(),
            None,
            "nothing is sent while an answer is outstanding"
        );
        gate[0].in_flight = false;
        assert_eq!(
            gate.next_to_send(),
            Some(1),
            "once answered, the waiting message goes next"
        );
    }

    #[test]
    fn a_refusal_keeps_accepted_messages_and_drops_the_waiting_ones_in_order() {
        let accepted = Entry {
            id: "accepted",
            waiting: false,
            in_flight: false,
        };
        let mut gate: SteerOrder<_> = [accepted, waiting("refused"), waiting("later")].into_iter().collect();
        let events = gate.refused(1, "input refused");
        assert_eq!(gate.len(), 1);
        assert_eq!(
            gate[0].id, "accepted",
            "an accepted message is not dropped by a later refusal"
        );
        assert!(
            matches!(
                &events[..],
                [
                    AgentDomainEvent::UserMessageDropped { client_message_id: first, reason },
                    AgentDomainEvent::UserMessageDropped { client_message_id: second, reason: behind },
                ] if first == "refused" && reason == "input refused" && second == "later" && behind == BEHIND_A_REFUSAL
            ),
            "the refused message and then each waiting one must be dropped: {events:?}"
        );
    }
}
