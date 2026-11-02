//! Every way a tab leaves this panel with its session: the terminal handoff, collected when its close
//! reports, and the move to the other window (`PanelTabHandoff`, built and taken up here).

use super::*;

/// The backstop on a terminal-handoff worker's close, once the window that started it is gone.
///
/// A separate number from `SESSION_CLOSE_BACKSTOP` although the two now bound the same SHAPE of
/// work: this one bounds a worker somebody else already started, that one a teardown started on
/// the close path itself, so a future reason to move either has nothing to do with the other.
/// Sharing one number would make the next change to it silently change the other.
///
/// It is the legacy backend's real close time with room to spare (~0.8s of grace periods plus
/// three thread joins) and is knowingly shorter than the sidecar path's documented worst case --
/// this backstop's expiry only releases the application hold, it does not kill anything, so
/// "knowingly short" here means Eitri's own process may exit before a still-reaping sidecar
/// finishes its cleanup, not that anything gets SIGKILLed early; `AgentPanelHandle::shutdown`'s
/// handoff branch says what expiring costs.
pub(super) const HANDOFF_CLOSE_BACKSTOP: std::time::Duration = std::time::Duration::from_secs(3);

/// What the frontend is told when the close worker died inside `shutdown()`. One constant because
/// it is both logged and dispatched, and the two drifting apart would make a log line unmatchable
/// to what the user saw.
pub(super) const HANDOFF_CLOSE_FAILED_MESSAGE: &str =
    "the handoff could not finish closing this session; it is no longer running here";

/// One turn of a tab's review hint, as it crosses to the other window: the tab it names is renumbered
/// there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct MovedHint {
    pub(super) turn: u32,
    pub(super) files: usize,
}

/// What the panel keeps for a tab beyond the tab set, as the tab moves to the other window: its
/// status-band hints, shown or not yet shown, which the page it lands in is owed as a reloaded document
/// would be, and the bridge socket its agent's tool server connects to. Not moved: a scratch edit out
/// in the editor (the editor's window sends its draft to the tab's holder) and review jobs on workers
/// (they finish where they run and report to the holder). A tab with a start in flight, an `r`
/// continuing a session in it, or a review request of its page not yet answered does not move; each
/// lasts seconds at most.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PanelTabHandoff {
    pub following: bool,
    pub(super) shown_hints: Vec<MovedHint>,
    pub(super) review_hints: Vec<MovedHint>,
    pub(super) editor_bridge: Option<editor_glue::TabBridge>,
}

impl PanelTabHandoff {
    /// The record as it crosses the link.
    pub fn to_value(&self) -> serde_json::Value {
        let hints = |hints: &[MovedHint]| {
            hints
                .iter()
                .map(|h| serde_json::json!({ "turn": h.turn, "files": h.files }))
                .collect::<Vec<_>>()
        };
        let bridge = self.editor_bridge.as_ref().map(|bridge| {
            serde_json::json!({
                "name": { "hex": bridge.name.hex, "key": bridge.name.key },
                "tools": bridge.tools,
            })
        });
        serde_json::json!({
            "following": self.following,
            "shown_hints": hints(&self.shown_hints),
            "review_hints": hints(&self.review_hints),
            "editor_bridge": bridge,
        })
    }

    /// The record read back; anything but the shape [`Self::to_value`] writes is refused.
    pub fn from_value(value: &serde_json::Value) -> Result<PanelTabHandoff, String> {
        let hints = |key: &str| -> Result<Vec<MovedHint>, String> {
            let list = value
                .get(key)
                .and_then(serde_json::Value::as_array)
                .ok_or_else(|| format!("the panel's record has no `{key}` list"))?;
            list.iter()
                .map(|hint| {
                    let turn = hint.get("turn").and_then(serde_json::Value::as_u64);
                    let files = hint.get("files").and_then(serde_json::Value::as_u64);
                    match (
                        turn.and_then(|t| u32::try_from(t).ok()),
                        files.and_then(|f| usize::try_from(f).ok()),
                    ) {
                        (Some(turn), Some(files)) => Ok(MovedHint { turn, files }),
                        _ => Err(format!("a hint in `{key}` is not a turn and a file count")),
                    }
                })
                .collect()
        };
        Ok(PanelTabHandoff {
            following: value
                .get("following")
                .and_then(serde_json::Value::as_bool)
                .ok_or_else(|| "the panel's record has no boolean `following`".to_string())?,
            shown_hints: hints("shown_hints")?,
            review_hints: hints("review_hints")?,
            editor_bridge: moved_bridge(value)?,
        })
    }
}

