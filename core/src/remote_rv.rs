//! The `rv-<instance>.sock` socket's grammar: what a remote observer's server (`eitri remote serve`) sends an Eitri
//! process about its tabs and its devices, what comes back, the socket's path, and a client for it.
//!
//! One JSON object per line, each carrying the protocol version `v`. Inbound ([`RvInbound`]): `subscribe`,
//! `unsubscribe` and `more` name a tab by its row key and need no token, since what they read a process of this user
//! can already read from the agents' own transcripts; `act` carries a device id, that device's token and the remote
//! message for the tab, and is acted on only for a device of the store whose grant allows steering; `ping` keeps a
//! quiet connection open; the rest are `serve`'s errands about devices (an enrollment, a grant, the listen address,
//! a self-revocation, a push endpoint, who is connected). Outbound ([`RvOutbound`]), from the Eitri process: the
//! frames of a followed tab ([`frame_line`]), `result` for an `act` ([`result_line`]), `refused` for a line turned
//! down ([`refused_line`]), `pong`, and the answers to `serve`'s errands. A line longer than [`RV_MAX_LINE`] goes as
//! `chunk` lines of at most [`CHUNK_BYTES`] each, the piece's escapes counted ([`chunk_lines`]). A credential in a line is a
//! [`Secret`], whose `Debug` never shows it.
//!
//! The socket sits in the control directory beside `ag-<instance>.sock`, built through `agent::socket_path::in_dir`,
//! which refuses a path over the socket path limit. [`RvListener`] binds it with the control sockets' posture (the
//! directory checked, the socket 0600, a peer served only when its uid is this process's effective uid, checked before
//! a byte is read) and hands each such peer to its owner; [`RvClient`] checks the other way, that the socket is served
//! by this user, before it writes a byte. `serve`'s own socket, `remote.sock`, is named by [`serve_socket_path`].

use crate::panel_control::euid;
use crate::remote_devices::Grant;
pub use crate::remote_pairing::PairMode;
use crate::tab_set::{GoneWhy, ObserverFrame, RowKey, WorkMark};
use agent::private_fs::{remove_if_same, FileIdentity};
use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The only version of the `rv-` lines this build speaks.
pub const RV_PROTOCOL: u32 = 1;

/// The longest inbound line, newline not counted; a longer one closes its connection.
pub const RV_MAX_LINE: usize = 1024 * 1024;

/// How many connections a process serves at once; one more is refused.
pub const RV_MAX_CONNECTIONS: usize = 8;

/// The reason in the `refused` line (no key) a process sends a connection past [`RV_MAX_CONNECTIONS`] before it closes
/// it: an answer to the connection, not to anything sent on it.
pub const RV_REFUSED_FULL: &str = "too many connections";

/// How many tabs one connection may follow at once.
pub const RV_MAX_OBSERVERS_PER_CONNECTION: usize = 16;

/// How long the rest of a line may take once its first byte has come.
pub const RV_LINE_WITHIN: Duration = Duration::from_secs(5);

/// The longest outbound line [`RvClient::recv`] takes: a snapshot carries a whole transcript.
pub const RV_MAX_OUTBOUND_LINE: usize = 64 * 1024 * 1024;

/// The longest command id taken. A longer one is refused and never copied, not even into the answer, which names
/// no id then; a 128-bit id fits many times over.
pub const COMMAND_ID_MAX_BYTES: usize = 64;

/// The bytes of answers (`result`, `refused`, `pong` and the errands' answers) one connection may have waiting for
/// its writer; one more closes it.
pub const CONTROL_BYTES_MAX: usize = 64 * 1024;

/// How long a connection may go without sending a line before it is closed; `serve` pings well inside it.
pub const RV_IDLE: Duration = Duration::from_secs(120);

/// How long a writer that owes frames may go without writing a byte before its connection is closed.
pub const RV_WRITE_STALL: Duration = Duration::from_secs(120);

/// The piece size of a frame too long for one line, sent as `chunk` frames.
pub const CHUNK_BYTES: usize = 512 * 1024;

/// The name of `serve`'s own socket in the runtime directory.
const SERVE_SOCKET: &str = "remote.sock";

/// The name of `serve`'s control socket in the runtime directory.
const SERVE_CONTROL_SOCKET: &str = "remote-ctl.sock";

/// How long [`RvClient::connect`] waits for the server to accept.
const CONNECT_WITHIN: Duration = Duration::from_secs(2);

/// How many lowercase hex digits an instance id has.
const INSTANCE_HEX: usize = 16;

/// The pause after a failed `poll` or `accept` (descriptors exhausted, say), so the thread never spins.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(50);

/// `control_dir/rv-<instance>.sock`, through `agent::socket_path::in_dir`, which refuses a path over the socket path
/// limit. `instance` must be 16 lowercase hex digits, so no id read from elsewhere can name a path outside the
/// directory.
pub fn rv_socket_path(control_dir: &Path, instance: &str) -> io::Result<PathBuf> {
    let is_instance = instance.len() == INSTANCE_HEX
        && instance
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    if !is_instance {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{instance:?} is not an instance id ({INSTANCE_HEX} lowercase hex digits)"),
        ));
    }
    agent::socket_path::in_dir(control_dir, &format!("rv-{instance}.sock"))
}

/// `runtime_dir/remote.sock`, `eitri remote serve`'s socket, through `agent::socket_path::in_dir`, which refuses a path
/// over the socket path limit.
pub fn serve_socket_path(runtime_dir: &Path) -> io::Result<PathBuf> {
    agent::socket_path::in_dir(runtime_dir, SERVE_SOCKET)
}

/// `runtime_dir/remote-ctl.sock`, the socket `eitri remote add-device` and `grant` reach a window through (bound by
/// `serve` whatever it listens on for phones), through `agent::socket_path::in_dir`.
pub fn serve_control_socket_path(runtime_dir: &Path) -> io::Result<PathBuf> {
    agent::socket_path::in_dir(runtime_dir, SERVE_CONTROL_SOCKET)
}

/// A credential a line carries: a device's token, an enrollment code, a push endpoint (whose URL may be the only
/// thing that lets a sender reach the phone). Passed on and compared, never shown: its `Debug` is `<hidden>`, so no
/// log line or panic message holds it.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
    pub fn new(text: impl Into<String>) -> Secret {
        Secret(text.into())
    }

    /// The credential itself, for the one check that needs it.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<hidden>")
    }
}

