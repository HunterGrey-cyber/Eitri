[English](README.md) | 简体中文
<!-- translated-from: README.md sha256=c51d97685dc8129b9189fc0f62462551ab608decc0c971222d575973e970e3ea -->

# Eitri

**你的 Neovim，与 Claude Code 并肩。**

一个真正好读的 Claude Code：排好版的回复和 diff，一张张权限卡片，全程不用碰鼠标。Eitri 是一个 Linux 桌面窗口（MIT 许可），包在你自己的 Neovim 配置和你已安装的 `claude` 外面。

<!-- MEDIA: hero.png -- the whole window. Left: the editor on LazyVim, a file open, a Visual selection.
Right: the panel mid-conversation: a markdown reply with a highlighted code block, a folded tool-call
row, and a waiting card with a line diff (auto runs in-project edits unasked, so stage the card with an
edit outside the project or on a protected path such as `.git/`). Terminal hidden. 2560x1600 at scale
2, shown at width 900; the caption names the desktop, session type and distribution. -->

- **你自己的 Neovim。** 一个真正的 `nvim --embed`，加载你的 `init.lua`、插件、LSP 和按键映射，由 Neovide GPU 渲染器的一个 fork 绘制；Eitri 只为自己保留少数几个按键。
- **读得下去的回复。** markdown、按你配色方案的语法颜色高亮的代码、结果可以折叠的工具调用，以及每一次编辑各占一行的 diff；编辑之后，你已打开且没改过的文件会在你的 Neovim 里重新加载。当前文件的文件名，以及 Visual 选区（如果有的话），会随每条提示一起发送。
- **键盘优先。** 面板里用 vim 的按键，窗口用 tmux 的按键（前缀键 `Ctrl+b`），`Ctrl+h/j/k/l` 在各窗格之间移动。
- **一个项目一个窗口**：多个 agent 会话放在标签页里，一个底部终端，以及按项目保存的布局。

```sh
curl --proto '=https' --proto-redir '=https' --tlsv1.2 -sSfL https://github.com/HunterGrey-cyber/eitri/releases/latest/download/install.sh | sh
eitri ~/path/to/project
```

