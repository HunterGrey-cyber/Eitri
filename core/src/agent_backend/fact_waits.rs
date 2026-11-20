//! The requests of a remote tab with the server's path facts that wait before the gate answers them: one whose
//! decision reads the fact and has none yet waits for it, at most [`GATE_FACT_WAIT`] from when the gate first held
//! it; one whose automatic answer came from a fact waits for the fresh check, at most [`GATE_FRESH_WAIT`] and a little
//! slack. Both are drawn as no card meanwhile. Entering bypass ends a fresh check planned under another mode: the
//! request is judged again in bypass at once. Nothing here reads a file, the laptop's boundary or an identity: the
//! facts are the server's, and this only keeps the time and the answer.
//!
//! A copy carries none of it (a move does not take it): the window the tab lands in holds the request again and asks
//! the server there.

use agent::gate_facts::{FreshFact, GATE_FACT_WAIT, GATE_FRESH_WAIT};
use agent::{GateFact, PermissionDecision, PermissionMode};
use std::cell::Cell;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

/// How long past [`GATE_FRESH_WAIT`] the gate waits for a fresh check's answer before it stops waiting: the provider
/// answers within that limit, and this covers a wake that arrives late.
const FRESH_SLACK: Duration = Duration::from_millis(500);

/// Why a request is a card when its fresh check ended after the tab's mode changed.
pub(crate) const MODE_CHANGED_DURING_THE_CHECK: &str = "the tab's mode changed during the server's check";

/// The server's fact for the request being judged, and what judging it read: `read` when any part of the decision
/// came from the fact (its places card or the absence of one, its boundary, its classification), `missing` when the
/// decision needed the fact and there was none.
pub(crate) struct FactRead<'a> {
    fact: Option<&'a GateFact>,
    read: Cell<bool>,
    missing: Cell<bool>,
}

impl<'a> FactRead<'a> {
    pub(crate) fn new(fact: Option<&'a GateFact>) -> Self {
        Self {
            fact,
            read: Cell::new(false),
            missing: Cell::new(false),
        }
    }

    /// The fact, noted as read; `None`, noted as missing, when there is none.
    pub(crate) fn get(&self) -> Option<&'a GateFact> {
        match self.fact {
            Some(fact) => {
                self.read.set(true);
                Some(fact)
            }
            None => {
                self.missing.set(true);
                None
            }
        }
    }

    pub(crate) fn read(&self) -> bool {
        self.read.get()
    }

    pub(crate) fn missing(&self) -> bool {
        self.missing.get()
    }
}

/// One automatic answer waiting for its fresh check: what it was planned from, and the answer once it came.
pub(crate) struct FreshWait {
    pub(crate) plan: FreshPlan,
    started: Instant,
    answer: Arc<Mutex<Option<FreshFact>>>,
}

/// What a request's fresh check says now.
pub(crate) enum FreshState {
    /// The request waits on no fresh check.
    NotWaiting,
    /// It waits, and no answer came yet.
    Waiting,
    /// The check is over: its answer, or `Unavailable` when none came in time. The wait is taken out.
    Ready(FreshWait, FreshFact),
}

/// The requests of one tab waiting for the server's fact or for a fresh check.
#[derive(Default)]
pub(crate) struct FactWaits {
    for_fact: BTreeMap<String, Instant>,
    fresh: BTreeMap<String, FreshWait>,
}

impl Clone for FactWaits {
    fn clone(&self) -> Self {
        Self::default()
    }
}

impl PartialEq for FactWaits {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

impl std::fmt::Debug for FactWaits {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FactWaits")
            .field("for_fact", &self.for_fact.len())
            .field("fresh", &self.fresh.len())
            .finish()
    }
}

fn lock(answer: &Mutex<Option<FreshFact>>) -> std::sync::MutexGuard<'_, Option<FreshFact>> {
    answer.lock().unwrap_or_else(PoisonError::into_inner)
}

impl FactWaits {
    pub(crate) fn is_empty(&self) -> bool {
        self.for_fact.is_empty() && self.fresh.is_empty()
    }

    /// Every request waiting here, drawn as no card.
    pub(crate) fn ids(&self) -> impl Iterator<Item = &String> {
        self.for_fact.keys().chain(self.fresh.keys())
    }

    /// Holds `id` for its fact, from `now` if it was not held yet; how long it has been held.
    pub(crate) fn hold_for_fact(&mut self, id: &str, now: Instant) -> Duration {
        let since = *self.for_fact.entry(id.to_string()).or_insert(now);
        now.saturating_duration_since(since)
    }

    /// `id` no longer waits for its fact: one arrived, or it was judged without one.
    pub(crate) fn fact_settled(&mut self, id: &str) {
        self.for_fact.remove(id);
    }

    pub(crate) fn holds_for_fact(&self, id: &str) -> bool {
        self.for_fact.contains_key(id)
    }

