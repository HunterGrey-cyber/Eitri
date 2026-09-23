//! Moved from `terminal-pane/src/metrics.rs` on `freeze/terminal-stack` @ `1e715ab` (2026-09-23),
//! unchanged below this paragraph but for rustfmt at this repository's `max_width = 120`, and for
//! one change: a private-use codepoint falls back through the terminal's own family
//! ([`TerminalMetrics::font_for`], GUI pass 2026-09-23, defect 4). Where it says "Verdandi" or "the
//! handoff", the contract now lives in this workspace's `terminal-render/`.
//!
//! The single cell lattice every op is placed on.
//!
//! Verdandi emits cells and has no opinion on fonts, cell size, baseline or DPI — the handoff says
//! so explicitly and carries none of them. That makes the grid-to-pixel transform entirely this
//! side's responsibility, and it makes having exactly ONE of it the whole ballgame: backgrounds,
//! glyphs, the cursor and the overlay notice must land on the same lattice or a cursor sits half a
//! pixel off its character and an underline misses the glyph it belongs to.
//!
//! The same object also decides the PTY size. That is deliberate rather than convenient: if text
//! layout and PTY geometry each computed a cell size, the terminal would be told it has a number of
//! columns that does not match the number being drawn, and every wrap would land in the wrong place.

use std::cell::RefCell;
use std::collections::HashMap;

use skia_safe::{Font, FontMgr, FontStyle};

/// Where pixels come from, and how many cells fit.
///
/// All pixel values are DEVICE pixels. There is no canvas transform anywhere in this crate: the
/// font is sized in device pixels up front, exactly as the editor pane does it
/// (`CachingShaper::new` multiplies the point size by the scale factor rather than scaling the
/// canvas). Mixing the two conventions in one window is how text ends up crisp in one pane and
/// blurred in the other.
#[derive(Clone)]
pub struct TerminalMetrics {
    font: Font,
    /// The fallback source for characters the primary face has no glyph for -- CJK, most obviously.
    /// Held here rather than looked up per draw because a font manager lookup is not free and a
    /// terminal draws thousands of glyphs a frame.
    font_mgr: FontMgr,
    family: String,
    /// Point size BEFORE the scale factor. Kept so a rescale can rebuild from the same intent.
    size_pt: f32,
    scale: f32,
    cell_width: f32,
    cell_height: f32,
    baseline: f32,
    cols: u16,
    rows: u16,
    /// Faces for graphemes the primary face has no glyph for, memoised by `(character, bold,
    /// italic)`.
    ///
    /// This is not a micro-optimisation. `FontMgr::match_family_style_character` is a fontconfig
    /// query, and the first measured full-frame benchmark spent **~600ms of a ~610ms frame** in it:
    /// a screen of CJK asks the system font database once per glyph per frame. It was a real defect
    /// found by measuring, and worth recording because damage tracking -- the optimisation this
    /// crate was going to reach for -- would not have fixed it.
    ///
    /// `RefCell` rather than `&mut self` so that rendering stays a `&TerminalMetrics` operation; a
    /// cache is not a semantic change and should not propagate mutability through the paint path.
    /// A `None` value is a real answer ("no face on this system has it") and is cached too, so a
    /// missing glyph does not re-query on every frame forever.
    fallback: RefCell<HashMap<(i32, bool, bool), Option<Font>>>,
}

/// What a terminal defaults to when nothing else is configured. A monospace family is not optional:
/// the whole cell model assumes every ASCII advance is identical, and a proportional face silently
/// breaks that while still rendering something that looks like text.
const DEFAULT_FAMILY: &str = "monospace";
const DEFAULT_SIZE_PT: f32 = 13.0;

impl TerminalMetrics {
    /// Builds metrics for a pane of `width_px` x `height_px` DEVICE pixels.
    ///
    /// `scale` is the device pixel ratio. Note that GTK's `scale_factor()` CEILS a fractional scale
    /// (1.25 becomes 2), which is measured behaviour in this repo rather than a guess -- and it is
    /// fine to feed in, because the same value is used on both sides of every conversion, so the
    /// lattice stays self-consistent even when it disagrees with the compositor's true fraction.
    pub fn new(width_px: f32, height_px: f32, scale: f32) -> Self {
        Self::with_font(DEFAULT_FAMILY, DEFAULT_SIZE_PT, width_px, height_px, scale)
    }

