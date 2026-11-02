//! What this process failed to keep on disk, so a person hears it once and not only stderr.
//!
//! A full disk leaves a window working: turns run, and every save fails beside them on a thread of
//! its own. Each save says here how it went. A kind that starts failing is news until the panel
//! takes it, and is news again only after it has saved once. Nothing here holds what was being
//! saved, only the error's own words: no prompt text passes through.

use std::sync::Mutex;

use crate::ingestion::UiWake;

/// One thing a window keeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SaveKind {
    /// The open tabs, which `s` brings back.
    Tabs,
    /// A session's record, which puts it in the list of past sessions.
    Record,
    /// A conversation's stored history.
    History,
}

const KINDS: [SaveKind; 3] = [SaveKind::Tabs, SaveKind::Record, SaveKind::History];

impl SaveKind {
    fn slot(self) -> usize {
        self as usize
    }

    /// What failed, then what it costs.
    fn words(self) -> (&'static str, &'static str) {
        match self {
            SaveKind::Tabs => (
                "could not save the open tabs",
                "they will not come back if this window ends now",
            ),
            SaveKind::Record => (
                "could not record this session",
                "it is not in the list of past sessions yet",
            ),
            SaveKind::History => (
                "could not store the history",
                "what is new may be missing when this conversation is reopened",
            ),
        }
    }
}

/// A kind newly failing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SaveFailure {
    pub kind: SaveKind,
    pub why: String,
}

impl SaveFailure {
    /// The line a person reads.
    pub fn text(&self) -> String {
        let (what, cost) = self.kind.words();
        format!("{what} ({}): {cost}", self.why)
    }
}

/// The words for `error`. A full disk is the common case and has plainer ones than the OS's.
fn why(error: &std::io::Error) -> String {
    match error.kind() {
        std::io::ErrorKind::StorageFull => "the disk is full".to_string(),
        _ => error.to_string(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Slot {
    Fine,
    /// Failing, and nobody has been told.
    News,
    /// Failing, and said once.
    Told,
}

struct Health {
    slots: [Slot; KINDS.len()],
    why: [String; KINDS.len()],
}

impl Health {
    const fn new() -> Health {
        Health {
            slots: [Slot::Fine; KINDS.len()],
            why: [String::new(), String::new(), String::new()],
        }
    }

    /// Whether this is news.
    fn failed(&mut self, kind: SaveKind, why: String) -> bool {
        let slot = kind.slot();
        self.why[slot] = why;
        if self.slots[slot] != Slot::Fine {
            return false;
        }
        self.slots[slot] = Slot::News;
        true
    }

    fn saved(&mut self, kind: SaveKind) {
        self.slots[kind.slot()] = Slot::Fine;
    }

    fn take_news(&mut self) -> Vec<SaveFailure> {
        KINDS
            .into_iter()
            .filter(|kind| self.slots[kind.slot()] == Slot::News)
            .collect::<Vec<_>>()
            .into_iter()
            .map(|kind| {
                self.slots[kind.slot()] = Slot::Told;
                SaveFailure {
                    kind,
                    why: self.why[kind.slot()].clone(),
                }
            })
            .collect()
    }
}

static HEALTH: Mutex<Health> = Mutex::new(Health::new());
static WAKE: Mutex<Option<UiWake>> = Mutex::new(None);

fn health() -> std::sync::MutexGuard<'static, Health> {
    HEALTH.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The window's wake: a failure on a writer's thread reaches a panel with nothing else to do.
pub fn set_wake(wake: UiWake) {
    *WAKE.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(wake);
}

/// A save of `kind` failed.
pub fn failed(kind: SaveKind, error: &std::io::Error) {
    let news = health().failed(kind, why(error));
    if news {
        let wake = WAKE.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
        if let Some(wake) = wake {
            wake();
        }
    }
}

/// A save of `kind` reached the disk.
pub fn saved(kind: SaveKind) {
    health().saved(kind);
}

/// The kinds that started failing since the last call.
pub fn take_news() -> Vec<SaveFailure> {
    health().take_news()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_kind_is_news_once_until_it_recovers() {
        let mut health = Health::new();
        assert!(health.failed(SaveKind::Tabs, "the disk is full".into()));
        assert!(!health.failed(SaveKind::Tabs, "the disk is full".into()));
        assert_eq!(health.take_news().len(), 1);
        assert!(!health.failed(SaveKind::Tabs, "the disk is full".into()));
        assert!(health.take_news().is_empty(), "said once, however often it fails");
        health.saved(SaveKind::Tabs);
        assert!(health.take_news().is_empty(), "a recovery says nothing");
        assert!(health.failed(SaveKind::Tabs, "the disk is full".into()));
        assert_eq!(health.take_news().len(), 1, "it can fail again later");
    }

    #[test]
    fn each_kind_is_its_own_news_with_its_newest_reason() {
        let mut health = Health::new();
        health.failed(SaveKind::History, "first".into());
        health.failed(SaveKind::History, "second".into());
        health.failed(SaveKind::Tabs, "the disk is full".into());
        let news = health.take_news();
        assert_eq!(
            news.iter().map(|failure| failure.kind).collect::<Vec<_>>(),
            [SaveKind::Tabs, SaveKind::History]
        );
        assert_eq!(news[1].why, "second");
    }

    #[test]
    fn the_line_says_what_failed_why_and_what_it_costs() {
        let failure = SaveFailure {
            kind: SaveKind::Tabs,
            why: why(&std::io::Error::from_raw_os_error(libc::ENOSPC)),
        };
        assert_eq!(
            failure.text(),
            "could not save the open tabs (the disk is full): they will not come back if this window ends now"
        );
    }
}
