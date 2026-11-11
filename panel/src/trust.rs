//! The workspace-trust question as the panel asks it: whose files a tab's question is about, a start held
//! until it is answered, the gate's events routed to the pages, the answers and `:trust` commands.

use super::*;

/// Seconds since the epoch, for when a project was trusted.
pub(super) fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The discovery profile of an agent: whose project files its trust question is about. A registered
/// agent's are the files its registration names. One no registration names (a tab saved while it was
/// registered) finds nothing; it still starts only after a `y`, which its start then refuses for want
/// of a command.
pub(super) fn trust_profile(registered: &[AgentRegistration], agent: &AgentId) -> Profile {
    match agent {
        AgentId::Claude => Profile::Claude,
        AgentId::Codex => Profile::Codex,
        AgentId::Acp(name) => Profile::Acp {
            name: name.clone(),
            files: registered
                .iter()
                .find(|a| a.name == *name)
                .map(|a| a.project_files.clone())
                .unwrap_or_default(),
        },
    }
}

/// The ACP command a registered agent's tab starts with, or `None` when nothing registered that name.
pub(super) fn acp_spec(registered: &[AgentRegistration], name: &str) -> Option<agent::providers::acp::AcpAgentSpec> {
    let registration = registered.iter().find(|a| a.name == name)?;
    Some(agent::providers::acp::AcpAgentSpec {
        name: registration.name.clone(),
        argv: registration.command.iter().map(std::ffi::OsString::from).collect(),
        auto_mode: registration.auto_mode.clone(),
        read_only_mode: registration.read_only_mode.clone(),
    })
}

/// The profile a tab's start waits on. A restore starts on its answer only the tabs of the agent it
/// waits for (Claude's, whenever it brings a Claude tab back), whichever agent the tab it waits in
/// runs; its other agents' tabs come back waiting for their own answers.
pub(super) fn waiting_profile(state: &AgentPanelState, tab: TabId) -> Profile {
    match state.tabs.get(tab) {
        Some(t) => match t.backend() {
            // A restore waits for the answer its own tabs start on: Claude's when it brings any
            // Claude tab back, else its first tab's agent's.
            TabBackend::AwaitingTrust(AwaitingTrust {
                start: DeferredStart::Restore { go },
                ..
            }) => trust_profile(&state.acp_agents, &go.trust_agent().unwrap_or(AgentId::Claude)),
            _ => trust_profile(&state.acp_agents, t.agent()),
        },
        None => Profile::Claude,
    }
}

/// Puts a session start off until the gate has checked the project for the tab's agent: the tab holds
/// what was asked for, and nothing is spawned or leased until the check (and, when it must, the user)
/// says how.
pub(super) fn gate_start(state: &mut AgentPanelState, tab: TabId, start: DeferredStart) -> Result<(), String> {
    state.tabs.defer_start(tab, start)?;
    let profile = waiting_profile(state, tab);
    state.trust.wait_for(tab, &profile);
    state.trust.begin_check(&profile);
    Ok(())
}

/// A start that will not happen, taken back as an `Escape` on its question would be: a first message
/// returns to the composer, a resume or a restore is dropped with a toast. `why` is said in the panel
/// and is the refused command's error.
pub(super) fn put_off_start(state: &mut AgentPanelState, tab: TabId, start: DeferredStart, why: &str) -> Outbox {
    state.resume_after.remove(&tab);
    let mut out = Outbox::default();
    let view = state.tabs.view_of(tab);
    out.push(Dest::Page(view), tabs_payload_recorded(state, view));
    out.push(Dest::Tab(tab), tab_notice(state, tab, why));
    match start {
        DeferredStart::FirstSend {
            request_id: sent,
            first_turn,
        } => return_first_message(state, tab, &sent, &first_turn, why, &mut out),
        DeferredStart::Resume { request_id: asked, .. } => {
            if let Some(asked_from) = start_answer_dest(state, tab, &asked) {
                out.push(asked_from, serialize_command_result_for_js(&asked, Err(why)));
            }
            out.toast = Some(why.to_string());
        }
        DeferredStart::Restore { go } => {
            put_off_restore(state, tab, go, why, &mut out);
            out.toast = Some(why.to_string());
        }
    }
    out
}

/// A notice about `tab`, for the page of its view. That page shows a notice over whichever tab is on
/// screen there, so one about a tab it does not show (a background tab of the base view) begins
/// `tab N: `, and is not read as being about the tab in front.
pub(super) fn tab_notice(state: &AgentPanelState, tab: TabId, text: &str) -> String {
    let in_front = state.tab_of_page(state.tabs.view_of(tab)) == Some(tab);
    let text = match state.tabs.get(tab) {
        Some(t) if !in_front => format!("tab {}: {text}", t.number),
        _ => text.to_string(),
    };
    eitri_core::agent_bridge::serialize_notice_for_js(&text)
}

/// A restore whose start will not happen keeps its sessions as failed tabs
/// ([`TabSet::keep_put_off_restore`]). The base page is shown its tabs again, and a view a kept tab
/// came back into is shown that tab.
pub(super) fn put_off_restore(state: &mut AgentPanelState, from: TabId, go: RestoreGo, why: &str, out: &mut Outbox) {
    let held_before = state.tabs.views();
    if !state.tabs.keep_put_off_restore(from, go, why) {
        state.restore_source = None;
    }
    switch_payloads(state, out);
    for view in state.tabs.views() {
        if held_before.contains(&view) {
            continue;
        }
        if let Some(tab) = state.tabs.tab_in(view) {
            shown_payloads(state, tab, out);
        }
    }
}

/// A first message whose start will not happen goes back to the composer, `why` being the refusal.
/// The refused send comes first: the page puts the message back in its box itself, keeping what was
/// typed there since and putting the message before it. Rust's copy of the draft is that same merge, so
/// the `draft` that follows (which replaces the box) holds both and loses neither.
pub(super) fn return_first_message(
    state: &mut AgentPanelState,
    tab: TabId,
    sent: &str,
    first_turn: &FirstTurn,
    why: &str,
    out: &mut Outbox,
) {
    let typed_since = state.tabs.get(tab).map(|t| t.draft().to_string()).unwrap_or_default();
    let draft = if typed_since.is_empty() {
        first_turn.typed.clone()
    } else {
        format!("{}\n{typed_since}", first_turn.typed)
    };
    state.tabs.set_draft(tab, &draft);
    // The refusal to the page that sent the message, which reports it with its text if it no longer
    // shows the tab; the draft to the page that shows the tab now, whose box takes the message back.
    if let Some(sent_from) = start_answer_dest(state, tab, sent) {
        out.push(sent_from, serialize_command_result_for_js(sent, Err(why)));
    }
    out.push(
        Dest::Tab(tab),
        eitri_core::agent_bridge::serialize_refused_draft_for_js(tab, &draft, sent),
    );
}

/// A tab waiting for the trust question, given what the gate knows now: started if the answer is
/// already known, asked if it is on screen and its question is not, left waiting otherwise. Does
/// nothing while its check or its record is still out.
pub(super) fn reroute_or_ask(state: &mut AgentPanelState, tab: TabId) -> Outbox {
    let mut out = Outbox::default();
    let asking = match state.tabs.get(tab).map(|t| t.backend()) {
        Some(TabBackend::AwaitingTrust(AwaitingTrust {
            recording: false,
            asking,
            ..
        })) => *asking,
        _ => return out,
    };
    if state.trust.is_waiting(tab) {
        return out;
    }
    let profile = waiting_profile(state, tab);
    match state.trust.current_route(&profile) {
        None => {}
        Some(Route::Start(project)) => {
            if let Some(start) = state.tabs.take_deferred(tab) {
                out.extend(start_deferred(state, tab, start, project));
            }
        }
        Some(Route::Ask) => {
            if asking.is_some_and(|nonce| state.trust.is_shown(tab, nonce)) {
                return out;
            }
            if state.page_of(tab).is_none() {
                // Asked when it is next on screen, in the page that shows it then.
                state.trust.hide(tab);
                return out;
            }
            let Ok(nonce) = state.tabs.ask_trust(tab) else {
                return out;
            };
            match state.trust.ask(tab, nonce, &profile) {
                Some(Asked::Prompt(message)) => out.push(Dest::Tab(tab), message),
                // Nothing in a prompt may be cut, so one too large to show refuses its start.
                Some(asked @ (Asked::TooLarge { .. } | Asked::Unavailable)) => {
                    if let Some(start) = state.tabs.take_deferred(tab) {
                        let why = match asked {
                            Asked::TooLarge { agent, bytes } => crate::trust_gate::too_large_to_start(&agent, bytes),
                            _ => crate::trust_gate::UNAVAILABLE_START.to_string(),
                        };
                        out.extend(put_off_start(state, tab, start, &why));
                    }
                }
                None => {}
            }
        }
    }
    out
}

/// [`reroute_or_ask`] for every waiting tab, in tab order.
pub(super) fn sweep_awaiting(state: &mut AgentPanelState) -> Outbox {
    let mut out = Outbox::default();
    for tab in state.tabs.awaiting_trust() {
        out.extend(reroute_or_ask(state, tab));
    }
    out
}

