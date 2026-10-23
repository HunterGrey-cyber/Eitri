//! `ModuleGrid` contains every module. Each host gets its parent once in [`ModuleGrid::add`]
//! and loses it only in `dispose`, when the window goes away. A web module's `WebView` is
//! parented to its [`WebHost`] before the host is added to the grid.
//!
//! Hiding uses `set_child_visible(false)`. Zoom, resize, split, swap, move and even operations
//! allocate the same children again. Floating or raising a module reorders children on the
//! same parent through [`ModuleGrid::restack`], which never moves the editor.
//! The terminal's `GtkGLArea` has no unrealize handler, and reparenting a `WebView` unrealizes it.
//! Re-realizing a GL module on another toplevel with a new GL context, frame clock and scale
//! has not been exercised. The editor releases its Skia state on unrealize and keeps nvim
//! across re-realization. Each host logs unrealize in [`ModuleGrid::add`] so unexpected
//! unrealization can be detected.
//!
//! Rectangles come from the layout's `arrange` function. The grid measures children's minimum
//! sizes, allocates the resulting geometry, holds web hosts at their settled size during
//! resizing, and converts divider drags into ratios.

pub(crate) mod floatbars;
pub(crate) mod strips;

use std::cell::{Cell, RefCell};
use std::collections::BTreeSet;
use std::rc::{Rc, Weak};

use gtk4::glib;
use gtk4::graphene;
use gtk4::prelude::*;
use gtk4::subclass::prelude::*;

use eitri_core::layout::{
    arrange, covering, Arrangement, Axis, Direction, Divider, FloatChange, FloatRect, Frame, Layout, LayoutError,
    ModuleDecl, ModuleId, OpenRule, Opened, Rect, Reopen, ScratchShow, Size, Strip, WorkspaceError, Workspaces, Zoom,
};

use eitri_core::wm::grid::throttle::{WebThrottle, QUIET};
#[cfg(test)]
use eitri_core::wm::grid::SlotMove;
#[cfg(test)]
pub(crate) use eitri_core::wm::grid::UNALLOCATED;
use eitri_core::wm::grid::{
    band_insert_index, floats_need_restack, frame_size, placeable, restack_moves, shown_floats, strip_assignments,
    Band, Slot, DEFAULT_STRIP_PX,
};
use eitri_core::wm::{Allocator, WinId};
use floatbars::{chrome_owners, chrome_plan, grip_rect, ChromeHooks, FloatChrome};
use strips::ConStrip;

use crate::gtk_host::GridPort;
use crate::web_host::WebHost;

/// The grid's CSS node name. `theme::gtk_css` styles its dividers as `modulegrid.content-area >
/// separator`, the rule `paned.content-area > separator` was before the grid replaced the paneds.
pub(crate) const CSS_NAME: &str = "modulegrid";

/// How a host follows its rectangle while it changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HostKind {
    /// Allocated its rectangle on every pass, as the editor always was.
    Direct,
    /// A `WebView`, in its [`WebHost`]: held at its settled size while being resized
    /// ([`eitri_core::wm::grid::throttle`]).
    Web,
}

/// Every module and its host, asked for at the moment it is needed, so a module added after the
/// window is up is never missing from it. In whatever order the closure's builder promises:
/// `ModuleGrid::live_hosts` is add order, and HINT's (`HintWidgets::modules`) is the layout's tree
/// order.
pub(crate) type ModuleHosts = Rc<dyn Fn() -> Vec<(ModuleId, gtk4::Widget)>>;

/// What runs when a strip entry is clicked, with the entry's module.
type StripHook = Rc<dyn Fn(&ModuleId)>;

/// A hook run after a change ([`ModuleGrid::connect_changed`], [`ModuleGrid::connect_chrome_changed`]).
type ChangeHook = Rc<dyn Fn()>;

/// The grid's two lists of change hooks. A change the grid applied itself runs both, the saver's first;
/// a change the window controller made runs only the chrome's, since the controller tells the saver itself.
/// Each list runs in the order its hooks were added.
#[derive(Default)]
struct ChangeHooks {
    /// The state file's: told that the layout changed behind the controller.
    changed: Vec<ChangeHook>,
    /// What redraws what the layout shows outside the grid: the selection, the tray, the mark, the
    /// workspace's name.
    chrome: Vec<ChangeHook>,
}

impl ChangeHooks {
    /// The hooks a change the grid applied runs, in order. Cloned out: a hook may add another.
    fn after_apply(&self) -> Vec<ChangeHook> {
        self.changed.iter().chain(&self.chrome).cloned().collect()
    }

    /// The hooks a change the controller made runs, in order.
    fn after_chrome(&self) -> Vec<ChangeHook> {
        self.chrome.clone()
    }
}

