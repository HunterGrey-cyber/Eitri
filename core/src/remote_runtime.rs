//! Whether a window switches remote control on, and what it starts: no toolkit here, so the decision is tested without
//! a window.
//!
//! With `remote.enabled` off a window binds nothing and starts nothing. On, it binds its `rv-` socket and starts
//! `eitri remote serve` when no `serve` holds the user's lock: one per user, whichever window comes first, and a window
//! that finds the lock held starts nothing (two windows racing both start one; `serve`'s own lock keeps one and the
//! other exits at once). An address in `remote.listen` is listened on only after the desk said yes to it: until then the
//! window asks ([`RemotePlan::On::ask_listen`]) and starts no `serve`, and after a yes it decides again. `eitri remote
//! stop` ends the running `serve`; a window starts one again only when it starts, since nothing watches for the stop.
//! `serve` ends on its own once no window has had a live `rv-` socket for [`SERVE_IDLE_EXIT`].

use crate::lua::config::{RemoteListen, RemoteSettings};
use crate::remote_pairing::{Endpoint, PairMode, PairScheme, PairingRoutes};
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::Path;
use std::time::Duration;

/// How long `serve` runs on with no window whose `rv-` socket takes a connection.
pub const SERVE_IDLE_EXIT: Duration = Duration::from_secs(60);

/// Whether a `serve` holds the user's lock when a window looks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockState {
    Free,
    Held,
}

/// What a window starts `serve` with: the `remote.*` keys, the port left to `serve` when it is unset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServeConfig {
    pub listen: RemoteListen,
    /// `None`: the port kept for this user, which `serve` settles itself.
    pub port: Option<u16>,
    pub origins: Vec<String>,
    /// `remote.audit.keep_days`.
    pub audit_keep_days: u32,
}

impl ServeConfig {
    pub fn from_settings(settings: &RemoteSettings) -> ServeConfig {
        ServeConfig {
            listen: settings.listen.clone(),
            port: settings.port,
            origins: settings.origins.clone(),
            audit_keep_days: settings.audit_keep_days,
        }
    }

    /// The arguments that follow `eitri remote serve`.
    pub fn args(&self) -> Vec<String> {
        let listen = match &self.listen {
            RemoteListen::Loopback => "loopback".to_owned(),
            RemoteListen::Socket => "socket".to_owned(),
            RemoteListen::Address(addr) => addr.to_string(),
        };
        let mut args = vec!["--listen".to_owned(), listen];
        if let Some(port) = self.port {
            args.extend(["--port".to_owned(), port.to_string()]);
        }
        for origin in &self.origins {
            args.extend(["--origin".to_owned(), origin.clone()]);
        }
        // The default is `serve`'s own, so a window on the default starts the same `serve` a build before the key did.
        if self.audit_keep_days != crate::remote_audit::AUDIT_KEEP_DAYS_DEFAULT {
            args.extend(["--audit-keep-days".to_owned(), self.audit_keep_days.to_string()]);
        }
        args
    }

    /// How `own` (a window's keys) differs from `self` (what the running `serve` holds), one `key: running ..., this
    /// window ...` phrase per key; empty when they agree. An unset port agrees with any: the window would have left it
    /// to `serve`. Origins are compared as `serve` keeps them, lowercase and in any order.
    pub fn differences(&self, own: &ServeConfig) -> Vec<String> {
        let mut out = Vec::new();
        let listen = |cfg: &ServeConfig| cfg.args()[1].clone();
        if self.listen != own.listen {
            out.push(format!("listen: running {}, this window {}", listen(self), listen(own)));
        }
        if let Some(port) = own.port.filter(|port| self.port != Some(*port)) {
            let running = self.port.map_or_else(|| "its own".to_owned(), |p| p.to_string());
            out.push(format!("port: running {running}, this window {port}"));
        }
        let set = |cfg: &ServeConfig| {
            cfg.origins
                .iter()
                .map(|o| o.to_ascii_lowercase())
                .collect::<std::collections::BTreeSet<_>>()
        };
        let (running, ours) = (set(self), set(own));
        if running != ours {
            let list = |set: &std::collections::BTreeSet<String>| {
                if set.is_empty() {
                    "none".to_owned()
                } else {
                    set.iter().cloned().collect::<Vec<_>>().join(" ")
                }
            };
            out.push(format!(
                "origins: running {}, this window {}",
                list(&running),
                list(&ours)
            ));
        }
        if self.audit_keep_days != own.audit_keep_days {
            out.push(format!(
                "remote.audit.keep_days: running {}, this window {}",
                self.audit_keep_days, own.audit_keep_days
            ));
        }
        out
    }
}