/// The tick's half of the gate: what its worker finished, acted on. Never blocks.
pub(super) fn trust_events(state: &mut AgentPanelState) -> Outbox {
    use eitri_core::agent_bridge::serialize_notice_for_js;
    let mut out = Outbox::default();
    for event in state.trust.poll() {
        match event {
            GateEvent::Route(tab, Route::Start(project)) => {
                if let Some(start) = state.tabs.take_deferred(tab) {
                    out.extend(start_deferred(state, tab, start, project));
                }
            }
            // Asked by the sweep below, which knows whether the tab is on screen.
            GateEvent::Route(_, Route::Ask) | GateEvent::Reask(_) => {}
            GateEvent::Recorded { tab, notice } => {
                if let Some(start) = state.tabs.take_recorded(tab) {
                    out.extend(start_deferred(state, tab, start, ProjectTrust::Trusted));
                    if let Some(notice) = notice {
                        out.push(Dest::Tab(tab), tab_notice(state, tab, &notice));
                    }
                }
            }
            GateEvent::RecordRefused { tab, notice } => {
                out.push(Dest::Tab(tab), tab_notice(state, tab, &notice));
                // The `y` is spent; the tab waits for a new answer, asked by the sweep below.
                if state.tabs.ask_trust(tab).is_ok() {
                    state.trust.hide(tab);
                }
            }
            GateEvent::CommandAsk {
                request_id: command,
                tab,
            } => {
                // Shown only in the page the command was typed in, while that page is open and still
                // shows the tab: a tab that moved to another page while the check ran must not take
                // the prompt with it, since that page never typed the command and could answer it.
                let in_its_page = match state.owed.get(&command) {
                    Some(OwedAnswer {
                        page: Some(_),
                        what: Owed::TrustCommand { typed_in, .. },
                        ..
                    }) => state.page_of(tab) == Some(*typed_in),
                    Some(_) => false,
                    None => state.page_of(tab).is_some(),
                };
                let shown = in_its_page.then(|| state.trust.ask_command(tab, &command)).flatten();
                match shown {
                    Some(Asked::Prompt(message)) => out.push(Dest::Tab(tab), message),
                    // Too large to show, or not ready: the command is over.
                    Some(asked @ (Asked::TooLarge { .. } | Asked::Unavailable)) => {
                        let why = match asked {
                            Asked::TooLarge { agent, bytes } => crate::trust_gate::too_large(&agent, bytes),
                            _ => crate::trust_gate::UNAVAILABLE_COMMAND.to_string(),
                        };
                        if let Some((typed_in, request_id)) = end_trust_command(state, &command) {
                            out.push(typed_in, serialize_notice_for_js(&why));
                            out.push(typed_in, serialize_command_result_for_js(&request_id, Err(&why)));
                        }
                    }
                    // The tab is not on screen in the page that typed it any more: `:trust` ends as
                    // not trusted on the next poll, in that page.
                    None => state.trust.drop_command_prompt(),
                }
            }
            GateEvent::CommandDone {
                request_id: command,
                ok,
                notice,
            } => {
                let Some((typed_in, request_id)) = end_trust_command(state, &command) else {
                    continue;
                };
                if let Some(notice) = &notice {
                    out.push(typed_in, serialize_notice_for_js(notice));
                }
                let error = notice.as_deref().unwrap_or(crate::trust_gate::NOT_TRUSTED);
                out.push(
                    typed_in,
                    serialize_command_result_for_js(&request_id, if ok { Ok(()) } else { Err(error) }),
                );
            }
        }
    }
    out.extend(sweep_awaiting(state));
    out
}

/// The command the gate knows as `command` is over: its record goes, and its result is owed to the
/// page it was typed in, under that page's own request id. `None` when that page has closed, or for a
/// command no record names.
pub(super) fn end_trust_command(state: &mut AgentPanelState, command: &str) -> Option<(Dest, String)> {
    if !matches!(
        state.owed.get(command).map(|answer| answer.what),
        Some(Owed::TrustCommand { .. })
    ) {
        eprintln!("[agent_panel] {command} ended, but no page's :trust is recorded for it");
        return None;
    }
    let ended = state.owed.take(command)?;
    Some((Dest::Page(ended.page?), ended.request_id))
}

/// `:trust` / `:untrust` from the `:` line of `view`'s page, about the agent of the tab that page
/// shows. `agent` is the agent the page believed that tab runs; a command typed for another agent's
/// tab (the page was behind a switch) is refused rather than applied to whichever agent is on screen
/// now. An extra page that shows no tab has no agent to trust.
pub(super) fn trust_command(
    state: &mut AgentPanelState,
    view: ViewId,
    request_id: &str,
    action: eitri_core::agent_bridge::TrustAction,
    agent: &str,
) -> Result<(), String> {
    let Some(shown) = state.tab_of_page(view).and_then(|tab| state.tabs.get(tab)) else {
        return Err(NO_TAB_HERE.to_string());
    };
    let (tab, running) = (shown.id, shown.agent().clone());
    if running.record_name() != agent {
        let command = match action {
            eitri_core::agent_bridge::TrustAction::Trust => ":trust",
            eitri_core::agent_bridge::TrustAction::Untrust => ":untrust",
        };
        let asked = AgentId::from_record_name(agent).map_or_else(|| format!("{agent:?}"), |a| a.label());
        return Err(format!(
            "that {command} was for a {asked} tab; this tab runs {}",
            running.label()
        ));
    }
    // Known to the gate by the ledger's key: two pages number their requests alike.
    let command = state
        .owed
        .owe(view, request_id, Owed::TrustCommand { tab, typed_in: view });
    state
        .trust
        .command(command, action, tab, &trust_profile(&state.acp_agents, &running));
    Ok(())
}

/// Drains the gate from the tick.
pub(super) fn poll_trust(state: &mut AgentPanelState, screen: &Screen, out: &mut Outbox) {
    let trust = trust_events(state).settled(state, screen);
    out.extend(trust);
}

/// `y` or `n` to a trust question, a tab's or `:trust`'s. Only checks and queues: a `y` starts its
/// session when the worker says the disk is unchanged since the prompt (and the record written, when
/// one is kept), never here.
// One parameter per field of the answer, and the view of the page that sent it.
#[allow(clippy::too_many_arguments)]
pub(super) fn answer_trust(
    state: &mut AgentPanelState,
    request_id: &str,
    tab: TabId,
    nonce: u64,
    fingerprint: &str,
    findings_digest: &str,
    trust: bool,
    view: ViewId,
) -> Outbox {
    use eitri_core::agent_bridge::serialize_notice_for_js;
    let mut out = Outbox::default();
    let here = Dest::Page(view);
    let now = now_unix();
    if state.tabs.check_trust_answer(tab, nonce, view).is_ok() {
        match state.trust.answer(tab, nonce, fingerprint, findings_digest, trust, now) {
            AnswerOutcome::Start(project, notice) => {
                let Ok(start) = state.tabs.take_answered(tab, nonce, view) else {
                    out.push(
                        here,
                        serialize_command_result_for_js(request_id, Err(crate::trust_gate::NOT_CURRENT)),
                    );
                    return out;
                };
                out.extend(start_deferred(state, tab, start, project));
                if let Some(notice) = notice {
                    out.push(here, serialize_notice_for_js(&notice));
                }
                out.push(here, serialize_command_result_for_js(request_id, Ok(())));
                // The window's answer may cover the other tabs waiting for it.
                out.extend(sweep_awaiting(state));
            }
            AnswerOutcome::Recording => {
                let marked = state.tabs.mark_recording(tab, nonce, view);
                out.push(
                    here,
                    serialize_command_result_for_js(request_id, marked.as_ref().map(|_| ()).map_err(String::as_str)),
                );
            }
            AnswerOutcome::Refused(why) => {
                out.push(here, serialize_notice_for_js(&why));
                out.push(here, serialize_command_result_for_js(request_id, Err(&why)));
                // Asked again with what is on disk now, or started if there is nothing left to ask.
                out.extend(reroute_or_ask(state, tab));
            }
            AnswerOutcome::NotStarted(why) => {
                let Ok(start) = state.tabs.take_answered(tab, nonce, view) else {
                    out.push(
                        here,
                        serialize_command_result_for_js(request_id, Err(crate::trust_gate::NOT_CURRENT)),
                    );
                    return out;
                };
                out.extend(put_off_start(state, tab, start, &why));
                // The answer itself was taken; it is the start that is refused.
                out.push(here, serialize_command_result_for_js(request_id, Ok(())));
            }
        }
        return out;
    }
    if state.tab_of_page(view) == Some(tab) && state.trust.is_command_prompt(tab, nonce) {
        match state
            .trust
            .answer_command(tab, nonce, fingerprint, findings_digest, trust, now)
        {
            Some(CommandAnswer::Done {
                request_id: command,
                ok,
                notice,
            }) => {
                if let Some(notice) = &notice {
                    out.push(here, serialize_notice_for_js(notice));
                }
                let error = notice.as_deref().unwrap_or(crate::trust_gate::NOT_TRUSTED);
                if let Some((typed_in, command)) = end_trust_command(state, &command) {
                    out.push(
                        typed_in,
                        serialize_command_result_for_js(&command, if ok { Ok(()) } else { Err(error) }),
                    );
                }
                out.push(here, serialize_command_result_for_js(request_id, Ok(())));
            }
            Some(CommandAnswer::Recording) => out.push(here, serialize_command_result_for_js(request_id, Ok(()))),
            None => out.push(
                here,
                serialize_command_result_for_js(request_id, Err(crate::trust_gate::NOT_CURRENT)),
            ),
        }
        return out;
    }
    out.push(
        here,
        serialize_command_result_for_js(request_id, Err(crate::trust_gate::NOT_CURRENT)),
    );
    out
}

/// Which trust question an answer is to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TrustAsked {
    /// The tab's own question, which holds its start.
    Tab,
    /// The prompt a `:trust` typed in the page asked.
    Command,
}

