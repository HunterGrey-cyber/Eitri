//! The ordered startup phases and the data each phase hands to the controller.

use super::host::*;
use super::module::SurfaceDecl;
use super::window::{ActivationToken, EditorKind, EditorPresence, Tiling, WinId, WindowParts, WindowSpec};
use super::Controller;
use crate::compositor::mode::{resolve_mode, Mode, ModeFacts, TilingChoice, Why};
use crate::compositor::Wm;
use crate::keymap::Keymap;
use crate::layout::{Frame, Layout, ModuleId, ModuleKeys, ModuleKind};
use crate::tabs::ViewId;
use crate::window_link::cm_file::CmTerminal;
use crate::window_link::names::WindowName;
use std::rc::Rc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartPhase {
    Feeds,
    Window,
    Editor,
    Kernel,
    Config,
    Layout,
    Reopen,
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
        StartPhase::Reopen,
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

/// What a window's start knows of itself before its mode is resolved: everything but `choice` is known before
/// `init.lua` runs, and `choice` as soon as it has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EarlyFacts {
    /// Window 1 started this window: window 2 or a module window, whose mode is not in question.
    pub started: bool,
    /// The window shows a chat of its own; a page window does not.
    pub shows_chat: bool,
    pub mac: bool,
    pub wayland: bool,
    pub wm: Wm,
    pub window2_orphan: bool,
    pub module_orphans: bool,
    pub unpaired: bool,
    pub alone: bool,
    /// What `window.tiling` says; `None` until `init.lua` has run.
    pub choice: Option<TilingChoice>,
}

impl EarlyFacts {
    /// These facts as the mode's rules read them, with what is known only after `init.lua` and the plugins.
    pub fn mode_facts(&self, own_window: bool, live: Option<bool>) -> ModeFacts {
        ModeFacts {
            choice: self.choice.unwrap_or(TilingChoice::Auto),
            mac: self.mac,
            wayland: self.wayland,
            wm: self.wm,
            live,
            own_window,
            window2_orphan: self.window2_orphan,
            module_orphans: self.module_orphans,
            unpaired: self.unpaired,
            alone: self.alone,
        }
    }

    /// How a full window or window 2 tiles, and the mode's rule that decided it. Only a window nobody started has a
    /// mode to resolve: a hub when it is `compositor`.
    pub fn tiling(&self, own_window: bool, live: Option<bool>) -> (Tiling, Mode, Why) {
        let (mode, why) = resolve_mode(&self.mode_facts(own_window, live));
        let hub = !self.started && mode == Mode::Compositor;
        (if hub { Tiling::Hub } else { Tiling::Eitri }, mode, why)
    }
}

/// The start step a window's base chat page can be built at: the first one where the facts rule a hub out, since a
/// hub builds none and everything else builds one before its first document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageAt {
    /// Before `init.lua`: nothing `init.lua` says can make this window a hub.
    Kernel,
    /// After `init.lua`: what it set rules a hub out.
    Config,
    /// Only the resolved mode can tell.
    Layout,
}

/// Where `early` puts the window's base chat page. Rules that force `eitri` mode come first, and module orphans
/// before the rest, since they force compositor mode whatever the session and the setting say; the mode's own order
/// is [`resolve_mode`]'s.
pub fn base_page_at(early: &EarlyFacts) -> PageAt {
    if early.started || early.mac || early.window2_orphan || early.unpaired || early.alone {
        return PageAt::Kernel;
    }
    if early.module_orphans {
        return PageAt::Layout;
    }
    if !early.wayland {
        return PageAt::Kernel;
    }
    match early.choice {
        Some(TilingChoice::Eitri) => PageAt::Config,
        Some(TilingChoice::Auto) if matches!(early.wm, Wm::Gnome | Wm::None) => PageAt::Config,
        _ => PageAt::Layout,
    }
}

/// What a hub's start opens beside it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StartWindows {
    /// The module windows, in the start layout's leaf order.
    pub open: Vec<WindowName>,
    /// The panels of the start layout whose id cannot name a window.
    pub unnamed: Vec<ModuleId>,
}

impl StartWindows {
    pub fn is_empty(&self) -> bool {
        self.open.is_empty() && self.unnamed.is_empty()
    }
}

/// Whether `id` is a module that lives in a window of its own beside a hub.
fn is_a_window(id: &ModuleId) -> bool {
    matches!(id.kind(), ModuleKind::Agent | ModuleKind::LuaWebview)
}

/// What `start`, the layout the window would open as a tile set, is as a hub: the editor over the bottom terminal,
/// that terminal hidden unless the user's own default shows it. A record supplies only its height. Every chat and
/// page `start` shows becomes a module window to open, in leaf order. A hidden one stays closed, as it stays off screen
/// in `eitri` mode; its key opens it. A panel whose id names no window is left out here and reported by [`unnamed_pages`].
pub fn hub_layout(start: &Layout, users_default: bool, record: Option<&CmTerminal>) -> (Layout, Vec<WindowName>) {
    let terminal = ModuleId::terminal();
    let shown = users_default && start.is_shown(&terminal);
    let layout = if let Some(record) = record {
        record.layout(shown)
    } else {
        let mut layout = crate::layout::hub_tree().layout;
        if !shown {
            crate::layout::hide(
                &mut layout,
                &terminal,
                &Frame::new(super::persist::FIRST_LAUNCH_FRAME, 1),
            )
            .expect("the keys are on the editor, so the terminal can hide");
        }
        layout
    };
    let windows = start
        .modules()
        .iter()
        .filter(|id| is_a_window(id) && start.is_shown(id))
        .filter_map(WindowName::for_module)
        .collect();
    (layout, windows)
}

