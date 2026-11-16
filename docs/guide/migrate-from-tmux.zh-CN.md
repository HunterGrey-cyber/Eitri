[English](migrate-from-tmux.md) | 简体中文
<!-- translated-from: migrate-from-tmux.md sha256=2f9876799c2fe2ea62027074e0e5b66591446ecea04255c6f15d54a74db54c34 -->

# 从 tmux 迁过来

如果你每天都在 tmux 里干活：一个窗格开 Neovim，一个窗格跑 `claude`（或 `codex`），旁边再开一两个 shell，这一页就是写给你的。下面说明 Eitri 会原样接过你现有配置里的哪些东西，哪些用法会变，哪些带不过来，以及想退回去时怎么办。

<a id="what-stays-and-what-changes"></a>
## 哪些不变，哪些会变

不变的：

- **Neovim。** 编辑器就是你自己的 `nvim`，用你的 `init.lua`、你的插件和你的键映射。
- **你的 tmux 按键。** Eitri 启动时会读你的 tmux 配置。只要 Eitri 有对应的动作，你的前缀和前缀后面绑的键用起来就和在 tmux 里一样。
- **你的 agent。** Claude 标签页运行的是你自己装的 `claude`，会话会加载你自己的 Claude Code 设置、hook、skill、插件和 `CLAUDE.md`。

会变的：

- **从终端换成桌面窗口。** Eitri 是一个 GTK 窗口，编辑器由 Neovide 渲染器的一个分支来画。agent 的回复显示在编辑器旁边的面板里，是渲染好的 Markdown，带 diff 和权限卡片。
- **窗格变成模块，分屏时要说明开哪个。** 窗口由模块组成：编辑器、聊天视图和终端。按下分屏键之后还要再按一个键，指定那里放哪个模块：`e`、`a` 或 `t`。
- **tmux 的 window 对应标签页。** `<prefix> c`、`n`、`p` 和数字键操作的是 agent 面板的标签页。
- **没有 detach。** Eitri 没有可以重新连上的服务进程，关掉窗口，它的终端里的 shell 也就结束了。

<a id="before-you-start"></a>
## 开始之前

Eitri 需要 Neovim 0.11 或更新版本。其他依赖和安装方法见[安装](../../INSTALL.zh-CN.md)，在哪些桌面上试过见[已知问题](../known-issues.zh-CN.md)。

<a id="your-neovim-as-it-is"></a>
## 你的 Neovim，原样就能用

在项目目录里启动 Eitri：

```sh
eitri ~/src/myproject
```

从终端启动时，窗口一起来 `eitri` 就把提示符还给你，窗口自己接着运行：关掉启动它的那个 tmux 窗格，窗口还在。

你的 Neovim 配置不用为 Eitri 加任何一行。在 Eitri 的编辑器里 `vim.g.neovide` 为真，所以你给 Neovide 写的 `if vim.g.neovide then ... end` 在这里同样生效；想单独给 Eitri 写一段，可以判断 `vim.g.eitri`。

有几个键会先被 Eitri 拿走，Neovim 收不到：

| 键 | 在 Eitri 里 | 要交给 Neovim 时 |
|---|---|---|
| `Ctrl+b`（或你 tmux 里的前缀） | 前缀 | 连按两次：`Ctrl+b Ctrl+b` 是 Neovim 的向上翻页。前缀是 `Ctrl+a` 时，tmux 配置里要有 `bind C-a send-prefix` 才能这样用，和 tmux 一样 |
| `Ctrl+h`、`Ctrl+j`、`Ctrl+k`、`Ctrl+l` | 在模块之间移动；在编辑器里先在 Neovim 自己的窗口之间移动 | `<prefix> Ctrl+h`（其余同理） |
| `Ctrl+=`、`Ctrl+-`、`Ctrl+0` | 窗口里所有模块的字号 | 你自己映射的 `<C-=>` 不会触发 |
| `F11` | 全屏 | |

