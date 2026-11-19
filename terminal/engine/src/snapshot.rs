//! The parser-boundary state shared by a headless terminal and its attached view.

use crate::replay::screen_replay::ScreenReplay;
use crate::Tail;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    /// Bytes parsed before this state. Subsequent output starts at this offset.
    pub offset: u64,
    pub replay: ScreenReplay,
    pub tail: Tail,
    pub history_at_cap: bool,
    pub history_trimmed: u32,
    pub output_while_detached: bool,
    pub input_written: u64,
}
