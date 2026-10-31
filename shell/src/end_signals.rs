//! The signals that ask a process to end -- `SIGTERM` (a `kill`, a session ending), `SIGINT` (`Ctrl+C` in the
//! terminal Eitri was started from) and `SIGHUP` (that terminal closing) -- end the window through its own close,
//! without the questions a click on the close button would ask.
//!
//! Without a handler each of them ended the process at once, and nothing ended the editor's nvim: it saw its
//! stdin close and began its own exit, which shows its last messages; in an editor only a few rows high they do
//! not fit, nvim stops at its `-- More --` prompt waiting for a key no UI can send any more, refuses `SIGTERM`
//! once it is exiting, and stays, re-parented to init, for good. The window's close ends nvim by the pid it holds
//! and kills it if it has not gone after five seconds (`NeovideEditorPane::shutdown`), so going through the close
//! leaves nothing behind.
//!
//! **Two listeners.** GLib hands a signal to every source that watches it, in whatever main context each is
//! attached to, from its own worker thread. The window's listener runs on the GTK thread (the default context):
//! it marks the signal answered and calls the window's handler, which starts the close. A watchdog thread with a
//! context of its own listens too, so a signal still ends the process when the GTK thread cannot answer (blocked
//! in a call that does not return, for one): [`decide`] ends it with `_exit(128 + signal)` when the window has not
//! answered the first signal within [`ANSWER_WITHIN`](eitri_core::wm::end_signals::ANSWER_WITHIN), or when a second signal arrives while the first is still
//! unanswered or a close is already under way -- the second `Ctrl+C` forces, as in most programs -- or when a
//! close the window started for one answered signal is still running [`CLOSE_WITHIN`](eitri_core::wm::end_signals::CLOSE_WITHIN) after it (a GTK thread that
//! blocked in the close, writing to a pipe whose reader stopped, for one). That hard path runs no teardown: nvim
//! is not ended by its pid, the agent's children are not shut down, and an nvim that was in the state above stays
//! behind, as it does after `SIGKILL` or a crash. One signal answered in time takes the close and nothing else
//! while the close runs within its own bounds: the watchdog reads its clock before the window's flags, and reads
//! the flags again just before it acts on the bound, so an answer that came in time is never overtaken by a stale
//! read.
//!
//! **Order.** GLib hands a signal only to the sources that exist when it arrives, so [`install`] starts the
//! watchdog and waits until its sources are attached before it adds the window's. A signal in between reaches the
//! watchdog alone and ends the process after [`ANSWER_WITHIN`](eitri_core::wm::end_signals::ANSWER_WITHIN) -- no worse than the instant end the default action
//! gave -- and every signal after `install` reaches both. The other order could hand a signal to the window's
//! listener alone, and a GTK thread that then blocked before running it would never end.
//!
//! The reason line the hard path writes never stands between the decision and `_exit`: it is written only if
//! stderr polls writable at once (a pipe whose reader stopped and filled it would block the write for good), and
//! it is far shorter than `PIPE_BUF`, so a pipe that polls writable takes all of it without blocking.
//!
//! **A terminal that has gone.** After `SIGHUP` from a closed terminal, a descriptor still pointing at it fails
//! every write with `EIO`, and `println!` panics on that -- on the GTK thread inside a GLib callback, which aborts
//! the process before the close has ended nvim. So each of descriptors 0, 1 and 2 that is a hung-up terminal is
//! pointed at `/dev/null` first ([`detach_hung_up_terminals`]), by whichever listener sees the signal first; a
//! terminal still there, a log file or a pipe is left alone. A process started with `setsid` or disowned by its
//! shell gets no `SIGHUP` at all, and even a forwarded one comes a little after the hangup, so the watchdog also
//! watches each of those descriptors that is a terminal for the hangup itself ([`watch_hangups`]) and detaches it
//! when its wake runs; one that has already hung up when the watch starts is detached then. What is left is a write on the GTK thread in the same instant as the hangup, before the
//! watchdog has run; a relay pipe for stdout and stderr would close that too, and is not done.
//!
//! glib 0.22.9 (which gtk4 0.11.4 pulls in) has no `unix_signal_add_local` and no unix signal source, so this calls
//! `g_unix_signal_add_full` and `g_unix_signal_source_new` itself, as `neovide_editor`'s `fd_watch` calls
//! `g_unix_fd_add_full`; the window's closure is wrapped in a [`ThreadGuard`] for the same reason it gives.

