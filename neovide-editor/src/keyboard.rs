//! Keyboard + IME input handling for the embedded editor surface.
//!
//! Extracted from `poc/neovide_embed_live/src/main.rs` (P2 probe, `build_ui`'s IME+keyboard
//! wiring, roughly lines 848-935) as part of the neovide-editor extraction -- mirroring what
//! `crate::mouse` already does for click/drag/scroll. Shared session state (`LiveState`, and
//! `LiveSession` reached through it) is defined in `crate` (`lib.rs`).
//!
//! **Scope (2026-09-08, P3 keyboard depth + P4 candidate placement).** The original extraction
//! carried over the P2 probe's deliberate simplification -- printable characters via
//! `Key::to_unicode()` plus four named keys, and *any* held Ctrl/Alt/Super treated as "not plain
//! text" and dropped. That is no longer true: this module now translates GDK key events into
//! Neovim's own bracketed key notation, mirroring the fork's own
//! `neovide::window::keyboard_manager::KeyboardManager` (`format_key_text`/
//! `format_modifier_string`/`get_special_key`) rule for rule, just keyed off GTK's
//! `gdk::Key`/`gdk::ModifierType` instead of winit's `KeyEvent`/`ModifiersState`. Modifier chords
//! (`<C-w>`, `<M-x>`, `<D-p>`, `<C-M-j>`, ...), function keys, arrows, and the
//! navigation/editing named keys all reach nvim now; unrecognized keys are still silently dropped
//! rather than guessed at, the same stance `crate::mouse::gdk_button_to_button_text` takes.
//!
//! The three `preedit-*` signals additionally call `IMContext::set_cursor_location` so the input
//! method draws its candidate window next to the real Neovim cursor -- see
//! `im_cursor_rect_for_editor`.

use std::cell::RefCell;
use std::rc::Rc;

use gtk4::gdk::{Key, ModifierType, Rectangle};
use gtk4::glib;
use gtk4::prelude::*;
use gtk4::{EventControllerKey, GLArea, IMMulticontext};

use neovide::units::{GridScale, PixelPos};

use crate::LiveState;

/// Told each time a key the user pressed in this pane was sent to nvim (see
/// [`NeovideEditorPane::connect_key_activity`](crate::NeovideEditorPane::connect_key_activity)).
///
/// A notification and nothing else: it never changes what reaches nvim, and it fires from the
/// same place the text is sent (`send_text`), so the two cannot drift apart. Cheap to clone -- a
/// shared slot -- so `attach_keyboard_input`'s closures and the pane hold the same one.
#[derive(Clone, Default)]
pub(crate) struct KeyActivity {
    callback: Rc<RefCell<ActivityCallback>>,
}

/// Named for `clippy::type_complexity`. An `Rc` so `notify` can call it with the slot released.
type ActivityCallback = Option<Rc<dyn Fn()>>;

impl KeyActivity {
    /// Replaces any earlier callback.
    pub(crate) fn set(&self, callback: impl Fn() + 'static) {
        *self.callback.borrow_mut() = Some(Rc::new(callback));
    }

    /// Calls the callback, if any, with the slot's borrow released first: a host's handler is free
    /// to call back into this pane (or replace itself through `set`).
    pub(crate) fn notify(&self) {
        let callback = self.callback.borrow().clone();
        if let Some(callback) = callback {
            callback();
        }
    }
}

/// Where keyboard text goes: the live nvim session, or a stand-in in a test.
pub(crate) trait TextTarget {
    /// Whether text sent now would reach a running nvim.
    fn accepts_text(&self) -> bool;
    fn send_text(&mut self, text: &str);
}

impl TextTarget for LiveState {
    fn accepts_text(&self) -> bool {
        matches!(self, LiveState::Ready(session) if !session.harness.has_neovim_exited())
    }

    fn send_text(&mut self, text: &str) {
        if let LiveState::Ready(session) = self {
            // nvim's reply requests the frame (event-driven redraw).
            session.harness.send_text_input(text);
        }
    }
}

/// The one way keyboard text (a key event's Neovim notation, or an input method's commit) reaches
/// nvim from this pane. Sends it when `target` can take it and only then tells `activity`, after
/// the borrow of `target` is released. Returns whether it was sent.
///
/// **Every keyboard path goes through here** -- `keyboard_text_reaches_nvim_only_through_send_text`
/// reads this file to hold that -- so "the user typed in the editor" is not a second opinion that
/// could disagree with what nvim actually received. `NeovideEditorPane::send_keys` deliberately
/// does not: it is a host handing a key on (`Ctrl+a Ctrl+a`) or driving a scratch round trip, not
/// the user typing here.
pub(crate) fn send_text<T: TextTarget>(target: &RefCell<T>, text: &str, activity: &KeyActivity) -> bool {
    let sent = {
        let mut target = target.borrow_mut();
        let accepts = target.accepts_text();
        if accepts {
            target.send_text(text);
        }
        accepts
    };
    if sent {
        activity.notify();
    }
    sent
}

