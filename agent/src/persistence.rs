//! Persistent `neovibe_conversation_id -> provider_session_id` identity (design doc §8.1),
//! surviving process restarts and reboots -- deliberately NOT under `$XDG_RUNTIME_DIR` (tmpfs,
//! cleared on logout; correct for the lease in `lease.rs`, wrong here) or `std::env::temp_dir()`.
//! Small JSON files, no database -- matches this project's own established "small file per key, no
//! DB dependency" style (e.g. `supervisor`'s own socket-based state, `agent`'s own per-conversation
//! hook sockets).
//!
//! **One file per SESSION, in a directory per conversation:**
//! `<conversations_dir>/<conversation_id>/<provider_session_id>.json`. The earlier layout was one
//! `<conversation_id>.json` per workspace directory, which meant a workspace could remember exactly
//! one session and two sessions in it silently overwrote each other -- the newer write won, so the
//! resume offer became "whichever session was written last" rather than "the last one you used".
//!
//! **A record written under that older layout is still READ**, alongside the new directory, so a
//! user who had a resume offer before this change still has it afterwards. It is never written or
//! rewritten in that shape, and never deleted either: `save_conversation_record` only ever writes
//! `<conversation_id>/<provider_session_id>.json`, so the single legacy file simply ages out of the
//! ranking as newer sessions appear. Reading it is nine lines; the alternative considered and
//! rejected was to leave it invisible, which would have silently retired a user's offer with
//! nothing anywhere saying so.
//!
//! **Records are capped** (`MAX_RECORDS_PER_CONVERSATION`). Without a cap a project opened daily
//! accumulates one file per session forever, and `resumable_session` -- which the agent panel calls
//! on the GTK main thread -- reads and parses every one of them.

use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};

/// How many session records one conversation keeps, newest-`updated_at` first.
///
/// A cap rather than no cap because `resumable_session` reads the whole directory on the GTK main
/// thread (`shell::agent_panel`'s `InboundMessage::Ready` handler), so an unbounded directory is an
/// unbounded stall in the editor pane as well as unbounded disk. 16 is well past what any resume UI
/// offers -- only the single most recent offerable record is ever shown -- while still leaving room
/// for several sessions' worth of forensic history in a workspace.
const MAX_RECORDS_PER_CONVERSATION: usize = 16;

/// How long a `write_record` temp file must be untouched before a later write deletes it.
///
/// `write_record` removes its own temp file when the write fails, but a process killed between
/// `File::create` and `rename` cannot: that temp file is orphaned for good. An age threshold is
/// what separates those from a temp file a CONCURRENT writer is using right now -- one minute is
/// several orders of magnitude above the microseconds a real write takes, and the cost of being
/// wrong in the safe direction is one stale file surviving until the next write.
const STALE_TEMP_AGE: std::time::Duration = std::time::Duration::from_secs(60);

/// What one workspace's last conversation was, and enough to offer continuing it.
///
/// The resume key is `provider_session_id` -- Claude's identity -- and deliberately NOT the
/// Verdandi session id. The real end-to-end test settled this: resuming mints a NEW Verdandi
/// session while the Claude session continues, so a record keyed on the Verdandi id would point at
/// something that no longer exists the moment it was used.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConversationRecord {
    pub conversation_id: String,
    pub provider: String,
    /// Claude's session UUID. The only identity that survives a resume.
    pub provider_session_id: String,
    pub canonical_cwd: String,
    pub created_at: String,
    pub updated_at: String,
    /// Whether the provider advertised `resume` the last time this record was written.
    ///
    /// The last-known value, not a guarantee -- it is what makes "does this workspace have a
    /// RESUMABLE session?" answerable before a provider exists, which is when the start screen has
    /// to decide whether to offer the option. It is re-checked against the live handshake at the
    /// moment resume is actually attempted, and a provider that no longer advertises it refuses
    /// there rather than here.
    ///
    /// `#[serde(default)]` because records written before this field existed must still load; they
    /// read as `false`, which correctly means "we cannot claim this is resumable".
    #[serde(default)]
    pub provider_advertised_resume: bool,
}

