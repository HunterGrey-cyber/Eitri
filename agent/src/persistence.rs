//! Persistent `neovibe_conversation_id -> provider_session_id` identity (design doc §8.1),
//! surviving process restarts and reboots -- deliberately NOT under `$XDG_RUNTIME_DIR` (tmpfs,
//! cleared on logout; correct for the lease in `lease.rs`, wrong here) or `std::env::temp_dir()`.
//! One JSON file per conversation, no database -- matches this project's own established "small
//! file per key, no DB dependency" style (e.g. `supervisor`'s own socket-based state, `agent`'s
//! own per-conversation hook sockets).

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

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
/// Returns `None` rather than an error for every negative case (no record, unreadable record, a
/// record whose provider did not advertise resume): none of them are failures a user can act on,
/// and all of them mean exactly one thing to the caller -- do not offer it.
pub fn resumable_session(conversation_id: &str) -> Option<ResumableSession> {
    let record = load_conversation_record(conversation_id).ok()?;
    if !record.provider_advertised_resume || record.provider_session_id.trim().is_empty() {
        return None;
    }
    Some(ResumableSession {
        provider: record.provider,
        provider_session_id: record.provider_session_id,
        updated_at: record.updated_at,
    })
}

/// `$XDG_STATE_HOME/neovibe/conversations/`, falling back to `~/.local/state/neovibe/conversations/`
/// per the XDG Base Directory spec's own stated default for `XDG_STATE_HOME` when unset -- this
/// project has never used this directory before (see this plan's "Verified facts" point 4);
/// `$XDG_RUNTIME_DIR` (tmpfs, cleared on logout) is deliberately NOT used here, unlike the lease
/// in `lease.rs`.
pub(crate) fn conversations_dir() -> std::io::Result<PathBuf> {
    if let Ok(state_home) = std::env::var("XDG_STATE_HOME") {
        return Ok(PathBuf::from(state_home).join("neovibe/conversations"));
    }
    let home = std::env::var("HOME").map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::NotFound, "neither XDG_STATE_HOME nor HOME is set")
    })?;
    Ok(PathBuf::from(home).join(".local/state/neovibe/conversations"))
}

/// Rejects any `conversation_id` that is empty or contains a character outside `[A-Za-z0-9_-]` --
/// `record_path` joins this value with no other validation, so an id shaped like
/// `../../../../tmp/pwned` would otherwise escape `conversations_dir()` entirely. `lease.rs` solves
/// the same class of problem by hashing its key into an opaque filename; this module validates
/// instead of hashing so the on-disk files stay human-readable (`<conversation_id>.json`) for
/// debugging.
fn validate_conversation_id(conversation_id: &str) -> std::io::Result<()> {
    let valid = !conversation_id.is_empty()
        && conversation_id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if valid {
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "invalid conversation_id {conversation_id:?}: must be non-empty and contain only \
                 [A-Za-z0-9_-]"
            ),
        ))
    }
}

fn record_path(conversation_id: &str) -> std::io::Result<PathBuf> {
    Ok(conversations_dir()?.join(format!("{conversation_id}.json")))
}

pub fn save_conversation_record(record: &ConversationRecord) -> std::io::Result<()> {
    validate_conversation_id(&record.conversation_id)?;
    let dir = conversations_dir()?;
    std::fs::create_dir_all(&dir)?;
    let path = record_path(&record.conversation_id)?;
    let json = serde_json::to_string_pretty(record)
        .map_err(std::io::Error::other)?;
    std::fs::write(path, json)
}

pub fn load_conversation_record(conversation_id: &str) -> std::io::Result<ConversationRecord> {
    validate_conversation_id(conversation_id)?;
    let path = record_path(conversation_id)?;
    let contents = std::fs::read_to_string(path)?;
    serde_json::from_str(&contents).map_err(std::io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_isolated_state_home<F: FnOnce()>(f: F) {
        let dir = std::env::temp_dir().join(format!("agent-persistence-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        // SAFETY: this test mutates process-wide env state (XDG_STATE_HOME) -- confined to this
        // one helper, called sequentially within each test function, matching the established
        // pattern in `supervisor/src/lib.rs`'s own `socket_path_respects_xdg_runtime_dir...` test.
        unsafe { std::env::set_var("XDG_STATE_HOME", &dir) };
        f();
        unsafe { std::env::remove_var("XDG_STATE_HOME") };
        let _ = std::fs::remove_dir_all(&dir);
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
        assert!(load_conversation_record("../../../../tmp/pwned").is_err());
    }

    #[test]
    fn conversation_record_persistence_behaves_correctly() {
        // SAFETY: this test mutates process-wide env state (XDG_STATE_HOME). All three scenarios
        // are exercised sequentially within this one test function specifically so no other test
        // in this crate can interleave with these mutations on a separate thread -- cargo's
        // default multi-threaded test runner made that a real, reproducible race when these were
        // 3 separate test functions (confirmed: 4/9 parallel runs failed with a spurious NotFound
        // panic from one test's own remove_var firing mid-way through another still-running
        // test). Matches `supervisor/src/lib.rs`'s own established pattern for exactly this
        // hazard.
        with_isolated_state_home(|| {
            // Scenario 1: save then load round-trips exactly.
            let record = ConversationRecord {
                conversation_id: "conv-1".into(),
                provider: "claude".into(),
                provider_session_id: "prov-1".into(),
                canonical_cwd: "/tmp/project".into(),
                created_at: "2026-09-10T00:00:00Z".into(),
                updated_at: "2026-09-10T00:00:00Z".into(),
                provider_advertised_resume: true,
            };
            save_conversation_record(&record).unwrap();
            let loaded = load_conversation_record("conv-1").unwrap();
            assert_eq!(loaded, record);

            // Scenario 2: loading an unknown conversation id is a real error.
            let result = load_conversation_record("does-not-exist");
            assert!(result.is_err());

            // Scenario 3: saving twice overwrites the first record.
            let mut record2 = record.clone();
            record2.provider_session_id = "prov-2".into();
            record2.updated_at = "2026-09-10T01:00:00Z".into();
            save_conversation_record(&record2).unwrap();
            let loaded2 = load_conversation_record("conv-1").unwrap();
            assert_eq!(loaded2.provider_session_id, "prov-2");

            // Scenario 4: resumable_session is the question the start screen actually asks.
            let resumable = resumable_session("conv-1").expect("a record that advertised resume is offerable");
            assert_eq!(resumable.provider_session_id, "prov-2");
            assert_eq!(resumable.provider, "claude");
            assert!(resumable_session("does-not-exist").is_none());

            // Scenario 5: a record whose provider did NOT advertise resume is not offerable. This
            // is also how every record written before the field existed reads (serde default).
            let mut record3 = record2.clone();
            record3.conversation_id = "conv-no-resume".into();
            record3.provider_advertised_resume = false;
            save_conversation_record(&record3).unwrap();
            assert!(resumable_session("conv-no-resume").is_none());

            // Scenario 6: an empty provider session id is never offerable, whatever the flag says.
            let mut record4 = record2.clone();
            record4.conversation_id = "conv-empty".into();
            record4.provider_session_id = "   ".into();
            save_conversation_record(&record4).unwrap();
            assert!(resumable_session("conv-empty").is_none());
        });
    }
}
