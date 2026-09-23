//! THE RENDERER CONTRACT. Frozen surface between Verdandi and any backend.
//!
//! # The one rule
//!
//! A backend EXECUTES these ops. It never interprets terminal semantics. If a
//! backend author ever has to ask "is this a wide-char spacer?", "should inverse
//! swap these colours?", "is this cell selected?", "is this the cursor cell?" or
//! "what does a negative line mean?", the contract has leaked and the bug is
//! HERE, not in the backend.
//!
//! Everything in this module is self-contained: no type from `terminal-frame`,
//! `alacritty_terminal` or `vte` appears in the public API, so a backend depends
//! on this crate alone and needs no terminal vocabulary to compile against it.
//!
//! # Coordinates: ONE space, cells, window-relative
//!
//! Every op addresses cells of the VISIBLE WINDOW:
//!
//! ```text
//!   row  0 .. rows     0 is the TOP row of the window. Never negative.
//!   col  0 .. cols     0 is the LEFT column. Never negative.
//!   cols               how many columns the op spans (1, or 2 for a wide glyph)
//! ```
//!
//! There are no absolute grid lines and no scrollback numbers in the op stream.
//! Verdandi resolves those before you see them, which is why a pinned historical
//! view and a live view are byte-identical in shape. [`PaintList::top_line`]
//! records which absolute grid line row 0 came from, for provenance and logging
//! ONLY -- a backend never needs it to paint.
//!
//! Pixels are the backend's, through its own cell metrics:
//!
//! ```text
//!   x = col * cell_width
//!   y = row * cell_height
//!   w = cols * cell_width
//!   h = cell_height
//! ```
//!
//! Verdandi has no opinion on fonts, cell size, baseline or DPI and carries none
//! of them. It emits cells; the backend owns the grid-to-pixel transform, and
//! must use ONE metrics object for every op so backgrounds, glyphs, cursor and
//! overlays land on the same lattice.
//!
//! # Z-order is fixed, not a backend choice
//!
//! [`PaintList::ops`] is emitted in strictly non-decreasing [`PaintLayer`] order,
//! and executing it front-to-back is correct by construction. A backend that
//! batches by type must still honour the layer order.
//!
//! ```text
//!   0 CellBackground   every cell's fill, selection already resolved into it
//!   1 Text             glyphs, with their underline and strikeout
//!   2 Cursor           the cursor, with the character it covers already re-resolved
//!   3 Overlay          view-state notices (anchor expired)
//! ```
//!
//! Within a single [`PaintOp::DrawText`] the order is: glyph, then underline,
//! then strikeout.
//!
//! # Before the first op
//!
//! Clear the whole surface with [`PaintList::surface_background`]. Ops do not
//! necessarily cover every cell, and the surface is usually not an exact
//! multiple of the cell size, so the remainder has to come from somewhere; this
//! is that colour, and it is the terminal's own resolved default background.

/// A fully resolved 24-bit colour. Never a palette index: the backend owns no
/// palette and never resolves one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct RgbColor {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl RgbColor {
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }
}

/// How to draw the cursor body.
///
/// These are DRAWING shapes, not terminal modes -- a backend needs no terminal
/// knowledge to execute them.
///
/// ```text
///   Block        fill the whole cell box
///   Underline    fill a thin bar along the bottom edge
///   Beam         fill a thin bar along the left edge
///   HollowBlock  stroke the outline of the cell box, leave the inside alone
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum CursorShape {
    #[default]
    Block,
    Underline,
    Beam,
    HollowBlock,
}

/// Which underline to draw. Already reduced to one choice: the terminal's
/// independent style bits are resolved upstream, so these are mutually
/// exclusive and a backend never combines them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum UnderlineKind {
    #[default]
    None,
    Single,
    Double,
    Curl,
    Dotted,
    Dashed,
}

/// Text decoration, fully resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct GlyphStyle {
    /// Use a bold face if the font has one. The COLOUR is already resolved --
    /// never brighten a colour because this is set.
    pub bold: bool,
    pub italic: bool,
    pub underline: UnderlineKind,
    /// The underline's colour. ALWAYS a concrete colour, never optional: when
    /// the terminal specified none it is already set to the glyph's own colour,
    /// so a backend draws the underline with this and asks nothing.
    pub underline_color: RgbColor,
    /// Draw a line through the middle. Uses the glyph's colour.
    pub strikeout: bool,
}

/// The character the cursor covers, re-resolved so it stays readable on top of
/// the cursor body.
///
/// The backend draws the cursor shape first and this on top. It does NOT invert
/// anything, and does not need the original cell -- the colour here is already
/// the contrasting one. `None` means draw nothing over the cursor, which is the
/// case for shapes that do not obscure the cell.
#[derive(Debug, Clone, PartialEq)]
pub struct CursorText {
    pub text: String,
    pub color: RgbColor,
    pub style: GlyphStyle,
}

