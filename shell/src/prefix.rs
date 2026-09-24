//! The `Ctrl+a` prefix, copied from the owner's tmux (spec:
//! docs/superpowers/specs/2026-09-19-window-modes-design.md §3). `Ctrl+a`, then `m`/`z` zoom,
//! `h`/`j`/`k`/`l` resize by 5 cells, `Ctrl+a` hands a real `Ctrl+a` to the pane. The bottom
//! terminal (spec 2026-09-23-bottom-terminal-design.md) adds `t` (show/focus/hide it) and `Ctrl+l`
//! (a literal `Ctrl+l` for it, his own `base.conf:73`).
//!
//! **Modules P2** (docs/superpowers/specs/2026-09-23-modules-and-canvas-design.md §6.3) adds the tmux
//! verbs over the module tree, as his `base.conf` binds them: `x` hides the module with the keys
//! (his `kill-pane`, reinterpreted -- "先做只隐藏吧"); `e`/`a` and each Lua panel's key show, focus or
//! hide that module; `\` or `"` then a module key opens it right of / below the module with the keys,
//! or moves it there; `H/J/K/L` swap; `|`/`_` even. `q` stays unbound on purpose (his `kill-window`).
//! None of them repeats. After `\` or `"` the prefix waits for a module key ([`Waiting::Module`]);
//! anything else is swallowed and ends the wait, `Esc` included.
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

use neovibe_core::layout::Axis;

use crate::layout::Direction;
use crate::text_size::TextStep;

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
    /// `Ctrl` plus one letter other than `a`, nothing else held but Ctrl: what `Ctrl+a Ctrl+l`
    /// needs to see. Everywhere but that one arm it behaves exactly as `Other` did.
    Control(char),
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
    if state.contains(ModifierType::CONTROL_MASK) && !state.intersects(others) {
        if key.to_lower() == Key::a {
            return PrefixKey::Prefix;
        }
        if let Some(letter) = key.to_lower().to_unicode().filter(char::is_ascii_lowercase) {
            return PrefixKey::Control(letter);
        }
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
    /// `=`/`-`/`0` (zoom-together spec §3): text size, for whichever pane holds the keys. Not
    /// named `Zoom` -- that name is already `PrefixCommand::Zoom` above, the pane-fill toggle.
    TextSize(TextStep),
    /// `t`: show, focus or hide the bottom terminal (bottom-terminal spec §2.4). Not repeatable.
    ToggleTerminal,
    /// `Ctrl+l`: hand the terminal a literal `Ctrl+l` (clear screen), which bare `Ctrl+l` cannot
    /// be -- neovibe takes that for pane navigation. His tmux's own answer: `base.conf:73`.
    SendCtrlL,
    /// `x`: hide the module with the keys; refused for the last one on screen.
    Hide,
    /// `e`, `a` or a Lua panel's key: that module's four-case rule (spec §4.3).
    Module(char),
    /// `\` (`Axis::Row`) or `"` (`Axis::Column`), then the module key `key`.
    Place {
        key: char,
        axis: Axis,
    },
    /// `H`/`J`/`K`/`L`: swap the module with the keys with its neighbour that way.
    Swap(Direction),
    /// `|` (`Axis::Row`) / `_` (`Axis::Column`): every shown module in one row / column.
    Even(Axis),
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

/// What a key right after the prefix does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Binding {
    /// Runs this command; `true` if tmux binds it with `-r` (§3.1).
    Run(PrefixCommand, bool),
    /// `\` / `"`: waits for a module key, then places it along this axis.
    AwaitModule(Axis),
}

