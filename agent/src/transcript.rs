//! Locates and probes the real Claude CLI's own transcript files under `~/.claude/projects/`
//! (design doc §8.4's stability probe, §8.5's external-writer boundary). This module never reads
//! or interprets transcript *content* -- only the file's existence and modification time, matching
//! the project's own repeated discipline of not reimplementing Claude's own private wire/storage
//! format.

use std::path::{Path, PathBuf};
use std::time::Duration;

/// Errors with `ErrorKind::NotFound` when neither `CLAUDE_CONFIG_DIR` nor `HOME` is set, rather
/// than silently falling back to the relative path `projects` -- matches `persistence.rs`'s
/// `conversations_dir()`, which already errors the same way for the same condition.
pub fn claude_projects_dir() -> std::io::Result<PathBuf> {
    let base = match std::env::var("CLAUDE_CONFIG_DIR") {
        Ok(v) => PathBuf::from(v),
        Err(_) => {
            let home = std::env::var("HOME").map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "neither CLAUDE_CONFIG_DIR nor HOME is set",
                )
            })?;
            PathBuf::from(home).join(".claude")
        }
    };
    Ok(base.join("projects"))
}

/// Replaces every Unicode scalar in `cwd` outside `[A-Za-z0-9-]` with `-`, matching the real
/// Claude CLI's own sanitization rule -- not just `/` and `.`. Verified against all 111 real
/// buckets under this machine's own `~/.claude/projects/`, by reading each transcript's own
/// recorded `cwd` field and comparing it against the real directory name: 107 of 111 match this
/// rule exactly. The 4 that don't fail for unrelated reasons -- worktree/`cd` cases where the
/// recorded `cwd` is not the cwd the directory was actually created from -- not because they
/// contradict the rule. The earlier "replace `/` and `.` only" version of this function matched
/// 103 of the same 111; the difference is real, not noise: an underscore (e.g. `capture_work` ->
/// `capture-work`) and non-ASCII path segments (e.g. `大集控` -> `---`, one `-` per character,
/// since this iterates `.chars()` -- Unicode scalars, not bytes) are both replaced by the real
/// CLI but were left untouched by the old rule.
pub fn sanitize_cwd_for_claude_projects(cwd: &str) -> String {
    cwd.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '-' })
        .collect()
}

pub fn transcript_path(cwd: &str, provider_session_id: &str) -> std::io::Result<PathBuf> {
    Ok(claude_projects_dir()?
        .join(sanitize_cwd_for_claude_projects(cwd))
        .join(format!("{provider_session_id}.jsonl")))
}

/// Design doc §8.4 step 4: "对 transcript 做短时间稳定性探测，发现仍在写则拒绝 resume". Reads the
/// file's mtime, waits `probe_delay`, reads it again -- `Ok(true)` if unchanged (stable, safe to
/// treat as not actively being written), `Ok(false)` if it changed (still being written). A
/// missing file is a real `Err` (not `Ok(false)`) -- the caller has a different, more specific
/// problem than "unstable" if the transcript doesn't exist at all.
pub fn is_transcript_stable(path: &Path, probe_delay: Duration) -> std::io::Result<bool> {
    let before = std::fs::metadata(path)?.modified()?;
    std::thread::sleep(probe_delay);
    let after = std::fs::metadata(path)?.modified()?;
    Ok(before == after)
}

/// Serializes every test in this crate that mutates the process-wide `CLAUDE_CONFIG_DIR` env
/// var -- this file's own one such test, plus `external_writer.rs`'s three. Both files
/// independently need a controlled value for it, and cargo's default parallel test runner would
/// otherwise let one test's mutation interleave with another's -- exactly the real, reproduced
/// race this plan's own Task 2 fix already found and fixed for a different env var
/// (`XDG_STATE_HOME` in `persistence.rs`). That fix merged same-file tests into one; this race
/// spans two different files/tasks, so a shared lock is the right shape instead. `std::sync::Mutex::new`
/// is a `const fn` (stable since Rust 1.63) -- no `OnceLock`/`lazy_static` needed for a bare
/// `static`, and no new crate dependency (e.g. `serial_test`) for one shared guard.
#[cfg(test)]
pub(crate) static CLAUDE_CONFIG_DIR_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_replaces_every_slash_and_dot() {
        assert_eq!(
            sanitize_cwd_for_claude_projects("/home/user/src/neovibe"),
            "-home-user-src-neovibe"
        );
    }

    #[test]
    fn sanitize_replaces_underscores_and_non_ascii_characters_too() {
        // These are the two classes the earlier "replace `/` and `.` only" rule missed -- see
        // this plan's own real, disk-verified comparison against all 111 real buckets under
        // `~/.claude/projects/` (module doc above). Values confirmed by running this exact
        // function, not by hand-counting.
        assert_eq!(sanitize_cwd_for_claude_projects("/tmp/a_b"), "-tmp-a-b");
        assert_eq!(sanitize_cwd_for_claude_projects("/home/x/大集控"), "-home-x----");
    }

    #[test]
    fn sanitize_handles_a_dotted_path_segment_like_a_real_worktree_path() {
        assert_eq!(
            sanitize_cwd_for_claude_projects("/home/user/src/neovibe/.claude/worktrees/foo"),
            "-home-user-src-neovibe--claude-worktrees-foo"
        );
    }

    #[test]
    fn transcript_path_joins_projects_dir_sanitized_cwd_and_session_id() {
        // Poisoning propagation is the standard, accepted idiom for a shared test-only lock like
        // this (matches this plan's own Global Constraints exception for exactly this pattern
        // elsewhere) -- .unwrap() here is fine, this is test code.
        let _guard = CLAUDE_CONFIG_DIR_TEST_LOCK.lock().unwrap();
        unsafe { std::env::set_var("CLAUDE_CONFIG_DIR", "/tmp/fake-claude-config") };
        let path = transcript_path("/tmp/project", "prov-1").unwrap();
        assert_eq!(
            path,
            PathBuf::from("/tmp/fake-claude-config/projects/-tmp-project/prov-1.jsonl")
        );
        unsafe { std::env::remove_var("CLAUDE_CONFIG_DIR") };
    }

    #[test]
    fn is_transcript_stable_returns_true_for_an_unmodified_file() {
        let dir = std::env::temp_dir().join(format!("agent-transcript-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test.jsonl");
        std::fs::write(&path, "{}\n").unwrap();
        let stable = is_transcript_stable(&path, Duration::from_millis(50)).unwrap();
        assert!(stable);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn is_transcript_stable_returns_false_when_the_file_changes_during_the_probe() {
        let dir = std::env::temp_dir().join(format!("agent-transcript-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test.jsonl");
        std::fs::write(&path, "{}\n").unwrap();
        let path_clone = path.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            std::fs::write(&path_clone, "{}\n{}\n").unwrap();
        });
        let stable = is_transcript_stable(&path, Duration::from_millis(80)).unwrap();
        assert!(!stable);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn is_transcript_stable_errors_on_a_missing_file() {
        let result = is_transcript_stable(
            Path::new("/tmp/definitely-does-not-exist-12345.jsonl"),
            Duration::from_millis(10),
        );
        assert!(result.is_err());
    }
}