/// Neovim's own name for a GDK key that is *never* sent as a bare character -- the GTK-keyed
/// equivalent of the fork's `neovide::window::keyboard_manager::get_special_key`, which does the
/// same job against winit's `NamedKey`. A name returned here is always bracketed
/// (`<Esc>`, `<F5>`, `<S-Tab>`, ...) and always carries Shift in its modifier prefix when held,
/// exactly like the reference's own `is_special` flag drives.
///
/// Coverage is deliberately "what a real Vim/LazyVim user actually presses", not an exhaustive
/// enumeration of GDK's keyval space: Escape/Return/BackSpace/Tab/Space/Delete/Insert, the four
/// arrows, Home/End/PageUp/PageDown, and F1-F12. A keyval this table doesn't know is not guessed
/// at -- it falls through to the printable-character path, and failing that is dropped entirely.
///
/// Numpad keys are mapped to their *editing* meanings (`KP_Enter` -> `Enter`, `KP_Left` ->
/// `Left`, ...) rather than to the reference's `k`-prefixed Neovim numpad names (`kEnter`,
/// `kLeft`): GDK already resolves NumLock for us by handing over a different keyval
/// (`KP_Left` vs `KP_4`), so the physical-location distinction the reference needs
/// `KeyLocation::Numpad` plus its own NumLock bookkeeping to recover is not available here -- and
/// is not information nvim needs in order to act correctly. NumLock-on digits arrive as ordinary
/// printable characters.
pub(crate) fn gdk_key_to_special_name(key: Key) -> Option<&'static str> {
    let name = match key {
        Key::Escape => "Esc",
        Key::Return | Key::KP_Enter | Key::ISO_Enter => "Enter",
        Key::BackSpace => "BS",
        Key::Tab | Key::ISO_Left_Tab | Key::KP_Tab => "Tab",
        Key::space | Key::KP_Space => "Space",
        Key::Delete | Key::KP_Delete => "Del",
        Key::Insert | Key::KP_Insert => "Insert",
        Key::Up | Key::KP_Up => "Up",
        Key::Down | Key::KP_Down => "Down",
        Key::Left | Key::KP_Left => "Left",
        Key::Right | Key::KP_Right => "Right",
        Key::Home | Key::KP_Home => "Home",
        Key::End | Key::KP_End => "End",
        Key::Page_Up | Key::KP_Page_Up => "PageUp",
        Key::Page_Down | Key::KP_Page_Down => "PageDown",
        Key::F1 => "F1",
        Key::F2 => "F2",
        Key::F3 => "F3",
        Key::F4 => "F4",
        Key::F5 => "F5",
        Key::F6 => "F6",
        Key::F7 => "F7",
        Key::F8 => "F8",
        Key::F9 => "F9",
        Key::F10 => "F10",
        Key::F11 => "F11",
        Key::F12 => "F12",
        _ => return None,
    };
    Some(name)
}

fn is_ascii_alphabetic_char(text: &str) -> bool {
    text.len() == 1 && text.chars().next().unwrap().is_ascii_alphabetic()
}

/// Neovim's modifier prefix (`"S-"`/`"C-"`/`"M-"`/`"D-"`, concatenated in that order) for a key
/// event -- a direct port of `KeyboardManager::format_modifier_string`, including the one rule
/// that is not obvious and is easy to lose by simplifying: **Shift is only ever emitted for a
/// special key, or for Ctrl + an ASCII letter.** For every other key the shifted character itself
/// already carries the information (GDK hands over `A`, not `a`+Shift; `!`, not `1`+Shift), and
/// adding a redundant `S-` would name a chord nvim resolves differently. See that method's own
/// comment in the fork for the full reasoning, including why `<C-a>`/`<C-A>` are the same key but
/// `<C-S-A>` is not.
///
/// Distinct from `crate::mouse::format_modifier_string`, which is effectively the
/// `is_special = true` case with no key text to reason about -- a mouse event always includes
/// Shift when held. Kept separate rather than shared because the deciding inputs here (is this a
/// special key, and is its text an ASCII letter) simply don't exist for a mouse button.
pub(crate) fn format_modifier_string(text: &str, is_special: bool, state: ModifierType) -> String {
    let control = state.contains(ModifierType::CONTROL_MASK);
    let include_shift =
        state.contains(ModifierType::SHIFT_MASK) && (is_special || (control && is_ascii_alphabetic_char(text)));

    let mut modifiers = String::new();
    if include_shift {
        modifiers.push_str("S-");
    }
    if control {
        modifiers.push_str("C-");
    }
    if state.contains(ModifierType::ALT_MASK) {
        modifiers.push_str("M-");
    }
    if state.contains(ModifierType::SUPER_MASK) {
        modifiers.push_str("D-");
    }
    modifiers
}

