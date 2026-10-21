//! The chrome of floats: a bar one strip row high across a float's top edge, naming its module, and a
//! 12px grip at its bottom-right corner. A drag on the bar moves the float, a drag on the grip sizes
//! it, and a primary press on either raises it. Like the strips (`strips.rs`), the chrome belongs to
//! the grid from the moment it is made until the window goes away: hidden with
//! `set_child_visible(false)` while no float needs it, reused by the next one that does, and unparented
//! only in the grid's `dispose`.
//!
//! Each piece of chrome keeps drawing the same float for as long as that float stays on screen
//! ([`chrome_owners`]): a press raises the float under the pointer, and if the chrome followed the
//! z-order instead, the bar being dragged would start drawing another float halfway through its own
//! drag.
//!
//! A bar's text is whatever the window's label function returns for its module, set as plain text: a
//! chat's session label is the user's (or the agent's) own words, and `<b>x</b>` in it must read as
//! those five characters, not as bold.

use std::cell::RefCell;
use std::ops::Range;
use std::rc::Rc;

use gtk4::glib;
use gtk4::prelude::*;

use eitri_core::layout::{FloatSpot, ModuleId, Rect};

/// The bar's CSS node name. `theme::gtk_css` styles it, and its text as `floatbar > label`.
pub(crate) const BAR_CSS_NAME: &str = "floatbar";

/// The grip's CSS node name.
pub(crate) const GRIP_CSS_NAME: &str = "floatgrip";

/// The grip's side in pixels.
pub(super) const GRIP_PX: i32 = 12;

/// The float a piece of chrome drew in the last allocation, and that float's whole rectangle.
type Drawn = Rc<RefCell<Option<(ModuleId, Rect)>>>;

/// What a drag step does with the float it began on and its next rectangle.
type DragHook = Rc<dyn Fn(&ModuleId, Rect)>;

/// What a press and a drag on a float's chrome do. Both are the grid's: it raises and restacks, and it
/// stores the dragged rectangle in the layout.
pub(super) struct ChromeHooks {
    /// A primary press on the bar or the grip, with the float it draws.
    pub(super) press: Rc<dyn Fn(&ModuleId)>,
    /// A drag's next rectangle for the float it began on.
    pub(super) drag: DragHook,
}

/// One float's bar and grip, pooled by the grid.
pub(super) struct FloatChrome {
    pub(super) bar: gtk4::Box,
    label: gtk4::Label,
    pub(super) grip: gtk4::Box,
    /// The float this chrome drew in the last allocation, and that float's whole rectangle: what a
    /// drag reads as "the rectangle as allocated now".
    current: Drawn,
}

impl FloatChrome {
    /// A bar and a grip, not yet parented and not yet drawn.
    pub(super) fn new(hooks: &ChromeHooks) -> Self {
        let bar: gtk4::Box = glib::Object::builder()
            .property("css-name", BAR_CSS_NAME)
            .property("orientation", gtk4::Orientation::Horizontal)
            .build();
        // The chrome never takes the keys: they belong to the modules, and a focusable bar would be one
        // more place GTK's own focus chain could send them when a host leaves the screen.
        bar.set_focusable(false);
        bar.set_child_visible(false);
        let label = gtk4::Label::new(None);
        // Plain text whatever the label function returns (the module doc).
        label.set_use_markup(false);
        label.set_single_line_mode(true);
        label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        label.set_xalign(0.0);
        label.set_hexpand(true);
        label.set_focusable(false);
        bar.append(&label);

        let grip: gtk4::Box = glib::Object::builder().property("css-name", GRIP_CSS_NAME).build();
        grip.set_focusable(false);
        grip.set_cursor_from_name(Some("se-resize"));
        grip.set_child_visible(false);

        let current: Drawn = Rc::new(RefCell::new(None));
        bar.add_controller(drag_gesture(&current, hooks, false));
        grip.add_controller(drag_gesture(&current, hooks, true));
        FloatChrome {
            bar,
            label,
            grip,
            current,
        }
    }

    /// Names `spot`'s module with `label`, records the rectangle a drag starts from, and shows the bar
    /// and the grip. The caller allocates them.
    pub(super) fn show(&self, spot: &FloatSpot, label: &dyn Fn(&ModuleId) -> String) {
        set_bar_text(&self.label, &label(&spot.id));
        *self.current.borrow_mut() = Some((spot.id.clone(), spot.rect));
        self.bar.set_child_visible(true);
        self.grip.set_child_visible(true);
    }

