//! The window's y/n, tmux's `confirm-before`: the window close (session tabs spec §3.5, D11 A),
//! shown when a close would interrupt a running tab, and `prefix x`'s `kill-pane` (2026-09-26,
//! `kill_pane`). `y` runs what was asked; any other key cancels, as tmux's `confirm-before` does; a
//! bare modifier is neither.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk4::gdk::Key;
use gtk4::glib;
use gtk4::graphene;
use gtk4::prelude::*;

use crate::toast::TOAST_MARGIN;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PromptKey {
    Yes,
    No,
    Ignore,
}

/// Whether a `No` runs the window close's decline hook ([`ClosePrompt::connect_window_close_declined`]):
/// only for the window close's own question, never for `prefix x`'s.
pub(crate) fn runs_decline_hook(answer: PromptKey, closes_window: bool) -> bool {
    answer == PromptKey::No && closes_window
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

/// Where the prompt's top-left goes, in the overlay's coordinates: in the top bar, from the strip's
/// left edge and centred on the bar's height, while the bar is drawn; at the toast's corner margin
/// (top-left) when Immersive hides it.
pub(crate) fn prompt_origin(bar_shown: bool, strip_x: i32, bar_height: i32, label_height: i32) -> (i32, i32) {
    if bar_shown {
        (strip_x.max(0), ((bar_height - label_height) / 2).max(0))
    } else {
        (TOAST_MARGIN, TOAST_MARGIN)
    }
}

/// The y/n's state without its label: what an answer does (tmux's `confirm-before`). GTK-free, so
/// the decline hook's wiring is a test's, not only the GUI checklist's (the Opus review's T6-4: the
/// hook's call could be deleted with every test green while it sat inside the key controller).
#[derive(Default)]
pub(crate) struct PromptState {
    open: Cell<bool>,
    confirmed: Cell<bool>,
    /// What `y` runs; taken when the prompt closes either way.
    on_yes: RefCell<Option<Box<dyn FnOnce()>>>,
    /// The open prompt is the window close's: `y` sets `confirmed` before it closes again.
    closes_window: Cell<bool>,
    /// Runs when the window close's own question is answered with anything but `y`: the window
    /// stays, and what the close was about to end may already be gone (nvim exited on its own,
    /// R1-2). Set once by `main.rs`.
    on_window_close_declined: RefCell<Option<Rc<dyn Fn()>>>,
}

impl PromptState {
    /// Opens the prompt, replacing one already open: `y` runs `on_yes`, and `closes_window` marks it
    /// as the window close's own question.
    pub(crate) fn open(&self, on_yes: Box<dyn FnOnce()>, closes_window: bool) {
        *self.on_yes.borrow_mut() = Some(on_yes);
        self.closes_window.set(closes_window);
        self.open.set(true);
    }

    pub(crate) fn is_open(&self) -> bool {
        self.open.get()
    }

    /// `answer` while the prompt is open. `false`, changing nothing, when it is not open or the key
    /// is a bare modifier: the key goes on. Otherwise the prompt closes -- `hide` runs first, since
    /// what runs next may ask again -- and `y` runs what was asked (confirming a window close first),
    /// anything else cancels it and, for the window close's own question only, runs the decline hook.
    pub(crate) fn answer(&self, answer: PromptKey, hide: impl FnOnce()) -> bool {
        if !self.open.get() || answer == PromptKey::Ignore {
            return false;
        }
        self.open.set(false);
        hide();
        // Taken out before anything runs: what runs may ask again.
        let on_yes = self.on_yes.borrow_mut().take();
        match answer {
            PromptKey::Yes => {
                if self.closes_window.get() {
                    self.confirmed.set(true);
                }
                if let Some(on_yes) = on_yes {
                    on_yes();
                }
            }
            PromptKey::No | PromptKey::Ignore => {
                if runs_decline_hook(answer, self.closes_window.get()) {
                    // Cloned out first: the hook changes the layout, which may ask again.
                    let declined = self.on_window_close_declined.borrow().clone();
                    if let Some(declined) = declined {
                        declined();
                    }
                }
            }
        }
        true
    }
}

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
    /// Hidden until [`ClosePrompt::ask`]. Its capture controller on `window` sees keys before every
    /// module and the prefix, only while the prompt is open. Adding it after `prefix::install` is
    /// what makes that true: GTK runs a widget's controllers most-recently-added first, so this one
    /// -- added later -- sees a key before the prefix's own capture controller does.
    pub(crate) fn install(
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
        let prompt = Rc::new(ClosePrompt {
            label,
            state: PromptState::default(),
            window: window.clone(),
            bar: bar.clone(),
            strip: strip.clone(),
            overlay: overlay.clone(),
        });
        let controller = gtk4::EventControllerKey::new();
        controller.set_propagation_phase(gtk4::PropagationPhase::Capture);
        {
            let prompt = Rc::downgrade(&prompt);
            controller.connect_key_pressed(move |event, key, _code, _state| {
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
                    .answer(classify(key, is_modifier), || prompt.label.set_visible(false))
                {
                    glib::Propagation::Stop
                } else {
                    glib::Propagation::Proceed
                }
            });
        }
        window.add_controller(controller);
        prompt
    }

    /// Closes the window as a `y` to its own question would have: the close handler sees
    /// [`ClosePrompt::confirmed`] and does not ask. For a question already asked and answered in
    /// another form (`prefix x` on the last module, `kill_pane`).
    pub(crate) fn close_window_confirmed(&self) {
        self.state.confirmed.set(true);
        self.window.close();
    }

    pub(crate) fn confirmed(&self) -> bool {
        self.state.confirmed.get()
    }

    /// Takes back a `y` the window close had, once the close hands the decision to nvim: from then
    /// on the answer travels with the quit (`kill_pane::EditorQuit::Window`), so a close after nvim
    /// exits asks again if what is running changed meanwhile (`kill_pane::close_is_confirmed`), and
    /// a close after nvim's prompt was cancelled asks again at all.
    pub(crate) fn withdraw_confirmation(&self) {
        self.state.confirmed.set(false);
    }

    /// `hook` runs whenever the window close's own y/n is answered with anything but `y`.
    pub(crate) fn connect_window_close_declined(&self, hook: impl Fn() + 'static) {
        *self.state.on_window_close_declined.borrow_mut() = Some(Rc::new(hook));
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

    /// R1-2: only a `No` to the window close's own question reaches its decline hook -- not a `y`,
    /// not a bare modifier, and not a `No` to `prefix x`'s kill-pane question.
    #[test]
    fn only_a_declined_window_close_runs_the_decline_hook() {
        assert!(runs_decline_hook(PromptKey::No, true));
        assert!(!runs_decline_hook(PromptKey::Yes, true));
        assert!(!runs_decline_hook(PromptKey::Ignore, true));
        assert!(!runs_decline_hook(PromptKey::No, false));
    }

    /// The Opus review's T6-4 (its fifth mutation, the decline hook disabled): a `No` to the window
    /// close's own question hides the prompt and then runs the hook; a `y` confirms the close and
    /// runs what was asked; `prefix x`'s question declined runs no hook; a bare modifier leaves the
    /// prompt open; a key with no prompt open goes on untouched.
    #[test]
    fn the_prompts_answers_run_what_they_say() {
        let log: Rc<RefCell<Vec<&'static str>>> = Rc::default();
        let state = PromptState::default();
        {
            let log = log.clone();
            *state.on_window_close_declined.borrow_mut() = Some(Rc::new(move || log.borrow_mut().push("declined")));
        }
        let on_yes = |log: &Rc<RefCell<Vec<&'static str>>>| -> Box<dyn FnOnce()> {
            let log = log.clone();
            Box::new(move || log.borrow_mut().push("yes"))
        };
        let hide = |log: &Rc<RefCell<Vec<&'static str>>>| {
            let log = log.clone();
            move || log.borrow_mut().push("hide")
        };

        assert!(
            !state.answer(PromptKey::No, hide(&log)),
            "no prompt open: the key goes on"
        );
        assert!(log.borrow().is_empty());

        state.open(on_yes(&log), true);
        assert!(!state.answer(PromptKey::Ignore, hide(&log)), "a bare modifier waits");
        assert!(state.is_open());
        assert!(state.answer(PromptKey::No, hide(&log)));
        assert_eq!(*log.borrow(), ["hide", "declined"]);
        assert!(!state.confirmed.get() && !state.is_open());

        log.borrow_mut().clear();
        state.open(on_yes(&log), true);
        assert!(state.answer(PromptKey::Yes, hide(&log)));
        assert_eq!(*log.borrow(), ["hide", "yes"]);
        assert!(state.confirmed.get(), "the window close's y confirms it");

        log.borrow_mut().clear();
        state.confirmed.set(false);
        state.open(on_yes(&log), false);
        assert!(state.answer(PromptKey::No, hide(&log)));
        assert_eq!(*log.borrow(), ["hide"], "prefix x's question declined runs no hook");
        state.open(on_yes(&log), false);
        assert!(state.answer(PromptKey::Yes, hide(&log)));
        assert!(!state.confirmed.get(), "and its y confirms no window close");
    }

    /// Task 5(b): the prompt sits in the top bar's strip while the bar is drawn (the owner reads
    /// `prefix x`'s question where the strip just was), centred on the bar's height; and at the
    /// toast's own corner margin when Immersive hides the bar.
    #[test]
    fn prompt_origin_sits_in_the_strip_while_the_bar_is_shown_and_at_the_toast_corner_otherwise() {
        assert_eq!(prompt_origin(true, 100, 24, 14), (100, 5));
        // A label taller than the bar: the centring math would go negative, clamped to 0.
        assert_eq!(prompt_origin(true, 100, 10, 24), (100, 0));
        assert_eq!(prompt_origin(false, 100, 24, 14), (TOAST_MARGIN, TOAST_MARGIN));
        // A strip whose `compute_point` x came back negative (not yet laid out) clamps to 0.
        assert_eq!(prompt_origin(true, -5, 24, 14), (0, 5));
    }
}
