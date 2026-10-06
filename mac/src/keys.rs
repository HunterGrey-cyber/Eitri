//! What the window does with a key-down before AppKit dispatches it: the prefix first, then the
//! pane's own rules (`Ctrl+h/j/k/l` between the editor and the panel, `Cmd` chords for the menu).
//!
//! The AppKit half only describes the event ([`KeyDown`]) and acts on the answer ([`KeyAction`]); every
//! decision is here, so the Linux workstation tests the whole table. The translation into the prefix's
//! [`Press`] follows the GDK one in `shell/src/prefix.rs` rule for rule, on AppKit's fields.

use std::rc::Rc;
use std::time::Instant;

use eitri_core::keymap::prefix::{Outcome, Prefix, PrefixCommand, Press, Waiting};
use eitri_core::keymap::{KeyName, KeySpec, Keymap};
use eitri_core::layout::{Direction, ModuleKeys};

/// One key-down as the AppKit monitor describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyDown {
    /// `charactersIgnoringModifiers`: Shift (and Caps Lock) applied, Control, Option and Command not.
    /// Empty for a key that produces no character (a dead key's first press, a function key's private-use
    /// character is never relied on: named keys are told apart by `keycode`).
    pub chars: String,
    /// `NSEvent.keyCode`: a `kVK_*` virtual key code, the hardware key whatever the layout names it.
    pub keycode: u16,
    pub ctrl: bool,
    pub cmd: bool,
    pub option: bool,
    pub shift: bool,
    /// `isARepeat`.
    pub repeat: bool,
}

/// Which pane has the keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pane {
    Editor,
    Panel,
}

/// What the AppKit half does with the event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyAction {
    /// AppKit dispatches it as usual.
    Pass,
    /// The prefix (or the guard against a repeat that follows its key into another pane) took it.
    Swallow,
    /// The prefix completed a command.
    RunPrefix(PrefixCommand),
    /// `Ctrl+h/j/k/l` from the page toward the editor.
    FocusEditor,
    /// A composition is open in the editor and `Ctrl+l` was pressed: end it, then hand this key to nvim
    /// through the link.
    EndCompositionThenNvim(&'static str),
    /// `Cmd` held while the page has the keys: the main menu sees it first.
    MenuFirst,
}

/// The window's two panes: the editor on the left, the panel on the right. The one place that says who
/// is next to whom, for the keys and for the assembly's focus moves.
pub fn neighbour(from: Pane, direction: Direction) -> Option<Pane> {
    match (from, direction) {
        (Pane::Panel, Direction::Left) => Some(Pane::Editor),
        (Pane::Editor, Direction::Right) => Some(Pane::Panel),
        _ => None,
    }
}

/// `kVK_*` codes of the bare modifier keys (right and left Command, Shift, Caps Lock, Option, Control,
/// Function).
const MODIFIER_KEYCODES: std::ops::RangeInclusive<u16> = 54..=63;
const KVK_ESCAPE: u16 = 53;

/// The tmux name of a key told apart by its hardware code, never by AppKit's private-use characters for
/// the function keys.
fn named(keycode: u16) -> Option<KeyName> {
    Some(match keycode {
        126 => KeyName::Up,
        125 => KeyName::Down,
        123 => KeyName::Left,
        124 => KeyName::Right,
        49 => KeyName::Space,
        36 | 76 => KeyName::Enter,
        48 => KeyName::Tab,
        51 => KeyName::BSpace,
        117 => KeyName::DC,
        114 => KeyName::IC,
        115 => KeyName::Home,
        119 => KeyName::End,
        116 => KeyName::PPage,
        121 => KeyName::NPage,
        _ => {
            // F1 to F20 in order.
            const FUNCTION: [u16; 20] = [
                122, 120, 99, 118, 96, 97, 98, 100, 101, 109, 103, 111, 105, 107, 113, 106, 64, 79, 80, 90,
            ];
            let index = FUNCTION.iter().position(|code| *code == keycode)?;
            KeyName::F(u8::try_from(index + 1).ok()?)
        }
    })
}

