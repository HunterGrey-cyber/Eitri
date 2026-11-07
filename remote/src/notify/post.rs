//! One wake-up: an HTTP POST of [`WAKE_BODY`] to a phone's UnifiedPush endpoint, and nothing else.
//!
//! The body is the same fixed bytes for every wake, so nothing a conversation holds can reach the push service. The
//! endpoint is the phone's to name, so the post must not become a way to reach the desk itself: the host is resolved
//! here, and when **any** address it resolves to is loopback, link-local, unspecified (`0.0.0.0/8`), broadcast,
//! multicast, an address of one of this machine's interfaces (its LAN, docker and VPN addresses, read at each post),
//! or the IPv4-mapped, IPv4-compatible or NAT64 (`64:ff9b::/96`) form of such an address, nothing is sent. Other
//! private ranges are let through: a self-hosted push server elsewhere on the user's network is the usual case.
//!
//! The connection goes to the checked addresses only (the name is never resolved a second time), follows no
//! redirect, reads the status line within the first [`STATUS_READ_MAX`] bytes and nothing more, and gives up after
//! [`POST_TIMEOUT`] in all, resolution included: the system's resolver takes no deadline, so it runs on a thread of
//! its own that the post stops waiting for ([`system_resolve`]). When this machine's addresses cannot be read, no
//! address can be told apart from them and nothing is sent. TLS is rustls with the system's roots, loaded once
//! ([`load_roots`]) before the helper confines itself.

use std::io::{self, Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, OnceLock};
use std::time::{Duration, Instant};

use rustls::pki_types::ServerName;

/// What every wake posts.
pub const WAKE_BODY: &[u8] = br#"{"v":1,"wake":true}"#;
/// The longest one post takes, resolution, connection, TLS and the status line together.
pub const POST_TIMEOUT: Duration = Duration::from_secs(10);
/// The most bytes of the answer read while looking for its status line.
pub const STATUS_READ_MAX: usize = 4096;

/// The variable a test build (`remote-test-hooks`) reads for the one `address:port` a post may reach whatever the
/// address checks say (the end-to-end test's own server on loopback). A release build has no such feature.
#[cfg(feature = "remote-test-hooks")]
pub const POST_ALLOW_TEST_VAR: &str = "EITRI_REMOTE_TEST_POST_ALLOW";

/// Why a wake was not delivered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PostError {
    /// Not posted: the endpoint is not one, or an address it names is refused.
    Refused(&'static str),
    /// Resolution, the connection, TLS or the answer failed, or the time ran out.
    Network(String),
    /// The push service answered with a status other than 2xx (a redirect included: none is followed).
    Status(u16),
}

impl PostError {
    /// What the audit log's `push` record says for it: the status, or an error word.
    pub fn word(&self) -> String {
        match self {
            PostError::Refused(_) => "refused".to_owned(),
            PostError::Network(why) if why.starts_with(TIMED_OUT) => "timeout".to_owned(),
            PostError::Network(_) => "network".to_owned(),
            PostError::Status(code) => code.to_string(),
        }
    }
}

const TIMED_OUT: &str = "timed out";

/// The most lookups a post gave up on that may still run; while that many do, no other is started.
const LOOKUPS_LEFT_MAX: usize = 4;
static LOOKUPS_LEFT: AtomicUsize = AtomicUsize::new(0);

/// Resolves `host` on `port` with the system's resolver, as `getaddrinfo` does, waiting at most `within`.
pub fn system_resolve(host: &str, port: u16, within: Duration) -> io::Result<Vec<SocketAddr>> {
    let host = host.to_owned();
    lookup_within(&LOOKUPS_LEFT, LOOKUPS_LEFT_MAX, within, move || {
        Ok((host.as_str(), port).to_socket_addrs()?.collect())
    })
}

