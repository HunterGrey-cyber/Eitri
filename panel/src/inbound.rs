//! The page's messages: `handle_inbound_message`, one arm per message kind, and the helpers that answer a
//! card or record what a command came to.

use super::*;

/// The most of a task's output the panel's whole-output view holds. It is one read, the most the sidecar
/// gives at once; what lies before it is named, not shown.
const TASK_OUTPUT_VIEW_BYTES: u32 = 1024 * 1024;

pub(super) enum TaskCommandReply {
    Stop(agent::TaskReply<()>),
    Output(agent::TaskReply<agent::TaskOutput>),
    Models(agent::TaskReply<agent::ModelList>),
    Settings(agent::TaskReply<agent::AppliedSettings>),
}

pub(super) struct PendingTaskCommand {
    pub(super) view: ViewId,
    pub(super) tab: TabId,
    pub(super) request_id: String,
    pub(super) task_id: String,
    pub(super) reply: TaskCommandReply,
}

/// Task RPCs run on the provider runtime. Only ready replies are taken on this thread.
pub(super) fn collect_task_commands(state: &mut AgentPanelState, out: &mut Outbox) {
    state.task_commands.retain(|pending| {
        use std::sync::mpsc::TryRecvError;
        let result = match &pending.reply {
            TaskCommandReply::Stop(rx) => rx
                .try_recv()
                .map(|result| result.map(|()| serialize_command_result_for_js(&pending.request_id, Ok(())))),
            TaskCommandReply::Output(rx) => rx.try_recv().map(|result| {
                result.map(|output| {
                    eitri_core::agent_bridge::serialize_task_output_for_js(
                        &pending.request_id,
                        pending.tab,
                        &pending.task_id,
                        &output,
                    )
                })
            }),
            TaskCommandReply::Models(rx) => rx.try_recv().map(|result| {
                result.map(|list| {
                    eitri_core::agent_bridge::serialize_models_for_js(&pending.request_id, pending.tab, &list)
                })
            }),
            TaskCommandReply::Settings(rx) => rx.try_recv().map(|result| {
                result.map(|applied| {
                    eitri_core::agent_bridge::serialize_applied_for_js(&pending.request_id, pending.tab, &applied)
                })
            }),
        };
        let payload = match result {
            Err(TryRecvError::Empty) => return true,
            Err(TryRecvError::Disconnected) => {
                serialize_command_result_for_js(&pending.request_id, Err("task request ended before an answer arrived"))
            }
            Ok(Err(error)) => serialize_command_result_for_js(
                &pending.request_id,
                Err(&agent::providers::claude_sidecar::task_refusal(error)),
            ),
            Ok(Ok(payload)) => payload,
        };
        out.send(pending.view, payload);
        false
    });
}

/// A `permission_response`'s own fields, out of the message (see `answer_permission_response`).
pub(super) struct PermissionAnswer {
    pub(super) permission_id: String,
    pub(super) decision: eitri_core::agent_bridge::DecisionChoice,
    pub(super) reason: Option<String>,
    pub(super) remember: bool,
}

impl PermissionAnswer {
    pub(super) fn of(message: InboundMessage) -> Option<Self> {
        match message {
            InboundMessage::PermissionResponse {
                permission_id,
                decision,
                reason,
                remember,
                ..
            } => Some(PermissionAnswer {
                permission_id,
                decision,
                reason,
                remember,
            }),
            _ => None,
        }
    }
}

/// Everything a `permission_response` does, with no WebView, so its route is tested whole: the rule the card offered is saved first when the user chose "Always allow" (never taken
/// from the panel), then the answer goes through `TabSet::answer_card` -- the one route
/// that records a human approval, which is what lets the CLI's own prompt for the same call through
/// without a second card. `Err` is a refusal the panel shows (the card stays and `a`
/// still works); `Ok` is the answer's own outcome.
pub(super) fn answer_permission_response(
    tabs: &mut eitri_core::tab_set::TabSet,
    rules_dir: Option<&Path>,
    project_dir: &Path,
    tab: TabId,
    page: ViewId,
    answer: PermissionAnswer,
) -> Result<Result<(), BackendError>, String> {
    let remembered = {
        let t = tabs.get(tab).ok_or_else(|| format!("protocol: no tab {}", tab.0))?;
        // Read right before the save, so an id that is no longer
        // pending -- or that this tab's user already answered, which the sidecar's projection still
        // lists until its `PermissionResolved` arrives -- saves nothing (`Tab::card_is_waiting`).
        let still_pending = t.card_is_waiting(&answer.permission_id);
        rule_to_remember(
            t,
            &answer.permission_id,
            answer.decision,
            answer.remember,
            still_pending,
        )
        .map_err(str::to_string)?
    };
    if let Some(rule) = remembered {
        let Some(dir) = rules_dir else {
            return Err("no state directory: the rule cannot be saved".to_string());
        };
        match eitri_core::permission_store::add(dir, project_dir, &rule) {
            Ok(rules) => {
                eprintln!(
                    "[permission] a rule saved ({})",
                    eitri_core::permission_store::path(dir, project_dir).display()
                );
                tabs.set_rules(rules);
            }
            // Not answered: the card stays and `a` still works.
            Err(e) => return Err(format!("could not save the rule: {e}")),
        }
    }
    let decision = answer.decision.into_decision(answer.reason);
    Ok(tabs.answer_card(tab, page, &answer.permission_id, decision))
}

/// The `remember` choice: the rule Rust offered for `permission_id`, only with an allow, and
/// only while that id is still a card waiting for the user.
/// `still_pending` is the caller's own read (`Tab::card_is_waiting`), taken right before this runs
/// -- passed in rather than read here so this stays testable without a live backend, the same
/// reason `changed_envelope` takes `document_ready` as a parameter.
///
/// The offer itself is gone once the first answer to that id succeeds, whatever the decision, and
/// never comes back (`TabSet::answer_card`, the tab's `user_answered`): on the sidecar path the
/// projection still lists the id as pending until the provider's real `PermissionResolved`
/// arrives, and `refresh_offers` recomputes offers from that projection whenever another id
/// arrives, so a one-time removal alone was undone by the next pump.
pub(super) fn rule_to_remember(
    tab: &Tab,
    permission_id: &str,
    decision: eitri_core::agent_bridge::DecisionChoice,
    remember: bool,
    still_pending: bool,
) -> Result<Option<agent::PrefixRule>, &'static str> {
    if !remember {
        return Ok(None);
    }
    if decision != eitri_core::agent_bridge::DecisionChoice::Allow {
        return Err("only an allow can be remembered");
    }
    if !still_pending {
        return Err("this request was already answered");
    }
    tab.session
        .rule_offers
        .get(permission_id)
        .cloned()
        .map(Some)
        .ok_or("no rule can be offered for this request")
}

/// Milliseconds since the epoch, for a queued item's `queuedAt`; 0 on a clock before 1970.
pub(super) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Applies one command's outcome to its tab and the frontend, uniformly.
///
/// The benign/fatal split is the whole point: a benign failure (a turn sent while one was running,
/// a permission answered twice) reports itself and leaves the session alone, while a fatal one
/// retires the tab's backend and marks the tab `Failed`. The `error`
/// envelope goes out only if `tab` is on screen, to the page that shows it -- each page draws only its
/// own tab, and a background tab catches up with a snapshot when it is switched to. The
/// `command_result` is owed either way, to `view`'s page, which sent the command.
/// An answer to one of Claude's questions on its way, and the page request its reply answers.
pub(super) struct PendingQuestionSend {
    tab: TabId,
    view: ViewId,
    request_id: String,
    send: eitri_core::agent_backend::QuestionSend,
}

/// A `question_answer` or `question_decline` taken by the tab's session: refused at once, the page is
/// told now; sent, it is told by the tick once the agent replied.
fn question_sent(
    state: &mut AgentPanelState,
    screen: &Screen,
    view: ViewId,
    tab: TabId,
    request_id: &str,
    sent: Result<eitri_core::agent_backend::QuestionSend, BackendError>,
    out: &mut Outbox,
) {
    match sent {
        Ok(send) => state.question_sends.push(PendingQuestionSend {
            tab,
            view,
            request_id: request_id.to_string(),
            send,
        }),
        Err(error) => apply_command_outcome(state, screen, view, tab, request_id, Err(error), out),
    }
}

/// Tells each page whose answer the agent replied to: a `command_result`, a refusal with the question
/// waiting again. Every refusal or failure of an answer is benign, so none ends a session here. The
/// handle is dropped once read, which lets the chat move again.
pub(super) fn poll_question_sends(state: &mut AgentPanelState, out: &mut Outbox) {
    if state.question_sends.is_empty() {
        return;
    }
    let AgentPanelState {
        question_sends, tabs, ..
    } = state;
    question_sends.retain(|pending| match tabs.finish_question_send(pending.tab, &pending.send) {
        None => true,
        Some(reply) => {
            if let Err(error) = &reply {
                eprintln!(
                    "[agent_panel] tab {}: answer not taken (session stays alive): {}",
                    pending.tab.0, error.message
                );
            }
            let told = match &reply {
                Ok(()) => serialize_command_result_for_js(&pending.request_id, Ok(())),
                Err(error) => serialize_command_result_for_js(&pending.request_id, Err(&error.message)),
            };
            out.send(pending.view, told);
            false
        }
    });
}

pub(super) fn apply_command_outcome(
    state: &mut AgentPanelState,
    screen: &Screen,
    view: ViewId,
    tab: TabId,
    request_id: &str,
    outcome: Result<(), BackendError>,
    out: &mut Outbox,
) {
    apply_outcome(state, screen, tab, Some((view, request_id)), outcome, out);
}

/// [`apply_command_outcome`] for an outcome no request is waiting on (a queue flush):
/// `request` is `None` and no `command_result` goes out.
pub(super) fn apply_outcome(
    state: &mut AgentPanelState,
    screen: &Screen,
    tab: TabId,
    request: Option<(ViewId, &str)>,
    outcome: Result<(), BackendError>,
    out: &mut Outbox,
) {
    let shown_in = state.shown_in(tab);
    match outcome {
        Ok(_) => {
            if let Some((view, id)) = request {
                out.send(view, serialize_command_result_for_js(id, Ok(())));
            }
        }
        Err(error) if error.benign => {
            eprintln!(
                "[agent_panel] tab {}: command rejected (session stays alive): {}",
                tab.0, error.message
            );
            if let Some((view, id)) = request {
                out.send(view, serialize_command_result_for_js(id, Err(&error.message)));
            }
        }
        Err(error) => {
            eprintln!("[agent_panel] tab {}: command failed fatally: {}", tab.0, error.message);
            // Retired on a worker thread, for the same reason construction runs on one. Two separate
            // problems with doing it here: `AgentConversation` has no `Drop`, so a bare drop never
            // sends `close_session` at all and the sidecar is left holding a session it thinks is
            // live; and the drop chain that DOES run (SpawnedSidecar polls for up to 3s before
            // escalating to SIGKILL, then RuntimeThread joins its Tokio thread) would block the
            // whole shell -- editor pane included -- on the GTK main loop.
            let attention_before = state.tabs.attention();
            let shown = shown_in.is_some_and(|view| screen.shows(view));
            let queue_before = queue_shown(state, tab);
            // Its cards end with it, and a message it never read is held again for `↑` to take back.
            let dead = state
                .tabs
                .fail_session(tab, error.message.clone(), FailedHow::AfterStart, shown);
            if let Some(retired) = dead {
                state.retiring.backend(retired);
            }
            if let Some((view, id)) = request {
                out.send(view, serialize_command_result_for_js(id, Err(&error.message)));
            }
            if let Some(view) = shown_in {
                out.send(view, serialize_error_for_js(tab, &error.message));
            }
            if queue_shown(state, tab) != queue_before {
                send_queue(state, tab, out);
            }
            report_attention(state, attention_before, out);
        }
    }
}

/// Stops the agent's work in `tab`, and nothing else: a `!` line running there is left alone. Refused while the tab
/// is frozen for a move: its turn moves as it runs, and stopped here the record would not say so.
pub(super) fn interrupt_agent(state: &mut AgentPanelState, tab: TabId) -> Result<(), BackendError> {
    if state.tabs.is_moving(tab) {
        return Err(BackendError {
            message: eitri_core::tab_set::MOVING.to_string(),
            benign: true,
        });
    }
    backend_for(&mut state.tabs, tab).and_then(AgentBackend::interrupt)
}

/// What a reset of `tab` does once it is allowed: the tab goes back to its start screen, or a session its agent stopped
/// with the conversation whole is continued in place. Answers `request_id` in `view`.
pub(super) fn reset_tab(state: &mut AgentPanelState, tab: TabId, view: ViewId, request_id: &str, out: &mut Outbox) {
    let ok = |out: &mut Outbox| out.send(view, serialize_command_result_for_js(request_id, Ok(())));
    let refuse = |out: &mut Outbox, why: &str| out.send(view, serialize_command_result_for_js(request_id, Err(why)));
    let before = state.tabs.attention();
    // A start put off for the trust question owes its command an answer; read before the
    // reset drops it.
    let owed = state.tabs.get(tab).and_then(|t| match t.backend() {
        TabBackend::AwaitingTrust(waiting) => waiting.start.request_id().map(str::to_owned),
        _ => None,
    });
    // A session its agent stopped with the conversation whole is continued in the same tab, not
    // left for a second `r` on the start screen.
    let continued = state.tabs.resumes_on_r(tab);
    match state.tabs.reset(tab) {
        Ok(old) => {
            // A wait an earlier `r` left for this tab is over: its start did not happen.
            state.resume_after.remove(&tab);
            if let Some(old) = old {
                if continued.is_some() {
                    let gone = state.retiring.backend_watched(old);
                    let id = continued.clone().unwrap_or_default();
                    state.resume_after.insert(tab, (id, gone));
                } else {
                    state.retiring.backend(old);
                }
            }
            state.trust.forget_tab(tab);
            // The put-off start's command is answered in the page it was sent from, unless that page
            // has closed.
            let owed = owed.and_then(|owed| {
                let asked_from = start_answer_dest(state, tab, &owed)?;
                let asked_from = match asked_from {
                    Dest::Page(asked_from) => asked_from,
                    Dest::Tab(_) => view,
                };
                Some((owed, asked_from))
            });
            // No lease check first: the tab's own old backend holds it until its teardown, which the
            // start waits for. The command is answered when the connect finishes. The tab is selected
            // only if it was on screen: a reset never moves the window to another tab.
            let resumed = continued.map(|provider_session_id| {
                let on_screen = state.shown_in(tab).is_some();
                resume_in(state, tab, request_id, provider_session_id, on_screen)
            });
            match &resumed {
                Some(Ok(())) => note_start_page(state, tab, request_id, view),
                Some(Err(_)) => {
                    state.resume_after.remove(&tab);
                }
                None => {}
            }
            // Before the draft the reset carries: the page puts a refused message back in
            // its box itself, and the draft that follows then only confirms it.
            if let Some((owed, asked_from)) = owed {
                out.send(
                    asked_from,
                    serialize_command_result_for_js(&owed, Err(crate::trust_gate::NOT_STARTED)),
                );
            }
            // `tabs` and the (empty) tab's own state, in the page that shows it.
            shown_payloads(state, tab, out);
            send_hello_if_open_sessions_changed(state, out);
            report_attention(state, before, out);
            match resumed {
                None => ok(out),
                Some(Ok(())) => {}
                Some(Err(why)) => refuse(out, &why),
            }
        }
        Err(why) => refuse(out, &why),
    }
}

