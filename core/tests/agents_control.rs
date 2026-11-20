//! The per-process control socket that carries `focus` and `new` to the window owning a row.
//!
//! Every test binds in its own private directory, under `XDG_RUNTIME_DIR` when the run sets one, so
//! nothing here meets a real window's socket.

use eitri_core::agents::control::{self, Reply, Request, Server, DEADLINE, MAX_LINE};
use eitri_core::agents::Intent;
use eitri_core::source_scan;
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// How long any one step may take on a loaded machine; a test that passes never waits it out.
const PATIENCE: Duration = Duration::from_secs(20);

const INSTANCE: &str = "a3f09c1d2e4b5f60";

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
            "ac-{}-{}",
            std::process::id(),
            SERIAL.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
        Scratch(path)
    }

    fn dir(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn focus(tab: Option<u64>) -> Request {
    Request::Focus {
        v: 1,
        tab,
        card_first: true,
        intent: Intent::Show,
        token: Some("tok-1".into()),
    }
}

/// A server whose handler counts its calls and answers `ok`.
fn counting_server(dir: &Path) -> (Server, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&calls);
    let server = Server::bind(dir, INSTANCE, move |_| {
        seen.fetch_add(1, Ordering::SeqCst);
        Reply::Ok
    })
    .unwrap();
    (server, calls)
}

/// Sends `bytes` as they are and returns the one line the server answers (empty when it closed
/// without one).
fn raw_exchange(dir: &Path, bytes: &[u8]) -> String {
    let stream = UnixStream::connect(control::socket_path(dir, INSTANCE).unwrap()).unwrap();
    stream.set_read_timeout(Some(PATIENCE)).unwrap();
    stream.set_write_timeout(Some(PATIENCE)).unwrap();
    // A server that refuses before reading may close before the whole write lands; its line is
    // still there to read.
    let _ = (&stream).write_all(bytes);
    read_reply(&stream)
}

fn read_reply(stream: &UnixStream) -> String {
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).unwrap();
    line.trim_end_matches('\n').to_owned()
}

/// The byte offsets of every `.sock"` string literal in `text` that is not an argument (at any
/// depth) of an `in_dir(` call within its own statement. Reads code as text with comments and
/// literals blanked for the structure, so where a line ends does not matter.
fn sock_literals_outside_in_dir(text: &str) -> Vec<usize> {
    let code = source_scan::code_only(text);
    assert_eq!(
        code.len(),
        text.len(),
        "the scan needs ASCII source to line the two texts up"
    );
    let bytes = code.as_bytes();
    let mut offenders = Vec::new();
    for (at, _) in text.match_indices(".sock\"") {
        let mut depth = 0usize;
        let mut routed = false;
        let mut i = at;
        while i > 0 {
            i -= 1;
            match bytes[i] {
                b')' => depth += 1,
                b'(' if depth > 0 => depth -= 1,
                b'(' => {
                    let before = code[..i].trim_end();
                    let word_start = before
                        .rfind(|c: char| !(c.is_alphanumeric() || c == '_'))
                        .map_or(0, |p| p + 1);
                    if &before[word_start..] == "in_dir" {
                        routed = true;
                        break;
                    }
                }
                b';' | b'{' | b'}' if depth == 0 => break,
                _ => {}
            }
        }
        if !routed {
            offenders.push(at);
        }
    }
    offenders
}

