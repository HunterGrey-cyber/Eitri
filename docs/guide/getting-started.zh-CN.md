[English](getting-started.md) | 简体中文
<!-- translated-from: getting-started.md sha256=9c91d8152ee4b55db55fe00dfa9d2585b5ab81eb073421a0f2208515359e8647 -->

# 快速上手

这一页带你从命令走到第一个完成的回合：打开一个项目、发送提示、回应 Eitri 弹出的卡片、同时使用多个会话，以及某个回合没有顺利结束时面板会说什么。它假定 Eitri 已经[安装好](../../INSTALL.zh-CN.md)，并且 `claude` 命令已经登录到你的账号。

文中的按键按默认设置来写：前缀是 `Ctrl+b`，“leader”是 `Space`，除非你的 Neovim 配置设了别的、面板能用的单个键的 `mapleader`（更长的或被保留的会退回到 `Space`）。完整的按键列表在[按键](keys.zh-CN.md)里，在面板里按 `?` 也能看到。

<a id="open-a-project"></a>
## 打开一个项目

```sh
eitri [DIR]
```

`DIR` 是项目目录。不写的话，Eitri 在设置了 `$EITRI_PROJECT_DIR` 时用它，否则用你运行命令时所在的目录。如果给的路径不是目录，或者给了 Eitri 不认识的选项，启动会直接带着一条消息停下，而不是去打开别的目录；要打开名字以 `-` 开头的目录，在它前面加 `--`（`eitri -- -notes`）。

每次启动都是一个独立的窗口，有自己的 Neovim、自己的会话和自己的项目根目录，所以你可以同时打开好几个项目。窗口标题写着项目名（`myproject — Eitri`；有两个工作区时再加上当前显示的那个：`myproject — Eitri · docs`），窗口切换器和合成器的窗口规则读的就是它。

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

<a id="your-first-turn"></a>
## 你的第一个回合

按 `Ctrl+l` 把焦点移到面板，再按 `i`（或 `Ctrl+j`）输入你的提示。如果你重新绑定了模块焦点键，就用那个键代替 `Ctrl+l`。`Enter` 发送；`Shift+Enter` 换行，`Alt+Enter` 以及反斜杠加 `Enter` 也一样。会话在你发出第一条消息时才启动，不会更早；而且在一个自带 Claude Code 配置（`.claude/`、`.mcp.json` 或 `CLAUDE.md`）的项目里，要等你回答一个问题才会启动：Eitri 会先扣住这次启动，问是否信任那份配置。`y` 加载它，`n` 不加载它而启动会话，`Esc` 把这次启动推迟并把你的文字还给你。在你回答之前，面板会写 “Waiting for your answer to the trust question — nothing has started”。问题显示什么、每个回答意味着什么，见[信任一个项目](permissions.zh-CN.md#trusting-a-project)。每条消息还会带上编辑器里打开的文件名，或者你在那里用 Visual 模式选中的行。选区才是让 agent 指向某一段代码的方式，所以说“修好这个函数”之前，先在编辑器里把那个函数选上；光标位置本身不会被发送。

Claude 回答时，回复会作为渲染好的 markdown 边流式输出边显示。agent 运行的每个工具是单独一行安静的文字，前面的标记说明它已完成（`✓`）、失败（`✗`）还是仍在运行（`◐`）：一条命令、一个读过的文件、一次搜索。每一次文件编辑是一张小卡片，写着文件名和增删的行数，diff 就在卡片里。

- `Esc` 离开输入框，回到浏览对话。在浏览模式里，`j` 和 `k` 在行与行之间移动，`Enter` 显示或隐藏一个工具的结果，`Ctrl+o` 显示详细视图（每个结果，摘录更长），`y` 复制这一行（到剪贴板和主选区，所以鼠标中键也能粘贴），`?` 列出所有按键。
- `Ctrl+c` 中断正在运行的回合。
- 在回合运行期间输入消息后按 `Enter`，它会插进这个回合而不打断它，和 Claude Code、Codex 一样：agent 在下一步读到它；在那之前，它以 `↪ <消息>` 显示在输入框上方，读到之后出现在对话里 agent 读到它的地方。agent 不能在回合中途接收消息的标签页会先留着它，显示为 `⧗ <消息>`，等回合结束再发；输入框为空时会说明 `Enter` 在这个标签页里是哪一种。目前 Codex 标签页能在回合中途接收消息，Claude 和 ACP 标签页会先留着。agent 在回合结束前没读到的消息（比如被中断了）会重新留着，并有一行说明原因。`Ctrl+Enter` 立刻发送：先中断正在运行的回合，再发送留着的消息和这条消息。
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
- 在输入框里，`Ctrl+y` 批准这个标签页里等得最久的那张卡片。输入框里没有用来拒绝的按键。

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

**继续一个对话。** 关闭标签页不会删除它的对话。在空标签页上按 `r` 继续最近一次保存的会话，`<prefix> w`（或空标签页上的 `w`）列出全部，每个都带着名字或标题；另一个窗口当前占用的会话会显示出来，但不能选。

**恢复上次的标签页。** Eitri 记得一个项目上一个窗口里开着哪些标签页：它们的顺序、名字和模式，以及当时在屏幕上的是哪一个。默认情况下，空标签页会把它们作为 `Restore last session`（`s`）提供出来。在你的 `init.lua` 里把 `agent.restore` 设为 `"auto"`，启动时不用按任何键就把它们带回来，`"off"` 则既不提供也不记录；见[配置](configuration.zh-CN.md)。如果某个标签页保存的对话已经没了，或者被另一个窗口占着，它会被跳过，并在一条简短的消息里点名。曾经处于 bypass 的标签页不会不经询问就以 bypass 回来：`s` 会先问（回答 `n` 就让它以 Auto 回来），自动恢复则让它以 Auto 回来并告诉你。曾经处于 manual 的标签页会以 manual 回来。唯一的例外是你事先给出的回答：在你的 `init.lua` 里把 `agent.default_mode` 设为 `"bypass"`，保存下来的 bypass 标签页就不用提问直接以 bypass 回来。

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

已知的毛边列在[已知问题](../known-issues.zh-CN.md)里。
