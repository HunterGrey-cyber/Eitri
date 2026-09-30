//! Locates and probes the real Claude CLI's own transcript files under `~/.claude/projects/`
//! (design doc §8.4's stability probe, §8.5's external-writer boundary). This module never reads
//! or interprets transcript *content* -- only the file's existence and modification time, matching
//! the project's own repeated discipline of not reimplementing Claude's own private wire/storage
//! format.
//!
//! **Correction (2026-09-20).** This module now READS transcript content. The sentence above --
//! "never reads or interprets transcript *content*" -- was written for the resume safety probe in
//! design §8.4/§8.5, and **within that scope it still stands**: whether a resume is SAFE is decided
//! from the file's mtime alone ([`is_transcript_stable`]), never from what is written inside it.
//! What is overturned is only its general form, "never, for any purpose".
//!
//! It is overturned because the panel is empty after a resume, and this file is the only place on
//! disk that remembers the conversation. Eitri's own copy (`$XDG_STATE_HOME/eitri/history/`,
//! [`crate::history::store`]) exists as a fallback, but it cannot be the primary source: it only
//! covers sessions Eitri itself drove, and only from the day it was added.
//!
//! **What a real transcript looks like, measured 2026-09-20** over the 822 `.jsonl` files (570 MB,
//! 117,826 lines) under this machine's own `$CLAUDE_CONFIG_DIR/projects` -- note
//! [`claude_projects_dir`] below, which is why `~/.claude/projects` is the wrong place to look on a
//! machine that sets that variable:
//!
//! - The directory holds two kinds of file. **43** are sessions, named `<uuid>.jsonl`, which is what
//!   [`transcript_path`] builds. **779** are subagent logs, named `agent-<hex>.jsonl`, carrying
//!   their PARENT's `sessionId`; they hold 77% of the bytes and no main-chain conversation at all.
//! - A session file carries **fifteen** top-level `type` values. Only four of them -- `user`,
//!   `assistant`, `system`, `attachment` -- are conversational, and of those only `user` and
//!   `assistant` become anything. The other eleven (`last-prompt`, `atis-latch`, `permission-mode`,
//!   `mode`, `ai-title`, `queue-operation`, `file-history-snapshot`, `file-history-delta`,
//!   `relocated`, `worktree-state`, `cost-state`) are side records the wire protocol has no
//!   counterpart for. Five more (`started`, `result`, `failed`, `launched`, `fork-context-ref`)
//!   exist only in the subagent files.
//! - **There is no schema version.** There IS a `version` field on all four conversational line
//!   types, 100% of the time, but it is the writing CLI's own version (2.1.270/272/276 here) and
//!   promises nothing about format. Report it when a parse yields nothing; never branch on it.
//! - **`type:"user"` does not mean "the user typed this".** In a session file, 962 user lines carry
//!   no tool result, and only 448 of them are a person's words. The rest are compaction summaries
//!   (`isCompactSummary`), injected material (`isMeta`), slash-command echoes, local command output,
//!   bash-mode lines and interrupt markers. Decide with an **allow-list** (`promptSource` ∈ {typed,
//!   suggestion_accepted, queued, sdk}, and none of the exclusion flags), never a deny-list: the
//!   failure direction of a deny-list is showing someone else's text under the user's name.
//! - **Compaction is not `type:"summary"`** -- that value does not occur. It is a `type:"user"` line
//!   flagged `isCompactSummary: true`, paired with a `type:"system"`, `subtype:"compact_boundary"`
//!   line.
//!
//! Reading it comes with four constraints, and none of them is optional:
//!
//! 1. **Read-only, forever.** Nothing here writes, renames, moves or deletes anything under
//!    `$CLAUDE_CONFIG_DIR`. That directory belongs to the CLI.
//! 2. **Parse defensively, never authoritatively fail.** An unknown line type, an unknown content
//!    block and a malformed line are all SKIPPED AND COUNTED, never an error that aborts the load.
//!    A format change must degrade to "less history was shown, and the panel said so", never to a
//!    failed resume.
//! 3. **Nothing read here is evidence, and nothing read here is ever persisted.** Transcript
//!    content feeds the panel, and -- since 2026-09-20, at the owner's ruling -- supplies the
//!    *display* title of a row in the resume picker: the newest `type:"ai-title"` line in the
//!    file's last 64 KiB, read fresh each time the picker is built ([`newest_ai_title`]), normalized
//!    through the same [`crate::persistence::title_from_prompt`] every other title goes through. It
//!    is **never written into a `ConversationRecord`**: `title` on disk stays what this project
//!    recorded at write time, first-one-wins, and the CLI's title is layered *above* it at display
//!    time only, falling back to it and then to the bare id when it is absent (37 of this machine's
//!    44 sessions have one; 7 do not). Transcript content still never decides whether a resume is
//!    safe, never reaches a permission decision, and is never composed into a turn sent to the
//!    model.
//! 4. **It is untrusted input.** These bytes are model output and tool output written to disk by
//!    someone else's process. They render through the same DOMPurify/markdown path every other
//!    message does; no second rendering path is opened for them.

