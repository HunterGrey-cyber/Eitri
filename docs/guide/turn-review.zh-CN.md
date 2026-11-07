[English](turn-review.md) | 简体中文
<!-- translated-from: turn-review.md sha256=7279f4e7344cc9d80b4a08c12199cfd831a15b7b42f07e3108c05a5d6524fa73 -->

# 审阅一个回合

agent 在你的项目上工作之后，回合审阅回答一个问题：在那个回合期间，磁盘上改了什么？它列出文件，显示每个文件的各个 hunk，让你把一个 hunk 或整个文件放回回合之前的样子，也让你把对这些改动的评论，作为你的下一条消息发回给 agent。

它依据的是 Eitri 对项目文件拍下的快照，而不是你的 git 历史，所以在不是 git 仓库的目录里，表现也完全一样。它从不往你项目的 `.git` 里写任何东西。

使用[合成器窗口](compositor-windows.zh-CN.md)时，审阅在聊天窗口里进行，它打开的内容会到窗口 1 的编辑器里。

下面的按键针对默认设置。在面板里，“浏览模式”就是你从消息输入框按 `Esc` 进入的那种模式；见[快速上手](getting-started.zh-CN.md)。

## 它显示什么

在有会话的标签页里，于浏览模式按 `c`，打开这个标签页最近一个回合的审阅。它画在对话之上，状态条仍然可见。标题行点出这个回合（“turn 3 of 5”）、它什么时候开始和结束，并写着 `changed on disk during this turn`。

每个改动的文件是一行，带着它增加和删除的行数，前面有一个标记：

| 标记 | 含义 |
|---|---|
| `✓` | 这个标签页里 agent 自己的某次成功的 `Write`、`Edit`、`MultiEdit` 或 `NotebookEdit` 调用点名了这个文件，并且它确实变了 |
| `·` | 那些调用中的某一次点名了这个文件，但比较结果显示它没有变化：要么这个文件没有变，要么你的忽略规则让它不进快照，所以看不到对它的改动 |
| `?` | 这个文件变了，而那些调用都没有点名它 |

在 Codex 标签页里，Codex 应用了的文件改动算作这样的一次调用（子 agent 的也算）；在通过 ACP 注册的 agent 的标签页里，没有出错就完成了的编辑、删除或移动调用算；见 [Codex 与其他 agent](agents.zh-CN.md)。

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

<a id="before-you-approve"></a>
### 批准之前

审阅显示的是磁盘上的变化，所以一个还在等权限卡片的编辑不会出现在里面。当这一回合有改文件的调用还没有结果时，最后一行会数出它们，并加上
`Ctrl+g on its card shows the proposal`。