/// A previous conversation in this workspace that can be offered for continuation.
#[derive(Debug, Clone, PartialEq)]
pub struct ResumableSession {
    pub provider: String,
    pub provider_session_id: String,
    pub updated_at: String,
}

/// Answers "does this workspace have a session worth offering to continue?" without needing a live
/// provider -- the start screen has to decide before one exists.
///
/// A workspace can hold several sessions, so this picks the one with the greatest `updated_at`
/// among the OFFERABLE ones. Filtering before ranking is deliberate: if the most recent session is
/// not offerable (its provider never advertised resume) the answer is the most recent one that is,
/// not `None` -- the user still has a conversation worth continuing.
///
/// Returns `None` rather than an error for every negative case (no records, unreadable records, no
/// record whose provider advertised resume): none of them are failures a user can act on, and all
/// of them mean exactly one thing to the caller -- do not offer it.
///
/// **This does synchronous file I/O on the caller's thread**, and its one product caller
/// (`shell::agent_backend::BackendGreeting::for_kind`, from the agent panel's `Ready` handler) is on
/// the GTK main loop. That is bounded, not free: one `read_dir` plus at most
/// `MAX_RECORDS_PER_CONVERSATION` + 1 small JSON parses. The cap is what makes the bound true --
/// before it existed this grew with every session the workspace had ever had.
pub fn resumable_session(conversation_id: &str) -> Option<ResumableSession> {
    let record = read_conversation_records(conversation_id)
        .into_iter()
        .filter(|r| r.provider_advertised_resume && !r.provider_session_id.trim().is_empty())
        .max_by(|a, b| updated_at_rank(a).cmp(&updated_at_rank(b)))?;
    Some(ResumableSession {
        provider: record.provider,
        provider_session_id: record.provider_session_id,
        updated_at: record.updated_at,
    })
}

/// Orders records by `updated_at`, then by session id.
///
/// `updated_at` is parsed rather than compared as text: `conversation::epoch_millis` writes
/// milliseconds since the epoch, and nothing pins that width -- a stamp in some other shape (the
/// ISO-8601 strings this module's own older records carry) would sort above every real one purely
/// because `'2' > '1'`. `None` orders below every parsed value, so a stamp we cannot place in time
/// never wins by accident.
///
/// The second component is `provider_session_id`, NOT `updated_at` again. Comparing `updated_at`
/// against itself is not a tiebreak at all -- two records with byte-identical stamps would compare
/// `Equal` and `max_by` would return whichever `read_dir` happened to yield last. Session ids are
/// unique within a conversation directory (each one IS its filename), so this makes the order total
/// and the answer genuinely reproducible. Which of two same-millisecond sessions wins is arbitrary;
/// that it is the same one every time is the property being bought.
fn updated_at_rank(record: &ConversationRecord) -> (Option<u128>, &str) {
    (record.updated_at.parse::<u128>().ok(), record.provider_session_id.as_str())
}

/// Every record stored for one conversation, skipping anything that will not parse.
///
/// Skipping rather than failing is the point: a half-written or hand-edited file must not hide
/// every other session in the same workspace. An empty vector covers "no such directory" too --
/// both mean the same thing to the one caller, which is that there is nothing to offer.
///
/// Reads BOTH layouts: the current `<conversation_id>/<provider_session_id>.json` directory, and
/// the single pre-2026-09-15 `<conversation_id>.json` beside it. See this module's header for why
/// the legacy file is read but never written or removed.
fn read_conversation_records(conversation_id: &str) -> Vec<ConversationRecord> {
    let Ok(dir) = conversation_dir(conversation_id) else { return Vec::new() };
    let mut records: Vec<ConversationRecord> = std::fs::read_dir(&dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            // `.json` only -- this also skips the `write_record` temp files, which are named with a
            // `.tmp` extension precisely so a reader arriving mid-write cannot pick one up.
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                return None;
            }
            serde_json::from_str(&std::fs::read_to_string(&path).ok()?).ok()
        })
        .collect();
    if let Some(legacy) = read_legacy_record(&dir) {
        records.push(legacy);
    }
    records
}

