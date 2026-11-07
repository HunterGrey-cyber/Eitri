//! The remote audit log: every remote action and refusal, one JSON object per line, in
//! `<remote state>/audit/audit.jsonl` (0600, in a 0700 directory), hash-chained, rotated, and copied to the
//! systemd journal.
//!
//! **Writers.** `eitri remote serve`, each window (for what only it sees) and `eitri remote revoke` append to the
//! same file. Each record is written under an exclusive `flock` on `audit.lock` beside it, because the chain needs
//! the previous line: the writer reads the last line, hashes it into the new record's `prev`, and appends the new
//! line with one `write(2)` on an `O_APPEND` descriptor opened for that record. Opening per record means a writer
//! never appends to a file another writer has rotated away: the path is looked up again under the lock each time.
//!
//! **A record.** `{"v","time","writer","run","seq","event","prev", ...fields}` in that order. `time` is UTC,
//! RFC 3339 with milliseconds; `writer` is `serve`, `cli` or the window's instance id; `run` is 16 random hex digits
//! drawn when the log is opened, so two `serve` runs (or two `cli` calls), whose writer names repeat, never reuse a
//! `(writer, run, seq)`; `seq` counts from 1 per open; `prev` is the lowercase hex SHA-256 of the previous line's
//! bytes without its newline, 64 zeros for the first record ever. A line is at most [`AUDIT_RECORD_MAX`] bytes with
//! its newline: a longer one has its longest string fields cut, with a trailing `…`, until it fits. It is never
//! split and never dropped.
//!
//! **What is never in it.** A token, an enrollment code, a prompt or message, an agent's words, transcript or
//! notification content: no [`Event`] field could carry one, and a text enters only as its length and SHA-256
//! ([`text_digest`]). The hash is unsalted so a record can be matched to its transcript entry, which also means the
//! log is not anonymous. Strings that came from the network (claimed device names, `Host`, `Origin`, peers) are
//! stored as received; whoever shows them escapes them.
//!
//! **Rotation.** Past [`AUDIT_ROTATE_BYTES`] the file becomes `audit.jsonl.1` (`.1` to `.2` and so on, at most
//! [`AUDIT_ROTATED_FILES`], the oldest dropped), and the next record's `prev` is read from `.1`, so the chain runs
//! across files. A writer opened with a `keep_days` then deletes, oldest first, every rotated file whose newest
//! record is older than that, and does the same when it opens.
//!
//! **The journal copy.** Each record also goes to the journal as one native-protocol datagram
//! (`SYSLOG_IDENTIFIER=eitri-remote`, `EITRI_RECORD` the line itself). A same-uid process can rewrite the file but
//! not the journal, and the journal's `_EXE` names the real sender, so [`verify`] compares the two. A failed send
//! never fails a record.

use serde_json::Value;
use sha2::{Digest, Sha256};
use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

mod chain;
mod journal;
mod read;
mod rotate;
mod verify;

pub use read::{newest_by_device, read, Filter, Record};
pub use verify::{verify, verify_from, JournalEntry, JournalSource, VerifyReport, WRITTEN_DURING_CHECK};

/// The record format this build writes.
pub const AUDIT_V: u32 = 1;

/// The longest line, newline included.
pub const AUDIT_RECORD_MAX: usize = 4096;

/// The size past which `audit.jsonl` is rotated.
pub const AUDIT_ROTATE_BYTES: u64 = 10 * 1024 * 1024;

/// Rotated files kept: `audit.jsonl.1` to `audit.jsonl.5`.
pub const AUDIT_ROTATED_FILES: u32 = 5;

/// How many days of rotated files `serve` keeps when the configuration does not say.
pub const AUDIT_KEEP_DAYS_DEFAULT: u32 = 90;

/// The systemd journal's native-protocol socket.
pub const JOURNAL_SOCKET: &str = "/run/systemd/journal/socket";

/// The variable a test build (`remote-test-hooks`) reads for another journal socket; a release build never reads it.
/// Not named `..._SOCKET`: those names are the feeds a window hands its nvim child, each of which an inner window
/// drops (`companion::env`), and this one no window hands to anything.
pub const JOURNAL_SOCKET_VAR: &str = "EITRI_REMOTE_TEST_JOURNAL";

/// The log's own file.
const CURRENT: &str = "audit.jsonl";

/// The lock every writer holds for one record.
const LOCK: &str = "audit.lock";

/// What a cut string ends with.
const ELLIPSIS: &str = "…";

/// How long a record waits for another writer's lock. A record holds it for a read of one line and one write.
const LOCK_WITHIN: Duration = Duration::from_secs(2);

/// The pause between two tries at the lock.
const LOCK_PAUSE: Duration = Duration::from_millis(1);

/// The audit directory inside the remote state directory: a directory of its own, so `serve` can be given write
/// access beneath it without being able to write the device store beside it.
pub fn audit_dir(remote_state: &Path) -> PathBuf {
    remote_state.join("audit")
}

/// `audit.jsonl.<n>`.
fn rotated_name(n: u32) -> String {
    format!("{CURRENT}.{n}")
}

/// Who writes a record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriterKind {
    /// `eitri remote serve`.
    Serve,
    /// `eitri remote` in a terminal.
    Cli,
    /// A window, by its instance id.
    Instance(String),
}

