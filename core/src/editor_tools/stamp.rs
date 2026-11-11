//! The deadline and sequence rule. A caller that gives up on a chunk cannot withdraw it: it stays ahead of
//! every later call and runs when nvim frees up. So every chunk carries a stamp, and `prelude.lua` does
//! nothing for a stamp past its deadline, or older than one already run in its scope. No tab and no broker
//! here, so any caller of a chunk can stamp one.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rmpv::Value;

/// How long the broker waits for a chunk.
pub const EDITOR_BUDGET: Duration = Duration::from_secs(5);

/// How far [`Stamp::checked`] lets a deadline lie ahead of window 1's clock.
pub const EDITOR_BUDGET_MAX: Duration = Duration::from_secs(10);

/// How much sooner a chunk gives up than its caller does, so a chunk that runs is always answered in time.
pub const STAMP_MARGIN: Duration = Duration::from_millis(250);

/// The longest scope [`Stamp::checked`] takes.
const SCOPE_MAX: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stamp {
    /// The wall clock, milliseconds since the epoch, past which the chunk does nothing.
    pub deadline_ms: u64,
    /// Whose numbers `seq` counts in.
    pub scope: String,
    pub seq: u64,
}

impl Stamp {
    /// The chunk's first argument.
    pub fn to_value(&self) -> Value {
        Value::Map(vec![
            (Value::from("deadline_ms"), Value::from(self.deadline_ms)),
            (Value::from("scope"), Value::from(self.scope.as_str())),
            (Value::from("seq"), Value::from(self.seq)),
        ])
    }

    /// Window 1's check of a stamp from window 2: the scope's shape (`[a-z0-9-]{1,64}`) and a deadline held to
    /// at most [`EDITOR_BUDGET_MAX`] after `now_ms`. A deadline already past stays past: the chunk then does
    /// nothing, as it would have in window 2's own editor.
    pub fn checked(mut self, now_ms: u64) -> Result<Stamp, String> {
        let shaped = !self.scope.is_empty()
            && self.scope.len() <= SCOPE_MAX
            && self
                .scope
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-');
        if !shaped {
            return Err(format!("a stamp's scope has the wrong shape: {:?}", self.scope));
        }
        let latest = now_ms.saturating_add(EDITOR_BUDGET_MAX.as_millis() as u64);
        self.deadline_ms = self.deadline_ms.min(latest);
        Ok(self)
    }
}

/// Stamps one scope's chunks, counting up from 1.
#[derive(Debug)]
pub struct Sequencer {
    scope: String,
    next: u64,
}

impl Sequencer {
    pub fn new(scope: String) -> Sequencer {
        Sequencer { scope, next: 1 }
    }

    /// The next stamp, with its deadline `budget` from now. The budget is taken as given: a caller picks its
    /// own margin below the moment it gives up.
    pub fn stamp(&mut self, budget: Duration) -> Stamp {
        let seq = self.next;
        self.next += 1;
        Stamp {
            deadline_ms: wall_ms().saturating_add(budget.as_millis() as u64),
            scope: self.scope.clone(),
            seq,
        }
    }
}

/// The monotonic clock, with a scoped override for synchronous unit tests.
pub(crate) fn monotonic_now() -> std::time::Instant {
    #[cfg(test)]
    if let Some(now) = super::testing::clock::now() {
        return now;
    }
    std::time::Instant::now()
}

/// The wall clock in milliseconds since the epoch, the clock `prelude.lua` reads.
pub fn wall_ms() -> u64 {
    #[cfg(test)]
    if let Some(now) = super::testing::clock::wall_ms() {
        return now;
    }
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stamp(scope: &str, deadline_ms: u64) -> Stamp {
        Stamp {
            deadline_ms,
            scope: scope.to_owned(),
            seq: 7,
        }
    }

    #[test]
    fn a_stamp_is_a_map_of_three() {
        let value = stamp("abc-1", 1_700_000_000_000).to_value();
        let map = value.as_map().expect("a map");
        let field = |name: &str| {
            let found: Vec<&Value> = map
                .iter()
                .filter(|(key, _)| key.as_str() == Some(name))
                .map(|(_, value)| value)
                .collect();
            assert_eq!(found.len(), 1, "{name}");
            found[0]
        };
        assert_eq!(map.len(), 3);
        assert_eq!(field("deadline_ms").as_u64(), Some(1_700_000_000_000));
        assert_eq!(field("scope").as_str(), Some("abc-1"));
        assert_eq!(field("seq").as_u64(), Some(7));
    }

    #[test]
    fn checked_refuses_a_scope_of_the_wrong_shape() {
        let long = "a".repeat(SCOPE_MAX + 1);
        for scope in ["", "Abc", "a b", "a_b", "a/b", "a\nb", "é", long.as_str()] {
            assert!(stamp(scope, 1).checked(0).is_err(), "{scope:?}");
        }
        let longest = "a".repeat(SCOPE_MAX);
        for scope in ["a", "0123abcdef-7", "ide-3", longest.as_str()] {
            assert!(stamp(scope, 1).checked(0).is_ok(), "{scope:?}");
        }
    }

    #[test]
    fn checked_holds_a_far_deadline_to_the_limit() {
        let now = 1_000_000;
        let limit = now + EDITOR_BUDGET_MAX.as_millis() as u64;
        assert_eq!(stamp("a", u64::MAX).checked(now).unwrap().deadline_ms, limit);
        assert_eq!(stamp("a", limit + 1).checked(now).unwrap().deadline_ms, limit);
        assert_eq!(stamp("a", limit - 1).checked(now).unwrap().deadline_ms, limit - 1);
        assert_eq!(
            stamp("a", now - 1).checked(now).unwrap().deadline_ms,
            now - 1,
            "a deadline already past stays past"
        );
        assert_eq!(stamp("a", u64::MAX).checked(u64::MAX).unwrap().deadline_ms, u64::MAX);
    }

    #[test]
    fn the_sequencer_counts_up() {
        let mut sequencer = Sequencer::new("abc-1".to_owned());
        let before = wall_ms();
        let first = sequencer.stamp(EDITOR_BUDGET);
        let second = sequencer.stamp(EDITOR_BUDGET);
        assert_eq!((first.seq, second.seq), (1, 2));
        assert_eq!(first.scope, "abc-1");
        assert!(first.deadline_ms >= before + EDITOR_BUDGET.as_millis() as u64);
    }
}
