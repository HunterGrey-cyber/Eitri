//! `Screen::render`'s live (unscrolled) view projects with `Projector::full`,
//! which drops a wholly blank row from `TerminalFrame::rows_changed` entirely and truncates a
//! short row right after its last non-default cell (`full_rows`, `terminal/frame/src/project.rs`).
//! A selection must highlight cells beyond what the frame supplies, as it does in the
//! scrolled-back view (`project_window`, which supplies every column of every row).
//!
//! Deliberately NOT `tests/common`'s `Screen`: that helper always projects with
//! `project_window` regardless of `ViewMode` (see its `paint_window`), so it never reproduces the
//! live-view gap this test exists to catch. This drives a real `Term` through the real
//! `vte::ansi::Processor` and reprojects it with `Projector::full`, exactly the frame shape
//! `eitri_terminal::screen::Screen::render` uses outside a synchronized update.

use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::vte::ansi::Processor;
use terminal_frame::Projector;
use terminal_render::{
    build_paint_list, palette_for, CursorColoring, PaintList, PaintOp, RenderInput, RgbColor, SelectionSpan, ViewMode,
};

struct Size {
    cols: usize,
    rows: usize,
}

impl Dimensions for Size {
    fn total_lines(&self) -> usize {
        self.rows
    }
    fn screen_lines(&self) -> usize {
        self.rows
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

fn bg_at(list: &PaintList, row: u16, col: u16) -> Option<RgbColor> {
    list.ops.iter().find_map(|op| match op {
        PaintOp::FillCells {
            row: r, col: c, color, ..
        } if *r == row && *c == col => Some(*color),
        _ => None,
    })
}

#[test]
fn a_selection_still_highlights_a_blank_row_and_the_trailing_blanks_of_a_short_row() {
    let size = Size { cols: 10, rows: 3 };
    let mut term = Term::new(Config::default(), &size, VoidListener);
    let mut parser: Processor = Processor::new();
    // Row 0: "abc". Row 1: wholly blank. Row 2: "def" (a short row).
    parser.advance(&mut term, b"abc\r\n\r\ndef");

    // The exact projection `Screen::render` uses for the live (unscrolled) view.
    let frame = Projector::new().full(&term);
    assert!(
        !frame.rows_changed.iter().any(|r| r.line == 1),
        "sanity: full_rows really does drop the wholly blank row entirely"
    );
    let row0 = frame.rows_changed.iter().find(|r| r.line == 0).unwrap();
    assert_eq!(
        row0.cells.len(),
        3,
        "sanity: full_rows really does truncate row 0 after 'abc'"
    );

    // A selection spanning all three rows: row 0 from col 1 to the last column, row 1 (blank)
    // in full, row 2 (short) from col 0 through col 5 -- past "def"'s own 3 cells, into the
    // trailing default cells a short row leaves unsupplied.
    let selection = [
        SelectionSpan {
            line: 0,
            start_col: 1,
            end_col: 9,
        },
        SelectionSpan {
            line: 1,
            start_col: 0,
            end_col: 9,
        },
        SelectionSpan {
            line: 2,
            start_col: 0,
            end_col: 5,
        },
    ];

    let palette = palette_for(&frame);
    let list = build_paint_list(&RenderInput {
        frame: &frame,
        window_top_line: 0,
        mode: ViewMode::FollowBottom,
        selection: &selection,
        focused: true,
        palette: &palette,
        cursor_color: CursorColoring::Palette,
    });

    // The highlight colour is whatever a covered, selected cell already gets -- read it off
    // column 1 of row 0, which the cell loop covers directly (the "b" of "abc").
    let highlight = bg_at(&list, 0, 1).expect("a covered selected cell has a fill op");

    // Row 0: col 0 ("a") sits outside the selection and keeps its own ordinary (unselected)
    // background fill; cols 1-9 are inside it and must be highlighted, covered by the frame or
    // not.
    assert_ne!(
        bg_at(&list, 0, 0),
        Some(highlight),
        "col 0 of row 0 is outside the selection and must not be highlighted"
    );
    for col in 1..10u16 {
        assert_eq!(
            bg_at(&list, 0, col),
            Some(highlight),
            "row 0 col {col} is inside the selection and must be highlighted"
        );
    }

    // Row 1 is wholly blank and absent from the frame entirely, but the whole row is selected.
    for col in 0..10u16 {
        assert_eq!(
            bg_at(&list, 1, col),
            Some(highlight),
            "the wholly blank row 1, entirely absent from the frame, must still be highlighted"
        );
    }

    // Row 2 ("def"): columns 0-2 are both covered and selected. Columns 3-5 are selected
    // but beyond `full_rows`, so they still need selection fill. Columns 6-9 are outside
    // the selection and must receive no fill.
    for col in 0..=5u16 {
        assert_eq!(
            bg_at(&list, 2, col),
            Some(highlight),
            "row 2 col {col} (including the trailing blanks past 'def') must be highlighted"
        );
    }
    for col in 6..10u16 {
        assert_eq!(
            bg_at(&list, 2, col),
            None,
            "row 2 col {col} is past the selection and must not be filled"
        );
    }
}

/// Build time grows in proportion to the number of rows: a 4x taller fully selected frame
/// stays below an 8x time ratio, where per-row scans of the whole selection would give about
/// 16x. The comparison is independent of how fast the machine is, with no absolute time bound.
#[test]
fn a_fully_selected_tall_frame_is_built_in_linear_time() {
    fn measure(rows: usize) -> (std::time::Duration, PaintList) {
        let size = Size { cols: 20, rows };
        let mut term = Term::new(Config::default(), &size, VoidListener);
        let mut parser: Processor = Processor::new();
        for i in 0..rows {
            parser.advance(&mut term, format!("{i:019}\r\n").as_bytes());
        }
        let frame = Projector::new().full(&term);
        let selection: Vec<SelectionSpan> = (0..rows)
            .map(|line| SelectionSpan {
                line: line as i32,
                start_col: 0,
                end_col: 19,
            })
            .collect();
        let palette = palette_for(&frame);
        let input = RenderInput {
            frame: &frame,
            window_top_line: 0,
            mode: ViewMode::FollowBottom,
            selection: &selection,
            focused: true,
            palette: &palette,
            cursor_color: CursorColoring::Palette,
        };
        let mut minimum = std::time::Duration::MAX;
        let mut last_list = None;
        for _ in 0..3 {
            let started = std::time::Instant::now();
            let list = build_paint_list(&input);
            let elapsed = started.elapsed();
            minimum = minimum.min(elapsed);
            last_list = Some(list);
        }
        (minimum, last_list.unwrap())
    }

    let (small, _) = measure(5_000);
    let (big, list) = measure(20_000);
    let highlight = bg_at(&list, 0, 0).expect("a fill");
    assert_eq!(
        bg_at(&list, 19_998, 19),
        Some(highlight),
        "the last full row is selected too"
    );
    assert!(
        big < small * 8,
        "5,000 rows: {small:?}; 20,000 rows: {big:?}; ratio: {:.2} (must be below 8)",
        big.as_secs_f64() / small.as_secs_f64()
    );
}
