//! The grid probe runs scripted operations on the real [`ModuleGrid`] and is compiled only
//! with `--features probe`. It exercises an embedded editor, terminal shells, and chat pages
//! through overlap, clipping, hiding, tab switches, reordering, and repeated creation and teardown.
//!
//! `EITRI_GRID_PROBE=<script.json>` starts the probe after the window is presented.
//! Operations use GLib timeouts to avoid blocking GTK, and the application quits when done.
//! The runner launches the window in an isolated compositor, supplies the script, and checks the log.
//!
//! Each step emits a `[probe]` line. Host unrealize logs detect unparenting, and `counts` lines
//! report each host's realize and unrealize totals. For compositor or process-tree checks,
//! a `sync` line waits for the runner's acknowledgement file. The runner compares costs with
//! the same run's calibration loop rather than asserting a fixed wall-clock bound.
//!
//! The only synthetic input is one pointer press for the `p1` probe, sent through the compositor's
//! virtual pointer.

mod hosts;
mod probes;
mod script;

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use gtk4::prelude::*;
use gtk4::{glib, graphene};
use neovide_editor::NeovideEditorPane;

use eitri_core::layout::{ModuleId, Rect};

use webkit6::WebView;

use crate::agent_panel::AgentPanelHandle;
use crate::module_grid::{ModuleGrid, ProbePlace};
use crate::terminal::TerminalPane;

/// What the probe needs from the window `build_ui` made.
pub(crate) struct Context {
    pub(crate) app: gtk4::Application,
    pub(crate) window: gtk4::ApplicationWindow,
    pub(crate) grid: ModuleGrid,
    pub(crate) pane: Rc<NeovideEditorPane>,
    /// The base terminal's pane, from the window's terminal registry (each numbered instance has its
    /// own pane there; the probe drives only the base one).
    pub(crate) terminal: TerminalPane,
    /// The base chat page's widget: a `WebView`, or the notice standing in for one.
    pub(crate) agent: gtk4::Widget,
    pub(crate) project_root: PathBuf,
    /// The window's own panel: every page the probe builds is attached to it, through the one
    /// constructor the window's chat views use, so what it measures is the window's routing.
    pub(crate) panel: AgentPanelHandle,
    /// A new chat view, as `instance.new { kind = "agent" }` makes one through the window's runner: the view made,
    /// or why none was.
    pub(crate) new_chat_view: Rc<dyn Fn() -> Result<ModuleId, String>>,
    /// The confirmed half of the window's kill of a chat module: what `prefix x` then `y` runs, with no
    /// key pressed.
    pub(crate) kill_chat_view: Rc<dyn Fn(&ModuleId)>,
    /// The window's pool of extra chat pages, read only.
    pub(crate) chat_views: crate::gtk_host_surfaces::ChatPool,
    /// Shows a module where it was hidden from, the keys staying where they are, as `eitri.layout.show` asks the
    /// window's controller: whether it was hidden and is shown now.
    pub(crate) show_module: Rc<dyn Fn(&ModuleId) -> bool>,
    /// Hides a module as `eitri.layout.hide` asks the window's controller, which gives its keys away first.
    pub(crate) hide_module: Rc<dyn Fn(&ModuleId)>,
}

/// Runs the script named by `EITRI_GRID_PROBE`, if the variable is set. Called once, after the window is
/// presented.
pub(crate) fn start_if_requested(ctx: Context) {
    let Some(path) = std::env::var_os("EITRI_GRID_PROBE") else {
        return;
    };
    let path = PathBuf::from(path);
    let script = match std::fs::read_to_string(&path)
        .map_err(|e| e.to_string())
        .and_then(|text| script::Script::parse(&text))
    {
        Ok(script) => script,
        Err(message) => {
            eprintln!("[probe] {}: {message}", path.display());
            ctx.app.quit();
            return;
        }
    };
    let dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
    let pooled: Vec<String> = ctx.chat_views.live().iter().map(|(id, _)| id.to_string()).collect();
    say(format!("chat-views live={}", pooled.join(",")));
    // Startup views have no restored tab, and the panel releases empty views on its first
    // tick. Give each an empty tab before the loop runs so the lifecycle probe kills a view
    // that the window actually holds.
    let startup: Vec<(ModuleId, usize)> = ctx
        .chat_views
        .live()
        .into_iter()
        .map(|(id, page)| (id, page_pointer(&page.view)))
        .collect();
    for (id, _) in &startup {
        if let Some(view) = crate::agent_panel::ViewId::of_module(id) {
            if let Err(err) = ctx.panel.open_in(view) {
                say(format!("warn {id} could not be given a tab: {err}"));
            }
        }
    }
    let probe = Rc::new(Probe::new(ctx, script, dir, startup));
    glib::spawn_future_local(async move { probe.run().await });
}

