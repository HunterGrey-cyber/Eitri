//! Executes a `PaintList` onto a Skia canvas.
//!
//! Terminal semantics are resolved before painting: colours, glyph column counts, and cursor
//! contrast arrive in the operations. Palette handling, SGR, inverse, selection, wide-character
//! spacers, and scrollback arithmetic belong in `terminal-render`, where they are resolved once.
//!
//! Every visible cell is repainted each frame. There is no damage tracking, partial redraw, or
//! glyph atlas here, keeping drawing errors confined to operations, metrics, drawing, or clipping.
//! [`paint_ops`] draws operations without clearing the canvas, so the host can draw input-method
//! preedit over a finished frame.

use crate::metrics::TerminalMetrics;
use skia_safe::{Canvas, Color, Paint, Rect, TextBlob};
use terminal_render::color::{BACKGROUND, CURSOR};
use terminal_render::{CursorShape, GlyphStyle, PaintList, PaintOp, Palette, RgbColor, UnderlineKind};

/// Thickness of an underline or strikeout, as a fraction of cell height. Skia's font metrics do
/// carry `underline_thickness`, but it is absent on many faces and a terminal wants a consistent
/// rule across whatever fallback font a CJK glyph happened to come from.
const RULE_THICKNESS: f32 = 0.08;
/// How far below the baseline an underline sits, as a fraction of cell height.
const UNDERLINE_DROP: f32 = 0.16;
/// Where a strikeout crosses, as a fraction of cell height above the baseline.
const STRIKE_RISE: f32 = 0.30;
/// Thickness of a beam cursor and of a hollow block's stroke, as a fraction of cell width.
const BEAM_FRACTION: f32 = 0.15;

fn to_skia(color: RgbColor) -> Color {
    Color::from_rgb(color.r, color.g, color.b)
}

fn fill_paint(color: RgbColor) -> Paint {
    let mut paint = Paint::default();
    paint.set_color(to_skia(color));
    paint.set_anti_alias(false); // Cell fills are axis-aligned rectangles; AA only blurs their seams.
    paint
}

fn text_paint(color: RgbColor) -> Paint {
    let mut paint = Paint::default();
    paint.set_color(to_skia(color));
    paint.set_anti_alias(true);
    paint
}

/// Paints one whole frame.
///
/// `canvas` is expected to be clipped to the pane already; this function paints from (0,0) in cell
/// space and does not know where on screen it is.
pub fn paint(canvas: &Canvas, list: &PaintList, metrics: &TerminalMetrics) {
    // "Clear the surface with `list.surface_background` before executing any op." Ops need not
    // cover every cell, and the pane is rarely an exact multiple of the cell size, so the remainder
    // has to come from somewhere -- this is that colour, and skipping it leaves the previous frame
    // showing through in the margin.
    canvas.clear(to_skia(list.surface_background));
    paint_ops(canvas, &list.ops, metrics);
}

/// Executes `ops` on top of whatever the canvas already holds, without clearing it: [`paint`]'s own
/// loop, and how the host paints something over a finished frame -- an input method's preedit
/// (`crate::preedit`). The same lattice, the same fonts, the same rules; nothing is interpreted.
pub fn paint_ops(canvas: &Canvas, ops: &[PaintOp], metrics: &TerminalMetrics) {
    // Front to back, in the order given. `ops` is guaranteed non-decreasing in PaintLayer, so
    // honouring the given order IS honouring z-order -- this loop must never sort or batch by type.
    for op in ops {
        match op {
            PaintOp::FillCells { row, col, cols, color } => {
                canvas.draw_rect(metrics.cell_rect(*row, *col, *cols), &fill_paint(*color));
            }
            PaintOp::DrawText {
                row,
                col,
                cols,
                text,
                color,
                style,
            } => {
                draw_text(
                    canvas,
                    metrics,
                    CellSpan {
                        row: *row,
                        col: *col,
                        cols: *cols,
                    },
                    text,
                    *color,
                    style,
                );
            }
            PaintOp::DrawCursor {
                row,
                col,
                cols,
                shape,
                color,
                text_under,
                ..
            } => {
                // `blinking` is ignored on purpose: the handoff calls it advisory and a steady
                // cursor conforming. A blink would also mean repainting on a timer, which is exactly
                // the unconditional-redraw pattern that cost this project a measured 60x idle CPU
                // regression in the editor pane.
                let at = CellSpan {
                    row: *row,
                    col: *col,
                    cols: *cols,
                };
                draw_cursor(canvas, metrics, at, *shape, *color);
                if let Some(under) = text_under {
                    // Already recoloured to contrast. Nothing is inverted here.
                    draw_text(canvas, metrics, at, &under.text, under.color, &under.style);
                }
            }
            PaintOp::DrawNotice {
                row,
                col,
                cols,
                text,
                color,
                background,
            } => {
                let box_rect = metrics.cell_rect(*row, *col, *cols);
                canvas.draw_rect(box_rect, &fill_paint(*background));
                // Clipped to its own box: the notice carries its own extent, and prose that spilled
                // past it would overwrite terminal content the user is still meant to read.
                canvas.save();
                canvas.clip_rect(box_rect, None, false);
                draw_text(
                    canvas,
                    metrics,
                    CellSpan {
                        row: *row,
                        col: *col,
                        cols: *cols,
                    },
                    text,
                    *color,
                    &GlyphStyle::default(),
                );
                canvas.restore();
            }
        }
    }
}