/// A strip entry's text for its module.
type StripLabel = Rc<dyn Fn(&ModuleId) -> String>;

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
        /// Called after changes to the layout ([`ModuleGrid::connect_changed`],
        /// [`ModuleGrid::connect_chrome_changed`]).
        pub(super) hooks: RefCell<ChangeHooks>,
        /// Where the arrangement and the minimum size come from once the window controller drives the grid
        /// ([`ModuleGrid::set_allocator`]); without one the grid arranges its own layout.
        pub(super) allocator: RefCell<Option<(WinId, Weak<dyn Allocator>)>>,
        /// The float chrome pool: a bar and a grip per piece ([`floatbars`]).
        pub(super) chrome: RefCell<Vec<FloatChrome>>,
        /// Which float each piece of chrome draws, slot by slot ([`floatbars::chrome_owners`]).
        pub(super) chrome_owners: RefCell<Vec<ModuleId>>,
        /// The floats the last [`ModuleGrid::restack`] put in the float band: what `child_bands`
        /// classifies as float hosts, and what `apply` compares the layout's floats with.
        pub(super) float_band: RefCell<BTreeSet<ModuleId>>,
        /// A restack asked for from inside an allocation and not yet run.
        pub(super) restack_queued: Cell<bool>,
        /// A float bar's text for its module ([`ModuleGrid::set_float_label`]); the id until set.
        pub(super) float_label: RefCell<Option<StripLabel>>,
        /// Called with a float's module after a primary press on its bar or grip raised it
        /// ([`ModuleGrid::connect_float_bar_press`]).
        pub(super) float_bar_press: RefCell<Vec<StripHook>>,
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
            let chrome = std::mem::take(&mut *self.chrome.borrow_mut());
            for piece in chrome {
                piece.bar.unparent();
                piece.grip.unparent();
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

    /// The layout the grid draws: the same cell the window's readers hold.
    pub(crate) fn layout(&self) -> Rc<RefCell<Layout>> {
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
    /// [`ModuleGrid::hosts_in_tree_order`]. Each new host needs its own key controllers,
    /// including the module stage installed by `gtk_host::wire` when the host is added.
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
        // A float may have been applied before its host existed. Its arrival must restore its
        // z position even though the set of shown floats has not changed.
        let needs_restack = self.imp().float_band.borrow().contains(&id);
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
        if needs_restack {
            self.restack();
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

    /// The grid's children in order, and the band each belongs to. A host is a float host when the
    /// last [`ModuleGrid::restack`] put it in the float band -- not when the layout floats it now: between
    /// a layout change and the restack that follows it the two differ, and the bands must describe
    /// where each child is. Anything that is not a handle, a strip or float chrome is a module host:
    /// the grid parents nothing else.
    fn child_bands(&self) -> (Vec<gtk4::Widget>, Vec<Band>) {
        let imp = self.imp();
        let handles: Vec<gtk4::Widget> = imp.handles.borrow().iter().map(|h| h.widget.clone().upcast()).collect();
        let strips: Vec<gtk4::Widget> = imp.strips.borrow().iter().map(|s| s.widget.clone().upcast()).collect();
        let chrome: Vec<gtk4::Widget> = imp
            .chrome
            .borrow()
            .iter()
            .flat_map(|c| [c.bar.clone().upcast(), c.grip.clone().upcast()])
            .collect();
        let float_band = imp.float_band.borrow();
        let float_hosts: Vec<gtk4::Widget> = imp
            .hosts
            .borrow()
            .iter()
            .filter(|h| float_band.contains(&h.id))
            .map(|h| h.widget.clone())
            .collect();
        let mut children = Vec::new();
        let mut bands = Vec::new();
        let mut child = self.first_child();
        while let Some(c) = child {
            child = c.next_sibling();
            bands.push(if handles.contains(&c) {
                Band::Handle
            } else if strips.contains(&c) {
                Band::Strip
            } else if chrome.contains(&c) {
                Band::FloatBar
            } else if float_hosts.contains(&c) {
                Band::FloatHost
            } else {
                Band::Host
            });
            children.push(c);
        }
        (children, bands)
    }

    /// The grid's children as [`restack_moves`] sees them, in order: a piece of float chrome is one
    /// slot, named where its bar is.
    fn child_slots(&self) -> Vec<Slot> {
        let imp = self.imp();
        let hosts = imp.hosts.borrow();
        let strips = imp.strips.borrow();
        let handles = imp.handles.borrow();
        let chrome = imp.chrome.borrow();
        let mut slots = Vec::new();
        let mut child = self.first_child();
        while let Some(c) = child {
            child = c.next_sibling();
            let slot = if let Some(h) = hosts.iter().find(|h| h.widget == c) {
                Slot::Host(h.id.clone())
            } else if let Some(i) = strips.iter().position(|s| *s.widget.upcast_ref::<gtk4::Widget>() == c) {
                Slot::Strip(i)
            } else if let Some(i) = handles.iter().position(|h| *h.widget.upcast_ref::<gtk4::Widget>() == c) {
                Slot::Handle(i)
            } else if let Some(i) = chrome.iter().position(|p| *p.bar.upcast_ref::<gtk4::Widget>() == c) {
                Slot::Chrome(i)
            } else {
                // A grip: it moves with its bar.
                continue;
            };
            slots.push(slot);
        }
        slots
    }

    /// The widgets a slot stands for, bottom first: a piece of chrome is its bar, then its grip.
    fn slot_widgets(&self, slot: &Slot) -> Vec<gtk4::Widget> {
        let imp = self.imp();
        match slot {
            Slot::Host(id) => imp
                .hosts
                .borrow()
                .iter()
                .filter(|h| h.id == *id)
                .map(|h| h.widget.clone())
                .collect(),
            Slot::Strip(i) => imp
                .strips
                .borrow()
                .get(*i)
                .map(|s| s.widget.clone().upcast())
                .into_iter()
                .collect(),
            Slot::Handle(i) => imp
                .handles
                .borrow()
                .get(*i)
                .map(|h| h.widget.clone().upcast())
                .into_iter()
                .collect(),
            Slot::Chrome(i) => imp
                .chrome
                .borrow()
                .get(*i)
                .map(|p| vec![p.bar.clone().upcast(), p.grip.clone().upcast()])
                .unwrap_or_default(),
        }
    }

    /// Puts every float host, bottom first, each followed by its bar and grip, above every tiled thing,
    /// and every other host back in the host band. Moves only float hosts, hosts that stopped
    /// floating, and float chrome, each with `gtk_widget_insert_after` on the grid itself: a sibling
    /// reorder, which neither unparents nor unrealizes; the editor is never moved. Grows the chrome
    /// pool first when more floats are on screen than it has ever drawn. Runs from `apply` whenever the
    /// set of floats on screen changed, and for the z-order alone after a raise.
    pub(crate) fn restack(&self) {
        let floats = {
            let layout = self.layout();
            let Ok(layout) = layout.try_borrow() else {
                // Nothing restacks inside one of the layout's own writes; a mistake costs one idle.
                self.queue_restack();
                return;
            };
            shown_floats(&layout)
        };
        self.restack_to(floats);
    }

    /// [`ModuleGrid::restack`] with the float band in `order`, bottom first, as the window controller
    /// decided it: the same moves, chrome and pool growth, from the controller's order instead of the
    /// layout read here.
    pub(crate) fn restack_floats(&self, order: &[ModuleId]) {
        debug_assert!(!order.contains(&ModuleId::editor()), "the editor never floats");
        self.restack_to(order.to_vec());
    }

    fn restack_to(&self, floats: Vec<ModuleId>) {
        let imp = self.imp();
        let owners = chrome_owners(&imp.chrome_owners.borrow(), &floats);
        let (make, _) = chrome_plan(imp.chrome.borrow().len(), owners.len());
        for _ in 0..make {
            let piece = self.new_chrome();
            self.insert_in_band(piece.bar.upcast_ref(), Band::FloatBar);
            piece.grip.insert_after(self, Some(&piece.bar));
            imp.chrome.borrow_mut().push(piece);
        }
        let reassigned = *imp.chrome_owners.borrow() != owners;
        *imp.chrome_owners.borrow_mut() = owners.clone();
        let moves = restack_moves(&self.child_slots(), &floats, &owners);
        let changed = make > 0 || reassigned || !moves.is_empty();
        // Every widget is looked up before any moves: a reorder runs no code of ours, but nothing
        // that might should find the pools borrowed.
        let steps: Vec<(Vec<gtk4::Widget>, Option<gtk4::Widget>)> = moves
            .iter()
            .map(|m| {
                let after = m.after.as_ref().and_then(|a| self.slot_widgets(a).last().cloned());
                (self.slot_widgets(&m.slot), after)
            })
            .collect();
        for (widgets, mut after) in steps {
            for widget in widgets {
                widget.insert_after(self, after.as_ref());
                after = Some(widget);
            }
        }
        *imp.float_band.borrow_mut() = floats.into_iter().collect();
        // A restack that found everything in place (the keys moving to the top float, say) costs no
        // allocation; anything else re-draws the chrome on its floats.
        if changed {
            self.queue_allocate();
        }
    }

    /// [`ModuleGrid::restack`] after a float went to the top of the layout's z-order, then the change
    /// hooks: what a raised float covers changed (the chat may be under it now, or out from under the
    /// float it rose above), and the tray's chip and the chat's attention follow it. No apply ran, so
    /// nothing else would tell them. The state file's saver writes nothing for it: it compares floats
    /// as a set.
    pub(crate) fn restacked_after_raise(&self) {
        self.restack();
        self.notify_changed();
    }

    /// Asks for one [`ModuleGrid::restack`] when the main loop is next idle.
    fn queue_restack(&self) {
        if self.imp().restack_queued.replace(true) {
            return;
        }
        let grid = self.downgrade();
        glib::idle_add_local_once(move || {
            if let Some(grid) = grid.upgrade() {
                grid.imp().restack_queued.set(false);
                grid.restack();
            }
        });
    }

    /// A new piece of float chrome, its press and drag wired back to this grid.
    fn new_chrome(&self) -> FloatChrome {
        let press = {
            let grid = self.downgrade();
            Rc::new(move |id: &ModuleId| {
                if let Some(grid) = grid.upgrade() {
                    grid.float_bar_pressed(id);
                }
            })
        };
        let drag = {
            let grid = self.downgrade();
            Rc::new(move |id: &ModuleId, rect: Rect| {
                if let Some(grid) = grid.upgrade() {
                    grid.float_dragged(id, rect);
                }
            })
        };
        FloatChrome::new(&ChromeHooks { press, drag })
    }

    /// A primary press on a float's bar or grip: the float is raised and restacked, then the hooks run
    /// with its module (they give it the keys).
    fn float_bar_pressed(&self, id: &ModuleId) {
        if self.raise_float(id) {
            self.restacked_after_raise();
        }
        // Cloned out: a hook gives the module the keys, which re-enters the grid.
        let hooks: Vec<StripHook> = self.imp().float_bar_press.borrow().clone();
        for hook in hooks {
            hook(id);
        }
    }

    /// Puts the float `id` on top of the z-order (`Layout::raise`). Not a change for the state file: the
    /// saver compares floats as a set. `false` if it is not a float, is on top already, or the layout is
    /// being written.
    fn raise_float(&self, id: &ModuleId) -> bool {
        self.layout().try_borrow_mut().is_ok_and(|mut layout| layout.raise(id))
    }

    /// One step of a drag on a float's bar or grip: `rect` is stored as the float's own (core's
    /// `set_float_rect` fits it into the grid), and the grid is allocated again.
    fn float_dragged(&self, id: &ModuleId, rect: Rect) {
        let size = self.size();
        let moved = {
            let layout = self.layout();
            let Ok(mut layout) = layout.try_borrow_mut() else {
                return;
            };
            self.with_frame(size, |frame| {
                eitri_core::layout::set_float_rect(&mut layout, id, rect, frame)
            })
        };
        if moved {
            self.queue_allocate();
            self.notify_changed();
        }
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

    /// Every module and its host in the layout's tree order, then the floats bottom first: HINT's and
    /// `select.number`'s order.
    pub(crate) fn hosts_in_tree_order(&self) -> Vec<(ModuleId, gtk4::Widget)> {
        let leaves = self.layout().borrow().modules();
        let hosts = self.hosts();
        leaves
            .into_iter()
            .filter_map(|id| hosts.iter().find(|(m, _)| *m == id).cloned())
            .collect()
    }

    /// Makes each host's visibility match the layout, then re-measures and re-allocates: a hide or a
    /// zoom changes what the grid needs at the least (`measure`). Call after every layout change.
    /// The layout is read, and released, before any child's visibility changes: hiding the focused
    /// widget moves GTK focus, whose focus report re-enters the controller and writes the layout.
    ///
    /// **It restacks whenever the set of floats on screen is not the one the last restack put in the
    /// float band** ([`floats_need_restack`]), so every verb that changes which modules float -- float
    /// and sink, the scratchpad, a swap between a float and a tile, a far move that docks, a split that
    /// sinks a float, a kill that reopens one tiled, a workspace switch -- restacks without a list of
    /// them to keep up. A divider drag or a window resize changes no float and never restacks.
    pub(crate) fn apply(&self) {
        let (wanted, floats): (Vec<(gtk4::Widget, bool)>, Vec<ModuleId>) = {
            let layout = self.layout();
            let layout = layout.borrow();
            let wanted = self
                .imp()
                .hosts
                .borrow()
                .iter()
                .filter(|h| !self.probe_places_host(&h.id))
                .map(|h| (h.widget.clone(), layout.is_visible(&h.id)))
                .collect();
            (wanted, shown_floats(&layout))
        };
        for (widget, visible) in wanted {
            if widget.is_child_visible() != visible {
                widget.set_child_visible(visible);
            }
        }
        if floats_need_restack(&self.imp().float_band.borrow(), &floats) {
            self.restack();
        }
        self.queue_resize();
        self.notify_changed();
    }

    /// `f` runs after every change the grid applies itself: each [`ModuleGrid::apply`] (what a verb
    /// still carried out in the shell calls), a divider drag, a float's drag and raise. A pinned split
    /// getting its length inside an allocation is not one: nothing moved on screen. What hangs off it is
    /// the state file's: the window controller re-reads the layout the shell changed behind it. A change
    /// the controller made runs only the chrome hooks, since the controller tells its saver itself.
    /// Runs with the layout released.
    pub(crate) fn connect_changed(&self, f: impl Fn() + 'static) {
        self.imp().hooks.borrow_mut().changed.push(Rc::new(f));
    }

    /// `f` runs after every change to what the layout shows: each one [`ModuleGrid::connect_changed`]
    /// hears, after its hooks, and each change the window controller made (its `ChromeHooks` op). What
    /// hangs off it, all in the window's setup: the top bar's selection and mark texts, the tray (with
    /// telling a chat view a float has left it that it is on screen), and the current workspace's name.
    /// Runs with the layout released.
    pub(crate) fn connect_chrome_changed(&self, f: impl Fn() + 'static) {
        self.imp().hooks.borrow_mut().chrome.push(Rc::new(f));
    }

    fn notify_changed(&self) {
        let hooks = self.imp().hooks.borrow().after_apply();
        for hook in hooks {
            hook();
        }
    }

    /// Runs the chrome hooks alone, for a change the window controller made.
    pub(crate) fn notify_chrome_changed(&self) {
        let hooks = self.imp().hooks.borrow().after_chrome();
        for hook in hooks {
            hook();
        }
    }

    /// Where the grid takes its arrangement and its minimum size from: `allocator`'s answers for `win`,
    /// computed at once inside the allocation and the measure. A pinned split shown for the first time is
    /// settled there, not here. A grid never given one (the companion window's) arranges its own layout.
    pub(crate) fn set_allocator(&self, win: WinId, allocator: Weak<dyn Allocator>) {
        *self.imp().allocator.borrow_mut() = Some((win, allocator));
    }

    /// The allocator while it lives, with the window it answers for.
    fn allocator(&self) -> Option<(WinId, Rc<dyn Allocator>)> {
        let held = self.imp().allocator.borrow().clone();
        held.and_then(|(win, allocator)| Some((win, allocator.upgrade()?)))
    }

    /// Takes every host off screen but those the probe places, before the window's first layout batch: a
    /// host is placed shown, and that batch maps only what the layout shows, as a host made later starts
    /// off screen until it is mapped. Nothing is realized yet, so no host loses the keys or its surface.
    pub(crate) fn take_every_host_off_screen(&self) {
        let widgets: Vec<gtk4::Widget> = self
            .imp()
            .hosts
            .borrow()
            .iter()
            .filter(|h| !self.probe_places_host(&h.id))
            .map(|h| h.widget.clone())
            .collect();
        for widget in widgets {
            widget.set_child_visible(false);
        }
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

    /// The text of a float's bar for its module, applied to every bar on screen now and used for every
    /// one drawn later; always set as plain text. Until it is set a bar shows the module's id.
    pub(crate) fn set_float_label(&self, f: impl Fn(&ModuleId) -> String + 'static) {
        *self.imp().float_label.borrow_mut() = Some(Rc::new(f));
        self.refresh_float_bars();
    }

    /// Names every float bar on screen again with the label function [`ModuleGrid::set_float_label`]
    /// installs (the window gives a module's title), so a new label reaches the bars already drawn.
    /// Nothing is measured or allocated.
    pub(crate) fn refresh_float_bars(&self) {
        let label = self.float_label();
        for piece in self.imp().chrome.borrow().iter() {
            piece.relabel(&*label);
        }
    }

    fn float_label(&self) -> StripLabel {
        self.imp()
            .float_label
            .borrow()
            .clone()
            .unwrap_or_else(|| Rc::new(|id: &ModuleId| id.to_string()))
    }

    /// `f` runs with a float's module after a primary press on its bar or grip has raised it. What it
    /// does -- giving the module the keys -- is the caller's, which knows how the window does that.
    /// Runs with the layout released.
    pub(crate) fn connect_float_bar_press(&self, f: impl Fn(&ModuleId) + 'static) {
        self.imp().float_bar_press.borrow_mut().push(Rc::new(f));
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

    /// Opens `id`, which the layout does not hold yet and whose host the grid does, where `rule` says
    /// (`eitri_core::layout::open_by_rule`) at the grid's size now, `keys` being the module with the
    /// keys. When a tile takes them, its host is mapped and `focus` gives them before `apply` unmaps
    /// what the open hid -- a module in the editor's place hides the editor, and unmapping that first
    /// would leave GTK's focus chain to choose where the keys go ([`leave_then_unmap`]'s order). A float
    /// that takes them is given them after `apply` has mapped and restacked it, as a scratchpad show
    /// is, so they never land under another float. A stowed or unfocused open only applies.
    pub(crate) fn open_module(
        &self,
        id: &ModuleId,
        rule: &OpenRule,
        keys: Option<&ModuleId>,
        focus: &dyn Fn(&ModuleId) -> bool,
    ) -> Result<Opened, LayoutError> {
        let hosted: Vec<ModuleId> = self.hosts().into_iter().map(|(id, _)| id).collect();
        placeable(&hosted, id)?;
        let size = self.size();
        let opened = eitri_core::layout::open_by_rule(&mut self.layout().borrow_mut(), id, rule, keys, size)?;
        let floats = self.layout().borrow().is_floating(id);
        if opened.takes_keys && !floats {
            self.map_arriving(id);
            focus(id);
        }
        self.apply();
        if opened.takes_keys && floats {
            focus(id);
        }
        Ok(opened)
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
        if let Some((win, allocator)) = self.allocator() {
            return allocator.min_size(win);
        }
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

    /// Puts back a zoom something else ended, around `keeps` (`Layout::restore_zoom`). `false`, and
    /// nothing redrawn, when the layout refuses it.
    pub(crate) fn restore_zoom(&self, zoom: &Zoom, keeps: &ModuleId) -> bool {
        let restored = self.layout().borrow_mut().restore_zoom(zoom, keeps);
        if restored {
            println!("[module_grid] zoom {zoom:?} put back around {keeps}");
            self.apply();
        }
        restored
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

    /// `float.toggle` (`eitri_core::layout::float_toggle`): floats a tile, or sinks a float beside the
    /// most recent tile in view. `apply` restacks, since the set of floats changed.
    pub(crate) fn float_toggle(&self, target: &ModuleId) -> Result<FloatChange, LayoutError> {
        let size = self.size();
        let change = {
            let layout = self.layout();
            let mut layout = layout.borrow_mut();
            self.with_frame(size, |frame| {
                eitri_core::layout::float_toggle(&mut layout, target, frame)
            })
        }?;
        self.apply();
        Ok(change)
    }

    /// `move.<dir>` on a float (`eitri_core::layout::move_float`): `px` that way, kept inside the grid.
    /// `false` when it could not move.
    pub(crate) fn move_float(&self, target: &ModuleId, dir: Direction, px: i32) -> bool {
        let size = self.size();
        let moved = {
            let layout = self.layout();
            let mut layout = layout.borrow_mut();
            self.with_frame(size, |frame| {
                eitri_core::layout::move_float(&mut layout, target, dir, px, frame)
            })
        };
        if moved {
            self.apply();
        }
        moved
    }

    /// `eitri.layout.float` (`eitri_core::layout::float_at`): floats `id` at `rect` (its remembered or
    /// the default rectangle when `None`), or moves a float there. `Ok(false)` when nothing changed.
    pub(crate) fn float_at(&self, id: &ModuleId, rect: Option<FloatRect>) -> Result<bool, LayoutError> {
        let size = self.size();
        let changed = {
            let layout = self.layout();
            let mut layout = layout.borrow_mut();
            self.with_frame(size, |frame| eitri_core::layout::float_at(&mut layout, id, rect, frame))
        }?;
        if changed {
            self.apply();
        }
        Ok(changed)
    }

    /// Stows `target` in [`leave_then_unmap`]'s order: the keys to the module the layout chooses, then
    /// the unmap (`eitri_core::layout::scratchpad_move`), then the restack.
    pub(crate) fn scratchpad_move(
        &self,
        target: &ModuleId,
        focus: &dyn Fn(&ModuleId) -> bool,
    ) -> Result<(), LayoutError> {
        let size = self.size();
        let layout = self.layout();
        self.with_frame(size, |frame| {
            leave_then_unmap(
                &layout,
                |layout| eitri_core::layout::scratchpad_move(layout, target, frame),
                |next| self.map_arriving(next),
                |next| {
                    focus(next);
                },
                || self.apply(),
            )
        })?;
        self.restack();
        Ok(())
    }

    /// `scratchpad.show` (`eitri_core::layout::scratchpad_show`), in [`scratch_show_in_order`]'s order:
    /// `Stowed` in [`leave_then_unmap`]'s, then the restack; `Raised`/`Shown`: mapped, restacked, then
    /// given the keys through `focus`. `Missing` changes nothing: the caller makes the instance.
    pub(crate) fn scratchpad_show(
        &self,
        module: Option<&ModuleId>,
        focus: &dyn Fn(&ModuleId) -> bool,
    ) -> Result<ScratchShow, LayoutError> {
        let size = self.size();
        let layout = self.layout();
        self.with_frame(size, |frame| {
            scratch_show_in_order(
                &layout,
                |layout| eitri_core::layout::scratchpad_show(layout, module, frame),
                |id| self.map_arriving(id),
                |id| {
                    focus(id);
                },
                || self.apply(),
                || self.restack(),
            )
        })
    }

    /// The floats over `id` on a fresh arrangement at the grid's size now -- never a stale allocation's
    /// -- bottom first (`eitri_core::layout::covering`). Empty for a module off screen.
    pub(crate) fn covered_now(&self, id: &ModuleId) -> Vec<ModuleId> {
        let size = self.size();
        let layout = self.layout();
        let layout = layout.borrow();
        let arrangement = self.with_frame(size, |frame| arrange(&layout, frame));
        covering(&arrangement, id)
    }

    /// Makes `to` the current workspace in [`switch_then_unmap`]'s order: `Workspaces::switch` swaps
    /// the live layout for `to`'s and names the module that takes the keys there, that module's host is
    /// mapped and given them through `focus`, `apply` unmaps what `to` does not show (and tells the
    /// change hooks: the saver, the tray, the mark, the top bar), and `restack` puts `to`'s floats in
    /// their band in its own z-order -- the same set of floats in another order is still a restack.
    /// Unmapping first would leave the keys on a host leaving the screen, and GTK's focus chain would
    /// choose where they went. `Ok(false)` when `to` is current already, and nothing moves. `lua` is the
    /// window's Lua declarations: where the keys go when `to`'s own last module is not on screen.
    pub(crate) fn switch_workspace(
        &self,
        workspaces: &RefCell<Workspaces>,
        to: &str,
        lua: &[ModuleDecl],
        focus: &dyn Fn(&ModuleId) -> bool,
    ) -> Result<bool, WorkspaceError> {
        {
            let workspaces = workspaces.borrow();
            if !workspaces.order().iter().any(|name| name == to) {
                return Err(WorkspaceError::NoSuch(to.to_string()));
            }
            if workspaces.current() == to {
                return Ok(false);
            }
        }
        let layout = self.layout();
        switch_then_unmap(
            || workspaces.borrow_mut().switch(&mut layout.borrow_mut(), to, lua),
            |next| self.map_arriving(next),
            |next| {
                focus(next);
            },
            || self.apply(),
            || self.restack(),
        )?;
        Ok(true)
    }

    /// Runs `f` with the frame the layout is allocated in now (a stored workspace's verbs need one).
    pub(crate) fn with_current_frame<R>(&self, f: impl FnOnce(&Frame) -> R) -> R {
        self.with_frame(self.size(), f)
    }

    fn allocate_children(&self, width: i32, height: i32) {
        let imp = self.imp();
        let size = Size { w: width, h: height };
        let allocator = self.allocator();
        let arrangement = arrangement_for(
            allocator.as_ref().map(|(_, allocator)| allocator.as_ref()),
            allocator.as_ref().map_or(WinId::MAIN, |(win, _)| *win),
            &self.layout(),
            size,
            |f| self.with_frame(size, |frame| f(frame)),
        );
        let was_on_screen = imp.on_screen.replace(
            arrangement
                .modules
                .iter()
                .map(|(id, _)| id.clone())
                .chain(arrangement.floats.iter().map(|spot| spot.id.clone()))
                .collect(),
        );
        let mut clips = Vec::new();
        let mut held = false;
        let mut restart_quiet = false;
        // The tiled hosts first, then each float's host below its bar, with the same throttle for a
        // web host either way: a dragged web float is held at its settled size as a resized tile is.
        let tiled = arrangement.modules.iter().map(|(id, rect)| (id, *rect));
        let floating = arrangement.floats.iter().map(|spot| (&spot.id, spot.host));
        for (id, target) in tiled.chain(floating) {
            if self.probe_places_host(id) {
                continue;
            }
            let hosts = imp.hosts.borrow();
            let Some(host) = hosts.iter().find(|h| h.id == *id) else {
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
        self.place_float_chrome(&arrangement);
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

    /// Draws each float on screen with the piece of chrome that owns it ([`floatbars::chrome_owners`]):
    /// its bar across the float's top strip row, its grip in the bottom-right corner. Every other piece
    /// is hidden, and so is every piece while a zoom is on (a zoomed float fills the grid alone, with no
    /// bar). A float with no piece of its own yet -- the layout changed without an `apply` -- has none
    /// this pass and asks for a restack, which makes one.
    fn place_float_chrome(&self, arrangement: &Arrangement) {
        let imp = self.imp();
        let owners = imp.chrome_owners.borrow().clone();
        let label = self.float_label();
        for (i, piece) in imp.chrome.borrow().iter().enumerate() {
            let Some(spot) = owners.get(i).and_then(|id| arrangement.float_spot(id)) else {
                piece.hide();
                continue;
            };
            piece.show(spot, &*label);
            for (widget, r) in [
                (piece.bar.upcast_ref::<gtk4::Widget>(), spot.bar),
                (piece.grip.upcast_ref::<gtk4::Widget>(), grip_rect(spot.rect)),
            ] {
                let _ = widget.measure(gtk4::Orientation::Horizontal, -1);
                let _ = widget.measure(gtk4::Orientation::Vertical, r.w);
                widget.size_allocate(&gtk4::Allocation::new(r.x, r.y, r.w, r.h), -1);
            }
        }
        if arrangement.floats.iter().any(|spot| !owners.contains(&spot.id)) {
            self.queue_restack();
        }
    }

    /// Each visible divider as the probe finds it: its axis, its rectangle in the grid, and the handle
    /// widget that takes a press on it.
    #[cfg(feature = "probe")]
    pub(crate) fn probe_dividers(&self) -> Vec<(Axis, Rect, gtk4::Widget)> {
        self.imp()
            .handles
            .borrow()
            .iter()
            .filter_map(|h| {
                let divider = h.current.borrow().clone()?;
                Some((divider.axis, divider.rect, h.widget.clone().upcast()))
            })
            .collect()
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
            let r = handle_allocation(divider);
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

/// The window controller's way into the grid, held weakly as the GTK host holds its windows: a grid gone
/// with its window answers as one never allocated and does nothing.
impl GridPort for glib::WeakRef<ModuleGrid> {
    fn size(&self) -> Size {
        self.upgrade()
            .map_or(eitri_core::wm::grid::UNALLOCATED, |grid| grid.size())
    }
    fn handle_px(&self) -> i32 {
        self.upgrade().map_or(1, |grid| grid.handle_px())
    }
    fn strip_px(&self) -> i32 {
        self.upgrade().map_or(DEFAULT_STRIP_PX, |grid| grid.strip_px())
    }
    fn module_min(&self, module: &ModuleId) -> Size {
        self.upgrade().map(|grid| grid.min_size(module)).unwrap_or_default()
    }
    fn queue_allocation(&self) {
        if let Some(grid) = self.upgrade() {
            grid.queue_resize();
        }
    }
    fn restack_floats(&self, order: &[ModuleId]) {
        if let Some(grid) = self.upgrade() {
            grid.restack_floats(order);
        }
    }
    fn notify_chrome_changed(&self) {
        if let Some(grid) = self.upgrade() {
            grid.notify_chrome_changed();
        }
    }
    fn probe_places(&self, module: &ModuleId) -> bool {
        self.upgrade().is_some_and(|grid| grid.probe_places_host(module))
    }
}

/// The arrangement the grid allocates at `size`: its allocator's, which settles the pinned splits shown for
/// the first time itself, or, for a grid with none, `layout`'s own in the frame `with_frame` gives, a pinned
/// split shown for the first time first given the length its ratio gives at this, the real allocation --
/// never at `UNALLOCATED`'s stand-in size. Asked first, with no geometry, so a drag or a resize sweep arranges
/// once per allocation.
pub(crate) fn arrangement_for(
    allocator: Option<&dyn Allocator>,
    win: WinId,
    layout: &RefCell<Layout>,
    size: Size,
    with_frame: impl Fn(&mut dyn FnMut(&Frame)),
) -> Rc<Arrangement> {
    if let Some(allocator) = allocator {
        return allocator.allocate(win, size);
    }
    if size.w > 0 && size.h > 0 && layout.try_borrow().is_ok_and(|layout| layout.has_pin_to_settle()) {
        if let Ok(mut layout) = layout.try_borrow_mut() {
            with_frame(&mut |frame| {
                eitri_core::layout::settle_pins(&mut layout, frame);
            });
        }
    }
    let layout = layout.borrow();
    let mut arrangement = Arrangement::default();
    with_frame(&mut |frame| arrangement = arrange(&layout, frame));
    Rc::new(arrangement)
}

/// How far a divider's handle reaches past the line it draws, on each side. A divider is one pixel
/// thick, and a pointer a pixel or two off it would land on the module beside it (a terminal then
/// starts a selection instead of the divider moving). The handle sits above the hosts in the child
/// list ([`band_insert_index`]), so the pixels it covers beyond the line are its own.
pub(crate) const DIVIDER_REACH_PX: i32 = 2;

/// The rectangle a divider's handle is allocated: the divider's own, widened by [`DIVIDER_REACH_PX`]
/// across its thickness, for either orientation alike. Only the middle of it is painted
/// (`theme::gtk_css`); the layout's own rectangle for the divider is unchanged.
fn handle_allocation(divider: &Divider) -> Rect {
    let r = divider.rect;
    match divider.axis {
        Axis::Row => Rect {
            x: r.x - DIVIDER_REACH_PX,
            w: r.w + 2 * DIVIDER_REACH_PX,
            ..r
        },
        Axis::Column => Rect {
            y: r.y - DIVIDER_REACH_PX,
            h: r.h + 2 * DIVIDER_REACH_PX,
            ..r
        },
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

/// [`ModuleGrid::switch_workspace`]'s order, with its GTK effects passed in so that a test without a
/// display can see it: [`leave_then_unmap`]'s, with the switch as the leave and one restack after the
/// unmap. `leave` swaps the layouts and names who gets the keys, borrowing and releasing what it
/// writes; `map` puts that module's host on screen, `focus` gives it the keys, `unmap` (`apply`) takes
/// off screen what the arriving workspace does not show, and `restack` orders its floats.
fn switch_then_unmap<E>(
    leave: impl FnOnce() -> Result<Option<ModuleId>, E>,
    map: impl FnOnce(&ModuleId),
    focus: impl FnOnce(&ModuleId),
    unmap: impl FnOnce(),
    restack: impl FnOnce(),
) -> Result<(), E> {
    let next = leave()?;
    if let Some(next) = next {
        map(&next);
        focus(&next);
    }
    unmap();
    restack();
    Ok(())
}

/// [`ModuleGrid::scratchpad_show`]'s order, with its GTK effects passed in so that a test without a
/// display can see it. `show` is core's verb. A stow (`Stowed`) leaves the screen in
/// [`leave_then_unmap`]'s order -- the keys' next owner mapped and given them, then `apply` unmaps the
/// stowed host -- and then the float band is restacked. A raise or a show (`Raised`/`Shown`) is the
/// other way round: `apply` maps the shown host, the restack puts it on top, and only then is it given
/// the keys, so they never land on a host still unmapped or under another float. `Missing` changed
/// nothing, and nothing runs.
fn scratch_show_in_order(
    layout: &RefCell<Layout>,
    show: impl FnOnce(&mut Layout) -> Result<ScratchShow, LayoutError>,
    map: impl Fn(&ModuleId),
    focus: impl Fn(&ModuleId),
    apply: impl Fn(),
    restack: impl FnOnce(),
) -> Result<ScratchShow, LayoutError> {
    let outcome: RefCell<Option<ScratchShow>> = RefCell::new(None);
    leave_then_unmap(
        layout,
        |layout| {
            let shown = show(layout)?;
            let next = match &shown {
                ScratchShow::Stowed(next) => next.clone(),
                _ => None,
            };
            *outcome.borrow_mut() = Some(shown);
            Ok(next)
        },
        &map,
        &focus,
        || {
            if !matches!(*outcome.borrow(), Some(ScratchShow::Missing(_))) {
                apply();
            }
        },
    )?;
    let shown = outcome.into_inner().expect("core's verb ran and answered");
    match &shown {
        ScratchShow::Stowed(_) => restack(),
        ScratchShow::Raised(id) | ScratchShow::Shown(id) => {
            restack();
            map(id);
            focus(id);
        }
        ScratchShow::Missing(_) => {}
    }
    Ok(shown)
}

/// [`ModuleGrid::focus_target`]'s choice, made on the grid's own host records (generic over the
/// widget so it is tested without a display): the focus target recorded for `id`, never its host.
fn focus_target_of<'a, W>(hosts: &'a [Host<W>], id: &ModuleId) -> Option<&'a W> {
    hosts.iter().find(|h| h.id == *id).map(|h| &h.focus)
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

/// Where a dragged bar (`grip: false`) or grip puts the float: the float's whole rectangle as allocated
/// **now** plus the drag's offset -- moved by a bar, resized from the bottom-right by a grip. The offset
/// is in the moving widget's own coordinates, and the bar or grip moves under the pointer as the drag
/// re-allocates it, so it is how far the pointer is from where that widget now is (as
/// [`dragged_first_px`] explains for a divider). Core's `set_float_rect` then fits the result into the
/// grid and above the float's minimum.
fn dragged_float_rect(now: Rect, dx: f64, dy: f64, grip: bool) -> Rect {
    let (dx, dy) = (dx.round() as i32, dy.round() as i32);
    if grip {
        Rect {
            w: now.w.saturating_add(dx),
            h: now.h.saturating_add(dy),
            ..now
        }
    } else {
        Rect {
            x: now.x.saturating_add(dx),
            y: now.y.saturating_add(dy),
            ..now
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eitri_core::layout::{Arrangement, Node};

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
                Child::Handle(n) => arrangement
                    .dividers
                    .get(*n)
                    .is_some_and(|d| contains(handle_allocation(d))),
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

        // The default window: the handle covers the divider's line and DIVIDER_REACH_PX pixels either
        // side of it, and the module beside it owns everything past that.
        let (layout, ids) = layout_and_add_order(&[], false);
        let arrangement = arrange(&layout, &frame);
        let r = arrangement.dividers[0].rect;
        assert_eq!(r.x, 760, "the default divider, as measured");
        let children = stack_as_add_builds(&ids);
        assert_eq!(
            pick(&children, &arrangement, r.x - DIVIDER_REACH_PX - 1, 300),
            Some(Child::Host(ModuleId::editor()))
        );
        assert_eq!(
            pick(&children, &arrangement, r.x + r.w + DIVIDER_REACH_PX + 1, 300),
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

    /// A pointer one pixel off the line of any divider, of either orientation, still lands on the
    /// divider's handle and never on a module: a terminal under a horizontal divider starts a text
    /// selection from a press that missed the one-pixel line, where the vertical divider between two
    /// columns takes the same press.
    #[test]
    fn a_press_within_a_pixel_of_a_divider_line_reaches_its_handle_for_both_orientations() {
        use eitri_core::layout::Placement::{BelowRoot, InPlaceOfEditor, RightOfRoot};
        let layouts: &[&[(&str, eitri_core::layout::Placement)]] = &[
            &[],
            &[("side1", RightOfRoot)],
            &[("bottom1", BelowRoot)],
            &[
                ("main1", InPlaceOfEditor),
                ("side1", RightOfRoot),
                ("bottom1", BelowRoot),
            ],
        ];
        let frame = Frame::new(UNALLOCATED, 1);
        let mut orientations = BTreeSet::new();
        for (panels, terminal_shown) in layouts.iter().flat_map(|p| [(p, false), (p, true)]) {
            let (layout, ids) = layout_and_add_order(panels, terminal_shown);
            let arrangement = arrange(&layout, &frame);
            let children = stack_as_add_builds(&ids);
            for (n, divider) in arrangement.dividers.iter().enumerate() {
                let r = divider.rect;
                orientations.insert(format!("{:?}", divider.axis));
                for off in [-1, 0, 1] {
                    // Along the line's middle, away from where another divider meets it.
                    let (x, y) = match divider.axis {
                        Axis::Row => (r.x + r.w / 2 + off, r.y + r.h / 2),
                        Axis::Column => (r.x + r.w / 2, r.y + r.h / 2 + off),
                    };
                    assert_eq!(
                        pick(&children, &arrangement, x, y),
                        Some(Child::Handle(n)),
                        "{panels:?} (terminal shown: {terminal_shown}): a press {off} px off the line of \
                         divider {n} ({:?}) at ({x}, {y})",
                        divider.axis
                    );
                }
            }
        }
        assert_eq!(
            orientations.len(),
            2,
            "the layouts must give a divider of each orientation"
        );
    }

    /// The widened handle changes nothing the layout sees: the allocation is the divider's rectangle
    /// plus the reach across its thickness only, centred on it.
    #[test]
    fn a_handle_is_the_divider_widened_across_its_thickness_only() {
        let (layout, _) = layout_and_add_order(&[], false);
        let any = arrange(&layout, &Frame::new(UNALLOCATED, 1)).dividers[0].clone();
        let row = Divider {
            axis: Axis::Row,
            rect: Rect {
                x: 100,
                y: 200,
                w: 1,
                h: 300,
            },
            ..any
        };
        assert_eq!(
            handle_allocation(&row),
            Rect {
                x: 100 - DIVIDER_REACH_PX,
                y: 200,
                w: 1 + 2 * DIVIDER_REACH_PX,
                h: 300
            }
        );
        let column = Divider {
            axis: Axis::Column,
            rect: Rect {
                x: 10,
                y: 50,
                w: 400,
                h: 1,
            },
            ..row
        };
        assert_eq!(
            handle_allocation(&column),
            Rect {
                x: 10,
                y: 50 - DIVIDER_REACH_PX,
                w: 400,
                h: 1 + 2 * DIVIDER_REACH_PX
            }
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
        /// The float band was put in the arriving workspace's order.
        Restack,
    }

    /// Runs `leave` on `layout` through [`leave_then_unmap`] and records each effect as it runs, with
    /// the grid's mapped hosts modelled as the set of modules on screen before `leave`. Each effect
    /// checks what the real one relies on: the layout is not borrowed (mapping reads it, a focus
    /// change reaches the focus report, which writes it, and `apply` reads it). `map` records a map only
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
        let mapped: RefCell<BTreeSet<ModuleId>> = RefCell::new(layout.borrow().visible_modules().into_iter().collect());
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

    // ---- Floats in the grid ----

    /// `children` after `moves`, each applied as `gtk_widget_insert_after` does: the slot leaves its
    /// place and goes right after `after` (first when `None`).
    fn apply_moves(children: &[Slot], moves: &[SlotMove]) -> Vec<Slot> {
        let mut now = children.to_vec();
        for m in moves {
            let at = now.iter().position(|s| *s == m.slot).expect("a move names a child");
            now.remove(at);
            let to = match &m.after {
                None => 0,
                Some(a) => now.iter().position(|s| s == a).expect("after names a child") + 1,
            };
            now.insert(to, m.slot.clone());
        }
        now
    }

    /// The pool's plan: chrome is made only when more floats are on screen than the pool has ever
    /// drawn, the rest is hidden and reused, and nothing is ever unparented (the pool never shrinks).
    /// A raise keeps every float on its own piece, so a bar being dragged keeps drawing its float.
    #[test]
    fn float_chrome_grows_with_the_floats_and_idles_what_it_does_not_use() {
        assert_eq!(chrome_plan(0, 2), (2, 2..2), "the first two floats");
        assert_eq!(chrome_plan(2, 1), (0, 1..2), "a stow or a sink: one spot fewer");
        assert_eq!(chrome_plan(2, 2), (0, 2..2), "back to two: slot 1 reused");
        assert_eq!(chrome_plan(2, 0), (0, 0..2), "none: all hidden, none dropped");
        assert_eq!(chrome_plan(1, 3), (2, 3..3), "only the missing ones are made");
        for pool in 0..5 {
            for spots in 0..5 {
                let (make, hide) = chrome_plan(pool, spots);
                assert_eq!(pool + make, pool.max(spots), "the pool never shrinks");
                assert_eq!(hide, spots..pool + make, "every slot past the floats is hidden");
            }
        }

        let (a, b, c) = (ModuleId::agent(), ModuleId::terminal(), ModuleId::lua("c"));
        let drew = chrome_owners(&[], &[a.clone(), b.clone()]);
        assert_eq!(drew, [a.clone(), b.clone()]);
        assert_eq!(
            chrome_owners(&drew, &[b.clone(), a.clone()]),
            drew,
            "a raise moves no float to another piece"
        );
        assert_eq!(
            chrome_owners(&drew, std::slice::from_ref(&b)),
            std::slice::from_ref(&b),
            "a float leaving frees its slot"
        );
        assert_eq!(
            chrome_owners(&drew, &[c.clone(), b.clone(), a.clone()]),
            [a.clone(), b.clone(), c.clone()],
            "a new float takes the next slot"
        );
        assert_eq!(
            chrome_owners(std::slice::from_ref(&b), std::slice::from_ref(&a)),
            std::slice::from_ref(&a),
            "a swap hands the slot the old float freed to the new one"
        );
    }

    /// A switch away from a workspace whose keys are on a float the arriving workspace does not show:
    /// the arriving keys' host is mapped and given them before `apply` unmaps the float, and the band
    /// is restacked last. Each effect checks what its real one relies on, as [`leave_and_record`]'s do:
    /// both cells released, the float off screen in the layout before the keys move, the keys' target
    /// shown and its host mapped. With the focus and the unmap swapped, GTK's focus chain would choose
    /// where the keys went from a float leaving the screen.
    #[test]
    fn a_workspace_switch_gives_the_keys_before_it_unmaps_a_float_that_held_them() {
        use std::collections::BTreeMap;
        let (agent, terminal) = (ModuleId::agent(), ModuleId::terminal());
        let frame = Frame::new(UNALLOCATED, 1);
        // `main`: the terminal floats and has the keys, the chat is hidden.
        let mut main = with_a_float(FloatRect::centred(UNALLOCATED, 0.6), &terminal);
        eitri_core::layout::hide(&mut main, &agent, &frame).unwrap();
        main.set_focus(&terminal).unwrap();
        assert!(
            !main.is_visible(&agent),
            "the premise: the chat's host is off screen in main"
        );
        // `study`: the first launch's layout, the terminal hidden, the keys on the chat.
        let mut study = crate::terminal::initial_layout(&[]).unwrap();
        study.set_focus(&agent).unwrap();
        assert!(
            !study.is_visible(&terminal),
            "the premise: study does not show the terminal"
        );
        let workspaces = RefCell::new(
            Workspaces::from_parts(
                vec!["main".to_string(), "study".to_string()],
                BTreeMap::from([("study".to_string(), study)]),
                "main".to_string(),
                Default::default(),
            )
            .unwrap(),
        );
        let layout = RefCell::new(main);
        let mapped: RefCell<BTreeSet<ModuleId>> = RefCell::new(layout.borrow().visible_modules().into_iter().collect());
        let effects = RefCell::new(Vec::new());
        let released = |what: &str| {
            assert!(layout.try_borrow_mut().is_ok(), "the layout is released before {what}");
            assert!(
                workspaces.try_borrow_mut().is_ok(),
                "the workspaces are released before {what}"
            );
        };

        let result = switch_then_unmap(
            || {
                let next = workspaces.borrow_mut().switch(&mut layout.borrow_mut(), "study", &[]);
                effects.borrow_mut().push(Effect::Leave);
                next
            },
            |next| {
                released("a host maps");
                if layout.borrow().is_visible(next) && mapped.borrow_mut().insert(next.clone()) {
                    effects.borrow_mut().push(Effect::Map(next.clone()));
                }
            },
            |next| {
                released("the keys move");
                let l = layout.borrow();
                assert!(!l.is_visible(&terminal), "the float is off screen in the layout first");
                assert!(l.is_shown(next), "the keys go to a shown module, {next}");
                assert!(
                    mapped.borrow().contains(next),
                    "{next}'s host is mapped before it gets the keys"
                );
                effects.borrow_mut().push(Effect::Focus(next.clone()));
            },
            || {
                released("the unmap");
                assert!(!layout.borrow().is_visible(&terminal), "apply unmaps the float");
                effects.borrow_mut().push(Effect::Unmap);
            },
            || {
                released("the restack");
                assert!(shown_floats(&layout.borrow()).is_empty(), "study shows no float");
                effects.borrow_mut().push(Effect::Restack);
            },
        );
        assert_eq!(result, Ok(()));
        assert_eq!(
            effects.into_inner(),
            [
                Effect::Leave,
                Effect::Map(agent.clone()),
                Effect::Focus(agent.clone()),
                Effect::Unmap,
                Effect::Restack,
            ]
        );
        let workspaces = workspaces.borrow();
        assert_eq!(workspaces.current(), "study");
        assert_eq!(layout.borrow().focus(), &agent);
        let left = workspaces.stored("main").unwrap();
        assert!(left.is_floating(&terminal), "main keeps its float for the way back");
        assert_eq!(left.focus(), &terminal, "and its keys");
    }

    /// Floats `terminal` at `rect` over the default window with the terminal shown and the keys on
    /// `keys`.
    fn with_a_float(rect: FloatRect, keys: &ModuleId) -> Layout {
        let mut layout = crate::terminal::initial_layout(&[]).unwrap();
        layout.show(&ModuleId::terminal()).unwrap();
        let frame = Frame::new(UNALLOCATED, 1);
        assert_eq!(
            eitri_core::layout::float_at(&mut layout, &ModuleId::terminal(), Some(rect), &frame),
            Ok(true)
        );
        layout.set_focus(keys).unwrap();
        layout
    }

    /// A drag on a float's bar keeps the bar under the pointer, and a drag on its grip keeps the grip
    /// there, both ways. This replays what GTK hands `drag-update` -- the pointer in the bar's (or the
    /// grip's) CURRENT coordinates, less where it was grabbed -- one motion event per frame, the grid
    /// re-arranging between events as it does.
    #[test]
    fn a_dragged_float_bar_stays_under_the_pointer() {
        let terminal = ModuleId::terminal();
        let frame = Frame::new(UNALLOCATED, 1);
        let rect_now = |layout: &Layout| arrange(layout, &frame).float_spot(&terminal).unwrap().rect;
        let float = FloatRect {
            cx: 0.5,
            cy: 0.5,
            w: 400,
            h: 300,
        };
        for (grip, step) in [(false, (15, 7)), (false, (-10, -5)), (true, (10, 6)), (true, (-8, -4))] {
            let mut layout = with_a_float(float, &terminal);
            let start = rect_now(&layout);
            // Where on the bar or the grip the press landed, in its own coordinates.
            let grabbed = (5, 5);
            let handle = |r: Rect| if grip { grip_rect(r) } else { r };
            let origin = handle(start);
            let pointer_start = (origin.x + grabbed.0, origin.y + grabbed.1);
            for n in 1..=10 {
                let pointer = (pointer_start.0 + step.0 * n, pointer_start.1 + step.1 * n);
                let now = rect_now(&layout);
                let widget = handle(now);
                let dx = f64::from(pointer.0 - widget.x - grabbed.0);
                let dy = f64::from(pointer.1 - widget.y - grabbed.1);
                let next = dragged_float_rect(now, dx, dy, grip);
                eitri_core::layout::set_float_rect(&mut layout, &terminal, next, &frame);
            }
            let end = rect_now(&layout);
            let (mx, my) = (step.0 * 10, step.1 * 10);
            if grip {
                assert_eq!(
                    end,
                    Rect {
                        w: start.w + mx,
                        h: start.h + my,
                        ..start
                    },
                    "a grip drag of {mx},{my}"
                );
            } else {
                assert_eq!(
                    end,
                    Rect {
                        x: start.x + mx,
                        y: start.y + my,
                        ..start
                    },
                    "a bar drag of {mx},{my}"
                );
            }
        }
        // The rule on its own: a bar moves the rectangle, a grip sizes it from the bottom-right.
        let r = Rect {
            x: 100,
            y: 50,
            w: 300,
            h: 200,
        };
        assert_eq!(dragged_float_rect(r, -20.4, 9.6, false), Rect { x: 80, y: 60, ..r });
        assert_eq!(dragged_float_rect(r, -20.4, 9.6, true), Rect { w: 280, h: 210, ..r });
    }

    /// What GTK's pick finds over a grid with floats, as [`pick`] models it for hosts and handles.
    #[derive(Debug, PartialEq)]
    enum Picked {
        Host(ModuleId),
        Handle(usize),
        Bar(ModuleId),
        Grip(ModuleId),
    }

    fn pick_slot(children: &[Slot], arrangement: &Arrangement, chrome: &[ModuleId], x: i32, y: i32) -> Option<Picked> {
        let contains = |r: Rect| x >= r.x && x <= r.x + r.w && y >= r.y && y <= r.y + r.h;
        children.iter().rev().find_map(|slot| match slot {
            Slot::Host(id) => arrangement
                .rect_of(id)
                .or_else(|| arrangement.float_spot(id).map(|s| s.host))
                .is_some_and(contains)
                .then(|| Picked::Host(id.clone())),
            Slot::Handle(n) => arrangement
                .dividers
                .get(*n)
                .is_some_and(|d| contains(d.rect))
                .then_some(Picked::Handle(*n)),
            Slot::Strip(_) => None,
            Slot::Chrome(i) => {
                let spot = chrome.get(*i).and_then(|id| arrangement.float_spot(id))?;
                // The grip is after the bar in the child list: it is picked first.
                if contains(grip_rect(spot.rect)) {
                    Some(Picked::Grip(spot.id.clone()))
                } else if contains(spot.bar) {
                    Some(Picked::Bar(spot.id.clone()))
                } else {
                    None
                }
            }
        })
    }

    /// A float over the root divider takes every press inside it: on the divider's pixels its host
    /// does, on its bar the bar, on its grip the grip; outside it the divider is still the handle's.
    #[test]
    fn a_press_inside_a_float_reaches_its_host_not_the_split_below() {
        let terminal = ModuleId::terminal();
        let layout = with_a_float(
            FloatRect {
                cx: 0.6,
                cy: 0.5,
                w: 400,
                h: 300,
            },
            &terminal,
        );
        let arrangement = arrange(&layout, &Frame::new(UNALLOCATED, 1));
        let spot = arrangement.float_spot(&terminal).unwrap().clone();
        let divider = arrangement.dividers[0].clone();
        assert_eq!(divider.axis, Axis::Row, "the premise: the editor | agent split");
        assert!(
            spot.host.x < divider.rect.x && divider.rect.x < spot.host.x + spot.host.w,
            "the premise: the float's host covers the divider ({spot:?}, {divider:?})"
        );

        let ids = [ModuleId::editor(), ModuleId::agent(), terminal.clone()];
        let mut children: Vec<Slot> = stack_as_add_builds(&ids)
            .into_iter()
            .map(|c| match c {
                Child::Host(id) => Slot::Host(id),
                Child::Handle(n) => Slot::Handle(n),
            })
            .collect();
        let floats = [terminal.clone()];
        let chrome = chrome_owners(&[], &floats);
        children.push(Slot::Chrome(0));
        let children = apply_moves(&children, &restack_moves(&children, &floats, &chrome));

        let r = divider.rect;
        let middle = spot.host.y + spot.host.h / 2;
        for x in [r.x, r.x + r.w] {
            assert_eq!(
                pick_slot(&children, &arrangement, &chrome, x, middle),
                Some(Picked::Host(terminal.clone())),
                "a press at ({x}, {middle}) on the divider under the float"
            );
        }
        let above = spot.rect.y - 10;
        assert_eq!(
            pick_slot(&children, &arrangement, &chrome, r.x, above),
            Some(Picked::Handle(0)),
            "the divider outside the float is the handle's"
        );
        let bar = (spot.bar.x + spot.bar.w / 2, spot.bar.y + spot.bar.h / 2);
        assert_eq!(
            pick_slot(&children, &arrangement, &chrome, bar.0, bar.1),
            Some(Picked::Bar(terminal.clone()))
        );
        let grip = grip_rect(spot.rect);
        assert_eq!(
            pick_slot(&children, &arrangement, &chrome, grip.x + 6, grip.y + 6),
            Some(Picked::Grip(terminal.clone()))
        );

        // Negative control: the same float left in the host band, where it was tiled, loses the
        // divider's pixels to the handle above it.
        let unstacked = stack_as_add_builds(&ids)
            .into_iter()
            .map(|c| match c {
                Child::Host(id) => Slot::Host(id),
                Child::Handle(n) => Slot::Handle(n),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            pick_slot(&unstacked, &arrangement, &chrome, r.x, middle),
            Some(Picked::Handle(0))
        );
    }

    /// Stowing the float that holds the keys -- `scratchpad.move`, or `scratchpad.show` on the shown
    /// scratch float with the keys -- gives them to the most recent module in view before the float's
    /// host is unmapped, through [`leave_then_unmap`], the door `ModuleGrid::scratchpad_move` and
    /// `scratchpad_show`'s `Stowed` take. Both tiled modules are tried as the one with the keys last.
    #[test]
    fn stowing_the_float_with_the_keys_gives_them_away_before_it_unmaps() {
        let terminal = ModuleId::terminal();
        let frame = Frame::new(UNALLOCATED, 1);
        let float = FloatRect::centred(UNALLOCATED, 0.6);
        for last in [ModuleId::agent(), ModuleId::editor()] {
            let mut layout = with_a_float(float, &last);
            layout.set_focus(&terminal).unwrap();
            let layout = RefCell::new(layout);
            let (result, effects) = leave_and_record(&layout, &terminal, |l| {
                eitri_core::layout::scratchpad_move(l, &terminal, &frame)
            });
            assert_eq!(result, Ok(()), "keys last on {last}");
            assert_eq!(
                effects,
                [Effect::Leave, Effect::Focus(last.clone()), Effect::Unmap],
                "keys last on {last}"
            );
            assert!(layout.borrow().is_stowed(&terminal));
            assert_eq!(layout.borrow().focus(), &last);

            // Shown again, given the keys, and stowed by `scratchpad.show` this time.
            let shown = eitri_core::layout::scratchpad_show(&mut layout.borrow_mut(), None, &frame);
            assert_eq!(shown, Ok(ScratchShow::Shown(terminal.clone())));
            layout.borrow_mut().set_focus(&terminal).unwrap();
            let (result, effects) =
                leave_and_record(&layout, &terminal, |l| {
                    match eitri_core::layout::scratchpad_show(l, None, &frame)? {
                        ScratchShow::Stowed(next) => Ok(next),
                        other => panic!("not a stow: {other:?}"),
                    }
                });
            assert_eq!(result, Ok(()), "keys last on {last}");
            assert_eq!(
                effects,
                [Effect::Leave, Effect::Focus(last.clone()), Effect::Unmap],
                "keys last on {last}"
            );
            assert!(layout.borrow().is_stowed(&terminal));
        }
    }

    /// What [`scratch_show_in_order`] did, in order.
    #[derive(Debug, PartialEq)]
    enum Step {
        Map(ModuleId),
        Focus(ModuleId),
        Apply,
        Restack,
    }

    fn show_and_record(
        layout: &RefCell<Layout>,
        module: Option<&ModuleId>,
    ) -> (Result<ScratchShow, LayoutError>, Vec<Step>) {
        let steps = RefCell::new(Vec::new());
        let frame = Frame::new(UNALLOCATED, 1);
        let released = |what: &str| {
            assert!(layout.try_borrow_mut().is_ok(), "the layout is released before {what}");
        };
        let result = scratch_show_in_order(
            layout,
            |l| eitri_core::layout::scratchpad_show(l, module, &frame),
            |id| {
                released("a map");
                steps.borrow_mut().push(Step::Map(id.clone()));
            },
            |id| {
                released("the keys move");
                steps.borrow_mut().push(Step::Focus(id.clone()));
            },
            || {
                released("apply");
                steps.borrow_mut().push(Step::Apply);
            },
            || {
                released("the restack");
                steps.borrow_mut().push(Step::Restack);
            },
        );
        (result, steps.into_inner())
    }

    /// `scratchpad.show`'s other answers: a float shown or raised is mapped by `apply` and restacked on
    /// top before it gets the keys; a stow gives the keys away before `apply` unmaps it; an instance
    /// that does not exist changes nothing at all.
    #[test]
    fn a_shown_scratch_float_is_mapped_and_raised_before_it_gets_the_keys() {
        let terminal = ModuleId::terminal();
        let layout = RefCell::new(with_a_float(FloatRect::centred(UNALLOCATED, 0.6), &ModuleId::agent()));
        let (result, steps) = show_and_record(&layout, Some(&terminal));
        assert_eq!(result, Ok(ScratchShow::Shown(terminal.clone())));
        assert_eq!(
            steps,
            [
                Step::Apply,
                Step::Restack,
                Step::Map(terminal.clone()),
                Step::Focus(terminal.clone())
            ]
        );
        layout.borrow_mut().set_focus(&terminal).unwrap();

        let (result, steps) = show_and_record(&layout, None);
        assert!(matches!(result, Ok(ScratchShow::Stowed(Some(_)))), "{result:?}");
        let Ok(ScratchShow::Stowed(Some(next))) = result else {
            unreachable!()
        };
        assert_eq!(
            steps,
            [
                Step::Map(next.clone()),
                Step::Focus(next.clone()),
                Step::Apply,
                Step::Restack
            ]
        );

        let missing = ModuleId::parse("terminal#3").unwrap();
        let (result, steps) = show_and_record(&layout, Some(&missing));
        assert_eq!(result, Ok(ScratchShow::Missing(missing)));
        assert!(steps.is_empty(), "{steps:?}");
    }

    /// A module title is the user's text: a float's bar shows `<b>x</b>` as those characters. Read off
    /// `floatbars.rs`'s code (comments, literals and any test module blanked): no markup setter at all,
    /// markup never switched on, and the text set in exactly one place, `set_bar_text`, which every
    /// bar goes through.
    #[test]
    fn float_bar_titles_are_set_as_text_never_markup() {
        use eitri_core::source_scan as scan;
        let source = scan::read_required("shell/src/module_grid/floatbars.rs");
        let code = scan::code_only(&source.text);
        let code = scan::without(&code, &scan::modules(&code, "tests"));
        let flat: String = code.split_whitespace().collect();
        for forbidden in [
            "set_markup(",
            "set_use_markup(true)",
            "set_label(",
            "with_mnemonic(",
            "set_text_with_mnemonic(",
            "set_markup_with_mnemonic(",
        ] {
            assert!(!flat.contains(forbidden), "floatbars.rs calls {forbidden}");
        }
        assert!(
            flat.contains("set_use_markup(false)"),
            "markup is switched off on the bar"
        );
        assert_eq!(flat.matches(".set_text(").count(), 1, "the text is set in one place");
        let setter = scan::functions(&code, "set_bar_text");
        assert_eq!(setter.len(), 1);
        assert!(setter[0].contains(".set_text("), "{}", setter[0]);
        // `Label::new(Some(text))` would set text past the one function.
        assert!(flat.contains("Label::new(None)"), "the bar's label is made empty");
        assert_eq!(flat.matches("Label::new(").count(), 1);
        assert!(
            scan::calls(&code, "set_bar_text") >= 2,
            "show and relabel both go through it"
        );
    }

    /// The saver's hook goes first and only after a change the grid applied; a change the controller made
    /// runs the four chrome hooks alone, each list in the order its hooks were added.
    #[test]
    fn a_chrome_hooks_op_runs_the_four_chrome_hooks_and_not_the_saver() {
        let ran: Rc<RefCell<Vec<&'static str>>> = Rc::default();
        let hook = |name: &'static str| -> ChangeHook {
            let ran = ran.clone();
            Rc::new(move || ran.borrow_mut().push(name))
        };
        let mut hooks = ChangeHooks::default();
        hooks.changed.push(hook("saver"));
        for name in ["selection", "tray", "mark", "workspace"] {
            hooks.chrome.push(hook(name));
        }
        for hook in hooks.after_chrome() {
            hook();
        }
        assert_eq!(*ran.borrow(), ["selection", "tray", "mark", "workspace"]);
        ran.borrow_mut().clear();
        for hook in hooks.after_apply() {
            hook();
        }
        assert_eq!(
            *ran.borrow(),
            ["saver", "selection", "tray", "mark", "workspace"],
            "the grid's own apply still tells the saver, first"
        );
    }

    /// What the window controller answered for the grid's allocation, and how many times it was asked.
    struct Answers {
        arrangement: Rc<Arrangement>,
        asked: Cell<usize>,
    }

    impl Allocator for Answers {
        fn allocate(&self, _win: WinId, _size: Size) -> Rc<Arrangement> {
            self.asked.set(self.asked.get() + 1);
            self.arrangement.clone()
        }
        fn min_size(&self, _win: WinId) -> Size {
            Size::default()
        }
    }

    /// A bottom terminal row pinned and never on screen yet, under the editor.
    fn pinned_row() -> Layout {
        Layout::from_parts(
            Node::pinned(
                Axis::Column,
                0.7,
                eitri_core::layout::Branch::Second,
                Node::Leaf(ModuleId::editor()),
                Node::Leaf(ModuleId::terminal()),
            ),
            BTreeSet::new(),
            ModuleId::editor(),
        )
        .unwrap()
    }

    /// With an allocator, its arrangement is the one allocated and the grid settles no pin of its own (the
    /// allocator does, and tells the saver); with none, the grid settles the pin and arranges its layout.
    #[test]
    fn the_grid_arranges_through_its_allocator_when_it_has_one() {
        let size = Size { w: 1200, h: 800 };
        let frame = |f: &mut dyn FnMut(&Frame)| f(&Frame::new(size, 1));
        let layout = RefCell::new(pinned_row());
        assert!(
            layout.borrow().has_pin_to_settle(),
            "the premise: a pin waits for its length"
        );
        let answers = Answers {
            arrangement: Rc::new(Arrangement::default()),
            asked: Cell::new(0),
        };
        let got = arrangement_for(Some(&answers), WinId::MAIN, &layout, size, frame);
        assert!(Rc::ptr_eq(&got, &answers.arrangement), "the allocator's arrangement");
        assert_eq!(answers.asked.get(), 1);
        assert!(layout.borrow().has_pin_to_settle(), "no pin was settled by the grid");

        let own = arrangement_for(None, WinId::MAIN, &layout, size, frame);
        assert!(
            !layout.borrow().has_pin_to_settle(),
            "a grid with no allocator settles its pin"
        );
        assert_eq!(*own, arrange(&layout.borrow(), &Frame::new(size, 1)));
        assert_eq!(own.modules.len(), 2);
    }
}
