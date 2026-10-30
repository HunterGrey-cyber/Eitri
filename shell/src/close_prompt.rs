//! The window's y/n, tmux's `confirm-before`: `y` runs what was asked, any other key cancels, as tmux's
//! `confirm-before` does, and a bare modifier is neither.
//!
//! In the main window it is the controller's question -- a window close that would interrupt a running tab or a
//! terminal's job, `prefix x`'s `kill-pane` -- drawn on the window's overlay from its chrome model
//! (`gtk_host::chrome::reconcile` shows, places and hides it). There the overlay has no question or answer of its
//! own: while the question shows, every key but a bare modifier goes to the controller's window stage, which answers
//! it. Built with [`ClosePrompt::new`], it holds no window, so it cannot ask anything itself.
//!
//! The companion window has no controller: its prompt ([`ClosePrompt::install`]) asks its own window close's
//! question ([`ClosePrompt::ask_to_close_window`]) and answers it here.

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};

use eitri_core::wm::close::{prompt_key, prompt_origin, PromptKey};
use eitri_core::wm::{Controller, Driver, KeyStage, Verdict, WinId};
use gtk4::glib;
use gtk4::graphene;
use gtk4::prelude::*;

use crate::gtk_host::chrome::PromptMeasures;
use crate::gtk_host::keys::key_press;

/// The companion window's own window-close question: whether it is up, and whether a `y` confirmed the close.
#[derive(Default)]
struct OwnClose {
    open: Cell<bool>,
    confirmed: Cell<bool>,
}

impl OwnClose {
    /// What `key` does to the question: `None` when it is not up or the key is a bare modifier, and the key goes on;
    /// otherwise the question goes, and `Some(true)` for a `y`, which confirms the close, `Some(false)` for any other
    /// key, which cancels it.
    fn answer(&self, key: PromptKey) -> Option<bool> {
        if !self.open.get() || key == PromptKey::Ignore {
            return None;
        }
        self.open.set(false);
        let yes = key == PromptKey::Yes;
        if yes {
            self.confirmed.set(true);
        }
        Some(yes)
    }
}

pub(crate) struct ClosePrompt {
    label: gtk4::Label,
    /// The controller's question shows: its keys are the controller's.
    open: Cell<bool>,
    /// The window whose close the prompt asks about itself: the companion's alone.
    window: Option<glib::WeakRef<gtk4::ApplicationWindow>>,
    own: OwnClose,
    /// The top bar: hidden in Immersive mode (`prompt_origin`'s `bar_shown`).
    bar: gtk4::Widget,
    /// The prefix strip: the prompt's left edge sits at its position while the bar is shown.
    strip: gtk4::Widget,
    /// The overlay both the label and `strip` are positioned within, for `compute_point`.
    overlay: gtk4::Overlay,
    /// The controller whose window stage answers its question.
    driver: RefCell<Weak<Driver<Controller>>>,
}

impl ClosePrompt {
    /// Hidden until [`ClosePrompt::show_model`], with its capture controller on `window` added at once: for a window
    /// whose prefix was installed before it, which is what makes its capture see a key before the prefix's (GTK
    /// runs a widget's controllers most-recently-added first). The main window builds its prompt with
    /// [`ClosePrompt::new`] and adds the capture where its declared key layers say
    /// (`gtk_host::keys::install_window_layers`).
    pub(crate) fn install(
        overlay: &gtk4::Overlay,
        window: &gtk4::ApplicationWindow,
        bar: &gtk4::Widget,
        strip: &gtk4::Widget,
    ) -> Rc<ClosePrompt> {
        let prompt = ClosePrompt::build(overlay, Some(window), bar, strip);
        crate::gtk_host::keys::PromptLayer::add_capture(&prompt, window.upcast_ref());
        prompt
    }

    /// Hidden until [`ClosePrompt::show_model`], with no capture yet: its key layer is added separately
    /// ([`PromptLayer::add_capture`](crate::gtk_host::keys::PromptLayer::add_capture)), and sees keys before every
    /// module and the prefix, only while the question shows.
    pub(crate) fn new(overlay: &gtk4::Overlay, bar: &gtk4::Widget, strip: &gtk4::Widget) -> Rc<ClosePrompt> {
        ClosePrompt::build(overlay, None, bar, strip)
    }

    fn build(
        overlay: &gtk4::Overlay,
        window: Option<&gtk4::ApplicationWindow>,
        bar: &gtk4::Widget,
        strip: &gtk4::Widget,
    ) -> Rc<ClosePrompt> {
        let label = gtk4::Label::new(None);
        label.add_css_class("close-prompt");
        label.set_halign(gtk4::Align::Start);
        label.set_valign(gtk4::Align::Start);
        label.set_can_target(false);
        label.set_can_focus(false);
        label.set_visible(false);
        overlay.add_overlay(&label);
        Rc::new(ClosePrompt {
            label,
            open: Cell::new(false),
            window: window.map(|window| window.downgrade()),
            own: OwnClose::default(),
            bar: bar.clone(),
            strip: strip.clone(),
            overlay: overlay.clone(),
            driver: RefCell::new(Weak::new()),
        })
    }

    /// The controller whose questions this overlay shows: their keys go to its window stage.
    pub(crate) fn set_driver(&self, driver: Weak<Driver<Controller>>) {
        *self.driver.borrow_mut() = driver;
    }

