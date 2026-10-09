[English](agents.md) | 简体中文
<!-- translated-from: agents.md sha256=e0ccd7d10db64dcbb19a99448ede21abc15babef39840ad49fa82af44920a62b -->

# Codex 与其他 agent

agent 面板里的每个标签页都运行一个 agent，创建标签页时选定。默认是 Claude Code。一个标签页也可以运行 Codex，或者任何你注册的、通过标准输入输出说 Agent Client Protocol（ACP）的 agent。这一页讲这两种需要准备什么、怎样打开这样的标签页、Eitri 在启动之前会问你什么、谁来回答 agent 的请求，以及和 Claude 标签页相比，哪些地方更弱或者没有。

Eitri 运行的是你自己安装并登录好的 agent；它不附带其中任何一个。面板、按键、队列、[回合审阅](turn-review.zh-CN.md)以及[快速上手](getting-started.zh-CN.md)的其余内容，在每个标签页里都一样。关于[权限](permissions.zh-CN.md)和 [Claude 账号](configuration.zh-CN.md#accounts)的页面只描述 Claude 标签页，除非这一页另有说明。

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
- Codex 0.160.0 或更新。Eitri 试过 0.160.0 到 0.161.0；标签页的 `agent` 一行会这么写。更新的 Codex 仍然能运行，这一行会写 “untested Codex”，后面跟着它的版本和试过的最新版本。更旧的会被拒绝，并提示 “update Codex”。

Eitri 按这个顺序挑选程序：环境变量 `EITRI_CODEX`（一个文件的绝对路径），然后是 `init.lua` 里的 `agent.codex.command`（绝对路径，或在 `PATH` 里查找的裸命令名），最后是 `PATH` 上的 `codex`。`EITRI_CODEX` 不是一个现有文件的绝对路径，Eitri 会在启动时停下并点出这个变量；`agent.codex.command` 是带目录部分的相对路径时也一样。其他找不到 `codex` 的情况只让 Codex 标签页的启动失败：`PATH` 上什么都没有时，消息是 “codex not found: install it and run `codex login`, or set EITRI_CODEX”，而 `agent.codex.command` 指向的文件不存在时，消息会点出它的名字。`PATH` 里为空或相对的条目会被跳过，因为 agent 是在你的项目目录里运行的，这样的条目会指向项目自己放的文件。

<a id="trust-the-question-codex-tabs-ask"></a>
### 信任：Codex 标签页会问的问题

和 Claude 标签页一样，Codex 标签页在一个项目里的第一个会话之前，会问是否加载项目自己的配置，并显示它找到了什么。没有什么可信任的项目会直接启动，不问问题。问题写的是 “Trust this project's Codex configuration?”。对 Codex，Eitri 会在项目目录以及它之上的每一层目录（直到所在 git 仓库的顶层，永远不越过你的主目录）里查看：

- `.codex/`：它的配置、规则、技能和钩子；
- `AGENTS.md` 和 `AGENTS.override.md`，以及你自己的 Codex `config.toml` 里列出的其他备选指令文件名；
- `.agents/skills/` 和 `.agents/plugins/`，以及顶层上其他工具保存的插件清单。

被链接的 git 工作树还会显示主检出里的 `.codex/`。你自己的 Codex 目录（`CODEX_HOME`，或 `~/.codex`）不属于项目，会被标为未读取。

这个回答的运作方式和 Claude 一样（见[信任一个项目](permissions.zh-CN.md#trusting-a-project)）：`y` 会为这个项目记住，并绑定到你看到的内容，所以内容一变就再问；Eitri 检查不了的文件会让这个回答只管一次启动；`:trust` 和 `:untrust` 作用于屏幕上那个标签页的 agent。每个 agent 都有自己的问题、自己被记住的回答和自己的 `:untrust`：为 Claude 信任了一个项目，对 Codex 什么也说明不了，两个 agent 的两个等待中的标签页会被问两次。

需要知道的是 `n` 做什么。Codex 标签页可以在不加载项目配置的情况下启动，所以 `n` 就这样启动它：Eitri 告诉 Codex 不要信任项目目录（以及它之上的任何目录），并且在项目未被信任期间，技能（你自己的也包括在内）都被关掉。你自己的 Codex 配置总会加载，Eitri 没办法关掉它。如果在这样的会话里 Codex 仍然报告它加载了项目里的东西（项目里的指令文件、只有项目才提到的工具服务器，或项目那一层的钩子），Eitri 会结束这个标签页，并给出一条写着 “Codex loaded project configuration Eitri did not allow” 的消息。

<a id="the-first-start-checks-the-codex-binary"></a>
### 第一次启动会检查 Codex 程序

Eitri 第一次启动一个它没检查过的 `codex` 时，会在你自己的机器上，对一个替身模型跑一段几秒钟的脚本化对话。这一步不用登录也不联网，也从不碰你的账号。这段对话会用一份刻意设成敌意的配置，试着让 Codex 用上 Eitri 关掉的东西，只有这些全都没有生效，检查才算通过。检查运行期间，状态栏写的是 “checking this Codex… (once for this binary, a few seconds)”。

- 通过会针对这个确切的程序（它的路径、大小、修改时间和版本）被记在 `~/.local/state/eitri/codex-check/` 下（设了 `XDG_STATE_HOME` 时在它里面）；Codex 更新之后会重新检查。
- **检查通过之前，每一次 Codex 启动都会被拒绝**，已信任的项目也一样，消息是 “Eitri could not confirm that this Codex keeps its levers”，后面跟着一个日志 `check.log` 的路径，它就在判定文件旁边。失败不会被记住，所以下次启动会再试。
- 检查会把沙箱绝不能让 Codex 写入的文件放在它自己的一个目录里。Codex 的沙箱总是允许命令写 `/tmp` 和 `$TMPDIR`，所以如果你的状态目录在这两者之一之下，而 `XDG_RUNTIME_DIR` 和你的缓存目录又都不在别处，检查就分辨不出沙箱是守住了还是坏了，于是失败；把 `XDG_STATE_HOME` 设到它们之外的目录。

<a id="auto-and-bypass-in-a-codex-tab"></a>
### Codex 标签页里的 Auto 和 bypass

`Shift+Tab` 像在任何标签页里一样在 Auto 和 Bypass 之间切换，进入 bypass 时问同样的问题。这两种模式在这里意思不同，因为 Codex 有它自己的批准流程：

- **Auto。** Codex 在它自己的审查者之下运行。当它需要离开沙箱时，由它的审查者决定。审查者允许或拒绝的调用，那一行上会有一条说明：`allowed by auto review: ...` 或 `blocked by auto review: ...`，后面是审查者给的理由。审查者没有裁决的，会以卡片的形式交给你。
- **Bypass。** Codex 发来的、本来会变成卡片的每一个请求，Eitri 都自己批准，不出卡片；那一行写的是 “allowed in bypass”。进入 bypass 时，已经在等待的卡片会被批准。

不论哪种模式，Eitri 的分类器、你保存的规则，以及 Claude 标签页里“交还给 Claude Code”的处理，都不适用于 Codex；一个请求要么是一张卡片，要么由审查者回答，要么按下面所说由 Eitri 自己回答。

模式的改变在 Eitri 里立即生效，在 Codex 的审查者那里要到下一个回合：正在运行的回合沿用它开始时的审查者。`<prefix> i` 的 `mode lag` 一行会这么说。

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
- 离开沙箱的请求，在 Auto 下由审查者决定，在 Bypass 下由 Eitri 批准。这样被批准的命令以你的全部权限运行。

`<prefix> i` 在 `runs without asking` 一行里为屏幕上的标签页把这些写明，这一行随标签页的模式而变。

<a id="interrupting-subagents-and-resuming"></a>
### 中断、子 agent 与恢复

- **中断。** `Ctrl+c` 立刻结束回合，但 Codex 会让已经启动的命令运行到结束。Eitri 在按键之后最多等三秒。如果到那时仍有命令、文件改动或它读不懂的东西在运行，Eitri 会停掉 Codex 会话和它启动的一切，并说明：“Codex kept running a command after the interrupt; its session was stopped so nothing more runs. The conversation can be resumed.” 模型在自己那次调用之后留在后台继续运行的命令不在此列：回合照常结束，状态栏显示还有几个在运行，那个回合的审查会说明之后的改动不在其中，`<leader>k` 会在一个 `y/n` 之后通过停掉会话来停掉它们（对话之后可以恢复）；之后的一次中断，或关闭标签页，也会停掉它们。
- **子 agent。** Codex 可能启动子 agent，Eitri 没法关掉这一点。它们的行和卡片写的是 `subagent`，它们继承标签页的沙箱和信任回答，中断或关闭标签页会波及所有子 agent，而在每一个都空闲之前，标签页都算忙。子 agent 自己的文字不会显示。
- **恢复。** Codex 的对话可以恢复。恢复选择器和 `w` 只列出你所在标签页那个 agent 的已保存会话。保存的标签页的恢复方式和 Claude 的一样（见[快速上手](getting-started.zh-CN.md#tabs-and-sessions)），但每个 Codex 标签页回来时都带着它的恢复信息，要等 Codex 自己的信任问题有了结果才启动：被回答了，或者因为项目没有什么可信任的而不需要问。
- **用量。** 标签页在 `prefix i`（`limits`）里显示 Codex 的速率限制报告，Codex 报告了的话，还会在回合结束那一行里写出重置时间。Codex 的 token 不带费用，所以不显示费用。

<a id="what-a-codex-tab-does-not-have"></a>
### Codex 标签页没有什么

- 斜杠命令按输入的原样发给 Codex。没有被拦下的命令列表，也没有 `/model` 或 `/effort` 选择器。
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
})
```

- **`name`** 必填：由字母、数字、`.`、`_` 和 `-` 组成，以字母或数字开头，至多 64 个字符。`claude` 和 `codex` 不论大小写都会被拒绝，同一个名字只能注册一次。
- **`command`** 必填：一个字符串列表，程序在最前。程序必须是绝对路径，或在 `PATH` 里查找的裸命令名；带 `/` 的相对路径会指向项目里的某个文件，因此会被拒绝。启动时不检查程序是否存在：不存在的只会让那个标签页的启动失败。
- **`project_files`** 必填且不能为空：agent 从项目里读取的文件和目录（结尾的 `/` 表示目录），每项一个名字，里面不能有 `/`。Eitri 用它来问下面的信任问题。请把 agent 读取的一切都列出来：这一点 Eitri 没法替你知道。
- **`auto_mode`** 可选：agent 某个模式的 id。见[模式](#modes-and-the-agents-own-settings)。

任何其他键、或者不对的值，都会让 Eitri 在启动时停下并点出这个键，而且在调用外面包一层 `pcall` 也藏不住它。之后 `:tabnew myagent` 就打开一个它的标签页，`agent.default` 也可以写它的名字；`agent.default` 是在整个文件跑完之后才读的，所以它可以写在 `register` 调用的前面。

<a id="trust-an-acp-agent-starts-only-after-a-yes"></a>
### 信任：ACP agent 只在你说了 yes 之后才启动

ACP agent 会自己从项目里读它的配置，而 Eitri 没有办法让它不读。所以对注册的 agent，这个问题的运作方式不同：

- 这个问题涵盖你在 `project_files` 里列出的文件，在项目目录以及它之上直到所在 git 仓库顶层的每一层目录里查找。列出的目录会被整个读取。
- **即使这些文件一个也不存在，也要问**：“myagent loads whatever project files it finds; there are none now. Start it here?”（用你的 agent 的名字）。`y` 会针对这个空的状态被记住，所以之后出现一个列出的文件，就会再问。
- **`y` 启动 agent；`n` 不启动。** `n` 不会被记住，标签页会说这个 agent 会加载这个项目自己的配置、而你没有信任它，所以 Eitri 没法在不加载它的情况下启动。页脚写的是 “y trusts and starts it · n does not start it · Esc puts this off”。
- Eitri 分辨不出 agent 实际会跟随列出的哪些文件，所以列出的文件或目录如果是符号链接，或者列出的目录里任何地方有链接，这个回答就只管一次启动。
- 这个问题和被记住的回答只属于那个 agent。一个名字已经不再注册的标签页，会在没有任何文件的情况下被问一次，然后被拒绝：“was not started: no agent of that name is registered in init.lua”。

Eitri 用你当前的环境原样启动 agent。如果一个 agent 把环境变量里的相对路径当作它的配置目录，它就会把项目的目录当成你的目录来读，而对于一个通用的 agent，Eitri 对此什么也不做。（Codex 标签页正是出于这个原因拒绝相对的 `CODEX_HOME`。）

<a id="modes-and-the-agents-own-settings"></a>
### 模式与 agent 自己的设置

Eitri 不向 agent 提供文件或终端方法，也从不调用它的认证步骤。它对 agent 只设一件事：注册里有 `auto_mode` 时，这个模式 id 会在每个会话开始时设上，两种 Eitri 模式下都一样。之后 Eitri 会拿 agent 报告的每一次模式去和它对照：设置被拒绝，或者有差异，都会让标签页带着一条不一致的消息结束，之后什么也不再回答。没有 `auto_mode` 时，Eitri 什么也不设、什么也不检查，`prefix i` 的 `runs without asking` 一行会说 Eitri 不知道这个 agent 不经询问会运行什么。

Eitri 的 Auto 和 Bypass 是 Eitri 自己的：它们立即决定 Eitri 怎样回答 agent 的请求，并且从不改变 agent 的模式。`mode lag` 一行说的就是没有这种延迟。

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

ACP agent 可能发来的计划、命令列表、会话标题和其他更新都不显示；这类标签页没有速率限制、没有上下文条、没有斜杠命令列表，也没有模型选择器。agent 报告上下文有多满时，`prefix i` 有一行 `context`，agent 报告之后它显示 agent 的上下文有多满。token 不带费用。

<a id="known-limits"></a>
## 已知限制

- **agent 不经询问运行什么，是 agent 自己的事。** 对 Codex，清单在[Codex 不经询问做什么](#what-codex-does-without-asking)下。对 ACP agent，Eitri 会显示它收到的每个请求，但 agent 因为自己的模式已经允许某个动作而根本没发出的请求，永远不会到你这里。
- **agent 自己的配置总会加载**，Eitri 没办法关掉它。信任一个项目，只是把项目的配置加在它上面。
- **你的钩子可能抢先回答。** 你自己的 Codex 配置里的 `PermissionRequest` 钩子，可能在 Eitri 看到请求之前就回答了它。钩子能否在你批准之后改写命令，还没有测过。
- **Codex 的沙箱能读到你能读到的任何地方。** 它不是保护机密的边界。
- **这些 agent 没有 Eitri 一侧的分类器或保存的规则。** Claude 标签页的权限检查是 Claude 独有的。
- **这些标签页是新的。** 这里描述的大部分内容是拿替身试过的，并没有在每一种环境上试过；[已知问题](../known-issues.zh-CN.md#codex-and-acp-tabs)列出了还没解决的。
