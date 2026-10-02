//! A synchronous msgpack-RPC client to a running nvim, whose calls never block their caller.
//!
//! The panel talks to a user's own nvim over its `--listen` socket from the GTK thread, so no call
//! may wait on the socket, and none may fail by timing out: nvim queues requests while it waits for
//! a character, and a slow answer is still an answer. [`NvimLink::call`] therefore only queues the
//! encoded request for a writer thread and returns a [`Pending`] the caller polls; a reader thread
//! routes each reply to the call it belongs to. Only [`NvimLink::connect`] blocks, and it is for a
//! worker thread.

use std::collections::HashMap;
use std::fmt;
use std::io::{self, BufReader, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use rmpv::Value;

/// What nvim answered, or why there is no answer.
type Reply = Result<Value, RpcError>;

/// Why a call has no value.
#[derive(Debug, Clone, PartialEq)]
pub enum RpcError {
    /// nvim answered with an error; the text is its message.
    Nvim(String),
    /// The connection ended before the answer came, or was already closed.
    Closed,
    /// The request could not be encoded.
    Encode(String),
}

impl fmt::Display for RpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RpcError::Nvim(message) => f.write_str(message),
            RpcError::Closed => f.write_str("the connection to nvim closed"),
            RpcError::Encode(error) => write!(f, "could not encode the request: {error}"),
        }
    }
}

impl std::error::Error for RpcError {}

/// What nvim sent that was not the answer to a call.
#[derive(Debug, PartialEq)]
pub enum LinkEvent {
    /// `rpcnotify` (or a UI redraw, on a link that attached a UI).
    Notification { method: String, params: Vec<Value> },
    /// The connection ended, whoever ended it. Sent once, and last.
    Closed,
}

/// The answer to one [`NvimLink::call`], still on its way or already here.
///
/// Take it once: the value is delivered through a channel whose sender is gone afterwards, so a
/// second take says [`RpcError::Closed`].
pub struct Pending(Receiver<Reply>);

impl Pending {
    /// The answer if it has arrived. Never blocks. A connection that ended first is
    /// `Some(Err(Closed))`.
    pub fn try_take(&self) -> Option<Reply> {
        match self.0.try_recv() {
            Ok(reply) => Some(reply),
            Err(mpsc::TryRecvError::Empty) => None,
            Err(mpsc::TryRecvError::Disconnected) => Some(Err(RpcError::Closed)),
        }
    }

    /// Like [`Pending::try_take`], waiting up to `timeout`. For tests and worker threads only: the
    /// GTK thread must poll.
    pub fn wait(&self, timeout: Duration) -> Option<Reply> {
        match self.0.recv_timeout(timeout) {
            Ok(reply) => Some(reply),
            Err(mpsc::RecvTimeoutError::Timeout) => None,
            Err(mpsc::RecvTimeoutError::Disconnected) => Some(Err(RpcError::Closed)),
        }
    }

    fn failed(error: RpcError) -> Pending {
        let (tx, rx) = mpsc::channel();
        let _ = tx.send(Err(error));
        Pending(rx)
    }
}

/// What the two threads and every clone of the link share.
struct Shared {
    /// Calls waiting for their answer, by request id. Once `alive` is false and this has been
    /// drained, nothing may be inserted: the insert checks `alive` under this same lock.
    waiting: Mutex<HashMap<u64, Sender<Reply>>>,
    /// Frames for the writer thread. The one `Sender` lives here and is never cloned, so taking it
    /// out is exactly "no more frames", which is what lets the writer drain and stop. Never held
    /// together with `waiting`.
    tx: Mutex<Option<Sender<Vec<u8>>>>,
    alive: Arc<AtomicBool>,
    /// A clone of the socket, for `shutdown` only.
    stream: UnixStream,
    next_id: AtomicU64,
    channel_id: u64,
}

/// When the last [`NvimLink`] clone goes, the queued frames are written and the connection ends;
/// otherwise a forgotten link would keep two threads and a descriptor for as long as nvim lives.
struct LinkInner(Arc<Shared>);

impl Drop for LinkInner {
    fn drop(&mut self) {
        lock(&self.0.tx).take();
    }
}

/// A connection to one nvim. Cheap to clone; every clone is the same connection.
#[derive(Clone)]
pub struct NvimLink(Arc<LinkInner>);

const _: fn() = || {
    fn is_send_sync<T: Send + Sync>() {}
    is_send_sync::<NvimLink>();
};

/// A poisoned lock still holds a consistent map or queue here (nothing panics mid-update), and a
/// caller on the GTK thread must not panic because some other thread did.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The read half of the socket. While `deadline` is set, every `read` waits no longer than what is
/// left of it, so a peer that drips one frame in pieces cannot stretch a bounded wait: the limit is
/// on the whole, not on each read.
struct Source {
    stream: UnixStream,
    deadline: Option<Instant>,
}

impl Read for Source {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if let Some(deadline) = self.deadline {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(io::ErrorKind::TimedOut.into());
            }
            self.stream.set_read_timeout(Some(remaining))?;
        }
        self.stream.read(buf)
    }
}