/// tmux copy-mode's `[N/M]`,
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

/// Draws one grapheme in its box, then its decorations.
///
/// `text` is ONE grapheme and may be several scalars (a base plus combining marks). It is shaped as
/// a unit and `cols` -- never a scalar count -- decides the width, because `cols` is the terminal's
/// own accounting and the only correct answer.
/// Where an op sits on the lattice. Carried as one value so the four call sites cannot disagree
/// about the order of three interchangeable `u16`s -- a transposed row and col is a bug that still
/// compiles and still draws something.
#[derive(Clone, Copy)]
struct CellSpan {
    row: u16,
    col: u16,
    cols: u16,
}

fn draw_text(
    canvas: &Canvas,
    metrics: &TerminalMetrics,
    at: CellSpan,
    text: &str,
    color: RgbColor,
    style: &GlyphStyle,
) {
    let (row, col, cols) = (at.row, at.col, at.cols);
    if text.is_empty() {
        return;
    }
    let origin = metrics.text_origin(row, col);
    // Face selection (including fallback for graphemes the primary face lacks) lives in the
    // metrics, with the rest of the font decisions and with the memoisation that keeps it off the
    // per-frame fontconfig path.
    let font = metrics.font_for_text(text, style.bold, style.italic);

    // One blob per op. A wide glyph is one draw spanning two columns: there is no second op for the
    // second column and nothing here to suppress, because the spacer concept never reaches a backend.
    if let Some(blob) = TextBlob::new(text, &font) {
        canvas.draw_text_blob(blob, origin, &text_paint(color));
    }

    let cell = metrics.cell_rect(row, col, cols);
    let thickness = (metrics.cell_height() * RULE_THICKNESS).max(1.0);
    if style.underline != UnderlineKind::None {
        // `underline_color` is always concrete -- the glyph's own colour when the terminal specified
        // none -- so there is nothing to fall back to here.
        draw_underline(
            canvas,
            cell,
            origin.y,
            thickness,
            style.underline,
            style.underline_color,
        );
    }
    if style.strikeout {
        let y = origin.y - metrics.cell_height() * STRIKE_RISE;
        canvas.draw_rect(
            Rect::from_xywh(cell.left, y, cell.width(), thickness),
            &fill_paint(color),
        );
    }
}

/// Where an underline's bars go: the first bar's top, every bar's thickness, and the second bar's top
/// for a double one. In whole pixels, because rules are filled without antialiasing and so cover
/// whole pixel rows anyway.
///
/// In order of what gives way last: a rule never reaches the row below (only that row's next repaint
/// would take it away), never starts above the baseline (it would be drawn over the bottoms of the
/// letters), and sits a fixed share of the cell below the baseline when there is room. A double
/// rule's first bar keeps a pixel clear below the baseline where it can; to fit, the gap between its
/// bars shrinks to one pixel first, then the pair moves up towards the baseline, and only in a cell
/// with less room than that do the bars get thinner. A face with a shallow descent needs all of it
/// (Andale Mono leaves 3.4 px of a 17 px cell below the baseline).
#[derive(Debug, Clone, Copy, PartialEq)]
struct UnderlineRows {
    y: f32,
    thickness: f32,
    second: Option<f32>,
}