use eitri_core::wm::end_signals::{decide, Outcome, Watch};

use std::io::Write;
use std::os::fd::RawFd;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use gtk4::glib::{self, ffi, thread_guard::ThreadGuard, translate::*};

/// The signals handled, with the name the log line gives each.
pub(crate) const SIGNALS: [(i32, &str); 3] = [
    (libc::SIGTERM, "SIGTERM"),
    (libc::SIGINT, "SIGINT"),
    (libc::SIGHUP, "SIGHUP"),
];

/// The window's listener has run for a signal.
static ANSWERED: AtomicBool = AtomicBool::new(false);
/// The window has started its close because of a signal ([`mark_closing`]).
static CLOSING: AtomicBool = AtomicBool::new(false);
/// What the watchdog has seen: how many signals, the first and the last, and when the first arrived (ms after
/// [`epoch`]).
static SEEN: AtomicU32 = AtomicU32::new(0);
static FIRST: AtomicI32 = AtomicI32::new(0);
static LAST: AtomicI32 = AtomicI32::new(0);
static FIRST_AT_MS: AtomicU64 = AtomicU64::new(0);

fn epoch() -> Instant {
    static EPOCH: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    *EPOCH.get_or_init(Instant::now)
}

/// The window says its close is under way: from now on a further signal ends the process at once.
pub(crate) fn mark_closing() {
    CLOSING.store(true, Ordering::SeqCst);
}

/// How long ago the first signal arrived; zero before one has, and while the watchdog, on its own thread, has yet to
/// write it down: its signal is then a moment old.
pub(crate) fn since_first_signal() -> Duration {
    since(FIRST_AT_MS.load(Ordering::SeqCst), epoch().elapsed())
}

fn since(first_at_ms: u64, now: Duration) -> Duration {
    match first_at_ms {
        0 => Duration::ZERO,
        at => now.saturating_sub(Duration::from_millis(at)),
    }
}

/// Points each of `fds` that is a terminal which has hung up (its writes fail with `EIO`; `isatty` says so by
/// failing with `EIO` itself) at `/dev/null`, so later writes to it succeed and go nowhere, as they would have
/// gone nowhere anyway. A live terminal and anything that is not a terminal are left as they are.
pub(crate) fn detach_hung_up_terminals(fds: &[RawFd]) {
    for &fd in fds {
        // SAFETY: plain calls on a descriptor number; `isatty` reads, and `dup2` replaces it atomically.
        unsafe {
            if libc::isatty(fd) == 1 {
                continue;
            }
            if std::io::Error::last_os_error().raw_os_error() != Some(libc::EIO) {
                continue;
            }
            let null = libc::open(c"/dev/null".as_ptr(), libc::O_RDWR | libc::O_CLOEXEC);
            if null < 0 {
                continue;
            }
            libc::dup2(null, fd);
            libc::close(null);
        }
    }
}

type Callback = ThreadGuard<Box<dyn FnMut() -> glib::ControlFlow>>;

unsafe extern "C" {
    fn g_unix_signal_add_full(
        priority: std::ffi::c_int,
        signum: std::ffi::c_int,
        handler: ffi::GSourceFunc,
        user_data: ffi::gpointer,
        notify: ffi::GDestroyNotify,
    ) -> std::ffi::c_uint;
    fn g_unix_signal_source_new(signum: std::ffi::c_int) -> *mut ffi::GSource;
    fn g_unix_fd_source_new(fd: std::ffi::c_int, condition: ffi::GIOCondition) -> *mut ffi::GSource;
}

/// A watched terminal hung up: point it at `/dev/null` and stop watching it (once detached it is no terminal
/// any more, and a hung-up descriptor would wake the poll again at once). Runs on the watchdog thread.
unsafe extern "C" fn hung_up(
    fd: std::ffi::c_int,
    _condition: ffi::GIOCondition,
    _data: ffi::gpointer,
) -> ffi::gboolean {
    detach_hung_up_terminals(&[fd]);
    ffi::GFALSE
}

