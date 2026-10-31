//! The key monitor: every key-down, key-up and left click of the window, seen before AppKit dispatches it
//! to either pane, so the prefix and the moves between the panes are decided whichever pane has the keys (and
//! during an editor composition, when the input method would otherwise take the key). Every decision is
//! `crate::keys`'s; this only describes the event and acts on the answer.

use std::ptr::NonNull;
use std::rc::Weak;
use std::time::Instant;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_app_kit::{NSApplication, NSEvent, NSEventMask, NSEventModifierFlags, NSEventType};

use super::window::{responder_agrees, Host, Responder};
use crate::keys::{is_plain_escape, is_quit_chord, move_before_key, KeyAction, KeyDown, Keys, Pane};

/// A key-down as `crate::keys` reads it, from the event's fields.
pub fn key_down_of(chars: String, keycode: u16, flags: NSEventModifierFlags, repeat: bool) -> KeyDown {
    KeyDown {
        chars,
        keycode,
        ctrl: flags.contains(NSEventModifierFlags::Control),
        cmd: flags.contains(NSEventModifierFlags::Command),
        option: flags.contains(NSEventModifierFlags::Option),
        shift: flags.contains(NSEventModifierFlags::Shift),
        repeat,
    }
}

/// Whether a key may be decided at all: the keys stayed where they were while the first responder was checked,
/// and AppKit's first responder is the pane that has them.
pub fn key_may_pass(pane_before: Pane, pane_after: Pane, responder: Responder) -> bool {
    pane_before == pane_after && responder_agrees(pane_after, responder)
}

/// Whether AppKit still dispatches the event after the host acted on `action`.
pub fn passes_on(action: &KeyAction) -> bool {
    matches!(action, KeyAction::Pass)
}

/// Whether a key AppKit is about to dispatch is offered to the main menu first: only one that goes on (`Pass`,
/// never a key the prefix or a move took), only a Cmd chord, and only while the panel has the keys (in the editor
/// the Edit items are disabled and nvim gets its own `<D-…>` keys).
pub fn offers_to_menu(action: &KeyAction, pane: Pane, key: &KeyDown) -> bool {
    passes_on(action) && pane == Pane::Panel && key.cmd
}

/// Installs the monitor; the returned object must be kept for as long as it should run.
/// The monitor holds the host weakly: the host owns the monitor, and removes it when it shuts down.
pub(super) fn install(host: Weak<Host>) -> Option<Retained<AnyObject>> {
    let block = RcBlock::new(move |event: NonNull<NSEvent>| -> *mut NSEvent {
        let Some(host) = host.upgrade() else {
            return event.as_ptr();
        };
        // SAFETY: AppKit hands the monitor a live event for the length of this call.
        let passes = on_event(&host, unsafe { event.as_ref() });
        if passes {
            event.as_ptr()
        } else {
            std::ptr::null_mut()
        }
    });
    // SAFETY: a local monitor runs on the main thread, inside AppKit's own event loop, with the block retained
    // by AppKit until the monitor is removed.
    unsafe {
        NSEvent::addLocalMonitorForEventsMatchingMask_handler(
            NSEventMask::KeyDown | NSEventMask::KeyUp | NSEventMask::FlagsChanged | NSEventMask::LeftMouseDown,
            &block,
        )
    }
}

