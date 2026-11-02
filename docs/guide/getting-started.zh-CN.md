[English](getting-started.md) | 简体中文
<!-- translated-from: getting-started.md sha256=ac0fb858affa35c5800aaa2f04ec4139d210de2e38105aef1521106ad53197fd -->

# 快速上手

这一页带你从命令走到第一个完成的回合：打开一个项目、发送提示、回应 Eitri 弹出的卡片、同时使用多个会话，以及某个回合没有顺利结束时面板会说什么。它假定 Eitri 已经[安装好](../../INSTALL.zh-CN.md)，并且 `claude` 命令已经登录到你的账号。如果 Eitri 找不到 `claude`，首次启动界面和 Claude 标签页的失败提示都会说明，并给出获取地址 <https://code.claude.com/docs/en/quickstart>；装好后确认 `claude` 能在终端里运行，再在该标签页里按 `r`。

文中的按键按默认设置来写：前缀是 `Ctrl+b`，“leader”是 `Space`，除非你的 Neovim 配置设了别的、面板能用的单个键的 `mapleader`（更长的或被保留的会退回到 `Space`）。完整的按键列表在[按键](keys.zh-CN.md)里，在面板里按 `?` 也能看到。

<a id="the-first-launch"></a>
## 第一次启动

如果你还没有 `init.lua`，第一个窗口会先打开一个简短的设置界面。它列出在你电脑上找到的东西（tmux 配置和它的前缀、你的桌面、可能和 Eitri 按键冲突的 Neovim 映射、正在运行的输入法），然后问窗口的按键要怎么用。在 Hyprland、sway 或 niri 上，它还提供并选中一个由窗口管理器排布聊天和页面的预设（[合成器窗口](compositor-windows.zh-CN.md)）：

- `j` 和 `k`，或预设的数字，用来选择；`Enter` 进入复查，复查会展示它要写的行。
- 复查时按 `Enter` 写入 `~/.config/eitri/init.lua`（只写一次，绝不覆盖已有的文件）；`Backspace` 返回。新按键和 `window.tiling` 这一行（如果有）都从下次启动起生效。写完后，`t` 打开 `:EitriTutor`，`Enter` 关闭。
- `Esc` 是“以后再说”：什么都不写，下次启动时界面再问一次。`d` 让它永远不再出现。以后可以用 `:Eitri setup` 叫回来。

