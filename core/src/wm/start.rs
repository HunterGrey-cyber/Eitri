//! The ordered startup phases and the data each phase hands to the controller.

use super::host::*;
use super::module::SurfaceDecl;
use super::window::{ActivationToken, EditorPresence, WinId, WindowParts, WindowSpec};
use super::Controller;
use crate::keymap::Keymap;
use crate::layout::{ModuleId, ModuleKeys};
use crate::tabs::ViewId;
use std::rc::Rc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartPhase {
    Feeds,
    Window,
    Editor,
    Kernel,
    Config,
    Layout,
    Surfaces,
    Chrome,
    Present,
    Requests,
}

pub fn start_plan() -> &'static [StartPhase] {
    &[
        StartPhase::Feeds,
        StartPhase::Window,
        StartPhase::Editor,
        StartPhase::Kernel,
        StartPhase::Config,
        StartPhase::Layout,
        StartPhase::Surfaces,
        StartPhase::Chrome,
        StartPhase::Present,
        StartPhase::Requests,
    ]
}

/// Both sides are canonical paths supplied by project selection and plugin loading. Comparing them here
/// performs no filesystem work on the window's thread; a child directory is a different project.
pub fn own_window(
    project_root: &std::path::Path,
    plugins: &[(String, std::path::PathBuf)],
    role: crate::window_link::pair::Role,
) -> Option<String> {
    if role == crate::window_link::pair::Role::Second {
        return None;
    }
    plugins
        .iter()
        .find(|(_, root)| root == project_root)
        .map(|(id, _)| id.clone())
}

pub enum StartStep {
    Feeds {
        spawn: EditorSpawn,
    },
    Window {
        spec: WindowSpec,
    },
    Editor,
    Kernel,
    /// The window's config is read: the keymap and the module keys it built.
    Config {
        keymap: Rc<Keymap>,
        module_keys: Rc<ModuleKeys>,
    },
    Layout {
        parts: WindowParts,
        own_window: Option<String>,
    },
    Surfaces {
        decls: Vec<SurfaceDecl>,
    },
    Chrome,
    Present {
        activation: Option<ActivationToken>,
        first: ModuleId,
        arrive: Option<ViewId>,
    },
    Requests,
}

impl StartStep {
    fn phase(&self) -> StartPhase {
        match self {
            Self::Feeds { .. } => StartPhase::Feeds,
            Self::Window { .. } => StartPhase::Window,
            Self::Editor => StartPhase::Editor,
            Self::Kernel => StartPhase::Kernel,
            Self::Config { .. } => StartPhase::Config,
            Self::Layout { .. } => StartPhase::Layout,
            Self::Surfaces { .. } => StartPhase::Surfaces,
            Self::Chrome => StartPhase::Chrome,
            Self::Present { .. } => StartPhase::Present,
            Self::Requests => StartPhase::Requests,
        }
    }
}

#[derive(Default)]
pub(super) struct StartState {
    next: usize,
    pub(super) own_window: Option<String>,
    pub(super) spawn: Option<EditorSpawn>,
}

impl Controller {
    /// Walks one window's startup in order, retaining the feed settings until the editor is made.
    pub fn start(&mut self, win: WinId, step: StartStep) -> Batch {
        let state = self.starts.entry(win).or_default();
        if start_plan().get(state.next) != Some(&step.phase()) {
            self.start_note("[wm] BUG: start step out of order".into());
            return Batch::default();
        }
        state.next += 1;
        let mut batch = Batch::default();
        match step {
            StartStep::Feeds { spawn } => {
                state.spawn = Some(spawn.clone());
                self.keep_editor_spawn(win, spawn);
            }
            StartStep::Window { spec } => {
                self.declare_window(win, spec.clone());
                batch.push_layout(HostOp::OpenWindow { win, spec });
            }
            StartStep::Editor => {
                let spawn = state.spawn.clone();
                if self
                    .window_spec(win)
                    .is_some_and(|spec| spec.presence == EditorPresence::Owned)
                {
                    // Reaching this phase requires the feeds phase to have supplied the launch settings.
                    let spawn = spawn.expect("ordered startup retains the editor spawn");
                    batch.push_layout(HostOp::CreateSurface {
                        win,
                        module: ModuleId::editor(),
                        kind: SurfaceKind::Editor { spawn },
                    });
                }
            }
            StartStep::Kernel => batch.push_lua(LuaOp::LoadInit),
            StartStep::Config { keymap, module_keys } => self.set_keymap(win, keymap, module_keys),
            StartStep::Chrome => {}
            StartStep::Layout { parts, own_window } => {
                state.own_window = own_window;
                self.attach_parts(win, parts);
                self.show_editor_for_targets(win);
            }
            StartStep::Surfaces { decls } => return self.create_surfaces(win, &decls),
            StartStep::Present {
                activation,
                first,
                arrive,
            } => {
                batch.push_layout(HostOp::Present { win, activation });
                batch.push_layout(HostOp::Focus {
                    win,
                    to: FocusTo::Module(first),
                    on_grab: arrive
                        .into_iter()
                        .map(|view| {
                            HostOp::Chat(ChatOp::Arrive {
                                win,
                                view,
                                card_first: true,
                            })
                        })
                        .collect(),
                });
            }
            StartStep::Requests => {
                batch.push_lua(LuaOp::Emit {
                    event: "shell:ready".into(),
                });
                // Window 2 starts once this window's own start has finished, `shell:ready` included: on an idle,
                // since a batch carries its Lua last.
                self.arm_second_restore(win);
            }
        }
        batch
    }
}

