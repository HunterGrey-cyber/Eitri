English | [简体中文](README.zh-CN.md)

# Eitri documentation

Eitri is a Neovim GUI for Linux that puts agents, terminals and the web pages you build beside your editor, tiled in one
window or, on Hyprland, sway and niri, as windows of your window manager. It is a desktop window around your own Neovim configuration and the agents you have installed: Neovim is
drawn by a fork of Neovide's GPU renderer, and the agent's replies appear as rendered markdown, diffs and permission
cards in a panel next to it, all driven from the keyboard.

## The window

The window is made of modules you arrange yourself, i3/tmux style:

- **[The editor](keys.md#moving-between-panes).** A real Neovim running your own `init.lua`, plugins, LSP and keymaps.
- **[Chat views](keys.md#chat-views).** The base agent panel and `agent#2` to `agent#9`, each its own page;
  break a session tab out into a view to see conversations side by side, with their replies, diffs and permission cards.
- **[Terminals](keys.md#containers-and-terminals).** The bottom terminal and `terminal#2` to `terminal#9`,
  each with its own shell, started when you first show it.
- **[Plugin and Lua pages](pages.md).** Pages you add are modules alongside the built-in ones; Eitri ships one, the [Markdown preview](markdown-preview.md).
- **[Containers](keys.md#containers-and-terminals).** Split modules side by side or one above another, or put them in tabbed or stacked containers.
- **[Floats and the scratchpad](keys.md#floats-the-scratchpad-and-workspaces).** Float a module over the others, or stow it and call it back; it keeps running. The editor stays tiled.
- **[Workspaces](keys.md#floats-the-scratchpad-and-workspaces).** Keep several arrangements of the same modules and switch between them.
- **[Two windows](two-windows.md).** A second window for a second monitor, with its own workspaces and no editor; terminals, chats and pages move between the two while they keep running.
- **[Compositor windows](compositor-windows.md).** On Hyprland, sway and niri, each chat and page is a window your own window manager tiles, and the editor keeps the first window.

Set [open rules](configuration.md#where-a-new-module-opens) in `$XDG_CONFIG_HOME/eitri/init.lua` (default
`~/.config/eitri/init.lua`) to choose where new modules go. Use
[`eitri.layout.export()`](configuration.md#layout-from-lua) to turn an arrangement into startup layout calls for that
file.

## What it is not

Eitri does not replace Neovim: buffers, motions, LSP and completion stay Neovim's own. It is also not an
orchestrator that sets agents to work on each other: each tab holds one session of Claude Code, Codex or an agent
you register, and you supervise each of them yourself, in a window you control.

## Where to go next

- [Install](../../INSTALL.md): packages, the install script, building from source, and verifying a download.
- [Getting started](getting-started.md): open a project, send your first prompt, and find your way around.
- [Keys](keys.md): the prefix, the panel's keys and moving between panes.
- [Configuration](configuration.md): `$XDG_CONFIG_HOME/eitri/init.lua` (default `~/.config/eitri/init.lua`), the settings it reads and the keymap.
- [How the editor reaches the screen](editor-presentation.md): the editor's own Wayland surface, `editor.presentation` and its fallback.
- [Compositor windows](compositor-windows.md): chats and pages as windows of Hyprland, sway or niri, with a window rule for each.
- [Companion mode](companion.md): the agent panel as its own window beside the Neovim you already use.
- [Turn review](turn-review.md): see what changed on disk during a turn, revert a hunk, send comments back.
- [Markdown preview](markdown-preview.md): a Markdown file rendered beside the editor, scrolling with it, read with Vim keys.
- [Permissions](permissions.md): what runs without asking, what waits on a card, and the bypass mode.
- [Codex and other agents](agents.md): tabs that run Codex, or an agent you register that speaks ACP.
- [Your editor, from an agent's tab](editor-tools.md): how an agent in any tab shows you a file, reads what your editor holds, asks your language servers and proposes changes for you to approve, and how to turn that off.
- [Pages you build](pages.md): your own HTML page as a Lua panel, or a plugin page with a manifest (experimental).
- [Known issues](../known-issues.md): what is unsupported or rough today.
