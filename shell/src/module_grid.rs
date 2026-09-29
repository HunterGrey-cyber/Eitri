//! `ModuleGrid`: the one container every module lives in (modules spec §5). It replaces
//! `layout::build_content_area`, `layout::build_vertical_split` and `PaneLayout`'s index matches.
//!
//! **It never reparents.** Every module host gets its parent exactly once, in [`ModuleGrid::add`],
//! and loses it only in `dispose`, when the window goes away. (A web module's `WebView` gets its own
//! once, before that: its [`WebHost`], `web_host`'s module doc.) Hiding is `set_child_visible(false)`;
//! zoom, resize, split, swap, move and even (modules P2) are new allocations of the same children. The
//! reason is the editor: its Skia `DirectContext` is made once, on the `GLArea`'s GL context, and
//! nothing in `neovide-editor` survives an unrealize (spec §2) -- and unparenting unrealizes. S3
//! drove 50 hides, 50 swaps and 13+ sweeps of 1,000 divider moves through a prototype of this
//! container with the editor's unrealize count at 0 throughout. Every host logs its own unrealize
//! ([`ModuleGrid::add`]), so a GUI pass reads that invariant off the log instead of trusting it.
//!
//! **What it decides and what it does not.** The rectangles come from
//! `neovibe_core::layout::arrange`; this file only measures children (their minimum sizes feed the
//! geometry), allocates what the geometry says, holds web hosts at their settled size while they
//! are being resized ([`throttle`]), and turns a divider drag back into a ratio.

mod throttle;

use std::cell::{Cell, RefCell};
use std::collections::BTreeSet;
use std::rc::Rc;

use gtk4::glib;
use gtk4::graphene;
use gtk4::prelude::*;
use gtk4::subclass::prelude::*;

use neovibe_core::layout::{
    arrange, Axis, Direction, Divider, Frame, Layout, LayoutError, ModuleId, Nav, Rect, Reopen, Size, ZoomChange,
};

use throttle::{WebThrottle, QUIET};

use crate::web_host::WebHost;

/// The grid's CSS node name. `theme::gtk_css` styles its dividers as `modulegrid.content-area >
/// separator`, the rule `paned.content-area > separator` was before the grid replaced the paneds.
pub(crate) const CSS_NAME: &str = "modulegrid";

/// How a host follows its rectangle while it changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HostKind {
    /// Allocated its rectangle on every pass, as the editor always was.
    Direct,
    /// A `WebView`, in its [`WebHost`]: held at its settled size while being resized ([`throttle`]).
    Web,
}

/// The content size the geometry is computed against before the grid's first allocation: the
/// default window (1280x760) minus the top bar, 39px by its CSS (`tree::BELOW_ROOT_SHARE` has the
/// arithmetic; not measured). Only a key pressed before the window is on screen could see it --
/// and `main.rs`'s hiding of the bottom terminal in the initial layout, which reads no geometry at
/// all unless the terminal held the keys, which at startup nothing does.
pub(crate) const UNALLOCATED: Size = Size { w: 1280, h: 721 };

/// Every module and its host, asked for at the moment it is needed, so a module added after the
/// window is up is never missing from it. In whatever order the closure's builder promises:
/// `ModuleGrid::live_hosts` is add order, and HINT's (`HintWidgets::modules`) is the layout's tree
/// order.
pub(crate) type ModuleHosts = Rc<dyn Fn() -> Vec<(ModuleId, gtk4::Widget)>>;

/// The size the geometry is computed against: the grid's allocation, or [`UNALLOCATED`] while GTK
/// still reports none (0x0 until the first allocation).
fn frame_size(width: i32, height: i32) -> Size {
    if width > 0 && height > 0 {
        Size { w: width, h: height }
    } else {
        UNALLOCATED
    }
}

/// A module's host and its focus target. Generic over the widget only so [`focus_target_of`] is
/// tested on these same records without a display; the grid holds `gtk4::Widget`s.
struct Host<W = gtk4::Widget> {
    id: ModuleId,
    widget: W,
    /// What takes the keys when the module is given them ([`ModuleGrid::add`]'s `focus`).
    focus: W,
    kind: HostKind,
}

/// A divider on screen, and the divider it currently draws (read by its drag).
struct Handle {
    widget: gtk4::Separator,
    current: Rc<RefCell<Option<Divider>>>,
}

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct ModuleGrid {
        pub(super) layout: RefCell<Option<Rc<RefCell<Layout>>>>,
        pub(super) hosts: RefCell<Vec<Host>>,
        pub(super) handles: RefCell<Vec<Handle>>,
        pub(super) throttle: RefCell<WebThrottle>,
        pub(super) settle: RefCell<Option<glib::SourceId>>,
        /// The modules allocated in the previous pass (the throttle's "was on screen").
        pub(super) on_screen: RefCell<BTreeSet<ModuleId>>,
        /// Web hosts allocated at a held size this pass, with the rectangle each is clipped to.
        pub(super) clips: RefCell<Vec<(gtk4::Widget, Rect)>>,
        /// Called after every change to the layout ([`ModuleGrid::connect_changed`]).
        pub(super) changed: RefCell<Vec<Rc<dyn Fn()>>>,
        /// Called after a pinned split got its length ([`ModuleGrid::connect_settled`]).
        pub(super) settled: RefCell<Vec<Rc<dyn Fn()>>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for ModuleGrid {
        const NAME: &'static str = "NeovibeModuleGrid";
        type Type = super::ModuleGrid;
        type ParentType = gtk4::Widget;

        fn class_init(klass: &mut Self::Class) {
            klass.set_css_name(super::CSS_NAME);
        }
    }

    impl ObjectImpl for ModuleGrid {
        fn dispose(&self) {
            if let Some(source) = self.settle.borrow_mut().take() {
                source.remove();
            }
            // The only place a child ever loses its parent: the window is going away. Taken out of
            // the `RefCell`s first: an unparent unrealizes, and nothing that runs then should find
            // them borrowed.
            let hosts = std::mem::take(&mut *self.hosts.borrow_mut());
            for host in hosts {
                host.widget.unparent();
            }
            let handles = std::mem::take(&mut *self.handles.borrow_mut());
            for handle in handles {
                handle.widget.unparent();
            }
        }
    }

    impl WidgetImpl for ModuleGrid {
        fn measure(&self, orientation: gtk4::Orientation, _for_size: i32) -> (i32, i32, i32, i32) {
            // The window decides the size and the tree divides it; the grid asks only for what is on
            // screen needs at the least (`neovibe_core::layout::min_size`), so the window cannot be
            // made smaller than a module's floor -- the paneds' `shrink = false` did the same.
            let min = self.obj().layout_min_size();
            let length = match orientation {
                gtk4::Orientation::Horizontal => min.w,
                _ => min.h,
            };
            (length, length, -1, -1)
        }

        fn size_allocate(&self, width: i32, height: i32, _baseline: i32) {
            self.obj().allocate_children(width, height);
        }

        fn snapshot(&self, snapshot: &gtk4::Snapshot) {
            self.obj().snapshot_children(snapshot);
        }
    }
}

glib::wrapper! {
    // `pub`, not `pub(crate)`: `ObjectSubclass::Type` must be at least as visible as the subclass.
    // The module itself is private to the crate.
    pub struct ModuleGrid(ObjectSubclass<imp::ModuleGrid>)
        @extends gtk4::Widget,
        @implements gtk4::Accessible, gtk4::Buildable, gtk4::ConstraintTarget;
}