    pub fn with_font(family: &str, size_pt: f32, width_px: f32, height_px: f32, scale: f32) -> Self {
        let font_mgr = FontMgr::new();
        // A named family, then the generic monospace alias, then whatever the system will give us.
        // The last step matters: a machine with no fontconfig monospace alias would otherwise panic
        // during window construction, which is a very confusing way to learn about a font problem.
        let typeface = font_mgr
            .match_family_style(family, FontStyle::normal())
            .or_else(|| font_mgr.match_family_style(DEFAULT_FAMILY, FontStyle::normal()))
            .or_else(|| font_mgr.legacy_make_typeface(None, FontStyle::normal()))
            .expect("no usable font: the system font manager offered nothing at all");
        let font = Font::from_typeface(typeface, size_pt * scale);

        // The advance of a representative ASCII glyph IS the cell width for a monospace face. `M` is
        // the conventional probe; on a monospace font every ASCII advance agrees, and `advance_ascii`
        // below asserts that rather than assuming it.
        let (cell_width, _) = font.measure_str("M", None);
        let font_metrics = font.metrics().1;
        // ascent is negative (above the baseline) and descent positive, so the line box is their
        // span plus leading. Rounded UP: a fractional cell height accumulates down the window and
        // the bottom row ends up clipped.
        let cell_height = (font_metrics.descent - font_metrics.ascent + font_metrics.leading).ceil();
        let baseline = -font_metrics.ascent;

        let cell_width = if cell_width > 0.0 {
            cell_width
        } else {
            size_pt * scale * 0.6
        };
        let cell_height = if cell_height > 0.0 {
            cell_height
        } else {
            (size_pt * scale * 1.2).ceil()
        };

        let (cols, rows) = Self::grid_for(width_px, height_px, cell_width, cell_height);
        Self {
            fallback: RefCell::new(HashMap::new()),
            font,
            font_mgr,
            family: family.to_string(),
            size_pt,
            scale,
            cell_width,
            cell_height,
            baseline,
            cols,
            rows,
        }
    }

    /// How many whole cells fit. Floored, and floored on purpose: a partial column would be a column
    /// the terminal believes it can write into and the pane cannot fully show.
    ///
    /// Clamped to at least 1x1 because a PTY with zero columns is not a smaller terminal, it is an
    /// invalid one -- and a pane briefly measures 0x0 during GTK's own layout, which would otherwise
    /// send `TIOCSWINSZ` a size no child can cope with.
    fn grid_for(width_px: f32, height_px: f32, cell_width: f32, cell_height: f32) -> (u16, u16) {
        let cols = (width_px / cell_width).floor().max(1.0);
        let rows = (height_px / cell_height).floor().max(1.0);
        (cols.min(u16::MAX as f32) as u16, rows.min(u16::MAX as f32) as u16)
    }

    /// Recomputes the grid for a new pane size, keeping the same font. Returns whether the cell
    /// dimensions changed, which is the only thing a PTY needs to be told about.
    pub fn resize(&mut self, width_px: f32, height_px: f32) -> bool {
        let (cols, rows) = Self::grid_for(width_px, height_px, self.cell_width, self.cell_height);
        let changed = (cols, rows) != (self.cols, self.rows);
        self.cols = cols;
        self.rows = rows;
        changed
    }

    /// Rebuilds for a new device scale, preserving the requested point size and pane size.
    pub fn rescale(&self, width_px: f32, height_px: f32, scale: f32) -> Self {
        Self::with_font(&self.family, self.size_pt, width_px, height_px, scale)
    }

    pub fn font(&self) -> &Font {
        &self.font
    }

