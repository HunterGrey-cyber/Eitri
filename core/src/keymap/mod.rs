//! The keymap (docs/superpowers/specs/2026-09-25-keymap-tabs-panel-design.md §2): tmux's key names,
//! the closed set of actions a key can run, the default prefix table (stock tmux, prefix `C-b`), the
//! root accelerators, and what `init.lua`'s `eitri.keymap` calls turn it into. GTK-free: `shell`
//! registers what this says and looks keys up in it; a macOS host would do the same.

pub mod action;
pub mod key;
pub mod panel;
pub mod root;
pub mod stock;
pub mod table;

pub use action::{Action, ActionError, OptValue, Parsed, SwapTarget, TabAction, TextChange};
pub use key::{Chord, KeyName, KeyParseError, KeySpec};
pub use panel::{
    default_bindings, effective, parse_action, parse_seq, reserved, LeaderSource, PanelAction, PanelBinding, PanelKey,
    PanelKeymap, PanelSeq, PanelSource, PanelUserTable, DEFAULT_GROUPS,
};
pub use root::HelpRow;
pub use table::{check_command_keybinding, Binding, Keymap, KeymapError, KeymapOp, Source};