/// `true`: AppKit dispatches the event as usual.
fn on_event(host: &Host, event: &NSEvent) -> bool {
    let key_down = event.r#type() == NSEventType::KeyDown;
    if key_down {
        // The quit chord first, in any window: once the question's sheet is up it is the key window, and a second
        // press (or the held key's repeat) there would otherwise reach the menu's Quit and end the turn unasked.
        // Going on, it goes straight to the menu, past the prefix.
        let key = key_of(event);
        if is_quit_chord(&key) {
            let goes_on = host.quit_chord(key.repeat);
            if goes_on {
                // The prefix never sees it, so a wait it left armed would take the next key: perhaps the answer
                // to nvim's own prompt for an unsaved buffer.
                host.cancel_keys();
                // Straight to the menu's Quit, whichever pane has the keys (see `menu_took`).
                if menu_took(host, event) {
                    return false;
                }
                crate::log_line!("[mac] the menu did not take the quit chord; dispatched as usual");
            }
            return goes_on;
        }
        // The quit question's sheet is the key window and takes this key (its Cancel answers Return, and Escape
        // below); once the sheet is gone the key is still down, and its repeats must not reach a pane.
        if Keys::held_off_by_question(&key, host.quit_question_open()) {
            if let Some(assembly) = host.assembly() {
                assembly.keys.borrow_mut().hold(key.keycode);
            }
        }
        if is_plain_escape(&key) && host.cancel_quit_question() {
            return false;
        }
    }
    // A sheet, the folder chooser or another of Neovide's windows has keys of its own.
    if !host.is_our_window(event.window(host.mtm()).as_deref()) {
        return true;
    }
    match event.r#type() {
        NSEventType::KeyDown => on_key_down(host, event),
        NSEventType::KeyUp => {
            if let Some(assembly) = host.assembly() {
                assembly.keys.borrow_mut().key_up(event.keyCode());
            }
            true
        }
        NSEventType::FlagsChanged => {
            if let Some(assembly) = host.assembly() {
                let ctrl = event.modifierFlags().contains(NSEventModifierFlags::Control);
                assembly.keys.borrow_mut().modifiers_changed(ctrl);
            }
            true
        }
        NSEventType::LeftMouseDown => {
            // A click ends the prefix's wait, and a click on the other pane moves the keys there first, so
            // the view clicked gets them. The click itself always goes on.
            host.cancel_keys();
            if let Some(pane) = host.click_target(event.locationInWindow()) {
                if pane != host.pane() {
                    host.focus_pane(pane);
                }
            }
            true
        }
        _ => true,
    }
}

/// The key-down as `crate::keys` reads it.
fn key_of(event: &NSEvent) -> KeyDown {
    key_down_of(
        event
            .charactersIgnoringModifiers()
            .map(|s| s.to_string())
            .unwrap_or_default(),
        event.keyCode(),
        event.modifierFlags(),
        event.isARepeat(),
    )
}

fn on_key_down(host: &Host, event: &NSEvent) -> bool {
    let Some(assembly) = host.assembly() else {
        return true;
    };
    // A panel hidden by a narrowing window still holds the keys until the next turn; the key does not wait for it.
    if let Some(to) = move_before_key(host.pane(), host.panel_view_hidden()) {
        if !host.focus_pane(to) {
            crate::log_line!("[mac] a key was dropped: the panel is hidden and the {to:?} could not take the keys");
            return false;
        }
    }
    // The key goes where the window says the keys are, whatever AppKit did meanwhile. If the first responder
    // could not be put back, or putting it back moved the keys, the key is dropped: it would land in a pane
    // other than the one shown as having the keys.
    let before = host.pane();
    host.reconcile("a key arrived");
    if !key_may_pass(before, host.pane(), host.responder()) {
        crate::log_line!("[mac] a key was dropped: the keys and AppKit's first responder did not agree");
        return false;
    }
    let key = key_of(event);
    let pane = host.pane();
    let marked = pane == Pane::Editor && host.editor_marked_text();
    // Bound first, so the borrow of the keys ends before anything below calls back into the assembly.
    let action = assembly.keys.borrow_mut().key_down(&key, pane, marked, Instant::now());
    host.update_subtitle();
    let passes = passes_on(&action);
    if offers_to_menu(&action, pane, &key) && menu_took(host, event) {
        // The Edit menu's Copy, Paste, Undo and Select All act on the page this way; while the editor has the
        // keys those items are disabled, so the menu takes none of them and nvim gets them.
        return false;
    }
    if passes && pane == Pane::Editor {
        // The panel slows its stream while the user types in the editor.
        assembly.panel.note_editor_key();
    }
    match action {
        KeyAction::Pass | KeyAction::Swallow => {}
        KeyAction::RunPrefix(command) => assembly.run_prefix(command, pane),
        KeyAction::FocusEditor => {
            host.focus_pane(Pane::Editor);
        }
        KeyAction::EndCompositionThenNvim(keys) => {
            // The input method would take this key; nvim gets it through the link instead, where its nav
            // fallback decides (a split to the right, or the panel at its edge).
            host.end_editor_composition();
            if let Err(why) = assembly.editor.send_keys(keys) {
                crate::log_line!("[mac] {keys} did not reach the editor: {why}");
            }
        }
    }
    passes
}

