//! A plugin's own files under `<state home>/eitri/plugins/<id>/`: small values (`state/<key>.json`, the
//! last write wins, replaced atomically) and append-only logs (`log/<stream>.NNNN.jsonl`, 32 MiB segments).
//! Everything is private to this user (directories 0700, files 0600, whatever the umask), and two Eitri
//! windows -- two processes -- may use one plugin's files at once: appends hold an exclusive `flock` on
//! the stream's lock file, reads a shared one. Nothing is compacted and nothing depends on a server.
//!
//! The store is used from a worker thread ([`StoreWorker`]), never from the GTK thread: a lock another
//! window holds, or a slow disk, waits there.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub segment_bytes: u64,
    pub total_log_bytes: u64,
    pub read_page_bytes: usize,
}

impl Limits {
    pub const DEFAULT: Limits = Limits {
        segment_bytes: 32 << 20,
        total_log_bytes: 1 << 30,
        read_page_bytes: 1 << 20,
    };
}

pub const MAX_KEYS: usize = 64;
pub const MAX_VALUE_BYTES: usize = 192 * 1024;
pub const MAX_ITEM_BYTES: usize = 64 * 1024;
pub const MAX_APPEND_BYTES: usize = 256 * 1024;
const MAX_SEGMENT: u32 = 9999;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreErrorKind {
    BadRequest,
    TooLarge,
    Io,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreError {
    pub kind: StoreErrorKind,
    pub message: String,
}

fn bad(message: impl Into<String>) -> StoreError {
    StoreError {
        kind: StoreErrorKind::BadRequest,
        message: message.into(),
    }
}

fn too_large(message: impl Into<String>) -> StoreError {
    StoreError {
        kind: StoreErrorKind::TooLarge,
        message: message.into(),
    }
}

fn io(context: &Path, e: std::io::Error) -> StoreError {
    StoreError {
        kind: StoreErrorKind::Io,
        message: format!("{}: {e}", context.display()),
    }
}

/// `[a-z0-9_.-]`, 1-64, not starting with `.`: a key is a file name.
pub fn valid_key(key: &str) -> bool {
    (1..=64).contains(&key.len())
        && !key.starts_with('.')
        && key
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'.' | b'-'))
}

/// `[a-z0-9-]`, 1-40, not starting with `-`: a stream is the first part of a file name.
pub fn valid_stream(stream: &str) -> bool {
    (1..=40).contains(&stream.len())
        && !stream.starts_with('-')
        && stream
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

fn euid() -> u32 {
    // SAFETY: `geteuid` has no preconditions and cannot fail.
    unsafe { libc::geteuid() }
}

/// Opens `path` to append, creating it 0600; refuses a symlink in its place (`O_NOFOLLOW`), a FIFO
/// (`O_NONBLOCK`, then the type check) and another user's file. Never truncates.
fn open_append(path: &Path) -> Result<File, StoreError> {
    let file = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .mode(agent::private_fs::PRIVATE_FILE_MODE)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
        .map_err(|e| io(path, e))?;
    own_regular(&file, path)?;
    Ok(file)
}

fn open_read(path: &Path) -> Result<File, StoreError> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
        .map_err(|e| io(path, e))?;
    own_regular(&file, path)?;
    Ok(file)
}

fn own_regular(file: &File, path: &Path) -> Result<(), StoreError> {
    let meta = file.metadata().map_err(|e| io(path, e))?;
    if !meta.is_file() || meta.uid() != euid() {
        return Err(io(path, std::io::Error::other("not a regular file of this user")));
    }
    // Exactly 0600 whatever the umask; fchmod on the open handle.
    file.set_permissions(std::fs::Permissions::from_mode(agent::private_fs::PRIVATE_FILE_MODE))
        .map_err(|e| io(path, e))
}

/// How often a lock another window holds is tried again while an operation still has time.
const LOCK_RETRY: Duration = Duration::from_millis(10);

