//! The window-close y/n (session tabs spec §3.5, D11 A): shown over the window when a close would
//! interrupt a running tab. `y` closes again with no prompt; any other key cancels, as tmux's
//! `confirm-before` does; a bare modifier is neither.

use std::cell::Cell;
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
        });
        let controller = gtk4::EventControllerKey::new();
        controller.set_propagation_phase(gtk4::PropagationPhase::Capture);
        {
            let prompt = prompt.clone();
            let window = window.clone();
            controller.connect_key_pressed(move |event, key, _code, _state| {
                if !prompt.open.get() {
                    return glib::Propagation::Proceed;
                }
                let is_modifier = event
                    .current_event()
                    .and_then(|e| e.downcast::<gtk4::gdk::KeyEvent>().ok())
                    .is_some_and(|e| e.is_modifier());
                match classify(key, is_modifier) {
                    PromptKey::Ignore => return glib::Propagation::Proceed,
                    PromptKey::No => prompt.hide(),
                    PromptKey::Yes => {
                        prompt.confirmed.set(true);
                        prompt.hide();
                        window.close();
                    }
                }
                glib::Propagation::Stop
            });
        }
        window.add_controller(controller);
        prompt
    }

    pub(crate) fn confirmed(&self) -> bool {
        self.confirmed.get()
    }

    pub(crate) fn ask(&self, text: &str) {
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