/// The prefix's reading of a key-down: the GDK `classify` rules, in the same order. Option plays the part
/// of GDK's Alt (`M-`), Command the part of Super and Meta (held, the press is `Other`).
pub fn press_for(key: &KeyDown) -> Press {
    if MODIFIER_KEYCODES.contains(&key.keycode) {
        return Press::Modifier;
    }
    if key.keycode == KVK_ESCAPE {
        return Press::Escape;
    }
    if key.cmd {
        return Press::Other;
    }
    if let Some(name) = named(key.keycode) {
        return Press::Key(KeySpec::named(name, key.ctrl, key.option, key.shift));
    }
    let mut chars = key.chars.chars();
    let (Some(ch), None) = (chars.next(), chars.next()) else {
        // No character (a dead key) or several: nothing tmux can name.
        return Press::Other;
    };
    // With Ctrl the letter is read lowercase, so Caps Lock does not turn `Ctrl+b` into `C-B`.
    let ch = if key.ctrl { ch.to_ascii_lowercase() } else { ch };
    if ch.is_control() || ch.is_whitespace() || (key.ctrl && key.shift) {
        return Press::Other;
    }
    Press::Key(KeySpec::char(ch, key.ctrl, key.option))
}

/// The direction `Ctrl+h/j/k/l` names; any other modifier alongside makes it a different key.
fn nav_direction(key: &KeyDown) -> Option<Direction> {
    if !key.ctrl || key.shift || key.option || key.cmd {
        return None;
    }
    match key.chars.to_lowercase().as_str() {
        "h" => Some(Direction::Left),
        "j" => Some(Direction::Down),
        "k" => Some(Direction::Up),
        "l" => Some(Direction::Right),
        _ => None,
    }
}

pub struct Keys {
    prefix: Prefix,
    nav_intercept: Rc<dyn Fn(Direction) -> bool>,
    /// The last key routed to a pane and still down: a repeat of it never follows it into the other pane.
    down: Option<(u16, Pane)>,
}

impl Keys {
    pub fn new(keymap: Rc<Keymap>, module_keys: Rc<ModuleKeys>, nav_intercept: Rc<dyn Fn(Direction) -> bool>) -> Keys {
        Keys {
            prefix: Prefix::new(keymap, module_keys),
            nav_intercept,
            down: None,
        }
    }

    /// What to do with `key`, which the window routes to `pane`. `editor_marked_text` is whether the
    /// editor's text input has a composition open.
    ///
    /// The prefix decides first, for either pane (the GTK window's capture controller does the same).
    /// Every action but `Pass` means the event must not reach AppKit's own dispatch.
    pub fn key_down(&mut self, key: &KeyDown, pane: Pane, editor_marked_text: bool, now: Instant) -> KeyAction {
        let code = key.keycode;
        // GDK has no repeat flag, so the prefix recognises a repeat by the keycode it is still holding.
        // AppKit has one, and sometimes drops a key-up (while Command is held): without this a key whose
        // key-up was lost would be taken for its own repeat and swallowed for good.
        if !key.repeat {
            self.prefix.release(u32::from(code));
        }
        let was = self.prefix.waiting();
        let outcome = self.prefix.press(press_for(key), u32::from(code), now);
        let waiting = self.prefix.waiting();
        if was != waiting {
            println!("[prefix] armed={} waiting={waiting:?}", waiting != Waiting::No);
        }
        match outcome {
            Outcome::Swallow => return KeyAction::Swallow,
            Outcome::Run(command) => {
                println!("[prefix] {command:?}");
                return KeyAction::RunPrefix(command);
            }
            Outcome::Pass => {}
        }
        // Holding Ctrl+h in the page moves the keys to the editor, and its repeats would then reach nvim
        // (in Insert mode `<C-h>` deletes); Ctrl+l at nvim's edge does the same the other way.
        if key.repeat && self.down.is_some_and(|(held, first)| held == code && first != pane) {
            return KeyAction::Swallow;
        }
        let direction = nav_direction(key);
        let action = match pane {
            Pane::Panel if key.cmd => KeyAction::MenuFirst,
            Pane::Panel => match direction {
                Some(direction) if (self.nav_intercept)(direction) => KeyAction::Swallow,
                Some(direction) if neighbour(Pane::Panel, direction) == Some(Pane::Editor) => KeyAction::FocusEditor,
                _ => KeyAction::Pass,
            },
            Pane::Editor if editor_marked_text && direction == Some(Direction::Right) => {
                KeyAction::EndCompositionThenNvim("<C-l>")
            }
            Pane::Editor => KeyAction::Pass,
        };
        if !key.repeat {
            self.down = Some((code, pane));
        }
        action
    }