/// What a window does about remote control.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemotePlan {
    /// Nothing: no socket, no process, no file.
    Off,
    On {
        /// Bind this window's `rv-` socket.
        bind_rv: bool,
        /// Start `serve` with this configuration.
        start_serve: Option<ServeConfig>,
        /// Ask the desk whether `serve` may listen on this address; nothing is started until it says yes.
        ask_listen: Option<String>,
    },
}

/// What a window with `config` does, given whether a `serve` runs and the address the desk said yes to, if any.
pub fn decide(config: &RemoteSettings, serve_lock: &LockState, confirmed_listen: Option<&str>) -> RemotePlan {
    if !config.enabled {
        return RemotePlan::Off;
    }
    let unconfirmed = match &config.listen {
        RemoteListen::Address(addr) => {
            let confirmed = confirmed_listen.and_then(|text| text.parse::<IpAddr>().ok());
            (confirmed != Some(*addr)).then(|| addr.to_string())
        }
        RemoteListen::Loopback | RemoteListen::Socket => None,
    };
    let free = *serve_lock == LockState::Free;
    RemotePlan::On {
        bind_rv: true,
        start_serve: (free && unconfirmed.is_none()).then(|| ServeConfig::from_settings(config)),
        // With a `serve` already running there is nothing to start on the address now.
        ask_listen: unconfirmed.filter(|_| free),
    }
}

/// Starts what `plan` says to start through `spawner`; whether it started a `serve`.
pub fn start_serve(plan: &RemotePlan, spawner: &dyn Spawner) -> io::Result<bool> {
    match plan {
        RemotePlan::On {
            start_serve: Some(cfg), ..
        } => spawner.spawn_serve(cfg).map(|()| true),
        _ => Ok(false),
    }
}

/// Starts `eitri remote serve`.
pub trait Spawner {
    fn spawn_serve(&self, cfg: &ServeConfig) -> io::Result<()>;
}

/// The ways a pairing code can send phones to a `serve` started with `cfg` on `port` (the port it listens on;
/// `control_dir` holds its Unix socket). A named address is reached directly, over plain HTTP, and has no SSH route:
/// `serve` listens on nothing else. On loopback the SSH recipe reaches the desk's loopback port through the phone's SSH
/// client, and on the Unix socket an SSH forward of the socket, named by its path; and a name in `origins` is the proxy
/// or Tailscale Serve in front of `serve`, reached directly: the first entry that can stand for the desk
/// ([`direct_origin`]), so with several the person puts the one phones use first.
pub fn pairing_routes(cfg: &ServeConfig, port: u16, control_dir: &Path) -> io::Result<PairingRoutes> {
    let origin = || cfg.origins.iter().find_map(|entry| direct_origin(entry));
    Ok(match &cfg.listen {
        RemoteListen::Address(addr) => PairingRoutes {
            ssh: None,
            direct: Some(Endpoint {
                mode: PairMode::Direct,
                scheme: PairScheme::Http,
                host: addr.to_string(),
                port,
            }),
        },
        RemoteListen::Loopback => PairingRoutes {
            ssh: Some(Endpoint {
                mode: PairMode::Ssh,
                scheme: PairScheme::Http,
                host: "127.0.0.1".to_owned(),
                port,
            }),
            direct: origin(),
        },
        RemoteListen::Socket => PairingRoutes {
            ssh: Some(Endpoint {
                mode: PairMode::Ssh,
                scheme: PairScheme::Http,
                host: crate::remote_rv::serve_socket_path(control_dir)?
                    .to_string_lossy()
                    .into_owned(),
                port: 0,
            }),
            direct: origin(),
        },
    })
}