impl WriterKind {
    /// The `writer` field.
    pub fn name(&self) -> &str {
        match self {
            WriterKind::Serve => "serve",
            WriterKind::Cli => "cli",
            WriterKind::Instance(id) => id,
        }
    }
}

/// Where the journal copy goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JournalTarget {
    /// Nowhere.
    Off,
    /// A native-protocol datagram socket.
    Socket(PathBuf),
}

/// The journal's socket when it exists, else [`JournalTarget::Off`]. A build with the `remote-test-hooks` feature
/// (every test build of the workspace, since `shell`'s tests enable it) takes a non-empty [`JOURNAL_SOCKET_VAR`] and
/// otherwise sends no copy, so no test writes to the real journal; a release build has no such feature and never
/// reads the variable.
pub fn journal_target() -> JournalTarget {
    journal_target_with(|name| std::env::var_os(name), |path| path.exists())
}

fn journal_target_with(env: impl Fn(&str) -> Option<OsString>, socket_exists: impl Fn(&Path) -> bool) -> JournalTarget {
    #[cfg(feature = "remote-test-hooks")]
    {
        let _ = socket_exists;
        match env(JOURNAL_SOCKET_VAR).filter(|v| !v.is_empty()) {
            Some(path) => JournalTarget::Socket(PathBuf::from(path)),
            None => JournalTarget::Off,
        }
    }
    #[cfg(not(feature = "remote-test-hooks"))]
    {
        let _ = env;
        if socket_exists(Path::new(JOURNAL_SOCKET)) {
            JournalTarget::Socket(PathBuf::from(JOURNAL_SOCKET))
        } else {
            JournalTarget::Off
        }
    }
}

/// Where a connection came in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Listener {
    /// The loopback listener.
    Loopback,
    /// The local socket.
    Socket,
    /// A named address, on its interface, of a kind (`wireguard`, `tailscale`, `lan`, ...).
    Address { addr: String, iface: String, kind: String },
}

/// A device as the store or the peer names it; `name` is as claimed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceRef {
    pub id: String,
    pub name: String,
}

/// A text, as its length in bytes and its SHA-256: the only form in which a text enters the log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextDigest {
    pub len: usize,
    pub sha256: String,
}

/// `text`'s length and lowercase hex SHA-256.
pub fn text_digest(text: &str) -> TextDigest {
    TextDigest {
        len: text.len(),
        sha256: chain::hex(&Sha256::digest(text.as_bytes())),
    }
}

/// Why something from the remote side was refused; `snake_case` on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Reason {
    BadToken,
    RevokedToken,
    EnrollCodeInvalid,
    Origin,
    Host,
    PathPrefix,
    PeerOutsideInterface,
    NotAllowed,
    Grant,
    ModeWidened,
    Bypass,
    TabNotLive,
    NotStarted,
    NotClaude,
    WorkChanged,
    SlashOrBang,
    TooLong,
    NotYours,
    DuplicateCommand,
    Busy,
    Agent,
}

impl Reason {
    /// Every reason, in declaration order.
    pub const ALL: [Reason; 21] = [
        Reason::BadToken,
        Reason::RevokedToken,
        Reason::EnrollCodeInvalid,
        Reason::Origin,
        Reason::Host,
        Reason::PathPrefix,
        Reason::PeerOutsideInterface,
        Reason::NotAllowed,
        Reason::Grant,
        Reason::ModeWidened,
        Reason::Bypass,
        Reason::TabNotLive,
        Reason::NotStarted,
        Reason::NotClaude,
        Reason::WorkChanged,
        Reason::SlashOrBang,
        Reason::TooLong,
        Reason::NotYours,
        Reason::DuplicateCommand,
        Reason::Busy,
        Reason::Agent,
    ];

    /// The word in the `reason` field.
    pub fn as_str(self) -> &'static str {
        match self {
            Reason::BadToken => "bad_token",
            Reason::RevokedToken => "revoked_token",
            Reason::EnrollCodeInvalid => "enroll_code_invalid",
            Reason::Origin => "origin",
            Reason::Host => "host",
            Reason::PathPrefix => "path_prefix",
            Reason::PeerOutsideInterface => "peer_outside_interface",
            Reason::NotAllowed => "not_allowed",
            Reason::Grant => "grant",
            Reason::ModeWidened => "mode_widened",
            Reason::Bypass => "bypass",
            Reason::TabNotLive => "tab_not_live",
            Reason::NotStarted => "not_started",
            Reason::NotClaude => "not_claude",
            Reason::WorkChanged => "work_changed",
            Reason::SlashOrBang => "slash_or_bang",
            Reason::TooLong => "too_long",
            Reason::NotYours => "not_yours",
            Reason::DuplicateCommand => "duplicate_command",
            Reason::Busy => "busy",
            Reason::Agent => "agent",
        }
    }

    /// The reason a `reason` word names.
    pub fn parse(word: &str) -> Option<Reason> {
        Reason::ALL.into_iter().find(|r| r.as_str() == word)
    }
}

