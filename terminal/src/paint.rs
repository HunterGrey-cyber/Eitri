//! Moved from `terminal-pane/src/backend.rs` on `freeze/terminal-stack` @ `1e715ab` (2026-09-23),
//! unchanged below this paragraph but for rustfmt at this repository's `max_width = 120`. Where it
//! says "Verdandi" or "the handoff", the contract now lives in this workspace's `terminal-render/`.
//!
//! Executes a `PaintList` onto a Skia canvas. This is the entire backend.
//!
//! The rule from the handoff, and the reason this file is short:
//!
//! > A backend EXECUTES these ops. It never interprets terminal semantics.
//!
//! So there is no palette here, no SGR, no inverse, no selection flag, no wide-char spacer, no
//! cursor inference, no scrollback arithmetic. Every colour arrives resolved, every glyph arrives
//! with the column count it occupies, the cursor arrives with the character it covers already
//! recoloured to contrast. If a change to this file ever needs one of those concepts, the contract
//! has leaked and the fix belongs in Verdandi — send it back with a failing test rather than
//! deciding it here, because a second interpretation that merely happens to agree today is the
//! thing this architecture exists to prevent.
//!
//! Correctness first, deliberately: every visible cell is repainted every frame. No damage, no
//! partial redraw, no glyph atlas. That keeps the fault domain to the four things that can actually
//! be wrong at this stage — the op contract, cell metrics, Skia drawing, and pane clipping — and
//! there is no measurement yet saying a full repaint is too slow.

use crate::metrics::TerminalMetrics;
use skia_safe::{Canvas, Color, Paint, Rect, TextBlob};
use terminal_render::{CursorShape, GlyphStyle, PaintList, PaintOp, RgbColor, UnderlineKind};

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

    // Front to back, in the order given. `ops` is guaranteed non-decreasing in PaintLayer, so
    // honouring the given order IS honouring z-order -- this loop must never sort or batch by type.
    for op in &list.ops {
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
    let font = metrics.font_for(text.chars().next().unwrap_or(' '), style.bold, style.italic);

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

fn draw_underline(canvas: &Canvas, cell: Rect, baseline_y: f32, thickness: f32, kind: UnderlineKind, color: RgbColor) {
    let y = baseline_y + cell.height() * UNDERLINE_DROP;
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
            bar(canvas, y + thickness * 2.0);
        }
        UnderlineKind::Curl | UnderlineKind::Dotted | UnderlineKind::Dashed => {
            // Dashes stand in for all three patterned kinds for now: they are visibly distinct from
            // Single, which is what matters at this stage, and a real sine curl needs a path stroke
            // that is worth doing only once the rest is correct. Recorded rather than silently
            // approximated -- see this crate's MANUAL_VERIFICATION.md.
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
