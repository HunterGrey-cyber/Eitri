//! The `rv-<instance>.sock` server: where a remote observer's server follows this window's tabs and sends their
//! remote messages.
//!
//! The socket's posture is the control sockets': the control directory checked (`panel_control::ensure_dir`), the
//! socket 0600 with its path built in `eitri-core` through `agent::socket_path::in_dir`, and a peer served only when
//! its uid is this process's effective uid, checked before a byte is read. It is still a door any process of this
//! user can open, an agent's own shell included, so reading is all it gives without more: `subscribe` follows a tab,
//! which shows nothing such a process cannot read from the agent's transcript, while an `act` is acted on only when
//! its device id and token match the device store and that device's grant allows steering, checked before the
//! message reaches [`AgentPanelHandle::remote_message`] and its closed list. Nothing here answers a card, a question
//! or a trust prompt; that list has no such message.
//!
//! No socket I/O happens on the tick's thread. One thread accepts, and each connection has a reader and a writer of
//! its own: the reader takes one line at a time (at most [`RV_MAX_LINE`] bytes, the rest of a line within
//! [`RV_LINE_WITHIN`], the protocol version read first) and hands it to the tick through a bounded channel, raising
//! the wake; the writer writes what the tick queued and raises the wake when it has room again after the tick found
//! it full. [`RemoteSocket::pump`], called from the tick, handles the queued lines and then takes an observer's
//! frames only while its connection's writer holds less than [`WRITER_BYTES`], so a peer that stops reading stops
//! its frames where they are: its observer's own cap and snapshot take over, and nothing the tick does waits for it.
//! A peer that keeps sending and never reads its answers is closed once [`CONTROL_LINES_MAX`] of them wait. A peer
//! that ends its sending side ends the connection: its reader shuts the stream both ways, which also frees a writer
//! held in a write the peer never reads.

use super::*;
use crate::panel_wake::PanelWake;
use eitri_core::remote_devices::{DeviceStore, Grant};
use eitri_core::remote_rv::{
    frame_line, parse_rv_line, refused_line, result_line, RvInbound, RvListener, RV_LINE_WITHIN, RV_MAX_CONNECTIONS,
    RV_MAX_LINE, RV_MAX_OBSERVERS_PER_CONNECTION,
};
use eitri_core::tab_set::{ObserveError, ObserverFrame, ObserverId, RowKey};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The bytes one connection's writer may hold before its observers' frames wait in the tab set.
pub const WRITER_BYTES: usize = 1024 * 1024;

/// The answers (`result`, `refused`) one connection may have waiting; one more closes it.
pub const CONTROL_LINES_MAX: usize = 64;

/// The lines the readers may have queued for the tick; a reader past it waits, and so does its peer.
const INBOUND_LINES: usize = 64;

/// The most queued lines one pump handles; the rest raise the wake for the next.
const INBOUND_PER_PUMP: usize = 64;

/// How long a refusal to a connection over the cap may take to write.
const REFUSAL_WITHIN: Duration = Duration::from_millis(100);

/// What a connection's writer holds, shared by the tick, which adds, and the writer, which takes away.
#[derive(Default)]
struct Gauge {
    bytes: AtomicUsize,
    control: AtomicUsize,
    /// The tick found the writer full and left frames waiting: the writer raises the wake when it has room.
    starved: AtomicBool,
}

/// A line for a connection's writer.
struct Out {
    line: String,
    control: bool,
}

/// What a connection's threads hand the tick.
enum Inbound {
    Opened {
        conn: u64,
        stream: UnixStream,
        out: mpsc::Sender<Out>,
        gauge: Arc<Gauge>,
    },
    Subscribe {
        conn: u64,
        key: String,
    },
    Unsubscribe {
        conn: u64,
        key: String,
    },
    /// An `act` whose device was checked on the reader's thread: the device's id and name when it may steer, else
    /// why not.
    Act {
        conn: u64,
        key: String,
        verdict: Result<(String, String), &'static str>,
        raw: String,
        command_id: Option<String>,
    },
    Closed {
        conn: u64,
    },
}

/// One connection as the tick holds it.
struct Conn {
    /// A clone of the connection, to close it from the tick.
    stream: UnixStream,
    out: mpsc::Sender<Out>,
    gauge: Arc<Gauge>,
    observers: BTreeMap<RowKey, ObserverId>,
}

impl Conn {
    fn queue(&self, line: String, control: bool) {
        let bytes = line.len() + 1;
        self.gauge.bytes.fetch_add(bytes, Ordering::SeqCst);
        if control {
            self.gauge.control.fetch_add(1, Ordering::SeqCst);
        }
        let mut line = line;
        line.push('\n');
        // A writer that ended has closed the connection; its reader reports it.
        let _ = self.out.send(Out { line, control });
    }