/// The record's bridge. Its name goes into a file name here and its tools become what the session may call, so
/// both are read as strictly as the rest of the record.
fn moved_bridge(value: &serde_json::Value) -> Result<Option<editor_glue::TabBridge>, String> {
    let bridge = value
        .get("editor_bridge")
        .ok_or_else(|| "the panel's record has no `editor_bridge`".to_string())?;
    if bridge.is_null() {
        return Ok(None);
    }
    let word = |key: &str| bridge["name"][key].as_str().map(str::to_owned);
    let (Some(hex), Some(key)) = (word("hex"), word("key")) else {
        return Err("the record's bridge has no name".to_string());
    };
    let name = editor_glue::BridgeName { hex, key };
    name.check().map_err(|why| format!("the record's bridge: {why}"))?;
    let tools = bridge["tools"]
        .as_array()
        .ok_or_else(|| "the record's bridge has no tool list".to_string())?
        .iter()
        .map(|tool| tool.as_str().map(str::to_owned))
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| "a tool in the record's bridge is not a name".to_string())?;
    Ok(Some(editor_glue::TabBridge { name, tools }))
}

/// The panel's record of `tab` for a move, or why the tab does not move now. Every field of the panel's
/// state is named: one added later does not compile here until it is placed in the record or named as
/// not the tab's.
pub(super) fn panel_tab_handoff(state: &AgentPanelState, tab: TabId) -> Result<PanelTabHandoff, String> {
    let AgentPanelState {
        // The tab's own state beyond the tab set.
        shown_hints,
        review_hints,
        editor_bridge_tabs,
        // Refusals: each is the tab's for a few seconds at most.
        owed,
        resume_after,
        // Named, not moved: a scratch edit is out in the editor, whose window sends the draft to the
        // tab's holder; a review job finishes where it runs and reports to the holder.
        pending_edits: _,
        task_commands: _,
        review_jobs: _,
        // A chat whose answer's reply is unread refuses to move (the provider's freeze), and the
        // reply is told to the page that answered here.
        question_sends: _,
        // The tab itself moves through the tab set's own record.
        tabs,
        // The window's, not the tab's.
        path_menu: _,
        // The window's: a held message answers to the page that sent it.
        mentions: _,
        mention_sections: _,
        document: _,
        host: _,
        backend_kind: _,
        project_dir: _,
        canonical_project_dir: _,
        prefs_dir: _,
        editor_context: _,
        supervisor: _,
        supervisor_pending: _,
        shutting_down: _,
        wake: _,
        last_tick: _,
        #[cfg(any(test, feature = "test-support"))]
            ticks_run: _,
        theme: _,
        keymap_help: _,
        root_note: _,
        pages: _,
        tabs_recorded: _,
        view_empty_hook: _,
        show_view_hook: _,
        emptied_views: _,
        empty_views_judged: _,
        chooser_view: _,
        typing_cadence: _,
        editor_has_keys: _,
        thinking_display: _,
        hint_hook: _,
        answer_gate: _,
        coverage: _,
        attention_hook: _,
        busy_hook: _,
        reported_activity: _,
        reported_turns: _,
        reported_tabs: _,
        scratch: _,
        proposal_jobs: _,
        view_jobs: _,
        // A failed tab's `r` waits for the window it was pressed in; the lookup is the window's.
        reset_checks: _,
        cli_missing: _,
        // The window's moves in flight; the record is what one of them builds.
        chat_moves: _,
        hand_overs: _,
        review_hint: _,
        review_enabled: _,
        review_roots: _,
        editor_request_hook: _,
        editor_rpc: _,
        // The window's: the broker's lines are per tab but end with the tab's leaving (they are not carried).
        editor_tools: _,
        editor_follower: _,
        // Asked by the window a tab arrived in, for that window's hold; the next window asks again.
        client_call_reads: _,
        editor_switches: _,
        editor_way: _,
        // The window's listener: what crosses is the tab's socket by name, and the other window binds it.
        editor_bridge: _,
        #[cfg(test)]
            bridge_beside: _,
        home: _,
        review_flow: _,
        review_editor: _,
        pair_outbound_hook: _,
        editor_done_hook: _,
        edit_ask: _,
        edit_selection_hook: _,
        choose_project_hook: _,
        retiring: _,
        history_writer: _,
        rules_dir: _,
        history: _,
        editor_link: _,
        tab_verb_hook: _,
        nav_fallthrough_hook: _,
        keymap_closed_hook: _,
        tab_memory: _,
        titles: _,
        hello_titles_stale: _,
        chooser_listed: _,
        chooser_titles_stale: _,
        conversation_id: _,
        restore_source: _,
        restore_policy: _,
        tab_files: _,
        group_tabs: _,
        launch_restore_held: _,
        restore_cut_note: _,
        launch_restore_owed: _,
        bypass_by_config: _,
        mode_remembered: _,
        default_agent: _,
        codex_command: _,
        acp_agents: _,
        start_notes: _,
        restore_run: _,
        toast_hook: _,
        warning_hook: _,
        startup_problems: _,
        trust: _,
        spawner: _,
    } = state;
    if tabs.get(tab).is_none() {
        return Err(format!("no tab {}", tab.0));
    }
    let page = tabs.view_of(tab);
    let owes = |what: &dyn Fn(&OwedAnswer) -> bool| owed.owed.values().any(what);
    if owes(&|answer| answer.what == Owed::StartPage(tab)) {
        return Err("its session is still starting".to_string());
    }
    if resume_after.contains_key(&tab) {
        return Err("a session is being continued in it".to_string());
    }
    if owes(&|answer| answer.what == Owed::ReviewRequest && answer.page == Some(page)) {
        return Err("a review its page asked for is still being worked out".to_string());
    }
    let of_tab = |hints: &[eitri_core::turn_review::ReviewHint]| {
        hints
            .iter()
            .filter(|hint| hint.tab == tab.0)
            .map(|hint| MovedHint {
                turn: hint.turn,
                files: hint.files,
            })
            .collect()
    };
    Ok(PanelTabHandoff {
        following: tabs.get(tab).ok_or_else(|| format!("no tab {}", tab.0))?.following,
        shown_hints: of_tab(shown_hints),
        review_hints: of_tab(review_hints),
        editor_bridge: editor_bridge_tabs.get(&tab).cloned(),
    })
}

