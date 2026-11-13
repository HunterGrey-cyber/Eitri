//! The record's state file, its restore marker, and the input file written for nvim.
//!
//! The record lives at `<state home>/eitri/editor/<project key>.json` for window 1 or a window alone and
//! `<project key>-u.json` for an unpaired window; window 2 embeds no nvim and has none. The marker beside it,
//! `<record name>.restoring`, holds the token of an nvim whose restore has not been seen to finish.

use std::ffi::{OsStr, OsString};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use super::{encode_name, parse, Record, RECORD_MAX};
use crate::state_paths::{create_state_dir, lock_store, project_key, state_subdir, temporary, LockTries};
use crate::window_link::pair::Role;

/// The input file's name in the pane-switch channel directory.
pub const INPUT_FILE_NAME: &str = "reopen-in.json";

/// A marker holds a 16-hex token; anything longer is not one of ours.
const MARKER_MAX: u64 = 64;

/// One window's record file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordFile {
    dir: PathBuf,
    path: PathBuf,
}

/// What a start found in the record file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartRead {
    Record(Record),
    /// No file, or a record that is not this window's to use (another version, another root), left in place.
    Nothing,
    /// The file could not be used and was moved whole to `<name>.unusable`; why, for the log and the notice.
    SetAside(String),
}

impl RecordFile {
    /// The record file of a window in `role` on `root`: `None` for window 2, which has no editor, and when neither
    /// variable gives a state directory.
    pub fn for_window(state_home: Option<&OsStr>, home: Option<&OsStr>, root: &Path, role: Role) -> Option<RecordFile> {
        let suffix = match role {
            Role::Main => "",
            Role::Unpaired => "-u",
            Role::Second => return None,
        };
        let dir = state_subdir(state_home, home, "editor")?;
        let path = dir.join(format!("{}{suffix}.json", project_key(root)));
        Some(RecordFile { dir, path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// `<record name>.restoring` beside the record.
    pub fn marker_path(&self) -> PathBuf {
        with_suffix(&self.path, ".restoring")
    }

    /// The record a start opens. One bounded read; a file that is too large, not a regular file of this user, not
    /// UTF-8 or not a record within its bounds is moved whole to `<name>.unusable`, so the next write does not
    /// replace what someone may want to look at. A record of another version or root is left in place and read as
    /// nothing.
    pub fn read_start(&self, root: &Path) -> StartRead {
        let text = match agent::private_fs::read_private_to_string_capped(&self.path, RECORD_MAX as u64) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return StartRead::Nothing,
            Err(e) => return self.set_aside(e.to_string()),
        };
        match parse(&text, root) {
            Ok(record) => StartRead::Record(record),
            Err(e) if e.reads_as_nothing() => StartRead::Nothing,
            Err(e) => self.set_aside(e.to_string()),
        }
    }

    fn set_aside(&self, why: String) -> StartRead {
        let aside = self.path.with_extension("json.unusable");
        match std::fs::rename(&self.path, &aside) {
            Ok(()) => StartRead::SetAside(format!(
                "{}: {why}; set aside as {}",
                self.path.display(),
                aside.display()
            )),
            Err(e) => StartRead::SetAside(format!(
                "{}: {why}; it could not be set aside: {e}",
                self.path.display()
            )),
        }
    }

    /// Writes `record` whole: to a temporary name of this process's beside the file, 0600 in a 0700 directory,
    /// then renamed over it, so a write that fails leaves the previous record. A record over [`RECORD_MAX`] is
    /// refused, since the next start would refuse to read it.
    pub fn write(&self, record: &Record) -> io::Result<()> {
        let json = record.to_json();
        if json.len() > RECORD_MAX {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("the record is {} bytes, over {RECORD_MAX}", json.len()),
            ));
        }
        create_state_dir(&self.dir)?;
        replace_private(&temporary(&self.path), &self.path, json.as_bytes())
    }

    /// The token in the marker, when there is one. A marker that is there but cannot be read (a link, a FIFO,
    /// another user's file) still says a restore did not finish, as an empty token.
    pub fn marker(&self) -> Option<String> {
        match agent::private_fs::read_private_nofollow_capped(&self.marker_path(), MARKER_MAX) {
            Ok(text) => Some(text.trim().to_string()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(_) => Some(String::new()),
        }
    }

    /// Writes the marker, 0600, holding `token`.
    pub fn set_marker(&self, token: &str) -> io::Result<()> {
        create_state_dir(&self.dir)?;
        let marker = self.marker_path();
        let tmp = with_suffix(&marker, &format!(".{}.tmp", std::process::id()));
        replace_private(&tmp, &marker, token.as_bytes())
    }

    /// Takes the record's store lock, so two windows on one record file never interleave a write or a marker change;
    /// makes the directory first, since the lock file lives beside the record.
    pub(crate) fn lock(&self, tries: LockTries) -> io::Result<agent::private_fs::PrivateLock> {
        create_state_dir(&self.dir)?;
        lock_store(&self.path, tries)
    }

    /// Removes the marker; none there is not an error.
    pub fn clear_marker(&self) -> io::Result<()> {
        match std::fs::remove_file(self.marker_path()) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }
}

/// `path` with `suffix` added to its last component.
fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = OsString::from(path.as_os_str());
    name.push(suffix);
    PathBuf::from(name)
}

/// Writes `bytes` to `tmp` as a 0600 file and renames it over `path`; on a failure the temporary file goes.
pub(crate) fn replace_private(tmp: &Path, path: &Path, bytes: &[u8]) -> io::Result<()> {
    let written = agent::private_fs::write_private(tmp, bytes).and_then(|()| std::fs::rename(tmp, path));
    if written.is_err() {
        let _ = std::fs::remove_file(tmp);
    }
    written
}

/// Whether `token` is 16 lowercase hex digits, the only form that goes into names nvim makes.
fn is_token(token: &str) -> bool {
    token.len() == 16 && token.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Writes nvim's input for one start into the pane-switch channel directory, 0600, as
/// `{"token", "root", "prefixes", "record", "skip"}`: the token naming this nvim's handoff, the root and the prefixes
/// to leave out as base64 of their bytes, the record to restore (re-serialised from the validated one, never the
/// bytes read) or `null`, and whether the restore is skipped. Written to a temporary name and renamed in, so nvim
/// never reads half of one.
pub fn write_input(
    channel_dir: &Path,
    token: &str,
    root: &Path,
    prefixes: &[PathBuf],
    record: Option<&Record>,
    skip: bool,
) -> io::Result<()> {
    if !is_token(token) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a reopen token is 16 lowercase hex digits",
        ));
    }
    let prefixes: Vec<String> = prefixes
        .iter()
        .map(|p| format!("\"{}\"", encode_name(p.as_os_str().as_bytes())))
        .collect();
    let record = record.map_or_else(|| "null".to_string(), Record::to_json);
    let json = format!(
        "{{\"token\":\"{token}\",\"root\":\"{}\",\"prefixes\":[{}],\"record\":{record},\"skip\":{skip}}}",
        encode_name(root.as_os_str().as_bytes()),
        prefixes.join(","),
    );
    let path = channel_dir.join(INPUT_FILE_NAME);
    let tmp = with_suffix(&path, &format!(".{}.tmp", std::process::id()));
    replace_private(&tmp, &path, json.as_bytes())
}
