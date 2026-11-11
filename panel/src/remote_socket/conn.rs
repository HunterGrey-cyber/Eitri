//! One connection's threads and what bounds them: the gauge of what its writer holds, the deadlines, the reader that
//! hands lines to the tick and the writer that writes what the tick queued.
//!
//! A connection is closed when its peer sends no line for [`Deadlines::idle`] (a quiet peer keeps it with `ping`), when a
//! line is not finished within [`RV_LINE_WITHIN`] of its first byte, when its writer owes frames and writes nothing for
//! [`Deadlines::write_stall`], when its answers waiting unread pass [`CONTROL_LINES_MAX`] or [`CONTROL_BYTES_MAX`], and
//! when it breaks a rule of the grammar. Whatever closes it, the reader shuts the stream both ways as it ends, which
//! also frees a writer held in a write the peer never reads, and the tick forgets the connection and its observers.

use super::*;

/// The bytes one connection's writer may hold before its observers' frames wait in the tab set.
pub const WRITER_BYTES: usize = 1024 * 1024;

/// The answers (`result`, `refused`, `pong`, an errand's answer) one connection may have waiting; one more closes it.
pub const CONTROL_LINES_MAX: usize = 64;

/// How long a refusal to a connection over the cap may take to write.
const REFUSAL_WITHIN: Duration = Duration::from_millis(100);

/// The deadlines a connection is held to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Deadlines {
    /// No line for this long closes the connection.
    pub(super) idle: Duration,
    /// A write that moves no byte for this long closes the connection.
    pub(super) write_stall: Duration,
}

impl Default for Deadlines {
    fn default() -> Deadlines {
        Deadlines {
            idle: RV_IDLE,
            write_stall: RV_WRITE_STALL,
        }
    }
}

/// Why a connection was closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CloseWhy {
    /// The peer closed, or the stream failed.
    Peer,
    /// No line came within the idle deadline.
    Idle,
    /// The writer moved nothing within the write deadline.
    WriteStall,
    /// A line broke the grammar: too long, too slow, another version, or not a line of it.
    Rule,
}

/// What a connection's writer holds, shared by the tick, which adds, and the writer, which takes away.
#[derive(Default)]
pub(super) struct Gauge {
    /// Every byte queued and not yet written, answers included.
    pub(super) bytes: AtomicUsize,
    /// Answers queued and not yet written.
    pub(super) control: AtomicUsize,
    /// Their bytes.
    pub(super) control_bytes: AtomicUsize,
    /// The tick found the writer full and left frames waiting: the writer raises the wake when it has room.
    pub(super) starved: AtomicBool,
    /// The writer gave up on a write that moved nothing for the write deadline.
    pub(super) stalled: AtomicBool,
}

impl Gauge {
    /// Whether one more answer of `bytes` (its newline counted) fits under both caps.
    pub(super) fn control_fits(&self, bytes: usize) -> bool {
        self.control.load(Ordering::SeqCst) < CONTROL_LINES_MAX
            && self.control_bytes.load(Ordering::SeqCst).saturating_add(bytes) <= CONTROL_BYTES_MAX
    }
}

/// A line for a connection's writer.
pub(super) struct Out {
    pub(super) line: String,
    pub(super) control: bool,
}

/// One connection as the tick holds it.
pub(super) struct Conn {
    /// A clone of the connection, to close it from the tick.
    pub(super) stream: UnixStream,
    pub(super) out: mpsc::Sender<Out>,
    pub(super) gauge: Arc<Gauge>,
    pub(super) observers: BTreeMap<RowKey, ObserverId>,
}

impl Conn {
    pub(super) fn queue(&self, line: String, control: bool) {
        let bytes = line.len() + 1;
        self.gauge.bytes.fetch_add(bytes, Ordering::SeqCst);
        if control {
            self.gauge.control.fetch_add(1, Ordering::SeqCst);
            self.gauge.control_bytes.fetch_add(bytes, Ordering::SeqCst);
        }
        let mut line = line;
        line.push('\n');
        // A writer that ended has closed the connection; its reader reports it.
        let _ = self.out.send(Out { line, control });
    }

    /// Whether the writer has room for frames. Marks it starved first, so a writer that drains in between either is
    /// seen to have room here or sees the mark and raises the wake.
    pub(super) fn has_room(&self) -> bool {
        self.gauge.starved.store(true, Ordering::SeqCst);
        if self.gauge.bytes.load(Ordering::SeqCst) < WRITER_BYTES {
            self.gauge.starved.store(false, Ordering::SeqCst);
            true
        } else {
            false
        }
    }

