use super::rig::*;
use super::*;
use crate::keymap::action::{SplitCwd, TextChange};
use crate::keymap::{KeySpec, SwapTarget, TabAction};
use crate::layout::{Axis, ConId, Direction, Float, FloatRect, Layout, ModuleId, ModuleKind, Node};
use crate::lua::layout::LayoutRequest;
use crate::wm::lua::lua_request_action;
use crate::wm::*;
use std::collections::BTreeSet;

fn editor() -> ModuleId {
    ModuleId::editor()
}
fn agent() -> ModuleId {
    ModuleId::agent()
}
fn terminal() -> ModuleId {
    ModuleId::terminal()
}

/// `editor | (agent / terminal)`, the keys on `keys`.
fn three(keys: ModuleId) -> Layout {
    let root = Node::split(
        Axis::Row,
        0.5,
        Node::Leaf(editor()),
        Node::split(Axis::Column, 0.5, Node::Leaf(agent()), Node::Leaf(terminal())),
    );
    Layout::new(root, keys).unwrap()
}

/// [`three`] with the container around the agent and the terminal selected.
fn three_with_a_container_selected() -> Layout {
    let mut layout = three(agent());
    assert!(
        crate::layout::focus_parent(&mut layout),
        "the premise: a container is selected"
    );
    assert!(matches!(layout.selection(), Selection::Con(_)));
    layout
}

/// `eitri.layout.hide` and `mark` act on one module, so a selected container refuses them as it refuses
/// `module.hide` and `mark.toggle`; `show`, `focus` and `split` end the selection as a module key and a split
/// key do; with the module selected, everything runs.
#[test]
fn a_lua_hide_is_refused_while_a_container_is_selected() {
    let con = Selection::Con(ConId::fresh());
    let gate = |request: LayoutRequest, selection: Selection| {
        lua_request_action(&request).map(|action| selection_gate(&action, selection))
    };
    let t = terminal();
    assert_eq!(gate(LayoutRequest::Hide(t.clone()), con), Some(SelectionGate::Refuse));
    assert_eq!(gate(LayoutRequest::Mark(t.clone()), con), Some(SelectionGate::Refuse));
    for request in [
        LayoutRequest::Show(t.clone()),
        LayoutRequest::Focus(t.clone()),
        LayoutRequest::Split(t.clone(), Axis::Row),
    ] {
        assert_eq!(
            gate(request.clone(), con),
            Some(SelectionGate::Run { reset: true }),
            "{request:?}"
        );
    }
    assert_eq!(
        gate(LayoutRequest::Float(t.clone(), None), con),
        Some(SelectionGate::Refuse),
        "eitri.layout.float is float.toggle's"
    );
    assert_eq!(
        gate(LayoutRequest::Run(Action::Zoom), con),
        None,
        "run meets the rules itself"
    );
    assert_eq!(
        gate(LayoutRequest::Hide(t.clone()), Selection::Module),
        Some(SelectionGate::Run { reset: false })
    );
    assert_eq!(
        gate(LayoutRequest::Mark(t), Selection::Module),
        Some(SelectionGate::Run { reset: false })
    );
    // The keys' own path: a verb that takes the selection keeps it.
    assert_eq!(
        selection_gate(&Action::ContainerTabbed, con),
        SelectionGate::Run { reset: false }
    );
    assert_eq!(selection_gate(&Action::ModuleKill, con), SelectionGate::Refuse);
}

