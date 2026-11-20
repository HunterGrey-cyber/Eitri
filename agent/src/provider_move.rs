//! What the Codex and ACP providers share when a chat moves to another process: why a freeze is refused, the
//! sequence of a freeze, of taking a frozen chat up and of committing or giving up the move, and how an instant of
//! the provider's state crosses as data. Each provider keeps only what is its own: which states hold a move, how its
//! state becomes data and back, and what a commit or a resume does to that state.
//!
//! An [`Instant`] means nothing in another process, so a deadline or a moment the provider keeps crosses
//! as milliseconds from the freeze: ahead of it for a deadline still to come, behind it for a moment already
//! past. The destination counts from its own clock when it takes the state up, so the time the move itself
//! took is added to every deadline rather than taken from it.

use crate::external_link::lock;
use crate::jsonrpc_link::{AnsweredRecord, JsonRpcLink, LinkHandoff, OverCapAnswer, Pending};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::io;
use std::os::fd::OwnedFd;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How long a freeze waits for a session or a turn that is still starting, whose answer only the caller
/// waiting for it can use.
const START_WAIT: Duration = Duration::from_secs(2);

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

/// A link as a move sees it: a freeze stops it where it stands (one that cannot stop comes back unchanged, with the
/// reason), and a commit starts it once it was taken up parked.
pub(crate) trait MovingLink: Sized {
    type Stopped;
    fn stop(self) -> Result<Self::Stopped, (Self, String)>;
    fn start_after_commit(&mut self) -> io::Result<()>;
}

impl MovingLink for JsonRpcLink {
    type Stopped = (LinkHandoff, [OwnedFd; 3]);

    fn stop(self) -> Result<Self::Stopped, (Self, String)> {
        self.stop_for_move().map_err(|refused| (*refused.link, refused.reason))
    }

    fn start_after_commit(&mut self) -> io::Result<()> {
        JsonRpcLink::start_after_commit(self)
    }
}

/// The freeze both providers make for a move. While `blocker` says the session or a turn is still starting it waits
/// up to [`START_WAIT`]: the start's caller waits outside the state's lock and takes it again once its answer comes.
/// Any other blocker refuses at once. Then the link is taken and stopped, and one that cannot stop is put back, the
/// provider left as it was. Only then is the state swapped for `gone` and handed to `hand_over`, with the lock still
/// held, so nothing is pumped from a half-stopped link and the provider is closed from here on.
pub(crate) fn freeze<S, L: MovingLink, H>(
    state: &Mutex<S>,
    link: &Mutex<Option<L>>,
    blocker: impl Fn(&S) -> Option<MoveRefusedWhy>,
    gone: impl FnOnce() -> S,
    hand_over: impl FnOnce(S) -> H,
    now: impl Fn() -> Instant,
    pause: impl Fn(Duration),
) -> Result<(H, L::Stopped), MoveRefusedWhy> {
    let deadline = now() + START_WAIT;
    let mut st = loop {
        let st = lock(state);
        match blocker(&st) {
            None => break st,
            Some(MoveRefusedWhy::Starting) if now() < deadline => {
                drop(st);
                pause(Duration::from_millis(10));
            }
            Some(why) => return Err(why),
        }
    };
    let Some(taken) = lock(link).take() else {
        return Err(MoveRefusedWhy::Ended);
    };
    let stopped = match taken.stop() {
        Ok(stopped) => stopped,
        Err((back, why)) => {
            *lock(link) = Some(back);
            return Err(MoveRefusedWhy::Link(why));
        }
    };
    let frozen = std::mem::replace(&mut *st, gone());
    Ok((hand_over(frozen), stopped))
}

/// How a frozen chat's link is taken up: by the destination, parked until the move is committed, or by the source
/// when the move is given up, reading on at once.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TakeUp {
    Adopt,
    Resume,
}

/// A link taken up from its handoff, the child's pid and the ids of every request it handed over.
pub(crate) struct TakenLink {
    pub(crate) link: JsonRpcLink,
    pub(crate) pid: u32,
    pub(crate) handed: Vec<u64>,
}

/// Takes a stopped link up again, as `how` says.
pub(crate) fn take_up(
    how: TakeUp,
    handoff: LinkHandoff,
    fds: [OwnedFd; 3],
    over_cap: OverCapAnswer,
) -> io::Result<TakenLink> {
    let pid = handoff.pid;
    let handed = handed_ids(&handoff);
    let link = match how {
        TakeUp::Adopt => JsonRpcLink::adopt(handoff, fds, over_cap)?,
        TakeUp::Resume => JsonRpcLink::resume_after_move(handoff, fds, over_cap)?,
    };
    Ok(TakenLink { link, pid, handed })
}

