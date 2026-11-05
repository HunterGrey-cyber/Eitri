English | [简体中文](companion.zh-CN.md)

# Beside your own Neovim

Eitri can also run as the agent panel alone: a separate window next to the Neovim you already use, in a
terminal (inside tmux or not), in upstream Neovide, or in any other Neovim GUI. This page covers how to start
it, how focus moves between the two windows, how a plugin page such as the Markdown preview opens beside your Neovim on
its own, and what each desktop needs. The window you get from plain
`eitri` is described in [Getting started](getting-started.md); the panel's own keys are the same in both, and
[Keys](keys.md) lists them.

The panel attaches to your Neovim over its RPC socket and installs a little glue inside it: the file you have
open and your Visual selection go to the agent, the panel takes your colorscheme and shows your which-key keys,
buffers reload while the agent works (see [Buffers reload](#buffers-reload)), a file opens at a line from the panel, `Ctrl+g` edits a draft in
Neovim, and `<leader>ae` in Visual mode sends you to the panel's box with the selection attached and
`Edit the selection: ` ready to finish (where the window manager can be asked to bring the panel forward; elsewhere use your
desktop's own window keys). It removes all of it again when the panel goes away. Nothing is written to your Neovim configuration.
Your editor keeps its own speed and keys, and your window manager arranges the two windows.

Plain `eitri` on Hyprland, sway or niri also puts its chats and pages in windows of their own, around Eitri's editor
rather than yours: see [Compositor windows](compositor-windows.md).

To move a Neovim that is already running in your terminal into an Eitri window instead, and have it back in the terminal when the
window ends, see [Take over a terminal nvim](takeover.md).

## The eitri.nvim plugin

`eitri.nvim` is a thin launcher: everything the panel needs inside Neovim, the panel installs itself, so the
plugin and the installed Eitri never have to match versions. The packages (`.deb`, `.rpm`, AUR) put it in
`/usr/share/eitri/nvim/eitri.nvim`; the tarball installer puts it in `~/.local/share/eitri/eitri.nvim` (under
`$XDG_DATA_HOME/eitri/` when that is set). See [Where things go](../../INSTALL.md#where-things-go) for the rest
of the installed files. With lazy.nvim, point a spec at the directory:

```lua
-- .deb, .rpm, AUR
{ dir = "/usr/share/eitri/nvim/eitri.nvim", cmd = { "EitriPanel", "EitriPage" } },

-- the tarball installer (install.sh)
{ dir = vim.fn.expand("~/.local/share/eitri/eitri.nvim"), cmd = { "EitriPanel", "EitriPage" } },
```

Without a plugin manager, add the directory to `runtimepath`:
`set runtimepath+=/usr/share/eitri/nvim/eitri.nvim`.

Calling `require("eitri").setup({ ... })` is optional. `mapping = "<leader>ep"` binds a normal-mode key to
the command, `page_mappings = { ["<leader>em"] = "markdown" }` binds normal-mode keys to
[pages beside your nvim](#pages-beside-your-nvim) (none by default), and `cmd = "/path/to/eitri"` names the launcher when it
is not on Neovim's `PATH`. `:help eitri.nvim` has the same in Neovim, and `:checkhealth eitri` finds the program, prints its
version and lists the page ids. Nothing of Eitri runs in your Neovim until you use one of the commands. A lazy.nvim
spec that names only `EitriPanel`, as this page used to show, leaves `:EitriPage` undefined until the panel has been
opened once: name both commands, as above.

`:EitriPanel` opens the panel for the current working directory, and `:EitriPanel ~/some/project` for another.
If Neovim has no server address yet, the plugin starts one. The panel's band says `attaching…` until the glue
is in, and nothing times out: Neovim answers the request once you finish a pending key or a prompt, and the band
then says so (`attaching… (nvim is waiting for a key)`).

`:EitriOpen` is the plugin's other command for the panel's window: it moves this Neovim into an Eitri window ([Take over a terminal
nvim](takeover.md)). Name `"EitriOpen"` in a lazy.nvim spec's `cmd` list to use it.

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

The panel window has the application id `cn.huntergrey.eitri.Panel` and the title `Eitri · <project directory name>`,
so a window rule can pick it out. It reads the same `$XDG_CONFIG_HOME/eitri/init.lua` (default
`~/.config/eitri/init.lua`) as the one-window mode (`agent.account` applies); Lua panels and commands registered there
are not shown, and one line in the panel's output says how many were left out. It has no editor, no bottom terminal
and no layout of its own: the tab keys, `r`, `?`, `:`, the text size keys, `f`, sending the prefix itself and the
arrow keys (toward the neighbouring window) work under the prefix, and a layout or window key answers
`not here: your window manager arranges this window` the first time you press one in a window (later presses say nothing).
A key you gave a plugin page with `eitri.plugin.load("markdown", { key = "b" })` in `init.lua` opens or raises
that [page's own window](#pages-beside-your-nvim) for the attached Neovim; with no Neovim attached it says
`no editor attached: run :EitriPanel in nvim`.

### Moving between the two windows

In Neovim's Normal and Visual modes, `Ctrl+h/j/k/l` at the edge of Neovim's own windows hands the move to the panel, which asks your
window manager to focus the neighbouring window. Eitri binds one of these keys only where your configuration
has left it empty, or at Neovim's own default (or a plain window move such as LazyVim's), so a key you mapped
yourself stays yours. In Insert mode only `Ctrl+l` is bound (it leaves Insert mode first); `Ctrl+h`, `Ctrl+j` and `Ctrl+k` keep their Insert-mode meaning there.

In the panel:

| key | what happens |
|---|---|
| `Ctrl+h`, `Ctrl+l` | always leave the window: the window manager moves focus |
| `Ctrl+k` | leaves the window from BROWSE; in INSERT it switches to BROWSE |
| `Ctrl+j` | leaves the window from INSERT; in BROWSE it switches to INSERT |
| `Ctrl+g` in INSERT | edits the draft in a scratch buffer in your Neovim (it says the editor is not connected when none is attached) |
| the prefix's `Select` keys | the same window-manager focus move as `Ctrl+h/j/k/l` |

Opening a file from the panel opens it in your Neovim and raises the editor's window (not from inside tmux,
see below). A second `:EitriPanel` raises the panel.

**`Ctrl+l` with a split to the right.** The key moves inside Neovim first: with another Neovim window to the right
it goes there, and only at the right edge does it go to the panel (the same for the other three directions). That holds in Normal and Visual mode, and in Insert mode for `Ctrl+l`. Where Eitri takes Neovim's own default `Ctrl+l`, that key no
longer clears the highlight and redraws, as with vim-tmux-navigator; map `:nohlsearch | diffupdate | redraw` to a key
of your own if you want it back.

**A key mapped by hand.** If you map a key yourself rather than through a navigator plugin, run the window move
first and hand the move to the panel only when it did not move, because `require("eitri").edge` does not look at
Neovim's windows:

```lua
vim.keymap.set("n", "<C-l>", function()
  local before = vim.fn.winnr()
  vim.cmd.wincmd("l")
  if vim.fn.winnr() == before then
    require("eitri").edge("right")
  end
end)
```

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

The same `at_edge` snippet works inside Eitri's own window, where `require("eitri").edge` hands the move to the
neighbouring module instead (see [Keys](keys.md#moving-between-panes)), so one configuration serves both.

When Neovim runs inside tmux, nothing in Neovim's environment changes, its navigator maps are left alone, and
your tmux setup keeps moving between tmux panes as it does today. The edge of tmux's panes is tmux's. With
vim-tmux-navigator there is then no crossing to the panel window (use a binding on the tmux side, or your
window manager's keys); with smart-splits.nvim, the `at_edge` hook above hands a move at tmux's own edge to the
panel. Neither has been tried on real hardware yet (see [known issues](../known-issues.md#companion-mode)). The
editor's window is not raised when you open a file from the panel, because from inside tmux the process tree
leads to the tmux server, not to the terminal. Neovim inside tmux also hears of focus changes only with
`set -g focus-events on` in your `tmux.conf`; without it the reload on focus, described next, does not happen.

### Buffers reload

While the agent is working (a turn is running, or a background task it started), Neovim checks once a second whether
a file changed on disk and reloads the buffers you have not touched. It also checks once at the end of every turn, when
the panel detaches or closes (so a panel killed mid-turn still reloads what it wrote last), and when Neovim gets focus
back. Outside those times nothing polls. A buffer you have changed yourself is left alone, with Neovim's own warning.
Two cases are not covered: when you replace one panel with another in the middle of a turn, files the first panel's
turn writes afterwards are reloaded at the new panel's next busy moment, on focus, or by hand; and a program the agent
started that keeps writing after the panel was killed is not reloaded either.

## Pages beside your nvim

A plugin page, such as the [Markdown preview](markdown-preview.md), can also open as a window of its own next to your
Neovim, with no agent panel. It follows the buffer of the Neovim it started with. Start it from a shell or from Neovim:

```sh
eitri page <id> [--nvim <addr>] [DIR]
eitri page --list
```

```vim
:EitriPage markdown
:EitriPage markdown ~/some/project
```

- `--nvim` and `DIR` are as for `eitri panel`: `--nvim` defaults to `$NVIM` and is required when that is unset, a path
  to a Unix socket you own; `DIR` is the project. `--account`, `--quiet`, `--clean`, `--legacy` and `--new-window` are
  refused. `:EitriPage {id} [dir]` completes plugin ids, then directories, and `require("eitri").page("markdown")` is the
  call for a key of your own. `eitri page --list` prints the ids that can open this way, one per line.
- Typing the command is the permission to load that plugin: by id, with its default settings, unless your `init.lua`
  loads it with options (`eitri.plugin.load("markdown", { ... })`), which are then used. Other plugins `init.lua` loads are
  not built in this window (one line in its log says how many). It has no Lua panels or commands. A key you gave
  another page starts or raises that page's window.
- Only plugins that ask for no more than these abilities open this way: following the editor's buffer
  (`editor.follow`), defining Neovim commands (`editor.commands`), opening files (`editor.open`), keeping state and
  reaching the network. A plugin that needs more says so and nothing opens:
  `eitri page: <id> needs Eitri's own window (it declares <capability>)`, exit status 64. The Markdown preview qualifies.
- A page window starts only when the plugin is on the plugin search path (`eitri page: no plugin "<id>" on the plugin
  search path`, exit 1). `eitri page: could not attach: <reason>` (exit 1) means it could not reach your Neovim.
- There is one page window per Neovim and plugin: a second `:EitriPage markdown` from the same Neovim raises the window
  that is running. The same command from another Neovim opens a second window that follows that one.
- You can also open a page from the agent window: the prefix and the page's key. In a page window the prefix and
  `a` raises the project's agent window, or starts one for this Neovim when none runs; an agent window attached to another
  Neovim keeps it. The prefix and `e` raises the window that holds your Neovim.

**What it puts in your Neovim.** A small slot of its own (`_G.__eitri_pages`): the page's colours follow your
colorscheme, and the plugin's own commands exist only while the window does (for Markdown: `:EitriMarkdown`,
`:EitriMarkdownToggle`, `:EitriMarkdownHide`). It defines no keys, maps nothing on `Ctrl+h/j/k/l`, makes no scratch
buffers, reloads nothing, and never replaces a command of your own with the same name. The agent window's glue and each page's
slot are independent: closing or killing one removes that one and nothing else, and the agent's `Ctrl+g` draft is never shown
in the preview.

**How it ends.**
- Close it with your window manager, the prefix and `x`, or `:EitriMarkdownHide` / `:EitriMarkdownToggle`: it closes without
  asking, and its slot leaves your Neovim.
- `:qa` in Neovim closes the page window (exit status 0, `[page] the editor went away; closing` in its log); the agent window
  stays, showing `editor detached`.
- If the window is killed, its slot is gone from your Neovim within a second or two; a Neovim that is blocked at a prompt removes it once it answers again.

**What hide and toggle mean here.** The plugin's commands live in the slot, and the slot goes with the window. So after
`:EitriMarkdownHide` or `:EitriMarkdownToggle` the window is closed, and `:EitriMarkdownToggle` cannot bring it back:
`:EitriMarkdown` then fails with Neovim's own `E492: Not an editor command`. `:EitriPage markdown` opens it again.
While it is open, `:EitriMarkdown` raises it, and with `auto = true` the preview follows the buffers you enter without taking
focus or coming forward.

**Keys in a page window.** The page's own keys work as in any window (`?` in the page lists them; `<prefix> ?` says
`the page's own ? lists its keys`). `Enter` on a heading moves your Neovim's cursor there and brings the editor's window
forward.

| key | what happens |
|---|---|
| a layout, window, workspace or float key | `not here: your window manager arranges this window` the first time, nothing after |
| a chat key (tab keys, `r`, `:`) | `no chat here: press the prefix and a to open the agent` the first time |
| prefix `e` | raises your Neovim's window; inside tmux it says `your nvim runs inside tmux: use your window manager's keys to reach it` once |
| prefix `a` | raises the agent window, or starts one |
| prefix `x`, text size, `f` | as in any module window |
| `Ctrl+h/j/k/l` past the page's edge | your window manager moves focus |

**Where it is refused.** In Eitri's own Neovim, which opens pages itself: `this window opens pages itself: press the prefix
and the page's key`. In a Neovim that Eitri has taken over: `this nvim is open in an Eitri window: its pages open there`
(exit status 64, no window).

The window's application id is `cn.huntergrey.eitri.Page` and its title is `<project> — <plugin title> · Eitri`, so one window
rule covers it. Eitri moves nothing: the window manager places it. Started from Neovim, its output goes to
`$XDG_STATE_HOME/eitri/companion/page-<16 hex>.log` (default `~/.local/state/eitri/companion/`).

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
default: `Ctrl+C` there, or closing that terminal, ends Neovide and the panel. This does not hold when a panel for the project is already running: `eitri split` then attaches it, returns at once and leaves Neovide running behind your prompt, so `Ctrl+C` at the prompt ends nothing. Start it detached
(`setsid eitri split DIR`, or from a launcher) when you want them to outlive the terminal. Only the panel that
`eitri split` attached closes with its Neovide: a panel you bring back later with `:EitriPanel` does not, and
`:EitriPanel` in another Neovim moves the panel there and ends that tie.

If a panel for the project is already running, `eitri split` attaches it to the new Neovide instead of
opening another.

## Window managers

Which window manager moves focus is detected from the session; to force one or turn it off, put this in
`$XDG_CONFIG_HOME/eitri/init.lua` (default `~/.config/eitri/init.lua`; any other value stops the panel at
startup, naming the key):

```lua
eitri.config.set("companion.wm", "auto")   -- "auto" (the default), "hyprland", "sway", "niri", "gnome" or "none"
```

`"none"` never asks the window manager for anything. [Configuration](configuration.md) lists the other
`init.lua` settings.

| desktop | detected by | what Eitri does |
|---|---|---|
| Hyprland | `HYPRLAND_INSTANCE_SIGNATURE` | moves focus with `hyprctl dispatch`, after checking that a visible window really lies in that direction, because Hyprland's own move picks a window even when nothing lies that way: a key at a real edge stops there. Windows that overlap, such as floats, are left to Hyprland. Eitri first asks Hyprland once whether it runs a Lua config (`hyprland.lua`) or a `hyprland.conf`, and writes its commands the way that config takes them |
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
`Ctrl+h/j/k/l` needs a small GNOME Shell extension, `eitri@huntergrey.cn` (GNOME Shell 45 to 51). The `.deb`,
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
- for 30 minutes it remembers the last time you yourself put focus in the panel (a click, Alt+Tab); moving into
  it with the focus keys or `:EitriPanel`, or the panel taking focus when it opens, does not count. So when the
  panel has just opened, past those 30 minutes, and after every screen lock, the focus key from the editor into
  the panel and `:EitriPanel` do nothing until you click the panel once (with focus following the mouse, switch to
  it with Alt+Tab or the overview instead: a pointer that only moves earns nothing);
- a draft, a `gf` or an opened review file brings the editor forward only when the panel's focus came from you or
  from the editor (the focus keys, `:EitriPanel`); when the panel took focus on its own (it has just opened, one of
  its own windows closed, after a screen lock), it does nothing until you put focus in the panel yourself: click
  another window and then the panel, or Alt+Tab away and back (a click on the panel while it already has focus
  changes nothing);
- a forwarded `eitri split` attaches the running panel but does not bring it forward (its new Neovide has had
  no key yet).
