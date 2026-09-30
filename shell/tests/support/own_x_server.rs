//! Provides an Xvfb started by each test instead of connecting to an inherited display.
//! The shell and editor tests include this shared file with #[path]. Checking only
//! GDK_BACKEND=x11 cannot exclude a desktop's XWayland session.
//!
//! The server disables TCP and filesystem Unix sockets with -nolisten tcp and -nolisten unix.
//! Clients use its abstract socket in the process's network namespace. It creates no X socket
//! or display lock file in /tmp, though it briefly writes a compiled keymap there.
//! Disabling filesystem sockets prevents a server in a separate network namespace with a shared
//! /tmp from unlinking a desktop socket whose abstract counterpart it cannot see.
//!
//! isolate selects a display number from FIRST_DISPLAY upward with no socket file, lock file
//! or visible socket. It retries if another server wins the bind. This also protects clients
//! that cannot see the abstract socket, including sandboxed WebKit processes: their filesystem
//! fallback finds no socket instead of reaching another server with the same display number.
//! The server uses -noreset to stay alive between clients. XAUTHORITY points at an empty file
//! in target/tmp, removed on shutdown and on SIGTERM, SIGINT or SIGHUP.
//! init_gtk checks the pid behind each X connection through OwnXServer::foreign_x_connections;
//! a display name alone cannot identify the server.
//!
//! Before any display connection, DISPLAY points at this server and GDK_BACKEND is x11.
//! WAYLAND_DISPLAY and WAYLAND_SOCKET are removed. Inherited displays, including xvfb-run's,
//! are ignored. GTK_IM_MODULE and XMODIFIERS select GTK's built-in input method so keys do not
//! reach a session input method or cause candidate windows on another display.
//! init_gtk initialises GTK and refuses to continue unless every X connection is to this server.
//!
//! cut_session_bus points DBUS_SESSION_BUS_ADDRESS at NO_SESSION_BUS, a socket path that cannot
//! exist, and removes AT_SPI_BUS_ADDRESS. Removing the session bus address alone would let GDBus
//! fall back to XDG_RUNTIME_DIR/bus or autolaunch. Blocking the bus prevents activation of
//! accessibility, portal and GVfs services, which these tests do not use.
//!
//! The server is stopped by its captured pid. OwnXServer::exit handles explicit main exits,
//! since process::exit runs no destructors; Drop handles panic unwinding out of main.
//! PR_SET_PDEATHSIG sends SIGTERM if the spawning thread ends through an abort, another exit
//! or SIGKILL. It is set between fork and exec. Call isolate from the main thread so the
//! server's lifetime follows that thread.
#![allow(dead_code)] // each test uses a different subset: `init_gtk` or `isolate`, `display` or not.

use std::io::BufRead;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicPtr, Ordering};
use std::time::{Duration, Instant};

/// The first display number [`isolate`] considers. Well above the numbers anything else takes by
/// default -- a desktop's XWayland (`:0`, `:1`), `Xvfb -displayfd` (from `:0` up), `xvfb-run` (from
/// `:99` up) -- so it rarely has to skip one; it is not what makes a number safe (the checks in
/// [`display_is_unclaimed`] are).
pub const FIRST_DISPLAY: u32 = 500;
/// How many numbers from [`FIRST_DISPLAY`] on it tries before refusing.
const DISPLAYS_TO_TRY: u32 = 100;

/// The session bus every guarded test runs with: a socket path that cannot exist, since `/dev/null`
/// is not a directory (`connect` fails with `ENOTDIR`), so nothing started under it can reach a bus,
/// D-Bus-activate a service on one, or have one appear there later.
pub const NO_SESSION_BUS: &str = "unix:path=/dev/null/eitri-test-has-no-session-bus";

/// Why [`OwnXServer::start_on`] did not start a server on the number it was given.
enum StartError {
    /// Another server holds that number: the next one may do.
    Taken(String),
    /// Every number would fail alike.
    Fatal(String),
}

/// An `Xvfb` this process started, and the only display it talks to.
#[must_use = "dropping the server stops it at once"]
pub struct OwnXServer {
    child: Option<Child>,
    display: String,
    xauthority: PathBuf,
}

