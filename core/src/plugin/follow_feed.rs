//! The plugin follow socket: nvim's follow snippet writes one JSON line per connection -- a view of the
//! followed buffer, a command the user typed, or a buffer that went away -- and the window reads them
//! here without blocking.
//!
//! Two kinds of line need opposite treatment. A view is a snapshot: only the newest per plugin matters,
//! and an older one may be dropped at will. A command or a `gone` is an event the user is waiting on:
//! none may be lost to a view, and none may overtake an older one, or a toggle followed by a hide could
//! reach the window as a hide followed by a toggle. So views are collapsed and never held back, and
//! ordered lines leave in the order their connections were accepted, a complete one waiting behind an
//! older connection that has not finished its line.
//!
//! A `gone` and a view of the same plugin still race the other way: a view is not held, so it may leave
//! before an older `gone` (in the same batch, or in an earlier one while the `gone` waited). The later of
//! the two is the truth -- a buffer whose filetype changed away and back is followed again -- so the
//! reader drops a `gone` accepted before a view of its plugin that already left. The opposite race cannot
//! happen: a `gone` waits behind an older view's unfinished connection like behind any other. So every
//! `gone` in a batch is newer than the batch's view of its plugin, and a consumer that applies the views
//! first and then the ordered lines applies them in the order they were sent.

use std::collections::{BTreeMap, VecDeque};
use std::io::{ErrorKind, Read};
use std::os::fd::RawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::Instant;

use serde_json::Value;

use crate::line_feed::{MAX_ACCEPTS_PER_POLL, MAX_CONNECTION_AGE, MAX_PENDING_CONNECTIONS, MAX_READ_BYTES_PER_POLL};
use crate::plugin::protocol::ViewCause;

/// Instance-directory prefix. Six bytes, as every other nvim socket's: with the six-byte file name below
/// the path is exactly 100 bytes at the macOS worst case, against a 103-byte cap.
pub(crate) const DIR_PREFIX: &str = "nv-fw-";

/// The socket file inside that directory. Six bytes, for the reason above.
const SOCKET_NAME: &str = "f.sock";

/// Every `(directory prefix, socket file)` pair this socket has used, so the sweep still finds the
/// directories an older build left if either name ever changes.
const SWEPT_NAMES: &[(&str, &str)] = &[(DIR_PREFIX, SOCKET_NAME)];

/// The longest line read. A view is a few hundred bytes even with a long file name; anything near this
/// is not from the snippet.
pub const MAX_FOLLOW_LINE: usize = 8 * 1024;

/// How many ordered lines may wait behind an unfinished connection. Past it the oldest are dropped: a
/// sender that opens a connection and never finishes must not grow this queue without bound.
pub const MAX_QUEUED_COMMANDS: usize = 32;

/// What nvim says the user is looking at in the followed buffer.
#[derive(Debug, Clone, PartialEq)]
pub struct ViewLine {
    pub owner: String,
    pub seq: u64,
    pub buf: u64,
    pub name: String,
    pub filetype: String,
    pub tick: u64,
    pub current: bool,
    pub line_count: u64,
    pub top: u64,
    pub top_fraction: f64,
    pub bottom: u64,
    pub cursor_line: u64,
    pub cursor_col: u64,
    pub cause: ViewCause,
    pub token: Option<u64>,
}

/// A command the user ran in nvim, for the plugin named by the line's owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FollowCmd {
    Show,
    Toggle,
    Hide,
    /// A buffer of a followed filetype was entered and the plugin asked to be shown for it.
    Auto,
}

/// A line that keeps its place among the others: a command, or the end of a buffer.
#[derive(Debug, Clone, PartialEq)]
pub enum Ordered {
    Cmd {
        owner: String,
        cmd: FollowCmd,
        buf: Option<u64>,
    },
    Gone {
        owner: String,
        buf: u64,
    },
}

