//! Manual mode: the three-way cycle.
#![cfg(test)]

use super::*;

/// A Claude tab cycles manual -> auto -> bypass -> manual; only the move into bypass asks, and it
/// is offered only from auto. Leaving bypass lands on manual.
#[test]
fn a_claude_tab_cycles_manual_auto_bypass_and_back_to_manual() {
    let mut set = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Manual);
    let tab = set.active();
    assert_eq!(set.get(tab).unwrap().mode(), SessionModeChoice::Manual);
    assert_eq!(
        set.cycle_mode(tab, ViewId::BASE),
        Ok(ModeCycle::Changed(SessionModeChoice::Auto))
    );
    assert_eq!(
        set.default_mode(),
        SessionModeChoice::Auto,
        "an empty tab carries the default"
    );
    let plan = plan(set.cycle_mode(tab, ViewId::BASE));
    assert_eq!(
        set.get(tab).unwrap().mode(),
        SessionModeChoice::Auto,
        "nothing moves before y"
    );
    set.confirm_bypass(plan.scope, plan.nonce, ViewId::BASE).unwrap();
    assert_eq!(set.get(tab).unwrap().mode(), SessionModeChoice::Bypass);
    assert_eq!(
        set.cycle_mode(tab, ViewId::BASE),
        Ok(ModeCycle::Changed(SessionModeChoice::Manual))
    );
    assert_eq!(set.get(tab).unwrap().mode(), SessionModeChoice::Manual);
    assert_eq!(
        set.default_mode(),
        SessionModeChoice::Manual,
        "the empty tab's default went with it"
    );
    shut_down_all(&mut set);
}

/// From manual the key never asks and never reaches bypass: bypass is entered only from auto, and
/// a confirm left over from a plan made in auto is no yes once the tab is in manual.
#[test]
fn bypass_is_entered_only_from_auto() {
    let dir = workspace("manual-bypass-only-from-auto");
    let mut set = set();
    let tab = set.active();
    let (_provider, backend) = live(&dir);
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
    set.get_mut(tab).unwrap().mode = SessionModeChoice::Manual;
    assert_eq!(
        set.cycle_mode(tab, ViewId::BASE),
        Ok(ModeCycle::Changed(SessionModeChoice::Auto))
    );
    let plan = plan(set.cycle_mode(tab, ViewId::BASE));
    // Something other than the key moved the tab under the open prompt.
    set.get_mut(tab).unwrap().mode = SessionModeChoice::Manual;
    assert_eq!(
        set.confirm_bypass(plan.scope, plan.nonce, ViewId::BASE),
        Err("that prompt is no longer current".to_string())
    );
    assert_eq!(set.get(tab).unwrap().mode(), SessionModeChoice::Manual);
    shut_down_all(&mut set);
}

/// An ACP agent's own mode is set once at its start, so its tab offers no manual: the key goes
/// auto <-> bypass, leaving bypass lands on auto.
#[test]
fn an_acp_tab_skips_manual() {
    let mut set = set();
    let acp = set.open(AgentId::Acp("gem".into()));
    assert_eq!(set.get(acp).unwrap().mode(), SessionModeChoice::Auto);
    let plan = plan(set.cycle_mode(acp, ViewId::BASE));
    set.confirm_bypass(plan.scope, plan.nonce, ViewId::BASE).unwrap();
    assert_eq!(
        set.cycle_mode(acp, ViewId::BASE),
        Ok(ModeCycle::Changed(SessionModeChoice::Auto))
    );
    assert_eq!(
        set.default_mode(),
        SessionModeChoice::Manual,
        "leaving bypass leaves the window default in manual; the tab's auto is only its agent's stand-in"
    );
    assert!(
        matches!(set.cycle_mode(acp, ViewId::BASE), Ok(ModeCycle::Confirm(_))),
        "auto -> bypass asks"
    );
    shut_down_all(&mut set);
}

/// A new ACP tab under a manual window default starts in auto, and the page is told once; a Claude
/// or Codex tab takes manual and says nothing.
#[test]
fn a_new_acp_tab_under_a_manual_default_starts_in_auto_and_says_so_once() {
    let mut set = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Manual);
    assert_eq!(set.take_mode_note(), None);
    let codex = set.open(AgentId::Codex);
    assert_eq!(set.get(codex).unwrap().mode(), SessionModeChoice::Manual);
    assert_eq!(set.take_mode_note(), None);
    let acp = set.open(AgentId::Acp("gem".into()));
    assert_eq!(set.get(acp).unwrap().mode(), SessionModeChoice::Auto);
    assert_eq!(
        set.default_mode(),
        SessionModeChoice::Manual,
        "the window default is unchanged"
    );
    let note = set.take_mode_note().expect("the page is told");
    assert!(note.contains("gem tabs have no manual mode"), "{note}");
    let number = set.get(acp).unwrap().number;
    assert!(note.ends_with(&format!("tab {number} is in auto")), "{note}");
    assert_eq!(set.take_mode_note(), None, "once");
}

