//! `Ctrl` + mouse wheel over a pane: the text size of that pane only (keymap spec §2.7). The
//! browser rule -- the pane under the pointer, not the one with the keys -- which is why this beat a
//! prefix key for per-pane size (`Ctrl+a =`/`-`/`0` left the defaults). Up is larger; one notch is
//! one 0.1 step of the pure model in `text_size`.
//!
//! A capture-phase scroll controller on each module host, claiming the event only while Ctrl is
//! held: it runs before `neovide-editor`'s own scroll controller (on the `GLArea` inside the host)
//! and before WebKit, so neither sees a Ctrl+scroll. The cost: nvim no longer receives
//! `<C-ScrollWheelUp>`.
//!
//! **The unit matters** (keymap GUI pass, 2026-09-25): a touchpad arrives as `ScrollUnit::Surface`,
//! in pixels, and counting a pixel as a notch stepped on nearly every event of a slow two-finger
//! scroll. `notches` converts at `SURFACE_PX_PER_NOTCH`. Seen in a headless sway with a simulated
//! wheel and finger source; the feel on real hardware is still owed.

use std::cell::RefCell;
use std::rc::Rc;

use eitri_core::layout::ModuleId;
use gtk4::gdk::{ModifierType, ScrollUnit};
use gtk4::glib;
use gtk4::prelude::*;

use crate::text_size::{self, TextSizeController, TextSizeTarget, TextStep};

/// The part of a scroll gesture not yet spent on a step.
#[derive(Debug, Default)]
pub(crate) struct WheelZoom {
    acc: f64,
}

impl WheelZoom {
    /// One scroll event's `dy` (negative is up). Returns the steps it makes: positive is larger.
    /// Whole units are spent; the fraction is kept for the next event, and dropped when the
    /// direction reverses (ruling 12).
    pub(crate) fn scroll(&mut self, dy: f64) -> i32 {
        if dy == 0.0 || !dy.is_finite() {
            return 0;
        }
        if self.acc != 0.0 && self.acc.signum() != dy.signum() {
            self.acc = 0.0;
        }
        self.acc += dy;
        // A hair of slack, so ten tenths of a notch (pixels divided into notches) make one.
        let whole = (self.acc + self.acc.signum() * 1e-9).trunc();
        self.acc -= whole;
        -(whole as i32)
    }

    /// One scroll event as GTK reports it: `dy` in `unit`. Returns the steps it makes.
    pub(crate) fn scroll_in(&mut self, unit: ScrollUnit, dy: f64) -> i32 {
        self.scroll(notches(unit, dy))
    }

    /// A scroll without Ctrl ends the gesture.
    pub(crate) fn reset(&mut self) {
        self.acc = 0.0;
    }
}

/// Surface pixels per wheel notch. wlroots hands a client libinput's own scroll value, and
/// libinput reports a wheel click as 15 (degrees); a touchpad's value is in the same units. So a
/// clickless scroll steps as often as the compositor would have counted a click.
pub(crate) const SURFACE_PX_PER_NOTCH: f64 = 15.0;

/// `dy` in wheel notches. GTK reports a scroll with a wheel click behind it (`value120`) as
/// `ScrollUnit::Wheel`, in notches, fractions for a high-resolution wheel; everything else -- a
/// touchpad, a wheel whose clicks the compositor did not forward -- as `ScrollUnit::Surface`, in
/// surface pixels.
pub(crate) fn notches(unit: ScrollUnit, dy: f64) -> f64 {
    match unit {
        ScrollUnit::Surface => dy / SURFACE_PX_PER_NOTCH,
        _ => dy,
    }
}

/// `n` steps as the model's steps: larger for a positive `n`, smaller for a negative one.
pub(crate) fn steps(n: i32) -> Vec<TextStep> {
    let step = if n > 0 { TextStep::Larger } else { TextStep::Smaller };
    std::iter::repeat_n(step, n.unsigned_abs() as usize).collect()
}