    /// Frames wait for this writer: marks it starved, so it raises the wake once it has room, and raises the wake now
    /// when it already has room (it may have drained before the mark).
    pub(super) fn wait_for_room(&self, wake: &agent::UiWake) {
        self.gauge.starved.store(true, Ordering::SeqCst);
        if self.gauge.bytes.load(Ordering::SeqCst) <= WRITER_BYTES / 2
            && self.gauge.starved.swap(false, Ordering::SeqCst)
        {
            wake();
        }
    }

    pub(super) fn close(&self) {
        shut_both_ways(&self.stream);
    }
}

/// Starts a peer's writer and reader, or refuses it when the cap is reached. `false` once the socket's owner is gone.
pub(super) fn start_connection(stream: UnixStream, shared: &Arc<Shared>) -> bool {
    if shared.live.load(Ordering::SeqCst) >= RV_MAX_CONNECTIONS {
        let _ = stream.set_write_timeout(Some(REFUSAL_WITHIN));
        let _ =
            (&stream).write_all(format!("{}\n", refused_line(None, eitri_core::remote_rv::RV_REFUSED_FULL)).as_bytes());
        return true;
    }
    let (Ok(writer), Ok(tick)) = (stream.try_clone(), stream.try_clone()) else {
        return true;
    };
    // A write that moves no byte for this long fails, and the writer closes the connection.
    if writer.set_write_timeout(Some(shared.deadlines.write_stall)).is_err() {
        return true;
    }
    let conn = shared.next_conn.fetch_add(1, Ordering::Relaxed);
    let gauge = Arc::new(Gauge::default());
    let (out, lines) = mpsc::channel();
    let writer_gauge = Arc::clone(&gauge);
    let writer_wake = Arc::clone(&shared.wake);
    let write_stall = shared.deadlines.write_stall;
    if std::thread::Builder::new()
        .name("eitri-remote-write".into())
        .spawn(move || write_loop(&writer, &lines, &writer_gauge, &writer_wake, write_stall))
        .is_err()
    {
        return true;
    }
    shared.live.fetch_add(1, Ordering::SeqCst);
    let opened = Inbound::Opened {
        conn,
        stream: tick,
        out,
        gauge: Arc::clone(&gauge),
    };
    if shared.inbound.send(opened).is_err() {
        shared.live.fetch_sub(1, Ordering::SeqCst);
        return false;
    }
    // Sent after the owner's drop had taken what was queued: nobody will close it, so it is closed here; its reader
    // then ends at once.
    if shared.stopped.load(Ordering::SeqCst) {
        shut_both_ways(&stream);
    }
    let reader_shared = Arc::clone(shared);
    let reader_stream = match stream.try_clone() {
        Ok(s) => s,
        Err(_) => {
            closed(shared, conn, CloseWhy::Peer);
            return true;
        }
    };
    if std::thread::Builder::new()
        .name("eitri-remote-read".into())
        .spawn(move || {
            let ended = read_loop(&reader_stream, conn, &reader_shared);
            // Both ways, whatever ended the reading, so a writer held in a write the peer never reads is let go.
            shut_both_ways(&reader_stream);
            let why = if gauge.stalled.load(Ordering::SeqCst) {
                CloseWhy::WriteStall
            } else {
                ended
            };
            closed(&reader_shared, conn, why);
        })
        .is_err()
    {
        shut_both_ways(&stream);
        closed(shared, conn, CloseWhy::Peer);
    }
    true
}

/// Shuts both directions of `stream`, which frees a writer held in a write the peer never reads (its write fails).
/// macOS refuses `SHUT_RDWR` with ENOTCONN once the peer has shut its own writing side, and then shuts nothing, so the
/// writer stayed held; there the writing side is shut first, alone, which macOS always does, and then the reading side.
pub(super) fn shut_both_ways(stream: &UnixStream) {
    #[cfg(target_os = "macos")]
    {
        let _ = stream.shutdown(Shutdown::Write);
        let _ = stream.shutdown(Shutdown::Read);
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = stream.shutdown(Shutdown::Both);
    }
}

/// A connection ended: the tick forgets it and its observers, and the cap has room for one more.
fn closed(shared: &Shared, conn: u64, why: CloseWhy) {
    let _ = shared.inbound.send(Inbound::Closed { conn, why });
    shared.live.fetch_sub(1, Ordering::SeqCst);
    (shared.wake)();
}

