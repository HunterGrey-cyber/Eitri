//! The `rv-<instance>.sock` socket's grammar: what a remote observer's server sends an Eitri process about its tabs,
//! what comes back, the socket's path, and a client for it.
//!
//! One JSON object per line, each carrying the protocol version `v`. Inbound ([`RvInbound`]): `subscribe` and
//! `unsubscribe` name a tab by its row key and need no token, since what they read a process of this user can already
//! read from the agents' own transcripts; `act` carries a device id, that device's token and the remote message for
//! the tab, and is acted on only for a device of the store whose grant allows steering. Outbound, from the Eitri
//! process: `snapshot`, `events` and `gone` frames of a followed tab ([`frame_line`]), `result` for an `act`
//! ([`result_line`]) and `refused` for a subscription or a connection turned down ([`refused_line`]).
//!
//! The socket sits in the control directory beside `ag-<instance>.sock`, built through `agent::socket_path::in_dir`,
//! which refuses a path over the socket path limit. [`RvListener`] binds it with the control sockets' posture (the
//! directory checked, the socket 0600, a peer served only when its uid is this process's effective uid, checked before
//! a byte is read) and hands each such peer to its owner; [`RvClient`] checks the other way, that the socket is served
//! by this user, before it writes a byte.

use crate::panel_control::euid;
use crate::tab_set::{GoneWhy, ObserverFrame, RowKey};
use agent::private_fs::{remove_if_same, FileIdentity};
use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The only version of the `rv-` lines this build speaks.
pub const RV_PROTOCOL: u32 = 1;

/// The longest inbound line, newline not counted; a longer one closes its connection.
pub const RV_MAX_LINE: usize = 1024 * 1024;

/// How many connections a process serves at once; one more is refused.
pub const RV_MAX_CONNECTIONS: usize = 8;

/// How many tabs one connection may follow at once.
pub const RV_MAX_OBSERVERS_PER_CONNECTION: usize = 16;

/// How long the rest of a line may take once its first byte has come.
pub const RV_LINE_WITHIN: Duration = Duration::from_secs(5);

/// The longest outbound line [`RvClient::recv`] takes: a snapshot carries a whole transcript.
pub const RV_MAX_OUTBOUND_LINE: usize = 64 * 1024 * 1024;

/// How long [`RvClient::connect`] waits for the server to accept.
const CONNECT_WITHIN: Duration = Duration::from_secs(2);

/// How many lowercase hex digits an instance id has.
const INSTANCE_HEX: usize = 16;

/// The pause after a failed `poll` or `accept` (descriptors exhausted, say), so the thread never spins.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(50);

/// `control_dir/rv-<instance>.sock`, through `agent::socket_path::in_dir`, which refuses a path over the socket path
/// limit. `instance` must be 16 lowercase hex digits, so no id read from elsewhere can name a path outside the
/// directory.
pub fn rv_socket_path(control_dir: &Path, instance: &str) -> io::Result<PathBuf> {
    let is_instance = instance.len() == INSTANCE_HEX
        && instance
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    if !is_instance {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{instance:?} is not an instance id ({INSTANCE_HEX} lowercase hex digits)"),
        ));
    }
    agent::socket_path::in_dir(control_dir, &format!("rv-{instance}.sock"))
}

/// One inbound line. Any other kind, and any field not named here, fails to parse. Its `Debug` never shows a token.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RvInbound {
    /// Follow the tab `key`: a snapshot, then its events, until `unsubscribe` or `gone`.
    Subscribe { v: u32, key: String },
    /// Stop following the tab `key`.
    Unsubscribe { v: u32, key: String },
    /// The remote message `cmd` about the tab `key`, from `device`, which proves itself with `token`.
    Act {
        v: u32,
        device: String,
        token: String,
        key: String,
        cmd: serde_json::Value,
    },
}

impl std::fmt::Debug for RvInbound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RvInbound::Subscribe { v, key } => f.debug_struct("Subscribe").field("v", v).field("key", key).finish(),
            RvInbound::Unsubscribe { v, key } => f.debug_struct("Unsubscribe").field("v", v).field("key", key).finish(),
            RvInbound::Act {
                v,
                device,
                token: _,
                key,
                cmd,
            } => f
                .debug_struct("Act")
                .field("v", v)
                .field("device", device)
                .field("token", &"<hidden>")
                .field("key", key)
                .field("cmd", cmd)
                .finish(),
        }
    }
}

