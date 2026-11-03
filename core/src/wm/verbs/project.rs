//! The window's request to choose another project.

use super::modules::ModuleSetup;
use crate::layout::ModuleId;
use crate::project_chooser::{Chooser, Effect, Ev, LineEv, Mode, Row};
use crate::wm::{
    chrome,
    focus::{arrive_tail, give_keys},
    host::{Batch, FocusTo, HostOp, HostReads, ProjectEvent, ProjectOp},
    window::{WinId, Window},
};
use std::path::PathBuf;

pub struct ProjectChooserState {
    pub chooser: Chooser,
    pub held_module: Option<ModuleId>,
}

/// Open a chooser without changing the layout or its selection.
pub fn run(w: &mut Window, reads: &dyn HostReads, win: WinId, setup: &ModuleSetup, batch: &mut Batch) {
    if w.project_chooser.is_some() || w.onboarding.is_some() {
        return;
    }
    crate::wm::keys::cancel(w, batch);
    w.project_generation = w
        .project_generation
        .checked_add(1)
        .expect("chooser generation exhausted");
    let env = reads.project_chooser_env(win);
    let mut chooser = Chooser::new(
        Mode::Window {
            root: setup.project_dir.clone(),
        },
        env.home,
        env.lang,
        w.project_generation,
    );
    let effects = chooser.shown();
    let held_module = w
        .focus
        .native_module()
        .cloned()
        .or_else(|| w.layout.try_borrow().ok().map(|layout| layout.focus().clone()));
    w.project_chooser = Some(ProjectChooserState { chooser, held_module });
    w.say("[project] chooser open".into());
    batch.push_layout(HostOp::Focus {
        win,
        to: FocusTo::ProjectChooser,
        on_grab: vec![],
    });
    carry(w, win, effects, batch);
}

/// Feed input or a worker answer to the live chooser and carry its requests.
pub fn event(w: &mut Window, win: WinId, ev: ProjectEvent, batch: &mut Batch) {
    if ev.win() != win {
        return;
    }
    let keys_taken = matches!(
        &ev,
        ProjectEvent::Line {
            ev: LineEv::KeysLost,
            ..
        }
    );
    let ev = match ev {
        ProjectEvent::Rows { serial, rows, .. } => Ev::Rows { serial, rows },
        ProjectEvent::PathAnswer { serial, answer, .. } => Ev::PathAnswer { serial, answer },
        ProjectEvent::Line { ev, .. } => Ev::Line(ev),
        ProjectEvent::Listing {
            serial, dir, listing, ..
        } => Ev::Listing { serial, dir, listing },
        ProjectEvent::Later { serial, git, links, .. } => Ev::Later { serial, git, links },
        ProjectEvent::Slow { serial, .. } => Ev::Slow { serial },
        ProjectEvent::Stuck { serial, .. } => Ev::Stuck { serial },
        ProjectEvent::Freed { mount, .. } => Ev::Freed { mount },
        ProjectEvent::Pick { index, rows_rev, .. } => Ev::Pick { index, rows_rev },
        ProjectEvent::PickTwice { index, rows_rev, .. } => Ev::PickTwice { index, rows_rev },
        ProjectEvent::Dismiss { .. } => Ev::Dismiss,
        ProjectEvent::Folder { path, .. } => Ev::Folder(path),
        ProjectEvent::OpenFailed { path, code, gone, .. } => {
            let failure = (path.clone(), code);
            if w.project_failure.as_ref() == Some(&failure) {
                return;
            }
            w.project_failure = Some(failure);
            w.say(crate::project_chooser::start_failed_line(code));
            let row = Row::new(path, None, String::new(), crate::project_chooser::Fact::Unknown);
            // Toasts are English whatever the chooser's language.
            let text = crate::project_chooser::start_failed_text(crate::onboarding::Lang::En, &row, gone);
            w.refuse(win, text, batch);
            return;
        }
    };
    let Some(state) = w.project_chooser.as_mut() else {
        return;
    };
    let before = matches!(ev, Ev::Rows { .. }).then(|| state.chooser.view());
    let effects = state.chooser.event(ev);
    // The keys are already with the module that took them; closing must not hand them back to the old holder.
    // A dialog ignores the event, and then the chooser stays and still owns the return.
    if keys_taken && effects.iter().any(|effect| matches!(effect, Effect::Close)) {
        state.held_module = None;
    }
    if before.is_some_and(|before| before != state.chooser.view()) {
        let count = state.chooser.projects();
        w.say(format!("[project] rows {count}"));
    }
    carry(w, win, effects, batch);
}

