//! The vocabulary of the listener: what its threads and the main context say to each other.
//!
//! Three threads: the worker, which holds every socket and answers frames; the judge, which does what may touch the
//! disk or cost a whole text's copy, in order; the main context, which alone speaks to nvim. Messages go worker to main
//! as [`Event`], main to worker as [`Command`], and to the judge and back as [`Job`] and [`Done`].

use std::os::fd::RawFd;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::editor_context::compose::EditorContext;
use crate::editor_context::feed::PushFacts;
use crate::editor_tools::tools::diagnostics::DiagnosticsAnswer;

use super::lines::LineShape;

/// The variable a `claude` reads to know which lock file is its own.
pub const PORT_VAR: &str = "CLAUDE_CODE_SSE_PORT";
/// The lock's `ideName` and the `serverInfo.name` of `initialize`.
pub const IDE_NAME: &str = "Eitri";

pub type ConnId = u64;

/// A JSON-RPC id. A client gets back the id it sent, of the type it sent.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RpcId {
    Num(i64),
    Text(String),
}

impl RpcId {
    pub fn to_json(&self) -> serde_json::Value {
        match self {
            RpcId::Num(number) => (*number).into(),
            RpcId::Text(text) => text.as_str().into(),
        }
    }

    /// `None` for anything but an integer or a string: a float, a `null` and the rest name no request.
    pub fn from_json(value: &serde_json::Value) -> Option<RpcId> {
        match value {
            serde_json::Value::Number(number) => number.as_i64().map(RpcId::Num),
            serde_json::Value::String(text) => Some(RpcId::Text(text.clone())),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub root: PathBuf,
    /// Taken as a value, so nothing under test falls back to the process's own `HOME`.
    pub home: Option<PathBuf>,
}

/// The listener's bounds. `Default` is production; a test shortens a time or sets a small send buffer.
#[derive(Debug, Clone)]
pub struct Limits {
    /// A connection that has not finished its upgrade by then is closed.
    pub handshake_within: Duration,
    /// The most an upgrade's head may hold.
    pub handshake_bytes: usize,
    /// Upgraded connections at once.
    pub authed: usize,
    /// Connections at once still in their upgrade, counted apart from those.
    pub unauthed: usize,
    /// The largest message kept and parsed. Past it a message's id is taken from its tail and answered with an error.
    pub message_bytes: usize,
    /// The most of one oversized message read and thrown away before the connection closes.
    pub discard_bytes: usize,
    /// Bytes queued for one client that has stopped reading.
    pub outbound_bytes: usize,
    /// The client's own cap on a frame it accepts.
    pub answer_frame_bytes: usize,
    /// Jobs out at the judge for one connection.
    pub jobs_per_conn: usize,
    /// `SO_SNDBUF` on accepted sockets; `None` leaves the system's.
    pub send_buffer: Option<usize>,
}

impl Default for Limits {
    fn default() -> Self {
        const KIB: usize = 1024;
        const MIB: usize = 1024 * KIB;
        Limits {
            handshake_within: Duration::from_secs(5),
            handshake_bytes: 8 * KIB,
            authed: 8,
            unauthed: 8,
            message_bytes: 8 * MIB,
            discard_bytes: 64 * MIB,
            outbound_bytes: 8 * MIB,
            answer_frame_bytes: 16 * MIB - 1,
            jobs_per_conn: 16,
            send_buffer: None,
        }
    }
}

/// `openDiff`'s arguments, as the CLI sent them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenDiffArgs {
    pub new_file_path: String,
    pub new_file_contents: String,
    pub tab_name: String,
}

/// Prebuilt on the judge: `send_open` moves the values into its arguments, so no large text is cut on the GTK thread.
#[derive(Debug, Clone, PartialEq)]
pub struct DiffRequest {
    pub tab_name: String,
    /// Resolved: the name the syntax comes from.
    pub real_path: PathBuf,
    /// Project-relative: what the buffers and winbars name.
    pub rel: String,
    /// The file as the CLI reads it: LF, empty for a new file.
    pub file_text: Vec<u8>,
    /// `file_text`'s lines, each `Value::Binary`.
    pub file_lines: Vec<rmpv::Value>,
    /// The proposal's lines as sent.
    pub proposal_lines: Vec<rmpv::Value>,
    /// The proposal's; `join_values` gives its bytes back.
    pub shape: LineShape,
}

/// What `getDiagnostics` asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiagnosticsQuery {
    One { uri: String, path: PathBuf },
    All,
}

/// How a start ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Started {
    Attached { port: u16 },
    NotAttached { why: String },
}

/// One `:EitriMention`, 0-based.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MentionLine {
    pub file: String,
    pub line_start: u32,
    pub line_end: u32,
}

/// Worker to main context.
#[derive(Debug)]
pub enum Event {
    /// Always the first event, exactly once.
    Started(Started),
    ClientGone {
        conn: ConnId,
    },
    Diagnostics {
        conn: ConnId,
        id: RpcId,
        query: DiagnosticsQuery,
    },
    OpenDiff {
        conn: ConnId,
        id: RpcId,
        diff: Box<DiffRequest>,
    },
    CloseTab {
        conn: ConnId,
        id: RpcId,
        tab_name: String,
    },
    CloseAll {
        conn: ConnId,
        id: RpcId,
    },
    Cancelled {
        conn: ConnId,
        id: RpcId,
    },
    /// Bound: define `:EitriMention`.
    MentionUp {
        socket: PathBuf,
    },
    /// The last client went: remove it.
    MentionDown,
    /// Say so in the message area.
    MentionOutside,
    /// The worker ended without being told to: no listener any more.
    Lost,
}

