//! Animation time is separate from the wall-clock interval used to report render FPS.
//!
//! An on-demand renderer may not have drawn for seconds when a new cursor destination arrives.
//! Feeding that entire gap to Neovide's spring completes the NEW animation in its first frame.
//! Like native Neovide's `reset_animation_period`, start a new animation with one display period.
//! During an animation keep elapsed display time, including missed frames, so a 60 Hz display
//! does not run animations at half speed. This is not the native scheduler's simulation substeps:
//! `LiveHarness` exposes only a combined animate-and-draw call; repeating it would redraw too.
//!
//! A session's first frames are the exception ([`AnimationClock::place`]): until one of them shows
//! nvim there is no cursor or window on screen to move from, and an animation started there only
//! sends the cursor in from the canvas origin. Its spring then keeps the pane drawing for about
//! 0.4 s, the last 0.25 s of it sub-pixel steps that change nothing on screen.

/// The step [`AnimationClock::place`] hands out: longer than any of Neovide's default animations
/// (0.15 s cursor and window moves, 0.3 s scrolling), so a spring given it ends at its destination
/// in that frame. It is also the longest gap the native scheduler simulates.
pub(crate) const PLACE_STEP: f32 = 1.0;

#[derive(Default)]
pub(crate) struct AnimationClock {
    last_frame_us: Option<i64>,
}

impl AnimationClock {
    pub(crate) fn advance_subsurface(&mut self, now_us: i64, period: std::time::Duration, was_animating: bool) -> f32 {
        self.advance(now_us, period.as_micros().min(i64::MAX as u128) as i64, was_animating)
    }
    /// Call once per actual render, never from the polling tick. GDK's frame time stays fixed
    /// throughout a frame, unlike `Instant::now()` inside a variable-cost render callback.
    pub(crate) fn advance(&mut self, frame_time_us: i64, refresh_interval_us: i64, was_animating: bool) -> f32 {
        let previous = self.last_frame_us.replace(frame_time_us);
        let interval = if refresh_interval_us > 0 {
            refresh_interval_us as f32 / 1_000_000.0
        } else {
            1.0 / 60.0
        };
        let Some(elapsed) = previous.and_then(|previous| frame_time_us.checked_sub(previous)) else {
            return interval;
        };
        if elapsed == 0 {
            return 0.0;
        }
        // A changed frame clock or long suspension starts a fresh period. The one-second bound
        // is the native scheduler's own protection against simulating a suspended application.
        if !was_animating || !(0..=1_000_000).contains(&elapsed) {
            interval
        } else {
            elapsed as f32 / 1_000_000.0
        }
    }

    /// For a frame that places what it draws instead of animating to it: records the frame like
    /// [`AnimationClock::advance`] and returns [`PLACE_STEP`].
    pub(crate) fn place(&mut self, frame_time_us: i64) -> f32 {
        self.last_frame_us = Some(frame_time_us);
        PLACE_STEP
    }

    pub(crate) fn reset(&mut self) {
        self.last_frame_us = None;
    }
}

#[cfg(test)]
mod tests {
    use super::AnimationClock;

    fn seconds(actual: f32, expected: f32) {
        assert!(
            (actual - expected).abs() < 0.000002,
            "got {actual}s, expected {expected}s"
        );
    }

    #[test]
    fn idle_time_does_not_finish_a_new_cursor_animation() {
        for idle_us in [200_000, 5_000_000] {
            let mut clock = AnimationClock::default();
            clock.advance(1_000_000, 16_667, false);
            seconds(clock.advance(1_000_000 + idle_us, 16_667, false), 0.016667);
        }
    }

    #[test]
    fn a_key_frame_before_the_nvim_reply_does_not_age_the_reply() {
        let mut clock = AnimationClock::default();
        clock.advance(1_000_000, 8_333, false);
        // The key can request a frame of the OLD cursor before nvim's redraw arrives.
        seconds(clock.advance(4_000_000, 8_333, false), 0.008333);
        seconds(clock.advance(4_016_666, 8_333, false), 0.008333);
        seconds(clock.advance(4_024_999, 8_333, true), 0.008333);
    }

    #[test]
    fn skipped_frames_during_an_animation_keep_the_full_elapsed_time() {
        let mut clock = AnimationClock::default();
        clock.advance(1_000_000, 16_667, false);
        // Two display periods pass between actual renders; tick polling must not change this.
        seconds(clock.advance(1_033_334, 16_667, true), 0.033334);
    }

    #[test]
    fn refresh_rate_does_not_change_animation_speed() {
        for hz in [60, 120, 144] {
            let mut clock = AnimationClock::default();
            let interval = 1_000_000 / hz;
            clock.advance(1_000_000, interval, false);
            let mut elapsed = 0.0;
            for frame in 1..=hz {
                elapsed += clock.advance(1_000_000 + frame * 1_000_000 / hz, interval, true);
            }
            seconds(elapsed, 1.0);
        }
    }

    #[test]
    fn redrawing_the_same_display_frame_does_not_advance_twice() {
        let mut clock = AnimationClock::default();
        clock.advance(1_000_000, 16_667, false);
        seconds(clock.advance(1_000_000, 16_667, true), 0.0);
        seconds(clock.advance(1_000_000, 16_667, false), 0.0);
    }

    #[test]
    fn remapping_or_changing_frame_clocks_starts_a_fresh_period() {
        let mut clock = AnimationClock::default();
        clock.advance(1_000_000, 16_667, true);
        clock.reset();
        seconds(clock.advance(9_000_000, 6_944, true), 0.006944);
        seconds(clock.advance(1_000_000, 6_944, true), 0.006944);
    }

    #[test]
    fn a_long_suspension_is_not_simulated_as_one_animation_step() {
        let mut clock = AnimationClock::default();
        clock.advance(1_000_000, 16_667, false);
        seconds(clock.advance(9_000_000, 16_667, true), 0.016667);
    }

    #[test]
    fn a_placed_frame_ends_every_default_animation_and_the_next_one_starts_a_period() {
        let mut clock = AnimationClock::default();
        let step = clock.place(1_000_000);
        assert!(step >= 0.3, "{step}s would leave a default scroll animation running");
        // The frame after it is timed from it, as after any frame that was not animating.
        seconds(clock.advance(1_050_000, 6_061, false), 0.006061);
        seconds(clock.advance(1_056_061, 6_061, true), 0.006061);
    }

    #[test]
    fn missing_refresh_information_uses_a_positive_default() {
        let mut clock = AnimationClock::default();
        seconds(clock.advance(1_000_000, 0, false), 1.0 / 60.0);
        seconds(clock.advance(2_000_000, -1, false), 1.0 / 60.0);
    }

    #[test]
    fn subsurface_animation_uses_the_pacers_period_and_the_render_time() {
        let mut clock = AnimationClock::default();
        let period = std::time::Duration::from_nanos(6_060_606);
        seconds(clock.advance_subsurface(1_000_000, period, false), 0.006060);
        seconds(clock.advance_subsurface(1_012_121, period, true), 0.012121);
        seconds(clock.advance_subsurface(9_000_000, period, false), 0.006060);
    }
}
