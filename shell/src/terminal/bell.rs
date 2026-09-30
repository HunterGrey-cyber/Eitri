//! The bell, as a flash of the pane (bottom-terminal phase 2; spec §6, "Bell flashes the pane").
//!
//! A program rings the bell with `BEL` (`\a`): a finished `make; tput bel`, zsh at the start of an
//! empty line, `less` at the end of a file. The pane tints itself with the theme's foreground for
//! [`FLASH`], and makes no sound. His foot is at its bell defaults (`~/.config/foot/foot.ini` names
//! no `[bell]`), which ring the desktop's system bell instead; the flash is the spec's choice, listed
//! for him to veto.
//!
//! **A flood must not strobe.** A `cat` of a binary file rings on most reads. The session already
//! folds bells to at most one per rendered frame (`eitri-terminal`'s
//! `a_bell_flood_is_coalesced_to_about_one_host_wake_per_frame`); here a new flash can start only
//! [`MIN_INTERVAL`] after the last one started, so a flood is at most two flashes a second.
//!
//! Pure, and tested without a display: the pane (`pane.rs`) asks, repaints, and paints [`tint`].

use std::time::{Duration, Instant};

use skia_safe::{Canvas, Color, Paint};
use terminal_render::RgbColor;

/// How long one flash shows.
pub(crate) const FLASH: Duration = Duration::from_millis(150);
/// The least time between the starts of two flashes.
pub(crate) const MIN_INTERVAL: Duration = Duration::from_millis(500);
/// How strongly a flash tints the pane with the theme's foreground: 0x33 of 0xff, a fifth.
pub(crate) const FLASH_ALPHA: u8 = 0x33;

#[derive(Debug, Default)]
pub(crate) struct BellFlash {
    started: Option<Instant>,
}

impl BellFlash {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// A bell rang at `now`. `Some(until)`: a flash starts, and the pane repaints at `until` to end
    /// it. `None`: absorbed, by the flash still showing or because the last one started less than
    /// [`MIN_INTERVAL`] ago.
    pub(crate) fn ring(&mut self, now: Instant) -> Option<Instant> {
        if self
            .started
            .is_some_and(|started| now.saturating_duration_since(started) < MIN_INTERVAL)
        {
            return None;
        }
        self.started = Some(now);
        Some(now + FLASH)
    }

    /// Whether a frame painted at `now` shows the flash.
    pub(crate) fn active(&self, now: Instant) -> bool {
        self.started
            .is_some_and(|started| now.saturating_duration_since(started) < FLASH)
    }
}

/// Paints the flash over whatever the canvas holds: the theme's foreground at [`FLASH_ALPHA`], over
/// the whole pane. A tint, not a cover: the screen stays readable through it.
pub(crate) fn tint(canvas: &Canvas, foreground: RgbColor) {
    let mut paint = Paint::default();
    paint.set_color(Color::from_argb(FLASH_ALPHA, foreground.r, foreground.g, foreground.b));
    canvas.draw_paint(&paint);
}

/// How long to arm the flash-end timer for, given the instant the flash should end and the instant
/// this is computed at (review 2026-09-24, task 5): never less than `until - now`, rounded *up* to a
/// whole millisecond.
///
/// glib's timer functions take a millisecond count and truncate a `Duration` down to it
/// (`glib::timeout_add_local_once` -> `timeout_add_local` -> `interval.as_millis()`, which discards
/// the fractional millisecond). `until - now` is almost never a whole number of milliseconds, so
/// truncating fires the timer up to 1 ms *before* `until` -- while [`BellFlash::active`] is still
/// `true` -- and nothing else repaints the pane afterwards, so the tint sticks until some unrelated
/// event (a keystroke, output, focus change, resize) forces a redraw. Rounding up instead means the
/// timer never fires before `until`.
pub(crate) fn repaint_delay(until: Instant, now: Instant) -> Duration {
    let wait = until.saturating_duration_since(now);
    Duration::from_millis(wait.as_micros().div_ceil(1000) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use skia_safe::{surfaces, ISize};

    #[test]
    fn a_bell_flashes_for_a_moment() {
        let mut bell = BellFlash::new();
        let t0 = Instant::now();
        assert!(!bell.active(t0), "no bell, no flash");
        assert_eq!(bell.ring(t0), Some(t0 + FLASH));
        assert!(bell.active(t0));
        assert!(bell.active(t0 + FLASH - Duration::from_millis(1)));
        assert!(!bell.active(t0 + FLASH), "over when the repaint comes");
    }

    /// A bell on every frame for a second -- what the session hands the pane for a `cat` of a binary
    /// file -- is two flashes, not sixty.
    #[test]
    fn a_flood_of_bells_flashes_at_most_twice_a_second() {
        let mut bell = BellFlash::new();
        let t0 = Instant::now();
        let flashes = (0..1000u64)
            .step_by(16)
            .filter(|ms| bell.ring(t0 + Duration::from_millis(*ms)).is_some())
            .count();
        assert_eq!(flashes, 2);
    }

    #[test]
    fn a_bell_well_after_the_last_one_flashes_again() {
        let mut bell = BellFlash::new();
        let t0 = Instant::now();
        assert!(bell.ring(t0).is_some());
        assert_eq!(bell.ring(t0 + Duration::from_millis(100)), None, "still flashing");
        let later = t0 + Duration::from_millis(600);
        assert_eq!(bell.ring(later), Some(later + FLASH));
    }

    /// Painted on a CPU raster: over white, a black tint at a fifth leaves 204 (255 x 0.8, give or
    /// take Skia's rounding) -- the text under it still shows, where a cover would leave 0.
    #[test]
    fn the_flash_tints_the_pane_rather_than_covering_it() {
        let mut surface = surfaces::raster_n32_premul(ISize::new(4, 4)).expect("a raster surface");
        surface.canvas().clear(Color::WHITE);
        tint(surface.canvas(), RgbColor::new(0, 0, 0));
        let info = surface.image_info();
        let mut pixels = vec![0u8; info.min_row_bytes() * 4];
        assert!(surface.read_pixels(&info, &mut pixels, info.min_row_bytes(), (0, 0)));
        for channel in &pixels[0..3] {
            assert!((203..=205).contains(channel), "{channel}: not a fifth-strong tint");
        }
    }

    /// The exact case that broke the flash-end timer (review 2026-09-24): `until - now` is 149.95 ms,
    /// which `Duration::as_millis()` truncates to 149 -- firing the timer 50 us before `until`, while
    /// the flash was still active. Rounding up must give 150, not 149.
    #[test]
    fn a_near_millisecond_remainder_rounds_the_delay_up_not_down() {
        let now = Instant::now();
        let until = now + Duration::from_micros(149_950);
        assert_eq!(repaint_delay(until, now), Duration::from_millis(150));
    }

    /// The general property the fix exists for: the returned delay never fires before `until`.
    #[test]
    fn the_repaint_delay_never_undershoots_until() {
        let now = Instant::now();
        for micros in [0, 1, 500, 999, 1_000, 1_001, 149_999, 150_000, 500_000] {
            let until = now + Duration::from_micros(micros);
            let delay = repaint_delay(until, now);
            assert!(
                now + delay >= until,
                "delay {delay:?} for a remaining {micros}us fires before `until`"
            );
        }
    }

    #[test]
    fn a_delay_already_in_the_past_is_zero() {
        let now = Instant::now();
        let until = now - Duration::from_millis(5);
        assert_eq!(repaint_delay(until, now), Duration::ZERO);
    }
}
