//! The link between the two windows of a linked pair: window 1, which owns nvim and the editor, and window 2, which
//! window 1 starts and talks to over a socketpair. What travels on it, the editor operations window 2 may ask for,
//! and the pieces both windows share. Nothing here touches GTK or a display.

pub mod base_tabs;
pub mod edge;
pub mod editor_op;
pub mod editor_ops;
#[cfg(target_os = "linux")]
pub mod fdpass;
pub mod ids;
#[cfg(target_os = "linux")]
pub mod link;
pub mod moves;
pub mod msg;
pub mod pair;
#[cfg(target_os = "linux")]
pub mod start;
pub mod summary;
pub mod workspace_move;

pub use editor_op::{EditorOp, WireEol, WireHunk, WireLine, WireMeta, WireScope};
pub use msg::{
    Drawn, EdgeKind, EditorRequest, FocusTarget, Frame, Header, LinkError, Msg, ReplyResult, Shape, Side, Summary,
    TurnEdge, INLINE_MAX, MAX_MOVE_FDS, PROTOCOL_VERSION,
};
