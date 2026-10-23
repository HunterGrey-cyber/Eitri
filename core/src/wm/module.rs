//! Module state and service effects derived from a window's layout, without changing that layout.

use super::host::*;
use super::window::WinId;
use crate::layout::{Layout, ModuleId, Reopen};

/// Where a declared module stands in its window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModuleState {
    Declared,
    Hosted,
    Shown,
    Hidden,
    Killed(Reopen),
    Retired,
    Away,
}

/// The terminal child is independent of whether its module is on screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TermRun {
    NotStarted,
    Running { pid: u32 },
    Exited,
}

/// The surface a window needs, including pages whose construction waits for their first show.
#[derive(Debug, Clone, PartialEq)]
pub struct SurfaceDecl {
    pub module: ModuleId,
    pub kind: SurfaceKind,
    pub deferred: bool,
}

struct Module {
    decl: SurfaceDecl,
    state: ModuleState,
    terminal: TermRun,
    built: bool,
}

/// Declarations retain their order so every plugin hears each change in a stable order.
#[derive(Default)]
pub struct Lifecycle {
    modules: Vec<Module>,
}

impl Lifecycle {
    /// Reusing an idle surface starts a fresh instance, including its child and deferred page.
    pub fn declare(&mut self, decl: &SurfaceDecl, hosted: bool) {
        let module = Module {
            decl: decl.clone(),
            state: if hosted {
                ModuleState::Hosted
            } else {
                ModuleState::Declared
            },
            terminal: TermRun::NotStarted,
            built: false,
        };
        if let Some(old) = self.modules.iter_mut().find(|m| m.decl.module == decl.module) {
            *old = module;
        } else {
            self.modules.push(module);
        }
    }

    pub fn state(&self, module: &ModuleId) -> Option<&ModuleState> {
        self.modules.iter().find(|m| &m.decl.module == module).map(|m| &m.state)
    }

    /// Effects have their own batch segment so later allocation decisions still run before them.
    pub fn after_change(&mut self, win: WinId, layout: &Layout, batch: &mut Batch) {
        self.follow_away(layout);
        let shown: Vec<_> = layout.modules().into_iter().filter(|id| layout.is_shown(id)).collect();
        for id in &shown {
            if let Some(module) = self.modules.iter().find(|m| &m.decl.module == id && m.hosted()) {
                if matches!(module.decl.kind, SurfaceKind::Terminal { .. }) && module.terminal == TermRun::NotStarted {
                    batch.push_effect(HostOp::Terminal(TermOp::Start {
                        win,
                        module: id.clone(),
                    }));
                }
            }
        }
        for id in &shown {
            if let Some(module) = self.modules.iter_mut().find(|m| &m.decl.module == id && m.hosted()) {
                if matches!(module.decl.kind, SurfaceKind::PluginPage { .. }) && module.decl.deferred && !module.built {
                    module.built = true;
                    batch.push_effect(HostOp::Plugin(PluginOp::Build {
                        win,
                        module: id.clone(),
                    }));
                }
            }
        }
        let visible = layout.visible_modules();
        for module in &self.modules {
            if module.hosted() && matches!(module.decl.kind, SurfaceKind::PluginPage { .. }) {
                batch.push_effect(HostOp::Plugin(PluginOp::OnScreen {
                    win,
                    module: module.decl.module.clone(),
                    on: visible.contains(&module.decl.module),
                }));
            }
        }
        for module in &mut self.modules {
            if !module.hosted() {
                continue;
            }
            if layout.is_shown(&module.decl.module) {
                if matches!(module.state, ModuleState::Killed(_)) {
                    match &module.decl.kind {
                        SurfaceKind::Chat { view } => {
                            batch.push_effect(HostOp::Chat(ChatOp::OpenChooser { win, view: *view }))
                        }
                        SurfaceKind::LuaPanel { .. } | SurfaceKind::PluginPage { .. } => {
                            batch.push_effect(HostOp::Plugin(PluginOp::Reload {
                                win,
                                module: module.decl.module.clone(),
                                reset_guard: true,
                            }));
                        }
                        _ => {}
                    }
                }
                module.state = ModuleState::Shown;
            } else if !matches!(module.state, ModuleState::Killed(_)) {
                module.state = ModuleState::Hidden;
            }
        }
    }

