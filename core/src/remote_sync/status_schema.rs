//! Shared disk status and panel payloads; readers refuse an unknown disk format version.

pub use crate::remote_project::PauseRecord;
use serde::{Deserialize, Serialize};

pub const STATUS_VERSION: u32 = 1;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusFile {
    pub version: u32,
    pub host: String,
    pub phase: String,
    pub git_ready: bool,
    pub git_pct: u8,
    pub cycle_running: bool,
    pub conflicts: Vec<ConflictEntry>,
    pub problems: Vec<ProblemEntry>,
    pub copied_back: Vec<String>,
    pub paused: Option<PauseRecord>,
    pub written_unix_ms: u64,
    /// Paths the last ignore change left out of sync while this laptop still holds them.
    #[serde(default)]
    pub kept_here: Vec<String>,
    /// A line the person should see while it holds (re-creation waiting for quiet, sync not started, ...).
    #[serde(default)]
    pub notice: Option<String>,
    /// How many more paths than `copied_back` lists were copied back.
    #[serde(default)]
    pub copied_back_more: u64,
    /// Conflicts Mutagen counted but did not list.
    #[serde(default)]
    pub conflicts_more: u64,
    /// Scan and transition problems Mutagen counted but did not list.
    #[serde(default)]
    pub problems_more: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConflictEntry {
    pub path: String,
    pub laptop_kind: String,
    pub server_kind: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProblemEntry {
    pub path: String,
    pub side: String,
    pub text: String,
}

/// The version is read on its own first, so a file written by another build's format is named as such rather
/// than reported as unreadable.
pub fn read_status(bytes: &[u8]) -> Result<StatusFile, String> {
    #[derive(Deserialize)]
    struct Version {
        version: u32,
    }
    let Version { version } =
        serde_json::from_slice(bytes).map_err(|error| format!("unreadable sync status: {error}"))?;
    if version != STATUS_VERSION {
        return Err(format!(
            "unsupported sync status version {version} (expected {STATUS_VERSION})"
        ));
    }
    serde_json::from_slice(bytes).map_err(|error| format!("unreadable sync status: {error}"))
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncChip {
    pub host: String,
    pub phase: String,
    pub conflicts: u32,
    #[serde(default)]
    pub git_pct: u8,
    pub paused_reason: Option<String>,
}

impl SyncChip {
    pub fn page_payload(&self) -> String {
        serde_json::json!({"kind":"sync_status", "chip":self}).to_string()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LateLine {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
    #[serde(default)]
    pub prompt: String,
    pub message_id: String,
    pub text: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_file_round_trips_and_rejects_another_version() {
        let mut status = StatusFile {
            version: STATUS_VERSION,
            host: "example-host".into(),
            phase: "paused".into(),
            git_ready: true,
            git_pct: 100,
            cycle_running: false,
            conflicts: vec![ConflictEntry {
                path: "src/main.rs".into(),
                laptop_kind: "file".into(),
                server_kind: "directory".into(),
            }],
            problems: vec![ProblemEntry {
                path: "link".into(),
                side: "server".into(),
                text: "absolute symlink".into(),
            }],
            copied_back: vec!["README.md".into()],
            paused: Some(PauseRecord {
                reason: "tripwire".into(),
                detail: "large batch".into(),
                since_unix_ms: 123,
                path: None,
            }),
            written_unix_ms: 456,
            kept_here: vec!["dist".into()],
            notice: Some("ignore rules changed; applying when idle".into()),
            copied_back_more: 0,
            conflicts_more: 2,
            problems_more: 3,
        };
        assert_eq!(read_status(&serde_json::to_vec(&status).unwrap()).unwrap(), status);
        status.version += 1;
        assert!(read_status(&serde_json::to_vec(&status).unwrap())
            .unwrap_err()
            .contains("version 2"));
        assert!(read_status(b"{}").is_err());
        let chip = SyncChip {
            git_pct: 0,
            host: "example-host".into(),
            phase: "paused".into(),
            conflicts: 1,
            paused_reason: Some("tripwire".into()),
        };
        let value = serde_json::to_value(&chip).unwrap();
        assert_eq!(value["pausedReason"], "tripwire");
        assert_eq!(serde_json::from_value::<SyncChip>(value).unwrap(), chip);
        let line = LateLine {
            started_at: None,
            seq: Some(1),
            prompt: "go".into(),
            message_id: "m1".into(),
            text: "waiting for sync".into(),
        };
        let value = serde_json::to_value(&line).unwrap();
        assert_eq!(value["messageId"], "m1");
        assert_eq!(serde_json::from_value::<LateLine>(value).unwrap(), line);
    }
}
