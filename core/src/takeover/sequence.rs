//! Blocking takeover steps for a worker thread; no step belongs on the GTK thread.
use std::io::{self, Read, Write};
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Mutex};
use std::time::{Duration, Instant};

use rmpv::Value;

use super::glue::GlueHandle;
use super::room::{room_binary, OriginLink, RoomDir, RoomEvent, RoomLink};
use crate::companion::{InstallError, InstallRefusal, TEARDOWN_LUA};
use crate::nvim_rpc::{NvimLink, RpcError};

const CHECK_WITHIN: Duration = Duration::from_millis(500);
const ANSWER_WITHIN: Duration = Duration::from_secs(2);
const TEARDOWN_WITHIN: Duration = Duration::from_millis(300);
const MOVE_LUA: &str = "local opts = ...; vim.cmd.connect({args = {opts.addr}, magic = {file = false}})";
const FACTS_LUA: &str =
    "return {addr=vim.v.servername, progpath=vim.v.progpath, version=vim.version(), cwd=vim.fn.getcwd(-1, -1)}";
const TAKEN_LUA: &str = "local c = rawget(_G, '__eitri_companion'); return type(c) == 'table' and c.mode == 'takeover'";

pub struct Checks {
    pub addr: String,
    pub takeover_on: bool,
    pub display_ok: bool,
}

pub struct Prepared {
    pub origin: OriginLink,
    pub room: RoomLink,
    pub dir: RoomDir,
    pub glue: GlueHandle,
    pub cwd: PathBuf,
    events: Mutex<Option<mpsc::Receiver<RoomEvent>>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Moved {
    Landed,
    NotLanded,
}

impl Prepared {
    /// Transfer the room's remaining notifications to the window after the move check.
    pub fn events(&self) -> mpsc::Receiver<RoomEvent> {
        self.events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            .unwrap_or_else(|| mpsc::channel().1)
    }
}

impl Drop for Prepared {
    fn drop(&mut self) {
        self.origin.link.close();
        // The room may still be waiting for a queued move. Its directory must outlive that wait.
        let socket = self.dir.socket().to_owned();
        let Some(path) = socket.parent().map(Path::to_owned) else {
            return;
        };
        let _ = std::thread::Builder::new()
            .name("takeover-room-cleanup".into())
            .spawn(move || {
                let record =
                    agent::private_fs::read_private_nofollow_capped(&path.join("room.pid"), 128).unwrap_or_default();
                let mut words = record.split_whitespace();
                let pid = words.next().and_then(|s| s.parse::<u32>().ok());
                let start = words.next().and_then(|s| s.parse::<u64>().ok());
                if let Some((pid, start)) = pid.zip(start).filter(|(_, start)| *start != 0) {
                    while crate::split::proc_start_time(pid) == Some(start) {
                        std::thread::sleep(Duration::from_millis(250));
                    }
                    let _ = std::fs::remove_dir_all(path);
                }
            });
    }
}

fn field<'a>(value: &'a Value, name: &str) -> Option<&'a Value> {
    value
        .as_map()?
        .iter()
        .find(|(key, _)| key.as_str() == Some(name))
        .map(|(_, value)| value)
}

fn socket_facts(absolute: bool, socket: bool, uid: u32, expected: u32) -> Result<(), Refusal> {
    if !absolute || !socket {
        return Err(Refusal::NotASocket);
    }
    if uid != expected {
        return Err(Refusal::OtherUid);
    }
    Ok(())
}

fn mode_ready(mode: &Value) -> Result<(), Refusal> {
    let waiting = field(mode, "mode")
        .and_then(Value::as_str)
        .is_none_or(|mode| mode.starts_with('r') || mode == "!");
    if field(mode, "blocking").and_then(Value::as_bool) != Some(false) || waiting {
        return Err(Refusal::Blocking);
    }
    Ok(())
}

