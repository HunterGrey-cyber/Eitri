//! THE SECOND VIEW: a read-only window that can be scrolled without moving the
//! authoritative `Term`.
//!
//! The architecture is one child -> one PTY -> one authoritative `Term` -> two
//! views. The authoritative `Term` NEVER moves for user scrolling:
//! `Term::scroll_display` mutates `Grid::display_offset`, which (a) is shared
//! state both views would fight over and (b) silently mangles damage -- see
//! `crate::project`, under-report 5. So the scrollback view is built here, by
//! reading the grid directly at an offset the *caller* owns.
//!
//! # CLAMPED INDEXING IS NOT OPTIONAL
//!
//! `Grid: Index<Line>` goes through `Storage::compute_index`
//! (`grid/storage.rs:232`), whose two bounds checks are `debug_assert!`. In a
//! release build an out-of-range `Line` does not panic: the index arithmetic
//! wraps into the ring buffer's *physical* backing store and returns a row that
//! is no longer logically part of the grid -- including rows the terminal was
//! explicitly told to erase (`CSI 3 J`, which calls `Grid::clear_history` and
//! only shrinks the logical length).
//!
//! `tests/viewport.rs::unclamped_indexing_returns_erased_scrollback` shows an
//! unclamped read handing back content from a cleared history, in a release
//! build, with no panic.
//!
//! # ONE CLAMP, AT THE ORIGIN
//!
//! The clamp is applied once, to the window's top line:
//!
//! ```ignore
//! let top = Line(-(scrollback as i32)).grid_clamp(&*grid, Boundary::Grid);
//! ```
//!
//! Every other index is `top + row` for `row in 0..rows`, and
//! `top <= bottommost_line() - rows + 1` is guaranteed by
//! `total_lines >= screen_lines`, so all of them are in bounds by arithmetic.
//! Clamping each row as well would be dead code -- an equivalent mutant that no
//! test could kill. `tests/viewport.rs::every_projected_line_is_inside_the_grid`
//! sweeps absurd scrollback values and asserts the derived range stays inside
//! `[topmost_line, bottommost_line]`, which is the claim that makes the single
//! clamp sufficient.

use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Boundary, Column, Line};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::Term;

use crate::frame::ColorOverride;
use crate::frame::{FrameKind, RowUpdate, TerminalFrame};
use crate::project::{from_vte_rgb, project_cell, project_cursor, project_modes};

/// A read-only projection of the `rows`-line window whose top edge is
/// `scrollback` lines above the top of the screen.
///
/// `scrollback` is clamped, so any value is safe; `0` is the live screen.
///
/// Always [`FrameKind::Full`]: there is no damage signal for a window the
/// terminal does not know about, and inventing one would be a second, unpinned
/// source of truth.
///
/// `generation` is `0`. This is NOT part of the live frame sequence. Line
/// numbers are ABSOLUTE grid lines and are **negative** once scrolled back, so
/// the result is for direct rendering, not for [`crate::FrameAssembler`], which
/// tracks the live `0..rows` screen. At `scrollback == 0` the two agree, and
/// `tests/viewport.rs::scrollback_zero_matches_the_live_full_frame` pins that.
pub fn project_scrollback<T>(term: &Term<T>, scrollback: usize) -> TerminalFrame {
    // Expressed in terms of the absolute-line primitive, which is the form the
    // renderer path uses. An OFFSET is a convenience for "follow the bottom, N
    // lines up"; it is NOT an anchor and must never be stored across output or
    // resize -- that is the model `RawViewport` exists to replace.
    project_window(term, -(scrollback.min(i32::MAX as usize) as i32), term.screen_lines())
}