    /// Whether the writer has room for frames. Marks it starved first, so a writer that drains in between either is
    /// seen to have room here or sees the mark and raises the wake.
    fn has_room(&self) -> bool {
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
    fn wait_for_room(&self, wake: &agent::UiWake) {
        self.gauge.starved.store(true, Ordering::SeqCst);
        if self.gauge.bytes.load(Ordering::SeqCst) <= WRITER_BYTES / 2
            && self.gauge.starved.swap(false, Ordering::SeqCst)
        {
            wake();
        }
    }

    fn close(&self) {
        let _ = self.stream.shutdown(Shutdown::Both);
    }
}

/// What the accepting and reading threads share.
struct Shared {
    inbound: SyncSender<Inbound>,
    wake: agent::UiWake,
    store: PathBuf,
    live: AtomicUsize,
    next_conn: AtomicU64,
    /// The socket's owner is gone: a connection accepted from now on is shut at once.
    stopped: AtomicBool,
}

/// A bound `rv-` socket, its threads, and what the tick holds of its connections.
///
/// Dropping it removes the socket while the name still holds the one bound and closes every connection; its
/// observers stay registered in the tab set until [`RemoteSocket::close`] forgets them.
pub struct RemoteSocket {
    listener: Option<RvListener>,
    shared: Arc<Shared>,
    inbound: Receiver<Inbound>,
    conns: RefCell<BTreeMap<u64, Conn>>,
}

impl RemoteSocket {
    /// Binds `rv-<instance>.sock` in `control_dir` and starts accepting. `act` lines are checked against the device
    /// store at `store_path`, read again for each, so a device taken out of it can no longer steer. `wake` is raised
    /// whenever a line waits for [`pump`](Self::pump) and whenever a writer that was full has room again.
    ///
    /// Fails, having created nothing, when the path would pass the socket path limit or `instance` is not an
    /// instance id; fails when the directory is not private or the name is taken.
    pub fn bind(control_dir: &Path, instance: &str, store_path: &Path, wake: &PanelWake) -> io::Result<RemoteSocket> {
        let (tx, rx) = mpsc::sync_channel(INBOUND_LINES);
        let shared = Arc::new(Shared {
            inbound: tx,
            wake: wake.waker(),
            store: store_path.to_path_buf(),
            live: AtomicUsize::new(0),
            next_conn: AtomicU64::new(1),
            stopped: AtomicBool::new(false),
        });
        let accepting = Arc::clone(&shared);
        let listener = RvListener::bind(control_dir, instance, move |stream| {
            start_connection(stream, &accepting)
        })?;
        Ok(RemoteSocket {
            listener: Some(listener),
            shared,
            inbound: rx,
            conns: RefCell::new(BTreeMap::new()),
        })
    }

    /// Called from the tick: handles queued inbound lines, then takes frames only for observers whose writer has
    /// room. Never waits on a socket.
    pub fn pump(&self, handle: &AgentPanelHandle) {
        let mut handled = 0;
        while handled < INBOUND_PER_PUMP {
            let Ok(inbound) = self.inbound.try_recv() else {
                break;
            };
            self.take(handle, inbound);
            handled += 1;
        }
        if handled == INBOUND_PER_PUMP {
            (self.shared.wake)();
        }
        self.send_frames(handle);
    }

    /// Forgets every observer of this socket's connections, then closes it.
    pub fn close(self, handle: &AgentPanelHandle) {
        let conns = std::mem::take(&mut *self.conns.borrow_mut());
        let mut state = handle.state.borrow_mut();
        for conn in conns.values() {
            for id in conn.observers.values() {
                state.tabs.unobserve(*id);
            }
            conn.close();
        }
    }

    fn take(&self, handle: &AgentPanelHandle, inbound: Inbound) {
        match inbound {
            Inbound::Opened {
                conn,
                stream,
                out,
                gauge,
            } => {
                self.conns.borrow_mut().insert(
                    conn,
                    Conn {
                        stream,
                        out,
                        gauge,
                        observers: BTreeMap::new(),
                    },
                );
            }
            Inbound::Closed { conn } => {
                if let Some(conn) = self.conns.borrow_mut().remove(&conn) {
                    conn.close();
                    let mut state = handle.state.borrow_mut();
                    for id in conn.observers.values() {
                        state.tabs.unobserve(*id);
                    }
                }
            }
            Inbound::Subscribe { conn, key } => self.subscribe(handle, conn, &key),
            Inbound::Unsubscribe { conn, key } => {
                let Some(key) = RowKey::parse(&key) else {
                    return;
                };
                let id = self
                    .conns
                    .borrow_mut()
                    .get_mut(&conn)
                    .and_then(|c| c.observers.remove(&key));
                if let Some(id) = id {
                    handle.state.borrow_mut().tabs.unobserve(id);
                }
            }
            Inbound::Act {
                conn,
                key,
                verdict,
                raw,
                command_id,
            } => {
                if !self.conns.borrow().contains_key(&conn) {
                    return;
                }
                let outcome = match (verdict, RowKey::parse(&key)) {
                    (Err(why), _) => Err(why.to_owned()),
                    (Ok(_), None) => Err(refusal_text(&RemoteRefusal::NoSuchTab)),
                    (Ok((device_id, device)), Some(key)) => handle
                        .remote_message(
                            &RemoteOrigin {
                                device_id,
                                device,
                                conn,
                            },
                            &key,
                            &raw,
                        )
                        .map_err(|why| refusal_text(&why)),
                };
                let line = result_line(
                    command_id.as_deref(),
                    outcome.as_ref().map(|_| ()).map_err(String::as_str),
                );
                self.answer(conn, line);
            }
        }
    }

    fn subscribe(&self, handle: &AgentPanelHandle, conn: u64, key: &str) {
        let mut conns = self.conns.borrow_mut();
        let Some(c) = conns.get_mut(&conn) else {
            return;
        };
        let Some(key) = RowKey::parse(key) else {
            drop(conns);
            return self.answer(conn, refused_line(None, "no such tab"));
        };
        let refusal = if c.observers.contains_key(&key) {
            Some("already subscribed")
        } else if c.observers.len() >= RV_MAX_OBSERVERS_PER_CONNECTION {
            Some("too many subscriptions")
        } else {
            let observed = handle.state.borrow_mut().tabs.observe(&key);
            match observed {
                Ok(id) => {
                    c.observers.insert(key.clone(), id);
                    None
                }
                Err(ObserveError::NoSuchTab) => Some("no such tab"),
                Err(ObserveError::NotLive) => Some("not live"),
            }
        };
        drop(conns);
        if let Some(why) = refusal {
            self.answer(conn, refused_line(Some(&key), why));
        }
    }

    /// Queues an answer, or closes a connection whose peer has left too many unread.
    fn answer(&self, conn: u64, line: String) {
        let conns = self.conns.borrow();
        let Some(c) = conns.get(&conn) else {
            return;
        };
        if c.gauge.control.load(Ordering::SeqCst) >= CONTROL_LINES_MAX {
            c.close();
            return;
        }
        c.queue(line, true);
    }

