//! Persistent `eitri_conversation_id -> provider_session_id` identity (design doc §8.1),
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
//! accumulates one file per session forever, and `resumable_sessions` -- which the agent panel calls
//! on the GTK main thread, and whose result is one row each on the start screen's conversation
//! picker -- reads and parses every one of them.

use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};

/// How many session records one conversation keeps, newest-`updated_at` first.
///
/// A cap rather than no cap because `resumable_sessions` reads the whole directory on the GTK main
/// thread (`eitri_panel::agent_panel`'s `InboundMessage::Ready` handler), so an unbounded directory is an
/// unbounded stall in the editor pane as well as unbounded disk.
///
/// It is also very nearly the bound on the resume PICKER: every offerable record is a row on the
/// start screen. The list can reach this cap PLUS ONE, because `prune` bounds the per-conversation
/// DIRECTORY and `read_conversation_records` then appends the one pre-2026-09-15 sibling file,
/// which nothing prunes -- so 17 rows, not 16. (This comment previously read "16 is well past what
/// any resume UI offers -- only the single most recent offerable record is ever shown", which was
/// true of the singular offer and stopped being true when the picker landed.) 16 rows is a list a
/// person can still read, and dropping the number is a product decision rather than a correctness
/// one -- nothing breaks at any value, older sessions simply stop being offerable sooner.
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
    /// The first line of the first prompt typed in this session, cut to `TITLE_MAX_CHARS`: what the
    /// resume picker names the row by (the owner's choice, 2026-09-19: "存首句当标题"). Written from
    /// the prompt AS TYPED -- never the editor context composed above it -- and never rewritten once
    /// set, so a resume keeps the session's original subject (`set_title_if_missing`,
    /// `conversation::persist_record`).
    ///
    /// `None` for a record written before this field existed, and for a session whose first prompt
    /// had not been sent when the record was last written. Absent from the JSON rather than `null`
    /// when `None`, so a record without one reads exactly as it did before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// The session tab's rename (`prefix ,`, session tabs spec §3.5): what the chooser shows ahead
    /// of the title. The last rename wins and an empty one clears it (`set_name`), unlike `title`,
    /// which is first-one-wins. Absent from the JSON when `None`, so an older record reads as before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// The longest title stored, in characters (not bytes: most of the owner's prompts are Chinese). A
/// row on the start screen is one line of a narrow panel; this is about two of them, and the CSS
/// does the rest.
pub const TITLE_MAX_CHARS: usize = 80;

/// A title for a session from the prompt that began it: its first line with visible text, with runs
/// of whitespace collapsed to one space, cut to `TITLE_MAX_CHARS` characters with `…` when anything
/// was cut. `None` for a prompt with no visible text.
///
/// "Visible" leaves out the zero-width characters a pasted line can consist of (a line of nothing
/// but U+200B would otherwise be a title that renders as a blank row). The cut is by Unicode scalar,
/// not grapheme cluster, so a title cut at character 79 can split an emoji sequence or a combining
/// accent. Cosmetic, and accepted: grapheme segmentation is a dependency for one character.
pub fn title_from_prompt(prompt: &str) -> Option<String> {
    let visible = |c: char| !c.is_whitespace() && !matches!(c, '\u{200B}'..='\u{200D}' | '\u{2060}' | '\u{FEFF}');
    let line = prompt.lines().map(str::trim).find(|l| l.chars().any(visible))?;
    let collapsed = line.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= TITLE_MAX_CHARS {
        return Some(collapsed);
    }
    let cut: String = collapsed.chars().take(TITLE_MAX_CHARS - 1).collect();
    Some(format!("{}…", cut.trim_end()))
}

/// A previous conversation in this workspace that can be offered for continuation.
///
/// **This is the whole of what one record knows about a session, and it is thin**: a provider name,
/// a session id, and two timestamps. There is no title, no first prompt, no turn or message count,
/// no model -- nothing a picker could use to say what a session was ABOUT. **Correction
/// (2026-09-19): there is a title now**, `title` below -- recorded here at write time, exactly the
/// route the last sentence of this paragraph names; the rest of what follows still holds. `ConversationRecord` has
/// never carried any of that, and the one place on disk that does -- the real Claude CLI's own
/// transcript under `~/.claude/projects/` -- is deliberately off limits: `agent::transcript`'s
/// module doc states that it "never reads or interprets transcript *content* -- only the file's
/// existence and modification time", matching this project's repeated decision not to reimplement
/// Claude's private storage format. So a UI built on this can honestly offer "which session" and
/// "when", and must not fabricate "what about". Making richer labels possible means recording more
/// HERE, at write time, not reading someone else's file.
///
/// **Correction (2026-09-20, the owner's ruling).** The two sentences above about the transcript
/// are overturned: it is no longer off limits, and richer labels no longer have to be recorded
/// here. `agent::transcript` READS it now -- see that module's own corrected doc, and constraint 3
/// in particular -- and `BackendGreeting::for_kind` layers the CLI's own `type:"ai-title"` over
/// `title` when it builds the picker. What that changes is only what a row DISPLAYS. This struct
/// and the record behind it are untouched: `title` is still what this project recorded at write
/// time, still first-one-wins, still the only title that reaches disk, and the CLI's is re-read on
/// every greeting and written nowhere.
///
/// **"must not fabricate 'what about'" stands, and is now load-bearing in a second place**: it is
/// why the display ladder has three real levels (the CLI's title, then this one, then the bare id
/// and timestamps) rather than a generated label for a row that has neither.
#[derive(Debug, Clone, PartialEq)]
pub struct ResumableSession {
    pub provider: String,
    pub provider_session_id: String,
    /// When this session was FIRST started. Preserved across resumes by
    /// `conversation::persist_record`, which asks `existing_record` for the stamp the
    /// session already had and only mints a new one when there is no earlier record at all.
    ///
    /// That lookup consults BOTH layouts. It used to read the directory record only, so the first
    /// resume of a session recorded only in the pre-2026-09-15 flat file reset `created_at` to the
    /// moment of the resume -- and `resumable_sessions`' de-duplication then discarded the flat
    /// record that still held the true value, making the loss permanent. The doc here claimed
    /// preservation unconditionally the whole time.
    pub created_at: String,
    /// When this session was last STARTED OR RESUMED -- not when it last had activity. Nothing
    /// rewrites a record during a conversation, so a session used for an hour and a session opened
    /// and abandoned carry the same stamp. Anything rendering this must not call it "last active".
    /// (Setting the title rewrites the record once, and leaves this stamp as it was.)
    pub updated_at: String,
    /// `ConversationRecord::title`. `None` for a session recorded before titles were kept.
    pub title: Option<String>,
    /// `ConversationRecord::name`.
    pub name: Option<String>,
}