/// The chats and pages `start` shows that [`hub_layout`] has no window for.
pub fn unnamed_pages(start: &Layout) -> Vec<ModuleId> {
    start
        .modules()
        .into_iter()
        .filter(|id| is_a_window(id) && start.is_shown(id) && WindowName::for_module(id).is_none())
        .collect()
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
        /// How the window takes part in the compositor's tiling, known only now that `init.lua` has run.
        tiling: Tiling,
    },
    /// What the shell found and started for reopening the editor's files, once `init.lua` has said whether to:
    /// the window keeps the record's writer and the token of the nvim it is about to start.
    Reopen {
        prepared: crate::editor_reopen_start::Prepared,
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
            Self::Reopen { .. } => StartPhase::Reopen,
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
    /// Whose nvim the window's editor shows, given before the window is opened or after.
    pub(super) editor_kind: EditorKind,
    pub(super) tiling: Tiling,
    pub(super) spawn: Option<EditorSpawn>,
    /// `None` for a window that was told none, which builds no base page.
    early: Option<EarlyFacts>,
    page_built: bool,
    /// What a hub opens once `shell:ready` has gone out.
    pub(super) windows: StartWindows,
}

impl StartState {
    /// Builds the base page unless the window shows no chat or has one already.
    fn build_page(&mut self, win: WinId, batch: &mut Batch) {
        if self.early.is_some_and(|early| early.shows_chat) && !self.page_built {
            self.page_built = true;
            batch.push_layout(HostOp::BuildBasePage { win });
        }
    }

    fn page_at(&self) -> Option<PageAt> {
        self.early.as_ref().map(base_page_at)
    }
}

impl Controller {
    /// What `win`'s start knows so far: given before the kernel phase, and again before the config phase with
    /// `window.tiling`.
    pub fn set_early_facts(&mut self, win: WinId, early: EarlyFacts) {
        self.starts.entry(win).or_default().early = Some(early);
    }

    /// What `win`'s start opens beside it as a hub, from the layout it would otherwise have opened.
    pub fn set_start_windows(&mut self, win: WinId, windows: StartWindows) {
        self.starts.entry(win).or_default().windows = windows;
    }

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
            StartStep::Kernel => {
                if state.page_at() == Some(PageAt::Kernel) {
                    state.build_page(win, &mut batch);
                }
                batch.push_lua(LuaOp::LoadInit);
            }
            StartStep::Config { keymap, module_keys } => {
                if state.page_at() == Some(PageAt::Config) {
                    state.build_page(win, &mut batch);
                }
                self.set_keymap(win, keymap, module_keys);
            }
            StartStep::Chrome => {}
            StartStep::Layout {
                parts,
                own_window,
                tiling,
            } => {
                state.own_window = own_window;
                state.tiling = tiling;
                if tiling == Tiling::Eitri {
                    state.build_page(win, &mut batch);
                }
                self.attach_parts(win, parts);
                self.show_editor_for_targets(win);
            }
            StartStep::Reopen { prepared } => self.reopen_prepared(win, prepared),
            StartStep::Surfaces { decls } => return self.create_surfaces(win, &decls),
            StartStep::Present {
                activation,
                first,
                arrive,
            } => {
                batch.push_layout(HostOp::Present { win, activation });
                // What the start found of the editor's last files waits for a window that can show it.
                self.reopen_present(win, &mut batch);
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
                                tab: None,
                                intent: crate::agents::Intent::Show,
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
                self.arm_start_windows(win);
            }
        }
        batch
    }
}