/// With the keys on a float, a verb on the tiled tree and a verb on the float run, and a verb that needs a
/// split around the module is refused; with the keys on a tile, everything runs.
#[test]
fn float_gate_runs_tiles_and_acts_and_refuses_the_rest() {
    for (action, rule) in [
        (Action::Tiled, FloatRule::Tiles),
        (Action::Rotate { down: true }, FloatRule::Tiles),
        (Action::Move(Direction::Left), FloatRule::Acts),
        (Action::Zoom, FloatRule::Acts),
        (Action::FloatToggle, FloatRule::Acts),
        (Action::Spread, FloatRule::Refuses),
        (Action::ContainerTabbed, FloatRule::Refuses),
        (Action::Swap(SwapTarget::Next), FloatRule::Refuses),
        (Action::FocusParent, FloatRule::Refuses),
    ] {
        assert_eq!(action.on_a_float(), rule, "{action:?}");
        let on_a_float = if rule == FloatRule::Refuses {
            FloatGate::Refuse
        } else {
            FloatGate::Run
        };
        assert_eq!(
            float_gate(&action, true),
            on_a_float,
            "{action:?} with the keys on a float"
        );
        assert_eq!(
            float_gate(&action, false),
            FloatGate::Run,
            "{action:?} with the keys on a tile"
        );
    }
}

/// The refusal says how to get back to one module, as the keymap binds `focus.child`.
#[test]
fn the_one_module_refusal_names_the_way_back() {
    let keymap = Keymap::defaults();
    let text = one_module_text(&keymap);
    assert!(text.starts_with("acts on one module: "), "{text}");
    match keymap.keys_for(&Action::FocusChild).first() {
        Some(key) => assert!(text.ends_with(&key.human()), "{text}"),
        None => assert!(text.ends_with("focus.child"), "{text}"),
    }
}

/// A tab verb ends a container selection whichever route sent it: the panel's own `gt` reaches the runner as
/// the prefix's chord does, and this is the rule it meets there.
#[test]
fn a_tab_verb_ends_a_container_selection() {
    let con = Selection::Con(ConId::fresh());
    for tab_action in [TabAction::Next, TabAction::Prev, TabAction::Select(2), TabAction::New] {
        assert_eq!(
            selection_gate(&Action::Tab(tab_action), con),
            SelectionGate::Run { reset: true },
            "{tab_action:?}"
        );
    }
}

/// The float refusal says how to sink the float, as the keymap binds `float.toggle`.
#[test]
fn the_no_split_refusal_names_the_key_that_sinks_the_float() {
    let keymap = Keymap::defaults();
    let text = no_split_text(&keymap);
    assert_eq!(
        text,
        format!(
            "a float has no split: {} sinks it",
            key_text(&keymap, &Action::FloatToggle)
        )
    );
}

/// One value of every action, and whether the runner decides it here: a match with no catch-all, so a new
/// action does not compile until its family is chosen.
fn decided_here(action: &Action) -> bool {
    match action {
        Action::MoveFar(_)
        | Action::ToggleSplit
        | Action::Rotate { .. }
        | Action::Flip(_)
        | Action::Spread
        | Action::Even(_)
        | Action::MainHorizontal
        | Action::MainVertical
        | Action::Tiled
        | Action::CycleLayout { .. }
        | Action::LayoutUndo
        | Action::MarkToggle
        | Action::MarkClear
        | Action::SwapMarked
        | Action::Zoom
        | Action::Resize { .. }
        | Action::Swap(_)
        | Action::Move(_)
        | Action::ContainerTabbed
        | Action::ContainerStacked
        | Action::FocusParent
        | Action::FocusChild
        | Action::Select(_)
        | Action::SelectLast
        | Action::SelectNext
        | Action::SelectNumber
        | Action::Hint
        | Action::SendPrefix
        | Action::SendKeys(_)
        | Action::CopyMode { .. }
        | Action::WindowImmersive
        | Action::WindowSetup
        | Action::Module(_)
        | Action::ModuleHide
        | Action::ModuleHideByKey
        | Action::ModuleKill
        | Action::InstanceNew { .. }
        | Action::Split { .. }
        | Action::WorkspaceStep { .. }
        | Action::WorkspaceLast
        | Action::WorkspaceNew
        | Action::WorkspaceSwitch { .. }
        | Action::WorkspaceChoose
        | Action::WorkspaceRename
        | Action::WindowNew
        | Action::WindowOther
        | Action::MoveToWindow { .. }
        | Action::WorkspaceToWindow { .. }
        | Action::WorkspaceSwapWindow
        | Action::FloatToggle
        | Action::FocusModeToggle
        | Action::ScratchpadMove
        | Action::ScratchpadShow { .. }
        | Action::PanelReload
        | Action::PanelKeymap
        | Action::PanelCommandLine
        | Action::Tab(_)
        | Action::TabBreak
        | Action::Text(_) => true,
    }
}