/// One thing to record. No field holds a token, a code or a text; a text is only ever a [`TextDigest`].
///
/// An `Option` in `PushEndpointSet` and `ConnOpen` is written as `null` when absent (the endpoint was cleared, the
/// client sent no `Origin`); the `Option`s of `Refused` are left out when absent (not known where it was refused).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Enable {
        listener: Listener,
        confirmed: bool,
    },
    Disable {
        why: String,
    },
    ListenChanged {
        listener: Listener,
        confirmed: bool,
    },
    EnrollOffered {
        recipe: String,
        instance: String,
    },
    EnrollConfirmed {
        device: DeviceRef,
        recipe: String,
        grant: String,
        host: String,
        peer: String,
    },
    EnrollRefused {
        device: DeviceRef,
        recipe: String,
        host: String,
        peer: String,
        why: String,
    },
    EnrollExpired {
        instance: String,
    },
    /// `by` is `desk`, `cli` or `device`.
    Revoke {
        device: DeviceRef,
        by: String,
    },
    GrantChanged {
        device: DeviceRef,
        from: String,
        to: String,
    },
    DeviceFlagged {
        device: DeviceRef,
    },
    /// The endpoint's host only, never its path.
    PushEndpointSet {
        device: DeviceRef,
        host: Option<String>,
    },
    /// `wake` is `needs_you` or `finished`; `status` the HTTP status or an error word.
    Push {
        device: DeviceRef,
        host: String,
        wake: String,
        status: String,
    },
    ConnOpen {
        device: DeviceRef,
        listener: Listener,
        peer: String,
        host: String,
        origin: Option<String>,
    },
    ConnClose {
        device: DeviceRef,
        why: String,
        secs: u64,
        bytes_in: u64,
        bytes_out: u64,
    },
    Cmd {
        device: DeviceRef,
        kind: String,
        instance: String,
        key: String,
        tab: u16,
        revision: u64,
        command_id: String,
        text: Option<TextDigest>,
    },
    /// `kind`: a phone command's kind word, only as [`command_kind`] gives it, so a phone's own text never reaches the
    /// log through it. `dropped`: how many records the writer did not write since its last recorded one, carried on the
    /// next refusal it writes. `serve` counts the refusals before any device was proved that it bounds (anyone who can
    /// reach a listener can be refused); a window counts every record its full queue dropped, refusal or not, so a
    /// count there can stand for commands and enrollments too, and waits for the window's next refusal.
    Refused {
        reason: Reason,
        device: Option<DeviceRef>,
        listener: Option<Listener>,
        peer: Option<String>,
        key: Option<String>,
        kind: Option<String>,
        dropped: Option<u64>,
    },
}

/// The kinds of command a phone may send, as `cmd` and `refused` records name them.
pub const COMMAND_KINDS: [&str; 6] = [
    "interrupt",
    "send",
    "queue",
    "stop_background",
    "stop_task",
    "take_back",
];

/// The kind word of a phone command, when `word` is one; a word from the wire is recorded only as one of these, by
/// `serve` and by a window alike.
pub fn command_kind(word: &str) -> Option<&'static str> {
    COMMAND_KINDS.into_iter().find(|kind| *kind == word)
}

