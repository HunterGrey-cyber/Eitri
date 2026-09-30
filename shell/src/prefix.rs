//! The prefix (keymap spec, docs/superpowers/specs/2026-09-25-keymap-tabs-panel-design.md §2): stock
//! tmux's `Ctrl+b` by default, or whatever `init.lua`'s `eitri.keymap.prefix` set, then one key
//! looked up in the effective prefix table (`eitri_core::keymap::Keymap`). The table is data; this
//! file is the state machine tmux runs over it: arm on the prefix, run the key's action, stay in a
//! 500ms repeat window after a `-r` binding, swallow an unbound key and end the wait, `Esc` cancels.
//!
//! After a split key (`split.right`/`split.below`, `%`/`"` by default) the prefix waits for a module
//! key -- `e a t` and each Lua panel's `key` (modules spec §6.3) -- and a capital is taken for its
//! lowercase when no Lua panel registered the capital itself (`"` is Shift+' on the owner's layout).
//! A Lua panel's key also runs its module directly, since no binding can have it (the reserved-set
//! check, `ModuleKeys::build`).
//!
//! A window-level capture controller, added at startup after every other window-level controller,
//! so it sees every key before any app accelerator and before the editor, the panel or the top bar;
//! HINT's controller, added when a HINT starts, sees keys before this one.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gtk4::gdk::{Key, ModifierType};
use gtk4::glib;
use gtk4::prelude::*;

use eitri_core::keymap::{Action, KeyName, KeySpec, Keymap};
use eitri_core::layout::{Axis, ModuleId, ModuleKeys, ModuleKind};

/// tmux's default `repeat-time`.
pub(crate) const REPEAT_TIME: Duration = Duration::from_millis(500);

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

/// One key press as the prefix reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Press {
    /// A key tmux can name: Ctrl/Alt as `C-`/`M-`; Shift folded into a character, kept on a named key.
    Key(KeySpec),
    Escape,
    Modifier,
    /// Super/Hyper/Meta held, `C-S-<char>`, or a key with no tmux name.
    Other,
}

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

/// What the prefix hands `main.rs` to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PrefixCommand {
    Run(Action),
    /// After a split key and a module key: open `module` next to the module with the keys, or move it.
    Place {
        module: ModuleId,
        axis: Axis,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// Not the prefix's: let the key go where it was going.
    Pass,
    /// The prefix's, and it does nothing (arming, an unbound key, `Esc`, a held key's repeat).
    Swallow,
    Run(PrefixCommand),
}

/// The key `send-prefix` / `send-keys` hands the pane with the keys (spec §2.6, ruling 3).
pub(crate) fn literal_for(action: &Action, keymap: &Keymap) -> Option<KeySpec> {
    match action {
        Action::SendPrefix => Some(*keymap.prefix()),
        Action::SendKeys(key) => Some(*key),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Idle,
    Armed,
    /// After a split key: the next key names the module to place along `axis`.
    AwaitModule {
        axis: Axis,
    },
    /// After a repeatable binding: another repeatable key before `until` runs without the prefix.
    Repeat {
        until: Instant,
    },
}

/// What the prefix is waiting for: what the top bar shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Waiting {
    /// Nothing (repeat mode included, which tmux does not show either).
    No,
    Command,
    Module(Axis),
}

pub(crate) struct Prefix {
    state: State,
    /// The hardware keycode of the last key this machine consumed, until it is released. Its
    /// auto-repeat is not a new key: holding the prefix must not turn into `prefix prefix`.
    held: Option<u32>,
    keymap: Rc<Keymap>,
    module_keys: Rc<ModuleKeys>,
}

impl Prefix {
    pub(crate) fn new(keymap: Rc<Keymap>, module_keys: Rc<ModuleKeys>) -> Self {
        Prefix {
            state: State::Idle,
            held: None,
            keymap,
            module_keys,
        }
    }

    pub(crate) fn is_armed(&self) -> bool {
        self.waiting() != Waiting::No
    }

    pub(crate) fn waiting(&self) -> Waiting {
        match self.state {
            State::Armed => Waiting::Command,
            State::AwaitModule { axis } => Waiting::Module(axis),
            State::Idle | State::Repeat { .. } => Waiting::No,
        }
    }