    /// A module the other window of the pair holds is only a placeholder here: `Away` is not hosted, so no effect
    /// names it while it is there. Back, it is hosted again and the change's own rules decide what it gets (a
    /// terminal never started starts once it is shown). A killed or retired module keeps its state: that is about
    /// its instance, and away is about where it lives.
    fn follow_away(&mut self, layout: &Layout) {
        for module in &mut self.modules {
            let away = layout.is_away(&module.decl.module);
            match module.state {
                ModuleState::Away if !away => module.state = ModuleState::Hosted,
                ModuleState::Hosted | ModuleState::Shown | ModuleState::Hidden if away => {
                    module.state = ModuleState::Away
                }
                _ => {}
            }
        }
    }

    /// Host facts never mutate the layout; the shell has already placed or removed the surface.
    pub fn on_event(&mut self, win: WinId, event: &HostEvent, layout: &Layout, batch: &mut Batch) {
        self.record_event(win, event);
        if let HostEvent::Declared {
            win: event_win, module, ..
        } = event
        {
            if *event_win == win {
                self.declared(win, module, layout, batch);
            }
            return;
        }
        if let HostEvent::SurfaceEnded {
            win: event_win,
            module,
            why: Ended::Killed { .. },
        } = event
        {
            if *event_win != win {
                return;
            }
            let Some(entry) = self.modules.iter().find(|m| &m.decl.module == module) else {
                return;
            };
            match entry.decl.kind {
                SurfaceKind::PluginPage { .. } => {
                    batch.push_effect(HostOp::Plugin(PluginOp::Terminate {
                        win,
                        module: module.clone(),
                    }));
                    batch.push_effect(HostOp::DropSurface {
                        win,
                        module: module.clone(),
                        why: DropWhy::Killed,
                    });
                }
                SurfaceKind::LuaPanel { .. } => batch.push_effect(HostOp::DropSurface {
                    win,
                    module: module.clone(),
                    why: DropWhy::Killed,
                }),
                _ => {}
            }
        }
    }

    /// A module is declared once the shell has placed it, after the change that showed it was heard, so
    /// a terminal already on screen gets its shell from the layout as it stands; nothing else of a
    /// declared module waits on its first show.
    fn declared(&self, win: WinId, id: &ModuleId, layout: &Layout, batch: &mut Batch) {
        let Some(module) = self.modules.iter().find(|m| &m.decl.module == id && m.hosted()) else {
            return;
        };
        if layout.is_shown(id)
            && matches!(module.decl.kind, SurfaceKind::Terminal { .. })
            && module.terminal == TermRun::NotStarted
        {
            batch.push_effect(HostOp::Terminal(TermOp::Start {
                win,
                module: id.clone(),
            }));
        }
    }

    /// These facts remain valid even when a shell closure still holds the layout's borrow.
    pub(super) fn record_event(&mut self, win: WinId, event: &HostEvent) {
        let (event_win, id) = match event {
            HostEvent::Declared {
                win: event_win,
                module,
                kind,
            } => {
                if *event_win != win {
                    return;
                }
                // A kill already forgot the old shell, so a shell running in a hosted entry is this
                // instance's own, started before its declaration arrived: forgetting it would start it again.
                let running = self
                    .modules
                    .iter()
                    .find(|m| &m.decl.module == module && m.hosted())
                    .map(|m| m.terminal.clone())
                    .filter(|run| matches!(run, TermRun::Running { .. }));
                self.declare(
                    &SurfaceDecl {
                        module: module.clone(),
                        kind: kind.clone(),
                        deferred: false,
                    },
                    true,
                );
                if let Some(run) = running.filter(|_| matches!(kind, SurfaceKind::Terminal { .. })) {
                    if let Some(entry) = self.modules.iter_mut().find(|m| &m.decl.module == module) {
                        entry.terminal = run;
                    }
                }
                return;
            }
            HostEvent::Terminal(TermEvent::Started { win, module, .. } | TermEvent::Exited { win, module })
            | HostEvent::SurfaceEnded { win, module, .. }
            | HostEvent::Refused {
                what: OpTag::CreateSurface,
                surface: Some((win, module)),
                ..
            } => (*win, module),
            _ => return,
        };
        if event_win != win {
            return;
        }
        let Some(module) = self.modules.iter_mut().find(|m| &m.decl.module == id) else {
            return;
        };
        match event {
            HostEvent::Terminal(TermEvent::Started { pid, .. }) => module.terminal = TermRun::Running { pid: *pid },
            HostEvent::Terminal(TermEvent::Exited { .. }) => module.terminal = TermRun::Exited,
            HostEvent::SurfaceEnded {
                why: Ended::Killed { reopen },
                ..
            } => {
                module.state = ModuleState::Killed(*reopen);
                if matches!(module.decl.kind, SurfaceKind::Terminal { .. }) {
                    module.terminal = TermRun::NotStarted;
                }
            }
            HostEvent::SurfaceEnded {
                why: Ended::Retired, ..
            } => module.state = ModuleState::Retired,
            // The host built nothing, so no effect may name the module until it is declared again.
            HostEvent::Refused { .. } => module.state = ModuleState::Declared,
            _ => {}
        }
    }
}

