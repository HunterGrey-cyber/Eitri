//! One phone's connection after its 101, and the [`Hub`] every connection of a `serve` shares.
//!
//! The phone's first frame must be `hello`, within [`HELLO_WITHIN`]. With a token, the token is checked against the
//! device store and the device's grant kept; a token that does not match, a `hello` that comes late, or any other
//! first frame closes the connection with no reply. Without a token the phone is enrolling: only `enroll` may follow,
//! sent to the window it names. A `protocol` this build does not speak gets `bye{reason: "protocol"}` and a close.
//!
//! After that ([`route`](super::route)): `subscribe_rows` starts an agents feed of the session's own; `subscribe`,
//! `unsubscribe`, `more` and `act` go to the `rv-` socket of the window that owns the tab, `revoke_self` and
//! `set_push_endpoint` to any window's (the store is shared), `ping` is answered here. An `act` from a `watch` device is
//! refused here, and the window refuses it again on its own. The session keeps one `rv-` connection per window it
//! talks to and follows each tab through the window that owns it now: when the tab's row moves to another window, the
//! session unsubscribes at the old one and subscribes at the new; when the window says `gone` with `moved`, it looks
//! for the row wherever it now is, the same window included once the move has had time to land; when a window closes
//! the connection, the session connects again and subscribes again, so the phone gets a fresh greeting and snapshot.
//! A connection the window closes before it answered a subscription (a window at its connection cap turns one away
//! with a `refused` naming no tab) counts as a failed try: the tries are [`RECONNECT_PAUSE`] apart and at most
//! [`RECONNECT_TRIES`], and that refusal is not the phone's to read.
//!
//! Four threads serve a connection: this one, which decides everything; a reader of the phone's frames; a writer
//! ([`outbound`](super::outbound)); and one reader per `rv-` connection, which passes a tab's frames straight to the
//! writer's queue while the session follows that tab through that connection. A feed adds its own reader thread.
//! The queue's overflow cuts every `rv-` connection and the feed, and its resync makes them again.
//!
//! [`Hub`] holds what the connections share: the rows index (one agents reader for the whole server), the device
//! store's view and its watch, which closes the connections of a device that is revoked or whose grant fell, and the
//! report of who is connected that every window gets ([`report`](super::report)).

use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use eitri_core::agents::{Reader, ReaderEvent, SCHEMA_V};
use eitri_core::ide_server::frames::{close_frame, pong_frame, Decoder, Inbound};
use eitri_core::remote_devices::Grant;
use eitri_core::remote_rv::{
    parse_rv_outbound, pong_line, refused_line, result_line, rv_socket_path, Connected, RvClient, RvInbound,
    RvOutbound, Secret, SendBlock, RV_MAX_OUTBOUND_LINE, RV_PROTOCOL, RV_REFUSED_FULL,
};
use eitri_core::remote_wire::{parse_phone_frame, PhoneFrame, PhoneInbound, MAX_PHONE_FRAME_BYTES, REMOTE_PROTOCOL};
use eitri_core::tab_set::{GoneWhy, RowKey};

use super::outbound::{write_through, Next, Notice, Outbound, Pushed, Source, PHONE_OUT_CAP_BYTES, PHONE_WRITE_STALL};
use super::report::{window_instances, Reporter};
use super::revoke::{watch_store, StoreView};
use super::route::{is_instance, route, Route, RowsIndex, NO_SUCH_TAB, NO_SUCH_WINDOW};
use super::ws::{Conn, SESSION_IDLE};
use crate::agents_feed::AgentsFeed;

/// The first frame must be a `hello` within this long of the 101.
pub const HELLO_WITHIN: Duration = Duration::from_secs(5);
/// How often each `rv-` connection is pinged, well inside the window's idle deadline.
pub const RV_PING_EVERY: Duration = Duration::from_secs(30);
/// How long a tab whose window said it moved may take to show up elsewhere before the same window is asked again.
pub const MOVE_SETTLE: Duration = Duration::from_secs(2);
/// How long a followed tab may have no window before the phone is told it is gone.
pub const ORPHAN_WITHIN: Duration = Duration::from_secs(30);
/// How long a tab the phone asked for may go unnamed by any row before it is refused: the phone reads its rows from
/// its own feed, which can run ahead of this server's.
pub const UNKNOWN_KEY_WITHIN: Duration = Duration::from_secs(2);
/// How many times in a row the session tries a window that does not answer before it tells the phone the tab is gone.
pub const RECONNECT_TRIES: u32 = 10;
/// The pause between two tries.
pub const RECONNECT_PAUSE: Duration = Duration::from_millis(200);
/// The most tabs one phone follows at once.
pub const MAX_FOLLOWED: usize = 64;
/// The phone's frames read ahead of the session; past it, the phone's reader waits and so does the phone.
const PHONE_FRAMES_AHEAD: usize = 16;

/// A device id as the store names it.
pub type DeviceId = String;

/// The deadlines and bounds of every connection. Tests shorten them.
#[derive(Debug, Clone)]
pub struct Limits {
    pub hello_within: Duration,
    pub idle: Duration,
    pub out_cap: usize,
    pub write_stall: Duration,
    pub rv_ping: Duration,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits {
            hello_within: HELLO_WITHIN,
            idle: SESSION_IDLE,
            out_cap: PHONE_OUT_CAP_BYTES,
            write_stall: PHONE_WRITE_STALL,
            rv_ping: RV_PING_EVERY,
        }
    }
}

/// What a session knows of its phone: who it is once its `hello` was checked, and what it last said it read. The
/// numbers of the frames sent to it are the writer's ([`Outbound`]), which stamps them as they go.
#[derive(Debug, Default)]
pub struct PhoneSession {
    pub device: Option<DeviceId>,
    pub grant: Option<Grant>,
    /// The last frame of ours the phone says it has.
    pub ack_in: u64,
    token: Option<Secret>,
}

/// What the session's own thread is told.
enum Event {
    Phone(PhoneFrame),
    /// A frame that is not one, or a broken rule of the phone's stream.
    Bad,
    /// The phone's stream ended, or said nothing in time.
    PhoneEnded,
    /// The phone closed its side, or broke a rule of the stream that ends it with this close code.
    Closing(u16),
    /// The connection is being closed from outside (revoked, a failed write).
    Killed,
    /// The rows index changed.
    Rows,
    /// A window closed the connection `id`.
    UpstreamEnded {
        id: u64,
    },
    /// The window behind `id` says `key` is no longer there.
    Gone {
        id: u64,
        key: String,
        moved: bool,
        line: String,
    },
    /// The window behind `id` sent a greeting or snapshot of `key`: the answer to subscribing there.
    Answered {
        id: u64,
        key: String,
    },
    /// The window behind `id` turned down `key`.
    Refused {
        id: u64,
        key: String,
        line: String,
    },
    Overflowed,
    Resync,
}

