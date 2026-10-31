//! The companion window's prefix: a window-level capture controller feeding
//! `eitri_core::keymap::prefix`, the keys translated as the main window's are (`gtk_host::keys`).
//! The companion window has no window controller, so its prefix runs here; the main window's is the
//! controller's window stage (`gtk_host::keys::install_window_layers`). Added at startup after every
//! other window-level controller, so it sees every key before any app accelerator and before the
//! panel; HINT's controller, added when a HINT starts, sees keys before this one.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Instant;

use gtk4::glib;
use gtk4::prelude::*;

use eitri_core::keymap::Keymap;
use eitri_core::layout::ModuleKeys;

pub(crate) use eitri_core::keymap::prefix::{literal_for, PrefixCommand, Waiting};
use eitri_core::keymap::prefix::{Outcome, Prefix, Press};

use crate::gtk_host::keys::classify;
use eitri_core::wm::keys::prefix_escape;

/// Installs the prefix on `window`. `on_command` runs a command and is told the keycode of the key
/// that ran it (HINT needs it: that key is still down); `on_waiting` hears every change of
/// `waiting()` made here, including one `on_command` made through the returned prefix. A change
/// made later, from outside (a timer ending the digit wait), is the caller's to announce.
/// `on_escape` runs when `Esc` reaches a prefix that is waiting for a key: the prefix's own `Esc`,
/// which disarms it. A bare `Esc` belongs to the module with the keys and never reaches it.
/// Cancelled by the window losing activation and by any click.
pub(crate) fn install(
    window: &gtk4::ApplicationWindow,
    keymap: Rc<Keymap>,
    module_keys: Rc<ModuleKeys>,
    on_command: impl Fn(PrefixCommand, u32) + 'static,
    on_waiting: impl Fn(Waiting) + 'static,
    on_escape: impl Fn() + 'static,
) -> Rc<RefCell<Prefix>> {
    let mut machine = Prefix::new(keymap, module_keys);
    machine.set_root_keep(Rc::new(eitri_core::keymap::companion::companion_root_keep));
    let prefix = Rc::new(RefCell::new(machine));
    let on_waiting: Rc<dyn Fn(Waiting)> = Rc::new(on_waiting);

    let keys = gtk4::EventControllerKey::new();
    keys.set_propagation_phase(gtk4::PropagationPhase::Capture);
    {
        let prefix = prefix.clone();
        let on_waiting = on_waiting.clone();
        keys.connect_key_pressed(move |_, key, keycode, state| {
            let press = classify(key, state);
            let escape = press == Press::Escape;
            let (outcome, was, now) = {
                let mut p = prefix.borrow_mut();
                let was = p.waiting();
                let outcome = p.press(press, keycode, Instant::now());
                (outcome, was, p.waiting())
            };
            if was != now {
                // Logged both ways since 2026-09-20: an arm that never disarms looks exactly like a
                // window refusing input.
                println!("[prefix] armed={} waiting={now:?}", now != Waiting::No);
                on_waiting(now);
            }
            if prefix_escape(escape, was) {
                on_escape();
            }
            match outcome {
                Outcome::Pass => glib::Propagation::Proceed,
                Outcome::Swallow => glib::Propagation::Stop,
                Outcome::Run(cmd) => {
                    println!("[prefix] {cmd:?}");
                    on_command(cmd, keycode);
                    // A command may start a wait of its own (`select.number`'s digit wait, which
                    // the host enters once its digits are drawn): announced like any other change.
                    let after = prefix.borrow().waiting();
                    if after != now {
                        println!("[prefix] armed={} waiting={after:?}", after != Waiting::No);
                        on_waiting(after);
                    }
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
            // Cancelled either way: a root key held while the window lost focus never sees its release.
            let mut p = prefix.borrow_mut();
            let armed = p.is_armed();
            p.cancel();
            drop(p);
            if armed {
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
    use eitri_core::keymap::{Action, KeymapOp, TabAction};

    fn root_op(key: &str, action: &str) -> KeymapOp {
        KeymapOp::Set {
            table: "root".into(),
            key: key.into(),
            action: action.into(),
            opts: vec![],
        }
    }

    fn press(key: &str) -> Press {
        Press::Key(eitri_core::keymap::KeySpec::parse(key).unwrap())
    }

    #[test]
    fn a_root_key_this_window_refuses_passes_on_and_one_it_runs_runs() {
        let keymap = Keymap::apply_user(&[root_op("M-y", "zoom"), root_op("M-u", "tab.next")], &[]).unwrap();
        let module_keys = ModuleKeys::build(&[], &keymap).unwrap();
        let mut machine = Prefix::new(Rc::new(keymap), Rc::new(module_keys));
        machine.set_root_keep(Rc::new(eitri_core::keymap::companion::companion_root_keep));
        let now = Instant::now();
        assert_eq!(
            machine.press(press("M-y"), 10, now),
            Outcome::Pass,
            "zoom is not in a companion window"
        );
        assert_eq!(
            machine.press(press("M-u"), 11, now),
            Outcome::Run(eitri_core::keymap::prefix::PrefixCommand::Run(Action::Tab(
                TabAction::Next
            )))
        );
    }
}
