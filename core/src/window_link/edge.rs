//! Two decisions a window makes from monitor geometry alone: whether the other window of the pair lies past this
//! grid's edge in a direction (then the keys cross to it), and which monitor to move to at start.

use serde::{Deserialize, Serialize};

use super::msg::Side;
use crate::layout::Direction;

/// A monitor's rectangle in GDK's logical (layout) coordinates, so outputs at different scales compare.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MonitorRect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeRoute {
    /// The other window's monitor lies that way: the keys cross to it.
    OtherWindow,
    /// Same monitor, unknown geometry, or nothing that way: the window manager decides.
    WindowManager,
}

fn overlap(a0: i32, a1: i32, b0: i32, b1: i32) -> bool {
    a0 < b1 && b0 < a1
}

pub fn edge_route(mine: Option<MonitorRect>, other: Option<MonitorRect>, side: Side) -> EdgeRoute {
    let (Some(a), Some(b)) = (mine, other) else {
        return EdgeRoute::WindowManager;
    };
    if a == b {
        return EdgeRoute::WindowManager;
    }
    let lies = match side {
        Side::Right => b.x >= a.x + a.w && overlap(a.y, a.y + a.h, b.y, b.y + b.h),
        Side::Left => b.x + b.w <= a.x && overlap(a.y, a.y + a.h, b.y, b.y + b.h),
        Side::Down => b.y >= a.y + a.h && overlap(a.x, a.x + a.w, b.x, b.x + b.w),
        Side::Up => b.y + b.h <= a.y && overlap(a.x, a.x + a.w, b.x, b.x + b.w),
    };
    if lies {
        EdgeRoute::OtherWindow
    } else {
        EdgeRoute::WindowManager
    }
}

/// Keys that left one grid to the right arrive at the other grid's left edge.
pub fn arrival_side(side: Side) -> Side {
    match side {
        Side::Left => Side::Right,
        Side::Right => Side::Left,
        Side::Up => Side::Down,
        Side::Down => Side::Up,
    }
}

impl From<Direction> for Side {
    fn from(d: Direction) -> Self {
        match d {
            Direction::Left => Side::Left,
            Direction::Right => Side::Right,
            Direction::Up => Side::Up,
            Direction::Down => Side::Down,
        }
    }
}

impl From<Side> for Direction {
    fn from(s: Side) -> Self {
        match s {
            Side::Left => Direction::Left,
            Side::Right => Direction::Right,
            Side::Up => Direction::Up,
            Side::Down => Direction::Down,
        }
    }
}

/// Where a window should be put: its remembered monitor, once, when that monitor exists and the window is not
/// already on it. A remembered monitor that is absent leaves placement to the window manager until it appears.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonitorMemory {
    wanted: Option<String>,
    settled: bool,
}

impl MonitorMemory {
    pub fn new(wanted: Option<String>) -> Self {
        MonitorMemory { wanted, settled: false }
    }

    /// Called at start and whenever GDK's monitor list changes; returns the connector to move to, at most once.
    pub fn decide(&mut self, current: Option<&str>, available: &[String]) -> Option<String> {
        if self.settled {
            return None;
        }
        let wanted = self.wanted.as_deref()?;
        if current == Some(wanted) {
            self.settled = true;
            return None;
        }
        if available.iter().any(|c| c == wanted) {
            self.settled = true;
            return Some(wanted.to_string());
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LEFT: MonitorRect = MonitorRect {
        x: 0,
        y: 0,
        w: 1920,
        h: 1080,
    };
    // A scale-2 output of 1920x1080 pixels is 960x540 logical, placed right of the first.
    const RIGHT: MonitorRect = MonitorRect {
        x: 1920,
        y: 0,
        w: 960,
        h: 540,
    };
    const BELOW: MonitorRect = MonitorRect {
        x: 0,
        y: 1080,
        w: 1920,
        h: 1080,
    };
    const FAR: MonitorRect = MonitorRect {
        x: 5000,
        y: 5000,
        w: 100,
        h: 100,
    };

    #[test]
    fn the_other_window_lies_that_way_only_past_the_edge_with_overlap() {
        assert_eq!(edge_route(Some(LEFT), Some(RIGHT), Side::Right), EdgeRoute::OtherWindow);
        assert_eq!(edge_route(Some(RIGHT), Some(LEFT), Side::Left), EdgeRoute::OtherWindow);
        assert_eq!(
            edge_route(Some(LEFT), Some(RIGHT), Side::Left),
            EdgeRoute::WindowManager
        );
        assert_eq!(edge_route(Some(LEFT), Some(BELOW), Side::Down), EdgeRoute::OtherWindow);
        assert_eq!(edge_route(Some(BELOW), Some(LEFT), Side::Up), EdgeRoute::OtherWindow);
        assert_eq!(
            edge_route(Some(LEFT), Some(BELOW), Side::Right),
            EdgeRoute::WindowManager
        );
        assert_eq!(
            edge_route(Some(LEFT), Some(FAR), Side::Right),
            EdgeRoute::WindowManager,
            "no overlap"
        );
    }

    #[test]
    fn one_monitor_or_unknown_geometry_leaves_the_edge_to_the_window_manager() {
        assert_eq!(
            edge_route(Some(LEFT), Some(LEFT), Side::Right),
            EdgeRoute::WindowManager
        );
        assert_eq!(edge_route(None, Some(RIGHT), Side::Right), EdgeRoute::WindowManager);
        assert_eq!(edge_route(Some(LEFT), None, Side::Right), EdgeRoute::WindowManager);
    }

    #[test]
    fn keys_arrive_at_the_opposite_edge_and_directions_convert_both_ways() {
        for side in [Side::Left, Side::Right, Side::Up, Side::Down] {
            assert_eq!(arrival_side(arrival_side(side)), side);
            assert_eq!(Side::from(Direction::from(side)), side);
        }
        assert_eq!(arrival_side(Side::Right), Side::Left);
    }

    #[test]
    fn a_remembered_monitor_is_used_once_and_waited_for_while_absent() {
        let all = vec!["DP-1".to_string(), "DP-2".to_string()];
        let mut m = MonitorMemory::new(Some("DP-2".into()));
        assert_eq!(m.decide(Some("DP-1"), &all), Some("DP-2".into()));
        assert_eq!(m.decide(Some("DP-1"), &all), None, "once");
        let mut waiting = MonitorMemory::new(Some("HDMI-A-1".into()));
        assert_eq!(waiting.decide(Some("DP-1"), &all), None);
        let more = vec!["DP-1".to_string(), "HDMI-A-1".to_string()];
        assert_eq!(waiting.decide(Some("DP-1"), &more), Some("HDMI-A-1".into()));
        let mut there = MonitorMemory::new(Some("DP-1".into()));
        assert_eq!(there.decide(Some("DP-1"), &all), None);
        assert_eq!(
            there.decide(Some("DP-2"), &all),
            None,
            "already settled where it opened"
        );
        assert_eq!(MonitorMemory::new(None).decide(Some("DP-1"), &all), None);
    }
}
