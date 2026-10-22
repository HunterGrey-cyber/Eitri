//! msgpack-RPC requests to nvim made off the GTK thread, written in the order they were made, and a
//! watch on whether nvim's loop is still answering while the window's quit is outstanding.
//!
//! **Why a request and not keys.** `NeovideEditorPane::send_keys` goes through `nvim_input`, and
//! typed keys are read as whatever nvim is waiting for: after `f`, inside `getchar()`, after an
//! insert-mode `Ctrl-V`, the first key of a command becomes that one character and the rest runs as
//! Normal-mode commands (`eitri_core::layout::kill::editor_quit_lua`'s doc has the measurement). A
//! request types nothing. It is what stock Neovide's own window close sends, too: `ParallelCommand::
//! Quit` runs `exit_handler.lua` through `nvim_exec_lua`.
//!
//! **Why a line of calls on a thread of its own.** The fork's `LiveHarness` has no API that runs Lua
//! and exposes its tokio runtime to nobody; it does hand out the connection
//! (`LiveHarness::neovim_handler`, "the same escape hatch real Neovide's own ... `RouteWindow`
//! exposes"). A request whose Lua opens a dialog is not answered until the user answers the dialog,
//! so nobody may wait for an answer on the GTK thread. Each session has one [`CallLine`]: a thread
//! that polls every call's future, oldest first, each time any of them can move. nvim-rs's future
//! only writes to nvim's stdin (a tokio pipe, which remembers the reactor that registered it, so
//! polling it from here is fine while the fork's runtime runs) and waits on a `futures` oneshot the
//! fork's IO task completes, matched to its request by msgid. So the calls reach nvim in the order
//! they were made, and a later one is written at once rather than held behind an earlier one's
//! answer -- with one gap. In the first poll of its future nvim-rs 0.9.2 (`send_msg`) takes the
//! request's msgid, then two `futures` locks of its connection in turn: the queue of callers waiting
//! for answers, which the IO loop also holds while it hands each answer over, and the write lock,
//! which the fork's own writes (a key, a resize) share. It writes the request only once it holds
//! the second. When either lock is held by someone else at that instant, the request waits for it;
//! a `futures` lock is not handed to the waiter it wakes but taken by whoever polls next, so if it
//! is freed in the instant between this call's poll and a later call's first poll on the same pass,
//! the later call is written first (with the higher msgid, so nvim sees them swapped). nvim-rs hides
//! both locks and the moment a request is written, so that instant cannot be closed from outside
//! it: only a writer of our own, which the fork's connection does not offer, would close it. It is
//! microseconds wide and needs another writer's lock release to land inside it; a caller whose
//! outcome depends on two calls' order makes the later one harmless to run first (the plugin
//! host's assist calls carry the order they were made in, and the snippet skips a call older than
//! one it already ran). **The future
//! is never dropped before it resolves**: nvim-rs's IO loop returns an error, ending the whole
//! connection, when a response arrives for a request whose receiver is gone (`io_loop`'s
//! `sender.send(..) .map_err(..)?`, nvim-rs 0.9.2 `src/neovim.rs`). A caller that stopped waiting
//! changes nothing: the call stays on the line, costing no thread of its own, until nvim answers.
//! nvim exiting resolves it: the IO loop hands every caller still waiting an error when the pipe
//! closes.
//!
//! **Why a watch.** Whether nvim can still be asked anything is not whether it drew lately: its
//! `:confirm qall` dialog draws once and then waits for the user, drawing nothing, as a hung nvim
//! also draws nothing. `nvim_get_mode` is one of nvim's fast calls, answered straight from its event
//! loop even while a dialog is up or a key is awaited, and never while that loop is stuck (a Lua
//! busy loop, a stopped process). Measured on nvim 0.12.5: dialog `r?`, a pending `f` `n`
//! blocking, a hit-enter prompt `r` blocking, `vim.wait()` `n`; SIGSTOP and a Lua busy loop, no
//! answer at all. So while the request is outstanding a second thread asks it every
//! [`HEARTBEAT`], and [`CallWatch`] reports when it last answered and what it said.

use std::future::Future;
use std::io;
use std::pin::{pin, Pin};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, TryRecvError};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Wake, Waker};
use std::thread::Thread;
use std::time::{Duration, Instant};

/// How often the watch asks `nvim_get_mode` while a request is outstanding.
pub(crate) const HEARTBEAT: Duration = Duration::from_millis(250);

