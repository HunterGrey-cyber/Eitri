//! The bounded, newest-wins line reader every nvim->host push socket in this crate shares.
//!
//! Extracted verbatim from `editor_context::feed` (spec `2026-09-26-panel-round2-design.md` §3),
//! where it was written and reviewed first; the only generalization is that the "is this line
//! valid" decision is the caller's `accept` closure rather than one fixed parser. Read the doc on
//! [`NewestLineReader`] before changing a bound: each one exists because a stalled or oversized
//! sender must not monopolize the GTK driver that polls this.

use std::io::{ErrorKind, Read};
use std::os::unix::net::{UnixListener, UnixStream};
use std::time::{Duration, Instant};

// A stalled/oversized sender must not monopolize the GTK driver. The editor-context snippet
// normally sends a small update every 150ms; these limits also leave room for its bounded 400-line
// selection. They do NOT leave room for a full nvim-keys report at its own stated bounds: 2000
// maps of up to 200 characters each for `rhs` and `desc` (`lhs` is not cut at all) is about 800KB
// before JSON overhead -- roughly 131 bytes of budget per map at 2000 maps, not 400. Per spec
// (2026-09-26-panel-round2-design.md §3.2/§3.8), an oversized report is dropped whole and the host
// keeps its last good one; whether to lower MAX_MAPS/MAX_TEXT to fit under this cap is a later
// decision, not made here.
pub(crate) const MAX_PENDING_CONNECTIONS: usize = 16;
pub(crate) const MAX_ACCEPTS_PER_POLL: usize = 16;
pub(crate) const MAX_LINE_BYTES: usize = 256 * 1024;
pub(crate) const MAX_READ_BYTES_PER_POLL: usize = 64 * 1024;
pub(crate) const MAX_CONNECTION_AGE: Duration = Duration::from_secs(2);
/// How many waiting connections one [`NewestLineReader::discard`] drops.
const DISCARD_ACCEPTS: usize = 256;

/// A non-blocking reader retained across the host's existing 100ms polls.
///
/// Connecting and writing are separate async operations in the Lua senders. An accepted socket
/// need not contain a whole line yet: retaining it on `WouldBlock` avoids both a GTK-thread read
/// wait and the lost updates a stateless non-blocking read would cause.
///
/// Accept order defines freshness. A newer complete, accepted line supersedes all older
/// connections, even if an older sender only finishes on a later poll. A line `accept` refuses
/// does not discard the last accepted one.
pub struct NewestLineReader {
    listener: UnixListener,
    label: &'static str,
    pending: Vec<PendingLine>,
    next_sequence: u64,
    published_sequence: u64,
}

struct PendingLine {
    stream: UnixStream,
    bytes: Vec<u8>,
    accepted_at: Instant,
    sequence: u64,
}

enum ReadStatus {
    Pending,
    Complete(Vec<u8>),
    Closed,
}

impl PendingLine {
    fn read_available(&mut self, budget: &mut usize) -> ReadStatus {
        let mut buffer = [0; 4096];
        while *budget > 0 {
            let limit = buffer.len().min(*budget);
            match self.stream.read(&mut buffer[..limit]) {
                // Preserve read_line's EOF framing: a complete JSON value without a newline is
                // still valid; a truncated value is rejected by `accept`, without clearing the cache.
                Ok(0) => return ReadStatus::Complete(std::mem::take(&mut self.bytes)),
                Ok(read) => {
                    *budget -= read;
                    let newline = buffer[..read].iter().position(|byte| *byte == b'\n');
                    let length = newline.unwrap_or(read);
                    if self.bytes.len() + length > MAX_LINE_BYTES {
                        return ReadStatus::Closed;
                    }
                    self.bytes.extend_from_slice(&buffer[..length]);
                    if newline.is_some() {
                        return ReadStatus::Complete(std::mem::take(&mut self.bytes));
                    }
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::Interrupted => {
                    return ReadStatus::Pending;
                }
                Err(_) => return ReadStatus::Closed,
            }
        }
        ReadStatus::Pending
    }
}

impl NewestLineReader {
    /// Takes a feed's already non-blocking listener. Each accepted stream is explicitly made
    /// non-blocking too: Linux does not inherit the listener's flag, while macOS does. `label` is
    /// the log prefix (`[<label>] ...`).
    pub fn new(listener: UnixListener, label: &'static str) -> Self {
        Self {
            listener,
            label,
            pending: Vec::new(),
            next_sequence: 1,
            published_sequence: 0,
        }
    }

