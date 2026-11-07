[English](editor-tools.md) | 简体中文
<!-- translated-from: editor-tools.md sha256=757f9fc2833558ff93ba25f01684c7537b61a2befceb576efde8e7c582d1aa94 -->

# 从 agent 标签页使用你的编辑器

任何标签页里的 agent，无论是 Claude Code、Codex 还是你注册的 agent，都可以给你看一个文件、读你的编辑器里现在有什么、向你的语言服务器询问代码，并通过它们提议改动。Eitri 把这些工具挂在名为 `eitri` 的服务上，每种标签页都一样。

了解你的编辑器里有什么：

- `open_file`：在你所在窗口旁边的一个窗口里，把一个文件打开到指定行，并高亮这几行。
- `read_buffer`：按你的编辑器里现在的样子读一个文件，包括未保存的改动。它只读已经打开的文件，不会自己去加载。
- `editor_state`：告诉它哪些文件开着、哪些有未保存的改动、你的光标在哪、你处于什么模式、选区是什么。项目之外的文件只计数，不说名字，你的光标在那样的文件里时也不告诉它位置。
- `diagnostics`：给它你的编辑器显示的错误和警告，最严重的排在前面。它可以只要这个回合开始以来新出现的那些。

询问你的语言服务器，也就是你的编辑器里已经在运行的那些：

- `usages`：说明一个符号在哪里定义、在哪里被引用、在哪里实现。
- `hover`：给出服务器关于一个符号的说明，是服务器自己的原文。
- `symbols`：列出一个文件的符号，或者按名字在项目的语言服务器里搜索。