/// Watches each of `fds` that is a terminal now for its hangup, on `context`, and detaches it when it hangs up
/// ([`detach_hung_up_terminals`]), whether or not a `SIGHUP` ever reaches this process: one started with
/// `setsid` or disowned by its shell gets none when its terminal closes. One that has already hung up when this
/// runs is detached here: `isatty` fails on it, so it would get no source, and no hangup event is left to come.
pub(crate) fn watch_hangups(context: &glib::MainContext, fds: &[RawFd]) {
    type FdFunc = unsafe extern "C" fn(std::ffi::c_int, ffi::GIOCondition, ffi::gpointer) -> ffi::gboolean;
    for &fd in fds {
        // SAFETY: `isatty` reads a descriptor number. The source is new, its callback has the signature a unix fd
        // source calls (GLib's own API takes it cast to `GSourceFunc`), it carries no data, and the context it is
        // attached to keeps it, so our reference is dropped at once.
        unsafe {
            if libc::isatty(fd) != 1 {
                detach_hung_up_terminals(&[fd]);
                continue;
            }
            let source = g_unix_fd_source_new(fd, ffi::G_IO_HUP | ffi::G_IO_ERR);
            let callback = std::mem::transmute::<FdFunc, unsafe extern "C" fn(ffi::gpointer) -> ffi::gboolean>(hung_up);
            ffi::g_source_set_callback(source, Some(callback), std::ptr::null_mut(), None);
            ffi::g_source_attach(source, context.as_ptr());
            ffi::g_source_unref(source);
        }
    }
}

unsafe extern "C" fn trampoline(data: ffi::gpointer) -> ffi::gboolean {
    // SAFETY: `data` is the `Box<Callback>` `add_local` leaked, alive until `destroy`.
    let callback = unsafe { &mut *(data as *mut Callback) };
    (callback.get_mut())().into_glib()
}

unsafe extern "C" fn destroy(data: ffi::gpointer) {
    // SAFETY: called once by GLib when the source is destroyed.
    drop(unsafe { Box::from_raw(data as *mut Callback) });
}

/// `install` was called from a thread that does not own the default main context, where the closure (which is
/// not `Send`) would run on the wrong thread.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct NotOwner;

fn add_local(signum: i32, func: impl FnMut() -> glib::ControlFlow + 'static) -> Result<glib::SourceId, NotOwner> {
    if !glib::MainContext::default().is_owner() {
        return Err(NotOwner);
    }
    let callback: Box<Callback> = Box::new(ThreadGuard::new(Box::new(func)));
    // SAFETY: a valid trampoline/destroy pair for the leaked box; the guard keeps the closure on this thread.
    Ok(unsafe {
        from_glib(g_unix_signal_add_full(
            ffi::G_PRIORITY_DEFAULT,
            signum,
            Some(trampoline),
            Box::into_raw(callback) as ffi::gpointer,
            Some(destroy),
        ))
    })
}

/// The watchdog's own sources' callback: `data` carries the signal number. Runs on the watchdog thread.
unsafe extern "C" fn watched(data: ffi::gpointer) -> ffi::gboolean {
    let signum = data as usize as i32;
    if signum == libc::SIGHUP {
        detach_hung_up_terminals(&[0, 1, 2]);
    }
    if SEEN.fetch_add(1, Ordering::SeqCst) == 0 {
        FIRST.store(signum, Ordering::SeqCst);
        FIRST_AT_MS.store(epoch().elapsed().as_millis() as u64, Ordering::SeqCst);
    }
    LAST.store(signum, Ordering::SeqCst);
    ffi::GTRUE
}

/// Writes `bytes` to `fd` only if it polls writable at once, and returns whether it wrote. Never blocks: a full
/// pipe or a stalled terminal is skipped.
pub(crate) fn write_if_writable(fd: RawFd, bytes: &[u8]) -> bool {
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLOUT,
        revents: 0,
    };
    // SAFETY: `poll` on one descriptor with a zero timeout, then a write of a buffer the caller owns.
    unsafe {
        if libc::poll(&mut pfd, 1, 0) != 1 || pfd.revents & libc::POLLOUT == 0 {
            return false;
        }
        libc::write(fd, bytes.as_ptr().cast(), bytes.len()) > 0
    }
}