/// A read-only projection of `rows` grid lines starting at absolute `top_line`.
///
/// THE RENDERER-FACING ROW SELECTION. `RawViewport::top_line` answers "which
/// absolute line is the top of my view", and this turns that answer into cells.
/// Selection by absolute line rather than by offset is what keeps the retired
/// offset model out of the render path entirely: there is nowhere here to store
/// a stale distance-from-bottom.
///
/// `top_line` is clamped, so any value is safe. See the module docs for why the
/// clamp is applied once, at the origin, and nowhere else.
pub fn project_window<T>(term: &Term<T>, top_line: i32, rows: usize) -> TerminalFrame {
    let grid = term.grid();
    let cols = term.columns();

    // THE CLAMP. See the module docs.
    let top = Line(top_line).grid_clamp(&*grid, Boundary::Grid);

    // AND THE SECOND BOUND, which the offset-only version did not need. The
    // module's "one clamp, at the origin" argument rests on
    // `total_lines >= screen_lines`, so a window of exactly `screen_lines` rows
    // starting anywhere valid always fits. A CALLER-SUPPLIED `rows` breaks that
    // guarantee: ask for more rows than remain below `top` and `top + row` walks
    // past `bottommost_line`, where `Grid: Index<Line>` has only a
    // `debug_assert!` -- in release it wraps into the ring's physical store and
    // hands back rows the terminal no longer considers part of the grid,
    // including ones it was explicitly told to erase. So the window is truncated
    // to what actually exists, and the frame reports the count it really carries.
    let available = (grid.bottommost_line().0 - top.0 + 1).max(0) as usize;
    let rows = rows.min(available);

    let mut rows_changed = Vec::with_capacity(rows);
    for row in 0..rows {
        let line = top + row;
        debug_assert!(line >= grid.topmost_line() && line <= grid.bottommost_line());
        let cells: Vec<_> = (0..cols).map(|col| project_cell(&grid[line][Column(col)])).collect();
        rows_changed.push(RowUpdate {
            line: line.0,
            left: 0,
            right: cols as u16 - 1,
            cells,
        });
    }

    let mut color_overrides = Vec::new();
    let colors = term.colors();
    for index in 0..crate::frame::PALETTE_LEN {
        if let Some(rgb) = colors[index] {
            color_overrides.push(ColorOverride {
                index: index as u16,
                color: Some(from_vte_rgb(rgb)),
            });
        }
    }

    TerminalFrame {
        generation: 0,
        kind: FrameKind::Full,
        cols: cols as u16,
        rows: rows as u16,
        cursor: project_cursor(term),
        focused: false,
        modes: project_modes(*term.mode()),
        color_overrides,
        rows_changed,
    }
}

/// The largest useful `scrollback`: `total_lines - screen_lines`.
pub fn max_scrollback<T>(term: &Term<T>) -> usize {
    term.grid().history_size()
}

// ============================================================================
// THE ANCHOR: what a pinned historical view actually holds on to
// ============================================================================

/// One logical line's footprint in the grid: where it starts and how many grid
/// rows it currently occupies.
///
/// A LOGICAL LINE is a maximal run of grid rows joined by `WRAPLINE` -- one line
/// the application printed, irrespective of how it happens to be folded right
/// now. Alacritty marks the row that CONTINUES into the next one, not the
/// continuation, so a logical line is a row plus every following row until one
/// lacks the flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogicalSpan {
    /// Absolute grid line where this logical line begins. Negative in history.
    pub start: i32,
    /// Grid rows it occupies at the current width. Changes under reflow.
    pub rows: usize,
}

/// Where a held anchor is now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnchorResolution {
    /// Still retained, at this index into the current logical-line sequence.
    Visible { index: usize },
    /// Scrolled out of history, or the alignment was lost (see
    /// [`RawViewport::observe`]). There is no correct content to show. The
    /// caller returns to the bottom; it must NEVER substitute a neighbour.
    Gone,
}

/// Client-side state for the scrolled-back view. Holds no borrow on `Term` and
/// never mutates it -- the whole point of the second view.
///
/// # Why an ORDINAL and not an index, and not an offset
///
/// This is the finding the Phase 3 resize gate exists to establish
/// (`tests/resize_probe.rs`), and it is the reason this type is not three
/// integers:
///
/// * A row OFFSET does not survive a column resize. Reflow re-folds every
///   logical line, so absolute grid lines stop identifying content --
///   `column_resize_reflows_so_absolute_lines_stop_identifying_content` asserts
///   exactly that, so the corpus cannot silently stop reflowing and let a naive
///   offset look correct.
/// * A logical-line INDEX does not survive eviction. Narrowing makes each
///   logical line occupy more rows, so a saturated history evicts from the top
///   and every index-from-top shifts at once. Measured in the gate: index 38
///   became index 4, logical-line count 70 became 36, while the pinned content
///   survived intact.
///
/// So the anchor is an ORDINAL: a number handed out once, never reused, that
/// names a logical line rather than a position. An index is derived from it
/// (`ordinal - oldest_ordinal`) at the moment it is needed.
///
/// # Ordinals are scoped to one pinning session
///
/// `alacritty_terminal` exposes no monotonic line counter -- `Grid` has
/// `history_size()` and `display_offset()` and nothing that counts what has
/// been scrolled away -- so an ordinal space spanning the whole life of the
/// terminal cannot be derived from the public API without reimplementing
/// alacritty's scroll bookkeeping, which is precisely the parallel runtime this
/// track is forbidden to build.
///
/// It does not need to span the terminal's life. Ordinals matter only while the
/// user is holding a historical view; at the bottom there is nothing to pin.
/// So [`RawViewport::pin`] starts a fresh ordinal space and [`RawViewport::
/// unpin`] ends it. This also makes the cost argument trivial: nothing below
/// runs at all unless a view is pinned, so the common case -- following the
/// bottom -- pays zero.
#[derive(Debug, Default)]
pub struct RawViewport {
    /// `None` == following the live bottom, the normal state.
    pinned: Option<Pinned>,
}

