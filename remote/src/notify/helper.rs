//! The process that posts wakes: `serve` forks it before it opens anything, and it holds nothing of `serve`'s.
//!
//! `serve` holds the tokens of connected phones, the device store's view and its listeners; a bug in an HTTP or TLS
//! path must reach none of that. So [`Helper::spawn`] forks while `serve` has one thread and has not yet taken its lock
//! or opened its record, listeners, control socket or audit log (each would be inherited across the fork, and
//! Landlock checks opens, not descriptors already held). The child ([`run_helper`]) closes every descriptor but 0-2
//! and its end of a `SOCK_SEQPACKET` pair, loads the TLS roots, confines itself ([`sandbox_helper`]: reads beneath the
//! resolver's and the roots' directories only, writes nothing, cannot exec, not dumpable), says it is ready, and then
//! takes one endpoint at a time and answers one status word for it. It ends when `serve`'s end closes (or when
//! `serve`'s main thread ends, through `PR_SET_PDEATHSIG`).
//!
//! In `serve`, [`Helper::wake`] queues an endpoint for a thread of its own that hands it over and waits for the word;
//! at most [`HELPER_QUEUE`] wait, and one more is dropped. A queued wake is asked again whether it is still due just
//! before it is handed over (the phone may have connected, been revoked or named another endpoint while it waited),
//! and one that is not is answered [`NOT_DUE`] without a post. A helper killed for being stuck is reaped.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::net::UnixDatagram;
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use eitri_core::remote_devices::PUSH_ENDPOINT_MAX_BYTES;

use super::post::{own_addresses, post_wake, system_resolve, POST_TIMEOUT};
use crate::serve::sandbox::sandbox_helper;

/// The most wakes waiting for the helper; one more is dropped.
pub const HELPER_QUEUE: usize = 32;
/// How long `serve` waits for the helper to say it is ready.
const READY_WITHIN: Duration = Duration::from_secs(10);
/// How long a post may take before the helper is taken for stuck and ended: the post's own bound and some.
const ANSWER_WITHIN: Duration = POST_TIMEOUT.saturating_add(Duration::from_secs(20));
/// The longest status word the helper sends.
const STATUS_MAX: usize = 24;
/// The word for a queued wake that was no longer due when its turn came: nothing was posted.
pub const NOT_DUE: &str = "not_due";

/// What the helper said once it was confined.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ready {
    /// `full` or `partial`, as `serve`'s own sandbox is named.
    pub sandbox: String,
    /// How many trust roots it loaded.
    pub roots: usize,
    /// The descriptors it held then.
    pub fds: Vec<RawFd>,
}

/// One endpoint for the helper, whether it is still due, and what to do with its status word.
struct Job {
    endpoint: String,
    still_due: Box<dyn Fn() -> bool + Send>,
    done: Box<dyn FnOnce(String) + Send>,
}

/// `serve`'s end of the helper.
pub struct Helper {
    sock: Arc<UnixDatagram>,
    pid: libc::pid_t,
    ready: Ready,
    queue: Mutex<Option<SyncSender<Job>>>,
}

