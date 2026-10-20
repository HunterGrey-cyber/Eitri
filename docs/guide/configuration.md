English | [简体中文](configuration.zh-CN.md)

# Configuration

Eitri has one configuration file, a Lua script that runs once at startup, and a handful of environment
variables for the cases where a setting belongs to a shell or a launcher rather than to you. This page lists
every setting, what it does and what it defaults to. Where a setting changes keys, the page says so and points
to [Keys](keys.md).

## init.lua

Eitri reads `~/.config/eitri/init.lua` when a window starts. Set `EITRI_CONFIG_DIR` to a directory to read
`init.lua` from there instead. There is no file to create first: without one, Eitri starts with its built-in
defaults and prints a line saying so.

Settings are made with `eitri.config.set(key, value)`. A value is a string, a number or a boolean (`true` or
`false`); `nil` unsets the key again. A value Eitri cannot keep (a table or a function, say) is a startup
failure.

```lua
eitri.config.set("agent.restore", "auto")
eitri.config.set("review.hint", true)
```

Three rules hold for every setting below:

- A value a setting does not accept stops Eitri at startup with a message that names the key. A misspelt value
  never falls back to the default quietly.
- A key Eitri does not know is not an error and does nothing.
- A Lua error in the file itself (a typo in the code, say) is printed in the terminal you started Eitri from
  and does not stop the launch: Eitri opens with whatever the file had set before the error.

`require` finds modules only under the `lua/` directory next to `init.lua`: `require("mine")` loads
`~/.config/eitri/lua/mine.lua` or `~/.config/eitri/lua/mine/init.lua`, and `require("a.b")` loads
`lua/a/b.lua` or `lua/a/b/init.lua`. It never looks in the project you opened or in the current directory, and
changes to `package.path` or `package.cpath` have no effect.

Both windows read the same file: the one-window mode and the [companion window](companion.md).

## Settings

