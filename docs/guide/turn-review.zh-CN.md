[English](turn-review.md) | 简体中文
<!-- translated-from: turn-review.md sha256=c1f234eb66c6e7449418d37baddfc885b06489fbdff5e0d20df4ce84900e25bb -->

# 审阅一个回合

agent 在你的项目上工作之后，回合审阅回答一个问题：在那个回合期间，磁盘上改了什么？它列出文件，显示每个文件的各个 hunk，让你把一个 hunk 或整个文件放回回合之前的样子，也让你把对这些改动的评论，作为你的下一条消息发回给 agent。

它依据的是 Eitri 对项目文件拍下的快照，而不是你的 git 历史，所以在不是 git 仓库的目录里，表现也完全一样。它从不往你项目的 `.git` 里写任何东西。

下面的按键针对默认设置。在面板里，“浏览模式”就是你从消息输入框按 `Esc` 进入的那种模式；见[快速上手](getting-started.zh-CN.md)。

## 它显示什么

在有会话的标签页里，于浏览模式按 `c`，打开这个标签页最近一个回合的审阅。它画在对话之上，状态条仍然可见。标题行点出这个回合（“turn 3 of 5”）、它什么时候开始和结束，并写着 `changed on disk during this turn`。

每个改动的文件是一行，带着它增加和删除的行数，前面有一个标记：

| 标记 | 含义 |
|---|---|
| `✓` | 这个标签页里 agent 自己的某次成功的 `Write`、`Edit`、`MultiEdit` 或 `NotebookEdit` 调用点名了这个文件，并且它确实变了 |
| `·` | 那些调用中的某一次点名了这个文件，但这个文件在磁盘上没有变 |
| `?` | 这个文件变了，而那些调用都没有点名它 |

这里的措辞刻意说的是磁盘，而不是谁做了什么。agent 运行的 shell 命令所做的改动，或者你在回合运行期间自己在编辑器里做的改动，没有哪次编辑调用点名它，所以显示为 `?`；这些文件被收在一个折叠的组 `changed outside this tab's edits` 里，按 `Enter` 打开它。只有这些标记才说明归属，而且 `?` 的意思是“没被点名”，不是“不是 agent 做的”。

有些文件没有 hunk 可以展开，它们的那一行会说明原因：`binary`、`too large to snapshot`（超过 8 MiB 的文件不进快照）或 `nested repository, not reviewed`（本身就是一个 git 仓库的目录）。超过 2000 行的补丁不会被截短：这一行会说它太大，给出计数，并提供 `o` 在编辑器里打开这个文件。

标题行可以带上关于比较有多精确的说明：

- `baseline late: changes made before <time> may be missing`：这个回合的第一个快照，是在 agent 已经开始工作之后才完成的。
- `this turn overlapped another tab's turn`：同一个项目的两个标签页同时在运行，所以一处改动可能属于其中任何一个。
- `may include the next turn's first changes`：下一个回合在这个回合的结束快照完成之前就开始了。
- `this turn is still running: compared with the files on disk now`，对于结束快照还在拍的回合也是同样的说明。
- `this turn did not finish while Eitri was running`，或者一条说明这个回合没有结束快照及原因的说明：文件拿来和现在的磁盘比较。
- `the baseline snapshot is still being taken`：在它完成之前，列表是空的。在无法进行比较时，空列表绝不会被显示成“没有改动”。

一个什么都没改的回合会写 `no files changed on disk`。

### 在其中移动