/// The configured default and the configured agent reach the empty tab the window already has in
/// a mode its agent offers, saying so when that is not the one asked for.
#[test]
fn the_configured_default_and_agent_give_an_acp_tab_auto_for_manual() {
    let mut set = set();
    set.apply_configured_agent(AgentId::Acp("gem".into()));
    set.apply_configured_default(SessionModeChoice::Manual);
    assert_eq!(set.default_mode(), SessionModeChoice::Manual);
    assert_eq!(set.active_tab().mode(), SessionModeChoice::Auto);
    assert!(set.take_mode_note().is_some());

    let mut other = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
    other.apply_configured_default(SessionModeChoice::Manual);
    assert_eq!(other.active_tab().mode(), SessionModeChoice::Manual);
    assert_eq!(other.take_mode_note(), None);
    other.apply_configured_agent(AgentId::Acp("gem".into()));
    assert_eq!(other.active_tab().mode(), SessionModeChoice::Auto);
    assert!(other.take_mode_note().is_some());
}

/// The window default cycles in the same order; a bypass default is left for manual.
#[test]
fn the_window_default_cycles_manual_auto_bypass() {
    let mut set = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Manual);
    assert_eq!(
        set.cycle_default_mode(ViewId::BASE),
        ModeCycle::Changed(SessionModeChoice::Auto)
    );
    let ModeCycle::Confirm(plan) = set.cycle_default_mode(ViewId::BASE) else {
        panic!("auto -> bypass asks")
    };
    set.confirm_bypass(plan.scope, plan.nonce, ViewId::BASE).unwrap();
    assert_eq!(set.default_mode(), SessionModeChoice::Bypass);
    assert_eq!(
        set.cycle_default_mode(ViewId::BASE),
        ModeCycle::Changed(SessionModeChoice::Manual)
    );
}

/// A tab saved in manual comes back in manual with no question, whichever way the restore runs:
/// it is the strictest mode.
#[test]
fn a_tab_saved_in_manual_comes_back_in_manual_unasked() {
    for policy in [BypassPolicy::Ask, BypassPolicy::Downgrade, BypassPolicy::Keep] {
        let mut set = set();
        let plan = plan_of(
            vec![
                planned("m", SessionModeChoice::Manual),
                planned("a", SessionModeChoice::Auto),
            ],
            None,
        );
        let RestoreStep::Go(go) = set.begin_restore(plan, policy) else {
            panic!("{policy:?}: nothing in bypass, nothing to ask")
        };
        assert_eq!((go.downgraded, go.asked), (0, false));
        let from = set.active();
        set.start_restore(from, go, |_, _| Ok(RestoreConnect::Started(mpsc::channel().1)));
        assert_eq!(mode_of(&set, "m"), SessionModeChoice::Manual, "{policy:?}");
        assert_eq!(mode_of(&set, "a"), SessionModeChoice::Auto, "{policy:?}");
    }
}

fn mode_of(set: &TabSet, session: &str) -> SessionModeChoice {
    let tab = set
        .tabs()
        .iter()
        .find(|t| t.provider_session_id().as_deref() == Some(session))
        .map(|t| t.id)
        .expect("the session has a tab");
    set.get(tab).unwrap().mode()
}

#[test]
fn tabs_saved_in_auto_go_ahead_without_a_question() {
    let mut set = set();
    let plan = plan_of(
        vec![
            planned("a", SessionModeChoice::Auto),
            planned("b", SessionModeChoice::Auto),
        ],
        None,
    );
    let RestoreStep::Go(go) = set.begin_restore(plan, BypassPolicy::Ask) else {
        panic!("nothing in bypass, nothing to ask")
    };
    assert_eq!((go.downgraded, go.asked), (0, false));
}