/// Whether nothing else has, or could fall back to, display `:n`: no socket file and no lock file for
/// it in `/tmp` (anything at those paths, of any type, counts), and no socket for it -- abstract or
/// not -- that this network namespace can see.
fn display_is_unclaimed(n: u32, unix_sockets: &str) -> bool {
    let file = format!("/tmp/.X11-unix/X{n}");
    let taken =
        |path: &str| !matches!(Path::new(path).symlink_metadata(), Err(e) if e.kind() == std::io::ErrorKind::NotFound);
    !taken(&file)
        && !taken(&format!("/tmp/.X{n}-lock"))
        && !unix_sockets.lines().any(|line| {
            line.split_whitespace()
                .nth(7)
                .is_some_and(|path| path.trim_start_matches('@') == file)
        })
}

impl OwnXServer {
    /// `Xvfb :<n> -displayfd 1` on the first unclaimed number from [`FIRST_DISPLAY`], with one screen
    /// of `screen` (e.g. `"1920x1200x24"`), no TCP listener, no socket file, no lock file, and no
    /// reset or exit when its last client leaves.
    fn start(test: &str, screen: &str) -> Result<Self, String> {
        let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
        let xauthority = dir.join(format!("{test}-xauthority-{}", std::process::id()));
        std::fs::write(&xauthority, b"").map_err(|e| format!("cannot create {}: {e}", xauthority.display()))?;
        let mut last_error = String::new();
        for n in FIRST_DISPLAY..FIRST_DISPLAY + DISPLAYS_TO_TRY {
            let unix = std::fs::read_to_string("/proc/net/unix").map_err(|e| format!("/proc/net/unix: {e}"))?;
            if !display_is_unclaimed(n, &unix) {
                continue;
            }
            match Self::start_on(test, screen, n, &dir) {
                // A file that appeared while the server started is someone else's: move on (the
                // server drops, and stops, here).
                Ok(_) if !display_is_unclaimed_by_file(n) => {
                    last_error = format!(":{n} gained a socket or lock file while starting")
                }
                Ok(mut server) => {
                    remove_on_fatal_signal(&xauthority);
                    server.xauthority = xauthority;
                    return Ok(server);
                }
                Err(StartError::Taken(e)) => last_error = e,
                Err(StartError::Fatal(e)) => {
                    let _ = std::fs::remove_file(&xauthority);
                    return Err(e);
                }
            }
        }
        let _ = std::fs::remove_file(&xauthority);
        Err(format!(
            "no display from :{FIRST_DISPLAY} to :{} could be started (last: {last_error})",
            FIRST_DISPLAY + DISPLAYS_TO_TRY - 1
        ))
    }