/// The exact string handed to `LiveHarness::send_text_input` (i.e. to `nvim.input`) for one key --
/// a direct port of `KeyboardManager::format_key_text`.
///
/// - Shift + an ASCII letter is upper-cased here as well as by GDK, because Neovim normalizes
///   shifted alphas to uppercase internally and the reference found platform cases where the
///   toolkit didn't (idempotent when GDK already did it, which on a normal layout it has).
/// - `<` becomes `lt` and is force-bracketed, since a literal `<` would otherwise open a key
///   name. Note it becomes special *after* the modifier string is computed, matching the
///   reference -- so `Shift+<` does not gain a spurious `S-`.
/// - With no modifiers, a plain character is sent as itself (`"a"`), a special key bracketed
///   (`"<Esc>"`). With modifiers, everything is bracketed (`"<C-w>"`, `"<S-Tab>"`).
pub(crate) fn format_key_text(text: &str, is_special: bool, state: ModifierType) -> String {
    let text = if state.contains(ModifierType::SHIFT_MASK) && is_ascii_alphabetic_char(text) {
        text.to_uppercase()
    } else {
        text.to_string()
    };

    let modifiers = format_modifier_string(&text, is_special, state);
    let (text, is_special) = if text == "<" {
        ("lt".to_string(), true)
    } else {
        (text, is_special)
    };

    if modifiers.is_empty() {
        if is_special {
            format!("<{text}>")
        } else {
            text
        }
    } else {
        format!("<{modifiers}{text}>")
    }
}

/// Literal text (an IME commit, or any other already-decided string) made safe to hand to
/// `nvim.input`, which parses `<...>` as key notation: every `<` becomes `<lt>`.
///
/// **This fixes a real, reproducible bug found during this pass's own sandbox verification, not a
/// hypothetical one.** With a `GtkIMContext` attached to the key controller, GTK's IM runs first
/// and *consumes* ordinary printable keypresses, emitting them through `::commit` rather than
/// letting `::key-pressed` fire -- so the `<` in typed text has always taken this path, never
/// `format_key_text`'s own `<` -> `lt` branch. Typing `ab<cd` into a real nvim buffer in the
/// sandbox produced `abcd`: nvim consumed `<cd` as the opening of a key name that never
/// terminated, and the character vanished silently.
///
/// The fork's own `KeyboardManager` has the same hazard and handles it the same way in spirit
/// (`Ime::Commit` is passed through `format_key_text(text, false)`, whose `text == "<"` branch
/// rewrites it), but only for a commit that is exactly `"<"` and nothing longer. Escaping every
/// occurrence instead is strictly safer and costs nothing: an IME commit is arbitrary text, and a
/// multi-character commit containing `<` is perfectly possible.
pub(crate) fn escape_for_nvim_input(text: &str) -> String {
    if text.contains('<') {
        text.replace('<', "<lt>")
    } else {
        text.to_string()
    }
}

/// Full GDK key event -> Neovim input string, or `None` for a key this crate deliberately doesn't
/// forward (a bare modifier press, a dead key, a media key -- anything `gdk_key_to_special_name`
/// doesn't name and `Key::to_unicode()` can't turn into a printable character).
///
/// Note the *order*: the special-key table wins over `to_unicode()`, exactly like the reference's
/// `format_key` tries `get_special_key` before `format_normal_key`. Several named keys do have a
/// Unicode value (Escape is U+001B, Return U+000D, Tab U+0009, Space U+0020) and sending those raw
/// would be wrong for Escape/Return/Tab and would make `<C-Space>` inexpressible.
pub(crate) fn format_key_event(key: Key, state: ModifierType) -> Option<String> {
    if let Some(name) = gdk_key_to_special_name(key) {
        return Some(format_key_text(name, true, state));
    }

    let ch = key.to_unicode()?;
    if ch.is_control() {
        return None;
    }
    Some(format_key_text(&ch.to_string(), false, state))
}

