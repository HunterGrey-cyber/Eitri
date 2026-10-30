use super::*;
use crate::keymap::{prefix::Press, KeySpec};
use crate::layout::{Axis, Layout, Node, Workspaces};
use crate::tabs::ViewId;
use crate::wm::fake::{FakeHost, FakeScheduler, Logged};
use crate::wm::host::*;
use crate::wm::persist::Saver;
use crate::wm::verbs::rig::{decl, rig_with, WIN};
use crate::wm::Driver;
use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;

struct Rig {
    driver: Rc<Driver>,
    host: Rc<FakeHost>,
    scheduler: Rc<FakeScheduler>,
    layout: Rc<RefCell<Layout>>,
}

fn driven(layout: Layout) -> Rig {
    driven_with(layout, |_, _| None)
}

fn driven_with(layout: Layout, saver: impl FnOnce(&Layout, &Workspaces) -> Option<Saver>) -> Rig {
    let r = rig_with(layout, saver);
    let hosted: Vec<_> = r.layout.borrow().modules().iter().map(decl).collect();
    for d in hosted {
        r.host.with_surface(WIN, d.module, d.kind, false);
    }
    let driver = Driver::new(r.controller, r.host.clone(), r.scheduler.clone());
    let sink: Rc<dyn HostSink> = driver.clone();
    r.host.attach(Rc::downgrade(&sink));
    r.scheduler.attach(Rc::downgrade(&sink));
    for op in r.first.ops() {
        r.host.apply(op);
    }
    Rig {
        driver,
        host: r.host,
        scheduler: r.scheduler,
        layout: r.layout,
    }
}

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

impl Rig {
    fn ops(&self) -> Vec<HostOp> {
        self.host
            .log()
            .into_iter()
            .filter_map(|entry| match entry {
                Logged::Op(op, _) if !matches!(op, HostOp::Chrome { .. }) => Some(op),
                _ => None,
            })
            .collect()
    }
    fn advance(&self) {
        self.scheduler.advance(Duration::ZERO);
    }
    fn running(&self) {
        self.host.set_chat_kill_counts(
            ViewId::BASE,
            ChatKillCounts {
                running: 1,
                ..Default::default()
            },
        );
    }
    fn answer(&self, key: &str) {
        let spec = KeySpec::parse(key).unwrap();
        self.driver.key(
            WIN,
            &KeyStage::Window,
            KeyPress {
                keycode: 42,
                press: Press::Key(spec),
                key: Some(spec.key),
                mods: Mods::default(),
                super_or_hyper: false,
                super_or_meta: false,
            },
        );
    }
    fn exited(&self, settled: Option<NvimAnswer>) {
        self.host.set_editor_facts(EditorFacts {
            exited: true,
            ..Default::default()
        });
        self.driver
            .post(HostEvent::Editor(EditorEvent::Exited { win: WIN, settled }));
    }
    fn signal(&self) {
        self.driver.post(HostEvent::EndSignal { name: "SIGTERM" });
    }
    fn close_count(&self) -> usize {
        self.ops()
            .iter()
            .filter(|op| matches!(op, HostOp::CloseWindow { .. }))
            .count()
    }
}

fn teardown_ops(written: bool) -> Vec<HostOp> {
    vec![
        HostOp::Cleanup(CleanupOp::PairOnClose {
            layout_written: written,
        }),
        HostOp::Editor(EditorCall::Shutdown { win: WIN }),
        HostOp::Terminal(TermOp::ShutdownAll { win: WIN }),
        HostOp::Cleanup(CleanupOp::ScratchDir),
        HostOp::Cleanup(CleanupOp::PaneSwitchDir),
        HostOp::Cleanup(CleanupOp::Feeds),
        HostOp::Plugin(PluginOp::ShutdownAll { win: WIN }),
        HostOp::Chat(ChatOp::ShutdownAll { win: WIN, hold: true }),
        HostOp::CloseWindow { win: WIN },
    ]
}

#[test]
fn a_signal_close_asks_nothing_and_tears_down_in_order() {
    let r = driven(two());
    r.running();
    let at = r.ops().len();
    r.signal();
    assert_eq!(&r.ops()[at..], teardown_ops(false));
    assert!(r.driver.query(|c| c.chrome(WIN).prompt.is_none()));
}

#[test]
fn a_close_that_may_proceed_tears_down_from_an_idle_never_inside_the_request() {
    let r = driven(two());
    let at = r.ops().len();
    assert_eq!(r.driver.close_requested(WIN, CloseBy::User), CloseVerdict::Stop);
    assert_eq!(r.ops().len(), at);
    r.advance();
    assert_eq!(&r.ops()[at..], teardown_ops(false));
}