    pub(crate) fn cancel(&mut self) {
        self.state = State::Idle;
        // The window losing focus is exactly when the release of a key that is down never arrives.
        self.held = None;
    }

    pub(crate) fn release(&mut self, keycode: u32) {
        if self.held == Some(keycode) {
            self.held = None;
        }
    }

    fn repeatable(&self, press: Press) -> Option<Action> {
        let Press::Key(key) = press else { return None };
        self.keymap
            .lookup(&key)
            .filter(|b| b.repeatable)
            .map(|b| b.action.clone())
    }

    pub(crate) fn press(&mut self, press: Press, keycode: u32, now: Instant) -> Outcome {
        if let State::Repeat { until } = self.state {
            if now > until {
                self.state = State::Idle;
            }
        }
        let is_prefix = matches!(press, Press::Key(key) if key == *self.keymap.prefix());
        if self.held == Some(keycode) {
            // A consumed key's auto-repeat: a repeatable binding keeps repeating, as under tmux's -r.
            if let State::Repeat { .. } = self.state {
                if let Some(action) = self.repeatable(press) {
                    self.state = State::Repeat {
                        until: now + REPEAT_TIME,
                    };
                    return Outcome::Run(PrefixCommand::Run(action));
                }
            }
            return Outcome::Swallow;
        }
        match self.state {
            State::Idle => {
                if is_prefix {
                    self.state = State::Armed;
                    self.held = Some(keycode);
                    return Outcome::Swallow;
                }
                Outcome::Pass
            }
            State::Armed => {
                if press == Press::Modifier {
                    return Outcome::Swallow;
                }
                self.held = Some(keycode);
                self.state = State::Idle;
                let Press::Key(key) = press else {
                    return Outcome::Swallow;
                };
                match self.keymap.lookup(&key) {
                    Some(binding) => match binding.action {
                        Action::Split(axis) => {
                            self.state = State::AwaitModule { axis };
                            Outcome::Swallow
                        }
                        ref action => {
                            if binding.repeatable {
                                self.state = State::Repeat {
                                    until: now + REPEAT_TIME,
                                };
                            }
                            Outcome::Run(PrefixCommand::Run(action.clone()))
                        }
                    },
                    None => match key.as_char().and_then(|c| self.module_keys.module(c)) {
                        Some(id) if id.kind() == ModuleKind::LuaWebview => {
                            Outcome::Run(PrefixCommand::Run(Action::Module(id.clone())))
                        }
                        _ => Outcome::Swallow,
                    },
                }
            }
            State::AwaitModule { axis } => {
                if press == Press::Modifier {
                    return Outcome::Swallow;
                }
                self.held = Some(keycode);
                self.state = State::Idle;
                let Some(c) = (match press {
                    Press::Key(key) => key.as_char(),
                    _ => None,
                }) else {
                    return Outcome::Swallow;
                };
                // The exact key first (a Lua panel may own a capital), then a capital's lowercase.
                match self
                    .module_keys
                    .module(c)
                    .or_else(|| self.module_keys.module(c.to_ascii_lowercase()))
                {
                    Some(module) => Outcome::Run(PrefixCommand::Place {
                        module: module.clone(),
                        axis,
                    }),
                    None => Outcome::Swallow,
                }
            }
            State::Repeat { .. } => {
                if press == Press::Modifier {
                    return Outcome::Pass;
                }
                if is_prefix {
                    self.state = State::Armed;
                    self.held = Some(keycode);
                    return Outcome::Swallow;
                }
                if let Some(action) = self.repeatable(press) {
                    self.held = Some(keycode);
                    self.state = State::Repeat {
                        until: now + REPEAT_TIME,
                    };
                    return Outcome::Run(PrefixCommand::Run(action));
                }
                // Not repeatable: repeat mode ends and the key goes to the pane as usual (tmux).
                self.state = State::Idle;
                Outcome::Pass
            }
        }
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
    use eitri_core::keymap::{KeymapOp, SwapTarget, TextChange};
    use eitri_core::layout::Direction;

    // Arbitrary, distinct hardware keycodes: a real keyboard never reuses one for two keys.
    const PREFIX: u32 = 56;
    const K1: u32 = 10;
    const K2: u32 = 11;
    const ESC: u32 = 9;
    const SHIFT_L: u32 = 50;

    fn key(s: &str) -> Press {
        Press::Key(KeySpec::parse(s).unwrap())
    }
    fn run(action: Action) -> Outcome {
        Outcome::Run(PrefixCommand::Run(action))
    }
    fn with(keymap: Keymap, lua_keys: &[(&str, &str)]) -> Prefix {
        let lua: Vec<(ModuleId, Option<String>)> = lua_keys
            .iter()
            .map(|(id, k)| (ModuleId::lua(id), Some(k.to_string())))
            .collect();
        let module_keys = ModuleKeys::build(&lua, &keymap).unwrap();
        Prefix::new(Rc::new(keymap), Rc::new(module_keys))
    }
    fn defaults() -> Prefix {
        with(Keymap::defaults(), &[])
    }
    /// The prefix pressed and released: armed.
    fn armed(mut p: Prefix, t: Instant) -> Prefix {
        let prefix = Press::Key(*p.keymap.prefix());
        assert_eq!(p.press(prefix, PREFIX, t), Outcome::Swallow);
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

    /// Spec §2.4's risk: these were all `Other` under `PrefixKey::Plain(char)`.
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
    /// keyvals (`;` is `semicolon`; Shift+`;` types `:`, which stays unbound), and bound without
    /// `-r`: pressed again with no prefix they are ordinary keys.
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
            Outcome::Swallow,
            ": is command-prompt in stock tmux, which is not adopted"
        );
    }