fn underline_rows(cell: Rect, baseline_y: f32, thickness: f32, double: bool) -> UnderlineRows {
    let bottom = cell.bottom.floor();
    let first_clear = baseline_y.ceil().min(bottom - 1.0);
    let room = bottom - first_clear;
    let preferred = (baseline_y + cell.height() * UNDERLINE_DROP).round();
    let mut t = thickness.round().max(1.0);
    if !double {
        t = t.min(room.max(1.0));
        let y = preferred.min(bottom - t).max(first_clear.min(bottom - t));
        return UnderlineRows {
            y,
            thickness: t,
            second: None,
        };
    }
    let wanted_top = (baseline_y + 1.0).ceil();
    let mut gap = t;
    if wanted_top + 2.0 * t + gap > bottom {
        gap = (bottom - wanted_top - 2.0 * t).max(1.0);
    }
    if 2.0 * t + gap > room {
        t = ((room - gap) / 2.0).floor().max(1.0);
    }
    let lowest = bottom - (2.0 * t + gap);
    // Above the baseline only in a cell too shallow for the pair at one pixel each: the next row
    // gives way last.
    let y = preferred.max(wanted_top).min(lowest).max(first_clear.min(lowest));
    UnderlineRows {
        y,
        thickness: t,
        second: Some(y + t + gap),
    }
}

fn draw_underline(canvas: &Canvas, cell: Rect, baseline_y: f32, thickness: f32, kind: UnderlineKind, color: RgbColor) {
    let UnderlineRows { y, thickness, second } =
        underline_rows(cell, baseline_y, thickness, kind == UnderlineKind::Double);
    let paint = fill_paint(color);
    let bar = |canvas: &Canvas, y: f32| {
        canvas.draw_rect(Rect::from_xywh(cell.left, y, cell.width(), thickness), &paint);
    };
    match kind {
        UnderlineKind::None => {}
        UnderlineKind::Single => bar(canvas, y),
        // Two bars rather than one thick one: a double underline that renders as a single heavy rule
        // is indistinguishable from Single, which is the whole distinction the terminal drew.
        UnderlineKind::Double => {
            bar(canvas, y);
            if let Some(second) = second {
                bar(canvas, second);
            }
        }
        UnderlineKind::Curl | UnderlineKind::Dotted | UnderlineKind::Dashed => {
            // Dashes approximate all three patterned underline kinds and keep them distinct from Single.
            // A sine curl would need a path stroke.
            let period = (cell.width() / 3.0).max(2.0);
            let mut x = cell.left;
            while x < cell.right {
                let w = (period * 0.6).min(cell.right - x);
                canvas.draw_rect(Rect::from_xywh(x, y, w, thickness), &paint);
                x += period;
            }
        }
    }
}

