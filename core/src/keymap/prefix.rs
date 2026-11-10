//! The prefix: stock tmux's `Ctrl+b` by default, or whatever `init.lua`'s `eitri.keymap.prefix` set,
//! then one key looked up in the effective prefix table (`Keymap`). The table is data; this file is
//! the state machine tmux runs over it: arm on the prefix, run the key's action, stay in a 500ms
//! repeat window after a `-r` binding, swallow an unbound key and end the wait, `Esc` cancels.
//!
//! After a split key (`split.right`/`split.below`, `%`/`"` by default) the prefix waits for a module
//! key -- `e a t` and each Lua panel's `key` -- and a capital is taken for its lowercase when no Lua
//! panel registered the capital itself (`"` is Shift+' on a US layout). The hide key
//! (`module.hide-by-key`, `d` by default) waits for a module key the same way, read the same way, and
//! names the module to hide. A Lua panel's key also
//! runs its module directly, since no binding can have it (the reserved-set check,
//! `ModuleKeys::build`).
//!
//! `init.lua`'s root table (`Keymap::root_lookup`) adds keys that need no prefix: with nothing armed, a root key
//! runs its action (a split root key waits for its module key like a split after the prefix), and a root key
//! pressed right after the prefix is sent to the pane with the keys instead, so nvim or a shell can still have
//! the chord. A host that cannot run some actions says so with `set_root_keep`; a root key whose action it
//! refuses passes on untouched. The auto-repeat of a held root key is swallowed like any consumed key's.
//!
//! A run of text size steps on Ctrl chords (`text.*` after the prefix, with Ctrl held) lasts while Ctrl stays down as
//! well as for the repeat time after its last step, whichever ends later: a person sizing a pane with Ctrl held has
//! not left the pane by pausing to look. Every other `-r` key keeps tmux's timer. Hosts report Ctrl's release through
//! [`Prefix::ctrl_released`]; a release lost to another window is covered by [`Prefix::cancel`].
//!
//! Presses arrive already translated (`Press`), and a consumed key is remembered by its hardware
//! keycode, not its name: an auto-repeat of a held key carries the same keycode however the layout
//! or modifiers rename it, and a release must find the key it ends. Nothing here knows a toolkit:
//! the host turns its own key events into `Press` (shell's GDK translation) and feeds them in.

use std::rc::Rc;
use std::time::{Duration, Instant};

use crate::keymap::action::SplitCwd;
use crate::keymap::{root, Action, KeyName, KeySpec, Keymap};
use crate::layout::{Axis, ModuleId, ModuleKeys, ModuleKind};

/// tmux's default `repeat-time`.
pub const REPEAT_TIME: Duration = Duration::from_millis(500);

/// tmux's default time for the pane numbers to remain on screen.
pub const DISPLAY_PANES_TIME: Duration = Duration::from_millis(1000);

/// One key press as the prefix reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Press {
    /// A key tmux can name: Ctrl/Alt as `C-`/`M-`; Shift folded into a character, kept on a named key.
    Key(KeySpec),
    Escape,
    Modifier,
    /// Ctrl and Shift held on a text size key, where Shift typed the character: `=` is Shift+0 on a German keyboard,
    /// `0` is Shift+à on a French one. After the prefix it reads as the chord it names. With nothing armed it is no
    /// key of the prefix's unless a user root key names it, so an accelerator still gets it. `+` this way is a US
    /// keyboard's `Ctrl+Shift+=`, which only the prefix reads.
    ShiftTyped(KeySpec),
    /// Super/Hyper/Meta held, `C-S-<char>`, or a key with no tmux name.
    Other,
}

/// What the workspace chooser was answered with: a 0-based position in the window's workspace order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspacePick {
    Switch(usize),
    New,
    Delete(usize),
}

/// What the prefix hands `main.rs` to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrefixCommand {
    Run(Action),
    Number(char),
    Workspace(WorkspacePick),
    /// After a split key and a module key: open `module` next to the module with the keys, or move it.
    Place {
        module: ModuleId,
        axis: Axis,
        cwd: SplitCwd,
    },
    /// After the hide key and a module key: hide the module that key names.
    Hide {
        module: ModuleId,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Not the prefix's: let the key go where it was going.
    Pass,
    /// The prefix's, and it does nothing (arming, an unbound key, `Esc`, a held key's repeat).
    Swallow,
    Run(PrefixCommand),
}

/// Which keys wait for a module key after them, by what the window can place and hide. A key that does not wait runs
/// as its action at once, so the window's own rule for that action answers the press.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ModuleWaits {
    /// A split key and the hide key both wait: the window lays modules out.
    #[default]
    SplitAndHide,
    /// Only the hide key waits: the window places nothing, but has a module to hide.
    Hide,
    /// Neither waits: the window holds one module.
    Neither,
}

/// The key `send-prefix` / `send-keys` hands the pane with the keys: `send-prefix` sends the configured prefix chord
/// itself, so a user who rebound the prefix still reaches a program inside the pane that wants it.
pub fn literal_for(action: &Action, keymap: &Keymap) -> Option<KeySpec> {
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
        cwd: SplitCwd,
    },
    /// After the hide key: the next key names the module to hide.
    AwaitHide,
    AwaitWorkspace {
        deleting: bool,
    },
    AwaitNumber {
        until: Instant,
    },
    /// After a repeatable binding: another repeatable key before `until` runs without the prefix. `ctrl`: a run of
    /// text size steps on Ctrl chords with Ctrl still down, which `until` does not end.
    Repeat {
        until: Instant,
        ctrl: bool,
    },
}

