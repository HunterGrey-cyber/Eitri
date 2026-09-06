//! Keyboard + IME input handling for the embedded editor surface.
//!
//! Extracted from `poc/neovide_embed_live/src/main.rs` (P2 probe, `build_ui`'s IME+keyboard
//! wiring, roughly lines 848-935) as part of the neovide-editor extraction -- mirroring what
//! `crate::mouse` already does for click/drag/scroll. Shared session state (`LiveState`, and
//! `LiveSession` reached through it) is defined in `crate` (`lib.rs`).
//!
//! **Modifier fidelity caveat** (referenced from `crate::mouse`'s own doc): this module does not
//! attempt full key-code/modifier translation fidelity -- just enough plain-text input to drive a
//! real nvim buffer (printable characters via `Key::to_unicode()`, plus a handful of named keys
//! common enough to actually use nvim with: Escape, Return, BackSpace, Tab). Any held Ctrl/Alt/
//! Super is treated as "not plain text" and the event is left unhandled (`Propagation::Proceed`)
//! rather than guessed at -- unlike the reference, which accumulates a
//! `winit::keyboard::ModifiersState` this crate has no equivalent for. Mouse events instead read
//! GTK's own live `ModifierType` per-event (see `crate::mouse::format_modifier_string`).

use std::cell::RefCell;
use std::rc::Rc;

use gtk4::gdk::{Key, ModifierType};
use gtk4::glib;
use gtk4::prelude::*;
use gtk4::{EventControllerKey, GLArea, IMMulticontext};

use crate::LiveState;

/// Wires IME composition (`GtkIMMulticontext`) and plain-key input (`GtkEventControllerKey`) onto
/// `gl_area`, both driving `live_state` -- everything the reference's inline IME+keyboard block in
/// `build_ui` did, minus constructing an `Application`/`Window`. Returns the constructed
/// `IMMulticontext` so the caller (`NeovideEditorPane::new`) can store it in `self.im_context` and
/// later call `im_context.focus_in()` from `grab_focus()` once the host's window is actually shown
/// -- mirroring the reference's own `gl_area.grab_focus(); im_context.focus_in();` pairing, which
/// only makes sense once the widget is realized, not at construction time.
pub(crate) fn attach_keyboard_input(
    gl_area: &GLArea,
    live_state: &Rc<RefCell<LiveState>>,
) -> IMMulticontext {
    // --- IME: a GtkIMMulticontext attached to the key controller via `set_im_context`, which
    // makes GTK itself run `gtk_im_context_filter_keypress` on every key event before ever
    // emitting ::key-pressed -- a key an input method consumes (composition in progress) never
    // reaches the plain-text handler below at all. Composed text arrives separately via
    // `connect_commit`, forwarded to nvim through the exact same `send_text_input` path plain
    // keystrokes already use.
    let im_context = IMMulticontext::new();
    im_context.set_client_widget(Some(gl_area));
    {
        let live_state = live_state.clone();
        im_context.connect_commit(move |_ctx, text| {
            let mut live = live_state.borrow_mut();
            if let LiveState::Ready(session) = &mut *live {
                if !session.harness.has_neovim_exited() {
                    session.harness.send_text_input(text);
                    session.wants_frame.set(true);
                }
            }
        });
    }
    im_context.connect_preedit_start(|_ctx| println!("[ime] preedit-start"));
    im_context.connect_preedit_end(|_ctx| println!("[ime] preedit-end"));
    im_context.connect_preedit_changed(|ctx| {
        let (text, _attrs, _cursor_pos) = ctx.preedit_string();
        println!("[ime] preedit-changed: {text:?}");
    });

    // --- keyboard input: GtkEventControllerKey attached directly to the GLArea (made focusable
    // by the caller, grab_focus()'d by the host once it shows its window). Deliberately not full
    // key-code/modifier translation fidelity -- just enough plain-text input to drive a real nvim
    // buffer: printable characters via `Key::to_unicode()`, plus a handful of named keys common
    // enough to actually use nvim with (Escape to leave insert mode, Return, BackSpace, Tab). Any
    // held Ctrl/Alt/Super is treated as "not plain text" and ignored. IME composition input never
    // reaches this closure at all -- see the `im_context` wiring just above.
    {
        let live_state = live_state.clone();
        let key_controller = EventControllerKey::new();
        key_controller.set_im_context(Some(&im_context));
        key_controller.connect_key_pressed(move |_controller, key, _keycode, state| {
            if state.intersects(
                ModifierType::CONTROL_MASK | ModifierType::ALT_MASK | ModifierType::SUPER_MASK,
            ) {
                return glib::Propagation::Proceed;
            }

            let text: Option<&str> = match key {
                Key::Escape => Some("<Esc>"),
                Key::Return | Key::KP_Enter => Some("<CR>"),
                Key::BackSpace => Some("<BS>"),
                Key::Tab => Some("<Tab>"),
                _ => None,
            };

            let owned_char;
            let text = if let Some(text) = text {
                Some(text)
            } else if let Some(ch) = key.to_unicode() {
                if ch.is_control() {
                    None
                } else {
                    owned_char = ch.to_string();
                    Some(owned_char.as_str())
                }
            } else {
                None
            };

            let Some(text) = text else {
                return glib::Propagation::Proceed;
            };

            let mut live = live_state.borrow_mut();
            if let LiveState::Ready(session) = &mut *live {
                if !session.harness.has_neovim_exited() {
                    session.harness.send_text_input(text);
                    // A keypress deserves a same-tick-latency render rather than waiting on
                    // nvim's async redraw round-trip to eventually move `last_seen_batches`.
                    session.wants_frame.set(true);
                }
            }
            // Handled either way (even pre-Ready/post-exit) -- there is nothing else on this
            // single-widget pane that should react to a keypress instead.
            glib::Propagation::Stop
        });
        gl_area.add_controller(key_controller);
    }

    im_context
}