impl From<&str> for Secret {
    fn from(text: &str) -> Secret {
        Secret(text.to_owned())
    }
}

impl From<String> for Secret {
    fn from(text: String) -> Secret {
        Secret(text)
    }
}

/// One phone connected to `serve`, as `serve` reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Connected {
    /// The device's id in the store.
    pub device: String,
    /// The `Host` the phone's request named.
    pub host: String,
    /// The address the connection came from.
    pub peer: String,
    /// When it connected, in seconds since the Unix epoch.
    pub since: u64,
}

/// One inbound line. Any other kind, and any field not named here, fails to parse.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RvInbound {
    /// Follow the tab `key`: a snapshot, then its events, until `unsubscribe` or `gone`.
    Subscribe { v: u32, key: String },
    /// Stop following the tab `key`.
    Unsubscribe { v: u32, key: String },
    /// The remote message `cmd` about the tab `key`, from `device`, which proves itself with `token`.
    Act {
        v: u32,
        device: String,
        token: Secret,
        key: String,
        cmd: serde_json::Value,
    },
    /// Answered with `pong`; any line keeps the connection open past [`RV_IDLE`].
    Ping { v: u32 },
    /// The turns of the followed tab `key` before `before_turn`, of the session `generation`, as a `page`.
    More {
        v: u32,
        key: String,
        generation: u64,
        before_turn: u64,
    },
    /// A phone's enrollment with the one-time `code`, as `name`, reaching `serve` through `host` from `peer` (`unix`
    /// for `serve`'s Unix socket), by the recipe the phone says it used.
    Enroll {
        v: u32,
        code: Secret,
        device_name: String,
        host: String,
        peer: String,
        recipe: PairMode,
    },
    /// `eitri remote add-device`: the window that has the focus starts an enrollment, by `recipe` when it names one
    /// (`--ssh`, `--direct`), else by the window's default.
    StartEnroll {
        v: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        recipe: Option<PairMode>,
    },
    /// `eitri remote grant`: the window asks its y/n before `device`'s grant becomes `grant`.
    GrantRequest { v: u32, device: String, grant: Grant },
    /// `remote.listen` names `addr`: the window asks its y/n before `serve` listens there.
    ListenRequest { v: u32, addr: String },
    /// The phone `device` takes itself out of the store, proving itself with `token`.
    RevokeSelf { v: u32, device: String, token: Secret },
    /// The phone `device` names where its notifications go, proving itself with `token`.
    SetPushEndpoint {
        v: u32,
        device: String,
        token: Secret,
        url: Secret,
    },
    /// `serve`'s report of the phones connected now.
    DevicesConnected { v: u32, devices: Vec<Connected> },
}

/// Why an inbound line was not read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RvLineError {
    /// Not one JSON object of a known kind with only the fields it has.
    Malformed,
    /// A `v` this build does not speak.
    Version(u64),
}

/// The inbound frame `line` says. The version is read first, so a newer peer's line is told its version rather than
/// that a field is unknown.
pub fn parse_rv_line(line: &[u8]) -> Result<RvInbound, RvLineError> {
    if line.len() > RV_MAX_LINE {
        return Err(RvLineError::Malformed);
    }
    let value: serde_json::Value = serde_json::from_slice(line).map_err(|_| RvLineError::Malformed)?;
    let version = value
        .get("v")
        .and_then(serde_json::Value::as_u64)
        .ok_or(RvLineError::Malformed)?;
    if version != u64::from(RV_PROTOCOL) {
        return Err(RvLineError::Version(version));
    }
    serde_json::from_value(value).map_err(|_| RvLineError::Malformed)
}

fn json_string(text: &str) -> String {
    serde_json::to_string(text).unwrap_or_else(|_| "\"\"".to_owned())
}

/// `"generation":g,"unseen":n` (`null` when the stretch is not counted yet): what a stop about the frame's work names.
fn work_fields(work: &WorkMark) -> String {
    let unseen = work.unseen.map_or_else(|| "null".to_owned(), |n| n.to_string());
    format!(r#""generation":{},"unseen":{unseen}"#, work.generation)
}

/// The outbound line for one observer frame, without the newline. The desk's envelope goes in `frame` as it was
/// serialized, never parsed again.
pub fn frame_line(frame: &ObserverFrame) -> String {
    match frame {
        ObserverFrame::Snapshot {
            key,
            through,
            work,
            first_turn,
            cut,
            json,
        } => format!(
            r#"{{"v":{RV_PROTOCOL},"kind":"snapshot","key":{},"through":{through},{},"first_turn":{first_turn},"cut":{},"frame":{json}}}"#,
            json_string(key.as_str()),
            work_fields(work),
            serde_json::to_string(cut).unwrap_or_else(|_| "[]".to_owned()),
        ),
        ObserverFrame::Page {
            key,
            mark,
            from_turn,
            through_turn,
            json,
        } => format!(
            r#"{{"v":{RV_PROTOCOL},"kind":"page","key":{},{},"from_turn":{from_turn},"through_turn":{through_turn},"frame":{json}}}"#,
            json_string(key.as_str()),
            work_fields(mark),
        ),
        ObserverFrame::Ended { key, how } => RvOutbound::Ended {
            v: RV_PROTOCOL,
            key: key.as_str().to_owned(),
            how: *how,
        }
        .line(),
        ObserverFrame::SendBlock { key, block } => RvOutbound::SendBlock {
            v: RV_PROTOCOL,
            key: key.as_str().to_owned(),
            block: *block,
        }
        .line(),
        ObserverFrame::Events {
            key,
            from,
            through,
            work,
            json,
        } => format!(
            r#"{{"v":{RV_PROTOCOL},"kind":"events","key":{},"from":{from},"through":{through},{},"frame":{json}}}"#,
            json_string(key.as_str()),
            work_fields(work),
        ),
        ObserverFrame::Gone { key, why } => {
            let why = match why {
                GoneWhy::Closed => "closed",
                GoneWhy::Moved => "moved",
            };
            format!(
                r#"{{"v":{RV_PROTOCOL},"kind":"gone","key":{},"why":"{why}"}}"#,
                json_string(key.as_str())
            )
        }
    }
}

/// Why sending to a followed tab is closed, as the phone shows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SendBlock {
    /// The tab runs in Bypass.
    Bypass,
    /// The tab's agent is not Claude.
    NotClaude,
    /// The tab holds no session yet.
    NotStarted,
    /// The device's grant is `watch`; `serve` sets this one, from the store.
    WatchOnly,
}

