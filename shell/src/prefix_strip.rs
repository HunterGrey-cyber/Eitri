//! What the top bar shows while the prefix waits (modules spec §6.5): the module keys --
//! `e editor · a agent · t terminal · <Lua keys>`, a hidden module dimmed -- then the verbs, as the effective keymap binds them (`Keymap::strip_verbs`). After
//! `\` or `"`, only the module keys, under where the module will go. Drawn as a row of labels in
//! the top bar beside the app name, which already turns into a solid block while the prefix waits.
//!
//! **The `·` between runs is the spec's**, and was missing from the first build, which spaced the
//! runs 10px apart (modules P2's GUI pass, C1, 2026-09-24). Several runs have spaces of their own
//! (`| _ even`, `H J K L swap`), so a space alone left where one run ended to the eye:
//! `| _ even  H J K L swap`. The same pass saw the last run touch the project name
//! (`H J K L swapproj`); `.prefix-strip`'s right margin (`theme::gtk_css`) is that half.

use eitri_core::keymap::companion::StripPiece;
use eitri_core::layout::{Axis, ModuleId, StripEntry};
use gtk4::prelude::*;

use crate::prefix::Waiting;

/// The separator between two runs.
const DOT: &str = "\u{00b7}";

/// The strip's space between two pieces: about one space of its 12px text either side of a `·`, so a
/// run, its `·` and the next run are spaced as `e editor · a agent` would be in running text. With
/// the `·`'s own advance (about 4px at 12px in a sans face; not measured on a screen) that is about
/// 10px from run to run, what the strip had before the `·` -- so the armed bar's minimum width stays
/// where it was: every run ellipsizes, the `·` does not.
const PIECE_SPACING: i32 = 3;

