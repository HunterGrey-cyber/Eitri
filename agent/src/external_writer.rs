//! Detects an unexpected write to a session's own transcript file while Eitri believes it is
//! the exclusive writer (design doc §8.5's honest boundary: Eitri can only DETECT a concurrent
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
    /// the honest limitation design doc §8.5 itself describes ("对 Neovibe 提供的 handoff/import
    /// 路径强制排他... 不得用 flock 存在就宣称外部 CLI 被锁住") -- detection, not prevention, and
    /// detection alone cannot attribute a write to its source.
    Changed,
}

impl ExternalWriterWatch {
    pub fn start(cwd: &str, provider_session_id: &str, poll_interval: Duration) -> std::io::Result<Self> {
        let path = transcript_path(cwd, provider_session_id)?;
        let last_seen_mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
        Ok(Self {
            path,
            last_seen_mtime,
            last_checked: Instant::now(),
            poll_interval,
        })
    }

    /// Never blocks. Returns `Unchanged` if `poll_interval` hasn't elapsed since the last real
    /// check yet (a cheap, deliberate no-op -- callers can call this every UI tick without
    /// worrying about hammering the filesystem).
    pub fn check(&mut self) -> ExternalWriterCheck {
        if self.last_checked.elapsed() < self.poll_interval {
            return ExternalWriterCheck::Unchanged;
        }
        self.last_checked = Instant::now();
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
    use crate::transcript::CLAUDE_CONFIG_DIR_TEST_LOCK;

    // Every test below acquires `CLAUDE_CONFIG_DIR_TEST_LOCK` (defined in `transcript.rs`, Task
    // 4's Step 2) before touching `CLAUDE_CONFIG_DIR` -- this file's tests and `transcript.rs`'s
    // own one `CLAUDE_CONFIG_DIR`-mutating test land in the same crate's test binary and would
    // otherwise race under cargo's default parallel test runner, the identical hazard this plan's
    // own Task 2 fix already found and fixed once for `XDG_STATE_HOME`. `.unwrap()` on the lock
    // is fine here (test code, standard poisoning-propagation idiom).

    #[test]
    fn check_reports_unchanged_when_the_poll_interval_has_not_elapsed() {
        let _guard = CLAUDE_CONFIG_DIR_TEST_LOCK.lock().unwrap();
        unsafe { std::env::set_var("CLAUDE_CONFIG_DIR", "/tmp/fake-claude-config-ext-writer") };
        let mut watch = ExternalWriterWatch::start("/tmp/project", "prov-1", Duration::from_secs(60)).unwrap();
        assert_eq!(watch.check(), ExternalWriterCheck::Unchanged);
        unsafe { std::env::remove_var("CLAUDE_CONFIG_DIR") };
    }

    #[test]
    fn check_reports_changed_when_the_transcript_file_is_modified_after_the_poll_interval() {
        let _guard = CLAUDE_CONFIG_DIR_TEST_LOCK.lock().unwrap();
        let dir = std::env::temp_dir().join(format!("agent-ext-writer-test-{}", uuid::Uuid::new_v4()));
        let project_dir = dir.join("projects/-tmp-project");
        std::fs::create_dir_all(&project_dir).unwrap();
        std::fs::write(project_dir.join("prov-1.jsonl"), "{}\n").unwrap();
        unsafe { std::env::set_var("CLAUDE_CONFIG_DIR", &dir) };

        let mut watch = ExternalWriterWatch::start("/tmp/project", "prov-1", Duration::from_millis(10)).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(project_dir.join("prov-1.jsonl"), "{}\n{}\n").unwrap();
        assert_eq!(watch.check(), ExternalWriterCheck::Changed);

        unsafe { std::env::remove_var("CLAUDE_CONFIG_DIR") };
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_transcript_that_is_missing_at_start_and_then_appears_is_reported_as_changed() {
        let _guard = CLAUDE_CONFIG_DIR_TEST_LOCK.lock().unwrap();
        let dir = std::env::temp_dir().join(format!("agent-ext-writer-test-{}", uuid::Uuid::new_v4()));
        let project_dir = dir.join("projects/-tmp-project");
        std::fs::create_dir_all(&project_dir).unwrap();
        unsafe { std::env::set_var("CLAUDE_CONFIG_DIR", &dir) };

        // No transcript exists yet -- start() must still succeed, recording "no mtime seen".
        let watch = ExternalWriterWatch::start("/tmp/project", "does-not-exist", Duration::from_millis(10));
        assert!(watch.is_ok());
        let mut watch = watch.unwrap();

        // The behavior that actually matters: a transcript appearing where there was none is
        // exactly the "someone else is writing this session" signal this module exists to raise.
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(project_dir.join("does-not-exist.jsonl"), "{}\n").unwrap();
        assert_eq!(watch.check(), ExternalWriterCheck::Changed);

        unsafe { std::env::remove_var("CLAUDE_CONFIG_DIR") };
        let _ = std::fs::remove_dir_all(&dir);
    }
}