impl Helper {
    /// Forks the helper and waits until it is confined. Refused while a second thread runs: only a process with one
    /// thread can fork and go on safely in the child.
    pub fn spawn() -> io::Result<Helper> {
        let threads = std::fs::read_dir("/proc/self/task")?.count();
        if threads != 1 {
            return Err(io::Error::other(format!(
                "the helper is forked while serve has one thread, and {threads} threads run"
            )));
        }
        let mut pair = [0 as RawFd; 2];
        // SAFETY: `pair` has room for the two descriptors `socketpair` writes.
        if unsafe {
            libc::socketpair(
                libc::AF_UNIX,
                libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
                0,
                pair.as_mut_ptr(),
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: both descriptors were just made by `socketpair` and are owned by nothing else.
        let (ours, theirs) = unsafe { (UnixDatagram::from_raw_fd(pair[0]), UnixDatagram::from_raw_fd(pair[1])) };
        // SAFETY: this process has one thread (checked above), so the child may run ordinary code after the fork.
        match unsafe { libc::fork() } {
            -1 => Err(io::Error::last_os_error()),
            0 => {
                drop(ours);
                run_helper(theirs)
            }
            pid => {
                drop(theirs);
                let ready = read_ready(&ours, pid)?;
                Ok(Helper {
                    sock: Arc::new(ours),
                    pid,
                    ready,
                    queue: Mutex::new(None),
                })
            }
        }
    }

    /// The helper's process id.
    pub fn pid(&self) -> libc::pid_t {
        self.pid
    }

    /// What the helper said when it was ready.
    pub fn ready(&self) -> &Ready {
        &self.ready
    }

    /// Queues one wake to `endpoint`; `done` gets the status word (the HTTP status, or `refused`, `network`,
    /// `timeout`, `helper_gone`, [`NOT_DUE`] when `still_due` says no once its turn comes) from the helper's thread.
    /// `Err` when [`HELPER_QUEUE`] wakes wait already (the wake is dropped) or the helper is gone; `done` is then not
    /// called.
    pub fn wake(
        &self,
        endpoint: &str,
        still_due: impl Fn() -> bool + Send + 'static,
        done: impl FnOnce(String) + Send + 'static,
    ) -> io::Result<()> {
        if endpoint.len() > PUSH_ENDPOINT_MAX_BYTES {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "the endpoint is too long"));
        }
        let mut queue = self.queue.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if queue.is_none() {
            let (tx, rx) = mpsc::sync_channel(HELPER_QUEUE);
            let (sock, pid) = (self.sock.clone(), self.pid);
            std::thread::Builder::new()
                .name("eitri-remote-wake".into())
                .spawn(move || hand_over(&sock, pid, rx))?;
            *queue = Some(tx);
        }
        let job = Job {
            endpoint: endpoint.to_owned(),
            still_due: Box::new(still_due),
            done: Box::new(done),
        };
        match queue.as_ref().map(|tx| tx.try_send(job)) {
            Some(Ok(())) => Ok(()),
            Some(Err(TrySendError::Full(_))) => Err(io::Error::new(io::ErrorKind::WouldBlock, "dropped")),
            Some(Err(TrySendError::Disconnected(_))) | None => {
                Err(io::Error::new(io::ErrorKind::BrokenPipe, "the helper is gone"))
            }
        }
    }
}

impl Drop for Helper {
    /// Closes `serve`'s end for both directions, which the helper reads as its cue to end.
    fn drop(&mut self) {
        let _ = self.sock.shutdown(std::net::Shutdown::Both);
    }
}

/// Reads the helper's first message: `ready <sandbox> <roots> <fd>,<fd>,...`, or `failed <why>`.
fn read_ready(sock: &UnixDatagram, pid: libc::pid_t) -> io::Result<Ready> {
    sock.set_read_timeout(Some(READY_WITHIN))?;
    let mut buf = [0u8; 1024];
    let got = sock.recv(&mut buf);
    let refused = |why: String| {
        end_child(pid);
        Err(io::Error::other(why))
    };
    let n = match got {
        Ok(0) => return refused("the helper ended before it was ready".to_owned()),
        Ok(n) => n,
        Err(e) => return refused(format!("the helper did not say it was ready: {e}")),
    };
    sock.set_read_timeout(None)?;
    let text = String::from_utf8_lossy(&buf[..n]).into_owned();
    let mut words = text.split(' ');
    match (words.next(), words.next(), words.next(), words.next()) {
        (Some("ready"), Some(sandbox), Some(roots), Some(fds)) => Ok(Ready {
            sandbox: sandbox.to_owned(),
            roots: roots.parse().unwrap_or(0),
            fds: fds.split(',').filter_map(|fd| fd.parse().ok()).collect(),
        }),
        _ => refused(format!("the helper could not start: {text}")),
    }
}

