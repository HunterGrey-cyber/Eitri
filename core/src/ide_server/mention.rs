//! `:EitriMention`'s way to the terminal's `claude`: a socket of its own, read in order, and the constant chunks that
//! define the command, take it away and speak for it.
//!
//! A mention is an event the person is waiting on, so it stays off the editor-context feed. That feed's reader keeps
//! only the newest line, and leaving Visual mode for the command raises a context update straight away, which would
//! replace the mention; its parser also reads any `v: 1` line as context, so a mention would wipe the window's context.
//! Here nothing is collapsed: lines leave in the order their connections were accepted, a complete one waiting behind
//! an older connection still being read.
//!
//! The socket exists only while a client is connected, so the listener's worker binds it and reads it. The worker
//! already sits in `poll`, and what it reads goes on to a path judgement that must stay off the GTK thread. The command
//! exists only between [`IDE_MENTION_DEFINE`] and [`IDE_MENTION_REMOVE`]; it is never in the feed's Lua, which loads
//! with the option off.

use std::collections::VecDeque;
use std::io::{ErrorKind, Read};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::Value;

use super::types::{MentionLine, MentionPort};
use crate::line_feed::{MAX_ACCEPTS_PER_POLL, MAX_CONNECTION_AGE, MAX_PENDING_CONNECTIONS, MAX_READ_BYTES_PER_POLL};

/// Instance-directory prefix. Six bytes, as every other nvim socket's: with the six-byte file name below the path is
/// exactly 100 bytes at the macOS worst case, against a 103-byte cap.
pub const DIR_PREFIX: &str = "nv-im-";

/// The socket file inside that directory. Six bytes, for the reason above.
const SOCKET_NAME: &str = "m.sock";

/// The longest line read. A line is a file name and two numbers; anything near this is not from the command.
const MAX_LINE_BYTES: usize = 8 * 1024;

/// How many complete lines may wait behind an unfinished connection. Past it the oldest are dropped: a sender that
/// opens a connection and never finishes must not grow this queue without bound.
const MAX_HELD_LINES: usize = 32;

/// How long the listener stays out of the poll set after `accept` failed. A connection left in the backlog keeps the
/// listener readable, and a level-triggered `poll` on it would spin a core for as long as descriptors ran short.
const ACCEPT_REST: Duration = Duration::from_secs(1);

/// Defines `:EitriMention`. Its one argument is the socket's path, which the function keeps as an upvalue.
///
/// One line per connection, as the feeds write theirs: libuv calls `connect(2)` before it returns, so two commands in a
/// row are accepted in the order they ran. Only a file's name goes out, since a terminal's or a view's name would read
/// as a relative path under the project. Any other buffer sends an empty name, which is judged outside, so the person
/// is told nothing went; so does a name JSON will not carry. `vim.json.encode` passes a name's bytes through as they
/// are, so one that is not UTF-8 is [`parse_line`]'s to catch, and the Lua stays out of judging names. `line1` and
/// `line2` are the cursor's line when no range was given. No key is bound: the person binds one if they want it.
/// Harmless late: run after [`IDE_MENTION_REMOVE`] it leaves a command whose connect fails, and that drops the line.
pub const IDE_MENTION_DEFINE: &str = crate::editor_rpc::takeover_chunk!(
    r#"
local socket = ...
local takeover_options = select(2, ...)
local takeover = type(takeover_options) == 'table' and takeover_options.takeover_chan ~= nil
if takeover and (vim.v.vim_did_enter == 0 or vim.fn.exists(':EitriMention') ~= 0) then return false end
local torn = false
vim.api.nvim_create_user_command('EitriMention', function(opts)
  if torn then return end
  local name = vim.bo.buftype == '' and vim.api.nvim_buf_get_name(0) or ''
  local function line_for(file)
    return vim.json.encode({ v = 1, file = file, start = opts.line1 - 1, ['end'] = opts.line2 - 1 })
  end
  local ok, payload = pcall(line_for, name)
  if not ok then
    payload = line_for('')
  end
  -- A connect error drops the line: the host may be gone, and nvim must not wait for it.
  local pipe = vim.uv.new_pipe(false)
  if not pipe then
    return
  end
  local started = pcall(pipe.connect, pipe, socket, function(err)
    if err then
      pipe:close()
      return
    end
    pipe:write(payload .. '\n', function()
      pipe:close()
    end)
  end)
  if not started and not pipe:is_closing() then
    pipe:close()
  end
end, { range = true, force = not takeover, desc = "Mention this file's lines to the claude in the bottom terminal" })
if takeover then
  local command = vim.api.nvim_get_commands({builtin=false}).EitriMention
  local glue = rawget(_G, '__eitri_companion')
  local record = {definition=command.definition}
  glue.takeover_mention = record
  record.teardown = function()
    if torn then return end
    torn = true
    local current = vim.api.nvim_get_commands({builtin=false}).EitriMention
    if current and current.definition == command.definition then pcall(vim.api.nvim_del_user_command, 'EitriMention') end
  end
end
return true
"#,
    r#"
local record = __takeover_glue.takeover_mention
if __takeover_result and record then __takeover_adopt('mention', record.teardown) end
"#
);