/// Installs the controller on `host`, the module `id`'s host widget.
pub(crate) fn install(id: ModuleId, host: &gtk4::Widget, text_size: Rc<TextSizeController>) {
    let target = text_size::route_focused_module(Some(&id));
    let wheel = RefCell::new(WheelZoom::default());
    let scroll = gtk4::EventControllerScroll::new(gtk4::EventControllerScrollFlags::VERTICAL);
    scroll.set_propagation_phase(gtk4::PropagationPhase::Capture);
    scroll.connect_scroll(move |controller, _dx, dy| {
        if !controller.current_event_state().contains(ModifierType::CONTROL_MASK) {
            wheel.borrow_mut().reset();
            return glib::Propagation::Proceed;
        }
        let n = wheel.borrow_mut().scroll_in(controller.unit(), dy);
        for step in steps(n) {
            match target {
                TextSizeTarget::Editor => text_size.apply_editor(step),
                TextSizeTarget::Panel => text_size.apply_panel(step),
                TextSizeTarget::Neither => {}
            }
        }
        if n != 0 {
            println!("[text-size] Ctrl+wheel over {id}: {n:+} step(s)");
        }
        glib::Propagation::Stop
    });
    host.add_controller(scroll);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_notch_is_one_step_and_up_is_larger() {
        let mut wheel = WheelZoom::default();
        assert_eq!(wheel.scroll(-1.0), 1);
        assert_eq!(wheel.scroll(1.0), -1);
        assert_eq!(wheel.scroll(-2.5), 2, "two whole notches, half a notch kept");
        assert_eq!(wheel.scroll(-0.5), 1, "the kept half completes a third");
        assert_eq!(wheel.scroll(0.0), 0);
        assert_eq!(wheel.scroll(f64::NAN), 0);
    }

    /// Review Focus 5: a touchpad sends many small `dy`s; one step per whole unit, and a reversal
    /// mid-gesture does not spend the remainder the other way.
    #[test]
    fn smooth_scroll_steps_once_per_whole_unit_and_a_reversal_clears_the_remainder() {
        let mut wheel = WheelZoom::default();
        let steps: Vec<i32> = (0..8).map(|_| wheel.scroll(-0.25)).collect();
        assert_eq!(steps, [0, 0, 0, 1, 0, 0, 0, 1]);
        assert_eq!(wheel.scroll(-0.75), 0);
        assert_eq!(wheel.scroll(0.5), 0, "reversed: the -0.75 is dropped, not netted");
        assert_eq!(wheel.scroll(0.5), -1);
        wheel.scroll(-0.9);
        wheel.reset();
        assert_eq!(wheel.scroll(-0.2), 0, "a reset forgets the remainder");
    }

    /// The keymap GUI pass (2026-09-25): GTK reports a touchpad's scroll -- and any scroll without
    /// a wheel click behind it -- as `ScrollUnit::Surface`, in surface pixels. Counting a pixel as a
    /// notch took the chat from 1.0 to 2.3 on ten 1.5px events, a slow two-finger scroll 15px
    /// long. Fifteen pixels are one notch, as the compositor itself counts a wheel click.
    #[test]
    fn a_surface_scroll_counts_pixels_not_notches() {
        let mut wheel = WheelZoom::default();
        let steps: i32 = (0..10).map(|_| wheel.scroll_in(ScrollUnit::Surface, -1.5)).sum();
        assert_eq!(steps, 1, "a slow 15px two-finger scroll is one step, not one per event");
        let mut wheel = WheelZoom::default();
        assert_eq!(wheel.scroll_in(ScrollUnit::Surface, 15.0), -1, "a clickless 15px notch");
        let mut wheel = WheelZoom::default();
        assert_eq!(
            wheel.scroll_in(ScrollUnit::Wheel, -1.0),
            1,
            "a wheel click is still one step"
        );
        assert_eq!(
            wheel.scroll_in(ScrollUnit::Wheel, -0.25),
            0,
            "a high-resolution quarter click"
        );
    }

    #[test]
    fn steps_are_that_many_larger_or_smaller() {
        assert_eq!(steps(2), [TextStep::Larger, TextStep::Larger]);
        assert_eq!(steps(-1), [TextStep::Smaller]);
        assert!(steps(0).is_empty());
    }

    /// The pane under the pointer, by its module: the editor and the chat have a text size; the
    /// terminal (until its phase 4) and a Lua panel take the scroll and do nothing.
    #[test]
    fn each_module_routes_to_its_own_pane_or_to_nothing() {
        assert_eq!(
            text_size::route_focused_module(Some(&ModuleId::editor())),
            TextSizeTarget::Editor
        );
        assert_eq!(
            text_size::route_focused_module(Some(&ModuleId::agent())),
            TextSizeTarget::Panel
        );
        assert_eq!(
            text_size::route_focused_module(Some(&ModuleId::terminal())),
            TextSizeTarget::Neither
        );
        assert_eq!(
            text_size::route_focused_module(Some(&ModuleId::lua("notes"))),
            TextSizeTarget::Neither
        );
    }
}