/// Takes up the panel's record of a tab another window moved here, now `tab`: every hint it carries is
/// owed to the page that shows it, which is sent them once it is ready, and its agent's tool server finds its
/// socket here under the same name.
pub(super) fn adopt_panel_tab(state: &mut AgentPanelState, tab: TabId, handoff: PanelTabHandoff) {
    let PanelTabHandoff {
        following,
        shown_hints,
        review_hints,
        editor_bridge,
    } = handoff;
    if let Some(t) = state.tabs.get_mut(tab) {
        t.following = following;
    }
    if let Some(bridge) = editor_bridge {
        editor_glue::tab_brought(state, tab, bridge);
    }
    let hints: Vec<eitri_core::turn_review::ReviewHint> = shown_hints
        .into_iter()
        .chain(review_hints)
        .map(|hint| eitri_core::turn_review::ReviewHint {
            tab: tab.0,
            turn: hint.turn,
            files: hint.files,
        })
        .collect();
    keep_newest_hint_per_tab(&mut state.review_hints, hints);
}

/// Every tab's in-flight terminal handoff, checked without blocking.
///
/// The command is dispatched here and nowhere else, which is what makes the handoff ordering real: by
/// the time the frontend can show a user the line to run, the session's own `shutdown()` has
/// already returned on the worker thread. The handoff card goes out only if its tab is on screen, to
/// the page that shows it (the tab keeps it in `last_handoff` for when it is shown); the
/// `command_result` is owed either way, and the tick's `tabs` envelope reports the state change.
pub(super) fn collect_pending_handoffs(state: &mut AgentPanelState, out: &mut Outbox) {
    let handing_off: Vec<TabId> = state
        .tabs
        .tabs()
        .iter()
        .filter(|t| t.pending_handoff.is_some())
        .map(|t| t.id)
        .collect();
    for tab in handing_off {
        let shown = state.shown_in(tab);
        let Some(t) = state.tabs.get_mut(tab) else { continue };
        let Some(pending) = t.pending_handoff.as_ref() else {
            continue;
        };
        let outcome = classify_close_signal(&pending.closed_rx);
        if outcome == HandoffCloseOutcome::StillClosing {
            continue;
        }
        let pending = t.pending_handoff.take().expect("checked Some above");
        t.session.reported_start_failure = false;
        t.session.turn_trace = None;
        if outcome == HandoffCloseOutcome::Closed {
            // Into the Rust host, not only down the wire: the panel's own reload affordance
            // would otherwise destroy the only copy, and on the default legacy backend nothing
            // else anywhere remembers this session id.
            t.last_handoff = Some(pending.command.clone());
        }
        // The page that asked, unless it has closed.
        let asked = state
            .owed
            .take_where(|answer| answer.what == Owed::Handoff(tab))
            .and_then(|answer| answer.page);
        // The handed-off session's shutdown has returned (or its worker died): the set counted it as
        // retiring when it took it.
        state.tabs.retire_done(tab);
        if outcome == HandoffCloseOutcome::CloseFailed {
            eprintln!("[agent_panel] tab {}: {HANDOFF_CLOSE_FAILED_MESSAGE}", tab.0);
        }
        match shown {
            Some(view) if Some(view) == asked => out.send_all(
                view,
                handoff_payloads(tab, &outcome, Some(&pending.request_id), &pending.command),
            ),
            // The tab moved, or is in the background: the page showing it gets the card or the error,
            // and the page that asked gets the answer, which ends its wait for the close wherever the
            // tab is now. Without it that page would keep the tab's composer disabled as closing.
            _ => {
                if let Some(view) = shown {
                    out.send_all(view, handoff_payloads(tab, &outcome, None, &pending.command));
                }
                let result = match outcome {
                    HandoffCloseOutcome::CloseFailed => Err(HANDOFF_CLOSE_FAILED_MESSAGE),
                    _ => Ok(()),
                };
                if let Some(asked) = asked {
                    out.send(asked, serialize_command_result_for_js(&pending.request_id, result));
                }
            }
        }
    }
    // A handoff that ended another way (its tab was removed with it) owes nothing.
    let tabs = &state.tabs;
    state.owed.retain(|_, answer| match answer.what {
        Owed::Handoff(tab) => tabs.get(tab).is_some_and(|t| t.pending_handoff.is_some()),
        _ => true,
    });
}

