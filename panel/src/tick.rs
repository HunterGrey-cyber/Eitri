//! The panel's tick: every tab's backend drained in one pass, what that pass owes the pages sent once it is
//! collected, and when the next one is due.

use super::*;

/// What the tick's first step drained from the backends for the rest of the tick: each stream payload
/// with the view whose page it goes to, and what the second step answers.
pub(super) struct Pumped {
    pub(super) streams: Vec<(ViewId, Stream)>,
    pub(super) turn_ended: Vec<TabId>,
    pub(super) offers_changed: Vec<TabId>,
    pub(super) queue_changed: Vec<TabId>,
    pub(super) review_hints: Vec<eitri_core::turn_review::ReviewHint>,
}

/// The tick's first step: the start notes, every connect and handoff that finished, every session that
/// never opened, the teardowns that returned, the supervisor's connect and status, and every tab's
/// backend drained (`TabSet::pump_views`). A tab the CLI-mode tripwire failed is shut down off this
/// thread, as a closed tab's is, and the shutdown denies what is still pending in it; a tab on screen in
/// some page also says why there, as a session that never opened does.
pub(super) fn tick_collect(state: &mut AgentPanelState, screen: &Screen, out: &mut Outbox) -> Pumped {
    let notes = state.start_notes.drain();
    out.send_all(ViewId::BASE, notes);
    collect_pending_starts(state, screen, out);
    restore_once_released(state, screen, out);
    collect_pending_handoffs(state, out);
    inbound::collect_task_commands(state, out);
    report_sessions_that_never_opened(state, screen, out);
    for tab in state.retiring.poll() {
        state.tabs.retire_done(tab);
    }
    collect_history(state, out);
    // A supervisor this window had to start itself finishes connecting here rather than
    // during `build_ui`, where waiting for it would have delayed the window appearing.
    if state.supervisor.is_none() {
        if let Some(rx) = state.supervisor_pending.as_ref() {
            match rx.try_recv() {
                Ok(client) => {
                    state.supervisor = client;
                    state.supervisor_pending = None;
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    state.supervisor_pending = None;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
            }
        }
    }
    // The project root travels in because the permission policy judges `Read`/`Grep`/`Glob`
    // paths against it -- see `AgentBackend::answer_what_needs_no_human`. A view counts as
    // mapped only while its page is live and on screen.
    let project_dir = state.project_dir.clone();
    let pumped = state.tabs.pump_views(&project_dir, &|view| screen.shows(view));
    let statuses: Vec<supervisor::AgentStatus> = state
        .tabs
        .tabs()
        .iter()
        .map(|t| {
            // Bound first: `projection()` returns a guard on the sidecar path, and passing
            // it inline would drop the ingestion lock before `derive_status` had read
            // through it.
            let projection = t.live().map(|b| b.projection());
            crate::supervisor_client::derive_status(projection.as_deref())
        })
        .collect();
    let status = crate::supervisor_client::aggregate_status(statuses);
    if let Some(supervisor) = state.supervisor.as_mut() {
        supervisor.send_status(status);
    }
    // Whose first text each payload carries, kept with it: the cadence may release it after
    // the tab, or that tab's turn, has changed (`panel_pacer::FirstText`).
    let mut streams: Vec<(ViewId, Stream)> = Vec::new();
    if let Some(payload) = pumped.active_payload {
        let active = state.tabs.active();
        let first_text = if pumped.first_text {
            first_text_of(&state.tabs, active)
        } else {
            None
        };
        streams.push((ViewId::BASE, (payload, first_text, pumped.active_class)));
    }
    for shown in pumped.view_payloads {
        let first_text = if shown.first_text {
            first_text_of(&state.tabs, shown.tab)
        } else {
            None
        };
        streams.push((shown.view, (shown.payload, first_text, shown.class)));
    }
    for tripped in pumped.tripped {
        let shown = state.shown_in(tripped.tab);
        state.retiring.backend(tripped.retired);
        if let Some(view) = shown {
            out.send(view, serialize_error_for_js(tripped.tab, &tripped.reason));
        }
    }
    Pumped {
        streams,
        turn_ended: pumped.turn_ended,
        offers_changed: pumped.offers_changed,
        queue_changed: pumped.queue_changed,
        review_hints: pumped.review_hints,
    }
}

/// The tick's second step, after the streams went through each page's cadence: what the pump said
/// changed, then every poll, then each page's `tabs` and `hello` if what they describe changed, the
/// attention, and the extra views left holding no tab.
pub(super) fn tick_send(
    state: &mut AgentPanelState,
    screen: &Screen,
    pumped: Pumped,
    attention_before: eitri_core::attention::Attention,
    out: &mut Outbox,
) {
    // A message put into a turn was read (its row went out above) or is held again.
    for tab in pumped.queue_changed {
        send_queue(state, tab, out);
    }
    // Prompts a linked window held until its presence marked their turns as running.
    for sent in state.tabs.poll_turn_sends(std::time::Instant::now()) {
        apply_sent(state, screen, sent, out);
    }
    // A held message whose tab left without `close_tab`, which answers it (the window closing), is
    // answered by no one.
    let tabs = &state.tabs;
    state.owed.retain(|_, answer| match answer.what {
        Owed::HeldPrompt(tab) => tabs.send_is_held(tab),
        _ => true,
    });
    // A turn ended in these tabs; their queued messages go out now.
    for tab in pumped.turn_ended {
        if let Some(flush) = state.tabs.flush_queue(tab) {
            apply_flush(state, screen, tab, flush, out);
        }
    }
    // A message a refused send left goes into the turn its tab now shows running; one typed behind a
    // card goes out once the card is answered.
    for tab in state.tabs.steer_waiting(super::inbound::now_ms()) {
        send_queue(state, tab, out);
    }
    for (tab, flush) in state.tabs.release_after_card() {
        apply_flush(state, screen, tab, flush, out);
    }
    // A turn's end snapshot landed: the status band's hint, in the page of the hint's tab's view. A
    // page that has not said `ready` (still loading, or reloading) has nothing to show it, so it
    // waits here for the page; the review itself is asked for by `c`, never by this.
    forget_hints_of_running_or_closed_tabs(state);
    let review_hints = hints_to_keep(state.review_hint, pumped.review_hints);
    keep_newest_hint_per_tab(&mut state.review_hints, review_hints);
    let hints = take_hints_for_ready_pages(state);
    if !hints.is_empty() {
        let sent: Vec<_> = hints.iter().map(|(_, hint)| hint.clone()).collect();
        keep_shown_hints(&mut state.shown_hints, &sent);
    }
    for (view, hint) in &hints {
        out.send(*view, eitri_core::agent_bridge::serialize_review_hint_for_js(hint));
    }
    // A shown tab's offers changed (a card arrived or went): its page's third buttons follow.
    for tab in pumped.offers_changed {
        let payload = state.page_of(tab).and_then(|view| {
            state.tabs.get(tab).map(|t| {
                (
                    view,
                    eitri_core::agent_bridge::serialize_rule_offers_for_js(t.id, &t.session.rule_offers),
                )
            })
        });
        if let Some((view, payload)) = payload {
            out.send(view, payload);
        }
    }
    send_context_if_changed(state, out);
    poll_scratch_edits(state, out);
    poll_review_jobs(state, out);
    poll_question_sends(state, out);
    poll_proposal_jobs(state, out);
    poll_review_flow(state, screen, out);
    // Before the tabs envelope, so a session the gate started this tick shows as starting at once.
    poll_trust(state, screen, out);
    apply_title_reads(state);
    send_chooser_titles_if_changed(state, out);
    send_tabs_if_changed(state, out);
    remember_tabs(state);
    send_hello_if_open_sessions_changed(state, out);
    // After the payload, not before it: a reaction that sends the panel an envelope -- one
    // that lands on a card, say -- must find the card it names already there.
    report_attention(state, attention_before, out);
    judge_empty_views(state);
    tell_emptied_views(state, out);
}

/// The oldest pending card among `view`'s tabs.
pub(super) fn oldest_card_in(tabs: &eitri_core::tab_set::TabSet, view: ViewId) -> Option<TabId> {
    let stamps: Vec<(TabId, Option<u64>)> = tabs
        .tabs()
        .iter()
        .filter(|t| t.view_id() == view)
        .map(|t| (t.id, t.attention.oldest_pending_stamp()))
        .collect();
    eitri_core::tabs::oldest_card(&stamps)
}

/// Once the restore has settled, every attached extra view still holding no tab is owed to the
/// window as empty, once per window.
pub(super) fn judge_empty_views(state: &mut AgentPanelState) {
    if state.empty_views_judged || state.shutting_down || !restore_settled(state) {
        return;
    }
    state.empty_views_judged = true;
    let empty: Vec<ViewId> = state
        .pages
        .keys()
        .copied()
        .filter(|view| !view.is_base() && state.tabs.tab_in(*view).is_none())
        .collect();
    for view in empty {
        if !state.emptied_views.contains(&view) {
            state.emptied_views.push(view);
        }
    }
}

/// Owes the window each extra view left empty since it was last told, once each: the window removes the
/// view and idles its page, which comes back into the panel (so it is told after the step, never from
/// inside it).
pub(super) fn tell_emptied_views(state: &mut AgentPanelState, out: &mut Outbox) {
    let mut views = std::mem::take(&mut state.emptied_views);
    views.dedup();
    // A view given a tab again since (a restore, a new tab) is not empty any more.
    views.retain(|view| state.tabs.tab_in(*view).is_none());
    for view in views {
        eprintln!("[agent_panel] view {}: no tab left", view.number());
        out.effect(Effect::ViewEmpty(view));
    }
}

/// Owes the attention hook a report, if the window's value changed or any tab's own did (or a tab moved
/// to another view): each chat view's chip shows only its own tabs, so a view that turns unread while
/// another already is must still be redrawn, and two views handed a card in one pump must both be
/// named. The values are taken now, where the change happened; the hook reaches the layout and the top
/// bar, and is called once the step is over.
pub(super) fn report_attention(
    state: &mut AgentPanelState,
    before: eitri_core::attention::Attention,
    out: &mut Outbox,
) {
    if state.shutting_down {
        return;
    }
    report_busy(state, out);
    let after = state.tabs.attention();
    let now = state.tabs.tab_attentions();
    if after == before && now == state.reported_tabs {
        return;
    }
    let handed = eitri_core::tabs::views_handed_a_card(&state.reported_tabs, &now);
    state.reported_tabs = now;
    out.effect(Effect::Attention { before, after, handed });
}

/// Running-tab and queue counts invalidate the window's counts even when no attention mark moved.
pub(super) fn report_busy(state: &mut AgentPanelState, out: &mut Outbox) {
    if state.shutting_down {
        return;
    }
    let now = state.tabs.view_activity();
    if now == state.reported_activity {
        return;
    }
    let mut changed: Vec<_> = now
        .iter()
        .filter(|entry| !state.reported_activity.contains(entry))
        .map(|(view, activity)| (*view, activity.running > 0))
        .collect();
    changed.extend(
        state
            .reported_activity
            .iter()
            .filter(|(view, _)| !now.iter().any(|(current, _)| current == view))
            .map(|(view, _)| (*view, false)),
    );
    state.reported_activity = now;
    out.effect(Effect::Busy { changed });
}

/// The window's wake, handed to every backend the tabs hold. A pair that cannot be made is logged, and the
/// tick then runs on its steady cadence as it always did.
pub(super) fn panel_wake(tabs: &mut eitri_core::tab_set::TabSet) -> Option<crate::panel_wake::PanelWake> {
    match crate::panel_wake::PanelWake::new() {
        Ok(wake) => {
            tabs.set_ui_wake(wake.waker());
            Some(wake)
        }
        Err(e) => {
            eprintln!(
                "[agent_panel] the panel's wake could not be made ({e}); it ticks every {:?}",
                AgentPanelHandle::TICK
            );
            None
        }
    }
}

/// Whether something the tick acts on can become ready without raising the wake, so the tick has to run on
/// its steady cadence: a worker reporting through a channel (a connect, a teardown, the supervisor's
/// connect, the trust check, a review or scratch job, the history's writer), a backend that does not announce
/// its folds, the review flow waiting on the editor, or a move waiting on its page or its presence.
pub(super) fn needs_polling(state: &AgentPanelState) -> bool {
    state.wake.is_none()
        || state.chat_moves.is_waiting()
        || state.tabs.needs_polling()
        || !state.retiring.is_empty()
        || state.supervisor_pending.is_some()
        || !state.pending_edits.is_empty()
        || !state.task_commands.is_empty()
        || !state.review_jobs.is_empty()
        || !state.question_sends.is_empty()
        || !state.proposal_jobs.is_empty()
        || !state.review_flow.is_idle()
        || state.trust.is_busy()
        || state.history_writer.is_busy()
}

/// One stream payload for a page's pacer: the envelope, whose first text it carries, and its class.
pub(super) type Stream = (
    String,
    Option<crate::panel_pacer::FirstText>,
    eitri_core::panel_cadence::EnvelopeClass,
);

/// The first text `tab`'s turn trace waits for, as the pacer stamps it: that tab and turn, never
/// whichever tab is on screen when the stamp lands.
pub(super) fn first_text_of(tabs: &eitri_core::tab_set::TabSet, tab: TabId) -> Option<crate::panel_pacer::FirstText> {
    tabs.get(tab)
        .and_then(|t| t.session.turn_trace.as_ref())
        .map(|trace| crate::panel_pacer::FirstText {
            tab,
            turn: trace.submitted_at(),
        })
}

/// Applies the pacer's first-text stamps (`panel_pacer::Sent::first_text`): each to the tab and turn
/// its envelope was pumped for, at the moment it was handed to the page -- never to whichever tab
/// is active when this runs, and never to a later turn of the same tab
/// (`TurnTrace::mark_first_text_dispatched_for`). Called by the pump tick and before a
/// `turn_rendered` report is folded.
pub(super) fn stamp_first_text_dispatches(
    state_ref: &mut AgentPanelState,
    stamps: Vec<(crate::panel_pacer::FirstText, std::time::Instant)>,
) {
    for (first, at) in stamps {
        if let Some(trace) = state_ref
            .tabs
            .get_mut(first.tab)
            .and_then(|t| t.session.turn_trace.as_mut())
        {
            if trace.mark_first_text_dispatched_for(first.turn, at) && trace.is_complete() {
                trace.emit();
            }
        }
    }
}

/// The first texts the pacer threw away (`Pacer::discard_pending`: a reload, a fresh `ready`, the
/// give-up page) reach the panel inside a snapshot, which it reports no paint for: each turn's
/// trace stops waiting for one, and prints now if that leaves nothing else to wait for -- no later
/// dispatch of this turn is coming to print it if the turn has already ended.
pub(super) fn trace_discarded_first_texts(
    state_ref: &mut AgentPanelState,
    discarded: Vec<crate::panel_pacer::FirstText>,
) {
    for first in discarded {
        if let Some(trace) = state_ref
            .tabs
            .get_mut(first.tab)
            .and_then(|t| t.session.turn_trace.as_mut())
        {
            if trace.mark_first_text_in_snapshot_for(first.turn) && trace.is_complete() {
                trace.emit();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_panel::tests::*;

    #[test]
    fn the_tick_runs_until_the_close_and_never_after() {
        let (handle, page) = handle_with_strict_page();
        handle.page_message(ViewId::BASE, r#"{"type":"ready","request_id":"r1"}"#);
        assert!(handle.tick());
        assert!(handle.tick());
        let watch = handle.shutdown();
        assert!(watch.is_empty(), "no tab ever started a backend");
        assert!(!handle.tick());
        let before = page.inner.sent.borrow().len();
        handle.page_message(ViewId::BASE, r#"{"type":"ready","request_id":"r2"}"#);
        assert_eq!(
            page.inner.sent.borrow().len(),
            before,
            "a closing panel answers nothing"
        );
    }

    /// What one tick of a settled panel costs: a ready page, one empty tab, a selection in the
    /// editor, nothing happening. For a measurement run by hand (`--ignored --nocapture`).
    #[test]
    #[ignore]
    fn measure_an_idle_tick() {
        let (handle, page) = handle_with_strict_page();
        page.inner.visible.set(true);
        let context = eitri_core::editor_context::EditorContext {
            file: "/p/src/main.rs".into(),
            selection: Some(eitri_core::editor_context::Selection {
                start_line: 10,
                end_line: 60,
                text: "x".repeat(2000),
            }),
        };
        handle.state.borrow_mut().editor_context = Rc::new(move || Some(context.clone()));
        handle.state.borrow_mut().project_dir = PathBuf::from("/p");
        handle.page_message(ViewId::BASE, r#"{"type":"ready","request_id":"r1"}"#);
        for _ in 0..10 {
            handle.tick();
        }
        let sent = page.inner.sent.borrow().len();
        let ticks = 1000;
        let started = std::time::Instant::now();
        let (_, bytes) = eitri_core::alloc_count::bytes_allocated_by(|| {
            for _ in 0..ticks {
                handle.tick();
            }
        });
        let elapsed = started.elapsed();
        assert_eq!(page.inner.sent.borrow().len(), sent, "an idle tick sends nothing");
        eprintln!(
            "idle tick: {:?} and {} bytes allocated each",
            elapsed / ticks,
            bytes / ticks as usize
        );
    }

    /// Idle, the tick waits for the wake; every call into the panel and every page message raises it, and
    /// the tick it brings sends what the call changed.
    #[test]
    fn a_settled_panel_waits_and_a_call_into_it_wakes_the_tick() {
        let (handle, page) = settled_panel();
        let now = std::time::Instant::now();
        assert_eq!(handle.take_wake(now), None, "nothing raised");
        let sent = page.sent.borrow().len();
        handle.new_tab();
        let due = handle.take_wake(now).expect("a call into the panel wakes it");
        assert_eq!(handle.run_tick(due), crate::panel_wake::NextTick::OnWake);
        assert!(
            page.sent.borrow()[sent..].iter().any(|p| kind_of(p) == "tabs"),
            "the new tab reached the page"
        );
        handle.page_message(ViewId::BASE, r#"{"type":"ready","request_id":"r2"}"#);
        assert!(handle.take_wake(now).is_some(), "a page message wakes it");
    }

    /// A wake runs the tick no sooner than one `TICK` after the last one, so a stream of folds is taken in
    /// the batches a steady tick made; after a quiet stretch it runs at once.
    #[test]
    fn a_wake_runs_the_tick_no_sooner_than_one_tick_after_the_last() {
        let (handle, _page) = settled_panel();
        let at = std::time::Instant::now();
        handle.run_tick(at);
        handle.wake();
        assert_eq!(
            handle.take_wake(at + std::time::Duration::from_millis(5)),
            Some(at + AgentPanelHandle::TICK)
        );
        handle.wake();
        let later = at + std::time::Duration::from_secs(2);
        assert_eq!(handle.take_wake(later), Some(later));
    }

    /// An agent's events wake the tick: the backend's fold raises the wake, and the tick it brings sends
    /// them to the page. Then it waits again.
    #[test]
    fn a_backends_fold_wakes_the_tick_and_reaches_the_page() {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("panel-fold-wakes");
        let (handle, page) = settled_panel();
        let (provider, backend) = live_backend(&dir);
        {
            let mut state = handle.state.borrow_mut();
            let tab = state.tabs.active();
            state.tabs.get_mut(tab).unwrap().set_backend(TabBackend::Live(backend));
        }
        // A backend put in place without the wake is polled; the tick that finds it tells it about the
        // wake (which raises it once), and runs once more.
        let at = std::time::Instant::now();
        assert_eq!(
            handle.run_tick(at),
            crate::panel_wake::NextTick::At(at + AgentPanelHandle::TICK)
        );
        let due = wait_for_wake(&handle);
        assert_eq!(handle.run_tick(due), crate::panel_wake::NextTick::OnWake);
        let sent = page.sent.borrow().len();
        provider.queue(agent::AgentDomainEvent::TurnStarted { turn_id: "t1".into() });
        let due = wait_for_wake(&handle);
        assert_eq!(
            handle.run_tick(due),
            crate::panel_wake::NextTick::OnWake,
            "nothing left to poll"
        );
        assert!(
            page.sent.borrow()[sent..].iter().any(|p| kind_of(p) == "events"),
            "the fold reached the page: {:?}",
            &page.sent.borrow()[sent..]
        );
        let tabs = handle.state.borrow_mut().tabs.take_all();
        for tab in tabs {
            if let Some(retired) = tab.retired {
                retired.into_backend().shutdown();
            }
        }
    }

    /// A connect reports through a channel nobody is told about: the tick runs on its steady cadence while
    /// it is pending, collects it, runs once more, and then waits.
    #[test]
    fn a_pending_connect_is_polled_until_collected_and_once_more() {
        use crate::panel_wake::NextTick;
        let (handle, _page) = settled_panel();
        let (connect_tx, result_rx) = mpsc::channel::<Result<AgentBackend, BackendError>>();
        {
            let mut state = handle.state.borrow_mut();
            let tab = state.tabs.active();
            state
                .tabs
                .get_mut(tab)
                .unwrap()
                .set_backend(TabBackend::Starting(PendingStart {
                    request_id: "req-send".into(),
                    result_rx,
                    first_turn: None,
                    resume: None,
                    resumed_title: None,
                    resumed_name: None,
                }));
        }
        let at = std::time::Instant::now();
        assert_eq!(
            handle.run_tick(at),
            NextTick::At(at + AgentPanelHandle::TICK),
            "pending: polled"
        );
        drop(connect_tx);
        let at = at + AgentPanelHandle::TICK;
        assert_eq!(
            handle.run_tick(at),
            NextTick::At(at + AgentPanelHandle::TICK),
            "collected: once more"
        );
        let at = at + AgentPanelHandle::TICK;
        assert_eq!(handle.run_tick(at), NextTick::OnWake, "then it waits");
    }

    /// Each pending thing the tick polls keeps it on its steady cadence: a closed tab's teardown, the
    /// supervisor's connect, the trust check, a review job, a scratch edit, the history's write. None of
    /// them, nothing to poll.
    #[test]
    fn every_unannounced_pending_job_keeps_the_tick_polling() {
        let (handle, _page) = settled_panel();
        let polls = |handle: &AgentPanelHandle| needs_polling(&handle.state.borrow());
        assert!(!polls(&handle));

        let (_task_tx, task_rx) = mpsc::channel();
        handle
            .state
            .borrow_mut()
            .task_commands
            .push(inbound::PendingTaskCommand {
                view: ViewId::BASE,
                tab: TabId(1),
                request_id: "task1".into(),
                task_id: "task".into(),
                reply: inbound::TaskCommandReply::Stop(task_rx),
            });
        assert!(polls(&handle), "a task RPC whose answer has no UI wake");
        handle.state.borrow_mut().task_commands.clear();
        assert!(!polls(&handle));

        let (done_tx, done_rx) = mpsc::channel();
        handle.state.borrow_mut().retiring.teardowns.push((None, done_rx));
        assert!(polls(&handle), "a teardown");
        done_tx.send(()).unwrap();
        handle.state.borrow_mut().retiring.poll();
        assert!(!polls(&handle));

        let (supervisor_tx, supervisor_rx) = mpsc::channel();
        handle.state.borrow_mut().supervisor_pending = Some(supervisor_rx);
        assert!(polls(&handle), "the supervisor's connect");
        drop(supervisor_tx);
        handle.state.borrow_mut().supervisor_pending = None;

        {
            let mut state = handle.state.borrow_mut();
            state.trust.wait_for(TabId(1), &Profile::Claude);
            state.trust.begin_check(&Profile::Claude);
        }
        assert!(polls(&handle), "the trust check's answer");
        let _ = handle.state.borrow_mut().trust.poll();
        assert!(!polls(&handle));

        let (_review_tx, review_rx) = mpsc::channel();
        handle.state.borrow_mut().review_jobs.push(PendingReview {
            request_id: "r".into(),
            view: ViewId::BASE,
            rx: review_rx,
            clears_hint: None,
        });
        assert!(polls(&handle), "a review job");
        handle.state.borrow_mut().review_jobs.clear();

        let (_proposal_tx, proposal_rx) = mpsc::channel();
        handle.state.borrow_mut().proposal_jobs.push(PendingProposal {
            request_id: "p".into(),
            view: ViewId::BASE,
            name: "a.rs".into(),
            rx: proposal_rx,
        });
        assert!(polls(&handle), "a proposal being read");
        handle.state.borrow_mut().proposal_jobs.clear();

        let mut scratch = eitri_core::scratch::ScratchDir::new().expect("a scratch directory");
        let (_request, edit) = scratch.prepare_edit("draft").unwrap();
        handle.state.borrow_mut().pending_edits.push(edit);
        assert!(polls(&handle), "a scratch edit");
        for edit in std::mem::take(&mut handle.state.borrow_mut().pending_edits) {
            edit.cleanup();
        }
        assert!(!polls(&handle));

        let history = agent::state_dirs::test_workspace_dir("panel-history-polls").join("eitri");
        std::fs::create_dir_all(&history).unwrap();
        let mut writer =
            eitri_core::prompt_history::HistoryWriter::new(Some(history.join("history")), "/projects/polls".into());
        writer.submit(&["a prompt".to_string()]);
        handle.state.borrow_mut().history_writer = writer;
        assert!(polls(&handle), "the history's write");
        handle.state.borrow_mut().history_writer.wait_answered();
        assert!(!polls(&handle));
    }

    /// A prompt reaches the window's copy and its page at once, with no disk touched on this thread; the
    /// history's writer writes it, the tick polls while that is out, and the writer's report brings in the
    /// prompt another window of the project had appended meanwhile.
    #[test]
    fn a_remembered_prompt_shows_at_once_and_the_writers_report_brings_in_another_windows() {
        let (handle, page) = settled_panel();
        let state_root = agent::state_dirs::test_workspace_dir("panel-history").join("eitri");
        std::fs::create_dir_all(&state_root).unwrap();
        let dir = state_root.join("history");
        let root = std::path::PathBuf::from("/projects/panel-history");
        eitri_core::prompt_history::append(&dir, &root, &["from the other window".to_string()]).unwrap();
        handle.state.borrow_mut().history_writer =
            eitri_core::prompt_history::HistoryWriter::new(Some(dir.clone()), root.clone());
        page.sent.borrow_mut().clear();

        handle.run(|state, out| remember_prompts(state, &["mine".to_string()], out));
        assert_eq!(handle.state.borrow().history, vec!["mine".to_string()]);
        assert_eq!(kinds(&page.sent.borrow()), vec!["history"], "told at once");
        assert!(needs_polling(&handle.state.borrow()), "the write is out");

        let mut at = std::time::Instant::now();
        while handle.state.borrow().history_writer.is_busy() {
            at += AgentPanelHandle::TICK;
            handle.run_tick(at);
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        handle.run_tick(at + AgentPanelHandle::TICK);
        assert_eq!(
            handle.state.borrow().history,
            vec!["from the other window".to_string(), "mine".to_string()]
        );
        assert_eq!(
            kinds(&page.sent.borrow()),
            vec!["history", "history"],
            "told again with the file's"
        );
        assert!(!needs_polling(&handle.state.borrow()));
        assert_eq!(
            eitri_core::prompt_history::load(&dir, &root),
            eitri_core::prompt_history::Loaded::Entries(vec!["from the other window".to_string(), "mine".to_string()])
        );
    }

    /// The review flow's work (here: the recovery journal read it starts once a document is ready and a
    /// review exists) reports through a worker nobody is told about: the tick runs on its steady cadence
    /// until the flow is idle, and then waits.
    #[test]
    fn a_busy_review_flow_keeps_the_tick_polling_until_it_is_idle() {
        use crate::panel_wake::NextTick;
        agent::state_dirs::redirect_state_to_a_test_root();
        let project = agent::state_dirs::test_workspace_dir("panel-review-flow-polls");
        let review_dir = agent::state_dirs::test_workspace_dir("panel-review-flow-polls-store");
        let (handle, _page) = settled_panel();
        {
            let mut state = handle.state.borrow_mut();
            let state = &mut *state;
            let options = eitri_core::turn_review::ReviewOptions {
                excludes: Some(None),
                trim_every: std::time::Duration::MAX,
                ..eitri_core::turn_review::ReviewOptions::default()
            };
            state
                .tabs
                .install_turn_review(eitri_core::turn_review::TurnReview::with_options(
                    Some(review_dir),
                    &project,
                    options,
                ))
                .unwrap();
            let _ = state.review_flow.document_ready(&state.tabs);
            assert!(!state.review_flow.is_idle(), "the journal read is out");
        }
        assert!(needs_polling(&handle.state.borrow()), "a busy review flow is polled");
        let mut at = std::time::Instant::now();
        let deadline = at + std::time::Duration::from_secs(10);
        loop {
            let next = handle.run_tick(at);
            if handle.state.borrow().review_flow.is_idle() && !needs_polling(&handle.state.borrow()) {
                break;
            }
            assert_eq!(next, NextTick::At(at + AgentPanelHandle::TICK), "polled while busy");
            assert!(std::time::Instant::now() < deadline, "the review flow never went idle");
            std::thread::sleep(std::time::Duration::from_millis(5));
            at += AgentPanelHandle::TICK;
        }
        // The tick that saw it go idle asked for one more; the one after waits.
        let next = handle.run_tick(at + AgentPanelHandle::TICK);
        assert_eq!(next, NextTick::OnWake);
    }

    /// The typing cadence's deadlines: the first key of a typing window wakes the tick, which tells the
    /// page and is due again at the window's end; later keys wake nothing.
    #[test]
    fn the_typing_cadence_names_when_the_tick_is_due() {
        use crate::panel_wake::NextTick;
        let (handle, page) = settled_panel();
        handle.set_typing_cadence(Some(2));
        handle.set_editor_has_keys(true);
        let at = std::time::Instant::now();
        handle.take_wake(at);
        handle.run_tick(at);
        handle.note_editor_key();
        let due = handle
            .take_wake(std::time::Instant::now())
            .expect("the first key wakes the tick");
        handle.note_editor_key();
        assert_eq!(
            handle.take_wake(std::time::Instant::now()),
            None,
            "a later key does not"
        );
        let sent = page.sent.borrow().len();
        let NextTick::At(end) = handle.run_tick(due) else {
            panic!("the window's end is due")
        };
        assert!(
            page.sent.borrow()[sent..].iter().any(|p| p.contains("\"typing\":true")),
            "the page was told"
        );
        assert!(end > due && end <= std::time::Instant::now() + eitri_core::panel_cadence::TYPING_WINDOW);
        std::thread::sleep(end.saturating_duration_since(std::time::Instant::now()));
        let next = handle.run_tick(end);
        assert!(
            page.sent.borrow().iter().any(|p| p.contains("\"typing\":false")),
            "the end was told"
        );
        assert_eq!(next, NextTick::OnWake);
    }

    /// The close stops the tick for good.
    #[test]
    fn the_close_stops_the_tick() {
        let (handle, _page) = settled_panel();
        let _ = handle.shutdown();
        assert!(
            handle.take_wake(std::time::Instant::now()).is_some(),
            "the close wakes it"
        );
        assert_eq!(
            handle.run_tick(std::time::Instant::now()),
            crate::panel_wake::NextTick::Stop
        );
    }

    #[test]
    fn the_busy_hook_hears_a_turn_start_and_end_with_no_card() {
        let dir = agent::state_dirs::test_workspace_dir("panel-busy-turn");
        let (handle, _page) = settled_panel();
        let (tab, provider) = live_tab(&handle, &dir);
        let heard = Rc::new(RefCell::new(Vec::new()));
        let calls = heard.clone();
        let state = Rc::downgrade(&handle.state);
        handle.on_busy(move |changed| {
            assert!(
                state.upgrade().unwrap().try_borrow_mut().is_ok(),
                "the step has released its state"
            );
            calls.borrow_mut().push(changed.to_vec());
        });
        handle.tick();
        heard.borrow_mut().clear();
        provider.queue(agent::AgentDomainEvent::TurnStarted { turn_id: "t1".into() });
        tick_handle_until(&handle, "running report", || !heard.borrow().is_empty());
        assert_eq!(
            heard.borrow_mut().drain(..).collect::<Vec<_>>(),
            vec![vec![(ViewId::BASE, true)]]
        );
        assert_eq!(handle.state.borrow().tabs.attention().pending, 0);
        handle
            .state
            .borrow_mut()
            .tabs
            .queue_message(tab, "later", "later".into(), 1, false, agent::PromptOrigin::Typed)
            .unwrap();
        handle.tick();
        assert_eq!(heard.borrow_mut().pop(), Some(vec![(ViewId::BASE, true)]));
        provider.queue(agent::AgentDomainEvent::TurnCompleted {
            turn_id: "t1".into(),
            outcome: agent::TurnOutcome::Completed,
            result_text: String::new(),
            stop_reason: None,
            usage: None,
            detail: Default::default(),
        });
        tick_handle_until(&handle, "turn end report", || !heard.borrow().is_empty());
        assert_eq!(heard.borrow_mut().pop(), Some(vec![(ViewId::BASE, false)]));
        assert_eq!(handle.state.borrow().tabs.queued_count(), 0);
        handle.tick();
        assert!(heard.borrow().is_empty());
        let _ = handle.shutdown();
    }

    #[test]
    fn the_busy_hook_hears_each_running_tab_change_in_one_view() {
        let dir = agent::state_dirs::test_workspace_dir("panel-busy-two-tabs");
        let (handle, _page) = settled_panel();
        let (first, backend) = live_backend(&dir);
        {
            let mut state = handle.state.borrow_mut();
            let tab = state.tabs.active();
            state.tabs.get_mut(tab).unwrap().set_backend(TabBackend::Live(backend));
        }
        let (_, second) = live_tab(&handle, &dir);
        let heard = Rc::new(RefCell::new(Vec::new()));
        let calls = heard.clone();
        handle.on_busy(move |changed| calls.borrow_mut().push(changed.to_vec()));
        handle.tick();
        heard.borrow_mut().clear();

        for (provider, running) in [(&first, 1), (&second, 2)] {
            provider.queue(agent::AgentDomainEvent::TurnStarted { turn_id: "t1".into() });
            tick_handle_until(&handle, "running tab count", || {
                handle.chat_summary().running == running
            });
            handle.tick();
            assert_eq!(
                heard.borrow_mut().drain(..).collect::<Vec<_>>(),
                vec![vec![(ViewId::BASE, true)]]
            );
        }
        for (provider, running) in [(&second, 1), (&first, 0)] {
            provider.queue(agent::AgentDomainEvent::TurnCompleted {
                turn_id: "t1".into(),
                outcome: agent::TurnOutcome::Completed,
                result_text: String::new(),
                stop_reason: None,
                usage: None,
                detail: Default::default(),
            });
            tick_handle_until(&handle, "ended tab count", || handle.chat_summary().running == running);
            handle.tick();
            assert_eq!(
                heard.borrow_mut().drain(..).collect::<Vec<_>>(),
                vec![vec![(ViewId::BASE, running > 0)]]
            );
        }
        assert_eq!(handle.chat_summary().queued, 0);
        handle.tick();
        assert!(heard.borrow().is_empty());
        let _ = handle.shutdown();
    }

    #[test]
    fn the_busy_hook_hears_a_change_whose_attention_did_not_move() {
        let dir = agent::state_dirs::test_workspace_dir("panel-busy-queue");
        let (handle, _page) = settled_panel();
        let (tab, _provider) = live_tab(&handle, &dir);
        let heard = Rc::new(RefCell::new(Vec::new()));
        let calls = heard.clone();
        handle.on_busy(move |changed| calls.borrow_mut().push(changed.to_vec()));
        handle.tick();
        heard.borrow_mut().clear();
        handle
            .state
            .borrow_mut()
            .tabs
            .queue_message(tab, "later", "later".into(), 1, false, agent::PromptOrigin::Typed)
            .unwrap();
        stepped(&handle.state, |state, out| {
            let before = state.tabs.attention();
            report_attention(state, before, out);
        });
        assert_eq!(heard.borrow_mut().pop(), Some(vec![(ViewId::BASE, false)]));
        let before = handle.state.borrow().tabs.attention();
        handle.page_message(
            ViewId::BASE,
            &serde_json::json!({
                "type": "queue_message",
                "request_id": "q1",
                "tab": tab.0,
                "text": "next",
            })
            .to_string(),
        );
        handle.tick();
        assert_eq!(handle.state.borrow().tabs.queued_count(), 0);
        assert_eq!(handle.state.borrow().tabs.attention(), before);
        assert_eq!(heard.borrow_mut().pop(), Some(vec![(ViewId::BASE, false)]));
        let _ = handle.shutdown();
    }

    #[test]
    fn the_busy_hook_reports_views_arriving_and_leaving_and_stops_at_shutdown() {
        let state = state_for_hooks(TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto));
        let heard = Rc::new(RefCell::new(Vec::new()));
        let calls = heard.clone();
        state.borrow_mut().busy_hook = Some(Rc::new(move |changed| calls.borrow_mut().push(changed.to_vec())));
        stepped(&state, report_busy);
        assert_eq!(heard.borrow_mut().pop(), Some(vec![(ViewId::BASE, false)]));
        let two = ViewId::new(2).unwrap();
        stepped(&state, |state, out| {
            state.tabs.open(eitri_core::tab_set::AgentId::Claude);
            state.tabs.break_out(two).unwrap();
            report_busy(state, out);
        });
        assert_eq!(heard.borrow_mut().pop(), Some(vec![(two, false)]));
        stepped(&state, |state, out| {
            state.tabs.join_back(two).unwrap();
            report_busy(state, out);
        });
        assert_eq!(heard.borrow_mut().pop(), Some(vec![(two, false)]));
        stepped(&state, |state, out| {
            state.shutting_down = true;
            state.tabs.take_all();
            let before = state.tabs.attention();
            report_attention(state, before, out);
            report_busy(state, out);
        });
        assert!(heard.borrow().is_empty());
    }

    /// Each chat view's chip shows its own tabs, so the hook hears a tab's own change even when the
    /// window's sum stays where it was, and both views handed a card in one pump are named.
    #[test]
    fn the_attention_hook_hears_each_views_change_and_names_every_view_handed_a_card() {
        use agent::AgentDomainEvent;
        use eitri_core::attention::Attention;
        let mut tabs = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
        let base = tabs.active();
        let moved = tabs.open(eitri_core::tab_set::AgentId::Claude);
        let two = ViewId::new(2).unwrap();
        assert_eq!(tabs.break_out(two), Ok(moved));
        let state = state_for_hooks(tabs);
        // Each call: whether the window's value changed, and the views handed a card.
        type Heard = Rc<RefCell<Vec<(bool, Vec<ViewId>)>>>;
        let heard: Heard = Rc::default();
        {
            let heard = heard.clone();
            state.borrow_mut().attention_hook = Some(Rc::new(
                move |before: Attention, after: Attention, handed: &[ViewId]| {
                    heard.borrow_mut().push((before != after, handed.to_vec()));
                },
            ));
        }
        let finished = AgentDomainEvent::TurnCompleted {
            turn_id: "t".into(),
            outcome: agent::TurnOutcome::Completed,
            result_text: String::new(),
            stop_reason: None,
            usage: None,
            detail: Default::default(),
        };
        let card = |id: &str| AgentDomainEvent::PermissionRequested {
            permission_id: id.into(),
            tool_use_id: None,
            tool_name: "Write".into(),
            input: serde_json::json!({}),
            provider_prompt: None,
        };
        let observe = |tab: TabId, event: &AgentDomainEvent| {
            let before = state.borrow().tabs.attention();
            state
                .borrow_mut()
                .tabs
                .get_mut(tab)
                .unwrap()
                .attention
                .observe(std::slice::from_ref(event), false);
            stepped(&state, |state, out| report_attention(state, before, out));
        };

        observe(base, &finished);
        assert_eq!(heard.borrow_mut().pop(), Some((true, vec![])));
        // The window was already unread: only agent 2's chip changes.
        observe(moved, &finished);
        assert_eq!(heard.borrow_mut().pop(), Some((false, vec![])));
        // Nothing changed: not told.
        observe(moved, &finished);
        assert_eq!(heard.borrow_mut().pop(), None);

        // agent 2 is handed a card first, the base view the newest, before one report.
        let before = state.borrow().tabs.attention();
        {
            let mut state = state.borrow_mut();
            state
                .tabs
                .get_mut(moved)
                .unwrap()
                .attention
                .observe(&[card("p1")], false);
            state
                .tabs
                .get_mut(base)
                .unwrap()
                .attention
                .observe(&[card("p2")], false);
        }
        stepped(&state, |state, out| report_attention(state, before, out));
        assert_eq!(heard.borrow_mut().pop(), Some((true, vec![two, ViewId::BASE])));
    }

    /// The trust worker builds the prompt beside its check and tells nobody when it is done: the prompt
    /// reaches the page only through the tick that polls the gate. Held on the worker, the tick keeps its
    /// steady cadence; let go, the prompt is sent and the tick waits for the answer.
    #[test]
    fn a_prompt_built_on_the_trust_worker_is_polled_for_until_it_reaches_the_page() {
        use crate::panel_wake::NextTick;
        let (handle, page) = settled_panel();
        let outer = agent::state_dirs::test_workspace_dir("panel-trust-polled");
        let home = outer.join("home");
        let state_home = outer.join("state");
        let root = home.join("p");
        std::fs::create_dir_all(root.join(".claude")).unwrap();
        std::fs::create_dir_all(&state_home).unwrap();
        std::fs::write(root.join(PROJECT_SETTINGS), session_start_hook("touch /tmp/never")).unwrap();
        let hold = crate::trust_gate::WorkerHold::closed();
        {
            let mut s = handle.state.borrow_mut();
            s.project_dir = root.clone();
            s.canonical_project_dir = root.to_string_lossy().into_owned();
            let store =
                eitri_core::project_trust::TrustStore::new(Some(state_home.as_os_str()), Some(home.as_os_str()));
            s.trust = TrustGate::new_held(store, root.clone(), Some(home.clone()), hold.clone());
            let tab = s.tabs.active();
            gate_start(&mut s, tab, first_send("r1", "hello")).unwrap();
        }
        let sent = page.sent.borrow().len();
        let prompt_sent = || page.sent.borrow()[sent..].iter().any(|p| kind_of(p) == "trust_prompt");
        let mut at = std::time::Instant::now();
        for _ in 0..5 {
            assert_eq!(
                handle.run_tick(at),
                NextTick::At(at + AgentPanelHandle::TICK),
                "the worker owes an answer: polled"
            );
            at += AgentPanelHandle::TICK;
        }
        assert!(!prompt_sent());
        hold.open();
        tick_until_quiet(&handle, at);
        assert!(
            prompt_sent(),
            "the prompt reached the page: {:?}",
            &page.sent.borrow()[sent..]
        );
    }
}
