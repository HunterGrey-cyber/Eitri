//! `shell` keeps the resize step and the bottom modules' minimum size from the two-`GtkPaned`
//! layout that `module_grid` replaced. `Direction`, divider selection and zoom behavior belong
//! to the core layout module. Its tests preserve the old `hidden_for_zoom` and `resize_target`
//! behavior.
//!
//! A hidden module is outside the geometry, so zoom and resize ignore it, and ending a zoom
//! never shows it. Hiding the zoomed module ends the zoom (`Layout::hide_unfocused`). Showing a
//! module restores its ratio; the first show gives it `BELOW_ROOT_SHARE`'s third.

pub(crate) use eitri_core::layout::Direction;

/// A cell's size, in logical px, when the editor cannot say (not ready yet, or hidden since
/// before nvim started).
pub(crate) const FALLBACK_CELL: (f64, f64) = (8.0, 16.0);

/// The bottom terminal's minimum height, and a Lua `bottom` panel's: `build_vertical_split`'s
/// `MIN_TERMINAL_HEIGHT`, kept for the reason that function gave -- a terminal dragged to zero
/// reports a 1-row grid to its child. The grid reads it as the host's minimum size and never
/// allocates less while there is room.
pub(crate) const BOTTOM_MIN_HEIGHT: i32 = 80;

/// One resize step in pixels: `cells` editor cells, across for left/right, down for up/down. The
/// sign is the geometry's (`eitri_core::layout::resize`).
pub(crate) fn resize_px(direction: Direction, cell: (f64, f64), cells: u16) -> i32 {
    let along = match direction {
        Direction::Left | Direction::Right => cell.0,
        Direction::Up | Direction::Down => cell.1,
    };
    (f64::from(cells) * along).round() as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_resize_step_is_cells_of_width_across_and_of_height_down() {
        assert_eq!(resize_px(Direction::Left, (8.0, 16.0), 5), 40);
        assert_eq!(resize_px(Direction::Right, (8.0, 16.0), 1), 8);
        assert_eq!(resize_px(Direction::Up, (8.0, 16.4), 5), 82);
        assert_eq!(resize_px(Direction::Down, (7.3, 16.0), 5), 80);
    }
}