#[test]
fn a_close_with_a_running_turn_asks_once_and_y_answers_both() {
    let r = driven(two());
    r.running();
    r.host.set_editor_running(true);
    r.driver.close_requested(WIN, CloseBy::User);
    let prompt = r.driver.query(|c| c.chrome(WIN).prompt.unwrap());
    r.driver.close_requested(WIN, CloseBy::Compositor);
    assert_eq!(r.driver.query(|c| c.chrome(WIN).prompt.unwrap()), prompt);
    let at = r.ops().len();
    r.answer("y");
    assert_eq!(
        &r.ops()[at..],
        &[
            HostOp::Focus {
                win: WIN,
                to: FocusTo::Module(ModuleId::editor()),
                on_grab: vec![]
            },
            HostOp::Editor(EditorCall::Quit {
                win: WIN,
                lua: editor_quit_lua(1),
                generation: 1
            }),
            HostOp::Editor(EditorCall::SetFocused {
                win: WIN,
                focused: true
            }),
            HostOp::Chat(ChatOp::EditorHasKeys {
                win: WIN,
                has_keys: true
            }),
            HostOp::Chat(ChatOp::PaneFocused {
                win: WIN,
                view: ViewId::BASE,
                focused: false
            }),
        ]
    );
    assert!(r
        .ops()
        .iter()
        .any(|op| matches!(op, HostOp::Editor(EditorCall::Quit { generation: 1, .. }))));
    r.exited(Some(NvimAnswer {
        sent_at: r.scheduler.now(),
        done: true,
        returned: false,
        last: None,
    }));
    assert!(r.driver.query(|c| c.chrome(WIN).prompt.is_none()));
    r.advance();
    assert_eq!(r.close_count(), 1);
}

#[test]
fn a_close_window_op_tears_down_exactly_once() {
    let r = driven(two());
    r.host.on_apply(|host, _, op| {
        if matches!(op, HostOp::Editor(EditorCall::Shutdown { .. })) {
            host.post(HostEvent::EndSignal { name: "SIGINT" });
            host.post(HostEvent::Editor(EditorEvent::Exited {
                win: WIN,
                settled: None,
            }));
        }
    });
    r.signal();
    r.driver.close_requested(WIN, CloseBy::Compositor);
    r.signal();
    assert_eq!(r.close_count(), 1);
}

#[test]
fn a_signal_during_the_close_prompt_or_nvims_quit_tears_down_once() {
    for answer in [false, true] {
        let r = driven(two());
        r.running();
        r.host.set_editor_running(true);
        r.driver.close_requested(WIN, CloseBy::User);
        if answer {
            r.answer("y");
        }
        r.signal();
        let at = r.ops().len();
        r.answer("y");
        r.exited(None);
        r.advance();
        assert_eq!(r.ops().len(), at);
        assert_eq!(r.close_count(), 1);
        assert!(r.driver.query(|c| c.chrome(WIN).prompt.is_none()));
    }
}

#[test]
fn the_last_terminals_exit_asks_what_a_window_close_asks() {
    let r = driven(Layout::new(Node::Leaf(ModuleId::terminal()), ModuleId::terminal()).unwrap());
    r.running();
    let at = r.ops().len();
    r.driver.post(HostEvent::Terminal(TermEvent::Exited {
        win: WIN,
        module: ModuleId::terminal(),
    }));
    assert_eq!(
        &r.ops()[at..],
        &[HostOp::Terminal(TermOp::SetFocused {
            win: WIN,
            module: ModuleId::terminal(),
            focused: false,
        })]
    );
    assert_eq!(
        r.driver.query(|c| c.chrome(WIN).prompt.unwrap().text),
        crate::tabs::window_close_prompt_with_jobs(1, 0, 0, 0).unwrap()
    );
}

#[test]
fn a_non_y_answer_to_the_window_close_runs_the_decline_once() {
    let r = driven(two());
    r.running();
    r.exited(None);
    r.answer("n");
    let at = r.ops().len();
    r.answer("n");
    assert_eq!(r.ops().len(), at, "the second answer found no question to answer");
    r.advance();
    assert_eq!(
        r.ops()
            .iter()
            .filter(|op| matches!(op, HostOp::Editor(EditorCall::ReleaseExited { .. })))
            .count(),
        1
    );
    assert_eq!(r.close_count(), 0);
    assert_eq!(
        r.driver.query(|c| c.chrome(WIN).toast.unwrap().text),
        crate::wm::feedback::editor_ended_text(&crate::keymap::Keymap::defaults())
    );
}

#[test]
fn an_unasked_exit_closes_the_window_once() {
    let r = driven(two());
    r.exited(None);
    r.exited(None);
    r.advance();
    assert_eq!(r.close_count(), 1);

    // A close confirmed with `y` and handed to nvim, which then exits without being asked again.
    let r = driven(two());
    r.running();
    r.host.set_editor_running(true);
    r.driver.close_requested(WIN, CloseBy::User);
    r.answer("y");
    let quit_done = Some(NvimAnswer {
        sent_at: r.scheduler.now(),
        done: true,
        returned: false,
        last: None,
    });
    r.exited(quit_done.clone());
    r.exited(quit_done);
    r.advance();
    assert_eq!(r.close_count(), 1);
    assert!(
        r.driver.query(|c| c.chrome(WIN).prompt.is_none()),
        "the confirmed close asks nothing twice"
    );
}

