//! Scroll indicators expressed as toolkit-free terminal paint operations.

use terminal_render::color::{BACKGROUND, CURSOR};
use terminal_render::{PaintOp, Palette};

/// tmux copy-mode's `[N/M]` (the indicator a tmux user already reads every day),
/// right-aligned in the top row: `above` is how many lines the scrolled-back window sits below the
/// live bottom (`crate::scroll::ScrollView::indicator`), `history` the terminal's whole retained
/// scrollback. Built as a `PaintOp::DrawNotice` -- never a bespoke fill-plus-text pair -- for the
/// same reason `crate::screen::Screen::render` appends it after every other op rather than mixing it
/// into `build_paint_list`'s own stream: `DrawNotice` is `terminal-render`'s highest `PaintLayer`
/// (`Overlay`), so appending one after whatever a frame already drew (a visible cursor included)
/// keeps `PaintList::is_layer_ordered` true with no special-casing at the call site.
///
/// "Reversed cursor colours": a filled block cursor paints the `CURSOR` colour over a cell and
/// recolours that cell's own character into its background so it still reads; this notice reverses
/// which colour is the fill and which is the text -- background in `CURSOR`, text in the terminal's
/// own `BACKGROUND` -- the same pairing, the other way round.
pub fn indicator_ops(above: usize, history: usize, cols: u16, palette: &Palette) -> Vec<PaintOp> {
    if cols == 0 {
        return Vec::new();
    }
    let full = format!("[{above}/{history}]");
    let width = (full.chars().count() as u16).min(cols);
    let start = cols - width;
    let text: String = full.chars().take(width as usize).collect();
    vec![PaintOp::DrawNotice {
        row: 0,
        col: start,
        cols: width,
        text,
        color: palette.get(BACKGROUND),
        background: palette.get(CURSOR),
    }]
}

#[cfg(test)]
mod tests {
    use super::*;
    use terminal_render::RgbColor;

    fn themed_palette() -> Palette {
        use terminal_frame::frame::{ColorOverride, Rgb as FrameRgb};
        let set = |index: u16, c: RgbColor| ColorOverride {
            index,
            color: Some(FrameRgb { r: c.r, g: c.g, b: c.b }),
        };
        let mut palette = Palette::xterm_default();
        palette.apply_overrides(&[
            set(BACKGROUND, RgbColor::new(0x12, 0x34, 0x56)),
            set(CURSOR, RgbColor::new(0xee, 0xdd, 0xcc)),
        ]);
        palette
    }

    /// The scroll indicator sits right-aligned in the top row, in the palette's `CURSOR`/`BACKGROUND`
    /// colours reversed: the fill takes `CURSOR`, the text `BACKGROUND` -- the opposite pairing a
    /// filled cursor itself uses, so the indicator never reads as a cursor.
    #[test]
    fn the_indicator_is_right_aligned_at_row_0_in_reversed_cursor_colours() {
        let palette = themed_palette();
        let ops = indicator_ops(3, 46, 20, &palette);
        assert_eq!(ops.len(), 1);
        match &ops[0] {
            PaintOp::DrawNotice {
                row,
                col,
                cols,
                text,
                color,
                background,
            } => {
                assert_eq!(*row, 0);
                assert_eq!(text, "[3/46]");
                assert_eq!(*cols, text.chars().count() as u16);
                assert_eq!(*col, 20 - cols, "right-aligned against the row's width");
                assert_eq!(*color, palette.get(BACKGROUND), "text in the terminal's own background");
                assert_eq!(*background, palette.get(CURSOR), "fill in the cursor colour");
            }
            other => panic!("expected one DrawNotice, got {other:?}"),
        }
    }

    /// A 0-column row (never reached with a real terminal, whose grid clamps below one cell) draws
    /// nothing rather than underflowing `cols - width`.
    #[test]
    fn a_zero_width_row_draws_nothing() {
        assert_eq!(indicator_ops(3, 46, 0, &themed_palette()), Vec::new());
    }

    /// Wider than the row it must fit in: clipped to the row's width rather than drawn past it.
    #[test]
    fn an_indicator_wider_than_the_row_is_clipped_to_it() {
        let ops = indicator_ops(123_456, 999_999, 4, &themed_palette());
        match &ops[0] {
            PaintOp::DrawNotice { col, cols, text, .. } => {
                assert_eq!(*cols, 4);
                assert_eq!(*col, 0);
                assert_eq!(text.chars().count(), 4);
            }
            other => panic!("expected one DrawNotice, got {other:?}"),
        }
    }

    /// `DrawNotice` is `terminal-render`'s highest `PaintLayer` (`Overlay`): appended after a frame's
    /// own ops, it never breaks `PaintList::is_layer_ordered`, however many ops already came before
    /// it -- background, text and even a drawn cursor.
    #[test]
    fn the_indicators_layer_is_the_highest_one_terminal_render_defines() {
        use terminal_render::PaintLayer;
        let ops = indicator_ops(1, 2, 10, &themed_palette());
        assert_eq!(ops[0].layer(), PaintLayer::Overlay);
        assert!(PaintLayer::Cursor <= PaintLayer::Overlay);
    }
}
