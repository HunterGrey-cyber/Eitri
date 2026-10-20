//! `ModuleGrid` contains every module. Each host gets its parent once in [`ModuleGrid::add`]
//! and loses it only in `dispose`, when the window goes away. A web module's `WebView` is
//! parented to its [`WebHost`] before the host is added to the grid.
//! Hiding uses `set_child_visible(false)`. Zoom, resize, split, swap, move and even operations
//! allocate the same children again. Unparenting unrealizes the editor, whose Skia
//! `DirectContext` is created once on the `GLArea`'s GL context and cannot survive unrealization.
//! Each host logs unrealize in [`ModuleGrid::add`] so unexpected unrealization can be detected.
//!
//! Rectangles come from the layout's `arrange` function. The grid measures children's minimum
//! sizes, allocates the resulting geometry, holds web hosts at their settled size during
//! resizing ([`throttle`]), and converts divider drags into ratios.

pub(crate) mod strips;
mod throttle;

use std::cell::{Cell, RefCell};
use std::collections::BTreeSet;
use std::rc::Rc;

use gtk4::glib;
use gtk4::graphene;
use gtk4::prelude::*;
use gtk4::subclass::prelude::*;

use eitri_core::layout::{
    arrange, Axis, ConLayout, Direction, Divider, Frame, Layout, LayoutError, ModuleId, Nav, Preset, Rect, Reopen,
    Size, Strip, ZoomChange, DEFAULT_STRIP_PX,
};

use strips::{strip_assignments, ConStrip};
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

/// What runs when a strip entry is clicked, with the entry's module.
type StripHook = Rc<dyn Fn(&ModuleId)>;

/// A strip entry's text for its module.
type StripLabel = Rc<dyn Fn(&ModuleId) -> String>;

/// The size the geometry is computed against: the grid's allocation, or [`UNALLOCATED`] while GTK
/// still reports none (0x0 until the first allocation).
fn frame_size(width: i32, height: i32) -> Size {
    if width > 0 && height > 0 {
        Size { w: width, h: height }
    } else {
        UNALLOCATED
    }
}

