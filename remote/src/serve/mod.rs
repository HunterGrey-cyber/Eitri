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
//!
//! Whatever it listens on for phones, it also binds its control socket ([`control`]), through which `eitri remote
//! add-device` and `grant` reach a window.
//!
//! A window with `remote.enabled` starts it ([`spawn`]); it ends with exit 0 once no window has had a live `rv-` socket
//! for [`SERVE_IDLE_EXIT`], and `eitri remote stop` ends it at once.
//!
//! **Its records.** `serve` opens the audit log (`<state>/eitri/remote/audit/`, `remote.audit.keep_days` of rotated
//! files kept, pruned at the open and at each rotation) before its sandbox, which then lets it write beneath that
//! directory and nowhere else, and writes it through one handle its threads share ([`session::Audit`]). It records
//! `enable` once it listens and is confined, and `disable` when it ends: at the idle exit, on `SIGTERM` (`eitri remote
//! stop`) or `SIGINT`, which a thread of its own waits for and then ends the process by the same signal, and when it
//! fails after `enable`. It records the refusals it makes before any `hello` (`host`, `origin`, `path_prefix`,
//! `peer_outside_interface`, with the listener and peer and no device) and, through its sessions, the rest.
//!
//! **Another build.** The record names its build; a reader from another build (an Eitri window or `eitri remote
//! status` after a package upgrade) sees [`Recorded::OtherBuild`] and says to restart the windows, rather than taking
//! the record for one still being written.

#[cfg(target_os = "linux")]
pub mod config;
#[cfg(target_os = "linux")]
pub mod control;
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
pub mod spawn;
#[cfg(target_os = "linux")]
pub mod ws;