/// One line of the follow socket: a view, or a line that keeps its order. Exactly one side is `Some`;
/// `None` for anything that is not a line the snippet writes. Members the host does not know are
/// ignored, so the snippet may grow without breaking an older host.
pub fn parse_line(bytes: &[u8]) -> Option<(Option<ViewLine>, Option<Ordered>)> {
    let value: Value = serde_json::from_slice(bytes).ok()?;
    let line = value.as_object()?;
    if line.get("v")?.as_u64()? != 1 {
        return None;
    }
    let owner = line.get("owner")?.as_str()?;
    if !crate::plugin::manifest::id_ok(owner) {
        return None;
    }
    let owner = owner.to_string();
    let int = |key: &str| line.get(key).and_then(Value::as_u64);
    match line.get("k")?.as_str()? {
        "view" => {
            let top_fraction = line.get("top_fraction")?.as_f64()?;
            if !top_fraction.is_finite() || !(0.0..1.0).contains(&top_fraction) {
                return None;
            }
            let cursor = line.get("cursor")?.as_object()?;
            let cause = match line.get("cause")?.as_str()? {
                "user" => ViewCause::User,
                "reveal" => ViewCause::Reveal,
                _ => return None,
            };
            let token = match line.get("token") {
                None => None,
                Some(token) => Some(token.as_u64()?),
            };
            let view = ViewLine {
                owner,
                seq: int("seq")?,
                buf: int("buf")?,
                name: line.get("name")?.as_str()?.to_string(),
                filetype: line.get("filetype")?.as_str()?.to_string(),
                tick: int("tick")?,
                current: line.get("current")?.as_bool()?,
                line_count: int("line_count")?,
                top: int("top")?,
                top_fraction,
                bottom: int("bottom")?,
                cursor_line: cursor.get("line")?.as_u64()?,
                cursor_col: cursor.get("col")?.as_u64()?,
                cause,
                token,
            };
            Some((Some(view), None))
        }
        "cmd" => {
            let cmd = match line.get("cmd")?.as_str()? {
                "show" => FollowCmd::Show,
                "toggle" => FollowCmd::Toggle,
                "hide" => FollowCmd::Hide,
                "auto" => FollowCmd::Auto,
                _ => return None,
            };
            // Only an automatic show names its buffer; a buffer on any other command is not from the
            // snippet.
            let buf = match (cmd, line.get("buf")) {
                (FollowCmd::Auto, Some(buf)) => Some(buf.as_u64()?),
                (FollowCmd::Auto, None) | (_, Some(_)) => return None,
                (_, None) => None,
            };
            Some((None, Some(Ordered::Cmd { owner, cmd, buf })))
        }
        "gone" => Some((
            None,
            Some(Ordered::Gone {
                owner,
                buf: int("buf")?,
            }),
        )),
        _ => None,
    }
}

/// What one poll of the follow socket found.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct FollowBatch {
    /// Commands and `gone` lines, in the order their connections were accepted. A `gone` here was accepted
    /// after every view of its owner this reader has let out, this batch's included.
    pub ordered: Vec<Ordered>,
    /// The newest view of each owner read in this poll, in owner order; never one accepted before a view of
    /// its owner this reader already let out.
    pub views: Vec<ViewLine>,
    /// Views parsed in this poll, before collapsing.
    pub views_received: u64,
    /// Ordered lines parsed in this poll, released or still held.
    pub ordered_received: u64,
}

impl FollowBatch {
    /// No ordered line and no view.
    pub fn is_empty(&self) -> bool {
        self.ordered.is_empty() && self.views.is_empty()
    }
}

/// This window's follow socket: a private directory holding the bound socket.
pub struct FollowSocket {
    dir: PathBuf,
    socket_path: PathBuf,
    listener: Option<UnixListener>,
}