    /// Takes the bar and the grip off the screen; they stay the grid's children, for the next float.
    pub(super) fn hide(&self) {
        self.bar.set_child_visible(false);
        self.grip.set_child_visible(false);
        self.current.borrow_mut().take();
    }

    /// Names the bar again with `label`, if it draws a float now.
    // Called through the grid's `refresh_float_bars`, which `set_float_label` runs so a new label
    // reaches a bar already on screen.
    pub(super) fn relabel(&self, label: &dyn Fn(&ModuleId) -> String) {
        let drawn = self.current.borrow().as_ref().map(|(id, _)| id.clone());
        if let Some(id) = drawn {
            set_bar_text(&self.label, &label(&id));
        }
    }
}

/// The drag on a bar (`grip: false`) or a grip. The press that begins it raises the float and runs the
/// bar-press hooks; each update stores the rectangle as allocated now plus the drag's offset
/// (`super::dragged_float_rect`), for the float the drag began on only: if this chrome draws another
/// float by then, the drag stops moving anything.
fn drag_gesture(current: &Drawn, hooks: &ChromeHooks, grip: bool) -> gtk4::GestureDrag {
    let drag = gtk4::GestureDrag::new();
    drag.set_button(gtk4::gdk::BUTTON_PRIMARY);
    let dragging: Rc<RefCell<Option<ModuleId>>> = Rc::new(RefCell::new(None));
    {
        let current = current.clone();
        let dragging = dragging.clone();
        let press = hooks.press.clone();
        drag.connect_drag_begin(move |gesture, _, _| {
            // Claimed: the press is the float's, and nothing under it acts on it as well.
            gesture.set_state(gtk4::EventSequenceState::Claimed);
            // Cloned out: the hooks raise and restack, and give the module the keys.
            let id = current.borrow().as_ref().map(|(id, _)| id.clone());
            *dragging.borrow_mut() = id.clone();
            if let Some(id) = id {
                press(&id);
            }
        });
    }
    {
        let current = current.clone();
        let dragging = dragging.clone();
        let moved = hooks.drag.clone();
        drag.connect_drag_update(move |_, dx, dy| {
            let Some(grabbed) = dragging.borrow().clone() else {
                return;
            };
            let Some((drawn, now)) = current.borrow().clone() else {
                return;
            };
            if drawn != grabbed {
                return;
            }
            moved(&grabbed, super::dragged_float_rect(now, dx, dy, grip));
        });
    }
    drag.connect_drag_end(move |_, _, _| {
        dragging.borrow_mut().take();
    });
    drag
}

/// The one way a bar's text is set: as text, never as markup (the module doc).
fn set_bar_text(label: &gtk4::Label, text: &str) {
    label.set_text(text);
}

/// Where a float's grip goes: the square at the bottom-right corner of its whole rectangle.
pub(super) fn grip_rect(rect: Rect) -> Rect {
    let w = GRIP_PX.min(rect.w).max(0);
    let h = GRIP_PX.min(rect.h).max(0);
    Rect {
        x: rect.x + rect.w - w,
        y: rect.y + rect.h - h,
        w,
        h,
    }
}

/// How a pool of `pool` pieces of chrome draws `spots` floats: how many new pieces to make first, and
/// which slots to hide. The pool grows only when more floats are on screen than it has ever drawn at
/// once; the slots past the last float are hidden, never dropped.
pub(super) fn chrome_plan(pool: usize, spots: usize) -> (usize, Range<usize>) {
    (spots.saturating_sub(pool), spots..pool.max(spots))
}

/// Which float each slot of the pool draws (slot `i` draws the `i`-th): the floats it drew before that
/// are still in `floats`, in the slots' order, then the floats that are new, in z-order. So a raise,
/// which changes only the order of `floats`, moves no float to another piece of chrome; a float that
/// leaves frees its slot, and the ones above it close up.
pub(super) fn chrome_owners(drew: &[ModuleId], floats: &[ModuleId]) -> Vec<ModuleId> {
    let mut owners: Vec<ModuleId> = drew.iter().filter(|id| floats.contains(id)).cloned().collect();
    for id in floats {
        if !owners.contains(id) {
            owners.push(id.clone());
        }
    }
    owners
}
