//! AN INDEPENDENT REFERENCE PROJECTION.
//!
//! Written from `alacritty_terminal`'s types directly and deliberately sharing
//! NO code with `terminal_frame::project` or `terminal_frame::assemble`.
//!
//! WHY IT EXISTS. The first version of this suite compared a delta-driven
//! `FrameAssembler` against a Full-driven `FrameAssembler`. That is a real
//! differential for the *damage* question, but both sides read through the same
//! accessors and the same `project_cell`, so a mutation of either mutated both
//! sides identically and the comparison still passed. `cargo mutants` found
//! thirteen such survivors in `assemble.rs` alone -- `FrameAssembler::cursor`
//! replaced with `Default::default()`, `palette` replaced with an empty slice,
//! `||` swapped for `&&` in the bounds check of `cell` -- every one of them a
//! silent corruption of what a consumer would actually see.
//!
//! So the truth a test compares against is the `Term` itself, read here in
//! upstream's own vocabulary.
#![allow(dead_code)]

use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Term, TermMode};
use alacritty_terminal::vte::ansi::{Color, CursorShape};

use terminal_frame::{
    CellFlags, FrameAssembler, FrameCell, FrameColor, FrameCursor, FrameCursorShape, Rgb, TerminalModes, PALETTE_LEN,
};

/// `vte::ansi::Color` -> `FrameColor`, written independently.
fn color(color: Color) -> FrameColor {
    match color {
        Color::Spec(rgb) => FrameColor::Rgb(Rgb {
            r: rgb.r,
            g: rgb.g,
            b: rgb.b,
        }),
        // `NamedColor`'s discriminants are palette indices, and `Color::Indexed`
        // is a palette index already.
        Color::Named(named) => FrameColor::Palette(u16::try_from(named as usize).unwrap()),
        Color::Indexed(index) => FrameColor::Palette(u16::from(index)),
    }
}

/// `Flags` -> `CellFlags`, written independently: a table of pairs, checked
/// one at a time, rather than the `set()` chain the crate uses.
fn flags(flags: Flags) -> CellFlags {
    const TABLE: &[(Flags, CellFlags)] = &[
        (Flags::INVERSE, CellFlags::INVERSE),
        (Flags::BOLD, CellFlags::BOLD),
        (Flags::ITALIC, CellFlags::ITALIC),
        (Flags::UNDERLINE, CellFlags::UNDERLINE),
        (Flags::WRAPLINE, CellFlags::WRAPLINE),
        (Flags::WIDE_CHAR, CellFlags::WIDE_CHAR),
        (Flags::WIDE_CHAR_SPACER, CellFlags::WIDE_CHAR_SPACER),
        (Flags::DIM, CellFlags::DIM),
        (Flags::HIDDEN, CellFlags::HIDDEN),
        (Flags::STRIKEOUT, CellFlags::STRIKEOUT),
        (Flags::LEADING_WIDE_CHAR_SPACER, CellFlags::LEADING_WIDE_CHAR_SPACER),
        (Flags::DOUBLE_UNDERLINE, CellFlags::DOUBLE_UNDERLINE),
        (Flags::UNDERCURL, CellFlags::UNDERCURL),
        (Flags::DOTTED_UNDERLINE, CellFlags::DOTTED_UNDERLINE),
        (Flags::DASHED_UNDERLINE, CellFlags::DASHED_UNDERLINE),
    ];
    let mut out = CellFlags::empty();
    for (upstream, ours) in TABLE {
        if flags.contains(*upstream) {
            out |= *ours;
        }
    }
    out
}

/// The cell the frame contract says should be at `(line, col)`.
pub fn cell_at(term: &Term<VoidListener>, line: i32, col: u16) -> FrameCell {
    let source = &term.grid()[Line(line)][Column(col as usize)];
    let marks = source.zerowidth().unwrap_or(&[]);
    let underline = source.underline_color().map(color);
    FrameCell {
        c: source.c,
        fg: color(source.fg),
        bg: color(source.bg),
        flags: flags(source.flags),
        extra: if marks.is_empty() && underline.is_none() {
            None
        } else {
            Some(Box::new(terminal_frame::CellExtras {
                zerowidth: marks.to_vec(),
                underline_color: underline,
            }))
        },
    }
}

