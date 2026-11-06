//! What `serve` owes one phone, and the thread that writes it.
//!
//! Every frame for the phone waits in one queue, whoever made it: the session itself (`pong`, a refusal, a `result`),
//! the agents feed, or an Eitri process's `rv-` connection. A frame gets its `seq` (counting from 1, with no gap) and
//! the `ack` (the last `seq` the phone sent) when it is written, so a frame dropped before that costs no number.
//!
//! The queue is bounded: a frame that would take it past [`PHONE_OUT_CAP_BYTES`] while it already holds something
//! drops it whole (that frame too), and a write that moves no byte for [`PHONE_WRITE_STALL`] does the same. Either
//! way every source's later frames are refused from then on, the session is told to cut its `rv-` connections and its
//! feed, and once the writer has caught up (the queue empty and nothing half written) it is told to make them again:
//! the phone gets a fresh greeting and snapshot of each tab it follows, and never a gap. A frame always fits an empty
//! queue, so what one phone costs is bounded by the cap and one frame.
//!
//! Which source may still queue is decided under the queue's lock: a `rv-` connection only while it is live and, for
//! a frame about a tab, only while the session follows that tab through it; the feed only while it is the current
//! one. A frame read from a connection the session has let go is dropped however late it comes.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io::{self, Write};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use eitri_core::remote_rv::CHUNK_BYTES;

/// The bytes of frames one phone may have waiting; one more drops them all.
pub const PHONE_OUT_CAP_BYTES: usize = 1024 * 1024;
/// How long a write to the phone may move no byte before its queue is dropped.
pub const PHONE_WRITE_STALL: Duration = Duration::from_secs(120);
/// The longest message written as one WebSocket frame; a longer one goes as continuation frames of
/// [`CHUNK_BYTES`].
pub const PHONE_FRAME_BYTES: usize = 1024 * 1024;

const OP_TEXT: u8 = 0x1;
const OP_CONTINUATION: u8 = 0x0;
const FIN: u8 = 0x80;

/// Who queues a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// The session itself.
    Local,
    /// The `rv-` connection with this id.
    Upstream(u64),
    /// The agents feed with this id.
    Feed(u64),
}

/// What became of a frame offered to the queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pushed {
    Queued,
    /// Its source is no longer heard.
    Dropped,
    /// It would have passed the cap: the queue was dropped, this frame with it.
    Overflowed,
    /// The session is closed.
    Closed,
}

/// What the writer is handed.
#[derive(Debug, PartialEq, Eq)]
pub enum Next {
    /// Bytes to write: a data message (stamped and framed) or a control frame. `last`: the connection ends after it.
    Write { bytes: Vec<u8>, data: bool, last: bool },
    /// The queue was dropped and the writer has caught up: the session makes its subscriptions again.
    Resync,
    /// Nothing more is written.
    Closed,
}

/// What the queue tells the session, from whichever thread found it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Notice {
    /// The queue was dropped: cut every `rv-` connection and the feed.
    Overflowed,
}

enum Item {
    Text(String),
    Control { bytes: Vec<u8>, last: bool },
}

impl Item {
    fn len(&self) -> usize {
        match self {
            Item::Text(text) => text.len(),
            Item::Control { bytes, .. } => bytes.len(),
        }
    }
}

#[derive(Default)]
struct State {
    queue: VecDeque<Item>,
    bytes: usize,
    seq_out: u64,
    /// The last `seq` the phone sent, echoed as `ack`.
    seq_in: u64,
    live: BTreeSet<u64>,
    /// The tabs followed, and the connection each is followed through.
    routes: BTreeMap<String, u64>,
    feed: Option<u64>,
    resync_owed: bool,
    closed: bool,
    high_water: usize,
    overflows: u64,
}

/// One phone's queue. Shared by the session, its `rv-` readers, its feed and its writer.
pub struct Outbound {
    state: Mutex<State>,
    ready: Condvar,
    cap: usize,
    notify: Box<dyn Fn(Notice) + Send + Sync>,
}

