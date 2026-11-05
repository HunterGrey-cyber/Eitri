use super::*;
use crate::keymap::{Action, TabAction};
use crate::layout::{Axis, Float, FloatRect, Frame, Layout, Node, Selection, Size};
use crate::plugin::keys::PluginKeysHelp;
use crate::wm::fake::{FakeHost, FakeScheduler, Logged};
use crate::wm::verbs::rig::decl;
use crate::wm::verbs::tabs::TabCall;
use crate::wm::*;
use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

const WIN: WinId = WinId::MAIN;
const GRID: Size = Size { w: 1200, h: 800 };

fn editor() -> ModuleId {
    ModuleId::editor()
}
fn agent() -> ModuleId {
    ModuleId::agent()
}
fn terminal() -> ModuleId {
    ModuleId::terminal()
}
fn two() -> ViewId {
    ViewId::new(2).unwrap()
}
fn agent2() -> ModuleId {
    two().module()
}
fn leaf(id: ModuleId) -> Node {
    Node::Leaf(id)
}

/// `editor | (agent / terminal)`, the keys on `keys`.
fn three(keys: ModuleId) -> Layout {
    let root = Node::split(
        Axis::Row,
        0.5,
        leaf(editor()),
        Node::split(Axis::Column, 0.5, leaf(agent()), leaf(terminal())),
    );
    Layout::new(root, keys).unwrap()
}

/// `editor | (agent / agent2)`: the base view and an extra one, the keys on `keys`.
fn with_extra_view(keys: ModuleId) -> Layout {
    let root = Node::split(
        Axis::Row,
        0.5,
        leaf(editor()),
        Node::split(Axis::Column, 0.5, leaf(agent()), leaf(agent2())),
    );
    Layout::new(root, keys).unwrap()
}

fn hide(layout: &mut Layout, id: &ModuleId) {
    crate::layout::hide(layout, id, &Frame::new(GRID, 1)).unwrap();
}

/// A controller holding `layout` as the main window behind a driver, over the fake host with a surface for every
/// module, every module the layout shows mapped and the keys given by the first apply.
struct Rig {
    driver: Rc<Driver>,
    host: Rc<FakeHost>,
    scheduler: Rc<FakeScheduler>,
    layout: Rc<RefCell<Layout>>,
}

fn driven(layout: Layout) -> Rig {
    driven_with(layout, EditorPresence::Owned)
}

/// [`driven`] for a window whose editor is `presence`.
fn driven_with(layout: Layout, presence: EditorPresence) -> Rig {
    let host = FakeHost::new();
    host.set_grid_size(GRID);
    let scheduler = FakeScheduler::new();
    let mut controller = Controller::new(scheduler.clone());
    controller.set_reads(host.clone());
    let hosted: Vec<_> = layout.modules().iter().map(decl).collect();
    for d in &hosted {
        host.with_surface(WIN, d.module.clone(), d.kind.clone(), false);
    }
    let layout = Rc::new(RefCell::new(layout));
    controller.open_window(
        WIN,
        WindowSpec {
            role: WindowRole::Main,
            presence,
            title: "project".into(),
        },
        WindowParts {
            layout: layout.clone(),
            workspaces: Rc::new(RefCell::new(crate::layout::Workspaces::single())),
            saver: None,
        },
    );
    let driver = Driver::new(controller, host.clone(), scheduler.clone());
    let sink: Rc<dyn HostSink> = driver.clone();
    host.attach(Rc::downgrade(&sink));
    scheduler.attach(Rc::downgrade(&sink));
    driver.first_apply(WIN, hosted);
    // The window's first focus: the layout's module takes the keys, as the toolkit reports once the window shows.
    let keys = layout.borrow().focus().clone();
    host.apply(&HostOp::Focus {
        win: WIN,
        to: FocusTo::Module(keys),
        on_grab: vec![],
    });
    Rig {
        driver,
        host,
        scheduler,
        layout,
    }
}

impl Rig {
    fn mark(&self) -> usize {
        self.host.log().len()
    }

    fn logged_since(&self, mark: usize) -> Vec<Logged> {
        self.host.log()[mark..].to_vec()
    }

    /// The ops applied since `mark`, but the chrome's announcements, with how each went.
    fn applied_since(&self, mark: usize) -> Vec<(HostOp, Applied)> {
        self.logged_since(mark)
            .into_iter()
            .filter_map(|logged| match logged {
                Logged::Op(HostOp::Chrome { .. }, _) => None,
                Logged::Op(op, applied) => Some((op, applied)),
                _ => None,
            })
            .collect()
    }

    fn ops_since(&self, mark: usize) -> Vec<HostOp> {
        self.applied_since(mark).into_iter().map(|(op, _)| op).collect()
    }

    fn post(&self, event: HostEvent) {
        self.host.post(event);
    }

    fn run(&self, action: Action) {
        self.driver.run(WIN, action, Origin::Key { keycode: 7 });
    }

    fn chat(&self, event: ChatEvent) {
        self.post(HostEvent::Chat(event));
    }