/// `serve`'s thread: hands each job's endpoint to the helper, one at a time, and gives `done` the word it answers.
fn hand_over(sock: &UnixDatagram, pid: libc::pid_t, jobs: Receiver<Job>) {
    let _ = sock.set_read_timeout(Some(ANSWER_WITHIN));
    let mut gone = false;
    for job in jobs {
        if !(job.still_due)() {
            (job.done)(NOT_DUE.to_owned());
            continue;
        }
        if gone {
            (job.done)("helper_gone".to_owned());
            continue;
        }
        let mut buf = [0u8; 64];
        let word = match sock.send(job.endpoint.as_bytes()).and_then(|_| sock.recv(&mut buf)) {
            Ok(n) if n > 0 => status_word(&buf[..n]),
            Ok(_) => {
                gone = true;
                "helper_gone".to_owned()
            }
            Err(_) => {
                // Stuck in a post past its own bound, or gone: either way no answer can be paired with a wake now.
                end_child(pid);
                gone = true;
                "helper_gone".to_owned()
            }
        };
        (job.done)(word);
    }
}

/// Kills the helper `pid` and reaps it, so no zombie stays for `serve`'s life. A pid that is not a process's own
/// (0 or below) is never signalled.
fn end_child(pid: libc::pid_t) {
    if pid <= 0 {
        return;
    }
    // SAFETY: `kill` and `waitpid` take no pointers but a null status; `pid` is this process's own child.
    unsafe {
        libc::kill(pid, libc::SIGKILL);
        libc::waitpid(pid, std::ptr::null_mut(), 0);
    }
}

/// The helper's answer as a word for the log: lowercase letters, digits and `_`, bounded.
fn status_word(bytes: &[u8]) -> String {
    let word: String = bytes
        .iter()
        .take(STATUS_MAX)
        .map(|b| match b {
            b'a'..=b'z' | b'0'..=b'9' | b'_' => *b as char,
            _ => '_',
        })
        .collect();
    if word.is_empty() {
        "helper_gone".to_owned()
    } else {
        word
    }
}

/// The forked child: never returns. Holds 0-2 and `sock` only, confines itself, then posts one wake for each
/// endpoint `serve` sends and answers its status word, until `serve`'s end closes.
pub fn run_helper(sock: UnixDatagram) -> ! {
    let code = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| helper_main(&sock))).unwrap_or(70);
    // SAFETY: ends this process at once, without running `serve`'s exit handlers or flushing buffers it inherited.
    unsafe { libc::_exit(code) }
}

fn helper_main(sock: &UnixDatagram) -> i32 {
    // SAFETY: `prctl` with integer arguments only.
    unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL as libc::c_ulong, 0, 0, 0) };
    close_others(sock.as_raw_fd());
    let fds = open_fds();
    let roots = super::post::load_roots();
    let state = match sandbox_helper() {
        Ok(state) => state,
        Err(e) => {
            let _ = sock.send(format!("failed {e}").as_bytes());
            return 1;
        }
    };
    let fds: Vec<String> = fds.iter().map(|fd| fd.to_string()).collect();
    if sock
        .send(format!("ready {} {roots} {}", state.word(), fds.join(",")).as_bytes())
        .is_err()
    {
        return 0;
    }
    let mut buf = vec![0u8; PUSH_ENDPOINT_MAX_BYTES + 1];
    loop {
        let n = match sock.recv(&mut buf) {
            Ok(0) | Err(_) => return 0,
            Ok(n) => n,
        };
        let word = match std::str::from_utf8(&buf[..n]) {
            Ok(endpoint) => match post_wake(endpoint, &system_resolve, &own_addresses) {
                Ok(code) => code.to_string(),
                Err(e) => e.word(),
            },
            Err(_) => "refused".to_owned(),
        };
        if sock.send(word.as_bytes()).is_err() {
            return 0;
        }
    }
}