/// The cursor the frame contract says should be reported.
pub fn cursor(term: &Term<VoidListener>) -> FrameCursor {
    let point = term.grid().cursor.point;
    let on_spacer = term.grid()[point].flags.contains(Flags::WIDE_CHAR_SPACER) && point.column > Column(0);
    let style = term.cursor_style();
    FrameCursor {
        line: point.line.0,
        col: if on_spacer {
            point.column.0 as u16 - 1
        } else {
            point.column.0 as u16
        },
        shape: match style.shape {
            CursorShape::Underline => FrameCursorShape::Underline,
            CursorShape::Beam => FrameCursorShape::Beam,
            CursorShape::HollowBlock => FrameCursorShape::HollowBlock,
            CursorShape::Block | CursorShape::Hidden => FrameCursorShape::Block,
        },
        visible: term.mode().contains(TermMode::SHOW_CURSOR),
        blinking: style.blinking,
    }
}

pub fn modes(term: &Term<VoidListener>) -> TerminalModes {
    let mode = *term.mode();
    TerminalModes {
        alt_screen: mode.contains(TermMode::ALT_SCREEN),
        line_wrap: mode.contains(TermMode::LINE_WRAP),
        insert: mode.contains(TermMode::INSERT),
        origin: mode.contains(TermMode::ORIGIN),
        mouse_reporting: mode.contains(TermMode::MOUSE_REPORT_CLICK)
            || mode.contains(TermMode::MOUSE_MOTION)
            || mode.contains(TermMode::MOUSE_DRAG),
    }
}

pub fn palette(term: &Term<VoidListener>) -> Vec<Option<Rgb>> {
    let colors = term.colors();
    (0..PALETTE_LEN)
        .map(|index| {
            colors[index].map(|rgb| Rgb {
                r: rgb.r,
                g: rgb.g,
                b: rgb.b,
            })
        })
        .collect()
}

/// THE COMPARISON EVERY CORRECTNESS TEST USES.
///
/// An assembled screen against the terminal it is supposed to be a picture of.
/// Returns a human-readable first difference, or `None`.
pub fn compare(assembler: &FrameAssembler, term: &Term<VoidListener>) -> Option<String> {
    let cols = term.columns() as u16;
    let rows = term.screen_lines() as u16;
    if (assembler.cols(), assembler.rows()) != (cols, rows) {
        return Some(format!(
            "geometry {}x{} but the terminal is {cols}x{rows}",
            assembler.cols(),
            assembler.rows()
        ));
    }

    for line in 0..rows as i32 {
        for col in 0..cols {
            let want = cell_at(term, line, col);
            match assembler.cell(line, col) {
                None => return Some(format!("cell ({line},{col}) is missing from the assembler")),
                Some(got) if *got != want => {
                    return Some(format!(
                        "cell ({line},{col}): assembled {got:?} but the terminal holds {want:?}"
                    ))
                }
                Some(_) => {}
            }
        }
    }

    // Out-of-range accesses must report nothing rather than wrap or panic.
    for (line, col) in [(-1, 0), (rows as i32, 0), (0, cols), (rows as i32, cols)] {
        if assembler.cell(line, col).is_some() {
            return Some(format!("cell ({line},{col}) is outside the screen but returned a cell"));
        }
    }

    if assembler.cursor() != cursor(term) {
        return Some(format!("cursor {:?} != {:?}", assembler.cursor(), cursor(term)));
    }
    if assembler.modes() != modes(term) {
        return Some(format!("modes {:?} != {:?}", assembler.modes(), modes(term)));
    }
    let want = palette(term);
    if assembler.palette() != want.as_slice() {
        let first = assembler
            .palette()
            .iter()
            .zip(&want)
            .enumerate()
            .find(|(_, (a, b))| a != b)
            .map(|(index, (a, b))| format!("palette[{index}] {a:?} != {b:?}"));
        return Some(
            first.unwrap_or_else(|| format!("palette length {} != {}", assembler.palette().len(), want.len())),
        );
    }

    // Text, independently, so a cell-level bug that somehow round-trips still
    // has to survive being read as text.
    for line in 0..rows as i32 {
        let mut expected = String::new();
        for col in 0..cols {
            let source = &term.grid()[Line(line)][Column(col as usize)];
            expected.push(source.c);
            for mark in source.zerowidth().unwrap_or(&[]) {
                expected.push(*mark);
            }
        }
        if assembler.line_text(line) != expected {
            return Some(format!(
                "line {line} text {:?} != {expected:?}",
                assembler.line_text(line)
            ));
        }
    }

    None
}
