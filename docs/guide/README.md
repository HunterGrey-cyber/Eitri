English | [简体中文](README.zh-CN.md)

# Eitri documentation

Eitri is a Neovim GUI for Linux with Claude Code beside it. It is a desktop window around your own Neovim
configuration and the `claude` you have installed: Neovim is drawn by a fork of Neovide's GPU renderer, and the
agent's replies appear as rendered markdown, diffs and permission cards in a panel next to it, all driven from
the keyboard.

## The window

- **The editor.** A real Neovim running your own `init.lua`, plugins, LSP and keymaps.
- **The agent panel.** Claude Code's conversation, one tab per session: rendered replies, each edit as its own
  diff row, and a card to approve or deny what the agent may not run on its own.
- **The bottom terminal.** A shell below the editor. It is hidden when a window first opens and
  only starts once you show it.

## What it is not

Eitri does not replace Neovim: buffers, motions, LSP and completion stay Neovim's own. It is also not an
orchestrator of many agents; you supervise one Claude Code session per tab, in a window you control.

## Where to go next

- [Install](../../INSTALL.md): packages, the install script, building from source, and verifying a download.
- [Getting started](getting-started.md): open a project, send your first prompt, and find your way around.
- [Keys](keys.md): the prefix, the panel's keys and moving between panes.
- [Configuration](configuration.md): `~/.config/eitri/init.lua`, the settings it reads and the keymap.
- [Companion mode](companion.md): the agent panel as its own window beside the Neovim you already use.
- [Turn review](turn-review.md): see what changed on disk during a turn, revert a hunk, send comments back.
- [Permissions](permissions.md): what runs without asking, what waits on a card, and the bypass mode.
- [Known issues](../known-issues.md): what is unsupported or rough today.