#[test]
fn the_socket_is_built_through_in_dir() {
    let scratch = Scratch::new();
    assert_eq!(
        control::socket_path(scratch.dir(), INSTANCE).unwrap(),
        agent::socket_path::in_dir(scratch.dir(), &format!("ag-{INSTANCE}.sock")).unwrap()
    );

    // The name is spelled where both the agents and the remote socket are named.
    let source = source_scan::read_required("core/src/control_listener.rs");
    assert!(
        source.text.contains(".sock\""),
        "the socket's name is no longer spelled in control_listener.rs; point this scan at where it moved"
    );
    assert_eq!(sock_literals_outside_in_dir(&source.text), Vec::<usize>::new());

    // The scan itself: a name joined by hand is caught, a wrapped `in_dir` call is not.
    let by_hand = "fn p(dir: &Path, i: &str) -> PathBuf {\n    dir.join(format!(\"ag-{i}.sock\"))\n}\n";
    assert_eq!(sock_literals_outside_in_dir(by_hand).len(), 1);
    let wrapped = "fn p(dir: &Path, i: &str) -> io::Result<PathBuf> {\n    agent::socket_path::in_dir(\n        \
                   dir,\n        &format!(\"ag-{i}.sock\"),\n    )\n}\n";
    assert_eq!(sock_literals_outside_in_dir(wrapped), Vec::<usize>::new());
}

#[test]
fn an_instance_that_is_not_sixteen_hex_digits_names_no_socket() {
    let scratch = Scratch::new();
    for bad in [
        "",
        "a3f09c1d2e4b5f6",
        "a3f09c1d2e4b5f600",
        "../../a3f09c1d2e",
        "A3F09C1D2E4B5F60",
        "a3f09c1d2e4b5f6g",
    ] {
        let err = control::socket_path(scratch.dir(), bad).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{bad:?}");
        assert!(Server::bind(scratch.dir(), bad, |_| Reply::Ok).is_err(), "{bad:?}");
        assert!(control::send(scratch.dir(), bad, &focus(None)).is_err(), "{bad:?}");
    }
}

#[test]
fn a_peer_of_another_uid_is_refused_before_reading() {
    let scratch = Scratch::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&calls);
    let stranger = |_: &UnixStream| -> io::Result<u32> {
        // SAFETY: `geteuid` takes no arguments and cannot fail.
        Ok(unsafe { libc::geteuid() }.wrapping_add(1))
    };
    let server = control::test_hooks::bind_reading_peer_uid_with(scratch.dir(), INSTANCE, stranger, move |_| {
        seen.fetch_add(1, Ordering::SeqCst);
        Reply::Ok
    })
    .unwrap();

    // Nothing is written: a server that read before checking would wait for a line here and answer
    // that it timed out; the refusal comes before any read.
    let stream = UnixStream::connect(control::socket_path(scratch.dir(), INSTANCE).unwrap()).unwrap();
    stream.set_read_timeout(Some(PATIENCE)).unwrap();
    assert_eq!(read_reply(&stream), "refused: not your process");

    // A full request from such a peer never reaches the handler either.
    let line = format!("{}\n", serde_json::to_string(&focus(Some(7))).unwrap());
    assert_eq!(
        raw_exchange(scratch.dir(), line.as_bytes()),
        "refused: not your process"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    server.stop();
}

