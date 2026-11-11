//! Translates native keys into controller input and chords into registered accelerators, and installs the
//! capture controllers that hand those keys to the controller: the window's key layers and each module host's
//! stage.
//!
//! **The window's key layers** are declared in one place ([`window_layers`], installed by
//! [`install_window_layers`]): HINT's capture first while a HINT runs, then the y/n prompt's while it is open,
//! then the prefix's, which hands every other key to the controller's window stage. GTK runs a widget's
//! controllers most-recently-added first and stops at the first key controller that handles the event
//! (`gtk_widget_add_controller` prepends, `gtk_widget_run_controllers` walks that list; GTK 4.22.5), so the
//! precedence is the reverse of the order they are added in: the prefix first, then the prompt, and HINT adds
//! its own when a HINT starts, after both. With the order written down here, moving a block of `build_ui` can no
//! longer change which layer sees a key first.
//!
//! **The module stage** ([`module_key`]) is one capture controller per module host, which asks the controller
//! what a key on its way into that module does (`Ctrl+h/j/k/l` between modules, a chat page's own claim of one,
//! no Super or Hyper chord into a web page). Only the second delivery of a key is answered here: WebKit hands a
//! key the page did not handle back to GTK once more, starting at the window ([`LetThrough`]), and asking the
//! controller again would move the keys twice.

use std::cell::RefCell;
use std::rc::{Rc, Weak};

use eitri_core::keymap::{key::keysym_name, prefix::Press, Chord, KeyName, KeySpec};
use eitri_core::wm::{
    Controller, Driver, HostEvent, KeyPress, KeyStage, Mods, PointerEv, PointerTarget, Verdict, WinId,
};
use gtk4::gdk::{Key, ModifierType};
use gtk4::glib;
use gtk4::prelude::*;

/// One of the window's key layers, each a capture controller on the window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Layer {
    /// HINT's own keys while a HINT runs; the coordinator adds its controller when a HINT starts.
    Hint,
    /// The y/n prompt's answer while the prompt is open: the overlay's own capture.
    Prompt,
    /// The prefix: every key the layers above let through goes to the controller's window stage.
    Prefix,
}

/// The window's key layers in the order they see a key.
pub(crate) fn window_layers() -> [Layer; 3] {
    [Layer::Hint, Layer::Prompt, Layer::Prefix]
}

/// The layers installed at startup, in the order they are added: GTK asks the last one added first, so this is
/// [`window_layers`] reversed, less HINT, which adds its own later and so comes first.
fn startup_layers(add: &mut dyn FnMut(Layer)) -> Vec<Layer> {
    let order: Vec<Layer> = window_layers()
        .into_iter()
        .rev()
        .filter(|layer| *layer != Layer::Hint)
        .collect();
    for layer in &order {
        add(*layer);
    }
    order
}

/// The y/n prompt's own capture, added to the window where the declared order puts it.
pub(crate) trait PromptLayer {
    fn add_capture(&self, window: &gtk4::Window);
}

/// Installs the window's startup key layers in their declared order (see the module documentation) and returns
/// the order they were added in. The prefix's layer hands each key to `driver`'s window stage and maps its verdict
/// to GTK's propagation; reports a key's release only when the controller watches that key; and reports every
/// click, never claiming it, since a click ends any wait of the prefix's.
pub(crate) fn install_window_layers(
    window: &gtk4::Window,
    prompt: &dyn PromptLayer,
    driver: Weak<Driver<Controller>>,
) -> Vec<Layer> {
    startup_layers(&mut |layer| match layer {
        Layer::Prefix => add_prefix_layer(window, driver.clone()),
        Layer::Prompt => prompt.add_capture(window),
        Layer::Hint => {}
    })
}