/// A connection's writer: writes each queued line whole, and raises the wake when it has room again for a tick that
/// found it full. A write that moves nothing within the write deadline marks the gauge stalled and closes the
/// connection. Ends when the connection fails or the tick forgets it.
fn write_loop(stream: &UnixStream, lines: &Receiver<Out>, gauge: &Gauge, wake: &agent::UiWake, stall: Duration) {
    for out in lines {
        let wrote = write_whole(stream, out.line.as_bytes(), stall);
        gauge.bytes.fetch_sub(out.line.len(), Ordering::SeqCst);
        if out.control {
            gauge.control.fetch_sub(1, Ordering::SeqCst);
            gauge.control_bytes.fetch_sub(out.line.len(), Ordering::SeqCst);
        }
        if let Err(e) = wrote {
            if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) {
                gauge.stalled.store(true, Ordering::SeqCst);
            }
            shut_both_ways(stream);
            return;
        }
        if gauge.bytes.load(Ordering::SeqCst) <= WRITER_BYTES / 2 && gauge.starved.swap(false, Ordering::SeqCst) {
            wake();
        }
    }
}

/// Writes `bytes` whole, or fails `TimedOut` once no byte has moved for `stall` (the stream's send timeout). The
/// kernel restarts that timeout at each byte it moves, but a send that moved some and then waited it out returns the
/// count, not an error; sending the rest would wait a whole deadline more for a peer that reads nothing. So a short
/// send that lasted the deadline is the stall. (A slack of a tenth covers the kernel rounding the timeout to its
/// clock ticks.) A short send cut by a signal instead is carried on.
fn write_whole(stream: &UnixStream, mut bytes: &[u8], stall: Duration) -> io::Result<()> {
    while !bytes.is_empty() {
        let began = Instant::now();
        match (&*stream).write(bytes) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(n) if n < bytes.len() && began.elapsed() >= stall - stall / 10 => {
                return Err(io::ErrorKind::TimedOut.into());
            }
            Ok(n) => bytes = &bytes[n..],
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// A connection's reader: one line at a time to the tick, until the peer closes, sends nothing for the idle deadline,
/// or breaks a rule (a line over [`RV_MAX_LINE`], one not finished within [`RV_LINE_WITHIN`], another protocol
/// version, anything that does not parse), which closes the connection. Why it ended.
fn read_loop(stream: &UnixStream, conn: u64, shared: &Shared) -> CloseWhy {
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = vec![0u8; 64 * 1024];
    // How much of `buf` is known to hold no newline, so a long line is searched once, not once per read.
    let mut scanned = 0;
    // When the first byte of the line now in `buf` came.
    let mut started: Option<Instant> = None;
    // When the last whole line ended, or the connection opened.
    let mut last_line = Instant::now();
    loop {
        while let Some(at) = buf[scanned..].iter().position(|&b| b == b'\n') {
            let end = scanned + at;
            scanned = 0;
            let line: Vec<u8> = buf.drain(..=end).collect();
            let line = &line[..line.len() - 1];
            if line.len() > RV_MAX_LINE || !hand_on(line, conn, shared) {
                shut_both_ways(stream);
                return CloseWhy::Rule;
            }
            last_line = Instant::now();
            started = (!buf.is_empty()).then(Instant::now);
        }
        scanned = buf.len();
        if buf.len() > RV_MAX_LINE {
            shut_both_ways(stream);
            return CloseWhy::Rule;
        }
        // A line begun must end within its own deadline; between lines, the idle deadline runs.
        let (until, why) = match started {
            Some(at) => (at + RV_LINE_WITHIN, CloseWhy::Rule),
            None => (last_line + shared.deadlines.idle, CloseWhy::Idle),
        };
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            shut_both_ways(stream);
            return why;
        }
        // A zero timeout is an error to set, and means "no timeout" to some platforms.
        if stream
            .set_read_timeout(Some(left.max(Duration::from_millis(1))))
            .is_err()
        {
            shut_both_ways(stream);
            return CloseWhy::Peer;
        }
        match (&*stream).read(&mut chunk) {
            Ok(0) => return CloseWhy::Peer,
            Ok(n) => {
                if buf.is_empty() {
                    started = Some(Instant::now());
                }
                buf.extend_from_slice(&chunk[..n]);
            }
            Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {}
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return CloseWhy::Peer,
        }
    }
}
