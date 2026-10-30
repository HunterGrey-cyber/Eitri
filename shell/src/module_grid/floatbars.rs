//! The chrome of floats: a bar one strip row high across a float's top edge, naming its module, and a
//! 12px grip at its bottom-right corner. A drag on the bar moves the float, a drag on the grip sizes
//! it, and a primary press on either raises it. Like the strips (`strips.rs`), the chrome belongs to
//! the grid from the moment it is made until the window goes away: hidden with
//! `set_child_visible(false)` while no float needs it, reused by the next one that does, and unparented
//! only in the grid's `dispose`.
//!
//! Each piece of chrome keeps drawing the same float for as long as that float stays on screen
//! ([`eitri_core::wm::grid::chrome_owners`]): a press raises the float under the pointer, and if the chrome followed the
//! z-order instead, the bar being dragged would start drawing another float halfway through its own
//! drag.
//!
//! A bar's text is whatever the window's label function returns for its module, set as plain text: a
//! chat's session label is the user's (or the agent's) own words, and `<b>x</b>` in it must read as
//! those five characters, not as bold.

use std::cell::RefCell;
use std::rc::Rc;

use gtk4::glib;
use gtk4::prelude::*;

use eitri_core::layout::{FloatSpot, ModuleId, Rect};
use eitri_core::wm::PointerTarget;

use super::DragPhase;

/// The bar's CSS node name. `theme::gtk_css` styles it, and its text as `floatbar > label`.
pub(crate) const BAR_CSS_NAME: &str = "floatbar";

/// The grip's CSS node name.
pub(crate) const GRIP_CSS_NAME: &str = "floatgrip";

/// The float a piece of chrome drew in the last allocation, and that float's whole rectangle.
type Drawn = Rc<RefCell<Option<(ModuleId, Rect)>>>;

/// Reports a native gesture's address, phase and unmodified coordinates to the grid's posting sink.
type PointerPost = Rc<dyn Fn(PointerTarget, DragPhase, f64, f64, u32)>;

/// Float chrome reports pointer events; the controller decides focus, stacking and geometry.
#[derive(Clone)]
pub(super) struct ChromeHooks {
    pub(super) post: PointerPost,
}

/// One float's bar and grip, pooled by the grid.
pub(super) struct FloatChrome {
    pub(super) bar: gtk4::Box,
    label: gtk4::Label,
    pub(super) grip: gtk4::Box,
    /// The float this chrome drew in the last allocation and its whole rectangle, for drawing and the probe.
    current: Drawn,
    hooks: ChromeHooks,
    /// Controllers bound to the module this chrome draws; a reused piece gets new event addresses.
    gestures: RefCell<Option<(gtk4::GestureDrag, gtk4::GestureDrag)>>,
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
        FloatChrome {
            bar,
            label,
            grip,
            current,
            hooks: hooks.clone(),
            gestures: RefCell::new(None),
        }
    }

    /// Names `spot`'s module with `label`, records what is drawn, and shows its chrome. The caller allocates it.
    pub(super) fn show(&self, spot: &FloatSpot, label: &dyn Fn(&ModuleId) -> String) {
        set_bar_text(&self.label, &label(&spot.id));
        let owner_changed = self.current.borrow().as_ref().is_none_or(|(id, _)| *id != spot.id);
        if owner_changed {
            // Event addresses stay with the drawn module. Reusing chrome cancels its old controllers instead
            // of redirecting a sequence to the next module that happens to occupy the pool slot.
            let old = self.gestures.borrow_mut().take();
            if let Some((bar, grip)) = old {
                self.bar.remove_controller(&bar);
                self.grip.remove_controller(&grip);
            }
            let bar = drag_gesture(PointerTarget::FloatBar(spot.id.clone()), &self.hooks);
            let grip = drag_gesture(PointerTarget::FloatGrip(spot.id.clone()), &self.hooks);
            self.bar.add_controller(bar.clone());
            self.grip.add_controller(grip.clone());
            *self.gestures.borrow_mut() = Some((bar, grip));
        }
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
    // Called through the grid's `set_float_labels`, so a new label reaches a bar already on screen.
    pub(super) fn relabel(&self, label: &dyn Fn(&ModuleId) -> String) {
        let drawn = self.current.borrow().as_ref().map(|(id, _)| id.clone());
        if let Some(id) = drawn {
            set_bar_text(&self.label, &label(&id));
        }
    }
}

/// A gesture's address is the module its chrome draws. All phases keep that address, even if the pool reuses
/// the chrome; only the controller remembers a drag or computes geometry from its offsets.
fn drag_gesture(on: PointerTarget, hooks: &ChromeHooks) -> gtk4::GestureDrag {
    let drag = gtk4::GestureDrag::new();
    drag.set_button(gtk4::gdk::BUTTON_PRIMARY);
    {
        let on = on.clone();
        let post = hooks.post.clone();
        drag.connect_drag_begin(move |gesture, x, y| {
            // The chrome owns this press; no module underneath it should act on it too.
            gesture.set_state(gtk4::EventSequenceState::Claimed);
            post(on.clone(), DragPhase::Begin, x, y, gesture.current_button());
        });
    }
    {
        let on = on.clone();
        let post = hooks.post.clone();
        drag.connect_drag_update(move |gesture, dx, dy| {
            post(on.clone(), DragPhase::Update, dx, dy, gesture.current_button());
        });
    }
    {
        let on = on.clone();
        let post = hooks.post.clone();
        drag.connect_cancel(move |gesture, _| {
            post(on.clone(), DragPhase::Cancel, 0.0, 0.0, gesture.current_button());
        });
    }
    let post = hooks.post.clone();
    drag.connect_drag_end(move |gesture, x, y| {
        post(on.clone(), DragPhase::End, x, y, gesture.current_button());
    });
    drag
}

/// The one way a bar's text is set: as text, never as markup (the module doc).
fn set_bar_text(label: &gtk4::Label, text: &str) {
    label.set_text(text);
}