/// Ends the process now, after one write of why that is skipped if it could block and ignores its own failure.
fn hard_exit(signal: i32, why: &str) -> ! {
    let line = format!(
        "[window] {why}: ending the process now without its close (exit {})\n",
        128 + signal
    );
    write_if_writable(2, line.as_bytes());
    // SAFETY: `_exit` runs nothing else.
    unsafe { libc::_exit(128 + signal) }
}

/// The watchdog thread: its own context, its own sources for [`SIGNALS`], and [`decide`] after each wake. Returns
/// once its sources are attached, so that every source [`install`] adds after it never gets a signal the watchdog
/// does not (GLib hands a signal only to the sources that exist when it arrives).
fn start_watchdog() -> std::io::Result<()> {
    epoch();
    let (attached, wait_attached) = std::sync::mpsc::channel::<()>();
    std::thread::Builder::new().name("end-signals".into()).spawn(move || {
        let context = glib::MainContext::new();
        let Ok(_owner) = context.acquire() else {
            return;
        };
        for (signum, _) in SIGNALS {
            // SAFETY: a new source with the callback above and the signal number as its data (no pointer
            // is dereferenced); attached to this thread's own context, which keeps it, so our reference is
            // dropped at once.
            unsafe {
                let source = g_unix_signal_source_new(signum);
                ffi::g_source_set_callback(source, Some(watched), signum as usize as ffi::gpointer, None);
                ffi::g_source_attach(source, context.as_ptr());
                ffi::g_source_unref(source);
            }
        }
        watch_hangups(&context, &[0, 1, 2]);
        let _ = attached.send(());
        loop {
            let seen = SEEN.load(Ordering::SeqCst);
            if seen == 0 {
                context.iteration(true);
                continue;
            }
            // The clock first, the window's flags after it: a flag read later can only be newer than the clock, so
            // a signal the window answered before this reading is seen answered.
            let now = epoch().elapsed();
            let first_at = Duration::from_millis(FIRST_AT_MS.load(Ordering::SeqCst));
            let watch = Watch {
                seen,
                first: FIRST.load(Ordering::SeqCst),
                last: LAST.load(Ordering::SeqCst),
                answered: ANSWERED.load(Ordering::SeqCst),
                closing: CLOSING.load(Ordering::SeqCst),
                elapsed: now.saturating_sub(first_at),
            };
            if decide(watch) != Outcome::Wait {
                // Read the flags once more just before acting: an answer that arrived meanwhile withdraws an end
                // by the bound.
                let fresh = Watch {
                    answered: ANSWERED.load(Ordering::SeqCst),
                    closing: CLOSING.load(Ordering::SeqCst),
                    ..watch
                };
                if let Outcome::Exit { signal, why } = decide(fresh) {
                    hard_exit(signal, why);
                }
            }
            context.iteration(false);
            std::thread::sleep(Duration::from_millis(50));
        }
    })?;
    wait_attached
        .recv_timeout(Duration::from_secs(5))
        .map_err(|_| std::io::Error::other("its signal sources were not attached within 5 s"))
}

