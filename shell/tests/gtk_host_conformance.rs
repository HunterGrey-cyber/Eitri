//! The GTK host carries out the window contract's ops the way every host must, in a real GTK window.
//!
//! **What this holds.** GTK emits signals while an op is still running: `grab_focus` notifies
//! `focus-widget` before it returns, and `close()` emits `close-request` from inside itself. Hiding
//! the widget that holds the keys is the opposite case: GTK 4.24 leaves the window's focus on a
//! child-invisible widget and notifies nothing, so the host clears it itself. The contract's conformance scripts
//! (`eitri_core::wm::conformance`) state what a host must report while each op applies; this drives
//! the product's own `GtkHost` (compiled in from `src/` below) through every one of them, then checks
//! the one path no script covers: a close the user asks for reaches the sink's `close_requested`
//! exactly once, while the host's own `CloseWindow` never does. A second `Focus` on the widget that
//! already holds the keys must still report where they are, though GTK notifies nothing then. A
//! `CloseWindow` the sink applies while it answers that very close request must still close the
//! window, though GTK ignores `close()` from inside its own close-request emission. And a closed
//! window must not outlive its close in the host, which would keep every window ever closed.
//!
//! `SetMode` is carried out through the shell's mode toggle, once per op, whatever state it names.
//!
//! A window's start adds three: `OpenWindow` builds the window for the application set on the host,
//! created and not shown, and adopts it (a second one for the same window is refused); `Present` shows
//! it; and a `Focus` goes through the shell's own focus function once one is set, answering what it
//! answered.
//!
//! The scripts that create and drop surfaces run with a stub surface maker and a stub service set
//! (what the shell's binary gives the host): the stub builds a focusable widget in the window, off
//! screen, and registers it, so what is checked is the host's half -- the answer, the registration,
//! and a dropped surface still being the module's. The scripts that allocate and restack run with a
//! stub grid, which counts the allocations it is asked for and stacks its hosts with the grid's own
//! rule (`eitri_core::wm::grid::restack_moves`) over a child list shaped as the grid's: hosts below a
//! strip and a divider.
//!
//! Needs a display, and never the real desktop: **it starts its own** (`support/own_x_server.rs`) --
//! an `Xvfb` with a 1024x768 screen, pointed to before GTK initialises, with any inherited
//! `WAYLAND_DISPLAY`/`DISPLAY` dropped --
//!
//!     cargo test -p shell --test gtk_host_conformance -- --ignored
//!
//! A plain `main` (`harness = false`), because GTK must own the main thread. About 2s.

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};
use std::time::{Duration, Instant};

use eitri_core::layout::{ModuleId, Size};
use eitri_core::wm::conformance::{self, EventMatch, Expect, Fixture, GridObserve, RecordingSink, Script};
use eitri_core::wm::grid::{restack_moves, Slot};
use eitri_core::wm::{
    Applied, ChatOp, CleanupOp, CloseBy, CloseVerdict, DropWhy, EditorCall, EditorPresence, FocusTo, Host, HostOp,
    HostSink, PluginOp, SurfaceKind, TermOp, WinId, WindowMode, WindowRole, WindowSpec,
};
use gtk4::prelude::*;

// The display this test runs on: its own Xvfb, never an inherited one.
#[path = "support/own_x_server.rs"]
mod own_x_server;

// The modules' own `#[cfg(test)] mod tests` come along, their `#[test]`s dropped for want of a
// harness, and their `use super::*` with them.
#[allow(unused_imports)]
#[path = "../src/gtk_host/mod.rs"]
mod gtk_host;

#[allow(dead_code, unused_imports)]
#[path = "../src/editor_hole.rs"]
mod editor_hole;
#[allow(dead_code, unused_imports)]
#[path = "../src/module_grid.rs"]
mod module_grid;
#[allow(dead_code, unused_imports)]
#[path = "../src/web_host.rs"]
mod web_host;

