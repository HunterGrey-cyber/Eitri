English | [简体中文](configuration.zh-CN.md)

# Configuration

Eitri has one configuration file, a Lua script that runs once at startup, and a handful of environment
variables for the cases where a setting belongs to a shell or a launcher rather than to you. This page lists
every setting, what it does and what it defaults to. Where a setting changes keys, the page says so and points
to [Keys](keys.md).

## init.lua

On Linux, Eitri reads `$XDG_CONFIG_HOME/eitri/init.lua` when `XDG_CONFIG_HOME` is non-empty and
absolute, otherwise `~/.config/eitri/init.lua`. The Mac uses `~/.config/eitri/init.lua`.
Set `EITRI_CONFIG_DIR` to an absolute directory to read `init.lua` from there instead.
An empty override counts as unset; a relative override refuses to start. There is no file to create first:
without one, Eitri starts with its built-in defaults and prints a line saying so. The first time you start Eitri
without one, it also offers to write a short file for you: see [First start](#first-start).

Settings are made with `eitri.config.set(key, value)`. A value is a string, a number or a boolean (`true` or
`false`); `nil` unsets the key again. A value Eitri cannot keep (a table or a function, say) is refused: the
call does nothing, and Eitri lists it when it starts.

```lua
eitri.config.set("agent.restore", "auto")
eitri.config.set("review.hint", true)
```

Three rules hold for every setting below:

- A value a setting does not accept never stops the window and never falls back quietly. The setting goes back to its
  default, and Eitri says so when it opens: the file and line, the key and what it accepts. There are two exceptions.
  `agent.account` still stops the start, because a window on some other account would write its sessions there. And
  the settings that decide what an agent may do or what is loaded go to the stricter side, not to the default: a bad
  `agent.default_mode` starts new tabs in Manual, a bad `agent.user_settings` leaves your own Claude Code
  configuration unloaded, and a bad `agent.editor.tools` (or a bad `agent.editor.tool.<name>`) turns those tools off
  (a tool name that does not exist turns all of them off). Eitri says which side is in force, and it stays until you
  fix the line. A value `eitri.config.set` could not store for one of those is treated the same way.
- A key that Eitri does not read does nothing. If it is in one of Eitri's families (`agent.`, `window.`, `terminal.`,
  `editor.`, `keymap.`, `review.`, `companion.`, `plugins.`, `modules.`) or is one typo away from a real key, Eitri
  lists it, with the key it probably means. A key of your own, read by your own Lua with `eitri.config.get`, is left
  alone.
- A Lua error in the file itself (a typo in the code, say) does not stop the launch either: Eitri opens with
  whatever the file had set before the error, and says the file and line in the window (a syntax error stops the
  whole file, since Lua reads it before it runs any of it). The same list goes to the terminal you started Eitri
  from (to the journal, for a launch from the desktop).

What Eitri says is a toast at the top right: how many problems there are and the first five, up for 40 seconds, with a pointer to the terminal when there are more. It
says it every start while the problem is in the file.

`require` finds modules only under the `lua/` directory next to `init.lua`: `require("mine")` loads
`lua/mine.lua` or `lua/mine/init.lua` in that configuration directory, and `require("a.b")` loads
`lua/a/b.lua` or `lua/a/b/init.lua`. It never looks in the project you opened or in the current directory, and
changes to `package.path` or `package.cpath` have no effect.

Both windows read the same file: the one-window mode and the [companion window](companion.md).

## Settings

