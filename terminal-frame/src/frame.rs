//! THE CONTRACT.
//!
//! A narrow Verdandi presentation representation. Nothing here is an
//! `alacritty_terminal` or `vte` type: this is the long-term cross-project
//! protocol, and re-exporting an upstream internal would make every upstream
//! bump a protocol break.
//!
//! # ONE COORDINATE SYSTEM: ABSOLUTE GRID LINES
//!
//! Every `line` in this module -- [`RowUpdate::line`], [`FrameCursor::line`] --
//! is an **absolute grid line**, the coordinate system of
//! `alacritty_terminal::index::Point`: `0` is the topmost row of the *screen*,
//! `rows - 1` the bottom, and **negative** values are scrollback history.
//!
//! It is deliberately NOT the viewport row. `LineDamageBounds.line` (the only
//! viewport-row quantity upstream exposes) is converted exactly once, inside
//! [`crate::project`], and the viewport row never leaves that function. See
//! `Projector::next` for the conversion and the reason it is not the identity
//! when `display_offset != 0`.
//!
//! Chosen this way because absolute grid lines are the coordinate system of
//! `display_iter`'s `Indexed.point`, `RenderableCursor.point` and
//! `SelectionRange.start/end` -- damage is the lone exception -- and because
//! they are the only one of the two that can address scrollback, which the
//! read-only second view ([`crate::viewport`]) needs.
//!
//! # RENDERING POLICY
//!
//! Semantic flags are preserved; the renderer decides the visual result. No
//! "this state is always this RGB" rule is baked in anywhere. Colours are
//! carried as *palette indices* plus the override table; the client owns the
//! default palette and the resolve function.

use bitflags::bitflags;

/// A 24-bit colour. Ours, not `vte::ansi::Rgb`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }
}

/// Number of entries in the terminal colour palette.
///
/// Mirrors `alacritty_terminal::term::color::COUNT`. `tests/contract.rs`
/// asserts the two are equal, so an upstream change is a red test.
///
/// | index   | meaning           |
/// |---------|-------------------|
/// | 0..16   | named ANSI        |
/// | 16..232 | 6x6x6 colour cube |
/// | 232..256| greyscale ramp    |
/// | 256     | foreground        |
/// | 257     | background        |
/// | 258     | cursor            |
/// | 259..267| dim colours (DimBlack..DimWhite) |
/// | 267     | bright foreground |
/// | 268     | dim foreground    |
pub const PALETTE_LEN: usize = 269;

/// A cell colour.
///
/// REFINEMENT vs. upstream. `vte::ansi::Color` has three variants --
/// `Named(NamedColor)`, `Indexed(u8)`, `Spec(Rgb)`. The first two are collapsed
/// here into one `Palette(u16)`, because they already denote the same thing:
/// `Colors: Index<NamedColor>` resolves with `self.0[index as usize]`
/// (`term/color.rs`), and `NamedColor`'s discriminants ARE palette indices
/// (`Black = 0 .. BrightWhite = 15`, `Foreground = 256 .. DimForeground = 268`).
/// So `Named(Red)` and `Indexed(1)` index the same slot and resolve to the same
/// colour. Collapsing them makes the cell colour and the override table key
/// share one index space, which is what lets a single `Vec<ColorOverride>`
/// describe every OSC 4/10/11/12 change.
///
/// It also keeps the renderer's freedom intact: "bold makes ANSI 0..8 bright"
/// is a renderer rule over `Palette(i) + CellFlags::BOLD`, not something this
/// type decides.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FrameColor {
    /// Index into the `PALETTE_LEN`-entry palette. The client owns the defaults.
    Palette(u16),
    /// A direct 24-bit colour (SGR 38;2 / 48;2). Not affected by the palette.
    Rgb(Rgb),
}

