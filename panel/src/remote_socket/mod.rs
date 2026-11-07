//! The `rv-<instance>.sock` server: where a remote observer's server (`eitri remote serve`) follows this window's tabs,
//! sends their remote messages and runs its errands about devices.
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
//! its own (`conn`): the reader takes one line at a time and hands it to the tick through a bounded channel, raising
//! the wake; the writer writes what the tick queued and raises the wake when it has room again after the tick found
//! it full. [`RemoteSocket::pump`], called from the tick, handles the queued lines and then takes an observer's
//! frames only while its connection's writer holds less than [`WRITER_BYTES`], so a peer that stops reading stops
//! its frames where they are: its observer's own cap and snapshot take over, and nothing the tick does waits for it.
//! What closes a connection, and the caps on its answers, are `conn`'s.
//!
//! Each kind of line has its handler file: `feed` (`subscribe`, `unsubscribe`, `more` and the frames), `acts`
//! (`act`) and `devices` (`serve`'s errands); `ping` is answered here, and so is `refused_notice`: `serve` refused a
//! device's command itself (an `act` from a `watch` device), and the targeted tab gets the same bounded notice as for
//! the window's own refusals (`remote_entry::refusal_notice`, one counter per tab and device id, whichever refused).
//! The line carries no token, so any process of this user could send it: the device must be in the store, the name
//! shown is the store's, the reason one of the audit log's words, and the text a fixed phrase for it; nothing is
//! answered. A kind this build parses and does not carry out is answered `refused` with `not yet`.

use super::*;
use crate::panel_wake::PanelWake;
use eitri_core::remote_devices::{DeviceStore, Grant};
use eitri_core::remote_rv::{
    frame_line, parse_rv_line, pong_line, refused_line, result_line, RvInbound, RvListener, COMMAND_ID_MAX_BYTES,
    CONTROL_BYTES_MAX, RV_IDLE, RV_LINE_WITHIN, RV_MAX_CONNECTIONS, RV_MAX_LINE, RV_MAX_OBSERVERS_PER_CONNECTION,
    RV_WRITE_STALL,
};
use eitri_core::tab_set::{ObserveError, ObserverFrame, ObserverId, ObserverReaper, RowKey};
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

mod acts;
mod conn;
mod devices;
mod feed;
#[cfg(test)]
mod tests;

use acts::ActInbound;
use conn::{start_connection, CloseWhy, Conn, Deadlines, Gauge};
pub use conn::{CONTROL_LINES_MAX, WRITER_BYTES};
use devices::DevicesInbound;
pub use devices::Pairing;
use feed::FeedInbound;

/// The lines the readers may have queued for the tick; a reader past it waits, and so does its peer.
const INBOUND_LINES: usize = 64;

/// The most queued lines one pump handles; the rest raise the wake for the next.
const INBOUND_PER_PUMP: usize = 64;

/// The `why` of a line this build parses and does not carry out.
const NOT_YET: &str = "not yet";

/// What a connection's threads hand the tick.
enum Inbound {
    Sandbox(String),
    Opened {
        conn: u64,
        stream: UnixStream,
        out: mpsc::Sender<conn::Out>,
        gauge: Arc<Gauge>,
    },
    Closed {
        conn: u64,
        why: CloseWhy,
    },
    Ping {
        conn: u64,
    },
    Feed {
        conn: u64,
        line: FeedInbound,
    },
    Act {
        conn: u64,
        act: ActInbound,
    },
    Devices {
        conn: u64,
        line: DevicesInbound,
    },
    /// `serve` refused `device_id` (the store's `device`) something about the tab `key`.
    RefusedNotice {
        key: String,
        device_id: String,
        device: String,
        reason: eitri_core::remote_audit::Reason,
    },
}

/// What the accepting and reading threads share.
struct Shared {
    inbound: SyncSender<Inbound>,
    wake: agent::UiWake,
    store: PathBuf,
    deadlines: Deadlines,
    live: AtomicUsize,
    next_conn: AtomicU64,
    /// The socket's owner is gone: a connection accepted from now on is shut at once.
    stopped: AtomicBool,
    /// What the errands about devices share across the threads: the code on show, where phones connect.
    devices: devices::DevicesShared,
    serve_probe: std::sync::Mutex<Option<Box<ServeProbe>>>,
}

/// Checks the lock only when phones were reported; also reads the sandbox while its record is still pending.
type ServeProbe = dyn FnMut(bool) -> (Option<bool>, Option<String>) + Send;

