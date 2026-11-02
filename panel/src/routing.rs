//! What goes to which page: the tab strips, the switch and view snapshots, `hello`, the editor context and
//! the window envelopes a `ready` is sent, and the held prompts, which keep the page they were typed in.

use super::*;

/// The `tabs` envelope for `view`'s page (`TabSet::tabs_payload_for`: the base page lists the base
/// view's tabs, an extra page its one tab), recorded as the last one sent to that page.
pub(super) fn tabs_payload_recorded(state: &mut AgentPanelState, view: ViewId) -> Strip {
    let payload = state.tabs.tabs_payload_for(view);
    if let Some(record) = state.pages.get_mut(&view) {
        record.last_tabs_payload = Some(payload.clone());
    }
    numbered_strip(state, payload)
}

/// `view`'s `tabs` envelope if it differs from the last one recorded for that page and the page is ready
/// for it, recorded as sent ([`changed_envelope`]).
pub(super) fn tabs_payload_if_changed(state: &mut AgentPanelState, view: ViewId) -> Option<Strip> {
    let now = state.tabs.tabs_payload_for(view);
    let ready = state.ready(view);
    let payload = state
        .pages
        .get_mut(&view)
        .and_then(|record| changed_envelope(&mut record.last_tabs_payload, now, ready))?;
    Some(numbered_strip(state, payload))
}

/// `payload`, a `tabs` envelope just recorded, with the next number.
pub(super) fn numbered_strip(state: &mut AgentPanelState, payload: String) -> Strip {
    state.tabs_recorded += 1;
    Strip {
        seq: state.tabs_recorded,
        payload,
    }
}

/// Whether the `tabs` envelope numbered `seq` may still go to `view`'s page, and if so, the page's latest.
/// One recorded before a strip the page already has is stale: a window hook that ran while a step's
/// outbox was being delivered can switch tabs and send its own, later strip ahead of the step's.
pub(super) fn strip_still_current(state: &mut AgentPanelState, view: ViewId, seq: u64) -> bool {
    let Some(record) = state.pages.get_mut(&view) else {
        return true;
    };
    if seq < record.tabs_delivered {
        return false;
    }
    record.tabs_delivered = seq;
    true
}

/// The views of the live chat pages, base first: one record per live page (`AgentPanelState::pages`).
pub(super) fn live_views(state: &AgentPanelState) -> Vec<ViewId> {
    state.pages.keys().copied().collect()
}

/// Sends `tabs` now: the set changed in a way the pages must see at once. The base page is always
/// sent its strip; an extra page only when its own changed and it is ready.
pub(super) fn send_tabs(state: &mut AgentPanelState, out: &mut Outbox) {
    let payload = tabs_payload_recorded(state, ViewId::BASE);
    out.send(ViewId::BASE, payload);
    send_extra_tabs_if_changed(state, out);
    send_mode_note(state, out);
}

/// Each live extra page's `tabs`, when its own changed since the last one it was sent.
pub(super) fn send_extra_tabs_if_changed(state: &mut AgentPanelState, out: &mut Outbox) {
    for view in live_views(state) {
        if view.is_base() {
            continue;
        }
        if let Some(strip) = tabs_payload_if_changed(state, view) {
            out.send(view, strip);
        }
    }
}

/// What the tab set noted about a tab that could not take the window's mode (an agent with no
/// manual mode started in auto), said once in the base page's transient slot, after the `tabs` that
/// shows the tab. Kept until the base page is ready to show it.
pub(super) fn send_mode_note(state: &mut AgentPanelState, out: &mut Outbox) {
    if !state.ready(ViewId::BASE) {
        return;
    }
    if let Some(note) = state.tabs.take_mode_note() {
        eprintln!("[permission] {note}");
        out.send(ViewId::BASE, eitri_core::agent_bridge::serialize_notice_for_js(&note));
    }
}

/// The tick's `tabs` envelopes, one per live page: each sent only when it differs from the last one
/// that page was sent (a marker, a label, a state -- whatever `TabSet::tabs_payload_for` says).
///
/// Held back while a page's document has not said `ready`, and then not recorded as sent, so it
/// goes out once it has.
pub(super) fn send_tabs_if_changed(state: &mut AgentPanelState, out: &mut Outbox) {
    let mut base_sent = false;
    for view in live_views(state) {
        if let Some(strip) = tabs_payload_if_changed(state, view) {
            out.send(view, strip);
            base_sent |= view.is_base();
        }
    }
    if base_sent {
        send_mode_note(state, out);
    }
}

