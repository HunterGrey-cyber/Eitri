[English](configuration.md) | 简体中文
<!-- translated-from: configuration.md sha256=687daecb5d974bfb5c2fb3d7aa30231237fe9ba002ce3b66aedb9354dd9570cd -->

# 配置

Eitri 有一个配置文件，是一个在启动时运行一次的 Lua 脚本；另外还有少数几个环境变量，用于某个设置属于 shell 或启动器、而不属于你本人的情形。这一页列出每一个设置、它的作用和默认值。某个设置会改变按键时，这一页会说明，并指向[按键](keys.zh-CN.md)。

## init.lua

Linux 上，若 `XDG_CONFIG_HOME` 非空且为绝对路径，Eitri 读取 `$XDG_CONFIG_HOME/eitri/init.lua`，否则读取 `~/.config/eitri/init.lua`。Mac 始终使用 `~/.config/eitri/init.lua`。把 `EITRI_CONFIG_DIR` 设成绝对路径，就改从那里读取 `init.lua`；空值视为未设置，相对路径会拒绝启动。不需要先创建文件：没有它，Eitri 用内置的默认值启动，并打印一行说明这一点。没有 `init.lua` 的第一次启动，它还会提议替你写一个很短的文件：见[首次启动](#first-start)。

设置用 `eitri.config.set(key, value)` 来做。值是字符串、数字或布尔值（`true` 或 `false`）；`nil` 会把这个键重新取消设置。Eitri 保存不了的值（比如表或函数）会被拒绝：这次调用什么也不做，Eitri 在启动时把它列出来。

```lua
eitri.config.set("agent.restore", "auto")
eitri.config.set("review.hint", true)
```

下面每个设置都遵循三条规则：

- 设置不接受的值既不会让窗口停下，也不会悄悄回退。这个设置回到它的默认值，Eitri 在窗口打开时说一声：文件和行号、这个键、它接受什么。有两个例外。`agent.account` 仍然会让启动停下，因为窗口换到别的账号，会把会话写到那个账号里。另外，决定 agent 能做什么、加载什么的设置，会取更严格的一边，而不是默认值：`agent.default_mode` 写错，新标签页从 Manual 开始；`agent.user_settings` 写错，不加载你自己的 Claude Code 配置；`agent.editor.tools`（或 `agent.editor.tool.<name>`）写错，对应的编辑器工具关闭（工具名不存在时，全部关闭）。Eitri 会说明现在生效的是哪一边，直到你改好这一行为止。`eitri.config.set` 存不下的值，对这几个设置同样处理。
- Eitri 不读的键什么也不做。如果它在 Eitri 的某个键族里（`agent.`、`window.`、`terminal.`、`editor.`、`keymap.`、`review.`、`companion.`、`plugins.`、`modules.`），或者和某个真正的键只差一个笔误，Eitri 会把它列出来，并给出它大概想写的那个键。你自己的键（由你自己的 Lua 用 `eitri.config.get` 读取）不受影响。
- 文件本身的 Lua 错误（比如代码里的笔误）同样不会让启动中止：Eitri 会带着文件在出错之前已经设好的内容打开，并在窗口里说出文件和行号（语法错误会让整个文件都不生效，因为 Lua 在运行之前先读完整个文件）。同一份清单也会写到你启动 Eitri 的那个终端里（从桌面启动时写到 journal）。

Eitri 说的方式是右上角的一条提示：有几个问题，以及前五个，停留 40 秒，多出来的会提示去终端里看。只要问题还在文件里，每次启动都会说。

`require` 只在 `init.lua` 旁边的 `lua/` 目录下找模块：`require("mine")` 加载该配置目录下的 `lua/mine.lua` 或 `lua/mine/init.lua`，`require("a.b")` 加载 `lua/a/b.lua` 或 `lua/a/b/init.lua`。它从不在你打开的项目或当前目录里找，改动 `package.path` 或 `package.cpath` 也没有效果。

两种窗口读的是同一个文件：单窗口模式和 [companion 窗口](companion.zh-CN.md)。

<a id="settings"></a>
## 设置

| 键 | 值 | 默认值 | 作用 |
|---|---|---|---|
| `agent.account` | 一个账号名 | 未设置 | 这个窗口使用的 Claude 账号。见[账号](#accounts)。 |
| `agent.font_size` | 9 到 32 之间的数字 | 编辑器的字号 | agent 面板的基础字号，单位是像素。对话按基础字号的 12/14 绘制（回复、工具行和代码），所以读起来比编辑器的文字略小；面板底部的状态行按基础字号本身绘制，这样它和编辑器的状态栏对齐。不设置时，基础字号就是编辑器（`guifont`）的字号。 |
| `agent.typing_cadence_hz` | 1 到 60 之间的整数，或 `false` / `"off"` | `5` | 你在编辑器里输入时，agent 面板每秒最多允许更新多少次。稳定的较慢频率可以避免面板在你输入时重绘；`"off"` 让它全速更新。 |
| `agent.default` | `"claude"`、`"codex"`，或用 `eitri.agent.register` 注册的 agent 的名字 | `"claude"` | 新标签页，以及新窗口的第一个标签页，运行哪个 agent。见 [Codex 与其他 agent](agents.zh-CN.md)。 |
| `agent.codex.command` | 绝对路径，或裸命令名 | 未设置 | Codex 标签页运行的 `codex`。`EITRI_CODEX` 优先于它；两者都没有时用 `PATH` 上的 `codex`。带目录部分的相对路径会被拒绝，这个设置保持未设置。 |
| `agent.default_mode` | `"manual"`、`"auto"` 或 `"bypass"` | `"auto"` | 新标签页开始时所处的模式。见[一次启动怎样开始](#how-a-launch-starts)。 |
| `agent.restore` | `"offer"`、`"auto"`、`"off"` 或 `false` | `"offer"` | 对这个项目上一个窗口里开着的标签页怎么办。见[一次启动怎样开始](#how-a-launch-starts)。 |
| `agent.thinking` | `"collapsed"`、`"preview"`、`"expanded"` 或 `"hidden"` | `"collapsed"` | agent 面板怎样显示 agent 的思考过程。`"collapsed"` 每段思考画成一行不显眼的行，像工具输出一样用 `Enter` 展开；`"preview"` 在一段思考还在写的时候，另外在一个不会变高的框里显示它最新的几行；`"expanded"` 完整显示每段思考；`"hidden"` 一段也不显示。无论这里怎么设，`Ctrl+o` 都会完整显示每段思考。 |
| `agent.user_settings` | `true` / `"on"` 或 `false` / `"off"` | `true` | agent 会话是否加载你自己的 Claude Code 配置（你的 hook、插件、skill、`CLAUDE.md` 和权限规则），和终端里的 `claude` 一样。`false` 把你自己的配置排除在外；项目自己的配置由[信任](permissions.zh-CN.md)管，不由这个键管。 |
| `agent.editor.tools` | `true` / `"on"` 或 `false` / `"off"` | `true` | 任何标签页里的 agent 是否得到[编辑器工具](editor-tools.zh-CN.md)。`false` 一个都不声明；已经在运行的会话沿用它开始时的工具。 |
| `agent.editor.tool.<name>` | `true` / `"on"` 或 `false` / `"off"` | `true`，`nvim_exec` 是 `false` | 按名字指定其中一个工具：`open_file`、`read_buffer`、`editor_state`、`diagnostics`、`usages`、`hover`、`symbols`、`lsp_rename`、`format`、`lsp_code_actions`、`apply_code_action`、`apply_proposal` 或 `nvim_exec`。每个工具都是开着的，除非你关掉；[`nvim_exec`](editor-tools.zh-CN.md#nvim-exec) 相反，你打开之前一直是关的。Eitri 不认识的名字会在启动时被列出并指出是哪个键，那个开关保持默认。 |
| `agent.editor.follow` | `true` / `"on"` 或 `false` / `"off"` | `false` | 新标签页是否在编辑器里跟随 agent 已完成的修改（[编辑器工具](editor-tools.zh-CN.md#following)）。此后每个标签页自己的选择（`Space e`）优先。值不对会在启动时被列出并指出是哪个键，设置保持关闭。 |
| `editor.restore` | `true` / `"on"` 或 `false` / `"off"` | `true` | 编辑器是否重新打开这个项目的上一个窗口关闭或崩溃时的文件、分屏、标签页和光标位置。`false` 既不读也不写，已保存的文件原样保留，再打开时从原来的地方接着来。它在 `init.lua` 运行之后读取。哪些会话插件会让 Eitri 让路，以及哪些东西会回来：[再次启动时](getting-started.zh-CN.md#when-you-start-again)。 |
| `editor.takeover` | `true` / `"on"` 或 `false` / `"off"` | `true` | `:EitriOpen`（和 `eitri open`）是否可以把终端里运行的 Neovim 搬进 Eitri 窗口。`false` 时它会说 `takeover is off (editor.takeover in init.lua)`，终端保持原样。它自己不会启动任何东西：开着的时候，你不运行命令就什么都不会发生。值不对时，启动时会列出来并点名这个键，`eitri open` 则会拒绝它。[接管终端里的 nvim](takeover.zh-CN.md)。 |
| `modules.chat.on_permission` | `"badge"` 或 `"reveal"` | `"badge"` | agent 面板被隐藏时来了一张权限卡片，会怎样。`"badge"` 在顶栏显示 `agent ⚑N` 并弹出一条提示；`"reveal"` 把面板带回它原来的位置，但不移动按键焦点。当另一个窗格被放大时，`"reveal"` 改为显示标记，因为显示面板并不会让它出现在屏幕上。companion 窗口从不隐藏它的面板，所以这个键在那里不起作用。 |
| `agents.enabled` | `true` / `"on"` 或 `false` / `"off"` | `true` | 这个窗口是否为自己的每个 agent 标签页发布一行，并读取所有窗口的行；[`<prefix> u` 和 `<prefix> B`](keys.zh-CN.md)、显示你的 agent 的那些页面和 `eitri agents` 都建立在它之上。`false` 什么都不发布也不读取，这两个键在这个窗口里只会显示 `agents are off in this window`。见[同时用很多 agent](many-agents.zh-CN.md#turn-it-off)。 |
| `review.enabled` | `true` / `"on"` 或 `false` / `"off"` | `true` | [回合审阅](turn-review.zh-CN.md)是否运行。`false` 不拍快照，也不提供审阅。 |
| `review.hint` | `true` / `"on"` 或 `false` / `"off"` | `false` | 一个编辑过文件的回合结束后，是否在状态条里加一行提示，以及回合进行期间是否在输入框上方用一行统计它的编辑。无论如何，审阅都只差一个键。 |
| `review.live` | `true` / `"on"` 或 `false` / `"off"` | `false` | agent 编辑的文件是否在编辑落地时就画在你的编辑器里，回合结束时再画一遍，并在缓冲区里提供全部保留和全部撤回（[实时画出改动](turn-review.zh-CN.md#live-drawings)）。启动时读取一次；`review.enabled` 是 `false` 时它不起作用，Eitri 会在启动时说明。 |
| `terminal.font` | `guifont` 格式的字体，`"<字体>:h<磅数>"` | 你的 `guifont` | 终端的字体，窗口里每个终端都用它：`"Iosevka:h12"` 同时设字体和字号，`"Iosevka"` 只设字体（字号仍跟着你的 `guifont`），`":h12"` 只设字号。多个字体用逗号分开、按顺序尝试，`_` 或 `\ ` 表示空格，和 `guifont` 一样。不设置时，终端用编辑器的字体和字号，这样编辑器下面的终端看起来和编辑器一致；`guifont` 为空时用 `monospace`。字号和编辑器一起缩放，见[按键](keys.zh-CN.md)。 |
| `terminal.claude_ide` | `true` / `"on"` 或 `false` / `"off"` | `false` | 你在底部终端里运行的 `claude` 能否连上本窗口的编辑器：打开后 Eitri 在 `127.0.0.1` 上监听，运行期间写下 `~/.claude/ide/<port>.lock`，并在终端的 shell 里设置 `CLAUDE_CODE_SSE_PORT`。由持有编辑器的窗口读取一次，第二个窗口跟随它。见[终端里的 `claude`](terminal-claude.zh-CN.md)。 |
| `keymap.from_tmux` | `true` / `"on"` 或 `false` / `"off"` | `"on"` | Eitri 是否从你的 tmux 配置里读取你的 tmux 前缀和前缀表按键。见[按键](#keys)。 |
| `keymap.which_key_delay` | 0 到 5000 之间的整数，或 `false` / `"off"` | `"off"` | 前缀等第二个键等多少毫秒后，用一个面板列出接下来可以按的键；`0` 立刻显示。见[提示面板](keys.zh-CN.md#the-hint-panel)。companion 窗口和 Mac 上不画。 |
| `keymap.nvim_prefix` | 一个最长 16 个字符的 Neovim 左端（`"<leader>W"`），或 `false` / `"off"` | `"off"` | 在 Neovim 的 Normal 模式里，把你前缀表里的布局键映射到这个左端之下，每个键运行对应的 `:Eitri` 动作。见[从 Neovim 用：`:Eitri`](keys.zh-CN.md#from-neovim-eitri)。 |
| `window.restore` | `true` / `"on"` 或 `false` / `"off"` | `true` | 持有这对窗口的窗口，是否在下次启动时恢复这对窗口上次结束时仍开着的窗口 2，以及合成器窗口组的窗口 1 是否恢复它的窗口、它们的输出和底部终端。设为 `false` 时，窗口 1 收回窗口 2 持有的东西，并提供它保存的标签页。见[两个窗口](two-windows.zh-CN.md)和[合成器窗口](compositor-windows.zh-CN.md#coming-back)。 |
| `window.desktop_start` | `"choose"`、`"last"` 或 `"cwd"` | `"choose"` | 从桌面启动 Eitri 而不带文件或文件夹时做什么。`"choose"` 先在一个小窗口里显示[你的项目列表](getting-started.md#打开一个项目)；`"last"` 不先显示窗口，直接打开你最近用过且还在的项目，没有这样的项目时才显示列表；`"cwd"` 打开启动器所在的目录，和有列表之前一样。其他值会在启动时被列出，设置保持 `"choose"`。只有这种桌面启动会读它：在终端里运行的 `eitri`、`<prefix> O` 列表和带路径的启动都不看它。 |
| `window.tiling` | `"auto"`、`"eitri"` 或 `"compositor"` | `"auto"` | 聊天和页面由谁排布。`"auto"` 在 Hyprland、sway 或 niri 回答时让它们成为窗口管理器的窗口，在别的地方留在 Eitri 的窗口里；`"eitri"` 留在里面；`"compositor"` 在任何 Wayland 桌面上都让它们成为窗口。其他值会在启动时被列出，设置保持 `"auto"`。见[合成器窗口](compositor-windows.zh-CN.md)。 |
| `companion.wm` | `"auto"`、`"hyprland"`、`"sway"`、`"niri"`、`"gnome"` 或 `"none"` | `"auto"` | 由哪个窗口管理器适配器在 [companion 窗口](companion.zh-CN.md)和你的编辑器之间移动焦点。`"auto"` 从会话里检测；`"none"` 从不向窗口管理器请求任何东西。 |

设置只在窗口启动时读取一次。改了 `init.lua` 之后，开一个新窗口才会生效。

<a id="registering-another-agent"></a>
## 注册另一个 agent

`eitri.agent.register` 不是一个设置：它接受一个表，加入一个通过标准输入输出说 ACP 的 agent，这样 `:tabnew NAME` 和 `agent.default` 就能用它。它的各个键、信任问题对它做什么，以及 Eitri 能为这样的 agent 做什么、不能做什么，都在 [Codex 与其他 agent](agents.zh-CN.md#other-agents-over-acp) 里。注册有问题会被拒绝，Eitri 在启动时把它列出来并点出那个键，这个 agent 不会被注册。

<a id="keys"></a>
## 按键

按键在同一个文件里用 `eitri.keymap` 来设：

```lua
local k = eitri.keymap
k.prefix("C-a")                               -- the prefix key
k.set("prefix", "g", "zoom")                  -- bind g (free by default) after the prefix to an action
k.del("prefix", "%")                          -- remove a default binding
k.set("root", "M-z", "zoom")                  -- a key that works without the prefix
```

`k.prefix(key)` 改前缀，`k.set(table, key, action, options)` 绑定一个键，`k.del(table, key)` 去掉一个。`"root"` 表放的是不带前缀就生效的键：它一开始是空的，只收 Ctrl 和 Alt 组合键，它能运行什么、拒绝什么、Eitri 何时警告，见[按键](keys.zh-CN.md#keys-without-the-prefix)。错误的键、不认识的动作或者和 Eitri 保留的键冲突的绑定，会被略过，并在启动时用一条点出这次调用的消息列出来，所以笔误不会悄无声息。[按键](keys.zh-CN.md)列出了默认值和这些动作。

面板的 `"panel"` 表中，`model.pick` 和 `effort.pick` 分别是 `<leader>M` 和 `<leader>E` 的动作名。
重新绑定前先删除默认绑定；例如，把模型选择器移到 `<leader>p`：

```lua
eitri.keymap.del("panel", "<leader>M")
eitri.keymap.set("panel", "<leader>p", "model.pick")
```

默认值是原版 tmux 的，以 `Ctrl+b` 为前缀。然后 Eitri 读取你的 tmux 配置（tmux 读的那些文件，以及它们用绝对路径 `source-file` 进来的文件；相对路径会被跳过，因为 Eitri 不知道 tmux 会拿哪个目录去解析它），最后应用你的 `eitri.keymap` 调用，所以你自己的调用优先。Eitri 只读 tmux 的文件；从不启动或询问 tmux。把 `keymap.from_tmux` 设为 `"off"` 可以跳过 tmux 这一步。

一个完整的例子是 [`docs/keymap/tmux-ctrl-a.lua`](../keymap/tmux-ctrl-a.lua)，它把前缀移到 `Ctrl+a`，并重新绑定了分割、调整大小和放大。把你想要的部分从里面粘贴到你的 `init.lua` 里。

`k.use(name)` 用一行记下一组命名的调用。目前只有一个名字 `"i3-alt"`，给把 Alt 当 `$mod` 的 i3 或 sway 用：它把前缀表里的 Alt 组合键改到 `Ctrl+Alt`，调用内容和 [`docs/keymap/i3-alt-mod.lua`](../keymap/i3-alt-mod.lua) 里的完全一样。它就是一次普通调用，所以写在它后面的调用优先；名字不认识，Eitri 会像对待其他错误调用一样在启动时停下。

```lua
eitri.keymap.use("i3-alt")
```

<a id="first-start"></a>
## 首次启动

没有 `init.lua` 的第一次启动时，完整启动的第一个窗口会在窗口上方打开一个设置界面。它问窗口的按键要怎么用，你同意的话就写入一个很短的 `init.lua`，然后提议运行 [`:EitriTutor`](keys.zh-CN.md)。最后一步之前不会写入任何内容，写出来的也是你自己的东西：一个带注释的普通 `init.lua`，可以像别的文件一样读、一样改。

命令行指定了文件的启动（`eitri FILE`）不会打开这个界面，因为你是为那个文件来的，也不算一次回答：下一次普通的 `eitri` 会打开它，`:Eitri setup` 随时都能打开它。

**它看什么。** 只读，从不改：Eitri 为 [`keymap.from_tmux`](#settings) 已经读过的 tmux 配置；你用的桌面，以及在 i3 或 sway 下 `$mod` 是 Alt 还是 Super（先问窗口管理器自己，再读它的配置文件）；Hyprland 下它的绑定；Hyprland、sway 或 niri 是否回答了版本命令，回答了就有一行 `windows` 说明 [`window.tiling`](compositor-windows.zh-CN.md) `"auto"` 在这里会怎么做；你的 Neovim 里哪个插件占用（或很可能占用）`Ctrl+b` 和 `Ctrl+h/j/k/l`；以及有没有输入法在运行（看进程；Eitri 从不读输入法自己的设置）。各行随答案到来逐个出现，界面不会等慢的那一行。这些文字都不会被抄进文件。

**你选什么。** `j`/`k` 或数字在预设之间移动，`Enter` 继续：

| 预设 | 写入什么 |
|---|---|
| Neovim + tmux | `keymap.from_tmux` 开，按键仍由你的 tmux 配置决定；提示面板写成一行注释（有 tmux 配置时的默认选项） |
| Neovim，不用 tmux | `keymap.from_tmux` 关，前缀是默认的 `Ctrl+b`，[提示面板](keys.zh-CN.md#the-hint-panel)开；Neovim 报告过 leader 时，窗口键也放在它下面（`keymap.nvim_prefix`） |
| 换个前缀 | `Ctrl+a`、`Ctrl+Space` 或你按下的键：`k.prefix`、去掉旧的 `Ctrl+b` 的 `k.del`，以及 `k.set("prefix", key, "send-prefix")`，写法同[按键](keys.zh-CN.md#choosing-ctrlspace-as-the-prefix)里所示 |
| i3 / sway 风格 | 默认按键；`$mod` 是 Alt 时再加 `eitri.keymap.use("i3-alt")`；是 Super 时不多写什么；`window.tiling` 写 `"eitri"`，由 Eitri 排布自己的模块 |
| 平铺合成器 | `window.tiling` 写 `"compositor"`：[窗口管理器把聊天和页面当作自己的窗口排布](compositor-windows.zh-CN.md)，Eitri 的布局键关闭。只有 Hyprland、sway 或 niri 回答了才列出，这时它是默认选项 |

写入之前，复查这一步会把这些行展示出来；`p` 修改前缀，`Backspace` 返回。在 Hyprland、sway 或 niri 上，前三个预设还多一个 `w`，它把 `window.tiling` 写成 `"compositor"` 或 `"eitri"`；旁边那一行说明是哪个（`Windows: sway arranges`）。选了和窗口当前所用不同的前缀，会先做一次**按键测试**：界面请你按一下那个键，并说明它到没到。被输入法或桌面占走的键到不了 Eitri，界面会直说，然后让你换一个前缀，或者仍然写入；等待期间按 `Esc` 返回。键位表不接受的键（`Ctrl+h`、单个字母）会立刻被拒绝，并给出原因。

窗口太矮、放不下界面时，用 `PageUp` 和 `PageDown` 滚动，在复查这一步 `j`/`k` 和方向键也可以。在第一页按 `r` 重新读取 Neovim 的映射，用于缓冲区自己的映射或晚加载的插件。按 `?` 显示一行帮助。

**只写一次，从不覆盖文件。** 文件先以临时名字写好，再链接到位，所以读它的人不会看到写了一半的内容；而且只有当那一刻不存在 `init.lua` 时才会创建：期间如果冒出了一个（比如另一个窗口写的），就什么都不写，并告诉你。Eitri 里没有任何代码会以写入方式打开已有的 `init.lua`。新按键在 Eitri 下次启动时生效；你正在用的窗口继续用现在的按键。

**什么时候打开。** 下面这些全部成立：这是完整启动的第一个窗口（不是第二个窗口、伴随窗口、插件自己的窗口或 `eitri split`），没有给 `--clean`，没有设置 `EITRI_CONFIG_DIR` 和 `EITRI_FIRST_START`（见下），没有 `init.lua`（原位置上是悬空链接、目录或别的东西，都算有），配置目录通过下面说的检查，而且你还没有说过不要（见下一段）。agent 面板正在问是否信任某个项目时，界面会等你回答完再出现。Mac 不显示它。

**按两次 `Esc`，或 `d`。** `Esc` 是“以后再说”：什么都不写，界面在下次启动时再来一次。第二次 `Esc` 之后它就不再自己打开。`d`（“不再询问”）立即让它不再打开。你的回答记在[状态目录](#environment-variables)（`XDG_STATE_HOME`）下 `eitri/` 里的 `first-start.json` 中，不在配置目录里；删掉这个文件，界面就会回来。每次本该打开却没打开的启动，会记一行日志。

**再次打开。** 在窗口的 Neovim 里运行 `:Eitri setup`，或者把动作 `window.setup` 绑到某个键上（它没有默认键，`前缀 ?` 的按键表会列出它），不管上面的条件怎么说，都会打开这个界面。已经有 `init.lua` 时，它只展示那些行，并标明没有写入；按 `o` 在临时缓冲区里打开它们，你可以复制想要的部分。写入失败时也提供同样的 `o`。

**为什么拒绝组写入的目录。** `init.lua` 是每次启动 Eitri 都会运行的代码。所以界面只在属于你、别人不能写（没有组写或其他人写权限）的目录里创建它，而且该目录的上一级也不能让别人改名其中的条目；否则别的用户就能换掉文件或目录。配置目录本身是符号链接（stow 和 chezmoi 会做这种链接）时会跟随，最多跟一层。检查不通过，界面保持关闭，日志里写明是哪个目录；对它 `chmod go-w`，或者用 `:Eitri setup` 读出那些行自己放进去。

**`EITRI_FIRST_START`。** `off` 让界面不再自己打开（不影响 `:Eitri setup`）；其他非空值会被忽略，并记一行日志。测试环境和脚本化的启动把它设成 `off`。`EITRI_CONFIG_DIR` 指向某个目录时，界面也一律关闭，因为那是有意为之的配置。

界面和它写出的文件里的注释都跟随你的语言环境：`LC_ALL`、`LC_MESSAGES`、`LANG`（取第一个设了的）以 `zh` 开头就是简体中文，否则是英文。代码行在两种语言里完全一样。

<a id="layout-from-lua"></a>
## 用 Lua 调整布局

[按键](keys.zh-CN.md)里每个改变布局的键同时也是一个有名字的动作，Lua 用同样的名字调用同样的动作。这些调用用在命令（`eitri.command.register`）或事件处理函数里：它们把你要求的事排进队列，处理函数返回后 Eitri 按顺序执行。在 `init.lua` 还在加载、窗口还不存在时调用，会停下并给出说明这一点的信息；塑造第一个窗口用的是 `eitri.layout.default`。

```lua
eitri.command.register({
  id = "tidy",
  title = "Rearrange the window",
  keybinding = "<Control><Alt>r",
  action = function()
    eitri.layout.run("layout.main-vertical")      -- any layout action, by its eitri.keymap name
    eitri.layout.run("rotate.down")
    eitri.layout.run("resize.left", { cells = 5 }) -- options go in a table, as in eitri.keymap.set
  end,
})
```

- **`eitri.layout.run(name, options?)`** 对拿着按键的那个模块执行一个布局动作：[按键](keys.zh-CN.md)里“模块”表中的那些（排布、交换、移到边缘、标记、放大、调整大小、隐藏、关闭），以及没有按键的那些（`swap.marked`、`layout.undo`、`layout.previous`、两种翻转、`move.far-right`）。要对别的模块操作，先调用 `eitri.layout.focus(id)`。关闭模块仍然会问那个 `y/n`。不是布局动作的（比如标签页动作）会被拒绝并点名，另外还有三样也会被拒绝：`split.right` 和 `split.below`（它们要等一个模块键；用 `eitri.layout.split(id, "right")` 或 `"below"` 来做这件事）、`select.number`（它要等一个数字；用 `eitri.layout.focus(id)` 让某个模块拿到按键）以及 `repeatable` 选项，它属于按键绑定。另外有两点和终端有关：`eitri.layout.run("instance.new", { kind = "terminal" })` 会在拿着按键的模块右边打开一个新的终端，和 `<prefix> Alt+Enter` 一样（`kind = "agent"` 则会建出一个带新的空标签页的[聊天视图](keys.zh-CN.md#chat-views)）；分割动作带一个 `cwd` 选项，见下面。
- **`eitri.layout.mark(id)`** 标记那个模块，不移动按键，就像 `<prefix> m` 标记拿着按键的那个模块。
- **`eitri.layout.get()`** 返回一张描述当前布局的新表，形状和 `eitri.layout.default` 接受的一样：`{ "row", { "editor", 0.6 }, { "column", { "agent" }, { "terminal" } } }`，其中除最后一个以外的每个子项都带着它在容器里的份额。它另外加上 `focus`（拿着按键的模块）、`hidden`（一个列表）、`zoomed` 和 `marked`（某个模块，或者什么都没有）、`floating`（浮动窗口，从下往上，只在有浮动窗口时出现；便笺浮动窗口带 `scratch = true`）、`workspace`（你所在工作区的名字）和 `workspaces`（所有名字，按顺序）。固定高度的底部行不会写进这张表；默认的形状会自己恢复底部行的高度。在布局正被修改的过程中调用它会停下并给出信息，而不是读到改了一半的东西。

- **`eitri.layout.export()`** 把窗口的布局写成能在第一次启动时摆出它的 `init.lua` 调用：`main` 写成 `eitri.layout.default(...)`，其余每个工作区按顺序写成 `eitri.layout.workspace(name, ...)`，形状和下面说的一样，浮动窗口也在内。那种形状没有字段表达的不写：哪个模块拿着按键、缩放、标记、固定高度的底部行的高度。隐藏的模块留在树里原来的位置，第一次启动时会显示出来，并有一行注释点出它。编号的聊天视图不写进去，并有注释说明，因为写在 `init.lua` 里的视图除非有保存的标签页绑着，否则启动时就会被移除（要说它在哪里打开，用[打开规则](#where-a-new-module-opens)）。和 `get()` 一样，它只在窗口存在之后才能用。绑到你自己的一个键上，它就把布局写进一个文件，方便你从里面复制：

  ```lua
  eitri.command.register({
    id = "layout-export",
    title = "Write the layout as init.lua calls",
    keybinding = "<Control><Alt>e",
    action = function()
      local file = assert(io.open(os.getenv("HOME") .. "/eitri-layout.lua", "w"))
      file:write(eitri.layout.export())
      file:close()
    end,
  })
  ```

- **`eitri.layout.float(id, rect?)`** 把那个模块浮起来：放在 `rect`（`{ cx = 0.5, cy = 0.4, w = 600, h = 300 }`：中心是窗口的一个比例，大小以像素计，包括那条栏）；没有就用它上次浮动的矩形；再没有就居中、占窗口的 60%。对一个已经浮动的模块，它把它移到 `rect`。编辑器从不浮动。和 `<prefix> Alt+Shift+Space` 一样，它会拒绝视野里最后一个平铺模块。
- **`eitri.layout.rule(target, opts)`** 只能在 `init.lua` 里用，决定窗口自己打开一个模块时把它放在哪里；见[新模块开在哪里](#where-a-new-module-opens)。
- **`eitri.layout.open(kind, opts?)`** 打开一种模块的新实例，`kind` 是 `"terminal"` 或 `"agent"`（后者是一个带新的空标签页的新聊天视图），和 `eitri.layout.run("instance.new", { kind = ... })` 一样，只是放在哪里由你来定。不带 `opts` 时由你的[打开规则](#where-a-new-module-opens)决定，没有规则就开在拿着按键的模块右边。带 `opts` 时（键和值与规则的相同），它们只决定这一次打开，不影响别的。和其他运行期调用一样，`init.lua` 还在加载时它会被拒绝；别的 `kind` 会让它带着信息停下；运行时的拒绝（没有空闲编号：`at most 9 terminals`、`at most 9 chat views`）会像按键那样记入日志并显示出来。
- **`eitri.layout.workspace(name, tree)`** 只能在 `init.lua` 里用，按 `default` 接受的形状定义一个工作区。窗口里没有保存过同名工作区时就得到它，排在已保存的之后，按调用的先后顺序；`main` 属于 `eitri.layout.default`，在这里会被拒绝。用这种方式定义的工作区，你在窗口里删除或重命名之后，**下次启动会从它的定义里回来**，所以要彻底去掉一个，就去掉它的调用。没有定义的工作区，删除或重命名之后保持原样。
- **`eitri.layout.switch(name)`** 去那个工作区，没有就先建：按它的定义，没有定义就复制你现在的排布。`eitri.layout.run("workspace.switch", { name = "study" })` 是一样的。`workspace.choose` 和 `workspace.rename` 要等一个键或等输入，所以 `run` 拒绝它们。

**`eitri.layout.default` 和 `get()` 用的形状。** 一张表以 `"row"` 或 `"column"` 开头（两个或更多子项，左右并排 / 上下叠放），或者以 `"tabbed"` 或 `"stacked"` 开头（一个或更多子项，同一时刻显示一个，上面是一行标题 / 每个子项各一行标题）。子项是一个模块名，后面可以跟它在容器里的份额作为第二项（`{ "editor", 0.6 }`），也可以是它自己的一张表，份额写成 `share = 0.4`。没有份额的子项平分其余的部分；每个份额在 0.05 到 1 之间，加起来为 1。标签页式或堆叠式的表上，`active = 2` 指明显示哪个子项（从 1 数起，默认第一个）。编号的终端（`terminal#2` 到 `terminal#9`）可以作为子项，如下面的例子，窗口会在它显示出来后给它自己的 shell；编号的聊天视图（`agent#2` 到 `agent#9`）也可以作为子项或浮动窗口，窗口会给它自己的页面。这个页面一开始没有自己的标签页：写在这里的视图只有在某个保存的标签页绑定到它时才会保留，否则恢复结束后启动时就会被移除，并留下日志行 `agent#2 holds no tab; removed`。要得到一个带新的空标签页的视图，请在窗口运行时创建（`eitri.layout.open("agent")`、`eitri.layout.run("instance.new", { kind = "agent" })`，或在某个标签页上按 `<prefix> !`），不要在这里写它的名字。拼错的字段会让启动停下，并点出这个字段。最外层的表（容器里面的不行）还可以带 `floating`，一个由 `{ "module", cx = 0.5, cy = 0.5, w = 600, h = 400, scratch = true }` 组成的列表，从下往上：`cx` 和 `cy` 在 0 到 1 之间，`w` 和 `h` 是 1 到 16384 的整数像素，`scratch`（不写就是 false）让这个浮动窗口一开始就是收起的。编辑器留在树里；agent 面板可以浮动；一个模块要么在树里、要么在列表里，不会两处都在。一个包含各种容器的例子：

```lua
eitri.layout.default({
  "row",
  { "editor", 0.55 },
  { "tabbed", { "agent" }, { "terminal" }, { "terminal#2" }, active = 1 },
})

eitri.layout.workspace("study", { "row", { "editor", 0.6 }, { "agent" },
  floating = { { "terminal#2", cx = 0.5, cy = 0.5, w = 900, h = 420, scratch = true } } })
```

**在终端所在目录开始的分割。** `split.right` 和 `split.below` 接受 `cwd = "pane"`，它让新终端的 shell 在你分割的那个终端所在的目录里启动（按键按下时从那个 shell 读取）。那个 shell 已经结束、读不到它的目录，或者按键是在不是终端的模块里按的，新的 shell 就在项目根目录启动。不带这个选项，或者 `cwd = "start"`，新的 shell 在项目根目录启动。

```lua
k.set("prefix", "\\", "split.right", { cwd = "pane" })
k.set("prefix", "-", "split.below", { cwd = "pane" })
```

tmux 导入开着时，带 `-c "#{pane_current_path}"` 的 `split-window` 绑定会变成同样的 `cwd = "pane"`。别的 `-c` 值不会被导入：这个绑定照样能用，它的新 shell 在项目根目录启动。

**布局保存在哪里。** Eitri 记住每个项目的布局，每个工作区一份，保存在状态目录（见下面的 `XDG_STATE_HOME`）里 `eitri/layout/` 下的 `<hash>.v2.json`。有容器之前的那个文件 `<hash>.json`，只在还没有 `.v2.json` 时读一次，之后原封不动地留着，逐字节不变，所以同一个项目上较旧的 Eitri 继续用它自己的文件。旁边的 `<hash>.v2.json.partial` 保存着这个版本只能用上一部分的文件的原件（读成一行的未知容器种类、它放不下的模块、它不认识的键），在第一次保存之前复制好，什么都不会丢；`<hash>.v2.json.unusable` 是它完全无法使用的文件（不是 JSON、属于别的项目、没有它能读的工作区），在写入新文件之前先被放到一边。文件里这个版本不认识的键会原样写回。

文件里的一个工作区可以带 `floating`（它的浮动窗口，从下往上，每个有 `module`、`cx`、`cy`、`w`、`h` 和 `scratch`）和 `float_memory`（每个浮动过的模块最后一次的矩形，按模块排列）；两者只在不为空时才写，所以没有浮动窗口的布局，写出的文件和浮动窗口出现之前的版本写的一样。读它的版本用不了的条目会被丢弃并留一条说明，原件复制到 `<hash>.v2.json.partial`。**浮动窗口出现之前的版本**（有容器和工作区、但没有浮动窗口的那个构建）把这两个键当作它不认识的键。只有**一个工作区**、而且浮动窗口是任何平铺树都没有的编号终端（`terminal#2` 等等）时，它会把它们原样保留，保留它们的编号，并写回，所以这个版本能再找到它们；被收起的便笺浮动窗口会变成显示着的。当某个浮动窗口点名的是那个版本自己会放置的模块时——基础的 `terminal`、聊天、任何模块的记忆矩形，或者在有**两个或更多工作区**时的任何浮动终端（那个版本把它隐藏在其他工作区的编辑器下面）——它会丢弃这些浮动窗口，把这些模块以平铺的方式打开（基础终端在编辑器下面），并先把文件复制到 `.v2.json.partial`。丢掉的只是浮动窗口的位置，不会丢任何模块。这个版本之后会把它们以平铺的方式打开，位置留在那份复制里。

**文件里的聊天视图。** 聊天视图（`agent#2` 到 `agent#9`）像编号的终端一样，用它的 id 出现在某个工作区的树或 `floating` 列表里，每个工作区都有任何一个工作区提到的视图。保存的标签页（状态目录里的 `eitri/tabs/<hash>.json`，是单独的文件）为显示在额外视图里的标签页带一个 `view` 编号。**有聊天视图之前的版本**会把 `agent#N` 从布局里去掉并留下一条提示，先把文件复制到 `<hash>.v2.json.partial`，所以视图不会丢；它忽略保存的标签页里的 `view`，所以这些标签页每个都回到基础视图。这个版本在布局里已经没有某个标签页的视图，或者第二个标签页提到了另一个标签页已经占了的视图时，同样让标签页回到基础视图。

<a id="where-a-new-module-opens"></a>
### 新模块开在哪里

`eitri.layout.rule(target, opts)` 只能在 `init.lua` 里用，决定窗口自己打开一个模块时把它放在哪里：新的终端或聊天视图（`<prefix> Alt+Enter`、`instance.new`、`eitri.layout.open`）、用 `<prefix> !` 拆出来的标签页、启动时放下的模块，以及你关掉之后又回来的模块。

```lua
eitri.layout.rule("terminal", { at = "below", size = 0.3 })            -- 终端开在拿着按键的模块下面
eitri.layout.rule("agent#2", { float = true, size = { 0.5, 0.6 } })     -- 聊天视图 2：浮动窗口，半个窗口宽、60% 高
eitri.layout.rule("terminal#3", { scratchpad = true, focus = false })  -- 终端 3：收进便笺板
```

- **目标**是一个种类（`"terminal"` 或 `"agent"`，对这种模块里没有自己规则的每个实例都有效；`"terminal"` 也包括基础的 `terminal`，而 `"agent"` 只管编号的聊天视图 `agent#2` 到 `agent#9`，因为第一个聊天视图总是开在侧边，或者开在布局文件给它的位置），或者一个模块：`"terminal#2"`、`"agent#3"`、Lua 面板的 `"lua:notes"`、`"editor"`。一个目标只能有一条规则；指向 Eitri 不认识的东西的目标会让启动停下，并点名那次调用。
- **选项**有 `at`（一个位置，见下）、`float`（布尔值）、`size`、`scratchpad`（布尔值：它浮动并以收起的状态打开，隐含 `float`）和 `focus`（布尔值，默认 `true`：新模块是否拿到按键；`false` 时按键留在原处，除非新模块取代了拿着按键的编辑器）。`at` 是下面之一：`side`（整个窗口右边的新列）、`bottom`（整个窗口下面的新行）、`main`（取代编辑器的位置，编辑器被隐藏）、`below-editor`（编辑器那一列下面的一行，终端的默认位置）、`right`、`below`、`left`、`above`（在拿着按键的模块旁边，占它一半的空间）和 `tab`（作为拿着按键的模块所在容器的一个标签页；那个容器是分割式时，会和它一起变成一个标签页式容器）。`size` 在模块平铺时是它在容器里所占的比例（大于 0 小于 1 的数），浮动时是 `{ w, h }` 或 `{ w = ..., h = ... }`，是窗口的比例，大于 0 且不超过 1；浮动窗口没有给大小时，居中打开，每个方向占窗口的 60%。
- **会被拒绝的**，在启动时列出并点名那次调用，这条规则不生效：不在上面这些里的选项、类型不对或超出范围的值、写成表的平铺大小或写成数字的浮动大小、`float` 或 `scratchpad` 与 `at` 同时给出、浮动的编辑器（编辑器永远不浮动；编辑器的其他规则会被接受，但永远不会生效，因为没有什么会去打开编辑器）、既不是种类也不是模块 id 的目标，以及同一目标的第二条规则。被拒绝的规则即使被你的 Lua 捕获也会被列出来。
- **哪条规则优先。** 先是模块自己 id 的规则，然后是它种类的规则，然后是 Lua 面板自己的 `position`，最后是它打开的那个场合的内置默认：新的终端或聊天视图和拆出来的标签页，开在拿着按键的模块右边（拆出来的标签页是基础视图的右边）；启动时放下的模块，或者关掉之后回来的模块，放在它一直在的地方（终端在编辑器下面，Lua 面板在它的 `position`）。没有任何模块拿着按键时（第一次启动，或者启动时读到的布局文件），`right`、`left` 和 `tab` 变成 `side`，`below` 和 `above` 变成 `bottom`，要求了它们的规则会在日志里留一条提示。
- **规则永远不决定的事。** 自己说明了放在哪里的操作永远优先：分割键、移动、拖动、`eitri.layout.float`。已保存的布局永远不会被规则改动，来自 `eitri.layout.default` 或 `eitri.layout.workspace` 的布局会放下它点名的模块（在[插件自己的窗口](#plugins)里，插件自己的布局排在前面，它的页面每次启动都会被放回去）。已保存的布局或模板里没有、而规则想让它浮动的模块，会以平铺的方式回到它的后备位置，处于隐藏状态，并留下一条提示说明位置。新模块会隐藏着加进其他工作区，放在它的规则或种类给它的位置。

<a id="plugins"></a>
## 插件

插件是窗口里一个自己的页面，作为模块和其他模块放在一起，只拿到它自己的清单里列出的能力。Eitri 自带四个：[Markdown 预览](markdown-preview.zh-CN.md)，以及显示你的 agent 的三个页面（`agents-strips`、`agents-pass` 和 `agents-top`，见[同时用很多 agent](many-agents.zh-CN.md)）；你也可以自己写或安装别的（[你自己做的页面](pages.zh-CN.md)讲了怎么写，以及怎么把一个普通的 HTML 页面作为 Lua 面板加进来）。每个插件都是一个含有 `eitri-plugin.toml` 的目录，在 `init.lua` 加载它之前，它的任何部分都不会运行：

```lua
eitri.plugin.load("markdown")                          -- 按名字
eitri.plugin.load("~/src/my-plugin", { key = "y" })    -- 按路径
```

- **按名字**，即只由小写字母、数字和 `-` 组成的一个词：Eitri 按顺序在这些地方查找，加载第一个含有 `eitri-plugin.toml` 的目录：
  1. `$XDG_DATA_HOME/eitri/plugins/<name>/`（没有设置 `XDG_DATA_HOME` 时是 `~/.local/share/eitri/plugins/<name>/`），放你自己的插件，以及你对自带插件的副本；
  2. 安装脚本随 Eitri 一起安装的那份（`~/.local/lib/eitri/plugins/<name>/`）；
  3. `<$XDG_DATA_DIRS 的每个文件夹>/eitri/plugins/<name>/`（没有设置时依次是 `/usr/local/share` 和 `/usr/share`），软件包把自己的那份装在这里（`/usr/share/eitri/plugins/<name>/`）。

  空目录会被跳过，并打印一行说明。`eitri-plugin.toml` 有错误的目录仍然算作这个插件：Eitri 会带着那个错误停下，而不是去加载另一份。插件的 `id` 必须和你写的名字一致。
- **按路径**，即其他任何写法：绝对路径，或以 `~` 开头的路径。相对路径会被拒绝，因为它在每个项目里会指向不同的目录。
- 每次加载都会在你启动 Eitri 的终端里打印它用了哪个目录、为什么：`[plugin markdown] loaded /usr/share/eitri/plugins/markdown (system data directory)`。一个名字在哪里都找不到时，Eitri 会在启动时停下，并列出它找过的每个目录。
- 选项是插件自己的设置，外加 `key`（前缀之后的模块按键）和 `position`（`"side"`、`"bottom"` 或 `"main"`）。插件按[打开规则](#where-a-new-module-opens)打开，模块名是 `lua:<id>`。
- 插件可以在清单里（`[window]`）指定一个属于它自己的目录。在那个目录里启动 Eitri，这个窗口就是插件的：顶栏和窗口标题显示插件的标题，而不是项目名。没有保存过布局时，插件的布局排在你的 `eitri.layout.default` 前面，它没有放进去的编辑器或聊天先隐藏着（按它们的键就回来）。已保存的布局仍然优先，只有一个例外：布局里没有插件的页面，或者页面是隐藏的，每次启动都会把页面放回它的位置。想要回到插件的整套布局，删掉那个目录保存的布局文件（见[布局保存在哪里](#layout-from-lua)）。
- 加载一个插件，就像运行任何程序一样信任它：它是你自己的本地目录。只加载你信任的插件。companion 窗口里没有插件。

<a id="keys-in-a-plugin-page"></a>
### 插件页面里的按键

读取按键的插件页面（[Markdown 预览](markdown-preview.zh-CN.md#reading-with-the-keyboard)就是）用的是 Vim 和 Vimium 的按键：`j`/`k` 滚动，`/` 查找，`f` 给可以跟随的东西标上标签，`?` 列出页面的按键。有六个设置同时作用于所有插件页面里的这些按键：

| 键 | 值 | 默认值 | 作用 |
|---|---|---|---|
| `plugins.keys.scroll_step` | 10 到 400 之间的整数 | `60` | 按一次 `j` 或 `k` 滚动多远，单位是像素。 |
| `plugins.keys.smooth` | `true` / `"on"` 或 `false` / `"off"` | `true` | 滚动是否平滑滑行。`false` 让每个滚动键直接跳到位，桌面的"减少动画"设置也是这样。 |
| `plugins.keys.prefix_wait` | `true` / `"on"` 或 `false` / `"off"` | `true` | 只是更长按键开头的键（`gg` 里的 `g`）是否一直等下一个键，和 Vim 自己的 `g` 一样。`false` 时超过 Neovim 的 `timeoutlen` 就放弃。 |
| `plugins.keys.which_key_delay` | 0 到 5000 之间的整数、`false` 或 `"off"` | `200` | 按下这样的键多少毫秒后，页面的状态行列出接下来可以按的键。`false` 时从不列出。 |
| `plugins.keys.hint_chars` | 2 到 26 个互不相同的小写字母 | `"sadfjklewcmpgh"` | `f` 的标签由哪些字母组成。 |
| `plugins.keys.find_case` | `"smart"`、`"ignore"` 或 `"match"` | `"smart"` | `/` 怎样处理大小写：`"smart"` 不区分大小写，除非搜索里有大写字母；`"ignore"` 总是不区分；`"match"` 总是区分。 |

```lua
eitri.config.set("plugins.keys.scroll_step", 80)
eitri.config.set("plugins.keys.find_case", "ignore")
```

它们只在窗口启动时读取一次；超出这些范围的值会在启动时被拒绝并列出，点出那个键。页面自己的按键在它的 `plugin:<id>` 表里用 `eitri.keymap.set` 修改。

<a id="how-a-launch-starts"></a>
## 一次启动怎样开始

有两个设置决定一个新窗口怎样处理上一个会话的标签页和模式。

```lua
eitri.config.set("agent.restore", "offer")        -- "offer"（默认）、"auto"、"off" 或 false
eitri.config.set("agent.default_mode", "auto")    -- "manual", "auto" (the default) or "bypass"
```

- **`agent.restore`** 决定上一个打开这个项目的窗口里开着的标签页怎么办。Eitri 会随时记下那些有可以继续的会话的标签页（它们的顺序、名字、模式、agent，以及当时在屏幕上的是哪一个），而且不会因为你关了窗口就记成“没有标签页”。设为 `"offer"` 时，只要窗口里还没有任何东西启动，空标签页的仪表盘就会显示一行 `Restore last session`，按 `s` 即可（按键一开始在编辑器里，那里的 `s` 是 nvim 的，所以 `<prefix> w` 打开的会话列表里也有一行 `Restore last session`）；`"auto"` 在启动时不用按任何键就把它们带回来（除非项目的 Claude 配置是新的或已改动，这时恢复要等你回答[信任问题](permissions.zh-CN.md#trusting-a-project)，在它上面按 `Esc` 会保留已保存的标签页：它们作为失败的标签页回来，可以用 `r` 再次启动；从空标签页的 `Restore last session` 那一行发起的，在那个标签页仍是空的时候，仍然留在那里供你再选）；`"off"` 或 `false` 既不提议也不记录；`true` 和 `"on"` 会被拒绝。每个标签页都会被继续（在你输入之前什么都不会发送），上次在屏幕上的那个仍然显示在屏幕上，并且会有一条消息告诉你恢复了几个。某个标签页如果保存的记录已经没了，或者它的会话被另一个窗口占着，就会被跳过并点名。曾经处于 bypass 的标签页，没有你的一句“是”，绝不会以 bypass 回来：`s` 会先问，回答 `n`（或使用 `"auto"`）就让它以 auto 回来。曾经处于 manual 的标签页会以 manual 回来，不用提问。
- **`agent.default_mode`** 是新标签页开始时所处的模式，针对你还没有用 `Shift+Tab` 改过空标签页模式的项目（那个选择，manual 或 auto，按项目记住，并且始终优先，它优先时启动日志会说出来；bypass 从不被记住，一个没有 manual 模式的 agent 的标签页离开 bypass 时落到的 auto 也不会被记住）。设为 `"manual"` 会让每个标签页都以最严格的模式开始，在那里每一次文件编辑都是一张卡片；一个没有 manual 模式的 agent 的标签页会改以 auto 开始，Eitri 会说一次，并指明是哪个标签页。设为 `"bypass"` 是唯一一种窗口不经询问就以 bypass 开始的方式，因为你已经在自己的文件里这么说了；它同时也让保存下来的 bypass 标签页不必回答那个问题就以 bypass 回来。这三种模式的含义见[权限](permissions.zh-CN.md)。

<a id="files-and-windows"></a>
## 文件和窗口

`eitri FILE` 在编辑器里打开一个文件，并从文件推出项目（见[打开文件](getting-started.zh-CN.md#open-a-file)）。其中有两点在命令行上决定，`init.lua` 里没有对应的设置：

- 这个文件所属的项目已经开着一个窗口时，`eitri FILE` 把文件交给那个窗口，把它提到前面，然后返回。`eitri --new-window FILE` 则改为开一个新窗口。`--clean` 和 `--account NAME` 也开新窗口，因为复用窗口会丢掉它们要求的东西。`eitri DIR` 和不带参数的 `eitri` 总是开新窗口。
- 在 Eitri 自己的底部终端里输入的命令会找到它自己的窗口：终端的 shell 里设置了 `EITRI_OPEN_SOCKET`，`eitri` 读它来找到那个窗口。这个变量不用你自己设置。

<a id="accounts"></a>
## 账号

如果你有不止一个 Claude 账号，就指明一个窗口该用哪个。有两种办法：

- 命令行上的 `eitri --account NAME`（它对 `eitri panel` 和 `eitri split` 也有效）。启动器会打印它选了哪个账号，以及这个名字是从哪里来的。带文件时它总是开新窗口。
- `init.lua` 里的 `eitri.config.set("agent.account", "NAME")`，作为那些没有指明账号的启动（比如从应用菜单启动）的默认值。

两者都有时，命令行优先。你的环境里已经设好的 `VERDANDI_CLAUDE_ACCOUNT` 也优先于 `init.lua`：`--account` 为这次启动设的就是同一个变量，所以一个标志也优先于继承来的值。

名字是单个词，由字母、数字、`.`、`_` 和 `-` 组成，以字母或数字开头。Eitri 对它使用 Claude Code 的配置目录 `~/.claude-NAME`；把 `VERDANDI_CLAUDE_CONFIG_DIR` 设成一个绝对路径，就用另一个目录（这时名字只是一个标签）。名字格式不对、或者它的目录不存在，会让启动停下，并给出一条点出这个名字来源的消息，而不是以碰巧启动了这个窗口的那个账号继续启动。空值算作没有账号。

账号决定会话使用哪个登录，以及从哪里读取它们的历史，所以继续一个对话时，会在这个账号自己的历史里找。一个窗口只有一个账号：每个[聊天视图](keys.zh-CN.md#chat-views)用的都是它，显示在自己视图里的标签页不会改在别的账号下启动会话。

<a id="environment-variables"></a>
## 环境变量

这些是用户可以设置的变量。Eitri 还会为它自己的子进程设置许多别的变量；那些不是设置，也没有列出来。

| 变量 | 作用 |
|---|---|
| `EITRI_NVIM` | 要运行的 `nvim` 的绝对路径。它优先于下面的一切。不是指向现有文件的绝对路径的值会让启动停下。 |
| `NEOVIM_BIN` | 在没有设置 `EITRI_NVIM` 时读取：规则相同，要求指向现有文件的绝对路径。两者都没有时，Eitri 用 `PATH` 上的 `nvim`；如果它不存在或者比 0.11 旧，并且你在 `eitri setup` 里接受过一个私有副本，就用 `$XDG_DATA_HOME/eitri/nvim/` 下最新的那个副本。 |
| `EITRI_CODEX` | Codex 标签页运行的 `codex`：一个现有文件的绝对路径，否则 Eitri 会在启动时停下并点出这个变量。它优先于 `agent.codex.command` 和 `PATH`。空值算作未设置。Eitri 不为 Codex 标签页设置 `CODEX_HOME`；Codex 读的是你的，而相对的 `CODEX_HOME` 会让每一次 Codex 启动都被拒绝。 |
| `EITRI_NEOVIDE` | `eitri split` 启动的那个 Neovide。它必须指向一个现有文件；没设置时用 `PATH` 上的 `neovide`。 |
| `EITRI_PROJECT_DIR` | 命令行上没有给目录时要打开的项目。命令行上的目录优先于它，它优先于当前目录。已安装的 `eitri` 命令不读它：没有给目录时，它打开当前目录。它只指目录；文件用 `eitri FILE` 打开。 |
| `EITRI_CONFIG_DIR` | 从这个目录读取 `init.lua` 和它的 `lua/` 模块，覆盖 Linux 的 XDG 配置目录或 `~/.config/eitri`。空值视为未设置，相对路径会拒绝启动。 |
| `EITRI_FIRST_START` | `off` 让[首次启动界面](#first-start)不再自己打开。其他非空值会被忽略，并记一行日志。 |
| `EITRI_AGENT_TRACE` | 设为 `1`，每个回合在 stderr 上打印一行 `[turn-trace]`，带着这个回合的耗时。 |
| `EITRI_WINDOW_LOG` | 设为 `off`，不写崩溃记录，也不留窗口日志。见 [Eitri 自己崩溃时](getting-started.zh-CN.md#eitri-自己崩溃时)。 |
| `EITRI_SIDECAR_BINARY` | 运行 agent 会话所用的 sidecar 程序的路径，替代 `eitri setup` 构建的或软件包安装的那个，也替代任何 [sidecar 更新](../../INSTALL.zh-CN.md#updating-the-sidecar)：它优先于更新，也不会被自带的 sidecar 顶替。不是文件的路径会在会话启动时报错。 |
| `EITRI_EDITOR_DMABUF` | 编辑器怎样把它的绘制交给 GTK。`0`、`off`、`false` 或 `no` 使用 GTK 自己的纹理路径；`1`、`on`、`true` 或 `yes` 即使在比 4.16 旧的 GTK 上也强制使用 Eitri 自己的缓冲区；其他任何值或不设置，则从 GTK 4.16 起使用 Eitri 自己的缓冲区。 |
| `XDG_STATE_HOME` | Eitri 保存它所记住的东西的地方：每个项目的布局、打开的标签页、提示历史、权限规则、信任的回答和回合审阅快照，都在这个目录下的 `eitri/` 里。未设置、为空或是相对路径时，表示 `~/.local/state`。不会往你的项目里写任何东西。 |
| `XDG_DATA_HOME`、`XDG_DATA_DIRS` | `eitri.plugin.load` 按名字查找插件的地方：先找 `XDG_DATA_HOME` 下的 `eitri/plugins/<name>/`（没有设置、为空或是相对路径时用 `~/.local/share`），最后找 `XDG_DATA_DIRS` 每个文件夹下的（没有设置或为空时依次是 `/usr/local/share` 和 `/usr/share`）。见[插件](#plugins)。 |
| `VERDANDI_CLAUDE_ACCOUNT`、`VERDANDI_CLAUDE_CONFIG_DIR` | 账号和它的配置目录；见[账号](#accounts)。 |
