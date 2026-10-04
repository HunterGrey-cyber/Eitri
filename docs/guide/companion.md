English | [简体中文](companion.zh-CN.md)

# Beside your own Neovim

Eitri can also run as the agent panel alone: a separate window next to the Neovim you already use, in a
terminal (inside tmux or not), in upstream Neovide, or in any other Neovim GUI. This page covers how to start
it, how focus moves between the two windows, and what each desktop needs. The window you get from plain
`eitri` is described in [Getting started](getting-started.md); the panel's own keys are the same in both, and
[Keys](keys.md) lists them.

The panel attaches to your Neovim over its RPC socket and installs a little glue inside it: the file you have
open and your Visual selection go to the agent, the panel takes your colorscheme and shows your which-key keys,
buffers reload after the agent edits them, a file opens at a line from the panel, and `Ctrl+g` edits a draft in
Neovim. It removes all of it again when the panel goes away. Nothing is written to your Neovim configuration.
Your editor keeps its own speed and keys, and your window manager arranges the two windows.

## The eitri.nvim plugin

`eitri.nvim` is a thin launcher: everything the panel needs inside Neovim, the panel installs itself, so the
plugin and the installed Eitri never have to match versions. The packages (`.deb`, `.rpm`, AUR) put it in
`/usr/share/eitri/nvim/eitri.nvim`; the tarball installer puts it in `~/.local/share/eitri/eitri.nvim` (under
`$XDG_DATA_HOME/eitri/` when that is set). See [Where things go](../../INSTALL.md#where-things-go) for the rest
of the installed files. With lazy.nvim, point a spec at the directory:

```lua
-- .deb, .rpm, AUR
{ dir = "/usr/share/eitri/nvim/eitri.nvim", cmd = "EitriPanel" },

-- the tarball installer (install.sh)
{ dir = vim.fn.expand("~/.local/share/eitri/eitri.nvim"), cmd = "EitriPanel" },
```

Without a plugin manager, add the directory to `runtimepath`:
`set runtimepath+=/usr/share/eitri/nvim/eitri.nvim`.

Calling `require("eitri").setup({ ... })` is optional. `mapping = "<leader>ep"` binds a normal-mode key to
the command, and `cmd = "/path/to/eitri"` names the launcher when it is not on Neovim's `PATH`. `:help
eitri.nvim` has the same in Neovim.

`:EitriPanel` opens the panel for the current working directory, and `:EitriPanel ~/some/project` for another.
If Neovim has no server address yet, the plugin starts one. The panel's band says `attaching…` until the glue
is in, and nothing times out: Neovim answers the request once you finish a pending key or a prompt, and the band
then says so (`attaching… (nvim is waiting for a key)`).

## eitri panel

You can also start the panel from a shell:

```sh
eitri panel [--nvim <addr>] [DIR]
```

`--nvim` is the address of the Neovim to attach to (`:echo v:servername` shows it): a path to a Unix socket
that you own. It defaults to `$NVIM`, which Neovim sets for its `:terminal` and `jobstart()` children; TCP
addresses (`host:port`) are refused. With no address the panel starts unattached and says so in its band;
running `:EitriPanel` in Neovim attaches it later. `DIR` is the project, resolved exactly as for `eitri DIR`.
`--account` and `--quiet` work as they do for `eitri`; `--clean` and `--legacy` do not apply and are refused.

There is one panel per project. A second `:EitriPanel` raises the running panel's window instead of opening a
second one. From the Neovim the panel is already attached to, that is all it does; from another Neovim in the
same project, the panel attaches to that one instead, and the first Neovim loses the glue. If the Neovim quits, the panel keeps its sessions and its band says `editor detached:
run :EitriPanel to attach again`. The band's other texts are `no editor attached: run :EitriPanel in nvim`,
`editor detached: another Eitri panel attached to it` and `could not attach: <reason>`.

The panel window has the application id `cn.huntergrey.eitri.Panel` and the title `Eitri · <project directory
name>`, so a window rule can pick it out. It reads the same `~/.config/eitri/init.lua` as the one-window mode
(`agent.account` applies); Lua panels and commands registered there are not shown, and one line in the panel's
output says how many were left out. It has no editor, no bottom terminal and no layout of its own: the tab keys, `?`,
`:`, the text size keys and `f` work under the prefix, and the layout keys answer `not in a companion window`.

### Moving between the two windows

In Neovim, `Ctrl+h/j/k/l` at the edge of Neovim's own windows hands the move to the panel, which asks your
window manager to focus the neighbouring window. Eitri binds one of these keys only where your configuration
has left it empty, or at Neovim's own default (or a plain window move such as LazyVim's), so a key you mapped
yourself stays yours.

In the panel:

| key | what happens |
|---|---|
| `Ctrl+h`, `Ctrl+l` | always leave the window: the window manager moves focus |
| `Ctrl+k` | leaves the window from BROWSE; in INPUT it switches to BROWSE |
| `Ctrl+j` | leaves the window from INPUT; in BROWSE it switches to INPUT |
| `Ctrl+g` in INPUT | edits the draft in a scratch buffer in your Neovim (it says the editor is not connected when none is attached) |
| the prefix's `Select` keys | the same window-manager focus move as `Ctrl+h/j/k/l` |

Opening a file from the panel opens it in your Neovim and raises the editor's window (not from inside tmux,
see below). A second `:EitriPanel` raises the panel.

### Navigator plugins and tmux

With Neovim outside tmux, vim-tmux-navigator needs nothing: while the panel is attached, its `TmuxNavigate`
maps are treated as plain window moves, so `Ctrl+h/j/k/l` crosses to the panel at Neovim's edge.

With smart-splits.nvim, hand a move off the edge to the panel from its `at_edge` hook. `require("eitri").edge`
returns `true` when a panel took the move and `false` when none is attached:

```lua
require("smart-splits").setup({
  at_edge = function(ctx)
    if not require("eitri").edge(ctx.direction) then
      -- no panel attached: your own fallback, or nothing
    end
  end,
})
```

When Neovim runs inside tmux, nothing in Neovim's environment changes, its navigator maps are left alone, and
your tmux setup keeps moving between tmux panes as it does today. The edge of tmux's panes is tmux's. With
vim-tmux-navigator there is then no crossing to the panel window (use a binding on the tmux side, or your
window manager's keys); with smart-splits.nvim, the `at_edge` hook above hands a move at tmux's own edge to the
panel. Neither has been tried on real hardware yet (see [known issues](../known-issues.md#companion-mode)). The
editor's window is not raised when you open a file from the panel, because from inside tmux the process tree
leads to the tmux server, not to the terminal.

## Two windows from one command: eitri split

```sh
eitri split [DIR]
```

Opens upstream Neovide as the editor and the agent panel as a second window, attached to each other, with
nothing else to set up. It takes the project (`DIR`, resolved as for `eitri DIR`) and `--account`/`--quiet`,
and no other option. **It needs Neovide**, which Eitri does not bundle: `neovide` on your `PATH`, or the file
named by `EITRI_NEOVIDE` (a name that does not exist is an error, not a fallback). It runs Neovide itself with
Neovim listening on a private socket, so your `init.lua` and plugins load as they always do; it is not the
Neovide fork that the one-window `eitri` draws with.

Closing the Neovide that `eitri split` started closes the panel too (it asks first when a turn is still
running, as any close does); closing the panel leaves Neovide open, since that is your editor, and
`:EitriPanel` in it brings a panel back. The sessions are kept either way (unless `agent.restore` is `"off"`), and the next `eitri split` for the
project offers them back on the empty tab as `Restore last session` (`s`); with `agent.restore = "auto"` they come
back at launch with no key pressed. Everything above applies to the panel it opens, including the focus keys.

Neovide runs in the foreground of the shell you started `eitri split` from, as Neovide itself does by
default: `Ctrl+C` there, or closing that terminal, ends Neovide and the panel. Start it detached
(`setsid eitri split DIR`, or from a launcher) when you want them to outlive the terminal. Only the panel that
`eitri split` attached closes with its Neovide: a panel you bring back later with `:EitriPanel` does not, and
`:EitriPanel` in another Neovim moves the panel there and ends that tie.

If a panel for the project is already running, `eitri split` attaches it to the new Neovide instead of
opening another.

## Window managers

Which window manager moves focus is detected from the session; to force one or turn it off, put this in
`~/.config/eitri/init.lua` (any other value stops the panel at startup, naming the key):

```lua
eitri.config.set("companion.wm", "auto")   -- "auto" (the default), "hyprland", "sway", "niri", "gnome" or "none"
```

`"none"` never asks the window manager for anything. [Configuration](configuration.md) lists the other
`init.lua` settings.

| desktop | detected by | what Eitri does |
|---|---|---|
| Hyprland | `HYPRLAND_INSTANCE_SIGNATURE` | moves focus with `hyprctl dispatch movefocus`; what happens at the edge is Hyprland's own |
| sway | `SWAYSOCK` | moves focus with `swaymsg`, after checking that a visible window really lies in that direction (on any output), so sway's default focus wrapping does not carry you to the far side. At the edge the key is consumed |
| niri | `NIRI_SOCKET` | moves focus with `niri msg action`; what happens at the edge is niri's own |
| GNOME | `XDG_CURRENT_DESKTOP` contains `GNOME`, in a Wayland session | moves focus through the [Eitri GNOME Shell extension](#gnome-the-extension), once you have enabled it. Without it no focus moves (a Wayland client cannot take focus on GNOME by itself), and the band says once that your desktop does not let Eitri move focus and names the extension |
| KDE, anything else | none of the above | no focus moves: a Wayland client cannot take focus there. The band says once that your desktop does not let Eitri move focus; use the desktop's own window keys |

The checks run in that order: if both `SWAYSOCK` and a GNOME desktop are set, sway wins. A GNOME session on
Xorg is not detected as GNOME; `"gnome"` in `init.lua` forces it, though the extension never moves focus
between X11 windows.

Windows that are not on screen (another workspace, a hidden tab) are never the neighbour a directional move
goes to.

## GNOME: the extension

On GNOME a program cannot take focus for itself, so moving between the panel and its editor with
`Ctrl+h/j/k/l` needs a small GNOME Shell extension, `eitri@huntergrey.cn` (GNOME Shell 45 to 50). The `.deb`,
the `.rpm`, the AUR packages and the tarball installer put its four files in place
([Where things go](../../INSTALL.md#where-things-go)); **turning it on is your step**, and the installer never
does it:

```sh
gnome-extensions enable eitri@huntergrey.cn
```

On Wayland a shell only reads extensions it found at login, so an extension installed while you are logged in
starts working at your next login. Until then, and when it is not enabled, the panel behaves as on any desktop
with no focus support: no focus moves, and the band says so once.

What it does, and what it does not. It moves keyboard focus only right after you pressed a key or clicked in
the window that has focus, and only when that window belongs to the program asking or to the editor that
program named as its partner. That is how the panel moves focus from itself to its neighbour or back to its
editor, and from its editor to itself. Any program on your session bus may ask it, under the same rules, so a
program in the background can at most take focus to its own window right after you typed in a window it named
-- which is what the panel does -- and, once it has focus, hand it to a neighbour or back to the window you
were in. It reports no titles, geometry or process ids.

Limits worth knowing:

- the focus keys (`Ctrl+h/j/k/l`) move only to a window on the same monitor as the focused one; opening a review
  file from the panel brings the editor forward wherever it is;
- windows of an X11 session or an XWayland program never count, because an X11 program writes its own key
  times: run the editor as a Wayland window;
- a terminal that runs all its windows in one process (GNOME Terminal, Ptyxis) counts as one partner, so
  handing focus back to the editor goes to its most recent window;
- moving back within about a third of a second of the previous move is refused by design;
- a forwarded `eitri split` attaches the running panel but does not bring it forward (its new Neovide has had
  no key yet).