/// One deadline covers a whole reply, including a peer that sends it a byte at a time.
struct ModeReader {
    stream: UnixStream,
    deadline: Instant,
    remaining: usize,
}
impl Read for ModeReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() || self.remaining == 0 {
            return Err(io::ErrorKind::TimedOut.into());
        }
        self.stream.set_read_timeout(Some(remaining))?;
        let len = buf.len().min(self.remaining);
        let got = self.stream.read(&mut buf[..len])?;
        self.remaining -= got;
        Ok(got)
    }
}

/// NvimLink starts with API-info, which queues behind a pending key. Probe mode before that handshake.
fn first_mode(addr: &Path) -> Result<(), Refusal> {
    let deadline = Instant::now() + CHECK_WITHIN;
    let mut stream = crate::instance_dir::connect_by(addr, deadline).map_err(|_| Refusal::Blocking)?;
    if crate::panel_control::peer_uid(&stream).map_err(|_| Refusal::OtherUid)? != crate::panel_control::euid() {
        return Err(Refusal::OtherUid);
    }
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(Refusal::Blocking);
    }
    stream
        .set_write_timeout(Some(remaining))
        .map_err(|_| Refusal::Blocking)?;
    let request = Value::Array(vec![0.into(), 0.into(), "nvim_get_mode".into(), Value::Array(vec![])]);
    let mut bytes = Vec::new();
    rmpv::encode::write_value(&mut bytes, &request).map_err(|_| Refusal::Blocking)?;
    stream.write_all(&bytes).map_err(|_| Refusal::Blocking)?;
    let mut reader = ModeReader {
        stream,
        deadline,
        remaining: 64 * 1024,
    };
    loop {
        let reply = rmpv::decode::read_value(&mut reader).map_err(|_| Refusal::Blocking)?;
        let parts = reply.as_array().ok_or(Refusal::Blocking)?;
        if parts.len() == 4 && parts[0].as_u64() == Some(1) && parts[1].as_u64() == Some(0) {
            if !parts[2].is_nil() {
                return Err(Refusal::Blocking);
            }
            return mode_ready(&parts[3]);
        }
        // Notifications sent to every RPC client can precede this connection's answer.
        if parts.len() != 3 || parts[0].as_u64() != Some(2) {
            return Err(Refusal::Blocking);
        }
    }
}

fn request(link: &NvimLink, method: &str, args: Vec<Value>, bound: Duration) -> Result<Value, Refusal> {
    link.call(method, args)
        .wait(bound)
        .and_then(Result::ok)
        .ok_or(Refusal::Blocking)
}

fn is_stdio_tui(info: &Value) -> bool {
    field(info, "stream").and_then(Value::as_str) == Some("stdio")
        && field(info, "client")
            .and_then(|client| field(client, "name"))
            .and_then(Value::as_str)
            == Some("nvim-tui")
}

fn check_tui(link: &NvimLink) -> Result<(), Refusal> {
    let uis = request(link, "nvim_list_uis", vec![], CHECK_WITHIN)?;
    let uis = uis.as_array().filter(|uis| uis.len() == 1).ok_or(Refusal::NotOneTui)?;
    let chan = field(&uis[0], "chan")
        .and_then(Value::as_u64)
        .ok_or(Refusal::NotOneTui)?;
    let info = request(link, "nvim_get_chan_info", vec![chan.into()], CHECK_WITHIN)?;
    if !is_stdio_tui(&info) {
        return Err(Refusal::NotOneTui);
    }
    Ok(())
}

fn version(value: &Value) -> Option<(u64, u64, u64)> {
    Some((
        field(value, "major")?.as_u64()?,
        field(value, "minor")?.as_u64()?,
        field(value, "patch")?.as_u64()?,
    ))
}

/// Refusals do no work in the origin except read its state. Run on a worker thread.
pub fn check(c: &Checks) -> Result<OriginLink, StepError> {
    check_with_owner(c, crate::panel_control::euid())
}

/// Exercise the pathname ownership refusal without changing a test process's credentials.
/// Peer authentication still uses the real effective uid.
#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub fn check_with_uid_for_tests(c: &Checks, uid: u32) -> Result<OriginLink, StepError> {
    check_with_owner(c, uid)
}