| key | value | default | what it does |
|---|---|---|---|
| `agent.account` | an account name | unset | The Claude account the window uses. See [Accounts](#accounts). |
| `agent.font_size` | a number from 9 to 32 | the editor's size | The agent panel's base text size, in pixels. The conversation is drawn at 12/14 of the base (a reply, tool lines and code), so it reads a little smaller than the editor's text; the status line at the bottom of the panel is drawn at the base itself, so it lines up with the editor's statusline. Unset, the base is the editor's own size (`guifont`). |
| `agent.typing_cadence_hz` | a whole number from 1 to 60, or `"off"` | `5` | While you type in the editor, how many times a second the agent panel is allowed to update. A steady slower rate keeps the panel from redrawing under your typing; `"off"` lets it update at full speed. |
| `agent.default` | `"claude"`, `"codex"` or the name of an agent registered with `eitri.agent.register` | `"claude"` | The agent a new tab runs, and the first tab of a fresh window. See [Codex and other agents](agents.md). |
| `agent.codex.command` | an absolute path, or a bare command name | unset | The `codex` a Codex tab runs. `EITRI_CODEX` outranks it; without either, `codex` on `PATH`. A relative path with a directory part stops Eitri at startup. |
| `agent.default_mode` | `"manual"`, `"auto"` or `"bypass"` | `"auto"` | The mode a new tab starts in. See [How a launch starts](#how-a-launch-starts). |
| `agent.restore` | `"offer"`, `"auto"` or `"off"` | `"offer"` | What happens to the tabs the last window on this project had open. See [How a launch starts](#how-a-launch-starts). |
| `agent.thinking` | `"collapsed"`, `"preview"`, `"expanded"` or `"hidden"` | `"collapsed"` | How the agent panel shows the agent's thinking. `"collapsed"` draws one quiet row per thought, opened with `Enter` like a tool's output; `"preview"` also shows the newest lines of a thought while it is being written, in a box that does not grow; `"expanded"` shows every thought in full; `"hidden"` shows none. `Ctrl+o` shows every thought in full whatever this says. |
| `agent.user_settings` | `true` or `false` | `true` | Whether agent sessions load your own Claude Code configuration (your hooks, plugins, skills, `CLAUDE.md` and permission rules), as `claude` does in a terminal. `false` leaves your own configuration out; a project's own configuration is governed by [trust](permissions.md), not by this key. |
| `modules.chat.on_permission` | `"badge"` or `"reveal"` | `"badge"` | What happens when a permission card arrives while the agent panel is hidden. `"badge"` shows `agent ⚑N` in the top bar and a toast; `"reveal"` brings the panel back where it was, without moving the keys. While another pane is zoomed `"reveal"` shows the badge instead, because showing the panel would not put it on screen. A companion window never hides its panel, so the key does nothing there. |
| `review.enabled` | `true` or `false` | `true` | Whether [turn review](turn-review.md) runs at all. `false` takes no snapshots and offers no review. |
| `review.hint` | `true` or `false` | `false` | Whether a finished turn that edited files adds a one-line hint to the status band, and a running turn's edits are counted on a line above the input box. The review is one key away either way. |
| `keymap.from_tmux` | `"on"` or `"off"` | `"on"` | Whether Eitri reads your tmux prefix and prefix-table keys from your tmux configuration. See [Keys](#keys). |
| `companion.wm` | `"auto"`, `"hyprland"`, `"sway"`, `"niri"`, `"gnome"` or `"none"` | `"auto"` | Which window-manager adapter moves focus between the [companion window](companion.md) and your editor. `"auto"` detects it from the session; `"none"` never asks the window manager for anything. |

The settings are read once, when the window starts. Change `init.lua` and open a new window to apply a change.

## Registering another agent

`eitri.agent.register` is not a setting: it takes a table and adds an agent that speaks ACP over standard input
and output, so that `:tabnew NAME` and `agent.default` can use it. Its keys, what the trust question does for it
and what Eitri can and cannot do for such an agent are on [Codex and other agents](agents.md#other-agents-over-acp).
A bad registration stops Eitri at startup and names the key.

## Keys

Keys are set with `eitri.keymap` in the same file:

```lua
local k = eitri.keymap
k.prefix("C-a")                               -- the prefix key
k.set("prefix", "m", "zoom")                  -- bind m after the prefix to an action
k.del("prefix", "%")                          -- remove a default binding
```

`k.prefix(key)` changes the prefix, `k.set(table, key, action, options)` binds a key and `k.del(table, key)`
removes one. A bad key, an unknown action or two bindings that collide stop Eitri at startup with a message
naming the call, so a typo does not leave you with a keymap you did not write. [Keys](keys.md) lists the
defaults and the actions.

The defaults are stock tmux's, with `Ctrl+b` as the prefix. Eitri then reads your tmux configuration (the same
files tmux reads, and what they `source-file` by an absolute path; a relative path is skipped, because Eitri
does not know the directory tmux would resolve it against), and finally applies your `eitri.keymap` calls, so your own
calls win. Eitri only reads tmux's files; it never starts or asks tmux. Set `keymap.from_tmux` to `"off"` to
skip the tmux step.

A complete example, which moves the prefix to `Ctrl+a` and rebinds splits, resizing and zoom, is
[`docs/keymap/tmux-ctrl-a.lua`](../keymap/tmux-ctrl-a.lua). Paste what you want from it into your `init.lua`.

## Layout from Lua

Every key in [Keys](keys.md) that changes the layout is also an action with a name, and Lua reaches the
same actions by the same names. These calls are for a command (`eitri.command.register`) or an event handler: they
queue what you ask for, and Eitri carries it out when the handler returns, in order. Called while `init.lua` is
still loading, before the window exists, they stop with a message that says so; `eitri.layout.default` is what
shapes the first window.

```lua
eitri.command.register({
  id = "tidy",
  title = "Rearrange the window",
  keybinding = "<Control><Alt>r",
  action = function()
    eitri.layout.run("layout.main-vertical")      -- any layout action, by its eitri.keymap name
    eitri.layout.run("rotate.down")
    eitri.layout.run("resize.left", { cells = 5 }) -- options go in a table, as in eitri.keymap.set
  end,
})
```

- **`eitri.layout.run(name, options?)`** runs one layout action on the module that has the keys: the ones in the
  Modules table of [Keys](keys.md) (arrangements, swaps, moves to an edge, marks, zoom, resize, hide, close)
  and the ones with no key (`swap.marked`, `layout.undo`, `layout.previous`, the flips, `move.far-right`). To act on
  another module, call `eitri.layout.focus(id)` first. Closing a module still asks its `y/n` question. An action that
  is not a layout action (a tab action, say) is refused naming it, and so are three more: `split.right` and
  `split.below` (they wait for a module key; `eitri.layout.split(id, "right")` or `"below"` does the work), `select.number`
  (it waits for a digit; `eitri.layout.focus(id)` gives a module the keys) and the `repeatable` option, which belongs
  to a key binding. Two more are for terminals: `eitri.layout.run("instance.new", { kind = "terminal" })` opens a
  new terminal to the right of the module that has the keys, as `<prefix> Alt+Enter` does (`kind = "agent"` is
  refused for now with `chat views: not in this build`), and a split action takes `cwd`, below.
- **`eitri.layout.mark(id)`** marks that module without moving the keys, as `<prefix> m` marks the one that has them.
- **`eitri.layout.get()`** returns a new table describing the layout as it is now, in the shape
  `eitri.layout.default` takes: `{ "row", { "editor", 0.6 }, { "column", { "agent" }, { "terminal" } } }`, where
  every child but the last carries its share of the container. It adds `focus` (the module with the keys), `hidden` (a list),
  `zoomed` and `marked` (a module, or nothing). Pinned bottom rows are not written into the table; the default shape
  restores a bottom row's height by itself. Called from inside a layout change it stops with a message instead of
  reading half a change.

**The shape `eitri.layout.default` and `get()` use.** A table starts with `"row"` or `"column"` (two or more
children, side by side / one above the other), or with `"tabbed"` or `"stacked"` (one or more children, one shown
at a time, with a row of titles / a row of title for each). A child is a module name, then optionally its share of
the container as a second item (`{ "editor", 0.6 }`), or a table of its own, whose share goes in as `share = 0.4`.
A child without a share takes an equal part of what the others leave; every share is from 0.05 to 1 and together
they come to 1. `active = 2` on a tabbed or stacked table names the child shown (counting from 1; the first by
default). A numbered terminal (`terminal#2` to `terminal#9`) can be a child, as in the example below, and the window
gives it its own shell once it is shown. A misspelled field stops the launch, naming it. An example with every kind of container:

```lua
eitri.layout.default({
  "row",
  { "editor", 0.55 },
  { "tabbed", { "agent" }, { "terminal" }, { "terminal#2" }, active = 1 },
})
```

**A split that starts in the terminal's directory.** `split.right` and `split.below` take `cwd = "pane"`, which
makes the new terminal's shell start in the directory the terminal you split from is in (read from the shell
while the key is pressed). When that shell has ended, its directory cannot be read, or the key is pressed in a
module that is not a terminal, the new shell starts in the project root. Without the option, or with `cwd = "start"`, the new shell starts in the project root.

```lua
k.set("prefix", "\\", "split.right", { cwd = "pane" })
k.set("prefix", "-", "split.below", { cwd = "pane" })
```

With the tmux import on, a `split-window` binding with `-c "#{pane_current_path}"` becomes the same `cwd = "pane"`.
Any other `-c` value is not imported: the binding still works, and its new shell starts in the project root.

**Where the layout is kept.** Eitri remembers each project's layouts, one for each workspace, in
`<hash>.v2.json` under `eitri/layout/` in the state directory (see `XDG_STATE_HOME` below). The file from before
containers, `<hash>.json`, is read once when there is no `.v2.json` yet and then left alone, byte for byte, so an
older Eitri on the same project keeps using its own file. A `<hash>.v2.json.partial` beside it holds the original
of a file this version could only partly use (an unknown kind of container read as a row, a module it cannot place,
a key it did not understand), copied before the first save so nothing is lost; a `<hash>.v2.json.unusable` is a
file it could not use at all (not JSON, another project's, no workspace it could read), set aside before a new
one is written. Keys in the file that this version does not know are written back unchanged.

## How a launch starts

Two settings decide what a new window does with the tabs and the mode of its last session.

```lua
eitri.config.set("agent.restore", "offer")        -- "offer" (the default), "auto" or "off"
eitri.config.set("agent.default_mode", "auto")    -- "manual", "auto" (the default) or "bypass"
```

- **`agent.restore`** is what happens to the tabs the last window on this project had open. Eitri keeps the
  tabs that have a session to resume (their order, names, modes, agents and which one was on screen) as it goes, and never
  records "no tabs" because you closed the window. With `"offer"` the empty tab's dashboard shows a
  `Restore last session` line, `s`, while nothing in the window has started; `"auto"` brings the tabs back at
  launch with no key pressed (unless the project's Claude configuration is new or changed, in which case the
  restore waits for your answer to the [trust question](permissions.md#trusting-a-project), and `Esc` on it keeps the saved tabs: they come back as failed tabs you can start again with `r`, or, from the empty tab's `Restore last session` line while the tab is still empty, stay on offer); `"off"` neither offers nor remembers. Each tab is resumed (nothing is sent until
  you type), the one on screen last time is on screen again, and one message says how many came back. A tab
  is skipped, and named, when its saved record is gone or another window holds its session. A tab that was in
  bypass is never put back in bypass without a yes: `s` asks first, and `n` (or `"auto"`) brings it back in
  auto. A tab that was in manual comes back in manual, with no question.
- **`agent.default_mode`** is the mode a new tab starts in, for a project where you have not changed an empty
  tab's mode with `Shift+Tab` (that choice, manual or auto, is remembered per project and keeps winning, and the
  launch log says so when it does; bypass is never remembered, and neither is the auto a tab of an agent with no
  manual mode lands on when it leaves bypass). `"manual"` starts every tab in the strictest mode, where every
  file edit is a card; a tab of an agent that has no manual mode starts in auto instead, and Eitri says so once,
  naming the tab. Setting it to
  `"bypass"` is the one way a window starts in bypass without asking, because you have said so in your own file;
  it also lets saved bypass tabs come back in bypass without the question. What the three modes mean is in
  [Permissions](permissions.md).

## Accounts

If you use more than one Claude account, name the one a window should use. Two ways:

- `eitri --account NAME` on the command line (it also works with `eitri panel` and `eitri split`). The launcher
  prints which account it chose and where the name came from.
- `eitri.config.set("agent.account", "NAME")` in `init.lua`, as the default for launches that name none, such as
  one from the application menu.

When both are present the command line wins. A `VERDANDI_CLAUDE_ACCOUNT` already set in your environment also
beats `init.lua`: `--account` sets that same variable for the launch, so a flag beats an inherited value too.

The name is a single word of letters, digits, `.`, `_` and `-`, starting with a letter or digit. Eitri uses the
Claude Code configuration directory `~/.claude-NAME` for it; set `VERDANDI_CLAUDE_CONFIG_DIR` to an absolute
path to use a different directory (the name is then only a label). A name that is malformed, or whose directory
does not exist, stops the launch with a message naming where the name came from, rather than starting under
whichever account happened to launch the window. An empty value counts as no account.

The account decides which login the sessions use and where their history is read from, so a resumed
conversation looks in the account's own history.

## Environment variables

These are the variables a user may set. Eitri sets a number of others for its own child processes; those are
not settings and are not listed.

| variable | effect |
|---|---|
| `EITRI_NVIM` | The absolute path of the `nvim` to run. It wins over everything below. A value that is not an absolute path to an existing file stops the launch. |
| `NEOVIM_BIN` | Read when `EITRI_NVIM` is not set: the same rule, an absolute path to an existing file. Without either, Eitri uses the `nvim` on `PATH`; if that is missing or older than 0.10 and you accepted a private copy from `eitri setup`, it uses the newest copy under `$XDG_DATA_HOME/eitri/nvim/`. |
| `EITRI_CODEX` | The `codex` a Codex tab runs: an absolute path to an existing file, else Eitri stops at startup and names the variable. It wins over `agent.codex.command` and `PATH`. Empty counts as unset. Eitri sets no `CODEX_HOME` for a Codex tab; Codex reads yours, and a relative `CODEX_HOME` makes every Codex start refuse. |
| `EITRI_NEOVIDE` | The Neovide that `eitri split` starts. It must name an existing file; unset, `neovide` on `PATH` is used. |
| `EITRI_PROJECT_DIR` | The project to open when no directory is given on the command line. The command-line directory wins over it, and it wins over the current directory. The installed `eitri` command does not read it: it opens the current directory when you name none. |
| `EITRI_CONFIG_DIR` | The directory `init.lua` and its `lua/` modules are read from, instead of `~/.config/eitri`. |
| `EITRI_AGENT_TRACE` | Set to `1` to print one `[turn-trace]` line per turn on stderr, with the turn's timings. |
| `EITRI_SUPERVISOR` | Set to `1`, `true` or `yes` to let a window start `eitri-supervisor`, a dashboard of the agent status of every open window, when none is running. Unset, a window only connects to one you started yourself. |
| `EITRI_SIDECAR_BINARY` | The path of a sidecar program to run agent sessions with, instead of the one `eitri setup` built or the package installed. A path that is not a file is an error when a session starts. |
| `EITRI_EDITOR_DMABUF` | How the editor hands its drawing to GTK. `0`, `off`, `false` or `no` use GTK's own texture path; `1`, `on`, `true` or `yes` force Eitri's own buffers even on a GTK older than 4.16; anything else, or unset, uses Eitri's own buffers from GTK 4.16 on. |
| `XDG_STATE_HOME` | Where Eitri keeps what it remembers: per-project layout, open tabs, prompt history, permission rules, trust answers and turn-review snapshots, under `eitri/` in this directory. Unset, empty or a relative path means `~/.local/state`. Nothing is written into your project. |
| `VERDANDI_CLAUDE_ACCOUNT`, `VERDANDI_CLAUDE_CONFIG_DIR` | The account and its configuration directory; see [Accounts](#accounts). |
