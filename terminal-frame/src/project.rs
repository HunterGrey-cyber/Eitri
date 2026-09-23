//! THE PROJECTION. An authoritative `Term` -> [`TerminalFrame`].
//!
//! # ONE OWNER OF DAMAGE
//!
//! `Term::damage()` / `Term::reset_damage()` are a **global, destructive** pair.
//! `reset_damage()` clears the state for everybody, so two consumers calling it
//! destroy each other's signal. [`Projector::next`] is the only place in the
//! engine that may call either, and it calls them exactly once per frame; the
//! resulting frame is fanned out to the renderer and the semantic interpreter.
//!
//! `damage()` takes `&mut self` and the returned `TermDamage<'_>` extends that
//! mutable borrow, so the grid cannot be read while it is held. The bounds are
//! therefore **collected first** into a plain `Vec`, the borrow dropped,
//! `reset_damage()` called, and only then is the grid read.
//!
//! # DAMAGE UNDER-REPORTS, AND WHAT WE DO ABOUT IT
//!
//! Damage is an optimisation hint, not a change log. Every compensation below
//! is pinned by a test in `tests/damage.rs` that FAILS without it, and the
//! whole set is pinned by `tests/delta_equals_full.rs`, whose negative control
//! runs the same corpora with compensation disabled and asserts it breaks.
//!
//! 1. **Combining marks.** `Term::input` handles a zero-width char by
//!    `push_zerowidth` onto a *previous* column and `return`s -- no damage call
//!    at all. The mark lands one column left of the cursor, or **two** when the
//!    previous cell is a `WIDE_CHAR_SPACER`.
//! 2. **The cross-line `LEADING_WIDE_CHAR_SPACER` clear.** `write_at_cursor`
//!    clears that flag on `grid[point.line - 1][last_column]` and damages
//!    nothing. That cell is on a line that may carry NO damage at all, so no
//!    span policy can reach it -- hence its own knob,
//!    [`Compensation::previous_line_last_column`].
//! 3. **The spacer-half overwrite.** The same block clears the wide char to the
//!    LEFT (`point.column - 1`, `clear_wide()`) and the spacer flag to the
//!    RIGHT (`point.column + 1`), again with no damage call.
//! 4. **`Term::grid_mut()`.** A public `&mut Grid` with no damage bookkeeping
//!    whatsoever. The engine does not call it; anything that does must call
//!    [`Projector::force_full`]. There is no way to detect it from here.
//! 5. **In-place writes below the fold while `display_offset != 0`.**
//!    `TermDamageIterator::new` truncates the damage array to
//!    `len - display_offset`, silently dropping the bottom `display_offset`
//!    screen lines. Compensated by re-damaging exactly those lines
//!    ([`Compensation::below_the_fold`]).
//!
//!    In this engine `display_offset` is *structurally* always 0 -- `Grid`
//!    raises it only from `Grid::scroll_display` or from `scroll_up` when it is
//!    *already* non-zero, and the architecture forbids `Term::scroll_display`.
//!    The compensation is still implemented and tested, because "structurally
//!    impossible" is a claim that should cost nothing to be wrong about.
//! 6. **Everything written to the right of where the cursor ended the frame.**
//!    NOT in the original list. `tests/delta_equals_full.rs` found it; the
//!    shrinker reduced it to four escapes.
//!
//!    `Term::input` -- the function that writes every printable character --
//!    contains **no damage call whatsoever**, and neither does `Term::put_tab`
//!    (which also *writes*, substituting `'\t'` into a blank cell). A printed
//!    run is covered only because `Term::damage()` damages the cursor point
//!    from the *previous* call and the cursor point now, and
//!    `LineDamageBounds::expand` takes min/max, so the two endpoints span the
//!    run between them. That argument collapses the moment the cursor ends the
//!    frame LEFT of the rightmost cell it wrote.
//!
//!    Wrapping usually covers itself -- `wrapline` calls `damage_cursor()`
//!    before moving, and `linefeed()` either scrolls (full damage) or moves the
//!    line with `damage_cursor()` on both sides. But `wrapline` SKIPS its
//!    pre-move `damage_cursor()` on the `linefeed()` path, and `linefeed()` is
//!    a **no-op** when the cursor sits on the bottom screen line while the
//!    scroll region ends above it (`cursor.line + 1 == scroll_region.end` is
//!    false and `cursor.line < bottommost_line()` is false). The cursor then
//!    snaps to column 0 of the same line, leaving the right-hand half of it
//!    undamaged. Four escapes reach it:
//!
//!    ```text
//!    ESC[4;5r   ESC[7;21H   ESC[3B   "tab<TAB>here"
//!    ```
//!
//!    DECSTBM homing the cursor reaches the same gap from a different
//!    direction, and there the write is not even on the cursor's final line.
//!
//!    **This is why [`SpanPolicy::FullLine`] is the production policy.** The
//!    gap is as wide as the line, so no fixed widening bounds it. Measured over
//!    286 corpora:
//!
//!    | policy | corpora that diverge |
//!    |---|---|
//!    | raw damage | 218 |
//!    | widen by 1 each side (the original rule) | 124 |
//!    | widen by 2 left / 1 right | 123 |
//!    | **FullLine** | **0** |
//!
//! Damage also **over-reports**: `move_forward` damages every column it passes
//! (`term/mod.rs:1238`) whether or not it wrote them. Over-reporting is free --
//! it costs bandwidth, never correctness -- so nothing is done about it.
//!
//! # WHAT IS NOT COMPENSATED, BECAUSE UPSTREAM ALREADY DOES IT
//!
//! `TermMode::INSERT` -> Full. `Term::damage()` starts with
//! `if self.mode.contains(TermMode::INSERT) { self.mark_fully_damaged(); }`
//! (`term/mod.rs:461`) and the *leaving* edge is handled in `unset_mode`
//! (`term/mod.rs:2128`). Duplicating that check here would be dead code and an
//! equivalent mutant. It is pinned instead by
//! `tests/damage.rs::upstream_forces_full_damage_under_insert_mode`, which goes
//! red if upstream stops doing it.
//!
//! # WHAT DAMAGE EXCLUDES ENTIRELY
//!
//! Upstream's own doc comment: "The user controlled elements, like `Vi` mode
//! cursor and `Selection` are **not** part of the collected damage state."
//! Neither are cursor shape, cursor visibility, OSC 12 cursor colour, or the
//! window title. A damage-driven consumer must diff those itself -- which is
//! why this projector puts the cursor, the modes and the colour overrides into
//! **every** frame, Full and Delta alike, and diffs the overrides itself.