/// Why an inbound line was not read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RvLineError {
    /// Not one JSON object of a known kind with only the fields it has.
    Malformed,
    /// A `v` this build does not speak.
    Version(u64),
}

/// The inbound frame `line` says. The version is read first, so a newer peer's line is told its version rather than
/// that a field is unknown.
pub fn parse_rv_line(line: &[u8]) -> Result<RvInbound, RvLineError> {
    if line.len() > RV_MAX_LINE {
        return Err(RvLineError::Malformed);
    }
    let value: serde_json::Value = serde_json::from_slice(line).map_err(|_| RvLineError::Malformed)?;
    let version = value
        .get("v")
        .and_then(serde_json::Value::as_u64)
        .ok_or(RvLineError::Malformed)?;
    if version != u64::from(RV_PROTOCOL) {
        return Err(RvLineError::Version(version));
    }
    serde_json::from_value(value).map_err(|_| RvLineError::Malformed)
}

fn json_string(text: &str) -> String {
    serde_json::to_string(text).unwrap_or_else(|_| "\"\"".to_owned())
}

/// The outbound line for one observer frame, without the newline. The desk's envelope goes in `frame` as it was
/// serialized, never parsed again.
pub fn frame_line(frame: &ObserverFrame) -> String {
    match frame {
        ObserverFrame::Snapshot { key, through, json } => format!(
            r#"{{"v":{RV_PROTOCOL},"kind":"snapshot","key":{},"through":{through},"frame":{json}}}"#,
            json_string(key.as_str())
        ),
        ObserverFrame::Events {
            key,
            from,
            through,
            json,
        } => format!(
            r#"{{"v":{RV_PROTOCOL},"kind":"events","key":{},"from":{from},"through":{through},"frame":{json}}}"#,
            json_string(key.as_str())
        ),
        ObserverFrame::Gone { key, why } => {
            let why = match why {
                GoneWhy::Closed => "closed",
                GoneWhy::Moved => "moved",
            };
            format!(
                r#"{{"v":{RV_PROTOCOL},"kind":"gone","key":{},"why":"{why}"}}"#,
                json_string(key.as_str())
            )
        }
    }
}

/// The answer to an `act`: `command_id` as the message gave it (`null` when it gave none), and why it did nothing.
pub fn result_line(command_id: Option<&str>, outcome: Result<(), &str>) -> String {
    serde_json::json!({
        "v": RV_PROTOCOL,
        "kind": "result",
        "command_id": command_id,
        "ok": outcome.is_ok(),
        "why": outcome.err(),
    })
    .to_string()
}

/// A subscription (`key`) or a connection (`None`) turned down, and why.
pub fn refused_line(key: Option<&RowKey>, why: &str) -> String {
    serde_json::json!({
        "v": RV_PROTOCOL,
        "kind": "refused",
        "key": key.map(RowKey::as_str),
        "why": why,
    })
    .to_string()
}

/// A bound `rv-` socket and the thread accepting on it.
///
/// Dropping it removes the socket only while the name still holds the one bound, and ends the accepting thread
/// without waiting for it; connections already handed on are their owner's.
pub struct RvListener {
    dir: std::fs::File,
    name: OsString,
    bound: FileIdentity,
    stop: Arc<AtomicBool>,
    /// The other end of the thread's wake pair: closing it wakes the thread's `poll`.
    stop_pair: Option<UnixStream>,
}

impl RvListener {
    /// Binds `rv-<instance>.sock` in `control_dir` and starts accepting. `serve` runs on the accepting thread with
    /// each peer whose uid is this process's effective uid, checked before a byte is read, as a blocking stream; a
    /// peer of another user is closed unanswered. Accepting stops when `serve` returns `false`.
    ///
    /// Fails, having created nothing, when the path would pass the socket path limit or `instance` is not an instance
    /// id; fails when the directory is not private or the name is taken.
    pub fn bind(
        control_dir: &Path,
        instance: &str,
        serve: impl FnMut(UnixStream) -> bool + Send + 'static,
    ) -> io::Result<RvListener> {
        bind_with(control_dir, instance, crate::panel_control::peer_uid, serve)
    }
}

