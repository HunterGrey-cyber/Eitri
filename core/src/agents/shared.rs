//! The selection and the round every face of every window shares: `selection.json` and `round.json` beside the
//! row files in `<control dir>/agents/`.
//!
//! One person makes the gestures, one at a time, so each file is written whole by whichever process acted on the
//! gesture and the last writer wins. A [`Reader`](super::Reader) reads both through the same watch as the row
//! files and hands them out as `selection` and `round` changes and in every whole view.
//!
//! The round's order is never stored: it is computed from the view each time ([`round_order`]), so a row that
//! starts waiting during a round joins it in its place. The file keeps only where the round started, the stop it
//! is at and the stops already visited. Only the process that owns the current row moves the round on by itself,
//! once that row stops waiting ([`should_advance`]).

use std::collections::{HashMap, HashSet};
use std::ffi::{CString, OsStr};
use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use agent::private_fs;
use serde::de::DeserializeOwned;
use serde::Serialize;

use super::publish::{agents_dir, open_own_dir};
pub use super::row::{Round, RoundPoint, Selection};
use super::row::{Row, View};
use super::wire::{recut_round, recut_selection, WireError};
use super::{Class, MAX_FILE_BYTES, ROUND_MAX_IDLE_MS, SCHEMA_V};

/// The shared selection's file name in the agents directory.
pub const SELECTION_FILE: &str = "selection.json";
/// The shared round's file name in the agents directory.
pub const ROUND_FILE: &str = "round.json";

/// Which shared file a name in the agents directory is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SharedName {
    Selection,
    Round,
}

impl SharedName {
    pub(crate) fn of(name: &OsStr) -> Option<SharedName> {
        if name == SELECTION_FILE {
            Some(SharedName::Selection)
        } else if name == ROUND_FILE {
            Some(SharedName::Round)
        } else {
            None
        }
    }
}

/// What reading a shared file found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SharedRead<T> {
    Missing,
    /// There is something at the name that is not read, and why.
    Ignored(String),
    Found(T),
}

impl<T> SharedRead<T> {
    fn found(self) -> Option<T> {
        match self {
            SharedRead::Found(value) => Some(value),
            SharedRead::Missing | SharedRead::Ignored(_) => None,
        }
    }
}

/// Parses a shared file of a schema up to `schema`. The version is read before the rest, as a newer schema may
/// have changed every other field's type; unknown fields are ignored.
fn parse<T: DeserializeOwned>(bytes: &[u8], schema: u32) -> Result<T, WireError> {
    if bytes.len() > MAX_FILE_BYTES {
        return Err(WireError::TooLarge);
    }
    #[derive(serde::Deserialize)]
    struct Version {
        #[serde(default)]
        v: u32,
    }
    let version: Version = serde_json::from_slice(bytes).map_err(|e| WireError::Json(e.to_string()))?;
    if version.v > schema {
        return Err(WireError::NewerMajor(version.v));
    }
    serde_json::from_slice(bytes).map_err(|e| WireError::Json(e.to_string()))
}

/// Reads the shared file at `path`: a regular file of this user (by `owner`), no link, at most `MAX_FILE_BYTES`,
/// of a schema up to `schema`, with its strings cut as every foreign string is.
fn read_at<T: DeserializeOwned>(
    path: &Path,
    schema: u32,
    owner: fn(&Path, u32) -> u32,
    recut: fn(&mut T),
) -> SharedRead<T> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return SharedRead::Missing,
        Err(e) => return SharedRead::Ignored(format!("cannot be looked at: {e}")),
    };
    if meta.file_type().is_symlink() {
        return SharedRead::Ignored("a symbolic link".to_owned());
    }
    if !meta.is_file() {
        return SharedRead::Ignored("not a regular file".to_owned());
    }
    if owner(path, meta.uid()) != private_fs::euid() {
        return SharedRead::Ignored("another user's file".to_owned());
    }
    if meta.len() > MAX_FILE_BYTES as u64 {
        return SharedRead::Ignored(format!("{} bytes, over the {MAX_FILE_BYTES}-byte limit", meta.len()));
    }
    let text = match private_fs::read_private_nofollow_capped(path, MAX_FILE_BYTES as u64) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return SharedRead::Missing,
        Err(e) => return SharedRead::Ignored(format!("cannot be read: {e}")),
    };
    match parse::<T>(text.as_bytes(), schema) {
        Ok(mut value) => {
            recut(&mut value);
            SharedRead::Found(value)
        }
        Err(WireError::NewerMajor(v)) => SharedRead::Ignored(format!("schema {v} is newer than {schema}")),
        Err(e) => SharedRead::Ignored(e.to_string()),
    }
}

pub(crate) fn read_selection_at(path: &Path, schema: u32, owner: fn(&Path, u32) -> u32) -> SharedRead<Selection> {
    read_at(path, schema, owner, recut_selection)
}

