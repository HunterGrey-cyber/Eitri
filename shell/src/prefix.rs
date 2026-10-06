//! The GDK half of the prefix: `classify` turns a GDK key and its modifier state into the machine's
//! `Press` (`eitri_core::keymap::prefix`, which holds the state machine, its repeat window and the
//! reason it keys on the hardware keycode), and `install` adds the window-level capture controller
//! that feeds it. The controller is added at startup after every other window-level controller, so
//! it sees every key before any app accelerator and before the editor, the panel or the top bar;
//! HINT's controller, added when a HINT starts, sees keys before this one.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Instant;

use gtk4::gdk::{Key, ModifierType};
use gtk4::glib;
use gtk4::prelude::*;

use eitri_core::keymap::{KeyName, KeySpec, Keymap};
use eitri_core::layout::ModuleKeys;

pub(crate) use eitri_core::keymap::prefix::{literal_for, Outcome, Prefix, PrefixCommand, Press, Waiting};

/// A bare modifier is not "the next key".
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

fn named(key: Key) -> Option<KeyName> {
    Some(match key {
        Key::Up | Key::KP_Up => KeyName::Up,
        Key::Down | Key::KP_Down => KeyName::Down,
        Key::Left | Key::KP_Left => KeyName::Left,
        Key::Right | Key::KP_Right => KeyName::Right,
        Key::space => KeyName::Space,
        Key::Return | Key::KP_Enter => KeyName::Enter,
        Key::Tab | Key::ISO_Left_Tab => KeyName::Tab,
        Key::BackSpace => KeyName::BSpace,
        Key::Delete => KeyName::DC,
        Key::Insert => KeyName::IC,
        Key::Home => KeyName::Home,
        Key::End => KeyName::End,
        Key::Page_Up => KeyName::PPage,
        Key::Page_Down => KeyName::NPage,
        _ => {
            let name = key.name()?;
            let n: u8 = name.strip_prefix('F')?.parse().ok()?;
            if !(1..=24).contains(&n) {
                return None;
            }
            KeyName::F(n)
        }
    })
}

pub(crate) fn classify(key: Key, state: ModifierType) -> Press {
    if MODIFIER_KEYS.contains(&key) {
        return Press::Modifier;
    }
    if key == Key::Escape {
        return Press::Escape;
    }
    if state.intersects(ModifierType::SUPER_MASK | ModifierType::HYPER_MASK | ModifierType::META_MASK) {
        return Press::Other;
    }
    let ctrl = state.contains(ModifierType::CONTROL_MASK);
    let meta = state.contains(ModifierType::ALT_MASK);
    let shift = state.contains(ModifierType::SHIFT_MASK);
    if let Some(name) = named(key) {
        return Press::Key(KeySpec::named(name, ctrl, meta, shift));
    }
    // With Ctrl the letter is read lowercase, so CapsLock does not turn `Ctrl+b` into `C-B`.
    let ch = if ctrl {
        key.to_lower().to_unicode()
    } else {
        key.to_unicode()
    };
    match ch {
        Some(ch) if !ch.is_control() && !ch.is_whitespace() => {
            if ctrl && shift {
                return Press::Other;
            }
            Press::Key(KeySpec::char(ch, ctrl, meta))
        }
        _ => Press::Other,
    }
}

