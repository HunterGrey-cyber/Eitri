//! How the panel lets a backend go: every shutdown runs on a worker (`spawn_teardown`), the close path
//! waits for those workers under its backstops (`ShutdownWatch`), and `Retiring` holds what is still going.

use super::*;

/// The backstop on a backend this panel is tearing down itself, once the window is gone. Since
/// session tabs it bounds every tab's teardown at once: they run in parallel, one worker each, under
/// this one span (`tear_down_all`).
///
/// **Nothing waits on this. No thread blocks on it at all, and the GTK main loop least of all.**
/// It is a deadline observed by a `CLOSE_WATCH_POLL` tick on the main loop (`close_watch_step`),
/// which does a `try_recv` and returns. The window is already destroyed and the teardown is on a
/// worker; what keeps the process alive meanwhile is a `gio` application hold, not a wait. Read
/// `hold_until_done` for the mechanism before changing this number.
///
/// **What is inside the span it bounds:** the whole of `AgentBackend::shutdown()` AND the drop of
/// the backend afterwards -- `spawn_teardown` drops it explicitly before the
/// worker reports. On the sidecar path those are two different waits and only the second one kills
/// anything: `shutdown()` issues `close_session`, a unary RPC bounded at `agent::UNARY_RPC_TIMEOUT`
/// (agent/src/providers/claude_sidecar/mod.rs), and it is `SpawnedSidecar::drop` (close stdin, poll
/// up to `agent::SIDECAR_EXIT_GRACE`, then SIGKILL) plus `RuntimeThread::drop` that end the child.
/// 18 seconds covers that ~16 s worst case with a little room. The bound must include the drop,
/// because it is the step that escalates to stop the child process.
///
/// **What expiring costs**, since it is a bound and not a guarantee: one line on stderr naming what
/// is still outstanding, then the hold is released anyway, `Application::run` returns, and the
/// detached worker dies with the process -- i.e. exactly the orphaned `claude`/sidecar/`node` this
/// file keeps chasing with pid diffs, which is why the number is generous rather than tight.
pub(super) const SESSION_CLOSE_BACKSTOP: std::time::Duration = std::time::Duration::from_secs(18);

/// How long a window close keeps the process alive for a backend that is still CONNECTING.
///
/// Not a teardown budget like the two above: what this waits out is `AgentBackend::start`
/// finishing, so that a backend which finishes after the window is gone can be shut down rather
/// than left running with nothing owning it. Three seconds is well past a warm connect and well
/// short of a cold one (`npm ci` in a fresh Verdandi checkout is minutes), so it deliberately does
/// not cover every case; what it must not do is cover none of them by exiting instantly.
///
/// The sidecar's own bind wait (`SIDECAR_BIND_BUDGET` in `agent`, 15 s) is longer than this on
/// purpose. A window closed while a cold sidecar is still starting does not wait out that budget:
/// the hold ends here and the process exits, the connect worker with it. The sidecar is left with
/// its stdin pipe closed, which it reads as the host being gone, so no one waits on it. A longer
/// hold would keep an invisible process alive for up to 15 s after the window closed, to save a
/// session nobody is looking at.
pub(super) const CONNECT_CLOSE_BACKSTOP: std::time::Duration = std::time::Duration::from_secs(3);

/// How long a close waits for the saved tabs' last write. A write is a small file renamed over the old one,
/// milliseconds even on a network file system; a disk that hangs longer is left, and since the rename comes
/// last the previous file stays whole.
pub(super) const SAVED_TABS_BACKSTOP: std::time::Duration = std::time::Duration::from_secs(5);

/// How long a close waits for the prompt history's last write: as long as its writer can take, the round
/// of tries under way when the window closes and one more for what it still holds
/// (`prompt_history::CLOSE_WAIT`). A write is a small file renamed over the old one, so the previous file
/// stays whole if the process leaves first.
pub(super) const HISTORY_BACKSTOP: std::time::Duration = eitri_core::prompt_history::CLOSE_WAIT;

/// How often the main loop looks at a close that is running on a worker. A `try_recv` and an
/// `Instant::elapsed`, so the interval only sets how long the process lingers past a teardown that
/// has already finished -- not how much work the main loop does.
pub const CLOSE_WATCH_POLL: std::time::Duration = std::time::Duration::from_millis(50);

/// What one tick of the close watch decides.
///
/// A value rather than control flow so the decision -- specifically that a report observed on the
/// same tick as the deadline counts as a report -- is assertable without a GTK main loop, which
/// `shell`'s tests cannot start.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(super) enum CloseWatch {
    /// Still running, still inside the backstop: keep holding the application.
    KeepHolding,
    /// The worker reported. Everything the span covers is done.
    Finished,
    /// The sender dropped without a report: the worker panicked inside the teardown. Immediate,
    /// and NOT a timeout -- calling it one would send a future debugger looking for a hang that
    /// never happened.
    WorkerDied,
    /// The backstop expired with the teardown still outstanding.
    Expired,
}