/// What this client answers to a request from nvim: it serves none, and nvim waits for an answer
/// to a request, so say so.
fn refusal(id: Value) -> Vec<u8> {
    let answer = Value::Array(vec![
        Value::from(1),
        id,
        Value::from("eitri-panel does not take requests"),
        Value::Nil,
    ]);
    let mut frame = Vec::new();
    // Encoding into a `Vec` cannot fail.
    let _ = rmpv::encode::write_value(&mut frame, &answer);
    frame
}

/// The answer to the request that opens every connection. Request ids for calls start at 1.
const HANDSHAKE_ID: u64 = 0;

/// The most bytes one frame from nvim may take. Bytes on the wire are not the whole cost: a decoded
/// value is a few dozen bytes even when its encoding was one (an array of nils), so a frame at this
/// cap can still decode to some hundreds of MiB, and a larger cap would scale that up with it. The
/// largest frame this link legitimately reads, the `nvim_get_api_info` answer, is tens of KiB; it
/// never carries buffer text.
const MAX_FRAME: u64 = 16 << 20;

/// One frame's read: hands the decoder at most [`MAX_FRAME`] bytes, then fails with
/// `InvalidData` instead of reporting an end of file, so the link says why it ended. Build one per
/// frame. It caps the slice it hands to the buffered reader, so only bytes the decoder consumed are
/// counted; what the buffer already holds of the next frame is left for that frame.
struct Capped<'a, R> {
    inner: &'a mut R,
    left: u64,
}

impl<'a, R: Read> Capped<'a, R> {
    fn frame(inner: &'a mut R) -> Capped<'a, R> {
        Capped { inner, left: MAX_FRAME }
    }
}

impl<R: Read> Read for Capped<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if self.left == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("a frame from nvim is larger than {} MiB", MAX_FRAME >> 20),
            ));
        }
        let take = buf.len().min(usize::try_from(self.left).unwrap_or(usize::MAX));
        let read = self.inner.read(&mut buf[..take])?;
        self.left -= read as u64;
        Ok(read)
    }
}

impl NvimLink {
    /// Connect to nvim's `--listen` socket and ask `nvim_get_api_info` (which nvim answers even
    /// while it waits for a key) for this connection's channel id. Blocks up to `timeout` for the
    /// whole of it; call it on a worker thread only. The `Receiver` carries what nvim sends that is
    /// not an answer.
    pub fn connect(addr: &Path, timeout: Duration) -> Result<(NvimLink, Receiver<LinkEvent>), String> {
        Self::connect_expecting(addr, timeout, crate::panel_control::euid())
    }

    /// [`NvimLink::connect`] to a socket that must be held by a process of `uid`. The socket's path
    /// was checked before this, but a directory others can write to lets the file be swapped
    /// between that check and the connect, so the process actually on the other end is what counts.
    fn connect_expecting(addr: &Path, timeout: Duration, uid: u32) -> Result<(NvimLink, Receiver<LinkEvent>), String> {
        let deadline = Instant::now() + timeout;
        let no_answer = || {
            format!(
                "connect {}: no answer within {} ms",
                addr.display(),
                timeout.as_millis()
            )
        };
        let failed = |error: &dyn fmt::Display| format!("connect {}: {error}", addr.display());

        // A blocking `connect(2)` to a socket whose accept queue is full waits for room, possibly
        // forever; this one gives up at the deadline.
        let stream = crate::instance_dir::connect_by(addr, deadline).map_err(|e| match e.kind() {
            io::ErrorKind::TimedOut => no_answer(),
            _ => failed(&e),
        })?;
        // Before a byte is written: an impostor learns nothing, not even the handshake.
        match crate::panel_control::peer_uid(&stream) {
            Ok(peer) if peer == uid => {}
            Ok(peer) => return Err(failed(&format!("the socket is held by another user (uid {peer})"))),
            Err(e) => return Err(failed(&format!("cannot tell who holds the socket: {e}"))),
        }

        // One buffered reader serves the handshake and then the reader thread, so bytes it already
        // took off the socket (a notification right behind the answer) are not lost.
        let mut reader = BufReader::new(Source {
            stream,
            deadline: Some(deadline),
        });
        let (events_tx, events_rx) = mpsc::channel();

        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(no_answer());
        }
        reader
            .get_ref()
            .stream
            .set_write_timeout(Some(remaining))
            .map_err(|e| failed(&e))?;
        let request = Value::Array(vec![
            Value::from(0),
            Value::from(HANDSHAKE_ID),
            Value::from("nvim_get_api_info"),
            Value::Array(vec![]),
        ]);
        let mut bytes = Vec::new();
        rmpv::encode::write_value(&mut bytes, &request).map_err(|e| failed(&e))?;
        let write_failed = |e: io::Error| match e.kind() {
            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => no_answer(),
            _ => failed(&e),
        };
        (&reader.get_ref().stream).write_all(&bytes).map_err(write_failed)?;