/// V1: the editor context's summary, to each live page whose last one differs.
pub(super) fn send_context_if_changed(state: &mut AgentPanelState, out: &mut Outbox) {
    let context = (state.editor_context)();
    super::edit_selection::poll(state, context.as_ref(), out);
    let views = live_views(state);
    if views.is_empty() {
        return;
    }
    let summary = eitri_core::agent_bridge::context_summary(context.as_ref(), &state.project_dir);
    let now = eitri_core::agent_bridge::serialize_editor_context_for_js(summary.as_ref());
    for view in views {
        let ready = state.ready(view);
        let payload = state
            .pages
            .get_mut(&view)
            .and_then(|record| changed_envelope(&mut record.last_context_payload, now.clone(), ready));
        if let Some(payload) = payload {
            out.send(view, payload);
        }
    }
}

/// Appends `texts` to the window's copy of the project's history at once, hands them to the history's
/// writer (the file is written off this thread), and tells every ready page (the history is the
/// window's). Best-effort: a failed write is logged by the writer and the pages keep this window's copy.
pub(super) fn remember_prompts(state: &mut AgentPanelState, texts: &[String], out: &mut Outbox) {
    if !state.history_writer.is_enabled() {
        return;
    }
    state.history = eitri_core::prompt_history::appended(&state.history, texts);
    state.history_writer.submit(texts);
    send_history(state, out);
}

/// What the history's writer read back after its newest write -- other windows' prompts among them --
/// in place of the window's copy, told to every ready page when it differs.
pub(super) fn collect_history(state: &mut AgentPanelState, out: &mut Outbox) {
    let Some(entries) = state.history_writer.poll() else {
        return;
    };
    if entries != state.history {
        state.history = entries;
        send_history(state, out);
    }
}

fn send_history(state: &AgentPanelState, out: &mut Outbox) {
    let payload = eitri_core::agent_bridge::serialize_history_for_js(&state.history);
    for view in live_views(state) {
        if state.ready(view) {
            out.send(view, payload.clone());
        }
    }
}

/// A tab's queue and the line under it, so a step that may change them can tell whether the page is owed
/// the queue again. `None` for a tab that is gone.
pub(super) fn queue_shown(
    state: &AgentPanelState,
    tab: TabId,
) -> Option<(Vec<eitri_core::tab_set::Queued>, Option<String>)> {
    state.tabs.get(tab).map(|t| (t.session.queue.clone(), t.queue_line()))
}

/// The queue of a tab on screen, after anything changed it, to the ready page that shows it.
pub(super) fn send_queue(state: &AgentPanelState, tab: TabId, out: &mut Outbox) {
    let Some(view) = state.page_of(tab) else { return };
    let t = state.tabs.get(tab).expect("a shown tab exists");
    out.send(
        view,
        eitri_core::agent_bridge::serialize_queue_for_js(
            tab,
            &t.session.queue,
            t.session.queue_error.as_deref(),
            t.queue_line().as_deref(),
            t.steers(),
        ),
    );
    out.send(
        view,
        eitri_core::agent_bridge::serialize_shell_running_for_js(tab, state.tabs.shell_line_runs(tab)),
    );
}

/// A flush's outcome: the same benign/fatal split a command gets, with no request to
/// answer. A refusal leaves the queue and its `error` line; the queue envelope says so.
pub(super) fn apply_flush(
    state: &mut AgentPanelState,
    screen: &Screen,
    tab: TabId,
    flush: eitri_core::tab_set::Flush,
    out: &mut Outbox,
) {
    if flush.outcome.is_ok() {
        title_from_first_prompt(state, tab, &flush.typed);
    }
    apply_outcome(state, screen, tab, None, flush.outcome, out);
    send_queue(state, tab, out);
}

/// Whether a move or a reload in `view`'s page ends the `:trust` prompt: it was typed in that page,
/// or its tab has left the view of the page it was typed in, or that page has closed, either of
/// which can no longer answer it (or no command is recorded, which leaves the gate's own record to
/// decide).
pub(super) fn command_prompt_is_in(state: &AgentPanelState, view: ViewId) -> bool {
    let Some(command) = state.trust.command_prompt().and_then(|id| state.owed.get(id)) else {
        return true;
    };
    let Owed::TrustCommand { tab, typed_in } = command.what else {
        return true;
    };
    command.page.is_none() || typed_in == view || state.tabs.get(tab).is_none_or(|t| t.view_id() != typed_in)
}

/// The base page's `tabs`, then the active tab's own state (always, not only when stale), and the
/// trust question again when the tab on screen waits for one: routing away took the old one off. A
/// `:trust` question typed in the base page goes with any move there, answered as not trusted, and so
/// does one whose tab has left the view it was typed in.
///
/// Asking again can put the tab's start off instead (a question too large to show); what that owes
/// goes into `out` whole -- its toast, and an answer to the page that asked for the start, which need
/// not be the base page -- after the base page's own payloads.
pub(super) fn switch_payloads(state: &mut AgentPanelState, out: &mut Outbox) {
    if command_prompt_is_in(state, ViewId::BASE) {
        state.trust.drop_command_prompt();
    }
    let strip = tabs_payload_recorded(state, ViewId::BASE);
    out.send(ViewId::BASE, strip);
    out.send_all(ViewId::BASE, state.tabs.active_state_payloads());
    let active = state.tabs.active();
    if state.trust.current_route(&waiting_profile(state, active)) == Some(Route::Ask) {
        out.extend(reroute_or_ask(state, active));
    }
}