fn check_with_owner(c: &Checks, uid: u32) -> Result<OriginLink, StepError> {
    let checked = (|| {
        let addr = Path::new(&c.addr);
        let meta = addr.symlink_metadata().map_err(|_| Refusal::NotASocket)?;
        socket_facts(addr.is_absolute(), meta.file_type().is_socket(), meta.uid(), uid)?;
        first_mode(addr)?;
        if !c.takeover_on {
            return Err(Refusal::Off);
        }
        if !c.display_ok {
            return Err(Refusal::NoDisplay);
        }
        let origin = OriginLink::connect(&c.addr, CHECK_WITHIN).map_err(|_| Refusal::Blocking)?;
        mode_ready(&request(&origin.link, "nvim_get_mode", vec![], CHECK_WITHIN)?)?;
        let api = request(&origin.link, "nvim_get_api_info", vec![], CHECK_WITHIN)?;
        let api_version = api
            .as_array()
            .and_then(|a| a.get(1))
            .and_then(|v| field(v, "version"))
            .and_then(version)
            .ok_or(Refusal::Blocking)?;
        if (api_version.0, api_version.1) < (0, 12) {
            return Err(Refusal::TooOld(format!(
                "{}.{}.{}",
                api_version.0, api_version.1, api_version.2
            )));
        }
        check_tui(&origin.link)?;
        let taken = origin
            .link
            .exec_lua(TAKEN_LUA, vec![])
            .wait(CHECK_WITHIN)
            .and_then(Result::ok)
            .and_then(|v| v.as_bool())
            .ok_or(Refusal::Blocking)?;
        if taken {
            return Err(Refusal::AlreadyTaken);
        }
        Ok(origin)
    })();
    checked.map_err(StepError::Refused)
}

/// Start the room before installing glue, and before any UI can move. Run on a worker thread.
pub fn prepare(
    origin: OriginLink,
    runtime: &Path,
    install: &dyn Fn(&OriginLink) -> Result<GlueHandle, InstallError>,
) -> Result<Prepared, StepError> {
    let facts = origin
        .link
        .exec_lua(FACTS_LUA, vec![])
        .wait(CHECK_WITHIN)
        .and_then(Result::ok)
        .ok_or(StepError::Refused(Refusal::Blocking))?;
    let text = |name| {
        field(&facts, name)
            .and_then(Value::as_str)
            .ok_or_else(|| StepError::Install(format!("nvim returned no {name}")))
    };
    let addr = text("addr")?;
    let binary = room_binary(text("progpath")?)
        .ok_or_else(|| StepError::RoomStart("nvim's executable is unavailable".into()))?;
    let cwd = PathBuf::from(text("cwd")?);
    let (major, minor, _) = field(&facts, "version")
        .and_then(version)
        .ok_or_else(|| StepError::RoomStart("nvim returned no version".into()))?;
    let dir = RoomDir::create(runtime).map_err(|e| StepError::RoomStart(e.to_string()))?;
    let (room, room_pid) = match RoomLink::start(&binary, &dir, (major, minor)) {
        Ok(room) => room,
        Err(error) => {
            let _ = dir.remove();
            return Err(error);
        }
    };
    let room_start = crate::split::proc_start_time(room_pid);
    if let Err(error) = room.install(addr, room.chan()) {
        // A failed install may have no liveness timer. Only this newly started child may be stopped.
        if room_start.is_some() && crate::split::proc_start_time(room_pid) == room_start {
            // SAFETY: the pid and its unchanged start time identify the child just started above.
            unsafe {
                libc::kill(room_pid as libc::pid_t, libc::SIGTERM);
            }
        }
        drop(room);
        origin.link.close();
        let _ = dir.remove();
        return Err(error);
    }
    let started = Instant::now();
    // Closing the link at the bound also releases an install waiting on a queued RPC answer.
    // A callback must finish when its RPC closes; arbitrary synchronous work cannot be interrupted.
    let (done, watchdog) = mpsc::channel();
    let timed_link = origin.link.clone();
    let timer = std::thread::spawn(move || {
        if watchdog.recv_timeout(ANSWER_WITHIN).is_err() {
            timed_link.close();
        }
    });
    let result = install(&origin);
    let _ = done.send(());
    let _ = timer.join();
    let result = if started.elapsed() >= ANSWER_WITHIN {
        Err(InstallError::Malformed(
            "the takeover install did not answer within 2 s".into(),
        ))
    } else {
        result
    };
    let glue = match result {
        Ok(glue) => glue,
        Err(error) => {
            let _ = room.quit();
            origin.link.close();
            drop(room);
            let _ = dir.remove();
            return Err(match error {
                InstallError::Refused(InstallRefusal::Takeover) => StepError::Refused(Refusal::AlreadyTaken),
                error => StepError::Install(error.to_string()),
            });
        }
    };
    let events = Mutex::new(Some(room.events()));
    Ok(Prepared {
        origin,
        room,
        dir,
        glue,
        cwd,
        events,
    })
}

