//! `eitri remote serve`, `stop` and `status`: the one server per user that phones connect to.
//!
//! `serve` reads its configuration from its command line ([`config`]), takes the user's lock (`remote.lock` in the
//! control directory, so one `serve` runs per user), binds what it was told to ([`listen`]), confines itself
//! ([`sandbox`]) before any thread starts, records what runs in `remote-serve.json` beside the lock (emptied as soon as
//! the lock is taken, written through the open handle after the sandbox), and then accepts. A second `serve` finds the lock held, prints
//! how its configuration differs from the running one's, and exits: 3 when they agree, 4 when they differ, so the
//! window that started it can show the difference at the desk. A configuration refused is exit 2, a failure to start
//! (the sandbox's included) exit 1.
//!
//! Each connection is answered on its own thread ([`ws`]); at most [`MAX_CONNECTIONS`] at once. After its 101 a
//! connection is a phone's session ([`session`]): its `hello` checked against the device store, its frames routed to
//! the window that owns each tab ([`route`]), what it is owed queued with a bound ([`outbound`]); the store is watched
//! so a revoked phone is cut off ([`revoke`]), and every window is told who is connected ([`report`]).

#[cfg(target_os = "linux")]
pub mod config;
#[cfg(target_os = "linux")]
pub mod listen;
#[cfg(target_os = "linux")]
pub mod origin;
#[cfg(target_os = "linux")]
pub mod outbound;
#[cfg(target_os = "linux")]
pub mod port;
#[cfg(target_os = "linux")]
pub mod report;
#[cfg(target_os = "linux")]
pub mod revoke;
#[cfg(target_os = "linux")]
pub mod route;
#[cfg(target_os = "linux")]
pub mod sandbox;
#[cfg(target_os = "linux")]
pub mod session;
#[cfg(target_os = "linux")]
pub mod ws;

#[cfg(target_os = "linux")]
pub use linux::*;

/// `eitri remote serve`: not on this platform.
#[cfg(not(target_os = "linux"))]
pub fn serve(_args: &[String]) -> i32 {
    eprintln!("eitri remote serve: Linux only");
    2
}

/// `eitri remote stop`: not on this platform.
#[cfg(not(target_os = "linux"))]
pub fn stop(_args: &[String]) -> i32 {
    eprintln!("eitri remote stop: Linux only");
    2
}

/// `eitri remote status`: not on this platform.
#[cfg(not(target_os = "linux"))]
pub fn status(_args: &[String]) -> i32 {
    eprintln!("eitri remote status: Linux only");
    2
}

#[cfg(target_os = "linux")]
mod linux {
    use std::io::{self, Write};
    use std::net::TcpListener;
    use std::os::unix::net::UnixListener;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use agent::private_fs::PrivateLock;

    use super::config::{Record, ServeConfig, RECORD_V};
    use super::listen::{bind, iface_of, peer_in, Listeners};
    use super::sandbox::{sandbox, SandboxState};
    use super::session::{Hub, Limits};
    use super::ws::{handshake, Conn};

    pub use super::config::Listen;
    pub use super::origin::check_request;
    pub use super::outbound::{PHONE_OUT_CAP_BYTES, PHONE_WRITE_STALL};
    pub use super::port::stable_port;
    pub use super::route::{route, Route, RowsIndex};
    pub use super::session::PhoneSession;
    pub use super::ws::WS_PATH;

    /// Connections answered at once; one more is closed at accept.
    pub const MAX_CONNECTIONS: usize = 32;
    /// The device store, in the state directory.
    pub const DEVICES_FILE: &str = "devices.json";
    /// The lock that makes `serve` one per user, in the control directory.
    pub const LOCK_FILE: &str = "remote.lock";
    /// What the running `serve` records of itself, beside the lock.
    pub const RECORD_FILE: &str = "remote-serve.json";

    /// `serve` could not start: an I/O failure or the sandbox.
    pub const EXIT_FAILED: i32 = 1;
    /// `serve`'s configuration was refused (an argument, a wildcard, an address the desk has not confirmed).
    pub const EXIT_REFUSED: i32 = 2;
    /// A `serve` with the same configuration already runs.
    pub const EXIT_RUNNING_SAME: i32 = 3;
    /// A `serve` with another configuration already runs; stderr names the difference.
    pub const EXIT_RUNNING_DIFFERENT: i32 = 4;
    /// `status`'s exit code when no `serve` runs.
    pub const EXIT_NOT_RUNNING: i32 = 3;

