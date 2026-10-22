//! The ordered startup phases and the data each phase hands to the controller.

use super::host::*;
use super::module::SurfaceDecl;
use super::window::{ActivationToken, EditorPresence, WinId, WindowParts, WindowSpec};
use super::Controller;
use crate::layout::ModuleId;
use crate::tabs::ViewId;

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

pub enum StartStep {
    Feeds {
        spawn: EditorSpawn,
    },
    Window {
        spec: WindowSpec,
    },
    Editor,
    Kernel,
    Config,
    Layout {
        parts: WindowParts,
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
            Self::Config => StartPhase::Config,
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
    spawn: Option<EditorSpawn>,
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
            StartStep::Feeds { spawn } => state.spawn = Some(spawn),
            StartStep::Window { spec } => {
                self.declare_window(win, spec.clone());
                batch.push_layout(HostOp::OpenWindow { win, spec });
            }
            StartStep::Editor => {
                let spawn = state.spawn.take();
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
            StartStep::Config | StartStep::Chrome => {}
            StartStep::Layout { parts } => self.attach_parts(win, parts),
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
                        .map(|view| HostOp::Chat(ChatOp::Arrive { win, view }))
                        .collect(),
                });
            }
            StartStep::Requests => batch.push_lua(LuaOp::Emit {
                event: "shell:ready".into(),
            }),
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
        | HostEvent::MinSize { win, .. }
        | HostEvent::Settled { win, .. }
        | HostEvent::LayoutChangedByShell { win }
        | HostEvent::Declared { win, .. }
        | HostEvent::SurfaceEnded { win, .. }
        | HostEvent::TopBarEntry { win, .. }
        | HostEvent::Refused {
            surface: Some((win, _)),
            ..
        }
        | HostEvent::Editor(
            EditorEvent::Exited { win }
            | EditorEvent::ScaleReported { win, .. }
            | EditorEvent::QuitAnswered { win, .. }
            | EditorEvent::StartFailed { win, .. }
            | EditorEvent::Navigate { win, .. }
            | EditorEvent::QuitCancelled { win, .. },
        )
        | HostEvent::Chat(
            ChatEvent::Attention { win, .. }
            | ChatEvent::Busy { win, .. }
            | ChatEvent::TabVerb { win, .. }
            | ChatEvent::KeysReturn { win, .. }
            | ChatEvent::Covered { win, .. }
            | ChatEvent::PageShown { win, .. }
            | ChatEvent::ShowView { win, .. }
            | ChatEvent::ViewEmpty { win, .. }
            | ChatEvent::NavFallthrough { win, .. }
            | ChatEvent::ReviewOpen { win }
            | ChatEvent::EditorDone { win, .. }
            | ChatEvent::Hint { win, .. }
            | ChatEvent::Toast { win, .. }
            | ChatEvent::KeymapClosed { win, .. }
            | ChatEvent::EditorRequest { win, .. },
        )
        | HostEvent::Terminal(
            TermEvent::Started { win, .. }
            | TermEvent::Exited { win, .. }
            | TermEvent::Bell { win, .. }
            | TermEvent::Detached { win, .. },
        )
        | HostEvent::Hint(HintEvent::Active { win, .. } | HintEvent::Landed { win, .. }) => win,
        HostEvent::EndSignal { .. }
        | HostEvent::Lua(_)
        | HostEvent::Peer(_)
        | HostEvent::MonitorsChanged
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

    fn setup() -> (Rc<Driver<Controller>>, Rc<FakeHost>) {
        let host = FakeHost::new();
        let scheduler = FakeScheduler::new();
        let driver = Driver::new(Controller::new(scheduler.clone()), host.clone(), scheduler.clone());
        let sink: Rc<dyn HostSink> = driver.clone();
        host.attach(Rc::downgrade(&sink));
        scheduler.attach(Rc::downgrade(&sink));
        (driver, host)
    }

    fn spawn() -> EditorSpawn {
        let feed = agent::socket_path::in_dir(std::path::Path::new("/project"), "feed.sock").unwrap();
        EditorSpawn {
            env: vec![("EDITOR_FEED".into(), feed.to_str().unwrap().into())],
            args: vec!["--embed".into()],
            cwd: Some("/project".into()),
            clean: true,
        }
    }

    fn spec(presence: EditorPresence) -> WindowSpec {
        WindowSpec {
            role: WindowRole::Main,
            presence,
            title: "project".into(),
        }
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
            StartStep::Config,
            StartStep::Layout { parts: parts() },
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
                    view: ViewId::BASE
                })],
            }
        );
        assert!(ops.contains(&HostOp::Chat(ChatOp::Arrive {
            win: WinId::MAIN,
            view: ViewId::BASE
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
            driver.post(HostEvent::LayoutChangedByShell { win: WinId::MAIN });
            assert_eq!(driver.query(|c| c.shell_revision(WinId::MAIN)), 0);
            assert!(driver.query(|c| c.start_notes().iter().any(
                |line| line.contains("LayoutChangedByShell") && line.ends_with("for a window with no layout yet")
            )));
            assert_eq!(ops(&host).len(), 1);
            driver.start(WinId::MAIN, StartStep::Editor);
            assert_eq!(ops(&host).len(), if presence == EditorPresence::Owned { 2 } else { 1 });
            driver.start(WinId::MAIN, StartStep::Kernel);
            driver.start(WinId::MAIN, StartStep::Config);
            driver.start(WinId::MAIN, StartStep::Layout { parts: parts() });
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
            driver.post(HostEvent::LayoutChangedByShell { win: WinId::MAIN });
            assert_eq!(driver.query(|c| c.shell_revision(WinId::MAIN)), 1);
            assert!(ops(&host).contains(&HostOp::Terminal(TermOp::Start {
                win: WinId::MAIN,
                module: ModuleId::terminal()
            })));
            assert_eq!(
                driver.query(|c| c.lifecycle(WinId::MAIN).unwrap().state(&ModuleId::terminal()).cloned()),
                Some(ModuleState::Shown)
            );
            let before = host.log().len();
            driver.post(HostEvent::Lua(LuaEvent::LayoutRequests));
            assert_eq!(host.log().len(), before);
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
}
