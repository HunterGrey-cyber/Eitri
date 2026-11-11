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
//! store's view and its watch, which closes the connections of a device that is revoked or whose grant fell, the
//! report of who is connected that every window gets ([`report`](super::report)), and `serve`'s audit log ([`Audit`]).
//!
//! **What a session records.** `conn_open` once a `hello` with a token is accepted (the device by its store id and
//! name, the listener, the peer, `Host` and `Origin`) and `conn_close` when that connection ends (why, how long, the
//! bytes each way after the 101); `refused` with `bad_token`, or `revoked_token` for a device the store has let go
//! ([`StoreView::revoked`]), for a `hello` whose token is not the device's; `refused` with `grant` for an `act` from a
//! `watch` device, which also sends the window that owns the tab a `refused_notice` (no command text) so the desk
//! sees the attempt; `push_endpoint_set` with the endpoint's host only, for an endpoint the window's own check takes
//! (one it refuses is not recorded; one its store write then fails on still is). `audit_mine` is answered here from
//! the log, with the session's own device's records alone, newest first, once a second per device. No record holds a
//! token or a command's text.
//!
//! Refusals made before a device is proved (before `hello`, and of the `hello` itself) are bounded: anyone who can
//! reach a listener can cause one, so past a burst each is counted on the next recorded one (`dropped`). When `serve`
//! ends (`disable`), every connection still open gets its `conn_close` (`serve_ended`) first.
//!
//! **What the notifier reads here.** `set_finished_keys` is kept by the hub, per device, with no reply: the tabs whose
//! finished turns wake that phone (its bells), replaced by each new one, kept past its connection and sent again by
//! the phone at each connect. [`Hub::push_targets`] pairs them with the store's push endpoints, and
//! [`Hub::connected_devices`] names the devices connected now, which are not woken.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use eitri_core::agents::{Reader, ReaderEvent, SCHEMA_V};
use eitri_core::ide_server::frames::{close_frame, pong_frame, Decoder, Inbound};
use eitri_core::remote_audit::{AuditLog, DeviceRef, Event as AuditEvent, Filter, Listener, Reason};
use eitri_core::remote_devices::Grant;
use eitri_core::remote_rv::{
    parse_rv_outbound, pong_line, refused_line, result_line, rv_socket_path, shut_both_ways, Connected, RvClient,
    RvInbound, RvOutbound, Secret, SendBlock, RV_MAX_OUTBOUND_LINE, RV_PROTOCOL, RV_REFUSED_FULL,
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
/// The most records one `audit_mine` answer holds.
pub const AUDIT_MINE_MAX: u32 = 200;
/// The most bytes of records one `audit_mine` answer holds; the newest that fit are sent.
const AUDIT_MINE_BYTES: usize = 256 * 1024;
/// How often a device is answered `audit_mine`, across all its connections: each answer reads the retained log, so a
/// phone asking in a loop is told to wait, and so is one asking while another device's answer is being read.
const AUDIT_MINE_EVERY: Duration = Duration::from_secs(1);
/// How many refusals before any `hello` are recorded one after another, and how often one more may be once those are
/// used: anyone who can reach a listener can be refused, so a flood is recorded as a count on the next record rather
/// than record by record, which would rotate the log's older files out and push the journal past its rate limit.
const EARLY_REFUSALS_BURST: u32 = 30;
const EARLY_REFUSAL_EVERY: Duration = Duration::from_secs(30);

/// `serve`'s one writer of the audit log, shared by its threads; a record holds the log's lock for that record only.
/// A record that cannot be written is said on stderr (the first time and every thousandth after) and never stops what
/// it records.
pub struct Audit {
    log: Option<Mutex<AuditLog>>,
    dir: Option<PathBuf>,
    failed: AtomicU64,
    disabled: AtomicBool,
    /// The connections whose `conn_open` is recorded and whose `conn_close` is not yet; `None` once `serve` ended,
    /// when `disable` recorded their close.
    open: Mutex<Option<BTreeMap<u64, OpenConn>>>,
    early: Mutex<EarlyRefusals>,
    /// When each device was last answered `audit_mine`.
    mine_last: Mutex<BTreeMap<String, Instant>>,
    /// Held while an `audit_mine` answer reads the log, so one reading runs at a time.
    mine_reading: Mutex<()>,
    #[cfg(feature = "remote-test-hooks")]
    mine_now: Mutex<Option<Instant>>,
}

/// A connection whose `hello` was accepted, as its `conn_close` will name it.
struct OpenConn {
    device: DeviceRef,
    at: Instant,
    kill: Arc<Kill>,
}

/// The bound on refusals before `hello`: a bucket of [`EARLY_REFUSALS_BURST`] that gains one every
/// [`EARLY_REFUSAL_EVERY`], and the count of those not recorded since the last that was.
struct EarlyRefusals {
    tokens: u32,
    refilled: Instant,
    dropped: u64,
    last_dropped: Option<Reason>,
}

impl EarlyRefusals {
    fn new(now: Instant) -> EarlyRefusals {
        EarlyRefusals {
            tokens: EARLY_REFUSALS_BURST,
            refilled: now,
            dropped: 0,
            last_dropped: None,
        }
    }

    /// Whether a refusal of `reason` at `now` is recorded: `Some` with how many were not since the last that was
    /// (`None` inside when there were none), or `None` when this one is only counted.
    fn take(&mut self, reason: Reason, now: Instant) -> Option<Option<u64>> {
        let gained = now.saturating_duration_since(self.refilled).as_nanos() / EARLY_REFUSAL_EVERY.as_nanos();
        if gained > 0 {
            let gained = u32::try_from(gained).unwrap_or(u32::MAX);
            self.tokens = self.tokens.saturating_add(gained).min(EARLY_REFUSALS_BURST);
            self.refilled = if self.tokens == EARLY_REFUSALS_BURST {
                now
            } else {
                self.refilled + EARLY_REFUSAL_EVERY * gained
            };
        }
        if self.tokens == 0 {
            self.dropped += 1;
            self.last_dropped = Some(reason);
            return None;
        }
        self.tokens -= 1;
        self.last_dropped = None;
        let dropped = std::mem::take(&mut self.dropped);
        Some((dropped > 0).then_some(dropped))
    }

    /// The refusals counted and not yet recorded, with the reason of the last; emptied.
    fn take_dropped(&mut self) -> Option<(Reason, u64)> {
        let reason = self.last_dropped.take()?;
        Some((reason, std::mem::take(&mut self.dropped)))
    }
}

impl Audit {
    #[cfg(feature = "remote-test-hooks")]
    pub(super) fn notifier_turn_path_for_tests(&self) -> Option<PathBuf> {
        self.dir.as_ref().map(|dir| dir.join("notifier-turn-for-tests"))
    }

    /// Records through `log`, which was opened on `dir`.
    pub fn new(log: AuditLog, dir: &Path) -> Arc<Audit> {
        Audit::with(Some(log), Some(dir.to_owned()))
    }

    /// Records nothing.
    pub fn off() -> Arc<Audit> {
        Audit::with(None, None)
    }

    fn with(log: Option<AuditLog>, dir: Option<PathBuf>) -> Arc<Audit> {
        Arc::new(Audit {
            log: log.map(Mutex::new),
            dir,
            failed: AtomicU64::new(0),
            disabled: AtomicBool::new(false),
            open: Mutex::new(Some(BTreeMap::new())),
            early: Mutex::new(EarlyRefusals::new(Instant::now())),
            mine_last: Mutex::new(BTreeMap::new()),
            mine_reading: Mutex::new(()),
            #[cfg(feature = "remote-test-hooks")]
            mine_now: Mutex::new(None),
        })
    }

    pub fn record(&self, event: AuditEvent) {
        let Some(log) = &self.log else { return };
        if let Err(e) = lock(log).record(event) {
            let failed = self.failed.fetch_add(1, Ordering::Relaxed);
            if failed % 1000 == 0 {
                eprintln!(
                    "[remote] a record was not written to the audit log ({} so far): {e}",
                    failed + 1
                );
            }
        }
    }

    /// Records `disable` with `why` the first time it is called: whichever end comes first (the idle exit, a signal, a
    /// failure) is the one recorded. Before it, the refusals before `hello` still only counted, and a `conn_close`
    /// (`serve_ended`) for each connection still open, since `serve` ends without their sessions' own.
    pub fn disable_once(&self, why: &str) {
        if self.disabled.swap(true, Ordering::SeqCst) {
            return;
        }
        if let Some((reason, dropped)) = lock(&self.early).take_dropped() {
            self.record(AuditEvent::Refused {
                reason,
                device: None,
                listener: None,
                peer: None,
                key: None,
                kind: None,
                dropped: Some(dropped),
            });
        }
        let open = lock(&self.open).take().unwrap_or_default();
        for conn in open.into_values() {
            self.record(conn.close("serve_ended"));
        }
        self.record(AuditEvent::Disable { why: why.to_owned() });
    }

    /// A refusal before any device was proved (before `hello`, or of the `hello` itself, naming the device it
    /// claimed): where it came in and from; within [`EARLY_REFUSALS_BURST`] and [`EARLY_REFUSAL_EVERY`], past which
    /// it is counted on the next one recorded.
    pub fn record_early_refusal(&self, reason: Reason, device: Option<DeviceRef>, listener: &Listener, peer: &str) {
        self.record_early_refusal_at(reason, device, listener, peer, Instant::now());
    }

    fn record_early_refusal_at(
        &self,
        reason: Reason,
        device: Option<DeviceRef>,
        listener: &Listener,
        peer: &str,
        now: Instant,
    ) {
        let Some(dropped) = lock(&self.early).take(reason, now) else {
            return;
        };
        self.record(AuditEvent::Refused {
            reason,
            device,
            listener: Some(listener.clone()),
            peer: Some(peer.to_owned()),
            key: None,
            kind: None,
            dropped,
        });
    }

    /// Notes connection `id`, whose `conn_open` naming `device` was just recorded.
    fn opened(&self, id: u64, device: DeviceRef, kill: &Arc<Kill>) {
        if let Some(open) = lock(&self.open).as_mut() {
            open.insert(
                id,
                OpenConn {
                    device,
                    at: Instant::now(),
                    kill: kill.clone(),
                },
            );
        }
    }

    /// Records connection `id`'s `conn_close` with `why`, unless `disable` recorded it or it never opened.
    fn closed(&self, id: u64, why: &str) {
        let conn = lock(&self.open).as_mut().and_then(|open| open.remove(&id));
        if let Some(conn) = conn {
            self.record(conn.close(why));
        }
    }

    /// The devices the retained log's `revoke` records name, with their names.
    pub fn revoked_in_log(&self) -> Vec<(String, String)> {
        let Some(dir) = &self.dir else { return Vec::new() };
        let filter = Filter {
            event: Some("revoke".to_owned()),
            ..Filter::default()
        };
        let records = eitri_core::remote_audit::read(dir, &filter, None).unwrap_or_else(|e| {
            eprintln!("[remote] the audit log's revocations were not read: {e}");
            Vec::new()
        });
        records
            .into_iter()
            .filter_map(|r| {
                let name = r
                    .fields
                    .get("device_name")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                Some((r.device()?.to_owned(), name.to_owned()))
            })
            .collect()
    }

    #[cfg(feature = "remote-test-hooks")]
    pub fn set_mine_time_for_tests(&self, now: Instant) {
        *lock(&self.mine_now) = Some(now);
    }

    /// The `audit_mine` answer: the newest records about `device` by its id (a record about another device whose
    /// claimed name is this id is not this device's), at most `limit` and [`AUDIT_MINE_MAX`], within
    /// [`AUDIT_MINE_BYTES`]. `None` when `device` was answered less than [`AUDIT_MINE_EVERY`] ago, on any of its
    /// connections, or another answer is reading the log now.
    fn mine(&self, device: &str, limit: u32) -> Option<String> {
        #[cfg(feature = "remote-test-hooks")]
        let now = lock(&self.mine_now).unwrap_or_else(Instant::now);
        #[cfg(not(feature = "remote-test-hooks"))]
        let now = Instant::now();
        {
            let mut last = lock(&self.mine_last);
            if last
                .get(device)
                .is_some_and(|at| now.duration_since(*at) < AUDIT_MINE_EVERY)
            {
                return None;
            }
            last.retain(|_, at| now.duration_since(*at) < AUDIT_MINE_EVERY);
            last.insert(device.to_owned(), now);
        }
        let _reading = match self.mine_reading.try_lock() {
            Ok(guard) => guard,
            Err(std::sync::TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => return None,
        };
        let limit = limit.min(AUDIT_MINE_MAX) as usize;
        let mut records: Vec<serde_json::Value> = Vec::new();
        if let (Some(dir), true) = (&self.dir, limit > 0) {
            let filter = Filter {
                device_id: Some(device.to_owned()),
                ..Filter::default()
            };
            let found = eitri_core::remote_audit::read(dir, &filter, Some(limit)).unwrap_or_default();
            let mut bytes = 0;
            for record in found {
                bytes += record.line.len() + 1;
                if bytes > AUDIT_MINE_BYTES {
                    break;
                }
                if let Ok(value) = serde_json::from_str(&record.line) {
                    records.push(value);
                }
            }
        }
        Some(serde_json::json!({ "v": REMOTE_PROTOCOL, "kind": "audit_mine", "records": records }).to_string())
    }
}

impl OpenConn {
    fn close(self, why: &str) -> AuditEvent {
        AuditEvent::ConnClose {
            device: self.device,
            why: why.to_owned(),
            secs: self.at.elapsed().as_secs(),
            bytes_in: self.kill.bytes_in.load(Ordering::Relaxed),
            bytes_out: self.kill.bytes_out.load(Ordering::Relaxed),
        }
    }
}

/// The host of a push endpoint, lowercase, without user information, port or path: what the audit log keeps of it.
/// `None` for what has no host.
pub fn endpoint_host(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    if scheme.is_empty() || !scheme.bytes().all(|b| b.is_ascii_alphanumeric() || b"+-.".contains(&b)) {
        return None;
    }
    let authority = rest.split(['/', '?', '#']).next()?;
    let host_port = authority.rsplit_once('@').map_or(authority, |(_, after)| after);
    let host = match host_port.strip_prefix('[') {
        Some(v6) => format!("[{}]", v6.split_once(']')?.0),
        None => host_port.split(':').next()?.to_owned(),
    };
    (!host.is_empty() && host != "[]").then(|| host.to_ascii_lowercase())
}

/// Where a connection came from, as `serve` saw it before its 101.
#[derive(Debug, Clone)]
pub struct Arrival {
    /// The `Host` its request named.
    pub host: String,
    /// Its `Origin`, when it sent one.
    pub origin: Option<String>,
    /// The peer address and port, or `unix`.
    pub peer: String,
    pub listener: Listener,
}

/// A device id as the store names it.
pub type DeviceId = String;

#[cfg(feature = "remote-test-hooks")]
#[derive(Debug)]
pub enum HelloReadForTests {
    Read,
    Deadline,
}

#[cfg(feature = "remote-test-hooks")]
#[derive(Debug)]
pub struct HelloWaitForTests {
    elapsed_ms: AtomicU64,
    calls: Sender<Duration>,
    steps: Mutex<Receiver<HelloReadForTests>>,
}

#[cfg(feature = "remote-test-hooks")]
impl HelloWaitForTests {
    pub fn new() -> (Arc<Self>, Receiver<Duration>, Sender<HelloReadForTests>) {
        let (calls, called) = std::sync::mpsc::channel();
        let (step, steps) = std::sync::mpsc::channel();
        (
            Arc::new(Self {
                elapsed_ms: AtomicU64::new(0),
                calls,
                steps: Mutex::new(steps),
            }),
            called,
            step,
        )
    }
    pub fn advance(&self, elapsed: Duration) {
        self.elapsed_ms
            .fetch_add(elapsed.as_millis().try_into().unwrap(), Ordering::SeqCst);
    }
}

/// The deadlines and bounds of every connection. Tests shorten them.
#[derive(Debug, Clone)]
pub struct Limits {
    pub hello_within: Duration,
    pub idle: Duration,
    pub out_cap: usize,
    pub write_stall: Duration,
    pub rv_ping: Duration,
    #[cfg(feature = "remote-test-hooks")]
    pub hello_wait_for_tests: Option<Arc<HelloWaitForTests>>,
    #[cfg(feature = "remote-test-hooks")]
    pub report_interval_for_tests: Option<Duration>,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits {
            hello_within: HELLO_WITHIN,
            idle: SESSION_IDLE,
            out_cap: PHONE_OUT_CAP_BYTES,
            write_stall: PHONE_WRITE_STALL,
            rv_ping: RV_PING_EVERY,
            #[cfg(feature = "remote-test-hooks")]
            hello_wait_for_tests: None,
            #[cfg(feature = "remote-test-hooks")]
            report_interval_for_tests: None,
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
    #[cfg(feature = "remote-test-hooks")]
    TestTick(Instant, Sender<()>),
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
    /// Why it was closed from outside, when it was.
    why: Mutex<Option<&'static str>>,
    /// The bytes read from and written to the phone after the 101.
    bytes_in: AtomicU64,
    bytes_out: AtomicU64,
}

impl Kill {
    fn new(phone: Box<dyn Conn>, out: Arc<Outbound>, events: Sender<Event>) -> Kill {
        Kill {
            dead: AtomicBool::new(false),
            phone,
            upstreams: Mutex::new(BTreeMap::new()),
            out,
            events,
            why: Mutex::new(None),
            bytes_in: AtomicU64::new(0),
            bytes_out: AtomicU64::new(0),
        }
    }

    /// Closes it, saying why, unless it was closed already.
    fn kill_because(&self, why: &'static str) {
        if !self.dead() {
            lock(&self.why).get_or_insert(why);
        }
        self.kill();
    }

    fn why(&self) -> Option<&'static str> {
        *lock(&self.why)
    }

    fn kill(&self) {
        if self.dead.swap(true, Ordering::SeqCst) {
            return;
        }
        self.phone.shut();
        self.out.close();
        for stream in lock(&self.upstreams).values() {
            shut_both_ways(stream);
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
    audit: Arc<Audit>,
    /// The tabs whose finished turns wake each device, as it last said (`set_finished_keys`); kept while `serve`
    /// runs, past the device's connection, since the wakes are for when it is not connected.
    finished_keys: Mutex<BTreeMap<DeviceId, BTreeSet<String>>>,
}

impl Hub {
    #[cfg(feature = "remote-test-hooks")]
    pub fn report_look_for_tests(&self) {
        self.reporter.look_for_tests();
    }

    #[cfg(feature = "remote-test-hooks")]
    pub fn tick_sessions_for_tests(&self, now: Instant) {
        let senders: Vec<_> = lock(&self.links).values().map(|link| link.events.clone()).collect();
        for sender in senders {
            let (tx, rx) = std::sync::mpsc::channel();
            sender
                .send(Event::TestTick(now, tx))
                .expect("the session ended before its tick");
            rx.recv_timeout(Duration::from_secs(60))
                .expect("the session did not finish its tick");
        }
    }

    /// [`Hub::start_audited`] with no audit log.
    pub fn start(control_dir: &Path, store_path: &Path, limits: Limits) -> io::Result<Arc<Hub>> {
        Hub::start_audited(control_dir, store_path, limits, Audit::off())
    }

    /// Starts reading the rows of every window in `control_dir`, watching the device store at `store_path` and the
    /// reporting thread, recording through `audit`; the devices its retained `revoke` records name count as revoked.
    /// Starts threads: call it after the sandbox.
    pub fn start_audited(
        control_dir: &Path,
        store_path: &Path,
        limits: Limits,
        audit: Arc<Audit>,
    ) -> io::Result<Arc<Hub>> {
        let store = Arc::new(StoreView::new(store_path));
        store.remember_revoked(audit.revoked_in_log());
        // The first reading: a device that leaves the store from now on is seen leaving.
        store.refresh();
        #[cfg(feature = "remote-test-hooks")]
        let reporter = Reporter::start_with_interval(
            control_dir,
            limits.report_interval_for_tests.unwrap_or(super::report::LOOK_EVERY),
        )?;
        #[cfg(not(feature = "remote-test-hooks"))]
        let reporter = Reporter::start(control_dir)?;
        let hub = Arc::new(Hub {
            control: control_dir.to_owned(),
            store,
            rows: Mutex::new(RowsIndex::default()),
            links: Mutex::new(BTreeMap::new()),
            next_id: AtomicU64::new(1),
            limits,
            reporter,
            reader: Mutex::new(None),
            audit,
            finished_keys: Mutex::new(BTreeMap::new()),
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

    /// `serve`'s audit log.
    pub fn audit(&self) -> &Arc<Audit> {
        &self.audit
    }

    /// Keeps `keys` as the tabs whose finished turns wake `device`, in place of what it said before.
    pub fn set_finished_keys(&self, device: &str, keys: Vec<String>) {
        lock(&self.finished_keys).insert(device.to_owned(), keys.into_iter().collect());
    }

    /// The devices of the store with a push endpoint, each with the finished keys it set: whom a wake can reach.
    pub fn push_targets(&self) -> Vec<crate::notify::Target> {
        let store = self.store.devices();
        let keys = lock(&self.finished_keys);
        store
            .devices()
            .iter()
            .filter_map(|device| {
                Some(crate::notify::Target {
                    device_id: device.id.clone(),
                    endpoint: device.push_endpoint.clone()?,
                    finished_keys: keys.get(&device.id).cloned().unwrap_or_default(),
                })
            })
            .collect()
    }

    /// The devices with a connection whose `hello` was accepted, open now.
    pub fn connected_devices(&self) -> BTreeSet<DeviceId> {
        lock(&self.links)
            .values()
            .filter_map(|link| link.device.as_ref().map(|(device, _, _)| device.clone()))
            .collect()
    }

    /// The store's name for `device` now.
    pub fn device_name(&self, device: &str) -> Option<String> {
        self.store.name_of(device)
    }

    /// Closes the connection of every device the store no longer holds with its token, or whose grant fell.
    pub fn recheck_devices(&self) {
        // Read even with no connection open, so a device that leaves the store is seen leaving.
        self.store.refresh();
        let doomed: Vec<(Arc<Kill>, &'static str)> = lock(&self.links)
            .values()
            .filter_map(|link| {
                let (device, token, grant) = link.device.as_ref()?;
                if self.store.still(device, token.expose(), *grant) {
                    return None;
                }
                let why = if self.store.verify(device, token.expose()).is_none() {
                    "revoked"
                } else {
                    "grant_lowered"
                };
                Some((link.kill.clone(), why))
            })
            .collect();
        for (kill, why) in doomed {
            kill.kill_because(why);
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
    /// from; it came in on the Unix socket and sent no `Origin`.
    pub fn serve_phone(self: &Arc<Hub>, stream: Box<dyn Conn>, host: String, peer: String) {
        let arrival = Arrival {
            host,
            origin: None,
            peer,
            listener: Listener::Socket,
        };
        self.serve_phone_from(stream, arrival);
    }

    /// Serves one upgraded connection that came as `arrival` says until it ends.
    pub fn serve_phone_from(self: &Arc<Hub>, stream: Box<dyn Conn>, arrival: Arrival) {
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
        let kill = Arc::new(Kill::new(phone, out.clone(), tx.clone()));
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
            host: arrival.host,
            peer: arrival.peer,
            origin: arrival.origin,
            listener: arrival.listener,
            opened: None,
            why: "ended",
            subs: BTreeMap::new(),
            upstreams: BTreeMap::new(),
            rows_wanted: false,
            feed: None,
            paused: false,
            next_ping: Instant::now() + self.limits.rv_ping,
            #[cfg(feature = "remote-test-hooks")]
            now_for_tests: None,
        };
        control.run(&rx);
        control.stop_feed();
        kill.kill();
        self.audit.closed(id, kill.why().unwrap_or(control.why));
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
    #[cfg(feature = "remote-test-hooks")]
    let initial_test_elapsed = limits
        .hello_wait_for_tests
        .as_ref()
        .map(|wait| wait.elapsed_ms.load(Ordering::SeqCst));
    let mut first = true;
    let mut decoder = Decoder::new(MAX_PHONE_FRAME_BYTES, MAX_PHONE_FRAME_BYTES);
    let mut buf = vec![0u8; 16 * 1024];
    let mut inbound = Vec::new();
    loop {
        #[cfg(feature = "remote-test-hooks")]
        let elapsed = limits.hello_wait_for_tests.as_ref().map_or_else(
            || started.elapsed(),
            |wait| {
                Duration::from_millis(
                    wait.elapsed_ms
                        .load(Ordering::SeqCst)
                        .saturating_sub(initial_test_elapsed.unwrap()),
                )
            },
        );
        #[cfg(not(feature = "remote-test-hooks"))]
        let elapsed = started.elapsed();
        let within = if first {
            limits.hello_within.saturating_sub(elapsed)
        } else {
            limits.idle
        };
        if within.is_zero() {
            let _ = tx.send(Event::PhoneEnded);
            return;
        }
        #[cfg(feature = "remote-test-hooks")]
        if let (true, Some(wait)) = (first, &limits.hello_wait_for_tests) {
            let _ = wait.calls.send(within);
            if matches!(
                lock(&wait.steps).recv_timeout(Duration::from_secs(60)),
                Ok(HelloReadForTests::Deadline)
            ) {
                wait.advance(within);
                continue;
            }
        }
        if stream.set_read_deadline(Some(within)).is_err() {
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
        kill.bytes_in.fetch_add(got as u64, Ordering::Relaxed);
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
                        kill.kill_because("revoked");
                        return;
                    }
                }
                let written = write_through(&mut stream, &bytes, stall, &|| out.stalled(), &|| kill.dead());
                if written.is_err() {
                    kill.kill_because("write_failed");
                    return;
                }
                kill.bytes_out.fetch_add(bytes.len() as u64, Ordering::Relaxed);
                if last {
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
    origin: Option<String>,
    listener: Listener,
    /// The device, once its `hello` was accepted: what this connection's records name.
    opened: Option<DeviceRef>,
    /// Why the session ended, as `conn_close` says it, when nothing outside closed it.
    why: &'static str,
    subs: BTreeMap<String, Sub>,
    upstreams: BTreeMap<u64, Upstream>,
    rows_wanted: bool,
    feed: Option<(u64, AgentsFeed)>,
    paused: bool,
    next_ping: Instant,
    #[cfg(feature = "remote-test-hooks")]
    now_for_tests: Option<Instant>,
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

/// The kind of an `act`'s command, for its record: only a word of the closed list, so a phone cannot put text of its
/// own into the log as a kind.
fn act_kind(cmd: &serde_json::Value) -> Option<String> {
    cmd.get("type")
        .and_then(serde_json::Value::as_str)
        .and_then(eitri_core::remote_audit::command_kind)
        .map(str::to_owned)
}

/// How a session that ends on `event` says why in its `conn_close`; a close from outside says its own.
fn ending_word(event: &Event) -> &'static str {
    match event {
        Event::Phone(_) => "protocol",
        Event::Bad => "bad_frame",
        Event::Closing(_) => "closed",
        Event::Killed => "killed",
        _ => "ended",
    }
}

impl Control {
    fn now(&self) -> Instant {
        #[cfg(feature = "remote-test-hooks")]
        if let Some(now) = self.now_for_tests {
            return now;
        }
        Instant::now()
    }

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
            let word = ending_word(&event);
            if let Err(end) = self.take(event) {
                self.why = word;
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
        #[cfg(feature = "remote-test-hooks")]
        let deadline = if self.hub.limits.hello_wait_for_tests.is_some() {
            Instant::now() + Duration::from_secs(600)
        } else {
            deadline
        };
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
            let (reason, name) = match self.hub.store.revoked(&device) {
                Some(name) => (Reason::RevokedToken, name),
                None => (Reason::BadToken, self.hub.store.name_of(&device).unwrap_or_default()),
            };
            // Refused before any device is proved, so as bounded as a refusal before `hello`.
            self.hub.audit.record_early_refusal(
                reason,
                Some(DeviceRef { id: device, name }),
                &self.listener,
                &self.peer,
            );
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
        let named = DeviceRef {
            name: self.hub.store.name_of(&device).unwrap_or_default(),
            id: device,
        };
        self.hub.audit.record(AuditEvent::ConnOpen {
            device: named.clone(),
            listener: self.listener.clone(),
            peer: self.peer.clone(),
            host: self.host.clone(),
            origin: self.origin.clone(),
        });
        self.hub.audit.opened(self.id, named.clone(), &self.kill);
        self.opened = Some(named);
        self.hub.report();
        Ok(())
    }

    /// Records a refusal of something this connection sent, where it came in. The tab key is the phone's, so it is
    /// recorded only when it is a key at all.
    fn record_refusal(&self, reason: Reason, device: Option<DeviceRef>, key: Option<String>, kind: Option<String>) {
        self.hub.audit.record(AuditEvent::Refused {
            reason,
            device,
            listener: Some(self.listener.clone()),
            peer: Some(self.peer.clone()),
            key: key.filter(|key| RowKey::parse(key).is_some()),
            kind,
            dropped: None,
        });
    }

    /// The device this connection's `hello` proved, as the log names it.
    fn named(&self) -> Option<DeviceRef> {
        self.opened.clone()
    }

    /// [`Self::named`], or the id `device` alone before the name is known.
    fn device_named(&self, device: &str) -> DeviceRef {
        self.named().unwrap_or_else(|| DeviceRef {
            id: device.to_owned(),
            name: String::new(),
        })
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
        let now = self.now();
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
            #[cfg(feature = "remote-test-hooks")]
            Event::TestTick(now, done) => {
                self.now_for_tests = Some(now);
                self.tick();
                let _ = done.send(());
                Ok(())
            }
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
                    (Grant::Watch, routed) => {
                        self.refuse_watch_act(&device, &key, &cmd, &routed);
                        Err("watch only")
                    }
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
                    (Grant::WatchSteer, routed) => {
                        // No window will see this act: the refusal is recorded here, as a window records one for a
                        // tab it does not hold.
                        let named = self.device_named(&device);
                        self.record_refusal(Reason::TabNotLive, Some(named), Some(key), act_kind(&cmd));
                        Err(match routed {
                            Route::Refused(why) => why,
                            _ => NO_SUCH_TAB,
                        })
                    }
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
                // Only an endpoint the window keeps is recorded as set: it refuses one that fails this same check.
                let host = eitri_core::remote_devices::push_endpoint_ok(url.expose())
                    .ok()
                    .and_then(|()| endpoint_host(url.expose()));
                let line = RvInbound::SetPushEndpoint {
                    v: RV_PROTOCOL,
                    device,
                    token,
                    url,
                };
                match self.send_to_any(&line) {
                    Err(why) => self.local(refused_line(None, why)),
                    // The window checks the endpoint again and keeps it; only its host is ever recorded.
                    Ok(()) => {
                        if let (Some(device), Some(host)) = (self.named(), host) {
                            self.hub.audit.record(AuditEvent::PushEndpointSet {
                                device,
                                host: Some(host),
                            });
                        }
                    }
                }
            }
            PhoneInbound::SetFinishedKeys { keys } => self.hub.set_finished_keys(&device, keys),
            PhoneInbound::AuditMine { limit } => match self.hub.audit.mine(&device, limit) {
                Some(line) => self.local(line),
                None => self.local(refused_line(None, "audit_mine: too often; ask again in a second")),
            },
        }
        Ok(())
    }

    /// An `act` from a `watch` device, refused here: recorded with the command's kind (never its text), and the
    /// window that owns the tab is told, so the desk sees the attempt as it sees its own refusals.
    fn refuse_watch_act(&mut self, device: &str, key: &str, cmd: &serde_json::Value, routed: &Route) {
        let named = self.device_named(device);
        self.record_refusal(Reason::Grant, Some(named.clone()), Some(key.to_owned()), act_kind(cmd));
        if let Route::Owner { instance } = routed {
            let notice = RvInbound::RefusedNotice {
                v: RV_PROTOCOL,
                device: named.name,
                device_id: named.id,
                key: key.to_owned(),
                reason: Reason::Grant.as_str().to_owned(),
            };
            let _ = self.send_to(instance, &notice);
        }
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
                recipe,
            } => {
                let line = RvInbound::Enroll {
                    v: RV_PROTOCOL,
                    code,
                    device_name,
                    host: self.host.clone(),
                    peer: self.peer.clone(),
                    recipe,
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
                    unknown_since: Some(self.now()),
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
        let now = self.now();
        let give_up = self.subs.get_mut(key).is_some_and(|sub| {
            sub.tries += 1;
            sub.not_before = Some(now + RECONNECT_PAUSE);
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
            shut_both_ways(&writer);
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
            shut_both_ways(&stream);
        }
        Some(upstream)
    }

    /// A window closed the connection `id` (or it failed): its tabs are followed again, through a new one, after
    /// [`RECONNECT_PAUSE`]. A tab the window had not answered on it counts the connection as a failed try.
    fn upstream_ended(&mut self, id: u64) {
        let Some(upstream) = self.close_upstream(id) else {
            return;
        };
        let now = self.now();
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
        let now = self.now();
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
        let now = self.now();
        let after_move = self.subs.get_mut(&key).is_some_and(|sub| {
            if sub.moved_from.is_none() {
                return false;
            }
            // Asked again after a move that has not landed: wait for the row instead.
            sub.via = None;
            sub.not_before = Some(now + MOVE_SETTLE);
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
        let now = self.now();
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
        let now = self.now();
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
        let now = self.now();
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
        let now = self.now();
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
    use eitri_core::remote_audit::{JournalTarget, WriterKind};

    fn audit_in(label: &str) -> (Arc<Audit>, PathBuf) {
        let dir = agent::state_dirs::test_workspace_dir(label).join("audit");
        let log = AuditLog::open(&dir, WriterKind::Serve, None, JournalTarget::Off).unwrap();
        (Audit::new(log, &dir), dir)
    }

    #[test]
    fn refusals_before_a_device_is_proved_are_bounded_and_counted() {
        let (audit, dir) = audit_in("audit-early-bound");
        let all = || eitri_core::remote_audit::read(&dir, &Filter::default(), None).unwrap();
        let start = Instant::now();
        let refuse = |reason, at| audit.record_early_refusal_at(reason, None, &Listener::Socket, "unix", at);
        for _ in 0..EARLY_REFUSALS_BURST + 10 {
            refuse(Reason::Host, start);
        }
        assert_eq!(
            all().len(),
            EARLY_REFUSALS_BURST as usize,
            "a burst is recorded, then counted"
        );
        // One more after the pause, with the count of those that were not recorded.
        refuse(Reason::Origin, start + EARLY_REFUSAL_EVERY);
        let records = all();
        assert_eq!(records.len(), EARLY_REFUSALS_BURST as usize + 1);
        assert_eq!(records[0].fields["reason"], "origin");
        assert_eq!(records[0].fields["dropped"], 10);
        assert!(records[1].fields.get("dropped").is_none(), "{}", records[1].line);
        // Those still only counted when `serve` ends are recorded before its `disable`.
        for _ in 0..3 {
            refuse(Reason::PathPrefix, start + EARLY_REFUSAL_EVERY);
        }
        audit.disable_once("idle");
        let records = all();
        assert_eq!(records[0].event, "disable");
        assert_eq!(records[1].fields["reason"], "path_prefix");
        assert_eq!(records[1].fields["dropped"], 3);
        assert!(records[1].fields.get("peer").is_none(), "{}", records[1].line);
        // A long pause fills the bucket again, and no further.
        let later = start + EARLY_REFUSAL_EVERY * 1000;
        let before = all().len();
        for _ in 0..EARLY_REFUSALS_BURST + 1 {
            refuse(Reason::Host, later);
        }
        assert_eq!(all().len() - before, EARLY_REFUSALS_BURST as usize);
    }

    #[test]
    fn audit_mine_finds_this_devices_records_behind_another_device_named_like_it() {
        let (audit, _dir) = audit_in("audit-mine-named-like");
        let named = |id: &str, name: &str| DeviceRef {
            id: id.into(),
            name: name.into(),
        };
        audit.record(AuditEvent::DeviceFlagged {
            device: named("d1", "phone"),
        });
        for _ in 0..56 {
            audit.record(AuditEvent::DeviceFlagged {
                device: named("d5", "d1"),
            });
        }
        let answer: serde_json::Value = serde_json::from_str(&audit.mine("d1", 20).unwrap()).unwrap();
        let records = answer["records"].as_array().unwrap();
        assert_eq!(records.len(), 1, "{answer}");
        assert_eq!(records[0]["device"], "d1");
    }

    #[test]
    fn an_acts_kind_is_recorded_only_as_a_known_word() {
        use serde_json::json;
        assert_eq!(act_kind(&json!({"type": "send", "text": "t"})).as_deref(), Some("send"));
        assert_eq!(act_kind(&json!({"type": "take_back"})).as_deref(), Some("take_back"));
        for cmd in [
            json!({"type": "a prompt the phone chose"}),
            json!({"type": "Send"}),
            json!({"type": 7}),
            json!({"text": "no type"}),
            json!("not an object"),
        ] {
            assert_eq!(act_kind(&cmd), None, "{cmd}");
        }
    }

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
        Arc::new(Kill::new(phone.try_clone_conn().unwrap(), out.clone(), events.clone()))
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
    fn an_endpoint_is_recorded_by_its_host_alone() {
        for (url, host) in [
            (
                concat!("https://user:secret", "@", "ntfy.example.org:8443/up/abc?x=1"),
                Some("ntfy.example.org"),
            ),
            ("https://[2001:db8::1]:443/t", Some("[2001:db8::1]")),
            ("http://push.example", Some("push.example")),
            ("HTTPS://Push.Example/t", Some("push.example")),
            ("not a url", None),
            ("https:///path", None),
            ("https://:8443/path", None),
        ] {
            assert_eq!(endpoint_host(url).as_deref(), host, "{url}");
        }
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
