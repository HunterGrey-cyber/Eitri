[English](README.md) | 简体中文
<!-- translated-from: README.md sha256=29f9e48000f9255ef5a7197f64e5b881c9e443cb5c35f80ca8cc9ea28e44a6bc -->

# Eitri

**你完整的 Neovim，未经改动，旁边配一个真正丰富的 agent 面板。两边都是真实的，外壳并不拥有任何一边的内部实现。**

把 AI agent 放到代码旁边，通常有两种普通做法，每一种都要在另一半上付出代价。把 agent 跑在终端里，你的编辑器毫发无损，但它说的一切都以纯文本形式出现在 PTY 里——markdown 不会被渲染，diff 你只能当作加减号行来读。换成某个 AI 编辑器 fork，面板确实做得不错，但内置的"Vim 模式"只是别人有空重新实现了哪些部分，就模拟哪些部分。

Eitri 拒绝这种取舍。编辑器面板走的是 Neovide 自己的管线——键盘和鼠标 → Neovim → Neovide 的渲染器 → Skia → GPU——驱动一个真正的 `nvim --embed`，加载你真实的配置，带着你的插件、你的 LSP、你的 Treesitter 和你的按键映射。agent 面板是跑在 WebKitGTK 里的 React 应用，因为渲染 markdown、diff 和工具输出，这依然是最合理的方式。两边都不是对方的重新实现。

![等待处理的权限卡片，旁边是真实的 Neovim](demo/permission-card.png)

## 唯一的原则

**把 Neovide 组件化，而不是虚拟化。** 没有终端模拟器中转，没有 WebView 中转，没有帧缓冲合成，也没有和 Neovim 缓冲区同步的第二套文本模型。Neovide 被 fork 出来并做了最小化的重构，成为一个可嵌入的 surface；fork 的改动始终停留在 surface、几何、input-adapter 和 renderer-viewport 相关的代码里，这样才能持续跟上游合并。

```
GTK4 外壳（gtk4-rs）
├── chrome        顶部栏、托盘、分隔线 — GTK 自身的外观已被禁用
├── editor pane   GtkGLArea → Skia → Neovide 渲染器 → 真实的 `nvim --embed`
├── agent panel   WebKitGTK → React/TypeScript — markdown、工具调用、diff、权限卡片
└── terminal      进程内 PTY → 终端状态 → Skia（是一个 shell，不是编辑器的中转）
```

- **GTK** 提供窗口管理、布局、焦点、输入法（IME）和 WebKitGTK 容器。它不负责视觉风格：外壳由 Eitri 自己绘制，并跟随你的 Neovim 配色方案。
- **Neovim** 继续拥有缓冲区、动作（motion）、LSP、补全、Treesitter 和渲染。外壳从不重新实现其中任何一项。
- **Agent 状态保存在 Rust 宿主进程里，而不是 WebView 中。** 面板只是对 Rust 中持有的一份投影（projection）的纯视图，因此重新加载或 WebView 崩溃都不会丢失任何东西。

## 它能做什么