/// The widget-local rectangle handed to `IMContext::set_cursor_location` so the platform input
/// method draws its candidate window against the real Neovim cursor cell instead of falling back
/// to the whole widget.
///
/// Three coordinate facts make this a one-liner rather than a re-derivation, and all three were
/// checked in the fork's own source rather than assumed:
///
/// 1. `LiveHarness::cursor_pixel_position()` (`Renderer::get_cursor_destination`) is already
///    expressed in the same pixel space as the `content_region` this crate hands to
///    `render_frame` -- `RenderedWindow::get_target_position` folds `grid_rect.min`
///    (`content_region / grid_scale`) into the window's own grid position before
///    `CursorRenderer::update_cursor_destination` multiplies through by `grid_scale`. So the
///    `content_region.min` offset `crate::gl_interop::pixel_to_grid_pos` has to *subtract* on the
///    way in is already *added* here on the way out; there is nothing more to apply.
/// 2. That space is device pixels (`crate::gl_interop::current_content_region` builds
///    `content_region` from `gl_area.width() * gl_area.scale_factor()`), whereas
///    `set_cursor_location` wants widget-local *logical* pixels -- the same units GTK4's own
///    `GestureClick`/`EventControllerMotion` report `x`/`y` in. Hence the single division by
///    `scale_factor`, the exact inverse of the multiplication `pixel_to_grid_pos` performs.
/// 3. The rect's size is one grid cell (`grid_scale`, likewise device pixels -> logical), so the
///    input method knows the height of the line it must not cover.
///
/// `scale_factor` is GTK's own rounded integer `Widget::scale_factor()`, matching every other
/// coordinate conversion in this crate; P5's cross-scale click-accuracy work established that both
/// sides of these conversions share that same rounded basis, so the rounded/true-fractional scale
/// mismatch never enters the arithmetic.
pub(crate) fn im_cursor_rect_for_editor(
    cursor_pixel_pos: PixelPos<f32>,
    grid_scale: GridScale,
    scale_factor: i32,
) -> Rectangle {
    let scale_factor = scale_factor.max(1) as f32;
    Rectangle::new(
        (cursor_pixel_pos.x / scale_factor).round() as i32,
        (cursor_pixel_pos.y / scale_factor).round() as i32,
        (grid_scale.width() / scale_factor).ceil().max(1.0) as i32,
        (grid_scale.height() / scale_factor).ceil().max(1.0) as i32,
    )
}

/// Reads the live cursor position out of `live_state` and tells `im_context` where it is. Called
/// from all three `preedit-*` handlers: `preedit-start` so the very first candidate window already
/// appears in the right place, `preedit-changed` so it keeps tracking as the composition grows
/// (and, more importantly, so it is still correct if the cursor moved between two compositions),
/// and `preedit-end` so a subsequent composition never begins against a stale rect.
///
/// A no-op unless the session is `Ready` and nvim is alive -- there is no meaningful cursor to
/// point at otherwise, and leaving the previously published location in place is strictly better
/// than publishing a `(0, 0)` one.
fn publish_cursor_location(im_context: &IMMulticontext, gl_area: &GLArea, live_state: &Rc<RefCell<LiveState>>) {
    let live = live_state.borrow();
    let LiveState::Ready(session) = &*live else {
        return;
    };
    if session.harness.has_neovim_exited() {
        return;
    }
    let rect = im_cursor_rect_for_editor(
        session.harness.cursor_pixel_position(),
        session.harness.grid_scale(),
        gl_area.scale_factor(),
    );
    if std::env::var_os("NEOVIDE_EDITOR_LOG_IME").is_some() {
        println!(
            "[ime] set_cursor_location x={} y={} w={} h={}",
            rect.x(),
            rect.y(),
            rect.width(),
            rect.height()
        );
    }
    im_context.set_cursor_location(&rect);
}

