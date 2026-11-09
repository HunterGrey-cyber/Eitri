//! The first-start marker: whether the screen has been shown, declined or postponed.
//!
//! One small JSON file in the state directory. Reading never fails: a file that is missing is an empty
//! marker, and one that cannot be read or parsed counts as absent with a note for the log, so a damaged
//! marker shows the screen again rather than hiding it for good. Writing returns the error for the caller to
//! log; the screen closes either way.

use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::state_paths::state_subdir;

/// The marker's file name in the state directory.
const FILE_NAME: &str = "first-start.json";
/// The shape of the file, written as `v`.
const VERSION: u32 = 1;

/// Where the marker is, from the two variables that decide the state home; `None` when neither gives an
/// absolute directory.
pub fn state_file(xdg_state_home: Option<&std::ffi::OsStr>, home: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    state_subdir(xdg_state_home, home, FILE_NAME)
}

/// What the file says.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Marker {
    /// How many times the screen was closed with "not now".
    pub not_now: u32,
    /// The day the user asked not to be asked again.
    pub declined: Option<String>,
    /// The day of the last change.
    pub last: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(default)]
struct Wire {
    v: u32,
    not_now: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    declined: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    last: Option<String>,
}

impl Default for Wire {
    fn default() -> Wire {
        Wire {
            v: VERSION,
            not_now: 0,
            declined: None,
            last: None,
        }
    }
}

/// The marker at `path`, and a log line when the file was there but could not be used (in which case the
/// marker is empty, as if the file were absent). A missing file is an empty marker and no note.
pub fn read(path: &Path) -> (Marker, Option<String>) {
    let text = match agent::private_fs::read_private_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return (Marker::default(), None),
        Err(e) => return (Marker::default(), Some(unusable(path, &e))),
    };
    match serde_json::from_str::<Wire>(&text) {
        Ok(wire) => (
            Marker {
                not_now: wire.not_now,
                declined: wire.declined,
                last: wire.last,
            },
            None,
        ),
        Err(e) => (Marker::default(), Some(unusable(path, &e))),
    }
}

fn unusable(path: &Path, why: &dyn std::fmt::Display) -> String {
    format!("[onboarding] {path:?} cannot be used ({why}); counted as absent")
}

/// Counts one "not now" on `today`. Blocks on the file system, so it runs on a worker.
pub fn not_now(path: &Path, today: &str) -> io::Result<()> {
    let (marker, _) = read(path);
    write(
        path,
        &Marker {
            not_now: marker.not_now.saturating_add(1),
            last: Some(today.to_owned()),
            ..marker
        },
    )
}

/// Records "don't ask again" on `today`, keeping the "not now" count. Blocks on the file system.
pub fn decline(path: &Path, today: &str) -> io::Result<()> {
    let (marker, _) = read(path);
    write(
        path,
        &Marker {
            declined: Some(today.to_owned()),
            last: Some(today.to_owned()),
            ..marker
        },
    )
}

/// Writes `marker` 0600 in a 0700 directory. The directory is made with itself as the root of what is
/// judged and tightened, so the state home above it is never changed.
fn write(path: &Path, marker: &Marker) -> io::Result<()> {
    let wire = Wire {
        v: VERSION,
        not_now: marker.not_now,
        declined: marker.declined.clone(),
        last: marker.last.clone(),
    };
    let mut text = serde_json::to_string(&wire).map_err(io::Error::other)?;
    text.push('\n');
    if let Some(dir) = path.parent() {
        agent::private_fs::create_private_dir_all(dir, dir)?;
    }
    agent::private_fs::write_private(path, text.as_bytes())
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("eitri-marker-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        dir
    }

    #[test]
    fn a_missing_file_is_an_empty_marker_without_a_note() {
        let dir = scratch("missing");
        assert_eq!(read(&dir.join("eitri").join(FILE_NAME)), (Marker::default(), None));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn not_now_round_trips_and_counts() {
        let dir = scratch("round");
        let path = dir.join("eitri").join(FILE_NAME);
        not_now(&path, "2026-11-02").unwrap();
        let (marker, note) = read(&path);
        assert_eq!(note, None);
        assert_eq!(
            marker,
            Marker {
                not_now: 1,
                declined: None,
                last: Some("2026-11-02".into())
            }
        );
        not_now(&path, "2026-11-03").unwrap();
        assert_eq!(read(&path).0.not_now, 2);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn the_file_is_the_documented_json() {
        let dir = scratch("json");
        let path = dir.join("eitri").join(FILE_NAME);
        not_now(&path, "2026-11-02").unwrap();
        let value: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(value, serde_json::json!({"v": 1, "not_now": 1, "last": "2026-11-02"}));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn decline_keeps_the_not_now_count() {
        let dir = scratch("decline");
        let path = dir.join("eitri").join(FILE_NAME);
        not_now(&path, "2026-11-02").unwrap();
        decline(&path, "2026-11-04").unwrap();
        assert_eq!(
            read(&path).0,
            Marker {
                not_now: 1,
                declined: Some("2026-11-04".into()),
                last: Some("2026-11-04".into())
            }
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn the_file_is_0600_and_its_directory_0700() {
        let dir = scratch("modes");
        let path = dir.join("eitri").join(FILE_NAME);
        // A restrictive umask must not matter, nor a wider one.
        not_now(&path, "2026-11-02").unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().unwrap()), 0o700);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn writing_does_not_tighten_the_state_home() {
        let dir = scratch("home");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        not_now(&dir.join("eitri").join(FILE_NAME), "2026-11-02").unwrap();
        assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o755);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_marker_that_cannot_be_parsed_counts_as_absent_with_a_note() {
        let dir = scratch("corrupt");
        let path = dir.join(FILE_NAME);
        crate::test_scratch_dir::write_private(&path, "{ not json").unwrap();
        let (marker, note) = read(&path);
        assert_eq!(marker, Marker::default());
        assert!(note.unwrap().contains("counted as absent"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn unknown_and_missing_keys_are_accepted() {
        let dir = scratch("keys");
        let path = dir.join(FILE_NAME);
        crate::test_scratch_dir::write_private(&path, r#"{"declined":"2026-11-01","extra":true}"#).unwrap();
        assert_eq!(
            read(&path),
            (
                Marker {
                    not_now: 0,
                    declined: Some("2026-11-01".into()),
                    last: None
                },
                None
            )
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_marker_that_cannot_be_written_is_a_log_line() {
        // A regular file where the state directory should be.
        let dir = scratch("blocked-dir");
        std::fs::write(dir.join("eitri"), "in the way").unwrap();
        let path = dir.join("eitri").join(FILE_NAME);
        assert!(not_now(&path, "2026-11-02").is_err());
        assert!(decline(&path, "2026-11-02").is_err());
        std::fs::remove_dir_all(&dir).unwrap();

        // A directory at the marker's own name: the directory is fine, the file cannot be written.
        let dir = scratch("blocked-file");
        let path = dir.join("eitri").join(FILE_NAME);
        std::fs::create_dir_all(&path).unwrap();
        assert!(not_now(&path, "2026-11-02").is_err());
        // Reading it is a note, not a panic, and counts as absent.
        let (marker, note) = read(&path);
        assert_eq!(marker, Marker::default());
        assert!(note.is_some());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn the_state_file_sits_under_eitri() {
        use std::ffi::OsStr;
        assert_eq!(
            state_file(Some(OsStr::new("/s")), None),
            Some(PathBuf::from("/s/eitri/first-start.json"))
        );
        assert_eq!(state_file(None, None), None);
    }
}