/// The one pre-2026-09-15 `<conversation_id>.json`, if this workspace still has one.
///
/// `dir` is the per-conversation directory, so the legacy file is its sibling of the same name plus
/// `.json`. A record whose `provider_session_id` already appears in the directory is NOT filtered
/// out here: the ranking prefers the newer stamp on its own, and an ISO-8601 legacy stamp cannot
/// parse, so it already orders below every record written since.
fn read_legacy_record(dir: &Path) -> Option<ConversationRecord> {
    let legacy = dir.with_extension("json");
    serde_json::from_str(&std::fs::read_to_string(legacy).ok()?).ok()
}

/// `$XDG_STATE_HOME/neovibe/conversations/` -- see `state_dirs` for the full rule, and for the one
/// redirect this crate's tests use so a `cargo test` run never writes into the developer's real
/// state directory.
pub(crate) fn conversations_dir() -> std::io::Result<PathBuf> {
    crate::state_dirs::conversations_dir()
}

/// Rejects any path component that is empty or contains a character outside `[A-Za-z0-9_-]` --
/// `record_path` joins both the conversation id and the provider session id with no other
/// validation, so a value shaped like `../../../../tmp/pwned` would otherwise escape
/// `conversations_dir()` entirely. `lease.rs` solves the same class of problem by hashing its key
/// into an opaque filename; this module validates instead of hashing so the on-disk files stay
/// human-readable (`<conversation_id>/<provider_session_id>.json`) for debugging.
///
/// Both components really do pass: the conversation id is 32 hex chars, and a Claude provider
/// session id is a UUID, i.e. hex digits and `-` (pinned by
/// `a_real_claude_session_uuid_is_a_valid_path_component` below).
fn validate_path_component(label: &str, value: &str) -> std::io::Result<()> {
    let valid = !value.is_empty() && value.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if valid {
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("invalid {label} {value:?}: must be non-empty and contain only [A-Za-z0-9_-]"),
        ))
    }
}

fn validate_conversation_id(conversation_id: &str) -> std::io::Result<()> {
    validate_path_component("conversation_id", conversation_id)
}

/// The directory holding every session record for one conversation.
fn conversation_dir(conversation_id: &str) -> std::io::Result<PathBuf> {
    validate_conversation_id(conversation_id)?;
    Ok(conversations_dir()?.join(conversation_id))
}

fn record_path(conversation_id: &str, provider_session_id: &str) -> std::io::Result<PathBuf> {
    validate_path_component("provider_session_id", provider_session_id)?;
    Ok(conversation_dir(conversation_id)?.join(format!("{provider_session_id}.json")))
}

pub fn save_conversation_record(record: &ConversationRecord) -> std::io::Result<()> {
    let path = record_path(&record.conversation_id, &record.provider_session_id)?;
    // `record_path` always joins onto a directory, so the parent is never `None`.
    let dir = path.parent().expect("a record path always has a parent directory").to_path_buf();
    std::fs::create_dir_all(&dir)?;
    let json = serde_json::to_string_pretty(record).map_err(std::io::Error::other)?;
    write_record(&path, json.as_bytes())?;
    // After the write, never before: a failed prune must not cost the caller its record, and the
    // record just written has to be part of the set being ranked.
    prune(&dir, &path);
    Ok(())
}