fn pump(ms: u64) {
    let context = gtk4::glib::MainContext::default();
    let end = Instant::now() + Duration::from_millis(ms);
    while Instant::now() < end {
        while context.iteration(false) {}
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// Builds each surface as a focusable button in the window's column, off screen until a `Map`, and adds its host
/// to the grid. A module whose host the grid already holds keeps it, as the shell's own maker keeps one: a host never
/// gets a second set of controllers.
struct StubMaker {
    host: Weak<gtk_host::GtkHost>,
    column: gtk4::glib::WeakRef<gtk4::Box>,
    grid: Weak<StubGrid>,
}

impl gtk_host::SurfaceMaker for StubMaker {
    fn create(&self, win: WinId, module: &ModuleId, _kind: &SurfaceKind) -> Result<(), String> {
        let (Some(host), Some(column), Some(grid)) = (self.host.upgrade(), self.column.upgrade(), self.grid.upgrade())
        else {
            return Err("the stage is gone".into());
        };
        let slot = Slot::Host(module.clone());
        if grid.children.borrow().contains(&slot) {
            return Ok(());
        }
        let button = gtk4::Button::with_label(module.as_str());
        button.set_focusable(true);
        column.append(&button);
        button.set_child_visible(false);
        host.register_surface(win, module.clone(), button.upcast_ref());
        let mut children = grid.children.borrow_mut();
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
    /// An arriving terminal's or chat view's place is built as its create builds that kind's host: off screen,
    /// registered.
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

/// Carries out every service op by doing nothing.
struct StubServices;

impl gtk_host::ServiceSet for StubServices {
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

/// The window's grid as the host drives it: its children as slots, the fixture's hosts first, then a strip and
/// a divider, as the grid puts every host below them. It also answers whether the stage's window is still open, for
/// the scripts that close it.
struct StubGrid {
    children: RefCell<Vec<Slot>>,
    allocations: Cell<usize>,
    window: RefCell<Option<gtk4::glib::WeakRef<gtk4::Window>>>,
}

impl StubGrid {
    fn new(f: &Fixture) -> Rc<StubGrid> {
        let mut children: Vec<Slot> = [&f.a, &f.b, &f.hidden]
            .into_iter()
            .map(|module| Slot::Host(module.clone()))
            .collect();
        children.extend([Slot::Strip(0), Slot::Handle(0)]);
        Rc::new(StubGrid {
            children: RefCell::new(children),
            allocations: Cell::new(0),
            window: RefCell::new(None),
        })
    }
}

impl gtk_host::GridPort for StubGrid {
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
    fn queue_allocation(&self) {
        self.allocations.set(self.allocations.get() + 1);
    }
    /// Each move puts its slot right after the one it names, the first child for none.
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

impl GridObserve for StubGrid {
    fn allocations_asked(&self, _win: WinId) -> usize {
        self.allocations.get()
    }
    fn window_open(&self, _win: WinId) -> bool {
        self.window
            .borrow()
            .as_ref()
            .and_then(|window| window.upgrade())
            .is_some_and(|window| window.is_visible())
    }
    fn stacking(&self, _win: WinId) -> Vec<ModuleId> {
        self.children
            .borrow()
            .iter()
            .filter_map(|slot| match slot {
                Slot::Host(module) => Some(module.clone()),
                _ => None,
            })
            .collect()
    }
}

/// A presented window holding three focusable widgets, registered as the fixture's modules, the
/// `hidden` one child-invisible; nothing holds the keys when it is handed back, so the first grab
/// changes focus and GTK notifies it.
struct Stage {
    host: Rc<gtk_host::GtkHost>,
    sink: Rc<RecordingSink>,
    grid: Rc<StubGrid>,
    window: gtk4::Window,
    /// The fixture's widgets in order: `a`, `b`, `hidden`.
    widgets: Vec<gtk4::Widget>,
    /// What the window's chrome widgets were told, one line per setter called.
    chrome: Rc<RefCell<Vec<String>>>,
    /// The first-start screen, in the window and hidden until something shows it.
    screen: Rc<gtk_host::onboarding::OnboardingScreen>,
}

/// The model every `Chrome` op on the stage reads: the project's name, a chip up, and one toast.
fn stage_model() -> eitri_core::wm::chrome::ChromeModel {
    eitri_core::wm::chrome::ChromeModel {
        top_bar: eitri_core::wm::chrome::TopBar {
            project: "conformance".into(),
            ..Default::default()
        },
        tray: vec![eitri_core::wm::chrome::Chip {
            module: ModuleId::terminal(),
            label: "terminal".into(),
            shown: true,
            attention: false,
            stowed: false,
        }],
        toast: Some(eitri_core::wm::chrome::Toast {
            text: "no last module".into(),
            kind: eitri_core::wm::toasts::ToastKind::Other,
            serial: 1,
        }),
        ..Default::default()
    }
}

/// Chrome widgets that write down each setter called, the way the stage reads them back.
fn recording_widgets(
    heard: &Rc<RefCell<Vec<String>>>,
    screen: &Rc<gtk_host::onboarding::OnboardingScreen>,
) -> gtk_host::chrome::ChromeWidgets {
    let say = |heard: &Rc<RefCell<Vec<String>>>| {
        let heard = heard.clone();
        move |line: String| heard.borrow_mut().push(line)
    };
    gtk_host::chrome::ChromeWidgets {
        other: Box::new(|_| {}),
        workspace: Box::new({
            let say = say(heard);
            move |project, workspace| say(format!("workspace {project} {workspace:?}"))
        }),
        selection: Box::new({
            let say = say(heard);
            move |text| say(format!("selection {text:?}"))
        }),
        mark: Box::new({
            let say = say(heard);
            move |text| say(format!("mark {text:?}"))
        }),
        flash: Box::new({
            let say = say(heard);
            move || say("flash".into())
        }),
        chip: Box::new({
            let say = say(heard);
            move |chip| say(format!("chip {} shown={}", chip.label, chip.shown))
        }),
        strips: Box::new({
            let say = say(heard);
            move |strips| say(format!("strips {}", strips.len()))
        }),
        float_bars: Box::new({
            let say = say(heard);
            move |bars| say(format!("float bars {}", bars.len()))
        }),
        prefix_strip: Box::new({
            let say = say(heard);
            move |pieces| say(format!("prefix strip {}", pieces.is_some()))
        }),
        prefix_panel: Box::new({
            let say = say(heard);
            move |panel| say(format!("prefix panel {}", panel.is_some()))
        }),
        toast: Box::new({
            let say = say(heard);
            move |toast| say(format!("toast {} {}", toast.serial, toast.text))
        }),
        top_bar_revealed: Box::new({
            let say = say(heard);
            move |revealed| say(format!("revealed {revealed}"))
        }),
        prompt: gtk_host::chrome::PromptWidgets {
            show: Box::new({
                let say = say(heard);
                move |text| say(format!("prompt {text}"))
            }),
            measures: Box::new(gtk_host::chrome::PromptMeasures::default),
            place: Box::new(|_, _| {}),
            hide: Box::new({
                let say = say(heard);
                move || say("prompt hidden".into())
            }),
        },
        rename: gtk_host::chrome::RenameWidgets {
            open: Box::new({
                let say = say(heard);
                move |text| say(format!("rename {text}"))
            }),
            refill: Box::new({
                let say = say(heard);
                move |text| say(format!("rename refilled {text}"))
            }),
            close: Box::new({
                let say = say(heard);
                move || say("rename closed".into())
            }),
        },
        onboarding: Box::new({
            let screen = screen.clone();
            move |view| screen.show(view)
        }),
    }
}

fn stage(f: &Fixture) -> Result<Stage, String> {
    let host = gtk_host::GtkHost::new();
    let sink = Rc::new(RecordingSink::default());
    let erased: Rc<dyn HostSink> = sink.clone();
    host.attach(Rc::downgrade(&erased));

    let window = gtk4::Window::new();
    window.set_default_size(600, 300);
    let column = gtk4::Box::new(gtk4::Orientation::Vertical, 4);
    let mut buttons = Vec::new();
    for module in [&f.a, &f.b, &f.hidden] {
        let button = gtk4::Button::with_label(module.as_str());
        button.set_focusable(true);
        column.append(&button);
        host.register_surface(f.win, module.clone(), button.upcast_ref());
        buttons.push(button);
    }
    buttons[2].set_child_visible(false);
    let grid = StubGrid::new(f);
    *grid.window.borrow_mut() = Some(window.downgrade());
    host.set_surface_maker(Rc::new(StubMaker {
        host: Rc::downgrade(&host),
        column: column.downgrade(),
        grid: Rc::downgrade(&grid),
    }));
    host.set_services(Rc::new(StubServices));
    host.set_grid(f.win, grid.clone());
    let chrome: Rc<RefCell<Vec<String>>> = Rc::default();
    host.set_chrome_model(Rc::new(|_| Some(stage_model())));
    let screen = gtk_host::onboarding::OnboardingScreen::new();
    column.append(screen.widget());
    {
        let screen = screen.clone();
        host.set_onboarding_focus(Rc::new(move |_| screen.grab_keys()));
    }
    host.set_chrome_widgets(f.win, recording_widgets(&chrome, &screen));
    window.set_child(Some(&column));
    host.adopt_window(f.win, &window);
    window.present();
    pump(300);
    if !buttons[0].is_mapped() || !buttons[1].is_mapped() {
        return Err("the window's visible widgets were never mapped".into());
    }
    if buttons[2].is_mapped() {
        return Err("the child-invisible widget is mapped".into());
    }
    GtkWindowExt::set_focus(&window, None::<&gtk4::Widget>);
    pump(50);
    if GtkWindowExt::focus(&window).is_some() {
        return Err("the window kept a focus widget after it was cleared".into());
    }
    let widgets = buttons.into_iter().map(|b| b.upcast()).collect();
    Ok(Stage {
        host,
        sink,
        grid,
        window,
        widgets,
        chrome,
        screen,
    })
}

fn finish(stage: Stage) {
    if stage.window.is_visible() {
        stage.window.destroy();
    }
    pump(50);
}

/// Scripts this host adds to the shared ones: behaviour GTK's signals alone would not give.
fn own_scripts(f: &Fixture) -> Vec<Script> {
    let focus = |module: &ModuleId| HostOp::Focus {
        win: f.win,
        to: FocusTo::Module(module.clone()),
        on_grab: vec![],
    };
    vec![Script {
        name: "focus_on_the_current_owner_still_reports_native_focus",
        ops: vec![focus(&f.a), focus(&f.a)],
        expect: vec![
            Expect::Answer {
                op: 1,
                applied: Applied::Done,
            },
            Expect::PostedDuring {
                op: 1,
                event: EventMatch::NativeFocusOn(Some(f.a.clone())),
            },
        ],
    }]
}

fn outside_close(f: &Fixture) -> Result<(), String> {
    let s = stage(f)?;
    s.window.close();
    pump(50);
    let asked = s.sink.close_requests();
    if asked != 1 {
        return Err(format!("a close from outside asked the sink {asked} times, not once"));
    }
    if s.window.is_visible() {
        return Err("the sink answered Proceed and the window is still shown".into());
    }
    finish(s);

    let s = stage(f)?;
    let answer = s.host.apply(&HostOp::CloseWindow { win: f.win });
    pump(50);
    if answer != Applied::Done {
        return Err(format!("CloseWindow answered {answer:?}"));
    }
    if s.sink.close_requests() != 0 {
        return Err("CloseWindow asked the sink whether it may close".into());
    }
    if s.window.is_visible() {
        return Err("CloseWindow left the window shown".into());
    }
    finish(s);
    Ok(())
}

/// A sink that keeps the window open: a user's close it stops stays stopped, and the host's own
/// close afterwards still goes through without asking.
struct StopSink(RecordingSink);

impl HostSink for StopSink {
    fn post(&self, event: eitri_core::wm::HostEvent) {
        self.0.post(event);
    }
    fn post_deferred(&self, event: eitri_core::wm::HostEvent) {
        self.0.post_deferred(event);
    }
    fn close_requested(&self, win: WinId, by: CloseBy) -> CloseVerdict {
        self.0.close_requested(win, by);
        CloseVerdict::Stop
    }
}

fn stopped_close(f: &Fixture) -> Result<(), String> {
    let s = stage(f)?;
    let stopper = Rc::new(StopSink(RecordingSink::default()));
    let erased: Rc<dyn HostSink> = stopper.clone();
    s.host.attach(Rc::downgrade(&erased));
    s.window.close();
    pump(50);
    if stopper.0.close_requests() != 1 || !s.window.is_visible() {
        return Err("a close the sink stopped did not keep the window open".into());
    }
    if s.host.apply(&HostOp::CloseWindow { win: f.win }) != Applied::Done || s.window.is_visible() {
        return Err("CloseWindow after a stopped close did not close the window".into());
    }
    if stopper.0.close_requests() != 1 {
        return Err("CloseWindow asked the sink after a stopped close".into());
    }
    finish(s);
    Ok(())
}

/// A sink that answers a close request by closing the window itself, from inside the request, and
/// by asking to keep it open: the host's own close wins over the verdict it was asked for.
struct CloseInsideSink {
    inner: RecordingSink,
    host: Weak<gtk_host::GtkHost>,
    win: WinId,
    answer: Cell<Option<Applied>>,
}

impl HostSink for CloseInsideSink {
    fn post(&self, event: eitri_core::wm::HostEvent) {
        self.inner.post(event);
    }
    fn post_deferred(&self, event: eitri_core::wm::HostEvent) {
        self.inner.post_deferred(event);
    }
    fn close_requested(&self, win: WinId, by: CloseBy) -> CloseVerdict {
        self.inner.close_requested(win, by);
        let answer = self
            .host
            .upgrade()
            .map(|host| host.apply(&HostOp::CloseWindow { win: self.win }));
        self.answer.set(answer);
        CloseVerdict::Stop
    }
}

fn close_window_inside_the_request(f: &Fixture) -> Result<(), String> {
    let s = stage(f)?;
    let sink = Rc::new(CloseInsideSink {
        inner: RecordingSink::default(),
        host: Rc::downgrade(&s.host),
        win: f.win,
        answer: Cell::new(None),
    });
    let erased: Rc<dyn HostSink> = sink.clone();
    s.host.attach(Rc::downgrade(&erased));
    s.window.close();
    pump(50);
    if sink.inner.close_requests() != 1 {
        return Err(format!(
            "a close from outside asked the sink {} times, not once",
            sink.inner.close_requests()
        ));
    }
    if sink.answer.get() != Some(Applied::Done) {
        return Err(format!(
            "CloseWindow inside the close request answered {:?}",
            sink.answer.get()
        ));
    }
    if s.window.is_visible() {
        return Err("CloseWindow inside the close request answered Done and left the window shown".into());
    }
    finish(s);
    Ok(())
}

/// Once a window has closed, by `CloseWindow` or by a close from outside that the sink let through,
/// the host holds neither it nor its surfaces: only the test's own references kept it alive, and
/// they are dropped first.
fn closed_windows_are_forgotten(f: &Fixture) -> Result<(), String> {
    for by_op in [true, false] {
        let Stage {
            host,
            sink,
            grid: _,
            window,
            widgets,
            chrome: _,
            screen,
        } = stage(f)?;
        drop(widgets);
        drop(screen);
        if by_op {
            if host.apply(&HostOp::CloseWindow { win: f.win }) != Applied::Done {
                return Err("CloseWindow on a shown window was refused".into());
            }
        } else {
            window.close();
        }
        drop(window);
        pump(50);
        if host.knows_window(f.win) {
            let how = if by_op { "CloseWindow" } else { "a close from outside" };
            return Err(format!("the host still holds the window or its surfaces after {how}"));
        }
        drop(sink);
    }
    Ok(())
}

/// After `Unmap` of the surface holding the keys, GTK's own focus is outside it: keys typed next do
/// not reach a widget no one can see.
fn unmapped_owner_keeps_no_keys(f: &Fixture) -> Result<(), String> {
    let s = stage(f)?;
    let focus = HostOp::Focus {
        win: f.win,
        to: FocusTo::Module(f.a.clone()),
        on_grab: vec![],
    };
    let unmap = HostOp::Unmap {
        win: f.win,
        module: f.a.clone(),
    };
    if s.host.apply(&focus) != Applied::Done || s.host.apply(&unmap) != Applied::Done {
        return Err("focus then unmap of a visible module was refused".into());
    }
    pump(50);
    let a = &s.widgets[0];
    if GtkWindowExt::focus(&s.window).is_some_and(|w| w == *a || w.is_ancestor(a)) {
        return Err("the window's focus stayed on the hidden surface".into());
    }
    finish(s);
    Ok(())
}

/// `OpenWindow` builds the window, adopted and not shown, and refuses a second for the same window; `Present`
/// shows it.
fn open_window_then_present(f: &Fixture) -> Result<(), String> {
    let app = gtk4::Application::builder()
        .application_id("cn.huntergrey.eitri.test.gtkhostconformance")
        .flags(gtk4::gio::ApplicationFlags::NON_UNIQUE)
        .build();
    app.register(None::<&gtk4::gio::Cancellable>)
        .map_err(|e| e.to_string())?;
    let host = gtk_host::GtkHost::new();
    let sink = Rc::new(RecordingSink::default());
    let erased: Rc<dyn HostSink> = sink.clone();
    host.attach(Rc::downgrade(&erased));
    let open = HostOp::OpenWindow {
        win: f.win,
        spec: WindowSpec {
            role: WindowRole::Main,
            presence: EditorPresence::Owned,
            title: "conformance".into(),
        },
    };
    if host.apply(&open) != Applied::Refused {
        return Err("OpenWindow with no application set was carried out".into());
    }
    host.set_application(&app);
    if host.apply(&open) != Applied::Done {
        return Err("OpenWindow was refused".into());
    }
    let window = host.window(f.win).ok_or("the opened window is not the host's")?;
    if !window.is::<gtk4::ApplicationWindow>() || window.title().as_deref() != Some("conformance") {
        return Err("the opened window is not the application's window with its title".into());
    }
    if window.is_visible() {
        return Err("OpenWindow showed the window".into());
    }
    if host.apply(&open) != Applied::Refused {
        return Err("a second OpenWindow for an open window was carried out".into());
    }
    let present = HostOp::Present {
        win: f.win,
        activation: None,
    };
    if host.apply(&present) != Applied::Done {
        return Err("Present was refused".into());
    }
    pump(100);
    if !window.is_visible() {
        return Err("Present did not show the window".into());
    }
    window.destroy();
    // The window disposes, and the host forgets it, once the last reference to it is gone.
    drop(window);
    pump(50);
    if host.knows_window(f.win) {
        return Err("the host still holds the destroyed window".into());
    }
    Ok(())
}

/// With the shell's focus function set, `Focus` asks it, not a bare grab: its yes is `Done` and its no `Refused`.
fn focus_goes_through_the_shells_function(f: &Fixture) -> Result<(), String> {
    let s = stage(f)?;
    let asked: Rc<std::cell::RefCell<Vec<ModuleId>>> = Rc::default();
    let grants = Rc::new(Cell::new(true));
    {
        let asked = asked.clone();
        let grants = grants.clone();
        let b = s.widgets[1].clone();
        s.host.set_focus(Rc::new(move |module: &ModuleId| {
            asked.borrow_mut().push(module.clone());
            // Where a shell's own function sends the keys is its own business: here, always to `b`.
            grants.get() && b.grab_focus()
        }));
    }
    let focus = HostOp::Focus {
        win: f.win,
        to: FocusTo::Module(f.a.clone()),
        on_grab: vec![],
    };
    if s.host.apply(&focus) != Applied::Done {
        return Err("a Focus the shell's function granted was refused".into());
    }
    grants.set(false);
    if s.host.apply(&focus) != Applied::Refused {
        return Err("a Focus the shell's function refused was carried out".into());
    }
    if *asked.borrow() != vec![f.a.clone(), f.a.clone()] {
        return Err(format!("the shell's function was asked {:?}", asked.borrow()));
    }
    if GtkWindowExt::focus(&s.window).as_ref() != Some(&s.widgets[1]) {
        return Err("the keys are not where the shell's function put them".into());
    }
    finish(s);
    Ok(())
}

/// `SetMode` goes through the shell's mode toggle, once per op, whatever state the op names: the window's real mode is
/// the shell's, which a bare F11 also changes, so the op is carried out as the toggle that asked for it. Until the
/// toggle is set, a `SetMode` is refused as not carried out.
fn gtk_host_sets_the_mode_it_is_asked_for(f: &Fixture) -> Result<(), String> {
    let s = stage(f)?;
    let op = HostOp::SetMode {
        win: f.win,
        mode: WindowMode {
            immersive: true,
            fullscreen: true,
            top_bar_revealed: false,
        },
    };
    if s.host.apply(&op) != Applied::Refused {
        return Err("a SetMode with no mode toggle set was carried out".into());
    }
    let toggled: Rc<RefCell<Vec<WinId>>> = Rc::default();
    {
        let toggled = toggled.clone();
        s.host
            .set_mode_toggle(Rc::new(move |win: WinId| toggled.borrow_mut().push(win)));
    }
    if s.host.apply(&op) != Applied::Done {
        return Err("a SetMode with the mode toggle set was refused".into());
    }
    if *toggled.borrow() != vec![f.win] {
        return Err(format!("the mode was toggled {:?}", toggled.borrow()));
    }
    // The same state asked again toggles again: what is carried out is the toggle, not the state.
    if s.host.apply(&op) != Applied::Done || toggled.borrow().len() != 2 {
        return Err(format!("a second SetMode toggled {:?}", toggled.borrow()));
    }
    finish(s);
    Ok(())
}

/// What the controller's top-bar focus and digit ops reach: each the function the shell set for it, answering
/// what that function answered, and nothing before one is set. A `Chrome` op reads the window's model and draws
/// it on the window's widgets: everything the first time, then only what changed, which for the same model is
/// nothing.
fn top_bar_chrome_and_hints_go_through_the_shells_functions(f: &Fixture) -> Result<(), String> {
    struct Hints(RefCell<Vec<String>>);
    impl gtk_host::HintPort for Hints {
        fn show_digits(&self, _win: WinId, digits: &[(ModuleId, u8)]) {
            self.0.borrow_mut().push(format!("show {}", digits.len()));
        }
        fn hide_digits(&self, _win: WinId) {
            self.0.borrow_mut().push("hide".into());
        }
        fn toggle(&self, _win: WinId, keycode: u32) {
            self.0.borrow_mut().push(format!("toggle {keycode}"));
        }
    }
    let s = stage(f)?;
    let top_bar = HostOp::Focus {
        win: f.win,
        to: FocusTo::TopBarItem(0),
        on_grab: vec![],
    };
    let rename = HostOp::Focus {
        win: f.win,
        to: FocusTo::RenameEntry,
        on_grab: vec![],
    };
    let ops = [
        top_bar.clone(),
        rename.clone(),
        HostOp::Hint(eitri_core::wm::HintOp::Hide),
    ];
    for op in &ops {
        if s.host.apply(op) != Applied::Refused {
            return Err(format!("{op:?} was carried out with no function to carry it"));
        }
    }
    let grants = Rc::new(Cell::new(true));
    {
        let grants = grants.clone();
        let item = s.widgets[1].clone();
        s.host
            .set_top_bar(Rc::new(move |_, index| grants.get() && index == 0 && item.grab_focus()));
    }
    let hints = Rc::new(Hints(RefCell::new(Vec::new())));
    s.host.set_hints(hints.clone());
    if s.host.apply(&top_bar) != Applied::Done {
        return Err("a top-bar Focus the shell's function granted was refused".into());
    }
    grants.set(false);
    if s.host.apply(&top_bar) != Applied::Refused {
        return Err("a top-bar Focus the shell's function refused was carried out".into());
    }
    let entry_grants = Rc::new(Cell::new(true));
    {
        let grants = entry_grants.clone();
        let entry = s.widgets[0].clone();
        s.host
            .set_rename_entry(Rc::new(move |_| grants.get() && entry.grab_focus()));
    }
    if s.host.apply(&rename) != Applied::Done {
        return Err("a rename entry Focus the shell's function granted was refused".into());
    }
    entry_grants.set(false);
    if s.host.apply(&rename) != Applied::Refused {
        return Err("a rename entry Focus the shell's function refused was carried out".into());
    }
    let chrome = HostOp::Chrome { win: f.win, rev: 3 };
    if s.host.apply(&chrome) != Applied::Done {
        return Err("a Chrome op for a window with its widgets was refused".into());
    }
    let first = std::mem::take(&mut *s.chrome.borrow_mut());
    let expected = [
        "workspace conformance None",
        "selection None",
        "mark None",
        "chip terminal shown=true",
        "strips 0",
        "float bars 0",
        "prefix strip false",
        "prefix panel false",
        "toast 1 no last module",
    ];
    if first != expected {
        return Err(format!("the first Chrome drew {first:?}"));
    }
    if s.host.apply(&chrome) != Applied::Done || !s.chrome.borrow().is_empty() {
        return Err(format!("the same model drawn again touched {:?}", s.chrome.borrow()));
    }
    for op in [
        HostOp::Hint(eitri_core::wm::HintOp::ShowDigits(vec![
            (f.a.clone(), 1),
            (f.b.clone(), 2),
        ])),
        HostOp::Hint(eitri_core::wm::HintOp::Hide),
        HostOp::Hint(eitri_core::wm::HintOp::Toggle { keycode: 41 }),
    ] {
        if s.host.apply(&op) != Applied::Done {
            return Err(format!("{op:?} was refused"));
        }
    }
    let heard = hints.0.borrow().clone();
    if heard != ["show 2", "hide", "toggle 41"] {
        return Err(format!("the hints heard {heard:?}"));
    }
    finish(s);
    Ok(())
}

/// `window.new` goes through the pair the shell gives the host, its answer the host's; `window.other`'s raise answers
/// with the activation token this window's display gave, posted while it applies, for the controller to send on.
fn the_pair_starts_and_raises_through_the_shell(f: &Fixture) -> Result<(), String> {
    use eitri_core::window_link::names::WindowName;
    use eitri_core::wm::{HostEvent, PeerEvent, PeerId};
    struct Pair {
        opened: RefCell<Vec<WinId>>,
        restored: RefCell<Vec<WinId>>,
        started: RefCell<Vec<WinId>>,
        answer: RefCell<Result<(), String>>,
    }
    impl gtk_host::LinkedWindows for Pair {
        fn open(&self, win: WinId, _name: &WindowName, _token: Option<String>) -> Result<(), String> {
            self.opened.borrow_mut().push(win);
            self.answer.borrow().clone()
        }
        fn end_peer(&self, win: WinId, _peer: PeerId) -> Result<(), String> {
            self.opened.borrow_mut().push(win);
            self.answer.borrow().clone()
        }
        fn restore(&self, win: WinId, _name: &WindowName) -> Result<(), String> {
            self.restored.borrow_mut().push(win);
            self.answer.borrow().clone()
        }
        fn start_editor(&self, win: WinId) -> Result<(), String> {
            self.started.borrow_mut().push(win);
            self.answer.borrow().clone()
        }
    }
    let s = stage(f)?;
    let pair = Rc::new(Pair {
        opened: RefCell::default(),
        restored: RefCell::default(),
        started: RefCell::default(),
        answer: RefCell::new(Ok(())),
    });
    s.host.set_linked_windows(pair.clone());
    let open = HostOp::OpenLinkedWindow { win: f.win };
    if s.host.apply(&open) != Applied::Done {
        return Err("an OpenLinkedWindow the pair carried out was refused".into());
    }
    *pair.answer.borrow_mut() = Err("this window cannot start a second window".into());
    if s.host.apply(&open) != Applied::Refused {
        return Err("an OpenLinkedWindow the pair refused was answered done".into());
    }
    if *pair.opened.borrow() != [f.win, f.win] {
        return Err(format!("the pair was asked for {:?}", pair.opened.borrow()));
    }
    // A launch's restore of window 2 goes through the same pair, its refusal the pair's.
    let restore = HostOp::RestoreLinkedWindow { win: f.win };
    if s.host.apply(&restore) != Applied::Refused {
        return Err("a RestoreLinkedWindow the pair refused was answered done".into());
    }
    *pair.answer.borrow_mut() = Ok(());
    if s.host.apply(&restore) != Applied::Done || *pair.restored.borrow() != [f.win, f.win] {
        return Err(format!("the pair was asked to restore {:?}", pair.restored.borrow()));
    }
    // A window 2's start of the editor window goes through the same pair, its refusal the pair's.
    let start = HostOp::StartEditorWindow { win: f.win };
    if s.host.apply(&start) != Applied::Done {
        return Err("a StartEditorWindow the pair carried out was refused".into());
    }
    *pair.answer.borrow_mut() = Err("the editor window runs already".into());
    if s.host.apply(&start) != Applied::Refused || *pair.started.borrow() != [f.win, f.win] {
        return Err(format!(
            "the pair was asked to start the editor {:?}",
            pair.started.borrow()
        ));
    }
    let start_refused = s.sink.events().into_iter().any(|(_, event)| {
        matches!(event, HostEvent::Refused { what: eitri_core::wm::OpTag::StartEditorWindow, why, .. } if why == "the editor window runs already")
    });
    if !start_refused {
        return Err("the editor start's refusal did not reach the sink with its reason".into());
    }
    *pair.answer.borrow_mut() = Err("this window cannot start a second window".into());
    let refused = s.sink.events().into_iter().any(|(_, event)| {
        matches!(event, HostEvent::Refused { why, .. } if why == "this window cannot start a second window")
    });
    if !refused {
        return Err("the pair's refusal did not reach the sink with its reason".into());
    }
    let before = s.sink.events().len();
    let raise = HostOp::RaisePeer {
        win: f.win,
        peer: PeerId(2),
    };
    s.sink.applying(Some(0));
    let answer = s.host.apply(&raise);
    s.sink.applying(None);
    if answer != Applied::Done {
        return Err(format!("RaisePeer answered {answer:?}"));
    }
    let raised: Vec<_> = s.sink.events()[before..]
        .iter()
        .filter(|(at, event)| {
            *at == Some(0) && matches!(event, HostEvent::Peer(PeerEvent::Raised { peer: PeerId(2), .. }))
        })
        .cloned()
        .collect();
    if raised.len() != 1 {
        return Err(format!("RaisePeer posted {:?}", &s.sink.events()[before..]));
    }
    // Ending the other window goes through the pair too, and its refusal is posted with the pair's words.
    let end = HostOp::EndPeer {
        win: f.win,
        peer: PeerId(2),
    };
    if s.host.apply(&end) != Applied::Refused {
        return Err("an EndPeer the pair refused was answered done".into());
    }
    *pair.answer.borrow_mut() = Ok(());
    if s.host.apply(&end) != Applied::Done {
        return Err("an EndPeer the pair carried out was refused".into());
    }
    if pair.opened.borrow().len() != 4 {
        return Err(format!("the pair was asked for {:?}", pair.opened.borrow()));
    }
    finish(s);
    Ok(())
}

fn prepared_grid() -> Result<
    (
        gtk4::Window,
        module_grid::ModuleGrid,
        gtk4::Widget,
        Rc<RefCell<eitri_core::layout::Layout>>,
    ),
    String,
> {
    use eitri_core::layout::{Axis, Layout, Node};
    let module = ModuleId::lua("markdown");
    let mut layout = Layout::new(
        Node::split(
            Axis::Row,
            0.5,
            Node::Leaf(ModuleId::agent()),
            Node::Leaf(module.clone()),
        ),
        ModuleId::agent(),
    )
    .unwrap();
    layout.set_away(&module, true).unwrap();
    // The test writes the layout behind the grid's view, as the window controller would.
    let (view, layout) = eitri_core::wm::LayoutView::for_test(layout);
    let grid = module_grid::ModuleGrid::new(view);
    let view = gtk4::Button::with_label("prepared page");
    let host = web_host::WebHost::new(&view);
    let agent = gtk4::Button::with_label("chat");
    grid.add(ModuleId::agent(), &agent, &agent, module_grid::HostKind::Direct);
    grid.add(module.clone(), &host, &view, module_grid::HostKind::Web);
    let window = gtk4::Window::new();
    window.set_default_size(1000, 700);
    window.set_child(Some(&grid));
    grid.apply();
    window.present();
    pump(50);
    grid.prepare_page(&module, (500, 700))?;
    Ok((window, grid, host.upcast(), layout))
}

fn a_prepared_host_is_allocated_at_its_final_size_drawn_and_covered() -> Result<(), String> {
    let (window, grid, host, _) = prepared_grid()?;
    let allocation = host.allocation();
    // Drawn (a widget at zero opacity is not, and its page gets no animation frames) and below every other child.
    if !host.is_child_visible()
        || host.opacity() != 1.0
        || host.can_target()
        || grid.first_child().as_ref() != Some(&host)
        || allocation.width() <= 0
        || allocation.height() != 700
    {
        return Err(format!(
            "prepared page: visible={} opacity={} first={} size={allocation:?}",
            host.is_child_visible(),
            host.opacity(),
            grid.first_child().as_ref() == Some(&host)
        ));
    }
    if host.parent().as_ref() != Some(grid.upcast_ref()) {
        return Err("the prepared host left its grid".into());
    }
    window.close();
    Ok(())
}

/// The colour and size of every colour node under `node`, whatever clips and transforms it sits in.
fn colour_nodes(node: &gtk4::gsk::RenderNode, out: &mut Vec<(gtk4::gdk::RGBA, f32, f32)>) {
    use gtk4::gsk;
    if let Some(color) = node.downcast_ref::<gsk::ColorNode>() {
        let bounds = node.bounds();
        out.push((color.color(), bounds.width(), bounds.height()));
    } else if let Some(container) = node.downcast_ref::<gsk::ContainerNode>() {
        for i in 0..container.n_children() {
            colour_nodes(&container.child(i), out);
        }
    } else if let Some(clip) = node.downcast_ref::<gsk::ClipNode>() {
        colour_nodes(&clip.child(), out);
    } else if let Some(clip) = node.downcast_ref::<gsk::RoundedClipNode>() {
        colour_nodes(&clip.child(), out);
    } else if let Some(transform) = node.downcast_ref::<gsk::TransformNode>() {
        colour_nodes(&transform.child(), out);
    } else if let Some(opacity) = node.downcast_ref::<gsk::OpacityNode>() {
        colour_nodes(&opacity.child(), out);
    } else if let Some(debug) = node.downcast_ref::<gsk::DebugNode>() {
        colour_nodes(&debug.child(), out);
    }
}

/// Whether the grid, drawn now, paints `cover` over the whole of `host`.
fn painted_over(
    window: &gtk4::Window,
    grid: &module_grid::ModuleGrid,
    host: &gtk4::Widget,
    cover: &gtk4::gdk::RGBA,
) -> bool {
    grid.queue_draw();
    let Some(bounds) = host.compute_bounds(grid) else {
        return false;
    };
    let snapshot = gtk4::Snapshot::new();
    window.snapshot_child(grid, &snapshot);
    let Some(node) = snapshot.to_node() else {
        return false;
    };
    let mut colours = Vec::new();
    colour_nodes(&node, &mut colours);
    let same = |a: &gtk4::gdk::RGBA| {
        (a.red() - cover.red()).abs() < 0.01
            && (a.green() - cover.green()).abs() < 0.01
            && (a.blue() - cover.blue()).abs() < 0.01
            && a.alpha() > 0.99
    };
    colours
        .iter()
        .any(|(c, w, h)| same(c) && (w - bounds.width()).abs() < 0.5 && (h - bounds.height()).abs() < 0.5)
}

fn a_prepared_page_is_covered_with_the_grids_background_until_it_is_revealed() -> Result<(), String> {
    let (window, grid, host, layout) = prepared_grid()?;
    let module = ModuleId::lua("markdown");
    // A background nothing else in the window paints.
    let mut tokens = eitri_core::theme::ThemeTokens::fallback();
    tokens.bg = eitri_core::theme::color::Rgb { r: 64, g: 128, b: 191 };
    let cover = gtk4::gdk::RGBA::new(64.0 / 255.0, 128.0 / 255.0, 191.0 / 255.0, 1.0);
    grid.set_page_cover(&tokens);
    pump(50);
    // Under its own cover, whether or not another host is drawn over its rectangle: a layout that shows only floats
    // leaves it under none.
    if !painted_over(&window, &grid, &host, &cover) {
        return Err("a prepared page is drawn with no cover of its own".into());
    }
    layout.borrow_mut().set_away(&module, false).unwrap();
    grid.map_prepared(&module, true);
    grid.size_allocate(&gtk4::Allocation::new(0, 0, 1000, 800), -1);
    if painted_over(&window, &grid, &host, &cover) {
        return Err("the revealed page is still covered".into());
    }
    window.close();
    Ok(())
}

fn the_swap_is_one_allocation_pass() -> Result<(), String> {
    let (window, grid, host, layout) = prepared_grid()?;
    let module = ModuleId::lua("markdown");
    layout.borrow_mut().set_away(&module, false).unwrap();
    grid.map_prepared(&module, true);
    grid.size_allocate(&gtk4::Allocation::new(0, 0, 1000, 800), -1);
    if host.opacity() != 1.0
        || !host.can_target()
        || host.allocation().x() <= 0
        || host.parent().as_ref() != Some(grid.upcast_ref())
    {
        return Err(format!(
            "the first committed allocation did not reveal the page: {:?}",
            host.allocation()
        ));
    }
    window.close();
    Ok(())
}

fn a_plugin_page_prepare_is_carried_out_and_answered_prepared(f: &Fixture) -> Result<(), String> {
    let s = stage(f)?;
    let module = ModuleId::lua("markdown");
    let op = HostOp::PrepareSurface {
        win: f.win,
        module: module.clone(),
        shape: eitri_core::window_link::Shape::Tiled,
        size: (640, 480),
    };
    if s.host.apply(&op) != Applied::Done {
        return Err("plugin page prepare refused".into());
    }
    // Carried out is what the driver answers as `HostEvent::Prepared`: the page's place exists, registered in the
    // grid, and is not revealed until the commit maps it. The stand-in maker builds that place here; the shell's own
    // page arm (the page built and laid out in its grid) needs a real page and is checked with the window pair.
    if !s.grid.children.borrow().contains(&Slot::Host(module.clone())) {
        return Err("the prepared page has no place in the grid".into());
    }
    let column = s.window.child().ok_or("the stage has no column")?;
    let mut child = column.first_child();
    let mut placed = None;
    while let Some(widget) = child {
        let label = widget.downcast_ref::<gtk4::Button>().and_then(|button| button.label());
        if label.is_some_and(|label| label == module.as_str()) {
            placed = Some(widget.clone());
        }
        child = widget.next_sibling();
    }
    let placed = placed.ok_or("the prepared page's host was never built")?;
    if placed.is_child_visible() {
        return Err("the prepared page was revealed before its commit".into());
    }
    finish(s);
    Ok(())
}

/// An arrival's place comes from the maker and is registered; its adoption takes the op's descriptors whatever comes
/// of it, and a maker that builds no such arrival refuses it naming the module, as a host with no place for it does.
fn an_arrival_is_prepared_and_its_adoption_owns_the_descriptors(f: &Fixture) -> Result<(), String> {
    use eitri_core::wm::{Descriptors, HostEvent, OpTag};
    let s = stage(f)?;
    let arriving = ModuleId::parse("terminal#4").unwrap();
    let prepare = HostOp::PrepareSurface {
        win: f.win,
        module: arriving.clone(),
        shape: eitri_core::window_link::Shape::Tiled,
        size: (640, 480),
    };
    if s.host.apply(&prepare) != Applied::Done {
        return Err("the stub maker's place was refused".into());
    }
    let adopt = |module: &ModuleId| {
        let (read, _write) = std::io::pipe().unwrap();
        let fds = Descriptors::new(vec![read.into()]);
        let op = HostOp::AdoptMoved {
            win: f.win,
            module: module.clone(),
            kind: eitri_core::wm::moves::MoveKind::Terminal,
            record: serde_json::Value::Null,
            fds: fds.clone(),
        };
        (s.host.apply(&op), fds.len())
    };
    for module in [arriving.clone(), ModuleId::parse("terminal#5").unwrap()] {
        let (answer, left) = adopt(&module);
        if answer != Applied::Refused || left != 0 {
            return Err(format!(
                "{module}: answered {answer:?} with {left} descriptors left in the op"
            ));
        }
        let named = s.sink.events().into_iter().any(|(_, event)| {
            matches!(event, HostEvent::Refused { what: OpTag::AdoptMoved, surface: Some((_, m)), .. } if m == module)
        });
        if !named {
            return Err(format!("{module}: the refused adoption did not name its module"));
        }
    }
    finish(s);
    Ok(())
}

/// A view of the first-start screen with a long row, a long body line and a button: more than any narrow window holds.
fn long_view() -> eitri_core::onboarding::OnboardingView {
    use eitri_core::onboarding::{OnboardingView, ViewButton, ViewItem, ViewKey, ViewRow};
    OnboardingView {
        title: "Set up Eitri's window keys".into(),
        step: "1 / 2".into(),
        rows: vec![ViewRow {
            label: "tmux".into(),
            text: format!("~/{}.tmux.conf: prefix Ctrl+a", "very/long/directory/".repeat(20)),
            note: "(will be used)".into(),
        }],
        question: Some("How should the window's keys work?".into()),
        items: vec![ViewItem {
            number: 1,
            name: "Neovim + tmux".into(),
            summary: "Ctrl+b, then a key, as in tmux ".repeat(10),
            tag: Some("found".into()),
            selected: true,
        }],
        body: vec!["eitri.keymap.use('nvim-tmux', { prefix = 'C-b' } ) -- ".repeat(8)],
        status: Vec::new(),
        footer: "j/k or 1-5 choose, and more words than a narrow line holds · Enter next · Esc not now".into(),
        buttons: vec![
            ViewButton::new(ViewKey::Char('p'), "Prefix: Ctrl+b   p change"),
            ViewButton::new(ViewKey::Enter, "Enter next"),
            ViewButton::new(ViewKey::Esc, "Esc not now"),
        ],
        help: None,
    }
}

/// Every widget below `root`, `root` first.
fn descendants(root: &gtk4::Widget) -> Vec<gtk4::Widget> {
    let mut out = vec![root.clone()];
    let mut child = root.first_child();
    while let Some(widget) = child {
        out.extend(descendants(&widget));
        child = widget.next_sibling();
    }
    out
}

/// A mouse alone drives the screen: the preset's row is a button that posts its number, the footer's Enter and Esc and
/// the step's own shortcut are buttons that post those keys, the footer's line says only what no button does, and the
/// chosen preset's labels carry the two classes that make its solid block (GTK reads one string with a space in it as
/// the name of a single class).
fn the_screen_is_driven_by_the_mouse_alone() -> Result<(), String> {
    use eitri_core::onboarding::ViewKey;
    let window = gtk4::Window::new();
    window.set_default_size(600, 400);
    let overlay = gtk4::Overlay::new();
    overlay.set_child(Some(&gtk4::Label::new(Some("window"))));
    let screen = gtk_host::onboarding::OnboardingScreen::new();
    overlay.add_overlay(screen.widget());
    window.set_child(Some(&overlay));
    let clicked: Rc<RefCell<Vec<ViewKey>>> = Rc::default();
    {
        let clicked = clicked.clone();
        screen.on_button(move |key| clicked.borrow_mut().push(key));
    }
    window.present();
    pump(200);
    screen.show(Some(&long_view()));
    pump(100);
    let all = descendants(screen.widget());
    let result = (|| {
        let both = all
            .iter()
            .filter(|w| w.has_css_class("onboarding-item") && w.has_css_class("selected"))
            .count();
        if both == 0 {
            return Err("no label of the chosen preset has both classes".to_string());
        }
        let footer = all
            .iter()
            .find(|w| w.has_css_class("onboarding-footer"))
            .and_then(|w| w.clone().downcast::<gtk4::Label>().ok())
            .ok_or_else(|| "no footer label".to_string())?;
        if footer.text() != long_view().footer_hint() || footer.text().contains("Enter next") {
            return Err(format!("the footer's line says {:?}", footer.text()));
        }
        let buttons: Vec<gtk4::Button> = all
            .iter()
            .filter_map(|w| w.clone().downcast::<gtk4::Button>().ok())
            .collect();
        // The preset's row, then the three shortcut buttons.
        if buttons.len() != 4 {
            return Err(format!("{} buttons, not 4", buttons.len()));
        }
        for button in &buttons {
            button.emit_clicked();
        }
        pump(100);
        let got = clicked.borrow().clone();
        let want = [ViewKey::Char('1'), ViewKey::Char('p'), ViewKey::Enter, ViewKey::Esc];
        if got != want {
            return Err(format!("the clicks posted {got:?}, not {want:?}"));
        }
        Ok(())
    })();
    window.destroy();
    pump(50);
    result
}

/// The screen's panel is an overlay child that asks the window for no width: in a 300x200 window, with long rows and
/// body lines on it, the window keeps its size and the panel's own minimum width is within it.
fn the_screen_asks_for_no_width_of_its_own() -> Result<(), String> {
    let window = gtk4::Window::new();
    window.set_default_size(300, 200);
    let overlay = gtk4::Overlay::new();
    let backdrop = gtk4::Label::new(Some("window"));
    overlay.set_child(Some(&backdrop));
    let screen = gtk_host::onboarding::OnboardingScreen::new();
    overlay.add_overlay(screen.widget());
    window.set_child(Some(&overlay));
    window.present();
    pump(200);
    screen.show(Some(&long_view()));
    pump(300);
    let (min, _, _, _) = screen.widget().measure(gtk4::Orientation::Horizontal, -1);
    let result = if window.width() != 300 || window.height() != 200 {
        Err(format!(
            "the window grew to {}x{} with the screen up",
            window.width(),
            window.height()
        ))
    } else if min > 300 {
        Err(format!("the screen's minimum width is {min}"))
    } else if screen.widget().width() > 300 {
        Err(format!(
            "the screen is {} wide in a 300 wide window",
            screen.widget().width()
        ))
    } else {
        Ok(())
    };
    window.destroy();
    pump(50);
    result
}

/// The screen's first page with `count` presets, `chosen` the chosen one, and enough rows above them that a low window
/// cannot hold it all.
fn tall_view(count: u32, chosen: u32) -> eitri_core::onboarding::OnboardingView {
    rows_view(12, count, chosen)
}

/// [`tall_view`] with `rows` found rows above the presets.
fn rows_view(rows: u32, count: u32, chosen: u32) -> eitri_core::onboarding::OnboardingView {
    use eitri_core::onboarding::{ViewItem, ViewRow};
    let mut view = long_view();
    view.body.clear();
    view.rows = (0..rows)
        .map(|row| ViewRow {
            label: format!("row {row}"),
            text: "found".into(),
            note: String::new(),
        })
        .collect();
    view.items = (1..=count)
        .map(|number| ViewItem {
            number,
            name: format!("preset {number}"),
            summary: "what it does".into(),
            tag: None,
            selected: number == chosen,
        })
        .collect();
    view
}

/// The scroller inside the panel, whose adjustment is what the user sees.
fn screen_scroller(screen: &gtk_host::onboarding::OnboardingScreen) -> Result<gtk4::ScrolledWindow, String> {
    descendants(screen.widget())
        .into_iter()
        .find_map(|widget| widget.downcast::<gtk4::ScrolledWindow>().ok())
        .ok_or_else(|| "no scroller in the screen".to_string())
}

/// A window too low for the screen: moving the choice brings the chosen preset into view, a key scrolls the text, a
/// row filling in leaves the text where it stands, and a new page starts at the top. In the keyboard's terms: nothing
/// but the screen's own keys ever reach this widget, so this is all the scrolling there is.
fn a_short_window_scrolls_to_the_choice_and_by_the_keys() -> Result<(), String> {
    use eitri_core::onboarding::Scroll;
    let window = gtk4::Window::new();
    window.set_default_size(500, 240);
    let overlay = gtk4::Overlay::new();
    overlay.set_child(Some(&gtk4::Label::new(Some("window"))));
    let screen = gtk_host::onboarding::OnboardingScreen::new();
    overlay.add_overlay(screen.widget());
    window.set_child(Some(&overlay));
    window.present();
    pump(200);
    let check = || -> Result<(), String> {
        screen.show(Some(&tall_view(4, 4)));
        pump(300);
        let adjustment = screen_scroller(&screen)?.vadjustment();
        if adjustment.upper() <= adjustment.page_size() {
            return Err(format!(
                "the view fits ({} in {}), so nothing here tests scrolling",
                adjustment.upper(),
                adjustment.page_size()
            ));
        }
        let bottom = adjustment.upper() - adjustment.page_size();
        if adjustment.value() < bottom - 2.0 {
            return Err(format!(
                "the last preset is chosen and the text stands at {} of {bottom}",
                adjustment.value()
            ));
        }
        screen.scroll(Scroll::PageUp);
        pump(100);
        let scrolled = adjustment.value();
        if scrolled >= bottom {
            return Err("PageUp did not move the text".into());
        }
        // A detection row came in: the same page, the same choice.
        screen.show(Some(&tall_view(4, 4)));
        pump(200);
        if (adjustment.value() - scrolled).abs() > 1.0 {
            return Err(format!(
                "a row filling in moved the text from {scrolled} to {}",
                adjustment.value()
            ));
        }
        screen.show(Some(&tall_view(4, 3)));
        pump(200);
        let moved = adjustment.value();
        screen.scroll(Scroll::LineUp);
        pump(100);
        if adjustment.value() >= moved {
            return Err(format!("LineUp did not move the text from {moved}"));
        }
        screen.scroll(Scroll::LineDown);
        pump(100);
        if (adjustment.value() - moved).abs() > 1.0 {
            return Err(format!("LineDown did not bring it back to {moved}"));
        }
        let mut review = long_view();
        review.step = "2 / 2".into();
        review.question = Some("Writes init.lua:".into());
        review.items.clear();
        review.rows.clear();
        review.body = (0..40).map(|line| format!("line {line}")).collect();
        screen.show(Some(&review));
        pump(200);
        if adjustment.value() != 0.0 {
            return Err(format!("a new page starts at {}", adjustment.value()));
        }
        let lines_below = adjustment.upper() - adjustment.page_size();
        if lines_below <= 0.0 {
            return Err("the review's lines fit, so scrolling them tests nothing".into());
        }
        for _ in 0..100 {
            screen.scroll(Scroll::PageDown);
        }
        pump(100);
        if (adjustment.value() - lines_below).abs() > 1.0 {
            return Err(format!(
                "the keys stop at {} of the {lines_below} the last line needs",
                adjustment.value()
            ));
        }
        Ok(())
    };
    let result = check();
    window.destroy();
    pump(50);
    result
}

/// A row that fills in above the presets after the page came up (a detection answer, `claude` not found) pushes them
/// down; with no scroll key used the presets come back into view, all of them when they fit the visible text, the
/// chosen one first of all. After a scroll key the text stays where the person put it.
fn a_row_filling_in_keeps_the_presets_in_view() -> Result<(), String> {
    use eitri_core::onboarding::Scroll;
    let window = gtk4::Window::new();
    window.set_default_size(500, 560);
    let overlay = gtk4::Overlay::new();
    overlay.set_child(Some(&gtk4::Label::new(Some("window"))));
    let screen = gtk_host::onboarding::OnboardingScreen::new();
    overlay.add_overlay(screen.widget());
    window.set_child(Some(&overlay));
    window.present();
    pump(200);
    // The question and the last preset's button, as the scroller shows them.
    let seen = |screen: &gtk_host::onboarding::OnboardingScreen| -> Result<(f32, f32, f32), String> {
        let scroller = screen_scroller(screen)?;
        let all = descendants(screen.widget());
        let top = |class: &str, last: bool| -> Result<(f32, f32), String> {
            let mut found = all.iter().filter(|w| w.has_css_class(class));
            let widget = if last { found.next_back() } else { found.next() };
            let widget = widget.ok_or_else(|| format!("no {class} in the screen"))?;
            let bounds = widget
                .compute_bounds(&scroller)
                .ok_or_else(|| format!("{class} has no bounds in the scroller"))?;
            Ok((bounds.y(), bounds.y() + bounds.height()))
        };
        let (question_top, _) = top("onboarding-question", false)?;
        let (_, last_bottom) = top("onboarding-item-button", true)?;
        Ok((question_top, last_bottom, scroller.height() as f32))
    };
    let check = || -> Result<(), String> {
        screen.show(Some(&rows_view(3, 5, 5)));
        pump(300);
        screen.show(Some(&rows_view(14, 5, 5)));
        pump(400);
        let adjustment = screen_scroller(&screen)?.vadjustment();
        if adjustment.upper() <= adjustment.page_size() {
            return Err("the rows fit, so nothing here tests the presets being pushed down".into());
        }
        let (top, bottom, page) = seen(&screen)?;
        if top < -1.0 || bottom > page + 1.0 {
            return Err(format!(
                "the presets stand at {top}..{bottom} of the {page} the text shows, after a row filled in above them"
            ));
        }
        // A scroll key pressed before the replacement widgets are laid out is not taken back by the reveal waiting for
        // them.
        let at_presets = adjustment.value();
        screen.show(Some(&rows_view(15, 5, 5)));
        screen.scroll(Scroll::PageUp);
        pump(400);
        if adjustment.value() >= at_presets - 2.0 {
            return Err(format!(
                "a PageUp before the layout was undone: the text stands at {} again, as it did at the presets ({at_presets})",
                adjustment.value()
            ));
        }
        screen.scroll(Scroll::PageUp);
        pump(100);
        let chosen = adjustment.value();
        screen.show(Some(&rows_view(15, 5, 5)));
        pump(300);
        if (adjustment.value() - chosen).abs() > 1.0 {
            return Err(format!(
                "a row filling in moved the text from {chosen} to {} after a scroll key",
                adjustment.value()
            ));
        }
        Ok(())
    };
    let result = check();
    window.destroy();
    pump(50);
    result
}

/// A sentence of the file or of step 3 wraps at the panel's width and is never cut: every body line is a wrapping
/// label with no ellipsis.
fn a_body_line_wraps_and_is_never_cut() -> Result<(), String> {
    let window = gtk4::Window::new();
    window.set_default_size(400, 500);
    let overlay = gtk4::Overlay::new();
    overlay.set_child(Some(&gtk4::Label::new(Some("window"))));
    let screen = gtk_host::onboarding::OnboardingScreen::new();
    overlay.add_overlay(screen.widget());
    window.set_child(Some(&overlay));
    window.present();
    pump(200);
    screen.show(Some(&long_view()));
    pump(200);
    let body: Vec<gtk4::Label> = descendants(screen.widget())
        .into_iter()
        .filter(|widget| widget.has_css_class("onboarding-body"))
        .filter_map(|widget| widget.downcast::<gtk4::Label>().ok())
        .collect();
    let result = if body.is_empty() {
        Err("no body line on the screen".to_string())
    } else if body
        .iter()
        .any(|label| !label.wraps() || label.ellipsize() != gtk4::pango::EllipsizeMode::None)
    {
        Err("a body line can be cut".to_string())
    } else if body[0].layout().line_count() < 2 {
        Err("the long line did not wrap".to_string())
    } else {
        Ok(())
    };
    window.destroy();
    pump(50);
    result
}

/// The controller's `Focus` comes before the `Chrome` op that shows the screen: with the screen hidden, the `Focus`
/// answers `Done` and the keys are inside the screen; the `Chrome` op then fills and shows it. The widget that holds
/// the keys is a plain box: no text widget, so no input-method context.
fn the_focus_op_before_the_chrome_op_still_grabs(f: &Fixture) -> Result<(), String> {
    let s = stage(f)?;
    let mut model = stage_model();
    model.onboarding = Some(long_view());
    s.host.set_chrome_model(Rc::new(move |_| Some(model.clone())));
    if s.screen.widget().is_visible() {
        return Err("the screen starts shown".into());
    }
    let focus = HostOp::Focus {
        win: f.win,
        to: FocusTo::Onboarding,
        on_grab: vec![],
    };
    if s.host.apply(&focus) != Applied::Done {
        return Err("a Focus on a hidden screen was refused".into());
    }
    let held = GtkWindowExt::focus(&s.window).ok_or("no widget holds the keys after the Focus")?;
    if held != *s.screen.widget() && !held.is_ancestor(s.screen.widget()) {
        return Err(format!("the keys are on {held:?}, outside the screen"));
    }
    if held.is::<gtk4::Text>() || held.is::<gtk4::Entry>() || held.is::<gtk4::TextView>() {
        return Err(format!("the keys are on a text widget, {held:?}"));
    }
    if !s.screen.has_keys() {
        return Err("the screen does not report holding the keys".into());
    }
    if s.host.apply(&HostOp::Chrome { win: f.win, rev: 2 }) != Applied::Done {
        return Err("the Chrome op that fills the screen was refused".into());
    }
    pump(100);
    if !s.screen.widget().is_mapped() {
        return Err("the screen is not shown after the Chrome op".into());
    }
    if !s.screen.has_keys() {
        return Err("the Chrome op moved the keys off the screen".into());
    }
    finish(s);
    Ok(())
}

/// `grab_keys` puts the keys on a box: not a `GtkText`, `GtkEntry` or `GtkTextView`, the widgets an input method
/// attaches its context to.
fn the_screen_takes_the_keys_with_no_input_context() -> Result<(), String> {
    let window = gtk4::Window::new();
    window.set_default_size(400, 300);
    let overlay = gtk4::Overlay::new();
    overlay.set_child(Some(&gtk4::Label::new(Some("window"))));
    let screen = gtk_host::onboarding::OnboardingScreen::new();
    overlay.add_overlay(screen.widget());
    window.set_child(Some(&overlay));
    window.present();
    pump(200);
    screen.show(Some(&long_view()));
    pump(100);
    let result = if !screen.grab_keys() {
        Err("grab_keys was refused".to_string())
    } else {
        match GtkWindowExt::focus(&window) {
            Some(held) if held.is::<gtk4::Box>() && !held.is::<gtk4::Text>() && !held.is::<gtk4::Entry>() => Ok(()),
            other => Err(format!("the keys are on {other:?}, not on the screen's box")),
        }
    };
    window.destroy();
    pump(50);
    result
}

fn run_all() -> Result<Vec<String>, String> {
    let f = Fixture {
        win: WinId::MAIN,
        a: ModuleId::agent(),
        b: ModuleId::terminal(),
        hidden: ModuleId::editor(),
    };
    let mut passed = Vec::new();
    // This host is given no pair here, so the pair's ops are refused; an arriving terminal's place is made by the stub
    // maker, as the shell's maker makes its pane.
    let scripts = conformance::scripts(&f)
        .into_iter()
        .chain(conformance::pair_refusal_scripts(&f))
        .chain(own_scripts(&f))
        .chain([
            conformance::join_back_refusal_script(f.win, "the tab could not join the base view"),
            conformance::teardown_script(f.win),
        ]);
    for script in scripts {
        let s = stage(&f)?;
        let result = conformance::run_observed(s.host.as_ref(), &s.sink, &script, Some(s.grid.as_ref()));
        let events = s.sink.events();
        finish(s);
        result.map_err(|e| format!("{e}; events posted: {events:?}"))?;
        passed.push(script.name.to_string());
    }
    unmapped_owner_keeps_no_keys(&f)?;
    passed.push("an_unmapped_owner_keeps_no_keys".into());
    outside_close(&f)?;
    passed.push("a_close_from_outside_asks_once_and_close_window_never".into());
    stopped_close(&f)?;
    passed.push("a_stopped_close_stays_stopped_and_close_window_still_closes".into());
    close_window_inside_the_request(&f)?;
    passed.push("close_window_inside_the_close_request_closes_the_window".into());
    closed_windows_are_forgotten(&f)?;
    passed.push("a_closed_window_is_forgotten_with_its_surfaces".into());
    open_window_then_present(&f)?;
    passed.push("open_window_builds_it_unshown_and_present_shows_it".into());
    focus_goes_through_the_shells_function(&f)?;
    passed.push("focus_goes_through_the_shells_focus_function_once_set".into());
    gtk_host_sets_the_mode_it_is_asked_for(&f)?;
    passed.push("gtk_host_sets_the_mode_it_is_asked_for".into());
    top_bar_chrome_and_hints_go_through_the_shells_functions(&f)?;
    passed.push("the_top_bar_chrome_and_hints_go_through_the_shells_functions_once_set".into());
    the_pair_starts_and_raises_through_the_shell(&f)?;
    passed.push("the_pair_starts_and_raises_through_the_shell".into());
    a_prepared_host_is_allocated_at_its_final_size_drawn_and_covered()?;
    passed.push("a_prepared_host_is_allocated_at_its_final_size_drawn_and_covered".into());
    a_prepared_page_is_covered_with_the_grids_background_until_it_is_revealed()?;
    passed.push("a_prepared_page_is_covered_with_the_grids_background_until_it_is_revealed".into());
    the_swap_is_one_allocation_pass()?;
    passed.push("the_swap_is_one_allocation_pass".into());
    a_plugin_page_prepare_is_carried_out_and_answered_prepared(&f)?;
    passed.push("a_plugin_page_prepare_is_carried_out_and_answered_prepared".into());
    an_arrival_is_prepared_and_its_adoption_owns_the_descriptors(&f)?;
    passed.push("an_arrival_is_prepared_and_its_adoption_owns_the_descriptors".into());
    the_screen_asks_for_no_width_of_its_own()?;
    passed.push("the_screen_asks_for_no_width_of_its_own".into());
    a_short_window_scrolls_to_the_choice_and_by_the_keys()?;
    passed.push("a_short_window_scrolls_to_the_choice_and_by_the_keys".into());
    a_row_filling_in_keeps_the_presets_in_view()?;
    passed.push("a_row_filling_in_keeps_the_presets_in_view".into());
    a_body_line_wraps_and_is_never_cut()?;
    passed.push("a_body_line_wraps_and_is_never_cut".into());
    the_focus_op_before_the_chrome_op_still_grabs(&f)?;
    passed.push("the_focus_op_before_the_chrome_op_still_grabs".into());
    the_screen_is_driven_by_the_mouse_alone()?;
    passed.push("the_screen_is_driven_by_the_mouse_alone".into());
    the_screen_takes_the_keys_with_no_input_context()?;
    passed.push("the_screen_takes_the_keys_with_no_input_context".into());
    Ok(passed)
}

fn main() {
    // Mirrors `#[ignore]` under a plain `main`: `cargo test --workspace` needs no display.
    let args: Vec<String> = std::env::args().collect();
    if !args.iter().any(|a| a == "--ignored" || a == "--include-ignored") {
        println!("gtk_host_conformance: ignored (drives real GTK on a display); run with `-- --ignored`");
        return;
    }
    // Its own Xvfb, GTK initialised on it and checked to be on it -- never an inherited display.
    let server = own_x_server::init_gtk("gtk_host_conformance", "1024x768x24");
    match run_all() {
        Ok(passed) => {
            for name in &passed {
                println!("gtk_host_conformance: {name}: ok");
            }
            println!("gtk_host_conformance: ok ({} checks)", passed.len());
        }
        Err(e) => {
            eprintln!("gtk_host_conformance: FAILED: {e}");
            server.exit(1);
        }
    }
}
