//! `ModuleGrid` contains every module and replaces `layout::build_content_area`,
//! `layout::build_vertical_split` and `PaneLayout`'s index matches.
//!
//! Every module host is parented once in `ModuleGrid::add` and unparented only in `dispose`
//! when the window closes. Hiding uses `set_child_visible(false)`; layout changes allocate the
//! same children again. A web module's `WebView` is parented to its `WebHost` before the host
//! enters the grid. Floating or raising a module reorders children on the same parent through
//! `ModuleGrid::restack`, without moving the editor. The terminal's `GtkGLArea` has no
//! unrealize handler, and reparenting unrealizes a `WebView`. Re-realizing a GL module in
//! another toplevel with a new GL context, frame clock and scale has not been tested. The
//! editor releases its Skia state on unrealize and keeps nvim across re-realize, so it no
//! longer requires this restriction on its own. Every host logs unrealize in `ModuleGrid::add`,
//! allowing a GUI check that layout changes preserve this invariant.
//!
//! Rectangles come from `layout::arrange`. This container measures children to supply their
//! minimum sizes, allocates the resulting geometry and holds web hosts at their settled size
//! during resizing through the grid throttle. Pointer gestures are reported to the controller.

#[path = "module_grid/floatbars.rs"]
pub(crate) mod floatbars;
#[path = "module_grid/strips.rs"]
pub(crate) mod strips;
#[path = "module_grid/unrealize.rs"]
mod unrealize;

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::rc::{Rc, Weak};

use gtk4::glib;
use gtk4::graphene;
use gtk4::prelude::*;
use gtk4::subclass::prelude::*;

use eitri_core::layout::{arrange, covering, Arrangement, Axis, Divider, Frame, ModuleId, Rect, Size, Strip};

use eitri_core::wm::chrome::{FloatLabel, StripLabels};
use eitri_core::wm::grid::throttle::{WebThrottle, QUIET};
#[cfg(test)]
pub(crate) use eitri_core::wm::grid::UNALLOCATED;
use eitri_core::wm::grid::{
    band_insert_index, chrome_owners, chrome_plan, floats_need_restack, frame_size, grip_rect, restack_moves,
    shown_floats, strip_assignments, Band, Slot, DEFAULT_STRIP_PX,
};
use eitri_core::wm::{Allocator, HostEvent, HostSink, LayoutView, PointerEv, PointerTarget, WinId};
use floatbars::{ChromeHooks, FloatChrome};
use strips::ConStrip;

use crate::gtk_host::GridPort;
use crate::web_host::WebHost;

/// The phases a native drag reports, without interpreting its coordinates.
#[derive(Clone, Copy)]
pub(crate) enum DragPhase {
    Begin,
    Update,
    End,
    Cancel,
}

/// Converts native drag coordinates to the controller's pointer events; updates remain offsets from the begin.
pub(crate) fn pointer_event(win: WinId, on: PointerTarget, phase: DragPhase, x: f64, y: f64, button: u32) -> HostEvent {
    let ev = match phase {
        DragPhase::Begin => PointerEv::Press { x, y, button },
        DragPhase::Update => PointerEv::Motion { x, y },
        DragPhase::End => PointerEv::Release { x, y },
        DragPhase::Cancel => PointerEv::Cancel,
    };
    HostEvent::Pointer { win, on, ev }
}

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

/// A hook run after a change the grid applied itself ([`ModuleGrid::connect_changed`]).
type ChangeHook = Rc<dyn Fn()>;

/// What a strip entry or a float bar says for `id`: its text in `labels`, else the module's id.
fn label_of(labels: &BTreeMap<ModuleId, String>, id: &ModuleId) -> String {
    labels.get(id).cloned().unwrap_or_else(|| id.to_string())
}

/// Where the probe puts a host, whatever the layout says: a rectangle in the grid's coordinates (it may
/// overlap another host or reach past the grid's edge), or off the screen.
#[cfg(feature = "probe")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProbePlace {
    Hidden,
    At(Rect),
}

/// A prepared page stays mapped, drawn and covered, below every other child and under a cover of its own, until its
/// committed arrangement can be allocated.
#[derive(Clone, Copy)]
struct Prepared {
    rect: Rect,
    reveal: bool,
}