        let channel_id = loop {
            // `Source` bounds every read by the deadline, so a frame sent in slow pieces ends here too.
            let value = rmpv::decode::read_value(&mut Capped::frame(&mut reader)).map_err(|e| match e.kind() {
                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => no_answer(),
                io::ErrorKind::UnexpectedEof => failed(&"nvim closed the connection"),
                _ => failed(&e),
            })?;
            match classify(&value) {
                Frame::Response {
                    id: HANDSHAKE_ID,
                    error,
                    result,
                } => {
                    if !error.is_nil() {
                        return Err(format!("nvim_get_api_info: {}", error_message(error)));
                    }
                    break result
                        .as_array()
                        .and_then(|info| info.first())
                        .and_then(Value::as_u64)
                        .ok_or("nvim_get_api_info: no channel id in the answer")?;
                }
                // nvim broadcasts `rpcnotify(0, ...)` to every channel, so a plugin's can land
                // before the answer; keep it for the caller instead of dropping it.
                Frame::Notification { method, params } => {
                    let _ = events_tx.send(LinkEvent::Notification { method, params });
                }
                // A plugin may `rpcrequest` this channel before the handshake answer (from
                // `ChanOpen`, say); it is blocked until it gets one, so refuse it as the reader
                // thread would.
                Frame::Request { id } => {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        return Err(no_answer());
                    }
                    let stream = &reader.get_ref().stream;
                    stream.set_write_timeout(Some(remaining)).map_err(|e| failed(&e))?;
                    (&*stream).write_all(&refusal(id)).map_err(write_failed)?;
                }
                _ => {}
            }
        };
        reader.get_mut().deadline = None;
        let stream = &reader.get_ref().stream;
        stream.set_read_timeout(None).map_err(|e| failed(&e))?;
        stream.set_write_timeout(None).map_err(|e| failed(&e))?;

        let link = start(reader, channel_id, events_tx).map_err(|e| failed(&e))?;
        Ok((link, events_rx))
    }

    /// A link over an already connected socket; the other end plays nvim.
    #[cfg(test)]
    fn from_stream(stream: UnixStream, channel_id: u64) -> (NvimLink, Receiver<LinkEvent>) {
        let (events_tx, events_rx) = mpsc::channel();
        let source = Source { stream, deadline: None };
        let link = start(BufReader::new(source), channel_id, events_tx).expect("threads start");
        (link, events_rx)
    }

    /// The channel id nvim gave this connection: what `rpcnotify` and `rpcrequest` address.
    pub fn channel_id(&self) -> u64 {
        self.shared().channel_id
    }

    /// Ask nvim to run `method`. Never touches the socket and never blocks: the request is queued
    /// for the writer thread, and there is no timeout, because nvim answers after a key press when
    /// it is waiting for one.
    ///
    /// Nothing bounds the queue: a peer that stops reading makes it, and the channel behind
    /// [`LinkEvent`], grow for as long as the caller keeps calling. A caller must stop once
    /// [`NvimLink::is_alive`] is false or [`LinkEvent::Closed`] has arrived. A writer stuck on a
    /// peer that never reads outlives the last dropped clone until nvim exits; only
    /// [`NvimLink::close`] wakes it.
    pub fn call(&self, method: &str, params: Vec<Value>) -> Pending {
        let shared = self.shared();
        let id = shared.next_id.fetch_add(1, Ordering::SeqCst);
        let request = Value::Array(vec![
            Value::from(0),
            Value::from(id),
            Value::from(method),
            Value::Array(params),
        ]);
        let mut frame = Vec::new();
        if let Err(e) = rmpv::encode::write_value(&mut frame, &request) {
            return Pending::failed(RpcError::Encode(e.to_string()));
        }

        let (reply_tx, reply_rx) = mpsc::channel();
        {
            let mut waiting = lock(&shared.waiting);
            // The reader clears `alive` and drains under this lock, so an entry inserted here is
            // either answered, or drained, or never inserted: it cannot be stranded.
            if !shared.alive.load(Ordering::SeqCst) {
                return Pending(reply_rx);
            }
            waiting.insert(id, reply_tx);
        }
        let queued = match lock(&shared.tx).as_ref() {
            Some(frames) => frames.send(frame).is_ok(),
            None => false,
        };
        if !queued {
            // Dropping the sender is what tells the `Pending` the connection is gone.
            lock(&shared.waiting).remove(&id);
        }
        Pending(reply_rx)
    }

    /// `nvim_exec_lua`: run `code` with `args` as its `...`.
    pub fn exec_lua(&self, code: &str, args: Vec<Value>) -> Pending {
        self.call("nvim_exec_lua", vec![Value::from(code), Value::Array(args)])
    }

    /// False once the connection has ended or been closed.
    pub fn is_alive(&self) -> bool {
        self.shared().alive.load(Ordering::SeqCst)
    }

    /// End the connection now. Frames still queued are not written; calls waiting for an answer
    /// end `Err(Closed)`, and so does every later call.
    pub fn close(&self) {
        let shared = self.shared();
        shared.alive.store(false, Ordering::SeqCst);
        // Shutdown first, so "now" does not push what is queued. It wakes a reader blocked in
        // `read` and a writer blocked in `write`.
        let _ = shared.stream.shutdown(Shutdown::Both);
        lock(&shared.tx).take();
    }

    /// End the connection once everything queued has been written. Never blocks the caller. The
    /// bytes reach the socket; whether nvim still executes them once it sees the connection close
    /// is up to nvim, which may drop requests queued for a channel that is gone. Calls made after
    /// this end `Err(Closed)`.
    pub fn close_when_flushed(&self) {
        lock(&self.shared().tx).take();
    }

    fn shared(&self) -> &Shared {
        &(self.0).0
    }
}