/// The binding a key after the prefix names (§3.1). `=`/`-` repeat like `m`/`h`/`j`/`k`/`l`; `0` is
/// one-shot like `z` (zoom-together spec §3); modules P2's keys are all one-shot, as the spec's
/// table has them (§6.3). The module keys `e`/`a` are here because they are neovibe's own; a Lua
/// panel's key is not a binding of this table but of the window ([`Prefix::with_module_keys`]).
fn binding(ch: char) -> Option<Binding> {
    use Binding::{AwaitModule, Run};
    Some(match ch {
        'm' => Run(PrefixCommand::Zoom, true),
        'z' => Run(PrefixCommand::Zoom, false),
        'h' => Run(PrefixCommand::Resize(Direction::Left), true),
        'j' => Run(PrefixCommand::Resize(Direction::Down), true),
        'k' => Run(PrefixCommand::Resize(Direction::Up), true),
        'l' => Run(PrefixCommand::Resize(Direction::Right), true),
        '=' => Run(PrefixCommand::TextSize(TextStep::Larger), true),
        '-' => Run(PrefixCommand::TextSize(TextStep::Smaller), true),
        '0' => Run(PrefixCommand::TextSize(TextStep::Reset), false),
        't' => Run(PrefixCommand::ToggleTerminal, false),
        'x' => Run(PrefixCommand::Hide, false),
        'e' | 'a' => Run(PrefixCommand::Module(ch), false),
        'H' => Run(PrefixCommand::Swap(Direction::Left), false),
        'J' => Run(PrefixCommand::Swap(Direction::Down), false),
        'K' => Run(PrefixCommand::Swap(Direction::Up), false),
        'L' => Run(PrefixCommand::Swap(Direction::Right), false),
        '|' => Run(PrefixCommand::Even(Axis::Row), false),
        '_' => Run(PrefixCommand::Even(Axis::Column), false),
        '\\' => AwaitModule(Axis::Row),
        '"' => AwaitModule(Axis::Column),
        _ => return None,
    })
}

/// The command a key names, when it names one, and whether it repeats.
fn command(ch: char) -> Option<(PrefixCommand, bool)> {
    match binding(ch)? {
        Binding::Run(command, repeatable) => Some((command, repeatable)),
        Binding::AwaitModule(_) => None,
    }
}

/// The module keys every window has: `e` the editor, `a` the agent, `t` the terminal
/// (`neovibe_core::layout::ModuleKeys::built_in`).
const BUILT_IN_MODULE_KEYS: [char; 3] = ['e', 'a', 't'];

/// Every character [`binding`] recognises after the prefix -- derived by actually calling it
/// rather than a second, hand-typed list, so `main.rs`'s own two-way check against `PREFIX_KEYS`
/// (`agent-ui/web/src/keymap.ts`) cannot itself drift from this table by restating it (item 3f:
/// deleting both of `text_size.rs`'s `PREFIX_KEYS` rows stayed green until this existed, because
/// nothing previously checked that direction at all). `\` and `"` are in it (spec §6.5: "`bound_keys()`
/// must also report `\` and `"`"). Test-only: nothing in the running product needs the bound-keys
/// list itself, only this crate's own tests (here and in `main.rs`) do.
#[cfg(test)]
pub(crate) fn bound_keys() -> Vec<char> {
    (0u8..=127u8)
        .map(char::from)
        .filter(|&ch| binding(ch).is_some())
        .collect()
}

/// The keys that wait for a module key, and for each, the module keys it takes -- found by driving
/// a [`Prefix`] with no Lua keys through `Ctrl+a`, the key, and every character (spec §6.5: "the
/// module keys must be checked through the `AwaitModule` state, not only through `command()`").
#[cfg(test)]
pub(crate) fn module_keys_after() -> Vec<(char, Vec<char>)> {
    let now = Instant::now();
    (0u8..=127u8)
        .map(char::from)
        .filter(|&ch| matches!(binding(ch), Some(Binding::AwaitModule(_))))
        .map(|split| {
            let taken = (0u8..=127u8)
                .map(char::from)
                .filter(|&ch| {
                    let mut prefix = Prefix::new();
                    prefix.press(PrefixKey::Prefix, 0, now);
                    prefix.press(PrefixKey::Plain(split), 1, now);
                    // A key that names itself: a capital taken for the module key it capitalizes
                    // (Shift still down from `"`) is a fallback, not a key of its own.
                    matches!(
                        prefix.press(PrefixKey::Plain(ch), 2, now),
                        Outcome::Run(PrefixCommand::Place { key, .. }) if key == ch
                    )
                })
                .collect();
            (split, taken)
        })
        .collect()
}

