//! The `Ctrl+a` prefix, copied from the owner's tmux (spec:
//! docs/superpowers/specs/2026-09-19-window-modes-design.md §3). `Ctrl+a`, then `m`/`z` zoom,
//! `h`/`j`/`k`/`l` resize by 5 cells, `Ctrl+a` hands a real `Ctrl+a` to the pane.
//!
//! A window-level capture controller, added at startup. GTK runs a widget's controllers
//! most-recently-added first, and the window's shortcut manager was added when the window was
//! created, so this sees every key before any app accelerator and before the editor, the panel or
//! the top bar; HINT's controller, added when a HINT starts, sees keys before this one (§3.3).

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gtk4::gdk::{Key, ModifierType};
use gtk4::glib;
use gtk4::prelude::*;

use crate::layout::Direction;

/// tmux's default `repeat-time`; the owner's config does not set it (§1).
pub(crate) const REPEAT_TIME: Duration = Duration::from_millis(500);

/// A bare modifier is not "the next key" (§3.1).
const MODIFIER_KEYS: [Key; 16] = [
    Key::Shift_L,
    Key::Shift_R,
    Key::Control_L,
    Key::Control_R,
    Key::Alt_L,
    Key::Alt_R,
    Key::Super_L,
    Key::Super_R,
    Key::Meta_L,
    Key::Meta_R,
    Key::Hyper_L,
    Key::Hyper_R,
    Key::ISO_Level3_Shift,
    Key::ISO_Level5_Shift,
    Key::Caps_Lock,
    Key::Num_Lock,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PrefixKey {
    /// `Ctrl+a`, and nothing else held but Ctrl (CapsLock does not count).
    Prefix,
    /// A key with no Ctrl/Alt/Super, as the character it types (`M` with Shift, like a terminal).
    Plain(char),
    Escape,
    Modifier,
    Other,
}

pub(crate) fn classify(key: Key, state: ModifierType) -> PrefixKey {
    if MODIFIER_KEYS.contains(&key) {
        return PrefixKey::Modifier;
    }
    if key == Key::Escape {
        return PrefixKey::Escape;
    }
    let chords = ModifierType::CONTROL_MASK
        | ModifierType::ALT_MASK
        | ModifierType::SUPER_MASK
        | ModifierType::META_MASK
        | ModifierType::HYPER_MASK;
    let others = chords.difference(ModifierType::CONTROL_MASK) | ModifierType::SHIFT_MASK;
    if state.contains(ModifierType::CONTROL_MASK) && !state.intersects(others) && key.to_lower() == Key::a {
        return PrefixKey::Prefix;
    }
    if state.intersects(chords) {
        return PrefixKey::Other;
    }
    match key.to_unicode() {
        Some(ch) if !ch.is_control() => PrefixKey::Plain(ch),
        _ => PrefixKey::Other,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PrefixCommand {
    Zoom,
    Resize(Direction),
    /// Hand a real `Ctrl+a` to the pane that has focus (§3.2).
    SendPrefix,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// Not the prefix's: let the key go where it was going.
    Pass,
    /// The prefix's, and it does nothing (arming, an unbound key, `Esc`, a held key's repeat).
    Swallow,
    /// The prefix's: run this.
    Run(PrefixCommand),
}

/// The command a key after the prefix names, and whether tmux binds it with `-r` (§3.1).
fn command(ch: char) -> Option<(PrefixCommand, bool)> {
    Some(match ch {
        'm' => (PrefixCommand::Zoom, true),
        'z' => (PrefixCommand::Zoom, false),
        'h' => (PrefixCommand::Resize(Direction::Left), true),
        'j' => (PrefixCommand::Resize(Direction::Down), true),
        'k' => (PrefixCommand::Resize(Direction::Up), true),
        'l' => (PrefixCommand::Resize(Direction::Right), true),
        _ => return None,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Idle,
    Armed,
    /// After a repeatable command: another repeatable key before `until` runs without the prefix.
    Repeat { until: Instant },
}

#[derive(Debug)]
pub(crate) struct Prefix {
    state: State,
    /// The hardware keycode of the last key this machine consumed, until it is released. Its
    /// auto-repeat is not a new key: holding `Ctrl+a` must not turn into `Ctrl+a Ctrl+a` (§3.1).
    held: Option<u32>,
}

impl Prefix {
    pub(crate) fn new() -> Self {
        Prefix { state: State::Idle, held: None }
    }

    /// Whether the prefix is waiting for its next key: what the top bar shows (§3.1). Repeat mode
    /// is not shown, as tmux does not show it.
    pub(crate) fn is_armed(&self) -> bool {
        self.state == State::Armed
    }

    pub(crate) fn cancel(&mut self) {
        self.state = State::Idle;
        // `held` too: the window losing focus is exactly the case where the release of a key that
        // is physically down never arrives, and a stale `held` swallows that key's next real press
        // as if it were an auto-repeat.
        self.held = None;
    }

    pub(crate) fn release(&mut self, keycode: u32) {
        if self.held == Some(keycode) {
            self.held = None;
        }
    }

    pub(crate) fn press(&mut self, key: PrefixKey, keycode: u32, now: Instant) -> Outcome {
        if let State::Repeat { until } = self.state {
            if now > until {
                self.state = State::Idle;
            }
        }
        if self.held == Some(keycode) {
            // A consumed key's auto-repeat. Holding a repeatable key keeps repeating it, as a
            // terminal's own auto-repeat does under tmux's `-r`; anything else reaches nobody.
            if let (State::Repeat { .. }, PrefixKey::Plain(ch)) = (self.state, key) {
                if let Some((cmd, true)) = command(ch) {
                    self.state = State::Repeat { until: now + REPEAT_TIME };
                    return Outcome::Run(cmd);
                }
            }
            return Outcome::Swallow;
        }
        match self.state {
            State::Idle => {
                if key == PrefixKey::Prefix {
                    self.state = State::Armed;
                    self.held = Some(keycode);
                    return Outcome::Swallow;
                }
                Outcome::Pass
            }
            State::Armed => {
                if key == PrefixKey::Modifier {
                    return Outcome::Swallow;
                }
                self.held = Some(keycode);
                self.state = State::Idle;
                match key {
                    PrefixKey::Prefix => Outcome::Run(PrefixCommand::SendPrefix),
                    PrefixKey::Plain(ch) => match command(ch) {
                        Some((cmd, repeatable)) => {
                            if repeatable {
                                self.state = State::Repeat { until: now + REPEAT_TIME };
                            }
                            Outcome::Run(cmd)
                        }
                        None => Outcome::Swallow,
                    },
                    _ => Outcome::Swallow,
                }
            }
            State::Repeat { .. } => match key {
                PrefixKey::Modifier => Outcome::Pass,
                PrefixKey::Prefix => {
                    self.state = State::Armed;
                    self.held = Some(keycode);
                    Outcome::Swallow
                }
                PrefixKey::Plain(ch) if matches!(command(ch), Some((_, true))) => {
                    self.held = Some(keycode);
                    self.state = State::Repeat { until: now + REPEAT_TIME };
                    Outcome::Run(command(ch).expect("matched above").0)
                }
                // Not repeatable: repeat mode ends and the key goes to the pane as usual (tmux).
                _ => {
                    self.state = State::Idle;
                    Outcome::Pass
                }
            },
        }
    }
}

/// Installs the prefix on `window`. `on_command` runs a command; `on_armed` hears every change of
/// `is_armed()` (the top bar's indicator). Cancelled by the window losing activation and by any
/// click -- the click still does what it does, it just also ends the wait (§3.1).
pub(crate) fn install(
    window: &gtk4::ApplicationWindow,
    on_command: impl Fn(PrefixCommand) + 'static,
    on_armed: impl Fn(bool) + 'static,
) -> Rc<RefCell<Prefix>> {
    let prefix = Rc::new(RefCell::new(Prefix::new()));
    let on_armed: Rc<dyn Fn(bool)> = Rc::new(on_armed);

    let keys = gtk4::EventControllerKey::new();
    keys.set_propagation_phase(gtk4::PropagationPhase::Capture);
    {
        let prefix = prefix.clone();
        let on_armed = on_armed.clone();
        keys.connect_key_pressed(move |_, key, keycode, state| {
            let (outcome, was_armed, is_armed) = {
                let mut p = prefix.borrow_mut();
                let was_armed = p.is_armed();
                let outcome = p.press(classify(key, state), keycode, Instant::now());
                (outcome, was_armed, p.is_armed())
            };
            if was_armed != is_armed {
                on_armed(is_armed);
            }
            match outcome {
                Outcome::Pass => glib::Propagation::Proceed,
                Outcome::Swallow => glib::Propagation::Stop,
                Outcome::Run(cmd) => {
                    println!("[prefix] {cmd:?}");
                    on_command(cmd);
                    glib::Propagation::Stop
                }
            }
        });
    }
    {
        let prefix = prefix.clone();
        keys.connect_key_released(move |_, _, keycode, _| prefix.borrow_mut().release(keycode));
    }
    window.add_controller(keys);

    let cancel = {
        let prefix = prefix.clone();
        let on_armed = on_armed.clone();
        Rc::new(move || {
            let mut p = prefix.borrow_mut();
            if p.is_armed() {
                p.cancel();
                drop(p);
                on_armed(false);
            }
        })
    };
    {
        let cancel = cancel.clone();
        window.connect_notify_local(Some("is-active"), move |window, _| {
            if !window.is_active() {
                cancel();
            }
        });
    }
    let clicks = gtk4::GestureClick::new();
    clicks.set_button(0);
    clicks.set_propagation_phase(gtk4::PropagationPhase::Capture);
    // Never claimed: the click still lands wherever it was going.
    clicks.connect_pressed(move |_, _, _, _| cancel());
    window.add_controller(clicks);

    prefix
}

#[cfg(test)]
mod tests {
    use super::*;

    // Arbitrary, distinct hardware keycodes -- what matters is that different logical keys carry
    // different values (a real keyboard never reuses one), not what the numbers are.
    const CTRL_A: u32 = 38;
    const M: u32 = 58;
    const Z: u32 = 52;
    const H: u32 = 43;
    const J: u32 = 44;
    const K: u32 = 45;
    const L: u32 = 46;
    const X: u32 = 53;
    const ESC: u32 = 9;
    const F11: u32 = 95;
    const SHIFT_L: u32 = 50;

    #[test]
    fn classify_every_row_of_the_spec_table() {
        assert_eq!(classify(Key::a, ModifierType::CONTROL_MASK), PrefixKey::Prefix);
        assert_eq!(
            classify(Key::A, ModifierType::CONTROL_MASK | ModifierType::LOCK_MASK),
            PrefixKey::Prefix
        );
        assert_eq!(
            classify(Key::a, ModifierType::CONTROL_MASK | ModifierType::SHIFT_MASK),
            PrefixKey::Other
        );
        assert_eq!(classify(Key::h, ModifierType::CONTROL_MASK), PrefixKey::Other);
        assert_eq!(classify(Key::m, ModifierType::empty()), PrefixKey::Plain('m'));
        assert_eq!(classify(Key::M, ModifierType::SHIFT_MASK), PrefixKey::Plain('M'));
        assert_eq!(classify(Key::Escape, ModifierType::empty()), PrefixKey::Escape);
        assert_eq!(classify(Key::Shift_L, ModifierType::SHIFT_MASK), PrefixKey::Modifier);
        assert_eq!(classify(Key::Control_L, ModifierType::CONTROL_MASK), PrefixKey::Modifier);
        assert_eq!(classify(Key::Return, ModifierType::empty()), PrefixKey::Other);
    }

    #[test]
    fn idle_passes_everything_but_the_prefix_which_arms_and_swallows() {
        let mut p = Prefix::new();
        let t0 = Instant::now();
        assert!(!p.is_armed());
        assert_eq!(p.press(PrefixKey::Plain('m'), M, t0), Outcome::Pass);
        assert!(!p.is_armed());
        assert_eq!(p.press(PrefixKey::Other, F11, t0), Outcome::Pass);
        assert!(!p.is_armed());
        assert_eq!(p.press(PrefixKey::Prefix, CTRL_A, t0), Outcome::Swallow);
        assert!(p.is_armed());
    }

    #[test]
    fn each_binding_after_the_prefix_runs_its_command_and_disarms() {
        let t0 = Instant::now();
        let cases = [
            (PrefixKey::Plain('m'), M, PrefixCommand::Zoom),
            (PrefixKey::Plain('z'), Z, PrefixCommand::Zoom),
            (PrefixKey::Plain('h'), H, PrefixCommand::Resize(Direction::Left)),
            (PrefixKey::Plain('j'), J, PrefixCommand::Resize(Direction::Down)),
            (PrefixKey::Plain('k'), K, PrefixCommand::Resize(Direction::Up)),
            (PrefixKey::Plain('l'), L, PrefixCommand::Resize(Direction::Right)),
            (PrefixKey::Prefix, CTRL_A, PrefixCommand::SendPrefix),
        ];
        for (key, keycode, expected) in cases {
            let mut p = Prefix::new();
            assert_eq!(p.press(PrefixKey::Prefix, CTRL_A, t0), Outcome::Swallow);
            // A real keyboard releases `a` before a second, distinct press of it -- the `Ctrl+a`
            // case in `cases` above reuses `CTRL_A`'s own keycode, so without this it would read
            // as that same key's auto-repeat rather than a fresh chord (§3.1, "松开 a 再按才算").
            p.release(CTRL_A);
            assert_eq!(p.press(key, keycode, t0), Outcome::Run(expected));
            assert!(!p.is_armed());
        }
    }

    #[test]
    fn an_unbound_key_after_the_prefix_is_swallowed_and_ends_it() {
        let t0 = Instant::now();
        for (key, keycode) in [(PrefixKey::Plain('x'), X), (PrefixKey::Other, F11)] {
            let mut p = Prefix::new();
            p.press(PrefixKey::Prefix, CTRL_A, t0);
            assert_eq!(p.press(key, keycode, t0), Outcome::Swallow);
            assert!(!p.is_armed());
        }
        let mut p = Prefix::new();
        p.press(PrefixKey::Prefix, CTRL_A, t0);
        assert_eq!(p.press(PrefixKey::Escape, ESC, t0), Outcome::Swallow);
        assert!(!p.is_armed());
    }

    #[test]
    fn a_bare_modifier_after_the_prefix_is_swallowed_and_the_prefix_still_waits() {
        let t0 = Instant::now();
        let mut p = Prefix::new();
        p.press(PrefixKey::Prefix, CTRL_A, t0);
        assert_eq!(p.press(PrefixKey::Modifier, SHIFT_L, t0), Outcome::Swallow);
        assert!(p.is_armed());
    }

    #[test]
    fn repeat_window_lets_a_bound_key_run_again_without_the_prefix_and_restarts_the_clock() {
        let t0 = Instant::now();
        let mut p = Prefix::new();
        p.press(PrefixKey::Prefix, CTRL_A, t0);
        assert_eq!(p.press(PrefixKey::Plain('h'), H, t0), Outcome::Run(PrefixCommand::Resize(Direction::Left)));
        p.release(H);

        let t1 = t0 + Duration::from_millis(400);
        assert_eq!(p.press(PrefixKey::Plain('h'), H, t1), Outcome::Run(PrefixCommand::Resize(Direction::Left)));
        p.release(H);

        // The window restarts from t1, not t0: another repeatable key 400ms after THAT still runs.
        let t2 = t1 + Duration::from_millis(400);
        assert_eq!(p.press(PrefixKey::Plain('l'), L, t2), Outcome::Run(PrefixCommand::Resize(Direction::Right)));
        p.release(L);

        // 600ms after the last one: past the window, so the key passes through unprefixed.
        let t3 = t2 + Duration::from_millis(600);
        assert_eq!(p.press(PrefixKey::Plain('h'), H, t3), Outcome::Pass);
    }

    #[test]
    fn m_repeats_and_z_does_not() {
        let t0 = Instant::now();

        let mut p = Prefix::new();
        p.press(PrefixKey::Prefix, CTRL_A, t0);
        p.press(PrefixKey::Plain('z'), Z, t0);
        p.release(Z);
        let t1 = t0 + Duration::from_millis(100);
        assert_eq!(p.press(PrefixKey::Plain('z'), Z, t1), Outcome::Pass);

        let mut p = Prefix::new();
        p.press(PrefixKey::Prefix, CTRL_A, t0);
        p.press(PrefixKey::Plain('m'), M, t0);
        p.release(M);
        let t2 = t0 + Duration::from_millis(100);
        assert_eq!(p.press(PrefixKey::Plain('m'), M, t2), Outcome::Run(PrefixCommand::Zoom));
    }

    #[test]
    fn a_non_repeatable_key_in_repeat_mode_ends_it_and_passes() {
        let t0 = Instant::now();
        let mut p = Prefix::new();
        p.press(PrefixKey::Prefix, CTRL_A, t0);
        assert_eq!(p.press(PrefixKey::Plain('h'), H, t0), Outcome::Run(PrefixCommand::Resize(Direction::Left)));
        p.release(H);

        let t1 = t0 + Duration::from_millis(100);
        assert_eq!(p.press(PrefixKey::Plain('x'), X, t1), Outcome::Pass);
        p.release(X);

        let t2 = t1 + Duration::from_millis(50);
        assert_eq!(p.press(PrefixKey::Plain('h'), H, t2), Outcome::Pass);
    }

    #[test]
    fn held_keys_auto_repeat_is_not_a_new_key() {
        let t0 = Instant::now();

        let mut p = Prefix::new();
        assert_eq!(p.press(PrefixKey::Prefix, CTRL_A, t0), Outcome::Swallow);
        assert!(p.is_armed());
        // Auto-repeat of the held Ctrl+a: same keycode, no `release` in between.
        assert_eq!(p.press(PrefixKey::Prefix, CTRL_A, t0), Outcome::Swallow);
        assert!(p.is_armed());
        p.release(CTRL_A);
        assert_eq!(p.press(PrefixKey::Prefix, CTRL_A, t0), Outcome::Run(PrefixCommand::SendPrefix));

        let mut p = Prefix::new();
        p.press(PrefixKey::Prefix, CTRL_A, t0);
        assert_eq!(p.press(PrefixKey::Plain('h'), H, t0), Outcome::Run(PrefixCommand::Resize(Direction::Left)));
        // Holding h (same keycode, no release) keeps running the command.
        assert_eq!(p.press(PrefixKey::Plain('h'), H, t0), Outcome::Run(PrefixCommand::Resize(Direction::Left)));

        let mut p = Prefix::new();
        p.press(PrefixKey::Prefix, CTRL_A, t0);
        assert_eq!(p.press(PrefixKey::Plain('x'), X, t0), Outcome::Swallow);
        // Holding an unbound key is swallowed too, not passed on a second time.
        assert_eq!(p.press(PrefixKey::Plain('x'), X, t0), Outcome::Swallow);
    }

    /// A key down when the window loses focus produces no release, so `cancel` has to forget it --
    /// otherwise its next real press is swallowed as that key's own auto-repeat.
    #[test]
    fn cancel_forgets_a_key_that_may_never_be_released() {
        let t0 = Instant::now();
        let mut p = Prefix::new();
        assert_eq!(p.press(PrefixKey::Prefix, CTRL_A, t0), Outcome::Swallow);
        p.cancel();
        assert_eq!(p.press(PrefixKey::Prefix, CTRL_A, t0), Outcome::Swallow, "a fresh press arms again");
        assert!(p.is_armed());
    }

    #[test]
    fn cancel_disarms() {
        let t0 = Instant::now();
        let mut p = Prefix::new();
        p.press(PrefixKey::Prefix, CTRL_A, t0);
        assert!(p.is_armed());
        p.cancel();
        assert!(!p.is_armed());
    }
}
