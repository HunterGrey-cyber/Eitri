//! A page's message before its arm runs: its tab resolved, a late draft taken, the message classified
//! (`admission`), and whether the page and the window may take the answer it carries.

use super::*;

/// The window's check on an answer from a page (`AgentPanelHandle::set_answer_gate`), asked with that
/// page's view.
pub(super) type AnswerGate = Rc<dyn Fn(ViewId) -> Result<(), String>>;

/// What a message from a page needs before its arm may act on it ([`admission`]).
#[derive(Debug, PartialEq)]
pub(super) enum Admission {
    /// An answer the window must agree to: its gate is asked with this view, the sending page's, at
    /// the moment of the answer and with the panel's state released.
    Gated(ViewId),
    /// Nothing the window is asked about.
    Ungated,
    /// Refused before the window is asked (an answer from a page that does not show its tab), with
    /// the words the page is told.
    Refused(String),
}

/// Which messages from `view`'s page meet the window's gate: every answer that resolves a card or one
/// of Claude's questions, starts a session the user was asked about, or loosens permissions -- a card
/// answer, a question's answer or decline, a bypass `y`, a restore `y` (it resumes the saved bypass
/// tabs in bypass), a trust `y` or `n` (a covered page's question may not be the one the user
/// read) and turning following on (the consent that lets edited files be loaded in the editor).
/// `target` is the tab the message resolved to. Every
/// variant is named, so a new message is classified when it is added rather than falling through
/// ungated. Changes nothing.
pub(super) fn admission(
    state: &AgentPanelState,
    msg: &InboundMessage,
    view: ViewId,
    target: Option<TabId>,
) -> Admission {
    // An answer whose page may not give it is refused naming the tab, before the window is asked,
    // so it never reaches the window whatever covers it.
    let from_its_page = |tab: TabId| match page_may_answer(&state.tabs, tab, view) {
        Ok(()) => Admission::Gated(view),
        Err(why) => Admission::Refused(why),
    };
    match msg {
        InboundMessage::StopTask { task_id, .. } | InboundMessage::TaskOutput { task_id, .. } => {
            let Some(tab) = target else {
                return Admission::Refused("no active session".into());
            };
            if let Err(why) = page_may_answer(&state.tabs, tab, view) {
                return Admission::Refused(why);
            }
            if state.tabs.is_moving(tab) {
                return Admission::Refused(eitri_core::tab_set::MOVING.into());
            }
            let Some(backend) = state.tabs.get(tab).and_then(|t| t.live()) else {
                return Admission::Refused("no active session".into());
            };
            if backend.agent() != AgentId::Claude {
                return Admission::Refused("task controls are only available for Claude".into());
            }
            if !backend
                .projection()
                .background_tasks
                .rows
                .iter()
                .any(|row| row.task.task_id == *task_id)
            {
                return Admission::Refused("task no longer exists".into());
            }
            Admission::Ungated
        }
        // A change of the model or the effort reaches the running session, also during a turn: only from the
        // page showing the tab, never on a tab that is moving, and only on a Claude or an ACP session (an ACP
        // agent offers what it lists itself). It widens nothing
        // by itself; the mode the CLI moves to arrives on the session's stream.
        InboundMessage::ListModels { .. } | InboundMessage::SetModel { .. } | InboundMessage::ApplyEffort { .. } => {
            let Some(tab) = target else {
                return Admission::Refused("no active session".into());
            };
            if let Err(why) = page_may_answer(&state.tabs, tab, view) {
                return Admission::Refused(why);
            }
            if state.tabs.is_moving(tab) {
                return Admission::Refused(eitri_core::tab_set::MOVING.into());
            }
            let Some(backend) = state.tabs.get(tab).and_then(|t| t.live()) else {
                return Admission::Refused("no active session".into());
            };
            if !matches!(backend.agent(), AgentId::Claude | AgentId::Acp(_)) {
                return Admission::Refused(
                    "the model and the effort are only changed here for Claude and ACP agents".into(),
                );
            }
            Admission::Ungated
        }
        // A question is answered the way a card is: only from the page showing its tab, and only
        // while the window agrees that page is not covered.
        InboundMessage::PermissionResponse { .. }
        | InboundMessage::QuestionAnswer { .. }
        | InboundMessage::QuestionDecline { .. } => match target {
            Some(tab) => from_its_page(tab),
            // Unreachable (an answer names its tab, or it was refused when resolved); asking the
            // window is the side that refuses more.
            None => Admission::Gated(view),
        },
        // A broker card meets the same gates. A tab frozen for a move answers no card: its calls end when it leaves,
        // and one queued again now would run for a window the agent no longer talks to.
        InboundMessage::EditorCardAnswer { .. } => match target {
            Some(tab) => match from_its_page(tab) {
                Admission::Gated(_) if state.tabs.is_moving(tab) => {
                    Admission::Refused(eitri_core::tab_set::MOVING.into())
                }
                admitted => admitted,
            },
            None => Admission::Gated(view),
        },
        // Enabling following permits file loads, so only the tab's uncovered page can give that consent.
        InboundMessage::ToggleFollowing { .. } => match target {
            Some(tab) => match from_its_page(tab) {
                Admission::Gated(_) if state.tabs.is_moving(tab) => {
                    Admission::Refused(eitri_core::tab_set::MOVING.into())
                }
                admitted => admitted,
            },
            None => Admission::Gated(view),
        },
        InboundMessage::ConfirmBypass { tab, scope, .. } => match scope.scope(*tab) {
            Ok(eitri_core::tab_set::BypassScope::Tab(tab)) => from_its_page(tab),
            Ok(eitri_core::tab_set::BypassScope::Default) => Admission::Gated(view),
            Err(protocol) => Admission::Refused(protocol),
        },
        // Only the base page answers the restore question (its arm refuses any other without asking
        // the window), and an `n` resumes nothing in bypass.
        InboundMessage::RestoreAnswer { keep_bypass, .. } => {
            if *keep_bypass && view.is_base() {
                Admission::Gated(view)
            } else {
                Admission::Ungated
            }
        }
        // An answer to no question this page may answer is refused by its arm as no longer current,
        // without asking the window.
        InboundMessage::TrustAnswer { nonce, .. } => {
            match target.and_then(|tab| trust_question_answered(state, tab, *nonce, view)) {
                Some(_) => Admission::Gated(view),
                None => Admission::Ungated,
            }
        }
        // Pressing the mode key answers no card: entering bypass raises its own y/n, and that answer
        // (`ConfirmBypass`) is gated.
        InboundMessage::CycleMode { .. } | InboundMessage::CycleDefaultMode { .. } => Admission::Ungated,
        // Escape on a trust question starts and loads nothing.
        InboundMessage::TrustCancel { .. } => Admission::Ungated,
        // None of these answers a card or a question, or loosens a permission.
        InboundMessage::Ready { .. }
        | InboundMessage::PathComplete { .. }
        | InboundMessage::SendMessage { .. }
        | InboundMessage::RunShell { .. }
        | InboundMessage::Interrupt { .. }
        | InboundMessage::StopBackground { .. }
        | InboundMessage::TurnRendered { .. }
        | InboundMessage::HandoffToTerminal { .. }
        | InboundMessage::Resume { .. }
        | InboundMessage::SelectTab { .. }
        | InboundMessage::RenameTab { .. }
        | InboundMessage::CloseTab { .. }
        | InboundMessage::ResetTab { .. }
        | InboundMessage::OpenDetail { .. }
        | InboundMessage::QueueMessage { .. }
        | InboundMessage::TakeBackQueue { .. }
        | InboundMessage::SendNow { .. }
        | InboundMessage::Draft { .. }
        | InboundMessage::EditDraft { .. }
        | InboundMessage::QuestionDraft { .. }
        | InboundMessage::EditQuestionOther { .. }
        | InboundMessage::HistoryPush { .. }
        | InboundMessage::OpenPath { .. }
        | InboundMessage::ReviewRequest { .. }
        | InboundMessage::ReviewDiffRequest { .. }
        | InboundMessage::ReviewRevert { .. }
        | InboundMessage::ReviewUndo { .. }
        | InboundMessage::ReviewCommentAdd { .. }
        | InboundMessage::ReviewCommentRemove { .. }
        | InboundMessage::ReviewSend { .. }
        | InboundMessage::ReviewRecover { .. }
        | InboundMessage::OpenInEditor { .. }
        | InboundMessage::ViewInEditor { .. }
        | InboundMessage::ViewProposal { .. }
        | InboundMessage::EditorCardView { .. }
        | InboundMessage::HintRequest { .. }
        | InboundMessage::HintTargets { .. }
        | InboundMessage::TabVerb { .. }
        | InboundMessage::CloseOthers { .. }
        | InboundMessage::PanelKeys { .. }
        | InboundMessage::NavFallthrough { .. }
        | InboundMessage::PaneNav { .. }
        | InboundMessage::ChooseProject { .. }
        | InboundMessage::KeymapClosed { .. }
        | InboundMessage::ViewExported { .. }
        | InboundMessage::OpenUrl { .. }
        | InboundMessage::RestoreLast { .. }
        | InboundMessage::TrustCommand { .. } => Admission::Ungated,
    }
}

