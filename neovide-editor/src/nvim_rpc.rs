//! One msgpack-RPC request to nvim made off the GTK thread, and a watch on whether nvim's loop is
//! still answering while that request is outstanding (v1 hardening Task 6, fix round 1).
//!
//! **Why a request and not keys.** `NeovideEditorPane::send_keys` goes through `nvim_input`, and
//! typed keys are read as whatever nvim is waiting for: after `f`, inside `getchar()`, after an
//! insert-mode `Ctrl-V`, the first key of a command becomes that one character and the rest runs as
//! Normal-mode commands (`neovibe_core::layout::kill::editor_quit_lua`'s doc has the measurement). A
//! request types nothing. It is what stock Neovide's own window close sends, too: `ParallelCommand::
//! Quit` runs `exit_handler.lua` through `nvim_exec_lua`.
//!
//! **Why a thread of its own.** The fork's `LiveHarness` has no API that runs Lua and exposes its
//! tokio runtime to nobody; it does hand out the connection (`LiveHarness::neovim_handler`, "the same
//! escape hatch real Neovide's own ... `RouteWindow` exposes"). A request whose Lua opens a dialog is
//! not answered until the user answers the dialog, so the call is made on its own thread and blocks
//! there, driven by [`block_on`]. nvim-rs's future only writes to nvim's stdin (a tokio pipe, which
//! remembers the reactor that registered it, so polling it from here is fine while the fork's
//! runtime runs) and waits on a `futures` oneshot the fork's IO task completes. **The future is never
//! dropped before it resolves**: nvim-rs's IO loop returns an error, ending the whole connection,
//! when a response arrives for a request whose receiver is gone (`io_loop`'s `sender.send(..)
//! .map_err(..)?`, nvim-rs 0.9.2 `src/neovim.rs`). nvim exiting resolves it: the IO loop hands every
//! caller still waiting an error when the pipe closes.
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
use std::pin::pin;
use std::sync::atomic::{AtomicBool, Ordering};
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
    /// It came back with nvim's answer: nvim took the request, ran it and is still there to say so.
    /// For a `:confirm qall` that means nvim did NOT quit (a real quit ends the process before any
    /// answer is sent), so a host can clear the quit it was waiting on. `false` while outstanding,
    /// and for a request that ended because nvim went away (codex finding C1: the two used to be
    /// one `done`, so a cancelled quit could not be told from an exit).
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

/// A request running on its own thread, watched by a second one until it is done or this is dropped.
pub(crate) struct WatchedCall {
    shared: Arc<Shared>,
}

impl WatchedCall {
    /// Runs `call()` to completion on one thread and, until it completes, `probe()` every
    /// [`HEARTBEAT`] on another. `call` resolves to whether nvim answered it ([`CallWatch::returned`]).
    /// `probe` resolves to `nvim_get_mode`'s `(mode, blocking)`, or `None` when nvim is gone, which
    /// ends the watch.
    pub(crate) fn start<C, CF, P, PF>(name: &str, call: C, mut probe: P) -> io::Result<WatchedCall>
    where
        C: FnOnce() -> CF + Send + 'static,
        CF: Future<Output = bool>,
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
        {
            let shared = shared.clone();
            std::thread::Builder::new()
                .name(format!("nvim-rpc {name}"))
                .spawn(move || {
                    let answered = block_on(call());
                    // `returned` first: a reader that sees `done` then sees how it ended.
                    shared.returned.store(answered, Ordering::SeqCst);
                    shared.done.store(true, Ordering::SeqCst);
                })?;
        }
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
        Ok(WatchedCall { shared })
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
    /// The watch once the request's worker has published how it ended, waited for up to `within`
    /// (round 3, codex finding 2). nvim-rs hands the answer -- or, when nvim's pipe closes, an
    /// error -- to the worker through a oneshot, and the worker publishes it a moment later; nvim's
    /// exit reaches the host without waiting for that. Called at the exit, when nvim-rs's IO loop
    /// has already resolved the request, this returns within microseconds; a request still
    /// outstanding at the bound comes back with `done == false`.
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
            move || async move {
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
        let watched = WatchedCall::start("test", call, probe).unwrap();
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
        let call = || std::future::pending::<bool>();
        let probe = || std::future::pending::<Option<(String, bool)>>();
        let watched = WatchedCall::start("stuck", call, probe).unwrap();
        std::thread::sleep(HEARTBEAT * 2);
        let watch = watched.watch();
        assert!(!watch.done);
        assert!(!watch.returned);
        assert_eq!(watch.last_answer, None);
        assert_eq!(watch.last_alive(), watch.sent_at);
    }

    /// Round 3, codex finding 2: nvim's answer reaches the worker through a oneshot, and the worker
    /// publishes it a moment later. `settled` waits (bounded) for that, so a caller deciding at
    /// nvim's exit reads how the request really ended; one still outstanding at the bound reads as
    /// outstanding.
    #[test]
    fn settled_waits_for_the_worker_to_publish() {
        let call = || async {
            std::thread::sleep(Duration::from_millis(150));
            true
        };
        let probe = || async { None::<(String, bool)> };
        let watched = WatchedCall::start("late", call, probe).unwrap();
        assert!(!watched.watch().done, "the case: not published yet");
        let settled = watched.settled(Duration::from_secs(2));
        assert!(settled.done && settled.returned, "{settled:?}");

        let pending = WatchedCall::start("never", || std::future::pending::<bool>(), probe).unwrap();
        let started = Instant::now();
        let unsettled = pending.settled(Duration::from_millis(100));
        assert!(!unsettled.done);
        assert!(started.elapsed() >= Duration::from_millis(100), "it waited the bound");
    }

    /// Codex finding C1's half here: a request that ended without nvim's answer (nvim went away) is
    /// `done` and NOT `returned`, so a host never takes an exit for a cancel.
    #[test]
    fn a_call_that_ends_without_an_answer_is_done_but_not_returned() {
        let call = || async { false };
        let probe = || async { None::<(String, bool)> };
        let watched = WatchedCall::start("gone", call, probe).unwrap();
        wait_until("the call to end", || watched.watch().done);
        assert!(!watched.watch().returned);
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
                move || async move { nvim.exec_lua(LUA, vec![]).await.is_ok() }
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
            WatchedCall::start("test quit", call, probe).expect("threads")
        }

        #[test]
        fn a_watched_quit_is_held_behind_a_pending_key_and_a_stopped_nvim_stops_answering() {
            // Codex finding C5: `nvim_child`'s tests look at every child this process has, so this
            // test's own `nvim --embed` must not appear while one of them runs.
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
