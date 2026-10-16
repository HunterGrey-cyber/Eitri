//! The agent panel's CSS px, in GTK logical px: WebKitGTK's own page zoom, mirrored.
//!
//! WebKitGTK zooms every page by the desktop's text scaling. `refreshInternalScaling`
//! (`Source/WebKit/UIProcess/API/gtk/WebKitWebViewBase.cpp`, tag `webkitgtk-2.52.6`) sets the page
//! zoom to `fontDPI() / 96`, and `fontDPI()` (`Source/WebCore/platform/gtk/PlatformScreenGtk.cpp`) is
//! `gtk-xft-dpi / 1024`, read from the default `GtkSettings` and re-read on `notify::gtk-xft-dpi`
//! (`Source/WebKit/UIProcess/gtk/SystemSettingsManagerProxyGtk.cpp`). It is a *page* zoom
//! (`setPageZoomFactor`), so one CSS px is `page scale` GTK logical px, and `devicePixelRatio` is the
//! widget's integer scale factor times it (`LocalDOMWindow::devicePixelRatio`). That is what the
//! measurements showed: `gtk-xft-dpi=147456` (a 144 dpi
//! value) gave 1.5 at scale 1 and 3 at scale 2, and 98304 gave 1 and 2. The monitor's scale does
//! not enter the page zoom: `refreshInternalScaling` also runs on a scale or monitor change, but
//! with the same `fontDPI()`, and `xft_dpi::ensure_xft_dpi` never leaves the setting unset -- the
//! one case where `fontDPI()` would read the monitor instead.
//!
//! So a length `shell` measures in GTK logical px (the editor's cell height) is a different length
//! in the panel: at 144 dpi, 22 logical px sent as `22px` drew a band 33 logical px high, its top
//! 11px into the editor's statusline. [`EditorRow`] keeps that one
//! length in the editor's unit and hands the panel the CSS px it needs.
//!
//! The editor's font size is converted the same way ([`EditorFont`]): by default the panel's text is
//! the editor's size (2026-10-15), and the editor's text is not zoomed by the desktop's text scaling.
//! Everything else in the panel is a ratio of that base, so it follows. An `agent.font_size` the user
//! set is a CSS px size, as it always was, and still zoomed by WebKit.
//! `shell/tests/band_xft_zoom.rs` holds this module against the real engine.

/// WebKitGTK's `pageScaleFactor` for one `WebView`. It starts at 1.0 (`WebKitWebViewBasePrivate`'s
/// initializer) and moves only when the new scale differs from the applied one by more than 2%, so
/// it is a fold over the `gtk-xft-dpi` values seen, not a function of the current one.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct PageScale(f64);

impl PageScale {
    pub(crate) const INITIAL: PageScale = PageScale(1.0);

    /// Where WebKit's `refreshInternalScaling` leaves the scale once `gtk-xft-dpi` reads `xft_dpi`.
    pub(crate) fn follow(self, xft_dpi: i32) -> PageScale {
        // `-1` (unset) never reaches WebKit (`SystemSettingsManagerProxy::settingsDidChange` skips
        // it), and `xft_dpi`'s guard replaces anything `<= 0` before WebKit reads the live value.
        if xft_dpi <= 0 {
            return self;
        }
        // `fontDPI() / 96.`, with `fontDPI()` = `xftDPI / 1024.0`.
        let scale = f64::from(xft_dpi) / 1024.0 / 96.0;
        if (scale / self.0 - 1.0).abs() > 0.02 {
            PageScale(scale)
        } else {
            self
        }
    }

    /// `logical_px` GTK logical px, in the panel's CSS px.
    pub(crate) fn css_px(self, logical_px: f32) -> f32 {
        (f64::from(logical_px) / self.0) as f32
    }
}

