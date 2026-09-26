//! Mouse-input handling for the embedded editor surface.
//! Coordinate conversions (`current_content_region`, `pixel_to_grid_pos`) live in `crate::gl_interop`.
//! Shared session state is defined by `LiveState` and `LiveSession` in `crate`.

use std::cell::RefCell;
use std::rc::Rc;

use gtk4::gdk::{ModifierType, ScrollUnit};
use gtk4::glib;
use gtk4::prelude::*;
use gtk4::{EventControllerMotion, EventControllerScroll, GLArea, GestureClick};

use crate::gl_interop::{current_content_region, pixel_to_grid_pos};
use crate::LiveState;

/// nvim's own button-text notation for a GDK button number, mirroring the reference
/// `neovide::window::mouse_manager::mouse_button_to_button_text`'s winit-`MouseButton`-keyed
/// equivalent. GDK numbers buttons in the X11 convention: 1=left/primary, 2=middle, 3=right/
/// secondary, 8=back, 9=forward. Anything else (e.g. an extra side button some mice report
/// differently) is not forwarded -- same "silently drop, don't guess" stance the reference takes
/// for a `MouseButton` variant its own match doesn't cover.
pub(crate) fn gdk_button_to_button_text(button: u32) -> Option<&'static str> {
    match button {
        1 => Some("left"),
        2 => Some("middle"),
        3 => Some("right"),
        8 => Some("x1"),
        9 => Some("x2"),
        _ => None,
    }
}