- **vim-tmux-navigator 不用改。** 在 Neovim 最右边按 `Ctrl+l` 会进到右边的模块，在最下边按 `Ctrl+j` 会进到下面的终端。其他导航插件见[在窗格之间移动](keys.zh-CN.md#moving-between-panes)。
- **没装导航插件时**，Eitri 自己给 `Ctrl+h/j/k/l` 加上同样的行为，但只加在你的配置没有另作他用的键上。
- **会话插件。** 如果你用 persistence.nvim、auto-session、resession.nvim 或别的 Eitri 认得的会话插件，重新打开文件的事就交给插件，Eitri 不插手。
- **快速排查。** `eitri --clean` 用 `nvim --clean` 启动编辑器，方便判断问题是不是出在你自己的配置上。

Eitri 自己的设置在另一个文件里：`~/.config/eitri/init.lua`（设了 `$XDG_CONFIG_HOME` 时在它下面）。你 Neovim 的 `init.lua` 不用动。

<a id="your-tmux-prefix-and-keys"></a>
## 你的 tmux 前缀和按键

Eitri 按 tmux 自己的顺序读 tmux 会读的文件：`/etc/tmux.conf`、`~/.tmux.conf`、`$XDG_CONFIG_HOME/tmux/tmux.conf` 和 `~/.config/tmux/tmux.conf`，以及这些文件用绝对路径 `source-file` 进来的文件。它只把这些文件当文本读，不会启动 tmux，也不会去问正在运行的 tmux。导入默认是开着的。

第一次导入时，以及之后导入结果有变化时，窗口顶部会出现一条提示，说明接过了什么：

```text
Using your tmux keys from ~/.tmux.conf: prefix Ctrl+a, 8 keys (18 skipped). Ctrl+a ? lists them; new-window opens a new tab. keymap.from_tmux = "off" turns this off.
```

之后按 `<prefix> ?` 会列出所有按键，从你配置里来的键后面标着 `(tmux)`。列表最后，"Skipped from tmux" 一节列出每一行没接过来的配置，带文件名、行号和原因；"Imported from tmux with changes" 一节列出接过来了、但丢掉了一部分选项的绑定。

能接过来的：

- `set -g prefix C-a`（任何 Ctrl 或 Alt 组合键，或者功能键都行），以及绑定到 `send-prefix` 的键。
- `bind -r`：半秒之内不用再按前缀就能重复按，和 tmux 一样。
- `split-window -h` 和 `-v`。按下后还会等一个模块键，比如你的分屏键是 `|`，那么 `<prefix> |` 再按 `t` 就开一个终端。带 `-c "#{pane_current_path}"` 时，新 shell 从被分屏的那个终端所在的目录启动。`-l 30%` 这样的尺寸不生效：新模块仍然占一半，这个绑定会列在 "Imported from tmux with changes" 里。带 `-b`（放在当前窗格前面）或 `-f`（占满整个窗口）的绑定会被跳过。每个选项的处理方式见[你的 tmux 键位](keys.zh-CN.md#your-tmux-keys)。
- `select-pane -L/-R/-U/-D`、`-l`、`-m` 和 `-M`；`last-pane`；带方向和格数的 `resize-pane`，以及 `-Z`（放大）；`swap-pane -U/-D`；`rotate-window`；`select-layout` 的 `even-horizontal`、`even-vertical`、`main-horizontal`、`main-vertical`、`tiled` 和 `-E`；`next-layout`；`display-panes`；`kill-pane`。
- `new-window`、`next-window`、`previous-window`、`last-window`、`kill-window` 和改名用的 `rename-window` 提示框，操作的都是标签页。`new-window` 新开一个标签页，提示里也会这么说。`break-pane` 把这个标签页拆出来，放进一个单独的聊天视图。
- `switch-client -n`、`-p` 和 `-l` 在工作区之间切换，`choose-tree -s` 选择工作区；不带 `-s` 的 `choose-tree` 打开会话列表。
- `copy-mode` 和 `copy-mode -u`、`unbind`、`unbind -a`。`unbind -a` 之后，Eitri 自己的键还在：模块键、HINT、`?`、重新加载面板，以及 tmux 没有对应命令的那些标签页键。

会被跳过的（原因见 `<prefix> ?`）：

- **根键**（`bind -n`、`-T root`）：不按前缀时，按键归 Neovim 和面板。要绑这种键，在 Eitri 的 `init.lua` 里用 `eitri.keymap.set("root", ...)`。vim-tmux-navigator 的那几行 `bind -n C-h if-shell ...` 就是因此被跳过的；用 `Ctrl+h/j/k/l` 在模块之间移动由 Eitri 自己负责。
- **复制模式的键表**（`bind -T copy-mode-vi ...`），以及 `prefix` 以外的所有键表。
- **Eitri 没有对应功能的命令**：`display-popup`、`display-menu`、`paste-buffer`、`choose-buffer`、绑定里的 `source-file` 等。
- **要运行 shell 的**：`run-shell` 和 `if-shell`。Eitri 从不执行 tmux 配置里的命令，所以 TPM、它加载的插件以及这些插件绑的键都带不过来。
- **一个键绑了多条命令的**、`send-keys` 发 `C-a` 以外的任何键、`select-window`（Eitri 的 `<prefix> 1` 到 `9` 已经用来选标签页，从 1 开始数）、`prefix2`、不带 `-g` 的 `set prefix`，以及用相对路径的 `source-file`（tmux 会按一个 Eitri 无从得知的目录去解析它）。
- 和按键无关的选项，比如 `mouse`、`status`、颜色和 `base-index`，直接忽略，也不会列出来。

要关掉导入，在 Eitri 的 `init.lua` 里写：

```lua
eitri.config.set("keymap.from_tmux", "off")
```

不管导入开没开，你在这个文件里写的 `eitri.keymap` 调用都优先于从 tmux 导入的键。关掉导入后，`Ctrl+a` 前缀写成 `eitri.keymap.prefix("C-a")`；[`docs/keymap/tmux-ctrl-a.lua`](../keymap/tmux-ctrl-a.lua) 是一份可以直接拿来抄的完整例子，[tmux 配方](../../examples/environments/tmux/README.zh-CN.md)是给常驻 tmux 的人准备的一份 `init.lua`。

第一次启动、还没有 Eitri 自己的 `init.lua` 时，会出现一个设置界面，列出它找到的 tmux 配置，并提供一个保持导入开启的 "Neovim + tmux" 预设（见[首次启动](configuration.zh-CN.md#first-start)）。

在 Hyprland、sway 和 niri 上，Eitri 默认把聊天和页面放进你窗口管理器的窗口里。在 Eitri 的 `init.lua` 里写 `window.tiling = "eitri"`，所有东西就都留在 Eitri 的窗口里。两种情况下各有哪些键能用，见[合成器窗口](compositor-windows.zh-CN.md#keys)。

<a id="your-agent-and-its-configuration"></a>
## 你的 agent 和它的配置

Claude 标签页运行 `PATH` 上的 `claude`，找不到时用 Claude Code 安装程序放在 `~/.local/bin` 里的那个；它会去找的所有位置见[快速上手](getting-started.zh-CN.md)。每个会话都加载你自己的 Claude Code 配置：你的设置、hook、插件、skill、`CLAUDE.md` 和权限规则。`eitri.config.set("agent.user_settings", false)` 可以不加载它们。

项目自带的配置（`.claude/`、`.mcp.json`、`CLAUDE.md`）要等你信任这个项目之后才加载：这个项目里的第一个会话会先问你，按 `y` 就加载。`<prefix> i` 显示一个标签页的会话加载了哪些配置。在标签页里按 `<leader>t`，会给出一条 `claude --resume ...` 命令，用它可以在终端里接着这段对话。

你也可以照旧在终端模块里跑 `claude`。那里就是一个普通的 shell，那个 `claude` 保留它自己的提示和模式。Codex 和支持 ACP 的 agent 也可以开标签页，见 [Codex 与其他 agent](agents.zh-CN.md)。

<a id="tmux-to-eitri"></a>
## tmux 和 Eitri 的对应关系

| tmux | Eitri |
|---|---|
| 窗格（pane） | 模块：编辑器、聊天视图（`agent`、`agent#2` … `agent#9`）或终端（`terminal`、`terminal#2` … `terminal#9`） |
| `%`、`"`（分屏） | `<prefix> %` 或 `"`，再按 `e`、`a` 或 `t` |
| `z`、`x`、`o`、`;`、`q`、`{`、`}`、`Space`、`M-1` … `M-5` | 同样的键，作用在模块上 |
| 窗口（window） | 标签页：`<prefix> c`、`n`、`p`、`l`、`,`、`&`、`1` … `9` |
| `w`（选 window） | `<prefix> w`：打开着的标签页和保存过的会话 |
| 会话（session） | 项目：`<prefix> O` 选一个 |
| `switch-client -n/-p/-l`、`s`、`$` | 工作区：`<prefix> (`、`)`、`L`、`s`、`$`；工作区是同一组模块的另一种摆法 |
| `d`（detach） | 没有。`<prefix> d` 是隐藏一个模块，会等你按 `e`、`a` 或 `t`；`Esc` 取消 |
| `[`（复制模式） | 在终端里按 `<prefix> [`：往回翻 |
| `display-popup` | 浮动窗口或便笺板：`<prefix> F`、`<prefix> _`、`<prefix> S` |
| `?` | `<prefix> ?`，包括 tmux 导入时没接过来的行 |

**从终端里复制。** 复制模式只能往回翻，目前还不能选中，也不能搜索。要复制就用鼠标：拖动、双击或三击（选中整行，连同行尾的换行）选中，松开时文字同时进入剪贴板和主选区。`Ctrl+Shift+V` 粘贴，鼠标中键粘贴主选区。复制模式的按键和其他细节见[容器和终端](keys.zh-CN.md#containers-and-terminals)。

**弹出式终端。** 想要 `display-popup` 那样一个键呼出的下拉终端，在 Eitri 的 `init.lua` 里给 `scratchpad.show` 指定一个模块：

```lua
eitri.keymap.set("prefix", "F2", "scratchpad.show", { module = "terminal#2" })
```

每按一次会发生什么，见[浮动窗口、便笺板和工作区](keys.zh-CN.md#floats-the-scratchpad-and-workspaces)。

<a id="what-does-not-carry-over"></a>
## 带不过来的东西

- **tmux 插件。** tmux 配置里要运行的东西一律不运行，所以 TPM、tmux-resurrect、tmux-continuum、主题，以及它们绑的键都没有。
- **detach，以及比窗口活得更久的服务进程。** 关掉窗口，它的终端就结束了。有终端正在跑命令时，会先问你（`close window? 1 terminal job (y/n)`）。
- **重启后终端里的内容。** 下次在这个项目里启动时，模块的摆法会回来，终端还在原来的位置，但里面是一个新的 shell，之前的输出都没有了。你打开过的文件会在 Neovim 里按原来的分屏重新打开，除非你用 `eitri --clean` 启动，或者由会话插件负责重新打开。agent 标签页会以 `Restore last session` 的形式提供，按 `s` 就连同对话一起恢复。各项设置分别管什么，见[容器和终端](keys.zh-CN.md#containers-and-terminals)。
- **另一台机器上的 tmux。** 在终端模块里可以像在任何终端里一样用 `ssh`，但 Eitri 不会连到任何 tmux 上，本机的和远程的都不会。
- **tmux 配置里的根键绑定和复制模式绑定**（见上文）。
- **`base-index`。** 标签页从 1 开始编号。
- **状态栏。** tmux 的状态栏和它的格式不会被读取；Eitri 有自己的顶栏。

终端是 Eitri 自己的，默认用你 Neovim 的 `guifont` 来画，`terminal.font` 可以另外指定。

<a id="going-back-or-meeting-halfway"></a>
## 退回去，或者各用一半

有两种办法能继续用你终端里的 Neovim：

- **把面板放在你的 Neovim 旁边**（[Companion 模式](companion.zh-CN.md)）。在终端里的 Neovim 中执行 `:EitriPanel`（来自 Eitri 自带的 `eitri.nvim` 插件），agent 面板会作为一个独立窗口打开，并连到这个 Neovim 上。目前试过哪些情况见[已知问题](../known-issues.zh-CN.md#companion-mode)，Neovim 跑在 tmux 里的情况还没有试过。
- **把正在运行的 Neovim 搬进来**（[接管终端里的 nvim](takeover.zh-CN.md)）。在 Neovim 0.12 或更新版本里执行 `:EitriOpen`，就把这个 Neovim 搬进一个 Eitri 窗口，窗口结束时再还给终端。

不想用 Eitri 了，关掉它的窗口就行。你的 tmux 配置只被读过。Eitri 自己的文件是 `~/.config/eitri/` 里的 `init.lua`，以及它存在 `~/.local/state/eitri/` 里的东西。

## 接下来

- [快速上手](getting-started.zh-CN.md)：第一个回合、权限卡片、标签页和会话。
- [按键](keys.zh-CN.md)：所有按键，以及[你的 tmux 键位](keys.zh-CN.md#your-tmux-keys)。
- [工作环境](environments.zh-CN.md)：tmux 配方和其他配方。
- [已知问题](../known-issues.zh-CN.md)：目前还粗糙或没测过的地方。
