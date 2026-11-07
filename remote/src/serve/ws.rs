//! The one route `serve` answers: a WebSocket upgrade on [`WS_PATH`]. There is no static route and no page: anything
//! else is 404, and a request whose `Host` or `Origin` this server does not answer to is 403 before its path is read.
//!
//! The request parse and the 101 are the IDE listener's (`eitri_core::ide_server::handshake`), so both listeners read
//! a head the same strict way; the checks are this server's own.

use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use eitri_core::ide_server::handshake::{head_end, refusal, upgrade_answer_with, HeadEnd, RequestHead};

use super::config::ServeConfig;
use super::origin::check_request;

/// The path of the phone's WebSocket. Exact: a proxy that serves it under a prefix is refused, as is a query.
pub const WS_PATH: &str = "/v1/ws";
/// The most a request's head may hold.
pub const HEAD_BYTES: usize = 8 * 1024;
/// A connection that has not sent a whole head by then is closed.
pub const HEAD_WITHIN: Duration = Duration::from_secs(5);
/// An upgraded connection that sends nothing for this long is closed (a phone pings well within it).
pub const SESSION_IDLE: Duration = Duration::from_secs(120);

/// What the bytes of a request come to so far.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    /// No blank line yet, and under the cap.
    NeedMore,
    /// The 101: write it, and the connection speaks WebSocket from then on. `host` is the request's `Host`, `origin`
    /// its `Origin` when it sent one.
    Upgrade {
        answer: Vec<u8>,
        host: String,
        origin: Option<String>,
    },
    /// An HTTP error: write it, then close. `why` is a constant naming the check, so nothing of the request reaches a
    /// log through it.
    Refuse { answer: Vec<u8>, why: &'static str },
}

const MALFORMED: &str = "not a well-formed request";
/// A request for another path than [`WS_PATH`] (a proxy serving it under a prefix, a query, any page).
pub const PATH_REFUSED: &str = "not the one path served";
/// A request for [`WS_PATH`] that asks no WebSocket upgrade.
pub const NOT_UPGRADE: &str = "not a WebSocket upgrade";

fn refuse(status: &str, why: &'static str) -> Answer {
    Answer::Refuse {
        answer: refusal(status),
        why,
    }
}

/// Judges `head`, everything read from a connection so far, in this order: the cap and the shape (400), `Host` and
/// `Origin` (403; exactly one `Host`, at most one `Origin`), the path and the upgrade (404).
pub fn answer(head: &[u8], cfg: &ServeConfig) -> Answer {
    let block = match head_end(head, HEAD_BYTES) {
        HeadEnd::NeedMore => return Answer::NeedMore,
        HeadEnd::TooLong => return refuse("400 Bad Request", MALFORMED),
        HeadEnd::Whole(block) => block,
    };
    let Some(request) = RequestHead::parse(block) else {
        return refuse("400 Bad Request", MALFORMED);
    };
    let Some(host) = request.one("host") else {
        return refuse("403 Forbidden", super::origin::HOST_REFUSED);
    };
    let origin = match request.all("origin").as_slice() {
        [] => None,
        [one] => Some(*one),
        _ => return refuse("403 Forbidden", super::origin::ORIGIN_REFUSED),
    };
    if let Err(why) = check_request(host, origin, cfg) {
        return refuse("403 Forbidden", why);
    }
    if request.target() != WS_PATH {
        return refuse("404 Not Found", PATH_REFUSED);
    }
    match request.websocket_key() {
        Some(key) => Answer::Upgrade {
            answer: upgrade_answer_with(key, None),
            host: host.to_owned(),
            origin: origin.map(str::to_owned),
        },
        None => refuse("404 Not Found", NOT_UPGRADE),
    }
}

/// A connected stream `serve` can bound in time, hand to a second thread and shut from a third.
pub trait Conn: Read + Write + Send + Sync {
    fn set_read_deadline(&self, within: Option<Duration>) -> io::Result<()>;
    fn set_write_deadline(&self, within: Option<Duration>) -> io::Result<()>;
    /// Another handle on the same connection.
    fn try_clone_conn(&self) -> io::Result<Box<dyn Conn>>;
    /// Ends the connection both ways; a thread blocked on it wakes.
    fn shut(&self);
}

impl Conn for TcpStream {
    fn set_read_deadline(&self, within: Option<Duration>) -> io::Result<()> {
        self.set_read_timeout(within)
    }

