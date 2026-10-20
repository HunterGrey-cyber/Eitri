//! The layout is a tree of modules, geometry and rearranging verbs, independent of widgets.

pub mod arrange;
pub mod con;
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
#[cfg(test)]
pub(crate) mod test_support;
pub mod tray;
pub mod tree;
#[cfg(test)]
pub(crate) mod v1_reference;
pub mod verbs;
pub mod workspace;

pub use arrange::Divider;
pub use con::{Child, Con, ConId, ConLayout, Node, Pin, MAX_CHILDREN};
pub use cycle::{next_on_screen, next_terminal, FocusHistory};
pub use geometry::{
    arrange, hide, min_size, move_divider, navigate, neighbor, resize, settle_pins, Arrangement, ConStripEntry,
    Direction, Frame, Nav, Rect, Size, Strip, DEFAULT_STRIP_PX,
};
pub use keys::{
    choose_instance, key_action, module_target, split_choice, strip, strip_direct, KeyAction, KeyError, ModuleKeys,
    SplitChoice, StripEntry,
};
pub use kill::{can_kill, kill, KillScope, Reopen};
pub use module::{ModuleDecl, ModuleError, ModuleId, ModuleKind, Placement};
pub use number::number_plan;
pub use ops::{even, place, swap, swap_adjacent};
pub use reconcile::{reconcile, reconcile_default, ReconcileError, Reconciled};
pub use tray::{agent_place, chip_label, tray};
pub use tree::{Axis, Branch, Layout, LayoutError, Selection, Zoom, ZoomChange};

pub use workspace::{WorkspaceError, Workspaces};

pub use verbs::{
    container_mode, cycle_preset, flip, focus_child, focus_parent, move_dir, move_far, preset, rotate, spread,
    swap_marked, switch_tab, toggle_split, Preset, MAX_PRESET_MODULES,
};
