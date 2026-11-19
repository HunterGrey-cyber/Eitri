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
    pub paused_reason: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LateLine {
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
            }),
            written_unix_ms: 456,
        };
        assert_eq!(read_status(&serde_json::to_vec(&status).unwrap()).unwrap(), status);
        status.version += 1;
        assert!(read_status(&serde_json::to_vec(&status).unwrap())
            .unwrap_err()
            .contains("version 2"));
        assert!(read_status(b"{}").is_err());
        let chip = SyncChip {
            host: "example-host".into(),
            phase: "paused".into(),
            conflicts: 1,
            paused_reason: Some("tripwire".into()),
        };
        let value = serde_json::to_value(&chip).unwrap();
        assert_eq!(value["pausedReason"], "tripwire");
        assert_eq!(serde_json::from_value::<SyncChip>(value).unwrap(), chip);
        let line = LateLine {
            message_id: "m1".into(),
            text: "waiting for sync".into(),
        };
        let value = serde_json::to_value(&line).unwrap();
        assert_eq!(value["messageId"], "m1");
        assert_eq!(serde_json::from_value::<LateLine>(value).unwrap(), line);
    }
}