#[test]
fn a_quit_cancelled_letter_that_names_no_quit_is_ignored() {
    let r = driven(two());
    r.host.set_editor_running(true);
    r.driver.close_requested(WIN, CloseBy::User);
    let at = r.ops().len();
    // `(1 << 32) + 1` would name the live quit, 1, if the letter's generation were truncated instead of refused.
    for generation in [u64::MAX, (1 << 32) + 1, 0] {
        r.driver
            .post(HostEvent::Editor(EditorEvent::QuitCancelled { win: WIN, generation }));
    }
    assert_eq!(r.ops().len(), at);
    assert!(r.driver.query(|c| c.chrome(WIN).toast.is_none()));
    // The quit is still in flight: while nvim asks, a second close is told to answer there, where a cleared quit
    // would have sent a fresh one.
    let now = r.scheduler.now();
    r.host.set_editor_facts(EditorFacts {
        running: true,
        watch: Some(NvimAnswer {
            sent_at: now,
            done: false,
            returned: false,
            last: Some(NvimReply {
                at: now,
                mode: "r?".into(),
                blocking: false,
            }),
        }),
        ..Default::default()
    });
    r.driver.close_requested(WIN, CloseBy::User);
    assert!(
        !r.ops()
            .iter()
            .any(|op| matches!(op, HostOp::Editor(EditorCall::Quit { generation: 2, .. }))),
        "an ignored letter left quit 1 in flight"
    );
    assert_eq!(
        r.driver.query(|c| c.chrome(WIN).toast.unwrap().text),
        answer_there_text(NvimState::Asking)
    );
    r.exited(Some(NvimAnswer {
        sent_at: r.scheduler.now(),
        done: true,
        returned: false,
        last: None,
    }));
    r.advance();
    assert_eq!(r.close_count(), 1);
}

#[test]
fn a_refused_set_scale_during_a_quit_leaves_the_quit_in_flight() {
    let r = driven(two());
    r.host.set_editor_running(true);
    r.driver.close_requested(WIN, CloseBy::User);
    let at = r.ops().len();
    r.driver.post(HostEvent::Refused {
        what: OpTag::Editor,
        why: "scale refused".into(),
        surface: Some((WIN, ModuleId::editor())),
    });
    r.driver.post(HostEvent::Editor(EditorEvent::QuitNotSent {
        win: WIN,
        generation: 0,
    }));
    assert_eq!(r.ops().len(), at);
    assert!(r.driver.query(|c| c.chrome(WIN).toast.is_none()));
    r.driver.post(HostEvent::Editor(EditorEvent::QuitNotSent {
        win: WIN,
        generation: 1,
    }));
    assert_eq!(r.ops().len(), at, "the refusal batch contains only chrome");
    assert_eq!(
        r.driver.query(|c| c.chrome(WIN).toast.unwrap().text),
        "nvim could not be asked to quit — quit it in the editor (:qa)"
    );
}

fn saved(dir: &Path, layout: Layout) -> Rig {
    let dir = dir.to_path_buf();
    driven_with(layout, |layout, workspaces| {
        Some(Saver::new(
            Some(dir),
            Path::new("/close-project"),
            vec![],
            None,
            layout,
            workspaces,
        ))
    })
}