/// Paint order. See the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum PaintLayer {
    CellBackground = 0,
    Text = 1,
    Cursor = 2,
    Overlay = 3,
}

/// One instruction. All coordinates are window cells; all colours are resolved.
#[derive(Debug, Clone, PartialEq)]
pub enum PaintOp {
    /// Fill `cols` columns of `row` with a solid colour.
    ///
    /// Emitted for every cell the frame covers, including the second half of a
    /// wide glyph and cells whose text is hidden -- a cell's background is
    /// visible even when its glyph is not. Selection is already resolved into
    /// this colour.
    FillCells {
        row: u16,
        col: u16,
        cols: u16,
        color: RgbColor,
    },

    /// Draw `text` starting at `col`, occupying exactly `cols` columns.
    ///
    /// `text` is ONE grapheme to place in that box. It may be several Unicode
    /// scalars (a base character plus combining marks); shape it as a unit and
    /// do not count scalars to decide width -- `cols` is the width, and it is
    /// the terminal's own accounting.
    ///
    /// `cols == 2` is a full-width glyph. Draw ONE glyph spanning both columns.
    /// No separate op is ever emitted for the second column, so there is nothing
    /// to suppress and no spacer concept to know about.
    DrawText {
        row: u16,
        col: u16,
        cols: u16,
        text: String,
        color: RgbColor,
        style: GlyphStyle,
    },

    /// Draw the cursor. Emitted only when it is visible AND inside the window,
    /// so its presence alone is the whole decision -- there is no hidden state
    /// to consult.
    ///
    /// Draw `shape` in `color`, then `text_under` on top if present. `blinking`
    /// is advisory: a backend may ignore it and draw a steady cursor.
    DrawCursor {
        row: u16,
        col: u16,
        cols: u16,
        shape: CursorShape,
        color: RgbColor,
        text_under: Option<CursorText>,
        blinking: bool,
    },

    /// A view-state message drawn over the content, fully specified so a backend
    /// invents nothing.
    ///
    /// Fill `cols` columns of `row` with `background`, then draw `text` in
    /// `color`, clipped to that box. This is the only op that carries prose, and
    /// it must never be skipped: it is how the user is told the view they pinned
    /// no longer exists. See [`crate::build::ViewMode::AnchorExpired`].
    DrawNotice {
        row: u16,
        col: u16,
        cols: u16,
        text: String,
        color: RgbColor,
        background: RgbColor,
    },
}

impl PaintOp {
    /// The layer this op belongs to. Derivable from the variant alone, which is
    /// what makes the ordering guarantee machine-checkable rather than prose.
    pub fn layer(&self) -> PaintLayer {
        match self {
            PaintOp::FillCells { .. } => PaintLayer::CellBackground,
            PaintOp::DrawText { .. } => PaintLayer::Text,
            PaintOp::DrawCursor { .. } => PaintLayer::Cursor,
            PaintOp::DrawNotice { .. } => PaintLayer::Overlay,
        }
    }
}

/// One frame's worth of instructions.
#[derive(Debug, Clone, PartialEq)]
pub struct PaintList {
    /// In non-decreasing [`PaintLayer`] order. Execute front to back.
    pub ops: Vec<PaintOp>,
    /// Window size in cells. Every op lies inside `0..rows` x `0..cols`.
    pub cols: u16,
    pub rows: u16,
    /// Clear the surface with this before executing any op. The terminal's
    /// resolved default background.
    pub surface_background: RgbColor,
    /// PROVENANCE ONLY. The absolute grid line that became `row` 0 -- negative
    /// when the window is scrolled into history. Useful in logs and for
    /// correlating a frame with `RawViewport`; never needed to paint, and a
    /// backend that does arithmetic with it is reimplementing scrollback.
    pub top_line: i32,
}

impl PaintList {
    /// True when `ops` is in non-decreasing layer order.
    pub fn is_layer_ordered(&self) -> bool {
        self.ops.windows(2).all(|w| w[0].layer() <= w[1].layer())
    }

    /// The text painted on one window row, columns in order. A reading of what
    /// the user sees, for tests and logs.
    pub fn row_text(&self, row: u16) -> String {
        let mut cells: Vec<(u16, &str)> = self
            .ops
            .iter()
            .filter_map(|op| match op {
                PaintOp::DrawText { row: r, col, text, .. } if *r == row => Some((*col, text.as_str())),
                _ => None,
            })
            .collect();
        cells.sort_by_key(|(c, _)| *c);
        cells.into_iter().map(|(_, t)| t).collect()
    }

    pub fn cursor(&self) -> Option<&PaintOp> {
        self.ops.iter().find(|op| matches!(op, PaintOp::DrawCursor { .. }))
    }

    pub fn notice(&self) -> Option<&PaintOp> {
        self.ops.iter().find(|op| matches!(op, PaintOp::DrawNotice { .. }))
    }

    pub fn has_expiry_notice(&self) -> bool {
        self.notice().is_some()
    }
}