fn stop_before_move(p: &Prepared) {
    let _ = p.room.quit();
    p.origin.link.close();
}

/// Move only after a fresh mode/UI check. An unanswered move leaves its room alive for a late TUI.
pub fn move_tui(p: &Prepared) -> Result<Moved, StepError> {
    let recheck = request(&p.origin.link, "nvim_get_mode", vec![], CHECK_WITHIN)
        .and_then(|mode| mode_ready(&mode))
        .and_then(|()| check_tui(&p.origin.link));
    if let Err(refusal) = recheck {
        stop_before_move(p);
        return Err(StepError::Refused(refusal));
    }
    let Some(address) = p.dir.socket().to_str() else {
        stop_before_move(p);
        return Err(StepError::MoveError("the room address is not UTF-8".into()));
    };
    if let Err(error) = p.room.moving() {
        stop_before_move(p);
        return Err(error);
    }
    match p
        .origin
        .link
        .exec_lua(MOVE_LUA, vec![Value::Map(vec![("addr".into(), address.into())])])
        .wait(ANSWER_WITHIN)
    {
        Some(Ok(_)) => (),
        Some(Err(RpcError::Closed)) | None => {
            // Never quit this room: the command is queued and may still send the TUI here.
            let _ = p.room.pending();
            p.origin.link.close();
            return Err(StepError::MoveUnanswered);
        }
        Some(Err(error)) => {
            stop_before_move(p);
            return Err(StepError::MoveError(error.to_string()));
        }
    }
    let deadline = Instant::now() + ANSWER_WITHIN;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            let _ = p.room.let_go();
            p.origin.link.close();
            return Err(StepError::TuiStayed);
        }
        let uis = p
            .origin
            .link
            .call("nvim_list_uis", vec![])
            .wait(remaining.min(CHECK_WITHIN));
        if matches!(uis, Some(Ok(Value::Array(ref uis))) if uis.is_empty()) {
            break;
        }
        std::thread::sleep(Duration::from_millis(10).min(remaining));
    }
    let deadline = Instant::now() + ANSWER_WITHIN;
    let events = p.events.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(events) = events.as_ref() {
        while let Ok(event) = events.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            if event == RoomEvent::Ui(true) {
                return Ok(Moved::Landed);
            }
        }
    }
    Ok(Moved::NotLanded)
}