fn one_of_each() -> Vec<Action> {
    vec![
        Action::Move(Direction::Left),
        Action::ContainerTabbed,
        Action::ContainerStacked,
        Action::FocusParent,
        Action::FocusChild,
        Action::InstanceNew {
            kind: ModuleKind::Terminal,
        },
        Action::FloatToggle,
        Action::FocusModeToggle,
        Action::ScratchpadShow { module: None },
        Action::ScratchpadMove,
        Action::WorkspaceStep { forward: true },
        Action::WorkspaceLast,
        Action::WorkspaceChoose,
        Action::WorkspaceRename,
        Action::WorkspaceSwitch { name: "two".into() },
        Action::WorkspaceNew,
        Action::TabBreak,
        Action::WindowNew,
        Action::WindowOther,
        Action::MoveToWindow {
            module: None,
            follow: false,
        },
        Action::WorkspaceToWindow { follow: false },
        Action::WorkspaceSwapWindow,
        Action::MoveFar(Direction::Right),
        Action::ToggleSplit,
        Action::Rotate { down: false },
        Action::Flip(Axis::Row),
        Action::MainHorizontal,
        Action::MainVertical,
        Action::Tiled,
        Action::CycleLayout { forward: true },
        Action::Spread,
        Action::LayoutUndo,
        Action::MarkToggle,
        Action::MarkClear,
        Action::SwapMarked,
        Action::SelectNumber,
        Action::SendPrefix,
        Action::SendKeys(KeySpec::char('a', true, false)),
        Action::Split {
            axis: Axis::Row,
            cwd: SplitCwd::default(),
        },
        Action::Zoom,
        Action::Resize {
            dir: Direction::Left,
            cells: 5,
        },
        Action::Select(Direction::Left),
        Action::SelectLast,
        Action::SelectNext,
        Action::Swap(SwapTarget::Next),
        Action::Even(Axis::Column),
        Action::ModuleHide,
        Action::ModuleHideByKey,
        Action::ModuleKill,
        Action::Module(agent()),
        Action::Tab(TabAction::Next),
        Action::Hint,
        Action::PanelReload,
        Action::PanelKeymap,
        Action::PanelCommandLine,
        Action::WindowImmersive,
        Action::WindowSetup,
        Action::Text(TextChange::Larger),
        Action::CopyMode { up: false },
    ]
}

#[test]
fn every_action_is_answered_by_the_runner() {
    let actions = one_of_each();
    let kinds: BTreeSet<String> = actions
        .iter()
        .map(|action| format!("{:?}", std::mem::discriminant(action)))
        .collect();
    assert_eq!(kinds.len(), actions.len(), "one value of each action, none twice");
    for action in actions {
        // A tile with the keys and no container selected: no gate refuses anything.
        let mut r = rig(three(terminal()));
        r.run(action.clone());
        assert!(decided_here(&action), "{action:?} is decided by the runner");
        assert!(
            !r.controller.notes(WIN).iter().any(|note| note.contains("BUG")),
            "{action:?} ran: {:?}",
            r.controller.notes(WIN)
        );
        let toast = r.controller.chrome(WIN).toast.map(|toast| toast.text);
        assert!(
            !toast.is_some_and(|text| text.starts_with("a float") || text.starts_with("acts on")),
            "{action:?} met no gate"
        );
    }
}

