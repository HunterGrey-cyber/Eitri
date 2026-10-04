[English](configuration.md) | 简体中文
<!-- translated-from: configuration.md sha256=87ecaaf4fa1cc11e5652e9ec95f75ef0a4860de0c1c02658afdb32315d5a064f -->

# 配置

Eitri 有一个配置文件，是一个在启动时运行一次的 Lua 脚本；另外还有少数几个环境变量，用于某个设置属于 shell 或启动器、而不属于你本人的情形。这一页列出每一个设置、它的作用和默认值。某个设置会改变按键时，这一页会说明，并指向[按键](keys.zh-CN.md)。

## init.lua

窗口启动时，Eitri 读取 `~/.config/eitri/init.lua`。把 `EITRI_CONFIG_DIR` 设成一个目录，就改从那里读取 `init.lua`。不需要先创建文件：没有它，Eitri 用内置的默认值启动，并打印一行说明这一点。

设置用 `eitri.config.set(key, value)` 来做。值是字符串、数字或布尔值（`true` 或 `false`）；`nil` 会把这个键重新取消设置。Eitri 保存不了的值（比如表或函数）会导致启动失败。

```lua
eitri.config.set("agent.restore", "auto")
eitri.config.set("review.hint", true)
```

下面每个设置都遵循三条规则：

- 设置不接受的值会让 Eitri 在启动时停下，并给出一条点出这个键的消息。拼错的值绝不会悄悄回退到默认值。
- Eitri 不认识的键不是错误，也不起任何作用。
- 文件本身的 Lua 错误（比如代码里的笔误）会打印在你启动 Eitri 的那个终端里，不会让启动中止：Eitri 会带着文件在出错之前已经设好的内容打开。

`require` 只在 `init.lua` 旁边的 `lua/` 目录下找模块：`require("mine")` 加载 `~/.config/eitri/lua/mine.lua` 或 `~/.config/eitri/lua/mine/init.lua`，`require("a.b")` 加载 `lua/a/b.lua` 或 `lua/a/b/init.lua`。它从不在你打开的项目或当前目录里找，改动 `package.path` 或 `package.cpath` 也没有效果。

两种窗口读的是同一个文件：单窗口模式和 [companion 窗口](companion.zh-CN.md)。

## 设置