    /// The face to draw `first` with, at this metrics' size, in the requested weight and slant.
    ///
    /// The primary face is a monospace one chosen for its ASCII cell metrics, which typically has no
    /// CJK coverage at all. Rather than render a row of missing-glyph boxes, this falls back per
    /// grapheme through Skia's font manager -- memoised, see [`TerminalMetrics::fallback`].
    ///
    /// The cell box does NOT change as a result. `cols` on the op already says how wide the terminal
    /// believes the glyph to be, and a fallback face's own advance is never consulted: one that
    /// measured wider would otherwise push every following column along, and the pane would disagree
    /// with the terminal about where text is.
    pub fn font_for(&self, first: char, bold: bool, italic: bool) -> Font {
        let styled = |mut font: Font| {
            if bold {
                font.set_embolden(true);
            }
            if italic {
                font.set_skew_x(-0.25);
            }
            font
        };
        if self.font.unichar_to_glyph(first as i32) != 0 {
            return styled(self.font.clone());
        }
        let key = (first as i32, bold, italic);
        if let Some(cached) = self.fallback.borrow().get(&key) {
            // A cached `None` means no face on this system has it: draw with the primary face and
            // let it show its missing-glyph box, which is information. Silently drawing nothing
            // would look like the terminal lost output.
            return cached.clone().unwrap_or_else(|| styled(self.font.clone()));
        }
        let found = self
            .font_mgr
            .match_family_style_character(
                fallback_family(first, &self.family),
                if bold { FontStyle::bold() } else { FontStyle::normal() },
                &[],
                first as i32,
            )
            .map(|typeface| {
                let mut font = Font::from_typeface(typeface, self.font.size());
                // Weight comes from the matched face itself; only the synthetic slant is applied.
                if italic {
                    font.set_skew_x(-0.25);
                }
                font
            });
        self.fallback.borrow_mut().insert(key, found.clone());
        found.unwrap_or_else(|| styled(self.font.clone()))
    }
    pub fn font_mgr(&self) -> &FontMgr {
        &self.font_mgr
    }
    pub fn cell_width(&self) -> f32 {
        self.cell_width
    }
    pub fn cell_height(&self) -> f32 {
        self.cell_height
    }
    pub fn baseline(&self) -> f32 {
        self.baseline
    }
    pub fn scale(&self) -> f32 {
        self.scale
    }
    pub fn cols(&self) -> u16 {
        self.cols
    }
    pub fn rows(&self) -> u16 {
        self.rows
    }

    /// THE conversion. `col`/`row` are window cells, exactly as every op carries them.
    ///
    /// Deliberately the only place cells become pixels in this crate. The handoff's rule -- "use one
    /// metrics object for backgrounds, glyphs, cursor and overlay or they will land on different
    /// lattices" -- is enforceable only if there is one function to point at.
    pub fn cell_rect(&self, row: u16, col: u16, cols: u16) -> skia_safe::Rect {
        let x = col as f32 * self.cell_width;
        let y = row as f32 * self.cell_height;
        skia_safe::Rect::from_xywh(x, y, cols as f32 * self.cell_width, self.cell_height)
    }

    /// Where a glyph's baseline origin sits for a given cell.
    pub fn text_origin(&self, row: u16, col: u16) -> skia_safe::Point {
        skia_safe::Point::new(
            col as f32 * self.cell_width,
            row as f32 * self.cell_height + self.baseline,
        )
    }
}

/// The family a fallback lookup for `first` is asked in.
///
/// A private-use codepoint means only what the font that defines it says: U+F43A is a clock in a
/// Nerd Font and something else in DejaVu Sans, which also maps it. Asked with no family, fontconfig
/// answers from the `sans-serif` alias -- DejaVu Sans, on the machine where lualine's clock came out
/// as another pictogram (GUI pass 2026-09-23, defect 4). Asked in the terminal's own family, it
/// answers from that family's alias list, which is where a user puts the Nerd Font whose icons their
/// tools print -- the fallback chain foot takes too (fcft sorts from the primary font's pattern).
///
/// Every other codepoint is the same character in any font that has it, so it keeps the unnamed
/// lookup: asking in `monospace` there would move CJK to whichever face heads that list (NSimSun,
/// a Song face, on this machine, in place of Microsoft YaHei), which is phase 4's decision -- the
/// terminal's face follows nvim's `guifont` then -- not this fix's.
fn fallback_family(first: char, family: &str) -> &str {
    if is_private_use(first) {
        family
    } else {
        ""
    }
}