/// Close the chooser before handing a target to the host.
pub fn open(w: &mut Window, win: WinId, target: PathBuf, batch: &mut Batch) {
    close(w, win, batch);
    w.project_failure = None;
    w.say("[project] open".into());
    batch.push_effect(HostOp::Project(ProjectOp::Open { win, target }));
}

/// Losing activation dismisses the chooser unless a native folder dialog owns input.
pub fn dismiss(w: &mut Window, win: WinId, batch: &mut Batch) {
    event(w, win, ProjectEvent::Dismiss { win }, batch);
}

fn carry(w: &mut Window, win: WinId, effects: Vec<Effect>, batch: &mut Batch) {
    for effect in effects {
        match effect {
            Effect::List { serial } => batch.push_effect(HostOp::Project(ProjectOp::List { win, serial })),
            Effect::Path { serial, text, base } => {
                batch.push_effect(HostOp::Project(ProjectOp::Path {
                    win,
                    serial,
                    text,
                    base,
                }));
            }
            Effect::Read {
                serial,
                dir,
                filter,
                debounce,
                near,
            } => {
                batch.push_effect(HostOp::Project(ProjectOp::Read {
                    win,
                    serial,
                    dir,
                    filter,
                    debounce,
                    near,
                }));
            }
            Effect::FolderDialog { at } => batch.push_effect(HostOp::Project(ProjectOp::FolderDialog { win, at })),
            // Completion already changed the model's line; the chrome carries it to the entry.
            Effect::SetLine(_) => {}
            Effect::Open(target) => open(w, win, target, batch),
            Effect::Close => close(w, win, batch),
        }
    }
    sync_view(w);
}

fn sync_view(w: &mut Window) {
    let view = w.project_chooser.as_ref().map(|state| state.chooser.view());
    if w.chrome.project_chooser != view {
        w.chrome.project_chooser = view;
        chrome::touched(w);
    }
}