#[test]
fn a_tab_saved_in_bypass_asks_first_and_y_gives_it_back_in_bypass() {
    let mut set = set();
    let plan = plan_of(
        vec![
            planned("a", SessionModeChoice::Auto),
            planned("b", SessionModeChoice::Bypass),
            planned("c", SessionModeChoice::Auto),
        ],
        None,
    );
    let RestoreStep::Confirm(prompt) = set.begin_restore(plan, BypassPolicy::Ask) else {
        panic!("a bypass tab is asked about")
    };
    assert_eq!(prompt.lines[0], "Restore 3 tabs (1 in bypass)? y/n");
    assert!(
        prompt.lines[1].contains("n brings the bypass tab back in auto"),
        "{:?}",
        prompt.lines
    );
    let go = set.answer_restore(prompt.nonce, true).unwrap();
    assert_eq!((go.downgraded, go.asked), (0, true));
    let from = set.active();
    let mut asked = Vec::new();
    set.start_restore(from, go, |_, id| {
        asked.push(id.to_string());
        Ok(RestoreConnect::Started(mpsc::channel().1))
    });
    assert_eq!(asked, ["a", "b", "c"]);
    assert_eq!(mode_of(&set, "a"), SessionModeChoice::Auto);
    assert_eq!(mode_of(&set, "b"), SessionModeChoice::Bypass, "after a yes");
    assert_eq!(mode_of(&set, "c"), SessionModeChoice::Auto);
}

#[test]
fn n_gives_the_bypass_tabs_back_in_auto_and_nothing_enters_bypass_without_a_yes() {
    let mut set = set();
    let plan = plan_of(
        vec![
            planned("a", SessionModeChoice::Bypass),
            planned("b", SessionModeChoice::Bypass),
        ],
        None,
    );
    let RestoreStep::Confirm(prompt) = set.begin_restore(plan, BypassPolicy::Ask) else {
        panic!("asked")
    };
    assert_eq!(prompt.lines[0], "Restore 2 tabs (2 in bypass)? y/n");
    let go = set.answer_restore(prompt.nonce, false).unwrap();
    assert_eq!(go.downgraded, 2);
    let from = set.active();
    set.start_restore(from, go, |_, _| Ok(RestoreConnect::Started(mpsc::channel().1)));
    assert_eq!(mode_of(&set, "a"), SessionModeChoice::Auto);
    assert_eq!(mode_of(&set, "b"), SessionModeChoice::Auto);
    assert_eq!(
        set.default_mode(),
        SessionModeChoice::Auto,
        "the window's default never moved"
    );
}

#[test]
fn a_stale_or_dropped_answer_restores_nothing_and_a_newer_question_replaces_an_older() {
    let mut set = set();
    let bypass = || plan_of(vec![planned("a", SessionModeChoice::Bypass)], None);
    let RestoreStep::Confirm(first) = set.begin_restore(bypass(), BypassPolicy::Ask) else {
        panic!("asked")
    };
    let RestoreStep::Confirm(second) = set.begin_restore(bypass(), BypassPolicy::Ask) else {
        panic!("asked")
    };
    assert_ne!(first.nonce, second.nonce);
    assert!(
        set.answer_restore(first.nonce, true).is_err(),
        "answers the question that was replaced"
    );
    assert!(set.answer_restore(0, true).is_err());
    // Moving to another tab ends the question, as it does a bypass prompt.
    let other = set.open(AgentId::Claude);
    let _ = other;
    assert!(
        set.answer_restore(second.nonce, true).is_err(),
        "the question was about the tab that was left"
    );
    // And an answer is taken once.
    let RestoreStep::Confirm(third) = set.begin_restore(bypass(), BypassPolicy::Ask) else {
        panic!("asked")
    };
    assert!(set.answer_restore(third.nonce, false).is_ok());
    assert!(set.answer_restore(third.nonce, false).is_err());
}

#[test]
fn a_quiet_restore_downgrades_bypass_and_says_so_and_the_users_own_default_keeps_it() {
    let mut set = set();
    let plan = || {
        plan_of(
            vec![
                planned("a", SessionModeChoice::Bypass),
                planned("b", SessionModeChoice::Auto),
            ],
            None,
        )
    };
    let RestoreStep::Go(quiet) = set.begin_restore(plan(), BypassPolicy::Downgrade) else {
        panic!("no question")
    };
    assert_eq!((quiet.downgraded, quiet.asked), (1, false));
    let RestoreStep::Go(kept) = set.begin_restore(plan(), BypassPolicy::Keep) else {
        panic!("no question")
    };
    assert_eq!((kept.downgraded, kept.asked), (0, false));
    let from = set.active();
    set.start_restore(from, kept, |_, _| Ok(RestoreConnect::Started(mpsc::channel().1)));
    assert_eq!(
        mode_of(&set, "a"),
        SessionModeChoice::Bypass,
        "init.lua said bypass is fine"
    );
}

