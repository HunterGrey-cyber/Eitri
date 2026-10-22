//! What the Codex and ACP providers share when a chat moves to another process: why a freeze is refused,
//! and how an instant of the provider's state crosses as data.
//!
//! An [`Instant`] means nothing in another process, so a deadline or a moment the provider keeps crosses
//! as milliseconds from the freeze: ahead of it for a deadline still to come, behind it for a moment already
//! past. The destination counts from its own clock when it takes the state up, so the time the move itself
//! took is added to every deadline rather than taken from it.

use crate::jsonrpc_link::{AnsweredRecord, JsonRpcLink, LinkHandoff, Pending};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::io;
use std::time::{Duration, Instant};

/// How long a freeze waits for a session or a turn that is still starting, whose answer only the caller
/// waiting for it can use.
pub(crate) const START_WAIT: Duration = Duration::from_secs(2);

/// Why a provider would not freeze for a move. The provider is left exactly as it was.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MoveRefusedWhy {
    /// This provider moves only between turns, and a turn is running.
    TurnRunning,
    /// The session is being stopped, by a thread of this process that cannot go along.
    Stopping,
    /// A resumed conversation's history is still being read.
    HistoryLoading,
    /// The session, or a turn, is still starting: its answer did not come within the bound.
    Starting,
    /// The session ended, or was closed.
    Ended,
    /// The connection to the agent could not be stopped where it stood.
    Link(String),
}

impl fmt::Display for MoveRefusedWhy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MoveRefusedWhy::TurnRunning => f.write_str("a turn is running, and this chat moves between turns"),
            MoveRefusedWhy::Stopping => f.write_str("the agent is being stopped"),
            MoveRefusedWhy::HistoryLoading => f.write_str("the conversation's history is still being read"),
            MoveRefusedWhy::Starting => f.write_str("the session or its turn is still starting"),
            MoveRefusedWhy::Ended => f.write_str("the session has ended"),
            MoveRefusedWhy::Link(why) => write!(f, "the agent's connection could not be handed over: {why}"),
        }
    }
}

impl std::error::Error for MoveRefusedWhy {}

/// One of the provider's own requests as it crosses: still open, to be rebound by its id against the
/// adopted link, or already answered before the link stopped and not yet read, to be read there once.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) enum MovedAnswer {
    Open(u64),
    Arrived(AnsweredRecord),
}

impl MovedAnswer {
    /// What `pending` is once its link has stopped.
    pub(crate) fn of(pending: &mut Pending) -> MovedAnswer {
        match pending.answered_record() {
            Some(record) => MovedAnswer::Arrived(record),
            None => MovedAnswer::Open(pending.id()),
        }
    }

    /// The [`Pending`] it stands for on the link that took the conversation up.
    pub(crate) fn rebind(self, link: &JsonRpcLink) -> io::Result<Pending> {
        match self {
            MovedAnswer::Arrived(record) => Ok(link.answered(record)),
            MovedAnswer::Open(id) => link.rebind(id).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("request {id} is not among the requests the link was handed"),
                )
            }),
        }
    }
}

/// The ids of every request a stopped link handed over, answered or not.
pub(crate) fn handed_ids(handoff: &LinkHandoff) -> Vec<u64> {
    let open = handoff.pending.iter().map(|record| record.id);
    open.chain(handoff.answered.iter().map(|record| record.id)).collect()
}

/// Lets go of every handed-over request the provider did not claim: nobody would read its answer, and an
/// entry never rebound would be kept until the link ends.
pub(crate) fn forget_unclaimed(link: &JsonRpcLink, handed: &[u64]) {
    for id in handed {
        drop(link.rebind(*id));
    }
}

/// `at` as milliseconds from `now`: positive for a moment ahead, negative for one behind.
pub(crate) fn offset_ms(at: Instant, now: Instant) -> i64 {
    match at.checked_duration_since(now) {
        Some(ahead) => i64::try_from(ahead.as_millis()).unwrap_or(i64::MAX),
        None => i64::try_from(now.duration_since(at).as_millis()).map_or(i64::MIN, |behind| -behind),
    }
}

/// The instant `ms` milliseconds from `now`. One further behind than this clock reaches is its earliest
/// instant: a moment that long past is past either way.
pub(crate) fn at_offset(ms: i64, now: Instant) -> Instant {
    let span = Duration::from_millis(ms.unsigned_abs());
    if ms >= 0 {
        now.checked_add(span).unwrap_or(now)
    } else {
        let mut back = span;
        loop {
            if let Some(at) = now.checked_sub(back) {
                return at;
            }
            back /= 2;
            if back.is_zero() {
                return now;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_instant_crosses_as_its_distance_from_the_freeze() {
        let now = Instant::now();
        let ahead = now + Duration::from_millis(1500);
        assert_eq!(offset_ms(ahead, now), 1500);
        assert_eq!(at_offset(1500, now), ahead);
        if let Some(behind) = now.checked_sub(Duration::from_millis(250)) {
            assert_eq!(offset_ms(behind, now), -250);
            assert_eq!(at_offset(-250, now), behind);
        }
        assert!(at_offset(i64::MIN, now) <= now, "a moment far behind stays behind");
    }
}