impl ModuleGrid {
    pub(crate) fn new(layout: Rc<RefCell<Layout>>) -> Self {
        let grid: Self = glib::Object::new();
        // A held web host's allocation may reach past the grid's own edge (`throttle`'s module
        // doc, sw-layout-2). `Hidden` makes GTK's pick return nothing for a point outside the grid
        // (`gtk_widget_do_pick`, gtkwidget.c, GTK 4.22.5: an early return for a point outside the
        // padding box), so such an allocation can never take a click from the top bar above it,
        // and clips its paint to the grid as well.
        grid.set_overflow(gtk4::Overflow::Hidden);
        grid.add_css_class("content-area");
        grid.set_hexpand(true);
        grid.set_vexpand(true);
        *grid.imp().layout.borrow_mut() = Some(layout);
        grid
    }

    fn layout(&self) -> Rc<RefCell<Layout>> {
        self.imp()
            .layout
            .borrow()
            .clone()
            .expect("ModuleGrid::new always sets the layout")
    }

    /// The ONLY place a module host gets a parent. Each host after the first also brings one
    /// divider into the pool: a tree over n leaves has at most n - 1 splits, so every divider the
    /// layout can ever need exists before the first allocation and none is created during one.
    ///
    /// **A host may be added after `present()`** (P3's canvas, created on its first draw). Focus
    /// tracking, `focus_module` and the prefix read the grid live since modules P2
    /// ([`ModuleGrid::live_hosts`], [`ModuleGrid::hosts`]), and HINT always did
    /// ([`ModuleGrid::hosts_in_tree_order`]). The one thing a later host must do for itself is the
    /// web modules' `Ctrl+h/j/k/l` controller: `main.rs`'s `install_module_nav`, called once per
    /// web host as it is added.
    ///
    /// **A host is stacked below every divider handle** ([`host_stack_index`]): GTK hands a press to
    /// the last child that contains it, and a host contains the first pixel column of the divider
    /// on its right. Appended after a handle, a Lua `main` panel took every press on the root
    /// divider (the 2026-09-23 GUI pass, item 11). The handles themselves are appended
    /// ([`Self::new_handle`]), so they stay above every host.
    ///
    /// **`focus` is what takes the keys when the module is given them** ([`ModuleGrid::focus_target`]):
    /// the host itself, or a widget inside it. It is not always the host. The editor's host is the
    /// overlay carrying its start-failure notice (`editor_start_failure`), and a web module's is a
    /// [`WebHost`] around its `WebView` (2026-09-29), which is not focusable itself -- it forwards a
    /// grab to its one child, but module focus does not rely on that; `web_host`'s module doc says why
    /// it is there. A `HostKind::Web` host must be a `WebHost`: a `WebView` placed in the grid
    /// directly has its input method's candidate window off by the module's position -- and a
    /// `WebHost`'s focus target is the one child it holds. These are wiring rules, checked here once
    /// per module as it is added.
    pub(crate) fn add(
        &self,
        id: ModuleId,
        widget: &impl IsA<gtk4::Widget>,
        focus: &impl IsA<gtk4::Widget>,
        kind: HostKind,
    ) {
        let widget: gtk4::Widget = widget.clone().upcast();
        let focus: gtk4::Widget = focus.clone().upcast();
        assert!(
            focus == widget || focus.is_ancestor(&widget),
            "{id}: its focus target must be its host or inside it"
        );
        if let Some(web) = widget.downcast_ref::<WebHost>() {
            assert!(
                web.content().as_ref() == Some(&focus),
                "{id}: a WebHost's focus target is the one child it holds"
            );
        }
        assert!(
            kind != HostKind::Web || widget.is::<WebHost>(),
            "{id}: a web module's host must be a WebHost (web_host's module doc)"
        );
        {
            let handles: Vec<gtk4::Widget> = self
                .imp()
                .handles
                .borrow()
                .iter()
                .map(|h| h.widget.clone().upcast())
                .collect();
            let mut children = Vec::new();
            let mut child = self.first_child();
            while let Some(c) = child {
                child = c.next_sibling();
                children.push(c);
            }
            match children.get(host_stack_index(&children, |c| handles.contains(c))) {
                // Parenting at a position, not reparenting: this is the host's first and only parent.
                Some(first_handle) => widget.insert_before(self, Some(first_handle)),
                None => widget.set_parent(self),
            }
        }
        let unrealized = Rc::new(Cell::new(0u32));
        {
            let id = id.clone();
            widget.connect_unrealize(move |_| {
                unrealized.set(unrealized.get() + 1);
                // Expected exactly once per host, when the window closes. Any earlier line is the
                // bug this container exists to rule out (spec §5: "held by a counter").
                eprintln!(
                    "[module_grid] {id} unrealized ({} so far) -- must not happen before the window closes",
                    unrealized.get()
                );
            });
        }
        let needs_handle = !self.imp().hosts.borrow().is_empty();
        self.imp().hosts.borrow_mut().push(Host {
            id,
            widget,
            focus,
            kind,
        });
        if needs_handle {
            let handle = self.new_handle();
            self.imp().handles.borrow_mut().push(handle);
        }
    }

    fn new_handle(&self) -> Handle {
        // Vertical (a `Row` split's divider) until `place_handles` says otherwise; the cursor is
        // the one `GtkPaned` shows over the same divider.
        let widget = gtk4::Separator::new(gtk4::Orientation::Vertical);
        widget.set_cursor_from_name(Some("col-resize"));
        widget.set_parent(self);
        widget.set_child_visible(false);
        let current: Rc<RefCell<Option<Divider>>> = Rc::new(RefCell::new(None));
        // The divider the drag grabbed: which split it moves.
        let dragging: Rc<RefCell<Option<Divider>>> = Rc::new(RefCell::new(None));
        let drag = gtk4::GestureDrag::new();
        {
            let current = current.clone();
            let dragging = dragging.clone();
            drag.connect_drag_begin(move |gesture, _, _| {
                // Claimed: the press is the divider's, and nothing else acts on it.
                gesture.set_state(gtk4::EventSequenceState::Claimed);
                *dragging.borrow_mut() = current.borrow().clone();
            });
        }
        {
            let current = current.clone();
            let dragging = dragging.clone();
            let grid = self.downgrade();
            drag.connect_drag_update(move |_, dx, dy| {
                let Some(grid) = grid.upgrade() else { return };
                let (Some(begin), Some(now)) = (dragging.borrow().clone(), current.borrow().clone()) else {
                    return;
                };
                let Some(first_px) = dragged_first_px(&begin, &now, dx, dy) else {
                    return;
                };
                // A ratio on a ratio split, pixels on a pinned one (`move_divider`): a dragged bottom
                // row keeps the height it was dragged to when the window grows.
                let moved = neovibe_core::layout::move_divider(&mut grid.layout().borrow_mut(), &now, first_px);
                if moved.is_ok() {
                    grid.queue_allocate();
                    grid.notify_changed();
                }
            });
        }
        drag.connect_drag_end(move |_, _, _| {
            dragging.borrow_mut().take();
        });
        widget.add_controller(drag);
        Handle { widget, current }
    }

    /// [`ModuleGrid::hosts`], as a function the caller keeps and asks again each time.
    pub(crate) fn live_hosts(&self) -> ModuleHosts {
        let grid = self.downgrade();
        Rc::new(move || grid.upgrade().map(|grid| grid.hosts()).unwrap_or_default())
    }

    /// Every module and its host, in the order they were added.
    pub(crate) fn hosts(&self) -> Vec<(ModuleId, gtk4::Widget)> {
        self.imp()
            .hosts
            .borrow()
            .iter()
            .map(|h| (h.id.clone(), h.widget.clone()))
            .collect()
    }

    /// The widget that takes the keys when `id` is given them ([`ModuleGrid::add`]'s `focus`), never
    /// a web module's host, which is not focusable itself (`web_host`'s module doc). `None` for a
    /// module this grid has no host for. Read live, as [`ModuleGrid::hosts`] is. The choice is
    /// [`focus_target_of`]'s, on the grid's own records.
    pub(crate) fn focus_target(&self, id: &ModuleId) -> Option<gtk4::Widget> {
        focus_target_of(&self.imp().hosts.borrow(), id).cloned()
    }