impl Module {
    fn hosted(&self) -> bool {
        !matches!(
            self.state,
            ModuleState::Declared | ModuleState::Retired | ModuleState::Away
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keymap::action::Action;
    use crate::layout::{hide, kill, Frame, ModuleDecl, Placement, Reopen, Size, Workspaces};
    use crate::tabs::ViewId;
    use crate::wm::fake::{FakeHost, FakeScheduler, Logged};
    use crate::wm::persist::Saver;
    use crate::wm::*;
    use std::cell::RefCell;
    use std::path::Path;
    use std::rc::Rc;

    const WIN: WinId = WinId::MAIN;

    fn terminal(name: &str) -> SurfaceDecl {
        SurfaceDecl {
            module: ModuleId::parse(name).unwrap(),
            kind: SurfaceKind::Terminal { cwd: ".".into() },
            deferred: false,
        }
    }

    fn page(name: &str, plugin: bool, deferred: bool) -> SurfaceDecl {
        SurfaceDecl {
            module: ModuleId::lua(name),
            kind: if plugin {
                SurfaceKind::PluginPage { plugin: name.into() }
            } else {
                SurfaceKind::LuaPanel {
                    url: "about:blank".into(),
                    title: name.into(),
                }
            },
            deferred,
        }
    }

    fn chat() -> SurfaceDecl {
        SurfaceDecl {
            module: ModuleId::agent(),
            kind: SurfaceKind::Chat { view: ViewId::BASE },
            deferred: false,
        }
    }

    fn editor() -> SurfaceDecl {
        SurfaceDecl {
            module: ModuleId::editor(),
            kind: SurfaceKind::Editor {
                spawn: EditorSpawn {
                    env: vec![],
                    args: vec![],
                    cwd: None,
                    clean: true,
                },
            },
            deferred: false,
        }
    }

    fn frame() -> Frame<'static> {
        Frame::new(Size { w: 1200, h: 800 }, 4)
    }

    fn start(module: &ModuleId) -> HostOp {
        HostOp::Terminal(TermOp::Start {
            win: WIN,
            module: module.clone(),
        })
    }

    fn plugin(module: &ModuleId, op: &str) -> HostOp {
        HostOp::Plugin(match op {
            "build" => PluginOp::Build {
                win: WIN,
                module: module.clone(),
            },
            "terminate" => PluginOp::Terminate {
                win: WIN,
                module: module.clone(),
            },
            _ => PluginOp::Reload {
                win: WIN,
                module: module.clone(),
                reset_guard: true,
            },
        })
    }

    fn on_screen(module: &ModuleId, on: bool) -> HostOp {
        HostOp::Plugin(PluginOp::OnScreen {
            win: WIN,
            module: module.clone(),
            on,
        })
    }

    fn dropped(module: &ModuleId) -> HostOp {
        HostOp::DropSurface {
            win: WIN,
            module: module.clone(),
            why: DropWhy::Killed,
        }
    }

    struct Driven {
        driver: Rc<Driver>,
        host: Rc<FakeHost>,
        scheduler: Rc<FakeScheduler>,
        layout: Rc<RefCell<Layout>>,
        workspaces: Rc<RefCell<Workspaces>>,
    }

    /// Whether `op` tells a module whether it has the keys.
    fn is_cursor_signal(op: &HostOp) -> bool {
        matches!(
            op,
            HostOp::Editor(EditorCall::SetFocused { .. })
                | HostOp::Chat(ChatOp::EditorHasKeys { .. } | ChatOp::PaneFocused { .. })
                | HostOp::Terminal(TermOp::SetFocused { .. })
                | HostOp::Plugin(PluginOp::HasKeys { .. })
        )
    }

    impl Driven {
        fn open(decls: &[SurfaceDecl], hidden: &[ModuleId], presence: EditorPresence) -> Self {
            let extras: Vec<_> = decls
                .iter()
                .filter(|d| d.module != ModuleId::editor() && d.module != ModuleId::agent())
                .map(|d| ModuleDecl {
                    id: d.module.clone(),
                    placement: Placement::BelowRoot,
                })
                .collect();
            let mut layout = Layout::initial(&extras).unwrap();
            for id in hidden {
                hide(&mut layout, id, &frame()).unwrap();
            }
            let workspaces = Workspaces::single();
            // A real saver makes an accidental startup change observable without writing a file.
            let saver = Saver::new(
                Some(Path::new("target/wm-module-state").into()),
                Path::new("/wm-module"),
                extras,
                None,
                &layout,
                &workspaces,
            );
            let layout = Rc::new(RefCell::new(layout));
            let workspaces = Rc::new(RefCell::new(workspaces));
            let host = FakeHost::new();
            let scheduler = FakeScheduler::new();
            let mut controller = Controller::new(scheduler.clone());
            controller.open_window(
                WIN,
                WindowSpec {
                    role: WindowRole::Main,
                    presence,
                    title: "test".into(),
                },
                WindowParts {
                    layout: layout.clone(),
                    workspaces: workspaces.clone(),
                    saver: Some(saver),
                },
            );
            let driver = Driver::new(controller, host.clone(), scheduler.clone());
            let sink: Rc<dyn HostSink> = driver.clone();
            host.attach(Rc::downgrade(&sink));
            scheduler.attach(Rc::downgrade(&sink));
            Self {
                driver,
                host,
                scheduler,
                layout,
                workspaces,
            }
        }

        /// Every module already held, as if declared before the layout was last heard: no effect yet.
        fn hosted(decls: &[SurfaceDecl], hidden: &[ModuleId]) -> Self {
            let driven = Self::open(decls, hidden, EditorPresence::Owned);
            let decls = decls.to_vec();
            driven.driver.test_batch(move |c| {
                let lifecycle = c.lifecycle_mut(WIN).unwrap();
                for decl in &decls {
                    lifecycle.declare(decl, true);
                }
                Batch::default()
            });
            driven
        }

        fn declare(&self, decl: &SurfaceDecl) {
            self.driver.post(HostEvent::Declared {
                win: WIN,
                module: decl.module.clone(),
                kind: decl.kind.clone(),
            });
        }

        fn state(&self, module: &ModuleId) -> Option<ModuleState> {
            self.driver
                .query(|c| c.lifecycle(WIN).and_then(|l| l.state(module)).cloned())
        }

        /// The lifecycle's ops. The "has the keys" signals a lifecycle event also brings are the focus family's,
        /// whose own tests hold them, so they are left out here.
        fn ops(&self) -> Vec<HostOp> {
            self.host
                .log()
                .into_iter()
                .filter_map(|entry| match entry {
                    Logged::Op(op, _) if !is_cursor_signal(&op) => Some(op),
                    _ => None,
                })
                .collect()
        }

        fn since(&self, count: usize) -> Vec<HostOp> {
            self.ops().into_iter().skip(count).collect()
        }

        /// The window's first layout batch over `hosted`, as the shell's startup runs it.
        fn first_apply(&self, hosted: Vec<SurfaceDecl>) {
            self.driver.first_apply(WIN, hosted);
        }

        /// The ops but those that lay out or redraw the window: the first layout batch's maps, its restack and
        /// allocation and its chrome hooks, around the lifecycle's effects.
        fn effects(&self) -> Vec<HostOp> {
            self.ops()
                .into_iter()
                .filter(|op| {
                    !matches!(
                        op,
                        HostOp::Map { .. }
                            | HostOp::Unmap { .. }
                            | HostOp::Restack { .. }
                            | HostOp::Allocate { .. }
                            | HostOp::ChromeHooks { .. }
                    )
                })
                .collect()
        }

        fn effects_since(&self, count: usize) -> Vec<HostOp> {
            self.effects().into_iter().skip(count).collect()
        }

        fn shell(&self, arm: impl Fn(&mut Layout) + 'static) {
            let layout = self.layout.clone();
            self.host.on_shell_verb(move |_, _, _, _| arm(&mut layout.borrow_mut()));
            self.driver.run(WIN, Action::Module(ModuleId::terminal()), Origin::Lua);
        }

        fn show(&self, module: &ModuleId) {
            let module = module.clone();
            self.shell(move |layout| {
                layout.show(&module).unwrap();
            });
        }

        fn changed(&self) {
            self.driver.post(HostEvent::LayoutChangedByShell { win: WIN });
        }

        fn ended(&self, module: &ModuleId, reopen: Reopen) {
            self.driver.post(HostEvent::SurfaceEnded {
                win: WIN,
                module: module.clone(),
                why: Ended::Killed { reopen },
            });
        }

        fn started(&self, module: &ModuleId) {
            self.driver.post(HostEvent::Terminal(TermEvent::Started {
                win: WIN,
                module: module.clone(),
                pid: 42,
            }));
        }

        fn exited(&self, module: &ModuleId) {
            self.driver.post(HostEvent::Terminal(TermEvent::Exited {
                win: WIN,
                module: module.clone(),
            }));
        }
    }

    #[test]
    fn an_editorless_window_creates_every_surface_but_the_editors() {
        let decls = vec![
            editor(),
            terminal("terminal"),
            chat(),
            page("page", false, false),
            page("plugin", true, true),
        ];
        for presence in [EditorPresence::Proxy, EditorPresence::None, EditorPresence::Owned] {
            let d = Driven::open(&decls, &[], presence.clone());
            let decls_for_batch = decls.clone();
            d.driver.test_batch(|c| c.create_surfaces(WIN, &decls_for_batch));
            let expected: Vec<_> = decls
                .iter()
                .filter(|decl| presence == EditorPresence::Owned || decl.module != ModuleId::editor())
                .map(|decl| HostOp::CreateSurface {
                    win: WIN,
                    module: decl.module.clone(),
                    kind: decl.kind.clone(),
                })
                .collect();
            assert_eq!(d.ops(), expected);
        }
    }

    #[test]
    fn a_retired_editor_is_retired_in_every_workspace() {
        let d = Driven::hosted(&[editor()], &[]);
        for name in ["two", "three"] {
            d.workspaces
                .borrow_mut()
                .create(name, d.layout.borrow().clone())
                .unwrap();
        }
        let workspaces = d.workspaces.clone();
        let host = d.host.clone();
        d.shell(move |layout| {
            kill(layout, &ModuleId::editor(), Reopen::Never, &frame()).unwrap();
            workspaces.borrow_mut().retire_everywhere(&ModuleId::editor(), &frame());
            host.post(HostEvent::SurfaceEnded {
                win: WIN,
                module: ModuleId::editor(),
                why: Ended::Retired,
            });
        });
        for (_, layout) in d.workspaces.borrow().each(&d.layout.borrow()) {
            assert!(layout.is_gone(&ModuleId::editor()));
        }
        assert_eq!(d.state(&ModuleId::editor()), Some(ModuleState::Retired));
        assert!(d.ops().is_empty());
    }

    #[test]
    fn a_killed_lua_panel_drops_its_surface_and_its_key_reopens_it() {
        for reopen in [Reopen::At(Placement::RightOfRoot), Reopen::InPlace] {
            let panel = page("panel", false, false);
            let d = Driven::hosted(&[panel.clone()], &[panel.module.clone()]);
            d.ended(&panel.module, reopen);
            assert_eq!(d.ops(), vec![dropped(&panel.module)]);
            assert_eq!(d.state(&panel.module), Some(ModuleState::Killed(reopen)));
            d.show(&panel.module);
            assert_eq!(d.since(1), vec![plugin(&panel.module, "reload")]);
            assert_eq!(d.state(&panel.module), Some(ModuleState::Shown));
            d.changed();
            assert_eq!(d.ops().len(), 2);
        }
    }

    #[test]
    fn a_killed_plugin_is_stopped_before_its_web_process_ends() {
        let page = page("plugin", true, false);
        let d = Driven::hosted(&[page.clone()], &[page.module.clone()]);
        let ended = HostEvent::SurfaceEnded {
            win: WIN,
            module: page.module.clone(),
            why: Ended::Killed {
                reopen: Reopen::InPlace,
            },
        };
        d.driver.test_batch(|c| {
            let mut batch = c.handle(ended);
            // A layout op joined to the kill's batch still applies before the kill's effects.
            batch.push_layout(HostOp::Allocate { win: WIN });
            batch
        });
        assert_eq!(d.state(&page.module), Some(ModuleState::Killed(Reopen::InPlace)));
        assert_eq!(
            d.ops(),
            vec![
                HostOp::Allocate { win: WIN },
                plugin(&page.module, "terminate"),
                dropped(&page.module)
            ]
        );
    }

    #[test]
    fn a_terminal_shown_by_a_lua_request_starts_its_shell() {
        let t = terminal("terminal");
        let d = Driven::hosted(&[t.clone()], &[t.module.clone()]);
        d.show(&t.module);
        assert_eq!(d.ops(), vec![start(&t.module)]);
        d.started(&t.module);
        d.changed();
        d.exited(&t.module);
        d.changed();
        assert_eq!(d.ops(), vec![start(&t.module)]);
    }

    #[test]
    fn a_killed_terminal_starts_a_fresh_shell_when_shown_again() {
        for exited in [false, true] {
            let t = terminal("terminal");
            let d = Driven::hosted(&[t.clone()], &[]);
            d.started(&t.module);
            if exited {
                d.exited(&t.module);
            }
            let id = t.module.clone();
            d.shell(move |layout| {
                kill(layout, &id, Reopen::At(Placement::BelowEditor), &frame()).unwrap();
            });
            d.ended(&t.module, Reopen::At(Placement::BelowEditor));
            assert!(d.ops().is_empty());
            d.show(&t.module);
            assert_eq!(d.ops(), vec![start(&t.module)]);
        }
    }

    #[test]
    fn a_revived_chat_opens_its_chooser() {
        let chat = chat();
        let d = Driven::hosted(&[chat.clone()], &[chat.module.clone()]);
        d.ended(&chat.module, Reopen::InPlace);
        d.show(&chat.module);
        assert_eq!(
            d.ops(),
            vec![HostOp::Chat(ChatOp::OpenChooser {
                win: WIN,
                view: ViewId::BASE
            })]
        );
        assert_eq!(d.state(&chat.module), Some(ModuleState::Shown));
    }

    #[test]
    fn a_numbered_instance_declared_again_starts_afresh() {
        let t = terminal("terminal#2");
        let d = Driven::hosted(&[t.clone()], &[t.module.clone()]);
        d.started(&t.module);
        d.ended(&t.module, Reopen::Remove);
        d.declare(&t);
        assert_eq!(d.state(&t.module), Some(ModuleState::Hosted));
        d.show(&t.module);
        assert_eq!(d.ops(), vec![start(&t.module)]);
    }

    #[test]
    fn a_reused_terminal_whose_shell_started_before_its_declaration_is_not_started_again() {
        let t = terminal("terminal#2");
        let d = Driven::hosted(&[t.clone()], &[]);
        d.started(&t.module);
        let id = t.module.clone();
        d.shell(move |layout| {
            kill(layout, &id, Reopen::At(Placement::BelowEditor), &frame()).unwrap();
        });
        d.ended(&t.module, Reopen::At(Placement::BelowEditor));
        d.show(&t.module);
        assert_eq!(d.ops(), vec![start(&t.module)]);
        d.started(&t.module);
        d.declare(&t);
        d.changed();
        assert_eq!(
            d.ops(),
            vec![start(&t.module)],
            "its running shell needs no second start"
        );
    }

    #[test]
    fn a_terminal_declared_after_the_change_that_showed_it_starts_its_shell() {
        let shown = terminal("terminal#2");
        let hidden = terminal("terminal#3");
        let d = Driven::open(
            &[shown.clone(), hidden.clone()],
            &[hidden.module.clone()],
            EditorPresence::Owned,
        );
        // The open's own change came first and found nothing declared.
        d.changed();
        assert!(d.ops().is_empty());
        d.declare(&shown);
        d.declare(&hidden);
        assert_eq!(d.ops(), vec![start(&shown.module)]);
        // Its shell runs: the next change starts nothing more, and a show of the other starts that one.
        d.started(&shown.module);
        d.show(&hidden.module);
        assert_eq!(d.since(1), vec![start(&hidden.module)]);
    }

    #[test]
    fn a_deferred_plugin_page_is_built_on_its_first_show() {
        let p = page("plugin", true, true);
        let d = Driven::open(&[p.clone()], &[p.module.clone()], EditorPresence::Owned);
        d.first_apply(vec![p.clone()]);
        assert_eq!(d.effects(), vec![on_screen(&p.module, false)]);
        d.show(&p.module);
        assert_eq!(
            d.effects_since(1),
            vec![plugin(&p.module, "build"), on_screen(&p.module, true)]
        );
        d.changed();
        assert_eq!(d.effects_since(3), vec![on_screen(&p.module, true)]);
    }

    #[test]
    fn the_startup_layout_gets_its_lifecycle_effects_without_a_save() {
        let shown = terminal("terminal");
        let hidden = terminal("terminal#2");
        let built = page("visible", true, true);
        let deferred = page("hidden", true, true);
        let decls = vec![shown.clone(), hidden.clone(), built.clone(), deferred.clone()];
        let d = Driven::open(
            &decls,
            &[hidden.module.clone(), deferred.module.clone()],
            EditorPresence::Owned,
        );
        // The saver would arm if startup were routed through the ordinary change path.
        d.workspaces
            .borrow_mut()
            .create("two", d.layout.borrow().clone())
            .unwrap();
        d.first_apply(decls);
        assert_eq!(
            d.effects(),
            vec![
                start(&shown.module),
                plugin(&built.module, "build"),
                on_screen(&built.module, true),
                on_screen(&deferred.module, false)
            ]
        );
        assert!(d.scheduler.armed().is_empty());
        assert_eq!(d.driver.query(|c| c.shell_revision(WIN)), 0);
        d.started(&shown.module);
        d.show(&hidden.module);
        assert_eq!(
            d.effects_since(4),
            vec![
                start(&hidden.module),
                on_screen(&built.module, true),
                on_screen(&deferred.module, false)
            ]
        );
        assert!(!d.scheduler.armed().is_empty());
    }

    #[test]
    fn effects_follow_the_change_never_precede_it() {
        let t = terminal("terminal");
        let p = page("plugin", true, true);
        let chat = chat();
        let d = Driven::hosted(&[t.clone(), p.clone(), chat.clone()], &[]);
        let ended = HostEvent::SurfaceEnded {
            win: WIN,
            module: chat.module.clone(),
            why: Ended::Killed {
                reopen: Reopen::InPlace,
            },
        };
        let map = HostOp::Map {
            win: WIN,
            module: t.module.clone(),
        };
        d.driver.test_batch(|c| {
            assert!(
                c.handle(ended).ops().all(is_cursor_signal),
                "a killed chat has nothing to stop"
            );
            let mut batch = c.handle(HostEvent::LayoutChangedByShell { win: WIN });
            // Layout ops joined after the change's effects were decided still apply before them.
            batch.push_layout(map);
            batch.push_layout(HostOp::Allocate { win: WIN });
            batch
        });
        assert_eq!(
            d.ops(),
            vec![
                HostOp::Map {
                    win: WIN,
                    module: t.module.clone()
                },
                HostOp::Allocate { win: WIN },
                start(&t.module),
                plugin(&p.module, "build"),
                on_screen(&p.module, true),
                HostOp::Chat(ChatOp::OpenChooser {
                    win: WIN,
                    view: ViewId::BASE
                })
            ]
        );
    }

    #[test]
    fn a_refused_surface_leaves_the_module_declared() {
        // Both are shown terminals, so only the refusal tells them apart; the refused one is created first.
        let refused = SurfaceDecl {
            kind: SurfaceKind::Terminal { cwd: "refused".into() },
            ..terminal("terminal#2")
        };
        let accepted = terminal("terminal");
        let d = Driven::open(&[refused.clone(), accepted.clone()], &[], EditorPresence::Owned);
        d.host
            .refuse_kind(|kind| matches!(kind, SurfaceKind::Terminal { cwd } if cwd == Path::new("refused")));
        let decls = [refused.clone(), accepted.clone()];
        d.driver.test_batch(|c| c.create_surfaces(WIN, &decls));
        assert_eq!(d.state(&refused.module), Some(ModuleState::Declared));
        assert_eq!(d.state(&accepted.module), Some(ModuleState::Hosted));
        assert_eq!(d.ops().len(), 2);
        d.changed();
        assert_eq!(
            d.since(2),
            vec![start(&accepted.module)],
            "only the created terminal starts"
        );
        assert_eq!(d.state(&refused.module), Some(ModuleState::Declared));
        assert_eq!(d.state(&accepted.module), Some(ModuleState::Shown));
    }

    #[test]
    fn a_borrowed_layout_keeps_host_facts_without_lifecycle_effects() {
        let t = terminal("terminal");
        let p = page("plugin", true, false);
        let d = Driven::hosted(&[t.clone(), p.clone()], &[]);
        let run = || {
            d.driver.query(|c| {
                c.lifecycle(WIN)
                    .unwrap()
                    .modules
                    .iter()
                    .find(|m| m.decl.module == t.module)
                    .unwrap()
                    .terminal
                    .clone()
            })
        };
        {
            let _borrow = d.layout.borrow_mut();
            d.started(&t.module);
            assert_eq!(run(), TermRun::Running { pid: 42 });
            d.exited(&t.module);
            assert_eq!(run(), TermRun::Exited);
            d.ended(&t.module, Reopen::At(Placement::BelowEditor));
            assert_eq!(run(), TermRun::NotStarted);
            d.ended(&p.module, Reopen::InPlace);
            d.changed();
            assert_eq!(d.state(&p.module), Some(ModuleState::Killed(Reopen::InPlace)));
            assert!(d.ops().is_empty());
        }
        d.changed();
        assert_eq!(
            d.ops(),
            vec![
                start(&t.module),
                on_screen(&p.module, true),
                plugin(&p.module, "reload")
            ]
        );
        assert_eq!(d.state(&t.module), Some(ModuleState::Shown));
    }

    #[test]
    fn terminals_start_in_tree_order_and_plugins_report_what_is_drawn() {
        let first = terminal("terminal#2");
        let second = terminal("terminal");
        let p = page("plugin", true, true);
        let d = Driven::open(&[first.clone(), second.clone(), p.clone()], &[], EditorPresence::Owned);
        d.layout.borrow_mut().toggle_zoom(&ModuleId::editor());
        // Declaration order differs from the layout; zoom changes drawing, not a module's shown state.
        d.first_apply(vec![p.clone(), second.clone(), first.clone()]);
        assert_eq!(
            d.effects(),
            vec![
                start(&first.module),
                start(&second.module),
                plugin(&p.module, "build"),
                on_screen(&p.module, false)
            ]
        );
        assert_eq!(d.state(&p.module), Some(ModuleState::Shown));
    }

    #[test]
    fn a_module_the_other_window_holds_is_away_and_no_effect_names_it_until_it_is_back() {
        let t = terminal("terminal");
        let p = page("plugin", true, false);
        let d = Driven::hosted(&[t.clone(), p.clone()], &[t.module.clone()]);
        let (term, page_id) = (t.module.clone(), p.module.clone());
        d.shell(move |layout| {
            layout.set_away(&term, true).unwrap();
            layout.set_away(&page_id, true).unwrap();
        });
        assert_eq!(d.state(&t.module), Some(ModuleState::Away));
        assert_eq!(d.state(&p.module), Some(ModuleState::Away));
        assert!(d.ops().is_empty(), "an away module gets no effect: {:?}", d.ops());
        d.changed();
        assert!(d.ops().is_empty(), "nor on a later change: {:?}", d.ops());

        let (term, page_id) = (t.module.clone(), p.module.clone());
        d.shell(move |layout| {
            layout.set_away(&term, false).unwrap();
            layout.set_away(&page_id, false).unwrap();
            layout.show(&term).unwrap();
        });
        assert_eq!(d.state(&t.module), Some(ModuleState::Shown));
        assert_eq!(
            d.state(&p.module),
            Some(ModuleState::Shown),
            "a page shown when it left is shown again"
        );
        assert_eq!(d.ops(), vec![start(&t.module), on_screen(&p.module, true)]);
    }

    #[test]
    fn an_away_module_keeps_a_kill_it_had() {
        let t = terminal("terminal");
        let d = Driven::hosted(&[t.clone()], &[]);
        let id = t.module.clone();
        d.shell(move |layout| {
            kill(layout, &id, Reopen::At(Placement::BelowEditor), &frame()).unwrap();
        });
        d.ended(&t.module, Reopen::At(Placement::BelowEditor));
        let id = t.module.clone();
        d.shell(move |layout| {
            layout.set_away(&id, true).unwrap();
        });
        assert_eq!(
            d.state(&t.module),
            Some(ModuleState::Killed(Reopen::At(Placement::BelowEditor)))
        );
        let id = t.module.clone();
        d.shell(move |layout| {
            layout.set_away(&id, false).unwrap();
        });
        d.show(&t.module);
        assert_eq!(
            d.ops(),
            vec![start(&t.module)],
            "the killed terminal starts a fresh shell when shown"
        );
    }

    #[test]
    fn redeclaring_a_deferred_page_forgets_its_previous_build() {
        let p = page("plugin", true, true);
        let d = Driven::open(&[p.clone()], &[], EditorPresence::Owned);
        d.first_apply(vec![p.clone()]);
        assert_eq!(
            d.effects(),
            vec![plugin(&p.module, "build"), on_screen(&p.module, true)]
        );
        d.ended(&p.module, Reopen::Remove);
        let before = d.effects().len();
        let decls = [p.clone()];
        d.driver.test_batch(|c| c.create_surfaces(WIN, &decls));
        d.changed();
        assert_eq!(
            d.effects_since(before),
            vec![
                HostOp::CreateSurface {
                    win: WIN,
                    module: p.module.clone(),
                    kind: p.kind.clone(),
                },
                plugin(&p.module, "build"),
                on_screen(&p.module, true)
            ]
        );
    }
}