pub(super) fn close_watch_step<T>(reported: &Result<T, mpsc::TryRecvError>, past_backstop: bool) -> CloseWatch {
    match reported {
        // Checked first on purpose: a `try_recv` that finds the value also finds the sender gone on
        // the NEXT call, and a teardown that finished in the same tick as the deadline finished.
        Ok(_) => CloseWatch::Finished,
        Err(mpsc::TryRecvError::Disconnected) => CloseWatch::WorkerDied,
        Err(mpsc::TryRecvError::Empty) if past_backstop => CloseWatch::Expired,
        Err(mpsc::TryRecvError::Empty) => CloseWatch::KeepHolding,
    }
}

/// One worker a window close left running: what to look at, and what to start when it reports.
/// Built by [`watched`]; looked at by [`ShutdownWatch::step`].
pub(super) struct Watched {
    /// One look at `now`: `None` while the worker is still inside its backstop, else the entries its
    /// end started (usually none).
    pub(super) look: Box<dyn FnMut(std::time::Instant) -> Option<Vec<Watched>>>,
}

/// Watches `rx` for one report, with nothing waiting for it. Its backstop runs from this call.
///
/// `what` names the branch in the log: the callers fail for different reasons and a debugger
/// reading one line needs to know which one it is looking at. `on_value` runs when the report
/// arrives and returns the entries it starts (the mid-connect branch starts a teardown there); they
/// join the same watch, so the process is never let go between the two.
pub(super) fn watched<T: 'static>(
    what: &'static str,
    rx: mpsc::Receiver<T>,
    backstop: std::time::Duration,
    on_value: impl FnMut(T) -> Vec<Watched> + 'static,
) -> Watched {
    watched_abandoning(
        what,
        rx,
        backstop,
        "whatever is left of it -- possibly a live `claude`, sidecar or `node` -- is being abandoned",
        on_value,
    )
}

/// [`watched`], saying `abandoned` (what is left behind when the backstop passes) in its log line.
pub(super) fn watched_abandoning<T: 'static>(
    what: &'static str,
    rx: mpsc::Receiver<T>,
    backstop: std::time::Duration,
    abandoned: &'static str,
    mut on_value: impl FnMut(T) -> Vec<Watched> + 'static,
) -> Watched {
    let started = std::time::Instant::now();
    Watched {
        look: Box::new(move |now| {
            let reported = rx.try_recv();
            match close_watch_step(&reported, now.saturating_duration_since(started) >= backstop) {
                CloseWatch::KeepHolding => return None,
                CloseWatch::Finished => {
                    if let Ok(value) = reported {
                        return Some(on_value(value));
                    }
                }
                CloseWatch::WorkerDied => {
                    eprintln!("[agent_panel] {what}: its worker died without reporting");
                }
                CloseWatch::Expired => eprintln!(
                    "[agent_panel] {what} had still not finished {}s after the window closed; the \
                     process is exiting anyway and {abandoned}",
                    backstop.as_secs()
                ),
            }
            Some(Vec::new())
        }),
    }
}

/// Teardowns and connects a window close left on worker threads, each with its backstop. The host
/// polls [`ShutdownWatch::step`] every [`CLOSE_WATCH_POLL`] while it keeps its process alive, and lets
/// the process go once `step` returns false. Nothing waits on a worker.
#[must_use]
pub struct ShutdownWatch {
    pub(super) entries: Vec<Watched>,
}

impl ShutdownWatch {
    /// Nothing to keep the process alive for.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// One look at every entry at `now`: a report, a dead worker or a passed backstop lets it go (logged
    /// as it always was); a connect that reports a backend adds that backend's teardown. Returns whether
    /// anything is still held.
    pub fn step(&mut self, now: std::time::Instant) -> bool {
        let mut started = Vec::new();
        self.entries.retain_mut(|entry| match (entry.look)(now) {
            None => true,
            Some(more) => {
                started.extend(more);
                false
            }
        });
        self.entries.extend(started);
        !self.entries.is_empty()
    }
}

/// A backend on its way to [`spawn_teardown`]: one a tab held, which the set counted as retiring for
/// that tab ([`Retired`]), or one no tab ever held -- a connect that finished after its tab or the
/// window was gone.
pub(super) enum Leaving {
    Retired(Retired),
    Unheld(AgentBackend),
}

impl Leaving {
    /// The tab whose presence the set keeps until this backend's teardown is reported.
    pub(super) fn tab(&self) -> Option<TabId> {
        match self {
            Leaving::Retired(retired) => Some(retired.tab()),
            Leaving::Unheld(_) => None,
        }
    }
}

impl From<Retired> for Leaving {
    fn from(retired: Retired) -> Self {
        Leaving::Retired(retired)
    }
}

impl From<AgentBackend> for Leaving {
    fn from(backend: AgentBackend) -> Self {
        Leaving::Unheld(backend)
    }
}

/// What [`spawn_teardown`]'s worker reports. `closed`: `shutdown()` has returned (the terminal handoff
/// shows its command on this). `dropped`: the backend has been dropped too, which on the sidecar path
/// is where the child dies (what a window's close and the presence wait for). A worker that panicked
/// drops its senders instead, which each receiver reads as disconnected.
pub(super) struct Teardown {
    pub(super) closed: mpsc::Receiver<()>,
    pub(super) dropped: mpsc::Receiver<()>,
}