impl Drop for RvListener {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        // Unlinked first, so no new peer finds it while the thread winds down.
        let _ = remove_if_same(&self.dir, &self.name, self.bound);
        self.stop_pair.take();
    }
}

type PeerUid = fn(&UnixStream) -> io::Result<u32>;

fn bind_with<S>(control_dir: &Path, instance: &str, peer_uid: PeerUid, serve: S) -> io::Result<RvListener>
where
    S: FnMut(UnixStream) -> bool + Send + 'static,
{
    let path = rv_socket_path(control_dir, instance)?;
    let name = path
        .file_name()
        .map(OsString::from)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "a socket path with no file name"))?;
    crate::panel_control::ensure_dir(control_dir)
        .map_err(|why| io::Error::new(io::ErrorKind::PermissionDenied, why))?;
    let dir = agent::private_fs::open_dir(control_dir)?;
    let listener = UnixListener::bind(&path)?;
    let meta = path.symlink_metadata()?;
    let bound = FileIdentity {
        dev: meta.dev(),
        ino: meta.ino(),
    };
    let started = (move || -> io::Result<(UnixStream, Arc<AtomicBool>)> {
        // `bind` leaves the umask's mode on the file.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        listener.set_nonblocking(true)?;
        let (stop_pair, stop_rx) = UnixStream::pair()?;
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        std::thread::Builder::new()
            .name("eitri-remote-accept".into())
            .spawn(move || accept_loop(&listener, &stop_rx, &thread_stop, peer_uid, serve))?;
        Ok((stop_pair, stop))
    })();
    match started {
        Ok((stop_pair, stop)) => Ok(RvListener {
            dir,
            name,
            bound,
            stop,
            stop_pair: Some(stop_pair),
        }),
        Err(e) => {
            let _ = remove_if_same(&dir, &name, bound);
            Err(e)
        }
    }
}

/// The accepting thread: wait for a peer or the stop, check the peer, hand it on.
fn accept_loop<S: FnMut(UnixStream) -> bool>(
    listener: &UnixListener,
    stop_rx: &UnixStream,
    stop: &AtomicBool,
    peer_uid: PeerUid,
    mut serve: S,
) {
    let me = euid();
    while !stop.load(Ordering::Acquire) {
        let mut fds = [
            libc::pollfd {
                fd: listener.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: stop_rx.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        // SAFETY: `fds` is a valid array of two initialized `pollfd`s whose descriptors stay open for the call; the
        // length passed is its length.
        let rc = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, -1) };
        if rc < 0 {
            if io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                std::thread::sleep(ACCEPT_BACKOFF);
            }
            continue;
        }
        // The stop end is never written: anything on it means the listener's owner dropped it.
        if fds[1].revents != 0 {
            return;
        }
        match listener.accept() {
            Ok((stream, _)) => {
                // Before a byte is read: a peer of another user is closed unanswered.
                match peer_uid(&stream) {
                    Ok(uid) if uid == me => {}
                    _ => continue,
                }
                // A socket accepted from a non-blocking listener inherits that flag on macOS and the BSDs.
                if stream.set_nonblocking(false).is_err() {
                    continue;
                }
                if stop.load(Ordering::Acquire) || !serve(stream) {
                    return;
                }
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
            Err(_) => std::thread::sleep(ACCEPT_BACKOFF),
        }
    }
}

/// A connection to a process's `rv-` socket.
pub struct RvClient {
    stream: UnixStream,
    /// Bytes read past the last line handed out.
    buf: Vec<u8>,
    /// How much of `buf` is known to hold no newline, so a long line is searched once, not once per read.
    scanned: usize,
}