/// Wires IME composition (`GtkIMMulticontext`) and key input (`GtkEventControllerKey`) onto
/// `gl_area`, both driving `live_state` -- everything the reference's inline IME+keyboard block in
/// `build_ui` did, minus constructing an `Application`/`Window`. Returns the constructed
/// `IMMulticontext` so the caller (`NeovideEditorPane::new`) can store it in `self.im_context` and
/// later call `im_context.focus_in()` from `grab_focus()` once the host's window is actually shown
/// -- mirroring the reference's own `gl_area.grab_focus(); im_context.focus_in();` pairing, which
/// only makes sense once the widget is realized, not at construction time.
pub(crate) fn attach_keyboard_input(
    gl_area: &GLArea,
    live_state: &Rc<RefCell<LiveState>>,
    activity: &KeyActivity,
) -> IMMulticontext {
    // --- IME: a GtkIMMulticontext attached to the key controller via `set_im_context`, which
    // makes GTK itself run `gtk_im_context_filter_keypress` on every key event before ever
    // emitting ::key-pressed -- a key an input method consumes (composition in progress) never
    // reaches the key handler below at all. That ordering is also what keeps the modifier-chord
    // forwarding added below from interfering with an in-flight composition: a chord the IME wants
    // (fcitx5's own input-method switch, rime's own bindings) is consumed before this crate ever
    // sees it, and one it doesn't want passes through unconsumed -- the same arbitration real
    // Neovide gets from winit. Composed text arrives separately via `connect_commit`, forwarded to
    // nvim through the exact same `send_text_input` path plain keystrokes already use.
    let im_context = IMMulticontext::new();
    im_context.set_client_widget(Some(gl_area));
    // The editor draws no preedit: nothing here renders `preedit-changed`'s string, and the
    // renderer has no place for it (native Neovide shows none either). Left at GTK's default,
    // the input method hands the composition to this client and shows it nowhere -- on the
    // owner's GNOME (fcitx5 through GTK_IM_MODULE=fcitx, candidates drawn by kimpanel) the pinyin
    // being typed was simply invisible (2026-09-28). With preedit off, the input method shows the
    // composition in its own candidate window, next to the rectangle published below.
    im_context.set_use_preedit(false);
    {
        let live_state = live_state.clone();
        let activity = activity.clone();
        im_context.connect_commit(move |_ctx, text| {
            let text = escape_for_nvim_input(text);
            if std::env::var_os("NEOVIDE_EDITOR_LOG_KEYS").is_some() {
                println!("[key] im-commit -> {text:?}");
            }
            send_text(&live_state, &text, &activity);
        });
    }
    // --- IME candidate-window placement (P4). Without these, `set_cursor_location` is never
    // called at all and the input method has no choice but to position its candidate popup
    // against the whole `GLArea` -- which is exactly what the feasibility doc's own check
    // ("候选框必须出现在正确的 Neovim cursor 附近") had no way to pass. See
    // `im_cursor_rect_for_editor` for the coordinate reasoning.
    {
        let live_state = live_state.clone();
        let area = gl_area.clone();
        im_context.connect_preedit_start(move |ctx| {
            publish_cursor_location(ctx, &area, &live_state);
        });
    }
    {
        let live_state = live_state.clone();
        let area = gl_area.clone();
        im_context.connect_preedit_end(move |ctx| {
            publish_cursor_location(ctx, &area, &live_state);
        });
    }
    {
        let live_state = live_state.clone();
        let area = gl_area.clone();
        im_context.connect_preedit_changed(move |ctx| {
            publish_cursor_location(ctx, &area, &live_state);
            if std::env::var_os("NEOVIDE_EDITOR_LOG_IME").is_some() {
                let (text, _attrs, _cursor_pos) = ctx.preedit_string();
                println!("[ime] preedit-changed: {text:?}");
            }
        });
    }

    // --- IME focus tracking. `NeovideEditorPane::grab_focus()` calls `im_context.focus_in()` once,
    // when the host first shows its window, which is all a single-focusable-widget window ever
    // needed -- and every crate this code was verified in before `shell` was exactly that. It is
    // not enough now: an input method keeps per-client focus state, and a context that is never
    // told it lost focus keeps believing it owns the keyboard after the user clicks into the agent
    // panel. Wiring GTK's own focus signals is the standard fix and the same thing every real GTK
    // text widget does; the explicit `grab_focus()` in `crate::mouse::handle_mouse_button` is what
    // makes the enter side of it actually fire on a click.
    {
        let focus_controller = gtk4::EventControllerFocus::new();
        {
            let ctx = im_context.clone();
            focus_controller.connect_enter(move |_| ctx.focus_in());
        }
        {
            let ctx = im_context.clone();
            focus_controller.connect_leave(move |_| ctx.focus_out());
        }
        gl_area.add_controller(focus_controller);
    }

    // --- IME candidate placement without preedit. With `set_use_preedit(false)` the `preedit-*`
    // signals above may never fire, so the rectangle is also published on every key press, in the
    // capture phase: that runs before the key controller below, whose attached input method may
    // consume the key (the first letter of a composition) before its own ::key-pressed ever runs.
    // The composition does not move nvim's cursor, so the rectangle published then stays right.
    {
        let live_state = live_state.clone();
        let area = gl_area.clone();
        let ctx = im_context.clone();
        let placement = EventControllerKey::new();
        placement.set_propagation_phase(gtk4::PropagationPhase::Capture);
        placement.connect_key_pressed(move |_controller, _key, _keycode, _state| {
            publish_cursor_location(&ctx, &area, &live_state);
            glib::Propagation::Proceed
        });
        gl_area.add_controller(placement);
    }

    // --- key input: GtkEventControllerKey attached directly to the GLArea (made focusable by the
    // caller, grab_focus()'d by the host once it shows its window). Every event is translated by
    // `format_key_event` into Neovim's own key notation and forwarded through the exact same
    // `send_text_input` (`nvim.input`) path an IME commit uses -- nvim cannot tell the two apart,
    // and interprets `<...>`-bracketed notation itself. IME composition input never reaches this
    // closure at all -- see the `im_context` wiring just above.
    {
        let live_state = live_state.clone();
        let activity = activity.clone();
        let key_controller = EventControllerKey::new();
        key_controller.set_im_context(Some(&im_context));
        key_controller.connect_key_pressed(move |_controller, key, _keycode, state| {
            // Opt-in tracing of what actually reached this closure, for verification work: it is
            // the only way to tell "the key never got here" (compositor/IME/accelerator ate it)
            // apart from "it got here and was translated wrong", and those two have completely
            // different fixes.
            let log_keys = std::env::var_os("NEOVIDE_EDITOR_LOG_KEYS").is_some();

            // A key this crate has no honest Neovim name for (a bare modifier press, a dead key, a
            // media key) is left alone rather than guessed at, so whatever else is listening --
            // a window-level accelerator, the host shell -- still gets its chance at it.
            let Some(text) = format_key_event(key, state) else {
                if log_keys {
                    println!("[key] {:?} state={state:?} -> (dropped)", key.name());
                }
                return glib::Propagation::Proceed;
            };
            if log_keys {
                println!("[key] {:?} state={state:?} -> {text:?}", key.name());
            }

            send_text(&live_state, &text, &activity);
            // Handled either way (even pre-Ready/post-exit) -- there is nothing else on this
            // single-widget pane that should react to a keypress instead.
            glib::Propagation::Stop
        });
        gl_area.add_controller(key_controller);
    }

    im_context
}