/// Calls `on_signal` on this thread, with the signal's name, each time one of [`SIGNALS`] arrives, from now on for
/// the life of the process, and starts the watchdog. `Err(NotOwner)`, installing nothing, unless this thread owns
/// the default main context (the GTK thread does once the application runs). Call it once.
pub(crate) fn install(on_signal: impl Fn(&'static str) + 'static) -> Result<(), NotOwner> {
    if !glib::MainContext::default().is_owner() {
        return Err(NotOwner);
    }
    // The watchdog's sources first (the module doc's "Order").
    if let Err(err) = start_watchdog() {
        let _ = writeln!(
            std::io::stderr(),
            "[window] the termination watchdog did not start ({err}); a blocked window ignores the signals"
        );
    }
    let on_signal = std::rc::Rc::new(on_signal);
    for (signum, name) in SIGNALS {
        let on_signal = on_signal.clone();
        add_local(signum, move || {
            if signum == libc::SIGHUP {
                detach_hung_up_terminals(&[0, 1, 2]);
            }
            ANSWERED.store(true, Ordering::SeqCst);
            on_signal(name);
            glib::ControlFlow::Continue
        })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsRawFd;

    #[test]
    fn a_signal_not_yet_written_down_is_a_moment_old() {
        let uptime = Duration::from_secs(3600);
        assert_eq!(since(0, uptime), Duration::ZERO, "not the time since the process began");
        assert_eq!(since(3_597_000, uptime), Duration::from_secs(3));
        assert_eq!(
            since(3_700_000, uptime),
            Duration::ZERO,
            "a clock read before the signal"
        );
    }

    #[test]
    fn a_full_pipe_is_not_written_and_does_not_block() {
        let (_read_end, write_end) = std::io::pipe().expect("a pipe");
        let fd = write_end.as_raw_fd();
        // Fill it without blocking, then make it blocking again, as stderr would be.
        // SAFETY: flag changes and writes on a descriptor this test owns.
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFL);
            libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
            let chunk = [0u8; 4096];
            while libc::write(fd, chunk.as_ptr().cast(), chunk.len()) > 0 {}
            libc::fcntl(fd, libc::F_SETFL, flags);
        }
        let (done, wait) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let wrote = write_if_writable(write_end.as_raw_fd(), b"reason\n");
            let _ = done.send(wrote);
            drop(write_end);
        });
        assert_eq!(
            wait.recv_timeout(Duration::from_secs(10)),
            Ok(false),
            "a full blocking pipe is skipped, not written"
        );
        let (_r, w) = std::io::pipe().expect("a second pipe");
        assert!(
            write_if_writable(w.as_raw_fd(), b"reason\n"),
            "an empty pipe takes the line"
        );
    }

    /// A pty pair opened close-on-exec: other tests of this binary start processes meanwhile, and a forked child
    /// that inherited the master would keep it open and the slave from hanging up.
    /// Waits until the terminal `fd` reports its hangup. A child that another test thread forks before it execs
    /// holds a dropped master for a moment, and until every copy of the master is closed the terminal is not hung up.
    fn wait_for_hangup(fd: RawFd) {
        let mut fds = [libc::pollfd {
            fd,
            events: 0,
            revents: 0,
        }];
        // SAFETY: `fds` is one valid `pollfd`.
        let ready = unsafe { libc::poll(fds.as_mut_ptr(), 1, 5000) };
        assert!(
            ready == 1 && fds[0].revents & libc::POLLHUP != 0,
            "the terminal reports the hangup once its master is closed"
        );
    }

    fn openpty() -> (std::fs::File, std::fs::File) {
        // SAFETY: `posix_openpt`, `grantpt`, `unlockpt`, `ptsname_r` and `open` on descriptors and a buffer owned
        // here; each result is checked before it is used.
        unsafe {
            use std::os::fd::FromRawFd;
            let master = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC);
            assert!(master >= 0, "posix_openpt: {}", std::io::Error::last_os_error());
            let master = std::fs::File::from_raw_fd(master);
            assert_eq!(libc::grantpt(master.as_raw_fd()), 0);
            assert_eq!(libc::unlockpt(master.as_raw_fd()), 0);
            let mut name = [0 as libc::c_char; 128];
            assert_eq!(libc::ptsname_r(master.as_raw_fd(), name.as_mut_ptr(), name.len()), 0);
            let slave = libc::open(name.as_ptr(), libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC);
            assert!(slave >= 0, "open the slave: {}", std::io::Error::last_os_error());
            (master, std::fs::File::from_raw_fd(slave))
        }
    }

    #[test]
    fn a_hung_up_terminal_is_pointed_at_dev_null_and_writes_succeed_again() {
        let (master, mut slave) = openpty();
        drop(master);
        wait_for_hangup(slave.as_raw_fd());
        let before = slave.write(b"x");
        assert_eq!(
            before.map_err(|e| e.raw_os_error()),
            Err(Some(libc::EIO)),
            "the control: a write to a terminal whose other end closed fails with EIO"
        );
        detach_hung_up_terminals(&[slave.as_raw_fd()]);
        assert_eq!(slave.write(b"x").expect("a write after the detach"), 1);
        assert_eq!(
            std::fs::read_link(format!("/proc/self/fd/{}", slave.as_raw_fd())).unwrap(),
            std::path::Path::new("/dev/null")
        );
    }

    /// The device a descriptor points at, `(st_dev, st_rdev, st_ino)`.
    fn device_of(fd: RawFd) -> (u64, u64, u64) {
        // SAFETY: `fstat` into a zeroed struct this function owns.
        unsafe {
            let mut st: libc::stat = std::mem::zeroed();
            assert_eq!(libc::fstat(fd, &mut st), 0, "fstat {fd}");
            (st.st_dev as u64, st.st_rdev as u64, st.st_ino as u64)
        }
    }

    #[test]
    fn a_hung_up_terminal_is_detached_without_a_signal() {
        use std::os::fd::FromRawFd;
        let (master, slave) = openpty();
        // SAFETY: a close-on-exec duplicate of a descriptor this test owns, owned by the `File` from here on.
        let mut watched = unsafe {
            let fd = libc::fcntl(slave.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0);
            assert!(fd >= 0, "dup the slave: {}", std::io::Error::last_os_error());
            std::fs::File::from_raw_fd(fd)
        };
        let null = std::fs::File::open("/dev/null").unwrap();
        let null_device = device_of(null.as_raw_fd());
        let context = glib::MainContext::new();
        watch_hangups(&context, &[watched.as_raw_fd()]);
        // Nothing happens while the terminal is there.
        for _ in 0..5 {
            context.iteration(false);
        }
        assert_ne!(
            device_of(watched.as_raw_fd()),
            null_device,
            "a live terminal is left alone"
        );
        drop(master);
        let deadline = Instant::now() + Duration::from_secs(2);
        while device_of(watched.as_raw_fd()) != null_device && Instant::now() < deadline {
            context.iteration(false);
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            device_of(watched.as_raw_fd()),
            null_device,
            "the hung-up terminal is pointed at /dev/null with no signal"
        );
        assert_eq!(watched.write(b"x").expect("a write after the hangup"), 1);
        drop(slave);
    }

    #[test]
    fn a_terminal_already_hung_up_when_the_watch_starts_is_detached() {
        use std::os::fd::FromRawFd;
        let (master, slave) = openpty();
        // SAFETY: a close-on-exec duplicate of a descriptor this test owns, owned by the `File` from here on.
        let mut watched = unsafe {
            let fd = libc::fcntl(slave.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0);
            assert!(fd >= 0, "dup the slave: {}", std::io::Error::last_os_error());
            std::fs::File::from_raw_fd(fd)
        };
        let null = std::fs::File::open("/dev/null").unwrap();
        drop(master);
        wait_for_hangup(watched.as_raw_fd());
        let context = glib::MainContext::new();
        watch_hangups(&context, &[watched.as_raw_fd()]);
        assert_eq!(
            device_of(watched.as_raw_fd()),
            device_of(null.as_raw_fd()),
            "a terminal that hung up before the watch is pointed at /dev/null by the watch itself"
        );
        assert_eq!(watched.write(b"x").expect("a write after the detach"), 1);
        drop(slave);
    }

    #[test]
    fn a_live_terminal_and_a_pipe_are_left_alone() {
        let (_master, slave) = openpty();
        let (mut read_end, mut write_end) = std::io::pipe().expect("a pipe");
        let names = |fds: &[RawFd]| -> Vec<std::path::PathBuf> {
            fds.iter()
                .map(|fd| std::fs::read_link(format!("/proc/self/fd/{fd}")).unwrap())
                .collect()
        };
        let fds = [slave.as_raw_fd(), write_end.as_raw_fd()];
        let before = names(&fds);
        detach_hung_up_terminals(&fds);
        assert_eq!(names(&fds), before);
        write_end.write_all(b"ok").unwrap();
        drop(write_end);
        let mut got = String::new();
        std::io::Read::read_to_string(&mut read_end, &mut got).unwrap();
        assert_eq!(got, "ok");
    }
}