/// Where the probe puts a host, whatever the layout says: a rectangle in the grid's coordinates (it may
/// overlap another host or reach past the grid's edge), or off the screen.
#[cfg(feature = "probe")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProbePlace {
    Hidden,
    At(Rect),
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
        /// The strip pool: every tabbed or stacked container on screen draws one, in arrangement order.
        pub(super) strips: RefCell<Vec<ConStrip>>,
        /// Called with a strip entry's module when it is clicked ([`ModuleGrid::connect_strip_activate`]).
        pub(super) strip_activate: RefCell<Vec<StripHook>>,
        /// A strip entry's text for its module ([`ModuleGrid::set_strip_label`]); the id until set.
        pub(super) strip_label: RefCell<Option<StripLabel>>,
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
        /// Hosts the probe places by hand instead of by the layout ([`ModuleGrid::probe_place`]).
        #[cfg(feature = "probe")]
        pub(super) probe_places: RefCell<std::collections::BTreeMap<ModuleId, ProbePlace>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for ModuleGrid {
        const NAME: &'static str = "EitriModuleGrid";
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
            let strips = std::mem::take(&mut *self.strips.borrow_mut());
            for strip in strips {
                strip.widget.unparent();
            }
        }
    }

    impl WidgetImpl for ModuleGrid {
        fn measure(&self, orientation: gtk4::Orientation, _for_size: i32) -> (i32, i32, i32, i32) {
            // The window decides the size and the tree divides it; the grid asks only for what is on
            // screen needs at the least (`eitri_core::layout::min_size`), so the window cannot be
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

    /// The only place a module host gets a parent. Each host after the first adds a divider to
    /// the pool. A tree over n leaves has at most n - 1 splits, so all dividers exist before
    /// allocation and none needs to be created during it.
    ///
    /// A host may be added after `present()`. Focus tracking, `focus_module` and the prefix read
    /// the grid live through [`ModuleGrid::live_hosts`] and [`ModuleGrid::hosts`]. HINT uses
    /// [`ModuleGrid::hosts_in_tree_order`]. Each new web host needs its own `Ctrl+h/j/k/l`
    /// controller, installed by `main.rs`'s `install_module_nav` when the host is added.
    ///
    /// Hosts are stacked below every strip and divider handle ([`band_insert_index`]). GTK gives
    /// a press to the last child containing it, and a host contains the first pixel column of the
    /// divider on its right. Appending a host after a handle lets it take that handle's presses.
    /// [`Self::insert_in_band`] keeps strips and handles in their bands regardless of creation order.
    ///
    /// `focus` is the widget that receives keys ([`ModuleGrid::focus_target`]). It must be the host
    /// or a widget inside it. The editor's host is the overlay carrying `editor_start_failure`.
    /// A web host must be a [`WebHost`] around its `WebView`, and its focus target must be that child.
    /// `WebHost` is not focusable itself; module focus uses the child directly. Putting a `WebView`
    /// directly in the grid offsets its input method's candidate window by the module's position.
    /// These relationships are checked once when the module is added.
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
        self.insert_in_band(&widget, Band::Host);
        let unrealized = Rc::new(Cell::new(0u32));
        {
            let id = id.clone();
            widget.connect_unrealize(move |_| {
                unrealized.set(unrealized.get() + 1);
                // Expected exactly once per host, when the window closes. Any earlier line is the
                // bug this container exists to rule out.
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
        self.insert_in_band(widget.upcast_ref(), Band::Handle);
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
                let moved = eitri_core::layout::move_divider(&mut grid.layout().borrow_mut(), &now, first_px);
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

    /// Gives `widget` its parent, the grid, at `band`'s place in the child list ([`band_insert_index`]).
    /// Parenting at a position, not reparenting: this is the widget's first and only parent.
    fn insert_in_band(&self, widget: &gtk4::Widget, band: Band) {
        let (children, bands) = self.child_bands();
        match children.get(band_insert_index(&bands, band)) {
            Some(above) => widget.insert_before(self, Some(above)),
            None => widget.set_parent(self),
        }
    }

    /// The grid's children in order, and the band each belongs to. Anything that is neither a handle
    /// nor a strip is a module host: the grid parents nothing else.
    fn child_bands(&self) -> (Vec<gtk4::Widget>, Vec<Band>) {
        let imp = self.imp();
        let handles: Vec<gtk4::Widget> = imp.handles.borrow().iter().map(|h| h.widget.clone().upcast()).collect();
        let strips: Vec<gtk4::Widget> = imp.strips.borrow().iter().map(|s| s.widget.clone().upcast()).collect();
        let mut children = Vec::new();
        let mut bands = Vec::new();
        let mut child = self.first_child();
        while let Some(c) = child {
            child = c.next_sibling();
            bands.push(if handles.contains(&c) {
                Band::Handle
            } else if strips.contains(&c) {
                Band::Strip
            } else {
                Band::Host
            });
            children.push(c);
        }
        (children, bands)
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

    /// Whether `id` has a host in the grid. A host is never removed, so a terminal instance that was
    /// killed keeps its host, child-invisible, and a new instance of the same number reuses it.
    pub(crate) fn has_host(&self, id: &ModuleId) -> bool {
        self.imp().hosts.borrow().iter().any(|h| h.id == *id)
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
                .filter(|h| !self.probe_places_host(&h.id))
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

    /// `f` runs with a strip entry's module when a primary click lands on that entry. What it does --
    /// switching to the tab, through [`ModuleGrid::switch_tab`] -- is the caller's, which knows how the
    /// window gives a module the keys. Runs with the layout released.
    pub(crate) fn connect_strip_activate(&self, f: impl Fn(&ModuleId) + 'static) {
        self.imp().strip_activate.borrow_mut().push(Rc::new(f));
    }

    /// Every strip entry naming a module now, with its label widget, strips in pool order: HINT's
    /// strip targets. An entry of a strip off screen is not mapped, which HINT reads as not visible.
    pub(crate) fn strip_entries(&self) -> Vec<(ModuleId, gtk4::Widget)> {
        self.imp()
            .strips
            .borrow()
            .iter()
            .flat_map(|strip| strip.entries_in_use())
            .collect()
    }

    /// What a primary click on `id`'s strip entry does, for a landing that is not a click (HINT).
    pub(crate) fn activate_strip_entry(&self, id: &ModuleId) {
        self.strip_activated(id);
    }

    fn strip_activated(&self, id: &ModuleId) {
        // Cloned out: a hook switches the tab, which re-allocates the strips.
        let hooks: Vec<StripHook> = self.imp().strip_activate.borrow().clone();
        for hook in hooks {
            hook(id);
        }
    }

    /// The text of a strip entry for its module, applied to every strip on screen now and used for
    /// every one drawn later. Until it is set an entry shows the module's id.
    pub(crate) fn set_strip_label(&self, f: impl Fn(&ModuleId) -> String + 'static) {
        *self.imp().strip_label.borrow_mut() = Some(Rc::new(f));
        self.refresh_strips();
    }

    /// Names every strip entry on screen again with the label function, for what it reads that the
    /// layout does not (a title, a chat's attention). The entries and their places stay as the last
    /// allocation left them: nothing is measured or allocated.
    pub(crate) fn refresh_strips(&self) {
        let label = self.strip_label();
        for strip in self.imp().strips.borrow().iter() {
            strip.relabel(&*label);
        }
    }

    fn strip_label(&self) -> StripLabel {
        self.imp()
            .strip_label
            .borrow()
            .clone()
            .unwrap_or_else(|| Rc::new(|id: &ModuleId| id.to_string()))
    }

    /// `Ctrl+a \ <key>` / `Ctrl+a " <key>`: `module` into a new split after `target` along `axis`,
    /// moved there if it is elsewhere, shown if hidden (`eitri_core::layout::place`). The layout
    /// gives `module` the keys; the caller gives it GTK focus.
    pub(crate) fn place_module(&self, module: &ModuleId, target: &ModuleId, axis: Axis) -> Result<(), LayoutError> {
        let hosted: Vec<ModuleId> = self.hosts().into_iter().map(|(id, _)| id).collect();
        placeable(&hosted, module)?;
        eitri_core::layout::place(&mut self.layout().borrow_mut(), module, target, axis)?;
        self.apply();
        Ok(())
    }

    /// `Ctrl+a H/J/K/L` (`eitri_core::layout::swap`). The neighbour, if they swapped.
    pub(crate) fn swap_modules(&self, focused: &ModuleId, direction: Direction) -> Option<ModuleId> {
        let size = self.size();
        let swapped = {
            let layout = self.layout();
            let mut layout = layout.borrow_mut();
            self.with_frame(size, |frame| {
                eitri_core::layout::swap(&mut layout, focused, direction, frame)
            })
        };
        if swapped.is_some() {
            self.apply();
        }
        swapped
    }

    /// `swap.prev`/`swap.next` (`eitri_core::layout::swap_adjacent`).
    pub(crate) fn swap_adjacent(&self, focused: &ModuleId, forward: bool) -> Option<ModuleId> {
        let swapped = {
            let layout = self.layout();
            let mut layout = layout.borrow_mut();
            eitri_core::layout::swap_adjacent(&mut layout, focused, forward)
        };
        if swapped.is_some() {
            self.apply();
        }
        swapped
    }

    /// `eitri_core::layout::even` on its own: an even split that is not a preset, so `layout.next`
    /// does not count it. The keys' `even-*` go through [`ModuleGrid::preset`] instead, which does.
    #[allow(dead_code)]
    pub(crate) fn even_modules(&self, axis: Axis) {
        eitri_core::layout::even(&mut self.layout().borrow_mut(), axis);
        self.apply();
    }

    // The verbs below share one shape: the layout is borrowed only while core changes it, and
    // released before `apply`, whose hooks read it; `apply` runs only when something changed, so a
    // refused or empty verb neither moves a widget nor counts as a change for the state file.

    /// `move.far-*` (`eitri_core::layout::move_far`): `Ok(false)` when `target` already spans that edge.
    pub(crate) fn move_far(&self, target: &ModuleId, dir: Direction) -> Result<bool, LayoutError> {
        let size = self.size();
        let moved = {
            let layout = self.layout();
            let mut layout = layout.borrow_mut();
            self.with_frame(size, |frame| {
                eitri_core::layout::move_far(&mut layout, target, dir, frame)
            })
        }?;
        if moved {
            self.apply();
        }
        Ok(moved)
    }

    /// `layout.toggle-split` (`eitri_core::layout::toggle_split`).
    pub(crate) fn toggle_split(&self, target: &ModuleId) -> bool {
        let changed = eitri_core::layout::toggle_split(&mut self.layout().borrow_mut(), target);
        if changed {
            self.apply();
        }
        changed
    }

    /// `layout.rotate-*` (`eitri_core::layout::rotate`).
    pub(crate) fn rotate(&self, down: bool) -> bool {
        let changed = eitri_core::layout::rotate(&mut self.layout().borrow_mut(), down);
        if changed {
            self.apply();
        }
        changed
    }

    /// `layout.flip-*` (`eitri_core::layout::flip`).
    pub(crate) fn flip(&self, axis: Axis) -> bool {
        let changed = eitri_core::layout::flip(&mut self.layout().borrow_mut(), axis);
        if changed {
            self.apply();
        }
        changed
    }

    /// The five tmux presets (`eitri_core::layout::preset`), remembered for `layout.next`.
    pub(crate) fn preset(&self, preset: Preset) -> Result<(), LayoutError> {
        eitri_core::layout::preset(&mut self.layout().borrow_mut(), preset)?;
        self.apply();
        Ok(())
    }

    /// `layout.next`/`layout.previous` (`eitri_core::layout::cycle_preset`): the preset applied.
    pub(crate) fn cycle_preset(&self, forward: bool) -> Result<Preset, LayoutError> {
        let preset = eitri_core::layout::cycle_preset(&mut self.layout().borrow_mut(), forward)?;
        self.apply();
        Ok(preset)
    }

    /// `layout.spread` (`eitri_core::layout::spread`).
    pub(crate) fn spread(&self, target: &ModuleId) -> bool {
        let changed = eitri_core::layout::spread(&mut self.layout().borrow_mut(), target);
        if changed {
            self.apply();
        }
        changed
    }

    /// `swap.marked` (`eitri_core::layout::swap_marked`): the marked module it swapped with.
    pub(crate) fn swap_marked(&self, target: &ModuleId) -> Result<ModuleId, LayoutError> {
        let marked = eitri_core::layout::swap_marked(&mut self.layout().borrow_mut(), target)?;
        self.apply();
        Ok(marked)
    }

    /// `layout.undo` (`Layout::undo`): pressed again, it redoes.
    pub(crate) fn undo(&self) -> Result<(), LayoutError> {
        self.layout().borrow_mut().undo()?;
        self.apply();
        Ok(())
    }

    /// i3's `move <dir>` (`eitri_core::layout::move_dir`): the module, or the selected container, one
    /// step that way. `Ok(false)` when there is nowhere to go.
    pub(crate) fn move_dir(&self, target: &ModuleId, dir: Direction) -> Result<bool, LayoutError> {
        let moved = eitri_core::layout::move_dir(&mut self.layout().borrow_mut(), target, dir)?;
        if moved {
            self.apply();
        }
        Ok(moved)
    }

    /// i3's `layout tabbed`/`layout stacking` on the container `target` sits in, or the selected one
    /// (`eitri_core::layout::container_mode`); pressed again on such a container, back to its split.
    /// `Ok(false)` when there is no container to change.
    pub(crate) fn container_mode(&self, target: &ModuleId, mode: ConLayout) -> Result<bool, LayoutError> {
        let changed = eitri_core::layout::container_mode(&mut self.layout().borrow_mut(), target, mode)?;
        if changed {
            self.apply();
        }
        Ok(changed)
    }

    /// i3's `focus parent` (`eitri_core::layout::focus_parent`). Only the selection changes, which
    /// nothing on screen draws inside the grid: no `apply`, and no change for the state file.
    pub(crate) fn focus_parent(&self) -> bool {
        eitri_core::layout::focus_parent(&mut self.layout().borrow_mut())
    }

    /// i3's `focus child` (`eitri_core::layout::focus_child`), as [`ModuleGrid::focus_parent`].
    pub(crate) fn focus_child(&self) -> bool {
        eitri_core::layout::focus_child(&mut self.layout().borrow_mut())
    }

    /// The height of a strip row: a strip entry's own CSS size (`constrip > label`), or
    /// [`DEFAULT_STRIP_PX`] until the first strip has an entry to measure.
    fn strip_px(&self) -> i32 {
        self.imp()
            .strips
            .borrow()
            .first()
            .and_then(|s| s.row_px())
            .unwrap_or(DEFAULT_STRIP_PX)
            .max(1)
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
        self.with_frame(self.size(), |frame| eitri_core::layout::min_size(&layout, frame))
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
            strip_px: self.strip_px(),
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

    /// `Ctrl+a h/j/k/l` around `target` by `px` (`eitri_core::layout::resize`).
    pub(crate) fn resize(&self, target: &ModuleId, direction: Direction, px: i32) {
        let size = self.size();
        let moved = {
            let layout = self.layout();
            let mut layout = layout.borrow_mut();
            self.with_frame(size, |frame| {
                eitri_core::layout::resize(&mut layout, target, direction, px, frame)
            })
        };
        if moved {
            self.apply();
        }
    }

    /// Where `Ctrl+h/j/k/l` from `from` goes, on the geometry the grid is allocated with
    /// (`eitri_core::layout::navigate`). Decides only; the caller unzooms and moves focus.
    pub(crate) fn navigate(&self, from: &ModuleId, direction: Direction) -> Nav {
        let size = self.size();
        let layout = self.layout();
        let layout = layout.borrow();
        self.with_frame(size, |frame| {
            eitri_core::layout::navigate(&layout, from, direction, frame)
        })
    }

    /// Hides `id`. If it holds the keys, the module chosen by the layout receives them through
    /// `focus` before `id` is unmapped, so GTK's focus chain cannot give them to the WebView instead.
    /// [`hide_then_unmap`] enforces this order and is tested without a display; this method supplies
    /// the grid's frame and the GTK effects.
    /// Every hide uses this path: `Ctrl+a x`, the focused module's toggle key, `Ctrl+a t`, and the Lua hide API.
    pub(crate) fn hide_module(&self, id: &ModuleId, focus: &dyn Fn(&ModuleId) -> bool) -> Result<(), LayoutError> {
        let size = self.size();
        let layout = self.layout();
        self.with_frame(size, |frame| {
            hide_then_unmap(
                &layout,
                id,
                frame,
                |next| self.map_arriving(next),
                |next| {
                    focus(next);
                },
                || self.apply(),
            )
        })
    }

    /// `prefix x` (`eitri_core::layout::kill`): `id` leaves the screen as a hide does -- the keys to
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
                |layout| eitri_core::layout::kill(layout, id, reopen, frame),
                |next| self.map_arriving(next),
                |next| {
                    focus(next);
                },
                || self.apply(),
            )
        })
    }

    /// Draws `to`'s tab and gives it the keys, wherever they were: a strip click, `Ctrl+h/j/k/l`, a
    /// module key and `eitri.layout.focus` all choose a tab this way
    /// (`eitri_core::layout::switch_tab`). In [`leave_then_unmap`]'s order: the layout switches,
    /// `to`'s host is mapped, `focus` gives it the keys, and only then is the tab it replaced
    /// unmapped. Unmapping first would leave the keys on a widget leaving the screen, and GTK's own
    /// focus chain would choose where they go.
    pub(crate) fn switch_tab(&self, to: &ModuleId, focus: &dyn Fn(&ModuleId) -> bool) -> Result<(), LayoutError> {
        let layout = self.layout();
        leave_then_unmap(
            &layout,
            |layout| eitri_core::layout::switch_tab(layout, to).map(Some),
            |next| self.map_arriving(next),
            |next| {
                focus(next);
            },
            || self.apply(),
        )
    }

    /// Maps `id`'s host if the layout now draws it and it is not on screen yet: a tab that became the
    /// drawn one is still unmapped until the next [`ModuleGrid::apply`], and a widget that is not
    /// mapped cannot take the keys. Runs with the layout released (it is read here).
    fn map_arriving(&self, id: &ModuleId) {
        if self.probe_places_host(id) {
            return;
        }
        let drawn = self.layout().try_borrow().is_ok_and(|layout| layout.is_visible(id));
        if !drawn {
            return;
        }
        // Cloned out: mapping realizes the host, and nothing that runs then should find it borrowed.
        let widget = self
            .imp()
            .hosts
            .borrow()
            .iter()
            .find(|h| h.id == *id)
            .map(|h| h.widget.clone());
        if let Some(widget) = widget {
            if !widget.is_child_visible() {
                widget.set_child_visible(true);
            }
        }
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
                    eitri_core::layout::settle_pins(&mut layout, frame)
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
            if self.probe_places_host(&host.id) {
                continue;
            }
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
        self.allocate_probe_hosts();
        let strip_px = self.strip_px();
        self.place_strips(&arrangement.strips);
        if self.strip_px() != strip_px {
            // The first strip measured a row height other than the stand-in this pass used. Asked
            // for after the pass, not inside it: an allocation does not queue another of itself.
            let grid = self.downgrade();
            glib::idle_add_local_once(move || {
                if let Some(grid) = grid.upgrade() {
                    grid.queue_resize();
                }
            });
        }
        self.place_handles(&arrangement.dividers);
        // Only a new target restarts the quiet period (`WebAllocation::restart_quiet`). The second
        // clause is belt and braces -- the first hold of any target already restarts -- so that a
        // held host is never left without a timer to settle it.
        if restart_quiet || (held && imp.settle.borrow().is_none()) {
            self.settle_after_quiet();
        }
    }

    /// Whether the probe places `id` by hand. Always false without the `probe` feature.
    #[cfg(feature = "probe")]
    fn probe_places_host(&self, id: &ModuleId) -> bool {
        self.imp().probe_places.borrow().contains_key(id)
    }

    #[cfg(not(feature = "probe"))]
    fn probe_places_host(&self, _id: &ModuleId) -> bool {
        false
    }

    /// Puts `id`'s host where the probe says, after the tiled ones, and `None` gives it back to the
    /// layout. A host placed here is not in the layout's tree at all or is tiled and overridden; either
    /// way the layout's arrangement does not allocate it.
    #[cfg(feature = "probe")]
    pub(crate) fn probe_place(&self, id: &ModuleId, place: Option<ProbePlace>) {
        let widget = self
            .imp()
            .hosts
            .borrow()
            .iter()
            .find(|h| h.id == *id)
            .map(|h| h.widget.clone());
        let Some(widget) = widget else { return };
        match place {
            Some(place) => {
                self.imp().probe_places.borrow_mut().insert(id.clone(), place);
                widget.set_child_visible(matches!(place, ProbePlace::At(_)));
                self.queue_allocate();
            }
            None => {
                self.imp().probe_places.borrow_mut().remove(id);
                self.apply();
            }
        }
    }

    #[cfg(feature = "probe")]
    fn allocate_probe_hosts(&self) {
        let places: Vec<(ModuleId, ProbePlace)> = self
            .imp()
            .probe_places
            .borrow()
            .iter()
            .map(|(id, place)| (id.clone(), *place))
            .collect();
        for (id, place) in places {
            let ProbePlace::At(rect) = place else { continue };
            let Some(widget) = self
                .imp()
                .hosts
                .borrow()
                .iter()
                .find(|h| h.id == id)
                .map(|h| h.widget.clone())
            else {
                continue;
            };
            let _ = widget.measure(gtk4::Orientation::Horizontal, -1);
            let _ = widget.measure(gtk4::Orientation::Vertical, rect.w);
            widget.size_allocate(&gtk4::Allocation::new(rect.x, rect.y, rect.w, rect.h), -1);
        }
    }

    #[cfg(not(feature = "probe"))]
    fn allocate_probe_hosts(&self) {}

    /// Draws each strip of the arrangement with a strip of the pool, in order, growing the pool only
    /// when this pass has more strips than it has ever had ([`strip_assignments`]); the rest are
    /// hidden. A strip made here is parented at its band's place, as a handle is.
    fn place_strips(&self, strips: &[Strip]) {
        let (create, slots) = strip_assignments(self.imp().strips.borrow().len(), strips.len());
        for _ in 0..create {
            let grid = self.downgrade();
            let strip = ConStrip::new(Rc::new(move |id: &ModuleId| {
                if let Some(grid) = grid.upgrade() {
                    grid.strip_activated(id);
                }
            }));
            self.insert_in_band(strip.widget.upcast_ref(), Band::Strip);
            self.imp().strips.borrow_mut().push(strip);
        }
        let label = self.strip_label();
        let pool = self.imp().strips.borrow();
        for (i, con_strip) in pool.iter().enumerate() {
            let Some(strip) = slots.iter().position(|&slot| slot == i).map(|n| &strips[n]) else {
                con_strip.hide();
                continue;
            };
            con_strip.show(strip, &*label);
            let r = strip.rect;
            let _ = con_strip.widget.measure(gtk4::Orientation::Horizontal, -1);
            let _ = con_strip.widget.measure(gtk4::Orientation::Vertical, r.w);
            con_strip
                .widget
                .size_allocate(&gtk4::Allocation::new(r.x, r.y, r.w, r.h), -1);
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

/// [`ModuleGrid::hide_module`]'s order, with GTK effects passed in for tests
/// without a display. `layout::hide` records the hide and chooses the most recent
/// neighbor if `id` holds the keys, or the next drawn tab if `id` was a drawn tab.
/// `map` shows the destination's host if it was an undrawn tab. `focus` then gives
/// it the keys before `unmap` removes `id` (`ModuleGrid::apply`,
/// `set_child_visible(false)`).
///
/// Unmapping the focused widget first lets GTK's focus chain choose the next
/// widget instead of the layout. Keeping the effects separate makes their order testable.
/// The layout is released before GTK effects run: focus changes write its focus,
/// and `apply` reads it.
fn hide_then_unmap(
    layout: &RefCell<Layout>,
    id: &ModuleId,
    frame: &Frame,
    map: impl FnOnce(&ModuleId),
    focus: impl FnOnce(&ModuleId),
    unmap: impl FnOnce(),
) -> Result<(), LayoutError> {
    leave_then_unmap(
        layout,
        |layout| eitri_core::layout::hide(layout, id, frame),
        map,
        focus,
        unmap,
    )
}

/// [`hide_then_unmap`]'s order for any way of leaving the screen (a hide, a kill, a tab switch):
/// `leave` changes the layout and names who gets the keys, `map` puts that module's host on screen
/// (a tab that was not drawn a moment ago has no mapped widget to take them), `focus` gives them,
/// `unmap` takes what left off screen.
fn leave_then_unmap(
    layout: &RefCell<Layout>,
    leave: impl FnOnce(&mut Layout) -> Result<Option<ModuleId>, LayoutError>,
    map: impl FnOnce(&ModuleId),
    focus: impl FnOnce(&ModuleId),
    unmap: impl FnOnce(),
) -> Result<(), LayoutError> {
    let next = leave(&mut layout.borrow_mut())?;
    if let Some(next) = next {
        map(&next);
        focus(&next);
    }
    unmap();
    Ok(())
}

/// The layers of the grid's child list, bottom to top. GTK picks and paints in child-list order, so a
/// band higher up takes a press where its widget overlaps one lower down, and paints over it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Band {
    /// A module host: below everything else, so a strip or a divider at its edge gets the press.
    Host,
    /// A tabbed or stacked container's strip: above the hosts, whose rectangles it borders.
    Strip,
    /// A divider handle: above hosts and strips, so its edge columns are always draggable.
    Handle,
    /// A floating module's host, above every tiled thing. Nothing floats yet; the variant is here so
    /// the order that floats need is fixed and tested now, and floating only adds widgets.
    #[allow(dead_code)]
    FloatHost,
    /// A floating module's title bar, right after its float host. Unused for the same reason as
    /// [`Band::FloatHost`].
    #[allow(dead_code)]
    FloatBar,
}

impl Band {
    /// Lower is further down. A float host and its bar share the top rank: each bar is added right
    /// after its own host, which is then the last child, so both go to the end and stay paired.
    fn rank(self) -> u8 {
        match self {
            Band::Host => 0,
            Band::Strip => 1,
            Band::Handle => 2,
            Band::FloatHost | Band::FloatBar => 3,
        }
    }
}

/// Returns the index for band `new` before the first higher band in `children`, preserving insertion order
/// within each band. Hosts precede strips, strips precede handles, and handles precede floating
/// hosts. A floating host and its bar are appended together at the end.
///
/// GTK picks children from last to first (`gtk_widget_do_pick`). A widget contains both edges
/// of its border box: `gsk_rounded_rect_locate_point` excludes a point only beyond `x + width`.
/// A host ending where a divider begins therefore contains the divider's first pixel column.
/// The later child receives a press there, so a host appended after a handle can take its clicks.
/// Handles also paint after every host and strip.
fn band_insert_index(children: &[Band], new: Band) -> usize {
    children
        .iter()
        .position(|band| band.rank() > new.rank())
        .unwrap_or(children.len())
}

/// [`ModuleGrid::focus_target`]'s choice, made on the grid's own host records (generic over the
/// widget so it is tested without a display): the focus target recorded for `id`, never its host.
fn focus_target_of<'a, W>(hosts: &'a [Host<W>], id: &ModuleId) -> Option<&'a W> {
    hosts.iter().find(|h| h.id == *id).map(|h| &h.focus)
}

/// Allows `module` into the tree only if this window has a host for it. Parsing an id does not
/// ensure a host exists: a mistyped `lua:x` or an uncreated `canvas` can still be a valid id.
/// A leaf without a host leaves space empty, lacks a divider and can receive focus with no widget.
/// The grid knows which hosts exist; a canvas host must be added before placing the canvas.
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
    if now.con != begin.con || now.index != begin.index || now.axis != begin.axis {
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
    use eitri_core::layout::{Arrangement, Node};

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
            layout.set_ratio(&now, now.ratio_for(first_px)).unwrap();
        }
        assert_eq!(arrange(&layout, &frame).dividers[0].rect.x, pointer_start + 400);

        let other_container = Divider {
            con: eitri_core::layout::ConId::fresh(),
            ..begin.clone()
        };
        assert_eq!(
            dragged_first_px(&begin, &other_container, 5.0, 0.0),
            None,
            "another container"
        );
        let other_divider = Divider {
            index: begin.index + 1,
            ..begin.clone()
        };
        assert_eq!(
            dragged_first_px(&begin, &other_divider, 5.0, 0.0),
            None,
            "another divider of the same container"
        );
    }

    /// One child of the grid, in the model below.
    #[derive(Debug, Clone, PartialEq)]
    enum Child {
        Host(ModuleId),
        /// The n-th handle the grid made: it draws `arrangement.dividers[n]` (`place_handles`).
        Handle(usize),
    }

    /// The child list `ModuleGrid::add` builds for hosts added in this order: each host where
    /// [`band_insert_index`] puts a host, then (from the second host on) one handle where it puts a
    /// handle, as `new_handle` does.
    fn stack_as_add_builds(ids: &[ModuleId]) -> Vec<Child> {
        let band = |c: &Child| match c {
            Child::Host(_) => Band::Host,
            Child::Handle(_) => Band::Handle,
        };
        let mut children: Vec<Child> = Vec::new();
        for (i, id) in ids.iter().enumerate() {
            let bands: Vec<Band> = children.iter().map(band).collect();
            children.insert(band_insert_index(&bands, Band::Host), Child::Host(id.clone()));
            if i > 0 {
                let bands: Vec<Band> = children.iter().map(band).collect();
                children.insert(band_insert_index(&bands, Band::Handle), Child::Handle(i - 1));
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
        panels: &[(&str, eitri_core::layout::Placement)],
        terminal_shown: bool,
    ) -> (Layout, Vec<ModuleId>) {
        let decls: Vec<eitri_core::layout::ModuleDecl> = panels
            .iter()
            .map(|(id, placement)| eitri_core::layout::ModuleDecl {
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

    /// A divider press reaches its handle in each tested layout at both integer pixel columns
    /// (or rows): the divider's own coordinate and the one touched by its right (or bottom) edge.
    ///
    /// A Lua `main` host appended after the root handle ends where the divider begins and can
    /// therefore take the press at the divider's own column. The negative control reproduces this.
    #[test]
    fn a_press_on_any_divider_reaches_its_handle_in_every_p1_layout() {
        use eitri_core::layout::Placement::{BelowRoot, InPlaceOfEditor, RightOfRoot};
        let layouts: &[&[(&str, eitri_core::layout::Placement)]] = &[
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
                    // The terminal's pinned split surrounds the editor's leaf, which can contain another split.
                    // With two visible Lua `main` panels, the terminal divider's starting corner also touches the
                    // bottom edge of the main1/main2 divider. The later-added handle is higher and wins that pixel.
                    // Both targets are adjacent divider handles. The explicit exception below checks this corner
                    // while keeping the assertion that hosts cannot take divider presses.
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
                        "{panels:?} (terminal shown: {terminal_shown}): a press at ({x}, {y}) on divider {n} \
                         (child {} of {:?})",
                        divider.index,
                        divider.con
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
            ..Default::default()
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

    /// What [`leave_then_unmap`] did, in order, as its effects saw the layout when each ran.
    #[derive(Debug, PartialEq)]
    enum Effect {
        /// The layout changed, and named who gets the keys.
        Leave,
        /// A host that was off the screen was put on it.
        Map(ModuleId),
        Focus(ModuleId),
        Unmap,
    }

    /// Runs `leave` on `layout` through [`leave_then_unmap`] and records each effect as it runs, with
    /// the grid's mapped hosts modelled as the set of modules on screen before `leave`. Each effect
    /// checks what the real one relies on: the layout is not borrowed (mapping reads it, a focus
    /// change reaches `pane_focus`, which writes it, and `apply` reads it). `map` records a map only
    /// where the real one maps -- a module the layout now draws whose host is not on screen.
    /// `focus` checks that its target is shown (`main.rs`'s `focus_module` refuses a hidden module)
    /// and that its host is mapped, since an unmapped widget cannot take the keys. `unmap` checks
    /// that `leaving` is off the screen in the layout, so `apply` unmaps it.
    fn leave_and_record(
        layout: &RefCell<Layout>,
        leaving: &ModuleId,
        leave: impl FnOnce(&mut Layout) -> Result<Option<ModuleId>, LayoutError>,
    ) -> (Result<(), LayoutError>, Vec<Effect>) {
        let effects = RefCell::new(Vec::new());
        let mapped: RefCell<BTreeSet<ModuleId>> = RefCell::new(layout.borrow().visible_leaves().into_iter().collect());
        let result = leave_then_unmap(
            layout,
            |l| {
                let next = leave(l)?;
                effects.borrow_mut().push(Effect::Leave);
                Ok(next)
            },
            |next| {
                let l = layout
                    .try_borrow_mut()
                    .expect("the layout is released before a host maps");
                if l.is_visible(next) && mapped.borrow_mut().insert(next.clone()) {
                    effects.borrow_mut().push(Effect::Map(next.clone()));
                }
            },
            |next| {
                let l = layout
                    .try_borrow_mut()
                    .expect("the layout is released before the keys move");
                assert!(
                    !l.is_visible(leaving),
                    "{leaving} is off screen in the layout before anything moves"
                );
                assert!(l.is_shown(next), "the keys go to a shown module, {next}");
                assert!(
                    mapped.borrow().contains(next),
                    "{next}'s host is mapped before it gets the keys"
                );
                effects.borrow_mut().push(Effect::Focus(next.clone()));
            },
            || {
                let l = layout
                    .try_borrow_mut()
                    .expect("the layout is released before the unmap");
                assert!(!l.is_visible(leaving), "apply unmaps what the layout takes off screen");
                effects.borrow_mut().push(Effect::Unmap);
            },
        );
        (result, effects.into_inner())
    }

    /// Hides `id` in `layout` as [`hide_then_unmap`] does, recording each effect ([`leave_and_record`]).
    fn hide_and_record(layout: &RefCell<Layout>, id: &ModuleId) -> (Result<(), LayoutError>, Vec<Effect>) {
        leave_and_record(layout, id, |l| {
            eitri_core::layout::hide(l, id, &Frame::new(UNALLOCATED, 1))
        })
    }

    /// Hiding the focused terminal with `Ctrl+a t` gives the keys to the most recent
    /// module above it before unmapping the terminal. Swapping those effects in
    /// `hide_then_unmap` must fail this test. Both upper modules are tried as the
    /// last focused module, so always choosing the same one cannot pass.
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
            assert_eq!(
                effects,
                [Effect::Leave, Effect::Focus(last_above.clone()), Effect::Unmap]
            );
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
        let (result, effects) = leave_and_record(&layout, &id, |l| {
            eitri_core::layout::kill(
                l,
                &id,
                Reopen::At(eitri_core::layout::Placement::BelowEditor),
                &Frame::new(UNALLOCATED, 1),
            )
        });
        assert_eq!(result, Ok(()));
        assert_eq!(
            effects,
            [Effect::Leave, Effect::Focus(ModuleId::agent()), Effect::Unmap]
        );
        assert_eq!(
            layout.borrow().root(),
            crate::terminal::initial_layout(&[]).unwrap().root(),
            "back where a first launch puts it"
        );
    }

    /// A numbered terminal killed while it holds the keys as the drawn tab of a tabbed container: the
    /// tab drawn next is mapped and given the keys before the killed one is unmapped, and the killed
    /// one leaves the layout altogether (`Reopen::Remove`).
    #[test]
    fn killing_an_instance_gives_the_keys_away_before_it_unmaps() {
        use eitri_core::layout::{Con, ConLayout};
        let (two, base) = (ModuleId::parse("terminal#2").unwrap(), ModuleId::terminal());
        let leaf = |id: ModuleId| eitri_core::layout::Child::new(Node::Leaf(id), 0.5);
        let tabs = Con::new(ConLayout::Tabbed, vec![leaf(two.clone()), leaf(base.clone())]);
        let root = Con::new(
            ConLayout::SplitH,
            vec![
                leaf(ModuleId::editor()),
                eitri_core::layout::Child::new(Node::Con(tabs), 0.5),
            ],
        );
        let mut layout = Layout::new(Node::Con(root), ModuleId::editor()).unwrap();
        layout.set_focus(&two).unwrap();
        assert!(
            layout.is_visible(&two) && !layout.is_visible(&base),
            "the premise: #2 is drawn"
        );
        let layout = RefCell::new(layout);
        let (result, effects) = leave_and_record(&layout, &two, |l| {
            eitri_core::layout::kill(l, &two, Reopen::Remove, &Frame::new(UNALLOCATED, 1))
        });
        assert_eq!(result, Ok(()));
        assert_eq!(
            effects,
            [
                Effect::Leave,
                Effect::Map(base.clone()),
                Effect::Focus(base.clone()),
                Effect::Unmap
            ]
        );
        let layout = layout.borrow();
        assert_eq!(layout.focus(), &base);
        assert!(!layout.contains(&two), "an instance leaves the layout");
        assert!(layout.is_visible(&base));
    }

    /// `editor` beside a tabbed container of `lua:a` (drawn) and `lua:b`, with the keys on `keys`.
    fn editor_beside_tabs(keys: &str) -> RefCell<Layout> {
        use eitri_core::layout::{Con, ConLayout};
        let leaf = |id: ModuleId| eitri_core::layout::Child::new(Node::Leaf(id), 0.5);
        let tabs = Con::new(
            ConLayout::Tabbed,
            vec![leaf(ModuleId::lua("a")), leaf(ModuleId::lua("b"))],
        );
        let root = Con::new(
            ConLayout::SplitH,
            vec![
                leaf(ModuleId::editor()),
                eitri_core::layout::Child::new(Node::Con(tabs), 0.5),
            ],
        );
        let mut layout = Layout::new(Node::Con(root), ModuleId::lua("a")).unwrap();
        layout.set_focus(&ModuleId::parse(keys).unwrap()).unwrap();
        assert!(
            layout.is_visible(&ModuleId::lua("a")),
            "the premise: a is the drawn tab"
        );
        assert!(!layout.is_visible(&ModuleId::lua("b")), "the premise: b is not drawn");
        RefCell::new(layout)
    }

    /// A tab switch gives the keys to the tab it draws, and that tab's host is mapped before it is
    /// given them; the tab it replaced is unmapped last. With the keys on the tab being replaced, an
    /// unmap first would leave GTK's focus chain to choose where they go; with the keys outside the
    /// container, focusing a host that is still unmapped would leave them where they were. Both
    /// starting places are tried, since a tab switch always moves the keys to the tab.
    #[test]
    fn a_tab_switch_maps_the_arriving_host_before_it_gets_the_keys() {
        let (a, b) = (ModuleId::lua("a"), ModuleId::lua("b"));
        for keys in ["lua:a", "editor"] {
            let layout = editor_beside_tabs(keys);
            let (result, effects) = leave_and_record(&layout, &a, |l| eitri_core::layout::switch_tab(l, &b).map(Some));
            assert_eq!(result, Ok(()), "keys on {keys}");
            assert_eq!(
                effects,
                [
                    Effect::Leave,
                    Effect::Map(b.clone()),
                    Effect::Focus(b.clone()),
                    Effect::Unmap
                ],
                "keys on {keys}"
            );
            let layout = layout.borrow();
            assert_eq!(layout.focus(), &b, "keys on {keys}");
            assert!(layout.is_visible(&b) && !layout.is_visible(&a), "keys on {keys}");
        }

        // The tab already drawn: nothing to map, and the keys still move to it.
        let layout = editor_beside_tabs("editor");
        let (result, effects) = leave_and_record(&layout, &b, |l| eitri_core::layout::switch_tab(l, &a).map(Some));
        assert_eq!(result, Ok(()));
        assert_eq!(effects, [Effect::Leave, Effect::Focus(a.clone()), Effect::Unmap]);
        assert_eq!(layout.borrow().focus(), &a);

        // A refused switch (a module not in the tree) does nothing at all.
        let layout = editor_beside_tabs("editor");
        let missing = ModuleId::lua("missing");
        let (result, effects) =
            leave_and_record(&layout, &a, |l| eitri_core::layout::switch_tab(l, &missing).map(Some));
        assert_eq!(result, Err(LayoutError::NotInTree(missing)));
        assert!(effects.is_empty(), "{effects:?}");
    }

    /// Hiding the drawn tab while it holds the keys gives them to the tab drawn next, which was
    /// unmapped a moment ago: its host is mapped before it gets them, and the hidden tab is unmapped
    /// last. A kill takes the same door, through the same `map` step.
    #[test]
    fn hiding_the_drawn_tab_maps_the_next_tab_before_it_gets_the_keys() {
        let (a, b) = (ModuleId::lua("a"), ModuleId::lua("b"));
        let layout = editor_beside_tabs("lua:a");
        let (result, effects) = hide_and_record(&layout, &a);
        assert_eq!(result, Ok(()));
        assert_eq!(
            effects,
            [
                Effect::Leave,
                Effect::Map(b.clone()),
                Effect::Focus(b.clone()),
                Effect::Unmap
            ]
        );
        assert_eq!(layout.borrow().focus(), &b);

        // Hidden without the keys: the next tab is drawn and mapped by `apply`, and no keys move.
        let layout = editor_beside_tabs("editor");
        let (result, effects) = hide_and_record(&layout, &a);
        assert_eq!(result, Ok(()));
        assert_eq!(effects, [Effect::Leave, Effect::Unmap]);
        assert_eq!(layout.borrow().focus(), &ModuleId::editor());
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
        assert_eq!(effects, [Effect::Leave, Effect::Unmap]);
        assert_eq!(layout.borrow().focus(), &ModuleId::editor());

        let (result, effects) = hide_and_record(&layout, &ModuleId::lua("nowhere"));
        assert_eq!(result, Err(LayoutError::NotInTree(ModuleId::lua("nowhere"))));
        assert!(effects.is_empty(), "{effects:?}");

        let mut alone = Layout::initial(&[]).unwrap();
        eitri_core::layout::hide(&mut alone, &ModuleId::agent(), &Frame::new(UNALLOCATED, 1)).unwrap();
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
            band_insert_index(&[], Band::Host),
            0,
            "the first host, into an empty grid"
        );
    }

    /// Every add puts its child at its band's place, whatever was added before it and in whatever
    /// order: hosts, then strips, then handles, then each float host followed by its own bar, and
    /// inside each band the order things were added in. Every interleaving of six adds over hosts,
    /// strips, handles and floats (a float host with its bar added right after it) is replayed, so a
    /// new host, strip or handle added while floats exist is checked to land below the first float
    /// host.
    #[test]
    fn the_band_order_holds_for_every_add() {
        // What each add brings: a host, a strip, a handle, or a float host and then its bar.
        const KINDS: [&[Band]; 4] = [
            &[Band::Host],
            &[Band::Strip],
            &[Band::Handle],
            &[Band::FloatHost, Band::FloatBar],
        ];
        const ADDS: u32 = 6;
        for mut code in 0..KINDS.len().pow(ADDS) {
            // Each child is its band and the number of the add that made it.
            let mut children: Vec<(Band, usize)> = Vec::new();
            let mut order = Vec::new();
            for n in 0..ADDS as usize {
                let kind = KINDS[code % KINDS.len()];
                code /= KINDS.len();
                order.push(kind[0]);
                for &band in kind {
                    let bands: Vec<Band> = children.iter().map(|(b, _)| *b).collect();
                    let first_float = bands.iter().position(|b| *b == Band::FloatHost);
                    let at = band_insert_index(&bands, band);
                    if band != Band::FloatHost && band != Band::FloatBar {
                        if let Some(first_float) = first_float {
                            assert!(at <= first_float, "{order:?}: a {band:?} above the first float host");
                        }
                    }
                    children.insert(at, (band, n));
                }
            }
            let bands: Vec<Band> = children.iter().map(|(b, _)| *b).collect();
            assert!(
                bands.windows(2).all(|w| w[0].rank() <= w[1].rank()),
                "{order:?}: out of band order: {bands:?}"
            );
            for band in [Band::Host, Band::Strip, Band::Handle, Band::FloatHost] {
                let adds: Vec<usize> = children.iter().filter(|(b, _)| *b == band).map(|(_, n)| *n).collect();
                assert!(
                    adds.windows(2).all(|w| w[0] < w[1]),
                    "{order:?}: {band:?}s out of the order they were added in: {adds:?}"
                );
            }
            for (i, (band, n)) in children.iter().enumerate() {
                if *band == Band::FloatHost {
                    assert_eq!(
                        children.get(i + 1),
                        Some(&(Band::FloatBar, *n)),
                        "{order:?}: a float host's bar sits right after it: {children:?}"
                    );
                }
            }
        }
    }

    /// Checks a drag inside a split that starts to the right of the grid's origin. The handle's
    /// `rect.x` includes the space left of the split, while `first_px` is relative to the split.
    /// The root-split replay cannot distinguish these coordinates because its split starts at zero,
    /// so it does not catch an implementation that adds the offset to `rect.x`.
    #[test]
    fn a_dragged_divider_inside_a_nested_split_stays_under_the_pointer() {
        use eitri_core::layout::Node;
        let l = ModuleId::lua;
        let root = Node::split(
            Axis::Row,
            0.4,
            Node::Leaf(l("a")),
            Node::split(Axis::Row, 0.5, Node::Leaf(l("b")), Node::Leaf(l("c"))),
        );
        let mut layout = Layout::new(root, l("b")).unwrap();
        let frame = Frame::new(UNALLOCATED, 1);
        let outer = layout.root().id;
        let inner = |layout: &Layout| {
            arrange(layout, &frame)
                .dividers
                .into_iter()
                .find(|d| d.con != outer)
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
            layout.set_ratio(&now, now.ratio_for(first_px)).unwrap();
        }
        assert_eq!(inner(&layout).rect.x, pointer_start + 150);
    }

    /// Dragging the bottom terminal's pinned divider keeps it under the pointer and stores a
    /// height in pixels that remains unchanged when the window grows taller.
    /// This divider is nested below the editor's leaf rather than at the root, so the test finds it
    /// by its `Column` axis instead of an empty path.
    #[test]
    fn a_dragged_pinned_divider_stays_under_the_pointer_and_keeps_its_height() {
        let mut layout = crate::terminal::initial_layout(&[]).unwrap();
        layout.show(&ModuleId::terminal()).unwrap();
        let frame = Frame::new(UNALLOCATED, 1);
        assert!(eitri_core::layout::settle_pins(&mut layout, &frame));
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
            eitri_core::layout::move_divider(&mut layout, &now, first_px).unwrap();
        }
        assert_eq!(terminal_divider(&layout).rect.y, 380);
        let taller = Frame::new(eitri_core::layout::Size { w: 1280, h: 1041 }, 1);
        assert_eq!(
            arrange(&layout, &taller).rect_of(&ModuleId::terminal()).map(|r| r.h),
            Some(340),
            "dragged to 340px, and kept at 340 when the window grows"
        );
    }

    /// Rejects a `canvas` with no host or a mistyped `lua:` id, using the same missing-module error
    /// as other layout operations instead of inserting a leaf with no host.
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
    /// the host fails here.
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