/// Shuts a backend down on a worker thread, then drops it there, and never waits for it. **The one
/// place this panel shuts a backend down**: a closed tab, `r`, the tripwire, a lost session, the
/// terminal handoff, the window's close and a connect that finished too late all come here, so the
/// order below exists once.
///
/// Why it may not happen inline: `AgentBackend::shutdown` stops ingestion first, and
/// `ConversationIngest::stop` JOINS the ingestion thread, which since the resume-history branch
/// may be inside `history::store::save` -- serializing up to `HISTORY_MAX_CHARS`, an `sync_all`, a
/// rename and a directory fsync, plus (on a first write) a `read_dir` and up to 17 record parses
/// to sweep orphans. That is a disk wait, and this repository already has one recorded whole-window
/// freeze from exactly the family "the GTK thread waits on something slow" (the projection-guard
/// deadlock, 2026-09-15); a frozen window during close is the same symptom whether the cause is a
/// mutex or an fsync on a full disk. A bare drop is no better: `AgentConversation` has no `Drop`, so
/// it never sends `close_session`, and the drop chain that does run (`SpawnedSidecar` polls for up to
/// 3 s before escalating to SIGKILL, then `RuntimeThread` joins its Tokio thread) would block the
/// whole shell.
///
/// **`drop(backend)` is explicit and its position is the point.** On the sidecar path `shutdown()`
/// only issues `close_session` and joins ingestion; the child is killed by `SpawnedSidecar::drop`
/// (close stdin, poll up to 3s, SIGKILL) and its Tokio thread joined by `RuntimeThread::drop`. A
/// version of this code reported to the watcher first and let the backend fall at the end of the
/// closure, which put the only part of the teardown that actually ends the process AFTER the thing
/// that decides the process may exit -- so the escalation raced process exit and the backstop did
/// not cover what its own doc derived it from. So `dropped` is sent after the drop, and `closed`,
/// which the handoff's command waits on, before it.
pub(super) fn spawn_teardown(leaving: impl Into<Leaving>) -> Teardown {
    let mut backend = match leaving.into() {
        Leaving::Retired(retired) => retired.into_backend(),
        Leaving::Unheld(backend) => backend,
    };
    let (closed_tx, closed) = mpsc::channel();
    let (dropped_tx, dropped) = mpsc::channel();
    std::thread::spawn(move || {
        backend.shutdown();
        // The receiver is gone if nobody waits for this signal any more; that is not an error.
        let _ = closed_tx.send(());
        // Explicit, and BEFORE the report below rather than at the end of this closure: on the
        // sidecar path this line is where the child actually dies. See this function's doc.
        drop(backend);
        let _ = dropped_tx.send(());
    });
    Teardown { closed, dropped }
}

/// A backend no tab held, torn down once the window is gone and watched until it is dropped.
pub(super) fn tear_down(what: &'static str, backend: AgentBackend) -> Watched {
    watched(what, spawn_teardown(backend).dropped, SESSION_CLOSE_BACKSTOP, |()| {
        Vec::new()
    })
}

/// Every tab's backend at once, one worker each, under ONE backstop: serial sidecar `CloseSession`s
/// are bounded at 10 s each, so N tabs torn down in turn could outlive any backstop. A collector waits
/// for every worker's drop and reports once to the watch. `None` when there is no backend: nothing is
/// held then.
pub(super) fn tear_down_all(what: &'static str, backends: Vec<Retired>) -> Option<Watched> {
    if backends.is_empty() {
        return None;
    }
    let expected = backends.len();
    let each: Vec<mpsc::Receiver<()>> = backends
        .into_iter()
        .map(|retired| spawn_teardown(retired).dropped)
        .collect();
    let (all_tx, all_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = all_tx.send(collect_reports(each));
    });
    Some(watched(what, all_rx, SESSION_CLOSE_BACKSTOP, move |finished| {
        if finished < expected {
            eprintln!(
                "[agent_panel] {what}: {} of {expected} workers died without reporting",
                expected - finished
            );
        }
        Vec::new()
    }))
}

/// A connect still running when the window closed -- a tab's, or a closed tab's (`Retiring`). A
/// connect, not a teardown: see `CONNECT_CLOSE_BACKSTOP`. A backend that finishes after the process
/// is gone is exactly the orphan `AgentPanelHandle::shutdown` exists to prevent, so one that
/// finishes inside the backstop is torn down, held and bounded like the installed sessions: its
/// shutdown reaches the same ingestion-thread join.
pub(super) fn watch_connect(rx: mpsc::Receiver<Result<AgentBackend, BackendError>>) -> Watched {
    watched(
        "a connect that was still running",
        rx,
        CONNECT_CLOSE_BACKSTOP,
        move |result| match result {
            Ok(backend) => {
                eprintln!("[agent_panel] window closed mid-connect; shutting down the backend that finished anyway");
                vec![tear_down("a backend that finished mid-connect", backend)]
            }
            Err(e) => {
                eprintln!(
                    "[agent_panel] window closed mid-connect; the backend had already failed: {}",
                    e.message
                );
                Vec::new()
            }
        },
    )
}

/// Waits for each teardown worker's report and says how many arrived. A worker that died drops its
/// sender without one, which ends that wait at once.
pub(super) fn collect_reports(each: Vec<mpsc::Receiver<()>>) -> usize {
    each.iter().filter(|rx| rx.recv().is_ok()).count()
}

