//! What of the prefix's hint panel fits the window, decided from measures a toolkit gives: the groups that fit across,
//! the rows of a group that fit its height, and the share of the window the panel may take. The panel's widgets are
//! the host's (`prefix_panel.rs` in the GTK shell).

/// The share of the window's width the panel may take, in percent.
pub const MAX_WIDTH_PERCENT: i32 = 90;
/// The share of the window's height the panel may take, in percent.
pub const MAX_HEIGHT_PERCENT: i32 = 45;

/// How many of the leading groups fit across `available` pixels: each width in `widths` is a group's own width
/// with the gap that follows it, and the first that would not fit, with every one after it, is left out. Nothing
/// fits in no room at all, and the footer is then all the panel shows.
pub fn columns_that_fit(widths: &[i32], available: i32) -> usize {
    let mut used = 0;
    for (index, width) in widths.iter().enumerate() {
        used += (*width).max(0);
        if used > available {
            return index;
        }
    }
    widths.len()
}

/// How many of a group's `rows`, each `row` pixels high, stay in `room` pixels: all of them when they fit, else as
/// many as leave a line for the one that says the rest were left out.
pub fn rows_that_fit(rows: usize, room: i32, row: i32) -> usize {
    if row <= 0 {
        return rows;
    }
    let lines = usize::try_from(room.max(0) / row).unwrap_or(0);
    if lines >= rows {
        rows
    } else {
        lines.saturating_sub(1)
    }
}

#[cfg(test)]
mod tests {
    use super::{columns_that_fit, rows_that_fit};

    #[test]
    fn every_group_that_fits_stays_and_the_first_that_does_not_ends_the_row() {
        assert_eq!(columns_that_fit(&[100, 100, 100], 300), 3, "all fit, to the pixel");
        assert_eq!(columns_that_fit(&[100, 100, 100], 299), 2, "the last group is dropped");
        assert_eq!(
            columns_that_fit(&[100, 250, 40], 300),
            1,
            "a group after one that does not fit is dropped too"
        );
        assert_eq!(columns_that_fit(&[], 300), 0, "no groups, none to fit");
    }

    #[test]
    fn rows_that_fit_all_stay_and_rows_that_do_not_leave_a_line_for_saying_so() {
        assert_eq!(rows_that_fit(26, 390, 15), 26, "all fit, to the pixel");
        assert_eq!(
            rows_that_fit(27, 390, 15),
            25,
            "the last line goes to the count of the rest"
        );
        assert_eq!(rows_that_fit(27, 15, 15), 0, "room for the count alone");
        assert_eq!(rows_that_fit(27, 0, 15), 0);
        assert_eq!(rows_that_fit(27, -5, 15), 0, "no room, no rows");
        assert_eq!(
            rows_that_fit(3, 100, 0),
            3,
            "a row not measured yet leaves everything in"
        );
    }

    #[test]
    fn with_no_room_no_group_shows_and_only_the_footer_is_left() {
        assert_eq!(columns_that_fit(&[100, 100], 50), 0);
        assert_eq!(columns_that_fit(&[100, 100], 0), 0);
        assert_eq!(
            columns_that_fit(&[100], -20),
            0,
            "a window narrower than the panel's frame"
        );
    }
}
