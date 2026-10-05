[English](known-issues.md) | 简体中文
<!-- translated-from: known-issues.md sha256=02b2565e4d77a00385acb9176df5bf1dd54d618ad4321004042efa038ba7b1b2 -->

# 已知问题与限制

当前 0.x 版本里不支持的、还比较粗糙的、以及还没有测过的部分。提 bug 之前请先看一遍；如果这里写的有错，或者已经变了，也请告诉我们。环境要求在 [eitri.cc](https://eitri.cc/zh/#requirements) 上也有；安装见 [INSTALL.zh-CN.md](../INSTALL.zh-CN.md)。

## 能在哪里运行

- **x86_64 的 Linux，Wayland。** X11 没有测试过（不过欢迎来自 X11 的反馈）。macOS：移植中。Windows：不支持，WSL2 配合 WSLg 也许能用，但没有测试过。目前还没有 ARM 版本。
- **在 GNOME 和 sway 上测试过。** 其他桌面和合成器，包括 KDE Plasma 和 Hyprland，没有测试过，欢迎反馈。
- **需要 WebKitGTK 2.40 或更新**（`webkitgtk-6.0` API）。预构建二进制按 glibc 2.39 和 GTK 4.14 构建，更旧的版本没有测试过。Ubuntu 24.04、Debian 13、Fedora 40+、RHEL 10 和 Arch 满足要求；Ubuntu 22.04 和 Debian 12 不满足。
- **Neovim 0.10 或更新**：用你自己 `PATH` 里的，或者让安装脚本为 Eitri 单独下载的一份私有副本。
- **Claude Code**，已安装并已登录，2.1.252 或更新，低于 3.0。没有它时 Eitri 照样能打开，编辑器可以用；agent 面板没法运行对话。

## 安装

- **agent sidecar 在你的机器上构建**，在安装的过程中完成（需要联网，以及约 600 MiB 可用磁盘空间；要下载 Node.js 和 npm 包）。AUR 的 `eitri-bin` 在它自己的构建过程里做同样的事，所以要几分钟，而不是几秒。原因见 [INSTALL.zh-CN.md](../INSTALL.zh-CN.md#为什么-sidecar-要在你自己的机器上构建)。
- **`.deb` 和 `.rpm` 装完后需要运行一次 `eitri setup`**，用你自己的用户身份；这两种包都不会构建 sidecar。
- **Ubuntu 23.10 及以后，包括 24.04：** AppArmor 默认会拦住 WebKit 的沙盒。Eitri 启动时会检查；编辑器和终端照常工作，agent 面板的位置会显示只需做一次的修复步骤。修复需要 `sudo`，也有代价，两者都写在 [INSTALL.zh-CN.md](../INSTALL.zh-CN.md#ubuntu-2310-及以后版本) 里。
- **`v0.2.0` 很快将无法再从 git 构建。** 0.2.0 之后，Neovide fork 的公开历史被改写了（只改了提交的元数据，文件内容没有变），所以 `v0.2.0` 这个 tag 以及之前的每一个提交，指向的都是已经没有任何分支持有的 fork 提交；请构建 0.2.1 或更新的版本。至于 0.2.0 本身：用 0.2.0 自己的 `install.sh` 加 `--from-source`，只在 GitHub 还提供那些旧对象时才能成功；用 0.2.0 发布页上的 `eitri-0.2.0-source.tar.gz` 则可以一直重新构建它；之后版本的 `install.sh` 没法从源码构建 0.2.0。安装包、tar 包和 `install.sh` 不受影响。

## 速度与绘制

- GTK 低于 4.16 时，编辑器换一种方式绘制，那里的打字延迟还没有测过。
- agent 回复流式输出时，在编辑器里打字，约 6-9 % 的按键要等两次刷新而不是一次（Intel 笔记本，GTK 4.22.5，模拟的输出流，按默认每秒 5 次更新，165 Hz 和 60 Hz）。真实的回复还没有测过。如果你在回复期间打字时觉得卡，请在「Testing feedback」表单里告诉我们，并附上你的 GPU 和显示器刷新率。
- **高刷新率下机器有负载时，编辑器会掉帧。** 在一台 Intel 笔记本上，165 Hz、有其他工作让机器保持 17-28 % 繁忙时，它每秒画 137-149 帧，而原生 Neovide 是 160 帧；繁忙度 34 % 时是 111 帧，四次测量里出现过一帧 83.6 ms。这是在 0.2.0 的打字延迟修复之前测的，之后没有重测。打字本身在测过的机器上与原生 Neovide 相差不到 1 ms。

## 在中国大陆访问 eitri.cc 和 GitHub

一些中国运营商的用户反馈（目前收到的有：福建、江苏、河南的中国电信），到不在运营商白名单上的境外网站的连接会被重置，所以 eitri.cc 和 github.com 在那里可能根本打不开。安装脚本从 GitHub 下载，所以也可能同样失败。如果你能通过别的办法拿到发布文件，`sh install.sh --tarball FILE --sums FILE --sig FILE` 可以直接用它们安装（[INSTALL.zh-CN.md](../INSTALL.zh-CN.md#offline-from-release-files)）；构建 sidecar 时仍然需要一个能访问 Node.js 和 npm 的网络。

<a id="companion-mode"></a>
## Companion 模式

Companion 模式是把 agent 面板作为一个独立窗口，开在你自己的 nvim 旁边（见[使用指南](guide/companion.zh-CN.md)）。它比较新，试过的部分比单窗口模式少。

- **到目前为止只在一个隔离的、无界面的测试会话里试过，还没有在真实硬件上试过：** sway 配合终端里的 nvim；sway 上配合 Neovide 0.16.2 的 `eitri split`（也试过从 tmux 里的 shell 启动）；无界面 GNOME Shell 50 里的 GNOME Shell 扩展。Hyprland、niri、别的 nvim 图形前端，以及在真实 GNOME 登录会话里启用扩展，完全还没有试过。Hyprland 和 niri 上，边缘上的行为是窗口管理器自己的，Eitri 不去检查它。
- **nvim 运行在 tmux 里的情况还没有试过。** tmux 窗格的边缘仍归 tmux 管。用 vim-tmux-navigator 时，没法用 `Ctrl+h/j/k/l` 从 nvim 跨到面板窗口；用 smart-splits.nvim 并按[使用指南](guide/companion.zh-CN.md#navigator-plugins-and-tmux)设好它的 `at_edge` 钩子时，在 tmux 自己的边缘上这次移动应该能到达面板，但还没有测试过。从面板里打开文件也不会把编辑器的窗口提到前面（从 nvim 往上的进程树通向的是 tmux server，而不是终端）。
- **在窗口管理器会话之前启动的 tmux server** 会在它的环境里一直保留那个会话旧的 `SWAYSOCK` 或 `HYPRLAND_INSTANCE_SIGNATURE`，从它里面的 nvim 启动的面板会继承这个过期的值：移动焦点和提到前面都不会有任何效果。重新登录后请重启 tmux server，或者从带有当前会话环境的 shell 里启动面板。
- **`foot --server` 的客户端共用同一个 pid**，所以按进程把编辑器提到前面时，可能提起同一个 server 的另一个 foot 窗口，而不是装着你那个 nvim 的窗口。
- **在 sway 的边缘，这个按键会被吞掉。** Eitri 必须先接管或放行 `Ctrl+h/j/k/l`，才能去问 sway 那个方向上有没有窗口，所以在边缘上这个按键什么也不做，和 tmux 自己的 `select-pane` 在它的边缘上一样。
- **KDE：不移动焦点。** 在那里 Wayland 客户端没法抢到焦点；请用桌面自己的窗口键。
- **GNOME 需要 Eitri GNOME Shell 扩展**才能移动焦点：由你自己启用，下次登录时生效（见[使用指南](guide/companion.zh-CN.md#gnome-the-extension)）。有了它：移动只在拥有焦点的窗口所在的显示器上进行；X11 或 XWayland 窗口永远不算数；在上一次移动之后大约三分之一秒内再移回去会被拒绝；所有窗口都跑在同一个进程里的终端（GNOME Terminal、Ptyxis）在交回焦点时会落到它最近使用的窗口，那未必是 nvim 所在的窗口；被转发的 `eitri split` 不会把面板带到前面；监视器的“带到前面”不起作用。

## 安全

- **Eitri 会先问一句，再加载项目自己的 Claude 配置，这一步有它的限度。** 在你信任一个项目之前，它的 agent 会话只加载你自己的用户设置，不加载项目里的 `.claude/settings.json`、`.claude/settings.local.json`、`.mcp.json` 和 `CLAUDE.md`，所以一个不是你写的仓库，没法在你发出第一条消息时启动它自己的 hook 和 MCP server，也没法预先批准工具调用。这个提问会列出找到的内容和确切的命令。信任按项目记住，并且和你看到的内容绑定：`.claude/`、`.mcp.json` 或 `CLAUDE.md` 有任何改动，Eitri 都会再问一次，包括终端里的 `claude` 做的改动（它的「不再询问」会改写 `settings.local.json`）。Eitri 不会导入终端 `claude` 自己的信任答案，所以你在终端里已经信任过的项目，在这里还会问一次。Eitri 检查不了的文件（超过 4 MiB、读不出来、管道，或者符号链接）会在提问里点名，你的回答只管这一次启动。hook 调用的、在 `.claude/` 之外的脚本，只通过 hook 的命令文本得到信任，所以信任一个项目，就是信任它的代码可以运行，和构建它一样。在真正的终端里继续一个对话，运行的是普通的 `claude`，适用它自己的信任提问。想忘掉所有回答，删掉 `~/.local/state/eitri/trust/`。
- **在 Auto 标签页里，Claude Code 自己的 auto 模式可能不经询问就让 agent 在 `.git/` 下写文件**，包括新建一个 git hook。回合审阅不显示 `.git/` 下的改动，信任提问盯的是 `.claude/`、`.mcp.json` 和 `CLAUDE.md`，不是 `.git/`。在你在意的仓库里跑完一个回合后，值得看一眼 `ls .git/hooks`。相反，`.claude/` 下的改动会让 Eitri 在下一个会话之前重新提出信任提问。
- **你自己 Claude Code 设置里的 hook（或者一个已信任项目的 hook）可以在你批准之后改掉工具调用。** Claude Code 在 Eitri 看过原始输入之后才应用 hook 改写过的输入；如果 Claude Code 随后再询问，它的卡片显示改写后的输入，而工具那一行显示原始的。在终端里 hook 也是这样工作的。
- **已信任项目的 `permissions.allow` 规则，只有在你也在终端 `claude` 里信任过那个目录时，才会作用到 Auto 标签页。** 这是 Claude Code 自己的规则；你用户设置里的规则始终有效。
- **底部终端可以写你的剪贴板，这是有意为之。** 在那里运行的程序可以用 OSC 52 转义序列设置剪贴板或主选区（和 Alacritty 的默认行为一样）；发生时 Eitri 不会提示，所以你接下来粘贴的内容可能不是你复制的。读取剪贴板则会被拒绝。
- **agent 面板的脚本策略是新的，只在测试里检查过，还没有在每一种环境的屏幕上看过。** 面板现在只允许自己的那段脚本（按哈希），不再允许任意内联脚本。如果更新后 agent 面板一直是空白，请连同你的 WebKitGTK 版本一起报告。

<a id="turn-review"></a>
## 回合审阅

agent 的一个回合结束后，在面板的 BROWSE 模式里按 `c`，会列出这个回合期间磁盘上发生变化的文件和它们的 hunk。从那里可以用 `x` 把一个 hunk 或一个文件撤销（revert）回回合之前的样子，用 `u` 撤销这次撤销，用 `i` 和 `s` 把评论和你的撤销发回给 agent，用 `o` 在你的 nvim 里把这些 hunk 画在文件上。

- **它会保存一份你项目文件的副本。** 为了知道一个回合改了什么，Eitri 在每个回合前后各拍一次快照，放进它自己的仓库，位置在 `~/.local/state/eitri/review/`（目录 0700，文件 0600）。凡是你的忽略规则没有排除的文件都会被复制，单个文件最大 8 MiB、最多 20,000 个文件，所以项目里有没写进 `.gitignore` 的大型生成文件时，这个目录会变大。删掉这个目录就清掉了全部内容，下一个回合重新开始。它按项目保留最近 100 个回合、最多 30 天。它不会往你的项目里写任何东西，也从不写你自己的 `.git`：Eitri 只向 git 询问你仓库的 git 目录在哪里，并读取其中的 `info/exclude`，让你在那里排除的文件同样不被复制。
- **“这个回合期间磁盘上变了”不等于“agent 改了”。** 你手动做的修改、`Bash` 或另一个标签页做的修改，会和 agent 自己的编辑（`✓`）并列显示为 `?`；与别的回合重叠的回合会被标出来。
- **撤销会保留被它替换掉的字节**，同样放在 `~/.local/state/eitri/review/` 下，保留 30 天，所以 `u` 可以把它们找回来。
- **另一个程序在同一时刻写同一个文件，可能丢掉它的那次写入。** 撤销在写入前会再检查一次文件，但并不存在其他程序都遵守的锁；`u` 会还原当时在那里的内容。
- **`review.enabled = false` 的窗口，或者使用旧后端（legacy）的窗口，仍然会挡住另一个窗口的撤销。** 同一个项目的每个 Eitri 窗口在打开期间都持有一个很小的锁文件，这样一个窗口不会在另一个窗口未保存的缓冲区下面写文件。
- **文件位于指向某个目录的符号链接之后时，撤销会被拒绝**，不管这个链接指向项目里面还是外面。
- **被中断的撤销只会在开着回合审阅的窗口里提示恢复。** 如果窗口在原地改写一个文件（有第二个硬链接、属于别的用户或带扩展属性的文件）的中途被杀掉，下一个开着回合审阅的窗口会提示恢复保存下来的字节。
- **限制：** 超过 8 MiB 的文件只列出“过大”，不给 patch；超过 2,000 行的 patch 只显示行数统计；文件名不是合法 UTF-8 的文件会被列出，但打不开它的 patch；位于更大仓库里面的项目（在仓库顶层之下打开），既不会遵守父目录在它自己根目录之上的 `.gitignore` 文件，也不会遵守该仓库的 `info/exclude`，所以只在那里被排除的文件会被复制进这个存储。

## 稳定性

0.2.0 是第一个公开版本。0.2.1 新增的很多内容（companion 模式、`eitri split`、GNOME 扩展、回合审阅、信任提问、使用 Claude Code 自己 auto 模式的 Auto 标签页）只在一个隔离的、无界面的测试会话里见过它正常工作；恢复上次会话的标签页、tmux 键位导入和新的 `j`/`k` 移动只经过了自动化测试。特别欢迎来自真实桌面的反馈。0.x 期间 `init.lua` 的 API 和默认按键还可能改动；0.3 的目标是稳定到能当我们自己日常的主力编辑器，要先经过真实硬件和一周的日常使用。

## 这里没有你遇到的问题？

[提交 bug 报告](https://github.com/HunterGrey-cyber/eitri/issues/new/choose)；如果不确定算不算 bug，可以先在 [Discussions](https://github.com/HunterGrey-cyber/eitri/discussions) 里问。