/// The editor's cell height, which the panel's bottom band must match.
/// Stores the GTK logical px reported by `neovide-editor` and sends CSS px to the
/// panel as `--nv-editor-row`. `main.rs` supplies new cell heights and `gtk-xft-dpi`
/// changes, then sends the returned values.
#[derive(Clone, Copy, Debug)]
pub(crate) struct EditorRow {
    logical_px: Option<f32>,
    scale: PageScale,
}

impl EditorRow {
    /// `xft_dpi` is `gtk-xft-dpi` as it was when the panel's `WebView` was created.
    pub(crate) fn new(xft_dpi: i32) -> Self {
        EditorRow {
            logical_px: None,
            scale: PageScale::INITIAL.follow(xft_dpi),
        }
    }

    /// `--nv-editor-row`, in CSS px; `None` until the editor has reported a cell height.
    pub(crate) fn css_px(&self) -> Option<f32> {
        self.logical_px.map(|h| self.scale.css_px(h))
    }

    /// The editor reported a cell height of `logical_px`; returns the new `--nv-editor-row`.
    pub(crate) fn set_cell_height(&mut self, logical_px: f32) -> f32 {
        self.logical_px = Some(logical_px);
        self.scale.css_px(logical_px)
    }

    /// `gtk-xft-dpi` now reads `xft_dpi`. `Some(new --nv-editor-row)` when the panel's zoom moved and
    /// the editor has reported a row; `None` when there is nothing to re-send.
    pub(crate) fn follow_xft_dpi(&mut self, xft_dpi: i32) -> Option<f32> {
        let scale = self.scale.follow(xft_dpi);
        if scale == self.scale {
            return None;
        }
        self.scale = scale;
        self.css_px()
    }
}

/// The editor's font size, which the panel's text equals by default (`agent.font_size` unset): kept in
/// the unit the editor draws in (GTK logical px, before `g:neovide_scale_factor`, which the panel's zoom
/// follows on its own) and handed to the panel as its base size in CSS px. Without this conversion the
/// panel's text was WebKit's page zoom larger than the editor's at any desktop text scaling but 100%:
/// 1.5 times at 144 dpi. `main.rs` feeds it the same two events [`EditorRow`] takes -- a new
/// `guifont` and a `gtk-xft-dpi` change -- and passes on whatever they return.
#[derive(Clone, Copy, Debug)]
pub(crate) struct EditorFont {
    logical_px: f32,
    scale: PageScale,
}

impl EditorFont {
    /// `xft_dpi` is `gtk-xft-dpi` as it was when the panel's `WebView` was created; `logical_px` is the
    /// size the editor starts at (`eitri_core::theme::guifont_size_px` of an unset `guifont`).
    pub(crate) fn new(xft_dpi: i32, logical_px: f32) -> Self {
        EditorFont {
            logical_px,
            scale: PageScale::INITIAL.follow(xft_dpi),
        }
    }

    /// The panel's base size, in CSS px.
    pub(crate) fn css_px(&self) -> f32 {
        self.scale.css_px(self.logical_px)
    }

    /// The editor's `guifont` now reads `logical_px`; `Some(new base)` when that changed it.
    pub(crate) fn set_logical_px(&mut self, logical_px: f32) -> Option<f32> {
        if logical_px == self.logical_px {
            return None;
        }
        self.logical_px = logical_px;
        Some(self.css_px())
    }