/// An `flock` held until dropped.
// The open descriptor is the lock: closing it releases the `flock`, so it is held and never read.
struct Locked(#[allow(dead_code)] File);

impl Locked {
    /// Waits for the lock: for as long as it takes with no `until`, else until then. A window whose process
    /// stopped while holding the lock would otherwise hold this worker, and every operation queued behind
    /// it, for good. The lock is always tried once, so a free lock is taken even past `until`.
    fn take(path: &Path, operation: libc::c_int, until: Option<Instant>) -> Result<Locked, StoreError> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(agent::private_fs::PRIVATE_FILE_MODE)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(path)
            .map_err(|e| io(path, e))?;
        own_regular(&file, path)?;
        let operation = if until.is_some() {
            operation | libc::LOCK_NB
        } else {
            operation
        };
        loop {
            // SAFETY: the descriptor is `file`'s own and stays open for the call.
            if unsafe { libc::flock(file.as_raw_fd(), operation) } == 0 {
                return Ok(Locked(file));
            }
            let e = std::io::Error::last_os_error();
            match (e.raw_os_error(), until) {
                (Some(libc::EINTR), _) => {}
                (Some(libc::EWOULDBLOCK), Some(until)) => {
                    let now = Instant::now();
                    if now >= until {
                        return Err(io(
                            path,
                            std::io::Error::other("another window held the lock for longer than the request may wait"),
                        ));
                    }
                    std::thread::sleep(LOCK_RETRY.min(until - now));
                }
                _ => return Err(io(path, e)),
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct PluginStore {
    root: PathBuf,
    dir: PathBuf,
    limits: Limits,
}

impl PluginStore {
    /// `eitri_root` is `<state home>/eitri`, the directory Eitri owns outright.
    pub fn new(eitri_root: PathBuf, id: &str) -> PluginStore {
        PluginStore::with_limits(eitri_root, id, Limits::DEFAULT)
    }

    pub fn with_limits(eitri_root: PathBuf, id: &str, limits: Limits) -> PluginStore {
        let dir = eitri_root.join("plugins").join(id);
        PluginStore {
            root: eitri_root,
            dir,
            limits,
        }
    }

    /// `<state home>/eitri`, from `XDG_STATE_HOME` or `HOME`, as every other piece of Eitri's state.
    pub fn eitri_root_from_env() -> Option<PathBuf> {
        crate::state_paths::state_subdir(
            std::env::var_os("XDG_STATE_HOME").as_deref(),
            std::env::var_os("HOME").as_deref(),
            "plugins",
        )
        .and_then(|plugins| plugins.parent().map(Path::to_path_buf))
    }

    fn sub(&self, name: &str) -> Result<PathBuf, StoreError> {
        let dir = self.dir.join(name);
        agent::private_fs::create_private_dir_all(&dir, &self.root).map_err(|e| io(&dir, e))?;
        Ok(dir)
    }

    pub fn read(&self, key: &str) -> Result<Option<Value>, StoreError> {
        if !valid_key(key) {
            return Err(bad(format!(
                "key {:?} must be [a-z0-9_.-], 1-64, not starting with .",
                key
            )));
        }
        let path = self.dir.join("state").join(format!("{key}.json"));
        match agent::private_fs::read_private_to_string(&path) {
            Ok(text) => serde_json::from_str(&text)
                .map(Some)
                .map_err(|e| io(&path, std::io::Error::other(e))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(io(&path, e)),
        }
    }

    pub fn write(&self, key: &str, value: &Value) -> Result<(), StoreError> {
        self.write_until(key, value, None)
    }

    /// [`Self::write`], waiting for another window's lock only until `until`.
    fn write_until(&self, key: &str, value: &Value, until: Option<Instant>) -> Result<(), StoreError> {
        if !valid_key(key) {
            return Err(bad(format!(
                "key {:?} must be [a-z0-9_.-], 1-64, not starting with .",
                key
            )));
        }
        let text = serde_json::to_string(value).map_err(|e| bad(e.to_string()))?;
        if text.len() > MAX_VALUE_BYTES {
            return Err(too_large(format!("a value of {} bytes; at most 192 KiB", text.len())));
        }
        let dir = self.sub("state")?;
        let path = dir.join(format!("{key}.json"));
        // One writer at a time per plugin: the key count and the temporary name are shared by every window
        // and every worker, so the count, the write and the rename are one step. Key names cannot start with
        // a dot, so this file never collides with a key.
        let _lock = Locked::take(&dir.join(".write.lock"), libc::LOCK_EX, until)?;
        if !path.exists() {
            let keys = std::fs::read_dir(&dir)
                .map_err(|e| io(&dir, e))?
                .filter_map(Result::ok)
                .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
                .count();
            if keys >= MAX_KEYS {
                return Err(too_large(format!("{MAX_KEYS} keys already; write over one of them")));
            }
        }
        let tmp = dir.join(format!("{key}.json.{}.tmp", std::process::id()));
        let written = agent::private_fs::write_private(&tmp, text.as_bytes())
            .and_then(|()| File::open(&tmp).and_then(|f| f.sync_all()))
            .and_then(|()| std::fs::rename(&tmp, &path));
        if let Err(e) = written {
            let _ = std::fs::remove_file(&tmp);
            return Err(io(&path, e));
        }
        Ok(())
    }

    /// The segments of `stream`, numbered, in order.
    fn segments(&self, log: &Path, stream: &str) -> Result<Vec<(u32, PathBuf)>, StoreError> {
        let prefix = format!("{stream}.");
        let mut found = Vec::new();
        let entries = match std::fs::read_dir(log) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(found),
            Err(e) => return Err(io(log, e)),
        };
        for entry in entries.filter_map(Result::ok) {
            let name = entry.file_name().to_string_lossy().into_owned();
            let number = name
                .strip_prefix(&prefix)
                .and_then(|rest| rest.strip_suffix(".jsonl"))
                .filter(|n| n.len() == 4)
                .and_then(|n| n.parse::<u32>().ok());
            if let Some(n) = number {
                found.push((n, entry.path()));
            }
        }
        found.sort();
        Ok(found)
    }

    fn log_bytes(&self, log: &Path) -> Result<u64, StoreError> {
        let mut total = 0;
        for entry in std::fs::read_dir(log).map_err(|e| io(log, e))?.filter_map(Result::ok) {
            if entry.path().extension().is_some_and(|x| x == "jsonl") {
                total += entry.metadata().map(|m| m.len()).unwrap_or(0);
            }
        }
        Ok(total)
    }

    pub fn append(&self, stream: &str, items: &[Value]) -> Result<(), StoreError> {
        self.append_until(stream, items, None)
    }

    /// [`Self::append`], waiting for another window's lock only until `until`.
    fn append_until(&self, stream: &str, items: &[Value], until: Option<Instant>) -> Result<(), StoreError> {
        if !valid_stream(stream) {
            return Err(bad(format!(
                "stream {stream:?} must be [a-z0-9-], 1-40, not starting with -"
            )));
        }
        let mut text = String::new();
        for item in items {
            let line = serde_json::to_string(item).map_err(|e| bad(e.to_string()))?;
            if line.len() > MAX_ITEM_BYTES {
                return Err(too_large(format!("an item of {} bytes; at most 64 KiB", line.len())));
            }
            text.push_str(&line);
            text.push('\n');
        }
        if text.len() > MAX_APPEND_BYTES {
            return Err(too_large(format!("an append of {} bytes; at most 256 KiB", text.len())));
        }
        if text.is_empty() {
            return Ok(());
        }
        let log = self.sub("log")?;
        // The 1 GiB limit spans every stream, so the check and the append are one step across streams: the
        // plugin-wide lock is always taken before a stream's (a stream name cannot start with a dot).
        let _plugin_lock = Locked::take(&log.join(".total.lock"), libc::LOCK_EX, until)?;
        let _lock = Locked::take(&log.join(format!("{stream}.lock")), libc::LOCK_EX, until)?;
        if self.log_bytes(&log)? + text.len() as u64 > self.limits.total_log_bytes {
            return Err(too_large(
                "this plugin's logs have reached their 1 GiB limit; export the log and start a new one",
            ));
        }
        let (mut number, mut path) = self
            .segments(&log, stream)?
            .pop()
            .unwrap_or_else(|| (0, log.join(format!("{stream}.0000.jsonl"))));
        let mut size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        // A writer that died mid-line leaves a tail without its newline; an append glued onto it would
        // be unreadable, so the tail is closed first and read_log skips it as the damaged line it is. A
        // bare newline is not enough: a cut-off number (`123` cut to `1`) is still JSON, and closing it
        // would turn the fragment into an item nobody wrote, so the closing carries a byte JSON cannot hold.
        let mut open_tail = size > 0 && !ends_with_newline(&path)?;
        if size > 0
            && size + text.len() as u64 + if open_tail { TAIL_CLOSE.len() as u64 } else { 0 }
                > self.limits.segment_bytes
        {
            number += 1;
            if number > MAX_SEGMENT {
                return Err(too_large("the stream has used every segment number; export it"));
            }
            path = log.join(format!("{stream}.{number:04}.jsonl"));
            size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            open_tail = false;
        }
        if open_tail {
            text.insert_str(0, TAIL_CLOSE);
        }
        let mut file = open_append(&path)?;
        if let Err(e) = file.write_all(text.as_bytes()) {
            // Give back the bytes that did get in, so the next append is not glued onto a fragment.
            let _ = file.set_len(size);
            return Err(io(&path, e));
        }
        file.sync_data().map_err(|e| io(&path, e))
    }

    /// One page of at most `read_page_bytes` from `from` (an opaque cursor; `None` is the start), and the
    /// cursor of the next page, `None` when this page reached the end. An item is never split; a line that
    /// is not JSON is skipped and logged.
    pub fn read_log(&self, stream: &str, from: Option<&str>) -> Result<(Vec<Value>, Option<String>), StoreError> {
        self.read_log_until(stream, from, None)
    }

    /// [`Self::read_log`], waiting for another window's lock only until `until`.
    fn read_log_until(
        &self,
        stream: &str,
        from: Option<&str>,
        until: Option<Instant>,
    ) -> Result<(Vec<Value>, Option<String>), StoreError> {
        if !valid_stream(stream) {
            return Err(bad(format!(
                "stream {stream:?} must be [a-z0-9-], 1-40, not starting with -"
            )));
        }
        let (mut segment, mut offset) = match from {
            None => (0u32, 0u64),
            Some(cursor) => {
                parse_cursor(cursor).ok_or_else(|| bad(format!("{cursor:?} is not a cursor this host wrote")))?
            }
        };
        let log = self.dir.join("log");
        if !log.exists() {
            return Ok((Vec::new(), None));
        }
        let _lock = Locked::take(&self.sub("log")?.join(format!("{stream}.lock")), libc::LOCK_SH, until)?;
        let segments = self.segments(&log, stream)?;
        let mut items = Vec::new();
        let mut budget = self.limits.read_page_bytes;
        let first = segment;
        for (number, path) in segments.iter().filter(|(n, _)| *n >= first) {
            if *number > segment {
                segment = *number;
                offset = 0;
            }
            let mut file = open_read(path)?;
            let len = file.metadata().map_err(|e| io(path, e))?.len();
            if offset >= len {
                continue;
            }
            file.seek(SeekFrom::Start(offset)).map_err(|e| io(path, e))?;
            let want = ((len - offset) as usize).min(budget);
            let mut buf = vec![0u8; want];
            file.read_exact(&mut buf).map_err(|e| io(path, e))?;
            let Some(end) = buf.iter().rposition(|&b| b == b'\n') else {
                if want as u64 == len - offset {
                    // The segment ends inside a line (a writer died mid-append): nothing whole is left in it.
                    eprintln!("[plugin] {}: skipped an unterminated last line", path.display());
                    offset = len;
                    continue;
                }
                if budget == self.limits.read_page_bytes {
                    return Err(too_large(format!(
                        "{}: a line longer than a whole page",
                        path.display()
                    )));
                }
                // Not even one whole line fits what is left of the page (lines skipped as damaged count
                // against it too): it is the next page's first.
                return Ok((items, Some(cursor(segment, offset))));
            };
            for line in buf[..end].split(|&b| b == b'\n') {
                match serde_json::from_slice::<Value>(line) {
                    Ok(v) => items.push(v),
                    Err(e) => eprintln!("[plugin] {}: skipped a line that is not JSON: {e}", path.display()),
                }
            }
            let reached_end = want as u64 == len - offset;
            offset += end as u64 + 1;
            budget -= end + 1;
            if reached_end && offset < len {
                eprintln!("[plugin] {}: skipped an unterminated last line", path.display());
                offset = len;
            }
            if offset < len {
                return Ok((items, Some(cursor(segment, offset))));
            }
            if budget == 0 {
                let more = segments
                    .iter()
                    .any(|(n, p)| *n > segment && std::fs::metadata(p).is_ok_and(|m| m.len() > 0));
                return Ok((items, more.then(|| cursor(segment, offset))));
            }
        }
        Ok((items, None))
    }

    /// Writes the capability list; returns a note when it differs from the one recorded before.
    pub fn record_capabilities(&self, capabilities: &[&str]) -> Result<Option<String>, StoreError> {
        agent::private_fs::create_private_dir_all(&self.dir, &self.root).map_err(|e| io(&self.dir, e))?;
        let path = self.dir.join("capabilities.json");
        let now: Vec<String> = capabilities.iter().map(|c| c.to_string()).collect();
        let before: Option<Vec<String>> = agent::private_fs::read_private_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok());
        if before.as_ref() == Some(&now) {
            return Ok(None);
        }
        let text = serde_json::to_string(&now).map_err(|e| bad(e.to_string()))?;
        agent::private_fs::write_private(&path, text.as_bytes()).map_err(|e| io(&path, e))?;
        Ok(before.map(|before| {
            let added: Vec<&str> = now.iter().filter(|c| !before.contains(c)).map(String::as_str).collect();
            let removed: Vec<&str> = before.iter().filter(|c| !now.contains(c)).map(String::as_str).collect();
            format!(
                "the manifest's capabilities changed since the last start (added: {}; removed: {})",
                if added.is_empty() {
                    "none".into()
                } else {
                    added.join(", ")
                },
                if removed.is_empty() {
                    "none".into()
                } else {
                    removed.join(", ")
                }
            )
        }))
    }
}

/// Written after an unterminated tail: a byte no JSON value can contain, then the newline.
const TAIL_CLOSE: &str = "\u{1e}\n";

/// Whether the last byte of the non-empty file at `path` is a newline.
fn ends_with_newline(path: &Path) -> Result<bool, StoreError> {
    let mut file = open_read(path)?;
    let len = file.metadata().map_err(|e| io(path, e))?.len();
    if len == 0 {
        return Ok(true);
    }
    file.seek(SeekFrom::Start(len - 1)).map_err(|e| io(path, e))?;
    let mut last = [0u8; 1];
    file.read_exact(&mut last).map_err(|e| io(path, e))?;
    Ok(last[0] == b'\n')
}

fn cursor(segment: u32, offset: u64) -> String {
    format!("{segment:04}:{offset}")
}

fn parse_cursor(text: &str) -> Option<(u32, u64)> {
    let (segment, offset) = text.split_once(':')?;
    if segment.len() != 4 || !segment.bytes().all(|b| b.is_ascii_digit()) || !offset.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    Some((segment.parse().ok()?, offset.parse().ok()?))
}

#[derive(Debug, Clone, PartialEq)]
pub enum StoreOp {
    Read { key: String },
    Write { key: String, value: Value },
    Append { stream: String, items: Vec<Value> },
    ReadLog { stream: String, from: Option<String> },
}

impl PluginStore {
    /// One operation as the page asked it, waiting for another window's lock only until `until`.
    fn run(&self, op: StoreOp, until: Instant) -> Result<Value, StoreError> {
        let until = Some(until);
        match op {
            StoreOp::Read { key } => self.read(&key).map(|v| json!({ "value": v })),
            StoreOp::Write { key, value } => self.write_until(&key, &value, until).map(|()| json!({})),
            StoreOp::Append { stream, items } => self.append_until(&stream, &items, until).map(|()| json!({})),
            StoreOp::ReadLog { stream, from } => self
                .read_log_until(&stream, from.as_deref(), until)
                .map(|(items, next)| json!({ "items": items, "next": next })),
        }
    }
}

/// One thread per plugin that does the store's work in order and raises the host's wake per answer. Each
/// operation carries its deadline: past it, a lock another window holds is no longer waited for, so one stuck
/// window cannot hold every later operation. An operation is still run when it reaches the worker after its
/// deadline, so a write the page was told timed out may still apply.
pub struct StoreWorker {
    tx: mpsc::Sender<(u64, StoreOp, Instant)>,
    rx: mpsc::Receiver<(u64, Result<Value, StoreError>)>,
}

impl StoreWorker {
    /// With a `gate`, each operation first waits for one message on it (tests only: it stands in for a disk
    /// or a lock that does not answer); a closed gate lets everything through.
    pub fn spawn(
        store: PluginStore,
        wake: agent::UiWake,
        gate: Option<mpsc::Receiver<()>>,
    ) -> std::io::Result<StoreWorker> {
        let (tx, jobs) = mpsc::channel::<(u64, StoreOp, Instant)>();
        let (done, rx) = mpsc::channel();
        std::thread::Builder::new().name("plugin-store".into()).spawn(move || {
            for (re, op, until) in jobs {
                if let Some(gate) = &gate {
                    let _ = gate.recv();
                }
                let result = store.run(op, until);
                if done.send((re, result)).is_err() {
                    return;
                }
                wake();
            }
        })?;
        Ok(StoreWorker { tx, rx })
    }

    pub fn submit(&self, re: u64, op: StoreOp, until: Instant) {
        let _ = self.tx.send((re, op, until));
    }

    pub fn try_results(&self) -> Vec<(u64, Result<Value, StoreError>)> {
        self.rx.try_iter().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::test_support::Scratch;
    use serde_json::json;

    fn store(s: &Scratch, limits: Limits) -> PluginStore {
        PluginStore::with_limits(s.0.join("eitri"), "demo", limits)
    }

    #[test]
    fn a_value_round_trips_and_a_missing_key_is_null() {
        let s = Scratch::new("state");
        let st = store(&s, Limits::DEFAULT);
        assert_eq!(st.read("device.token").unwrap(), None);
        st.write("device.token", &json!({"t": "abc", "n": 1})).unwrap();
        assert_eq!(st.read("device.token").unwrap(), Some(json!({"t": "abc", "n": 1})));
        st.write("device.token", &json!(2)).unwrap();
        assert_eq!(st.read("device.token").unwrap(), Some(json!(2)), "the last write wins");
    }

    #[test]
    fn keys_values_and_the_key_count_are_capped() {
        let s = Scratch::new("caps");
        let st = store(&s, Limits::DEFAULT);
        let long = "k".repeat(65);
        for bad in ["", ".hidden", "a/b", "A", long.as_str()] {
            assert_eq!(
                st.write(bad, &json!(1)).unwrap_err().kind,
                StoreErrorKind::BadRequest,
                "{bad:?}"
            );
        }
        let big = json!("x".repeat(MAX_VALUE_BYTES));
        assert_eq!(st.write("big", &big).unwrap_err().kind, StoreErrorKind::TooLarge);
        for i in 0..MAX_KEYS {
            st.write(&format!("k{i}"), &json!(i)).unwrap();
        }
        assert_eq!(
            st.write("one-more", &json!(0)).unwrap_err().kind,
            StoreErrorKind::TooLarge
        );
        st.write("k0", &json!("overwrite"))
            .expect("an existing key may still be written");
    }

    #[test]
    fn a_log_pages_across_segments_in_order() {
        let s = Scratch::new("log");
        let st = store(
            &s,
            Limits {
                segment_bytes: 4096,
                total_log_bytes: 1 << 30,
                read_page_bytes: 3000,
            },
        );
        for i in 0..200 {
            st.append("events", &[json!({"i": i, "pad": "x".repeat(40)})]).unwrap();
        }
        let segments = std::fs::read_dir(s.0.join("eitri/plugins/demo/log")).unwrap().count();
        assert!(segments > 2, "the log spread over segments ({segments} files)");
        let mut got = Vec::new();
        let mut from: Option<String> = None;
        loop {
            let (items, next) = st.read_log("events", from.as_deref()).unwrap();
            assert!(!items.is_empty() || next.is_none());
            got.extend(items.into_iter().map(|v| v["i"].as_u64().unwrap()));
            match next {
                Some(cursor) => from = Some(cursor),
                None => break,
            }
        }
        assert_eq!(got, (0..200).collect::<Vec<u64>>());
        assert_eq!(st.read_log("other", None).unwrap(), (vec![], None), "an empty stream");
        assert_eq!(
            st.read_log("events", Some("garbage")).unwrap_err().kind,
            StoreErrorKind::BadRequest
        );
    }

    #[test]
    fn log_items_appends_and_the_whole_log_are_capped() {
        let s = Scratch::new("logcaps");
        let st = store(
            &s,
            Limits {
                segment_bytes: 1 << 20,
                total_log_bytes: 200_000,
                read_page_bytes: 1 << 20,
            },
        );
        let item = json!("y".repeat(MAX_ITEM_BYTES));
        assert_eq!(st.append("events", &[item]).unwrap_err().kind, StoreErrorKind::TooLarge);
        let five: Vec<Value> = (0..5).map(|_| json!("z".repeat(60 * 1024))).collect();
        assert_eq!(st.append("events", &five).unwrap_err().kind, StoreErrorKind::TooLarge);
        assert_eq!(
            st.read_log("events", None).unwrap().0.len(),
            0,
            "a refused append writes nothing"
        );
        let three: Vec<Value> = (0..3).map(|_| json!("z".repeat(60 * 1024))).collect();
        st.append("events", &three).unwrap();
        let err = st.append("events", &three).unwrap_err();
        assert_eq!(err.kind, StoreErrorKind::TooLarge);
        assert!(err.message.contains("export"), "{}", err.message);
        assert_eq!(
            st.append("Bad/Name", &[json!(1)]).unwrap_err().kind,
            StoreErrorKind::BadRequest
        );
    }

    #[test]
    fn a_torn_last_line_is_skipped_not_an_error() {
        let s = Scratch::new("torn");
        let st = store(&s, Limits::DEFAULT);
        st.append("events", &[json!(1)]).unwrap();
        let path = s.0.join("eitri/plugins/demo/log/events.0000.jsonl");
        let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(b"{\"cut").unwrap();
        assert_eq!(st.read_log("events", None).unwrap(), (vec![json!(1)], None));
    }

    #[test]
    fn an_append_after_a_torn_tail_is_not_glued_onto_it() {
        let s = Scratch::new("torn-append");
        let st = store(&s, Limits::DEFAULT);
        st.append("events", &[json!(1)]).unwrap();
        let path = s.0.join("eitri/plugins/demo/log/events.0000.jsonl");
        let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(b"{\"cut").unwrap();
        st.append("events", &[json!(2)]).unwrap();
        st.append("events", &[json!(3)]).unwrap();
        assert_eq!(
            st.read_log("events", None).unwrap(),
            (vec![json!(1), json!(2), json!(3)], None),
            "the fragment is the only thing lost"
        );
    }

    #[test]
    fn a_cut_off_number_is_not_turned_into_an_item_by_the_next_append() {
        let s = Scratch::new("torn-number");
        let st = store(&s, Limits::DEFAULT);
        st.append("events", &[json!(0)]).unwrap();
        let path = s.0.join("eitri/plugins/demo/log/events.0000.jsonl");
        let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        // `123\n` cut after its first digit: the fragment alone parses as a number.
        f.write_all(b"1").unwrap();
        st.append("events", &[json!(2)]).unwrap();
        assert_eq!(
            st.read_log("events", None).unwrap(),
            (vec![json!(0), json!(2)], None),
            "the cut fragment is dropped, not read as 1"
        );
    }

    #[test]
    fn a_page_of_damaged_lines_does_not_make_the_next_line_too_large() {
        let s = Scratch::new("damaged-page");
        let st = store(
            &s,
            Limits {
                segment_bytes: 1 << 20,
                total_log_bytes: 1 << 30,
                read_page_bytes: 3000,
            },
        );
        st.append("events", &[json!(0)]).unwrap();
        let log = s.0.join("eitri/plugins/demo/log");
        std::fs::write(log.join("events.0000.jsonl"), format!("{}\n", "x".repeat(2500))).unwrap();
        let good = json!("g".repeat(1000));
        std::fs::write(log.join("events.0001.jsonl"), format!("{good}\n")).unwrap();
        let mut got = Vec::new();
        let mut from: Option<String> = None;
        for _ in 0..5 {
            let (items, next) = st.read_log("events", from.as_deref()).unwrap();
            got.extend(items);
            match next {
                Some(c) => from = Some(c),
                None => break,
            }
        }
        assert_eq!(got, vec![good], "a line that fits a whole page is read on the next one");
        assert!(from.is_some(), "the first page ended with a cursor, not an error");
    }

    #[test]
    fn concurrent_new_keys_never_pass_the_key_cap() {
        let s = Scratch::new("racing-keys");
        let st = store(&s, Limits::DEFAULT);
        let results: Vec<Result<(), StoreError>> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..MAX_KEYS + 16)
                .map(|i| {
                    let st = st.clone();
                    scope.spawn(move || st.write(&format!("k{i}"), &json!(i)))
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        assert_eq!(results.iter().filter(|r| r.is_ok()).count(), MAX_KEYS);
        assert!(results
            .iter()
            .filter_map(|r| r.as_ref().err())
            .all(|e| e.kind == StoreErrorKind::TooLarge));
        let files = std::fs::read_dir(s.0.join("eitri/plugins/demo/state"))
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
            .count();
        assert_eq!(files, MAX_KEYS);
    }

    #[test]
    fn concurrent_writes_to_one_key_all_succeed_and_leave_whole_json() {
        let s = Scratch::new("racing-one-key");
        let st = store(&s, Limits::DEFAULT);
        std::thread::scope(|scope| {
            for t in 0..4 {
                let st = st.clone();
                scope.spawn(move || {
                    for i in 0..30 {
                        st.write("shared", &json!({"t": t, "i": i, "pad": "p".repeat(2000)}))
                            .unwrap();
                        assert!(st.read("shared").unwrap().is_some());
                    }
                });
            }
        });
        let dir = s.0.join("eitri/plugins/demo/state");
        let leftovers = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.path().extension().is_some_and(|x| x == "tmp"))
            .count();
        assert_eq!(leftovers, 0, "no temporary file is left behind");
    }

    #[test]
    fn concurrent_appends_to_different_streams_share_one_total_cap() {
        let s = Scratch::new("racing-streams");
        let st = store(
            &s,
            Limits {
                segment_bytes: 1 << 20,
                total_log_bytes: 200_000,
                read_page_bytes: 1 << 20,
            },
        );
        let two: Vec<Value> = (0..2).map(|_| json!("z".repeat(40 * 1024))).collect();
        let results: Vec<Result<(), StoreError>> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..4)
                .map(|i| {
                    let st = st.clone();
                    let two = two.clone();
                    scope.spawn(move || st.append(&format!("s{i}"), &two))
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 2, "{results:?}");
        let total = st.log_bytes(&s.0.join("eitri/plugins/demo/log")).unwrap();
        assert!(total <= 200_000, "{total} bytes");
    }

    #[test]
    fn a_changed_capability_list_is_noticed_once() {
        let s = Scratch::new("caplist");
        let st = store(&s, Limits::DEFAULT);
        assert_eq!(
            st.record_capabilities(&["tasks", "state"]).unwrap(),
            None,
            "the first load is the grant"
        );
        assert_eq!(st.record_capabilities(&["tasks", "state"]).unwrap(), None);
        let note = st.record_capabilities(&["tasks", "state", "net"]).unwrap().unwrap();
        assert!(note.contains("net"), "{note}");
        assert_eq!(st.record_capabilities(&["tasks", "state", "net"]).unwrap(), None);
    }

    #[test]
    fn the_worker_answers_off_the_callers_thread_and_raises_the_wake() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let s = Scratch::new("worker");
        let wakes = std::sync::Arc::new(AtomicUsize::new(0));
        let w = wakes.clone();
        let worker = StoreWorker::spawn(
            store(&s, Limits::DEFAULT),
            std::sync::Arc::new(move || {
                w.fetch_add(1, Ordering::SeqCst);
            }),
            None,
        )
        .unwrap();
        let until = Instant::now() + Duration::from_secs(60);
        worker.submit(
            7,
            StoreOp::Write {
                key: "k".into(),
                value: json!(1),
            },
            until,
        );
        worker.submit(8, StoreOp::Read { key: "k".into() }, until);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        let mut got = Vec::new();
        while got.len() < 2 {
            assert!(std::time::Instant::now() < deadline);
            got.extend(worker.try_results());
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(got, [(7, Ok(json!({}))), (8, Ok(json!({"value": 1})))]);
        assert!(wakes.load(Ordering::SeqCst) >= 1);
    }

    #[test]
    fn a_lock_another_window_holds_past_the_deadline_fails_the_write_instead_of_waiting() {
        let s = Scratch::new("lock-deadline");
        let st = store(&s, Limits::DEFAULT);
        st.write("k", &json!(1)).unwrap();
        // flock locks belong to an open file, so a second open of the lock file in this process conflicts
        // with the store's exactly as another window's would.
        let lock_path = s.0.join("eitri/plugins/demo/state/.write.lock");
        let held = Locked::take(&lock_path, libc::LOCK_EX, None).unwrap();
        let refused = st.write_until("k", &json!(2), Some(Instant::now())).unwrap_err();
        assert_eq!(refused.kind, StoreErrorKind::Io, "{refused:?}");
        assert_eq!(
            st.read("k").unwrap(),
            Some(json!(1)),
            "the refused write changed nothing"
        );
        drop(held);
        st.write_until("k", &json!(3), Some(Instant::now())).unwrap();
        assert_eq!(
            st.read("k").unwrap(),
            Some(json!(3)),
            "a free lock is taken even past the deadline"
        );
    }
}