/// nvim's own modifier-prefix notation (`"S-"`/`"C-"`/`"M-"`/`"D-"`, concatenated in that order)
/// for a GDK modifier snapshot -- mirrors
/// `neovide::window::keyboard_manager::KeyboardManager::format_modifier_string` called with
/// `is_special = true` the way every mouse RPC in the reference `MouseManager` calls it (mouse
/// events, like special keys, always include Shift when held rather than only when combined with
/// Ctrl+ASCII -- see that method's own doc for why). Unlike the reference, which reads a
/// `winit::keyboard::ModifiersState` this crate never accumulates (see the keyboard controller's
/// own module-doc caveat on modifier fidelity), this reads GTK's own live `ModifierType` for
/// whichever event is currently being handled
/// (`EventControllerExt::current_event_state`) -- sufficient for a mouse command, which is always
/// built and sent synchronously from inside the one GTK signal callback that observed it.
pub(crate) fn format_modifier_string(state: ModifierType) -> String {
    let mut modifiers = String::new();
    if state.contains(ModifierType::SHIFT_MASK) {
        modifiers.push_str("S-");
    }
    if state.contains(ModifierType::CONTROL_MASK) {
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

/// Mouse button + last grid cell an ongoing drag was last sent at -- tracked by `LiveSession::
/// active_drag` while a button is held, mirroring `MouseManager::drag_details`. Its presence is
/// what tells the `EventControllerMotion` handler below whether a `Drag` RPC should be sent at
/// all (only while some button is down); its `last_grid_pos` is what lets that handler dedupe on
/// the grid cell actually changing, the same `has_moved` check
/// `MouseManager::handle_pointer_motion` does internally -- see `LiveHarness::send_mouse_drag`'s
/// own doc for why that dedup has to live in the caller here rather than the harness.
#[derive(Clone, Copy)]
pub(crate) struct DragState {
    button: &'static str,
    last_grid_pos: (u32, u32),
}

/// Shared body for `GestureClick`'s `pressed`/`released` handlers -- see the controller wiring in
/// `poc/neovide_embed_live`'s `build_ui` for how each is attached there (this crate's own
/// equivalent wiring is in `NeovideEditorPane::new`, `lib.rs`). `pressed` selects which one this
/// call is for.
///
/// On press: forwards a `MouseButton{action: "press"}` via `LiveHarness::send_mouse_button`, then
/// arms `session.active_drag` so the motion handler below starts sending `Drag` RPCs while this
/// button stays held (mirroring `MouseManager::send_nvim_mouse_button`'s own `self.drag_details =
/// Some(..)` on press). On release: forwards `MouseButton{action: "release"}` at whichever grid
/// cell the drag was last actually at (`active_drag`'s own `last_grid_pos`, if a drag happened) --
/// matching the reference's own `if !down && self.has_moved { self.grid_position } else {
/// self.get_relative_position(..) }` choice of position for a release after a drag -- then
/// disarms `active_drag` regardless.
///
/// `gesture.current_button()` (not a `button` signal argument -- neither `pressed` nor `released`
/// carries one) is nvim's own button-text notation source; a button this crate doesn't recognize
/// (`gdk_button_to_button_text` returning `None`) is silently ignored, same as the reference.
/// `x`/`y` are whatever the firing signal itself handed the caller (both `pressed` and `released`
/// report the pointer position at that instant).
pub(crate) fn handle_mouse_button(
    live_state: &Rc<RefCell<LiveState>>,
    gl_area: &GLArea,
    gesture: &GestureClick,
    x: f64,
    y: f64,
    pressed: bool,
) {
    // Clicking the editor must also *focus* it. GTK4 does not focus a plain widget on click the
    // way it does a button (there is no focus-on-click behavior for a bare `GtkGLArea`), and
    // pointer events are delivered by position regardless of who holds the keyboard focus -- so
    // without this, a click here moves the Neovim cursor while the keyboard keeps going somewhere
    // else entirely.
    //
    // **This closes a real, reproduced bug, not a hypothetical one.** It was invisible for as long
    // as the editor was the only focusable widget in its window, which was true of every crate
    // this code had been verified in before now. With `shell`'s agent panel now a real, focusable
    // `WebView`, the 2026-09-08 sandbox pass caught it directly: click the composer, type, click
    // back on the editor, type -- and the second batch of text went into the composer too, with
    // the editor's own key controller seeing zero events. Done before the button lookup so that a
    // click with a button this crate doesn't forward still restores focus.
    if pressed && !gl_area.has_focus() {
        gl_area.grab_focus();
    }

    let Some(button) = gdk_button_to_button_text(gesture.current_button()) else {
        return;
    };
    let modifier_string = format_modifier_string(gesture.current_event_state());

    let mut live = live_state.borrow_mut();
    let LiveState::Ready(session) = &mut *live else {
        return;
    };
    if session.harness.has_neovim_exited() {
        return;
    }

    let content_region = current_content_region(session.fb_size.get(), session.harness.grid_scale());
    let position_from_event = pixel_to_grid_pos(
        x,
        y,
        gl_area.scale_factor(),
        &content_region,
        session.harness.grid_scale(),
        session.harness.get_grid_size(),
    );
    let grid_pos = if !pressed {
        // A release after a drag: send it at the drag's own last-known cell, not wherever the
        // pointer happens to sit right now relative to a freshly recomputed content_region --
        // mirrors the reference's `self.has_moved -> self.grid_position` branch exactly.
        match session.active_drag.get() {
            Some(drag) if drag.button == button => drag.last_grid_pos,
            _ => position_from_event,
        }
    } else {
        position_from_event
    };

    session
        .harness
        .send_mouse_button(button, pressed, grid_pos, &modifier_string);
    session.active_drag.set(if pressed {
        Some(DragState {
            button,
            last_grid_pos: grid_pos,
        })
    } else {
        None
    });
    session.wants_frame.set(true);
}

/// `EventControllerMotion`'s `motion` handler -- sends a `Drag` RPC via
/// `LiveHarness::send_mouse_drag` only while `session.active_drag` is armed (some button held,
/// set by `handle_mouse_button` above) and only when the computed grid cell actually differs from
/// `active_drag`'s own `last_grid_pos`, mirroring `MouseManager::handle_pointer_motion`'s combined
/// `drag_details.is_some()` + `has_moved` gate. A plain hover-move with no button held is not
/// forwarded at all -- the reference only does that when `WindowSettings::mouse_move_event` is
/// explicitly enabled (default off), which this crate has no equivalent setting for.
pub(crate) fn handle_mouse_motion(
    live_state: &Rc<RefCell<LiveState>>,
    gl_area: &GLArea,
    controller: &EventControllerMotion,
    x: f64,
    y: f64,
) {
    let mut live = live_state.borrow_mut();
    let LiveState::Ready(session) = &mut *live else {
        return;
    };
    // Recorded on every motion/enter regardless of drag state -- this is the only source of
    // pointer position a subsequent scroll event (which carries none of its own) has. See
    // `LiveSession::last_pointer_pos`'s own doc.
    session.last_pointer_pos.set((x, y));

    let Some(drag) = session.active_drag.get() else {
        return;
    };
    if session.harness.has_neovim_exited() {
        return;
    }

    let content_region = current_content_region(session.fb_size.get(), session.harness.grid_scale());
    let grid_pos = pixel_to_grid_pos(
        x,
        y,
        gl_area.scale_factor(),
        &content_region,
        session.harness.grid_scale(),
        session.harness.get_grid_size(),
    );
    if grid_pos == drag.last_grid_pos {
        return;
    }

    let modifier_string = format_modifier_string(controller.current_event_state());
    session.harness.send_mouse_drag(drag.button, grid_pos, &modifier_string);
    session.active_drag.set(Some(DragState {
        button: drag.button,
        last_grid_pos: grid_pos,
    }));
    session.wants_frame.set(true);
}

/// Handles `EventControllerScroll` deltas using the same accumulation as
/// `MouseManager::handle_line_scroll` and `handle_pixel_scroll`. `controller.unit()`
/// distinguishes wheel-notch units (`Wheel`) from device pixels (`Surface`). Surface
/// deltas are divided by `grid_scale` before being added to `session.scroll_position`.
/// Changes in the floored position determine how many whole-line `Scroll` RPCs to send.
///
/// Positive GDK deltas map to nvim's `"down"` and `"right"`. Synthetic virtual-pointer
/// scrolls with `axis_source(Wheel)` moved the viewport in both directions and back.
/// Physical wheel direction remains unverified; an inverted direction would require
/// swapping the `Greater` and `Less` arms below.
///
/// In the sandbox, `wlrctl pointer scroll` sends `axis` and `frame` without `axis_source`.
/// The GDK/Wayland backend drops those events before this handler is called.
pub(crate) fn handle_mouse_scroll(
    live_state: &Rc<RefCell<LiveState>>,
    gl_area: &GLArea,
    controller: &EventControllerScroll,
    dx: f64,
    dy: f64,
) -> glib::Propagation {
    let mut live = live_state.borrow_mut();
    let LiveState::Ready(session) = &mut *live else {
        return glib::Propagation::Proceed;
    };
    if session.harness.has_neovim_exited() {
        return glib::Propagation::Proceed;
    }

    // In the sandbox, synthetic virtual-pointer scrolls with `axis_source=Wheel` arrive as
    // `ScrollUnit::Surface`. A simulated notch of about 10 pixels becomes less than half a
    // grid line, so several notches may be needed for one `Scroll` RPC. Scrolling still moves
    // the viewport in both directions. Surface deltas use pixel units and must be divided
    // by `grid_scale`, matching `handle_pixel_scroll`. Physical wheel classification remains
    // unverified, so the synthetic result alone does not justify changing this conversion.
    let (mut amount_x, mut amount_y) = (dx as f32, dy as f32);
    if controller.unit() == ScrollUnit::Surface {
        let grid_scale = session.harness.grid_scale();
        amount_x /= grid_scale.width();
        amount_y /= grid_scale.height();
    }

    let content_region = current_content_region(session.fb_size.get(), session.harness.grid_scale());
    // `EventControllerScroll` (unlike `GestureClick`/`EventControllerMotion`) never reports the
    // pointer's own x/y at all -- only deltas -- so the grid cell a scroll targets comes from
    // `last_pointer_pos`, the most recent position `EventControllerMotion` observed. See
    // `LiveSession::last_pointer_pos`'s own doc for why that mirrors the reference exactly rather
    // than being a workaround.
    let (px, py) = session.last_pointer_pos.get();
    let grid_pos = pixel_to_grid_pos(
        px,
        py,
        gl_area.scale_factor(),
        &content_region,
        session.harness.grid_scale(),
        session.harness.get_grid_size(),
    );

    let (prev_x, prev_y) = session.scroll_position.get();
    let (new_x, new_y) = (prev_x + amount_x, prev_y + amount_y);
    session.scroll_position.set((new_x, new_y));

    let modifier_string = format_modifier_string(controller.current_event_state());

    let (prev_floor_y, new_floor_y) = (prev_y.floor() as i64, new_y.floor() as i64);
    let vertical_direction = match new_floor_y.cmp(&prev_floor_y) {
        std::cmp::Ordering::Greater => Some("down"),
        std::cmp::Ordering::Less => Some("up"),
        std::cmp::Ordering::Equal => None,
    };
    if let Some(direction) = vertical_direction {
        for _ in 0..(new_floor_y - prev_floor_y).abs() {
            session.harness.send_mouse_scroll(direction, grid_pos, &modifier_string);
        }
    }

    let (prev_floor_x, new_floor_x) = (prev_x.floor() as i64, new_x.floor() as i64);
    let horizontal_direction = match new_floor_x.cmp(&prev_floor_x) {
        std::cmp::Ordering::Greater => Some("right"),
        std::cmp::Ordering::Less => Some("left"),
        std::cmp::Ordering::Equal => None,
    };
    if let Some(direction) = horizontal_direction {
        for _ in 0..(new_floor_x - prev_floor_x).abs() {
            session.harness.send_mouse_scroll(direction, grid_pos, &modifier_string);
        }
    }

    if vertical_direction.is_some() || horizontal_direction.is_some() {
        session.wants_frame.set(true);
    }

    glib::Propagation::Stop
}

/// Checks coordinate math and modifier formatting in pure helpers without a GTK/GLib runtime.
/// These tests catch inverted clamps and missing modifier bits. Real click, drag and scroll
/// signals in a Wayland session still need end-to-end verification.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gdk_button_mapping_matches_x11_convention() {
        assert_eq!(gdk_button_to_button_text(1), Some("left"));
        assert_eq!(gdk_button_to_button_text(2), Some("middle"));
        assert_eq!(gdk_button_to_button_text(3), Some("right"));
        assert_eq!(gdk_button_to_button_text(8), Some("x1"));
        assert_eq!(gdk_button_to_button_text(9), Some("x2"));
        // A button code this crate doesn't recognize is silently dropped, not guessed at.
        assert_eq!(gdk_button_to_button_text(4), None);
        assert_eq!(gdk_button_to_button_text(0), None);
    }

    #[test]
    fn modifier_string_orders_shift_control_alt_super() {
        assert_eq!(format_modifier_string(ModifierType::empty()), "");
        assert_eq!(format_modifier_string(ModifierType::CONTROL_MASK), "C-");
        assert_eq!(
            format_modifier_string(ModifierType::SHIFT_MASK | ModifierType::CONTROL_MASK),
            "S-C-"
        );
        assert_eq!(
            format_modifier_string(
                ModifierType::SHIFT_MASK
                    | ModifierType::CONTROL_MASK
                    | ModifierType::ALT_MASK
                    | ModifierType::SUPER_MASK
            ),
            "S-C-M-D-"
        );
    }
}