    /// The directories `serve` works in, from the environment.
    #[derive(Debug, Clone)]
    pub struct Dirs {
        /// `<runtime dir>/eitri`: the control sockets, the agents directory, the lock and the record.
        pub control: PathBuf,
        /// `<state home>/eitri/remote`: the device store and the port.
        pub state: PathBuf,
    }

    impl Dirs {
        pub fn from_env() -> Result<Dirs, String> {
            let control = eitri_core::panel_control::control_dir(
                std::env::var_os("XDG_RUNTIME_DIR").as_deref(),
                &std::env::temp_dir(),
            );
            let state = eitri_core::state_paths::state_subdir(
                std::env::var_os("XDG_STATE_HOME").as_deref(),
                std::env::var_os("HOME").as_deref(),
                "remote",
            )
            .ok_or("neither XDG_STATE_HOME nor HOME names an absolute directory")?;
            Ok(Dirs { control, state })
        }

        pub fn agents(&self) -> PathBuf {
            eitri_core::agents::agents_dir(&self.control)
        }
    }

    /// The address the desk confirmed for listening. Nothing keeps the desk's answer in this build, so an address
    /// listener is refused until it does.
    fn confirmed_listen(_state: &Path) -> Option<String> {
        None
    }

    /// `eitri remote serve [--listen ...] [--port ...] [--origin ...]`. Returns only when it could not start, or another
    /// `serve` runs.
    pub fn serve(args: &[String]) -> i32 {
        let dirs = match Dirs::from_env() {
            Ok(dirs) => dirs,
            Err(e) => {
                eprintln!("eitri remote serve: {e}");
                return EXIT_FAILED;
            }
        };
        let Err((code, line)) = run_serve(&dirs, args, confirmed_listen);
        eprintln!("eitri remote serve: {line}");
        code
    }

    enum Never {}

    fn run_serve(dirs: &Dirs, args: &[String], confirmed: fn(&Path) -> Option<String>) -> Result<Never, (i32, String)> {
        let failed = |e: String| (EXIT_FAILED, e);
        eitri_core::panel_control::ensure_dir(&dirs.control).map_err(failed)?;
        agent::private_fs::create_private_dir_all(&dirs.agents(), &dirs.control)
            .map_err(|e| failed(format!("could not make {}: {e}", dirs.agents().display())))?;
        eitri_core::state_paths::create_state_dir(&dirs.state)
            .map_err(|e| failed(format!("could not make {}: {e}", dirs.state.display())))?;
        let cfg = ServeConfig::from_args(args, || super::port::stable_port(&dirs.state), iface_of)
            .map_err(|e| (EXIT_REFUSED, e))?;

        let lock = PrivateLock::open(&dirs.control.join(LOCK_FILE))
            .map_err(|e| failed(format!("could not open the lock: {e}")))?;
        if !take_lock(&lock).map_err(|e| failed(format!("could not take the lock: {e}")))? {
            return Err(already_running(dirs, &cfg));
        }
        // Emptied before anything else: what a previous `serve` left there names a process that may no longer be it.
        let mut record_file = agent::private_fs::open_private(&dirs.control.join(RECORD_FILE))
            .map_err(|e| failed(format!("could not open the record: {e}")))?;

        let listeners = bind(&cfg, &dirs.control, confirmed(&dirs.state).as_deref()).map_err(|e| (EXIT_REFUSED, e))?;

        let state = sandbox(&dirs.agents(), &dirs.control, &dirs.state)
            .map_err(|e| failed(format!("could not confine itself: {e}")))?;
        if state == SandboxState::NoLandlock {
            eprintln!("[remote] this kernel has no Landlock: serving with seccomp and not dumpable only");
        }
        let record = Record {
            v: RECORD_V,
            pid: std::process::id(),
            args: cfg.to_args(),
            sandbox: state.word().to_owned(),
        };
        let mut line = serde_json::to_vec(&record).map_err(|e| failed(e.to_string()))?;
        line.push(b'\n');
        record_file
            .write_all(&line)
            .map_err(|e| failed(format!("could not write the record: {e}")))?;
        drop(record_file);

        let hub = Hub::start(&dirs.control, &dirs.state.join(DEVICES_FILE), Limits::default())
            .map_err(|e| failed(format!("could not start serving: {e}")))?;
        accept_all(listeners, Arc::new(cfg), hub);
        // The accepting threads never end, and the lock is held for as long as the process runs.
        let _held = lock;
        loop {
            std::thread::park();
        }
    }

