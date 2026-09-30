//! `eitri-terminal`: the engine of the bottom terminal (spec
//! `docs/superpowers/specs/2026-09-23-bottom-terminal-design.md`).
//!
//! One terminal is one `TerminalSession`: a thread that owns the PTY, the child, the
//! `alacritty_terminal::Term` and everything that parses or renders -- and nothing that draws on a
//! screen. The host (`shell/src/terminal/`, GTK) sends it `SessionCommand`s, is woken when there is
//! a new frame, and paints that frame with [`paint`].
//!
//! **GTK-free by construction**, the same way `eitri-core` is: `src/manifest_guard.rs` fails on a
//! manifest that names `gtk4`/`glib`/`gdk`/`webkit6`. Unix-only, like `eitri-core`: PTYs are.
//!
//! Where the pieces came from, so the history is findable:
//! - `paint.rs` and `metrics.rs` are the frozen `terminal-pane/src/{backend,metrics}.rs` from
//!   `freeze/terminal-stack` @ `1e715ab`, unchanged but for a header paragraph and rustfmt.
//! - `screen.rs` is the frozen `terminal-pane/src/live.rs` with feed and render split (the spike's
//!   `SplitDriver`) and a real `EventListener` in place of `VoidListener`.
//! - `pty.rs` replaces the Node sidecar and gRPC session (`terminal-session`, discarded).

pub mod clock;
mod listener;
pub mod metrics;
pub mod mouse;
mod outbox;
pub mod paint;
pub mod preedit;
pub mod pty;
pub mod screen;
pub mod scroll;
pub mod select;
pub mod session;

pub use listener::HostEvents;
pub use metrics::TerminalMetrics;
pub use mouse::MouseModes;
pub use outbox::REPLY_CAP;
pub use paint::{indicator_ops, paint, paint_ops};
pub use preedit::{layout_preedit, PreeditLayout};
pub use pty::{child_environment, PtyChild, PtySize, SpawnSpec};
pub use screen::{CursorCell, Screen, TerminalColors};
pub use scroll::{ScrollRequest, ScrollView};
pub use select::{SelectCommand, SelectKind};
pub use session::{ExitInfo, SessionCommand, SessionConfig, TerminalSession, Update};

/// The guard that this crate's manifest never grows a GTK/WebKit dependency.
#[cfg(test)]
mod manifest_guard;