/// Unicode's three private-use areas: the BMP's, and planes 15 and 16. Nerd Fonts live in the first
/// two.
fn is_private_use(ch: char) -> bool {
    matches!(ch as u32, 0xE000..=0xF8FF | 0xF_0000..=0xF_FFFD | 0x10_0000..=0x10_FFFD)
}

impl std::fmt::Debug for TerminalMetrics {
    // Hand-written because `skia_safe::Font` is not Debug, and a metrics object that cannot be
    // printed is painful to diagnose a lattice bug with.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TerminalMetrics")
            .field("family", &self.family)
            .field("size_pt", &self.size_pt)
            .field("scale", &self.scale)
            .field("cell", &(self.cell_width, self.cell_height))
            .field("baseline", &self.baseline)
            .field("grid", &(self.cols, self.rows))
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_monospace_face_gives_every_ascii_glyph_the_same_advance() {
        let metrics = TerminalMetrics::new(800.0, 600.0, 1.0);
        // The cell model is built on this. If it is false the chosen family is not monospace and
        // every column after the first would drift -- a failure that looks like "text is slightly
        // wrong" rather than like a font problem.
        let probe = "MiW il1|";
        for ch in probe.chars() {
            let (advance, _) = metrics.font().measure_str(ch.to_string(), None);
            assert!(
                (advance - metrics.cell_width()).abs() < 0.01,
                "{ch:?} advances {advance} but the cell is {} wide",
                metrics.cell_width()
            );
        }
    }

    #[test]
    fn the_grid_floors_rather_than_rounding() {
        let metrics = TerminalMetrics::new(800.0, 600.0, 1.0);
        let cw = metrics.cell_width();
        let ch = metrics.cell_height();
        // Exactly 10 cells plus nine tenths of one: the partial column must not be offered to the
        // terminal, because it is a column the child would write into and the pane could not show.
        let mut m = metrics.clone();
        m.resize(cw * 10.9, ch * 5.9);
        assert_eq!((m.cols(), m.rows()), (10, 5));
    }

    #[test]
    fn a_zero_sized_pane_still_reports_a_usable_grid() {
        // GTK measures a widget at 0x0 during its own layout. A PTY told it has zero columns is not
        // a small terminal, it is an invalid one.
        let mut metrics = TerminalMetrics::new(800.0, 600.0, 1.0);
        metrics.resize(0.0, 0.0);
        assert_eq!((metrics.cols(), metrics.rows()), (1, 1));
    }

    #[test]
    fn resize_reports_only_real_grid_changes() {
        let mut metrics = TerminalMetrics::new(800.0, 600.0, 1.0);
        let (cols, rows) = (metrics.cols(), metrics.rows());
        // A sub-cell change is not a resize. Reporting it would send TIOCSWINSZ on every pixel of a
        // drag, and a resize storm is how a child process ends up redrawing instead of working.
        assert!(!metrics.resize(
            cols as f32 * metrics.cell_width() + metrics.cell_width() * 0.4,
            rows as f32 * metrics.cell_height()
        ));
        assert!(metrics.resize(
            (cols as f32 + 1.0) * metrics.cell_width(),
            rows as f32 * metrics.cell_height()
        ));
    }

    #[test]
    fn cells_tile_the_surface_without_gaps_or_overlap() {
        let metrics = TerminalMetrics::new(800.0, 600.0, 1.0);
        let a = metrics.cell_rect(3, 4, 1);
        let b = metrics.cell_rect(3, 5, 1);
        let below = metrics.cell_rect(4, 4, 1);
        assert_eq!(a.right, b.left, "adjacent columns must share an edge");
        assert_eq!(a.bottom, below.top, "adjacent rows must share an edge");

        // A two-column op covers exactly the two cells it claims -- the wide-glyph case.
        let wide = metrics.cell_rect(3, 4, 2);
        assert_eq!(wide.left, a.left);
        assert_eq!(wide.right, b.right);
    }

