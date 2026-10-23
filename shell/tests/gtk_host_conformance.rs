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
    Applied, ChatOp, CloseBy, CloseVerdict, DropWhy, EditorCall, EditorPresence, FocusTo, Host, HostOp, HostSink,
    PluginOp, SurfaceKind, TermOp, WinId, WindowRole, WindowSpec,
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

fn pump(ms: u64) {
    let context = gtk4::glib::MainContext::default();
    let end = Instant::now() + Duration::from_millis(ms);
    while Instant::now() < end {
        while context.iteration(false) {}
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// Builds each surface as a focusable button in the window's column, off screen until a `Map`.
struct StubMaker {
    host: Weak<gtk_host::GtkHost>,
    column: gtk4::glib::WeakRef<gtk4::Box>,
}

impl gtk_host::SurfaceMaker for StubMaker {
    fn create(&self, win: WinId, module: &ModuleId, _kind: &SurfaceKind) -> Result<(), String> {
        let (Some(host), Some(column)) = (self.host.upgrade(), self.column.upgrade()) else {
            return Err("the stage is gone".into());
        };
        let button = gtk4::Button::with_label(module.as_str());
        button.set_focusable(true);
        column.append(&button);
        button.set_child_visible(false);
        host.register_surface(win, module.clone(), button.upcast_ref());
        Ok(())
    }
    fn drop_surface(&self, _win: WinId, _module: &ModuleId, _why: &DropWhy) -> Result<(), String> {
        Ok(())
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
    fn chat(&self, _op: &ChatOp) -> Result<(), String> {
        Ok(())
    }
}

/// The window's grid as the host drives it: its children as slots, the fixture's hosts first, then a strip and
/// a divider, as the grid puts every host below them.
struct StubGrid {
    children: RefCell<Vec<Slot>>,
    allocations: Cell<usize>,
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
    fn notify_chrome_changed(&self) {}
    fn probe_places(&self, _module: &ModuleId) -> bool {
        false
    }
}

impl GridObserve for StubGrid {
    fn allocations_asked(&self, _win: WinId) -> usize {
        self.allocations.get()
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
    host.set_surface_maker(Rc::new(StubMaker {
        host: Rc::downgrade(&host),
        column: column.downgrade(),
    }));
    host.set_services(Rc::new(StubServices));
    let grid = StubGrid::new(f);
    host.set_grid(f.win, grid.clone());
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
        } = stage(f)?;
        drop(widgets);
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

/// What the controller's top-bar focus, chrome and digit ops reach: each the function the shell set for it,
/// answering what that function answered, and nothing before one is set.
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
    let chrome = HostOp::Chrome { win: f.win, rev: 3 };
    let ops = [
        top_bar.clone(),
        chrome.clone(),
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
    let drawn: Rc<RefCell<Vec<u64>>> = Rc::default();
    {
        let drawn = drawn.clone();
        s.host.set_chrome(Rc::new(move |_, rev| drawn.borrow_mut().push(rev)));
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
    if s.host.apply(&chrome) != Applied::Done || *drawn.borrow() != vec![3] {
        return Err(format!("the chrome was drawn for {:?}", drawn.borrow()));
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

fn run_all() -> Result<Vec<String>, String> {
    let f = Fixture {
        win: WinId::MAIN,
        a: ModuleId::agent(),
        b: ModuleId::terminal(),
        hidden: ModuleId::editor(),
    };
    let mut passed = Vec::new();
    for script in conformance::scripts(&f).into_iter().chain(own_scripts(&f)) {
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
    top_bar_chrome_and_hints_go_through_the_shells_functions(&f)?;
    passed.push("the_top_bar_chrome_and_hints_go_through_the_shells_functions_once_set".into());
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
