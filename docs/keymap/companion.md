# Companion mode keys

The keys of the agent panel when it runs as a window of its own beside your nvim (`eitri panel`, or
`:EitriPanel` from nvim). Setting it up is in INSTALL.md, "Use it beside your own nvim". The panel's
own keys (BROWSE and INPUT, `j/k`, `f` hints, `:`, `?`) are the same as in the one-window mode; this
page lists only what differs or crosses between the two windows.

## Between nvim and the panel

| where | key | what happens |
|---|---|---|
| nvim | `Ctrl+h/j/k/l` at the edge of nvim's own windows | nvim hands the move to the panel, which asks the window manager to focus the neighbouring window. Eitri's fallback binds a key only where your configuration has left it empty or at nvim's default |
| nvim | smart-splits `at_edge` hook | `require("eitri").edge(ctx.direction)` hands the move over; it returns `false` when no panel is attached |
| nvim | vim-tmux-navigator maps, nvim outside tmux | treated as plain window moves while a panel is attached; nothing to configure |
| nvim | `:EitriPanel [dir]` | opens the panel for `dir` (default: the working directory), or attaches and raises the one already open for the project |
| panel | `Ctrl+h`, `Ctrl+l` | always leave the window (the window manager moves focus) |
| panel | `Ctrl+k` | leaves the window from BROWSE; in INPUT it switches to BROWSE |
| panel | `Ctrl+j` | leaves the window from INPUT; in BROWSE it switches to INPUT |
| panel | `Ctrl+g` in INPUT | edits the draft in a scratch buffer in your nvim (it answers that the editor is not connected when no editor is attached) |
| panel | open a file at a line | opens it in your nvim and raises the editor's window (not from inside tmux) |

Focus moves need a window-manager adapter: sway, Hyprland and niri have one, GNOME and KDE do not (use
the desktop's own window keys). `eitri.config.set("companion.wm", "auto" | "hyprland" | "sway" |
"niri" | "none")` in `~/.config/eitri/init.lua` picks it; `none` never asks the window manager for
anything.

## The prefix in a companion window

The prefix is the same as in the one-window mode (stock tmux's `Ctrl+b`, or the prefix you set or
that was read from your tmux configuration). The window has no layout, so only the panel's part of
the table works:

| key after the prefix | what happens |
|---|---|
| `c`, `n`, `p`, `l`, `0`-`9`, `,`, `&`, `w`, `i` | the tab keys: new, next, previous, last, select, rename, close, choose, info |
| `r` | reload the panel's page |
| `?` | the key overlay, which lists only these keys |
| `:` | the panel's command line |
| text size keys | the panel's text size |
| `f` | hint labels over the panel |
| the prefix again | sends the literal prefix key to the panel |
| the `Select` keys (stock tmux's arrow keys, or what you bound) | a window-manager focus move, like `Ctrl+h/j/k/l` |

Every layout, module or window action (splits, zoom, resize, swap, copy mode, window modes) answers
`not in a companion window`: the window manager arranges windows. Layout actions that you rebind in
`init.lua` are refused the same way.

## What the band says

| state | text |
|---|---|
| no editor | `no editor attached: run :EitriPanel in nvim` |
| connecting | `attaching…`, or `attaching… (nvim is waiting for a key)` |
| attached | nothing |
| nvim quit | `editor detached: run :EitriPanel to attach again` |
| another panel took the nvim | `editor detached: another Eitri panel attached to it` |
| could not connect | `could not attach: <reason>` |
| draft editing stopped | `draft editing stopped: the editor went away` |
| no window-manager adapter | `your desktop does not let Eitri move focus; use its own window keys` (once) |