    /// Whether `id`'s wait for its fact is over by `now`, or ended by bypass (the tab in `mode`): judged again then.
    pub(crate) fn fact_due(&self, id: &str, now: Instant, mode: PermissionMode) -> bool {
        if mode == PermissionMode::Bypass && self.for_fact.contains_key(id) {
            return true;
        }
        self.for_fact
            .get(id)
            .is_some_and(|since| now.saturating_duration_since(*since) >= GATE_FACT_WAIT)
    }

    /// Starts `id`'s fresh check wait and returns what the provider calls with the answer. The returned callback only
    /// stores the answer: it may run on the caller's thread, inside the provider's call.
    pub(crate) fn start_fresh(&mut self, id: &str, plan: FreshPlan, now: Instant) -> Box<dyn FnOnce(FreshFact) + Send> {
        let answer = Arc::new(Mutex::new(None));
        let slot = answer.clone();
        self.for_fact.remove(id);
        self.fresh.insert(
            id.to_string(),
            FreshWait {
                plan,
                started: now,
                answer,
            },
        );
        Box::new(move |fresh| {
            lock(&slot).get_or_insert(fresh);
        })
    }

    /// Whether `id`'s fresh check is over by `now`: answered, out of time, or ended by bypass.
    pub(crate) fn fresh_ready(&self, id: &str, now: Instant, mode: PermissionMode) -> bool {
        self.fresh.get(id).is_some_and(|wait| {
            ended_by_bypass(wait, mode)
                || lock(&wait.answer).is_some()
                || now.saturating_duration_since(wait.started) >= GATE_FRESH_WAIT + FRESH_SLACK
        })
    }

    /// `id`'s fresh check as it stands by `now`, the tab being in `mode`: a check that is over is taken out with its
    /// answer, and one bypass ended is taken out as if it never began.
    pub(crate) fn fresh_state(&mut self, id: &str, now: Instant, mode: PermissionMode) -> FreshState {
        if !self.fresh.contains_key(id) {
            return FreshState::NotWaiting;
        }
        if !self.fresh_ready(id, now, mode) {
            return FreshState::Waiting;
        }
        if self.fresh.get(id).is_some_and(|wait| ended_by_bypass(wait, mode)) {
            self.fresh.remove(id);
            return FreshState::NotWaiting;
        }
        let wait = self.fresh.remove(id).expect("checked above");
        let answer = lock(&wait.answer).take().unwrap_or(FreshFact::Unavailable);
        FreshState::Ready(wait, answer)
    }

    /// Keeps only the requests `keep` says still wait.
    pub(crate) fn retain(&mut self, mut keep: impl FnMut(&str) -> bool) {
        self.for_fact.retain(|id, _| keep(id));
        self.fresh.retain(|id, _| keep(id));
    }

    /// `id` is answered some other way: it waits here no more, and a fresh answer that comes later is dropped.
    pub(crate) fn forget(&mut self, id: &str) {
        self.for_fact.remove(id);
        self.fresh.remove(id);
    }

    /// How long until the first wait here ends by `now`: zero for one already over, `None` when none waits. A fact or
    /// a fresh answer that arrives wakes the tab itself; this covers the time limits, which nothing else wakes.
    pub(crate) fn next_release(&self, now: Instant) -> Option<Duration> {
        let facts = self
            .for_fact
            .values()
            .map(|since| GATE_FACT_WAIT.saturating_sub(now.saturating_duration_since(*since)));
        let fresh = self.fresh.values().map(|wait| {
            if lock(&wait.answer).is_some() {
                Duration::ZERO
            } else {
                (GATE_FRESH_WAIT + FRESH_SLACK).saturating_sub(now.saturating_duration_since(wait.started))
            }
        });
        facts.chain(fresh).min()
    }

    /// Moves the start of `id`'s wait for its fact back to `since`. For tests of the time limit.
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn held_for_fact_since(&mut self, id: &str, since: Instant) {
        if let Some(at) = self.for_fact.get_mut(id) {
            *at = since;
        }
    }
}

/// Whether bypass ends `wait`, the tab being in `mode`: a check planned under another mode settles nothing in bypass,
/// which judges the request itself.
fn ended_by_bypass(wait: &FreshWait, mode: PermissionMode) -> bool {
    mode == PermissionMode::Bypass && wait.plan.mode != PermissionMode::Bypass
}

/// What a fresh check is started with: the answer planned and the fact it was planned from.
pub(crate) struct FreshPlan {
    /// The fact the answer was planned from, as the laptop held it.
    pub(crate) first: GateFact,
    /// The request's digest, as the provider computed it from the request's own bytes.
    pub(crate) digest: [u8; 32],
    /// The mode the answer was planned under (after an Auto tab followed the CLI out of auto).
    pub(crate) mode: PermissionMode,
    pub(crate) decision: PermissionDecision,
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent::{Classification, PermissionVerdict};