    fn set_write_deadline(&self, within: Option<Duration>) -> io::Result<()> {
        self.set_write_timeout(within)
    }

    fn try_clone_conn(&self) -> io::Result<Box<dyn Conn>> {
        Ok(Box::new(self.try_clone()?))
    }

    fn shut(&self) {
        let _ = self.shutdown(Shutdown::Both);
    }
}

impl Conn for UnixStream {
    fn set_read_deadline(&self, within: Option<Duration>) -> io::Result<()> {
        self.set_read_timeout(within)
    }

    fn set_write_deadline(&self, within: Option<Duration>) -> io::Result<()> {
        self.set_write_timeout(within)
    }

    fn try_clone_conn(&self) -> io::Result<Box<dyn Conn>> {
        Ok(Box::new(self.try_clone()?))
    }

    fn shut(&self) {
        let _ = self.shutdown(Shutdown::Both);
    }
}

/// How a connection's request ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Handshake {
    /// The 101 was written: the request's `Host`, and its `Origin` when it sent one.
    Upgraded { host: String, origin: Option<String> },
    /// The request was refused for `why` (one of this module's or `origin`'s constants) and the answer written.
    Refused(&'static str),
    /// The client sent no whole request in time, or left.
    Gone,
}

/// Reads a request from `stream` within [`HEAD_WITHIN`] and answers it. A refusal is written, said on stderr (each
/// reason once in the process's life, so a client knocking cannot fill it) and returned, so the caller can record it.
pub fn handshake(stream: &mut dyn Conn, cfg: &ServeConfig) -> io::Result<Handshake> {
    let started = Instant::now();
    let mut head = Vec::with_capacity(1024);
    let mut buf = [0u8; 1024];
    loop {
        let left = HEAD_WITHIN.saturating_sub(started.elapsed());
        if left.is_zero() {
            return Ok(Handshake::Gone);
        }
        stream.set_read_deadline(Some(left))?;
        let got = match stream.read(&mut buf) {
            Ok(0) => return Ok(Handshake::Gone),
            Ok(got) => got,
            Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {
                return Ok(Handshake::Gone)
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        head.extend_from_slice(&buf[..got]);
        match answer(&head, cfg) {
            Answer::NeedMore => {}
            Answer::Upgrade { answer, host, origin } => {
                stream.write_all(&answer)?;
                return Ok(Handshake::Upgraded { host, origin });
            }
            Answer::Refuse { answer, why } => {
                log_refusal(why);
                let _ = stream.write_all(&answer);
                return Ok(Handshake::Refused(why));
            }
        }
    }
}

/// One stderr line per refusal reason, the first time it happens.
fn log_refusal(why: &'static str) {
    static LOGGED: AtomicU32 = AtomicU32::new(0);
    let reasons = [
        MALFORMED,
        PATH_REFUSED,
        NOT_UPGRADE,
        super::origin::HOST_REFUSED,
        super::origin::ORIGIN_REFUSED,
    ];
    let bit = reasons.iter().position(|r| *r == why).map_or(1 << 31, |at| 1 << at);
    if LOGGED.fetch_or(bit, Ordering::Relaxed) & bit == 0 {
        eprintln!("[remote] refused a client: {why}");
    }
}

#[cfg(test)]
mod tests {
    use super::super::config::Listen;
    use super::*;

    const KEY: &str = "dGhlIHNhbXBsZSBub25jZQ==";
    const ACCEPT: &str = "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=";

    fn cfg() -> ServeConfig {
        ServeConfig {
            listen: Listen::Loopback,
            port: 41000,
            origins: vec!["desk.tail1.ts.net".to_owned()],
            audit_keep_days: eitri_core::remote_audit::AUDIT_KEEP_DAYS_DEFAULT,
            notify: true,
        }
    }

    fn request(target: &str, extra: &[(&str, &str)]) -> Vec<u8> {
        let mut text = format!(
            "GET {target} HTTP/1.1\r\nHost: 127.0.0.1:41000\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
             Sec-WebSocket-Key: {KEY}\r\nSec-WebSocket-Version: 13\r\n"
        );
        for (name, value) in extra {
            text.push_str(&format!("{name}: {value}\r\n"));
        }
        text.push_str("\r\n");
        text.into_bytes()
    }

    fn status(answer: &Answer) -> u16 {
        let bytes = match answer {
            Answer::NeedMore => panic!("a whole head got NeedMore"),
            Answer::Upgrade { answer, .. } | Answer::Refuse { answer, .. } => answer,
        };
        std::str::from_utf8(bytes).unwrap()[9..12].parse().unwrap()
    }

    #[test]
    fn only_the_ws_path_upgrades() {
        let upgraded = answer(&request(WS_PATH, &[]), &cfg());
        assert_eq!(status(&upgraded), 101);
        let Answer::Upgrade {
            answer: bytes,
            host,
            origin,
        } = upgraded
        else {
            unreachable!()
        };
        assert_eq!(
            host, "127.0.0.1:41000",
            "the session is told the Host it was reached through"
        );
        assert_eq!(origin, None, "a native client sends no Origin");
        let Answer::Upgrade { origin, .. } = answer(&request(WS_PATH, &[("Origin", "http://localhost:5173")]), &cfg())
        else {
            panic!("a loopback page's Origin upgrades")
        };
        assert_eq!(origin.as_deref(), Some("http://localhost:5173"));
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.contains(ACCEPT), "{text}");
        assert!(!text.contains("Sec-WebSocket-Protocol"), "{text}");
        for target in [
            "/",
            "/index.html",
            "/v1",
            "/v1/",
            "/v1/ws/",
            "/v1/wss",
            "/V1/WS",
            "*",
            "http://127.0.0.1:41000/v1/ws",
        ] {
            assert_eq!(status(&answer(&request(target, &[]), &cfg())), 404, "{target}");
        }
        let plain_get = b"GET /v1/ws HTTP/1.1\r\nHost: 127.0.0.1:41000\r\n\r\n";
        assert_eq!(status(&answer(plain_get, &cfg())), 404, "a GET that asks no upgrade");
        assert!(
            matches!(answer(plain_get, &cfg()), Answer::Refuse { why: NOT_UPGRADE, .. }),
            "the right path without an upgrade is not a path refusal"
        );
    }

    #[test]
    fn path_prefix_is_refused() {
        for target in ["/eitri/v1/ws", "/v1/ws?token=x", "/v1/ws#x", "/./v1/ws", "//v1/ws"] {
            let refused = answer(&request(target, &[]), &cfg());
            assert_eq!(status(&refused), 404, "{target}");
            assert!(matches!(refused, Answer::Refuse { why: PATH_REFUSED, .. }), "{target}");
        }
    }

    #[test]
    fn host_and_origin_are_judged_before_the_path() {
        let rebinding = String::from_utf8(request("/", &[]))
            .unwrap()
            .replace("Host: 127.0.0.1:41000", "Host: evil.example");
        assert_eq!(status(&answer(rebinding.as_bytes(), &cfg())), 403);
        let cross_site = request("/nothing", &[("Origin", "https://evil.example")]);
        assert_eq!(status(&answer(&cross_site, &cfg())), 403);
        let two_origins = request(
            WS_PATH,
            &[("Origin", "http://localhost"), ("Origin", "http://localhost")],
        );
        assert_eq!(status(&answer(&two_origins, &cfg())), 403);
        let no_host = b"GET /v1/ws HTTP/1.1\r\nUpgrade: websocket\r\n\r\n";
        assert_eq!(status(&answer(no_host, &cfg())), 403);
        let proxied = String::from_utf8(request(WS_PATH, &[("Origin", "https://desk.tail1.ts.net")]))
            .unwrap()
            .replace("Host: 127.0.0.1:41000", "Host: desk.tail1.ts.net");
        assert_eq!(status(&answer(proxied.as_bytes(), &cfg())), 101);
    }

    #[test]
    fn a_head_over_the_cap_or_malformed_is_400() {
        assert_eq!(answer(b"GET /v1/ws HTTP/1.1\r\nHost: 1", &cfg()), Answer::NeedMore);
        assert_eq!(status(&answer(&vec![b'a'; HEAD_BYTES], &cfg())), 400);
        assert_eq!(
            status(&answer(b"POST /v1/ws HTTP/1.1\r\nHost: localhost\r\n\r\n", &cfg())),
            400
        );
    }

    #[test]
    fn a_refusal_repeats_nothing_of_the_request() {
        let probe = request("/secret-path-marker", &[("X-Marker", "secret-header-marker")]);
        let Answer::Refuse { answer: bytes, .. } = answer(&probe, &cfg()) else {
            panic!("a request for another path is refused")
        };
        let text = String::from_utf8(bytes).unwrap();
        assert!(!text.contains("marker"), "{text}");
    }
}