impl Outbound {
    /// A queue of at most `cap` bytes; `notify` hears of an overflow, on the thread that caused it and under the
    /// queue's lock, so the session is told before the writer can find the queue drained and owe it a resync. `notify`
    /// must not call back into the queue.
    pub fn new(cap: usize, notify: impl Fn(Notice) + Send + Sync + 'static) -> Arc<Outbound> {
        Arc::new(Outbound {
            state: Mutex::new(State::default()),
            ready: Condvar::new(),
            cap,
            notify: Box::new(notify),
        })
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Offers `text`, one JSON object, from `source`; `key` is the tab it is about, if any.
    pub fn push(&self, source: Source, key: Option<&str>, text: String) -> Pushed {
        let state = self.lock();
        if state.closed {
            return Pushed::Closed;
        }
        let heard = match source {
            Source::Local => true,
            Source::Feed(id) => state.feed == Some(id),
            Source::Upstream(id) => {
                state.live.contains(&id) && key.is_none_or(|key| state.routes.get(key) == Some(&id))
            }
        };
        if !heard {
            return Pushed::Dropped;
        }
        self.enqueue(state, Item::Text(text))
    }

    /// Queues a control frame (a pong, a close); `last` ends the connection after it.
    pub fn push_control(&self, bytes: Vec<u8>, last: bool) -> Pushed {
        let state = self.lock();
        if state.closed {
            return Pushed::Closed;
        }
        self.enqueue(state, Item::Control { bytes, last })
    }

    fn enqueue(&self, mut state: MutexGuard<'_, State>, item: Item) -> Pushed {
        let len = item.len();
        if !state.queue.is_empty() && state.bytes + len > self.cap {
            drop_queue(&mut state);
            (self.notify)(Notice::Overflowed);
            return Pushed::Overflowed;
        }
        state.bytes += len;
        state.high_water = state.high_water.max(state.bytes);
        state.queue.push_back(item);
        drop(state);
        self.ready.notify_all();
        Pushed::Queued
    }

    /// The writer found a write that moved no byte for the stall deadline: drop as an overflow does.
    pub fn stalled(&self) {
        let mut state = self.lock();
        if state.closed {
            return;
        }
        drop_queue(&mut state);
        (self.notify)(Notice::Overflowed);
    }

    /// Blocks until there is something for the writer.
    pub fn next(&self) -> Next {
        let mut state = self.lock();
        loop {
            if state.closed {
                return Next::Closed;
            }
            if let Some(item) = state.queue.pop_front() {
                state.bytes -= item.len();
                return match item {
                    Item::Text(text) => {
                        state.seq_out += 1;
                        let stamped = stamp(&text, state.seq_out, state.seq_in);
                        Next::Write {
                            bytes: encode_message(stamped.as_bytes()),
                            data: true,
                            last: false,
                        }
                    }
                    Item::Control { bytes, last } => Next::Write {
                        bytes,
                        data: false,
                        last,
                    },
                };
            }
            if state.resync_owed {
                state.resync_owed = false;
                return Next::Resync;
            }
            state = self.ready.wait(state).unwrap_or_else(|poisoned| poisoned.into_inner());
        }
    }

    /// Nothing more is queued or written; what waits is dropped.
    pub fn close(&self) {
        let mut state = self.lock();
        state.closed = true;
        state.queue.clear();
        state.bytes = 0;
        drop(state);
        self.ready.notify_all();
    }

    pub fn is_closed(&self) -> bool {
        self.lock().closed
    }

    /// Hears the `rv-` connection `id` from now on.
    pub fn hear(&self, id: u64) {
        self.lock().live.insert(id);
    }

    /// Stops hearing the connection `id`, and every tab followed through it.
    pub fn forget(&self, id: u64) {
        let mut state = self.lock();
        state.live.remove(&id);
        state.routes.retain(|_, via| *via != id);
    }

    /// Frames about `key` are heard from the connection `via` only (`None`: from none).
    pub fn route(&self, key: &str, via: Option<u64>) {
        let mut state = self.lock();
        match via {
            Some(id) => state.routes.insert(key.to_owned(), id),
            None => state.routes.remove(key),
        };
    }

    /// The feed heard from now on (`None`: none).
    pub fn set_feed(&self, feed: Option<u64>) {
        self.lock().feed = feed;
    }

    /// The phone's frame `seq` arrived: later frames carry it as `ack`. `ack` is what it says it has read; `Err` when
    /// either goes backwards or names a frame never sent.
    pub fn heard(&self, seq: u64, ack: u64, last_ack: &mut u64) -> Result<(), &'static str> {
        let mut state = self.lock();
        if seq <= state.seq_in {
            return Err("seq must grow");
        }
        if ack < *last_ack || ack > state.seq_out {
            return Err("ack names a frame not sent");
        }
        state.seq_in = seq;
        *last_ack = ack;
        Ok(())
    }