    /// Review Focus 2.
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

    #[test]
    fn idle_passes_everything_but_the_prefix_which_arms_and_swallows() {
        let t = Instant::now();
        let mut p = defaults();
        assert_eq!(p.press(key("m"), K1, t), Outcome::Pass);
        assert_eq!(
            p.press(key("C-a"), K2, t),
            Outcome::Pass,
            "C-a is nvim's again by default"
        );
        assert!(!p.is_armed());
        assert_eq!(p.press(key("C-b"), PREFIX, t), Outcome::Swallow);
        assert_eq!(p.waiting(), Waiting::Command);
    }

    /// Every default binding but the two split keys runs its own action, from the table itself.
    #[test]
    fn every_default_binding_after_the_prefix_runs_its_action_and_disarms() {
        let t = Instant::now();
        for binding in Keymap::defaults().bindings() {
            if matches!(binding.action, Action::Split(_)) {
                continue;
            }
            let mut p = armed(defaults(), t);
            assert_eq!(
                p.press(Press::Key(binding.key), K1, t),
                run(binding.action.clone()),
                "{}",
                binding.key
            );
            assert!(!p.is_armed(), "{}", binding.key);
        }
    }

    #[test]
    fn an_unbound_key_or_esc_after_the_prefix_is_swallowed_and_ends_it() {
        let t = Instant::now();
        for press in [key("q"), key("s"), key("S-Up"), Press::Other, Press::Escape] {
            let mut p = armed(defaults(), t);
            assert_eq!(p.press(press, K1, t), Outcome::Swallow, "{press:?}");
            assert!(!p.is_armed());
        }
    }

    #[test]
    fn a_bare_modifier_after_the_prefix_is_swallowed_and_the_prefix_still_waits() {
        let t = Instant::now();
        let mut p = armed(defaults(), t);
        assert_eq!(p.press(Press::Modifier, SHIFT_L, t), Outcome::Swallow);
        assert!(p.is_armed());
    }

    /// tmux's `-r` on the arrows: `C-` and `M-` arrows repeat within 500ms, from the table's flag.
    #[test]
    fn ctrl_and_alt_arrows_repeat_within_the_window_and_not_after_it() {
        let t0 = Instant::now();
        let mut p = armed(defaults(), t0);
        assert_eq!(
            p.press(key("C-Up"), K1, t0),
            run(Action::Resize {
                dir: Direction::Up,
                cells: 1
            })
        );
        p.release(K1);
        let t1 = t0 + Duration::from_millis(400);
        assert_eq!(
            p.press(key("M-Down"), K2, t1),
            run(Action::Resize {
                dir: Direction::Down,
                cells: 5
            })
        );
        p.release(K2);
        let t2 = t1 + Duration::from_millis(400);
        assert_eq!(
            p.press(key("Left"), K1, t2),
            run(Action::Select(Direction::Left)),
            "the window restarts"
        );
        p.release(K1);
        let t3 = t2 + Duration::from_millis(600);
        assert_eq!(
            p.press(key("C-Up"), K1, t3),
            Outcome::Pass,
            "past the window it reaches the pane"
        );
    }