/// The strip's pieces, left to right. `entries` are the module keys for `waiting`: the keys that reach
/// each module now while armed (`eitri_core::layout::strip_direct`), the fixed module keys after a
/// split key (`eitri_core::layout::strip`). `focus_title` names the module with the keys (where `\`/`"`
/// put the next module); `title` names any module.
pub(crate) fn strip_pieces(
    waiting: Waiting,
    entries: &[StripEntry],
    focus_title: &str,
    title: &dyn Fn(&ModuleId) -> String,
    verbs: &[String],
) -> Vec<StripPiece> {
    let modules = entries.iter().map(|entry| StripPiece::Run {
        text: format!("{} {}", entry.key, title(&entry.module)),
        dimmed: entry.dimmed,
    });
    let (heading, runs): (Option<String>, Vec<StripPiece>) = match waiting {
        Waiting::No => return Vec::new(),
        Waiting::Command => (
            None,
            modules
                .chain(verbs.iter().map(|verb| StripPiece::Run {
                    text: verb.clone(),
                    dimmed: false,
                }))
                .collect(),
        ),
        Waiting::Module(axis) => (
            Some(match axis {
                Axis::Row => format!("right of {focus_title}:"),
                Axis::Column => format!("below {focus_title}:"),
            }),
            modules.collect(),
        ),
    };
    let mut pieces: Vec<StripPiece> = heading.into_iter().map(StripPiece::Heading).collect();
    for (i, run) in runs.into_iter().enumerate() {
        if i > 0 {
            pieces.push(StripPiece::Dot);
        }
        pieces.push(run);
    }
    pieces
}

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

    /// Replaces what the strip shows; nothing hides it.
    ///
    /// Each run and the heading ellipsize, so the strip's minimum width is a few ellipses rather than
    /// every key and verb spelled out: armed, it is about 490px at 12px, and a label that cannot
    /// shrink raises the top bar's minimum -- and so the window's -- above a half-tiled window's
    /// width, which GTK answers by growing the window (the whole-branch review's window finding 4).
    /// Short of room, a run reads `x hi…` rather than pushing the window controls off. A `·` is one
    /// narrow glyph and does not ellipsize. known limit: GTK layout, so no headless test holds it;
    /// `shell/MANUAL_VERIFICATION.md`'s Modules P2 item 11 looks at it.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn entries() -> Vec<StripEntry> {
        vec![
            StripEntry {
                key: "e".into(),
                module: ModuleId::editor(),
                dimmed: false,
            },
            StripEntry {
                key: "a".into(),
                module: ModuleId::agent(),
                dimmed: false,
            },
            StripEntry {
                key: "t".into(),
                module: ModuleId::terminal(),
                dimmed: true,
            },
        ]
    }

    fn title(id: &ModuleId) -> String {
        id.as_str().to_string()
    }

    /// The strip as it would read: each piece's text, a heading and a run as themselves, a `·` as
    /// itself, a dimmed run in brackets; joined by one space.
    fn verbs() -> Vec<String> {
        eitri_core::keymap::Keymap::defaults().strip_verbs()
    }

    fn reads(pieces: &[StripPiece]) -> String {
        pieces
            .iter()
            .map(|piece| match piece {
                StripPiece::Heading(text) => text.clone(),
                StripPiece::Run { text, dimmed: false } => text.clone(),
                StripPiece::Run { text, dimmed: true } => format!("[{text}]"),
                StripPiece::Dot => "\u{00b7}".to_string(),
            })
            .collect::<Vec<_>>()
            .join(" ")
    }

    #[test]
    fn armed_it_lists_the_module_keys_then_the_verbs() {
        let pieces = strip_pieces(Waiting::Command, &entries(), "editor", &title, &verbs());
        assert_eq!(
            reads(&pieces),
            "e editor \u{b7} a agent \u{b7} [t terminal] \u{b7} x kill \u{b7} % right \u{b7} \" below \u{b7} \
             M-1 M-2 even \u{b7} { } swap",
            "a hidden module is dimmed"
        );
    }

    #[test]
    fn after_a_split_key_it_lists_only_the_module_keys_under_where_they_go() {
        let below = strip_pieces(Waiting::Module(Axis::Column), &entries(), "agent", &title, &verbs());
        assert_eq!(
            reads(&below),
            "below agent: e editor \u{b7} a agent \u{b7} [t terminal]"
        );
        let right = strip_pieces(Waiting::Module(Axis::Row), &entries(), "agent", &title, &verbs());
        assert_eq!(right[0], StripPiece::Heading("right of agent:".to_string()));
        assert!(strip_pieces(Waiting::No, &entries(), "agent", &title, &verbs()).is_empty());
    }

    /// The GUI pass's C1 (2026-09-24): the build spaced the runs with no `·`, so a run with spaces of
    /// its own ran into the next (`| _ even  H J K L swap`). Spec §6.5 writes a `·` between every two
    /// runs -- module keys and verbs alike -- and none before the first, after the last, or after the
    /// heading (`below agent: e editor · a agent`), whatever the number of runs.
    #[test]
    fn a_dot_stands_between_every_two_runs_and_nowhere_else() {
        for n in 0..=3 {
            let some = &entries()[..n];
            for waiting in [
                Waiting::Command,
                Waiting::Module(Axis::Row),
                Waiting::Module(Axis::Column),
            ] {
                let pieces = strip_pieces(waiting, some, "agent", &title, &verbs());
                let body: &[StripPiece] = match pieces.first() {
                    Some(StripPiece::Heading(_)) => &pieces[1..],
                    _ => &pieces[..],
                };
                assert!(
                    body.iter().all(|p| !matches!(p, StripPiece::Heading(_))),
                    "{waiting:?}, {n}: {pieces:?}"
                );
                let runs = body.iter().filter(|p| matches!(p, StripPiece::Run { .. })).count();
                for (i, piece) in body.iter().enumerate() {
                    let expect_run = i % 2 == 0;
                    assert_eq!(
                        matches!(piece, StripPiece::Run { .. }),
                        expect_run,
                        "{waiting:?}, {n}: runs and dots alternate, a run first: {pieces:?}"
                    );
                }
                assert_eq!(
                    body.len(),
                    if runs == 0 { 0 } else { 2 * runs - 1 },
                    "{waiting:?}, {n}: a run last: {pieces:?}"
                );
            }
        }
    }
}