#[test]
fn replacing_the_draft_ends_the_late_mirror_allowance() {
    let mut set = set();
    let tab = set.active();
    set.set_draft(tab, "mirrored before the move");
    set.open(AgentId::Claude);
    assert!(set.select(tab));
    let view = ViewId::new(2).unwrap();
    assert_eq!(set.break_out(view), Ok(tab));
    assert!(set.take_late_draft(tab, ViewId::BASE));

    assert_eq!(set.join_back(view), Some(tab));
    set.get_mut(tab).unwrap().replace_draft("newer draft".into());
    assert!(!set.take_late_draft(tab, view));
}

#[test]
fn a_late_draft_is_taken_once_from_the_view_the_tab_left_and_never_after_a_newer_one() {
    let mut set = set();
    let first = set.active();
    set.open(AgentId::Claude);
    assert!(set.select(first));
    let view = ViewId::new(2).unwrap();
    assert_eq!(set.break_out(view), Ok(first));
    assert!(
        !set.take_late_draft(first, ViewId::new(3).unwrap()),
        "not the view it left"
    );
    assert!(set.take_late_draft(first, ViewId::BASE));
    assert!(!set.take_late_draft(first, ViewId::BASE), "taken once");
    assert_eq!(set.join_back(view), Some(first));
    assert!(set.set_draft(first, "newer, from the page that shows it"));
    assert!(!set.take_late_draft(first, view), "a newer copy came in since the move");
}

#[test]
fn a_late_draft_is_not_taken_once_the_draft_was_set_any_other_way() {
    use crate::scratch::EditDone;
    let mut set = set();
    let first = set.active();
    set.open(AgentId::Claude);
    assert!(set.select(first));
    let view = ViewId::new(2).unwrap();
    let joined = |set: &mut TabSet| {
        assert_eq!(set.break_out(view), Ok(first));
        assert_eq!(set.join_back(view), Some(first));
    };

    joined(&mut set);
    set.begin_scratch_edit(first, 1).unwrap();
    set.finish_scratch_edit(1, &EditDone::Written("edited in nvim".into()))
        .unwrap();
    assert!(
        !set.take_late_draft(first, view),
        "an nvim edit came back after the move"
    );
    assert_eq!(set.get(first).unwrap().session.draft, "edited in nvim");

    joined(&mut set);
    set.begin_scratch_edit(first, 2).unwrap();
    set.cancel_scratch_edits(&[(2, EditDone::Written("saved as nvim went".into()))]);
    assert!(
        !set.take_late_draft(first, view),
        "an edit saved as the editor went away"
    );

    joined(&mut set);
    set.note_sent(first);
    assert!(!set.take_late_draft(first, view), "the draft was just sent");

    joined(&mut set);
    set.begin_scratch_edit(first, 3).unwrap();
    set.finish_scratch_edit(3, &EditDone::Discarded).unwrap();
    assert!(
        set.take_late_draft(first, view),
        "an edit that saved nothing leaves the late mirror due"
    );
}

#[test]
fn a_put_off_restore_whose_holder_broke_out_keeps_its_sessions_in_the_base_view() {
    let mut set = set();
    let first = set.active();
    set.open(AgentId::Claude);
    assert!(set.select(first));
    let from = set.active();
    let view = ViewId::new(2).unwrap();
    assert_eq!(set.break_out(view), Ok(from));
    let plan = plan_of(
        vec![
            planned("a", SessionModeChoice::Auto),
            planned("b", SessionModeChoice::Auto),
        ],
        Some("a"),
    );
    let RestoreStep::Go(go) = set.begin_restore(plan, BypassPolicy::Ask) else {
        panic!("no question")
    };
    assert!(!set.keep_put_off_restore(from, go, "put off"));
    assert!(set.v1_holds());
    for id in ["a", "b"] {
        let tab = set
            .tabs()
            .iter()
            .find(|t| t.failed_resume.as_deref() == Some(id))
            .map(|t| t.id)
            .unwrap_or_else(|| panic!("session {id} is kept"));
        assert_eq!(set.view_of(tab), ViewId::BASE, "session {id} is kept in the base view");
    }
    assert_eq!(set.get(from).unwrap().failed_resume, None, "the moved tab holds none");
    assert_eq!(set.view_of(from), view);
    let saved = set.saved_snapshot("conv").unwrap();
    assert!(
        saved.tabs.iter().all(|t| t.view.is_none()),
        "no kept session is saved as agent#2's"
    );
}