/// Takes `:EitriMention` away. Harmless late or twice: the command may already be gone.
pub const IDE_MENTION_REMOVE: &str = "local glue = rawget(_G, '__eitri_companion'); if glue and glue.mode == 'takeover' then if glue.takeover_mention then glue.takeover_mention.teardown() end else pcall(vim.api.nvim_del_user_command, 'EitriMention') end";

/// Says in the message area that a mention was not sent, since the judgement runs on the judge's thread, not in nvim.
/// The file may be in the project and still have a name no client can be told. Harmless late: it changes nothing but
/// the message area.
pub const IDE_MENTION_OUTSIDE: &str = "vim.api.nvim_echo({ { 'Eitri: that file is outside the project, or its name \
     cannot be sent; nothing was sent', 'WarningMsg' } }, true, {})";

/// One line of the socket: `{"v":1,"file":..,"start":N,"end":N}`, 0-based. `None` for anything else, a range running
/// backwards included. Members the host does not know are ignored, so the command may grow without breaking an older
/// host. The name is not judged here: that takes the disk, and belongs to the judge's thread. A line that is not UTF-8
/// is the command's all the same, since buffer names are bytes: it comes through with no name, which is judged outside,
/// so the person is told nothing went.
pub fn parse_line(bytes: &[u8]) -> Option<MentionLine> {
    let name_is_text = std::str::from_utf8(bytes).is_ok();
    let value: Value = serde_json::from_str(&String::from_utf8_lossy(bytes)).ok()?;
    let line = value.as_object()?;
    if line.get("v")?.as_u64()? != 1 {
        return None;
    }
    let number = |key: &str| u32::try_from(line.get(key)?.as_u64()?).ok();
    let (line_start, line_end) = (number("start")?, number("end")?);
    if line_start > line_end {
        return None;
    }
    let file = line.get("file")?.as_str()?;
    Some(MentionLine {
        file: if name_is_text { file.to_string() } else { String::new() },
        line_start,
        line_end,
    })
}

/// The socket while it is bound.
struct Bound {
    dir: PathBuf,
    socket: PathBuf,
    listener: UnixListener,
}

impl Bound {
    /// Sweeps stale directories under `tmp`, makes this socket's 0700 directory and binds its 0600 socket, or logs why
    /// not and returns `None`; then nvim gets no `:EitriMention`.
    fn bind(tmp: &Path) -> Option<Bound> {
        // 0700: whatever can connect to this socket can put a mention into the person's prompt.
        let dir = match crate::instance_dir::create_instance_dir(tmp, DIR_PREFIX, SOCKET_NAME, "ide-mention") {
            Ok(dir) => dir,
            Err(e) => {
                eprintln!(
                    "[ide-mention] could not create a directory under {}: {e} -- no :EitriMention",
                    tmp.display()
                );
                return None;
            }
        };
        match Bound::listen_in(&dir) {
            Ok((socket, listener)) => Some(Bound { dir, socket, listener }),
            Err(why) => {
                eprintln!("[ide-mention] {why} -- no :EitriMention");
                let _ = std::fs::remove_dir_all(&dir);
                None
            }
        }
    }

