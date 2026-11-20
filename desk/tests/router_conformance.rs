//! The host's op router, over a back end that keeps its windows and surfaces as data, reproduces the GTK host's
//! record (`router_conformance.trace`) line for line: every conformance script and one op of every kind, with the
//! same collaborators recorded around the same stage, give the same calls, the same posted events and the same
//! answers. The record is the GTK host's (`gtk_host_conformance` holds it, and only it may write it), so the router
//! routes every op as that host does whatever back end it is given; a host for another toolkit implements the same
//! two traits. The back end here answers with the GTK host's refusal texts; only the Xvfb suite ties those to GTK.
//!
//! The stage here is the GTK suite's stage as data: one window, open; the fixture's three modules registered, the
//! hidden one not shown; a grid that queues allocations and restacks its floats; the chrome drawn; the first-start
//! screen and the project chooser taking the keys; no shell focus function, mode toggle, placer, application or
//! compositor adapter, as there.
//!
//! Needs no display.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::{Rc, Weak};

use eitri_core::layout::{Direction, ModuleId, Size};
use eitri_core::wm::conformance::Fixture;
use eitri_core::wm::grid::{restack_moves, Slot};
use eitri_core::wm::{
    ActivationToken, Applied, ChatOp, ChooserEnv, CleanupOp, DropWhy, EditorCall, HostEvent, HostSink, Monitor,
    PlacementRole, PluginOp, ProjectEvent, ProjectOp, SurfaceKind, TermOp, WinId, WindowSpec,
};

/// Where the router record's collaborator traits live for this host.
mod traits {
    pub(crate) use eitri_desk::host_traits::{
        BasePageMaker, BesidePort, GridPort, HintPort, LinkedWindows, LuaRunner, ProjectHost, RestartRefused,
        ServiceSet, SurfaceMaker,
    };
}

#[path = "support/router_record.rs"]
mod router_record;

use eitri_desk::host_traits::{EditorReads, GridPort, ProjectHost, ServiceSet, SurfaceMaker, NOT_CARRIED_OUT};
use eitri_desk::router::{Router, Views, WindowPort};

/// The stage's window and its modules, as data.
struct StageViews {
    win: WinId,
    /// The modules with a surface in the window, each with whether it is shown.
    surfaces: RefCell<BTreeMap<ModuleId, bool>>,
    /// The grid's slots, which the maker adds a new module's host to.
    slots: Rc<StageGrid>,
    /// The grid the router reaches: the slots, recorded.
    grid: RefCell<Option<Rc<dyn GridPort>>>,
}

impl Views for StageViews {
    fn attach(&self, _sink: Weak<dyn HostSink>) {}
    fn has_surface(&self, win: WinId, module: &ModuleId) -> bool {
        win == self.win && self.surfaces.borrow().contains_key(module)
    }
    fn grid(&self, win: WinId) -> Option<Rc<dyn GridPort>> {
        self.grid.borrow().clone().filter(|_| win == self.win)
    }
    fn editor(&self, _win: WinId) -> Option<Rc<dyn EditorReads>> {
        None
    }
    /// The bare grab: a module with no surface, or one not shown, cannot take the keys.
    fn focus_module(&self, win: WinId, module: &ModuleId) -> Result<(), String> {
        match (win == self.win)
            .then(|| self.surfaces.borrow().get(module).copied())
            .flatten()
        {
            None => Err("the module has no surface in this window".into()),
            Some(false) => Err("the module's surface is not on screen".into()),
            Some(true) => Ok(()),
        }
    }
    fn focus_top_bar(&self, _win: WinId, _item: u32) -> Result<(), String> {
        Err(NOT_CARRIED_OUT.into())
    }
    fn focus_rename_entry(&self, _win: WinId) -> Result<(), String> {
        Err(NOT_CARRIED_OUT.into())
    }
    fn focus_onboarding(&self, _win: WinId) -> Result<(), String> {
        Ok(())
    }
    fn grab_reported(&self, _win: WinId, grab: &dyn Fn() -> bool) -> bool {
        grab()
    }
    fn release_keys(&self, _win: WinId, _module: &ModuleId) {}
    fn show(&self, win: WinId, module: &ModuleId, shown: bool) {
        if win == self.win {
            if let Some(slot) = self.surfaces.borrow_mut().get_mut(module) {
                *slot = shown;
            }
        }
    }
    fn chrome(&self, win: WinId) -> Result<(), String> {
        if win == self.win {
            Ok(())
        } else {
            Err(NOT_CARRIED_OUT.into())
        }
    }
}