    /// Every module and its host in the layout's tree order: HINT's order.
    pub(crate) fn hosts_in_tree_order(&self) -> Vec<(ModuleId, gtk4::Widget)> {
        let leaves = self.layout().borrow().leaves();
        let hosts = self.hosts();
        leaves
            .into_iter()
            .filter_map(|id| hosts.iter().find(|(m, _)| *m == id).cloned())
            .collect()
    }

    /// Makes each host's visibility match the layout, then re-measures and re-allocates: a hide or a
    /// zoom changes what the grid needs at the least (`measure`). Call after every layout change.
    /// The layout is read, and released, before any child's visibility changes: hiding the focused
    /// widget moves GTK focus, which re-enters `pane_focus` and writes the layout.
    pub(crate) fn apply(&self) {
        let wanted: Vec<(gtk4::Widget, bool)> = {
            let layout = self.layout();
            let layout = layout.borrow();
            self.imp()
                .hosts
                .borrow()
                .iter()
                .map(|h| (h.widget.clone(), layout.is_visible(&h.id)))
                .collect()
        };
        for (widget, visible) in wanted {
            if widget.is_child_visible() != visible {
                widget.set_child_visible(visible);
            }
        }
        self.queue_resize();
        self.notify_changed();
    }

    /// `f` runs after every change to the layout: each [`ModuleGrid::apply`] and a divider drag. A
    /// pinned split getting its length (`settle_pins`, inside an allocation) is not one: nothing
    /// moved on screen, and the state file's save that follows the change which put the split on
    /// screen -- 500ms later -- already holds the length. What hangs off it: the per-project state
    /// file's debounced save, the top bar's tray, and starting the terminal's shell once it is shown
    /// however it got there. Runs with the layout released.
    pub(crate) fn connect_changed(&self, f: impl Fn() + 'static) {
        self.imp().changed.borrow_mut().push(Rc::new(f));
    }

    fn notify_changed(&self) {
        // Cloned out: a hook may add another.
        let hooks: Vec<Rc<dyn Fn()>> = self.imp().changed.borrow().clone();
        for hook in hooks {
            hook();
        }
    }

    /// `f` runs after a pinned split got its length (`settle_pins`, inside an allocation) -- which
    /// [`ModuleGrid::connect_changed`] does not report, since nothing moved -- so the state file's
    /// saver can count the length as what the window opened with rather than as a change. Runs with
    /// the layout released, inside `size_allocate`: a hook must not touch a widget.
    pub(crate) fn connect_settled(&self, f: impl Fn() + 'static) {
        self.imp().settled.borrow_mut().push(Rc::new(f));
    }

    /// `Ctrl+a \ <key>` / `Ctrl+a " <key>`: `module` into a new split after `target` along `axis`,
    /// moved there if it is elsewhere, shown if hidden (`neovibe_core::layout::place`). The layout
    /// gives `module` the keys; the caller gives it GTK focus.
    pub(crate) fn place_module(&self, module: &ModuleId, target: &ModuleId, axis: Axis) -> Result<(), LayoutError> {
        let hosted: Vec<ModuleId> = self.hosts().into_iter().map(|(id, _)| id).collect();
        placeable(&hosted, module)?;
        neovibe_core::layout::place(&mut self.layout().borrow_mut(), module, target, axis)?;
        self.apply();
        Ok(())
    }

    /// `Ctrl+a H/J/K/L` (`neovibe_core::layout::swap`). The neighbour, if they swapped.
    pub(crate) fn swap_modules(&self, focused: &ModuleId, direction: Direction) -> Option<ModuleId> {
        let size = self.size();
        let swapped = {
            let layout = self.layout();
            let mut layout = layout.borrow_mut();
            self.with_frame(size, |frame| {
                neovibe_core::layout::swap(&mut layout, focused, direction, frame)
            })
        };
        if swapped.is_some() {
            self.apply();
        }
        swapped
    }

    /// `swap.prev`/`swap.next` (`neovibe_core::layout::swap_adjacent`).
    pub(crate) fn swap_adjacent(&self, focused: &ModuleId, forward: bool) -> Option<ModuleId> {
        let swapped = {
            let layout = self.layout();
            let mut layout = layout.borrow_mut();
            neovibe_core::layout::swap_adjacent(&mut layout, focused, forward)
        };
        if swapped.is_some() {
            self.apply();
        }
        swapped
    }

    /// `Ctrl+a |` / `Ctrl+a _` (`neovibe_core::layout::even`).
    pub(crate) fn even_modules(&self, axis: Axis) {
        neovibe_core::layout::even(&mut self.layout().borrow_mut(), axis);
        self.apply();
    }

    /// The divider thickness: the separators' own CSS size (`.content-area > separator`, 1px).
    fn handle_px(&self) -> i32 {
        self.imp()
            .handles
            .borrow()
            .first()
            .map(|h| {
                let (w, ..) = h.widget.measure(gtk4::Orientation::Horizontal, -1);
                let (hgt, ..) = h.widget.measure(gtk4::Orientation::Vertical, -1);
                w.max(hgt)
            })
            .unwrap_or(1)
            .max(1)
    }

    /// What the layout needs at the least, as the hosts measure now (`measure` reports it). A layout
    /// that is borrowed for writing right now -- nothing measures the grid inside one of its own
    /// writes, but a mistake should cost a frame, not a panic -- asks for nothing this once.
    fn layout_min_size(&self) -> Size {
        let layout = self.layout();
        let Ok(layout) = layout.try_borrow() else {
            return Size::default();
        };
        self.with_frame(self.size(), |frame| neovibe_core::layout::min_size(&layout, frame))
    }

    /// A module's minimum size, as its host measures it.
    fn min_size(&self, id: &ModuleId) -> Size {
        let hosts = self.imp().hosts.borrow();
        let Some(host) = hosts.iter().find(|h| h.id == *id) else {
            return Size::default();
        };
        let (w, ..) = host.widget.measure(gtk4::Orientation::Horizontal, -1);
        let (h, ..) = host.widget.measure(gtk4::Orientation::Vertical, -1);
        Size { w, h }
    }

    fn size(&self) -> Size {
        frame_size(self.width(), self.height())
    }

    /// Runs `f` with the frame the layout is currently allocated in.
    fn with_frame<R>(&self, size: Size, f: impl FnOnce(&Frame) -> R) -> R {
        let min = |id: &ModuleId| self.min_size(id);
        f(&Frame {
            size,
            handle_px: self.handle_px(),
            min: &min,
        })
    }

    /// `Ctrl+a m`/`z` on `target`, or the end of the zoom that is on.
    pub(crate) fn toggle_zoom(&self, target: &ModuleId) {
        let change = self.layout().borrow_mut().toggle_zoom(target);
        match change {
            ZoomChange::Zoomed => println!("[module_grid] zoomed {target}"),
            ZoomChange::Unzoomed => println!("[module_grid] unzoomed"),
            ZoomChange::Nothing => return,
        }
        self.apply();
    }

    /// Ends a zoom, as tmux's `select-pane` and `resize-pane` do first. `false` if none was on.
    pub(crate) fn unzoom(&self) -> bool {
        let ended = self.layout().borrow_mut().unzoom();
        if ended {
            println!("[module_grid] unzoomed");
            self.apply();
        }
        ended
    }

    /// `Ctrl+a h/j/k/l` around `target` by `px` (`neovibe_core::layout::resize`).
    pub(crate) fn resize(&self, target: &ModuleId, direction: Direction, px: i32) {
        let size = self.size();
        let moved = {
            let layout = self.layout();
            let mut layout = layout.borrow_mut();
            self.with_frame(size, |frame| {
                neovibe_core::layout::resize(&mut layout, target, direction, px, frame)
            })
        };
        if moved {
            self.apply();
        }
    }