/// Runs `lookup` on a thread of its own and waits at most `within` for its answer (`TimedOut` after that). A lookup
/// given up on runs to its end alone; `running` counts those still running, and while `cap` do, none is started.
fn lookup_within(
    running: &'static AtomicUsize,
    cap: usize,
    within: Duration,
    lookup: impl FnOnce() -> io::Result<Vec<SocketAddr>> + Send + 'static,
) -> io::Result<Vec<SocketAddr>> {
    if running.fetch_add(1, Ordering::SeqCst) >= cap {
        running.fetch_sub(1, Ordering::SeqCst);
        return Err(io::Error::new(io::ErrorKind::TimedOut, "earlier lookups still run"));
    }
    let (tx, rx) = mpsc::sync_channel(1);
    let spawned = std::thread::Builder::new()
        .name("eitri-remote-resolve".into())
        .spawn(move || {
            let _ = tx.send(lookup());
            running.fetch_sub(1, Ordering::SeqCst);
        });
    if let Err(e) = spawned {
        running.fetch_sub(1, Ordering::SeqCst);
        return Err(e);
    }
    rx.recv_timeout(within)
        .unwrap_or_else(|_| Err(io::Error::new(io::ErrorKind::TimedOut, "no answer in time")))
}

/// This machine's addresses now, every interface's.
pub fn own_addresses() -> io::Result<Vec<IpAddr>> {
    crate::serve::listen::interface_addresses().map(|all| all.into_iter().map(|(_, addr, _)| addr).collect())
}

/// How the resolver is called: the host, the port and the time left for it.
pub type Resolve<'a> = &'a dyn Fn(&str, u16, Duration) -> io::Result<Vec<SocketAddr>>;
/// How this machine's addresses are read.
pub type OwnAddresses<'a> = &'a dyn Fn() -> io::Result<Vec<IpAddr>>;

/// Posts one wake to `endpoint`; the status (2xx) when it was taken.
pub fn post_wake(endpoint: &str, resolve: Resolve<'_>, own: OwnAddresses<'_>) -> Result<u16, PostError> {
    post_wake_with(endpoint, resolve, own, test_allow(), POST_TIMEOUT)
}

/// The address a test build lets through, from [`POST_ALLOW_TEST_VAR`]; `None` in every other build.
fn test_allow() -> Option<SocketAddr> {
    #[cfg(feature = "remote-test-hooks")]
    {
        std::env::var(POST_ALLOW_TEST_VAR).ok().and_then(|v| v.parse().ok())
    }
    #[cfg(not(feature = "remote-test-hooks"))]
    {
        None
    }
}

/// [`post_wake`] with the one address the checks let through (`allow`) and the time it may take given.
pub(crate) fn post_wake_with(
    endpoint: &str,
    resolve: Resolve<'_>,
    own: OwnAddresses<'_>,
    allow: Option<SocketAddr>,
    timeout: Duration,
) -> Result<u16, PostError> {
    let deadline = Instant::now() + timeout;
    let target = Endpoint::parse(endpoint)?;
    let addrs = resolve(&target.host, target.port, left(deadline)?).map_err(|e| match e.kind() {
        io::ErrorKind::TimedOut => PostError::Network(format!("{TIMED_OUT}: resolving: {e}")),
        _ => PostError::Network(format!("resolving: {e}")),
    })?;
    if addrs.is_empty() {
        return Err(PostError::Network("the name resolves to no address".to_owned()));
    }
    let own = own().map_err(|_| PostError::Refused("this machine's addresses could not be read"))?;
    for addr in &addrs {
        if Some(*addr) == allow {
            continue;
        }
        if let Some(why) = refused_address(addr.ip(), &own) {
            return Err(PostError::Refused(why));
        }
    }
    let stream = connect(&addrs, deadline)?;
    let socket = stream.try_clone().map_err(net)?;
    let request = target.request();
    if target.tls {
        let name = ServerName::try_from(target.host.clone())
            .map_err(|_| PostError::Refused("the host is not a name TLS can check"))?;
        let conn =
            rustls::ClientConnection::new(tls_config(), name).map_err(|e| PostError::Network(format!("TLS: {e}")))?;
        exchange(rustls::StreamOwned::new(conn, socket), &stream, &request, deadline)
    } else {
        exchange(socket, &stream, &request, deadline)
    }
}

fn net(e: io::Error) -> PostError {
    if matches!(e.kind(), io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock) {
        PostError::Network(format!("{TIMED_OUT}: {e}"))
    } else {
        PostError::Network(e.to_string())
    }
}

