//! A window held by the controller over the fake host, for the runner's tests: its layout shared, its hosts
//! declared by the first apply, the fake's reads given to the controller.

use crate::keymap::Action;
use crate::layout::{Layout, ModuleId, ModuleKind, Workspaces};
use crate::tabs::ViewId;
use crate::wm::fake::{FakeHost, FakeScheduler};
use crate::wm::module::SurfaceDecl;
use crate::wm::persist::Saver;
use crate::wm::*;
use std::cell::RefCell;
use std::rc::Rc;

pub(crate) const WIN: WinId = WinId::MAIN;

/// A surface of the kind `module`'s id names.
pub(crate) fn decl(module: &ModuleId) -> SurfaceDecl {
    let kind = match module.kind() {
        ModuleKind::Editor => SurfaceKind::Editor {
            spawn: EditorSpawn {
                env: vec![],
                args: vec![],
                cwd: None,
                clean: true,
            },
        },
        ModuleKind::Agent => SurfaceKind::Chat {
            view: ViewId::of_module(module).expect("a chat module"),
        },
        ModuleKind::Terminal => SurfaceKind::Terminal { cwd: ".".into() },
        ModuleKind::LuaWebview | ModuleKind::Canvas => SurfaceKind::LuaPanel {
            url: format!("eitri://{module}"),
            title: module.to_string(),
        },
    };
    SurfaceDecl {
        module: module.clone(),
        kind,
        deferred: false,
    }
}

pub(crate) struct Rig {
    pub(crate) host: Rc<FakeHost>,
    pub(crate) scheduler: Rc<FakeScheduler>,
    pub(crate) controller: Controller,
    pub(crate) layout: Rc<RefCell<Layout>>,
    /// The first apply's batch.
    pub(crate) first: Batch,
}

/// A controller holding `layout` as the main window, every module of it hosted, with `saver` when given.
pub(crate) fn rig_with(layout: Layout, saver: impl FnOnce(&Layout, &Workspaces) -> Option<Saver>) -> Rig {
    let host = FakeHost::new();
    let scheduler = FakeScheduler::new();
    let mut controller = Controller::new(scheduler.clone());
    controller.set_reads(host.clone());
    let workspaces = Workspaces::single();
    let saver = saver(&layout, &workspaces);
    let hosted: Vec<SurfaceDecl> = layout.modules().iter().map(decl).collect();
    let layout = Rc::new(RefCell::new(layout));
    controller.open_window(
        WIN,
        WindowSpec {
            role: WindowRole::Main,
            presence: EditorPresence::Owned,
            title: "project".into(),
        },
        WindowParts {
            layout: layout.clone(),
            workspaces: Rc::new(RefCell::new(workspaces)),
            saver,
        },
    );
    let first = controller.first_apply(WIN, &hosted);
    Rig {
        host,
        scheduler,
        controller,
        layout,
        first,
    }
}

pub(crate) fn rig(layout: Layout) -> Rig {
    rig_with(layout, |_, _| None)
}

impl Rig {
    pub(crate) fn run(&mut self, action: Action) -> Batch {
        self.controller.run(WIN, action, Origin::Key { keycode: 7 })
    }
}

/// A batch's native and service ops, layout first.
pub(crate) fn ops(batch: &Batch) -> Vec<HostOp> {
    batch.ops().cloned().collect()
}

/// A batch's layout ops only: what moves hosts on screen.
pub(crate) fn moves(batch: &Batch) -> Vec<HostOp> {
    batch
        .ops()
        .filter(|op| {
            matches!(
                op,
                HostOp::Map { .. }
                    | HostOp::Unmap { .. }
                    | HostOp::Focus { .. }
                    | HostOp::Restack { .. }
                    | HostOp::Allocate { .. }
            )
        })
        .cloned()
        .collect()
}

pub(crate) fn map(module: &ModuleId) -> HostOp {
    HostOp::Map {
        win: WIN,
        module: module.clone(),
    }
}

pub(crate) fn unmap(module: &ModuleId) -> HostOp {
    HostOp::Unmap {
        win: WIN,
        module: module.clone(),
    }
}

pub(crate) fn allocate() -> HostOp {
    HostOp::Allocate { win: WIN }
}

pub(crate) fn refuse(text: &str) -> HostOp {
    HostOp::Refuse {
        win: WIN,
        text: text.into(),
    }
}

pub(crate) fn chrome_hooks() -> HostOp {
    HostOp::ChromeHooks { win: WIN }
}