/// The stage's one window: open, and still held after a close, as the suite's own reference keeps the GTK one.
impl WindowPort for StageViews {
    fn attach(&self, _sink: Weak<dyn HostSink>) {}
    fn is_open(&self, win: WinId) -> bool {
        win == self.win
    }
    fn open(&self, win: WinId, _spec: &WindowSpec) -> Result<(), String> {
        if win == self.win {
            Err("the window is already open".into())
        } else {
            Err(NOT_CARRIED_OUT.into())
        }
    }
    fn present(&self, win: WinId, _activation: Option<&ActivationToken>) -> Result<(), String> {
        self.held(win)
    }
    fn close(&self, win: WinId) -> Result<(), String> {
        self.held(win)
    }
    fn toggle_mode(&self, _win: WinId) -> Result<(), String> {
        Err(NOT_CARRIED_OUT.into())
    }
    fn activation_token(&self, _win: WinId) -> Option<String> {
        None
    }
    fn place(&self, win: WinId, _connector: Option<&str>, _role: PlacementRole) -> Result<(), String> {
        self.held(win)?;
        Err(NOT_CARRIED_OUT.into())
    }
    fn compositor_focus(&self, _direction: Direction) -> Result<(), Option<String>> {
        Err(Some(NOT_CARRIED_OUT.into()))
    }
    fn compositor_name(&self) -> &'static str {
        "the compositor"
    }
    fn monitors(&self) -> Vec<Monitor> {
        Vec::new()
    }
}

impl StageViews {
    fn held(&self, win: WinId) -> Result<(), String> {
        if win == self.win {
            Ok(())
        } else {
            Err("no such window".into())
        }
    }
}

/// The grid's slots, the fixture's hosts first, then a strip and a divider; a module host made later goes below
/// them.
struct StageGrid {
    children: RefCell<Vec<Slot>>,
}

impl GridPort for StageGrid {
    fn size(&self) -> Size {
        eitri_core::wm::grid::UNALLOCATED
    }
    fn handle_px(&self) -> i32 {
        1
    }
    fn strip_px(&self) -> i32 {
        eitri_core::wm::grid::DEFAULT_STRIP_PX
    }
    fn module_min(&self, _module: &ModuleId) -> Size {
        Size::default()
    }
    fn queue_allocation(&self) {}
    fn restack_floats(&self, order: &[ModuleId]) {
        let mut children = self.children.borrow_mut();
        for step in restack_moves(&children, order, &[]) {
            children.retain(|slot| *slot != step.slot);
            let at = match &step.after {
                Some(after) => children.iter().position(|slot| slot == after).map_or(0, |i| i + 1),
                None => 0,
            };
            children.insert(at, step.slot);
        }
    }
    fn probe_places(&self, _module: &ModuleId) -> bool {
        false
    }
}

/// Builds each surface hidden and registers it, once: a module the grid holds keeps its host.
struct StageMaker {
    views: Rc<StageViews>,
}

impl SurfaceMaker for StageMaker {
    fn create(&self, win: WinId, module: &ModuleId, _kind: &SurfaceKind) -> Result<(), String> {
        let slot = Slot::Host(module.clone());
        let mut children = self.views.slots.children.borrow_mut();
        if children.contains(&slot) {
            return Ok(());
        }
        if win == self.views.win {
            self.views.surfaces.borrow_mut().insert(module.clone(), false);
        }
        let at = children
            .iter()
            .position(|slot| !matches!(slot, Slot::Host(_)))
            .unwrap_or(children.len());
        children.insert(at, slot);
        Ok(())
    }
    fn drop_surface(&self, _win: WinId, _module: &ModuleId, _why: &DropWhy) -> Result<(), String> {
        Ok(())
    }
    fn prepare(
        &self,
        win: WinId,
        module: &ModuleId,
        _shape: &eitri_core::window_link::Shape,
        _size: (u32, u32),
    ) -> Result<(), String> {
        let kind = match eitri_core::tabs::ViewId::of_module(module) {
            Some(view) => SurfaceKind::Chat { view },
            None => SurfaceKind::Terminal { cwd: ".".into() },
        };
        self.create(win, module, &kind)
    }
}

/// Carries out every service op, a join back excepted.
struct StageServices;