/// What the prefix is waiting for: what the top bar shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Waiting {
    /// Nothing (repeat mode included, which tmux does not show either).
    No,
    Command,
    Module(Axis),
    /// After the hide key: a module key names the module to hide.
    Hide,
    Number,
    Workspace {
        deleting: bool,
    },
}

pub struct Prefix {
    state: State,
    /// Which actions this host runs for a root key; a root key whose action it refuses is not a root key here.
    root_keep: Rc<dyn Fn(&Action) -> bool>,
    /// Whether the built-in root keys the prefix runs itself (`root::key_stage_action`) are this host's.
    key_stage_roots: bool,
    /// The hardware keycode of the last key this machine consumed, until it is released. Its
    /// auto-repeat is not a new key: holding the prefix must not turn into `prefix prefix`.
    held: Option<u32>,
    keymap: Rc<Keymap>,
    module_keys: Rc<ModuleKeys>,
    waits: ModuleWaits,
}

impl Prefix {
    pub fn new(keymap: Rc<Keymap>, module_keys: Rc<ModuleKeys>) -> Self {
        Prefix {
            state: State::Idle,
            root_keep: Rc::new(|_| true),
            key_stage_roots: true,
            held: None,
            keymap,
            module_keys,
            waits: ModuleWaits::default(),
        }
    }

    /// Which keys wait for a module key from the next press on.
    pub fn set_module_waits(&mut self, waits: ModuleWaits) {
        self.waits = waits;
    }

    /// Limits root keys to the actions `keep` accepts (a companion window has no layout to run most of them on).
    pub fn set_root_keep(&mut self, keep: Rc<dyn Fn(&Action) -> bool>) {
        self.root_keep = keep;
    }

    /// Whether this host has the built-in chords at all. A host without them leaves `Ctrl++` to the pane, as it does
    /// `Ctrl+=`.
    pub fn set_key_stage_roots(&mut self, on: bool) {
        self.key_stage_roots = on;
    }

    pub fn is_armed(&self) -> bool {
        self.waiting() != Waiting::No
    }

    pub fn waiting(&self) -> Waiting {
        match self.state {
            State::AwaitWorkspace { deleting } => Waiting::Workspace { deleting },
            State::Armed => Waiting::Command,
            State::AwaitNumber { .. } => Waiting::Number,
            State::AwaitModule { axis, .. } => Waiting::Module(axis),
            State::AwaitHide => Waiting::Hide,
            State::Idle | State::Repeat { .. } => Waiting::No,
        }
    }

    /// Wait without a timer after the host draws the workspace chooser.
    pub fn await_workspace(&mut self) {
        self.state = State::AwaitWorkspace { deleting: false };
    }

    /// Start waiting after the host draws the digits. A window that refuses the action never
    /// calls this, so it never eats a digit.
    pub fn await_number(&mut self, now: Instant) {
        self.state = State::AwaitNumber {
            until: now + DISPLAY_PANES_TIME,
        };
    }

    /// End a number wait once its display time is past; report whether one ended.
    pub fn expire(&mut self, now: Instant) -> bool {
        if let State::AwaitNumber { until } = self.state {
            if now > until {
                self.state = State::Idle;
                return true;
            }
        }
        false
    }

    pub fn cancel(&mut self) {
        self.state = State::Idle;
        // The window losing focus is exactly when the release of a key that is down never arrives.
        self.held = None;
    }

    pub fn release(&mut self, keycode: u32) {
        if self.held == Some(keycode) {
            self.held = None;
        }
    }

    /// Whether `keycode` is the key this machine consumed last and has not seen released: the one release
    /// [`Prefix::release`] acts on, so a host need report no other.
    pub fn holds(&self, keycode: u32) -> bool {
        self.held == Some(keycode)
    }

    /// Whether a run is waiting on Ctrl: the one time a host need report Ctrl's release.
    pub fn watches_ctrl(&self) -> bool {
        matches!(self.state, State::Repeat { ctrl: true, .. })
    }

    /// Ctrl went up: a text size run has what is left of its repeat time.
    pub fn ctrl_released(&mut self) {
        if let State::Repeat { ctrl, .. } = &mut self.state {
            *ctrl = false;
        }
    }

    /// The module a key names while a wait for a module key is up: the exact key first (a Lua panel may own a
    /// capital), then a capital's lowercase.
    fn module_named(&self, press: Press) -> Option<ModuleId> {
        let Press::Key(key) = press else { return None };
        let c = key.as_char()?;
        self.module_keys
            .module(c)
            .or_else(|| self.module_keys.module(c.to_ascii_lowercase()))
            .cloned()
    }

    fn repeatable(&self, press: Press) -> Option<Action> {
        let (Press::Key(key) | Press::ShiftTyped(key)) = press else {
            return None;
        };
        self.keymap
            .lookup(&key)
            .filter(|b| b.repeatable)
            .map(|b| b.action.clone())
    }

    /// The action of the root key `press` is, when this host keeps it: the user's root table first, so `init.lua` can
    /// take a text size chord, then the built-in chords run here. A chord whose user action the host refuses keeps its
    /// built-in one, as an accelerator's does.
    fn root_action(&self, press: Press) -> Option<Action> {
        let key = match press {
            Press::Key(key) => key,
            Press::ShiftTyped(key) if key.key != KeyName::Char('+') => key,
            _ => return None,
        };
        let built_in = || {
            if self.key_stage_roots && matches!(press, Press::Key(_)) {
                root::key_stage_action(&key)
            } else {
                None
            }
        };
        let keep = |action: &Action| (self.root_keep)(action);
        self.keymap
            .root_lookup(&key)
            .map(|b| b.action.clone())
            .filter(keep)
            .or_else(|| built_in().filter(keep))
    }

