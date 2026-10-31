English | [简体中文](terminal-claude.zh-CN.md)

# The `claude` in your terminal

If you run Claude Code in Eitri's bottom terminal, Eitri can play the part an editor's extension plays for the
`claude` in an editor's built-in terminal. An edit that `claude` asks you about also opens as a diff in your Neovim, the
diagnostics your language servers hold go to it, and your selection and `@`-mentions go with it. This is **off by
default**. Nothing in this page happens until you turn it on.

That `claude` is not an Eitri tab. It has no row in the agent panel, none of Eitri's permission cards, modes or saved
rules, and no turn review; it keeps its own prompts, modes and configuration. Eitri only gives it a way to reach your
editor.

This was built against the protocol of Claude Code 2.1.311 and tested with a stand-in client that speaks it. It has
not yet been run with a real interactive `claude`, so treat it as untried: nothing here promises that your `claude`
attaches or behaves as described.

## Turn it on

In `$XDG_CONFIG_HOME/eitri/init.lua` (default `~/.config/eitri/init.lua`):

```lua
eitri.config.set("terminal.claude_ide", true)
```

The value is `true` / `"on"` or `false` / `"off"`, and the default is off. It is read once, when the window that holds
the editor starts, so changing it takes a restart of that window. The second window follows the first, and the
[companion window](companion.md) ignores the key because it has no terminal. See
[Configuration](configuration.md).

## What it does

While the key is on and the window is open, Eitri:

- listens on `127.0.0.1` only, on a port the system picks, and accepts only a client that presents a random token that
  is new for each start of the window;
- writes `~/.claude/ide/<port>.lock` for that listener, readable by you alone, and removes it when the window ends. It
  also removes stale lock files of its own that a crash left behind, and touches nothing else in `~/.claude`;
- sets `CLAUDE_CODE_SSE_PORT` to that port in the shells of its bottom terminals, in both windows. A `claude` started
  from such a shell, or from anything started inside it, finds the lock by that port and attaches.

With the key off Eitri does none of this: no listener, no lock file, and nothing added to any terminal's environment.

When a `claude` has attached:

- **Diffs.** When it asks you about an edit, the file and its proposed version open side by side next to your window,
  without taking your cursor or leaving your mode. You can edit the proposal. `<localleader>a` (`\a` unless you changed your
  local leader) or `:w` in the proposal accepts it; `<localleader>d` rejects it. `<localleader>q`, `:q` or closing either window leaves the view without
  answering, and the prompt in the terminal then decides. Eitri writes no file itself: an accept hands the text back to
  the `claude`, which does its own write. Only files inside the project open this way.
- **Diagnostics.** The `claude` can read the diagnostics of the files your editor has loaded in the project.
- **Selection.** Your current selection, and the cursor's line, are sent to it as you move.
- **`:EitriMention`.** Sends the cursor's line, or a range (`:'<,'>EitriMention`), to it as an `@`-mention. The command
  has no default key; map it yourself if you want one.

Nothing a `claude` can call through this writes a file, runs code, or opens or loads a file in your editor.

## Keep a session out

A `claude` that you did not start from Eitri's terminal does not attach by itself. To keep one that is in Eitri's
terminal from attaching:

- set `CLAUDE_CODE_AUTO_CONNECT_IDE=false` in that shell before starting it; or
- set the CLI's own `diffTool` setting to `terminal`, which keeps diffs in the terminal.

Any process of your user can read the lock file and connect with its token. The access that gives is the list above,
nothing more; that is the same footing as any program that can already read your files or type into your terminals.
Leave the key off if you do not want the lock file to exist.

## Limits

- A tmux server that was already running keeps its own environment, so its shells lack the port. Run
  `tmux setenv CLAUDE_CODE_SSE_PORT <port>` (the port is the number in the lock file's name) or start tmux from an
  Eitri terminal.
- A shell, a tmux server or a `nohup` job that outlives Eitri keeps the old port. After a restart of the window, a
  `claude` there does not reach the new Eitri; start it from a new shell.
- The port belongs to the first window. If you close it while the second window stays open, a connected `claude` loses
  the connection, and the second window's later shells have no port.
- If Eitri cannot make the lock (no home directory, a `~/.claude` or `~/.claude/ide` that others can write, no random
  bytes), it starts no listener, shows a short notice once, and sets no port.
- Other versions of the `claude` CLI may behave differently from 2.1.311.
