//! The layout is data (docs/superpowers/specs/2026-09-23-modules-and-canvas-design.md §3-§4): a
//! tree of splits whose leaves are modules, the pure geometry over it, and nothing that knows what
//! a widget is. `shell::module_grid` is the GTK container that allocates what this computes; a
//! macOS host would allocate the same rectangles into its own views.

pub mod geometry;
pub mod module;
pub mod tree;

pub use geometry::{
    arrange, hide, min_size, navigate, neighbor, resize, Arrangement, Direction, Divider, Frame, Nav, Rect, Size,
};
pub use module::{ModuleDecl, ModuleError, ModuleId, ModuleKind, Placement};
pub use tree::{Axis, Branch, Layout, LayoutError, Node, SplitPath, ZoomChange};