#[cfg(test)]
mod phone_fixtures;

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
    pub use eitri_core::remote_runtime::SERVE_IDLE_EXIT;

    use eitri_core::remote_audit::{AuditLog, Event, JournalTarget, Listener, Reason, WriterKind};

    use super::config::{this_build, Record, ServeConfig, RECORD_V};
    use super::listen::{audit_listener, bind, iface_of, peer_in, Listeners};
    use super::sandbox::sandbox;
    use super::session::{Arrival, Audit, Hub, Limits};
    use super::ws::{handshake, Conn, Handshake};

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
    /// When `eitri remote stop` last ran, beside the lock: a window that found `serve` running and looks again later
    /// starts none after a stop.
    pub const STOPPED_FILE: &str = "remote-stopped";

    /// How long `stop` waits for the signalled server to release its lock.
    pub const STOP_WAIT: Duration = Duration::from_secs(5);
    /// A window's look-back for a stop already in flight: the note precedes the lock check, record read and signal,
    /// so allow a second for those steps as well as the wait for serve to release its lock.
    pub const STOP_NOTE_REACH: Duration = STOP_WAIT.saturating_add(Duration::from_secs(1));

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

    /// The variable a test build (`remote-test-hooks`) reads for `serve`'s idle deadline in milliseconds; a release
    /// build has no such feature and never reads it.
    pub const IDLE_TEST_VAR: &str = "EITRI_REMOTE_TEST_IDLE_MS";

    /// What a window should do when the `serve` it found is another build's.
    pub const OTHER_BUILD_FIX: &str =
        "Eitri was upgraded: run eitri remote stop, then restart the Eitri windows, so the new serve starts";

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

    /// The address the desk said yes to, recorded beside the device store by the window that asked, while the link
    /// that holds it is still of the kind the desk was told; an unreadable record is no answer.
    fn confirmed_listen(state: &Path) -> Option<String> {
        eitri_core::remote_pairing::confirmed_listen_now(state).ok().flatten()
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
        let Err((code, line)) = run_serve(&dirs, args, confirmed_listen, idle_deadline());
        eprintln!("eitri remote serve: {line}");
        code
    }

    enum Never {}

    /// [`SERVE_IDLE_EXIT`], or a test build's [`IDLE_TEST_VAR`].
    fn idle_deadline() -> Duration {
        #[cfg(feature = "remote-test-hooks")]
        if let Some(ms) = std::env::var(IDLE_TEST_VAR).ok().and_then(|v| v.parse::<u64>().ok()) {
            return Duration::from_millis(ms);
        }
        SERVE_IDLE_EXIT
    }

    /// Where the journal copy goes: the journal's socket, while its directory exists (the sandbox can grant only a
    /// directory that is there), and that directory.
    fn journal() -> (JournalTarget, Option<PathBuf>) {
        match eitri_core::remote_audit::journal_target() {
            JournalTarget::Socket(path) => match path.parent().filter(|dir| dir.is_dir()) {
                Some(dir) => {
                    let dir = dir.to_path_buf();
                    (JournalTarget::Socket(path), Some(dir))
                }
                None => (JournalTarget::Off, None),
            },
            JournalTarget::Off => (JournalTarget::Off, None),
        }
    }

    /// Runs `serve` until no window has had a live `rv-` socket for `idle` (then `Err` with exit 0), or fails.
    fn run_serve(
        dirs: &Dirs,
        args: &[String],
        confirmed: fn(&Path) -> Option<String>,
        idle: Duration,
    ) -> Result<Never, (i32, String)> {
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
        let (control, _control_path) = super::control::bind_control(&dirs.control).map_err(failed)?;
        // Before the sandbox: it makes the audit directory, and the link's kind is read where the sandbox reads not.
        let place = audit_listener(&cfg.listen);
        let audit_dir = eitri_core::remote_audit::audit_dir(&dirs.state);
        let (journal, journal_dir) = journal();
        let log = AuditLog::open(&audit_dir, WriterKind::Serve, Some(cfg.audit_keep_days), journal)
            .map_err(|e| failed(format!("could not open the audit log in {}: {e}", audit_dir.display())))?;

        let state = sandbox(
            &dirs.agents(),
            &dirs.control,
            &dirs.state,
            &audit_dir,
            journal_dir.as_deref(),
        )
        .map_err(|e| failed(format!("could not confine itself: {e}")))?;
        let audit = Audit::new(log, &audit_dir);
        let sandbox_why = state.missing();
        if let Some(missing) = &sandbox_why {
            eprintln!("[remote] serving with a partial sandbox: {missing}");
        }
        // Blocked before any thread starts, so every thread inherits the mask and only the waiter takes them, and
        // before the record that `eitri remote stop` reads the pid from: a stop from then on is held for the waiter,
        // which records `disable` after `enable`.
        let signals = block_end_signals();
        let record = Record {
            v: RECORD_V,
            pid: std::process::id(),
            args: cfg.to_args(),
            sandbox: state.word().to_owned(),
            sandbox_why,
            build: Some(this_build()),
        };
        let mut line = serde_json::to_vec(&record).map_err(|e| failed(e.to_string()))?;
        line.push(b'\n');
        record_file
            .write_all(&line)
            .map_err(|e| failed(format!("could not write the record: {e}")))?;
        drop(record_file);

        audit.record(Event::Enable {
            listener: place.clone(),
            confirmed: matches!(cfg.listen, super::config::Listen::Address { .. }),
        });
        let ended = |e: String| {
            audit.disable_once("failed");
            failed(e)
        };
        std::thread::Builder::new()
            .name("eitri-remote-signals".into())
            .spawn({
                let audit = audit.clone();
                move || end_on_signal(signals, &audit)
            })
            .map_err(|e| ended(format!("could not start: {e}")))?;
        let hub = Hub::start_audited(
            &dirs.control,
            &dirs.state.join(DEVICES_FILE),
            Limits::default(),
            audit.clone(),
        )
        .map_err(|e| ended(format!("could not start serving: {e}")))?;
        accept_all(listeners, Arc::new(cfg), hub, Arc::new(place));
        super::control::serve_control(control, dirs.control.clone())
            .map_err(|e| ended(format!("could not answer the control socket: {e}")))?;
        // The lock is held for as long as the process runs: a window that looks while it waits here sees it running.
        let _held = lock;
        wait_for_no_window(&dirs.control, idle, idle_poll(idle));
        audit.disable_once("idle");
        Err((
            0,
            format!(
                "no window has had remote control on for {} ms; ending",
                idle.as_millis()
            ),
        ))
    }

    /// The signals that end `serve` and are recorded first: `SIGTERM` (`eitri remote stop`) and, unless it was
    /// ignored when `serve` started, `SIGINT`. Blocked in the calling thread; the set to wait for is returned.
    fn block_end_signals() -> libc::sigset_t {
        // SAFETY: `set` is a local `sigset_t` initialised by `sigemptyset` before use; `old` is written by `sigaction`
        // with a null new action, which changes nothing; `pthread_sigmask` reads `set` only.
        unsafe {
            let mut set: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut set);
            libc::sigaddset(&mut set, libc::SIGTERM);
            let mut old: libc::sigaction = std::mem::zeroed();
            if libc::sigaction(libc::SIGINT, std::ptr::null(), &mut old) == 0 && old.sa_sigaction != libc::SIG_IGN {
                libc::sigaddset(&mut set, libc::SIGINT);
            }
            libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
            set
        }
    }

    /// Waits for one of `signals`, records `disable`, and ends the process by that same signal, so whoever waits for
    /// it sees it end as it would have without the record.
    fn end_on_signal(signals: libc::sigset_t, audit: &Audit) {
        let mut signal: libc::c_int = 0;
        // SAFETY: `signals` is an initialised set and `signal` a local the call writes.
        if unsafe { libc::sigwait(&signals, &mut signal) } != 0 {
            return;
        }
        audit.disable_once(if signal == libc::SIGTERM {
            "stopped"
        } else {
            "interrupted"
        });
        // SAFETY: restores the default action for `signal`, unblocks it in this thread and raises it here; the
        // default action ends the process. No pointer outlives its call.
        unsafe {
            libc::signal(signal, libc::SIG_DFL);
            let mut one: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut one);
            libc::sigaddset(&mut one, signal);
            libc::pthread_sigmask(libc::SIG_UNBLOCK, &one, std::ptr::null_mut());
            libc::raise(signal);
        }
    }

    /// How often the windows are looked for while `serve` waits for the last to go.
    fn idle_poll(idle: Duration) -> Duration {
        (idle / 12).clamp(Duration::from_millis(10), Duration::from_secs(5))
    }

    /// Returns once no window in `control` has had a live `rv-` socket for `idle`, looking every `poll`
    /// ([`live_windows`](super::report::live_windows): a socket whose window's rows name a live process; one a
    /// crashed window left behind is none). The sockets are not connected to, so the windows are not woken.
    pub fn wait_for_no_window(control: &Path, idle: Duration, poll: Duration) {
        let mut last_seen = Instant::now();
        loop {
            let any = !super::report::live_windows(control).is_empty();
            if any {
                last_seen = Instant::now();
            } else if last_seen.elapsed() >= idle {
                return;
            }
            std::thread::sleep(poll);
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
        let (record, running) = match read_recorded(dirs) {
            Recorded::Whole(record, running) => (record, running),
            Recorded::OtherBuild { pid, build } => {
                return (EXIT_RUNNING_DIFFERENT, other_build_text(pid, build.as_deref()));
            }
            Recorded::NotYet => {
                return (
                    EXIT_RUNNING_SAME,
                    "already running (still starting: its record is not written yet)".to_owned(),
                )
            }
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

    /// What the record of the running `serve` says.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum Recorded {
        /// A record of this build, whole, and the configuration it holds.
        Whole(Record, ServeConfig),
        /// A record another build wrote (`build` is its own name for itself, `None` from a build that named none): only
        /// its pid is read.
        OtherBuild { pid: u32, build: Option<String> },
        /// Nothing whole yet: `serve` empties it when it takes the lock and writes it once it is confined.
        NotYet,
    }

    /// Reads the record beside the lock. Its build is looked at first, in a reading that takes any fields, so a record
    /// of another build is named as such whatever its shape; only this build's is read field by field.
    pub fn read_recorded(dirs: &Dirs) -> Recorded {
        let Ok(text) = agent::private_fs::read_private_nofollow_capped(&dirs.control.join(RECORD_FILE), 64 * 1024)
        else {
            return Recorded::NotYet;
        };
        let Ok(loose) = serde_json::from_str::<serde_json::Value>(text.trim_end()) else {
            return Recorded::NotYet;
        };
        let build = loose.get("build").and_then(serde_json::Value::as_str);
        if build != Some(this_build().as_str()) {
            return match loose
                .get("pid")
                .and_then(serde_json::Value::as_u64)
                .and_then(|p| u32::try_from(p).ok())
            {
                Some(pid) => Recorded::OtherBuild {
                    pid,
                    build: build.map(str::to_owned),
                },
                None => Recorded::NotYet,
            };
        }
        let Ok(record) = serde_json::from_value::<Record>(loose) else {
            return Recorded::NotYet;
        };
        if record.v != RECORD_V {
            return Recorded::NotYet;
        }
        // The interface was looked up when it started; an address this machine no longer has keeps an empty name.
        match ServeConfig::from_args(&record.args, || Ok(0), |addr| iface_of(addr).or(Some(String::new()))) {
            Ok(cfg) => Recorded::Whole(record, cfg),
            Err(_) => Recorded::NotYet,
        }
    }

    /// The running `serve`'s record and the configuration it holds, when the record is whole and this build's.
    pub fn read_record(dirs: &Dirs) -> Option<(Record, ServeConfig)> {
        match read_recorded(dirs) {
            Recorded::Whole(record, cfg) => Some((record, cfg)),
            Recorded::OtherBuild { .. } | Recorded::NotYet => None,
        }
    }

    /// When the running `serve`'s record is another build's, what the desk is told.
    pub fn other_build(dirs: &Dirs) -> Option<String> {
        match read_recorded(dirs) {
            Recorded::OtherBuild { pid, build } => Some(other_build_text(pid, build.as_deref())),
            Recorded::Whole(..) | Recorded::NotYet => None,
        }
    }

    /// What is said of a running `serve` that another build started.
    pub fn other_build_text(pid: u32, build: Option<&str>) -> String {
        let theirs = build.map_or_else(|| "an earlier build".to_owned(), |b| format!("build {b}"));
        format!(
            "eitri remote serve (pid {pid}) is from another Eitri build ({theirs}; this is {}): {OTHER_BUILD_FIX}",
            this_build()
        )
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

    fn accept_all(listeners: Listeners, cfg: Arc<ServeConfig>, hub: Arc<Hub>, place: Arc<Listener>) {
        let open = Arc::new(AtomicUsize::new(0));
        let ranges = Arc::new(listeners.peer_ranges);
        for listener in listeners.tcp {
            let (cfg, open, ranges, hub, place) =
                (cfg.clone(), open.clone(), ranges.clone(), hub.clone(), place.clone());
            std::thread::spawn(move || accept_tcp(listener, cfg, open, ranges, hub, place));
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
        place: Arc<Listener>,
    ) {
        for stream in listener.incoming() {
            let Ok(stream) = stream else {
                pause_after_accept_error();
                continue;
            };
            let peer = stream
                .peer_addr()
                .map_or_else(|_| "unknown".to_owned(), |peer| peer.to_string());
            if let Some(networks) = ranges.as_ref() {
                if !stream.peer_addr().is_ok_and(|peer| peer_in(peer.ip(), networks)) {
                    record_early_refusal(&hub, Reason::PeerOutsideInterface, &place, &peer);
                    continue;
                }
            }
            let Some(slot) = Slot::take(&open) else { continue };
            let (cfg, hub, place) = (cfg.clone(), hub.clone(), place.clone());
            std::thread::spawn(move || {
                let _slot = slot;
                answer(Box::new(stream), &cfg, &hub, peer, &place);
            });
        }
    }

    /// A refusal before any `hello`: where it came in and from, and no device.
    fn record_early_refusal(hub: &Hub, reason: Reason, place: &Listener, peer: &str) {
        hub.audit().record_early_refusal(reason, None, place, peer);
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
                answer(Box::new(stream), &cfg, &hub, "unix".to_owned(), &Listener::Socket);
            });
        }
    }

    /// An accept that failed (out of descriptors, say) fails again at once until something is let go: a pause keeps
    /// the accepting thread from spinning meanwhile.
    fn pause_after_accept_error() {
        std::thread::sleep(Duration::from_millis(50));
    }

    fn answer(mut stream: Box<dyn Conn>, cfg: &ServeConfig, hub: &Arc<Hub>, peer: String, place: &Listener) {
        match handshake(stream.as_mut(), cfg) {
            Ok(Handshake::Upgraded { host, origin }) => {
                let arrival = Arrival {
                    host,
                    origin,
                    peer,
                    listener: place.clone(),
                };
                hub.serve_phone_from(stream, arrival);
            }
            Ok(Handshake::Refused(why)) => {
                if let Some(reason) = super::origin::refusal_reason(why) {
                    record_early_refusal(hub, reason, place, &peer);
                }
            }
            Ok(Handshake::Gone) | Err(_) => {}
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
        let _ = write_status_of(&mut out, &read_recorded(&dirs));
        0
    }

    /// What `status` prints for a running `serve` whose record says `recorded`.
    fn write_status_of(out: &mut impl Write, recorded: &Recorded) -> io::Result<()> {
        match recorded {
            Recorded::Whole(record, cfg) => write_status(out, record, cfg),
            Recorded::OtherBuild { pid, build } => {
                writeln!(out, "running: pid {pid}")?;
                writeln!(out, "build: {}", other_build_text(*pid, build.as_deref()))
            }
            Recorded::NotYet => writeln!(out, "running: starting"),
        }
    }

    fn write_status(out: &mut impl Write, record: &Record, cfg: &ServeConfig) -> io::Result<()> {
        let origins = if cfg.origins.is_empty() {
            "none".to_owned()
        } else {
            cfg.origins.join(" ")
        };
        writeln!(out, "running: pid {}", record.pid)?;
        writeln!(out, "listen: {}", cfg.listen_text())?;
        writeln!(out, "port: {}", cfg.port)?;
        writeln!(out, "origins: {origins}")?;
        writeln!(out, "sandbox: {}", record.sandbox_text())
    }

    /// Notes that `eitri remote stop` runs now, for [`stopped_since`].
    pub fn mark_stopped(dirs: &Dirs) -> io::Result<()> {
        let at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(io::Error::other)?
            .as_nanos();
        agent::private_fs::write_private(&dirs.control.join(STOPPED_FILE), format!("{at}\n").as_bytes())
    }

    /// Whether `eitri remote stop` ran at or after `since`. A note that cannot be read is no stop.
    pub fn stopped_since(dirs: &Dirs, since: std::time::SystemTime) -> bool {
        let Ok(text) = agent::private_fs::read_private_nofollow_capped(&dirs.control.join(STOPPED_FILE), 64) else {
            return false;
        };
        let Ok(at) = text.trim_end().parse::<u128>() else {
            return false;
        };
        since
            .duration_since(std::time::UNIX_EPOCH)
            .is_ok_and(|since| at >= since.as_nanos())
    }

    /// `eitri remote stop`: ends the running `serve`, and every phone's connection with it, then waits up to 5 s for
    /// its lock to be let go. A window starts it again only at its own next start, while remote control is on: the
    /// stop is noted first ([`mark_stopped`]), so a window that found `serve` running and looks again does not.
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
        // Noted even when none runs: a window may be about to look again for one that had just ended.
        match mark_stopped(&dirs) {
            // No control directory: no window has started, so none looks again.
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => {
                eprintln!("eitri remote stop: not noted ({e}); a window started in the last 10 s may start it again")
            }
            Ok(()) => {}
        }
        if !is_running(&dirs).unwrap_or(false) {
            println!("not running");
            return 0;
        }
        // Another build's `serve` is stopped all the same: only its pid is needed.
        let pid = match read_recorded(&dirs) {
            Recorded::Whole(record, _) => record.pid,
            Recorded::OtherBuild { pid, .. } => pid,
            Recorded::NotYet => {
                eprintln!("eitri remote stop: it is still starting; try again");
                return EXIT_FAILED;
            }
        };
        // The holder empties the record as soon as it takes the lock and writes it only while holding it, and the
        // lock goes with the process, so a pid read while it is held names the holder (unless the holder ended
        // between the two looks and its pid was taken again in that moment).
        // SAFETY: `kill` takes no pointers.
        if unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) } != 0 {
            eprintln!("eitri remote stop: {}", io::Error::last_os_error());
            return EXIT_FAILED;
        }
        let until = Instant::now() + STOP_WAIT;
        while Instant::now() < until {
            if !is_running(&dirs).unwrap_or(true) {
                println!("stopped");
                return 0;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        eprintln!("eitri remote stop: pid {pid} still holds the lock after 5 s");
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
        fn status_prints_the_recorded_sandbox_reason() {
            let record: Record = serde_json::from_str(
                r#"{"v":1,"pid":1,"args":[],"sandbox":"partial","sandbox_why":"the kernel has no Landlock"}"#,
            )
            .unwrap();
            let cfg = ServeConfig::from_args(&[], || Ok(7500), |_| None).unwrap();
            let mut out = Vec::new();
            write_status(&mut out, &record, &cfg).unwrap();
            assert!(String::from_utf8(out)
                .unwrap()
                .ends_with("sandbox: partial (the kernel has no Landlock)\n"));
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
            let (code, line) = match run_serve(&dirs, &args, record_at_bind, SERVE_IDLE_EXIT) {
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
        fn serve_exits_after_the_last_window() {
            use eitri_core::agents::{ProjectRef, Publisher, WindowRef};
            let control = scratch("idle");
            let instance = "a3f09c1d2e4b5f60";
            let socket = eitri_core::remote_rv::rv_socket_path(&control, instance).unwrap();
            let window = std::os::unix::net::UnixListener::bind(&socket).unwrap();
            window.set_nonblocking(true).unwrap();
            let rows = Publisher::start(
                &control,
                instance,
                ProjectRef {
                    key: "9b2c4e7a10f3d856".into(),
                    name: "p".into(),
                },
                WindowRef {
                    instance: instance.into(),
                    name: "1".into(),
                    focused: false,
                },
            )
            .unwrap();
            let until = Instant::now() + Duration::from_secs(30);
            while super::super::report::live_windows(&control).is_empty() {
                assert!(Instant::now() < until, "the window's rows never showed");
                std::thread::sleep(Duration::from_millis(10));
            }
            let (done_tx, done) = std::sync::mpsc::channel();
            let watched = control.clone();
            std::thread::spawn(move || {
                wait_for_no_window(&watched, Duration::from_millis(300), Duration::from_millis(10));
                let _ = done_tx.send(());
            });
            assert!(
                done.recv_timeout(Duration::from_millis(900)).is_err(),
                "it ended while a window was up"
            );
            assert_eq!(
                window.accept().map(|_| ()).unwrap_err().kind(),
                io::ErrorKind::WouldBlock,
                "looking for windows connected to one"
            );
            // The window's rows go (a crashed window's name a dead process, which counts the same) while its socket
            // stays, and here would even take a connection.
            rows.stop();
            assert!(socket.exists());
            done.recv_timeout(Duration::from_secs(30))
                .expect("it did not end after the last window went");
            drop(window);
        }

        #[test]
        fn a_stop_is_seen_only_by_a_look_before_it() {
            let dir = scratch("stopped");
            let dirs = Dirs {
                control: dir.join("control"),
                state: dir.join("state"),
            };
            eitri_core::panel_control::ensure_dir(&dirs.control).unwrap();
            let looked = std::time::SystemTime::now();
            assert!(!stopped_since(&dirs, looked), "no stop was noted yet");
            mark_stopped(&dirs).unwrap();
            assert!(stopped_since(&dirs, looked));
            std::thread::sleep(Duration::from_millis(5));
            assert!(
                !stopped_since(&dirs, std::time::SystemTime::now()),
                "a stop before the look counted"
            );
            std::fs::write(dirs.control.join(STOPPED_FILE), b"not a time\n").unwrap();
            assert!(!stopped_since(&dirs, looked), "an unreadable note counted as a stop");
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
        fn a_serve_from_another_build_says_to_restart_the_windows() {
            let dir = scratch("skew");
            let dirs = Dirs {
                control: dir.join("control"),
                state: dir.join("state"),
            };
            eitri_core::panel_control::ensure_dir(&dirs.control).unwrap();
            let cfg =
                ServeConfig::from_args(&["--listen".to_owned(), "socket".to_owned()], || Ok(7500), |_| None).unwrap();
            for (body, build) in [
                // A build from before the record named its build.
                (r#"{"v":1,"pid":7,"args":["--listen","socket"],"sandbox":"full"}"#, None),
                // A later build's, with a field this one does not know.
                (
                    r#"{"v":1,"pid":7,"args":["--listen","socket"],"sandbox":"full","build":"9.9.9-0","later":1}"#,
                    Some("9.9.9-0"),
                ),
            ] {
                std::fs::write(dirs.control.join(RECORD_FILE), format!("{body}\n")).unwrap();
                assert!(read_record(&dirs).is_none(), "{body}");
                assert_eq!(
                    read_recorded(&dirs),
                    Recorded::OtherBuild {
                        pid: 7,
                        build: build.map(str::to_owned)
                    },
                    "{body}"
                );
                let mut out = Vec::new();
                write_status_of(&mut out, &read_recorded(&dirs)).unwrap();
                let text = String::from_utf8(out).unwrap();
                assert!(text.starts_with("running: pid 7\n"), "{text}");
                assert!(
                    text.contains("Eitri was upgraded") && text.contains("restart the Eitri windows"),
                    "{text}"
                );
                let (code, line) = already_running(&dirs, &cfg);
                assert_eq!(code, EXIT_RUNNING_DIFFERENT, "{line}");
                assert!(line.contains("Eitri was upgraded"), "{line}");
            }
            let ours = serde_json::json!({
                "v": RECORD_V, "pid": 8, "args": ["--listen", "socket", "--port", "7500"], "sandbox": "full",
                "build": super::super::config::this_build(),
            });
            std::fs::write(dirs.control.join(RECORD_FILE), format!("{ours}\n")).unwrap();
            assert!(matches!(read_recorded(&dirs), Recorded::Whole(record, _) if record.pid == 8));
            assert_eq!(already_running(&dirs, &cfg).0, EXIT_RUNNING_SAME);
        }

        #[test]
        fn a_peer_outside_the_interface_is_recorded() {
            use eitri_core::remote_audit::{read, AuditLog, Filter, JournalTarget, Listener, WriterKind};
            let dir = scratch("outside");
            let control = dir.join("control");
            eitri_core::panel_control::ensure_dir(&control).unwrap();
            let audit_dir = dir.join("audit");
            let log = AuditLog::open(&audit_dir, WriterKind::Serve, None, JournalTarget::Off).unwrap();
            let hub = Hub::start_audited(
                &control,
                &dir.join(DEVICES_FILE),
                Limits::default(),
                super::super::session::Audit::new(log, &audit_dir),
            )
            .unwrap();
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            let cfg = Arc::new(ServeConfig::from_args(&[], || Ok(addr.port()), |_| None).unwrap());
            let ranges = Arc::new(Some(vec![("198.51.100.0".parse().unwrap(), 24)]));
            let place = Arc::new(Listener::Address {
                addr: "198.51.100.2".into(),
                iface: "wg0".into(),
                kind: "wireguard".into(),
            });
            let open = Arc::new(AtomicUsize::new(0));
            std::thread::spawn(move || accept_tcp(listener, cfg, open, ranges, hub, place));
            let _peer = std::net::TcpStream::connect(addr).unwrap();
            let until = Instant::now() + Duration::from_secs(30);
            let refused = loop {
                let got = read(&audit_dir, &Filter::default(), None).unwrap();
                if let Some(record) = got.into_iter().find(|r| r.event == "refused") {
                    break record;
                }
                assert!(Instant::now() < until, "no refusal was recorded");
                std::thread::sleep(Duration::from_millis(10));
            };
            assert_eq!(refused.fields["reason"], "peer_outside_interface");
            assert_eq!(refused.fields["listener"], "address");
            assert_eq!(refused.fields["listen_iface"], "wg0");
            assert!(
                refused.fields["peer"]
                    .as_str()
                    .is_some_and(|p| p.starts_with("127.0.0.1:")),
                "{:?}",
                refused.fields
            );
            assert!(
                refused.fields.get("device").is_none(),
                "a refusal before hello names no device"
            );
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
