//! Cached server job facts. Reads never contact the server or inspect the laptop's filesystem.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const MAX_AGE: Duration = Duration::from_secs(5);

#[derive(Clone, Debug)]
pub struct TerminalFacts {
    pub busy: Option<bool>,
    pub name: Option<String>,
    pub cwd: Option<PathBuf>,
    pub at: Instant,
}

#[derive(Default)]
struct Cache {
    facts: BTreeMap<String, TerminalFacts>,
    refresh: BTreeSet<String>,
}

/// Shared with the link's worker. No I/O or callbacks run while the cache is held.
#[derive(Clone, Default)]
pub struct TerminalFactsHandle(Arc<Mutex<Cache>>);

impl TerminalFactsHandle {
    pub fn set(&self, id: impl Into<String>, facts: TerminalFacts) {
        self.0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .facts
            .insert(id.into(), facts);
    }

    /// A contended cache is unknown rather than a reason to wait on the UI thread. Stale values are all unknown,
    /// including the directory: a restart must never use an old directory as if it were current.
    pub fn get(&self, id: &str) -> Option<TerminalFacts> {
        let mut facts = self.0.try_lock().ok()?.facts.get(id)?.clone();
        if facts.at.elapsed() > MAX_AGE {
            facts.busy = None;
            facts.name = None;
            facts.cwd = None;
        }
        Some(facts)
    }

    pub fn request_refresh(&self, id: impl Into<String>) {
        self.0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .refresh
            .insert(id.into());
    }

    /// Each connection consumes only requests for terminals it serves.
    pub fn take_refresh_requests_for(&self, ids: &BTreeSet<String>) -> Vec<String> {
        let mut cache = self.0.lock().unwrap_or_else(|error| error.into_inner());
        let requested: Vec<_> = cache.refresh.intersection(ids).cloned().collect();
        for id in &requested {
            cache.refresh.remove(id);
        }
        requested
    }

    /// Invalidate one terminal without disturbing another connection's facts or requests.
    pub fn remove(&self, id: &str) {
        let mut cache = self.0.lock().unwrap_or_else(|error| error.into_inner());
        cache.facts.remove(id);
        cache.refresh.remove(id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(at: Instant) -> TerminalFacts {
        TerminalFacts {
            busy: Some(false),
            name: Some("nvim".into()),
            cwd: Some("/srv/project".into()),
            at,
        }
    }

    #[test]
    fn facts_are_shared_by_id_and_stale_facts_are_unknown() {
        let cache = TerminalFactsHandle::default();
        assert!(cache.get("terminal-a").is_none());
        cache.set("terminal-a", facts(Instant::now()));
        assert_eq!(cache.clone().get("terminal-a").unwrap().busy, Some(false));
        assert!(cache.get("terminal-b").is_none());
        cache.set("terminal-a", facts(Instant::now() - MAX_AGE - Duration::from_secs(1)));
        let stale = cache.get("terminal-a").unwrap();
        assert_eq!(stale.busy, None);
        assert_eq!(stale.name, None);
        assert_eq!(stale.cwd, None);
        cache.set("terminal-a", facts(Instant::now()));
        cache.remove("terminal-a");
        assert!(cache.get("terminal-a").is_none());
    }

    #[test]
    fn refresh_requests_are_deduplicated_and_drained() {
        let cache = TerminalFactsHandle::default();
        cache.request_refresh("terminal-a");
        cache.clone().request_refresh("terminal-a");
        cache.request_refresh("terminal-b");
        let a = BTreeSet::from(["terminal-a".into()]);
        let b = BTreeSet::from(["terminal-b".into()]);
        assert_eq!(cache.take_refresh_requests_for(&a), ["terminal-a"]);
        assert!(cache.take_refresh_requests_for(&a).is_empty());
        assert_eq!(cache.take_refresh_requests_for(&b), ["terminal-b"]);
    }

    #[test]
    fn removal_preserves_other_ids_facts_and_refresh_requests() {
        let cache = TerminalFactsHandle::default();
        for id in ["terminal-a", "terminal-b"] {
            cache.set(id, facts(Instant::now()));
            cache.request_refresh(id);
        }
        cache.remove("terminal-a");
        assert!(cache.get("terminal-a").is_none());
        assert!(cache.get("terminal-b").is_some());
        let ids = BTreeSet::from(["terminal-a".into(), "terminal-b".into()]);
        assert_eq!(cache.take_refresh_requests_for(&ids), ["terminal-b"]);
    }

    #[test]
    fn a_contended_cache_is_unknown_without_waiting() {
        let cache = TerminalFactsHandle::default();
        cache.set("terminal-a", facts(Instant::now()));
        let _held = cache.0.lock().unwrap();
        assert!(cache.get("terminal-a").is_none());
    }
}