/// An event's fields, in the order they are written.
type Fields = Vec<(&'static str, Value)>;

fn put_str(fields: &mut Fields, key: &'static str, value: String) {
    fields.push((key, Value::String(value)));
}

fn put_device(fields: &mut Fields, device: DeviceRef) {
    put_str(fields, "device", device.id);
    put_str(fields, "device_name", device.name);
}

fn put_listener(fields: &mut Fields, listener: Listener) {
    match listener {
        Listener::Loopback => put_str(fields, "listener", "loopback".into()),
        Listener::Socket => put_str(fields, "listener", "socket".into()),
        Listener::Address { addr, iface, kind } => {
            put_str(fields, "listener", "address".into());
            put_str(fields, "listen_addr", addr);
            put_str(fields, "listen_iface", iface);
            put_str(fields, "listen_kind", kind);
        }
    }
}

impl Event {
    /// The `event` field.
    pub fn name(&self) -> &'static str {
        match self {
            Event::Enable { .. } => "enable",
            Event::Disable { .. } => "disable",
            Event::ListenChanged { .. } => "listen_changed",
            Event::EnrollOffered { .. } => "enroll_offered",
            Event::EnrollConfirmed { .. } => "enroll_confirmed",
            Event::EnrollRefused { .. } => "enroll_refused",
            Event::EnrollExpired { .. } => "enroll_expired",
            Event::Revoke { .. } => "revoke",
            Event::GrantChanged { .. } => "grant_changed",
            Event::DeviceFlagged { .. } => "device_flagged",
            Event::PushEndpointSet { .. } => "push_endpoint_set",
            Event::Push { .. } => "push",
            Event::ConnOpen { .. } => "conn_open",
            Event::ConnClose { .. } => "conn_close",
            Event::Cmd { .. } => "cmd",
            Event::Refused { .. } => "refused",
        }
    }

    /// The event's own fields, in their order.
    fn into_fields(self) -> Fields {
        let mut f = Fields::new();
        match self {
            Event::Enable { listener, confirmed } | Event::ListenChanged { listener, confirmed } => {
                put_listener(&mut f, listener);
                f.push(("confirmed", Value::Bool(confirmed)));
            }
            Event::Disable { why } => put_str(&mut f, "why", why),
            Event::EnrollOffered { recipe, instance } => {
                put_str(&mut f, "recipe", recipe);
                put_str(&mut f, "instance", instance);
            }
            Event::EnrollConfirmed {
                device,
                recipe,
                grant,
                host,
                peer,
            } => {
                put_device(&mut f, device);
                put_str(&mut f, "recipe", recipe);
                put_str(&mut f, "grant", grant);
                put_str(&mut f, "host", host);
                put_str(&mut f, "peer", peer);
            }
            Event::EnrollRefused {
                device,
                recipe,
                host,
                peer,
                why,
            } => {
                put_device(&mut f, device);
                put_str(&mut f, "recipe", recipe);
                put_str(&mut f, "host", host);
                put_str(&mut f, "peer", peer);
                put_str(&mut f, "why", why);
            }
            Event::EnrollExpired { instance } => put_str(&mut f, "instance", instance),
            Event::Revoke { device, by } => {
                put_device(&mut f, device);
                put_str(&mut f, "by", by);
            }
            Event::GrantChanged { device, from, to } => {
                put_device(&mut f, device);
                put_str(&mut f, "from", from);
                put_str(&mut f, "to", to);
            }
            Event::DeviceFlagged { device } => put_device(&mut f, device),
            Event::PushEndpointSet { device, host } => {
                put_device(&mut f, device);
                f.push(("host", host.map_or(Value::Null, Value::String)));
            }
            Event::Push {
                device,
                host,
                wake,
                status,
            } => {
                put_device(&mut f, device);
                put_str(&mut f, "host", host);
                put_str(&mut f, "wake", wake);
                put_str(&mut f, "status", status);
            }
            Event::ConnOpen {
                device,
                listener,
                peer,
                host,
                origin,
            } => {
                put_device(&mut f, device);
                put_listener(&mut f, listener);
                put_str(&mut f, "peer", peer);
                put_str(&mut f, "host", host);
                f.push(("origin", origin.map_or(Value::Null, Value::String)));
            }
            Event::ConnClose {
                device,
                why,
                secs,
                bytes_in,
                bytes_out,
            } => {
                put_device(&mut f, device);
                put_str(&mut f, "why", why);
                f.push(("secs", secs.into()));
                f.push(("bytes_in", bytes_in.into()));
                f.push(("bytes_out", bytes_out.into()));
            }
            Event::Cmd {
                device,
                kind,
                instance,
                key,
                tab,
                revision,
                command_id,
                text,
            } => {
                put_device(&mut f, device);
                put_str(&mut f, "kind", kind);
                put_str(&mut f, "instance", instance);
                put_str(&mut f, "key", key);
                f.push(("tab", tab.into()));
                f.push(("revision", revision.into()));
                put_str(&mut f, "command_id", command_id);
                if let Some(text) = text {
                    f.push(("text_len", (text.len as u64).into()));
                    put_str(&mut f, "text_sha256", text.sha256);
                }
            }
            Event::Refused {
                reason,
                device,
                listener,
                peer,
                key,
                kind,
                dropped,
            } => {
                put_str(&mut f, "reason", reason.as_str().into());
                if let Some(device) = device {
                    put_device(&mut f, device);
                }
                if let Some(listener) = listener {
                    put_listener(&mut f, listener);
                }
                for (name, value) in [("peer", peer), ("key", key), ("kind", kind)] {
                    if let Some(value) = value {
                        put_str(&mut f, name, value);
                    }
                }
                if let Some(dropped) = dropped {
                    f.push(("dropped", dropped.into()));
                }
            }
        }
        f
    }
}

/// The clock a log stamps its records with.
type Clock = Arc<dyn Fn() -> SystemTime + Send + Sync>;

/// One writer's handle on the log. Its `run` is drawn at [`AuditLog::open`] and its `seq` counts from 1 from there.
/// Recording waits up to two seconds for another writer's lock, so it never runs on the UI thread.
pub struct AuditLog {
    dir: PathBuf,
    writer: String,
    run: String,
    seq: u64,
    /// The hash of the last line this writer appended: what a record the file refused names as its `prev`.
    last_hash: Option<String>,
    keep_days: Option<u32>,
    journal: journal::Journal,
    now: Clock,
    rotate_bytes: u64,
}

impl AuditLog {
    /// Opens the log in `dir`, making the directory (0700) when it is missing. `keep_days: None` never drops a
    /// rotated file by age (the windows, the CLI); `serve` passes its setting and also prunes here.
    pub fn open(
        dir: &Path,
        writer: WriterKind,
        keep_days: Option<u32>,
        journal: JournalTarget,
    ) -> io::Result<AuditLog> {
        AuditLog::open_with_clock(dir, writer, keep_days, journal, Arc::new(SystemTime::now))
    }

    fn open_with_clock(
        dir: &Path,
        writer: WriterKind,
        keep_days: Option<u32>,
        journal: JournalTarget,
        now: Clock,
    ) -> io::Result<AuditLog> {
        make_dir(dir)?;
        let log = AuditLog {
            dir: dir.to_path_buf(),
            writer: writer.name().to_string(),
            run: new_run_id(),
            seq: 0,
            last_hash: None,
            keep_days,
            journal: journal::Journal::new(journal),
            now,
            rotate_bytes: AUDIT_ROTATE_BYTES,
        };
        if let Some(days) = keep_days {
            let _lock = log.lock()?;
            rotate::prune(&log.dir, days, (log.now)())?;
        }
        Ok(log)
    }

    /// This writer's `run`.
    pub fn run(&self) -> &str {
        &self.run
    }