    fn keys(&self) -> Option<ModuleId> {
        self.host.focus_owner(WIN)
    }

    fn toast(&self) -> Option<String> {
        self.driver.query(|c| c.chrome(WIN).toast.map(|toast| toast.text))
    }

    fn flashes(&self) -> u64 {
        self.driver.query(|c| c.chrome(WIN).top_bar.flash)
    }

    fn notes(&self) -> Vec<String> {
        self.driver.query(|c| c.notes(WIN))
    }

    /// Runs every idle armed now.
    fn idle(&self) {
        self.scheduler.advance(Duration::ZERO);
    }
}

fn focus_on(module: &ModuleId) -> impl Fn(&HostOp) -> bool + '_ {
    move |op| matches!(op, HostOp::Focus { to: FocusTo::Module(to), .. } if to == module)
}

fn position(ops: &[HostOp], what: impl Fn(&HostOp) -> bool) -> usize {
    ops.iter()
        .position(what)
        .unwrap_or_else(|| panic!("not in the batch: {ops:#?}"))
}

fn opening_of(ops: &[HostOp]) -> u64 {
    ops.iter()
        .find_map(|op| match op {
            HostOp::Chat(ChatOp::OpenKeymap { opening, .. }) => Some(*opening),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no sheet opened: {ops:#?}"))
}

fn help() -> PluginKeysHelp {
    PluginKeysHelp {
        title: "Page".into(),
        declared: true,
        rows: vec![],
    }
}

#[test]
fn the_keys_sheet_reads_plugin_rows_before_the_chat_takes_the_keys() {
    let page = ModuleId::lua("page");
    let root = Node::split(
        Axis::Row,
        0.5,
        leaf(editor()),
        Node::split(Axis::Column, 0.5, leaf(agent()), leaf(page.clone())),
    );
    let r = driven(Layout::new(root, page.clone()).unwrap());
    assert_eq!(r.keys(), Some(page.clone()), "the premise: the page holds the keys");
    r.host.set_plugin_keys_help(page.clone(), Some(help()));
    let mark = r.mark();
    r.run(Action::PanelKeymap);
    let logged = r.logged_since(mark);
    let read = logged
        .iter()
        .position(|l| *l == Logged::Read(format!("plugin_keys_help({page})")))
        .unwrap_or_else(|| panic!("the page's rows were not read: {logged:#?}"));
    let first_op = logged
        .iter()
        .position(|l| matches!(l, Logged::Op(..)))
        .expect("the sheet's batch applied");
    assert!(read < first_op, "read before anything moved: {logged:#?}");
    let ops = r.ops_since(mark);
    let open = position(&ops, |op| matches!(op, HostOp::Chat(ChatOp::OpenKeymap { .. })));
    assert!(
        position(&ops, focus_on(&agent())) < open,
        "the chat takes the keys first: {ops:#?}"
    );
    assert_eq!(
        ops[open],
        HostOp::Chat(ChatOp::OpenKeymap {
            win: WIN,
            view: ViewId::BASE,
            plugin: Some(help()),
            opening: opening_of(&ops),
        })
    );
    assert_eq!(r.keys(), Some(agent()));
}

#[test]
fn a_forced_close_of_the_sheet_moves_nothing() {
    let mut layout = three(terminal());
    layout.toggle_zoom(&terminal());
    assert!(layout.zoom_hides(&agent()), "the premise: the zoom hides the chat");
    let r = driven(layout);
    let mark = r.mark();
    r.run(Action::PanelKeymap);
    let opening = opening_of(&r.ops_since(mark));
    assert!(r.layout.borrow().zoom().is_none(), "showing the chat ended the zoom");
    assert_eq!(r.keys(), Some(agent()));
    let mark = r.mark();
    r.chat(ChatEvent::KeymapClosed {
        win: WIN,
        view: ViewId::BASE,
        opening,
        forced: true,
    });
    assert_eq!(r.ops_since(mark), vec![], "a forced close moves nothing");
    assert!(r.layout.borrow().zoom().is_none(), "nor puts the zoom back");
    assert_eq!(r.keys(), Some(agent()));
}

#[test]
fn a_keys_sheet_closed_by_its_own_keys_goes_back_where_the_keys_were() {
    let mut layout = three(terminal());
    layout.toggle_zoom(&terminal());
    let zoom = layout.zoom().cloned();
    let r = driven(layout);
    let mark = r.mark();
    r.run(Action::PanelKeymap);
    let opening = opening_of(&r.ops_since(mark));
    r.chat(ChatEvent::KeymapClosed {
        win: WIN,
        view: ViewId::BASE,
        opening,
        forced: false,
    });
    assert_eq!(r.keys(), Some(terminal()));
    assert_eq!(
        r.layout.borrow().zoom().cloned(),
        zoom,
        "the zoom the sheet ended is back"
    );
}

#[test]
fn a_tab_verb_resets_the_selection_whichever_route_sent_it() {
    let r = driven(three(agent()));
    r.host.set_tab_can_step(1, true);
    let step = HostOp::Chat(ChatOp::Tab {
        win: WIN,
        call: TabCall::Step(1),
    });
    for route in ["key", "panel"] {
        assert!(crate::layout::focus_parent(&mut r.layout.borrow_mut()));
        assert!(matches!(r.layout.borrow().selection(), Selection::Con(_)), "{route}");
        let mark = r.mark();
        match route {
            "key" => r.run(Action::Tab(TabAction::Next)),
            _ => r.chat(ChatEvent::TabVerb {
                win: WIN,
                view: ViewId::BASE,
                action: Action::Tab(TabAction::Next),
            }),
        }
        assert_eq!(r.layout.borrow().selection(), Selection::Module, "{route}");
        assert_eq!(
            r.ops_since(mark).last(),
            Some(&step),
            "{route}: the step is the batch's last op: {:#?}",
            r.ops_since(mark)
        );
        assert_eq!(r.driver.query(|c| c.chrome(WIN).top_bar.selection), None, "{route}");
    }
}

/// `editor | agent` with the chat hidden, the editor zoomed and the keys there, under a float as large as the grid.
fn covered_by_a_float() -> Layout {
    let float = ModuleId::lua("cover");
    let mut layout = Layout::from_parts_floating(
        Node::split(Axis::Row, 0.5, leaf(editor()), leaf(agent())),
        vec![Float {
            id: float,
            rect: FloatRect {
                cx: 0.5,
                cy: 0.5,
                w: 1200,
                h: 800,
            },
            scratch: false,
        }],
        [agent()].into_iter().collect(),
        editor(),
        Default::default(),
    )
    .unwrap();
    layout.toggle_zoom(&editor());
    layout
}

#[test]
fn a_covered_chat_refuses_the_sheet_and_puts_back_what_showing_it_changed() {
    let layout = covered_by_a_float();
    let zoom = layout.zoom().cloned();
    assert!(zoom.is_some(), "the premise: the editor is zoomed");
    let r = driven(layout);
    let keys = r.keys();
    let flashes = r.flashes();
    let mark = r.mark();
    r.run(Action::PanelKeymap);
    let ops = r.ops_since(mark);
    assert!(
        !ops.iter()
            .any(|op| matches!(op, HostOp::Chat(ChatOp::OpenKeymap { .. }))),
        "nothing opens: {ops:#?}"
    );
    assert!(
        !ops.iter().any(focus_on(&agent())),
        "the chat never takes the keys: {ops:#?}"
    );
    assert!(!r.layout.borrow().is_shown(&agent()), "the chat is hidden again");
    assert_eq!(r.layout.borrow().zoom().cloned(), zoom, "the zoom it ended is back");
    assert_eq!(r.keys(), keys);
    assert_eq!(r.flashes(), flashes + 1);
    let toast = r.toast().expect("the refusal says why");
    assert!(toast.contains(" is under "), "{toast}");
    assert!(
        r.notes().contains(&format!("[modules] refused: {toast}")),
        "{:#?}",
        r.notes()
    );
}

#[test]
fn the_command_line_supersedes_a_sheet_close_on_its_way() {
    let r = driven(three(terminal()));
    let mark = r.mark();
    r.run(Action::PanelKeymap);
    let opening = opening_of(&r.ops_since(mark));
    let mark = r.mark();
    r.run(Action::PanelCommandLine);
    let ops = r.ops_since(mark);
    assert!(
        ops.contains(&HostOp::Chat(ChatOp::CommandLine {
            win: WIN,
            view: ViewId::BASE
        })),
        "{ops:#?}"
    );
    let mark = r.mark();
    r.chat(ChatEvent::KeymapClosed {
        win: WIN,
        view: ViewId::BASE,
        opening,
        forced: false,
    });
    assert_eq!(r.ops_since(mark), vec![], "the older close moves nothing");
    assert_eq!(r.keys(), Some(agent()));
}

#[test]
fn a_view_shown_by_the_panel_arrives_only_if_the_grab_took() {
    let arrive = HostOp::Chat(ChatOp::Arrive {
        win: WIN,
        view: two(),
        card_first: false,
        tab: None,
        intent: crate::agents::Intent::Show,
    });
    let r = driven(with_extra_view(editor()));
    let refused = agent2();
    r.host.refuse_op(move |op| focus_on(&refused)(op));
    let mark = r.mark();
    r.chat(ChatEvent::ShowView { win: WIN, view: two() });
    let applied = r.applied_since(mark);
    assert!(
        applied.contains(&(
            HostOp::Focus {
                win: WIN,
                to: FocusTo::Module(agent2()),
                on_grab: vec![arrive.clone()],
            },
            Applied::Refused
        )),
        "{applied:#?}"
    );
    assert!(!r.ops_since(mark).contains(&arrive), "a refused grab arrives nowhere");

    let r = driven(with_extra_view(editor()));
    let mark = r.mark();
    r.chat(ChatEvent::ShowView { win: WIN, view: two() });
    assert!(r.ops_since(mark).contains(&arrive), "{:#?}", r.ops_since(mark));
    assert_eq!(r.keys(), Some(agent2()));
}

#[test]
fn an_empty_view_is_dropped_from_an_idle_and_the_keys_go_to_the_base_view_first() {
    let r = driven(with_extra_view(agent2()));
    assert_eq!(r.keys(), Some(agent2()), "the premise: the extra view holds the keys");
    r.host.set_chat_views(vec![ViewId::BASE]);
    let mark = r.mark();
    r.chat(ChatEvent::ViewEmpty { win: WIN, view: two() });
    assert_eq!(r.ops_since(mark), vec![], "nothing moves inside the panel's own call");
    r.idle();
    let ops = r.ops_since(mark);
    let keys = position(&ops, focus_on(&agent()));
    let unmap = position(&ops, |op| {
        *op == HostOp::Unmap {
            win: WIN,
            module: agent2(),
        }
    });
    assert!(keys < unmap, "the keys leave before the view does: {ops:#?}");
    assert!(ops.contains(&HostOp::DropSurface {
        win: WIN,
        module: agent2(),
        why: DropWhy::Retired,
    }));
    assert!(ops.contains(&HostOp::Chat(ChatOp::LayoutViews {
        win: WIN,
        views: vec![]
    })));
    assert!(!r.layout.borrow().contains(&agent2()));
    assert_eq!(r.keys(), Some(agent()));
    assert!(
        r.notes()
            .contains(&format!("[chat] {} holds no tab; removed", agent2())),
        "{:#?}",
        r.notes()
    );

    // A view given a tab again before the idle stays, and the base view never goes.
    let r = driven(with_extra_view(editor()));
    r.host.set_chat_views(vec![ViewId::BASE, two()]);
    let mark = r.mark();
    r.chat(ChatEvent::ViewEmpty { win: WIN, view: two() });
    r.chat(ChatEvent::ViewEmpty {
        win: WIN,
        view: ViewId::BASE,
    });
    r.idle();
    assert_eq!(r.ops_since(mark), vec![]);
    assert!(r.layout.borrow().contains(&agent2()));
}

#[test]
fn an_empty_zoomed_view_puts_the_base_view_on_screen_before_handing_it_the_keys() {
    let mut layout = with_extra_view(agent2());
    layout.toggle_zoom(&agent2());
    assert!(
        layout.zoom_hides(&agent()),
        "the premise: the zoom keeps the base view off screen"
    );
    let r = driven(layout);
    assert_eq!(r.keys(), Some(agent2()), "the premise: the zoomed view holds the keys");
    r.host.set_chat_views(vec![ViewId::BASE]);
    let mark = r.mark();
    r.chat(ChatEvent::ViewEmpty { win: WIN, view: two() });
    r.idle();
    let applied = r.applied_since(mark);
    let ops: Vec<HostOp> = applied.iter().map(|(op, _)| op.clone()).collect();
    let mapped = position(&ops, |op| {
        *op == HostOp::Map {
            win: WIN,
            module: agent(),
        }
    });
    let keys = position(&ops, focus_on(&agent()));
    let unmap = position(&ops, |op| {
        *op == HostOp::Unmap {
            win: WIN,
            module: agent2(),
        }
    });
    assert!(
        mapped < keys && keys < unmap,
        "the base view is mapped, then takes the keys, then the view leaves: {ops:#?}"
    );
    assert!(
        applied
            .iter()
            .filter(|(op, _)| focus_on(&agent())(op))
            .all(|(_, applied)| *applied == Applied::Done),
        "no grab of the base view is refused: {applied:#?}"
    );
    assert!(r.layout.borrow().zoom().is_none());
    assert!(!r.layout.borrow().contains(&agent2()));
    assert_eq!(
        r.keys(),
        Some(agent()),
        "a module holds the keys after the view has gone"
    );
}

#[test]
fn an_empty_view_alone_on_screen_gets_the_base_view_beside_it_first() {
    let mut layout = with_extra_view(agent2());
    hide(&mut layout, &editor());
    hide(&mut layout, &agent());
    let r = driven(layout);
    let mark = r.mark();
    r.chat(ChatEvent::ViewEmpty { win: WIN, view: two() });
    r.idle();
    let ops = r.ops_since(mark);
    let shown = position(&ops, |op| {
        *op == HostOp::Map {
            win: WIN,
            module: agent(),
        }
    });
    let unmap = position(&ops, |op| {
        *op == HostOp::Unmap {
            win: WIN,
            module: agent2(),
        }
    });
    assert!(shown < unmap, "{ops:#?}");
    assert!(!r.layout.borrow().contains(&agent2()));
    assert!(r.layout.borrow().is_shown(&agent()));
}

#[test]
fn a_break_opens_a_view_and_a_join_refuses_a_view_with_no_tab() {
    let r = driven(three(agent()));
    r.host.set_can_break_out(Ok(()));
    let mark = r.mark();
    r.run(Action::TabBreak);
    let ops = r.ops_since(mark);
    assert!(
        ops.iter().any(|op| matches!(
            op,
            HostOp::CreateSurface { module, kind: SurfaceKind::Chat { .. }, .. } if *module == agent2()
        )),
        "{ops:#?}"
    );

    let r = driven(with_extra_view(agent2()));
    r.host.set_chat_views(vec![ViewId::BASE]);
    let flashes = r.flashes();
    let mark = r.mark();
    r.run(Action::TabBreak);
    assert_eq!(r.ops_since(mark), vec![], "nothing moves for a view with no tab");
    assert_eq!(r.toast().as_deref(), Some("agent 2 shows no tab"));
    assert_eq!(r.flashes(), flashes + 1);
    assert_eq!(r.keys(), Some(agent2()));

    r.host.set_chat_views(vec![ViewId::BASE, two()]);
    let mark = r.mark();
    r.run(Action::TabBreak);
    let ops = r.ops_since(mark);
    let join = HostOp::Chat(ChatOp::JoinBack { win: WIN, view: two() });
    assert!(
        position(&ops, focus_on(&agent())) < position(&ops, |op| *op == join),
        "{ops:#?}"
    );
    assert_eq!(r.keys(), Some(agent()));
}

#[test]
fn a_join_the_panel_refuses_says_the_panels_words() {
    let r = driven(with_extra_view(agent2()));
    r.host.set_chat_views(vec![ViewId::BASE, two()]);
    r.host
        .refuse_op(|op| matches!(op, HostOp::Chat(ChatOp::JoinBack { .. })));
    let flashes = r.flashes();
    r.run(Action::TabBreak);
    assert_eq!(
        r.toast().as_deref(),
        Some("the native model cannot apply this operation")
    );
    assert_eq!(r.flashes(), flashes + 1);
}

#[test]
fn a_break_that_cannot_happen_makes_no_view_and_says_why() {
    let r = driven(three(agent()));
    let why = "the base view has one tab left".to_string();
    r.host.set_can_break_out(Err(why.clone()));
    let flashes = r.flashes();
    let mark = r.mark();
    r.run(Action::TabBreak);
    assert_eq!(r.ops_since(mark), vec![], "the refusal's chrome alone");
    assert!(
        r.logged_since(mark)
            .iter()
            .any(|l| matches!(l, Logged::Op(HostOp::Chrome { .. }, _))),
        "the chrome is announced"
    );
    assert_eq!(r.toast(), Some(why));
    assert_eq!(r.flashes(), flashes + 1);
}

#[test]
fn a_break_with_the_keys_in_no_chat_view_is_refused() {
    let r = driven(three(terminal()));
    let mark = r.mark();
    r.run(Action::TabBreak);
    assert_eq!(r.ops_since(mark), vec![]);
    assert_eq!(r.toast().as_deref(), Some("tab.break: the keys are in no chat view"));
    assert!(r
        .notes()
        .contains(&"[prefix] tab.break: the keys are in no chat view".to_string()));
}

/// `prefix !` breaks from the base view, joins back from an extra one, and does nothing with the keys in no chat
/// view, as tmux's `break-pane` acts on the pane with the keys.
#[test]
fn break_plan_breaks_from_the_base_view_joins_from_an_extra_one_and_refuses_elsewhere() {
    let two = ViewId::new(2).unwrap();
    assert_eq!(break_plan(Some(ViewId::BASE)), BreakPlan::Break);
    assert_eq!(break_plan(Some(two)), BreakPlan::Join(two));
    assert_eq!(break_plan(ViewId::of_module(&ModuleId::editor())), BreakPlan::Refuse);
    assert_eq!(break_plan(ViewId::of_module(&ModuleId::terminal())), BreakPlan::Refuse);
    assert_eq!(break_plan(None), BreakPlan::Refuse);
}

#[test]
fn an_editor_request_is_checked_and_carried_out_synchronously() {
    let mut layout = three(agent());
    hide(&mut layout, &editor());
    let r = driven(layout);
    assert_eq!(
        r.driver.query(|c| c.editor_request_refusal(WIN)).as_deref(),
        Some("the editor is not ready yet"),
        "an nvim that does not run is refused"
    );
    assert_eq!(
        r.driver.query(|c| c.editor_request_refusal(WinId(9))).as_deref(),
        Some(feedback::LAYOUT_CHANGING_TEXT),
        "a window it does not hold cannot be judged"
    );
    r.host.set_editor_running(true);
    assert_eq!(r.driver.query(|c| c.editor_request_refusal(WIN)), None);
    let keys = "<Cmd>lua EitriScratch.call('00')<CR>".to_string();
    let mark = r.mark();
    assert_eq!(r.driver.editor_request(WIN, keys.clone()), Ok(()));
    let ops = r.ops_since(mark);
    let mapped = position(&ops, |op| {
        *op == HostOp::Map {
            win: WIN,
            module: editor(),
        }
    });
    let focus = position(&ops, focus_on(&editor()));
    let sent = position(&ops, |op| {
        *op == HostOp::Editor(EditorCall::SendKeys {
            win: WIN,
            keys: keys.clone(),
        })
    });
    assert!(mapped < focus && focus < sent, "{ops:#?}");
    assert_eq!(r.keys(), Some(editor()));
}

#[test]
fn a_failed_editor_show_returns_to_the_caller_without_a_local_toast() {
    let r = driven(Layout::new(leaf(agent()), agent()).unwrap());
    r.host.set_editor_running(true);
    assert_eq!(r.driver.query(|c| c.editor_request_refusal(WIN)), None);
    for keys in [String::new(), "gf".into()] {
        let mark = r.mark();
        assert_eq!(
            r.driver.editor_request(WIN, keys),
            Err(feedback::layout_refusal_text(&LayoutError::NotInTree(editor())))
        );
        assert_eq!(r.toast(), None);
        assert!(!r
            .ops_since(mark)
            .iter()
            .any(|op| matches!(op, HostOp::Editor(EditorCall::SendKeys { .. }) | HostOp::Focus { .. })));
    }
}

#[test]
fn an_editor_request_refuses_when_the_controller_cannot_be_borrowed() {
    let r = driven(three(agent()));
    r.host.set_editor_running(true);
    let mark = r.mark();
    let answer = r.driver.query_or(Ok(()), |_| r.driver.editor_request(WIN, "gf".into()));
    assert_eq!(answer, Err(feedback::LAYOUT_CHANGING_TEXT.into()));
    assert!(r.ops_since(mark).is_empty());
}

#[test]
fn an_editor_request_is_refused_where_the_editor_is_gone_or_not_this_windows() {
    let mut layout = three(agent());
    crate::layout::kill(&mut layout, &editor(), Reopen::Never, &Frame::new(GRID, 1)).unwrap();
    assert!(layout.is_gone(&editor()), "the premise: the editor was killed for good");
    let r = driven(layout);
    r.host.set_editor_running(true);
    assert_eq!(
        r.driver.query(|c| c.editor_request_refusal(WIN)),
        Some(LayoutError::Gone(editor()).to_string()),
        "a killed editor is refused with the layout's own words"
    );

    for presence in [EditorPresence::Proxy, EditorPresence::None] {
        let r = driven_with(three(agent()), presence.clone());
        r.host.set_editor_running(true);
        assert_eq!(
            r.driver.query(|c| c.editor_request_refusal(WIN)).as_deref(),
            Some(feedback::NO_EDITOR_HERE_TEXT),
            "a window that does not own the editor sends the request nowhere: {presence:?}"
        );
    }
}

#[test]
fn a_review_shows_the_editor_and_gives_it_the_keys() {
    let mut layout = three(agent());
    hide(&mut layout, &editor());
    let r = driven(layout);
    r.chat(ChatEvent::ReviewOpen { win: WIN });
    assert!(r.layout.borrow().is_shown(&editor()));
    assert_eq!(r.keys(), Some(editor()));
}

#[test]
fn an_ended_edit_returns_the_keys_to_its_view_or_else_the_base_view() {
    let r = driven(with_extra_view(editor()));
    r.chat(ChatEvent::EditorDone { win: WIN, view: two() });
    assert_eq!(r.keys(), Some(agent2()));

    let r = driven(three(editor()));
    r.chat(ChatEvent::EditorDone { win: WIN, view: two() });
    assert_eq!(r.keys(), Some(agent()), "the edit's view has gone: the base view");
}

#[test]
fn a_reload_acts_in_the_view_with_the_keys_else_the_base_view() {
    let r = driven(with_extra_view(agent2()));
    let mark = r.mark();
    r.run(Action::PanelReload);
    assert_eq!(
        r.ops_since(mark),
        vec![HostOp::Chat(ChatOp::Reload { win: WIN, view: two() })]
    );
    let r = driven(three(terminal()));
    let mark = r.mark();
    r.run(Action::PanelReload);
    assert_eq!(
        r.ops_since(mark),
        vec![HostOp::Chat(ChatOp::Reload {
            win: WIN,
            view: ViewId::BASE
        })]
    );
}

/// A chat hidden from a tabbed container whose other tab then took the keys comes back shown but
/// behind that tab, which holds the keys: the reveal reports it so the caller toasts instead. In a
/// container beside the keys its tab is drawn and the keys stay where they are.
#[test]
fn a_hidden_chat_in_a_tabbed_container_is_drawn_or_reported_behind() {
    use crate::layout::{hide, Child, Con, ConLayout, Frame, Layout, Node, Size};
    let frame = Frame::new(Size { w: 1280, h: 721 }, 1);
    let chat = ModuleId::agent();

    let mut tabs = Con::new(
        ConLayout::Tabbed,
        vec![
            Child::new(Node::Leaf(ModuleId::editor()), 0.5),
            Child::new(Node::Leaf(ModuleId::agent()), 0.5),
        ],
    );
    tabs.focus = 1;
    let mut layout = Layout::new(Node::Con(tabs), chat.clone()).unwrap();
    let next = hide(&mut layout, &chat, &frame)
        .unwrap()
        .expect("the chat held the keys");
    layout.set_focus(&next).unwrap();
    assert_eq!(next, ModuleId::editor());
    assert_eq!(reveal_hidden(&mut layout, &chat).unwrap(), (true, false));
    assert!(layout.is_tabbed_away(&chat), "shown, on the tab not drawn");
    assert_eq!(layout.focus(), &ModuleId::editor());

    let mut tabs = Con::new(
        ConLayout::Tabbed,
        vec![
            Child::new(Node::Leaf(ModuleId::agent()), 0.5),
            Child::new(Node::Leaf(ModuleId::terminal()), 0.5),
        ],
    );
    tabs.focus = 1;
    let mut layout = Layout::new(
        Node::split(Axis::Row, 0.5, Node::Leaf(ModuleId::editor()), Node::Con(tabs)),
        ModuleId::editor(),
    )
    .unwrap();
    hide(&mut layout, &chat, &frame).unwrap();
    assert!(!layout.is_shown(&chat));
    assert_eq!(reveal_hidden(&mut layout, &chat).unwrap(), (true, true));
    assert!(layout.is_visible(&chat));
    assert_eq!(layout.focus(), &ModuleId::editor());

    assert_eq!(
        reveal_hidden(&mut layout, &chat).unwrap(),
        (false, true),
        "already on screen"
    );
}

/// The attention policy's reveal is the controller's change: the hidden chat is mapped and laid out, and the keys
/// stay where they were.
#[test]
fn a_card_reveals_its_hidden_chat_through_the_controller_and_leaves_the_keys() {
    let mut layout = three(editor());
    hide(&mut layout, &agent());
    let r = driven(layout);
    let mark = r.mark();
    r.chat(ChatEvent::Reveal {
        win: WIN,
        view: ViewId::BASE,
    });
    let ops = r.ops_since(mark);
    let mapped = position(&ops, |op| {
        *op == HostOp::Map {
            win: WIN,
            module: agent(),
        }
    });
    let allocated = position(&ops, |op| *op == HostOp::Allocate { win: WIN });
    assert!(mapped < allocated, "{ops:#?}");
    assert!(!ops.iter().any(|op| matches!(op, HostOp::Focus { .. })), "{ops:#?}");
    assert!(r.layout.borrow().is_visible(&agent()));
    assert_eq!(r.keys(), Some(editor()));
    let mark = r.mark();
    r.chat(ChatEvent::Reveal {
        win: WIN,
        view: ViewId::BASE,
    });
    assert!(r.ops_since(mark).is_empty(), "a view on screen already changes nothing");
}

/// Window 2's editor requests reach window 1's controller as a chat's do: the editor is shown and given the keys
/// before nvim is typed anything, and a request for the editor alone types nothing.
#[test]
fn drive_editor_shows_the_editor_before_nvim_is_asked() {
    let mut layout = three(agent());
    hide(&mut layout, &editor());
    let r = driven(layout);
    r.host.set_editor_running(true);
    let mark = r.mark();
    assert_eq!(r.driver.editor_request(WIN, String::new()), Ok(()));
    let ops = r.ops_since(mark);
    let mapped = position(&ops, |op| {
        *op == HostOp::Map {
            win: WIN,
            module: editor(),
        }
    });
    let focus = position(&ops, focus_on(&editor()));
    assert!(mapped < focus, "{ops:#?}");
    assert!(
        !ops.iter()
            .any(|op| matches!(op, HostOp::Editor(EditorCall::SendKeys { .. }))),
        "nothing is typed for the editor alone: {ops:#?}"
    );
    assert_eq!(r.keys(), Some(editor()));

    r.run(Action::Module(agent()));
    let keys = "gf".to_string();
    let mark = r.mark();
    assert_eq!(r.driver.editor_request(WIN, keys.clone()), Ok(()));
    let ops = r.ops_since(mark);
    let focus = position(&ops, focus_on(&editor()));
    let sent = position(&ops, |op| {
        *op == HostOp::Editor(EditorCall::SendKeys {
            win: WIN,
            keys: keys.clone(),
        })
    });
    assert!(focus < sent, "{ops:#?}");
}

/// A plugin page's `layout.focus` is the controller's: a hidden module is shown, then given the keys, in one batch;
/// one the layout does not hold is refused with the layout's words and nothing moves.
#[test]
fn a_plugin_pages_focus_is_decided_by_the_controller() {
    let mut layout = three(editor());
    hide(&mut layout, &terminal());
    let r = driven(layout);
    let mark = r.mark();
    assert_eq!(r.driver.plugin_focus(WIN, &terminal()), Ok(()));
    let ops = r.ops_since(mark);
    let mapped = position(&ops, |op| {
        *op == HostOp::Map {
            win: WIN,
            module: terminal(),
        }
    });
    let focus = position(&ops, focus_on(&terminal()));
    assert!(mapped < focus, "{ops:#?}");
    assert_eq!(r.keys(), Some(terminal()));

    let mark = r.mark();
    let refused = r.driver.plugin_focus(WIN, &ModuleId::lua("nowhere"));
    assert!(refused.is_err(), "{refused:?}");
    assert!(
        r.ops_since(mark)
            .iter()
            .all(|op| !matches!(op, HostOp::Map { .. } | HostOp::Focus { .. } | HostOp::Allocate { .. })),
        "a refused focus moves nothing"
    );
    assert_eq!(r.keys(), Some(terminal()));
}

fn attention_choice(
    r: &Rig,
    policy: crate::attention::ChatOnPermission,
    mine: crate::attention::Attention,
) -> AttentionChoice {
    let choice = r
        .driver
        .query_or(AttentionChoice::Toast, |c| c.attention(WIN, ViewId::BASE, policy, mine));
    match choice {
        AttentionChoice::Reveal(event) => {
            r.chat(event.clone());
            r.driver
                .query_or(AttentionChoice::Toast, |c| c.attention_revealed(&event))
        }
        choice => choice,
    }
}

fn waiting_card() -> crate::attention::Attention {
    crate::attention::Attention {
        pending: 1,
        arrived: 1,
        unread: false,
    }
}

#[test]
fn attention_on_screen_or_without_a_waiting_card_needs_nothing() {
    use crate::attention::ChatOnPermission;
    let r = driven(three(editor()));
    for policy in [ChatOnPermission::Badge, ChatOnPermission::Reveal] {
        assert_eq!(attention_choice(&r, policy, waiting_card()), AttentionChoice::None);
        hide(&mut r.layout.borrow_mut(), &agent());
        assert_eq!(attention_choice(&r, policy, Default::default()), AttentionChoice::None);
        r.layout.borrow_mut().show(&agent()).unwrap();
    }
}

#[test]
fn attention_toasts_for_badge_or_a_zoom_and_reveals_a_hidden_view() {
    use crate::attention::ChatOnPermission;
    for zoomed in [false, true] {
        for policy in [ChatOnPermission::Badge, ChatOnPermission::Reveal] {
            let mut layout = three(editor());
            hide(&mut layout, &agent());
            if zoomed {
                layout.toggle_zoom(&editor());
            }
            let r = driven(layout);
            let reveal = !zoomed && policy == ChatOnPermission::Reveal;
            assert_eq!(
                attention_choice(&r, policy, waiting_card()),
                if reveal {
                    AttentionChoice::None
                } else {
                    AttentionChoice::Toast
                }
            );
            assert_eq!(r.layout.borrow().is_shown(&agent()), reveal);
            assert_eq!(r.keys(), Some(editor()));
        }
    }
}

#[test]
fn attention_toasts_when_a_float_still_covers_and_reveals_after_stowing_a_scratch_float() {
    use crate::attention::ChatOnPermission;
    for scratch in [false, true] {
        let mut layout = three(editor());
        layout
            .add_float(&ModuleId::lua("cover"), FloatRect::centred(GRID, 0.9), scratch)
            .unwrap();
        let r = driven(layout);
        assert_eq!(
            attention_choice(&r, ChatOnPermission::Reveal, waiting_card()),
            if scratch {
                AttentionChoice::None
            } else {
                AttentionChoice::Toast
            }
        );
        assert_eq!(r.layout.borrow().is_shown(&ModuleId::lua("cover")), !scratch);
        assert_eq!(r.keys(), Some(editor()));
    }
}

/// The project chooser covers a view as a float does: stowing the scratch float leaves the card out of reach.
#[test]
fn attention_toasts_when_the_chooser_still_covers_after_a_scratch_float_is_stowed() {
    use crate::attention::ChatOnPermission;
    let cover = ModuleId::lua("cover");
    let mut layout = three(editor());
    layout.add_float(&cover, FloatRect::centred(GRID, 0.9), true).unwrap();
    let r = driven(layout);
    r.driver.run(WIN, Action::ProjectChoose, Origin::Lua);
    assert_eq!(
        attention_choice(&r, ChatOnPermission::Reveal, waiting_card()),
        AttentionChoice::Toast
    );
    assert!(!r.layout.borrow().is_shown(&cover));
}

#[test]
fn attention_reveals_a_tab_beside_the_keys_and_toasts_for_a_tab_behind_them() {
    use crate::attention::ChatOnPermission;
    use crate::layout::{Child, Con, ConLayout};
    for beside in [false, true] {
        let tabs = Node::Con(Con::new(
            ConLayout::Tabbed,
            vec![Child::new(leaf(terminal()), 0.5), Child::new(leaf(agent()), 0.5)],
        ));
        let root = if beside {
            Node::split(Axis::Row, 0.5, leaf(editor()), tabs)
        } else {
            tabs
        };
        let keys = if beside { editor() } else { terminal() };
        let r = driven(Layout::new(root, keys.clone()).unwrap());
        assert!(r.layout.borrow().is_tabbed_away(&agent()));
        assert_eq!(
            attention_choice(&r, ChatOnPermission::Reveal, waiting_card()),
            if beside {
                AttentionChoice::None
            } else {
                AttentionChoice::Toast
            }
        );
        assert_eq!(r.layout.borrow().is_tabbed_away(&agent()), !beside);
        assert_eq!(r.keys(), Some(keys));
    }
}
