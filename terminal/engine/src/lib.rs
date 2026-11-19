//! `eitri-terminal`: the engine of the bottom terminal.
//!
//! A local terminal is one `TerminalSession`: a thread that owns the PTY, the child, the
//! `alacritty_terminal::Term` and everything that parses or renders -- and nothing that draws on a
//! screen. The host (`shell/src/terminal/`, GTK) sends it `SessionCommand`s, is woken when there is
//! a new frame, and paints that frame with `paint` (the default `paint` feature).
//! A `HeadlessTerminal` owns its PTY and parser on a worker too, answering queries without a view
//! and publishing ordered output, resize markers and parser-boundary snapshots to one attachment.
//!
//! **GTK-free by construction**, the same way `eitri-core` is: `src/manifest_guard.rs` fails on a
//! manifest that names `gtk4`/`glib`/`gdk`/`webkit6`. Unix-only, like `eitri-core`: PTYs are.
//!
//! Where the pieces came from, so the history is findable:
//! - `paint.rs` and `metrics.rs` are the frozen `terminal-pane/src/{backend,metrics}.rs`, and both
//!   have changed since.
//! - `screen.rs` is the frozen `terminal-pane/src/live.rs` with feed and render split (the spike's
//!   `SplitDriver`) and a real `EventListener` in place of `VoidListener`.
//! - `pty.rs` replaces the Node sidecar and gRPC session (`terminal-session`, discarded).

pub mod clock;
pub mod headless;
mod indicator;
mod listener;
#[cfg(feature = "paint")]
pub mod metrics;
pub mod mirror;
pub mod mouse;
mod outbox;
#[cfg(feature = "paint")]
pub mod paint;
pub mod preedit;
pub mod pty;
pub mod replay;
pub mod screen;
pub mod scroll;
pub mod select;
pub mod session;
pub mod snapshot;

pub use indicator::indicator_ops;
pub use listener::HostEvents;
#[cfg(feature = "paint")]
pub use metrics::TerminalMetrics;
pub use mouse::MouseModes;
pub use outbox::REPLY_CAP;
#[cfg(feature = "paint")]
pub use paint::{paint, paint_ops};
pub use preedit::{layout_preedit, PreeditLayout};
pub use pty::{child_environment, child_environment_with, PtyChild, PtySize, SpawnExtras, SpawnSpec};
pub use screen::{CursorCell, Screen, SelectionRecord, TerminalColors, ViewState};
pub use scroll::{ScrollRequest, ScrollView};
pub use select::{SelectCommand, SelectKind};
pub use session::{
    DetachRefused, Detached, ExitInfo, SessionCommand, SessionConfig, Tail, TerminalSession, Update, BOUNDARY_WAIT,
    RAW_TAIL_MAX,
};

/// The guard that this crate's manifest never grows a GTK/WebKit dependency.
#[cfg(test)]
mod manifest_guard;

pub use headless::{
    Attachment, ClientId, DetachWhy, HeadlessConfig, HeadlessStatus, HeadlessTerminal, Job, StreamItem, WaitWhy,
};
pub use snapshot::Snapshot;