/// Pure-function checks for the GDK-key -> Neovim-notation translation added in the P3 keyboard
/// depth pass, in the same spirit as `crate::mouse`'s own tests: they pin the notation down
/// against no GTK/GLib runtime at all, so a future edit can't silently drop a modifier or invent a
/// key name nvim doesn't know. They are deliberately *not* a substitute for the real end-to-end
/// sandbox verification (a real chord producing its real nvim effect, confirmed by an RPC oracle)
/// recorded in this crate's `MANUAL_VERIFICATION.md`.
#[cfg(test)]
mod tests {
    use super::*;
    use neovide::units::PixelSize;

    const CTRL: ModifierType = ModifierType::CONTROL_MASK;
    const SHIFT: ModifierType = ModifierType::SHIFT_MASK;
    const ALT: ModifierType = ModifierType::ALT_MASK;
    const SUPER: ModifierType = ModifierType::SUPER_MASK;

    #[test]
    fn plain_printable_keys_are_sent_as_bare_characters() {
        // The behavior the P2 probe already had, preserved exactly: no brackets, no prefix.
        assert_eq!(format_key_event(Key::a, ModifierType::empty()).as_deref(), Some("a"));
        assert_eq!(format_key_event(Key::_7, ModifierType::empty()).as_deref(), Some("7"));
        // GDK already hands over the shifted keyval; Shift alone adds no prefix.
        assert_eq!(format_key_event(Key::A, SHIFT).as_deref(), Some("A"));
        assert_eq!(format_key_event(Key::exclam, SHIFT).as_deref(), Some("!"));
    }

    #[test]
    fn named_keys_are_bracketed_and_beat_their_own_unicode_value() {
        // Escape/Return/Tab/Space all have a Unicode value; the special table must win, or
        // Escape would be sent as a raw U+001B and `<C-Space>` would be inexpressible.
        assert_eq!(
            format_key_event(Key::Escape, ModifierType::empty()).as_deref(),
            Some("<Esc>")
        );
        assert_eq!(
            format_key_event(Key::Return, ModifierType::empty()).as_deref(),
            Some("<Enter>")
        );
        assert_eq!(
            format_key_event(Key::Tab, ModifierType::empty()).as_deref(),
            Some("<Tab>")
        );
        assert_eq!(
            format_key_event(Key::space, ModifierType::empty()).as_deref(),
            Some("<Space>")
        );
        assert_eq!(
            format_key_event(Key::BackSpace, ModifierType::empty()).as_deref(),
            Some("<BS>")
        );
        assert_eq!(
            format_key_event(Key::Up, ModifierType::empty()).as_deref(),
            Some("<Up>")
        );
        assert_eq!(
            format_key_event(Key::F5, ModifierType::empty()).as_deref(),
            Some("<F5>")
        );
        assert_eq!(
            format_key_event(Key::F12, ModifierType::empty()).as_deref(),
            Some("<F12>")
        );
        assert_eq!(
            format_key_event(Key::Page_Down, ModifierType::empty()).as_deref(),
            Some("<PageDown>")
        );
        assert_eq!(
            format_key_event(Key::KP_Enter, ModifierType::empty()).as_deref(),
            Some("<Enter>")
        );
    }

    #[test]
    fn modifier_chords_use_neovim_bracket_notation() {
        assert_eq!(format_key_event(Key::w, CTRL).as_deref(), Some("<C-w>"));
        assert_eq!(format_key_event(Key::p, CTRL).as_deref(), Some("<C-p>"));
        assert_eq!(format_key_event(Key::x, ALT).as_deref(), Some("<M-x>"));
        assert_eq!(format_key_event(Key::p, SUPER).as_deref(), Some("<D-p>"));
        assert_eq!(format_key_event(Key::j, CTRL | ALT).as_deref(), Some("<C-M-j>"));
        assert_eq!(
            format_key_event(Key::Z, SHIFT | CTRL | ALT | SUPER).as_deref(),
            Some("<S-C-M-D-Z>")
        );
        // Special keys compose with modifiers too.
        assert_eq!(format_key_event(Key::Tab, SHIFT).as_deref(), Some("<S-Tab>"));
        assert_eq!(format_key_event(Key::Left, CTRL).as_deref(), Some("<C-Left>"));
        assert_eq!(format_key_event(Key::space, CTRL).as_deref(), Some("<C-Space>"));
        assert_eq!(format_key_event(Key::F1, ALT).as_deref(), Some("<M-F1>"));
    }