/// Every letter whose `Ctrl` chord right after `Ctrl+a` runs a command: `a` (the literal `Ctrl+a`)
/// and, since the bottom terminal, `l`. Derived by driving [`Prefix`] itself, for the reason
/// [`bound_keys`] is derived from [`command`]: `main.rs` holds `PREFIX_KEYS`' `Ctrl+a Ctrl+<x>`
/// rows to it, which [`bound_keys`] cannot, since those tokens are not one character.
#[cfg(test)]
pub(crate) fn bound_control_keys() -> Vec<char> {
    ('a'..='z')
        .filter(|&letter| {
            let now = Instant::now();
            let mut prefix = Prefix::new();
            prefix.press(PrefixKey::Prefix, 0, now);
            let key = if letter == 'a' {
                PrefixKey::Prefix
            } else {
                PrefixKey::Control(letter)
            };
            matches!(prefix.press(key, 1, now), Outcome::Run(_))
        })
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Idle,
    Armed,
    /// After `\` or `"`: the next key names the module to place along `axis`.
    AwaitModule {
        axis: Axis,
    },
    /// After a repeatable command: another repeatable key before `until` runs without the prefix.
    Repeat {
        until: Instant,
    },
}

/// What the prefix is waiting for: what the top bar shows (spec §6.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Waiting {
    /// Nothing (repeat mode included, which tmux does not show either).
    No,
    /// A command key: the bar's indicator, and the strip of module keys and verbs.
    Command,
    /// A module key after `\` or `"`: the strip shows the module keys alone.
    Module(Axis),
}

#[derive(Debug)]
pub(crate) struct Prefix {
    state: State,
    /// The hardware keycode of the last key this machine consumed, until it is released. Its
    /// auto-repeat is not a new key: holding `Ctrl+a` must not turn into `Ctrl+a Ctrl+a` (§3.1).
    held: Option<u32>,
    /// Every key that names a module: the built-ins, then each Lua panel's.
    module_keys: Vec<char>,
}

impl Prefix {
    /// The prefix with the built-in module keys only: what every test here drives.
    #[cfg(test)]
    pub(crate) fn new() -> Self {
        Prefix::with_module_keys(&[])
    }

    /// The prefix, answering also to each Lua panel's key (`ModuleKeys::lua_keys`, already checked
    /// against the reserved set, so none of them is a binding of its own).
    pub(crate) fn with_module_keys(lua_keys: &[char]) -> Self {
        Prefix {
            state: State::Idle,
            held: None,
            module_keys: BUILT_IN_MODULE_KEYS.iter().chain(lua_keys).copied().collect(),
        }
    }