/// Installs the prefix on `window`. `on_command` runs a command and is told the keycode of the key
/// that ran it (HINT needs it: that key is still down); `on_waiting` hears every change of
/// `waiting()`. Cancelled by the window losing activation and by any click.
pub(crate) fn install(
    window: &gtk4::ApplicationWindow,
    keymap: Rc<Keymap>,
    module_keys: Rc<ModuleKeys>,
    on_command: impl Fn(PrefixCommand, u32) + 'static,
    on_waiting: impl Fn(Waiting) + 'static,
) -> Rc<RefCell<Prefix>> {
    let prefix = Rc::new(RefCell::new(Prefix::new(keymap, module_keys)));
    let on_waiting: Rc<dyn Fn(Waiting)> = Rc::new(on_waiting);

    let keys = gtk4::EventControllerKey::new();
    keys.set_propagation_phase(gtk4::PropagationPhase::Capture);
    {
        let prefix = prefix.clone();
        let on_waiting = on_waiting.clone();
        keys.connect_key_pressed(move |_, key, keycode, state| {
            let (outcome, was, now) = {
                let mut p = prefix.borrow_mut();
                let was = p.waiting();
                let outcome = p.press(classify(key, state), keycode, Instant::now());
                (outcome, was, p.waiting())
            };
            if was != now {
                // Logged both ways since 2026-09-20: an arm that never disarms looks exactly like a
                // window refusing input.
                println!("[prefix] armed={} waiting={now:?}", now != Waiting::No);
                on_waiting(now);
            }
            match outcome {
                Outcome::Pass => glib::Propagation::Proceed,
                Outcome::Swallow => glib::Propagation::Stop,
                Outcome::Run(cmd) => {
                    println!("[prefix] {cmd:?}");
                    on_command(cmd, keycode);
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
        let on_waiting = on_waiting.clone();
        Rc::new(move || {
            let mut p = prefix.borrow_mut();
            if p.is_armed() {
                p.cancel();
                drop(p);
                on_waiting(Waiting::No);
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
    use eitri_core::keymap::Action;
    use eitri_core::layout::Direction;
    use std::time::Duration;

    // Arbitrary, distinct hardware keycodes: a real keyboard never reuses one for two keys.
    const PREFIX: u32 = 56;
    const K1: u32 = 10;
    const K2: u32 = 11;

    fn key(s: &str) -> Press {
        Press::Key(KeySpec::parse(s).unwrap())
    }
    fn run(action: Action) -> Outcome {
        Outcome::Run(PrefixCommand::Run(action))
    }
    fn defaults() -> Prefix {
        let keymap = Keymap::defaults();
        let module_keys = ModuleKeys::build(&[], &keymap).unwrap();
        Prefix::new(Rc::new(keymap), Rc::new(module_keys))
    }
    /// The default prefix pressed and released: armed.
    fn armed(mut p: Prefix, t: Instant) -> Prefix {
        assert_eq!(p.press(key("C-b"), PREFIX, t), Outcome::Swallow);
        p.release(PREFIX);
        p
    }

    #[test]
    fn classify_reads_gdk_keys_as_tmux_names() {
        let ctrl = ModifierType::CONTROL_MASK;
        let shift = ModifierType::SHIFT_MASK;
        assert_eq!(classify(Key::b, ctrl), key("C-b"));
        assert_eq!(
            classify(Key::a, ctrl | shift),
            Press::Other,
            "C-S-<char> has no spelling"
        );
        assert_eq!(classify(Key::m, ModifierType::empty()), key("m"));
        assert_eq!(classify(Key::M, shift), key("M"), "Shift is folded into a character");
        assert_eq!(classify(Key::percent, shift), key("%"));
        assert_eq!(classify(Key::Escape, ModifierType::empty()), Press::Escape);
        assert_eq!(classify(Key::Shift_L, shift), Press::Modifier);
        assert_eq!(classify(Key::Control_L, ctrl), Press::Modifier);
        assert_eq!(classify(Key::x, ModifierType::SUPER_MASK), Press::Other);
        assert_eq!(classify(Key::Return, ModifierType::empty()), key("Enter"));
        assert_eq!(classify(Key::space, ModifierType::empty()), key("Space"));
    }

    /// Named and modified keys (Ctrl+Up, Alt+Up, Alt+1, F11, ...) must classify: under the old one-character key
    /// representation they were all `Other` and could never be bound.
    #[test]
    fn ctrl_up_alt_up_up_alt_1_and_f11_classify_and_run() {
        let t = Instant::now();
        for (gdk, state, expected) in [
            (
                Key::Up,
                ModifierType::CONTROL_MASK,
                Action::Resize {
                    dir: Direction::Up,
                    cells: 1,
                },
            ),
            (
                Key::Left,
                ModifierType::ALT_MASK,
                Action::Resize {
                    dir: Direction::Left,
                    cells: 5,
                },
            ),
            (Key::Right, ModifierType::empty(), Action::Select(Direction::Right)),
            (
                Key::_1,
                ModifierType::ALT_MASK,
                Action::Even(eitri_core::layout::Axis::Row),
            ),
            (Key::F11, ModifierType::empty(), Action::WindowImmersive),
        ] {
            let mut p = armed(defaults(), t);
            assert_eq!(p.press(classify(gdk, state), K1, t), run(expected), "{gdk:?} {state:?}");
            assert!(!p.is_armed());
        }
    }

    /// v1 picks (2026-09-29): stock tmux's `last-pane` and `select-pane -t :.+`. Read from GDK's own
    /// keyvals (`;` is `semicolon`; Shift+`;` types `:`), and bound without `-r`: pressed again with
    /// no prefix they are ordinary keys. Since rc.4 (decision #28, K16) `:` is bound too: it opens
    /// the panel's no-op `:` line, where stock tmux opens its command prompt.
    #[test]
    fn semicolon_and_o_classify_from_gdk_and_run_without_repeating() {
        let t = Instant::now();
        for (gdk, expected) in [(Key::semicolon, Action::SelectLast), (Key::o, Action::SelectNext)] {
            let press = classify(gdk, ModifierType::empty());
            let mut p = armed(defaults(), t);
            assert_eq!(p.press(press, K1, t), run(expected), "{gdk:?}");
            assert!(!p.is_armed(), "{gdk:?}");
            p.release(K1);
            assert_eq!(
                p.press(press, K2, t + Duration::from_millis(100)),
                Outcome::Pass,
                "{gdk:?}: no `-r`, so no repeat window"
            );
        }
        let colon = classify(Key::colon, ModifierType::SHIFT_MASK);
        assert_eq!(colon, key(":"), "Shift+; is the character it types");
        assert_eq!(
            armed(defaults(), t).press(colon, K1, t),
            run(Action::PanelCommandLine),
            ": opens the panel's no-op command line (decision #28), not stock tmux's command-prompt"
        );
    }

    /// Caps Lock does not change the chord (`C-b` with Lock still arms), and Shift stays on a named key, so `S-Up` is
    /// its own unbound key and never `select.up`.
    #[test]
    fn capslock_still_arms_and_shift_is_kept_on_named_keys() {
        let t = Instant::now();
        let mut p = defaults();
        let caps_ctrl_b = classify(Key::B, ModifierType::CONTROL_MASK | ModifierType::LOCK_MASK);
        assert_eq!(caps_ctrl_b, key("C-b"));
        assert_eq!(p.press(caps_ctrl_b, PREFIX, t), Outcome::Swallow);
        assert!(p.is_armed());
        p.release(PREFIX);
        let shift_up = classify(Key::Up, ModifierType::SHIFT_MASK);
        assert_eq!(shift_up, key("S-Up"));
        assert_eq!(
            p.press(shift_up, K1, t),
            Outcome::Swallow,
            "S-Up is unbound, never select.up"
        );
    }
}