/// What one tick of the handoff collector found, as a value rather than as control flow.
///
/// Split out because this is the sequencing the whole feature's honesty rests on -- the card says
/// "This conversation is closed in Eitri", which is true only because the command is not released
/// until `AgentBackend::shutdown()` has returned -- and a property that load-bearing should be
/// assertable without a `WebView`, a GTK loop or a real backend.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum HandoffCloseOutcome {
    /// `shutdown()` has not returned yet. Nothing at all is dispatched in this state.
    StillClosing,
    /// The worker sent, which it does only after `shutdown()` returned.
    Closed,
    /// The worker's sender dropped without a send: it panicked inside `shutdown()`.
    CloseFailed,
}

pub(super) fn classify_close_signal(closed_rx: &mpsc::Receiver<()>) -> HandoffCloseOutcome {
    match closed_rx.try_recv() {
        Err(mpsc::TryRecvError::Empty) => HandoffCloseOutcome::StillClosing,
        Ok(()) => HandoffCloseOutcome::Closed,
        Err(mpsc::TryRecvError::Disconnected) => HandoffCloseOutcome::CloseFailed,
    }
}

/// The envelopes owed for a handoff whose close reached `outcome`, in dispatch order, when its tab
/// is on screen; the `command_result` only when `request_id` is given (the page showing the tab is
/// the one that asked).
///
/// The property worth stating: only `Closed` ever produces a `handoff` envelope. A close that is
/// still running, or one that died partway, must not be followed by a card whose first line claims
/// the conversation is closed.
pub(super) fn handoff_payloads(
    tab: TabId,
    outcome: &HandoffCloseOutcome,
    request_id: Option<&str>,
    command: &agent::handoff::ClaudeResumeCommand,
) -> Vec<String> {
    match outcome {
        HandoffCloseOutcome::StillClosing => Vec::new(),
        HandoffCloseOutcome::Closed => {
            let mut payloads = vec![eitri_core::agent_bridge::serialize_handoff_for_js(tab, command)];
            payloads.extend(request_id.map(|id| serialize_command_result_for_js(id, Ok(()))));
            payloads
        }
        // The session is gone regardless -- it was handed to the worker before this. Saying so is
        // the only honest option: an `error` envelope returns the tab to its empty screen, where a
        // failed handoff reads as a dead session rather than as a conversation that is still there.
        HandoffCloseOutcome::CloseFailed => {
            let mut payloads: Vec<String> = request_id
                .map(|id| serialize_command_result_for_js(id, Err(HANDOFF_CLOSE_FAILED_MESSAGE)))
                .into_iter()
                .collect();
            payloads.push(serialize_error_for_js(tab, HANDOFF_CLOSE_FAILED_MESSAGE));
            payloads
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_panel::tests::*;

    /// A tab frozen for a move is not closed: its agent is the other window's to take up, and the close
    /// would interrupt it and shut it down. Once the move is called off the tab closes as any other.
    #[test]
    fn a_tab_moving_to_the_other_window_is_not_closed() {
        let (handle, _page, _host) = handle_with_recording_page();
        let moving = handle.state.borrow_mut().tabs.open(AgentId::Claude);
        let _frozen = handle
            .state
            .borrow_mut()
            .tabs
            .freeze_for_move(moving)
            .expect("a tab with no session freezes");
        assert_eq!(
            handle.run(|state, out| close_tab(state, moving, out)),
            Err(eitri_core::tab_set::MOVING)
        );
        assert!(
            handle.state.borrow().tabs.get(moving).is_some(),
            "the tab stays for its move"
        );
        handle.state.borrow_mut().tabs.unfreeze(moving);
        handle.run(|state, out| close_tab(state, moving, out)).unwrap();
        assert!(handle.state.borrow().tabs.get(moving).is_none());
    }

    /// A tab moves only once nothing of its page's is still on its way: a start, a session continued in
    /// it with `r`, a review its page asked for. A request of another page does not hold it.
    #[test]
    fn a_tab_with_a_start_in_flight_or_a_resume_or_a_pending_review_request_refuses_the_move() {
        let mut tabs = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
        let tab = tabs.active();
        let other = tabs.open(eitri_core::tab_set::AgentId::Claude);
        let two = ViewId::new(2).unwrap();
        assert_eq!(tabs.break_out(two), Ok(other));
        let state = state_for_hooks(tabs);
        let mut s = state.borrow_mut();
        assert!(panel_tab_handoff(&s, tab).is_ok(), "nothing holds it");

        let start = s.owed.owe(ViewId::BASE, "start-1", Owed::StartPage(tab));
        assert!(panel_tab_handoff(&s, tab).unwrap_err().contains("starting"));
        s.owed.owed.remove(&start);

        s.resume_after.insert(tab, ("sess-r".into(), mpsc::channel().1));
        assert!(panel_tab_handoff(&s, tab).unwrap_err().contains("continued"));
        s.resume_after.clear();

        let review = s.owed.owe(ViewId::BASE, "review-1", Owed::ReviewRequest);
        assert!(panel_tab_handoff(&s, tab).unwrap_err().contains("review"));
        assert!(
            panel_tab_handoff(&s, other).is_ok(),
            "a review asked for by another page does not hold this tab"
        );
        s.owed.owed.remove(&review);
        assert!(panel_tab_handoff(&s, tab).is_ok());
        assert!(panel_tab_handoff(&s, TabId(999)).is_err());
    }

    /// A message held for the presence is answered to the page that typed it, and to no page once that
    /// one has gone: a page reusing its view numbers its requests afresh, so the old id would name one of
    /// its own.
    #[test]
    fn a_held_messages_answer_goes_only_to_the_page_that_typed_it() {
        use eitri_core::tab_set::Sent;
        let tabs = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
        let tab = tabs.active();
        let state = state_for_hooks(tabs);
        let screen = Screen::read(&state, &Pages::default());
        let two = ViewId::new(2).unwrap();
        let prompt = |request_id: &str| Sent::Prompt {
            tab,
            page: two,
            request_id: request_id.into(),
            typed: "hi".into(),
            outcome: Ok(()),
        };
        let answers_to = |out: &Outbox, request_id: &str| {
            out.dests()
                .into_iter()
                .zip(out.payloads())
                .filter(|(_, payload)| {
                    let value: serde_json::Value = serde_json::from_str(payload).unwrap();
                    value["kind"] == "command_result" && value["requestId"] == request_id
                })
                .map(|(dest, _)| dest)
                .collect::<Vec<_>>()
        };
        let mut s = state.borrow_mut();

        s.owed.owe(two, "3", Owed::HeldPrompt(tab));
        s.owed.page_closed(two);
        let mut out = Outbox::default();
        apply_sent(&mut s, &screen, prompt("3"), &mut out);
        assert!(answers_to(&out, "3").is_empty(), "the page that typed it is gone");
        assert!(s.owed.is_empty(), "and the entry is spent");

        s.owed.owe(two, "4", Owed::HeldPrompt(tab));
        let mut out = Outbox::default();
        apply_sent(&mut s, &screen, prompt("4"), &mut out);
        assert_eq!(
            answers_to(&out, "4"),
            vec![Dest::Page(two)],
            "answered where it was typed"
        );
        assert!(s.owed.is_empty());
    }

    /// In a linked window a message waits for the presence. A first message held so keeps its start's
    /// page in the ledger once its session is live (the tab may have been broken out meanwhile), and a
    /// held message of either kind that goes with its closed tab is answered, unsent, to the page that
    /// sent it, so that page takes its text back.
    #[test]
    fn a_held_message_keeps_its_page_and_is_answered_when_its_tab_closes() {
        use eitri_core::tab_set::FirstTurn;
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("panel-held-close");
        let mut tabs = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
        let (holder, _parked) =
            eitri_core::turn_review::PresenceHolder::start_parked(dir.join(".review-state"), std::process::id());
        tabs.hold_presence(holder);
        let _ = tabs.set_partner(Some(eitri_core::turn_review::Partner {
            pid: 100,
            start: 0,
            counts_as_other: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }));
        let typed = |text: &str| FirstTurn {
            typed_by_person: true,
            wire: text.into(),
            typed: text.into(),
            shell_line: false,
        };
        let composer = tabs.active();
        let (composer_provider, backend) = live_backend(&dir);
        tabs.get_mut(composer).unwrap().set_backend(TabBackend::Live(backend));
        let first = tabs.open(AgentId::Claude);
        let (first_provider, backend) = live_backend(&dir);
        tabs.get_mut(first).unwrap().set_backend(TabBackend::Live(backend));
        let two = ViewId::new(2).unwrap();
        assert_eq!(tabs.break_out(two), Ok(first));
        assert!(tabs.send_first_turn(first, "first-1", typed("hi")).is_none(), "held");
        assert!(tabs
            .send_prompt(composer, "prompt-1", ViewId::BASE, typed("there"))
            .is_none());
        let state = state_for_hooks(tabs);
        let screen = Screen::read(&state, &Pages::default());
        let mut s = state.borrow_mut();
        s.owed.owe(ViewId::BASE, "first-1", Owed::StartPage(first));
        s.owed.owe(ViewId::BASE, "prompt-1", Owed::HeldPrompt(composer));

        let mut out = Outbox::default();
        collect_pending_starts(&mut s, &screen, &mut out);
        assert!(
            s.owed
                .key_where(|answer| answer.what == Owed::StartPage(first))
                .is_some(),
            "the held first message is still owed to the base page"
        );

        let answers = |out: &Outbox, request_id: &str| {
            out.dests()
                .into_iter()
                .zip(out.payloads())
                .filter_map(|(dest, payload)| {
                    let value: serde_json::Value = serde_json::from_str(&payload).unwrap();
                    (value["kind"] == "command_result" && value["requestId"] == request_id)
                        .then(|| (dest, value["error"].as_str().unwrap_or_default().to_string()))
                })
                .collect::<Vec<_>>()
        };
        let closed = vec![(Dest::Page(ViewId::BASE), TAB_CLOSED_MESSAGE.to_string())];
        let mut out = Outbox::default();
        close_tab(&mut s, first, &mut out).unwrap();
        assert_eq!(
            answers(&out, "first-1"),
            closed,
            "answered where it was sent, not in the tab's view"
        );
        let mut out = Outbox::default();
        close_tab(&mut s, composer, &mut out).unwrap();
        assert_eq!(answers(&out, "prompt-1"), closed);
        assert!(
            first_provider.turns().is_empty() && composer_provider.turns().is_empty(),
            "nothing held reached an agent"
        );
        assert!(s.owed.is_empty(), "nothing is left owed");
    }

    /// The hints a tab's page was shown, and the ones it was owed, are owed to the page the tab lands in,
    /// under the tab's number there; another tab's stay.
    #[test]
    fn the_panels_tab_record_carries_its_hints_to_the_page_it_lands_in() {
        use eitri_core::turn_review::ReviewHint;
        let mut tabs = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
        let tab = tabs.active();
        let other = tabs.open(eitri_core::tab_set::AgentId::Claude);
        let state = state_for_hooks(tabs);
        let record = {
            let mut s = state.borrow_mut();
            s.shown_hints.push(ReviewHint {
                tab: tab.0,
                turn: 2,
                files: 3,
            });
            s.review_hints.push(ReviewHint {
                tab: other.0,
                turn: 1,
                files: 1,
            });
            panel_tab_handoff(&s, tab).unwrap()
        };
        let carried = PanelTabHandoff::from_value(&record.to_value()).expect("the record reads back");
        assert_eq!(carried, record);
        assert!(PanelTabHandoff::from_value(&serde_json::json!({ "shown_hints": [{ "turn": -1 }] })).is_err());

        let dest_tabs = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
        let landed = dest_tabs.active();
        let dest = state_for_hooks(dest_tabs);
        adopt_panel_tab(&mut dest.borrow_mut(), landed, carried);
        assert_eq!(
            dest.borrow().review_hints,
            vec![ReviewHint {
                tab: landed.0,
                turn: 2,
                files: 3,
            }],
            "owed to the page that shows the tab now"
        );
    }

    /// The name goes into a file name and the tools into what the session may call, so nothing else is taken.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_records_bridge_is_read_by_its_shape() {
        let good = || {
            serde_json::json!({
                "following": false,
                "shown_hints": [],
                "review_hints": [],
                "editor_bridge": {
                    "name": { "hex": "0123456789abcdef", "key": "0123456789abcdef0123456789abcdef" },
                    "tools": ["open_file"],
                },
            })
        };
        let read = PanelTabHandoff::from_value(&good()).expect("the shape the record writes");
        assert_eq!(read.editor_bridge.as_ref().unwrap().tools, ["open_file"]);
        assert_eq!(PanelTabHandoff::from_value(&read.to_value()).unwrap(), read);

        let spoilt = |change: &dyn Fn(&mut serde_json::Value)| {
            let mut record = good();
            change(&mut record);
            PanelTabHandoff::from_value(&record)
        };
        assert!(spoilt(&|r| r["editor_bridge"]["name"]["hex"] = "../../x".into()).is_err());
        assert!(spoilt(&|r| r["editor_bridge"]["name"]["key"] = 7.into()).is_err());
        assert!(spoilt(&|r| r["editor_bridge"]["tools"] = serde_json::json!([1])).is_err());
        assert!(spoilt(&|r| r["editor_bridge"]["tools"] = "open_file".into()).is_err());
        assert!(spoilt(&|r| {
            r.as_object_mut().unwrap().remove("editor_bridge");
        })
        .is_err());
        let none = spoilt(&|r| r["editor_bridge"] = serde_json::Value::Null).unwrap();
        assert_eq!(none.editor_bridge, None);
    }

    /// A session handed to a terminal must not also be offered for resume on the start screen --
    /// accepting that offer would put two writers on one transcript, which is exactly what the card
    /// beside it warns about. This is the case the frontend's own suppression cannot cover: `hello`
    /// is recomputed here on every mount, and `agent::resumable_session` picks the most recently
    /// updated record, which right after a handoff is the handed-over session itself.
    #[test]
    fn a_handoff_drops_exactly_the_session_it_gave_away_and_no_other() {
        // One list, two rows: the session just handed to a terminal, and an unrelated one the
        // workspace also remembers. Before the picker landed, `resumable` held at most one record
        // and these were two tests that could not tell "dropped the right row" from "cleared the
        // offer" -- the distinction only becomes testable when the list can hold both at once.
        let greeting = BackendGreeting {
            kind: BackendKind::Sidecar,
            project_dir: PathBuf::from("/home/user/project"),
            permission_modes: eitri_core::agent_backend::CLIENT_IMPLEMENTED_PERMISSION_MODES,
            resumable: vec![
                agent::ResumableSession {
                    provider: "claude".to_string(),
                    provider_session_id: "1857dcd5-973b-46a2".to_string(),
                    created_at: "1757600000000".to_string(),
                    updated_at: "1757700000000".to_string(),
                    title: None,
                    name: None,
                },
                agent::ResumableSession {
                    provider: "claude".to_string(),
                    provider_session_id: "some-other-session".to_string(),
                    created_at: "1757500000000".to_string(),
                    updated_at: "1757600000000".to_string(),
                    title: None,
                    name: None,
                },
            ],
            account: None,
        };
        let command = a_command();
        // The tab set's `open_sessions` includes a handed-off session.
        let open = vec![(AgentId::Claude, command.provider_session_id().to_string())];
        let payloads = ready_payloads(
            greeting,
            &open,
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
            .expect("the offer is a list")
            .iter()
            .map(|s| s["providerSessionId"].as_str().unwrap())
            .collect();
        assert_eq!(
            offered,
            vec!["some-other-session"],
            "a handoff must drop the session it gave away -- and only that one. Nothing holds a \
             lock on it, so re-offering it would put two writers on one transcript; clearing the \
             whole list instead would hide a conversation nobody gave away."
        );
    }

    /// The sequencing the whole feature's honesty rests on: while the close worker has not reported,
    /// the tick decides `StillClosing` -- and `handoff_payloads` dispatches nothing at all for it.
    /// A real channel across a real send, not a mocked one.
    #[test]
    fn the_command_is_not_released_until_the_close_worker_has_reported() {
        let (closed_tx, closed_rx) = mpsc::channel();
        assert_eq!(classify_close_signal(&closed_rx), HandoffCloseOutcome::StillClosing);
        assert!(handoff_payloads(
            TabId(1),
            &HandoffCloseOutcome::StillClosing,
            Some("req-1"),
            &a_command()
        )
        .is_empty());

        closed_tx.send(()).unwrap();
        assert_eq!(classify_close_signal(&closed_rx), HandoffCloseOutcome::Closed);
        assert_eq!(
            kinds(&handoff_payloads(
                TabId(1),
                &HandoffCloseOutcome::Closed,
                Some("req-1"),
                &a_command()
            )),
            vec!["handoff", "command_result"]
        );
    }

    /// A worker that panicked inside `shutdown()` drops its sender without sending. That is a failed
    /// close, not a finished one -- and a failed close must NOT be followed by a card whose first
    /// line says the conversation is closed.
    #[test]
    fn a_close_that_died_partway_reports_a_dead_session_rather_than_the_command() {
        let (closed_tx, closed_rx) = mpsc::channel::<()>();
        drop(closed_tx);
        assert_eq!(classify_close_signal(&closed_rx), HandoffCloseOutcome::CloseFailed);

        let payloads = handoff_payloads(TabId(1), &HandoffCloseOutcome::CloseFailed, Some("req-2"), &a_command());
        assert_eq!(kinds(&payloads), vec!["command_result", "error"]);
        assert!(
            !payloads.iter().any(|p| p.contains("1857dcd5-973b-46a2")),
            "a failed close leaked the resume command anyway: {payloads:?}"
        );
    }

    // ---- bringing the last window's tabs back --------------------------------------------------
}
