//! The single cell lattice every op is placed on.
//!
//! terminal-render emits cells without fonts, cell size, baseline or DPI. One grid-to-pixel transform
//! must place backgrounds, glyphs, cursor and overlay on the same lattice, or a cursor sits half a
//! pixel off its character and an underline misses its glyph.
//!
//! The same metrics decide the PTY size: computing layout and PTY geometry separately would tell
//! the terminal a column count different from the one drawn, breaking every wrap.

use std::cell::RefCell;
use std::collections::HashMap;

use skia_safe::{Font, FontMgr, FontStyle, TextBlob};

/// Where pixels come from, and how many cells fit.
///
/// All pixel values are DEVICE pixels. There is no canvas transform anywhere in this crate: the
/// font is sized in device pixels up front, exactly as the editor pane does it
/// (`CachingShaper::new` multiplies the logical pixel size by the scale factor rather than scaling the
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
    families: Vec<String>,
    /// Logical pixel size BEFORE the scale factor, so a rescale can rebuild from the same intent.
    size_px: f32,
    scale: f32,
    cell_width: f32,
    cell_height: f32,
    baseline: f32,
    cols: u16,
    rows: u16,
    /// Faces for graphemes the primary face has no glyph for, memoised by `(character, bold,
    /// italic)`.
    ///
    /// `FontMgr::match_family_style_character` queries fontconfig. Cache it so CJK output does
    /// not query the system font database once per glyph per frame; damage tracking alone cannot
    /// remove that cost from a full frame.
    ///
    /// `RefCell` rather than `&mut self` so that rendering stays a `&TerminalMetrics` operation; a
    /// cache is not a semantic change and should not propagate mutability through the paint path.
    /// A `None` value is a real answer ("no face on this system has it") and is cached too, so a
    /// missing glyph does not re-query on every frame forever.
    fallback: RefCell<HashMap<(i32, bool, bool), Option<Font>>>,
    /// The shaped blob for a grapheme, per `[bold][italic]`, memoised the same way as `fallback`
    /// (see [`TerminalMetrics::text_blob`]).
    blobs: RefCell<[[HashMap<String, Option<TextBlob>>; 2]; 2]>,
}

/// How many distinct graphemes [`TerminalMetrics::text_blob`] keeps per style before it starts
/// over. A screen of text uses a few hundred; this only bounds a stream of ever-new ones (`cat` of
/// random CJK), whose blobs would otherwise accumulate for the life of the pane.
const BLOB_CACHE_CAP: usize = 4096;

/// What a terminal defaults to when nothing else is configured. A monospace family is not optional:
/// the whole cell model assumes every ASCII advance is identical, and a proportional face silently
/// breaks that while still rendering something that looks like text.
const DEFAULT_FAMILY: &str = "monospace";
const DEFAULT_SIZE_PX: f32 = 13.0;

/// What the generic `monospace` stands for, tried in order after it. fontconfig resolves the alias itself.
/// CoreText knows no family by that name and its last resort is Helvetica, a proportional face, so on macOS
/// the faces the system ships for it follow: Menlo (Terminal's default), Monaco, Courier.
#[cfg(target_os = "macos")]
const MONOSPACE: &[&str] = &[DEFAULT_FAMILY, "Menlo", "Monaco", "Courier"];
#[cfg(not(target_os = "macos"))]
const MONOSPACE: &[&str] = &[DEFAULT_FAMILY];

impl TerminalMetrics {
    /// Builds metrics for a pane of `width_px` x `height_px` DEVICE pixels.
    ///
    /// `scale` is the device pixel ratio. Note that GTK's `scale_factor()` CEILS a fractional scale
    /// (1.25 becomes 2), which is measured behaviour in this repo rather than a guess -- and it is
    /// fine to feed in, because the same value is used on both sides of every conversion, so the
    /// lattice stays self-consistent even when it disagrees with the compositor's true fraction.
    pub fn new(width_px: f32, height_px: f32, scale: f32) -> Self {
        Self::with_font(DEFAULT_FAMILY, DEFAULT_SIZE_PX, width_px, height_px, scale)
    }

    pub fn with_font(family: &str, size_px: f32, width_px: f32, height_px: f32, scale: f32) -> Self {
        Self::with_families(&[family.to_owned()], size_px, width_px, height_px, scale)
    }

