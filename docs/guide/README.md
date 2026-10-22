English | [简体中文](README.zh-CN.md)

# Eitri documentation

Eitri is a Neovim GUI for Linux that puts agents, terminals and web pages beside your editor, in a tiling “window”
manager. It is a desktop window around your own Neovim configuration and the `claude` you have installed: Neovim is
drawn by a fork of Neovide's GPU renderer, and the agent's replies appear as rendered markdown, diffs and permission
cards in a panel next to it, all driven from the keyboard.

## The window

The window is made of modules you arrange yourself, i3/tmux style:

- **[The editor](keys.md#moving-between-panes).** A real Neovim running your own `init.lua`, plugins, LSP and keymaps.
- **[Chat views](keys.md#chat-views).** The base agent panel and `agent#2` to `agent#9`, each its own page;
  break a session tab out into a view to see conversations side by side, with their replies, diffs and permission cards.
- **[Terminals](keys.md#containers-and-terminals).** The bottom terminal and `terminal#2` to `terminal#9`,
  each with its own shell, started when you first show it.
- **[Plugin and Lua pages](configuration.md#plugins).** Pages you add are modules alongside the built-in ones; Eitri ships one, the [Markdown preview](markdown-preview.md).
- **[Containers](keys.md#containers-and-terminals).** Split modules side by side or one above another, or put them in tabbed or stacked containers.
- **[Floats and the scratchpad](keys.md#floats-the-scratchpad-and-workspaces).** Float a module over the others, or stow it and call it back; it keeps running. The editor stays tiled.
- **[Workspaces](keys.md#floats-the-scratchpad-and-workspaces).** Keep several arrangements of the same modules and switch between them.

Set [open rules](configuration.md#where-a-new-module-opens) in `$XDG_CONFIG_HOME/eitri/init.lua` (default
`~/.config/eitri/init.lua`) to choose where new modules go. Use
[`eitri.layout.export()`](configuration.md#layout-from-lua) to turn an arrangement into startup layout calls for that
file.

## What it is not

Eitri does not replace Neovim: buffers, motions, LSP and completion stay Neovim's own. It is also not an
orchestrator of many agents; you supervise one Claude Code session per tab, in a window you control.

## Where to go next

- [Install](../../INSTALL.md): packages, the install script, building from source, and verifying a download.
- [Getting started](getting-started.md): open a project, send your first prompt, and find your way around.
- [Keys](keys.md): the prefix, the panel's keys and moving between panes.
- [Configuration](configuration.md): `$XDG_CONFIG_HOME/eitri/init.lua` (default `~/.config/eitri/init.lua`), the settings it reads and the keymap.
- [Companion mode](companion.md): the agent panel as its own window beside the Neovim you already use.
- [Turn review](turn-review.md): see what changed on disk during a turn, revert a hunk, send comments back.
- [Markdown preview](markdown-preview.md): a Markdown file rendered beside the editor, scrolling with it, read with Vim keys.
- [Permissions](permissions.md): what runs without asking, what waits on a card, and the bypass mode.
- [Codex and other agents](agents.md): tabs that run Codex, or an agent you register that speaks ACP.
- [Known issues](../known-issues.md): what is unsupported or rough today.
