//! `terminal-render` -- the native Verdandi terminal renderer.
//!
//! ```text
//!   authoritative Term
//!          |
//!    TerminalFrame            (terminal-frame: projection + damage)
//!          |
//!   RawViewport::top_line()   (terminal-frame: which rows, by ORDINAL)
//!          |
//!      build_paint_list       <-- this crate, PURE
//!          |
//!       PaintList
//!          |
//!      a backend              <-- NOT this crate. Another repo, another team.
//! ```
//!
//! # This crate contains no backend, deliberately
//!
//! There is no Skia here, no graphics dependency, and there will not be one.
//! `terminal-frame` is the only dependency. A backend consumes [`paint::PaintList`]
//! and belongs to whichever frontend is drawing -- the contract is designed to be
//! reusable by more than one, and a backend living here would quietly make this
//! crate the frontend's.
//!
//! `tests/backend.rs` is NOT a backend. It is a text-surface stand-in that
//! imports this crate and nothing else, so that a terminal type re-entering the
//! public API breaks compilation.
//!
//! # Why the paint list exists
//!
//! "Wrong pixels" must have a small set of possible causes. Splitting here means
//! the cell contract, the colour rules, wide-character occupancy, combining
//! marks, cursor placement and selection are all decided in code that needs no
//! GPU, no window and no graphics build to test -- so a correctness failure is
//! located before any painting happens, and the backend is left with nothing to
//! get wrong except geometry.
//!
//! # What this crate does NOT own
//!
//! VT parsing, scrollback semantics, terminal modes, and RawViewport identity.
//! It consumes presentation state only. It never touches `Term`, and in
//! particular never scrolls one to see history.
//!
//! # Full redraw first
//!
//! [`build::build_paint_list`] paints every visible cell, every time. Damage-based
//! partial redraw is deliberately absent until full-frame correctness is locked
//! and the cost is measured -- damage remains a renderer optimisation, never a
//! correctness input.

pub mod build;
pub mod color;
pub mod paint;

pub use build::{
    build_paint_list, palette_for, CursorColoring, RenderInput, SelectionSpan, ViewMode, ANCHOR_EXPIRED_NOTICE,
};
pub use color::Palette;
pub use paint::{CursorShape, CursorText, GlyphStyle, PaintLayer, PaintList, PaintOp, RgbColor, UnderlineKind};