/// An entry a snapshot holds only the start of, as the frame names it: `entry_id` is the collection of the desk's
/// envelope and the entry's `seq` (`transcript:42`, `toolCalls:57`), `bytes` about how long the whole entry is. The
/// whole entry comes in the `page` that `more` with a `before_turn` past the entry's turn returns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CutEntry {
    pub entry_id: String,
    pub bytes: u64,
}

/// Why a `more` line was turned down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoreRefusal {
    /// The session it names is not the one the observer follows now (replaced, or not yet snapshotted).
    WorkChanged,
    /// A page is already owed to this observer; one at a time.
    Outstanding,
    /// No such turn in the session: `before_turn` is 0 or past the last turn.
    NoSuchTurn,
}

impl MoreRefusal {
    /// The `why` of the `refused` line.
    pub fn why(self) -> &'static str {
        match self {
            MoreRefusal::WorkChanged => "the session changed",
            MoreRefusal::Outstanding => "a page is on its way",
            MoreRefusal::NoSuchTurn => "no such turn",
        }
    }
}

/// How a followed tab's session ended without the tab closing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndedHow {
    /// The session failed.
    Failed,
    /// The session went away (its process ended).
    Lost,
}

/// One outbound line, as `serve` reads it. Any other kind, and any field not named here, fails to parse; every field
/// is written out, an empty one as `null`, and a line that leaves one out fails too (a `send_block` without its
/// `block` is not "may send"). A tab's frames carry the desk's own envelope in `frame`, which this side passes on and
/// never reads.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RvOutbound {
    /// The answer to `ping`.
    Pong { v: u32 },
    /// The followed tab's state through `through`, from turn `first_turn` on; `cut` names the entries it holds only
    /// the start of.
    Snapshot {
        v: u32,
        key: String,
        through: u64,
        generation: u64,
        #[serde(deserialize_with = "present")]
        unseen: Option<u64>,
        first_turn: u64,
        cut: Vec<CutEntry>,
        frame: serde_json::Value,
    },
    /// The followed tab's events after `from` through `through`.
    Events {
        v: u32,
        key: String,
        from: u64,
        through: u64,
        generation: u64,
        #[serde(deserialize_with = "present")]
        unseen: Option<u64>,
        frame: serde_json::Value,
    },
    /// Older turns, `from_turn` through `through_turn`, the answer to `more`.
    Page {
        v: u32,
        key: String,
        generation: u64,
        #[serde(deserialize_with = "present")]
        unseen: Option<u64>,
        from_turn: u64,
        through_turn: u64,
        frame: serde_json::Value,
    },
    /// Piece `i` of the line numbered `of`, which was too long to send whole; `last` on the final piece.
    Chunk {
        v: u32,
        of: u64,
        i: u32,
        last: bool,
        data: String,
    },
    /// The followed tab is no longer here; nothing follows for `key`.
    Gone { v: u32, key: String, why: GoneWhy },
    /// The followed tab's session ended; the tab is still here.
    Ended { v: u32, key: String, how: EndedHow },
    /// What a phone shows of a tab before its snapshot.
    Greeting {
        v: u32,
        key: String,
        greeting: serde_json::Value,
    },
    /// The window's colours changed.
    Theme { v: u32, theme: serde_json::Value },
    /// Whether, and why not, the phone may send to the tab `key` now.
    SendBlock {
        v: u32,
        key: String,
        #[serde(deserialize_with = "present")]
        block: Option<SendBlock>,
    },
    /// The answer to an `act`.
    Result {
        v: u32,
        #[serde(deserialize_with = "present")]
        command_id: Option<String>,
        ok: bool,
        #[serde(deserialize_with = "present")]
        why: Option<String>,
    },
    /// A subscription (`key`) or a line or connection (no `key`) turned down.
    Refused {
        v: u32,
        #[serde(deserialize_with = "present")]
        key: Option<String>,
        why: String,
    },
    /// The desk said yes to the enrollment: `device` is in the store and `token` goes to the phone, once.
    Enrolled { v: u32, device: String, token: Secret },
    /// The enrollment was turned down, or the desk said no.
    EnrollRefused { v: u32, why: String },
    /// The answer to `start_enroll`: the window shows a code whose phone pairs by `recipe` and dials `target`
    /// ([`crate::remote_pairing::Endpoint::describe`]).
    EnrollShown { v: u32, recipe: PairMode, target: String },
    /// The desk's answer about listening on `addr`.
    ListenAnswer { v: u32, addr: String, yes: bool },
}

impl RvOutbound {
    /// The line, without its newline.
    pub fn line(&self) -> String {
        // Every field is a string, a number, a bool or a JSON value, which always serialize.
        serde_json::to_string(self).unwrap_or_default()
    }
}

// Serde reads a missing `Option` field as `None` unless the field names its own reader; this one reads the same
// value, so a field left out fails as missing while an explicit `null` is still `None`.
fn present<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

/// The outbound frame `line` says, read as strictly as [`parse_rv_line`] reads an inbound one.
pub fn parse_rv_outbound(line: &[u8]) -> Result<RvOutbound, RvLineError> {
    if line.len() > RV_MAX_OUTBOUND_LINE {
        return Err(RvLineError::Malformed);
    }
    let value: serde_json::Value = serde_json::from_slice(line).map_err(|_| RvLineError::Malformed)?;
    let version = value
        .get("v")
        .and_then(serde_json::Value::as_u64)
        .ok_or(RvLineError::Malformed)?;
    if version != u64::from(RV_PROTOCOL) {
        return Err(RvLineError::Version(version));
    }
    serde_json::from_value(value).map_err(|_| RvLineError::Malformed)
}

/// The answer to an `act`: `command_id` as the message gave it (`null` when it gave none, or one longer than
/// [`COMMAND_ID_MAX_BYTES`], which is never copied), and why it did nothing.
pub fn result_line(command_id: Option<&str>, outcome: Result<(), &str>) -> String {
    RvOutbound::Result {
        v: RV_PROTOCOL,
        command_id: command_id
            .filter(|id| id.len() <= COMMAND_ID_MAX_BYTES)
            .map(str::to_owned),
        ok: outcome.is_ok(),
        why: outcome.err().map(str::to_owned),
    }
    .line()
}