在浏览模式里，对一张还在等待的编辑卡片按 `Ctrl+g`，会在 Neovim 里打开一个新标签页：左边是磁盘上现在的文件，右边是这次编辑之后的文件，以 diff 模式对比，两边都只读；`q` 关闭这个标签页。
文件本身不会被写入，你自己的缓冲区也不会被动到。这适用于 Claude 的 `Edit`、`MultiEdit` 或 `Write`，Codex 新增一个文件的改动，以及 ACP 智能体对一个文件的编辑。Codex 对已有文件的改动、涉及多个文件的改动、删除或移动，会像 `Ctrl+g` 在其他行上那样显示卡片的文字。agent 的[提议](editor-tools.zh-CN.md#proposals)的卡片在多个文件时是例外：每按一次，为其中一个文件打开这样的对比。

如果卡片出现之后文件变了、这次编辑已经对不上它，状态栏会显示
`the proposal no longer applies to <path> as it is on disk`，而不是拿错的文字去做对比。不是普通文件的路径、超过 4 MiB 的文件或提议文本，以及不是 UTF-8 文本的文件也会这样被拒绝，并说明原因。只改换行符的改动也会显示出来：一侧是 CR-LF、另一侧是 LF 时，CR 显示为 `^M`；加上或去掉末尾的换行，会显示为最后多出一个空行。

### 在其中移动

| 按键 | 作用 |
|---|---|
| `j` / `k` | 下一个 / 上一个文件或 hunk（在已展开的 hunk 框里，它们先滚动这个框） |
| `gg` / `G` | 第一个 / 最后一个 |
| `Ctrl+d` / `Ctrl+u` | 向下 / 向上五站 |
| `Enter` | 展开或收起一个文件的 hunk；在 hunk 上，收起它的文件；在折叠的组上，展开它 |
| `[` / `]` | 上一个 / 下一个回合 |
| `S` | 在这个回合和整个会话之间切换（自会话的第一个快照以来改动的一切） |
| `y` | 复制光标所在行的 `path:line`；补丁还没打开的文件行，以及二进制或过大的文件，只复制路径 |
| `o` | 在你的编辑器里打开这个文件，定位到 hunk 的第一行（见[在你的编辑器里](#in-your-editor)） |
| `q`、`Esc` 或 `c` | 关闭 |

审阅打开期间，它接管每一个键：比如回答权限卡片的那些键，在你关掉它之前什么都不做。补丁只用 diff 的颜色绘制，带有两列行号。

默认情况下，没有任何东西告诉你有一个审阅在等着。如果你希望在每个改动了文件的回合之后得到一个提示，状态条可以写 `2 files changed · c to review`；回合进行期间，输入框上方还会有一行，统计它的编辑到目前为止改了什么（`2 files changed +14 −1`）。见[关掉它](#turning-it-off)。

## 还原和撤销

在一个 hunk 上按 `x`，把那个 hunk 的行放回回合之前的样子（在会话视图里，是放回会话第一个回合之前的样子）。在文件行上按 `x` 还原整个文件，并且先问一句：`revert the whole file <path> to before turn N? y/n`。回合创建的文件会被删除，回合删除的文件会带着它原来的权限位回来，链接会作为链接回来。二进制文件、回合创建或删除的文件，以及符号链接，只能整个还原：在它们的 hunk 上按 `x` 会被拒绝，并说明原因。

被还原的 hunk 或文件会标上 `reverted`。`u` 撤销在这里做的最后一次还原，把文件逐字节放回去，权限位也包括在内；那个标记随后写 `reverted, undone`。除非下面每一条都成立，否则这两个键都不会写入任何东西，有某一条不成立时，页脚会说是哪一条：

- **这个项目的任何 Eitri 窗口里都没有 agent 回合在运行。** 否则：`an agent turn is running`。别的项目的窗口里的回合不算。
- **没有别的 Eitri 窗口打开着这个项目。** 否则：`another Eitri window has this project open; revert from one window at a time`。
- **你的编辑器里没有这个文件的未保存改动。** 否则：`<path> has unsaved changes in the editor; write or discard them first`。没有附着的编辑器时（例如一个已脱离的 [companion](companion.zh-CN.md) 面板），Eitri 无法判断，所以拒绝：`no editor is connected, so Eitri cannot tell whether <path> has unsaved changes`。
- **要还原的那些行仍然是回合留下的样子。** 对整个文件，指的是它的每一个字节；对一个 hunk，只指那个 hunk 的行和它周围的上下文，所以你在文件别处做的编辑会保留。如果它们不同，还原会被拒绝，并给出 `changed since the turn ended; open it in the editor (o)`，所以你自己的工作绝不会被一次过期的还原覆盖。
- **这个路径在项目之内。** 是符号链接的父目录会被拒绝，即使它通向的地方无害。
- **这个回合已经结束，并且它的快照还在。** 没有结束快照的回合，没有固定的东西可以还原回去；老到已经被清理的回合，会说它的快照被删除了。

有第二个硬链接的文件从不被还原：通过其中一个链接写入会改动其他链接，而它们可能在项目之外，所以这次还原会被拒绝，并给出 `the file has N hard links; writing it would change the other links too, so it was not written`。

如果 Eitri 在改写一个无法原子替换的文件（比如属于别的用户或带扩展属性的文件）的中途被杀掉，它本来要覆盖的字节会被保留下来。下一次你打开这个项目时，即使提示已被关掉，状态条也会写 `an interrupted revert left <path>`。窗口里一旦有会话打开，就打开审阅（`c`）；这一行在最上面，在它上面按 `Enter` 会问，是把文件恢复成被中断的那次还原之前的字节（`y`），还是忘掉这个条目（`n`）。

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

在一个文件或 hunk 上按 `o`，在你的编辑器里打开这个文件，定位到 hunk 的第一行，并把这个回合的 hunk 画在缓冲区上：新增的行高亮，被删除的行显示在它们上方或下方，作为不在缓冲区里的文字。在完整窗口里，编辑器拿到按键；在 companion 窗口里，编辑器的窗口被带到前面。行已经和缓冲区对不上的 hunk 会被跳过，提示里会说有几个；仍然对得上的 hunk 会被画出来。重新加载缓冲区（`:e!` 或来自外部的改动）会清除这层叠加，并给出一条提示。

在那个缓冲区里：

| 按键 | 作用 |
|---|---|
| `]h` / `[h` | 下一个 / 上一个 hunk（带数字则移动好几个） |
| `<localleader>r` | 只在缓冲区里还原光标下的 hunk：它不会被保存，`u` 像撤销任何编辑一样撤销它 |
| `<localleader>q` | 关闭这层叠加 |

`<localleader>` 是你 Neovim 的 `maplocalleader`（默认是 `\`）。这些键只存在于显示着叠加层的缓冲区里：另一个插件的 `]h`（比如 gitsigns）在那里被放到一边，叠加层关闭时再回来。这样做的还原会被加进草稿；在缓冲区里撤销它，就把它再取出来。要不要保存文件，由你决定。

<a id="live-drawings"></a>
## 实时画出改动

实时画出改动默认是关的，要打开它：在你的 `init.lua` 里加上 `eitri.config.set("review.live", true)`，然后重新启动 Eitri（这个设置只在启动时读一次）。

打开 `review.live` 之后，agent 编辑的文件会在编辑落地时就被画在你的编辑器里，不用你按 `o`。它对显示在编辑器某个 Neovim 窗口里的文件起作用（完整窗口或 companion 窗口都行），对每一种 agent 标签页都一样，第二个窗口里的标签页也一样。画出来的和 `o` 画的是同一种：新增的行高亮，被删除的行显示在它们上方或下方，作为不在缓冲区里的文字。

画图从不做这些事：打开文件或窗口，移动你的光标或视图，抢焦点，改变模式，或者说一句话。只有缓冲区没有被修改过时才会画；你正在里面输入的缓冲区，或者带着等待保存的还原的缓冲区，原样不动，你用 `o` 打开的叠加层也一样。不在任何窗口里的文件什么也得不到（审阅里按 `o` 仍然可以按需画出），浮动窗口里的文件（比如选择器的预览）也一样。agent 用 shell 命令改的文件、被你的忽略规则挡在快照之外的文件、二进制文件、超过 8 MiB 的文件、补丁超过 2,000 行的改动，都不画。

回合结束时，它编辑过的每个文件都按这个回合的快照重新画一遍，用的是审阅面板和草稿所用的同一些 hunk；做不到的地方就把画出的东西取下来。画出的东西一直留到你关掉它：新回合不会移除它，除非那个回合编辑了同一个文件，这时它被新回合的改动替换。两个 agent 在重叠的时间里编辑同一个文件，这个文件就完全不画，因为每一方的 diff 里都会带着对方的编辑；审阅面板的 `c` 会列出这种重叠。

在画着东西的缓冲区里：

| 按键或命令 | 作用 |
|---|---|
| `]h` / `[h` | 下一个 / 上一个 hunk（带数字则移动好几个） |
| `<localleader>a` 或 `:EitriReviewKeep` | 全部保留：把画出的东西从这个缓冲区取下来，并把它的 hunk 记为已保留，同一个回合不会再画它们。它什么也不写；审阅面板不变 |
| `<localleader>d` 或 `:EitriReviewUndo` | 全部撤回：把这个缓冲区里画出的每个 hunk 都还原，作为一步，一个 `u` 就全部带回来。缓冲区被改动但不保存；`:w` 才会写入 |
| `<localleader>r` | 只在这个缓冲区里还原光标下的 hunk，和审阅叠加层里一样 |
| `<localleader>q` | 关掉这层画出的东西，什么也不记 |
| `:EitriReviewClear` | 把所有缓冲区里的实时画面都取下来 |

回合还在运行时，全部撤回和还原会说明这一点然后什么也不做（在面板里按 `Ctrl+c` 会打断这个回合）。全部撤回要么全做要么不做：如果有一处改动在缓冲区里撤不了（它创建或清空文件、改变行尾、或太大），或者这个回合有些改动没有画在那里，它什么也不改，并告诉你到面板里用 `x`。保留和撤回都作用于你所在的缓冲区，不是整个回合。

画出的东西从不抢你已经在用的键：别的东西映射了同一个键的地方（比如 gitsigns 的 `]h`），那个键留给它，命令仍然可用。`<localleader>` 是你 Neovim 的 `maplocalleader`（默认是 `\`）。

<a id="turning-it-off"></a>
## 关掉它

你的 `init.lua` 里的三个设置（见[配置](configuration.zh-CN.md)）：

```lua
eitri.config.set("review.enabled", false) -- take no snapshots at all; `c` says turn review is off
eitri.config.set("review.hint", true)     -- say "N files changed · c to review" in the status band after a turn
eitri.config.set("review.live", true)     -- draw an agent's edits in your editor as they land (see Live drawings)
```

每一个的默认值在[配置](configuration.zh-CN.md)的表里。`true` 或 `false` 之外的任何值会在启动时被列出来，并点出这个键，这个设置保持它的默认值；`review.enabled` 是 `false` 时，`review.live` 也会说它没有效果。审阅关闭时，不拍任何快照，也打不开任何审阅；不管 `review.hint` 是什么，状态条和输入框上方的那一行都不会提回合的改动。

## 快照存在哪里

在 `$XDG_STATE_HOME/eitri/review/<项目键>/` 下（那个变量没设置时是 `~/.local/state/eitri/review/`），位于 Eitri 自己的一个私有 git 仓库里，每个项目一个目录。Eitri 为一个项目保留最新的 100 个回合，最长 30 天，每个快照最多取 20,000 个文件和 10 秒。你的 `.gitignore`、`.git/info/exclude`（当项目是某个仓库的顶层时）或你全局 git 排除列表里列出的文件，不进快照；当项目是一个更大仓库的子目录时，项目自己目录之上的忽略文件不会被读取。删掉这个目录会丢掉审阅历史：下一个回合会重新开始一份。状态栏还在提示你恢复一次被打断的回退时（见上文），不要删它：那次恢复要用的、保存下来的原始字节也在这个目录里。