#[test]
fn a_move_with_the_keys_on_a_float_and_the_editors_kill_are_decided_here() {
    let float = ModuleId::lua("x");
    let layout = Layout::from_parts_floating(
        Node::split(Axis::Row, 0.5, Node::Leaf(editor()), Node::Leaf(agent())),
        vec![Float {
            id: float.clone(),
            rect: FloatRect {
                cx: 0.5,
                cy: 0.5,
                w: 300,
                h: 200,
            },
            scratch: false,
        }],
        Default::default(),
        float,
        Default::default(),
    )
    .unwrap();
    let mut r = rig(layout);
    r.run(Action::Move(Direction::Left));
    assert_eq!(family_of(&Action::Move(Direction::Left), true, false), Family::Floats);
    assert!(
        !r.controller.notes(WIN).iter().any(|note| note.contains("BUG")),
        "a float's move is decided here: {:?}",
        r.controller.notes(WIN)
    );
    let mut editor_rig = rig(three(editor()));
    editor_rig.run(Action::ModuleKill);
    assert!(editor_rig.controller.chrome(WIN).prompt.is_some());
    assert_eq!(family_of(&Action::ModuleKill, false, true), Family::Close);
    assert_eq!(family_of(&Action::ModuleKill, false, false), Family::Modules);
    assert_eq!(family_of(&Action::Move(Direction::Up), false, false), Family::Tiling);
}

#[test]
fn a_selection_reset_clears_it_after_the_verb_and_refreshes() {
    let mut r = rig(three_with_a_container_selected());
    let action = Action::Rotate { down: true };
    assert_eq!(
        selection_gate(&action, r.layout.borrow().selection()),
        SelectionGate::Run { reset: true },
        "the premise: rotate ends a selection"
    );
    let batch = r.run(action);
    assert_eq!(r.layout.borrow().selection(), Selection::Module);
    assert_eq!(r.controller.chrome(WIN).top_bar.selection, None);
    let all: Vec<HostOp> = batch.ops().cloned().collect();
    assert!(matches!(all.last(), Some(HostOp::Chrome { .. })), "{all:#?}");
    assert_eq!(
        all.iter().filter(|op| matches!(op, HostOp::Chrome { .. })).count(),
        1,
        "one announcement reads the whole batch's state"
    );
}

#[test]
fn a_selection_refusal_says_how_to_get_back_and_runs_nothing() {
    let mut r = rig(three_with_a_container_selected());
    let before = r.layout.borrow().clone();
    let said = r.said();
    let batch = r.run(Action::ModuleKill);
    let text = one_module_text(&Keymap::defaults());
    r.assert_refused(&batch, &said, &text);
    assert_eq!(*r.layout.borrow(), before, "a refused action leaves the selection too");
    assert!(
        r.controller
            .notes(WIN)
            .contains(&format!("[modules] {}: {text}", Action::ModuleKill.name())),
        "{:?}",
        r.controller.notes(WIN)
    );
}

#[test]
fn a_text_steps_selection_reset_is_in_its_own_batch() {
    let mut r = rig(three_with_a_container_selected());
    let batch = r.run(Action::Text(TextChange::Larger));
    assert_eq!(r.layout.borrow().selection(), Selection::Module);
    assert!(announced(&batch), "the selection's text went");
    assert_eq!(r.controller.chrome(WIN).top_bar.selection, None);
    // With no container selected, the reset changes nothing.
    let quiet = r.run(Action::Text(TextChange::Larger));
    assert!(!announced(&quiet));
}

#[test]
fn the_config_phases_keymap_names_the_keys_of_its_refusals() {
    let mut r = rig(three_with_a_container_selected());
    let keymap = std::rc::Rc::new(Keymap::defaults());
    let module_keys = std::rc::Rc::new(crate::layout::ModuleKeys::build(&[], &keymap).unwrap());
    r.controller.set_keymap(WIN, keymap.clone(), module_keys);
    let said = r.said();
    let batch = r.run(Action::SwapMarked);
    r.assert_refused(&batch, &said, &one_module_text(&keymap));
}

