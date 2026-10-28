//! A loopback HTTP proxy that refuses everything and remembers where each request was headed.
//!
//! Pointed at by `HTTPS_PROXY`/`HTTP_PROXY` in the runs that want to know whether the CLI tries to
//! reach a host other than the scripted endpoint. Every `CONNECT host:port` and every absolute-URI
//! request is appended to `tripwire.jsonl` in the log directory and answered with 403; the host list is
//! kept in memory for the stand-in's cleanliness check.
use serde_json::json;
use std::{
    fs::File,
    io::{self, Read, Write},
    net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

/// Where the proxy listens in a harness run.
pub const DEFAULT_ADDR: &str = "127.0.0.1:4142";

const MAX_HEAD: usize = 16 * 1024;

struct Shared {
    hosts: Mutex<Vec<String>>,
    log: Mutex<File>,
    stop: AtomicBool,
}

pub struct Tripwire {
    addr: SocketAddr,
    log_path: PathBuf,
    shared: Arc<Shared>,
    listener: Option<JoinHandle<()>>,
}

impl Tripwire {
    /// Listens on `addr` and appends to `<log_dir>/tripwire.jsonl`, which must not exist yet.
    pub fn start(addr: impl ToSocketAddrs, log_dir: &Path) -> io::Result<Self> {
        crate::private_fs::create_private_dir_all(log_dir, log_dir)?;
        let log_path = log_dir.join("tripwire.jsonl");
        if log_path.try_exists()? {
            return Err(io::Error::new(io::ErrorKind::AlreadyExists, "tripwire log exists"));
        }
        let log = crate::private_fs::open_private(&log_path)?;
        let listener = TcpListener::bind(addr)?;
        let addr = listener.local_addr()?;
        listener.set_nonblocking(true)?;
        let shared = Arc::new(Shared {
            hosts: Mutex::new(Vec::new()),
            log: Mutex::new(log),
            stop: AtomicBool::new(false),
        });
        let worker = shared.clone();
        let handle = thread::Builder::new().name("tripwire".into()).spawn(move || {
            let mut connections: Vec<JoinHandle<()>> = Vec::new();
            while !worker.stop.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let shared = worker.clone();
                        connections.push(thread::spawn(move || serve(stream, &shared)));
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::Interrupted => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => thread::sleep(Duration::from_millis(10)),
                }
                connections.retain(|c| !c.is_finished());
            }
            for connection in connections {
                let _ = connection.join();
            }
        })?;
        Ok(Self {
            addr,
            log_path,
            shared,
            listener: Some(handle),
        })
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn log_path(&self) -> &Path {
        &self.log_path
    }

    /// Every `host:port` a request named, in arrival order, repeats included.
    pub fn hosts(&self) -> Vec<String> {
        self.shared.hosts.lock().unwrap().clone()
    }

    pub fn shutdown(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        if let Some(handle) = self.listener.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for Tripwire {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn serve(mut stream: TcpStream, shared: &Shared) {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
    let head = read_head(&mut stream);
    let (kind, method, target) = classify(&head);
    // Recorded before the answer, so a client that has read the 403 finds its request listed.
    let time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64();
    shared.hosts.lock().unwrap().push(target.clone());
    let entry = json!({"time": time, "kind": kind, "method": method, "host": target});
    let _ = writeln!(shared.log.lock().unwrap(), "{entry}");
    let _ = stream.write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
    let _ = stream.flush();
}

fn read_head(stream: &mut TcpStream) -> String {
    let mut head = Vec::new();
    let mut buf = [0u8; 2048];
    while head.len() < MAX_HEAD && !head.windows(4).any(|w| w == b"\r\n\r\n") {
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => head.extend_from_slice(&buf[..n]),
        }
    }
    String::from_utf8_lossy(&head).into_owned()
}

/// The kind of request, its method and the `host:port` it was headed for.
fn classify(head: &str) -> (&'static str, String, String) {
    let mut lines = head.split("\r\n");
    let mut parts = lines.next().unwrap_or("").split_whitespace();
    let (method, target) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    if method.eq_ignore_ascii_case("CONNECT") && !target.is_empty() {
        return ("connect", method.into(), target.to_string());
    }
    for (scheme, port) in [("http://", 80), ("https://", 443)] {
        if let Some(rest) = target
            .get(..scheme.len())
            .filter(|s| s.eq_ignore_ascii_case(scheme))
            .map(|_| &target[scheme.len()..])
        {
            let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
            let authority = authority.rsplit('@').next().unwrap_or("");
            if authority.is_empty() {
                break;
            }
            let host = if has_port(authority) {
                authority.to_string()
            } else {
                format!("{authority}:{port}")
            };
            return ("absolute", method.into(), host);
        }
    }
    let host = lines
        .find_map(|l| {
            let (name, value) = l.split_once(':')?;
            name.trim()
                .eq_ignore_ascii_case("host")
                .then(|| value.trim().to_string())
        })
        .filter(|h| !h.is_empty());
    match host {
        Some(host) => ("origin", method.into(), host),
        None => ("malformed", method.into(), "<malformed>".into()),
    }
}

/// Whether a URL authority already ends in `:port` (an IPv6 literal's own colons do not count).
fn has_port(authority: &str) -> bool {
    match authority.rsplit_once(':') {
        Some((host, port)) => {
            (!host.contains(':') || host.ends_with(']')) && !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit())
        }
        None => false,
    }
}