use alacritty_terminal::grid::{Dimensions, Grid};
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::color::Colors;
use alacritty_terminal::term::{Term, TermDamage, TermMode};
use alacritty_terminal::vte::ansi::{Color, CursorShape, NamedColor, Rgb as VteRgb};

use crate::frame::{
    CellExtras, CellFlags, ColorOverride, FrameCell, FrameColor, FrameCursor, FrameCursorShape, FrameKind, Rgb,
    RowUpdate, TerminalFrame, TerminalModes, PALETTE_LEN,
};

/// How far a reported damage span is widened before it is trusted.
///
/// MEASURED, NOT CHOSEN. `tests/delta_equals_full.rs::report_what_each_compensation_is_worth`
/// runs both policies over both corpora and prints how many diverge;
/// `compensation_is_load_bearing` pins the numbers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpanPolicy {
    /// PRODUCTION. Every line that damage mentions at all is carried whole.
    ///
    /// This is what it takes. `Term::input` -- the function that writes every
    /// printable character -- emits NO damage; a printed run is covered only
    /// because `Term::damage()` damages the cursor point from the previous call
    /// and the cursor point now, and `LineDamageBounds::expand` spans between
    /// them. The moment the cursor ends the frame LEFT of the rightmost cell it
    /// wrote, the right-hand part of that line is simply absent from damage,
    /// and no fixed widening can bound the gap: it is as wide as the line.
    /// See under-report 6 in the module docs.
    ///
    /// The line is carried whole, not the screen: a delta still costs one row
    /// per touched line, not `rows` rows.
    FullLine,
    /// The narrow policy: upstream's span widened by `left` / `right` columns.
    ///
    /// Retained because it is strictly cheaper and because it is the policy the
    /// measurement REJECTED -- keeping it runnable is what makes "FullLine is
    /// necessary" a measurement rather than an assertion.
    Widened { left: u16, right: u16 },
}

