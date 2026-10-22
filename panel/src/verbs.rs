//! Verbs a message or key runs on a tab or a view: the mode cycles and bypass confirmation, the tab verbs,
//! the navigation and keymap hooks.

use super::*;

/// The log line for `init.lua`'s `agent.default_mode` = `configured`, given the mode this project
/// remembers from Shift+Tab, if any, which beats it.
pub(super) fn default_mode_said(configured: SessionModeChoice, remembered: Option<SessionModeChoice>) -> String {
    match remembered {
        Some(kept) => format!(
            "[agent] init.lua's agent.default_mode is {}, but this project remembers {} (Shift+Tab on an empty \
             tab), which wins: new tabs start in {}",
            configured.as_str(),
            kept.as_str(),
            kept.as_str()
        ),
        None => format!(
            "[agent] new tabs start in {} (init.lua's agent.default_mode){}",
            configured.as_str(),
            if configured == SessionModeChoice::Manual {
                "; a tab of an agent with no manual mode starts in auto"
            } else {
                ""
            }
        ),
    }
}

/// The `CycleMode` arm's WebView-free body: `TabSet::cycle_mode` on `tab`, plus remembering the mode an
/// empty tab moved to, for the next empty tab and the next launch. `TabSet::cycle_mode` can reach
/// `Changed` on a live tab too, so whether the tab was empty is read before the call. Only a mode the
/// user moved to on a tab that offers every mode is remembered: a tab whose agent has no manual takes
/// auto in its place when it leaves bypass, which nobody chose, and remembered it would beat
/// `init.lua`'s default on every later launch. A failure to write is logged, not refused: the mode
/// itself did change.
pub(super) fn cycle_tab_mode(
    state: &mut AgentPanelState,
    tab: TabId,
    page: ViewId,
) -> Result<eitri_core::tab_set::ModeCycle, String> {
    let remembered_if_moved = state
        .tabs
        .get(tab)
        .is_some_and(|t| matches!(t.backend(), TabBackend::NotStarted) && !t.agent().mode_fixed_at_start());
    let outcome = state.tabs.cycle_mode(tab, page);
    if let Ok(eitri_core::tab_set::ModeCycle::Changed(mode)) = &outcome {
        if remembered_if_moved {
            if let Some(dir) = &state.prefs_dir {
                if let Err(e) = eitri_core::agent_prefs::save_mode(dir, &state.project_dir, *mode) {
                    eprintln!("[agent_panel] could not remember the permission mode: {e}");
                }
            }
        }
    }
    outcome
}

/// `cycle_default_mode`: `TabSet::cycle_default_mode` plus remembering the new default on
/// `Changed(Auto)` only -- bypass is never written, and a `Confirm` is not persisted here
/// either (only actually entering bypass, via `apply_confirm_bypass` below, ever could -- and that is
/// forbidden too). A failure to write is logged, not refused, since the mode itself did change.
/// Never touches the WebView, so it is tested directly; the arm above owns `send_tabs` and
/// dispatching the prompt.
pub(super) fn cycle_default_mode(state: &mut AgentPanelState, page: ViewId) -> eitri_core::tab_set::ModeCycle {
    let outcome = state.tabs.cycle_default_mode(page);
    if let eitri_core::tab_set::ModeCycle::Changed(mode) = outcome {
        if let Some(dir) = &state.prefs_dir {
            if let Err(e) = eitri_core::agent_prefs::save_mode(dir, &state.project_dir, mode) {
                eprintln!("[agent_panel] could not remember the permission mode: {e}");
            }
        }
    }
    outcome
}

/// `confirm_bypass`: the `ConfirmBypass` arm's WebView-free body, the same split `cycle_default_mode`
/// gets above. `TabSet::confirm_bypass` does all of the checking (the re-check against exactly the
/// delivered cards, the nonce, the active tab, ended/failed) and bypass is never written to
/// disk, so there is nothing else for this wrapper to do -- it exists so a test can drive the exact
/// call the arm makes without a WebView, the same way `cycle_default_mode` can.
pub(super) fn apply_confirm_bypass(
    state: &mut AgentPanelState,
    scope: eitri_core::tab_set::BypassScope,
    nonce: u64,
    page: ViewId,
) -> Result<eitri_core::tab_set::ConfirmOutcome, String> {
    state.tabs.confirm_bypass(scope, nonce, page)
}

/// `tab_verb`: owes the hook `main.rs` installs via `on_tab_verb` (`run_tab_action`) the verb, mapped from
/// the wire shape -- or a refusal naming why there is none yet (a legacy backend at startup, before
/// `main.rs` has wired it). The hook goes back through this panel's state (`switch_with`, `new_tab`), so
/// it runs once the step is over. Never touches the WebView, so it is tested directly.
pub(super) fn run_tab_verb(
    state: &AgentPanelState,
    view: ViewId,
    verb: TabVerbWire,
    out: &mut Outbox,
) -> Result<(), &'static str> {
    if state.tab_verb_hook.is_none() {
        return Err("tab verbs are not wired");
    }
    out.effect(Effect::TabVerb(view, tab_action_for(verb)));
    Ok(())
}

