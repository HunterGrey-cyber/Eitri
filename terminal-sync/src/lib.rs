//! DECSET 2026 synchronized-output **publication barrier** for the verdandi terminal runtime
//! engine.
//!
//! Locked semantic model -- a publication barrier, NOT a replacement parser:
//!
//! ```text
//! BSU (ESC[?2026h)  -> Term keeps parsing and mutating normally
//!                      (no raw PTY bytes are buffered outside Term, parsing is not delayed)
//!                   -> wakeups may occur; publication of intermediate snapshots is suppressed
//! ESU (ESC[?2026l)  -> publish ONE consolidated snapshot
//! ```
//!
//! Three pieces:
//! * [`NeverBuffer`] -- a `vte::ansi::Timeout` that disables vte's 2 MiB raw-byte sync buffer.
//! * [`SyncSpy`] -- a fully transparent `Handler` that intercepts exactly the two 2026 mode
//!   dispatches (and still forwards them).
//! * [`SyncBarrier`] / [`SyncDriver`] -- publication gating driven by Handler DISPATCH, never by
//!   byte arrival.
//!
//! Verified against alacritty_terminal 0.26.0 and vte 0.15.0; see module docs for exact file
//! and line citations.

mod barrier;
mod driver;
mod never_buffer;
mod spy;

pub use barrier::{PublishReason, SyncBarrier};
pub use driver::SyncDriver;
pub use never_buffer::NeverBuffer;
pub use spy::SyncSpy;

/// The exact vte re-exported by alacritty_terminal, so a two-vte-versions mistake is impossible.
pub use alacritty_terminal::vte;