    /// Appends `event` as one line chained to the line before it, rotates when the file has passed its size, and
    /// sends the line to the journal. Fails only when the line could not be written to the file; the journal gets
    /// the record then too, under its own number, so a file made unwritable cannot silence the journal's copy:
    /// `verify` names it as missing from the file.
    pub fn record(&mut self, event: Event) -> io::Result<()> {
        let name = event.name();
        let fields = event.into_fields();
        let seq = self.seq + 1;
        let written = self.append(name, seq, fields.clone());
        self.seq = seq;
        let line = match written {
            Ok(line) => {
                self.last_hash = Some(chain::line_hash(line.as_bytes()));
                line
            }
            Err(e) => {
                let prev = self.last_hash.clone().unwrap_or_else(|| chain::ZERO_PREV.to_string());
                let header = chain::Header {
                    time: format_time((self.now)()),
                    writer: &self.writer,
                    run: &self.run,
                    seq,
                    event: name,
                    prev: &prev,
                };
                if let Ok(line) = chain::render_within(&header, fields) {
                    self.journal.send(name, &self.writer, &self.run, seq, &line);
                }
                return Err(e);
            }
        };
        self.journal.send(name, &self.writer, &self.run, seq, &line);
        Ok(())
    }

    /// Writes one record numbered `seq` to the file under the lock, rotating after it when the file has passed its
    /// size; the line written.
    fn append(&mut self, name: &'static str, seq: u64, fields: Fields) -> io::Result<String> {
        let _lock = self.lock()?;
        let mut file = chain::open_current(&self.dir)?;
        let (prev, open_tail) = match chain::tail(&file)? {
            Some(tail) => (tail.hash, !tail.closed),
            None => (chain::previous_file_hash(&self.dir)?, false),
        };
        let header = chain::Header {
            time: format_time((self.now)()),
            writer: &self.writer,
            run: &self.run,
            seq,
            event: name,
            prev: &prev,
        };
        let line = chain::render_within(&header, fields)?;
        // A writer that died mid-line leaves a tail without its newline; the new line must not be glued onto
        // it. The fragment becomes a line of its own, which no reader parses and the chain still covers.
        let mut bytes = Vec::with_capacity(line.len() + 2);
        if open_tail {
            bytes.push(b'\n');
        }
        bytes.extend_from_slice(line.as_bytes());
        bytes.push(b'\n');
        chain::append(&mut file, &bytes)?;
        let size = file.metadata().map(|m| m.len()).unwrap_or(0);
        drop(file);
        if size > self.rotate_bytes {
            if let Err(e) = rotate::rotate(&self.dir) {
                eprintln!("eitri: the remote audit log was not rotated (tried again with the next record): {e}");
            } else if let Some(days) = self.keep_days {
                if let Err(e) = rotate::prune(&self.dir, days, (self.now)()) {
                    eprintln!("eitri: old remote audit files were not removed: {e}");
                }
            }
        }
        Ok(line)
    }

    /// The log's lock, tried until [`LOCK_WITHIN`] has passed. A directory someone removed is made again.
    fn lock(&self) -> io::Result<agent::private_fs::PrivateLock> {
        let path = self.dir.join(LOCK);
        let lock = match agent::private_fs::PrivateLock::open(&path) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                make_dir(&self.dir)?;
                agent::private_fs::PrivateLock::open(&path)?
            }
            other => other?,
        };
        let deadline = Instant::now() + LOCK_WITHIN;
        loop {
            if lock.try_lock()? {
                return Ok(lock);
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    format!("another process is writing {}", self.dir.join(CURRENT).display()),
                ));
            }
            std::thread::sleep(LOCK_PAUSE);
        }
    }
}

/// `dir`, 0700, with its parent as the boundary of what is tightened.
fn make_dir(dir: &Path) -> io::Result<()> {
    agent::private_fs::create_private_dir_all(dir, dir.parent().unwrap_or(dir))
}

/// 16 random hex digits. A v4 UUID fixes six of its bits (the version nibble at bits 76-79, the variant at 62-63);
/// its top 48 and lowest 16 bits are random.
fn new_run_id() -> String {
    let u = uuid::Uuid::new_v4().as_u128();
    let x = ((u >> 80) as u64) << 16 | (u as u64 & 0xffff);
    format!("{x:016x}")
}

/// `t` as `YYYY-MM-DDTHH:MM:SS.mmmZ`; a time before 1970 reads as the epoch.
pub(crate) fn format_time(t: SystemTime) -> String {
    let since = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = since.as_secs();
    let (year, month, day) = civil_from_days((secs / 86_400) as i64);
    let clock = secs % 86_400;
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        clock / 3600,
        clock % 3600 / 60,
        clock % 60,
        since.subsec_millis()
    )
}

/// A time [`format_time`] wrote, exactly that shape; anything else is `None`.
pub(crate) fn parse_time(text: &str) -> Option<SystemTime> {
    let b = text.as_bytes();
    let shape = b.len() == 24
        && b[4] == b'-'
        && b[7] == b'-'
        && b[10] == b'T'
        && b[13] == b':'
        && b[16] == b':'
        && b[19] == b'.'
        && b[23] == b'Z';
    if !shape {
        return None;
    }
    let num = |from: usize, to: usize| -> Option<u64> {
        let digits = &text[from..to];
        if digits.bytes().all(|c| c.is_ascii_digit()) {
            digits.parse().ok()
        } else {
            None
        }
    };
    let (year, month, day) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let (hour, minute, second, millis) = (num(11, 13)?, num(14, 16)?, num(17, 19)?, num(20, 23)?);
    if year < 1970 || !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || minute > 59 || second > 60
    {
        return None;
    }
    let days = days_from_civil(year as i64, month as u32, day as u32);
    if days < 0 {
        return None;
    }
    let secs = days as u64 * 86_400 + hour * 3600 + minute * 60 + second;
    Some(UNIX_EPOCH + Duration::from_secs(secs) + Duration::from_millis(millis))
}

