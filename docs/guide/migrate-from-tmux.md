English | [简体中文](migrate-from-tmux.zh-CN.md)

# Coming from tmux

This page is for you if your day runs in tmux: Neovim in one pane, `claude` (or `codex`) in another, a shell or two
beside them. It says what Eitri takes from that setup as it is, what works differently, what does not come over, and
how to go back.

## What stays and what changes

What stays yours:

- **Neovim.** The editor is your own `nvim`, with your `init.lua`, your plugins and your mappings.
- **Your tmux keys.** Eitri reads your tmux configuration at startup, so your prefix and the keys you bound after it
  work as they do in tmux, wherever Eitri has a matching action.
- **Your agent.** A Claude tab runs the `claude` you installed, and its sessions load your own Claude Code settings,
  hooks, skills, plugins and `CLAUDE.md`.

What changes:

- **A desktop window instead of a terminal.** Eitri is a GTK window, and the editor is drawn by a fork of Neovide's
  renderer. Agent replies appear in a panel beside the editor as rendered Markdown, with diffs and permission cards.
- **A pane is a module, and a split asks which one.** The window is made of modules: the editor, chat views and
  terminals. A split key waits for a second key that names the module to open there: `e`, `a` or `t`.
- **A tmux window is a tab.** `<prefix> c`, `n`, `p` and the digits act on the agent panel's tabs.
- **There is no detach.** Eitri has no server to come back to, and closing the window ends the shells in its
  terminals.

## Before you start