/// Every session in this workspace worth offering, newest first.
///
/// Answers "what is there to continue?" without needing a live provider -- the start screen has to
/// decide before one exists. Filtering happens before ranking, so a workspace whose most recent
/// session is not offerable still offers the most recent one that is, rather than nothing.
///
/// An empty vector covers every negative case (no records, unreadable records, no record whose
/// provider advertised resume): none of them are failures a user can act on, and all of them mean
/// exactly one thing to the caller -- there is nothing to offer.
///
/// **De-duplicated by `provider_session_id`**, keeping the higher-ranked record. One Claude session
/// can legitimately be recorded twice (the pre-2026-09-15 single file, plus a per-session record
/// written when it was later resumed); that was invisible while only the maximum was ever taken and
/// would be two identical-looking rows in a list. The row kept is the right one because the flat
/// file has had no writer since 2026-09-15 (`77569d6`) while the directory record for the same
/// session is written on every adoption and resume -- so the directory record is stamped later by
/// `epoch_millis`, and outranks it. (Wall-clock, therefore not a proof: a clock that moved backwards
/// between the two writes would invert it. The cost of that is one row showing the older of two
/// records for one session, which is a cosmetic wrong, not a wrong session.)
///
/// **This does synchronous file I/O on the caller's thread**, and its one product caller
/// (`shell::agent_backend::BackendGreeting::for_kind`, from the agent panel's `Ready` handler) is on
/// the GTK main loop. That is bounded, not free: one `read_dir` plus at most
/// `MAX_RECORDS_PER_CONVERSATION` + 1 small JSON parses. The cap is what makes the bound true --
/// before it existed this grew with every session the workspace had ever had.
pub fn resumable_sessions(conversation_id: &str) -> Vec<ResumableSession> {
    let mut records: Vec<ConversationRecord> = read_conversation_records(conversation_id)
        .into_iter()
        .filter(|r| r.provider_advertised_resume && !r.provider_session_id.trim().is_empty())
        .collect();
    // Descending: `updated_at_rank` orders oldest-to-newest, and a picker leads with the newest.
    records.sort_by(|a, b| updated_at_rank(b).cmp(&updated_at_rank(a)));
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    records
        .into_iter()
        // After the sort, so the row kept for a twice-recorded session is its higher-ranked one.
        .filter(|record| seen.insert(record.provider_session_id.clone()))
        .map(|record| ResumableSession {
            provider: record.provider,
            provider_session_id: record.provider_session_id,
            created_at: record.created_at,
            updated_at: record.updated_at,
            title: record.title,
            name: record.name,
        })
        .collect()
}

/// Orders records by `updated_at`, then by session id.
///
/// `updated_at` is parsed rather than compared as text. Every stamp this project has ever written
/// is epoch milliseconds -- `conversation::epoch_millis`, the one and only writer since the first
/// record was ever persisted (`b567c98`, 2026-09-11), including under the flat pre-2026-09-15
/// layout -- but nothing pins that WIDTH, and a text compare puts a shorter number above a longer
/// one. Parsing also means a stamp that is not a number at all (a hand-edited or corrupted record,
/// which `read_conversation_records` deliberately tolerates rather than discards) yields `None`,
/// which orders below every parsed value instead of winning on `'2' > '1'`.
///
/// This comment previously said the module's own older records carry ISO-8601 strings. They do not:
/// only the earliest test fixtures in this file ever did, added by `a21b55c` when this module had
/// no caller and wrote nothing.
///
/// The second component is `provider_session_id`, NOT `updated_at` again. Comparing `updated_at`
/// against itself is not a tiebreak at all -- two records with byte-identical stamps would compare
/// `Equal` and `max_by` would return whichever `read_dir` happened to yield last. Session ids are
/// unique within a conversation directory (each one IS its filename), so this makes the order total
/// and the answer genuinely reproducible. Which of two same-millisecond sessions wins is arbitrary;
/// that it is the same one every time is the property being bought.
fn updated_at_rank(record: &ConversationRecord) -> (Option<u128>, &str) {
    (
        record.updated_at.parse::<u128>().ok(),
        record.provider_session_id.as_str(),
    )
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
    let Ok(dir) = conversation_dir(conversation_id) else {
        return Vec::new();
    };
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
            serde_json::from_str(&crate::private_fs::read_private_to_string(&path).ok()?).ok()
        })
        .collect();
    if let Some(legacy) = read_legacy_record(&dir) {
        records.push(legacy);
    }
    records
}