/// Where a phone is sent through one `remote.origins` entry (`name`, `name:port`, `http(s)://name[:port]`, an IPv6
/// host in brackets): the host lowercased and without the brackets, as the pairing URI names a host, the port, and
/// the scheme. Without a port an `http` origin is port 80 and anything else 443, and a bare name is HTTPS: a proxy's
/// or Tailscale Serve's name. `None` for a host that is this desk itself ([`is_desk_itself`]: a phone-side loopback
/// URL could be any app on the phone, so it never stands for the desk), a scheme other than `http` or `https`, a path,
/// user information or anything that is not a host.
pub fn direct_origin(entry: &str) -> Option<Endpoint> {
    let lower = entry.to_ascii_lowercase();
    let (scheme, default_port, authority) = match lower.split_once("://") {
        Some(("https", rest)) => (PairScheme::Https, 443, rest),
        Some(("http", rest)) => (PairScheme::Http, 80, rest),
        Some(_) => return None,
        None => (PairScheme::Https, 443, lower.as_str()),
    };
    if authority.is_empty() || authority.contains(['/', '?', '#', '@', '*']) {
        return None;
    }
    let (host, port) = match authority.strip_prefix('[') {
        Some(rest) => {
            let (inside, after) = rest.split_once(']')?;
            inside.parse::<Ipv6Addr>().ok()?;
            let port = match after {
                "" => None,
                _ => Some(after.strip_prefix(':')?),
            };
            (inside, port)
        }
        None => match authority.split_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (authority, None),
        },
    };
    let port = match port {
        None => default_port,
        Some(digits) => match digits.parse::<u16>() {
            Ok(port) if port != 0 && digits.bytes().all(|b| b.is_ascii_digit()) => port,
            _ => return None,
        },
    };
    let host_ok = !host.is_empty()
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b':'))
        && !host.starts_with('.')
        && !host.contains("..");
    if !host_ok || is_desk_itself(host) {
        return None;
    }
    Some(Endpoint {
        mode: PairMode::Direct,
        scheme,
        host: host.to_owned(),
        port,
    })
}

/// Whether a phone dialling `host` would reach itself or nothing rather than the desk: `localhost` or a name under it,
/// a loopback or unspecified address in any spelling a resolver reads (`127.1`, `0x7f.1`, `2130706433`, `017700000001`,
/// `0`), `::`, `::1`, and an IPv6 address that maps or embeds one of those (`::ffff:127.0.0.1`, `::127.0.0.1`). One
/// trailing dot (the root of a fully qualified name) is read past. A name that only resolves to loopback cannot be
/// told without resolving it; the phone's own refusal of a loopback connection covers that.
fn is_desk_itself(host: &str) -> bool {
    let host = host.strip_suffix('.').unwrap_or(host);
    if host == "localhost" || host.ends_with(".localhost") {
        return true;
    }
    let v4_local = |addr: Ipv4Addr| addr.is_loopback() || addr.octets()[0] == 0;
    if let Ok(addr) = host.parse::<Ipv6Addr>() {
        return addr.is_loopback()
            || addr.is_unspecified()
            || addr.to_ipv4_mapped().is_some_and(v4_local)
            || addr.to_ipv4().is_some_and(v4_local);
    }
    inet_aton(host).is_some_and(v4_local)
}