#[test]
fn a_restore_whose_holder_broke_out_resumes_in_the_base_view() {
    let mut set = set();
    let first = set.active();
    set.open(AgentId::Claude);
    assert!(set.select(first));
    let from = set.active();
    let view = ViewId::new(2).unwrap();
    assert_eq!(set.break_out(view), Ok(from));
    assert_eq!(set.view_of(from), view);
    let plan = plan_of(
        vec![
            planned("a", SessionModeChoice::Auto),
            planned("b", SessionModeChoice::Auto),
        ],
        Some("a"),
    );
    let RestoreStep::Go(go) = set.begin_restore(plan, BypassPolicy::Ask) else {
        panic!("no question")
    };
    let run = set.start_restore(from, go, |_, _| Ok(RestoreConnect::Started(mpsc::channel().1)));
    for id in ["a", "b"] {
        let tab = set
            .tab_with_session(&AgentId::Claude, id)
            .unwrap_or_else(|| panic!("session {id} came back"));
        assert_eq!(
            set.view_of(tab),
            ViewId::BASE,
            "session {id} came back in the base view"
        );
        assert!(run.owns(tab));
    }
    assert_eq!(
        set.get(from).unwrap().provider_session_id(),
        None,
        "the moved tab holds none"
    );
    assert!(!run.began_in(from));
    assert!(set.view_of(set.active()).is_base());
    assert_eq!(set.active_tab().provider_session_id().as_deref(), Some("a"));
}

#[test]
fn restored_tabs_start_in_order_the_first_in_the_empty_tab_and_the_saved_active_one_is_shown() {
    let mut set = set();
    let from = set.active();
    let plan = plan_of(
        vec![
            planned("a", SessionModeChoice::Auto),
            planned("b", SessionModeChoice::Auto),
            planned("c", SessionModeChoice::Auto),
        ],
        Some("b"),
    );
    let RestoreStep::Go(go) = set.begin_restore(plan, BypassPolicy::Ask) else {
        panic!("no question")
    };
    let run = set.start_restore(from, go, |_, _| Ok(RestoreConnect::Started(mpsc::channel().1)));
    let numbers: Vec<(u16, Option<String>)> = set.tabs().iter().map(|t| (t.number, t.provider_session_id())).collect();
    assert_eq!(
        numbers,
        vec![
            (1, Some("a".to_string())),
            (2, Some("b".to_string())),
            (3, Some("c".to_string()))
        ],
        "the first went into the tab the restore began in, the rest into new ones, in order"
    );
    assert_eq!(set.get(from).unwrap().provider_session_id().as_deref(), Some("a"));
    assert_eq!(set.active_tab().provider_session_id().as_deref(), Some("b"));
    assert!(run.owns(from) && run.began_in(from));
    assert_eq!(run.outcome(), None, "nothing has connected yet");
}

#[test]
fn when_the_saved_active_tab_is_skipped_the_first_restored_tab_is_shown() {
    let mut set = set();
    let from = set.active();
    let mut plan = plan_of(
        vec![
            planned("a", SessionModeChoice::Auto),
            planned("b", SessionModeChoice::Auto),
        ],
        Some("gone"),
    );
    plan.skipped.push(Skipped {
        label: "gone".into(),
        reason: "its saved record is gone".into(),
        kept: None,
    });
    let RestoreStep::Go(go) = set.begin_restore(plan, BypassPolicy::Ask) else {
        panic!("no question")
    };
    set.start_restore(from, go, |_, _| Ok(RestoreConnect::Started(mpsc::channel().1)));
    assert_eq!(set.active_tab().provider_session_id().as_deref(), Some("a"));
}

#[test]
fn a_session_already_open_here_is_switched_to_and_never_resumed_twice() {
    let dir = workspace("tabs-restore-dedupe");
    let mut set = set();
    let holder = set.active();
    give_session(&mut set, holder, &dir, "s-open");
    let from = set.open(AgentId::Claude);
    let plan = plan_of(
        vec![
            planned("s-open", SessionModeChoice::Auto),
            planned("s-new", SessionModeChoice::Auto),
        ],
        None,
    );
    let RestoreStep::Go(go) = set.begin_restore(plan, BypassPolicy::Ask) else {
        panic!("no question")
    };
    let mut connected = Vec::new();
    let run = set.start_restore(from, go, |_, id| {
        connected.push(id.to_string());
        Ok(RestoreConnect::Started(mpsc::channel().1))
    });
    assert_eq!(connected, ["s-new"], "the open one is not connected again");
    assert_eq!(set.tab_with_session(&AgentId::Claude, "s-open"), Some(holder));
    assert_eq!(set.tabs().len(), 2, "and no tab was made for it");
    assert_eq!(run.outcome(), None, "s-new is still connecting");
    shut_down_all(&mut set);
}

