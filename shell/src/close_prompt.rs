//! The window's y/n, tmux's `confirm-before`: shown when a window close would interrupt a
//! running tab, and for `prefix x`'s `kill-pane`. `y` runs what was asked; any other key cancels,
//! as tmux's `confirm-before` does; a bare modifier is neither.

use std::rc::Rc;

use gtk4::glib;
use gtk4::graphene;
use gtk4::prelude::*;

use crate::gtk_host::keys::key_press;

pub(crate) use eitri_core::wm::close::{prompt_key, prompt_origin, PromptState};
#[allow(unused_imports)]
pub(crate) use eitri_core::wm::close::{runs_decline_hook, PromptKey};

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
        })
    }

    /// Closes the window as a `y` to its own question would have: the close handler sees
    /// [`ClosePrompt::confirmed`] and does not ask. For a question already asked and answered in
    /// another form (`prefix x` on the last module, `kill_pane`).
    pub(crate) fn close_window_confirmed(&self) {
        self.state.set_confirmed(true);
        self.window.close();
    }

    pub(crate) fn confirmed(&self) -> bool {
        self.state.confirmed()
    }

    /// Takes back a `y` the window close had, once the close hands the decision to nvim: from then
    /// on the answer travels with the quit (`kill_pane::EditorQuit::Window`), so a close after nvim
    /// exits asks again if what is running changed meanwhile (`kill_pane::close_is_confirmed`), and
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
    /// replaces it. ASSUMES the strip is empty/hidden while the prompt is open (the prefix is done
    /// by then) -- if the strip can still hold pieces, hide it while the prompt is open.
    pub(crate) fn ask(&self, text: &str, on_yes: impl FnOnce() + 'static) {
        self.show(text, Box::new(on_yes), false);
    }

    fn show(&self, text: &str, on_yes: Box<dyn FnOnce()>, closes_window: bool) {
        self.label.set_label(text);
        let strip_x = self
            .strip
            .compute_point(&self.overlay, &graphene::Point::new(0.0, 0.0))
            .map(|p| p.x().round() as i32)
            .unwrap_or(0);
        // Shown BEFORE it is measured: GTK measures a hidden widget as 0 high, which centred the
        // prompt's top edge on the bar's middle and hung half of it over the pane below (the
        // integration GUI pass, 2026-09-26; `tests/close_prompt_placement.rs`).
        self.label.set_visible(true);
        let label_height = self.label.measure(gtk4::Orientation::Vertical, -1).1;
        let (x, y) = prompt_origin(self.bar.is_visible(), strip_x, self.bar.height(), label_height.max(0));
        self.label.set_margin_start(x);
        self.label.set_margin_top(y);
        self.state.open(on_yes, closes_window);
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
            if !prompt.state.is_open() {
                return glib::Propagation::Proceed;
            }
            let is_modifier = event
                .current_event()
                .and_then(|e| e.downcast::<gtk4::gdk::KeyEvent>().ok())
                .is_some_and(|e| e.is_modifier());
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
