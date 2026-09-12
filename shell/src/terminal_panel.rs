//! The built-in terminal panel: a `terminal_pane::TerminalPane` in the bottom slot.
//!
//! # Why it is behind `--terminal`
//!
//! Because it cannot do anything yet, and pretending otherwise would be worse than not shipping it.
//! Verdandi has no Rust PTY + `Term` session type, and writing one in this repository would be the
//! second terminal emulator the whole architecture exists to forbid. So the pane renders, takes
//! focus, resizes, reports its grid and normalises keystrokes -- and there is nothing on the other
//! end of any of it.
//!
//! Making that opt-in keeps the product's default startup, memory and focus behaviour exactly what
//! it was, while the layout and focus wiring the terminal will need is real, in the real binary,
//! and verifiable now rather than written blind later. The flag goes away when a session type
//! lands.
//!
//! # What it shows
//!
//! A frame that says what is happening, rendered through the same `PaintList` path a real session
//! will use -- not a GTK label. A label would prove the layout works and prove nothing about the
//! renderer; this way, if the pane is blank, the renderer is broken, and that is worth knowing.

use std::rc::Rc;

use gtk4::prelude::*;
use terminal_pane::TerminalPane;
use terminal_render::{CursorShape, GlyphStyle, PaintList, PaintOp, RgbColor, UnderlineKind};

const BG: RgbColor = RgbColor::new(0x14, 0x16, 0x1c);
const FG: RgbColor = RgbColor::new(0x9a, 0xa5, 0xb4);
const ACCENT: RgbColor = RgbColor::new(0x8a, 0xbe, 0xb7);

/// Builds the terminal panel and returns it with its widget.
///
/// The `Rc` is what the caller needs: focus routing and the resize log both outlive this function,
/// and `TerminalPane` is not `Clone`.
pub(crate) fn build_terminal_panel() -> (Rc<TerminalPane>, gtk4::Widget) {
    let pane = Rc::new(TerminalPane::new());

    // Redrawn on every grid change, because the placeholder text is laid out in cells: a frame
    // built for the old grid would be clipped or stranded mid-line after a resize.
    {
        let pane_for_resize = pane.clone();
        pane.connect_resize(move |cols, rows| {
            println!("[terminal] grid {cols}x{rows}");
            pane_for_resize.set_paint_list(placeholder(cols, rows));
        });
    }

    // Input is normalised and then deliberately goes nowhere. Logged rather than dropped silently
    // so that "the terminal has focus and is receiving keys" stays observable while there is
    // nothing to send them to.
    pane.connect_input(|input| {
        println!("[terminal] input (no session to encode for): {input:?}");
    });

    let (cols, rows) = pane.grid_size();
    pane.set_paint_list(placeholder(cols, rows));

    let widget: gtk4::Widget = pane.widget().clone().upcast();
    (pane, widget)
}

/// The "there is no session" frame.
fn placeholder(cols: u16, rows: u16) -> PaintList {
    let lines: [(&str, RgbColor, bool); 4] = [
        ("neovibe terminal", ACCENT, true),
        ("", FG, false),
        ("no session yet -- Verdandi owns the PTY and Term, and has not shipped them.", FG, false),
        ("the pane itself is live: resize it, focus it, type into it (see stdout).", FG, false),
    ];

    let mut ops = Vec::new();
    for row in 0..rows {
        ops.push(PaintOp::FillCells { row, col: 0, cols, color: BG });
    }
    for (index, (text, color, bold)) in lines.iter().enumerate() {
        let row = index as u16 + 1;
        if row >= rows {
            break;
        }
        // Starts at column 2 for a left margin, and stops at the pane's own edge: the contract
        // requires every op to lie inside the window, and a narrow pane must truncate rather than
        // emit an op past its last column.
        for (col, ch) in (2u16..).zip(text.chars()) {
            if col >= cols {
                break;
            }
            ops.push(PaintOp::DrawText {
                row,
                col,
                cols: 1,
                text: ch.to_string(),
                color: *color,
                style: GlyphStyle {
                    bold: *bold,
                    italic: false,
                    underline: UnderlineKind::None,
                    underline_color: *color,
                    strikeout: false,
                },
            });
        }
    }
    // A hollow cursor, which is the contract's own "this surface does not have the keyboard" shape.
    // Drawing a solid block would claim focus the pane may not have.
    if rows > 5 {
        ops.push(PaintOp::DrawCursor {
            row: 5,
            col: 2,
            cols: 1,
            shape: CursorShape::HollowBlock,
            color: FG,
            text_under: None,
            blinking: false,
        });
    }

    PaintList { ops, cols, rows, surface_background: BG, top_line: 0 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use terminal_render::PaintLayer;

    /// The backend relies on layer order, and a hand-built list is exactly where that gets broken.
    #[test]
    fn the_placeholder_is_layer_ordered() {
        assert!(placeholder(80, 24).is_layer_ordered());
    }

    /// A pane measured before GTK has laid it out reports 1x1. The placeholder must survive that
    /// rather than index past the end of a one-row grid.
    #[test]
    fn a_one_cell_grid_still_produces_a_valid_frame() {
        let list = placeholder(1, 1);
        assert!(list.is_layer_ordered());
        for op in &list.ops {
            let (row, col, cols) = match op {
                PaintOp::FillCells { row, col, cols, .. } => (*row, *col, *cols),
                PaintOp::DrawText { row, col, cols, .. } => (*row, *col, *cols),
                PaintOp::DrawCursor { row, col, cols, .. } => (*row, *col, *cols),
                PaintOp::DrawNotice { row, col, cols, .. } => (*row, *col, *cols),
            };
            assert!(row < list.rows, "row {row} outside {}", list.rows);
            assert!(col + cols <= list.cols, "col {col}+{cols} outside {}", list.cols);
        }
    }

    /// Every op must lie inside the window at a realistic size too -- the invariant the contract
    /// states for Verdandi's own lists, held to here because the backend trusts it either way.
    #[test]
    fn every_op_lies_inside_the_window() {
        let list = placeholder(30, 8);
        assert!(list.ops.iter().all(|op| op.layer() <= PaintLayer::Cursor));
        let text_rows: Vec<u16> = list
            .ops
            .iter()
            .filter_map(|op| match op {
                PaintOp::DrawText { row, .. } => Some(*row),
                _ => None,
            })
            .collect();
        assert!(text_rows.iter().all(|r| *r < 8));
        // Narrow enough that the long lines are truncated rather than wrapped or overflowed.
        assert!(list.ops.iter().all(|op| match op {
            PaintOp::DrawText { col, cols, .. } => col + cols <= 30,
            _ => true,
        }));
    }
}