    /// The most bytes the queue has held, and how many times it was dropped.
    pub fn gauge(&self) -> (usize, u64) {
        let state = self.lock();
        (state.high_water, state.overflows)
    }
}

fn drop_queue(state: &mut State) {
    state.queue.clear();
    state.bytes = 0;
    state.live.clear();
    state.routes.clear();
    state.feed = None;
    state.resync_owed = true;
    state.overflows += 1;
}

/// `text`, a JSON object, with `seq` and `ack` as its first fields.
fn stamp(text: &str, seq: u64, ack: u64) -> String {
    let rest = text.strip_prefix('{').unwrap_or(text).trim_start();
    if rest.starts_with('}') {
        format!(r#"{{"seq":{seq},"ack":{ack}{rest}"#)
    } else {
        format!(r#"{{"seq":{seq},"ack":{ack},{rest}"#)
    }
}

/// One text message as server frames: whole up to [`PHONE_FRAME_BYTES`], else a first frame and continuation frames of
/// [`CHUNK_BYTES`].
pub fn encode_message(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 16);
    if payload.len() <= PHONE_FRAME_BYTES {
        frame_into(&mut out, FIN | OP_TEXT, payload);
        return out;
    }
    let pieces = payload.chunks(CHUNK_BYTES).count();
    for (at, piece) in payload.chunks(CHUNK_BYTES).enumerate() {
        let opcode = if at == 0 { OP_TEXT } else { OP_CONTINUATION };
        let fin = if at + 1 == pieces { FIN } else { 0 };
        frame_into(&mut out, fin | opcode, piece);
    }
    out
}

fn frame_into(out: &mut Vec<u8>, first: u8, payload: &[u8]) {
    out.push(first);
    match payload.len() {
        len @ 0..=125 => out.push(len as u8),
        len @ 126..=65535 => {
            out.push(126);
            out.extend((len as u16).to_be_bytes());
        }
        len => {
            out.push(127);
            out.extend((len as u64).to_be_bytes());
        }
    }
    out.extend_from_slice(payload);
}

/// Writes `bytes` whole. A write that moves no byte for `stall` (the stream's write timeout), or a short one that
/// took that long, calls `on_stall` once and goes on: the frame must still end, or the stream is no longer WebSocket.
/// `Err` when the stream fails or `gone` says the session ended.
pub fn write_through(
    stream: &mut dyn Write,
    mut bytes: &[u8],
    stall: Duration,
    on_stall: &dyn Fn(),
    gone: &dyn Fn() -> bool,
) -> io::Result<()> {
    let mut stalled = false;
    while !bytes.is_empty() {
        if gone() {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "the session ended"));
        }
        let started = Instant::now();
        match stream.write(bytes) {
            Ok(0) => return Err(io::Error::new(io::ErrorKind::WriteZero, "the phone took nothing")),
            Ok(moved) => {
                bytes = &bytes[moved..];
                if !bytes.is_empty() && started.elapsed() >= stall && !stalled {
                    stalled = true;
                    on_stall();
                }
            }
            Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {
                if !stalled {
                    stalled = true;
                    on_stall();
                }
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    stream.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use eitri_core::ide_server::frames::{Decoder, Inbound};
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn queue(cap: usize) -> (Arc<Outbound>, Arc<AtomicUsize>) {
        let told = Arc::new(AtomicUsize::new(0));
        let counted = told.clone();
        let out = Outbound::new(cap, move |_| {
            counted.fetch_add(1, Ordering::SeqCst);
        });
        (out, told)
    }

    /// The JSON of the next data message, through a client's decoder.
    fn next_json(out: &Outbound) -> serde_json::Value {
        let Next::Write { bytes, data: true, .. } = out.next() else {
            panic!("a data message was due")
        };
        let mut decoder = Decoder::new(64 << 20, 64 << 20);
        let mut got = Vec::new();
        // A server frame is unmasked; the decoder reads client frames, so mask it with a zero key.
        decoder.feed(&mask_zero(&bytes), &mut got);
        let [Inbound::Text(text)] = got.as_slice() else {
            panic!("one text message, got {got:?}")
        };
        serde_json::from_slice(text).unwrap()
    }

    /// The same frames with the mask bit set and a zero key, as a client would send them.
    fn mask_zero(mut bytes: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        while !bytes.is_empty() {
            out.push(bytes[0]);
            let (len, head) = match bytes[1] {
                126 => (u16::from_be_bytes([bytes[2], bytes[3]]) as usize, 4),
                127 => (u64::from_be_bytes(bytes[2..10].try_into().unwrap()) as usize, 10),
                n => (n as usize, 2),
            };
            out.push(bytes[1] | 0x80);
            out.extend_from_slice(&bytes[2..head]);
            out.extend_from_slice(&[0, 0, 0, 0]);
            out.extend_from_slice(&bytes[head..head + len]);
            bytes = &bytes[head + len..];
        }
        out
    }

    #[test]
    fn frames_get_seq_and_ack_when_written() {
        let (out, _) = queue(PHONE_OUT_CAP_BYTES);
        let mut last_ack = 0;
        out.heard(1, 0, &mut last_ack).unwrap();
        assert_eq!(
            out.push(Source::Local, None, r#"{"v":1,"kind":"pong"}"#.into()),
            Pushed::Queued
        );
        let first = next_json(&out);
        assert_eq!((first["seq"].as_u64(), first["ack"].as_u64()), (Some(1), Some(1)));
        assert_eq!(first["kind"], "pong");
        out.heard(2, 1, &mut last_ack).unwrap();
        out.push(Source::Local, None, r#"{"v":1,"kind":"pong"}"#.into());
        let second = next_json(&out);
        assert_eq!((second["seq"].as_u64(), second["ack"].as_u64()), (Some(2), Some(2)));
        assert!(out.heard(2, 1, &mut last_ack).is_err(), "a seq repeated");
        assert!(out.heard(3, 3, &mut last_ack).is_err(), "an ack past what was sent");
        assert!(out.heard(3, 0, &mut last_ack).is_err(), "an ack going back");
    }

    #[test]
    fn a_frame_from_a_connection_let_go_is_dropped() {
        let (out, _) = queue(PHONE_OUT_CAP_BYTES);
        let frame = || r#"{"v":1,"kind":"events","key":"k-1"}"#.to_owned();
        assert_eq!(out.push(Source::Upstream(7), Some("k-1"), frame()), Pushed::Dropped);
        out.hear(7);
        assert_eq!(
            out.push(Source::Upstream(7), Some("k-1"), frame()),
            Pushed::Dropped,
            "not followed through 7"
        );
        out.route("k-1", Some(7));
        assert_eq!(out.push(Source::Upstream(7), Some("k-1"), frame()), Pushed::Queued);
        assert_eq!(
            out.push(Source::Upstream(7), None, r#"{"v":1,"kind":"theme"}"#.into()),
            Pushed::Queued
        );
        out.route("k-1", Some(8));
        assert_eq!(
            out.push(Source::Upstream(7), Some("k-1"), frame()),
            Pushed::Dropped,
            "moved to 8"
        );
        out.forget(7);
        assert_eq!(
            out.push(Source::Upstream(7), None, r#"{"v":1,"kind":"theme"}"#.into()),
            Pushed::Dropped
        );
        assert_eq!(out.push(Source::Feed(1), None, "{}".into()), Pushed::Dropped);
        out.set_feed(Some(1));
        assert_eq!(out.push(Source::Feed(1), None, r#"{"v":1}"#.into()), Pushed::Queued);
    }

    #[test]
    fn past_the_cap_the_queue_drops_whole_and_a_resync_follows_once_caught_up() {
        let (out, told) = queue(100);
        out.hear(1);
        out.route("k-1", Some(1));
        let frame = |n: usize| format!(r#"{{"v":1,"kind":"events","key":"k-1","pad":"{}"}}"#, "x".repeat(n));
        assert_eq!(
            out.push(Source::Upstream(1), Some("k-1"), frame(150)),
            Pushed::Queued,
            "fits an empty queue"
        );
        assert_eq!(out.push(Source::Upstream(1), Some("k-1"), frame(1)), Pushed::Overflowed);
        assert_eq!(told.load(Ordering::SeqCst), 1);
        assert_eq!(
            out.push(Source::Upstream(1), Some("k-1"), frame(1)),
            Pushed::Dropped,
            "the source was cut"
        );
        assert_eq!(out.next(), Next::Resync, "nothing was left to write");
        let (high, overflows) = out.gauge();
        assert!(high <= 100 + frame(150).len(), "{high}");
        assert_eq!(overflows, 1);
        out.push(Source::Local, None, r#"{"v":1,"kind":"pong"}"#.into());
        assert_eq!(next_json(&out)["seq"], 1, "the dropped frames took no seq");
    }

    /// The session pauses on the overflow notice and resumes on the writer's resync; a resync that overtook the notice
    /// would be ignored and the late notice would pause the phone for good.
    #[test]
    fn the_overflow_is_told_before_the_writer_can_see_the_resync() {
        let order = Arc::new(Mutex::new(Vec::new()));
        let (telling, told) = std::sync::mpsc::channel();
        let telling = Mutex::new(telling);
        let noted = order.clone();
        let out = Outbound::new(10, move |_| {
            let _ = telling.lock().unwrap().send(());
            // A notice that takes its time to arrive.
            std::thread::sleep(Duration::from_millis(300));
            noted.lock().unwrap().push("overflowed");
        });
        assert_eq!(out.push(Source::Local, None, "x".repeat(10)), Pushed::Queued);
        let pusher = {
            let out = out.clone();
            std::thread::spawn(move || out.push(Source::Local, None, "y".into()))
        };
        told.recv().unwrap();
        let writer = {
            let (out, order) = (out.clone(), order.clone());
            std::thread::spawn(move || {
                let next = out.next();
                order.lock().unwrap().push("resync");
                next
            })
        };
        assert_eq!(pusher.join().unwrap(), Pushed::Overflowed);
        assert_eq!(writer.join().unwrap(), Next::Resync);
        assert_eq!(*order.lock().unwrap(), vec!["overflowed", "resync"]);
    }

    #[test]
    fn a_stall_drops_like_an_overflow() {
        let (out, told) = queue(PHONE_OUT_CAP_BYTES);
        out.set_feed(Some(3));
        out.push(Source::Feed(3), None, r#"{"v":1,"kind":"agents_row"}"#.into());
        out.stalled();
        assert_eq!(told.load(Ordering::SeqCst), 1);
        assert_eq!(out.next(), Next::Resync);
        assert_eq!(out.push(Source::Feed(3), None, "{}".into()), Pushed::Dropped);
        out.close();
        assert_eq!(out.next(), Next::Closed);
        assert_eq!(out.push(Source::Local, None, "{}".into()), Pushed::Closed);
    }

    #[test]
    fn a_message_over_the_frame_cap_goes_as_continuation_frames() {
        let payload = vec![b'a'; PHONE_FRAME_BYTES + CHUNK_BYTES + 7];
        let bytes = encode_message(&payload);
        assert_eq!(bytes[0], OP_TEXT, "the first frame is not final");
        let mut decoder = Decoder::new(payload.len(), payload.len());
        let mut got = Vec::new();
        decoder.feed(&mask_zero(&bytes), &mut got);
        assert_eq!(got, vec![Inbound::Text(payload.clone())]);
        let frames = {
            let (mut n, mut rest) = (0, &bytes[..]);
            while !rest.is_empty() {
                let (len, head) = match rest[1] {
                    127 => (u64::from_be_bytes(rest[2..10].try_into().unwrap()) as usize, 10),
                    126 => (u16::from_be_bytes([rest[2], rest[3]]) as usize, 4),
                    short => (short as usize, 2),
                };
                assert!(len <= CHUNK_BYTES);
                rest = &rest[head + len..];
                n += 1;
            }
            n
        };
        assert_eq!(frames, 4);
        let small = encode_message(b"{}");
        assert_eq!(small, vec![FIN | OP_TEXT, 2, b'{', b'}']);
    }

    #[test]
    fn a_short_write_that_lasted_the_deadline_is_the_stall() {
        struct Slow(Vec<u8>);
        impl Write for Slow {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                if self.0.len().is_multiple_of(2) {
                    std::thread::sleep(Duration::from_millis(20));
                }
                self.0.push(buf[0]);
                Ok(1)
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let stalls = AtomicUsize::new(0);
        let mut slow = Slow(Vec::new());
        write_through(
            &mut slow,
            b"abcd",
            Duration::from_millis(10),
            &|| {
                stalls.fetch_add(1, Ordering::SeqCst);
            },
            &|| false,
        )
        .unwrap();
        assert_eq!(slow.0, b"abcd", "the frame still ends");
        assert_eq!(stalls.load(Ordering::SeqCst), 1, "told once per frame");
    }
}