/// Writes through a temp file in the SAME directory, then renames over the target.
///
/// `std::fs::write` truncates in place, so a crash (or a reader arriving mid-write) between the
/// truncate and the last byte leaves a file that parses as nothing -- which `resumable_session`
/// would read as "this workspace has no session at all". A rename within one directory is atomic,
/// so a reader sees either the whole old record or the whole new one and never a partial file.
///
/// Surviving a POWER LOSS needs both fsyncs, not just the one this used to have: `sync_all` on the
/// temp file puts its contents on the medium, and the `sync_all` on the DIRECTORY afterwards puts
/// the rename itself there. Without the second, a power loss can lose the new name entirely and
/// leave the old record (or no record) behind however thoroughly the bytes were flushed -- file
/// contents and the directory entry pointing at them are two separate pieces of metadata. The
/// comment here previously claimed the first fsync alone bought this; it never did.
///
/// The directory fsync is best-effort, so the honest statement is narrower than "records survive a
/// power loss": no reader ever sees a partial record, and a record survives a power loss **when the
/// directory fsync succeeds**. It is not worth failing a save over, since a filesystem that refuses
/// it has already produced a record that is correct and visible in every way a running system can
/// observe.
///
/// No lock is taken, and none is needed: each session writes its OWN file, so two sessions in one
/// workspace are not two writers of one path.
fn write_record(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = path.parent().expect("a record path always has a parent directory");
    // `.tmp` extension, not `.json`: `read_conversation_records` filters on the extension, so a
    // reader that lists the directory mid-write cannot pick this up as a record.
    let temp = dir.join(format!(".{}.tmp", uuid::Uuid::new_v4()));
    let written = (|| {
        let mut file = std::fs::File::create(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()
    })();
    if let Err(e) = written.and_then(|()| std::fs::rename(&temp, path)) {
        // Leaving a temp file behind would accumulate one per failed write forever.
        let _ = std::fs::remove_file(&temp);
        return Err(e);
    }
    // The rename itself, not the bytes. Best-effort -- see this function's doc for exactly what
    // that narrows the durability claim to.
    if let Ok(handle) = std::fs::File::open(dir) {
        let _ = handle.sync_all();
    }
    Ok(())
}

/// Keeps one conversation's directory bounded: at most `MAX_RECORDS_PER_CONVERSATION` records, plus
/// a sweep of temp files orphaned by a process that died mid-write.
///
/// `keep` is the record just written, which is never a prune candidate however its stamp ranks --
/// deleting the record a caller just asked to save would be the one genuinely surprising outcome.
/// Everything here is best-effort and silent on failure: this is housekeeping, and a save that
/// succeeded must not be reported as failed because a stale file could not be unlinked.
fn prune(dir: &Path, keep: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut records: Vec<(Option<u128>, String, PathBuf)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        match path.extension().and_then(|e| e.to_str()) {
            Some("tmp") => {
                let orphaned = entry
                    .metadata()
                    .and_then(|m| m.modified())
                    .and_then(|m| std::time::SystemTime::now().duration_since(m).map_err(std::io::Error::other))
                    .map(|age| age > STALE_TEMP_AGE)
                    .unwrap_or(false);
                if orphaned {
                    let _ = std::fs::remove_file(&path);
                }
            }
            Some("json") if path != keep => {
                let Ok(text) = std::fs::read_to_string(&path) else { continue };
                let Ok(record) = serde_json::from_str::<ConversationRecord>(&text) else {
                    // Unparseable files are left alone rather than deleted: this is the only copy
                    // of whatever is in them, and `read_conversation_records` already skips them.
                    continue;
                };
                let (stamp, _) = updated_at_rank(&record);
                records.push((stamp, record.provider_session_id, path));
            }
            _ => {}
        }
    }
    if records.len() < MAX_RECORDS_PER_CONVERSATION {
        return;
    }
    // Same order `resumable_session` ranks by, so what is dropped is exactly what that would never
    // have offered. `keep` is excluded above and occupies one of the cap's slots.
    records.sort_by(|a, b| (a.0, a.1.as_str()).cmp(&(b.0, b.1.as_str())));
    let over = records.len() + 1 - MAX_RECORDS_PER_CONVERSATION;
    for (_, _, path) in records.into_iter().take(over) {
        let _ = std::fs::remove_file(path);
    }
}

