[English](agents.md) | 简体中文
<!-- translated-from: agents.md sha256=4f759e11e2d25330ef5f591fcb4e9913c2d96e182dcc76f4f5cd1a1b44c41dfc -->

# Codex 与其他 agent

agent 面板里的每个标签页都运行一个 agent，创建标签页时选定。在 Hyprland、sway 和 niri 上，面板默认是一个独立的窗口（[合成器窗口](compositor-windows.zh-CN.md)）。默认的 agent 是 Claude Code。一个标签页也可以运行 Codex，或者任何你注册的、通过标准输入输出说 Agent Client Protocol（ACP）的 agent。这一页讲这两种需要准备什么、怎样打开这样的标签页、Eitri 在启动之前会问你什么、谁来回答 agent 的请求，以及和 Claude 标签页相比，哪些地方更弱或者没有。

Eitri 运行的是你自己安装并登录好的 agent；它不附带其中任何一个。面板、按键、队列、[回合审阅](turn-review.zh-CN.md)以及[快速上手](getting-started.zh-CN.md)的其余内容，在每个标签页里都一样。关于[权限](permissions.zh-CN.md)和 [Claude 账号](configuration.zh-CN.md#accounts)的页面只描述 Claude 标签页，除非这一页另有说明。

Claude 标签页还能看到 Claude Code 自己在做的事：后台任务、动态工作流、子 agent、它自己发起的回合，以及它问你的问题。这部分放在最前面，因为 Codex 和 ACP 标签页都没有。

<a id="what-a-claude-tab-shows-of-claude-codes-own-work"></a>
## Claude 标签页能看到 Claude Code 自己做的事

Claude Code 做的很多事，发生在你发起的回合之外：它在后台运行命令和子 agent，运行动态工作流，给自己定时唤醒，等这些有了结果，再自己发起回合。Claude 标签页把这些都显示出来，也让你能停掉它们。

<a id="turns-claude-code-starts"></a>
### Claude Code 自己发起的回合

后台命令结束、monitor 有了报告、后台的子 agent 或工作流跑完、定时唤醒或 cron 任务到点时，Claude Code 会在你没有发提示的情况下开始一个回合。标签页把它当作和你的回合一样的回合来显示：第一行写明是什么引发了它，比如 `Background command "build" completed` 或 `Scheduled wake-up: check the deploy`，后面是它的工具调用、卡片和回复。它运行期间，活动行显示 `Claude Code is handling: ...`；它引出的卡片就放在这个回合里对应的工具行下面。

这样的回合进行中按 `Enter`，消息会进入这个回合，和任何进行中的回合一样。Claude Code 留到之后再读的消息，会在活动行上方显示为一行 `↪`，等 Claude Code 读到后，再出现在它被读到的位置。

<a id="background-tasks-and-tasks"></a>
### 后台任务与 `/tasks`

状态栏用 Claude Code 的说法统计这个标签页的后台工作：`1 shell`、`2 shells, 1 monitor`、`1 local agent`。没有回合在运行时，输入框上方还有一行显示同样的内容，结尾是 `/tasks`。

`/tasks`（或 `/bashes`）打开后台任务列表：shell、monitor、local agent（在后台运行的子 agent）和动态工作流，然后是已经结束的任务。`j` 和 `k` 选择任务，`Enter` 显示详情：shell 或 monitor 显示它输出的末尾，转义序列已去掉，隐藏字符会显示出来；工作流显示它的各个阶段和 agent。`x` 停掉选中的任务，只停这一个。`o` 在 Neovim 里以只读方式打开它的输出。`q` 返回；在列表里按则关闭列表。全部按键见[按键](keys.zh-CN.md#the-background-tasks-list)。

`<leader>k` 会先问一个列出这些任务的 `y/n`（`Stop 2 shells, 1 monitor? y/n`），然后停掉这个标签页的所有后台任务。它只在没有回合运行时可用，对话会继续下去。

`Ctrl+c` 打断回合，但不会停掉它留下的后台工作；要停，用 `/tasks` 或 `<leader>k`。后台任务还在运行时，关闭这个标签页、它所在的视图或整个窗口都会先问，并说明还有什么在运行。定时唤醒和 cron 任务不算在内，所以关闭会直接取消它们，不会先问；被要求跨会话保留的 cron 任务除外（见[Claude 标签页暂时还没有的](#not-in-a-claude-tab-yet)）。

<a id="dynamic-workflows"></a>
### 动态工作流

动态工作流是 Claude Code 在后台运行的一个脚本，带着它自己的 agent。需要你批准时，卡片写着 `Run a dynamic workflow?`，以纯文本显示工作流的名字、参数和脚本；`View raw script` 在 Neovim 里以只读方式打开脚本。运行期间，状态栏显示它的名字、已完成的 agent 数量、用时和 token。`/workflows` 打开任务列表并定位到工作流，工作流的详情先列出各个阶段，再列出它的 agent，以及每个 agent 的状态、模型、token 和工具调用次数。在那里按 `x` 会停掉这次运行。工作流结束后，Claude Code 会在一个自己的回合里汇报。

<a id="subagents"></a>
### 子 agent

Claude 在前台运行的子 agent，它的调用嵌套在这个子 agent 那一行下面，`Enter` 展开或折叠。在后台运行的子 agent 只占一行状态，写着它用了几次工具，直到完成；它交回的报告会开始一个自己的回合。

<a id="notices-compaction-and-clear"></a>
### 提示、压缩与 `/clear`

Claude Code 的提示出现在它的终端里显示的位置。Claude Code 压缩对话时，活动行显示 `Compacting conversation...`，之后一条分隔线标出压缩的位置，并写着压缩前后的 token 数。模型服务过载时，活动行显示重试（`overloaded · Retrying in 1s · attempt 1/2`），直到回复到来。被 `UserPromptSubmit` hook 拦下的提示，会在一行淡色的行里显示 hook 的原话。

`/clear` 在同一个标签页里开始一段新对话。画面清空，后台任务继续运行，并且仍然显示在状态栏里；之后恢复这个标签页时，接上的是新对话。

<a id="long-running-commands"></a>
### 长时间运行的命令

Claude Code 限制 Claude 能给一条命令设的超时。Eitri 启动它时把 `BASH_MAX_TIMEOUT_MS` 设为 2147483647，也就是它接受的最大值（将近 25 天），所以 Claude 带着长超时启动的后台命令，会一直运行到这个超时为止。没有给超时的后台命令，仍然会在 30 分钟后被停掉。这是 Claude Code 自己的默认值，Eitri 有意保留它。发生这种情况时 Claude 会得到通知，可以带上超时重新启动这条命令，`/tasks` 里这个任务的详情也会写明。如果你自己设置了 `BASH_MAX_TIMEOUT_MS`，不管是在环境变量里，还是在 Claude Code 的 `settings.json` 的 `env` 里，Claude Code 都用你的值。

<a id="claudes-questions"></a>
### Claude 的提问

Claude 用选择题问你时（Claude Code 的 `AskUserQuestion` 工具），提问框会取代输入框，活动行显示 `⚑ Claude has a question`，标签页把这个提问和卡片一起计数。用数字选一个选项，或者用 `j` 和 `k` 移到它上面再按空格；这两种方式都会停在当前问题上，在可以多选的问题里，同样的键可以选上或取消任意多个选项。在只能选一个的问题里，`Enter` 选定光标所在的选项并进入下一步。`Type something` 可以填你自己的回答，`Ctrl+g` 在 Neovim 里编辑它。`n` 给带预览的选项加备注。`Tab` 进入下一个问题；问题不止一个时，最后有一页汇总你的回答。`Ctrl+Enter` 把所有回答一起发出。在 `Chat about this` 上按 `Enter`，是告诉 Claude 你想先聊一聊（按它的数字只会把光标移过去，空格在它上面不起作用）；`Esc` 取消提问，这会停掉当前回合。`Ctrl+k` 回到对话，提问继续等着，按 `i` 再回到提问框。写到一半的回答，在切换到别的标签页再回来、或重新加载页面之后都还在。

只有你能回答提问：bypass、`Ctrl+y`、卡片按键、保存的规则和 Claude Code 的分类器都不会替你回答（见[权限](permissions.zh-CN.md#claudes-questions-to-you)）。有提问在等时关闭标签页，会先问，并说明关闭会取消这个提问。全部按键见[按键](keys.zh-CN.md#claudes-questions)。

<a id="resuming-a-conversation"></a>
### 恢复对话

恢复的对话是根据 Claude Code 自己的会话文件重建的，回合的划分也和 Claude Code 一致。Claude Code 自己发起的回合，会连同引发它的原因一起回来。你输入过的斜杠命令和 `!` 命令，会作为你的提示回来。提问会连同你给出的回答一起回来，或者显示为已拒绝。Claude Code 在打断之后写下的那行 `No response requested.` 不会出现。

有几样东西不会回来：子 agent 自己的调用（Claude Code 把它们存在单独的文件里），以及另一个 Claude Code 会话在回合进行中送进来的消息（实时的标签页里也不显示它）。还有少数回合，是 Claude Code 出于 Eitri 暂时还不认识的原因自己发起的，它们会并到前一个回合里。被打断的提问，或者标签页关闭时还在等的提问，恢复后可能显示为已拒绝，因为 Claude Code 就是这样记录的。

<a id="not-in-a-claude-tab-yet"></a>
### Claude 标签页暂时还没有的

plan 模式、给消息附带文件或图片，以及定时唤醒和 cron 任务的列表。只要标签页开着，它们照样会触发，每次都是一个单独的回合。关闭这个标签页、它所在的视图或整个窗口会取消它们；Eitri 没有它们的列表，所以不会先问。Claude Code 只会在会话结束后保留一种 cron 任务：被要求跨会话保留的那种，它会存在项目的 `.claude/scheduled_tasks.json` 里，之后在这个项目的另一个会话里可能还会运行。

Claude Code 终端里把正在运行的前台命令移到后台的 `Ctrl+B`，在 Claude 标签页里没有对应的操作。在 Eitri 里，`Ctrl+b` 是前缀键。

<a id="opening-a-tab-for-another-agent"></a>
## 为另一个 agent 打开标签页

- **`:tabnew codex`**，或者 `:tabnew` 后面跟 `claude` 或一个你注册过的 agent 的名字，在面板的命令行里输入（浏览模式下按 `:`，或在任何地方按 `<prefix> :`；见[命令行](keys.zh-CN.md#the-command-line)）。它会打开一个运行该 agent 的新标签页，并把你带进去。在新窗口的起始画面上也能用。不带词的 `:tabnew` 打开默认 agent 的标签页，和 `<prefix> c` 打开的一样。不对应任何 agent 的词会被拒绝，并给出一条列出可接受的词的消息。
- **`init.lua` 里的 `agent.default`** 选择新标签页和新窗口的第一个标签页运行哪个 agent：

  ```lua
  eitri.config.set("agent.default", "codex")    -- "claude"（默认）、"codex" 或一个已注册的名字
  ```

  写别的值，Eitri 会在启动时停下，并给出一条点出这个键的消息。

运行其他 agent 的标签页会在标签栏里写出它的名字；它的空标签页的菜单写的是 `New Codex session`（或者 `New` 加上 agent 的名字），上面一行里原来显示 Claude 账号的地方显示 agent 的名字。在你发送第一条消息、并且项目已经被问过（见下文）之前，什么都不会启动。`<prefix> i` 显示标签页的详情；这类标签页有它自己的几行，其中有 `runs without asking` 和 `mode lag`。

<a id="codex"></a>
## Codex

<a id="what-you-need"></a>
### 需要什么

- `codex` 命令，已经安装，并用 `codex login` 登录。没有登录时，标签页会说 “Codex is not signed in: run `codex login` in a terminal”。
- Codex 0.160.0 或更新。Eitri 试过 0.160.0 到 0.163.0；标签页的 `agent` 一行会这么写。更新的 Codex 仍然能运行，这一行会写 “untested Codex”，后面跟着它的版本和试过的最新版本。更旧的会被拒绝，并提示 “update Codex”。

Eitri 按这个顺序挑选程序：环境变量 `EITRI_CODEX`（一个文件的绝对路径），然后是 `init.lua` 里的 `agent.codex.command`（绝对路径，或在 `PATH` 里查找的裸命令名），最后是 `PATH` 上的 `codex`。`EITRI_CODEX` 不是一个现有文件的绝对路径，Eitri 会在启动时停下并点出这个变量；`agent.codex.command` 是带目录部分的相对路径时也一样。其他找不到 `codex` 的情况只让 Codex 标签页的启动失败：`PATH` 上什么都没有时，消息是 “codex not found: install it and run `codex login`, or set EITRI_CODEX”，而 `agent.codex.command` 指向的文件不存在时，消息会点出它的名字。`PATH` 里为空或相对的条目会被跳过，因为 agent 是在你的项目目录里运行的，这样的条目会指向项目自己放的文件。

<a id="trust-the-question-codex-tabs-ask"></a>
### 信任：Codex 标签页会问的问题

和 Claude 标签页一样，Codex 标签页在一个项目里的第一个会话之前，会问是否加载项目自己的配置，并显示它找到了什么。没有什么可信任的项目会直接启动，不问问题。问题写的是 “Trust this project's Codex configuration?”。对 Codex，Eitri 会在项目目录以及它之上的每一层目录（直到所在 git 仓库的顶层，永远不越过你的主目录）里查看：

- `.codex/`：它的配置、规则、技能和钩子；
- `AGENTS.md` 和 `AGENTS.override.md`，以及你自己的 Codex `config.toml` 里列出的其他备选指令文件名；
- `.agents/skills/` 和 `.agents/plugins/`，以及顶层上其他工具保存的插件清单。

被链接的 git 工作树还会显示主检出里的 `.codex/`。你自己的 Codex 目录（`CODEX_HOME`，或 `~/.codex`）不属于项目，会被标为未读取。

这个回答的运作方式和 Claude 一样（见[信任一个项目](permissions.zh-CN.md#trusting-a-project)）：`y` 会为这个项目记住，并绑定到你看到的内容，所以内容一变就再问；Eitri 检查不了的文件会让这个回答只管一次启动；问题里的任何内容都不会被截断，所以提问会超过 2 MiB 的项目只会得到一个简短的、只接受 `n`（或 `Esc`）的问题，`y` 会被拒绝；`:trust` 和 `:untrust` 作用于屏幕上那个标签页的 agent。每个 agent 都有自己的问题、自己被记住的回答和自己的 `:untrust`：为 Claude 信任了一个项目，对 Codex 什么也说明不了，两个 agent 的两个等待中的标签页会被问两次。

需要知道的是 `n` 做什么。Codex 标签页可以在不加载项目配置的情况下启动，所以 `n` 就这样启动它：Eitri 告诉 Codex 不要信任项目目录（以及它之上的任何目录），并且在项目未被信任期间，技能（你自己的也包括在内）都被关掉。你自己的 Codex 配置总会加载，Eitri 没办法关掉它。如果在这样的会话里 Codex 仍然报告它加载了项目里的东西（项目里的指令文件、只有项目才提到的工具服务器，或项目那一层的钩子），Eitri 会结束这个标签页，并给出一条写着 “Codex loaded project configuration Eitri did not allow” 的消息。

<a id="the-first-start-checks-the-codex-binary"></a>
### 第一次启动会检查 Codex 程序

Eitri 第一次启动一个它没检查过的 `codex` 时，会在你自己的机器上，对一个替身模型跑一段几秒钟的脚本化对话。这一步不用登录也不联网，也从不碰你的账号。这段对话会用一份刻意设成敌意的配置，试着让 Codex 用上 Eitri 关掉的东西，只有这些全都没有生效，检查才算通过。检查运行期间，状态栏写的是 “checking this Codex… (once for this binary, a few seconds)”。

- 通过会针对这个确切的程序（它的路径、大小、修改时间和版本）被记在 `~/.local/state/eitri/codex-check/` 下（设了 `XDG_STATE_HOME` 时在它里面）；Codex 更新之后会重新检查。
- **检查通过之前，每一次 Codex 启动都会被拒绝**，已信任的项目也一样，消息是 “Eitri could not confirm that this Codex keeps its levers”，后面跟着一个日志 `check.log` 的路径，它就在判定文件旁边。失败不会被记住，所以下次启动会再试。
- 检查会把沙箱绝不能让 Codex 写入的文件放在它自己的一个目录里。Codex 的沙箱总是允许命令写 `/tmp` 和 `$TMPDIR`，所以如果你的状态目录在这两者之一之下，而 `XDG_RUNTIME_DIR` 和你的缓存目录又都不在别处，检查就分辨不出沙箱是守住了还是坏了，于是失败；把 `XDG_STATE_HOME` 设到它们之外的目录。

<a id="manual-auto-and-bypass-in-a-codex-tab"></a>
### Codex 标签页里的 Manual、auto 和 bypass

`Shift+Tab` 像在任何 Claude 标签页里一样轮换 Manual、Auto 和 Bypass，进入 bypass 时问同样的问题。这些模式在这里意思不同，因为 Codex 有它自己的批准流程：

- **Manual。** Codex 的审查者交给了你：每当 Codex 需要离开它的沙箱，它都会问，而每一个这样的请求都是一张卡片。Manual 管不到沙箱里面：那里的命令和编辑仍然不经请求就运行，每种模式下都是如此（见下文）。

- **Auto。** Codex 在它自己的审查者之下运行。当它需要离开沙箱时，由它的审查者决定。审查者允许或拒绝的调用，那一行上会有一条说明：`allowed by auto review: ...` 或 `blocked by auto review: ...`，后面是审查者给的理由。审查者没有裁决的，会以卡片的形式交给你。
- **Bypass。** Codex 发来的、本来会变成卡片的每一个请求，Eitri 都自己批准，不出卡片；那一行写的是 “allowed in bypass”。进入 bypass 时，已经在等待的卡片会被批准。

不论哪种模式，Eitri 的分类器、你保存的规则，以及 Claude 标签页里“交还给 Claude Code”的处理，都不适用于 Codex；一个请求要么是一张卡片，要么由审查者回答，要么按下面所说由 Eitri 自己回答。

模式的改变在 Eitri 里立即生效，在 Codex 的审查者那里要到下一个回合：正在运行的回合沿用它开始时的审查者（在 Manual 或 Bypass 下开始的回合，对沙箱之外的一切都继续询问；在 Auto 下开始的回合，继续由它的审查者决定）。`<prefix> i` 的 `mode lag` 一行会这么说。

**在卡片上批准和拒绝。** `a` 批准一次，什么都不会被记住；没有“总是允许”。`d` 拒绝，回合继续。`D` 会闪出提示：Codex 的拒绝不接受理由。`Ctrl+c` 中断回合，同时取消正在等待的请求。卡片显示它批准的东西：命令和它所在的文件夹、每个被改的文件及其完整的 diff，或者工具服务器、工具和参数。命令卡片会写明它 “runs outside the sandbox with your full rights”，网络卡片写的是它可能这样。

**Eitri 在每种模式下都自己回答的，那一行上会有说明。** 有些请求不值得出卡片，或者没法展示，Eitri 会拒绝它们或什么也不授予：向一个已经在运行的命令里输入的内容；Eitri 没有完整看到的文件改动；要求在本次会话剩余时间里可以写某个文件夹下任何位置的请求；要求额外权限的请求（Eitri 一概不授予）；它展示不了的命令或工具调用；工具服务器发来的、不属于工具批准的表单。那一行写的是 “answered by Eitri:”，后面是 Codex 要求了什么，以及大多数情况下 Eitri 做了什么，比如 “Codex asked for extra permissions; Eitri grants none”。

<a id="what-codex-does-without-asking"></a>
### Codex 不经询问做什么

Codex 在它的 `workspace-write` 沙箱里运行，批准策略是 `on-request`，Eitri 每个回合都发送这个沙箱的窄版本，这样你自己的或项目的 Codex 配置里的 `network_access` 或额外的可写文件夹就没法把它放宽。在沙箱里面：

- 项目内部的命令和编辑不出卡片就运行。写 `/tmp` 和 `$TMPDIR` 也可以，Codex 在这个沙箱里允许这样做。
- **Codex 能读取你能读取的任何文件。** 沙箱限制的是写入、网络和套接字，不限制读取。
- 沙箱里没有网络，也不能访问套接字。
- **你自己的 exec-policy `allow` 规则覆盖的命令在沙箱之外运行**，不经任何请求。
- **MCP 工具遵循它们所在服务器的批准模式。** 服务器不需要批准的工具，不经请求就运行。
- 离开沙箱的请求，在 Manual 下是一张卡片，在 Auto 下由审查者决定，在 Bypass 下由 Eitri 批准。这样被批准的命令以你的全部权限运行。

在[不算边界的文件夹](permissions.zh-CN.md#a-folder-that-is-no-boundary)里，Codex 改在它的 `read-only` 沙箱里运行：读取和上面一样，什么也不写，也没有网络和套接字，所以每次写入都必须请求离开沙箱。这个请求在 Auto 下也是一张卡片，只有 Bypass 会不问就批准。上面其他的途径不变：你的 exec-policy `allow` 规则和 MCP 工具。即使信任问题回答了 `y`，项目自己的 Codex 配置在那里也不会加载。Eitri 在每次启动和每个回合都会看一次这个文件夹，子 agent 继承这个沙箱。

`<prefix> i` 在 `runs without asking` 一行里为屏幕上的标签页把这些写明，这一行随标签页的模式而变。

<a id="interrupting-subagents-and-resuming"></a>
### 中断、子 agent 与恢复

- **中断。** `Ctrl+c` 立刻结束回合，但 Codex 会让已经启动的命令运行到结束。Eitri 在按键之后最多等三秒。如果到那时仍有命令、文件改动或它读不懂的东西在运行，Eitri 会停掉 Codex 会话和它启动的一切，并说明：“Codex kept running a command after the interrupt; its session was stopped so nothing more runs. The conversation can be resumed.” 如果 Eitri 让 Codex 停下回合三秒之后，回合本身仍未结束（回合刚开始时 Codex 可能拒绝中断，Eitri 会先再请求一次），也会这样停掉会话。模型在自己那次调用之后留在后台继续运行的命令不在此列：回合照常结束，状态栏显示还有几个在运行，那个回合的审查会说明之后的改动不在其中，`<leader>k` 会在一个 `y/n` 之后通过停掉会话来停掉它们（对话之后可以恢复）；之后的一次中断，或关闭标签页，也会停掉它们。
- **回合运行时发的消息。** `Enter` 会把它插进正在运行的回合（Codex 的 `turn/steer`）而不打断回合；Codex 在下一步读到它（如果它正在等某个命令或卡片，就在那之后），对话会在 Codex 读到它的地方显示它。Codex 拒绝的消息（按 Codex 自己的源码，审查或压缩期间可能拒绝接收；Eitri 尚未实际见到这种情况）或者回合结束时还没读到的消息，会留到回合结束再发，并有一行说明原因。回合被中断时，Codex 会丢掉还没读到的消息，所以 Eitri 会在下一个回合把它重新发出。
- **子 agent。** Codex 可能启动子 agent，Eitri 没法关掉这一点。它们的行和卡片写的是 `subagent`，它们继承标签页的沙箱和信任回答，中断或关闭标签页会波及所有子 agent，而在每一个都空闲之前，标签页都算忙。子 agent 自己的文字不会显示。
- **恢复。** Codex 的对话可以恢复。恢复选择器和 `w` 只列出你所在标签页那个 agent 的已保存会话。保存的标签页的恢复方式和 Claude 的一样（见[快速上手](getting-started.zh-CN.md#tabs-and-sessions)），但每个 Codex 标签页回来时都带着它的恢复信息，要等 Codex 自己的信任问题有了结果才启动：被回答了，或者因为项目没有什么可信任的而不需要问。
- **用量。** 标签页在 `prefix i`（`limits`）里显示 Codex 的速率限制报告，Codex 报告了的话，还会在回合结束那一行里写出重置时间。Codex 的 token 不带费用，所以不显示费用。

<a id="what-a-codex-tab-does-not-have"></a>
### Codex 标签页没有什么

- 斜杠命令按输入的原样发给 Codex。没有被拦下的命令列表，没有 `/model` 或 `/effort` 选择器，也没有 `/` 命令菜单（Codex 不报告可补全的命令列表）。
- 没有[Claude 标签页能看到的那些 Claude Code 自己做的事](#what-a-claude-tab-shows-of-claude-codes-own-work)：`/tasks` 和 `/workflows` 按原样发给 Codex，状态栏只统计还在后台运行的命令。
- `<leader>t`（在终端里继续）会闪出 “Continuing a Codex session in a terminal is not available yet”。
- `prefix i` 没有只属于 Claude 的几行（Claude 会话 id、CLI 构建、权限规则、账号和模型）。
- 保存的权限规则、Claude Code 的 auto 模式和 `agent.user_settings` 开关都不适用。

<a id="other-agents-over-acp"></a>
## 通过 ACP 接入的其他 agent

任何通过标准输入输出说 ACP 第 1 版的 agent 都可以成为一个标签页。你在 `init.lua` 里注册它：

```lua
eitri.agent.register({
  name = "myagent",                              -- :tabnew 和 agent.default 对它的称呼
  command = { "my-acp-agent", "--acp" },         -- 程序和它的参数
  project_files = { ".myagent/", "MYAGENT.md" }, -- agent 从项目里读取的东西
  auto_mode = "auto",                            -- 可选：agent 的一个模式 id
  read_only_mode = "plan",                       -- 可选：用于不算边界的文件夹的模式
})
```

- **`name`** 必填：由字母、数字、`.`、`_` 和 `-` 组成，以字母或数字开头，至多 64 个字符。`claude` 和 `codex` 不论大小写都会被拒绝，同一个名字只能注册一次。
- **`command`** 必填：一个字符串列表，程序在最前。程序必须是绝对路径，或在 `PATH` 里查找的裸命令名；带 `/` 的相对路径会指向项目里的某个文件，因此会被拒绝。启动时不检查程序是否存在：不存在的只会让那个标签页的启动失败。
- **`project_files`** 必填且不能为空：agent 从项目里读取的文件和目录（结尾的 `/` 表示目录），每项一个名字，里面不能有 `/`。Eitri 用它来问下面的信任问题。请把 agent 读取的一切都列出来：这一点 Eitri 没法替你知道。
- **`auto_mode`** 可选：agent 某个模式的 id。见[模式](#modes-and-the-agents-own-settings)。
- **`read_only_mode`** 可选：在[不算边界的文件夹](permissions.zh-CN.md#a-folder-that-is-no-boundary)里代替 `auto_mode` 使用的模式 id。Eitri 不知道一个模式会做什么，所以要点名一个只读的模式。没有它时，agent 在这样的文件夹里保持平常的模式。

任何其他键、或者不对的值，都会让 Eitri 在启动时停下并点出这个键，而且在调用外面包一层 `pcall` 也藏不住它。之后 `:tabnew myagent` 就打开一个它的标签页，`agent.default` 也可以写它的名字；`agent.default` 是在整个文件跑完之后才读的，所以它可以写在 `register` 调用的前面。

<a id="trust-an-acp-agent-starts-only-after-a-yes"></a>
### 信任：ACP agent 只在你说了 yes 之后才启动

ACP agent 会自己从项目里读它的配置，而 Eitri 没有办法让它不读。所以对注册的 agent，这个问题的运作方式不同：

- 这个问题涵盖你在 `project_files` 里列出的文件，在项目目录以及它之上直到所在 git 仓库顶层的每一层目录里查找。列出的目录会被整个读取。
- **即使这些文件一个也不存在，也要问**：“myagent loads whatever project files it finds; there are none now. Start it here?”（用你的 agent 的名字）。`y` 会针对这个空的状态被记住，所以之后出现一个列出的文件，就会再问。
- **`y` 启动 agent；`n` 不启动。** `n` 不会被记住，标签页会说这个 agent 会加载这个项目自己的配置、而你没有信任它，所以 Eitri 没法在不加载它的情况下启动。页脚写的是 “y trusts and starts it · n does not start it · Esc puts this off”。
- 问题里的任何内容都不会被截断。如果它会超过 2 MiB，Eitri 没法完整显示它，这个 agent 的启动就会被拒绝，并给出一条说明这一点的消息。
- Eitri 分辨不出 agent 实际会跟随列出的哪些文件，所以列出的文件或目录如果是符号链接，或者列出的目录里任何地方有链接，这个回答就只管一次启动。
- 这个问题和被记住的回答只属于那个 agent。一个名字已经不再注册的标签页，会在没有任何文件的情况下被问一次，然后被拒绝：“was not started: no agent of that name is registered in init.lua”。

Eitri 用你当前的环境原样启动 agent。如果一个 agent 把环境变量里的相对路径当作它的配置目录，它就会把项目的目录当成你的目录来读，而对于一个通用的 agent，Eitri 对此什么也不做。（Codex 标签页正是出于这个原因拒绝相对的 `CODEX_HOME`。）

<a id="modes-and-the-agents-own-settings"></a>
### 模式与 agent 自己的设置

Eitri 不向 agent 提供文件或终端方法，也从不调用它的认证步骤。它对 agent 只设一件事：注册里有 `auto_mode` 时，这个模式 id 会在每个会话开始时设上，两种 Eitri 模式下都一样。之后 Eitri 会拿 agent 报告的每一次模式去和它对照：设置被拒绝，或者有差异，都会让标签页带着一条不一致的消息结束，之后什么也不再回答。在不算边界的文件夹里，有 `read_only_mode` 时，它在这一切里代替 `auto_mode`。两者都没有时，Eitri 什么也不设、什么也不检查，`prefix i` 的 `runs without asking` 一行会说 Eitri 不知道这个 agent 不经询问会运行什么。

Eitri 的 Auto 和 Bypass 是 Eitri 自己的：它们立即决定 Eitri 怎样回答 agent 的请求，并且从不改变 agent 的模式。`mode lag` 一行说的就是没有这种延迟。注册的 agent 的标签页没有 Manual 模式：agent 自己的模式只在开始时设一次，Eitri 之后没法让它更严格。在那里 `Shift+Tab` 在 Auto 和 Bypass 之间切换；新标签页默认以 Manual 开始时，新开的这种 agent 的标签页会以 Auto 开始，Eitri 会说一次。

<a id="cards"></a>
### 卡片

到达 Eitri 的每个请求，在 Auto 下都是一张卡片；在 Bypass 下由 Eitri 批准。保存的规则和 Eitri 的分类器都不适用。

- 批准选择 agent 的 “allow once” 选项，拒绝选它的 “reject once” 选项。“allow always” 和 “reject always” 选项从不提供也从不选择。一个请求如果没有你所按的那一类选项，就没法这样回答，卡片会留着。
- 无法展示整个调用的卡片没有批准选项：调用输入超过 256 KiB、多个 diff 加起来超过这个数，或者某个 diff 的路径或文本不是字符串。卡片会写明 Eitri 扣下了这个选项，并列出文件名。这时 `a` 和 `Ctrl+y` 会闪出 “this card cannot be approved”。你仍然可以拒绝它或中断。
- 卡片会画出每个 diff 的每一行。
- `Ctrl+c` 中断：Eitri 取消回合，并把每一个未决的请求都答成已取消。没有本地超时；一个永远不结束回合的 agent 会让标签页一直忙，直到你把它关掉。

<a id="resuming"></a>
### 恢复

只有当 agent 表示它能加载已保存的会话时，ACP 标签页才可以恢复，才会跟其他标签页一起保存。保存的标签页回来时带着它的恢复信息，要等它自己那个 agent 的问题被 `y` 回答后才启动。名字已经不在 `init.lua` 里注册的已保存标签页会被跳过。

<a id="what-an-acp-tab-does-not-have"></a>
### ACP 标签页没有什么

ACP 没有办法把消息插进正在运行的回合，所以回合运行时按 `Enter`，消息会留到回合结束再发。ACP agent 可能发来的计划、会话标题和其他更新都不显示；这类标签页没有速率限制、没有上下文条，也没有模型选择器。agent 报告的命令列表就是输入框里的 `/` 菜单（见[输入框的按键](keys.zh-CN.md)）：名字和说明都按纯文本绘制，agent 还没报告过命令的标签页没有菜单。agent 报告上下文有多满时，`prefix i` 有一行 `context`，agent 报告之后它显示 agent 的上下文有多满。token 不带费用。

## 从 agent 标签页使用你的编辑器

任何标签页里的 agent，包括 Codex 和 ACP，都可以给你看一个文件、读你的编辑器里现在有什么、向你的语言服务器询问代码，并提议改动等你批准。这些工具做什么、Eitri 什么时候先问你、Codex 标签页不带它们时会说什么、怎样关掉它们，都在单独一页：[从 agent 标签页使用你的编辑器](editor-tools.zh-CN.md)。[跟随 agent 的修改](editor-tools.zh-CN.md#following)和 [`nvim_exec`](editor-tools.zh-CN.md#nvim-exec)（唯一一个你打开之前一直关着的工具）也在那里。

<a id="chat-views"></a>
## 聊天视图

你一开始用的 agent 面板是**基础视图**（`agent`）。**聊天视图**（`agent#2` 到 `agent#9`）是同一个面板的另一个页面，是布局里一个单独的模块，只显示一个会话标签页；基础视图显示其余所有的标签页，并带着它的标签条。创建和使用它们的按键在[按键](keys.zh-CN.md#chat-views)里。对 agent 来说，要紧的是这些：

- **视图只是标签页显示的地方。** 标签页属于窗口，不管它运行的是哪个 agent（Claude、Codex 或 ACP agent）：视图被隐藏时它照常运行，它的卡片等着你，它的会话和其他的一起保存。整个窗口只有一个账号，所以每个视图的标签页用的都是窗口启动时所用的账号（[账号](configuration.zh-CN.md#accounts)）。
- **`<prefix> !`** 把你正在用的标签页拆出来，放进它自己的视图，在那个视图里再按就把它并回去。基础视图永远留着一个标签页，所以只有一个时它会拒绝。新视图也可以从空的开始（`instance.new { kind = "agent" }`、`eitri.layout.open("agent")`，或者在聊天里按分割键再按 `a`）；它的第一条消息像任何第一个会话一样，要在那个视图里先问[信任问题](permissions.zh-CN.md#trusting-a-project)。
- **卡片只能在显示它的标签页的那个页面里回答。** 标签页被你拆出去之前显示在基础视图，拆出去之后显示在它的额外视图；从任何别的页面发出的回答（`a`、`d`、`D`、`Ctrl+y`、按钮、bypass 的 `y`，或者信任的 `y` 和 `n`）都会被拒绝，卡片继续等着。这条规则针对的情形是：你在基础视图里按下回答，恰好在这一刻 `<prefix> !` 把那个标签页移进了它自己的视图；这个回答到不了标签页，卡片在它的新视图里等着你回答。
- **被盖住的视图不能回答，信任问题会再问一次。** 一个浮动窗口盖住一个视图（见[浮动窗口下面的聊天](#a-chat-under-a-float)）；检查是按视图做的，所以盖在 `agent#2` 上的浮动窗口会拒绝那个视图的回答，不影响基础视图的。从被盖住的页面回答的是信任问题时，回答被拒绝并给出原因，同一个页面里会再次显示这个问题，浮动窗口移开之后就可以回答。在被盖住的页面里输入 `:trust` 会以“未信任”结束并给出原因（什么都不会记录），页面不再被盖住之后再输入一次就会问这个问题。
- **基础视图的标签条只列出它自己的标签页**，所以在别的视图里显示的标签页会在编号里留下空缺（选择器 `w` 列出每个标签页，并在显示于某个视图的标签页后面写上 ` · agent 2`）。只有一个标签页的视图不画标签条。
- **恢复的标签页回到它的视图**，条件是布局里还有那个视图、并且没有别的标签页占了它；否则回到基础视图。恢复结束之后没有标签页的视图会被移除。
- **每个视图有自己的注意力提示。** 它的芯片和标签条条目在那个视图的标签页里有卡片等着时读作 `agent 2 ⚑1`；`modules.chat.on_permission = "reveal"` 时，隐藏着的视图里来了新卡片会像基础视图那样被显示出来，提示里写的是那个视图和回到它的办法。

<a id="a-chat-under-a-float"></a>
## 浮动窗口下面的聊天

agent 面板，或者任何一个[聊天视图](#chat-views)，可以放在一个[浮动窗口](keys.zh-CN.md#floats-the-scratchpad-and-workspaces)下面（比如便笺板里的一个终端）。聊天只要被某个浮动窗口盖住了任何一点，它仍在运行，但按“被盖住”处理，而不是按“显示着”处理：

- **它在顶栏里的芯片回来了。** 有卡片等着时读作 `agent ⚑N`，和聊天被隐藏时一样。被盖住时结束的一轮会留下 `agent •`，浮动窗口离开聊天后它才消失。
- **不能在它上面回答卡片。** 在被盖住的页面上回答卡片（`a`、`d`、`D`、`Ctrl+y`、按钮），或者按进入 bypass 模式的 `y`，都会被拒绝，卡片继续等着。应用名会闪一下，提示和页面会说是哪个浮动窗口盖住了它，以及怎样解除（按键要在那个浮动窗口里时按）：便笺浮动窗口读作 `agent is under terminal 2: <prefix> Alt+- from terminal 2 stows it`，其他浮动窗口读作 `agent is under terminal: <prefix> Alt+Shift+Space from terminal sinks it`（没有键时写动作的名字）。在聊天里按时，第一次 `<prefix> Alt+-` 把按键移进一个显示着的便笺浮动窗口，第二次才把它收起。检查是在你回答的那一刻、按浮动窗口当时的位置做的，所以浮动窗口一挪开或被收起，回答立刻就能通过。
- **`modules.chat.on_permission = "reveal"` 时**，新来的卡片会收起盖住聊天的便笺浮动窗口，拿着按键的那个除外（按键不动）。这之后仍盖着聊天的浮动窗口，或者默认的 `"badge"` 下的任何遮盖，只得到芯片和提示。

<a id="known-limits"></a>
## 已知限制

- **agent 不经询问运行什么，是 agent 自己的事。** 对 Codex，清单在[Codex 不经询问做什么](#what-codex-does-without-asking)下。对 ACP agent，Eitri 会显示它收到的每个请求，但 agent 因为自己的模式已经允许某个动作而根本没发出的请求，永远不会到你这里。
- **agent 自己的配置总会加载**，Eitri 没办法关掉它。信任一个项目，只是把项目的配置加在它上面。
- **你的钩子可能抢先回答。** 你自己的 Codex 配置里的 `PermissionRequest` 钩子，可能在 Eitri 看到请求之前就回答了它。钩子能否在你批准之后改写命令，还没有测过。
- **Codex 的沙箱能读到你能读到的任何地方。** 它不是保护机密的边界。
- **这些 agent 没有 Eitri 一侧的分类器或保存的规则。** Claude 标签页的权限检查是 Claude 独有的。
- **这些标签页是新的。** 这里描述的大部分内容是拿替身试过的，并没有在每一种环境上试过；[已知问题](../known-issues.zh-CN.md#codex-and-acp-tabs)列出了还没解决的。