/// One decoded frame, by what the protocol says it is.
enum Frame {
    Request {
        id: Value,
    },
    Response {
        id: u64,
        error: Value,
        result: Value,
    },
    Notification {
        method: String,
        params: Vec<Value>,
    },
    /// Well formed msgpack that is no frame we know: skipped, not fatal.
    Other,
}

fn classify(value: &Value) -> Frame {
    let Some(items) = value.as_array() else {
        return Frame::Other;
    };
    match (items.first().and_then(Value::as_u64), items.len()) {
        (Some(0), 4) if items[1].is_u64() => Frame::Request { id: items[1].clone() },
        (Some(1), 4) => match items[1].as_u64() {
            Some(id) => Frame::Response {
                id,
                error: items[2].clone(),
                result: items[3].clone(),
            },
            None => Frame::Other,
        },
        (Some(2), 3) => match (items[1].as_str(), items[2].as_array()) {
            (Some(method), Some(params)) => Frame::Notification {
                method: method.to_owned(),
                params: params.clone(),
            },
            _ => Frame::Other,
        },
        _ => Frame::Other,
    }
}

/// nvim's error is `[type, message]`; a bare string is accepted too.
fn error_message(error: Value) -> String {
    if let Some(message) = error.as_array().and_then(|e| e.get(1)).and_then(Value::as_str) {
        return message.to_owned();
    }
    if let Some(message) = error.as_str() {
        return message.to_owned();
    }
    error.to_string()
}

/// Start the two threads on a connection whose handshake is done.
fn start(reader: BufReader<Source>, channel_id: u64, events: Sender<LinkEvent>) -> io::Result<NvimLink> {
    let (frames_tx, frames_rx) = mpsc::channel::<Vec<u8>>();
    let alive = Arc::new(AtomicBool::new(true));
    let shared = Arc::new(Shared {
        waiting: Mutex::new(HashMap::new()),
        tx: Mutex::new(Some(frames_tx)),
        alive: alive.clone(),
        stream: reader.get_ref().stream.try_clone()?,
        next_id: AtomicU64::new(1),
        channel_id,
    });

    // The writer must not hold `shared`: it owns the only `Receiver`, and `shared` owns the only
    // `Sender`, so holding it would keep that `Sender` alive and the writer could never see that
    // every sender is gone.
    let write_stream = reader.get_ref().stream.try_clone()?;
    thread::Builder::new()
        .name("nvim-rpc-writer".to_owned())
        .spawn(move || write_frames(frames_rx, write_stream, alive))?;

    let for_reader = shared.clone();
    thread::Builder::new()
        .name("nvim-rpc-reader".to_owned())
        .spawn(move || read_frames(reader, for_reader, events))?;

    Ok(NvimLink(Arc::new(LinkInner(shared))))
}

/// Write queued frames in order until every sender is gone (the queue is delivered before the
/// channel reports that) or a write fails, then shut the socket down so the reader sees the end.
fn write_frames(frames: Receiver<Vec<u8>>, mut stream: UnixStream, alive: Arc<AtomicBool>) {
    while let Ok(frame) = frames.recv() {
        if stream.write_all(&frame).is_err() {
            alive.store(false, Ordering::SeqCst);
            break;
        }
    }
    // After a `close()` the socket is already shut down, which is not an error worth reporting.
    let _ = stream.shutdown(Shutdown::Both);
}