#[derive(Debug)]
struct Pinned {
    /// The ordinal this view is holding.
    anchor: u64,
    /// Ordinal of the oldest logical line still retained. Monotonic.
    oldest_ordinal: u64,
    /// Fingerprints of the retained logical lines as of the last observation,
    /// oldest first. The alignment key -- see `observe`.
    fingerprints: Vec<u64>,
    /// Once the anchor is gone it stays gone; re-finding a matching fingerprint
    /// later would be landing on a different line that happens to read the same.
    gone: bool,
}

impl RawViewport {
    pub fn new() -> Self {
        Self { pinned: None }
    }

    /// True while a historical view is held.
    pub fn is_pinned(&self) -> bool {
        self.pinned.is_some()
    }

    /// The held ordinal, if any. Opaque to the caller: it is only meaningful to
    /// the `RawViewport` that issued it, and only until [`Self::unpin`].
    pub fn anchor(&self) -> Option<u64> {
        self.pinned.as_ref().map(|p| p.anchor)
    }

    /// Pin the logical line containing absolute grid `line`, starting a fresh
    /// ordinal space.
    ///
    /// This is the renderer's entry into scrollback: it knows which grid line is
    /// at the top of the window it is showing, and that line is what the user
    /// means by "keep this where it is".
    pub fn pin<T>(&mut self, term: &Term<T>, line: i32) -> u64 {
        let spans = logical_spans(term);
        // The containing logical line, not merely one starting at `line`: a
        // window top can land mid-way through a wrapped line, and pinning the
        // continuation row would pin something with no ordinal of its own.
        let index = spans.iter().rposition(|s| s.start <= line).unwrap_or(0);
        self.pinned = Some(Pinned {
            anchor: index as u64,
            oldest_ordinal: 0,
            fingerprints: fingerprints(term, &spans),
            gone: false,
        });
        index as u64
    }

    /// Return to the live bottom and end the ordinal space.
    pub fn unpin(&mut self) {
        self.pinned = None;
    }

    /// Re-align the ordinal space against the grid's current contents. Call once
    /// per wakeup while pinned -- output and resize both shift what is retained.
    ///
    /// # How eviction is counted
    ///
    /// Logical lines are appended at the bottom and evicted from the top, so the
    /// new sequence is always some suffix of the old one followed by new lines.
    /// The number evicted is the `j` for which `prev[j..]` is a prefix of `next`.
    /// Searching for that `j` is what advances `oldest_ordinal`.
    ///
    /// Fingerprints are taken over the logical line's JOINED text, which is what
    /// makes this survive a column resize: reflow changes how a line is folded,
    /// never what it says. That is the gate's Category A finding turned into the
    /// mechanism that relies on it.
    ///
    /// # When alignment fails
    ///
    /// If no `j` aligns, the entire retained history turned over since the last
    /// observation and there is no evidence left of where the anchor went. The
    /// anchor is declared [`AnchorResolution::Gone`] and STAYS gone. This is the
    /// one bounded fallback the gate sanctioned: an evicted anchor is reported
    /// missing, never resolved to a neighbour. Note which way it fails -- an
    /// unalignable observation loses the view, it does not silently move it.
    pub fn observe<T>(&mut self, term: &Term<T>) {
        let Some(p) = self.pinned.as_mut() else { return };
        // A COST SHORT-CIRCUIT, NOT A CORRECTNESS GUARD -- deleting it changes no
        // observable behaviour, and no test kills it. Stickiness comes from the
        // flag itself: `gone` is cleared only by `pin`, so once set, `resolve`
        // reports Gone no matter what later observations compute. What this line
        // buys is not walking the whole history on every wakeup for a view that is
        // already lost. Verified by mutation: removing it leaves all anchor tests
        // green, which is the expected result and is recorded here so the next
        // person does not go hunting for the missing test.
        if p.gone {
            return;
        }
        let spans = logical_spans(term);
        let next = fingerprints(term, &spans);

        match align(&p.fingerprints, &next) {
            Some(evicted) => {
                p.oldest_ordinal += evicted as u64;
                p.fingerprints = next;
                // Where the invariant `resolve` relies on is established: once the
                // oldest retained ordinal has passed the anchor, the anchored line
                // has been evicted and there is no correct content for it.
                if p.anchor < p.oldest_ordinal {
                    p.gone = true;
                }
            }
            None => p.gone = true,
        }
    }

