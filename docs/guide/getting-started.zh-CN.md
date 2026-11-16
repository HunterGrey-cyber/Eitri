[English](getting-started.md) | 简体中文
<!-- translated-from: getting-started.md sha256=469cf302737e7cc3ff1eba293e75460a58fd257661e150a3e94c1aabe38b8f10 -->

# 快速上手

这一页从敲下命令讲到第一个回合结束：打开项目、发提示、回答卡片、开几个会话，以及回合没有正常结束时面板会怎么说。

开始之前，先[装好](../../INSTALL.zh-CN.md) Eitri，并把 `claude` 命令装好并设置好。Eitri 自己不带 Claude Code，用的是你装的那个；找不到时怎么办，见本页末尾的 [Eitri 去哪里找 `claude`](#where-eitri-looks-for-claude)。

文中的按键都按默认设置写：

- **前缀**是 `Ctrl+b`。写成 `<prefix> x` 的意思是：按 `Ctrl+b`，松开，再按 `x`，和 tmux 一样。
- **leader** 是 `Space`。如果你的 Neovim 配置把 `mapleader` 设成了别的单个键、而且面板能用，就用你的；更长的或者被保留的键会退回 `Space`。

完整的按键表在[按键](keys.zh-CN.md)里，面板里按 `F1` 也能看到。没用过 tmux 的话，先看[这 6 个键](tmux-6-keys.zh-CN.md)。

<a id="the-first-launch"></a>
## 第一次启动

还没有 `init.lua` 的话，第一个窗口会先打开一个简短的设置界面。它列出在你电脑上找到的东西：tmux 配置和它的前缀、你用的桌面、可能和 Eitri 按键冲突的 Neovim 映射、正在运行的输入法。然后问你窗口的按键怎么设。在 Hyprland、sway 或 niri 上，它还会提供一个由窗口管理器来排布对话和页面的预设，并默认选中（[合成器窗口](compositor-windows.zh-CN.md)）。

- `j` / `k` 或者预设的数字来选；`Enter` 进入确认页，上面列出它要写的那几行。
- 确认页上按 `Enter` 写入 `~/.config/eitri/init.lua`（只写一次，已有的文件绝不覆盖）；`Backspace` 返回。新的按键和 `window.tiling` 这一行（如果有）下次启动才生效。写完后按 `t` 打开 `:EitriTutor`，按 `Enter` 关掉。
- `Esc` 是「以后再说」：什么都不写，下次启动再问一次。`d` 是不再问。以后想再打开，用 `:Eitri setup`。

想自己写这个文件的话，按两次 `Esc` 或者按 `d`。这个界面能设的东西，[按键](keys.zh-CN.md)和[配置](configuration.zh-CN.md#first-start)里都有。

<a id="open-a-project"></a>
## 打开一个项目

```sh
eitri [DIR]
```

`DIR` 是项目目录。不写的话，先看 `$EITRI_PROJECT_DIR`，没设就用你当前所在的目录。路径写错会让启动停下并给出消息，奇怪的名字怎么处理见[目录参数的细节](#directory-arguments)。要打开文件而不是目录，见[打开文件](#open-a-file)。

从终端启动时，窗口一出来 `eitri` 就把提示符还给你，什么都不打印。窗口是独立运行的：关掉那个终端、`exit`、杀掉 tmux 窗格、ssh 断开，窗口都还在，输出写进它的日志（`~/.local/state/eitri/log/`）。如果窗口还没出来就退出了，命令会说明情况，打印窗口的输出，并用它的退出码退出。想要以前的方式（窗口在前台、输出在终端、跟着终端一起结束），用 `eitri --no-fork DIR`。不开窗口的命令（`eitri agents`、`eitri sidecar ...`、`eitri setup`、`eitri panel`、`eitri page`、`eitri split`、`eitri open`）始终留在终端里。

每打开一个目录就是一个窗口，有自己的 Neovim、自己的会话和自己的项目根目录，所以几个项目可以同时开着。窗口标题写着项目名（`myproject — Eitri`；有两个工作区时还写当前那个：`myproject — Eitri · docs`），窗口切换器和合成器的窗口规则读的就是它。

窗口一边是编辑器，一边是 agent 面板。编辑器就是你自己的 Neovim，带着你的配置和插件。面板一开始是一个空标签页，显示项目目录、配置了的话还有 Claude 账号，以及一个小菜单：

| 键 | 做什么 |
|---|---|
| `i` | 新开一个会话，开始打字 |
| `s` | 恢复这个项目上次窗口里开着的标签页（有的时候才显示） |
| `r` | 接着最近一次保存的对话（有的时候才显示） |
| `w` | 从所有保存的会话里选 |
| `p` | 打开另一个项目（你别的项目列表；[按键](keys.zh-CN.md#前缀)） |
| `Shift+Tab` | 开始之前切换模式（Manual、Auto、Bypass；模式显示在面板底部） |
| `?` | 列出所有按键 |

菜单下面的 **Pages** 列出 Eitri 自带、但这个窗口还没加载的页面（三个 agent 页面和 Markdown 预览，每个一行说明）。`j` / `k` 移到上面；`Enter` 在这次运行里加载并显示它；`o` 在 Neovim 的一个临时 buffer 里打开「每次都加载它」要写的那行 `init.lua`，给你复制（Eitri 不会改你的 `init.lua`）。见[插件](configuration.zh-CN.md#plugins)。

`Ctrl+h` 和 `Ctrl+l` 在编辑器和面板之间移动按键焦点。

两个值得记住的选项：`eitri --version` 打印版本后退出；`eitri --clean` 用 `nvim --clean` 启动编辑器，也就是不加载你的配置和插件，想分清问题是不是出在你自己的配置上，这是最快的办法。

**换个项目。** 在任何窗口里按 `<prefix> O`（或者在空标签页上按 `p`，或者点顶栏上的项目名），会列出你以前打开过的项目，最近的在前。选一个：已经有窗口的会被提到前面，没有的在新窗口里打开。可以过滤列表，也可以直接输入路径（`~/src/app`，`Tab` 补全）。具体按键和找新目录的方法见[本页末尾](#more-on-projects)。

**从桌面图标启动。** 不带文件或目录从桌面启动时，Eitri 不会打开你的 home 目录，而是先在一个小窗口里显示同一份项目列表，这时还什么都没启动：没有 Neovim，也不问要不要信任某个文件夹。选中项目后打开它的窗口，小窗口关掉；`Esc` 关掉小窗口，什么都不留。第一次用时列表是空的，窗口会提供 `Browse folders…` 和 `Home folder`，然后首次启动的设置界面会在你选的那个窗口里打开。[`window.desktop_start`](configuration.zh-CN.md#settings) 可以改这个行为：`"last"` 直接打开你最近用的项目、不显示列表；`"cwd"` 恢复旧行为，用启动器所在的目录。在终端里跑 `eitri` 不受影响。

<a id="your-first-turn"></a>
## 你的第一个回合

按 `Ctrl+l` 把焦点移到面板，再按 `i`（或 `Ctrl+j`）开始输入。如果你改过模块间移动的键，用你的键代替 `Ctrl+l`。

- `Enter` 发送；`Shift+Enter` 换行，`Alt+Enter`、或者反斜杠后面跟 `Enter`，也是换行。
- 会话在你发出第一条消息时才启动，不会提前。
- 如果项目自带 Claude Code 的配置（`.claude/`、`.mcp.json` 或 `CLAUDE.md`），还要等你先回答一个问题：Eitri 会先停住，问你信不信任这份配置。`y` 加载它，`n` 不加载、直接启动会话，`Esc` 先不启动、把你写的字还给你。回答之前，面板会显示「Waiting for your answer to the trust question — nothing has started」。这个问题显示什么、每个回答意味着什么，见[信任一个项目](permissions.zh-CN.md#trusting-a-project)。

每条消息还会带上编辑器里打开的文件名，或者你在 Visual 模式下选中的那几行。想让 agent 看某一段代码，就先在编辑器里选中它，再说「修一下这个函数」；只有光标位置的话，什么都不会发过去。选中几行后，在编辑器里按 `<leader>ae` 可以省一步：按键焦点跳到面板，输入框已经打开，里面写好了 `Edit the selection: `，你接着写就是一条普通消息（细节见[按键](keys.zh-CN.md#editing-the-selection-from-neovim)）。

Claude 回答时，回复一边流式输出一边渲染成 Markdown。agent 跑的每个工具单独一行，颜色较淡，前面的符号表示状态：完成（`✓`）、失败（`✗`）、还在跑（`◐`）。比如一条命令、读了一个文件、一次搜索。每次改文件是一张小卡片，写着文件名和增删的行数，里面是 diff。

- `Esc` 离开输入框，回到浏览对话。浏览时 `j` / `k` 一行一行地走，`Enter` 显示或隐藏一个工具的结果，`zi` 切到详细视图（显示所有结果，摘录更长），`yy` 复制这一行（同时复制到剪贴板和 primary selection，中键也能粘贴；`y` 加一个文本对象可以复制一行的一部分），`F1` 列出所有按键。
- `Ctrl+c` 打断正在跑的回合。
- 回合进行中输入消息再按 `Enter`，消息会插进这个回合，回合不停，和 Claude Code、Codex 里一样：agent 在下一步读到它。在那之前，它以 `↪ <消息>` 显示在输入框上方，agent 读到后出现在对话里对应的位置。如果这个标签页的 agent 不能在回合中途接收消息，消息会先留着，显示为 `⧗ <消息>`，回合结束后再发；空输入框里写着这个标签页的 `Enter` 是哪一种。Claude 和 Codex 标签页能中途接收，ACP 标签页先留着。回合结束前 agent 没来得及读到的消息（比如你打断了回合），会重新留着，并附一行说明原因。
- `Ctrl+Enter` 立刻发：先打断正在跑的回合，再把留着的和这条一起发出去。`Ctrl+x` 再按 `Enter` 是等回合结束再发，不插进这个回合。
- 在输入框里按 `Ctrl+g`，用 Neovim 编辑你的草稿，`:wq` 带回来。

改过文件的回合结束后，浏览时按 `c` 看这个回合期间磁盘上改了什么，见[审阅一个回合](turn-review.zh-CN.md)。

<a id="arranging-the-window"></a>
## 摆一摆窗口

第一个回合之后，可以试着排一排模块。这里的 `<prefix>` 是按 `Ctrl+b`，松开，再按下一个键或组合键，和 tmux 一样。下面是默认按键；如果 Eitri 读到了你的 tmux 绑定，见[你的 tmux 按键](keys.zh-CN.md#your-tmux-keys)；整个从 tmux 迁过来的过程，见[从 tmux 迁过来](migrate-from-tmux.zh-CN.md)。

1. 焦点在 agent 面板时，按 `<prefix> "`，再按 `t`：底部终端在面板下面打开，焦点也过去。`<prefix> %` 是往右分。两种分屏都会等你选一个模块打开或移过去。
2. 按 `<prefix> o` 把焦点移到屏幕上的下一个模块，一直按，直到回到终端。`<prefix> Left`、`Right`、`Up`、`Down` 去那个方向的模块。
3. 在终端里按 `<prefix> Alt+Enter`，在它右边开第二个终端，shell 也从项目根目录启动。
4. 焦点在第二个终端时，按 `<prefix> F` 让它浮起来。再按 `<prefix> _` 把它收进 scratchpad。`<prefix> S` 把它叫回来，再按一次又收回去。
5. 按 `<prefix> s`，再按 `c`，复制当前的布局建一个新工作区并切过去。`<prefix> L` 回到上一个工作区。模块是共用的，每个工作区各自记着自己的布局。
6. 回到最初那个 agent 面板，按 `<prefix> c` 加一个对话标签页，再按 `<prefix> !` 把它拆出来，在面板旁边单独成一个视图。最初那个视图至少要留一个标签页。在新视图里再按 `<prefix> !`，标签页就合回去。
7. 想给没有保存过布局的项目定一个初始布局，在 `$XDG_CONFIG_HOME/eitri/init.lua`（默认 `~/.config/eitri/init.lua`）里写 `eitri.layout.default(...)`。窗口开着时，`eitri.layout.export()` 会返回可以贴进去的布局调用，工作区和浮动窗口都在里面；怎样用一个 Lua 命令把它存下来，见[配置](configuration.zh-CN.md#layout-from-lua)。带编号的对话视图不会导出：它们要有一个保存过的标签页才能在启动后留下来，所以新视图放在哪里，用[打开规则](configuration.zh-CN.md#where-a-new-module-opens)来定。

标签式和堆叠式容器、调整大小以及其他布局键，见[按键](keys.zh-CN.md#前缀)。

<a id="when-eitri-asks"></a>
## Eitri 什么时候会问你

有些工具调用会等你。Eitri 在对话里、紧挨着那次调用画一张权限卡片，上面有 **Approve** 和 **Deny** 两个按钮；很多命令中间还有第三个按钮「Always allow ... in this project」，它会存一条规则，以后同类命令不再弹卡片。规则管不到 Claude Code 自己提出的问题，所以它还要问的命令照样会有卡片，见 [Claude Code 自己问的时候](permissions.zh-CN.md#when-claude-code-itself-asks)。

回答卡片要进入**卡片模式**，卡片会一直占着按键，直到你回答或者离开。按 `Alt-n`（在输入框里也行）接过这个标签页里下一张在等的卡片，或者在卡片那一行上按 `Enter`。有卡片来时，状态条会提示。

- 数字对应按钮，顺序和 Claude Code 一样：`1` 是允许，`2` 是拒绝；卡片上有「Always allow」时，`2` 是允许并存规则，`3` 是拒绝。
- 想走到某个按钮上，用 `h` / `l`，再按 `Enter`。
- 在 Claude 的卡片上，`Tab` 打开一个写理由的框，在里面按 `Enter` 就带着理由拒绝。
- `Esc` 离开卡片模式，卡片继续等着，你回到原来的位置。它不会自己跳到下一张卡片。

字母不会回答卡片：`a`、`d`、`D` 都没反应，输入框里打的字也不会。数字要等卡片模式就绪后才算数（数字从暗变亮，大约三分之一秒），而且如果大约四分之一秒内后面又跟了别的键，这个数字也不算。这样你正在打一句话时，开头几个字母不会误答卡片。完整的键见[按键](keys.zh-CN.md#answering-a-card)。

**Manual、Auto 和 Bypass。** 每个标签页处在三种模式之一，`Shift+Tab`（或 `<leader>m`）按 Claude Code 的顺序轮换：manual、auto、bypass，然后回到 manual。

- **Manual** 最严。每次调用都由 Eitri 自己的规则来定：项目内的读取和只读命令直接跑，你保存的规则照样生效，其他的全是卡片，每次改文件、写文件也是。不交给 Claude Code 自己的 auto 模式。想让每个标签页都从 manual 开始，在 `init.lua` 里把 `agent.default_mode` 设成 `"manual"`。
- **Auto** 是默认。Eitri 仍然看得到每次调用。你保存的规则允许的，马上放行；其余的交给 Claude Code 自己的 auto 模式判断，不弹卡片。它拒绝一次调用时，那一行会写 `blocked by auto: <原因>`；连续拒绝几次后，Claude Code 会自己用卡片来问你。如果你的 Claude Code 跑不了它的 auto 模式，就改由 Eitri 自己的规则决定哪些调用要卡片。
- **Bypass** 让 Eitri 自己批准每一次工具调用，不弹卡片：它自己的检查、Claude Code 自己的问题，包括你用 `permissions.ask` 规则强制要问的，都一并批准。因为这要给很大的信任，`Shift+Tab` 切进 bypass 前会先问一次（一个 `y/n` 问题，会说明还有什么跟着变，比如会批准几张正在等的卡片），而且 `y` 要单独按、并且在问题出现片刻之后按才算。只能从 auto 切进 bypass；离开 bypass 不用回答问题，会落到 manual。

每种模式具体放行什么、保存的规则、怎样信任项目自带的 Claude Code 配置，见[权限与信任](permissions.zh-CN.md)。

<a id="tabs-and-sessions"></a>
## 标签页和会话

每个对话是一个标签页。有两个以上时才显示标签栏。

| 键 | 做什么 |
|---|---|
| `<prefix> c` | 新标签页（空的，带上面那个菜单） |
| `<prefix> n` / `<prefix> p` | 下一个 / 上一个标签页 |
| `<prefix> 1` ... `9` | 按编号去某个标签页 |
| `<prefix> l` | 刚才那个标签页 |
| `<prefix> ,` | 改标签页名字 |
| `<prefix> &` | 关掉标签页（先问一次；有回合在跑、会被打断时会说明） |
| `<prefix> w` | 列出开着的标签页和保存的会话，用来切换或接着聊 |
| `<prefix> i` | 这个标签页的详情：名字、模式、模型、会话 id、加载了哪些设置、项目是否受信任 |

在面板里，`H` / `L`（或者 `gT` / `gt`）也能去上一个 / 下一个标签页。每个标签页跑一个 agent，默认是 Claude；`:tabnew codex` 开一个 Codex 标签页，你注册的 agent 也能开标签页，见 [Codex 与其他 agent](agents.zh-CN.md)。几个标签页同时跑：后台的标签页继续输出，标签栏上的标记告诉你它需要什么（`⚑` 有卡片在等，`•` 你不在时有回合结束了，`✕` 会话异常结束）。

**接着之前的对话。** 关掉标签页不会删掉对话。空标签页上按 `r` 接着最近保存的会话；`<prefix> w`（或空标签页上的 `w`）列出所有会话，最新的在前，每个带名字或标题。别的窗口正占着的会话会列出来，但选不了。每个项目保留最新的 200 个会话：存满时列表会写 `only the newest 200 are kept`，再开新会话就把最旧的一个从列表里去掉（那个对话在 Claude Code 自己的 `claude --resume` 里还在）。

**恢复上次的标签页。** Eitri 记得这个项目上次窗口里开着哪些标签页：顺序、名字、模式，以及当时显示的是哪一个。默认情况下空标签页会提供 `Restore last session`（面板里按 `s`）。按键焦点一开始在编辑器里，那里的 `s` 是 nvim 的，所以空标签页会提示把焦点移到面板的键（`Ctrl+l to use these keys`）；在编辑器里按 `<prefix> w` 也会打开会话列表，光标停在 `Restore last session` 那一行，按 `Enter` 就行。在 `init.lua` 里把 `agent.restore` 设成 `"auto"`，启动时不用按键就自动恢复；设成 `"off"` 既不提供也不记录，见[配置](configuration.zh-CN.md)。保存的对话已经不在、或者被别的窗口占着的标签页会被跳过，并在一条短消息里点名。原来在 bypass 的标签页不会不经询问就以 bypass 回来：`s` 会问（`n` 以 Auto 恢复），自动恢复则以 Auto 恢复并说明。原来在 manual 的回来还是 manual。唯一的例外是你提前给过答复：`init.lua` 里 `agent.default_mode` 设成 `"bypass"` 时，保存的 bypass 标签页直接以 bypass 回来，不问。

标签页里没发出去的文字会跟它一起保存，按 `s` 时回到输入框里，最多 256 KiB。回不来的文字（标签页还没有会话，或者太长存不下）会在关窗口时的问题里点数：`close window? 1 unsent draft (y/n)`。不按 `s`、而是新开别的，会替换掉保存的内容，没发的文字也一起替换。

**到真正的终端里接着聊。** 按 `<leader>t`（或者打开 `<prefix> i`，选 `Continue in a terminal...`）。Eitri 会先说明它要先在面板里关掉这个对话，你确认后，它给出在你自己的终端里接着聊的 `claude --resume ...` 命令，按 `y` 复制。Eitri 不会替你开那个终端，也不给会话上锁，所以别在两个地方同时开着它。那个终端里跑的是普通的 `claude`，用你自己的 Claude Code 设置，不经过 Eitri 的权限检查，所以它的工具调用不会显示成卡片。还没跑过第一个回合的会话没有东西可接，有回合正在跑的也不行。

<a id="when-you-start-again"></a>
## 再次启动时

关掉 Eitri（在编辑器里 `:qa` 会关窗口，就像关 Neovim 一样，直接关窗口也行；只有会打断什么、比如有回合在跑时，才会先问），或者因为崩溃、重启丢了它，下次在同一个项目里启动，编辑器会回到你离开时的样子。窗口里开着的文件按原来的分屏回来，在同样的 tab page 里，每个窗口回到原来的行和滚动位置，光标也在原来那个窗口里。列在 buffer 里但没显示的文件回到 `:ls`。分屏保持比例，30/70 的分屏放到窄一点的窗格里还是 30/70。全部恢复成功时，Eitri 什么都不说。

Eitri 在它的状态目录里存一个小文件，记着开了什么：每 30 秒、窗口失去焦点时、编辑器退出时（`:qa`、关窗口、SIGTERM）各更新一次。如果 Eitri 被直接杀掉，下次启动用的是最后一次更新的记录：通常会丢掉最后 30 秒里的窗口变动和光标移动，编辑器当时忙着没响应的话丢得更多。这个文件只记文件名和位置，不记文字内容。没保存的文字和在普通 Neovim 里一样安全：你的配置开着 swap、而且 Neovim 已经写过 swap（默认是打了 200 个字符或停手 4 秒后）时，它在 swap 文件里；用了 `noswapfile` 就没有。

不会恢复的：

- 折叠，窗口和 buffer 的选项（`wrap`、`spell`、`number`，这些由你自己的配置来设）；
- 工作目录，包括窗口级和 tab 级的；
- 跳转列表、mark、寄存器、命令历史和 undo，这些 Neovim 自己管，Eitri 不碰；
- 终端 buffer、帮助、quickfix 和 location list、浮动窗口、没有名字的 buffer；
- 从没写进磁盘的文件，以及到下次启动时已经不在、或者不再是普通文件的文件；
- Eitri 自己的视图：插件的 diff 标签页、回合审阅的 buffer。

有东西没恢复时，会用一行短消息说明：`2 files from last time are gone: src/old.rs and 1 more`、`could not reopen src/x.rs`，或者 `3 files from last time did not fit on screen; they are in :ls`。如果某个文件在 swap 里有之前那个编辑器没保存就退出的修改，它以只读方式打开，后面跟着平常那行关于未保存修改的提示（`:e` 这个文件，再按 `r` 恢复）。如果上一次恢复没做完（Neovim 打开文件时崩了或卡住了），下次启动什么都不打开，说明一次，等写出新的记录后再试。

**什么时候 Eitri 不插手。** 如果你用会话插件，重新打开文件的事交给它：Eitri 不恢复、不记录，也不说话。它认得这些：`persistence.nvim`（LazyVim 用的）、`auto-session`、`resession.nvim`（AstroNvim 用的）、`possession.nvim`、`nvim-possession`、`neovim-session-manager`、`persisted.nvim`、`mini.sessions`、设了 `g:startify_session_autoload` 的 vim-startify、vim-prosession，以及你的配置已经加载过的会话。它只看已经加载或声明了的东西，从不替你加载或设置任何插件。不在这个名单上的会话工具会和 Eitri 一起恢复，你的文件就会开两遍；这时把 `editor.restore` 设成 `false`（见[配置](configuration.zh-CN.md#settings)）关掉 Eitri 这边。另外这些情况 Eitri 也不恢复：带文件启动（`eitri FILE`）、你的配置在编辑器显示前已经打开了文件、`eitri --clean`、第二个窗口、[companion](companion.zh-CN.md) 窗口（那里的 Neovim 是你自己的）。同一个项目的第二个窗口，如果没和第一个配对，就各记各的，从不打开第一个窗口的记录。

**配置开头的 `autocmd!`。** Eitri 的恢复靠的是在你的 `init.lua` 运行之前建好的 autocommand。配置顶层一个光秃秃的 `autocmd!` 会清掉所有组的 autocommand，Eitri 的也在内：那一次运行什么都不恢复、什么都不记录，上一次正常运行留下的文件保持原样。把 `autocmd!` 放进它所属的那个 `augroup` 里，就只清那一组。

<a id="when-something-ends"></a>
## 回合没有正常结束时

没有正常结束的回合，看起来和正常结束的不一样。它停下的地方会出现一行斜体的淡色说明，面板底部的状态行也会用几个字说明，直到你发下一条提示：

| 发生了什么 | 那一行写着 | 状态行写着 |
|---|---|---|
| 你按了 `Ctrl+c` | `interrupted` | `interrupted` |
| 出错 | `the turn ended with an error (...)`，后面是 Claude Code 自己的话 | `turn failed` |
| 触到速率或用量上限（HTTP 429） | `rate or usage limit reached (HTTP 429)`，后面是 Claude Code 的话 | `rate limited` |
| 到了回合数上限或花费上限 | `stopped at the turn limit` 或 `stopped at the spending limit` | `turn limit` 或 `spending limit` |
| 对话装不下了 | `stopped: the context is full` | `context full` |
| 你的 hook 让它停了 | `stopped by a hook` | `stopped by a hook` |
| 会话在回合中途结束 | `the turn did not finish: the session ended` | `turn did not finish` |

冒号后面的文字是 Claude Code 原样给的，所以只有 Claude Code 写了重置时间，你才看得到。这一行留在对话里，面板重新加载（`<prefix> r`）和切换标签页之后还在，可以像别的行一样用 `yy` 复制。

会话本身没了的话，对话里会说明。红色的 `This session was lost` 表示连接断了，上面显示的内容可能不全；淡色的 `This session has ended (<原因>)` 表示会话已经关闭。两种情况都可以在浏览时按 `r`，在这个标签页里开一个新会话。回合进行中留着的消息，在那个回合结束后仍然会发出去，就算回合是因为上限而结束的也一样。

如果 nvim 自己没了（被杀掉或崩溃），窗口还在，并且会说：`nvim stopped unexpectedly — <prefix> e starts it again and says what can be recovered`。最后一个 buffer 上的 `:q` 照样关窗口，跟普通 Neovim 一样。启动时如果发现这个项目有文件在 swap 里留着未保存的修改，会提示一次：`unsaved changes kept for long.txt — :e the file, then r recovers them or d deletes them`。你不这样做就什么都不会恢复，swap 文件还在的话，每次启动都会再提示。

如果 Eitri 要存的东西写不进去了，每类问题提示一次，直到恢复正常，说明哪些没存上：`could not save the open tabs`、`could not record this session` 或 `could not store the history`，原因通常是 `the disk is full`。回合照常跑。腾出空间后会自己补上。

Claude Code 自己更新之后面板不正常了，可能是它背后的 sidecar 也需要更新。`eitri sidecar check` 查有没有更新，`eitri sidecar update` 装上，`eitri sidecar use bundled` 改回自带的。不会自动去查，已经开着的标签页继续用它启动时的 sidecar。Eitri 0.3.0 没有 `eitri sidecar` 命令（会回答 `eitri: no such directory`），要先装 0.3.0 之后的版本。细节见[更新 sidecar](../../INSTALL.zh-CN.md#updating-the-sidecar)。

<a id="when-eitri-itself-crashes"></a>
## Eitri 自己崩溃时

崩溃时会写一份记录到 `~/.local/state/eitri/log/crash-<时间>-<pid>.log`（设了 `$XDG_STATE_HOME` 的话在它下面），只有你能读。下次启动会用一行提示一次，带着文件路径。要报告的话，把这个文件附到 [bug 报告](https://github.com/HunterGrey-cyber/eitri/issues/new?template=bug_report.yml)里。你不附上，它就不会离开你的机器：Eitri 不往任何地方发东西。

记录是纯文本：版本、构建、内核、线程、代码停在哪里、backtrace，以及窗口最后打印的几行。里面没有提示、对话或工具输出；崩溃消息本身可能引用这些内容时，也会被去掉。路径是有的，home 目录显示成 `~`，所以附上之前先看一遍。

从桌面、从项目列表启动的窗口，或者从终端正常启动的窗口，会把输出存在同一个目录的 `main-<16 位十六进制>.log` 里，每个项目一个名字；到 2 MiB 时文件挪成 `.log.1`，新的一次运行开始时上一次的也会挪过去。只有 `eitri --no-fork` 让窗口留在前台、把输出打印到那个终端，不留窗口日志。磁盘满了的话丢掉这些行，窗口照常跑。`EITRI_WINDOW_LOG=off` 关掉日志和崩溃记录。

原生库（GTK、WebKit、显卡驱动）里的崩溃不会写记录。到那时为止窗口打印了什么，在日志里。

已知的毛病列在[已知问题](../known-issues.zh-CN.md)里。

---

## 细节

主线里没放的几段较长的说明。

<a id="where-eitri-looks-for-claude"></a>
### Eitri 去哪里找 `claude`

Eitri 运行它的 `PATH` 上的 `claude`。从桌面启动器或菜单打开的窗口常常在 `PATH` 上找不到它，这时 Eitri 会按这个顺序去 Claude Code 的安装程序放它的地方找：

1. `~/.local/bin/claude`（快速上手页面里那个安装程序装的）
2. `~/.claude/local/claude`（较早的本地安装）
3. 你自己设过的 npm 全局前缀下的 `bin` 目录（`NPM_CONFIG_PREFIX`，或者 `~/.npmrc` 里的 `prefix`）

它取第一个满足这些条件的：是一个你能运行的普通文件，属于你或 root，而且它本身和通往它的每一级目录（链接会跟过去）都不能被其他用户写。不满足的跳过，窗口日志里写着原因。把 `VERDANDI_CLAUDE_CLI_PATH` 设成一个路径就是你自己指定，Eitri 不再去别处找。

找不到 `claude` 时，首次启动界面和 Claude 标签页的失败提示都会说明，列出找过的地方，并给出获取地址 <https://code.claude.com/docs/en/quickstart>；装好后在那个标签页里按 `r`。

<a id="desktop-path"></a>
### 从桌面启动时的 `PATH`

从桌面启动器、菜单或合成器快捷键启动的窗口，拿到的是桌面会话给它的 `PATH`，常常缺了你 shell 配置里加的东西（`~/.local/bin`、`~/.cargo/bin`、版本管理器的目录）。所以这样启动时，Eitri 会问一次你的登录 shell（`$SHELL -i -l -c`，最多 2 秒）要它的 `PATH`，给窗口、它的 Neovim、底部终端和 agent 用。shell 配置很重的话，启动会多花这么一段时间（带 banner 的 zsh 大约 0.3 秒）。这时你的配置能看到 `EITRI_RESOLVING_ENVIRONMENT=1`，想跳过什么可以据此判断。shell 太慢或者出错的话，窗口保留原来拿到的 `PATH`，日志里写着原因。不想要这一步，设 `EITRI_LOGIN_PATH=off`。另一个办法是让桌面启动的所有程序都拿到同样的 `PATH`：在 `~/.config/environment.d/` 里放一个文件（比如 `10-path.conf` 里写 `PATH=$HOME/.local/bin:$PATH`），systemd 的用户会话登录时会读它。

<a id="directory-arguments"></a>
### 目录参数的细节

不存在的文件夹里的路径、或者 Eitri 不认识的选项，会让启动停下并给出消息，不会去打开别的目录。要打开名字以 `-` 开头的目录，在前面加 `--`（`eitri -- -notes`）。如果文件夹存在、但里面没有这个名字，它会被当成新文件，所以打错目录名会打开一个同名的空 buffer。

<a id="open-a-file"></a>
### 打开文件

```sh
eitri src/lib.rs
eitri +42 src/lib.rs
eitri src/lib.rs:42:7
eitri a.rs b.rs
eitri DIR FILE
```

`eitri FILE` 在编辑器里打开这个文件，并从它推出项目：项目是文件所在的 git 仓库，没有仓库的话就是文件自己所在的文件夹。例外是：你的当前目录包含这个文件、本身是个可用的文件夹、并且（有仓库的话）在仓库里面时，用当前目录（`cd crate; eitri src/x.rs` 打开的项目是 `crate`，和在那里 `eitri .` 一样）。仓库本身也要是可用的文件夹才算（见[不能当边界的文件夹](permissions.zh-CN.md#a-folder-that-is-no-boundary)）；不算的话跳过它，继续往上找。在 home 目录里、没有仓库时，`eitri code/scratch/a.py` 打开的项目是 `~/code/scratch`，不是 `~`。这样推出来的项目和 `eitri DIR` 指定的一样，包括第一次开会话时的[信任问题](permissions.zh-CN.md#trusting-a-project)。文件不必已经存在：它所在的文件夹要存在，`:w` 会创建文件。

- 几个文件时，显示第一个，全部打开，和 `nvim a b c` 一样。项目外的文件会提示一句，照样打开。
- 第一个文件前的 `+N` 从第 N 行开始，单独一个 `+` 从最后一行开始。`FILE:LINE` 和 `FILE:LINE:COL` 效果相同，但只在不存在字面上叫这个名字的文件时才这样解释。行号只对第一个文件有效。`+` 后面跟的不是数字（比如 `+page.svelte` 或 `+/pattern`）会被拒绝，因为那会变成一条 nvim 命令；真有这个名字的文件，写 `./+page.svelte` 或 `eitri -- +page.svelte`。
- `--` 之后的每个词都是路径。
- `eitri DIR FILE...` 以 `DIR` 为项目，先显示这些文件。
- 不存在的文件夹、管道、socket、设备，以及不是合法 UTF-8 的名字，都会被拒绝并给出消息。`eitri panel` 和 `eitri split` 仍然只接受目录，`EITRI_PROJECT_DIR` 也仍然是目录。

**这个项目已经有窗口开着。** 这时 `eitri FILE` 不会再开一个窗口：正在跑的编辑器打开这个文件，窗口提到前面，焦点在编辑器上，命令马上返回。如果那个窗口的编辑器是隐藏的或者还没启动，会先显示它，准备好后再打开文件。在 Eitri 窗口自己的底部终端里敲 `eitri FILE`，只要文件在这个窗口的项目里，就在这个窗口打开，`cd` 进子目录之后也一样。`eitri DIR` 和不带参数的 `eitri` 总是开新窗口，`eitri --new-window FILE` 也是（`--clean` 和 `--account` 隐含了它）。`VERDANDI_CLAUDE_ACCOUNT` 里设的账号不隐含它：正在跑的窗口保持它原来的账号。在底部终端之外，项目上层文件夹的窗口也算数，一直往上到文件所在的仓库为止，近的优先：`cd repo/crate; eitri src/x.rs` 推出的项目是 `crate`，没有 `crate` 的窗口时，会在 `repo` 的窗口里打开。装不下你给的所有文件的窗口会被跳过；没有仓库时只看项目本身。没有合适的窗口就开新的；窗口暂时接不了文件时（比如正停在一个有未保存 buffer 的退出提示上）也开新的。编辑器里当前 buffer 有未保存修改时，文件在一个分屏里打开，那个 buffer 保持不动。

GNOME 上只有带激活令牌的启动才能把窗口提到前面，终端里敲的命令没有这个令牌，所以 GNOME 可能显示「窗口已就绪」的通知，而不是把窗口提上来。sway、Hyprland 和 niri 预计会由合成器来聚焦窗口，只在 sway 上试过。

**没有自己项目的文件。** `eitri ~/notes.md`、`eitri /etc/hosts`、`eitri /tmp/x.py` 也能打开，项目是文件所在的文件夹。如果这个文件夹不能当 agent 的边界（你的 home 目录、不属于你的文件夹、其他用户能写的文件夹、文件系统根目录），面板会在每个标签页上显示一条说明：`No project boundary here (<why>): a Claude tab asks before every file and command, and a Codex tab starts read-only, without the folder's configuration. Other agents keep their own rules.` 它一直显示，直到你关掉。这对每种模式意味着什么，见[权限与信任](permissions.zh-CN.md#a-folder-that-is-no-boundary)。

**从文件管理器打开。** Eitri 的桌面入口登记了纯文本、Markdown、C、C++、Python、Rust、JSON、TOML 和 shell 脚本，所以这些文件的「打开方式」里有它（不会变成默认程序）。这样启动通常从你的 home 目录开始，项目按上面说的从文件推出来。一起选中的几个文件和 `eitri a.rs b.rs` 一样打开，几个文件夹各开一个窗口。

<a id="more-on-projects"></a>
### 项目列表的更多用法

要找列表里没有的项目，在某个项目上按 `h`，会显示装着它的那个文件夹和它的兄弟文件夹，并选中这个项目。`l` 或 `Enter` 进入文件夹，`h` 回到上一级，在 `.` 那一行上按 `Enter` 把当前所在的文件夹作为项目打开。在输入行里输入路径，也会这样显示那个文件夹的子文件夹；网络共享上没有响应的文件夹会直接说明，不会让窗口一直等。按键见[按键](keys.zh-CN.md#前缀)。

被提到前面的窗口保持原样，用它启动时的 Claude 账号，就算你是从另一个账号的窗口或桌面入口选的它；`eitri --account <name> --new-window <dir>` 用那个账号给这个项目再开一个窗口。新窗口起不来时，会有一条短消息说文件夹不在，或者让你去看那个项目的窗口日志（见 [Eitri 自己崩溃时](#when-eitri-itself-crashes)）。