/// Starts an adopted link once the move is committed; nothing to do for a running link. `committed` then lets the
/// provider pump again, also when the link's threads could not start: the tab then learns that the session ended,
/// instead of waiting for good.
pub(crate) fn commit<S, L: MovingLink>(
    state: &Mutex<S>,
    link: &Mutex<Option<L>>,
    committed: impl FnOnce(&mut S),
) -> io::Result<()> {
    let mut st = lock(state);
    let started = match lock(link).as_mut() {
        Some(link) => link.start_after_commit(),
        None => Ok(()),
    };
    committed(&mut st);
    started
}

/// The source's way back when a move is given up before its commit: its own frozen state, rebuilt, and the link
/// taken up with [`TakeUp::Resume`] replace what the freeze left. That link reads on at once, so `reading` clears what
/// waits for a commit.
pub(crate) fn resume<S, L>(
    state: &Mutex<S>,
    link: &Mutex<Option<L>>,
    rebuilt: (Mutex<S>, Mutex<Option<L>>),
    reading: impl FnOnce(&mut S),
) {
    let (rebuilt_state, rebuilt_link) = rebuilt;
    let mut st = lock(state);
    *st = rebuilt_state
        .into_inner()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    reading(&mut st);
    *lock(link) = rebuilt_link
        .into_inner()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
}

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
fn handed_ids(handoff: &LinkHandoff) -> Vec<u64> {
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
    use std::cell::Cell;
    use std::sync::{Arc, Weak};

    /// A link that stops or starts, or refuses to. While it stops, the provider's state is locked and its own slot is
    /// not; while it starts, the state is locked.
    struct FakeLink {
        stops: bool,
        starts: bool,
        state: Arc<Mutex<u32>>,
        slot: Weak<Mutex<Option<FakeLink>>>,
    }

    impl fmt::Debug for FakeLink {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "FakeLink {{ stops: {} }}", self.stops)
        }
    }

    impl MovingLink for FakeLink {
        type Stopped = &'static str;

        fn stop(self) -> Result<&'static str, (FakeLink, String)> {
            assert!(
                self.state.try_lock().is_err(),
                "the state stays locked while the link stops"
            );
            let slot = self.slot.upgrade().unwrap();
            assert!(slot.try_lock().is_ok(), "the link's slot is free while it stops");
            if self.stops {
                Ok("stopped")
            } else {
                Err((self, "a write the agent does not take".to_owned()))
            }
        }

        fn start_after_commit(&mut self) -> io::Result<()> {
            assert!(
                self.state.try_lock().is_err(),
                "the state stays locked while the link starts"
            );
            if self.starts {
                Ok(())
            } else {
                Err(io::Error::other("the reader thread could not start"))
            }
        }
    }

    /// A provider's two locks, its state `7` and a link in its slot.
    struct Provider {
        state: Arc<Mutex<u32>>,
        link: Arc<Mutex<Option<FakeLink>>>,
    }

    impl Provider {
        fn new(stops: bool, starts: bool) -> Provider {
            let state = Arc::new(Mutex::new(7));
            let link = Arc::new_cyclic(|slot| {
                Mutex::new(Some(FakeLink {
                    stops,
                    starts,
                    state: Arc::clone(&state),
                    slot: slot.clone(),
                }))
            });
            Provider { state, link }
        }

        fn without_link() -> Provider {
            Provider {
                state: Arc::new(Mutex::new(7)),
                link: Arc::new(Mutex::new(None)),
            }
        }

        fn state(&self) -> u32 {
            *lock(&self.state)
        }

        /// The `stops` of the link in its slot, if one is there.
        fn link(&self) -> Option<bool> {
            lock(&self.link).as_ref().map(|link| link.stops)
        }

        fn freeze(
            &self,
            clock: &Clock,
            blocker: impl Fn(&u32) -> Option<MoveRefusedWhy>,
        ) -> Result<(u32, &'static str), MoveRefusedWhy> {
            freeze(
                &self.state,
                &self.link,
                blocker,
                || 0,
                |frozen| {
                    assert!(self.state.try_lock().is_err(), "the state is handed over still locked");
                    frozen
                },
                || clock.now(),
                |by| {
                    assert!(
                        self.state.try_lock().is_ok(),
                        "a start's caller can take the state during the wait"
                    );
                    clock.pause(by)
                },
            )
        }
    }

    /// A clock that moves only when the freeze pauses, counting the pauses.
    struct Clock {
        start: Instant,
        at: Cell<Instant>,
        pauses: Cell<u32>,
    }

    impl Clock {
        fn new() -> Clock {
            let start = Instant::now();
            Clock {
                start,
                at: Cell::new(start),
                pauses: Cell::new(0),
            }
        }

        fn now(&self) -> Instant {
            self.at.get()
        }

        fn pause(&self, by: Duration) {
            self.pauses.set(self.pauses.get() + 1);
            assert!(self.pauses.get() <= 201, "the start wait ignored its deadline");
            self.at.set(self.at.get() + by);
        }
    }

    #[test]
    fn a_start_holds_a_freeze_for_the_start_wait_and_then_refuses_it() {
        let (provider, clock) = (Provider::new(true, true), Clock::new());
        let refused = provider.freeze(&clock, |_| Some(MoveRefusedWhy::Starting));
        assert_eq!(refused, Err(MoveRefusedWhy::Starting));
        assert_eq!(clock.now() - clock.start, START_WAIT);
        assert_eq!(provider.state(), 7, "the provider is left as it was");
        assert_eq!(provider.link(), Some(true));
    }

    #[test]
    fn a_start_that_answers_within_the_wait_lets_the_freeze_go_on() {
        let (provider, clock) = (Provider::new(true, true), Clock::new());
        let asked = Cell::new(0);
        let frozen = provider.freeze(&clock, |_| {
            asked.set(asked.get() + 1);
            (asked.get() <= 3).then_some(MoveRefusedWhy::Starting)
        });
        assert_eq!(frozen, Ok((7, "stopped")));
        assert_eq!(clock.pauses.get(), 3);
    }

    #[test]
    fn any_other_blocker_refuses_at_once() {
        for why in [
            MoveRefusedWhy::TurnRunning,
            MoveRefusedWhy::Stopping,
            MoveRefusedWhy::HistoryLoading,
            MoveRefusedWhy::Ended,
        ] {
            let (provider, clock) = (Provider::new(true, true), Clock::new());
            assert_eq!(provider.freeze(&clock, |_| Some(why.clone())), Err(why));
            assert_eq!(clock.pauses.get(), 0, "it did not wait");
            assert_eq!(provider.state(), 7);
            assert_eq!(provider.link(), Some(true));
        }
    }

    #[test]
    fn no_link_is_an_ended_session() {
        let (provider, clock) = (Provider::without_link(), Clock::new());
        assert_eq!(provider.freeze(&clock, |_| None), Err(MoveRefusedWhy::Ended));
        assert_eq!(provider.state(), 7);
    }

    #[test]
    fn a_link_that_cannot_stop_is_put_back_and_the_state_kept() {
        let (provider, clock) = (Provider::new(false, true), Clock::new());
        assert_eq!(
            provider.freeze(&clock, |_| None),
            Err(MoveRefusedWhy::Link("a write the agent does not take".to_owned()))
        );
        assert_eq!(provider.link(), Some(false));
        assert_eq!(provider.state(), 7);
    }

    #[test]
    fn a_freeze_hands_the_state_over_and_leaves_the_provider_closed() {
        let (provider, clock) = (Provider::new(true, true), Clock::new());
        assert_eq!(provider.freeze(&clock, |_| None), Ok((7, "stopped")));
        assert_eq!(provider.state(), 0, "what the freeze leaves is the closed state");
        assert_eq!(provider.link(), None, "the link went with the handoff");
    }

    #[test]
    fn a_commit_lets_the_provider_pump_also_when_its_link_cannot_start() {
        for starts in [true, false] {
            let provider = Provider::new(true, starts);
            let started = commit(&provider.state, &provider.link, |st| *st = 8);
            assert_eq!(started.is_ok(), starts);
            assert_eq!(provider.state(), 8, "the hook ran, whatever the start gave");
        }
        let provider = Provider::without_link();
        assert!(commit(&provider.state, &provider.link, |st| *st = 8).is_ok());
        assert_eq!(provider.state(), 8);
    }

    #[test]
    fn a_resume_puts_the_rebuilt_state_and_link_back() {
        let source = Provider::without_link();
        let rebuilt = Provider::new(false, true);
        let rebuilt = (Mutex::new(5), Mutex::new(lock(&rebuilt.link).take()));
        resume(&source.state, &source.link, rebuilt, |st| *st += 1);
        assert_eq!(source.state(), 6, "the rebuilt state, then the hook");
        assert_eq!(source.link(), Some(false));
    }

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