/// `tab_verb new` with an agent word (`:tabnew <agent>`, typed in `view`'s page): opens and selects a
/// base-view tab that runs the agent the word names, or refuses naming the words that are accepted.
/// Touches no page, so it is tested directly.
pub(super) fn open_tab_named(state: &mut AgentPanelState, view: ViewId, word: &str) -> Result<(), String> {
    let registered: Vec<&str> = state.acp_agents.iter().map(|a| a.name.as_str()).collect();
    let Some(agent) = eitri_core::agent_prefs::agent_named(word, &registered) else {
        let mut accepted = vec!["claude", "codex"];
        accepted.extend(registered);
        return Err(format!(
            "unknown agent {word} — :tabnew accepts {}",
            accepted.join(", ")
        ));
    };
    state.tabs.drop_bypass_prompt_in(view);
    state.tabs.open(agent);
    Ok(())
}

/// `nav_fallthrough`: owes the hook `main.rs` installs via `on_nav_fallthrough` the direction the page
/// could not apply -- or a refusal naming why there is none yet. Never touches the WebView, so it is
/// tested directly, the same shape as `run_tab_verb`.
pub(super) fn run_nav_fallthrough(
    state: &AgentPanelState,
    view: ViewId,
    direction: NavKeyDirection,
    out: &mut Outbox,
) -> Result<(), &'static str> {
    if state.nav_fallthrough_hook.is_none() {
        return Err("nav fallthrough is not wired");
    }
    let direction = match direction {
        NavKeyDirection::Down => Direction::Down,
        NavKeyDirection::Up => Direction::Up,
    };
    out.effect(Effect::Nav(view, direction));
    Ok(())
}

/// `pane_nav`: the same hook `nav_fallthrough` uses -- `main.rs` installs it as
/// `move_focus(&ModuleId::agent(), direction)`, exactly what `Ctrl+h/j/k/l` do from the panel.
pub(super) fn run_pane_nav(
    state: &AgentPanelState,
    view: ViewId,
    direction: PaneNavDirection,
    out: &mut Outbox,
) -> Result<(), &'static str> {
    if state.nav_fallthrough_hook.is_none() {
        return Err("pane nav is not wired");
    }
    let direction = match direction {
        PaneNavDirection::Left => Direction::Left,
        PaneNavDirection::Down => Direction::Down,
        PaneNavDirection::Up => Direction::Up,
        PaneNavDirection::Right => Direction::Right,
    };
    out.effect(Effect::Nav(view, direction));
    Ok(())
}

/// `keymap_closed`: owes the hook `main.rs` installs via `on_keymap_closed` the opening and how it closed.
/// What it does (moving the keys) re-enters the panel through `pane_focus`, so it runs once the step is
/// over. With no hook it does nothing.
pub(super) fn run_keymap_closed(opening: u64, return_keys: bool, out: &mut Outbox) {
    out.effect(Effect::KeymapClosed { opening, return_keys });
}

/// `tab_verb`'s wire shape to the `TabAction` the prefix's own `Action::Tab` arm runs.
pub(super) fn tab_action_for(verb: TabVerbWire) -> TabAction {
    match verb {
        TabVerbWire::Next => TabAction::Next,
        TabVerbWire::Prev => TabAction::Prev,
        TabVerbWire::Last => TabAction::Last,
        TabVerbWire::New => TabAction::New,
        TabVerbWire::Close => TabAction::Close,
        TabVerbWire::CloseOthers => TabAction::CloseOthers,
        TabVerbWire::Choose => TabAction::Choose,
        TabVerbWire::Info => TabAction::Info,
    }
}