fn add_prefix_layer(window: &gtk4::Window, driver: Weak<Driver<Controller>>) {
    let keys = gtk4::EventControllerKey::new();
    keys.set_propagation_phase(gtk4::PropagationPhase::Capture);
    let ctrl_keys = Rc::new(CtrlKeys::default());
    {
        let driver = driver.clone();
        let ctrl_keys = ctrl_keys.clone();
        keys.connect_key_pressed(move |_, key, keycode, state| {
            ctrl_keys.pressed(key, keycode, state);
            let Some(driver) = driver.upgrade() else {
                return glib::Propagation::Proceed;
            };
            match driver.key(WinId::MAIN, &KeyStage::Window, key_press(key, keycode, state)) {
                Verdict::Consumed => glib::Propagation::Stop,
                Verdict::PassThrough | Verdict::LetThrough => glib::Propagation::Proceed,
            }
        });
    }
    {
        let ctrl_keys = ctrl_keys.clone();
        window.connect_notify_local(Some("is-active"), move |window, _| {
            if !window.is_active() {
                ctrl_keys.forget();
            }
        });
    }
    {
        let driver = driver.clone();
        keys.connect_key_released(move |_, key, keycode, _| {
            let Some(driver) = driver.upgrade() else { return };
            if driver.query(move |c| c.watches_release(WinId::MAIN, keycode)) {
                driver.post(HostEvent::KeyReleased {
                    win: WinId::MAIN,
                    keycode,
                });
            }
            if ctrl_keys.released(key, keycode) && driver.query(|c| c.watches_ctrl(WinId::MAIN)) {
                driver.post(HostEvent::CtrlReleased { win: WinId::MAIN });
            }
        });
    }
    window.add_controller(keys);
    let clicks = gtk4::GestureClick::new();
    clicks.set_button(0);
    clicks.set_propagation_phase(gtk4::PropagationPhase::Capture);
    // Never claimed: the click still lands wherever it was going.
    clicks.connect_pressed(move |gesture, _, x, y| {
        if let Some(driver) = driver.upgrade() {
            driver.post(HostEvent::Pointer {
                win: WinId::MAIN,
                on: PointerTarget::Window,
                ev: PointerEv::Press {
                    x,
                    y,
                    button: gesture.current_button(),
                },
            });
        }
    });
    window.add_controller(clicks);
}

/// How many let-through key events [`LetThrough`] remembers. A page that handles a key never sends it back, so
/// its entry stays until this many later ones push it out.
const LET_THROUGH_REMEMBERED: usize = 16;

/// The key events a module host's stage let through to its page, so it knows each one when WebKit hands it back.
///
/// **WebKit delivers a key the page did not handle twice.** Its key handling is asynchronous: the first delivery
/// goes to the web process, and when the page reports the key unhandled, WebKitGTK puts the SAME `GdkEvent` back on
/// the display's queue so GTK's own bindings get a turn (`webkitWebViewBasePropagateKeyEvent`,
/// `gdk_display_put_event`, WebKitWebViewBase.cpp in 2.52.6). That second dispatch starts at the window again and
/// runs the capture-phase controller on the host again. A chord with nowhere to go is exactly that case: the
/// controller lets it through, the panel does not handle `Ctrl+l`, and the chord comes back.
///
/// Holds the events themselves (a reference each, for `gdk::Event`), so no address is reused by a later event
/// while it is remembered, and equality is identity. More than one because the round trip through the web process
/// is asynchronous, so a second chord can be let through before the first comes back.
#[derive(Debug)]
pub(crate) struct LetThrough<E> {
    outstanding: std::collections::VecDeque<E>,
}

impl<E: PartialEq> LetThrough<E> {
    pub(crate) fn new() -> Self {
        Self {
            outstanding: std::collections::VecDeque::new(),
        }
    }

    /// Remembers `event` as let through to the page.
    pub(crate) fn let_through(&mut self, event: E) {
        if self.outstanding.len() == LET_THROUGH_REMEMBERED {
            self.outstanding.pop_front();
        }
        self.outstanding.push_back(event);
    }

    /// `true` if `event` was let through and this is WebKit handing it back. Forgets it: WebKit hands an event
    /// back at most once.
    pub(crate) fn is_second_delivery(&mut self, event: &E) -> bool {
        match self.outstanding.iter().position(|e| e == event) {
            Some(at) => {
                self.outstanding.remove(at);
                true
            }
            None => false,
        }
    }
}

/// A `gdk::Event` compared by identity, which is what [`LetThrough`] needs: WebKit hands back the same event
/// object, and `gdk::Event` itself has no `PartialEq`.
pub(crate) struct SameEvent(pub(crate) gtk4::gdk::Event);