fn prepared_rectangle(grid: Size, size: (u32, u32)) -> Rect {
    Rect {
        x: 0,
        y: 0,
        w: if size.0 == 0 {
            grid.w.max(1)
        } else {
            (size.0.min(i32::MAX as u32) as i32).min(grid.w.max(1))
        },
        h: if size.1 == 0 {
            grid.h.max(1)
        } else {
            (size.1.min(i32::MAX as u32) as i32).min(grid.h.max(1))
        },
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

/// A divider on screen, and the divider it currently draws for drawing and the probe.
struct Handle {
    widget: gtk4::Separator,
    current: Rc<RefCell<Option<Divider>>>,
}

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct ModuleGrid {
        /// What the grid draws: the window's layout, read through its view, never written here.
        pub(super) layout: RefCell<Option<LayoutView>>,
        pub(super) hosts: RefCell<Vec<Host>>,
        pub(super) handles: RefCell<Vec<Handle>>,
        /// The strip pool: every tabbed or stacked container on screen draws one, in arrangement order.
        pub(super) strips: RefCell<Vec<ConStrip>>,
        /// Called with a strip entry's module when it is clicked ([`ModuleGrid::connect_strip_activate`]).
        pub(super) strip_activate: RefCell<Vec<StripHook>>,
        /// Each strip entry's text by its module, as the window's chrome model says it
        /// ([`ModuleGrid::set_strip_labels`]); an entry it does not name shows its module's id.
        pub(super) strip_labels: RefCell<BTreeMap<ModuleId, String>>,
        pub(super) throttle: RefCell<WebThrottle>,
        pub(super) settle: RefCell<Option<glib::SourceId>>,
        /// The modules allocated in the previous pass (the throttle's "was on screen").
        pub(super) on_screen: RefCell<BTreeSet<ModuleId>>,
        /// Web hosts allocated at a held size this pass, with the rectangle each is clipped to.
        pub(super) clips: RefCell<Vec<(gtk4::Widget, Rect)>>,
        pub(super) prepared: RefCell<BTreeMap<ModuleId, Prepared>>,
        /// What a prepared page is painted over with until its reveal: the grid's own background, which the window
        /// sets from its theme ([`ModuleGrid::set_page_cover`]). `None` paints no cover.
        pub(super) page_cover: Cell<Option<gtk4::gdk::RGBA>>,
        /// The open editor rectangle in grid coordinates, excluded from prepared-page paint.
        pub(super) editor_hole: Cell<Option<graphene::Rect>>,
        /// Called after each allocation the window controller asked for, its change of the layout carried out
        /// ([`ModuleGrid::connect_changed_by_controller`]), in the order they were added.
        pub(super) controller_hooks: RefCell<Vec<ChangeHook>>,
        /// Where the arrangement and the minimum size come from once the window controller drives the grid
        /// ([`ModuleGrid::set_allocator`]); without one the grid arranges its own layout.
        pub(super) allocator: RefCell<Option<(WinId, Weak<dyn Allocator>)>>,
        /// The float chrome pool: a bar and a grip per piece ([`floatbars`]).
        pub(super) chrome: RefCell<Vec<FloatChrome>>,
        /// Which float each piece of chrome draws, slot by slot ([`chrome_owners`]).
        pub(super) chrome_owners: RefCell<Vec<ModuleId>>,
        /// The floats the last [`ModuleGrid::restack`] put in the float band: what `child_bands`
        /// classifies as float hosts, and what `apply` compares the layout's floats with.
        pub(super) float_band: RefCell<BTreeSet<ModuleId>>,
        /// A restack asked for from inside an allocation and not yet run.
        pub(super) restack_queued: Cell<bool>,
        /// Each float bar's text by its module, as the window's chrome model says it
        /// ([`ModuleGrid::set_float_labels`]); a bar it does not name shows its module's id.
        pub(super) float_labels: RefCell<BTreeMap<ModuleId, String>>,
        /// The window and its weak posting sink; gestures release this borrow before reporting an event.
        pub(super) pointer_sink: RefCell<Option<(WinId, Weak<dyn HostSink>)>>,
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
    pub(crate) fn new(layout: LayoutView) -> Self {
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

    /// Keeps the page in its own grid, laid out at its final rectangle and mapped at full opacity as the grid's
    /// first child, drawn first and painted over at once with the grid's background ([`ModuleGrid::set_page_cover`]),
    /// so it is unseen until the reveal whatever else is on screen: a layout that shows only floats leaves its
    /// rectangle under no other host. It must be drawn while it is unseen: GTK does not draw a widget at zero
    /// opacity, and WebKit runs a page's animation frames only for a view that is drawn, so a page that answers from
    /// a frame callback could not say it is ready before it is revealed. The host never moves at the reveal, only its
    /// stacking does: WebKit can retain a stale picture when an off-screen view is moved into the visible band. It
    /// takes no pointer until it is revealed.
    pub(crate) fn prepare_page(&self, id: &ModuleId, size: (u32, u32)) -> Result<(), String> {
        let widget = self
            .hosts()
            .into_iter()
            .find(|(module, _)| module == id)
            .map(|(_, w)| w)
            .ok_or("the plugin has no grid host")?;
        let grid = Size {
            w: self.width().max(1),
            h: self.height().max(1),
        };
        let mut rect = prepared_rectangle(grid, size);
        let mut future = self.layout().read().clone();
        let _ = future.set_away(id, false);
        let _ = future.show(id);
        self.with_frame(grid, |frame| {
            let arrangement = arrange(&future, frame);
            if let Some((_, target)) = arrangement.modules.iter().find(|(module, _)| module == id) {
                rect = *target;
            }
            if let Some(spot) = arrangement.floats.iter().find(|spot| spot.id == *id) {
                rect = spot.host;
            }
        });
        self.imp()
            .prepared
            .borrow_mut()
            .insert(id.clone(), Prepared { rect, reveal: false });
        widget.insert_after(self, None::<&gtk4::Widget>);
        widget.set_opacity(1.0);
        widget.set_can_target(false);
        widget.set_child_visible(true);
        // The ready answer may leave before GTK's next frame, so the host must have its size already.
        let _ = widget.measure(gtk4::Orientation::Horizontal, -1);
        let _ = widget.measure(gtk4::Orientation::Vertical, rect.w);
        widget.size_allocate(&gtk4::Allocation::new(rect.x, rect.y, rect.w, rect.h), -1);
        self.queue_allocate();
        Ok(())
    }

    /// The colour a prepared page is covered with until it is revealed: the grid's own background, the colour that
    /// rectangle shows with nothing on it, which is the theme's `bg` (`.content-area` in `theme::gtk_css`). The
    /// window sets it from the theme it styles the grid with, and again on each change of that theme.
    pub(crate) fn set_page_cover(&self, tokens: &eitri_core::theme::ThemeTokens) {
        let bg = tokens.bg;
        self.imp().page_cover.set(Some(gtk4::gdk::RGBA::new(
            f32::from(bg.r) / 255.0,
            f32::from(bg.g) / 255.0,
            f32::from(bg.b) / 255.0,
            1.0,
        )));
        if !self.imp().prepared.borrow().is_empty() {
            self.queue_draw();
        }
    }

    pub(crate) fn set_editor_hole(&self, hole: Option<graphene::Rect>) {
        self.imp().editor_hole.set(hole);
        self.queue_draw();
    }

    /// The hosts of prepared pages not yet revealed, each drawn under its cover.
    fn covered_hosts(&self) -> Vec<gtk4::Widget> {
        let prepared = self.imp().prepared.borrow();
        self.imp()
            .hosts
            .borrow()
            .iter()
            .filter(|h| prepared.contains_key(&h.id))
            .map(|h| h.widget.clone())
            .collect()
    }

    pub(crate) fn map_prepared(&self, id: &ModuleId, mapped: bool) {
        if !self.imp().prepared.borrow().contains_key(id) {
            return;
        }
        if mapped {
            if let Some(prepared) = self.imp().prepared.borrow_mut().get_mut(id) {
                prepared.reveal = true;
            }
        } else if self.imp().prepared.borrow_mut().remove(id).is_some() {
            if let Some((_, widget)) = self.hosts().into_iter().find(|(module, _)| module == id) {
                widget.set_child_visible(false);
                widget.set_opacity(1.0);
                widget.set_can_target(true);
            }
        }
        self.restack();
        self.queue_allocate();
    }

    /// The layout the grid draws, through the same view the window's readers hold.
    fn layout(&self) -> LayoutView {
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
            widget.connect_unrealize(move |widget| {
                unrealized.set(unrealized.get() + 1);
                // Expected exactly once per host, when the window closes. Any earlier line is the
                // bug this container exists to rule out (held by a counter).
                eprintln!(
                    "{}",
                    unrealize::line(&id, unrealized.get(), unrealize::window_is_closing(widget))
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
        let on = PointerTarget::Divider(self.imp().handles.borrow().len() as u32);
        let drag = gtk4::GestureDrag::new();
        {
            let grid = self.downgrade();
            let on = on.clone();
            drag.connect_drag_begin(move |gesture, x, y| {
                // The divider owns the press; the module beneath it must not act on it too.
                gesture.set_state(gtk4::EventSequenceState::Claimed);
                if let Some(grid) = grid.upgrade() {
                    grid.post_pointer(on.clone(), DragPhase::Begin, x, y, gesture.current_button());
                }
            });
        }
        {
            let grid = self.downgrade();
            let on = on.clone();
            drag.connect_drag_update(move |gesture, dx, dy| {
                if let Some(grid) = grid.upgrade() {
                    grid.post_pointer(on.clone(), DragPhase::Update, dx, dy, gesture.current_button());
                }
            });
        }
        {
            let grid = self.downgrade();
            let on = on.clone();
            drag.connect_cancel(move |gesture, _| {
                if let Some(grid) = grid.upgrade() {
                    grid.post_pointer(on.clone(), DragPhase::Cancel, 0.0, 0.0, gesture.current_button());
                }
            });
        }
        let grid = self.downgrade();
        drag.connect_drag_end(move |gesture, x, y| {
            if let Some(grid) = grid.upgrade() {
                grid.post_pointer(on.clone(), DragPhase::End, x, y, gesture.current_button());
            }
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
            let Some(layout) = layout.try_read() else {
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

    /// Reports to the same sink as the window's other native events; no gesture owns the controller.
    pub(crate) fn set_pointer_sink(&self, win: WinId, sink: Weak<dyn HostSink>) {
        *self.imp().pointer_sink.borrow_mut() = Some((win, sink));
    }

    fn post_pointer(&self, on: PointerTarget, phase: DragPhase, x: f64, y: f64, button: u32) {
        let posting = self.imp().pointer_sink.borrow().clone();
        if let Some((win, sink)) = posting {
            if let Some(sink) = sink.upgrade() {
                sink.post(pointer_event(win, on, phase, x, y, button));
            }
        }
    }

    /// A new piece of float chrome, wired only to report native events.
    fn new_chrome(&self) -> FloatChrome {
        let grid = self.downgrade();
        FloatChrome::new(&ChromeHooks {
            post: Rc::new(move |on, phase, x, y, button| {
                if let Some(grid) = grid.upgrade() {
                    grid.post_pointer(on, phase, x, y, button);
                }
            }),
        })
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
        let leaves = self.layout().read().modules();
        let hosts = self.hosts();
        leaves
            .into_iter()
            .filter_map(|id| hosts.iter().find(|(m, _)| *m == id).cloned())
            .collect()
    }

    /// Makes each host's visibility match the layout, then re-measures and re-allocates: what a grid with no
    /// window controller behind it (the companion window's, whose layout never changes) does once its hosts are
    /// added, and what a host the grid probe placed by hand goes back to; a window controller's grid is otherwise
    /// mapped and allocated by its ops. The layout is read, and
    /// released, before any child's visibility changes: hiding the focused widget moves GTK focus, whose focus
    /// report re-enters the controller.
    ///
    /// **It restacks whenever the set of floats on screen is not the one the last restack put in the
    /// float band** ([`floats_need_restack`]), so every verb that changes which modules float -- float
    /// and sink, the scratchpad, a swap between a float and a tile, a far move that docks, a split that
    /// sinks a float, a kill that reopens one tiled, a workspace switch -- restacks without a list of
    /// them to keep up. A divider drag or a window resize changes no float and never restacks.
    pub(crate) fn apply(&self) {
        let (wanted, floats): (Vec<(gtk4::Widget, bool)>, Vec<ModuleId>) = {
            let layout = self.layout();
            let layout = layout.read();
            let wanted = self
                .imp()
                .hosts
                .borrow()
                .iter()
                .filter(|h| !self.probe_places_host(&h.id) && !self.imp().prepared.borrow().contains_key(&h.id))
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
    }

    /// `f` runs after every allocation the window controller asks for: every change of the layout ends with one,
    /// since the controller is the one that changes it, so this is the hook for what must follow the layout. Runs
    /// with the layout released.
    pub(crate) fn connect_changed_by_controller(&self, f: impl Fn() + 'static) {
        self.imp().controller_hooks.borrow_mut().push(Rc::new(f));
    }

    fn notify_changed_by_controller(&self) {
        // Cloned out: a hook may add another.
        let hooks: Vec<ChangeHook> = self.imp().controller_hooks.borrow().clone();
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
            .filter(|h| !self.probe_places_host(&h.id) && !self.imp().prepared.borrow().contains_key(&h.id))
            .map(|h| h.widget.clone())
            .collect();
        for widget in widgets {
            widget.set_child_visible(false);
        }
    }

    /// `f` runs with a strip entry's module when a primary click lands on that entry. What it does --
    /// telling the window controller, which switches to the tab and gives it the keys -- is the caller's. Runs
    /// with the layout released.
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

    /// What every strip entry says, as the window's chrome model has it: applied to the strips on screen now and
    /// used for every one drawn later. The entries and their places stay as the last allocation left them:
    /// nothing is measured or allocated. Until an entry is named it shows its module's id.
    pub(crate) fn set_strip_labels(&self, strips: &[StripLabels]) {
        let labels = strips
            .iter()
            .flat_map(|strip| &strip.entries)
            .map(|entry| (entry.module.clone(), entry.label.clone()))
            .collect();
        *self.imp().strip_labels.borrow_mut() = labels;
        let labels = self.imp().strip_labels.borrow();
        let label = |id: &ModuleId| label_of(&labels, id);
        for strip in self.imp().strips.borrow().iter() {
            strip.relabel(&label);
        }
    }

    /// What every float bar says, as the window's chrome model has it: applied to the bars on screen now and used
    /// for every one drawn later, always as plain text. Nothing is measured or allocated. Until a bar is named it
    /// shows its module's id.
    pub(crate) fn set_float_labels(&self, bars: &[FloatLabel]) {
        let labels = bars.iter().map(|bar| (bar.module.clone(), bar.label.clone())).collect();
        *self.imp().float_labels.borrow_mut() = labels;
        let labels = self.imp().float_labels.borrow();
        let label = |id: &ModuleId| label_of(&labels, id);
        for piece in self.imp().chrome.borrow().iter() {
            piece.relabel(&label);
        }
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
        let Some(layout) = layout.try_read() else {
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

    /// The floats over `id` on a fresh arrangement at the grid's size now -- never a stale allocation's
    /// -- bottom first (`eitri_core::layout::covering`). Empty for a module off screen.
    pub(crate) fn covered_now(&self, id: &ModuleId) -> Vec<ModuleId> {
        let size = self.size();
        let layout = self.layout();
        let layout = layout.read();
        let arrangement = self.with_frame(size, |frame| arrange(&layout, frame));
        covering(&arrangement, id)
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
            if self.probe_places_host(id) || self.imp().prepared.borrow().contains_key(id) {
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
        let prepared: Vec<_> = imp
            .prepared
            .borrow()
            .iter()
            .map(|(id, state)| (id.clone(), *state))
            .collect();
        for (id, state) in prepared {
            let target = arrangement
                .modules
                .iter()
                .find(|(module, _)| *module == id)
                .map(|(_, r)| *r)
                .or_else(|| {
                    arrangement
                        .floats
                        .iter()
                        .find(|spot| spot.id == id)
                        .map(|spot| spot.host)
                });
            let rect = target.unwrap_or(state.rect);
            if let Some(widget) = self
                .hosts()
                .into_iter()
                .find(|(module, _)| *module == id)
                .map(|(_, w)| w)
            {
                let _ = widget.measure(gtk4::Orientation::Horizontal, -1);
                let _ = widget.measure(gtk4::Orientation::Vertical, rect.w);
                widget.size_allocate(&gtk4::Allocation::new(rect.x, rect.y, rect.w, rect.h), -1);
                if state.reveal {
                    widget.set_opacity(1.0);
                    widget.set_can_target(true);
                    imp.prepared.borrow_mut().remove(&id);
                }
            }
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
        let labels = self.imp().strip_labels.borrow();
        let label = |id: &ModuleId| label_of(&labels, id);
        let pool = self.imp().strips.borrow();
        for (i, con_strip) in pool.iter().enumerate() {
            let Some(strip) = slots.iter().position(|&slot| slot == i).map(|n| &strips[n]) else {
                con_strip.hide();
                continue;
            };
            con_strip.show(strip, &label);
            let r = strip.rect;
            let _ = con_strip.widget.measure(gtk4::Orientation::Horizontal, -1);
            let _ = con_strip.widget.measure(gtk4::Orientation::Vertical, r.w);
            con_strip
                .widget
                .size_allocate(&gtk4::Allocation::new(r.x, r.y, r.w, r.h), -1);
        }
    }

    /// Draws each float on screen with the piece of chrome that owns it ([`chrome_owners`]):
    /// its bar across the float's top strip row, its grip in the bottom-right corner. Every other piece
    /// is hidden, and so is every piece while a zoom is on (a zoomed float fills the grid alone, with no
    /// bar). A float with no piece of its own yet -- the layout changed without an `apply` -- has none
    /// this pass and asks for a restack, which makes one.
    fn place_float_chrome(&self, arrangement: &Arrangement) {
        let imp = self.imp();
        let owners = imp.chrome_owners.borrow().clone();
        let labels = imp.float_labels.borrow();
        let label = |id: &ModuleId| label_of(&labels, id);
        for (i, piece) in imp.chrome.borrow().iter().enumerate() {
            let Some(spot) = owners.get(i).and_then(|id| arrangement.float_spot(id)) else {
                piece.hide();
                continue;
            };
            piece.show(spot, &label);
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

    /// Draws each prepared page first, then its cover over it, then every other child in order: whatever a restack
    /// did to the child order meanwhile, nothing on screen is drawn under a page that is not revealed yet.
    fn snapshot_children(&self, snapshot: &gtk4::Snapshot) {
        let covered = self.covered_hosts();
        let cover = self.imp().page_cover.get();
        let hole = self.imp().editor_hole.get();
        for widget in &covered {
            if hole.is_none() {
                self.snapshot_child(widget, snapshot);
                if let (Some(color), Some(bounds)) = (cover, widget.compute_bounds(self)) {
                    snapshot.append_color(&color, &bounds);
                }
                continue;
            }
            let Some(bounds) = widget.compute_bounds(self) else {
                self.snapshot_child(widget, snapshot);
                continue;
            };
            // The page and its cover stay outside the editor: either could otherwise obscure its separate surface.
            let page = gtk4::Snapshot::new();
            self.snapshot_child(widget, &page);
            let page = page.to_node();
            page_outside_hole(bounds, hole, page.as_ref(), cover, |op| match op {
                PageOp::Clip(rect) => snapshot.push_clip(&rect),
                PageOp::Page(page) => snapshot.append_node(page),
                PageOp::Cover(color, rect) => snapshot.append_color(&color, &rect),
                PageOp::Pop => snapshot.pop(),
            });
        }
        let clips = self.imp().clips.borrow();
        let mut child = self.first_child();
        while let Some(widget) = child {
            if covered.contains(&widget) {
                child = widget.next_sibling();
                continue;
            }
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

/// One drawing call of [`page_outside_hole`], kept apart from `gtk4::Snapshot` so the sequence is tested without a
/// display.
#[derive(Debug, PartialEq)]
enum PageOp<'a, P, C> {
    Clip(graphene::Rect),
    Page(&'a P),
    Cover(C, graphene::Rect),
    Pop,
}

/// Draws a prepared page and its cover only outside the editor's hole: each complement rectangle gets its own clip
/// holding the page and then the cover, so neither ever lands on the editor's separate surface.
fn page_outside_hole<'a, P, C: Copy>(
    bounds: graphene::Rect,
    hole: Option<graphene::Rect>,
    page: Option<&'a P>,
    cover: Option<C>,
    mut draw: impl FnMut(PageOp<'a, P, C>),
) {
    for rect in crate::editor_hole::hole_complement(bounds, hole) {
        draw(PageOp::Clip(rect));
        if let Some(page) = page {
            draw(PageOp::Page(page));
        }
        if let Some(color) = cover {
            draw(PageOp::Cover(color, rect));
        }
        draw(PageOp::Pop);
    }
}

/// The window controller's way into the grid, held weakly as the GTK host holds its windows: a grid gone
/// with its window answers as one never allocated and does nothing.
impl GridPort for glib::WeakRef<ModuleGrid> {
    fn set_pointer_sink(&self, win: WinId, sink: Weak<dyn HostSink>) {
        if let Some(grid) = self.upgrade() {
            grid.set_pointer_sink(win, sink);
        }
    }

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
            grid.notify_changed_by_controller();
        }
    }
    fn restack_floats(&self, order: &[ModuleId]) {
        if let Some(grid) = self.upgrade() {
            grid.restack_floats(order);
        }
    }
    fn probe_places(&self, module: &ModuleId) -> bool {
        self.upgrade().is_some_and(|grid| grid.probe_places_host(module))
    }
    fn module_size(&self, module: &ModuleId) -> Option<Size> {
        let grid = self.upgrade()?;
        let (_, host) = grid.hosts().into_iter().find(|(id, _)| id == module)?;
        let size = Size {
            w: host.width(),
            h: host.height(),
        };
        (size.w > 0 && size.h > 0).then_some(size)
    }
}

/// The arrangement the grid allocates at `size`: its allocator's, which settles the pinned splits shown for
/// the first time itself, or, for a grid with none, `layout`'s own in the frame `with_frame` gives. A grid with
/// none writes nothing: a pinned split shown for the first time is arranged on a copy given the length its ratio
/// gives at this, the real allocation -- never at `UNALLOCATED`'s stand-in size. Asked first, with no geometry,
/// so a drag or a resize sweep arranges once per allocation.
pub(crate) fn arrangement_for(
    allocator: Option<&dyn Allocator>,
    win: WinId,
    layout: &LayoutView,
    size: Size,
    with_frame: impl Fn(&mut dyn FnMut(&Frame)),
) -> Rc<Arrangement> {
    if let Some(allocator) = allocator {
        return allocator.allocate(win, size);
    }
    let Some(read) = layout.try_read() else {
        return Rc::new(Arrangement::default());
    };
    let mut arrangement = Arrangement::default();
    if size.w > 0 && size.h > 0 && read.has_pin_to_settle() {
        let mut settled = read.clone();
        drop(read);
        with_frame(&mut |frame| {
            eitri_core::layout::settle_pins(&mut settled, frame);
            arrangement = arrange(&settled, frame);
        });
    } else {
        with_frame(&mut |frame| arrangement = arrange(&read, frame));
    }
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

/// [`ModuleGrid::focus_target`]'s choice, made on the grid's own host records (generic over the
/// widget so it is tested without a display): the focus target recorded for `id`, never its host.
fn focus_target_of<'a, W>(hosts: &'a [Host<W>], id: &ModuleId) -> Option<&'a W> {
    hosts.iter().find(|h| h.id == *id).map(|h| &h.focus)
}

#[cfg(test)]
mod tests {
    use super::*;
    use eitri_core::layout::{Arrangement, Layout, Node};

    #[test]
    fn a_prepared_page_cover_is_clipped_out_of_the_hole() {
        let cover = graphene::Rect::new(20.0, 30.0, 500.0, 400.0);
        let hole = graphene::Rect::new(100.0, 120.0, 700.0, 600.0);
        let rectangles = crate::editor_hole::hole_complement(cover, Some(hole));
        assert_eq!(rectangles.len(), 2);
        assert_eq!(
            rectangles.iter().map(|r| r.width() * r.height()).sum::<f32>(),
            500.0 * 400.0 - 420.0 * 310.0
        );
        for rect in rectangles {
            assert!(cover.contains_rect(&rect));
            assert!(rect.intersection(&hole).is_none_or(|r| r.width() * r.height() == 0.0));
        }
    }

    #[test]
    fn a_prepared_page_and_its_cover_are_drawn_only_inside_clips_outside_the_hole() {
        let bounds = graphene::Rect::new(20.0, 30.0, 500.0, 400.0);
        let hole = graphene::Rect::new(100.0, 120.0, 700.0, 600.0);
        let mut ops = Vec::new();
        page_outside_hole(bounds, Some(hole), Some(&"page"), Some('c'), |op| ops.push(op));
        let rects = crate::editor_hole::hole_complement(bounds, Some(hole));
        assert_eq!(rects.len(), 2);
        let expected: Vec<_> = rects
            .iter()
            .flat_map(|&r| {
                [
                    PageOp::Clip(r),
                    PageOp::Page(&"page"),
                    PageOp::Cover('c', r),
                    PageOp::Pop,
                ]
            })
            .collect();
        assert_eq!(ops, expected, "each clip holds the page, then the cover, then closes");

        let mut ops = Vec::new();
        page_outside_hole(bounds, Some(hole), Some(&"page"), None::<char>, |op| ops.push(op));
        assert!(
            !ops.iter().any(|op| matches!(op, PageOp::Cover(..))),
            "no cover colour, no cover"
        );
        assert_eq!(ops.iter().filter(|op| matches!(op, PageOp::Page(_))).count(), 2);

        let mut ops = Vec::new();
        page_outside_hole(bounds, Some(bounds), Some(&"page"), Some('c'), |op| ops.push(op));
        assert!(ops.is_empty(), "a page wholly inside the hole is not drawn at all");
    }

    #[test]
    fn a_prepared_rectangle_uses_the_offered_size_within_the_grid() {
        let rect = prepared_rectangle(Size { w: 1000, h: 800 }, (600, 400));
        assert_eq!(
            rect,
            Rect {
                x: 0,
                y: 0,
                w: 600,
                h: 400
            }
        );
    }

    #[test]
    fn a_drag_reports_its_begin_offsets_and_end() {
        let win = WinId(7);
        for on in [
            PointerTarget::Divider(3),
            PointerTarget::FloatBar(ModuleId::terminal()),
            PointerTarget::FloatGrip(ModuleId::agent()),
        ] {
            for (phase, x, y, expected) in [
                (
                    DragPhase::Begin,
                    12.5,
                    8.0,
                    PointerEv::Press {
                        x: 12.5,
                        y: 8.0,
                        button: 2,
                    },
                ),
                (
                    DragPhase::Update,
                    -400.0,
                    23.5,
                    PointerEv::Motion { x: -400.0, y: 23.5 },
                ),
                (DragPhase::End, 401.5, -18.0, PointerEv::Release { x: 401.5, y: -18.0 }),
                (DragPhase::Cancel, 0.0, 0.0, PointerEv::Cancel),
            ] {
                let HostEvent::Pointer {
                    win: got_win,
                    on: got_on,
                    ev,
                } = pointer_event(win, on.clone(), phase, x, y, 2)
                else {
                    panic!("a drag must report a pointer event")
                };
                assert_eq!(got_win, win);
                assert_eq!(got_on, on);
                assert_eq!(ev, expected);
            }
        }
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
        let mut layout = eitri_core::wm::persist::first_launch_layout(&decls).unwrap();
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
    /// allocator does, and tells the saver); with none, the grid arranges its layout with the pin settled, and
    /// writes nothing.
    #[test]
    fn the_grid_arranges_through_its_allocator_when_it_has_one() {
        let size = Size { w: 1200, h: 800 };
        let frame = |f: &mut dyn FnMut(&Frame)| f(&Frame::new(size, 1));
        let (layout, cell) = LayoutView::for_test(pinned_row());
        assert!(
            layout.read().has_pin_to_settle(),
            "the premise: a pin waits for its length"
        );
        let answers = Answers {
            arrangement: Rc::new(Arrangement::default()),
            asked: Cell::new(0),
        };
        let got = arrangement_for(Some(&answers), WinId::MAIN, &layout, size, frame);
        assert!(Rc::ptr_eq(&got, &answers.arrangement), "the allocator's arrangement");
        assert_eq!(answers.asked.get(), 1);
        assert!(layout.read().has_pin_to_settle(), "no pin was settled by the grid");

        let own = arrangement_for(None, WinId::MAIN, &layout, size, frame);
        assert!(
            layout.read().has_pin_to_settle(),
            "a grid with no allocator writes nothing"
        );
        let mut settled = layout.read().clone();
        eitri_core::layout::settle_pins(&mut settled, &Frame::new(size, 1));
        assert_eq!(*own, arrange(&settled, &Frame::new(size, 1)));
        assert_eq!(own.modules.len(), 2);
        assert!(cell.borrow().has_pin_to_settle());
    }
}