pub fn load_conversation_record(
    conversation_id: &str,
    provider_session_id: &str,
) -> std::io::Result<ConversationRecord> {
    let path = record_path(conversation_id, provider_session_id)?;
    let contents = std::fs::read_to_string(path)?;
    serde_json::from_str(&contents).map_err(std::io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A conversation id no other test in this binary uses, inside this process's disposable state
    /// root.
    ///
    /// This replaces a helper that set `XDG_STATE_HOME` with `std::env::set_var` and removed it
    /// again. That was process-wide in a multi-threaded test binary -- it could unset the variable
    /// out from under any other test reading it on another thread, and its comment claimed the
    /// single-function shape prevented that, which was never true of a process-wide mutation. The
    /// redirect in `state_dirs` is set once and never cleared, so there is nothing to race on; a
    /// unique id per scenario is what keeps the scenarios from reading each other's records.
    fn unique_conversation_id(label: &str) -> String {
        crate::state_dirs::redirect_state_to_a_test_root();
        format!("{label}-{}", uuid::Uuid::new_v4().simple())
    }

    #[test]
    fn a_record_written_before_provider_advertised_resume_existed_still_loads() {
        // Records on disk predate this field. Without serde's default they would fail to
        // deserialize, which would silently look like "this workspace has no previous session".
        let json = r#"{
            "conversation_id": "old",
            "provider": "claude",
            "provider_session_id": "prov-old",
            "canonical_cwd": "/tmp/project",
            "created_at": "1",
            "updated_at": "2"
        }"#;
        let record: ConversationRecord = serde_json::from_str(json).expect("an older record must still load");
        assert_eq!(record.provider_session_id, "prov-old");
        assert!(!record.provider_advertised_resume, "unknown resumability must read as not-resumable");
    }

    #[test]
    fn a_path_traversal_shaped_conversation_id_is_rejected() {
        let result = validate_conversation_id("../../../../tmp/pwned");
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::InvalidInput);

        // Also confirmed at the public entry points, not just the private helper directly --
        // this is a pure string check, no XDG_STATE_HOME isolation needed.
        let record = ConversationRecord {
            conversation_id: "../../../../tmp/pwned".into(),
            provider: "claude".into(),
            provider_session_id: "prov-1".into(),
            canonical_cwd: "/tmp/project".into(),
            created_at: "2026-09-10T00:00:00Z".into(),
            updated_at: "2026-09-10T00:00:00Z".into(),
            provider_advertised_resume: true,
        };
        assert!(save_conversation_record(&record).is_err());
        assert!(load_conversation_record("../../../../tmp/pwned", "prov-1").is_err());
    }

    /// The session id is now a path component too, so it needs the same guard the conversation id
    /// has always had -- it reaches this module from the provider's own wire, not from a value this
    /// side derived.
    #[test]
    fn a_path_traversal_shaped_provider_session_id_is_rejected() {
        let record = ConversationRecord {
            conversation_id: "conv-1".into(),
            provider: "claude".into(),
            provider_session_id: "../../../../tmp/pwned".into(),
            canonical_cwd: "/tmp/project".into(),
            created_at: "1".into(),
            updated_at: "2".into(),
            provider_advertised_resume: true,
        };
        let error = save_conversation_record(&record).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        assert!(load_conversation_record("conv-1", "../../../../tmp/pwned").is_err());
    }

    /// The premise the per-session layout rests on: a real Claude session id survives being used as
    /// a filename unchanged. Asserted rather than assumed -- if Claude ever mints an id in some
    /// other shape, every save for that session would start failing, and this says why.
    #[test]
    fn a_real_claude_session_uuid_is_a_valid_path_component() {
        assert!(validate_path_component("provider_session_id", "3f2a1b4c-5d6e-4f70-8192-a3b4c5d6e7f8").is_ok());
    }

    /// A record with the given ids, epoch-millis stamps, and resume advertised.
    fn record(conversation_id: &str, provider_session_id: &str, updated_at: &str) -> ConversationRecord {
        ConversationRecord {
            conversation_id: conversation_id.into(),
            provider: "claude".into(),
            provider_session_id: provider_session_id.into(),
            canonical_cwd: "/tmp/project".into(),
            created_at: "1000".into(),
            updated_at: updated_at.into(),
            provider_advertised_resume: true,
        }
    }

    #[test]
    fn a_saved_record_loads_back_exactly() {
        let conv = unique_conversation_id("roundtrip");
        let saved = record(&conv, "prov-1", "1000");
        save_conversation_record(&saved).unwrap();
        assert_eq!(load_conversation_record(&conv, "prov-1").unwrap(), saved);
    }

    #[test]
    fn loading_an_unknown_conversation_is_an_error_and_offers_nothing() {
        let conv = unique_conversation_id("unknown");
        assert!(load_conversation_record(&conv, "prov-1").is_err());
        assert!(resumable_session(&conv).is_none());
    }

    #[test]
    fn saving_the_same_session_twice_overwrites_only_that_session() {
        let conv = unique_conversation_id("overwrite");
        save_conversation_record(&record(&conv, "prov-1", "1000")).unwrap();
        save_conversation_record(&record(&conv, "prov-2", "1500")).unwrap();
        save_conversation_record(&record(&conv, "prov-1", "2000")).unwrap();

        assert_eq!(load_conversation_record(&conv, "prov-1").unwrap().updated_at, "2000");
        assert_eq!(load_conversation_record(&conv, "prov-2").unwrap().updated_at, "1500");
    }

    /// The question the start screen actually asks.
    #[test]
    fn a_record_that_advertised_resume_is_offerable() {
        let conv = unique_conversation_id("offerable");
        save_conversation_record(&record(&conv, "prov-1", "1000")).unwrap();
        let resumable = resumable_session(&conv).expect("a record that advertised resume is offerable");
        assert_eq!(resumable.provider_session_id, "prov-1");
        assert_eq!(resumable.provider, "claude");
    }

    /// A record whose provider did NOT advertise resume is not offerable. This is also how every
    /// record written before the field existed reads (serde default).
    #[test]
    fn a_record_whose_provider_never_advertised_resume_is_not_offerable() {
        let conv = unique_conversation_id("no-resume");
        let mut without = record(&conv, "prov-1", "1000");
        without.provider_advertised_resume = false;
        save_conversation_record(&without).unwrap();
        assert!(resumable_session(&conv).is_none());
    }

    /// A blank provider session id can no longer even be written -- it is a path component now, so
    /// the validator rejects it before anything touches the disk. The `trim().is_empty()` guard in
    /// `resumable_session` stays regardless, because it also covers a record hand-written or left
    /// over from the older layout.
    #[test]
    fn a_blank_provider_session_id_cannot_be_written_and_is_never_offered() {
        let conv = unique_conversation_id("blank");
        assert!(save_conversation_record(&record(&conv, "   ", "1000")).is_err());
        assert!(resumable_session(&conv).is_none());
    }

    /// One workspace remembers MORE THAN ONE session, and the offer is the one whose `updated_at`
    /// is greatest -- not merely the most recently written file.
    ///
    /// Written newest-first on purpose. The pre-2026-09-15 layout kept one file per workspace, so
    /// the second save clobbered the first and the offer became whichever save happened last, which
    /// is the opposite answer.
    ///
    /// `updated_at` means "when this session was last started or resumed", not "when it last had
    /// activity": `conversation::persist_record` runs on first adoption and on resume, and nothing
    /// refreshes it during a conversation.
    #[test]
    fn a_workspace_with_two_sessions_offers_the_one_with_the_greatest_updated_at() {
        let conv = unique_conversation_id("multi");
        save_conversation_record(&record(&conv, "prov-newer", "2000")).unwrap();
        save_conversation_record(&record(&conv, "prov-older", "1000")).unwrap();
        let offered = resumable_session(&conv).expect("a workspace with two sessions still offers one");
        assert_eq!(offered.provider_session_id, "prov-newer");
    }

    /// Two records with byte-identical stamps must still resolve the same way every time.
    ///
    /// The previous `updated_at_rank` compared `updated_at` against itself as its "tiebreak", so
    /// this case fell through to `read_dir` order and the answer could differ between runs on one
    /// unchanged directory.
    #[test]
    fn two_records_with_identical_stamps_resolve_deterministically() {
        let conv = unique_conversation_id("tie");
        save_conversation_record(&record(&conv, "prov-aaa", "2000")).unwrap();
        save_conversation_record(&record(&conv, "prov-zzz", "2000")).unwrap();
        let first = resumable_session(&conv).unwrap().provider_session_id;
        for _ in 0..5 {
            assert_eq!(resumable_session(&conv).unwrap().provider_session_id, first);
        }
        // Which one wins is arbitrary; that it is decided by the session id rather than by
        // directory order is what makes it reproducible.
        assert_eq!(first, "prov-zzz");
    }

    /// A record that cannot be parsed is skipped, not fatal. A half-written or hand-edited file
    /// must not hide every other session in the same workspace.
    #[test]
    fn one_unreadable_record_does_not_hide_the_rest() {
        let conv = unique_conversation_id("corrupt");
        save_conversation_record(&record(&conv, "prov-good", "2000")).unwrap();
        std::fs::write(conversations_dir().unwrap().join(&conv).join("prov-corrupt.json"), "{ this is not json").unwrap();
        let offered = resumable_session(&conv).expect("one unreadable record must not hide the rest");
        assert_eq!(offered.provider_session_id, "prov-good");
    }

    /// A user who had a resume offer under the pre-2026-09-15 `<conversation_id>.json` layout still
    /// has it. The alternative shipped for a day and abandoned it silently: nothing warned anyone
    /// their "continue previous session" offer had gone.
    #[test]
    fn a_record_in_the_old_single_file_layout_is_still_offered() {
        let conv = unique_conversation_id("legacy");
        let dir = conversations_dir().unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        let legacy = record(&conv, "prov-legacy", "2026-09-10T00:00:00Z");
        std::fs::write(dir.join(format!("{conv}.json")), serde_json::to_string(&legacy).unwrap()).unwrap();

        let offered = resumable_session(&conv).expect("an old-layout record must still be offered");
        assert_eq!(offered.provider_session_id, "prov-legacy");
    }

    /// ...and it loses to any record written since, because its ISO-8601 stamp cannot be placed in
    /// time at all and so ranks below every epoch-millis one.
    #[test]
    fn a_new_layout_record_outranks_the_old_single_file_one() {
        let conv = unique_conversation_id("legacy-vs-new");
        let dir = conversations_dir().unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        let legacy = record(&conv, "prov-legacy", "2026-09-10T00:00:00Z");
        std::fs::write(dir.join(format!("{conv}.json")), serde_json::to_string(&legacy).unwrap()).unwrap();
        save_conversation_record(&record(&conv, "prov-new", "1000")).unwrap();

        assert_eq!(resumable_session(&conv).unwrap().provider_session_id, "prov-new");
    }

    /// Records are capped, so a project opened daily for a year does not turn the agent panel's
    /// `Ready` handler into an unbounded directory read on the GTK main thread.
    #[test]
    fn records_are_capped_and_the_oldest_are_dropped_first() {
        let conv = unique_conversation_id("cap");
        for i in 0..(MAX_RECORDS_PER_CONVERSATION as u32 + 5) {
            save_conversation_record(&record(&conv, &format!("prov-{i:03}"), &(1000 + i).to_string())).unwrap();
        }
        let dir = conversations_dir().unwrap().join(&conv);
        let kept: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(kept.len(), MAX_RECORDS_PER_CONVERSATION, "got: {kept:?}");
        assert!(kept.contains(&"prov-020.json".to_string()), "the newest must survive: {kept:?}");
        assert!(!kept.contains(&"prov-000.json".to_string()), "the oldest must be dropped: {kept:?}");
        // And the offer is still the newest of what is left.
        assert_eq!(resumable_session(&conv).unwrap().provider_session_id, "prov-020");
    }

    /// A temp file orphaned by a process killed mid-write is swept; one a concurrent writer may
    /// still be using is not. `write_record` can only clean up after a failure it survives.
    #[test]
    fn a_stale_temp_file_is_swept_and_a_fresh_one_is_left_alone() {
        let conv = unique_conversation_id("temp");
        save_conversation_record(&record(&conv, "prov-1", "1000")).unwrap();
        let dir = conversations_dir().unwrap().join(&conv);

        let fresh = dir.join(".fresh.tmp");
        let stale = dir.join(".stale.tmp");
        std::fs::write(&fresh, b"x").unwrap();
        std::fs::write(&stale, b"x").unwrap();
        let old = std::time::SystemTime::now() - (STALE_TEMP_AGE * 2);
        std::fs::File::open(&stale).unwrap().set_modified(old).unwrap();

        save_conversation_record(&record(&conv, "prov-2", "2000")).unwrap();

        assert!(!stale.exists(), "an orphaned temp file must be swept");
        assert!(fresh.exists(), "a temp file a concurrent writer may still hold must be left alone");
    }
}
