//! A web page laid out off screen and then swapped in, to see whether the first frame after the swap already shows
//! the page as it was prepared: no blank frame, no jump. GTK and WebKitGTK only; no Eitri code. Run it inside a
//! headless sandbox compositor, never on a desktop:
//!
//! ```text
//! mw_prepared_page_probe [--place right|left|below|under|front] [--load allocated|at-once] [--context own|shared]
//!                        [--nudge on|off] [--views reuse|new]
//! ```
//!
//! The window (titled `mw prepared page probe`, undecorated) holds one container laid out the way the module grid
//! lays out its hosts: a custom widget with `overflow: hidden` that allocates every child itself. Its left half is the
//! visible slot; its right half holds a plain label as a neighbour; a second host sits at the visible slot's size but
//! past the container's right edge, so it is child-visible, mapped and painted, and clipped out of sight.
//!
//! Each host holds one `WebView` with its own `WebContext` (a web process of its own, as each plugin page has). The
//! page is a long document (5,000 lines of 24 px on `#202020`), built by its own script, which after `load` scrolls so
//! that the target line sits at a given row of the viewport, paints that line in the page's marker colour (`A` green,
//! `B` magenta), and from the next `requestAnimationFrame` posts `ready <scrollY> <width>x<height> <visibility>` (its
//! scroll offset, its viewport and `document.visibilityState`).
//!
//! Where the hidden host goes is `--place`: `right` (the default: past the container's right edge), `left` or `below`
//! (past that edge), or `under` (in the visible slot itself, drawn before the host in front, so covered by it);
//! `front` prepares nothing: the new view is put in the visible slot before its page loads, to compare against.
//!
//! `--load` is when a new view starts loading its page: `allocated` (the default) waits for the first frame in which
//! the view is mapped with a non-empty allocation; `at-once` loads it as soon as it is put in its host.
//!
//! `--context own` (the default) gives each view a `WebContext` of its own, a web process of its own as each plugin
//! page has; `shared` puts every view in WebKit's default context.
//!
//! `--nudge on` gives a view's host one pixel less width for one allocation pass when its page is committed
//! (printed as `nudged <host> <t>`), so the view's size changes once while its web process runs.
//!
//! `--views reuse` (the default) creates both views at start, before the window is first shown, and loads each later
//! page into the view the hidden host already has; `new` puts every page after the first into a new view made then
//! (a `WebView` made after its window is shown got a viewport of 0x0 in the sandbox, whatever its place).
//!
//! At start page `A` loads into the visible slot and page `B` into the hidden host (with `--views new`, `B` waits for
//! `A`'s `ready` and gets a view made then); when `B` is ready the container swaps the two hosts in one allocation
//! pass: `B` where `A` was and `A` past the edge. Key `s` repeats the swap the other way: the hidden host loads the
//! other page at a new target line and row (into its view, or a new one), and the swap follows its `ready`.
//!
//! What it prints on stdout, one line each, times in microseconds of `CLOCK_MONOTONIC`:
//! - `shown <page> <line> <row> <t> <scrollY> <width>x<height> <visibility>` when the first page is ready in the
//!   visible slot;
//! - `key <n> <t>` when key `s` starts swap `n` (`busy <t>` when a swap is still being prepared);
//! - `loading <n> <page> <line> <row> <t>` when the hidden host starts loading swap `n`'s page;
//! - `swapped <n> <page> <line> <row> <t> <scrollY> <width>x<height> <visibility> gtk <width>x<height> <mapped>` when
//!   that page posted `ready` and the swap was asked for (the last three: the view's allocation and whether it was
//!   mapped, at that moment);
//! - `allocated <n> <t>` when the allocation pass that carries swap `n` ran;
//! - `painted <n> <t>` after the first frame the window painted with swap `n` in it;
//! - `slot <x> <y> <w> <h>` (window coordinates) whenever the visible slot's rectangle changes;
//! - `resized <page> <n> <width>x<height> <scrollY> <t>` each time a page's viewport changes size (`n` is 0 for the
//!   first page);
//! - `gone <page> <reason>` if a web process ended on its own.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk4 as gtk;
use gtk4::prelude::*;
use gtk4::subclass::prelude::*;
use gtk4::{gdk, glib};
use webkit6::prelude::*;
use webkit6::{UserContentManager, WebContext, WebView};

