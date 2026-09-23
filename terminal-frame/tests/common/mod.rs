//! Shared scaffolding: a real `alacritty_terminal::Term` driven by a real
//! `vte::ansi::Processor`, plus the oracle every correctness test compares
//! against.
#![allow(dead_code)]

use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::vte::ansi::Processor;

use terminal_frame::project::Compensation;
use terminal_frame::{FrameAssembler, Projector};

pub mod reference;

/// The plan's canonical geometry.
pub const CANONICAL_COLS: usize = 120;
pub const CANONICAL_ROWS: usize = 40;

#[derive(Clone, Copy)]
pub struct Size {
    pub cols: usize,
    pub rows: usize,
    pub history: usize,
}

impl Dimensions for Size {
    fn total_lines(&self) -> usize {
        self.rows + self.history
    }

    fn screen_lines(&self) -> usize {
        self.rows
    }

    fn columns(&self) -> usize {
        self.cols
    }
}

/// A `Term` plus the parser that feeds it. The engine's shape, minus the PTY.
pub struct Harness {
    pub term: Term<VoidListener>,
    pub parser: Processor,
    pub size: Size,
}

impl Harness {
    pub fn new(cols: usize, rows: usize) -> Self {
        Self::with_history(cols, rows, 1000)
    }

    pub fn with_history(cols: usize, rows: usize, history: usize) -> Self {
        let size = Size { cols, rows, history };
        let config = Config {
            scrolling_history: history,
            ..Config::default()
        };
        Self {
            term: Term::new(config, &size, VoidListener),
            parser: Processor::new(),
            size,
        }
    }

    pub fn canonical() -> Self {
        Self::new(CANONICAL_COLS, CANONICAL_ROWS)
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.term, bytes);
    }

    pub fn feed_str(&mut self, text: &str) {
        self.feed(text.as_bytes());
    }

    /// The grid as text, one line per row, trailing blanks trimmed. Combining
    /// marks are appended after the cell they ride on, exactly the way
    /// `FrameAssembler::line_text` does, so the two are comparable.
    pub fn screen_text(&self) -> String {
        let grid = self.term.grid();
        (0..self.size.rows)
            .map(|row| {
                let mut out = String::new();
                for col in 0..self.size.cols {
                    let cell = &grid[Line(row as i32)][Column(col)];
                    out.push(cell.c);
                    for mark in cell.zerowidth().unwrap_or(&[]) {
                        out.push(*mark);
                    }
                }
                out.trim_end().to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Where a delta-driven assembler and the terminal disagree.
#[derive(Debug)]
pub struct Divergence {
    pub step: usize,
    pub detail: String,
}

/// Feed `chunks` to a terminal, publishing a delta after each chunk, and report
/// the FIRST place the incrementally-assembled screen stops matching the
/// terminal itself.
///
/// The truth is the `Term`, read through [`reference`], which shares no code
/// with the crate under test. Comparing against a second `FrameAssembler`
/// instead -- the obvious design, and the one this suite started with -- lets a
/// mutation of any accessor mutate both sides identically and pass.
pub fn run_differential(harness: &mut Harness, chunks: &[Vec<u8>], compensation: Compensation) -> Option<Divergence> {
    let mut projector = Projector::with_compensation(compensation);
    let mut assembler = FrameAssembler::new();

    // Take the baseline with `next()`, not `full()`: a fresh Term starts with
    // `TermDamageState::full = true` and only `reset_damage()` clears it, so a
    // `full()` baseline would leave the flag standing and turn the first
    // "delta" into a silent Full.
    let first = projector.next(&mut harness.term);
    first.check().expect("baseline frame is well formed");
    assembler.apply(&first).expect("baseline applies");

    for (step, chunk) in chunks.iter().enumerate() {
        harness.feed(chunk);
        let frame = projector.next(&mut harness.term);
        if let Err(error) = frame.check() {
            return Some(Divergence {
                step,
                detail: format!("malformed frame: {error:?}"),
            });
        }
        if let Err(error) = assembler.apply(&frame) {
            return Some(Divergence {
                step,
                detail: format!("apply failed: {error:?}"),
            });
        }
        if assembler.generation() != frame.generation {
            return Some(Divergence {
                step,
                detail: format!(
                    "assembler generation {} != frame generation {}",
                    assembler.generation(),
                    frame.generation
                ),
            });
        }
        if assembler.applied() != step as u64 + 2 {
            return Some(Divergence {
                step,
                detail: format!(
                    "assembler applied {} frames, expected {}",
                    assembler.applied(),
                    step + 2
                ),
            });
        }
        if let Some(detail) = reference::compare(&assembler, &harness.term) {
            return Some(Divergence { step, detail });
        }
    }
    None
}

/// Deterministic xorshift64*, so "random" corpora are reproducible.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed | 1)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    pub fn below(&mut self, bound: usize) -> usize {
        (self.next_u64() % bound as u64) as usize
    }

    pub fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }
}
