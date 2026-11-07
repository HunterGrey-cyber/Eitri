//! `serve`'s control socket: how `eitri remote add-device` and `eitri remote grant` reach a window, whatever `serve`
//! listens on for phones.
//!
//! `remote-ctl.sock` in the runtime directory (through `remote_rv::serve_control_socket_path`), 0600, bound before the
//! sandbox and served to this user's processes only, as the window's own `rv-` socket is. It takes one line per
//! connection, `start_enroll` or `grant_request`, and nothing else: it hands the line to one window's `rv-` socket and
//! passes that window's one answer back. It adds no power: the same line written to the window's socket directly does
//! the same, and the window still asks its desk y/n before anything changes.
//!
//! The window is the one whose rows say it has the focus, else the only window with remote control on; with several
//! and none focused the request is refused, naming why (a terminal outside Eitri holds the focus itself).

use std::collections::BTreeSet;
use std::io::{self, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use eitri_core::remote_pairing::ENROLL_CODE_TTL;
use eitri_core::remote_rv::{
    parse_rv_line, parse_rv_outbound, refused_line, rv_socket_path, serve_control_socket_path, RvClient, RvInbound,
    RvOutbound, RV_LINE_WITHIN, RV_MAX_LINE, RV_PROTOCOL,
};

use super::report::live_windows;

/// Requests carried at once; one more is closed at accept.
pub const MAX_CONTROL_CONNECTIONS: usize = 4;

/// How long a request waits for its window's answer: a grant waits for the desk as long as an enrollment code lives.
pub const ANSWER_WITHIN: Duration = Duration::from_secs(ENROLL_CODE_TTL.as_secs() + 30);

/// How often the connection that waits for a window's answer says it is still there: well inside the `rv-` socket's
/// idle deadline.
const KEEPALIVE: Duration = Duration::from_secs(20);

/// What a request is told when no window has remote control on.
pub const NO_WINDOW: &str = "no Eitri window has remote control on";

/// What a request is told when several windows have remote control on and none has the focus.
pub const NO_FOCUSED_WINDOW: &str = "more than one Eitri window has remote control on and none has the focus: run \
                                     it from the terminal of the window to use, or focus that window first";

/// What a line other than the two requests is told.
pub const NOT_A_REQUEST: &str = "this socket takes add-device and grant requests only";

/// Binds the control socket in `control_dir`, removing one an earlier `serve` of this user left (the lock is held, so
/// no other `serve` owns it). `Err` is the line to print.
pub fn bind_control(control_dir: &Path) -> Result<(UnixListener, PathBuf), String> {
    let path = serve_control_socket_path(control_dir)
        .map_err(|e| format!("no control socket path in {}: {e}", control_dir.display()))?;
    super::listen::remove_stale_socket(&path)?;
    let listener = UnixListener::bind(&path).map_err(|e| format!("could not listen on {}: {e}", path.display()))?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| format!("could not set the mode of {}: {e}", path.display()))?;
    Ok((listener, path))
}

/// Answers requests on `listener` on a thread of its own, each on a thread of its own, for the windows in `control`.
pub fn serve_control(listener: UnixListener, control: PathBuf) -> io::Result<()> {
    std::thread::Builder::new()
        .name("eitri-remote-control".into())
        .spawn(move || accept(listener, &control))?;
    Ok(())
}

fn accept(listener: UnixListener, control: &Path) {
    let open = Arc::new(AtomicUsize::new(0));
    for stream in listener.incoming() {
        let Ok(stream) = stream else {
            std::thread::sleep(Duration::from_millis(50));
            continue;
        };
        if eitri_core::panel_control::peer_uid(&stream).ok() != Some(agent::private_fs::euid()) {
            continue;
        }
        if open.fetch_add(1, Ordering::AcqRel) >= MAX_CONTROL_CONNECTIONS {
            open.fetch_sub(1, Ordering::AcqRel);
            continue;
        }
        let (open, control) = (open.clone(), control.to_owned());
        let spawned = std::thread::Builder::new()
            .name("eitri-remote-request".into())
            .spawn(move || {
                answer(stream, &control);
                open.fetch_sub(1, Ordering::AcqRel);
            });
        if spawned.is_err() {
            // The stream went with the closure: the request is closed unanswered.
            eprintln!("[remote] no thread for a request on the control socket");
        }
    }
}