Eitri needs Neovim 0.11 or newer. The [install page](../../INSTALL.md) lists the rest of what it needs and how to
install it, and [Known issues](../known-issues.md#where-it-runs) says which desktops it has been tried on.

## Your Neovim, as it is

Start Eitri in a project directory:

```sh
eitri ~/src/myproject
```

Started from a terminal, `eitri` gives the prompt back once the window is up, and the window runs on its own: closing
the tmux pane you started it from leaves it running.

Your Neovim configuration needs no Eitri lines. Inside Eitri's editor `vim.g.neovide` is true, so a
`if vim.g.neovide then ... end` block you keep for Neovide runs here too, and `vim.g.eitri` is set if you want a
block for Eitri alone.

Eitri takes a few keys before Neovim sees them:

| keys | in Eitri | to give them to Neovim |
|---|---|---|
| `Ctrl+b` (or your tmux prefix) | the prefix | press it twice: `Ctrl+b Ctrl+b` is Neovim's page up. A `Ctrl+a` prefix needs `bind C-a send-prefix` in your tmux configuration for this, as in tmux |
| `Ctrl+h`, `Ctrl+j`, `Ctrl+k`, `Ctrl+l` | move between modules; inside the editor they move between Neovim's own windows first | `<prefix> Ctrl+h` (and the others) |
| `Ctrl+=`, `Ctrl+-`, `Ctrl+0` | the text size of every module in the window | a `<C-=>` mapping of yours does not run |
| `F11` | fullscreen | |

- **vim-tmux-navigator works as it is.** `Ctrl+l` at Neovim's right edge reaches the module on that side, and
  `Ctrl+j` at its bottom edge reaches the terminal below. Other navigator plugins are in
  [Moving between panes](keys.md#moving-between-panes).
- **Without a navigator plugin** Eitri gives `Ctrl+h/j/k/l` that behaviour itself, but only on a key your
  configuration has not mapped to something else.
- **Session plugins.** If you use persistence.nvim, auto-session, resession.nvim or another session plugin Eitri
  recognises, it leaves reopening files to that plugin.
- **A quick check.** `eitri --clean` starts the editor with `nvim --clean`, so you can tell whether a problem comes from
  your configuration.

Eitri's own settings live in a separate file, `~/.config/eitri/init.lua` (under `$XDG_CONFIG_HOME` when that is set).
Your Neovim `init.lua` stays where it is.

## Your tmux prefix and keys

Eitri reads the files tmux reads, in tmux's order: `/etc/tmux.conf`, `~/.tmux.conf`,
`$XDG_CONFIG_HOME/tmux/tmux.conf` and `~/.config/tmux/tmux.conf`, and the files they `source-file` by an absolute path.
It reads them as text: it never starts tmux and never asks a running one. The import is on by default.

The first time, and again only when what it imports changes, a notice at the top of the window says what it took:

```text
Using your tmux keys from ~/.tmux.conf: prefix Ctrl+a, 8 keys (18 skipped). Ctrl+a ? lists them; new-window opens a new tab. keymap.from_tmux = "off" turns this off.
```

`<prefix> ?` then lists every key, with `(tmux)` after the ones that came from your configuration. At the end of the
list, "Skipped from tmux" gives each line that was not taken, with its file, its line number and the reason, and
"Imported from tmux with changes" gives each binding that came across with part of it dropped.

What comes across:

- `set -g prefix C-a` (any Ctrl or Alt chord, or a function key) and a `send-prefix` binding.
- `bind -r`: the key repeats without the prefix for half a second, as in tmux.
- `split-window -h` and `-v`. The key then waits for a module key, so `<prefix> |` (if that is your split) followed by
  `t` opens a terminal. With `-c "#{pane_current_path}"` the new shell starts in the directory of the terminal you
  split. A size such as `-l 30%` is not honoured: the new module takes half, and the binding is listed under "Imported
  from tmux with changes". `-b` (before this pane) and `-f` (full window) skip the binding.
  [Your tmux keys](keys.md#your-tmux-keys) has every flag.
- `select-pane -L/-R/-U/-D`, `-l`, `-m` and `-M`; `last-pane`; `resize-pane` with a direction and a count, and `-Z`
  (zoom); `swap-pane -U/-D`; `rotate-window`; `select-layout` with `even-horizontal`, `even-vertical`,
  `main-horizontal`, `main-vertical` or `tiled`, and `-E`; `next-layout`; `display-panes`; `kill-pane`.
- `new-window`, `next-window`, `previous-window`, `last-window`, `kill-window` and a `rename-window` prompt act on tabs.
  `new-window` opens a new tab, and the notice says so. `break-pane` breaks the tab out into a chat view of its own.
- `switch-client -n`, `-p` and `-l` step between workspaces, and `choose-tree -s` chooses one; a plain `choose-tree`
  opens the session list.
- `copy-mode` and `copy-mode -u`, `unbind`, and `unbind -a`. After `unbind -a` Eitri's own keys stay: the module keys,
  HINT, `?`, the panel reload and the tab keys tmux has no command for.

What is skipped, with the reason in `<prefix> ?`:

- **Root keys** (`bind -n`, `-T root`): without the prefix, keys belong to Neovim and the panel. Bind one in Eitri's
  `init.lua` with `eitri.keymap.set("root", ...)`. vim-tmux-navigator's `bind -n C-h if-shell ...` lines are skipped
  for this reason; Eitri moves between modules on `Ctrl+h/j/k/l` itself.
- **Copy-mode tables** (`bind -T copy-mode-vi ...`) and any table but `prefix`.
- **Commands Eitri has no equivalent of**: `display-popup`, `display-menu`, `paste-buffer`, `choose-buffer`,
  `source-file` inside a binding, and others.
- **Anything that runs a shell**: `run-shell` and `if-shell`. Eitri never runs your tmux configuration's commands, so
  TPM, the plugins it loads and the keys they bind do not come across.
- **A binding that runs more than one command**, `send-keys` of any key but `C-a`, `select-window` (Eitri's
  `<prefix> 1` to `9` already pick tabs, counted from 1), `prefix2`, `set prefix` without `-g`, and `source-file` with a
  relative path (tmux resolves it against a directory Eitri cannot know).
- Options that are not about keys, such as `mouse`, `status`, colours and `base-index`, are ignored and not listed.

To turn the import off, put this in Eitri's `init.lua`:

```lua
eitri.config.set("keymap.from_tmux", "off")
```

Your own `eitri.keymap` calls in that file win over what came from tmux, with the import on or off. With it off, a
`Ctrl+a` prefix is `eitri.keymap.prefix("C-a")`; [`docs/keymap/tmux-ctrl-a.lua`](../keymap/tmux-ctrl-a.lua) is a whole
example to paste from, and the [tmux recipe](../../examples/environments/tmux/README.md) is an `init.lua` for someone
who lives in tmux.

The first time you start Eitri without its own `init.lua`, a setup screen shows the tmux configuration it found and
offers a "Neovim + tmux" preset that keeps the import on ([First start](configuration.md#first-start)).

On Hyprland, sway and niri, Eitri by default puts chats and pages in windows of your window manager.
`window.tiling = "eitri"` in Eitri's `init.lua` keeps everything inside the Eitri window instead.
[Compositor windows](compositor-windows.md#keys) says which keys work in each case.

## Your agent and its configuration

A Claude tab runs the `claude` on your `PATH`, or else the one Claude Code's installer put in `~/.local/bin`;
[Getting started](getting-started.md) lists every place it looks. Every session loads your own Claude Code
configuration: your settings, hooks, plugins, skills, `CLAUDE.md` and permission rules.
`eitri.config.set("agent.user_settings", false)` leaves them out.

A project's own configuration (`.claude/`, `.mcp.json`, `CLAUDE.md`) loads only after you trust the project: the first
session there asks, and `y` loads it. `<prefix> i` shows what a tab's session loaded. `<leader>t` in a tab gives you
the `claude --resume ...` command that continues the conversation in a terminal.

You can also keep running `claude` in a terminal module as you do now. It is an ordinary shell there, and that `claude`
keeps its own prompts and modes. Codex and agents that speak ACP can have tabs too; see
[Codex and other agents](agents.md).

## tmux to Eitri

| tmux | Eitri |
|---|---|
| pane | module: the editor, a chat view (`agent`, `agent#2` ... `agent#9`) or a terminal (`terminal`, `terminal#2` ... `terminal#9`) |
| `%`, `"` (split) | `<prefix> %` or `"`, then `e`, `a` or `t` |
| `z`, `x`, `o`, `;`, `q`, `{`, `}`, `Space`, `M-1` ... `M-5` | the same keys, on modules |
| window | tab: `<prefix> c`, `n`, `p`, `l`, `,`, `&`, `1` ... `9` |
| `w` (choose window) | `<prefix> w`: the open tabs and the saved sessions |
| session | a project: `<prefix> O` chooses one |
| `switch-client -n/-p/-l`, `s`, `$` | workspaces: `<prefix> (`, `)`, `L`, `s`, `$`; a workspace is another arrangement of the same modules |
| `d` (detach) | none. `<prefix> d` hides a module, and waits for `e`, `a` or `t`; `Esc` cancels |
| `[` (copy mode) | `<prefix> [` in a terminal: scroll back |
| `display-popup` | a float or the scratchpad: `<prefix> F`, `<prefix> _`, `<prefix> S` |
| `?` | `<prefix> ?`, with what your tmux import left out |

**Copying from a terminal.** Copy mode scrolls back and has no selection or search yet. Select with the mouse instead:
a drag, a double click or a triple click (the whole line, newline included) copies to the clipboard and the primary
selection when you let go. `Ctrl+Shift+V` pastes and a middle click pastes the primary selection.
[Containers and terminals](keys.md#containers-and-terminals) has the copy-mode keys and the rest.

**A popup terminal.** For a drop-down terminal on one key, as `display-popup` gives you, give `scratchpad.show` a
module in Eitri's `init.lua`:

```lua
eitri.keymap.set("prefix", "F2", "scratchpad.show", { module = "terminal#2" })
```

[Floats, the scratchpad and workspaces](keys.md#floats-the-scratchpad-and-workspaces) says what each press does.

## What does not carry over

- **tmux plugins.** Nothing your tmux configuration runs is run, so TPM, tmux-resurrect, tmux-continuum, themes and
  the keys they bind are not there.
- **Detach, and a server that outlives the window.** Closing the window ends its terminals. When one is running a
  command it asks first (`close window? 1 terminal job (y/n)`).
- **Terminal contents after a restart.** At the next start in the project the arrangement of modules comes back, a
  terminal in its place with a fresh shell and none of its old output. The files you had open come back in Neovim,
  split as they were, unless you start with `eitri --clean` or a session plugin does the reopening. The agent tabs are
  offered as `Restore last session`; `s` brings them back with their conversations.
  [Containers and terminals](keys.md#containers-and-terminals) says what each setting changes.
- **A tmux on another machine.** `ssh` works in a terminal module as in any terminal, but Eitri never attaches to a
  tmux, here or there.
- **Root bindings and copy-mode bindings** from your tmux configuration (see above).
- **`base-index`.** Tabs are numbered from 1.
- **The status line.** tmux's status bar and its formats are not read; Eitri has its own top bar.

Terminals are Eitri's own. They draw in your Neovim's `guifont` unless `terminal.font` says otherwise.

## Going back, or meeting halfway

Two ways keep your terminal Neovim:

- **The panel beside your Neovim** ([Companion mode](companion.md)). `:EitriPanel` in your terminal Neovim (from the
  `eitri.nvim` plugin that comes with Eitri) opens the agent panel as a window of its own and attaches it to that
  Neovim. [Known issues](../known-issues.md#companion-mode) says what has been tried so far; a Neovim inside tmux is not
  among it yet.
- **Move the running Neovim in** ([Take over a terminal nvim](takeover.md)). `:EitriOpen` in Neovim 0.12 or later
  moves that Neovim into an Eitri window, and gives it back to the terminal when the window ends.

To stop using Eitri, close its windows. Your tmux configuration was only read. Eitri's own files are its `init.lua`
in `~/.config/eitri/` and what it keeps in `~/.local/state/eitri/`.

## Next

- [Getting started](getting-started.md): the first turn, permission cards, tabs and sessions.
- [Keys](keys.md): every key, and [Your tmux keys](keys.md#your-tmux-keys).
- [Working environments](environments.md): the tmux recipe and the others.
- [Known issues](../known-issues.md): what is rough or untested today.