    /// Where `Ctrl+h/j/k/l` from `from` goes, on the geometry the grid is allocated with
    /// (`neovibe_core::layout::navigate`). Decides only; the caller unzooms and moves focus.
    pub(crate) fn navigate(&self, from: &ModuleId, direction: Direction) -> Nav {
        let size = self.size();
        let layout = self.layout();
        let layout = layout.borrow();
        self.with_frame(size, |frame| {
            neovibe_core::layout::navigate(&layout, from, direction, frame)
        })
    }

    /// Hides `id`. If it holds the keys, the module the layout chooses gets them through `focus`
    /// **before** `id` is unmapped (S3 change 3: GTK's own focus chain handed them to the WebView).
    /// The order is [`hide_then_unmap`]'s, which a test without a display holds; this method only
    /// hands it the grid's frame and its two GTK effects. Every hide goes through it: `Ctrl+a x`, a
    /// module key held by the module with the keys, `Ctrl+a t`, and `neovibe.layout.hide`.
    pub(crate) fn hide_module(&self, id: &ModuleId, focus: &dyn Fn(&ModuleId) -> bool) -> Result<(), LayoutError> {
        let size = self.size();
        let layout = self.layout();
        self.with_frame(size, |frame| {
            hide_then_unmap(
                &layout,
                id,
                frame,
                |next| {
                    focus(next);
                },
                || self.apply(),
            )
        })
    }

    /// `prefix x` (`neovibe_core::layout::kill`): `id` leaves the screen as a hide does -- the keys to
    /// the module the layout chooses first, then the unmap, in [`hide_then_unmap`]'s order -- and is
    /// left where `reopen` says. Ending what ran in it is the caller's.
    pub(crate) fn kill_module(
        &self,
        id: &ModuleId,
        reopen: Reopen,
        focus: &dyn Fn(&ModuleId) -> bool,
    ) -> Result<(), LayoutError> {
        let size = self.size();
        let layout = self.layout();
        self.with_frame(size, |frame| {
            leave_then_unmap(
                &layout,
                |layout| neovibe_core::layout::kill(layout, id, reopen, frame),
                |next| {
                    focus(next);
                },
                || self.apply(),
            )
        })
    }

    /// Shows `id` where it was hidden from. `Ok(false)` if it was already shown. First caller:
    /// `Ctrl+a t` showing the bottom terminal.
    pub(crate) fn show_module(&self, id: &ModuleId) -> Result<bool, LayoutError> {
        let shown = self.layout().borrow_mut().show(id)?;
        if shown {
            self.apply();
        }
        Ok(shown)
    }

    fn allocate_children(&self, width: i32, height: i32) {
        let imp = self.imp();
        // A pinned split that is on screen for the first time keeps the length its ratio gives it at
        // this, the real allocation (`settle_pins`); never at `UNALLOCATED`'s stand-in size. Asked
        // first (`has_pin_to_settle`, no geometry), so a drag or a resize sweep arranges once per
        // allocation, as it did before pins.
        if width > 0
            && height > 0
            && self
                .layout()
                .try_borrow()
                .is_ok_and(|layout| layout.has_pin_to_settle())
        {
            let settled = self.layout().try_borrow_mut().is_ok_and(|mut layout| {
                self.with_frame(Size { w: width, h: height }, |frame| {
                    neovibe_core::layout::settle_pins(&mut layout, frame)
                })
            });
            if settled {
                let hooks: Vec<Rc<dyn Fn()>> = imp.settled.borrow().clone();
                for hook in hooks {
                    hook();
                }
            }
        }
        let arrangement = {
            let layout = self.layout();
            let layout = layout.borrow();
            self.with_frame(Size { w: width, h: height }, |frame| arrange(&layout, frame))
        };
        let was_on_screen = imp
            .on_screen
            .replace(arrangement.modules.iter().map(|(id, _)| id.clone()).collect());
        let mut clips = Vec::new();
        let mut held = false;
        let mut restart_quiet = false;
        for host in imp.hosts.borrow().iter() {
            let Some(target) = arrangement.rect_of(&host.id) else {
                continue;
            };
            let rect = match host.kind {
                HostKind::Direct => target,
                HostKind::Web => {
                    let web = imp.throttle.borrow_mut().allocate(
                        &host.id,
                        target,
                        was_on_screen.contains(&host.id),
                        Size { w: width, h: height },
                    );
                    if web.held {
                        held = true;
                        clips.push((host.widget.clone(), target));
                    }
                    restart_quiet |= web.restart_quiet;
                    web.rect
                }
            };
            // GTK wants a measure before every allocation; the zoom path computed no minimums.
            let _ = host.widget.measure(gtk4::Orientation::Horizontal, -1);
            let _ = host.widget.measure(gtk4::Orientation::Vertical, rect.w);
            host.widget
                .size_allocate(&gtk4::Allocation::new(rect.x, rect.y, rect.w, rect.h), -1);
        }
        *imp.clips.borrow_mut() = clips;
        self.place_handles(&arrangement.dividers);
        // Only a new target restarts the quiet period (`WebAllocation::restart_quiet`). The second
        // clause is belt and braces -- the first hold of any target already restarts -- so that a
        // held host is never left without a timer to settle it.
        if restart_quiet || (held && imp.settle.borrow().is_none()) {
            self.settle_after_quiet();
        }
    }

    fn place_handles(&self, dividers: &[Divider]) {
        for (i, handle) in self.imp().handles.borrow().iter().enumerate() {
            let Some(divider) = dividers.get(i) else {
                handle.widget.set_child_visible(false);
                handle.current.borrow_mut().take();
                continue;
            };
            let (orientation, cursor) = match divider.axis {
                Axis::Row => (gtk4::Orientation::Vertical, "col-resize"),
                Axis::Column => (gtk4::Orientation::Horizontal, "row-resize"),
            };
            if handle.widget.orientation() != orientation {
                handle.widget.set_orientation(orientation);
                handle.widget.set_cursor_from_name(Some(cursor));
            }
            handle.widget.set_child_visible(true);
            *handle.current.borrow_mut() = Some(divider.clone());
            let r = divider.rect;
            let _ = handle.widget.measure(gtk4::Orientation::Horizontal, -1);
            let _ = handle.widget.measure(gtk4::Orientation::Vertical, r.w);
            handle
                .widget
                .size_allocate(&gtk4::Allocation::new(r.x, r.y, r.w, r.h), -1);
        }
    }

    /// (Re)starts the quiet timer: every held web host gets its real size [`QUIET`] after the
    /// last pass that held one for a new target.
    fn settle_after_quiet(&self) {
        if let Some(source) = self.imp().settle.borrow_mut().take() {
            source.remove();
        }
        let grid = self.downgrade();
        let source = glib::timeout_add_local_once(QUIET, move || {
            let Some(grid) = grid.upgrade() else { return };
            grid.imp().settle.borrow_mut().take();
            grid.imp().throttle.borrow_mut().settle();
            grid.queue_allocate();
        });
        *self.imp().settle.borrow_mut() = Some(source);
    }

    fn snapshot_children(&self, snapshot: &gtk4::Snapshot) {
        let clips = self.imp().clips.borrow();
        let mut child = self.first_child();
        while let Some(widget) = child {
            match clips.iter().find(|(held, _)| *held == widget) {
                Some((_, r)) => {
                    snapshot.push_clip(&graphene::Rect::new(r.x as f32, r.y as f32, r.w as f32, r.h as f32));
                    self.snapshot_child(&widget, snapshot);
                    snapshot.pop();
                }
                None => self.snapshot_child(&widget, snapshot),
            }
            child = widget.next_sibling();
        }
    }
}