    #[test]
    fn shift_is_only_emitted_for_special_keys_or_ctrl_plus_ascii_letter() {
        // Ctrl + shifted ASCII letter: Shift is meaningful (`<C-S-A>` differs from `<C-a>`),
        // and the letter is upper-cased the way Neovim normalizes it internally.
        assert_eq!(format_key_event(Key::A, SHIFT | CTRL).as_deref(), Some("<S-C-A>"));
        // A shifted *non*-letter: the character itself already carries the Shift, so adding `S-`
        // would name a different chord than the user actually pressed.
        assert_eq!(format_key_event(Key::dollar, SHIFT | ALT).as_deref(), Some("<M-$>"));
        // Special key: Shift always included.
        assert_eq!(format_key_event(Key::End, SHIFT).as_deref(), Some("<S-End>"));
    }

    #[test]
    fn less_than_becomes_lt_without_gaining_a_spurious_shift() {
        // A literal `<` would open a key name, so it is force-bracketed as `<lt>` -- but only
        // after the modifier string is computed, so Shift (which is what produces `<` on a US
        // layout) does not turn it into `<S-lt>`.
        assert_eq!(format_key_event(Key::less, SHIFT).as_deref(), Some("<lt>"));
        assert_eq!(format_key_event(Key::less, SHIFT | CTRL).as_deref(), Some("<C-lt>"));
    }

    #[test]
    fn unnameable_keys_are_dropped_rather_than_guessed_at() {
        // Bare modifier presses must produce nothing -- forwarding them would inject junk into
        // the buffer on every chord, and (per the sandbox findings recorded in
        // docs/canonical/neovibe_feasibility_status.md) a lone Shift is a real fcitx5
        // input-method-switch gesture that must stay unconsumed.
        assert_eq!(format_key_event(Key::Control_L, ModifierType::empty()), None);
        assert_eq!(format_key_event(Key::Shift_L, ModifierType::empty()), None);
        assert_eq!(format_key_event(Key::Alt_L, ModifierType::empty()), None);
        assert_eq!(format_key_event(Key::Super_L, ModifierType::empty()), None);
        // A key this crate has no honest Neovim name for.
        assert_eq!(format_key_event(Key::AudioPlay, ModifierType::empty()), None);
    }

    #[test]
    fn committed_text_escapes_every_angle_bracket() {
        // Regression test for the real bug found in the sandbox on 2026-09-08: with an IM context
        // attached, ordinary printable keys are consumed by GTK's IM and arrive via ::commit, so
        // a typed `<` never passes through `format_key_text`'s own `lt` branch. Typing `ab<cd`
        // produced `abcd` in a real nvim buffer -- nvim read `<cd` as an unterminated key name.
        assert_eq!(escape_for_nvim_input("<"), "<lt>");
        assert_eq!(escape_for_nvim_input("ab<cd"), "ab<lt>cd");
        // Every occurrence, not just the first -- an IME commit is arbitrary text.
        assert_eq!(escape_for_nvim_input("<<"), "<lt><lt>");
        // Text with nothing to escape is passed through unchanged.
        assert_eq!(escape_for_nvim_input("你好"), "你好");
        assert_eq!(escape_for_nvim_input("hello"), "hello");
        assert_eq!(escape_for_nvim_input(""), "");
    }

    /// A stand-in for the live session: records what reached "nvim" and in what order relative to
    /// the activity callback, and can refuse text the way an exited nvim does.
    struct FakeTarget {
        accepts: bool,
        sent: Vec<String>,
        log: Rc<RefCell<Vec<String>>>,
    }

    impl FakeTarget {
        fn new(accepts: bool, log: &Rc<RefCell<Vec<String>>>) -> RefCell<Self> {
            RefCell::new(Self {
                accepts,
                sent: Vec::new(),
                log: log.clone(),
            })
        }
    }

    impl TextTarget for FakeTarget {
        fn accepts_text(&self) -> bool {
            self.accepts
        }

        fn send_text(&mut self, text: &str) {
            self.sent.push(text.to_string());
            self.log.borrow_mut().push(format!("sent {text}"));
        }
    }

    fn activity_logging(log: &Rc<RefCell<Vec<String>>>) -> KeyActivity {
        let activity = KeyActivity::default();
        let log = log.clone();
        activity.set(move || log.borrow_mut().push("activity".to_string()));
        activity
    }