/// Which damage compensations are active.
///
/// Production always uses [`Compensation::default`]. The knobs exist so
/// `tests/delta_equals_full.rs` can run its **negative control**: the same
/// corpora with a compensation switched off must produce a frame stream that
/// diverges from the truth. A compensation nobody can show the absence of is
/// not a compensation, it is a comment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Compensation {
    pub spans: SpanPolicy,
    /// Also redraw the last column of the line above every damaged line.
    /// Under-report 2; that cell can be on a line with no damage at all, so no
    /// span policy reaches it.
    pub previous_line_last_column: bool,
    /// Re-damage the `display_offset` screen lines the damage iterator
    /// truncates away. Under-report 5.
    pub below_the_fold: bool,
}

impl Default for Compensation {
    fn default() -> Self {
        Self {
            spans: SpanPolicy::FullLine,
            previous_line_last_column: true,
            below_the_fold: true,
        }
    }
}

impl Compensation {
    /// Every compensation off: raw damage, exactly as upstream reports it.
    /// Only for the negative control.
    pub const NONE: Self = Self {
        spans: SpanPolicy::Widened { left: 0, right: 0 },
        previous_line_last_column: false,
        below_the_fold: false,
    };

    /// The same as [`Compensation::default`], usable in a `const` context so a
    /// test can write `..Compensation::DEFAULT`.
    pub const DEFAULT: Self = Self {
        spans: SpanPolicy::FullLine,
        previous_line_last_column: true,
        below_the_fold: true,
    };

    /// The brief's original rule: widen by one column each side.
    pub const WIDEN_ONE: Self = Self {
        spans: SpanPolicy::Widened { left: 1, right: 1 },
        previous_line_last_column: true,
        below_the_fold: true,
    };

    /// Widen by two on the left, which is what a combining mark on a fullwidth
    /// character actually needs.
    pub const WIDEN_TWO: Self = Self {
        spans: SpanPolicy::Widened { left: 2, right: 1 },
        previous_line_last_column: true,
        below_the_fold: true,
    };
}

/// Counters over the life of a projector. The numbers the transport argument
/// is built from.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FrameStats {
    pub full_frames: u64,
    pub delta_frames: u64,
    /// Deltas whose `rows_changed` was empty. `Term::damage()` calls
    /// `damage_cursor()` unconditionally, so this is essentially never hit on a
    /// live terminal -- an empty partial does NOT mean "nothing changed".
    pub empty_deltas: u64,
    pub rows_emitted: u64,
    pub cells_emitted: u64,
}

impl FrameStats {
    pub fn frames(&self) -> u64 {
        self.full_frames + self.delta_frames
    }
}

/// Projects frames out of one authoritative `Term`.
///
/// Holds the generation counter, the last published colour-override table (so
/// deltas can carry only what changed), and the client-parked focus flag.
#[derive(Debug)]
pub struct Projector {
    generation: u64,
    /// Last published override table, indexed by palette index.
    last_colors: [Option<Rgb>; PALETTE_LEN],
    focused: bool,
    force_full: bool,
    compensation: Compensation,
    stats: FrameStats,
}

impl Default for Projector {
    fn default() -> Self {
        Self::new()
    }
}

impl Projector {
    pub fn new() -> Self {
        Self::with_compensation(Compensation::default())
    }

    pub fn with_compensation(compensation: Compensation) -> Self {
        Self {
            generation: 0,
            last_colors: [None; PALETTE_LEN],
            focused: false,
            force_full: false,
            compensation,
            stats: FrameStats::default(),
        }
    }

    pub fn stats(&self) -> FrameStats {
        self.stats
    }

    pub fn compensation(&self) -> Compensation {
        self.compensation
    }