/// Every `provider_session_id` this conversation still has a record for, in no particular order.
///
/// Both layouts, because `read_conversation_records` reads both. The one caller is
/// `history::store`'s orphan sweep, which deletes a stored history whose session is no longer
/// offerable; an id missing here is exactly what "no longer offerable" means, since the picker is
/// built from the same function.
///
/// An EMPTY result is deliberately ambiguous and its caller must treat it as such: it means "no
/// records" and "the directory could not be read" alike, and deleting every history file on the
/// strength of a failed `read_dir` would be the worst possible reading of it.
pub(crate) fn recorded_session_ids(conversation_id: &str) -> Vec<String> {
    read_conversation_records(conversation_id)
        .into_iter()
        .map(|r| r.provider_session_id)
        .collect()
}

/// The one pre-2026-09-15 `<conversation_id>.json`, if this workspace still has one.
///
/// `dir` is the per-conversation directory, so the legacy file is its sibling of the same name plus
/// `.json`. A record whose `provider_session_id` already appears in the directory is NOT filtered
/// out here; `resumable_sessions` de-duplicates after ranking instead, and keeps the right one
/// because a directory record for a session is written LATER in wall-clock time than the flat file
/// it supersedes -- see that function's doc for why, and for the one way that can be wrong.
///
/// It is worth being explicit about what this file holds, because a comment here used to get it
/// wrong: its stamps are epoch milliseconds, exactly like a directory record's. The flat layout's
/// only writer went through the same `conversation::epoch_millis`. It does not rank below newer
/// records by failing to parse; it ranks by its real timestamp, and can legitimately sit above one.
fn read_legacy_record(dir: &Path) -> Option<ConversationRecord> {
    let legacy = dir.with_extension("json");
    serde_json::from_str(&crate::private_fs::read_private_to_string(&legacy).ok()?).ok()
}

/// `$XDG_STATE_HOME/eitri/conversations/` -- see `state_dirs` for the full rule, and for the one
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
pub(crate) fn validate_path_component(label: &str, value: &str) -> std::io::Result<()> {
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
    let dir = path
        .parent()
        .expect("a record path always has a parent directory")
        .to_path_buf();
    create_private_state_dir(&dir, crate::state_dirs::conversations_dir()?)?;
    let json = serde_json::to_string_pretty(record).map_err(std::io::Error::other)?;
    write_record(&path, json.as_bytes())?;
    // After the write, never before: a failed prune must not cost the caller its record, and the
    // record just written has to be part of the set being ranked.
    prune(&dir, &path);
    Ok(())
}

/// Creates `dir`, which lies under `kind_dir` (`conversations_dir()` or `history_dir()`), 0700 all
/// the way down from Eitri's own state root -- `kind_dir`'s parent, `<state home>/eitri` -- and
/// tightens those that an older build left open (`private_fs`'s module doc; ruling R5).
pub(crate) fn create_private_state_dir(dir: &Path, kind_dir: PathBuf) -> std::io::Result<()> {
    let root = kind_dir.parent().map(Path::to_path_buf).unwrap_or(kind_dir);
    crate::private_fs::create_private_dir_all(dir, &root)
}

