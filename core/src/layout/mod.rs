//! The layout is data (docs/superpowers/specs/2026-09-23-modules-and-canvas-design.md §3-§4): a
//! tree of splits whose leaves are modules, the pure geometry over it, and nothing that knows what
//! a widget is. `shell::module_grid` is the GTK container that allocates what this computes; a
//! macOS host would allocate the same rectangles into its own views.

pub mod geometry;
pub mod keys;
pub mod module;
pub mod ops;
pub mod persist;
pub mod reconcile;
pub mod tray;
pub mod tree;

pub use geometry::{
    arrange, hide, min_size, move_divider, navigate, neighbor, resize, settle_pins, Arrangement, Direction, Divider,
    Frame, Nav, Rect, Size,
};
pub use keys::{key_action, strip, strip_direct, KeyAction, KeyError, ModuleKeys, StripEntry};
pub use module::{ModuleDecl, ModuleError, ModuleId, ModuleKind, Placement};
pub use ops::{even, place, swap, swap_adjacent};
pub use reconcile::{reconcile, reconcile_default, ReconcileError, Reconciled};
pub use tray::{agent_place, chip_label, tray};
pub use tree::{Axis, Branch, Layout, LayoutError, Node, Pin, SplitPath, ZoomChange};
