//! The layout is a tree of modules, geometry and rearranging verbs, independent of widgets.

pub mod cycle;
#[cfg(test)]
pub(crate) mod fixture;
pub mod geometry;
pub mod keys;
pub mod kill;
pub mod module;
pub mod number;
pub mod ops;
pub mod persist;
pub mod reconcile;
pub mod tray;
pub mod tree;
pub mod verbs;

pub use cycle::{next_on_screen, FocusHistory};
pub use geometry::{
    arrange, hide, min_size, move_divider, navigate, neighbor, resize, settle_pins, Arrangement, Direction, Divider,
    Frame, Nav, Rect, Size,
};
pub use keys::{key_action, strip, strip_direct, KeyAction, KeyError, ModuleKeys, StripEntry};
pub use kill::{can_kill, kill, KillScope, Reopen};
pub use module::{ModuleDecl, ModuleError, ModuleId, ModuleKind, Placement};
pub use number::number_plan;
pub use ops::{even, place, swap, swap_adjacent};
pub use reconcile::{reconcile, reconcile_default, ReconcileError, Reconciled};
pub use tray::{agent_place, chip_label, tray};
pub use tree::{Axis, Branch, Layout, LayoutError, Node, Pin, SplitPath, ZoomChange};

pub use verbs::{
    cycle_preset, flip, move_far, preset, rotate, spread, swap_marked, toggle_split, Preset, MAX_PRESET_MODULES,
};