/// The time left before `deadline`, or the timeout error.
fn left(deadline: Instant) -> Result<Duration, PostError> {
    match deadline.checked_duration_since(Instant::now()) {
        Some(left) if !left.is_zero() => Ok(left),
        _ => Err(PostError::Network(TIMED_OUT.to_owned())),
    }
}

/// Connects to the first of `addrs` that answers, in the resolver's order.
fn connect(addrs: &[SocketAddr], deadline: Instant) -> Result<TcpStream, PostError> {
    let mut last = PostError::Network("no address answered".to_owned());
    for addr in addrs {
        match TcpStream::connect_timeout(addr, left(deadline)?) {
            Ok(stream) => return Ok(stream),
            Err(e) => last = net(e),
        }
    }
    Err(last)
}

/// Writes the request and reads the status line through `io`, each step bounded by what is left of `deadline` on
/// `socket` (the TCP stream under `io`).
fn exchange(
    mut io: impl Read + Write,
    socket: &TcpStream,
    request: &[u8],
    deadline: Instant,
) -> Result<u16, PostError> {
    socket.set_write_timeout(Some(left(deadline)?)).map_err(net)?;
    socket.set_read_timeout(Some(left(deadline)?)).map_err(net)?;
    io.write_all(request).map_err(net)?;
    io.flush().map_err(net)?;
    let mut answer = Vec::with_capacity(256);
    let mut chunk = [0u8; 512];
    loop {
        if let Some(code) = status_of(&answer)? {
            return if (200..300).contains(&code) {
                Ok(code)
            } else {
                Err(PostError::Status(code))
            };
        }
        if answer.len() >= STATUS_READ_MAX {
            return Err(PostError::Network(
                "no status line in the answer's first 4 KiB".to_owned(),
            ));
        }
        socket.set_read_timeout(Some(left(deadline)?)).map_err(net)?;
        let room = (STATUS_READ_MAX - answer.len()).min(chunk.len());
        match io.read(&mut chunk[..room]) {
            Ok(0) => return Err(PostError::Network("the answer ended before its status line".to_owned())),
            Ok(n) => answer.extend_from_slice(&chunk[..n]),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(net(e)),
        }
    }
}

/// The status of an answer that starts `HTTP/1.x NNN`, once its first line is whole.
fn status_of(answer: &[u8]) -> Result<Option<u16>, PostError> {
    let Some(end) = answer.windows(2).position(|w| w == b"\r\n") else {
        return Ok(None);
    };
    let line = &answer[..end];
    let bad = || PostError::Network("the answer's first line is not an HTTP status line".to_owned());
    let rest = line
        .strip_prefix(b"HTTP/1.1 ")
        .or_else(|| line.strip_prefix(b"HTTP/1.0 "))
        .ok_or_else(bad)?;
    let digits = rest.get(..3).ok_or_else(bad)?;
    if !digits.iter().all(u8::is_ascii_digit) || rest.get(3).is_some_and(|b| *b != b' ') {
        return Err(bad());
    }
    let code = std::str::from_utf8(digits)
        .map_err(|_| bad())?
        .parse()
        .map_err(|_| bad())?;
    Ok(Some(code))
}

/// A push endpoint taken apart.
#[derive(Debug, PartialEq, Eq)]
struct Endpoint {
    tls: bool,
    /// The host without brackets.
    host: String,
    port: u16,
    /// The path and query, `/` when the URL has none.
    path: String,
    /// What `Host` says: the host as written, with its port when it is not the scheme's.
    authority: String,
}

impl Endpoint {
    fn parse(url: &str) -> Result<Endpoint, PostError> {
        eitri_core::remote_devices::push_endpoint_ok(url)
            .map_err(|_| PostError::Refused("not an http or https URL"))?;
        let (scheme, rest) = url
            .split_once("://")
            .ok_or(PostError::Refused("not an http or https URL"))?;
        let tls = scheme.eq_ignore_ascii_case("https");
        let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        let authority = &rest[..end];
        let path = rest[end..].split('#').next().unwrap_or_default();
        let path = if path.starts_with('/') {
            path.to_owned()
        } else {
            format!("/{path}")
        };
        let (host, port) = match authority.strip_prefix('[') {
            Some(bracketed) => {
                let (host, after) = bracketed.split_once(']').ok_or(PostError::Refused("a bad IPv6 host"))?;
                (host, after.strip_prefix(':'))
            }
            None => match authority.split_once(':') {
                Some((host, port)) => (host, Some(port)),
                None => (authority, None),
            },
        };
        let default = if tls { 443 } else { 80 };
        let port = match port {
            None => default,
            Some(text) => match text.parse::<u16>() {
                Ok(port) if port != 0 => port,
                _ => return Err(PostError::Refused("a bad port")),
            },
        };
        Ok(Endpoint {
            tls,
            host: host.to_ascii_lowercase(),
            port,
            path,
            authority: authority.to_ascii_lowercase(),
        })
    }