fn close(w: &mut Window, win: WinId, batch: &mut Batch) {
    let Some(state) = w.project_chooser.take() else {
        return;
    };
    if let Some(held) = state.held_module {
        let target = w.layout.try_borrow().ok().map(|layout| {
            if layout.can_focus(&held).is_ok() {
                held
            } else {
                layout.focus().clone()
            }
        });
        if let Some(target) = target {
            give_keys(w, win, &target, arrive_tail(win, &target, false), batch);
        } else {
            w.warn("[project] BUG: the layout was borrowed when the chooser returned the keys".into());
        }
    }
    w.say("[project] closed".into());
    sync_view(w);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keymap::prefix::{Press, Waiting};
    use crate::keymap::{Action, KeySpec, Keymap};
    use crate::layout::{Axis, Layout, ModuleId, ModuleKeys, Node};
    use crate::project_chooser::{Fact, LineEv, PathAnswer, Row};
    use crate::wm::chrome::Prompt;
    use crate::wm::conformance::{self, RecordingSink};
    use crate::wm::host::*;
    use crate::wm::verbs::rig::{self, Rig, WIN};
    use crate::wm::window::Tiling;
    use std::path::PathBuf;
    use std::rc::Rc;

    fn rig() -> Rig {
        let layout = Layout::new(
            Node::split(
                Axis::Row,
                0.5,
                Node::Leaf(ModuleId::editor()),
                Node::Leaf(ModuleId::agent()),
            ),
            ModuleId::editor(),
        )
        .unwrap();
        let mut r = rig::rig(layout);
        let keymap = Keymap::defaults();
        let modules = ModuleKeys::build(&[], &keymap).unwrap();
        r.controller.set_keymap(WIN, Rc::new(keymap), Rc::new(modules));
        r.controller.set_module_setup(
            WIN,
            ModuleSetup {
                project_dir: "/work/current".into(),
                ..ModuleSetup::default()
            },
        );
        r
    }

    fn show(r: &mut Rig) -> (u64, Batch) {
        let batch = r.run(Action::ProjectChoose);
        let serial = batch
            .ops()
            .find_map(|op| match op {
                HostOp::Project(ProjectOp::List { win: WIN, serial }) => Some(*serial),
                _ => None,
            })
            .expect("opening asks for the recent projects");
        (serial, batch)
    }

    fn post(r: &mut Rig, event: ProjectEvent) -> Batch {
        r.controller.handle(HostEvent::Project(event))
    }

    fn line(r: &mut Rig, ev: LineEv) -> Batch {
        post(r, ProjectEvent::Line { win: WIN, ev })
    }

    fn key(text: &str) -> KeyPress {
        let spec = (text != "Esc").then(|| KeySpec::parse(text).unwrap());
        KeyPress {
            keycode: 42,
            press: spec.map(Press::Key).unwrap_or(Press::Escape),
            key: spec.map(|spec| spec.key),
            mods: Mods {
                ctrl: spec.is_some_and(|spec| spec.ctrl),
                ..Mods::default()
            },
            super_or_hyper: false,
            super_or_meta: false,
        }
    }

    fn row(path: &str) -> Row {
        Row::new(path.into(), None, "5 m".into(), Fact::Present)
    }

    fn view(r: &Rig) -> crate::project_chooser::ChooserView {
        r.controller.chrome(WIN).project_chooser.expect("the chooser is shown")
    }

    #[test]
    fn it_opens_with_a_list_op_in_each_kind_of_window() {
        for (tiling, module) in [
            (Tiling::Eitri, ModuleId::editor()),
            (Tiling::Hub, ModuleId::editor()),
            (Tiling::ModuleWindow, ModuleId::agent()),
            (Tiling::ModuleWindow, ModuleId::lua("page")),
        ] {
            let mut r = rig();
            if tiling == Tiling::ModuleWindow {
                *r.layout.borrow_mut() = Layout::new(Node::Leaf(module.clone()), module.clone()).unwrap();
            }
            r.controller.window_mut(WIN).unwrap().tiling = tiling;
            let (_, batch) = show(&mut r);
            assert!(batch.ops().any(|op| matches!(
                op,
                HostOp::Focus {
                    to: FocusTo::ProjectChooser,
                    ..
                }
            )));
            assert_eq!(view(&r).title, "Open a project");
            assert_eq!(r.layout.borrow().focus(), &module);
        }
    }

    #[test]
    fn keys_pass_through_while_open() {
        let mut r = rig();
        show(&mut r);
        for text in ["C-j", "C-b", "O", "Esc"] {
            for stage in [KeyStage::Window, KeyStage::Module(&ModuleId::editor())] {
                let (verdict, batch) = r.controller.key(WIN, &stage, &key(text));
                assert_eq!(verdict, Verdict::PassThrough, "{text} reaches the entry");
                assert!(batch.is_empty());
                assert_eq!(r.controller.prefix_shown(WIN).unwrap().0, Waiting::No);
            }
        }
        assert!(r.controller.chrome(WIN).project_chooser.is_some());
    }

    #[test]
    fn a_pointer_press_does_not_close_it() {
        let mut r = rig();
        show(&mut r);
        r.controller.handle(HostEvent::Pointer {
            win: WIN,
            on: PointerTarget::Window,
            ev: PointerEv::Press {
                x: 1.0,
                y: 1.0,
                button: 1,
            },
        });
        assert!(r.controller.chrome(WIN).project_chooser.is_some());
    }

    #[test]
    fn losing_activation_closes_it_but_not_with_a_dialog_open() {
        let mut r = rig();
        show(&mut r);
        r.controller.handle(HostEvent::Active {
            win: WIN,
            active: false,
        });
        assert!(r.controller.chrome(WIN).project_chooser.is_none());
        show(&mut r);
        let batch = line(&mut r, LineEv::ChooseFolder);
        assert!(batch
            .ops()
            .any(|op| matches!(op, HostOp::Project(ProjectOp::FolderDialog { win: WIN, at: None }))));
        r.controller.handle(HostEvent::Active {
            win: WIN,
            active: false,
        });
        assert!(r.controller.chrome(WIN).project_chooser.is_some());
        post(&mut r, ProjectEvent::Folder { win: WIN, path: None });
        r.controller.handle(HostEvent::Active {
            win: WIN,
            active: false,
        });
        assert!(r.controller.chrome(WIN).project_chooser.is_none());
    }

    #[test]
    fn a_prompt_answers_first() {
        let mut r = rig();
        show(&mut r);
        r.controller.window_mut(WIN).unwrap().chrome.prompt = Some(Prompt {
            text: "Close?".into(),
            ..Prompt::default()
        });
        let (verdict, _) = r.controller.key(WIN, &KeyStage::Window, &key("n"));
        assert_eq!(verdict, Verdict::Consumed);
        assert!(r.controller.chrome(WIN).prompt.is_none());
        assert!(r.controller.chrome(WIN).project_chooser.is_some());
    }

    #[test]
    fn esc_leaves_the_selection_and_gives_the_keys_back() {
        let mut r = rig();
        r.run(Action::FocusParent);
        let selection = r.layout.borrow().selection();
        let focus = r.layout.borrow().focus().clone();
        for module in r.layout.borrow().modules() {
            let surface = rig::decl(&module);
            r.host.with_surface(WIN, surface.module, surface.kind, false);
        }
        let driver = crate::wm::Driver::new(r.controller, r.host.clone(), r.scheduler.clone());
        let erased: Rc<dyn HostSink> = driver.clone();
        r.host.attach(Rc::downgrade(&erased));
        for op in r.first.ops() {
            r.host.apply(op);
        }
        driver.run(WIN, Action::ProjectChoose, Origin::Lua);
        assert!(driver.query(|controller| controller.chrome(WIN).project_chooser.is_some()));
        let mark = r.host.log().len();
        driver.post(HostEvent::Project(ProjectEvent::Line {
            win: WIN,
            ev: LineEv::Escape,
        }));
        assert!(driver.query(|controller| controller.chrome(WIN).project_chooser.is_none()));
        assert_eq!(r.layout.borrow().selection(), selection);
        assert_eq!(r.layout.borrow().focus(), &focus);
        assert_eq!(r.host.focus_owner(WIN), Some(focus.clone()));
        assert!(r.host.log()[mark..].iter().any(|logged| matches!(logged,
            crate::wm::fake::Logged::Op(HostOp::Focus { to: FocusTo::Module(module), .. }, Applied::Done) if module == &focus)));
    }

    #[test]
    fn a_second_run_changes_nothing() {
        let mut r = rig();
        show(&mut r);
        line(&mut r, LineEv::Changed("typed".into()));
        let before = view(&r);
        assert!(r.run(Action::ProjectChoose).is_empty());
        assert_eq!(view(&r), before);
    }

    #[test]
    fn the_first_start_screen_holding_the_keys_stops_it() {
        use crate::onboarding::{Build, Lang, OpenMode, Opening};
        let mut r = rig();
        r.controller.handle(HostEvent::Onboarding(OnboardingEvent::Open {
            win: WIN,
            opening: Opening {
                generation: 0,
                mode: OpenMode::First,
                show_why: None,
                trust_open: false,
                lang: Lang::En,
                build: Build::THIS_BINARY,
                running_prefix: KeySpec::parse("C-b").unwrap(),
                keymap_check: vec![],
                today: "2026-11-03".into(),
                config_path: "/scratch/config".into(),
            },
        }));
        assert!(r.controller.chrome(WIN).onboarding.is_some());
        assert!(r.run(Action::ProjectChoose).is_empty());
        assert!(r.controller.chrome(WIN).project_chooser.is_none());
    }

    #[test]
    fn refused_closes_it_and_toasts() {
        let mut r = rig();
        show(&mut r);
        line(&mut r, LineEv::ChooseFolder);
        r.controller.handle(HostEvent::Refused {
            what: OpTag::Project,
            why: "unavailable here".into(),
            surface: None,
        });
        assert!(r.controller.chrome(WIN).project_chooser.is_none());
        assert_eq!(r.said().toast.unwrap().1, "unavailable here");
    }

    #[test]
    fn the_rows_line_counts_the_projects_listed_not_the_window_s_own() {
        let mut r = rig();
        let (serial, _) = show(&mut r);
        post(
            &mut r,
            ProjectEvent::Rows {
                win: WIN,
                serial,
                rows: vec![row("/work/current"), row("/work/beta"), row("/work/gamma")],
            },
        );
        let names: Vec<_> = view(&r).rows.iter().map(|row| row.name.clone()).collect();
        assert_eq!(names[..2], ["beta", "gamma"]);
        let lines: Vec<_> = r
            .controller
            .notes(WIN)
            .into_iter()
            .filter(|note| note.starts_with("[project] rows"))
            .collect();
        assert_eq!(lines, ["[project] rows 2"]);
    }

    #[test]
    fn a_stale_serial_is_dropped() {
        let mut r = rig();
        let (serial, _) = show(&mut r);
        let before = view(&r);
        post(
            &mut r,
            ProjectEvent::Rows {
                win: WIN,
                serial: serial + 1,
                rows: vec![row("/wrong")],
            },
        );
        assert_eq!(view(&r), before);
        post(
            &mut r,
            ProjectEvent::Rows {
                win: WIN,
                serial,
                rows: vec![row("/right")],
            },
        );
        assert_eq!(view(&r).rows[0].name, "right");
        let batch = line(&mut r, LineEv::Changed("./a/x".into()));
        let serial = batch
            .ops()
            .find_map(|op| match op {
                HostOp::Project(ProjectOp::Read { serial, dir, .. }) => {
                    assert_eq!(dir, &PathBuf::from("/work/current/a"));
                    Some(*serial)
                }
                _ => None,
            })
            .unwrap();
        line(&mut r, LineEv::Changed("./b/x".into()));
        let before = view(&r);
        post(
            &mut r,
            ProjectEvent::Listing {
                win: WIN,
                serial,
                dir: "/work/current/a".into(),
                listing: crate::project_chooser::Listing::Missing,
            },
        );
        assert_eq!(view(&r), before);
    }

    #[test]
    fn a_reply_from_before_a_reopen_is_dropped() {
        let mut r = rig();
        let (old, _) = show(&mut r);
        line(&mut r, LineEv::Escape);
        let (new, _) = show(&mut r);
        assert_ne!(old, new);
        let before = view(&r);
        post(
            &mut r,
            ProjectEvent::Rows {
                win: WIN,
                serial: old,
                rows: vec![row("/old")],
            },
        );
        assert_eq!(view(&r), before);
        post(
            &mut r,
            ProjectEvent::Rows {
                win: WIN,
                serial: new,
                rows: vec![row("/new")],
            },
        );
        assert_eq!(view(&r).rows[0].name, "new");
    }

    #[test]
    fn open_closes_and_pushes_the_op() {
        let mut r = rig();
        let (serial, _) = show(&mut r);
        let target = "/work/名字 <b>\nproject";
        post(
            &mut r,
            ProjectEvent::Rows {
                win: WIN,
                serial,
                rows: vec![row(target)],
            },
        );
        let rows_rev = view(&r).rows_rev;
        let batch = post(
            &mut r,
            ProjectEvent::Pick {
                win: WIN,
                index: 0,
                rows_rev,
            },
        );
        assert!(r.controller.chrome(WIN).project_chooser.is_none());
        let ops = batch.ops().collect::<Vec<_>>();
        let focus = ops
            .iter()
            .position(|op| {
                matches!(
                    op,
                    HostOp::Focus {
                        to: FocusTo::Module(_),
                        ..
                    }
                )
            })
            .unwrap();
        let open = ops.iter().position(|op| matches!(op, HostOp::Project(ProjectOp::Open { target: path, .. }) if path == &PathBuf::from(target))).unwrap();
        assert!(focus < open);
        assert_eq!(r.layout.borrow().focus(), &ModuleId::editor());
    }

    #[test]
    fn open_failed_toasts_once_and_names_the_folder() {
        let mut r = rig();
        let (serial, _) = show(&mut r);
        post(
            &mut r,
            ProjectEvent::Rows {
                win: WIN,
                serial,
                rows: vec![row("/work/app\nname")],
            },
        );
        line(&mut r, LineEv::Enter);
        let failure = ProjectEvent::OpenFailed {
            win: WIN,
            path: "/work/app\nname".into(),
            code: Some(7),
            gone: false,
        };
        post(&mut r, failure.clone());
        let toast = r.said().toast.unwrap();
        assert_eq!(toast.1, "could not open app\\u{a}name: see its window log");
        post(&mut r, failure);
        assert_eq!(r.said().toast, Some(toast));
        assert_eq!(
            r.controller
                .notes(WIN)
                .iter()
                .filter(|line| *line == "[project] start failed 7")
                .count(),
            1
        );
    }

    #[test]
    fn a_start_whose_folder_went_says_so_and_one_with_no_code_logs_unknown() {
        let mut r = rig();
        post(
            &mut r,
            ProjectEvent::OpenFailed {
                win: WIN,
                path: "/work/away".into(),
                code: None,
                gone: true,
            },
        );
        assert_eq!(r.said().toast.unwrap().1, "/work/away is not there");
        assert!(r
            .controller
            .notes(WIN)
            .iter()
            .any(|line| line == "[project] start failed unknown"));
    }

    #[test]
    fn another_module_taking_the_keys_closes_it() {
        let mut r = rig();
        show(&mut r);
        let taken = r.controller.handle(HostEvent::NativeFocus {
            win: WIN,
            owner: Some(FocusOwner::Module(ModuleId::agent())),
            chain: FocusChain(vec![ModuleId::agent()]),
            active: true,
            op_index: None,
        });
        assert!(taken.ops().all(|op| !matches!(op, HostOp::Focus { .. })));
        let batch = line(&mut r, LineEv::KeysLost);
        assert!(r.controller.chrome(WIN).project_chooser.is_none());
        assert!(
            batch.ops().all(|op| !matches!(op, HostOp::Focus { .. })),
            "the keys stay with the module that took them: {:?}",
            batch.ops().collect::<Vec<_>>()
        );
        assert_eq!(r.layout.borrow().focus(), &ModuleId::agent());
    }

    #[test]
    fn project_list_focus_and_close() {
        let r = rig();
        let surface = rig::decl(&ModuleId::editor());
        r.host.with_surface(WIN, surface.module, surface.kind, true);
        let rows = vec![row("/work/other")];
        r.host.set_project_rows(rows.clone());
        let sink = Rc::new(RecordingSink::default());
        let erased: Rc<dyn HostSink> = sink.clone();
        r.host.attach(Rc::downgrade(&erased));
        for script in conformance::project_list_focus_and_close(WIN, rows) {
            conformance::run(r.host.as_ref(), &sink, &script).unwrap();
        }
    }

    #[test]
    fn opening_ends_a_prefix_wait() {
        let mut r = rig();
        assert_eq!(
            r.controller.key(WIN, &KeyStage::Window, &key("C-b")).0,
            Verdict::Consumed
        );
        assert_ne!(r.controller.prefix_shown(WIN).unwrap().0, Waiting::No);
        show(&mut r);
        assert_eq!(r.controller.prefix_shown(WIN).unwrap().0, Waiting::No);
        line(&mut r, LineEv::Escape);
        assert_eq!(
            r.controller.key(WIN, &KeyStage::Window, &key("O")).0,
            Verdict::PassThrough
        );
    }

    #[test]
    fn the_cached_home_and_language_reach_the_model() {
        let mut r = rig();
        r.host.set_project_chooser_env(ChooserEnv {
            home: Some("/home/u".into()),
            lang: crate::onboarding::Lang::ZhCn,
        });
        show(&mut r);
        assert_eq!(view(&r).title, "打开项目");
        let batch = line(&mut r, LineEv::Changed("~/o".into()));
        let serial = batch
            .ops()
            .find_map(|op| match op {
                HostOp::Project(ProjectOp::Read { serial, dir, .. }) => {
                    assert_eq!(dir, &PathBuf::from("/home/u"));
                    Some(*serial)
                }
                _ => None,
            })
            .unwrap();
        post(
            &mut r,
            ProjectEvent::Listing {
                win: WIN,
                serial,
                dir: "/home/u".into(),
                listing: crate::project_chooser::Listing::Read {
                    dir: "/home/u".into(),
                    entries: vec![crate::project_chooser::Entry {
                        raw: "other".into(),
                        name: "other".into(),
                        kind: crate::project_chooser::EntryKind::Dir { mount_point: false },
                        physical: Some("/home/u/other".into()),
                    }],
                    capped: None,
                    exact: None,
                },
            },
        );
        assert_eq!(view(&r).rows[0].name, "other/");
        line(&mut r, LineEv::Tab);
        assert_eq!(view(&r).line, "~/other/");
    }

    #[test]
    fn project_events_before_the_layout_log_no_payload() {
        let mut controller = crate::wm::Controller::new(crate::wm::fake::FakeScheduler::new());
        controller.declare_window(
            WIN,
            crate::wm::WindowSpec {
                role: crate::wm::WindowRole::Main,
                presence: crate::wm::EditorPresence::Owned,
                title: "project".into(),
            },
        );
        let path = PathBuf::from("/private/project");
        let events = [
            ProjectEvent::Rows {
                win: WIN,
                serial: 1,
                rows: vec![row("/private/project")],
            },
            ProjectEvent::PathAnswer {
                win: WIN,
                serial: 2,
                answer: PathAnswer::Missing,
            },
            ProjectEvent::Line {
                win: WIN,
                ev: LineEv::Changed("/private/project".into()),
            },
            ProjectEvent::Pick {
                win: WIN,
                index: 0,
                rows_rev: 0,
            },
            ProjectEvent::Dismiss { win: WIN },
            ProjectEvent::Folder {
                win: WIN,
                path: Some(path.clone()),
            },
            ProjectEvent::OpenFailed {
                win: WIN,
                path,
                code: None,
                gone: true,
            },
        ];
        for event in events {
            let event = HostEvent::Project(event);
            assert_eq!(crate::wm::start::event_window(&event), Some(WIN));
            assert!(controller.handle(event).is_empty());
        }
        assert_eq!(controller.start_notes().len(), 7);
        assert!(controller.start_notes().iter().all(|note| !note.contains("/private")));
    }

    use crate::project_chooser::{LinkKind, Listing};

    #[test]
    fn a_read_effect_becomes_a_read_op_with_debounce_and_near() {
        let mut r = rig();
        show(&mut r);
        let mut batch = Batch::default();
        carry(
            r.controller.window_mut(WIN).unwrap(),
            WIN,
            vec![Effect::Read {
                serial: 19,
                dir: "/walked".into(),
                filter: Some("ab".into()),
                debounce: true,
                near: Some("abc".into()),
            }],
            &mut batch,
        );
        assert!(batch.ops().any(|op| *op
            == HostOp::Project(ProjectOp::Read {
                win: WIN,
                serial: 19,
                dir: "/walked".into(),
                filter: Some("ab".into()),
                debounce: true,
                near: Some("abc".into()),
            })));
    }

    #[test]
    fn listing_later_slow_stuck_and_freed_reach_the_model() {
        let mut r = rig();
        show(&mut r);
        let before = view(&r);
        for ev in [
            ProjectEvent::Listing {
                win: WIN,
                serial: 19,
                dir: "/walked".into(),
                listing: Listing::Stuck,
            },
            ProjectEvent::Later {
                win: WIN,
                serial: 19,
                git: vec!["abc".into()],
                links: vec![("link".into(), LinkKind::Dir)],
            },
            ProjectEvent::Slow { win: WIN, serial: 19 },
            ProjectEvent::Stuck { win: WIN, serial: 19 },
            ProjectEvent::Freed {
                win: WIN,
                mount: "/physical".into(),
            },
        ] {
            assert_eq!(ev.win(), WIN);
            let batch = post(&mut r, ev);
            assert!(!batch.ops().any(|op| matches!(op, HostOp::Project(_))));
            assert_eq!(view(&r), before);
        }
    }

    #[test]
    fn pick_and_pick_twice_carry_rows_rev_to_the_model() {
        let mut r = rig();
        let (serial, _) = show(&mut r);
        let old = view(&r).rows_rev;
        post(
            &mut r,
            ProjectEvent::Rows {
                win: WIN,
                serial,
                rows: vec![row("/app")],
            },
        );
        let current = view(&r).rows_rev;
        for ev in [
            ProjectEvent::Pick {
                win: WIN,
                index: 0,
                rows_rev: old,
            },
            ProjectEvent::PickTwice {
                win: WIN,
                index: 0,
                rows_rev: old,
            },
            ProjectEvent::PickTwice {
                win: WIN,
                index: 0,
                rows_rev: current,
            },
        ] {
            post(&mut r, ev);
            assert!(r.controller.chrome(WIN).project_chooser.is_some());
        }
        let batch = post(
            &mut r,
            ProjectEvent::Pick {
                win: WIN,
                index: 0,
                rows_rev: current,
            },
        );
        assert!(batch.ops().any(
            |op| matches!(op, HostOp::Project(ProjectOp::Open { target, .. }) if target == std::path::Path::new("/app"))
        ));
    }
}