/// Whether `view`'s page may answer about `tab`: refused, naming the tab, when the tab is in another
/// view. Asked before the window's gate, so an answer from a page that does not show the tab never
/// reaches the window, whatever covers it.
pub(super) fn page_may_answer(tabs: &eitri_core::tab_set::TabSet, tab: TabId, view: ViewId) -> Result<(), String> {
    if tabs.may_answer(tab, view) {
        return Ok(());
    }
    Err(match tabs.get(tab) {
        Some(t) => format!("tab {} is not shown in this page", t.number),
        None => format!("no tab {}", tab.0),
    })
}

/// The window's word on whether a float covers a chat view's module (`AgentPanelHandle::set_coverage`).
pub(super) type Coverage = Rc<dyn Fn(ViewId) -> bool>;

/// What an extra page that shows no tab answers a command about a tab.
pub(super) const NO_TAB_HERE: &str = "protocol: this page shows no tab";

/// What an extra page is told when it asks about the last window's tabs: only the base page offers them.
pub(super) const RESTORE_IS_THE_BASE_PAGES: &str = "protocol: the last session's tabs are restored from the base view";

/// What an extra page is told when it asks to close the other tabs: it shows one.
pub(super) const ONE_TAB_HERE: &str = "this view shows one tab: there are no others to close";

/// The one late mirror of `tab` from the page it was just moved out of (`TabSet::take_late_draft`,
/// already taken): it is newer than the copy the tab's new page was given, so the tab keeps it and that
/// page is offered it, marked late, so that a box edited there since keeps its own newer text.
pub(super) fn deliver_late_draft(state: &mut AgentPanelState, tab: TabId, text: &str, out: &mut Outbox) {
    state.tabs.set_draft(tab, text);
    if let Some(shown) = state.shown_in(tab) {
        out.send(shown, eitri_core::agent_bridge::serialize_late_draft_for_js(tab, text));
    }
}

/// A message from `view`'s page after it stopped being live. A view emptied by its tab leaving is
/// retired on the loop's next turn, which can come before its document has delivered the mirror its
/// composer flushed as the keys went; that mirror is still the tab's newest text, so it is taken as the
/// late draft it is. Nothing else from such a page is acted on, and nothing is answered: its document
/// is being replaced. Whether the message was taken.
pub(super) fn late_draft_from_idle_page(
    raw: &str,
    state: &mut AgentPanelState,
    view: ViewId,
    out: &mut Outbox,
) -> bool {
    if view.is_base() || state.shutting_down {
        return false;
    }
    let Some(message) = parse_inbound_message(raw) else {
        return false;
    };
    if !matches!(message, InboundMessage::Draft { .. }) {
        return false;
    }
    let resolved = state.tabs.resolve(message.tab_ref());
    let (Ok(Some(tab)), InboundMessage::Draft { text, .. }) = (resolved, message) else {
        return false;
    };
    if !state.tabs.take_late_draft(tab, view) {
        return false;
    }
    deliver_late_draft(state, tab, &text, out);
    true
}

/// What a page's message resolved to before the window is asked about it ([`admit`]).
pub(super) struct Admitted {
    /// The tab the message names, if it names one.
    pub(super) target: Option<TabId>,
    /// Whether this is the one `draft` the page a tab just left may still post for it.
    pub(super) late_draft: bool,
    /// What the window must agree to before the message's arm runs.
    pub(super) admission: Admission,
}

/// The first half of a message from `view`'s page: the tab it names, resolved, and what the window must
/// agree to before its arm runs. `None` when the message ends here, refused, with the refusal in `out`.
pub(super) fn admit(
    state: &mut AgentPanelState,
    message: &InboundMessage,
    view: ViewId,
    out: &mut Outbox,
) -> Option<Admitted> {
    let request_id = message.request_id();
    // A tab command names its tab, or it is refused. Never "the
    // active one" -- a permission answered just after a switch must reach the tab that asked.
    let resolved = state.tabs.resolve(message.tab_ref());
    // Whether this is the one `draft` the page a tab just left may still post for it.
    let mut late_draft = false;
    let target = match resolved {
        // An extra page that shows no tab has no tab to command, but the tab that just left it may be
        // owed the mirror its composer flushed as the keys went.
        Ok(Some(tab)) if !view.is_base() && state.tabs.tab_in(view).is_none() => {
            late_draft = matches!(message, InboundMessage::Draft { .. }) && state.tabs.take_late_draft(tab, view);
            if !late_draft {
                eprintln!("[agent_panel] refused {request_id}: {NO_TAB_HERE}");
                out.send(view, serialize_command_result_for_js(request_id, Err(NO_TAB_HERE)));
                return None;
            }
            Some(tab)
        }
        Ok(target) => target,
        Err(protocol) => {
            eprintln!("[agent_panel] refused {request_id}: {protocol}");
            out.send(view, serialize_command_result_for_js(request_id, Err(&protocol)));
            return None;
        }
    };
    // Every answer the window must agree to is classified here, and only here, before its arm runs.
    let admission = admission(state, message, view, target);
    Some(Admitted {
        target,
        late_draft,
        admission,
    })
}