    /// Whether the prefix is waiting for its next key: what the top bar shows (§3.1). Repeat mode
    /// is not shown, as tmux does not show it.
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
                    self.state = State::Repeat {
                        until: now + REPEAT_TIME,
                    };
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
                    PrefixKey::Control('l') => Outcome::Run(PrefixCommand::SendCtrlL),
                    PrefixKey::Plain(ch) => match binding(ch) {
                        Some(Binding::Run(cmd, repeatable)) => {
                            if repeatable {
                                self.state = State::Repeat {
                                    until: now + REPEAT_TIME,
                                };
                            }
                            Outcome::Run(cmd)
                        }
                        Some(Binding::AwaitModule(axis)) => {
                            self.state = State::AwaitModule { axis };
                            Outcome::Swallow
                        }
                        // A Lua panel's key: never one of the table's (they are reserved).
                        None if self.module_keys.contains(&ch) => Outcome::Run(PrefixCommand::Module(ch)),
                        None => Outcome::Swallow,
                    },
                    _ => Outcome::Swallow,
                }
            }
            State::AwaitModule { axis } => {
                if key == PrefixKey::Modifier {
                    return Outcome::Swallow;
                }
                self.held = Some(keycode);
                self.state = State::Idle;
                match key {
                    PrefixKey::Plain(ch) if self.module_keys.contains(&ch) => {
                        Outcome::Run(PrefixCommand::Place { key: ch, axis })
                    }
                    // `"` is Shift+' on the owner's layout, so the key after it often arrives with
                    // Shift still down: `A` for `a`. tmux's `"` takes no second key and never meets
                    // this. A Lua panel that registered the capital itself was matched just above
                    // (never a built-in's capital: `ModuleKeys::build` refuses those).
                    PrefixKey::Plain(ch) if self.module_keys.contains(&ch.to_ascii_lowercase()) => {
                        Outcome::Run(PrefixCommand::Place {
                            key: ch.to_ascii_lowercase(),
                            axis,
                        })
                    }
                    // Not a module key, `Esc` included: swallowed, and the wait is over (spec §6.3).
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
                    self.state = State::Repeat {
                        until: now + REPEAT_TIME,
                    };
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

/// Installs the prefix on `window`, answering also to `lua_keys` (each Lua panel's module key).
/// `on_command` runs a command; `on_waiting` hears every change of `waiting()` (the top bar's
/// indicator and its strip). Cancelled by the window losing activation and by any click -- the
/// click still does what it does, it just also ends the wait (§3.1).
pub(crate) fn install(
    window: &gtk4::ApplicationWindow,
    lua_keys: &[char],
    on_command: impl Fn(PrefixCommand) + 'static,
    on_waiting: impl Fn(Waiting) + 'static,
) -> Rc<RefCell<Prefix>> {
    let prefix = Rc::new(RefCell::new(Prefix::with_module_keys(lua_keys)));
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
                // While the prefix is armed this same capture controller swallows every key that is
                // not one of its own bindings -- by design, matching tmux -- so an arm that never
                // disarms is indistinguishable from the window refusing input. Logged both ways
                // since 2026-09-20, when a report of exactly that symptom could not be told apart
                // from a stuck HINT or a stranded focus, because none of the three said anything.
                println!("[prefix] armed={} waiting={now:?}", now != Waiting::No);
                on_waiting(now);
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
    const Q: u32 = 24;
    const ESC: u32 = 9;
    const F11: u32 = 95;
    const SHIFT_L: u32 = 50;
    const EQUALS: u32 = 21;
    const MINUS: u32 = 20;
    const ZERO: u32 = 19;
    const T: u32 = 28;

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
        assert_eq!(classify(Key::h, ModifierType::CONTROL_MASK), PrefixKey::Control('h'));
        assert_eq!(classify(Key::l, ModifierType::CONTROL_MASK), PrefixKey::Control('l'));
        assert_eq!(
            classify(Key::l, ModifierType::CONTROL_MASK | ModifierType::SHIFT_MASK),
            PrefixKey::Other
        );
        assert_eq!(classify(Key::m, ModifierType::empty()), PrefixKey::Plain('m'));
        assert_eq!(classify(Key::M, ModifierType::SHIFT_MASK), PrefixKey::Plain('M'));
        assert_eq!(classify(Key::Escape, ModifierType::empty()), PrefixKey::Escape);
        assert_eq!(classify(Key::Shift_L, ModifierType::SHIFT_MASK), PrefixKey::Modifier);
        assert_eq!(
            classify(Key::Control_L, ModifierType::CONTROL_MASK),
            PrefixKey::Modifier
        );
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
            (PrefixKey::Plain('='), EQUALS, PrefixCommand::TextSize(TextStep::Larger)),
            (PrefixKey::Plain('-'), MINUS, PrefixCommand::TextSize(TextStep::Smaller)),
            (PrefixKey::Plain('0'), ZERO, PrefixCommand::TextSize(TextStep::Reset)),
            (PrefixKey::Plain('t'), T, PrefixCommand::ToggleTerminal),
            (PrefixKey::Control('l'), L, PrefixCommand::SendCtrlL),
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
        for (key, keycode) in [(PrefixKey::Plain('q'), Q), (PrefixKey::Other, F11)] {
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
        assert_eq!(
            p.press(PrefixKey::Plain('h'), H, t0),
            Outcome::Run(PrefixCommand::Resize(Direction::Left))
        );
        p.release(H);

        let t1 = t0 + Duration::from_millis(400);
        assert_eq!(
            p.press(PrefixKey::Plain('h'), H, t1),
            Outcome::Run(PrefixCommand::Resize(Direction::Left))
        );
        p.release(H);

        // The window restarts from t1, not t0: another repeatable key 400ms after THAT still runs.
        let t2 = t1 + Duration::from_millis(400);
        assert_eq!(
            p.press(PrefixKey::Plain('l'), L, t2),
            Outcome::Run(PrefixCommand::Resize(Direction::Right))
        );
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

    /// `=`/`-` repeat like `m`/`h`/`j`/`k`/`l`; `0` is one-shot like `z` (zoom-together spec §3).
    #[test]
    fn equals_and_minus_repeat_and_zero_does_not() {
        let t0 = Instant::now();

        for (ch, keycode, expected) in [
            ('=', EQUALS, PrefixCommand::TextSize(TextStep::Larger)),
            ('-', MINUS, PrefixCommand::TextSize(TextStep::Smaller)),
        ] {
            let mut p = Prefix::new();
            p.press(PrefixKey::Prefix, CTRL_A, t0);
            p.press(PrefixKey::Plain(ch), keycode, t0);
            p.release(keycode);
            let t1 = t0 + Duration::from_millis(100);
            assert_eq!(p.press(PrefixKey::Plain(ch), keycode, t1), Outcome::Run(expected));
        }

        let mut p = Prefix::new();
        p.press(PrefixKey::Prefix, CTRL_A, t0);
        p.press(PrefixKey::Plain('0'), ZERO, t0);
        p.release(ZERO);
        let t1 = t0 + Duration::from_millis(100);
        assert_eq!(p.press(PrefixKey::Plain('0'), ZERO, t1), Outcome::Pass);
    }

    #[test]
    fn a_non_repeatable_key_in_repeat_mode_ends_it_and_passes() {
        let t0 = Instant::now();
        let mut p = Prefix::new();
        p.press(PrefixKey::Prefix, CTRL_A, t0);
        assert_eq!(
            p.press(PrefixKey::Plain('h'), H, t0),
            Outcome::Run(PrefixCommand::Resize(Direction::Left))
        );
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
        assert_eq!(
            p.press(PrefixKey::Prefix, CTRL_A, t0),
            Outcome::Run(PrefixCommand::SendPrefix)
        );

        let mut p = Prefix::new();
        p.press(PrefixKey::Prefix, CTRL_A, t0);
        assert_eq!(
            p.press(PrefixKey::Plain('h'), H, t0),
            Outcome::Run(PrefixCommand::Resize(Direction::Left))
        );
        // Holding h (same keycode, no release) keeps running the command.
        assert_eq!(
            p.press(PrefixKey::Plain('h'), H, t0),
            Outcome::Run(PrefixCommand::Resize(Direction::Left))
        );

        let mut p = Prefix::new();
        p.press(PrefixKey::Prefix, CTRL_A, t0);
        assert_eq!(p.press(PrefixKey::Plain('q'), Q, t0), Outcome::Swallow);
        // Holding an unbound key is swallowed too, not passed on a second time. (`q`, not `x`, since
        // modules P2 bound `x`: `q` stays unbound on purpose.)
        assert_eq!(p.press(PrefixKey::Plain('q'), Q, t0), Outcome::Swallow);
    }

    /// A key down when the window loses focus produces no release, so `cancel` has to forget it --
    /// otherwise its next real press is swallowed as that key's own auto-repeat.
    #[test]
    fn cancel_forgets_a_key_that_may_never_be_released() {
        let t0 = Instant::now();
        let mut p = Prefix::new();
        assert_eq!(p.press(PrefixKey::Prefix, CTRL_A, t0), Outcome::Swallow);
        p.cancel();
        assert_eq!(
            p.press(PrefixKey::Prefix, CTRL_A, t0),
            Outcome::Swallow,
            "a fresh press arms again"
        );
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

    #[test]
    fn bound_keys_names_exactly_the_commands_table() {
        let mut keys = bound_keys();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                '"', '-', '0', '=', 'H', 'J', 'K', 'L', '\\', '_', 'a', 'e', 'h', 'j', 'k', 'l', 'm', 't', 'x', 'z',
                '|'
            ]
        );
    }

    /// Every key the prefix binds is one no Lua panel may take (spec §6.4), so a panel's key can
    /// never shadow one of neovibe's own.
    #[test]
    fn every_bound_key_is_reserved_against_lua_panels() {
        for ch in bound_keys() {
            assert!(
                neovibe_core::layout::keys::RESERVED.contains(&ch),
                "{ch:?} is bound after Ctrl+a but a Lua panel could take it"
            );
        }
        let built_in: Vec<char> = neovibe_core::layout::ModuleKeys::built_in()
            .entries()
            .iter()
            .map(|(k, _)| *k)
            .collect();
        assert_eq!(built_in, BUILT_IN_MODULE_KEYS, "one list of built-in module keys");
    }

    /// Modules P2 (spec §6.3): each new key after the prefix runs its command once and disarms;
    /// none repeats, so a second press inside the repeat window is typed, not run again.
    #[test]
    fn the_module_verbs_run_once_and_do_not_repeat() {
        let t0 = Instant::now();
        let cases = [
            ('x', PrefixCommand::Hide),
            ('e', PrefixCommand::Module('e')),
            ('a', PrefixCommand::Module('a')),
            ('H', PrefixCommand::Swap(Direction::Left)),
            ('J', PrefixCommand::Swap(Direction::Down)),
            ('K', PrefixCommand::Swap(Direction::Up)),
            ('L', PrefixCommand::Swap(Direction::Right)),
            ('|', PrefixCommand::Even(Axis::Row)),
            ('_', PrefixCommand::Even(Axis::Column)),
        ];
        for (ch, expected) in cases {
            let mut p = Prefix::new();
            p.press(PrefixKey::Prefix, CTRL_A, t0);
            assert_eq!(p.press(PrefixKey::Plain(ch), X, t0), Outcome::Run(expected), "{ch}");
            assert!(!p.is_armed());
            p.release(X);
            assert_eq!(
                p.press(PrefixKey::Plain(ch), X, t0 + Duration::from_millis(100)),
                Outcome::Pass,
                "{ch} does not repeat"
            );
        }
    }

    /// `Ctrl+a \ <key>` and `Ctrl+a " <key>`: the prefix waits for a module key, and says so; a
    /// module key places that module; anything else -- `Esc`, a verb, an unknown key -- is swallowed
    /// and ends the wait (spec §6.3's mechanics).
    #[test]
    fn a_split_key_waits_for_a_module_key() {
        let t0 = Instant::now();
        for (split, axis) in [('\\', Axis::Row), ('"', Axis::Column)] {
            for (key, keycode, expected) in [
                (
                    PrefixKey::Plain('a'),
                    X,
                    Outcome::Run(PrefixCommand::Place { key: 'a', axis }),
                ),
                (
                    PrefixKey::Plain('t'),
                    T,
                    Outcome::Run(PrefixCommand::Place { key: 't', axis }),
                ),
                (PrefixKey::Plain('x'), X, Outcome::Swallow),
                (PrefixKey::Plain('q'), X, Outcome::Swallow),
                (PrefixKey::Escape, ESC, Outcome::Swallow),
            ] {
                let mut p = Prefix::new();
                assert_eq!(p.waiting(), Waiting::No);
                p.press(PrefixKey::Prefix, CTRL_A, t0);
                // Armed, it waits for a command: the strip lists the module keys AND the verbs
                // (Task 9's review, minor 3 -- no test told `Command` from `Module(..)` here).
                assert_eq!(p.waiting(), Waiting::Command);
                assert_eq!(p.press(PrefixKey::Plain(split), M, t0), Outcome::Swallow);
                assert_eq!(p.waiting(), Waiting::Module(axis));
                assert_eq!(p.press(PrefixKey::Modifier, SHIFT_L, t0), Outcome::Swallow);
                assert_eq!(
                    p.waiting(),
                    Waiting::Module(axis),
                    "a bare modifier is not the next key"
                );
                assert_eq!(p.press(key, keycode, t0), expected, "{split} then {key:?}");
                assert_eq!(p.waiting(), Waiting::No);
            }
        }
    }

    /// `"` is Shift+' on the owner's layout, so `Ctrl+a " a` often arrives as `" A`, Shift released a
    /// beat late. tmux never meets this -- its `"` takes no second key -- so the capital is taken as
    /// the module key it capitalizes (the plan review's second round, finding 4). A Lua panel that
    /// registered the capital itself is matched exactly first -- never a built-in's capital, which
    /// `ModuleKeys::build` refuses (`E`/`A`/`T`/`C`), so `" A` is always the chat -- and a capital
    /// naming nothing is swallowed.
    #[test]
    fn a_module_key_still_shifted_from_the_quote_names_its_module() {
        let t0 = Instant::now();
        let place = |key| {
            Outcome::Run(PrefixCommand::Place {
                key,
                axis: Axis::Column,
            })
        };
        let none: &[char] = &[];
        for (lua_keys, pressed, expected) in [
            (none, 'A', place('a')),
            (none, 'T', place('t')),
            (&['g', 'G'][..], 'G', place('G')),
            (&['g'][..], 'G', place('g')),
            (none, 'Q', Outcome::Swallow),
        ] {
            let mut p = Prefix::with_module_keys(lua_keys);
            p.press(PrefixKey::Prefix, CTRL_A, t0);
            p.press(PrefixKey::Plain('"'), M, t0);
            assert_eq!(
                p.press(PrefixKey::Plain(pressed), X, t0),
                expected,
                "{lua_keys:?} then {pressed}"
            );
        }
    }

    /// A Lua panel's key is a module key like `e`/`a`/`t`: on its own, and after `\`/`"`. A
    /// window without that panel swallows it like any unbound key.
    #[test]
    fn a_lua_panels_key_names_its_module() {
        let t0 = Instant::now();
        let mut p = Prefix::with_module_keys(&['N']);
        p.press(PrefixKey::Prefix, CTRL_A, t0);
        assert_eq!(
            p.press(PrefixKey::Plain('N'), X, t0),
            Outcome::Run(PrefixCommand::Module('N'))
        );
        p.release(X);
        p.press(PrefixKey::Prefix, CTRL_A, t0);
        p.press(PrefixKey::Plain('"'), M, t0);
        assert_eq!(
            p.press(PrefixKey::Plain('N'), X, t0),
            Outcome::Run(PrefixCommand::Place {
                key: 'N',
                axis: Axis::Column
            })
        );
        let mut plain = Prefix::new();
        plain.press(PrefixKey::Prefix, CTRL_A, t0);
        assert_eq!(plain.press(PrefixKey::Plain('N'), X, t0), Outcome::Swallow);
    }

    /// `q` is deliberately unbound (decision c: in his tmux it is `kill-window`); `c` waits for P3's
    /// canvas; `-` stays text size, not split-below.
    #[test]
    fn q_and_c_are_swallowed_and_minus_is_still_text_size() {
        let t0 = Instant::now();
        for ch in ['q', 'c'] {
            let mut p = Prefix::new();
            p.press(PrefixKey::Prefix, CTRL_A, t0);
            assert_eq!(p.press(PrefixKey::Plain(ch), X, t0), Outcome::Swallow, "{ch}");
        }
        let mut p = Prefix::new();
        p.press(PrefixKey::Prefix, CTRL_A, t0);
        assert_eq!(
            p.press(PrefixKey::Plain('-'), MINUS, t0),
            Outcome::Run(PrefixCommand::TextSize(TextStep::Smaller))
        );
    }

    #[test]
    fn the_split_keys_take_every_built_in_module_key() {
        let mut found = module_keys_after();
        for (_, keys) in &mut found {
            keys.sort_unstable();
        }
        assert_eq!(found, [('"', vec!['a', 'e', 't']), ('\\', vec!['a', 'e', 't'])]);
    }

    #[test]
    fn bound_control_keys_are_ctrl_a_and_ctrl_l() {
        assert_eq!(bound_control_keys(), ['a', 'l']);
    }

    /// `Ctrl+l` is the prefix's only after `Ctrl+a`: idle, and in a repeat window, it passes to the
    /// pane exactly as every other Ctrl chord does, and any other Ctrl letter after the prefix is
    /// swallowed like any unbound key.
    #[test]
    fn ctrl_l_is_the_prefixs_only_right_after_ctrl_a() {
        let t0 = Instant::now();
        let mut p = Prefix::new();
        assert_eq!(p.press(PrefixKey::Control('l'), L, t0), Outcome::Pass);

        let mut p = Prefix::new();
        p.press(PrefixKey::Prefix, CTRL_A, t0);
        assert_eq!(p.press(PrefixKey::Control('x'), X, t0), Outcome::Swallow);
        assert!(!p.is_armed());

        let mut p = Prefix::new();
        p.press(PrefixKey::Prefix, CTRL_A, t0);
        p.press(PrefixKey::Plain('h'), H, t0);
        p.release(H);
        assert_eq!(
            p.press(PrefixKey::Control('l'), L, t0 + Duration::from_millis(100)),
            Outcome::Pass
        );
    }

    /// `t` is one-shot: a second `t` inside the repeat window is typed, not a second toggle.
    #[test]
    fn t_does_not_repeat() {
        let t0 = Instant::now();
        let mut p = Prefix::new();
        p.press(PrefixKey::Prefix, CTRL_A, t0);
        assert_eq!(
            p.press(PrefixKey::Plain('t'), T, t0),
            Outcome::Run(PrefixCommand::ToggleTerminal)
        );
        p.release(T);
        assert_eq!(
            p.press(PrefixKey::Plain('t'), T, t0 + Duration::from_millis(100)),
            Outcome::Pass
        );
    }
}
