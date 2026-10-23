//! The top bar draws core's prefix strip pieces as plain labels separated by dots.

use eitri_core::wm::prefix_strip::StripPiece;
use gtk4::prelude::*;

/// The separator between two runs.
const DOT: &str = "\u{00b7}";

/// The strip's space between two pieces: about one space of its 12px text either side of a `·`, so a
/// run, its `·` and the next run are spaced as `e editor · a agent` would be in running text. With
/// the `·`'s own advance (about 4px at 12px in a sans face; not measured on a screen) that is about
/// 10px from run to run, what the strip had before the `·` -- so the armed bar's minimum width stays
/// where it was: every run ellipsizes, the `·` does not.
const PIECE_SPACING: i32 = 3;

/// The strip's widget, empty and hidden until the prefix waits.
pub(crate) struct PrefixStrip {
    widget: gtk4::Box,
}

impl PrefixStrip {
    pub(crate) fn new() -> PrefixStrip {
        let widget = gtk4::Box::new(gtk4::Orientation::Horizontal, PIECE_SPACING);
        widget.add_css_class("prefix-strip");
        widget.set_valign(gtk4::Align::Center);
        widget.set_visible(false);
        PrefixStrip { widget }
    }

    pub(crate) fn widget(&self) -> &gtk4::Box {
        &self.widget
    }

    /// Replaces the strip's contents.
    ///
    /// Each run and heading ellipsizes so the strip can shrink to a few ellipses. Fully spelled out,
    /// the armed strip is about 490px at 12px. Labels that cannot shrink raise the top bar's minimum
    /// width and can make GTK grow the window beyond a half-tiled width.
    /// Short of room, a run reads `x hi` followed by an ellipsis instead of pushing the window controls off.
    /// The narrow `·` glyph does not ellipsize. This depends on GTK layout and is not covered by
    /// headless tests.
    pub(crate) fn show(&self, pieces: &[StripPiece]) {
        while let Some(child) = self.widget.first_child() {
            self.widget.remove(&child);
        }
        for piece in pieces {
            let label = match piece {
                StripPiece::Heading(text) | StripPiece::Run { text, .. } => {
                    let label = gtk4::Label::new(Some(text));
                    label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
                    if let StripPiece::Run { dimmed: true, .. } = piece {
                        label.add_css_class("dimmed");
                    }
                    label
                }
                StripPiece::Dot => {
                    let dot = gtk4::Label::new(Some(DOT));
                    dot.add_css_class("strip-dot");
                    dot
                }
            };
            self.widget.append(&label);
        }
        self.widget.set_visible(!pieces.is_empty());
    }
}