    #[test]
    fn the_private_use_areas_and_only_they_fall_back_through_the_terminals_family() {
        for ch in [
            '\u{e000}',
            '\u{e0a0}',
            '\u{f43a}',
            '\u{f8ff}',
            '\u{f09aa}',
            '\u{f_fffd}',
            '\u{10_0000}',
        ] {
            assert_eq!(fallback_family(ch, "monospace"), "monospace", "U+{:X}", ch as u32);
        }
        for ch in ['\u{d7ff}', '\u{f900}', '漢', 'é', '\u{2500}', '\u{1f600}'] {
            assert_eq!(fallback_family(ch, "monospace"), "", "U+{:X}", ch as u32);
        }
    }

    /// GUI pass 2026-09-23, defect 4: lualine's clock (U+F43A, nf-oct-clock) came out as another
    /// pictogram, because the unnamed fallback lookup answered DejaVu Sans, which maps that
    /// codepoint too. The face drawn must be the one the terminal's own family would fall back to.
    ///
    /// This depends on the fonts installed, and says so: on a machine where the two lookups agree
    /// for every codepoint below, nothing here can tell the fix from its absence, and the test
    /// prints that instead of passing silently. On the machine the defect was found on they
    /// disagree for U+F43A (DejaVu Sans against Maple Mono NF CN), and this was checked red there.
    #[test]
    fn a_private_use_glyph_falls_back_through_the_terminals_own_family() {
        let metrics = TerminalMetrics::new(800.0, 600.0, 1.0);
        let lookup = |family: &str, ch: char| {
            metrics
                .font_mgr()
                .match_family_style_character(family, FontStyle::normal(), &[], ch as i32)
                .map(|t| t.family_name())
        };
        let mut discriminating = 0;
        // Nerd Font codepoints lualine and p10k print: the clock, the branch, a powerline arrow,
        // a chevron, and one from the plane-15 Material set.
        for ch in ['\u{f43a}', '\u{e0a0}', '\u{e0b0}', '\u{f054}', '\u{f09aa}'] {
            if metrics.font().unichar_to_glyph(ch as i32) != 0 {
                continue; // the primary face has it; no fallback happens
            }
            let own = lookup(DEFAULT_FAMILY, ch);
            let unnamed = lookup("", ch);
            let drawn = metrics.font_for(ch, false, false).typeface().family_name();
            if let Some(own) = &own {
                assert_eq!(&drawn, own, "U+{:X}", ch as u32);
            }
            if own != unnamed {
                discriminating += 1;
            }
        }
        if discriminating == 0 {
            eprintln!(
                "a_private_use_glyph_falls_back_through_the_terminals_own_family: no codepoint here \
                 falls back differently by family on this machine; the check above could not fail"
            );
        }
    }

    /// The other half of `fallback_family`: a codepoint outside the private-use areas is looked up
    /// exactly as before, so CJK keeps the face it had.
    #[test]
    fn a_cjk_glyph_still_falls_back_through_the_unnamed_lookup() {
        let metrics = TerminalMetrics::new(800.0, 600.0, 1.0);
        let ch = '漢';
        if metrics.font().unichar_to_glyph(ch as i32) != 0 {
            return;
        }
        let unnamed = metrics
            .font_mgr()
            .match_family_style_character("", FontStyle::normal(), &[], ch as i32)
            .map(|t| t.family_name());
        if let Some(unnamed) = unnamed {
            assert_eq!(metrics.font_for(ch, false, false).typeface().family_name(), unnamed);
        }
    }

    #[test]
    fn a_higher_scale_gives_proportionally_larger_cells_and_a_smaller_grid() {
        let one = TerminalMetrics::new(1600.0, 1200.0, 1.0);
        let two = TerminalMetrics::new(1600.0, 1200.0, 2.0);
        assert!(two.cell_width() > one.cell_width() * 1.8, "{:?} vs {:?}", one, two);
        assert!(
            two.cols() < one.cols(),
            "a 2x scale in the same pixel box must fit fewer columns"
        );
        // The font is sized in device pixels rather than the canvas being transformed, matching the
        // editor pane. A crate that scaled the canvas instead would disagree with it by a whole
        // hinting pass.
        assert!((two.baseline() / one.baseline() - 2.0).abs() < 0.15);
    }
}
