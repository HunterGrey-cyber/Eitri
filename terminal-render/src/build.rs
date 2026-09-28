//! `TerminalFrame` + view state -> [`PaintList`]. Every terminal decision is
//! made here so that none of them reaches a backend.

use terminal_frame::frame::{CellFlags, FrameCell, FrameColor, FrameCursorShape, TerminalFrame};

use crate::color::{resolve, to_rgb, Palette, BACKGROUND, CURSOR};
use crate::paint::{CursorShape, CursorText, GlyphStyle, PaintList, PaintOp, RgbColor, UnderlineKind};

/// What the view is doing. Position is carried separately by
/// [`RenderInput::window_top_line`] so there is exactly one source of truth for
/// it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewMode {
    /// Following the live bottom.
    FollowBottom,
    /// Holding a historical view, resolved through `RawViewport::top_line`.
    Pinned,
    /// The pinned anchor was evicted or destroyed. The frame supplied is the
    /// live bottom, and a [`PaintOp::DrawNotice`] says the pin is gone.
    ///
    /// This state exists so the transition cannot be silent. Rendering whatever
    /// now occupies the old position, while the user still believes the view is
    /// pinned, is the specific failure this forbids.
    AnchorExpired,
}

/// An inclusive run of selected columns on one ABSOLUTE grid line.
///
/// Selection is view state supplied beside the frame, never read out of it:
/// `TerminalFrame` deliberately excludes selection, and historical browsing and
/// selection stay separate concepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelectionSpan {
    pub line: i32,
    pub start_col: u16,
    pub end_col: u16,
}

/// Everything the builder consumes.
pub struct RenderInput<'a> {
    /// The window's cells, from `terminal_frame::project_window`.
    pub frame: &'a TerminalFrame,
    /// The absolute grid line that is the TOP of this window -- the same value
    /// passed to `project_window`, which for a pinned view is
    /// `RawViewport::top_line()`. This is what turns the frame's absolute lines
    /// into window rows, and it is why a backend never sees a negative line.
    pub window_top_line: i32,
    pub mode: ViewMode,
    pub selection: &'a [SelectionSpan],
    /// Window focus. VIEW state, not frame state: `project_window` cannot know
    /// it and hardcodes `focused: false`, so reading it off the frame would make
    /// every window permanently unfocused.
    pub focused: bool,
    pub palette: &'a Palette,
    /// Where the cursor's colour comes from when the program has not chosen one
    /// itself. A HOST decision -- whether it has a cursor colour to give -- so it
    /// rides beside the palette rather than being guessed from a slot's value.
    pub cursor_color: CursorColoring,
}

/// How the cursor is coloured when the frame carries no program-set cursor
/// colour (OSC 12). One set by the program always wins: it chose it for the
/// colours it paints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorColoring {
    /// A fixed colour, the palette's `CURSOR` slot, for the body; the covered
    /// cell's background for the character under a block. For a host that has a
    /// cursor colour to give.
    Palette,
    /// The covered cell's own colours, swapped: the body in the cell's
    /// foreground, the character under a block in its background. What foot and
    /// alacritty do when no cursor colour is configured, and the one rule that
    /// stays visible whatever colours a program paints under the cursor. A fixed
    /// colour taken from a host's theme is invisible on a cell the program
    /// painted near it -- a dark theme's pale foreground on LazyVim's light
    /// background, neovibe's GUI pass 2026-09-23, defect 3.
    CellInverse,
}

/// The words shown when a pinned view's anchor is gone.
///
/// Part of the contract rather than the backend's invention: a backend given a
/// bare signal has to make up wording, and one that makes up nothing renders the
/// silent substitution this state exists to prevent.
pub const ANCHOR_EXPIRED_NOTICE: &str = " pinned history no longer available -- showing live output ";

fn underline_kind(flags: CellFlags) -> UnderlineKind {
    // Most specific first: upstream can set UNDERLINE alongside a fancier style,
    // and drawing both would double-draw.
    if flags.contains(CellFlags::UNDERCURL) {
        UnderlineKind::Curl
    } else if flags.contains(CellFlags::DOUBLE_UNDERLINE) {
        UnderlineKind::Double
    } else if flags.contains(CellFlags::DOTTED_UNDERLINE) {
        UnderlineKind::Dotted
    } else if flags.contains(CellFlags::DASHED_UNDERLINE) {
        UnderlineKind::Dashed
    } else if flags.contains(CellFlags::UNDERLINE) {
        UnderlineKind::Single
    } else {
        UnderlineKind::None
    }
}

/// The shaping input for a cell: its char followed by any combining marks.
///
/// NOT `cell.c` alone. A combining mark is stored on the cell it modifies, so
/// dropping the zero-width run renders "e" where the terminal holds "é" -- text
/// corruption that no colour or layout test would catch.
fn cell_text(cell: &FrameCell) -> String {
    let mut s = String::with_capacity(1 + cell.zerowidth().len());
    s.push(cell.c);
    s.extend(cell.zerowidth());
    s
}

