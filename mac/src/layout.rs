//! How wide the agent panel is, and when the window is too narrow to show it.
//!
//! Pure arithmetic with no AppKit type, so every edge is tested here.

/// The panel's width in points.
pub const PANEL_WIDTH_PT: f64 = 440.0;

/// The narrowest editor worth showing beside the panel, in points.
pub const MIN_EDITOR_PT: f64 = 200.0;

/// The content width below which the panel is hidden rather than squeezing the editor.
pub fn min_content_width_pt() -> f64 {
    PANEL_WIDTH_PT + MIN_EDITOR_PT
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