    /// How long a held lock is asked again before a running `serve` is believed: `status` and `stop` take it for a
    /// moment to see whether it is held.
    const LOCK_PATIENCE: Duration = Duration::from_millis(300);

    fn take_lock(lock: &PrivateLock) -> io::Result<bool> {
        let until = Instant::now() + LOCK_PATIENCE;
        loop {
            if lock.try_lock()? {
                return Ok(true);
            }
            if Instant::now() >= until {
                return Ok(false);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// What a second `serve` says: the running one's pid and how this one's configuration differs from it.
    fn already_running(dirs: &Dirs, asked: &ServeConfig) -> (i32, String) {
        let Some((record, running)) = read_record(dirs) else {
            return (
                EXIT_RUNNING_SAME,
                "already running (still starting: its record is not written yet)".to_owned(),
            );
        };
        let differences = running.differences(asked);
        if differences.is_empty() {
            (EXIT_RUNNING_SAME, format!("already running (pid {})", record.pid))
        } else {
            (
                EXIT_RUNNING_DIFFERENT,
                format!(
                    "already running (pid {}) with another configuration; {}",
                    record.pid,
                    differences.join("; ")
                ),
            )
        }
    }

    /// The running `serve`'s record and the configuration it holds, when the record is whole.
    pub fn read_record(dirs: &Dirs) -> Option<(Record, ServeConfig)> {
        let text = agent::private_fs::read_private_nofollow_capped(&dirs.control.join(RECORD_FILE), 64 * 1024).ok()?;
        let record: Record = serde_json::from_str(text.trim_end()).ok()?;
        if record.v != RECORD_V {
            return None;
        }
        // The interface was looked up when it started; an address this machine no longer has keeps an empty name.
        let cfg = ServeConfig::from_args(&record.args, || Ok(0), |addr| iface_of(addr).or(Some(String::new()))).ok()?;
        Some((record, cfg))
    }

    /// Whether a `serve` holds the lock now.
    pub fn is_running(dirs: &Dirs) -> io::Result<bool> {
        let path = dirs.control.join(LOCK_FILE);
        match std::fs::symlink_metadata(&path) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(e),
            Ok(_) => {}
        }
        Ok(!PrivateLock::open(&path)?.try_lock()?)
    }

    fn accept_all(listeners: Listeners, cfg: Arc<ServeConfig>, hub: Arc<Hub>) {
        let open = Arc::new(AtomicUsize::new(0));
        let ranges = Arc::new(listeners.peer_ranges);
        for listener in listeners.tcp {
            let (cfg, open, ranges, hub) = (cfg.clone(), open.clone(), ranges.clone(), hub.clone());
            std::thread::spawn(move || accept_tcp(listener, cfg, open, ranges, hub));
        }
        if let Some((listener, _path)) = listeners.unix {
            std::thread::spawn(move || accept_unix(listener, cfg, open, hub));
        }
    }

    /// Accepts phones on `listener` on a thread of its own, each served through `hub`: what `serve --listen socket`
    /// runs after its sandbox, and what a test runs in its own process.
    pub fn serve_unix(listener: UnixListener, cfg: Arc<ServeConfig>, hub: Arc<Hub>) -> io::Result<()> {
        let open = Arc::new(AtomicUsize::new(0));
        std::thread::Builder::new()
            .name("eitri-remote-accept".into())
            .spawn(move || accept_unix(listener, cfg, open, hub))?;
        Ok(())
    }

    /// Counts a connection while it lives.
    struct Slot(Arc<AtomicUsize>);

    impl Slot {
        fn take(open: &Arc<AtomicUsize>) -> Option<Slot> {
            if open.fetch_add(1, Ordering::AcqRel) >= MAX_CONNECTIONS {
                open.fetch_sub(1, Ordering::AcqRel);
                return None;
            }
            Some(Slot(open.clone()))
        }
    }

    impl Drop for Slot {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::AcqRel);
        }
    }