    const AUTO: PermissionMode = PermissionMode::Auto;

    fn fact() -> GateFact {
        GateFact {
            root_is_boundary: true,
            places_card: None,
            classification: Classification {
                verdict: PermissionVerdict::AllowWithoutAsking,
                reason: "a reason",
            },
        }
    }

    fn plan() -> FreshPlan {
        FreshPlan {
            first: fact(),
            digest: [7; 32],
            mode: PermissionMode::Auto,
            decision: PermissionDecision::Allow,
        }
    }

    #[test]
    fn a_fact_read_says_whether_the_decision_read_it_or_missed_it() {
        let held = fact();
        let with = FactRead::new(Some(&held));
        assert!(!with.read() && !with.missing());
        assert_eq!(with.get(), Some(&held));
        assert!(with.read() && !with.missing());
        let without = FactRead::new(None);
        assert_eq!(without.get(), None);
        assert!(without.missing() && !without.read());
    }

    #[test]
    fn a_fact_wait_ends_after_its_limit_and_starts_once() {
        let mut waits = FactWaits::default();
        let start = Instant::now();
        assert_eq!(waits.hold_for_fact("p1", start), Duration::ZERO);
        let later = start + Duration::from_millis(500);
        assert_eq!(waits.hold_for_fact("p1", later), Duration::from_millis(500));
        assert!(!waits.fact_due("p1", later, AUTO));
        assert!(waits.fact_due("p1", start + GATE_FACT_WAIT, AUTO));
        assert_eq!(
            waits.next_release(later),
            Some(GATE_FACT_WAIT - Duration::from_millis(500))
        );
        waits.fact_settled("p1");
        assert!(waits.is_empty());
        assert_eq!(waits.next_release(later), None);
    }

    #[test]
    fn a_fresh_answer_is_taken_once_and_one_that_never_comes_is_unavailable() {
        let mut waits = FactWaits::default();
        let start = Instant::now();
        let done = waits.start_fresh("p1", plan(), start);
        assert!(matches!(waits.fresh_state("p1", start, AUTO), FreshState::Waiting));
        done(FreshFact::Refused("no longer pending".into()));
        assert_eq!(waits.next_release(start), Some(Duration::ZERO));
        match waits.fresh_state("p1", start, AUTO) {
            FreshState::Ready(wait, FreshFact::Refused(why)) => {
                assert_eq!(why, "no longer pending");
                assert_eq!(wait.plan.decision, PermissionDecision::Allow);
            }
            _ => panic!("the answer is ready"),
        }
        assert!(matches!(waits.fresh_state("p1", start, AUTO), FreshState::NotWaiting));
        // A check whose answer never comes is over once its limit has passed.
        let _never = waits.start_fresh("p2", plan(), start);
        assert!(!waits.fresh_ready("p2", start + GATE_FRESH_WAIT, AUTO));
        let late = start + GATE_FRESH_WAIT + FRESH_SLACK;
        assert!(matches!(
            waits.fresh_state("p2", late, AUTO),
            FreshState::Ready(_, FreshFact::Unavailable)
        ));
    }

    #[test]
    fn a_forgotten_wait_drops_its_late_answer_and_a_copy_carries_nothing() {
        let mut waits = FactWaits::default();
        let start = Instant::now();
        let done = waits.start_fresh("p1", plan(), start);
        waits.hold_for_fact("p2", start);
        assert!(waits.clone().is_empty());
        waits.forget("p1");
        done(FreshFact::Unavailable);
        assert!(matches!(waits.fresh_state("p1", start, AUTO), FreshState::NotWaiting));
        assert_eq!(waits.ids().collect::<Vec<_>>(), ["p2"]);
    }

    #[test]
    fn bypass_ends_a_check_planned_under_another_mode_and_a_wait_for_a_fact() {
        let mut waits = FactWaits::default();
        let start = Instant::now();
        let _auto = waits.start_fresh("p1", plan(), start);
        let _bypass = waits.start_fresh(
            "p2",
            FreshPlan {
                mode: PermissionMode::Bypass,
                ..plan()
            },
            start,
        );
        waits.hold_for_fact("p3", start);
        let bypass = PermissionMode::Bypass;
        assert!(!waits.fresh_ready("p1", start, AUTO));
        assert!(waits.fresh_ready("p1", start, bypass));
        assert!(matches!(waits.fresh_state("p1", start, bypass), FreshState::NotWaiting));
        assert!(!waits.ids().any(|id| id == "p1"), "taken out as if it never began");
        // One planned in bypass still waits for its answer.
        assert!(matches!(waits.fresh_state("p2", start, bypass), FreshState::Waiting));
        assert!(!waits.fact_due("p3", start, AUTO));
        assert!(waits.fact_due("p3", start, bypass));
    }
}