    fn start_on(test: &str, screen: &str, n: u32, dir: &Path) -> Result<Self, StartError> {
        let log = dir.join(format!("{test}-xvfb-{}.log", std::process::id()));
        let stderr = std::fs::File::create(&log)
            .map_err(|e| StartError::Fatal(format!("cannot create {}: {e}", log.display())))?;
        let parent = std::process::id() as libc::pid_t;
        let mut command = Command::new("Xvfb");
        command
            .arg(format!(":{n}"))
            .args([
                "-displayfd",
                "1",
                "-nolisten",
                "tcp",
                "-nolisten",
                "unix",
                "-noreset",
                "-screen",
                "0",
                screen,
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::from(stderr));
        // SAFETY: the closure runs in the forked child before `exec` and calls only async-signal-safe
        // functions (`prctl`, `getppid`, `_exit`); it allocates nothing and takes no lock.
        unsafe {
            command.pre_exec(move || {
                // SIGTERM to the server when this process dies, however it dies.
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM as libc::c_ulong) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                // Died between fork and prctl: nobody would ever send that signal.
                if libc::getppid() != parent {
                    libc::_exit(1);
                }
                Ok(())
            });
        }
        let mut child = command
            .spawn()
            .map_err(|e| StartError::Fatal(format!("`Xvfb` could not be started ({e}) -- install xorg-server-xvfb")))?;
        let stdout = child.stdout.take().expect("stdout was piped");
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut line = String::new();
            let _ = std::io::BufReader::new(stdout).read_line(&mut line);
            let _ = tx.send(line);
        });
        let mut server = OwnXServer {
            child: Some(child),
            display: String::new(),
            // Set by `start` once it keeps this server: until then the file is not this server's to
            // remove when it stops.
            xauthority: PathBuf::new(),
        };
        match rx.recv_timeout(Duration::from_secs(15)) {
            Ok(line) if line.trim() == n.to_string() => {
                server.display = format!(":{n}");
                Ok(server)
            }
            // It exited without a number. Another server won the bind for `:n` in this namespace
            // (probed: "Cannot establish any listening sockets", exit 1) -- try the next one; any
            // other reason would fail on every number alike.
            Ok(line) if line.is_empty() => {
                let _ = server.stop();
                let said = std::fs::read_to_string(&log).unwrap_or_default();
                if said.contains("Cannot establish any listening sockets") {
                    Err(StartError::Taken(format!(":{n} is taken")))
                } else {
                    Err(StartError::Fatal(format!(
                        "Xvfb on :{n} exited without a display number (log: {})",
                        log.display()
                    )))
                }
            }
            Ok(line) => Err(StartError::Fatal(format!(
                "Xvfb on :{n} wrote {line:?}, not its display number (log: {})",
                log.display()
            ))),
            Err(_) => Err(StartError::Fatal(format!(
                "Xvfb on :{n} reported nothing within 15 s (log: {})",
                log.display()
            ))),
        }
        // On either error `server` drops here, which stops the child.
    }

    /// This server's display name, e.g. `:500`.
    pub fn display(&self) -> &str {
        &self.display
    }

    /// The server's pid while it is this process's unreaped child.
    pub fn pid(&self) -> Option<u32> {
        self.child.as_ref().map(Child::id)
    }

    /// Whether the server is still running (reaps it if it has exited).
    pub fn is_running(&mut self) -> bool {
        match self.child.as_mut() {
            Some(child) => matches!(child.try_wait(), Ok(None)),
            None => false,
        }
    }

    /// Every connection this process holds to an X server's socket (any `…/.X11-unix/X<n>`, abstract
    /// or file), as `(fd, address, server pid)`, split into this server's and every other one's. The
    /// server is identified by the pid behind the socket (`SO_PEERCRED`), which a name cannot fake:
    /// two servers can share the name `:0` in different network namespaces.
    pub fn x_connections(&self) -> (usize, Vec<String>) {
        let own = self.pid();
        let mut mine = 0;
        let mut foreign = Vec::new();
        let Ok(fds) = std::fs::read_dir("/proc/self/fd") else {
            return (0, vec!["/proc/self/fd cannot be read".into()]);
        };
        for fd in fds
            .filter_map(|e| e.ok())
            .filter_map(|e| e.file_name().to_str()?.parse::<i32>().ok())
        {
            let Some(address) = unix_peer_address(fd) else { continue };
            if !address.contains("/.X11-unix/X") {
                continue;
            }
            let pid = peer_pid(fd);
            if pid.is_some() && pid == own {
                mine += 1;
            } else {
                foreign.push(format!("fd {fd} -> {address} (server pid {pid:?})"));
            }
        }
        (mine, foreign)
    }

    /// The connections [`OwnXServer::x_connections`] finds to any X server but this one.
    pub fn foreign_x_connections(&self) -> Vec<String> {
        self.x_connections().1
    }

    /// SIGTERM by the captured pid, then SIGKILL if it has not exited within 3 s; returns whether it
    /// sent a signal. Idempotent. **Never signals a reaped child**: once
    /// [`OwnXServer::is_running`] (or anything else) has reaped the server, its pid is free for the
    /// kernel to hand to an unrelated process, so the pid is signalled only while `try_wait` still
    /// says the child is unreaped -- a zombie keeps its pid, so nothing can take it in between.
    pub fn stop(&mut self) -> bool {
        if !self.xauthority.as_os_str().is_empty() {
            let _ = std::fs::remove_file(&self.xauthority);
        }
        let Some(mut child) = self.child.take() else {
            return false;
        };
        // `try_wait` on a child already reaped returns the status it kept, without a syscall.
        if !matches!(child.try_wait(), Ok(None)) {
            return false;
        }
        // SAFETY: `kill` has no memory-safety preconditions; the child is unreaped (above), so this
        // pid is still this server's, running or a zombie.
        unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) };
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if let Ok(Some(_)) = child.try_wait() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        // `Child::kill` itself sends nothing to a child it has already reaped.
        let _ = child.kill();
        let _ = child.wait();
        true
    }

    /// Stops the server, then exits with `code`. `std::process::exit` runs no destructor, so every
    /// exit after [`isolate`] goes through here rather than calling it directly. Call it only once
    /// nothing will iterate GTK's main loop again: GDK's X I/O error handler exits the process with
    /// its own status when its display goes away under it.
    pub fn exit(mut self, code: i32) -> ! {
        let _ = self.stop();
        std::process::exit(code)
    }
}

