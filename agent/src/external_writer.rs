//! Detects an unexpected write to a session's own transcript file while Eitri believes it is
//! the exclusive writer (an honest boundary: Eitri can only DETECT a concurrent
//! raw-CLI writer, never prevent one -- there is no lock both sides observe). Polls the
//! transcript's own mtime; a change Eitri itself did not just cause (this module has no way to
//! distinguish "Eitri's own last write" from "someone else's" by mtime alone -- see this
//! module's own doc on `ExternalWriterCheck::Changed` below for the honest caveat) is reported to
//! the caller to act on.

use crate::transcript::transcript_path;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};

pub struct ExternalWriterWatch {
    path: PathBuf,
    last_seen_mtime: Option<SystemTime>,
    last_checked: Instant,
    poll_interval: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExternalWriterCheck {
    Unchanged,
    /// The transcript's mtime changed since the last check. This module cannot distinguish "the
    /// session this same process is driving legitimately wrote a new turn" from "a different
    /// process wrote to it" -- callers that expect their OWN writes must account for that
    /// separately (e.g. only start polling again after each of their own writes has settled) or
    /// treat every `Changed` as worth a closer look rather than an automatic hard denial. This is
    /// the honest limitation of an advisory lock: Eitri enforces exclusivity only on the paths it
    /// provides, and never claims an external CLI is locked out because a `flock` exists --
    /// detection, not prevention, and detection alone cannot attribute a write to its source.
    Changed,
}

impl ExternalWriterWatch {
    pub fn start(cwd: &str, provider_session_id: &str, poll_interval: Duration) -> std::io::Result<Self> {
        Ok(Self::at(transcript_path(cwd, provider_session_id)?, poll_interval))
    }

    fn at(path: PathBuf, poll_interval: Duration) -> Self {
        let last_seen_mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
        Self {
            path,
            last_seen_mtime,
            last_checked: Instant::now(),
            poll_interval,
        }
    }

    /// Never blocks. Returns `Unchanged` if `poll_interval` hasn't elapsed since the last real
    /// check yet (a cheap, deliberate no-op -- callers can call this every UI tick without
    /// worrying about hammering the filesystem).
    pub fn check(&mut self) -> ExternalWriterCheck {
        self.check_with_clock(Instant::now)
    }

    fn check_with_clock(&mut self, now: impl Fn() -> Instant) -> ExternalWriterCheck {
        if now().duration_since(self.last_checked) < self.poll_interval {
            return ExternalWriterCheck::Unchanged;
        }
        self.last_checked = now();
        let current_mtime = std::fs::metadata(&self.path).and_then(|m| m.modified()).ok();
        if current_mtime == self.last_seen_mtime {
            ExternalWriterCheck::Unchanged
        } else {
            self.last_seen_mtime = current_mtime;
            ExternalWriterCheck::Changed
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn check_reports_unchanged_when_the_poll_interval_has_not_elapsed() {
        let dir = std::env::temp_dir().join(format!("agent-ext-writer-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("prov-1.jsonl");
        let file = std::fs::File::create(&path).unwrap();
        file.set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(123))
            .unwrap();
        let mut watch = ExternalWriterWatch::at(path, Duration::from_secs(60));
        file.set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(456))
            .unwrap();
        let watch_time = watch.last_checked + Duration::from_secs(1);
        assert_eq!(watch.check_with_clock(|| watch_time), ExternalWriterCheck::Unchanged);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn check_reports_changed_when_the_transcript_file_is_modified_after_the_poll_interval() {
        let dir = std::env::temp_dir().join(format!("agent-ext-writer-test-{}", uuid::Uuid::new_v4()));
        let project_dir = dir.join("projects/-tmp-project");
        std::fs::create_dir_all(&project_dir).unwrap();
        std::fs::write(project_dir.join("prov-1.jsonl"), "{}\n").unwrap();

        let path = project_dir.join("prov-1.jsonl");
        let mut watch = ExternalWriterWatch::at(path.clone(), Duration::from_millis(10));
        let checked_at = watch.last_checked + watch.poll_interval;
        std::fs::write(&path, "{}\n{}\n").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(123))
            .unwrap();
        assert_eq!(watch.check_with_clock(|| checked_at), ExternalWriterCheck::Changed);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_transcript_that_is_missing_at_start_and_then_appears_is_reported_as_changed() {
        let dir = std::env::temp_dir().join(format!("agent-ext-writer-test-{}", uuid::Uuid::new_v4()));
        let project_dir = dir.join("projects/-tmp-project");
        std::fs::create_dir_all(&project_dir).unwrap();

        // No transcript exists yet: the watch starts with no mtime seen.
        let mut watch = ExternalWriterWatch::at(project_dir.join("does-not-exist.jsonl"), Duration::from_millis(10));
        assert_eq!(watch.last_seen_mtime, None);

        // The behavior that actually matters: a transcript appearing where there was none is
        // exactly the "someone else is writing this session" signal this module exists to raise.
        let checked_at = watch.last_checked + watch.poll_interval;
        std::fs::write(project_dir.join("does-not-exist.jsonl"), "{}\n").unwrap();
        assert_eq!(watch.check_with_clock(|| checked_at), ExternalWriterCheck::Changed);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