/// Which trust question an answer from `view`'s page is to, if that page may answer it: the tab's own
/// question it shows, or the `:trust` prompt typed in it. `None` is an answer [`answer_trust`] refuses
/// as no longer current. Changes nothing.
pub(super) fn trust_question_answered(
    state: &AgentPanelState,
    tab: TabId,
    nonce: u64,
    view: ViewId,
) -> Option<TrustAsked> {
    if state.tabs.check_trust_answer(tab, nonce, view).is_ok() {
        Some(TrustAsked::Tab)
    } else if state.tab_of_page(view) == Some(tab) && state.trust.is_command_prompt(tab, nonce) {
        Some(TrustAsked::Command)
    } else {
        None
    }
}

/// The refusal for a `:trust` prompt's answer the window's gate turned down: the command is over, and
/// the user typed it, so typing it again is its way back.
pub(super) fn trust_command_refused(why: &str) -> String {
    format!("{why} — :trust was not answered; type :trust again once it is uncovered")
}

/// A trust answer the window's gate refused (`why`, said to the page). A tab's own question stays
/// open: the page closed it when the key was pressed, so it is asked again in that page under a fresh
/// nonce, and the tab keeps waiting for an answer from an uncovered page. A `:trust` prompt ends the
/// command unrecorded (`not trusted`, on the next poll).
// One parameter per fact of the refused answer, and the view of the page that sent it.
#[allow(clippy::too_many_arguments)]
pub(super) fn refuse_trust_answer(
    state: &mut AgentPanelState,
    request_id: &str,
    tab: TabId,
    nonce: u64,
    asked: TrustAsked,
    why: &str,
    view: ViewId,
) -> Outbox {
    use eitri_core::agent_bridge::serialize_notice_for_js;
    let mut out = Outbox::default();
    let here = Dest::Page(view);
    match asked {
        TrustAsked::Tab => {
            out.push(here, serialize_notice_for_js(why));
            out.push(here, serialize_command_result_for_js(request_id, Err(why)));
            if state.tabs.check_trust_answer(tab, nonce, view).is_ok() {
                state.trust.hide(tab);
                out.extend(reroute_or_ask(state, tab));
            }
        }
        TrustAsked::Command => {
            let text = trust_command_refused(why);
            if state.trust.is_command_prompt(tab, nonce) {
                state.trust.drop_command_prompt();
            }
            out.push(here, serialize_notice_for_js(&text));
            out.push(here, serialize_command_result_for_js(request_id, Err(&text)));
        }
    }
    out
}