/// What nvim last said to `nvim_get_mode`, and when.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NvimMode {
    pub at: Instant,
    /// `nvim_get_mode`'s `mode`: `r?` while a `:confirm` dialog is up.
    pub mode: String,
    /// `nvim_get_mode`'s `blocking`: nvim waits for a key before it takes a request (after `f`, at a
    /// hit-enter prompt).
    pub blocking: bool,
}

/// A watched request, as it stands ([`crate::NeovideEditorPane::watched_call`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallWatch {
    pub sent_at: Instant,
    /// The request came back: its Lua returned, or nvim went away.
    pub done: bool,
    /// Whether nvim answered the request after running it. For `:confirm qall`, an answer means
    /// nvim did not quit: a successful quit ends the process before sending an answer.
    /// The host can therefore clear a pending quit. `false` while outstanding or when the request
    /// ended because nvim exited, distinguishing an exit from a cancelled quit.
    pub returned: bool,
    /// The last `nvim_get_mode` answer since it was sent; `None` before the first one.
    pub last_answer: Option<NvimMode>,
}

impl CallWatch {
    /// When nvim's loop was last known to run: its last answer, or the send.
    pub fn last_alive(&self) -> Instant {
        self.last_answer.as_ref().map_or(self.sent_at, |answer| answer.at)
    }
}

struct Shared {
    sent_at: Instant,
    done: AtomicBool,
    returned: AtomicBool,
    /// Set when the [`WatchedCall`] is dropped: the watch stops asking (the request itself cannot be
    /// abandoned, see the module doc).
    stopped: AtomicBool,
    last_answer: Mutex<Option<NvimMode>>,
}

/// A request on a [`CallLine`], watched by a thread of its own until it is done or this is dropped.
pub(crate) struct WatchedCall {
    shared: Arc<Shared>,
}

impl WatchedCall {
    /// Hands `call` to `line`, behind every call handed to it before, and, until it completes, runs
    /// `probe()` every [`HEARTBEAT`] on a thread of its own. `call` resolves to whether nvim answered
    /// it ([`CallWatch::returned`]). `probe` resolves to `nvim_get_mode`'s `(mode, blocking)`, or
    /// `None` when nvim is gone, which ends the watch. An `Err` sent nothing: the watch's thread
    /// could not start, or the line's thread is gone.
    pub(crate) fn start<CF, P, PF>(line: &CallLine, name: &str, call: CF, mut probe: P) -> io::Result<WatchedCall>
    where
        CF: Future<Output = bool> + Send + 'static,
        P: FnMut() -> PF + Send + 'static,
        PF: Future<Output = Option<(String, bool)>>,
    {
        let shared = Arc::new(Shared {
            sent_at: Instant::now(),
            done: AtomicBool::new(false),
            returned: AtomicBool::new(false),
            stopped: AtomicBool::new(false),
            last_answer: Mutex::new(None),
        });
        let watched = WatchedCall { shared: shared.clone() };
        {
            let shared = shared.clone();
            std::thread::Builder::new()
                .name(format!("nvim-rpc {name} watch"))
                .spawn(move || {
                    while !shared.done.load(Ordering::SeqCst) && !shared.stopped.load(Ordering::SeqCst) {
                        let Some((mode, blocking)) = block_on(probe()) else {
                            break;
                        };
                        *shared.last_answer.lock().unwrap_or_else(|e| e.into_inner()) = Some(NvimMode {
                            at: Instant::now(),
                            mode,
                            blocking,
                        });
                        std::thread::sleep(HEARTBEAT);
                    }
                })?;
        }
        // The watch first: a call handed over is sent, so nothing may fail after it.
        let handed = line.push(Box::pin(async move {
            let answered = call.await;
            // `returned` first: a reader that sees `done` then sees how it ended.
            shared.returned.store(answered, Ordering::SeqCst);
            shared.done.store(true, Ordering::SeqCst);
        }));
        if !handed {
            // Dropping `watched` stops the watch.
            return Err(io::Error::other("the call line's thread is gone"));
        }
        Ok(watched)
    }

    pub(crate) fn watch(&self) -> CallWatch {
        CallWatch {
            sent_at: self.shared.sent_at,
            done: self.shared.done.load(Ordering::SeqCst),
            returned: self.shared.returned.load(Ordering::SeqCst),
            last_answer: self
                .shared
                .last_answer
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
        }
    }
}