/// What a byte of a string takes once JSON escapes it: a quote, a backslash or a control character more than itself.
fn json_escaped_byte(byte: u8) -> usize {
    match byte {
        b'"' | b'\\' | b'\n' | b'\r' | b'\t' | 0x08 | 0x0c => 2,
        0..=0x1f => 6,
        _ => 1,
    }
}

/// How many bytes `text` takes inside a JSON string, its escapes counted (its quotes not).
pub fn json_escaped_len(text: &str) -> usize {
    text.bytes().map(json_escaped_byte).sum()
}

/// What a `chunk` line takes beside its piece: the envelope, the largest numbers, and the newline.
const CHUNK_ENVELOPE_BYTES: usize = 128;

/// `line` as `chunk` lines numbered `of`, cut only between characters, so the pieces joined in order are `line` again.
/// Each piece is sized by what it takes once escaped into its `data`, so every chunk line, newline included, is at most
/// [`CHUNK_BYTES`] whatever the text holds. A line of at most [`RV_MAX_LINE`] is returned whole.
pub fn chunk_lines(of: u64, line: &str) -> Vec<String> {
    chunk_lines_over(of, line, RV_MAX_LINE)
}

/// [`chunk_lines`] for a line longer than `whole` bytes; one of at most `whole` is returned as it is.
pub fn chunk_lines_over(of: u64, line: &str, whole: usize) -> Vec<String> {
    if line.len() <= whole {
        return vec![line.to_owned()];
    }
    const PIECE: usize = CHUNK_BYTES - CHUNK_ENVELOPE_BYTES;
    let mut pieces = Vec::with_capacity(line.len() / PIECE + 1);
    let mut rest = line;
    while !rest.is_empty() {
        let mut escaped = 0;
        let mut end = 0;
        for &byte in rest.as_bytes() {
            let cost = json_escaped_byte(byte);
            if escaped + cost > PIECE {
                break;
            }
            escaped += cost;
            end += 1;
        }
        while !rest.is_char_boundary(end) {
            end -= 1;
        }
        pieces.push(&rest[..end]);
        rest = &rest[end..];
    }
    let last = pieces.len() - 1;
    pieces
        .into_iter()
        .enumerate()
        .map(|(i, data)| {
            RvOutbound::Chunk {
                v: RV_PROTOCOL,
                of,
                i: u32::try_from(i).unwrap_or(u32::MAX),
                last: i == last,
                data: data.to_owned(),
            }
            .line()
        })
        .collect()
}

/// A subscription (`key`) or a line or connection (`None`) turned down, and why.
pub fn refused_line(key: Option<&RowKey>, why: &str) -> String {
    RvOutbound::Refused {
        v: RV_PROTOCOL,
        key: key.map(|k| k.as_str().to_owned()),
        why: why.to_owned(),
    }
    .line()
}

/// The answer to `ping`.
pub fn pong_line() -> String {
    RvOutbound::Pong { v: RV_PROTOCOL }.line()
}

/// A bound `rv-` socket and the thread accepting on it.
///
/// Dropping it removes the socket only while the name still holds the one bound, and ends the accepting thread
/// without waiting for it; connections already handed on are their owner's.
pub struct RvListener {
    dir: std::fs::File,
    name: OsString,
    bound: FileIdentity,
    stop: Arc<AtomicBool>,
    /// The other end of the thread's wake pair: closing it wakes the thread's `poll`.
    stop_pair: Option<UnixStream>,
}

impl RvListener {
    /// Binds `rv-<instance>.sock` in `control_dir` and starts accepting. `serve` runs on the accepting thread with
    /// each peer whose uid is this process's effective uid, checked before a byte is read, as a blocking stream; a
    /// peer of another user is closed unanswered. Accepting stops when `serve` returns `false`.
    ///
    /// Fails, having created nothing, when the path would pass the socket path limit or `instance` is not an instance
    /// id; fails when the directory is not private or the name is taken.
    pub fn bind(
        control_dir: &Path,
        instance: &str,
        serve: impl FnMut(UnixStream) -> bool + Send + 'static,
    ) -> io::Result<RvListener> {
        bind_with(control_dir, instance, crate::panel_control::peer_uid, serve)
    }
}

impl Drop for RvListener {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        // Unlinked first, so no new peer finds it while the thread winds down.
        let _ = remove_if_same(&self.dir, &self.name, self.bound);
        self.stop_pair.take();
    }
}

type PeerUid = fn(&UnixStream) -> io::Result<u32>;

fn bind_with<S>(control_dir: &Path, instance: &str, peer_uid: PeerUid, serve: S) -> io::Result<RvListener>
where
    S: FnMut(UnixStream) -> bool + Send + 'static,
{
    let path = rv_socket_path(control_dir, instance)?;
    let name = path
        .file_name()
        .map(OsString::from)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "a socket path with no file name"))?;
    crate::panel_control::ensure_dir(control_dir)
        .map_err(|why| io::Error::new(io::ErrorKind::PermissionDenied, why))?;
    let dir = agent::private_fs::open_dir(control_dir)?;
    let listener = UnixListener::bind(&path)?;
    let meta = path.symlink_metadata()?;
    let bound = FileIdentity {
        dev: meta.dev(),
        ino: meta.ino(),
    };
    let started = (move || -> io::Result<(UnixStream, Arc<AtomicBool>)> {
        // `bind` leaves the umask's mode on the file.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        listener.set_nonblocking(true)?;
        let (stop_pair, stop_rx) = UnixStream::pair()?;
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        std::thread::Builder::new()
            .name("eitri-remote-accept".into())
            .spawn(move || accept_loop(&listener, &stop_rx, &thread_stop, peer_uid, serve))?;
        Ok((stop_pair, stop))
    })();
    match started {
        Ok((stop_pair, stop)) => Ok(RvListener {
            dir,
            name,
            bound,
            stop,
            stop_pair: Some(stop_pair),
        }),
        Err(e) => {
            let _ = remove_if_same(&dir, &name, bound);
            Err(e)
        }
    }
}

