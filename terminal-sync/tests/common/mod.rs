//! Shared scaffolding: a real `alacritty_terminal::Term` and a text rendering of its grid.
#![allow(dead_code)]

use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::{Config, Term};

pub const COLUMNS: usize = 40;
pub const SCREEN_LINES: usize = 8;

pub struct TestSize;

impl Dimensions for TestSize {
    fn total_lines(&self) -> usize {
        SCREEN_LINES
    }

    fn screen_lines(&self) -> usize {
        SCREEN_LINES
    }

    fn columns(&self) -> usize {
        COLUMNS
    }
}

pub fn new_term() -> Term<VoidListener> {
    Term::new(Config::default(), &TestSize, VoidListener)
}

/// A presentation snapshot: the visible grid as text, trailing blanks trimmed.
pub fn snapshot(term: &Term<VoidListener>) -> String {
    let grid = term.grid();
    let mut out = Vec::new();
    for line in 0..SCREEN_LINES {
        let mut row = String::new();
        for col in 0..COLUMNS {
            row.push(grid[Line(line as i32)][Column(col)].c);
        }
        out.push(row.trim_end().to_string());
    }
    while out.last().map(|l| l.is_empty()).unwrap_or(false) {
        out.pop();
    }
    out.join("\n")
}

/// Deterministic splitter: xorshift64*, so "nondeterministic chunking" is reproducible.
pub struct Splitter(u64);

impl Splitter {
    pub fn new(seed: u64) -> Self {
        Self(seed | 1)
    }

    pub fn next_len(&mut self, max: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        1 + (self.0 % max as u64) as usize
    }

    pub fn split<'a>(&mut self, bytes: &'a [u8], max: usize) -> Vec<&'a [u8]> {
        let mut out = Vec::new();
        let mut rest = bytes;
        while !rest.is_empty() {
            let n = self.next_len(max).min(rest.len());
            let (head, tail) = rest.split_at(n);
            out.push(head);
            rest = tail;
        }
        out
    }
}

pub const BSU: &[u8] = b"\x1b[?2026h";
pub const ESU: &[u8] = b"\x1b[?2026l";
