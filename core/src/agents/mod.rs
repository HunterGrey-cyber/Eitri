//! Read-only summaries of agent tabs, shared by windows and command-line readers.

#[cfg(target_os = "linux")]
pub mod cli;
pub mod control;
#[cfg(target_os = "linux")]
pub mod liveness;
pub mod publish;
#[cfg(target_os = "linux")]
pub mod read;
pub mod row;
pub mod shared;
pub mod state;
pub mod wire;

pub use publish::{agents_dir, new_instance, Publisher};
#[cfg(target_os = "linux")]
pub use read::{scan_once, scan_windows, Reader};
pub use row::{Background, Change, Departed, GoneWhy, LabelFrom, Needs, ProcessFile, ProjectRef, Row, View, WindowRef};
pub use state::{derive, Class, EndedHow, State, TabInputs};

pub const SCHEMA_V: u32 = 1;
pub const MAX_ROWS_PER_FILE: usize = 64;
pub const MAX_FILE_BYTES: usize = 65_536;
pub const COALESCE_MS: u64 = 100;
pub const DEPARTED_KEEP_MS: u64 = 5_000;
pub const MOVE_HOLD_MS: u64 = 1_000;
pub const LOCK_POLL_MS: u64 = 2_000;
pub const LABEL_MAX: usize = 40;
pub const GESTURE_MS: u64 = 1_000;
pub const NEW_MIN_INTERVAL_MS: u64 = 1_000;
pub const ROUND_MAX_IDLE_MS: u64 = 600_000;

/// What a reader of the agents directory hands its caller, on the reader's own thread. Nearly every event is a
/// change, so the change is not boxed to spare the rare error its size. Defined here, not beside the Linux reader,
/// because a page's feed takes these events on every platform.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReaderEvent {
    Change(Change),
    /// The reader cannot go on (the directory went, the watch failed); nothing follows it.
    Error(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Intent {
    Show,
    Stop,
}