/// What tabs that left while the window stays open are still doing: a backend being shut down on
/// a worker, or a connect that has not finished (a closed tab, `r` reset, a fatal command, a
/// session that never opened).
///
/// **Held in panel state so the window-close backstop sees it.** These used to run on bare threads nobody recorded, so a window closed a few seconds
/// after `prefix &` y let `Application::run` return with the worker halfway through `shutdown()`:
/// `SpawnedSidecar::drop` never ran and the sidecar outlived the process, and a connect finishing
/// later was never shut down at all. `AgentPanelHandle::shutdown` now watches every entry here under
/// the same holds and backstops as the tabs it takes; the pump forgets an entry once it finishes.
#[derive(Default)]
pub(super) struct Retiring {
    /// Connects of tabs that are gone. Whatever one produces is shut down (`poll`).
    pub(super) connects: Vec<mpsc::Receiver<Result<AgentBackend, BackendError>>>,
    /// One per teardown, the teardown's `dropped`: sent after the backend was shut down AND dropped --
    /// the drop is where the sidecar child actually dies. Each names the tab its [`Retired`] was
    /// counted for, whose presence the set keeps until `poll` returns it; `None` for a backend no tab
    /// held.
    pub(super) teardowns: Vec<(Option<TabId>, mpsc::Receiver<()>)>,
}

impl Retiring {
    /// One backend, torn down by [`spawn_teardown`] and watched here until it is dropped.
    pub(super) fn backend(&mut self, leaving: impl Into<Leaving>) {
        let leaving = leaving.into();
        let tab = leaving.tab();
        self.teardowns.push((tab, spawn_teardown(leaving).dropped));
    }

    /// [`backend`](Self::backend), and a receiver told when it is dropped (or its worker died): for a
    /// start that must not begin before the old session lets go of its lease, which goes with the drop.
    pub(super) fn backend_watched(&mut self, retired: Retired) -> mpsc::Receiver<()> {
        let tab = retired.tab();
        let dropped = spawn_teardown(retired).dropped;
        // Two readers want the drop -- this watch and the start -- and a receiver has one, so a relay
        // tells both. A worker that died reads as disconnected, and has dropped the backend on its way.
        let (held_tx, held_rx) = mpsc::channel();
        let (gone_tx, gone_rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = dropped.recv();
            let _ = held_tx.send(());
            let _ = gone_tx.send(());
        });
        self.teardowns.push((Some(tab), held_rx));
        gone_rx
    }

    /// A tab removed from the set: its retired backend is shut down, its connect is waited for, and a
    /// handoff's own worker (which already owns that backend) is waited for too. Returns the
    /// requests the tab still owed a `command_result`, for the caller to refuse -- a connect's
    /// `send_message`/`resume`, and a handoff's `handoff_to_terminal`. The record is never deleted.
    pub(super) fn tab(&mut self, removed: RemovedTab) -> Vec<String> {
        let RemovedTab { mut tab, retired } = removed;
        let mut owed = Vec::new();
        if let Some(handoff) = tab.pending_handoff.take() {
            owed.push(handoff.request_id);
            // Its `Retired` was counted when the handoff took it, and its close signal will never be
            // read now: the drop is what this tab's presence waits for.
            self.teardowns.push((Some(tab.id), handoff.dropped_rx));
        }
        if let Some(retired) = retired {
            self.backend(retired);
        }
        match tab.into_backend() {
            TabBackend::Starting(pending) => {
                owed.push(pending.request_id);
                self.connects.push(pending.result_rx);
            }
            // Nothing was spawned, so there is nothing to stop; the command that asked is owed an answer.
            TabBackend::AwaitingTrust(waiting) => owed.extend(waiting.start.request_id().map(str::to_owned)),
            // A live backend left the tab as `retired`, above.
            TabBackend::Live(_) | TabBackend::NotStarted | TabBackend::Failed { .. } => {}
        }
        owed
    }

    /// Each tick: a connect that finished has its backend shut down; a teardown that reported, or
    /// whose worker died, is forgotten, and the tab it was counted for is returned for
    /// [`TabSet::retire_done`]. `try_recv` only -- this runs on the GTK main loop.
    pub(super) fn poll(&mut self) -> Vec<TabId> {
        let mut finished = Vec::new();
        self.connects.retain(|rx| match rx.try_recv() {
            Ok(Ok(backend)) => {
                finished.push(backend);
                false
            }
            Ok(Err(_)) | Err(mpsc::TryRecvError::Disconnected) => false,
            Err(mpsc::TryRecvError::Empty) => true,
        });
        for backend in finished {
            eprintln!("[agent_panel] a closed tab's connect finished; shutting its backend down");
            self.backend(backend);
        }
        let mut done = Vec::new();
        self.teardowns.retain(|(tab, rx)| {
            // A worker that died has dropped its backend on the way out, which ends the child too.
            let finished = !matches!(rx.try_recv(), Err(mpsc::TryRecvError::Empty));
            if finished {
                done.extend(*tab);
            }
            !finished
        });
        done
    }

    pub(super) fn is_empty(&self) -> bool {
        self.connects.is_empty() && self.teardowns.is_empty()
    }
}