/// Reads one request from `stream`, carries it to a window and writes back the answer.
fn answer(mut stream: UnixStream, control: &Path) {
    let reply = match read_request(&stream) {
        Ok(request) => carry(control, &request),
        Err(why) => refused_line(None, why),
    };
    let _ = stream.set_write_timeout(Some(RV_LINE_WITHIN));
    let _ = stream.write_all(format!("{reply}\n").as_bytes());
}

/// The request on `stream`: one line within [`RV_LINE_WITHIN`], at most [`RV_MAX_LINE`] bytes, one of the two kinds.
fn read_request(stream: &UnixStream) -> Result<RvInbound, &'static str> {
    read_request_within(stream, RV_LINE_WITHIN)
}

/// [`read_request`], the whole line due within `within` of the first read: each read waits only what is left, so a
/// peer that sends a byte now and then cannot hold the request open.
fn read_request_within(stream: &UnixStream, within: Duration) -> Result<RvInbound, &'static str> {
    let until = Instant::now() + within;
    let mut line = Vec::new();
    let mut chunk = [0u8; 4096];
    while line.last() != Some(&b'\n') {
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err("no request came in time");
        }
        // A zero timeout means none at all.
        stream
            .set_read_timeout(Some(left.max(Duration::from_millis(1))))
            .map_err(|_| "the request could not be read")?;
        let room = (RV_MAX_LINE + 1 - line.len()).min(chunk.len());
        match (&*stream).read(&mut chunk[..room]) {
            Ok(0) => break,
            Ok(n) => {
                // One line per connection: anything after its newline is not read.
                match chunk[..n].iter().position(|&b| b == b'\n') {
                    Some(at) => line.extend_from_slice(&chunk[..=at]),
                    None => line.extend_from_slice(&chunk[..n]),
                }
                if line.len() > RV_MAX_LINE {
                    break;
                }
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return Err("no request came in time"),
        }
    }
    if line.last() != Some(&b'\n') {
        return Err("the request is not one line");
    }
    line.pop();
    match parse_rv_line(&line) {
        Ok(request @ (RvInbound::StartEnroll { .. } | RvInbound::GrantRequest { .. })) => Ok(request),
        Ok(_) => Err(NOT_A_REQUEST),
        Err(eitri_core::remote_rv::RvLineError::Version(_)) => Err("the request speaks another protocol version"),
        Err(eitri_core::remote_rv::RvLineError::Malformed) => Err("the request is not one this serve reads"),
    }
}

/// Hands `request` to the window [`window_for`] picks and waits for its answer; the line to pass back.
fn carry(control: &Path, request: &RvInbound) -> String {
    let instance = match window_for(control) {
        Ok(instance) => instance,
        Err(why) => return refused_line(None, why),
    };
    match ask_window(control, &instance, request) {
        Ok(line) => line,
        Err(e) => refused_line(None, &format!("the window did not answer: {e}")),
    }
}

/// The window a request goes to: the one with the focus, else the only one; `Err` says why there is none.
pub fn window_for(control: &Path) -> Result<String, &'static str> {
    let live = live_windows(control);
    let focused: BTreeSet<String> = eitri_core::agents::scan_once(control, eitri_core::agents::SCHEMA_V)
        .map(|view| {
            view.rows
                .into_iter()
                .filter(|row| row.window.focused)
                .map(|row| row.window.instance)
                .collect()
        })
        .unwrap_or_default();
    pick(&live, &focused)
}

