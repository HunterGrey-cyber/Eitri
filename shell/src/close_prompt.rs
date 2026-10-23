//! The window's y/n, tmux's `confirm-before`: shown when a window close would interrupt a
//! running tab, and for `prefix x`'s `kill-pane`. `y` runs what was asked; any other key cancels,
//! as tmux's `confirm-before` does; a bare modifier is neither.
//!
//! Two questions share the overlay. The window close's is this file's own: its text, its answer and what `y` runs.
//! `prefix x`'s is the controller's: its text comes from the chrome model, drawn here through
//! [`ClosePrompt::show_model`], and while it shows every key goes to the controller's window stage, which answers it.
//! One shows at a time, the newest: the other is withdrawn, so no key answers a question that is no longer on screen.

use std::cell::RefCell;
use std::rc::{Rc, Weak};

use eitri_core::wm::close::{prompt_key, prompt_origin, PromptState};
use eitri_core::wm::{Controller, Driver, HostEvent, KeyStage, Verdict, WinId};
use gtk4::glib;
use gtk4::graphene;
use gtk4::prelude::*;

use crate::gtk_host::chrome::PromptMeasures;
use crate::gtk_host::keys::key_press;

pub(crate) struct ClosePrompt {
    label: gtk4::Label,
    state: PromptState,
    window: gtk4::ApplicationWindow,
    /// The top bar: hidden in Immersive mode ([`prompt_origin`]'s `bar_shown`).
    bar: gtk4::Widget,
    /// The prefix strip: the prompt's left edge sits at its position while the bar is shown.
    strip: gtk4::Widget,
    /// The overlay both the label and `strip` are positioned within, for `compute_point`.
    overlay: gtk4::Overlay,
    /// The controller whose window stage answers its question.
    driver: RefCell<Weak<Driver<Controller>>>,
}

impl ClosePrompt {
    /// Hidden until [`ClosePrompt::ask`], with its capture controller on `window` added at once: for a window
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
        let prompt = ClosePrompt::new(overlay, window, bar, strip);
        crate::gtk_host::keys::PromptLayer::add_capture(&prompt, window.upcast_ref());
        prompt
    }

    /// Hidden until [`ClosePrompt::ask`], with no capture yet: its key layer is added separately
    /// ([`PromptLayer::add_capture`](crate::gtk_host::keys::PromptLayer::add_capture)), and sees keys before every
    /// module and the prefix, only while the prompt is open.
    pub(crate) fn new(
        overlay: &gtk4::Overlay,
        window: &gtk4::ApplicationWindow,
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
            state: PromptState::default(),
            window: window.clone(),
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
    /// places it from [`ClosePrompt::measures`] right after. A window close's question it replaces is withdrawn,
    /// and the window stays as a `no` would have left it.
    pub(crate) fn show_model(&self, text: &str) {
        if let Some(declined) = self.state.show_model() {
            // On the loop's next turn: this draw is part of a batch being applied, and the hook changes the layout.
            glib::idle_add_local_once(move || declined());
        }
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

    /// The controller's question is answered or gone. The window close's, if one is open, keeps the overlay.
    pub(crate) fn hide_model(&self) {
        if self.state.hide_model() {
            self.label.set_visible(false);
        }
    }

    /// Closes the window as a `y` to its own question would have: the close handler sees
    /// [`ClosePrompt::confirmed`] and does not ask. For a question already asked and answered in
    /// another form (`prefix x` on the last module, `eitri_core::wm::close`).
    pub(crate) fn close_window_confirmed(&self) {
        self.state.set_confirmed(true);
        self.window.close();
    }

    pub(crate) fn confirmed(&self) -> bool {
        self.state.confirmed()
    }

    /// Takes back a `y` the window close had, once the close hands the decision to nvim: from then
    /// on the answer travels with the quit (`eitri_core::wm::close::EditorQuit::Window`), so a close after nvim
    /// exits asks again if what is running changed meanwhile (`eitri_core::wm::close::close_is_confirmed`), and
    /// a close after nvim's prompt was cancelled asks again at all.
    pub(crate) fn withdraw_confirmation(&self) {
        self.state.set_confirmed(false);
    }

    /// `hook` runs whenever the window close's own y/n is answered with anything but `y`.
    pub(crate) fn connect_window_close_declined(&self, hook: impl Fn() + 'static) {
        self.state.connect_window_close_declined(hook);
    }

    /// The window close's y/n: `y` closes the window again, and the close handler sees
    /// [`ClosePrompt::confirmed`].
    pub(crate) fn ask_to_close_window(&self, text: &str) {
        let window = self.window.clone();
        self.show(text, Box::new(move || window.close()), true);
    }

    /// Shows `text` and runs `on_yes` if the next key is `y`. A second ask while one is open
    /// replaces it, the controller's question too. ASSUMES the strip is empty/hidden while the prompt is open (the prefix is done
    /// by then) -- if the strip can still hold pieces, hide it while the prompt is open.
    pub(crate) fn ask(&self, text: &str, on_yes: impl FnOnce() + 'static) {
        self.show(text, Box::new(on_yes), false);
    }

    fn show(&self, text: &str, on_yes: Box<dyn FnOnce()>, closes_window: bool) {
        self.label.set_label(text);
        // Shown BEFORE it is measured: GTK measures a hidden widget as 0 high, which centred the
        // prompt's top edge on the bar's middle and hung half of it over the pane below
        // (`tests/close_prompt_placement.rs`).
        self.label.set_visible(true);
        let at = self.measures();
        let (x, y) = prompt_origin(at.bar_shown, at.strip_x, at.bar_height, at.label_height.max(0));
        self.place(x, y);
        if self.state.open(on_yes, closes_window) {
            // The controller's question was on the overlay: withdrawn there too, or its next key would answer it.
            let driver = self.driver.borrow().upgrade();
            if let Some(driver) = driver {
                driver.post(HostEvent::PromptWithdrawn { win: WinId::MAIN });
            }
        }
    }
}

/// The prompt's key layer: a capture controller that answers the key while the prompt is open and lets every key
/// through while it is not.
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
            if !prompt.state.is_open() {
                // The controller's question: its window stage answers it before the prefix sees the key. A bare
                // modifier answers nothing and goes on to the prefix's layer, which hands it to the same stage once.
                if !prompt.state.model_open() || is_modifier {
                    return glib::Propagation::Proceed;
                }
                let Some(driver) = prompt.driver.borrow().upgrade() else {
                    return glib::Propagation::Proceed;
                };
                // Every other key answers the question, and is the window stage's once: going on to the prefix's
                // layer, it would be decided a second time.
                let verdict = driver.key(WinId::MAIN, &KeyStage::Window, key_press(key, code, state));
                if verdict != Verdict::Consumed {
                    eprintln!("[wm] the y/n's key was not taken by the window stage ({verdict:?}); it goes no further");
                }
                return glib::Propagation::Stop;
            }
            if prompt
                .state
                .answer(prompt_key(&key_press(key, code, state), is_modifier), || {
                    prompt.label.set_visible(false)
                })
            {
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        });
        window.add_controller(controller);
    }
}