| key | value | default | what it does |
|---|---|---|---|
| `agent.account` | an account name | unset | The Claude account the window uses. See [Accounts](#accounts). |
| `agent.font_size` | a number from 9 to 32 | the editor's size | The agent panel's base text size, in pixels. The conversation is drawn at 12/14 of the base (a reply, tool lines and code), so it reads a little smaller than the editor's text; the status line at the bottom of the panel is drawn at the base itself, so it lines up with the editor's statusline. Unset, the base is the editor's own size (`guifont`). |
| `agent.typing_cadence_hz` | a whole number from 1 to 60, or `false` / `"off"` | `5` | While you type in the editor, how many times a second the agent panel is allowed to update. A steady slower rate keeps the panel from redrawing under your typing; `"off"` lets it update at full speed. |
| `agent.default` | `"claude"`, `"codex"` or the name of an agent registered with `eitri.agent.register` | `"claude"` | The agent a new tab runs, and the first tab of a fresh window. See [Codex and other agents](agents.md). |
| `agent.codex.command` | an absolute path, or a bare command name | unset | The `codex` a Codex tab runs. `EITRI_CODEX` outranks it; without either, `codex` on `PATH`. A relative path with a directory part is refused and the setting stays unset. |
| `agent.default_mode` | `"manual"`, `"auto"` or `"bypass"` | `"auto"` | The mode a new tab starts in. See [How a launch starts](#how-a-launch-starts). |
| `agent.restore` | `"offer"`, `"auto"`, `"off"` or `false` | `"offer"` | What happens to the tabs the last window on this project had open. See [How a launch starts](#how-a-launch-starts). |
| `agent.thinking` | `"collapsed"`, `"preview"`, `"expanded"` or `"hidden"` | `"collapsed"` | How the agent panel shows the agent's thinking. `"collapsed"` draws one quiet row per thought, opened with `Enter` like a tool's output; `"preview"` also shows the newest lines of a thought while it is being written, in a box that does not grow; `"expanded"` shows every thought in full; `"hidden"` shows none. `Ctrl+o` shows every thought in full whatever this says. |
| `agent.user_settings` | `true` / `"on"` or `false` / `"off"` | `true` | Whether agent sessions load your own Claude Code configuration (your hooks, plugins, skills, `CLAUDE.md` and permission rules), as `claude` does in a terminal. `false` leaves your own configuration out; a project's own configuration is governed by [trust](permissions.md), not by this key. |
| `agent.editor.tools` | `true` / `"on"` or `false` / `"off"` | `true` | Whether an agent gets the [editor tools](editor-tools.md) in any tab. `false` declares none of them; a session already running keeps what it began with. |
| `agent.editor.tool.<name>` | `true` / `"on"` or `false` / `"off"` | `true`, except `false` for `nvim_exec` | One of those tools by name: `open_file`, `read_buffer`, `editor_state`, `diagnostics`, `usages`, `hover`, `symbols`, `lsp_rename`, `format`, `lsp_code_actions`, `apply_code_action`, `apply_proposal` or `nvim_exec`. Every tool is on unless you turn it off, except [`nvim_exec`](editor-tools.md#nvim-exec), which is off unless you turn it on. A name Eitri does not know is listed at startup, naming the key, and that switch is left at its default. |
| `agent.editor.follow` | `true` / `"on"` or `false` / `"off"` | `false` | Whether a new tab follows the agent's completed edits in the editor ([editor tools](editor-tools.md#following)). Each tab's own choice (`Space e`) overrides it from then on. A bad value is listed at startup, naming the key, and the setting stays off. |
| `editor.restore` | `true` / `"on"` or `false` / `"off"` | `true` | Whether the editor reopens the files, splits, tab pages and cursor positions it had when the last window on this project closed or crashed. `false` reads and writes nothing and leaves the saved file as it is, so turning it on again picks up where it was. It is read after `init.lua` has run. What comes back, and which session plugins Eitri stands aside for: [When you start again](getting-started.md#when-you-start-again). |
| `modules.chat.on_permission` | `"badge"` or `"reveal"` | `"badge"` | What happens when a permission card arrives while the agent panel is hidden. `"badge"` shows `agent ⚑N` in the top bar and a toast; `"reveal"` brings the panel back where it was, without moving the keys. While another pane is zoomed `"reveal"` shows the badge instead, because showing the panel would not put it on screen. A companion window never hides its panel, so the key does nothing there. |
| `review.enabled` | `true` / `"on"` or `false` / `"off"` | `true` | Whether [turn review](turn-review.md) runs at all. `false` takes no snapshots and offers no review. |
| `review.hint` | `true` / `"on"` or `false` / `"off"` | `false` | Whether a finished turn that edited files adds a one-line hint to the status band, and a running turn's edits are counted on a line above the input box. The review is one key away either way. |
| `terminal.font` | a font in `guifont`'s form, `"<families>:h<points>"` | your `guifont` | The terminal's font, for every terminal of the window: `"Iosevka:h12"` sets both, `"Iosevka"` only the family (the size still follows your `guifont`), `":h12"` only the size. Families are comma-separated and tried in order, `_` or `\ ` stand for a space, as in `guifont`. Unset, the terminal draws in the editor's families and size, so a terminal under the editor looks like it; an empty `guifont` draws in `monospace`. The size is zoomed with the editor's, see [Keys](keys.md). |
| `terminal.claude_ide` | `true` / `"on"` or `false` / `"off"` | `false` | Whether a `claude` you run in the bottom terminal can attach to the window's editor: Eitri then listens on `127.0.0.1`, writes `~/.claude/ide/<port>.lock` while it runs and sets `CLAUDE_CODE_SSE_PORT` in its terminals' shells. Read once by the window that holds the editor; the second window follows it. See [The `claude` in your terminal](terminal-claude.md). |
| `keymap.from_tmux` | `true` / `"on"` or `false` / `"off"` | `"on"` | Whether Eitri reads your tmux prefix and prefix-table keys from your tmux configuration. See [Keys](#keys). |
| `keymap.which_key_delay` | a whole number from 0 to 5000, or `false` / `"off"` | `"off"` | Milliseconds the prefix waits for a second key before a panel lists the keys that may follow; `0` shows it at once. See [The hint panel](keys.md#the-hint-panel). Not drawn in a companion window or on the Mac. |
| `keymap.nvim_prefix` | a Neovim left-hand side of at most 16 characters (`"<leader>W"`), or `false` / `"off"` | `"off"` | Maps the layout keys of your prefix table in Neovim's Normal mode under this left-hand side, each running its `:Eitri` action. See [From Neovim: `:Eitri`](keys.md#from-neovim-eitri). |
| `window.restore` | `true` / `"on"` or `false` / `"off"` | `true` | Whether the window that holds a pair brings window 2 back at the next start when it was open when the pair last ended, and whether window 1 of a compositor group brings back its windows, their outputs and its bottom terminal. With `false`, window 1 takes back what window 2 held and offers its saved tabs. See [Two windows](two-windows.md) and [Compositor windows](compositor-windows.md#coming-back). |
| `window.desktop_start` | `"choose"`, `"last"` or `"cwd"` | `"choose"` | What starting Eitri from the desktop with no file or folder does. `"choose"` shows the [list of your projects](getting-started.md#open-a-project) in a small window first; `"last"` opens the project you used most recently that is still there, with no window first, and shows the list when there is none; `"cwd"` opens the directory the launcher ran in, as Eitri did before the list. Any other value is listed at startup and the setting stays `"choose"`. It is read only by that desktop start: `eitri` run in a terminal, the `<prefix> O` list and a launch with a path never look at it. |
| `window.tiling` | `"auto"`, `"eitri"` or `"compositor"` | `"auto"` | Who arranges chats and pages. `"auto"` makes them windows of your window manager on Hyprland, sway or niri when the compositor answers, and keeps them inside Eitri's window anywhere else; `"eitri"` keeps them inside; `"compositor"` makes them windows on any Wayland desktop. Any other value is listed at startup and the setting stays `"auto"`. See [Compositor windows](compositor-windows.md). |
| `companion.wm` | `"auto"`, `"hyprland"`, `"sway"`, `"niri"`, `"gnome"` or `"none"` | `"auto"` | Which window-manager adapter moves focus between the [companion window](companion.md) and your editor. `"auto"` detects it from the session; `"none"` never asks the window manager for anything. |

The settings are read once, when the window starts. Change `init.lua` and open a new window to apply a change.

## Registering another agent

`eitri.agent.register` is not a setting: it takes a table and adds an agent that speaks ACP over standard input
and output, so that `:tabnew NAME` and `agent.default` can use it. Its keys, what the trust question does for it
and what Eitri can and cannot do for such an agent are on [Codex and other agents](agents.md#other-agents-over-acp).
A bad registration is refused and listed when Eitri starts; the agent is not registered.

## Keys

Keys are set with `eitri.keymap` in the same file:

```lua
local k = eitri.keymap
k.prefix("C-a")                               -- the prefix key
k.set("prefix", "g", "zoom")                  -- bind g (free by default) after the prefix to an action
k.del("prefix", "%")                          -- remove a default binding
k.set("root", "M-z", "zoom")                  -- a key that works without the prefix
```

`k.prefix(key)` changes the prefix, `k.set(table, key, action, options)` binds a key and `k.del(table, key)`
removes one. The `"root"` table holds keys that work without the prefix: it starts empty, takes only Ctrl and Alt
chords, and [Keys](keys.md#keys-without-the-prefix) says what it runs, what it refuses and when Eitri warns. A bad key, an unknown action or a binding that collides with a key Eitri keeps is left out and listed at startup with a message
naming the call, so a typo is never silent. [Keys](keys.md) lists the
defaults and the actions.

The panel's `"panel"` table uses `model.pick` and `effort.pick` as the action names of `<leader>M` and
`<leader>E`. Delete a default binding before rebinding it; for example, to move the model picker to `<leader>p`:

```lua
eitri.keymap.del("panel", "<leader>M")
eitri.keymap.set("panel", "<leader>p", "model.pick")
```

The defaults are stock tmux's, with `Ctrl+b` as the prefix. Eitri then reads your tmux configuration (the same
files tmux reads, and what they `source-file` by an absolute path; a relative path is skipped, because Eitri
does not know the directory tmux would resolve it against), and finally applies your `eitri.keymap` calls, so your own
calls win. Eitri only reads tmux's files; it never starts or asks tmux. Set `keymap.from_tmux` to `"off"` to
skip the tmux step.

A complete example, which moves the prefix to `Ctrl+a` and rebinds splits, resizing and zoom, is
[`docs/keymap/tmux-ctrl-a.lua`](../keymap/tmux-ctrl-a.lua). Paste what you want from it into your `init.lua`.

`k.use(name)` records a named set of these calls in one line. The only name is `"i3-alt"`, for i3 or sway with
Alt as `$mod`: it moves the Alt chords of the prefix table to `Ctrl+Alt`, exactly the calls in
[`docs/keymap/i3-alt-mod.lua`](../keymap/i3-alt-mod.lua). It is an ordinary call, so your own calls after it win,
and an unknown name is refused like any other bad call: it is listed at startup and does nothing.

```lua
eitri.keymap.use("i3-alt")
```

## First start

The first time you start Eitri without an `init.lua`, window 1 of a full launch opens a setup screen over the
window. It asks how the window's keys should work, writes a short `init.lua` if you say so, and then offers
[`:EitriTutor`](keys.md). Nothing is written until the last step, and what it writes is yours: it is an ordinary
`init.lua`, with comments, that you can read and edit like any other.

A launch that names a file (`eitri FILE`) leaves the screen closed, since you came for that file, and counts for
nothing: the next plain `eitri` opens it, and `:Eitri setup` opens it at any time.

**What it looks at.** It reads, and never changes: the tmux configuration Eitri already read for
[`keymap.from_tmux`](#settings); the desktop you are on and, under i3 or sway, whether `$mod` is Alt or Super (from
the window manager's own answer, else its config file); under Hyprland, its binds; whether Hyprland, sway or niri answers its version command, and then a `windows` row says
what [`window.tiling`](compositor-windows.md) `"auto"` would do here; which plugin in your Neovim owns, or probably owns, `Ctrl+b` and `Ctrl+h/j/k/l`; and whether an input method is running (by its process; Eitri
never reads an input method's own settings). Rows appear as the answers come in, and the screen does not wait for
a slow one. None of that text is copied into the file.

**What you choose.** `j`/`k` or a number move among the presets, `Enter` goes on:

| preset | what it writes |
|---|---|
| Neovim + tmux | `keymap.from_tmux` on, so the keys stay your tmux configuration's; the hint panel as a commented line (the default when a tmux configuration exists) |
| Neovim, no tmux | `keymap.from_tmux` off, the stock `Ctrl+b` prefix, and the [hint panel](keys.md#the-hint-panel) on; when Neovim reported a leader, the window keys under it (`keymap.nvim_prefix`) |
| Another prefix | `Ctrl+a`, `Ctrl+Space`, or a key you press: `k.prefix`, `k.del` for the old `Ctrl+b` and `k.set("prefix", key, "send-prefix")`, as [Keys](keys.md#choosing-ctrlspace-as-the-prefix) shows |
| i3 / sway style | the stock keys, and `eitri.keymap.use("i3-alt")` when your `$mod` is Alt; nothing extra when it is Super; `window.tiling` `"eitri"`, so Eitri arranges its own modules |
| Tiling compositor | `window.tiling` `"compositor"`: [your window manager arranges chats and pages](compositor-windows.md) as its own windows, and Eitri's layout keys are off. Listed only when Hyprland, sway or niri answered, and then it is the default |

The review step shows the lines before they are written; `p` changes the prefix and `Backspace` goes back. On
Hyprland, sway or niri the first three presets also get `w`, which writes `window.tiling` as `"compositor"` or
`"eitri"`; the line beside it says which (`Windows: sway arranges`). A prefix other than the one the window runs with is checked with a **press test** first: the screen asks you to press
it once and says whether it arrived. A key an input method or the desktop takes never reaches Eitri, and the screen
says so, then lets you choose another prefix or write it anyway; `Esc` goes back while it waits. A key the keymap
refuses (`Ctrl+h`, a bare letter) is refused at once, with the reason.

In a window too low for the screen, `PageUp` and `PageDown` scroll it, and on the review `j`/`k` and the arrows do
too. On the first page `r` reads Neovim's mappings again, for a buffer's own mappings or a plugin that loaded late.
`?` shows a line of help.

**It writes once and never over a file.** The file is made under a temporary name and linked into place, so a
reader never sees half of it, and only if no `init.lua` exists at that moment: if one appeared meanwhile (another
window), nothing is written and the screen says so. Nothing in Eitri opens an existing `init.lua` for writing.
The new keys apply the next time Eitri starts; the window you are in keeps the keys it has.

**When it opens.** All of these hold: this is window 1 of a full launch (not a second window, a companion window, a
plugin's own window or `eitri split`), `--clean` was not given, `EITRI_CONFIG_DIR` and `EITRI_FIRST_START` are
not set (see below), there is no `init.lua` (a dangling link, a directory or anything else in its place counts as
one), the configuration directory passes the check below, and you have not said no (next paragraph). If the agent
panel is asking whether to trust a project, the screen waits until you have answered. The Mac does not show it.

**Esc twice, or `d`.** `Esc` is "not now": nothing is written, and the screen comes back once, at the next start.
A second `Esc` stops it from opening by itself. `d` ("don't ask again") stops it at once. What you said is kept in
`first-start.json` under `eitri/` in the [state directory](#environment-variables) (`XDG_STATE_HOME`), never in
the configuration directory; deleting that file brings the screen back. Each start where it would have opened logs
one line saying so.

**Opening it again.** `:Eitri setup` in the window's Neovim, or the action `window.setup` bound to a key (it has
no default key, and the `prefix ?` sheet lists it), opens the screen whatever the conditions above say. With an
`init.lua` already there it only
shows the lines, marked as not written; `o` opens them in a scratch buffer so you can copy what you want. The
same `o` is offered if the lines could not be written.

**Why a group-writable directory is refused.** `init.lua` is code that runs every time Eitri starts. The screen
therefore creates it only in a directory that belongs to you and that no one else can write (no group or other
write bit), inside a parent that no one else can rename entries in; otherwise another user could swap the file or
the directory. A configuration directory that is a symbolic link (stow and chezmoi make them) is followed, one
link deep. If the check fails the screen stays closed and the log names the directory; `chmod go-w` on it, or
`:Eitri setup` to read the lines and place them yourself.

**`EITRI_FIRST_START`.** `off` keeps the screen from opening by itself (it does not stop `:Eitri setup`); any
other non-empty value is ignored with a log line. Test setups and scripted launches set it to `off`. The screen
is also closed whenever `EITRI_CONFIG_DIR` names a directory, because that is a deliberate setup.

The screen and the comments in the file it writes follow your locale: Simplified Chinese when `LC_ALL`,
`LC_MESSAGES` or `LANG` (the first one set) starts with `zh`, English otherwise. The code lines are the same in both.

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
  new terminal to the right of the module that has the keys, as `<prefix> Alt+Enter` does (`kind = "agent"` makes a
  [chat view](keys.md#chat-views) holding a new empty tab instead), and a split action takes `cwd`, below.
- **`eitri.layout.mark(id)`** marks that module without moving the keys, as `<prefix> m` marks the one that has them.
- **`eitri.layout.get()`** returns a new table describing the layout as it is now, in the shape
  `eitri.layout.default` takes: `{ "row", { "editor", 0.6 }, { "column", { "agent" }, { "terminal" } } }`, where
  every child but the last carries its share of the container. It adds `focus` (the module with the keys), `hidden` (a list),
  `zoomed` and `marked` (a module, or nothing), `floating` (the floats, bottom first, only when there are any; a
  scratch float carries `scratch = true`), `workspace` (the name of the workspace you are in) and `workspaces` (every
  name, in order). Pinned bottom rows are not written into the table; the default shape
  restores a bottom row's height by itself. Called from inside a layout change it stops with a message instead of
  reading half a change.

- **`eitri.layout.export()`** returns the window's layout as the `init.lua` calls that make it at a first launch:
  `eitri.layout.default(...)` for `main`, then `eitri.layout.workspace(name, ...)` for every other workspace, in the
  shape below, floats included. What that shape has no field for is not written: which module has the keys, the
  zoom, the mark, a pinned bottom row's height. A hidden module stays where it is in the tree, shown at a first
  launch, and a comment names it. Numbered chat views are left out, with a comment saying so, since a view named in
  `init.lua` is removed at startup unless a saved tab holds it (an [open rule](#where-a-new-module-opens) says where
  one opens instead). Like `get()`, it works only once the window exists. Bound to a key of your own, it writes the
  layout to a file you can paste from:

  ```lua
  eitri.command.register({
    id = "layout-export",
    title = "Write the layout as init.lua calls",
    keybinding = "<Control><Alt>e",
    action = function()
      local file = assert(io.open(os.getenv("HOME") .. "/eitri-layout.lua", "w"))
      file:write(eitri.layout.export())
      file:close()
    end,
  })
  ```

- **`eitri.layout.float(id, rect?)`** floats that module: at `rect` (`{ cx = 0.5, cy = 0.4, w = 600, h = 300 }`: the
  centre as a fraction of the window, the size in pixels, the bar included), else at the rectangle it last floated
  at, else centred at 60% of the window. On a module that already floats it moves it to `rect`. The editor never
  floats. Like `<prefix> Alt+Shift+Space`, it refuses the last tiled module in view.
- **`eitri.layout.rule(target, opts)`**, in `init.lua` only, says where a module opens when the window opens it by
  itself; it is described under [Where a new module opens](#where-a-new-module-opens).
- **`eitri.layout.open(kind, opts?)`** opens a new instance of a kind, `"terminal"` or `"agent"` (a new chat view
  holding a new empty tab), as `eitri.layout.run("instance.new", { kind = ... })` does, but with the choice of where
  it goes in your hands. Without `opts` your [open rules](#where-a-new-module-opens) decide, and without a rule it
  opens to the right of the module that has the keys. With `opts`, which take the same keys and values as a rule's, they decide this one
  opening and nothing else. Like the other runtime calls it is refused while `init.lua` is loading, and it stops with
  a message for any other kind; a refusal at run time (no free number: `at most 9 terminals`, `at most 9 chat views`)
  is logged and shown as it is for the key.
- **`eitri.layout.workspace(name, tree)`**, in `init.lua` only, defines a workspace in the shape `default` takes. A
  window with no saved workspace of that name gets it, after the saved ones in the order the calls come; `main` is
  `eitri.layout.default`'s and is refused here. A workspace you define this way **comes back from its definition**
  at the next launch if you delete or rename it in the window, so removing one for good means removing its call.
  Workspaces without a definition stay deleted or renamed.
- **`eitri.layout.switch(name)`** goes to that workspace, making it first when there is none: from its definition,
  else as a copy of the arrangement you are in. `eitri.layout.run("workspace.switch", { name = "study" })` is the
  same. `workspace.choose` and `workspace.rename` wait for a key or for typing, so `run` refuses them.

**The shape `eitri.layout.default` and `get()` use.** A table starts with `"row"` or `"column"` (two or more
children, side by side / one above the other), or with `"tabbed"` or `"stacked"` (one or more children, one shown
at a time, with a row of titles / a row of title for each). A child is a module name, then optionally its share of
the container as a second item (`{ "editor", 0.6 }`), or a table of its own, whose share goes in as `share = 0.4`.
A child without a share takes an equal part of what the others leave; every share is from 0.05 to 1 and together
they come to 1. `active = 2` on a tabbed or stacked table names the child shown (counting from 1; the first by
default). A numbered terminal (`terminal#2` to `terminal#9`) can be a child, as in the example below, and the window
gives it its own shell once it is shown; a numbered chat view (`agent#2` to `agent#9`) can be a child or a float
too, and the window gives it a page of its own. The page starts with no tab of its own: a view named here is kept only
when a saved tab is bound to it, and otherwise it is removed at startup once the restore settles, with the log line
`agent#2 holds no tab; removed`. To get a view with a new, empty tab, make it while the window runs
(`eitri.layout.open("agent")`, `eitri.layout.run("instance.new", { kind = "agent" })`, or `<prefix> !` on a tab) rather than
naming it here. A misspelled field is refused and listed at startup, naming it. The top table (not a container inside it) may also carry
`floating`, a list of `{ "module", cx = 0.5, cy = 0.5, w = 600, h = 400, scratch = true }`, bottom first: `cx` and
`cy` are from 0 to 1, `w` and `h` whole pixels from 1 to 16384, and `scratch` (false unless given) starts the
float put away. The editor stays in the tree; the agent panel may float; a module is in the tree or in the list,
never both. An example with every kind of container:

```lua
eitri.layout.default({
  "row",
  { "editor", 0.55 },
  { "tabbed", { "agent" }, { "terminal" }, { "terminal#2" }, active = 1 },
})

eitri.layout.workspace("study", { "row", { "editor", 0.6 }, { "agent" },
  floating = { { "terminal#2", cx = 0.5, cy = 0.5, w = 900, h = 420, scratch = true } } })
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

A workspace in the file may carry `floating` (its floats, bottom first, each `module`, `cx`, `cy`, `w`, `h` and
`scratch`) and `float_memory` (the last rectangle of each module that has floated, by module); both are written
only when they are not empty, so a layout without floats writes the same file as the version before floats. An
entry the version reading it cannot use is left out with a note, and the original is copied to
`<hash>.v2.json.partial`. **The version before floats** (the build that has containers and workspaces but no
floats) reads these two keys as ones it does not know. With **one workspace**, when the floats are numbered
terminals that no tiled tree holds (`terminal#2` and so on), it keeps them as they were, reserves their numbers and
writes them back, so this version finds them again; a scratch float that was put away comes back shown. When a float
names a module that version places itself -- the base `terminal`, the chat, any module's remembered rectangle, or,
with **two or more workspaces**, any floating terminal (that version keeps it hidden below the editor in the other
workspaces) -- it drops the floats, opens those modules tiled (the base terminal below the editor), and copies the
file to `.v2.json.partial` first. Float positions are all that is lost; no module is. This version then opens them
tiled, and the copy has the positions.

**Chat views in the file.** A chat view (`agent#2` to `agent#9`) appears in a workspace's tree or `floating` list by
its id, like a numbered terminal, and every workspace holds the views any workspace names. The saved tabs
(`eitri/tabs/<hash>.json` in the state directory, a file of their own) carry a `view` number for a tab shown in an extra view. **A
build from before chat views** leaves an `agent#N` out of the layout with a note and copies the file to
`<hash>.v2.json.partial` first, so the views are not lost; it ignores `view` in the saved tabs, so each of those tabs
comes back in the base view. This version too brings a tab back in the base view when the layout no longer holds its
view, or when a second tab names a view another tab already took.

### Where a new module opens

`eitri.layout.rule(target, opts)`, in `init.lua` only, says where the window puts a
module it opens by itself: a new terminal or chat view (`<prefix> Alt+Enter`, `instance.new`, `eitri.layout.open`), a tab broken out with `<prefix> !`, a module a launch places, and one that comes back
after you closed it.

```lua
eitri.layout.rule("terminal", { at = "below", size = 0.3 })            -- terminals open below the module with the keys
eitri.layout.rule("agent#2", { float = true, size = { 0.5, 0.6 } })     -- chat view 2: a float, half the window wide and 60% high
eitri.layout.rule("terminal#3", { scratchpad = true, focus = false })  -- terminal 3: put away in the scratchpad
```

- **The target** is a kind (`"terminal"` or `"agent"`, which covers every instance of it that has no rule of its
  own; for `"terminal"` that includes the base `terminal`, while `"agent"` reaches only the numbered chat views
  `agent#2` to `agent#9`, since the first chat view always opens at the side, or where a layout file puts it) or one
  module: `"terminal#2"`, `"agent#3"`, `"lua:notes"` for a Lua
  panel, `"editor"`. Only one rule per target; a target that names nothing Eitri knows is refused and listed at startup, naming the call.
- **The options** are `at` (a place, below), `float` (a boolean), `size`, `scratchpad` (a boolean: it floats and
  opens put away, and implies `float`) and `focus` (a boolean, `true` by default: whether the new module takes the
  keys; with `false` the keys stay where they were, unless the new module takes the place of the editor that had
  them). `at` is one of `side` (a new column to the right of the whole window), `bottom` (a new row below the whole
  window), `main` (in the editor's place, hiding the editor), `below-editor` (a row below the editor's column, a
  terminal's default place), `right`, `below`, `left`, `above` (beside the module that has the keys, taking half of its space) and `tab` (as a tab of the
  container the module with the keys is in, which is made tabbed with the two of them if it is split). `size` is the
  share of its container when the module is tiled (a number above 0 and below 1) and `{ w, h }` or `{ w = ..., h = ... }`,
  fractions of the window above 0 and at most 1, when it floats; a float with no size opens centred at 60% of the
  window on each axis.
- **What is refused,** listed at startup with a message that names the call, the rule not applied: an option that is not one of those, a value of the wrong type or out of range, a
  tiled size given as a table or a floating one as a number, `float` or `scratchpad` together with `at`, a
  floating editor (the editor never floats; any other rule for it is accepted and never applies, since nothing opens
  the editor), a target that is neither a kind nor a module id, and a second rule for one target. A refused rule is listed even if your Lua catches the error.
- **Which rule wins.** A rule for the module's own id, then the rule for its kind, then a Lua panel's own `position`,
  then the built-in default of where it opens: a new terminal or chat view, and a tab broken out, go to the right of the module
  that has the keys (the base view, for a broken-out tab); a module the window places at a launch or brings back
  after a close goes where it always did (the terminal below the editor, a Lua panel at its `position`). With no module
  holding the keys (a first launch, a layout file read at startup) `right`, `left` and `tab` become `side`, and
  `below` and `above` become `bottom`, with a note in the log for a rule that asked for them.
- **What a rule never decides.** Anything that says where itself always wins: a split key, a move, a drag,
  `eitri.layout.float`. A saved layout is never changed by a rule, and a layout from `eitri.layout.default` or
  `eitri.layout.workspace` places the modules it names (in a [plugin's own window](#plugins), the plugin's own
  layout comes first, and its page is put back at each start). A module that a saved layout or a template lacks,
  and that a rule would float, comes back tiled at its fallback place, hidden, with a note naming the place. A new
  module is added hidden to the other workspaces, at the place its rule or kind gives it.

## Plugins

A plugin is a page of its own in the window, as a module beside the others, given only the powers its own manifest
lists. Eitri ships one, the [Markdown preview](markdown-preview.md); you may also write or install your own
([Pages you build](pages.md) shows how, and how to add a plain HTML page as a Lua panel). Each is a
directory holding an `eitri-plugin.toml`, and nothing of it runs until `init.lua` loads it:

```lua
eitri.plugin.load("markdown")                          -- by name
eitri.plugin.load("~/src/my-plugin", { key = "y" })    -- by path
```

- **By name**, a word of lower-case letters, digits and `-`: Eitri looks in these places, in order, and loads the first
  directory that holds an `eitri-plugin.toml`:
  1. `$XDG_DATA_HOME/eitri/plugins/<name>/` (`~/.local/share/eitri/plugins/<name>/` when `XDG_DATA_HOME` is not set),
     your own plugins and your own copies of shipped ones;
  2. the copy installed with Eitri by the install script (`~/.local/lib/eitri/plugins/<name>/`);
  3. `<each folder of $XDG_DATA_DIRS>/eitri/plugins/<name>/` (`/usr/local/share` then `/usr/share` when it is not
     set), where the packages install theirs (`/usr/share/eitri/plugins/<name>/`).

  An empty directory is skipped with a line saying so. A directory whose `eitri-plugin.toml` has an error is still the
  plugin: Eitri stops with that error rather than loading another copy. The plugin's `id` must be the name you wrote.
- **By path**, anything else: an absolute path, or one starting with `~`. A relative path is refused, because it would
  mean a different directory in every project.
- Every load prints which directory it used and why, in the terminal you started Eitri from:
  `[plugin markdown] loaded /usr/share/eitri/plugins/markdown (system data directory)`. A name that is found nowhere
  stops Eitri at startup with a message listing every directory it looked in.
- The options are the plugin's own settings, plus `key` (a module key after the prefix) and `position` (`"side"`,
  `"bottom"` or `"main"`). A plugin opens where [open rules](#where-a-new-module-opens) say, as `lua:<id>`.
- A plugin may name a directory of its own in its manifest (`[window]`). Start Eitri in that directory and the window
  is the plugin's: the top bar and the window title show the plugin's title instead of the project's name. With no
  saved layout, the plugin's layout is used before your `eitri.layout.default`, and the editor or chat it leaves out
  start hidden (their keys bring them back). A saved layout still wins, with one exception: if it lacks the plugin's
  page, or has it hidden, the page is put back at its place at every start. To get the plugin's whole layout back,
  delete that directory's saved layout file ([where the layout is kept](#layout-from-lua)).
- Loading a plugin trusts it as you trust any program you run: it is your own local directory. Load only plugins you
  trust. Plugins are not available in the companion window.

### Keys in a plugin page

A plugin page that reads keys (the [Markdown preview](markdown-preview.md#reading-with-the-keyboard) does) uses Vim's
and Vimium's: `j`/`k` scroll, `/` finds, `f` labels what can be followed, `?` lists the page's keys. Six settings
shape them in every plugin page at once:

| key | value | default | what it does |
|---|---|---|---|
| `plugins.keys.scroll_step` | a whole number from 10 to 400 | `60` | How far one `j` or `k` scrolls, in pixels. |
| `plugins.keys.smooth` | `true` / `"on"` or `false` / `"off"` | `true` | Whether scrolling glides. `false` makes every scroll key jump, as does the desktop's reduced-motion setting. |
| `plugins.keys.prefix_wait` | `true` / `"on"` or `false` / `"off"` | `true` | Whether a key that only begins longer keys (the `g` of `gg`) waits for the next key as long as it takes, as Vim's own `g` does. `false` gives up after Neovim's `timeoutlen`. |
| `plugins.keys.which_key_delay` | a whole number from 0 to 5000, `false` or `"off"` | `200` | How many milliseconds after such a key the page's status line lists the keys that may follow. `false` never lists them. |
| `plugins.keys.hint_chars` | 2 to 26 different lowercase letters | `"sadfjklewcmpgh"` | The letters the labels of `f` are made of. |
| `plugins.keys.find_case` | `"smart"`, `"ignore"` or `"match"` | `"smart"` | How `/` treats case: `"smart"` ignores it unless the search has a capital letter, `"ignore"` always ignores it, `"match"` never does. |

```lua
eitri.config.set("plugins.keys.scroll_step", 80)
eitri.config.set("plugins.keys.find_case", "ignore")
```

They are read once, when the window starts; a value outside these is refused and listed at startup, naming the
key. A page's own keys are changed in its `plugin:<id>` table with `eitri.keymap.set`.

## How a launch starts

Two settings decide what a new window does with the tabs and the mode of its last session.

```lua
eitri.config.set("agent.restore", "offer")        -- "offer" (the default), "auto", "off" or false
eitri.config.set("agent.default_mode", "auto")    -- "manual", "auto" (the default) or "bypass"
```

- **`agent.restore`** is what happens to the tabs the last window on this project had open. Eitri keeps the
  tabs that have a session to resume (their order, names, modes, agents and which one was on screen) as it goes, and never
  records "no tabs" because you closed the window. With `"offer"` the empty tab's dashboard shows a
  `Restore last session` line, `s`, while nothing in the window has started (the keys start in the editor, where `s` is nvim's, so `<prefix> w` also opens the session list on a `Restore last session` row); `"auto"` brings the tabs back at
  launch with no key pressed (unless the project's Claude configuration is new or changed, in which case the
  restore waits for your answer to the [trust question](permissions.md#trusting-a-project), and `Esc` on it keeps the saved tabs: they come back as failed tabs you can start again with `r`, or, from the empty tab's `Restore last session` line while the tab is still empty, stay on offer); `"off"` or `false` neither offers nor remembers; `true` and `"on"` are refused. Each tab is resumed (nothing is sent until
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

## Files and windows

`eitri FILE` opens a file in the editor and takes its project from it (see
[Open a file](getting-started.md#open-a-file)). Two things about it are decided on the command line, and there is no
`init.lua` setting for them:

- When a window of the file's project is already open, `eitri FILE` hands the file to that window, raises it and
  returns. `eitri --new-window FILE` starts a new window instead. `--clean` and `--account NAME` also start a new
  window, since reusing a window would drop what they ask for. `eitri DIR` and a bare `eitri` always start one.
- A command typed in Eitri's own bottom terminal reaches its own window: the terminal's shell has `EITRI_OPEN_SOCKET`
  set, and `eitri` reads it to find that window. You do not set this variable yourself.

## Accounts

If you use more than one Claude account, name the one a window should use. Two ways:

- `eitri --account NAME` on the command line (it also works with `eitri panel` and `eitri split`). The launcher
  prints which account it chose and where the name came from. With a file it always starts a new window.
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
conversation looks in the account's own history. A window has one account: every [chat view](keys.md#chat-views)
spends it, and a tab shown in a view of its own does not start a session under another.

## Environment variables

These are the variables a user may set. Eitri sets a number of others for its own child processes; those are
not settings and are not listed.

| variable | effect |
|---|---|
| `EITRI_NVIM` | The absolute path of the `nvim` to run. It wins over everything below. A value that is not an absolute path to an existing file stops the launch. |
| `NEOVIM_BIN` | Read when `EITRI_NVIM` is not set: the same rule, an absolute path to an existing file. Without either, Eitri uses the `nvim` on `PATH`; if that is missing or older than 0.10 and you accepted a private copy from `eitri setup`, it uses the newest copy under `$XDG_DATA_HOME/eitri/nvim/`. |
| `EITRI_CODEX` | The `codex` a Codex tab runs: an absolute path to an existing file, else Eitri stops at startup and names the variable. It wins over `agent.codex.command` and `PATH`. Empty counts as unset. Eitri sets no `CODEX_HOME` for a Codex tab; Codex reads yours, and a relative `CODEX_HOME` makes every Codex start refuse. |
| `EITRI_NEOVIDE` | The Neovide that `eitri split` starts. It must name an existing file; unset, `neovide` on `PATH` is used. |
| `EITRI_PROJECT_DIR` | The project to open when no directory is given on the command line. The command-line directory wins over it, and it wins over the current directory. The installed `eitri` command does not read it: it opens the current directory when you name none. It names a directory only; a file is opened with `eitri FILE`. |
| `EITRI_CONFIG_DIR` | The directory `init.lua` and its `lua/` modules are read from, overriding the Linux XDG directory or `~/.config/eitri`. Empty is unset; a relative value refuses to start. |
| `EITRI_FIRST_START` | `off` stops the [first-start screen](#first-start) from opening by itself. Any other non-empty value is ignored with a log line. |
| `EITRI_AGENT_TRACE` | Set to `1` to print one `[turn-trace]` line per turn on stderr, with the turn's timings. |
| `EITRI_WINDOW_LOG` | Set to `off` for no crash record and no window log. See [When Eitri itself crashes](getting-started.md#when-eitri-itself-crashes). |
| `EITRI_SUPERVISOR` | Set to `1`, `true` or `yes` to let a window start `eitri-supervisor`, a dashboard of the agent status of every open window, when none is running. Unset, a window only connects to one you started yourself. |
| `EITRI_SIDECAR_BINARY` | The path of a sidecar program to run agent sessions with, instead of the one `eitri setup` built or the package installed, and instead of any [sidecar update](../../INSTALL.md#updating-the-sidecar): it wins over an update and is never replaced by the bundled one. A path that is not a file is an error when a session starts. |
| `EITRI_EDITOR_DMABUF` | How the editor hands its drawing to GTK. `0`, `off`, `false` or `no` use GTK's own texture path; `1`, `on`, `true` or `yes` force Eitri's own buffers even on a GTK older than 4.16; anything else, or unset, uses Eitri's own buffers from GTK 4.16 on. |
| `XDG_STATE_HOME` | Where Eitri keeps what it remembers: per-project layout, open tabs, prompt history, permission rules, trust answers and turn-review snapshots, under `eitri/` in this directory. Unset, empty or a relative path means `~/.local/state`. Nothing is written into your project. |
| `XDG_DATA_HOME`, `XDG_DATA_DIRS` | Where `eitri.plugin.load` looks for a plugin by name: `eitri/plugins/<name>/` under `XDG_DATA_HOME` first (`~/.local/share` when unset, empty or a relative path), under each folder of `XDG_DATA_DIRS` last (`/usr/local/share` then `/usr/share` when unset or empty). See [Plugins](#plugins). |
| `VERDANDI_CLAUDE_ACCOUNT`, `VERDANDI_CLAUDE_CONFIG_DIR` | The account and its configuration directory; see [Accounts](#accounts). |