    #[test]
    fn a_key_sent_to_nvim_reports_activity_after_the_text_went() {
        let log = Rc::new(RefCell::new(Vec::new()));
        let target = FakeTarget::new(true, &log);
        let activity = activity_logging(&log);
        assert!(send_text(&target, "j", &activity));
        assert!(send_text(&target, "<C-w>", &activity));
        assert_eq!(target.borrow().sent, vec!["j", "<C-w>"]);
        assert_eq!(
            *log.borrow(),
            vec!["sent j", "activity", "sent <C-w>", "activity"],
            "one report per key, after its text was handed over"
        );
    }

    #[test]
    fn text_an_exited_or_missing_nvim_cannot_take_is_not_activity() {
        let log = Rc::new(RefCell::new(Vec::new()));
        let target = FakeTarget::new(false, &log);
        let activity = activity_logging(&log);
        assert!(!send_text(&target, "j", &activity));
        assert!(target.borrow().sent.is_empty());
        assert!(log.borrow().is_empty(), "nothing was typed into a live editor");
    }

    #[test]
    fn a_host_handler_can_call_back_into_the_target_and_replace_itself() {
        // The handler runs with neither the target's borrow nor the callback slot's held.
        let log = Rc::new(RefCell::new(Vec::new()));
        let target = Rc::new(FakeTarget::new(true, &log));
        let activity = KeyActivity::default();
        {
            let target = target.clone();
            let again = activity.clone();
            let log = log.clone();
            activity.set(move || {
                assert!(
                    target.try_borrow_mut().is_ok(),
                    "the target is free while the host is told"
                );
                log.borrow_mut().push("first".to_string());
                let log = log.clone();
                again.set(move || log.borrow_mut().push("second".to_string()));
            });
        }
        send_text(&target, "a", &activity);
        send_text(&target, "b", &activity);
        assert_eq!(*log.borrow(), vec!["sent a", "first", "sent b", "second"]);
    }

    #[test]
    fn with_no_callback_a_key_is_just_sent() {
        let log = Rc::new(RefCell::new(Vec::new()));
        let target = FakeTarget::new(true, &log);
        assert!(send_text(&target, "x", &KeyActivity::default()));
        assert_eq!(target.borrow().sent, vec!["x"]);
    }

    #[test]
    fn a_later_callback_replaces_an_earlier_one() {
        let log = Rc::new(RefCell::new(Vec::new()));
        let activity = KeyActivity::default();
        for name in ["one", "two"] {
            let log = log.clone();
            activity.set(move || log.borrow_mut().push(name.to_string()));
        }
        activity.notify();
        assert_eq!(*log.borrow(), vec!["two"]);
    }

    #[test]
    fn keyboard_text_reaches_nvim_only_through_send_text() {
        // Both keyboard handlers (an input method's commit and a key press) call `send_text`; this
        // file may name `.send_text_input(` in code exactly once, inside `TextTarget for LiveState`.
        // A third site would type into the editor without telling the host, and the panel would
        // go on streaming at full rate under the user's fingers.
        let source = include_str!("keyboard.rs");
        let code = source
            .split("#[cfg(test)]")
            .next()
            .expect("split always yields one part");
        let sites = code
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .filter(|line| line.contains(".send_text_input("))
            .count();
        assert_eq!(sites, 1, "keyboard.rs sends text to nvim outside `send_text`");
        assert!(
            code.matches("send_text(&live_state, &text, &activity)").count() == 2,
            "the commit path and the key-press path both go through send_text"
        );
    }

    #[test]
    fn im_cursor_rect_is_widget_local_logical_pixels_of_one_grid_cell() {
        let grid_scale = GridScale::new(PixelSize::new(9.0, 18.0));

        // scale_factor 1: device pixels == logical pixels, and the position is used as-is
        // because `cursor_pixel_position()` already includes the content_region origin.
        let rect = im_cursor_rect_for_editor(PixelPos::new(130.0, 76.0), grid_scale, 1);
        assert_eq!((rect.x(), rect.y()), (130, 76));
        assert_eq!((rect.width(), rect.height()), (9, 18));

        // scale_factor 2 (HiDPI): the exact inverse of `pixel_to_grid_pos`'s own
        // `logical * scale_factor` conversion.
        let rect = im_cursor_rect_for_editor(PixelPos::new(260.0, 152.0), grid_scale, 2);
        assert_eq!((rect.x(), rect.y()), (130, 76));
        assert_eq!((rect.width(), rect.height()), (5, 9));

        // A sub-pixel cell still reports a usable, non-zero size.
        let tiny = GridScale::new(PixelSize::new(0.4, 0.4));
        let rect = im_cursor_rect_for_editor(PixelPos::new(0.0, 0.0), tiny, 1);
        assert_eq!((rect.width(), rect.height()), (1, 1));
    }
}
