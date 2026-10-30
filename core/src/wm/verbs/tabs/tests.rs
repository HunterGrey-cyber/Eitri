use super::*;
use crate::keymap::Action;
use crate::layout::{Axis, Layout, ModuleId, Node};
use crate::wm::verbs::rig::*;
use crate::wm::*;

fn two() -> Layout {
    Layout::new(
        Node::split(
            Axis::Row,
            0.5,
            Node::Leaf(ModuleId::editor()),
            Node::Leaf(ModuleId::agent()),
        ),
        ModuleId::editor(),
    )
    .unwrap()
}
/// `layout` with the base chat hidden, as a hide leaves it.
fn base_hidden(mut layout: Layout) -> Layout {
    let host = crate::wm::fake::FakeHost::new();
    frame_with(&*host, WIN, host.grid_size(WIN), |frame| {
        crate::layout::hide(&mut layout, &ModuleId::agent(), frame)
    })
    .unwrap();
    layout
}
fn three() -> Layout {
    Layout::new(
        Node::split(
            Axis::Row,
            0.5,
            Node::Leaf(ModuleId::editor()),
            Node::split(
                Axis::Column,
                0.5,
                Node::Leaf(ModuleId::terminal()),
                Node::Leaf(ModuleId::agent()),
            ),
        ),
        ModuleId::editor(),
    )
    .unwrap()
}
fn tab_calls(batch: &Batch) -> Vec<HostOp> {
    batch
        .ops()
        .filter(|op| matches!(op, HostOp::Chat(ChatOp::Tab { .. })))
        .cloned()
        .collect()
}
fn call(call: TabCall) -> HostOp {
    HostOp::Chat(ChatOp::Tab { win: WIN, call })
}
fn panel(r: &mut Rig, view: ViewId, action: TabAction) -> Batch {
    r.controller.handle(HostEvent::Chat(ChatEvent::TabVerb {
        win: WIN,
        view,
        action: Action::Tab(action),
    }))
}
#[test]
fn the_companion_and_the_main_window_map_a_tab_verb_to_the_same_panel_call() {
    for (verb, call) in [
        (TabVerb::New, Some(TabCall::New)),
        (TabVerb::Step(-1), Some(TabCall::Step(-1))),
        (TabVerb::Last, Some(TabCall::Last)),
        (TabVerb::Select(9), Some(TabCall::Select(9))),
        (TabVerb::Rename, Some(TabCall::Rename(ViewId::BASE))),
        (TabVerb::Close, Some(TabCall::Close(ViewId::BASE))),
        (TabVerb::CloseOthers, Some(TabCall::CloseOthers)),
        (TabVerb::Choose, Some(TabCall::Choose(ViewId::BASE))),
        (TabVerb::Info, Some(TabCall::Info(ViewId::BASE))),
        (TabVerb::Flash, None),
        (TabVerb::Nothing, None),
        (TabVerb::Sibling(1), None),
        (TabVerb::Terminal(-1), None),
    ] {
        assert_eq!(base_call(&verb), call);
    }
}
#[test]
fn a_tab_number_with_no_tab_flashes_and_moves_nothing() {
    let mut r = rig(two());
    let before = r.said();
    let batch = panel(&mut r, ViewId::BASE, TabAction::Select(9));
    r.assert_refused(&batch, &before, "");
}
#[test]
fn an_extra_views_tab_verb_acts_on_its_own_page() {
    let view = ViewId::new(2).unwrap();
    assert_eq!(view_call(&TabVerb::Rename, view), Some(TabCall::Rename(view)));
}
#[test]
fn a_panel_tab_verb_reads_the_view_that_sent_it() {
    let mut r = rig(two());
    let view = ViewId::new(2).unwrap();
    let batch = panel(&mut r, view, TabAction::Rename);
    assert_eq!(
        ops(&batch),
        vec![HostOp::Chat(ChatOp::Tab {
            win: WIN,
            call: TabCall::Rename(view)
        })]
    );
    assert!(r.layout.borrow().focus() == &ModuleId::editor());
}