/// Closes every descriptor but 0-2 and `keep`: `close_range` around it, or one by one where the kernel has none.
pub fn close_others(keep: RawFd) {
    let range = |first: u32, last: u32| {
        // SAFETY: `close_range` takes two descriptor numbers and flags; it touches no memory.
        first > last || unsafe { libc::syscall(libc::SYS_close_range, first, last, 0u32) } == 0
    };
    let keep_n = keep.max(0) as u32;
    let done = if keep_n >= 3 {
        range(3, keep_n - 1) && range(keep_n + 1, u32::MAX)
    } else {
        range(3, u32::MAX)
    };
    if !done {
        for fd in open_fds() {
            if fd > 2 && fd != keep {
                // SAFETY: closes a descriptor this process holds and no Rust value owns past this point.
                unsafe { libc::close(fd) };
            }
        }
    }
}

/// The descriptors this process holds, from `/proc/self/fd`, without the one used to read it.
pub fn open_fds() -> Vec<RawFd> {
    let own = format!("/proc/{}/fd", std::process::id());
    let Ok(entries) = std::fs::read_dir("/proc/self/fd") else {
        return Vec::new();
    };
    let mut fds: Vec<RawFd> = entries
        .filter_map(Result::ok)
        .filter(|entry| std::fs::read_link(entry.path()).map_or(true, |target| target.as_os_str() != own.as_str()))
        .filter_map(|entry| entry.file_name().to_str()?.parse().ok())
        .collect();
    fds.sort_unstable();
    fds
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_words_are_safe_for_the_log() {
        assert_eq!(status_word(b"201"), "201");
        assert_eq!(status_word(b"timeout"), "timeout");
        assert_eq!(status_word(b"\x1b[31mred"), "__31mred");
        assert_eq!(status_word(&[b'a'; 100]).len(), STATUS_MAX);
        assert_eq!(status_word(b""), "helper_gone");
    }

    /// A `SOCK_SEQPACKET` pair, as `Helper::spawn` makes.
    fn pair() -> (UnixDatagram, UnixDatagram) {
        let mut fds = [0 as RawFd; 2];
        // SAFETY: `fds` has room for the two descriptors `socketpair` writes.
        let made = unsafe {
            libc::socketpair(
                libc::AF_UNIX,
                libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
                0,
                fds.as_mut_ptr(),
            )
        };
        assert_eq!(made, 0, "{}", io::Error::last_os_error());
        // SAFETY: both descriptors were just made and are owned by nothing else.
        unsafe { (UnixDatagram::from_raw_fd(fds[0]), UnixDatagram::from_raw_fd(fds[1])) }
    }

    #[test]
    fn a_wake_no_longer_due_is_not_handed_over() {
        let (ours, theirs) = pair();
        // Stands in for the helper: answers each endpoint it is given until the other end closes.
        let peer = std::thread::spawn(move || {
            let mut seen = Vec::new();
            let mut buf = [0u8; 256];
            while let Ok(n) = theirs.recv(&mut buf) {
                if n == 0 {
                    break;
                }
                seen.push(String::from_utf8_lossy(&buf[..n]).into_owned());
                if theirs.send(b"200").is_err() {
                    break;
                }
            }
            seen
        });
        let words = Arc::new(Mutex::new(Vec::new()));
        let (tx, rx) = mpsc::sync_channel(HELPER_QUEUE);
        for (endpoint, due) in [("https://push.example/a", false), ("https://push.example/b", true)] {
            let words = words.clone();
            tx.send(Job {
                endpoint: endpoint.to_owned(),
                still_due: Box::new(move || due),
                done: Box::new(move |word| words.lock().unwrap().push(word)),
            })
            .unwrap();
        }
        drop(tx);
        // No process stands behind the pair: a pid of 0 is never signalled.
        hand_over(&ours, 0, rx);
        ours.shutdown(std::net::Shutdown::Both).unwrap();
        assert_eq!(peer.join().unwrap(), ["https://push.example/b"]);
        assert_eq!(*words.lock().unwrap(), [NOT_DUE, "200"]);
    }

    #[test]
    fn the_helper_is_not_forked_beside_other_threads() {
        // libtest runs this beside its own threads.
        let err = Helper::spawn().err().expect("forked beside other threads");
        assert!(err.to_string().contains("one thread"), "{err}");
    }
}