/// Writes through a temp file in the SAME directory, then renames over the target.
///
/// The temp file is created 0600, so the record the rename puts in place is 0600 too: a record
/// carries the session's title, its first prompt line (ruling R5).
///
/// `std::fs::write` truncates in place, so a crash (or a reader arriving mid-write) between the
/// truncate and the last byte leaves a file that parses as nothing -- which `resumable_sessions`
/// silently skips, so the session it describes simply vanishes from the picker (and from the offer
/// entirely, when it is the workspace's only record). A rename within one directory is atomic, so a
/// reader sees either the whole old record or the whole new one and never a partial file.
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
pub(crate) fn write_record(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = path.parent().expect("a record path always has a parent directory");
    // `.tmp` extension, not `.json`: `read_conversation_records` filters on the extension, so a
    // reader that lists the directory mid-write cannot pick this up as a record.
    let temp = dir.join(format!(".{}.tmp", uuid::Uuid::new_v4()));
    let written = (|| {
        let mut file = crate::private_fs::open_private(&temp)?;
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
///
/// **Session tabs plan, ruling 21 / spec §3.8 point 4: a record whose session is open somewhere
/// (its lease is held -- by a tab of this window, or another window) is never a candidate.** It
/// still counts toward the cap, so what goes is the oldest record nobody holds -- an open tab must
/// never lose its own record out from under it, but the cap still binds overall. The lease probe
/// (`SessionLease::is_held`) is documented to cost a few microseconds of a competing `try_acquire`
/// seeing `AlreadyHeld`; this is one of the two call sites that accepts that cost.
fn prune(dir: &Path, keep: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut records: Vec<(Option<u128>, String, PathBuf)> = Vec::new();
    let mut held_count = 0usize;
    for entry in entries.flatten() {
        let path = entry.path();
        match path.extension().and_then(|e| e.to_str()) {
            Some("tmp") => {
                let orphaned = entry
                    .metadata()
                    .and_then(|m| m.modified())
                    .and_then(|m| {
                        std::time::SystemTime::now()
                            .duration_since(m)
                            .map_err(std::io::Error::other)
                    })
                    .map(|age| age > STALE_TEMP_AGE)
                    .unwrap_or(false);
                if orphaned {
                    let _ = std::fs::remove_file(&path);
                }
            }
            Some("json") if path != keep => {
                let Ok(text) = crate::private_fs::read_private_to_string(&path) else {
                    continue;
                };
                let Ok(record) = serde_json::from_str::<ConversationRecord>(&text) else {
                    // Unparseable files are left alone rather than deleted: this is the only copy
                    // of whatever is in them, and `read_conversation_records` already skips them.
                    continue;
                };
                let (stamp, _) = updated_at_rank(&record);
                // Spec §3.8 point 4: a record whose session is open somewhere (its lease is held --
                // a session tab of this window, or another window) is never a candidate. It still
                // counts toward the cap, so what goes is the oldest record nobody holds.
                let held = crate::lease::SessionLease::is_held(
                    &record.provider,
                    &record.canonical_cwd,
                    &record.provider_session_id,
                )
                .unwrap_or(false);
                held_count += usize::from(held);
                if !held {
                    records.push((stamp, record.provider_session_id, path));
                }
            }
            _ => {}
        }
    }
    let total = records.len() + held_count;
    if total < MAX_RECORDS_PER_CONVERSATION {
        return;
    }
    // Same order `resumable_sessions` ranks by, so what is dropped is exactly what that would have
    // listed last. `keep` is excluded above and occupies one of the cap's slots.
    records.sort_by(|a, b| (a.0, a.1.as_str()).cmp(&(b.0, b.1.as_str())));
    let over = total + 1 - MAX_RECORDS_PER_CONVERSATION;
    for (_, _, path) in records.into_iter().take(over) {
        let _ = std::fs::remove_file(path);
    }
}

pub fn load_conversation_record(
    conversation_id: &str,
    provider_session_id: &str,
) -> std::io::Result<ConversationRecord> {
    let path = record_path(conversation_id, provider_session_id)?;
    let contents = crate::private_fs::read_private_to_string(&path)?;
    serde_json::from_str(&contents).map_err(std::io::Error::other)
}

/// The record this session already has on disk, in EITHER layout, or `None` if it has none -- which
/// is where its `created_at` survives a resume from.
///
/// `conversation::persist_record` calls this so a resume keeps the session's real start time
/// instead of restamping it. The directory record is preferred; the pre-2026-09-15 flat file is the
/// fallback, and only when it is genuinely the same session -- that file is keyed by conversation
/// alone, so a DIFFERENT session's record can be sitting there, and inheriting its stamp would
/// state one session's start time on another.
///
/// The flat-file arm is what makes `ResumableSession::created_at`'s doc true. Without it the first
/// resume of a flat-layout-only session wrote `created_at = now`, and `resumable_sessions`'
/// de-duplication then dropped the flat record that still held the real value -- one-way, since
/// nothing rewrites the flat file either.
///
/// **Renamed 2026-09-19** from `created_at_of_existing_record`, when it began returning the whole
/// record: `persist_record` now keeps the session's title across a resume the same way, and needs
/// both from one read. What this doc says of `created_at` holds for `title` too.
pub(crate) fn existing_record(conversation_id: &str, provider_session_id: &str) -> Option<ConversationRecord> {
    if let Ok(record) = load_conversation_record(conversation_id, provider_session_id) {
        return Some(record);
    }
    let dir = conversation_dir(conversation_id).ok()?;
    let legacy = read_legacy_record(&dir)?;
    (legacy.provider_session_id == provider_session_id).then_some(legacy)
}

/// Gives this session's directory record `title`, if it has none yet. `Ok(false)` when there is no
/// directory record or the record already has a title -- the first one wins, for good. Called only
/// by the ingestion thread, and only once it knows the record is on disk (`ingestion::flush_title`):
/// a caller that cannot know that loses the title to the `Ok(false)`, which is how the first version
/// of this feature dropped one (review of `7fb787b`).
/// `updated_at` is left alone: it means "last started or resumed", and titling is neither.
pub(crate) fn set_title_if_missing(
    conversation_id: &str,
    provider_session_id: &str,
    title: &str,
) -> std::io::Result<bool> {
    let Ok(mut record) = load_conversation_record(conversation_id, provider_session_id) else {
        return Ok(false);
    };
    if record.title.is_some() {
        return Ok(false);
    }
    record.title = Some(title.to_string());
    save_conversation_record(&record)?;
    Ok(true)
}

/// What a record write does with the session's rename.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NameUpdate {
    /// Keep whatever the record on disk already says (a resume, an adoption with no rename yet).
    Keep,
    /// The tab was renamed before the record existed: write this (`None` clears it).
    Set(Option<String>),
}

/// Gives this session's directory record `name` (`None` clears it). The last one wins.
/// `Ok(false)` when there is no directory record. `updated_at` is left alone. Called only from the
/// ingestion thread, once the record is on disk (`ingestion::flush_name`), for the reason
/// `set_title_if_missing` gives.
pub(crate) fn set_name(conversation_id: &str, provider_session_id: &str, name: Option<&str>) -> std::io::Result<bool> {
    let Ok(mut record) = load_conversation_record(conversation_id, provider_session_id) else {
        return Ok(false);
    };
    record.name = name.map(str::to_string);
    save_conversation_record(&record)?;
    Ok(true)
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
        assert!(
            !record.provider_advertised_resume,
            "unknown resumability must read as not-resumable"
        );
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
            title: None,
            name: None,
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
            title: None,
            name: None,
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

    /// The most recent offerable session: the head of `resumable_sessions`.
    ///
    /// A TEST helper, not a public function. `agent` used to export this as `resumable_session`,
    /// and nothing outside these tests ever called it once the start screen became a picker -- a
    /// public API with no product caller, kept green by a test that asserted the head of a list is
    /// the head of that list. The tests below still ask "which one would you offer", which is a
    /// real question about the ranking; they just no longer need product code to ask it.
    fn most_recent_offer(conversation_id: &str) -> Option<ResumableSession> {
        resumable_sessions(conversation_id).into_iter().next()
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
            title: None,
            name: None,
        }
    }

    #[test]
    fn a_saved_record_loads_back_exactly() {
        let conv = unique_conversation_id("roundtrip");
        let saved = record(&conv, "prov-1", "1000");
        save_conversation_record(&saved).unwrap();
        assert_eq!(load_conversation_record(&conv, "prov-1").unwrap(), saved);
    }

    /// Local-IPC review finding 8 (ruling R5): a record -- its title is the session's first prompt
    /// line -- is a 0600 file, in 0700 directories from the state root down, not the umask's
    /// `0644` in `0755`.
    #[test]
    fn a_saved_record_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path| std::fs::symlink_metadata(p).unwrap().permissions().mode() & 0o777;
        let conv = unique_conversation_id("private");
        let mut saved = record(&conv, "prov-1", "1000");
        saved.title = Some("the first prompt".into());
        save_conversation_record(&saved).unwrap();
        let path = record_path(&conv, "prov-1").unwrap();
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().unwrap()), 0o700, "the conversation's directory");
        let conversations = crate::state_dirs::conversations_dir().unwrap();
        assert_eq!(mode(&conversations), 0o700);
        assert_eq!(mode(conversations.parent().unwrap()), 0o700, "the state root");
    }

    #[test]
    fn loading_an_unknown_conversation_is_an_error_and_offers_nothing() {
        let conv = unique_conversation_id("unknown");
        assert!(load_conversation_record(&conv, "prov-1").is_err());
        assert!(most_recent_offer(&conv).is_none());
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
        let resumable = most_recent_offer(&conv).expect("a record that advertised resume is offerable");
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
        assert!(most_recent_offer(&conv).is_none());
    }

    /// A blank provider session id can no longer even be written -- it is a path component now, so
    /// the validator rejects it before anything touches the disk. The `trim().is_empty()` guard in
    /// `resumable_sessions` stays regardless, because it also covers a record hand-written or left
    /// over from the older layout.
    #[test]
    fn a_blank_provider_session_id_cannot_be_written_and_is_never_offered() {
        let conv = unique_conversation_id("blank");
        assert!(save_conversation_record(&record(&conv, "   ", "1000")).is_err());
        assert!(most_recent_offer(&conv).is_none());
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
        let offered = most_recent_offer(&conv).expect("a workspace with two sessions still offers one");
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
        let first = most_recent_offer(&conv).unwrap().provider_session_id;
        for _ in 0..5 {
            assert_eq!(most_recent_offer(&conv).unwrap().provider_session_id, first);
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
        std::fs::write(
            conversations_dir().unwrap().join(&conv).join("prov-corrupt.json"),
            "{ this is not json",
        )
        .unwrap();
        let offered = most_recent_offer(&conv).expect("one unreadable record must not hide the rest");
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
        let legacy = record(&conv, "prov-legacy", "2000");
        std::fs::write(
            dir.join(format!("{conv}.json")),
            serde_json::to_string(&legacy).unwrap(),
        )
        .unwrap();

        let offered = most_recent_offer(&conv).expect("an old-layout record must still be offered");
        assert_eq!(offered.provider_session_id, "prov-legacy");
    }

    /// ...and a record written since outranks it only when its stamp really is greater. Both
    /// layouts hold epoch millis, so there is no free win for either one.
    ///
    /// This test used to give the legacy record an ISO-8601 stamp and assert the directory record
    /// always wins "because an ISO-8601 stamp cannot be placed in time". Production never wrote
    /// such a stamp; against real data the comparison is an ordinary numeric one, which is what the
    /// two halves below pin.
    #[test]
    fn a_new_layout_record_outranks_the_old_single_file_one_only_by_its_stamp() {
        let conv = unique_conversation_id("legacy-vs-new");
        let dir = conversations_dir().unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        let legacy = record(&conv, "prov-legacy", "2000");
        std::fs::write(
            dir.join(format!("{conv}.json")),
            serde_json::to_string(&legacy).unwrap(),
        )
        .unwrap();
        save_conversation_record(&record(&conv, "prov-new", "3000")).unwrap();
        assert_eq!(most_recent_offer(&conv).unwrap().provider_session_id, "prov-new");

        let older = unique_conversation_id("new-vs-legacy");
        std::fs::create_dir_all(&dir).unwrap();
        let legacy = record(&older, "prov-legacy", "3000");
        std::fs::write(
            dir.join(format!("{older}.json")),
            serde_json::to_string(&legacy).unwrap(),
        )
        .unwrap();
        save_conversation_record(&record(&older, "prov-new", "2000")).unwrap();
        assert_eq!(
            most_recent_offer(&older).unwrap().provider_session_id,
            "prov-legacy",
            "a legacy record with the greater stamp really does lead"
        );
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
        assert!(
            kept.contains(&"prov-020.json".to_string()),
            "the newest must survive: {kept:?}"
        );
        assert!(
            !kept.contains(&"prov-000.json".to_string()),
            "the oldest must be dropped: {kept:?}"
        );
        // And the offer is still the newest of what is left.
        assert_eq!(most_recent_offer(&conv).unwrap().provider_session_id, "prov-020");
    }

    fn named(conversation_id: &str, cwd: &str, id: &str, stamp: u128) -> ConversationRecord {
        ConversationRecord {
            conversation_id: conversation_id.to_string(),
            provider: "claude".to_string(),
            provider_session_id: id.to_string(),
            canonical_cwd: cwd.to_string(),
            created_at: stamp.to_string(),
            updated_at: stamp.to_string(),
            provider_advertised_resume: true,
            title: None,
            name: None,
        }
    }

    /// Spec §3.8 point 4: a long-lived tab ranks oldest while newer tabs write records, and the
    /// prune must not take its record out from under it.
    #[test]
    fn prune_spares_a_record_whose_lease_is_held() {
        let conversation_id = unique_conversation_id("prune-lease");
        let cwd = "/tmp/prune-lease-project";
        save_conversation_record(&named(&conversation_id, cwd, "held-oldest", 1)).unwrap();
        let lease = crate::lease::SessionLease::try_acquire("claude", cwd, "held-oldest").unwrap();
        for n in 0..MAX_RECORDS_PER_CONVERSATION {
            save_conversation_record(&named(&conversation_id, cwd, &format!("s-{n:02}"), 100 + n as u128)).unwrap();
        }
        assert!(
            load_conversation_record(&conversation_id, "held-oldest").is_ok(),
            "an open tab's record was pruned"
        );
        assert!(
            load_conversation_record(&conversation_id, "s-00").is_err(),
            "the oldest record nobody holds goes instead, so the cap still binds"
        );
        drop(lease);
        // A `flock` belongs to the open file description, and a child another test thread is
        // spawning holds a copy of every fd until its `exec` closes them, so on a loaded machine
        // the lock can outlive `drop` by a few milliseconds. Prune is right to spare it then; this
        // test is about what happens once it is really free, so wait for that (bounded).
        let released_by = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while crate::lease::SessionLease::is_held("claude", cwd, "held-oldest").unwrap()
            && std::time::Instant::now() < released_by
        {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(
            !crate::lease::SessionLease::is_held("claude", cwd, "held-oldest").unwrap(),
            "the dropped lease should be free within 5s"
        );
        save_conversation_record(&named(&conversation_id, cwd, "s-last", 1000)).unwrap();
        assert!(
            load_conversation_record(&conversation_id, "held-oldest").is_err(),
            "released, it ages out as before"
        );
    }

    #[test]
    fn a_name_round_trips_and_an_old_record_without_one_still_loads() {
        let json = r#"{
            "conversation_id": "old", "provider": "claude", "provider_session_id": "prov-old",
            "canonical_cwd": "/tmp/project", "created_at": "1", "updated_at": "2"
        }"#;
        let old: ConversationRecord = serde_json::from_str(json).expect("an older record must still load");
        assert_eq!(old.name, None);
        assert!(
            !serde_json::to_string(&old).unwrap().contains("\"name\""),
            "absent, not null"
        );

        let conversation_id = unique_conversation_id("name");
        save_conversation_record(&named(&conversation_id, "/tmp/name-project", "prov-n", 5)).unwrap();
        assert!(set_name(&conversation_id, "prov-n", Some("docs")).unwrap());
        let record = load_conversation_record(&conversation_id, "prov-n").unwrap();
        assert_eq!(record.name.as_deref(), Some("docs"));
        assert_eq!(record.updated_at, "5", "naming is not starting or resuming");
        assert_eq!(
            resumable_sessions(&conversation_id)[0].name.as_deref(),
            Some("docs"),
            "the chooser shows it"
        );
        assert!(
            set_name(&conversation_id, "prov-n", Some("later")).unwrap(),
            "the last rename wins"
        );
        assert!(
            set_name(&conversation_id, "prov-n", None).unwrap(),
            "an empty rename clears it"
        );
        assert_eq!(load_conversation_record(&conversation_id, "prov-n").unwrap().name, None);
        assert!(
            !set_name(&conversation_id, "prov-missing", Some("x")).unwrap(),
            "no record, nothing written"
        );
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
        assert!(
            fresh.exists(),
            "a temp file a concurrent writer may still hold must be left alone"
        );
    }

    // ---- The list, not just the head ----------------------------------------------------------
    //
    // A workspace can hold up to `MAX_RECORDS_PER_CONVERSATION` sessions, plus the one legacy file.
    // The tests above ask "which ONE would you offer" via `most_recent_offer`; these pin the answer to
    // "which ONES are there", which is what a picker renders.

    /// The ordering key is `updated_at`, not write order -- the same property the singular offer
    /// has, asserted across the whole list rather than only at its head.
    #[test]
    fn every_offerable_session_is_listed_newest_first() {
        let conv = unique_conversation_id("list-order");
        save_conversation_record(&record(&conv, "prov-mid", "2000")).unwrap();
        save_conversation_record(&record(&conv, "prov-new", "3000")).unwrap();
        save_conversation_record(&record(&conv, "prov-old", "1000")).unwrap();

        let ids: Vec<String> = resumable_sessions(&conv)
            .into_iter()
            .map(|s| s.provider_session_id)
            .collect();
        assert_eq!(ids, vec!["prov-new", "prov-mid", "prov-old"]);
    }

    /// The same filter the singular offer applies, applied to every element: a record whose
    /// provider never advertised resume is not offerable, so a picker must not list it as one.
    #[test]
    fn the_list_omits_records_whose_provider_never_advertised_resume() {
        let conv = unique_conversation_id("list-filter");
        save_conversation_record(&record(&conv, "prov-yes", "2000")).unwrap();
        let mut no = record(&conv, "prov-no", "3000");
        no.provider_advertised_resume = false;
        save_conversation_record(&no).unwrap();

        let ids: Vec<String> = resumable_sessions(&conv)
            .into_iter()
            .map(|s| s.provider_session_id)
            .collect();
        assert_eq!(
            ids,
            vec!["prov-yes"],
            "the newest record is not offerable and must not be listed"
        );
    }

    /// The pre-2026-09-15 single file is a row like any other, ranked by its own stamp.
    ///
    /// **Not "it sorts last".** A test here used to assert that, using an ISO-8601 fixture stamp and
    /// reasoning that an unparseable stamp orders below everything. Production never wrote such a
    /// stamp: the flat layout's only writer (`b567c98`) used `conversation::epoch_millis`, the same
    /// as today's. So the fixture below is epoch millis, and the legacy record leads the list --
    /// which is the real behaviour and the opposite of what was pinned before.
    #[test]
    fn the_old_single_file_record_is_ranked_by_its_stamp_like_any_other() {
        let conv = unique_conversation_id("list-legacy");
        let dir = conversations_dir().unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        let legacy = record(&conv, "prov-legacy", "9000");
        std::fs::write(
            dir.join(format!("{conv}.json")),
            serde_json::to_string(&legacy).unwrap(),
        )
        .unwrap();
        save_conversation_record(&record(&conv, "prov-new", "1000")).unwrap();

        let ids: Vec<String> = resumable_sessions(&conv)
            .into_iter()
            .map(|s| s.provider_session_id)
            .collect();
        assert_eq!(
            ids,
            vec!["prov-legacy", "prov-new"],
            "the greater stamp leads, whichever layout wrote it"
        );
    }

    /// One Claude session is one row, even when it is recorded in BOTH layouts.
    ///
    /// Invisible while only the top-ranked record was ever shown; a real duplicate the moment a
    /// list is rendered. It happens for real: a workspace with an old `<conversation_id>.json`
    /// whose session is then resumed gets a `<conversation_id>/<that session>.json` beside it.
    /// The surviving row is the higher-ranked one, so the newer stamp is what the user sees.
    ///
    /// Both stamps here are epoch millis, because that is what both layouts really hold. The
    /// directory record is the greater one for the reason `resumable_sessions`' doc gives: it is
    /// written later in wall-clock time than the flat file it supersedes, not because the flat
    /// file's stamp fails to parse.
    #[test]
    fn one_session_recorded_in_both_layouts_is_listed_once() {
        let conv = unique_conversation_id("list-dedupe");
        let dir = conversations_dir().unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        let legacy = record(&conv, "prov-same", "4000");
        std::fs::write(
            dir.join(format!("{conv}.json")),
            serde_json::to_string(&legacy).unwrap(),
        )
        .unwrap();
        save_conversation_record(&record(&conv, "prov-same", "5000")).unwrap();

        let listed = resumable_sessions(&conv);
        assert_eq!(listed.len(), 1, "one Claude session must be one row: {listed:?}");
        assert_eq!(listed[0].updated_at, "5000", "the higher-ranked record is the one kept");
    }

    /// Byte-identical stamps must not shuffle the list between runs. `updated_at_rank`'s second
    /// component (the session id) is what makes the order total; without it two same-millisecond
    /// sessions would land in whatever order `read_dir` yielded them.
    #[test]
    fn the_listed_order_is_total_so_identical_stamps_do_not_shuffle() {
        let conv = unique_conversation_id("list-tie");
        save_conversation_record(&record(&conv, "prov-aaa", "2000")).unwrap();
        save_conversation_record(&record(&conv, "prov-zzz", "2000")).unwrap();
        let first: Vec<String> = resumable_sessions(&conv)
            .into_iter()
            .map(|s| s.provider_session_id)
            .collect();
        for _ in 0..5 {
            let again: Vec<String> = resumable_sessions(&conv)
                .into_iter()
                .map(|s| s.provider_session_id)
                .collect();
            assert_eq!(again, first);
        }
        // Which comes first is arbitrary; that it is decided by the id rather than by directory
        // order is the property. Descending, so the greater id leads -- the same element
        // `most_recent_offer` picks.
        assert_eq!(first, vec!["prov-zzz", "prov-aaa"]);
    }

    /// A resume must keep the session's real start time even when the only record it has is the
    /// pre-2026-09-15 flat file.
    ///
    /// The directory-record case was always covered; this one was not, and was silently broken:
    /// `conversation::persist_record` looked the old stamp up by `<conv>/<session>.json` only, so
    /// the first resume of a flat-layout-only session wrote `created_at = now`, and the
    /// de-duplication in `resumable_sessions` then dropped the flat record that still held the real
    /// value. One-way, because nothing rewrites the flat file either.
    #[test]
    fn a_created_at_recorded_only_in_the_old_layout_survives_the_move_to_the_new_one() {
        let conv = unique_conversation_id("created-legacy");
        let dir = conversations_dir().unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        let mut legacy = record(&conv, "prov-same", "2000");
        legacy.created_at = "1234".into();
        std::fs::write(
            dir.join(format!("{conv}.json")),
            serde_json::to_string(&legacy).unwrap(),
        )
        .unwrap();

        assert_eq!(
            existing_record(&conv, "prov-same").map(|r| r.created_at).as_deref(),
            Some("1234")
        );
    }

    /// ...but only for the SAME session. The flat file is keyed by conversation alone, so a
    /// workspace's one legacy record can belong to a different session entirely; inheriting its
    /// stamp would state one session's start time on another.
    #[test]
    fn a_different_sessions_legacy_created_at_is_not_inherited() {
        let conv = unique_conversation_id("created-legacy-other");
        let dir = conversations_dir().unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        let mut legacy = record(&conv, "prov-other", "2000");
        legacy.created_at = "1234".into();
        std::fs::write(
            dir.join(format!("{conv}.json")),
            serde_json::to_string(&legacy).unwrap(),
        )
        .unwrap();

        assert_eq!(existing_record(&conv, "prov-same").map(|r| r.created_at), None);
    }

    /// The directory record wins when both exist, and a session with neither has no stamp to keep.
    #[test]
    fn the_directory_record_is_preferred_and_an_unknown_session_has_no_created_at() {
        let conv = unique_conversation_id("created-both");
        let dir = conversations_dir().unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        let mut legacy = record(&conv, "prov-same", "2000");
        legacy.created_at = "1234".into();
        std::fs::write(
            dir.join(format!("{conv}.json")),
            serde_json::to_string(&legacy).unwrap(),
        )
        .unwrap();
        // `record()` writes created_at "1000".
        save_conversation_record(&record(&conv, "prov-same", "5000")).unwrap();

        assert_eq!(
            existing_record(&conv, "prov-same").map(|r| r.created_at).as_deref(),
            Some("1000")
        );
        assert_eq!(existing_record(&conv, "prov-never-seen").map(|r| r.created_at), None);
    }

    /// A listed session carries both stamps, because they mean different things: `created_at` is
    /// when the conversation began and `updated_at` is when it was last opened. A picker that
    /// showed only one would have to pick which question it was answering and would answer the
    /// other one wrongly.
    #[test]
    fn a_listed_session_carries_the_created_at_its_record_was_first_written_with() {
        let conv = unique_conversation_id("list-created");
        // `record()` writes created_at "1000"; persist_record's real preservation across resumes is
        // covered in conversation.rs. This pins only that the field reaches the caller at all.
        save_conversation_record(&record(&conv, "prov-1", "9000")).unwrap();
        let listed = resumable_sessions(&conv);
        assert_eq!(listed[0].created_at, "1000");
        assert_eq!(listed[0].updated_at, "9000");
        assert_eq!(listed[0].provider, "claude");
    }

    /// The retention cap bounds the picker too: whatever a workspace accumulates, a start screen
    /// renders at most `MAX_RECORDS_PER_CONVERSATION` rows (plus at most one legacy record).
    #[test]
    fn the_list_is_bounded_by_the_retention_cap() {
        let conv = unique_conversation_id("list-cap");
        for i in 0..(MAX_RECORDS_PER_CONVERSATION as u32 + 5) {
            save_conversation_record(&record(&conv, &format!("prov-{i:03}"), &(1000 + i).to_string())).unwrap();
        }
        let listed = resumable_sessions(&conv);
        assert_eq!(listed.len(), MAX_RECORDS_PER_CONVERSATION);
        assert_eq!(listed[0].provider_session_id, "prov-020", "newest first");
    }

    /// The title is the prompt's first line with visible text, as typed, whitespace collapsed.
    #[test]
    fn a_title_is_the_first_line_with_text_in_it() {
        assert_eq!(
            title_from_prompt("fix the resume picker").as_deref(),
            Some("fix the resume picker")
        );
        assert_eq!(
            title_from_prompt("\n  \n  两个  空格\t和制表符\nsecond line").as_deref(),
            Some("两个 空格 和制表符")
        );
        assert_eq!(title_from_prompt(""), None);
        assert_eq!(title_from_prompt(" \n\t\n"), None);
        assert_eq!(
            title_from_prompt("\u{200B}\u{FEFF}\nthe real line").as_deref(),
            Some("the real line")
        );
        assert_eq!(title_from_prompt("line one\r\nline two").as_deref(), Some("line one"));
    }

    /// Cut by characters, not bytes: an 80-byte cut would split a Chinese character (3 bytes in
    /// UTF-8) and panic, or land 27 characters in.
    #[test]
    fn a_long_title_is_cut_by_characters_and_says_so() {
        let exact: String = "字".repeat(TITLE_MAX_CHARS);
        assert_eq!(
            title_from_prompt(&exact).as_deref(),
            Some(exact.as_str()),
            "at the limit nothing is cut"
        );
        let long: String = "字".repeat(TITLE_MAX_CHARS + 20);
        let title = title_from_prompt(&long).unwrap();
        assert_eq!(title.chars().count(), TITLE_MAX_CHARS);
        assert!(title.ends_with('…'));
    }

    /// A record from before titles existed loads, and one without a title is written exactly as
    /// before -- no `"title": null` appears in a file an older build might read.
    #[test]
    fn a_record_without_a_title_reads_and_writes_as_it_always_did() {
        let json = r#"{"conversation_id":"c","provider":"claude","provider_session_id":"p","canonical_cwd":"/","created_at":"1","updated_at":"2","provider_advertised_resume":true}"#;
        let record: ConversationRecord = serde_json::from_str(json).unwrap();
        assert_eq!(record.title, None);
        assert!(!serde_json::to_string(&record).unwrap().contains("title"));
    }

    /// The first title wins, for good, and titling does not count as opening the session.
    #[test]
    fn set_title_if_missing_sets_it_once_and_leaves_updated_at_alone() {
        let conv = unique_conversation_id("title-once");
        assert!(
            !set_title_if_missing(&conv, "prov-1", "nothing to title").unwrap(),
            "no record, nothing written"
        );
        save_conversation_record(&record(&conv, "prov-1", "5000")).unwrap();

        assert!(set_title_if_missing(&conv, "prov-1", "first").unwrap());
        assert!(!set_title_if_missing(&conv, "prov-1", "second").unwrap());
        let loaded = load_conversation_record(&conv, "prov-1").unwrap();
        assert_eq!(loaded.title.as_deref(), Some("first"));
        assert_eq!(loaded.updated_at, "5000");
    }

    /// The title reaches the picker's row.
    #[test]
    fn a_listed_session_carries_its_title() {
        let conv = unique_conversation_id("list-title");
        let mut titled = record(&conv, "prov-titled", "9000");
        titled.title = Some("what it was about".into());
        save_conversation_record(&titled).unwrap();
        save_conversation_record(&record(&conv, "prov-untitled", "8000")).unwrap();
        let listed = resumable_sessions(&conv);
        assert_eq!(listed[0].title.as_deref(), Some("what it was about"));
        assert_eq!(listed[1].title, None);
    }
}