#[test]
fn an_extra_views_own_verbs_leave_a_hidden_base_view_hidden() {
    let view = ViewId::new(2).unwrap();
    for (action, made) in [
        (TabAction::Rename, TabCall::Rename(view)),
        (TabAction::Close, TabCall::Close(view)),
        (TabAction::Info, TabCall::Info(view)),
        (TabAction::Choose, TabCall::Choose(view)),
    ] {
        let mut r = rig(base_hidden(two()));
        let before = r.said();
        let batch = panel(&mut r, view, action);
        assert_eq!(ops(&batch), vec![call(made)], "{action:?} from {view:?} moves nothing");
        assert!(
            !r.layout.borrow().is_visible(&ModuleId::agent()),
            "{action:?} left the base chat hidden"
        );
        assert_eq!(r.said(), before, "{action:?} made a call, so nothing flashes");
    }
}

#[test]
fn an_extra_views_step_over_the_base_tabs_reveals_the_hidden_base_view() {
    let mut r = rig(base_hidden(two()));
    r.host.set_tab_can_step(1, true);
    let batch = panel(&mut r, ViewId::new(2).unwrap(), TabAction::Next);
    assert_eq!(moves(&batch), vec![map(&ModuleId::agent()), allocate()]);
    assert_eq!(tab_calls(&batch), vec![call(TabCall::Step(1))]);
}

#[test]
fn a_sibling_step_in_a_tabbed_terminal_gives_the_keys_to_its_neighbour_or_flashes() {
    let from = ModuleId::terminal();
    let to = ModuleId::editor();
    let root = Node::Con(crate::layout::Con::new(
        crate::layout::ConLayout::Tabbed,
        vec![
            crate::layout::Child::new(Node::Leaf(from.clone()), 0.5),
            crate::layout::Child::new(Node::Leaf(to.clone()), 0.5),
        ],
    ));
    let mut r = rig(Layout::new(root, from.clone()).unwrap());
    let before = r.said();
    let batch = r.run_tab(TabAction::Next, Some(from.clone()));
    let focus: Vec<_> = moves(&batch)
        .into_iter()
        .filter_map(|op| match op {
            HostOp::Focus {
                to: FocusTo::Module(id),
                ..
            } => Some(id),
            _ => None,
        })
        .collect();
    assert_eq!(focus, vec![to], "the keys go to the terminal's sibling tab");
    assert!(tab_calls(&batch).is_empty(), "a sibling step makes no panel call");
    assert_eq!(r.said(), before, "a step that landed does not flash");
    // The plan picks a sibling or terminal step only when the layout has one to go to, so a step finding none is
    // the layout read again coming up empty: it flashes the app name alone, as a tab step with no tab does.
    for verb in [TabVerb::Sibling(1), TabVerb::Terminal(-1)] {
        assert_eq!(
            crate::wm::feedback::tabs(&verb, crate::wm::feedback::VerbOutcome::Nothing),
            crate::wm::feedback::tabs(&TabVerb::Step(1), crate::wm::feedback::VerbOutcome::Nothing),
            "{verb:?} with nowhere to go flashes as a tab step with no tab does"
        );
    }
}

#[test]
fn a_base_tab_chosen_from_elsewhere_reveals_the_base_view_where_it_was() {
    let mut r = rig(base_hidden(three()));
    r.host.set_tab_can_step(1, true);
    let batch = r.run_tab(TabAction::Next, Some(ModuleId::editor()));
    assert_eq!(tab_calls(&batch), vec![call(TabCall::Step(1))]);
    assert_eq!(moves(&batch), vec![map(&ModuleId::agent()), allocate()]);
    assert_eq!(
        r.layout.borrow().focus(),
        &ModuleId::editor(),
        "the keys stay in the editor"
    );

    let mut r = rig(base_hidden(three()));
    r.host.set_tab_place(1, Some(TabPlace::Base(crate::tabs::TabId(1))));
    let batch = r.run_tab(TabAction::Select(1), Some(ModuleId::terminal()));
    assert_eq!(tab_calls(&batch), vec![call(TabCall::Select(1))]);
    assert_eq!(moves(&batch), vec![map(&ModuleId::agent()), allocate()]);
    assert_eq!(
        r.layout.borrow().focus(),
        &ModuleId::editor(),
        "a number never moves the keys"
    );
}
