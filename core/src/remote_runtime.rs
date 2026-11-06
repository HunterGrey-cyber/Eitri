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
use crate::remote_pairing::{Endpoint, PairMode};
use std::io;
use std::net::IpAddr;
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
}

impl ServeConfig {
    pub fn from_settings(settings: &RemoteSettings) -> ServeConfig {
        ServeConfig {
            listen: settings.listen.clone(),
            port: settings.port,
            origins: settings.origins.clone(),
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
        args
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

/// Where a pairing code sends phones for a `serve` started with `cfg` on `port` (the port it listens on; `control_dir`
/// holds its Unix socket). Loopback is reached through the phone's SSH client, to the desk's loopback port; a named
/// address directly; the Unix socket through an SSH forward of the socket, named by its path.
pub fn pairing_endpoint(cfg: &ServeConfig, port: u16, control_dir: &Path) -> io::Result<Endpoint> {
    Ok(match &cfg.listen {
        RemoteListen::Loopback => Endpoint {
            mode: PairMode::Ssh,
            host: "127.0.0.1".to_owned(),
            port,
        },
        RemoteListen::Address(addr) => Endpoint {
            mode: PairMode::Direct,
            host: addr.to_string(),
            port,
        },
        RemoteListen::Socket => Endpoint {
            mode: PairMode::Ssh,
            host: crate::remote_rv::serve_socket_path(control_dir)?
                .to_string_lossy()
                .into_owned(),
            port: 0,
        },
    })
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
                "https://phone.example.org"
            ]
        );
    }

    #[test]
    fn pairing_follows_the_listener() {
        let control = Path::new("/run/user/1000/eitri");
        let of = |listen| {
            let cfg = ServeConfig {
                listen,
                port: None,
                origins: Vec::new(),
            };
            let e = pairing_endpoint(&cfg, 41000, control).unwrap();
            (e.mode, e.host, e.port)
        };
        assert_eq!(
            of(RemoteListen::Loopback),
            (PairMode::Ssh, "127.0.0.1".to_owned(), 41000)
        );
        assert_eq!(
            of(RemoteListen::Address("198.51.100.2".parse().unwrap())),
            (PairMode::Direct, "198.51.100.2".to_owned(), 41000)
        );
        let socket = crate::remote_rv::serve_socket_path(control).unwrap();
        assert_eq!(
            of(RemoteListen::Socket),
            (PairMode::Ssh, socket.to_string_lossy().into_owned(), 0)
        );
    }
}
