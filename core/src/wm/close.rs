//! Close prompt key rules, placement and callback ordering.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use super::KeyPress;
use crate::keymap::KeyName;

/// How far the window's overlay labels sit from the edge they are drawn over: the toast always, and
/// this prompt at its corner when the top bar is hidden (Immersive). One value, so the two stay aligned.
pub const TOAST_MARGIN: i32 = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptKey {
    Yes,
    No,
    Ignore,
}

/// Only declining the window close's own question runs its decline hook.
pub fn runs_decline_hook(answer: PromptKey, closes_window: bool) -> bool {
    answer == PromptKey::No && closes_window
}

/// A bare modifier waits; either case of y confirms, regardless of its modifiers.
pub fn prompt_key(press: &KeyPress, is_modifier: bool) -> PromptKey {
    if is_modifier {
        PromptKey::Ignore
    } else if matches!(press.key, Some(KeyName::Char('y' | 'Y'))) {
        PromptKey::Yes
    } else {
        PromptKey::No
    }
}

/// Where the prompt's top-left goes, in the overlay's coordinates: in the top bar, from the strip's
/// left edge and centred on the bar's height, while the bar is drawn; at the toast's corner margin
/// (top-left) when Immersive hides it.
pub fn prompt_origin(bar_shown: bool, strip_x: i32, bar_height: i32, label_height: i32) -> (i32, i32) {
    if bar_shown {
        (strip_x.max(0), ((bar_height - label_height) / 2).max(0))
    } else {
        (TOAST_MARGIN, TOAST_MARGIN)
    }
}

/// The prompt state is separate from its label so callbacks can be checked without a display.
#[derive(Default)]
pub struct PromptState {
    open: Cell<bool>,
    confirmed: Cell<bool>,
    /// What `y` runs; taken when the prompt closes either way.
    on_yes: RefCell<Option<Box<dyn FnOnce()>>>,
    /// The open prompt is the window close's: `y` sets `confirmed` before it closes again.
    closes_window: Cell<bool>,
    /// Runs when the window close's own question is answered with anything but `y`: the window
    /// stays, and what the close was about to end may already be gone.
    on_window_close_declined: RefCell<Option<Rc<dyn Fn()>>>,
}

impl PromptState {
    /// Whether a yes has confirmed the window close.
    pub fn confirmed(&self) -> bool {
        self.confirmed.get()
    }

    /// Changes the confirmation when a close is handed to or returned from the editor.
    pub fn set_confirmed(&self, confirmed: bool) {
        self.confirmed.set(confirmed);
    }

    /// Registers the callback for a declined window close.
    pub fn connect_window_close_declined(&self, hook: impl Fn() + 'static) {
        *self.on_window_close_declined.borrow_mut() = Some(Rc::new(hook));
    }

    /// Opens the prompt, replacing one already open: `y` runs `on_yes`, and `closes_window` marks it
    /// as the window close's own question.
    pub fn open(&self, on_yes: Box<dyn FnOnce()>, closes_window: bool) {
        *self.on_yes.borrow_mut() = Some(on_yes);
        self.closes_window.set(closes_window);
        self.open.set(true);
    }

    pub fn is_open(&self) -> bool {
        self.open.get()
    }

    /// `answer` while the prompt is open. `false`, changing nothing, when it is not open or the key
    /// is a bare modifier: the key goes on. Otherwise the prompt closes -- `hide` runs first, since
    /// what runs next may ask again -- and `y` runs what was asked (confirming a window close first),
    /// anything else cancels it and, for the window close's own question only, runs the decline hook.
    pub fn answer(&self, answer: PromptKey, hide: impl FnOnce()) -> bool {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        keymap::{prefix::Press, KeyName},
        wm::{KeyPress, Mods},
    };

    fn press(key: Option<KeyName>) -> KeyPress {
        KeyPress {
            keycode: 42,
            press: Press::Other,
            key,
            mods: Mods::default(),
            super_or_hyper: false,
            super_or_meta: false,
        }
    }

    #[test]
    fn y_confirms_any_other_key_cancels_and_a_bare_modifier_waits() {
        assert_eq!(prompt_key(&press(Some(KeyName::Char('y'))), false), PromptKey::Yes);
        assert_eq!(prompt_key(&press(Some(KeyName::Char('Y'))), false), PromptKey::Yes);
        assert_eq!(prompt_key(&press(Some(KeyName::Char('n'))), false), PromptKey::No);
        assert_eq!(prompt_key(&press(None), false), PromptKey::No);
        assert_eq!(
            prompt_key(&press(Some(KeyName::Enter)), false),
            PromptKey::No,
            "Enter is not yes"
        );
        assert_eq!(prompt_key(&press(None), true), PromptKey::Ignore);
    }

    /// Only a `No` to the window close's own question reaches its decline hook -- not a `y`,
    /// not a bare modifier, and not a `No` to `prefix x`'s kill-pane question.
    #[test]
    fn only_a_declined_window_close_runs_the_decline_hook() {
        assert!(runs_decline_hook(PromptKey::No, true));
        assert!(!runs_decline_hook(PromptKey::Yes, true));
        assert!(!runs_decline_hook(PromptKey::Ignore, true));
        assert!(!runs_decline_hook(PromptKey::No, false));
    }

    /// Answers hide the prompt before running a callback, which may open another prompt.
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

    /// The prompt stays in the strip or at the corner when the bar is hidden.
    #[test]
    fn prompt_origin_sits_in_the_strip_while_the_bar_is_shown_and_at_the_toast_corner_otherwise() {
        assert_eq!(prompt_origin(true, 100, 24, 14), (100, 5));
        // A label taller than the bar: the centring math would go negative, clamped to 0.
        assert_eq!(prompt_origin(true, 100, 10, 24), (100, 0));
        assert_eq!(prompt_origin(false, 100, 24, 14), (TOAST_MARGIN, TOAST_MARGIN));
        // A strip whose `compute_point` x came back negative (not yet laid out) clamps to 0.
        assert_eq!(prompt_origin(true, -5, 24, 14), (0, 5));
    }
    #[test]
    fn ctrl_shift_y_answers_the_close_prompt() {
        let mut press = press(Some(KeyName::Char('y')));
        press.mods.ctrl = true;
        press.mods.shift = true;
        assert_eq!(press.press, Press::Other);
        assert_eq!(prompt_key(&press, false), PromptKey::Yes);
    }
}