    /// Tries families in preference order, retaining the list so a device-scale change can rebuild
    /// the same face. Glyph fallback uses the family that matched, including its system aliases.
    pub fn with_families(families: &[String], size_px: f32, width_px: f32, height_px: f32, scale: f32) -> Self {
        let font_mgr = FontMgr::new();
        // Named families, then the generic monospace alias (`MONOSPACE`), then whatever the system will give us.
        // The last step matters: a machine with no fontconfig monospace alias would otherwise panic
        // during window construction, which is a very confusing way to learn about a font problem.
        let (family, typeface) = families
            .iter()
            .map(String::as_str)
            .chain(MONOSPACE.iter().copied())
            .find_map(|family| {
                font_mgr
                    .match_family_style(family, FontStyle::normal())
                    .map(|typeface| (family.to_owned(), typeface))
            })
            .or_else(|| {
                font_mgr
                    .legacy_make_typeface(None, FontStyle::normal())
                    .map(|typeface| (typeface.family_name(), typeface))
            })
            .expect("no usable font: the system font manager offered nothing at all");
        let font = Font::from_typeface(typeface, size_px * scale);

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
            size_px * scale * 0.6
        };
        let cell_height = if cell_height > 0.0 {
            cell_height
        } else {
            (size_px * scale * 1.2).ceil()
        };