pub(crate) fn read_round_at(path: &Path, schema: u32, owner: fn(&Path, u32) -> u32) -> SharedRead<Round> {
    read_at(path, schema, owner, recut_round)
}

fn owner_as_is(_: &Path, uid: u32) -> u32 {
    uid
}

/// A shared file's bytes: the schema version first, then the value's own fields, with its strings cut.
fn file_json<T: Serialize + Clone>(value: &T, recut: fn(&mut T)) -> io::Result<Vec<u8>> {
    #[derive(Serialize)]
    struct Versioned<'a, T> {
        v: u32,
        #[serde(flatten)]
        body: &'a T,
    }
    let mut value = value.clone();
    recut(&mut value);
    let bytes = serde_json::to_vec(&Versioned {
        v: SCHEMA_V,
        body: &value,
    })
    .map_err(io::Error::other)?;
    if bytes.len() > MAX_FILE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "a shared agents file of {} bytes is over the {MAX_FILE_BYTES}-byte limit",
                bytes.len()
            ),
        ));
    }
    Ok(bytes)
}

/// The shared files of one control directory. Every method does disk work on the calling thread, so none is
/// called on the GTK thread.
pub struct SharedStore {
    /// The agents directory, opened once and judged (this user's, no group or other bit, not a link); every
    /// write, removal and read goes through it.
    dir: File,
    #[cfg_attr(target_os = "linux", allow(dead_code))]
    path: PathBuf,
}

impl SharedStore {
    /// Makes `<control dir>/agents` if needed (0700) and opens it as the publisher and the reader do: a link at
    /// its name, another user's directory or one with a group or other bit is refused.
    pub fn open(control_dir: &Path) -> io::Result<SharedStore> {
        crate::panel_control::ensure_dir(control_dir)
            .map_err(|e| io::Error::new(io::ErrorKind::PermissionDenied, e))?;
        let path = agents_dir(control_dir);
        private_fs::create_private_dir_all(&path, control_dir)?;
        let dir = open_own_dir(&path)?;
        Ok(SharedStore { dir, path })
    }

    /// `name` in the very directory that was judged at `open`.
    fn at(&self, name: &str) -> PathBuf {
        #[cfg(target_os = "linux")]
        {
            PathBuf::from(format!("/proc/self/fd/{}", self.dir.as_raw_fd())).join(name)
        }
        #[cfg(not(target_os = "linux"))]
        {
            self.path.join(name)
        }
    }

    /// The selection, or `None` when there is none or what is there is not read (a link, another user's file,
    /// a newer schema's).
    pub fn read_selection(&self) -> Option<Selection> {
        read_selection_at(&self.at(SELECTION_FILE), SCHEMA_V, owner_as_is).found()
    }

    /// Replaces the selection whole (a temporary name, 0600, synced, renamed), never following a link.
    pub fn write_selection(&self, s: &Selection) -> io::Result<()> {
        let bytes = file_json(s, recut_selection)?;
        private_fs::publish_private_file(&self.dir, OsStr::new(SELECTION_FILE), &bytes).map(|_| ())
    }

    /// The round, or `None` when none runs or what is there is not read.
    pub fn read_round(&self) -> Option<Round> {
        read_round_at(&self.at(ROUND_FILE), SCHEMA_V, owner_as_is).found()
    }

    /// Replaces the round whole, as [`write_selection`](Self::write_selection) does; `None` removes the file
    /// (none there is not an error).
    pub fn write_round(&self, r: Option<&Round>) -> io::Result<()> {
        match r {
            Some(r) => {
                let bytes = file_json(r, recut_round)?;
                private_fs::publish_private_file(&self.dir, OsStr::new(ROUND_FILE), &bytes).map(|_| ())
            }
            None => self.remove(ROUND_FILE),
        }
    }

    fn remove(&self, name: &str) -> io::Result<()> {
        let name = CString::new(name).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        // SAFETY: `self.dir` is open for the call and `name` is NUL-terminated and outlives it. Without
        // `AT_REMOVEDIR` a link at the name is removed itself, never what it points to.
        if unsafe { libc::unlinkat(self.dir.as_raw_fd(), name.as_ptr(), 0) } != 0 {
            let e = io::Error::last_os_error();
            if e.kind() != io::ErrorKind::NotFound {
                return Err(e);
            }
        }
        Ok(())
    }
}

/// One stop of a round: a row that waits for the person, or every trust row of one window, which one answer
/// settles. `key` is the stop's name in the order: its oldest member's.
struct Stop<'a> {
    key: &'a str,
    members: Vec<&'a str>,
}

impl Stop<'_> {
    fn visited(&self, round: Option<&Round>) -> bool {
        round.is_some_and(|r| self.members.iter().any(|m| r.visited.iter().any(|v| v == m)))
    }
}

/// When a row began to wait: its `waiting_since`, else when it took its state.
fn waited(r: &Row) -> u64 {
    r.waiting_since.unwrap_or(r.since)
}

