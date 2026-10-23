[English](README.md) | 简体中文
<!-- translated-from: README.md sha256=a4437dd19232cedf746fd85724d79360e1c34e3ad0dd6a2d451085ae951540a7 -->

# Eitri 文档

Eitri 是一个面向 Linux 的 Neovim 图形前端，把 agent、终端和你自己做的网页放在编辑器旁边，平铺在一个窗口里。它是一个围绕你自己的 Neovim 配置和你已经装好的 `claude` 的桌面窗口：Neovim 由 Neovide 的 GPU 渲染器的一个 fork 绘制，agent 的回复则以渲染好的 markdown、diff 和权限卡片的形式出现在旁边的面板里，一切都靠键盘驱动。

## 窗口

窗口由你自己排布的模块组成，像 i3/tmux 那样：

- **[编辑器](keys.zh-CN.md#在窗格之间移动)。** 一个真正的 Neovim，运行你自己的 `init.lua`、插件、LSP 和按键映射。
- **[聊天视图](keys.zh-CN.md#chat-views)。** 基础 agent 面板和 `agent#2` 到 `agent#9`，每个都有自己的页面；把会话标签页拆进独立视图，就能并排看对话、回复、diff 和权限卡片。
- **[终端](keys.zh-CN.md#containers-and-terminals)。** 底部终端和 `terminal#2` 到 `terminal#9`，每个有自己的 shell，第一次显示时才启动。
- **[插件和 Lua 页面](configuration.zh-CN.md#plugins)。** 你添加的页面也是模块，和内置模块放在一起；Eitri 自带一个，[Markdown 预览](markdown-preview.zh-CN.md)。
- **[容器](keys.zh-CN.md#containers-and-terminals)。** 把模块左右或上下分割，或者放进标签式、堆叠式容器。
- **[浮动窗口和便笺板](keys.zh-CN.md#浮动窗口便笺板和工作区)。** 让模块浮在其他模块上方，或者收起来再叫出来；它会一直运行。编辑器始终平铺。
- **[工作区](keys.zh-CN.md#浮动窗口便笺板和工作区)。** 为同一组模块保留好几种排布，在它们之间切换。

在 `$XDG_CONFIG_HOME/eitri/init.lua`（默认 `~/.config/eitri/init.lua`）里设[打开规则](configuration.zh-CN.md#where-a-new-module-opens)，决定新模块放在哪里。用 [`eitri.layout.export()`](configuration.zh-CN.md#layout-from-lua) 把排好的布局变成可以写进这个文件的启动布局调用。

## 它不是什么

Eitri 不取代 Neovim：缓冲区、动作、LSP 和补全仍然是 Neovim 自己的。它也不是许多 agent 的编排器；你在自己掌控的窗口里，每个标签页监督一个 Claude Code 会话。

## 接下来看哪里

- [安装](../../INSTALL.zh-CN.md)：软件包、安装脚本、从源码构建，以及验证下载的文件。
- [快速上手](getting-started.zh-CN.md)：打开一个项目、发送你的第一个提示，并熟悉各处。
- [按键](keys.zh-CN.md)：前缀、面板的按键，以及在窗格之间移动。
- [配置](configuration.zh-CN.md)：`$XDG_CONFIG_HOME/eitri/init.lua`（默认 `~/.config/eitri/init.lua`）、它读取的设置和按键映射。
- [Companion 模式](companion.zh-CN.md)：agent 面板作为独立窗口，放在你已经在用的 Neovim 旁边。
- [回合审阅](turn-review.zh-CN.md)：查看一个回合期间磁盘上改了什么，还原一个 hunk，把评论发回去。
- [Markdown 预览](markdown-preview.zh-CN.md)：在编辑器旁边显示渲染后的 Markdown 文件，和编辑器一起滚动，用 Vim 键阅读。
- [权限](permissions.zh-CN.md)：什么不经询问就运行，什么要等一张卡片，以及 bypass 模式。
- [Codex 与其他 agent](agents.zh-CN.md)：运行 Codex，或你注册的、说 ACP 的 agent 的标签页。
- [已知问题](../known-issues.zh-CN.md)：今天不支持的和还比较粗糙的地方。