/// A bound `rv-` socket, its threads, and what the tick holds of its connections.
///
/// Dropping it removes the socket while the name still holds the one bound and closes every connection; its
/// observers are handed to the tab set's reaper, which forgets them at the next tick. [`RemoteSocket::close`] forgets
/// them at once.
pub struct RemoteSocket {
    listener: Option<RvListener>,
    shared: Arc<Shared>,
    inbound: Receiver<Inbound>,
    conns: RefCell<BTreeMap<u64, Conn>>,
    /// The tab set's reaper, taken at the first subscription.
    reaper: RefCell<Option<ObserverReaper>>,
    /// What the feed keeps across pumps: the colours connections were told, the number of the next chunked line.
    feed: feed::FeedState,
    /// Why each connection the tick forgot was closed, in order.
    #[cfg(test)]
    closes: RefCell<Vec<CloseWhy>>,
}

impl RemoteSocket {
    /// Binds `rv-<instance>.sock` in `control_dir` and starts accepting. `act` lines are checked against the device
    /// store at `store_path`, read again for each, so a device taken out of it can no longer steer. `wake` is raised
    /// whenever a line waits for [`pump`](Self::pump) and whenever a writer that was full has room again.
    ///
    /// Fails, having created nothing, when the path would pass the socket path limit or `instance` is not an
    /// instance id; fails when the directory is not private or the name is taken.
    pub fn bind(control_dir: &Path, instance: &str, store_path: &Path, wake: &PanelWake) -> io::Result<RemoteSocket> {
        RemoteSocket::bind_with(control_dir, instance, store_path, wake.waker(), Deadlines::default())
    }

    /// [`bind`](Self::bind) raising `wake` itself: what a window binds off the tick's thread with its panel's raise
    /// ([`AgentPanelHandle::remote_waker`]) before it hands the socket to the panel ([`AgentPanelHandle::set_remote`]).
    pub fn bind_waking(
        control_dir: &Path,
        instance: &str,
        store_path: &Path,
        wake: agent::UiWake,
    ) -> io::Result<RemoteSocket> {
        RemoteSocket::bind_with(control_dir, instance, store_path, wake, Deadlines::default())
    }

    fn bind_with(
        control_dir: &Path,
        instance: &str,
        store_path: &Path,
        wake: agent::UiWake,
        deadlines: Deadlines,
    ) -> io::Result<RemoteSocket> {
        let (tx, rx) = mpsc::sync_channel(INBOUND_LINES);
        let shared = Arc::new(Shared {
            inbound: tx,
            wake,
            store: store_path.to_path_buf(),
            deadlines,
            live: AtomicUsize::new(0),
            next_conn: AtomicU64::new(1),
            stopped: AtomicBool::new(false),
            devices: devices::DevicesShared::new(instance),
            serve_probe: std::sync::Mutex::new(None),
        });
        let accepting = Arc::clone(&shared);
        let maintained = Arc::clone(&shared);
        let listener = RvListener::bind_maintained(
            control_dir,
            instance,
            move |stream| start_connection(stream, &accepting),
            move || devices::check_serve(&maintained),
        )?;
        Ok(RemoteSocket {
            listener: Some(listener),
            shared,
            inbound: rx,
            conns: RefCell::new(BTreeMap::new()),
            reaper: RefCell::new(None),
            feed: feed::FeedState::default(),
            #[cfg(test)]
            closes: RefCell::new(Vec::new()),
        })
    }

    /// Installs the host's lock/record reader. Called off the window's thread; the reader subsequently runs on the
    /// socket's accepting thread every few seconds. `None` for the lock means it was not checked or was unreadable.
    pub fn watch_serve(&self, probe: impl FnMut(bool) -> (Option<bool>, Option<String>) + Send + 'static) {
        *self.shared.serve_probe.lock().unwrap_or_else(|e| e.into_inner()) = Some(Box::new(probe));
    }