    #[test]
    fn a_non_repeatable_key_in_repeat_mode_ends_it_and_passes() {
        let t0 = Instant::now();
        let mut p = armed(defaults(), t0);
        p.press(key("C-Up"), K1, t0);
        p.release(K1);
        assert_eq!(p.press(key("x"), K2, t0 + Duration::from_millis(100)), Outcome::Pass);
        p.release(K2);
        assert_eq!(p.press(key("C-Up"), K1, t0 + Duration::from_millis(150)), Outcome::Pass);
    }

    /// Holding a repeatable key keeps running it; holding anything else runs it once.
    #[test]
    fn a_held_keys_auto_repeat_is_not_a_new_key() {
        let t = Instant::now();
        let mut p = armed(defaults(), t);
        assert_eq!(
            p.press(key("C-Up"), K1, t),
            run(Action::Resize {
                dir: Direction::Up,
                cells: 1
            })
        );
        assert_eq!(
            p.press(key("C-Up"), K1, t),
            run(Action::Resize {
                dir: Direction::Up,
                cells: 1
            })
        );
        let mut p = armed(defaults(), t);
        assert_eq!(p.press(key("q"), K1, t), Outcome::Swallow);
        assert_eq!(
            p.press(key("q"), K1, t),
            Outcome::Swallow,
            "an unbound key's repeat reaches nobody"
        );
    }

    /// Review Focus 1, and spec §2.6.
    #[test]
    fn held_prefix_is_not_sent_twice_and_send_prefix_names_the_configured_chord() {
        let t = Instant::now();
        let ops = [
            KeymapOp::Prefix { key: "C-a".into() },
            KeymapOp::Del {
                table: "prefix".into(),
                key: "C-b".into(),
            },
            KeymapOp::Set {
                table: "prefix".into(),
                key: "C-a".into(),
                action: "send-prefix".into(),
                opts: vec![],
            },
        ];
        let custom = Keymap::apply_user(&ops, &[]).unwrap();
        let mut p = with(custom.clone(), &[]);
        assert_eq!(p.press(key("C-a"), PREFIX, t), Outcome::Swallow);
        assert_eq!(
            p.press(key("C-a"), PREFIX, t),
            Outcome::Swallow,
            "auto-repeat of the held prefix"
        );
        assert!(p.is_armed());
        p.release(PREFIX);
        assert_eq!(p.press(key("C-a"), PREFIX, t), run(Action::SendPrefix));
        assert_eq!(literal_for(&Action::SendPrefix, &custom).unwrap().to_vim(), "<C-a>");
        assert_eq!(
            literal_for(&Action::SendPrefix, &Keymap::defaults()).unwrap().to_vim(),
            "<C-b>"
        );
        let c_l = KeySpec::parse("C-l").unwrap();
        assert_eq!(literal_for(&Action::SendKeys(c_l), &custom), Some(c_l));
        assert_eq!(literal_for(&Action::Zoom, &custom), None);
        // `C-b` is not the prefix any more, so it passes to the pane.
        assert_eq!(p.press(key("C-b"), K1, t), Outcome::Pass);
    }

    #[test]
    fn cancel_disarms_and_forgets_a_key_that_may_never_be_released() {
        let t = Instant::now();
        let mut p = defaults();
        p.press(key("C-b"), PREFIX, t);
        p.cancel();
        assert!(!p.is_armed());
        assert_eq!(
            p.press(key("C-b"), PREFIX, t),
            Outcome::Swallow,
            "a fresh press arms again"
        );
        assert!(p.is_armed());
    }