#[test]
fn the_teardown_saves_the_layout_before_it_shuts_the_editor_down() {
    let dir = agent::state_dirs::test_workspace_dir("close-before-shutdown");
    let r = saved(&dir, crate::wm::persist::first_launch_layout(&[]).unwrap());
    r.layout.borrow_mut().show(&ModuleId::terminal()).unwrap();
    let path = dir.join(crate::layout::persist::file_name_v2(Path::new("/close-project")));
    let check = path.clone();
    r.host.on_apply(move |_, _, op| {
        if matches!(op, HostOp::Editor(EditorCall::Shutdown { .. })) {
            assert!(check.exists(), "the save finished before applying editor shutdown");
        }
    });
    let at = r.ops().len();
    r.signal();
    assert_eq!(&r.ops()[at..], teardown_ops(true));
    assert!(path.exists());
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn a_failed_layout_write_tells_the_pair_nothing_was_written() {
    let dir = agent::state_dirs::test_workspace_dir("close-failed-write");
    let state = dir.join("state");
    std::fs::write(&state, "not a directory").unwrap();
    let r = saved(&state, crate::wm::persist::first_launch_layout(&[]).unwrap());
    r.layout.borrow_mut().show(&ModuleId::terminal()).unwrap();
    let at = r.ops().len();
    r.signal();
    assert_eq!(&r.ops()[at..], teardown_ops(false));
    std::fs::remove_file(&state).unwrap();
    // A window tears down once; the next window's close is the next save.
    let next = saved(&state, crate::wm::persist::first_launch_layout(&[]).unwrap());
    next.layout.borrow_mut().show(&ModuleId::terminal()).unwrap();
    let at = next.ops().len();
    next.signal();
    assert_eq!(&next.ops()[at..], teardown_ops(true));
    std::fs::remove_dir_all(dir).unwrap();
}

fn pinned() -> Layout {
    use crate::layout::persist::Loaded;
    use crate::layout::OpenRules;
    use crate::lua::layout::LuaTree;
    use crate::wm::persist::choose_startup_layout;
    choose_startup_layout(
        Some(&LuaTree {
            tiled: Node::split(
                Axis::Column,
                0.7,
                Node::split(
                    Axis::Row,
                    0.5,
                    Node::Leaf(ModuleId::editor()),
                    Node::Leaf(ModuleId::agent()),
                ),
                Node::Leaf(ModuleId::terminal()),
            ),
            floats: vec![],
        }),
        None,
        Some(Loaded::Missing),
        &[],
        &OpenRules::default(),
        None,
        &[],
        &[],
    )
    .unwrap()
    .1
}

#[test]
fn the_teardown_hears_pending_settles_before_it_saves() {
    let dir = agent::state_dirs::test_workspace_dir("close-pending-settle");
    let r = saved(&dir, pinned());
    let path = dir.join(crate::layout::persist::file_name_v2(Path::new("/close-project")));
    r.driver.allocate(WIN, crate::layout::Size { w: 1200, h: 800 });
    assert!(!r.layout.borrow().has_pin_to_settle());
    r.signal();
    assert!(!path.exists(), "a settled pin is still the unchanged launch layout");
    let at = r.ops().len();
    r.advance();
    assert_eq!(r.ops().len(), at, "the late idle heard nothing twice");
    assert!(!path.exists());
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn prefix_x_on_the_last_module_asks_the_window_close() {
    let r = driven(Layout::new(Node::Leaf(ModuleId::agent()), ModuleId::agent()).unwrap());
    r.running();
    r.host.set_terminal_jobs(2);
    r.driver
        .run(WIN, crate::keymap::Action::ModuleKill, Origin::Key { keycode: 42 });
    let prompt = r.driver.query(|c| c.chrome(WIN).prompt.unwrap());
    assert!(prompt.closes_window);
    assert_eq!(
        prompt.text,
        "kill-pane agent? closes Eitri, 1 running, 2 terminal jobs (y/n)"
    );
    r.answer("y");
    r.advance();
    assert_eq!(r.close_count(), 1);
}

#[test]
fn the_teardown_ops_are_answered_done_in_order() {
    let host = FakeHost::new();
    let sink = Rc::new(crate::wm::conformance::RecordingSink::default());
    let erased: Rc<dyn HostSink> = sink.clone();
    host.attach(Rc::downgrade(&erased));
    let script = crate::wm::conformance::teardown_script(WIN);
    crate::wm::conformance::run(host.as_ref(), &sink, &script).unwrap();
    assert_eq!(
        host.log()
            .into_iter()
            .filter_map(|entry| match entry {
                Logged::Op(op, Applied::Done) => Some(op),
                _ => None,
            })
            .collect::<Vec<_>>(),
        teardown_ops(true)
    );
}

fn assert_feedback(outcome: crate::wm::feedback::CloseOutcome<'_>, flash: bool, text: Option<&str>) {
    let actual = crate::wm::feedback::close(outcome);
    assert_eq!(actual.flash, flash);
    assert_eq!(actual.toast.as_deref(), text);
}

#[test]
fn feedback_close_window_refused() {
    let text = answer_there_text(NvimState::Asking);
    assert_feedback(crate::wm::feedback::CloseOutcome::Refused(text), true, Some(text));
}

#[test]
fn feedback_close_editor_kill_refused() {
    assert_feedback(crate::wm::feedback::CloseOutcome::KillRefused, true, None);
}

#[test]
fn feedback_close_quit_not_sent() {
    let text = "nvim could not be asked to quit — quit it in the editor (:qa)";
    assert_feedback(crate::wm::feedback::CloseOutcome::Refused(text), true, Some(text));
}

#[test]
fn feedback_close_unreachable() {
    let r = driven(two());
    let at = r.ops().len();
    let flash = r.driver.query(|c| c.chrome(WIN).top_bar.flash);
    r.driver.post(HostEvent::Editor(EditorEvent::Unreachable { win: WIN }));
    assert_eq!(r.ops().len(), at);
    assert_eq!(r.driver.query(|c| c.chrome(WIN).top_bar.flash), flash);
    assert_eq!(
        r.driver.query(|c| c.chrome(WIN).toast.unwrap().text),
        crate::wm::quit::UNREACHABLE_NVIM
    );
    assert_feedback(
        crate::wm::feedback::CloseOutcome::Toast(crate::wm::quit::UNREACHABLE_NVIM),
        false,
        Some(crate::wm::quit::UNREACHABLE_NVIM),
    );
}

#[test]
fn feedback_close_retired() {
    assert_feedback(
        crate::wm::feedback::CloseOutcome::Toast(&crate::wm::feedback::editor_ended_text(
            &crate::keymap::Keymap::defaults(),
        )),
        false,
        Some(&crate::wm::feedback::editor_ended_text(
            &crate::keymap::Keymap::defaults(),
        )),
    );
}

#[test]
fn feedback_close_window_not_answering() {
    let quit = EditorQuit::Window { confirmed: None };
    let text = not_answering_text(&quit);
    assert_feedback(crate::wm::feedback::CloseOutcome::Toast(text), false, Some(text));
}

#[test]
fn feedback_close_module_not_answering() {
    let text = not_answering_text(&EditorQuit::Module);
    assert_feedback(crate::wm::feedback::CloseOutcome::Toast(text), false, Some(text));
}

#[test]
fn on_exit_reads_can_kills_scope() {
    assert_eq!(on_shell_exit(KillScope::Module), OnExit::CloseModule);
    assert_eq!(on_shell_exit(KillScope::Window), OnExit::CloseWindow);
}

#[test]
fn feedback_close_last_editor_refused() {
    let r = driven(Layout::new(Node::Leaf(ModuleId::editor()), ModuleId::editor()).unwrap());
    let now = r.scheduler.now();
    r.host.set_editor_facts(EditorFacts {
        running: true,
        watch: Some(NvimAnswer {
            sent_at: now,
            done: false,
            returned: false,
            last: Some(NvimReply {
                at: now,
                mode: "r?".into(),
                blocking: false,
            }),
        }),
        ..Default::default()
    });
    for _ in 0..2 {
        r.driver
            .run(WIN, crate::keymap::Action::ModuleKill, Origin::Key { keycode: 42 });
        r.answer("y");
    }
    assert_eq!(r.driver.query(|c| c.chrome(WIN).top_bar.flash), 1);
    assert!(
        r.driver.query(|c| c.chrome(WIN).toast.is_none()),
        "a refused module kill flashes without a toast"
    );
    assert_eq!(
        r.ops()
            .iter()
            .filter(|op| matches!(op, HostOp::Editor(EditorCall::Quit { .. })))
            .count(),
        1
    );
}

#[test]
fn the_window_close_counts_each_chat_views_tab_once() {
    let r = driven(two());
    let view = ViewId::of_module(&ModuleId::instance(ModuleKind::Agent, 2).unwrap()).unwrap();
    r.host.set_chat_views(vec![view]);
    // The base view's count is every tab's, the extra view's own tab included.
    r.running();
    r.host.set_chat_kill_counts(
        view,
        ChatKillCounts {
            running: 1,
            ..Default::default()
        },
    );
    r.driver.close_requested(WIN, CloseBy::User);
    assert_eq!(
        r.driver.query(|c| c.chrome(WIN).prompt.unwrap().text),
        crate::tabs::window_close_prompt_with_jobs(1, 0, 0, 0).unwrap()
    );
}

/// Background work alone makes the window close ask, counted over the base view's tabs and each extra view's tab,
/// and the last module's kill names it in its one question.
#[test]
fn the_window_close_asks_while_only_background_work_runs() {
    let r = driven(two());
    let view = ViewId::of_module(&ModuleId::instance(ModuleKind::Agent, 2).unwrap()).unwrap();
    r.host.set_chat_views(vec![ViewId::BASE, view]);
    for v in [ViewId::BASE, view] {
        r.host.set_chat_kill_counts(
            v,
            ChatKillCounts {
                background: 1,
                ..Default::default()
            },
        );
    }
    r.driver.close_requested(WIN, CloseBy::User);
    let prompt = r.driver.query(|c| c.chrome(WIN).prompt.unwrap());
    assert!(prompt.closes_window);
    assert_eq!(
        prompt.text,
        crate::tabs::window_close_prompt_with_jobs(0, 0, 0, 2).unwrap()
    );
    assert_eq!(r.close_count(), 0);

    let r = driven(Layout::new(Node::Leaf(ModuleId::terminal()), ModuleId::terminal()).unwrap());
    r.host.set_chat_kill_counts(
        ViewId::BASE,
        ChatKillCounts {
            background: 1,
            ..Default::default()
        },
    );
    r.driver.run(WIN, crate::keymap::Action::ModuleKill, Origin::Lua);
    let text = r.driver.query(|c| c.chrome(WIN).prompt.unwrap().text);
    assert!(text.contains("closes Eitri, 1 with background tasks"), "{text}");
}

fn agent_focused() -> Layout {
    Layout::new(
        Node::split(
            Axis::Row,
            0.5,
            Node::Leaf(ModuleId::editor()),
            Node::Leaf(ModuleId::agent()),
        ),
        ModuleId::agent(),
    )
    .unwrap()
}

#[test]
fn a_module_question_over_the_window_close_leaves_the_next_close_asking() {
    let r = driven(agent_focused());
    r.running();
    r.driver.close_requested(WIN, CloseBy::User);
    assert!(r.driver.query(|c| c.chrome(WIN).prompt.unwrap().closes_window));
    r.driver.run(WIN, crate::keymap::Action::ModuleKill, Origin::Lua);
    assert!(!r.driver.query(|c| c.chrome(WIN).prompt.unwrap().closes_window));
    r.answer("n");
    assert!(r.driver.query(|c| c.chrome(WIN).prompt.is_none()));
    r.driver.close_requested(WIN, CloseBy::User);
    let prompt = r.driver.query(|c| c.chrome(WIN).prompt.unwrap());
    assert!(prompt.closes_window, "the next close asks again");
    assert_eq!(
        prompt.text,
        crate::tabs::window_close_prompt_with_jobs(1, 0, 0, 0).unwrap()
    );
}

#[test]
fn a_module_question_over_the_window_close_after_nvim_exited_retires_the_editor() {
    let r = driven(agent_focused());
    r.running();
    r.exited(None);
    assert!(r.driver.query(|c| c.chrome(WIN).prompt.unwrap().closes_window));
    r.driver.run(WIN, crate::keymap::Action::ModuleKill, Origin::Lua);
    r.advance();
    assert_eq!(
        r.driver.query(|c| c.chrome(WIN).toast.unwrap().text),
        crate::wm::feedback::editor_ended_text(&crate::keymap::Keymap::defaults()),
        "the replaced close is declined, as a `no` to it is"
    );
    assert_eq!(r.close_count(), 0);
}

#[test]
fn a_no_to_the_last_modules_question_leaves_an_exited_editor_alone() {
    // The editor is hidden, not gone, so a declined window close would retire it.
    let r = driven(two());
    r.driver.run(WIN, crate::keymap::Action::ModuleHide, Origin::Lua);
    assert!(!r.layout.borrow().is_visible(&ModuleId::editor()));
    assert_eq!(r.layout.borrow().focus(), &ModuleId::agent());
    r.host.set_editor_facts(EditorFacts {
        exited: true,
        ..Default::default()
    });
    r.driver
        .run(WIN, crate::keymap::Action::ModuleKill, Origin::Key { keycode: 42 });
    assert!(r.driver.query(|c| c.chrome(WIN).prompt.unwrap().closes_window));
    r.answer("n");
    r.advance();
    assert!(r.driver.query(|c| c.chrome(WIN).prompt.is_none()));
    assert!(
        r.driver.query(|c| c.chrome(WIN).toast.is_none()),
        "declining a module's kill is not declining the window close"
    );
    assert!(!r
        .ops()
        .iter()
        .any(|op| matches!(op, HostOp::Editor(EditorCall::ReleaseExited { .. }))));
    assert_eq!(r.close_count(), 0);
}

/// Only the editor on screen, the chat hidden beside it: the editor's `prefix x` is the window close.
fn editor_alone_chat_hidden() -> Rig {
    let r = driven(agent_focused());
    r.driver.run(WIN, crate::keymap::Action::ModuleHide, Origin::Lua);
    assert!(!r.layout.borrow().is_visible(&ModuleId::agent()));
    assert_eq!(r.layout.borrow().focus(), &ModuleId::editor());
    r.host.set_editor_facts(EditorFacts {
        running: true,
        ..Default::default()
    });
    r.driver
        .run(WIN, crate::keymap::Action::ModuleKill, Origin::Key { keycode: 42 });
    assert!(r.driver.query(|c| c.chrome(WIN).prompt.unwrap().closes_window));
    r
}

#[test]
fn a_yes_after_another_module_came_on_screen_kills_only_the_editor() {
    let r = editor_alone_chat_hidden();
    r.driver
        .run(WIN, crate::keymap::Action::Module(ModuleId::agent()), Origin::Chip);
    r.advance();
    assert!(r.layout.borrow().is_visible(&ModuleId::agent()));
    r.answer("y");
    r.advance();
    assert!(r
        .ops()
        .iter()
        .any(|op| matches!(op, HostOp::Editor(EditorCall::Quit { .. }))));
    r.exited(Some(NvimAnswer {
        sent_at: r.scheduler.now(),
        done: true,
        returned: false,
        last: None,
    }));
    r.advance();
    assert_eq!(r.close_count(), 0, "the chat now on screen keeps the window open");
    assert!(
        !r.layout.borrow().is_gone(&ModuleId::editor()) && !r.layout.borrow().is_shown(&ModuleId::editor()),
        "nvim's exit kills the editor in place"
    );
    assert!(r.layout.borrow().is_visible(&ModuleId::agent()));
}

#[test]
fn nvim_exiting_during_the_last_modules_question_closes_the_window() {
    let r = editor_alone_chat_hidden();
    r.exited(None);
    r.advance();
    assert_eq!(
        r.close_count(),
        1,
        "nothing needed confirming, so the exit closes the window"
    );
}

#[test]
fn nvim_exiting_during_the_last_modules_question_asks_the_window_close_and_a_no_retires_it() {
    let r = editor_alone_chat_hidden();
    r.running();
    r.exited(None);
    let prompt = r.driver.query(|c| c.chrome(WIN).prompt.unwrap());
    assert!(prompt.closes_window);
    assert_eq!(
        prompt.text,
        crate::tabs::window_close_prompt_with_jobs(1, 0, 0, 0).unwrap(),
        "the window close's own question replaces the kill's"
    );
    r.answer("n");
    r.advance();
    assert_eq!(r.close_count(), 0);
    assert!(
        !r.layout.borrow().is_visible(&ModuleId::editor()),
        "the dead editor never stays on screen holding the keys"
    );
    assert_eq!(
        r.driver.query(|c| c.chrome(WIN).toast.unwrap().text),
        crate::wm::feedback::editor_ended_text(&crate::keymap::Keymap::defaults())
    );
}

#[test]
fn the_close_prompt_names_the_other_windows_work() {
    for last in [false, true] {
        let r = driven(if last {
            Layout::new(Node::Leaf(ModuleId::agent()), ModuleId::agent()).unwrap()
        } else {
            two()
        });
        r.running();
        let peer = crate::wm::PeerId(2);
        r.driver.attach_peer(WIN, peer, crate::wm::fake::FakePeerPort::new());
        r.driver.post(HostEvent::Peer(PeerEvent::Summary {
            peer,
            summary: crate::window_link::Summary {
                running: 2,
                queued: 3,
                ..Default::default()
            },
        }));
        if last {
            r.driver
                .run(WIN, crate::keymap::Action::ModuleKill, crate::wm::Origin::Lua);
        } else {
            r.driver.close_requested(WIN, CloseBy::User);
        }
        let text = r.driver.query(|c| c.chrome(WIN)).prompt.unwrap().text;
        assert!(
            text.contains("window 2: 2 running, 3 queued (stays open without the editor)"),
            "{text}"
        );
    }
}

fn moving_close() -> (Rig, Rc<crate::wm::fake::FakePeerPort>) {
    let r = driven(two());
    r.running();
    r.host
        .set_running_chats(vec![(ModuleId::agent(), Some(crate::tabs::TabId(1)))]);
    r.host.set_chat_move(
        ViewId::BASE,
        Ok(ChatMoveFacts {
            tab: crate::tabs::TabId(1),
            session: Some("running".into()),
            running: true,
            live: true,
        }),
    );
    let port = crate::wm::fake::FakePeerPort::new();
    r.driver.attach_peer(WIN, super::super::PeerId(2), port.clone());
    r.driver.close_requested(WIN, CloseBy::User);
    (r, port)
}

#[test]
fn m_moves_the_running_work_then_closes() {
    let (r, port) = moving_close();
    let prompt = r.driver.query(|c| c.chrome(WIN).prompt.unwrap());
    assert!(prompt.offers_move);
    assert!(prompt.text.contains("m: move 1"), "{}", prompt.text);
    r.answer("m");
    assert!(!r.ops().iter().any(|op| matches!(op, HostOp::CloseWindow { .. })));
    let id = port
        .sent()
        .iter()
        .find_map(|s| match s.msg {
            crate::window_link::Msg::MoveOffer { mv, .. } => Some(mv),
            _ => None,
        })
        .unwrap();
    r.driver.post(HostEvent::Peer(PeerEvent::MoveReady {
        peer: super::super::PeerId(2),
        id: crate::window_link::moves::MoveId(id),
    }));
    r.driver.post(HostEvent::Chat(ChatEvent::Frozen {
        win: WIN,
        view: ViewId::BASE,
        mv: crate::window_link::moves::MoveId(id),
        record: serde_json::json!({}),
        handoff: Handoff::new(Rc::new(())),
    }));
    r.driver.post(HostEvent::Peer(PeerEvent::MoveDone {
        peer: super::super::PeerId(2),
        id: crate::window_link::moves::MoveId(id),
    }));
    r.advance();
    let ops = r.ops();
    let cleanup = ops
        .iter()
        .position(|op| matches!(op, HostOp::Cleanup(CleanupOp::PairOnClose { .. })))
        .unwrap();
    let close = ops
        .iter()
        .position(|op| matches!(op, HostOp::CloseWindow { .. }))
        .unwrap();
    assert!(cleanup < close);
}

#[test]
fn m_with_a_refusal_still_closes_and_says_why() {
    let (r, port) = moving_close();
    r.answer("m");
    let id = port
        .sent()
        .iter()
        .find_map(|s| match s.msg {
            crate::window_link::Msg::MoveOffer { mv, .. } => Some(mv),
            _ => None,
        })
        .unwrap();
    r.driver.post(HostEvent::Peer(PeerEvent::MoveRefused {
        peer: super::super::PeerId(2),
        id: crate::window_link::moves::MoveId(id),
        why: "cannot move this session".into(),
    }));
    r.advance();
    assert!(r.ops().iter().any(|op| matches!(op, HostOp::CloseWindow { .. })));
    assert!(r
        .driver
        .query(|c| c.notes(WIN))
        .iter()
        .any(|s| s.contains("cannot move this session")));
}

#[test]
fn the_link_ending_while_m_moves_closes_the_window() {
    let (r, _) = moving_close();
    r.answer("m");
    r.driver.close_requested(WIN, CloseBy::User);
    assert!(r
        .driver
        .query(|c| c.notes(WIN))
        .iter()
        .any(|s| s == "[window] the window is already closing"));
    r.driver.post(HostEvent::Peer(PeerEvent::LinkClosed {
        peer: super::super::PeerId(2),
    }));
    r.advance();
    assert!(r.ops().iter().any(|op| matches!(op, HostOp::CloseWindow { .. })));
}

#[test]
fn m_waits_for_every_running_tab_even_when_the_first_is_refused() {
    let (r, port) = moving_close();
    let facts: Vec<_> = [1, 2]
        .into_iter()
        .map(|n| ChatMoveFacts {
            tab: crate::tabs::TabId(n),
            session: Some(format!("session-{n}")),
            running: true,
            live: true,
        })
        .collect();
    r.host.set_base_tabs(facts, Some(crate::tabs::TabId(1)));
    r.host.set_running_chats(
        [1, 2]
            .into_iter()
            .map(|n| (ModuleId::agent(), Some(crate::tabs::TabId(n))))
            .collect(),
    );
    r.answer("M");
    let ids: Vec<_> = port
        .sent()
        .into_iter()
        .filter_map(|s| match s.msg {
            crate::window_link::Msg::MoveOffer { mv, .. } => Some(mv),
            _ => None,
        })
        .collect();
    assert_eq!(ids.len(), 2);
    for (ix, id) in ids.into_iter().enumerate() {
        r.driver.post(HostEvent::Peer(PeerEvent::MoveRefused {
            peer: super::super::PeerId(2),
            id: crate::window_link::moves::MoveId(id),
            why: "the session stayed".into(),
        }));
        r.advance();
        assert_eq!(
            r.ops().iter().any(|op| matches!(op, HostOp::CloseWindow { .. })),
            ix == 1
        );
    }
}

#[test]
fn m_waits_for_a_turn_that_cannot_move_live_and_a_dead_link_still_closes() {
    let (r, port) = moving_close();
    r.host.set_base_tabs(
        vec![ChatMoveFacts {
            tab: crate::tabs::TabId(1),
            session: Some("wait".into()),
            running: true,
            live: false,
        }],
        Some(crate::tabs::TabId(1)),
    );
    r.answer("m");
    r.advance();
    assert!(!port
        .sent()
        .iter()
        .any(|s| matches!(s.msg, crate::window_link::Msg::MoveOffer { .. })));
    assert!(!r.ops().iter().any(|op| matches!(op, HostOp::CloseWindow { .. })));
    r.driver.post(HostEvent::Peer(PeerEvent::LinkClosed {
        peer: super::super::PeerId(2),
    }));
    r.advance();
    assert!(r.ops().iter().any(|op| matches!(op, HostOp::CloseWindow { .. })));
}

#[test]
fn an_end_signal_while_m_waits_for_a_turn_tears_down_at_once() {
    let (r, _) = moving_close();
    r.host.set_base_tabs(
        vec![ChatMoveFacts {
            tab: crate::tabs::TabId(1),
            session: Some("wait".into()),
            running: true,
            live: false,
        }],
        Some(crate::tabs::TabId(1)),
    );
    r.answer("m");
    r.advance();
    assert!(!r.ops().iter().any(|op| matches!(op, HostOp::CloseWindow { .. })));
    r.driver.post(HostEvent::EndSignal { name: "SIGTERM" });
    let ops = r.ops();
    let cleanup = ops
        .iter()
        .position(|op| matches!(op, HostOp::Cleanup(CleanupOp::PairOnClose { .. })))
        .expect("the signal's teardown says goodbye to the other window");
    let close = ops
        .iter()
        .position(|op| matches!(op, HostOp::CloseWindow { .. }))
        .expect("a signal never waits for the moves");
    assert!(cleanup < close);
}

#[test]
fn an_unlinked_close_question_counts_no_pages_it_cannot_move() {
    let ask = |pages: bool| {
        let r = driven(two());
        r.running();
        if pages {
            r.host.set_pages_with_tasks(vec![ModuleId::lua("notes")]);
        }
        r.driver.close_requested(WIN, CloseBy::User);
        r.driver.query(|c| c.chrome(WIN).prompt.map(|p| p.text))
    };
    let without = ask(false);
    assert!(without.is_some(), "a running turn asks");
    assert_eq!(ask(true), without);
}

/// Window 1 tells window 2 it is closing before anything of it ends, whether its close was answered with `y` or
/// had nothing to ask: the pair's goodbye comes first in the teardown, ahead of the editor and the window.
#[test]
fn a_clean_close_of_window_1_says_bye_through_its_teardown() {
    let in_order = |ops: &[HostOp]| {
        let at = |want: &dyn Fn(&HostOp) -> bool| {
            let found: Vec<usize> = ops
                .iter()
                .enumerate()
                .filter(|(_, op)| want(op))
                .map(|(i, _)| i)
                .collect();
            assert_eq!(found.len(), 1, "one of each in {ops:?}");
            found[0]
        };
        let bye = at(&|op| matches!(op, HostOp::Cleanup(CleanupOp::PairOnClose { .. })));
        let editor = at(&|op| matches!(op, HostOp::Editor(EditorCall::Shutdown { .. })));
        let window = at(&|op| matches!(op, HostOp::CloseWindow { .. }));
        assert!(bye < editor && editor < window, "{ops:?}");
    };

    let quiet = driven(two());
    let at = quiet.ops().len();
    quiet.driver.close_requested(WIN, CloseBy::User);
    quiet.advance();
    in_order(&quiet.ops()[at..]);

    let asked = driven(two());
    asked.running();
    asked.host.set_editor_running(true);
    let at = asked.ops().len();
    asked.driver.close_requested(WIN, CloseBy::User);
    assert!(
        asked.driver.query(|c| c.chrome(WIN).prompt.is_some()),
        "a running turn is asked about"
    );
    asked.answer("y");
    asked.exited(Some(NvimAnswer {
        sent_at: asked.scheduler.now(),
        done: true,
        returned: false,
        last: None,
    }));
    asked.advance();
    in_order(&asked.ops()[at..]);
}
