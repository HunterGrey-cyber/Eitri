//! Poll the session transcript's mtime for changes while this client expects exclusive access.
//! A raw CLI writer cannot be prevented because there is no lock both clients observe.
//! Mtime alone cannot distinguish this client's last write from another writer's change.
//! Report changes for the caller to handle; `ExternalWriterCheck::Changed` describes that caveat.

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
    /// The transcript's mtime changed since the last check. This does not distinguish this
    /// process's own writes from another process's writes. Callers must account for expected
    /// writes, for example by waiting for them to settle before polling again, or inspect each
    /// `Changed` result before denying access. This detects changes without preventing them
    /// or identifying their source.
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

    // Every test below acquires `CLAUDE_CONFIG_DIR_TEST_LOCK`, defined in `transcript.rs`,
    // before touching `CLAUDE_CONFIG_DIR`. Tests in both modules share a test binary and would
    // otherwise race under Cargo's parallel test runner. Unwrapping the lock propagates poisoning.

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
