//! What `shell` keeps of the two-`GtkPaned` layout that `module_grid` replaced (modules design P1,
//! docs/superpowers/specs/2026-09-23-modules-and-canvas-design.md §5): the step `Ctrl+a h/j/k/l`
//! moves a divider by, the bottom modules' floor, and `Direction`, which is `neovibe-core`'s now.
//! Which divider moves, and how a zoom hides the rest, is `neovibe_core::layout`'s; the old
//! `hidden_for_zoom`/`resize_target` tables live on as its tests
//! (`navigation_reproduces_todays_dispatch_*`, `resize_moves_the_divider_todays_resize_target_moved`).
//!
//! The bottom terminal's own `PaneLayout` additions on `main` (`bottom_shown`, `bottom_visible`,
//! `shown_position`, `hiding_bottom_must_unzoom`) did not come across with it when the modules
//! design re-homed it (P1, Task 11): a hidden module is not in the geometry, so a zoom and a resize
//! ignore it and ending a zoom never shows it (`invariant_4_a_zoom_never_touches_the_hidden_set`);
//! hiding the zoomed module ends the zoom (`Layout::hide_unfocused`); and a shown module keeps its
//! ratio, the first show giving it `BELOW_ROOT_SHARE`'s third (`show_puts_a_module_back_where_it_was`,
//! `the_terminal_hides_and_shows_as_ctrl_a_t_did_on_main`).

pub(crate) use neovibe_core::layout::Direction;

/// How far one `Ctrl+a h/j/k/l` moves a divider, in editor cells (owner's tmux: `resize-pane -L 5`).
pub(crate) const RESIZE_CELLS: i32 = 5;

/// A cell's size, in logical px, when the editor cannot say (not ready yet, or hidden since
/// before nvim started).
pub(crate) const FALLBACK_CELL: (f64, f64) = (8.0, 16.0);

/// The bottom terminal's minimum height, and a Lua `bottom` panel's: `build_vertical_split`'s
/// `MIN_TERMINAL_HEIGHT`, kept for the reason that function gave -- a terminal dragged to zero
/// reports a 1-row grid to its child. The grid reads it as the host's minimum size and never
/// allocates less while there is room.
pub(crate) const BOTTOM_MIN_HEIGHT: i32 = 80;

/// One `Ctrl+a h/j/k/l` step in pixels: `RESIZE_CELLS` editor cells, across for `h`/`l`, down for
/// `j`/`k`. The sign is the geometry's (`neovibe_core::layout::resize`).
pub(crate) fn resize_px(direction: Direction, cell: (f64, f64)) -> i32 {
    let along = match direction {
        Direction::Left | Direction::Right => cell.0,
        Direction::Up | Direction::Down => cell.1,
    };
    (f64::from(RESIZE_CELLS) * along).round() as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_resize_step_is_five_cells_of_width_across_and_of_height_down() {
        assert_eq!(resize_px(Direction::Left, (8.0, 16.0)), 40);
        assert_eq!(resize_px(Direction::Right, (8.0, 16.0)), 40);
        assert_eq!(resize_px(Direction::Up, (8.0, 16.4)), 82);
        assert_eq!(resize_px(Direction::Down, (7.3, 16.0)), 80);
    }
}