/// The view's stops, oldest wait first; rows that waited equally long keep the view's order.
fn stops(view: &View) -> Vec<Stop<'_>> {
    let mut waiting: Vec<&Row> = view
        .rows
        .iter()
        .filter(|r| r.class == Class::Needs && !r.key.is_empty())
        .collect();
    waiting.sort_by_key(|r| waited(r));
    let mut stops: Vec<Stop> = Vec::new();
    let mut seen: HashSet<&str> = HashSet::new();
    let mut trust_of_window: HashMap<&str, usize> = HashMap::new();
    for r in waiting {
        if !seen.insert(r.key.as_str()) {
            continue;
        }
        if r.needs.trust {
            if let Some(&at) = trust_of_window.get(r.window.instance.as_str()) {
                stops[at].members.push(r.key.as_str());
                continue;
            }
            trust_of_window.insert(r.window.instance.as_str(), stops.len());
        }
        stops.push(Stop {
            key: r.key.as_str(),
            members: vec![r.key.as_str()],
        });
    }
    stops
}

/// Whether `round` stood still for [`ROUND_MAX_IDLE_MS`] since its last move: then it is over, for both keys, the
/// automatic advance and every face.
pub fn is_idle(round: &Round, now_ms: u64) -> bool {
    now_ms.saturating_sub(round.at_ms) >= ROUND_MAX_IDLE_MS
}

/// Whether `round` runs over `view` at `now_ms`: it is at a stop that names a row the view still lists, and it has
/// not stood idle too long. A round that ended with the person in it (no current stop) is kept only so `prefix B`
/// can go back; one whose stop's process went, or that stood idle, runs no more either.
pub fn is_running(view: &View, round: &Round, now_ms: u64) -> bool {
    !is_idle(round, now_ms)
        && round
            .current
            .as_deref()
            .is_some_and(|key| view.rows.iter().any(|row| row.key == key))
}

/// The keys of the round's stops: rows of class `needs`, the oldest `waiting_since` first, a window's trust rows
/// as one stop (named by its oldest), and the stops `round` has visited last.
pub fn round_order(view: &View, round: Option<&Round>) -> Vec<String> {
    let (fresh, visited): (Vec<Stop>, Vec<Stop>) = stops(view).into_iter().partition(|s| !s.visited(round));
    fresh.into_iter().chain(visited).map(|s| s.key.to_owned()).collect()
}

/// A round starting from `from` (where the keys are, to go back to) at `key`, or at the order's first stop when
/// no key is given; the stop's every member is visited. `None` when nothing waits, or when `key` names no row that waits.
pub fn start_at(view: &View, from: RoundPoint, key: Option<&str>, now_ms: u64) -> Option<Round> {
    let stops = stops(view);
    let (current, stop) = match key {
        Some(key) => (key, stops.iter().find(|s| s.members.contains(&key))?),
        None => {
            let first = stops.first()?;
            (first.key, first)
        }
    };
    Some(Round {
        from,
        current: Some(current.to_owned()),
        visited: stop.members.iter().map(|m| (*m).to_owned()).collect(),
        started_at_ms: now_ms,
        at_ms: now_ms,
    })
}

/// The round moved to the first stop in the order it has not visited, keeping where it started. `None` when no
/// stop is left: the round has ended. Visited keys no row in the view has any more are dropped, so the file
/// never outgrows the view; a stop's every member is recorded as visited, so a window's trust stop stays visited
/// when the row that named it is gone.
pub fn advance(view: &View, r: &Round, now_ms: u64) -> Option<Round> {
    let stops = stops(view);
    let next = stops.iter().find(|s| !s.visited(Some(r)))?;
    let mut visited: Vec<String> = r
        .visited
        .iter()
        .filter(|k| view.rows.iter().any(|row| row.key == **k))
        .cloned()
        .collect();
    visited.extend(next.members.iter().map(|m| (*m).to_owned()));
    Some(Round {
        from: r.from.clone(),
        current: Some(next.key.to_owned()),
        visited,
        started_at_ms: r.started_at_ms,
        at_ms: now_ms,
    })
}

/// Whether the process `own_instance` moves the round on by itself: only that process's own current row, shown
/// and not mid-move, once it has left class `needs` (its card or question was answered). Every other process
/// sees `false` for the same view, so of all the windows watching, one moves it. The answer is `false` again for
/// the round it writes next (its current row still waits), but the process's own reader hands that round back
/// only later: until then the caller asks with the round it wrote, not the one its reader last gave, or it would
/// advance twice.
pub fn should_advance(own_instance: &str, view: &View, r: &Round) -> bool {
    let Some(current) = r.current.as_deref() else {
        return false;
    };
    view.rows
        .iter()
        .find(|row| row.key == current)
        .is_some_and(|row| row.window.instance == own_instance && !row.moving && row.class != Class::Needs)
}
