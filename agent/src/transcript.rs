//! Locates Claude CLI transcripts and reads conversation history for display after resume.
//! [`claude_projects_dir`] resolves the projects directory using `CLAUDE_CONFIG_DIR` when set.
//! Resume stability depends only on the file's modification time ([`is_transcript_stable`]),
//! never on transcript content.
//!
//! [`crate::history::store`] provides a fallback, but only covers sessions driven by this
//! application since history storage was added. CLI transcripts also cover other sessions.
//! Session files are named `<uuid>.jsonl`, as built by [`transcript_path`]. Subagent logs named
//! `agent-<hex>.jsonl` carry their parent's `sessionId` and contain no main-chain conversation.
//!
//! Only `user` and `assistant` entries become conversation messages. Other entries include
//! system messages, attachments and side data with no wire-protocol counterpart. The `version`
//! field identifies the writing CLI, not a schema. Report it when parsing yields nothing;
//! do not use it to select a parser.
//!
//! A `type:"user"` entry may contain injected text rather than a typed prompt. Accept only
//! `promptSource` values `typed`, `suggestion_accepted`, `queued` and `sdk`, with no exclusion
//! flags, to avoid displaying injected material as the user's words. Compaction uses a user
//! entry with `isCompactSummary: true` and a system entry with `subtype:"compact_boundary"`.
//!
//! Files under `CLAUDE_CONFIG_DIR` are read-only here. Unknown entry types, unknown content
//! blocks and malformed lines are skipped and counted. Format changes reduce the displayed
//! history with a diagnostic rather than aborting a resume.
//!
//! Transcript content is never persisted or used for resume safety, permission decisions or
//! turns sent to the model. It supplies panel history and a display-only resume-picker title.
//! [`newest_ai_title`] reads the newest `type:"ai-title"` entry in the last 64 KiB each time
//! the picker is built. [`crate::persistence::title_from_prompt`] normalizes the title.
//! The result is never written into a `ConversationRecord`; the stored title stays first-one-wins.
//! Display falls back to the stored title and then the bare id when no CLI title is available.
//!
//! Transcript bytes are untrusted model and tool output from another process. Render them
//! through the same DOMPurify and markdown path as other messages.

use std::path::{Path, PathBuf};
use std::time::Duration;

/// Errors with `ErrorKind::NotFound` when neither `CLAUDE_CONFIG_DIR` nor `HOME` is set, rather
/// than silently falling back to the relative path `projects` -- matches `persistence.rs`'s
/// `conversations_dir()`, which already errors the same way for the same condition.
///
/// **Correction (2026-09-21): this process's own `CLAUDE_CONFIG_DIR` is the *fallback*, not the
/// answer.** When [`crate::account`] has been given an account -- `init.lua`'s
/// `neovibe.config.set("agent.account", ...)` -- the directory comes from that account's own
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
/// 103 of the same 111; the difference is real, not noise: an underscore (e.g. `my_notes` ->
/// `my-notes`) and non-ASCII path segments (e.g. `あいう` -> `---`, one `-` per character,
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

/// Checks whether the transcript's modification time stays unchanged across `probe_delay`.
/// Returns `Ok(true)` if unchanged and `Ok(false)` if it changes, so callers can refuse
/// resume while the file is being written. A missing file returns `Err` because it is a
/// different problem from an unstable transcript.
pub fn is_transcript_stable(path: &Path, probe_delay: Duration) -> std::io::Result<bool> {
    let before = std::fs::metadata(path)?.modified()?;
    std::thread::sleep(probe_delay);
    let after = std::fs::metadata(path)?.modified()?;
    Ok(before == after)
}

/// Bounds how many bytes [`newest_ai_title`] reads from the end of a transcript.
///
/// In a sample of 44 session files, the newest title was at most 33,157 bytes from EOF.
/// A 64 KiB tail matched a full scan for all 37 titled sessions; a head read found none,
/// because initial attachment data put the first title beyond the head window.
///
/// This is a bounded heuristic. Enough data after the last title pushes it outside the
/// window, making it indistinguishable from an absent title. The picker then uses its
/// next title fallback. Expanding the read without a bound would block the GTK main loop
/// on an arbitrarily large transcript.
pub const AI_TITLE_TAIL_BYTES: u64 = 64 * 1024;

/// Returns the CLI title for display in the resume picker, or `None`.
/// Reads the last [`AI_TITLE_TAIL_BYTES`] of `path` and takes the `aiTitle` from the last
/// `type:"ai-title"` entry in that window, so the newest title in the window wins.
///
/// The result is never written into a `ConversationRecord`. The stored title remains
/// first-one-wins and `updated_at` is untouched. Every picker build reads the file again.
///
/// The caller runs on the GTK main loop. The persistence retention cap bounds the picker
/// to `MAX_RECORDS_PER_CONVERSATION` + 1 rows, so this adds one fixed-size read per row.
/// Parsing entire transcripts would make main-loop work grow with externally written files.
///
/// [`is_transcript_stable`] sleeps and would stall the main loop, so there is no stability
/// probe here. A partial last line is skipped like any malformed line; the row can fall
/// back to its stored title or id.
///
/// Missing or unreadable files, seek failures, invalid UTF-8, malformed JSON and non-string
/// or blank titles yield `None`. The caller can use its title fallbacks.
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

/// Serializes tests that mutate the process-wide `CLAUDE_CONFIG_DIR` environment variable.
/// Tests in this file and `external_writer.rs` need controlled values, so parallel execution
/// would otherwise let their mutations interleave. A shared lock protects both files.
/// `Mutex::new` is const, so the static guard needs no lazy initialization or extra dependency.
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
    /// stays green with the guard deleted -- a blind spot that hides attempted opens and
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
        assert_eq!(sanitize_cwd_for_claude_projects("/home/x/あいう"), "-home-x----");
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
        // Propagate poisoning of this shared test lock with `unwrap()`.
        // A previous panic while holding it should fail this test too.
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

    // ---- newest_ai_title -------------------------------------------------------------------

    /// A transcript to read a title out of, in its own temp directory. Returns the file's path.
    fn a_transcript(lines: &str) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("agent-ai-title-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session.jsonl");
        std::fs::write(&path, lines).unwrap();
        (dir, path)
    }

    /// The last `ai-title` line in the window wins.
    /// Repeated title entries must not make the scan return an older value.
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

    /// A transcript without an `ai-title` returns `None`, allowing the next title fallback.
    /// This is also the result when the title is outside the read window;
    /// see `a_title_outside_the_window_is_not_chased`.
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
    /// **The assertion is that no open was ATTEMPTED, not that the result was `None`** (since
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

    /// Reads the tail rather than the head so initial attachment data cannot hide a recent title.
    /// The fixture puts a title near EOF and fills the head window with attachment lines.
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

    /// A title outside the window is not found, and the read never expands to the whole file.
    /// That miss looks like an absent title, so the picker uses the next title fallback.
    /// Keeping the read bounded prevents an arbitrarily large transcript from blocking the GTK main loop.
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

    /// Reuses the redacted `ai-title` entry in the history fixture as a positive title sample.
    /// The history fold discards this entry, while `newest_ai_title` extracts its title.
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