    fn request(&self) -> Vec<u8> {
        let mut request = format!(
            "POST {} HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            self.path,
            self.authority,
            WAKE_BODY.len()
        )
        .into_bytes();
        request.extend_from_slice(WAKE_BODY);
        request
    }
}

/// Why a post may not go to `addr`, given this machine's own addresses `own`; `None` when it may.
pub fn refused_address(addr: IpAddr, own: &[IpAddr]) -> Option<&'static str> {
    match addr {
        IpAddr::V4(v4) => refused_v4(v4, own),
        IpAddr::V6(v6) => {
            if v6.is_loopback() {
                return Some("loopback");
            }
            if v6.is_unspecified() {
                return Some("unspecified");
            }
            // `::ffff:a.b.c.d` and the old `::a.b.c.d` reach the IPv4 address they carry.
            if let Some(v4) = v6.to_ipv4() {
                if refused_v4(v4, own).is_some() {
                    return Some("an IPv4 address refused, written as IPv6");
                }
            }
            if let Some(v4) = nat64(v6) {
                if refused_v4(v4, own).is_some() {
                    return Some("an IPv4 address refused, through NAT64");
                }
            }
            if v6.is_multicast() {
                return Some("multicast");
            }
            if v6.is_unicast_link_local() {
                return Some("link-local");
            }
            if own.iter().any(|o| o.to_canonical() == IpAddr::V6(v6)) {
                return Some("an address of this machine");
            }
            None
        }
    }
}

fn refused_v4(v4: Ipv4Addr, own: &[IpAddr]) -> Option<&'static str> {
    if v4.is_loopback() {
        Some("loopback")
    } else if v4.octets()[0] == 0 {
        Some("unspecified")
    } else if v4.is_link_local() {
        Some("link-local")
    } else if v4.is_broadcast() {
        Some("broadcast")
    } else if v4.is_multicast() {
        Some("multicast")
    } else if own.iter().any(|o| o.to_canonical() == IpAddr::V4(v4)) {
        Some("an address of this machine")
    } else {
        None
    }
}

/// The IPv4 address a well-known NAT64 prefix (`64:ff9b::/96`) carries.
fn nat64(v6: Ipv6Addr) -> Option<Ipv4Addr> {
    let s = v6.segments();
    (s[..6] == [0x64, 0xff9b, 0, 0, 0, 0]).then(|| {
        let o = v6.octets();
        Ipv4Addr::new(o[12], o[13], o[14], o[15])
    })
}

/// The client configuration and how many system roots it holds, made once.
static TLS: OnceLock<(Arc<rustls::ClientConfig>, usize)> = OnceLock::new();

/// Loads the system's trust roots (with any CA the user added to it) into the TLS configuration, once; how many
/// were loaded. Called by the helper before it confines itself, since the roots are files it may not read after.
pub fn load_roots() -> usize {
    TLS.get_or_init(build_tls).1
}

fn tls_config() -> Arc<rustls::ClientConfig> {
    TLS.get_or_init(build_tls).0.clone()
}