    pub fn key_up(&mut self, keycode: u16) {
        self.prefix.release(u32::from(keycode));
        if self.down.is_some_and(|(held, _)| held == keycode) {
            self.down = None;
        }
    }

    /// The window was deactivated or a click arrived: whatever the prefix waits for ends.
    ///
    /// The key still down keeps its pane. A click into the other pane while a key is held would otherwise
    /// let that key's repeats reach the pane clicked (a held Backspace begun in the composer deleting in
    /// the editor). A key-up lost meanwhile costs nothing: only a repeat is checked against it, and the
    /// next press of any key replaces it.
    pub fn cancel(&mut self) {
        let was = self.prefix.waiting();
        self.prefix.cancel();
        if was != Waiting::No {
            println!("[prefix] armed=false waiting={:?}", Waiting::No);
        }
    }

    pub fn waiting(&self) -> Waiting {
        self.prefix.waiting()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eitri_core::keymap::{Action, TabAction};
    use std::cell::RefCell;

    // Real `kVK_ANSI_*` codes.
    const B: u16 = 11;
    const C: u16 = 8;
    const Z: u16 = 6;
    const H: u16 = 4;
    const J: u16 = 38;
    const K: u16 = 40;
    const L: u16 = 37;

    fn plain(chars: &str, keycode: u16) -> KeyDown {
        KeyDown {
            chars: chars.to_string(),
            keycode,
            ctrl: false,
            cmd: false,
            option: false,
            shift: false,
            repeat: false,
        }
    }
    fn ctrl(chars: &str, keycode: u16) -> KeyDown {
        KeyDown {
            ctrl: true,
            ..plain(chars, keycode)
        }
    }
    fn cmd(chars: &str, keycode: u16) -> KeyDown {
        KeyDown {
            cmd: true,
            ..plain(chars, keycode)
        }
    }
    fn repeating(key: KeyDown) -> KeyDown {
        KeyDown { repeat: true, ..key }
    }

    /// Records what the page's intercept was asked, and claims the directions it was told to.
    struct Fixture {
        keys: Keys,
        asked: Rc<RefCell<Vec<Direction>>>,
    }
    fn fixture(claims: &'static [Direction]) -> Fixture {
        let keymap = Rc::new(Keymap::defaults());
        let module_keys = Rc::new(ModuleKeys::build(&[], &keymap).unwrap());
        let asked = Rc::new(RefCell::new(Vec::new()));
        let record = asked.clone();
        let intercept: Rc<dyn Fn(Direction) -> bool> = Rc::new(move |direction| {
            record.borrow_mut().push(direction);
            claims.contains(&direction)
        });
        Fixture {
            keys: Keys::new(keymap, module_keys, intercept),
            asked,
        }
    }
    fn down(f: &mut Fixture, key: &KeyDown, pane: Pane, marked: bool) -> KeyAction {
        f.keys.key_down(key, pane, marked, Instant::now())
    }
    fn arm(f: &mut Fixture, pane: Pane) {
        assert_eq!(down(f, &ctrl("b", B), pane, false), KeyAction::Swallow);
        f.keys.key_up(B);
    }

    #[test]
    fn editor_ctrl_l_passes_to_nvims_nav_fallback() {
        let mut f = fixture(&[]);
        assert_eq!(down(&mut f, &ctrl("l", L), Pane::Editor, false), KeyAction::Pass);
    }

    #[test]
    fn editor_ctrl_l_during_a_composition_ends_it_and_goes_through_the_link() {
        let mut f = fixture(&[]);
        assert_eq!(
            down(&mut f, &ctrl("l", L), Pane::Editor, true),
            KeyAction::EndCompositionThenNvim("<C-l>")
        );
        // Only Ctrl+l: the other motions keep their meaning in an input method.
        for (chars, code) in [("h", H), ("j", J), ("k", K)] {
            assert_eq!(down(&mut f, &ctrl(chars, code), Pane::Editor, true), KeyAction::Pass);
        }
    }

    #[test]
    fn panel_ctrl_h_gives_the_editor_the_keys() {
        let mut f = fixture(&[]);
        assert_eq!(down(&mut f, &ctrl("h", H), Pane::Panel, false), KeyAction::FocusEditor);
        assert_eq!(*f.asked.borrow(), vec![Direction::Left]);
    }

    #[test]
    fn panel_ctrl_j_claimed_in_browse_is_swallowed() {
        let mut f = fixture(&[Direction::Down]);
        assert_eq!(down(&mut f, &ctrl("j", J), Pane::Panel, false), KeyAction::Swallow);
        assert_eq!(*f.asked.borrow(), vec![Direction::Down]);
    }

    #[test]
    fn panel_ctrl_j_unclaimed_in_input_passes() {
        let mut f = fixture(&[]);
        assert_eq!(down(&mut f, &ctrl("j", J), Pane::Panel, false), KeyAction::Pass);
        assert_eq!(*f.asked.borrow(), vec![Direction::Down]);
    }

    #[test]
    fn panel_ctrl_l_has_no_neighbour_and_passes() {
        let mut f = fixture(&[]);
        assert_eq!(down(&mut f, &ctrl("l", L), Pane::Panel, false), KeyAction::Pass);
    }

    #[test]
    fn panel_cmd_c_goes_to_the_menu_first() {
        let mut f = fixture(&[]);
        assert_eq!(down(&mut f, &cmd("c", C), Pane::Panel, false), KeyAction::MenuFirst);
    }

    #[test]
    fn editor_cmd_c_passes_to_neovide() {
        let mut f = fixture(&[]);
        assert_eq!(down(&mut f, &cmd("c", C), Pane::Editor, false), KeyAction::Pass);
    }

    #[test]
    fn prefix_then_c_is_a_new_tab_in_either_pane() {
        for pane in [Pane::Editor, Pane::Panel] {
            let mut f = fixture(&[]);
            arm(&mut f, pane);
            assert_eq!(f.keys.waiting(), Waiting::Command);
            assert_eq!(
                down(&mut f, &plain("c", C), pane, false),
                KeyAction::RunPrefix(PrefixCommand::Run(Action::Tab(TabAction::New)))
            );
            assert_eq!(f.keys.waiting(), Waiting::No);
        }
    }

    #[test]
    fn panel_ctrl_shift_h_passes() {
        let mut f = fixture(&[]);
        let key = KeyDown {
            shift: true,
            ..ctrl("H", H)
        };
        assert_eq!(down(&mut f, &key, Pane::Panel, false), KeyAction::Pass);
        assert!(f.asked.borrow().is_empty(), "not a navigation chord");
    }

    #[test]
    fn prefix_then_z_is_zoom() {
        let mut f = fixture(&[]);
        arm(&mut f, Pane::Panel);
        assert_eq!(
            down(&mut f, &plain("z", Z), Pane::Panel, false),
            KeyAction::RunPrefix(PrefixCommand::Run(Action::Zoom))
        );
    }

    #[test]
    fn an_auto_repeat_of_the_prefix_is_swallowed_and_stays_armed() {
        let mut f = fixture(&[]);
        assert_eq!(down(&mut f, &ctrl("b", B), Pane::Editor, false), KeyAction::Swallow);
        for _ in 0..3 {
            assert_eq!(
                down(&mut f, &repeating(ctrl("b", B)), Pane::Editor, false),
                KeyAction::Swallow
            );
            assert_eq!(f.keys.waiting(), Waiting::Command, "no second arm, no command");
        }
    }

    #[test]
    fn the_prefix_pressed_twice_is_send_prefix() {
        let mut f = fixture(&[]);
        arm(&mut f, Pane::Editor);
        assert_eq!(
            down(&mut f, &ctrl("b", B), Pane::Editor, false),
            KeyAction::RunPrefix(PrefixCommand::Run(Action::SendPrefix))
        );
    }

    #[test]
    fn a_fresh_press_after_a_lost_key_up_is_not_a_repeat() {
        let mut f = fixture(&[]);
        arm(&mut f, Pane::Editor);
        assert!(matches!(
            down(&mut f, &plain("c", C), Pane::Editor, false),
            KeyAction::RunPrefix(_)
        ));
        // The key-up of `c` never arrived; the next `c` is a new press (`isARepeat` false).
        assert_eq!(down(&mut f, &plain("c", C), Pane::Editor, false), KeyAction::Pass);
    }

    #[test]
    fn a_held_ctrl_h_does_not_repeat_into_the_editor() {
        let mut f = fixture(&[]);
        assert_eq!(down(&mut f, &ctrl("h", H), Pane::Panel, false), KeyAction::FocusEditor);
        assert_eq!(
            down(&mut f, &repeating(ctrl("h", H)), Pane::Editor, false),
            KeyAction::Swallow
        );
        f.keys.key_up(H);
        assert_eq!(down(&mut f, &ctrl("h", H), Pane::Editor, false), KeyAction::Pass);
    }

    #[test]
    fn a_held_ctrl_l_does_not_repeat_into_the_page() {
        let mut f = fixture(&[]);
        assert_eq!(down(&mut f, &ctrl("l", L), Pane::Editor, false), KeyAction::Pass);
        assert_eq!(
            down(&mut f, &repeating(ctrl("l", L)), Pane::Panel, false),
            KeyAction::Swallow
        );
        // A repeat in the pane the key started in is that pane's own.
        let mut g = fixture(&[]);
        assert_eq!(down(&mut g, &ctrl("l", L), Pane::Editor, false), KeyAction::Pass);
        assert_eq!(
            down(&mut g, &repeating(ctrl("l", L)), Pane::Editor, false),
            KeyAction::Pass
        );
    }

    #[test]
    fn armed_then_cmd_c_is_swallowed_not_sent_to_the_menu() {
        let mut f = fixture(&[]);
        arm(&mut f, Pane::Panel);
        assert_eq!(down(&mut f, &cmd("c", C), Pane::Panel, false), KeyAction::Swallow);
        assert_eq!(f.keys.waiting(), Waiting::No, "an unbound key ends the wait");
    }

    #[test]
    fn cancel_disarms() {
        let mut f = fixture(&[]);
        arm(&mut f, Pane::Panel);
        f.keys.cancel();
        assert_eq!(f.keys.waiting(), Waiting::No);
        assert_eq!(down(&mut f, &plain("c", C), Pane::Panel, false), KeyAction::Pass);
    }

    #[test]
    fn a_click_does_not_let_a_held_key_repeat_into_the_other_pane() {
        const BACKSPACE: u16 = 51;
        let mut f = fixture(&[]);
        assert_eq!(
            down(&mut f, &plain("\u{7f}", BACKSPACE), Pane::Panel, false),
            KeyAction::Pass
        );
        // A click on the editor while Backspace is still held.
        f.keys.cancel();
        assert_eq!(
            down(&mut f, &repeating(plain("\u{7f}", BACKSPACE)), Pane::Editor, false),
            KeyAction::Swallow
        );
        f.keys.key_up(BACKSPACE);
        assert_eq!(
            down(&mut f, &plain("\u{7f}", BACKSPACE), Pane::Editor, false),
            KeyAction::Pass
        );
        assert_eq!(
            down(&mut f, &repeating(plain("\u{7f}", BACKSPACE)), Pane::Editor, false),
            KeyAction::Pass
        );
    }

    #[test]
    fn a_fresh_press_after_a_cancel_belongs_to_its_pane() {
        // The key-up was lost while the window was inactive; the next press is new, and so are its repeats.
        let mut f = fixture(&[]);
        assert_eq!(down(&mut f, &plain("c", C), Pane::Panel, false), KeyAction::Pass);
        f.keys.cancel();
        assert_eq!(down(&mut f, &plain("c", C), Pane::Editor, false), KeyAction::Pass);
        assert_eq!(
            down(&mut f, &repeating(plain("c", C)), Pane::Editor, false),
            KeyAction::Pass
        );
    }

    fn key(spec: &str) -> Press {
        Press::Key(KeySpec::parse(spec).unwrap())
    }

    #[test]
    fn press_for_names_keys_by_their_hardware_code() {
        assert_eq!(press_for(&plain("", 126)), key("Up"));
        assert_eq!(press_for(&plain("", 96)), key("F5"));
        assert_eq!(press_for(&plain("", 90)), key("F20"));
        assert_eq!(
            press_for(&KeyDown {
                shift: true,
                ..plain("\u{19}", 48)
            }),
            key("S-Tab")
        );
        assert_eq!(press_for(&plain(" ", 49)), key("Space"));
        assert_eq!(press_for(&plain("\r", 76)), key("Enter"));
    }

    #[test]
    fn press_for_reads_characters_the_way_gdk_does() {
        // Option plays Alt: `Option+e` arrives as the base letter.
        assert_eq!(
            press_for(&KeyDown {
                option: true,
                ..plain("e", 14)
            }),
            key("M-e")
        );
        // Ctrl reads the letter lowercase, so Caps Lock does not make `C-B`.
        assert_eq!(press_for(&ctrl("B", B)), key("C-b"));
        assert_eq!(press_for(&plain("", 14)), Press::Other, "a dead key's empty string");
        assert_eq!(press_for(&plain("ab", 14)), Press::Other);
        assert_eq!(
            press_for(&KeyDown {
                shift: true,
                ..ctrl("H", H)
            }),
            Press::Other,
            "C-S-<char> has no tmux name"
        );
        assert_eq!(press_for(&plain("%", 23)), key("%"));
    }

    #[test]
    fn press_for_sorts_modifiers_escape_and_command() {
        assert_eq!(press_for(&plain("", 59)), Press::Modifier);
        assert_eq!(press_for(&plain("", 55)), Press::Modifier);
        assert_eq!(press_for(&plain("\u{1b}", 53)), Press::Escape);
        assert_eq!(
            press_for(&KeyDown {
                cmd: true,
                ..plain("\u{1b}", 53)
            }),
            Press::Escape,
            "Escape is checked before Command, as GDK checks it before Super"
        );
        assert_eq!(press_for(&cmd("c", C)), Press::Other);
    }

    #[test]
    fn only_the_editor_left_of_the_panel_has_a_neighbour() {
        for direction in [Direction::Left, Direction::Down, Direction::Up, Direction::Right] {
            assert_eq!(
                neighbour(Pane::Panel, direction),
                (direction == Direction::Left).then_some(Pane::Editor)
            );
            assert_eq!(
                neighbour(Pane::Editor, direction),
                (direction == Direction::Right).then_some(Pane::Panel)
            );
        }
    }
}
