//! The window's toast draws a plain label without taking clicks or keys. It stays below the
//! top bar so the window controls and the attention chip remain visible.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

// The toast's distance from the edge of what it sits over is core's, because `close_prompt`'s corner margin in
// Immersive is the same one: the two overlay labels share it. How long a toast stays up is core's too, since the
// window's chrome model times the toasts it shows.
use eitri_core::wm::chrome::{TOAST_FOR, WARNING_FOR};
use eitri_core::wm::close::TOAST_MARGIN;
use eitri_core::wm::toasts::{after_timed, toast_top, ToastKind};
use gtk4::glib;
use gtk4::prelude::*;

/// How long a one-time notice stays up: it is a sentence read once, not a glance at a count.
pub(crate) const NOTICE_FOR: Duration = Duration::from_secs(12);

/// How long what `init.lua` got wrong stays up: a few lines to read and then find in the file.
pub(crate) const PROBLEM_FOR: Duration = Duration::from_secs(40);

/// How often a waiting notice looks again, and how long it waits at most: what `init.lua` got wrong is up for
/// [`PROBLEM_FOR`], and two [`NOTICE_FOR`] notices wait behind it, so all three back to back fit.
const NOTICE_WAIT_STEP: Duration = Duration::from_millis(500);
const NOTICE_WAIT_MAX: Duration = Duration::from_secs(75);

pub(crate) struct Toast {
    label: gtk4::Label,
    /// The top bar: the toast goes below it while it is drawn ([`toast_top`]).
    bar: gtk4::Widget,
    hide: RefCell<Option<glib::SourceId>>,
    /// What the label says while it is up; `None` once it has hidden.
    showing: Cell<Option<ToastKind>>,
    /// The note that stays up between timed lines, while what it says holds.
    standing: RefCell<Option<String>>,
}

impl Toast {
    /// The toast, as an overlay child of `overlay` (the window's HINT overlay, which already wraps
    /// the whole window, top bar included), kept below `bar`. Hidden until [`Toast::show`].
    pub(crate) fn install(overlay: &gtk4::Overlay, bar: &gtk4::Widget) -> Rc<Toast> {
        let label = gtk4::Label::new(None);
        label.add_css_class("module-toast");
        label.set_halign(gtk4::Align::End);
        label.set_valign(gtk4::Align::Start);
        label.set_margin_top(TOAST_MARGIN);
        label.set_margin_end(TOAST_MARGIN);
        // A sentence longer than the window wraps inside it instead of running past its left edge: the label takes
        // the room the overlay has, never more, and keeps a margin on both sides.
        label.set_margin_start(TOAST_MARGIN);
        label.set_wrap(true);
        label.set_wrap_mode(gtk4::pango::WrapMode::WordChar);
        label.set_xalign(0.0);
        label.set_can_target(false);
        label.set_can_focus(false);
        label.set_visible(false);
        overlay.add_overlay(&label);
        Rc::new(Toast {
            label,
            bar: bar.clone(),
            hide: RefCell::new(None),
            showing: Cell::new(None),
            standing: RefCell::new(None),
        })
    }

    pub(crate) fn show(self: &Rc<Self>, text: &str) {
        self.show_for(text, TOAST_FOR);
    }

    pub(crate) fn show_for(self: &Rc<Self>, text: &str, duration: Duration) {
        self.show_kind(text, duration, ToastKind::Other);
    }

    /// A one-time notice that waits for what is up now to hide, so it cannot cut another notice short: checked every
    /// [`NOTICE_WAIT_STEP`] for at most [`NOTICE_WAIT_MAX`], then shown over whatever is there. A standing note does
    /// not hold it back (a timed line covers that one and gives it back).
    pub(crate) fn show_notice_when_free(self: &Rc<Self>, text: String) {
        self.show_notice_when_free_for(text, NOTICE_FOR);
    }

    /// [`Toast::show_notice_when_free`] for a notice that stays up `duration`.
    pub(crate) fn show_notice_when_free_for(self: &Rc<Self>, text: String, duration: Duration) {
        self.show_notice_after(text, duration, NOTICE_WAIT_MAX, None);
    }

    /// [`Toast::show_notice_when_free`] that runs `shown` the moment the notice goes up, so a record that it was said
    /// is made then and not while the notice still waits.
    pub(crate) fn show_notice_when_free_then(self: &Rc<Self>, text: String, shown: impl FnOnce() + 'static) {
        self.show_notice_after(text, NOTICE_FOR, NOTICE_WAIT_MAX, Some(Box::new(shown)));
    }