提议改动，在 `apply_proposal` 之前什么都不写（见[提议](#proposals)）：

- `lsp_rename`：算出整个项目里的一次重命名。
- `format`：算出一个文件的格式化结果。
- `lsp_code_actions` 列出某一行的代码操作，`apply_code_action` 算出其中一个会改什么。
- `apply_proposal`：把一个提议写到磁盘上。

有一个工具在你打开之前一直是关的：[`nvim_exec`](#nvim-exec) 在你的编辑器里运行 Ex 命令或 Lua。

在 [companion 窗口](companion.zh-CN.md)里，面板附着之前 Neovim 已经加载的缓冲区，agent 什么也看不到，直到你用 `:e` 重新加载它。

## 它从不夺走你的光标

打开一个文件，不会改变你当前的窗口、光标、模式和文本。如果 agent 打开文件的时候你正在插入模式里打字，你仍在插入模式里，下一个按键落在原来该落的地方。文件出现在一个侧边窗口里；Eitri 之前为 agent 开的那个窗口会被复用，变的从来不是你自己的窗口。高亮在四秒后消失，或者你进入那个窗口时消失。标签页的那一行会写明发生了什么，例如“showed `b.py:42` in the editor”。

在 Claude Code 或 Codex 标签页里，折叠的编辑器工具那一行显示它回答的前几行；处于 BROWSE 时用 `Enter`、`za` 或 `zo` 展开它，就能读到完整的回答：缓冲区文本、诊断、符号位置、悬浮说明，或者提议的文件路径和改动片段。未保存的缓冲区和省略的内容都会标出。`Ctrl+o` 打开详细视图，显示原始回答；再按一次就返回。工具文本按原样显示，其中的 Markdown 或 HTML 也只是文本。其他 agent 保留自己的工具标题和正文。

<a id="when-the-editor-is-out-of-sight"></a>
## 编辑器不在视线里的时候

Eitri 从不为 agent 把编辑器带到前面，那样会打乱你正在做的事。如果编辑器被隐藏、被放大的窗格挡住，或者在另一个工作区，文件照样在那里打开，agent 会被告知你看不到它。这时那一行写的是“opened `b.py:42` in the editor, which is not on screen”。等你把编辑器调回来，高亮可能已经消失；侧边窗口还在。

从[窗口 2](two-windows.zh-CN.md) 发起时，文件在窗口 1 里打开，窗口 1 留在原处，那一行写的是“opened `b.py:42` in window 1's editor”。agent 会被告知你可能看不到。Eitri 无法知道一个窗口是否被别的窗口盖住，从窗口 2 也无法知道窗口 1 的编辑器是否被隐藏。

<a id="cards"></a>
## Eitri 什么时候先问你

| 这次调用要做什么 | Manual | Auto | Bypass |
|---|---|---|---|
| 读项目之内已经打开的文件 | 没有卡片 | 没有卡片 | 没有卡片 |
| 要加载一个没有打开的文件 | 卡片 | 信任问题答了“是”之后没有卡片，其他情况有卡片 | 没有卡片 |
| 读或询问项目之外的文件 | 卡片 | 卡片 | 没有卡片 |
| `apply_proposal` | 卡片 | 卡片 | 没有卡片 |
| `nvim_exec` | 卡片 | 卡片 | 卡片 |

没有打开的文件要先加载，才能显示或询问，而加载可能启动语言服务器，运行项目里的代码。所以只有在[信任问题](permissions.zh-CN.md#trusting-a-project)答了“是”之后，加载才不出卡片。没有东西可问的项目没有信任问题：没有人答过“否”，它的会话有全部工具；也没有人答过“是”，所以在这里加载会先问你。卡片上的路径、改动和代码都按纯文本显示。

在每一种标签页里，决定这些工具的都只有这张表。[保存的规则](permissions.zh-CN.md#saved-rules)、Claude Code 的 auto 模式和 Codex 标签页的审查者都不判断它们。

这些卡片和标签页里别的卡片有几点不同：

- **在卡片上回答。** 浏览时 `a` 批准，`d` 拒绝；`]p` 和 `[p` 可以跳到它，它的按钮照常可用。`Ctrl+y` 不回答这种卡片：在输入框里要先按 `Esc`。如果它后面还等着一张 provider 的卡片，`Ctrl+y` 批准的是那一张，状态条会点出是哪张。
- **`D` 不带理由**，卡片也从不提供“总是允许”。
- **进入 Bypass 不会回答它。** 切换之前出现的卡片继续在 Bypass 标签页里等着，`Shift+Tab` 问的那个问题也不把它算进去。切换之后发起的调用没有卡片，`nvim_exec` 除外。
- **卡片批准的是它写明的那个位置。** 如果路径是一个链接，而你回答时它已经指向别处，就什么都不读、什么都不显示，agent 会被告知。
- **卡片最多等十分钟**，之后这次调用以“the person did not answer”结束，卡片消失。ACP agent 有它自己的时限，可能更早放弃；它不声不响放弃的卡片，会留到你回答或者十分钟到时。这时你批准的话，它要求的事仍然会运行，只是答复没有人收。

<a id="proposals"></a>
## 提议

`lsp_rename`、`format` 和 `apply_code_action` 不改任何东西，磁盘和缓冲区都不动。它们回答的是一个提议：一个 id、会碰到的文件，以及每个文件的改动。只有 `apply_proposal` 才写入，在 Manual 和 Auto 标签页里它会先出一张卡片，上面是这些文件和改动。`a` 写入，`d` 保持原样；提议还留着，agent 会被告知。

在浏览模式里，对这张卡片按 `Ctrl+g`，会在 Neovim 的一个新标签页里把提议中的一个文件在磁盘上的样子和提议之后的样子并排显示，两边都只读，`q` 关闭。每按一次显示下一个文件，有多个文件时状态条会说是哪一个，例如 `2 of 5: src/b.rs`。很长的提议，卡片上会略去一些行并说明略去了多少；`Ctrl+g` 仍然能看到每个文件的全部。

语言服务器的一次编辑，满足下面这些才能成为提议：

- **其中的文件在你的编辑器里都没有未保存的改动**，服务器回答之后也都没有变过。Eitri 会把服务器依据的内容和缓冲区、磁盘对比，不一致就告诉 agent 重新询问。
- **最多碰 64 个文件，每个不超过 4 MiB，全都在项目之内。**
- **只编辑已有的文件。** 创建、重命名或删除文件的编辑会被整个拒绝。

语言服务器的命令从不运行。只运行命令的代码操作会被拒绝。既有编辑又有命令的，只提议编辑，并告诉 agent 命令没有运行。

`apply_proposal` 运行时会再看一次未保存的改动，因为卡片可能等了好几分钟；提议之后变过的文件不会被覆盖。写入的文件如果在某个窗口里，你的编辑器大约一秒内会重新加载它。一个标签页保留最新的八个提议，标签页移到另一个窗口时它们不会跟过去。

在信任问题上回答 `n` 启动的会话没有这些工具。

<a id="following"></a>
## 跟随 agent 的修改

跟随默认关闭。在 BROWSE 中按 `Space e`（改过 leader 时是 `<leader>e`），可以为当前标签页开关跟随；开启时状态栏显示 `following`。agent 编辑项目里的文件后，Eitri 会在 agent 的编辑器窗口中显示这个文件，定位到第一处改动；没有可用的改动行时显示文件开头。它不会移动你的光标、改变你的模式或替换未保存的文字。只有在信任问题上答了“是”之后，跟随才会加载尚未打开的文件；这时开启跟随就允许加载这些文件，这可能会启动语言服务器和文件类型插件。关闭跟随时，还在排队等着显示的也不再显示。

在 `init.lua` 中设置 `eitri.config.set("agent.editor.follow", true)`（或 `"on"`），即可为新标签页开启跟随；`false`（或 `"off"`）则保持关闭。标签页移到另一个窗口后，仍保留自己的选择。

<a id="nvim-exec"></a>
## 在你的编辑器里运行代码

有一个工具在你打开之前一直是关的。`nvim_exec` 让 agent 在你的编辑器里运行 Ex 命令或 Lua，要在 `init.lua` 里打开：

```lua
eitri.config.set("agent.editor.tool.nvim_exec", true)
```

打开之后，每次调用都是一张卡片，里面是完整的代码，所有权限模式都如此，bypass 也不例外。没有“always”：下一次调用还会再问。同意之前请读这张卡片：代码在你的编辑器里以你的用户的全部权限运行，没有任何沙箱。在信任问题上回答 `n` 启动的会话没有这个工具。

<a id="window-2"></a>
## 从窗口 2 发起

窗口 2 里的标签页能够到窗口 1 的编辑器，但只限项目之内、不经过链接的路径。其他路径都回答“outside the project, or through a link; move the tab to the editor's window to be asked”，没有卡片：只有编辑器旁边的标签页才能问你。

显示和询问的工具（`open_file`、`read_buffer`、`editor_state`、`diagnostics`、`usages`、`hover` 和 `symbols`）都是这样。提议或应用改动的工具，以及 `nvim_exec`，在窗口 2 里完全不能用：它们都回答“move the tab to the editor's window”。

<a id="off-switches"></a>
## 两个关闭开关

都写在你的 `init.lua` 里，在窗口启动时读取（已经在运行的会话沿用它开始时的工具）：

```lua
eitri.config.set("agent.editor.tools", "off")           -- 所有标签页都没有编辑器工具
eitri.config.set("agent.editor.tool.open_file", "off")  -- 只关一个工具，其余照旧
```

关掉的工具不会被声明，agent 看不到它。Eitri 不认识的工具名，或者既不是 on 也不是 off 的值，会让启动失败，并指出是哪个键。每个工具都是开着的，除非你关掉；[`nvim_exec`](#nvim-exec) 相反，你打开之前一直是关的。工具名在[设置参考](configuration.zh-CN.md)里。

<a id="when-a-codex-tab-has-none"></a>
## Codex 标签页什么时候没有这些工具

Eitri 不能确定这些工具仍然是自己的时候，Codex 标签页就不带它们，并说明一次：

- **“a server named eitri is in your Codex configuration”**：你的 `config.toml` 或项目的 `.codex/config.toml` 定义了 `[mcp_servers.eitri]`，不管它有没有被关掉。Codex 会把两者合并，所以请给它改个名字。
- **“Eitri could not read all of your Codex configuration”**：一个可能定义这个名字的文件没有读到，所以 Eitri 不知道。这个文件可能是你的 `config.toml`（它是链接、不是合法的 TOML、超过 4 MiB，或者 `CODEX_HOME` 是相对路径），也可能是项目的 `.codex/config.toml`（它或它的 `.codex` 是链接、Eitri 的解析器拒绝了它，或者项目文件太多走不完）。修好这个文件，或者把它换成普通文件。
- **“this Codex would ask about the editor tools”**：Eitri 对这个 `codex` 的检查发现它会在工具调用之前发问，这样 Codex 自己的问题会和 Eitri 的卡片并排出现。这项检查是在 Codex 0.163.0 上测得的；更新的 Codex 可能保留这些工具。