/// What answers a `tools/call`. There is no variant for an answer that approves on closing: the CLI reads a closed tab
/// as approval, and a person closing a view has approved nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolAnswer {
    /// One empty text content.
    Empty,
    /// One text content.
    DiagnosticsText(String),
    /// Two text contents: "FILE_SAVED", the file's text.
    FileSaved(String),
    /// One text content: "DIFF_REJECTED".
    DiffRejected,
    /// A JSON-RPC error, -32000.
    Error(String),
}

/// One update of the editor-context feed, as the listener's pushes need it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextUpdate {
    pub context: Option<EditorContext>,
    pub facts: PushFacts,
}

/// Main context to worker.
#[derive(Debug)]
pub enum Command {
    Answer {
        conn: ConnId,
        id: RpcId,
        answer: ToolAnswer,
    },
    Entries {
        conn: ConnId,
        id: RpcId,
        named_uri: Option<String>,
        answer: DiagnosticsAnswer,
    },
    Saved {
        conn: ConnId,
        id: RpcId,
        lines: Vec<rmpv::Value>,
        shape: LineShape,
    },
    Context(ContextUpdate),
    Stop,
}

/// Worker to judge. Ordered: the judge runs one at a time.
#[derive(Debug)]
pub enum Job {
    Uri {
        conn: ConnId,
        id: RpcId,
        uri: String,
    },
    Diff {
        conn: ConnId,
        id: RpcId,
        args: Box<OpenDiffArgs>,
    },
    Entries {
        conn: ConnId,
        id: RpcId,
        named_uri: Option<String>,
        answer: DiagnosticsAnswer,
    },
    Join {
        conn: ConnId,
        id: RpcId,
        lines: Vec<rmpv::Value>,
        shape: LineShape,
    },
    /// `CloseTab`, `CloseAll`: comes back unchanged, in its place behind the `Diff` before it.
    Pass(Event),
    Context(ContextUpdate),
    Mention(MentionLine),
}

/// Judge to worker.
#[derive(Debug)]
pub enum Done {
    Answer {
        conn: ConnId,
        id: RpcId,
        answer: ToolAnswer,
    },
    Host(Event),
    /// `selection_changed`'s params.
    Selection(serde_json::Value),
    /// `at_mentioned`'s params.
    Mention(serde_json::Value),
}

/// The lock file's life, on the worker's thread.
pub trait Housekeeping: Send + 'static {
    /// Directories, sweep, publish. `Err(why)`: no lock, so no listener.
    fn start(&mut self, port: u16, token: &str) -> Result<(), String>;
    /// Readable when `on_wake` has something to look at.
    fn watch_fd(&self) -> Option<RawFd>;
    fn on_wake(&mut self, now: Instant);
    /// When `on_wake` is owed with nothing readable (the once-a-second bound, a re-walk).
    fn retry_in(&self, now: Instant) -> Option<Duration>;
    /// Stops watching, then removes the file only when it is still the one written.
    fn finish(&mut self);
}

/// The ordered mention path, on the worker's thread. Bound only while a client is connected.
pub trait MentionPort: Send + 'static {
    /// `None`: no socket, so no `:EitriMention`.
    fn open(&mut self) -> Option<PathBuf>;
    fn fds(&self) -> Vec<RawFd>;
    /// Lines in accept order; never one ahead of an older connection still being read.
    fn poll(&mut self, now: Instant) -> Vec<MentionLine>;
    fn next_deadline(&self) -> Option<Instant>;
    fn close(&mut self);
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn an_rpc_id_round_trips_and_a_float_or_null_is_none() {
        assert_eq!(RpcId::Num(7).to_json(), json!(7));
        assert_eq!(RpcId::Text("7".into()).to_json(), json!("7"));
        for id in [
            RpcId::Num(7),
            RpcId::Num(0),
            RpcId::Num(-1),
            RpcId::Num(i64::MAX),
            RpcId::Text("7".into()),
            RpcId::Text(String::new()),
        ] {
            assert_eq!(RpcId::from_json(&id.to_json()), Some(id));
        }
        for value in [
            json!(null),
            json!(1.5),
            json!(1.0),
            json!(u64::MAX),
            json!(true),
            json!([1]),
            json!({"id": 1}),
        ] {
            assert_eq!(RpcId::from_json(&value), None, "{value}");
        }
    }

    #[test]
    fn the_default_limits_are_the_stated_ones() {
        // Taken apart with no `..`, so a new bound cannot arrive without a line here.
        let Limits {
            handshake_within,
            handshake_bytes,
            authed,
            unauthed,
            message_bytes,
            discard_bytes,
            outbound_bytes,
            answer_frame_bytes,
            jobs_per_conn,
            send_buffer,
        } = Limits::default();
        assert_eq!(handshake_within, Duration::from_secs(5));
        assert_eq!(handshake_bytes, 8 * 1024);
        assert_eq!(authed, 8);
        assert_eq!(unauthed, 8);
        assert_eq!(message_bytes, 8 * 1024 * 1024);
        assert_eq!(discard_bytes, 64 * 1024 * 1024);
        assert_eq!(outbound_bytes, 8 * 1024 * 1024);
        assert_eq!(answer_frame_bytes, 16 * 1024 * 1024 - 1);
        assert_eq!(jobs_per_conn, 16);
        assert_eq!(send_buffer, None);
    }
}