    /// `%`/`"` wait for a module key and say so; a module key places it; anything else, `Esc`
    /// included, is swallowed and ends the wait.
    #[test]
    fn a_split_key_waits_for_a_module_key() {
        let t = Instant::now();
        for (split, axis) in [("%", Axis::Row), ("\"", Axis::Column)] {
            for (next, expected) in [
                (
                    key("a"),
                    Outcome::Run(PrefixCommand::Place {
                        module: ModuleId::agent(),
                        axis,
                    }),
                ),
                (
                    key("t"),
                    Outcome::Run(PrefixCommand::Place {
                        module: ModuleId::terminal(),
                        axis,
                    }),
                ),
                (
                    key("A"),
                    Outcome::Run(PrefixCommand::Place {
                        module: ModuleId::agent(),
                        axis,
                    }),
                ),
                (key("x"), Outcome::Swallow),
                (key("v"), Outcome::Swallow),
                (Press::Escape, Outcome::Swallow),
            ] {
                let mut p = armed(defaults(), t);
                assert_eq!(p.press(key(split), K1, t), Outcome::Swallow);
                assert_eq!(p.waiting(), Waiting::Module(axis));
                assert_eq!(p.press(Press::Modifier, SHIFT_L, t), Outcome::Swallow);
                assert_eq!(
                    p.waiting(),
                    Waiting::Module(axis),
                    "a bare modifier is not the next key"
                );
                assert_eq!(
                    p.press(next, if matches!(next, Press::Escape) { ESC } else { K2 }, t),
                    expected,
                    "{split} {next:?}"
                );
                assert_eq!(p.waiting(), Waiting::No);
            }
        }
    }

    /// A Lua panel's key is a module key: on its own, and after a split key. Its exact capital wins
    /// over the lowercase fallback.
    #[test]
    fn a_lua_panels_key_names_its_module() {
        let t = Instant::now();
        let mut p = armed(with(Keymap::defaults(), &[("notes", "N"), ("g1", "g"), ("g2", "G")]), t);
        assert_eq!(p.press(key("N"), K1, t), run(Action::Module(ModuleId::lua("notes"))));
        p.release(K1);
        let mut p = armed(with(Keymap::defaults(), &[("g1", "g"), ("g2", "G")]), t);
        p.press(key("\""), K1, t);
        assert_eq!(
            p.press(key("G"), K2, t),
            Outcome::Run(PrefixCommand::Place {
                module: ModuleId::lua("g2"),
                axis: Axis::Column
            })
        );
        let mut plain = armed(defaults(), t);
        assert_eq!(plain.press(key("N"), K1, t), Outcome::Swallow);
    }

    /// Only a Lua panel's key falls back to its module: a built-in module key the user deleted is gone.
    #[test]
    fn a_deleted_built_in_module_key_is_unbound() {
        let t = Instant::now();
        let keymap = Keymap::apply_user(
            &[KeymapOp::Del {
                table: "prefix".into(),
                key: "e".into(),
            }],
            &[],
        )
        .unwrap();
        let mut p = armed(with(keymap, &[]), t);
        assert_eq!(p.press(key("e"), K1, t), Outcome::Swallow);
    }

    #[test]
    fn the_owners_bindings_run_through_the_same_machine() {
        let t = Instant::now();
        let ops = [
            KeymapOp::Del {
                table: "prefix".into(),
                key: "l".into(),
            },
            KeymapOp::Set {
                table: "prefix".into(),
                key: "l".into(),
                action: "resize.right".into(),
                opts: vec![
                    ("cells".into(), eitri_core::keymap::OptValue::Int(5)),
                    ("repeatable".into(), eitri_core::keymap::OptValue::Bool(true)),
                ],
            },
            KeymapOp::Set {
                table: "prefix".into(),
                key: "H".into(),
                action: "swap.left".into(),
                opts: vec![],
            },
            KeymapOp::Set {
                table: "prefix".into(),
                key: "=".into(),
                action: "text.larger".into(),
                opts: vec![],
            },
        ];
        let keymap = Keymap::apply_user(&ops, &[]).unwrap();
        let mut p = armed(with(keymap.clone(), &[]), t);
        assert_eq!(
            p.press(key("l"), K1, t),
            run(Action::Resize {
                dir: Direction::Right,
                cells: 5
            })
        );
        p.release(K1);
        assert_eq!(
            p.press(key("l"), K1, t + Duration::from_millis(100)),
            run(Action::Resize {
                dir: Direction::Right,
                cells: 5
            })
        );
        let mut p = armed(with(keymap.clone(), &[]), t);
        assert_eq!(
            p.press(key("H"), K1, t),
            run(Action::Swap(SwapTarget::Toward(Direction::Left)))
        );
        let mut p = armed(with(keymap, &[]), t);
        assert_eq!(p.press(key("="), K1, t), run(Action::Text(TextChange::Larger)));
    }
}
