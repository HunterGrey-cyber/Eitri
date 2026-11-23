[English](terminal-claude.md) | 简体中文
<!-- translated-from: terminal-claude.md sha256=8d2e02e16f5d618a71cc80ef9153bf8d1f14e09abbda0d1821c383158dd6c2fb -->

# 终端里的 `claude`

如果你在 Eitri 的底部终端里运行 Claude Code，Eitri 可以扮演编辑器扩展为内置终端里的 `claude` 所扮演的角色：这个 `claude` 问你要不要做的修改，也会作为 diff 在你的 Neovim 里打开；你的语言服务器持有的诊断会交给它；你的选区和 `@` 提及也会一并送给它。这项功能**默认关闭**，在你打开它之前，本页说的事一件也不会发生。

这个 `claude` 不是 Eitri 的标签页。它在 agent 面板里没有行，没有 Eitri 的权限卡片、模式或已保存的规则，也没有回合审阅；它保留自己的提示、模式和配置。Eitri 只是给它一条通向你的编辑器的路。

这是按 Claude Code 2.1.311 的协议做的。它还没有和交互式的 `claude` 一起试过，所以请当作未经试用：这里没有任何一句话保证你的 `claude` 会连上或按所述工作。

## 打开它

在 `$XDG_CONFIG_HOME/eitri/init.lua`（默认 `~/.config/eitri/init.lua`）里：

```lua
eitri.config.set("terminal.claude_ide", true)
```

取值是 `true` / `"on"` 或 `false` / `"off"`，默认关闭。它在持有编辑器的窗口启动时读取一次，所以修改后要重启那个窗口。第二个窗口跟随第一个窗口；[伴随窗口](companion.zh-CN.md)忽略这个键，因为它没有终端。见[配置](configuration.zh-CN.md)。

## 它做什么

键打开、窗口开着的时候，Eitri 会：

- 只在 `127.0.0.1` 上监听，端口由系统挑选，只接受出示随机令牌的客户端，这个令牌每次窗口启动都是新的；
- 为这个监听写下 `~/.claude/ide/<port>.lock`，只有你自己能读，窗口结束时删除。它还会删掉崩溃后遗留的、属于它自己的过期锁文件，`~/.claude` 里的其他东西一概不碰；
- 在两个窗口的底部终端的 shell 里，把 `CLAUDE_CODE_SSE_PORT` 设为这个端口。从这样的 shell 里、或从它里面启动的任何东西里启动的 `claude`，按这个端口找到锁文件并连上。

键关闭时，Eitri 一样都不做：没有监听，没有锁文件，任何终端的环境里也不会多出东西。

`claude` 连上之后：

- **Diff。** 当它问你要不要做某个修改时，文件和它提议的版本会并排在你的窗口旁边打开，不抢你的光标，也不改变你所在的模式。你可以编辑提议的版本。在提议里按 `<localleader>a`（没改过 local leader 就是 `\a`）、`:w` 或运行 `:EitriDiffAccept` 表示接受，`<localleader>d` 或 `:EitriDiffReject` 表示拒绝。这两个命令只存在于视图的两个缓冲区里，供你自己映射。拒绝会让这个 `claude` 停下当前回合。接受一份和文件相同的文本也一样，它会把这当作拒绝；出现这种情况时 winbar 会提醒你。`<localleader>q`、`:q` 或关闭任一窗口表示离开这个视图而不作答，之后由终端里的提示决定。Eitri 自己不写任何文件：接受只是把文本交还给 `claude`，由它自己写。只有项目内的文件才会这样打开。
- **诊断。** `claude` 可以读取你的编辑器已加载的、项目内文件的诊断。
- **选区。** 对项目内的文件，你当前的选区以及光标所在的行，会随着你的移动发给它。如果缓冲区的文本是从项目外读来的，无论它现在叫什么名字，这两样都不发：比如一个通过链接打开的缓冲区，而那个链接后来被换成了普通文件。
- **`:EitriMention`。** 把光标所在的行，或一个范围（`:'<,'>EitriMention`），作为 `@` 提及发给它。这个命令没有默认按键，想要的话自己映射。

通过这条路，`claude` 能调用的东西都不能写文件、不能运行代码，也不能在你的编辑器里打开或加载文件。

## 看它有没有连上

Eitri 不为此显示任何标记。在 `claude` 里，`/ide` 会列出它能看到的 IDE。Eitri 自己的记录写在 stderr 上，所以要从终端启动它才能读到：

- `[ide-server] a client says it is pid <n>`：有客户端连上了。
- `[ide-server] refused a client: <why>`：有客户端尝试连接并被拒绝，`<why>` 说明它没通过哪项检查。每种原因在每次窗口启动后只打印一次。

如果这两行都没出现，说明没有客户端尝试过。检查那个 shell 里有没有设 `CLAUDE_CODE_SSE_PORT`，以及 `~/.claude/ide/<port>.lock` 是否存在。

## 让某个会话不连上

不是从 Eitri 终端启动的 `claude` 不会自己连上。要让 Eitri 终端里的某一个不连上，启动前在那个 shell 里设 `CLAUDE_CODE_AUTO_CONNECT_IDE=false`。

要让它连上、但把 diff 留在终端里，就把 CLI 自己的 `diffTool` 设置设为 `terminal`。这样的 `claude` 仍然会收到你的选区、文件路径和诊断。

你的用户的任何进程都能读到锁文件，并用其中的令牌连上。这样得到的权限就是上面那份清单，不多一分；这和任何本来就能读你的文件、往你的终端里打字的程序处在同样的位置。如果你不想让锁文件存在，就让这个键保持关闭。

## 限制

- 已经在运行的 tmux 服务器保留自己的环境，所以它的 shell 里没有这个端口，在 Eitri 终端里运行 `tmux` 也只是加入那个服务器。可以从 Eitri 终端里启动一个新的服务器（比如 `tmux -L eitri`），或者运行 `tmux setenv CLAUDE_CODE_SSE_PORT <port>`（端口就是锁文件名里的数字）再开一个新窗格：已经开着的窗格保持原样。端口每次窗口启动都会变，所以重启后要重新设。
- 比 Eitri 活得更久的 shell、tmux 服务器或 `nohup` 任务保留旧端口。窗口重启后，那里的 `claude` 到不了新的 Eitri；请从新的 shell 里启动它。
- 端口属于第一个窗口。第二个窗口还开着而你关掉了第一个窗口时，已连上的 `claude` 会失去连接，第二个窗口之后新开的 shell 也没有端口。
- 在[不算项目边界的文件夹](permissions.zh-CN.md#a-folder-that-is-no-boundary)里，`claude` 仍然会连上，但没有任何东西算在项目内：diff 全都留在终端里，它收不到选区和诊断，`:EitriMention` 会说文件在项目外。请改在项目文件夹里启动 Eitri。
- 如果 Eitri 做不出锁文件，它不启动监听，不设端口，并提示一次。这发生在 `HOME` 不是绝对路径时，以及 `~/.claude` 或 `~/.claude/ide` 不是你私有的目录时：它是符号链接（有些 dotfile 管理器会这样做）、是别的用户的目录，或者同组用户或其他人可写。这些情况的提示都是 `~/.claude/ide is not a private directory of yours`。把链接换成真正的目录，或者运行 `chmod go-w ~/.claude ~/.claude/ide`，然后重启窗口。
- 如果系统给不出随机字节或端口，同样没有监听，也没有端口。这时只有 stderr 上的一行说明：`[ide-server] no listener: <why>`。
- 其他版本的 `claude` CLI 可能和 2.1.311 表现不同。