/// Days since the epoch to the proleptic Gregorian date (H. Hinnant's `civil_from_days`).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// The proleptic Gregorian date to days since the epoch (H. Hinnant's `days_from_civil`).
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let yoe = year - era * 400;
    let mp = (i64::from(month) + 9) % 12;
    let doy = (153 * mp + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
pub(crate) mod testing {
    //! What the tests of every file here share: a fresh audit directory, a clock they move, a socket standing in
    //! for the journal, and the log's lines.

    use super::*;
    use std::os::unix::net::UnixDatagram;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A path for a new audit directory; [`AuditLog::open`] makes it.
    pub(crate) fn audit_dir_for(label: &str) -> PathBuf {
        audit_dir(&agent::state_dirs::test_workspace_dir(label).join("remote"))
    }

    pub(crate) fn device(id: &str) -> DeviceRef {
        DeviceRef {
            id: id.into(),
            name: format!("phone {id}"),
        }
    }

    /// A `cmd` record without text.
    pub(crate) fn cmd(id: &str, n: u64) -> Event {
        Event::Cmd {
            device: device(id),
            kind: "interrupt".into(),
            instance: "i1".into(),
            key: "k1".into(),
            tab: 1,
            revision: n,
            command_id: format!("c{n}"),
            text: None,
        }
    }

    /// A clock a test sets, in milliseconds since the epoch.
    #[derive(Clone)]
    pub(crate) struct FakeClock(Arc<AtomicU64>);

    impl FakeClock {
        pub(crate) fn at_ms(ms: u64) -> FakeClock {
            FakeClock(Arc::new(AtomicU64::new(ms)))
        }
        pub(crate) fn set_ms(&self, ms: u64) {
            self.0.store(ms, Ordering::SeqCst);
        }
        pub(crate) fn clock(&self) -> Clock {
            let ms = self.0.clone();
            Arc::new(move || UNIX_EPOCH + Duration::from_millis(ms.load(Ordering::SeqCst)))
        }
    }

    /// Milliseconds in `days` days.
    pub(crate) const fn days_ms(days: u64) -> u64 {
        days * 86_400_000
    }

    /// An open log on a fake clock.
    pub(crate) fn open_at(
        dir: &Path,
        writer: WriterKind,
        keep_days: Option<u32>,
        journal: JournalTarget,
        clock: &FakeClock,
    ) -> AuditLog {
        AuditLog::open_with_clock(dir, writer, keep_days, journal, clock.clock()).unwrap()
    }

    /// The files of `dir`, oldest first, as `(name, lines)`.
    pub(crate) fn files(dir: &Path) -> Vec<(String, Vec<String>)> {
        let mut out = Vec::new();
        for n in (1..=AUDIT_ROTATED_FILES + 1).rev() {
            let name = rotated_name(n);
            if let Ok(text) = std::fs::read_to_string(dir.join(&name)) {
                out.push((name, text.lines().map(str::to_string).collect()));
            }
        }
        if let Ok(text) = std::fs::read_to_string(dir.join(CURRENT)) {
            out.push((CURRENT.to_string(), text.lines().map(str::to_string).collect()));
        }
        out
    }

    /// Every line of `dir`, oldest first, across its files.
    pub(crate) fn lines(dir: &Path) -> Vec<String> {
        files(dir).into_iter().flat_map(|(_, lines)| lines).collect()
    }

    /// The parsed JSON object of a line.
    pub(crate) fn object(line: &str) -> serde_json::Map<String, Value> {
        serde_json::from_str(line).unwrap_or_else(|e| panic!("{line:?} is not a JSON object: {e}"))
    }

    /// The hash a line's successor carries as `prev`.
    pub(crate) fn hash_of(line: &str) -> String {
        chain::line_hash(line.as_bytes())
    }

    /// Asserts that every line after the first carries the hash of the line before it.
    pub(crate) fn assert_chained(lines: &[String]) {
        for pair in lines.windows(2) {
            assert_eq!(
                object(&pair[1])["prev"].as_str().unwrap(),
                hash_of(&pair[0]),
                "the chain broke between\n{}\nand\n{}",
                pair[0],
                pair[1]
            );
        }
    }

    /// A datagram socket standing in for the journal.
    pub(crate) struct FakeJournal {
        socket: UnixDatagram,
        pub(crate) path: PathBuf,
    }

    impl FakeJournal {
        pub(crate) fn bind() -> FakeJournal {
            // A short label: the socket's path has to stay within the 103 bytes macOS allows.
            let path = agent::socket_path::in_dir(&agent::state_dirs::test_workspace_dir("j"), "j").unwrap();
            let socket = UnixDatagram::bind(&path).unwrap();
            socket.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            FakeJournal { socket, path }
        }

        pub(crate) fn target(&self) -> JournalTarget {
            JournalTarget::Socket(self.path.clone())
        }

        /// The next datagram's fields.
        pub(crate) fn next(&self) -> Vec<(String, Vec<u8>)> {
            let mut buf = vec![0u8; 64 * 1024];
            let n = self.socket.recv(&mut buf).expect("a journal datagram");
            journal::decode(&buf[..n]).expect("a well-formed native-protocol datagram")
        }

        /// Whether a datagram is waiting.
        pub(crate) fn is_empty(&self) -> bool {
            self.socket.set_nonblocking(true).unwrap();
            let mut buf = [0u8; 16];
            let empty = self.socket.recv(&mut buf).is_err();
            self.socket.set_nonblocking(false).unwrap();
            empty
        }

        /// The next datagram as the journal would show it, sent by `exe`.
        pub(crate) fn next_entry(&self, exe: &str) -> JournalEntry {
            let fields = self.next();
            let record = fields
                .iter()
                .find(|(k, _)| k == "EITRI_RECORD")
                .map(|(_, v)| String::from_utf8(v.clone()).unwrap());
            JournalEntry {
                exe: Some(exe.to_string()),
                record,
            }
        }
    }

    /// A journal that holds what a test gave it.
    pub(crate) struct Entries(pub(crate) Vec<JournalEntry>);

    impl JournalSource for Entries {
        fn entries(&self) -> io::Result<Vec<JournalEntry>> {
            Ok(self.0.clone())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn only_a_known_command_kind_is_a_kind() {
        for kind in COMMAND_KINDS {
            assert_eq!(command_kind(kind), Some(kind));
        }
        for word in [
            "",
            "Send",
            "send ",
            "approve",
            "my secret prompt",
            "permission_response",
        ] {
            assert_eq!(command_kind(word), None, "{word:?}");
        }
    }

    #[test]
    fn record_has_required_fields_and_chain() {
        let dir = audit_dir_for("audit-fields");
        let clock = FakeClock::at_ms(1_762_400_000_123);
        let mut log = open_at(&dir, WriterKind::Serve, None, JournalTarget::Off, &clock);
        log.record(cmd("d1", 7)).unwrap();
        clock.set_ms(1_762_400_000_124);
        log.record(Event::Disable { why: "desk".into() }).unwrap();
        let lines = lines(&dir);
        assert_eq!(lines.len(), 2);
        assert!(
            lines[0].starts_with(r#"{"v":1,"time":"2025-11-06T03:33:20.123Z","writer":"serve","run":""#),
            "the header leads, in its order: {}",
            lines[0]
        );
        let first = object(&lines[0]);
        assert_eq!(first["seq"], 1);
        assert_eq!(first["event"], "cmd");
        assert_eq!(first["run"].as_str().unwrap(), log.run());
        assert_eq!(log.run().len(), 16);
        assert!(log
            .run()
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()));
        for (key, want) in [
            ("device", "d1"),
            ("device_name", "phone d1"),
            ("kind", "interrupt"),
            ("instance", "i1"),
            ("key", "k1"),
            ("command_id", "c7"),
        ] {
            assert_eq!(first[key], want, "field {key}");
        }
        assert_eq!(first["tab"], 1);
        assert_eq!(first["revision"], 7);
        assert!(first.get("text_len").is_none(), "no text, no digest");
        let second = object(&lines[1]);
        assert_eq!(second["seq"], 2);
        assert_eq!(second["event"], "disable");
        assert_eq!(second["why"], "desk");
        assert_eq!(second["time"], "2025-11-06T03:33:20.124Z");
        assert_eq!(second["prev"].as_str().unwrap(), hash_of(&lines[0]));
    }

    #[test]
    fn first_record_prev_is_zeros() {
        let dir = audit_dir_for("audit-first");
        let mut log = AuditLog::open(&dir, WriterKind::Cli, None, JournalTarget::Off).unwrap();
        log.record(cmd("d1", 1)).unwrap();
        let lines = lines(&dir);
        assert_eq!(object(&lines[0])["prev"], "0".repeat(64));
        assert_eq!(object(&lines[0])["writer"], "cli");
    }

    #[test]
    fn file_is_0600_in_a_0700_dir() {
        let dir = audit_dir_for("audit-modes");
        let mut log = AuditLog::open(&dir, WriterKind::Instance("i-7".into()), None, JournalTarget::Off).unwrap();
        log.record(cmd("d1", 1)).unwrap();
        let mode = |p: &Path| std::fs::symlink_metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&dir.join(CURRENT)), 0o600);
        assert_eq!(mode(&dir.join(LOCK)), 0o600);
        // A file someone widened is narrowed again by the next record.
        std::fs::set_permissions(dir.join(CURRENT), std::fs::Permissions::from_mode(0o644)).unwrap();
        log.record(cmd("d1", 2)).unwrap();
        assert_eq!(mode(&dir.join(CURRENT)), 0o600);
        assert_eq!(object(&lines(&dir)[0])["writer"], "i-7");
    }

    #[test]
    fn record_over_4k_is_shortened_not_split() {
        let dir = audit_dir_for("audit-long");
        let mut log = AuditLog::open(&dir, WriterKind::Serve, None, JournalTarget::Off).unwrap();
        let long_name = "é".repeat(3000);
        log.record(Event::EnrollRefused {
            device: DeviceRef {
                id: "d1".into(),
                name: long_name.clone(),
            },
            recipe: "ssh".into(),
            host: "h".repeat(2500),
            peer: "127.0.0.1:5".into(),
            why: "\u{1}".repeat(2000),
        })
        .unwrap();
        log.record(cmd("d1", 2)).unwrap();
        let raw = std::fs::read(dir.join(CURRENT)).unwrap();
        let lines = lines(&dir);
        assert_eq!(lines.len(), 2, "one line per record");
        assert!(lines[0].len() < AUDIT_RECORD_MAX, "{} bytes", lines[0].len());
        assert!(
            lines[0].len() > AUDIT_RECORD_MAX - 64,
            "cut no more than needed: {}",
            lines[0].len()
        );
        assert_eq!(raw.iter().filter(|b| **b == b'\n').count(), 2);
        let first = object(&lines[0]);
        assert_eq!(first["device"], "d1", "short fields are kept");
        assert_eq!(first["recipe"], "ssh");
        assert_eq!(first["peer"], "127.0.0.1:5");
        let name = first["device_name"].as_str().unwrap();
        assert!(name.ends_with(ELLIPSIS) && name.len() < long_name.len());
        assert!(
            name.trim_end_matches(ELLIPSIS).chars().all(|c| c == 'é'),
            "cut on a character boundary"
        );
        assert_chained(&lines);
    }

    #[test]
    fn cmd_text_is_length_and_hash_only() {
        let dir = audit_dir_for("audit-text");
        let sentinel = "SENTINEL-PROMPT-7f3a: please delete the build directory";
        let mut log = AuditLog::open(&dir, WriterKind::Serve, None, JournalTarget::Off).unwrap();
        log.record(Event::Cmd {
            device: device("d1"),
            kind: "send".into(),
            instance: "i1".into(),
            key: "k1".into(),
            tab: 2,
            revision: 9,
            command_id: "c1".into(),
            text: Some(text_digest(sentinel)),
        })
        .unwrap();
        let raw = std::fs::read(dir.join(CURRENT)).unwrap();
        assert!(
            !raw.windows(8).any(|w| w == b"SENTINEL"),
            "the text never reaches the file"
        );
        let record = object(&lines(&dir)[0]);
        assert_eq!(record["text_len"], sentinel.len());
        assert_eq!(
            record["text_sha256"].as_str().unwrap(),
            chain::hex(&Sha256::digest(sentinel.as_bytes()))
        );
    }

    #[test]
    fn every_reason_has_its_own_word() {
        let words: std::collections::HashSet<_> = Reason::ALL.iter().map(|r| r.as_str()).collect();
        assert_eq!(words.len(), Reason::ALL.len());
        for reason in Reason::ALL {
            assert_eq!(Reason::parse(reason.as_str()), Some(reason));
        }
    }

    #[test]
    fn times_are_utc_with_milliseconds_and_read_back() {
        for ms in [0, 951_782_400_000, 1_762_400_000_123, 4_102_444_799_999] {
            let t = UNIX_EPOCH + Duration::from_millis(ms);
            let text = format_time(t);
            assert_eq!(parse_time(&text), Some(t), "{text}");
        }
        assert_eq!(
            format_time(UNIX_EPOCH + Duration::from_millis(951_782_400_000)),
            "2000-02-29T00:00:00.000Z"
        );
        for bad in [
            "2025-11-06T03:33:20Z",
            "2025-11-06 03:33:20.123Z",
            "2025-13-06T03:33:20.123Z",
            "x",
        ] {
            assert_eq!(parse_time(bad), None, "{bad}");
        }
    }

    #[test]
    fn run_ids_differ_per_open() {
        let dir = audit_dir_for("audit-runs");
        let a = AuditLog::open(&dir, WriterKind::Serve, None, JournalTarget::Off).unwrap();
        let b = AuditLog::open(&dir, WriterKind::Serve, None, JournalTarget::Off).unwrap();
        assert_ne!(a.run(), b.run());
    }

    #[cfg(not(feature = "remote-test-hooks"))]
    #[test]
    fn journal_override_needs_the_feature() {
        let asked = std::cell::Cell::new(false);
        let env = |_: &str| {
            asked.set(true);
            Some(OsString::from("/elsewhere/j"))
        };
        assert_eq!(
            journal_target_with(env, |_| true),
            JournalTarget::Socket(PathBuf::from(JOURNAL_SOCKET))
        );
        assert_eq!(journal_target_with(env, |_| false), JournalTarget::Off);
        assert!(!asked.get(), "a build without the feature never reads the variable");
    }

    #[cfg(feature = "remote-test-hooks")]
    #[test]
    fn journal_override_wins_with_the_feature() {
        let env = |name: &str| (name == JOURNAL_SOCKET_VAR).then(|| OsString::from("/elsewhere/j"));
        assert_eq!(
            journal_target_with(env, |_| true),
            JournalTarget::Socket(PathBuf::from("/elsewhere/j"))
        );
        assert_eq!(
            journal_target_with(|_| Some(OsString::new()), |_| false),
            JournalTarget::Off,
            "an empty variable counts as unset"
        );
        assert_eq!(
            journal_target_with(|_| None, |_| true),
            JournalTarget::Off,
            "a test build with no override never writes to the real journal"
        );
    }
}