- **编辑器和 agent 彼此感知。** 你当前的文件、光标位置和可视选区都会作为上下文进入这一轮对话；agent 提出的编辑会以逐行 diff 的形式显示在它的权限卡片上；agent 写入的缓冲区会在编辑器里重新加载。
- **只有真正需要人来判断时才会询问的权限机制。** 在 Auto 模式下，面板会自动放行那些 Claude Code 自身交互式默认模式本就会无需询问就允许的操作——项目内的读取、只读的 shell 命令——其余一律弹出卡片。任何不确定的情况都会倒向弹卡片这一边：没有 shell 解析器，所以重定向、管道或命令替换都会询问；未知的工具会询问；解析结果落在项目之外的路径会询问，`..` 和符号链接也不例外。
- **键盘优先。** `Ctrl-h`/`Ctrl-l` 可以在不破坏你的分屏布局的前提下在面板之间移动（内嵌的 Neovim 相信自己身处 tmux 之中，所以由 `vim-tmux-navigator` 来判断边界）。面板本身是一份文档，带有 `BROWSE` 模式：`j`/`k`、`h`/`l`、用 `a`/`d` 批准或拒绝、`G`/`gg`，还有你的 Neovim `mapleader` 配合 which-key 提示框。`f` 会在窗口里的每一个目标上打上跳转标签。
- **会话标签页。** 每个标签页对应一段对话，所有标签页同时运行，由 tmux 的窗口按键驱动。
- **布局即数据。** 编辑器、面板和终端都是一棵树里的模块：用 tmux 的按键隐藏、关闭、拆分、交换和放大；项目重新打开时会保持你离开时的样子。
- **一个底部终端。** 拥有自己的 PTY，用 Skia 绘制，支持输入法、括号粘贴（bracketed paste）和 OSC 52 复制、回滚（`Shift-PageUp`/`PageDown`、滚轮）、鼠标选择加中键粘贴，以及向请求了鼠标上报的程序（`nvim`、`htop`、`less`）报告鼠标事件。
- **它是继承，不是强加。** 在 Neovim 里执行一次 `:colorscheme`，外壳、编辑器和面板都会跟着变。窗口按键默认使用原生 tmux 的设定（前缀键 `Ctrl-b`），可以在 `~/.config/eitri/init.lua` 里重新绑定，这是一个真正的 Lua 内核（`eitri.keymap`、`eitri.command`、`eitri.panel`、`eitri.config`）。两个按键绑定冲突时，启动会报错并指出这两个绑定分别是什么。一个 `Ctrl-a` 的配置示例在 [`docs/keymap/`](docs/keymap/) 里。

![Eitri 跟随你的配色方案](demo/colorscheme.gif)

## 现状 —— 使用前请先读这一节

- **Wayland 上的 Linux 是主要且唯一经过验证的平台。** 开发环境是 GNOME 和一个 headless sway；X11 还没有试过。不依赖 GTK 的那部分 crate 正在移植到 macOS，但应用本身目前还不能在那上面运行。
- **其中大部分只在一个隔离的 GUI 沙盒里验证过**，用的是 headless compositor 上的合成输入。上面列出的每一项都存在并且有测试；但在真实硬件上、由真人操作检查过的部分要少得多。请预期会有一些粗糙的地方。
- 可行性问题（Neovide 的渲染器脱离自己的窗口、旁边放一个浏览器引擎而不拖累编辑器、输入法、小数缩放、fork 是否还能合并）都各自对照一份事先写好的 Go/No-Go 标准测试过，结果都是 Go，其中几项附带了书面条件。动工之前的两份设计文档在 [`docs/`](docs/) 里，中文原版在 [`docs/zh/`](docs/zh/) 下。
- Agent 这一侧只对接 Claude Code，不是一个多提供商的客户端。

## 工作区

仓库根目录下是一个 Cargo workspace。