/// What `view`'s page is owed when the tab it shows arrived or changed state, put in `out`: for the
/// base view, [`switch_payloads`]; for an extra view, its `tabs`, its tab's own state and its tab's
/// trust question asked again (or, as there, all a start put off instead owes). An extra view with no
/// tab gets only its (empty) `tabs`.
pub(super) fn view_payloads(state: &mut AgentPanelState, view: ViewId, out: &mut Outbox) {
    if view.is_base() {
        return switch_payloads(state, out);
    }
    let strip = tabs_payload_recorded(state, view);
    out.send(view, strip);
    let Some(tab) = state.tabs.tab_in(view) else {
        return;
    };
    out.send_all(view, state.tabs.state_payloads(tab));
    if state.trust.current_route(&waiting_profile(state, tab)) == Some(Route::Ask) {
        out.extend(reroute_or_ask(state, tab));
    }
}

/// After the base view's active tab changed: `mark_seen` if the base page is on screen, then its
/// `tabs` and the new active tab's state.
pub(super) fn send_switch(state: &mut AgentPanelState, screen: &Screen, out: &mut Outbox) {
    send_view(state, screen, ViewId::BASE, out);
}

/// [`send_switch`] for any view's page: `mark_seen` of the tab it shows if the page is on screen, then
/// [`view_payloads`]. An extra page is sent nothing until its document is ready: its `ready` sends
/// all of it.
pub(super) fn send_view(state: &mut AgentPanelState, screen: &Screen, view: ViewId, out: &mut Outbox) {
    if !view.is_base() && !state.ready(view) {
        return;
    }
    let before = state.tabs.attention();
    if screen.shows(view) {
        if let Some(tab) = state.tab_of_page(view) {
            state.tabs.mark_seen(tab);
        }
    }
    view_payloads(state, view, out);
    report_attention(state, before, out);
}

/// What a page shows for tab `tab` once it changed state, put in `out`: the whole view's when the tab
/// is on screen in some page ([`view_payloads`]), else only the `tabs` of its view, to that view's page.
pub(super) fn shown_payloads(state: &mut AgentPanelState, tab: TabId, out: &mut Outbox) {
    let view = state.tabs.view_of(tab);
    if state.tabs.is_shown(tab) && (view.is_base() || state.ready(view)) {
        view_payloads(state, view, out);
    } else {
        let strip = tabs_payload_recorded(state, view);
        out.send(view, strip);
    }
}

/// `hello` is per page and re-sent, recomputed, whenever what it describes changes -- a session
/// adopted, a resume installed, a tab closed or reset, the page's tab now running another agent.
pub(super) fn send_hello_if_open_sessions_changed(state: &mut AgentPanelState, out: &mut Outbox) {
    // A title the worker found since: the rows it names are out of date, in every page.
    let titles_changed = std::mem::take(&mut state.hello_titles_stale);
    for view in live_views(state) {
        // `ready` sends `hello` itself, and records what it was computed against.
        if !state.ready(view) {
            continue;
        }
        let open = state.tabs.open_sessions();
        // The saved tabs are on offer only in the base page, and only while nothing here has
        // started: when that stops being true the dashboard's line has to go, though no session id
        // changed with it.
        let offered = view.is_base() && restore_is_on_offer(state);
        // The greeting lists the page's tab's agent's sessions, so a switch to a tab running another
        // agent changes it too, though no session id changed with it.
        let agent = hello_agent(state, view);
        let record = state.pages.get(&view).expect("a ready page has its record");
        if open == record.last_open_ids
            && offered == record.last_restore_offered
            && agent == record.last_hello_agent
            && !titles_changed
        {
            continue;
        }
        let mut greeting = BackendGreeting::for_agent_titled(
            state.backend_kind,
            &agent,
            state.project_dir.clone(),
            eitri_core::agent_backend::Titles::Cached(&state.titles),
        );
        greeting
            .resumable
            .retain(|r| !eitri_core::tab_restore::is_open(&open, r));
        let offer = if view.is_base() { restore_offer(state) } else { None };
        let record = state.pages.get_mut(&view).expect("a ready page has its record");
        record.last_open_ids = open;
        record.last_restore_offered = offered;
        record.last_hello_agent = agent;
        out.send(
            view,
            serialize_hello_with_project_for_js(&greeting, offer.as_ref(), state.choose_project_hook.is_some()),
        );
    }
}