#[test]
fn a_tab_that_cannot_connect_is_skipped_before_any_tab_is_made_for_it() {
    let mut set = set();
    let from = set.active();
    let plan = plan_of(
        vec![
            planned("held", SessionModeChoice::Auto),
            planned("free", SessionModeChoice::Auto),
        ],
        None,
    );
    let RestoreStep::Go(go) = set.begin_restore(plan, BypassPolicy::Ask) else {
        panic!("no question")
    };
    let run = set.start_restore(from, go, |_, id| {
        if id == "held" {
            Err("open in another window".to_string())
        } else {
            Ok(RestoreConnect::Started(mpsc::channel().1))
        }
    });
    assert_eq!(
        set.tabs().len(),
        1,
        "the one that connected took the empty tab; none was made for the other"
    );
    assert_eq!(set.get(from).unwrap().provider_session_id().as_deref(), Some("free"));
    assert!(run.owns(from));
    assert_eq!(run.failed.len(), 1);
}

/// The whole of a restore as the panel drives it: every resume reports through `collect_starts`,
/// and one message ends it.
#[test]
fn a_restore_ends_in_one_message_once_every_resume_has_returned() {
    let dir = workspace("tabs-restore-outcome");
    let mut set = set();
    let from = set.active();
    let mut plan = plan_of(
        vec![
            planned("a", SessionModeChoice::Auto),
            planned("b", SessionModeChoice::Auto),
            planned("c", SessionModeChoice::Auto),
        ],
        Some("c"),
    );
    plan.skipped.push(Skipped {
        label: "docs".into(),
        reason: "open in another window".into(),
        kept: None,
    });
    let RestoreStep::Go(go) = set.begin_restore(plan, BypassPolicy::Ask) else {
        panic!("no question")
    };
    let mut senders = Vec::new();
    let mut run = set.start_restore(from, go, |_, _| {
        let (tx, rx) = mpsc::channel();
        senders.push(tx);
        Ok(RestoreConnect::Started(rx))
    });
    assert_eq!(run.outcome(), None);

    // a connects, b is refused, c has not answered yet.
    let (_provider, backend) = live(&dir);
    senders[0].send(Ok(backend)).unwrap();
    senders[1]
        .send(Err(refused("could not continue the previous session: no such session")))
        .unwrap();
    for collected in set.collect_starts() {
        match collected {
            StartCollected::Installed { tab, .. } => run.note_installed(tab),
            StartCollected::Failed { tab, error, .. } => {
                run.note_failed(tab, &error.message);
                set.discard_failed_restore(tab, run.back_to(tab));
            }
        }
    }
    assert_eq!(run.outcome(), None, "c is still connecting");
    assert_eq!(set.tabs().len(), 2, "the refused tab is gone, not left behind dead");
    assert!(set.tab_with_session(&AgentId::Claude, "b").is_none());

    let (_provider, backend) = live(&dir);
    senders[2].send(Ok(backend)).unwrap();
    for collected in set.collect_starts() {
        if let StartCollected::Installed { tab, .. } = collected {
            run.note_installed(tab);
        }
    }
    assert_eq!(
        run.outcome().unwrap().message(),
        "Restored 2 of 4 tabs; 2 could not be: docs (open in another window), title of b (could not continue the previous session: no such session)"
    );
    shut_down_all(&mut set);
}

#[test]
fn when_the_tab_the_restore_began_in_fails_it_is_an_empty_tab_again() {
    let mut set = set();
    let from = set.active();
    let plan = plan_of(vec![planned("a", SessionModeChoice::Auto)], None);
    let RestoreStep::Go(go) = set.begin_restore(plan, BypassPolicy::Ask) else {
        panic!("no question")
    };
    let (tx, rx) = mpsc::channel();
    let mut rx = Some(rx);
    let mut run = set.start_restore(from, go, |_, _| Ok(RestoreConnect::Started(rx.take().unwrap())));
    tx.send(Err(refused("refused"))).unwrap();
    for collected in set.collect_starts() {
        if let StartCollected::Failed { tab, error, .. } = collected {
            run.note_failed(tab, &error.message);
            set.discard_failed_restore(tab, run.back_to(tab));
        }
    }
    assert!(matches!(set.get(from).unwrap().backend, TabBackend::NotStarted));
    assert!(set.is_pristine());
    assert_eq!(
        run.outcome().unwrap().message(),
        "Restored 0 of 1 tab; 1 could not be: title of a (refused)"
    );
}

