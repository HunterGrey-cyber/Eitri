[English](permissions.md) | 简体中文
<!-- translated-from: permissions.md sha256=216313e4a0f07962520e14ddf01781a39373a3cdc7386794694914a55f205c2c -->

# 权限与信任

这一页说明一次工具调用在运行之前会怎样、由谁来回答、一条保存的规则能做什么和不能做什么、Claude Code 自己还可能问什么，以及 Eitri 怎样决定是否加载项目自己的 Claude Code 配置。它描述的是 Eitri 今天实际做的事，包括它比你想象的要弱的地方；这些都汇集在[已知限制](#known-limits)里。

怎样用键盘回答一张卡片，见[快速上手](getting-started.zh-CN.md)和[按键](keys.zh-CN.md#answering-a-card)。

## 谁来决定一次工具调用

Claude Code 想运行的每一次工具调用，在运行之前都会先发给 Eitri，两种模式下都是如此。接下来发生什么，取决于标签页的模式（`Shift+Tab` 切换它）以及 Claude Code 是怎样在运行的。

<a id="auto-when-claude-code-runs-its-own-auto-mode"></a>
### Auto，在 Claude Code 运行它自己的 auto 模式时

这是正常情况。Eitri 请 Claude Code 运行它自己的 auto 模式，然后让它来判断这些调用：

- 你的[保存的规则](#saved-rules)允许的调用会被立即批准，工具那一行会说是哪条规则做的。
- 其他每一次调用都交给 Claude Code 的 auto 模式，由它用自己的分类器来决定。Eitri 不为它画卡片，也不应用自己的检查。
- Claude Code 拒绝一次调用时，工具那一行会写 `blocked by auto: <原因>`。多次被拒绝之后，Claude Code 会自己用一张卡片来问你。

<a id="auto-when-claude-code-is-not-running-its-auto-mode"></a>
### Auto，在 Claude Code 没有运行它的 auto 模式时

Claude Code 可能回退到它普通的默认模式，而且某些已安装的环境可能根本提供不了 auto 模式。这时由 Eitri 自己的规则决定哪些调用需要卡片。这些规则是刻意保守的：凡是 Eitri 不能确定的，都是一张卡片。

- 在项目内部读取和搜索，不经询问直接运行；由只读取东西的程序加上普通参数组成的命令也是。带重定向、管道、`;` 或 `&&`、通配符、引号、`$(...)`、项目之外的路径，或者把 `git` 指向另一个仓库的选项的命令，会出卡片。
- 在项目内部编辑或写入文件，不经询问直接运行，但 Claude Code 保护的文件和目录除外：`.git`、`.claude`、`.vscode`、`.idea`、`.husky`、`.cargo`、`.devcontainer`、`.yarn` 或 `.mvn` 下的任何东西，以及 `.gitconfig`、`.bashrc`、`.zshrc`、`.profile`、`.envrc`、`.npmrc` 和 `.mcp.json` 这样的文件。这些会出卡片。
- `WebFetch`、`WebSearch` 和 `MultiEdit` 一律询问。Eitri 不认识的工具会询问。少数不改任何文件、不运行任何命令、也不访问任何网络的工具从不询问。
- 项目目录如果就是你的主目录，或者在它之上，就不算边界：每一次必须对照项目来判断的调用都会询问。
- 一条[保存的规则](#saved-rules)可以允许一个只是没通过“已知是只读的”检验的命令。

### Bypass

在 bypass 下，Eitri 自己批准每一次调用，不出卡片。因为这意味着很大的信任，`Shift+Tab` 进入 bypass 时会先问。这个问题是一个 `y/n`，会说明还有什么会变：如果标签页里有卡片在等，它写的是 “Switch to bypass and approve the N waiting cards?”。只有单独按下、并且在问题出现片刻之后按的 `y` 才算数，而且只有此刻仍在等待的卡片会被批准。离开 bypass 不需要确认。

Bypass 并不回答一切。Claude Code 自己提出的、只有人才能回答的问题，在 bypass 下仍然是卡片；它们是[下一节](#when-claude-code-itself-asks)的主题。有这样的卡片在等时，bypass 的问题会写 “N cards only you can answer stay waiting after the switch”。

要不经提问就以 bypass 开始，只能事先说好，办法是在你的 `init.lua` 里写 `agent.default_mode = "bypass"`；见[一次启动怎样开始](configuration.zh-CN.md#how-a-launch-starts)。

<a id="what-claude-code-itself-runs-in"></a>
### Claude Code 自己运行在什么模式下

在 Eitri 之下，Claude Code 从不运行在它自己的 bypass 模式里：它运行在它的默认模式或 auto 模式里，Eitri 站在它前面，而“bypass”只是 Eitri 替你回答。Eitri 还会检查 Claude Code 报告的是哪种模式。如果一个会话报告的模式是 Eitri 没有要求、也不预期的，比如 Claude Code 自己的 bypass 模式，Eitri 会带着一条消息结束那个会话，并且在其中不再回答任何东西。

<a id="saved-rules"></a>
## 保存的规则

一个普通 shell 命令的权限卡片可以带一个第三个按钮，“Always allow `git log *` in this project”。按下它会保存一条规则并回答这张卡片。这个按钮属于 Eitri 自己的检查所画的卡片。Claude Code 运行它的 auto 模式时（这是通常的情况），Eitri 会把大多数调用交给那个模式，而不是画卡片，所以你可能很少甚至从来看不到这个按钮；规则主要来自 Claude Code 运行在默认模式下的会话，或者来自手工编辑规则文件。

- **规则长什么样。** 它按 Claude Code 的写法来写，`Bash(git log *)`，并且匹配完整的开头单词：`cargo test *` 允许 `cargo test --release`，但不允许 `cargo testx`。只有 `Bash` 有规则；其他任何工具都没有。
- **按钮提供什么。** 命令的第一个单词，再加上第二个单词（当它是个普通单词时），所以 `git log --oneline` 提供 `git log *`，`uname -a` 提供 `uname *`。当一个选项出现在后面的普通单词之前（`git -C sub log`）时，什么都不提供，因为只用第一个单词会覆盖每一个子命令。按钮只出现在规则能起作用的地方：对于只因为它的第一个单词（或它的 `git` 子命令）不是已知只读的才询问的命令。因为其他任何原因而询问的命令没有第三个按钮。
- **永远不能成为规则的东西。** 第一个单词是一个运行另一个程序的程序（按名字或路径匹配，`/usr/bin/timeout` 也算），或者第一个单词里含有 `=`（`LC_ALL=C cargo test`，shell 把它读作一个赋值再加上真正的程序）的规则。规则文件里这样的一行会被跳过。这些程序是：

  ```text
  bash busybox chroot chrt command dash doas env eval exec fish flock ionice ltrace nice nohup
  runuser setsid sh stdbuf strace su sudo taskset time timeout unbuffer watch xargs zsh
  ```

- **规则从不让 Eitri 读不清楚的命令运行起来。** 规则只替换“这个程序不在只读名单上”这一个判断。带重定向、管道、`;` 或 `&&`、通配符、引号或 `$(...)`、项目之外的路径，或者 `git -C` 之类选项的命令，即使规则的单词和它匹配，也不会被规则放行。在 Claude Code 运行它的 auto 模式的地方（通常情况），这样的调用交给那个模式的分类器，它可能不出任何卡片就运行它；否则它是一张卡片。
- **规则覆盖的比按钮提供的更多。** 按钮的谨慎只决定它建议什么。一条保存的规则会允许每一个以它的单词开头、能被读清楚的命令，包括按钮根本不会为之提供规则的命令：保存了 `cargo *` 之后，`cargo -p helper run` 也会不出卡片就运行。保存或写下能满足需要的最窄的规则。
- **规则在哪里适用。** 在 Auto 标签页里，上面两种情形都适用。在 bypass 下不需要规则。规则从不回答 Claude Code 自己的问题（下一节）。
- **它们存在哪里。** 在项目之外，每个项目一个文件，位于 `~/.local/state/eitri/permissions/` 下（设置了 `$XDG_STATE_HOME/eitri/permissions/` 时用它）。Eitri 不往项目里写任何东西，这和 Claude Code 自己的 “don't ask again” 不一样，后者会写 `.claude/settings.local.json`。要去掉一条规则，就编辑那个文件：`<prefix> i` 在 `permission rules` 那一行里显示它的路径。Eitri 用不了的文件会在名字后加上 `.unusable` 放到一边，并被当作没有规则来读，这意味着更多的卡片，绝不会更少。

<a id="when-claude-code-itself-asks"></a>
## Claude Code 自己提问时

Eitri 放行一次调用之后，Claude Code 仍可能自己再就它提问。那会作为一张卡片出现，标签会说是谁在问。下面两种问题，都绝不会被保存的规则回答，也绝不会交给 Claude Code 的分类器。

**一律是卡片，在每种模式下，bypass 也包括在内：**

- 由你的某条点名了工具的 `permissions.ask` 规则强制发出的问题。卡片会写 “your ask rule:” 和工具名，例如 `your ask rule: Write`。
- 没有给出理由的问题。卡片会写 “Claude Code asked (maybe your ask rule)”。Claude Code 只在 ask 规则是一个单独的工具名时才点出它；带模式写的规则，比如 `Bash(echo:*)`，到达 Eitri 时就是一个没有理由的问题，所以 Eitri 没法把它和其他没有解释的问题区分开，就把它显示出来。
- 这个版本的 Eitri 不认识的那一类问题。

**给出了理由的问题**（卡片写 “Claude Code asked” 并显示那句话）：在 bypass 标签页里，Eitri 替你回答它，这一行会这么说。在 Auto 标签页里，只有当你片刻之前在一张卡片上批准过同一次调用、同样的输入，并且只用一次时，Eitri 才回答它；否则它是一张卡片。因为在 Auto 标签页里通常由 Claude Code 自己的 auto 模式来决定，所以通常没有之前的批准，这样的问题在那里就是一张卡片。在 Claude Code 的 auto 模式运行的地方，它对敏感文件（`.git/`、`.claude/`）的检查根本不会询问，由它的分类器来决定；敏感文件的问题出现在 Claude Code 运行于默认模式的时候。

有一个要知道的限制：Eitri 分不清 Claude Code 自己的理由和 hook 给出的理由。你的某个 hook（或者某个被信任的项目的 hook）带着理由来询问，会被当作任何其他带理由的问题，所以在 bypass 标签页里它会被替你回答。

<a id="trusting-a-project"></a>
## 信任一个项目

一个项目可以带着它自己的 Claude Code 配置：hook、MCP server、权限规则。加载它就意味着运行仓库里写的任何东西，所以在你看过它并说“是”之前，Eitri 不会加载它。

**看哪些东西。** 在项目目录里和它之上的每个目录里，一直到它的 git 仓库顶层（而且从不高于你的主目录）：`.claude/`、`.mcp.json`、`CLAUDE.md` 和 `CLAUDE.local.md`。`.claude/worktrees/`（Claude Code 在那里保存它自己的检出）会被点名，但不读取。没有这些东西的项目立即启动，不提任何问题。

**那个问题。** 在这样的项目里第一个会话启动之前，面板会显示 “Trust this project's Claude configuration?” 以及它找到的东西，逐个文件列出：每个 hook 和它的事件及命令，每个 MCP server 和它的命令行（环境变量在前），某个设置文件设置的环境变量的名字，一个 `apiKeyHelper`，每条 `permissions.allow` 规则，每个 `additionalDirectories` 条目，每个 `CLAUDE.md`，以及其他任何设置原样。这些文字里的隐藏字符和改变方向的字符会被画成可见的转义，所以一条命令不会看起来像是别的东西。

- `y` 信任这个项目并加载它的配置。`n` 不带它启动会话。`Esc` 把这个问题推后：第一条消息回到输入框里，继续或恢复则被丢弃，并给出一条消息。`y` 和 `n` 只有单独按下、并且在问题出现片刻之后按才算数；太早敲的键会闪出 “wait a moment, then y or n”。
- **`y` 按项目记住，并且和你看到的内容严格绑定。** 这些文件的任何变化都会再问一次，并列出加了什么、去掉了什么、改了什么。这包括 agent 在 `.claude/` 下做的一次编辑，以及 Claude Code 自己的 “don't ask again”（它会写 `.claude/settings.local.json`）。
- **`n` 只对这个窗口记住**，只针对同样的内容，并且只在 Eitri 能检查所有东西时才记住（见下一条）；否则这个问题会在这个窗口的下一次启动时回来。另一个窗口自己再问。
- **Eitri 检查不了的东西，会让信任只持续一次启动。** 超过 4 MiB 的文件、读不了的文件、管道或其他特殊文件、Claude Code 读取文件的地方出现了符号链接，或者文件太多的仓库，会在问题里被点名为 “cannot be checked”，这时 `y` 只覆盖这一次启动；什么都不保存，下一次启动再问。
- 用任何其他方式启动会话，恢复标签页或继续一个对话，也要等这个回答。

**命令。** 面板命令行上的 `:trust`（在浏览模式里按 `:`；见[命令行](keys.zh-CN.md#the-command-line)）显示同样的问题，不记录任何你没看过的东西。`:untrust` 立即忘掉这个回答。两者都对之后启动的会话生效：正在运行的会话保留它已经加载的东西。

**它存在哪里。** 每个项目一个小记录，位于 `~/.local/state/eitri/trust/` 下。删掉这个目录就忘掉每一个回答。Eitri 不读终端里 `claude` 的信任回答，终端里的 `claude` 也看不到 Eitri 的，所以你在终端里信任过的项目，在这里还要再问一次。

<a id="what-an-agent-session-loads"></a>
## 一个 agent 会话会加载什么

和终端里的 `claude` 一样，每个会话都会加载你自己的 Claude Code 设置：你的 hook、插件、skill、`CLAUDE.md` 和权限规则。不想加载它们，就在 `~/.config/eitri/init.lua` 里写上：

```lua
eitri.config.set("agent.user_settings", false)    -- true (the default) or false
```

其他任何值都会让 Eitri 在启动时停下，并给出一条点出这个设置的消息。

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
- **在 Auto 标签页里，Claude Code 的 auto 模式可以不询问就写入 `.git/` 下**，新的 git hook 也包括在内。回合审阅不显示 `.git/` 下的改动，信任问题盯着 `.claude/`、`.mcp.json` 和 `CLAUDE.md`，不盯 `.git/`。在一个你在意的仓库里完成一个回合之后，值得看一眼 `ls .git/hooks`。对 `.claude/` 下的编辑，Eitri 确实会在下一个会话之前再问一次信任问题。
- **被信任的项目自己的 `permissions.allow` 规则，只有在你也在终端 `claude` 里信任了那个目录时，才会到达 Auto 标签页。** 这是 Claude Code 自己的规则。你的用户设置里的规则始终适用。
- **信任一个项目，就意味着信任它的代码会运行。** hook 从 `.claude/` 之外调用的脚本，只通过 hook 的命令文本被覆盖，就像构建这个项目一样。
- **理由不是出处的证明。** 在 bypass 下，带理由的问题会被替你回答，而 Eitri 分不清 Claude Code 自己的检查和一个带着理由来询问的 hook。