/// [`ModuleGrid::hide_module`]'s order, with its two GTK effects passed in so that a test without a
/// display can see it. `neovibe_core::layout::hide` records the hide and chooses who gets the keys
/// (the most recent module next to `id`, if `id` holds them). Then `focus` gives that module the
/// keys, and only then does `unmap` take `id` off the screen (`ModuleGrid::apply`,
/// `set_child_visible(false)`).
///
/// **Focus first is required, not only tidy** (modules spec §3.2, S3 change 3). Unmapping the widget
/// that holds the keys first lets GTK's own focus chain choose where they go, and in S3 it chose the
/// WebView. Until Task 11's review (finding 1) this order sat inline in `hide_module`, and swapping
/// its two statements left every test green.
///
/// The layout is released before either effect runs, because both re-enter it: a focus change
/// reaches `pane_focus`, which writes the layout's focus, and `apply` reads the layout.
fn hide_then_unmap(
    layout: &RefCell<Layout>,
    id: &ModuleId,
    frame: &Frame,
    focus: impl FnOnce(&ModuleId),
    unmap: impl FnOnce(),
) -> Result<(), LayoutError> {
    leave_then_unmap(
        layout,
        |layout| neovibe_core::layout::hide(layout, id, frame),
        focus,
        unmap,
    )
}

/// [`hide_then_unmap`]'s order for any way of leaving the screen (a hide, a kill): `leave` changes the
/// layout and names who gets the keys, `focus` gives them, `unmap` takes the module off screen.
fn leave_then_unmap(
    layout: &RefCell<Layout>,
    leave: impl FnOnce(&mut Layout) -> Result<Option<ModuleId>, LayoutError>,
    focus: impl FnOnce(&ModuleId),
    unmap: impl FnOnce(),
) -> Result<(), LayoutError> {
    let next = leave(&mut layout.borrow_mut())?;
    if let Some(next) = next {
        focus(&next);
    }
    unmap();
    Ok(())
}

/// Where a new module host goes in the grid's child list: before the first divider handle, so after
/// every host and below every handle. The end of the list if there is no handle yet.
///
/// **Why the order decides who gets a press.** GTK picks a container's children LAST to first
/// (`gtk_widget_do_pick`, gtkwidget.c, GTK 4.22.5), and a widget contains both edges of its border
/// box: `gsk_rounded_rect_locate_point` (gskroundedrect.c) calls a point outside only if it is
/// `> x + width`. So a host whose rectangle ends where a divider begins contains that divider's
/// first pixel column, where a pointer on integer coordinates sits (every synthetic pointer does,
/// and a real one can). Whichever of the two is later in the list takes the press. Appending each
/// host put `lua:main1` after the root divider's handle, and a press on that divider went to the
/// Lua WebView (GUI pass 2026-09-23, item 11). The `side` and `bottom` layouts escaped only because
/// the host added after each handle lies right of or below that divider, and starts one pixel past it.
///
/// The drawing order follows too: handles now paint after every host.
fn host_stack_index<T>(children: &[T], is_handle: impl Fn(&T) -> bool) -> usize {
    children.iter().position(is_handle).unwrap_or(children.len())
}

/// [`ModuleGrid::focus_target`]'s choice, made on the grid's own host records (generic over the
/// widget so it is tested without a display): the focus target recorded for `id`, never its host.
fn focus_target_of<'a, W>(hosts: &'a [Host<W>], id: &ModuleId) -> Option<&'a W> {
    hosts.iter().find(|h| h.id == *id).map(|h| &h.focus)
}

/// Whether `module` may be placed into the tree: only a module this window has a host for. The
/// layout accepts any id `ModuleId::parse` does -- a typo'd `lua:x`, or `canvas` before P3 builds it,
/// through `neovibe.layout.split` -- and a leaf with no host is a hole: half the target's space
/// empty, one divider short, and the keys on a module with no widget (the plan review's finding 2).
/// The grid is the one place that knows which modules exist; P3 adds the canvas's host before it
/// places the canvas.
fn placeable(hosted: &[ModuleId], module: &ModuleId) -> Result<(), LayoutError> {
    if hosted.contains(module) {
        Ok(())
    } else {
        Err(LayoutError::NotInTree(module.clone()))
    }
}