| 键 | 值 | 默认值 | 作用 |
|---|---|---|---|
| `agent.account` | 一个账号名 | 未设置 | 这个窗口使用的 Claude 账号。见[账号](#accounts)。 |
| `agent.font_size` | 9 到 32 之间的数字 | `14` | agent 面板的文字大小，单位是像素。 |
| `agent.typing_cadence_hz` | 1 到 60 之间的整数，或 `"off"` | `5` | 你在编辑器里输入时，agent 面板每秒最多允许更新多少次。稳定的较慢频率可以避免面板在你输入时重绘；`"off"` 让它全速更新。 |
| `agent.default_mode` | `"auto"` 或 `"bypass"` | `"auto"` | 新标签页开始时所处的模式。见[一次启动怎样开始](#how-a-launch-starts)。 |
| `agent.restore` | `"offer"`、`"auto"` 或 `"off"` | `"offer"` | 对这个项目上一个窗口里开着的标签页怎么办。见[一次启动怎样开始](#how-a-launch-starts)。 |
| `agent.user_settings` | `true` 或 `false` | `true` | agent 会话是否加载你自己的 Claude Code 配置（你的 hook、插件、skill、`CLAUDE.md` 和权限规则），和终端里的 `claude` 一样。`false` 把你自己的配置排除在外；项目自己的配置由[信任](permissions.zh-CN.md)管，不由这个键管。 |
| `modules.chat.on_permission` | `"badge"` 或 `"reveal"` | `"badge"` | agent 面板被隐藏时来了一张权限卡片，会怎样。`"badge"` 在顶栏显示 `agent ⚑N` 并弹出一条提示；`"reveal"` 把面板带回它原来的位置，但不移动按键焦点。当另一个窗格被放大时，`"reveal"` 改为显示标记，因为显示面板并不会让它出现在屏幕上。companion 窗口从不隐藏它的面板，所以这个键在那里不起作用。 |
| `review.enabled` | `true` 或 `false` | `true` | [回合审阅](turn-review.zh-CN.md)是否运行。`false` 不拍快照，也不提供审阅。 |
| `review.hint` | `true` 或 `false` | `false` | 一个编辑过文件的回合结束后，是否在状态条里加一行提示。无论如何，审阅都只差一个键。 |
| `keymap.from_tmux` | `"on"` 或 `"off"` | `"on"` | Eitri 是否从你的 tmux 配置里读取你的 tmux 前缀和前缀表按键。见[按键](#keys)。 |
| `companion.wm` | `"auto"`、`"hyprland"`、`"sway"`、`"niri"`、`"gnome"` 或 `"none"` | `"auto"` | 由哪个窗口管理器适配器在 [companion 窗口](companion.zh-CN.md)和你的编辑器之间移动焦点。`"auto"` 从会话里检测；`"none"` 从不向窗口管理器请求任何东西。 |

设置只在窗口启动时读取一次。改了 `init.lua` 之后，开一个新窗口才会生效。

<a id="keys"></a>
## 按键

按键在同一个文件里用 `eitri.keymap` 来设：

```lua
local k = eitri.keymap
k.prefix("C-a")                               -- the prefix key
k.set("prefix", "m", "zoom")                  -- bind m after the prefix to an action
k.del("prefix", "%")                          -- remove a default binding
```

`k.prefix(key)` 改前缀，`k.set(table, key, action, options)` 绑定一个键，`k.del(table, key)` 去掉一个。错误的键、不认识的动作或者两个互相冲突的绑定，会让 Eitri 在启动时停下，并给出一条点出这次调用的消息，所以笔误不会留给你一套你没有写过的按键映射。[按键](keys.zh-CN.md)列出了默认值和这些动作。

默认值是原版 tmux 的，以 `Ctrl+b` 为前缀。然后 Eitri 读取你的 tmux 配置（tmux 读的那些文件，以及它们 `source-file` 进来的文件），最后应用你的 `eitri.keymap` 调用，所以你自己的调用优先。Eitri 只读 tmux 的文件；从不启动或询问 tmux。把 `keymap.from_tmux` 设为 `"off"` 可以跳过 tmux 这一步。

一个完整的例子是 [`docs/keymap/tmux-ctrl-a.lua`](../keymap/tmux-ctrl-a.lua)，它把前缀移到 `Ctrl+a`，并重新绑定了分割、调整大小和放大。把你想要的部分从里面粘贴到你的 `init.lua` 里。

<a id="how-a-launch-starts"></a>
## 一次启动怎样开始

有两个设置决定一个新窗口怎样处理上一个会话的标签页和模式。

```lua
eitri.config.set("agent.restore", "offer")        -- "offer" (the default), "auto" or "off"
eitri.config.set("agent.default_mode", "auto")    -- "auto" (the default) or "bypass"
```

- **`agent.restore`** 决定上一个打开这个项目的窗口里开着的标签页怎么办。Eitri 会随时记下那些有 Claude 会话的标签页（它们的顺序、名字、模式，以及当时在屏幕上的是哪一个），而且不会因为你关了窗口就记成“没有标签页”。设为 `"offer"` 时，只要窗口里还没有任何东西启动，空标签页的仪表盘就会显示一行 `Restore last session`，按 `s` 即可；`"auto"` 在启动时不用按任何键就把它们带回来；`"off"` 既不提议也不记录。每个标签页都会被继续（在你输入之前什么都不会发送），上次在屏幕上的那个仍然显示在屏幕上，并且会有一条消息告诉你恢复了几个。某个标签页如果保存的记录已经没了，或者它的会话被另一个窗口占着，就会被跳过并点名。曾经处于 bypass 的标签页，没有你的一句“是”，绝不会以 bypass 回来：`s` 会先问，回答 `n`（或使用 `"auto"`）就让它以 auto 回来。
- **`agent.default_mode`** 是新标签页开始时所处的模式，针对你还没有用 `Shift+Tab` 离开过 bypass 的项目（那个选择按项目记住，并且始终优先）。设为 `"bypass"` 是唯一一种窗口不经询问就以 bypass 开始的方式，因为你已经在自己的文件里这么说了；它同时也让保存下来的 bypass 标签页不必回答那个问题就以 bypass 回来。这两种模式的含义见[权限](permissions.zh-CN.md)。

<a id="accounts"></a>
## 账号

如果你有不止一个 Claude 账号，就指明一个窗口该用哪个。有两种办法：

- 命令行上的 `eitri --account NAME`（它对 `eitri panel` 和 `eitri split` 也有效）。启动器会打印它选了哪个账号，以及这个名字是从哪里来的。
- `init.lua` 里的 `eitri.config.set("agent.account", "NAME")`，作为那些没有指明账号的启动（比如从应用菜单启动）的默认值。

两者都有时，命令行优先。你的环境里已经设好的 `VERDANDI_CLAUDE_ACCOUNT` 也优先于 `init.lua`：`--account` 为这次启动设的就是同一个变量，所以一个标志也优先于继承来的值。

名字是单个词，由字母、数字、`.`、`_` 和 `-` 组成，以字母或数字开头。Eitri 对它使用 Claude Code 的配置目录 `~/.claude-NAME`；把 `VERDANDI_CLAUDE_CONFIG_DIR` 设成一个绝对路径，就用另一个目录（这时名字只是一个标签）。名字格式不对、或者它的目录不存在，会让启动停下，并给出一条点出这个名字来源的消息，而不是以碰巧启动了这个窗口的那个账号继续启动。空值算作没有账号。

账号决定会话使用哪个登录，以及从哪里读取它们的历史，所以继续一个对话时，会在这个账号自己的历史里找。

## 环境变量

这些是用户可以设置的变量。Eitri 还会为它自己的子进程设置许多别的变量；那些不是设置，也没有列出来。

| 变量 | 作用 |
|---|---|
| `EITRI_NVIM` | 要运行的 `nvim` 的绝对路径。它优先于下面的一切。不是指向现有文件的绝对路径的值会让启动停下。 |
| `NEOVIM_BIN` | 在没有设置 `EITRI_NVIM` 时读取：规则相同，要求指向现有文件的绝对路径。两者都没有时，Eitri 用 `PATH` 上的 `nvim`；如果它不存在或者比 0.10 旧，并且你在 `eitri setup` 里接受过一个私有副本，就用 `$XDG_DATA_HOME/eitri/nvim/` 下最新的那个副本。 |
| `EITRI_NEOVIDE` | `eitri split` 启动的那个 Neovide。它必须指向一个现有文件；没设置时用 `PATH` 上的 `neovide`。 |
| `EITRI_PROJECT_DIR` | 命令行上没有给目录时要打开的项目。命令行上的目录优先于它，它优先于当前目录。 |
| `EITRI_CONFIG_DIR` | 从这个目录读取 `init.lua` 和它的 `lua/` 模块，而不是 `~/.config/eitri`。 |
| `EITRI_AGENT_TRACE` | 设为 `1`，每个回合在 stderr 上打印一行 `[turn-trace]`，带着这个回合的耗时。 |
| `EITRI_SUPERVISOR` | 设为 `1`、`true` 或 `yes`，允许窗口在没有运行时启动 `eitri-supervisor`，一个显示所有已打开窗口的 agent 状态的仪表盘。没设置时，窗口只会连接到你自己启动的那个。 |
| `EITRI_SIDECAR_BINARY` | 运行 agent 会话所用的 sidecar 程序的路径，替代 `eitri setup` 构建的或软件包安装的那个。不是文件的路径会在会话启动时报错。 |
| `EITRI_EDITOR_DMABUF` | 编辑器怎样把它的绘制交给 GTK。`0`、`off`、`false` 或 `no` 使用 GTK 自己的纹理路径；`1`、`on`、`true` 或 `yes` 即使在比 4.16 旧的 GTK 上也强制使用 Eitri 自己的缓冲区；其他任何值或不设置，则从 GTK 4.16 起使用 Eitri 自己的缓冲区。 |
| `XDG_STATE_HOME` | Eitri 保存它所记住的东西的地方：每个项目的布局、打开的标签页、提示历史、权限规则、信任的回答和回合审阅快照，都在这个目录下的 `eitri/` 里。未设置、为空或是相对路径时，表示 `~/.local/state`。不会往你的项目里写任何东西。 |
| `VERDANDI_CLAUDE_ACCOUNT`、`VERDANDI_CLAUDE_CONFIG_DIR` | 账号和它的配置目录；见[账号](#accounts)。 |