/// The accepting thread: wait for a peer or the stop, check the peer, hand it on.
fn accept_loop<S: FnMut(UnixStream) -> bool>(
    listener: &UnixListener,
    stop_rx: &UnixStream,
    stop: &AtomicBool,
    peer_uid: PeerUid,
    mut serve: S,
) {
    let me = euid();
    while !stop.load(Ordering::Acquire) {
        let mut fds = [
            libc::pollfd {
                fd: listener.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: stop_rx.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        // SAFETY: `fds` is a valid array of two initialized `pollfd`s whose descriptors stay open for the call; the
        // length passed is its length.
        let rc = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, -1) };
        if rc < 0 {
            if io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                std::thread::sleep(ACCEPT_BACKOFF);
            }
            continue;
        }
        // The stop end is never written: anything on it means the listener's owner dropped it.
        if fds[1].revents != 0 {
            return;
        }
        match listener.accept() {
            Ok((stream, _)) => {
                // Before a byte is read: a peer of another user is closed unanswered.
                match peer_uid(&stream) {
                    Ok(uid) if uid == me => {}
                    _ => continue,
                }
                // A socket accepted from a non-blocking listener inherits that flag on macOS and the BSDs.
                if stream.set_nonblocking(false).is_err() {
                    continue;
                }
                if stop.load(Ordering::Acquire) || !serve(stream) {
                    return;
                }
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
            Err(_) => std::thread::sleep(ACCEPT_BACKOFF),
        }
    }
}

/// Takes the first whole line out of `buf`, without its newline; `None` while none has ended. `scanned` is how much
/// of `buf` is known to hold no newline. A line longer than `cap`, ended or not, is `InvalidData`, however its bytes
/// were split across reads.
fn next_line(buf: &mut Vec<u8>, scanned: &mut usize, cap: usize) -> io::Result<Option<Vec<u8>>> {
    let too_long = || io::Error::new(io::ErrorKind::InvalidData, "line too long");
    if let Some(at) = buf[*scanned..].iter().position(|&b| b == b'\n') {
        let end = *scanned + at;
        if end > cap {
            return Err(too_long());
        }
        *scanned = 0;
        let mut line: Vec<u8> = buf.drain(..=end).collect();
        line.pop();
        return Ok(Some(line));
    }
    *scanned = buf.len();
    if buf.len() > cap {
        return Err(too_long());
    }
    Ok(None)
}

/// A connection to a process's `rv-` socket.
pub struct RvClient {
    stream: UnixStream,
    /// Bytes read past the last line handed out.
    buf: Vec<u8>,
    /// How much of `buf` is known to hold no newline, so a long line is searched once, not once per read.
    scanned: usize,
}