#[test]
fn an_unknown_field_is_refused() {
    let scratch = Scratch::new();
    let (server, calls) = counting_server(scratch.dir());
    let extra = r#"{"v":1,"verb":"focus","tab":7,"card_first":true,"intent":"show","token":null,"answer":"a"}"#;
    assert_eq!(
        raw_exchange(scratch.dir(), format!("{extra}\n").as_bytes()),
        "refused: malformed request"
    );
    let extra_new = r#"{"v":1,"verb":"new","token":null,"prompt":"rm -rf"}"#;
    assert_eq!(
        raw_exchange(scratch.dir(), format!("{extra_new}\n").as_bytes()),
        "refused: malformed request"
    );
    let unknown_verb = r#"{"v":1,"verb":"interrupt","tab":7}"#;
    assert_eq!(
        raw_exchange(scratch.dir(), format!("{unknown_verb}\n").as_bytes()),
        "refused: malformed request"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    server.stop();
}

#[test]
fn an_unsupported_version_is_refused() {
    let scratch = Scratch::new();
    let (server, calls) = counting_server(scratch.dir());
    let v2 = r#"{"v":2,"verb":"new","token":null}"#;
    assert_eq!(
        raw_exchange(scratch.dir(), format!("{v2}\n").as_bytes()),
        "refused: unsupported protocol version 2"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    server.stop();
}

#[test]
fn a_line_over_4096_bytes_is_refused() {
    assert_eq!(MAX_LINE, 4096);
    let scratch = Scratch::new();
    let (server, calls) = counting_server(scratch.dir());

    // JSON allows blanks before the closing brace, so a valid request can be padded to any length.
    let pad_to = |len: usize| {
        let body = r#"{"v":1,"verb":"new","token":null"#;
        format!("{body}{}}}", " ".repeat(len - body.len() - 1))
    };
    let at_limit = pad_to(MAX_LINE);
    assert_eq!(at_limit.len(), MAX_LINE);
    assert_eq!(raw_exchange(scratch.dir(), format!("{at_limit}\n").as_bytes()), "ok");
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    let over = pad_to(MAX_LINE + 1);
    assert_eq!(
        raw_exchange(scratch.dir(), format!("{over}\n").as_bytes()),
        "refused: request too long"
    );
    // No newline at all: the server stops reading at the limit rather than waiting for one.
    assert_eq!(
        raw_exchange(scratch.dir(), &vec![b' '; 3 * MAX_LINE]),
        "refused: request too long"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    server.stop();
}

#[test]
fn a_peer_silent_for_a_second_is_dropped() {
    assert_eq!(DEADLINE, Duration::from_secs(1));
    let scratch = Scratch::new();
    let (server, calls) = counting_server(scratch.dir());

    let silent = UnixStream::connect(control::socket_path(scratch.dir(), INSTANCE).unwrap()).unwrap();
    silent.set_read_timeout(Some(PATIENCE)).unwrap();
    assert_eq!(read_reply(&silent), "refused: request timed out");

    // Half a line, then nothing: the same.
    let half = UnixStream::connect(control::socket_path(scratch.dir(), INSTANCE).unwrap()).unwrap();
    half.set_read_timeout(Some(PATIENCE)).unwrap();
    (&half).write_all(br#"{"v":1,"verb":"#).unwrap();
    assert_eq!(read_reply(&half), "refused: request timed out");
    assert_eq!(calls.load(Ordering::SeqCst), 0);

    // The server goes on serving after dropping them.
    assert_eq!(
        control::send(scratch.dir(), INSTANCE, &focus(Some(1))).unwrap(),
        Reply::Ok
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    server.stop();
}

#[test]
fn focus_and_new_round_trip() {
    let scratch = Scratch::new();
    let (tx, rx) = mpsc::channel();
    let tx = Mutex::new(tx);
    let server = Server::bind(scratch.dir(), INSTANCE, move |req: Request| {
        let reply = match &req {
            Request::Focus { tab: Some(7), .. } => Reply::Ok,
            Request::Focus { .. } => Reply::Refused("gone".into()),
            Request::New { .. } => Reply::Refused("no_window".into()),
        };
        tx.lock().unwrap().send(req).unwrap();
        reply
    })
    .unwrap();

    let shown = focus(Some(7));
    assert_eq!(control::send(scratch.dir(), INSTANCE, &shown).unwrap(), Reply::Ok);
    assert_eq!(rx.recv_timeout(PATIENCE).unwrap(), shown);

    let stop = Request::Focus {
        v: 1,
        tab: Some(9),
        card_first: false,
        intent: Intent::Stop,
        token: None,
    };
    assert_eq!(
        control::send(scratch.dir(), INSTANCE, &stop).unwrap(),
        Reply::Refused("gone".into())
    );
    assert_eq!(rx.recv_timeout(PATIENCE).unwrap(), stop);

    let new = Request::New {
        v: 1,
        token: Some("tok-2".into()),
    };
    assert_eq!(
        control::send(scratch.dir(), INSTANCE, &new).unwrap(),
        Reply::Refused("no_window".into())
    );
    assert_eq!(rx.recv_timeout(PATIENCE).unwrap(), new);

    // The wire as written by hand, one JSON line in and one line out.
    let line = r#"{ "v": 1, "verb": "focus", "tab": 7, "card_first": true, "intent": "show", "token": null }"#;
    assert_eq!(raw_exchange(scratch.dir(), format!("{line}\n").as_bytes()), "ok");
    assert_eq!(
        rx.recv_timeout(PATIENCE).unwrap(),
        Request::Focus {
            v: 1,
            tab: Some(7),
            card_first: true,
            intent: Intent::Show,
            token: None
        }
    );
    let line = r#"{ "v": 1, "verb": "new", "token": "t" }"#;
    assert_eq!(
        raw_exchange(scratch.dir(), format!("{line}\n").as_bytes()),
        "refused: no_window"
    );
    server.stop();
}

#[test]
fn tab_null_parses() {
    let parsed: Request = serde_json::from_str(
        r#"{ "v": 1, "verb": "focus", "tab": null, "card_first": false, "intent": "show", "token": "..." }"#,
    )
    .unwrap();
    assert_eq!(
        parsed,
        Request::Focus {
            v: 1,
            tab: None,
            card_first: false,
            intent: Intent::Show,
            token: Some("...".into())
        }
    );
    // And it is written back as `null`, so the reader on the other side sees the same request.
    let json: serde_json::Value = serde_json::to_value(&parsed).unwrap();
    assert_eq!(json["tab"], serde_json::Value::Null);
    assert_eq!(json["verb"], "focus");

    let stop: Request = serde_json::from_str(
        r#"{ "v": 1, "verb": "focus", "tab": 7, "card_first": false, "intent": "stop", "token": "..." }"#,
    )
    .unwrap();
    assert!(matches!(
        stop,
        Request::Focus {
            tab: Some(7),
            intent: Intent::Stop,
            ..
        }
    ));
    let new: Request = serde_json::from_str(r#"{ "v": 1, "verb": "new", "token": null }"#).unwrap();
    assert_eq!(new, Request::New { v: 1, token: None });
}

#[test]
fn replies_read_back_as_written() {
    assert_eq!(Reply::Ok.line(), "ok");
    assert_eq!(Reply::Refused("gone".into()).line(), "refused: gone");
    // A reason is one line on the wire, whatever it held.
    assert_eq!(Reply::Refused("a\nb\rc".into()).line(), "refused: a b c");
    assert_eq!(Reply::parse("ok"), Some(Reply::Ok));
    assert_eq!(
        Reply::parse("refused: no_gesture"),
        Some(Reply::Refused("no_gesture".into()))
    );
    assert_eq!(Reply::parse("ready"), None);
}

#[test]
fn the_client_refuses_a_server_of_another_uid() {
    let scratch = Scratch::new();
    let (server, calls) = counting_server(scratch.dir());
    let stranger = |_: &UnixStream| -> io::Result<u32> {
        // SAFETY: `geteuid` takes no arguments and cannot fail.
        Ok(unsafe { libc::geteuid() }.wrapping_add(1))
    };
    let err = control::test_hooks::send_reading_peer_uid_with(scratch.dir(), INSTANCE, &focus(Some(7)), stranger)
        .unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    // The same server, asked by a client that reads the real uid, answers.
    assert_eq!(
        control::send(scratch.dir(), INSTANCE, &focus(Some(7))).unwrap(),
        Reply::Ok
    );
    server.stop();
}

#[test]
fn a_server_that_never_answers_times_the_client_out() {
    let scratch = Scratch::new();
    let path = control::socket_path(scratch.dir(), INSTANCE).unwrap();
    let listener = UnixListener::bind(&path).unwrap();
    let (held_tx, held_rx) = mpsc::channel();
    let holder = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        // Keep the connection open, never answer, until the client has given up.
        let _ = held_rx.recv_timeout(PATIENCE);
        drop(stream);
    });
    let err = control::send(scratch.dir(), INSTANCE, &focus(Some(7))).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::TimedOut);
    held_tx.send(()).unwrap();
    holder.join().unwrap();
}

#[test]
fn a_socket_path_over_the_limit_turns_the_layer_off_quietly() {
    let scratch = Scratch::new();
    let long = scratch.dir().join("d".repeat(100));
    let err = control::socket_path(&long, INSTANCE).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    let err = Server::bind(&long, INSTANCE, |_| Reply::Ok)
        .err()
        .expect("a path over the limit cannot bind");
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    assert!(!long.exists(), "a refused bind leaves nothing behind");
    let err = control::send(&long, INSTANCE, &focus(None)).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
}

#[test]
fn the_socket_is_private_and_stop_unlinks_it() {
    let scratch = Scratch::new();
    let (server, _) = counting_server(scratch.dir());
    let path = control::socket_path(scratch.dir(), INSTANCE).unwrap();
    let meta = path.symlink_metadata().unwrap();
    assert!(meta.file_type().is_socket());
    assert_eq!(meta.permissions().mode() & 0o777, 0o600);
    server.stop();
    assert!(path.symlink_metadata().is_err(), "stop leaves its own socket behind");
    assert_eq!(
        control::send(scratch.dir(), INSTANCE, &focus(None)).unwrap_err().kind(),
        io::ErrorKind::NotFound
    );
}

#[test]
fn stop_unlinks_only_its_own_socket() {
    let scratch = Scratch::new();
    let path = control::socket_path(scratch.dir(), INSTANCE).unwrap();

    // Another socket took the name after this server bound it.
    let (server, _) = counting_server(scratch.dir());
    std::fs::remove_file(&path).unwrap();
    let other = UnixListener::bind(&path).unwrap();
    server.stop();
    assert!(path.symlink_metadata().unwrap().file_type().is_socket());
    assert!(
        UnixStream::connect(&path).is_ok(),
        "stop removed a socket it did not bind"
    );
    drop(other);
    std::fs::remove_file(&path).unwrap();

    // A plain file put in its place is left alone as well.
    let (server, _) = counting_server(scratch.dir());
    std::fs::remove_file(&path).unwrap();
    std::fs::write(&path, b"not a socket").unwrap();
    server.stop();
    assert_eq!(std::fs::read(&path).unwrap(), b"not a socket");
}

#[test]
fn a_name_already_taken_is_not_bound_over() {
    let scratch = Scratch::new();
    let path = control::socket_path(scratch.dir(), INSTANCE).unwrap();
    let _other = UnixListener::bind(&path).unwrap();
    assert!(Server::bind(scratch.dir(), INSTANCE, |_| Reply::Ok).is_err());
    assert!(
        UnixStream::connect(&path).is_ok(),
        "a failed bind removed the socket it found"
    );
}

#[test]
fn stop_does_not_wait_for_a_request_in_progress() {
    let scratch = Scratch::new();
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let entered_tx = Mutex::new(entered_tx);
    let release_rx = Mutex::new(release_rx);
    // The handler waits for the thread that will call `stop`, as one waiting for the GTK side does.
    let server = Server::bind(scratch.dir(), INSTANCE, move |_| {
        entered_tx.lock().unwrap().send(()).unwrap();
        let _ = release_rx.lock().unwrap().recv_timeout(PATIENCE);
        Reply::Ok
    })
    .unwrap();
    let dir = scratch.dir().to_owned();
    let client = std::thread::spawn(move || control::send(&dir, INSTANCE, &focus(Some(7))));
    entered_rx.recv_timeout(PATIENCE).unwrap();

    let (stopped_tx, stopped_rx) = mpsc::channel();
    std::thread::spawn(move || {
        server.stop();
        let _ = stopped_tx.send(());
    });
    let stopped = stopped_rx.recv_timeout(PATIENCE);
    release_tx.send(()).unwrap();
    assert!(
        stopped.is_ok(),
        "stop waited for a request whose handler was still running"
    );
    assert!(control::socket_path(scratch.dir(), INSTANCE)
        .unwrap()
        .symlink_metadata()
        .is_err());
    let _ = client.join().unwrap();
}

#[test]
fn a_request_missing_tab_or_token_is_refused() {
    let scratch = Scratch::new();
    let (server, calls) = counting_server(scratch.dir());
    for line in [
        r#"{"v":1,"verb":"focus","card_first":false,"intent":"show","token":null}"#,
        r#"{"v":1,"verb":"focus","tab":7,"card_first":false,"intent":"show"}"#,
        r#"{"v":1,"verb":"new"}"#,
    ] {
        assert!(serde_json::from_str::<Request>(line).is_err(), "{line} parsed");
        assert_eq!(
            raw_exchange(scratch.dir(), format!("{line}\n").as_bytes()),
            "refused: malformed request",
            "{line}"
        );
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    server.stop();
}

/// A non-blocking connect to `path`: the connected descriptor, or `None` when the accept queue is
/// full.
#[cfg(target_os = "linux")]
fn connect_unless_full(path: &Path) -> Option<std::os::fd::OwnedFd> {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;
    // SAFETY: plain socket creation; the descriptor is owned at once.
    let raw = unsafe {
        libc::socket(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
            0,
        )
    };
    assert!(raw >= 0, "socket: {}", io::Error::last_os_error());
    // SAFETY: `raw` was just opened here and nothing else owns it.
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    // SAFETY: an all-zero `sockaddr_un` is valid.
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    let bytes = path.as_os_str().as_bytes();
    assert!(bytes.len() < addr.sun_path.len());
    for (to, from) in addr.sun_path.iter_mut().zip(bytes) {
        *to = *from as libc::c_char;
    }
    // SAFETY: `fd` is open; `addr` is initialized and its size is passed.
    let rc = unsafe {
        libc::connect(
            fd.as_raw_fd(),
            &addr as *const _ as *const libc::sockaddr,
            std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t,
        )
    };
    if rc == 0 {
        return Some(fd);
    }
    let error = io::Error::last_os_error();
    assert_eq!(error.raw_os_error(), Some(libc::EAGAIN), "connect: {error}");
    None
}

#[cfg(target_os = "linux")]
#[test]
fn a_client_facing_a_full_accept_queue_leaves_no_connection_behind() {
    use std::os::fd::AsRawFd;
    let scratch = Scratch::new();
    let path = control::socket_path(scratch.dir(), INSTANCE).unwrap();
    let listener = UnixListener::bind(&path).unwrap();
    // SAFETY: `listener` is open; a second `listen` only shrinks its queue.
    assert_eq!(unsafe { libc::listen(listener.as_raw_fd(), 0) }, 0);
    let mut queued = Vec::new();
    while let Some(fd) = connect_unless_full(&path) {
        queued.push(fd);
        assert!(queued.len() < 64, "the accept queue never filled");
    }

    let observed = std::sync::Arc::new(std::sync::Mutex::new(None));
    let seen = observed.clone();
    eitri_core::instance_dir::observe_connect_socket_for_tests(move |fd| {
        *seen.lock().unwrap() = Some(std::fs::read_link(format!("/proc/self/fd/{fd}")).unwrap());
    });
    let err = control::send(scratch.dir(), INSTANCE, &focus(Some(7))).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::TimedOut);

    let socket = observed
        .lock()
        .unwrap()
        .take()
        .expect("the timed-out connect's socket was observed");
    let guard = std::time::Instant::now() + Duration::from_secs(60);
    while std::fs::read_dir("/proc/self/fd")
        .unwrap()
        .flatten()
        .any(|entry| std::fs::read_link(entry.path()).ok().as_ref() == Some(&socket))
    {
        assert!(
            std::time::Instant::now() < guard,
            "a timed-out connect retained its socket"
        );
        std::thread::yield_now();
    }
    listener.set_nonblocking(true).unwrap();
    let mut accepted = 0;
    while listener.accept().is_ok() {
        accepted += 1;
    }
    assert_eq!(
        accepted,
        queued.len(),
        "a connection arrived after the client had given up"
    );
}

/// The agents control socket and the remote observation socket are bound, made private and removed by one
/// sequence: either refusing what the other allows would leave one socket open to what the other keeps out.
#[test]
fn the_agents_and_remote_sockets_are_bound_and_removed_alike() {
    use eitri_core::remote_rv::{rv_socket_path, RvListener};
    use std::os::unix::fs::{symlink, MetadataExt};

    type Bind = fn(&Path) -> io::Result<Box<dyn std::any::Any>>;
    type Named = fn(&Path, &str) -> io::Result<PathBuf>;
    fn agents(dir: &Path) -> io::Result<Box<dyn std::any::Any>> {
        Server::bind(dir, INSTANCE, |_| Reply::Ok).map(|server| Box::new(server) as Box<dyn std::any::Any>)
    }
    fn remote(dir: &Path) -> io::Result<Box<dyn std::any::Any>> {
        RvListener::bind(dir, INSTANCE, |_| true).map(|listener| Box::new(listener) as Box<dyn std::any::Any>)
    }
    let sockets: [(&str, Bind, Named); 2] = [("ag", agents, control::socket_path), ("rv", remote, rv_socket_path)];

    for (prefix, bind, named) in sockets {
        // Only an instance id names a socket, with the same refusal.
        for bad in ["../a3f09c1d2e4b5f60", "A3F09C1D2E4B5F60", "a3f09c1d2e4b5f6"] {
            let refused = named(Path::new("/run/x"), bad).unwrap_err();
            assert_eq!(refused.kind(), io::ErrorKind::InvalidInput, "{prefix}");
            assert_eq!(
                refused.to_string(),
                format!("{bad:?} is not an instance id (16 lowercase hex digits)"),
                "{prefix}"
            );
        }

        // Bound 0600, and removed with its owner.
        let scratch = Scratch::new();
        let path = named(scratch.dir(), INSTANCE).unwrap();
        assert!(path.ends_with(format!("{prefix}-{INSTANCE}.sock")), "{prefix}");
        let bound = bind(scratch.dir()).unwrap();
        let meta = std::fs::symlink_metadata(&path).unwrap();
        assert!(meta.file_type().is_socket(), "{prefix}");
        assert_eq!(meta.mode() & 0o777, 0o600, "{prefix}");
        // A second bind of a name in use is refused, and leaves the first in place.
        assert!(bind(scratch.dir()).is_err(), "{prefix}: a name in use");
        assert_eq!(std::fs::symlink_metadata(&path).unwrap().ino(), meta.ino(), "{prefix}");
        drop(bound);
        assert!(std::fs::symlink_metadata(&path).is_err(), "{prefix}: removed on drop");

        // A name that holds something else by the time of the drop is not removed.
        let bound = bind(scratch.dir()).unwrap();
        let other = scratch.dir().join("other");
        std::fs::write(&other, "not the socket").unwrap();
        std::fs::rename(&other, &path).unwrap();
        drop(bound);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "not the socket", "{prefix}");
        std::fs::remove_file(&path).unwrap();

        // A link at the name is refused, and nothing is made where it points.
        let target = scratch.dir().join("elsewhere.sock");
        symlink(&target, &path).unwrap();
        assert!(bind(scratch.dir()).is_err(), "{prefix}: a link at the name");
        assert!(
            std::fs::symlink_metadata(&target).is_err(),
            "{prefix}: nothing made through the link"
        );
        assert!(
            std::fs::symlink_metadata(&path).unwrap().file_type().is_symlink(),
            "{prefix}"
        );
        std::fs::remove_file(&path).unwrap();

        // A directory others can write in is refused, with nothing made in it.
        std::fs::set_permissions(scratch.dir(), std::fs::Permissions::from_mode(0o777)).unwrap();
        let refused = bind(scratch.dir()).map(|_| ()).unwrap_err();
        assert_eq!(refused.kind(), io::ErrorKind::PermissionDenied, "{prefix}: {refused}");
        assert!(std::fs::symlink_metadata(&path).is_err(), "{prefix}");
    }
}