| crate | 是什么 |
|---|---|
| `shell/` | **应用程序二进制本体。** 窗口、外壳、焦点、模块网格、Lua 内核、主题跟随 |
| `neovide-editor/` | 库：`NeovideEditorPane`，跑在 `GtkGLArea` 上的、fork 出来的 Neovide 渲染器、输入和运行时 |
| `agent/` | 库：Claude 会话后端（不依赖 GTK）。同时构建出 `agent-hook` 和 `eitri-claude-handoff` |
| `core/` | 库，包名为 **`eitri-core`**：外壳中不依赖 GTK 的那一半——后端选择、Lua 内核、主题 token、每窗口的 socket、布局、按键映射、会话标签页 |
| `agent-ui/web/` | 面板的 React/TypeScript 前端。不是一个 crate：`shell/build.rs` 用 npm 构建它，并把构建出的单文件结果内嵌进去 |
| `supervisor/` | `eitri-supervisor`，一个只读的跨窗口 agent 状态看板 |
| `terminal/` | 包名为 **`eitri-terminal`**：底部终端的引擎——PTY、会话线程、终端状态、Skia 绘制 |
| `terminal-render/`、`terminal-frame/`、`terminal-sync/`、`terminal-input/` | 终端的绘制列表、帧投影、同步更新屏障和按键编码器。`terminal-input` 使用 Apache-2.0 许可（派生自 Alacritty）；见它自己的 `NOTICE` |
| `neovide/` | 子模块：Neovide 的 fork（[HunterGrey-cyber/neovide](https://github.com/HunterGrey-cyber/neovide)，分支 `neovibe-integration`） |
| `poc/` | 来自可行性验证阶段、已冻结的概念验证 crate。不是产品本身；它们特意保留了那些只在产品 crate 里修复过的缺陷 |

### Verdandi 依赖

Agent 通过一个 **sidecar** 与 Claude Code 通信，这是姊妹项目 [Verdandi](https://github.com/HunterGrey-cyber/verdandi)（`apps/claude-sidecar`）提供的一个 Node 服务，通过每窗口一个的 Unix socket 走 gRPC。`agent` 依赖 Verdandi 的 `claude-runtime-protocol` crate，在 `agent/Cargo.toml` 里按 revision 锁定，由 Cargo 从 GitHub 拉取；**构建** Eitri 不需要 Verdandi 的其他任何东西。

要*运行* sidecar 后端，Eitri 需要找到 sidecar 本体，按这个顺序查找：先看 `EITRI_SIDECAR_BINARY`（一个已构建好的可执行文件）；如果没设置，再看 `EITRI_VERDANDI_CHECKOUT`（指向某个 Verdandi checkout 的开发用覆盖项）；然后是 `shell` 二进制旁边的 `verdandi-claude-sidecar`（各个软件包和 AUR 构建产生的就是这个）；再然后是 `eitri setup` 为你这个用户构建出的产物，位于 `$XDG_DATA_HOME/eitri/sidecar/<pinned revision>/`（未设置时是 `~/.local/share/…`），以本次构建锁定的确切 revision 为键，这样旧版本构建出的东西永远不会被更新的版本误用；最后是默认路径 `~/src/verdandi` 下的一个 Verdandi checkout，但前提是那里已经放着一个、并且已经构建过东西。这里的任何一步都不会自己主动去构建任何东西——为什么这个构建必须在你自己的机器上进行、以及要通过 `eitri setup` 来做，见[安装](#install)一节。

**任何发行版本都不包含旧版（legacy）后端**，也就是以前那种直接在你的 `PATH` 上拉起 `claude` 的做法。它仍然留在代码树里，作为一个仅供开发使用的构建选项（`cargo build --features shell/legacy-backend`），供本地测试和项目自己的沙盒 GUI 验证使用；发行构建无法选择它（在发行版本上执行 `eitri --legacy` 会以退出码 1 失败，并说明原因）。不管是 sidecar 还是旧版开发构建，你都需要安装并登录 Claude Code；Eitri 花的是它所运行账号下的用量。

<a id="install"></a>
## 安装

最快的方式——通过 HTTPS 下载并运行安装脚本，安装到 `~/.local`，不需要 `sudo`：

```sh
curl --proto '=https' --proto-redir '=https' --tlsv1.2 -sSfL https://github.com/HunterGrey-cyber/eitri/releases/latest/download/install.sh | sh
```

把脚本接进 shell 之前，可以先读一遍，或者先对照签名过的校验和检查一下——完整的"运行前先验证"流程、验证能保护什么、不能保护什么，以及其他每一种安装方式的详细说明，见 [`INSTALL.zh-CN.md`](INSTALL.zh-CN.md)：

- **`.deb`/`.rpm`**：从[发布页面](https://github.com/HunterGrey-cyber/eitri/releases)下载，然后运行一次 `eitri setup`——它会为这台机器构建 sidecar，如果你的 `nvim` 太旧，还会主动提出为你获取一份私有的 `nvim`。
- **AUR**：`yay -S eitri-bin`（预构建二进制，sidecar 在安装时构建）或 `yay -S eitri-git`（包括 Eitri 本身在内的一切都从源码构建）。
- **`--from-source`**：`sh install.sh --from-source` 会在你自己的机器上构建 Eitri——需要的工具链见[构建](#building)。**0.2.0 只支持 x86_64**；`--from-source` 在其他任何 `uname -m` 上也会拒绝运行，所以它并不是一条 ARM 路径。

**环境要求**：GTK ≥ 4.14、WebKitGTK 6.0、glibc ≥ 2.39、x86_64 Linux——不管是预构建二进制还是从源码构建，都是这个要求。**nvim ≥ 0.10**——可以是你自己 `PATH` 上的那份，也可以让安装脚本为 Eitri 单独获取一份私有副本（`$XDG_DATA_HOME/eitri/nvim/<version>/`，永远不会放上 `PATH`，也永远不会触碰、替换或删除系统里其他任何叫 `nvim`/`vim`/`vi` 的东西）。还需要安装并登录 Claude Code。

- Ubuntu 23.10 及以后版本（含 24.04）：默认桌面的 AppArmor 策略会拦截 WebKitGTK 的沙盒，所以在你应用一次性修复之前，代理面板的位置会显示修复步骤，而不是面板本身。见 [INSTALL.zh-CN.md 的 "Ubuntu 23.10 及以后版本" 一节](INSTALL.zh-CN.md#ubuntu-2310-and-later)。
<!-- /ubuntu-userns -->

**为什么 sidecar 要在你自己的机器上构建，而不是随包分发。** 它打包了 Anthropic 的 `@anthropic-ai/claude-agent-sdk`（"© Anthropic PBC. All rights reserved"），我们不对它进行再分发。`eitri setup`（以及安装脚本的其他路径，还有 AUR 软件包自己的 `build()`）会把它从 npm 仓库拉到你的机器上，遵循 Anthropic 自己的条款，并基于锁定的 Verdandi revision 构建出 sidecar。sidecar 本身不出现在任何 Eitri 发行产物里。

<a id="building"></a>
## 构建

```sh
git clone --recurse-submodules https://github.com/HunterGrey-cyber/eitri.git
cd eitri
# 克隆时没加 --recurse-submodules？运行 git submodule update --init
cargo build --locked
```

`cargo build --locked`——**不是** `--workspace`——才是产品本体：也就是 workspace 的 `default-members`（`shell`、`neovide-editor`、`agent`、`core`、`supervisor` 以及终端相关的 crate），锁定在 GTK 4.14 这条底线上。`--workspace` 还会连带构建被冻结的 `poc/` crate，它们锁定的是更新的 GTK（4.18），并不属于发行的内容。

下面这些前置条件全部都是硬性要求——少了任何一个都会导致构建失败，而且失败往往发生在某个第三方构建脚本内部，报错里根本不会提到本项目的名字：

- **Rust 1.96 或更新版本**，以支持 fork 使用的 2024 edition（安装脚本会拒绝更旧的版本）。
- **GTK 4.14+ 和 WebKitGTK 6.0 的开发包**，以及 `pkg-config`（Arch：`gtk4 webkitgtk-6.0`；Debian/Ubuntu：`libgtk-4-dev libwebkitgtk-6.0-dev`）。
- **Node.js 和 npm**：`shell/build.rs` 会在 `agent-ui/web/` 里执行 `npm ci` 和 `npm run build`。
- **`protoc`**（`protobuf`/`protobuf-compiler`）：Verdandi 的协议 crate 用它来生成类型。
- **一套 C 工具链**：Lua 5.4 是从源码构建并静态链接进去的。
- **第一次构建需要联网**：`skia-safe` 会下载一份预构建的 Skia，Cargo 也会从 GitHub 拉取 Verdandi 的协议 crate。
- 运行时还需要：`PATH` 上的 **`nvim`**（或者 Eitri 自己的私有副本，见[安装](#install)）、一个可用的 OpenGL 驱动、一个 Wayland 会话，以及 Claude Code。

## 运行

```sh
cargo run -p shell -- /path/to/project     # 或者：EITRI_PROJECT_DIR=...，或者当前目录
cargo run -p shell -- --clean /path        # 以 --clean 启动内嵌的 nvim
```

项目目录只会被解析一次，编辑器、agent 和终端共享这一份解析结果。传入一个不是目录的路径，或者一个未知选项，会导致启动失败并报错，而不是被默默忽略（`--` 用来结束选项解析）。每一次启动都是独立的进程，带着自己独立的 `nvim`。

如果你已经安装了 Eitri（见[安装](#install)），就用 `eitri [项目目录]` 来运行它。本仓库里的 `./try-eitri.sh` 会启动一个源码构建版本，并在一开始就说明用的是哪个后端、哪个账号。配置文件在 `~/.config/eitri/init.lua`（`EITRI_CONFIG_DIR` 可以覆盖这个路径）；每个项目自己的状态（布局、历史记录、已保存的权限规则）保存在 `$XDG_STATE_HOME/eitri/` 下。

只运行编辑器组件，不带外壳：`cargo run -p neovide-editor --example standalone`。

## 按键

一切默认都是原生 tmux 的设定：前缀键 `Ctrl-b`，然后接一个常见的 tmux/vim 风格按键（`c` 新建一个会话标签页、`t` 打开底部终端、`z` 放大当前拥有按键的面板、`x` 关闭它、方向键配合前缀键在面板间移动）。面板导航本身不需要前缀键——就是前面提到的 `Ctrl-h`/`j`/`k`/`l`，vim-tmux-navigator 的风格。这些都可以在 `~/.config/eitri/init.lua` 里重新绑定（`eitri.keymap`）；两个绑定冲突时，启动会报错并指出这两个绑定分别是什么。

**`前缀键 ?` 会显示你这份构建当下绑定的每一个按键，并且已经应用了你自己的重新绑定。** 对于一份仍在持续增长的按键映射来说，权威来源是那个浮层，而不是这份 README。

## 测试

```sh
cargo test                                  # 非 #[ignore] 的测试；把 XDG_STATE_HOME 设到一个临时目录
cd agent-ui/web && npm ci && npx vitest run
```

`cargo test` 在 GTK 4.14 这条底线上运行产品本体，和上面的构建一致；`cargo test --workspace` 还会额外构建并测试被冻结的 `poc/` crate，这需要 GTK 4.18+。标了 `#[ignore]` 的测试需要一个显示环境、一个真实的 `nvim`，或者一个真实的 Claude 账号（并会产生费用）；每一个都在自己的文档注释里说明需要什么。

## 许可证

MIT——见 [`LICENSE`](LICENSE)，`terminal-input/` 除外，它是 Apache-2.0（派生自 Alacritty；见它自己的 `NOTICE`）。构建在 [Neovide](https://github.com/neovide/neovide)（MIT）的一个 fork 之上，Neovide 的架构和代码直接塑造了编辑器这个组件。

`shell` 静态链接了 [`nvim-rs`](https://crates.io/crates/nvim-rs) 0.9.2，它是 **LGPL-3.0** 许可。每个发行版本都会在同一个发布页面上，以单独的附件形式附带对应源码（Corresponding Source），即 `eitri-<version>-source.tar.gz`（构建所用的完整公开代码树、`neovide` fork、`cargo vendor` 的输出，以及锁定版本的 Skia 归档），还有一个 `SOURCE` 文件（安装好的软件包里位于 `/usr/share/licenses/eitri/SOURCE`），说明这个附件是什么，并给出修改 `nvim-rs` 并把它重新链接进发行二进制的确切方法。

`packaging/collect-licenses.py` 会生成每个软件包都会附带的完整第三方声明文件 `THIRD-PARTY-LICENSES`。**sidecar——这里唯一会接触到 Anthropic Claude Agent SDK 的部分——不出现在任何 Eitri 发行产物里**；为什么以及它改在哪里构建，见[安装](#install)。