    fn accept_tcp(
        listener: TcpListener,
        cfg: Arc<ServeConfig>,
        open: Arc<AtomicUsize>,
        ranges: Arc<Option<Vec<(std::net::IpAddr, u8)>>>,
        hub: Arc<Hub>,
    ) {
        for stream in listener.incoming() {
            let Ok(stream) = stream else {
                pause_after_accept_error();
                continue;
            };
            if let Some(networks) = ranges.as_ref() {
                if !stream.peer_addr().is_ok_and(|peer| peer_in(peer.ip(), networks)) {
                    continue;
                }
            }
            let Some(slot) = Slot::take(&open) else { continue };
            let (cfg, hub) = (cfg.clone(), hub.clone());
            let peer = stream
                .peer_addr()
                .map_or_else(|_| "unknown".to_owned(), |peer| peer.to_string());
            std::thread::spawn(move || {
                let _slot = slot;
                answer(Box::new(stream), &cfg, &hub, peer);
            });
        }
    }

    fn accept_unix(listener: UnixListener, cfg: Arc<ServeConfig>, open: Arc<AtomicUsize>, hub: Arc<Hub>) {
        for stream in listener.incoming() {
            let Ok(stream) = stream else {
                pause_after_accept_error();
                continue;
            };
            // This user's processes only: an SSH forward to the socket arrives as this user.
            if eitri_core::panel_control::peer_uid(&stream).ok() != Some(agent::private_fs::euid()) {
                continue;
            }
            let Some(slot) = Slot::take(&open) else { continue };
            let (cfg, hub) = (cfg.clone(), hub.clone());
            std::thread::spawn(move || {
                let _slot = slot;
                // An SSH forward or a local client: the address says nothing more.
                answer(Box::new(stream), &cfg, &hub, "unix".to_owned());
            });
        }
    }

    /// An accept that failed (out of descriptors, say) fails again at once until something is let go: a pause keeps
    /// the accepting thread from spinning meanwhile.
    fn pause_after_accept_error() {
        std::thread::sleep(Duration::from_millis(50));
    }

    fn answer(mut stream: Box<dyn Conn>, cfg: &ServeConfig, hub: &Arc<Hub>, peer: String) {
        if let Ok(Some(host)) = handshake(stream.as_mut(), cfg) {
            hub.serve_phone(stream, host, peer);
        }
    }

    /// `eitri remote status`: whether `serve` runs, where it listens and how it is confined. Exit 0 when it runs,
    /// [`EXIT_NOT_RUNNING`] when it does not.
    pub fn status(args: &[String]) -> i32 {
        if !args.is_empty() {
            eprintln!("usage: eitri remote status");
            return EXIT_REFUSED;
        }
        let dirs = match Dirs::from_env() {
            Ok(dirs) => dirs,
            Err(e) => {
                eprintln!("eitri remote status: {e}");
                return EXIT_FAILED;
            }
        };
        match is_running(&dirs) {
            Ok(true) => {}
            Ok(false) => {
                println!("not running");
                return EXIT_NOT_RUNNING;
            }
            Err(e) => {
                eprintln!("eitri remote status: {e}");
                return EXIT_FAILED;
            }
        }
        let mut out = io::stdout().lock();
        match read_record(&dirs) {
            Some((record, cfg)) => {
                let origins = if cfg.origins.is_empty() {
                    "none".to_owned()
                } else {
                    cfg.origins.join(" ")
                };
                let _ = writeln!(out, "running: pid {}", record.pid);
                let _ = writeln!(out, "listen: {}", cfg.listen_text());
                let _ = writeln!(out, "port: {}", cfg.port);
                let _ = writeln!(out, "origins: {origins}");
                let _ = writeln!(out, "sandbox: {}", record.sandbox);
            }
            None => {
                let _ = writeln!(out, "running: starting");
            }
        }
        0
    }