    fn show_notice_after(
        self: &Rc<Self>,
        text: String,
        duration: Duration,
        patience: Duration,
        shown: Option<Box<dyn FnOnce()>>,
    ) {
        let busy = matches!(
            self.showing.get(),
            Some(ToastKind::Other | ToastKind::Attention | ToastKind::Warning)
        );
        if !busy || patience < NOTICE_WAIT_STEP {
            self.show_for(&text, duration);
            if let Some(shown) = shown {
                shown();
            }
            return;
        }
        let toast = Rc::downgrade(self);
        glib::timeout_add_local_once(NOTICE_WAIT_STEP, move || {
            if let Some(toast) = toast.upgrade() {
                toast.show_notice_after(text, duration, patience - NOTICE_WAIT_STEP, shown);
            }
        });
    }

    /// A hidden chat's waiting card (`eitri_core::wm::toasts::permission_toast_text`), for its [`TOAST_FOR`]. The
    /// window's chrome model keeps it up against refusals: a refusal meanwhile shows no toast.
    pub(crate) fn show_attention(self: &Rc<Self>, text: &str) {
        self.show_kind(text, TOAST_FOR, ToastKind::Attention);
    }

    /// A line that says something may be lost, for [`WARNING_FOR`].
    pub(crate) fn show_warning(self: &Rc<Self>, text: &str) {
        self.show_kind(text, WARNING_FOR, ToastKind::Warning);
    }

    /// A note that stays up until it is set again: `Some` shows it now and after every timed line, `None` takes
    /// it down (a timed line up now still runs its time).
    pub(crate) fn set_standing(self: &Rc<Self>, text: Option<&str>) {
        *self.standing.borrow_mut() = text.map(str::to_string);
        match text {
            Some(text) => {
                if let Some(source) = self.hide.borrow_mut().take() {
                    source.remove();
                }
                self.place(text, ToastKind::Standing);
            }
            None if self.showing.get() == Some(ToastKind::Standing) => {
                self.showing.set(None);
                self.label.set_visible(false);
            }
            None => {}
        }
    }

    /// Puts `text` up, below the top bar while it is drawn.
    fn place(&self, text: &str, kind: ToastKind) {
        // A toast raised while the window is still being built (the tmux import's notice) comes
        // before the bar has a height; its natural height is the one it is about to get.
        let bar_height = match self.bar.height() {
            0 => self.bar.measure(gtk4::Orientation::Vertical, -1).1,
            allocated => allocated,
        };
        self.label.set_margin_top(toast_top(self.bar.is_visible(), bar_height));
        self.label.set_label(text);
        self.label.set_visible(true);
        self.showing.set(Some(kind));
    }

    fn show_kind(self: &Rc<Self>, text: &str, duration: Duration, kind: ToastKind) {
        self.place(text, kind);
        if let Some(source) = self.hide.borrow_mut().take() {
            source.remove();
        }
        let toast = Rc::downgrade(self);
        let source = glib::timeout_add_local_once(duration, move || {
            if let Some(toast) = toast.upgrade() {
                toast.hide.borrow_mut().take();
                let standing = toast.standing.borrow().clone();
                match after_timed(standing.as_deref()) {
                    Some((text, kind)) => toast.place(text, kind),
                    None => {
                        toast.showing.set(None);
                        toast.label.set_visible(false);
                    }
                }
            }
        });
        *self.hide.borrow_mut() = Some(source);
    }
}

#[cfg(test)]
mod tests {
    /// A toast sentence is longer than a narrow window (the tmux import's notice is about 130 characters): the label
    /// wraps inside the overlay and keeps a margin on its left, so no part of it is drawn past the window's edge.
    /// Read off the code (comments and this module blanked); where it lands in a 700 px window is the GUI pass's.
    #[test]
    fn the_toast_label_wraps_and_keeps_a_margin_on_both_sides() {
        use eitri_core::source_scan as scan;
        let source = scan::read_required("shell/src/toast.rs");
        let code = scan::code_only(&source.text);
        let code = scan::without(&code, &scan::modules(&code, "tests"));
        let flat: String = code.split_whitespace().collect();
        for wanted in [
            "label.set_wrap(true)",
            "label.set_wrap_mode(gtk4::pango::WrapMode::WordChar)",
            "label.set_margin_start(TOAST_MARGIN)",
            "label.set_margin_end(TOAST_MARGIN)",
        ] {
            assert!(flat.contains(wanted), "toast.rs lacks {wanted}");
        }
    }
}