    /// Where the anchor is now.
    pub fn resolve<T>(&self, term: &Term<T>) -> AnchorResolution {
        let Some(p) = self.pinned.as_ref() else {
            return AnchorResolution::Gone;
        };
        // `gone` is the SINGLE source of truth for "this anchor is past". An
        // earlier revision also tested `anchor < oldest_ordinal` here; that was
        // unkillable by mutation, because `observe` sets `gone` in exactly that
        // case and is the only thing that can advance `oldest_ordinal`. The
        // duplicate check was removed rather than documented -- defensive code no
        // test can exercise is a place for a future bug to hide undetected. The
        // invariant it was guarding is asserted where it is actually established.
        if p.gone {
            return AnchorResolution::Gone;
        }
        debug_assert!(p.anchor >= p.oldest_ordinal, "observe() must mark a passed anchor gone");
        let index = (p.anchor - p.oldest_ordinal) as usize;
        // Compared against the LIVE span count, not the stored fingerprint
        // length: resolve may be called after a resize that `observe` has not
        // seen yet, and reporting a stale in-range index would hand the renderer
        // a line number that no longer exists.
        if index >= logical_spans(term).len() {
            return AnchorResolution::Gone;
        }
        AnchorResolution::Visible { index }
    }

    /// The absolute grid line the anchored logical line currently starts at --
    /// what a renderer needs to place the window. `None` once the anchor is
    /// [`AnchorResolution::Gone`].
    pub fn top_line<T>(&self, term: &Term<T>) -> Option<i32> {
        match self.resolve(term) {
            AnchorResolution::Visible { index } => logical_spans(term).get(index).map(|s| s.start),
            AnchorResolution::Gone => None,
        }
    }
}

/// Every logical line currently in the grid, oldest first.
pub fn logical_spans<T>(term: &Term<T>) -> Vec<LogicalSpan> {
    let grid = term.grid();
    let top = -(grid.history_size() as i32);
    let bottom = grid.screen_lines() as i32 - 1;

    let mut out = Vec::new();
    let mut start: Option<i32> = None;
    for line in top..=bottom {
        if start.is_none() {
            start = Some(line);
        }
        if !wraps(term, line) {
            let s = start.take().expect("start is set at the top of every iteration");
            out.push(LogicalSpan {
                start: s,
                rows: (line - s + 1) as usize,
            });
        }
    }
    // A line still open at the bottom edge: the application has printed a
    // wrapped line whose continuation has not arrived. It is a logical line and
    // must be counted, or the count -- and every ordinal derived from it --
    // shifts by one the moment the rest of it lands.
    if let Some(s) = start {
        out.push(LogicalSpan {
            start: s,
            rows: (bottom - s + 1) as usize,
        });
    }
    out
}

/// Does this row continue into the next? Alacritty flags the CONTINUING row.
fn wraps<T>(term: &Term<T>, line: i32) -> bool {
    let grid = term.grid();
    let l = Line(line).grid_clamp(grid, Boundary::Grid);
    let row = &grid[l];
    (0..grid.columns()).any(|c| row[Column(c)].flags.contains(Flags::WRAPLINE))
}