impl WatchedCall {
    /// Waits up to `within` for the request's worker to publish how it ended, then returns the watch.
    /// Nvim-rs delivers an answer or a closed-pipe error through a oneshot. Nvim's exit can reach the
    /// host before the worker publishes it. Waiting here lets an exit handler read the completed
    /// result; a request still outstanding at the bound returns with `done == false`.
    pub(crate) fn settled(&self, within: Duration) -> CallWatch {
        let deadline = Instant::now() + within;
        loop {
            let watch = self.watch();
            if watch.done || Instant::now() >= deadline {
                return watch;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}

impl Drop for WatchedCall {
    fn drop(&mut self) {
        self.shared.stopped.store(true, Ordering::SeqCst);
    }
}

/// A call on a [`CallLine`]: the request's future together with what is done with its answer.
type LineCall = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

/// One nvim's calls, written in the order they were handed over: a single thread polls them all,
/// oldest first, until each resolves (the module doc). The thread ends once this is dropped and
/// every call it holds has resolved; nvim exiting resolves them all.
pub(crate) struct CallLine {
    /// `None` once dropped.
    calls: Option<mpsc::Sender<LineCall>>,
    worker: Thread,
}

impl CallLine {
    pub(crate) fn start(name: &str) -> io::Result<CallLine> {
        let (calls, handed) = mpsc::channel();
        let worker = std::thread::Builder::new()
            .name(format!("nvim-rpc {name}"))
            .spawn(move || drive(handed))?
            .thread()
            .clone();
        Ok(CallLine {
            calls: Some(calls),
            worker,
        })
    }

    /// Hands `call` to the line, behind every call handed over before it; `reply` gets its output
    /// on the line's thread. The future is polled until it resolves, whoever still wants the
    /// answer: dropping an nvim-rs request before its response arrives ends the whole connection
    /// (the module doc), so a caller that stopped caring changes nothing here; `reply` simply
    /// delivers to nobody. `false` when the line's thread is gone: `call` was never polled -- and
    /// nvim-rs writes a request only when its future is first polled, so nothing was sent and no
    /// response can come back for it -- and `reply` is dropped uncalled.
    pub(crate) fn submit<F>(&self, call: F, reply: crate::LuaReply) -> bool
    where
        F: Future<Output = Result<rmpv::Value, String>> + Send + 'static,
    {
        self.push(Box::pin(async move { reply(call.await) }))
    }

    fn push(&self, call: LineCall) -> bool {
        let Some(calls) = &self.calls else {
            return false;
        };
        if calls.send(call).is_err() {
            return false;
        }
        self.worker.unpark();
        true
    }
}

impl Drop for CallLine {
    fn drop(&mut self) {
        // The sender goes before the wake, so the woken thread sees that nothing more will come.
        drop(self.calls.take());
        self.worker.unpark();
    }
}

/// The line's thread: takes what was handed over, polls every call oldest first, and parks until a
/// call can move or another arrives.
fn drive(handed: mpsc::Receiver<LineCall>) {
    let waker = Waker::from(Arc::new(Unpark(std::thread::current())));
    let mut cx = Context::from_waker(&waker);
    let mut outstanding: Vec<LineCall> = Vec::new();
    let mut open = true;
    loop {
        // Taken before the pass, so a call handed over during it waits for the next pass and is
        // polled after every older one.
        while open {
            match handed.try_recv() {
                Ok(call) => outstanding.push(call),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => open = false,
            }
        }
        outstanding.retain_mut(|call| call.as_mut().poll(&mut cx).is_pending());
        if !open && outstanding.is_empty() {
            return;
        }
        // A wake or a hand-over since the pass began leaves a token, so this returns at once.
        std::thread::park();
    }
}

/// Wakes the thread that is blocked in [`block_on`].
struct Unpark(Thread);

impl Wake for Unpark {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.unpark();
    }
}

/// Drives `future` to completion on this thread, parking between polls. No runtime: nvim-rs's
/// request future needs only a waker (the module doc).
pub(crate) fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = pin!(future);
    let waker = Waker::from(Arc::new(Unpark(std::thread::current())));
    let mut cx = Context::from_waker(&waker);
    loop {
        if let Poll::Ready(output) = future.as_mut().poll(&mut cx) {
            return output;
        }
        std::thread::park();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wait_until(what: &str, mut condition: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !condition() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn line(name: &str) -> CallLine {
        CallLine::start(name).expect("the line's thread")
    }

    /// How many threads of this process carry `name` (the kernel keeps its first 15 bytes).
    #[cfg(target_os = "linux")]
    fn threads_named(name: &str) -> usize {
        let want = &name.as_bytes()[..name.len().min(15)];
        std::fs::read_dir("/proc/self/task")
            .expect("this process's threads")
            .filter_map(Result::ok)
            .filter_map(|task| std::fs::read(task.path().join("comm")).ok())
            .filter(|comm| comm.strip_suffix(b"\n").unwrap_or(comm) == want)
            .count()
    }

    #[test]
    fn the_line_delivers_the_value() {
        let (tx, rx) = std::sync::mpsc::channel();
        let handed = line("test").submit(
            async { Ok(rmpv::Value::from(3)) },
            Box::new(move |result| {
                let _ = tx.send(result);
            }),
        );
        assert!(handed);
        assert_eq!(rx.recv_timeout(Duration::from_secs(5)), Ok(Ok(rmpv::Value::from(3))));
    }

    /// Nobody waits for the answer any more, and the call still runs to its end: the future is
    /// never dropped half way.
    #[test]
    fn the_line_runs_the_future_to_its_end_after_the_receiver_is_gone() {
        let (tx, rx) = std::sync::mpsc::channel::<Result<rmpv::Value, String>>();
        drop(rx);
        let release = Arc::new(AtomicBool::new(false));
        let finished = Arc::new(AtomicBool::new(false));
        let replied = Arc::new(AtomicBool::new(false));
        let call = {
            let (release, finished) = (release.clone(), finished.clone());
            async move {
                while !release.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(5));
                }
                finished.store(true, Ordering::SeqCst);
                Ok(rmpv::Value::Nil)
            }
        };
        let reply = {
            let replied = replied.clone();
            Box::new(move |result| {
                replied.store(true, Ordering::SeqCst);
                let _ = tx.send(result);
            })
        };
        assert!(line("test").submit(call, reply));
        std::thread::sleep(Duration::from_millis(30));
        assert!(!finished.load(Ordering::SeqCst), "still waiting for its release");
        release.store(true, Ordering::SeqCst);
        wait_until("the future to finish", || finished.load(Ordering::SeqCst));
        wait_until("the reply", || replied.load(Ordering::SeqCst));
    }

    #[test]
    fn the_line_reports_an_error_as_err() {
        let (tx, rx) = std::sync::mpsc::channel();
        let handed = line("test").submit(
            async { Err("boom".to_owned()) },
            Box::new(move |result| {
                let _ = tx.send(result);
            }),
        );
        assert!(handed);
        assert_eq!(rx.recv_timeout(Duration::from_secs(5)), Ok(Err("boom".to_owned())));
    }

    /// Dropping the line ends nothing still outstanding; its thread ends once the last call has.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_line_thread_ends_once_dropped_and_its_last_call_is_done() {
        let release = Arc::new(AtomicBool::new(false));
        let call = {
            let release = release.clone();
            async move {
                while !release.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Ok(rmpv::Value::Nil)
            }
        };
        let calls = line("line-ends");
        assert!(calls.submit(call, Box::new(|_| {})));
        drop(calls);
        std::thread::sleep(Duration::from_millis(30));
        assert_eq!(threads_named("nvim-rpc line-ends"), 1, "still carrying its call");
        release.store(true, Ordering::SeqCst);
        wait_until("the line's thread to end", || threads_named("nvim-rpc line-ends") == 0);
    }