/// Closes a connection from any thread: the phone's stream first, so nothing more reaches it, then its queue and its
/// `rv-` connections.
struct Kill {
    dead: AtomicBool,
    phone: Box<dyn Conn>,
    upstreams: Mutex<BTreeMap<u64, UnixStream>>,
    out: Arc<Outbound>,
    events: Sender<Event>,
}

impl Kill {
    fn kill(&self) {
        if self.dead.swap(true, Ordering::SeqCst) {
            return;
        }
        self.phone.shut();
        self.out.close();
        for stream in lock(&self.upstreams).values() {
            let _ = stream.shutdown(Shutdown::Both);
        }
        let _ = self.events.send(Event::Killed);
    }

    fn dead(&self) -> bool {
        self.dead.load(Ordering::SeqCst)
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// A connection as the hub holds it.
struct Link {
    events: Sender<Event>,
    kill: Arc<Kill>,
    /// The device and token its `hello` proved, and the grant it had then.
    device: Option<(DeviceId, Secret, Grant)>,
    connected: Option<Connected>,
}

/// What every connection of a `serve` shares.
pub struct Hub {
    control: PathBuf,
    store: Arc<StoreView>,
    rows: Mutex<RowsIndex>,
    links: Mutex<BTreeMap<u64, Link>>,
    next_id: AtomicU64,
    limits: Limits,
    reporter: Reporter,
    reader: Mutex<Option<Reader>>,
}

impl Hub {
    /// Starts reading the rows of every window in `control_dir`, watching the device store at `store_path` and the
    /// reporting thread. Starts threads: call it after the sandbox.
    pub fn start(control_dir: &Path, store_path: &Path, limits: Limits) -> io::Result<Arc<Hub>> {
        let hub = Arc::new(Hub {
            control: control_dir.to_owned(),
            store: Arc::new(StoreView::new(store_path)),
            rows: Mutex::new(RowsIndex::default()),
            links: Mutex::new(BTreeMap::new()),
            next_id: AtomicU64::new(1),
            limits,
            reporter: Reporter::start(control_dir)?,
            reader: Mutex::new(None),
        });
        let weak = Arc::downgrade(&hub);
        let reader = Reader::start(control_dir, SCHEMA_V, move |event| match event {
            ReaderEvent::Change(change) => {
                if let Some(hub) = weak.upgrade() {
                    hub.rows_changed(&change);
                }
            }
            ReaderEvent::Error(why) => eprintln!("[remote] the rows are no longer read: {why}"),
        })?;
        *lock(&hub.reader) = Some(reader);
        let weak = Arc::downgrade(&hub);
        let dir = store_path.parent().unwrap_or(Path::new("/"));
        watch_store(dir, move || match weak.upgrade() {
            Some(hub) => {
                hub.recheck_devices();
                true
            }
            None => false,
        })?;
        Ok(hub)
    }

    fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    fn rows_changed(&self, change: &eitri_core::agents::Change) {
        if !lock(&self.rows).apply(change) {
            return;
        }
        for link in lock(&self.links).values() {
            let _ = link.events.send(Event::Rows);
        }
    }

    /// Closes the connection of every device the store no longer holds with its token, or whose grant fell.
    pub fn recheck_devices(&self) {
        let doomed: Vec<Arc<Kill>> = lock(&self.links)
            .values()
            .filter(|link| {
                link.device
                    .as_ref()
                    .is_some_and(|(device, token, grant)| !self.store.still(device, token.expose(), *grant))
            })
            .map(|link| link.kill.clone())
            .collect();
        for kill in doomed {
            kill.kill();
        }
    }

    /// Reports who is connected now. Taken and handed on under the links' lock, so the reports reach the reporter in
    /// the order they were taken and the last one it reads is the newest.
    fn report(&self) {
        let links = lock(&self.links);
        let now = links.values().filter_map(|link| link.connected.clone()).collect();
        self.reporter.report(now);
    }

    /// The most bytes each open connection's queue has held, and how many times each was dropped.
    pub fn gauges(&self) -> Vec<(usize, u64)> {
        lock(&self.links).values().map(|link| link.kill.out.gauge()).collect()
    }

    /// How many connections are open.
    pub fn connections(&self) -> usize {
        lock(&self.links).len()
    }

    /// Serves one upgraded connection until it ends. `host` is the `Host` its request named, `peer` where it came
    /// from.
    pub fn serve_phone(self: &Arc<Hub>, stream: Box<dyn Conn>, host: String, peer: String) {
        let Ok(phone) = stream.try_clone_conn() else { return };
        let Ok(writer_stream) = stream.try_clone_conn() else {
            return;
        };
        let id = self.next_id();
        let (tx, rx) = mpsc::channel();
        let told = Mutex::new(tx.clone());
        let out = Outbound::new(self.limits.out_cap, move |notice| match notice {
            Notice::Overflowed => {
                let _ = lock(&told).send(Event::Overflowed);
            }
        });
        let kill = Arc::new(Kill {
            dead: AtomicBool::new(false),
            phone,
            upstreams: Mutex::new(BTreeMap::new()),
            out: out.clone(),
            events: tx.clone(),
        });
        lock(&self.links).insert(
            id,
            Link {
                events: tx.clone(),
                kill: kill.clone(),
                device: None,
                connected: None,
            },
        );
        let device = Arc::new(OnceLock::new());
        let credits = Arc::new(Credits::default());
        let writing = {
            let (out, kill, tx, store, device, stall) = (
                out.clone(),
                kill.clone(),
                tx.clone(),
                self.store.clone(),
                device.clone(),
                self.limits.write_stall,
            );
            std::thread::Builder::new()
                .name("eitri-remote-write".into())
                .spawn(move || write_phone(writer_stream, &out, &kill, &tx, &store, &device, stall))
        };
        let reading = {
            let (out, kill, tx, credits, limits) = (
                out.clone(),
                kill.clone(),
                tx.clone(),
                credits.clone(),
                self.limits.clone(),
            );
            std::thread::Builder::new()
                .name("eitri-remote-read".into())
                .spawn(move || read_phone(stream, &out, &kill, &tx, &credits, &limits))
        };
        if writing.is_err() || reading.is_err() {
            kill.kill();
        }
        let mut control = Control {
            hub: self.clone(),
            id,
            out,
            kill: kill.clone(),
            tx,
            credits,
            device,
            session: PhoneSession::default(),
            host,
            peer,
            subs: BTreeMap::new(),
            upstreams: BTreeMap::new(),
            rows_wanted: false,
            feed: None,
            paused: false,
            next_ping: Instant::now() + self.limits.rv_ping,
        };
        control.run(&rx);
        control.stop_feed();
        kill.kill();
        let connected = lock(&self.links).remove(&id).and_then(|link| link.connected);
        if connected.is_some() {
            self.report();
        }
    }
}

/// How many of the phone's frames wait for the session.
#[derive(Default)]
struct Credits {
    taken: Mutex<usize>,
    freed: Condvar,
}

impl Credits {
    /// Waits for room; `false` when the connection died meanwhile.
    fn take(&self, kill: &Kill) -> bool {
        let mut taken = lock(&self.taken);
        while *taken >= PHONE_FRAMES_AHEAD {
            if kill.dead() {
                return false;
            }
            taken = self
                .freed
                .wait_timeout(taken, Duration::from_millis(100))
                .map(|(guard, _)| guard)
                .unwrap_or_else(|poisoned| poisoned.into_inner().0);
        }
        *taken += 1;
        true
    }

    fn give(&self) {
        let mut taken = lock(&self.taken);
        *taken = taken.saturating_sub(1);
        self.freed.notify_all();
    }
}

/// The phone's reader: WebSocket frames to the session's events, pings answered, the deadlines kept.
fn read_phone(
    mut stream: Box<dyn Conn>,
    out: &Outbound,
    kill: &Kill,
    tx: &Sender<Event>,
    credits: &Credits,
    limits: &Limits,
) {
    const MESSAGE_TOO_BIG: u16 = 1009;
    let started = Instant::now();
    let mut first = true;
    let mut decoder = Decoder::new(MAX_PHONE_FRAME_BYTES, MAX_PHONE_FRAME_BYTES);
    let mut buf = vec![0u8; 16 * 1024];
    let mut inbound = Vec::new();
    loop {
        let within = if first {
            limits.hello_within.saturating_sub(started.elapsed())
        } else {
            limits.idle
        };
        if within.is_zero() || stream.set_read_deadline(Some(within)).is_err() {
            let _ = tx.send(Event::PhoneEnded);
            return;
        }
        let got = match stream.read(&mut buf) {
            Ok(0) => {
                let _ = tx.send(Event::PhoneEnded);
                return;
            }
            Ok(got) => got,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => {
                let _ = tx.send(Event::PhoneEnded);
                return;
            }
        };
        decoder.feed(&buf[..got], &mut inbound);
        for frame in inbound.drain(..) {
            match frame {
                Inbound::Text(bytes) => {
                    first = false;
                    let parsed = String::from_utf8(bytes)
                        .ok()
                        .and_then(|text| parse_phone_frame(&text).ok());
                    let Some(parsed) = parsed else {
                        let _ = tx.send(Event::Bad);
                        return;
                    };
                    if !credits.take(kill) || tx.send(Event::Phone(parsed)).is_err() {
                        return;
                    }
                }
                Inbound::Ping(payload) => {
                    out.push_control(pong_frame(&payload), false);
                }
                Inbound::Pong => {}
                // The session echoes the close and ends the connection within its own deadline, whether or not the
                // close frame finds room in the queue.
                Inbound::Close(code) => {
                    let _ = tx.send(Event::Closing(code.unwrap_or(NORMAL)));
                    return;
                }
                Inbound::Violation(code) => {
                    let _ = tx.send(Event::Closing(code));
                    return;
                }
                Inbound::Oversized { .. } => {
                    let _ = tx.send(Event::Closing(MESSAGE_TOO_BIG));
                    return;
                }
            }
        }
    }
}

/// The phone's writer: the queue's frames, the store asked again before each frame of a known device, a pong or a
/// close as much as data.
fn write_phone(
    mut stream: Box<dyn Conn>,
    out: &Outbound,
    kill: &Kill,
    tx: &Sender<Event>,
    store: &StoreView,
    device: &OnceLock<(DeviceId, Secret, Grant)>,
    stall: Duration,
) {
    if stream.set_write_deadline(Some(stall)).is_err() {
        kill.kill();
        return;
    }
    loop {
        match out.next() {
            Next::Closed => return,
            Next::Resync => {
                let _ = tx.send(Event::Resync);
            }
            Next::Write { bytes, last, .. } => {
                if let Some((id, token, grant)) = device.get() {
                    if !store.still(id, token.expose(), *grant) {
                        kill.kill();
                        return;
                    }
                }
                let written = write_through(&mut stream, &bytes, stall, &|| out.stalled(), &|| kill.dead());
                if written.is_err() || last {
                    kill.kill();
                    return;
                }
            }
        }
    }
}

/// Pieces of a line too long for one, put together again.
#[derive(Default)]
struct Chunks {
    of: Option<u64>,
    next: u32,
    text: String,
}

impl Chunks {
    /// Takes piece `i` of line `of`; the whole line once `last` came. `Err` for a piece out of order or a line past
    /// the cap.
    fn take(&mut self, of: u64, i: u32, last: bool, data: &str) -> Result<Option<String>, ()> {
        if i == 0 {
            self.of = Some(of);
            self.next = 0;
            self.text.clear();
        }
        if self.of != Some(of) || i != self.next || self.text.len() + data.len() > RV_MAX_OUTBOUND_LINE {
            *self = Chunks::default();
            return Err(());
        }
        self.text.push_str(data);
        self.next += 1;
        if last {
            self.of = None;
            return Ok(Some(std::mem::take(&mut self.text)));
        }
        Ok(None)
    }
}

/// The tab a window's frame is about, if it is about one.
fn key_of(frame: &RvOutbound) -> Option<&str> {
    match frame {
        RvOutbound::Snapshot { key, .. }
        | RvOutbound::Events { key, .. }
        | RvOutbound::Page { key, .. }
        | RvOutbound::Gone { key, .. }
        | RvOutbound::Ended { key, .. }
        | RvOutbound::Greeting { key, .. }
        | RvOutbound::SendBlock { key, .. } => Some(key),
        RvOutbound::Refused { key, .. } => key.as_deref(),
        _ => None,
    }
}

/// One `rv-` connection's reader: what the window sends, to the phone's queue while the session hears it.
fn read_upstream(id: u64, mut client: RvClient, out: &Outbound, tx: &Sender<Event>, watch_only: bool) {
    let mut chunks = Chunks::default();
    loop {
        let line = match client.recv(Duration::from_secs(3600)) {
            Ok(Some(line)) => line,
            Ok(None) => continue,
            Err(_) => break,
        };
        let Ok(mut frame) = parse_rv_outbound(line.as_bytes()) else {
            eprintln!("[remote] a window sent a line this build does not read; its connection is closed");
            break;
        };
        let mut line = line;
        if let RvOutbound::Chunk { of, i, last, data, .. } = &frame {
            match chunks.take(*of, *i, *last, data) {
                Ok(None) => continue,
                Ok(Some(whole)) => match parse_rv_outbound(whole.as_bytes()) {
                    Ok(RvOutbound::Chunk { .. }) | Err(_) => break,
                    Ok(parsed) => {
                        frame = parsed;
                        line = whole;
                    }
                },
                Err(()) => break,
            }
        }
        let pushed = match frame {
            RvOutbound::Pong { .. } => Pushed::Queued,
            // The window turned this connection away; the session tries again later.
            RvOutbound::Refused { key: None, why, .. } if why == RV_REFUSED_FULL => break,
            RvOutbound::Gone { key, why, .. } => {
                let moved = why == GoneWhy::Moved;
                let _ = tx.send(Event::Gone { id, key, moved, line });
                Pushed::Queued
            }
            RvOutbound::Refused { key: Some(key), .. } => {
                let _ = tx.send(Event::Refused { id, key, line });
                Pushed::Queued
            }
            RvOutbound::SendBlock { key, .. } if watch_only => {
                let line = RvOutbound::SendBlock {
                    v: RV_PROTOCOL,
                    key: key.clone(),
                    block: Some(SendBlock::WatchOnly),
                }
                .line();
                out.push(Source::Upstream(id), Some(&key), line)
            }
            other => {
                let answer = matches!(other, RvOutbound::Snapshot { .. } | RvOutbound::Greeting { .. });
                let key = key_of(&other).map(str::to_owned);
                let pushed = out.push(Source::Upstream(id), key.as_deref(), line);
                if let (true, Some(key)) = (answer, key) {
                    let _ = tx.send(Event::Answered { id, key });
                }
                pushed
            }
        };
        if pushed == Pushed::Closed {
            return;
        }
    }
    let _ = tx.send(Event::UpstreamEnded { id });
}

/// A followed tab.
#[derive(Debug, Default)]
struct Sub {
    /// The connection it is followed through; `None` while it waits for a window.
    via: Option<u64>,
    /// Not tried again before this.
    not_before: Option<Instant>,
    /// Waiting since, while it has no window.
    orphan_since: Option<Instant>,
    /// The window that said it moved, while the move lands.
    moved_from: Option<String>,
    /// Asked for before any row named it, since: the phone's rows can be read before this server's.
    unknown_since: Option<Instant>,
    /// The window it is followed through has sent its greeting or snapshot. Until then a `refused` about it is the
    /// window turning the subscription down, and `more` is not sent; after, a `refused` answers something else.
    answered: bool,
    /// Connections in a row that ended or failed before the window answered.
    tries: u32,
}

/// An `rv-` connection.
struct Upstream {
    instance: String,
    writer: UnixStream,
    keys: BTreeSet<String>,
}

/// The session's own thread.
struct Control {
    hub: Arc<Hub>,
    id: u64,
    out: Arc<Outbound>,
    kill: Arc<Kill>,
    tx: Sender<Event>,
    credits: Arc<Credits>,
    device: Arc<OnceLock<(DeviceId, Secret, Grant)>>,
    session: PhoneSession,
    host: String,
    peer: String,
    subs: BTreeMap<String, Sub>,
    upstreams: BTreeMap<u64, Upstream>,
    rows_wanted: bool,
    feed: Option<(u64, AgentsFeed)>,
    paused: bool,
    next_ping: Instant,
}

/// Why the session ends.
enum End {
    /// With no reply: a `hello` refused, a phone gone.
    Quiet,
    /// After a close frame with this code.
    Close(u16),
}

/// The refusal of `more` for a tab the phone does not follow.
const NOT_FOLLOWED: &str = "not subscribed";
/// The refusal of `more` for a tab whose window has not answered the subscription yet.
const NOT_YET: &str = "not yet";

const POLICY: u16 = 1008;
const PROTOCOL_ERROR: u16 = 1002;
const NORMAL: u16 = 1000;

impl Control {
    fn run(&mut self, rx: &Receiver<Event>) {
        match self.hello(rx) {
            Ok(()) => {}
            Err(end) => return self.end(end, rx),
        }
        loop {
            let wait = self.wait_for();
            let event = match rx.recv_timeout(wait) {
                Ok(event) => event,
                Err(RecvTimeoutError::Timeout) => {
                    self.tick();
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => return,
            };
            if let Err(end) = self.take(event) {
                return self.end(end, rx);
            }
            if self.kill.dead() {
                return;
            }
        }
    }

    /// Ends the connection: at once, or after the writer wrote a close frame (within a second).
    fn end(&mut self, end: End, rx: &Receiver<Event>) {
        match end {
            End::Quiet => self.kill.kill(),
            End::Close(code) => {
                self.out.push_control(close_frame(code), true);
                let until = Instant::now() + Duration::from_secs(1);
                while !self.kill.dead() {
                    let left = until.saturating_duration_since(Instant::now());
                    if left.is_zero() || rx.recv_timeout(left).is_err() {
                        break;
                    }
                }
                self.kill.kill();
            }
        }
    }

    /// The first frame, within the deadline.
    fn hello(&mut self, rx: &Receiver<Event>) -> Result<(), End> {
        let deadline = Instant::now() + self.hub.limits.hello_within + Duration::from_millis(500);
        let first = loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match rx.recv_timeout(left) {
                Ok(Event::Phone(frame)) => break frame,
                Ok(Event::Overflowed | Event::Resync | Event::Rows) => continue,
                _ => return Err(End::Quiet),
            }
        };
        self.credits.give();
        let PhoneInbound::Hello {
            device,
            token,
            protocol,
            ..
        } = first.body
        else {
            return Err(End::Quiet);
        };
        if self.out.heard(first.seq, first.ack, &mut self.session.ack_in).is_err() {
            return Err(End::Quiet);
        }
        if protocol != REMOTE_PROTOCOL {
            let bye = serde_json::json!({ "v": REMOTE_PROTOCOL, "kind": "bye", "reason": "protocol" });
            self.out.push(Source::Local, None, bye.to_string());
            return Err(End::Close(NORMAL));
        }
        let Some(token) = token else {
            // Enrolling: no device yet.
            return Ok(());
        };
        let Some(grant) = self.hub.store.verify(&device, token.expose()) else {
            return Err(End::Quiet);
        };
        let _ = self.device.set((device.clone(), token.clone(), grant));
        self.session.device = Some(device.clone());
        self.session.grant = Some(grant);
        self.session.token = Some(token.clone());
        let since = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
        if let Some(link) = lock(&self.hub.links).get_mut(&self.id) {
            link.device = Some((device.clone(), token.clone(), grant));
            link.connected = Some(Connected {
                device: device.clone(),
                host: self.host.clone(),
                peer: self.peer.clone(),
                since,
            });
        }
        // A revocation between the check above and the link holding the device is caught here.
        if !self.hub.store.still(&device, token.expose(), grant) {
            return Err(End::Quiet);
        }
        self.hub.report();
        Ok(())
    }

    fn verified(&self) -> Option<(&str, &Secret, Grant)> {
        Some((
            self.session.device.as_deref()?,
            self.session.token.as_ref()?,
            self.session.grant?,
        ))
    }

    /// Until the next ping, or the next deadline of a tab that waits for a window.
    fn wait_for(&self) -> Duration {
        let now = Instant::now();
        let mut until = self.next_ping;
        for sub in self.subs.values().filter(|sub| sub.via.is_none()) {
            let deadlines = [
                sub.not_before,
                sub.unknown_since.map(|t| t + UNKNOWN_KEY_WITHIN),
                sub.orphan_since.map(|t| t + ORPHAN_WITHIN),
            ];
            for at in deadlines.into_iter().flatten() {
                until = until.min(at);
            }
        }
        until.saturating_duration_since(now).max(Duration::from_millis(1))
    }

    fn take(&mut self, event: Event) -> Result<(), End> {
        match event {
            Event::Phone(frame) => {
                self.credits.give();
                if self.out.heard(frame.seq, frame.ack, &mut self.session.ack_in).is_err() {
                    return Err(End::Close(PROTOCOL_ERROR));
                }
                self.phone(frame.body)
            }
            Event::Bad => Err(if self.verified().is_some() {
                End::Close(POLICY)
            } else {
                End::Quiet
            }),
            Event::PhoneEnded | Event::Killed => Err(End::Quiet),
            Event::Closing(code) => Err(End::Close(code)),
            Event::Rows => {
                self.follow_rows();
                Ok(())
            }
            Event::UpstreamEnded { id } => {
                self.upstream_ended(id);
                Ok(())
            }
            Event::Gone { id, key, moved, line } => {
                self.gone(id, key, moved, line);
                Ok(())
            }
            Event::Answered { id, key } => {
                self.answered(id, &key);
                Ok(())
            }
            Event::Refused { id, key, line } => {
                self.refused(id, key, line);
                Ok(())
            }
            Event::Overflowed => {
                self.pause();
                Ok(())
            }
            Event::Resync => {
                self.resume();
                Ok(())
            }
        }
    }

    fn local(&self, line: String) {
        self.out.push(Source::Local, None, line);
    }

    /// One frame from the phone, after its `hello`.
    fn phone(&mut self, body: PhoneInbound) -> Result<(), End> {
        let Some((device, token, grant)) = self.verified().map(|(d, t, g)| (d.to_owned(), t.clone(), g)) else {
            return self.enrolling(body);
        };
        let rows = lock(&self.hub.rows).clone();
        let routed = route(&body, &rows);
        match body {
            PhoneInbound::Hello { .. } => return Err(End::Close(PROTOCOL_ERROR)),
            PhoneInbound::Ping => self.local(pong_line()),
            PhoneInbound::SubscribeRows => {
                self.rows_wanted = true;
                if !self.paused {
                    self.start_feed();
                }
            }
            PhoneInbound::Subscribe { key } => self.subscribe(key, routed),
            PhoneInbound::Unsubscribe { key } => self.unsubscribe(&key),
            PhoneInbound::More {
                key,
                generation,
                before_turn,
            } => {
                // A page is asked of the window that serves the subscription, once it has answered it: the window
                // pages only a tab it is followed through, and a refusal of the page is then not taken for the
                // subscription's.
                let via = match self.subs.get(&key) {
                    None => Err(NOT_FOLLOWED),
                    Some(sub) => sub
                        .via
                        .filter(|id| sub.answered && self.upstreams.contains_key(id))
                        .ok_or(NOT_YET),
                };
                let line = RvInbound::More {
                    v: RV_PROTOCOL,
                    key: key.clone(),
                    generation,
                    before_turn,
                };
                let sent = via.and_then(|id| self.send(id, &line));
                if let Err(why) = sent {
                    self.local(refused_line(RowKey::parse(&key).as_ref(), why));
                }
            }
            PhoneInbound::Act { key, cmd } => {
                let command_id = cmd
                    .get("command_id")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned);
                let sent = match (grant, routed) {
                    (Grant::Watch, _) => Err("watch only"),
                    (Grant::WatchSteer, Route::Owner { instance }) => self.send_to(
                        &instance,
                        &RvInbound::Act {
                            v: RV_PROTOCOL,
                            device: device.clone(),
                            token: token.clone(),
                            key,
                            cmd,
                        },
                    ),
                    (Grant::WatchSteer, Route::Refused(why)) => Err(why),
                    (Grant::WatchSteer, _) => Err(NO_SUCH_TAB),
                };
                if let Err(why) = sent {
                    self.local(result_line(command_id.as_deref(), Err(why)));
                }
            }
            PhoneInbound::Enroll { .. } => {
                self.local(
                    RvOutbound::EnrollRefused {
                        v: RV_PROTOCOL,
                        why: "already enrolled".into(),
                    }
                    .line(),
                );
            }
            PhoneInbound::RevokeSelf => {
                let line = RvInbound::RevokeSelf {
                    v: RV_PROTOCOL,
                    device,
                    token,
                };
                if let Err(why) = self.send_to_any(&line) {
                    self.local(refused_line(None, why));
                }
            }
            PhoneInbound::SetPushEndpoint { url } => {
                let line = RvInbound::SetPushEndpoint {
                    v: RV_PROTOCOL,
                    device,
                    token,
                    url,
                };
                if let Err(why) = self.send_to_any(&line) {
                    self.local(refused_line(None, why));
                }
            }
        }
        Ok(())
    }

    /// A frame from a phone with no token: only `enroll` (and `ping`).
    fn enrolling(&mut self, body: PhoneInbound) -> Result<(), End> {
        match body {
            PhoneInbound::Ping => {
                self.local(pong_line());
                Ok(())
            }
            PhoneInbound::Enroll {
                instance,
                code,
                device_name,
            } => {
                let line = RvInbound::Enroll {
                    v: RV_PROTOCOL,
                    code,
                    device_name,
                    host: self.host.clone(),
                    peer: self.peer.clone(),
                };
                let sent = if is_instance(&instance) {
                    self.send_to(&instance, &line)
                } else {
                    Err(NO_SUCH_WINDOW)
                };
                if let Err(why) = sent {
                    let refused = RvOutbound::EnrollRefused {
                        v: RV_PROTOCOL,
                        why: why.into(),
                    };
                    self.local(refused.line());
                }
                Ok(())
            }
            _ => Err(End::Quiet),
        }
    }

    fn subscribe(&mut self, key: String, routed: Route) {
        let parsed = RowKey::parse(&key);
        if self.subs.contains_key(&key) {
            return self.local(refused_line(parsed.as_ref(), "already subscribed"));
        }
        if self.subs.len() >= MAX_FOLLOWED {
            return self.local(refused_line(parsed.as_ref(), "too many subscriptions"));
        }
        let instance = match routed {
            Route::Owner { instance } => instance,
            Route::Refused(NO_SUCH_TAB) => {
                let waiting = Sub {
                    unknown_since: Some(Instant::now()),
                    ..Sub::default()
                };
                self.subs.insert(key, waiting);
                return;
            }
            Route::Refused(why) => return self.local(refused_line(parsed.as_ref(), why)),
            _ => return self.local(refused_line(parsed.as_ref(), NO_SUCH_TAB)),
        };
        self.subs.insert(key.clone(), Sub::default());
        if !self.paused {
            self.attach(&key, &instance);
        }
    }

    fn unsubscribe(&mut self, key: &str) {
        let Some(sub) = self.subs.remove(key) else { return };
        self.out.route(key, None);
        if let Some(id) = sub.via {
            self.let_go_remote(id, key);
        }
    }

    /// Stops following `key` through `id`, telling the window.
    fn let_go_remote(&mut self, id: u64, key: &str) {
        if let Some(upstream) = self.upstreams.get_mut(&id) {
            upstream.keys.remove(key);
        }
        let line = RvInbound::Unsubscribe {
            v: RV_PROTOCOL,
            key: key.to_owned(),
        };
        let _ = self.send(id, &line);
    }

    /// Follows `key` through the window `instance`. A window that cannot be reached, or that ends the connection
    /// before it answers, is tried again a few times. A failed write is left to the event loop, never handled here, so
    /// a window that keeps closing at once cannot make this call itself.
    fn attach(&mut self, key: &str, instance: &str) {
        if !self.subs.contains_key(key) {
            return;
        }
        match self.upstream_for(instance) {
            Ok(id) => {
                if let Some(sub) = self.subs.get_mut(key) {
                    sub.via = Some(id);
                    sub.not_before = None;
                    sub.unknown_since = None;
                    sub.answered = false;
                }
                self.out.route(key, Some(id));
                if let Some(upstream) = self.upstreams.get_mut(&id) {
                    upstream.keys.insert(key.to_owned());
                }
                let line = RvInbound::Subscribe {
                    v: RV_PROTOCOL,
                    key: key.to_owned(),
                };
                if self.send(id, &line).is_err() {
                    let _ = self.tx.send(Event::UpstreamEnded { id });
                }
            }
            Err(_) => {
                if let Some(sub) = self.subs.get_mut(key) {
                    sub.via = None;
                }
                self.failed_try(key);
            }
        }
    }

    /// One more try at `key`'s window came to nothing: the next is [`RECONNECT_PAUSE`] away, and past
    /// [`RECONNECT_TRIES`] the phone is told the tab is gone.
    fn failed_try(&mut self, key: &str) {
        let give_up = self.subs.get_mut(key).is_some_and(|sub| {
            sub.tries += 1;
            sub.not_before = Some(Instant::now() + RECONNECT_PAUSE);
            sub.tries > RECONNECT_TRIES
        });
        if give_up {
            self.forget_gone(key);
        }
    }

    /// The window behind `id` answered `key`'s subscription: it is reachable, whatever came before.
    fn answered(&mut self, id: u64, key: &str) {
        if let Some(sub) = self.subs.get_mut(key).filter(|sub| sub.via == Some(id)) {
            sub.answered = true;
            sub.tries = 0;
            sub.orphan_since = None;
        }
    }

    /// Tells the phone `key` is gone and stops following it.
    fn forget_gone(&mut self, key: &str) {
        let Some(sub) = self.subs.remove(key) else { return };
        self.out.route(key, None);
        if let Some(id) = sub.via {
            if let Some(upstream) = self.upstreams.get_mut(&id) {
                upstream.keys.remove(key);
            }
        }
        let gone = RvOutbound::Gone {
            v: RV_PROTOCOL,
            key: key.to_owned(),
            why: GoneWhy::Closed,
        };
        self.local(gone.line());
    }

    /// The connection to `instance`, opened if there is none.
    fn upstream_for(&mut self, instance: &str) -> io::Result<u64> {
        if let Some((id, _)) = self.upstreams.iter().find(|(_, up)| up.instance == instance) {
            return Ok(*id);
        }
        let path = rv_socket_path(&self.hub.control, instance)?;
        let client = RvClient::connect(&path)?;
        let writer = client.writer()?;
        let shut = writer.try_clone()?;
        let id = self.hub.next_id();
        self.out.hear(id);
        lock(&self.kill.upstreams).insert(id, shut);
        if self.kill.dead() {
            let _ = writer.shutdown(Shutdown::Both);
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "the session ended"));
        }
        let (out, tx) = (self.out.clone(), self.tx.clone());
        let watch_only = self.session.grant == Some(Grant::Watch);
        std::thread::Builder::new()
            .name("eitri-remote-rv".into())
            .spawn(move || read_upstream(id, client, &out, &tx, watch_only))?;
        self.upstreams.insert(
            id,
            Upstream {
                instance: instance.to_owned(),
                writer,
                keys: BTreeSet::new(),
            },
        );
        Ok(id)
    }

    fn send(&mut self, id: u64, line: &RvInbound) -> Result<(), &'static str> {
        let Some(upstream) = self.upstreams.get_mut(&id) else {
            return Err("window not reachable");
        };
        let mut bytes = serde_json::to_vec(line).map_err(|_| "malformed")?;
        bytes.push(b'\n');
        if upstream.writer.write_all(&bytes).is_err() {
            return Err("window not reachable");
        }
        Ok(())
    }

    fn send_to(&mut self, instance: &str, line: &RvInbound) -> Result<(), &'static str> {
        let id = self.upstream_for(instance).map_err(|_| "window not reachable")?;
        let sent = self.send(id, line);
        if sent.is_err() {
            self.upstream_ended(id);
        }
        sent
    }

    fn send_to_any(&mut self, line: &RvInbound) -> Result<(), &'static str> {
        let mut instances: Vec<String> = self.upstreams.values().map(|up| up.instance.clone()).collect();
        instances.extend(lock(&self.hub.rows).instances());
        instances.extend(window_instances(&self.hub.control));
        let mut tried = BTreeSet::new();
        for instance in instances {
            if tried.insert(instance.clone()) && self.send_to(&instance, line).is_ok() {
                return Ok(());
            }
        }
        Err("no window")
    }

    fn close_upstream(&mut self, id: u64) -> Option<Upstream> {
        let upstream = self.upstreams.remove(&id)?;
        self.out.forget(id);
        if let Some(stream) = lock(&self.kill.upstreams).remove(&id) {
            let _ = stream.shutdown(Shutdown::Both);
        }
        Some(upstream)
    }

    /// A window closed the connection `id` (or it failed): its tabs are followed again, through a new one, after
    /// [`RECONNECT_PAUSE`]. A tab the window had not answered on it counts the connection as a failed try.
    fn upstream_ended(&mut self, id: u64) {
        let Some(upstream) = self.close_upstream(id) else {
            return;
        };
        let now = Instant::now();
        for key in upstream.keys {
            let Some(sub) = self.subs.get_mut(&key).filter(|sub| sub.via == Some(id)) else {
                continue;
            };
            sub.via = None;
            sub.orphan_since.get_or_insert(now);
            if std::mem::take(&mut sub.answered) {
                sub.not_before = Some(now + RECONNECT_PAUSE);
            } else {
                self.failed_try(&key);
            }
        }
        self.retry_waiting();
    }

    fn gone(&mut self, id: u64, key: String, moved: bool, line: String) {
        if self.subs.get(&key).and_then(|sub| sub.via) != Some(id) {
            return;
        }
        let from = self.upstreams.get_mut(&id).map(|upstream| {
            upstream.keys.remove(&key);
            upstream.instance.clone()
        });
        self.out.route(&key, None);
        if !moved {
            self.subs.remove(&key);
            return self.local(line);
        }
        // Wherever the row is now; the same window once the move has had time to land there or not at all.
        let now = Instant::now();
        if let Some(sub) = self.subs.get_mut(&key) {
            sub.via = None;
            sub.tries = 0;
            sub.moved_from = from;
            sub.orphan_since = Some(now);
            sub.not_before = Some(now + MOVE_SETTLE);
        }
        self.follow_rows();
    }

    fn refused(&mut self, id: u64, key: String, line: String) {
        let Some(sub) = self.subs.get(&key).filter(|sub| sub.via == Some(id)) else {
            return;
        };
        if sub.answered {
            // The subscription stands; the window turned down something else about the tab (a page).
            return self.local(line);
        }
        if let Some(upstream) = self.upstreams.get_mut(&id) {
            upstream.keys.remove(&key);
        }
        self.out.route(&key, None);
        let after_move = self.subs.get_mut(&key).is_some_and(|sub| {
            if sub.moved_from.is_none() {
                return false;
            }
            // Asked again after a move that has not landed: wait for the row instead.
            sub.via = None;
            sub.not_before = Some(Instant::now() + MOVE_SETTLE);
            true
        });
        if !after_move {
            self.subs.remove(&key);
            self.local(line);
        }
    }

    /// Follows each tab through the window its row names now.
    fn follow_rows(&mut self) {
        if self.paused {
            return;
        }
        let rows = lock(&self.hub.rows).clone();
        let now = Instant::now();
        let mut moves = Vec::new();
        for (key, sub) in &self.subs {
            let owner = rows.owner(key);
            let through = sub
                .via
                .and_then(|id| self.upstreams.get(&id))
                .map(|up| up.instance.as_str());
            match (through, owner) {
                (Some(at), Some(owner)) if at != owner => moves.push((key.clone(), owner.to_owned(), sub.via)),
                (None, Some(owner)) => {
                    let settling = sub.moved_from.as_deref() == Some(owner) && sub.not_before.is_some_and(|t| now < t);
                    let pausing = sub.moved_from.is_none() && sub.not_before.is_some_and(|t| now < t);
                    if !settling && !pausing {
                        moves.push((key.clone(), owner.to_owned(), None));
                    }
                }
                _ => {}
            }
        }
        for (key, owner, via) in moves {
            if let Some(id) = via {
                self.out.route(&key, None);
                self.let_go_remote(id, &key);
            }
            if let Some(sub) = self.subs.get_mut(&key) {
                sub.via = None;
                sub.moved_from = None;
            }
            self.attach(&key, &owner);
        }
    }

    /// Tries the tabs that wait for a window and whose pause is over; tells the phone of those that waited too long.
    fn retry_waiting(&mut self) {
        if self.paused {
            return;
        }
        let rows = lock(&self.hub.rows).clone();
        let now = Instant::now();
        let mut due = Vec::new();
        let mut orphans = Vec::new();
        let mut unknown = Vec::new();
        for (key, sub) in self.subs.iter_mut() {
            if sub.via.is_some() || sub.not_before.is_some_and(|t| now < t) {
                continue;
            }
            match rows.owner(key) {
                Some(owner) => due.push((key.clone(), owner.to_owned())),
                None if sub
                    .unknown_since
                    .is_some_and(|t| now.duration_since(t) >= UNKNOWN_KEY_WITHIN) =>
                {
                    unknown.push(key.clone())
                }
                None if sub.orphan_since.is_some_and(|t| now.duration_since(t) >= ORPHAN_WITHIN) => {
                    orphans.push(key.clone())
                }
                // Waits for a row to name it (`follow_rows`), or for its deadline.
                None => sub.not_before = None,
            }
        }
        for (key, owner) in due {
            if let Some(sub) = self.subs.get_mut(&key) {
                sub.moved_from = None;
            }
            self.attach(&key, &owner);
        }
        for key in orphans {
            self.forget_gone(&key);
        }
        for key in unknown {
            self.subs.remove(&key);
            self.local(refused_line(RowKey::parse(&key).as_ref(), NO_SUCH_TAB));
        }
    }

    fn tick(&mut self) {
        let now = Instant::now();
        if now >= self.next_ping {
            self.next_ping = now + self.hub.limits.rv_ping;
            let ids: Vec<u64> = self.upstreams.keys().copied().collect();
            for id in ids {
                if self.send(id, &RvInbound::Ping { v: RV_PROTOCOL }).is_err() {
                    self.upstream_ended(id);
                }
            }
        }
        self.retry_waiting();
    }

    fn start_feed(&mut self) {
        self.stop_feed();
        let id = self.hub.next_id();
        self.out.set_feed(Some(id));
        let out = self.out.clone();
        match AgentsFeed::start(&self.hub.control, move |frame| {
            out.push(Source::Feed(id), None, frame);
        }) {
            Ok(feed) => self.feed = Some((id, feed)),
            Err(e) => {
                eprintln!("[remote] the agents feed did not start: {e}");
                self.out.set_feed(None);
                self.local(refused_line(None, "the rows cannot be read"));
            }
        }
    }

    fn stop_feed(&mut self) {
        self.out.set_feed(None);
        if let Some((_, feed)) = self.feed.take() {
            feed.stop();
        }
    }

    /// The queue was dropped: every `rv-` connection and the feed are cut until the writer has caught up.
    fn pause(&mut self) {
        self.paused = true;
        let ids: Vec<u64> = self.upstreams.keys().copied().collect();
        for id in ids {
            self.close_upstream(id);
        }
        for sub in self.subs.values_mut() {
            sub.via = None;
            sub.not_before = None;
            sub.moved_from = None;
            sub.answered = false;
        }
        self.stop_feed();
    }

    /// The writer has caught up: every tab is followed again from a fresh greeting and snapshot, and the feed starts
    /// again from its whole view.
    fn resume(&mut self) {
        if !self.paused {
            return;
        }
        self.paused = false;
        let now = Instant::now();
        for sub in self.subs.values_mut() {
            sub.tries = 0;
            sub.orphan_since.get_or_insert(now);
        }
        self.retry_waiting();
        if self.rows_wanted {
            self.start_feed();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunks_come_together_only_in_order() {
        let mut chunks = Chunks::default();
        assert_eq!(chunks.take(4, 0, false, "ab"), Ok(None));
        assert_eq!(chunks.take(4, 1, false, "cd"), Ok(None));
        assert_eq!(chunks.take(4, 2, true, "e"), Ok(Some("abcde".to_owned())));
        assert_eq!(chunks.take(5, 1, true, "x"), Err(()), "a piece without its first");
        assert_eq!(chunks.take(6, 0, false, "a"), Ok(None));
        assert_eq!(chunks.take(7, 1, true, "b"), Err(()), "a piece of another line");
    }

    fn kill_for(phone: &dyn Conn, out: &Arc<Outbound>, events: &Sender<Event>) -> Arc<Kill> {
        Arc::new(Kill {
            dead: AtomicBool::new(false),
            phone: phone.try_clone_conn().unwrap(),
            upstreams: Mutex::new(BTreeMap::new()),
            out: out.clone(),
            events: events.clone(),
        })
    }

    #[test]
    fn a_device_the_store_no_longer_holds_is_written_no_frame_of_any_kind() {
        let (server, mut phone) = UnixStream::pair().unwrap();
        let server: Box<dyn Conn> = Box::new(server);
        let (tx, _rx) = mpsc::channel();
        let out = Outbound::new(PHONE_OUT_CAP_BYTES, |_| {});
        let kill = kill_for(server.as_ref(), &out, &tx);
        // A store that is not there holds no device: this one is gone, and the watch has not run yet.
        let store = StoreView::new(Path::new("/nonexistent-eitri-store/devices.json"));
        let device = OnceLock::new();
        let _ = device.set(("d1".to_owned(), Secret::new("tok"), Grant::WatchSteer));
        out.push_control(pong_frame(b"p"), false);
        out.push_control(close_frame(NORMAL), true);
        let writer = {
            let (out, kill) = (out.clone(), kill.clone());
            std::thread::spawn(move || write_phone(server, &out, &kill, &tx, &store, &device, Duration::from_secs(5)))
        };
        phone.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let mut got = Vec::new();
        let _ = phone.read_to_end(&mut got);
        out.close();
        writer.join().unwrap();
        assert!(got.is_empty(), "{} bytes reached the phone: {got:?}", got.len());
        assert!(kill.dead(), "the connection was closed");
    }

    #[test]
    fn a_close_from_the_phone_ends_the_session_even_when_its_echo_does_not_fit() {
        let (server, mut phone) = UnixStream::pair().unwrap();
        let server: Box<dyn Conn> = Box::new(server);
        let (tx, rx) = mpsc::channel();
        let out = Outbound::new(8, |_| {});
        // The queue already holds a frame, and the echoed close would take it past the cap.
        assert_eq!(out.push(Source::Local, None, r#"{"pad":1}"#.into()), Pushed::Queued);
        let kill = kill_for(server.as_ref(), &out, &tx);
        // A masked close frame with code 1000.
        phone.write_all(&[0x88, 0x82, 0, 0, 0, 0, 0x03, 0xe8]).unwrap();
        read_phone(server, &out, &kill, &tx, &Credits::default(), &Limits::default());
        assert!(
            matches!(rx.try_recv(), Ok(Event::Closing(1000))),
            "the reader stopped and told the session nothing: no deadline would end it"
        );
    }

    #[test]
    fn a_frame_about_a_tab_names_it() {
        let gone = parse_rv_outbound(br#"{"v":1,"kind":"gone","key":"k-1","why":"closed"}"#).unwrap();
        assert_eq!(key_of(&gone), Some("k-1"));
        let theme = parse_rv_outbound(br#"{"v":1,"kind":"theme","theme":{}}"#).unwrap();
        assert_eq!(key_of(&theme), None);
        let refused = parse_rv_outbound(br#"{"v":1,"kind":"refused","key":null,"why":"x"}"#).unwrap();
        assert_eq!(key_of(&refused), None);
    }
}