        let (cols, rows) = Self::grid_for(width_px, height_px, cell_width, cell_height);
        Self {
            fallback: RefCell::new(HashMap::new()),
            blobs: RefCell::default(),
            font,
            font_mgr,
            family,
            families: families.to_vec(),
            size_px,
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
    /// send `TIOCSWINSZ` a size no child can cope with. The upper bounds keep tiny cells or huge
    /// surfaces from allocating an enormous grid.
    fn grid_for(width_px: f32, height_px: f32, cell_width: f32, cell_height: f32) -> (u16, u16) {
        let cols = (width_px / cell_width).floor().max(1.0);
        let rows = (height_px / cell_height).floor().max(1.0);
        (cols.min(4096.0) as u16, rows.min(2048.0) as u16)
    }

    /// Recomputes the grid for a new pane size, keeping the same font. Returns whether the grid's
    /// columns or rows changed, which is the only thing a PTY needs to be told about.
    pub fn resize(&mut self, width_px: f32, height_px: f32) -> bool {
        let (cols, rows) = Self::grid_for(width_px, height_px, self.cell_width, self.cell_height);
        let changed = (cols, rows) != (self.cols, self.rows);
        self.cols = cols;
        self.rows = rows;
        changed
    }

    /// Rebuilds for a new device scale, preserving the requested families, logical size and pane size.
    pub fn rescale(&self, width_px: f32, height_px: f32, scale: f32) -> Self {
        Self::with_families(&self.families, self.size_px, width_px, height_px, scale)
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

    /// The face to draw the grapheme `text` with: [`TerminalMetrics::font_for`] its first character,
    /// unless that face lacks a later one.
    ///
    /// A primary face can have a base letter and not the combining mark on it (Andale Mono has `e`
    /// and no U+0301), and drawing `é` in it loses the accent without a trace. Then the face found
    /// for the first character it lacks is used, but only when that face has every character of
    /// the grapheme: one that lacked the base would lose the letter instead of the mark.
    pub fn font_for_text(&self, text: &str, bold: bool, italic: bool) -> Font {
        let first = text.chars().next().unwrap_or(' ');
        let font = self.font_for(first, bold, italic);
        let has = |font: &Font, ch: char| font.unichar_to_glyph(ch as i32) != 0;
        let Some(missing) = text.chars().skip(1).find(|&ch| !has(&font, ch)) else {
            return font;
        };
        let other = self.font_for(missing, bold, italic);
        if text.chars().all(|ch| has(&other, ch)) {
            other
        } else {
            font
        }
    }

    /// The blob that draws `text` (one grapheme) in this weight and slant, positioned at the origin,
    /// as `TextBlob::new(text, &self.font_for_text(..))` makes it -- built once and then reused.
    ///
    /// Every repaint draws every visible glyph. Building each one's blob again cost 1.1 ms of CPU a
    /// frame on a dense 160x50 screen, on the thread that paints, and a new blob is also a new
    /// identity to Skia's GPU text cache, which keys on the blob and so could reuse nothing from the
    /// frame before. The font a grapheme is drawn with depends only on these metrics and the three
    /// keys, so the blob does too; [`TerminalMetrics::rescale`] builds new metrics, and with them an
    /// empty cache. A `None` (nothing to shape) is remembered like a blob.
    pub fn text_blob(&self, text: &str, bold: bool, italic: bool) -> Option<TextBlob> {
        let mut blobs = self.blobs.borrow_mut();
        let cache = &mut blobs[usize::from(bold)][usize::from(italic)];
        if let Some(blob) = cache.get(text) {
            return blob.clone();
        }
        if cache.len() >= BLOB_CACHE_CAP {
            cache.clear();
        }
        let font = self.font_for_text(text, bold, italic);
        let blob = grapheme_blob(text, &font);
        cache.insert(text.to_owned(), blob.clone());
        blob
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
    pub fn size(&self) -> f32 {
        self.size_px
    }
    pub fn families(&self) -> &[String] {
        &self.families
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
    /// Keep one cell-to-pixel transform for backgrounds, glyphs, cursor and overlay so they
    /// cannot land on different lattices.
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

/// The blob for one grapheme, its glyphs placed as `TextBlob::new` places them (each after the last one's advance)
/// except one kind of combining mark: a face can give its marks a whole cell of advance with the ink inside that
/// cell (Menlo), and drawn after its base the accent of `é` lands in the next column. Such a mark is drawn at its
/// base's origin and does not move the pen. Faces whose marks have no advance (their ink reaches back over the base)
/// or whose ink starts before the mark's own origin (JetBrains Mono's, which expect to follow the base) are placed as
/// before, and so are graphemes without a mark: `TextBlob::new` itself.
fn grapheme_blob(text: &str, font: &Font) -> Option<TextBlob> {
    use unicode_width::UnicodeWidthChar;
    let chars: Vec<char> = text.chars().collect();
    let marks = chars.iter().skip(1).any(|ch| ch.width() == Some(0));
    let glyphs = font.str_to_glyphs_vec(text);
    if !marks || glyphs.len() != chars.len() {
        return TextBlob::new(text, font);
    }
    let mut widths = vec![0.0; glyphs.len()];
    font.get_widths(&glyphs, &mut widths);
    let mut bounds = vec![skia_safe::Rect::default(); glyphs.len()];
    font.get_bounds(&glyphs, &mut bounds, None);
    let glyph_list: Vec<PlacedGlyph> = chars
        .iter()
        .zip(&glyphs)
        .zip(widths.iter().zip(&bounds))
        .map(|((ch, glyph), (advance, ink))| PlacedGlyph {
            zero_width: ch.width() == Some(0),
            missing: *glyph == 0,
            advance: *advance,
            ink_left: ink.left,
        })
        .collect();
    let xs = glyph_positions(&glyph_list);
    TextBlob::from_pos_text_h(text, &xs, 0.0, font)
}

/// What [`glyph_positions`] needs to know of one glyph of a grapheme.
#[derive(Clone, Copy, Debug)]
struct PlacedGlyph {
    /// The character is a combining mark (Unicode width 0).
    zero_width: bool,
    /// The face has no glyph for it (glyph 0, the missing-glyph box).
    missing: bool,
    advance: f32,
    /// Where its ink starts, from its own origin.
    ink_left: f32,
}

/// The x of each glyph of a grapheme. As `TextBlob::new` would place it (after the previous glyph's advance), except a
/// mark (not the first glyph) that has a glyph of its own, advances, and has its ink at or after its origin: that one
/// is placed at the origin of the base before it (the last glyph that is not a mark) and does not move the pen.
fn glyph_positions(glyphs: &[PlacedGlyph]) -> Vec<f32> {
    let (mut pen, mut base) = (0.0f32, 0.0f32);
    let mut xs = Vec::with_capacity(glyphs.len());
    for (i, g) in glyphs.iter().enumerate() {
        let mark = i > 0 && g.zero_width;
        if mark && !g.missing && g.advance > 0.0 && g.ink_left >= 0.0 {
            xs.push(base);
            continue;
        }
        if !mark {
            base = pen;
        }
        xs.push(pen);
        pen += g.advance;
    }
    xs
}

/// The family a fallback lookup for `first` is asked in.
///
/// A private-use codepoint means only what the font that defines it says: U+F43A is a clock in a
/// Nerd Font and something else in DejaVu Sans, which also maps it. Asked with no family, fontconfig
/// answers from the `sans-serif` alias, which can turn a Nerd Font clock into another pictogram.
/// Asked in the terminal's own family, it
/// answers from that family's alias list, which is where a user puts the Nerd Font whose icons their
/// tools print -- the fallback chain foot takes too (fcft sorts from the primary font's pattern).
///
/// Every other codepoint is the same character in any font that has it, so it keeps the unnamed
/// lookup: asking in `monospace` there would move CJK to whichever face heads that list (NSimSun,
/// a Song face, on this machine, in place of Microsoft YaHei). Which face CJK gets belongs with
/// making the terminal's face follow nvim's `guifont`, not with this lookup.
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
            .field("families", &self.families)
            .field("size_px", &self.size_px)
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
    fn unknown_families_fall_through_to_the_next_match_then_monospace() {
        let expected = TerminalMetrics::with_font("monospace", 16.0, 800.0, 600.0, 1.0);
        for families in [
            vec!["NoSuchFamily-Eitri-Test".to_owned(), "monospace".to_owned()],
            vec![
                "NoSuchFamily-Eitri-Test".to_owned(),
                "NoSuchFamily-Eitri-Test-2".to_owned(),
            ],
            vec![],
        ] {
            let metrics = TerminalMetrics::with_families(&families, 16.0, 800.0, 600.0, 1.0);
            assert_eq!(metrics.cell_width(), expected.cell_width());
            assert_eq!(metrics.cell_height(), expected.cell_height());
            assert_eq!(metrics.families(), families);
            // The face the generic alias resolves to on this system (`monospace` itself where fontconfig
            // knows it, Menlo on macOS).
            assert_eq!(fallback_family('\u{f43a}', &metrics.family), expected.family);
        }
    }

    #[test]
    fn rescale_keeps_the_requested_families_and_logical_pixel_size() {
        let families = vec!["NoSuchFamily-Eitri-Test".to_owned(), "monospace".to_owned()];
        let metrics = TerminalMetrics::with_families(&families, 16.0, 800.0, 600.0, 1.0);
        let scaled = metrics.rescale(800.0, 600.0, 2.0);
        let expected = TerminalMetrics::with_font("monospace", 16.0, 800.0, 600.0, 2.0);
        assert_eq!(scaled.families(), families);
        assert_eq!(scaled.size(), 16.0);
        assert_eq!(scaled.font().size(), 32.0);
        assert_eq!(scaled.cell_width(), expected.cell_width());
        assert_eq!(scaled.cell_height(), expected.cell_height());
    }

    /// A grapheme's blob is built once per style and reused: the same blob, not a new one with the
    /// same glyphs, so Skia sees one identity across frames. Another style is another blob, and the
    /// cache stays bounded however many distinct graphemes go by.
    #[test]
    fn a_glyphs_blob_is_built_once_and_reused() {
        let metrics = TerminalMetrics::new(800.0, 600.0, 1.0);
        let first = metrics.text_blob("a", false, false).expect("a blob");
        let again = metrics.text_blob("a", false, false).expect("a blob");
        assert_eq!(first.unique_id(), again.unique_id(), "the same blob, reused");
        let bold = metrics.text_blob("a", true, false).expect("a blob");
        assert_ne!(bold.unique_id(), first.unique_id(), "bold is drawn with another font");
        for c in (0x4e00..0x4e00 + BLOB_CACHE_CAP as u32 + 10).filter_map(char::from_u32) {
            metrics.text_blob(&c.to_string(), false, false);
        }
        assert!(metrics.blobs.borrow()[0][0].len() <= BLOB_CACHE_CAP);
    }

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
    fn regression_grid_dimensions_are_bounded_independently_of_font_and_pane() {
        assert_eq!(TerminalMetrics::grid_for(800.0, 600.0, 0.001, 0.001), (4096, 2048));
        assert_eq!(
            TerminalMetrics::grid_for(f32::INFINITY, f32::INFINITY, 1.0, 1.0),
            (4096, 2048)
        );
        assert_eq!(TerminalMetrics::grid_for(80.0, 24.0, 1.0, 1.0), (80, 24));
        assert_eq!(TerminalMetrics::grid_for(0.0, 0.0, 1.0, 1.0), (1, 1));
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

    /// Lualine's clock (U+F43A, nf-oct-clock) came out as another
    /// pictogram, because the unnamed fallback lookup answered DejaVu Sans, which maps that
    /// codepoint too. The face drawn must be the one the terminal's own family would fall back to.
    ///
    /// This depends on the fonts installed, and says so: on a machine where the two lookups agree
    /// for every codepoint below, nothing here can tell the fix from its absence, and the test
    /// prints that instead of passing silently.
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

    /// A combining mark the primary face lacks is not lost with its base: the grapheme is drawn in
    /// a face that has both whenever the fallback finds one.
    ///
    /// This depends on the fonts installed, and says so: where the primary face has U+0301 the old
    /// first-character rule draws it too, and the test prints that it could not fail. Where it does
    /// not (Andale Mono as `monospace`), this was checked red against the first-character rule.
    #[test]
    fn a_combining_mark_the_primary_face_lacks_is_drawn_with_its_base() {
        let metrics = TerminalMetrics::new(800.0, 600.0, 1.0);
        let has = |font: &Font, ch: char| font.unichar_to_glyph(ch as i32) != 0;
        let mark = '\u{301}';
        if has(metrics.font(), mark) {
            eprintln!(
                "a_combining_mark_the_primary_face_lacks_is_drawn_with_its_base: the primary face has \
                 U+0301 on this machine; the check below could not fail"
            );
        }
        let fallback = metrics.font_for(mark, false, false);
        if has(&fallback, 'e') && has(&fallback, mark) {
            let drawn = metrics.font_for_text("e\u{301}", false, false);
            assert!(
                has(&drawn, 'e') && has(&drawn, mark),
                "{}",
                drawn.typeface().family_name()
            );
        } else {
            eprintln!(
                "a_combining_mark_the_primary_face_lacks_is_drawn_with_its_base: the face found for U+0301 \
                 ({}) does not have both `e` and U+0301 on this machine, so no face can draw the grapheme \
                 whole and the check above was not made",
                fallback.typeface().family_name()
            );
        }
        // A grapheme the primary face covers keeps it, whatever the fallback would say.
        assert_eq!(
            metrics.font_for_text("e", false, false).typeface().family_name(),
            metrics.font().typeface().family_name()
        );
    }

    /// The cached blob is shaped in the face `font_for_text` picks, not the first character's: with
    /// a primary face that lacks the combining mark the blob must carry the accent's glyph, so its
    /// bounds are those of a blob built from that face. (Where the primary face has the mark, or no
    /// face has both letters, both faces are the same and this cannot fail.)
    #[test]
    fn the_cached_blob_of_a_grapheme_is_shaped_in_the_face_that_has_all_of_it() {
        let metrics = TerminalMetrics::new(800.0, 600.0, 1.0);
        let text = "e\u{301}";
        let whole = grapheme_blob(text, &metrics.font_for_text(text, false, false)).expect("a blob");
        let cached = metrics.text_blob(text, false, false).expect("a blob");
        assert_eq!(cached.bounds(), whole.bounds());
    }

    /// A base with a combining mark stays in its own cell whatever advance the face gives the mark: Menlo
    /// (macOS) gives it a whole cell, and drawn after the base the accent of `é` landed in the next column. The
    /// oracle is ink on a raster, not the blob's bounds, which are the face's whole box at each glyph.
    #[test]
    fn a_combining_mark_is_drawn_over_its_base_not_in_the_next_cell() {
        use skia_safe::{surfaces, Color, ColorType, ISize, Paint};
        let metrics = TerminalMetrics::new(800.0, 600.0, 1.0);
        let (cw, ch) = (metrics.cell_width(), metrics.cell_height());
        let (w, h) = ((cw * 4.0).ceil() as i32, ch.ceil() as i32);
        for text in ["e\u{301}", "a\u{308}", "o\u{302}"] {
            let mut surface = surfaces::raster_n32_premul(ISize::new(w, h)).expect("a raster surface");
            surface.canvas().clear(Color::WHITE);
            let blob = metrics.text_blob(text, false, false).expect("a blob");
            // In the second cell; the third and fourth must stay blank.
            let mut paint = Paint::default();
            paint.set_color(Color::BLACK);
            surface.canvas().draw_text_blob(&blob, (cw, metrics.baseline()), &paint);
            let info = surface.image_info().with_color_type(ColorType::BGRA8888);
            let row_bytes = info.min_row_bytes();
            let mut px = vec![0u8; row_bytes * h as usize];
            assert!(surface.read_pixels(&info, &mut px, row_bytes, (0, 0)));
            // One pixel of slack for antialiasing at the cell's edge.
            let from = (cw * 2.0).ceil() as usize + 1;
            let past = (0..h as usize)
                .flat_map(|y| (from..w as usize).map(move |x| (x, y)))
                .filter(|&(x, y)| px[y * row_bytes + x * 4] < 128)
                .count();
            assert_eq!(past, 0, "{text:?}: {past} dark pixels past its cell (cell {cw} px)");
            // And the mark is drawn: the cell's ink differs from the base's alone.
            let ink = |px: &[u8]| {
                (0..h as usize)
                    .flat_map(|y| (0..w as usize).map(move |x| (x, y)))
                    .filter(|&(x, y)| px[y * row_bytes + x * 4] < 128)
                    .count()
            };
            let mut bare = surfaces::raster_n32_premul(ISize::new(w, h)).expect("a raster surface");
            bare.canvas().clear(Color::WHITE);
            let base = metrics.text_blob(&text[..1], false, false).expect("a blob");
            bare.canvas().draw_text_blob(&base, (cw, metrics.baseline()), &paint);
            let mut bare_px = vec![0u8; row_bytes * h as usize];
            assert!(bare.read_pixels(&info, &mut bare_px, row_bytes, (0, 0)));
            assert_ne!(ink(&px), ink(&bare_px), "{text:?}: the mark drew nothing");
        }
    }

    fn glyph(zero_width: bool, advance: f32, ink_left: f32) -> PlacedGlyph {
        PlacedGlyph {
            zero_width,
            missing: false,
            advance,
            ink_left,
        }
    }

    /// Menlo's marks advance a cell with their ink inside it: drawn at the base's origin, the pen not moved.
    #[test]
    fn a_mark_that_advances_with_its_ink_inside_goes_over_its_base() {
        let xs = glyph_positions(&[glyph(false, 7.8, 0.6), glyph(true, 7.8, 3.0), glyph(false, 7.8, 0.6)]);
        assert_eq!(xs, [0.0, 0.0, 7.8]);
    }

    /// Marks with no advance (DejaVu, Monaco), and marks that advance with their ink before their origin (JetBrains
    /// Mono: -368..-165 units in a 600-unit cell), are placed as `TextBlob::new` places them.
    #[test]
    fn other_marks_keep_the_plain_placement() {
        assert_eq!(
            glyph_positions(&[glyph(false, 7.8, 0.6), glyph(true, 0.0, -5.5)]),
            [0.0, 7.8]
        );
        assert_eq!(
            glyph_positions(&[glyph(false, 6.0, 0.5), glyph(true, 6.0, -3.68)]),
            [0.0, 6.0]
        );
    }

    /// A missing glyph (an unsupported variation selector's box) is never moved over its base.
    #[test]
    fn a_missing_glyph_keeps_the_plain_placement() {
        let mut selector = glyph(true, 7.8, 0.5);
        selector.missing = true;
        assert_eq!(glyph_positions(&[glyph(false, 15.6, 0.5), selector]), [0.0, 15.6]);
    }

    /// A zero-advance mark does not become the base of the next mark: an advancing mark after it still goes to the
    /// letter's origin.
    #[test]
    fn stacked_marks_keep_the_letter_as_their_base() {
        let xs = glyph_positions(&[glyph(false, 7.8, 0.6), glyph(true, 0.0, -5.0), glyph(true, 7.8, 3.0)]);
        assert_eq!(xs, [0.0, 7.8, 0.0]);
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
        assert!(two.cell_width() > one.cell_width() * 1.8, "{one:?} vs {two:?}");
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
