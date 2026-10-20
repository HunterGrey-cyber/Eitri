//! While the prefix waits, the top bar shows module keys, then the effective keymap's verbs
//! (`Keymap::strip_verbs`): `e editor · a agent · t terminal · <Lua keys>`, with hidden modules dimmed.
//! After `\` or `"`, only module keys appear under a heading naming their destination.
//! The labels sit beside the app name, which becomes a solid block while the prefix waits.
//!
//! A `·` separates runs because runs such as `| _ even` and `H J K L swap` contain spaces themselves.
//! Spaces alone leave their boundaries unclear. The right margin on `.prefix-strip` in
//! `theme::gtk_css` keeps the last run from touching the project name.

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
    let after_split = matches!(waiting, Waiting::Module(_));
    let modules = entries.iter().map(|entry| StripPiece::Run {
        text: entry_text(entry, title, after_split),
        dimmed: entry.dimmed,
    });
    let (heading, runs): (Option<String>, Vec<StripPiece>) = match waiting {
        Waiting::No => return Vec::new(),
        Waiting::Number => return Vec::new(),
        Waiting::Workspace { .. } => return Vec::new(),
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

/// One module key's run: `t terminal`, `t terminal ×3` while three terminals are in the layout (the key
/// reaches one of them, `eitri_core::layout::module_target`'s), and after a split key that makes a new
/// terminal (the keys are in a terminal), `t new terminal`. Armed, the same key in a terminal toggles
/// that terminal, so the strip never says `new` there.
fn entry_text(entry: &StripEntry, title: &dyn Fn(&ModuleId) -> String, after_split: bool) -> String {
    if after_split && entry.new_instance {
        format!("{} new {}", entry.key, title(&entry.module))
    } else if entry.count > 1 {
        format!("{} {} \u{00d7}{}", entry.key, title(&entry.module), entry.count)
    } else {
        format!("{} {}", entry.key, title(&entry.module))
    }
}

/// The strip while `select.number` waits for a digit: each number and the module it reaches,
/// `1 editor · 2 agent`, in the order the digits were drawn. Empty with no modules to number.
pub(crate) fn number_pieces(plan: &[(char, ModuleId)], title: &dyn Fn(&ModuleId) -> String) -> Vec<StripPiece> {
    let mut pieces = Vec::new();
    for (i, (digit, id)) in plan.iter().enumerate() {
        if i > 0 {
            pieces.push(StripPiece::Dot);
        }
        pieces.push(StripPiece::Run {
            text: format!("{digit} {}", title(id)),
            dimmed: false,
        });
    }
    pieces
}

/// The digit the workspace chooser reads for the workspace at `index` in the window's order: `1`-`9`
/// for the first nine and `0` for the tenth, as tmux's `display-panes` numbers panes; none after
/// that (those are reached by the previous/next keys and Lua).
pub(crate) fn workspace_digit(index: usize) -> Option<char> {
    match index {
        0..=8 => char::from_digit(index as u32 + 1, 10),
        9 => Some('0'),
        _ => None,
    }
}

/// The strip while the workspace chooser waits: each workspace a digit reaches, by its digit, then
/// `c new` and `x delete` -- `1 main · 2 study · c new · x delete`. After `x`, the heading `delete:`
/// and only the workspaces that can be deleted: every one but `main` and the current one, each by
/// the digit that deletes it.
pub(crate) fn workspace_pieces(order: &[String], current: &str, deleting: bool) -> Vec<StripPiece> {
    let mut runs: Vec<String> = order
        .iter()
        .enumerate()
        .filter(|(_, name)| !deleting || (name.as_str() != eitri_core::layout::workspace::MAIN && *name != current))
        .filter_map(|(i, name)| workspace_digit(i).map(|digit| format!("{digit} {name}")))
        .collect();
    if !deleting {
        runs.push("c new".to_string());
        runs.push("x delete".to_string());
    }
    let mut pieces = Vec::new();
    if deleting {
        pieces.push(StripPiece::Heading("delete:".to_string()));
    }
    for (i, text) in runs.into_iter().enumerate() {
        if i > 0 {
            pieces.push(StripPiece::Dot);
        }
        pieces.push(StripPiece::Run { text, dimmed: false });
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

#[cfg(test)]
mod tests {
    use super::*;

    fn entries() -> Vec<StripEntry> {
        vec![
            StripEntry {
                key: "e".into(),
                module: ModuleId::editor(),
                dimmed: false,
                count: 1,
                new_instance: false,
            },
            StripEntry {
                key: "a".into(),
                module: ModuleId::agent(),
                dimmed: false,
                count: 1,
                new_instance: false,
            },
            StripEntry {
                key: "t".into(),
                module: ModuleId::terminal(),
                dimmed: true,
                count: 1,
                new_instance: false,
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

    #[test]
    fn a_module_key_with_several_instances_shows_their_count_and_a_new_one_says_so() {
        let mut entries = entries();
        entries[2].dimmed = false;
        entries[2].count = 3;
        let armed = strip_pieces(Waiting::Command, &entries, "terminal", &title, &[]);
        assert_eq!(reads(&armed), "e editor \u{b7} a agent \u{b7} t terminal \u{d7}3");
        entries[2].new_instance = true;
        let armed = strip_pieces(Waiting::Command, &entries, "terminal", &title, &[]);
        assert_eq!(
            reads(&armed),
            "e editor \u{b7} a agent \u{b7} t terminal \u{d7}3",
            "armed, the terminal's key toggles it rather than making one"
        );
        let after_split = strip_pieces(Waiting::Module(Axis::Row), &entries, "terminal", &title, &[]);
        assert_eq!(
            reads(&after_split),
            "right of terminal: e editor \u{b7} a agent \u{b7} t new terminal"
        );
    }

    #[test]
    fn waiting_for_a_digit_it_lists_each_number_and_its_module() {
        let plan = [('1', ModuleId::editor()), ('2', ModuleId::agent())];
        assert_eq!(reads(&number_pieces(&plan, &title)), "1 editor \u{b7} 2 agent");
        assert!(number_pieces(&[], &title).is_empty());
    }

    #[test]
    fn the_chooser_strip_numbers_workspaces_and_offers_new_and_delete() {
        let order: Vec<String> = ["main", "study", "review"].iter().map(|n| n.to_string()).collect();
        assert_eq!(
            reads(&workspace_pieces(&order, "study", false)),
            "1 main \u{b7} 2 study \u{b7} 3 review \u{b7} c new \u{b7} x delete"
        );
        assert_eq!(
            reads(&workspace_pieces(&order, "study", true)),
            "delete: 3 review",
            "neither main nor the current workspace is offered for deletion"
        );
        let many: Vec<String> = (1..=11).map(|n| format!("w{n}")).collect();
        let strip = reads(&workspace_pieces(&many, "w1", false));
        assert!(strip.contains("9 w9 \u{b7} 0 w10 \u{b7} c new"), "{strip}");
        assert!(!strip.contains("w11"), "no digit reaches the eleventh: {strip}");
    }

    /// A `·` separates every two runs, including module keys and verbs, so runs with spaces of their
    /// own stay distinct. No separator precedes the first run, follows the last, or follows the heading
    /// (`below agent: e editor · a agent`), regardless of the number of runs.
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