仅支持 Linux x86_64；请先读[状态与限制](#status-and-limits)。候选版本（release candidate）的安装命令写在它自己的发布说明里，见[发布页面](https://github.com/HunterGrey-cyber/eitri/releases)。安装脚本会从 GitHub 下载发行版本、校验其校验和，安装到 `~/.local` 下（不需要 `sudo`），并在你的机器上构建 agent sidecar；`.deb`、`.rpm`、从源码构建，以及如何验证正式版本，见 [INSTALL.zh-CN.md](INSTALL.zh-CN.md)。

## 它能做什么

- **编辑器。** 跑在 `GtkGLArea` 上的 Neovide 渲染器，驱动一个在项目目录里启动的 `nvim`。Eitri 保留的按键：`Ctrl+h/j/k/l`、`Ctrl+=`/`Ctrl+-`/`Ctrl+0`（文字大小）和 `F11`。
- **agent。** Claude Code，通过 Claude Agent SDK，运行在单独的 sidecar 进程里。回复边写边流式显示。对话保存在 Rust 宿主进程里，而不是 WebView 里，所以面板重新加载或崩溃后对话仍在（最近 300 ms 内输入的草稿可能会丢）。
- **审批。** 每次工具调用都要经过一道关卡。在 **auto** 模式下，项目内的编辑，以及 Eitri 能确认不会越出项目范围的读取，由 Eitri 自动放行；其余几乎一切都是一张卡片（没有 shell 解析器，所以管道、重定向或 `cd` 都会询问），卡片上还可以选择在此项目里一律允许这类调用。在 **bypass** 模式下什么都不询问；进入该模式时会先询问一次。
- **会话。** 每个标签页一段对话，全部同时运行。可以从选择器里恢复一个已记录的会话；`<leader>t` 会在这里关闭一段对话，并显示能在你的终端里接着这段对话的 `claude --resume` 命令。
- **窗口。** 编辑器、面板和终端都是模块，可以用 tmux 的按键显示、隐藏、拆分、交换和放大。底部终端是自己的一个 PTY，运行你的 shell，支持括号粘贴（bracketed paste）、OSC 52 复制和复制模式。在 Neovim 里执行一次 `:colorscheme`，外壳和面板都会跟着换色。按键和选项写在 `~/.config/eitri/init.lua` 里；绑定冲突会导致启动报错，并指出冲突的两个绑定。

<!-- MEDIA: colorscheme.webm (or .gif if the host renders no video) -- the same window as hero.png,
switching `:colorscheme` three times (a light theme, a dark theme, back); the chrome, the panel and
its code block follow each switch. 1280x800, 6 seconds or less, under 3 MB. -->

## 按键

窗口按键沿用原生 tmux，面板按键沿用 vim。**`prefix ?` 会列出你这份构建里绑定的每一个按键，并已应用你自己的重新绑定；以它为准，而不是这张表。**

| 位置 | 按键 |
|---|---|
| 前缀键（prefix，`Ctrl+b`）之后 | `c` 新建 agent 标签页，`n`/`p` 下一个/上一个标签页，`w` 选择标签页或会话，`a`/`e`/`t` 显示并聚焦 agent、编辑器或终端（如果它已经拥有按键，则隐藏它），`z` 放大，`x` 经 y/n 确认后关闭该模块，`f` 跳转标签，`?` 显示全部按键 |
| agent 面板，浏览 | `j`/`k` 在各行之间移动，`gg`/`G`，`/` 搜索，`a`/`d` 允许/拒绝光标所在的卡片，`D` 附理由拒绝，`y` 复制，`i` 或 `Ctrl+j` 开始输入 |
| agent 面板，输入 | `Enter` 发送，`Ctrl+y` 批准等得最久的那张卡片，`Ctrl+g` 在 `nvim` 里编辑提示，`Shift+Tab` 在 auto ⇄ bypass 之间切换，`Esc` 或 `Ctrl+k` 回到浏览 |

`Ctrl+h/j/k/l` 在窗格之间移动：在 Neovim 的 Normal 和 Visual 模式下，先在它自己的分屏之间移动，走到边缘才移到别的窗格（如果你在用 vim-tmux-navigator，由它来判断边界）；在终端里则始终有效，终端里的 shell 永远收不到这几个键（`prefix Ctrl+l` 会把字面的 `Ctrl+l` 发过去）。面板的 leader 键就是你 Neovim 的 `mapleader`。[`docs/keymap/tmux-ctrl-a.lua`](docs/keymap/tmux-ctrl-a.lua) 移植了一整份 tmux 配置。

<!-- MEDIA: hint.png -- after `prefix f`: jump labels over the editor, the panel and the tray.
1600x1000 at scale 2, shown at width 800. -->

## 与其他方案的比较

在 Neovim 旁边运行 coding agent 的其他方式，按各项目自己的描述列出（2026-09-30 查阅）：

| | 编辑器 | agent | 你在哪里读 agent 的工作成果 |
|---|---|---|---|
| Eitri | Neovim（`nvim --embed`，你自己的配置） | Claude Code | 渲染好的面板就在你自己的 Neovim 旁边，同在一个窗口里。 |
| [avante.nvim](https://github.com/avante-corp/avante.nvim) | Neovim（插件） | 多家 LLM 提供商；ACP agent，其中包括 Claude Code | Neovim 窗口里的侧边栏聊天 |
| [codecompanion.nvim](https://github.com/olimorris/codecompanion.nvim) | Neovim（插件） | LLM 适配器；ACP agent，其中包括 Claude Code | 一个聊天 buffer；较大的修改建议放在浮动窗口的 diff 里 |
| [claude-code.nvim](https://github.com/greggh/claude-code.nvim) | Neovim（插件） | Claude Code | Neovim 窗口里的 Claude Code 终端界面 |
| [claudecode.nvim](https://github.com/coder/claudecode.nvim) | Neovim（插件，走 Claude Code 的 IDE 协议） | Claude Code | 终端分屏里的 Claude Code；修改建议显示在 Neovim 的 diff 视图里 |
| [CopilotChat.nvim](https://github.com/CopilotC-Nvim/CopilotChat.nvim) | Neovim（插件） | 通过 GitHub Copilot 提供的模型，外加自定义 provider | Neovim 里的聊天窗口 |
| [Claude Code CLI](https://code.claude.com/docs/en/overview)，放在 tmux 窗格里 | 另一个窗格里的 Neovim | Claude Code | CLI 自带的终端界面 |
| [Zed](https://zed.dev/docs/ai/overview) | Zed，带它的 Vim 模拟层 | Zed 自己的 agent；ACP agent，其中包括 Claude | Zed 的 agent 面板，支持逐 hunk 审阅 |
| [Cursor](https://cursor.com/docs) | Cursor，基于 VS Code | Cursor 自己的 agent，模型来自多家厂商 | 一个侧边窗格和一个 agents 窗口 |

## 为什么不是插件

我们自己也是终端的重度用户，也不打算劝你离开终端。Eitri 走出终端只为一件事：把回复排成文档来读，这是终端的字符格子画不出来的。上面那些 Neovim 插件，以及 tmux 窗格里跑的 `claude`，都让你留在终端、tmux 和 `ssh` 里；Eitri 多出来的是“读”。而且，如果单是 Neovide 本身还不足以让你离开终端，那么 Eitri 的编辑器这一半同样不构成理由：编辑器就是同一个 Neovim，以同样的方式绘制。

理由在另一半：读一读 agent 做了什么。在终端或 Neovim buffer 里，回复是画进一个单一字体的字符格子网格里的。Eitri 把它放进 WebView 里排版（正文、表格、代码块、diff），并让它周围的一切仍按 Vim 的方式运作：面板有模式、计数、`gg`/`G`、搜索和你的 leader 键；批准就是在卡片上按 `a`；整个窗口由 tmux 的按键驱动。Zed 和 Cursor 也会渲染 agent 的工作成果，只不过是围绕它们自己的编辑器。

代价：一个 GUI 窗口（Linux、x86_64，在 Wayland 上测试过；不支持远程编辑）、Claude Code 是唯一的 agent，以及比原生 Neovide 更多的内存和 GPU 时间，因为面板是一个 WebView。测量环境为 headless sway（Intel Core Ultra 5 125H，Mesa 26.2.2，GTK 4.22.5，WebKitGTK 2.52.6，165 Hz 下 3072x1920，缩放 1.5），两个应用都用 `nvim --clean` 和全新的用户配置，原生 Neovide 由同一个 fork 构建，面板停在空标签页上、没有 agent 在运行；尚未在 GTK 4.14 或 Ubuntu 24.04 上测量：

- **内存。** 272 MiB（PSS，整棵进程树），对比 67-70 MiB；其中 WebKit 的两个进程约占 137 MiB。每个 agent 标签页会多出一个 sidecar（约 80 MiB RSS）和它自己的 `claude`（265-290 MiB RSS），每个标签页大约 350 MiB。
- **GPU** 每个有变化的帧 3.2 ms，对比约 1 ms；**启动** 到可编辑的 buffer 为 672 ms，对比 499 ms。

<a id="status-and-limits"></a>
## 状态与限制

Eitri 0.2.0 目前处于候选版本阶段。大概率还有 bug；`init.lua` 的 API 和默认按键在 0.x 期间仍可能变化。

<!-- TODO: after the rc.2 hardware test: the machines, GPUs, desktops and distributions it has been
used on, and what was exercised only in the project's headless GUI sandbox. -->

- **平台。** 在 Wayland（GNOME 和 sway）上测试过；X11 不会被拒绝，但没有测试过。没有 macOS 或 Windows 版本。
- **Ubuntu 23.10 及以后版本。** 默认的 AppArmor 策略会拦截 WebKitGTK 的沙盒；面板的位置会改为显示只需做一次的修复步骤（[INSTALL.zh-CN.md](INSTALL.zh-CN.md#ubuntu-2310-and-later)）。
- **输入法与缩放。** 三个窗格里 fcitx5 的组合输入，以及输出缩放的实时变化，都只在 headless 沙盒里测试过。刚做完缩放（zoom）或调整大小之后马上开始的一次组合输入，候选窗口可能会按过期的光标位置摆放。
- **打字延迟**（在 headless compositor 中，从按键事件到合成帧，并非输入到光子；165 Hz；相对原生 Neovide 增加的中位数）：AMD 上 +0.34 ms，Intel 上 +0.71-0.78 ms；AMD 上设置 `EITRI_EDITOR_DMABUF=0` 时 +10.75 ms。GTK 版本低于 4.16 时用另一种方式绘制编辑器，尚未测量。
- **回复流式输出时打字。** 你在编辑器里打字时，面板的流式更新会被限制在每秒 5 次（`agent.typing_cadence_hz`）。即便如此，仍有 6-9 % 的按键要用约两次刷新而不是一次，这是在 Intel 上用一条模拟的流测得的；真实回复还没有测过。

## 环境要求

- x86_64 的 Linux，并且有 `webkitgtk-6.0` API（WebKitGTK 2.40 或更新，必需）。预构建二进制按 glibc 2.39 和 GTK 4.14 构建，更旧的 glibc 和 GTK 没有测试过。Ubuntu 24.04、Debian 13、Fedora 40+、RHEL 10 和 Arch 都满足这些要求。需要一个 Wayland 会话和可用的 OpenGL 驱动。
- `PATH` 上的 Neovim ≥ 0.10，或者由安装脚本单独为 Eitri 获取的一份私有副本（永远不会放上 `PATH`）。
- 已安装的 Claude Code ≥ 2.1.252 且 < 3.0。Eitri 不捆绑 Claude Code，它运行的是你自己装的那份。
- 对于在安装时于你机器上构建的 agent sidecar：需要网络访问（安装脚本会获取 Verdandi 源码和它自己锁定版本的 Node.js；不使用系统的 Node 或 npm），以及约 600 MiB 的可用空间。

## 路线图

按这个顺序，不给日期。

1. ACP 客户端：Claude Code 以外的 agent 也能进面板，先接 Codex。
2. `@` 引用：从 Neovim 里挑文件、符号和诊断。
3. 行内 diff：agent 的修改直接画在你的 Neovim buffer 里，逐块接受或丢弃。
4. 一轮对话改过的文件一次看完，并能整轮回滚。
5. 从终端出发：在你正在用的 Neovim 里敲 `:Eitri`，窗口接上同一个会话；关掉窗口，就回到终端里原来的位置。
   也可以让 Neovim 一直留在终端里，只把 agent 面板开成单独的窗口，交给 Hyprland（或 sway）平铺在旁边：
   Neovim 保持你终端原生的速度，`Ctrl+h/j/k/l` 在两者之间移动。

## 遥测

Eitri 自己不增加任何遥测；Claude Code 自己的遥测按你的 Claude Code 设置来。

## 许可证

MIT（[LICENSE](LICENSE)），`terminal-input/` 除外，它是 Apache-2.0（派生自 Alacritty；见它自己的 `NOTICE`）。编辑器窗格构建在 [Neovide](https://github.com/neovide/neovide)（MIT）的一个 fork 之上。

`shell` 静态链接了 [nvim-rs](https://crates.io/crates/nvim-rs) 0.9.2，它是 **LGPL-3.0** 许可：每个发布页面都附带对应源码（Corresponding Source，`eitri-<version>-source.tar.gz`），安装后的 `SOURCE` 文件给出了针对修改过的 nvim-rs 重新链接的方法。`THIRD-PARTY-LICENSES` 收录其余所有声明。

agent sidecar 打包了 Anthropic 的 Claude Agent SDK，它不是开源软件。没有任何一个 Eitri 发行版本包含它：`eitri setup` 会从 npm 下载它，并在你的机器上构建 sidecar，适用 Anthropic 自己的条款。

## 链接

- [发布页面](https://github.com/HunterGrey-cyber/eitri/releases)、[issues](https://github.com/HunterGrey-cyber/eitri/issues)
- [INSTALL.zh-CN.md](INSTALL.zh-CN.md)：每一种安装方式、验证下载、更新、卸载
- [CONTRIBUTING.md](CONTRIBUTING.md)（仅有英文版）：从源码构建和运行测试
- [HunterGrey-cyber/neovide](https://github.com/HunterGrey-cyber/neovide)：Neovide 的 fork（`neovibe-integration` 分支）
- [HunterGrey-cyber/verdandi](https://github.com/HunterGrey-cyber/verdandi)：agent sidecar

Eitri 的开发有 Claude 协助。