/// `text` read as an IPv4 address the way `inet_aton(3)`, and so a resolver, reads it: one to four parts separated by
/// dots, each decimal, octal (a leading `0`) or hexadecimal (`0x`), the last part filling every byte the others left.
/// `None` for anything else, a part too large for the bytes it fills among them.
fn inet_aton(text: &str) -> Option<Ipv4Addr> {
    let parts: Vec<&str> = text.split('.').collect();
    if parts.len() > 4 {
        return None;
    }
    let mut values = Vec::with_capacity(parts.len());
    for part in &parts {
        let (digits, radix) = if let Some(hex) = part.strip_prefix("0x").or_else(|| part.strip_prefix("0X")) {
            (hex, 16)
        } else if part.len() > 1 && part.starts_with('0') {
            (&part[1..], 8)
        } else {
            (*part, 10)
        };
        let value = match (digits, radix) {
            // `0x` alone is zero to `inet_aton`.
            ("", 16) => 0,
            ("", _) => return None,
            _ if !digits.chars().all(|c| c.is_digit(radix)) => return None,
            _ => u32::from_str_radix(digits, radix).ok()?,
        };
        values.push(value);
    }
    let (last, leading) = values.split_last()?;
    if leading.iter().any(|&v| v > 0xff) {
        return None;
    }
    let last_bytes = 4 - leading.len() as u32;
    if last_bytes < 4 && *last >= 1u32 << (8 * last_bytes) {
        return None;
    }
    let mut addr = *last;
    for (i, &v) in leading.iter().enumerate() {
        addr |= v << (24 - 8 * i as u32);
    }
    Some(Ipv4Addr::from(addr))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    fn settings(enabled: bool, listen: RemoteListen) -> RemoteSettings {
        RemoteSettings {
            enabled,
            listen,
            port: None,
            origins: Vec::new(),
            audit_keep_days: 90,
        }
    }

    /// The user's lock and the processes started, as a window sees them: a start takes the lock.
    struct World {
        lock: Cell<LockState>,
        started: RefCell<Vec<Vec<String>>>,
    }

    impl World {
        fn new() -> World {
            World {
                lock: Cell::new(LockState::Free),
                started: RefCell::new(Vec::new()),
            }
        }

        /// A window starting with `config`: what it decided, and whether it started a `serve`.
        fn window(&self, config: &RemoteSettings, confirmed: Option<&str>) -> (RemotePlan, bool) {
            let plan = decide(config, &self.lock.get(), confirmed);
            let started = start_serve(&plan, self).unwrap();
            (plan, started)
        }
    }

    impl Spawner for World {
        fn spawn_serve(&self, cfg: &ServeConfig) -> io::Result<()> {
            self.started.borrow_mut().push(cfg.args());
            self.lock.set(LockState::Held);
            Ok(())
        }
    }

    #[test]
    fn disabled_binds_nothing_and_starts_nothing() {
        let world = World::new();
        for listen in [
            RemoteListen::Loopback,
            RemoteListen::Socket,
            RemoteListen::Address("198.51.100.2".parse().unwrap()),
        ] {
            for lock in [LockState::Free, LockState::Held] {
                world.lock.set(lock);
                for confirmed in [None, Some("198.51.100.2")] {
                    let (plan, started) = world.window(&settings(false, listen.clone()), confirmed);
                    assert_eq!(plan, RemotePlan::Off, "{listen:?} {lock:?} {confirmed:?}");
                    assert!(!started);
                }
            }
        }
        assert!(world.started.borrow().is_empty());
        let defaults = RemoteSettings::read(&crate::lua::config::ConfigStore::default()).unwrap();
        assert_eq!(decide(&defaults, &LockState::Free, None), RemotePlan::Off);
    }

    #[test]
    fn enabled_binds_and_starts_one_serve() {
        let world = World::new();
        let config = settings(true, RemoteListen::Loopback);
        let (first, first_started) = world.window(&config, None);
        let (second, second_started) = world.window(&config, None);
        assert!(matches!(first, RemotePlan::On { bind_rv: true, .. }), "{first:?}");
        assert!(
            matches!(
                second,
                RemotePlan::On {
                    bind_rv: true,
                    start_serve: None,
                    ask_listen: None
                }
            ),
            "{second:?}"
        );
        assert!(first_started && !second_started);
        assert_eq!(
            *world.started.borrow(),
            vec![vec!["--listen".to_owned(), "loopback".to_owned()]]
        );
    }

    #[test]
    fn an_unconfirmed_address_is_asked_and_starts_nothing() {
        let world = World::new();
        let config = settings(true, RemoteListen::Address("198.51.100.2".parse().unwrap()));
        for confirmed in [None, Some("198.51.100.3"), Some("not an address")] {
            let (plan, started) = world.window(&config, confirmed);
            assert_eq!(
                plan,
                RemotePlan::On {
                    bind_rv: true,
                    start_serve: None,
                    ask_listen: Some("198.51.100.2".to_owned())
                },
                "{confirmed:?}"
            );
            assert!(!started);
        }
        let (plan, started) = world.window(&config, Some("198.51.100.2"));
        assert!(started, "{plan:?}");
        assert_eq!(world.started.borrow()[0], ["--listen", "198.51.100.2"]);
        // Asked with a `serve` already running: nothing could be started on it now.
        let (plan, _) = world.window(&config, None);
        assert!(
            matches!(
                plan,
                RemotePlan::On {
                    ask_listen: None,
                    start_serve: None,
                    ..
                }
            ),
            "{plan:?}"
        );
    }

    #[test]
    fn serve_is_given_every_key() {
        let cfg = ServeConfig {
            listen: RemoteListen::Socket,
            port: Some(7400),
            origins: vec!["desk.tail1.ts.net".to_owned(), "https://phone.example.org".to_owned()],
            audit_keep_days: 30,
        };
        assert_eq!(
            cfg.args(),
            [
                "--listen",
                "socket",
                "--port",
                "7400",
                "--origin",
                "desk.tail1.ts.net",
                "--origin",
                "https://phone.example.org",
                "--audit-keep-days",
                "30"
            ]
        );
    }

    #[test]
    fn differences_names_keep_days() {
        let running = ServeConfig {
            listen: RemoteListen::Loopback,
            port: Some(41000),
            origins: Vec::new(),
            audit_keep_days: 90,
        };
        let mut own = running.clone();
        assert!(running.differences(&own).is_empty());
        assert!(
            !own.args().iter().any(|a| a == "--audit-keep-days"),
            "the default is left to serve: {:?}",
            own.args()
        );
        own.audit_keep_days = 7;
        assert_eq!(
            running.differences(&own),
            ["remote.audit.keep_days: running 90, this window 7"]
        );
        let settings = RemoteSettings {
            audit_keep_days: 7,
            ..settings(true, RemoteListen::Loopback)
        };
        assert_eq!(ServeConfig::from_settings(&settings).audit_keep_days, 7);
    }

    #[test]
    fn a_running_serve_with_other_keys_is_named_key_by_key() {
        let running = ServeConfig {
            listen: RemoteListen::Loopback,
            port: Some(41000),
            origins: vec!["desk.tail1.ts.net".to_owned(), "b.example".to_owned()],
            audit_keep_days: 90,
        };
        let mut own = ServeConfig {
            listen: RemoteListen::Loopback,
            port: None,
            origins: vec!["B.example".to_owned(), "desk.tail1.ts.net".to_owned()],
            audit_keep_days: 90,
        };
        assert!(
            running.differences(&own).is_empty(),
            "case, order and an unset port agree"
        );
        own.port = Some(41000);
        assert!(running.differences(&own).is_empty());
        own = ServeConfig {
            listen: RemoteListen::Address("198.51.100.2".parse().unwrap()),
            port: Some(7341),
            origins: Vec::new(),
            audit_keep_days: 90,
        };
        assert_eq!(
            running.differences(&own),
            [
                "listen: running loopback, this window 198.51.100.2",
                "port: running 41000, this window 7341",
                "origins: running b.example desk.tail1.ts.net, this window none",
            ]
        );
    }

    /// The routes for `listen` and `origins` on port 41000, each as (mode, scheme, host, port).
    fn routes(listen: RemoteListen, origins: &[&str]) -> (Option<Route>, Option<Route>) {
        let cfg = ServeConfig {
            listen,
            port: None,
            origins: origins.iter().map(|o| (*o).to_owned()).collect(),
            audit_keep_days: 90,
        };
        let r = pairing_routes(&cfg, 41000, Path::new("/run/user/1000/eitri")).unwrap();
        (r.ssh.map(route), r.direct.map(route))
    }

    type Route = (PairMode, PairScheme, String, u16);

    fn route(e: Endpoint) -> Route {
        (e.mode, e.scheme, e.host, e.port)
    }

    fn direct(scheme: PairScheme, host: &str, port: u16) -> Option<Route> {
        Some((PairMode::Direct, scheme, host.to_owned(), port))
    }

    #[test]
    fn pairing_follows_the_listener() {
        let loopback_ssh = Some((PairMode::Ssh, PairScheme::Http, "127.0.0.1".to_owned(), 41000));
        assert_eq!(routes(RemoteListen::Loopback, &[]), (loopback_ssh, None));
        assert_eq!(
            routes(RemoteListen::Address("198.51.100.2".parse().unwrap()), &[]),
            (None, direct(PairScheme::Http, "198.51.100.2", 41000)),
            "a named address has no SSH route: serve listens on nothing else"
        );
        let socket = crate::remote_rv::serve_socket_path(Path::new("/run/user/1000/eitri")).unwrap();
        assert_eq!(
            routes(RemoteListen::Socket, &[]),
            (
                Some((
                    PairMode::Ssh,
                    PairScheme::Http,
                    socket.to_string_lossy().into_owned(),
                    0
                )),
                None
            )
        );
    }

    #[test]
    fn a_proxy_or_tailscale_name_in_origins_adds_the_direct_route_and_keeps_ssh() {
        let https = |host, port| direct(PairScheme::Https, host, port);
        // Tailscale Serve and an own proxy are HTTPS names in front of a loopback `serve`; the SSH route stays.
        let (ssh, d) = routes(RemoteListen::Loopback, &["desk.tail1.ts.net"]);
        assert_eq!(ssh.unwrap().0, PairMode::Ssh);
        assert_eq!(d, https("desk.tail1.ts.net", 443));
        assert_eq!(
            routes(RemoteListen::Loopback, &["https://Phone.Example.org:8443"]).1,
            https("phone.example.org", 8443)
        );
        assert_eq!(
            routes(RemoteListen::Loopback, &["http://198.51.100.7"]).1,
            direct(PairScheme::Http, "198.51.100.7", 80)
        );
        assert_eq!(
            routes(RemoteListen::Loopback, &["http://desk.example:8080"]).1,
            direct(PairScheme::Http, "desk.example", 8080)
        );
        let (ssh, d) = routes(RemoteListen::Socket, &["desk.tail1.ts.net"]);
        assert_eq!(ssh.unwrap().0, PairMode::Ssh);
        assert_eq!(d, https("desk.tail1.ts.net", 443));
        assert_eq!(
            routes(RemoteListen::Loopback, &["[fd00::2]:8443"]).1,
            https("fd00::2", 8443)
        );
        // Several: the first that can stand for the desk; a loopback name never does.
        assert_eq!(
            routes(
                RemoteListen::Loopback,
                &["localhost:8765", "127.0.0.1", "a.example", "b.example"]
            )
            .1,
            https("a.example", 443)
        );
        assert_eq!(
            routes(
                RemoteListen::Loopback,
                &["ftp://a.example", "a.example/eitri", "b.example"]
            )
            .1,
            https("b.example", 443)
        );
        // Loopback or the socket with no usable name has the SSH route only.
        assert_eq!(
            routes(RemoteListen::Loopback, &["localhost", "x.localhost:9000", "[::1]:8765"]).1,
            None
        );
        // A named address is what phones reach, whatever names a proxy also has.
        assert_eq!(
            routes(
                RemoteListen::Address("198.51.100.2".parse().unwrap()),
                &["desk.tail1.ts.net"]
            ),
            (None, direct(PairScheme::Http, "198.51.100.2", 41000))
        );
    }

    #[test]
    fn an_origin_that_is_the_desk_itself_in_any_spelling_is_no_phone_target() {
        for entry in [
            "localhost",
            "localhost.",
            "a.localhost",
            "127.0.0.1",
            "127.0.0.1.",
            "127.1",
            "127.0.1",
            "127.255.255.254",
            "0x7f.1",
            "0X7F.0.0.1",
            "0x7f000001",
            "2130706433",
            "017700000001",
            "0177.0.0.1",
            "0",
            "0.0.0.0",
            "0x0",
            "00",
            "0.1.2.3",
            "https://127.1:8443",
            "http://2130706433",
            "[::1]",
            "[::]",
            "[::ffff:127.0.0.1]",
            "[::ffff:7f00:1]",
            "[::ffff:0.0.0.0]",
            "[::127.0.0.1]",
            "[0:0:0:0:0:0:0:1]:8443",
        ] {
            assert_eq!(direct_origin(entry), None, "{entry}");
        }
    }

    #[test]
    fn an_origin_that_only_looks_numeric_is_a_name() {
        for (entry, host) in [
            ("198.51.100.7", "198.51.100.7"),
            ("0xc6.51.100.7", "0xc6.51.100.7"),
            ("1e100.net", "1e100.net"),
            ("128.0.0.1", "128.0.0.1"),
            ("deadbeef", "deadbeef"),
            ("0x7f.example", "0x7f.example"),
            ("127.0.0.1.example", "127.0.0.1.example"),
            ("[fd00::127]", "fd00::127"),
            ("[::ffff:198.51.100.2]", "::ffff:198.51.100.2"),
        ] {
            assert_eq!(direct_origin(entry).map(|e| e.host), Some(host.to_owned()), "{entry}");
        }
    }

    #[test]
    fn inet_aton_reads_every_ipv4_spelling() {
        let of = |text| inet_aton(text).map(|a| a.to_string());
        for (text, addr) in [
            ("127.1", "127.0.0.1"),
            ("127.0.1", "127.0.0.1"),
            ("0x7f.1", "127.0.0.1"),
            ("2130706433", "127.0.0.1"),
            ("017700000001", "127.0.0.1"),
            ("0177.0.0.01", "127.0.0.1"),
            ("1.2.3.4", "1.2.3.4"),
            ("198.65535", "198.0.255.255"),
            ("4294967295", "255.255.255.255"),
            ("0x", "0.0.0.0"),
        ] {
            assert_eq!(of(text).as_deref(), Some(addr), "{text}");
        }
        for text in [
            "",
            "1.2.3.4.5",
            "256.1",
            "1.2.65536",
            "4294967296",
            "08",
            "0x1g",
            "1..2",
            "a.b",
            "1.2.3.-4",
        ] {
            assert_eq!(of(text), None, "{text}");
        }
    }

    #[test]
    fn a_route_is_picked_as_asked_or_by_default() {
        let both = PairingRoutes {
            ssh: Some(Endpoint {
                mode: PairMode::Ssh,
                scheme: PairScheme::Http,
                host: "127.0.0.1".into(),
                port: 41000,
            }),
            direct: direct_origin("desk.tail1.ts.net"),
        };
        assert_eq!(
            both.pick(None).unwrap().mode,
            PairMode::Direct,
            "the name set up for phones first"
        );
        assert_eq!(both.pick(Some(PairMode::Ssh)).unwrap().mode, PairMode::Ssh);
        assert_eq!(both.pick(Some(PairMode::Direct)).unwrap().host, "desk.tail1.ts.net");
        let ssh_only = PairingRoutes {
            direct: None,
            ..both.clone()
        };
        assert_eq!(ssh_only.pick(None).unwrap().mode, PairMode::Ssh);
        assert_eq!(
            ssh_only.pick(Some(PairMode::Direct)),
            Err(crate::remote_pairing::NO_DIRECT_ROUTE)
        );
        let direct_only = PairingRoutes { ssh: None, ..both };
        assert_eq!(
            direct_only.pick(Some(PairMode::Ssh)),
            Err(crate::remote_pairing::NO_SSH_ROUTE)
        );
    }
}
