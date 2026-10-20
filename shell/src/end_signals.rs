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
//! answered the first signal within [`ANSWER_WITHIN`], or when a second signal arrives while the first is still
//! unanswered or a close is already under way -- the second `Ctrl+C` forces, as in most programs. That hard path
//! runs no teardown: nvim is not ended by its pid, the agent's children are not shut down, and an nvim that was
//! in the state above stays behind, as it does after `SIGKILL` or a crash. One signal answered in time takes the
//! close and nothing else: the watchdog reads its clock before the window's flags, and reads the flags again just
//! before it acts on the bound, so an answer that came in time is never overtaken by a stale read.
//!
//! **Order.** GLib hands a signal only to the sources that exist when it arrives, so [`install`] starts the
//! watchdog and waits until its sources are attached before it adds the window's. A signal in between reaches the
//! watchdog alone and ends the process after [`ANSWER_WITHIN`] -- no worse than the instant end the default action
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
//! terminal still there, a log file or a pipe is left alone.
//!
//! glib 0.22.9 (which gtk4 0.11.4 pulls in) has no `unix_signal_add_local` and no unix signal source, so this calls
//! `g_unix_signal_add_full` and `g_unix_signal_source_new` itself, as `neovide_editor`'s `fd_watch` calls
//! `g_unix_fd_add_full`; the window's closure is wrapped in a [`ThreadGuard`] for the same reason it gives.

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

/// How long the window has to answer the first signal before the watchdog ends the process. A dispatch on a
/// responsive GTK thread takes milliseconds; this is far above that and still short for someone waiting.
pub(crate) const ANSWER_WITHIN: Duration = Duration::from_secs(5);

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

/// What the watchdog knows when it decides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Watch {
    /// Termination signals seen so far.
    pub(crate) seen: u32,
    pub(crate) first: i32,
    pub(crate) last: i32,
    /// The window's listener has run.
    pub(crate) answered: bool,
    /// The window has started its close.
    pub(crate) closing: bool,
    /// Since the first signal.
    pub(crate) elapsed: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Verdict {
    Wait,
    /// `_exit(128 + signal)`, with the reason written to stderr first.
    Exit {
        signal: i32,
        why: &'static str,
    },
}

/// The watchdog's rule (the module doc says why).
pub(crate) fn decide(w: Watch) -> Verdict {
    if w.seen == 0 {
        return Verdict::Wait;
    }
    if w.seen >= 2 && (!w.answered || w.closing) {
        return Verdict::Exit {
            signal: w.last,
            why: "a second termination signal",
        };
    }
    if !w.answered && w.elapsed >= ANSWER_WITHIN {
        return Verdict::Exit {
            signal: w.first,
            why: "the window did not answer the termination signal within 5 s",
        };
    }
    Verdict::Wait
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
            if decide(watch) != Verdict::Wait {
                // Read the flags once more just before acting: an answer that arrived meanwhile withdraws an end
                // by the bound.
                let fresh = Watch {
                    answered: ANSWERED.load(Ordering::SeqCst),
                    closing: CLOSING.load(Ordering::SeqCst),
                    ..watch
                };
                if let Verdict::Exit { signal, why } = decide(fresh) {
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

    fn watch(seen: u32, answered: bool, closing: bool, elapsed_ms: u64) -> Watch {
        Watch {
            seen,
            first: libc::SIGTERM,
            last: if seen >= 2 { libc::SIGINT } else { libc::SIGTERM },
            answered,
            closing,
            elapsed: Duration::from_millis(elapsed_ms),
        }
    }

    #[test]
    fn one_signal_answered_in_time_is_left_to_the_close() {
        assert_eq!(decide(watch(0, false, false, 60_000)), Verdict::Wait);
        assert_eq!(decide(watch(1, false, false, 4_999)), Verdict::Wait);
        // Answered: the close takes as long as it takes (nvim's own end allows 5 s, the agent's teardown more).
        assert_eq!(decide(watch(1, true, true, 60_000)), Verdict::Wait);
        assert_eq!(decide(watch(1, true, false, 60_000)), Verdict::Wait);
    }

    #[test]
    fn a_signal_answered_is_never_ended_by_the_bound_whatever_the_clock_reads() {
        for elapsed_ms in [0, 4_999, 5_000, 5_001, 60_000, u64::MAX / 2] {
            for closing in [false, true] {
                assert_eq!(
                    decide(watch(1, true, closing, elapsed_ms)),
                    Verdict::Wait,
                    "{elapsed_ms} ms"
                );
            }
        }
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

    #[test]
    fn an_unanswered_signal_ends_the_process_after_the_bound_with_its_own_number() {
        let Verdict::Exit { signal, .. } = decide(watch(1, false, false, 5_000)) else {
            panic!("not ended at the bound");
        };
        assert_eq!(signal, libc::SIGTERM);
    }

    #[test]
    fn a_second_signal_forces_unless_the_first_was_answered_and_no_close_began() {
        for (answered, closing) in [(false, false), (true, true), (false, true)] {
            let Verdict::Exit { signal, .. } = decide(watch(2, answered, closing, 10)) else {
                panic!("a second signal did not force (answered {answered}, closing {closing})");
            };
            assert_eq!(signal, libc::SIGINT, "the second signal's own number");
        }
        assert_eq!(decide(watch(2, true, false, 10)), Verdict::Wait);
    }

    /// A pty pair opened close-on-exec: other tests of this binary start processes meanwhile, and a forked child
    /// that inherited the master would keep it open and the slave from hanging up.
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
