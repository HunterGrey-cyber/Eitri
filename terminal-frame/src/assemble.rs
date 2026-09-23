//! THE CONSUMER SIDE. Frames -> a screen.
//!
//! This is what the renderer and the semantic interpreter each hold. It exists
//! in this crate for three reasons:
//!
//! * it makes [`crate::FrameKind`]'s meaning executable rather than prose --
//!   "Full = reset then apply, Delta = patch" is code here;
//! * it is what `tests/delta_equals_full.rs` drives -- the only test that can
//!   catch an uncompensated damage under-report -- against the terminal itself;
//! * a consumer that rolls its own would be a second declaration of the
//!   contract, free to drift.

use crate::frame::{ColorOverride, FrameCell, FrameCursor, FrameKind, Rgb, TerminalFrame, TerminalModes, PALETTE_LEN};

/// Why a frame could not be applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplyError {
    /// A delta arrived out of order. The consumer has missed a frame and must
    /// ask for a Full; applying this one would silently corrupt the screen.
    GenerationGap { expected: u64, got: u64 },
    /// A delta changed the geometry. Only a Full may resize.
    DeltaResized { from: (u16, u16), to: (u16, u16) },
    /// A row update fell outside `0..rows`.
    LineOutOfBounds { line: i32, rows: u16 },
    /// The frame failed its own structural check.
    Malformed(crate::frame::FrameError),
}

/// A consumer's reconstruction of the screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameAssembler {
    cols: u16,
    rows: u16,
    cells: Vec<FrameCell>,
    cursor: FrameCursor,
    focused: bool,
    modes: TerminalModes,
    palette: Vec<Option<Rgb>>,
    generation: u64,
    applied: u64,
}

impl Default for FrameAssembler {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameAssembler {
    pub fn new() -> Self {
        Self {
            cols: 0,
            rows: 0,
            cells: Vec::new(),
            cursor: FrameCursor::default(),
            focused: false,
            modes: TerminalModes::default(),
            palette: vec![None; PALETTE_LEN],
            generation: 0,
            applied: 0,
        }
    }

    pub fn cols(&self) -> u16 {
        self.cols
    }

    pub fn rows(&self) -> u16 {
        self.rows
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Frames successfully applied. Distinct from `generation`, which is the
    /// producer's counter.
    pub fn applied(&self) -> u64 {
        self.applied
    }

    pub fn cursor(&self) -> FrameCursor {
        self.cursor
    }

    pub fn focused(&self) -> bool {
        self.focused
    }

    pub fn modes(&self) -> TerminalModes {
        self.modes
    }

    /// The override table. `None` means "the client's own default".
    pub fn palette(&self) -> &[Option<Rgb>] {
        &self.palette
    }

    /// The cell at an ABSOLUTE grid line, or `None` outside the screen.
    ///
    /// The column check is load-bearing and the line check is NOT, which is why
    /// only one of them is written: `cells` is `rows * cols` long and row-major,
    /// so `line >= rows` is already out of range for `get`, and a negative line
    /// cannot survive `usize::try_from`. `col >= cols`, by contrast, is a
    /// perfectly valid index into the NEXT row -- dropping that check would
    /// silently return a cell from the wrong line. Writing the redundant check
    /// anyway would leave a mutant no test could kill.
    pub fn cell(&self, line: i32, col: u16) -> Option<&FrameCell> {
        if col >= self.cols {
            return None;
        }
        let row = usize::try_from(line).ok()?;
        self.cells.get(row * self.cols as usize + col as usize)
    }

    /// The whole screen, row-major, `rows * cols` long.
    pub fn cells(&self) -> &[FrameCell] {
        &self.cells
    }

    /// One row as plain text, trailing blanks kept. What a semantic reader sees.
    pub fn line_text(&self, line: i32) -> String {
        let mut out = String::new();
        for col in 0..self.cols {
            if let Some(cell) = self.cell(line, col) {
                out.push(cell.c);
                for mark in cell.zerowidth() {
                    out.push(*mark);
                }
            }
        }
        out
    }

    pub fn screen_text(&self) -> String {
        (0..self.rows as i32)
            .map(|line| self.line_text(line).trim_end().to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Apply one frame.
    ///
    /// ATOMIC: every check happens before any mutation, so a refused frame
    /// leaves the screen exactly as it was. The first version validated row
    /// bounds inside the write loop, which meant a `Full` whose rows were out
    /// of range had already blanked the screen by the time it was rejected --
    /// the consumer then held neither the old screen nor the new one.
    /// `contract.rs::a_row_update_outside_the_screen_is_refused_rather_than_panicking`
    /// pins it.
    pub fn apply(&mut self, frame: &TerminalFrame) -> Result<(), ApplyError> {
        frame.check().map_err(ApplyError::Malformed)?;

        if frame.kind == FrameKind::Delta {
            if self.applied != 0 && frame.generation != self.generation + 1 {
                return Err(ApplyError::GenerationGap {
                    expected: self.generation + 1,
                    got: frame.generation,
                });
            }
            if (frame.cols, frame.rows) != (self.cols, self.rows) {
                return Err(ApplyError::DeltaResized {
                    from: (self.cols, self.rows),
                    to: (frame.cols, frame.rows),
                });
            }
        }

        for row in &frame.rows_changed {
            if row.line < 0 || row.line >= frame.rows as i32 {
                return Err(ApplyError::LineOutOfBounds {
                    line: row.line,
                    rows: frame.rows,
                });
            }
        }

        // ---- nothing below this line can fail ----

        if frame.kind == FrameKind::Full {
            self.cols = frame.cols;
            self.rows = frame.rows;
            let len = frame.cols as usize * frame.rows as usize;
            self.cells.clear();
            self.cells.resize(len, FrameCell::default());
            // A Full REPLACES the override table. Anything not listed is back
            // to the client's default -- which is exactly what a fresh
            // `Term::colors()` (all `None`) means.
            self.palette.clear();
            self.palette.resize(PALETTE_LEN, None);
        }

        for over in &frame.color_overrides {
            let ColorOverride { index, color } = *over;
            self.palette[index as usize] = color;
        }

        for row in &frame.rows_changed {
            let base = row.line as usize * self.cols as usize;
            for (offset, cell) in row.cells.iter().enumerate() {
                self.cells[base + row.left as usize + offset] = cell.clone();
            }
        }

        self.cursor = frame.cursor;
        self.focused = frame.focused;
        self.modes = frame.modes;
        self.generation = frame.generation;
        self.applied += 1;
        Ok(())
    }
}
