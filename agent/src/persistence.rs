//! Persistent `neovibe_conversation_id -> provider_session_id` identity (design doc §8.1),
//! surviving process restarts and reboots -- deliberately NOT under `$XDG_RUNTIME_DIR` (tmpfs,
//! cleared on logout; correct for the lease in `lease.rs`, wrong here) or `std::env::temp_dir()`.
//! One JSON file per conversation, no database -- matches this project's own established "small
//! file per key, no DB dependency" style (e.g. `supervisor`'s own socket-based state, `agent`'s
//! own per-conversation hook sockets).

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConversationRecord {
    pub conversation_id: String,
    pub provider: String,
    pub provider_session_id: String,
    pub canonical_cwd: String,
    pub created_at: String,
    pub updated_at: String,
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
        });
    }
}