/// A competing UI causes teardown and detachment of only the channel supplied by the editor.
/// A room that no longer answers returns `WayBackGone` and keeps the attach.
pub fn after_attach(p: &Prepared, editor_chan: u64) -> Result<(), StepError> {
    let uis = p
        .origin
        .link
        .call("nvim_list_uis", vec![])
        .wait(CHECK_WITHIN)
        .and_then(Result::ok);
    let ours = uis
        .as_ref()
        .and_then(Value::as_array)
        .is_some_and(|uis| uis.len() == 1 && field(&uis[0], "chan").and_then(Value::as_u64) == Some(editor_chan));
    if !ours {
        let _ = p
            .origin
            .link
            .exec_lua(TEARDOWN_LUA, vec![p.glue.link_chan.into()])
            .wait(TEARDOWN_WITHIN);
        let _ = p
            .origin
            .link
            .exec_lua("vim.fn.chanclose(...)", vec![editor_chan.into()])
            .wait(TEARDOWN_WITHIN);
        let _ = p.room.let_go();
        p.origin.link.close();
        return Err(StepError::OtherUi);
    }
    // Only the UI set decides the detach. A room that cannot be reached leaves Eitri's editor in place.
    p.room.held().map_err(|_| StepError::WayBackGone)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    NotASocket,
    OtherUid,
    Off,
    TooOld(String),
    NotOneTui,
    AlreadyTaken,
    NoDisplay,
    Blocking,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepError {
    Refused(Refusal),
    RoomStart(String),
    Install(String),
    MoveError(String),
    MoveUnanswered,
    TuiStayed,
    OtherUi,
    /// The room is gone after the move landed: the attach stands, and the terminal cannot be handed back.
    WayBackGone,
}

impl StepError {
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Refused(_) => 64,
            Self::RoomStart(_)
            | Self::Install(_)
            | Self::MoveError(_)
            | Self::MoveUnanswered
            | Self::TuiStayed
            | Self::OtherUi
            | Self::WayBackGone => 1,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Refusal, StepError};

    #[test]
    fn takeover_socket_checks_refuse_foreign_owners_before_requests() {
        assert_eq!(super::socket_facts(false, true, 1000, 1000), Err(Refusal::NotASocket));
        assert_eq!(super::socket_facts(true, false, 1000, 1000), Err(Refusal::NotASocket));
        assert_eq!(super::socket_facts(true, true, 1001, 1000), Err(Refusal::OtherUid));
        assert_eq!(super::socket_facts(true, true, 1000, 1000), Ok(()));
    }

    #[test]
    fn takeover_accepts_only_the_original_stdio_tui() {
        let value = |stream: &str, name: &str| {
            rmpv::Value::Map(vec![
                ("stream".into(), stream.into()),
                ("client".into(), rmpv::Value::Map(vec![("name".into(), name.into())])),
            ])
        };
        assert!(super::is_stdio_tui(&value("stdio", "nvim-tui")));
        assert!(!super::is_stdio_tui(&value("socket", "nvim-tui")));
        assert!(!super::is_stdio_tui(&value("stdio", "another-ui")));
        assert!(!super::is_stdio_tui(&rmpv::Value::Nil));
    }

    #[test]
    fn takeover_a_missing_or_malformed_mode_answer_is_busy() {
        use rmpv::Value;
        assert_eq!(super::mode_ready(&Value::Nil), Err(Refusal::Blocking));
        for blocking in [true, false] {
            let mode = Value::Map(vec![("blocking".into(), blocking.into()), ("mode".into(), "n".into())]);
            assert_eq!(
                super::mode_ready(&mode),
                if blocking { Err(Refusal::Blocking) } else { Ok(()) }
            );
        }
        let dialog = Value::Map(vec![("blocking".into(), false.into()), ("mode".into(), "r?".into())]);
        assert_eq!(super::mode_ready(&dialog), Err(Refusal::Blocking));
    }

    #[test]
    fn exit_codes_are_64_for_a_refusal_and_1_otherwise() {
        for refusal in [
            Refusal::NotASocket,
            Refusal::OtherUid,
            Refusal::Off,
            Refusal::TooOld("0.11.2".into()),
            Refusal::NotOneTui,
            Refusal::AlreadyTaken,
            Refusal::NoDisplay,
            Refusal::Blocking,
        ] {
            assert_eq!(StepError::Refused(refusal).exit_code(), 64);
        }

        for error in [
            StepError::RoomStart("not yet".into()),
            StepError::Install("not yet".into()),
            StepError::MoveError("connection closed".into()),
            StepError::MoveUnanswered,
            StepError::TuiStayed,
            StepError::OtherUi,
            StepError::WayBackGone,
        ] {
            assert_eq!(error.exit_code(), 1);
        }
    }
}