fn map_shape(shape: FrameCursorShape, focused: bool) -> CursorShape {
    // An unfocused window shows a hollow cursor whatever shape was chosen, which
    // is the near-universal terminal convention and, more to the point, a
    // decision that must be made SOMEWHERE -- making it here keeps focus
    // handling out of the backend.
    if !focused {
        return CursorShape::HollowBlock;
    }
    match shape {
        FrameCursorShape::Block => CursorShape::Block,
        FrameCursorShape::Underline => CursorShape::Underline,
        FrameCursorShape::Beam => CursorShape::Beam,
        FrameCursorShape::HollowBlock => CursorShape::HollowBlock,
    }
}

/// Does this shape cover the cell, so the character under it must be redrawn to
/// stay readable?
fn obscures_cell(shape: CursorShape) -> bool {
    matches!(shape, CursorShape::Block)
}

struct Resolved {
    text: String,
    fg: RgbColor,
    bg: RgbColor,
    style: GlyphStyle,
    cols: u16,
    /// A spacer or hidden cell: paint the background, draw no glyph.
    suppressed: bool,
}

fn resolve_cell(cell: &FrameCell, selected: bool, palette: &Palette) -> Resolved {
    let (mut fg, mut bg) = resolve(cell.fg, cell.bg, cell.flags, palette);
    // Selection swaps the RESOLVED pair, so it composes correctly with a cell
    // that is already SGR-inverse (two swaps return it to its original colours).
    if selected {
        std::mem::swap(&mut fg, &mut bg);
    }

    // A spacer is the second half of a full-width glyph, or the padding cell
    // before a wide char that could not fit at the end of a row. Suppressing it
    // HERE is what lets the contract promise "no op is ever emitted for the
    // second column" -- so the backend has no spacer concept at all.
    //
    // Note this is not covered by the blank-cell skip below: a spacer inherits
    // the wide char's SGR, so `SGR 4;9` leaves it ' ' with UNDERLINE | STRIKEOUT,
    // which is not blank by the decoration test, and without this rule it paints
    // a second underlined struck-through run beside the glyph.
    let spacer = cell
        .flags
        .intersects(CellFlags::WIDE_CHAR_SPACER | CellFlags::LEADING_WIDE_CHAR_SPACER);
    // HIDDEN (SGR 8) keeps the cell's background and suppresses its text.
    let hidden = cell.flags.contains(CellFlags::HIDDEN);

    let text = cell_text(cell);
    let underline = underline_kind(cell.flags);
    let decorated = underline != UnderlineKind::None || cell.flags.contains(CellFlags::STRIKEOUT);
    // Checked on the TEXT, not on `cell.c`, so a space carrying a combining mark
    // is still painted.
    let blank = text == " " && !decorated;

    let underline_color = match cell.underline_color() {
        Some(c) => to_rgb(c, palette),
        // SGR 58 absent: the underline takes the glyph's colour. Resolved here
        // so the contract carries a colour rather than an Option the backend
        // would have to interpret.
        None => fg,
    };

    Resolved {
        text,
        fg,
        bg,
        style: GlyphStyle {
            bold: cell.flags.contains(CellFlags::BOLD),
            italic: cell.flags.contains(CellFlags::ITALIC),
            underline,
            underline_color,
            strikeout: cell.flags.contains(CellFlags::STRIKEOUT),
        },
        // ONE glyph, TWO cells. The advance comes from the terminal's own width
        // accounting, never from counting scalars in `text`.
        cols: if cell.flags.contains(CellFlags::WIDE_CHAR) {
            2
        } else {
            1
        },
        suppressed: spacer || hidden || blank,
    }
}