/// Every window-addressed fact is checked before any layout-dependent handler runs.
pub(super) fn event_window(event: &HostEvent) -> Option<WinId> {
    let win = match event {
        HostEvent::Project(event) => return Some(event.win()),
        HostEvent::KeyReleased { win, .. }
        | HostEvent::CtrlReleased { win }
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
        | HostEvent::ModuleWindowFailed { win, .. }
        | HostEvent::GroupClosed { win }
        | HostEvent::AnswerRefused { win, .. }
        | HostEvent::OpenFile { win, .. }
        | HostEvent::RaiseWindow { win, .. }
        | HostEvent::SurfaceEnded { win, .. }
        | HostEvent::TopBarEntry { win, .. }
        | HostEvent::Onboarding(
            OnboardingEvent::Open { win, .. }
            | OnboardingEvent::Found { win, .. }
            | OnboardingEvent::Written { win, .. }
            | OnboardingEvent::CheckFailed { win, .. }
            | OnboardingEvent::NoScreen { win },
        )
        | HostEvent::TopBarLeft { win }
        | HostEvent::Agents { win, .. }
        | HostEvent::Refused {
            surface: Some((win, _)),
            ..
        }
        | HostEvent::TextSize(
            TextSizeEvent::Both { win, .. }
            | TextSizeEvent::Pane { win, .. }
            | TextSizeEvent::PageAttached { win, .. }
            | TextSizeEvent::Configured { win, .. }
            | TextSizeEvent::Base { win, .. },
        )
        | HostEvent::Editor(
            EditorEvent::Ready { win }
            | EditorEvent::Released { win }
            | EditorEvent::ReleaseRefused { win }
            | EditorEvent::RestartRefused { win, .. }
            | EditorEvent::Exited { win, .. }
            | EditorEvent::Unreachable { win }
            | EditorEvent::Watched { win }
            | EditorEvent::Leaving { win }
            | EditorEvent::Recoverable { win, .. }
            | EditorEvent::Reopen { win, .. }
            | EditorEvent::Captured { win, .. }
            | EditorEvent::QuitNotSent { win, .. }
            | EditorEvent::ScaleReported { win, .. }
            | EditorEvent::QuitAnswered { win, .. }
            | EditorEvent::StartFailed { win, .. }
            | EditorEvent::Navigate { win, .. }
            | EditorEvent::QuitCancelled { win, .. }
            | EditorEvent::OpenFailed { win, .. }
            | EditorEvent::Detached { win }
            | EditorEvent::CloseFacts { win, .. }
            | EditorEvent::HandedBack { win, .. }
            | EditorEvent::Followed { win, .. },
        )
        | HostEvent::Plugin(
            PluginEvent::Frozen { win, .. }
            | PluginEvent::FreezeRefused { win, .. }
            | PluginEvent::Ready { win, .. }
            | PluginEvent::Loaded { win, .. }
            | PluginEvent::LoadFailed { win, .. },
        )
        | HostEvent::Chat(
            ChatEvent::Attention { win, .. }
            | ChatEvent::Busy { win, .. }
            | ChatEvent::TabVerb { win, .. }
            | ChatEvent::ChooserPartnerPick { win, .. }
            | ChatEvent::TabKept { win, .. }
            | ChatEvent::TabLeft { win }
            | ChatEvent::KeysReturn { win, .. }
            | ChatEvent::Covered { win, .. }
            | ChatEvent::Reveal { win, .. }
            | ChatEvent::PageShown { win, .. }
            | ChatEvent::ShowView { win, .. }
            | ChatEvent::ViewEmpty { win, .. }
            | ChatEvent::NavFallthrough { win, .. }
            | ChatEvent::ReviewOpen { win }
            | ChatEvent::EditorDone { win, .. }
            | ChatEvent::EditSelection { win }
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
    use crate::window_link::names::WindowName;
    use crate::wm::fake::{FakeHost, FakeScheduler, Logged};
    use crate::wm::module::{ModuleState, SurfaceDecl};
    use crate::wm::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    #[test]
    fn project_read_answers_are_routed_before_a_layout_exists() {
        use crate::project_chooser::{LinkKind, Listing};
        let win = WinId::MAIN;
        let mut controller = Controller::new(FakeScheduler::new());
        controller.declare_window(win, spec(EditorPresence::Owned));
        let events = [
            ProjectEvent::Listing {
                win,
                serial: 8,
                dir: "/walked".into(),
                listing: Listing::Read {
                    dir: "/physical".into(),
                    entries: vec![],
                    capped: None,
                    exact: None,
                },
            },
            ProjectEvent::Later {
                win,
                serial: 8,
                git: vec!["folder".into()],
                links: vec![("link".into(), LinkKind::Dir)],
            },
            ProjectEvent::Slow { win, serial: 8 },
            ProjectEvent::Stuck { win, serial: 8 },
            ProjectEvent::Freed {
                win,
                mount: "/physical".into(),
            },
            ProjectEvent::Pick {
                win,
                index: 0,
                rows_rev: 12,
            },
            ProjectEvent::PickTwice {
                win,
                index: 0,
                rows_rev: 12,
            },
        ];
        let count = events.len();
        for event in events {
            let event = HostEvent::Project(event);
            assert_eq!(event_window(&event), Some(win));
            assert!(controller.handle(event).is_empty());
        }
        assert_eq!(controller.start_notes().len(), count);
        assert!(controller
            .start_notes()
            .iter()
            .all(|note| !note.contains("/walked") && !note.contains("/physical")));
    }

    #[test]
    fn a_hub_set_at_the_layout_step_reaches_the_window() {
        for tiling in [Tiling::Eitri, Tiling::Hub, Tiling::ModuleWindow] {
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
                    own_window: None,
                    tiling,
                },
            ] {
                controller.start(WinId::MAIN, step);
            }
            assert_eq!(controller.window_mut(WinId::MAIN).unwrap().tiling, tiling);
        }
        assert_eq!(Tiling::default(), Tiling::Eitri);
    }

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

    /// The host takes nothing off the socket until the window can act on it, so a request waits there. What the
    /// controller adds is that neither kind can be left unanswered, or reach a window that has no record yet.
    #[test]
    fn raise_during_start_waits_as_an_open_does() {
        let win = WinId::MAIN;
        let raise = |id| HostEvent::RaiseWindow { win, id, token: None };
        let open = |id| HostEvent::OpenFile {
            win,
            id,
            path: "/project/a.rs".into(),
            line: None,
            col: None,
            token: None,
        };
        assert_eq!(event_window(&raise(1)), Some(win));

        let (driver, host) = setup();
        driver.start(win, StartStep::Feeds { spawn: spawn() });
        driver.start(
            win,
            StartStep::Window {
                spec: spec(EditorPresence::Owned),
            },
        );
        driver.post(open(1));
        driver.post(raise(2));
        let refused = |id| HostOp::OpenAnswer { id, accepted: false };
        assert_eq!(answers(&host), [refused(1), refused(2)]);
        assert!(!ops(&host).iter().any(|op| matches!(op, HostOp::Present { .. })));
        assert!(
            driver.query(|c| c.start_notes().is_empty()),
            "nothing here is a surprise"
        );

        driver.start(win, StartStep::Editor);
        driver.start(win, StartStep::Kernel);
        driver.start(win, config());
        driver.start(
            win,
            StartStep::Layout {
                parts: parts(),
                own_window: None,
                tiling: Tiling::Eitri,
            },
        );
        driver.post(raise(3));
        assert_eq!(answers(&host)[2..], [HostOp::OpenAnswer { id: 3, accepted: true }]);
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
                tiling: Tiling::Eitri,
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

    /// The reopen phase of a start that reopens nothing.
    fn reopen() -> StartStep {
        StartStep::Reopen {
            prepared: crate::editor_reopen_start::Prepared::nothing(),
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
                StartPhase::Reopen,
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
                tiling: Tiling::Eitri,
            },
            reopen(),
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

    fn answers(host: &FakeHost) -> Vec<HostOp> {
        let answer = |op: &HostOp| matches!(op, HostOp::OpenAnswer { .. });
        ops(host).into_iter().filter(answer).collect()
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
                tiling: Tiling::Eitri,
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
        driver.start(WinId::MAIN, reopen());
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
                tiling: Tiling::Eitri,
            },
        );
        driver.start(WinId::MAIN, reopen());
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
                    tab: None,
                    intent: crate::agents::Intent::Show,
                })],
            }
        );
        assert!(ops.contains(&HostOp::Chat(ChatOp::Arrive {
            win: WinId::MAIN,
            view: ViewId::BASE,
            card_first: true,
            tab: None,
            intent: crate::agents::Intent::Show,
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
                    tiling: Tiling::Eitri,
                },
            );
            driver.start(WinId::MAIN, reopen());
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

    /// Walks the start of the chat or page window `name` as the shell does: the one leaf, no editor, no saver.
    fn walk_module_window(
        driver: &Driver<Controller>,
        host: &FakeHost,
        name: &WindowName,
        kind: SurfaceKind,
    ) -> (ModuleId, crate::wm::window::LayoutView) {
        // A chat window shows its chat in its own base view, whatever its number.
        let module = match name {
            WindowName::Chat(_) => ModuleId::agent(),
            _ => name.module().expect("a page window has a module"),
        };
        let layout = crate::layout::reconcile_module(crate::layout::Node::Leaf(module.clone()), &module)
            .unwrap()
            .layout;
        let parts = WindowParts::new(layout, Workspaces::single(), None);
        let (view, _) = parts.views();
        let title = crate::wm::titles::module_window_title("project", name, None);
        for step in [
            StartStep::Feeds { spawn: spawn() },
            StartStep::Window {
                spec: WindowSpec {
                    role: WindowRole::Linked { side: LinkSide::Second },
                    presence: EditorPresence::None,
                    title,
                },
            },
            StartStep::Editor,
            StartStep::Kernel,
            config(),
            StartStep::Layout {
                parts,
                own_window: None,
                tiling: Tiling::ModuleWindow,
            },
            reopen(),
            StartStep::Surfaces {
                decls: vec![SurfaceDecl {
                    module: module.clone(),
                    kind: kind.clone(),
                    deferred: false,
                }],
            },
            StartStep::Chrome,
        ] {
            driver.start(WinId::MAIN, step);
        }
        host.with_surface(WinId::MAIN, module.clone(), kind, true);
        driver.start(
            WinId::MAIN,
            StartStep::Present {
                activation: None,
                first: module.clone(),
                arrive: None,
            },
        );
        driver.start(WinId::MAIN, StartStep::Requests);
        (module, view)
    }

    fn assert_starts_alone(
        driver: &Driver<Controller>,
        host: &FakeHost,
        (module, view): &(ModuleId, crate::wm::window::LayoutView),
        title: &str,
    ) {
        assert_eq!(
            driver.query(|c| c.window_spec(WinId::MAIN).map(|spec| spec.title.clone())),
            Some(title.to_string())
        );
        assert_eq!(
            view.read().modules(),
            vec![module.clone()],
            "one leaf and nothing beside it"
        );
        let ops = ops(host);
        assert!(
            !ops.iter().any(|op| matches!(
                op,
                HostOp::CreateSurface {
                    kind: SurfaceKind::Editor { .. },
                    ..
                }
            )),
            "no editor is made"
        );
        for op in &ops {
            if let HostOp::Focus { to, .. } = op {
                assert_eq!(
                    *to,
                    FocusTo::Module(module.clone()),
                    "the keys start on the module, not the top bar"
                );
            }
        }
        assert_eq!(host.focus_owner(WinId::MAIN), Some(module.clone()));
    }

    #[test]
    fn a_chat_window_starts_with_its_chat_alone() {
        let (driver, host) = setup();
        let name = WindowName::Chat(3);
        let started = walk_module_window(&driver, &host, &name, SurfaceKind::Chat { view: ViewId::BASE });
        assert_eq!(started.0, ModuleId::agent(), "its chat is in its own base view");
        assert_starts_alone(&driver, &host, &started, "project \u{2014} Eitri chat 3");
    }

    #[test]
    fn a_page_window_starts_with_its_page_alone() {
        let (driver, host) = setup();
        let name = WindowName::Page("markdown".into());
        let kind = SurfaceKind::PluginPage {
            plugin: "markdown".into(),
        };
        let started = walk_module_window(&driver, &host, &name, kind);
        assert_eq!(started.0, ModuleId::lua("markdown"));
        assert_starts_alone(&driver, &host, &started, "project \u{2014} markdown \u{b7} Eitri");
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

    fn compositor_session() -> EarlyFacts {
        EarlyFacts {
            started: false,
            shows_chat: true,
            mac: false,
            wayland: true,
            wm: Wm::Sway,
            window2_orphan: false,
            module_orphans: false,
            unpaired: false,
            alone: false,
            choice: None,
        }
    }

    /// The start steps up to the layout's, with `early` told as the shell tells it; which phases carried
    /// `BuildBasePage`, and how the window tiled.
    fn page_built_at(early: EarlyFacts, choice: TilingChoice, live: Option<bool>) -> (Vec<StartPhase>, Tiling) {
        let (driver, host) = setup();
        let mut early = early;
        driver.set_early_facts(WinId::MAIN, early);
        for step in [
            StartStep::Feeds { spawn: spawn() },
            StartStep::Window {
                spec: spec(EditorPresence::Owned),
            },
            StartStep::Editor,
        ] {
            driver.start(WinId::MAIN, step);
        }
        let built = |host: &FakeHost| {
            ops(host)
                .iter()
                .filter(|op| matches!(op, HostOp::BuildBasePage { win } if *win == WinId::MAIN))
                .count()
        };
        let mut phases = Vec::new();
        driver.start(WinId::MAIN, StartStep::Kernel);
        if built(&host) > 0 {
            phases.push(StartPhase::Kernel);
        }
        early.choice = Some(choice);
        driver.set_early_facts(WinId::MAIN, early);
        let so_far = built(&host);
        driver.start(WinId::MAIN, config());
        if built(&host) > so_far {
            phases.push(StartPhase::Config);
        }
        let tiling = early.tiling(false, live).0;
        let so_far = built(&host);
        driver.start(
            WinId::MAIN,
            StartStep::Layout {
                parts: parts(),
                own_window: None,
                tiling,
            },
        );
        if built(&host) > so_far {
            phases.push(StartPhase::Layout);
        }
        (phases, tiling)
    }

    #[test]
    fn the_base_page_is_built_at_the_step_the_facts_allow() {
        use StartPhase::{Config, Kernel, Layout as AtLayout};
        let at = |early, choice, live| page_built_at(early, choice, live).0;
        let base = compositor_session();

        // What rules a hub out before `init.lua`: nothing it says can change the answer, even `compositor`.
        let x11 = EarlyFacts { wayland: false, ..base };
        assert_eq!(at(x11, TilingChoice::Compositor, Some(true)), [Kernel]);
        for early in [
            EarlyFacts { mac: true, ..base },
            EarlyFacts {
                window2_orphan: true,
                ..base
            },
            EarlyFacts { unpaired: true, ..base },
            EarlyFacts { alone: true, ..base },
        ] {
            assert_eq!(at(early, TilingChoice::Compositor, Some(true)), [Kernel]);
        }

        // What `init.lua` rules out: the setting itself, or `auto` on a desktop that does not tile.
        let gnome = EarlyFacts { wm: Wm::Gnome, ..base };
        assert_eq!(at(gnome, TilingChoice::Auto, None), [Config]);
        assert_eq!(
            at(EarlyFacts { wm: Wm::None, ..base }, TilingChoice::Auto, None),
            [Config]
        );
        assert_eq!(at(base, TilingChoice::Eitri, None), [Config]);

        // A tiling compositor under `auto`: only the resolved mode tells, and then only `eitri` builds it.
        assert_eq!(at(base, TilingChoice::Auto, None), [AtLayout]);
        assert_eq!(at(base, TilingChoice::Auto, Some(true)), []);
        assert_eq!(at(gnome, TilingChoice::Compositor, Some(true)), []);

        // Orphaned module windows force compositor mode whatever the session and the setting say.
        let orphans = EarlyFacts {
            module_orphans: true,
            ..base
        };
        for early in [
            orphans,
            EarlyFacts {
                wayland: false,
                ..orphans
            },
        ] {
            for choice in [TilingChoice::Auto, TilingChoice::Eitri, TilingChoice::Compositor] {
                assert_eq!(at(early, choice, None), [], "{early:?} {choice:?}");
                assert_eq!(
                    base_page_at(&EarlyFacts {
                        choice: Some(choice),
                        ..early
                    }),
                    PageAt::Layout
                );
            }
        }
    }

    #[test]
    fn eitri_mode_builds_the_base_chat_page_once() {
        let x11 = EarlyFacts {
            wayland: false,
            ..compositor_session()
        };
        for (early, choice) in [
            (x11, TilingChoice::Auto),
            (compositor_session(), TilingChoice::Eitri),
            (compositor_session(), TilingChoice::Auto),
        ] {
            let (phases, tiling) = page_built_at(early, choice, None);
            assert_eq!(tiling, Tiling::Eitri);
            assert_eq!(phases.len(), 1, "{early:?}: {phases:?}");
        }
    }

    #[test]
    fn a_chat_window_gets_the_page_before_init_lua_and_a_page_window_never() {
        let started = EarlyFacts {
            started: true,
            ..compositor_session()
        };
        // Window 2 and a chat window: `init.lua` cannot make either a hub, and a live compositor changes nothing.
        let (phases, tiling) = page_built_at(started, TilingChoice::Auto, Some(true));
        assert_eq!(phases, [StartPhase::Kernel]);
        assert_eq!(tiling, Tiling::Eitri, "a started window has no mode of its own");
        let page = EarlyFacts {
            shows_chat: false,
            ..started
        };
        assert_eq!(page_built_at(page, TilingChoice::Auto, Some(true)).0, []);
    }

    /// `editor | agent` over the terminal, as a first launch opens it, the terminal hidden.
    fn default_start() -> Layout {
        crate::wm::persist::first_launch_layout(&[]).unwrap()
    }

    fn lua(id: &str) -> ModuleId {
        ModuleId::lua(id)
    }

    fn tree_of(ids: &[ModuleId]) -> Layout {
        let mut leaves = ids.iter().cloned().map(crate::layout::Node::Leaf);
        let first = leaves.next().unwrap();
        let root = leaves.fold(first, |left, right| {
            crate::layout::Node::split(crate::layout::Axis::Row, 0.5, left, right)
        });
        Layout::new(root, ModuleId::editor()).unwrap()
    }

    fn hub_start(driver: &Driver<Controller>, host: &FakeHost, parts: WindowParts, windows: StartWindows) {
        let surfaces = [ModuleId::editor(), ModuleId::terminal()].map(|module| SurfaceDecl {
            kind: if module == ModuleId::editor() {
                SurfaceKind::Editor { spawn: spawn() }
            } else {
                SurfaceKind::Terminal { cwd: ".".into() }
            },
            module,
            deferred: false,
        });
        let early = EarlyFacts {
            choice: Some(TilingChoice::Compositor),
            ..compositor_session()
        };
        driver.set_early_facts(WinId::MAIN, early);
        driver.set_start_windows(WinId::MAIN, windows);
        for step in [
            StartStep::Feeds { spawn: spawn() },
            StartStep::Window {
                spec: spec(EditorPresence::Owned),
            },
            StartStep::Editor,
            StartStep::Kernel,
            config(),
            StartStep::Layout {
                parts,
                own_window: None,
                tiling: Tiling::Hub,
            },
            reopen(),
            StartStep::Surfaces {
                decls: surfaces.to_vec(),
            },
            StartStep::Chrome,
        ] {
            driver.start(WinId::MAIN, step);
        }
        for decl in surfaces {
            host.with_surface(WinId::MAIN, decl.module, decl.kind, true);
        }
        driver.start(
            WinId::MAIN,
            StartStep::Present {
                activation: None,
                first: ModuleId::editor(),
                arrive: None,
            },
        );
        driver.start(WinId::MAIN, StartStep::Requests);
    }

    #[test]
    fn a_start_layout_that_is_not_the_users_default_never_shows_the_terminal() {
        let start = tree_of(&[ModuleId::editor(), ModuleId::agent(), ModuleId::terminal()]);
        let (layout, windows) = hub_layout(&start, false, None);
        assert!(!layout.is_shown(&ModuleId::terminal()));
        assert_eq!(layout.leaves(), [ModuleId::editor(), ModuleId::terminal()]);
        assert_eq!(windows, [WindowName::Chat(1)]);
        let (layout, _) = hub_layout(&default_start(), false, None);
        assert!(!layout.is_shown(&ModuleId::terminal()));
    }

    #[test]
    fn a_record_that_had_the_terminal_shown_starts_it_hidden_with_its_height() {
        let record = CmTerminal {
            shown: true,
            px: Some(300),
        };
        let start = default_start();
        let (layout, _) = hub_layout(&start, false, Some(&record));
        assert!(!layout.is_shown(&ModuleId::terminal()));
        assert_eq!(CmTerminal::of(&layout).px, Some(300));

        let (driver, host, scheduler) = setup_with_clock();
        let (parts, windows) = hub_parts(&start, false, Some(&record));
        let (view, _) = parts.views();
        hub_start(&driver, &host, parts, windows);
        scheduler.advance(std::time::Duration::ZERO);
        assert!(!view.read().is_shown(&ModuleId::terminal()));
        assert_eq!(view.read().focus(), &ModuleId::editor());
    }

    #[test]
    fn the_users_default_showing_the_terminal_shows_it_at_every_start() {
        let start = tree_of(&[ModuleId::editor(), ModuleId::agent(), ModuleId::terminal()]);
        let (layout, _) = hub_layout(&start, true, None);
        assert!(layout.is_shown(&ModuleId::terminal()));

        let record = CmTerminal {
            shown: false,
            px: Some(250),
        };
        let (layout, _) = hub_layout(&start, true, Some(&record));
        assert!(layout.is_shown(&ModuleId::terminal()));
        assert_eq!(CmTerminal::of(&layout).px, Some(250));

        let hidden = tree_of(&[ModuleId::editor(), ModuleId::agent()]);
        for record in [None, Some(&record)] {
            let (layout, _) = hub_layout(&hidden, true, record);
            assert!(!layout.is_shown(&ModuleId::terminal()));
        }
    }

    fn hub_parts(start: &Layout, users_default: bool, record: Option<&CmTerminal>) -> (WindowParts, StartWindows) {
        let (layout, open) = hub_layout(start, users_default, record);
        let windows = StartWindows {
            open,
            unnamed: unnamed_pages(start),
        };
        (WindowParts::new(layout, Workspaces::single(), None), windows)
    }

    fn opened(host: &FakeHost) -> Vec<WindowName> {
        ops(host)
            .into_iter()
            .filter_map(|op| match op {
                HostOp::OpenModuleWindow { name, keep_keys, .. } => {
                    assert!(
                        keep_keys,
                        "{name:?}: a window the start opens leaves the keys where they are"
                    );
                    Some(name)
                }
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_compositor_window_1_builds_no_chat_page_and_no_agent_module() {
        let (driver, host, scheduler) = setup_with_clock();
        let (parts, windows) = hub_parts(&default_start(), false, None);
        let (view, _) = parts.views();
        hub_start(&driver, &host, parts, windows);
        scheduler.advance(std::time::Duration::ZERO);
        assert!(
            !ops(&host).iter().any(|op| matches!(op, HostOp::BuildBasePage { .. })),
            "a hub builds no chat page"
        );
        assert_eq!(view.read().leaves(), [ModuleId::editor(), ModuleId::terminal()]);
        assert!(
            !view.read().is_shown(&ModuleId::terminal()),
            "it starts hidden, as a first launch has it"
        );
    }

    #[test]
    fn a_hub_start_leaves_the_keys_in_the_editor() {
        let (driver, host, scheduler) = setup_with_clock();
        let (parts, windows) = hub_parts(&default_start(), false, None);
        let (view, _) = parts.views();
        hub_start(&driver, &host, parts, windows);
        scheduler.advance(std::time::Duration::ZERO);
        assert_eq!(view.read().focus(), &ModuleId::editor());
        assert_eq!(opened(&host), [WindowName::Chat(1)]);
        let focuses: Vec<HostOp> = ops(&host)
            .into_iter()
            .filter(|op| matches!(op, HostOp::Focus { .. }))
            .collect();
        assert_eq!(focuses.len(), 1, "{focuses:?}");
        assert!(
            matches!(&focuses[0], HostOp::Focus { to: FocusTo::Module(module), .. } if *module == ModuleId::editor())
        );
    }

    #[test]
    fn the_start_layouts_chats_and_pages_open_as_module_windows_in_order() {
        assert_eq!(hub_layout(&default_start(), false, None).1, [WindowName::Chat(1)]);

        let chat3 = ModuleId::instance(ModuleKind::Agent, 3).unwrap();
        let start = tree_of(&[ModuleId::editor(), ModuleId::agent(), lua("markdown"), chat3]);
        assert_eq!(
            hub_layout(&start, false, None).1,
            [
                WindowName::Chat(1),
                WindowName::Page("markdown".into()),
                WindowName::Chat(3)
            ]
        );

        // They go out once `shell:ready` has, in that order.
        let (driver, host, scheduler) = setup_with_clock();
        let (parts, windows) = hub_parts(&start, false, None);
        hub_start(&driver, &host, parts, windows);
        assert_eq!(opened(&host), [], "nothing is started inside the start's own batches");
        scheduler.advance(std::time::Duration::ZERO);
        assert_eq!(
            opened(&host),
            [
                WindowName::Chat(1),
                WindowName::Page("markdown".into()),
                WindowName::Chat(3)
            ]
        );
        let ready = host
            .log()
            .iter()
            .position(
                |entry| matches!(entry, Logged::Op(HostOp::Lua(LuaOp::Emit { event }), _) if event == "shell:ready"),
            )
            .unwrap();
        let first = host
            .log()
            .iter()
            .position(|entry| matches!(entry, Logged::Op(HostOp::OpenModuleWindow { .. }, _)))
            .unwrap();
        assert!(ready < first, "the windows start after shell:ready");
        scheduler.advance(std::time::Duration::from_secs(1));
        assert_eq!(opened(&host).len(), 3, "started once");
    }

    #[test]
    fn the_terminal_the_users_default_shows_stays_shown_and_a_hidden_chat_stays_closed() {
        let start = tree_of(&[
            ModuleId::editor(),
            ModuleId::agent(),
            ModuleId::terminal(),
            lua("notes"),
        ]);
        let (layout, _) = hub_layout(&start, true, None);
        assert!(layout.is_shown(&ModuleId::terminal()));
        assert_eq!(layout.leaves(), [ModuleId::editor(), ModuleId::terminal()]);

        let mut hidden = start;
        crate::layout::hide(
            &mut hidden,
            &ModuleId::agent(),
            &crate::layout::Frame::new(crate::wm::persist::FIRST_LAUNCH_FRAME, 1),
        )
        .unwrap();
        assert_eq!(hub_layout(&hidden, true, None).1, [WindowName::Page("notes".into())]);
    }

    #[test]
    fn a_page_whose_id_names_no_window_is_skipped_with_a_line_and_a_toast() {
        let plain = lua("Notes");
        let odd = lua("My Panel");
        let start = tree_of(&[ModuleId::editor(), odd.clone(), plain]);
        assert_eq!(hub_layout(&start, false, None).1, [WindowName::Page("Notes".into())]);
        assert_eq!(unnamed_pages(&start), [odd]);

        let (driver, host, scheduler) = setup_with_clock();
        let (parts, windows) = hub_parts(&start, false, None);
        hub_start(&driver, &host, parts, windows);
        scheduler.advance(std::time::Duration::ZERO);
        assert_eq!(opened(&host), [WindowName::Page("Notes".into())]);
        let toast = driver.query(|c| c.chrome(WinId::MAIN).toast.clone().map(|toast| toast.text));
        assert_eq!(
            toast.as_deref(),
            Some(crate::wm::feedback::no_page_window_text("My Panel").as_str())
        );
    }

    #[test]
    fn a_window_already_linked_or_starting_is_not_opened_again() {
        let (driver, host, scheduler) = setup_with_clock();
        let (parts, windows) = hub_parts(
            &tree_of(&[
                ModuleId::editor(),
                ModuleId::agent(),
                ModuleId::instance(ModuleKind::Agent, 2).unwrap(),
            ]),
            false,
            None,
        );
        hub_start(&driver, &host, parts, windows);
        // An orphan the start relinked is linked before the idle runs.
        driver.attach_peer(
            WinId::MAIN,
            PeerId(16),
            WindowName::Chat(1),
            crate::wm::fake::FakePeerPort::new(),
        );
        scheduler.advance(std::time::Duration::ZERO);
        assert_eq!(opened(&host), [WindowName::Chat(2)]);
    }

    #[test]
    fn a_compositor_window_1_writes_no_eitri_layout_file() {
        use crate::wm::persist::{layout_dir, Saver};
        let scratch = crate::test_scratch_dir::ScratchDir::new("nv-start", "hub-layout-files");
        let dir = scratch.to_path_buf();
        assert_eq!(layout_dir(Some(dir.clone()), Tiling::Hub), None);
        assert_eq!(layout_dir(Some(dir.clone()), Tiling::ModuleWindow), None);
        assert_eq!(layout_dir(Some(dir.clone()), Tiling::Eitri), Some(dir.clone()));

        let start = |tiling: Tiling, layout_files: &std::path::Path| {
            let (driver, host, scheduler) = setup_with_clock();
            let layout = hub_layout(&default_start(), false, None).0;
            let workspaces = Workspaces::single();
            let saver = Saver::new(
                Some(layout_files.to_path_buf()),
                std::path::Path::new("/project"),
                Vec::new(),
                None,
                &layout,
                &workspaces,
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
                    parts: WindowParts::new(layout, workspaces, Some(saver)),
                    own_window: None,
                    tiling,
                },
            ] {
                driver.start(WinId::MAIN, step);
            }
            host.with_surface(
                WinId::MAIN,
                ModuleId::terminal(),
                SurfaceKind::Terminal { cwd: ".".into() },
                true,
            );
            driver.first_apply(WinId::MAIN, Vec::new());
            driver.run(
                WinId::MAIN,
                crate::keymap::Action::Module(ModuleId::terminal()),
                Origin::Key { keycode: 1 },
            );
            scheduler.advance(std::time::Duration::from_secs(2));
            driver
        };

        // The same start and the same change as a tile window writes: the file the test looks for is real.
        let tiled = dir.join("tiled");
        std::fs::create_dir_all(&tiled).unwrap();
        let driver = start(Tiling::Eitri, &tiled);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while std::fs::read_dir(&tiled).unwrap().next().is_none() {
            assert!(std::time::Instant::now() < deadline, "a tile window writes its layout");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        drop(driver);

        let hub = dir.join("hub");
        std::fs::create_dir_all(&hub).unwrap();
        std::fs::write(hub.join("project.v2.json"), b"{\"left\":\"by eitri mode\"}").unwrap();
        let listing = || {
            let mut all: Vec<_> = std::fs::read_dir(&hub)
                .unwrap()
                .map(|entry| {
                    let path = entry.unwrap().path();
                    (path.clone(), std::fs::read(path).unwrap())
                })
                .collect();
            all.sort();
            all
        };
        let before = listing();
        let driver = start(Tiling::Hub, &hub);
        assert!(driver.query(|c| c.window(WinId::MAIN).is_some_and(|w| w.saver.is_none())));
        drop(driver);
        assert_eq!(listing(), before);
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
    fn a_new_module_window_asks_for_the_keys_once_it_is_mapped_and_is_never_moved() {
        let (driver, host) = setup();
        driver.set_placement(
            WinId::MAIN,
            WindowPlacement {
                role: PlacementRole::NewModule,
                remembered: None,
                restore_second: false,
            },
        );
        driver.post(HostEvent::MonitorsChanged {
            monitors: vec![monitor("DP-1", 0), monitor("DP-2", 1920)],
        });
        walk(&driver, &host, ModuleId::editor(), None, true);
        on(&driver, None);
        assert!(placements(&host).is_empty(), "wait until mapped onto an output");
        on(&driver, Some("DP-2"));
        assert_eq!(placements(&host), vec![(None, PlacementRole::NewModule)]);
        on(&driver, Some("DP-1"));
        on(&driver, Some("DP-2"));
        assert_eq!(
            placements(&host),
            vec![(None, PlacementRole::NewModule)],
            "later reports cannot take the keys again"
        );
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