/// The draft of a saved tab is what comes back in its composer, after anything the tab it lands in holds, and
/// a tab that cannot come back hands its draft to the tab the restore started from.
#[test]
fn a_saved_draft_comes_back_with_its_tab() {
    let mut set = set();
    let from = set.active();
    let mut plan = plan_of(
        vec![
            planned("a", SessionModeChoice::Auto),
            planned("b", SessionModeChoice::Auto),
        ],
        Some("a"),
    );
    plan.items[0].draft = Some("for a".to_string());
    plan.items[1].draft = Some("for b".to_string());
    let RestoreStep::Go(go) = set.begin_restore(plan, BypassPolicy::Ask) else {
        panic!("no question")
    };
    let (_tx, rx) = mpsc::channel();
    let mut rx = Some(rx);
    set.start_restore(from, go, |_, id| match id {
        "a" => Ok(RestoreConnect::Started(rx.take().unwrap())),
        _ => Err("refused".to_string()),
    });
    assert_eq!(set.get(from).unwrap().draft(), "for a\n\nfor b");
}

#[test]
fn a_skipped_tabs_draft_comes_back_and_a_restore_put_off_keeps_each_tabs_own() {
    let mut set = set();
    let from = set.active();
    let mut plan = plan_of(vec![planned("a", SessionModeChoice::Auto)], Some("a"));
    plan.items[0].draft = Some("for a".to_string());
    plan.skipped.push(Skipped {
        label: "gone".to_string(),
        reason: "its record is gone".to_string(),
        kept: Some(SavedTab {
            draft: Some("for the gone tab".to_string()),
            ..saved_tab_named("gone")
        }),
    });
    let RestoreStep::Go(go) = set.begin_restore(plan, BypassPolicy::Ask) else {
        panic!("no question")
    };
    set.keep_put_off_restore(from, go, "put off");
    let saved = set.saved_snapshot(PROJECT).unwrap();
    assert_eq!(saved.tabs[0].draft.as_deref(), Some("for the gone tab\n\nfor a"));
}

#[test]
fn a_saved_tab_carries_its_draft_and_only_text_that_would_go_is_counted() {
    let dir = workspace("saved-draft");
    let mut set = set();
    let tab = set.active();
    let (provider, backend) = live(&dir);
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
    provider.queue(AgentDomainEvent::SessionOpened {
        session_id: "verdandi-1".into(),
        provider_session_id: "sess-a".into(),
        model: "m".into(),
        cwd: dir.to_string_lossy().into_owned(),
    });
    until("the session", || {
        set.pump(&dir, true);
        set.get(tab).unwrap().provider_session_id().is_some()
    });
    assert_eq!(set.unsaved_draft_count(true, &Default::default()), 0);
    set.set_draft(tab, "still typing");
    let saved = set.saved_snapshot(PROJECT).unwrap();
    assert_eq!(saved.tabs[0].draft.as_deref(), Some("still typing"));
    assert_eq!(
        set.unsaved_draft_count(true, &Default::default()),
        0,
        "`s` brings it back"
    );
    assert_eq!(
        set.unsaved_draft_count(false, &Default::default()),
        1,
        "a window that keeps no tabs loses it"
    );
    set.set_draft(tab, &"x".repeat(crate::saved_tabs::DRAFT_SAVE_MAX + 1));
    assert_eq!(set.saved_snapshot(PROJECT).unwrap().tabs[0].draft, None);
    assert_eq!(
        set.unsaved_draft_count(true, &Default::default()),
        1,
        "too long for the file"
    );
    set.set_draft(tab, "   ");
    assert_eq!(
        set.unsaved_draft_count(true, &Default::default()),
        0,
        "nothing was typed"
    );

    let fresh = set.open(AgentId::Claude);
    set.set_draft(fresh, "no session yet");
    assert_eq!(set.unsaved_draft_count(true, &Default::default()), 1);
    // Words held for the editor go to the draft at a close, ahead of what was typed since: one tab, counted once,
    // and judged as the whole text, which can be too long for the file.
    let held = std::collections::BTreeMap::from([(tab, "@buffers why".to_owned()), (fresh, "held".to_owned())]);
    set.set_draft(tab, "and then");
    assert_eq!(
        set.unsaved_draft_count(true, &held),
        1,
        "`tab` is saved with both, `fresh` is not saved"
    );
    assert_eq!(
        set.unsaved_draft_count(false, &held),
        2,
        "no saved tabs: both lose theirs"
    );
    // Each fits the file, both together do not: the tab is judged by what the close would save.
    let half = "y".repeat(crate::saved_tabs::DRAFT_SAVE_MAX / 2 + 10);
    set.set_draft(tab, &half);
    let held = std::collections::BTreeMap::from([(tab, half.clone())]);
    assert_eq!(
        set.unsaved_draft_count(true, &held),
        2,
        "held words and draft are one text, too long: and `fresh`"
    );
    set.set_draft(tab, "   ");
    let held = std::collections::BTreeMap::from([(tab, "x".repeat(crate::saved_tabs::DRAFT_SAVE_MAX + 1))]);
    assert_eq!(
        set.unsaved_draft_count(true, &held),
        2,
        "held words too long for the file, and `fresh`"
    );
    shut_down_all(&mut set);
}