/// The agent whose records `view`'s page offers: the agent of the tab it shows, or the window's
/// default for an extra page that shows none yet.
pub(super) fn hello_agent(state: &AgentPanelState, view: ViewId) -> AgentId {
    state
        .tab_of_page(view)
        .and_then(|tab| state.tabs.get(tab))
        .map_or_else(|| state.default_agent.clone(), |t| t.agent().clone())
}

/// The view whose page a [`Dest`] names now.
pub(super) fn dest_view(state: &AgentPanelState, dest: Dest) -> ViewId {
    match dest {
        Dest::Page(view) => view,
        Dest::Tab(tab) => state.tabs.view_of(tab),
    }
}

/// What a turn the tab set let go did: a flushed queue, a first message (its `send_message` answered
/// in the page it came from, unless that page has closed), or one the presence could not mark as
/// running, which waits at the head of its queue.
pub(super) fn apply_sent(
    state: &mut AgentPanelState,
    screen: &Screen,
    sent: eitri_core::tab_set::Sent,
    out: &mut Outbox,
) {
    use eitri_core::tab_set::Sent;
    match sent {
        Sent::Flushed { tab, flush } => apply_flush(state, screen, tab, flush, out),
        Sent::First {
            tab,
            request_id,
            typed,
            outcome,
        } => {
            if outcome.is_ok() {
                title_from_first_prompt(state, tab, &typed);
            }
            // A first message that was a `!` line is running now, and the page has to know.
            if state.tabs.shell_line_runs(tab) {
                send_queue(state, tab, out);
            }
            let view = start_answer_dest(state, tab, &request_id).map(|sent_from| dest_view(state, sent_from));
            apply_outcome(
                state,
                screen,
                tab,
                view.map(|view| (view, request_id.as_str())),
                outcome,
                out,
            );
        }
        Sent::Prompt {
            tab,
            page,
            request_id,
            typed,
            outcome,
        } => {
            let page = held_prompt_page(state, tab, &request_id, page);
            prompt_sent(
                state,
                screen,
                tab,
                page.map(|page| (page, request_id.as_str())),
                &typed,
                outcome,
                out,
            );
        }
        Sent::NotSent {
            tab,
            note,
            request_id,
            page,
        } => {
            if let Some(request_id) = request_id {
                let view = match page {
                    Some(page) => held_prompt_page(state, tab, &request_id, page),
                    None => start_answer_dest(state, tab, &request_id).map(|sent_from| dest_view(state, sent_from)),
                };
                let refused = Err(eitri_core::agent_backend::BackendError {
                    message: note,
                    benign: true,
                });
                apply_outcome(
                    state,
                    screen,
                    tab,
                    view.map(|view| (view, request_id.as_str())),
                    refused,
                    out,
                );
            }
            send_queue(state, tab, out);
        }
    }
}

/// A typed message's outcome: a sent one may title the tab, and the command's result goes to `request`'s
/// page (none when that page has gone).
pub(super) fn prompt_sent(
    state: &mut AgentPanelState,
    screen: &Screen,
    tab: TabId,
    request: Option<(ViewId, &str)>,
    typed: &str,
    outcome: Result<(), BackendError>,
    out: &mut Outbox,
) {
    if outcome.is_ok() {
        title_from_first_prompt(state, tab, typed);
    }
    apply_outcome(state, screen, tab, request, outcome, out);
    // A message the tab kept instead of sending (a card waits, or the agent was busy) is a line in the
    // queue the page draws.
    if state.tabs.get(tab).is_some_and(|t| !t.session.queue.is_empty()) {
        send_queue(state, tab, out);
    }
}

/// The page a held message's result goes to, as the ledger has it: its page, or `None` once that page
/// has gone (a page reusing its view numbers its requests afresh, so the old id would name one of its
/// own). The entry is spent. A message with no entry was never held, and is answered to `page`.
pub(super) fn held_prompt_page(
    state: &mut AgentPanelState,
    tab: TabId,
    request_id: &str,
    page: ViewId,
) -> Option<ViewId> {
    match state
        .owed
        .take_where(|answer| answer.what == Owed::HeldPrompt(tab) && answer.request_id == request_id)
    {
        Some(asked) => asked.page,
        None => Some(page),
    }
}

/// A tab with no title takes its first prompt's (`agent::persistence::title_from_prompt`, the rule
/// the sidecar's record uses too).
pub(super) fn title_from_first_prompt(state: &mut AgentPanelState, tab: TabId, typed: &str) {
    if let Some(t) = state.tabs.get_mut(tab) {
        if t.session.title.is_none() {
            t.session.title = agent::persistence::title_from_prompt(typed);
        }
    }
}