    fn send_frames(&self, handle: &AgentPanelHandle) {
        let mut conns = self.conns.borrow_mut();
        if conns.is_empty() {
            return;
        }
        let mut state = handle.state.borrow_mut();
        let owners: BTreeMap<ObserverId, u64> = conns
            .iter()
            .flat_map(|(conn, c)| c.observers.values().map(move |id| (*id, *conn)))
            .collect();
        for id in state.tabs.observers_with_frames() {
            let Some(conn) = owners.get(&id) else {
                continue;
            };
            let Some(c) = conns.get_mut(conn) else {
                continue;
            };
            if !c.has_room() {
                continue;
            }
            let budget = WRITER_BYTES.saturating_sub(c.gauge.bytes.load(Ordering::SeqCst));
            for frame in state.tabs.take_observer_frames(id, budget) {
                if let ObserverFrame::Gone { key, .. } = &frame {
                    c.observers.remove(key);
                }
                c.queue(frame_line(&frame), false);
            }
        }
        // Frames the budget left behind, or that found the writer full, wait for that writer's room.
        let waiting: std::collections::BTreeSet<u64> = state
            .tabs
            .observers_with_frames()
            .iter()
            .filter_map(|id| owners.get(id).copied())
            .collect();
        for conn in waiting {
            if let Some(c) = conns.get(&conn) {
                c.wait_for_room(&self.shared.wake);
            }
        }
    }
}

impl Drop for RemoteSocket {
    fn drop(&mut self) {
        // The socket goes first, so no new peer finds it while the connections close.
        self.listener.take();
        // The accepting thread is not waited for: a peer it is handing on now sees this and is shut.
        self.shared.stopped.store(true, Ordering::SeqCst);
        for conn in self.conns.borrow().values() {
            conn.close();
        }
        // Connections accepted but not yet taken in by a pump.
        while let Ok(inbound) = self.inbound.try_recv() {
            if let Inbound::Opened { stream, .. } = inbound {
                let _ = stream.shutdown(Shutdown::Both);
            }
        }
    }
}

/// How a refusal reads in a `result` line.
fn refusal_text(why: &RemoteRefusal) -> String {
    match why {
        RemoteRefusal::Parse => "malformed".to_owned(),
        RemoteRefusal::NoSuchTab => "no such tab".to_owned(),
        RemoteRefusal::NotClaude => "not a Claude tab".to_owned(),
        RemoteRefusal::NotLive => "not live".to_owned(),
        RemoteRefusal::WorkChanged => "work changed".to_owned(),
        RemoteRefusal::Duplicate => "duplicate command id".to_owned(),
        RemoteRefusal::Agent(text) => text.replace(['\n', '\r'], " "),
    }
}

/// Starts a peer's writer and reader, or refuses it when the cap is reached. `false` once the socket's owner is gone.
fn start_connection(stream: UnixStream, shared: &Arc<Shared>) -> bool {
    if shared.live.load(Ordering::SeqCst) >= RV_MAX_CONNECTIONS {
        let _ = stream.set_write_timeout(Some(REFUSAL_WITHIN));
        let _ = (&stream).write_all(format!("{}\n", refused_line(None, "too many connections")).as_bytes());
        return true;
    }
    let (Ok(writer), Ok(tick)) = (stream.try_clone(), stream.try_clone()) else {
        return true;
    };
    let conn = shared.next_conn.fetch_add(1, Ordering::Relaxed);
    let gauge = Arc::new(Gauge::default());
    let (out, lines) = mpsc::channel();
    let writer_gauge = Arc::clone(&gauge);
    let writer_wake = Arc::clone(&shared.wake);
    if std::thread::Builder::new()
        .name("eitri-remote-write".into())
        .spawn(move || write_loop(&writer, &lines, &writer_gauge, &writer_wake))
        .is_err()
    {
        return true;
    }
    shared.live.fetch_add(1, Ordering::SeqCst);
    let opened = Inbound::Opened {
        conn,
        stream: tick,
        out,
        gauge,
    };
    if shared.inbound.send(opened).is_err() {
        shared.live.fetch_sub(1, Ordering::SeqCst);
        return false;
    }
    // Sent after the owner's drop had taken what was queued: nobody will close it, so it is closed here; its reader
    // then ends at once.
    if shared.stopped.load(Ordering::SeqCst) {
        let _ = stream.shutdown(Shutdown::Both);
    }
    let reader_shared = Arc::clone(shared);
    let reader_stream = match stream.try_clone() {
        Ok(s) => s,
        Err(_) => {
            closed(shared, conn);
            return true;
        }
    };
    if std::thread::Builder::new()
        .name("eitri-remote-read".into())
        .spawn(move || {
            read_loop(&reader_stream, conn, &reader_shared);
            // Both ways, whatever ended the reading, so a writer held in a write the peer never reads is let go.
            let _ = reader_stream.shutdown(Shutdown::Both);
            closed(&reader_shared, conn);
        })
        .is_err()
    {
        let _ = stream.shutdown(Shutdown::Both);
        closed(shared, conn);
    }
    true
}

/// A connection ended: the tick forgets it and its observers, and the cap has room for one more.
fn closed(shared: &Shared, conn: u64) {
    let _ = shared.inbound.send(Inbound::Closed { conn });
    shared.live.fetch_sub(1, Ordering::SeqCst);
    (shared.wake)();
}

/// A connection's writer: writes each queued line whole, and raises the wake when it has room again for a tick that
/// found it full. Ends when the connection fails or the tick forgets it.
fn write_loop(stream: &UnixStream, lines: &Receiver<Out>, gauge: &Gauge, wake: &agent::UiWake) {
    for out in lines {
        let wrote = (&*stream).write_all(out.line.as_bytes());
        gauge.bytes.fetch_sub(out.line.len(), Ordering::SeqCst);
        if out.control {
            gauge.control.fetch_sub(1, Ordering::SeqCst);
        }
        if wrote.is_err() {
            let _ = stream.shutdown(Shutdown::Both);
            return;
        }
        if gauge.bytes.load(Ordering::SeqCst) <= WRITER_BYTES / 2 && gauge.starved.swap(false, Ordering::SeqCst) {
            wake();
        }
    }
}

/// A connection's reader: one line at a time to the tick, until the peer closes or breaks a rule (a line over
/// [`RV_MAX_LINE`], one not finished within [`RV_LINE_WITHIN`], another protocol version, anything that does not
/// parse), which closes the connection.
fn read_loop(stream: &UnixStream, conn: u64, shared: &Shared) {
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = vec![0u8; 64 * 1024];
    // How much of `buf` is known to hold no newline, so a long line is searched once, not once per read.
    let mut scanned = 0;
    // When the first byte of the line now in `buf` came.
    let mut started: Option<Instant> = None;
    loop {
        while let Some(at) = buf[scanned..].iter().position(|&b| b == b'\n') {
            let end = scanned + at;
            scanned = 0;
            let line: Vec<u8> = buf.drain(..=end).collect();
            let line = &line[..line.len() - 1];
            if line.len() > RV_MAX_LINE || !hand_on(line, conn, shared) {
                let _ = stream.shutdown(Shutdown::Both);
                return;
            }
            started = (!buf.is_empty()).then(Instant::now);
        }
        scanned = buf.len();
        if buf.len() > RV_MAX_LINE {
            let _ = stream.shutdown(Shutdown::Both);
            return;
        }
        let timeout = match started {
            None => None,
            Some(at) => {
                let left = (at + RV_LINE_WITHIN).saturating_duration_since(Instant::now());
                if left.is_zero() {
                    let _ = stream.shutdown(Shutdown::Both);
                    return;
                }
                // A zero timeout is an error to set, and means "no timeout" to some platforms.
                Some(left.max(Duration::from_millis(1)))
            }
        };
        if stream.set_read_timeout(timeout).is_err() {
            let _ = stream.shutdown(Shutdown::Both);
            return;
        }
        match (&*stream).read(&mut chunk) {
            Ok(0) => return,
            Ok(n) => {
                if buf.is_empty() {
                    started = Some(Instant::now());
                }
                buf.extend_from_slice(&chunk[..n]);
            }
            Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {}
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return,
        }
    }
}

/// Parses one line and hands it to the tick, raising the wake. An `act`'s device is checked here, off the tick, and
/// its token goes no further. `false` for a line that closes the connection.
fn hand_on(line: &[u8], conn: u64, shared: &Shared) -> bool {
    let inbound = match parse_rv_line(line) {
        Ok(RvInbound::Subscribe { key, .. }) => Inbound::Subscribe { conn, key },
        Ok(RvInbound::Unsubscribe { key, .. }) => Inbound::Unsubscribe { conn, key },
        Ok(RvInbound::Act {
            device,
            token,
            key,
            cmd,
            ..
        }) => {
            let verdict = match DeviceStore::load(&shared.store) {
                Ok(store) => match store.verify(&device, &token) {
                    Some((name, Grant::WatchSteer)) => Ok((device, name)),
                    Some((_, Grant::Watch)) => Err("watch only"),
                    None => Err("not authorized"),
                },
                Err(e) => {
                    eprintln!("[agent_panel] remote: the device store is not used: {e}");
                    Err("not authorized")
                }
            };
            let command_id = cmd
                .get("command_id")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned);
            Inbound::Act {
                conn,
                key,
                verdict,
                raw: cmd.to_string(),
                command_id,
            }
        }
        Err(_) => return false,
    };
    if shared.inbound.send(inbound).is_err() {
        return false;
    }
    (shared.wake)();
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_panel::tests::*;
    use eitri_core::remote_devices::token_hash_hex;
    use eitri_core::remote_rv::{rv_socket_path, RvClient};
    use std::os::unix::fs::DirBuilderExt;
    use std::sync::atomic::AtomicU32;