/// Build the complete paint list. Full redraw, every visible cell: no damage, no
/// partial ranges, no bookkeeping.
pub fn build_paint_list(input: &RenderInput<'_>) -> PaintList {
    let frame = input.frame;
    let top = input.window_top_line;
    let rows = frame.rows;
    let cols = frame.cols;

    // Absolute grid line -> window row. The ONE place this conversion happens,
    // which is what keeps absolute and negative line numbers out of the op
    // stream entirely.
    let to_row = |line: i32| -> Option<u16> {
        let r = line.checked_sub(top)?;
        if r < 0 || r >= rows as i32 {
            return None;
        }
        Some(r as u16)
    };

    let selected = |line: i32, col: u16| {
        input
            .selection
            .iter()
            .any(|s| s.line == line && col >= s.start_col && col <= s.end_col)
    };

    let mut backgrounds = Vec::new();
    let mut texts = Vec::new();

    for row_update in &frame.rows_changed {
        let Some(row) = to_row(row_update.line) else { continue };
        for (i, cell) in row_update.cells.iter().enumerate() {
            let col = row_update.left + i as u16;
            if col >= cols {
                continue;
            }
            let r = resolve_cell(cell, selected(row_update.line, col), input.palette);
            backgrounds.push(PaintOp::FillCells {
                row,
                col,
                cols: 1,
                color: r.bg,
            });
            if !r.suppressed {
                texts.push(PaintOp::DrawText {
                    row,
                    col,
                    cols: r.cols,
                    text: r.text,
                    color: r.fg,
                    style: r.style,
                });
            }
        }
    }

    // ---- selection over cells the frame did not supply ----
    //
    // sw-terminal-6. The frame only carries the cells `Projector` chose to report: a wholly
    // blank row is dropped from `rows_changed` entirely (`full_rows` skips a line with no
    // non-default cell at all), and a short row is truncated right after its last non-default
    // cell. Both are ordinary, unselected DEFAULT cells in the terminal's own model -- a
    // selection spanning past what was supplied must still highlight them, or the live
    // (unscrolled) view stops short of where the actual selection (and its copied text) extends,
    // while the scrolled-back view (`project_window`, which always supplies every column of
    // every row) draws it correctly. Fill exactly the DEFAULT cell's own selected colour, so the
    // highlight is seamless with what the cell loop above already drew for a covered default
    // cell in the same span.
    if !input.selection.is_empty() {
        let default_selected = resolve_cell(&FrameCell::default(), true, input.palette);
        for span in input.selection {
            let Some(row) = to_row(span.line) else { continue };
            let covered = frame.rows_changed.iter().find(|r| r.line == span.line);
            let (covered_left, covered_right) = covered
                .map(|r| (r.left, r.left + r.cells.len() as u16))
                .unwrap_or((0, 0));
            let end_col = span.end_col.min(cols.saturating_sub(1));
            if span.start_col > end_col {
                continue;
            }
            for col in span.start_col..=end_col {
                if col >= covered_left && col < covered_right {
                    continue; // already painted by the cell loop above
                }
                backgrounds.push(PaintOp::FillCells {
                    row,
                    col,
                    cols: 1,
                    color: default_selected.bg,
                });
            }
        }
    }

    let mut ops = backgrounds;
    ops.append(&mut texts);

    // ---- cursor ----
    // From the frame's authoritative cursor, never from the last glyph emitted.
    if frame.cursor.visible {
        if let Some(row) = to_row(frame.cursor.line) {
            let col = frame.cursor.col;
            if col < cols {
                let covered = frame
                    .rows_changed
                    .iter()
                    .find(|r| r.line == frame.cursor.line)
                    .and_then(|r| col.checked_sub(r.left).and_then(|i| r.cells.get(i as usize)));
                let resolved = covered.map(|c| resolve_cell(c, selected(frame.cursor.line, col), input.palette));
                let shape = map_shape(frame.cursor.shape, input.focused);
                // A program's own OSC 12 is in the frame's overrides; the palette
                // has it applied too, so `CURSOR` is then the program's colour.
                let program_set = frame
                    .color_overrides
                    .iter()
                    .any(|o| o.index == CURSOR && o.color.is_some());
                let cursor_color = match (&resolved, input.cursor_color) {
                    (Some(r), CursorColoring::CellInverse) if !program_set => r.fg,
                    _ => input.palette.get(CURSOR),
                };

                // A block cursor fills the cell, so the character under it would
                // vanish. Re-resolve it against the cursor's own colour and hand
                // that over already done -- the backend composites nothing and
                // inverts nothing.
                let text_under = match &resolved {
                    Some(r) if obscures_cell(shape) && !r.suppressed => Some(CursorText {
                        text: r.text.clone(),
                        color: r.bg,
                        style: GlyphStyle {
                            underline_color: r.bg,
                            ..r.style
                        },
                    }),
                    _ => None,
                };

                ops.push(PaintOp::DrawCursor {
                    row,
                    col,
                    cols: resolved.as_ref().map(|r| r.cols).unwrap_or(1),
                    shape,
                    color: cursor_color,
                    text_under,
                    blinking: frame.cursor.blinking,
                });
            }
        }
    }

    // ---- overlay ----
    if input.mode == ViewMode::AnchorExpired {
        // Row 0 of the window, always: a window-relative row cannot be outside
        // the window, which the old absolute-line fallback could be.
        ops.push(PaintOp::DrawNotice {
            row: 0,
            col: 0,
            cols,
            text: ANCHOR_EXPIRED_NOTICE.to_owned(),
            color: input.palette.get(BACKGROUND),
            background: input.palette.get(crate::color::FOREGROUND),
        });
    }

    PaintList {
        ops,
        cols,
        rows,
        surface_background: input.palette.get(BACKGROUND),
        top_line: top,
    }
}

/// Convenience for callers that have no palette overrides of their own.
pub fn palette_for(frame: &TerminalFrame) -> Palette {
    let mut p = Palette::xterm_default();
    p.apply_overrides(&frame.color_overrides);
    p
}

/// Resolve a frame colour against a palette. Exposed because `RenderInput`
/// builders sometimes need it; backends never do.
pub fn resolve_frame_color(color: FrameColor, palette: &Palette) -> RgbColor {
    to_rgb(color, palette)
}
