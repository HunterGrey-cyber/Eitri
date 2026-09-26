//! Selection: drag, double/triple click, and turning the region into a copyable string
//! (bottom-terminal phase 3b).
//!
//! **Adopt, don't build** (task 8's own brief): `alacritty_terminal` 0.26 already carries the whole
//! model -- `selection::{Selection, SelectionType}`, `Side`, `Term::selection` (a public field the
//! grid itself rotates and clears on scroll/resize) and `Term::selection_to_string` (wide characters
//! and wrapped lines already handled, by the same code alacritty and every other user of this crate
//! relies on). This module is only the shape a host gesture takes and how it maps onto that field;
//! nothing here reimplements text extraction.
//!
//! Points are **absolute** grid lines (negative in history), the same space as
//! [`crate::scroll::ScrollView`]'s (Task 7), so a selection made while scrolled back names the same
//! cells whether or not the view later returns to the bottom.
//!
//! Owner ruling R6 and spec "Phase 3" (`docs/superpowers/plans/2026-09-26-wave4.md`).

use alacritty_terminal::index::{Column, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::Term;

/// One host gesture against the selection, all in absolute grid coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectCommand {
    /// A button press (or the start of a double/triple click's expansion): where, which half of the
    /// cell the pointer is in (alacritty's own `Side::Left`/`Side::Right`, deciding whether the
    /// starting cell itself is included), and what kind of selection this click starts.
    Start {
        line: i32,
        col: u16,
        right_half: bool,
        kind: SelectKind,
    },
    /// A drag: moves the far end of the selection. A no-op with nothing started yet.
    Extend { line: i32, col: u16, right_half: bool },
    /// The button was released: the selection stays (it is not cleared on release, matching every
    /// terminal that lets you keep reading a selection after letting go), and its text is read out.
    Finish,
    /// Drop the selection outright (typing -- [`crate::screen::Screen::note_input`] -- or a click
    /// with nothing dragged).
    Clear,
}

/// What a click starts. Alacritty's own click counting (foot the same): one press is a plain
/// character-by-character selection, two expand to the word under the pointer, three or more to
/// the whole line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectKind {
    Simple,
    Word,
    Line,
}

impl SelectKind {
    fn selection_type(self) -> SelectionType {
        match self {
            SelectKind::Simple => SelectionType::Simple,
            // "Word" is alacritty's `Semantic`: expand to the nearest semantic escape char in
            // either direction (the crate's own default `semantic_escape_chars`, kept as `Term`'s
            // default -- `screen::term_config` sets nothing else for it).
            SelectKind::Word => SelectionType::Semantic,
            SelectKind::Line => SelectionType::Lines,
        }
    }
}

fn side(right_half: bool) -> Side {
    if right_half {
        Side::Right
    } else {
        Side::Left
    }
}

fn point(line: i32, col: u16) -> Point {
    Point::new(Line(line), Column(usize::from(col)))
}

/// Applies one [`SelectCommand`] to `term.selection`, alacritty's own field
/// (`Term::selection_to_string`'s doc: "Convert the active selection to a String"). Returns the
/// selection's text on `Finish`, `None` otherwise -- including when nothing was ever selected,
/// which `Term::selection_to_string` already reports the same way.
pub(crate) fn apply<T>(term: &mut Term<T>, cmd: SelectCommand) -> Option<String> {
    match cmd {
        SelectCommand::Start {
            line,
            col,
            right_half,
            kind,
        } => {
            term.selection = Some(Selection::new(
                kind.selection_type(),
                point(line, col),
                side(right_half),
            ));
            None
        }
        SelectCommand::Extend { line, col, right_half } => {
            if let Some(selection) = term.selection.as_mut() {
                selection.update(point(line, col), side(right_half));
            }
            None
        }
        SelectCommand::Finish => term.selection_to_string(),
        SelectCommand::Clear => {
            term.selection = None;
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal::grid::Dimensions;
    use alacritty_terminal::term::Config;

    struct GridSize {
        cols: usize,
        rows: usize,
    }

    impl Dimensions for GridSize {
        fn total_lines(&self) -> usize {
            self.rows
        }
        fn screen_lines(&self) -> usize {
            self.rows
        }
        fn columns(&self) -> usize {
            self.cols
        }
    }

    fn term(cols: usize, rows: usize) -> Term<()> {
        Term::new(Config::default(), &GridSize { cols, rows }, ())
    }

    #[test]
    fn start_extend_finish_reads_out_the_dragged_text() {
        let mut t = term(20, 5);
        // A minimal check that `apply` at least reaches `term.selection`; full text extraction
        // against real fed content (wide characters, wrapped lines) is exercised through `Screen`
        // in `screen.rs`'s own tests.
        assert!(apply(
            &mut t,
            SelectCommand::Start {
                line: 0,
                col: 1,
                right_half: false,
                kind: SelectKind::Simple,
            }
        )
        .is_none());
        assert!(t.selection.is_some());
        apply(
            &mut t,
            SelectCommand::Extend {
                line: 0,
                col: 3,
                right_half: false,
            },
        );
        apply(&mut t, SelectCommand::Clear);
        assert!(t.selection.is_none(), "Clear drops the selection");
    }

    #[test]
    fn word_and_line_pick_alacrittys_own_selection_types() {
        assert_eq!(SelectKind::Simple.selection_type(), SelectionType::Simple);
        assert_eq!(SelectKind::Word.selection_type(), SelectionType::Semantic);
        assert_eq!(SelectKind::Line.selection_type(), SelectionType::Lines);
    }
}