impl RvClient {
    /// Connects to the socket at `path`, waiting at most two seconds for the server to accept, and checks that it
    /// is served by this user before anything is written.
    pub fn connect(path: &Path) -> io::Result<RvClient> {
        let stream = crate::instance_dir::connect_by(path, Instant::now() + CONNECT_WITHIN)?;
        if crate::panel_control::peer_uid(&stream)? != euid() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("{} is served by another user", path.display()),
            ));
        }
        stream.set_write_timeout(Some(RV_LINE_WITHIN))?;
        Ok(RvClient {
            stream,
            buf: Vec::new(),
            scanned: 0,
        })
    }

    /// Writes `frame` as one line.
    pub fn send(&mut self, frame: &RvInbound) -> io::Result<()> {
        let mut line = serde_json::to_vec(frame).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        if line.len() > RV_MAX_LINE {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "line too long"));
        }
        line.push(b'\n');
        (&self.stream).write_all(&line)
    }

    /// The next line the server sent, without its newline, or `None` when none came within `timeout`. A server that
    /// closed is `UnexpectedEof`; a line over [`RV_MAX_OUTBOUND_LINE`] or not UTF-8 is `InvalidData`.
    pub fn recv(&mut self, timeout: Duration) -> io::Result<Option<String>> {
        let deadline = Instant::now() + timeout;
        let mut chunk = vec![0u8; 64 * 1024];
        loop {
            if let Some(at) = self.buf[self.scanned..].iter().position(|&b| b == b'\n') {
                let end = self.scanned + at;
                self.scanned = 0;
                let mut line: Vec<u8> = self.buf.drain(..=end).collect();
                line.pop();
                return String::from_utf8(line)
                    .map(Some)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e));
            }
            self.scanned = self.buf.len();
            if self.buf.len() > RV_MAX_OUTBOUND_LINE {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "line too long"));
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(None);
            }
            // A zero timeout is an error to set, and means "no timeout" to some platforms.
            self.stream.set_read_timeout(Some(left.max(Duration::from_millis(1))))?;
            match (&self.stream).read(&mut chunk) {
                Ok(0) => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "the server closed")),
                Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
                Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {}
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    const INSTANCE: &str = "a3f09c1d2e4b5f60";

    #[test]
    fn remote_rv_act_debug_hides_the_token() {
        let act = RvInbound::Act {
            v: 1,
            device: "d1".into(),
            token: "s3cret-token".into(),
            key: "k".into(),
            cmd: serde_json::json!({}),
        };
        let shown = format!("{act:?}");
        assert!(!shown.contains("s3cret-token"), "{shown}");
        assert!(shown.contains("d1"), "{shown}");
    }

    #[test]
    fn rv_path_refuses_an_over_long_control_dir() {
        let short = Path::new("/run/user/1000/eitri-1000");
        let path = rv_socket_path(short, INSTANCE).expect("a short directory is under the limit");
        assert_eq!(path.parent(), Some(short));
        let name = path.file_name().and_then(|n| n.to_str()).unwrap();
        assert!(name.starts_with("rv-") && name.contains(INSTANCE), "{name}");
        let long = PathBuf::from(format!("/{}", "d".repeat(90)));
        let err = rv_socket_path(&long, INSTANCE).expect_err("the socket path would pass the platform limit");
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn rv_path_refuses_what_is_not_an_instance_id() {
        let dir = Path::new("/run/user/1000/eitri-1000");
        for bad in [
            "",
            "../../etc/passwd",
            "A3F09C1D2E4B5F60",
            "a3f09c1d2e4b5f6",
            "a3f09c1d2e4b5f600",
        ] {
            assert!(rv_socket_path(dir, bad).is_err(), "{bad:?}");
        }
    }

    /// A private directory standing in for the control directory, removed on drop.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Scratch {
            use std::os::unix::fs::DirBuilderExt;
            use std::sync::atomic::AtomicU32;
            static SERIAL: AtomicU32 = AtomicU32::new(0);
            let base = std::env::var_os("XDG_RUNTIME_DIR")
                .map(PathBuf::from)
                .filter(|p| p.is_absolute())
                .unwrap_or_else(std::env::temp_dir);
            let path = base.join(format!(
                "rr-{}-{}",
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

    #[test]
    fn remote_rv_listener_hands_on_only_peers_of_this_user() {
        let scratch = Scratch::new();
        let (tx, rx) = std::sync::mpsc::channel();
        let listener = bind_with(
            &scratch.0,
            INSTANCE,
            |_| Ok(u32::MAX),
            move |stream| tx.send(stream).is_ok(),
        )
        .expect("the listener binds");
        let path = rv_socket_path(&scratch.0, INSTANCE).unwrap();
        let mode = std::fs::symlink_metadata(&path).unwrap().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let mut peer = UnixStream::connect(&path).unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
        let mut byte = [0u8; 1];
        assert_eq!(
            peer.read(&mut byte).unwrap(),
            0,
            "another user's peer is closed unanswered"
        );
        assert!(rx.try_recv().is_err(), "and never handed on");
        drop(listener);
        assert!(
            std::fs::symlink_metadata(&path).is_err(),
            "dropping it removes the socket"
        );

        let (tx, rx) = std::sync::mpsc::channel();
        let _listener = RvListener::bind(&scratch.0, INSTANCE, move |stream| tx.send(stream).is_ok()).unwrap();
        let _peer = RvClient::connect(&path).expect("this user's peer connects");
        assert!(rx.recv_timeout(Duration::from_secs(20)).is_ok(), "and is handed on");
    }

    #[test]
    fn remote_rv_lines_parse_strictly() {
        let sub = br#"{"v":1,"kind":"subscribe","key":"k-0123456789abcdef"}"#;
        assert_eq!(
            parse_rv_line(sub),
            Ok(RvInbound::Subscribe {
                v: 1,
                key: "k-0123456789abcdef".into()
            })
        );
        assert_eq!(
            parse_rv_line(br#"{"v":2,"kind":"subscribe","key":"k"}"#),
            Err(RvLineError::Version(2))
        );
        for bad in [
            &br#"{"kind":"subscribe","key":"k"}"#[..],
            br#"{"v":1,"kind":"subscribe","key":"k","extra":0}"#,
            br#"{"v":1,"kind":"answer","key":"k"}"#,
            br#"{"v":1,"kind":"act","device":"d","key":"k","cmd":{}}"#,
            br#"not json"#,
            br#"[1]"#,
        ] {
            assert_eq!(
                parse_rv_line(bad),
                Err(RvLineError::Malformed),
                "{}",
                String::from_utf8_lossy(bad)
            );
        }
        let act = br#"{"v":1,"kind":"act","device":"d1","token":"t","key":"k","cmd":{"type":"interrupt"}}"#;
        assert!(matches!(parse_rv_line(act), Ok(RvInbound::Act { .. })));
    }

    #[test]
    fn remote_rv_frame_lines_carry_the_version_and_the_frame_verbatim() {
        let key = RowKey::parse("k-0123456789abcdef").unwrap();
        let snapshot = ObserverFrame::Snapshot {
            key: key.clone(),
            through: 4,
            json: Arc::from(r#"{"kind":"snapshot","tab":1}"#),
        };
        let line: serde_json::Value = serde_json::from_str(&frame_line(&snapshot)).unwrap();
        assert_eq!(line["v"], 1);
        assert_eq!(line["kind"], "snapshot");
        assert_eq!(line["key"], "k-0123456789abcdef");
        assert_eq!(line["through"], 4);
        assert_eq!(line["frame"]["kind"], "snapshot");
        let events = ObserverFrame::Events {
            key: key.clone(),
            from: 4,
            through: 6,
            json: Arc::from(r#"{"kind":"events"}"#),
        };
        let line: serde_json::Value = serde_json::from_str(&frame_line(&events)).unwrap();
        assert_eq!(
            (line["kind"].as_str(), line["from"].as_u64(), line["through"].as_u64()),
            (Some("events"), Some(4), Some(6))
        );
        let gone = ObserverFrame::Gone {
            key,
            why: GoneWhy::Moved,
        };
        let line: serde_json::Value = serde_json::from_str(&frame_line(&gone)).unwrap();
        assert_eq!(
            (line["kind"].as_str(), line["why"].as_str()),
            (Some("gone"), Some("moved"))
        );

        let result: serde_json::Value = serde_json::from_str(&result_line(Some("c1"), Err("watch only"))).unwrap();
        assert_eq!(
            result,
            serde_json::json!({"v":1,"kind":"result","command_id":"c1","ok":false,"why":"watch only"})
        );
        let result: serde_json::Value = serde_json::from_str(&result_line(None, Ok(()))).unwrap();
        assert_eq!(
            result,
            serde_json::json!({"v":1,"kind":"result","command_id":null,"ok":true,"why":null})
        );
    }
}