impl FollowSocket {
    /// Sweeps stale directories, makes this window's 0700 directory and binds its 0600 socket -- or logs why not
    /// and returns `None`; then no plugin gets nvim commands or a followed buffer in this window.
    pub fn bind() -> Option<FollowSocket> {
        let tmp = std::env::temp_dir();
        for (prefix, socket_name) in SWEPT_NAMES {
            crate::instance_dir::sweep_stale_instance_dirs(&tmp, prefix, socket_name, "follow");
        }

        let dir = crate::instance_dir::instance_dir_path(&tmp, DIR_PREFIX);
        // 0700: whatever can connect to this socket can show, hide and steer a plugin's page.
        if let Err(e) = crate::instance_dir::create_private_dir(&dir) {
            eprintln!(
                "[follow] could not create {}: {e} -- no plugin gets editor commands or a followed buffer",
                dir.display()
            );
            return None;
        }
        let fail = |what: String| {
            eprintln!("[follow] {what} -- no plugin gets editor commands or a followed buffer");
            let _ = std::fs::remove_dir_all(&dir);
        };

        let socket_path = match agent::socket_path::in_dir(&dir, SOCKET_NAME) {
            Ok(path) => path,
            Err(e) => {
                fail(format!("{e}"));
                return None;
            }
        };
        let listener = match UnixListener::bind(&socket_path) {
            Ok(listener) => listener,
            Err(e) => {
                fail(format!("could not bind {}: {e}", socket_path.display()));
                return None;
            }
        };
        if let Err(e) = crate::instance_dir::make_socket_private(&socket_path) {
            fail(format!(
                "could not make {} reachable by nvim: {e}",
                socket_path.display()
            ));
            return None;
        }
        if let Err(e) = listener.set_nonblocking(true) {
            fail(format!("could not make the follow socket non-blocking: {e}"));
            return None;
        }
        println!("[follow] socket at {}", socket_path.display());
        Some(FollowSocket {
            dir,
            socket_path,
            listener: Some(listener),
        })
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// Hands the bound listener to the window's reader. `None` the second time, which is how a host
    /// detects that it wired two readers to one socket.
    pub fn take_listener(&mut self) -> Option<UnixListener> {
        self.listener.take()
    }

    /// Removes the directory. Called explicitly from the window's close handler as well: GTK does not
    /// reliably drop signal-handler closures before the process exits, so `Drop` alone can leak it.
    pub fn cleanup(&self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Drop for FollowSocket {
    fn drop(&mut self) {
        self.cleanup();
    }
}

/// An accepted connection whose line is not finished.
struct Pending {
    stream: UnixStream,
    bytes: Vec<u8>,
    accepted_at: Instant,
    number: u64,
}

enum ReadOutcome {
    Waiting,
    Line(Vec<u8>),
    /// Closed with nothing written.
    Empty,
    Oversize,
    Failed,
}

impl Pending {
    /// Reads what is there without blocking, from the shared budget. A line ends at a newline or at
    /// end of file; one that would pass [`MAX_FOLLOW_LINE`] is given up.
    fn read_available(&mut self, budget: &mut usize) -> ReadOutcome {
        let mut buffer = [0u8; 4096];
        while *budget > 0 {
            let limit = buffer.len().min(*budget);
            match self.stream.read(&mut buffer[..limit]) {
                Ok(0) if self.bytes.is_empty() => return ReadOutcome::Empty,
                Ok(0) => return ReadOutcome::Line(std::mem::take(&mut self.bytes)),
                Ok(read) => {
                    *budget -= read;
                    let newline = buffer[..read].iter().position(|byte| *byte == b'\n');
                    let length = newline.unwrap_or(read);
                    if self.bytes.len() + length > MAX_FOLLOW_LINE {
                        return ReadOutcome::Oversize;
                    }
                    self.bytes.extend_from_slice(&buffer[..length]);
                    if newline.is_some() {
                        return ReadOutcome::Line(std::mem::take(&mut self.bytes));
                    }
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::Interrupted => {
                    return ReadOutcome::Waiting;
                }
                Err(_) => return ReadOutcome::Failed,
            }
        }
        ReadOutcome::Waiting
    }
}

/// The non-blocking reader of a [`FollowSocket`]'s listener, polled from the window's feed pump.
///
/// Each poll is bounded the way every nvim feed is (16 accepts, 16 connections still reading, 64 KiB read,
/// a connection given up 2 s after it was accepted), so a stalled or flooding sender costs a bounded
/// amount of the thread that polls it.
pub struct FollowReader {
    listener: UnixListener,
    pending: VecDeque<Pending>,
    /// Ordered lines already read, waiting behind an older connection still in `pending`; sorted by
    /// accept number.
    held: VecDeque<(u64, Ordered)>,
    /// Per owner, the accept number of the newest view let out, kept only while an older `gone` or an
    /// older view could still be read.
    newest_view: BTreeMap<String, u64>,
    next_number: u64,
    #[cfg(test)]
    drop_logs: u32,
}

impl FollowReader {
    /// Takes a [`FollowSocket`]'s non-blocking listener.
    pub fn new(listener: UnixListener) -> FollowReader {
        FollowReader {
            listener,
            pending: VecDeque::new(),
            held: VecDeque::new(),
            newest_view: BTreeMap::new(),
            next_number: 0,
            #[cfg(test)]
            drop_logs: 0,
        }
    }

    /// The listening socket, for a host that waits for it to become readable.
    pub fn listener_fd(&self) -> RawFd {
        std::os::fd::AsRawFd::as_raw_fd(&self.listener)
    }

    /// A connection still reading, or an ordered line held behind one: the host must poll again on its
    /// timer, since nothing more may arrive on the listener to wake it.
    pub fn holds_connections(&self) -> bool {
        !self.pending.is_empty() || !self.held.is_empty()
    }

    #[cfg(test)]
    fn pending(&self) -> usize {
        self.pending.len()
    }

    /// Ordered lines in accept order, at most the newest 32; views collapsed to the newest per owner.
    pub fn poll(&mut self, now: Instant) -> FollowBatch {
        self.pending
            .retain(|pending| now.saturating_duration_since(pending.accepted_at) < MAX_CONNECTION_AGE);

        for _ in 0..MAX_ACCEPTS_PER_POLL {
            // The rest wait in the kernel's backlog, still in the order they connected.
            if self.pending.len() >= MAX_PENDING_CONNECTIONS {
                break;
            }
            match self.listener.accept() {
                Ok((stream, _)) => {
                    // Linux does not carry the listener's non-blocking flag onto an accepted stream.
                    if let Err(e) = stream.set_nonblocking(true) {
                        eprintln!("[follow] could not make an accepted connection non-blocking: {e} -- ignoring it");
                        continue;
                    }
                    self.pending.push_back(Pending {
                        stream,
                        bytes: Vec::new(),
                        accepted_at: now,
                        number: self.next_number,
                    });
                    self.next_number += 1;
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::Interrupted => break,
                Err(e) => {
                    eprintln!("[follow] accept failed: {e}");
                    break;
                }
            }
        }

        // Oldest first, so the line every later one may be waiting behind gets the budget first.
        let mut budget = MAX_READ_BYTES_PER_POLL;
        let mut lines = Vec::new();
        let mut oversize = 0usize;
        let mut index = 0;
        while index < self.pending.len() {
            match self.pending[index].read_available(&mut budget) {
                ReadOutcome::Waiting => index += 1,
                outcome => {
                    let done = self.pending.remove(index).expect("index is in range");
                    match outcome {
                        ReadOutcome::Line(bytes) => lines.push((done.number, bytes)),
                        ReadOutcome::Oversize => oversize += 1,
                        ReadOutcome::Empty | ReadOutcome::Failed | ReadOutcome::Waiting => {}
                    }
                }
            }
        }
        if oversize > 0 {
            eprintln!("[follow] dropped {oversize} line(s) over {MAX_FOLLOW_LINE} bytes");
        }

        let mut batch = FollowBatch::default();
        let mut views = BTreeMap::new();
        let mut malformed = 0usize;
        for (number, bytes) in lines {
            match parse_line(&bytes) {
                Some((Some(view), _)) => {
                    batch.views_received += 1;
                    // A slow connection can finish after a newer view of the same owner already left; letting
                    // it out would move the plugin back to a buffer or position nvim has since left.
                    if self.newest_view.get(&view.owner).is_some_and(|newest| *newest > number) {
                        continue;
                    }
                    self.newest_view.insert(view.owner.clone(), number);
                    views.insert(view.owner.clone(), view);
                }
                Some((None, Some(ordered))) => {
                    batch.ordered_received += 1;
                    let at = self.held.partition_point(|(held, _)| *held < number);
                    self.held.insert(at, (number, ordered));
                }
                _ => malformed += 1,
            }
        }
        if malformed > 0 {
            eprintln!("[follow] ignored {malformed} malformed line(s)");
        }

        let excess = self.held.len().saturating_sub(MAX_QUEUED_COMMANDS);
        if excess > 0 {
            self.held.drain(..excess);
            eprintln!("[follow] dropped {excess} command lines");
            #[cfg(test)]
            {
                self.drop_logs += 1;
            }
        }

        // A line leaves only once every connection accepted before it has finished or been given up.
        let oldest_reading = self.pending.iter().map(|pending| pending.number).min();
        while let Some((number, _)) = self.held.front() {
            if oldest_reading.is_some_and(|oldest| *number > oldest) {
                break;
            }
            let (number, ordered) = self.held.pop_front().expect("front exists");
            if let Ordered::Gone { owner, .. } = &ordered {
                // A view sent after it already left: the plugin follows a buffer again, which this end must
                // not close.
                if self.newest_view.get(owner).is_some_and(|view| *view > number) {
                    continue;
                }
            }
            batch.ordered.push(ordered);
        }
        // A `gone` still to leave was accepted at or after the oldest connection still reading, the oldest
        // line still held, or the next accept: a view not newer than that is never compared again.
        let floor = self
            .pending
            .iter()
            .map(|pending| pending.number)
            .chain(self.held.iter().map(|(number, _)| *number))
            .fold(self.next_number, u64::min);
        self.newest_view.retain(|_, view| *view > floor);
        batch.views = views.into_values().collect();
        batch
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use std::time::Duration;

    const VIEW: &str = r#"{"v":1,"k":"view","owner":"markdown","seq":12,"buf":3,"name":"/abs/README.md","filetype":"markdown","tick":41,"current":true,"line_count":300,"top":120,"top_fraction":0.0,"bottom":160,"cursor":{"line":125,"col":4},"cause":"user"}"#;

    fn parse(text: &str) -> Option<(Option<ViewLine>, Option<Ordered>)> {
        parse_line(text.as_bytes())
    }

    fn cmd(owner: &str, cmd: FollowCmd) -> Ordered {
        Ordered::Cmd {
            owner: owner.to_string(),
            cmd,
            buf: None,
        }
    }

    fn cmd_line(owner: &str, cmd: &str) -> String {
        format!(r#"{{"v":1,"k":"cmd","owner":"{owner}","cmd":"{cmd}"}}"#)
    }

    fn view_line(owner: &str, top: u64) -> String {
        format!(
            r#"{{"v":1,"k":"view","owner":"{owner}","seq":1,"buf":1,"name":"/p/a","filetype":"x","tick":1,"current":true,"line_count":10,"top":{top},"top_fraction":0.5,"bottom":9,"cursor":{{"line":1,"col":0}},"cause":"user"}}"#
        )
    }

    /// macOS's temp directory once canonicalised (`/var` is a link to `/private/var`): the longest prefix a
    /// test socket's directory gets.
    const MACOS_CANONICAL_TMP: &str = "/private/var/folders/33/0tqfpnyn4z3c049gljzppdv00000gn/T";

    /// A scratch directory for a test socket, removed on drop. Its name is short because the shared
    /// scratch helper's long names put a socket inside them past the 103-byte cap under macOS's temp dir.
    struct SocketDir(PathBuf);

    impl SocketDir {
        fn new(label: &str) -> SocketDir {
            let id = uuid::Uuid::new_v4().simple().to_string();
            let dir = std::env::temp_dir().join(socket_dir_name(label, &id));
            std::fs::create_dir_all(&dir).unwrap();
            SocketDir(std::fs::canonicalize(&dir).unwrap())
        }
    }

    impl Drop for SocketDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn socket_dir_name(label: &str, id: &str) -> String {
        format!("{label}-{}", &id[..12])
    }

    /// A listener in its own scratch directory, as the window's socket is, and the path to connect to.
    fn reader(label: &str) -> (SocketDir, PathBuf, FollowReader) {
        let scratch = SocketDir::new(label);
        let path = agent::socket_path::in_dir(&scratch.0, SOCKET_NAME).expect("fits");
        let listener = UnixListener::bind(&path).expect("bind");
        listener.set_nonblocking(true).expect("non-blocking");
        (scratch, path, FollowReader::new(listener))
    }

    fn send(path: &Path, line: &str) {
        let mut stream = UnixStream::connect(path).expect("connect");
        stream.write_all(format!("{line}\n").as_bytes()).expect("write");
    }

    #[test]
    fn each_kind_of_line_parses_and_a_bad_one_is_refused() {
        let expected = ViewLine {
            owner: "markdown".into(),
            seq: 12,
            buf: 3,
            name: "/abs/README.md".into(),
            filetype: "markdown".into(),
            tick: 41,
            current: true,
            line_count: 300,
            top: 120,
            top_fraction: 0.0,
            bottom: 160,
            cursor_line: 125,
            cursor_col: 4,
            cause: ViewCause::User,
            token: None,
        };
        assert_eq!(parse(VIEW), Some((Some(expected.clone()), None)));
        let reveal = VIEW.replace(r#""cause":"user""#, r#""cause":"reveal","token":7"#);
        assert_eq!(
            parse(&reveal),
            Some((
                Some(ViewLine {
                    cause: ViewCause::Reveal,
                    token: Some(7),
                    ..expected.clone()
                }),
                None
            ))
        );
        let grown = VIEW.replace(r#""v":1,"#, r#""v":1,"later":{"x":[1]},"#);
        assert_eq!(
            parse(&grown),
            Some((Some(expected), None)),
            "unknown members are ignored"
        );

        assert_eq!(
            parse(r#"{"v":1,"k":"cmd","owner":"markdown","cmd":"toggle"}"#),
            Some((None, Some(cmd("markdown", FollowCmd::Toggle))))
        );
        for (word, expected) in [
            ("show", FollowCmd::Show),
            ("hide", FollowCmd::Hide),
            ("toggle", FollowCmd::Toggle),
        ] {
            assert_eq!(
                parse(&cmd_line("markdown", word)),
                Some((None, Some(cmd("markdown", expected))))
            );
        }
        assert_eq!(
            parse(r#"{"v":1,"k":"cmd","owner":"markdown","cmd":"auto","buf":3}"#),
            Some((
                None,
                Some(Ordered::Cmd {
                    owner: "markdown".into(),
                    cmd: FollowCmd::Auto,
                    buf: Some(3)
                })
            ))
        );
        assert_eq!(
            parse(r#"{"v":1,"k":"gone","owner":"markdown","buf":3}"#),
            Some((
                None,
                Some(Ordered::Gone {
                    owner: "markdown".into(),
                    buf: 3
                })
            ))
        );

        let refused = [
            VIEW.replace(r#""v":1"#, r#""v":2"#),
            VIEW.replace(r#""k":"view""#, r#""k":"scroll""#),
            VIEW.replace(r#""owner":"markdown""#, r#""owner":"Mark down""#),
            VIEW.replace(r#""owner":"markdown","#, ""),
            r#"{"v":1,"k":"cmd","cmd":"show"}"#.to_string(),
            cmd_line("markdown", "open"),
            r#"{"v":1,"k":"cmd","owner":"markdown","cmd":"auto"}"#.to_string(),
            r#"{"v":1,"k":"cmd","owner":"markdown","cmd":"show","buf":3}"#.to_string(),
            r#"{"v":1,"k":"gone","owner":"markdown"}"#.to_string(),
            VIEW.replace(r#""top_fraction":0.0"#, r#""top_fraction":1.0"#),
            VIEW.replace(r#""top_fraction":0.0"#, r#""top_fraction":-0.5"#),
            VIEW.replace(r#""cause":"user""#, r#""cause":"page""#),
            VIEW.replace(r#""cause":"user""#, r#""cause":"user","token":null"#),
            VIEW.replace(r#""tick":41,"#, ""),
            VIEW.replace(r#""top":120"#, r#""top":-1"#),
            VIEW.replace(r#""current":true"#, r#""current":1"#),
            VIEW.replace(r#""cursor":{"line":125,"col":4}"#, r#""cursor":{"line":125}"#),
            "not json".to_string(),
            "[1,2]".to_string(),
            String::new(),
        ];
        for line in refused {
            assert_eq!(parse(&line), None, "{line}");
        }
    }

    /// The path at macOS's own worst case, asserted exactly rather than against the cap: an inequality
    /// would let the three spare bytes every nvim socket shares be spent in silence.
    #[test]
    fn the_follow_socket_path_is_exactly_100_bytes_at_the_macos_worst_case() {
        let macos_tmp = Path::new("/var/folders/33/0tqfpnyn4z3c049gljzppdv00000gn/T/");
        assert_eq!(macos_tmp.as_os_str().len(), 49);
        // A synthetic 5-digit pid (macOS's PID_MAX is 99999), so the length does not follow the pid
        // that runs the suite.
        let dir = macos_tmp.join(format!("{DIR_PREFIX}99999-{}", uuid::Uuid::new_v4().simple()));
        let path = agent::socket_path::in_dir(&dir, SOCKET_NAME).expect("must fit");
        assert_eq!(path.as_os_str().len(), 100, "{path:?}");
        assert!(
            path.as_os_str().len() <= agent::socket_path::MAX_SOCKET_PATH_BYTES,
            "{path:?}"
        );
    }

    #[test]
    fn a_real_socket_binds_private_and_round_trips_a_line() {
        let Some(mut socket) = FollowSocket::bind() else {
            return;
        };
        let path = socket.socket_path().to_path_buf();
        let dir = path.parent().unwrap().to_path_buf();
        assert!(
            dir.file_name().unwrap().to_string_lossy().starts_with(DIR_PREFIX),
            "{dir:?}"
        );
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&path), 0o600);

        let mut reader = FollowReader::new(socket.take_listener().unwrap());
        assert!(socket.take_listener().is_none(), "handed out once");
        send(&path, &cmd_line("demo", "show"));
        let batch = reader.poll(Instant::now());
        assert_eq!(batch.ordered, vec![cmd("demo", FollowCmd::Show)]);

        socket.cleanup();
        assert!(!dir.exists(), "cleanup removes the directory");
    }

    /// Every label the reader tests use, at macOS's canonical temp dir, still fits the socket cap.
    #[test]
    fn the_reader_tests_socket_paths_fit_under_the_macos_temp_dir() {
        let id = uuid::Uuid::new_v4().simple().to_string();
        for label in ["ff-order", "ff-big", "ff-overtake", "ff-queue", "ff-stale", "ff-cap"] {
            let dir = Path::new(MACOS_CANONICAL_TMP).join(socket_dir_name(label, &id));
            let path = agent::socket_path::in_dir(&dir, SOCKET_NAME).expect("fits under macOS's temp dir");
            assert!(
                path.as_os_str().len() <= agent::socket_path::MAX_SOCKET_PATH_BYTES,
                "{path:?}"
            );
        }
    }

    #[test]
    fn commands_keep_their_order_and_views_collapse_to_the_newest_per_owner() {
        let (_scratch, path, mut reader) = reader("ff-order");
        send(&path, &view_line("a", 1));
        send(&path, &cmd_line("a", "toggle"));
        send(&path, &view_line("b", 5));
        send(&path, &view_line("a", 2));
        send(&path, &cmd_line("a", "hide"));
        send(&path, &view_line("a", 3));
        send(&path, r#"{"v":1,"k":"gone","owner":"b","buf":7}"#);

        let batch = reader.poll(Instant::now());
        assert_eq!(
            batch.ordered,
            vec![
                cmd("a", FollowCmd::Toggle),
                cmd("a", FollowCmd::Hide),
                Ordered::Gone {
                    owner: "b".into(),
                    buf: 7
                },
            ]
        );
        let tops: Vec<(&str, u64)> = batch.views.iter().map(|v| (v.owner.as_str(), v.top)).collect();
        assert_eq!(tops, vec![("a", 3), ("b", 5)]);
        assert_eq!(batch.views_received, 4);
        assert_eq!(batch.ordered_received, 3);
        assert!(!reader.holds_connections());
    }

    #[test]
    fn a_line_over_8_kib_is_dropped_and_the_next_one_still_arrives() {
        let (_scratch, path, mut reader) = reader("ff-big");
        let padded = view_line("a", 1).replace("/p/a", &"x".repeat(MAX_FOLLOW_LINE));
        assert!(padded.len() > MAX_FOLLOW_LINE);
        // From a thread: a send buffer may be smaller than the line (macOS's is 8 KiB), and the reader
        // closes the connection part way, so the write may fail.
        let writer = {
            let path = path.clone();
            std::thread::spawn(move || {
                let mut stream = UnixStream::connect(&path).expect("connect");
                let _ = stream.write_all(format!("{padded}\n").as_bytes());
                drop(stream);
                send(&path, &cmd_line("a", "show"));
            })
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut ordered = Vec::new();
        let mut views = Vec::new();
        while ordered.is_empty() && Instant::now() < deadline {
            let batch = reader.poll(Instant::now());
            ordered.extend(batch.ordered);
            views.extend(batch.views);
            std::thread::sleep(Duration::from_millis(2));
        }
        writer.join().unwrap();
        assert_eq!(ordered, vec![cmd("a", FollowCmd::Show)]);
        assert!(views.is_empty(), "the long view never arrives");
    }

    #[test]
    fn a_command_never_overtakes_an_older_unfinished_one() {
        let (_scratch, path, mut reader) = reader("ff-overtake");
        let hide = cmd_line("a", "hide");
        let (first, rest) = hide.split_at(10);
        let mut older = UnixStream::connect(&path).expect("connect");
        older.write_all(first.as_bytes()).unwrap();
        send(&path, &cmd_line("a", "show"));
        send(&path, &view_line("a", 4));

        let now = Instant::now();
        let batch = reader.poll(now);
        assert!(batch.ordered.is_empty(), "the show waits behind the unfinished hide");
        assert!(reader.holds_connections());
        assert_eq!(batch.views.len(), 1, "a view is never held");
        assert_eq!(batch.ordered_received, 1);

        older.write_all(format!("{rest}\n").as_bytes()).unwrap();
        let batch = reader.poll(now);
        assert_eq!(
            batch.ordered,
            vec![cmd("a", FollowCmd::Hide), cmd("a", FollowCmd::Show)]
        );
        assert!(!reader.holds_connections());
    }

    fn gone_line(owner: &str, buf: u64) -> String {
        format!(r#"{{"v":1,"k":"gone","owner":"{owner}","buf":{buf}}}"#)
    }

    fn gone(owner: &str, buf: u64) -> Ordered {
        Ordered::Gone {
            owner: owner.into(),
            buf,
        }
    }

    #[test]
    fn a_gone_older_than_a_view_of_its_plugin_is_dropped_and_a_newer_one_kept() {
        let (_scratch, path, mut reader) = reader("ff-gone");
        // The buffer's filetype changed away and back: the view after the gone is the truth.
        send(&path, &gone_line("a", 1));
        send(&path, &view_line("a", 2));
        send(&path, &gone_line("b", 1));
        let batch = reader.poll(Instant::now());
        assert_eq!(batch.ordered, vec![gone("b", 1)], "b has no newer view");
        assert_eq!(batch.views.len(), 1);
        assert_eq!(batch.ordered_received, 2);

        // Wiped after its view: the gone stays, and it follows the view in the batch.
        send(&path, &view_line("a", 3));
        send(&path, &gone_line("a", 1));
        let batch = reader.poll(Instant::now());
        assert_eq!(batch.ordered, vec![gone("a", 1)]);
        assert_eq!(batch.views.len(), 1);
        assert!(reader.newest_view.is_empty(), "nothing older can still arrive");
    }

    #[test]
    fn a_held_gone_is_dropped_when_a_newer_view_already_left() {
        let (_scratch, path, mut reader) = reader("ff-gone-held");
        let hide = cmd_line("a", "hide");
        let (first, rest) = hide.split_at(10);
        let mut older = UnixStream::connect(&path).expect("connect");
        older.write_all(first.as_bytes()).unwrap();
        send(&path, &gone_line("a", 1));
        send(&path, &view_line("a", 4));

        let now = Instant::now();
        let batch = reader.poll(now);
        assert!(batch.ordered.is_empty(), "the gone waits behind the unfinished hide");
        assert_eq!(batch.views.len(), 1, "the newer view leaves at once");

        older.write_all(format!("{rest}\n").as_bytes()).unwrap();
        let batch = reader.poll(now);
        assert_eq!(
            batch.ordered,
            vec![cmd("a", FollowCmd::Hide)],
            "the gone is older than the view"
        );
        assert!(!reader.holds_connections());
        assert!(reader.newest_view.is_empty());

        send(&path, &gone_line("a", 1));
        assert_eq!(reader.poll(now).ordered, vec![gone("a", 1)], "a later gone leaves");
    }

    #[test]
    fn a_view_finished_after_a_newer_one_left_is_dropped() {
        let (_scratch, path, mut reader) = reader("ff-old-view");
        let old = view_line("a", 2);
        let (first, rest) = old.split_at(20);
        let mut older = UnixStream::connect(&path).expect("connect");
        older.write_all(first.as_bytes()).unwrap();
        send(&path, &view_line("a", 7));

        let now = Instant::now();
        let batch = reader.poll(now);
        assert_eq!(batch.views.len(), 1);
        assert_eq!(batch.views[0].top, 7, "the newer view leaves at once");

        older.write_all(format!("{rest}\n").as_bytes()).unwrap();
        let batch = reader.poll(now);
        assert!(batch.views.is_empty(), "the older view never follows it: {batch:?}");
        assert_eq!(batch.views_received, 1, "it was read");
        assert!(!reader.holds_connections());
        assert!(reader.newest_view.is_empty(), "nothing older can still arrive");

        send(&path, &view_line("a", 9));
        let batch = reader.poll(now);
        assert_eq!(batch.views.len(), 1, "a later view leaves");
        assert_eq!(batch.views[0].top, 9);
    }

    #[test]
    fn past_32_held_command_lines_the_oldest_are_dropped() {
        let (_scratch, path, mut reader) = reader("ff-queue");
        let mut stuck = UnixStream::connect(&path).expect("connect");
        stuck.write_all(br#"{"v":1,"k":"cmd""#).unwrap();
        // One owner per line, so which ones survived can be read back.
        for i in 1..=40 {
            send(
                &path,
                &cmd_line(&format!("c{i}"), if i == 1 { "show" } else { "toggle" }),
            );
        }

        let t0 = Instant::now();
        for _ in 0..3 {
            assert!(
                reader.poll(t0).ordered.is_empty(),
                "everything waits behind the stuck line"
            );
        }
        assert_eq!(reader.drop_logs, 1, "one log line for the poll that dropped");

        let batch = reader.poll(t0 + Duration::from_secs(3));
        assert_eq!(batch.ordered.len(), MAX_QUEUED_COMMANDS);
        assert_eq!(
            batch.ordered[0],
            cmd("c9", FollowCmd::Toggle),
            "the 9th sent comes first"
        );
        assert_eq!(batch.ordered[31], cmd("c40", FollowCmd::Toggle));
        assert_eq!(reader.drop_logs, 1);
        assert!(!reader.holds_connections(), "the stuck connection was given up");
        drop(stuck);
    }

    #[test]
    fn a_half_written_line_waits_and_a_stale_one_is_dropped() {
        let (_scratch, path, mut reader) = reader("ff-stale");
        let show = cmd_line("a", "show");
        let (first, rest) = show.split_at(12);
        let now = Instant::now();

        let mut half = UnixStream::connect(&path).expect("connect");
        half.write_all(first.as_bytes()).unwrap();
        assert!(reader.poll(now).is_empty());
        assert!(reader.holds_connections());
        half.write_all(format!("{rest}\n").as_bytes()).unwrap();
        assert_eq!(reader.poll(now).ordered, vec![cmd("a", FollowCmd::Show)]);
        assert!(!reader.holds_connections());

        let mut stale = UnixStream::connect(&path).expect("connect");
        stale.write_all(first.as_bytes()).unwrap();
        assert!(reader.poll(now).is_empty());
        assert!(reader.holds_connections(), "accepted, still reading");
        assert!(reader.poll(now + Duration::from_secs(3)).is_empty());
        assert!(!reader.holds_connections(), "given up after its age ran out");
        drop((half, stale));
    }

    #[test]
    fn at_most_16_connections_wait_at_once() {
        let (_scratch, path, mut reader) = reader("ff-cap");
        let silent: Vec<UnixStream> = (0..20).map(|_| UnixStream::connect(&path).expect("connect")).collect();
        let batch = reader.poll(Instant::now());
        assert!(batch.is_empty());
        assert!(reader.holds_connections());
        assert_eq!(reader.pending(), MAX_PENDING_CONNECTIONS);
        drop(silent);
    }
}
