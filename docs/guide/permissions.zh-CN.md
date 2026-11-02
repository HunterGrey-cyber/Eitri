[English](permissions.md) | 简体中文
<!-- translated-from: permissions.md sha256=33c397eb4ba154a671a39af0edfbbbac689750f3ae883188a55a35b7518a474a -->

# 权限与信任

这一页说明一次工具调用在运行之前会怎样、由谁来回答、一条保存的规则能做什么和不能做什么、Claude Code 自己还可能问什么，以及 Eitri 怎样决定是否加载项目自己的 Claude Code 配置。它描述的是 Eitri 今天实际做的事，包括它比你想象的要弱的地方；这些都汇集在[已知限制](#known-limits)里。

怎样用键盘回答一张卡片，见[快速上手](getting-started.zh-CN.md)和[按键](keys.zh-CN.md#answering-a-card)。

这一页讲的是 Claude 标签页。运行 Codex 或你注册的 agent 的标签页有它自己的沙箱、审查者和信任问题，它们在 [Codex 与其他 agent](agents.zh-CN.md) 里描述。

agent 的[编辑器工具](editor-tools.zh-CN.md)（`mcp__eitri__open_file` 等）在每一种标签页里都不受这些约束：保存的规则、下面 Manual 和 Auto 的规则、Claude Code 的 auto 模式都不判断它们。Eitri 给它们的表[在它们自己那一页](editor-tools.zh-CN.md#cards)，它们的卡片有哪些不同也写在那里。

## 谁来决定一次工具调用

Claude Code 想运行的每一次工具调用，在运行之前都会先发给 Eitri，每种模式下都是如此。接下来发生什么，取决于标签页的模式以及 Claude Code 是怎样在运行的。一个标签页处在三种模式之一，`Shift+Tab`（或 `<leader>m`）按 Claude Code 的顺序轮换它们：manual、auto、bypass，然后再回到 manual。新标签页以 auto 开始，除非你设置了 `agent.default_mode`（[一次启动怎样开始](configuration.zh-CN.md#how-a-launch-starts)）。

<a id="manual"></a>
### Manual

manual 是最严格的模式。不管 Claude Code 在运行什么，每一次调用都由 Eitri 自己的规则来回答：也就是[Auto，在 Claude Code 没有运行它的 auto 模式时](#auto-when-claude-code-is-not-running-its-auto-mode)里描述的那些规则，只有一处不同：**用 Claude Code 的文件工具（`Write`、`Edit`、`NotebookEdit`）做的每一次文件编辑或写入都是一张卡片**，在项目内部也一样。没有任何东西交给 Claude Code 的 auto 模式，所以不会有哪一行写 `blocked by auto`。在项目内部读取和搜索、只读命令仍然不经询问直接运行，你的[保存的规则](#saved-rules)仍然适用（会写文件的 shell 命令和在 auto 下一样按它的文字判断，所以允许它的规则照样放行），Claude Code 自己的问题按 auto 下的方式处理（[Claude Code 自己提问时](#when-claude-code-itself-asks)）。

以 Eitri 自己的规则来衡量，manual 绝不比 auto 更宽松：它只是把这些规则放行的一部分变成卡片。但在 Claude Code 运行它自己的 auto 模式的地方，Auto 标签页并不使用这些规则：凡是没有保存的规则放行的调用，它都交给 Claude Code 的分类器，而分类器可能拒绝。Manual 标签页自己回答这些调用，所以在项目内部读取、只读命令、子代理（`Task`、`Agent`）、`ToolSearch` 和 `TodoWrite` 在 manual 下运行时不经过 Claude Code 的分类器（Eitri 的批准会跳过它），而在 auto 下那个分类器本可能拦下其中一个。其余凡是 Eitri 的规则会放到卡片上的，在 manual 下也是卡片。

Claude Code 的定时和任务工具在任何模式下都不出卡片，因为 Claude Code 自己运行它们时也不询问：`ScheduleWakeup`、`CronCreate`、`CronList`、`CronDelete`、`ListAgents` 和 `TaskStop`。它们安排的事，只要标签页还开着，之后会作为一个单独的回合出现；关掉标签页就取消了，只有被要求跨会话保留的 cron 任务例外：Claude Code 会把它存在项目里，之后在这个项目的另一个会话里可能还会运行（见[Codex 与其他 agent](agents.zh-CN.md#not-in-a-claude-tab-yet)）。`SendMessage` 会把消息送到另一个会话，凡是由 Eitri 的规则来决定的地方，它仍然是一张卡片。

<a id="auto-when-claude-code-runs-its-own-auto-mode"></a>
### Auto，在 Claude Code 运行它自己的 auto 模式时

这是正常情况。Eitri 请 Claude Code 运行它自己的 auto 模式，然后让它来判断这些调用：

- 你的[保存的规则](#saved-rules)允许的调用会被立即批准，工具那一行会说是哪条规则做的。
- 其他每一次调用都交给 Claude Code 的 auto 模式，由它用自己的分类器来决定。Eitri 不为它画卡片，也不应用自己的检查。
  例外是[不算边界](#a-folder-that-is-no-boundary)的项目：在那里由 Eitri 自己的规则来回答，和 Manual 标签页一样。
- Claude Code 拒绝一次调用时，工具那一行会写 `blocked by auto: <原因>`。多次被拒绝之后，Claude Code 会自己用一张卡片来问你。

<a id="auto-when-claude-code-is-not-running-its-auto-mode"></a>
### Auto，在 Claude Code 没有运行它的 auto 模式时

选择不能运行 auto 模式的模型，会让这个会话的 auto 模式结束；之后再换模型，Claude Code 也不会回到 auto。标签页在那一刻转到 manual 并说明原因，回合进行到一半时也一样。你可以把它切回 auto，在新会话开始之前，这时的 auto 指的是下面这些规则；把标签页移到另一个窗口会让它回到 manual。

Claude Code 可能回退到它普通的默认模式，而且某些已安装的环境可能根本提供不了 auto 模式。这时由 Eitri 自己的规则决定哪些调用需要卡片。这些规则是刻意保守的：凡是 Eitri 不能确定的，都是一张卡片。

- 在项目内部读取和搜索，不经询问直接运行；由只读取东西的程序加上普通参数组成的命令也是。带重定向、管道、`;` 或 `&&`、通配符、引号、`$(...)`、项目之外的路径，或者把 `git` 指向另一个仓库的选项的命令，会出卡片。
- 在项目内部编辑或写入文件，不经询问直接运行，但 Claude Code 保护的文件和目录除外：`.git`、`.claude`、`.vscode`、`.idea`、`.husky`、`.cargo`、`.devcontainer`、`.yarn` 或 `.mvn` 下的任何东西，以及 `.gitconfig`、`.bashrc`、`.zshrc`、`.profile`、`.envrc`、`.npmrc` 和 `.mcp.json` 这样的文件。这些会出卡片。
- `WebFetch`、`WebSearch` 和 `MultiEdit` 一律询问。Eitri 不认识的工具会询问。少数不改任何文件、不运行任何命令、也不访问任何网络的工具从不询问。
- 不算边界的项目目录（你的主目录或它之上，以及[不算边界的文件夹](#a-folder-that-is-no-boundary)里列出的其他情况），会让每一次必须对照项目来判断的调用都询问。
- 一条[保存的规则](#saved-rules)可以允许一个只是没通过“已知是只读的”检验的命令。

### Bypass

在 bypass 下，Eitri 自己批准每一次调用，不出卡片。这也包括 Claude Code 自己提出的问题（[下一节](#when-claude-code-itself-asks)），连你自己的 `permissions.ask` 规则强制发出的问题也在内：在 bypass 标签页里，ask 规则什么也拦不住。唯一的例外是 [`nvim_exec`](editor-tools.zh-CN.md#nvim-exec)，一个你打开之前一直关着的编辑器工具：它的每次调用都是一张卡片，在 bypass 下也一样。因为这意味着很大的信任，`Shift+Tab` 进入 bypass 时会先问。这个问题是一个 `y/n`，会说明还有什么会变：如果标签页里有卡片在等，它写的是 “Switch to bypass and approve the N waiting cards?”，每一张在等的卡片都算在内，但 agent 的[编辑器工具](editor-tools.zh-CN.md#cards)的卡片除外，进入 bypass 从不回答它。只有单独按下、并且在问题出现片刻之后按的 `y` 才算数，而且只有此刻仍在等待的卡片会被批准。只能从 auto 进入 bypass。离开 bypass 不需要确认，并且会落到 manual。

Claude 问你的选择题，bypass 永远不会回答（[Claude 问你的问题](#claudes-questions-to-you)）。

要不经提问就以 bypass 开始，只能事先说好，办法是在你的 `init.lua` 里写 `agent.default_mode = "bypass"`；见[一次启动怎样开始](configuration.zh-CN.md#how-a-launch-starts)。

<a id="what-claude-code-itself-runs-in"></a>
### Claude Code 自己运行在什么模式下

在 Eitri 之下，Claude Code 从不运行在它自己的 bypass 模式里：它运行在它的默认模式或 auto 模式里，Eitri 站在它前面，而“bypass”只是 Eitri 替你回答。Eitri 还会检查 Claude Code 报告的是哪种模式。如果一个会话报告的模式是 Eitri 没有要求、也不预期的，比如 Claude Code 自己的 bypass 模式，Eitri 会带着一条消息结束那个会话，并且在其中不再回答任何东西。

<a id="a-folder-that-is-no-boundary"></a>
## 不算边界的文件夹

Eitri 自己的规则拿路径去对照项目目录来判断。有些文件夹不能当这个边界，这时每一次必须对照项目来判断的调用都会询问。下面这些文件夹不算边界：

- 文件系统根、内核目录树（`/proc`、`/sys`、`/dev`），或者解析之后又被移走的文件夹；
- 你的主目录或它之上；
- 不属于你的（属于别的用户，比如 `/etc`）；
- 别的用户可写的（`/tmp` 和 `/var/tmp`，或共享的目录树）。只有你自己的组可写的文件夹仍然算，前提是那个组没有别的成员：umask 为 0002 时产生的 `drwxrwxr-x you you` 检出仍然是边界。

最常见的情形是对没有自己项目的文件用 [`eitri FILE`](getting-started.zh-CN.md#open-a-file)，比如 `eitri ~/notes.md`。它对这种文件夹上的每个窗口都生效，不管窗口是怎么启动的，所以 `eitri /tmp` 和 `eitri ~` 的表现一样。面板会用一条提示说明，并给出原因。

- **Manual 和 Auto 的 Claude 标签页会询问。** 每一次要对照项目来判断的调用都是一张卡片。原本会交给 Claude Code 自己的分类器的 Auto 标签页，在这里改由 Eitri 的规则回答，而它们会询问。
- **Bypass 不询问**，和别处一样：那是你自己的选择。
- **Codex 标签页以只读方式开始。** 沙箱里面什么也写不了，要写入必须通过 Codex 自己的批准流程请求离开沙箱。这在 Manual 和 Auto 下都是一张卡片，因为在这样的文件夹里不由 Codex 自己的审查者决定；Bypass 自己回答这个请求。不管你怎样回答信任问题，项目自己的 Codex 配置在这里都不会加载：文件夹里的规则或钩子可能不出卡片就让命令离开沙箱。你自己的 exec-policy `allow` 规则覆盖的内容仍然在沙箱之外运行，MCP 工具也仍然遵循它们所在服务器的批准模式。每个回合都会重新看一次这个文件夹。
- **注册的 agent 的标签页保持它自己的规则。** ACP 没有 Eitri 可以要求的沙箱，Eitri 也不知道一个 agent 的各个模式是什么意思。注册时可以点名一个 [`read_only_mode`](agents.zh-CN.md#other-agents-over-acp)，在这样的文件夹里，它会代替 `auto_mode` 被设上，只在开始时设一次。没有点名时，agent 的模式保持不变。

<a id="saved-rules"></a>
## 保存的规则

一个普通 shell 命令的权限卡片可以带一个第三个按钮，“Always allow `git log *` in this project”。按下它会保存一条规则并回答这张卡片。这个按钮属于 Eitri 自己的检查所画的卡片。Claude Code 运行它的 auto 模式时（这是通常的情况），Auto 标签页会把大多数调用交给那个模式，而不是画卡片，所以你在那里可能很少看到这个按钮；规则这时主要来自 Manual 标签页、Claude Code 运行在默认模式下的会话，或者来自手工编辑规则文件。

- **规则长什么样。** 它按 Claude Code 的写法来写，`Bash(git log *)`，并且匹配完整的开头单词：`cargo test *` 允许 `cargo test --release`，但不允许 `cargo testx`。只有 `Bash` 有规则；其他任何工具都没有。
- **按钮提供什么。** 命令的第一个单词，再加上第二个单词（当它是个普通单词时），所以 `git log --oneline` 提供 `git log *`，`uname -a` 提供 `uname *`。当一个选项出现在后面的普通单词之前（`git -C sub log`）时，什么都不提供，因为只用第一个单词会覆盖每一个子命令。按钮只出现在规则能起作用的地方：对于只因为它的第一个单词（或它的 `git` 子命令）不是已知只读的才询问的命令。因为其他任何原因而询问的命令没有第三个按钮。
- **永远不能成为规则的东西。** 第一个单词是一个运行另一个程序的程序（按名字或路径匹配，`/usr/bin/timeout` 也算），或者第一个单词里含有 `=`（`LC_ALL=C cargo test`，shell 把它读作一个赋值再加上真正的程序）的规则。规则文件里这样的一行会被跳过。这些程序是：

  ```text
  bash busybox chroot chrt command dash doas env eval exec fish flock ionice ltrace nice nohup
  runuser setsid sh stdbuf strace su sudo taskset time timeout unbuffer watch xargs zsh
  ```

- **规则从不让 Eitri 读不清楚的命令运行起来。** 规则只替换“这个程序不在只读名单上”这一个判断。带重定向、管道、`;` 或 `&&`、通配符、引号或 `$(...)`、项目之外的路径，或者 `git -C` 之类选项的命令，即使规则的单词和它匹配，也不会被规则放行。在 Claude Code 运行它的 auto 模式的地方（通常情况），这样的调用交给那个模式的分类器，它可能不出任何卡片就运行它；否则它是一张卡片。
- **规则覆盖的比按钮提供的更多。** 按钮的谨慎只决定它建议什么。一条保存的规则会允许每一个以它的单词开头、能被读清楚的命令，包括按钮根本不会为之提供规则的命令：保存了 `cargo *` 之后，`cargo -p helper run` 也会不出卡片就运行。保存或写下能满足需要的最窄的规则。
- **规则在哪里适用。** 在 Manual 标签页里，以及在 Auto 标签页里上面两种情形下都适用。在 bypass 下不需要规则。规则从不回答 Claude Code 自己的问题（下一节）。
- **它们存在哪里。** 在项目之外，每个项目一个文件，位于 `~/.local/state/eitri/permissions/` 下（设置了 `$XDG_STATE_HOME/eitri/permissions/` 时用它）。Eitri 不往项目里写任何东西，这和 Claude Code 自己的 “don't ask again” 不一样，后者会写 `.claude/settings.local.json`。要去掉一条规则，就编辑那个文件：`<prefix> i` 在 `permission rules` 那一行里显示它的路径。窗口只在启动时读一次这个文件，所以已经打开的窗口里，被删掉的规则仍然有效，直到你重启那个窗口。Eitri 用不了的文件会在名字后加上 `.unusable` 放到一边，并被当作没有规则来读，这意味着更多的卡片，绝不会更少。

<a id="when-claude-code-itself-asks"></a>
## Claude Code 自己提问时

Eitri 放行一次调用之后，Claude Code 仍可能自己再就它提问。这样的问题绝不会被保存的规则回答，也绝不会交给 Claude Code 的分类器。

**在 bypass 标签页里，每一个都由 Eitri 替你回答**，不管它有没有说明为什么要问；工具那一行会在问题的标签下这么写，例如 `your ask rule: Write — allowed in bypass`。进入 bypass 时，已经在等的这类问题也一并批准。

**在 Auto 或 Manual 标签页里，它会作为一张卡片出现，标签会说是谁在问。** 下面这些在那里一律是卡片：

- 由你的某条点名了工具的 `permissions.ask` 规则强制发出的问题。卡片会写 “your ask rule:” 和工具名，例如 `your ask rule: Write`。
- 没有给出理由的问题。卡片会写 “Claude Code asked (maybe your ask rule)”。Claude Code 只在 ask 规则是一个单独的工具名时才点出它；带模式写的规则，比如 `Bash(echo:*)`，到达 Eitri 时就是一个没有理由的问题，所以 Eitri 没法把它和其他没有解释的问题区分开，就把它显示出来。
- 这个版本的 Eitri 不认识的那一类问题。

**给出了理由的问题**（卡片写 “Claude Code asked” 并显示那句话）：在 Auto 或 Manual 标签页里，只有当你片刻之前在一张卡片上批准过同一次调用、同样的输入，并且只用一次时，Eitri 才回答它；否则它是一张卡片。因为在 Auto 标签页里通常由 Claude Code 自己的 auto 模式来决定，所以通常没有之前的批准，这样的问题在那里就是一张卡片；在 Manual 标签页里，你通常已经先在 Eitri 自己的卡片上批准了这次调用。在 Claude Code 的 auto 模式运行的地方，它对敏感文件（`.git/`、`.claude/`）的检查根本不会询问，由它的分类器来决定；敏感文件的问题出现在 Claude Code 运行于默认模式的时候。

有一个要知道的限制：Eitri 分不清 Claude Code 自己的理由和 hook 给出的理由。你的某个 hook（或者某个被信任的项目的 hook）带着理由来询问，会被当作任何其他带理由的问题，所以在 Auto 或 Manual 标签页里，你在卡片上对同一次调用的批准也会回答它。

<a id="a-dynamic-workflows-card"></a>
## 动态工作流的卡片

Claude Code 的 `Workflow` 工具的卡片写着 `Run a dynamic workflow?`。它以纯文本显示工作流的名字（或者它的脚本路径）、参数和脚本，`View raw script` 在 Neovim 里以只读方式打开脚本。你像对待其他卡片一样批准或拒绝它。工作流没有“不再询问”的选项，在你批准之前，卡片也不会列出任何阶段。谁来回答这张卡片，以及 Auto 把什么交给 Claude Code，都和其他工具一样。

<a id="claudes-questions-to-you"></a>
## Claude 问你的问题

除了请求权限，Claude 还可以用选择题问你（Claude Code 的 `AskUserQuestion` 工具）。这种提问从来不是权限卡片，只有你能回答它，用的是你在提问框里按的键（[按键](keys.zh-CN.md#claudes-questions)）。bypass 不会回答它，进入 bypass 也不会；`Ctrl+y`、卡片按键 `a` 和 `d`、保存的规则以及 Claude Code 的分类器同样不会。卡片和提问同时在等时，这些键只回答卡片。

- 被[浮动窗口](agents.zh-CN.md#a-chat-under-a-float)盖住的提问框不会发出回答，和被盖住的卡片一样，你已经选的内容会保留；浮动窗口移开之后就能发出。
- 问题、选项和预览都以纯文本显示，隐藏字符和改变文字方向的字符会显示成可见的转义。预览里的 Markdown 不会显示图片、运行脚本或出现可点的控件。
- 有提问在等时关闭标签页，会先问，并说明关闭会取消这个提问。

<a id="trusting-a-project"></a>
## 信任一个项目

一个项目可以带着它自己的 Claude Code 配置：hook、MCP server、权限规则。加载它就意味着运行仓库里写的任何东西，所以在你看过它并说“是”之前，Eitri 不会加载它。

**看哪些东西。** 在项目目录里和它之上的每个目录里，一直到它的 git 仓库顶层（而且从不高于你的主目录）：`.claude/`（整个目录，不只是设置文件）、`.mcp.json`、`CLAUDE.md` 和 `CLAUDE.local.md`。`.claude/worktrees/`（Claude Code 在那里保存它自己的检出）会被点名，但不读取。Claude Code 还会从仓库之上的各级目录（一直到根目录）加载记忆文件和 MCP server，所以这些也会被查看：`CLAUDE.md`、`CLAUDE.local.md`、`.mcp.json`、`.claude/CLAUDE.md` 和 `.claude/rules/`（主目录下的 `.claude/` 除外，那是你自己的配置）。它们以 `../` 开头的路径列出；那里属于既不是你也不是 root 的文件无法被检查（见下文）。项目子目录里的 `CLAUDE.md` 不会被查看，问题里也会这么说：信任之后，Claude Code 在处理那个子目录里的文件时会读取它。没有这些东西的项目立即启动，不提任何问题。

**那个问题。** 在这样的项目里第一个会话启动之前，面板会显示 “Trust this project's Claude configuration?” 以及它找到的东西，逐个文件列出：

- 每个 hook，带它的事件、matcher 和命令（以及该 handler 的其他字段）；
- 每个 MCP server，带它的命令行（环境变量在前）和其他字段；
- 设置文件设置的每个环境变量，写成 `NAME=value`；
- `apiKeyHelper`、每条 `permissions.allow` 规则（并注明 Claude Code 只有在它自己也信任这个文件夹时才会应用这样的规则）、每个 `additionalDirectories` 条目和每个 `CLAUDE.md`；
- 其他任何设置，原样显示；
- `.claude/` 下其余每个文件，按名字、所在目录和大小列出；如果某个 Markdown 文件在 frontmatter 里设置了 `hooks`、`allowed-tools`、`permissionMode` 或 `mcpServers`，会加一条注明。

**问题里的任何内容都不会被截断。** `y` 批准的就是方框里完整显示的东西，不管某条 hook 命令有多长。这些文字里的隐藏字符和改变方向的字符会被画成可见的转义，所以一条命令不会看起来像是别的东西。

如果一个配置的提问会超过 2 MiB，就没法完整显示，Eitri 也不会信任任何没看过的东西。对 Claude 标签页，面板这时只问一个简短的问题，只接受 `n`（不带项目配置启动）或 `Esc`，`y` 会被拒绝。`:trust` 会说明配置太大、没法显示，并且什么也不信任。这样的项目在 Eitri 里根本没法被信任。

- `y` 信任这个项目并加载它的配置，但[不算边界的文件夹](#a-folder-that-is-no-boundary)里的 Codex 标签页除外，它仍然不带这份配置启动。`n` 不带它启动会话。`Esc` 把这个问题推后：第一条消息回到输入框里，继续一个会话则被丢弃，并给出一条 “not started: the trust question was put off” 的消息。以这种方式推后的恢复会保留它的会话：这些标签页会作为失败的标签页回来，仍然保存着各自的对话（在标签页里按 `r` 再次启动它），而你从空标签页的 `Restore last session` 那一行发起的恢复，只要那个标签页仍是空的，就仍然留在那里供你再选（否则它的标签页同样作为失败的标签页回来）。`y` 和 `n` 只有单独按下、并且在问题出现片刻之后按才算数；太早敲的键会闪出 “wait a moment, then y or n”。
- **`y` 按项目记住，并且和你看到的内容严格绑定。** 这些文件的任何变化都会再问一次，并列出加了什么、去掉了什么、改了什么。这包括 agent 在 `.claude/` 下做的一次编辑，以及 Claude Code 自己的 “don't ask again”（它会写 `.claude/settings.local.json`）。它还和问题当时显示了多少内容绑定：某个版本让问题显示得比以前更多时，你信任过的每个项目都会再被问一次，即使项目里什么都没变。
- **指令文件的变化以 diff 显示。** 当 `CLAUDE.md`、`CLAUDE.local.md`、`.claude/CLAUDE.md`、`.claude/rules/` 下的 Markdown 文件、`AGENTS.md` 或已注册 agent 指定的文件和你上次信任时不同，问题会在所列内容的最后显示改动，没变的行折叠起来。Eitri 比较的是你上次按 `y` 时留下的副本。没有副本可比（之后才新增，或你上次是在旧版本上回答的）、当时或现在超过 256 KiB、超出每个项目保留的 1 MiB、不是文本、或经由链接读到的文件，都只有一行说明。所有文件加起来最多画 2,000 行，改动小的先画，所以大改动挤不掉小改动：没画出来的文件会写 `changed, not drawn` 和增删的行数，本身就太长的文件也会说明。接近问题本身的 2 MiB 上限时，diff 会退成这些说明行，到了上限这一节就不显示。无论哪种情况，问的都是同一个问题。hook、MCP 服务器和设置总是完整显示，所以不需要 diff。
- **`n` 只对这个窗口记住**，只针对同样的内容，并且只在 Eitri 能检查所有东西时才记住（见下一条）；否则这个问题会在这个窗口的下一次启动时回来。另一个窗口自己再问。
- **Eitri 检查不了的东西，会让信任只持续一次启动。** 超过 4 MiB 的文件、读不了的文件、管道或其他特殊文件、不是合法 JSON 的设置文件或 `.mcp.json`（“could not be parsed for this check”）、Eitri 跟不过去的符号链接（顶替了某个配置文件，或在 `.claude/` 下）、仓库之上那些既不属于你也不属于 root 的文件，或者文件太多的项目，会在问题里被点名为 “cannot be checked”，这时 `y` 只覆盖这一次启动；什么都不保存，下一次启动再问。在 Claude 标签页里，`.claude/` 下的符号链接，或顶替 `CLAUDE.md`、`CLAUDE.local.md` 的符号链接，如果用相对路径指向仓库内部（项目不在仓库里时，指项目文件夹内部），Eitri 会跟过去；Codex 标签页里 `.codex/skills/` 内的符号链接也一样。Eitri 检查它指向的内容，并连同目标一起显示，所以在 `.claude/skills/` 和 `.codex/skills/` 之间链接的 skill、链接到 `AGENTS.md` 的 `CLAUDE.md` 都和其他文件一样会被记住。离开这个范围的链接、绝对路径和循环链接都检查不了。如果你的主目录在仓库里面，或者项目之上有 `.git` 但都无法验证，任何链接都不会被跟随。
- **非常大的配置只对这个窗口记住。** 如果一次回答的记录会超过 1 MiB（大约 1,900 个长文件名的文件），问题里会这么说，`y` 只在这个窗口里有效；不写任何记录，所以新窗口会再问。
- 用任何其他方式启动会话，恢复标签页或继续一个对话，也要等这个回答。

**命令。** 面板命令行上的 `:trust`（在浏览模式里按 `:`；见[命令行](keys.zh-CN.md#the-command-line)）显示同样的问题，在你回答之前不记录任何东西。`:untrust` 立即忘掉这个回答，当前窗口里的也一样。两者都对之后启动的会话生效：正在运行的会话保留它已经加载的东西。

**它存在哪里。** 每个项目一个小记录，位于 `~/.local/state/eitri/trust/` 下（`$XDG_STATE_HOME` 设为绝对路径时用 `$XDG_STATE_HOME/eitri/trust/`），旁边还有一份你上次信任时的指令文件副本，包括 `CLAUDE.local.md`：每个项目每个 agent 最多 1 MiB，通常和这些文件差不多大，只有你能读，不会发送到任何地方。`:untrust` 会把两者都删掉。删掉这个目录，之后启动的窗口就忘掉每一个回答。已经拿到你回答的窗口会继续用它，直到你运行 `:untrust` 或重启它。Eitri 不读终端里 `claude` 的信任回答，终端里的 `claude` 也看不到 Eitri 的，所以你在终端里信任过的项目，在这里还要再问一次。

<a id="what-an-agent-session-loads"></a>
## 一个 agent 会话会加载什么

和终端里的 `claude` 一样，每个会话都会加载你自己的 Claude Code 设置：你的 hook、插件、skill、`CLAUDE.md` 和权限规则。不想加载它们，就在 `$XDG_CONFIG_HOME/eitri/init.lua`（默认 `~/.config/eitri/init.lua`）里写上：

```lua
eitri.config.set("agent.user_settings", false)    -- true (the default) or false
```

其他任何值，Eitri 都会在启动时列出来并点出这个设置，在你改好之前不加载你自己的配置。

项目自己的配置只有在你信任它之后才加载（上一节）。所以一个会话加载什么，取决于这两个选择：

| `agent.user_settings` | 项目是否被信任 | 这个会话加载 |
|---|---|---|
| `true`（默认） | 否，或没有什么可信任的 | 只有你的用户设置 |
| `true`（默认） | 是 | 你的用户设置，然后是项目自己的 `.claude/`、`.mcp.json` 和 `CLAUDE.md` |
| `false` | 否，或没有什么可信任的 | 两边都不加载 |
| `false` | 是 | 只有项目自己的设置 |

`<prefix> i` 在一个标签页的 `settings` 和 `trust` 两行里显示这一点。一个标签页保留它的会话启动时的内容；改动对之后启动的会话生效。

项目自己的 `permissions.allow` 规则还有一个条件。Claude Code 自己会忽略它们，除非它在终端里也信任那个目录；见[已知限制](#known-limits)。

回合审阅，也就是面板在一个回合之后能显示的另一样东西，在[审阅一个回合](turn-review.zh-CN.md)里讲。

<a id="known-limits"></a>
## 已知限制

其中大多数也在[已知问题](../known-issues.zh-CN.md#安全)里，它们是上面所述的行为比听起来要弱的地方。

- **一个 hook 可以在调用被批准之后改动它。** 你自己的 Claude Code 设置里的 hook，或者被信任的项目里的 hook，可以在 Eitri 看到原始调用之后改写一次工具调用的输入。这时 Claude Code 再询问，它的卡片显示的是改写后的输入，而工具那一行显示的是原来的。在终端里 hook 也是这样工作的。
- **在 Auto 标签页里，Claude Code 的 auto 模式可以不询问就写入 `.git/` 下**，新的 git hook 也包括在内。Manual 标签页把每一次 `Write`、`Edit` 和 `NotebookEdit` 调用都放到卡片上，但不是每一次写入：shell 命令按它的文字判断，所以像 `Bash(cp *)` 这样保存的规则仍然会让 `cp x .git/hooks/pre-commit` 运行。回合审阅不显示 `.git/` 下的改动，信任问题盯着 `.claude/`、`.mcp.json` 和 `CLAUDE.md`，不盯 `.git/`。在一个你在意的仓库里完成一个回合之后，值得看一眼 `ls .git/hooks`。对 `.claude/` 下的编辑，Eitri 确实会在下一个会话之前再问一次信任问题。
- **被信任的项目自己的 `permissions.allow` 规则，只有在你也在终端 `claude` 里信任了那个目录时，才会到达 Auto 标签页。** 这是 Claude Code 自己的规则。你的用户设置里的规则始终适用。
- **信任一个项目，就意味着信任它的代码会运行。** hook 从 `.claude/` 之外调用的脚本，只通过 hook 的命令文本被覆盖，就像构建这个项目一样。
- **Bypass 也会回答你自己的 ask 规则。** 在 bypass 标签页里，`permissions.ask` 规则（你的，或者被信任的项目的）什么也拦不住：它引出的问题，Eitri 会像对待其他问题一样替你回答。
- **另一个 Claude Code 会话发来的消息不会显示。** 回合进行中另一个会话给这个会话发消息时，Claude Code 会把它送进这个回合，Claude 也会读到，但标签页里没有对应的一行，实时不显示，恢复之后也不显示。
- **理由不是出处的证明。** 在 Auto 或 Manual 标签页里，带理由的问题会凭你对同一次调用的批准被回答，而 Eitri 分不清 Claude Code 自己的检查和一个带着理由来询问的 hook。