/// Which `Ctrl+j`/`Ctrl+k` the panel takes for its composer: only `Ctrl+j` (`Direction::Down`) while the
/// `panel_keys` mirror reports `Browse`, or `Ctrl+k` (`Direction::Up`) while it reports `Input`; every
/// other combination goes to the host's focus move exactly as it would without an intercept. Pure, so
/// the whole table is a unit test with no display.
pub(super) fn claims(mirror: PanelKeys, dir: Direction) -> bool {
    matches!(
        (dir, mirror),
        (Direction::Down, PanelKeys::Browse) | (Direction::Up, PanelKeys::Input)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_panel::tests::*;

    /// The full decision table: the two claimed cells (`Down`+`Browse`, `Up`+`Input`) and every other
    /// combination, which must fall through to the host's focus move unclaimed.
    #[test]
    fn claims_only_down_in_browse_and_up_in_input() {
        for dir in [Direction::Left, Direction::Down, Direction::Up, Direction::Right] {
            for mirror in [PanelKeys::Browse, PanelKeys::Input, PanelKeys::Other] {
                let expected = matches!(
                    (dir, mirror),
                    (Direction::Down, PanelKeys::Browse) | (Direction::Up, PanelKeys::Input)
                );
                assert_eq!(claims(mirror, dir), expected, "{dir:?} + {mirror:?}");
            }
        }
    }

    /// `tab_verb` refuses naming why until `main.rs` installs a hook
    /// (`AgentPanelHandle::on_tab_verb`), and once one is installed it is called with the mapped
    /// `TabAction` -- every wire value, not just one, since a mapping this exhaustive is easy to
    /// get wrong silently for the one case nobody wrote a test for.
    #[test]
    fn tab_verb_refuses_with_no_hook_and_calls_the_installed_hook_with_the_mapped_action() {
        let state = state_for_hooks(TabSet::new(
            BackendKind::Sidecar,
            eitri_core::agent_bridge::SessionModeChoice::Auto,
        ));
        assert_eq!(
            stepped(&state, |state, out| run_tab_verb(
                state,
                ViewId::BASE,
                TabVerbWire::Close,
                out
            )),
            Err("tab verbs are not wired")
        );

        let seen: Rc<RefCell<Vec<TabAction>>> = Rc::new(RefCell::new(Vec::new()));
        {
            let seen = seen.clone();
            state.borrow_mut().tab_verb_hook = Some(Rc::new(move |_view, action| seen.borrow_mut().push(action)));
        }
        for (verb, action) in [
            (TabVerbWire::Next, TabAction::Next),
            (TabVerbWire::Prev, TabAction::Prev),
            (TabVerbWire::Last, TabAction::Last),
            (TabVerbWire::New, TabAction::New),
            (TabVerbWire::Close, TabAction::Close),
            (TabVerbWire::CloseOthers, TabAction::CloseOthers),
            (TabVerbWire::Choose, TabAction::Choose),
            (TabVerbWire::Info, TabAction::Info),
        ] {
            assert_eq!(
                stepped(&state, |state, out| run_tab_verb(state, ViewId::BASE, verb, out)),
                Ok(())
            );
            assert_eq!(seen.borrow_mut().pop(), Some(action), "{verb:?} -> {action:?}");
        }
    }

    /// The product hook (`main.rs`'s `run_tab_action`) re-enters this
    /// same state with `borrow_mut` (`switch_with`, `new_tab`). `run_tab_verb` must not still hold
    /// its own borrow while the hook runs, or every panel tab key panics on the GTK thread.
    #[test]
    fn tab_verb_hook_may_borrow_the_panel_state_mutably() {
        let state = state_for_hooks(TabSet::new(
            BackendKind::Sidecar,
            eitri_core::agent_bridge::SessionModeChoice::Auto,
        ));
        let ran = Rc::new(RefCell::new(0usize));
        {
            let weak = Rc::downgrade(&state);
            let ran = ran.clone();
            state.borrow_mut().tab_verb_hook = Some(Rc::new(move |_view, _action| {
                let state = weak.upgrade().expect("state alive");
                let _guard = state.borrow_mut();
                *ran.borrow_mut() += 1;
            }));
        }
        assert_eq!(
            stepped(&state, |state, out| run_tab_verb(
                state,
                ViewId::BASE,
                TabVerbWire::Next,
                out
            )),
            Ok(())
        );
        assert_eq!(*ran.borrow(), 1);
    }

    /// The nav-mode mirror: `handle_inbound_message`'s `PanelKeys` arm is what
    /// `AgentPanelHandle::panel_keys` reads back, and it starts `Other`.
    #[test]
    fn a_panel_keys_message_updates_the_mirror_handle_inbound_message_records() {
        let state = state_for_hooks(TabSet::new(
            BackendKind::Sidecar,
            eitri_core::agent_bridge::SessionModeChoice::Auto,
        ));
        assert_eq!(
            state.borrow().page(ViewId::BASE).unwrap().nav_mode.get(),
            PanelKeys::Other
        );
        for mode in [PanelKeys::Browse, PanelKeys::Input, PanelKeys::Other] {
            state.borrow().page(ViewId::BASE).unwrap().nav_mode.set(mode);
            assert_eq!(state.borrow().page(ViewId::BASE).unwrap().nav_mode.get(), mode);
        }
    }

    /// The same shape as `tab_verb_refuses_with_no_hook_and_calls_the_installed_hook_with_the_mapped_action`:
    /// a refusal naming why until `main.rs` installs a hook, then called with the mapped
    /// `eitri_core::layout::Direction` -- both wire values, not just one.
    #[test]
    fn nav_fallthrough_refuses_with_no_hook_and_calls_the_installed_hook_with_the_mapped_direction() {
        let state = state_for_hooks(TabSet::new(
            BackendKind::Sidecar,
            eitri_core::agent_bridge::SessionModeChoice::Auto,
        ));
        assert_eq!(
            stepped(&state, |state, out| run_nav_fallthrough(
                state,
                ViewId::BASE,
                NavKeyDirection::Down,
                out
            )),
            Err("nav fallthrough is not wired")
        );

        let seen: Rc<RefCell<Vec<Direction>>> = Rc::new(RefCell::new(Vec::new()));
        {
            let seen = seen.clone();
            state.borrow_mut().nav_fallthrough_hook =
                Some(Rc::new(move |_view, direction| seen.borrow_mut().push(direction)));
        }
        for (wire, direction) in [
            (NavKeyDirection::Down, Direction::Down),
            (NavKeyDirection::Up, Direction::Up),
        ] {
            assert_eq!(
                stepped(&state, |state, out| run_nav_fallthrough(state, ViewId::BASE, wire, out)),
                Ok(())
            );
            assert_eq!(seen.borrow_mut().pop(), Some(direction), "{wire:?} -> {direction:?}");
        }
    }

    /// `pane_nav` runs the very hook `nav_fallthrough` does -- `main.rs`'s
    /// `move_focus(&ModuleId::agent(), direction)` -- so `Ctrl+w h/j/k/l` moves the keys exactly as
    /// `Ctrl+h/j/k/l` do from the panel. A refusal naming why until `main.rs` installs it, then called
    /// with the mapped `Direction`, once per message, for all four wire values (the move is left to
    /// the hook: a side with no module is its business, not this function's).
    #[test]
    fn pane_nav_refuses_with_no_hook_and_calls_the_installed_hook_with_the_mapped_direction() {
        let state = state_for_hooks(TabSet::new(
            BackendKind::Sidecar,
            eitri_core::agent_bridge::SessionModeChoice::Auto,
        ));
        for wire in [
            PaneNavDirection::Left,
            PaneNavDirection::Down,
            PaneNavDirection::Up,
            PaneNavDirection::Right,
        ] {
            assert_eq!(
                stepped(&state, |state, out| run_pane_nav(state, ViewId::BASE, wire, out)),
                Err("pane nav is not wired"),
                "{wire:?}"
            );
        }

        let seen: Rc<RefCell<Vec<Direction>>> = Rc::new(RefCell::new(Vec::new()));
        {
            let seen = seen.clone();
            state.borrow_mut().nav_fallthrough_hook =
                Some(Rc::new(move |_view, direction| seen.borrow_mut().push(direction)));
        }
        for (wire, direction) in [
            (PaneNavDirection::Left, Direction::Left),
            (PaneNavDirection::Down, Direction::Down),
            (PaneNavDirection::Up, Direction::Up),
            (PaneNavDirection::Right, Direction::Right),
        ] {
            assert_eq!(
                stepped(&state, |state, out| run_pane_nav(state, ViewId::BASE, wire, out)),
                Ok(())
            );
            assert_eq!(seen.borrow_mut().pop(), Some(direction), "{wire:?} -> {direction:?}");
            assert!(
                seen.borrow().is_empty(),
                "{wire:?}: the hook is called once per message"
            );
        }
    }

    /// `keymap_closed` reaches the installed hook with the opening and the `return_keys` the page sent, once
    /// per message, and holds no borrow of the state while the hook runs (the hook moves the keys, which
    /// re-enters the panel).
    #[test]
    fn a_keymap_closed_reaches_the_hook_with_its_opening_and_return_keys() {
        let state = state_for_hooks(TabSet::new(
            BackendKind::Sidecar,
            eitri_core::agent_bridge::SessionModeChoice::Auto,
        ));
        let seen: Rc<RefCell<Vec<(u64, bool)>>> = Rc::new(RefCell::new(Vec::new()));
        let reentrant = Rc::downgrade(&state);
        {
            let seen = seen.clone();
            state.borrow_mut().keymap_closed_hook = Some(Rc::new(move |opening, return_keys| {
                let state = reentrant.upgrade().expect("the state outlives the call");
                state.borrow_mut().page_mut(ViewId::BASE).pane_focused = true;
                seen.borrow_mut().push((opening, return_keys));
            }));
        }
        stepped(&state, |_, out| run_keymap_closed(7, true, out));
        stepped(&state, |_, out| run_keymap_closed(8, false, out));
        assert_eq!(*seen.borrow(), vec![(7, true), (8, false)]);
        assert!(state.borrow().page(ViewId::BASE).is_some_and(|page| page.pane_focused));
    }

    /// The companion window and `eitri-mac` install no hook: the message is accepted and nothing happens.
    #[test]
    fn a_keymap_closed_without_a_hook_does_nothing() {
        let state = state_for_hooks(TabSet::new(
            BackendKind::Sidecar,
            eitri_core::agent_bridge::SessionModeChoice::Auto,
        ));
        assert!(state.borrow().keymap_closed_hook.is_none());
        stepped(&state, |_, out| run_keymap_closed(3, true, out));
        assert!(!state.borrow().page(ViewId::BASE).is_some_and(|page| page.pane_focused));
    }

    /// A raw `keymap_closed` through the inbound dispatch is answered with an ok `command_result` for its
    /// request (never an error toast) with or without a hook, and the hook gets the opening and
    /// `return_keys` the message carried.
    #[test]
    fn a_raw_keymap_closed_is_answered_ok_with_or_without_a_hook() {
        let (handle, page, _host) = handle_with_recording_page();
        let seen: Rc<RefCell<Vec<(u64, bool)>>> = Rc::new(RefCell::new(Vec::new()));
        let result = |request: &str| -> Option<(bool, String)> {
            page.sent.borrow().iter().find_map(|envelope| {
                let v: serde_json::Value = serde_json::from_str(envelope).unwrap();
                (v["kind"] == "command_result" && v["requestId"] == request)
                    .then(|| (v["ok"] == true, v["error"].as_str().unwrap_or("").to_string()))
            })
        };

        handle.page_message(
            ViewId::BASE,
            r#"{"type":"keymap_closed","request_id":"k1","opening":4,"return_keys":true}"#,
        );
        assert_eq!(result("k1"), Some((true, String::new())), "no hook: accepted, no error");

        {
            let seen = seen.clone();
            handle.on_keymap_closed(move |opening, return_keys| seen.borrow_mut().push((opening, return_keys)));
        }
        handle.page_message(
            ViewId::BASE,
            r#"{"type":"keymap_closed","request_id":"k2","opening":5,"return_keys":false}"#,
        );
        assert_eq!(result("k2"), Some((true, String::new())), "a hook: accepted, no error");
        assert_eq!(
            *seen.borrow(),
            vec![(5, false)],
            "only the message sent with a hook reaches it"
        );
    }

    /// The hook `main.rs` installs goes back through the panel's state (`move_focus` reads and writes
    /// it), so `run_pane_nav` must not hold a borrow of the state across the call -- the same reason
    /// `run_tab_verb` clones its hook out first. A hook that borrows the state mutably panics here if
    /// it does.
    #[test]
    fn pane_nav_does_not_hold_the_state_borrowed_while_the_hook_runs() {
        let state = state_for_hooks(TabSet::new(
            BackendKind::Sidecar,
            eitri_core::agent_bridge::SessionModeChoice::Auto,
        ));
        let reentrant = Rc::downgrade(&state);
        state.borrow_mut().nav_fallthrough_hook = Some(Rc::new(move |_, _| {
            let state = reentrant.upgrade().expect("the state outlives the call");
            state.borrow_mut().page_mut(ViewId::BASE).pane_focused = true;
        }));
        assert_eq!(
            stepped(&state, |state, out| run_pane_nav(
                state,
                ViewId::BASE,
                PaneNavDirection::Right,
                out
            )),
            Ok(())
        );
        assert!(state.borrow().page(ViewId::BASE).unwrap().pane_focused);
    }

    /// Bypass is never remembered on disk: `save_mode` refuses it outright. Auto -> bypass
    /// always asks first and writes nothing either way; leaving bypass (to manual) and manual ->
    /// auto are remembered.
    #[test]
    fn cycle_default_mode_moves_the_default_and_never_remembers_bypass_on_disk() {
        let dir = agent::state_dirs::test_workspace_dir("panel-cycle-default-mode");
        let prefs_dir = dir.join("prefs");
        let project_dir = dir.join("project");
        std::fs::create_dir_all(&project_dir).unwrap();
        let set = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
        let open_tab = set.active();
        let state = state_for_hooks(set);
        state.borrow_mut().prefs_dir = Some(prefs_dir.clone());
        state.borrow_mut().project_dir = project_dir.clone();

        let cycled = cycle_default_mode(&mut state.borrow_mut(), ViewId::BASE);
        let plan = match cycled {
            eitri_core::tab_set::ModeCycle::Confirm(plan) => plan,
            other => panic!("auto -> bypass must ask first: {other:?}"),
        };
        assert_eq!(
            state.borrow().tabs.default_mode(),
            SessionModeChoice::Auto,
            "nothing moved yet"
        );
        assert_eq!(
            eitri_core::agent_prefs::load_mode(&prefs_dir, &project_dir),
            eitri_core::agent_prefs::LoadedMode::Missing,
            "nothing on disk yet"
        );

        // Confirming is `apply_confirm_bypass`'s job, which is `TabSet::confirm_bypass`
        // and nothing else -- it moves the in-memory default and still writes nothing.
        assert_eq!(
            apply_confirm_bypass(&mut state.borrow_mut(), plan.scope, plan.nonce, ViewId::BASE),
            Ok(eitri_core::tab_set::ConfirmOutcome::Entered { approved: 0 })
        );
        assert_eq!(state.borrow().tabs.default_mode(), SessionModeChoice::Bypass);
        // Never the open tab's own mode (still `NotStarted`, so `cycle_mode` alone would move it;
        // `confirm_bypass(Default, ..)` must not).
        assert_eq!(set_mode(&state, open_tab), SessionModeChoice::Auto);
        assert_eq!(
            eitri_core::agent_prefs::load_mode(&prefs_dir, &project_dir),
            eitri_core::agent_prefs::LoadedMode::Missing,
            "still nothing on disk while bypass"
        );

        // Leaving bypass moves at once, to manual, and IS remembered -- manual is allowed.
        assert_eq!(
            cycle_default_mode(&mut state.borrow_mut(), ViewId::BASE),
            eitri_core::tab_set::ModeCycle::Changed(SessionModeChoice::Manual)
        );
        assert_eq!(state.borrow().tabs.default_mode(), SessionModeChoice::Manual);
        assert_eq!(
            eitri_core::agent_prefs::load_mode(&prefs_dir, &project_dir),
            eitri_core::agent_prefs::LoadedMode::Remembered(SessionModeChoice::Manual)
        );
        // Manual -> auto moves at once and is remembered too.
        assert_eq!(
            cycle_default_mode(&mut state.borrow_mut(), ViewId::BASE),
            eitri_core::tab_set::ModeCycle::Changed(SessionModeChoice::Auto)
        );
        assert_eq!(
            eitri_core::agent_prefs::load_mode(&prefs_dir, &project_dir),
            eitri_core::agent_prefs::LoadedMode::Remembered(SessionModeChoice::Auto)
        );
    }

    /// The log says what new tabs start in: init.lua's mode, or the remembered one that beats it.
    #[test]
    fn the_default_mode_log_names_the_mode_that_wins() {
        let said = default_mode_said(SessionModeChoice::Manual, None);
        assert!(said.starts_with("[agent] new tabs start in manual (init.lua"), "{said}");
        let said = default_mode_said(SessionModeChoice::Manual, Some(SessionModeChoice::Auto));
        assert!(said.ends_with("new tabs start in auto"), "{said}");
        assert!(said.contains("agent.default_mode is manual"), "{said}");
    }

    /// Shift+Tab on an empty tab remembers the mode it moved to, except where the tab's agent has no
    /// manual: leaving bypass there lands on auto only because that agent offers nothing stricter, so
    /// nothing is written and init.lua's manual default is not beaten on the next launch. The window
    /// default stays manual.
    #[test]
    fn leaving_bypass_on_an_empty_acp_tab_remembers_nothing() {
        let dir = agent::state_dirs::test_workspace_dir("panel-cycle-acp-tab");
        let prefs_dir = dir.join("prefs");
        let project_dir = dir.join("project");
        std::fs::create_dir_all(&project_dir).unwrap();
        let mut set = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Manual);
        let claude = set.active();
        let acp = set.open(eitri_core::tab_set::AgentId::Acp("gem".into()));
        let state = state_for_hooks(set);
        state.borrow_mut().prefs_dir = Some(prefs_dir.clone());
        state.borrow_mut().project_dir = project_dir.clone();
        assert_eq!(set_mode(&state, acp), SessionModeChoice::Auto);

        let cycled = cycle_tab_mode(&mut state.borrow_mut(), acp, ViewId::BASE);
        let plan = match cycled {
            Ok(eitri_core::tab_set::ModeCycle::Confirm(plan)) => plan,
            other => panic!("auto -> bypass asks: {other:?}"),
        };
        apply_confirm_bypass(&mut state.borrow_mut(), plan.scope, plan.nonce, ViewId::BASE).unwrap();
        assert_eq!(set_mode(&state, acp), SessionModeChoice::Bypass);
        assert_eq!(
            cycle_tab_mode(&mut state.borrow_mut(), acp, ViewId::BASE),
            Ok(eitri_core::tab_set::ModeCycle::Changed(SessionModeChoice::Auto))
        );
        assert_eq!(state.borrow().tabs.default_mode(), SessionModeChoice::Manual);
        assert_eq!(
            eitri_core::agent_prefs::load_mode(&prefs_dir, &project_dir),
            eitri_core::agent_prefs::LoadedMode::Missing,
            "a stand-in mode is not remembered"
        );

        // The same move on an empty Claude tab is remembered, as before.
        state.borrow_mut().tabs.select(claude);
        assert_eq!(
            cycle_tab_mode(&mut state.borrow_mut(), claude, ViewId::BASE),
            Ok(eitri_core::tab_set::ModeCycle::Changed(SessionModeChoice::Auto))
        );
        assert_eq!(
            eitri_core::agent_prefs::load_mode(&prefs_dir, &project_dir),
            eitri_core::agent_prefs::LoadedMode::Remembered(SessionModeChoice::Auto)
        );
    }

    /// `apply_confirm_bypass` -- the `ConfirmBypass` arm's WebView-free body -- moves a
    /// TAB's own mode (not only the window default the test above covers) and, like every path into
    /// bypass, never writes it to the prefs dir either way.
    #[test]
    fn apply_confirm_bypass_moves_a_live_tabs_mode_and_never_touches_prefs() {
        let dir = agent::state_dirs::test_workspace_dir("panel-apply-confirm-bypass-tab");
        let prefs_dir = dir.join("prefs");
        let project_dir = dir.join("project");
        std::fs::create_dir_all(&project_dir).unwrap();
        let set = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
        let tab = set.active();
        let state = state_for_hooks(set);
        state.borrow_mut().prefs_dir = Some(prefs_dir.clone());
        state.borrow_mut().project_dir = project_dir.clone();

        let cycled = state.borrow_mut().tabs.cycle_mode(tab, ViewId::BASE);
        let plan = match cycled {
            Ok(eitri_core::tab_set::ModeCycle::Confirm(plan)) => plan,
            other => panic!("an empty auto tab must ask first: {other:?}"),
        };
        assert_eq!(
            apply_confirm_bypass(&mut state.borrow_mut(), plan.scope, plan.nonce, ViewId::BASE),
            Ok(eitri_core::tab_set::ConfirmOutcome::Entered { approved: 0 })
        );
        assert_eq!(set_mode(&state, tab), SessionModeChoice::Bypass);
        assert_eq!(
            eitri_core::agent_prefs::load_mode(&prefs_dir, &project_dir),
            eitri_core::agent_prefs::LoadedMode::Missing,
            "bypass is never written, even the one apply_confirm_bypass itself enters"
        );
    }

    /// `:tabnew <agent>` opens a tab of the agent the word names and refuses any other word, saying
    /// which are accepted; the new-tab key's tab runs `agent.default`.
    #[test]
    fn tabnew_opens_the_named_agent_and_refuses_other_words() {
        let state = state_for_hooks(TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto));
        assert_eq!(open_tab_named(&mut state.borrow_mut(), ViewId::BASE, "codex"), Ok(()));
        let active = state.borrow().tabs.active();
        assert_eq!(state.borrow().tabs.get(active).unwrap().agent(), &AgentId::Codex);
        assert_eq!(open_tab_named(&mut state.borrow_mut(), ViewId::BASE, "claude"), Ok(()));
        let active = state.borrow().tabs.active();
        assert_eq!(state.borrow().tabs.get(active).unwrap().agent(), &AgentId::Claude);
        let before = state.borrow().tabs.tabs().len();
        let refused = open_tab_named(&mut state.borrow_mut(), ViewId::BASE, "gemini").unwrap_err();
        assert!(
            refused.contains("gemini") && refused.contains("claude, codex"),
            "{refused}"
        );
        assert_eq!(state.borrow().tabs.tabs().len(), before, "a refused word opens nothing");
    }

    /// `agent.default` reaches the tab the window already has (empty, unstarted) and every later one;
    /// a started tab keeps its agent.
    #[test]
    fn the_default_agent_applies_to_empty_tabs_only() {
        let mut set = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
        let first = set.active();
        let named = set.open(AgentId::Claude);
        set.get_mut(named).unwrap().name = Some("docs".into());
        set.apply_configured_agent(AgentId::Codex);
        assert_eq!(set.get(first).unwrap().agent(), &AgentId::Codex);
        assert_eq!(
            set.get(named).unwrap().agent(),
            &AgentId::Claude,
            "a tab the user named is theirs"
        );
    }

    /// What `prefix x` on an extra view names: the label of the one tab it shows, as its strip does, and
    /// nothing for a view with no tab or for the base view, whose kill closes every base-view tab.
    #[test]
    fn an_extra_views_kill_names_its_one_tab() {
        let (handle, _base, _second) = handle_with_two_pages();
        assert_eq!(handle.view_tab_label(view(2)), None, "no tab yet");
        assert_eq!(handle.view_running_queued(view(2)), (0, 0));
        let tab = handle.open_in(view(2)).unwrap();
        let number = handle.state.borrow().tabs.get(tab).unwrap().number;
        assert_eq!(handle.view_tab_label(view(2)), Some(format!("{number} new")));
        assert_eq!(handle.view_running_queued(view(2)), (0, 0), "an empty tab runs nothing");
        assert_eq!(handle.view_tab_label(ViewId::BASE), None);
        assert_eq!(handle.view_tab_label(view(3)), None);
    }

    #[test]
    fn cycle_mode_into_bypass_from_an_extra_view_shows_the_prompt_there_and_takes_its_confirmation() {
        let (handle, base, second) = handle_with_two_pages();
        let tab = handle.open_in(view(2)).unwrap();
        assert_eq!(mode_of(&handle, tab), SessionModeChoice::Auto);
        let asked = recording_gate(&handle, &[], Rc::new(Cell::new(false)));
        clear(&[&base, &second]);
        cycle_mode_from(&handle, view(2), "m1", tab);
        assert_eq!(result_for(&second.sent.borrow(), "m1"), Some(Ok(())));
        let (prompt_tab, nonce) = last_bypass_prompt(&second).expect("the prompt, in page 2");
        assert_eq!(prompt_tab, Some(tab.0));
        assert_eq!(last_bypass_prompt(&base), None, "the base page is shown nothing");

        // The base page's `y` with the same tab and nonce is not page 2's answer, and leaves its prompt.
        confirm_bypass_from(&handle, ViewId::BASE, "y1", tab, nonce);
        assert!(
            matches!(result_for(&base.sent.borrow(), "y1"), Some(Err(why)) if why.contains("not shown in this page"))
        );
        assert_eq!(mode_of(&handle, tab), SessionModeChoice::Auto);
        assert!(asked.borrow().is_empty(), "refused before the window is asked");

        confirm_bypass_from(&handle, view(2), "y2", tab, nonce);
        assert_eq!(result_for(&second.sent.borrow(), "y2"), Some(Ok(())));
        assert_eq!(mode_of(&handle, tab), SessionModeChoice::Bypass);
        assert_eq!(*asked.borrow(), vec![view(2)]);

        // A press in the base page about page 2's tab is refused, so no prompt is built there.
        clear(&[&base]);
        cycle_mode_from(&handle, ViewId::BASE, "m2", tab);
        assert!(matches!(result_for(&base.sent.borrow(), "m2"), Some(Err(_))));
        assert_eq!(last_bypass_prompt(&base), None);
    }

    #[test]
    fn a_bypass_prompt_is_dropped_when_its_page_loses_the_keys() {
        let (handle, base, second) = handle_with_two_pages();
        let first = handle.state.borrow().tabs.active();
        let tab = handle.open_in(view(2)).unwrap();
        cycle_mode_from(&handle, view(2), "m1", tab);
        let (_, nonce) = last_bypass_prompt(&second).expect("page 2's prompt");
        handle.set_pane_focused(view(2), false);
        confirm_bypass_from(&handle, view(2), "y1", tab, nonce);
        assert!(
            matches!(result_for(&second.sent.borrow(), "y1"), Some(Err(_))),
            "dropped with the keys"
        );
        assert_eq!(mode_of(&handle, tab), SessionModeChoice::Auto);

        // A prompt built in the base page stays when page 2 loses the keys.
        cycle_mode_from(&handle, ViewId::BASE, "m2", first);
        let (prompt_tab, nonce) = last_bypass_prompt(&base).expect("the base page's prompt");
        assert_eq!(prompt_tab, Some(first.0));
        handle.set_pane_focused(view(2), false);
        confirm_bypass_from(&handle, ViewId::BASE, "y2", first, nonce);
        assert_eq!(result_for(&base.sent.borrow(), "y2"), Some(Ok(())));
        assert_eq!(mode_of(&handle, first), SessionModeChoice::Bypass);
    }

    /// The mode key answers no card, so pressing it in a covered chat is not put to the window; the
    /// y/n it raises on the way into bypass is.
    #[test]
    fn cycling_the_mode_is_not_asked_of_the_window() {
        let (handle, base, _second) = handle_with_two_pages();
        let tab = handle.state.borrow().tabs.active();
        let asked = recording_gate(&handle, &[1], Rc::new(Cell::new(true)));
        clear(&[&base]);

        handle.page_message(ViewId::BASE, r#"{"type":"cycle_default_mode","request_id":"d1"}"#);
        assert_eq!(result_for(&base.sent.borrow(), "d1"), Some(Ok(())));
        cycle_mode_from(&handle, ViewId::BASE, "m1", tab);
        assert_eq!(result_for(&base.sent.borrow(), "m1"), Some(Ok(())));
        assert!(
            asked.borrow().is_empty(),
            "a press of the mode key is not asked of the window"
        );

        let (_, nonce) = last_bypass_prompt(&base).expect("the way into bypass asks its y/n");
        confirm_bypass_from(&handle, ViewId::BASE, "y1", tab, nonce);
        assert!(matches!(result_for(&base.sent.borrow(), "y1"), Some(Err(_))));
        assert_eq!(*asked.borrow(), vec![ViewId::BASE], "its `y` is");
    }
}