bitflags! {
    /// The cell attribute bits.
    ///
    /// Bit-for-bit the same positions as `alacritty_terminal::term::cell::Flags`,
    /// but mapped **explicitly**, flag by flag, in [`crate::project`] rather than
    /// bit-cast: a bit-cast would silently re-interpret if upstream renumbered.
    /// `tests/contract.rs` asserts the mapping covers exactly `Flags::all()`.
    ///
    /// All 15 bits are carried, including the four that upstream only ever
    /// *writes* inside the crate (BOLD, DIM, ITALIC, HIDDEN) and the four that
    /// are emulation-load-bearing (WIDE_CHAR, WIDE_CHAR_SPACER,
    /// LEADING_WIDE_CHAR_SPACER, WRAPLINE -- reflow, search, selection, vi
    /// motion all read them). The renderer skips spacers at paint time but must
    /// still paint their background; that is a renderer rule, not a reason to
    /// drop the bit here.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
    pub struct CellFlags: u16 {
        const INVERSE                  = 1 << 0;
        const BOLD                     = 1 << 1;
        const ITALIC                   = 1 << 2;
        const UNDERLINE                = 1 << 3;
        const WRAPLINE                 = 1 << 4;
        const WIDE_CHAR                = 1 << 5;
        const WIDE_CHAR_SPACER         = 1 << 6;
        const DIM                      = 1 << 7;
        const HIDDEN                   = 1 << 8;
        const STRIKEOUT                = 1 << 9;
        const LEADING_WIDE_CHAR_SPACER = 1 << 10;
        const DOUBLE_UNDERLINE         = 1 << 11;
        const UNDERCURL                = 1 << 12;
        const DOTTED_UNDERLINE         = 1 << 13;
        const DASHED_UNDERLINE         = 1 << 14;
    }
}

/// Rarely-set cell attributes, boxed out of the common cell.
///
/// Same trick upstream plays with `Option<Arc<CellExtra>>`: on a real screen the
/// overwhelming majority of cells have neither combining marks nor a separate
/// underline colour, and paying 8 bytes for the `Option<Box<_>>` instead of
/// ~32 inline is the difference between a 24-byte and a 48-byte cell.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CellExtras {
    /// Combining marks riding on this cell.
    ///
    /// The shaping input for a cell is `[cell.c]` followed by these, so any
    /// glyph cache key must include them. Upstream reaches them through
    /// `Cell::zerowidth() -> Option<&[char]>`.
    pub zerowidth: Vec<char>,
    /// SGR 58 underline colour, independent of `fg`.
    pub underline_color: Option<FrameColor>,
}

/// One cell.
///
/// NOT carried, by decision (see the crate README "Known gaps"):
/// OSC 8 hyperlinks and SGR 5/6/25 blink.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameCell {
    pub c: char,
    pub fg: FrameColor,
    pub bg: FrameColor,
    pub flags: CellFlags,
    pub extra: Option<Box<CellExtras>>,
}

impl Default for FrameCell {
    fn default() -> Self {
        // Mirrors `alacritty_terminal::term::cell::Cell::default()`:
        // ' ' on Named(Background) with Named(Foreground).
        Self {
            c: ' ',
            fg: FrameColor::Palette(PALETTE_FOREGROUND),
            bg: FrameColor::Palette(PALETTE_BACKGROUND),
            flags: CellFlags::empty(),
            extra: None,
        }
    }
}

/// Palette index of the default foreground (`NamedColor::Foreground`).
pub const PALETTE_FOREGROUND: u16 = 256;
/// Palette index of the default background (`NamedColor::Background`).
pub const PALETTE_BACKGROUND: u16 = 257;
/// Palette index of the cursor colour (`NamedColor::Cursor`), set by OSC 12.
pub const PALETTE_CURSOR: u16 = 258;

impl FrameCell {
    /// Combining marks, or `&[]`.
    pub fn zerowidth(&self) -> &[char] {
        match &self.extra {
            Some(extra) => &extra.zerowidth,
            None => &[],
        }
    }

    pub fn underline_color(&self) -> Option<FrameColor> {
        self.extra.as_ref().and_then(|e| e.underline_color)
    }
}

/// Cursor shape.
///
/// REFINEMENT vs. upstream. `vte::ansi::CursorShape` has a fifth variant,
/// `Hidden`, and `RenderableCursor::new` folds "DECTCEM is off" into it --
/// which destroys the shape the application had chosen. Here visibility is a
/// separate `bool`, so a client can keep drawing the right shape when the
/// cursor comes back, and a semantic consumer can say "the cursor is at (r, c)
/// but hidden" instead of "there is no cursor".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum FrameCursorShape {
    #[default]
    Block,
    Underline,
    Beam,
    HollowBlock,
}

/// Cursor position and appearance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameCursor {
    /// ABSOLUTE grid line. See the module docs.
    pub line: i32,
    pub col: u16,
    pub shape: FrameCursorShape,
    /// DECTCEM (`CSI ? 25 h/l`).
    pub visible: bool,
    /// `Term::cursor_style().blinking`. **Not** available from
    /// `RenderableCursor`, which carries only shape and point -- this is the
    /// one field a damage-driven consumer cannot get from the renderable path
    /// at all.
    pub blinking: bool,
}

