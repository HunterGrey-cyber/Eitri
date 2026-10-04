[English](README.md) | 简体中文
<!-- translated-from: README.md sha256=be131bc89c00be6035e374cb965d1be6b1016d509d4218fa062652cb7d0434a7 -->

# Eitri 文档

Eitri 是一个面向 Linux 的 Neovim 图形前端，旁边带着 Claude Code。它是一个围绕你自己的 Neovim 配置和你已经装好的 `claude` 的桌面窗口：Neovim 由 Neovide 的 GPU 渲染器的一个 fork 绘制，agent 的回复则以渲染好的 markdown、diff 和权限卡片的形式出现在旁边的面板里，一切都靠键盘驱动。

## 窗口

- **编辑器。** 一个真正的 Neovim，运行你自己的 `init.lua`、插件、LSP 和按键映射。
- **agent 面板。** Claude Code 的对话，每个会话一个标签页：渲染好的回复、每一次编辑各自一个 diff 行，以及一张用来批准或拒绝 agent 不能自己运行的东西的卡片。
- **底部终端。** 编辑器下面的一个 shell。窗口刚打开时它是隐藏的，只有在你把它显示出来之后才会启动。

## 它不是什么

Eitri 不取代 Neovim：缓冲区、动作、LSP 和补全仍然是 Neovim 自己的。它也不是许多 agent 的编排器；你在自己掌控的窗口里，每个标签页监督一个 Claude Code 会话。

## 接下来看哪里

- [安装](../../INSTALL.zh-CN.md)：软件包、安装脚本、从源码构建，以及验证下载的文件。
- [快速上手](getting-started.zh-CN.md)：打开一个项目、发送你的第一个提示，并熟悉各处。
- [按键](keys.zh-CN.md)：前缀、面板的按键，以及在窗格之间移动。
- [配置](configuration.zh-CN.md)：`~/.config/eitri/init.lua`、它读取的设置和按键映射。
- [Companion 模式](companion.zh-CN.md)：agent 面板作为独立窗口，放在你已经在用的 Neovim 旁边。
- [回合审阅](turn-review.zh-CN.md)：查看一个回合期间磁盘上改了什么，还原一个 hunk，把评论发回去。
- [权限](permissions.zh-CN.md)：什么不经询问就运行，什么要等一张卡片，以及 bypass 模式。
- [已知问题](../known-issues.zh-CN.md)：今天不支持的和还比较粗糙的地方。