/// What a failed tab's `r` says while no `claude` can be found: the row stays and says so, and nothing starts.
const STILL_NOT_FOUND: &str = "claude is still not found";
/// The same while the only `claude` found is one the lookup refuses, as the row says (found but not used).
const STILL_NOT_USED: &str = "claude is still found but not used; the window's log says why";

/// A failed tab's reset waiting on the lookup of `claude`, which runs on a worker.
pub(super) struct PendingResetCheck {
    tab: TabId,
    /// The page that pressed `r`: the answer goes there.
    view: ViewId,
    request_id: String,
    rx: mpsc::Receiver<Option<agent::ClaudeProblem>>,
}

/// Starts the lookup a failed tab's `r` waits for. A second `r` while one is out for the tab is refused rather than
/// queued: the first answers both.
fn start_reset_check(state: &mut AgentPanelState, tab: TabId, view: ViewId, request_id: &str) -> Result<(), String> {
    if state.reset_checks.iter().any(|check| check.tab == tab) {
        return Err("still looking for Claude Code".to_string());
    }
    let (tx, rx) = mpsc::channel();
    let missing = state.cli_missing.clone();
    // A thread the OS refuses is the page's to hear about: this runs inside the WebView's message callback, where a
    // panic is no answer.
    std::thread::Builder::new()
        .name("claude-lookup".to_string())
        .spawn(move || {
            // A window closed in the meantime has dropped the receiver; the answer has nowhere to go.
            let _ = tx.send(missing());
        })
        .map_err(|e| format!("could not look for Claude Code: {e}"))?;
    state.reset_checks.push(PendingResetCheck {
        tab,
        view,
        request_id: request_id.to_string(),
        rx,
    });
    Ok(())
}

/// The tick's half of a failed tab's `r`: a lookup that found `claude` lets the reset run; one that did not, or a
/// worker that died, leaves the tab as it was and tells the page. A tab that is no longer failed (it was reset or
/// closed meanwhile) is left alone and its `r` answered as refused.
pub(super) fn poll_reset_checks(state: &mut AgentPanelState, out: &mut Outbox) {
    let mut i = 0;
    while i < state.reset_checks.len() {
        let missing = match state.reset_checks[i].rx.try_recv() {
            Ok(missing) => missing,
            Err(mpsc::TryRecvError::Empty) => {
                i += 1;
                continue;
            }
            // A lookup that never answered is not a reason to start over on a guess.
            Err(mpsc::TryRecvError::Disconnected) => Some(agent::ClaudeProblem::Missing),
        };
        let check = state.reset_checks.remove(i);
        let still_failed = state
            .tabs
            .get(check.tab)
            .is_some_and(|t| matches!(t.backend(), TabBackend::Failed { .. }));
        if !still_failed {
            out.send(
                check.view,
                serialize_command_result_for_js(&check.request_id, Err("the tab is no longer waiting for Claude Code")),
            );
        } else if let Some(problem) = missing {
            let words = match problem {
                agent::ClaudeProblem::Missing => STILL_NOT_FOUND,
                agent::ClaudeProblem::Refused => STILL_NOT_USED,
            };
            out.send(
                check.view,
                serialize_command_result_for_js(&check.request_id, Err(words)),
            );
        } else {
            reset_tab(state, check.tab, check.view, &check.request_id, out);
        }
    }
}