fn read_frames(mut reader: BufReader<Source>, shared: Arc<Shared>, events: Sender<LinkEvent>) {
    // Only an unreadable stream (end of file, bad encoding, a frame over the cap) ends the loop. A
    // frame that decodes but means nothing to us is skipped, and so is a notification nobody is
    // listening for: the replies behind them are still owed.
    while let Ok(value) = rmpv::decode::read_value(&mut Capped::frame(&mut reader)) {
        match classify(&value) {
            Frame::Response { id, error, result } => {
                let waiter = lock(&shared.waiting).remove(&id);
                if let Some(waiter) = waiter {
                    let reply = if error.is_nil() {
                        Ok(result)
                    } else {
                        Err(RpcError::Nvim(error_message(error)))
                    };
                    let _ = waiter.send(reply);
                }
            }
            Frame::Notification { method, params } => {
                let _ = events.send(LinkEvent::Notification { method, params });
            }
            Frame::Request { id } => {
                if let Some(frames) = lock(&shared.tx).as_ref() {
                    let _ = frames.send(refusal(id));
                }
            }
            Frame::Other => {}
        }
    }

    {
        let mut waiting = lock(&shared.waiting);
        shared.alive.store(false, Ordering::SeqCst);
        for (_, waiter) in waiting.drain() {
            let _ = waiter.send(Err(RpcError::Closed));
        }
    }
    // Let the writer end too (it may be idle in `recv`, or blocked writing to a peer that is gone).
    let _ = shared.stream.shutdown(Shutdown::Both);
    lock(&shared.tx).take();
    let _ = events.send(LinkEvent::Closed);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufRead;

    const T: Duration = Duration::from_secs(2);

    type Linked = (NvimLink, Receiver<LinkEvent>, UnixStream, BufReader<UnixStream>);

    /// A link over one half of a socket pair, and the other half standing in for nvim.
    fn linked() -> Linked {
        let (ours, fake) = UnixStream::pair().unwrap();
        fake.set_read_timeout(Some(T)).unwrap();
        let reader = BufReader::new(fake.try_clone().unwrap());
        let (link, events) = NvimLink::from_stream(ours, 3);
        (link, events, fake, reader)
    }

    /// The next request the link wrote: `(id, method, params)`.
    fn read_request(reader: &mut BufReader<UnixStream>) -> (u64, String, Vec<Value>) {
        let value = rmpv::decode::read_value(reader).expect("a request from the link");
        let items = value.as_array().expect("an array").clone();
        assert_eq!(items[0].as_u64(), Some(0), "a request has type 0: {items:?}");
        (
            items[1].as_u64().unwrap(),
            items[2].as_str().unwrap().to_owned(),
            items[3].as_array().unwrap().clone(),
        )
    }

    fn write_value(fake: &mut UnixStream, value: Value) {
        let mut bytes = Vec::new();
        rmpv::encode::write_value(&mut bytes, &value).unwrap();
        fake.write_all(&bytes).unwrap();
    }

    fn response(id: u64, err: Value, result: Value) -> Value {
        Value::Array(vec![Value::from(1), Value::from(id), err, result])
    }

    #[test]
    fn a_call_gets_its_reply() {
        let (link, _events, mut fake, mut reader) = linked();
        let pending = link.call("nvim_eval", vec![Value::from("1+1")]);
        let (id, method, params) = read_request(&mut reader);
        assert_eq!(method, "nvim_eval");
        assert_eq!(params, vec![Value::from("1+1")]);
        assert!(pending.try_take().is_none(), "no reply yet");
        write_value(&mut fake, response(id, Value::Nil, Value::from(2)));
        assert_eq!(pending.wait(T), Some(Ok(Value::from(2))));
    }

    #[test]
    fn responses_out_of_order_reach_their_own_callers() {
        let (link, _events, mut fake, mut reader) = linked();
        let first = link.call("a", vec![]);
        let second = link.call("b", vec![]);
        let (id1, _, _) = read_request(&mut reader);
        let (id2, _, _) = read_request(&mut reader);
        assert_ne!(id1, id2);
        write_value(&mut fake, response(id2, Value::Nil, Value::from("two")));
        write_value(&mut fake, response(id1, Value::Nil, Value::from("one")));
        assert_eq!(first.wait(T), Some(Ok(Value::from("one"))));
        assert_eq!(second.wait(T), Some(Ok(Value::from("two"))));
    }

    #[test]
    fn a_frame_split_across_two_writes_decodes() {
        let (link, _events, mut fake, mut reader) = linked();
        let pending = link.call("a", vec![]);
        let (id, _, _) = read_request(&mut reader);
        let mut bytes = Vec::new();
        rmpv::encode::write_value(
            &mut bytes,
            &response(id, Value::Nil, Value::from("a long enough string")),
        )
        .unwrap();
        let (head, tail) = bytes.split_at(bytes.len() / 2);
        fake.write_all(head).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        assert!(pending.try_take().is_none(), "half a frame is not a reply");
        fake.write_all(tail).unwrap();
        assert_eq!(pending.wait(T), Some(Ok(Value::from("a long enough string"))));
    }

    #[test]
    fn an_unknown_response_id_is_ignored() {
        let (link, _events, mut fake, mut reader) = linked();
        let pending = link.call("a", vec![]);
        let (id, _, _) = read_request(&mut reader);
        write_value(&mut fake, response(999, Value::Nil, Value::from(5)));
        write_value(&mut fake, response(id, Value::Nil, Value::from("real")));
        assert_eq!(pending.wait(T), Some(Ok(Value::from("real"))));
        assert!(link.is_alive());
    }

    #[test]
    fn a_well_formed_frame_of_the_wrong_shape_does_not_stop_the_reader() {
        let (link, events, mut fake, mut reader) = linked();
        let pending = link.call("a", vec![]);
        let (id, _, _) = read_request(&mut reader);
        write_value(&mut fake, Value::from("not an array"));
        write_value(&mut fake, Value::Array(vec![Value::from(9), Value::Nil]));
        write_value(
            &mut fake,
            Value::Array(vec![Value::from(1), Value::from("id"), Value::Nil, Value::Nil]),
        );
        write_value(
            &mut fake,
            Value::Array(vec![Value::from(2), Value::from(1), Value::Array(vec![])]),
        );
        // A notification nobody listens to any more must not end the reading either.
        drop(events);
        write_value(&mut fake, response(id, Value::Nil, Value::from("still reading")));
        assert_eq!(pending.wait(T), Some(Ok(Value::from("still reading"))));
        assert!(link.is_alive());
    }

    #[test]
    fn eof_ends_every_pending_call_with_closed_and_sends_one_closed_event() {
        let (link, events, fake, mut reader) = linked();
        let first = link.call("a", vec![]);
        let second = link.call("b", vec![]);
        read_request(&mut reader);
        read_request(&mut reader);
        drop(reader);
        drop(fake);
        assert_eq!(first.wait(T), Some(Err(RpcError::Closed)));
        assert_eq!(second.wait(T), Some(Err(RpcError::Closed)));
        assert_eq!(events.recv_timeout(T), Ok(LinkEvent::Closed));
        assert_eq!(events.recv_timeout(T), Err(mpsc::RecvTimeoutError::Disconnected));
        assert!(!link.is_alive());
    }

    #[test]
    fn a_notification_is_delivered_as_an_event() {
        let (_link, events, mut fake, _reader) = linked();
        write_value(
            &mut fake,
            Value::Array(vec![
                Value::from(2),
                Value::from("t"),
                Value::Array(vec![Value::from(1), Value::from("a")]),
            ]),
        );
        assert_eq!(
            events.recv_timeout(T),
            Ok(LinkEvent::Notification {
                method: "t".to_owned(),
                params: vec![Value::from(1), Value::from("a")],
            })
        );
    }

    #[test]
    fn a_request_from_nvim_is_answered_with_an_error_so_nvim_never_waits() {
        let (_link, _events, mut fake, mut reader) = linked();
        write_value(
            &mut fake,
            Value::Array(vec![
                Value::from(0),
                Value::from(7),
                Value::from("x"),
                Value::Array(vec![]),
            ]),
        );
        let answer = rmpv::decode::read_value(&mut reader).unwrap();
        let items = answer.as_array().unwrap();
        assert_eq!(items.len(), 4);
        assert_eq!(items[0].as_u64(), Some(1));
        assert_eq!(items[1].as_u64(), Some(7));
        assert!(!items[2].is_nil(), "an error, so nvim does not take it for a result");
        assert!(items[3].is_nil());
    }

    #[test]
    fn an_nvim_error_reaches_the_caller_as_its_message() {
        let (link, _events, mut fake, mut reader) = linked();
        let typed = link.call("a", vec![]);
        let bare = link.call("b", vec![]);
        let (id1, _, _) = read_request(&mut reader);
        let (id2, _, _) = read_request(&mut reader);
        write_value(
            &mut fake,
            response(id1, Value::Array(vec![Value::from(0), Value::from("boom")]), Value::Nil),
        );
        write_value(&mut fake, response(id2, Value::from("boom"), Value::Nil));
        assert_eq!(typed.wait(T), Some(Err(RpcError::Nvim("boom".to_owned()))));
        assert_eq!(bare.wait(T), Some(Err(RpcError::Nvim("boom".to_owned()))));
    }

    #[test]
    fn call_never_blocks_when_the_peer_never_reads() {
        let (link, _events, _fake, _reader) = linked();
        let big = Value::from("x".repeat(64 * 1024));
        let args: Vec<Vec<Value>> = (0..200).map(|_| vec![big.clone()]).collect();
        let (done_tx, done_rx) = mpsc::channel();
        let for_thread = link.clone();
        std::thread::spawn(move || {
            let started = Instant::now();
            let pendings: Vec<Pending> = args.into_iter().map(|a| for_thread.call("m", a)).collect();
            let _ = done_tx.send((started.elapsed(), pendings));
        });
        // A call that blocks makes this fail rather than hang the suite.
        let (elapsed, pendings) = done_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("200 calls returned");
        assert!(elapsed < Duration::from_millis(250), "200 calls took {elapsed:?}");
        assert!(pendings.iter().all(|p| p.try_take().is_none()));
        link.close();
    }

    #[test]
    fn close_when_flushed_delivers_what_was_queued() {
        let (link, _events, _fake, mut reader) = linked();
        let pendings: Vec<Pending> = (0..3).map(|n| link.call("m", vec![Value::from(n)])).collect();
        link.close_when_flushed();
        for expected in 0..3 {
            let (_, _, params) = read_request(&mut reader);
            assert_eq!(params, vec![Value::from(expected)]);
        }
        let mut rest = Vec::new();
        assert_eq!(reader.read_until(0, &mut rest).unwrap(), 0, "then EOF");
        for pending in &pendings {
            assert_eq!(pending.wait(T), Some(Err(RpcError::Closed)));
        }
    }

    #[test]
    fn a_call_after_the_link_closed_ends_closed_at_once() {
        let (link, events, _fake, _reader) = linked();
        link.close();
        assert_eq!(events.recv_timeout(T), Ok(LinkEvent::Closed));
        assert_eq!(link.call("a", vec![]).try_take(), Some(Err(RpcError::Closed)));

        let (link, events, _fake, _reader) = linked();
        link.close_when_flushed();
        assert_eq!(link.call("a", vec![]).try_take(), Some(Err(RpcError::Closed)));
        assert_eq!(events.recv_timeout(T), Ok(LinkEvent::Closed));
    }

    #[test]
    fn dropping_the_last_clone_closes_the_connection() {
        let (link, events, _fake, mut reader) = linked();
        let other = link.clone();
        drop(link);
        let mut rest = Vec::new();
        assert!(events.try_recv().is_err(), "a clone is still alive");
        drop(other);
        assert_eq!(
            reader.read_until(0, &mut rest).unwrap(),
            0,
            "the connection was shut down"
        );
        assert_eq!(events.recv_timeout(T), Ok(LinkEvent::Closed));
    }

    fn listener_dir(case: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("rpc-{}-{case}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn connect_fails_within_the_timeout_when_the_listener_never_answers() {
        let dir = listener_dir("silent");
        let sock = agent::socket_path::in_dir(&dir, "f.sock").unwrap();
        // Bound but never accepted: the kernel completes the connect from the backlog, so only the
        // handshake read can time out.
        let _listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        let started = Instant::now();
        let error = NvimLink::connect(&sock, Duration::from_millis(300))
            .err()
            .expect("no answer");
        assert!(
            started.elapsed() < Duration::from_millis(600),
            "took {:?}",
            started.elapsed()
        );
        assert!(error.contains("no answer within 300 ms"), "{error}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn connect_reads_the_channel_id_past_an_early_notification() {
        let dir = listener_dir("early");
        let sock = agent::socket_path::in_dir(&dir, "f.sock").unwrap();
        let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        let fake = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(T)).unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let (id, method, _) = read_request(&mut reader);
            assert_eq!((id, method.as_str()), (0, "nvim_get_api_info"));
            write_value(
                &mut stream,
                Value::Array(vec![Value::from(2), Value::from("x"), Value::Array(vec![])]),
            );
            write_value(
                &mut stream,
                response(0, Value::Nil, Value::Array(vec![Value::from(7), Value::Map(vec![])])),
            );
            // Stay connected until the test is done with the link.
            let mut rest = Vec::new();
            let _ = reader.read_until(0, &mut rest);
        });
        let (link, events) = NvimLink::connect(&sock, T).unwrap();
        assert_eq!(link.channel_id(), 7);
        assert_eq!(
            events.recv_timeout(T),
            Ok(LinkEvent::Notification {
                method: "x".to_owned(),
                params: vec![]
            })
        );
        link.close();
        fake.join().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn connect_refuses_a_request_that_arrives_before_the_handshake_answer() {
        let dir = listener_dir("early-request");
        let sock = agent::socket_path::in_dir(&dir, "f.sock").unwrap();
        let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        let fake = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(T)).unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let (id, method, _) = read_request(&mut reader);
            assert_eq!((id, method.as_str()), (0, "nvim_get_api_info"));
            write_value(
                &mut stream,
                Value::Array(vec![
                    Value::from(0),
                    Value::from(41),
                    Value::from("x"),
                    Value::Array(vec![]),
                ]),
            );
            // The plugin is blocked on its request: the refusal must come before we answer.
            let answer = rmpv::decode::read_value(&mut reader).expect("an answer to the request");
            let items = answer.as_array().unwrap();
            assert_eq!(items[0].as_u64(), Some(1));
            assert_eq!(items[1].as_u64(), Some(41));
            assert!(
                !items[2].is_nil(),
                "an error, so the plugin does not take it for a result"
            );
            write_value(
                &mut stream,
                response(0, Value::Nil, Value::Array(vec![Value::from(7), Value::Map(vec![])])),
            );
            let mut rest = Vec::new();
            let _ = reader.read_until(0, &mut rest);
        });
        let (link, _events) = NvimLink::connect(&sock, T).unwrap();
        assert_eq!(link.channel_id(), 7);
        link.close();
        fake.join().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn connect_deadline_covers_a_frame_that_arrives_in_slow_pieces() {
        let dir = listener_dir("drip");
        let sock = agent::socket_path::in_dir(&dir, "f.sock").unwrap();
        let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        let fake = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(T)).unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let _ = read_request(&mut reader);
            // The start of an array of four, then one byte at a time, each well inside the timeout
            // and together far past it. The peer stops when the link hangs up.
            let mut bytes = vec![0x94];
            bytes.extend(std::iter::repeat_n(0xc0, 3));
            let _ = stream.write_all(&bytes[..1]);
            for byte in &bytes[1..] {
                std::thread::sleep(Duration::from_millis(150));
                if stream.write_all(&[*byte]).is_err() {
                    return;
                }
            }
            std::thread::sleep(Duration::from_millis(150));
        });
        let started = Instant::now();
        let error = NvimLink::connect(&sock, Duration::from_millis(300))
            .err()
            .expect("the frame never completed in time");
        assert!(
            started.elapsed() < Duration::from_millis(450),
            "took {:?}",
            started.elapsed()
        );
        assert!(error.contains("no answer within 300 ms"), "{error}");
        fake.join().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn connect_refuses_a_socket_held_by_another_uid() {
        let dir = listener_dir("uid");
        let sock = agent::socket_path::in_dir(&dir, "f.sock").unwrap();
        let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        let fake = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(T)).unwrap();
            let mut got = Vec::new();
            let _ = stream.read_to_end(&mut got);
            got.len()
        });
        let other = crate::panel_control::euid().wrapping_add(1);
        let error = NvimLink::connect_expecting(&sock, T, other)
            .err()
            .expect("a peer of another uid is refused");
        assert!(error.contains("another user"), "{error}");
        assert_eq!(fake.join().unwrap(), 0, "nothing was sent to the impostor");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The start of a bin32 that claims `len` bytes.
    fn bin32_header(len: u32) -> Vec<u8> {
        let mut bytes = vec![0xc6];
        bytes.extend(len.to_be_bytes());
        bytes
    }

    /// Writes `head`, then zeros until the other end hangs up. Rust ignores SIGPIPE, so the end
    /// is an `Err` from `write`, not a signal.
    fn flood(mut stream: UnixStream, head: Vec<u8>) {
        if stream.write_all(&head).is_err() {
            return;
        }
        let zeros = vec![0u8; 64 * 1024];
        while stream.write_all(&zeros).is_ok() {}
    }

    #[test]
    fn an_oversized_frame_closes_the_link() {
        let (link, events, fake, _reader) = linked();
        let pending = link.call("x", vec![]);
        let writer = std::thread::spawn(move || flood(fake, bin32_header(MAX_FRAME as u32)));
        assert_eq!(events.recv_timeout(Duration::from_secs(5)), Ok(LinkEvent::Closed));
        assert_eq!(pending.wait(T), Some(Err(RpcError::Closed)));
        writer.join().unwrap();
    }

    #[test]
    fn frames_below_the_cap_each_get_a_fresh_budget() {
        let (link, _events, mut fake, mut reader) = linked();
        let pendings: Vec<Pending> = (0..3).map(|_| link.call("m", vec![])).collect();
        let ids: Vec<u64> = (0..3).map(|_| read_request(&mut reader).0).collect();
        let half = vec![7u8; (MAX_FRAME / 2) as usize];
        let for_writer = half.clone();
        let writer = std::thread::spawn(move || {
            for id in ids {
                write_value(&mut fake, response(id, Value::Nil, Value::Binary(for_writer.clone())));
            }
            fake
        });
        for pending in &pendings {
            assert_eq!(
                pending.wait(Duration::from_secs(10)),
                Some(Ok(Value::Binary(half.clone())))
            );
        }
        assert!(link.is_alive());
        let _fake = writer.join().unwrap();
    }

    #[test]
    fn connect_deadline_covers_an_oversized_handshake_frame() {
        let dir = listener_dir("big-handshake");
        let sock = agent::socket_path::in_dir(&dir, "f.sock").unwrap();
        let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        let fake = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(T)).unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let _ = read_request(&mut reader);
            let mut head = vec![0x94, 0x01, 0x00, 0xc0];
            head.extend(bin32_header(MAX_FRAME as u32));
            flood(stream, head);
        });
        let started = Instant::now();
        let error = NvimLink::connect(&sock, T).err().expect("an oversized answer");
        assert!(started.elapsed() < T, "took {:?}", started.elapsed());
        assert!(error.contains("larger than"), "{error}");
        fake.join().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A listener that never accepts, with its queue full: a blocking `connect(2)` to it would
    /// sleep until the queue has room, which is never. Linux only: macOS refuses such a connect at
    /// once, so there is nothing to wait out.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_full_backlog_times_out_and_leaves_no_thread() {
        use crate::instance_dir::tests::{raw_bind_listen, raw_nonblocking_connect};
        let dir = listener_dir("backlog");
        let sock = agent::socket_path::in_dir(&dir, "f.sock").unwrap();
        let listener_fd = raw_bind_listen(&sock, 0);
        let mut client_fds = Vec::new();
        loop {
            match raw_nonblocking_connect(&sock) {
                Ok(fd) => client_fds.push(fd),
                Err(errno) if errno == libc::EAGAIN || errno == libc::EWOULDBLOCK => break,
                Err(errno) => panic!("unexpected connect() errno {errno}"),
            }
            assert!(client_fds.len() < 4096, "the accept queue never filled");
        }

        let started = Instant::now();
        let error = NvimLink::connect(&sock, Duration::from_millis(300))
            .err()
            .expect("a full queue never answers");
        let elapsed = started.elapsed();

        // Counting threads would race the tests running beside this one, so look for the one thing a
        // connect left waiting on a helper thread would show: a thread named for it, still there
        // ("nvim-rpc-connect", which `comm` cuts to 15 bytes).
        let lingering: Vec<String> = std::fs::read_dir("/proc/self/task")
            .unwrap()
            .filter_map(|task| std::fs::read_to_string(task.ok()?.path().join("comm")).ok())
            .map(|comm| comm.trim_end().to_owned())
            .filter(|comm| comm == "nvim-rpc-connec")
            .collect();

        for fd in client_fds {
            // SAFETY: each fd came from the fill loop above and is closed exactly once, here.
            unsafe { libc::close(fd) };
        }
        // SAFETY: `listener_fd` came from `raw_bind_listen` above and is closed exactly once.
        unsafe { libc::close(listener_fd) };
        let _ = std::fs::remove_dir_all(&dir);

        assert!(error.contains("no answer within 300 ms"), "{error}");
        assert!(elapsed < Duration::from_millis(600), "took {elapsed:?}");
        assert!(lingering.is_empty(), "a connect thread was left behind: {lingering:?}");
    }

    #[test]
    fn connect_to_a_missing_socket_names_the_address() {
        let error = NvimLink::connect(Path::new("/nonexistent/eitri/nvim-address"), T)
            .err()
            .unwrap();
        assert!(
            error.starts_with("connect /nonexistent/eitri/nvim-address: "),
            "{error}"
        );
    }
}