impl Drop for OwnXServer {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

/// No socket or lock file for `:n` exists (the part of [`display_is_unclaimed`] that can change
/// while the server starts without being this server's own doing).
fn display_is_unclaimed_by_file(n: u32) -> bool {
    display_is_unclaimed(n, "")
}

/// The xauthority files of this process's servers, for [`unlink_and_die`]: C strings leaked on
/// purpose, since a signal handler can neither take a lock nor free memory. A slot is never emptied
/// again; unlinking a file `stop` already removed fails harmlessly.
static XAUTHORITY_FILES: [AtomicPtr<libc::c_char>; 4] = [const { AtomicPtr::new(std::ptr::null_mut()) }; 4];
static FATAL_SIGNAL_HANDLERS: std::sync::Once = std::sync::Once::new();

/// Removes `path` if SIGTERM, SIGINT or SIGHUP ends this process. Those end a
/// test without [`OwnXServer::stop`] -- the server itself still goes, on `PR_SET_PDEATHSIG` -- and each
/// left its empty xauthority file in `target/tmp`. A signal whose disposition is not the default (a
/// background job's ignored SIGINT, a handler the test set) is left as it was, and so is its file.
/// SIGKILL cannot be caught: a SIGKILLed run still leaves the file, empty and inert, as it leaves the
/// server's log (`<test>-xvfb-<pid>.log`, kept on every path so a failure can be read).
fn remove_on_fatal_signal(path: &Path) {
    use std::os::unix::ffi::OsStrExt;
    let Ok(path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return;
    };
    let raw = path.into_raw();
    let stored = XAUTHORITY_FILES.iter().any(|slot| {
        slot.compare_exchange(std::ptr::null_mut(), raw, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    });
    if !stored {
        // More servers in one process than slots (no test starts more than two): `stop` alone
        // removes this file.
        // SAFETY: `raw` came from `into_raw` just above and was stored nowhere.
        drop(unsafe { std::ffi::CString::from_raw(raw) });
        return;
    }
    FATAL_SIGNAL_HANDLERS.call_once(|| {
        for signal in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
            // SAFETY: an all-zero `sigaction` is a valid value, and every pointer passed is valid for
            // the call.
            unsafe {
                let mut current: libc::sigaction = std::mem::zeroed();
                if libc::sigaction(signal, std::ptr::null(), &mut current) != 0 || current.sa_sigaction != libc::SIG_DFL
                {
                    continue;
                }
                let mut action: libc::sigaction = std::mem::zeroed();
                action.sa_sigaction = unlink_and_die as extern "C" fn(libc::c_int) as libc::sighandler_t;
                // On entry the default action is restored and the signal left unblocked, so the
                // `raise` in the handler ends the process as the signal would have.
                action.sa_flags = libc::SA_RESETHAND | libc::SA_NODEFER;
                libc::sigemptyset(&mut action.sa_mask);
                libc::sigaction(signal, &action, std::ptr::null_mut());
            }
        }
    });
}

/// The handler [`remove_on_fatal_signal`] installs: only async-signal-safe calls.
extern "C" fn unlink_and_die(signal: libc::c_int) {
    for slot in &XAUTHORITY_FILES {
        let path = slot.load(Ordering::Acquire);
        if !path.is_null() {
            // SAFETY: `path` is a NUL-terminated string that is never freed; `unlink` is
            // async-signal-safe.
            unsafe { libc::unlink(path) };
        }
    }
    // SAFETY: `raise` is async-signal-safe; the signal's default action is back (`SA_RESETHAND`).
    unsafe { libc::raise(signal) };
}

/// The address of the Unix socket `fd`'s peer, `@`-prefixed if abstract; `None` for anything else.
fn unix_peer_address(fd: i32) -> Option<String> {
    // SAFETY: an all-zero `sockaddr_un` is a valid value.
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t;
    // SAFETY: `address` and `len` are valid for writes of the sizes given; `fd` is only queried.
    let rc = unsafe { libc::getpeername(fd, (&mut address as *mut libc::sockaddr_un).cast(), &mut len) };
    if rc != 0 || address.sun_family != libc::AF_UNIX as libc::sa_family_t {
        return None;
    }
    let offset = std::mem::size_of::<libc::sa_family_t>();
    let path_len = (len as usize).saturating_sub(offset).min(address.sun_path.len());
    let bytes: Vec<u8> = address.sun_path[..path_len].iter().map(|&c| c as u8).collect();
    match bytes.split_first() {
        Some((0, abstract_name)) => Some(format!("@{}", String::from_utf8_lossy(abstract_name))),
        Some(_) => Some(String::from_utf8_lossy(bytes.split(|&b| b == 0).next().unwrap_or(&[])).into_owned()),
        None => None,
    }
}

/// The pid of the process that listens on the socket `fd` is connected to (`SO_PEERCRED`).
fn peer_pid(fd: i32) -> Option<u32> {
    let mut cred = libc::ucred { pid: 0, uid: 0, gid: 0 };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: `cred` and `len` are valid for writes of the sizes given; `fd` is only queried.
    let rc = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut cred as *mut libc::ucred).cast(),
            &mut len,
        )
    };
    (rc == 0 && cred.pid > 0).then_some(cred.pid as u32)
}