use std::path::{Path, PathBuf};
use std::time::Duration;

/// Errors with `ErrorKind::NotFound` when neither `CLAUDE_CONFIG_DIR` nor `HOME` is set, rather
/// than silently falling back to the relative path `projects` -- matches `persistence.rs`'s
/// `conversations_dir()`, which already errors the same way for the same condition.
///
/// **Correction (2026-09-21): this process's own `CLAUDE_CONFIG_DIR` is the *fallback*, not the
/// answer.** When [`crate::account`] has been given an account -- `init.lua`'s
/// `eitri.config.set("agent.account", ...)` -- the directory comes from that account's own
/// convention instead, because this process's variable says only which account launched the
/// window, while the account says which one the CLI is spending. The two were different on the
/// host this was found on (`.claude-personal` inherited, `.claude-work` written), which is why a
/// resumed session opened empty. With no account configured this function is byte-identical to
/// what it always was, and that is the shipped default.
pub fn claude_projects_dir() -> std::io::Result<PathBuf> {
    projects_dir_for(crate::account::configured())
}

/// The half that takes the account as a parameter, so the two branches can be tested without
/// pinning a process-global account inside a shared test binary.
fn projects_dir_for(account: Option<&crate::account::ClaudeAccount>) -> std::io::Result<PathBuf> {
    if let Some(account) = account {
        return Ok(account.config_dir().join("projects"));
    }
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

/// How far back from a transcript's end [`newest_ai_title`] reads, in bytes.
///
/// **Measured, both directions** (design §3.1.3's Amendment ①, 2026-09-20, 44 real session files):
/// the newest `ai-title` line sits a median of 6,928 bytes and at most 33,157 bytes from EOF, so a
/// 64 KiB tail is twice the worst case seen and was byte-identical to a full scan on all 37 titled
/// sessions. A 64 KiB HEAD read found it on **none** of them -- a session opens with `attachment`
/// lines and the first title is already past 117 KB. The direction is not reasoned, it is the
/// result of running both.
///
/// It is an empirical margin, not a guarantee. A session that writes more than this after its last
/// title (one 483,545-byte tool result would do it) reads as having no title, and that miss is
/// **undetectable**: inside the window it looks exactly like a session that never had one. That is
/// accepted, because the only consequence is one picker row dropping to the next level of the
/// ladder, and the alternative -- widening until it is always right -- is an unbounded read of
/// someone else's file on the GTK main loop.
pub const AI_TITLE_TAIL_BYTES: u64 = 64 * 1024;

/// The title the CLI itself last recorded for this session, for DISPLAY in the resume picker, or
/// `None`.
///
/// At the owner's ruling (2026-09-20). It reads the last [`AI_TITLE_TAIL_BYTES`] of `path` and
/// takes the `aiTitle` of the **last** `type:"ai-title"` line in that window. Measured: this
/// machine's 37 titled sessions each hold exactly one distinct value, rewritten a median of 24
/// times, so "the newest wins" is a rule about a case that has not occurred yet rather than one
/// this machine exercises -- which is why it is decided here instead of left to whichever line a
/// scan happens to meet first.
///
/// **Never persisted.** The value is not written into a `ConversationRecord`, so this project's own
/// `title` keeps its first-one-wins rule and `updated_at` is untouched; every picker re-reads. See
/// this module's constraint 3.
///
/// **Bounded and cheap, because its caller is the GTK main loop.**
/// `BackendGreeting::for_kind` already reads Eitri's own small records synchronously there, and
/// `persistence`'s own retention cap already bounds how many rows a picker can have
/// (`MAX_RECORDS_PER_CONVERSATION` + 1), so this adds one fixed-size read per row and nothing that
/// grows -- measured at 1.74 ms hot and 5.92 ms cold over this machine's 16 largest sessions
/// (116 MB in total). **Parsing the whole transcript here would not be acceptable at any speed**:
/// it would trade a bounded read of Eitri's own files for one that grows with a file this project
/// neither controls nor knows a bound for (the largest here is 47,254,948 bytes).
///
/// **No stability probe.** [`is_transcript_stable`] sleeps, and a sleep on the GTK main loop is a
/// visible stall. A half-written last line fails to parse and is skipped like any other malformed
/// line; the worst outcome is one row without a title.
///
/// Every failure -- a missing file, no permission, a seek that fails, invalid UTF-8, malformed
/// JSON, a non-string or blank `aiTitle` -- is `None`, never an `Err`. The caller has three levels
/// of ladder and nothing it could do with an error.
pub fn newest_ai_title(path: &Path) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};

    // **Asked before the open, because `open(2)` on a FIFO blocks until a writer appears** and this
    // function runs on the GTK main loop. A `stat` does not block; a blocking open here would
    // freeze the window while the picker is being built, with no diagnostic and nothing to cancel.
    // `metadata` follows symlinks, which is what is wanted: a link to a real transcript is a real
    // transcript.
    if !std::fs::metadata(path).ok()?.is_file() {
        return None;
    }
    // Recorded before the call, not after it: what the guard above prevents is the ATTEMPT. An
    // `open(2)` on a FIFO never returns, so a probe that only saw successful opens would be blind
    // to exactly the case this exists for. See `tests::OPEN_ATTEMPTS`.
    #[cfg(test)]
    tests::note_open_attempt(path);
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    // A file shorter than the window is read whole, and then its first line is REAL and must be
    // kept. Dropping it unconditionally would cost the title of every short session.
    let seeked = len > AI_TITLE_TAIL_BYTES;
    if seeked {
        file.seek(SeekFrom::Start(len - AI_TITLE_TAIL_BYTES)).ok()?;
    }
    let mut window = Vec::new();
    // `take` rather than trusting the length just read: the CLI may be writing this file right now,
    // and the cap is what makes this function's memory a constant rather than the file's size.
    file.take(AI_TITLE_TAIL_BYTES).read_to_end(&mut window).ok()?;

    // The LAST `ai-title` line wins, and it wins even when its own value is unusable: a newer line
    // saying something this code cannot render is still the newest thing the CLI recorded, and
    // silently showing an older title would be inventing one. `None` from here drops one level.
    let mut newest: Option<String> = None;
    for (index, line) in window.split(|byte| *byte == b'\n').enumerate() {
        // The first line of a seeked window is whatever fragment straddled the window's start.
        if seeked && index == 0 {
            continue;
        }
        let Ok(text) = std::str::from_utf8(line) else { continue };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
            continue;
        };
        if value.get("type").and_then(serde_json::Value::as_str) != Some("ai-title") {
            continue;
        }
        newest = value
            .get("aiTitle")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);
    }
    // The same normalizer every other title goes through -- length cap by Unicode scalar, collapsed
    // whitespace, first visible line only, and `None` for a value that is blank or zero-width.
    // Reused rather than reimplemented, so a title from the CLI can never render differently from
    // one this project recorded itself.
    crate::persistence::title_from_prompt(&newest?)
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

    /// Every path [`newest_ai_title`] has tried to `File::open`, in this process.
    ///
    /// Test-only, written immediately before the open. It exists because the guard it pins cannot
    /// be observed from the function's RETURN value: a directory declines a second time inside
    /// `File::open` (`EISDIR`, swallowed by the `.ok()?`), so a test that only checked for `None`
    /// stayed green with the guard deleted -- which is what the re-review of this branch found and
    /// is the whole reason this probe exists. A FIFO, the case the guard is really for, cannot be
    /// used as the stand-in instead: without the guard the test would hang in `open(2)` forever
    /// rather than fail.
    ///
    /// A set of paths rather than a counter, so that tests running in parallel cannot disturb each
    /// other: each test asks only whether its OWN path is in here.
    pub(crate) static OPEN_ATTEMPTS: std::sync::Mutex<Option<std::collections::HashSet<PathBuf>>> =
        std::sync::Mutex::new(None);

    pub(super) fn note_open_attempt(path: &Path) {
        OPEN_ATTEMPTS
            .lock()
            .unwrap()
            .get_or_insert_with(std::collections::HashSet::new)
            .insert(path.to_path_buf());
    }

    fn was_opened(path: &Path) -> bool {
        OPEN_ATTEMPTS
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|attempts| attempts.contains(path))
    }

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

    /// The defect this pair exists for: the host's own variable said `.claude-personal` while the
    /// sidecar's CLI wrote under `.claude-work`, so the resumed session found no transcript.
    /// A configured account wins over the inherited variable -- that is the whole fix.
    #[test]
    fn a_configured_account_decides_the_projects_dir_not_this_processs_own_variable() {
        let _guard = CLAUDE_CONFIG_DIR_TEST_LOCK.lock().unwrap();
        unsafe { std::env::set_var("CLAUDE_CONFIG_DIR", "/home/user/.claude-personal") };
        let account =
            crate::account::ClaudeAccount::resolve_from("work", |k| (k == "HOME").then(|| "/home/user".to_string()))
                .unwrap();
        assert_eq!(
            projects_dir_for(Some(&account)).unwrap(),
            PathBuf::from("/home/user/.claude-work/projects")
        );
        unsafe { std::env::remove_var("CLAUDE_CONFIG_DIR") };
    }

    /// The other direction, and the one that matters for everyone who configures nothing: with no
    /// account pinned, this function is the function it was before accounts existed.
    #[test]
    fn with_no_account_the_resolution_is_byte_identical_to_the_environment_only_rule() {
        let _guard = CLAUDE_CONFIG_DIR_TEST_LOCK.lock().unwrap();
        unsafe { std::env::set_var("CLAUDE_CONFIG_DIR", "/home/user/.claude-personal") };
        assert_eq!(
            projects_dir_for(None).unwrap(),
            PathBuf::from("/home/user/.claude-personal/projects")
        );
        // ... and the `$HOME/.claude` fallback underneath it, unchanged.
        unsafe { std::env::remove_var("CLAUDE_CONFIG_DIR") };
        let home = std::env::var("HOME").unwrap();
        assert_eq!(
            projects_dir_for(None).unwrap(),
            PathBuf::from(home).join(".claude").join("projects")
        );
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

    // ---- newest_ai_title (design §3.1.3's Amendment) -----------------------------------------

    /// A transcript to read a title out of, in its own temp directory. Returns the file's path.
    fn a_transcript(lines: &str) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("agent-ai-title-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session.jsonl");
        std::fs::write(&path, lines).unwrap();
        (dir, path)
    }

    /// The NEWEST title wins, not the first one the scan meets. Measured (design §3.1.3): a real
    /// session rewrites the same title a median of 24 times and up to 808, so "which one" is a
    /// question every read answers, even though this machine has never seen the value change.
    #[test]
    fn the_last_ai_title_line_in_the_window_is_the_one_taken() {
        let (dir, path) = a_transcript(concat!(
            r#"{"type":"ai-title","aiTitle":"first"}"#,
            "\n",
            r#"{"type":"user","message":{"role":"user","content":"hi"}}"#,
            "\n",
            r#"{"type":"ai-title","aiTitle":"second"}"#,
            "\n",
        ));
        assert_eq!(newest_ai_title(&path).as_deref(), Some("second"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A transcript with no `ai-title` at all reads as `None` -- the ladder's whole fallback
    /// promise (design §11's invariant 17). Indistinguishable, on purpose, from a window that
    /// simply did not reach far enough back; see `a_title_outside_the_window_is_not_chased`.
    #[test]
    fn a_transcript_with_no_ai_title_reads_as_none() {
        let (dir, path) = a_transcript(concat!(
            r#"{"type":"user","message":{"role":"user","content":"hi"}}"#,
            "\n"
        ));
        assert_eq!(newest_ai_title(&path), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Someone else's process writes this file, so its value is input, not a promise. A non-string,
    /// a blank string and a string of nothing but zero-width characters all decline the title --
    /// through `title_from_prompt`, the same normalizer every other title goes through, rather
    /// than through a second rule written here.
    #[test]
    fn an_unusable_ai_title_value_declines_rather_than_rendering() {
        for line in [
            r#"{"type":"ai-title","aiTitle":42}"#,
            r#"{"type":"ai-title","aiTitle":"   "}"#,
            "{\"type\":\"ai-title\",\"aiTitle\":\"\u{200B}\u{200B}\"}",
            r#"{"type":"ai-title"}"#,
        ] {
            let (dir, path) = a_transcript(&format!("{line}\n"));
            assert_eq!(newest_ai_title(&path), None, "{line}");
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    /// A missing, unopenable transcript is `None`, never an `Err`: the picker has three levels to
    /// fall back through and no way to act on an error.
    #[test]
    fn an_unreadable_transcript_is_none_rather_than_an_error() {
        assert_eq!(newest_ai_title(Path::new("/tmp/no-such-transcript-91827.jsonl")), None);
    }

    /// A path that is not a regular file declines **without opening it**.
    ///
    /// This function runs on the GTK main loop, and `File::open` on a FIFO blocks in `open(2)`
    /// until a writer appears -- a whole-window freeze while the resume picker is being built,
    /// with nothing printed and nothing to cancel. A `metadata` call is a `stat` and does not
    /// block, so the guard is asking before opening.
    ///
    /// A directory stands in for the FIFO: it needs no `mkfifo` and no second thread to avoid
    /// hanging the test itself, and it reaches the same guard on the same line -- `metadata`
    /// cannot tell the two apart in any way this code branches on. A test that really opened a
    /// FIFO would be a test that hangs forever when the guard is removed, which is the one shape
    /// a regression test must not have.
    ///
    /// **The assertion is that no open was ATTEMPTED, not that the result was `None`** (re-review,
    /// 2026-09-20). The first version of this test asserted `None` and could not fail: with the
    /// guard deleted a directory still declines, because `File::open` returns `EISDIR` and the
    /// `.ok()?` swallows it. The reviewer deleted the guard and the whole module stayed green. A
    /// FIFO would not have declined -- it would have hung -- and that difference is exactly what
    /// `None` cannot see and `OPEN_ATTEMPTS` can.
    ///
    /// The second half is the positive control, and it is what stops this test from passing
    /// because the probe itself stopped recording: a real transcript in the same run must appear
    /// in `OPEN_ATTEMPTS`. Without it, a `note_open_attempt` that did nothing would look like a
    /// guard that works.
    #[test]
    fn a_path_that_is_not_a_regular_file_is_not_even_opened() {
        let dir = crate::state_dirs::test_workspace_dir("ai-title-not-a-file");
        assert!(dir.is_dir(), "the stand-in has to be a real non-file that exists");
        assert_eq!(newest_ai_title(&dir), None);
        assert!(
            !was_opened(&dir),
            "a non-regular path must be refused before the open, or a FIFO here freezes the GTK main loop"
        );

        let (tmp, path) = a_transcript(concat!(r#"{"type":"ai-title","aiTitle":"a real one"}"#, "\n"));
        assert_eq!(newest_ai_title(&path).as_deref(), Some("a real one"));
        assert!(
            was_opened(&path),
            "positive control: the probe has to record a real open, or the assertion above is vacuous"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// **The head-vs-tail choice, as a test rather than a comment.** Measured over this machine's
    /// 37 titled sessions (design §3.1.3's Amendment ①): reading the file's HEAD finds the title
    /// 0 times out of 37, because a session opens with `attachment` lines and the first `ai-title`
    /// already sits past 117 KB; reading its TAIL finds it 37 times out of 37. This pins the
    /// direction with a file shaped like that: a title near the end, and a head window full of
    /// something else.
    #[test]
    fn the_window_reads_the_tail_and_a_head_window_would_have_missed_it() {
        let filler = format!(
            "{}\n",
            serde_json::json!({"type": "attachment", "content": "x".repeat(4096)})
        );
        let head = filler.repeat(40); // well past AI_TITLE_TAIL_BYTES
        let (dir, path) = a_transcript(&format!(
            "{head}{}\n",
            r#"{"type":"ai-title","aiTitle":"near the end"}"#
        ));
        assert_eq!(newest_ai_title(&path).as_deref(), Some("near the end"));
        // The same file read from the FRONT, with the same window: nothing. Not a hypothetical --
        // this is the read that scored 0/37 on real sessions.
        let mut front = vec![0u8; AI_TITLE_TAIL_BYTES as usize];
        use std::io::Read;
        std::fs::File::open(&path).unwrap().read_exact(&mut front).unwrap();
        assert!(
            !String::from_utf8_lossy(&front).contains("ai-title"),
            "the head window must not contain the title, or this test proves nothing"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A title further back than the window is **deliberately** not found, and the read does not
    /// fall back to scanning the whole file. Design §3.1.3's Amendment ① calls this miss
    /// undetectable and acceptable: the row drops one level down the ladder and nothing else
    /// happens. Without this test the bound reads like an oversight somebody should "fix", which
    /// would put an unbounded read of someone else's file on the GTK main loop.
    #[test]
    fn a_title_outside_the_window_is_not_chased() {
        let filler = format!(
            "{}\n",
            serde_json::json!({"type": "attachment", "content": "x".repeat(4096)})
        );
        let (dir, path) = a_transcript(&format!(
            "{}\n{}",
            r#"{"type":"ai-title","aiTitle":"too far back"}"#,
            filler.repeat(40)
        ));
        assert_eq!(newest_ai_title(&path), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The first line of a seeked window is a fragment of whatever line straddles the window's
    /// start, so it is dropped. A file SMALLER than the window is not seeked and keeps its first
    /// line -- the two halves of one rule, and getting the second wrong would lose the title of
    /// every short session.
    #[test]
    fn the_partial_first_line_is_dropped_only_when_the_window_was_seeked() {
        let filler = format!(
            "{}\n",
            serde_json::json!({"type": "attachment", "content": "x".repeat(4096)})
        );
        // Short file: the very first line is a real, complete `ai-title` and must survive.
        let (short_dir, short) = a_transcript(&format!("{}\n", r#"{"type":"ai-title","aiTitle":"short file"}"#));
        assert_eq!(newest_ai_title(&short).as_deref(), Some("short file"));
        let _ = std::fs::remove_dir_all(&short_dir);
        // Long file: a line straddling the window start is a fragment, and skipping it must not
        // cost the real title that follows.
        let (dir, path) = a_transcript(&format!(
            "{}{}\n",
            filler.repeat(40),
            r#"{"type":"ai-title","aiTitle":"after the fragment"}"#
        ));
        assert_eq!(newest_ai_title(&path).as_deref(), Some("after the fragment"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The real redacted `ai-title` line the history fixtures already carry (see
    /// `agent/tests/fixtures/transcript_lines.README.md`). It was checked in as a side record to be
    /// DISCARDED by the fold; the owner's 2026-09-20 ruling makes the same line this path's
    /// positive sample, and §12's Amendment says to reuse it rather than take a second one.
    #[test]
    fn the_real_redacted_ai_title_line_reads_as_its_title() {
        let raw = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/transcript_lines.jsonl"
        ))
        .expect("the fixture is checked in");
        let (dir, path) = a_transcript(&raw);
        assert_eq!(newest_ai_title(&path).as_deref(), Some("Redacted session title"));
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
