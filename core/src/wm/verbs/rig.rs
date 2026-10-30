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
                background: None,
                open: vec![],
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

    /// A tab key with the keys in `keys`.
    pub(crate) fn run_tab(&mut self, tab: crate::keymap::TabAction, keys: Option<ModuleId>) -> Batch {
        self.controller.run_tab(WIN, tab, keys)
    }
}

/// A batch's native and service ops, layout first, but the chrome's announcement: what the chrome shows is
/// read from the model, and the chrome's own tests count its announcements.
pub(crate) fn ops(batch: &Batch) -> Vec<HostOp> {
    batch
        .ops()
        .filter(|op| !matches!(op, HostOp::Chrome { .. }))
        .cloned()
        .collect()
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

/// Whether `batch` announces the chrome: some field's value changed.
pub(crate) fn announced(batch: &Batch) -> bool {
    batch.ops().any(|op| matches!(op, HostOp::Chrome { .. }))
}

/// What the chrome says of refusals at one moment: how often the app name has flashed, and the toast up with its
/// serial.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Said {
    pub(crate) flash: u64,
    pub(crate) toast: Option<(u64, String)>,
}

impl Rig {
    /// What the chrome says of refusals now.
    pub(crate) fn said(&self) -> Said {
        let chrome = self.controller.chrome(WIN);
        Said {
            flash: chrome.top_bar.flash,
            toast: chrome.toast.map(|toast| (toast.serial, toast.text)),
        }
    }

    /// `batch` is a refusal as the chrome draws one: nothing in it but the chrome's announcement, the app name
    /// flashed once more than `before`, and a new toast saying `text`, or for an empty `text` the toast left as it
    /// was.
    pub(crate) fn assert_refused(&self, batch: &Batch, before: &Said, text: &str) {
        assert!(ops(batch).is_empty(), "a refusal moves nothing: {:#?}", ops(batch));
        assert!(announced(batch), "a refusal announces the chrome it changed");
        let now = self.said();
        assert_eq!(now.flash, before.flash + 1, "the app name flashed once");
        if text.is_empty() {
            assert_eq!(now.toast, before.toast, "a bare flash says nothing on the toast");
        } else {
            assert_eq!(now.toast.as_ref().map(|(_, said)| said.as_str()), Some(text));
            assert_ne!(
                now.toast.map(|(serial, _)| serial),
                before.toast.as_ref().map(|(serial, _)| *serial),
                "each refusal is a new toast"
            );
        }
    }
}