    /// Called from the tick: answers no to the desk's questions left past their time (what that tells whoever asked
    /// is handled below, in the same pump), handles queued inbound lines, then takes frames only for observers whose
    /// writer has room. Never waits on a socket.
    pub fn pump(&self, handle: &AgentPanelHandle) {
        if handle.state.borrow().remote_desk.any_waiting() {
            handle.run(|state, out| {
                crate::agent_panel::remote_enroll::expire_questions(state, std::time::Instant::now(), out)
            });
        }
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
        self.deliver_results(handle);
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
            Inbound::Sandbox(sandbox) => {
                handle.run(|state, out| crate::agent_panel::remote_enroll::show_sandbox(state, sandbox, out))
            }
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
            Inbound::Closed { conn, why } => {
                if let Some(conn) = self.conns.borrow_mut().remove(&conn) {
                    conn.close();
                    let mut state = handle.state.borrow_mut();
                    for id in conn.observers.values() {
                        state.tabs.unobserve(*id);
                    }
                }
                if matches!(why, CloseWhy::Idle | CloseWhy::WriteStall) {
                    eprintln!("[agent_panel] remote: connection {conn} closed: {why:?}");
                }
                #[cfg(test)]
                self.closes.borrow_mut().push(why);
            }
            Inbound::Ping { conn } => self.answer(conn, pong_line()),
            Inbound::Feed { conn, line } => self.take_feed(handle, conn, line),
            Inbound::Act { conn, act } => self.take_act(handle, conn, act),
            Inbound::Devices { conn, line } => self.take_devices(handle, conn, line),
            Inbound::RefusedNotice {
                key,
                device_id,
                device,
                reason,
            } => {
                let Some(key) = RowKey::parse(&key) else { return };
                let mut state = handle.state.borrow_mut();
                let Some(tab) = state.tabs.tab_by_key(&key) else { return };
                super::remote_entry::refusal_notice(&mut state, tab, &device_id, &device, notice_words(reason));
                drop(state);
                (self.shared.wake)();
            }
        }
    }

    /// Queues an answer, or closes a connection whose peer has left too many unread, by count or by bytes.
    fn answer(&self, conn: u64, line: String) {
        let conns = self.conns.borrow();
        let Some(c) = conns.get(&conn) else {
            return;
        };
        if !c.gauge.control_fits(line.len() + 1) {
            c.close();
            return;
        }
        c.queue(line, true);
    }
}

impl Drop for RemoteSocket {
    fn drop(&mut self) {
        // The socket goes first, so no new peer finds it while the connections close.
        self.listener.take();
        // The accepting thread is not waited for: a peer it is handing on now sees this and is shut.
        self.shared.stopped.store(true, Ordering::SeqCst);
        let conns = self.conns.borrow();
        for conn in conns.values() {
            conn.close();
        }
        // The tab set is out of reach here: its reaper forgets these observers at the next tick.
        if let Some(reaper) = self.reaper.borrow().as_ref() {
            reaper.hand_over(conns.values().flat_map(|c| c.observers.values().copied()));
            (self.shared.wake)();
        }
        // Connections accepted but not yet taken in by a pump.
        while let Ok(inbound) = self.inbound.try_recv() {
            if let Inbound::Opened { stream, .. } = inbound {
                let _ = stream.shutdown(Shutdown::Both);
            }
        }
    }
}

/// The notice's words for a refusal `serve` made: fixed, so nothing a peer sends is shown.
fn notice_words(reason: eitri_core::remote_audit::Reason) -> &'static str {
    match reason {
        eitri_core::remote_audit::Reason::Grant => "the device may only watch",
        other => other.as_str(),
    }
}

/// Parses one line and hands it to the tick, raising the wake; its handler file reads it first, on this thread, so a
/// credential it carries is checked or dropped here and goes no further. `false` for a line that closes the
/// connection.
fn hand_on(line: &[u8], conn: u64, shared: &Shared) -> bool {
    let Ok(line) = parse_rv_line(line) else {
        return false;
    };
    // Keep the store update and its page report ordered together, including a liveness check's empty report.
    let _ordered = matches!(line, RvInbound::DevicesConnected { .. })
        .then(|| shared.devices.report_order.lock().unwrap_or_else(|e| e.into_inner()));
    let inbound = match line {
        RvInbound::Ping { .. } => Some(Inbound::Ping { conn }),
        RvInbound::Subscribe { .. } | RvInbound::Unsubscribe { .. } | RvInbound::More { .. } => {
            FeedInbound::read(line).map(|line| Inbound::Feed { conn, line })
        }
        RvInbound::Act { .. } => ActInbound::read(line, shared).map(|act| Inbound::Act { conn, act }),
        RvInbound::RefusedNotice {
            device_id, key, reason, ..
        } => {
            let Some(reason) = eitri_core::remote_audit::Reason::parse(&reason) else {
                return true;
            };
            // The store's name, read here off the tick; a device it does not hold leaves no notice.
            let device = match DeviceStore::load(&shared.store) {
                Ok(store) => store.get(&device_id).map(|d| d.name.clone()),
                Err(_) => None,
            };
            let Some(device) = device else {
                return true;
            };
            Some(Inbound::RefusedNotice {
                key,
                device_id,
                device,
                reason,
            })
        }
        RvInbound::Enroll { .. }
        | RvInbound::StartEnroll { .. }
        | RvInbound::GrantRequest { .. }
        | RvInbound::ListenRequest { .. }
        | RvInbound::RevokeSelf { .. }
        | RvInbound::SetPushEndpoint { .. }
        | RvInbound::DevicesConnected { .. } => {
            DevicesInbound::read(line, shared).map(|line| Inbound::Devices { conn, line })
        }
    };
    let Some(inbound) = inbound else {
        return false;
    };
    if shared.inbound.send(inbound).is_err() {
        return false;
    }
    (shared.wake)();
    true
}