/// Everything a freshly-mounted document is owed, in the order it must arrive.
///
/// A pure function over plain data, for the same reason `SnapshotView` is one: the reload path has
/// no `WebView`-free test otherwise, and "what does a reloaded panel get back" is exactly the list
/// a later change loses something from without any test noticing.
///
/// `hello` first, then the theme and the keymap (colours before anything drawn with them), then the
/// window-level envelopes in `window` (the history, then the editor context), then the
/// `tabs` envelope, then the active tab's own state. The handoff card is now one of the active
/// tab's own payloads (`TabSet::active_state_payloads`), as its snapshot is.
// One parameter per thing the page is owed, in the order it receives them.
#[allow(clippy::too_many_arguments)]
pub(super) fn ready_payloads(
    mut greeting: BackendGreeting,
    open: &[(AgentId, String)],
    window: Vec<String>,
    tabs: String,
    active: Vec<String>,
    theme: Option<&str>,
    keymap: Option<&str>,
    restore: Option<&RestoreOffer>,
    can_choose_project: bool,
) -> Vec<String> {
    // No session open in any tab, or handed off from one, is offered for resume -- the
    // lease refuses a second driver, and a handed-off one has a CLI writing it.
    greeting
        .resumable
        .retain(|r| !eitri_core::tab_restore::is_open(open, r));
    let mut payloads = vec![serialize_hello_with_project_for_js(
        &greeting,
        restore,
        can_choose_project,
    )];
    payloads.extend(theme.map(str::to_string));
    payloads.extend(keymap.map(str::to_string));
    // Window-level state (the history, then the editor context), before the tabs that use it.
    payloads.extend(window);
    payloads.push(tabs);
    payloads.extend(active);
    payloads
}

/// The window-level envelopes of a fresh document, in order: the history, the editor context and,
/// only in a window that has an editor to follow, where the panel stands with it. The one-window
/// mode never sets a link, so its list is the two it always was.
pub(super) fn window_payloads(history: String, context: String, link: Option<&String>) -> Vec<String> {
    let mut payloads = vec![history, context];
    payloads.extend(link.cloned());
    payloads
}

