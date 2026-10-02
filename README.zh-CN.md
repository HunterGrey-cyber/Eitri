[English](README.md) | 简体中文
<!-- translated-from: README.md sha256=19a2e78f49d81a873c1bb7532a8b21989bd3e97d7ce96ae9bc1a7cc5cdcce6c0 -->

<div align="center">

<img src="docs/images/logo.svg" width="128" height="128" alt="Eitri 的 logo：一个窗口，编辑区里是用蓝色行号栏和三行绿色代码拼成的 E，旁边是一栏空白，底部是状态栏">

<h1>Eitri</h1>

<p><b>你的 Neovim，与 Claude Code 并肩。</b></p>

<p>
<a href="https://eitri.cc/zh/">官网</a> ·
<a href="INSTALL.zh-CN.md">安装</a> ·
<a href="https://github.com/HunterGrey-cyber/eitri/discussions">Discussions</a> ·
<a href="https://t.me/eitri_cc">Telegram</a> ·
<a href="https://matrix.to/#/#eitri:matrix.org">Matrix</a>
</p>

[![Release](https://img.shields.io/github/v/release/HunterGrey-cyber/eitri?label=release)](https://github.com/HunterGrey-cyber/eitri/releases/latest)
[![AUR](https://img.shields.io/aur/version/eitri-bin?label=AUR&logo=archlinux&logoColor=white)](https://aur.archlinux.org/packages/eitri-bin)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue)](LICENSE)

</div>

一个 Linux 上的 Neovim 图形前端。一个真正好读的 Claude Code：排好版的回复和 diff，一张张权限卡片，全程不用碰鼠标。Eitri 是一个桌面窗口（MIT 许可），包在你自己的 Neovim 配置和你已安装的 `claude` 外面。

**[eitri.cc](https://eitri.cc/zh/)** 上有截图、按键、和其他方案的对比，以及环境要求和限制。

> [!NOTE]
> **早期版本。** 已经能用，但难免有粗糙的地方，0.x 期间 `init.lua` 的 API 和默认按键还可能改动。遇到问题欢迎在
> [Issues](https://github.com/HunterGrey-cyber/eitri/issues) 或 [Discussions](https://github.com/HunterGrey-cyber/eitri/discussions)
> 告诉我们。0.3 的目标：稳定到能当我们自己日常的主力编辑器。

![Eitri：左边是打开着 Python 文件的 Neovim；右边是排好版的 Claude Code 回复，带一个高亮代码块，以及按下 leader 键后弹出的 which-key 提示](docs/images/hero.webp)

- **你自己的 Neovim。** 一个真正的 `nvim --embed`，加载你的 `init.lua`、插件、LSP 和按键映射，由 Neovide 渲染器的一个 fork 在 GPU 上绘制。
- **读得下去的回复。** markdown、按你配色高亮的代码、可以折叠的工具调用，每一次编辑单独一行 diff。
- **从头到尾用键盘。** 面板里用 vim 的按键，窗口用 tmux 的按键，`Ctrl+h/j/k/l` 在窗格之间移动。
- **也可以放在你自己的 nvim 旁边。** `:EitriPanel` 只把 agent 面板作为一个独立窗口，开在你终端里的 Neovim 旁边。

**更想把 Neovim 留在终端里？** companion 模式把同一个 agent 面板作为一个独立窗口，开在你已经在用的 nvim 旁边（在 tmux 里或不在，上游 Neovide 或别的图形前端都行）。它通过 nvim 自己的 RPC socket 附着上去，把你打开的文件和选区送给 agent，agent 改完后重新加载缓冲区，并能按行号打开文件；面板一消失，它装进去的东西就全部撤掉，你的配置不会被碰。两个窗口怎么摆由你的窗口管理器决定，sway、Hyprland 和 niri 还能用同样的 `Ctrl+h/j/k/l` 在它们之间移动焦点。装上 `eitri.nvim` 插件，运行 `:EitriPanel` 即可：见[配合你自己的 nvim 使用](INSTALL.zh-CN.md#use-it-beside-your-own-nvim)。

## 安装

```sh
curl --proto '=https' --proto-redir '=https' --tlsv1.2 -sSfL https://github.com/HunterGrey-cyber/eitri/releases/latest/download/install.sh | sh
eitri ~/path/to/project
```

需要 x86_64 的 Linux（Wayland），以及已安装的 Neovim 0.10 或更新版本和 Claude Code。Arch 用户可以从 AUR 安装 [`eitri-bin`](https://aur.archlinux.org/packages/eitri-bin) 或 [`eitri-git`](https://aur.archlinux.org/packages/eitri-git)。`.deb`、`.rpm`、从源码构建，以及如何验证下载的文件，见 [INSTALL.zh-CN.md](INSTALL.zh-CN.md)。

已知的限制见 [eitri.cc](https://eitri.cc/zh/#requirements)，接下来要做的见[路线图](https://eitri.cc/zh/#roadmap)。

## 社区

- [Discussions](https://github.com/HunterGrey-cyber/eitri/discussions)：提问、建议、分享你的配置
- [Telegram 群](https://t.me/eitri_cc)（中文）和 [Matrix 房间](https://matrix.to/#/#eitri:matrix.org) `#eitri:matrix.org`（英文）
- [已知问题](docs/known-issues.zh-CN.md)：目前不支持的和还比较粗糙的地方，提 bug 之前值得先看一眼
- Bug 请发到 [Issues](https://github.com/HunterGrey-cyber/eitri/issues)；构建和测试见 [CONTRIBUTING.md](CONTRIBUTING.md)（仅有英文版）
- [Neovide fork](https://github.com/HunterGrey-cyber/neovide)（`neovibe-integration` 分支）和 [agent sidecar](https://github.com/HunterGrey-cyber/verdandi) 在各自的仓库里

## 许可证

MIT（[LICENSE](LICENSE)），`terminal-input/` 除外，它是 Apache-2.0（派生自 Alacritty；见它自己的 `NOTICE`）。编辑器窗格构建在 [Neovide](https://github.com/neovide/neovide)（MIT）的一个 fork 之上。

`shell` 静态链接了 [nvim-rs](https://crates.io/crates/nvim-rs) 0.9.2，它是 **LGPL-3.0** 许可：每个发布页面都附带对应源码（Corresponding Source，`eitri-<version>-source.tar.gz`），安装后的 `SOURCE` 文件给出了针对修改过的 nvim-rs 重新链接的方法。`THIRD-PARTY-LICENSES` 收录其余所有声明。

agent sidecar 打包了 Anthropic 的 Claude Agent SDK，它不是开源软件。没有任何一个 Eitri 发行版本包含它：`eitri setup` 会从 npm 下载它，并在你的机器上构建 sidecar，适用 Anthropic 自己的条款。

Logo 的蓝绿两色取自 Jason Long 设计的 [Neovim logo](https://neovim.io)（CC BY 3.0）。

Eitri 的开发有 Claude 协助。