const TITLE: &str = "mw prepared page probe";
const LINES: u32 = 5_000;
const LINE_PX: u32 = 24;
const HANDLER: &str = "probe";
/// The two pages, by name and marker colour. The pass rule tells them apart by the marker alone.
const PAGES: [(&str, &str); 2] = [("A", "#30d030"), ("B", "#ff00ff")];

const PAGE_HTML: &str = r#"<!doctype html>
<html><head><meta charset="utf-8"><style>
html, body { margin: 0; padding: 0; background: #202020; color: #c8c8c8; font: 16px/24px monospace; }
div.l { height: 24px; padding-left: 48px; white-space: pre; overflow: hidden; }
div.m { background: @MARKER@; color: #000000; }
</style></head><body><script>
const lines = [];
for (let i = 0; i < @LINES@; i++) {
  const mark = i === @TARGET@ ? ' m' : '';
  lines.push('<div class="l' + mark + '">@PAGE@ ' + String(i).padStart(5, '0') +
             '  the quick brown fox jumps over the lazy dog</div>');
}
document.body.innerHTML = lines.join('');
window.addEventListener('resize', () => {
  const report = [window.innerWidth + 'x' + window.innerHeight, Math.round(window.scrollY)];
  window.webkit.messageHandlers.@HANDLER@.postMessage('resized ' + report.join(' '));
});
window.addEventListener('load', () => {
  window.scrollTo(0, (@TARGET@ - @ROW@) * @LINE_PX@);
  requestAnimationFrame(() => {
    const report = [Math.round(window.scrollY), window.innerWidth + 'x' + window.innerHeight, document.visibilityState];
    window.webkit.messageHandlers.@HANDLER@.postMessage('ready ' + report.join(' '));
  });
});
</script></body></html>
"#;

fn say(line: &str) {
    // Rust's stdout is line-buffered, so each line reaches a reading pipe as soon as it is printed.
    println!("{line}");
}

fn now_us() -> i64 {
    glib::monotonic_time()
}

/// Swap `n`'s target: a line near the middle of the document, a different one each time, and the viewport row it
/// is scrolled to, which moves by seven rows from one swap to the next so a frame left over from the page's previous
/// position cannot pass for the new one.
fn target(n: u32) -> (u32, u32) {
    (LINES / 2 - 500 + (n * 389) % 1000, 4 + (n * 7) % 30)
}

fn page_html(page: usize, line: u32, row: u32) -> String {
    let (name, marker) = PAGES[page];
    PAGE_HTML
        .replace("@MARKER@", marker)
        .replace("@LINES@", &LINES.to_string())
        .replace("@TARGET@", &line.to_string())
        .replace("@PAGE@", name)
        .replace("@ROW@", &row.to_string())
        .replace("@LINE_PX@", &LINE_PX.to_string())
        .replace("@HANDLER@", HANDLER)
}

mod imp {
    use super::*;

    pub type FrontHook = Box<dyn Fn(usize)>;

    /// Where the host that is not in front goes.
    #[derive(Clone, Copy, Default, PartialEq, Eq)]
    pub enum Place {
        #[default]
        Right,
        Left,
        Below,
        Under,
        /// No preparing: the new view is put in front before its page loads (to compare against).
        Front,
    }

    #[derive(Default)]
    pub struct ProbeGrid {
        /// The two hosts; `front` is the one in the visible slot.
        pub hosts: RefCell<Vec<gtk::Box>>,
        pub neighbour: RefCell<Option<gtk::Widget>>,
        pub front: Cell<usize>,
        pub place: Cell<Place>,
        /// The host the last allocation pass put in the visible slot, and the slot it gave it.
        pub allocated_front: Cell<Option<usize>>,
        pub allocated_slot: Cell<Option<(i32, i32, i32, i32)>>,
        /// A host given one pixel less width for one allocation pass, so its view's size changes once.
        pub nudge: Cell<Option<usize>>,
        /// Called from inside an allocation pass that put a different host in front than the pass before.
        pub on_front_allocated: RefCell<Option<FrontHook>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for ProbeGrid {
        const NAME: &'static str = "MwPreparedPageProbeGrid";
        type Type = super::ProbeGrid;
        type ParentType = gtk::Widget;
    }

    impl ObjectImpl for ProbeGrid {
        fn dispose(&self) {
            for host in std::mem::take(&mut *self.hosts.borrow_mut()) {
                host.unparent();
            }
            if let Some(neighbour) = self.neighbour.borrow_mut().take() {
                neighbour.unparent();
            }
        }
    }

    impl WidgetImpl for ProbeGrid {
        fn measure(&self, _orientation: gtk::Orientation, _for_size: i32) -> (i32, i32, i32, i32) {
            // The window decides the size.
            (0, 0, -1, -1)
        }

        fn size_allocate(&self, width: i32, height: i32, _baseline: i32) {
            self.obj().allocate_children(width, height);
        }

        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            // The host in front last, so that a host placed under it is covered.
            let obj = self.obj();
            let front = self.front.get();
            let hosts = self.hosts.borrow();
            for (index, host) in hosts.iter().enumerate() {
                if index != front {
                    obj.snapshot_child(host, snapshot);
                }
            }
            obj.snapshot_child(&hosts[front], snapshot);
            if let Some(neighbour) = self.neighbour.borrow().as_ref() {
                obj.snapshot_child(neighbour, snapshot);
            }
        }
    }
}

glib::wrapper! {
    pub struct ProbeGrid(ObjectSubclass<imp::ProbeGrid>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl ProbeGrid {
    fn new(place: imp::Place) -> Self {
        let grid: Self = glib::Object::new();
        grid.imp().place.set(place);
        // Clips every child to the container, so a host allocated past its edge is painted but never seen, and
        // can take no pointer event there either.
        grid.set_overflow(gtk::Overflow::Hidden);
        grid.set_hexpand(true);
        grid.set_vexpand(true);
        for _ in 0..2 {
            let host = gtk::Box::new(gtk::Orientation::Vertical, 0);
            host.set_parent(&grid);
            grid.imp().hosts.borrow_mut().push(host);
        }
        let neighbour = gtk::Label::new(Some("the neighbouring module"));
        neighbour.set_parent(&grid);
        *grid.imp().neighbour.borrow_mut() = Some(neighbour.upcast());
        grid
    }

    fn host(&self, index: usize) -> gtk::Box {
        self.imp().hosts.borrow()[index].clone()
    }

    /// Gives host `index` one pixel less width for one allocation pass, then its slot's width again.
    fn nudge(&self, index: usize) {
        self.imp().nudge.set(Some(index));
        self.queue_allocate();
        let grid = self.downgrade();
        glib::idle_add_local_once(move || {
            if let Some(grid) = grid.upgrade() {
                grid.imp().nudge.set(None);
                grid.queue_allocate();
            }
        });
    }

    fn front(&self) -> usize {
        self.imp().front.get()
    }

    /// Puts `index` in the visible slot and the other host past the edge, in the next allocation pass.
    fn swap_to(&self, index: usize) {
        self.imp().front.set(index);
        self.queue_allocate();
    }

    fn allocate_children(&self, width: i32, height: i32) {
        let imp = self.imp();
        let half = width / 2;
        let front = imp.front.get();
        let hosts = imp.hosts.borrow().clone();
        for (index, host) in hosts.iter().enumerate() {
            // The hidden host keeps the visible slot's size, so its page lays out at the width it will be shown at.
            let (x, y) = match imp.place.get() {
                _ if index == front => (0, 0),
                imp::Place::Right | imp::Place::Front => (width, 0),
                imp::Place::Left => (-half, 0),
                imp::Place::Below => (0, height),
                imp::Place::Under => (0, 0),
            };
            let _ = host.measure(gtk::Orientation::Horizontal, -1);
            let w = if imp.nudge.get() == Some(index) { half - 1 } else { half };
            let _ = host.measure(gtk::Orientation::Vertical, w);
            host.size_allocate(&gtk::Allocation::new(x, y, w, height), -1);
        }
        if let Some(neighbour) = imp.neighbour.borrow().as_ref() {
            let _ = neighbour.measure(gtk::Orientation::Horizontal, -1);
            let _ = neighbour.measure(gtk::Orientation::Vertical, width - half);
            neighbour.size_allocate(&gtk::Allocation::new(half, 0, width - half, height), -1);
        }
        let slot = (0, 0, half, height);
        if imp.allocated_slot.replace(Some(slot)) != Some(slot) {
            say(&format!("slot {} {} {} {}", slot.0, slot.1, slot.2, slot.3));
        }
        if imp.allocated_front.replace(Some(front)) != Some(front) {
            if let Some(hook) = imp.on_front_allocated.borrow().as_ref() {
                hook(front);
            }
        }
    }
}

/// What a host's view is loading or showing: its page, the swap it is for (none for the first page) and its target.
#[derive(Clone, Copy)]
struct Loading {
    page: usize,
    swap: Option<u32>,
    line: u32,
    row: u32,
}

struct Probe {
    grid: ProbeGrid,
    loading: [Cell<Option<Loading>>; 2],
    reuse_views: bool,
    load_at_once: bool,
    shared_context: bool,
    nudge: bool,
    /// The view in each host, and the page it shows.
    views: RefCell<[Option<(WebView, usize)>; 2]>,
    /// The swap whose page the hidden host is preparing.
    pending: Cell<Option<u32>>,
    swaps: Cell<u32>,
    /// The swap whose allocation ran and whose first painted frame is still to be reported.
    to_paint: Cell<Option<u32>>,
    /// The swap the next front change belongs to.
    swapping: Cell<Option<u32>>,
}

impl Probe {
    /// Loads `page` into host `host` in a new view with a web process of its own, replacing the view it had.
    /// Loads `page` into host `host`: into the view it has when views are reused, else into a new view (with a web
    /// process of its own unless the context is shared) that replaces it.
    fn load(self: &Rc<Self>, host: usize, page: usize, swap: Option<u32>, line: u32, row: u32) {
        self.loading[host].set(Some(Loading { page, swap, line, row }));
        let html = page_html(page, line, row);
        if self.reuse_views {
            if let Some((view, shown)) = self.views.borrow_mut()[host].as_mut() {
                *shown = page;
                view.load_html(&html, None);
                return;
            }
        }
        let container = self.grid.host(host);
        if let Some((old, _)) = self.views.borrow_mut()[host].take() {
            container.remove(&old);
        }
        let content = UserContentManager::new();
        content.register_script_message_handler(HANDLER, None);
        let view = if self.shared_context {
            WebView::builder().user_content_manager(&content).build()
        } else {
            WebView::builder()
                .web_context(&WebContext::new())
                .user_content_manager(&content)
                .build()
        };
        // What shows before the web process paints, as a page's theme background does.
        view.set_background_color(&gdk::RGBA::new(32.0 / 255.0, 32.0 / 255.0, 32.0 / 255.0, 1.0));
        view.set_hexpand(true);
        view.set_vexpand(true);
        {
            let probe = Rc::downgrade(self);
            let weak = view.downgrade();
            content.connect_script_message_received(Some(HANDLER), move |_, value| {
                let (Some(probe), Some(view)) = (probe.upgrade(), weak.upgrade()) else {
                    return;
                };
                // What the view is loading now: a reused view loads one page after another.
                let Some(Loading { page, swap, line, row }) = probe.loading[host].get() else {
                    return;
                };
                let text = value.to_str().to_string();
                if let Some(size) = text.strip_prefix("resized ") {
                    let (name, n) = (PAGES[page].0, swap.unwrap_or(0));
                    say(&format!("resized {name} {n} {} {}", size.trim(), now_us()));
                    return;
                }
                let Some(report) = text.strip_prefix("ready ") else {
                    return;
                };
                let mapped = if view.is_mapped() { "mapped" } else { "unmapped" };
                let report = format!("{} gtk {}x{} {mapped}", report.trim(), view.width(), view.height());
                probe.ready(host, page, swap, line, row, &report);
            });
        }
        {
            let probe = Rc::downgrade(self);
            view.connect_web_process_terminated(move |_, reason| {
                let page = probe
                    .upgrade()
                    .and_then(|probe| probe.loading[host].get())
                    .map_or("?", |l| PAGES[l.page].0);
                say(&format!("gone {page} {reason:?}"));
            });
        }
        if self.nudge {
            let grid = self.grid.downgrade();
            view.connect_load_changed(move |_, event| {
                if event == webkit6::LoadEvent::Committed {
                    if let Some(grid) = grid.upgrade() {
                        say(&format!("nudged {host} {}", now_us()));
                        grid.nudge(host);
                    }
                }
            });
        }
        container.append(&view);
        if self.load_at_once {
            view.load_html(&html, None);
        } else {
            view.add_tick_callback(move |view, _| {
                if !view.is_mapped() || view.width() == 0 || view.height() == 0 {
                    return glib::ControlFlow::Continue;
                }
                view.load_html(&html, None);
                glib::ControlFlow::Break
            });
        }
        self.views.borrow_mut()[host] = Some((view, page));
    }

    fn ready(self: &Rc<Self>, host: usize, page: usize, swap: Option<u32>, line: u32, row: u32, report: &str) {
        let name = PAGES[page].0;
        let Some(swap) = swap else {
            say(&format!("shown {name} {line} {row} {} {report}", now_us()));
            // The first page is up: prepare the other one in the hidden host, unless that began at start.
            if self.pending.get().is_none() && self.swaps.get() == 0 {
                self.start_swap(1);
            }
            return;
        };
        if self.pending.get() != Some(swap) {
            return;
        }
        self.pending.set(None);
        self.swapping.set(Some(swap));
        self.grid.swap_to(host);
        self.swaps.set(swap);
        say(&format!("swapped {swap} {name} {line} {row} {} {report}", now_us()));
    }

    fn start_swap(self: &Rc<Self>, swap: u32) {
        let host = 1 - self.grid.front();
        let shown = self.views.borrow()[self.grid.front()]
            .as_ref()
            .map_or(0, |(_, page)| *page);
        let page = 1 - shown;
        let (line, row) = target(swap);
        self.pending.set(Some(swap));
        if self.grid.imp().place.get() == imp::Place::Front {
            self.swapping.set(Some(swap));
            self.grid.swap_to(host);
        }
        say(&format!("loading {swap} {} {line} {row} {}", PAGES[page].0, now_us()));
        self.load(host, page, Some(swap), line, row);
    }

    fn key_swap(self: &Rc<Self>) {
        if self.pending.get().is_some() || self.views.borrow()[self.grid.front()].is_none() {
            say(&format!("busy {}", now_us()));
            return;
        }
        let swap = self.swaps.get() + 1;
        say(&format!("key {swap} {}", now_us()));
        self.start_swap(swap);
    }
}

fn main() {
    let usage = || -> ! {
        eprintln!(
            "usage: mw_prepared_page_probe [--place right|left|below|under|front] [--load allocated|at-once] \
             [--context own|shared] [--nudge on|off] [--views reuse|new]"
        );
        std::process::exit(2);
    };
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (mut place, mut load_at_once, mut shared_context, mut nudge) = (imp::Place::Right, false, false, false);
    let mut reuse_views = true;
    for pair in args.chunks(2) {
        match (pair[0].as_str(), pair.get(1).map(String::as_str)) {
            ("--nudge", Some("on")) => nudge = true,
            ("--views", Some("new")) => reuse_views = false,
            ("--views", Some("reuse")) => reuse_views = true,
            ("--nudge", Some("off")) => nudge = false,
            ("--place", Some("right")) => place = imp::Place::Right,
            ("--place", Some("left")) => place = imp::Place::Left,
            ("--place", Some("below")) => place = imp::Place::Below,
            ("--place", Some("under")) => place = imp::Place::Under,
            ("--place", Some("front")) => place = imp::Place::Front,
            ("--load", Some("allocated")) => load_at_once = false,
            ("--load", Some("at-once")) => load_at_once = true,
            ("--context", Some("own")) => shared_context = false,
            ("--context", Some("shared")) => shared_context = true,
            _ => usage(),
        }
    }
    gtk::init().expect("GTK could not open a display");
    let main_loop = glib::MainLoop::new(None, false);

    let grid = ProbeGrid::new(place);
    let window = gtk::Window::builder()
        .title(TITLE)
        .decorated(false)
        .default_width(1600)
        .default_height(1000)
        .child(&grid)
        .build();
    let probe = Rc::new(Probe {
        grid: grid.clone(),
        loading: [Cell::new(None), Cell::new(None)],
        reuse_views,
        load_at_once,
        shared_context,
        nudge,
        views: RefCell::new([None, None]),
        pending: Cell::new(None),
        swaps: Cell::new(0),
        to_paint: Cell::new(None),
        swapping: Cell::new(None),
    });
    {
        let probe = Rc::downgrade(&probe);
        *grid.imp().on_front_allocated.borrow_mut() = Some(Box::new(move |_front| {
            let Some(probe) = probe.upgrade() else { return };
            if let Some(swap) = probe.swapping.take() {
                say(&format!("allocated {swap} {}", now_us()));
                probe.to_paint.set(Some(swap));
            }
        }));
    }
    {
        let probe = Rc::downgrade(&probe);
        window.connect_realize(move |w| {
            let Some(clock) = w.frame_clock() else { return };
            let probe = probe.clone();
            clock.connect_after_paint(move |_| {
                let Some(probe) = probe.upgrade() else { return };
                if let Some(swap) = probe.to_paint.take() {
                    say(&format!("painted {swap} {}", now_us()));
                }
            });
        });
    }
    {
        // In the capture phase on the window, so the key is the probe's even while a page has the focus.
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        let probe = Rc::downgrade(&probe);
        keys.connect_key_pressed(move |_, key, _, _| {
            if key != gdk::Key::s {
                return glib::Propagation::Proceed;
            }
            if let Some(probe) = probe.upgrade() {
                probe.key_swap();
            }
            glib::Propagation::Stop
        });
        window.add_controller(keys);
    }
    {
        let main_loop = main_loop.clone();
        window.connect_close_request(move |_| {
            main_loop.quit();
            glib::Propagation::Proceed
        });
    }
    window.present();
    let (line, row) = target(0);
    probe.load(0, 0, None, line, row);
    if reuse_views {
        // Both views exist before the window is first shown: the hidden one starts the first swap now.
        probe.start_swap(1);
    }
    // Nothing else reads stdin; its end is the harness going away.
    {
        let main_loop = main_loop.clone();
        std::thread::spawn(move || {
            let mut sink = String::new();
            while std::io::stdin().read_line(&mut sink).is_ok_and(|n| n > 0) {
                sink.clear();
            }
            main_loop.quit();
        });
    }
    main_loop.run();
}