    /// A future that is pending until another thread wakes it: `block_on` parks, and the wake
    /// resumes it.
    #[test]
    fn block_on_resumes_on_a_wake_from_another_thread() {
        let slot: Arc<Mutex<(Option<u32>, Option<Waker>)>> = Arc::default();
        {
            let slot = slot.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(50));
                let mut slot = slot.lock().unwrap();
                slot.0 = Some(7);
                if let Some(waker) = slot.1.take() {
                    waker.wake();
                }
            });
        }
        let got = block_on(std::future::poll_fn(|cx| {
            let mut slot = slot.lock().unwrap();
            match slot.0 {
                Some(value) => Poll::Ready(value),
                None => {
                    slot.1 = Some(cx.waker().clone());
                    Poll::Pending
                }
            }
        }));
        assert_eq!(got, 7);
    }

    /// The watch reports the probe's answers while the call is outstanding, stops asking once the
    /// call is done, and a probe that never comes back leaves `last_alive` where it was.
    #[test]
    fn the_watch_follows_the_probe_until_the_call_is_done() {
        let release = Arc::new(AtomicBool::new(false));
        let probes = Arc::new(Mutex::new(0u32));
        let call = {
            let release = release.clone();
            async move {
                while !release.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(5));
                }
                true
            }
        };
        let probe = {
            let probes = probes.clone();
            move || {
                *probes.lock().unwrap() += 1;
                async { Some(("r?".to_string(), false)) }
            }
        };
        let watched = WatchedCall::start(&line("test"), "test", call, probe).unwrap();
        wait_until("a first answer", || watched.watch().last_answer.is_some());
        let watch = watched.watch();
        assert!(!watch.done);
        assert!(!watch.returned, "not back yet");
        assert_eq!(watch.last_answer.as_ref().map(|a| a.mode.as_str()), Some("r?"));
        assert!(watch.last_alive() >= watch.sent_at);

        release.store(true, Ordering::SeqCst);
        wait_until("the call to finish", || watched.watch().done);
        assert!(watched.watch().returned, "it came back with an answer");
        std::thread::sleep(HEARTBEAT * 2);
        let settled = *probes.lock().unwrap();
        std::thread::sleep(HEARTBEAT * 3);
        assert_eq!(*probes.lock().unwrap(), settled, "no probe after the call is done");
    }

    /// A probe that never resolves (nvim's loop is stuck): no answer is ever recorded, so the watch
    /// reports nvim alive only as of the send.
    #[test]
    fn a_probe_that_never_answers_leaves_the_last_sign_of_life_at_the_send() {
        let call = std::future::pending::<bool>();
        let probe = || std::future::pending::<Option<(String, bool)>>();
        let watched = WatchedCall::start(&line("stuck"), "stuck", call, probe).unwrap();
        std::thread::sleep(HEARTBEAT * 2);
        let watch = watched.watch();
        assert!(!watch.done);
        assert!(!watch.returned);
        assert_eq!(watch.last_answer, None);
        assert_eq!(watch.last_alive(), watch.sent_at);
    }

    /// Nvim's answer reaches the worker through a oneshot before the worker publishes it.
    /// `settled` waits up to its bound for publication so a caller at nvim's exit can read the result.
    /// A request still outstanding at the bound remains outstanding.
    #[test]
    fn settled_waits_for_the_worker_to_publish() {
        let call = async {
            std::thread::sleep(Duration::from_millis(150));
            true
        };
        let probe = || async { None::<(String, bool)> };
        let watched = WatchedCall::start(&line("late"), "late", call, probe).unwrap();
        assert!(!watched.watch().done, "the case: not published yet");
        let settled = watched.settled(Duration::from_secs(2));
        assert!(settled.done && settled.returned, "{settled:?}");

        let pending = WatchedCall::start(&line("never"), "never", std::future::pending::<bool>(), probe).unwrap();
        let started = Instant::now();
        let unsettled = pending.settled(Duration::from_millis(100));
        assert!(!unsettled.done);
        assert!(started.elapsed() >= Duration::from_millis(100), "it waited the bound");
    }

    /// A request ended by nvim exiting without an answer is `done` but not `returned`.
    /// The host must distinguish that exit from a cancelled quit.
    #[test]
    fn a_call_that_ends_without_an_answer_is_done_but_not_returned() {
        let call = async { false };
        let probe = || async { None::<(String, bool)> };
        let watched = WatchedCall::start(&line("gone"), "gone", call, probe).unwrap();
        wait_until("the call to end", || watched.watch().done);
        assert!(!watched.watch().returned);
    }

    /// Calls to an nvim stand-in on a Unix socket, over an nvim-rs connection made as the fork makes
    /// its own: which requests reach the socket, in which order, before which answer, and which
    /// answer reaches which caller.
    mod fake_nvim {
        use super::super::*;
        use nvim_rs::compat::tokio::Compat;
        use nvim_rs::rpc::handler::Dummy;
        use nvim_rs::Neovim;
        use std::cell::RefCell;
        use std::io::{BufReader, Write};
        use std::os::unix::net::{UnixListener, UnixStream};
        use std::path::PathBuf;
        use std::sync::mpsc;

        type Nvim = Neovim<Compat<tokio::io::WriteHalf<tokio::net::UnixStream>>>;
        type Answers = mpsc::Sender<(&'static str, Result<rmpv::Value, String>)>;

        /// Only a hang guard: every wait below ends as soon as what it waits for arrives.
        const HANG_GUARD: Duration = Duration::from_secs(20);

        /// The nvim end of the socket: every request it reads is reported as `(msgid, code)`, and
        /// it answers only when told to.
        struct FakeNvim {
            dir: PathBuf,
            path: PathBuf,
            requests: mpsc::Receiver<(u64, String)>,
            accepted: mpsc::Receiver<UnixStream>,
            stream: RefCell<Option<UnixStream>>,
        }

        impl FakeNvim {
            fn start(case: &str) -> FakeNvim {
                let dir = std::env::temp_dir().join(format!("nvrpc-{}-{case}", std::process::id()));
                let _ = std::fs::remove_dir_all(&dir);
                std::fs::create_dir_all(&dir).expect("scratch dir");
                let path = dir.join("nvim.sock");
                let listener = UnixListener::bind(&path).expect("bind the stand-in's socket");
                let (requests_tx, requests) = mpsc::channel();
                let (accepted_tx, accepted) = mpsc::channel();
                std::thread::spawn(move || {
                    let Ok((stream, _)) = listener.accept() else { return };
                    let _ = accepted_tx.send(stream.try_clone().expect("clone the socket"));
                    let mut reader = BufReader::new(stream);
                    while let Ok(frame) = rmpv::decode::read_value(&mut reader) {
                        let Some(parts) = frame.as_array() else { continue };
                        if parts.first().and_then(rmpv::Value::as_u64) != Some(0) {
                            continue;
                        }
                        let msgid = parts.get(1).and_then(rmpv::Value::as_u64).expect("a msgid");
                        let code = parts
                            .get(3)
                            .and_then(rmpv::Value::as_array)
                            .and_then(|params| params.first())
                            .and_then(rmpv::Value::as_str)
                            .unwrap_or_default()
                            .to_owned();
                        if requests_tx.send((msgid, code)).is_err() {
                            return;
                        }
                    }
                });
                FakeNvim {
                    dir,
                    path,
                    requests,
                    accepted,
                    stream: RefCell::new(None),
                }
            }

            fn next_request(&self) -> (u64, String) {
                self.requests
                    .recv_timeout(HANG_GUARD)
                    .expect("a request reaches the stand-in")
            }

            /// Nothing more was written within a moment. A check that something did not happen,
            /// never the proof that something did.
            fn nothing_more_written(&self) -> bool {
                self.requests.recv_timeout(Duration::from_millis(100)).is_err()
            }

            fn answer(&self, msgid: u64, value: &str) {
                let mut slot = self.stream.borrow_mut();
                if slot.is_none() {
                    *slot = Some(self.accepted.recv_timeout(HANG_GUARD).expect("the connection"));
                }
                let stream = slot.as_mut().expect("just set");
                let response = rmpv::Value::Array(vec![1.into(), msgid.into(), rmpv::Value::Nil, value.into()]);
                rmpv::encode::write_value(stream, &response).expect("write the answer");
                stream.flush().expect("flush the answer");
            }
        }

        impl Drop for FakeNvim {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.dir);
            }
        }

        /// An nvim-rs connection to the stand-in, its IO loop on a runtime of its own as the
        /// fork's is.
        fn connect(fake: &FakeNvim) -> (tokio::runtime::Runtime, Nvim) {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("runtime");
            let (nvim, _io) = runtime
                .block_on(nvim_rs::create::tokio::new_path(fake.path.clone(), Dummy::new()))
                .expect("connect to the stand-in");
            (runtime, nvim)
        }

        /// `nvim_exec_lua(code)` handed to `line`; its answer reaches `answers`, tagged with `code`.
        fn ask(line: &CallLine, nvim: &Nvim, code: &'static str, answers: Answers) {
            let nvim = nvim.clone();
            let call = async move { nvim.exec_lua(code, vec![]).await.map_err(|e| e.to_string()) };
            let reply: crate::LuaReply = Box::new(move |result| {
                let _ = answers.send((code, result));
            });
            assert!(line.submit(call, reply), "the line takes the call");
        }

        #[test]
        fn two_calls_are_written_in_the_order_they_were_made() {
            let fake = FakeNvim::start("order");
            let (_runtime, nvim) = connect(&fake);
            let calls = CallLine::start("order").expect("the line's thread");
            let (answers_tx, answers) = mpsc::channel();
            ask(&calls, &nvim, "return 'first'", answers_tx.clone());
            ask(&calls, &nvim, "return 'second'", answers_tx);
            let (first_id, first) = fake.next_request();
            let (second_id, second) = fake.next_request();
            assert_eq!(
                [first.as_str(), second.as_str()],
                ["return 'first'", "return 'second'"],
                "nvim reads the calls in the order they were made"
            );
            // Both are on the wire before either is answered; each answer reaches its own caller.
            fake.answer(second_id, "two");
            assert_eq!(
                answers.recv_timeout(HANG_GUARD).expect("an answer"),
                ("return 'second'", Ok(rmpv::Value::from("two")))
            );
            fake.answer(first_id, "one");
            assert_eq!(
                answers.recv_timeout(HANG_GUARD).expect("an answer"),
                ("return 'first'", Ok(rmpv::Value::from("one")))
            );
        }

        #[test]
        fn a_call_dropped_at_its_deadline_does_not_reorder_the_next() {
            let fake = FakeNvim::start("dropped");
            let (_runtime, nvim) = connect(&fake);
            let calls = CallLine::start("dl-order").expect("the line's thread");
            let (answers_tx, answers) = mpsc::channel();
            {
                // The caller stopped waiting for this one: its answer goes to nobody.
                let (gone_tx, gone) = mpsc::channel();
                drop(gone);
                ask(&calls, &nvim, "return 'given up'", gone_tx);
            }
            ask(&calls, &nvim, "return 'next'", answers_tx.clone());
            let (given_up_id, given_up) = fake.next_request();
            let (next_id, next) = fake.next_request();
            assert_eq!(
                [given_up.as_str(), next.as_str()],
                ["return 'given up'", "return 'next'"],
                "the call nobody waits for is still written ahead of the one after it"
            );
            #[cfg(target_os = "linux")]
            assert_eq!(
                super::threads_named("nvim-rpc dl-order"),
                1,
                "one thread carries both calls, the one given up included"
            );
            fake.answer(next_id, "next");
            assert_eq!(
                answers.recv_timeout(HANG_GUARD).expect("an answer"),
                ("return 'next'", Ok(rmpv::Value::from("next")))
            );
            assert!(fake.nothing_more_written());
            // Its late answer reaches nobody and ends nothing: the connection still carries calls.
            fake.answer(given_up_id, "late");
            ask(&calls, &nvim, "return 'after'", answers_tx);
            let (after_id, after) = fake.next_request();
            assert_eq!(after, "return 'after'");
            fake.answer(after_id, "after");
            assert_eq!(
                answers.recv_timeout(HANG_GUARD).expect("an answer"),
                ("return 'after'", Ok(rmpv::Value::from("after")))
            );
        }
    }

    /// The mechanism against a real `nvim --embed` (needs `nvim` on PATH, no display): an nvim-rs
    /// connection on a tokio runtime, as the fork's is, driven from plain threads. A quit sent while
    /// nvim waits for `f`'s character is held, not typed; the watch keeps answering (`n`,
    /// blocking), then `r?` once the key arrives; a cancel completes the call; the edit is intact.
    /// Then a stopped nvim stops answering, which is what tells a hung nvim from one showing its
    /// dialog.
    mod real_nvim {
        use super::super::*;
        use nvim_rs::compat::tokio::Compat;
        use nvim_rs::rpc::handler::Dummy;
        use nvim_rs::{Neovim, UiAttachOptions};

        type Nvim = Neovim<Compat<tokio::process::ChildStdin>>;

        const LUA: &str = "pcall(vim.cmd, 'confirm qall')";

        fn wait_for(what: &str, mut condition: impl FnMut() -> bool) {
            let deadline = Instant::now() + Duration::from_secs(5);
            while !condition() {
                assert!(Instant::now() < deadline, "timed out waiting for {what}");
                std::thread::sleep(Duration::from_millis(20));
            }
        }

        fn watched(nvim: &Nvim) -> WatchedCall {
            let call = {
                let nvim = nvim.clone();
                async move { nvim.exec_lua(LUA, vec![]).await.is_ok() }
            };
            let probe = {
                let nvim = nvim.clone();
                move || {
                    let nvim = nvim.clone();
                    async move {
                        let pairs = nvim.get_mode().await.ok()?;
                        let field = |name: &str| pairs.iter().find(|(k, _)| k.as_str() == Some(name)).map(|(_, v)| v);
                        Some((
                            field("mode").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
                            field("blocking").and_then(|v| v.as_bool()).unwrap_or(false),
                        ))
                    }
                }
            };
            WatchedCall::start(
                &CallLine::start("test quit").expect("the line's thread"),
                "test quit",
                call,
                probe,
            )
            .expect("threads")
        }

        #[test]
        fn a_watched_quit_is_held_behind_a_pending_key_and_a_stopped_nvim_stops_answering() {
            // `nvim_child` tests examine every child of this process. Hold their spawn lock so this test's
            // `nvim --embed` cannot appear during another test's child search.
            let _serial = crate::nvim_child::SPAWNING.lock().unwrap_or_else(|e| e.into_inner());
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("runtime");
            let (nvim, _io, mut child) = runtime
                .block_on(async {
                    nvim_rs::create::tokio::new_child_cmd(
                        tokio::process::Command::new("nvim").args(["--clean", "--embed", "-n"]),
                        Dummy::new(),
                    )
                    .await
                })
                .expect("nvim must be on PATH for this test");
            let lines = |nvim: &Nvim| -> Vec<String> {
                let nvim = nvim.clone();
                block_on(async move {
                    nvim.get_current_buf()
                        .await
                        .unwrap()
                        .get_lines(0, -1, false)
                        .await
                        .unwrap()
                })
            };
            {
                let nvim = nvim.clone();
                block_on(async move {
                    nvim.ui_attach(80, 24, UiAttachOptions::new().set_linegrid_external(true))
                        .await
                        .unwrap();
                    nvim.command("file draft.txt").await.unwrap();
                    nvim.get_current_buf()
                        .await
                        .unwrap()
                        .set_lines(0, -1, false, vec!["hello world".into()])
                        .await
                        .unwrap();
                    nvim.input("A user edit<Esc>").await.unwrap();
                    nvim.input("f").await.unwrap();
                });
            }

            let quit = watched(&nvim);
            wait_for("an answer while f waits", || quit.watch().last_answer.is_some());
            std::thread::sleep(HEARTBEAT * 2);
            let watch = quit.watch();
            let answer = watch.last_answer.clone().unwrap();
            assert_eq!((answer.mode.as_str(), answer.blocking), ("n", true), "held behind f");
            assert!(!watch.done);

            {
                let nvim = nvim.clone();
                block_on(async move { nvim.input("x").await.unwrap() });
            }
            wait_for("the dialog", || {
                quit.watch().last_answer.is_some_and(|a| a.mode == "r?")
            });
            {
                let nvim = nvim.clone();
                block_on(async move { nvim.input("c").await.unwrap() });
            }
            wait_for("the cancelled quit to come back", || quit.watch().done);
            assert!(quit.watch().returned, "a cancel comes back with nvim's answer");
            assert_eq!(
                lines(&nvim),
                vec!["hello world user edit".to_string()],
                "nothing was typed"
            );

            // A stopped nvim: the watch's last sign of life stays where it was.
            let stuck = watched(&nvim);
            wait_for("the second dialog", || {
                stuck.watch().last_answer.is_some_and(|a| a.mode == "r?")
            });
            let pid = child.id().expect("pid") as libc::pid_t;
            // SAFETY: a signal to the child this test spawned and still holds.
            unsafe { libc::kill(pid, libc::SIGSTOP) };
            std::thread::sleep(HEARTBEAT * 2);
            let frozen = stuck.watch().last_alive();
            std::thread::sleep(Duration::from_millis(1500));
            assert_eq!(stuck.watch().last_alive(), frozen, "a stopped nvim does not answer");
            assert!(!stuck.watch().done);
            // SAFETY: as above.
            unsafe { libc::kill(pid, libc::SIGCONT) };
            wait_for("answers again", || stuck.watch().last_alive() > frozen);

            // SAFETY: as above; the dialog is up, so end it outright.
            unsafe { libc::kill(pid, libc::SIGKILL) };
            wait_for("the call to see nvim go", || stuck.watch().done);
            assert!(
                !stuck.watch().returned,
                "an nvim that went away never answered the quit"
            );
            let _ = runtime.block_on(child.wait());
        }
    }
}