fn draw_cursor(canvas: &Canvas, metrics: &TerminalMetrics, at: CellSpan, shape: CursorShape, color: RgbColor) {
    let cell = metrics.cell_rect(at.row, at.col, at.cols);
    let paint = fill_paint(color);
    let thin = (metrics.cell_width() * BEAM_FRACTION).max(1.0);
    match shape {
        CursorShape::Block => {
            canvas.draw_rect(cell, &paint);
        }
        CursorShape::Underline => {
            canvas.draw_rect(
                Rect::from_xywh(cell.left, cell.bottom - thin, cell.width(), thin),
                &paint,
            );
        }
        CursorShape::Beam => {
            canvas.draw_rect(Rect::from_xywh(cell.left, cell.top, thin, cell.height()), &paint);
        }
        CursorShape::HollowBlock => {
            // Stroked, leaving the inside alone -- the character under an unfocused cursor stays
            // readable, which is why the contract sends no `text_under` for this shape.
            let mut stroke = paint;
            stroke.set_style(skia_safe::paint::Style::Stroke);
            stroke.set_stroke_width(thin.max(1.0));
            canvas.draw_rect(cell.with_inset((thin / 2.0, thin / 2.0)), &stroke);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// Right-aligned in the top row, and in the palette's `CURSOR`/`BACKGROUND` colours reversed
    ///: the fill takes `CURSOR`, the text `BACKGROUND` -- the opposite pairing a
    /// filled cursor itself uses.
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

    /// An underline stays between its baseline and its cell's bottom, of every kind: it never reaches
    /// the row below, and never rises above the baseline into the letters. The geometries are
    /// Andale Mono's at 15 pt (a 17 px cell with the baseline at 13.6, where the rule once landed on
    /// the next row and a clamped double one then rose above the baseline), a DejaVu-like one, and
    /// a roomy one.
    #[test]
    fn an_underline_stays_between_its_baseline_and_its_cells_bottom() {
        for (height, baseline) in [(17.0_f32, 13.6_f32), (18.0, 14.1), (24.0, 17.0)] {
            let cell = Rect::from_xywh(0.0, 0.0, 9.0, height);
            let thickness = (cell.height() * RULE_THICKNESS).max(1.0);
            let rows = (height as usize) * 2;
            for kind in [
                UnderlineKind::Single,
                UnderlineKind::Double,
                UnderlineKind::Curl,
                UnderlineKind::Dotted,
                UnderlineKind::Dashed,
            ] {
                let mut surface = skia_safe::surfaces::raster_n32_premul(skia_safe::ISize::new(9, rows as i32))
                    .expect("raster surface");
                surface.canvas().clear(Color::BLACK);
                draw_underline(
                    surface.canvas(),
                    cell,
                    baseline,
                    thickness,
                    kind,
                    RgbColor::new(0xff, 0xff, 0xff),
                );
                let info = surface.image_info();
                let row_bytes = info.min_row_bytes();
                let mut pixels = vec![0u8; row_bytes * rows];
                assert!(surface.read_pixels(&info, &mut pixels, row_bytes, (0, 0)));
                let lit = |rows: std::ops::Range<usize>| {
                    rows.flat_map(|y| pixels[y * row_bytes..y * row_bytes + 9 * 4].chunks(4))
                        .filter(|px| px[..3] != [0, 0, 0])
                        .count()
                };
                let what = format!("{kind:?} in a {height} px cell, baseline {baseline}");
                assert!(lit(0..height as usize) > 0, "{what}: no rule drawn in its own cell");
                assert_eq!(lit(height as usize..rows), 0, "{what}: the rule reached the row below");
                // Every pixel row that starts above the baseline: rules are filled without
                // antialiasing, so a lit row the baseline crosses is a whole row of rule drawn over
                // the bottoms of the letters.
                assert_eq!(
                    lit(0..baseline.ceil() as usize),
                    0,
                    "{what}: the rule rose above the baseline"
                );
            }
        }
    }

    /// Where a double rule's bars go: the first a pixel clear below the baseline where the cell has
    /// room for it, the gap given up before that, and always a visible gap between the bars.
    #[test]
    fn a_double_underline_gives_up_its_gap_before_its_clearance() {
        let rows = |height: f32, baseline: f32| {
            let cell = Rect::from_xywh(0.0, 0.0, 9.0, height);
            underline_rows(cell, baseline, (height * RULE_THICKNESS).max(1.0), true)
        };
        // Andale Mono: 3.4 px below the baseline, so the pair starts on the first row under it.
        assert_eq!(
            rows(17.0, 13.6),
            UnderlineRows {
                y: 14.0,
                thickness: 1.0,
                second: Some(16.0)
            }
        );
        // DejaVu-like: the gap shrinks to a pixel and the pair still starts under the baseline.
        assert_eq!(
            rows(18.0, 14.1),
            UnderlineRows {
                y: 15.0,
                thickness: 1.0,
                second: Some(17.0)
            }
        );
        // Room for everything: a full gap, and the first bar a pixel clear of the baseline.
        assert_eq!(
            rows(24.0, 17.0),
            UnderlineRows {
                y: 18.0,
                thickness: 2.0,
                second: Some(22.0)
            }
        );
        for (height, baseline) in [(17.0, 13.6), (18.0, 14.1), (24.0, 17.0), (12.0, 11.5), (40.0, 30.2)] {
            let r = rows(height, baseline);
            let second = r.second.expect("a double rule has two bars");
            assert!(second >= r.y + r.thickness + 1.0, "{height}/{baseline}: {r:?}");
            assert!(second + r.thickness <= height, "{height}/{baseline}: {r:?}");
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