/// FNV-1a over a logical line's joined text.
///
/// Taken over TEXT, not over grid geometry, because that is the property the
/// resize gate proved stable: a column resize re-folds a logical line but does
/// not change what it says. Hashing anything positional here would reintroduce
/// exactly the offset fragility this type exists to avoid.
fn fingerprints<T>(term: &Term<T>, spans: &[LogicalSpan]) -> Vec<u64> {
    let grid = term.grid();
    let cols = grid.columns();
    spans
        .iter()
        .map(|span| {
            let mut text = String::new();
            for row in 0..span.rows {
                let l = Line(span.start + row as i32).grid_clamp(grid, Boundary::Grid);
                let r = &grid[l];
                for c in 0..cols {
                    text.push(r[Column(c)].c);
                }
            }
            // Only the END is trimmed. Interior padding is real content at this
            // width; trailing blanks are an artifact of the row being wider than
            // the text, and they differ between widths -- so trimming the end is
            // what makes the fingerprint width-independent.
            let mut h: u64 = 0xcbf2_9ce4_8422_2325;
            for ch in text.trim_end().chars() {
                h ^= ch as u64;
                h = h.wrapping_mul(0x100_0000_01b3);
            }
            h
        })
        .collect()
}

/// How many leading entries of `prev` were evicted to produce `next`.
///
/// # THE OLDEST RETAINED LOGICAL LINE IS NOT A RELIABLE KEY
///
/// It can be a FRAGMENT. When a logical line is partially evicted, the rows that
/// survive are still flagged as one run, so `logical_spans` reports them as a
/// logical line -- but its text is only the tail of what the application printed,
/// and *where that tail begins depends on the current width*. Reflow therefore
/// changes it. Measured on a 40->20 narrowing of a saturated history: the oldest
/// entry went from 25 `x`s to 5, while every other line's text was identical.
///
/// Nothing upstream can fix this: whether the topmost retained row is a
/// continuation of an evicted line is exactly the information eviction destroyed.
/// `Grid` keeps no marker for it.
///
/// So the first entry of each sequence is excluded from the comparison -- `prev[j]`
/// is the one that lines up with `next[0]`, and matching starts at `prev[j + 1]`
/// against `next[1]`. Everything from the second entry on is a whole logical line
/// whose text is width-independent, which is what makes the fingerprint work at
/// all.
///
/// # THE NEWEST LOGICAL LINE IS NOT A RELIABLE KEY EITHER
///
/// The last entry is the line the cursor is on. It is still being written: every
/// character the application prints changes its text, and the run of output that
/// makes a pinned view worth having is exactly a stream of such changes. Using
/// it as a key breaks alignment on EVERY append -- which is the common case, not
/// an edge one.
///
/// This was a live bug, and the tests that should have caught it did not: the
/// three cases exercising output-while-pinned all asserted the anchor was
/// `Gone`, and a broken alignment produces `Gone` too. They passed for the wrong
/// reason. `an_anchor_stays_put_while_output_arrives_below_it` is the positive
/// case that was missing, and it is the one that fails if either exclusion is
/// removed.
///
/// So BOTH ends are excluded: `prev[j]` corresponds to `next[0]` (either may be
/// a fragment) and `prev[prev.len() - 1]` is the cursor line. The comparison runs
/// over `prev[j + 1 .. prev.len() - 1]`, every entry of which is a complete,
/// settled logical line whose text is width-independent.
///
/// `Some(j)` when that slice is a prefix of `next[1..]`; `None` when nothing
/// aligns. A vacuous match on an empty overlap is deliberately NOT accepted: it
/// would report "nothing evicted" on no evidence, which is the silent-drift
/// direction. Returning `None` loses the view instead, which the caller reports
/// as [`AnchorResolution::Gone`].
///
/// Note what pins `j` even when fingerprints repeat (a screen full of identical
/// lines, a build log): the slice must match in FULL, so its LENGTH constrains
/// the offset. A shorter accidental match at a smaller `j` leaves the lengths
/// inconsistent and is rejected.
fn align(prev: &[u64], next: &[u64]) -> Option<usize> {
    if next.is_empty() || prev.len() < 2 {
        return None;
    }
    let settled_end = prev.len() - 1;
    for j in 0..settled_end {
        let tail = &prev[j + 1..settled_end];
        if tail.is_empty() || tail.len() > next.len() - 1 {
            continue;
        }
        if next[1..1 + tail.len()] == *tail {
            return Some(j);
        }
    }
    None
}