impl PartialEq for SameEvent {
    fn eq(&self, other: &Self) -> bool {
        self.0.as_ptr() == other.0.as_ptr()
    }
}

/// A module host's stage for one key press: WebKit's second delivery of a key this stage let through goes on
/// untouched, before `ask` runs, since asking again would decide the key twice; any other press is `ask`ed, and a
/// key the controller lets through is remembered for its return.
pub(crate) fn module_key<E: PartialEq>(
    let_through: &RefCell<LetThrough<E>>,
    event: Option<E>,
    ask: impl FnOnce() -> Verdict,
) -> glib::Propagation {
    if event
        .as_ref()
        .is_some_and(|event| let_through.borrow_mut().is_second_delivery(event))
    {
        return glib::Propagation::Proceed;
    }
    match ask() {
        Verdict::Consumed => glib::Propagation::Stop,
        Verdict::PassThrough => glib::Propagation::Proceed,
        Verdict::LetThrough => {
            if let Some(event) = event {
                let_through.borrow_mut().let_through(event);
            }
            glib::Propagation::Proceed
        }
    }
}

pub(crate) fn gtk_accel(chord: &Chord) -> String {
    let mut out = String::new();
    if chord.ctrl {
        out.push_str("<Control>");
    }
    if chord.alt {
        out.push_str("<Alt>");
    }
    if chord.shift {
        out.push_str("<Shift>");
    }
    let mut chars = chord.key.chars();
    let key = match (chars.next(), chars.next()) {
        (Some(c), None) => keysym_name(c).unwrap_or(&chord.key),
        _ => &chord.key,
    };
    out.push_str(key);
    out
}

pub(crate) fn command_accel(keybinding: &str) -> Result<String, String> {
    Chord::from_accel(keybinding).map(|chord| gtk_accel(&chord))
}

/// What `set_accels_for_action` gets for a command's `keybinding`. A modifier the keymap does not
/// read (`<Super>`, `<Meta>`, `<Hyper>`) is one Eitri never binds, so nothing can collide with it,
/// and GTK still reads it: such a keybinding goes to GTK as written and keeps working.
pub(crate) fn registered_accel(keybinding: &str) -> String {
    command_accel(keybinding).unwrap_or_else(|_| keybinding.to_string())
}

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

/// The Control keys down in this window, by keycode (a remapped Caps Lock types `Control_L` too), for the release that
/// ends a text size run: the last one's. What is held elsewhere is unknown here, so the list is forgotten when the
/// window loses the keys, and a press with Control off clears anything a lost release left in it.
#[derive(Debug, Default)]
pub(crate) struct CtrlKeys {
    down: RefCell<Vec<u32>>,
}

impl CtrlKeys {
    pub(crate) fn pressed(&self, key: Key, keycode: u32, state: ModifierType) {
        let mut down = self.down.borrow_mut();
        if !state.contains(ModifierType::CONTROL_MASK) {
            down.clear();
        }
        if matches!(key, Key::Control_L | Key::Control_R) && !down.contains(&keycode) {
            down.push(keycode);
        }
    }

    /// Whether this release leaves no Control key down.
    pub(crate) fn released(&self, key: Key, keycode: u32) -> bool {
        if !matches!(key, Key::Control_L | Key::Control_R) {
            return false;
        }
        let mut down = self.down.borrow_mut();
        down.retain(|held| *held != keycode);
        down.is_empty()
    }

    pub(crate) fn forget(&self) {
        self.down.borrow_mut().clear();
    }
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
                return match ch {
                    '=' | '+' | '-' | '0' if !meta => Press::ShiftTyped(KeySpec::char(ch, true, false)),
                    _ => Press::Other,
                };
            }
            Press::Key(KeySpec::char(ch, ctrl, meta))
        }
        _ => Press::Other,
    }
}