#[test]
fn a_lua_mark_marks_the_module_it_names_not_the_one_with_the_keys() {
    let mut r = rig(three(editor()));
    let batch = r.controller.lua_request(WIN, LayoutRequest::Mark(terminal()));
    assert_eq!(r.layout.borrow().mark(), Some(&terminal()));
    assert_eq!(r.layout.borrow().focus(), &editor(), "the keys stay where they are");
    assert!(
        ops(&batch).is_empty() && announced(&batch),
        "a mark only redraws its text, as the key's mark does: nothing is arranged again: {batch:?}"
    );
    assert!(r.controller.chrome(WIN).top_bar.mark.is_some());
    r.controller.lua_request(WIN, LayoutRequest::Mark(terminal()));
    assert_eq!(
        r.layout.borrow().mark(),
        Some(&terminal()),
        "marking the marked module again leaves it marked"
    );
}

#[test]
fn a_lua_request_meets_the_selection_gate_only_and_logs_its_refusal() {
    let mut r = rig(three_with_a_container_selected());
    let text = one_module_text(&Keymap::defaults());
    let mark = LayoutRequest::Mark(terminal());
    let batch = r.controller.lua_request(WIN, mark.clone());
    assert!(batch.is_empty(), "a Lua refusal has no toast: {batch:?}");
    assert_eq!(r.layout.borrow().mark(), None);
    assert!(
        r.controller
            .notes(WIN)
            .contains(&format!("[lua] layout: {mark:?} refused: {text}")),
        "{:?}",
        r.controller.notes(WIN)
    );
    // `run` meets the runner's gates exactly as the key does.
    let said = r.said();
    let by_lua = r.controller.lua_request(WIN, LayoutRequest::Run(Action::ModuleKill));
    r.assert_refused(&by_lua, &said, &text);
    let said = r.said();
    let by_key = r.run(Action::ModuleKill);
    r.assert_refused(&by_key, &said, &text);
}

/// A request from `:Eitri` is decided by the same code as the Lua call, so the batch, the layout and the log lines
/// are the Lua call's, but for the log tag.
#[test]
fn an_nvim_request_runs_the_same_batch_as_the_same_lua_request() {
    let requests = [
        LayoutRequest::Run(Action::Zoom),
        LayoutRequest::Split(agent(), Axis::Row),
        LayoutRequest::Focus(terminal()),
        LayoutRequest::Show(terminal()),
        LayoutRequest::Hide(terminal()),
        LayoutRequest::Open(ModuleKind::Terminal, None),
        LayoutRequest::Switch("w2".into()),
    ];
    for request in requests {
        let mut by_lua = rig(three(editor()));
        let mut by_nvim = rig(three(editor()));
        let from_lua = by_lua.controller.lua_request(WIN, request.clone());
        let from_nvim = by_nvim.controller.nvim_request(WIN, request.clone());
        assert_eq!(from_nvim, from_lua, "{request:?}");
        assert_eq!(*by_nvim.layout.borrow(), *by_lua.layout.borrow(), "{request:?}");
        let retagged: Vec<String> = by_nvim
            .controller
            .notes(WIN)
            .iter()
            .map(|line| line.replacen("[nvim]", "[lua]", 1))
            .collect();
        assert_eq!(retagged, by_lua.controller.notes(WIN), "{request:?}");
        assert!(
            by_nvim
                .controller
                .notes(WIN)
                .iter()
                .any(|line| line.starts_with("[nvim] layout: ")),
            "{request:?}: {:?}",
            by_nvim.controller.notes(WIN)
        );
        assert!(
            !by_nvim.controller.notes(WIN).iter().any(|line| line.contains("[lua]")),
            "{request:?}: {:?}",
            by_nvim.controller.notes(WIN)
        );
    }
}