/// `Escape` on a trust question: the start is put off. A first message goes back to the composer; a
/// resume is dropped and the toast says so; a restore keeps its sessions (as failed tabs holding their
/// resume ids, or still on offer in an otherwise empty window). Nothing was leased, so nothing is released.
pub(super) fn cancel_trust_question(
    state: &mut AgentPanelState,
    request_id: &str,
    tab: TabId,
    nonce: u64,
    view: ViewId,
) -> Outbox {
    use crate::trust_gate::{NOT_CURRENT, NOT_STARTED, NOT_TRUSTED, PUT_OFF};
    let mut out = Outbox::default();
    let here = Dest::Page(view);
    if state.tabs.check_trust_answer(tab, nonce, view).is_err() {
        if state.tab_of_page(view) == Some(tab) {
            if let Some(command) = state.trust.cancel_command(tab, nonce) {
                if let Some((typed_in, command)) = end_trust_command(state, &command) {
                    out.push(typed_in, serialize_command_result_for_js(&command, Err(NOT_TRUSTED)));
                }
                out.push(here, serialize_command_result_for_js(request_id, Ok(())));
                return out;
            }
        }
        out.push(here, serialize_command_result_for_js(request_id, Err(NOT_CURRENT)));
        return out;
    }
    let Ok(start) = state.tabs.cancel_trust(tab, nonce, view) else {
        out.push(here, serialize_command_result_for_js(request_id, Err(NOT_CURRENT)));
        return out;
    };
    state.trust.hide(tab);
    let tabs_view = state.tabs.view_of(tab);
    out.push(Dest::Page(tabs_view), tabs_payload_recorded(state, tabs_view));
    match start {
        DeferredStart::FirstSend {
            request_id: sent,
            first_turn,
        } => return_first_message(state, tab, &sent, &first_turn, NOT_STARTED, &mut out),
        DeferredStart::Resume { request_id: asked, .. } => {
            if let Some(asked_from) = start_answer_dest(state, tab, &asked) {
                out.push(asked_from, serialize_command_result_for_js(&asked, Err(PUT_OFF)));
            }
            out.toast = Some(PUT_OFF.to_string());
        }
        DeferredStart::Restore { go } => {
            put_off_restore(state, tab, go, PUT_OFF, &mut out);
            out.toast = Some(PUT_OFF.to_string());
        }
    }
    out.push(here, serialize_command_result_for_js(request_id, Ok(())));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_panel::tests::*;

    /// A Codex tab, opened and on screen.
    fn codex_tab(state: &Rc<RefCell<AgentPanelState>>) -> TabId {
        let mut s = state.borrow_mut();
        let tab = s.tabs.open(AgentId::Codex);
        s.tabs.select(tab);
        tab
    }

    fn items_of_prompt(payloads: &[String]) -> Vec<String> {
        payloads
            .iter()
            .map(|p| json_of(p))
            .filter(|v| v["kind"] == "trust_prompt")
            .flat_map(|v| {
                v["items"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|item| item["file"].as_str().unwrap().to_string())
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    /// `:trust` and `:untrust` act on the agent of the tab on screen: in a Codex tab they ask about,
    /// record and forget Codex's configuration, never Claude's; one the page typed for another agent's
    /// tab is refused.
    #[test]
    fn trust_command_acts_on_the_active_tab_agent() {
        use eitri_core::agent_bridge::TrustAction;
        let (state, fx) = trust_panel("trust-command-agent");
        fx.write(PROJECT_SETTINGS, &session_start_hook("touch /tmp/claude"));
        fx.write("AGENTS.md", b"codex instructions\n");
        let tab = codex_tab(&state);
        let codex_record = PathBuf::from(format!("{}.codex", fx.record_file().display()));

        let refused = trust_command(
            &mut state.borrow_mut(),
            ViewId::BASE,
            "c0",
            TrustAction::Trust,
            "claude",
        );
        assert_eq!(
            refused,
            Err("that :trust was for a Claude tab; this tab runs Codex".to_string())
        );
        let refused = trust_command(
            &mut state.borrow_mut(),
            ViewId::BASE,
            "c0",
            TrustAction::Untrust,
            "claude",
        );
        assert_eq!(
            refused,
            Err("that :untrust was for a Claude tab; this tab runs Codex".to_string())
        );

        trust_command(&mut state.borrow_mut(), ViewId::BASE, "c1", TrustAction::Trust, "codex").unwrap();
        let seen = tick_until(&state, |seen| !prompts(&all_payloads(seen)).is_empty());
        let payloads = all_payloads(&seen);
        assert_eq!(
            items_of_prompt(&payloads),
            ["AGENTS.md"],
            "the Codex files, not Claude's"
        );
        let prompt = prompts(&payloads).pop().unwrap();
        assert_eq!(prompt.tab, tab);
        let out = answer_prompt(&state, "a1", &prompt, true);
        assert_eq!(result_for(&out.payloads(), "a1"), Some(Ok(())));
        let seen = tick_until(&state, |seen| result_for(&all_payloads(seen), "c1").is_some());
        assert_eq!(result_for(&all_payloads(&seen), "c1"), Some(Ok(())));
        assert!(codex_record.is_file());
        assert!(!fx.record_file().exists(), "Claude's record is untouched");
        assert!(state.borrow().trust.row(&Profile::Codex).starts_with("trusted since "));
        assert_eq!(state.borrow().trust.row(&Profile::Claude), "not checked yet");

        trust_command(
            &mut state.borrow_mut(),
            ViewId::BASE,
            "c2",
            TrustAction::Untrust,
            "codex",
        )
        .unwrap();
        let seen = tick_until(&state, |seen| result_for(&all_payloads(seen), "c2").is_some());
        assert_eq!(result_for(&all_payloads(&seen), "c2"), Some(Ok(())));
        assert!(!codex_record.exists());
    }

    /// A `y` at a folder others can write is not the folder's to act on: a rule or a hook in it would leave Codex's
    /// read-only sandbox with no card. The plan is an `n`'s, and the tab says the project is not loaded.
    #[test]
    fn a_codex_tab_at_a_root_that_is_no_boundary_starts_untrusted_after_a_yes() {
        use std::os::unix::fs::PermissionsExt;
        let (state, fx) = trust_panel("trust-no-boundary");
        fx.write(".codex/config.toml", b"model = \"x\"\n");
        std::fs::set_permissions(&fx.root, std::fs::Permissions::from_mode(0o777)).unwrap();
        let tab = codex_tab(&state);
        gate_start(&mut state.borrow_mut(), tab, first_send("r1", "hello codex")).unwrap();
        let prompt = next_prompt(&state);
        answer_prompt(&state, "a1", &prompt, true);
        tick_until(&state, |_| !fx.external.borrow().is_empty());

        let plan = fx.external.borrow()[0].plan.clone();
        assert!(
            plan.dirs.iter().all(|(_, level)| *level == agent::DirTrust::Untrusted),
            "{:?}",
            plan.dirs
        );
        assert!(plan.skills_off);
        let tiers = state.borrow().tabs.get(tab).unwrap().session.project_tiers;
        assert_eq!(tiers, Some(ProjectTrust::Untrusted));

        // A tab with no session yet says the same as one started: the answer on record does not count here.
        let empty = state.borrow_mut().tabs.open(AgentId::Codex);
        let s = state.borrow();
        let payload = crate::agent_panel::detail::detail_payload(&s, empty);
        assert!(payload.contains("this project's: not loaded"), "{payload}");
        assert!(
            payload.contains("not applied here, this folder is no boundary"),
            "{payload}"
        );
        drop(s);
        std::fs::set_permissions(&fx.root, std::fs::Permissions::from_mode(0o700)).unwrap();
    }

    /// A skill link read through lets the answer be kept, and a root that is no boundary still takes none: the
    /// record written for the first tab starts the second as untrusted too.
    #[test]
    fn a_codex_skill_link_read_through_does_not_trust_a_root_that_is_no_boundary() {
        use std::os::unix::fs::PermissionsExt;
        let (state, fx) = trust_panel("trust-no-boundary-link");
        fx.write(".codex/config.toml", b"model = \"x\"\n");
        fx.write(".claude/skills/vm/SKILL.md", b"skill\n");
        std::fs::create_dir_all(fx.root.join(".codex/skills")).unwrap();
        std::os::unix::fs::symlink("../../.claude/skills/vm", fx.root.join(".codex/skills/vm")).unwrap();
        std::fs::set_permissions(&fx.root, std::fs::Permissions::from_mode(0o777)).unwrap();
        let first = codex_tab(&state);
        gate_start(&mut state.borrow_mut(), first, first_send("r1", "hello codex")).unwrap();
        let prompt = next_prompt(&state);
        answer_prompt(&state, "a1", &prompt, true);
        tick_until(&state, |_| !fx.external.borrow().is_empty());

        let second = state.borrow_mut().tabs.open(AgentId::Codex);
        gate_start(&mut state.borrow_mut(), second, first_send("r2", "hello again")).unwrap();
        tick_until(&state, |_| fx.external.borrow().len() == 2);
        for start in fx.external.borrow().iter() {
            assert!(
                start
                    .plan
                    .dirs
                    .iter()
                    .all(|(_, level)| *level == agent::DirTrust::Untrusted),
                "{:?}",
                start.plan.dirs
            );
            assert!(start.plan.skills_off);
        }
        std::fs::set_permissions(&fx.root, std::fs::Permissions::from_mode(0o700)).unwrap();
    }

    /// The external start is handed the trust plan of the discovery its answer was bound to: a `y`
    /// trusts the directories that discovery walked and nothing above them, names the project's own
    /// tool servers, and keeps skills on; an `n` in another tab untrusts every directory up to `/`
    /// and turns skills off. Nothing goes through the Claude spawner.
    #[test]
    fn external_start_gets_the_plan_of_the_bound_discovery() {
        let (state, fx) = trust_panel("trust-external-plan");
        fx.write("AGENTS.md", b"codex instructions\n");
        fx.write(".codex/config.toml", b"[mcp_servers.srv]\ncommand = \"srv\"\n");
        let tab = codex_tab(&state);
        gate_start(&mut state.borrow_mut(), tab, first_send("r1", "hello codex")).unwrap();
        let prompt = next_prompt(&state);
        assert_eq!(prompt.tab, tab);
        assert!(fx.external.borrow().is_empty(), "nothing before the answer");
        answer_prompt(&state, "a1", &prompt, true);
        tick_until(&state, |_| !fx.external.borrow().is_empty());

        let started = fx.external.borrow()[0].clone();
        assert_eq!(started.agent, AgentId::Codex);
        assert_eq!(started.mode, SessionModeChoice::Auto);
        assert_eq!(started.resume, None);
        assert_eq!(started.editor_server, None, "a test binary has no bridge beside it");
        let plan = &started.plan;
        assert_eq!(
            plan.fingerprint, prompt.fingerprint,
            "the plan is the shown discovery's"
        );
        assert_eq!(plan.project_only, ["srv"]);
        assert!(!plan.skills_off);
        let root = fx.root.canonicalize().unwrap();
        let expected: Vec<(PathBuf, agent::DirTrust)> = root
            .ancestors()
            .map(|dir| {
                let level = if dir == root {
                    agent::DirTrust::Trusted
                } else {
                    agent::DirTrust::Untrusted
                };
                (dir.to_path_buf(), level)
            })
            .collect();
        assert_eq!(plan.dirs, expected);
        assert_eq!(plan.dirs.last().unwrap().0, PathBuf::from("/"));
        assert!(fx.spawned.borrow().is_empty(), "the Claude spawner is not used");
        {
            let s = state.borrow();
            match s.tabs.get(tab).unwrap().backend() {
                TabBackend::Starting(pending) => {
                    assert_eq!(pending.request_id, "r1");
                    assert_eq!(pending.first_turn.as_ref().unwrap().typed, "hello codex");
                }
                _ => panic!("the Codex tab connects once the record is written"),
            }
        }

        // `:untrust`, then an `n` in a second Codex tab: every directory untrusted, skills off.
        trust_command(
            &mut state.borrow_mut(),
            ViewId::BASE,
            "u1",
            eitri_core::agent_bridge::TrustAction::Untrust,
            "codex",
        )
        .unwrap();
        tick_until(&state, |seen| result_for(&all_payloads(seen), "u1").is_some());
        let second = codex_tab(&state);
        gate_start(&mut state.borrow_mut(), second, first_send("r2", "again")).unwrap();
        let prompt = next_prompt(&state);
        assert_eq!(prompt.tab, second);
        answer_prompt(&state, "n1", &prompt, false);
        assert_eq!(fx.external.borrow().len(), 2);
        let plan = fx.external.borrow()[1].plan.clone();
        assert_eq!(plan.fingerprint, prompt.fingerprint);
        assert!(plan.skills_off);
        assert!(plan.dirs.iter().all(|(_, level)| *level == agent::DirTrust::Untrusted));
        assert_eq!(plan.dirs.len(), root.ancestors().count());
    }

    /// A resume of a Codex session runs the gate again and is given a plan built from what is on disk
    /// now, not the one its first start had.
    #[test]
    fn resume_reasks_and_rebuilds_the_plan() {
        let (state, fx) = trust_panel("trust-external-resume");
        fx.write("AGENTS.md", b"first\n");
        let first = codex_tab(&state);
        gate_start(&mut state.borrow_mut(), first, first_send("r1", "hello")).unwrap();
        let prompt = next_prompt(&state);
        answer_prompt(&state, "a1", &prompt, true);
        tick_until(&state, |_| !fx.external.borrow().is_empty());
        let before = fx.external.borrow()[0].plan.clone();

        fx.write("AGENTS.md", b"changed since\n");
        let second = codex_tab(&state);
        let resume = DeferredStart::Resume {
            request_id: "r2".into(),
            provider_session_id: "thread-1".into(),
            resumed_title: None,
            resumed_name: None,
        };
        gate_start(&mut state.borrow_mut(), second, resume).unwrap();
        let again = next_prompt(&state);
        assert_eq!(again.tab, second, "the changed configuration is asked about again");
        assert_ne!(again.fingerprint, prompt.fingerprint);
        assert_eq!(fx.external.borrow().len(), 1, "nothing resumes before the answer");
        answer_prompt(&state, "a2", &again, true);
        tick_until(&state, |_| fx.external.borrow().len() == 2);
        let resumed = fx.external.borrow()[1].clone();
        assert_eq!(resumed.resume.as_deref(), Some("thread-1"));
        assert_eq!(resumed.plan.fingerprint, again.fingerprint);
        assert_ne!(resumed.plan.fingerprint, before.fingerprint);
        {
            let s = state.borrow();
            match s.tabs.get(second).unwrap().backend() {
                TabBackend::Starting(pending) => assert_eq!(pending.resume.as_deref(), Some("thread-1")),
                _ => panic!("the resume connects once the record is written"),
            }
        }

        // Unchanged since, a later resume is let through by the record without a question, and is
        // still given a plan of its own from a fresh check.
        let third = codex_tab(&state);
        let resume = DeferredStart::Resume {
            request_id: "r3".into(),
            provider_session_id: "thread-1".into(),
            resumed_title: None,
            resumed_name: None,
        };
        gate_start(&mut state.borrow_mut(), third, resume).unwrap();
        let seen = tick_until(&state, |_| fx.external.borrow().len() == 3);
        assert!(prompts(&all_payloads(&seen)).is_empty());
        assert_eq!(fx.external.borrow()[2].plan.fingerprint, again.fingerprint);
    }

    /// A resume `r` continues in the tab its session stopped in is started holding the wait for that tab's old
    /// backend, which keeps the session's lease until it is gone; a fresh start in a tab is never held by a
    /// wait left there.
    #[test]
    fn a_resume_continued_by_r_waits_for_the_old_backend() {
        let (state, fx) = trust_panel("trust-resume-waits");
        let started = |n: usize| {
            let seen = tick_until(&state, |seen| {
                fx.external.borrow().len() == n || !prompts(&all_payloads(seen)).is_empty()
            });
            if let Some(p) = prompts(&all_payloads(&seen)).pop() {
                answer_prompt(&state, "a", &p, true);
                tick_until(&state, |_| fx.external.borrow().len() == n);
            }
        };
        let tab = codex_tab(&state);
        let (_old_gone, gone) = mpsc::channel::<()>();
        state.borrow_mut().resume_after.insert(tab, ("thread-1".into(), gone));
        resume_in(&mut state.borrow_mut(), tab, "r1", "thread-1".into(), true).unwrap();
        started(1);
        let spawned = fx.external.borrow()[0].clone();
        assert_eq!(spawned.resume.as_deref(), Some("thread-1"));
        assert!(spawned.waits_for_old);
        assert!(state.borrow().resume_after.is_empty());

        let other = codex_tab(&state);
        let (_never, left) = mpsc::channel::<()>();
        state.borrow_mut().resume_after.insert(other, ("thread-1".into(), left));
        gate_start(&mut state.borrow_mut(), other, first_send("r2", "hello")).unwrap();
        started(2);
        assert!(!fx.external.borrow()[1].waits_for_old);
        assert!(state.borrow().resume_after.is_empty());

        // A resume of another session than the one the wait is for does not wait either.
        let third = codex_tab(&state);
        let (_never, left) = mpsc::channel::<()>();
        state.borrow_mut().resume_after.insert(third, ("thread-1".into(), left));
        resume_in(&mut state.borrow_mut(), third, "r3", "thread-9".into(), true).unwrap();
        started(3);
        assert_eq!(fx.external.borrow()[2].resume.as_deref(), Some("thread-9"));
        assert!(!fx.external.borrow()[2].waits_for_old);
        assert!(state.borrow().resume_after.is_empty());
    }

    /// No trust check result for the tab's agent (its worker never ran): the external start is
    /// dropped with the reason and the typed text goes back to the composer; nothing is spawned.
    #[test]
    fn an_external_start_without_a_bound_discovery_is_refused() {
        let (state, fx) = trust_panel("trust-external-unbound");
        state.borrow_mut().trust = TrustGate::without_worker(fx.root.clone());
        let tab = codex_tab(&state);
        gate_start(&mut state.borrow_mut(), tab, first_send("r1", "hello")).unwrap();
        let seen = tick_until(&state, |seen| result_for(&all_payloads(seen), "r1").is_some());
        let payloads = all_payloads(&seen);
        let why = "Codex was not started: the trust check of this project has not run";
        assert_eq!(result_for(&payloads, "r1"), Some(Err(why.to_string())));
        assert!(notices(&payloads).contains(&why.to_string()));
        assert!(fx.external.borrow().is_empty());
        assert!(matches!(
            state.borrow().tabs.get(tab).unwrap().backend(),
            TabBackend::NotStarted
        ));
    }

    /// A switch shows a tab whose resume waits for its trust question, and that question cannot be shown
    /// (its prompt was lost, as with one too large to show): the start is put off, and everything that
    /// says so goes out -- the toast too, not only what the base page is sent.
    #[test]
    fn a_start_a_switch_puts_off_says_so_in_full() {
        let (state, fx) = trust_panel("trust-switch-put-off");
        fx.write("AGENTS.md", b"codex instructions\n");
        let tab = {
            let mut s = state.borrow_mut();
            let shown = s.tabs.active();
            let tab = s.tabs.open(AgentId::Codex);
            s.tabs.select(shown);
            tab
        };
        resume_in(&mut state.borrow_mut(), tab, "r1", "thread-1".into(), false).unwrap();
        tick_until(&state, |_| {
            state.borrow().trust.current_route(&Profile::Codex) == Some(Route::Ask)
        });
        state.borrow_mut().trust.lose_prompts();
        let mut out = Outbox::default();
        {
            let mut s = state.borrow_mut();
            s.tabs.select(tab);
            send_switch(&mut s, &Screen::nothing_shown(), &mut out);
        }
        let toast = out.toast.clone().expect("the put-off resume's toast is kept");
        assert_eq!(toast, crate::trust_gate::UNAVAILABLE_START);
        assert!(notices(&out.payloads()).contains(&toast), "{:?}", out.payloads());
        assert!(fx.external.borrow().is_empty());
    }

    /// A notice about a tab its page does not show (a background tab of the base view) names the tab, so it
    /// is not read as being about the tab on screen; one about the tab on screen is as it was.
    #[test]
    fn a_notice_about_a_tab_not_on_screen_names_it() {
        let (state, _fx) = trust_panel("trust-notice-background");
        let mut s = state.borrow_mut();
        let shown = s.tabs.active();
        let background = s.tabs.open(AgentId::Claude);
        s.tabs.select(shown);
        let number = s.tabs.get(background).unwrap().number;
        for (tab, said) in [
            (background, format!("tab {number}: not started: no")),
            (shown, "not started: no".to_string()),
        ] {
            gate_start(&mut s, tab, first_send("r1", "hello")).unwrap();
            let start = s.tabs.take_deferred(tab).unwrap();
            let out = put_off_start(&mut s, tab, start, "not started: no");
            assert!(
                notices(&out.payloads()).contains(&said),
                "{said:?}: {:?}",
                out.payloads()
            );
        }
    }

    /// A project whose trust question would be larger than the page is ever sent (here one hook of
    /// 3 MiB) is not shown in part. Claude starts without the project's configuration on an `n`, so it is
    /// asked a short question that has no `y`: a `y` is refused and the question asked again, nothing is
    /// started or recorded, and an `n` starts the session untrusted. `:trust` is told the configuration
    /// is too large and shown nothing.
    #[test]
    fn a_trust_prompt_too_large_to_show_is_a_short_question_with_no_y() {
        let (state, fx) = trust_panel("trust-too-large");
        fx.write(PROJECT_SETTINGS, &session_start_hook(&"x".repeat(3 << 20)));
        let tab = state.borrow().tabs.active();
        gate_start(&mut state.borrow_mut(), tab, first_send("r1", "hello")).unwrap();
        let seen = tick_until(&state, |seen| !prompts(&all_payloads(seen)).is_empty());
        let raw = all_payloads(&seen)
            .into_iter()
            .find(|p| json_of(p)["kind"] == "trust_prompt")
            .unwrap();
        assert!(
            raw.len() < 4096,
            "no part of the configuration is sent: {} bytes",
            raw.len()
        );
        let prompt = prompts(&all_payloads(&seen)).pop().unwrap();
        assert_eq!(
            (prompt.state.as_str(), prompt.remember.as_str()),
            ("too_large", "window")
        );

        // A `y` is refused and the question asked again; nothing started or recorded.
        let out = answer_prompt(&state, "y1", &prompt, true);
        assert_eq!(
            result_for(&out.payloads(), "y1"),
            Some(Err(crate::trust_gate::Y_NOT_TAKEN.to_string()))
        );
        let again = prompts(&out.payloads());
        assert_eq!(again.len(), 1, "asked again: {:?}", out.payloads());
        assert_ne!(again[0].nonce, prompt.nonce);
        assert!(fx.spawned.borrow().is_empty() && !fx.record_file().exists());
        assert!(matches!(
            state.borrow().tabs.get(tab).unwrap().backend(),
            TabBackend::AwaitingTrust(_)
        ));

        // An `n` starts the session without the project's configuration.
        let out = answer_prompt(&state, "n1", &again[0], false);
        assert_eq!(result_for(&out.payloads(), "n1"), Some(Ok(())));
        assert!(notices(&out.payloads()).contains(&crate::trust_gate::NOT_LOADED.to_string()));
        assert_eq!(*fx.spawned.borrow(), [(None, ProjectTrust::Untrusted)]);
        assert!(!fx.record_file().exists());

        // `:trust` has nothing to trust: told, with nothing shown.
        use eitri_core::agent_bridge::TrustAction;
        trust_command(
            &mut state.borrow_mut(),
            ViewId::BASE,
            "c1",
            TrustAction::Trust,
            "claude",
        )
        .unwrap();
        let seen = tick_until(&state, |seen| result_for(&all_payloads(seen), "c1").is_some());
        let payloads = all_payloads(&seen);
        assert!(prompts(&payloads).is_empty());
        let refused = result_for(&payloads, "c1").unwrap().unwrap_err();
        assert!(refused.contains("too large to show whole"), "{refused}");
        assert!(!fx.record_file().exists());
    }

    /// The trust worker dies after a check of a registered agent's configuration came back: the next
    /// start is routed untrusted with that check's discovery still held, and is refused because such
    /// an agent starts only after a `y`. Nothing is spawned.
    #[test]
    fn an_untrusted_start_of_an_agent_that_needs_a_yes_is_refused() {
        let (state, fx) = trust_panel("trust-external-explicit-yes");
        state.borrow_mut().acp_agents = vec![gem_registration()];
        let agent = AgentId::Acp("gem".into());
        let profile = trust_profile(&state.borrow().acp_agents, &agent);
        state.borrow_mut().trust.begin_check(&profile);
        tick_until(&state, |_| state.borrow().trust.bound_discovery(&profile).is_some());
        state.borrow_mut().trust.drop_worker();
        let tab = {
            let mut s = state.borrow_mut();
            let tab = s.tabs.open(agent);
            s.tabs.select(tab);
            tab
        };
        gate_start(&mut state.borrow_mut(), tab, first_send("r1", "hello")).unwrap();
        let seen = tick_until(&state, |seen| result_for(&all_payloads(seen), "r1").is_some());
        let payloads = all_payloads(&seen);
        let why = crate::trust_gate::refused_without_yes(&profile);
        assert_eq!(result_for(&payloads, "r1"), Some(Err(why.clone())));
        assert!(notices(&payloads).contains(&why));
        assert!(fx.external.borrow().is_empty());
        assert!(fx.spawned.borrow().is_empty());
        assert!(matches!(
            state.borrow().tabs.get(tab).unwrap().backend(),
            TabBackend::NotStarted
        ));
    }

    /// A tab of the registered `gem`, opened and on screen.
    fn gem_tab(state: &Rc<RefCell<AgentPanelState>>) -> TabId {
        let mut s = state.borrow_mut();
        s.acp_agents = vec![gem_registration()];
        let tab = s.tabs.open(AgentId::Acp("gem".into()));
        s.tabs.select(tab);
        tab
    }

    fn prompt_json(payloads: &[String]) -> serde_json::Value {
        payloads
            .iter()
            .map(|p| json_of(p))
            .rfind(|v| v["kind"] == "trust_prompt")
            .expect("a trust prompt")
    }

    /// `:tabnew <name>` opens a tab of a registered agent; a name nothing registered is refused with
    /// the accepted words, the registered ones among them.
    #[test]
    fn registered_agent_offered_by_tabnew() {
        let state = state_for_hooks(TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto));
        let refused = open_tab_named(&mut state.borrow_mut(), ViewId::BASE, "gem").unwrap_err();
        assert_eq!(refused, "unknown agent gem — :tabnew accepts claude, codex");
        state.borrow_mut().acp_agents = vec![gem_registration()];
        open_tab_named(&mut state.borrow_mut(), ViewId::BASE, "gem").unwrap();
        assert_eq!(state.borrow().tabs.active_tab().agent(), &AgentId::Acp("gem".into()));
        let refused = open_tab_named(&mut state.borrow_mut(), ViewId::BASE, "other").unwrap_err();
        assert_eq!(refused, "unknown agent other — :tabnew accepts claude, codex, gem");
        assert!(open_tab_named(&mut state.borrow_mut(), ViewId::BASE, "acp:gem").is_err());
    }

    /// A registered agent in a root with none of its files is still asked about, with the question
    /// saying there are none and that `n` does not start it; a `y` records the empty configuration and
    /// starts the agent with its registered command and mode.
    #[test]
    fn acp_root_with_nothing_found_asks_and_records_the_empty_fingerprint() {
        let (state, fx) = trust_panel("acp-nothing-found");
        fx.write("CLAUDE.md", b"another agent's file, not gem's\n");
        let tab = gem_tab(&state);
        gate_start(&mut state.borrow_mut(), tab, first_send("r1", "hello")).unwrap();
        let seen = tick_until(&state, |seen| !prompts(&all_payloads(seen)).is_empty());
        let payloads = all_payloads(&seen);
        let shown = prompt_json(&payloads);
        assert_eq!(shown["agent"], "gem");
        assert_eq!(shown["state"], "nothing");
        assert_eq!(shown["needsYes"], true);
        assert_eq!(shown["items"], serde_json::json!([]));
        let prompt = prompts(&payloads).pop().unwrap();
        assert_eq!(prompt.tab, tab);
        assert!(fx.external.borrow().is_empty(), "nothing before the answer");

        let out = answer_prompt(&state, "a1", &prompt, true);
        assert_eq!(result_for(&out.payloads(), "a1"), Some(Ok(())));
        tick_until(&state, |_| !fx.external.borrow().is_empty());
        let record = PathBuf::from(format!("{}.acp-gem", fx.record_file().display()));
        let written: serde_json::Value = serde_json::from_slice(&std::fs::read(&record).unwrap()).unwrap();
        assert_eq!(written["fingerprint"], prompt.fingerprint.as_str());
        assert!(!fx.record_file().exists(), "Claude's record is untouched");
        let started = fx.external.borrow()[0].clone();
        assert_eq!(started.agent, AgentId::Acp("gem".into()));
        assert_eq!(
            started.acp,
            Some((
                vec![std::ffi::OsString::from("gem-acp"), std::ffi::OsString::from("--stdio")],
                Some("auto_edit".to_string())
            ))
        );
        assert!(
            fx.spawned.borrow().is_empty(),
            "nothing went through the Claude spawner"
        );
    }

    /// An `n` to a registered agent's question starts nothing and says why; it is not remembered, so the
    /// next start asks again.
    #[test]
    fn acp_n_refuses_with_the_notice() {
        let (state, fx) = trust_panel("acp-n-refuses");
        fx.write("GEM.md", b"gem instructions\n");
        let tab = gem_tab(&state);
        gate_start(&mut state.borrow_mut(), tab, first_send("r1", "hello")).unwrap();
        let seen = tick_until(&state, |seen| !prompts(&all_payloads(seen)).is_empty());
        let payloads = all_payloads(&seen);
        assert_eq!(prompt_json(&payloads)["state"], "untrusted");
        assert_eq!(prompt_json(&payloads)["needsYes"], true);
        assert_eq!(items_of_prompt(&payloads), ["GEM.md"]);
        let prompt = prompts(&payloads).pop().unwrap();

        let out = answer_prompt(&state, "a1", &prompt, false);
        assert_eq!(
            result_for(&out.payloads(), "a1"),
            Some(Ok(())),
            "the answer is taken; the start is refused"
        );
        let payloads = out.payloads();
        let why = "gem would load this project's own configuration, which you have not trusted; Eitri cannot \
                   start it without loading that";
        let profile = trust_profile(&state.borrow().acp_agents, &AgentId::Acp("gem".into()));
        assert_eq!(crate::trust_gate::refused_without_yes(&profile), why);
        assert_eq!(result_for(&payloads, "r1"), Some(Err(why.to_string())));
        assert!(notices(&payloads).contains(&why.to_string()), "{payloads:?}");
        assert!(fx.external.borrow().is_empty());
        assert!(fx.spawned.borrow().is_empty());

        gate_start(&mut state.borrow_mut(), tab, first_send("r2", "again")).unwrap();
        let again = next_prompt(&state);
        assert_eq!(again.tab, tab);
        assert!(fx.external.borrow().is_empty());
    }

    /// Trusted with nothing found, a registered agent starts without asking; once one of its project
    /// files appears, the fingerprint no longer matches and the next start asks again, naming it.
    #[test]
    fn a_file_appearing_later_reasks() {
        let (state, fx) = trust_panel("acp-file-later");
        let tab = gem_tab(&state);
        gate_start(&mut state.borrow_mut(), tab, first_send("r1", "hello")).unwrap();
        let prompt = next_prompt(&state);
        answer_prompt(&state, "a1", &prompt, true);
        tick_until(&state, |_| fx.external.borrow().len() == 1);

        let second = {
            let mut s = state.borrow_mut();
            let second = s.tabs.open(AgentId::Acp("gem".into()));
            s.tabs.select(second);
            second
        };
        gate_start(&mut state.borrow_mut(), second, first_send("r2", "hello")).unwrap();
        tick_until(&state, |_| fx.external.borrow().len() == 2);

        fx.write(".gem/settings.json", b"{\"tools\": \"all\"}\n");
        let third = {
            let mut s = state.borrow_mut();
            let third = s.tabs.open(AgentId::Acp("gem".into()));
            s.tabs.select(third);
            third
        };
        gate_start(&mut state.borrow_mut(), third, first_send("r3", "hello")).unwrap();
        let seen = tick_until(&state, |seen| !prompts(&all_payloads(seen)).is_empty());
        let payloads = all_payloads(&seen);
        assert_eq!(prompts(&payloads).pop().unwrap().tab, third);
        assert_eq!(prompt_json(&payloads)["state"], "changed");
        assert_eq!(fx.external.borrow().len(), 2, "the third start waits for the answer");
    }

    #[test]
    fn a_first_send_in_an_untrusted_root_spawns_nothing_until_answered() {
        let (state, fx) = trust_panel("trust-first-send");
        fx.write(PROJECT_SETTINGS, &session_start_hook("touch /tmp/never"));
        let tab = state.borrow().tabs.active();
        gate_start(&mut state.borrow_mut(), tab, first_send("r1", "hello")).unwrap();
        let prompt = next_prompt(&state);
        assert_eq!(prompt.tab, tab);
        assert_eq!(prompt.remember, "yes");
        assert!(fx.spawned.borrow().is_empty(), "nothing before the answer");

        let out = answer_prompt(&state, "a1", &prompt, true);
        assert_eq!(result_for(&out.payloads(), "a1"), Some(Ok(())));
        assert!(fx.spawned.borrow().is_empty(), "nothing before the record is written");
        assert!(matches!(
            state.borrow().tabs.get(tab).unwrap().backend(),
            TabBackend::AwaitingTrust(AwaitingTrust { recording: true, .. })
        ));

        tick_until(&state, |_| !fx.spawned.borrow().is_empty());
        assert_eq!(*fx.spawned.borrow(), [(None, ProjectTrust::Trusted)]);
        assert!(fx.record_file().is_file());
        let s = state.borrow();
        let t = s.tabs.get(tab).unwrap();
        assert_eq!(t.session.project_tiers, Some(ProjectTrust::Trusted));
        match t.backend() {
            TabBackend::Starting(pending) => {
                assert_eq!(pending.request_id, "r1");
                let first = pending
                    .first_turn
                    .as_ref()
                    .expect("the first message goes once connected");
                assert_eq!((first.wire.as_str(), first.typed.as_str()), ("wire: hello", "hello"));
            }
            _ => panic!("the tab connects once the record is written"),
        }
    }

    /// A resume by hand waits for the question like a first message, and an `n` resumes it without
    /// the project's tiers.
    #[test]
    fn a_resume_waits_for_the_question_and_n_resumes_it_untrusted() {
        let (state, fx) = trust_panel("trust-resume");
        fx.write(PROJECT_SETTINGS, &session_start_hook("touch /tmp/never"));
        let tab = state.borrow().tabs.active();
        let resume = DeferredStart::Resume {
            request_id: "r1".into(),
            provider_session_id: "s-1".into(),
            resumed_title: Some("about s-1".into()),
            resumed_name: None,
        };
        gate_start(&mut state.borrow_mut(), tab, resume).unwrap();
        let prompt = next_prompt(&state);
        assert!(fx.spawned.borrow().is_empty());
        let out = answer_prompt(&state, "n1", &prompt, false);
        assert_eq!(result_for(&out.payloads(), "n1"), Some(Ok(())));
        assert_eq!(
            *fx.spawned.borrow(),
            [(Some("s-1".to_string()), ProjectTrust::Untrusted)]
        );
        let s = state.borrow();
        match s.tabs.get(tab).unwrap().backend() {
            TabBackend::Starting(pending) => {
                assert_eq!(pending.resume.as_deref(), Some("s-1"));
                assert_eq!(pending.resumed_title.as_deref(), Some("about s-1"));
            }
            _ => panic!("the resume connects on the answer"),
        }
    }

    /// A check made for tab B (the hook changed meanwhile) takes A's prompt off; A's answer to what it
    /// was shown before is refused and A is asked about what is there now. Nothing is recorded and
    /// nothing starts.
    #[test]
    fn two_tabs_an_older_prompt_cannot_record_a_newer_discovery() {
        let (state, fx) = trust_panel("trust-two-tabs");
        fx.write(PROJECT_SETTINGS, &session_start_hook("touch /tmp/one"));
        let (a, b) = {
            let mut s = state.borrow_mut();
            let a = s.tabs.active();
            let b = s.tabs.open(eitri_core::tab_set::AgentId::Claude);
            s.tabs.select(a);
            (a, b)
        };
        gate_start(&mut state.borrow_mut(), a, first_send("ra", "to a")).unwrap();
        let first = next_prompt(&state);
        assert_eq!(first.tab, a);

        fx.write(PROJECT_SETTINGS, &session_start_hook("touch /tmp/two"));
        gate_start(&mut state.borrow_mut(), b, first_send("rb", "to b")).unwrap();
        let second = next_prompt(&state);
        assert_eq!(second.tab, a, "the tab on screen is asked again at once");
        assert_ne!(second.fingerprint, first.fingerprint);
        assert!(!state.borrow().trust.is_waiting(b));

        // The old prompt's nonce is spent.
        let out = answer_prompt(&state, "x1", &first, true);
        assert_eq!(
            result_for(&out.payloads(), "x1"),
            Some(Err(crate::trust_gate::NOT_CURRENT.to_string()))
        );
        // The current nonce, echoing what the old prompt showed: refused, and asked again.
        let stale_echo = Prompt {
            nonce: second.nonce,
            ..first.clone()
        };
        let out = answer_prompt(&state, "x2", &stale_echo, true);
        assert_eq!(
            result_for(&out.payloads(), "x2"),
            Some(Err(crate::trust_gate::OUT_OF_DATE.to_string()))
        );
        let again = prompts(&out.payloads());
        assert_eq!(again.len(), 1, "{:?}", out.payloads());
        assert_eq!(again[0].fingerprint, second.fingerprint);

        tick_until(&state, |_| !state.borrow().trust.is_busy());
        assert!(!fx.record_file().exists());
        assert!(fx.spawned.borrow().is_empty());
        for tab in [a, b] {
            assert!(matches!(
                state.borrow().tabs.get(tab).unwrap().backend(),
                TabBackend::AwaitingTrust(_)
            ));
        }
    }

    #[test]
    fn a_failed_record_is_said_in_the_panel_and_starts_nothing() {
        let (state, fx) = trust_panel("trust-record-fails");
        fx.write(PROJECT_SETTINGS, &session_start_hook("touch /tmp/never"));
        std::fs::create_dir_all(fx.state.join("eitri")).unwrap();
        std::fs::write(fx.state.join("eitri/trust"), b"not a directory\n").unwrap();
        let hold = crate::trust_gate::WorkerHold::default();
        let home = fx.root.parent().unwrap().to_path_buf();
        let store = eitri_core::project_trust::TrustStore::new(Some(fx.state.as_os_str()), Some(home.as_os_str()));
        state.borrow_mut().trust = TrustGate::new_held(store, fx.root.clone(), Some(home), hold.clone());
        let tab = state.borrow().tabs.active();
        gate_start(&mut state.borrow_mut(), tab, first_send("r1", "hello")).unwrap();
        let prompt = next_prompt(&state);

        hold.close();
        let out = answer_prompt(&state, "a1", &prompt, true);
        assert!(!hold.timed_out(), "the handler waited for the held worker");
        assert!(fx.spawned.borrow().is_empty());
        hold.open();
        assert_eq!(result_for(&out.payloads(), "a1"), Some(Ok(())));

        let seen = tick_until(&state, |seen| !prompts(&all_payloads(seen)).is_empty());
        let payloads = all_payloads(&seen);
        let said = notices(&payloads);
        assert!(
            said.iter().any(|n| n.starts_with("trust could not be recorded (")
                && n.ends_with("); asking again for this window only")),
            "{said:?}"
        );
        let asked = prompts(&payloads).pop().unwrap();
        assert_eq!(asked.remember, "window");
        assert!(fx.spawned.borrow().is_empty());
        assert!(!fx.record_file().exists());

        // The window-only `y` also waits for the worker to find the disk unchanged: the handler
        // starts nothing, the tick does, and says the trust lasts this window only.
        let out = answer_prompt(&state, "a2", &asked, true);
        assert_eq!(result_for(&out.payloads(), "a2"), Some(Ok(())));
        assert!(fx.spawned.borrow().is_empty(), "not started from the handler");
        let seen = tick_until(&state, |_| !fx.spawned.borrow().is_empty());
        assert_eq!(*fx.spawned.borrow(), vec![(None, ProjectTrust::Trusted)]);
        assert!(
            notices(&all_payloads(&seen))
                .iter()
                .any(|n| n.starts_with("trusted for this window only: ")),
            "{seen:?}",
            seen = all_payloads(&seen)
        );
        assert!(!fx.record_file().exists());
    }

    #[test]
    fn an_answer_for_another_tab_is_refused() {
        let (state, fx) = trust_panel("trust-other-tab");
        fx.write(PROJECT_SETTINGS, &session_start_hook("touch /tmp/never"));
        let (a, b) = {
            let mut s = state.borrow_mut();
            let a = s.tabs.active();
            let b = s.tabs.open(eitri_core::tab_set::AgentId::Claude);
            s.tabs.select(a);
            (a, b)
        };
        gate_start(&mut state.borrow_mut(), a, first_send("ra", "to a")).unwrap();
        let prompt = next_prompt(&state);
        // Named for the other tab: refused.
        let other = Prompt {
            tab: b,
            ..prompt.clone()
        };
        let out = answer_prompt(&state, "x1", &other, true);
        assert!(matches!(result_for(&out.payloads(), "x1"), Some(Err(_))));
        // The right tab, but no longer on screen: refused too.
        state.borrow_mut().tabs.select(b);
        let out = answer_prompt(&state, "x2", &prompt, true);
        assert!(matches!(result_for(&out.payloads(), "x2"), Some(Err(_))));
        tick_until(&state, |_| !state.borrow().trust.is_busy());
        assert!(fx.spawned.borrow().is_empty());
        assert!(!fx.record_file().exists());
    }

    /// A first message put off while the user typed on: the composer's text since the send (Rust's
    /// mirrored copy of the box) is kept after the message, as the page merges it, so the `draft` that
    /// follows the refusal replaces the box with both and loses neither.
    #[test]
    fn a_put_off_first_message_comes_back_with_what_was_typed_since() {
        let (state, fx) = trust_panel("trust-escape-typed-since");
        fx.write(PROJECT_SETTINGS, &session_start_hook("touch /tmp/never"));
        let tab = state.borrow().tabs.active();
        gate_start(&mut state.borrow_mut(), tab, first_send("r1", "hello")).unwrap();
        let prompt = next_prompt(&state);
        state.borrow_mut().tabs.set_draft(tab, "and more");
        let out = cancel_trust_question(&mut state.borrow_mut(), "c1", tab, prompt.nonce, ViewId::BASE);
        let drafts: Vec<_> = out
            .payloads()
            .iter()
            .map(|p| json_of(p))
            .filter(|v| v["kind"] == "draft")
            .collect();
        assert_eq!(drafts.len(), 1);
        assert_eq!(drafts[0]["text"], "hello\nand more");
        assert_eq!(
            drafts[0]["refused"], "r1",
            "it names the refusal it follows, so the page can skip it"
        );
        assert_eq!(state.borrow().tabs.get(tab).unwrap().draft(), "hello\nand more");
    }

    /// `Escape` on the question for a first message: the tab is empty again and the message is back in
    /// the box, once: the refused send comes before the draft, so the page does not put it back twice.
    #[test]
    fn escape_returns_the_first_message_to_the_composer() {
        let (state, fx) = trust_panel("trust-escape");
        fx.write(PROJECT_SETTINGS, &session_start_hook("touch /tmp/never"));
        let tab = state.borrow().tabs.active();
        gate_start(&mut state.borrow_mut(), tab, first_send("r1", "hello")).unwrap();
        let prompt = next_prompt(&state);
        let out = cancel_trust_question(&mut state.borrow_mut(), "c1", tab, prompt.nonce, ViewId::BASE);
        assert_eq!(result_for(&out.payloads(), "c1"), Some(Ok(())));
        assert_eq!(
            result_for(&out.payloads(), "r1"),
            Some(Err(crate::trust_gate::NOT_STARTED.to_string()))
        );
        let at = |kind: &str| out.payloads().iter().position(|p| json_of(p)["kind"] == kind).unwrap();
        let refused = out
            .payloads()
            .iter()
            .position(|p| json_of(p)["requestId"] == "r1")
            .unwrap();
        assert!(refused < at("draft"), "{:?}", out.payloads());
        let drafts: Vec<_> = out
            .payloads()
            .iter()
            .map(|p| json_of(p))
            .filter(|v| v["kind"] == "draft")
            .collect();
        assert_eq!(drafts.len(), 1);
        assert_eq!(drafts[0]["text"], "hello");
        assert_eq!(drafts[0]["refused"], "r1");
        let s = state.borrow();
        let t = s.tabs.get(tab).unwrap();
        assert!(matches!(t.backend(), TabBackend::NotStarted));
        assert_eq!(t.draft(), "hello");
        drop(s);

        // A resume put off is dropped with a toast.
        gate_start(
            &mut state.borrow_mut(),
            tab,
            DeferredStart::Resume {
                request_id: "r2".into(),
                provider_session_id: "s-1".into(),
                resumed_title: None,
                resumed_name: None,
            },
        )
        .unwrap();
        let prompt = next_prompt(&state);
        let out = cancel_trust_question(&mut state.borrow_mut(), "c2", tab, prompt.nonce, ViewId::BASE);
        assert_eq!(out.toast.as_deref(), Some(crate::trust_gate::PUT_OFF));
        assert_eq!(
            result_for(&out.payloads(), "r2"),
            Some(Err(crate::trust_gate::PUT_OFF.to_string()))
        );
        assert!(fx.spawned.borrow().is_empty());
    }

    #[test]
    fn an_extra_views_trust_question_survives_a_base_view_switch_and_its_y_starts_the_turn() {
        let TrustViews {
            handle,
            base,
            second,
            tab,
            fx,
            hold: _hold,
        } = trust_views("views-trust-switch");
        let first = handle.state.borrow().tabs.active();
        handle.state.borrow_mut().tabs.open(AgentId::Claude);
        let asked = recording_gate(&handle, &[], Rc::new(Cell::new(false)));
        clear(&[&base, &second]);
        handle.page_message(
            view(2),
            &format!(
                r#"{{"type":"send_message","request_id":"s1","tab":{},"text":"hello"}}"#,
                tab.0
            ),
        );
        poll_trust_until(&handle, "the question in page 2", || {
            !prompts(&second.sent.borrow()).is_empty()
        });
        let prompt = prompts(&second.sent.borrow()).pop().unwrap();
        assert_eq!(prompt.tab, tab);
        assert!(
            prompts(&base.sent.borrow()).is_empty(),
            "asked only in the page that shows it"
        );

        trust_answer_from(&handle, ViewId::BASE, "a1", &prompt, true);
        assert!(matches!(result_for(&base.sent.borrow(), "a1"), Some(Err(_))));
        assert!(awaiting_trust(&handle, tab));
        assert!(
            asked.borrow().is_empty(),
            "another page's answer never reaches the window"
        );

        // A switch in the base view drops the base view's questions only.
        handle.page_message(
            ViewId::BASE,
            &format!(r#"{{"type":"select_tab","request_id":"p1","tab":{}}}"#, first.0),
        );
        assert_eq!(handle.state.borrow().tabs.active(), first);
        assert!(handle
            .state
            .borrow()
            .tabs
            .check_trust_answer(tab, prompt.nonce, view(2))
            .is_ok());

        trust_answer_from(&handle, view(2), "a2", &prompt, true);
        assert_eq!(result_for(&second.sent.borrow(), "a2"), Some(Ok(())));
        assert_eq!(*asked.borrow(), vec![view(2)]);
        poll_trust_until(&handle, "the start", || !fx.spawned.borrow().is_empty());
        assert_eq!(*fx.spawned.borrow(), [(None, ProjectTrust::Trusted)]);
        let state = handle.state.borrow();
        match state.tabs.get(tab).unwrap().backend() {
            TabBackend::Starting(pending) => {
                assert_eq!(pending.request_id, "s1");
                assert_eq!(pending.first_turn.as_ref().unwrap().typed, "hello");
            }
            _ => panic!("page 2's tab connects once the record is written"),
        };
    }

    /// A `:trust` typed in the base page whose tab breaks out ends: the base page no longer shows that
    /// tab and cannot answer its prompt. One typed in page 2 whose tab joins back ends the same way.
    #[test]
    fn a_trust_command_ends_when_its_tab_leaves_the_page_it_was_typed_in() {
        let TrustViews {
            handle,
            base,
            second,
            tab,
            fx: _fx,
            hold: _hold,
        } = trust_views("views-trust-command-moves");
        let first = handle.state.borrow().tabs.active();
        handle.state.borrow_mut().tabs.open(AgentId::Claude);
        handle.state.borrow_mut().tabs.select(first);
        let third = attach_recording_page(&handle, view(3));
        handle.page_message(view(3), READY);

        trust_command_from(&handle, ViewId::BASE, "c1", "trust");
        poll_trust_until(&handle, "the :trust prompt in the base page", || {
            !prompts(&base.sent.borrow()).is_empty()
        });
        assert_eq!(handle.break_out(view(3)), Ok(first));
        poll_trust_until(&handle, "the command's end", || {
            result_for(&base.sent.borrow(), "c1").is_some()
        });
        assert!(matches!(result_for(&base.sent.borrow(), "c1"), Some(Err(_))));
        assert_eq!(result_for(&third.sent.borrow(), "c1"), None);

        clear(&[&base, &second, &third]);
        trust_command_from(&handle, view(2), "c2", "trust");
        poll_trust_until(&handle, "the :trust prompt in page 2", || {
            !prompts(&second.sent.borrow()).is_empty()
        });
        assert_eq!(handle.join_back(view(2)), Ok(tab));
        poll_trust_until(&handle, "the second command's end", || {
            result_for(&second.sent.borrow(), "c2").is_some()
        });
        assert!(matches!(result_for(&second.sent.borrow(), "c2"), Some(Err(_))));
        assert_eq!(result_for(&base.sent.borrow(), "c2"), None);
    }

    /// A `:trust` whose tab breaks out of the base page while the gate is still checking the project
    /// is never shown in the page the tab moved to: that page never typed it. It ends, unrecorded, in
    /// the page it was typed in.
    #[test]
    fn a_trust_prompt_whose_tab_broke_out_before_the_check_returned_is_never_shown() {
        let TrustViews {
            handle,
            base,
            second: _second,
            tab: _tab,
            fx,
            hold,
        } = trust_views("views-trust-command-moves-early");
        let first = handle.state.borrow().tabs.active();
        handle.state.borrow_mut().tabs.open(AgentId::Claude);
        handle.state.borrow_mut().tabs.select(first);
        let third = attach_recording_page(&handle, view(3));
        handle.page_message(view(3), READY);

        // The worker cannot finish the command's check until the tab has moved.
        hold.close();
        trust_command_from(&handle, ViewId::BASE, "c1", "trust");
        assert_eq!(handle.break_out(view(3)), Ok(first));
        hold.open();
        poll_trust_until(&handle, "c1's end", || result_for(&base.sent.borrow(), "c1").is_some());

        assert!(matches!(result_for(&base.sent.borrow(), "c1"), Some(Err(_))));
        assert!(prompts(&third.sent.borrow()).is_empty(), "{:?}", third.sent.borrow());
        assert_eq!(result_for(&third.sent.borrow(), "c1"), None);
        assert!(!fx.record_file().exists(), "nothing recorded");
    }

    /// Two pages number their requests alike: a `:untrust` from page 2 and a `:trust` from the base
    /// page, both `c1` and both out at once, are each answered in the page that typed it.
    #[test]
    fn a_trust_and_an_untrust_out_at_once_from_two_pages_are_each_answered_in_their_own_page() {
        let TrustViews {
            handle,
            base,
            second,
            fx: _fx,
            hold,
            ..
        } = trust_views("views-trust-two-commands");
        poll_trust_until_idle(&handle);
        hold.close();
        trust_command_from(&handle, view(2), "c1", "untrust");
        trust_command_from(&handle, ViewId::BASE, "c1", "trust");
        hold.open();
        poll_trust_until(&handle, "the :untrust's result and the :trust prompt", || {
            result_for(&second.sent.borrow(), "c1").is_some() && !prompts(&base.sent.borrow()).is_empty()
        });
        assert!(result_for(&second.sent.borrow(), "c1").is_some());
        assert_eq!(
            result_for(&base.sent.borrow(), "c1"),
            None,
            "the base page's :trust waits for its answer"
        );

        let prompt = prompts(&base.sent.borrow()).pop().unwrap();
        clear(&[&base, &second]);
        trust_answer_from(&handle, ViewId::BASE, "n1", &prompt, false);
        poll_trust_until(&handle, "the :trust's result", || {
            result_for(&base.sent.borrow(), "c1").is_some()
        });
        assert!(matches!(result_for(&base.sent.borrow(), "c1"), Some(Err(_))));
        assert_eq!(
            result_for(&second.sent.borrow(), "c1"),
            None,
            "page 2's c1 was answered once"
        );
        assert!(handle.state.borrow().owed.is_empty(), "no record is left behind");
    }
}
