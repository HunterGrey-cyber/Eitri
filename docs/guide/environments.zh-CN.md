[English](environments.md) | 简体中文
<!-- translated-from: environments.md sha256=49693c9cf5e34c4cc827f6653b9c46b653b4e0d14d6e02e085f6942610ee8247 -->

# 工作环境

Eitri 自己做的事不多，直到你告诉它你怎么工作。这里是几份完整的 `init.lua`，每份写给一种工作方式，可以复制了再改。每一份都是 Eitri 源码树里 `examples/environments/` 下的一个目录，里面有 `init.lua` 和一份简短的 README，说明它适合谁、设置了什么。

| 配方 | 适合你，如果 | 它设置了什么 |
|---|---|---|
| [`tmux`](../../examples/environments/tmux/README.zh-CN.md) | 你整天待在 tmux 里 | 在窗格所在目录启动的分割、开在下方的新终端、一个 shell 工作区、停顿之后出现的键列表；你的 tmux 键自己就会带过来（哪些能带过来，见[从 tmux 迁过来](migrate-from-tmux.zh-CN.md)） |
| [`i3`](../../examples/environments/i3/README.zh-CN.md) | 你用 i3 或 sway，并把 Alt 当作 `$mod` | 在 `Ctrl+Alt` 上的容器键、Eitri 里的所有东西都平铺、标签式容器、一个下拉终端 |
| [`markdown-writer`](../../examples/environments/markdown-writer/README.zh-CN.md) | 你用 Markdown 写作 | 只要当前是 Markdown 文件，预览就在编辑器旁边，聊天在下面 |
| [`many-agents`](../../examples/environments/many-agents/README.zh-CN.md) | 有好几个 agent 同时为你工作 | Strips 栏、The pass 和表格；新的聊天视图开在拿着按键的模块右边；一张卡片把它的聊天带回来 |
| [`beside-nvim`](../../examples/environments/beside-nvim/README.zh-CN.md) | 你把 Neovim 放在终端里 | agent 面板和 Markdown 预览作为窗口开在你自己的 Neovim 旁边 |

它们里面的每个调用，都是[配置](configuration.zh-CN.md)里有文档的，每个文件都能作为 `init.lua` 顺利启动。

<a id="making-one-your-own"></a>
## 把一份变成你自己的

- **从一份开始，然后改。** 配方就是普通的 `init.lua`：把它复制到 `~/.config/eitri/init.lua`，或者把你想要的部分粘贴进你已有的那份。Eitri 拒绝的调用什么也不做，窗口打开时会列出来，带着文件和行号。
- **先排好窗口，再把它写下来。** 先手动把窗口布局摆好，然后在你自己的一个命令里调用 `eitri.layout.export()`。它以文本的形式返回 `eitri.layout.default` 和 `eitri.layout.workspace` 调用，第一次启动时用它们就能得到这个布局；[从 Lua 设置布局](configuration.zh-CN.md#layout-from-lua)里有一个绑在键上的命令，把它们写进文件，方便粘贴。
- **已保存的布局优先。** Eitri 会记住每个项目的布局，所以改过的 `eitri.layout.default` 只在还没有保存布局的项目里才看得到。想在你以前打开过的项目里看到它，就删掉那个项目的布局文件（[布局保存在哪里](configuration.zh-CN.md#layout-from-lua)）。
- **加一个你自己的页面。** 当某个配方需要 Eitri 没有显示的东西时，把它写成插件：[写一个插件](write-a-plugin.zh-CN.md)是参考，`many-agents` 配方里的 agent 页面也是用同样的方式构建的。
