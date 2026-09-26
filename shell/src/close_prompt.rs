//! The window's y/n, tmux's `confirm-before`: the window close (session tabs spec §3.5, D11 A),
//! shown when a close would interrupt a running tab, and `prefix x`'s `kill-pane` (2026-09-26,
//! `kill_pane`). `y` runs what was asked; any other key cancels, as tmux's `confirm-before` does; a
//! bare modifier is neither.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk4::gdk::Key;
use gtk4::glib;
use gtk4::prelude::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PromptKey {
    Yes,
    No,
    Ignore,
}

pub(crate) fn classify(key: Key, is_modifier: bool) -> PromptKey {
    if is_modifier {
        PromptKey::Ignore
    } else if key == Key::y || key == Key::Y {
        PromptKey::Yes
    } else {
        PromptKey::No
    }
}

pub(crate) struct ClosePrompt {
    label: gtk4::Label,
    open: Cell<bool>,
    confirmed: Cell<bool>,
    window: gtk4::ApplicationWindow,
    /// What `y` runs; taken when the prompt closes either way.
    on_yes: RefCell<Option<Box<dyn FnOnce()>>>,
    /// The open prompt is the window close's: `y` sets `confirmed` before it closes again.
    closes_window: Cell<bool>,
}

impl ClosePrompt {
    /// Hidden until [`ClosePrompt::ask`]. Its capture controller on `window` sees keys before every
    /// module and the prefix, only while the prompt is open. Adding it after `prefix::install` is
    /// what makes that true: GTK runs a widget's controllers most-recently-added first, so this one
    /// -- added later -- sees a key before the prefix's own capture controller does.
    pub(crate) fn install(overlay: &gtk4::Overlay, window: &gtk4::ApplicationWindow) -> Rc<ClosePrompt> {
        let label = gtk4::Label::new(None);
        label.add_css_class("close-prompt");
        label.set_halign(gtk4::Align::Center);
        label.set_valign(gtk4::Align::Center);
        label.set_can_target(false);
        label.set_can_focus(false);
        label.set_visible(false);
        overlay.add_overlay(&label);
        let prompt = Rc::new(ClosePrompt {
            label,
            open: Cell::new(false),
            confirmed: Cell::new(false),
            window: window.clone(),
            on_yes: RefCell::new(None),
            closes_window: Cell::new(false),
        });
        let controller = gtk4::EventControllerKey::new();
        controller.set_propagation_phase(gtk4::PropagationPhase::Capture);
        {
            let prompt = Rc::downgrade(&prompt);
            controller.connect_key_pressed(move |event, key, _code, _state| {
                let Some(prompt) = prompt.upgrade() else {
                    return glib::Propagation::Proceed;
                };
                if !prompt.open.get() {
                    return glib::Propagation::Proceed;
                }
                let is_modifier = event
                    .current_event()
                    .and_then(|e| e.downcast::<gtk4::gdk::KeyEvent>().ok())
                    .is_some_and(|e| e.is_modifier());
                match classify(key, is_modifier) {
                    PromptKey::Ignore => return glib::Propagation::Proceed,
                    PromptKey::No => {
                        prompt.hide();
                        prompt.on_yes.borrow_mut().take();
                    }
                    PromptKey::Yes => {
                        prompt.hide();
                        if prompt.closes_window.get() {
                            prompt.confirmed.set(true);
                        }
                        // Taken out before it runs: what it does may ask again.
                        let on_yes = prompt.on_yes.borrow_mut().take();
                        if let Some(on_yes) = on_yes {
                            on_yes();
                        }
                    }
                }
                glib::Propagation::Stop
            });
        }
        window.add_controller(controller);
        prompt
    }

    /// Closes the window as a `y` to its own question would have: the close handler sees
    /// [`ClosePrompt::confirmed`] and does not ask. For a question already asked and answered in
    /// another form (`prefix x` on the last module, `kill_pane`).
    pub(crate) fn close_window_confirmed(&self) {
        self.confirmed.set(true);
        self.window.close();
    }

    pub(crate) fn confirmed(&self) -> bool {
        self.confirmed.get()
    }

    /// The window close's y/n: `y` closes the window again, and the close handler sees
    /// [`ClosePrompt::confirmed`].
    pub(crate) fn ask_to_close_window(&self, text: &str) {
        let window = self.window.clone();
        self.ask(text, move || window.close());
        self.closes_window.set(true);
    }

    /// Shows `text` and runs `on_yes` if the next key is `y`. A second ask while one is open
    /// replaces it.
    pub(crate) fn ask(&self, text: &str, on_yes: impl FnOnce() + 'static) {
        self.closes_window.set(false);
        *self.on_yes.borrow_mut() = Some(Box::new(on_yes));
        self.label.set_label(text);
        self.label.set_visible(true);
        self.open.set(true);
    }

    fn hide(&self) {
        self.label.set_visible(false);
        self.open.set(false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gtk4::gdk::Key;

    #[test]
    fn y_confirms_any_other_key_cancels_and_a_bare_modifier_waits() {
        assert_eq!(classify(Key::y, false), PromptKey::Yes);
        assert_eq!(classify(Key::Y, false), PromptKey::Yes);
        assert_eq!(classify(Key::n, false), PromptKey::No);
        assert_eq!(classify(Key::Escape, false), PromptKey::No);
        assert_eq!(classify(Key::Return, false), PromptKey::No, "Enter is not yes");
        assert_eq!(classify(Key::Shift_L, true), PromptKey::Ignore);
    }
}