    /// Window focus, parked here because `Term::is_focused` is client state
    /// this crate never reads.
    pub fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
    }

    /// Make the next frame a Full. The escape hatch for anything that mutates
    /// the grid outside the parser -- `Term::grid_mut()` above all -- and the
    /// right thing to call when a consumer reconnects.
    pub fn force_full(&mut self) {
        self.force_full = true;
    }

    /// A full snapshot. Does **not** touch damage, so it is safe to call for a
    /// newly attached consumer at any moment without stealing the delta
    /// stream. Bumps the generation like any other published frame.
    pub fn full<T>(&mut self, term: &Term<T>) -> TerminalFrame {
        self.force_full = false;
        let mut frame = self.skeleton(term, FrameKind::Full);
        frame.color_overrides = self.diff_colors(term.colors(), true);
        frame.rows_changed = full_rows(term.grid(), term.screen_lines(), term.columns());
        self.stats.full_frames += 1;
        self.account(&frame);
        frame
    }

    /// THE damage-driven frame. The only caller of `Term::damage()` and
    /// `Term::reset_damage()` in the engine.
    pub fn next<T>(&mut self, term: &mut Term<T>) -> TerminalFrame {
        // ---- phase 1: collect. The TermDamage borrow is mutable and must die
        // before a single grid cell is read.
        let mut is_full = self.force_full;
        let mut spans: Vec<(usize, usize, usize)> = Vec::new();
        match term.damage() {
            TermDamage::Full => is_full = true,
            TermDamage::Partial(iter) => spans.extend(iter.map(|line| (line.line, line.left, line.right))),
        }
        term.reset_damage();
        // ---- borrow released; the grid is readable again.

        if is_full {
            return self.full(term);
        }

        let rows = term.screen_lines();
        let cols = term.columns();
        let display_offset = term.grid().display_offset();

        let last = cols as u16 - 1;
        let mut merged: Vec<Option<(u16, u16)>> = vec![None; rows];
        for (viewport_row, left, right) in spans {
            // THE ONE CONVERSION. `LineDamageBounds.line` is a VIEWPORT ROW:
            // `TermDamageIterator::next` returns `line.line + display_offset`
            // where `line.line` is the absolute screen line index into the
            // damage array (`term/mod.rs:211`). Undo it here, once, and never
            // let a viewport row past this point.
            let absolute = viewport_row as i64 - display_offset as i64;
            if absolute < 0 || absolute >= rows as i64 {
                continue;
            }
            let line = absolute as usize;
            match self.compensation.spans {
                SpanPolicy::FullLine => merge_raw(&mut merged, line, 0, last),
                SpanPolicy::Widened {
                    left: pad_left,
                    right: pad_right,
                } => {
                    // Upstream can report `right` past the last column:
                    // `clear_line` passes `right.0 - 1` on an exclusive bound
                    // and `erase_chars` passes `end.0` where `end` is
                    // `min(start + count, Column(columns))`.
                    let from = (left as u16).saturating_sub(pad_left).min(last);
                    let to = (right as u16).saturating_add(pad_right).min(last);
                    merge_raw(&mut merged, line, from, to);
                }
            }
        }

        if self.compensation.below_the_fold && display_offset > 0 {
            // The damage iterator threw away the bottom `display_offset` screen
            // lines. Take them back, whole.
            for line in rows.saturating_sub(display_offset)..rows {
                merged[line] = Some((0, last));
            }
        }

        if self.compensation.previous_line_last_column {
            let damaged: Vec<usize> = (0..rows).filter(|line| merged[*line].is_some()).collect();
            for line in damaged {
                if line > 0 {
                    merge_raw(&mut merged, line - 1, last, last);
                }
            }
        }

        let grid = term.grid();
        let mut rows_changed = Vec::new();
        for (line, span) in merged.iter().enumerate() {
            let Some((left, right)) = *span else { continue };
            let point_line = Line(line as i32);
            let cells = (left..=right)
                .map(|col| project_cell(&grid[point_line][Column(col as usize)]))
                .collect();
            rows_changed.push(RowUpdate {
                line: line as i32,
                left,
                right,
                cells,
            });
        }

        let mut frame = self.skeleton(term, FrameKind::Delta);
        frame.color_overrides = self.diff_colors(term.colors(), false);
        if rows_changed.is_empty() {
            self.stats.empty_deltas += 1;
        }
        frame.rows_changed = rows_changed;
        self.stats.delta_frames += 1;
        self.account(&frame);
        frame
    }

    fn account(&mut self, frame: &TerminalFrame) {
        self.stats.rows_emitted += frame.rows_changed.len() as u64;
        self.stats.cells_emitted += frame.cell_count() as u64;
    }

    fn skeleton<T>(&mut self, term: &Term<T>, kind: FrameKind) -> TerminalFrame {
        self.generation += 1;
        TerminalFrame {
            generation: self.generation,
            kind,
            cols: term.columns() as u16,
            rows: term.screen_lines() as u16,
            cursor: project_cursor(term),
            focused: self.focused,
            modes: project_modes(*term.mode()),
            color_overrides: Vec::new(),
            rows_changed: Vec::new(),
        }
    }

    /// Diff the override table. `Colors` exposes only `Index`, no iterator, so
    /// this walks all `PALETTE_LEN` slots -- 269 `Option<Rgb>` compares per
    /// frame, which is nothing next to reading even one row of cells.
    fn diff_colors(&mut self, colors: &Colors, full: bool) -> Vec<ColorOverride> {
        let mut out = Vec::new();
        for index in 0..PALETTE_LEN {
            let now = colors[index].map(from_vte_rgb);
            let before = self.last_colors[index];
            let changed = now != before;
            self.last_colors[index] = now;
            if full {
                // A Full replaces the table wholesale, so it carries the set
                // overrides and nothing else -- a `None` would be noise.
                if let Some(rgb) = now {
                    out.push(ColorOverride {
                        index: index as u16,
                        color: Some(rgb),
                    });
                }
            } else if changed {
                // `None` here is meaningful: OSC 104/110/111/112 reset an
                // override, and the consumer must go back to its default.
                out.push(ColorOverride {
                    index: index as u16,
                    color: now,
                });
            }
        }
        out
    }
}