/// [`window_for`] over the live windows and the focused ones.
fn pick(live: &[String], focused: &BTreeSet<String>) -> Result<String, &'static str> {
    let mut with_focus = live.iter().filter(|instance| focused.contains(*instance));
    match (with_focus.next(), with_focus.next()) {
        (Some(one), None) => return Ok(one.clone()),
        // Two at once can only be a moment in which the focus moves.
        (Some(_), Some(_)) => return Err(NO_FOCUSED_WINDOW),
        (None, _) => {}
    }
    match live {
        [] => Err(NO_WINDOW),
        [one] => Ok(one.clone()),
        _ => Err(NO_FOCUSED_WINDOW),
    }
}

/// Sends `request` to `instance`'s `rv-` socket and returns its answer line, saying it is still there while the desk
/// is asked.
fn ask_window(control: &Path, instance: &str, request: &RvInbound) -> io::Result<String> {
    let mut client = RvClient::connect(&rv_socket_path(control, instance)?)?;
    client.send(request)?;
    let until = Instant::now() + ANSWER_WITHIN;
    while Instant::now() < until {
        let Some(line) = client.recv(KEEPALIVE)? else {
            client.send(&RvInbound::Ping { v: RV_PROTOCOL })?;
            continue;
        };
        match parse_rv_outbound(line.as_bytes()) {
            Ok(RvOutbound::Result { .. } | RvOutbound::Refused { .. }) => return Ok(line),
            _ => {}
        }
    }
    Err(io::Error::new(io::ErrorKind::TimedOut, "no answer in time"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn live(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| n.to_string()).collect()
    }

    fn set(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|n| n.to_string()).collect()
    }

    #[test]
    fn a_request_sent_a_byte_at_a_time_ends_at_its_deadline() {
        let (reader, mut writer) = UnixStream::pair().unwrap();
        let within = Duration::from_millis(400);
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let dripping = {
            let stop = stop.clone();
            std::thread::spawn(move || {
                // Each byte well inside the deadline, and never a newline.
                let started = Instant::now();
                while !stop.load(Ordering::SeqCst) && started.elapsed() < Duration::from_secs(20) {
                    if writer.write_all(b"x").is_err() {
                        return;
                    }
                    std::thread::sleep(within / 4);
                }
            })
        };
        let started = Instant::now();
        let read = read_request_within(&reader, within);
        let took = started.elapsed();
        stop.store(true, Ordering::SeqCst);
        dripping.join().unwrap();
        assert!(read.is_err());
        // Ten deadlines: far more than one, and far less than the drip runs.
        assert!(took < within * 10, "a dripped request was read for {took:?}");
    }

    #[test]
    fn a_whole_request_is_read() {
        let (reader, mut writer) = UnixStream::pair().unwrap();
        writer.write_all(b"{\"v\":1,\"kind\":\"start_enroll\"}\n").unwrap();
        assert!(matches!(
            read_request_within(&reader, Duration::from_secs(5)),
            Ok(RvInbound::StartEnroll { .. })
        ));
        writer.write_all(b"{\"v\":1,\"kind\":\"ping\"}").unwrap();
        drop(writer);
        assert_eq!(
            read_request_within(&reader, Duration::from_secs(5)).err(),
            Some("the request is not one line")
        );
    }

    #[test]
    fn the_focused_window_or_the_only_one_is_asked() {
        assert_eq!(pick(&live(&[]), &set(&[])), Err(NO_WINDOW));
        assert_eq!(pick(&live(&["a"]), &set(&[])), Ok("a".into()));
        assert_eq!(pick(&live(&["a", "b"]), &set(&["b"])), Ok("b".into()));
        assert_eq!(pick(&live(&["a", "b"]), &set(&[])), Err(NO_FOCUSED_WINDOW));
        // A focused window whose remote control is off is no target.
        assert_eq!(pick(&live(&["a", "b"]), &set(&["c"])), Err(NO_FOCUSED_WINDOW));
        assert_eq!(pick(&live(&["a"]), &set(&["c"])), Ok("a".into()));
        assert_eq!(pick(&live(&["a", "b"]), &set(&["a", "b"])), Err(NO_FOCUSED_WINDOW));
    }
}