/// Only a failed restored tab is cleaned up: a live session is never closed by this.
#[test]
fn discarding_leaves_any_tab_that_is_not_failed_alone() {
    let mut set = set();
    let tab = set.active();
    set.discard_failed_restore(tab, Some((SessionModeChoice::Auto, AgentId::Claude)));
    let other = set.open(AgentId::Claude);
    set.discard_failed_restore(other, None);
    assert_eq!(set.tabs().len(), 2);
}

/// What `shell` does every tick: show the memory the window's tabs. Each listed change reaches the
/// file once; an empty tab, a repeat and the window closing reach it not at all.
#[test]
fn the_saved_file_follows_the_tabs_and_never_records_the_window_closing() {
    use crate::saved_tabs::{load, Loaded, TabMemory};
    let dir = workspace("tabs-memory");
    let state = crate::test_scratch_dir::ScratchDir::new("nv-tab-memory", "follows");
    let root = PathBuf::from("/home/user/project");
    let mut memory = TabMemory::new(Some(state.to_path_buf()), root.clone());
    let mut set = set();
    let show = |set: &TabSet, memory: &mut TabMemory| memory.observe(set.saved_snapshot(PROJECT)).is_some();
    let on_disk = |state: &Path| match load(state, &root) {
        Loaded::Saved(saved) => saved,
        other => panic!("expected a saved file, found {other:?}"),
    };

    assert!(
        !show(&set, &mut memory),
        "a launch that has done nothing writes nothing"
    );

    let first = set.active();
    give_session(&mut set, first, &dir, "s-one");
    assert!(show(&set, &mut memory), "a session adopted");
    assert_eq!(ids(&on_disk(&state)), ["s-one"]);
    assert!(!show(&set, &mut memory), "the same tabs again");

    set.rename(first, "api");
    assert!(show(&set, &mut memory), "renamed");
    assert_eq!(on_disk(&state).tabs[0].name.as_deref(), Some("api"));

    let empty = set.open(AgentId::Claude);
    assert!(
        !show(&set, &mut memory),
        "an empty tab is not listed, and neither is its being active"
    );
    let second = set.open(AgentId::Claude);
    give_session(&mut set, second, &dir, "s-two");
    assert!(show(&set, &mut memory), "a second session");
    assert_eq!(on_disk(&state).active, Some(1));

    set.select(first);
    assert!(show(&set, &mut memory), "the active tab switched");
    assert_eq!(on_disk(&state).active, Some(0));

    set.get_mut(second).unwrap().mode = SessionModeChoice::Bypass;
    assert!(show(&set, &mut memory), "its mode changed");
    assert_eq!(on_disk(&state).tabs[1].mode, SessionModeChoice::Bypass);

    let gone = set.remove(empty);
    assert!(gone.is_some());
    assert!(!show(&set, &mut memory), "closing an empty tab changes nothing listed");
    set.remove(second);
    assert!(show(&set, &mut memory), "a listed tab closed");
    assert_eq!(ids(&on_disk(&state)), ["s-one"]);

    // The window closing takes every tab to tear them down: nothing is recorded.
    let before = std::fs::read_to_string(state.join(crate::layout::persist::file_name(&root))).unwrap();
    let taken = set.take_all();
    assert!(!show(&set, &mut memory));
    for tab in taken {
        if let Some(retired) = tab.retired {
            retired.into_backend().shutdown();
        }
    }
    assert!(!show(&set, &mut memory));
    let after = std::fs::read_to_string(state.join(crate::layout::persist::file_name(&root))).unwrap();
    assert_eq!(before, after, "the file still says what was open");
}
