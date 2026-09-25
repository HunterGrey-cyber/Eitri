//! Stock tmux's prefix table, as probed -- the fixture the default table is checked against, and
//! the keys a Lua panel may never take (spec §2.3 rule 3: "every key stock tmux binds in its prefix
//! table ... stays reserved so a Lua panel never shadows a tmux reflex that a later phase might
//! adopt").
//!
//! Probed 2026-09-25 on tmux next-3.7, a throwaway server with no config, which exits by itself
//! since it has no sessions (remove its stale socket afterwards):
//!
//! ```sh
//! tmux -L specprobe -f /dev/null start-server \; list-keys -T prefix
//! ```
//!
//! It reports `prefix C-b`, `base-index 0`, `repeat-time 500`. Re-probe on a tmux upgrade; a changed
//! stock table is a spec question, not a silent edit here.

use super::key::KeySpec;

/// Stock tmux's `prefix`.
pub const STOCK_PREFIX: &str = "C-b";

/// (key, bound with `-r`, the command it runs) for every row of `list-keys -T prefix`, in the
/// probe's order.
pub const STOCK_TMUX_PREFIX: &[(&str, bool, &str)] = &[
    ("Space", false, "next-layout"),
    ("!", false, "break-pane"),
    ("\"", false, "split-window"),
    ("#", false, "list-buffers"),
    ("$", false, "command-prompt"),
    ("%", false, "split-window"),
    ("&", false, "confirm-before"),
    ("'", false, "command-prompt"),
    ("(", false, "switch-client"),
    (")", false, "switch-client"),
    (",", false, "command-prompt"),
    ("-", false, "delete-buffer"),
    (".", false, "command-prompt"),
    ("/", false, "command-prompt"),
    ("0", false, "select-window"),
    ("1", false, "select-window"),
    ("2", false, "select-window"),
    ("3", false, "select-window"),
    ("4", false, "select-window"),
    ("5", false, "select-window"),
    ("6", false, "select-window"),
    ("7", false, "select-window"),
    ("8", false, "select-window"),
    ("9", false, "select-window"),
    (":", false, "command-prompt"),
    (";", false, "last-pane"),
    ("<", false, "display-menu"),
    ("=", false, "choose-buffer"),
    (">", false, "display-menu"),
    ("?", false, "list-keys"),
    ("C", false, "customize-mode"),
    ("D", false, "choose-client"),
    ("E", false, "select-layout"),
    ("L", false, "switch-client"),
    ("M", false, "select-pane"),
    ("[", false, "copy-mode"),
    ("]", false, "paste-buffer"),
    ("c", false, "new-window"),
    ("d", false, "detach-client"),
    ("f", false, "command-prompt"),
    ("i", false, "display-message"),
    ("l", false, "last-window"),
    ("m", false, "select-pane"),
    ("n", false, "next-window"),
    ("o", false, "select-pane"),
    ("p", false, "previous-window"),
    ("q", false, "display-panes"),
    ("r", false, "refresh-client"),
    ("s", false, "choose-tree"),
    ("t", false, "clock-mode"),
    ("w", false, "choose-tree"),
    ("x", false, "confirm-before"),
    ("z", false, "resize-pane"),
    ("{", false, "swap-pane"),
    ("}", false, "swap-pane"),
    ("~", false, "show-messages"),
    ("DC", true, "refresh-client"),
    ("PPage", false, "copy-mode"),
    ("Up", true, "select-pane"),
    ("Down", true, "select-pane"),
    ("Left", true, "select-pane"),
    ("Right", true, "select-pane"),
    ("M-1", false, "select-layout"),
    ("M-2", false, "select-layout"),
    ("M-3", false, "select-layout"),
    ("M-4", false, "select-layout"),
    ("M-5", false, "select-layout"),
    ("M-6", false, "select-layout"),
    ("M-7", false, "select-layout"),
    ("M-n", false, "next-window"),
    ("M-o", false, "rotate-window"),
    ("M-p", false, "previous-window"),
    ("M-Up", true, "resize-pane"),
    ("M-Down", true, "resize-pane"),
    ("M-Left", true, "resize-pane"),
    ("M-Right", true, "resize-pane"),
    ("C-b", false, "send-prefix"),
    ("C-o", false, "rotate-window"),
    ("C-z", false, "suspend-client"),
    ("C-Up", true, "resize-pane"),
    ("C-Down", true, "resize-pane"),
    ("C-Left", true, "resize-pane"),
    ("C-Right", true, "resize-pane"),
    ("S-Up", true, "refresh-client"),
    ("S-Down", true, "refresh-client"),
    ("S-Left", true, "refresh-client"),
    ("S-Right", true, "refresh-client"),
];

/// The command stock tmux runs for `key` after its prefix, if it binds one.
pub fn stock_command(key: &KeySpec) -> Option<&'static str> {
    let name = key.to_string();
    STOCK_TMUX_PREFIX
        .iter()
        .find(|(k, _, _)| *k == name)
        .map(|(_, _, command)| *command)
}