/// Starts this test's own X server and points the process at it, and at no session bus
/// ([`cut_session_bus`]), before anything connects to a display or a bus; exits with status 1, having
/// connected to nothing, if the server cannot be started. Call it first thing in `main`, on the main
/// thread, while no other thread reads the environment.
pub fn isolate(test: &str, screen: &str) -> OwnXServer {
    // First, so that the server, and every process this test starts, inherits no bus either.
    cut_session_bus(test);
    let server = match OwnXServer::start(test, screen) {
        Ok(server) => server,
        Err(e) => {
            eprintln!("{test}: refusing to run without a display of its own: {e}");
            std::process::exit(1);
        }
    };
    for var in ["WAYLAND_DISPLAY", "WAYLAND_SOCKET", "DISPLAY", "XAUTHORITY"] {
        if let Some(inherited) = std::env::var_os(var) {
            println!(
                "{test}: ignoring the inherited {var}={}; using its own Xvfb {}",
                inherited.to_string_lossy(),
                server.display
            );
        }
    }
    // Rust 2021: `set_var`/`remove_var` are safe to call here. They are sound only while no other
    // thread reads the environment -- true at this point: the Xvfb reader thread has delivered and
    // exited, and neither GTK nor winit has started.
    std::env::set_var("DISPLAY", &server.display);
    std::env::set_var("XAUTHORITY", &server.xauthority);
    std::env::set_var("GDK_BACKEND", "x11");
    std::env::remove_var("WAYLAND_DISPLAY");
    std::env::remove_var("WAYLAND_SOCKET");
    std::env::set_var("GTK_IM_MODULE", "gtk-im-context-simple");
    std::env::remove_var("XMODIFIERS");
    server
}

/// Points this process's session bus at [`NO_SESSION_BUS`] and removes `AT_SPI_BUS_ADDRESS` (which GTK
/// reads before asking the session bus for the accessibility bus), saying which inherited values it
/// ignores. [`isolate`] calls it first; `neovide-editor`'s `cursor_animation`, which cannot use this
/// file's server, calls it before `gtk4::init()`. Call it on the main thread while no other thread
/// reads the environment.
pub fn cut_session_bus(test: &str) {
    for var in ["DBUS_SESSION_BUS_ADDRESS", "AT_SPI_BUS_ADDRESS"] {
        match std::env::var_os(var) {
            Some(inherited) if inherited != NO_SESSION_BUS => println!(
                "{test}: ignoring the inherited {var}={}; no session bus",
                inherited.to_string_lossy()
            ),
            _ => {}
        }
    }
    // Rust 2021: sound while no other thread reads the environment (the caller's precondition).
    std::env::set_var("DBUS_SESSION_BUS_ADDRESS", NO_SESSION_BUS);
    std::env::remove_var("AT_SPI_BUS_ADDRESS");
}

/// [`isolate`], then `gtk4::init()`, then a check that GDK really opened this server's display: by
/// name, and by the pid behind every X connection the process now holds. Exits with status 1,
/// stopping the server, on either failure.
pub fn init_gtk(test: &str, screen: &str) -> OwnXServer {
    use gtk4::prelude::DisplayExt;
    let server = isolate(test, screen);
    if let Err(e) = gtk4::init() {
        eprintln!(
            "{test}: GTK could not initialise on its own Xvfb {} ({e})",
            server.display
        );
        server.exit(1);
    }
    match gtk4::gdk::Display::default().map(|d| d.name().to_string()) {
        Some(name) if name == server.display => {}
        other => {
            eprintln!(
                "{test}: GDK opened {other:?}, not its own Xvfb {} -- refusing",
                server.display
            );
            server.exit(1)
        }
    }
    match server.x_connections() {
        (mine, foreign) if mine > 0 && foreign.is_empty() => server,
        (mine, foreign) => {
            eprintln!(
                "{test}: after GTK started, {mine} X connection(s) to its own Xvfb {} (pid {:?}) and \
                 these to another server: {foreign:?} -- refusing",
                server.display,
                server.pid()
            );
            server.exit(1)
        }
    }
}