/// Offers a Cmd chord to the main menu before AppKit's usual dispatch does. That dispatch asks the first
/// responder's views first, and the web view, holding the keys, sends every Cmd chord to its page, which keeps
/// it: the menu would never see it. `true`: a menu item took it, and the event goes no further.
fn menu_took(host: &Host, event: &NSEvent) -> bool {
    NSApplication::sharedApplication(host.mtm())
        .mainMenu()
        .is_some_and(|menu| menu.performKeyEquivalent(event))
}

#[cfg(test)]
mod tests {
    use super::*;
    use eitri_core::keymap::prefix::PrefixCommand;
    use eitri_core::keymap::Action;

    #[test]
    fn the_events_flags_become_the_keys_modifiers() {
        let key = key_down_of(
            "l".into(),
            37,
            NSEventModifierFlags::Control | NSEventModifierFlags::CapsLock,
            true,
        );
        assert_eq!(
            key,
            KeyDown {
                chars: "l".into(),
                keycode: 37,
                ctrl: true,
                cmd: false,
                option: false,
                shift: false,
                repeat: true,
            }
        );
        let key = key_down_of(
            "C".into(),
            8,
            NSEventModifierFlags::Command | NSEventModifierFlags::Option | NSEventModifierFlags::Shift,
            false,
        );
        assert!(key.cmd && key.option && key.shift && !key.ctrl && !key.repeat);
    }

    #[test]
    fn a_key_is_dropped_when_the_keys_and_the_responder_disagree() {
        assert!(key_may_pass(Pane::Editor, Pane::Editor, Responder::Editor));
        assert!(key_may_pass(Pane::Panel, Pane::Panel, Responder::Panel));
        // Putting the responder back failed.
        assert!(!key_may_pass(Pane::Editor, Pane::Editor, Responder::Panel));
        assert!(!key_may_pass(Pane::Panel, Pane::Panel, Responder::Other));
        assert!(!key_may_pass(Pane::Panel, Pane::Panel, Responder::None));
        // Putting it back moved the keys: the key was typed at the other pane.
        assert!(!key_may_pass(Pane::Panel, Pane::Editor, Responder::Editor));
    }

    #[test]
    fn only_a_cmd_chord_that_goes_on_in_the_panel_is_offered_to_the_menu() {
        let cmd_c = key_down_of("c".into(), 8, NSEventModifierFlags::Command, false);
        let ctrl_c = key_down_of("c".into(), 8, NSEventModifierFlags::Control, false);
        let plain_c = key_down_of("c".into(), 8, NSEventModifierFlags::empty(), false);
        assert!(offers_to_menu(&KeyAction::Pass, Pane::Panel, &cmd_c));
        // The editor's Cmd chords are nvim's.
        assert!(!offers_to_menu(&KeyAction::Pass, Pane::Editor, &cmd_c));
        // No Cmd: the page's own key.
        assert!(!offers_to_menu(&KeyAction::Pass, Pane::Panel, &ctrl_c));
        assert!(!offers_to_menu(&KeyAction::Pass, Pane::Panel, &plain_c));
        // A key the prefix or a move took never reaches the menu, in either pane.
        for pane in [Pane::Panel, Pane::Editor] {
            for action in [
                KeyAction::Swallow,
                KeyAction::FocusEditor,
                KeyAction::EndCompositionThenNvim("<C-l>"),
                KeyAction::RunPrefix(PrefixCommand::Run(Action::Zoom)),
            ] {
                assert!(!offers_to_menu(&action, pane, &cmd_c), "{action:?} in {pane:?}");
            }
        }
    }

    #[test]
    fn only_a_pass_reaches_appkit() {
        assert!(passes_on(&KeyAction::Pass));
        for action in [
            KeyAction::Swallow,
            KeyAction::FocusEditor,
            KeyAction::EndCompositionThenNvim("<C-l>"),
            KeyAction::RunPrefix(PrefixCommand::Run(Action::Zoom)),
        ] {
            assert!(!passes_on(&action), "{action:?}");
        }
    }
}