impl Default for FrameCursor {
    fn default() -> Self {
        Self {
            line: 0,
            col: 0,
            shape: FrameCursorShape::Block,
            visible: true,
            blinking: false,
        }
    }
}

/// The projected, stable subset of `TermMode`.
///
/// REFINEMENT vs. the starting shape: not raw bitflags, and deliberately a
/// **small** set. Every field here has a named consumer on the client side.
/// The input-encoding modes (`APP_CURSOR`, `APP_KEYPAD`, `BRACKETED_PASTE`, the
/// five kitty-keyboard bits, `SGR_MOUSE`/`UTF8_MOUSE`) are **absent on
/// purpose**: input is encoded engine-side by `terminal-input`, which consumes
/// the authoritative `TermMode` directly. Shipping a second copy of those bits
/// across the boundary would create exactly the "two declarations of the same
/// thing drift apart" seam this crate exists to avoid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TerminalModes {
    /// `TermMode::ALT_SCREEN`. The strongest single semantic signal there is:
    /// full-screen application vs. shell transcript.
    pub alt_screen: bool,
    /// `TermMode::LINE_WRAP` (DECAWM). Tells a semantic reader whether a
    /// `WRAPLINE` run is a continued logical line or a hard break.
    pub line_wrap: bool,
    /// `TermMode::INSERT` (IRM). Carried because it is also the reason the
    /// frame is Full: upstream's `Term::damage()` forces full damage while
    /// INSERT is set.
    pub insert: bool,
    /// `TermMode::ORIGIN` (DECOM). Cursor reports are scroll-region relative.
    pub origin: bool,
    /// Any of `TermMode::MOUSE_MODE`. The client decides local text selection
    /// vs. forwarding the event; without this it cannot.
    pub mouse_reporting: bool,
}

/// One palette override.
///
/// REFINEMENT vs. the starting shape's `Vec<(u16, Rgb)>`. OSC 104 / 110 / 111 /
/// 112 **reset** an override back to "no override", and a `(u16, Rgb)` pair
/// cannot say that. `color: None` means "this index is back to the client's
/// default".
///
/// `Term::colors()` is `[Option<Rgb>; 269]` and is entirely `None` on a fresh
/// `Term`: it stores overrides only. The frame must carry them, because cells
/// reference palette indices and nothing else records that index 4 is now
/// `#ff0000`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColorOverride {
    pub index: u16,
    pub color: Option<Rgb>,
}

/// A contiguous run of cells on one line.
///
/// `cells.len() == right - left + 1`, and `left <= right`. Enforced by
/// [`TerminalFrame::check`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowUpdate {
    /// ABSOLUTE grid line. See the module docs.
    pub line: i32,
    pub left: u16,
    pub right: u16,
    pub cells: Vec<FrameCell>,
}

impl RowUpdate {
    /// Number of cells this span covers.
    ///
    /// PRECONDITION: `left <= right`. [`TerminalFrame::validate`] rejects an
    /// inverted span (`FrameError::RowSpanInverted`) BEFORE it ever calls this,
    /// so the validated path cannot reach the violation -- but this method is
    /// `pub`, and on an inverted span the two profiles disagree: debug panics on
    /// overflow while release wraps to ~1.8e19. A caller that sized an allocation
    /// from that number would ask the allocator for 18 exabytes. Make the
    /// precondition explicit rather than leaving it to be rediscovered.
    pub fn width(&self) -> usize {
        debug_assert!(
            self.left <= self.right,
            "RowUpdate::width requires left <= right; got left={} right={} on line {}. \
             Validate the frame first -- FrameError::RowSpanInverted exists for this.",
            self.left,
            self.right,
            self.line,
        );
        (self.right as usize + 1).saturating_sub(self.left as usize)
    }
}