你更想自己写文件的话，按两次 `Esc` 或按 `d`；界面提供的一切都在[按键](keys.zh-CN.md)和[配置](configuration.zh-CN.md#first-start)里。

<a id="open-a-project"></a>
## 打开一个项目

```sh
eitri [DIR]
```

`DIR` 是项目目录。不写的话，Eitri 在设置了 `$EITRI_PROJECT_DIR` 时用它，否则用你运行命令时所在的目录。如果路径所在的文件夹不存在，或者给了 Eitri 不认识的选项，启动会直接带着一条消息停下，而不是去打开别的目录；要打开名字以 `-` 开头的目录，在它前面加 `--`（`eitri -- -notes`）。文件夹存在而名字不存在时，这个名字算新文件，所以目录名打错时会打开一个以它命名的空缓冲区。要打开文件而不是目录，见[打开文件](#open-a-file)。

打开的每个目录都是一个独立的窗口，有自己的 Neovim、自己的会话和自己的项目根目录，所以你可以同时打开好几个项目。窗口标题写着项目名（`myproject — Eitri`；有两个工作区时再加上当前显示的那个：`myproject — Eitri · docs`），窗口切换器和合成器的窗口规则读的就是它。

窗口一侧是你的编辑器，另一侧是 agent 面板。编辑器就是你自己的 Neovim，带着你的配置和插件。面板从一个空标签页开始，上面显示项目目录、配置了 Claude 账号时的账号名，以及一个简短的菜单：

| 按键 | 作用 |
|---|---|
| `i` | 通过输入开始一个新会话 |
| `s` | 恢复这个项目上一个窗口里开着的标签页（只在有可恢复的内容时显示） |
| `r` | 继续最近一次保存的对话（只在有的时候显示） |
| `w` | 在所有保存过的会话里选一个 |
| `Shift+Tab` | 在开始之前轮换模式（Manual、Auto、Bypass；当前模式显示在面板底部） |
| `?` | 列出所有按键 |

`Ctrl+h` 和 `Ctrl+l` 在编辑器和面板之间移动按键焦点。

有两个选项值得知道。`eitri --version` 打印版本后退出。`eitri --clean` 用 `nvim --clean` 启动编辑器里的 Neovim，也就是不带你的配置和插件，这是判断问题是否出在你自己的设置上最快的办法。

<a id="open-a-file"></a>
## 打开文件

```sh
eitri src/lib.rs
eitri +42 src/lib.rs
eitri src/lib.rs:42:7
eitri a.rs b.rs
eitri DIR FILE
```

`eitri FILE` 在编辑器里打开这个文件，并从文件推出项目：项目是文件所在的 git 仓库，没有仓库时是文件自己所在的文件夹；例外是当前目录包含这个文件、它本身是可用的文件夹、并且（有仓库时）位于仓库之内，这时用当前目录（`cd crate; eitri src/x.rs` 打开的项目是 `crate`，和在那里运行 `eitri .` 一样）。仓库也必须是可用的文件夹才算数（见[不算边界的文件夹](permissions.zh-CN.md#a-folder-that-is-no-boundary)）；不可用的仓库会被跳过，继续往上找。在主目录里、没有仓库时运行 `eitri code/scratch/a.py`，项目是 `~/code/scratch`，不是 `~`。这样得到的项目和用 `eitri DIR` 指定的一样，包括第一次启动会话时的[信任问题](permissions.zh-CN.md#trusting-a-project)。文件不必已经存在：它所在的文件夹必须存在，`:w` 会创建这个文件。

- 给了好几个文件时，第一个显示出来，全部都已打开，和 `nvim a b c` 一样。项目之外的文件会打印一条提示，仍然打开。
- 第一个文件之前的 `+N` 让它从第 N 行开始，单独一个 `+` 表示最后一行。`FILE:LINE` 和 `FILE:LINE:COL` 效果相同，但只在不存在字面同名文件时才这样解释。行号只作用于第一个文件。`+` 后面如果不是数字，比如 `+page.svelte` 或 `+/pattern`，会被拒绝，因为那会是一条 nvim 命令；要打开这样名字的文件，写 `./+page.svelte` 或 `eitri -- +page.svelte`。
- `--` 之后的每个词都是路径。
- `eitri DIR FILE...` 仍以 `DIR` 为项目，并先显示这些文件。
- 不存在的文件夹、管道、套接字或设备文件，以及不是有效 UTF-8 的名字，会带着一条消息被拒绝。`eitri panel` 和 `eitri split` 仍然只接受目录，`EITRI_PROJECT_DIR` 也仍然只是目录。

**这个项目已经开着一个窗口。** 这时 `eitri FILE` 不会再开一个窗口：正在运行的编辑器打开这个文件，窗口被带到前面、按键交给编辑器，命令立刻返回。如果那个窗口的编辑器是隐藏的或还没启动，会先显示出来，等它就绪后再打开文件。在某个 Eitri 窗口自己的底部终端里输入 `eitri FILE`，文件就在那个窗口里打开，即使你已经 `cd` 进了子文件夹，只要文件在该窗口的项目之内。`eitri DIR` 和不带参数的 `eitri` 总是开新窗口，`eitri --new-window FILE` 也是（`--clean` 和 `--account` 隐含它）。写在 `VERDANDI_CLAUDE_ACCOUNT` 里的账号不隐含它：正在运行的窗口保持自己的账号。在底部终端之外，这里推出的项目以上的文件夹上的窗口也算，一直到文件所在的仓库，由近及远：`cd repo/crate; eitri src/x.rs` 推出的是 `crate`，如果 `crate` 上没有开着的窗口而 `repo` 上有，文件就在 `repo` 的窗口里打开。放不下你点名的全部文件的窗口会被跳过，没有仓库时只问这个项目自己。没有这样的窗口时开新窗口；窗口此刻无法接收文件时也开新窗口，比如它停在带未保存缓冲区的退出提示上。编辑器里当前缓冲区有未保存的修改时，文件在一个分割窗口里打开，那个缓冲区保持不动。

在 GNOME 上，只有带激活令牌的启动才能把窗口提到前面，而在终端里输入的命令没有这个令牌：那里 GNOME 可能显示一条“窗口已就绪”的通知，而不是把窗口提到前面。预计 Sway、Hyprland 和 niri 会通过合成器把焦点交给窗口。只在 sway 上试过。

**没有自己项目的文件。** `eitri ~/notes.md`、`eitri /etc/hosts` 和 `eitri /tmp/x.py` 也能打开，项目就是文件所在的文件夹。当这个文件夹不能作为 agent 的边界时（你的主目录、不属于你的文件夹、别的用户可写的文件夹、文件系统根），面板的每个页面都会显示一条提示：`No project boundary here (<why>): a Claude tab asks before every file and command, and a Codex tab starts read-only, without the folder's configuration. Other agents keep their own rules.` 它会一直留着，直到你关掉它。各个模式下这意味着什么，见[权限](permissions.zh-CN.md#a-folder-that-is-no-boundary)。

**从文件管理器打开。** Eitri 的桌面入口列出了纯文本、Markdown、C、C++、Python、Rust、JSON、TOML 和 shell 脚本，所以“打开方式”里会出现它（它不会成为默认程序）。这样的启动通常从你的主目录开始，所以项目按上面所说的从文件推出来。选中的多个文件像 `eitri a.rs b.rs` 那样一起打开，选中的多个文件夹各开一个窗口。

<a id="your-first-turn"></a>
## 你的第一个回合

按 `Ctrl+l` 把焦点移到面板，再按 `i`（或 `Ctrl+j`）输入你的提示。如果你重新绑定了模块焦点键，就用那个键代替 `Ctrl+l`。`Enter` 发送；`Shift+Enter` 换行，`Alt+Enter` 以及反斜杠加 `Enter` 也一样。会话在你发出第一条消息时才启动，不会更早；而且在一个自带 Claude Code 配置（`.claude/`、`.mcp.json` 或 `CLAUDE.md`）的项目里，要等你回答一个问题才会启动：Eitri 会先扣住这次启动，问是否信任那份配置。`y` 加载它，`n` 不加载它而启动会话，`Esc` 把这次启动推迟并把你的文字还给你。在你回答之前，面板会写 “Waiting for your answer to the trust question — nothing has started”。问题显示什么、每个回答意味着什么，见[信任一个项目](permissions.zh-CN.md#trusting-a-project)。每条消息还会带上编辑器里打开的文件名，或者你在那里用 Visual 模式选中的行。选区才是让 agent 指向某一段代码的方式，所以说“修好这个函数”之前，先在编辑器里把那个函数选上；光标位置本身不会被发送。选中了行之后，在编辑器里按 `<leader>ae` 就替你做了第一步：按键交给面板，输入框打开，里面已有 `Edit the selection: `，你接着输入的就是一条普通消息（细节见[按键](keys.zh-CN.md#editing-the-selection-from-neovim)）。

Claude 回答时，回复会作为渲染好的 markdown 边流式输出边显示。agent 运行的每个工具是单独一行安静的文字，前面的标记说明它已完成（`✓`）、失败（`✗`）还是仍在运行（`◐`）：一条命令、一个读过的文件、一次搜索。每一次文件编辑是一张小卡片，写着文件名和增删的行数，diff 就在卡片里。

- `Esc` 离开输入框，回到浏览对话。在浏览模式里，`j` 和 `k` 在行与行之间移动，`Enter` 显示或隐藏一个工具的结果，`Ctrl+o` 显示详细视图（每个结果，摘录更长），`y` 复制这一行（到剪贴板和主选区，所以鼠标中键也能粘贴），`?` 列出所有按键。
- `Ctrl+c` 中断正在运行的回合。
- 在回合运行期间输入消息后按 `Enter`，它会插进这个回合而不打断它，和 Claude Code、Codex 一样：agent 在下一步读到它；在那之前，它以 `↪ <消息>` 显示在输入框上方，读到之后出现在对话里 agent 读到它的地方。agent 不能在回合中途接收消息的标签页会先留着它，显示为 `⧗ <消息>`，等回合结束再发；输入框为空时会说明 `Enter` 在这个标签页里是哪一种。Claude 和 Codex 标签页能在回合中途接收消息，ACP 标签页会先留着。agent 在回合结束前没读到的消息（比如被中断了）会重新留着，并有一行说明原因。`Ctrl+Enter` 立刻发送：先中断正在运行的回合，再发送留着的消息和这条消息。
- 在输入框里按 `Ctrl+g`，会在 Neovim 里编辑你的草稿；`:wq` 把它带回来。

在一个编辑过文件的回合之后，在浏览模式里按 `c` 可以看这个回合期间磁盘上改了什么；见[回合审阅](turn-review.zh-CN.md)。

## 排布窗口

第一个回合之后，试着排一排模块。这里的 `<prefix>` 是先按 `Ctrl+b`，松开，再按后面的键或组合键，和 tmux 一样。这些都是默认键；如果 Eitri 拿到了你的 tmux 绑定，见[你的 tmux 键位](keys.zh-CN.md#your-tmux-keys)。

1. 按键在 agent 面板里时，按 `<prefix> "`，再按 `t`：底部终端会在它下面打开，并拿到按键。`<prefix> %` 则向右分割；两种分割都会等你选一个模块，把它打开或移到那里。
2. 按 `<prefix> o` 把按键交给屏幕上的下一个模块，一直按到按键回到终端。`<prefix> Left`、`Right`、`Up` 和 `Down` 则去那一侧的模块。
3. 在终端里按 `<prefix> Alt+Enter`，在它右边打开第二个终端，有自己的 shell，从项目根目录开始。
4. 第二个终端拿着按键时，按 `<prefix> Alt+Shift+Space` 让它浮起来，再按 `<prefix> Alt+_` 把它收进便笺板。`<prefix> Alt+-` 把它叫回来；再按一遍这个序列，又把它收起来。
5. 按 `<prefix> s`，再按 `c`，复制当前排布，建一个第二工作区并切过去。`<prefix> L` 回到之前的工作区。模块是同一批，每个工作区保留自己的排布。
6. 移到基础 agent 面板。按 `<prefix> c` 添加第二个聊天标签页，再按 `<prefix> !` 把这个标签页拆进面板旁边独立的视图。基础视图必须至少留下一个标签页。在新视图里按 `<prefix> !`，就把标签页并回去。
7. 要为还没有保存过布局的项目设置起始布局，在 `$XDG_CONFIG_HOME/eitri/init.lua`（默认 `~/.config/eitri/init.lua`）里写 `eitri.layout.default(...)`。窗口运行起来之后，`eitri.layout.export()` 会返回能粘贴到那里的布局调用，包括工作区和浮动窗口；[配置](configuration.zh-CN.md#layout-from-lua)里有用 Lua 命令把这段文字存下来的例子。导出会略去编号聊天视图：它们要有保存的标签页才能在启动后留下来，所以用[打开规则](configuration.zh-CN.md#where-a-new-module-opens)安排新视图的位置。

标签式、堆叠式容器、调整大小和其余布局按键，见[按键](keys.zh-CN.md#前缀)。

<a id="when-eitri-asks"></a>
## Eitri 什么时候会问你

有些工具调用要等你。Eitri 会在对话里、紧挨着它所拦住的那次调用画出一张权限卡片，上面有 **Approve**（批准）和 **Deny**（拒绝）两个按钮，对许多命令还有第三个按钮 “Always allow ... in this project”，它会保存一条规则，之后这类命令不出卡片就被批准。规则不回答 Claude Code 自己提出的问题，所以 Claude Code 仍然要问的命令，即使有规则也还是会出卡片；见 [Claude Code 自己提问时](permissions.zh-CN.md#when-claude-code-itself-asks)。

在浏览模式里：

- `a` 批准光标下的卡片，`d` 拒绝它。`]p` 和 `[p` 跳到下一张和上一张等待中的卡片，不回答任何东西。
- `D` 拒绝，并先让你输入一个理由；然后 `Enter` 把拒绝连同理由一起发出。
- 如果你更愿意走到某个按钮上，`h`、`l` 和 `Enter` 可以操作按钮。
- 在输入框里，`Ctrl+y` 批准这个标签页里等得最久的那张卡片，agent 的[编辑器工具](editor-tools.zh-CN.md#cards)的卡片除外，那种卡片用 `a` 或 `d` 回答。输入框里没有用来拒绝的按键。

只有单独按下一个键时，它才会回答卡片：如果在这之前或之后大约四分之一秒内还按过别的键，`a`、`d` 和 `D` 什么都不做，而且它们都不接受数字前缀。这样你正在输入的句子的头几个字母，就不会误回答一张卡片。

**Manual、auto 和 bypass。** 每个标签页处于三种模式之一，`Shift+Tab`（或 `<leader>m`）按 Claude Code 的顺序轮换它们：manual、auto、bypass，然后再回到 manual。

- **Manual** 最严格。每一次调用都由 Eitri 自己的规则来回答：在项目内部读取和只读命令会运行，你保存的规则仍然适用，其余的都是卡片，每一次文件编辑和写入也在内。没有任何东西交给 Claude Code 自己的 auto 模式。在你的 `init.lua` 里把 `agent.default_mode` 设为 `"manual"`，每个标签页就都以它开始。

- **Auto** 是默认模式。Eitri 仍然看得到每一次调用。你保存的规则允许的调用会被立即批准。其余的交给 Claude Code 自己的 auto 模式来决定，它不用卡片；它拒绝一次调用时，该工具那一行会写 `blocked by auto: <原因>`，多次被拒绝之后，Claude Code 会自己用一张卡片来问你。如果你的 Claude Code 运行不了它的 auto 模式，就由 Eitri 自己的规则来决定哪些调用需要卡片。
- **Bypass** 让 Eitri 自己批准每一次工具调用，不出卡片：它自己的检查和 Claude Code 自己提出的问题都一样，你的 `permissions.ask` 规则强制要问的也在内。因为 bypass 意味着很大的信任，`Shift+Tab` 进入它时会先问一句（一个 `y/n` 问题，措辞会说明还有什么会变，例如它会批准多少张等待中的卡片），而且只有单独按下、并且在问题出现片刻之后按的 `y` 才算数。只能从 auto 进入 bypass；离开它不需要确认，并且会落到 manual。

每种模式具体放行什么、保存的规则，以及项目自己的 Claude Code 配置如何被信任，都在[权限](permissions.zh-CN.md)里。

<a id="tabs-and-sessions"></a>
## 标签页和会话

每个对话是一个标签页。有两个或更多标签页时才会出现标签栏。

| 按键 | 作用 |
|---|---|
| `<prefix> c` | 新建标签页（一个空的，带上面的菜单） |
| `<prefix> n` / `<prefix> p` | 下一个 / 上一个标签页 |
| `<prefix> 1` ... `9` | 按编号切到那个标签页 |
| `<prefix> l` | 切回你之前所在的标签页 |
| `<prefix> ,` | 重命名标签页 |
| `<prefix> &` | 关闭标签页（会先问，并且在有回合正在运行、会被中断时说明这一点） |
| `<prefix> w` | 已打开的标签页和保存过的会话的列表，用来切换或继续 |
| `<prefix> i` | 这个标签页的详情：名字、模式、模型、会话 id、它加载了哪些设置，以及它的项目是否被信任 |

在面板里，`H` 和 `L`（或 `gT` 和 `gt`）同样切到上一个和下一个标签页。每个标签页运行一个 agent，默认是 Claude；`:tabnew codex` 会打开一个 Codex 的标签页，你注册的 agent 也可以有标签页。见 [Codex 与其他 agent](agents.zh-CN.md)。标签页并排运行：后台标签页会继续流式输出，它在标签栏里的标记会显示它需要什么（`⚑` 表示有等待中的卡片，`•` 表示在你看别处时一个回合结束了，`✕` 表示一个会话糟糕地结束了）。

**继续一个对话。** 关闭标签页不会删除它的对话。在空标签页上按 `r` 继续最近一次保存的会话，`<prefix> w`（或空标签页上的 `w`）按从新到旧列出它们，每个都带着名字或标题；另一个窗口当前占用的会话会显示出来，但不能选。一个项目保留最新的 200 个会话：存满之后，列表会写 `only the newest 200 are kept`，下一个新会话会把最旧的那个从列表里去掉（它的对话仍在 Claude Code 自己的 `claude --resume` 里）。

**恢复上次的标签页。** Eitri 记得一个项目上一个窗口里开着哪些标签页：它们的顺序、名字和模式，以及当时在屏幕上的是哪一个。默认情况下，空标签页会把它们作为 `Restore last session`（在面板里按 `s`）提供出来。按键一开始在编辑器里，那里的 `s` 是 nvim 的；从那里按 `<prefix> w` 打开会话列表，光标就在 `Restore last session` 这一行上，按 `Enter` 即可。在你的 `init.lua` 里把 `agent.restore` 设为 `"auto"`，启动时不用按任何键就把它们带回来，`"off"` 则既不提供也不记录；见[配置](configuration.zh-CN.md)。如果某个标签页保存的对话已经没了，或者被另一个窗口占着，它会被跳过，并在一条简短的消息里点名。曾经处于 bypass 的标签页不会不经询问就以 bypass 回来：`s` 会先问（回答 `n` 就让它以 Auto 回来），自动恢复则让它以 Auto 回来并告诉你。曾经处于 manual 的标签页会以 manual 回来。唯一的例外是你事先给出的回答：在你的 `init.lua` 里把 `agent.default_mode` 设为 `"bypass"`，保存下来的 bypass 标签页就不用提问直接以 bypass 回来。

标签页里没发送的文字会随它一起保存，按 `s` 时回到输入框，最多保存 256 KiB。回不来的文字（在还没有会话的标签页里，或者太长存不下）会计入窗口的关闭问题：`close window? 1 unsent draft (y/n)`。如果不按 `s` 而是开始新的内容，保存的东西会被替换，没发送的文字也一起没了。

**在真正的终端里继续。** 按 `<leader>t`（或者打开 `<prefix> i` 选 `Continue in a terminal...`）。Eitri 会先说明它会先关闭面板里的这个对话，然后在你确认后，显示在你自己的终端里继续它的 `claude --resume ...` 命令；按 `y` 复制它。Eitri 不会替你启动那个终端，也不会对会话加锁，所以不要在两个地方同时开着它。那个终端运行的是一个普通的 `claude`，用你自己的 Claude Code 设置，没有 Eitri 的权限关卡，所以它的工具调用不会以卡片出现。还没有过第一个回合的会话没有什么可继续的，还有回合在运行的会话也一样。

<a id="when-something-ends"></a>
## 有东西结束时

一个没有正常结束的回合，看起来不再和正常结束的一样。它停下的地方会出现一行暗色的斜体文字，面板底部的状态行会用几个字说明，直到你发送下一个提示：

| 发生了什么 | 这一行写 | 状态行写 |
|---|---|---|
| 你按了 `Ctrl+c` | `interrupted` | `interrupted` |
| 一次失败 | `the turn ended with an error (...)`，然后是 Claude Code 自己的话 | `turn failed` |
| 速率或用量限制（HTTP 429） | `rate or usage limit reached (HTTP 429)`，然后是 Claude Code 的话 | `rate limited` |
| 回合数上限或花费上限 | `stopped at the turn limit` 或 `stopped at the spending limit` | `turn limit` 或 `spending limit` |
| 对话装不下了 | `stopped: the context is full` | `context full` |
| 你的某个 hook 把它停住了 | `stopped by a hook` | `stopped by a hook` |
| 会话在它进行时结束了 | `the turn did not finish: the session ended` | `turn did not finish` |

一行里冒号后面的文字是 Claude Code 自己的消息，原样显示，所以限制的重置时间只有在 Claude Code 写了的时候才会出现。这一行会留在对话里，面板重新加载（`<prefix> r`）和切换标签页后依然在，也可以像其他行一样用 `y` 复制。

会话本身没了的时候，对话里会说出来。红色的 `This session was lost` 横幅表示连接断了，上面显示的内容可能不完整；安静的 `This session has ended (<reason>)` 表示它关闭了。无论哪种，在浏览模式里按 `r`，就会在这个标签页里开始一个新会话。回合运行时留着的消息，仍会在这个回合结束时发出，即使它是在碰到限制时结束的。

如果 nvim 没人要求就没了（被杀掉或崩溃），窗口会留着并说明：`nvim stopped unexpectedly — <prefix> e starts it again and says what can be recovered`。在最后一个缓冲区上 `:q` 仍然关闭窗口，和普通 Neovim 一样。启动时如果发现这个项目的文件有未保存的改动留在 swap 文件里，会说一次：`unsaved changes kept for long.txt — :e the file, then r recovers them or d deletes them`。除非你这样做，否则什么都不会恢复；只要 swap 文件还在，每次启动都会再说这一行。

如果 Eitri 要保存的东西写不下去了，窗口会按种类各说一次，直到恢复正常，并说明什么没有保存：`could not save the open tabs`、`could not record this session` 或 `could not store the history`，通常是 `the disk is full`。回合照常运行。有了空间之后，保存会自己补上。

Claude Code 自己更新之后面板出了问题，背后的 sidecar 可能也需要更新。`eitri sidecar check` 问有没有更新，`eitri sidecar update` 装上它，`eitri sidecar use bundled` 退回去。什么都不会自己检查，已经打开的标签页继续用它启动时的 sidecar。Eitri 0.3.0 没有 `eitri sidecar` 这个命令（它会回答 `eitri: no such directory`），先装它之后的版本。细节见[更新 sidecar](../../INSTALL.zh-CN.md#updating-the-sidecar)。

## Eitri 自己崩溃时

崩溃时会写一份记录到 `~/.local/state/eitri/log/crash-<时间>-<pid>.log`（设了 `$XDG_STATE_HOME` 就在它下面），只有你能读。下次启动时说一次，一行，带文件路径。要报告的话，把这个文件附到一个 [bug report](https://github.com/HunterGrey-cyber/eitri/issues/new?template=bug_report.yml) 里。你不附，什么都不会离开你的机器：Eitri 不往任何地方发东西。

记录是纯文本：版本、构建、内核、线程、代码停在哪里、回溯，还有窗口最后打印的几行。里面没有提示词、对话或工具输出，崩溃本身的消息如果可能引用到这些内容，也会略去。路径是有的，你的主目录显示成 `~`，所以附之前先读一遍。

从桌面启动的窗口，把打印的内容留在同一目录的 `main-<16 位十六进制>.log` 里；到 2 MiB 时这个文件移到 `.log.1`，新一次启动时上一次的也一样。从终端启动的窗口打印到那个终端，不留日志。磁盘满时这些行被丢掉，窗口继续运行。`EITRI_WINDOW_LOG=off` 关掉日志和记录。

崩在原生库里（GTK、WebKit、显卡驱动）不会写记录。窗口到那一刻为止打印的内容在日志里。

已知的毛边列在[已知问题](../known-issues.zh-CN.md)里。