    fn listen_in(dir: &Path) -> Result<(PathBuf, UnixListener), String> {
        let socket = agent::socket_path::in_dir(dir, SOCKET_NAME).map_err(|e| e.to_string())?;
        let listener = UnixListener::bind(&socket).map_err(|e| format!("could not bind {}: {e}", socket.display()))?;
        crate::instance_dir::make_socket_private(&socket)
            .map_err(|e| format!("could not make {} reachable by nvim: {e}", socket.display()))?;
        listener
            .set_nonblocking(true)
            .map_err(|e| format!("could not make the mention socket non-blocking: {e}"))?;
        Ok((socket, listener))
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
    /// Reads what is there without blocking, from the shared budget. A line ends at a newline or at end of file; one
    /// that would pass [`MAX_LINE_BYTES`] is given up.
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
                    if self.bytes.len() + length > MAX_LINE_BYTES {
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

/// The mention socket and its reader, polled from the listener's worker.
///
/// Each poll is bounded the way every nvim feed is (16 accepts, 16 connections still reading, 64 KiB read, a connection
/// given up 2 s after it was accepted), so a stalled or flooding sender costs a bounded amount of the worker.
pub struct LiveMentions {
    bound: Option<Bound>,
    pending: VecDeque<Pending>,
    /// Lines already read, waiting behind an older connection still in `pending`; sorted by accept number.
    held: VecDeque<(u64, MentionLine)>,
    next_number: u64,
    /// While set, no `accept` runs and the listener is out of the poll set.
    accept_rest: Option<Instant>,
    #[cfg(test)]
    drop_logs: u32,
}

impl LiveMentions {
    pub fn new() -> LiveMentions {
        LiveMentions {
            bound: None,
            pending: VecDeque::new(),
            held: VecDeque::new(),
            next_number: 0,
            accept_rest: None,
            #[cfg(test)]
            drop_logs: 0,
        }
    }

    fn open_in(&mut self, tmp: &Path) -> Option<PathBuf> {
        if self.bound.is_none() {
            self.bound = Bound::bind(tmp);
        }
        self.bound.as_ref().map(|bound| bound.socket.clone())
    }

    fn accept_waiting(&mut self, now: Instant) {
        let Some(bound) = &self.bound else { return };
        for _ in 0..MAX_ACCEPTS_PER_POLL {
            // The rest wait in the kernel's backlog, still in the order they connected.
            if self.pending.len() >= MAX_PENDING_CONNECTIONS {
                return;
            }
            match bound.listener.accept() {
                Ok((stream, _)) => {
                    // Linux does not carry the listener's non-blocking flag onto an accepted stream.
                    if let Err(e) = stream.set_nonblocking(true) {
                        eprintln!("[ide-mention] could not make a connection non-blocking: {e} -- ignoring it");
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
                Err(e) if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::Interrupted => return,
                Err(e) => {
                    eprintln!("[ide-mention] accept failed: {e}");
                    self.accept_rest = Some(now + ACCEPT_REST);
                    return;
                }
            }
        }
    }

    /// Reads every connection, oldest first so the line every later one may be waiting behind gets the budget first.
    /// What finished moves to `held`, in accept order.
    fn read_pending(&mut self) {
        let mut budget = MAX_READ_BYTES_PER_POLL;
        let mut oversize = 0usize;
        let mut malformed = 0usize;
        let mut index = 0;
        while index < self.pending.len() {
            match self.pending[index].read_available(&mut budget) {
                ReadOutcome::Waiting => index += 1,
                outcome => {
                    let done = self.pending.remove(index).expect("index is in range");
                    match outcome {
                        ReadOutcome::Line(bytes) => match parse_line(&bytes) {
                            Some(line) => {
                                let at = self.held.partition_point(|(number, _)| *number < done.number);
                                self.held.insert(at, (done.number, line));
                            }
                            None => malformed += 1,
                        },
                        ReadOutcome::Oversize => oversize += 1,
                        ReadOutcome::Empty | ReadOutcome::Failed | ReadOutcome::Waiting => {}
                    }
                }
            }
        }
        if oversize > 0 {
            eprintln!("[ide-mention] dropped {oversize} line(s) over {MAX_LINE_BYTES} bytes");
        }
        if malformed > 0 {
            eprintln!("[ide-mention] ignored {malformed} malformed line(s)");
        }
    }
}

impl Default for LiveMentions {
    fn default() -> LiveMentions {
        LiveMentions::new()
    }
}

impl MentionPort for LiveMentions {
    fn open(&mut self) -> Option<PathBuf> {
        let base = match crate::instance_dir::feed_base("editor mention socket") {
            Ok(base) => base,
            Err(e) => {
                eprintln!("[ide-mention] {e} -- :EitriMention is unavailable");
                return None;
            }
        };
        let tmp = std::env::temp_dir();
        if base != tmp {
            crate::instance_dir::sweep_stale_instance_dirs(&tmp, DIR_PREFIX, SOCKET_NAME, "ide-mention");
        }
        self.open_in(&base)
    }

    /// The listener is left out while nothing would accept it: with every pending slot taken or `accept` resting, a
    /// backlog would keep it readable and spin the worker's `poll`.
    fn fds(&self) -> Vec<RawFd> {
        let Some(bound) = &self.bound else {
            return Vec::new();
        };
        let accepting = self.pending.len() < MAX_PENDING_CONNECTIONS && self.accept_rest.is_none();
        accepting
            .then(|| bound.listener.as_raw_fd())
            .into_iter()
            .chain(self.pending.iter().map(|pending| pending.stream.as_raw_fd()))
            .collect()
    }

    fn poll(&mut self, now: Instant) -> Vec<MentionLine> {
        if self.bound.is_none() {
            return Vec::new();
        }
        self.pending
            .retain(|pending| now.saturating_duration_since(pending.accepted_at) < MAX_CONNECTION_AGE);
        if self.accept_rest.is_some_and(|until| now >= until) {
            self.accept_rest = None;
        }
        if self.accept_rest.is_none() {
            self.accept_waiting(now);
        }
        self.read_pending();

        let excess = self.held.len().saturating_sub(MAX_HELD_LINES);
        if excess > 0 {
            self.held.drain(..excess);
            eprintln!("[ide-mention] dropped {excess} line(s) waiting behind an unfinished connection");
            #[cfg(test)]
            {
                self.drop_logs += 1;
            }
        }

        // A line leaves only once every connection accepted before it has finished or been given up.
        let oldest_reading = self.pending.front().map(|pending| pending.number);
        let mut lines = Vec::new();
        while let Some((number, _)) = self.held.front() {
            if oldest_reading.is_some_and(|oldest| *number > oldest) {
                break;
            }
            let (_, line) = self.held.pop_front().expect("front exists");
            lines.push(line);
        }
        lines
    }

    fn next_deadline(&self) -> Option<Instant> {
        // Accepted in order, each at the time of its poll, so the first is the first to run out.
        let given_up = self
            .pending
            .front()
            .map(|pending| pending.accepted_at + MAX_CONNECTION_AGE);
        given_up.into_iter().chain(self.accept_rest).min()
    }

    fn close(&mut self) {
        self.pending.clear();
        self.held.clear();
        self.accept_rest = None;
        if let Some(bound) = self.bound.take() {
            let _ = std::fs::remove_dir_all(&bound.dir);
        }
    }
}

impl Drop for LiveMentions {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    fn opened() -> (LiveMentions, PathBuf) {
        let mut mentions = LiveMentions::new();
        let path = mentions.open().expect("the socket binds");
        (mentions, path)
    }

    fn line(file: &str, start: u32, end: u32) -> String {
        format!(r#"{{"v":1,"file":"{file}","start":{start},"end":{end}}}"#)
    }

    fn mention(file: &str, line_start: u32, line_end: u32) -> MentionLine {
        MentionLine {
            file: file.to_string(),
            line_start,
            line_end,
        }
    }

    fn send(path: &Path, text: &str) {
        let mut stream = UnixStream::connect(path).expect("connect");
        stream.write_all(format!("{text}\n").as_bytes()).expect("write");
    }

    /// The path at macOS's own worst case, asserted exactly rather than against the cap: an inequality would let the
    /// three spare bytes every nvim socket shares be spent in silence.
    #[test]
    fn the_socket_is_exactly_100_bytes_at_the_macos_worst_case() {
        let macos_tmp = Path::new("/var/folders/33/0tqfpnyn4z3c049gljzppdv00000gn/T/");
        assert_eq!(macos_tmp.as_os_str().len(), 49);
        // A synthetic 5-digit pid (macOS's PID_MAX is 99999), so the length does not follow the pid that runs the
        // suite.
        let dir = macos_tmp.join(format!("{DIR_PREFIX}99999-{}", uuid::Uuid::new_v4().simple()));
        let path = agent::socket_path::in_dir(&dir, SOCKET_NAME).expect("must fit");
        assert_eq!(path.as_os_str().len(), 100, "{path:?}");
        assert!(
            path.as_os_str().len() <= agent::socket_path::MAX_SOCKET_PATH_BYTES,
            "{path:?}"
        );
    }

    #[test]
    fn the_directory_is_0700_and_the_socket_0600() {
        let (_mentions, path) = opened();
        let dir = path.parent().unwrap();
        assert_eq!(dir.parent(), agent::state_dirs::runtime_dir().as_deref());
        assert!(
            dir.file_name().unwrap().to_string_lossy().starts_with(DIR_PREFIX),
            "{dir:?}"
        );
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(dir), 0o700);
        assert_eq!(mode(&path), 0o600);
    }

    #[test]
    fn opening_sweeps_a_dead_instances_directory() {
        // Pid 0 is never alive, and nothing in the directory answers.
        let stale = std::env::temp_dir().join(format!("{DIR_PREFIX}0-{}", uuid::Uuid::new_v4().simple()));
        crate::instance_dir::create_private_dir(&stale).unwrap();
        let (_mentions, _path) = opened();
        let gone = !stale.exists();
        let _ = std::fs::remove_dir_all(&stale);
        assert!(gone, "the stale directory was not swept");
    }

    #[test]
    fn a_second_open_keeps_the_socket() {
        let (mut mentions, path) = opened();
        assert_eq!(mentions.open(), Some(path));
    }

    #[test]
    fn a_directory_that_cannot_be_made_means_no_socket() {
        let missing = std::env::temp_dir().join(format!("nv-none-{}", uuid::Uuid::new_v4().simple()));
        let mut mentions = LiveMentions::new();
        assert_eq!(mentions.open_in(&missing), None);
        assert!(mentions.fds().is_empty());
        assert!(mentions.poll(Instant::now()).is_empty());
    }

    #[test]
    fn a_path_over_the_cap_means_no_socket_and_leaves_no_directory() {
        let scratch = crate::test_scratch_dir::ScratchDir::new("nv-long", "cap");
        let long = scratch.join("x".repeat(120));
        std::fs::create_dir(&long).unwrap();
        let mut mentions = LiveMentions::new();
        let opened = mentions.open_in(&long);
        let left = std::fs::read_dir(&long).unwrap().count();
        assert_eq!(opened, None);
        assert_eq!(left, 0, "the directory made for the socket goes with it");
    }

    #[test]
    fn a_line_comes_through() {
        let (mut mentions, path) = opened();
        send(&path, &line("/p/a", 4, 9));
        assert_eq!(mentions.poll(Instant::now()), vec![mention("/p/a", 4, 9)]);
        assert_eq!(mentions.next_deadline(), None, "nothing is left to wait for");
    }

    #[test]
    fn lines_leave_in_accept_order() {
        let (mut mentions, path) = opened();
        let whole = line("/p/a", 0, 1);
        let (first, rest) = whole.split_at(8);
        let mut older = UnixStream::connect(&path).expect("connect");
        older.write_all(first.as_bytes()).unwrap();
        send(&path, &line("/p/b", 2, 2));

        let now = Instant::now();
        assert!(mentions.poll(now).is_empty(), "b waits behind the unfinished a");
        older.write_all(format!("{rest}\n").as_bytes()).unwrap();
        assert_eq!(mentions.poll(now), vec![mention("/p/a", 0, 1), mention("/p/b", 2, 2)]);
    }

    #[test]
    fn a_stalled_connection_is_given_up_after_its_age() {
        let (mut mentions, path) = opened();
        let mut stalled = UnixStream::connect(&path).expect("connect");
        stalled.write_all(br#"{"v":1,"#).unwrap();
        send(&path, &line("/p/b", 2, 2));

        let now = Instant::now();
        assert!(mentions.poll(now).is_empty());
        assert_eq!(mentions.next_deadline(), Some(now + MAX_CONNECTION_AGE));
        assert!(mentions.poll(now + Duration::from_millis(1999)).is_empty());
        assert_eq!(
            mentions.poll(now + MAX_CONNECTION_AGE),
            vec![mention("/p/b", 2, 2)],
            "the line behind it leaves once it is given up"
        );
        assert_eq!(mentions.next_deadline(), None);
    }

    #[test]
    fn an_oversized_line_is_dropped() {
        let (mut mentions, path) = opened();
        let padded = line(&"x".repeat(MAX_LINE_BYTES), 0, 0);
        assert!(padded.len() > MAX_LINE_BYTES);
        // From a thread: a send buffer may be smaller than the line, and the reader closes the connection part way, so
        // the write may fail.
        let writer = {
            let path = path.clone();
            std::thread::spawn(move || {
                let mut stream = UnixStream::connect(&path).expect("connect");
                let _ = stream.write_all(format!("{padded}\n").as_bytes());
                drop(stream);
                send(&path, &line("/p/next", 1, 1));
            })
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut lines = Vec::new();
        while lines.is_empty() && Instant::now() < deadline {
            lines.extend(mentions.poll(Instant::now()));
            std::thread::sleep(Duration::from_millis(2));
        }
        writer.join().unwrap();
        assert_eq!(lines, vec![mention("/p/next", 1, 1)]);
    }

    #[test]
    fn the_queue_keeps_its_bound() {
        let (mut mentions, path) = opened();
        let mut stuck = UnixStream::connect(&path).expect("connect");
        stuck.write_all(br#"{"v":1,"#).unwrap();
        for i in 1..=40 {
            send(&path, &line(&format!("/p/{i}"), i, i));
        }

        let now = Instant::now();
        for _ in 0..3 {
            assert!(mentions.poll(now).is_empty(), "everything waits behind the stuck line");
        }
        assert_eq!(mentions.drop_logs, 1, "one log line for the poll that dropped");

        let lines = mentions.poll(now + Duration::from_secs(3));
        assert_eq!(lines.len(), MAX_HELD_LINES);
        assert_eq!(lines[0], mention("/p/9", 9, 9), "the oldest went");
        assert_eq!(lines[31], mention("/p/40", 40, 40));
        assert_eq!(mentions.drop_logs, 1);
        drop(stuck);
    }

    #[test]
    fn a_bad_line_is_not_a_mention() {
        let (mut mentions, path) = opened();
        for bad in ["not json", "[1,2]", r#"{"v":2,"file":"/p/a","start":0,"end":0}"#, ""] {
            send(&path, bad);
        }
        send(&path, &line("/p/good", 3, 3));
        assert_eq!(mentions.poll(Instant::now()), vec![mention("/p/good", 3, 3)]);
    }

    #[test]
    fn close_removes_the_directory() {
        let (mut mentions, path) = opened();
        let dir = path.parent().unwrap().to_path_buf();
        let held_open = UnixStream::connect(&path).expect("connect");
        mentions.poll(Instant::now());
        assert_eq!(mentions.fds().len(), 2, "the listener and the connection");

        mentions.close();
        assert!(!dir.exists());
        assert!(mentions.fds().is_empty());
        assert_eq!(mentions.next_deadline(), None);
        assert!(mentions.poll(Instant::now()).is_empty());
        drop(held_open);

        let again = mentions.open().expect("a later client binds it again");
        assert_ne!(again, path);
        mentions.close();
        mentions.close();
        assert!(!again.parent().unwrap().exists());
    }

    #[test]
    fn dropping_it_removes_the_directory() {
        let (mentions, path) = opened();
        drop(mentions);
        assert!(!path.parent().unwrap().exists());
    }

    #[test]
    fn with_every_slot_taken_the_listener_leaves_the_poll_set() {
        let (mut mentions, path) = opened();
        let listener = mentions.fds()[0];
        let silent: Vec<UnixStream> = (0..MAX_PENDING_CONNECTIONS + 4)
            .map(|_| UnixStream::connect(&path).expect("connect"))
            .collect();
        let now = Instant::now();
        mentions.poll(now);
        let fds = mentions.fds();
        assert_eq!(fds.len(), MAX_PENDING_CONNECTIONS);
        assert!(
            !fds.contains(&listener),
            "a backlog would keep a level-triggered poll awake"
        );

        drop(silent);
        // A child another test thread is starting holds a copy of every descriptor until it execs, so the end of file
        // comes when the last copy closes. `now` stands still: it is the end of file that frees the slots, not their age.
        let deadline = Instant::now() + Duration::from_secs(20);
        while !mentions.fds().contains(&listener) {
            assert!(Instant::now() < deadline, "room again");
            mentions.poll(now);
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn a_resting_accept_leaves_the_poll_set_and_comes_back() {
        let (mut mentions, path) = opened();
        let listener = mentions.fds()[0];
        let now = Instant::now();
        mentions.accept_rest = Some(now + ACCEPT_REST);
        send(&path, &line("/p/a", 0, 0));

        assert!(mentions.fds().is_empty());
        assert_eq!(mentions.next_deadline(), Some(now + ACCEPT_REST));
        assert!(mentions.poll(now).is_empty(), "nothing is accepted while it rests");
        assert_eq!(mentions.poll(now + ACCEPT_REST), vec![mention("/p/a", 0, 0)]);
        assert_eq!(mentions.fds(), vec![listener]);
    }

    #[test]
    fn parse_line_refuses_an_end_before_its_start() {
        let parse = |text: &str| parse_line(text.as_bytes());
        assert_eq!(parse(&line("/p/a", 2, 2)), Some(mention("/p/a", 2, 2)));
        assert_eq!(parse(&line("/p/a", 2, 3)), Some(mention("/p/a", 2, 3)));
        assert_eq!(parse(&line("/p/a", 3, 2)), None);
    }

    #[test]
    fn a_name_that_is_not_utf8_comes_through_as_no_name() {
        assert_eq!(
            parse_line(b"{\"v\":1,\"file\":\"/p/caf\xe9.rs\",\"start\":2,\"end\":3}"),
            Some(mention("", 2, 3)),
            "an empty name is judged outside, so the person is told nothing went"
        );
        assert_eq!(parse_line(b"not json \xe9"), None);
    }

    #[test]
    fn parse_line_takes_only_what_the_command_writes() {
        let parse = |text: &str| parse_line(text.as_bytes());
        assert_eq!(
            parse(r#"{"v":1,"later":[1],"file":"/p/a","start":0,"end":0}"#),
            Some(mention("/p/a", 0, 0)),
            "unknown members are ignored"
        );
        for bad in [
            r#"{"file":"/p/a","start":0,"end":0}"#,
            r#"{"v":2,"file":"/p/a","start":0,"end":0}"#,
            r#"{"v":1,"start":0,"end":0}"#,
            r#"{"v":1,"file":1,"start":0,"end":0}"#,
            r#"{"v":1,"file":"/p/a","end":0}"#,
            r#"{"v":1,"file":"/p/a","start":0}"#,
            r#"{"v":1,"file":"/p/a","start":-1,"end":0}"#,
            r#"{"v":1,"file":"/p/a","start":0.5,"end":1}"#,
            r#"{"v":1,"file":"/p/a","start":0,"end":4294967296}"#,
            "not json",
            "",
        ] {
            assert_eq!(parse(bad), None, "{bad}");
        }
    }
}