fn merge_raw(merged: &mut [Option<(u16, u16)>], line: usize, left: u16, right: u16) {
    merged[line] = Some(match merged[line] {
        Some((old_left, old_right)) => (old_left.min(left), old_right.max(right)),
        None => (left, right),
    });
}

/// Every non-default row of the screen, trailing default cells trimmed.
///
/// Lossless only because [`FrameKind::Full`] is defined as "reset to default,
/// then apply".
fn full_rows(grid: &Grid<Cell>, rows: usize, cols: usize) -> Vec<RowUpdate> {
    let default = FrameCell::default();
    let mut out = Vec::new();
    for line in 0..rows {
        let point_line = Line(line as i32);
        let cells: Vec<FrameCell> = (0..cols)
            .map(|col| project_cell(&grid[point_line][Column(col)]))
            .collect();
        let right = match cells.iter().rposition(|cell| *cell != default) {
            Some(right) => right,
            None => continue,
        };
        let mut cells = cells;
        cells.truncate(right + 1);
        out.push(RowUpdate {
            line: line as i32,
            left: 0,
            right: right as u16,
            cells,
        });
    }
    out
}

/// One `alacritty_terminal` cell -> one frame cell.
pub(crate) fn project_cell(cell: &Cell) -> FrameCell {
    let zerowidth = cell.zerowidth().unwrap_or(&[]);
    let underline_color = cell.underline_color().map(project_color);
    let extra = if zerowidth.is_empty() && underline_color.is_none() {
        None
    } else {
        Some(Box::new(CellExtras {
            zerowidth: zerowidth.to_vec(),
            underline_color,
        }))
    };
    FrameCell {
        c: cell.c,
        fg: project_color(cell.fg),
        bg: project_color(cell.bg),
        flags: project_flags(cell.flags),
        extra,
    }
}

/// `vte::ansi::Color` -> [`FrameColor`].
///
/// `Named` and `Indexed` both become `Palette`; see [`FrameColor`] for why that
/// is identity rather than loss.
pub(crate) fn project_color(color: Color) -> FrameColor {
    match color {
        Color::Named(named) => FrameColor::Palette(named as u16),
        Color::Indexed(index) => FrameColor::Palette(index as u16),
        Color::Spec(rgb) => FrameColor::Rgb(from_vte_rgb(rgb)),
    }
}

pub(crate) fn from_vte_rgb(rgb: VteRgb) -> Rgb {
    Rgb {
        r: rgb.r,
        g: rgb.g,
        b: rgb.b,
    }
}