    /// How long any one step may take on a loaded machine; a test that passes never waits it out.
    const PATIENCE: Duration = Duration::from_secs(30);

    const INSTANCE: &str = "b4e1c09a7d3f5e21";

    /// A private directory standing in for the control directory, removed on drop.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Scratch {
            static SERIAL: AtomicU32 = AtomicU32::new(0);
            let base = std::env::var_os("XDG_RUNTIME_DIR")
                .map(PathBuf::from)
                .filter(|p| p.is_absolute())
                .unwrap_or_else(std::env::temp_dir);
            let path = base.join(format!(
                "rs-{}-{}",
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

    /// The store: `d1` ("phone", token `tok`) may steer, `d2` ("tablet", token `wtok`) may only watch, `d3` (also
    /// "phone", token `tok3`) may steer.
    fn write_store(dir: &Path) -> PathBuf {
        let path = dir.join("devices.json");
        let body = serde_json::json!({
            "v": 1,
            "devices": [
                { "id": "d1", "name": "phone", "token_sha256": token_hash_hex("tok"), "grant": "watch+steer" },
                { "id": "d2", "name": "tablet", "token_sha256": token_hash_hex("wtok"), "grant": "watch" },
                { "id": "d3", "name": "phone", "token_sha256": token_hash_hex("tok3"), "grant": "watch+steer" },
            ],
        });
        agent::private_fs::write_private(&path, body.to_string().as_bytes()).unwrap();
        path
    }

    struct Rig {
        handle: AgentPanelHandle,
        socket: Option<RemoteSocket>,
        path: PathBuf,
        wake: PanelWake,
        dir: PathBuf,
        _scratch: Scratch,
    }

    impl Rig {
        fn new(label: &str) -> Rig {
            agent::state_dirs::redirect_state_to_a_test_root();
            let dir = agent::state_dirs::test_workspace_dir(label);
            let (handle, _base, _second) = handle_with_two_pages();
            let scratch = Scratch::new();
            let store = write_store(&scratch.0);
            let wake = PanelWake::new().unwrap();
            let socket = RemoteSocket::bind(&scratch.0, INSTANCE, &store, &wake).expect("the socket binds");
            let path = rv_socket_path(&scratch.0, INSTANCE).unwrap();
            Rig {
                handle,
                socket: Some(socket),
                path,
                wake,
                dir,
                _scratch: scratch,
            }
        }

        fn socket(&self) -> &RemoteSocket {
            self.socket.as_ref().unwrap()
        }

        fn connect(&self) -> RvClient {
            RvClient::connect(&self.path).expect("the client connects")
        }

        /// One tick and one pump.
        fn turn(&self) {
            self.handle.tick();
            self.socket().pump(&self.handle);
        }

        /// A new Claude tab whose provider can interrupt, and its row key.
        fn tab(&self) -> (TabId, Arc<RecordingProvider>, RowKey) {
            let provider = Arc::new(RecordingProvider::interruptible());
            let conversation =
                agent::AgentConversation::create(provider.clone(), &self.dir, ProjectTrust::Untrusted).unwrap();
            let backend = AgentBackend::Conversation {
                agent: AgentId::Claude,
                conversation: Box::new(conversation),
            };
            let mut state = self.handle.state.borrow_mut();
            let tab = state.tabs.open(AgentId::Claude);
            let held = state.tabs.get_mut(tab).unwrap();
            held.set_backend(TabBackend::Live(backend));
            let key = held.row_key().clone();
            (tab, provider, key)
        }

        fn generation(&self, tab: TabId) -> u64 {
            self.handle.state.borrow().tabs.get(tab).unwrap().backend_generation()
        }

        fn active_turn(&self, tab: TabId) -> Option<String> {
            let state = self.handle.state.borrow();
            let turn = state.tabs.get(tab).unwrap().live()?.projection().active_turn_id.clone();
            turn
        }

        fn revision(&self, tab: TabId) -> u64 {
            let state = self.handle.state.borrow();
            let revision = state.tabs.get(tab).unwrap().live().unwrap().projection().last_revision;
            revision
        }

        fn start_turn(&self, tab: TabId, provider: &RecordingProvider, turn: &str) {
            provider.queue(agent::AgentDomainEvent::TurnStarted { turn_id: turn.into() });
            tick_handle_until(&self.handle, "the turn", || {
                self.active_turn(tab).as_deref() == Some(turn)
            });
        }

        /// Queues `count` deltas of `text` and turns until they are folded; the revision reached.
        fn fold(&self, tab: TabId, provider: &RecordingProvider, count: usize, text: &str) -> u64 {
            let target = self.revision(tab) + count as u64;
            provider.queue_all((0..count).map(|_| delta("t1", text)).collect());
            let deadline = Instant::now() + PATIENCE;
            while self.revision(tab) < target {
                assert!(Instant::now() < deadline, "the batch was never folded");
                self.turn();
            }
            self.revision(tab)
        }

        /// What every connection's writer holds.
        fn writer_bytes(&self) -> usize {
            self.socket()
                .conns
                .borrow()
                .values()
                .map(|c| c.gauge.bytes.load(Ordering::SeqCst))
                .sum()
        }

        /// Turns until `client` has received a line `done` accepts; every line received on the way, that one last.
        fn until(
            &self,
            client: &mut RvClient,
            what: &str,
            done: impl Fn(&serde_json::Value) -> bool,
        ) -> Vec<serde_json::Value> {
            let deadline = Instant::now() + PATIENCE;
            let mut lines = Vec::new();
            loop {
                assert!(
                    Instant::now() < deadline,
                    "timed out waiting for {what}; got {:?}",
                    summary(&lines)
                );
                self.turn();
                while let Some(line) = client
                    .recv(Duration::from_millis(5))
                    .expect("the connection stays open")
                {
                    let value: serde_json::Value = serde_json::from_str(&line).expect("one JSON object per line");
                    assert_eq!(value["v"], 1, "every line carries the version");
                    let found = done(&value);
                    lines.push(value);
                    if found {
                        return lines;
                    }
                }
            }
        }

        /// Turns until `client`'s connection is closed by the server.
        fn until_closed(&self, client: &mut RvClient, what: &str) {
            let deadline = Instant::now() + PATIENCE;
            loop {
                assert!(Instant::now() < deadline, "timed out waiting for {what}");
                self.turn();
                match client.recv(Duration::from_millis(5)) {
                    Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return,
                    Err(e) if e.kind() == io::ErrorKind::ConnectionReset => return,
                    Err(e) => panic!("{what}: {e}"),
                    Ok(_) => {}
                }
            }
        }

        fn subscribe(&self, client: &mut RvClient, key: &RowKey) {
            client
                .send(&RvInbound::Subscribe {
                    v: 1,
                    key: key.to_string(),
                })
                .unwrap();
        }

        fn act(&self, client: &mut RvClient, device: &str, token: &str, key: &RowKey, cmd: serde_json::Value) {
            client
                .send(&RvInbound::Act {
                    v: 1,
                    device: device.into(),
                    token: token.into(),
                    key: key.to_string(),
                    cmd,
                })
                .unwrap();
        }
    }

    impl Drop for Rig {
        fn drop(&mut self) {
            if let Some(socket) = self.socket.take() {
                socket.close(&self.handle);
            }
            shut_down_every_tab(&self.handle);
        }
    }

    /// Each line's kind and revisions, for a failure message.
    fn summary(lines: &[serde_json::Value]) -> Vec<String> {
        lines
            .iter()
            .map(|l| format!("{} {} {} {}", l["kind"], l["from"], l["through"], l["why"]))
            .collect()
    }

    fn interrupt(command_id: &str, turn: &str, generation: u64) -> serde_json::Value {
        serde_json::json!({
            "type": "interrupt",
            "command_id": command_id,
            "expect_work": { "turn": { "id": turn, "generation": generation } },
        })
    }

    fn delta(turn: &str, text: &str) -> agent::AgentDomainEvent {
        agent::AgentDomainEvent::ContentDelta {
            turn_id: turn.into(),
            kind: agent::ContentKind::Text,
            text: text.into(),
        }
    }

    /// Checks that `lines` (of one key) start with a snapshot and that each events frame begins where the frame
    /// before it ended; a later snapshot starts over. The revision reached.
    fn contiguous(lines: &[serde_json::Value]) -> u64 {
        let frames: Vec<&serde_json::Value> = lines
            .iter()
            .filter(|l| l["kind"] == "snapshot" || l["kind"] == "events")
            .collect();
        assert_eq!(
            frames.first().map(|f| &f["kind"]),
            Some(&serde_json::json!("snapshot")),
            "a snapshot first"
        );
        let mut at = 0;
        for frame in frames {
            if frame["kind"] == "snapshot" {
                at = frame["through"].as_u64().unwrap();
            } else {
                assert_eq!(frame["from"].as_u64(), Some(at), "no gap and no repeat before {frame}");
                at = frame["through"].as_u64().unwrap();
            }
        }
        at
    }

    #[test]
    fn subscribe_streams_snapshot_then_events() {
        let rig = Rig::new("remote-subscribe");
        let (tab, provider, key) = rig.tab();
        rig.start_turn(tab, &provider, "t1");
        let mut client = rig.connect();
        rig.subscribe(&mut client, &key);
        let mut lines = rig.until(&mut client, "the snapshot", |l| l["kind"] == "snapshot");
        assert_eq!(lines[0]["key"], key.as_str());
        assert_eq!(lines[0]["frame"]["kind"], "snapshot", "the desk's own envelope");

        provider.queue(delta("t1", "hello"));
        provider.queue(delta("t1", " world"));
        let target = {
            tick_handle_until(&rig.handle, "the deltas", || {
                rig.revision(tab) >= lines[0]["through"].as_u64().unwrap() + 2
            });
            rig.revision(tab)
        };
        lines.extend(rig.until(&mut client, "the events", |l| {
            l["kind"] == "events" && l["through"].as_u64() == Some(target)
        }));
        assert!(
            lines.iter().any(|l| l["kind"] == "events"),
            "events follow the snapshot"
        );
        assert_eq!(contiguous(&lines), target);
    }

    #[test]
    fn act_without_token_or_with_watch_grant_is_refused() {
        let rig = Rig::new("remote-act-refused");
        let (tab, provider, key) = rig.tab();
        rig.start_turn(tab, &provider, "t1");
        let mut client = rig.connect();
        for (n, (device, token, why)) in [
            ("d1", "", "not authorized"),
            ("d1", "wrong", "not authorized"),
            ("d9", "tok", "not authorized"),
            ("d2", "wtok", "watch only"),
        ]
        .into_iter()
        .enumerate()
        {
            let id = format!("c{n}");
            rig.act(
                &mut client,
                device,
                token,
                &key,
                interrupt(&id, "t1", rig.generation(tab)),
            );
            let lines = rig.until(&mut client, "the result", |l| l["kind"] == "result");
            let result = lines.last().unwrap();
            assert_eq!(result["command_id"], id.as_str());
            assert_eq!(result["ok"], false, "{device} with {token:?}");
            assert_eq!(result["why"], why, "{device} with {token:?}");
        }
        assert_eq!(provider.interrupts(), 0, "nothing stopped");
        assert_eq!(rig.active_turn(tab).as_deref(), Some("t1"), "the turn keeps running");
    }

    #[test]
    fn act_with_steer_grant_interrupts() {
        let rig = Rig::new("remote-act-steer");
        let (tab, provider, key) = rig.tab();
        rig.start_turn(tab, &provider, "t1");
        let mut client = rig.connect();
        // The phone names the session as the frames it was sent name it.
        rig.subscribe(&mut client, &key);
        let lines = rig.until(&mut client, "the snapshot", |l| l["kind"] == "snapshot");
        let generation = lines.last().unwrap()["generation"]
            .as_u64()
            .expect("the snapshot names its session");
        rig.act(&mut client, "d1", "tok", &key, interrupt("c1", "t1", generation));
        let lines = rig.until(&mut client, "the result", |l| l["kind"] == "result");
        let result = lines.last().unwrap();
        assert_eq!(
            result,
            &serde_json::json!({ "v": 1, "kind": "result", "command_id": "c1", "ok": true, "why": null })
        );
        assert_eq!(provider.interrupts(), 1, "the turn was stopped");

        // The list behind the token is still the remote one: a desk-only kind is refused whoever sends it.
        let answer = serde_json::json!({ "type": "permission_response", "command_id": "c2", "permission_id": "p1" });
        rig.act(&mut client, "d1", "tok", &key, answer);
        let lines = rig.until(&mut client, "the result", |l| l["kind"] == "result");
        assert_eq!(lines.last().unwrap()["ok"], false);
        assert_eq!(lines.last().unwrap()["why"], "malformed");
    }

    #[test]
    fn unseen_work_is_stopped_by_the_number_its_frames_carry() {
        let rig = Rig::new("remote-unseen-by-frames");
        let (_tab, provider, key) = rig.tab();
        let mut client = rig.connect();
        rig.subscribe(&mut client, &key);
        rig.until(&mut client, "the snapshot", |l| l["kind"] == "snapshot");
        provider.queue(agent::AgentDomainEvent::CliWorkUnseen);
        let lines = rig.until(&mut client, "a frame naming the stretch", |l| {
            l["kind"] == "events" && l["unseen"].is_u64()
        });
        let stretch = lines.last().unwrap()["unseen"].clone();
        let stop = serde_json::json!({ "type": "interrupt", "command_id": "c1", "expect_work": { "unseen": stretch } });
        rig.act(&mut client, "d1", "tok", &key, stop);
        let lines = rig.until(&mut client, "the result", |l| l["kind"] == "result");
        assert_eq!(lines.last().unwrap()["ok"], true, "{:?}", lines.last());
        assert_eq!(provider.interrupts(), 1, "the unseen work was stopped");
    }

    #[test]
    fn wrong_protocol_version_is_refused() {
        let rig = Rig::new("remote-version");
        let (_tab, _provider, key) = rig.tab();
        let mut client = rig.connect();
        client
            .send(&RvInbound::Subscribe {
                v: 2,
                key: key.to_string(),
            })
            .unwrap();
        rig.until_closed(&mut client, "the close");
        assert!(
            rig.handle.state.borrow().tabs.observers_with_frames().is_empty(),
            "nothing was subscribed"
        );
    }

    #[test]
    fn line_over_cap_closes_the_connection() {
        let rig = Rig::new("remote-line-cap");
        let (tab, provider, key) = rig.tab();
        rig.start_turn(tab, &provider, "t1");
        let mut other = rig.connect();
        let mut raw = UnixStream::connect(&rig.path).unwrap();
        // The server may close before the whole line is written.
        let _ = raw.write_all(&vec![b'x'; RV_MAX_LINE + 2]);
        raw.set_read_timeout(Some(Duration::from_millis(5))).unwrap();
        let deadline = Instant::now() + PATIENCE;
        let mut byte = [0u8; 1];
        loop {
            assert!(Instant::now() < deadline, "the over-long line's connection stays open");
            rig.turn();
            match raw.read(&mut byte) {
                Ok(0) => break,
                Ok(_) => panic!("nothing is answered to an over-long line"),
                Err(e) if e.kind() == io::ErrorKind::ConnectionReset => break,
                Err(_) => {}
            }
        }
        rig.subscribe(&mut other, &key);
        rig.until(&mut other, "the other client's snapshot", |l| l["kind"] == "snapshot");
    }

    #[test]
    fn connection_and_observer_caps_hold() {
        let rig = Rig::new("remote-caps");
        let tabs: Vec<RowKey> = (0..=RV_MAX_OBSERVERS_PER_CONNECTION).map(|_| rig.tab().2).collect();
        let mut clients: Vec<RvClient> = (0..RV_MAX_CONNECTIONS).map(|_| rig.connect()).collect();
        // Each has been taken in: a subscription on each is answered.
        for client in clients.iter_mut() {
            rig.subscribe(client, &tabs[0]);
            rig.until(client, "the snapshot", |l| l["kind"] == "snapshot");
        }
        let mut ninth = rig.connect();
        let lines = rig.until(&mut ninth, "the refusal", |l| l["kind"] == "refused");
        assert_eq!(lines.last().unwrap()["why"], "too many connections");
        rig.until_closed(&mut ninth, "the ninth's close");

        let first = &mut clients[0];
        for key in &tabs[1..RV_MAX_OBSERVERS_PER_CONNECTION] {
            rig.subscribe(first, key);
            rig.until(first, "the snapshot", |l| {
                l["kind"] == "snapshot" && l["key"] == key.as_str()
            });
        }
        let over = &tabs[RV_MAX_OBSERVERS_PER_CONNECTION];
        rig.subscribe(first, over);
        let lines = rig.until(first, "the refusal", |l| l["kind"] == "refused");
        let refusal = lines.last().unwrap();
        assert_eq!(
            (refusal["key"].as_str(), refusal["why"].as_str()),
            (Some(over.as_str()), Some("too many subscriptions"))
        );

        // A connection that leaves makes room for another.
        clients.pop();
        let deadline = Instant::now() + PATIENCE;
        loop {
            assert!(
                Instant::now() < deadline,
                "a place freed by a closed connection is never taken"
            );
            rig.turn();
            let mut again = rig.connect();
            // A refused connection may be closed before this is written.
            let _ = again.send(&RvInbound::Subscribe {
                v: 1,
                key: tabs[1].to_string(),
            });
            let mut answered = None;
            let until = Instant::now() + Duration::from_secs(2);
            while answered.is_none() && Instant::now() < until {
                rig.turn();
                match again.recv(Duration::from_millis(5)) {
                    Ok(Some(line)) => answered = Some(serde_json::from_str::<serde_json::Value>(&line).unwrap()),
                    Ok(None) => {}
                    Err(_) => break,
                }
            }
            if answered.is_some_and(|l| l["kind"] == "snapshot") {
                break;
            }
        }
    }

    #[test]
    fn inbound_line_raises_the_wake() {
        let rig = Rig::new("remote-wake");
        let (_tab, _provider, key) = rig.tab();
        let mut client = rig.connect();
        std::thread::sleep(Duration::from_millis(50));
        rig.wake.take();
        rig.subscribe(&mut client, &key);
        let deadline = Instant::now() + PATIENCE;
        while !rig.wake.take() {
            assert!(Instant::now() < deadline, "the wake was never raised");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn stalled_client_never_blocks_the_tick() {
        const EVENTS: usize = 10_000;
        const BATCH: usize = 100;
        let rig = Rig::new("remote-stalled");
        let (tab, provider, key) = rig.tab();
        rig.start_turn(tab, &provider, "t1");
        let mut client = rig.connect();
        rig.subscribe(&mut client, &key);
        // Taken in, and given its snapshot in the same pump; the client reads none of it.
        let deadline = Instant::now() + PATIENCE;
        while rig.socket().conns.borrow().values().all(|c| c.observers.is_empty()) {
            assert!(Instant::now() < deadline, "the subscription was never taken");
            rig.turn();
        }

        let text = "x".repeat(1024);
        let mut folded = rig.revision(tab);
        for _ in 0..EVENTS / BATCH {
            provider.queue_all((0..BATCH).map(|_| delta("t1", &text)).collect());
            let target = folded + BATCH as u64;
            let deadline = Instant::now() + PATIENCE;
            while rig.revision(tab) < target {
                assert!(Instant::now() < deadline, "the batch was never folded");
                rig.turn();
            }
            folded = rig.revision(tab);
        }
        rig.turn();
        let gauge = |rig: &Rig| {
            rig.socket()
                .conns
                .borrow()
                .values()
                .map(|c| c.gauge.bytes.load(Ordering::SeqCst))
                .sum::<usize>()
        };
        assert!(gauge(&rig) >= WRITER_BYTES, "the writer is full");
        provider.queue(delta("t1", "one more"));
        let deadline = Instant::now() + PATIENCE;
        while rig.revision(tab) <= folded {
            assert!(Instant::now() < deadline, "the last event was never folded");
            rig.turn();
        }
        folded = rig.revision(tab);
        for _ in 0..3 {
            rig.turn();
            assert!(
                !rig.handle.state.borrow().tabs.observers_with_frames().is_empty(),
                "the observer's frames wait in the tab set while the writer is full"
            );
        }

        eprintln!("folded through {folded}");
        let lines = rig.until(&mut client, "the last revision", |l| {
            (l["kind"] == "events" || l["kind"] == "snapshot") && l["through"].as_u64() == Some(folded)
        });
        assert_eq!(contiguous(&lines), folded);
    }

    #[test]
    fn frames_left_by_the_budget_follow_the_wake() {
        let rig = Rig::new("remote-budget-wake");
        let (tab, provider, key) = rig.tab();
        rig.start_turn(tab, &provider, "t1");
        let mut client = rig.connect();
        rig.subscribe(&mut client, &key);
        let deadline = Instant::now() + PATIENCE;
        while rig.socket().conns.borrow().values().all(|c| c.observers.is_empty()) {
            assert!(Instant::now() < deadline, "the subscription was never taken");
            rig.turn();
        }
        // The writer fills, then about twice its room waits in the tab set, under the observer's own cap.
        let text = "x".repeat(1024);
        let mut folded = rig.revision(tab);
        while rig.writer_bytes() < WRITER_BYTES {
            folded = rig.fold(tab, &provider, 100, &text);
        }
        for _ in 0..20 {
            folded = rig.fold(tab, &provider, 100, &text);
        }
        assert!(
            !rig.handle.state.borrow().tabs.observers_with_frames().is_empty(),
            "frames wait in the tab set"
        );

        // From here only the wake drives a pump, as the window's loop does once the backend is quiet.
        rig.wake.take();
        let deadline = Instant::now() + PATIENCE;
        let mut lines: Vec<serde_json::Value> = Vec::new();
        while !lines.iter().any(|l| l["through"].as_u64() == Some(folded)) {
            assert!(
                Instant::now() < deadline,
                "frames stayed behind with nothing to wake the pump; got {:?}",
                summary(&lines)
            );
            if rig.wake.take() {
                rig.turn();
            }
            while let Some(line) = client
                .recv(Duration::from_millis(5))
                .expect("the connection stays open")
            {
                lines.push(serde_json::from_str(&line).expect("one JSON object per line"));
            }
        }
        assert_eq!(contiguous(&lines), folded);
    }

    #[test]
    fn a_half_closed_peer_frees_its_writer() {
        let rig = Rig::new("remote-half-close");
        let (tab, provider, key) = rig.tab();
        rig.start_turn(tab, &provider, "t1");
        // A snapshot of about 1 MB, far more than the socket holds unread.
        let text = "x".repeat(1024);
        for _ in 0..10 {
            rig.fold(tab, &provider, 100, &text);
        }
        let mut raw = UnixStream::connect(&rig.path).unwrap();
        let subscribe = RvInbound::Subscribe {
            v: 1,
            key: key.to_string(),
        };
        raw.write_all(format!("{}\n", serde_json::to_string(&subscribe).unwrap()).as_bytes())
            .unwrap();
        let deadline = Instant::now() + PATIENCE;
        let gauge = loop {
            assert!(Instant::now() < deadline, "the snapshot was never queued");
            rig.turn();
            let queued = rig
                .socket()
                .conns
                .borrow()
                .values()
                .find(|c| !c.observers.is_empty() && c.gauge.bytes.load(Ordering::SeqCst) > 0)
                .map(|c| Arc::clone(&c.gauge));
            if let Some(gauge) = queued {
                break gauge;
            }
        };
        std::thread::sleep(Duration::from_millis(100));
        assert!(
            gauge.bytes.load(Ordering::SeqCst) > 0,
            "the writer is held in its write"
        );

        // The peer stops sending but keeps its receiving side open and never reads.
        raw.shutdown(Shutdown::Write).unwrap();
        let deadline = Instant::now() + PATIENCE;
        while !rig.socket().conns.borrow().is_empty() || Arc::strong_count(&gauge) > 1 {
            assert!(
                Instant::now() < deadline,
                "the writer of a peer that stopped sending is still held"
            );
            rig.turn();
            std::thread::sleep(Duration::from_millis(5));
        }
        drop(raw);
    }

    #[test]
    fn devices_sharing_a_name_keep_their_own_command_ids() {
        let rig = Rig::new("remote-same-name");
        let (tab, provider, key) = rig.tab();
        rig.start_turn(tab, &provider, "t1");
        let mut client = rig.connect();
        rig.act(
            &mut client,
            "d1",
            "tok",
            &key,
            interrupt("c1", "t1", rig.generation(tab)),
        );
        let lines = rig.until(&mut client, "the first result", |l| l["kind"] == "result");
        assert_eq!(lines.last().unwrap()["ok"], true);

        rig.start_turn(tab, &provider, "t2");
        rig.act(
            &mut client,
            "d3",
            "tok3",
            &key,
            interrupt("c1", "t2", rig.generation(tab)),
        );
        let lines = rig.until(&mut client, "the second result", |l| l["kind"] == "result");
        let result = lines.last().unwrap();
        assert_eq!(
            (&result["ok"], &result["why"]),
            (&serde_json::json!(true), &serde_json::Value::Null),
            "another device's command ids are its own, whatever its name"
        );
        assert_eq!(provider.interrupts(), 2);
    }
}