/// One message from `view`'s page, [`admit`]ted, with the window's word on it: `refused` when the
/// window's gate turned the answer down (or the admission did), and which pages are on screen. Every
/// answer goes back to that page; what changes for a tab goes to the page that shows it. `pacer` is
/// that page's typing cadence, whose held envelopes a fresh document makes stale.
pub(super) fn handle_inbound_message(
    state: &mut AgentPanelState,
    message: InboundMessage,
    view: ViewId,
    admitted: Admitted,
    refused: Option<String>,
    screen: &Screen,
    pacer: &RefCell<crate::panel_pacer::Pacer>,
) -> Outbox {
    let mut out = Outbox::default();
    let Admitted { target, late_draft, .. } = admitted;
    let request_id = message.request_id().to_string();
    let tab_of = |target: Option<TabId>| target.expect("a tab-scoped message resolved to a tab");
    let ok = |out: &mut Outbox| out.send(view, serialize_command_result_for_js(&request_id, Ok(())));
    let refuse = |out: &mut Outbox, why: &str| {
        out.send(view, serialize_command_result_for_js(&request_id, Err(why)));
    };
    if let Some(why) = refused {
        if matches!(
            &message,
            InboundMessage::LoadPage { .. } | InboundMessage::PageLine { .. }
        ) {
            eprintln!("[agent_panel] dropped page request: {why}");
            return out;
        }
        // A trust question whose answer the window turned down is asked again (`refuse_trust_answer`);
        // every other refusal is the command's, and what it answered is left waiting.
        let trust = match (&message, target) {
            (InboundMessage::TrustAnswer { nonce, .. }, Some(tab)) => Some((tab, *nonce)),
            _ => None,
        };
        let asked_again = trust.and_then(|(tab, nonce)| {
            let asked = trust_question_answered(state, tab, nonce, view)?;
            Some(refuse_trust_answer(state, &request_id, tab, nonce, asked, &why, view))
        });
        match asked_again {
            Some(asked) => {
                let asked = asked.settled(state, screen);
                out.extend(asked);
            }
            None => refuse(&mut out, &why),
        }
        return out;
    }

    match message {
        InboundMessage::Ready { .. } => {
            // A held message of the document that was here answers a page that is gone.
            super::mentions::drop_view(state, view, &mut out);
            // A stream envelope the typing cadence still holds is already in the snapshot below
            // (it was drained from the queue and folded into the projection before it was held);
            // sent after that snapshot it would draw the same text twice.
            let discarded = pacer.borrow_mut().discard_pending();
            trace_discarded_first_texts(state, discarded);
            // Everything a fresh document is owed is decided by one pure function, so the reload
            // path is testable without a WebView -- see `ready_payloads`. A `ready` never opens the
            // chooser,  on the first one or on a reload -- the empty tab's dashboard is
            // what a fresh document sees. Only this page's tab is in it: another page's tab and its
            // cards are that page's.
            let payloads = {
                // The nav-mode mirror starts as `Other` and returns to it on every ready
                // (a reload). A fresh document starts BROWSE-or-dashboard, never whatever mode
                // the torn-down document last reported, so the mirror must not carry the old
                // value across -- see `reset_nav_mode_for_ready`'s own doc for the failure this
                // closes.
                reset_nav_mode_for_ready(state, view);
                let agent = hello_agent(state, view);
                let greeting = BackendGreeting::for_agent_titled(
                    state.backend_kind,
                    &agent,
                    state.project_dir.clone(),
                    eitri_core::agent_backend::Titles::Cached(&state.titles),
                );
                let open = state.tabs.open_sessions();
                let theme = eitri_core::agent_bridge::serialize_theme_for_js(&state.theme);
                let context =
                    eitri_core::agent_bridge::context_summary((state.editor_context)().as_ref(), &state.project_dir);
                let context_payload = eitri_core::agent_bridge::serialize_editor_context_for_js(context.as_ref());
                state.page_mut(view).last_context_payload = Some(context_payload.clone());
                let mut window = window_payloads(
                    eitri_core::agent_bridge::serialize_history_for_js(&state.history),
                    context_payload,
                    state.editor_link.as_ref(),
                );
                // How thinking is shown, before the tabs and the snapshot it applies to.
                window.insert(
                    0,
                    eitri_core::agent_bridge::serialize_display_for_js(state.thinking_display),
                );
                // Whether the page may speak of a turn's changes, before the tabs it would speak of.
                window.insert(1, review_settings_payload(state));
                if view.is_base() {
                    if let Some(pages) = &state.page_lines {
                        window.push(eitri_core::agent_bridge::serialize_pages_for_js(pages));
                    }
                }
                // The other window's page's view of a tab that arrived before this document was ready, before
                // the tabs whose switch takes it up.
                window.extend(chat_moves::take_held_imports(state, view));
                // A new document starts from this strip whatever an older one queued before it said:
                // it goes as it is, and counts as delivered with the rest of the greeting.
                let tabs = tabs_payload_recorded(state, view);
                strip_still_current(state, view, tabs.seq);
                let tabs = tabs.payload;
                // The base page's active tab, as always; an extra page's one tab, or nothing for an
                // extra page that shows none yet.
                let active = if view.is_base() {
                    state.tabs.active_state_payloads()
                } else {
                    match state.tabs.tab_in(view) {
                        Some(tab) => state.tabs.state_payloads(tab),
                        None => Vec::new(),
                    }
                };
                // What the last window left open, if the dashboard may offer it: a reloaded page
                // is told again, like everything else in the greeting. Only the base page offers it.
                let mut offered_greeting = greeting;
                offered_greeting
                    .resumable
                    .retain(|r| !eitri_core::tab_restore::is_open(&open, r));
                let offer = if view.is_base() { restore_offer(state) } else { None };
                let mut payloads = ready_payloads(
                    offered_greeting,
                    &open,
                    window,
                    tabs,
                    active,
                    Some(&theme),
                    state.keymap_help.as_deref(),
                    offer.as_ref(),
                    state.choose_project_hook.is_some(),
                );
                // Why the folder is no boundary: a fact about the window, said again to every document,
                // right after the greeting it belongs with.
                if let Some(note) = &state.root_note {
                    payloads.insert(1, eitri_core::agent_bridge::serialize_root_note_for_js(note));
                }
                // The status-band hints the replaced document was showing, of this view's tabs.
                for hint in hints_for_new_document(state, view) {
                    payloads.push(eitri_core::agent_bridge::serialize_review_hint_for_js(&hint));
                }
                // Whether remote control is on here, and who is connected from a phone: a new document starts with
                // neither.
                payloads.extend(super::remote_enroll::remote_on_for_new_document(state, view));
                payloads.extend(super::remote_enroll::connected_for_new_document(state));
                // The remote-control questions still waiting: the document that was here, if any, took them with it.
                if view.is_base() {
                    payloads.extend(super::remote_enroll::questions_for_new_document(
                        state,
                        std::time::Instant::now(),
                    ));
                }
                // Last, so nothing the document draws from the payloads above can reset it.
                let focused = state.page(view).is_some_and(|record| record.pane_focused);
                payloads.push(eitri_core::agent_bridge::serialize_pane_focus_for_js(focused));
                let offered = view.is_base() && restore_is_on_offer(state);
                let record = state.page_mut(view);
                record.last_open_ids = open;
                record.last_restore_offered = offered;
                record.last_hello_agent = agent;
                // From here on the tick may send into this document.
                record.document_ready = true;
                payloads
            };
            eprintln!("[agent_panel] view {}: ready", view.number());
            out.send_all(view, payloads);
            // A mode note set before any page was ready (the configured agent clamping the empty
            // tab) is held back by `send_mode_note`; this is the first moment it can be said, after
            // the `tabs` that shows the tab. Taking it clears it, so a later `tabs` does not repeat it.
            send_mode_note(state, &mut out);
            ok(&mut out);
            // The interrupted reverts, offered once the base page can show them (and again after a
            // reload). They are the window's, so only the base page offers them.
            if view.is_base() {
                let owed = state.review_flow.document_ready(&state.tabs);
                dispatch_review_outs(state, screen, owed, &mut out);
            }
            // A fresh document has no trust question on screen: the tab it shows, if that tab waits
            // for one, is asked again, and a fresh check gives `prefix i` its row. A `:trust` prompt
            // ends only when it was this page's.
            if command_prompt_is_in(state, view) {
                state.trust.drop_command_prompt();
            }
            let asked = match state.tab_of_page(view) {
                Some(tab) => {
                    state.trust.hide(tab);
                    let profile = waiting_profile(state, tab);
                    state.trust.begin_check(&profile);
                    reroute_or_ask(state, tab)
                }
                None => Outbox::default(),
            };
            let asked = asked.settled(state, screen);
            out.extend(asked);
            // `agent.restore = "auto"`: only once the base page can take what it starts.
            if view.is_base() {
                restore_at_launch(state, screen, &mut out);
            }
        }
        // The last window's tabs are offered, and asked about, in the base page only.
        InboundMessage::RestoreLast { .. } | InboundMessage::RestoreAnswer { .. } if !view.is_base() => {
            refuse(&mut out, RESTORE_IS_THE_BASE_PAGES);
        }
        InboundMessage::RestoreLast { .. } => {
            let tab = tab_of(target);
            let result = ask_for_restore(state, tab, RestoreAsk::Dashboard);
            match deliver_restore(state, screen, result, &mut out) {
                Ok(()) => ok(&mut out),
                Err(why) => refuse(&mut out, &why),
            }
        }
        InboundMessage::RestoreAnswer { nonce, keep_bypass, .. } => {
            // A `y` has already met the window's gate (`admission`); refused there, the question is
            // left as it was, so the same nonce can still be answered and the restore key asks it again.
            let result = answer_restore_question(state, nonce, keep_bypass);
            match deliver_restore(state, screen, result, &mut out) {
                Ok(()) => ok(&mut out),
                Err(why) => refuse(&mut out, &why),
            }
        }
        InboundMessage::SendMessage { text, .. } => {
            let tab = tab_of(target);
            // A sent prompt is history whether or not the backend takes it, as in Claude Code.
            remember_prompts(state, std::slice::from_ref(&text), &mut out);
            // ...and no longer the tab's draft (a refusal puts it back in the box panel-side).
            // (A held message cleared it at Enter; what was typed since is the box's, not this message's.)
            if state.mention_sections.is_none() {
                state.tabs.note_sent(tab);
            }
            enum Plan {
                Send(FirstTurn),
                Defer(FirstTurn),
                Refused(&'static str),
            }
            let plan = {
                // wire 1's one composition point, read when Enter is pressed. Above both
                // backends on purpose: the turn's String travels unmodified from here to
                // `send_turn` on either path. A `None` context means the turn goes out exactly as
                // the user typed it.
                let composed = super::mentions::compose(state, &text);
                let moving = state.tabs.is_moving(tab);
                let t = state.tabs.get_mut(tab).expect("resolved above");
                if moving {
                    // Its record is taken: a turn or a start begun here would not move with it.
                    Plan::Refused(eitri_core::tab_set::MOVING)
                } else if t.pending_handoff.is_some() {
                    // Starting here would spawn a second `claude` alongside the one still being
                    // closed, in the same project.
                    Plan::Refused("the previous session is still being handed off to a terminal")
                } else {
                    match t.backend() {
                        // `typed` is the user's own: the panel shows what was typed, never the
                        // composed wire text. The tab set sends it under the tab's mode (an external
                        // agent's turn runs under it), traced from the send.
                        TabBackend::Live(_) => Plan::Send(FirstTurn {
                            typed_by_person: true,
                            wire: composed,
                            typed: text.clone(),
                            shell_line: false,
                        }),
                        // The session waits for the trust check (and, when it must, the question)
                        // before anything is spawned.
                        TabBackend::NotStarted => Plan::Defer(FirstTurn {
                            typed_by_person: true,
                            wire: composed,
                            typed: text.clone(),
                            shell_line: false,
                        }),
                        TabBackend::Starting(_) => Plan::Refused("the session is still starting"),
                        TabBackend::AwaitingTrust(_) => Plan::Refused("waiting for the trust question"),
                        TabBackend::Failed { .. } => Plan::Refused("this tab's session failed; press r to start over"),
                    }
                }
            };
            match plan {
                // Through the tab set, which puts the tab in the running set before anything goes out; in a
                // linked window the message may wait there for the presence, and the tick answers it then.
                Plan::Send(turn) => match state.tabs.send_prompt(tab, &request_id, view, turn) {
                    // Answered here, to this page: a refusal never reached the ledger, and must not
                    // take a held message's entry that an earlier page numbered alike.
                    Some(eitri_core::tab_set::Sent::Prompt { typed, outcome, .. }) => {
                        prompt_sent(state, screen, tab, Some((view, &request_id)), &typed, outcome, &mut out);
                    }
                    Some(eitri_core::tab_set::Sent::NotSent { note, .. }) => {
                        let refused = Err(BackendError {
                            message: note,
                            benign: true,
                        });
                        apply_command_outcome(state, screen, view, tab, &request_id, refused, &mut out);
                        send_queue(state, tab, &mut out);
                    }
                    Some(sent) => apply_sent(state, screen, sent, &mut out),
                    // Held for the presence: the tick answers it, to this page if it is still there.
                    None => {
                        state.owed.owe(view, &request_id, Owed::HeldPrompt(tab));
                    }
                },
                // The `command_result` is owed once the connect finishes and the first turn is
                // sent (`collect_pending_starts`), or when the question is put off.
                Plan::Defer(first_turn) => {
                    let start = DeferredStart::FirstSend {
                        request_id: request_id.clone(),
                        first_turn,
                    };
                    match gate_start(state, tab, start) {
                        Ok(()) => {
                            note_start_page(state, tab, &request_id, view);
                            send_tabs(state, &mut out)
                        }
                        Err(why) => refuse(&mut out, &why),
                    }
                }
                Plan::Refused(why) => refuse(&mut out, why),
            }
        }
        InboundMessage::Resume {
            provider_session_id, ..
        } => {
            let tab = tab_of(target);
            if state.tabs.get(tab).is_some_and(|t| t.pending_handoff.is_some()) {
                refuse(&mut out, "the previous session is still being handed off to a terminal");
                return out;
            }
            let canonical = state.canonical_project_dir.clone();
            // The session is the tab's agent's: its lease is under that agent's name.
            let agent = state.tabs.get(tab).map_or(AgentId::Claude, |t| t.agent().clone());
            let lease_name = agent.record_name();
            let held =
                agent::lease::SessionLease::is_held(&lease_name, &canonical, &provider_session_id).unwrap_or(false);
            let route = state.tabs.route_resume(tab, &agent, &provider_session_id, held);
            match route {
                // Already open: a bound tab is never selected, its view is shown instead; a base-view
                // tab is the base view's now, and the base view is shown if the pick was elsewhere.
                ResumeRoute::SwitchTo(open) => {
                    let open_in = state.tabs.view_of(open);
                    if open_in.is_base() {
                        send_switch(state, screen, &mut out);
                    }
                    if open_in != view {
                        out.effect(Effect::ShowView(open_in));
                    }
                    ok(&mut out);
                }
                ResumeRoute::Refuse(why) => refuse(&mut out, &why),
                ResumeRoute::StartIn(target_tab) => {
                    let deferred = resume_in(state, target_tab, &request_id, provider_session_id, true);
                    // An extra page's empty tab takes the resume itself; any other opens in a new
                    // base-view tab, and the base view takes the keys.
                    let started_in = state.tabs.view_of(target_tab);
                    match deferred {
                        // The command_result is owed once the connect finishes, or when the question
                        // is put off.
                        Ok(()) => {
                            note_start_page(state, target_tab, &request_id, view);
                            send_view(state, screen, started_in, &mut out);
                            if started_in != view {
                                out.effect(Effect::ShowView(started_in));
                            }
                        }
                        Err(why) => refuse(&mut out, &why),
                    }
                }
            }
        }
        InboundMessage::Interrupt { .. } => {
            let tab = tab_of(target);
            // The desk's `!` line is the desk's to stop first; only then the agent's work.
            let outcome = if state.tabs.interrupt_shell_line(tab) {
                Ok(())
            } else {
                interrupt_agent(state, tab)
            };
            apply_command_outcome(state, screen, view, tab, &request_id, outcome, &mut out);
        }
        InboundMessage::StopBackground { running, .. } => {
            // The tab the question was asked in, never the one on screen now. Claude stops its tasks;
            // Codex ends its session through the pump. A refusal leaves the session alone.
            let tab = tab_of(target);
            let outcome = state.tabs.stop_background(tab, running);
            apply_command_outcome(state, screen, view, tab, &request_id, outcome, &mut out);
        }
        InboundMessage::PathComplete { query, seq, .. } => {
            if let Some(tab) = target {
                state.path_menu.ask(tab, view, query, seq);
                super::path_menu::poll(state, &mut out);
                ok(&mut out);
            } else {
                refuse(&mut out, "no tab for path completion");
            }
        }
        message @ (InboundMessage::StopTask { .. } | InboundMessage::TaskOutput { .. }) => {
            let tab = tab_of(target);
            if state.task_commands.len() >= 64 {
                refuse(&mut out, "too many task requests: wait for an answer");
            } else {
                let (task_id, output) = match message {
                    InboundMessage::StopTask { task_id, .. } => (task_id, None),
                    InboundMessage::TaskOutput { task_id, offset, .. } => (task_id, Some(offset)),
                    _ => unreachable!(),
                };
                let result = backend_for(&mut state.tabs, tab).and_then(|backend| match output {
                    Some(Some(offset)) => backend
                        .task_output_range(&task_id, offset, TASK_OUTPUT_VIEW_BYTES)
                        .map(TaskCommandReply::Output),
                    Some(None) => backend.task_output(&task_id).map(TaskCommandReply::Output),
                    None => backend.stop_task(&task_id).map(TaskCommandReply::Stop),
                });
                match result {
                    Ok(reply) => state.task_commands.push(PendingTaskCommand {
                        view,
                        tab,
                        request_id,
                        task_id,
                        reply,
                    }),
                    Err(error) => refuse(&mut out, &error.message),
                }
            }
        }
        message @ (InboundMessage::ListModels { .. }
        | InboundMessage::SetModel { .. }
        | InboundMessage::ApplyEffort { .. }) => {
            let tab = tab_of(target);
            if state.task_commands.len() >= 64 {
                refuse(&mut out, "too many task requests: wait for an answer");
            } else {
                let result = backend_for(&mut state.tabs, tab).and_then(|backend| match message {
                    InboundMessage::ListModels { .. } => backend.list_models().map(TaskCommandReply::Models),
                    InboundMessage::SetModel { model, .. } => backend.set_model(&model).map(TaskCommandReply::Settings),
                    InboundMessage::ApplyEffort { effort, .. } => {
                        backend.apply_effort(&effort).map(TaskCommandReply::Settings)
                    }
                    _ => unreachable!(),
                });
                match result {
                    Ok(reply) => state.task_commands.push(PendingTaskCommand {
                        view,
                        tab,
                        request_id,
                        task_id: String::new(),
                        reply,
                    }),
                    Err(error) => refuse(&mut out, &error.message),
                }
            }
        }
        message @ InboundMessage::PermissionResponse { .. } => {
            let tab = tab_of(target);
            let answer = PermissionAnswer::of(message).expect("matched as a permission_response above");
            // `Ctrl+y`, BROWSE's answer keys and a card's own buttons all arrive here, so the one
            // admission step covers every way a card is answered: another page's card is refused
            // before the window is asked, then the window's gate. `answer_card` checks the page again,
            // so a later change to the admission step cannot skip it.
            let answered = answer_permission_response(
                &mut state.tabs,
                state.rules_dir.as_deref(),
                &state.project_dir,
                tab,
                view,
                answer,
            );
            match answered {
                Err(why) => refuse(&mut out, &why),
                Ok(outcome) => apply_command_outcome(state, screen, view, tab, &request_id, outcome, &mut out),
            }
        }
        // A broker card's answer goes to the broker, by the card's own id, past the same gates as any card
        // (admission). The call runs from the tick; the page learns the card went from its `editor_cards`.
        InboundMessage::EditorCardAnswer {
            card_id,
            decision,
            origin,
            ..
        } => {
            let tab = tab_of(target);
            let decision = decision.card_decision();
            match super::editor_glue::answer_card(state, tab, &card_id, decision, origin.origin(), &mut out) {
                Ok(()) => ok(&mut out),
                Err(why) => refuse(&mut out, &why),
            }
        }
        // Only the person answers Claude's question, from the page that shows its tab, past the window's
        // gate (admission); `answer_question` checks the page again and checks the answer against the
        // set before anything is sent. A refusal leaves the question waiting and the page keeps its
        // draft. The answer goes out without waiting for the agent; its `command_result` follows from
        // the tick once the agent replied (`poll_question_sends`).
        InboundMessage::QuestionAnswer {
            interaction_id,
            answers,
            ..
        } => {
            let tab = tab_of(target);
            let answers = answers
                .into_iter()
                .map(eitri_core::agent_bridge::QuestionAnswerWire::into_answer)
                .collect();
            let sent = state.tabs.answer_question(tab, view, &interaction_id, answers);
            question_sent(state, screen, view, tab, &request_id, sent, &mut out);
        }
        InboundMessage::QuestionDecline {
            interaction_id,
            kind,
            so_far,
            ..
        } => {
            let tab = tab_of(target);
            let so_far = so_far
                .into_iter()
                .map(eitri_core::agent_bridge::QuestionAnswerWire::into_answer)
                .collect();
            let sent = state
                .tabs
                .decline_question(tab, view, &interaction_id, kind.kind(), so_far);
            question_sent(state, screen, view, tab, &request_id, sent, &mut out);
        }
        // The window's HINT session reacts by calling back into this panel, so it is told once the
        // step is over.
        InboundMessage::HintRequest { .. } => out.effect(Effect::Hint(HintInbound::Request)),
        InboundMessage::HintTargets { session_id, count, .. } => {
            out.effect(Effect::Hint(HintInbound::Targets { session_id, count }));
        }
        InboundMessage::TurnRendered {
            receive_to_frame_ms, ..
        } => {
            // Purely a diagnostic: no command_result, and nothing downstream reads it. A WebView
            // that never sends one costs only a missing column in a trace line.
            let tab = tab_of(target);
            // A flush between ticks (a focus change, a tab switch) may have released the first
            // text after the last tick: its stamp is applied first, or this report -- which the
            // page sends only once -- would find no dispatch to measure from and be dropped.
            let stamps = pacer
                .try_borrow_mut()
                .map(|mut pacer| pacer.take_first_text())
                .unwrap_or_default();
            stamp_first_text_dispatches(state, stamps);
            if let Some(trace) = state.tabs.get_mut(tab).and_then(|t| t.session.turn_trace.as_mut()) {
                trace.mark_painted(receive_to_frame_ms);
                if trace.is_complete() {
                    trace.emit();
                }
            }
        }
        InboundMessage::HandoffToTerminal { .. } => {
            let tab = tab_of(target);
            // Its session is the move's: handed to a terminal, the window it lands in would hold a
            // closed one.
            if state.tabs.is_moving(tab) {
                refuse(&mut out, eitri_core::tab_set::MOVING);
                return out;
            }
            // The command is built from canonical state FIRST, while the session is still readable
            // -- ownership moves to the shutdown worker below and nothing here can read it after
            // that. Owned values rather than borrows because `prepare_handoff` is a pure function
            // and the state must not stay borrowed across the steps that follow.
            let (project_dir, session_facts, already_handing_off) = {
                let t = state.tabs.get(tab).expect("resolved above");
                let facts = t.live().map(|backend| {
                    // `provider_session_id()` BEFORE `projection()`, and the order is load-bearing
                    // rather than stylistic. On the sidecar path both reach for the SAME
                    // `Mutex<IngestState>` -- `projection()` returns a guard that holds it, and
                    // `provider_session_id()` locks it again. `std::sync::Mutex` is not reentrant,
                    // so calling the second inside the first's scope deadlocks the GTK main loop
                    // outright: no clicks, no keys, not even a compositor close request, with the
                    // last frame still painted so it looks alive. Reproduced 3/3 on 2026-09-15 and
                    // confirmed by backtrace.
                    let provider_session_id = backend.provider_session_id();
                    let projection = backend.projection();
                    let question_waiting = projection.waiting_questions().next().is_some();
                    (
                        provider_session_id,
                        projection.active_turn_id.clone(),
                        projection.cli_unseen_busy,
                        eitri_core::tabs::background_summary(&projection),
                        projection.background_calls,
                        question_waiting,
                        projection.cwd.clone(),
                        crate::terminal_handoff::ConversationLiveness::of(&projection.status),
                    )
                });
                (state.project_dir.clone(), facts, t.pending_handoff.is_some())
            };
            if already_handing_off {
                refuse(&mut out, "this conversation is already being handed off");
                return out;
            }
            let prepared = crate::terminal_handoff::prepare_handoff(
                &project_dir,
                session_facts.as_ref().map(
                    |(
                        provider_session_id,
                        active_turn_id,
                        cli_unseen_busy,
                        background,
                        background_count,
                        question_waiting,
                        reported_cwd,
                        liveness,
                    )| {
                        crate::terminal_handoff::HandoffFacts {
                            provider_session_id: provider_session_id.as_deref(),
                            active_turn_id: active_turn_id.as_deref(),
                            cli_unseen_busy: *cli_unseen_busy,
                            background: background.as_deref(),
                            background_count: *background_count,
                            question_waiting: *question_waiting,
                            reported_cwd: reported_cwd.as_deref(),
                            liveness: *liveness,
                        }
                    },
                ),
            );
            let command = match prepared {
                Ok(command) => command,
                Err(refusal) => {
                    // The frontend disables the control for exactly these states, so reaching here
                    // means its view was stale (a turn started between the render and the click).
                    // The session is untouched.
                    refuse(&mut out, &refusal.message());
                    return out;
                }
            };

            // The handoff's steps, in the only order that is safe: from here on this tab
            // holds no session (`NotStarted` at once), so no further turn can be sent,
            // and `shutdown()` itself cancels every pending permission (fail-closed) and closes the
            // provider's session.
            let queue_before = queue_shown(state, tab);
            // The tab becomes `NotStarted` under whatever bypass
            // prompt was open for it -- a live tab's `Switch to bypass?` whose `y` would then also
            // have moved the window default. Reached by clicks that never pass
            // through the panel's y/n, so it is dropped here; `confirm_bypass`'s own re-check of
            // which question was asked is the other half. Only this page's: the click was here.
            state.tabs.drop_bypass_prompt_in(view);
            // Retired until its close signal arrives (`collect_pending_handoffs`), or, if the tab is
            // closed first, until its worker has dropped it (`Retiring::tab`).
            let Some(retired) = state.tabs.take_for_handoff(tab) else {
                // `prepare_handoff` already returned `NoSession` for this, so it is unreachable
                // unless something took the session between that call and this line -- both run on
                // the GTK main loop, so nothing can. Reported rather than unwrapped anyway.
                refuse(&mut out, "the conversation ended before it could be handed off");
                return out;
            };
            // `closed` is the whole signal the command waits for: it is not dispatched until the
            // shutdown has returned, and a panic inside `shutdown()` reads as a failed close
            // (`collect_pending_handoffs`). The drop that ends the child comes later; the command is
            // deliberately NOT made to wait for it, but a window closing at this moment is.
            let Teardown { closed, dropped } = spawn_teardown(retired);
            if let Some(t) = state.tabs.get_mut(tab) {
                // The page that asked holds the request, wherever the tab is when the close ends.
                state.owed.owe(view, &request_id, Owed::Handoff(tab));
                t.pending_handoff = Some(PendingHandoff {
                    request_id,
                    command,
                    closed_rx: closed,
                    dropped_rx: dropped,
                });
            }
            // No command_result yet -- `collect_pending_handoffs` owes it once the close finishes.
            send_tabs(state, &mut out);
            if queue_shown(state, tab) != queue_before {
                send_queue(state, tab, &mut out);
            }
        }
        // A pick in the chooser, which lists every tab: a base-view tab is selected there, and the
        // base view shown if the pick was in another page; a tab bound to an extra view is never
        // selected, its view is shown instead.
        InboundMessage::SelectTab { .. } => {
            let tab = tab_of(target);
            match state.tabs.select_place(tab) {
                Some(TabPlace::View(shown_in, _)) => {
                    if shown_in != view {
                        out.effect(Effect::ShowView(shown_in));
                    }
                }
                _ => {
                    send_switch(state, screen, &mut out);
                    if !view.is_base() {
                        out.effect(Effect::ShowView(ViewId::BASE));
                    }
                }
            }
            ok(&mut out);
        }
        InboundMessage::ToggleFollowing { .. } => {
            let tab = tab_of(target);
            if state.tabs.toggle_following(tab) {
                if !state.tabs.get(tab).unwrap().following {
                    state.editor_follower.forget(tab.0);
                }
                send_tabs(state, &mut out);
                ok(&mut out);
            } else {
                let why = if state.tabs.is_moving(tab) {
                    eitri_core::tab_set::MOVING.to_string()
                } else {
                    format!("no tab {}", tab.0)
                };
                refuse(&mut out, &why);
            }
        }
        InboundMessage::RenameTab { name, .. } => {
            let tab = tab_of(target);
            state.tabs.rename(tab, &name);
            send_tabs(state, &mut out);
            ok(&mut out);
        }
        InboundMessage::CloseTab { .. } => {
            let tab = tab_of(target);
            match close_tab_asked(state, tab, &mut out) {
                Ok(()) => ok(&mut out),
                Err(why) => refuse(&mut out, why),
            }
        }
        InboundMessage::ResetTab { .. } => {
            let tab = tab_of(target);
            // A tab that failed because no `claude` was there starts over only if one is there now: the lookup
            // is the disk's, so it runs on a worker and the tick finishes the reset (or refuses it).
            let failure = state.tabs.get(tab).and_then(|t| match t.backend() {
                TabBackend::Failed { reason } => Some(reason.clone()),
                _ => None,
            });
            if failure.is_some_and(|reason| agent::failure_names_missing_cli(&reason)) {
                match start_reset_check(state, tab, view, &request_id) {
                    Ok(()) => {}
                    Err(why) => refuse(&mut out, &why),
                }
            } else {
                reset_tab(state, tab, view, &request_id, &mut out);
            }
        }
        // `TabSet::cycle_mode` returns a `Result<ModeCycle, String>` -- a live,
        // active tab can toggle too, not only an empty one, so `Ok(Changed(_))` here no longer
        // means "was empty". `was_empty` is read BEFORE the call so the persisted "next launch"
        // default is written only for the tab whose move that preference is actually
        // about. `Confirm(plan)` dispatches the real y/n prompt rather than refusing --
        // `apply_confirm_bypass`, below, is what a later `confirm_bypass` message answers it with.
        InboundMessage::CycleMode { .. } => {
            let tab = tab_of(target);
            match cycle_tab_mode(state, tab, view) {
                Ok(eitri_core::tab_set::ModeCycle::Changed(_)) => {
                    send_tabs(state, &mut out);
                    ok(&mut out);
                }
                Ok(eitri_core::tab_set::ModeCycle::Confirm(plan)) => {
                    out.send(view, eitri_core::agent_bridge::serialize_confirm_bypass_for_js(&plan));
                    ok(&mut out);
                }
                Err(why) => refuse(&mut out, &why),
            }
        }
        InboundMessage::RunShell { text, .. } => {
            let tab = tab_of(target);
            remember_prompts(state, std::slice::from_ref(&text), &mut out);
            if state.mention_sections.is_none() {
                state.tabs.note_sent(tab);
            }
            let fresh = matches!(
                state.tabs.get(tab).expect("resolved above").backend(),
                TabBackend::NotStarted
            );
            if fresh && eitri_core::tab_set::shell_command(&text).is_some() {
                // A tab with no session starts one first, trust question and all, as any first message does.
                // The start runs the line instead of sending its words.
                let start = DeferredStart::FirstSend {
                    request_id: request_id.clone(),
                    first_turn: FirstTurn {
                        typed_by_person: false,
                        wire: text.clone(),
                        typed: text,
                        shell_line: true,
                    },
                };
                match gate_start(state, tab, start) {
                    Ok(()) => {
                        note_start_page(state, tab, &request_id, view);
                        send_tabs(state, &mut out)
                    }
                    Err(why) => refuse(&mut out, &why),
                }
            } else {
                match state.tabs.queue_shell_line(tab, &text, now_ms()) {
                    Ok(()) => {
                        match state.tabs.flush_queue(tab) {
                            Some(flush) => apply_flush(state, screen, tab, flush, &mut out),
                            None => send_queue(state, tab, &mut out),
                        }
                        ok(&mut out);
                    }
                    Err(why) => refuse(&mut out, &why),
                }
            }
        }
        InboundMessage::OpenDetail { .. } => {
            let tab = tab_of(target);
            // A route away from this page's bypass prompt (the panel cancels its own on `tab_detail`).
            state.tabs.drop_bypass_prompt_in(view);
            out.send(view, detail_payload(state, tab));
            ok(&mut out);
        }
        InboundMessage::QueueMessage { text, turn_id, .. } => {
            let tab = tab_of(target);
            // Captured now: what the editor shows when Enter is pressed.
            let wire = super::mentions::compose(state, &text);
            match state
                .tabs
                .queue_message(tab, &text, wire, now_ms(), true, agent::PromptOrigin::Typed)
            {
                Ok(_) => {
                    remember_prompts(state, std::slice::from_ref(&text), &mut out);
                    // The turn may have ended between the panel's Enter and this handler.
                    match state.tabs.flush_queue(tab) {
                        Some(flush) => apply_flush(state, screen, tab, flush, &mut out),
                        None => {
                            // Into the running turn when its agent can take it; held otherwise. Never
                            // waits on the agent: the outcome arrives through the pump.
                            state.tabs.steer_held(tab, now_ms(), turn_id.as_deref());
                            send_queue(state, tab, &mut out);
                        }
                    }
                    ok(&mut out);
                }
                Err(why) => refuse(&mut out, &why),
            }
        }
        InboundMessage::TakeBackQueue { .. } => {
            let tab = tab_of(target);
            let texts = state.tabs.take_back_queue(tab);
            out.send(
                view,
                eitri_core::agent_bridge::serialize_queue_taken_for_js(tab, &texts),
            );
            send_queue(state, tab, &mut out);
            ok(&mut out);
        }
        InboundMessage::SendNow { text, .. } => {
            let tab = tab_of(target);
            let wire = super::mentions::compose(state, &text);
            let result = state.tabs.send_now(tab, &text, wire, now_ms());
            if !text.trim().is_empty() {
                remember_prompts(state, std::slice::from_ref(&text), &mut out);
            }
            match result {
                Err(why) => refuse(&mut out, &why),
                Ok(eitri_core::tab_set::SendNow::Interrupting(outcome)) => {
                    send_queue(state, tab, &mut out);
                    apply_command_outcome(state, screen, view, tab, &request_id, outcome, &mut out);
                }
                Ok(eitri_core::tab_set::SendNow::Flushed(flush)) => {
                    if let Some(flush) = flush {
                        apply_flush(state, screen, tab, flush, &mut out);
                    }
                    ok(&mut out);
                }
            }
        }
        InboundMessage::Draft { text, .. } => {
            // A mirror, answered with nothing, from the page whose view holds the tab (a base-view tab
            // in the background included: the base page flushes as it switches away from it).
            let tab = tab_of(target);
            let held_here = page_may_answer(&state.tabs, tab, view);
            if held_here.is_ok() {
                state.tabs.set_draft(tab, &text);
                // The panel may be idle, with no tick due: a window killed now still has this in its file.
                super::restore::remember_tabs(state);
                // A draft that would not come back is something the window's close asks about, and window 1 reads it in
                // the window's summary.
                super::tick::report_busy(state, &mut out);
            } else if late_draft || state.tabs.take_late_draft(tab, view) {
                deliver_late_draft(state, tab, &text, &mut out);
            } else if let Err(why) = held_here {
                eprintln!("[agent_panel] refused {request_id}: {why}");
                refuse(&mut out, &why);
            }
        }
        InboundMessage::HistoryPush { text, .. } => {
            remember_prompts(state, std::slice::from_ref(&text), &mut out);
            ok(&mut out);
        }
        // `gf`: the window shows the editor and hands it the file; its answer is the page's.
        InboundMessage::OpenPath { path, line, .. } => {
            let tab = if view.is_base() {
                state.tabs.active()
            } else {
                let Some(tab) = state.tabs.tab_in(view) else {
                    refuse(&mut out, "no tab in this view");
                    return out;
                };
                tab
            };
            if let Some(result) = super::path_lookup::pick(state, &path, line, view, &request_id, tab) {
                if let Err(why) = result {
                    refuse(&mut out, &why);
                }
            } else {
                match super::links::open_path_exact(&state.project_dir, &path) {
                    Ok(resolved) => out.effect(Effect::AskEditor {
                        request: eitri_core::scratch::open_request(&resolved, line),
                        then: EditorThen::Answer { view, request_id },
                    }),
                    Err(super::links::OpenPathRefusal::Missing)
                        if eitri_core::path_complete::suffix_wanted(&path).is_some() =>
                    {
                        if let Err(why) = super::path_lookup::start(state, path, line, view, request_id.clone(), tab) {
                            refuse(&mut out, &why);
                        }
                    }
                    Err(why) => refuse(&mut out, &why.message(&path)),
                }
            }
        }
        // `c` in BROWSE: the jobs are built here, from this tab's own session and edit calls, and run
        // on a worker; the answer is an envelope carrying the request id, sent by the tick.
        InboundMessage::ReviewRequest { turn, scope, .. } => {
            let tab = tab_of(target);
            // Memory only: the draft is cloned here, on the GTK thread, and moves to the worker.
            let job = state.tabs.review_overview_job(tab, turn.into(), scope.into());
            let draft = state.tabs.review_draft(tab).cloned();
            let started = job.and_then(|job| {
                let draft = draft?;
                let id = request_id.clone();
                start_review_job(state, view, &request_id, Some((tab.0, turn)), move || {
                    job.run()
                        .map(|overview| eitri_core::agent_bridge::serialize_review_for_js(&id, tab, &overview, &draft))
                        .map_err(|why| why.to_string())
                })
            });
            if let Err(why) = started {
                refuse(&mut out, &why);
            }
        }
        // The panel's revert, undo, comment, send and recovery messages belong to the review flow,
        // which checks, asks the editor and does the writing without ever waiting here. Its answers
        // come back now or on a later tick.
        InboundMessage::ReviewRevert { .. }
        | InboundMessage::ReviewUndo { .. }
        | InboundMessage::ReviewCommentAdd { .. }
        | InboundMessage::ReviewCommentRemove { .. }
        | InboundMessage::ReviewSend { .. }
        | InboundMessage::ReviewRecover { .. }
        | InboundMessage::OpenInEditor { .. } => run_review_flow(state, screen, view, message, &mut out),
        InboundMessage::ReviewDiffRequest { turn, scope, path, .. } => {
            let tab = tab_of(target);
            let job = state.tabs.review_diff_job(tab, turn, scope.into(), &path);
            let started = job.and_then(|job| {
                let id = request_id.clone();
                start_review_job(state, view, &request_id, None, move || {
                    job.run()
                        .map(|diff| eitri_core::agent_bridge::serialize_review_diff_for_js(&id, tab, &diff))
                        .map_err(|why| why.to_string())
                })
            });
            match started {
                Ok(()) => {
                    // A file the editor draws moves to this turn with the panel.
                    state.review_editor.sync(&mut state.review_flow);
                    state
                        .review_flow
                        .panel_shows(&state.tabs, tab, turn, scope.into(), &path);
                }
                Err(why) => refuse(&mut out, &why),
            }
        }
        // The text is written to the scratch file on a worker; the tick hands nvim the view, or tells the page why
        // it could not be written.
        InboundMessage::ViewInEditor { title, text, .. } => {
            let asker = ViewAsker::Page {
                view,
                request_id: request_id.clone(),
            };
            if let Err(why) = start_view_job(state, asker, &title, text) {
                refuse(&mut out, &why);
            }
        }
        // `Ctrl+g` on a pending edit card: the file is read on a worker, and the tick hands nvim the diff.
        InboundMessage::ViewProposal { path, change, .. } => {
            let started = if state.scratch.is_none() {
                Err("the scratch directory could not be created at startup".to_string())
            } else {
                proposal_target(&state.project_dir, &path).and_then(|file| {
                    let name = file.to_string_lossy().into_owned();
                    start_proposal_job(state, view, &request_id, name, move || {
                        eitri_core::scratch::proposal_texts(&file, &change)
                    })
                })
            };
            if let Err(why) = started {
                refuse(&mut out, &why);
            }
        }
        // `Ctrl+g` on a proposal an editor tool computed: the two sides come from the review store, by the
        // proposal's id in this tab's memory; the page names only which proposal and which file.
        InboundMessage::EditorCardView { proposal, file, .. } => {
            let tab = tab_of(target);
            if let Err(why) = super::editor_glue::view_proposal(state, view, &request_id, tab, &proposal, file) {
                refuse(&mut out, &why);
            }
        }
        InboundMessage::QuestionDraft {
            interaction_id, draft, ..
        } => {
            if let Err(why) = state
                .tabs
                .set_question_draft(tab_of(target), view, &interaction_id, draft)
            {
                refuse(&mut out, &why);
            }
        }
        InboundMessage::EditQuestionOther {
            interaction_id,
            question_index,
            text,
            ..
        } => {
            let tab = tab_of(target);
            let prepared = (|| {
                state
                    .tabs
                    .check_question_edit(tab, view, &interaction_id, Some(question_index))?;
                if text.len() > 64 * 1024 {
                    return Err("the question text exceeds 64 KiB".to_string());
                }
                state
                    .scratch
                    .as_mut()
                    .ok_or("the scratch directory could not be created at startup")?
                    .prepare_edit(&text)
                    .map_err(|e| format!("could not write the scratch file: {e}"))
            })();
            let (request, edit) = match prepared {
                Ok(pair) => pair,
                Err(why) => {
                    refuse(&mut out, &why);
                    return out;
                }
            };
            if let Err(why) = state
                .tabs
                .begin_question_edit(tab, edit.id, interaction_id, question_index)
            {
                edit.cleanup();
                refuse(&mut out, &why);
                return out;
            }
            out.effect(Effect::AskEditor {
                request,
                then: EditorThen::Draft {
                    view,
                    request_id,
                    tab,
                    text,
                    edit,
                },
            });
        }
        // `Ctrl+g`: the draft goes to nvim. Begun here; the window's answer keeps the edit or undoes it.
        InboundMessage::EditDraft { text, .. } => {
            let tab = tab_of(target);
            let prepared = state
                .scratch
                .as_mut()
                .map(|dir| {
                    dir.prepare_edit(&text)
                        .map_err(|e| format!("could not write the scratch file: {e}"))
                })
                .unwrap_or_else(|| Err("the scratch directory could not be created at startup".to_string()));
            let (request, edit) = match prepared {
                Ok(pair) => pair,
                Err(why) => {
                    refuse(&mut out, &why);
                    return out;
                }
            };
            if let Err(why) = state.tabs.begin_scratch_edit(tab, edit.id) {
                edit.cleanup();
                refuse(&mut out, &why);
                return out;
            }
            out.effect(Effect::AskEditor {
                request,
                then: EditorThen::Draft {
                    view,
                    request_id,
                    tab,
                    text,
                    edit,
                },
            });
        }
        // `<leader>bd`: quiet unless `close_needs_confirm`, about the tab this page shows; `prefix &`
        // never comes through here.
        InboundMessage::TabVerb {
            verb: TabVerbWire::Close,
            ..
        } if state
            .tab_of_page(view)
            .is_some_and(|tab| !state.tabs.close_needs_confirm(tab)) =>
        {
            let shown = state.tab_of_page(view).expect("checked in the guard");
            match close_tab_asked(state, shown, &mut out) {
                Ok(()) => ok(&mut out),
                Err(why) => refuse(&mut out, why),
            }
        }
        // `:tabnew <agent>`: the page names the agent, so the hook the prefix's `c` goes through (which
        // carries none) is not used. Keys are in the panel already, since the word was typed there.
        InboundMessage::TabVerb {
            verb: TabVerbWire::New,
            agent: Some(word),
            ..
        } => match open_tab_named(state, view, &word) {
            Ok(()) => {
                send_tabs(state, &mut out);
                // The new tab is the base view's: typed in an extra page, the base view is shown and
                // takes the keys first.
                if !view.is_base() {
                    out.effect(Effect::ShowView(ViewId::BASE));
                }
                out.send(ViewId::BASE, eitri_core::agent_bridge::serialize_enter_input_for_js());
                ok(&mut out);
            }
            Err(why) => refuse(&mut out, &why),
        },
        InboundMessage::TabVerb { verb, .. } => match run_tab_verb(state, view, verb, &mut out) {
            Ok(()) => ok(&mut out),
            Err(why) => refuse(&mut out, why),
        },
        InboundMessage::CycleDefaultMode { .. } => {
            // `Shift+Tab` on the chooser's `New session` row or a record: the window's
            // remembered default, exactly as `CycleMode` remembers a tab's own mode above. `Confirm`
            // dispatches the same y/n prompt, answered by a later `confirm_bypass` message.
            match cycle_default_mode(state, view) {
                eitri_core::tab_set::ModeCycle::Changed(_) => {
                    send_tabs(state, &mut out);
                    ok(&mut out);
                }
                eitri_core::tab_set::ModeCycle::Confirm(plan) => {
                    out.send(view, eitri_core::agent_bridge::serialize_confirm_bypass_for_js(&plan));
                    ok(&mut out);
                }
            }
        }
        // An extra view shows one tab: there are no others to close from its page.
        InboundMessage::CloseOthers { .. } if !view.is_base() => refuse(&mut out, ONE_TAB_HERE),
        InboundMessage::CloseOthers { .. } => {
            // `y` to `confirm_close_others`: recomputed fresh rather than
            // trusting the set the prompt was shown with, then `close_tab` -- the same close path
            // `prefix &`'s own `y` runs -- once per tab. Only the base view's tabs: another view's
            // tab is not one of the strip's others.
            let ids = state.tabs.close_others_plan().map_or_else(Vec::new, |(ids, _)| ids);
            let mut first_err = None;
            for id in ids {
                if let Err(why) = close_tab(state, id, &mut out) {
                    first_err.get_or_insert(why);
                }
            }
            match first_err {
                None => ok(&mut out),
                Some(why) => refuse(&mut out, why),
            }
        }
        // The nav-mode mirror: recorded so `main.rs`'s intercept can read it back through
        // `AgentPanelHandle::panel_keys`. Never refused -- an unrecognised `mode` is a parse
        // failure in `parse_inbound_message`, not something that reaches here.
        InboundMessage::PanelKeys { mode, .. } => {
            if let Some(record) = state.page(view) {
                record.nav_mode.set(mode);
            }
            ok(&mut out);
        }
        InboundMessage::NavFallthrough { direction, .. } => {
            match run_nav_fallthrough(state, view, direction, &mut out) {
                Ok(()) => ok(&mut out),
                Err(why) => refuse(&mut out, why),
            }
        }
        // `Ctrl+w h/j/k/l` in the panel's BROWSE -- the move `Ctrl+h/j/k/l` make.
        InboundMessage::PaneNav { direction, .. } => match run_pane_nav(state, view, direction, &mut out) {
            Ok(()) => ok(&mut out),
            Err(why) => refuse(&mut out, why),
        },
        InboundMessage::LoadPage { id } => out.effect(Effect::LoadPage(id)),
        InboundMessage::PageLine { id } => out.effect(Effect::PageLine(id)),
        InboundMessage::ChooseProject { .. } => {
            out.effect(Effect::ChooseProject);
            ok(&mut out);
        }
        // The `?` overlay `prefix ?` opened was closed. With no hook (the companion window, `eitri-mac`) there is
        // nothing to give back, and the message is accepted all the same.
        InboundMessage::KeymapClosed {
            opening, return_keys, ..
        } => {
            run_keymap_closed(opening, return_keys, &mut out);
            ok(&mut out);
        }
        // The page's view of a tab a move takes; only the freeze that asked for it reads it.
        InboundMessage::ViewExported {
            request, view: blob, ..
        } => {
            chat_moves::view_exported(state, view, request, blob, &mut out);
            ok(&mut out);
        }
        // `gx` -- a web link out to the system browser. The page's own check
        // (`nav.ts#webUrl`) is not trusted with what reaches the launcher: `web_url` decides again, and a
        // refusal means a page/Rust mismatch or a page that should not be sending this, so it is logged, without the
        // address: it came from the conversation.
        InboundMessage::OpenUrl { url, .. } => match web_url(&url) {
            Some(url) => {
                out.effect(Effect::OpenExternal(url.to_string()));
                ok(&mut out);
            }
            None => {
                eprintln!("[agent_panel] open_url refused: an address of {} bytes", url.len());
                refuse(&mut out, "not a web link");
            }
        },
        // `y`/`n` to a trust question. Only checked and queued here: no disk is touched, and a `y`
        // starts its session from the tick, once the worker found the disk unchanged (and wrote the
        // record, when one is kept).
        InboundMessage::TrustAnswer {
            nonce,
            fingerprint,
            findings_digest,
            trust,
            ..
        } => {
            let tab = tab_of(target);
            // Past the window's gate (`admission`) when this page may give it; one it refused was
            // asked again there and never reaches this arm.
            let answered = answer_trust(
                state,
                &request_id,
                tab,
                nonce,
                &fingerprint,
                &findings_digest,
                trust,
                view,
            );
            let answered = answered.settled(state, screen);
            out.extend(answered);
        }
        InboundMessage::TrustCancel { nonce, .. } => {
            let tab = tab_of(target);
            let cancelled = cancel_trust_question(state, &request_id, tab, nonce, view).settled(state, screen);
            out.extend(cancelled);
        }
        // `:trust` / `:untrust`: queued for the gate's worker; the notice and this command's
        // `command_result` come from the tick when it is done.
        InboundMessage::TrustCommand { action, agent, .. } => {
            if let Err(why) = trust_command(state, view, &request_id, action, &agent) {
                refuse(&mut out, &why);
            }
        }
        // `y`/`Y` to the band's bypass prompt. The typed-input guard (the 250ms
        // `TYPING_GUARD_MS` rule, cancelling on every route away) is enforced client-side
        // -- this only re-validates the facts (the nonce, the active tab, exactly
        // the delivered cards still pending) via `apply_confirm_bypass`/`TabSet::confirm_bypass`,
        // which is where all of that is actually checked.
        //
        // A tab's `y` approves every card still waiting in it, and the prompt itself may be what a
        // float hides, so it has met the window's card-answer gate (`admission`), as a card answer
        // does: refused from a covered chat, nothing changes, and Shift+Tab asks again (the page has
        // closed the prompt already). A tab's prompt is answered only from the page that shows the
        // tab, checked there before the window is asked.
        InboundMessage::ConfirmBypass { tab, scope, nonce, .. } => match scope.scope(tab) {
            Err(why) => refuse(&mut out, &why),
            Ok(scope) => {
                let before = state.tabs.attention();
                match apply_confirm_bypass(state, scope, nonce, view) {
                    Ok(eitri_core::tab_set::ConfirmOutcome::Entered { .. }) => {
                        send_tabs(state, &mut out);
                        // The cards `confirm_bypass` just approved leave the tray at once, rather
                        // than waiting for a later poll to notice `attention()` dropped.
                        report_attention(state, before, &mut out);
                        ok(&mut out);
                    }
                    Ok(eitri_core::tab_set::ConfirmOutcome::AlreadyBypass) => {
                        send_tabs(state, &mut out);
                        ok(&mut out);
                    }
                    Ok(eitri_core::tab_set::ConfirmOutcome::Reprompt(plan)) => {
                        // A card arrived while the prompt was up -- nothing changed, and the
                        // panel is shown the fresh count under a fresh nonce instead.
                        out.send(view, eitri_core::agent_bridge::serialize_confirm_bypass_for_js(&plan));
                        ok(&mut out);
                    }
                    Err(why) => refuse(&mut out, &why),
                }
            }
        },
        // Remote control's questions: admitted from the base page only, a `yes` past the window's gate.
        InboundMessage::RemoteEnrollAnswer { nonce, yes, grant, .. } => {
            match super::remote_enroll::answer_enroll(state, nonce, yes, grant, std::time::Instant::now()) {
                Ok(()) => ok(&mut out),
                Err(why) => refuse(&mut out, &why),
            }
        }
        InboundMessage::RemoteListenAnswer { nonce, yes, .. } => {
            match super::remote_enroll::answer_listen(state, nonce, yes, std::time::Instant::now()) {
                Ok(()) => ok(&mut out),
                Err(why) => refuse(&mut out, &why),
            }
        }
        // The device list's requests: base page only (admission), carried out by the `rv-` socket off the tick.
        InboundMessage::RemoteDevicesOpen { .. } => {
            match super::remote_enroll::want(state, super::remote_enroll::DeskRequest::List) {
                Ok(()) => ok(&mut out),
                Err(why) => refuse(&mut out, why),
            }
        }
        InboundMessage::RemoteStartEnroll { recipe, .. } => {
            match super::remote_enroll::want(state, super::remote_enroll::DeskRequest::StartEnroll(recipe.mode())) {
                Ok(()) => ok(&mut out),
                Err(why) => refuse(&mut out, why),
            }
        }
        InboundMessage::RemoteRevokeAsk { device, .. } => {
            if device.is_empty() || device.len() > super::remote_enroll::REMOTE_FIELD_MAX_BYTES {
                refuse(&mut out, "no such device");
            } else {
                match super::remote_enroll::want(state, super::remote_enroll::DeskRequest::RevokeAsk(device)) {
                    Ok(()) => ok(&mut out),
                    Err(why) => refuse(&mut out, why),
                }
            }
        }
        // A revoke's yes: admitted from the base page only, past the window's gate.
        InboundMessage::RemoteRevoke { device, nonce, .. } => {
            match super::remote_enroll::answer_revoke(state, nonce, &device, std::time::Instant::now()) {
                Ok(()) => ok(&mut out),
                Err(why) => refuse(&mut out, &why),
            }
        }
    }
    out
}

/// A command arriving with no session. Benign: the frontend's start screen is showing, or the
/// session just ended -- either way the panel is healthy and there is nothing to tear down.
pub(super) fn no_session_error() -> eitri_core::agent_backend::BackendError {
    eitri_core::agent_backend::BackendError {
        message: "no active session".to_string(),
        benign: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_panel::tests::*;

    #[test]
    fn the_message_calls_the_hook_and_is_admitted_as_keymap_closed_is() {
        let (handle, _page) = handle_with_strict_page();
        let count = Rc::new(std::cell::Cell::new(0));
        let counted = count.clone();
        let state_for_hook = handle.state.clone();
        handle.on_choose_project(move || {
            let _state = state_for_hook.borrow_mut();
            counted.set(counted.get() + 1);
        });
        let choose = parse_inbound_message(r#"{"type":"choose_project","request_id":"p1"}"#).unwrap();
        let closed =
            parse_inbound_message(r#"{"type":"keymap_closed","request_id":"k1","opening":1,"return_keys":true}"#)
                .unwrap();
        {
            let state = handle.state.borrow();
            assert_eq!(
                admission(&state, &choose, ViewId::BASE, None),
                admission(&state, &closed, ViewId::BASE, None)
            );
            assert_eq!(admission(&state, &choose, ViewId::BASE, None), Admission::Ungated);
        }
        handle.page_message(ViewId::BASE, r#"{"type":"choose_project","request_id":"p1"}"#);
        assert_eq!(count.get(), 1);
    }

    fn message(kind: &str, request_id: &str, tab: TabId, text: &str) -> String {
        serde_json::json!({ "type": kind, "request_id": request_id, "tab": tab.0, "text": text }).to_string()
    }

    /// The page's one door to a command, and the same words through the others.
    #[test]
    fn only_run_shell_runs_a_line_and_the_same_words_elsewhere_are_words() {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("panel-shell-door");
        let (handle, page, _second) = handle_with_two_pages();
        handle.state.borrow_mut().project_dir = dir.clone();
        let (tab, provider) = live_tab(&handle, &dir);
        let marker = dir.join("ran");
        let line = format!("!touch {}", marker.display());
        handle.tick();

        handle.page_message(ViewId::BASE, &message("send_message", "s1", tab, &line));
        tick_handle_until(&handle, "the message", || provider.turns().len() == 1);
        assert_eq!(provider.turns(), [line.clone()], "it went to the model as text");
        assert!(!handle.state.borrow().tabs.shell_line_runs(tab));
        provider.queue(agent::AgentDomainEvent::TurnStarted { turn_id: "t1".into() });
        tick_handle_until(&handle, "the turn", || {
            handle.state.borrow().tabs.get(tab).unwrap().turn_running()
        });
        handle.page_message(ViewId::BASE, &message("queue_message", "q1", tab, &line));
        handle.tick();
        assert!(!handle.state.borrow().tabs.shell_line_runs(tab));
        assert!(!marker.exists());
        provider.queue(agent::AgentDomainEvent::TurnCompleted {
            turn_id: "t1".into(),
            outcome: agent::TurnOutcome::Completed,
            result_text: String::new(),
            stop_reason: None,
            usage: None,
            detail: Default::default(),
        });
        tick_handle_until(&handle, "the queue", || provider.turns().len() == 2);
        assert_eq!(provider.turns()[1], line);
        provider.queue(agent::AgentDomainEvent::TurnStarted { turn_id: "t2".into() });
        provider.queue(agent::AgentDomainEvent::TurnCompleted {
            turn_id: "t2".into(),
            outcome: agent::TurnOutcome::Completed,
            result_text: String::new(),
            stop_reason: None,
            usage: None,
            detail: Default::default(),
        });
        tick_handle_until(&handle, "the turn's end", || {
            !handle.state.borrow().tabs.get(tab).unwrap().turn_running()
        });
        assert!(!marker.exists());

        handle.page_message(ViewId::BASE, &message("run_shell", "r1", tab, &line));
        assert_eq!(result_for(&page.sent.borrow(), "r1"), Some(Ok(())));
        tick_handle_until(&handle, "the line's report", || provider.turns().len() == 3);
        assert!(marker.exists());
        assert!(provider.turns()[2].starts_with("<bash-input>touch "));
        let shown: Vec<serde_json::Value> = page.sent.borrow().iter().map(|p| json_of(p)).collect();
        assert!(shown
            .iter()
            .any(|v| v["kind"] == "shell_running" && v["running"] == true));
        assert!(shown
            .iter()
            .any(|v| v["kind"] == "shell_running" && v["running"] == false));
        shut_down_every_tab(&handle);
    }

    #[test]
    fn stopping_a_line_answers_the_interrupt_and_puts_it_back_in_the_box() {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("panel-shell-stop");
        let (handle, page, _second) = handle_with_two_pages();
        handle.state.borrow_mut().project_dir = dir.clone();
        let (tab, provider) = live_tab(&handle, &dir);
        handle.tick();
        handle.page_message(ViewId::BASE, &message("run_shell", "r1", tab, "!sleep 20"));
        assert!(handle.state.borrow().tabs.shell_line_runs(tab));
        let interrupt = serde_json::json!({ "type": "interrupt", "request_id": "i1", "tab": tab.0 }).to_string();
        handle.page_message(ViewId::BASE, &interrupt);
        assert_eq!(result_for(&page.sent.borrow(), "i1"), Some(Ok(())));
        tick_handle_until(&handle, "the line back in the box", || {
            page.sent
                .borrow()
                .iter()
                .map(|p| json_of(p))
                .any(|v| v["kind"] == "queue_taken" && v["texts"] == serde_json::json!(["!sleep 20"]))
        });
        assert!(provider.turns().is_empty());
        assert_eq!(provider.interrupts(), 0);
        shut_down_every_tab(&handle);
    }

    #[test]
    fn a_bang_with_no_command_is_refused() {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("panel-shell-other");
        let (handle, page, _second) = handle_with_two_pages();
        let (tab, _provider) = live_tab(&handle, &dir);
        handle.tick();
        handle.page_message(ViewId::BASE, &message("run_shell", "r1", tab, "!"));
        assert!(matches!(result_for(&page.sent.borrow(), "r1"), Some(Err(_))));
        assert!(!handle.state.borrow().tabs.shell_line_runs(tab));
        shut_down_every_tab(&handle);
    }

    /// A permission answered after a switch reaches the tab
    /// that asked, not the one on screen.
    #[test]
    fn a_permission_answer_names_its_own_tab_not_the_active_one() {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("panel-permission-tab");
        let mut set = TabSet::new(BackendKind::Sidecar, eitri_core::agent_bridge::SessionModeChoice::Auto);
        let asking = set.active();
        let (asking_provider, backend) = live_backend(&dir);
        set.get_mut(asking).unwrap().set_backend(TabBackend::Live(backend));
        let other = set.open(eitri_core::tab_set::AgentId::Claude);
        let (other_provider, backend) = live_backend(&dir);
        set.get_mut(other).unwrap().set_backend(TabBackend::Live(backend));
        assert_eq!(set.active(), other, "the user has switched away");
        // `perm-1` must really be pending on the asking tab: a conversation refuses an answer to a
        // permission it never asked for. A `Write` needs a human, so the pump leaves it a card --
        // `.git/main.rs` rather than plain `main.rs`:
        // an ordinary in-project `Write` no longer cards at all under the acceptEdits fast path.
        asking_provider.queue(agent::AgentDomainEvent::PermissionRequested {
            permission_id: "perm-1".into(),
            tool_use_id: None,
            tool_name: "Write".into(),
            input: serde_json::json!({ "file_path": ".git/main.rs", "content": "" }),
            provider_prompt: None,
        });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while set.get(asking).unwrap().attention.attention().pending != 1 {
            assert!(std::time::Instant::now() < deadline, "timed out waiting for the card");
            set.pump(&dir, true);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        let message = parse_inbound_message(&format!(
            r#"{{"type":"permission_response","request_id":"r","tab":{},"permission_id":"perm-1","decision":"allow"}}"#,
            asking.0
        ))
        .unwrap();
        let tab = set.resolve(message.tab_ref()).unwrap().unwrap();
        // The production route itself (`InboundMessage::PermissionResponse`'s whole body).
        let outcome = answer_permission_response(
            &mut set,
            None,
            &dir,
            tab,
            ViewId::BASE,
            PermissionAnswer::of(message).unwrap(),
        );
        assert!(
            matches!(outcome, Ok(Ok(_))),
            "{:?}",
            outcome.map(|r| r.map_err(|e| e.message))
        );
        assert_eq!(asking_provider.resolutions(), vec![("perm-1".to_string(), true)]);
        assert!(other_provider.resolutions().is_empty(), "never the active tab");
        for tab in set.take_all() {
            if let Some(retired) = tab.retired {
                retired.into_backend().shutdown();
            }
        }
    }

    /// The panel's own `permission_response` route records the human's approval, so the
    /// CLI's own prompt for the same call that follows it is answered without a second card. Were the route to answer the backend directly, this would be a second card.
    #[test]
    fn the_panels_approve_route_lets_the_clis_own_prompt_for_that_call_through() {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("panel-o3-approve-route");
        let mut set = TabSet::new(BackendKind::Sidecar, eitri_core::agent_bridge::SessionModeChoice::Auto);
        let tab = set.active();
        let (provider, backend) = live_backend(&dir);
        set.get_mut(tab).unwrap().set_backend(TabBackend::Live(backend));
        let input = serde_json::json!({ "file_path": ".git/probe2", "content": "o3" });
        provider.queue(agent::AgentDomainEvent::PermissionRequested {
            permission_id: "perm-hook".into(),
            tool_use_id: Some("toolu_route".into()),
            tool_name: "Write".into(),
            input: input.clone(),
            provider_prompt: None,
        });
        let pump_until = |set: &mut TabSet, what: &str, done: &dyn Fn(&TabSet) -> bool| {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while !done(set) {
                assert!(std::time::Instant::now() < deadline, "timed out waiting for {what}");
                set.pump(&dir, true);
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        };
        pump_until(&mut set, "the gate's card", &|set| {
            set.get(tab).unwrap().attention.attention().pending == 1
        });

        let message = parse_inbound_message(&format!(
            r#"{{"type":"permission_response","request_id":"r","tab":{},"permission_id":"perm-hook","decision":"allow"}}"#,
            tab.0
        ))
        .unwrap();
        let tab = set.resolve(message.tab_ref()).unwrap().unwrap();
        let outcome = answer_permission_response(
            &mut set,
            None,
            &dir,
            tab,
            ViewId::BASE,
            PermissionAnswer::of(message).unwrap(),
        );
        assert!(matches!(outcome, Ok(Ok(_))));

        provider.queue(agent::AgentDomainEvent::PermissionResolved {
            permission_id: "perm-hook".into(),
            outcome: agent::PermissionOutcome::Allowed,
        });
        provider.queue(agent::AgentDomainEvent::PermissionRequested {
            permission_id: "perm-cli".into(),
            tool_use_id: Some("toolu_route".into()),
            tool_name: "Write".into(),
            input,
            provider_prompt: Some(agent::ProviderPrompt {
                reason: Some("Claude requested permissions to edit .git/probe2 which is a sensitive file.".into()),
                ..Default::default()
            }),
        });
        let answered = |_: &TabSet| provider.resolutions().len() == 2;
        pump_until(&mut set, "the CLI's own prompt answered", &answered);
        assert_eq!(provider.resolutions()[1], ("perm-cli".to_string(), true));
        assert_eq!(
            set.get(tab).unwrap().attention.attention().arrived,
            1,
            "one card for the call"
        );
        for tab in set.take_all() {
            if let Some(retired) = tab.retired {
                retired.into_backend().shutdown();
            }
        }
    }

    #[test]
    fn a_command_to_a_tab_with_no_session_is_a_benign_refusal() {
        let mut set = TabSet::new(BackendKind::Sidecar, eitri_core::agent_bridge::SessionModeChoice::Auto);
        let tab = set.active();
        let error = backend_for(&mut set, tab).err().expect("an empty tab has no backend");
        assert!(error.benign);
        assert_eq!(error.message, "no active session");
    }

    /// A command that fails fatally takes the session away while a message is still in its turn: the
    /// message is held again with the reason, and `↑` takes it back rather than leaving it shown as sent.
    #[test]
    fn an_interrupt_that_fails_fatally_gives_back_the_steered_message() {
        use eitri_core::test_providers::{empty_trust_plan, ExternalRecordingProvider};
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("panel-fatal-interrupt-steered");
        let (handle, _page, _host) = handle_with_recording_page();
        let provider = std::sync::Arc::new(ExternalRecordingProvider::steering_codex());
        let codex = {
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
            codex
        };
        handle.page_message(ViewId::BASE, r#"{"type":"ready","request_id":"r1"}"#);
        provider.inner.queue(agent::AgentDomainEvent::TurnStarted {
            turn_id: "turn-1".into(),
        });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !handle.state.borrow().tabs.get(codex).unwrap().turn_running() {
            assert!(std::time::Instant::now() < deadline, "the turn never started");
            handle.tick();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        handle.page_message(
            ViewId::BASE,
            &serde_json::json!({
                "type": "queue_message",
                "request_id": "q1",
                "tab": codex.0,
                "text": "in the turn",
                "turn_id": "turn-1",
            })
            .to_string(),
        );
        assert_eq!(provider.steers().len(), 1, "the message went into the turn");

        provider.inner.fail_interrupts(true);
        handle.page_message(
            ViewId::BASE,
            &serde_json::json!({"type": "interrupt", "request_id": "i1", "tab": codex.0}).to_string(),
        );
        assert_eq!(provider.inner.interrupts(), 1, "the interrupt reached the agent");
        assert!(matches!(
            handle.state.borrow().tabs.get(codex).unwrap().backend(),
            TabBackend::Failed { .. }
        ));
        // Read before the take-back, which clears it.
        assert_eq!(
            handle
                .state
                .borrow()
                .tabs
                .get(codex)
                .unwrap()
                .session
                .queue_note
                .as_deref(),
            Some("held until the turn ends: the session was closed before the agent read it")
        );
        assert_eq!(
            handle.state.borrow_mut().tabs.take_back_queue(codex),
            ["in the turn"],
            "the message comes back to the box"
        );
    }

    #[test]
    fn task_commands_reach_the_provider_and_the_output_reply_returns_to_the_requesting_page() {
        let dir = agent::state_dirs::test_workspace_dir("panel-task-output");
        let (handle, page, _) = handle_with_recording_page();
        let provider = std::sync::Arc::new(RecordingProvider::task_controlling());
        let conversation = agent::AgentConversation::create(provider.clone(), &dir, ProjectTrust::Untrusted).unwrap();
        let tab = handle.state.borrow().tabs.active();
        handle
            .state
            .borrow_mut()
            .tabs
            .get_mut(tab)
            .unwrap()
            .set_backend(TabBackend::Live(AgentBackend::Conversation {
                agent: AgentId::Claude,
                conversation: Box::new(conversation),
            }));
        provider.queue(agent::AgentDomainEvent::BackgroundTasksChanged {
            tasks: vec![agent::BackgroundTask {
                task_id: "task".into(),
                run_id: None,
                kind: agent::TaskKind::Shell,
                task_type: "shell".into(),
                description: "test".into(),
                subagent_type: None,
                parent_task_id: None,
                tool_use_id: None,
                ambient: false,
                workflow_name: None,
            }],
            occurred_at: None,
        });
        handle.page_message(ViewId::BASE, r#"{"type":"ready","request_id":"ready"}"#);
        tick_handle_until(&handle, "the roster", || {
            handle
                .state
                .borrow()
                .tabs
                .get(tab)
                .unwrap()
                .live()
                .unwrap()
                .projection()
                .background_calls
                == 1
        });
        for (kind, request) in [("stop_task", "s1"), ("task_output", "o1")] {
            handle.page_message(
                ViewId::BASE,
                &serde_json::json!({"type": kind, "request_id": request, "tab": tab.0, "task_id": "task"}).to_string(),
            );
        }
        assert!(crate::agent_panel::tick::needs_polling(&handle.state.borrow()));
        tick_handle_until(&handle, "the task replies", || {
            page.sent.borrow().iter().any(|payload| {
                let value: serde_json::Value = serde_json::from_str(payload).unwrap();
                value["kind"] == "task_output_result" && value["requestId"] == "o1"
            })
        });
        assert_eq!(provider.stopped_tasks(), ["task"]);
        assert_eq!(result_for(&page.sent.borrow(), "s1"), Some(Ok(())));
        let output: serde_json::Value = page
            .sent
            .borrow()
            .iter()
            .map(|payload| serde_json::from_str::<serde_json::Value>(payload).unwrap())
            .find(|value| value["kind"] == "task_output_result")
            .unwrap();
        assert_eq!(output["tab"], tab.0);
        assert_eq!(output["taskId"], "task");
        assert_eq!(output["output"], "<b>plain</b>\u{202e}");
        assert_eq!(output["totalBytes"], 10000);
        assert_eq!(output["truncated"], true);
        assert!(handle.state.borrow().task_commands.is_empty());
        shut_down_every_tab(&handle);
    }

    #[test]
    fn the_direct_settings_reach_the_provider_and_answer_the_page_that_asked() {
        let dir = agent::state_dirs::test_workspace_dir("panel-settings");
        let (handle, page, _) = handle_with_recording_page();
        let provider = std::sync::Arc::new(RecordingProvider::task_controlling());
        let conversation = agent::AgentConversation::create(provider.clone(), &dir, ProjectTrust::Untrusted).unwrap();
        let tab = handle.state.borrow().tabs.active();
        handle
            .state
            .borrow_mut()
            .tabs
            .get_mut(tab)
            .unwrap()
            .set_backend(TabBackend::Live(AgentBackend::Conversation {
                agent: AgentId::Claude,
                conversation: Box::new(conversation),
            }));
        handle.page_message(ViewId::BASE, r#"{"type":"ready","request_id":"ready"}"#);
        for message in [
            serde_json::json!({"type": "list_models", "request_id": "l1", "tab": tab.0}),
            serde_json::json!({"type": "set_model", "request_id": "m1", "tab": tab.0, "model": "sonnet"}),
            serde_json::json!({"type": "apply_effort", "request_id": "e1", "tab": tab.0, "effort": "low"}),
        ] {
            handle.page_message(ViewId::BASE, &message.to_string());
        }
        let found = |request: &str| -> Option<serde_json::Value> {
            page.sent
                .borrow()
                .iter()
                .map(|payload| serde_json::from_str::<serde_json::Value>(payload).unwrap())
                .find(|value| value["requestId"] == request)
        };
        tick_handle_until(&handle, "the replies", || {
            found("l1").is_some() && found("m1").is_some() && found("e1").is_some()
        });
        assert_eq!(provider.settings_calls(), ["model sonnet", "effort low"]);
        let list = found("l1").unwrap();
        assert_eq!(list["kind"], "models_result");
        assert_eq!(list["models"][0]["value"], "sonnet");
        assert_eq!(list["applied"]["model"], "claude-opus-5-5");
        let model = found("m1").unwrap();
        assert_eq!(model["kind"], "settings_result");
        assert_eq!(model["applied"]["model"], "resolved-sonnet");
        assert_eq!(found("e1").unwrap()["applied"]["effort"], "low");
        assert!(handle.state.borrow().task_commands.is_empty());
        shut_down_every_tab(&handle);
    }

    /// `o` on a long output: a read with an offset is a range, not the tail.
    #[test]
    fn a_read_with_an_offset_asks_for_that_range() {
        let dir = agent::state_dirs::test_workspace_dir("panel-output-range");
        let (handle, page, _) = handle_with_recording_page();
        let provider = std::sync::Arc::new(RecordingProvider::task_controlling());
        let conversation = agent::AgentConversation::create(provider.clone(), &dir, ProjectTrust::Untrusted).unwrap();
        let tab = handle.state.borrow().tabs.active();
        handle
            .state
            .borrow_mut()
            .tabs
            .get_mut(tab)
            .unwrap()
            .set_backend(TabBackend::Live(AgentBackend::Conversation {
                agent: AgentId::Claude,
                conversation: Box::new(conversation),
            }));
        provider.queue(agent::AgentDomainEvent::BackgroundTasksChanged {
            tasks: vec![agent::BackgroundTask {
                task_id: "task".into(),
                run_id: None,
                kind: agent::TaskKind::Shell,
                task_type: "shell".into(),
                description: "test".into(),
                subagent_type: None,
                parent_task_id: None,
                tool_use_id: None,
                ambient: false,
                workflow_name: None,
            }],
            occurred_at: None,
        });
        handle.page_message(ViewId::BASE, r#"{"type":"ready","request_id":"ready"}"#);
        tick_handle_until(&handle, "the roster", || {
            handle
                .state
                .borrow()
                .tabs
                .get(tab)
                .unwrap()
                .live()
                .unwrap()
                .projection()
                .background_calls
                == 1
        });
        handle.page_message(
            ViewId::BASE,
            &serde_json::json!({"type": "task_output", "request_id": "o1", "tab": tab.0, "task_id": "task", "offset": 7})
                .to_string(),
        );
        tick_handle_until(&handle, "the reply", || {
            page.sent
                .borrow()
                .iter()
                .any(|payload| payload.contains("\"the whole\""))
        });
        assert_eq!(provider.output_ranges(), [(7, TASK_OUTPUT_VIEW_BYTES)]);
        shut_down_every_tab(&handle);
    }

    /// The confirmed `<leader>k` reaches the conversation of the tab it names, never the one on screen.
    /// One that names a closed tab, or none, is refused; a refusal because of when it was asked is said
    /// and leaves the tab live; a taken stop ends the session through the pump, as a lost one that can
    /// be resumed. One `r` on it resumes it in the same tab, and the start waits until the old backend has
    /// let go of the session.
    #[test]
    fn a_stop_of_the_background_reaches_the_tab_it_names_and_ends_its_session() {
        use eitri_core::test_providers::{empty_trust_plan, ExternalRecordingProvider};
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("panel-stop-background");
        let (handle, page, _host) = handle_with_recording_page();
        page.visible.set(true);
        let provider = std::sync::Arc::new(ExternalRecordingProvider::codex());
        let (codex, on_screen) = {
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
            let on_screen = tabs.tabs().iter().find(|t| t.id != codex).unwrap().id;
            tabs.select(on_screen);
            (codex, on_screen)
        };
        provider
            .inner
            .queue(agent::AgentDomainEvent::BackgroundCalls { running: 1 });
        let count = || {
            handle
                .state
                .borrow()
                .tabs
                .get(codex)
                .and_then(|t| t.live())
                .map(|b| b.projection().background_calls)
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while count() != Some(1) {
            assert!(std::time::Instant::now() < deadline, "the count never arrived");
            handle.tick();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let result = |request: &str| -> Option<Result<(), String>> {
            page.sent.borrow().iter().find_map(|envelope| {
                let v: serde_json::Value = serde_json::from_str(envelope).unwrap();
                (v["kind"] == "command_result" && v["requestId"] == request).then(|| {
                    if v["ok"] == true {
                        Ok(())
                    } else {
                        Err(v["error"].as_str().unwrap_or("").to_string())
                    }
                })
            })
        };
        let stop = |request: &str, tab: Option<TabId>| {
            let tab = tab.map_or(String::new(), |t| format!(r#","tab":{}"#, t.0));
            handle.page_message(
                ViewId::BASE,
                &format!(r#"{{"type":"stop_background","request_id":"{request}"{tab},"running":1}}"#),
            );
        };

        stop("s0", Some(TabId(9_999)));
        assert_eq!(result("s0"), Some(Err("protocol: no tab 9999".to_string())));
        stop("s1", None);
        assert_eq!(
            result("s1"),
            Some(Err("protocol: this command names no tab".to_string()))
        );
        stop("s2", Some(on_screen));
        assert_eq!(result("s2"), Some(Err("no active session".to_string())));
        provider.refuse_next_stop(agent::ProviderErrorCode::NoActiveTurn);
        stop("s3", Some(codex));
        assert!(matches!(result("s3"), Some(Err(_))), "{:?}", result("s3"));
        assert!(
            handle.state.borrow().tabs.get(codex).unwrap().live().is_some(),
            "a refusal leaves the session alone"
        );
        assert_eq!(provider.stops(), 0);

        stop("s4", Some(codex));
        assert_eq!(result("s4"), Some(Ok(())));
        assert_eq!(provider.stops(), 1, "the tab it named, not the one on screen");
        let ended = || handle.state.borrow().tabs.get(codex).unwrap().wire_state().as_str() == "ended";
        while !ended() {
            assert!(std::time::Instant::now() < deadline, "the session never ended");
            handle.tick();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(count(), Some(0));
        assert!(handle.state.borrow().tabs.get(on_screen).is_some());

        // The row says `r` resumes, and one `r` does.
        let wire = |tab: TabId| {
            let tabs: serde_json::Value = serde_json::from_str(&handle.state.borrow().tabs.tabs_payload()).unwrap();
            tabs["tabs"]
                .as_array()
                .unwrap()
                .iter()
                .find(|t| t["id"] == tab.0)
                .unwrap()
                .clone()
        };
        assert_eq!(wire(codex)["resumesOnR"], true);
        assert_eq!(wire(on_screen)["resumesOnR"], false);
        let release = provider.hold_close();
        handle.page_message(
            ViewId::BASE,
            &format!(r#"{{"type":"reset_tab","request_id":"r1","tab":{}}}"#, codex.0),
        );
        {
            let state_ref = handle.state.borrow();
            let tab = state_ref.tabs.get(codex).unwrap();
            assert!(
                matches!(
                    tab.backend(),
                    TabBackend::AwaitingTrust(AwaitingTrust {
                        start: DeferredStart::Resume { provider_session_id, .. },
                        ..
                    }) if provider_session_id == "thread-1"
                ),
                "the session is resumed in place, not left for a start screen"
            );
            assert_eq!(
                state_ref.tabs.active(),
                on_screen,
                "a reset never moves the window to its tab"
            );
        }
        assert_eq!(result("r1"), None, "answered once the resume connects");
        assert_eq!(wire(codex)["resumesOnR"], false);
        // Its old backend keeps the lease until it is dropped; the start must not begin before. (This panel's
        // gate has no worker, so the start itself is put off on the next tick: the wait is read here.)
        let (session, gone) = handle
            .state
            .borrow_mut()
            .resume_after
            .remove(&codex)
            .expect("the start waits for the old backend");
        assert_eq!(session, "thread-1");
        for _ in 0..5 {
            handle.tick();
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(
            gone.try_recv(),
            Err(mpsc::TryRecvError::Empty),
            "the old backend is still closing"
        );
        release.send(()).unwrap();
        assert_eq!(gone.recv_timeout(std::time::Duration::from_secs(10)), Ok(()));
        assert_eq!(provider.inner.closes(), 1);
    }

    /// `r` on an ended tab whose agent did not stop it itself still empties it to a start screen.
    #[test]
    fn r_on_a_session_that_simply_ended_starts_over() {
        use eitri_core::test_providers::{empty_trust_plan, ExternalRecordingProvider};
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("panel-r-starts-over");
        let (handle, page, _host) = handle_with_recording_page();
        page.visible.set(true);
        let provider = std::sync::Arc::new(ExternalRecordingProvider::codex());
        let codex = {
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
            tabs.select(codex);
            codex
        };
        provider.inner.queue(agent::AgentDomainEvent::SessionUnavailable {
            reason: "the stream ended".into(),
        });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while handle.state.borrow().tabs.get(codex).unwrap().wire_state().as_str() != "ended" {
            assert!(std::time::Instant::now() < deadline, "the session never ended");
            handle.tick();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(handle.state.borrow().tabs.resumes_on_r(codex), None);
        handle.page_message(
            ViewId::BASE,
            &format!(r#"{{"type":"reset_tab","request_id":"r1","tab":{}}}"#, codex.0),
        );
        assert!(matches!(
            handle.state.borrow().tabs.get(codex).unwrap().backend(),
            TabBackend::NotStarted
        ));
        assert!(handle.state.borrow().resume_after.is_empty());
    }

    /// The rule comes from what Rust offered for that card, never from the panel, and only
    /// with an allow.
    #[test]
    fn remember_saves_only_the_rule_rust_offered_and_only_with_an_allow() {
        use eitri_core::agent_bridge::DecisionChoice;
        let mut set = TabSet::new(BackendKind::Sidecar, eitri_core::agent_bridge::SessionModeChoice::Auto);
        let tab = set.active();
        set.get_mut(tab)
            .unwrap()
            .session
            .rule_offers
            .insert("perm-1".into(), agent::PrefixRule::parse("Bash(git push *)").unwrap());
        let t = set.get(tab).unwrap();
        assert_eq!(
            rule_to_remember(t, "perm-1", DecisionChoice::Allow, false, true),
            Ok(None),
            "a plain allow"
        );
        assert_eq!(
            rule_to_remember(t, "perm-1", DecisionChoice::Allow, true, true)
                .unwrap()
                .unwrap()
                .display(),
            "git push *"
        );
        assert!(
            rule_to_remember(t, "perm-2", DecisionChoice::Allow, true, true).is_err(),
            "never offered"
        );
        assert!(
            rule_to_remember(t, "perm-1", DecisionChoice::Deny, true, true).is_err(),
            "remember is an allow"
        );
    }

    /// A request that is no longer pending -- because it was
    /// already answered -- saves no rule, even though the offer Rust made for it may still be
    /// sitting in `rule_offers` (the offer is only recomputed once per pump tick).
    #[test]
    fn a_no_longer_pending_request_saves_no_rule_even_if_still_offered() {
        use eitri_core::agent_bridge::DecisionChoice;
        let mut set = TabSet::new(BackendKind::Sidecar, eitri_core::agent_bridge::SessionModeChoice::Auto);
        let tab = set.active();
        set.get_mut(tab)
            .unwrap()
            .session
            .rule_offers
            .insert("perm-1".into(), agent::PrefixRule::parse("Bash(git push *)").unwrap());
        let t = set.get(tab).unwrap();
        assert!(
            rule_to_remember(t, "perm-1", DecisionChoice::Allow, true, false).is_err(),
            "not pending any more, whatever the offer still says"
        );
    }

    /// The sidecar's own longer window, through the production route
    /// (`answer_permission_response`, the whole `permission_response` arm): `respond_permission`'s
    /// own resolution is not folded into the projection until the provider's real event arrives
    /// (`AgentConversation::respond_permission`'s own doc), so a projection read alone cannot catch
    /// a second response landing before that. The first successful answer, whatever its decision,
    /// drops the offer and marks the card answered (`TabSet::answer_card`), and the next pump, which recomputes offers because another id arrived, does
    /// not bring it back.
    #[test]
    fn an_answered_cards_offer_never_comes_back_and_a_replayed_remember_saves_nothing() {
        agent::state_dirs::redirect_state_to_a_test_root();
        // Its own repository: the `git push` cards below are offered a rule only when the policy finds
        // no repository above the project, and the shared temp dir above a bare workspace may hold a
        // stray `.git`.
        let dir = agent::state_dirs::test_repository_dir("panel-permission-remember-race");
        let rules_dir = dir.join("rules");
        let mut set = TabSet::new(BackendKind::Sidecar, eitri_core::agent_bridge::SessionModeChoice::Auto);
        let tab = set.active();
        let (provider, backend) = live_backend(&dir);
        set.get_mut(tab).unwrap().set_backend(TabBackend::Live(backend));
        let pump_until = |set: &mut TabSet, what: &str, done: &dyn Fn(&TabSet) -> bool| {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while !done(set) {
                assert!(std::time::Instant::now() < deadline, "timed out waiting for {what}");
                set.pump(&dir, true);
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        };
        let bash = |id: &str, command: &str| agent::AgentDomainEvent::PermissionRequested {
            permission_id: id.into(),
            tool_use_id: None,
            tool_name: "Bash".into(),
            input: serde_json::json!({ "command": command }),
            provider_prompt: None,
        };
        let response = |id: &str, decision: &str, remember: bool| {
            let message = parse_inbound_message(&format!(
                r#"{{"type":"permission_response","request_id":"r","tab":{},"permission_id":"{id}","decision":"{decision}","remember":{remember}}}"#,
                tab.0
            ))
            .unwrap();
            PermissionAnswer::of(message).unwrap()
        };
        provider.queue(bash("perm-1", "git push origin main"));
        pump_until(&mut set, "the offer", &|set| {
            !set.get(tab).unwrap().session.rule_offers.is_empty()
        });
        assert_eq!(
            set.get(tab).unwrap().session.rule_offers["perm-1"].display(),
            "git push *"
        );
        assert!(set.get(tab).unwrap().card_is_waiting("perm-1"));

        // The first response: a deny, never remembered, through the production route.
        let outcome = answer_permission_response(
            &mut set,
            Some(&rules_dir),
            &dir,
            tab,
            ViewId::BASE,
            response("perm-1", "deny", false),
        );
        assert!(matches!(outcome, Ok(Ok(_))));
        assert_eq!(provider.resolutions(), vec![("perm-1".to_string(), false)]);
        assert!(
            !set.get(tab).unwrap().session.rule_offers.contains_key("perm-1"),
            "the route itself drops the offer"
        );

        // A parallel call's card arrives before the provider's `PermissionResolved` for perm-1, so
        // the pump recomputes the offers from a projection that still lists perm-1 as pending.
        provider.queue(bash("perm-2", "git push origin other"));
        pump_until(&mut set, "the second card's offer", &|set| {
            set.get(tab).unwrap().session.rule_offers.contains_key("perm-2")
        });
        assert!(
            set.get(tab)
                .unwrap()
                .live()
                .unwrap()
                .projection()
                .pending_permissions
                .contains_key("perm-1"),
            "the premise: the sidecar has not folded perm-1's resolution yet"
        );
        let t = set.get(tab).unwrap();
        assert!(
            !t.session.rule_offers.contains_key("perm-1"),
            "the recompute must not bring back an answered card's offer"
        );
        assert!(!t.card_is_waiting("perm-1"), "answered, whatever the projection says");
        assert!(t.card_is_waiting("perm-2"));

        // A replayed "Always allow" for perm-1 (a tab switch or a reload re-drew the card): refused,
        // and no rule reaches the file.
        let replay = answer_permission_response(
            &mut set,
            Some(&rules_dir),
            &dir,
            tab,
            ViewId::BASE,
            response("perm-1", "allow", true),
        );
        assert_eq!(replay.map(|_| ()), Err("this request was already answered".to_string()));
        assert_eq!(
            eitri_core::permission_store::load(&rules_dir, &dir),
            eitri_core::permission_store::LoadedRules::Missing,
            "no rule was saved"
        );
        assert_eq!(provider.resolutions().len(), 1, "and nothing more reached the provider");
        for tab in set.take_all() {
            if let Some(retired) = tab.retired {
                retired.into_backend().shutdown();
            }
        }
    }

    /// A failed tab whose `claude` is still missing stays as it is on `r`: the answer is a refusal that says so, and no
    /// session starts. Once one is there, the same `r` starts the tab over. The lookup runs on a worker, so the page
    /// is answered by the tick, not by the message.
    #[test]
    fn r_on_a_not_found_tab_starts_over_only_once_claude_is_there() {
        use std::sync::atomic::{AtomicU8, Ordering};
        use std::sync::Arc;
        let (handle, page, _host) = handle_with_recording_page();
        page.visible.set(true);
        let missing = Arc::new(AtomicU8::new(1));
        let tab = {
            let mut state = handle.state.borrow_mut();
            let flag = missing.clone();
            // 1: nothing found, 2: one found and refused, 0: found.
            state.cli_missing = Arc::new(move || match flag.load(Ordering::SeqCst) {
                1 => Some(agent::ClaudeProblem::Missing),
                2 => Some(agent::ClaudeProblem::Refused),
                _ => None,
            });
            let tab = state.tabs.active();
            let reason = format!(
                "{} via \"/no/such/claude\" (spawn error: ENOENT)",
                agent::CLI_MISSING_FAILURE_MARKER
            );
            state
                .tabs
                .fail_session(tab, reason, eitri_core::tab_set::FailedHow::StartNeverOpened, true);
            tab
        };
        let answered = |id: &str| {
            page.sent
                .borrow()
                .iter()
                .filter(|payload| payload.contains("command_result") && payload.contains(&format!("\"{id}\"")))
                .cloned()
                .collect::<Vec<_>>()
        };
        let is_failed = || {
            matches!(
                handle.state.borrow().tabs.get(tab).unwrap().backend(),
                TabBackend::Failed { .. }
            )
        };
        let reset = |id: &str| {
            handle.page_message(
                ViewId::BASE,
                &format!(r#"{{"type":"reset_tab","request_id":"{id}","tab":{}}}"#, tab.0),
            );
        };

        reset("r1");
        assert!(
            answered("r1").is_empty(),
            "the lookup is a worker's: nothing is answered in the message"
        );
        assert!(is_failed(), "and nothing is reset while it runs");
        tick_handle_until(&handle, "the refusal", || !answered("r1").is_empty());
        let refusal = answered("r1").remove(0);
        assert!(refusal.contains("still not found"), "{refusal}");
        assert!(is_failed(), "the row stays: the tab is as it was");
        assert!(handle.state.borrow().reset_checks.is_empty());

        // A refused one: the answer says what the row says, found but not used.
        missing.store(2, Ordering::SeqCst);
        reset("r1b");
        tick_handle_until(&handle, "the refusal", || !answered("r1b").is_empty());
        let refusal = answered("r1b").remove(0);
        assert!(refusal.contains("found but not used"), "{refusal}");
        assert!(is_failed(), "the row stays for a refused one too");

        missing.store(0, Ordering::SeqCst);
        reset("r2");
        tick_handle_until(&handle, "the reset", || !answered("r2").is_empty());
        assert!(answered("r2")[0].contains("\"ok\":true"), "{}", answered("r2")[0]);
        assert!(matches!(
            handle.state.borrow().tabs.get(tab).unwrap().backend(),
            TabBackend::NotStarted
        ));
    }

    /// Only the not-found failure waits for the lookup: any other failed tab starts over at once, as before, even when
    /// `claude` is missing.
    #[test]
    fn r_on_any_other_failed_tab_starts_over_without_a_lookup() {
        use std::sync::Arc;
        let (handle, page, _host) = handle_with_recording_page();
        page.visible.set(true);
        let tab = {
            let mut state = handle.state.borrow_mut();
            state.cli_missing = Arc::new(|| panic!("no lookup for a failure that is not a missing claude"));
            let tab = state.tabs.active();
            state.tabs.fail_session(
                tab,
                "the connection to the provider ended before this session did".to_string(),
                eitri_core::tab_set::FailedHow::StartNeverOpened,
                true,
            );
            tab
        };
        handle.page_message(
            ViewId::BASE,
            &format!(r#"{{"type":"reset_tab","request_id":"r1","tab":{}}}"#, tab.0),
        );
        assert!(handle.state.borrow().reset_checks.is_empty());
        assert!(matches!(
            handle.state.borrow().tabs.get(tab).unwrap().backend(),
            TabBackend::NotStarted
        ));
    }

    /// What the chat's stop question needs of a tab: a turn, unseen Claude Code work or background work running. A tab
    /// gone or idle has none.
    #[test]
    fn stop_answer_target_has_work_only_while_a_turn_or_background_work_runs() {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("panel-stop-has-work");
        let (handle, _page, _second) = handle_with_two_pages();
        handle.state.borrow_mut().project_dir = dir.clone();
        let (tab, provider) = live_tab(&handle, &dir);
        handle.tick();
        assert!(!handle.tab_has_work(tab), "an idle tab has nothing to stop");
        assert!(
            !handle.tab_has_work(TabId(9_999)),
            "a tab that is not here has nothing to stop"
        );
        provider.queue(agent::AgentDomainEvent::BackgroundCalls { running: 2 });
        tick_handle_until(&handle, "the background calls", || handle.tab_has_work(tab));
        provider.queue(agent::AgentDomainEvent::BackgroundCalls { running: 0 });
        tick_handle_until(&handle, "the background calls ending", || !handle.tab_has_work(tab));
        provider.queue(agent::AgentDomainEvent::CliWorkUnseen);
        tick_handle_until(&handle, "Claude Code working where the chat cannot show it", || {
            handle.tab_has_work(tab)
        });
        provider.queue(agent::AgentDomainEvent::TurnCompleted {
            turn_id: "t0".into(),
            outcome: agent::TurnOutcome::Completed,
            result_text: String::new(),
            stop_reason: None,
            usage: None,
            detail: Default::default(),
        });
        tick_handle_until(&handle, "the unseen work ending", || !handle.tab_has_work(tab));
        provider.queue(agent::AgentDomainEvent::TurnStarted { turn_id: "t1".into() });
        tick_handle_until(&handle, "the turn", || handle.tab_has_work(tab));
        shut_down_every_tab(&handle);
    }

    /// The stop question is answered with the chat's own stop messages, so it reaches exactly the arms the chat's stop
    /// keys reach; there is no message of its own for a stop.
    #[test]
    fn stop_answer_reaches_only_the_existing_arms() {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("panel-stop-answer");
        let (handle, page, _second) = handle_with_two_pages();
        handle.state.borrow_mut().project_dir = dir.clone();
        let provider = std::sync::Arc::new(eitri_core::test_providers::RecordingProvider::interruptible());
        let tab = {
            let conversation = agent::AgentConversation::create(
                provider.clone(),
                &dir,
                agent::setting_sources::ProjectTrust::Untrusted,
            )
            .unwrap();
            let mut state = handle.state.borrow_mut();
            let tab = state.tabs.open(AgentId::Claude);
            state
                .tabs
                .get_mut(tab)
                .unwrap()
                .set_backend(TabBackend::Live(AgentBackend::Conversation {
                    agent: AgentId::Claude,
                    conversation: Box::new(conversation),
                }));
            tab
        };
        handle.tick();
        for kind in ["stop_answer", "stop", "stop_turn"] {
            let line = serde_json::json!({ "type": kind, "request_id": "s1", "tab": tab.0, "yes": true }).to_string();
            assert!(
                parse_inbound_message(&line).is_none(),
                "{kind} is no message the panel takes"
            );
        }
        let interrupt = parse_inbound_message(
            &serde_json::json!({ "type": "interrupt", "request_id": "i1", "tab": tab.0 }).to_string(),
        );
        assert!(matches!(interrupt, Some(InboundMessage::Interrupt { .. })));
        let stop = parse_inbound_message(
            &serde_json::json!({ "type": "stop_background", "request_id": "b1", "tab": tab.0, "running": 2 })
                .to_string(),
        );
        assert!(matches!(stop, Some(InboundMessage::StopBackground { running: 2, .. })));

        provider.queue(agent::AgentDomainEvent::TurnStarted { turn_id: "t1".into() });
        tick_handle_until(&handle, "the turn", || handle.tab_has_work(tab));
        handle.page_message(
            ViewId::BASE,
            &serde_json::json!({ "type": "interrupt", "request_id": "i1", "tab": tab.0 }).to_string(),
        );
        assert_eq!(result_for(&page.sent.borrow(), "i1"), Some(Ok(())));
        assert_eq!(provider.interrupts(), 1, "the stop reached the agent's interrupt");
        shut_down_every_tab(&handle);
    }
}
