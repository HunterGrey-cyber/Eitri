//! `terminal-frame` -- the stable Verdandi presentation contract, its
//! projection from an authoritative `alacritty_terminal::Term`, and the wire
//! encodings it is measured with.
//!
//! ```text
//!                       ONE authoritative Term
//!                                |
//!            Projector::next  (the ONLY damage()/reset_damage() pair)
//!                                |
//!                          TerminalFrame
//!                    /                      \
//!          FrameAssembler                encode_rle
//!       (renderer, semantic               (a process
//!        interpreter -- in-process)        boundary)
//! ```
//!
//! Four modules, four jobs:
//!
//! | module | job |
//! |---|---|
//! | [`frame`] | the contract. No upstream type crosses it. One coordinate system: **absolute grid lines**. |
//! | [`project`] | `Term` -> frame. Owns damage, and compensates the five ways damage under-reports. |
//! | [`assemble`] | frame -> screen. Makes `FrameKind`'s meaning executable, and is the oracle for the differential test. |
//! | [`encode`] | frame -> bytes, reversibly. Naive and RLE, for the bandwidth measurement. |
//! | [`viewport`] | the read-only second view, with clamped scrollback indexing. |
//!
//! # Known gaps, by decision
//!
//! * **SGR blink (5 / 6 / 25) is unrecoverable.** `alacritty_terminal` discards
//!   it: `Attr::BlinkSlow`/`BlinkFast`/`CancelBlink` reach `terminal_attribute`
//!   and fall into the `_ => ()` arm, so no `Flags` bit and no `TermMode` bit
//!   ever records it. There is nothing to project. Forking upstream for it
//!   would be a far larger liability than a blinking cell is worth.
//!   `tests/contract.rs::sgr_blink_is_unrecoverable_from_term` pins the gap so
//!   it is a known absence rather than a surprise.
//! * **OSC 8 hyperlinks are not carried.** They are a pure renderer affordance
//!   with no semantic content, and carrying them would put an unbounded URI in
//!   the cell type. The escape hatch, if one is ever wanted, is a per-frame
//!   side table with a `u16` index in [`frame::CellExtras`] -- not a `String`
//!   per cell.
//! * **Selection is not carried.** It is client state that the terminal
//!   deliberately excludes from damage; the client owns it, the same way it
//!   owns focus.
//!
//! # Who drives resize
//!
//! Resize is expressed in CELLS only. The chain is
//! `font metrics -> cell box -> (cols, rows)`, and all of it is
//! **renderer-owned**. From one `(cols, rows)` decision the engine must drive
//! BOTH:
//!
//! 1. `Term::resize(TermSize { columns, screen_lines })`, and
//! 2. the PTY `TIOCSWINSZ`,
//!
//! in that order. Letting those diverge is a silent-corruption seam: the child
//! lays out for one width while the grid is another, and nothing errors --
//! output simply wraps in the wrong places for as long as the mismatch lasts.
//! The frame carries `cols`/`rows` so a consumer can *detect* the mismatch
//! ([`assemble::ApplyError::DeltaResized`]), never so it can drive it.
//!
//! Verified against `alacritty_terminal` 0.26.0 and `vte` 0.15.0. Every line
//! citation in this crate was read, not remembered.

pub mod assemble;
pub mod encode;
pub mod frame;
pub mod project;
pub mod viewport;

pub use assemble::{ApplyError, FrameAssembler};
pub use encode::{decode, encode, encode_naive, encode_rle, Encoding};
pub use frame::{
    CellExtras, CellFlags, ColorOverride, FrameCell, FrameColor, FrameCursor, FrameCursorShape, FrameError, FrameKind,
    Rgb, RowUpdate, TerminalFrame, TerminalModes, PALETTE_LEN,
};
pub use project::{Compensation, FrameStats, Projector, SpanPolicy, MAX_ZERO_WIDTH_MARKS_PER_CELL};
pub use viewport::{max_scrollback, project_scrollback, project_window};

/// The exact `vte` `alacritty_terminal` was compiled against, so a
/// two-vte-versions-in-one-binary mistake is structurally impossible. Same rule
/// `terminal-sync` follows.
pub use alacritty_terminal::vte;