/// The selection gates meet an `:Eitri` request as they meet the Lua call, and refuse with the same words, which
/// the window also shows: nobody at nvim's command line reads the log.
#[test]
fn a_refused_nvim_request_is_refused_with_the_words_of_the_lua_call() {
    let text = one_module_text(&Keymap::defaults());
    let mut r = rig(three_with_a_container_selected());
    let hide = LayoutRequest::Hide(terminal());
    let said = r.said();
    let batch = r.controller.nvim_request(WIN, hide.clone());
    r.assert_refused(&batch, &said, &text);
    assert!(
        r.controller
            .notes(WIN)
            .contains(&format!("[nvim] layout: {hide:?} refused: {text}")),
        "{:?}",
        r.controller.notes(WIN)
    );
    // `run` meets the runner's gates, with the toast the key's refusal has.
    let said = r.said();
    let by_nvim = r.controller.nvim_request(WIN, LayoutRequest::Run(Action::ModuleKill));
    r.assert_refused(&by_nvim, &said, &text);
}

/// The same refusal from Lua stays a log line: a script's caller reads the log, and a config must not toast at start.
#[test]
fn a_refused_lua_request_is_logged_and_shows_nothing() {
    let mut r = rig(three_with_a_container_selected());
    let said = r.said();
    let batch = r.controller.lua_request(WIN, LayoutRequest::Hide(terminal()));
    assert!(batch.is_empty(), "{batch:?}");
    assert_eq!(r.said().toast, said.toast);
}

/// A request whose own operation fails, as a module no layout holds, is toasted with the error's words.
#[test]
fn an_nvim_request_the_layout_refuses_is_toasted() {
    let module = ModuleId::parse("terminal#5").unwrap();
    let request = LayoutRequest::Focus(module);
    let mut by_lua = rig(three(editor()));
    let _ = by_lua.controller.lua_request(WIN, request.clone());
    let logged = by_lua.controller.notes(WIN).last().cloned().unwrap();
    let (_, why) = logged.rsplit_once(" refused: ").expect("the Lua call logs a refusal");

    let mut r = rig(three(editor()));
    let said = r.said();
    let batch = r.controller.nvim_request(WIN, request);
    r.assert_refused(&batch, &said, why);
}

/// A line the parser refused never became a request: the log line and the toast are all it does.
#[test]
fn an_nvim_line_the_parser_refused_is_logged_and_toasted() {
    let mut r = rig(three(editor()));
    let said = r.said();
    let batch = r.controller.nvim_refused(WIN, ":Eitri \"zoomm\" is not an action");
    r.assert_refused(&batch, &said, ":Eitri \"zoomm\" is not an action");
    assert!(r
        .controller
        .notes(WIN)
        .contains(&"[nvim] :Eitri \"zoomm\" is not an action".to_string()));
}

/// The controller decides every Lua layout request, and the shell posts every one of them, floats included,
/// to the controller in one event per drain: the shell keeps no route of its own for any of them.
#[test]
fn every_lua_request_is_decided_and_sent_to_the_controller() {
    let mut r = rig(three(editor()));
    assert!(crate::wm::lua::decided(&LayoutRequest::Run(Action::Zoom)));
    assert!(crate::wm::lua::decided(&LayoutRequest::Mark(terminal())));
    // The module requests, a switch and a float are decided here; the shell posts one event per drain.
    for request in [
        LayoutRequest::Show(terminal()),
        LayoutRequest::Hide(terminal()),
        LayoutRequest::Focus(terminal()),
        LayoutRequest::Split(terminal(), Axis::Row),
        LayoutRequest::Open(ModuleKind::Terminal, None),
        LayoutRequest::Switch("two".into()),
        LayoutRequest::Float(terminal(), None),
    ] {
        assert!(crate::wm::lua::decided(&request), "{request:?}");
    }
    r.controller.lua_request(WIN, LayoutRequest::Switch("two".into()));
    assert_eq!(r.controller.chrome(WIN).top_bar.workspace.as_deref(), Some("two"));
    assert!(r.controller.notes(WIN).contains(&"[workspace] made two".into()));
}