/// A remote observer's message before its arm runs: the tab its key names in this window, and whether that tab may
/// take it. Apart from a page's admission and never through it: a remote message names no view, so nothing here asks
/// the page check or the window's gate, and no remote kind answers what they guard. Every kind is named, so a new one
/// is classified when it is added.
pub(super) fn admit_remote(
    state: &AgentPanelState,
    message: &RemoteInbound,
    key: &eitri_core::tab_set::RowKey,
) -> Result<TabId, RemoteRefusal> {
    let tab = state.tabs.tab_by_key(key).ok_or(RemoteRefusal::NoSuchTab)?;
    let held = state.tabs.get(tab).ok_or(RemoteRefusal::NoSuchTab)?;
    match message {
        // Stopping only stops work, so every mode takes it; but only of a Claude session that is here and running
        // the work the stop was asked about.
        RemoteInbound::Interrupt { expect_work, .. } => {
            if *held.agent() != AgentId::Claude {
                return Err(RemoteRefusal::NotClaude);
            }
            let Some(backend) = held.live().filter(|_| !state.tabs.is_moving(tab)) else {
                return Err(RemoteRefusal::NotLive);
            };
            let projection = backend.projection();
            let still_running = match expect_work {
                ExpectWork::Turn(turn) => projection.active_turn_id.as_deref() == Some(turn.as_str()),
                // The number only from a tick that counted every fold this projection shows.
                ExpectWork::Unseen(stretch) => {
                    projection.active_turn_id.is_none()
                        && projection.cli_unseen_busy
                        && held.unseen_stretch_at(projection.last_revision) == Some(*stretch)
                }
            };
            if !still_running {
                return Err(RemoteRefusal::WorkChanged);
            }
            Ok(tab)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_panel::editor_glue::tests::fixture as card_fixture;
    use crate::agent_panel::tests::*;

    fn toggle(handle: &AgentPanelHandle, view: ViewId, tab: TabId, request: &str) {
        handle.page_message(
            view,
            &serde_json::json!({
                "type": "toggle_following", "request_id": request, "tab": tab.0,
            })
            .to_string(),
        );
    }

    #[test]
    fn a_toggle_from_a_covered_view_is_refused() {
        let (handle, page, _) = handle_with_recording_page();
        handle.page_message(ViewId::BASE, READY);
        let tab = handle.state.borrow().tabs.active();
        handle.set_answer_gate(Rc::new(|_| Err("agent is under terminal 2".into())));
        toggle(&handle, ViewId::BASE, tab, "f1");
        assert_eq!(
            result_for(&page.sent.borrow(), "f1"),
            Some(Err("agent is under terminal 2".into()))
        );
        assert!(!handle.state.borrow().tabs.get(tab).unwrap().following);
        handle.set_answer_gate(Rc::new(|_| Ok(())));
        toggle(&handle, ViewId::BASE, tab, "f2");
        assert_eq!(result_for(&page.sent.borrow(), "f2"), Some(Ok(())));
        assert!(handle.state.borrow().tabs.get(tab).unwrap().following);
        handle.state.borrow_mut().editor_follower.edited(tab.0, "a.rs".into());
        assert!(!handle.state.borrow().editor_follower.is_idle());
        toggle(&handle, ViewId::BASE, tab, "f3");
        assert_eq!(result_for(&page.sent.borrow(), "f3"), Some(Ok(())));
        assert!(!handle.state.borrow().tabs.get(tab).unwrap().following);
        assert!(handle.state.borrow().editor_follower.is_idle());
    }

    #[test]
    fn a_toggle_from_another_page_is_refused() {
        let (handle, page, _) = handle_with_recording_page();
        handle.page_message(ViewId::BASE, READY);
        let tab = handle.state.borrow().tabs.active();
        attach_recording_page(&handle, view(2));
        handle.page_message(view(2), READY);
        let other = handle.state.borrow_mut().tabs.open(AgentId::Claude);
        assert_eq!(handle.break_out(view(2)), Ok(other));
        toggle(&handle, ViewId::BASE, other, "f1");
        assert!(result_for(&page.sent.borrow(), "f1")
            .unwrap()
            .unwrap_err()
            .contains("not shown in this page"));
        assert!(!handle.state.borrow().tabs.get(other).unwrap().following);
        assert!(!handle.state.borrow().tabs.get(tab).unwrap().following);
    }

    #[test]
    fn a_toggle_for_a_moving_tab_is_refused() {
        let (handle, page, _) = handle_with_recording_page();
        handle.page_message(ViewId::BASE, READY);
        let tab = handle.state.borrow().tabs.active();
        handle.state.borrow_mut().tabs.freeze_for_move(tab).unwrap();
        toggle(&handle, ViewId::BASE, tab, "f1");
        assert_eq!(
            result_for(&page.sent.borrow(), "f1"),
            Some(Err(eitri_core::tab_set::MOVING.into()))
        );
        assert!(!handle.state.borrow().tabs.get(tab).unwrap().following);
    }

    #[test]
    fn task_commands_refuse_an_unknown_task_and_a_codex_tab() {
        let dir = agent::state_dirs::test_workspace_dir("task-admission");
        let (handle, _, _) = handle_with_recording_page();
        let (provider, backend) = live_backend(&dir);
        let tab = handle.state.borrow().tabs.active();
        handle
            .state
            .borrow_mut()
            .tabs
            .get_mut(tab)
            .unwrap()
            .set_backend(TabBackend::Live(backend));
        provider.queue(agent::AgentDomainEvent::BackgroundTasksChanged {
            tasks: vec![agent::BackgroundTask {
                task_id: "known".into(),
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
        for kind in ["stop_task", "task_output"] {
            for (id, accepted) in [("known", true), ("missing", false)] {
                let message = parse_inbound_message(
                    &serde_json::json!({"type": kind, "request_id": "t1", "tab": tab.0, "task_id": id}).to_string(),
                )
                .unwrap();
                let result = admission(&handle.state.borrow(), &message, ViewId::BASE, Some(tab));
                assert_eq!(result == Admission::Ungated, accepted);
            }
        }
        let external = std::sync::Arc::new(eitri_core::test_providers::ExternalRecordingProvider::codex());
        let backend = AgentBackend::start_external_with(
            AgentId::Codex,
            external,
            &dir,
            eitri_core::test_providers::empty_trust_plan(),
            SessionModeChoice::Auto,
            None,
        )
        .unwrap();
        let codex = handle.state.borrow_mut().tabs.open(AgentId::Codex);
        handle
            .state
            .borrow_mut()
            .tabs
            .get_mut(codex)
            .unwrap()
            .set_backend(TabBackend::Live(backend));
        for kind in ["stop_task", "task_output"] {
            let message = parse_inbound_message(
                &serde_json::json!({"type": kind, "request_id": "t1", "tab": codex.0, "task_id": "known"}).to_string(),
            )
            .unwrap();
            assert!(
                matches!(admission(&handle.state.borrow(), &message, ViewId::BASE, Some(codex)), Admission::Refused(why) if why.contains("only available for Claude"))
            );
        }
        for raw in [
            serde_json::json!({"type": "list_models", "request_id": "m", "tab": codex.0}),
            serde_json::json!({"type": "set_model", "request_id": "m", "tab": codex.0, "model": "x"}),
            serde_json::json!({"type": "apply_effort", "request_id": "m", "tab": codex.0, "effort": "low"}),
        ] {
            let message = parse_inbound_message(&raw.to_string()).unwrap();
            assert!(
                matches!(
                    admission(&handle.state.borrow(), &message, ViewId::BASE, Some(codex)),
                    Admission::Refused(_)
                ),
                "{raw}: the model of another agent's session is not changed here"
            );
        }
        for raw in [
            serde_json::json!({"type": "list_models", "request_id": "m", "tab": tab.0}),
            serde_json::json!({"type": "set_model", "request_id": "m", "tab": tab.0, "model": "x"}),
            serde_json::json!({"type": "apply_effort", "request_id": "m", "tab": tab.0, "effort": "low"}),
        ] {
            let message = parse_inbound_message(&raw.to_string()).unwrap();
            assert!(
                admission(&handle.state.borrow(), &message, ViewId::BASE, Some(tab)) == Admission::Ungated,
                "{raw}"
            );
        }
        // An ACP agent's session offers what that agent lists, so the same controls reach it.
        let acp_agent = AgentId::Acp("gem".into());
        let acp = std::sync::Arc::new(eitri_core::test_providers::ExternalRecordingProvider::named(
            "acp:gem", true,
        ));
        let backend = AgentBackend::start_external_with(
            acp_agent.clone(),
            acp,
            &dir,
            eitri_core::test_providers::empty_trust_plan(),
            SessionModeChoice::Auto,
            None,
        )
        .unwrap();
        let acp_tab = handle.state.borrow_mut().tabs.open(acp_agent);
        handle
            .state
            .borrow_mut()
            .tabs
            .get_mut(acp_tab)
            .unwrap()
            .set_backend(TabBackend::Live(backend));
        for raw in [
            serde_json::json!({"type": "list_models", "request_id": "m", "tab": acp_tab.0}),
            serde_json::json!({"type": "set_model", "request_id": "m", "tab": acp_tab.0, "model": "x"}),
            serde_json::json!({"type": "apply_effort", "request_id": "m", "tab": acp_tab.0, "effort": "low"}),
        ] {
            let message = parse_inbound_message(&raw.to_string()).unwrap();
            assert!(
                admission(&handle.state.borrow(), &message, ViewId::BASE, Some(acp_tab)) == Admission::Ungated,
                "{raw}"
            );
        }
        shut_down_every_tab(&handle);
    }

    /// A card answer from the page meets the window's gate at the moment it arrives: refused while the
    /// gate says a float covers the chat -- the card still waiting, nothing sent to the agent, the page
    /// told why -- and the same answer taken once it says the chat is clear. With no gate, answers are
    /// taken as they always were.
    #[test]
    fn a_card_answer_is_refused_only_while_a_float_covers_the_chat_now() {
        const COVERED: &str = "agent is under terminal 2: C-b M-- from terminal 2 stows it";
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("panel-answer-gate");
        let (handle, page, _host) = handle_with_recording_page();
        let state = handle.state.clone();
        let tab = state.borrow().tabs.active();
        let (provider, backend) = live_backend(&dir);
        state
            .borrow_mut()
            .tabs
            .get_mut(tab)
            .unwrap()
            .set_backend(TabBackend::Live(backend));
        // A `Write` into `.git` needs a human, so each is left a card.
        for id in ["perm-1", "perm-2"] {
            provider.queue(agent::AgentDomainEvent::PermissionRequested {
                permission_id: id.into(),
                tool_use_id: None,
                tool_name: "Write".into(),
                input: serde_json::json!({ "file_path": format!(".git/{id}"), "content": "" }),
                provider_prompt: None,
            });
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while state.borrow().tabs.get(tab).unwrap().attention.attention().pending != 2 {
            assert!(std::time::Instant::now() < deadline, "timed out waiting for the cards");
            state.borrow_mut().tabs.pump(&dir, true);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let covered = Rc::new(Cell::new(true));
        let asked = Rc::new(Cell::new(0));
        handle.set_answer_gate(Rc::new({
            let covered = covered.clone();
            let asked = asked.clone();
            move |view: ViewId| {
                assert_eq!(view, ViewId::BASE, "asked with the view of the page that answered");
                asked.set(asked.get() + 1);
                if covered.get() {
                    Err(COVERED.to_string())
                } else {
                    Ok(())
                }
            }
        }));
        let answer = |request: &str, permission: &str| {
            handle.page_message(ViewId::BASE, &format!(
                r#"{{"type":"permission_response","request_id":"{request}","tab":{},"permission_id":"{permission}","decision":"allow"}}"#,
                tab.0
            ));
        };

        answer("r1", "perm-1");
        assert_eq!(result_for(&page.sent.borrow(), "r1"), Some(Err(COVERED.to_string())));
        assert!(
            state.borrow().tabs.get(tab).unwrap().card_is_waiting("perm-1"),
            "a refused answer leaves the card waiting"
        );
        assert!(provider.resolutions().is_empty(), "nothing reached the agent");

        // A tab's bypass `y` approves every card waiting in it, so from a covered chat it meets the
        // same gate before anything about the prompt is checked, and both cards stay.
        handle.page_message(
            ViewId::BASE,
            &format!(
                r#"{{"type":"confirm_bypass","request_id":"b1","tab":{},"scope":"tab","nonce":1}}"#,
                tab.0
            ),
        );
        assert_eq!(result_for(&page.sent.borrow(), "b1"), Some(Err(COVERED.to_string())));
        for id in ["perm-1", "perm-2"] {
            assert!(
                state.borrow().tabs.get(tab).unwrap().card_is_waiting(id),
                "{id} still waits"
            );
        }
        assert!(provider.resolutions().is_empty(), "bypass approved nothing");

        // The float moved off: the gate is asked again, at this answer, and lets it through.
        covered.set(false);
        answer("r2", "perm-1");
        assert_eq!(result_for(&page.sent.borrow(), "r2"), Some(Ok(())));
        assert_eq!(provider.resolutions(), vec![("perm-1".to_string(), true)]);
        assert_eq!(asked.get(), 3, "asked once per answer");

        state.borrow_mut().answer_gate = None;
        answer("r3", "perm-2");
        assert_eq!(
            result_for(&page.sent.borrow(), "r3"),
            Some(Ok(())),
            "no gate: taken as before"
        );
        assert_eq!(
            provider.resolutions(),
            vec![("perm-1".to_string(), true), ("perm-2".to_string(), true)]
        );
        for t in state.borrow_mut().tabs.take_all() {
            if let Some(retired) = t.retired {
                retired.into_backend().shutdown();
            }
        }
    }

    /// A covered page's card may not be the one the person read, so its key answers nothing, and the card waits.
    #[test]
    fn a_card_answer_from_a_covered_view_is_refused() {
        const COVERED: &str = "agent is under terminal 2: C-b M-- from terminal 2 stows it";
        let panel = card_fixture("admission-card-covered");
        let card = panel.raise_a_card("c1");
        let covered = Rc::new(Cell::new(true));
        panel.handle.set_answer_gate(Rc::new({
            let covered = covered.clone();
            move |view: ViewId| {
                assert_eq!(view, ViewId::BASE, "asked with the view of the page that answered");
                if covered.get() {
                    Err(COVERED.to_string())
                } else {
                    Ok(())
                }
            }
        }));

        panel.answer_card(ViewId::BASE, "card-1", &card, "allow");
        assert_eq!(
            result_for(&panel.page.sent.borrow(), "card-1"),
            Some(Err(COVERED.to_string()))
        );
        assert_eq!(panel.held_cards(), 1, "a refused answer leaves the card waiting");
        panel.settle();
        assert!(panel.editor.asked().is_empty());

        covered.set(false);
        panel.answer_card(ViewId::BASE, "card-2", &card, "allow");
        assert_eq!(result_for(&panel.page.sent.borrow(), "card-2"), Some(Ok(())));
        assert_eq!(panel.held_cards(), 0);
    }

    #[test]
    fn a_card_answer_from_another_page_is_refused() {
        let panel = card_fixture("admission-card-other-page");
        let card = panel.raise_a_card("c1");
        let second = attach_recording_page(&panel.handle, view(2));
        panel.handle.page_message(view(2), READY);
        assert_eq!(panel.handle.break_out(view(2)), Ok(panel.tab));
        let asked_views = recording_gate(&panel.handle, &[], Rc::new(Cell::new(false)));
        let number = panel.handle.state.borrow().tabs.get(panel.tab).unwrap().number;

        panel.answer_card(ViewId::BASE, "card-1", &card, "allow");
        assert_eq!(
            result_for(&panel.page.sent.borrow(), "card-1"),
            Some(Err(format!("tab {number} is not shown in this page")))
        );
        assert!(asked_views.borrow().is_empty(), "refused before the window is asked");
        assert_eq!(panel.held_cards(), 1);

        panel.answer_card(view(2), "card-2", &card, "allow");
        assert_eq!(result_for(&second.sent.borrow(), "card-2"), Some(Ok(())));
        assert_eq!(*asked_views.borrow(), vec![view(2)]);
        assert_eq!(panel.held_cards(), 0);
    }

    /// A frozen tab's calls end when it leaves, so an answer now would run a call for an agent gone elsewhere.
    #[test]
    fn a_card_answer_for_a_moving_tab_is_refused() {
        let panel = card_fixture("admission-card-moving");
        let card = panel.raise_a_card("c1");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let frozen = panel.handle.state.borrow_mut().tabs.freeze_for_move(panel.tab);
            match frozen {
                Ok(_) => break,
                Err(eitri_core::tab_set::MoveRefusedWhy::Folding) if std::time::Instant::now() < deadline => {}
                Err(why) => panic!("the tab does not freeze: {why:?}"),
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }

        panel.answer_card(ViewId::BASE, "card-1", &card, "allow");
        assert_eq!(
            result_for(&panel.page.sent.borrow(), "card-1"),
            Some(Err(eitri_core::tab_set::MOVING.to_string()))
        );
        assert_eq!(panel.held_cards(), 1);
    }

    /// A key can only name a card it was shown: an id the broker no longer holds answers nothing, once answered
    /// included, and leaves the card that is waiting alone.
    #[test]
    fn a_stale_card_id_meets_no_such_card() {
        const STALE: &str = "ec-0000000000000000-9";
        let panel = card_fixture("admission-card-stale");
        let card = panel.raise_a_card("c1");
        panel.answer_card(ViewId::BASE, "card-1", STALE, "allow");
        assert_eq!(
            result_for(&panel.page.sent.borrow(), "card-1"),
            Some(Err(format!("no such card: {STALE}")))
        );
        assert_eq!(panel.held_cards(), 1);

        panel.answer_card(ViewId::BASE, "card-2", &card, "allow");
        panel.answer_card(ViewId::BASE, "card-3", &card, "allow");
        assert_eq!(result_for(&panel.page.sent.borrow(), "card-2"), Some(Ok(())));
        assert_eq!(
            result_for(&panel.page.sent.borrow(), "card-3"),
            Some(Err(format!("no such card: {card}")))
        );
    }

    /// A chat a float covers is off screen for attention: what finished in it stays unread while the
    /// page is mapped under the float, and is seen once the window says the float is off it
    /// (`page_shown` again). With no probe set, a mapped page is on screen as before.
    #[test]
    fn a_chat_under_a_float_leaves_what_finished_unread_until_the_float_is_off_it() {
        let (handle, page, _host) = handle_with_recording_page();
        let state = handle.state.clone();
        let tab = state.borrow().tabs.active();
        let unread = || state.borrow().tabs.get(tab).unwrap().attention.attention().unread;
        page.visible.set(true);
        state
            .borrow_mut()
            .tabs
            .get_mut(tab)
            .unwrap()
            .attention
            .session_ended(false);
        assert!(unread());

        let covered = Rc::new(Cell::new(true));
        handle.set_coverage(Rc::new({
            let covered = covered.clone();
            move |_view| covered.get()
        }));
        handle.page_shown(ViewId::BASE);
        assert!(unread(), "mapped under a float is not seen");
        assert!(!Screen::read(&state, &handle.pages).shows(ViewId::BASE));

        covered.set(false);
        handle.page_shown(ViewId::BASE);
        assert!(!unread(), "seen once the float is off it");
        assert!(Screen::read(&state, &handle.pages).shows(ViewId::BASE));

        page.visible.set(false);
        assert!(
            !Screen::read(&state, &handle.pages).shows(ViewId::BASE),
            "an unmapped page is never on screen"
        );
        state.borrow_mut().coverage = None;
        page.visible.set(true);
        assert!(
            Screen::read(&state, &handle.pages).shows(ViewId::BASE),
            "no probe: mapped is on screen"
        );
    }

    fn post_draft(handle: &AgentPanelHandle, from: ViewId, request: &str, tab: TabId, text: &str) {
        let message = serde_json::json!({ "type": "draft", "request_id": request, "tab": tab.0, "text": text });
        handle.page_message(from, &message.to_string());
    }

    #[test]
    fn a_late_draft_from_the_page_a_tab_just_left_reaches_it() {
        let (handle, base, second) = handle_with_two_pages();
        let moved = handle.state.borrow_mut().tabs.open(AgentId::Claude);
        assert_eq!(handle.break_out(view(2)), Ok(moved));
        post_draft(&handle, view(2), "d-1", moved, "fix the te");
        assert_eq!(handle.state.borrow().tabs.get(moved).unwrap().draft(), "fix the te");

        assert_eq!(handle.join_back(view(2)), Ok(moved));
        clear(&[&base, &second]);
        post_draft(&handle, view(2), "d-2", moved, "fix the test");
        assert_eq!(handle.state.borrow().tabs.get(moved).unwrap().draft(), "fix the test");
        assert_eq!(
            result_for(&second.sent.borrow(), "d-2"),
            None,
            "a mirror is answered with nothing"
        );
        let told: Vec<serde_json::Value> = base.sent.borrow().iter().map(|e| json_of(e)).collect();
        assert!(
            told.contains(
                &serde_json::json!({ "kind": "draft", "tab": moved.0, "text": "fix the test", "late": true })
            ),
            "the base page's box is offered it: {told:?}"
        );

        post_draft(&handle, view(2), "d-3", moved, "fix the test again");
        assert_eq!(
            result_for(&second.sent.borrow(), "d-3"),
            Some(Err(NO_TAB_HERE.to_string())),
            "taken once"
        );
        assert_eq!(handle.state.borrow().tabs.get(moved).unwrap().draft(), "fix the test");

        let third = attach_recording_page(&handle, view(3));
        handle.page_message(view(3), READY);
        handle.open_in(view(3)).unwrap();
        post_draft(&handle, view(3), "d-4", moved, "not mine");
        let number = handle.state.borrow().tabs.get(moved).unwrap().number;
        assert_eq!(
            result_for(&third.sent.borrow(), "d-4"),
            Some(Err(format!("tab {number} is not shown in this page")))
        );
        assert_eq!(handle.state.borrow().tabs.get(moved).unwrap().draft(), "fix the test");
    }

    #[test]
    fn a_late_draft_reaches_the_tab_after_the_emptied_view_s_page_went_idle() {
        let (handle, base, second) = handle_with_two_pages();
        let moved = handle.state.borrow_mut().tabs.open(AgentId::Claude);
        assert_eq!(handle.break_out(view(2)), Ok(moved));
        post_draft(&handle, view(2), "d-1", moved, "fix the te");
        let emptied = Rc::new(RefCell::new(Vec::<ViewId>::new()));
        handle.on_view_empty({
            let emptied = emptied.clone();
            move |view| emptied.borrow_mut().push(view)
        });
        assert_eq!(handle.join_back(view(2)), Ok(moved));
        assert_eq!(*emptied.borrow(), vec![view(2)]);
        // The window retires the emptied view on the loop's next turn, which can come before the
        // document delivers what its composer flushed as the keys went.
        handle.idle_page(view(2));
        assert!(handle.pages.live(view(2)).is_none(), "the emptied view's page is idle");
        clear(&[&base, &second]);

        post_draft(&handle, view(2), "d-2", moved, "fix the test");
        assert_eq!(handle.state.borrow().tabs.get(moved).unwrap().draft(), "fix the test");
        let told: Vec<serde_json::Value> = base.sent.borrow().iter().map(|e| json_of(e)).collect();
        assert!(
            told.contains(
                &serde_json::json!({ "kind": "draft", "tab": moved.0, "text": "fix the test", "late": true })
            ),
            "the base page's box is offered it: {told:?}"
        );
        assert_eq!(
            result_for(&second.sent.borrow(), "d-2"),
            None,
            "an idle page is answered nothing"
        );

        // Taken once; nothing else an idle page posts is acted on.
        post_draft(&handle, view(2), "d-3", moved, "fix the test again");
        assert_eq!(handle.state.borrow().tabs.get(moved).unwrap().draft(), "fix the test");
        handle.page_message(view(2), READY);
        assert!(
            second.sent.borrow().is_empty(),
            "an idle page is sent nothing: {:?}",
            second.sent.borrow()
        );
    }

    #[test]
    fn a_permission_answer_is_taken_only_from_the_page_that_shows_its_tab() {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("views-answer-per-page");
        let (handle, base, second) = handle_with_two_pages();
        let (bound, bound_provider) = live_tab(&handle, &dir);
        assert_eq!(handle.break_out(view(2)), Ok(bound));
        // A base-view tab left in the background: still the base page's to answer.
        let (background, background_provider) = live_tab(&handle, &dir);
        handle.state.borrow_mut().tabs.open(AgentId::Claude);
        assert_ne!(handle.state.borrow().tabs.active(), background);
        bound_provider.queue(write_card("perm-2"));
        background_provider.queue(write_card("perm-b"));
        tick_handle_until(&handle, "both cards", || {
            pending_cards(&handle, bound) == 1 && pending_cards(&handle, background) == 1
        });
        let asked = recording_gate(&handle, &[], Rc::new(Cell::new(false)));
        let number = handle.state.borrow().tabs.get(bound).unwrap().number;

        permission_response(&handle, ViewId::BASE, "r1", bound, "perm-2");
        assert_eq!(
            result_for(&base.sent.borrow(), "r1"),
            Some(Err(format!("tab {number} is not shown in this page")))
        );
        assert!(waiting(&handle, bound, "perm-2"), "still waiting");
        assert!(bound_provider.resolutions().is_empty());
        assert!(asked.borrow().is_empty(), "refused before the window is asked");

        permission_response(&handle, view(2), "r2", bound, "perm-2");
        assert_eq!(result_for(&second.sent.borrow(), "r2"), Some(Ok(())));
        assert_eq!(bound_provider.resolutions(), vec![("perm-2".to_string(), true)]);

        // Page 2 cannot answer the base view's card, whichever tab the base view shows.
        permission_response(&handle, view(2), "r3", background, "perm-b");
        assert!(matches!(result_for(&second.sent.borrow(), "r3"), Some(Err(_))));
        assert!(waiting(&handle, background, "perm-b"));
        permission_response(&handle, ViewId::BASE, "r4", background, "perm-b");
        assert_eq!(result_for(&base.sent.borrow(), "r4"), Some(Ok(())));
        assert_eq!(background_provider.resolutions(), vec![("perm-b".to_string(), true)]);
        assert_eq!(
            *asked.borrow(),
            vec![view(2), ViewId::BASE],
            "asked with each answering page's view"
        );
        shut_down_every_tab(&handle);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_permission_answer_from_the_page_a_tab_left_is_refused_and_the_new_page_answers_it() {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("views-answer-left-page");
        let (handle, base, second) = handle_with_two_pages();
        let (tab, provider) = live_tab(&handle, &dir);
        provider.queue(write_card("perm-1"));
        tick_handle_until(&handle, "the card", || pending_cards(&handle, tab) == 1);
        // The base page drew the card; the tab moves to view 2 before its answer arrives.
        assert_eq!(handle.break_out(view(2)), Ok(tab));
        permission_response(&handle, ViewId::BASE, "r1", tab, "perm-1");
        assert!(
            matches!(result_for(&base.sent.borrow(), "r1"), Some(Err(why)) if why.contains("not shown in this page"))
        );
        assert!(
            waiting(&handle, tab, "perm-1"),
            "the card waits for the page that shows it"
        );
        assert!(provider.resolutions().is_empty());

        permission_response(&handle, view(2), "r2", tab, "perm-1");
        assert_eq!(result_for(&second.sent.borrow(), "r2"), Some(Ok(())));
        assert_eq!(provider.resolutions(), vec![("perm-1".to_string(), true)]);
        shut_down_every_tab(&handle);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_card_answer_from_a_covered_view_is_refused_and_the_card_stays() {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("views-answer-covered");
        let (handle, _base, second) = handle_with_two_pages();
        let (tab, provider) = live_tab(&handle, &dir);
        assert_eq!(handle.break_out(view(2)), Ok(tab));
        provider.queue(write_card("perm-1"));
        tick_handle_until(&handle, "the card", || pending_cards(&handle, tab) == 1);
        let covered = Rc::new(Cell::new(true));
        let asked = recording_gate(&handle, &[2], covered.clone());

        permission_response(&handle, view(2), "r1", tab, "perm-1");
        assert!(matches!(result_for(&second.sent.borrow(), "r1"), Some(Err(why)) if why.contains("under terminal 2")));
        assert!(
            waiting(&handle, tab, "perm-1"),
            "a refused answer leaves the card waiting"
        );
        assert!(provider.resolutions().is_empty(), "nothing reached the agent");
        assert_eq!(*asked.borrow(), vec![view(2)]);

        covered.set(false);
        permission_response(&handle, view(2), "r2", tab, "perm-1");
        assert_eq!(result_for(&second.sent.borrow(), "r2"), Some(Ok(())));
        assert_eq!(provider.resolutions(), vec![("perm-1".to_string(), true)]);
        shut_down_every_tab(&handle);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_trust_answer_from_a_covered_page_is_refused_and_asked_again() {
        let TrustViews {
            handle,
            second,
            tab,
            fx,
            ..
        } = trust_views("views-trust-covered");
        let covered = Rc::new(Cell::new(true));
        let asked = recording_gate(&handle, &[2], covered.clone());
        handle.page_message(
            view(2),
            &format!(
                r#"{{"type":"send_message","request_id":"s1","tab":{},"text":"hello"}}"#,
                tab.0
            ),
        );
        poll_trust_until(&handle, "the question", || !prompts(&second.sent.borrow()).is_empty());
        let first = prompts(&second.sent.borrow()).pop().unwrap();
        let mut shown = first.clone();

        // Both a `y` and an `n` are refused from a covered page.
        for (request, trust) in [("a1", true), ("n1", false)] {
            clear(&[&second]);
            trust_answer_from(&handle, view(2), request, &shown, trust);
            let sent = second.sent.borrow().clone();
            assert!(
                matches!(result_for(&sent, request), Some(Err(why)) if why.contains("under terminal 2")),
                "{sent:?}"
            );
            assert!(
                notices(&sent).iter().any(|n| n.contains("under terminal 2")),
                "the page is told why"
            );
            poll_trust_until(&handle, "the question again", || {
                !prompts(&second.sent.borrow()).is_empty()
            });
            let again = prompts(&second.sent.borrow()).pop().unwrap();
            assert_eq!(again.tab, tab);
            assert_ne!(again.nonce, shown.nonce, "asked again under a fresh nonce");
            assert!(awaiting_trust(&handle, tab), "the tab still waits for its answer");
            assert!(fx.spawned.borrow().is_empty(), "nothing started");
            shown = again;
        }
        assert_eq!(*asked.borrow(), vec![view(2), view(2)]);

        covered.set(false);
        trust_answer_from(&handle, view(2), "a2", &first, true);
        assert!(
            matches!(result_for(&second.sent.borrow(), "a2"), Some(Err(_))),
            "the question first shown is no longer current"
        );
        trust_answer_from(&handle, view(2), "a3", &shown, true);
        assert_eq!(result_for(&second.sent.borrow(), "a3"), Some(Ok(())));
        poll_trust_until(&handle, "the start", || !fx.spawned.borrow().is_empty());
        assert!(fx.record_file().is_file());
    }

    #[test]
    fn a_trust_command_answer_from_a_covered_page_ends_the_command_unrecorded() {
        let TrustViews {
            handle,
            second,
            fx,
            hold,
            ..
        } = trust_views("views-trust-command-covered");
        let covered = Rc::new(Cell::new(true));
        recording_gate(&handle, &[2], covered.clone());
        let trust_command_from = |request: &str| {
            handle.page_message(
                view(2),
                &format!(r#"{{"type":"trust_command","request_id":"{request}","action":"trust","agent":"claude"}}"#),
            );
        };
        trust_command_from("c1");
        poll_trust_until(&handle, "the :trust prompt", || {
            !prompts(&second.sent.borrow()).is_empty()
        });
        let prompt = prompts(&second.sent.borrow()).pop().unwrap();
        poll_trust_until_idle(&handle);
        // From here a job the answer queued would stay queued, and the gate would say it is busy.
        hold.close();
        clear(&[&second]);
        trust_answer_from(&handle, view(2), "a1", &prompt, true);
        let sent = second.sent.borrow().clone();
        assert!(
            notices(&sent)
                .iter()
                .any(|n| n.contains("under terminal 2") && n.contains(":trust")),
            "{sent:?}"
        );
        assert!(matches!(result_for(&sent, "a1"), Some(Err(why)) if why.contains(":trust")));
        poll_trust_until(&handle, "the command's result", || {
            result_for(&second.sent.borrow(), "c1").is_some()
        });
        assert!(matches!(result_for(&second.sent.borrow(), "c1"), Some(Err(_))));
        assert!(
            !handle.state.borrow().trust.is_busy(),
            "no Record or Confirm job was queued"
        );
        assert!(!fx.record_file().exists(), "nothing recorded");
        hold.open();

        covered.set(false);
        clear(&[&second]);
        trust_command_from("c2");
        poll_trust_until(&handle, "the :trust prompt again", || {
            !prompts(&second.sent.borrow()).is_empty()
        });
        assert_ne!(prompts(&second.sent.borrow()).pop().unwrap().nonce, prompt.nonce);
    }

    // ---- One admission step: every answer the window must agree to is put to its gate once. ----

    /// A card answer the window's gate turns down changes nothing: the card waits and the agent hears
    /// nothing, and the same answer is taken once the gate lets it through.
    #[test]
    fn a_refused_card_answer_leaves_the_card_waiting() {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("admission-card");
        let (handle, base, _second) = handle_with_two_pages();
        let (tab, provider) = live_tab(&handle, &dir);
        provider.queue(write_card("perm-1"));
        tick_handle_until(&handle, "the card", || pending_cards(&handle, tab) == 1);
        let covered = Rc::new(Cell::new(true));
        let asked = recording_gate(&handle, &[1], covered.clone());

        permission_response(&handle, ViewId::BASE, "r1", tab, "perm-1");
        assert!(
            matches!(result_for(&base.sent.borrow(), "r1"), Some(Err(why)) if why.contains("under terminal 2")),
            "the page is told the window's words"
        );
        assert!(waiting(&handle, tab, "perm-1"), "the card still waits");
        assert!(provider.resolutions().is_empty(), "nothing reached the agent");
        assert_eq!(
            *asked.borrow(),
            vec![ViewId::BASE],
            "asked once, with the answering page's view"
        );

        covered.set(false);
        permission_response(&handle, ViewId::BASE, "r2", tab, "perm-1");
        assert_eq!(result_for(&base.sent.borrow(), "r2"), Some(Ok(())));
        assert_eq!(provider.resolutions(), vec![("perm-1".to_string(), true)]);
        shut_down_every_tab(&handle);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A trust `y` the window's gate turns down starts nothing: the tab keeps waiting and its question
    /// is shown again in the same page under a fresh nonce, which is the one that answers it later.
    #[test]
    fn a_refused_trust_answer_is_asked_again_under_a_fresh_nonce() {
        let TrustViews {
            handle,
            second,
            tab,
            fx,
            ..
        } = trust_views("admission-trust");
        let covered = Rc::new(Cell::new(true));
        let asked = recording_gate(&handle, &[2], covered.clone());
        handle.page_message(
            view(2),
            &format!(
                r#"{{"type":"send_message","request_id":"s1","tab":{},"text":"hello"}}"#,
                tab.0
            ),
        );
        poll_trust_until(&handle, "the question", || !prompts(&second.sent.borrow()).is_empty());
        let shown = prompts(&second.sent.borrow()).pop().unwrap();
        clear(&[&second]);

        trust_answer_from(&handle, view(2), "a1", &shown, true);
        assert!(
            matches!(result_for(&second.sent.borrow(), "a1"), Some(Err(why)) if why.contains("under terminal 2")),
            "the page is told the window's words"
        );
        poll_trust_until(&handle, "the question again", || {
            !prompts(&second.sent.borrow()).is_empty()
        });
        let again = prompts(&second.sent.borrow()).pop().unwrap();
        assert_eq!(again.tab, tab);
        assert_ne!(again.nonce, shown.nonce, "asked again under a fresh nonce");
        assert!(awaiting_trust(&handle, tab), "the tab still waits for its answer");
        assert!(fx.spawned.borrow().is_empty(), "nothing started");
        assert_eq!(
            *asked.borrow(),
            vec![view(2)],
            "asked once, with the answering page's view"
        );

        covered.set(false);
        trust_answer_from(&handle, view(2), "a2", &again, true);
        assert_eq!(result_for(&second.sent.borrow(), "a2"), Some(Ok(())));
        poll_trust_until(&handle, "the start", || !fx.spawned.borrow().is_empty());
    }

    /// A bypass `y` the window's gate turns down changes nothing: the tab keeps its mode, its card
    /// waits, and the prompt keeps its nonce, so the same `y` enters bypass once the gate lets it.
    #[test]
    fn a_refused_bypass_confirm_changes_nothing() {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("admission-bypass");
        let (handle, base, _second) = handle_with_two_pages();
        let (tab, provider) = live_tab(&handle, &dir);
        provider.queue(write_card("perm-1"));
        tick_handle_until(&handle, "the card", || pending_cards(&handle, tab) == 1);
        let before = mode_of(&handle, tab);
        assert_ne!(before, SessionModeChoice::Bypass);
        let covered = Rc::new(Cell::new(true));
        let asked = recording_gate(&handle, &[1], covered.clone());
        clear(&[&base]);
        cycle_mode_from(&handle, ViewId::BASE, "m1", tab);
        let (_, nonce) = last_bypass_prompt(&base).expect("the bypass prompt");

        confirm_bypass_from(&handle, ViewId::BASE, "y1", tab, nonce);
        assert!(
            matches!(result_for(&base.sent.borrow(), "y1"), Some(Err(why)) if why.contains("under terminal 2")),
            "the page is told the window's words"
        );
        assert_eq!(mode_of(&handle, tab), before, "the mode is unchanged");
        assert!(waiting(&handle, tab, "perm-1"), "the card still waits");
        assert!(provider.resolutions().is_empty(), "bypass approved nothing");
        assert_eq!(
            *asked.borrow(),
            vec![ViewId::BASE],
            "asked once, with the answering page's view"
        );

        covered.set(false);
        confirm_bypass_from(&handle, ViewId::BASE, "y2", tab, nonce);
        assert_eq!(
            result_for(&base.sent.borrow(), "y2"),
            Some(Ok(())),
            "the prompt was left under its nonce"
        );
        assert_eq!(mode_of(&handle, tab), SessionModeChoice::Bypass);
        shut_down_every_tab(&handle);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The window's gate is read and asked in one place, after one classification of the message
    /// that names every message kind and before the message's arm runs; an arm that asked the gate
    /// itself could ask it with the state held, or in its own order.
    #[test]
    fn admission_is_the_only_caller_of_the_gate() {
        the_gate_is_asked_once_between_admission_and_the_arm(&panel_sources());
    }

    /// The smallest panel that asks the gate where it should, for the scan's own test.
    const ASKS_THE_GATE_ONCE: &str = "\
        fn ask_gate(state: &S) { let hook = state.answer_gate.clone(); gate(view); }\n\
        impl AgentPanelHandle { pub fn page_message(&self) { let a = admit(m); ask_gate(a); handle_inbound_message(a); } }\n\
        fn admit(state: &mut S) { state.take_late_draft(); let kind = admission(m); }\n\
        fn handle_inbound_message() {}\n\
        fn admission(m: &M) -> A { match m { M::A => 1, M::B => 2 } }\n";

    #[test]
    fn a_gate_asked_in_a_second_file_of_the_panel_fails_the_scan() {
        let clean = fixture(&[
            ("panel/src/a.rs", ASKS_THE_GATE_ONCE),
            ("panel/src/b.rs", "fn arm() {}"),
        ]);
        assert_eq!(
            scan_failure(|| the_gate_is_asked_once_between_admission_and_the_arm(&clean)),
            None
        );
        let read = fixture(&[
            ("panel/src/a.rs", ASKS_THE_GATE_ONCE),
            ("panel/src/b.rs", "fn arm() { ask_gate(view); }"),
        ]);
        let why = scan_failure(|| the_gate_is_asked_once_between_admission_and_the_arm(&read))
            .expect("the second file is read");
        assert!(why.contains("asked for one message in one place"), "{why}");
    }

    /// Panics unless the panel's code, every file of `sources` read as a whole, reads the window's gate in
    /// one function and asks it for a message once, after `admit` has classified it and before its arm.
    fn the_gate_is_asked_once_between_admission_and_the_arm(sources: &[eitri_core::source_scan::Source]) {
        use eitri_core::source_scan as scan;
        let production: Vec<String> = sources
            .iter()
            .map(|source| {
                let code = scan::code_only(&source.text);
                scan::without(&code, &scan::modules(&code, "tests"))
            })
            .collect();
        let everywhere = |needle: &str| -> usize { production.iter().map(|code| code.matches(needle).count()).sum() };
        assert_eq!(everywhere("through_answer_gate"), 0, "no per-arm gate helper");
        assert_eq!(
            everywhere(".answer_gate.clone()"),
            1,
            "the gate is read out of the state in one place"
        );
        let asker: Vec<&str> = production
            .iter()
            .flat_map(|code| scan::functions(code, "ask_gate"))
            .collect();
        assert_eq!(asker.len(), 1, "one function asks the gate");
        assert!(
            asker[0].contains(".answer_gate.clone()"),
            "and it is the one that reads it"
        );
        assert_eq!(
            scan::calls(asker[0], "gate"),
            1,
            "the window's gate is called in one place"
        );

        let code = production_code(sources);
        assert_eq!(scan::calls(&code, "ask_gate"), 1, "asked for one message in one place");
        assert_eq!(scan::calls(&code, "admission"), 1, "one classification of each message");
        // A message is admitted (its tab resolved, the late draft taken, the message classified), then
        // the gate is asked with the state released, and only then does its arm run.
        let entry = scan::functions(&code, "page_message");
        assert_eq!(entry.len(), 1);
        let entry = entry[0];
        let in_entry = |needle: &str| {
            entry
                .find(needle)
                .unwrap_or_else(|| panic!("{needle} in the message's entry"))
        };
        assert!(
            in_entry("admit(") < in_entry("ask_gate(") && in_entry("ask_gate(") < in_entry("handle_inbound_message("),
            "admitted, then the gate asked, then the arm"
        );
        let admit = scan::functions(&code, "admit");
        assert_eq!(admit.len(), 1);
        let admit = admit[0];
        let at = |needle: &str| admit.find(needle).unwrap_or_else(|| panic!("{needle} in admit"));
        let classified = at("admission(");
        assert!(
            at("take_late_draft") < classified,
            "classified after the late draft is taken"
        );
        // Its own statement: a `let`, so nothing it borrowed is held past it.
        let statement = admit[..classified].rsplit([';', '{', '}']).next().unwrap();
        assert!(statement.trim_start().starts_with("let "), "{statement:?}");
        let handler = scan::functions(&code, "handle_inbound_message");
        assert_eq!(handler.len(), 1);
        assert_eq!(scan::calls(handler[0], "ask_gate"), 0, "no arm asks the gate");
        assert_eq!(scan::calls(handler[0], "gate"), 0, "no arm calls a gate");

        let admission = scan::functions(&code, "admission");
        assert_eq!(admission.len(), 1);
        let flat: String = admission[0].split_whitespace().collect();
        for wildcard in ["{_=>", ",_=>", "}_=>", "|_=>"] {
            assert!(
                !flat.contains(wildcard),
                "every message kind is named, none falls through `_`"
            );
        }
    }
}

#[cfg(test)]
mod question_tests {
    //! Claude's question is answered the way a card is: only from the page that shows its tab, and
    //! only while the window agrees that page is not covered. A refusal leaves it waiting.
    use super::*;
    use crate::agent_panel::tests::*;

    fn asked(id: &str) -> agent::AgentDomainEvent {
        agent::AgentDomainEvent::QuestionAsked {
            interaction_id: id.into(),
            tool_use_id: Some(format!("toolu_{id}")),
            questions: vec![agent::QuestionSpec {
                question: "Which layout?".into(),
                header: "Layout".into(),
                multi_select: false,
                options: ["Grid", "List"]
                    .into_iter()
                    .map(|label| agent::QuestionOptionSpec {
                        label: label.into(),
                        description: String::new(),
                        preview: None,
                    })
                    .collect(),
            }],
            unsupported_reason: None,
        }
    }

    fn question_state(handle: &AgentPanelHandle, tab: TabId, id: &str) -> Option<agent::QuestionState> {
        let state = handle.state.borrow();
        let backend = state.tabs.get(tab)?.live()?;
        let found = backend.projection().question(id).map(|record| record.state.clone());
        found
    }

    fn question_answer(handle: &AgentPanelHandle, from: ViewId, request: &str, tab: TabId, id: &str) {
        handle.page_message(
            from,
            &format!(
                r#"{{"type":"question_answer","request_id":"{request}","tab":{},"interaction_id":"{id}","answers":[{{"question_index":0,"option_indexes":[1]}}]}}"#,
                tab.0
            ),
        );
    }

    fn question_decline(handle: &AgentPanelHandle, from: ViewId, request: &str, tab: TabId, id: &str, kind: &str) {
        handle.page_message(
            from,
            &format!(
                r#"{{"type":"question_decline","request_id":"{request}","tab":{},"interaction_id":"{id}","kind":"{kind}"}}"#,
                tab.0
            ),
        );
    }

    /// A covered page's answer and decline are refused by the window's gate: the page is told why (a
    /// failing `command_result`, on which the dock keeps its picks and typed text), nothing reaches the
    /// agent and the question waits. Uncovered, the same page's answer goes.
    #[test]
    fn a_question_answer_from_a_covered_view_is_refused_and_the_dock_keeps_its_draft() {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("views-question-covered");
        let (handle, _base, second) = handle_with_two_pages();
        let (tab, provider) = live_tab(&handle, &dir);
        assert_eq!(handle.break_out(view(2)), Ok(tab));
        provider.queue(asked("q1"));
        tick_handle_until(&handle, "the question", || question_state(&handle, tab, "q1").is_some());
        let covered = Rc::new(Cell::new(true));
        let asked_views = recording_gate(&handle, &[2], covered.clone());

        question_answer(&handle, view(2), "r1", tab, "q1");
        question_decline(&handle, view(2), "r2", tab, "q1", "cancel");
        for request in ["r1", "r2"] {
            assert!(
                matches!(result_for(&second.sent.borrow(), request), Some(Err(why)) if why.contains("under terminal 2")),
                "{request}"
            );
        }
        assert!(provider.interactions().is_empty(), "nothing reached the agent");
        assert_eq!(question_state(&handle, tab, "q1"), Some(agent::QuestionState::Waiting));
        assert_eq!(*asked_views.borrow(), vec![view(2), view(2)], "asked at each answer");

        covered.set(false);
        question_answer(&handle, view(2), "r3", tab, "q1");
        assert_eq!(
            result_for(&second.sent.borrow(), "r3"),
            None,
            "told once the agent replied"
        );
        handle.tick();
        assert_eq!(result_for(&second.sent.borrow(), "r3"), Some(Ok(())));
        assert_eq!(provider.interactions().len(), 1);
        assert_eq!(
            question_state(&handle, tab, "q1"),
            Some(agent::QuestionState::Answering)
        );
        shut_down_every_tab(&handle);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn editing_question_panel(
        label: &str,
    ) -> (
        AgentPanelHandle,
        Rc<RecordingPage>,
        TabId,
        std::sync::Arc<eitri_core::test_providers::RecordingProvider>,
    ) {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir(label);
        let (handle, base, _) = handle_with_two_pages();
        let (tab, provider) = live_tab(&handle, &dir);
        handle.state.borrow_mut().scratch = Some(eitri_core::scratch::ScratchDir::new().unwrap());
        provider.queue(asked("q1"));
        tick_handle_until(&handle, "question", || question_state(&handle, tab, "q1").is_some());
        handle.set_answer_gate(Rc::new(|_| panic!("editing a question answers nothing")));
        (handle, base, tab, provider)
    }

    #[test]
    fn question_draft_and_edit_question_other_are_ungated_and_ctrl_g_round_trips() {
        let (handle, page, tab, provider) = editing_question_panel("question-other-edit");
        let requests = Rc::new(RefCell::new(Vec::new()));
        handle.on_editor_request({
            let requests = requests.clone();
            move |request| {
                requests.borrow_mut().push(request.clone());
                Ok(())
            }
        });
        let before = page.sent.borrow().len();
        handle.page_message(
            ViewId::BASE,
            &serde_json::json!({
                "type":"question_draft", "request_id":"d", "tab":tab.0,
                "interaction_id":"q1", "draft":{"text":"typed"}
            })
            .to_string(),
        );
        assert_eq!(page.sent.borrow().len(), before, "a draft is never echoed");
        handle.page_message(
            ViewId::BASE,
            &serde_json::json!({
                "type":"edit_question_other", "request_id":"e", "tab":tab.0,
                "interaction_id":"q1", "question_index":0, "text":"typed"
            })
            .to_string(),
        );
        assert_eq!(requests.borrow().len(), 1);
        assert_eq!(result_for(&page.sent.borrow(), "e"), Some(Ok(())));
        let edit = handle.state.borrow().pending_edits[0].clone();
        assert_eq!(std::fs::read_to_string(&edit.body).unwrap().trim_end(), "typed");
        std::fs::write(
            &edit.body,
            "edited in nvim
",
        )
        .unwrap();
        std::fs::write(&edit.done, "written").unwrap();
        handle.tick();
        let edited: serde_json::Value = page
            .sent
            .borrow()
            .iter()
            .map(|s| json_of(s))
            .find(|v| v["kind"] == "question_other_edited")
            .unwrap();
        assert_eq!(edited["tab"], tab.0);
        assert_eq!(edited["interaction_id"], "q1");
        assert_eq!(edited["question_index"], 0);
        assert_eq!(edited["text"], "edited in nvim");
        assert_eq!(handle.state.borrow().tabs.get(tab).unwrap().draft(), "");
        assert!(provider.interactions().is_empty());
        assert_eq!(question_state(&handle, tab, "q1"), Some(agent::QuestionState::Waiting));
        assert!(!edit.body.exists() && !edit.done.exists());
        shut_down_every_tab(&handle);
    }

    #[test]
    fn edit_question_other_refused_when_detached() {
        let (handle, page, tab, provider) = editing_question_panel("question-other-detached");
        handle.page_message(
            ViewId::BASE,
            &serde_json::json!({
                "type":"edit_question_other", "request_id":"e", "tab":tab.0,
                "interaction_id":"q1", "question_index":0, "text":"typed"
            })
            .to_string(),
        );
        assert!(
            matches!(result_for(&page.sent.borrow(), "e"), Some(Err(why)) if why.contains("editor is not connected"))
        );
        assert!(handle.state.borrow().pending_edits.is_empty());
        assert!(handle.state.borrow().tabs.get(tab).unwrap().editing_draft.is_none());
        assert!(provider.interactions().is_empty());
        assert_eq!(question_state(&handle, tab, "q1"), Some(agent::QuestionState::Waiting));
        shut_down_every_tab(&handle);
    }

    /// Another page's answer is refused naming the tab, before the window is asked.
    #[test]
    fn a_question_answer_from_another_page_is_refused_before_the_window_is_asked() {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("views-question-other-page");
        let (handle, base, second) = handle_with_two_pages();
        let (tab, provider) = live_tab(&handle, &dir);
        assert_eq!(handle.break_out(view(2)), Ok(tab));
        provider.queue(asked("q1"));
        tick_handle_until(&handle, "the question", || question_state(&handle, tab, "q1").is_some());
        let asked_views = recording_gate(&handle, &[], Rc::new(Cell::new(false)));
        let number = handle.state.borrow().tabs.get(tab).unwrap().number;

        question_answer(&handle, ViewId::BASE, "r1", tab, "q1");
        question_decline(&handle, ViewId::BASE, "r2", tab, "q1", "chat");
        for request in ["r1", "r2"] {
            assert_eq!(
                result_for(&base.sent.borrow(), request),
                Some(Err(format!("tab {number} is not shown in this page")))
            );
        }
        assert!(asked_views.borrow().is_empty(), "refused before the window is asked");
        assert!(provider.interactions().is_empty());
        assert_eq!(question_state(&handle, tab, "q1"), Some(agent::QuestionState::Waiting));

        question_decline(&handle, view(2), "r3", tab, "q1", "chat");
        handle.tick();
        assert_eq!(result_for(&second.sent.borrow(), "r3"), Some(Ok(())));
        assert_eq!(provider.interactions().len(), 1);
        shut_down_every_tab(&handle);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The page's answer never waits for the agent on the GTK thread: the message returns with the
    /// answer out, the tick tells the page once the agent replied, and a reply that is a failure (a
    /// timeout, a lost connection) is a refusal that leaves the session running and the question
    /// waiting for another answer.
    #[test]
    fn a_slow_agent_never_holds_the_answer_and_its_failure_keeps_the_session() {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("views-question-slow-agent");
        let (handle, base, _second) = handle_with_two_pages();
        let (tab, provider) = live_tab(&handle, &dir);
        handle.state.borrow_mut().tabs.select(tab);
        provider.queue(asked("q1"));
        tick_handle_until(&handle, "the question", || question_state(&handle, tab, "q1").is_some());
        provider.hold_interaction_replies(true);

        question_answer(&handle, ViewId::BASE, "r1", tab, "q1");
        assert_eq!(provider.interactions().len(), 1, "the answer went out");
        assert_eq!(
            question_state(&handle, tab, "q1"),
            Some(agent::QuestionState::Answering)
        );
        handle.tick();
        assert_eq!(result_for(&base.sent.borrow(), "r1"), None, "no reply yet");

        provider.reply_to_held_interactions(|| Err(agent::ProviderError::Timeout));
        handle.tick();
        assert!(
            matches!(result_for(&base.sent.borrow(), "r1"), Some(Err(why)) if why.contains("waiting again")),
            "{:?}",
            result_for(&base.sent.borrow(), "r1")
        );
        assert_eq!(question_state(&handle, tab, "q1"), Some(agent::QuestionState::Waiting));
        assert!(handle.state.borrow().question_sends.is_empty());
        assert!(
            handle.state.borrow().tabs.get(tab).unwrap().live().is_some(),
            "the session goes on"
        );
        shut_down_every_tab(&handle);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An unknown decline kind or a malformed answer does not parse: nothing is answered, nothing is
    /// sent, and no default stands in for what the page meant.
    #[test]
    fn a_malformed_question_message_answers_nothing() {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("views-question-malformed");
        let (handle, base, _second) = handle_with_two_pages();
        let (tab, provider) = live_tab(&handle, &dir);
        handle.state.borrow_mut().tabs.select(tab);
        provider.queue(asked("q1"));
        tick_handle_until(&handle, "the question", || question_state(&handle, tab, "q1").is_some());
        question_decline(&handle, ViewId::BASE, "r1", tab, "q1", "later");
        for answers in [
            r#"[{"question_index":0,"option_indexes":["1"]}]"#,
            r#"[{"question_index":-1,"option_indexes":[0]}]"#,
            r#"[{"question_index":0}]"#,
            r#"[{"question_index":0,"option_indexes":[0],"label":"Grid"}]"#,
        ] {
            handle.page_message(
                ViewId::BASE,
                &format!(
                    r#"{{"type":"question_answer","request_id":"r2","tab":{},"interaction_id":"q1","answers":{answers}}}"#,
                    tab.0
                ),
            );
        }
        assert_eq!(result_for(&base.sent.borrow(), "r1"), None);
        assert_eq!(result_for(&base.sent.borrow(), "r2"), None);
        assert!(provider.interactions().is_empty());
        assert_eq!(question_state(&handle, tab, "q1"), Some(agent::QuestionState::Waiting));
        shut_down_every_tab(&handle);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