/// One host whose realize and unrealize events are counted from the moment the probe starts watching it.
struct Counter {
    name: String,
    realized: Rc<Cell<u32>>,
    unrealized: Rc<Cell<u32>>,
}

pub(crate) struct Probe {
    ctx: Context,
    script: script::Script,
    dir: PathBuf,
    seq: Cell<u32>,
    counters: RefCell<Vec<Counter>>,
    /// The pages and terminals the probe made, kept alive for the run.
    made: RefCell<hosts::Made>,
    /// The extra chat views the window opened with and the pointer of each one's `WebView`, as they were
    /// when the probe started.
    startup_views: Vec<(ModuleId, usize)>,
}

/// The identity of a page's `WebView` for the log: two reads of one view give one number, and two
/// views never share one while both are alive.
fn page_pointer(view: &WebView) -> usize {
    use gtk4::glib::object::ObjectType;
    view.as_ptr() as usize
}

/// One line of the log.
fn say(line: impl AsRef<str>) {
    println!("[probe] {}", line.as_ref());
}

impl Probe {
    fn new(ctx: Context, script: script::Script, dir: PathBuf, startup_views: Vec<(ModuleId, usize)>) -> Probe {
        Probe {
            ctx,
            script,
            dir,
            seq: Cell::new(0),
            counters: RefCell::new(Vec::new()),
            made: RefCell::new(hosts::Made::default()),
            startup_views,
        }
    }

    async fn run(self: Rc<Self>) {
        if !self.setup().await {
            say("abort the editor never became ready");
            self.ctx.app.quit();
            return;
        }
        let plan: Vec<String> = self.script.run.clone();
        for name in plan {
            say(format!("begin {name}"));
            self.snapshot(&format!("{name}.begin"));
            match name.as_str() {
                "cal" => self.calibration().await,
                "p1" => self.p1_overlap().await,
                "p2" => self.p2_tabbed().await,
                "p3" => self.p3_outside().await,
                "p4" => self.p4_reorder().await,
                "p5" => self.p5_web().await,
                "p6" => self.p6_pooled().await,
                "p7" => self.p7_second_chat().await,
                "p8" => self.p8_divider_pick().await,
                other => say(format!("skip {other} unknown")),
            }
            self.release_all();
            self.snapshot(&name);
            self.web_state(&format!("end-{name}")).await;
            say(format!("end {name}"));
        }
        say("done");
        // Let the last lines reach the runner, then end the loop. Not `window.close()`: that asks the
        // editor to quit first, and the probe's own buffer is not what that question is about.
        self.sleep(300).await;
        self.ctx.app.quit();
    }

    // ---- the log, the runner and the clock -------------------------------------------------------

    async fn sleep(&self, ms: u64) {
        glib::timeout_future(Duration::from_millis(ms)).await;
    }

    /// Asks the runner to act (`capture`, `sample`, `press`) and waits for its acknowledgement file. The
    /// runner reads the `sync` line from the log, so `fields` is `key=value` words.
    async fn sync(&self, name: &str, fields: &str) {
        let n = self.seq.get() + 1;
        self.seq.set(n);
        say(format!("sync {name} n={n} {fields}"));
        let ack = self.dir.join(format!("ack-{n}"));
        let deadline = Instant::now() + Duration::from_secs(120);
        while !ack.exists() {
            if Instant::now() > deadline {
                say(format!("sync-timeout {name} n={n}"));
                return;
            }
            glib::timeout_future(Duration::from_millis(5)).await;
        }
    }

    async fn sample(&self, label: &str) {
        self.sync("sample", &format!("label={label}")).await;
    }

    /// The runner captures `region` (window coordinates) as `label`; with `against` it also compares the
    /// sub-rectangle `part` of the new capture with the same rectangle of the capture stored under that
    /// label, and logs the verdict itself.
    async fn capture(&self, label: &str, region: Rect, against: Option<(&str, Rect)>) {
        self.capture_shifted(label, region, against, 0).await;
    }

    /// [`Self::capture`], where the comparison may find the picture moved by up to `shift` pixels: a host given
    /// a bigger grid of cells has its text moved by the leftover after the last whole cell, and by nothing else.
    async fn capture_shifted(&self, label: &str, region: Rect, against: Option<(&str, Rect)>, shift: i32) {
        let mut fields = format!(
            "label={label} x={} y={} w={} h={}",
            region.x, region.y, region.w, region.h
        );
        if let Some((reference, part)) = against {
            fields.push_str(&format!(
                " ref={reference} px={} py={} pw={} ph={}",
                part.x, part.y, part.w, part.h
            ));
        }
        if shift > 0 {
            fields.push_str(&format!(" shift={shift}"));
        }
        self.sync("capture", &fields).await;
    }