fn build_tls() -> (Arc<rustls::ClientConfig>, usize) {
    let mut roots = rustls::RootCertStore::empty();
    let found = rustls_native_certs::load_native_certs();
    let (added, _ignored) = roots.add_parsable_certificates(found.certs);
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("ring supports the default protocol versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
    (Arc::new(config), added)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;

    fn ip(text: &str) -> IpAddr {
        text.parse().unwrap()
    }

    fn none() -> io::Result<Vec<IpAddr>> {
        Ok(Vec::new())
    }

    fn resolve_to(addrs: Vec<SocketAddr>) -> impl Fn(&str, u16, Duration) -> io::Result<Vec<SocketAddr>> {
        move |_, _, _| Ok(addrs.clone())
    }

    #[test]
    fn endpoints_on_the_desk_or_its_edge_are_never_posted() {
        // The desk's own addresses, from the documentation ranges.
        let own_v4 = ip("198.51.100.20");
        let own_v6 = ip("2001:db8:7::20");
        let own = [own_v4, own_v6];
        for refused in [
            "127.0.0.1",
            "127.8.9.10",
            "::1",
            "169.254.1.1",
            "fe80::1",
            "::ffff:127.0.0.1",
            "::ffff:169.254.1.1",
            "64:ff9b::7f00:1",
            "0.0.0.0",
            "0.1.2.3",
            "::",
            "255.255.255.255",
            "224.0.0.1",
            "ff02::1",
            "198.51.100.20",
            "::ffff:198.51.100.20",
            "64:ff9b::c633:6414",
            "2001:db8:7::20",
        ] {
            assert!(
                refused_address(ip(refused), &own).is_some(),
                "{refused} was let through"
            );
        }
        for allowed in [
            "93.184.216.34",
            "198.51.100.21",
            "2001:db8::5",
            "fd00:7::21",
            "::ffff:93.184.216.34",
        ] {
            assert_eq!(refused_address(ip(allowed), &own), None, "{allowed} was refused");
        }
        // Private ranges elsewhere on the user's network pass: a self-hosted push server.
        for allowed in [
            Ipv4Addr::new(10, 0, 0, 5),
            Ipv4Addr::new(172, 20, 1, 2),
            Ipv4Addr::new(192, 168, 7, 21),
        ] {
            assert_eq!(
                refused_address(IpAddr::V4(allowed), &own),
                None,
                "{allowed} was refused"
            );
        }
        // This host's own addresses, read as a post reads them, are refused too.
        let here = own_addresses().expect("this host's addresses");
        for addr in &here {
            assert!(
                refused_address(*addr, &here).is_some(),
                "{addr} of this host was let through"
            );
        }
        // One refused address among public ones refuses the whole name, before any connection.
        let mixed = resolve_to(vec![
            "93.184.216.34:443".parse().unwrap(),
            "127.0.0.1:443".parse().unwrap(),
        ]);
        assert_eq!(
            post_wake_with("https://push.example/up", &mixed, &none, None, POST_TIMEOUT),
            Err(PostError::Refused("loopback"))
        );
        let own_in_list = resolve_to(vec!["198.51.100.20:80".parse().unwrap()]);
        assert_eq!(
            post_wake_with(
                "http://desk.lan/up",
                &own_in_list,
                &|| Ok(own.to_vec()),
                None,
                POST_TIMEOUT
            ),
            Err(PostError::Refused("an address of this machine"))
        );
        // When this machine's addresses cannot be read, none of them can be told apart: nothing is posted.
        let public = resolve_to(vec!["93.184.216.34:443".parse().unwrap()]);
        assert_eq!(
            post_wake_with(
                "https://push.example/up",
                &public,
                &|| Err(io::Error::other("no interface list")),
                None,
                POST_TIMEOUT
            ),
            Err(PostError::Refused("this machine's addresses could not be read"))
        );
        // Not a URL the store would keep is never posted either.
        let anything = resolve_to(vec!["93.184.216.34:80".parse().unwrap()]);
        assert!(matches!(
            post_wake_with("ftp://push.example/", &anything, &none, None, POST_TIMEOUT),
            Err(PostError::Refused(_))
        ));
    }

    /// A server on loopback that answers each connection with `answer` (or never, for `None`) and hands the test
    /// what it read.
    fn server(answer: Option<&'static [u8]>) -> (SocketAddr, mpsc::Receiver<Vec<u8>>, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = mpsc::channel();
        let accepted = Arc::new(AtomicUsize::new(0));
        let count = accepted.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                count.fetch_add(1, Ordering::SeqCst);
                let tx = tx.clone();
                std::thread::spawn(move || {
                    stream.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
                    let mut seen = Vec::new();
                    let mut buf = [0u8; 1024];
                    // The head, then as many body bytes as it says.
                    while let Ok(n) = stream.read(&mut buf) {
                        if n == 0 {
                            break;
                        }
                        seen.extend_from_slice(&buf[..n]);
                        if let Some(head) = seen.windows(4).position(|w| w == b"\r\n\r\n") {
                            let text = String::from_utf8_lossy(&seen[..head]).to_ascii_lowercase();
                            let length = text
                                .lines()
                                .find_map(|l| l.strip_prefix("content-length: "))
                                .and_then(|v| v.trim().parse::<usize>().ok())
                                .unwrap_or(0);
                            if seen.len() >= head + 4 + length {
                                break;
                            }
                        }
                    }
                    let _ = tx.send(seen);
                    match answer {
                        Some(bytes) => {
                            let _ = stream.write_all(bytes);
                        }
                        // Holds the connection open and says nothing.
                        None => std::thread::sleep(Duration::from_secs(30)),
                    }
                });
            }
        });
        (addr, rx, accepted)
    }

    #[test]
    fn body_is_the_fixed_wake() {
        let (addr, seen, _) = server(Some(b"HTTP/1.1 201 Created\r\nContent-Length: 0\r\n\r\n"));
        let url = format!("http://127.0.0.1:{}/up/AbC?x=1", addr.port());
        let resolve = resolve_to(vec![addr]);
        assert_eq!(post_wake_with(&url, &resolve, &none, Some(addr), POST_TIMEOUT), Ok(201));
        let request = seen.recv_timeout(Duration::from_secs(30)).unwrap();
        let text = String::from_utf8(request).unwrap();
        let (head, body) = text.split_once("\r\n\r\n").unwrap();
        assert!(head.starts_with("POST /up/AbC?x=1 HTTP/1.1\r\n"), "{head}");
        assert!(head.contains(&format!("\r\nHost: 127.0.0.1:{}", addr.port())), "{head}");
        assert!(head.contains("\r\nContent-Length: 19"), "{head}");
        assert_eq!(body.as_bytes(), WAKE_BODY);
        assert_eq!(WAKE_BODY, br#"{"v":1,"wake":true}"#);
    }

    #[test]
    fn redirects_are_not_followed() {
        let (other, _, other_accepted) = server(Some(b"HTTP/1.1 200 OK\r\n\r\n"));
        let redirect: &'static [u8] = Box::leak(
            format!(
                "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:{}/\r\nContent-Length: 0\r\n\r\n",
                other.port()
            )
            .into_bytes()
            .into_boxed_slice(),
        );
        let (addr, seen, _) = server(Some(redirect));
        let url = format!("http://127.0.0.1:{}/up", addr.port());
        let resolve = resolve_to(vec![addr]);
        assert_eq!(
            post_wake_with(&url, &resolve, &none, Some(addr), POST_TIMEOUT),
            Err(PostError::Status(302))
        );
        seen.recv_timeout(Duration::from_secs(30)).unwrap();
        assert_eq!(other_accepted.load(Ordering::SeqCst), 0, "the redirect was followed");
        // Without the allowance the loopback server is not reached at all.
        assert_eq!(
            post_wake_with(&url, &resolve, &none, None, POST_TIMEOUT),
            Err(PostError::Refused("loopback"))
        );
    }

    #[test]
    fn post_times_out() {
        let (addr, seen, _) = server(None);
        let url = format!("http://127.0.0.1:{}/up", addr.port());
        let resolve = resolve_to(vec![addr]);
        let err = post_wake_with(&url, &resolve, &none, Some(addr), Duration::from_millis(300)).unwrap_err();
        assert_eq!(err.word(), "timeout", "{err:?}");
        // It did send its wake before it waited.
        let request = seen.recv_timeout(Duration::from_secs(30)).unwrap();
        assert!(request.ends_with(WAKE_BODY));
    }

    /// A lookup that answers only once `release` goes.
    fn hanging_lookup(
        release: &Arc<std::sync::Mutex<mpsc::Receiver<()>>>,
    ) -> impl FnOnce() -> io::Result<Vec<SocketAddr>> + Send + 'static {
        let release = release.clone();
        move || {
            let _ = release.lock().unwrap().recv();
            Err(io::Error::other("released"))
        }
    }

    #[test]
    fn a_resolver_that_hangs_ends_the_post_as_a_timeout() {
        static RUNNING: AtomicUsize = AtomicUsize::new(0);
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let release = Arc::new(std::sync::Mutex::new(release_rx));
        let given = Arc::new(std::sync::Mutex::new(None));
        let hanging = {
            let given = given.clone();
            move |_: &str, _: u16, within: Duration| {
                *given.lock().unwrap() = Some(within);
                lookup_within(&RUNNING, 4, within, hanging_lookup(&release))
            }
        };
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(post_wake_with(
                "https://push.example/up",
                &hanging,
                &none,
                None,
                Duration::from_millis(300),
            ));
        });
        let result = rx
            .recv_timeout(Duration::from_secs(30))
            .expect("the post waited on the resolver past its own bound");
        assert_eq!(result.unwrap_err().word(), "timeout");
        let within = given.lock().unwrap().expect("the resolver was asked");
        assert!(
            within <= Duration::from_millis(300),
            "the resolver was given {within:?}"
        );
        assert_eq!(
            RUNNING.load(Ordering::SeqCst),
            1,
            "the lookup given up on runs on alone"
        );
        drop(release_tx);
    }

    #[test]
    fn lookups_left_running_are_bounded() {
        static RUNNING: AtomicUsize = AtomicUsize::new(0);
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let release = Arc::new(std::sync::Mutex::new(release_rx));
        for _ in 0..2 {
            let err = lookup_within(&RUNNING, 2, Duration::from_millis(50), hanging_lookup(&release)).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::TimedOut, "{err}");
        }
        let started = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let err = lookup_within(&RUNNING, 2, Duration::from_secs(30), {
            let started = started.clone();
            move || {
                started.store(true, Ordering::SeqCst);
                Ok(Vec::new())
            }
        })
        .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut, "{err}");
        assert!(
            !started.load(Ordering::SeqCst),
            "a third lookup started beside two still running"
        );
        drop(release_tx);
    }

    #[test]
    fn status_lines_are_read_strictly() {
        assert_eq!(status_of(b"HTTP/1.1 200 OK\r\n"), Ok(Some(200)));
        assert_eq!(status_of(b"HTTP/1.0 404\r\n"), Ok(Some(404)));
        assert_eq!(status_of(b"HTTP/1.1 20"), Ok(None));
        assert!(status_of(b"SSH-2.0-OpenSSH\r\n").is_err());
        assert!(status_of(b"HTTP/1.1 2000 OK\r\n").is_err());
    }

    #[test]
    fn endpoints_are_taken_apart() {
        let e = Endpoint::parse("https://Push.Example:8443/up/x?y=1#frag").unwrap();
        assert_eq!(
            (e.tls, e.host.as_str(), e.port, e.path.as_str(), e.authority.as_str()),
            (true, "push.example", 8443, "/up/x?y=1", "push.example:8443")
        );
        let e = Endpoint::parse("http://[2001:db8::5]/").unwrap();
        assert_eq!((e.tls, e.host.as_str(), e.port), (false, "2001:db8::5", 80));
        let e = Endpoint::parse("https://push.example?k=v").unwrap();
        assert_eq!((e.port, e.path.as_str()), (443, "/?k=v"));
        assert!(Endpoint::parse("https://user@[2001:db8::5]/").is_err());
    }

    #[cfg(feature = "remote-test-hooks")]
    #[test]
    fn post_allow_hook_names_one_address_in_a_test_build() {
        assert_eq!(POST_ALLOW_TEST_VAR, "EITRI_REMOTE_TEST_POST_ALLOW");
    }

    #[cfg(not(feature = "remote-test-hooks"))]
    #[test]
    fn post_allow_hook_absent_without_the_feature() {
        // Set as a test build would set it: a build without the feature never reads it.
        std::env::set_var("EITRI_REMOTE_TEST_POST_ALLOW", "127.0.0.1:9");
        assert_eq!(test_allow(), None);
        let resolve = resolve_to(vec!["127.0.0.1:9".parse().unwrap()]);
        assert_eq!(
            post_wake("http://127.0.0.1:9/", &resolve, &none),
            Err(PostError::Refused("loopback"))
        );
        std::env::remove_var("EITRI_REMOTE_TEST_POST_ALLOW");
    }
}