/// What a command still owed by a tab that was closed is answered with.
pub(super) const TAB_CLOSED_MESSAGE: &str = "the tab was closed before this finished";

/// `y` to `prefix &`: `tabs::close_steps`, in order. The record is never deleted.
///
/// **Refused while the tab moves to the other window** (`MOVING`): the agent is that window's to take
/// up, and the close would interrupt and shut it down.
///
/// **Refused while the tab is handing off**: the close takes
/// at most the sidecar's 10 s `close_session`, and the only thing it produces is the command the
/// user asked to be shown. Closing the tab under it would drop the command and the
/// `command_result` the panel is still waiting on.
///
/// A tab closed while it is still connecting has its `send_message`/`resume` answered with an
/// error here, so the panel's in-flight record is closed and a first message comes back with its
/// text; its connect is kept in `Retiring` and shut down when it finishes. A message held for the
/// presence (a first one or a composer's) is answered the same way, to the page that sent it.
///
/// A tab bound to an extra view leaves that view empty: its page is sent its empty strip, and the
/// window is told by the next tick (`on_view_empty`), never from inside a page's own message.
pub(super) fn close_tab(state: &mut AgentPanelState, tab: TabId, out: &mut Outbox) -> Result<(), &'static str> {
    let before = state.tabs.attention();
    let Some(facts) = state.tabs.close_facts(tab) else {
        return Ok(());
    };
    let view = state.tabs.view_of(tab);
    if state.tabs.get(tab).is_some_and(|t| t.pending_handoff.is_some()) {
        return Err("this tab is still being handed off to a terminal; close it once that finishes");
    }
    // Its agent is being handed to the other window: an interrupt or a shutdown here would end the
    // session that window is taking up. The move either lands there or is called off and the tab
    // takes input again.
    if state.tabs.is_moving(tab) {
        return Err(eitri_core::tab_set::MOVING);
    }
    // A wait for the tab's old backend goes with the tab; nothing will start in it.
    state.resume_after.remove(&tab);
    // Taken before the removal: a start asked from a page the tab has since left is answered there.
    let start_page = state.owed.take_where(|answer| answer.what == Owed::StartPage(tab));
    // A message held for the presence goes with the tab, unsent: its command is answered like a start
    // in flight, so its text comes back to the page that typed it.
    let held = state
        .tabs
        .held_send_request(tab)
        .map(|(request_id, page)| (request_id.to_string(), page));
    let mut owed = Vec::new();
    for step in eitri_core::tabs::close_steps(&facts) {
        match step {
            eitri_core::tabs::CloseStep::Interrupt => {
                let interrupted = backend_for(&mut state.tabs, tab).and_then(AgentBackend::interrupt);
                if let Err(e) = interrupted {
                    // The close goes on: the shutdown below ends the turn anyway.
                    eprintln!(
                        "[agent_panel] tab {}: interrupt before close failed: {}",
                        tab.0, e.message
                    );
                }
            }
            // The queued words outlive the tab as history.
            eitri_core::tabs::CloseStep::QueueToHistory => {
                let texts = state.tabs.queue_texts(tab);
                remember_prompts(state, &texts, out);
            }
            // Folded into `Remove`: `Retiring::tab` shuts the removed tab's retired backend down.
            eitri_core::tabs::CloseStep::Shutdown => {}
            eitri_core::tabs::CloseStep::Remove => {
                if let Some(removed) = state.tabs.remove(tab) {
                    owed = state.retiring.tab(removed);
                }
                // Its question goes with it: a record still being written for it starts nothing.
                state.trust.forget_tab(tab);
                editor_glue::tab_gone(state, tab);
            }
        }
    }
    if view.is_base() {
        switch_payloads(state, out);
    } else {
        let payload = tabs_payload_recorded(state, view);
        out.send(view, payload);
        state.emptied_views.push(view);
    }
    // After the switch, not before: the panel restores a refused send into the composer only while
    // its tab is the active one, and the switch that follows would then replace it with the next
    // tab's draft. Answered once the closed tab is no longer active, the refusal is reported with
    // the message's full text instead, and the switch cannot clear that notice.
    // The page that sent a request holds it, and it may not be the page that showed the tab: a first
    // message sent in the base view, its tab broken out while it waited, is still the base page's to
    // report (with its text) and to take back.
    // A page that has closed is sent nothing.
    match held {
        // A first message: its start's page is the one owed.
        Some((request_id, None)) => owed.push(request_id),
        Some((request_id, Some(page))) => {
            if let Some(typed_in) = held_prompt_page(state, tab, &request_id, page) {
                out.send(
                    typed_in,
                    serialize_command_result_for_js(&request_id, Err(TAB_CLOSED_MESSAGE)),
                );
            }
        }
        None => {}
    }
    for request_id in owed {
        let asked_from = match &start_page {
            Some(asked) if asked.request_id == request_id => match asked.page {
                Some(page) => page,
                None => continue,
            },
            _ => view,
        };
        out.send(
            asked_from,
            serialize_command_result_for_js(&request_id, Err(TAB_CLOSED_MESSAGE)),
        );
    }
    send_hello_if_open_sessions_changed(state, out);
    report_attention(state, before, out);
    Ok(())
}