    /// The run a repeatable `action` starts or goes on with, from a press at `now`.
    fn run_state(press: Press, action: &Action, now: Instant) -> State {
        let on_ctrl = matches!(press, Press::Key(key) | Press::ShiftTyped(key) if key.ctrl);
        State::Repeat {
            until: now + REPEAT_TIME,
            ctrl: on_ctrl && action.is_text_size(),
        }
    }

    /// A root key pressed with nothing armed: consumed, and run (a split waits for its module key first).
    fn run_root(&mut self, action: Action, keycode: u32) -> Outcome {
        self.held = Some(keycode);
        match action {
            Action::ModuleHideByKey if self.waits != ModuleWaits::Neither => {
                self.state = State::AwaitHide;
                Outcome::Swallow
            }
            Action::Split { axis, cwd } => {
                self.state = State::AwaitModule { axis, cwd };
                Outcome::Swallow
            }
            action => {
                self.state = State::Idle;
                Outcome::Run(PrefixCommand::Run(action))
            }
        }
    }

    pub fn press(&mut self, press: Press, keycode: u32, now: Instant) -> Outcome {
        if let State::Repeat { until, ctrl: false } | State::AwaitNumber { until } = self.state {
            if now > until {
                self.state = State::Idle;
            }
        }
        let is_prefix = matches!(press, Press::Key(key) if key == *self.keymap.prefix());
        if self.held == Some(keycode) {
            // A consumed key's auto-repeat: a repeatable binding keeps repeating, as under tmux's -r.
            if let State::Repeat { .. } = self.state {
                if let Some(action) = self.repeatable(press) {
                    self.state = Self::run_state(press, &action, now);
                    return Outcome::Run(PrefixCommand::Run(action));
                }
            }
            // A text size root key steps while held, as the chords an accelerator takes do.
            if self.state == State::Idle {
                if let Some(action) = self.root_action(press).filter(Action::is_text_size) {
                    return Outcome::Run(PrefixCommand::Run(action));
                }
            }
            return Outcome::Swallow;
        }
        match self.state {
            State::AwaitWorkspace { deleting } => {
                if press == Press::Modifier {
                    return Outcome::Swallow;
                }
                self.held = Some(keycode);
                self.state = State::Idle;
                if let Press::Key(key) = press {
                    match key.as_char() {
                        Some(c @ '0'..='9') => {
                            let index = if c == '0' { 9 } else { (c as u8 - b'1') as usize };
                            let pick = if deleting {
                                WorkspacePick::Delete(index)
                            } else {
                                WorkspacePick::Switch(index)
                            };
                            return Outcome::Run(PrefixCommand::Workspace(pick));
                        }
                        Some('c') if !deleting => return Outcome::Run(PrefixCommand::Workspace(WorkspacePick::New)),
                        Some('x') if !deleting => self.state = State::AwaitWorkspace { deleting: true },
                        _ => {}
                    }
                }
                Outcome::Swallow
            }
            State::AwaitNumber { .. } => {
                if press == Press::Modifier {
                    return Outcome::Swallow;
                }
                self.held = Some(keycode);
                self.state = State::Idle;
                if let Press::Key(key) = press {
                    if let Some(c) = key.as_char().filter(char::is_ascii_digit) {
                        return Outcome::Run(PrefixCommand::Number(c));
                    }
                }
                Outcome::Swallow
            }
            State::Idle => {
                if is_prefix {
                    self.state = State::Armed;
                    self.held = Some(keycode);
                    return Outcome::Swallow;
                }
                match self.root_action(press) {
                    Some(action) => self.run_root(action, keycode),
                    None => Outcome::Pass,
                }
            }
            State::Armed => {
                if press == Press::Modifier {
                    return Outcome::Swallow;
                }
                self.held = Some(keycode);
                self.state = State::Idle;
                let (Press::Key(key) | Press::ShiftTyped(key)) = press else {
                    return Outcome::Swallow;
                };
                match self.keymap.lookup(&key) {
                    Some(binding) => match binding.action {
                        Action::Split { axis, cwd } if self.waits == ModuleWaits::SplitAndHide => {
                            self.state = State::AwaitModule { axis, cwd };
                            Outcome::Swallow
                        }
                        Action::ModuleHideByKey if self.waits != ModuleWaits::Neither => {
                            self.state = State::AwaitHide;
                            Outcome::Swallow
                        }
                        ref action => {
                            if binding.repeatable {
                                self.state = Self::run_state(press, action, now);
                            }
                            Outcome::Run(PrefixCommand::Run(action.clone()))
                        }
                    },
                    // A root key is what the prefix is for sending: the pane with the keys gets the chord itself.
                    None if self.root_action(press).is_some() => {
                        Outcome::Run(PrefixCommand::Run(Action::SendKeys(key)))
                    }
                    None => match key.as_char().and_then(|c| self.module_keys.module(c)) {
                        Some(id) if id.kind() == ModuleKind::LuaWebview => {
                            Outcome::Run(PrefixCommand::Run(Action::Module(id.clone())))
                        }
                        _ => Outcome::Swallow,
                    },
                }
            }
            State::AwaitModule { axis, cwd } => {
                if press == Press::Modifier {
                    return Outcome::Swallow;
                }
                self.held = Some(keycode);
                self.state = State::Idle;
                match self.module_named(press) {
                    Some(module) => Outcome::Run(PrefixCommand::Place { module, axis, cwd }),
                    None => Outcome::Swallow,
                }
            }
            State::AwaitHide => {
                if press == Press::Modifier {
                    return Outcome::Swallow;
                }
                self.held = Some(keycode);
                self.state = State::Idle;
                match self.module_named(press) {
                    Some(module) => Outcome::Run(PrefixCommand::Hide { module }),
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
                    self.state = Self::run_state(press, &action, now);
                    return Outcome::Run(PrefixCommand::Run(action));
                }
                // Not repeatable: repeat mode ends. A root key runs; any other key goes to the pane as usual (tmux).
                self.state = State::Idle;
                match self.root_action(press) {
                    Some(action) => self.run_root(action, keycode),
                    None => Outcome::Pass,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keymap::{KeymapOp, SwapTarget, TabAction, TextChange};
    use crate::layout::Direction;

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

    /// Every default binding but the two split keys and the hide key runs its own action, from the table itself.
    #[test]
    fn every_default_binding_after_the_prefix_runs_its_action_and_disarms() {
        let t = Instant::now();
        for binding in Keymap::defaults().bindings() {
            if matches!(binding.action, Action::Split { .. } | Action::ModuleHideByKey) {
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

    /// `d` waits for a module key and hides the module it names: a capital reads as its lowercase, a Lua panel's
    /// key names its panel, a modifier keeps the wait, and `Esc` or any other key ends it with nothing run.
    #[test]
    fn the_hide_key_waits_for_a_module_key_and_names_its_module() {
        let t = Instant::now();
        let hide = |module: ModuleId| Outcome::Run(PrefixCommand::Hide { module });
        let mut p = with(Keymap::defaults(), &[("notes", "g")]);
        for (text, module) in [
            ("t", ModuleId::terminal()),
            ("E", ModuleId::editor()),
            ("g", ModuleId::lua("notes")),
            ("a", ModuleId::agent()),
        ] {
            p = armed(p, t);
            assert_eq!(p.press(key("d"), K1, t), Outcome::Swallow);
            assert_eq!(p.waiting(), Waiting::Hide);
            p.release(K1);
            assert_eq!(
                p.press(Press::Modifier, SHIFT_L, t),
                Outcome::Swallow,
                "a modifier keeps the wait"
            );
            assert_eq!(p.waiting(), Waiting::Hide);
            assert_eq!(p.press(key(text), K2, t), hide(module), "{text}");
            assert_eq!(p.waiting(), Waiting::No, "{text}");
            p.release(K2);
        }
        for cancel in [Press::Escape, key("z"), key("d"), key("C-b"), Press::Other] {
            p = armed(p, t);
            assert_eq!(p.press(key("d"), K1, t), Outcome::Swallow);
            p.release(K1);
            assert_eq!(p.press(cancel, K2, t), Outcome::Swallow, "{cancel:?}");
            assert_eq!(p.waiting(), Waiting::No, "{cancel:?}");
            p.release(K2);
        }
    }

    #[test]
    fn an_unbound_key_or_esc_after_the_prefix_is_swallowed_and_ends_it() {
        let t = Instant::now();
        for press in [key("g"), key("y"), key("S-Up"), Press::Other, Press::Escape] {
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

    fn text(change: TextChange) -> Outcome {
        run(Action::Text(change))
    }

    /// `prefix C-=`, released with Ctrl held, is a run on the pane for as long as Ctrl stays down. The same press
    /// after the run ends goes on to the accelerator, which sizes every module.
    #[test]
    fn a_text_size_run_lasts_while_ctrl_is_down_and_past_its_time() {
        let t0 = Instant::now();
        let mut p = armed(defaults(), t0);
        assert_eq!(p.press(key("C-="), K1, t0), text(TextChange::Larger));
        p.release(K1);
        assert!(p.watches_ctrl());
        let t1 = t0 + Duration::from_secs(30);
        assert_eq!(
            p.press(key("C--"), K2, t1),
            text(TextChange::Smaller),
            "half a minute on"
        );
        p.release(K2);
        let t2 = t1 + Duration::from_secs(30);
        assert_eq!(p.press(key("C-="), K1, t2), text(TextChange::Larger));
        p.release(K1);
        p.ctrl_released();
        assert!(!p.watches_ctrl());
        let t3 = t2 + Duration::from_millis(600);
        assert_eq!(
            p.press(key("C-="), K1, t3),
            Outcome::Pass,
            "Ctrl went up and the time ran out"
        );
    }

    /// Letting go does not shorten what the timer gives: a press 400 ms after the last step stays on the pane, as it
    /// does with no Ctrl rule at all, and goes on to last while Ctrl is down again.
    #[test]
    fn a_text_size_run_ends_at_the_later_of_the_release_and_the_time() {
        let t0 = Instant::now();
        let mut p = armed(defaults(), t0);
        p.press(key("C-="), K1, t0);
        p.release(K1);
        p.ctrl_released();
        let t1 = t0 + Duration::from_millis(400);
        assert_eq!(p.press(key("C-="), K1, t1), text(TextChange::Larger));
        p.release(K1);
        assert!(p.watches_ctrl(), "Ctrl is down again");
        let t2 = t1 + Duration::from_secs(5);
        assert_eq!(p.press(key("C-="), K1, t2), text(TextChange::Larger));
    }

    #[test]
    fn a_reset_joins_the_run_at_any_point_of_it() {
        let t0 = Instant::now();
        let mut p = armed(defaults(), t0);
        assert_eq!(p.press(key("C-="), K1, t0), text(TextChange::Larger));
        p.release(K1);
        let t1 = t0 + Duration::from_secs(2);
        assert_eq!(p.press(key("C-0"), K2, t1), text(TextChange::Reset), "in the run");
        p.release(K2);
        let t2 = t1 + Duration::from_secs(2);
        assert_eq!(p.press(key("C-="), K1, t2), text(TextChange::Larger), "and it goes on");

        let mut p = armed(defaults(), t0);
        assert_eq!(p.press(key("C-0"), K2, t0), text(TextChange::Reset), "it can start one");
        p.release(K2);
        assert_eq!(p.press(key("C-="), K1, t1), text(TextChange::Larger));
    }

    /// With a keyboard repeat delay past the repeat time the first auto-repeat came after the run had ended.
    #[test]
    fn a_held_text_key_keeps_stepping_however_late_its_repeat_starts() {
        let t0 = Instant::now();
        let mut p = armed(defaults(), t0);
        assert_eq!(p.press(key("C-="), K1, t0), text(TextChange::Larger));
        let t1 = t0 + Duration::from_millis(660);
        assert_eq!(p.press(key("C-="), K1, t1), text(TextChange::Larger), "the auto-repeat");
    }

    #[test]
    fn a_non_repeatable_key_ends_a_text_size_run_with_ctrl_still_down() {
        let t0 = Instant::now();
        let mut p = armed(defaults(), t0);
        p.press(key("C-="), K1, t0);
        p.release(K1);
        assert_eq!(p.press(key("C-h"), K2, t0 + Duration::from_secs(1)), Outcome::Pass);
        p.release(K2);
        assert!(!p.watches_ctrl());
        assert_eq!(p.press(key("C-="), K1, t0 + Duration::from_secs(2)), Outcome::Pass);
    }

    /// Only `text.*` on a Ctrl chord: the arrows keep tmux's timer, and so does a text key with no Ctrl on it.
    #[test]
    fn every_other_repeat_keeps_the_timer() {
        let t0 = Instant::now();
        let mut p = armed(defaults(), t0);
        p.press(key("C-Up"), K1, t0);
        p.release(K1);
        assert!(!p.watches_ctrl(), "a resize holds Ctrl and is no text size step");

        let bare = KeymapOp::Set {
            table: "prefix".into(),
            key: "=".into(),
            action: "text.larger".into(),
            opts: vec![("repeatable".into(), crate::keymap::OptValue::Bool(true))],
        };
        let mut p = armed(with(Keymap::apply_user(&[bare], &[]).unwrap(), &[]), t0);
        assert_eq!(p.press(key("="), K1, t0), text(TextChange::Larger));
        p.release(K1);
        assert!(!p.watches_ctrl());
        assert_eq!(p.press(key("="), K1, t0 + Duration::from_millis(600)), Outcome::Pass);
    }

    #[test]
    fn a_step_of_another_key_after_a_ctrl_run_is_that_keys_rule() {
        let t0 = Instant::now();
        let mut p = armed(defaults(), t0);
        p.press(key("C-="), K1, t0);
        p.release(K1);
        let t1 = t0 + Duration::from_secs(5);
        assert_eq!(
            p.press(key("C-Up"), K2, t1),
            run(Action::Resize {
                dir: Direction::Up,
                cells: 1
            }),
            "still in the run, and a repeatable key"
        );
        p.release(K2);
        assert!(!p.watches_ctrl());
        assert_eq!(p.press(key("C-="), K1, t1 + Duration::from_millis(600)), Outcome::Pass);
    }

    #[test]
    fn losing_the_window_ends_a_run_whose_ctrl_release_never_comes() {
        let t0 = Instant::now();
        let mut p = armed(defaults(), t0);
        p.press(key("C-="), K1, t0);
        p.release(K1);
        p.cancel();
        assert!(!p.watches_ctrl());
        assert_eq!(p.press(key("C-="), K1, t0 + Duration::from_secs(1)), Outcome::Pass);
    }

    fn shift_typed(s: &str) -> Press {
        Press::ShiftTyped(KeySpec::parse(s).unwrap())
    }

    /// Swiss and JIS keyboards type `=` with Shift, and French `0`: after the prefix those presses are the chords
    /// they name, and they keep a run going. With nothing armed the prefix leaves them to the accelerators.
    #[test]
    fn a_key_shift_typed_reads_as_its_chord_after_the_prefix_and_nowhere_else() {
        let t0 = Instant::now();
        let mut p = defaults();
        assert_eq!(p.press(shift_typed("C-="), K1, t0), Outcome::Pass);
        p.release(K1);
        p = armed(p, t0);
        assert_eq!(p.press(shift_typed("C-="), K1, t0), text(TextChange::Larger));
        p.release(K1);
        assert!(p.watches_ctrl());
        let t1 = t0 + Duration::from_secs(2);
        assert_eq!(p.press(shift_typed("C-0"), K2, t1), text(TextChange::Reset));
        p.release(K2);
        assert_eq!(p.press(shift_typed("C-+"), K1, t1), text(TextChange::Larger));
        p.release(K1);
        let mut p = armed(defaults(), t0);
        assert_eq!(
            p.press(Press::Modifier, SHIFT_L, t0),
            Outcome::Swallow,
            "Shift going down first keeps the wait"
        );
        assert_eq!(p.press(shift_typed("C-="), K1, t0), text(TextChange::Larger));
    }

    fn all(change: TextChange) -> Outcome {
        run(Action::TextAll(change))
    }

    #[test]
    fn ctrl_plus_with_no_shift_sizes_every_module_with_nothing_armed() {
        let t = Instant::now();
        let mut p = defaults();
        assert_eq!(p.press(key("C-+"), K1, t), all(TextChange::Larger));
        assert_eq!(
            p.press(key("C-+"), K1, t),
            all(TextChange::Larger),
            "held, it steps as the accelerators do"
        );
        p.release(K1);
        assert_eq!(
            p.press(shift_typed("C-+"), K1, t),
            Outcome::Pass,
            "a US Ctrl+Shift+= stays the pane's: a <C-+> mapping keeps working"
        );
        p.release(K1);
        let mut p = armed(defaults(), t);
        assert_eq!(
            p.press(key("C-+"), K1, t),
            text(TextChange::Larger),
            "after the prefix, the pane"
        );
    }

    /// `+` with Shift is a US keyboard's `Ctrl+Shift+=`, so a user root key on `C-+` is the press with no Shift alone.
    #[test]
    fn a_root_key_on_plus_is_not_a_us_ctrl_shift_equal() {
        let t = Instant::now();
        let mut p = with_root(&[root_op("C-+", "text.larger")]);
        assert_eq!(p.press(shift_typed("C-+"), K1, t), Outcome::Pass);
        p.release(K1);
        assert_eq!(p.press(key("C-+"), K1, t), text(TextChange::Larger));
    }

    #[test]
    fn a_host_without_the_built_in_chords_leaves_ctrl_plus_to_the_pane() {
        let t = Instant::now();
        let mut p = defaults();
        p.set_key_stage_roots(false);
        assert_eq!(p.press(key("C-+"), K1, t), Outcome::Pass);
    }

    #[test]
    fn ctrl_plus_keeps_its_built_in_step_where_the_users_action_is_refused() {
        let t = Instant::now();
        let mut p = with_root(&[root_op("C-+", "zoom")]);
        p.set_root_keep(Rc::new(|action| !matches!(action, Action::Zoom)));
        assert_eq!(p.press(key("C-+"), K1, t), all(TextChange::Larger));
    }

    #[test]
    fn a_host_that_refuses_the_action_leaves_ctrl_plus_to_the_pane() {
        let t = Instant::now();
        let mut p = defaults();
        p.set_root_keep(Rc::new(|action| !action.is_text_size()));
        assert_eq!(p.press(key("C-+"), K1, t), Outcome::Pass);
    }

    fn swapped() -> Prefix {
        let mut ops = Vec::new();
        for (chord, change) in [("C-=", "larger"), ("C--", "smaller"), ("C-0", "reset")] {
            ops.push(root_op(chord, &format!("text.{change}")));
            ops.push(KeymapOp::Del {
                table: "prefix".into(),
                key: chord.into(),
            });
            ops.push(KeymapOp::Set {
                table: "prefix".into(),
                key: chord.into(),
                action: format!("text.all.{change}"),
                opts: vec![("repeatable".into(), crate::keymap::OptValue::Bool(true))],
            });
        }
        with_root(&ops)
    }

    /// `init.lua` swaps the two scopes: the plain chord is the pane, the prefix before it every module, whose run
    /// lasts on Ctrl as well. A held text size root key steps, as a built-in chord does.
    #[test]
    fn the_two_scopes_can_change_places() {
        let t0 = Instant::now();
        let mut p = swapped();
        assert_eq!(p.press(key("C-="), K1, t0), text(TextChange::Larger));
        assert_eq!(p.press(key("C-="), K1, t0), text(TextChange::Larger), "held");
        p.release(K1);
        assert_eq!(
            p.press(shift_typed("C-="), K1, t0),
            text(TextChange::Larger),
            "the same chord where Shift types the key"
        );
        p.release(K1);
        p = armed(p, t0);
        assert_eq!(p.press(key("C-="), K1, t0), all(TextChange::Larger));
        p.release(K1);
        assert!(p.watches_ctrl());
        assert_eq!(
            p.press(key("C-0"), K2, t0 + Duration::from_secs(3)),
            all(TextChange::Reset)
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
        assert_eq!(p.press(key("g"), K1, t), Outcome::Swallow);
        assert_eq!(
            p.press(key("g"), K1, t),
            Outcome::Swallow,
            "an unbound key's repeat reaches nobody"
        );
    }

    /// A held prefix chord's auto-repeat is swallowed rather than sent on, and `send-prefix` sends the chord that is
    /// configured, never a hard-coded `C-b`.
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
                        cwd: SplitCwd::Start,
                    }),
                ),
                (
                    key("t"),
                    Outcome::Run(PrefixCommand::Place {
                        module: ModuleId::terminal(),
                        axis,
                        cwd: SplitCwd::Start,
                    }),
                ),
                (
                    key("A"),
                    Outcome::Run(PrefixCommand::Place {
                        module: ModuleId::agent(),
                        axis,
                        cwd: SplitCwd::Start,
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
                axis: Axis::Column,
                cwd: SplitCwd::Start,
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
                    ("cells".into(), crate::keymap::OptValue::Int(5)),
                    ("repeatable".into(), crate::keymap::OptValue::Bool(true)),
                ],
            },
            KeymapOp::Del {
                table: "prefix".into(),
                key: "H".into(),
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

    #[test]
    fn the_workspace_chooser_waits_for_a_digit_c_or_x_then_a_digit() {
        let t = Instant::now();
        let mut p = armed(defaults(), t);
        assert_eq!(p.press(key("s"), K1, t), run(Action::WorkspaceChoose));
        assert_eq!(p.waiting(), Waiting::No, "the host starts the wait, not the key");
        p.await_workspace();
        assert_eq!(p.waiting(), Waiting::Workspace { deleting: false });
        assert!(p.is_armed());
        assert_eq!(p.press(key("s"), K1, t), Outcome::Swallow, "the held s's own repeat");
        assert_eq!(p.press(Press::Modifier, SHIFT_L, t), Outcome::Swallow);
        assert_eq!(p.waiting(), Waiting::Workspace { deleting: false });
        p.release(K1);
        assert_eq!(
            p.press(key("2"), K2, t + Duration::from_secs(30)),
            Outcome::Run(PrefixCommand::Workspace(WorkspacePick::Switch(1))),
            "the chooser has no time limit"
        );
        assert_eq!(p.waiting(), Waiting::No);

        let mut p = defaults();
        p.await_workspace();
        assert_eq!(
            p.press(key("0"), K2, t),
            Outcome::Run(PrefixCommand::Workspace(WorkspacePick::Switch(9)))
        );
        let mut p = defaults();
        p.await_workspace();
        assert_eq!(
            p.press(key("c"), K2, t),
            Outcome::Run(PrefixCommand::Workspace(WorkspacePick::New))
        );
        let mut p = defaults();
        p.await_workspace();
        assert_eq!(p.press(key("x"), K2, t), Outcome::Swallow);
        assert_eq!(p.waiting(), Waiting::Workspace { deleting: true });
        p.release(K2);
        assert_eq!(
            p.press(key("3"), K1, t),
            Outcome::Run(PrefixCommand::Workspace(WorkspacePick::Delete(2)))
        );
        assert_eq!(p.waiting(), Waiting::No);

        for press in [Press::Escape, key("q"), key("M-1"), key("C-b"), Press::Other] {
            let mut p = defaults();
            p.await_workspace();
            assert_eq!(p.press(press, K2, t), Outcome::Swallow, "{press:?}");
            assert_eq!(p.waiting(), Waiting::No, "{press:?}");
        }
        let mut p = defaults();
        p.await_workspace();
        assert_eq!(p.press(key("x"), K2, t), Outcome::Swallow);
        p.release(K2);
        assert_eq!(p.press(key("c"), K1, t), Outcome::Swallow, "c after x makes nothing");
        assert_eq!(p.waiting(), Waiting::No);
    }

    #[test]
    fn display_panes_waits_for_a_digit_until_its_time_is_up() {
        let t = Instant::now();
        for (digit, want) in [("3", '3'), ("0", '0')] {
            let mut p = armed(defaults(), t);
            assert_eq!(p.press(key("q"), K1, t), run(Action::SelectNumber));
            assert_eq!(p.waiting(), Waiting::No, "the host starts the wait, not the key");
            p.await_number(t);
            assert_eq!(p.waiting(), Waiting::Number);
            assert!(p.is_armed());
            assert_eq!(p.press(key("q"), K1, t), Outcome::Swallow, "the held q's own repeat");
            assert_eq!(p.waiting(), Waiting::Number);
            assert_eq!(p.press(Press::Modifier, SHIFT_L, t), Outcome::Swallow);
            assert_eq!(p.waiting(), Waiting::Number, "a bare modifier is not the next key");
            p.release(K1);
            assert_eq!(
                p.press(key(digit), K2, t + Duration::from_millis(900)),
                Outcome::Run(PrefixCommand::Number(want))
            );
            assert_eq!(p.waiting(), Waiting::No);
        }
        for press in [Press::Escape, key("x"), key("M-1"), Press::Other, key("C-b")] {
            let mut p = defaults();
            p.await_number(t);
            assert_eq!(p.press(press, K2, t), Outcome::Swallow, "{press:?}");
            assert_eq!(p.waiting(), Waiting::No, "{press:?}");
        }
        let mut p = defaults();
        p.await_number(t);
        assert_eq!(
            p.press(key("3"), K2, t + Duration::from_millis(1001)),
            Outcome::Pass,
            "past its time the key goes where it was going"
        );
        assert_eq!(p.waiting(), Waiting::No);
        let mut p = defaults();
        p.await_number(t);
        assert!(!p.expire(t + Duration::from_millis(999)));
        assert_eq!(p.waiting(), Waiting::Number);
        assert!(p.expire(t + Duration::from_millis(1001)));
        assert_eq!(p.waiting(), Waiting::No);
        assert!(!p.expire(t + Duration::from_millis(2000)), "nothing left to end");
    }

    #[test]
    fn a_split_binding_carries_its_cwd_to_the_place() {
        for cwd in [
            crate::keymap::action::SplitCwd::Start,
            crate::keymap::action::SplitCwd::Pane,
        ] {
            let map = if cwd == crate::keymap::action::SplitCwd::Start {
                Keymap::defaults()
            } else {
                Keymap::apply_user(
                    &[
                        KeymapOp::Del {
                            table: "prefix".into(),
                            key: "%".into(),
                        },
                        KeymapOp::Set {
                            table: "prefix".into(),
                            key: "%".into(),
                            action: "split.right".into(),
                            opts: vec![("cwd".into(), crate::keymap::OptValue::Str("pane".into()))],
                        },
                    ],
                    &[],
                )
                .unwrap()
            };
            let t = Instant::now();
            let mut p = armed(with(map, &[]), t);
            assert_eq!(p.press(key("%"), K1, t), Outcome::Swallow);
            assert_eq!(
                p.press(key("t"), K2, t),
                Outcome::Run(PrefixCommand::Place {
                    module: ModuleId::terminal(),
                    axis: Axis::Row,
                    cwd,
                })
            );
        }
    }

    fn root_op(key: &str, action: &str) -> KeymapOp {
        KeymapOp::Set {
            table: "root".into(),
            key: key.into(),
            action: action.into(),
            opts: vec![],
        }
    }
    fn with_root(ops: &[KeymapOp]) -> Prefix {
        with(Keymap::apply_user(ops, &[]).unwrap(), &[])
    }

    #[test]
    fn regression_root_hide_by_key_waits_under_the_same_policy_as_prefix() {
        for waits in [ModuleWaits::SplitAndHide, ModuleWaits::Hide, ModuleWaits::Neither] {
            let mut p = with_root(&[root_op("M-F9", "module.hide-by-key")]);
            p.set_module_waits(waits);
            let t = Instant::now();
            let result = p.press(key("M-F9"), K1, t);
            if waits == ModuleWaits::Neither {
                assert_eq!(result, run(Action::ModuleHideByKey));
                assert_eq!(p.waiting(), Waiting::No);
            } else {
                assert_eq!(result, Outcome::Swallow);
                assert_eq!(p.waiting(), Waiting::Hide);
                p.release(K1);
                assert_eq!(
                    p.press(key("t"), K2, t),
                    Outcome::Run(PrefixCommand::Hide {
                        module: ModuleId::terminal()
                    })
                );
            }
        }
    }

    #[test]
    fn a_root_key_runs_with_nothing_armed_and_the_prefix_still_arms() {
        let t = Instant::now();
        let mut p = with_root(&[root_op("M-y", "tab.next")]);
        assert_eq!(p.press(key("M-y"), K1, t), run(Action::Tab(TabAction::Next)));
        assert!(!p.is_armed());
        p.release(K1);
        assert_eq!(p.press(key("C-b"), PREFIX, t), Outcome::Swallow);
        assert_eq!(p.waiting(), Waiting::Command);
        p.release(PREFIX);
        assert_eq!(
            p.press(key("g"), K2, t),
            Outcome::Swallow,
            "an unbound key after the prefix"
        );
        p.release(K2);
        assert_eq!(
            p.press(key("M-u"), K2, t),
            Outcome::Pass,
            "an unbound key with nothing armed"
        );
        assert_eq!(p.press(key("y"), K2, t), Outcome::Pass);
    }

    /// A held `M-t` bound to `module.terminal` must open one terminal, however long it is held.
    #[test]
    fn a_held_root_keys_auto_repeat_is_swallowed_until_its_release() {
        let t = Instant::now();
        let mut p = with_root(&[root_op("M-t", "module.terminal")]);
        let action = run(Action::Module(ModuleId::terminal()));
        assert_eq!(p.press(key("M-t"), K1, t), action);
        for _ in 0..5 {
            assert_eq!(p.press(key("M-t"), K1, t), Outcome::Swallow);
        }
        assert!(p.holds(K1));
        p.release(K1);
        assert_eq!(p.press(key("M-t"), K1, t), action, "the next press runs again");
    }

    #[test]
    fn a_root_split_key_waits_for_the_module_key_like_a_split_after_the_prefix() {
        let t = Instant::now();
        let mut p = with_root(&[root_op("M-y", "split.right")]);
        assert_eq!(p.press(key("M-y"), K1, t), Outcome::Swallow);
        assert_eq!(p.waiting(), Waiting::Module(Axis::Row));
        p.release(K1);
        assert_eq!(
            p.press(key("t"), K2, t),
            Outcome::Run(PrefixCommand::Place {
                module: ModuleId::terminal(),
                axis: Axis::Row,
                cwd: crate::keymap::action::SplitCwd::Start,
            })
        );
        assert!(!p.is_armed());
    }

    #[test]
    fn a_root_key_during_repeat_mode_ends_it_and_runs() {
        let t0 = Instant::now();
        let mut p = armed(with_root(&[root_op("M-y", "tab.next")]), t0);
        assert_eq!(
            p.press(key("C-Up"), K1, t0),
            run(Action::Resize {
                dir: Direction::Up,
                cells: 1
            })
        );
        p.release(K1);
        let t1 = t0 + Duration::from_millis(100);
        assert_eq!(p.press(key("M-y"), K2, t1), run(Action::Tab(TabAction::Next)));
        p.release(K2);
        assert_eq!(
            p.press(key("C-Up"), K1, t1 + Duration::from_millis(50)),
            Outcome::Pass,
            "repeat mode ended with the root key"
        );
    }

    /// A root key that moved the keys to another window never gets its release in this one.
    #[test]
    fn a_root_key_held_when_focus_is_lost_does_not_swallow_its_next_press() {
        let t = Instant::now();
        let mut p = with_root(&[root_op("M-y", "tab.next")]);
        let action = run(Action::Tab(TabAction::Next));
        assert_eq!(p.press(key("M-y"), K1, t), action);
        p.cancel();
        assert_eq!(p.press(key("M-y"), K1, t), action);
    }

    #[test]
    fn a_root_key_the_host_refuses_passes_on_untouched() {
        let t = Instant::now();
        let mut p = with_root(&[root_op("M-y", "zoom"), root_op("M-u", "tab.next")]);
        p.set_root_keep(Rc::new(|action| !matches!(action, Action::Zoom)));
        assert_eq!(p.press(key("M-y"), K1, t), Outcome::Pass);
        assert!(!p.holds(K1), "a passed key is not remembered");
        assert_eq!(p.press(key("M-u"), K2, t), run(Action::Tab(TabAction::Next)));
        // Armed, a refused root key is no root key either.
        let mut p = armed(with_root(&[root_op("M-y", "zoom")]), t);
        p.set_root_keep(Rc::new(|_| false));
        assert_eq!(p.press(key("M-y"), K1, t), Outcome::Swallow);
    }

    #[test]
    fn a_root_key_after_the_prefix_is_sent_through_and_a_prefix_key_keeps_its_binding() {
        let t = Instant::now();
        let mut p = armed(with_root(&[root_op("M-y", "tab.next")]), t);
        assert_eq!(
            p.press(key("M-y"), K1, t),
            run(Action::SendKeys(KeySpec::parse("M-y").unwrap()))
        );
        assert!(!p.is_armed());
        let mut p = armed(with_root(&[root_op("M-y", "tab.next")]), t);
        assert_eq!(p.press(key("n"), K1, t), run(Action::Tab(TabAction::Next)));
        let mut p = armed(with_root(&[root_op("M-y", "tab.next")]), t);
        assert_eq!(p.press(key("g"), K1, t), Outcome::Swallow, "an unbound, non-root key");
    }
}