/// Keeps the native key name even when the prefix cannot spell its modified chord.
pub(crate) fn key_press(key: Key, keycode: u32, state: ModifierType) -> KeyPress {
    let translated = if MODIFIER_KEYS.contains(&key) || key == Key::Escape {
        None
    } else {
        named(key).or_else(|| {
            key.to_unicode()
                .filter(|ch| !ch.is_control() && !ch.is_whitespace())
                .map(KeyName::Char)
        })
    };
    KeyPress {
        keycode,
        press: classify(key, state),
        key: translated,
        mods: Mods {
            ctrl: state.contains(ModifierType::CONTROL_MASK),
            alt: state.contains(ModifierType::ALT_MASK),
            shift: state.contains(ModifierType::SHIFT_MASK),
        },
        super_or_hyper: state.intersects(ModifierType::SUPER_MASK | ModifierType::HYPER_MASK),
        super_or_meta: state.intersects(ModifierType::SUPER_MASK | ModifierType::META_MASK),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eitri_core::keymap::{
        prefix::{Outcome, Prefix, PrefixCommand, Press},
        Action, KeyName, KeySpec, Keymap,
    };
    use eitri_core::keymap::{root::ROOT_ACCELS, Chord};
    use eitri_core::layout::{Direction, ModuleKeys};
    use gtk4::gdk::{Key, ModifierType};
    use std::time::{Duration, Instant};

    #[test]
    fn gtk_accel_round_trips_every_root_chord() {
        let old = [
            "F11",
            "<Control>equal",
            "<Control>KP_Add",
            "<Control>minus",
            "<Control>KP_Subtract",
            "<Control>0",
            "<Control>KP_0",
            "<Control>KP_Insert",
        ];
        let chords: Vec<_> = ROOT_ACCELS.iter().flat_map(|r| r.chords).collect();
        assert_eq!(chords.len(), old.len());
        for (root, old) in chords.into_iter().zip(old) {
            let c = root.chord();
            assert_eq!(gtk_accel(&c), old);
            assert_eq!(Chord::from_accel(&gtk_accel(&c)).unwrap(), c);
        }
    }

    #[test]
    fn a_primary_or_uppercase_keybinding_registers_the_accelerator_it_did_before() {
        for (input, expected) in [
            ("<Primary>h", "<Control>h"),
            ("<Control>h", "<Control>h"),
            ("<Control>H", "<Control>h"),
            ("<Control><Alt>r", "<Control><Alt>r"),
            ("<Shift><Mod1><Ctrl>question", "<Control><Alt><Shift>question"),
            ("<primary>t", "<Control>t"),
            ("<control><alt>r", "<Control><Alt>r"),
        ] {
            assert_eq!(command_accel(input).unwrap(), expected);
            assert_eq!(registered_accel(input), expected);
        }
        assert!(command_accel("<Super>x").is_err());
        for unread in ["<Super>x", "<super>x", "<Control><Meta>x"] {
            assert_eq!(
                registered_accel(unread),
                unread,
                "a modifier the keymap does not read still reaches GTK as written"
            );
        }
    }

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
        // The per-module text size chords, from the main keys and the keypad.
        assert_eq!(classify(Key::equal, ctrl), key("C-="));
        assert_eq!(classify(Key::minus, ctrl), key("C--"));
        assert_eq!(classify(Key::_0, ctrl), key("C-0"));
        assert_eq!(classify(Key::KP_Add, ctrl), key("C-+"));
        assert_eq!(classify(Key::KP_Subtract, ctrl), key("C--"));
        // `+` where it is a key of its own, and the four when Shift typed them.
        assert_eq!(classify(Key::plus, ctrl), key("C-+"));
        for (native, name) in [
            (Key::equal, "C-="),
            (Key::plus, "C-+"),
            (Key::minus, "C--"),
            (Key::_0, "C-0"),
        ] {
            assert_eq!(
                classify(native, ctrl | shift),
                Press::ShiftTyped(KeySpec::parse(name).unwrap())
            );
            assert_eq!(classify(native, ctrl | shift | ModifierType::ALT_MASK), Press::Other);
        }
        assert_eq!(classify(Key::underscore, ctrl | shift), Press::Other);
        assert_eq!(classify(Key::Shift_L, shift), Press::Modifier);
        assert_eq!(classify(Key::Control_L, ctrl), Press::Modifier);
        assert_eq!(classify(Key::x, ModifierType::SUPER_MASK), Press::Other);
        assert_eq!(classify(Key::Return, ModifierType::empty()), key("Enter"));
        assert_eq!(classify(Key::space, ModifierType::empty()), key("Space"));
    }

    /// A run lasts until no Control key is down, whichever went first, and a release never seen does not hold it open.
    #[test]
    fn the_last_control_key_up_is_the_release_that_counts() {
        const LEFT: u32 = 37;
        const RIGHT: u32 = 105;
        const CAPS: u32 = 66;
        let keys = CtrlKeys::default();
        let ctrl = ModifierType::CONTROL_MASK;
        keys.pressed(Key::Control_L, LEFT, ModifierType::empty());
        assert!(keys.released(Key::Control_L, LEFT), "the only one");

        keys.pressed(Key::Control_L, LEFT, ModifierType::empty());
        keys.pressed(Key::Control_R, RIGHT, ctrl);
        assert!(!keys.released(Key::Control_L, LEFT), "the right one is still down");
        assert!(keys.released(Key::Control_R, RIGHT));
        assert!(!keys.released(Key::a, 38), "no other key is a Control key");

        keys.pressed(Key::Control_L, LEFT, ModifierType::empty());
        keys.pressed(Key::Control_L, CAPS, ctrl);
        assert!(
            !keys.released(Key::Control_L, CAPS),
            "Caps Lock as Ctrl types the same keysym"
        );
        assert!(keys.released(Key::Control_L, LEFT));

        keys.pressed(Key::Control_L, LEFT, ModifierType::empty());
        keys.pressed(Key::Control_R, RIGHT, ModifierType::empty());
        assert!(keys.released(Key::Control_R, RIGHT), "the left one's release was lost");

        assert!(
            keys.released(Key::Control_L, LEFT),
            "held since before the window had the keys"
        );

        keys.pressed(Key::Control_L, LEFT, ModifierType::empty());
        keys.forget();
        assert!(
            keys.released(Key::Control_R, RIGHT),
            "released elsewhere while another window had the keys"
        );
    }

    #[test]
    fn ctrl_space_classifies_as_c_space() {
        assert_eq!(classify(Key::space, ModifierType::CONTROL_MASK), key("C-Space"));
        assert_eq!(
            classify(Key::space, ModifierType::CONTROL_MASK | ModifierType::SHIFT_MASK),
            key("C-S-Space")
        );
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

    #[test]
    fn f1_alt_n_and_alt_p_pass_through_an_unarmed_default_prefix() {
        let t = Instant::now();
        for (native, state, name) in [
            (Key::F1, ModifierType::empty(), "F1"),
            (Key::n, ModifierType::ALT_MASK, "M-n"),
            (Key::p, ModifierType::ALT_MASK, "M-p"),
        ] {
            let mut p = defaults();
            let press = classify(native, state);
            assert_eq!(press, key(name));
            assert_eq!(p.press(press, K1, t), Outcome::Pass, "{name}");
            assert!(!p.is_armed(), "{name}");
        }
    }

    /// These non-repeating bindings become ordinary keys again after their first command.
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
            ": opens the panel's no-op command line"
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

    #[test]
    fn alt_shift_space_alt_minus_and_alt_underscore_read_as_tmux_names() {
        assert_eq!(
            classify(Key::space, ModifierType::ALT_MASK | ModifierType::SHIFT_MASK),
            key("M-S-Space")
        );
        assert_eq!(classify(Key::minus, ModifierType::ALT_MASK), key("M--"));
        assert_eq!(
            classify(Key::underscore, ModifierType::ALT_MASK | ModifierType::SHIFT_MASK),
            key("M-_")
        );
    }

    #[test]
    fn alt_shift_h_and_alt_return_read_as_tmux_names() {
        assert_eq!(
            classify(Key::H, ModifierType::ALT_MASK | ModifierType::SHIFT_MASK),
            key("M-H")
        );
        assert_eq!(classify(Key::Return, ModifierType::ALT_MASK), key("M-Enter"));
    }
    #[test]
    fn key_press_carries_the_key_before_the_prefix_folds_it() {
        let press = key_press(Key::Y, 29, ModifierType::CONTROL_MASK | ModifierType::SHIFT_MASK);
        assert_eq!(press.press, Press::Other);
        assert_eq!(press.key, Some(KeyName::Char('Y')));
        assert_eq!(press.keycode, 29);
        assert!(press.mods.ctrl && press.mods.shift && !press.mods.alt);
    }

    #[test]
    fn a_non_latin_dead_or_capslock_key_translates_as_it_did_before() {
        for (native, state, expected_press, expected_key) in [
            (
                Key::Cyrillic_er,
                ModifierType::empty(),
                Press::Key(KeySpec::char('р', false, false)),
                Some(KeyName::Char('р')),
            ),
            (
                Key::Cyrillic_er,
                ModifierType::CONTROL_MASK,
                Press::Key(KeySpec::char('р', true, false)),
                Some(KeyName::Char('р')),
            ),
            (Key::dead_acute, ModifierType::empty(), Press::Other, None),
            // With CapsLock on GDK already delivers the uppercase keyval.
            (Key::H, ModifierType::LOCK_MASK, key("H"), Some(KeyName::Char('H'))),
            (
                Key::y,
                ModifierType::CONTROL_MASK | ModifierType::SHIFT_MASK,
                Press::Other,
                Some(KeyName::Char('y')),
            ),
        ] {
            let press = key_press(native, 42, state);
            assert_eq!(press.press, expected_press, "{native:?} {state:?}");
            assert_eq!(press.key, expected_key, "{native:?} {state:?}");
        }
    }

    #[test]
    fn escape_and_a_bare_modifier_have_no_key_name() {
        for (native, state, expected) in [
            (Key::Escape, ModifierType::empty(), Press::Escape),
            (Key::Shift_L, ModifierType::SHIFT_MASK, Press::Modifier),
        ] {
            let press = key_press(native, 42, state);
            assert_eq!(press.press, expected);
            assert_eq!(press.key, None);
        }
        for state in [
            ModifierType::SUPER_MASK,
            ModifierType::HYPER_MASK,
            ModifierType::META_MASK,
            ModifierType::ALT_MASK,
        ] {
            let press = key_press(Key::y, 42, state);
            assert_eq!(
                press.super_or_hyper,
                state.intersects(ModifierType::SUPER_MASK | ModifierType::HYPER_MASK)
            );
            assert_eq!(
                press.super_or_meta,
                state.intersects(ModifierType::SUPER_MASK | ModifierType::META_MASK)
            );
            assert_eq!(press.mods.alt, state.contains(ModifierType::ALT_MASK));
        }
    }

    #[test]
    fn a_second_delivery_never_reaches_the_controller() {
        use std::cell::Cell;

        let seen = RefCell::new(LetThrough::new());
        seen.borrow_mut().let_through(7u32);
        let asked = Cell::new(false);
        let answer = module_key(&seen, Some(7), || {
            asked.set(true);
            Verdict::Consumed
        });
        assert_eq!(
            answer,
            glib::Propagation::Proceed,
            "WebKit's second delivery goes on untouched"
        );
        assert!(
            !asked.get(),
            "the controller is not asked about a key it already decided"
        );
        // A first delivery is asked, and one let through is known when it comes back.
        assert_eq!(
            module_key(&seen, Some(8), || Verdict::LetThrough),
            glib::Propagation::Proceed
        );
        assert!(seen.borrow_mut().is_second_delivery(&8));
        assert_eq!(
            module_key(&seen, Some(9), || Verdict::Consumed),
            glib::Propagation::Stop
        );
        assert!(
            !seen.borrow_mut().is_second_delivery(&9),
            "a consumed key never comes back"
        );
    }

    #[test]
    fn the_window_key_layers_are_hint_then_prompt_then_prefix() {
        assert_eq!(window_layers(), [Layer::Hint, Layer::Prompt, Layer::Prefix]);
        let mut added = Vec::new();
        let order = startup_layers(&mut |layer| added.push(layer));
        assert_eq!(
            added,
            vec![Layer::Prefix, Layer::Prompt],
            "the prefix first, so the prompt sees keys before it"
        );
        assert_eq!(order, added, "the order returned is the order added");
        let mut asked: Vec<Layer> = added.into_iter().rev().collect();
        asked.insert(0, Layer::Hint);
        assert_eq!(
            asked,
            window_layers(),
            "GTK asks the last added first, and HINT's comes later still"
        );
    }

    /// A chord let through to the page comes back once, and is known when it does -- so the controller does not
    /// decide it (and log it) a second time. Identity stands in for `gdk::Event`'s pointer equality here: `1` is
    /// one press, `2` another.
    #[test]
    fn a_key_let_through_to_the_page_is_known_when_webkit_hands_it_back() {
        let mut seen = LetThrough::new();
        assert!(!seen.is_second_delivery(&1), "the first delivery is new");
        seen.let_through(1);
        assert!(seen.is_second_delivery(&1), "WebKit put it back");
        assert!(
            !seen.is_second_delivery(&1),
            "forgotten once handed back: WebKit hands an event back at most once"
        );
        assert!(
            !seen.is_second_delivery(&2),
            "a press never let through never comes back"
        );
    }

    /// The round trip through the web process is asynchronous: a second chord can be let through before the
    /// first comes back, and both are still known, in either order.
    #[test]
    fn two_chords_in_flight_are_each_known_when_they_come_back() {
        let mut seen = LetThrough::new();
        seen.let_through(1);
        seen.let_through(2);
        assert!(seen.is_second_delivery(&2));
        assert!(seen.is_second_delivery(&1));
        assert!(!seen.is_second_delivery(&1));
    }

    /// A key the page handled never comes back; it is pushed out by later ones instead of being held for the
    /// life of the window.
    #[test]
    fn a_key_the_page_handled_is_forgotten_after_enough_later_ones() {
        let mut seen = LetThrough::new();
        seen.let_through(0);
        for press in 1..=LET_THROUGH_REMEMBERED {
            seen.let_through(press);
        }
        assert!(!seen.is_second_delivery(&0), "pushed out");
        assert!(seen.is_second_delivery(&1), "the oldest still remembered");
        assert!(seen.is_second_delivery(&LET_THROUGH_REMEMBERED), "the newest");
    }

    /// `Ctrl+h/j/k/l` as the controller reads them from GDK's keys and modifier state.
    fn nav(key: Key, state: ModifierType) -> Option<Direction> {
        eitri_core::wm::keys::nav_direction(&key_press(key, 0, state))
    }

    /// Super or Meta held with Control keeps the chord the module's; Hyper alone does not, so `Ctrl+l` with Hyper
    /// still moves the keys out of the bottom terminal and out of an editor with no nvim running.
    #[test]
    fn a_system_modifier_held_with_control_is_no_move() {
        assert_eq!(nav(Key::l, ModifierType::CONTROL_MASK), Some(Direction::Right));
        for held in [
            ModifierType::SUPER_MASK,
            ModifierType::META_MASK,
            ModifierType::SUPER_MASK | ModifierType::HYPER_MASK,
        ] {
            assert_eq!(
                nav(Key::l, ModifierType::CONTROL_MASK | held),
                None,
                "Ctrl+l with {held:?} held stays the module's"
            );
        }
        assert_eq!(
            nav(Key::l, ModifierType::CONTROL_MASK | ModifierType::HYPER_MASK),
            Some(Direction::Right),
            "Ctrl+l with Hyper alone held still moves"
        );
    }

    #[test]
    fn exactly_ctrl_hjkl_are_taken_from_the_terminal() {
        let ctrl = ModifierType::CONTROL_MASK;
        assert_eq!(nav(Key::h, ctrl), Some(Direction::Left));
        assert_eq!(nav(Key::j, ctrl), Some(Direction::Down));
        assert_eq!(nav(Key::k, ctrl), Some(Direction::Up));
        assert_eq!(nav(Key::l, ctrl), Some(Direction::Right));
        assert_eq!(
            nav(Key::k, ctrl | ModifierType::LOCK_MASK),
            Some(Direction::Up),
            "CapsLock is not a chord"
        );
        // GDK delivers the UPPERCASE keyval under CapsLock, not `Key::k` with `LOCK_MASK` alone.
        assert_eq!(
            nav(Key::K, ctrl | ModifierType::LOCK_MASK),
            Some(Direction::Up),
            "CapsLock delivers the uppercase keyval"
        );
        assert_eq!(nav(Key::l, ctrl | ModifierType::SHIFT_MASK), None);
        assert_eq!(nav(Key::a, ctrl), None, "the prefix is the prefix's");
        assert_eq!(nav(Key::k, ModifierType::empty()), None);
    }
}