impl ServiceSet for StageServices {
    fn editor(&self, _op: &EditorCall) -> Result<(), String> {
        Ok(())
    }
    fn terminal(&self, _op: &TermOp) -> Result<(), String> {
        Ok(())
    }
    fn plugin(&self, _op: &PluginOp) -> Result<(), String> {
        Ok(())
    }
    fn chat(&self, op: &ChatOp) -> Result<(), String> {
        if matches!(op, ChatOp::JoinBack { .. }) {
            return Err("the tab could not join the base view".into());
        }
        Ok(())
    }
    fn cleanup(&self, _op: &CleanupOp) -> Result<(), String> {
        Ok(())
    }
}

/// Answers project work at once, as the suite's chooser does.
struct StageProjects {
    sink: Weak<dyn HostSink>,
}

impl ProjectHost for StageProjects {
    fn apply(&self, op: &ProjectOp) -> Applied {
        let answer = match op {
            ProjectOp::List { win, serial } => Some(ProjectEvent::Rows {
                win: *win,
                serial: *serial,
                rows: Vec::new(),
            }),
            ProjectOp::Read { win, serial, dir, .. } => Some(ProjectEvent::Listing {
                win: *win,
                serial: *serial,
                dir: dir.clone(),
                listing: eitri_core::project_chooser::Listing::Stuck,
            }),
            ProjectOp::FolderDialog { win, .. } => Some(ProjectEvent::Folder { win: *win, path: None }),
            ProjectOp::Open { .. } => None,
        };
        if let (Some(sink), Some(answer)) = (self.sink.upgrade(), answer) {
            sink.post(HostEvent::Project(answer));
        }
        Applied::Done
    }
    fn open(&self, _win: WinId, _target: &std::path::Path, _token: Option<String>) -> Applied {
        Applied::Done
    }
    fn focus(&self, _win: WinId) -> bool {
        true
    }
    fn env(&self, _win: WinId) -> ChooserEnv {
        ChooserEnv {
            home: None,
            lang: eitri_core::onboarding::Lang::En,
        }
    }
}

/// The stage, its collaborators recorded into `trace`, and the sink the router posts to (held while it runs).
fn stage(
    f: &Fixture,
    trace: &router_record::Trace,
    full: bool,
) -> (Rc<Router<StageViews, StageViews>>, Rc<dyn HostSink>) {
    use router_record::*;
    let grid = Rc::new(StageGrid {
        children: RefCell::new(
            [&f.a, &f.b, &f.hidden]
                .into_iter()
                .map(|module| Slot::Host(module.clone()))
                .chain([Slot::Strip(0), Slot::Handle(0)])
                .collect(),
        ),
    });
    let views = Rc::new(StageViews {
        win: f.win,
        surfaces: RefCell::new(BTreeMap::from([
            (f.a.clone(), true),
            (f.b.clone(), true),
            (f.hidden.clone(), false),
        ])),
        slots: grid.clone(),
        grid: RefCell::new(Some(Rc::new(RecGrid {
            inner: grid,
            trace: trace.clone(),
        }))),
    });
    let router = Router::new(views.clone(), views.clone());
    let sink: Rc<dyn HostSink> = TraceSink::new(trace);
    router.attach(Rc::downgrade(&sink));
    router.set_surface_maker(Rc::new(RecMaker {
        inner: StageMaker { views: views.clone() },
        trace: trace.clone(),
    }));
    router.set_services(Rc::new(RecServices {
        inner: StageServices,
        trace: trace.clone(),
    }));
    router.set_project_host(Rc::new(RecProjects {
        inner: StageProjects {
            sink: Rc::downgrade(&sink),
        },
        trace: trace.clone(),
    }));
    if full {
        router.set_lua(Rc::new(RecLua(trace.clone())));
        router.set_linked_windows(Rc::new(RecLinked(trace.clone())));
        router.set_hints(Rc::new(RecHints(trace.clone())));
        router.set_beside(Rc::new(RecBeside(trace.clone())));
        router.set_base_page_maker(Rc::new(RecBasePage(trace.clone())));
        router.set_onboarding(rec_onboarding(trace));
    }
    (router, sink)
}

#[test]
fn the_router_writes_the_gtk_hosts_record() {
    let f = Fixture {
        win: WinId::MAIN,
        a: ModuleId::agent(),
        b: ModuleId::terminal(),
        hidden: ModuleId::editor(),
    };
    let trace: router_record::Trace = Rc::default();
    for (full, script) in router_record::record_runs(&f) {
        let (router, _sink) = stage(&f, &trace, full);
        router_record::record(router.as_ref(), &trace, router_record::config(full), &script);
    }
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join(router_record::TRACE_FILE);
    if let Err(why) = router_record::check_trace(&trace, &path, false) {
        panic!("{why}");
    }
}