/// [`close_tab`] for a close someone asked for on one tab. The window hears when that was the base view's last: it may
/// be a chat window with nothing left to show. A tab closed for a hand-over goes through [`close_tab`], since its window
/// has to stay until the tab is safe in its new one.
pub(super) fn close_tab_asked(state: &mut AgentPanelState, tab: TabId, out: &mut Outbox) -> Result<(), &'static str> {
    let last = state.tabs.base_tabs() == [tab];
    close_tab(state, tab, out)?;
    if last {
        out.effect(Effect::ChatMove(ChatMoveEvent::LastTabClosed));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_panel::tests::*;

    #[test]
    fn closing_the_base_views_last_tab_by_asking_tells_the_window() {
        let (handle, _page) = settled_panel();
        let heard = Rc::new(RefCell::new(Vec::new()));
        let into = heard.clone();
        handle.state.borrow_mut().chat_moves.hook = Some(Rc::new(move |event| into.borrow_mut().push(event)));
        let lasts = || {
            let lasts = heard.borrow();
            lasts
                .iter()
                .filter(|event| matches!(event, ChatMoveEvent::LastTabClosed))
                .count()
        };
        let (first, second) = {
            let mut state = handle.state.borrow_mut();
            let first = state.tabs.active();
            (first, state.tabs.open(AgentId::Claude))
        };
        handle.run(|state, out| close_tab_asked(state, second, out)).unwrap();
        assert_eq!(lasts(), 0, "a tab is left");
        handle.run(|state, out| close_tab(state, first, out)).unwrap();
        assert_eq!(lasts(), 0, "a close nobody asked for on this tab says nothing");
        let fresh = handle.state.borrow().tabs.active();
        handle.run(|state, out| close_tab_asked(state, fresh, out)).unwrap();
        assert_eq!(lasts(), 1);
        assert_eq!(
            handle.state.borrow().tabs.base_tabs().len(),
            1,
            "a fresh tab stands in its place"
        );
    }

    /// `SESSION_CLOSE_BACKSTOP` must cover both the `close_session` unary RPC and the
    /// sidecar's own reap of the `claude` CLI it owns, with margin -- so the drop normally finishes
    /// inside this backstop rather than racing it.
    #[test]
    fn the_close_backstop_covers_a_close_rpc_and_the_sidecars_reap() {
        assert!(
            SESSION_CLOSE_BACKSTOP
                >= agent::UNARY_RPC_TIMEOUT + agent::SIDECAR_EXIT_GRACE + std::time::Duration::from_secs(1)
        );
    }

    /// A backend is shut down in one place, on a worker, and dropped right after on that worker: the
    /// drop is where the sidecar's child dies, so a copy of the order elsewhere is one more place that
    /// can report the teardown before the process has ended.
    #[test]
    fn every_shutdown_runs_through_one_teardown() {
        use eitri_core::source_scan as scan;
        let sources = scan::rust_sources(&["panel/src"]);
        let (mut everywhere, mut in_teardown) = (0, 0);
        for source in &sources {
            let code = scan::code_only(&source.text);
            let code = scan::without(&code, &scan::modules(&code, "tests"));
            let flat: String = code.split_whitespace().collect();
            everywhere += flat.matches(".shutdown()").count();
            for body in scan::functions(&code, "spawn_teardown") {
                let flat: String = body.split_whitespace().collect();
                in_teardown += flat.matches(".shutdown()").count();
            }
        }
        assert_eq!(
            everywhere, 1,
            "a backend's shutdown is called from one place in the panel"
        );
        assert_eq!(in_teardown, 1, "that place is the teardown worker");
    }

    /// A tab closed while the window stays open is kept in panel state
    /// until its teardown or connect finishes, so the window-close backstop can see it; and the
    /// command a connecting tab still owed is handed back to be answered.
    #[test]
    fn a_closed_tabs_teardown_and_connect_are_kept_until_they_finish() {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("panel-retiring");
        let mut set = TabSet::new(BackendKind::Sidecar, eitri_core::agent_bridge::SessionModeChoice::Auto);
        let live = set.active();
        set.get_mut(live)
            .unwrap()
            .set_backend(TabBackend::Live(live_backend(&dir).1));
        let connecting = set.open(eitri_core::tab_set::AgentId::Claude);
        let (connect_tx, result_rx) = mpsc::channel();
        set.get_mut(connecting)
            .unwrap()
            .set_backend(TabBackend::Starting(PendingStart {
                request_id: "req-send".into(),
                result_rx,
                first_turn: None,
                resume: None,
                resumed_title: None,
                resumed_name: None,
            }));

        let mut retiring = Retiring::default();
        assert!(
            retiring.tab(set.remove(live).unwrap()).is_empty(),
            "a live tab owes nothing"
        );
        assert_eq!(
            retiring.tab(set.remove(connecting).unwrap()),
            vec!["req-send".to_string()]
        );
        assert_eq!((retiring.connects.len(), retiring.teardowns.len()), (1, 1));

        // The connect finishes after its tab is gone: what it built is shut down, and held too.
        connect_tx.send(Ok(live_backend(&dir).1)).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !retiring.is_empty() {
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for the teardowns"
            );
            retiring.poll();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    #[test]
    fn a_watch_holds_until_its_worker_reports() {
        let (tx, rx) = mpsc::channel::<()>();
        let mut watch = ShutdownWatch {
            entries: vec![watched("t", rx, SESSION_CLOSE_BACKSTOP, |()| Vec::new())],
        };
        let now = std::time::Instant::now();
        assert!(watch.step(now));
        tx.send(()).unwrap();
        assert!(!watch.step(now));
        assert!(watch.is_empty());
    }

    #[test]
    fn a_watch_lets_go_at_the_backstop_or_when_the_worker_dies() {
        let (_tx, rx) = mpsc::channel::<()>();
        let mut watch = ShutdownWatch {
            entries: vec![watched("t", rx, SESSION_CLOSE_BACKSTOP, |()| Vec::new())],
        };
        let start = std::time::Instant::now();
        assert!(watch.step(start));
        assert!(!watch.step(start + SESSION_CLOSE_BACKSTOP + std::time::Duration::from_millis(1)));
        let (tx, rx) = mpsc::channel::<()>();
        let mut watch = ShutdownWatch {
            entries: vec![watched("t", rx, SESSION_CLOSE_BACKSTOP, |()| Vec::new())],
        };
        drop(tx);
        assert!(!watch.step(std::time::Instant::now()));
    }

    #[test]
    fn a_connect_that_finishes_during_the_close_is_held_through_its_teardown() {
        let (connect_tx, connect_rx) = mpsc::channel::<u32>();
        let (teardown_tx, teardown_rx) = mpsc::channel::<()>();
        let teardown_rx = RefCell::new(Some(teardown_rx));
        let mut watch = ShutdownWatch {
            entries: vec![watched(
                "connect",
                connect_rx,
                CONNECT_CLOSE_BACKSTOP,
                move |_backend: u32| {
                    vec![watched(
                        "teardown",
                        teardown_rx.borrow_mut().take().unwrap(),
                        SESSION_CLOSE_BACKSTOP,
                        |()| Vec::new(),
                    )]
                },
            )],
        };
        let now = std::time::Instant::now();
        connect_tx.send(7).unwrap();
        assert!(watch.step(now), "the teardown it started is held");
        teardown_tx.send(()).unwrap();
        assert!(!watch.step(now));
    }

    /// A closed tab's agent may still be writing while its backend shuts down (a subagent, during
    /// the shutdown's grace), so the window's presence stays as if a turn ran until that shutdown has
    /// returned: no revert in this window or another one can write meanwhile.
    #[test]
    fn closed_tab_keeps_presence_until_its_backend_has_shut_down() {
        use eitri_core::test_providers::{empty_trust_plan, ExternalRecordingProvider};
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("panel-retiring-presence");
        let (handle, page, _host) = handle_with_recording_page();
        let provider = std::sync::Arc::new(ExternalRecordingProvider::codex());
        let release = provider.hold_close();
        let (codex, survivor, guard) = {
            let mut state_ref = handle.state.borrow_mut();
            let tabs = &mut state_ref.tabs;
            let codex = tabs.open(AgentId::Codex);
            tabs.get_mut(codex).unwrap().set_backend(TabBackend::Live(
                AgentBackend::start_external_with(
                    AgentId::Codex,
                    provider.clone(),
                    &dir,
                    empty_trust_plan(),
                    SessionModeChoice::Auto,
                    None,
                )
                .unwrap(),
            ));
            let survivor = tabs.tabs().iter().find(|t| t.id != codex).unwrap().id;
            tabs.hold_presence(eitri_core::turn_review::PresenceHolder::start(
                dir.join(".review-state"),
                std::process::id(),
            ));
            (codex, survivor, tabs.presence_guard().unwrap())
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while matches!(guard.check(), Err(eitri_core::turn_review::Blocked::NotHeld(_))) {
            assert!(std::time::Instant::now() < deadline, "the presence was never held");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(guard.check(), Ok(()), "nothing runs yet");

        handle.run(|state, out| close_tab(state, codex, out)).unwrap();
        assert!(handle.state.borrow().tabs.get(codex).is_none());
        assert!(handle.state.borrow().tabs.get(survivor).is_some());
        page.visible.set(true);
        for _ in 0..5 {
            handle.tick();
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(
            guard.check(),
            Err(eitri_core::turn_review::Blocked::TurnRunningHere),
            "the closed tab's backend is still shutting down"
        );
        assert_eq!(provider.inner.closes(), 0, "its close is held");

        release.send(()).unwrap();
        while guard.check() != Ok(()) {
            assert!(
                std::time::Instant::now() < deadline,
                "the presence never let go: {:?}",
                guard.check()
            );
            handle.tick();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(provider.inner.closes(), 1, "released only after the shutdown returned");
    }

    /// Every worker is counted, and a worker that died (dropped its
    /// sender without reporting) neither hangs the collector nor counts as finished.
    #[test]
    fn the_close_collector_counts_every_worker_and_survives_one_dying() {
        let mut each = Vec::new();
        for n in 0..3 {
            let (tx, rx) = mpsc::channel();
            each.push(rx);
            std::thread::spawn(move || {
                if n == 1 {
                    drop(tx);
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(20 * n));
                let _ = tx.send(());
            });
        }
        let (done_tx, done_rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = done_tx.send(collect_reports(each));
        });
        let finished = done_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("the collector must not hang on a dead worker");
        assert_eq!(finished, 2);
    }

    /// What each tick of the close watch decides, including the two orderings that are judgements
    /// rather than mechanics.
    ///
    /// This is the whole of what `shell`'s own suite can say about the close path: it cannot open a
    /// window, so it cannot exercise `AgentPanelHandle::shutdown`, the application hold, or GTK's
    /// window destruction. What a test can pin is that no tick misreports what it saw.
    #[test]
    fn a_close_watch_tick_reports_what_it_saw() {
        // Still running, still inside the backstop: the application stays held.
        assert_eq!(
            close_watch_step::<()>(&Err(mpsc::TryRecvError::Empty), false),
            CloseWatch::KeepHolding
        );
        // Still running, past the deadline: the process gives up on it.
        assert_eq!(
            close_watch_step::<()>(&Err(mpsc::TryRecvError::Empty), true),
            CloseWatch::Expired
        );
        // A report ARRIVING on the same tick as the deadline is a report. A teardown that finished
        // must never be logged as an abandoned one -- that line names a possible orphaned `claude`.
        assert_eq!(close_watch_step(&Ok(()), true), CloseWatch::Finished);
        assert_eq!(close_watch_step(&Ok(()), false), CloseWatch::Finished);
        // A dead worker is a dead worker at any point in the window, never a timeout: the two send
        // a debugger to completely different places (a panic in `shutdown()` vs. a hang in it).
        assert_eq!(
            close_watch_step::<()>(&Err(mpsc::TryRecvError::Disconnected), true),
            CloseWatch::WorkerDied
        );
        assert_eq!(
            close_watch_step::<()>(&Err(mpsc::TryRecvError::Disconnected), false),
            CloseWatch::WorkerDied
        );
    }

    /// The close hands the writer the tabs as they are and holds the process until that write is on disk:
    /// a session adopted after the last tick is still kept.
    #[test]
    fn the_close_waits_for_the_last_saved_tabs_write() {
        let (state, dir) = restore_panel("restore-close", &[], &[]);
        let files = agent::state_dirs::test_workspace_dir("restore-close-files");
        state.borrow_mut().tab_memory = TabWriter::new(Some(files.clone()), dir.clone());
        let tab = state.borrow().tabs.active();
        let (provider, backend) = live_backend(&dir);
        state
            .borrow_mut()
            .tabs
            .get_mut(tab)
            .unwrap()
            .set_backend(TabBackend::Live(backend));
        provider.open_session("s-closing", &dir);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while state.borrow().tabs.get(tab).unwrap().provider_session_id().is_none() {
            assert!(std::time::Instant::now() < deadline, "the session id never arrived");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        // No tick ran since: the close itself hands the set over.
        let mut watch = handle_for(&state).shutdown();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while watch.step(std::time::Instant::now()) {
            assert!(std::time::Instant::now() < deadline, "the close never let go");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let loaded = match eitri_core::saved_tabs::load(&files, &dir) {
            eitri_core::saved_tabs::Loaded::Saved(saved) => saved,
            other => panic!("{other:?}"),
        };
        assert_eq!(loaded.tabs.len(), 1);
        assert_eq!(loaded.tabs[0].provider_session_id, "s-closing");
    }

    /// The close holds the process while the writer is inside the last write and lets go only once it is on
    /// disk: with the write held, every look keeps holding; released, the close ends with the tab saved.
    #[test]
    fn the_close_holds_while_the_last_saved_tabs_write_is_under_way() {
        let (state, dir) = restore_panel("restore-close-held", &[], &[]);
        let files = agent::state_dirs::test_workspace_dir("restore-close-held-files");
        let mut writer = TabWriter::new(Some(files.clone()), dir.clone());
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let release_rx = std::sync::Mutex::new(release_rx);
        writer.set_before_write(std::sync::Arc::new(move || {
            let _ = release_rx
                .lock()
                .unwrap()
                .recv_timeout(std::time::Duration::from_secs(10));
        }));
        state.borrow_mut().tab_memory = writer;
        {
            // A tab that keeps its session's id with no backend to tear down: the write is all the close owes.
            let mut s = state.borrow_mut();
            let tab = s.tabs.active();
            let t = s.tabs.get_mut(tab).unwrap();
            t.failed_resume = Some("s-held".to_string());
            t.set_backend(TabBackend::Failed {
                reason: "stopped".to_string(),
            });
        }
        let mut watch = handle_for(&state).shutdown();
        let held_until = std::time::Instant::now() + std::time::Duration::from_millis(300);
        while std::time::Instant::now() < held_until {
            assert!(
                watch.step(std::time::Instant::now()),
                "let go while the write was under way"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        release_tx.send(()).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while watch.step(std::time::Instant::now()) {
            assert!(std::time::Instant::now() < deadline, "the close never let go");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let loaded = match eitri_core::saved_tabs::load(&files, &dir) {
            eitri_core::saved_tabs::Loaded::Saved(saved) => saved,
            other => panic!("{other:?}"),
        };
        let ids: Vec<_> = loaded.tabs.iter().map(|t| t.provider_session_id.as_str()).collect();
        assert_eq!(ids, ["s-held"]);
    }
}