/// Explicit, flag by flag. A `CellFlags::from_bits_truncate(flags.bits())`
/// would be shorter and would silently re-interpret if upstream ever renumbered
/// a bit. `tests/contract.rs::the_flag_mapping_covers_every_upstream_bit`
/// asserts this table is exhaustive over `Flags::all()`.
pub(crate) fn project_flags(flags: Flags) -> CellFlags {
    let mut out = CellFlags::empty();
    out.set(CellFlags::INVERSE, flags.contains(Flags::INVERSE));
    out.set(CellFlags::BOLD, flags.contains(Flags::BOLD));
    out.set(CellFlags::ITALIC, flags.contains(Flags::ITALIC));
    out.set(CellFlags::UNDERLINE, flags.contains(Flags::UNDERLINE));
    out.set(CellFlags::WRAPLINE, flags.contains(Flags::WRAPLINE));
    out.set(CellFlags::WIDE_CHAR, flags.contains(Flags::WIDE_CHAR));
    out.set(CellFlags::WIDE_CHAR_SPACER, flags.contains(Flags::WIDE_CHAR_SPACER));
    out.set(CellFlags::DIM, flags.contains(Flags::DIM));
    out.set(CellFlags::HIDDEN, flags.contains(Flags::HIDDEN));
    out.set(CellFlags::STRIKEOUT, flags.contains(Flags::STRIKEOUT));
    out.set(
        CellFlags::LEADING_WIDE_CHAR_SPACER,
        flags.contains(Flags::LEADING_WIDE_CHAR_SPACER),
    );
    out.set(CellFlags::DOUBLE_UNDERLINE, flags.contains(Flags::DOUBLE_UNDERLINE));
    out.set(CellFlags::UNDERCURL, flags.contains(Flags::UNDERCURL));
    out.set(CellFlags::DOTTED_UNDERLINE, flags.contains(Flags::DOTTED_UNDERLINE));
    out.set(CellFlags::DASHED_UNDERLINE, flags.contains(Flags::DASHED_UNDERLINE));
    out
}

/// Cursor -> [`FrameCursor`], in ABSOLUTE grid coordinates.
///
/// Reproduces `RenderableCursor::new`'s one piece of real behaviour -- a cursor
/// sitting on a `WIDE_CHAR_SPACER` belongs to the wide char one column left --
/// and deliberately does NOT reproduce its folding of "hidden" into the shape.
///
/// `term.vi_mode_cursor` is ignored: vi mode is an Alacritty *user* feature
/// driven by keybindings this engine does not implement, never by the child
/// process. If it is ever wired up it belongs in its own frame field, not
/// silently swapped in for the real cursor.
pub(crate) fn project_cursor<T>(term: &Term<T>) -> FrameCursor {
    let grid = term.grid();
    let mut point: Point = grid.cursor.point;
    if grid[point].flags.contains(Flags::WIDE_CHAR_SPACER) && point.column > Column(0) {
        point.column -= 1;
    }
    let style = term.cursor_style();
    FrameCursor {
        line: point.line.0,
        col: point.column.0 as u16,
        shape: project_shape(style.shape),
        visible: term.mode().contains(TermMode::SHOW_CURSOR),
        blinking: style.blinking,
    }
}

fn project_shape(shape: CursorShape) -> FrameCursorShape {
    match shape {
        CursorShape::Block => FrameCursorShape::Block,
        CursorShape::Underline => FrameCursorShape::Underline,
        CursorShape::Beam => FrameCursorShape::Beam,
        CursorShape::HollowBlock => FrameCursorShape::HollowBlock,
        // `Term::cursor_style()` returns the configured style and never
        // synthesises `Hidden` -- only `RenderableCursor` does, by folding
        // DECTCEM in. Visibility is its own field here, so this arm maps back
        // to the default shape rather than inventing a fifth one.
        CursorShape::Hidden => FrameCursorShape::Block,
    }
}

pub(crate) fn project_modes(mode: TermMode) -> TerminalModes {
    TerminalModes {
        alt_screen: mode.contains(TermMode::ALT_SCREEN),
        line_wrap: mode.contains(TermMode::LINE_WRAP),
        insert: mode.contains(TermMode::INSERT),
        origin: mode.contains(TermMode::ORIGIN),
        mouse_reporting: mode.intersects(TermMode::MOUSE_MODE),
    }
}

/// [`project_color`], exposed for `tests/contract.rs`. Not part of the public
/// contract: the projection is an implementation detail of this crate, but the
/// Named/Indexed collapse it performs is load-bearing enough to deserve a
/// direct test rather than one inferred through a whole `Term`.
pub fn project_color_for_tests(color: Color) -> FrameColor {
    project_color(color)
}

/// Palette index of a [`NamedColor`], for callers building override tables.
pub fn palette_index(named: NamedColor) -> u16 {
    named as u16
}