impl RvClient {
    /// Connects to the socket at `path`, waiting at most two seconds for the server to accept, and checks that it
    /// is served by this user before anything is written.
    pub fn connect(path: &Path) -> io::Result<RvClient> {
        let stream = crate::instance_dir::connect_by(path, Instant::now() + CONNECT_WITHIN)?;
        if crate::panel_control::peer_uid(&stream)? != euid() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("{} is served by another user", path.display()),
            ));
        }
        stream.set_write_timeout(Some(RV_LINE_WITHIN))?;
        Ok(RvClient {
            stream,
            buf: Vec::new(),
            scanned: 0,
        })
    }

    /// A second handle on the same connection, for a thread that writes lines while another receives; shutting it
    /// down ends a [`recv`](Self::recv) waiting on the other.
    pub fn writer(&self) -> io::Result<UnixStream> {
        self.stream.try_clone()
    }

    /// Writes `frame` as one line.
    pub fn send(&mut self, frame: &RvInbound) -> io::Result<()> {
        let mut line = serde_json::to_vec(frame).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        if line.len() > RV_MAX_LINE {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "line too long"));
        }
        line.push(b'\n');
        (&self.stream).write_all(&line)
    }

    /// The next line the server sent, without its newline, or `None` when none came within `timeout`. A server that
    /// closed is `UnexpectedEof`; a line over [`RV_MAX_OUTBOUND_LINE`] or not UTF-8 is `InvalidData`.
    pub fn recv(&mut self, timeout: Duration) -> io::Result<Option<String>> {
        let deadline = Instant::now() + timeout;
        let mut chunk = vec![0u8; 64 * 1024];
        loop {
            if let Some(line) = next_line(&mut self.buf, &mut self.scanned, RV_MAX_OUTBOUND_LINE)? {
                return String::from_utf8(line)
                    .map(Some)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e));
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(None);
            }
            // A zero timeout is an error to set, and means "no timeout" to some platforms.
            self.stream.set_read_timeout(Some(left.max(Duration::from_millis(1))))?;
            match (&self.stream).read(&mut chunk) {
                Ok(0) => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "the server closed")),
                Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
                Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {}
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    const INSTANCE: &str = "a3f09c1d2e4b5f60";

    #[test]
    fn remote_rv_act_debug_hides_the_token() {
        let act = RvInbound::Act {
            v: 1,
            device: "d1".into(),
            token: "s3cret-token".into(),
            key: "k".into(),
            cmd: serde_json::json!({}),
        };
        let shown = format!("{act:?}");
        assert!(!shown.contains("s3cret-token"), "{shown}");
        assert!(shown.contains("d1"), "{shown}");
    }

    #[test]
    fn rv_path_refuses_an_over_long_control_dir() {
        let short = Path::new("/run/user/1000/eitri-1000");
        let path = rv_socket_path(short, INSTANCE).expect("a short directory is under the limit");
        assert_eq!(path.parent(), Some(short));
        let name = path.file_name().and_then(|n| n.to_str()).unwrap();
        assert!(name.starts_with("rv-") && name.contains(INSTANCE), "{name}");
        let long = PathBuf::from(format!("/{}", "d".repeat(90)));
        let err = rv_socket_path(&long, INSTANCE).expect_err("the socket path would pass the platform limit");
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn rv_path_refuses_what_is_not_an_instance_id() {
        let dir = Path::new("/run/user/1000/eitri-1000");
        for bad in [
            "",
            "../../etc/passwd",
            "A3F09C1D2E4B5F60",
            "a3f09c1d2e4b5f6",
            "a3f09c1d2e4b5f600",
        ] {
            assert!(rv_socket_path(dir, bad).is_err(), "{bad:?}");
        }
    }

    /// A private directory standing in for the control directory, removed on drop.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Scratch {
            use std::os::unix::fs::DirBuilderExt;
            use std::sync::atomic::AtomicU32;
            static SERIAL: AtomicU32 = AtomicU32::new(0);
            let base = std::env::var_os("XDG_RUNTIME_DIR")
                .map(PathBuf::from)
                .filter(|p| p.is_absolute())
                .unwrap_or_else(std::env::temp_dir);
            let path = base.join(format!(
                "rr-{}-{}",
                std::process::id(),
                SERIAL.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
            Scratch(path)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn remote_rv_listener_hands_on_only_peers_of_this_user() {
        let scratch = Scratch::new();
        let (tx, rx) = std::sync::mpsc::channel();
        let listener = bind_with(
            &scratch.0,
            INSTANCE,
            |_| Ok(u32::MAX),
            move |stream| tx.send(stream).is_ok(),
        )
        .expect("the listener binds");
        let path = rv_socket_path(&scratch.0, INSTANCE).unwrap();
        let mode = std::fs::symlink_metadata(&path).unwrap().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let mut peer = UnixStream::connect(&path).unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
        let mut byte = [0u8; 1];
        assert_eq!(
            peer.read(&mut byte).unwrap(),
            0,
            "another user's peer is closed unanswered"
        );
        assert!(rx.try_recv().is_err(), "and never handed on");
        drop(listener);
        assert!(
            std::fs::symlink_metadata(&path).is_err(),
            "dropping it removes the socket"
        );

        let (tx, rx) = std::sync::mpsc::channel();
        let _listener = RvListener::bind(&scratch.0, INSTANCE, move |stream| tx.send(stream).is_ok()).unwrap();
        let _peer = RvClient::connect(&path).expect("this user's peer connects");
        assert!(rx.recv_timeout(Duration::from_secs(20)).is_ok(), "and is handed on");
    }

    #[test]
    fn remote_rv_lines_parse_strictly() {
        let sub = br#"{"v":1,"kind":"subscribe","key":"k-0123456789abcdef"}"#;
        assert_eq!(
            parse_rv_line(sub),
            Ok(RvInbound::Subscribe {
                v: 1,
                key: "k-0123456789abcdef".into()
            })
        );
        assert_eq!(
            parse_rv_line(br#"{"v":2,"kind":"subscribe","key":"k"}"#),
            Err(RvLineError::Version(2))
        );
        for bad in [
            &br#"{"kind":"subscribe","key":"k"}"#[..],
            br#"{"v":1,"kind":"subscribe","key":"k","extra":0}"#,
            br#"{"v":1,"kind":"answer","key":"k"}"#,
            br#"{"v":1,"kind":"act","device":"d","key":"k","cmd":{}}"#,
            br#"not json"#,
            br#"[1]"#,
        ] {
            assert_eq!(
                parse_rv_line(bad),
                Err(RvLineError::Malformed),
                "{}",
                String::from_utf8_lossy(bad)
            );
        }
        let act = br#"{"v":1,"kind":"act","device":"d1","token":"t","key":"k","cmd":{"type":"interrupt"}}"#;
        assert!(matches!(parse_rv_line(act), Ok(RvInbound::Act { .. })));
    }

    #[test]
    fn remote_rv_frame_lines_carry_the_version_and_the_frame_verbatim() {
        let key = RowKey::parse("k-0123456789abcdef").unwrap();
        let snapshot = ObserverFrame::Snapshot {
            key: key.clone(),
            through: 4,
            work: WorkMark {
                generation: 2,
                unseen: None,
            },
            first_turn: 7,
            cut: vec![CutEntry {
                entry_id: "transcript:42".into(),
                bytes: 10_000_000,
            }],
            json: Arc::from(r#"{"kind":"snapshot","tab":1}"#),
        };
        let line: serde_json::Value = serde_json::from_str(&frame_line(&snapshot)).unwrap();
        assert_eq!(line["v"], 1);
        assert_eq!(line["kind"], "snapshot");
        assert_eq!(line["key"], "k-0123456789abcdef");
        assert_eq!(line["through"], 4);
        assert_eq!(line["generation"], 2);
        assert!(line["unseen"].is_null(), "a stretch not counted yet is named by none");
        assert_eq!(line["frame"]["kind"], "snapshot");
        assert_eq!(line["first_turn"], 7);
        assert_eq!(
            line["cut"],
            serde_json::json!([{"entry_id": "transcript:42", "bytes": 10_000_000}])
        );
        let events = ObserverFrame::Events {
            key: key.clone(),
            from: 4,
            through: 6,
            work: WorkMark {
                generation: 2,
                unseen: Some(3),
            },
            json: Arc::from(r#"{"kind":"events"}"#),
        };
        let line: serde_json::Value = serde_json::from_str(&frame_line(&events)).unwrap();
        assert_eq!(
            (line["kind"].as_str(), line["from"].as_u64(), line["through"].as_u64()),
            (Some("events"), Some(4), Some(6))
        );
        assert_eq!(
            (line["generation"].as_u64(), line["unseen"].as_u64()),
            (Some(2), Some(3))
        );
        let gone = ObserverFrame::Gone {
            key,
            why: GoneWhy::Moved,
        };
        let line: serde_json::Value = serde_json::from_str(&frame_line(&gone)).unwrap();
        assert_eq!(
            (line["kind"].as_str(), line["why"].as_str()),
            (Some("gone"), Some("moved"))
        );

        let result: serde_json::Value = serde_json::from_str(&result_line(Some("c1"), Err("watch only"))).unwrap();
        assert_eq!(
            result,
            serde_json::json!({"v":1,"kind":"result","command_id":"c1","ok":false,"why":"watch only"})
        );
        let result: serde_json::Value = serde_json::from_str(&result_line(None, Ok(()))).unwrap();
        assert_eq!(
            result,
            serde_json::json!({"v":1,"kind":"result","command_id":null,"ok":true,"why":null})
        );
    }

    #[test]
    fn serve_socket_path_refuses_an_over_long_dir() {
        let short = Path::new("/run/user/1000");
        assert_eq!(
            serve_socket_path(short).expect("a short directory is under the limit"),
            short.join(SERVE_SOCKET)
        );
        let long = PathBuf::from(format!("/{}", "d".repeat(100)));
        let err = serve_socket_path(&long).expect_err("the socket path would pass the platform limit");
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn every_new_rv_kind_parses_strictly() {
        let lines = [
            r#"{"v":1,"kind":"ping"}"#,
            r#"{"v":1,"kind":"more","key":"k-1","generation":2,"before_turn":40}"#,
            r#"{"v":1,"kind":"enroll","code":"c0de","device_name":"Pixel","host":"127.0.0.1:7341","peer":"127.0.0.1:5000","recipe":"ssh"}"#,
            r#"{"v":1,"kind":"start_enroll"}"#,
            r#"{"v":1,"kind":"start_enroll","recipe":"direct"}"#,
            r#"{"v":1,"kind":"grant_request","device":"d1","grant":"watch+steer"}"#,
            r#"{"v":1,"kind":"listen_request","addr":"192.0.2.10:7341"}"#,
            r#"{"v":1,"kind":"revoke_self","device":"d1","token":"tok"}"#,
            r#"{"v":1,"kind":"set_push_endpoint","device":"d1","token":"tok","url":"https://ntfy.example/t"}"#,
            r#"{"v":1,"kind":"devices_connected","devices":[{"device":"d1","host":"h","peer":"p","since":7}]}"#,
        ];
        for line in lines {
            let parsed = parse_rv_line(line.as_bytes()).unwrap_or_else(|e| panic!("{line}: {e:?}"));
            assert_eq!(
                serde_json::to_value(&parsed).unwrap(),
                serde_json::from_str::<serde_json::Value>(line).unwrap(),
                "{line} reads back as it was written"
            );
            let mut extra: serde_json::Value = serde_json::from_str(line).unwrap();
            extra["x"] = serde_json::json!(1);
            assert_eq!(
                parse_rv_line(extra.to_string().as_bytes()),
                Err(RvLineError::Malformed),
                "an unknown field: {extra}"
            );
        }
        let nested =
            r#"{"v":1,"kind":"devices_connected","devices":[{"device":"d1","host":"h","peer":"p","since":7,"x":1}]}"#;
        assert_eq!(parse_rv_line(nested.as_bytes()), Err(RvLineError::Malformed));
        let grant = r#"{"v":1,"kind":"grant_request","device":"d1","grant":"admin"}"#;
        assert_eq!(parse_rv_line(grant.as_bytes()), Err(RvLineError::Malformed));
        let without_token = r#"{"v":1,"kind":"revoke_self","device":"d1"}"#;
        assert_eq!(parse_rv_line(without_token.as_bytes()), Err(RvLineError::Malformed));

        for line in [lines[2], lines[6], lines[7]] {
            let shown = format!("{:?}", parse_rv_line(line.as_bytes()).unwrap());
            assert!(!shown.contains("c0de") && !shown.contains("tok\""), "{shown}");
            assert!(
                !shown.contains("ntfy.example"),
                "the push endpoint is never shown: {shown}"
            );
        }
    }

    #[test]
    fn rv_outbound_lines_parse_back_strictly() {
        let key = RowKey::parse("k-0123456789abcdef").unwrap();
        let work = WorkMark {
            generation: 2,
            unseen: Some(1),
        };
        let frames = [
            ObserverFrame::Snapshot {
                key: key.clone(),
                through: 4,
                work,
                first_turn: 0,
                cut: Vec::new(),
                json: Arc::from(r#"{"kind":"snapshot"}"#),
            },
            ObserverFrame::Page {
                key: key.clone(),
                mark: work,
                from_turn: 3,
                through_turn: 9,
                json: Arc::from(r#"{"kind":"snapshot"}"#),
            },
            ObserverFrame::Ended {
                key: key.clone(),
                how: EndedHow::Lost,
            },
            ObserverFrame::SendBlock {
                key: key.clone(),
                block: None,
            },
            ObserverFrame::SendBlock {
                key: key.clone(),
                block: Some(SendBlock::Bypass),
            },
            ObserverFrame::Events {
                key: key.clone(),
                from: 4,
                through: 6,
                work,
                json: Arc::from(r#"{"kind":"events"}"#),
            },
            ObserverFrame::Gone {
                key: key.clone(),
                why: GoneWhy::Closed,
            },
        ];
        let mut lines: Vec<String> = frames.iter().map(frame_line).collect();
        lines.push(result_line(Some("c1"), Err("not yet")));
        lines.push(refused_line(Some(&key), "not yet"));
        lines.push(pong_line());
        for outbound in [
            RvOutbound::Chunk {
                v: 1,
                of: 3,
                i: 0,
                last: false,
                data: "abc".into(),
            },
            RvOutbound::Ended {
                v: 1,
                key: key.to_string(),
                how: EndedHow::Failed,
            },
            RvOutbound::SendBlock {
                v: 1,
                key: key.to_string(),
                block: Some(SendBlock::WatchOnly),
            },
            RvOutbound::Enrolled {
                v: 1,
                device: "d1".into(),
                token: "t0ken".into(),
            },
            RvOutbound::EnrollRefused {
                v: 1,
                why: "expired".into(),
            },
            RvOutbound::ListenAnswer {
                v: 1,
                addr: "192.0.2.10:7341".into(),
                yes: false,
            },
        ] {
            lines.push(outbound.line());
        }
        for line in &lines {
            let parsed = parse_rv_outbound(line.as_bytes()).unwrap_or_else(|e| panic!("{line}: {e:?}"));
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&parsed.line()).unwrap(),
                serde_json::from_str::<serde_json::Value>(line).unwrap(),
                "{line} reads back as it was written"
            );
            let mut extra: serde_json::Value = serde_json::from_str(line).unwrap();
            extra["x"] = serde_json::json!(1);
            assert_eq!(
                parse_rv_outbound(extra.to_string().as_bytes()),
                Err(RvLineError::Malformed)
            );
        }
        let enrolled = parse_rv_outbound(lines.iter().find(|l| l.contains("enrolled")).unwrap().as_bytes()).unwrap();
        assert!(!format!("{enrolled:?}").contains("t0ken"), "the token is never shown");
        assert_eq!(
            parse_rv_outbound(br#"{"v":2,"kind":"pong"}"#),
            Err(RvLineError::Version(2))
        );
    }

    #[test]
    fn rv_outbound_optional_fields_must_be_written_out() {
        let lines = [
            (
                r#"{"v":1,"kind":"snapshot","key":"k-1","through":4,"generation":2,"unseen":null,"first_turn":0,"cut":[],"frame":{}}"#,
                "unseen",
            ),
            (
                r#"{"v":1,"kind":"snapshot","key":"k-1","through":4,"generation":2,"unseen":null,"first_turn":0,"cut":[],"frame":{}}"#,
                "first_turn",
            ),
            (
                r#"{"v":1,"kind":"snapshot","key":"k-1","through":4,"generation":2,"unseen":null,"first_turn":0,"cut":[],"frame":{}}"#,
                "cut",
            ),
            (
                r#"{"v":1,"kind":"events","key":"k-1","from":4,"through":6,"generation":2,"unseen":1,"frame":{}}"#,
                "unseen",
            ),
            (
                r#"{"v":1,"kind":"page","key":"k-1","generation":2,"unseen":null,"from_turn":1,"through_turn":3,"frame":{}}"#,
                "unseen",
            ),
            (r#"{"v":1,"kind":"send_block","key":"k-1","block":null}"#, "block"),
            (
                r#"{"v":1,"kind":"result","command_id":"c1","ok":false,"why":"not yet"}"#,
                "command_id",
            ),
            (
                r#"{"v":1,"kind":"result","command_id":null,"ok":true,"why":null}"#,
                "why",
            ),
            (r#"{"v":1,"kind":"refused","key":null,"why":"not yet"}"#, "key"),
        ];
        for (line, field) in lines {
            assert!(parse_rv_outbound(line.as_bytes()).is_ok(), "{line}");
            let mut without: serde_json::Value = serde_json::from_str(line).unwrap();
            without.as_object_mut().unwrap().remove(field);
            assert_eq!(
                parse_rv_outbound(without.to_string().as_bytes()),
                Err(RvLineError::Malformed),
                "a line leaving out `{field}` is not read as its empty value: {without}"
            );
        }
    }

    #[test]
    fn a_line_over_the_cap_goes_as_chunks_that_join_to_it() {
        let short = r#"{"v":1,"kind":"pong"}"#;
        assert_eq!(chunk_lines(1, short), [short]);
        // Multi-byte characters across every piece boundary: no piece is cut inside one.
        let text = "é".repeat(RV_MAX_LINE) + "z";
        let line = format!(r#"{{"v":1,"kind":"events","x":"{text}"}}"#);
        let lines = chunk_lines(9, &line);
        assert!(lines.len() > 2);
        let mut joined = String::new();
        for (n, piece) in lines.iter().enumerate() {
            let RvOutbound::Chunk { of, i, last, data, .. } = parse_rv_outbound(piece.as_bytes()).unwrap() else {
                panic!("a chunk: {piece:.80}");
            };
            assert_eq!((of, i as usize, last), (9, n, n == lines.len() - 1));
            assert!(data.len() <= CHUNK_BYTES);
            assert!(piece.len() < CHUNK_BYTES, "{} bytes", piece.len());
            joined.push_str(&data);
        }
        assert_eq!(joined, line, "the pieces in order are the line");
    }

    #[test]
    fn chunk_lines_stay_under_the_piece_size_once_escaped() {
        // Every byte of these doubles (or more) when the piece is written as a JSON string.
        for fill in ["\\", "\"", "\u{1}"] {
            let line = format!(r#"{{"v":1,"kind":"page","x":"{}"}}"#, fill.repeat(RV_MAX_LINE + 1));
            let lines = chunk_lines(3, &line);
            assert!(lines.len() > 2, "{fill:?}");
            let mut joined = String::new();
            for piece in &lines {
                assert!(
                    piece.len() < CHUNK_BYTES && piece.len() < RV_MAX_LINE,
                    "a chunk line of {} bytes for {fill:?}",
                    piece.len()
                );
                let RvOutbound::Chunk { data, .. } = parse_rv_outbound(piece.as_bytes()).unwrap() else {
                    panic!("a chunk");
                };
                joined.push_str(&data);
            }
            assert_eq!(joined, line, "the pieces in order are the line");
        }
        assert_eq!(json_escaped_len("a\"\\\n\u{1}é"), 1 + 2 + 2 + 2 + 6 + 2);
    }

    #[test]
    fn result_line_never_copies_an_oversized_command_id() {
        let long = "c".repeat(COMMAND_ID_MAX_BYTES + 1);
        let line: serde_json::Value = serde_json::from_str(&result_line(Some(&long), Err("malformed"))).unwrap();
        assert!(line["command_id"].is_null(), "{line}");
        let fits = "c".repeat(COMMAND_ID_MAX_BYTES);
        let line: serde_json::Value = serde_json::from_str(&result_line(Some(&fits), Ok(()))).unwrap();
        assert_eq!(line["command_id"], fits.as_str());
    }

    #[test]
    fn a_received_line_over_the_cap_is_refused_however_its_bytes_arrive() {
        const CAP: usize = 8;
        let mut buf = Vec::new();
        let mut scanned = 0;
        buf.extend_from_slice(b"12345678");
        assert!(
            next_line(&mut buf, &mut scanned, CAP).unwrap().is_none(),
            "a full line may still end"
        );
        buf.extend_from_slice(b"9\n");
        let error = next_line(&mut buf, &mut scanned, CAP).unwrap_err();
        assert_eq!(
            error.kind(),
            io::ErrorKind::InvalidData,
            "the newline came one byte past the cap"
        );

        let (mut buf, mut scanned) = (b"123456789\nab".to_vec(), 0);
        let error = next_line(&mut buf, &mut scanned, CAP).unwrap_err();
        assert_eq!(
            error.kind(),
            io::ErrorKind::InvalidData,
            "a long line read in one piece"
        );

        let (mut buf, mut scanned) = (b"12345678\nab".to_vec(), 0);
        assert_eq!(
            next_line(&mut buf, &mut scanned, CAP).unwrap().as_deref(),
            Some(&b"12345678"[..]),
            "a line of exactly the cap is taken"
        );
        assert_eq!(buf, b"ab", "what follows it waits for the next call");
        assert!(next_line(&mut buf, &mut scanned, CAP).unwrap().is_none());
        buf.extend_from_slice(b"123456789");
        let error = next_line(&mut buf, &mut scanned, CAP).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData, "no newline and past the cap");
    }
}
