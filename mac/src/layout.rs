//! How wide the agent panel is, and when the window is too narrow to show it.
//!
//! Pure arithmetic with no AppKit type, so every edge is tested here.

/// The panel's width in points.
pub const PANEL_WIDTH_PT: f64 = 440.0;

/// The narrowest editor worth showing beside the panel, in points.
pub const MIN_EDITOR_PT: f64 = 200.0;

/// The window's smallest content height, in points.
pub const MIN_CONTENT_HEIGHT_PT: f64 = 200.0;

/// The content width below which the panel is hidden rather than squeezing the editor.
pub fn min_content_width_pt() -> f64 {
    PANEL_WIDTH_PT + MIN_EDITOR_PT
}

/// Whether the panel is on screen at this content width: the same rule that reserves its room
/// ([`reserved_px`]), so the view, the editor's grid and where the keys may go never disagree.
pub fn panel_shown(content_width_pt: f64, hidden_by_user: bool) -> bool {
    reserved_px(content_width_pt, 1.0, hidden_by_user) > 0
}

/// The content size, in points, a window too narrow for the panel is grown to: room for an editor of about a
/// hundred columns beside the panel, within the screen's usable area (less a margin), never below the width the
/// panel needs. Neovide's own first-start size is 500 by 500 pixels, a quarter of the panel's width on a Retina
/// display.
pub fn first_window_content_pt(visible_width_pt: f64, visible_height_pt: f64) -> (f64, f64) {
    const WIDTH: f64 = 1200.0;
    const HEIGHT: f64 = 760.0;
    const MARGIN: f64 = 40.0;
    // A title bar's height and a margin come off the usable height.
    const TITLE_AND_MARGIN: f64 = 80.0;
    let fit = |want: f64, room: f64| {
        if room.is_finite() && room > 0.0 {
            want.min(room)
        } else {
            want
        }
    };
    let width = fit(WIDTH, visible_width_pt - MARGIN).max(min_content_width_pt());
    let height = fit(HEIGHT, visible_height_pt - TITLE_AND_MARGIN).max(MIN_CONTENT_HEIGHT_PT);
    (width, height)
}

/// Whether the first window, found too narrow for the panel when it opened, is grown on the next turn: not when it is
/// full screen or on its way there (a start in full screen or maximized begins from a small frame, and a size set
/// during the transition is the one AppKit comes back to), and not when it has meanwhile become wide enough. The
/// width is read again then, not the one seen at the start. A width that is not a number is grown.
#[allow(clippy::neg_cmp_op_on_partial_ord)] // the negation is the point: a NaN width must fail the test
pub fn grows_first_window(content_width_pt: f64, full_screen: bool) -> bool {
    !full_screen && !(content_width_pt >= min_content_width_pt())
}

/// The panel's frame in the editor's view, in points (`x`, `y`, width, height; the origin at the bottom left): its
/// width against the right edge, the full height. A view narrower than the panel gives a negative `x`, which is
/// the point: the frame's left margin then grows with the window and its right edge stays on the window's.
pub fn panel_frame_pt(editor_width_pt: f64, editor_height_pt: f64) -> (f64, f64, f64, f64) {
    (editor_width_pt - PANEL_WIDTH_PT, 0.0, PANEL_WIDTH_PT, editor_height_pt)
}

/// The pixels the panel reserves on the editor's right edge, 0 when it is hidden: the user hid it, the window
/// is too narrow to keep the editor's minimum beside it, or the inputs are not numbers a size can come from.
///
/// The comparisons are written so that a NaN fails them and hides the panel (a `<` test would reserve the
/// panel's width for a NaN), and a scale that is zero, negative, NaN or infinite reserves nothing instead of
/// saturating the conversion to `u32::MAX`.
#[allow(clippy::neg_cmp_op_on_partial_ord)] // the negation is the point: a NaN width must fail the test
pub fn reserved_px(content_width_pt: f64, scale_factor: f64, hidden_by_user: bool) -> u32 {
    if hidden_by_user || !(content_width_pt >= min_content_width_pt()) {
        return 0;
    }
    if !(scale_factor.is_finite() && scale_factor > 0.0) {
        return 0;
    }
    (PANEL_WIDTH_PT * scale_factor).round() as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_panel_hides_below_its_width_plus_the_editor_minimum() {
        assert_eq!(min_content_width_pt(), 640.0);
        assert_eq!(reserved_px(1240.0, 2.0, false), 880);
        assert_eq!(reserved_px(640.0, 2.0, false), 880);
        assert_eq!(reserved_px(639.0, 2.0, false), 0);
        assert_eq!(reserved_px(1240.0, 2.0, true), 0);
    }

    #[test]
    fn the_panel_shows_by_the_same_rule_that_reserves_its_room() {
        assert!(panel_shown(640.0, false));
        assert!(!panel_shown(639.0, false));
        assert!(!panel_shown(1240.0, true));
        assert!(!panel_shown(f64::NAN, false));
    }

    #[test]
    fn a_first_window_fits_the_screen_and_the_panel() {
        // This MacBook Air's usable area at its default scaling, and a big display.
        assert_eq!(first_window_content_pt(1470.0, 919.0), (1200.0, 760.0));
        assert_eq!(first_window_content_pt(2560.0, 1415.0), (1200.0, 760.0));
        // A small screen: fitted, but never narrower than the panel needs.
        assert_eq!(first_window_content_pt(1024.0, 700.0), (984.0, 620.0));
        assert_eq!(first_window_content_pt(600.0, 250.0), (640.0, 200.0));
        // No usable numbers: the defaults.
        assert_eq!(first_window_content_pt(f64::NAN, 0.0), (1200.0, 760.0));
    }

    #[test]
    fn the_first_window_grows_only_when_it_is_still_narrow_and_not_full_screen() {
        assert!(grows_first_window(250.0, false));
        assert!(grows_first_window(639.0, false));
        assert!(grows_first_window(f64::NAN, false));
        // Grown meanwhile (a saved size, a maximized start): left as it is.
        assert!(!grows_first_window(640.0, false));
        assert!(!grows_first_window(1470.0, false));
        // Full screen, or entering it: never resized under the transition.
        assert!(!grows_first_window(250.0, true));
        assert!(!grows_first_window(1470.0, true));
    }

    #[test]
    fn the_panel_frame_keeps_the_right_edge() {
        // Exactly the panel's width: from the left edge.
        assert_eq!(panel_frame_pt(440.0, 700.0), (0.0, 0.0, 440.0, 700.0));
        // Neovide's first window, 250 pt: past the left edge, right edge on the view's.
        let (x, _, width, _) = panel_frame_pt(250.0, 250.0);
        assert_eq!((x, x + width), (-190.0, 250.0));
        assert_eq!(panel_frame_pt(1200.0, 760.0), (760.0, 0.0, 440.0, 760.0));
    }

    #[test]
    fn a_fractional_scale_rounds_to_whole_pixels() {
        assert_eq!(reserved_px(640.0, 1.5, false), 660);
    }

    #[test]
    fn numbers_a_size_cannot_come_from_reserve_nothing() {
        assert_eq!(reserved_px(f64::NAN, 2.0, false), 0);
        for scale in [f64::INFINITY, f64::NEG_INFINITY, f64::NAN, 0.0, -1.0] {
            assert_eq!(reserved_px(1240.0, scale, false), 0, "{scale}");
        }
        assert_eq!(reserved_px(f64::INFINITY, 2.0, false), 880);
    }
}
