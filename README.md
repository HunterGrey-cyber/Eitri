English | [简体中文](README.zh-CN.md)

<div align="center">

<img src="docs/images/logo.svg" width="112" height="112" alt="The Eitri logo: a window whose editor pane holds an E made of a blue gutter and three green code lines, a blank pane beside it, and a status bar">

<h1>Eitri</h1>

<p><b>Your own Neovim, tiled like tmux, in a GUI that renders.</b></p>
<p>Neovim, coding agents, rendered Markdown, terminals and web pages you build yourself, tiled in one window and driven by vim and tmux keys.</p>

<p>
<a href="https://eitri.cc">Website</a> ·
<a href="https://eitri.cc/docs/">Docs</a> ·
<a href="INSTALL.md">Install</a> ·
<a href="https://github.com/HunterGrey-cyber/eitri/discussions">Discussions</a>
</p>

[![Release](https://img.shields.io/github/v/release/HunterGrey-cyber/eitri?label=release)](https://github.com/HunterGrey-cyber/eitri/releases/latest)
[![AUR](https://img.shields.io/aur/version/eitri-bin?label=AUR&logo=archlinux&logoColor=white)](https://aur.archlinux.org/packages/eitri-bin)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue)](LICENSE)

</div>

<p align="center">
<a href="https://eitri.cc/#workspace"><img src="docs/images/workspace-loop.webp" width="880" alt="Eitri with Neovim on the left and a Claude Code chat on the right: a question about selected code, edits approved on cards, the turn's changes reviewed with one hunk put back, and a note sent back; a caption names each step"></a>
<br><sub>Your Neovim on the left, a Claude Code chat on the right: ask about a selection, approve edits, review the turn, put one hunk back.</sub>
</p>

```sh
curl --proto '=https' --proto-redir '=https' --tlsv1.2 -sSfL https://github.com/HunterGrey-cyber/eitri/releases/latest/download/install.sh | sh
eitri ~/path/to/project
```

Linux x86_64 on Wayland, Neovim 0.11 or newer, and Claude Code, Codex or an ACP agent you have installed.
On Arch: [`eitri-bin`](https://aur.archlinux.org/packages/eitri-bin). Packages and source builds: [INSTALL.md](INSTALL.md).

## Why try it

- Eitri runs your own Neovim, with your `init.lua`, plugins, LSP and keymaps, drawn by a fork of Neovide's renderer.
- Agent replies are laid out like documents, and each edit is a row with its diff.
- The Markdown preview sits beside the file you edit and scrolls with Neovim.
- Chats, terminals and pages tile around the editor with tmux's keys, or as windows of your compositor on Hyprland, sway and niri.
- Claude Code, Codex and ACP agents each get tabs, and `Ctrl+b u` jumps to the one that has waited longest.
- After a turn, `c` lists every file that changed and `x` puts a hunk back, with or without git.
- `:EitriPanel` opens the agent panel beside the Neovim you already run in a terminal.

New here? [Getting started](https://eitri.cc/docs/getting-started/) goes from the first launch to a finished turn, and `:EitriTutor` teaches the keys.

## Limits

Eitri 0.4.0 is an early release and not yet stable: the `init.lua` API and default keys may change during 0.x.
It runs only on Linux x86_64 for now (macOS is in progress), and it uses more memory than native Neovide.
The installer builds the agent sidecar on your machine (a few minutes, about 1.4 GiB of free disk).
Eitri sends nothing anywhere on its own ([what leaves your machine](docs/known-issues.md#what-leaves-your-machine)). The rest: [known issues](docs/known-issues.md).

## More

- [Docs](https://eitri.cc/docs/): guides, keys, configuration, releases
- [How it compares](https://eitri.cc/#differs) with claudecode.nvim, CodeCompanion, avante.nvim and Zed
- [Discussions](https://github.com/HunterGrey-cyber/eitri/discussions), [Matrix](https://matrix.to/#/#eitri:matrix.org), [Telegram](https://t.me/eitri_cc) (mostly Chinese), [Issues](https://github.com/HunterGrey-cyber/eitri/issues), [CONTRIBUTING.md](CONTRIBUTING.md)

## How Eitri is made

Most of the code is written by coding agents, Claude Code and Codex. Hunter Grey sets the direction, chooses between
designs, reads closely where a mistake would cost the most, and steps in when something is off.

## Licence

MIT ([LICENSE](LICENSE)), except `terminal/input/` (Apache-2.0, from Alacritty). `shell` statically links
[nvim-rs](https://crates.io/crates/nvim-rs) 0.9.2 (LGPL-3.0): each release carries its Corresponding Source
(`eitri-<version>-source.tar.gz`), and the installed `SOURCE` file says how to relink. The agent sidecar uses
Anthropic's Claude Agent SDK, which is not open source and is never shipped: `eitri setup` fetches it and builds the
sidecar on your machine. Every other notice: `THIRD-PARTY-LICENSES`. Logo colours from the
[Neovim logo](https://neovim.io) by Jason Long (CC BY 3.0).