| 按键 | 作用 |
|---|---|
| `j` / `k` | 下一个 / 上一个文件或 hunk（在已展开的 hunk 框里，它们先滚动这个框） |
| `gg` / `G` | 第一个 / 最后一个 |
| `Ctrl+d` / `Ctrl+u` | 向下 / 向上五站 |
| `Enter` | 展开或收起一个文件的 hunk；在 hunk 上，收起它的文件；在折叠的组上，展开它 |
| `[` / `]` | 上一个 / 下一个回合 |
| `S` | 在这个回合和整个会话之间切换（自会话的第一个快照以来改动的一切） |
| `y` | 复制光标所在行的 `path:line` |
| `o` | 在你的编辑器里打开这个文件，定位到 hunk 的第一行（见[在你的编辑器里](#in-your-editor)） |
| `q`、`Esc` 或 `c` | 关闭 |

审阅打开期间，它接管每一个键：比如回答权限卡片的那些键，在你关掉它之前什么都不做。补丁只用 diff 的颜色绘制，带有两列行号。

默认情况下，没有任何东西告诉你有一个审阅在等着。如果你希望在每个改动了文件的回合之后得到一个提示，状态条可以写 `2 files changed · c to review`；见[关掉它](#turning-it-off)。

## 还原和撤销

在一个 hunk 上按 `x`，把那个 hunk 的行放回回合之前的样子（在会话视图里，是放回会话第一个回合之前的样子）。在文件行上按 `x` 还原整个文件，并且先问一句：`revert the whole file <path> to before turn N? y/n`。回合创建的文件会被删除，回合删除的文件会带着它原来的权限位回来，链接会作为链接回来。二进制文件只能整个还原。

被还原的 hunk 或文件会标上 `reverted`。`u` 撤销在这里做的最后一次还原，把文件逐字节放回去，权限位也包括在内；那个标记随后写 `reverted, undone`。除非下面每一条都成立，否则这两个键都不会写入任何东西，有某一条不成立时，页脚会说是哪一条：

- **在任何 Eitri 窗口里都没有 agent 回合在运行。** 否则：`an agent turn is running`。
- **没有别的 Eitri 窗口打开着这个项目。** 否则：`another Eitri window has this project open; revert from one window at a time`。
- **你的编辑器里没有这个文件的未保存改动。** 否则：`<path> has unsaved changes in the editor; write or discard them first`。没有附着的编辑器时（例如一个已脱离的 [companion](companion.zh-CN.md) 面板），Eitri 无法判断，所以拒绝：`no editor is connected, so Eitri cannot tell whether <path> has unsaved changes`。
- **这个文件仍然是回合留下的样子。** 如果你在那之后又编辑过它，还原会被拒绝，并给出 `changed since the turn ended; open it in the editor (o)`，所以你自己的工作绝不会被一次过期的还原覆盖。
- **这个路径在项目之内。** 是符号链接的父目录会被拒绝，即使它通向的地方无害。
- **这个回合已经结束，并且它的快照还在。** 没有结束快照的回合，没有固定的东西可以还原回去；老到已经被清理的回合，会说它的快照被删除了。

如果 Eitri 在改写一个无法原子替换的文件（比如有第二个硬链接的文件）的中途被杀掉，它本来要覆盖的字节会被保留下来。下一次你打开这个项目时，即使提示已被关掉，状态条也会写 `an interrupted revert left <path>`。窗口里一旦有会话打开，就打开审阅（`c`）；这一行在最上面，在它上面按 `Enter` 会问，是把文件恢复成被中断的那次还原之前的字节（`y`），还是忘掉这个条目（`n`）。

## 把评论发回去

在一个 hunk 上按 `i`，会为这个 hunk 新增的那些行打开一个单行评论。`Enter` 保存它，`Esc` 取消。评论显示在它的 hunk 下面，带着行范围；在评论上按 `x` 删除它。在文件行上，`i` 什么都不做。

你的评论和你做的还原，构成一份草稿，在 Eitri 运行期间按标签页保存；页脚会写 `draft: 2 comments, 1 revert · s sends them to the agent`。重新加载面板不会丢掉它。

`s` 显示到底会发送什么（根据那一刻的磁盘内容生成），并询问。这条消息长这样：

```
Review of your last turn: 2 comments, 1 revert.

I reverted these changes; the files no longer contain them:
- src/main.rs lines 12-20 (back to how they were before your turn)

Comments:
1. src/lib.rs:40-44
   > the lines you commented on, quoted
   Why does this not return an error?
```

一次你已经撤销的还原、一次只存在于未保存的编辑器缓冲区里的还原，或者行已经发生变化的还原，不会被报告为已经在文件里：预览会把它连同原因列在消息下面，并且把它略去。`y` 把这条消息作为你的一条消息发出去（有回合在运行时，它会排队到那个回合结束，问题里会这么说）；`n` 或 `Esc` 取消并保留草稿。发送之后草稿被清空。没有这个预览，什么都不会发出去。

<a id="in-your-editor"></a>
## 在你的编辑器里

在一个文件或 hunk 上按 `o`，在你的编辑器里打开这个文件，定位到 hunk 的第一行，并把这个回合的 hunk 画在缓冲区上：新增的行高亮，被删除的行显示在它们上方或下方，作为不在缓冲区里的文字。在完整窗口里，编辑器拿到按键；在 companion 窗口里，编辑器的窗口被带到前面。自回合以来已经变化的文件不会被画上去，重新加载缓冲区（`:e!` 或来自外部的改动）会清除这层叠加，并给出一条提示。

在那个缓冲区里：

| 按键 | 作用 |
|---|---|
| `]h` / `[h` | 下一个 / 上一个 hunk（带数字则移动好几个） |
| `<localleader>r` | 只在缓冲区里还原光标下的 hunk：它不会被保存，`u` 像撤销任何编辑一样撤销它 |
| `<localleader>q` | 关闭这层叠加 |

`<localleader>` 是你 Neovim 的 `maplocalleader`（默认是 `\`）。这些键只存在于显示着叠加层的缓冲区里：另一个插件的 `]h`（比如 gitsigns）在那里被放到一边，叠加层关闭时再回来。这样做的还原会被加进草稿；在缓冲区里撤销它，就把它再取出来。要不要保存文件，由你决定。

<a id="turning-it-off"></a>
## 关掉它

你的 `init.lua` 里的两个设置（见[配置](configuration.zh-CN.md)）：

```lua
eitri.config.set("review.enabled", false) -- take no snapshots at all; `c` says turn review is off
eitri.config.set("review.hint", true)     -- say "N files changed · c to review" in the status band after a turn
```

`review.enabled` 默认是 `true`，`review.hint` 默认是 `false`。`true` 或 `false` 之外的任何值都是启动错误，并点出这个键。审阅关闭时，不拍任何快照，也打不开任何审阅。

## 快照存在哪里

在 `$XDG_STATE_HOME/eitri/review/<项目键>/` 下（那个变量没设置时是 `~/.local/state/eitri/review/`），位于 Eitri 自己的一个私有 git 仓库里，每个项目一个目录。Eitri 为一个项目保留最新的 100 个回合，最长 30 天，每个快照最多取 20,000 个文件和 10 秒。你的 `.gitignore`、`.git/info/exclude`（当项目是某个仓库的顶层时）或你全局 git 排除列表里列出的文件，不进快照；当项目是一个更大仓库的子目录时，项目自己目录之上的忽略文件不会被读取。删掉这个目录会丢掉审阅历史：下一个回合会重新开始一份。状态栏还在提示你恢复一次被打断的回退时（见上文），不要删它：那次恢复要用的、保存下来的原始字节也在这个目录里。