/// Where a divider drag puts the first child: the divider as it is allocated **now**, plus the
/// drag's offset. GTK reports that offset in the handle's own coordinates, and the handle moves
/// under the pointer as the drag re-allocates it, so the offset is how far the pointer is from where
/// the handle now is, not from where it was grabbed. `begin.first_px + offset` -- the grab's divider
/// plus that offset -- lags: at one motion event per frame the divider crawls after the pointer at
/// about half its speed, 200px short of a 400px drag. `GtkPaned` avoids the same trap by gesturing on
/// itself, which never moves. `None` if the handle now draws another split than the grabbed one.
fn dragged_first_px(begin: &Divider, now: &Divider, dx: f64, dy: f64) -> Option<i32> {
    if now.path != begin.path || now.axis != begin.axis {
        return None;
    }
    let offset = match now.axis {
        Axis::Row => dx,
        Axis::Column => dy,
    };
    Some(now.first_px + offset.round() as i32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use neovibe_core::layout::{Arrangement, Node};

    /// A key pressed before the window is laid out (`Ctrl+a h` in the first frame, a shim letter
    /// racing the first allocation) still meets a real geometry, not a 0x0 one in which every
    /// module is an empty rectangle and nothing is anyone's neighbour.
    #[test]
    fn before_the_first_allocation_the_geometry_uses_the_default_windows_content() {
        assert_eq!(frame_size(0, 0), UNALLOCATED);
        assert_eq!(frame_size(1280, 0), UNALLOCATED, "one axis unallocated is unallocated");
        assert_eq!(frame_size(-1, 400), UNALLOCATED);
        assert_eq!(frame_size(900, 500), Size { w: 900, h: 500 });
    }

    /// The divider stays under the pointer for a whole drag. This replays what GTK hands
    /// `drag-update` -- the pointer in the handle's CURRENT coordinates, less where it was grabbed --
    /// one motion event per frame, the grid re-arranging between events as it does. Adding the offset
    /// to the divider the drag began with instead leaves it at 960px: half the drag, 200px behind a
    /// pointer at 1160.
    #[test]
    fn a_dragged_divider_stays_under_the_pointer() {
        let mut layout = Layout::initial(&[]).unwrap();
        let frame = Frame::new(UNALLOCATED, 1);
        let begin = arrange(&layout, &frame).dividers[0].clone();
        let grabbed_at = 0;
        let pointer_start = begin.rect.x + grabbed_at;
        for step in 1..=20 {
            let pointer = pointer_start + 20 * step;
            let now = arrange(&layout, &frame).dividers[0].clone();
            let dx = f64::from(pointer - now.rect.x - grabbed_at);
            let first_px = dragged_first_px(&begin, &now, dx, 0.0).unwrap();
            layout.set_ratio(&now.path, now.ratio_for(first_px)).unwrap();
        }
        assert_eq!(arrange(&layout, &frame).dividers[0].rect.x, pointer_start + 400);

        let elsewhere = Divider {
            path: vec![neovibe_core::layout::Branch::First],
            ..begin.clone()
        };
        assert_eq!(dragged_first_px(&begin, &elsewhere, 5.0, 0.0), None, "another split");
    }

    /// One child of the grid, in the model below.
    #[derive(Debug, Clone, PartialEq)]
    enum Child {
        Host(ModuleId),
        /// The n-th handle the grid made: it draws `arrangement.dividers[n]` (`place_handles`).
        Handle(usize),
    }

    /// The child list `ModuleGrid::add` builds for hosts added in this order: each host where
    /// [`host_stack_index`] says, then (from the second host on) one handle appended, as
    /// `new_handle`'s `set_parent` does.
    fn stack_as_add_builds(ids: &[ModuleId]) -> Vec<Child> {
        let mut children = Vec::new();
        for (i, id) in ids.iter().enumerate() {
            let at = host_stack_index(&children, |c| matches!(c, Child::Handle(_)));
            children.insert(at, Child::Host(id.clone()));
            if i > 0 {
                children.push(Child::Handle(i - 1));
            }
        }
        children
    }

    /// The same list as the grid built it before the fix: every child appended in the order added.
    fn stack_appended(ids: &[ModuleId]) -> Vec<Child> {
        let mut children = Vec::new();
        for (i, id) in ids.iter().enumerate() {
            children.push(Child::Host(id.clone()));
            if i > 0 {
                children.push(Child::Handle(i - 1));
            }
        }
        children
    }

    /// GTK's pick over the grid, as `gtk_widget_do_pick` (gtkwidget.c, GTK 4.22.5) does it: the
    /// last child first, skipping one that is not on screen, and a child contains a point on either
    /// edge of its rectangle (`gsk_rounded_rect_locate_point`, gskroundedrect.c). A model of GTK
    /// read from its source, not GTK itself -- the one thing this test cannot run without a display.
    fn pick(children: &[Child], arrangement: &Arrangement, x: i32, y: i32) -> Option<Child> {
        let contains = |r: Rect| x >= r.x && x <= r.x + r.w && y >= r.y && y <= r.y + r.h;
        children
            .iter()
            .rev()
            .find(|child| match child {
                Child::Host(id) => arrangement.rect_of(id).is_some_and(contains),
                Child::Handle(n) => arrangement.dividers.get(*n).is_some_and(|d| contains(d.rect)),
            })
            .cloned()
    }

    /// The modules `main.rs` adds, in its order: the editor, the agent, the bottom terminal, then
    /// each Lua panel in the order it was registered -- over the layout `main.rs` builds, with the
    /// terminal hidden, or shown as the first `Ctrl+a t` leaves it.
    fn layout_and_add_order(
        panels: &[(&str, neovibe_core::layout::Placement)],
        terminal_shown: bool,
    ) -> (Layout, Vec<ModuleId>) {
        let decls: Vec<neovibe_core::layout::ModuleDecl> = panels
            .iter()
            .map(|(id, placement)| neovibe_core::layout::ModuleDecl {
                id: ModuleId::lua(id),
                placement: *placement,
            })
            .collect();
        let mut ids = vec![ModuleId::editor(), ModuleId::agent(), ModuleId::terminal()];
        ids.extend(decls.iter().map(|d| d.id.clone()));
        let mut layout = crate::terminal::initial_layout(&decls).unwrap();
        if terminal_shown {
            layout.show(&ModuleId::terminal()).unwrap();
        }
        (layout, ids)
    }

    /// A press on a divider reaches its handle, in every layout P1 can open, at both of the pixel
    /// columns (rows) a pointer on integer coordinates can sit on: the divider's own, and the one
    /// its right (bottom) edge touches -- which is what the GUI pass measured on `main` and on P1's
    /// default window ("a press at x and at x+1 grabs, x-1 and x+2 do not").
    ///
    /// Before the fix the Lua `main` layout failed this: the Lua host was added after the root
    /// divider's handle, ends where the divider begins, and so took the press at the divider's own
    /// column -- the item 11 regression. The negative control below reproduces it on the model.
    #[test]
    fn a_press_on_any_divider_reaches_its_handle_in_every_p1_layout() {
        use neovibe_core::layout::Placement::{BelowRoot, InPlaceOfEditor, RightOfRoot};
        let layouts: &[&[(&str, neovibe_core::layout::Placement)]] = &[
            &[],
            &[("main1", InPlaceOfEditor)],
            &[("side1", RightOfRoot)],
            &[("bottom1", BelowRoot)],
            &[
                ("main1", InPlaceOfEditor),
                ("side1", RightOfRoot),
                ("bottom1", BelowRoot),
            ],
            &[
                ("side1", RightOfRoot),
                ("bottom1", BelowRoot),
                ("main1", InPlaceOfEditor),
            ],
            &[("main1", InPlaceOfEditor), ("main2", InPlaceOfEditor)],
        ];
        let frame = Frame::new(UNALLOCATED, 1);
        for (panels, terminal_shown) in layouts.iter().flat_map(|p| [(p, false), (p, true)]) {
            let (layout, ids) = layout_and_add_order(panels, terminal_shown);
            let arrangement = arrange(&layout, &frame);
            assert!(!arrangement.dividers.is_empty(), "{panels:?}: nothing to press");
            let children = stack_as_add_builds(&ids);
            for (n, divider) in arrangement.dividers.iter().enumerate() {
                let r = divider.rect;
                let presses = match divider.axis {
                    Axis::Row => [(r.x, r.y + r.h / 2), (r.x + r.w, r.y + r.h / 2)],
                    Axis::Column => [(r.x + r.w / 2, r.y), (r.x + r.w / 2, r.y + r.h)],
                };
                for (x, y) in presses {
                    let got = pick(&children, &arrangement, x, y);
                    // v1 trial item 6 (2026-09-28): the terminal's own pinned split now nests
                    // around the editor's leaf, a subtree that can itself hold another split --
                    // two Lua `main` panels, both shown, is the one case this matrix reaches.
                    // Newly reachable: the single corner pixel where the terminal's own divider
                    // begins is also the bottom edge of the nested main1/main2 divider, and the
                    // later-added (so higher, by the very same "last in the child list wins"
                    // rule host_stack_index relies on) handle wins there. Both targets are real,
                    // adjacent dividers -- never a host -- so this is not the item 11 failure
                    // mode this test exists to catch; it is named here rather than silently
                    // accepted by loosening the assertion below.
                    if panels.len() == 2
                        && panels[0].0 == "main1"
                        && panels[1].0 == "main2"
                        && terminal_shown
                        && n == 1
                        && (x, y) == (r.x + r.w / 2, r.y)
                    {
                        assert_eq!(got, Some(Child::Handle(2)), "the known corner exception moved");
                        continue;
                    }
                    assert_eq!(
                        got,
                        Some(Child::Handle(n)),
                        "{panels:?} (terminal shown: {terminal_shown}): a press at ({x}, {y}) on divider {n} {:?}",
                        divider.path
                    );
                }
            }
        }

        // The model against what the GUI pass measured on the default window: one column either
        // side of the two that grab belongs to the editor and the agent.
        let (layout, ids) = layout_and_add_order(&[], false);
        let arrangement = arrange(&layout, &frame);
        let r = arrangement.dividers[0].rect;
        assert_eq!(r.x, 760, "the default divider, as measured");
        let children = stack_as_add_builds(&ids);
        assert_eq!(
            pick(&children, &arrangement, r.x - 1, 300),
            Some(Child::Host(ModuleId::editor()))
        );
        assert_eq!(
            pick(&children, &arrangement, r.x + 2, 300),
            Some(Child::Host(ModuleId::agent()))
        );

        // Negative control: the child list as the grid appended it before the fix loses the Lua
        // `main` layout's root divider to the Lua host, as the GUI pass saw on screen.
        let (layout, ids) = layout_and_add_order(&[("main1", InPlaceOfEditor)], false);
        let arrangement = arrange(&layout, &frame);
        let r = arrangement.dividers[0].rect;
        assert_eq!(
            pick(&stack_appended(&ids), &arrangement, r.x, r.y + r.h / 2),
            Some(Child::Host(ModuleId::lua("main1")))
        );
    }

    /// sw-layout-2 (2026-09-27 Codex sweep): a web host zoomed to the whole window, then unzoomed,
    /// used to keep the real GTK allocation `size_allocate` gave it while zoomed -- the whole
    /// window -- at its new, correct position, so it stuck out past its own target and covered the
    /// sibling that has since settled into the space the zoom vacated. `pick()` above models GTK's
    /// real algorithm, which resolves a click by each child's own allocation
    /// (`gtk_widget_do_pick`/`gtk_widget_contains`), never `snapshot_children`'s paint clip -- so a
    /// click that visibly lands on the sibling used to resolve to the stale host instead.
    /// `main.rs` adds the editor before the agent, so the agent -- later in the child list -- is
    /// picked first and this is reachable with either as the web host.
    #[test]
    fn a_zoomed_then_unzoomed_web_host_never_steals_a_click_from_its_settled_neighbour() {
        let (agent, editor) = (ModuleId::agent(), ModuleId::editor());
        let root = Node::split(Axis::Row, 0.5, Node::Leaf(agent.clone()), Node::Leaf(editor.clone()));
        let mut layout = Layout::new(root, agent.clone()).unwrap();
        let frame = Frame::new(Size { w: 1280, h: 721 }, 1);
        let mut throttle = WebThrottle::default();

        // The agent is zoomed to the whole window, and settles there once quiet.
        layout.toggle_zoom(&agent);
        let zoomed_target = arrange(&layout, &frame).rect_of(&agent).unwrap();
        let window = Size { w: 1280, h: 721 };
        throttle.allocate(&agent, zoomed_target, false, window);
        throttle.settle();
        let real_alloc_while_zoomed = throttle.allocate(&agent, zoomed_target, true, window).rect;
        assert_eq!(real_alloc_while_zoomed, zoomed_target);

        // Unzoom: the agent goes back to its left column, the editor takes the right.
        layout.unzoom();
        let arrangement = arrange(&layout, &frame);
        let (agent_target, editor_target) = (
            arrangement.rect_of(&agent).unwrap(),
            arrangement.rect_of(&editor).unwrap(),
        );
        assert!(editor_target.w > 0, "editor is shown again, unzoomed");

        let held = throttle.allocate(&agent, agent_target, true, window);
        assert!(held.held, "the target changed: the throttle holds the agent's old size");
        assert!(
            held.rect.x + held.rect.w <= agent_target.x + agent_target.w,
            "the held allocation {:?} reaches past the agent's target {agent_target:?} toward the editor",
            held.rect
        );

        // What `main.rs` really allocates: the editor gets its own (unthrottled) target, the agent
        // whatever the throttle just returned.
        let real_arrangement = Arrangement {
            modules: vec![(editor.clone(), editor_target), (agent.clone(), held.rect)],
            dividers: Vec::new(),
        };
        let children = stack_as_add_builds(&[editor.clone(), agent.clone()]);
        let inside_editor = (
            editor_target.x + editor_target.w / 2,
            editor_target.y + editor_target.h / 2,
        );
        assert_eq!(
            pick(&children, &real_arrangement, inside_editor.0, inside_editor.1),
            Some(Child::Host(editor)),
            "a click inside the editor's own drawn area must not resolve to the agent's stale \
             allocation {:?} (editor's real area: {editor_target:?})",
            held.rect,
        );
    }

    /// What [`hide_then_unmap`] did, in order, as its two effects saw the layout when each ran.
    #[derive(Debug, PartialEq)]
    enum Effect {
        Focus(ModuleId),
        Unmap,
    }

    /// Hides `id` in `layout` through [`hide_then_unmap`] and records each effect as it runs. Each
    /// effect checks what the real one relies on: the layout is not borrowed (a focus change reaches
    /// `pane_focus`, which writes it; `apply` reads it), and `id` is already hidden in it (so
    /// `apply` unmaps it). `focus` also checks that its target is shown, because `main.rs`'s
    /// `focus_module` refuses a hidden module and the keys would then stay on the module being
    /// hidden.
    fn hide_and_record(layout: &RefCell<Layout>, id: &ModuleId) -> (Result<(), LayoutError>, Vec<Effect>) {
        let effects = RefCell::new(Vec::new());
        let result = hide_then_unmap(
            layout,
            id,
            &Frame::new(UNALLOCATED, 1),
            |next| {
                let l = layout
                    .try_borrow_mut()
                    .expect("the layout is released before the keys move");
                assert!(!l.is_shown(id), "{id} is hidden in the layout before anything moves");
                assert!(l.is_shown(next), "the keys go to a shown module, {next}");
                effects.borrow_mut().push(Effect::Focus(next.clone()));
            },
            || {
                let l = layout
                    .try_borrow_mut()
                    .expect("the layout is released before the unmap");
                assert!(!l.is_shown(id), "apply unmaps what the layout hides");
                effects.borrow_mut().push(Effect::Unmap);
            },
        );
        (result, effects.into_inner())
    }

    /// Task 11's review, finding 1: `Ctrl+a t` with the keys in the terminal gives them to the
    /// module above it, and only then unmaps it (S3 change 3). With the two effects swapped in
    /// `hide_then_unmap` this fails; before the seam, the same swap inline in `hide_module` left
    /// all 190 `shell` tests green. Both upper modules are tried as the one that had the keys last,
    /// so a fixed choice cannot pass either.
    #[test]
    fn hiding_the_module_with_the_keys_gives_them_away_before_it_unmaps() {
        for last_above in [ModuleId::agent(), ModuleId::editor()] {
            let mut layout = crate::terminal::initial_layout(&[]).unwrap();
            layout.show(&ModuleId::terminal()).unwrap();
            layout.set_focus(&last_above).unwrap();
            layout.set_focus(&ModuleId::terminal()).unwrap();
            let layout = RefCell::new(layout);

            let (result, effects) = hide_and_record(&layout, &ModuleId::terminal());
            assert_eq!(result, Ok(()));
            assert_eq!(effects, [Effect::Focus(last_above.clone()), Effect::Unmap]);
            assert_eq!(layout.borrow().focus(), &last_above);
        }
    }

    /// `prefix x` takes the same door (2026-09-26): the keys leave the killed terminal before it
    /// unmaps, and it is left hidden where a first launch puts it.
    #[test]
    fn killing_the_module_with_the_keys_gives_them_away_before_it_unmaps() {
        let mut layout = crate::terminal::initial_layout(&[]).unwrap();
        layout.show(&ModuleId::terminal()).unwrap();
        layout.set_focus(&ModuleId::agent()).unwrap();
        layout.set_focus(&ModuleId::terminal()).unwrap();
        let layout = RefCell::new(layout);
        let id = ModuleId::terminal();
        let effects = RefCell::new(Vec::new());
        let result = leave_then_unmap(
            &layout,
            |l| {
                neovibe_core::layout::kill(
                    l,
                    &id,
                    Reopen::At(neovibe_core::layout::Placement::BelowEditor),
                    &Frame::new(UNALLOCATED, 1),
                )
            },
            |next| {
                let l = layout.try_borrow_mut().expect("released before the keys move");
                assert!(!l.is_shown(&id));
                effects.borrow_mut().push(Effect::Focus(next.clone()));
            },
            || effects.borrow_mut().push(Effect::Unmap),
        );
        assert_eq!(result, Ok(()));
        assert_eq!(effects.into_inner(), [Effect::Focus(ModuleId::agent()), Effect::Unmap]);
        assert_eq!(
            layout.borrow().root(),
            crate::terminal::initial_layout(&[]).unwrap().root(),
            "back where a first launch puts it"
        );
    }

    /// Hiding a module that does not hold the keys moves no keys, and still unmaps it. A hide the
    /// layout refuses does neither: the last shown module (invariant 3), or one not in the tree.
    #[test]
    fn a_hide_without_the_keys_only_unmaps_and_a_refused_hide_does_nothing() {
        let mut layout = crate::terminal::initial_layout(&[]).unwrap();
        layout.show(&ModuleId::terminal()).unwrap();
        let layout = RefCell::new(layout);
        assert_eq!(layout.borrow().focus(), &ModuleId::editor(), "the premise");
        let (result, effects) = hide_and_record(&layout, &ModuleId::terminal());
        assert_eq!(result, Ok(()));
        assert_eq!(effects, [Effect::Unmap]);
        assert_eq!(layout.borrow().focus(), &ModuleId::editor());

        let (result, effects) = hide_and_record(&layout, &ModuleId::lua("nowhere"));
        assert_eq!(result, Err(LayoutError::NotInTree(ModuleId::lua("nowhere"))));
        assert!(effects.is_empty(), "{effects:?}");

        let mut alone = Layout::initial(&[]).unwrap();
        neovibe_core::layout::hide(&mut alone, &ModuleId::agent(), &Frame::new(UNALLOCATED, 1)).unwrap();
        let alone = RefCell::new(alone);
        let (result, effects) = hide_and_record(&alone, &ModuleId::editor());
        assert_eq!(result, Err(LayoutError::LastVisible(ModuleId::editor())));
        assert!(effects.is_empty(), "{effects:?}");
    }

    /// Every host sits below every handle however many modules there are, and the hosts keep the
    /// order they were added in (focus and HINT never read the child list, but nothing should
    /// reorder the modules as a side effect).
    #[test]
    fn hosts_stack_below_every_handle_in_the_order_they_were_added() {
        let ids: Vec<ModuleId> = [ModuleId::editor(), ModuleId::agent()]
            .into_iter()
            .chain(["a", "b", "c"].map(ModuleId::lua))
            .collect();
        let children = stack_as_add_builds(&ids);
        let hosts: Vec<Child> = ids.iter().cloned().map(Child::Host).collect();
        let handles: Vec<Child> = (0..ids.len() - 1).map(Child::Handle).collect();
        assert_eq!(children, [hosts, handles].concat());
        assert_eq!(
            host_stack_index::<Child>(&[], |_| true),
            0,
            "the first host, into an empty grid"
        );
    }

    /// The same drag on a divider inside a split that does not start at the grid's origin, where
    /// the handle's grid x (`rect.x`) and the split's own `first_px` differ by everything left of
    /// the split. The root-split replay above cannot tell them apart -- every split P1 builds
    /// starts at 0 -- so an implementation adding the offset to `rect.x` passed it.
    #[test]
    fn a_dragged_divider_inside_a_nested_split_stays_under_the_pointer() {
        use neovibe_core::layout::{Branch, Node};
        let l = ModuleId::lua;
        let root = Node::split(
            Axis::Row,
            0.4,
            Node::Leaf(l("a")),
            Node::split(Axis::Row, 0.5, Node::Leaf(l("b")), Node::Leaf(l("c"))),
        );
        let mut layout = Layout::new(root, l("b")).unwrap();
        let frame = Frame::new(UNALLOCATED, 1);
        let inner = |layout: &Layout| {
            arrange(layout, &frame)
                .dividers
                .into_iter()
                .find(|d| d.path == [Branch::Second])
                .unwrap()
        };
        let begin = inner(&layout);
        assert_ne!(begin.rect.x, begin.first_px, "the premise: the split starts right of 0");
        let pointer_start = begin.rect.x;
        for step in 1..=10 {
            let pointer = pointer_start + 15 * step;
            let now = inner(&layout);
            let dx = f64::from(pointer - now.rect.x);
            let first_px = dragged_first_px(&begin, &now, dx, 0.0).unwrap();
            layout.set_ratio(&now.path, now.ratio_for(first_px)).unwrap();
        }
        assert_eq!(inner(&layout).rect.x, pointer_start + 150);
    }

    /// The drag on the bottom terminal's divider, which is pinned (modules P2): the divider stays
    /// under the pointer, and what the drag leaves is a height in pixels that a taller window keeps.
    /// Since v1 trial item 6 (2026-09-28) that divider is nested one level in (below the editor's
    /// own leaf, not the root split), so it is found by its `Column` axis rather than an empty path.
    #[test]
    fn a_dragged_pinned_divider_stays_under_the_pointer_and_keeps_its_height() {
        let mut layout = crate::terminal::initial_layout(&[]).unwrap();
        layout.show(&ModuleId::terminal()).unwrap();
        let frame = Frame::new(UNALLOCATED, 1);
        assert!(neovibe_core::layout::settle_pins(&mut layout, &frame));
        let terminal_divider = |layout: &Layout| {
            arrange(layout, &frame)
                .dividers
                .into_iter()
                .find(|d| d.axis == Axis::Column)
                .unwrap()
        };
        let begin = terminal_divider(&layout);
        assert_eq!(begin.rect.y, 480, "the terminal's first show, a third of 721");
        for step in 1..=10 {
            let pointer = begin.rect.y - 10 * step;
            let now = terminal_divider(&layout);
            let dy = f64::from(pointer - now.rect.y);
            let first_px = dragged_first_px(&begin, &now, 0.0, dy).unwrap();
            neovibe_core::layout::move_divider(&mut layout, &now, first_px).unwrap();
        }
        assert_eq!(terminal_divider(&layout).rect.y, 380);
        let taller = Frame::new(neovibe_core::layout::Size { w: 1280, h: 1041 }, 1);
        assert_eq!(
            arrange(&layout, &taller).rect_of(&ModuleId::terminal()).map(|r| r.h),
            Some(340),
            "dragged to 340px, and kept at 340 when the window grows"
        );
    }

    /// `neovibe.layout.split("canvas", ..)` before P3, or a typo'd `lua:` id: refused, as a module
    /// that is not in the layout is refused everywhere else, rather than placed as a leaf with no host.
    #[test]
    fn only_a_module_with_a_host_can_be_placed() {
        let hosted = [
            ModuleId::editor(),
            ModuleId::agent(),
            ModuleId::terminal(),
            ModuleId::lua("notes"),
        ];
        for id in &hosted {
            assert_eq!(placeable(&hosted, id), Ok(()));
        }
        for id in [ModuleId::parse("canvas").unwrap(), ModuleId::lua("nots")] {
            assert_eq!(placeable(&hosted, &id), Err(LayoutError::NotInTree(id.clone())));
        }
    }

    /// Module focus grabs what `add` was given as the focus target, never the host (2026-09-29): a
    /// web module's host is a `WebHost`, which is not focusable itself, and grabbing a `GtkBox` in its
    /// place left the keys where they were in the sandbox's first wrapped run. These are the grid's
    /// own records, with each module's host and focus target told apart, so an answer taken from
    /// the host fails here (review round 1: the first version of this test never saw a host at all).
    #[test]
    fn a_module_is_focused_through_its_focus_target_not_its_host() {
        let host = |id: ModuleId, widget: &'static str, focus: &'static str, kind| Host {
            id,
            widget,
            focus,
            kind,
        };
        let hosts = [
            host(ModuleId::editor(), "editor-overlay", "editor-glarea", HostKind::Direct),
            host(ModuleId::agent(), "agent-webhost", "agent-webview", HostKind::Web),
            host(
                ModuleId::terminal(),
                "terminal-glarea",
                "terminal-glarea",
                HostKind::Direct,
            ),
            host(ModuleId::lua("notes"), "notes-webhost", "notes-webview", HostKind::Web),
        ];
        assert_eq!(focus_target_of(&hosts, &ModuleId::agent()), Some(&"agent-webview"));
        assert_eq!(focus_target_of(&hosts, &ModuleId::lua("notes")), Some(&"notes-webview"));
        assert_eq!(focus_target_of(&hosts, &ModuleId::editor()), Some(&"editor-glarea"));
        assert_eq!(
            focus_target_of(&hosts, &ModuleId::terminal()),
            Some(&"terminal-glarea"),
            "a module that is its own focus target"
        );
        assert_eq!(
            focus_target_of(&hosts, &ModuleId::parse("canvas").unwrap()),
            None,
            "a module this grid has no host for"
        );
    }
}