    /// Shows the controller's question, `text`, as plain text, where it was last placed; the chrome's reconcile
    /// places it from [`ClosePrompt::measures`] right after. Shown before it is measured: GTK measures a hidden
    /// widget as 0 high, which centred the prompt's top edge on the bar's middle and hung half of it over the pane
    /// below (`tests/close_prompt_placement.rs`).
    pub(crate) fn show_model(&self, text: &str) {
        self.open.set(true);
        self.label.set_label(text);
        self.label.set_visible(true);
    }

    /// What the question is placed by: the bar, the strip's left edge in the overlay, and the label's height as it
    /// holds its text. Read once it shows, since GTK measures a hidden widget as 0 high.
    pub(crate) fn measures(&self) -> PromptMeasures {
        let strip_x = self
            .strip
            .compute_point(&self.overlay, &graphene::Point::new(0.0, 0.0))
            .map(|p| p.x().round() as i32)
            .unwrap_or(0);
        PromptMeasures {
            bar_shown: self.bar.is_visible(),
            strip_x,
            bar_height: self.bar.height(),
            label_height: self.label.measure(gtk4::Orientation::Vertical, -1).1,
        }
    }

    /// Moves the question to its origin in the overlay.
    pub(crate) fn place(&self, x: i32, y: i32) {
        self.label.set_margin_start(x);
        self.label.set_margin_top(y);
    }

    /// The controller's question is answered or gone.
    pub(crate) fn hide_model(&self) {
        self.open.set(false);
        self.label.set_visible(false);
    }

    /// The companion window's close question: `y` closes the window again, and its close handler then sees
    /// [`ClosePrompt::confirmed`]. A prompt built without a window (the main window's) asks nothing.
    pub(crate) fn ask_to_close_window(&self, text: &str) {
        if self.window.is_none() {
            eprintln!("[window] BUG: a close question asked of a prompt with no window of its own");
            return;
        }
        self.show_model(text);
        let at = self.measures();
        let (x, y) = prompt_origin(at.bar_shown, at.strip_x, at.bar_height, at.label_height.max(0));
        self.place(x, y);
        self.open.set(false);
        self.own.open.set(true);
    }

    /// Whether a `y` to [`ClosePrompt::ask_to_close_window`] confirmed the window close.
    pub(crate) fn confirmed(&self) -> bool {
        self.own.confirmed.get()
    }
}

/// The prompt's key layer: a capture controller that hands the key to the controller's window stage while the
/// question shows and lets every key through while it does not.
impl crate::gtk_host::keys::PromptLayer for Rc<ClosePrompt> {
    fn add_capture(&self, window: &gtk4::Window) {
        let controller = gtk4::EventControllerKey::new();
        controller.set_propagation_phase(gtk4::PropagationPhase::Capture);
        let prompt = Rc::downgrade(self);
        controller.connect_key_pressed(move |event, key, code, state| {
            let Some(prompt) = prompt.upgrade() else {
                return glib::Propagation::Proceed;
            };
            let is_modifier = event
                .current_event()
                .and_then(|e| e.downcast::<gtk4::gdk::KeyEvent>().ok())
                .is_some_and(|e| e.is_modifier());
            let press = key_press(key, code, state);
            if let Some(yes) = prompt.own.answer(prompt_key(&press, is_modifier, false)) {
                prompt.label.set_visible(false);
                if yes {
                    if let Some(window) = prompt.window.as_ref().and_then(|window| window.upgrade()) {
                        window.close();
                    }
                }
                return glib::Propagation::Stop;
            }
            // A bare modifier answers nothing and goes on to the prefix's layer, which hands it to the same stage
            // once.
            if !prompt.open.get() || is_modifier {
                return glib::Propagation::Proceed;
            }
            let Some(driver) = prompt.driver.borrow().upgrade() else {
                return glib::Propagation::Proceed;
            };
            // Every other key answers the question, and is the window stage's once: going on to the prefix's layer,
            // it would be decided a second time.
            let verdict = driver.key(WinId::MAIN, &KeyStage::Window, press);
            if verdict != Verdict::Consumed {
                eprintln!("[wm] the y/n's key was not taken by the window stage ({verdict:?}); it goes no further");
            }
            glib::Propagation::Stop
        });
        window.add_controller(controller);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The companion's own question waits through a bare modifier, and goes with the next key: `y` confirms the
    /// close, anything else cancels it and leaves it unconfirmed; with no question up, every key goes on.
    #[test]
    fn the_companions_close_question_confirms_on_y_and_goes_on_any_other_key() {
        let own = OwnClose::default();
        assert_eq!(own.answer(PromptKey::Yes), None, "no question up: the key goes on");
        own.open.set(true);
        assert_eq!(own.answer(PromptKey::Ignore), None, "a bare modifier waits");
        assert_eq!(own.answer(PromptKey::No), Some(false));
        assert!(!own.confirmed.get(), "a cancel confirms nothing");
        assert_eq!(own.answer(PromptKey::No), None, "the cancel took the question down");
        own.open.set(true);
        assert_eq!(own.answer(PromptKey::Yes), Some(true));
        assert!(own.confirmed.get(), "y confirms the close");
    }
}