    /// `eitri remote stop`: ends the running `serve`, and every phone's connection with it, then waits up to 5 s for
    /// its lock to be let go. A window starts it again at its next start while remote control is on.
    pub fn stop(args: &[String]) -> i32 {
        if !args.is_empty() {
            eprintln!("usage: eitri remote stop");
            return EXIT_REFUSED;
        }
        let dirs = match Dirs::from_env() {
            Ok(dirs) => dirs,
            Err(e) => {
                eprintln!("eitri remote stop: {e}");
                return EXIT_FAILED;
            }
        };
        if !is_running(&dirs).unwrap_or(false) {
            println!("not running");
            return 0;
        }
        let Some((record, _)) = read_record(&dirs) else {
            eprintln!("eitri remote stop: it is still starting; try again");
            return EXIT_FAILED;
        };
        // The holder empties the record as soon as it takes the lock and writes it only while holding it, and the
        // lock goes with the process, so a pid read while it is held names the holder (unless the holder ended
        // between the two looks and its pid was taken again in that moment).
        // SAFETY: `kill` takes no pointers.
        if unsafe { libc::kill(record.pid as libc::pid_t, libc::SIGTERM) } != 0 {
            eprintln!("eitri remote stop: {}", io::Error::last_os_error());
            return EXIT_FAILED;
        }
        let until = Instant::now() + Duration::from_secs(5);
        while Instant::now() < until {
            if !is_running(&dirs).unwrap_or(true) {
                println!("stopped");
                return 0;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        eprintln!("eitri remote stop: pid {} still holds the lock after 5 s", record.pid);
        EXIT_FAILED
    }
    #[cfg(test)]
    mod tests {
        use super::*;
        use std::os::unix::fs::PermissionsExt;
        use std::sync::Mutex;

        fn scratch(name: &str) -> PathBuf {
            let dir = PathBuf::from(
                option_env!("CARGO_TARGET_DIR").unwrap_or(concat!(env!("CARGO_MANIFEST_DIR"), "/../target")),
            )
            .join("tmp/remote-serve-tests")
            .join(format!("{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
            dir
        }

        /// The record as `serve` left it when it came to bind, read beside the state directory it was given.
        static RECORD_AT_BIND: Mutex<Option<String>> = Mutex::new(None);

        fn record_at_bind(state: &Path) -> Option<String> {
            let record = state.parent().unwrap().join("control").join(RECORD_FILE);
            *RECORD_AT_BIND.lock().unwrap() = Some(std::fs::read_to_string(record).unwrap_or_default());
            None
        }

        #[test]
        fn a_previous_record_is_emptied_once_the_lock_is_taken() {
            let dir = scratch("record");
            let dirs = Dirs {
                control: dir.join("control"),
                state: dir.join("state"),
            };
            eitri_core::panel_control::ensure_dir(&dirs.control).unwrap();
            let stale = r#"{"v":1,"pid":1,"args":["--listen","loopback"],"sandbox":"full"}"#;
            std::fs::write(dirs.control.join(RECORD_FILE), format!("{stale}\n")).unwrap();
            let args = ["--listen".to_owned(), "socket".to_owned()];
            // The sandbox refuses beside libtest's threads, so this start ends there, after binding.
            let (code, line) = match run_serve(&dirs, &args, record_at_bind) {
                Ok(never) => match never {},
                Err(refused) => refused,
            };
            assert_eq!(code, EXIT_FAILED, "{line}");
            assert!(line.contains("could not confine itself"), "{line}");
            assert_eq!(
                RECORD_AT_BIND.lock().unwrap().as_deref(),
                Some(""),
                "the previous serve's pid was still recorded while this one held the lock"
            );
        }

        #[test]
        fn a_lock_held_for_a_moment_is_taken() {
            let path = scratch("moment").join(LOCK_FILE);
            let probe = PrivateLock::open(&path).unwrap();
            assert!(probe.try_lock().unwrap());
            let release = std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(60));
                drop(probe);
            });
            let lock = PrivateLock::open(&path).unwrap();
            assert!(
                take_lock(&lock).unwrap(),
                "a lock held only for a look was taken as a running serve"
            );
            release.join().unwrap();
        }

        #[test]
        fn a_lock_held_on_is_not_taken() {
            let path = scratch("held").join(LOCK_FILE);
            let holder = PrivateLock::open(&path).unwrap();
            assert!(holder.try_lock().unwrap());
            let lock = PrivateLock::open(&path).unwrap();
            assert!(!take_lock(&lock).unwrap());
            drop(holder);
        }
    }
}