    /// Forgets every line not yet delivered: the connections already being read, and the ones
    /// still waiting in the listener's backlog. A host that changes what is on the other end of
    /// the socket calls this, so nothing the old end wrote reaches the new one. What a sender
    /// writes after this call is delivered as usual.
    pub fn discard(&mut self) {
        self.pending.clear();
        // Bounded: a sender that connects as fast as it is drained must not hold the caller.
        for _ in 0..DISCARD_ACCEPTS {
            match self.listener.accept() {
                Ok(_) => {}
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
    }

    /// The newest complete line whose `accept` returned true, or None for no news.
    pub fn poll_with(&mut self, accept: impl FnMut(&[u8]) -> bool) -> Option<Vec<u8>> {
        self.poll_with_at(Instant::now(), accept)
    }

    pub(crate) fn poll_with_at(&mut self, now: Instant, mut accept: impl FnMut(&[u8]) -> bool) -> Option<Vec<u8>> {
        self.pending
            .retain(|pending| now.duration_since(pending.accepted_at) < MAX_CONNECTION_AGE);
        for _ in 0..MAX_ACCEPTS_PER_POLL {
            match self.listener.accept() {
                Ok((stream, _)) => {
                    if let Err(e) = stream.set_nonblocking(true) {
                        eprintln!(
                            "[{}] could not make an accepted connection non-blocking: {e}",
                            self.label
                        );
                        continue;
                    }
                    if self.pending.len() == MAX_PENDING_CONNECTIONS {
                        // Prefer a recent line over a sender that has not completed an older one.
                        self.pending.remove(0);
                    }
                    self.pending.push(PendingLine {
                        stream,
                        bytes: Vec::new(),
                        accepted_at: now,
                        sequence: self.next_sequence,
                    });
                    self.next_sequence += 1;
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::Interrupted => break,
                Err(e) => {
                    eprintln!("[{}] accept failed: {e}", self.label);
                    break;
                }
            }
        }

        let mut budget = MAX_READ_BYTES_PER_POLL;
        let mut latest = None;
        // Newest first: an older large line must not consume the read budget before a small,
        // recent update. Completed refused lines still leave earlier accepted ones eligible.
        for index in (0..self.pending.len()).rev() {
            let pending = &mut self.pending[index];
            if pending.sequence <= self.published_sequence {
                self.pending.remove(index);
                continue;
            }
            match pending.read_available(&mut budget) {
                ReadStatus::Pending => {}
                ReadStatus::Closed => {
                    self.pending.remove(index);
                }
                ReadStatus::Complete(bytes) => {
                    let sequence = pending.sequence;
                    self.pending.remove(index);
                    if accept(&bytes) {
                        self.published_sequence = sequence;
                        latest = Some(bytes);
                    }
                }
            }
        }
        latest
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor_context::feed::EditorContextFeed;
    use std::io::Write;

    /// These tests exercise the bounds, not a parser; the accepted shape is the editor-context
    /// one they were written against (`{"v":1,"file":...}`), read through this minimal stand-in.
    struct Context {
        file: String,
    }

    fn file_of(line: &[u8]) -> Option<Context> {
        let value: serde_json::Value = serde_json::from_slice(line).ok()?;
        if value.get("v").and_then(serde_json::Value::as_u64) != Some(1) {
            return None;
        }
        Some(Context {
            file: value.get("file")?.as_str()?.to_string(),
        })
    }

    trait PollContext {
        fn poll(&mut self) -> Option<Context>;
        fn poll_at(&mut self, now: Instant) -> Option<Context>;
    }

    impl PollContext for NewestLineReader {
        fn poll(&mut self) -> Option<Context> {
            self.poll_with(|line| file_of(line).is_some())
                .and_then(|line| file_of(&line))
        }
        fn poll_at(&mut self, now: Instant) -> Option<Context> {
            self.poll_with_at(now, |line| file_of(line).is_some())
                .and_then(|line| file_of(&line))
        }
    }

    fn reader_for(feed: &mut EditorContextFeed) -> NewestLineReader {
        NewestLineReader::new(feed.take_listener().unwrap(), "line-feed-test")
    }

    #[test]
    fn an_incomplete_line_survives_until_a_later_poll() {
        let mut feed = EditorContextFeed::new().expect("bind the test feed");
        let mut reader = reader_for(&mut feed);
        let mut client = UnixStream::connect(feed.socket_path()).unwrap();
        client.write_all(br#"{"v":1,"file":"/partial"#).unwrap();
        assert!(reader.poll().is_none());
        client
            .write_all(b".rs\"}\n")
            .expect("an incomplete update must stay connected");
        let context = reader.poll().expect("retain the first part");
        assert_eq!(context.file, "/partial.rs");
    }

    /// A coarse guard against the old four successive 100ms socket waits. This is a real Unix
    /// socket check, not a claim about GUI frame time or input-to-photon latency.
    #[test]
    fn four_silent_connections_do_not_hold_the_ui_poll() {
        let mut feed = EditorContextFeed::new().expect("bind the test feed");
        let mut reader = reader_for(&mut feed);
        let clients: Vec<_> = (0..4)
            .map(|_| UnixStream::connect(feed.socket_path()).unwrap())
            .collect();
        let before = Instant::now();
        assert!(reader.poll().is_none());
        let elapsed = before.elapsed();
        eprintln!("line-feed poll with four silent clients: {elapsed:?}");
        assert!(elapsed < Duration::from_millis(200), "poll blocked for {elapsed:?}");
        drop(clients);
    }

    #[test]
    fn discard_drops_what_was_queued_and_what_was_half_read_but_not_what_comes_after() {
        let mut feed = EditorContextFeed::new().expect("bind the test feed");
        let mut reader = reader_for(&mut feed);
        // Half read: accepted by a poll, the rest still to come.
        let mut half = UnixStream::connect(feed.socket_path()).unwrap();
        half.write_all(br#"{"v":1,"file":"/half"#).unwrap();
        assert!(reader.poll().is_none());
        // Complete but never polled: waiting in the listener's backlog.
        let mut queued = UnixStream::connect(feed.socket_path()).unwrap();
        queued.write_all(b"{\"v\":1,\"file\":\"/queued.rs\"}\n").unwrap();

        reader.discard();
        let _ = half.write_all(b".rs\"}\n");
        assert!(reader.poll().is_none(), "nothing from before the discard is delivered");

        let mut after = UnixStream::connect(feed.socket_path()).unwrap();
        after.write_all(b"{\"v\":1,\"file\":\"/after.rs\"}\n").unwrap();
        assert_eq!(reader.poll().unwrap().file, "/after.rs");
    }

    #[test]
    fn a_late_old_connection_cannot_overwrite_a_newer_context() {
        let mut feed = EditorContextFeed::new().expect("bind the test feed");
        let mut reader = reader_for(&mut feed);
        let mut old = UnixStream::connect(feed.socket_path()).unwrap();
        old.write_all(br#"{"v":1,"file":"/old"#).unwrap();
        assert!(reader.poll().is_none());

        let mut new = UnixStream::connect(feed.socket_path()).unwrap();
        new.write_all(b"{\"v\":1,\"file\":\"/new.rs\"}\n").unwrap();
        assert_eq!(reader.poll().unwrap().file, "/new.rs");
        // Superseded connections may already have been closed; either way they cannot publish.
        let _ = old.write_all(b".rs\"}\n");
        assert!(reader.poll().is_none());
    }

    #[test]
    fn a_malformed_new_connection_does_not_supersede_an_older_valid_one() {
        let mut feed = EditorContextFeed::new().expect("bind the test feed");
        let mut reader = reader_for(&mut feed);
        let mut old = UnixStream::connect(feed.socket_path()).unwrap();
        old.write_all(br#"{"v":1,"file":"/valid"#).unwrap();
        assert!(reader.poll().is_none());
        let mut new = UnixStream::connect(feed.socket_path()).unwrap();
        new.write_all(b"broken json\n").unwrap();
        assert!(reader.poll().is_none());
        old.write_all(b".rs\"}\n").unwrap();
        assert_eq!(reader.poll().unwrap().file, "/valid.rs");
    }

    #[test]
    fn split_utf8_is_decoded_only_after_the_line_finishes() {
        let mut feed = EditorContextFeed::new().expect("bind the test feed");
        let mut reader = reader_for(&mut feed);
        let mut client = UnixStream::connect(feed.socket_path()).unwrap();
        client.write_all(b"{\"v\":1,\"file\":\"/\xe4").unwrap();
        assert!(reader.poll().is_none());
        client.write_all(b"\xb8\xad.rs\"}\n").unwrap();
        assert_eq!(reader.poll().unwrap().file, "/中.rs");
    }

    #[test]
    fn closed_partial_messages_are_discarded_but_valid_eof_framing_still_works() {
        let mut feed = EditorContextFeed::new().expect("bind the test feed");
        let mut reader = reader_for(&mut feed);
        let mut client = UnixStream::connect(feed.socket_path()).unwrap();
        client.write_all(br#"{"v":1,"file":"/partial"#).unwrap();
        assert!(reader.poll().is_none());
        drop(client);
        assert!(reader.poll().is_none());
        assert!(reader.pending.is_empty());
        let mut client = UnixStream::connect(feed.socket_path()).unwrap();
        client.write_all(br#"{"v":1,"file":"/eof.rs"}"#).unwrap();
        drop(client);
        assert_eq!(reader.poll().unwrap().file, "/eof.rs");
    }

    #[test]
    fn incomplete_connections_expire_without_a_real_time_sleep() {
        let mut feed = EditorContextFeed::new().expect("bind the test feed");
        let mut reader = reader_for(&mut feed);
        let mut client = UnixStream::connect(feed.socket_path()).unwrap();
        client.write_all(br#"{"v":1,"file":"/expired"#).unwrap();
        let accepted = Instant::now();
        assert!(reader.poll_at(accepted).is_none());
        assert!(reader.poll_at(accepted + MAX_CONNECTION_AGE).is_none());
        let _ = client.write_all(b".rs\"}\n");
        assert!(reader.poll_at(accepted + MAX_CONNECTION_AGE).is_none());
        assert!(reader.pending.is_empty());
    }

    #[test]
    fn connection_pressure_evicts_the_oldest_incomplete_message() {
        let mut feed = EditorContextFeed::new().expect("bind the test feed");
        let mut reader = reader_for(&mut feed);
        let mut old = UnixStream::connect(feed.socket_path()).unwrap();
        old.write_all(br#"{"v":1,"file":"/evicted"#).unwrap();
        assert!(reader.poll().is_none());
        let clients: Vec<_> = (0..MAX_PENDING_CONNECTIONS)
            .map(|_| UnixStream::connect(feed.socket_path()).unwrap())
            .collect();
        assert!(reader.poll().is_none());
        let _ = old.write_all(b".rs\"}\n");
        assert!(reader.poll().is_none());
        drop(clients);
    }

    #[test]
    fn accepting_a_backlog_is_bounded_per_poll() {
        let mut feed = EditorContextFeed::new().expect("bind the test feed");
        let mut reader = reader_for(&mut feed);
        let mut clients: Vec<_> = (0..=MAX_ACCEPTS_PER_POLL)
            .map(|_| UnixStream::connect(feed.socket_path()).unwrap())
            .collect();
        clients
            .last_mut()
            .unwrap()
            .write_all(b"{\"v\":1,\"file\":\"/last.rs\"}\n")
            .unwrap();
        assert!(
            reader.poll().is_none(),
            "the accept budget leaves the last connection for the next poll"
        );
        assert_eq!(reader.poll().unwrap().file, "/last.rs");
    }

    #[test]
    fn the_read_budget_keeps_large_messages_for_later_polls() {
        let mut feed = EditorContextFeed::new().expect("bind the test feed");
        let mut reader = reader_for(&mut feed);
        let mut client = UnixStream::connect(feed.socket_path()).unwrap();
        let file = "x".repeat(MAX_READ_BYTES_PER_POLL);
        // macOS's default AF_UNIX send buffer is a few KiB, so this one write would block before the
        // reader ever polls; give the client room for the whole message so the write completes first
        // and the first poll really sees more data than its budget.
        #[cfg(target_os = "macos")]
        {
            use std::os::fd::AsRawFd;
            let room: libc::c_int = (4 * MAX_READ_BYTES_PER_POLL) as libc::c_int;
            // SAFETY: the descriptor is open for the call and `room` outlives it.
            let set = unsafe {
                libc::setsockopt(
                    client.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_SNDBUF,
                    (&room as *const libc::c_int).cast(),
                    std::mem::size_of::<libc::c_int>() as libc::socklen_t,
                )
            };
            assert_eq!(set, 0, "raise the test client's send buffer");
        }
        writeln!(client, "{{\"v\":1,\"file\":\"{file}\"}}").unwrap();
        assert!(
            reader.poll().is_none(),
            "one poll must not consume more than its read budget"
        );
        assert_eq!(reader.poll().unwrap().file, file);
    }

    #[test]
    fn oversized_messages_are_closed_without_blocking_later_updates() {
        let mut feed = EditorContextFeed::new().expect("bind the test feed");
        let mut reader = reader_for(&mut feed);
        let mut client = UnixStream::connect(feed.socket_path()).unwrap();
        // Stream in chunks with a poll between them, so this test does not depend on the OS socket
        // send-buffer capacity. No newline is sent; the size limit must apply to partial lines too.
        for _ in 0..MAX_LINE_BYTES / 4096 {
            client.write_all(&[b'x'; 4096]).unwrap();
            assert!(reader.poll().is_none());
        }
        client.write_all(b"x").unwrap();
        assert!(reader.poll().is_none());
        assert!(
            reader.pending.is_empty(),
            "an oversized incomplete line must be dropped"
        );
        let mut good = UnixStream::connect(feed.socket_path()).unwrap();
        good.write_all(b"{\"v\":1,\"file\":\"/after-limit.rs\"}\n").unwrap();
        assert_eq!(reader.poll().unwrap().file, "/after-limit.rs");
    }
}
