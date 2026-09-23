//! The bottom terminal engine.
//!
//! Each `TerminalSession` owns a thread with the PTY, child process, and
//! `alacritty_terminal::Term`. It parses output and builds frames without drawing on a screen.
//! The host sends `SessionCommand`s, wakes when a frame is ready, and draws it with [`paint`].
//!
//! `src/manifest_guard.rs` rejects manifests that name `gtk4`, `glib`, `gdk`, or `webkit6`,
//! keeping this crate independent of GTK. PTY support is Unix-only.
//! `paint.rs` draws frames, `metrics.rs` supplies cell geometry, `screen.rs` separates feed
//! from render and handles terminal events, and `pty.rs` manages the PTY and child.

pub mod clock;
mod listener;
pub mod metrics;
mod outbox;
pub mod paint;
pub mod pty;
pub mod screen;
pub mod session;

pub use listener::HostEvents;
pub use metrics::TerminalMetrics;
pub use outbox::REPLY_CAP;
pub use paint::paint;
pub use pty::{child_environment, PtyChild, PtySize, SpawnSpec};
pub use screen::{Screen, TerminalColors};
pub use session::{ExitInfo, SessionCommand, SessionConfig, TerminalSession, Update};

/// The guard that this crate's manifest never grows a GTK/WebKit dependency.
#[cfg(test)]
mod manifest_guard;