    /// `gtk-xft-dpi` now reads `xft_dpi`; `Some(new base)` when the panel's zoom moved.
    pub(crate) fn follow_xft_dpi(&mut self, xft_dpi: i32) -> Option<f32> {
        let scale = self.scale.follow(xft_dpi);
        if scale == self.scale {
            return None;
        }
        self.scale = scale;
        Some(self.css_px())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// At 144 dpi, an unset `guifont` gives 14 points (18.67 logical px). The panel's
    /// base is 18.67 / 1.5 CSS px, which WebKit's page zoom draws at the editor's own 18.67.
    #[test]
    fn the_editor_font_is_handed_over_in_css_px_and_follows_both_events() {
        let mut font = EditorFont::new(DPI_144, 56.0 / 3.0);
        assert!(close(font.css_px(), 56.0 / 3.0 / 1.5));
        assert_eq!(font.set_logical_px(56.0 / 3.0), None);
        assert!(close(font.set_logical_px(16.0).unwrap(), 16.0 / 1.5));
        assert!(close(font.follow_xft_dpi(DPI_96).unwrap(), 16.0));
        assert_eq!(font.follow_xft_dpi(DPI_96), None);
        let at_96 = EditorFont::new(DPI_96, 56.0 / 3.0);
        assert!(close(at_96.css_px(), 56.0 / 3.0));
    }

    const DPI_96: i32 = 96 * 1024;
    const DPI_144: i32 = 147_456; // 144 dpi in `gtk-xft-dpi` units

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-4
    }

    /// The v1-scale pass's defect A: at 144 dpi a 22 logical px row is 22 / 1.5 CSS px, so the band
    /// is 22 logical px high rather than 33.
    #[test]
    fn at_144_dpi_the_row_is_two_thirds_as_many_css_px() {
        let mut row = EditorRow::new(DPI_144);
        let css = row.set_cell_height(22.0);
        assert!(close(css, 22.0 / 1.5), "{css}");
        assert!(close(css * 1.5, 22.0), "back in logical px: {}", css * 1.5);
        assert_eq!(row.css_px(), Some(css));
    }

    #[test]
    fn at_96_dpi_css_px_are_logical_px() {
        let mut row = EditorRow::new(DPI_96);
        assert_eq!(row.css_px(), None, "nothing until the editor reports a row");
        assert_eq!(row.set_cell_height(22.0), 22.0);
    }

    /// WebKit follows `notify::gtk-xft-dpi` live, so the band has to as well -- including a row the
    /// editor reported before the change.
    #[test]
    fn a_dpi_change_re_sends_the_row() {
        let mut row = EditorRow::new(DPI_96);
        assert_eq!(
            row.follow_xft_dpi(DPI_144),
            None,
            "no row reported yet: nothing to send"
        );
        assert!(close(row.set_cell_height(22.0), 22.0 / 1.5));
        let back = row.follow_xft_dpi(DPI_96).expect("the zoom moved back to 1");
        assert!(close(back, 22.0), "{back}");
    }

    /// `refreshInternalScaling` leaves the zoom where it is for a change of 2% or less, so a
    /// re-derivation from the current value alone would be off by up to 2%.
    #[test]
    fn a_change_of_two_percent_or_less_leaves_webkit_s_zoom_alone() {
        let mut row = EditorRow::new(97 * 1024); // 97/96 = 1.0104
        assert!(close(row.set_cell_height(22.0), 22.0), "WebKit stayed at 1.0");
        let mut row = EditorRow::new(DPI_144);
        row.set_cell_height(22.0);
        assert_eq!(
            row.follow_xft_dpi(146 * 1024),
            None,
            "146/144 is within 2%: WebKit stays at 1.5"
        );
        assert!(close(row.css_px().unwrap(), 22.0 / 1.5));
        let moved = row.follow_xft_dpi(120 * 1024).expect("120/144 is not");
        assert!(close(moved, 22.0 / 1.25), "{moved}");
    }

    /// `-1` (GTK's unset) is never forwarded to WebKit (`SystemSettingsManagerProxy::settingsDidChange`),
    /// and `xft_dpi`'s guard replaces anything `<= 0` inside the same notify, before WebKit (which
    /// reads the live value) sees it; neither moves the zoom.
    #[test]
    fn an_unset_dpi_leaves_the_zoom_alone() {
        let mut row = EditorRow::new(DPI_144);
        row.set_cell_height(22.0);
        assert_eq!(row.follow_xft_dpi(-1), None);
        assert_eq!(row.follow_xft_dpi(0), None);
        assert!(close(row.css_px().unwrap(), 22.0 / 1.5));
    }
}