/// Whether this frame replaces the consumer's state or patches it.
///
/// REFINEMENT vs. the starting shape, and a hard requirement rather than a
/// nicety: without it, a consumer cannot tell "a delta that happened to touch
/// every row" from "a full snapshot", and therefore cannot know whether the
/// rows NOT listed are unchanged or blank. Those are different screens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameKind {
    /// Reset to an all-default screen of `cols x rows`, THEN apply
    /// `rows_changed`; replace the override table with `color_overrides`
    /// wholesale. Rows and cells not mentioned are default.
    ///
    /// Because the reset is part of the definition, a Full may omit rows that
    /// are entirely default and trim trailing default cells from the rows it
    /// does send. That is lossless here and only here -- see [`FrameKind::Delta`].
    Full,
    /// Patch `rows_changed` onto the state left by generation `generation - 1`;
    /// merge `color_overrides` into the existing table. Trailing-default
    /// trimming is UNSOUND for a delta (it would leave stale content behind),
    /// so a delta's spans are exactly what the projection decided changed.
    Delta,
}

/// One presentation frame.
///
/// Deltas are only valid applied in order: a consumer that sees
/// `generation != last + 1` has missed a frame and must ask for a Full.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalFrame {
    /// Monotonic, +1 per published frame, per projector.
    pub generation: u64,
    pub kind: FrameKind,
    /// Geometry in CELLS. Font metrics -> cell box -> (cols, rows) is
    /// RENDERER-owned; the renderer must drive BOTH `Term::resize` and the PTY
    /// `TIOCSWINSZ`, in that order, from one decision. See the crate README
    /// "Who drives resize".
    pub cols: u16,
    pub rows: u16,
    pub cursor: FrameCursor,
    /// Window focus. `Term::is_focused` is a public field this crate never
    /// reads -- it is client-parked state, so the client tells the frame, not
    /// the other way round. Set by [`crate::Projector::set_focused`].
    pub focused: bool,
    pub modes: TerminalModes,
    pub color_overrides: Vec<ColorOverride>,
    pub rows_changed: Vec<RowUpdate>,
}

/// A frame that violates the contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameError {
    /// `cells.len()` disagreed with `right - left + 1`.
    RowWidthMismatch {
        line: i32,
        left: u16,
        right: u16,
        cells: usize,
    },
    /// `left > right`, i.e. an empty span. Spans are inclusive and never empty.
    RowSpanInverted { line: i32, left: u16, right: u16 },
    /// A span ran past `cols`.
    RowSpanOutOfBounds { line: i32, right: u16, cols: u16 },
    /// A palette index >= [`PALETTE_LEN`].
    ColorIndexOutOfRange { index: u16 },
    /// Two updates for the same line in one frame. The apply order would then
    /// decide the result, which makes the frame ambiguous.
    DuplicateLine { line: i32 },
}

impl TerminalFrame {
    /// Structural self-check. Cheap; the differential test runs it on every
    /// frame it produces so a malformed frame is caught at the producer rather
    /// than being silently mis-applied at the consumer.
    pub fn check(&self) -> Result<(), FrameError> {
        let mut seen: Vec<i32> = Vec::with_capacity(self.rows_changed.len());
        for row in &self.rows_changed {
            if row.left > row.right {
                return Err(FrameError::RowSpanInverted {
                    line: row.line,
                    left: row.left,
                    right: row.right,
                });
            }
            if row.right >= self.cols {
                return Err(FrameError::RowSpanOutOfBounds {
                    line: row.line,
                    right: row.right,
                    cols: self.cols,
                });
            }
            if row.cells.len() != row.width() {
                return Err(FrameError::RowWidthMismatch {
                    line: row.line,
                    left: row.left,
                    right: row.right,
                    cells: row.cells.len(),
                });
            }
            if seen.contains(&row.line) {
                return Err(FrameError::DuplicateLine { line: row.line });
            }
            seen.push(row.line);
            for cell in &row.cells {
                check_color(cell.fg)?;
                check_color(cell.bg)?;
                if let Some(c) = cell.underline_color() {
                    check_color(c)?;
                }
            }
        }
        for over in &self.color_overrides {
            if over.index as usize >= PALETTE_LEN {
                return Err(FrameError::ColorIndexOutOfRange { index: over.index });
            }
        }
        Ok(())
    }

    /// Total cells carried. The denominator of every bytes/cell figure.
    pub fn cell_count(&self) -> usize {
        self.rows_changed.iter().map(|r| r.cells.len()).sum()
    }
}

fn check_color(color: FrameColor) -> Result<(), FrameError> {
    match color {
        FrameColor::Palette(index) if index as usize >= PALETTE_LEN => Err(FrameError::ColorIndexOutOfRange { index }),
        _ => Ok(()),
    }
}