/// A window-level envelope for the tick: `Some` when the page can take it and it changed since the
/// last one sent. Recorded as sent only when it is returned.
pub(super) fn changed_envelope(last: &mut Option<String>, now: String, document_ready: bool) -> Option<String> {
    if !document_ready || last.as_deref() == Some(now.as_str()) {
        return None;
    }
    *last = Some(now.clone());
    Some(now)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_panel::tests::*;

    #[test]
    fn hello_says_can_choose_project_when_a_hook_is_installed() {
        for installed in [false, true] {
            let (handle, page) = handle_with_strict_page();
            page.inner.visible.set(true);
            if installed {
                handle.on_choose_project(|| {});
            }
            handle.page_message(ViewId::BASE, r#"{"type":"ready","request_id":"r1"}"#);
            let sent = page.inner.sent.borrow();
            let hello = sent.iter().find(|payload| kind_of(payload) == "hello").unwrap();
            let hello: serde_json::Value = serde_json::from_str(hello).unwrap();
            assert_eq!(hello["canChooseProject"], installed);
        }
    }

    #[test]
    fn ready_gets_its_envelopes_in_their_fixed_order() {
        let (handle, page) = handle_with_strict_page();
        page.inner.visible.set(true);
        handle.page_message(ViewId::BASE, r#"{"type":"ready","request_id":"r1"}"#);
        let kinds: Vec<String> = page.inner.sent.borrow().iter().map(|e| kind_of(e)).collect();
        let at = |k: &str| {
            kinds
                .iter()
                .position(|x| x == k)
                .unwrap_or_else(|| panic!("no {k} in {kinds:?}"))
        };
        assert_eq!(kinds[0], "hello");
        assert!(
            at("history") < at("tabs") && at("editor_context") < at("tabs"),
            "{kinds:?}"
        );
        assert!(at("display") < at("tabs"), "{kinds:?}");
        assert!(kinds.iter().any(|k| k == "command_result"), "{kinds:?}");
    }

    const ROOT_NOTE: &str =
        "No project boundary here (it is open to others): a Claude tab asks before every file and command.";

    fn root_note_texts(sent: &[String]) -> Vec<String> {
        sent.iter()
            .filter(|e| kind_of(e) == "root_note")
            .map(|e| {
                serde_json::from_str::<serde_json::Value>(e).unwrap()["text"]
                    .as_str()
                    .unwrap()
                    .to_owned()
            })
            .collect()
    }

    #[test]
    fn each_page_gets_the_root_note_after_its_hello() {
        let (handle, base, _host) = handle_with_recording_page();
        let second = attach_recording_page(&handle, view(2));
        handle.state.borrow_mut().root_note = Some(ROOT_NOTE.to_owned());
        handle.page_message(ViewId::BASE, READY);
        handle.page_message(view(2), READY);
        for page in [&base, &second] {
            let sent = page.sent.borrow();
            let kinds: Vec<String> = sent.iter().map(|e| kind_of(e)).collect();
            assert_eq!(&kinds[..2], ["hello", "root_note"], "{kinds:?}");
            assert_eq!(root_note_texts(&sent), [ROOT_NOTE]);
        }
    }

    #[test]
    fn no_root_note_no_message() {
        let (handle, page) = handle_with_strict_page();
        page.inner.visible.set(true);
        handle.page_message(ViewId::BASE, READY);
        assert!(root_note_texts(&page.inner.sent.borrow()).is_empty());
    }

    #[test]
    fn a_reloaded_page_gets_the_root_note_again() {
        let (handle, page) = handle_with_strict_page();
        page.inner.visible.set(true);
        handle.state.borrow_mut().root_note = Some(ROOT_NOTE.to_owned());
        handle.page_message(ViewId::BASE, READY);
        handle.page_message(ViewId::BASE, READY);
        assert_eq!(root_note_texts(&page.inner.sent.borrow()), [ROOT_NOTE, ROOT_NOTE]);
    }

    /// `agent.thinking` reaches the page: at once to a page already listening, and again on every
    /// `ready`, so a reloaded document shows thinking the way the first one did.
    #[test]
    fn the_thinking_level_is_told_at_once_and_again_on_every_ready() {
        use eitri_core::agent_prefs::ThinkingDisplay;
        let (handle, page) = handle_with_strict_page();
        page.inner.visible.set(true);
        handle.page_message(ViewId::BASE, r#"{"type":"ready","request_id":"r1"}"#);
        let displays = |page: &std::cell::RefCell<Vec<String>>| -> Vec<String> {
            page.borrow()
                .iter()
                .filter(|e| kind_of(e) == "display")
                .map(|e| {
                    serde_json::from_str::<serde_json::Value>(e).unwrap()["thinking"]
                        .as_str()
                        .unwrap()
                        .to_string()
                })
                .collect()
        };
        assert_eq!(displays(&page.inner.sent), vec!["collapsed"]);
        handle.set_thinking_display(ThinkingDisplay::Hidden);
        assert_eq!(displays(&page.inner.sent), vec!["collapsed", "hidden"]);
        handle.page_message(ViewId::BASE, r#"{"type":"ready","request_id":"r2"}"#);
        assert_eq!(displays(&page.inner.sent), vec!["collapsed", "hidden", "hidden"]);
    }

    /// The first tick dispatched `tabs` into a page that had not
    /// loaded. A window-level envelope goes out only once the document said `ready`, and one held
    /// back is not recorded as sent, so it still goes out once it is.
    #[test]
    fn a_window_envelope_waits_for_the_document_and_goes_out_once_per_change() {
        let mut last = None;
        assert_eq!(changed_envelope(&mut last, "tabs-1".into(), false), None);
        assert_eq!(last, None, "not recorded while the page cannot take it");
        assert_eq!(
            changed_envelope(&mut last, "tabs-1".into(), true),
            Some("tabs-1".into())
        );
        assert_eq!(changed_envelope(&mut last, "tabs-1".into(), true), None, "unchanged");
        assert_eq!(
            changed_envelope(&mut last, "tabs-2".into(), true),
            Some("tabs-2".into())
        );
    }

    #[test]
    fn ready_sends_the_history_and_the_editor_context_before_the_tabs() {
        let theme = eitri_core::agent_bridge::serialize_theme_for_js(&eitri_core::theme::ThemeTokens::fallback());
        let keymap = eitri_core::agent_bridge::serialize_keymap_for_js(
            "Ctrl+b",
            &[],
            &[],
            &eitri_core::keymap::panel::effective(&Default::default(), None).0,
            "Ctrl+b c",
            &[],
        );
        let window = vec![
            eitri_core::agent_bridge::serialize_history_for_js(&["earlier".into()]),
            eitri_core::agent_bridge::serialize_editor_context_for_js(None),
        ];
        let payloads = ready_payloads(
            legacy_greeting(),
            &[],
            window,
            r#"{"kind":"tabs","active":1,"tabs":[]}"#.to_string(),
            vec![],
            Some(&theme),
            Some(&keymap),
            None,
            false,
        );
        assert_eq!(
            kinds(&payloads),
            vec!["hello", "theme", "keymap", "history", "editor_context", "tabs"]
        );
    }

    #[test]
    fn ready_sends_the_editor_link_only_when_one_was_set() {
        let history = || eitri_core::agent_bridge::serialize_history_for_js(&[]);
        let context = || eitri_core::agent_bridge::serialize_editor_context_for_js(None);
        let none = window_payloads(history(), context(), None);
        assert_eq!(
            kinds(&none),
            vec!["history", "editor_context"],
            "the one-window list is unchanged"
        );
        let link = eitri_core::agent_bridge::serialize_editor_link_for_js("none", "no editor attached");
        let some = window_payloads(history(), context(), Some(&link));
        assert_eq!(kinds(&some), vec!["history", "editor_context", "editor_link"]);
    }

    #[test]
    fn ready_sends_hello_theme_keymap_tabs_then_the_active_tabs_state() {
        let theme = eitri_core::agent_bridge::serialize_theme_for_js(&eitri_core::theme::ThemeTokens::fallback());
        let keymap = eitri_core::agent_bridge::serialize_keymap_for_js(
            "Ctrl+b",
            &[],
            &[],
            &eitri_core::keymap::panel::effective(&Default::default(), None).0,
            "Ctrl+b c",
            &[],
        );
        let snapshot = r#"{"kind":"snapshot","tab":1,"throughRevision":3,"state":{}}"#.to_string();
        let payloads = ready_payloads(
            legacy_greeting(),
            &[],
            vec![],
            r#"{"kind":"tabs","active":1,"tabs":[]}"#.to_string(),
            vec![snapshot],
            Some(&theme),
            Some(&keymap),
            None,
            false,
        );
        assert_eq!(kinds(&payloads), vec!["hello", "theme", "keymap", "tabs", "snapshot"]);
    }

    /// `hello` offers no session that is open in a tab, and still
    /// drops a session a tab handed off (the older rule, now per tab).
    #[test]
    fn hello_offers_no_session_that_is_open_in_a_tab() {
        let mut greeting = legacy_greeting();
        greeting.resumable = ["open-here", "free"]
            .iter()
            .map(|id| agent::ResumableSession {
                provider: "claude".into(),
                provider_session_id: id.to_string(),
                created_at: "1".into(),
                updated_at: "2".into(),
                title: None,
                name: None,
            })
            .collect();
        let payloads = ready_payloads(
            greeting,
            &[(AgentId::Claude, "open-here".to_string())],
            vec![],
            "{}".to_string(),
            vec![],
            None,
            None,
            None,
            false,
        );
        let hello: serde_json::Value = serde_json::from_str(&payloads[0]).unwrap();
        let offered: Vec<&str> = hello["resumableSessions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["providerSessionId"].as_str().unwrap())
            .collect();
        assert_eq!(offered, vec!["free"]);
    }

    fn legacy_greeting() -> BackendGreeting {
        BackendGreeting::for_kind(BackendKind::Legacy, PathBuf::from("/home/user/project"))
    }

    /// The whole point of holding the command in Rust. A document that reloads -- Ctrl+Shift+R, the
    /// top bar's `⟳`, or a WebKit crash -- gets the command back, because the panel still has it.
    ///
    /// The backend here is `legacy` deliberately: it is the default, and it is the case where the
    /// loss was unrecoverable. The first assert states why, from the greeting's own payload -- legacy
    /// offers no resume, so `hello` carries no copy of this id and nothing else in the process does
    /// either.
    #[test]
    fn a_reloaded_document_is_handed_back_the_command_the_old_one_was_showing() {
        let command = a_command();
        let payloads = ready_payloads(
            legacy_greeting(),
            &[],
            vec![],
            "{\"kind\":\"tabs\"}".to_string(),
            vec![eitri_core::agent_bridge::serialize_handoff_for_js(TabId(1), &command)],
            None,
            None,
            None,
            false,
        );
        let hello: serde_json::Value = serde_json::from_str(&payloads[0]).unwrap();
        assert!(
            hello["resumableSession"].is_null(),
            "legacy has no other record of this session"
        );

        assert_eq!(kinds(&payloads), vec!["hello", "tabs", "handoff"]);
        let handoff: serde_json::Value = serde_json::from_str(&payloads[2]).unwrap();
        assert_eq!(
            handoff["command"],
            "cd /home/user/project && claude --resume 1857dcd5-973b-46a2"
        );
        assert_eq!(handoff["providerSessionId"], "1857dcd5-973b-46a2");
    }

    /// The ordinary case: a panel that handed nothing off sends `hello` and the tab bar alone.
    #[test]
    fn a_panel_that_handed_nothing_off_sends_only_the_greeting_and_the_tabs() {
        assert_eq!(
            kinds(&ready_payloads(
                legacy_greeting(),
                &[],
                vec![],
                "{\"kind\":\"tabs\"}".to_string(),
                vec![],
                None,
                None,
                None,
                false,
            )),
            vec!["hello", "tabs"]
        );
    }

    /// The first
    /// `ready` draws the empty tab's dashboard, never the chooser -- `ready_payloads` is all a `ready` sends now.
    #[test]
    fn a_ready_sends_no_chooser() {
        assert!(!kinds(&ready_payloads(
            legacy_greeting(),
            &[],
            vec![],
            "{\"kind\":\"tabs\"}".to_string(),
            vec![],
            None,
            None,
            None,
            false,
        ))
        .contains(&"chooser".to_string()));
    }

    /// A reloaded document must get its colours back before anything it would draw with them.
    #[test]
    fn the_theme_follows_the_greeting_and_precedes_everything_else() {
        let theme = eitri_core::agent_bridge::serialize_theme_for_js(&eitri_core::theme::ThemeTokens::fallback());
        assert_eq!(
            kinds(&ready_payloads(
                legacy_greeting(),
                &[],
                vec![],
                "{\"kind\":\"tabs\"}".to_string(),
                vec![],
                Some(&theme),
                None,
                None,
                false,
            )),
            vec!["hello", "theme", "tabs"]
        );
        let payloads = ready_payloads(
            legacy_greeting(),
            &[],
            vec![],
            "{\"kind\":\"tabs\"}".to_string(),
            vec![r#"{"kind":"snapshot","tab":1,"throughRevision":3,"state":{}}"#.to_string()],
            Some(&theme),
            None,
            None,
            false,
        );
        assert_eq!(kinds(&payloads), vec!["hello", "theme", "tabs", "snapshot"]);
    }

    /// The `?` overlay's rows come with every `ready`, right after the colours, so a reloaded
    /// document never shows a keymap it was not told.
    #[test]
    fn the_keymap_follows_the_theme() {
        let theme = eitri_core::agent_bridge::serialize_theme_for_js(&eitri_core::theme::ThemeTokens::fallback());
        let keymap = eitri_core::agent_bridge::serialize_keymap_for_js(
            "Ctrl+b",
            &[],
            &[],
            &eitri_core::keymap::panel::effective(&Default::default(), None).0,
            "Ctrl+b c",
            &[],
        );
        assert_eq!(
            kinds(&ready_payloads(
                legacy_greeting(),
                &[],
                vec![],
                "{\"kind\":\"tabs\"}".to_string(),
                vec![],
                Some(&theme),
                Some(&keymap),
                None,
                false,
            )),
            vec!["hello", "theme", "keymap", "tabs"]
        );
    }

    /// A window hook run while a tick's outbox is delivered may switch the base view to another tab and
    /// send that page its own `tabs`. The tick's `tabs`, built before the hook ran, must not follow and
    /// switch the page back to a tab the panel no longer shows: the panel would then compare against the
    /// hook's strip and never send the page a correction.
    #[test]
    fn a_tab_switch_made_by_a_hook_during_delivery_is_not_undone_by_the_ticks_older_strip() {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("panel-hook-switch-keeps-its-strip");
        let (handle, page) = settled_panel();
        let (background, background_provider) = live_tab(&handle, &dir);
        let drafting = handle.state.borrow_mut().tabs.open(AgentId::Claude);
        assert_eq!(handle.state.borrow().tabs.active(), drafting);
        background_provider.queue(write_card("perm-b"));
        tick_handle_until(&handle, "the background card", || {
            pending_cards(&handle, background) == 1
        });
        // Back from a draft edit, the keys go to the oldest card, as the companion window and the Mac
        // host do: that switches the base view to the background tab while the tick is delivered.
        let hooked = handle.clone();
        handle.on_editor_done(move |_view| {
            hooked.focus_oldest_card(ViewId::BASE);
        });
        let mut scratch = eitri_core::scratch::ScratchDir::new().expect("a scratch directory");
        let (_request, edit) = scratch.prepare_edit("draft").unwrap();
        handle
            .state
            .borrow_mut()
            .tabs
            .begin_scratch_edit(drafting, edit.id)
            .unwrap();
        handle.state.borrow_mut().pending_edits.push(edit.clone());
        std::fs::write(&edit.body, "edited\n").unwrap();
        std::fs::write(&edit.done, "written").unwrap();
        // The same tick also owes the base page a changed strip, built while the drafting tab is active.
        assert!(handle.state.borrow_mut().tabs.rename(drafting, "renamed"));
        page.sent.borrow_mut().clear();
        handle.tick();

        assert_eq!(
            handle.state.borrow().tabs.active(),
            background,
            "the hook switched tabs"
        );
        let strips: Vec<serde_json::Value> = page
            .sent
            .borrow()
            .iter()
            .map(|p| serde_json::from_str::<serde_json::Value>(p).unwrap())
            .filter(|v| v["kind"] == "tabs")
            .collect();
        assert!(!strips.is_empty(), "the switch sent the page its tabs");
        assert_eq!(
            strips.last().unwrap()["active"].as_u64(),
            Some(background.0),
            "the page is left on the tab the panel shows: {strips:?}"
        );
        shut_down_every_tab(&handle);
    }
}