/// Every window-addressed fact is checked before any layout-dependent handler runs.
pub(super) fn event_window(event: &HostEvent) -> Option<WinId> {
    let win = match event {
        HostEvent::KeyReleased { win, .. }
        | HostEvent::Pointer { win, .. }
        | HostEvent::NativeFocus { win, .. }
        | HostEvent::Active { win, .. }
        | HostEvent::Resized { win, .. }
        | HostEvent::WindowMonitor { win, .. }
        | HostEvent::MinSize { win, .. }
        | HostEvent::Settled { win, .. }
        | HostEvent::Prepared { win, .. }
        | HostEvent::Lua(LuaEvent::LayoutRequests { win, .. })
        | HostEvent::Pair(PairEvent::LinkEnded { win, .. } | PairEvent::RestoreFailed { win })
        | HostEvent::AnswerRefused { win, .. }
        | HostEvent::OpenFile { win, .. }
        | HostEvent::SurfaceEnded { win, .. }
        | HostEvent::TopBarEntry { win, .. }
        | HostEvent::TopBarLeft { win }
        | HostEvent::Refused {
            surface: Some((win, _)),
            ..
        }
        | HostEvent::TextSize(
            TextSizeEvent::Both { win, .. }
            | TextSizeEvent::Pane { win, .. }
            | TextSizeEvent::PageAttached { win, .. }
            | TextSizeEvent::Base { win, .. },
        )
        | HostEvent::Editor(
            EditorEvent::Ready { win }
            | EditorEvent::Released { win }
            | EditorEvent::ReleaseRefused { win }
            | EditorEvent::RestartRefused { win, .. }
            | EditorEvent::Exited { win, .. }
            | EditorEvent::Unreachable { win }
            | EditorEvent::QuitNotSent { win, .. }
            | EditorEvent::ScaleReported { win, .. }
            | EditorEvent::QuitAnswered { win, .. }
            | EditorEvent::StartFailed { win, .. }
            | EditorEvent::Navigate { win, .. }
            | EditorEvent::QuitCancelled { win, .. }
            | EditorEvent::OpenFailed { win, .. },
        )
        | HostEvent::Plugin(
            PluginEvent::Frozen { win, .. } | PluginEvent::FreezeRefused { win, .. } | PluginEvent::Ready { win, .. },
        )
        | HostEvent::Chat(
            ChatEvent::Attention { win, .. }
            | ChatEvent::Busy { win, .. }
            | ChatEvent::TabVerb { win, .. }
            | ChatEvent::ChooserPartnerPick { win, .. }
            | ChatEvent::KeysReturn { win, .. }
            | ChatEvent::Covered { win, .. }
            | ChatEvent::Reveal { win, .. }
            | ChatEvent::PageShown { win, .. }
            | ChatEvent::ShowView { win, .. }
            | ChatEvent::ViewEmpty { win, .. }
            | ChatEvent::NavFallthrough { win, .. }
            | ChatEvent::ReviewOpen { win }
            | ChatEvent::EditorDone { win, .. }
            | ChatEvent::Hint { win, .. }
            | ChatEvent::Toast { win, .. }
            | ChatEvent::KeymapClosed { win, .. }
            | ChatEvent::Frozen { win, .. }
            | ChatEvent::FreezeRefused { win, .. }
            | ChatEvent::PresenceHeld { win, .. }
            | ChatEvent::PresenceRefused { win, .. },
        )
        | HostEvent::Terminal(
            TermEvent::Started { win, .. }
            | TermEvent::Exited { win, .. }
            | TermEvent::Bell { win, .. }
            | TermEvent::Cwd { win, .. }
            | TermEvent::Job { win, .. }
            | TermEvent::Detached { win, .. },
        )
        | HostEvent::Hint(HintEvent::Active { win, .. } | HintEvent::Landed { win, .. }) => win,
        // The editor's surface is made before the window's layout exists; its creation waits for nothing.
        HostEvent::Created { .. }
        | HostEvent::EndSignal { .. }
        | HostEvent::Peer(_)
        | HostEvent::MonitorsChanged { .. }
        | HostEvent::Timer(_)
        | HostEvent::Fd(_)
        | HostEvent::Wake
        | HostEvent::Refused { surface: None, .. } => return None,
    };
    Some(*win)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::{Layout, ModuleDecl, Placement};
    use crate::layout::{ModuleId, Workspaces};
    use crate::tabs::ViewId;
    use crate::wm::fake::{FakeHost, FakeScheduler, Logged};
    use crate::wm::module::{ModuleState, SurfaceDecl};
    use crate::wm::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    #[test]
    fn event_window_covers_the_new_event() {
        let win = WinId(2);
        let open = HostEvent::OpenFile {
            win,
            id: 1,
            path: "/project/a.rs".into(),
            line: None,
            col: None,
            token: None,
        };
        let failed = HostEvent::Editor(EditorEvent::OpenFailed {
            win,
            why: "no answer".into(),
        });
        assert_eq!(event_window(&open), Some(win));
        assert_eq!(event_window(&failed), Some(win));
    }

    #[test]
    fn the_layout_phase_keeps_the_own_window_id() {
        let mut controller = Controller::new(FakeScheduler::new());
        for step in [
            StartStep::Feeds { spawn: spawn() },
            StartStep::Window {
                spec: spec(EditorPresence::Owned),
            },
            StartStep::Editor,
            StartStep::Kernel,
            config(),
            StartStep::Layout {
                parts: parts(),
                own_window: Some("demo".into()),
            },
        ] {
            controller.start(WinId::MAIN, step);
        }
        assert_eq!(
            controller.window_mut(WinId::MAIN).unwrap().own_window.as_deref(),
            Some("demo")
        );
    }

    fn setup() -> (Rc<Driver<Controller>>, Rc<FakeHost>) {
        let (driver, host, _) = setup_with_scheduler();
        (driver, host)
    }

    fn setup_with_scheduler() -> (Rc<Driver<Controller>>, Rc<FakeHost>, Rc<FakeScheduler>) {
        let host = FakeHost::new();
        let scheduler = FakeScheduler::new();
        let mut controller = Controller::new(scheduler.clone());
        controller.set_reads(host.clone());
        let driver = Driver::new(controller, host.clone(), scheduler.clone());
        let sink: Rc<dyn HostSink> = driver.clone();
        host.attach(Rc::downgrade(&sink));
        scheduler.attach(Rc::downgrade(&sink));
        (driver, host, scheduler)
    }

    fn spawn() -> EditorSpawn {
        let feed = agent::socket_path::in_dir(std::path::Path::new("/project"), "feed.sock").unwrap();
        EditorSpawn {
            env: vec![("EDITOR_FEED".into(), feed.to_str().unwrap().into())],
            args: vec!["--embed".into()],
            cwd: Some("/project".into()),
            clean: true,
            background: None,
            open: vec![],
        }
    }

    fn spec(presence: EditorPresence) -> WindowSpec {
        WindowSpec {
            role: WindowRole::Main,
            presence,
            title: "project".into(),
        }
    }

    fn config() -> StartStep {
        let keymap = Rc::new(crate::keymap::Keymap::defaults());
        let module_keys = Rc::new(ModuleKeys::build(&[], &keymap).unwrap());
        StartStep::Config { keymap, module_keys }
    }

    fn parts() -> WindowParts {
        WindowParts {
            layout: Rc::new(RefCell::new(
                Layout::initial(&[ModuleDecl {
                    id: ModuleId::terminal(),
                    placement: Placement::BelowRoot,
                }])
                .unwrap(),
            )),
            workspaces: Rc::new(RefCell::new(Workspaces::single())),
            saver: None,
        }
    }

    fn walk(driver: &Driver<Controller>, host: &FakeHost, first: ModuleId, arrive: Option<ViewId>, mapped: bool) {
        assert_eq!(
            start_plan(),
            &[
                StartPhase::Feeds,
                StartPhase::Window,
                StartPhase::Editor,
                StartPhase::Kernel,
                StartPhase::Config,
                StartPhase::Layout,
                StartPhase::Surfaces,
                StartPhase::Chrome,
                StartPhase::Present,
                StartPhase::Requests,
            ]
        );
        for step in [
            StartStep::Feeds { spawn: spawn() },
            StartStep::Window {
                spec: spec(EditorPresence::Owned),
            },
            StartStep::Editor,
            StartStep::Kernel,
            config(),
            StartStep::Layout {
                parts: parts(),
                own_window: None,
            },
            StartStep::Surfaces {
                decls: vec![SurfaceDecl {
                    module: ModuleId::agent(),
                    kind: SurfaceKind::Chat { view: ViewId::BASE },
                    deferred: false,
                }],
            },
            StartStep::Chrome,
        ] {
            driver.start(WinId::MAIN, step);
        }
        let kind = if first == ModuleId::editor() {
            SurfaceKind::Editor { spawn: spawn() }
        } else {
            SurfaceKind::Chat { view: ViewId::BASE }
        };
        host.with_surface(WinId::MAIN, first.clone(), kind, mapped);
        driver.start(
            WinId::MAIN,
            StartStep::Present {
                activation: Some(ActivationToken("token".into())),
                first,
                arrive,
            },
        );
        driver.start(WinId::MAIN, StartStep::Requests);
    }

    fn ops(host: &FakeHost) -> Vec<HostOp> {
        host.log()
            .into_iter()
            .filter_map(|entry| match entry {
                Logged::Op(op, _) => Some(op),
                _ => None,
            })
            .collect()
    }

    fn targeted() -> EditorSpawn {
        EditorSpawn {
            open: vec!["+3".into(), "/project/src/a b.rs".into()],
            ..spawn()
        }
    }

    fn frame() -> crate::layout::Frame<'static> {
        crate::layout::Frame::new(crate::layout::Size { w: 1200, h: 800 }, 4)
    }

    /// The first-launch layout with the agent holding the keys, then the editor hidden.
    fn editor_hidden() -> Layout {
        let mut layout = Layout::initial(&[ModuleDecl {
            id: ModuleId::terminal(),
            placement: Placement::BelowRoot,
        }])
        .unwrap();
        layout.set_focus(&ModuleId::agent()).unwrap();
        crate::layout::hide(&mut layout, &ModuleId::editor(), &frame()).unwrap();
        layout
    }

    /// A layout whose tree has no editor leaf, as a workspace that never held it.
    fn editor_missing() -> Layout {
        use crate::layout::{Axis, Node};
        Layout::from_parts(
            Node::split(
                Axis::Row,
                0.5,
                Node::Leaf(ModuleId::agent()),
                Node::Leaf(ModuleId::terminal()),
            ),
            Default::default(),
            ModuleId::agent(),
        )
        .unwrap()
    }

    /// Walks a whole start over `layout` (with a real saver, so a save armed by the start shows) and returns the
    /// shared layout.
    fn walk_over(
        driver: &Driver<Controller>,
        host: &FakeHost,
        spawn: EditorSpawn,
        presence: EditorPresence,
        layout: Layout,
    ) -> Rc<RefCell<Layout>> {
        walk_over_saving_in(
            agent::state_dirs::test_workspace_dir("start-open"),
            driver,
            host,
            spawn,
            presence,
            layout,
        )
    }

    /// [`walk_over`] with the saver writing into `dir`.
    fn walk_over_saving_in(
        dir: std::path::PathBuf,
        driver: &Driver<Controller>,
        host: &FakeHost,
        spawn: EditorSpawn,
        presence: EditorPresence,
        layout: Layout,
    ) -> Rc<RefCell<Layout>> {
        let workspaces = Workspaces::single();
        let saver = crate::wm::persist::Saver::new(
            Some(dir),
            std::path::Path::new("/project"),
            vec![],
            None,
            &layout,
            &workspaces,
        );
        let parts = WindowParts {
            layout: Rc::new(RefCell::new(layout)),
            workspaces: Rc::new(RefCell::new(workspaces)),
            saver: Some(saver),
        };
        let shared = parts.layout.clone();
        for step in [
            StartStep::Feeds { spawn },
            StartStep::Window { spec: spec(presence) },
            StartStep::Editor,
            StartStep::Kernel,
            config(),
            StartStep::Layout {
                parts,
                own_window: None,
            },
        ] {
            driver.start(WinId::MAIN, step);
        }
        let decls = shared
            .borrow()
            .modules()
            .iter()
            .map(crate::wm::verbs::rig::decl)
            .collect();
        driver.start(WinId::MAIN, StartStep::Surfaces { decls });
        driver.start(WinId::MAIN, StartStep::Chrome);
        let first = shared.borrow().focus().clone();
        host.with_surface(
            WinId::MAIN,
            first.clone(),
            crate::wm::verbs::rig::decl(&first).kind,
            true,
        );
        driver.start(
            WinId::MAIN,
            StartStep::Present {
                activation: None,
                first,
                arrive: None,
            },
        );
        driver.start(WinId::MAIN, StartStep::Requests);
        shared
    }

    fn open_state(driver: &Driver<Controller>) -> (Vec<String>, bool, bool) {
        driver.query(|c| c.open_state(WinId::MAIN).unwrap())
    }

    #[test]
    fn a_start_with_targets_shows_a_hidden_editor_and_gives_it_the_keys() {
        let (driver, host) = setup();
        let layout = walk_over(&driver, &host, targeted(), EditorPresence::Owned, editor_hidden());
        let editor = ModuleId::editor();
        assert!(layout.borrow().is_shown(&editor));
        assert_eq!(layout.borrow().focus(), &editor);
        assert_eq!(host.focus_owner(WinId::MAIN), Some(editor.clone()));
        assert!(
            ops(&host).iter().any(|op| matches!(
                op,
                HostOp::Focus { to: FocusTo::Module(m), .. } if *m == editor
            )),
            "the first focus goes to the editor"
        );
        assert_eq!(
            open_state(&driver).0,
            targeted().open,
            "the targets wait for the first Ready"
        );
    }

    #[test]
    fn the_editor_is_created_with_the_targets_and_the_window_keeps_the_spawn_without_them() {
        let (driver, host) = setup();
        walk_over(&driver, &host, targeted(), EditorPresence::Owned, editor_hidden());
        assert_eq!(
            ops(&host)
                .iter()
                .find_map(|op| match op {
                    HostOp::CreateSurface {
                        kind: SurfaceKind::Editor { spawn },
                        ..
                    } => Some(spawn.open.clone()),
                    _ => None,
                })
                .unwrap(),
            targeted().open,
            "the first launch carries the targets"
        );
        assert_eq!(driver.query(|c| c.editor_spawn_open(WinId::MAIN)), Some(vec![]));
    }

    #[test]
    fn a_start_with_targets_ends_a_zoom_on_another_module() {
        let (driver, host) = setup();
        let mut layout = editor_hidden();
        layout.show(&ModuleId::editor()).unwrap();
        layout.set_focus(&ModuleId::agent()).unwrap();
        assert_eq!(
            layout.toggle_zoom(&ModuleId::agent()),
            crate::layout::ZoomChange::Zoomed
        );
        let shared = walk_over(&driver, &host, targeted(), EditorPresence::Owned, layout);
        assert!(shared.borrow().zoom().is_none(), "the zoom over the editor is over");
        assert_eq!(shared.borrow().focus(), &ModuleId::editor());
        assert!(shared.borrow().is_shown(&ModuleId::editor()));
    }

    #[test]
    fn a_start_with_targets_places_an_editor_the_workspace_lacks() {
        let (driver, host) = setup();
        let layout = editor_missing();
        assert!(!layout.contains(&ModuleId::editor()));
        let shared = walk_over(&driver, &host, targeted(), EditorPresence::Owned, layout);
        let editor = ModuleId::editor();
        assert!(shared.borrow().contains(&editor));
        assert!(shared.borrow().is_shown(&editor));
        assert_eq!(shared.borrow().focus(), &editor);
        assert_eq!(host.focus_owner(WinId::MAIN), Some(editor));
    }

    #[test]
    fn a_start_without_targets_moves_nothing() {
        let (driver, host) = setup();
        let shared = walk_over(&driver, &host, spawn(), EditorPresence::Owned, editor_hidden());
        assert!(!shared.borrow().is_shown(&ModuleId::editor()), "no targets, no move");
        assert_eq!(shared.borrow().focus(), &ModuleId::agent());
        assert_eq!(open_state(&driver).0, Vec::<String>::new());
        assert!(driver.query(|c| c.start_notes().is_empty()));
    }

    #[test]
    fn targets_over_a_shown_editor_change_nothing_but_the_editor_spawn() {
        let editor_first = || {
            let mut layout = Layout::initial(&[ModuleDecl {
                id: ModuleId::terminal(),
                placement: Placement::BelowRoot,
            }])
            .unwrap();
            layout.set_focus(&ModuleId::editor()).unwrap();
            layout
        };
        let (plain_driver, plain_host) = setup();
        walk_over(
            &plain_driver,
            &plain_host,
            spawn(),
            EditorPresence::Owned,
            editor_first(),
        );
        let (driver, host) = setup();
        walk_over(&driver, &host, targeted(), EditorPresence::Owned, editor_first());
        let plain = ops(&plain_host);
        let targeted_ops = ops(&host);
        assert_eq!(plain.len(), targeted_ops.len());
        for (plain, targeted_op) in plain.iter().zip(&targeted_ops) {
            match (plain, targeted_op) {
                (
                    HostOp::CreateSurface {
                        kind: SurfaceKind::Editor { spawn: plain_spawn },
                        ..
                    },
                    HostOp::CreateSurface {
                        kind: SurfaceKind::Editor { spawn: targeted_spawn },
                        ..
                    },
                ) => assert_eq!(
                    plain_spawn,
                    &EditorSpawn {
                        open: vec![],
                        ..targeted_spawn.clone()
                    },
                    "the editor is created with the targets and otherwise as it always was"
                ),
                (plain, targeted_op) => assert_eq!(plain, targeted_op),
            }
        }
        assert!(driver.query(|c| c.start_notes().is_empty()));
    }

    /// The user's close of the window, run through its teardown.
    fn close(driver: &Driver<Controller>, scheduler: &FakeScheduler) {
        driver.close_requested(WinId::MAIN, CloseBy::User);
        scheduler.advance(std::time::Duration::ZERO);
    }

    /// The files the saver wrote into `dir`.
    fn written(dir: &std::path::Path) -> usize {
        std::fs::read_dir(dir).unwrap().count()
    }

    /// Starts over `layout` with targets, closes as the teardown does, and returns how many files the close wrote.
    fn files_written_by_a_targeted_start_and_close(label: &str, layout: Layout) -> (usize, Rc<RefCell<Layout>>) {
        let (driver, host, scheduler) = setup_with_scheduler();
        let dir = agent::state_dirs::test_workspace_dir(label);
        let shared = walk_over_saving_in(dir.clone(), &driver, &host, targeted(), EditorPresence::Owned, layout);
        assert!(shared.borrow().is_shown(&ModuleId::editor()));
        assert!(!open_state(&driver).2, "no save timer is armed by the launch");
        close(&driver, &scheduler);
        (written(&dir), shared)
    }

    #[test]
    fn the_start_does_not_save_the_shown_editor() {
        let (files, _) = files_written_by_a_targeted_start_and_close("start-save-hidden", editor_hidden());
        assert_eq!(files, 0, "closing over the launch's own show writes nothing");
    }

    #[test]
    fn the_start_does_not_save_the_placed_editor() {
        let (files, _) = files_written_by_a_targeted_start_and_close("start-save-placed", editor_missing());
        assert_eq!(files, 0, "closing over the launch's own placement writes nothing");
    }

    #[test]
    fn a_change_after_the_targeted_start_is_saved() {
        let (driver, host, scheduler) = setup_with_scheduler();
        let dir = agent::state_dirs::test_workspace_dir("start-save-after");
        let shared = walk_over_saving_in(
            dir.clone(),
            &driver,
            &host,
            targeted(),
            EditorPresence::Owned,
            editor_hidden(),
        );
        crate::layout::hide(&mut shared.borrow_mut(), &ModuleId::terminal(), &frame()).unwrap();
        close(&driver, &scheduler);
        assert_eq!(written(&dir), 1, "a change the user made is written");
    }

    #[test]
    fn a_refused_show_starts_without_moving_the_keys() {
        let (driver, host) = setup();
        let mut layout = editor_hidden();
        layout.set_away(&ModuleId::editor(), true).unwrap();
        let shared = walk_over(&driver, &host, targeted(), EditorPresence::Owned, layout);
        assert!(!shared.borrow().is_shown(&ModuleId::editor()));
        assert_eq!(shared.borrow().focus(), &ModuleId::agent());
        assert!(driver.query(|c| c
            .start_notes()
            .iter()
            .any(|note| note.contains("the launch's editor was not shown"))));
        assert!(!open_state(&driver).2);
    }

    #[test]
    fn window_2_carries_no_targets() {
        let (driver, host) = setup();
        let shared = walk_over(&driver, &host, targeted(), EditorPresence::Proxy, editor_hidden());
        assert!(
            !shared.borrow().is_shown(&ModuleId::editor()),
            "a window with no editor shows none"
        );
        assert_eq!(shared.borrow().focus(), &ModuleId::agent());
        assert!(driver.query(|c| c.start_notes().is_empty()));
    }

    /// Runs a window's start up to its editor phase with `spawn` as the feeds' settings, and returns the editor's
    /// surface creation.
    fn editor_launched_with(spawn: EditorSpawn) -> HostOp {
        let (driver, host) = setup();
        for step in [
            StartStep::Feeds { spawn },
            StartStep::Window {
                spec: spec(EditorPresence::Owned),
            },
            StartStep::Editor,
        ] {
            driver.start(WinId::MAIN, step);
        }
        ops(&host)
            .into_iter()
            .find(|op| {
                matches!(
                    op,
                    HostOp::CreateSurface {
                        kind: SurfaceKind::Editor { .. },
                        ..
                    }
                )
            })
            .expect("an owned editor is created in the editor phase")
    }

    #[test]
    fn a_restarted_editor_gets_the_feeds_env_again() {
        let (driver, host) = setup();
        let mut settings = spawn();
        settings
            .args
            .extend(["--cmd".into(), "let g:feed = 1".into(), "file.rs".into()]);
        driver.start(
            WinId::MAIN,
            StartStep::Feeds {
                spawn: settings.clone(),
            },
        );
        driver.start(
            WinId::MAIN,
            StartStep::Window {
                spec: spec(EditorPresence::Owned),
            },
        );
        driver.start(WinId::MAIN, StartStep::Editor);
        driver.start(WinId::MAIN, StartStep::Kernel);
        driver.start(WinId::MAIN, config());
        driver.start(
            WinId::MAIN,
            StartStep::Layout {
                parts: parts(),
                own_window: None,
            },
        );
        driver.start(
            WinId::MAIN,
            StartStep::Surfaces {
                decls: vec![
                    crate::wm::verbs::rig::decl(&ModuleId::editor()),
                    crate::wm::verbs::rig::decl(&ModuleId::agent()),
                ],
            },
        );
        host.set_editor_facts(EditorFacts {
            exited: true,
            ..Default::default()
        });
        driver.run(
            WinId::MAIN,
            crate::keymap::Action::ModuleKill,
            Origin::Key { keycode: 7 },
        );
        let key = crate::keymap::KeySpec::parse("y").unwrap();
        driver.key(
            WinId::MAIN,
            &KeyStage::Window,
            KeyPress {
                keycode: 42,
                press: crate::keymap::prefix::Press::Key(key),
                key: Some(key.key),
                mods: Mods::default(),
                super_or_hyper: false,
                super_or_meta: false,
            },
        );
        host.set_editor_facts(EditorFacts {
            exited: true,
            ..Default::default()
        });
        driver.post(HostEvent::Editor(EditorEvent::Released { win: WinId::MAIN }));
        driver.run(
            WinId::MAIN,
            crate::keymap::Action::Module(ModuleId::editor()),
            Origin::Key { keycode: 7 },
        );
        let log = ops(&host);
        let initial = log
            .iter()
            .find_map(|op| match op {
                HostOp::CreateSurface {
                    kind: SurfaceKind::Editor { spawn },
                    ..
                } => Some(spawn),
                _ => None,
            })
            .unwrap();
        let restarted = log
            .iter()
            .find_map(|op| match op {
                HostOp::Editor(EditorCall::Restart { spawn, .. }) => Some(spawn),
                _ => None,
            })
            .unwrap();
        assert_eq!(initial, &settings);
        assert_eq!(restarted, initial);
    }

    #[test]
    fn the_editor_is_launched_with_the_feeds_spawns_background() {
        let light = EditorSpawn {
            background: Some((0xfa, 0xf4, 0xed)),
            ..spawn()
        };
        assert_eq!(
            editor_launched_with(light.clone()),
            HostOp::CreateSurface {
                win: WinId::MAIN,
                module: ModuleId::editor(),
                kind: SurfaceKind::Editor { spawn: light },
            },
            "the start colour reaches the editor exactly as the feeds phase gave it"
        );
    }

    #[test]
    fn without_a_remembered_theme_the_editor_keeps_its_own_placeholder() {
        let plain = spawn();
        assert_eq!(plain.background, None);
        assert_eq!(
            editor_launched_with(plain.clone()),
            HostOp::CreateSurface {
                win: WinId::MAIN,
                module: ModuleId::editor(),
                kind: SurfaceKind::Editor { spawn: plain },
            },
            "no start colour is invented when none was given"
        );
    }

    #[test]
    fn the_start_plan_runs_feeds_before_the_editor_spawns_and_presents_before_the_first_focus() {
        let (driver, host) = setup();
        walk(&driver, &host, ModuleId::agent(), Some(ViewId::BASE), true);
        let ops = ops(&host);
        assert!(matches!(&ops[0], HostOp::OpenWindow { win: WinId::MAIN, .. }));
        assert_eq!(
            ops[1],
            HostOp::CreateSurface {
                win: WinId::MAIN,
                module: ModuleId::editor(),
                kind: SurfaceKind::Editor { spawn: spawn() },
            }
        );
        let init = ops.iter().position(|op| *op == HostOp::Lua(LuaOp::LoadInit)).unwrap();
        for (index, op) in ops.iter().enumerate() {
            if matches!(op, HostOp::CreateSurface { kind, .. } if !matches!(kind, SurfaceKind::Editor { .. })) {
                assert!(init < index);
            }
        }
        let present = ops.iter().position(|op| matches!(op, HostOp::Present { .. })).unwrap();
        let focus = ops.iter().position(|op| matches!(op, HostOp::Focus { .. })).unwrap();
        assert!(present < focus);
        assert_eq!(
            ops[focus],
            HostOp::Focus {
                win: WinId::MAIN,
                to: FocusTo::Module(ModuleId::agent()),
                on_grab: vec![HostOp::Chat(ChatOp::Arrive {
                    win: WinId::MAIN,
                    view: ViewId::BASE,
                    card_first: true,
                })],
            }
        );
        assert!(ops.contains(&HostOp::Chat(ChatOp::Arrive {
            win: WinId::MAIN,
            view: ViewId::BASE,
            card_first: true,
        })));
        assert_eq!(
            ops.last(),
            Some(&HostOp::Lua(LuaOp::Emit {
                event: "shell:ready".into()
            }))
        );
    }

    #[test]
    fn no_op_names_a_window_before_open_window() {
        let (driver, host) = setup();
        walk(&driver, &host, ModuleId::editor(), None, true);
        let mut opened = false;
        for op in ops(&host) {
            match op {
                HostOp::OpenWindow { win, .. } => {
                    assert_eq!(win, WinId::MAIN);
                    opened = true;
                }
                HostOp::Lua(_) => {}
                _ => assert!(opened, "{op:?} before OpenWindow"),
            }
        }
        assert!(opened);
    }

    #[test]
    fn a_refused_first_focus_runs_no_arrive() {
        let (driver, host) = setup();
        walk(&driver, &host, ModuleId::agent(), Some(ViewId::BASE), false);
        assert!(host
            .log()
            .iter()
            .any(|entry| matches!(entry, Logged::Op(HostOp::Focus { .. }, Applied::Refused))));
        assert!(!ops(&host)
            .iter()
            .any(|op| matches!(op, HostOp::Chat(ChatOp::Arrive { .. }))));
    }

    #[test]
    fn a_first_focus_without_a_chat_still_takes_the_keys() {
        let (driver, host) = setup();
        walk(&driver, &host, ModuleId::editor(), None, true);
        let ops = ops(&host);
        let present = ops.iter().position(|op| matches!(op, HostOp::Present { .. })).unwrap();
        assert_eq!(
            ops[present],
            HostOp::Present {
                win: WinId::MAIN,
                activation: Some(ActivationToken("token".into()))
            }
        );
        assert_eq!(
            ops[present + 1],
            HostOp::Focus {
                win: WinId::MAIN,
                to: FocusTo::Module(ModuleId::editor()),
                on_grab: vec![],
            }
        );
        assert_eq!(host.focus_owner(WinId::MAIN), Some(ModuleId::editor()));
    }

    #[test]
    fn the_window_exists_before_its_layout() {
        for presence in [EditorPresence::Owned, EditorPresence::Proxy, EditorPresence::None] {
            let (driver, host) = setup();
            driver.start(WinId::MAIN, StartStep::Feeds { spawn: spawn() });
            driver.start(
                WinId::MAIN,
                StartStep::Window {
                    spec: spec(presence.clone()),
                },
            );
            assert_eq!(
                driver.query(|c| c.window_spec(WinId::MAIN).cloned()),
                Some(spec(presence.clone()))
            );
            driver.post(HostEvent::Pair(PairEvent::RestoreFailed { win: WinId::MAIN }));
            assert!(driver.query(|c| c
                .start_notes()
                .iter()
                .any(|line| line.contains("RestoreFailed") && line.ends_with("for a window with no layout yet"))));
            assert_eq!(ops(&host).len(), 1);
            let before = host.log().len();
            driver.post(HostEvent::Lua(LuaEvent::LayoutRequests {
                win: WinId::MAIN,
                requests: vec![crate::lua::layout::LayoutRequest::Show(ModuleId::terminal())],
            }));
            assert_eq!(host.log().len(), before);
            assert!(driver.query(|c| c
                .start_notes()
                .iter()
                .any(|line| line.contains("LayoutRequests") && line.ends_with("for a window with no layout yet"))));
            driver.start(WinId::MAIN, StartStep::Editor);
            assert_eq!(ops(&host).len(), if presence == EditorPresence::Owned { 2 } else { 1 });
            driver.start(WinId::MAIN, StartStep::Kernel);
            driver.start(WinId::MAIN, config());
            driver.start(
                WinId::MAIN,
                StartStep::Layout {
                    parts: parts(),
                    own_window: None,
                },
            );
            driver.start(
                WinId::MAIN,
                StartStep::Surfaces {
                    decls: vec![SurfaceDecl {
                        module: ModuleId::terminal(),
                        kind: SurfaceKind::Terminal { cwd: ".".into() },
                        deferred: false,
                    }],
                },
            );
            driver.test_batch(|c| c.changed_in_test(WinId::MAIN));
            assert!(ops(&host).contains(&HostOp::Terminal(TermOp::Start {
                win: WinId::MAIN,
                module: ModuleId::terminal()
            })));
            assert_eq!(
                driver.query(|c| c.lifecycle(WinId::MAIN).unwrap().state(&ModuleId::terminal()).cloned()),
                Some(ModuleState::Shown)
            );
        }
    }

    #[test]
    fn a_start_step_out_of_order_is_refused() {
        let (driver, host) = setup();
        driver.start(WinId::MAIN, StartStep::Editor);
        assert!(host.log().is_empty());
        assert!(driver.query(|c| c
            .start_notes()
            .iter()
            .any(|line| line == "[wm] BUG: start step out of order")));
        driver.start(WinId::MAIN, StartStep::Feeds { spawn: spawn() });
        driver.start(WinId::MAIN, StartStep::Kernel);
        assert!(host.log().is_empty());
        driver.start(
            WinId::MAIN,
            StartStep::Window {
                spec: spec(EditorPresence::Owned),
            },
        );
        driver.start(WinId::MAIN, StartStep::Editor);
        assert_eq!(ops(&host).len(), 2);
        driver.start(WinId::MAIN, StartStep::Editor);
        assert_eq!(ops(&host).len(), 2);
    }

    /// [`setup`] with its scheduler, whose idles a test fires by advancing it.
    fn setup_with_clock() -> (Rc<Driver<Controller>>, Rc<FakeHost>, Rc<FakeScheduler>) {
        let host = FakeHost::new();
        let scheduler = FakeScheduler::new();
        let mut controller = Controller::new(scheduler.clone());
        controller.set_reads(host.clone());
        let driver = Driver::new(controller, host.clone(), scheduler.clone());
        let sink: Rc<dyn HostSink> = driver.clone();
        host.attach(Rc::downgrade(&sink));
        scheduler.attach(Rc::downgrade(&sink));
        (driver, host, scheduler)
    }

    fn restores(host: &FakeHost) -> usize {
        ops(host)
            .iter()
            .filter(|op| matches!(op, HostOp::RestoreLinkedWindow { win } if *win == WinId::MAIN))
            .count()
    }

    #[test]
    fn window_2_is_restored_at_the_requests_phase_only_when_the_placement_says_so() {
        for restore_second in [false, true] {
            let (driver, host, scheduler) = setup_with_clock();
            driver.set_placement(
                WinId::MAIN,
                WindowPlacement {
                    role: PlacementRole::Main,
                    remembered: None,
                    restore_second,
                },
            );
            walk(&driver, &host, ModuleId::editor(), None, true);
            // Once the requests phase's own batch has gone, `shell:ready` with it.
            assert_eq!(restores(&host), 0, "nothing is started inside the start's own batches");
            let ready = host
                .log()
                .iter()
                .position(|entry| {
                    matches!(entry, Logged::Op(HostOp::Lua(LuaOp::Emit { event }), _) if event == "shell:ready")
                })
                .expect("the requests phase emits shell:ready");
            scheduler.advance(std::time::Duration::ZERO);
            assert_eq!(
                restores(&host),
                usize::from(restore_second),
                "restore_second {restore_second}"
            );
            if restore_second {
                let at = host
                    .log()
                    .iter()
                    .position(|entry| matches!(entry, Logged::Op(HostOp::RestoreLinkedWindow { .. }, _)))
                    .unwrap();
                assert!(at > ready, "window 2 starts after shell:ready");
            }
            scheduler.advance(std::time::Duration::from_secs(1));
            assert_eq!(restores(&host), usize::from(restore_second), "started at most once");
            // A refusal is the host's to act on; the controller only logs it.
            let before = host.log().len();
            driver.post(HostEvent::Refused {
                what: OpTag::RestoreLinkedWindow,
                why: "the second window could not be started".into(),
                surface: None,
            });
            assert_eq!(host.log().len(), before, "a refused restore applies nothing");
        }
    }

    fn monitor(connector: &str, x: i32) -> Monitor {
        Monitor {
            id: MonitorId(connector.into()),
            rect: crate::layout::Rect {
                x,
                y: 0,
                w: 1920,
                h: 1080,
            },
            scale: 1.0,
            name: connector.into(),
        }
    }

    fn moves(host: &FakeHost) -> Vec<String> {
        ops(host)
            .into_iter()
            .filter_map(|op| match op {
                HostOp::PlaceWindow { win, monitor, .. } if win == WinId::MAIN => monitor.map(|m| m.0),
                _ => None,
            })
            .collect()
    }

    fn on(driver: &Driver<Controller>, connector: Option<&str>) {
        driver.post(HostEvent::WindowMonitor {
            win: WinId::MAIN,
            monitor: connector.map(|c| MonitorId(c.into())),
        });
    }

    fn remembering(connector: Option<&str>) -> (Rc<Driver<Controller>>, Rc<FakeHost>) {
        let (driver, host) = setup();
        driver.set_placement(
            WinId::MAIN,
            WindowPlacement {
                role: PlacementRole::Main,
                remembered: connector.map(Into::into),
                restore_second: false,
            },
        );
        (driver, host)
    }

    fn placements(host: &FakeHost) -> Vec<(Option<MonitorId>, PlacementRole)> {
        ops(host)
            .into_iter()
            .filter_map(|op| match op {
                HostOp::PlaceWindow { monitor, role, .. } => Some((monitor, role)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn placement_carries_each_role_and_restores_focus_even_without_a_move() {
        for role in [
            PlacementRole::Main,
            PlacementRole::NewSecond,
            PlacementRole::RestoredSecond { first_pid: 123 },
        ] {
            for remembered in [None, Some("DP-1"), Some("DP-2")] {
                let (driver, host) = setup();
                driver.set_placement(
                    WinId::MAIN,
                    WindowPlacement {
                        role,
                        remembered: remembered.map(Into::into),
                        restore_second: false,
                    },
                );
                driver.post(HostEvent::MonitorsChanged {
                    monitors: vec![monitor("DP-1", 0), monitor("DP-2", 1920)],
                });
                walk(&driver, &host, ModuleId::editor(), None, true);
                on(&driver, None);
                assert!(placements(&host).is_empty(), "wait until mapped onto an output");
                on(&driver, Some("DP-1"));
                let moves = remembered == Some("DP-2");
                let expected = if moves || matches!(role, PlacementRole::RestoredSecond { .. }) {
                    vec![(moves.then(|| MonitorId("DP-2".into())), role)]
                } else {
                    vec![]
                };
                assert_eq!(placements(&host), expected, "{role:?}, {remembered:?}");
                on(&driver, Some("DP-2"));
                on(&driver, Some("DP-1"));
                driver.post(HostEvent::MonitorsChanged {
                    monitors: vec![monitor("DP-1", 0), monitor("DP-2", 1920)],
                });
                assert_eq!(placements(&host), expected, "later reports cannot take the keys again");
            }
        }
    }

    #[test]
    fn a_restore_returns_focus_at_mapping_and_a_late_output_move_takes_no_focus() {
        let (driver, host) = setup();
        walk(&driver, &host, ModuleId::editor(), None, true);
        on(&driver, Some("DP-1"));
        let role = PlacementRole::RestoredSecond { first_pid: 123 };
        driver.set_placement(
            WinId::MAIN,
            WindowPlacement {
                role,
                remembered: Some("DP-2".into()),
                restore_second: false,
            },
        );
        assert_eq!(
            placements(&host),
            [(None, role)],
            "late placement applies to a mapped window"
        );
        driver.post(HostEvent::MonitorsChanged {
            monitors: vec![monitor("DP-1", 0)],
        });
        assert_eq!(placements(&host), [(None, role)]);
        driver.post(HostEvent::MonitorsChanged {
            monitors: vec![monitor("DP-1", 0), monitor("DP-2", 1920)],
        });
        let expected = [(None, role), (Some(MonitorId("DP-2".into())), PlacementRole::Main)];
        assert_eq!(placements(&host), expected);
        on(&driver, Some("DP-2"));
        assert_eq!(placements(&host), expected);
    }

    #[test]
    fn a_new_second_window_whose_output_appears_late_moves_without_focus() {
        let (driver, host) = setup();
        driver.set_placement(
            WinId::MAIN,
            WindowPlacement {
                role: PlacementRole::NewSecond,
                remembered: Some("DP-2".into()),
                restore_second: false,
            },
        );
        driver.post(HostEvent::MonitorsChanged {
            monitors: vec![monitor("DP-1", 0)],
        });
        walk(&driver, &host, ModuleId::editor(), None, true);
        on(&driver, Some("DP-1"));
        assert!(placements(&host).is_empty());
        driver.post(HostEvent::MonitorsChanged {
            monitors: vec![monitor("DP-1", 0), monitor("DP-2", 1920)],
        });
        let expected = [(Some(MonitorId("DP-2".into())), PlacementRole::Main)];
        assert_eq!(placements(&host), expected);
        on(&driver, Some("DP-2"));
        assert_eq!(placements(&host), expected);
    }

    #[test]
    fn the_remembered_monitor_is_asked_for_once_when_it_exists() {
        let (driver, host) = remembering(Some("DP-2"));
        driver.post(HostEvent::MonitorsChanged {
            monitors: vec![monitor("DP-1", 0), monitor("DP-2", 1920)],
        });
        walk(&driver, &host, ModuleId::editor(), None, true);
        assert!(moves(&host).is_empty(), "not before the window is on an output");
        on(&driver, Some("DP-1"));
        assert_eq!(moves(&host), ["DP-2"]);
        // The move itself, and the outputs changing after it, ask nothing again.
        on(&driver, Some("DP-2"));
        on(&driver, Some("DP-1"));
        driver.post(HostEvent::MonitorsChanged {
            monitors: vec![monitor("DP-1", 0), monitor("DP-2", 1920), monitor("HDMI-A-1", 3840)],
        });
        assert_eq!(moves(&host), ["DP-2"], "asked for once");

        // Already there: nothing is asked, then or later.
        let (driver, host) = remembering(Some("DP-2"));
        driver.post(HostEvent::MonitorsChanged {
            monitors: vec![monitor("DP-1", 0), monitor("DP-2", 1920)],
        });
        walk(&driver, &host, ModuleId::editor(), None, true);
        on(&driver, Some("DP-2"));
        on(&driver, Some("DP-1"));
        assert!(moves(&host).is_empty(), "a window already on its monitor stays put");

        // Nothing remembered, or the placement given once the window runs: the same rule.
        let (driver, host) = remembering(None);
        driver.post(HostEvent::MonitorsChanged {
            monitors: vec![monitor("DP-1", 0), monitor("DP-2", 1920)],
        });
        walk(&driver, &host, ModuleId::editor(), None, true);
        on(&driver, Some("DP-1"));
        assert!(moves(&host).is_empty(), "no remembered monitor, no move");
        driver.set_placement(
            WinId::MAIN,
            WindowPlacement {
                role: PlacementRole::Main,
                remembered: Some("DP-2".into()),
                restore_second: false,
            },
        );
        assert_eq!(moves(&host), ["DP-2"], "a placement given later applies at once");

        // A refused move is logged and changes nothing.
        let before = host.log().len();
        driver.post(HostEvent::Refused {
            what: OpTag::PlaceWindow,
            why: "this window manager cannot move a window to an output".into(),
            surface: None,
        });
        assert_eq!(host.log().len(), before, "a refused move applies nothing");
    }

    #[test]
    fn a_missing_monitor_appearing_later_moves_the_window_once() {
        let (driver, host) = remembering(Some("HDMI-A-1"));
        driver.post(HostEvent::MonitorsChanged {
            monitors: vec![monitor("DP-1", 0)],
        });
        walk(&driver, &host, ModuleId::editor(), None, true);
        on(&driver, Some("DP-1"));
        assert!(
            moves(&host).is_empty(),
            "an absent monitor leaves the window where it is"
        );
        driver.post(HostEvent::MonitorsChanged {
            monitors: vec![monitor("DP-1", 0), monitor("HDMI-A-1", 1920)],
        });
        assert_eq!(moves(&host), ["HDMI-A-1"], "moved when it appears");
        driver.post(HostEvent::MonitorsChanged {
            monitors: vec![monitor("DP-1", 0)],
        });
        driver.post(HostEvent::MonitorsChanged {
            monitors: vec![monitor("DP-1", 0), monitor("HDMI-A-1", 1920)],
        });
        on(&driver, Some("DP-1"));
        assert_eq!(moves(&host), ["HDMI-A-1"], "moved once");
    }
}

#[cfg(test)]
mod own_window_tests {
    use super::*;
    use crate::window_link::pair::Role;
    use std::path::Path;

    #[test]
    fn own_window_matches_by_canonical_root() {
        let roots = vec![("demo".into(), Path::new("/project").to_owned())];
        for role in [Role::Main, Role::Unpaired] {
            assert_eq!(own_window(Path::new("/project"), &roots, role).as_deref(), Some("demo"));
        }
    }

    #[test]
    fn a_symlinked_work_dir_matches() {
        let dir = agent::state_dirs::test_workspace_dir("own-window-link");
        std::fs::create_dir(dir.join("project")).unwrap();
        std::os::unix::fs::symlink(dir.join("project"), dir.join("alias")).unwrap();
        let roots = vec![("demo".into(), dir.join("project").canonicalize().unwrap())];
        // Project selection resolves the work directory before the window starts.
        let root =
            crate::project_root::resolve_in(&[dir.join("alias").into_os_string()], None, Ok(dir.clone())).unwrap();
        assert_eq!(own_window(&root, &roots, Role::Main).as_deref(), Some("demo"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_subdirectory_does_not_match() {
        let roots = vec![("demo".into(), Path::new("/project").to_owned())];
        assert_eq!(own_window(Path::new("/project/child"), &roots, Role::Main), None);
    }

    #[test]
    fn window_two_never_decides_own_window() {
        let roots = vec![("demo".into(), Path::new("/project").to_owned())];
        assert_eq!(own_window(Path::new("/project"), &roots, Role::Second), None);
    }
}