    // ---- counting what the hosts do ---------------------------------------------------------------

    /// Counts `widget`'s realize and unrealize events from now on, under `name`.
    fn track(&self, name: &str, widget: &gtk4::Widget) {
        let realized = Rc::new(Cell::new(0u32));
        let unrealized = Rc::new(Cell::new(0u32));
        {
            let realized = realized.clone();
            widget.connect_realize(move |_| realized.set(realized.get() + 1));
        }
        {
            let unrealized = unrealized.clone();
            widget.connect_unrealize(move |_| unrealized.set(unrealized.get() + 1));
        }
        self.counters.borrow_mut().push(Counter {
            name: name.to_owned(),
            realized,
            unrealized,
        });
    }

    /// `counts <label> <host>=<realized>/<unrealized> ...`: events since the probe began watching.
    fn snapshot(&self, label: &str) {
        let cells: Vec<String> = self
            .counters
            .borrow()
            .iter()
            .map(|c| format!("{}={}/{}", c.name, c.realized.get(), c.unrealized.get()))
            .collect();
        say(format!("counts {label} {}", cells.join(" ")));
    }

    // ---- geometry ---------------------------------------------------------------------------------

    fn grid_size(&self) -> (i32, i32) {
        (self.ctx.grid.width(), self.ctx.grid.height())
    }

    /// `widget`'s rectangle in the grid's coordinates.
    fn bounds_of(&self, widget: &gtk4::Widget) -> Rect {
        widget
            .compute_bounds(&self.ctx.grid)
            .map(|b| Rect {
                x: b.x().round() as i32,
                y: b.y().round() as i32,
                w: b.width().round() as i32,
                h: b.height().round() as i32,
            })
            .unwrap_or_default()
    }

    /// The grid's top-left corner in the window's coordinates.
    fn grid_origin(&self) -> (i32, i32) {
        self.ctx
            .grid
            .compute_point(&self.ctx.window, &graphene::Point::new(0.0, 0.0))
            .map(|p| (p.x().round() as i32, p.y().round() as i32))
            .unwrap_or((0, 0))
    }

    /// A rectangle of the grid, in the window's coordinates (what the runner captures and presses in).
    fn in_window(&self, r: Rect) -> Rect {
        let (ox, oy) = self.grid_origin();
        Rect {
            x: r.x + ox,
            y: r.y + oy,
            w: r.w,
            h: r.h,
        }
    }

    fn say_geom(&self, name: &str, r: Rect) {
        let w = self.in_window(r);
        say(format!("geom {name} x={} y={} w={} h={}", w.x, w.y, w.w, w.h));
    }

    /// The host widget the grid keeps for `id`.
    fn host(&self, id: &ModuleId) -> Option<gtk4::Widget> {
        self.ctx.grid.hosts().into_iter().find(|(m, _)| m == id).map(|(_, w)| w)
    }

    // ---- placing -----------------------------------------------------------------------------------

    fn place(&self, id: &ModuleId, r: Rect) {
        self.ctx.grid.probe_place(id, Some(ProbePlace::At(r)));
    }

    fn hide(&self, id: &ModuleId) {
        self.ctx.grid.probe_place(id, Some(ProbePlace::Hidden));
    }

    /// Gives every host the probe placed back to the layout, and hides the probe's own hosts.
    fn release_all(&self) {
        for id in self.made.borrow().ids() {
            self.hide(&id);
        }
        for id in [ModuleId::editor(), ModuleId::terminal(), ModuleId::agent()] {
            self.ctx.grid.probe_place(&id, None);
        }
    }

    /// Which module holds the window's focus: the first host that is the focus widget or above it.
    fn focus_owner(&self) -> String {
        self.owner_of(gtk4::prelude::GtkWindowExt::focus(&self.ctx.window))
    }

    /// The module whose host is `widget` or above it, `none` if it is in none.
    fn owner_of(&self, widget: Option<gtk4::Widget>) -> String {
        let hosts = self.ctx.grid.hosts();
        std::iter::successors(widget, |w| w.parent())
            .find_map(|w| hosts.iter().find(|(_, host)| *host == w).map(|(id, _)| id.to_string()))
            .unwrap_or_else(|| "none".to_owned())
    }

    /// Gives `id`'s focus target the keys. `false` if the module has no host or the grab was refused.
    fn give_keys(&self, id: &ModuleId) -> bool {
        self.ctx.grid.focus_target(id).is_some_and(|w| w.grab_focus())
    }
}
