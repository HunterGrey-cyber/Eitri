[English](known-issues.md) | 简体中文
<!-- translated-from: known-issues.md sha256=03f034904823bdb62a17357472893c93ae9e469b1f8df442000e9ca0588088fb -->

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

## 速度与绘制

- GTK 低于 4.16 时，编辑器换一种方式绘制，那里的打字延迟还没有测过。
- agent 回复流式输出时，在编辑器里打字，约 6-9 % 的按键要等两次刷新而不是一次（Intel 笔记本，GTK 4.22.5，模拟的输出流，按默认每秒 5 次更新，165 Hz 和 60 Hz）。真实的回复还没有测过。如果你在回复期间打字时觉得卡，请在「Testing feedback」表单里告诉我们，并附上你的 GPU 和显示器刷新率。

## 在中国大陆访问 eitri.cc 和 GitHub

一些中国运营商的用户反馈（目前收到的有：福建、江苏、河南的中国电信），到不在运营商白名单上的境外网站的连接会被重置，所以 eitri.cc 和 github.com 在那里可能根本打不开。安装脚本从 GitHub 下载，所以也可能同样失败。如果你能通过别的办法拿到发布文件，`sh install.sh --tarball FILE --sums FILE --sig FILE` 可以直接用它们安装（[INSTALL.zh-CN.md](../INSTALL.zh-CN.md#快速安装)）；构建 sidecar 时仍然需要一个能访问 Node.js 和 npm 的网络。

<a id="companion-mode"></a>
## Companion 模式

Companion 模式是把 agent 面板作为一个独立窗口，开在你自己的 nvim 旁边（[INSTALL.zh-CN.md](../INSTALL.zh-CN.md#use-it-beside-your-own-nvim)）。它比较新，试过的部分比单窗口模式少。

- **只试过 sway 配合终端里的 nvim。** Hyprland、niri、GNOME，以及上游 Neovide（或别的 nvim 图形前端）作为宿主，还没有在真实硬件上见过。Hyprland 和 niri 上，边缘上的行为是窗口管理器自己的，Eitri 不去检查它。
- **在 tmux 里**，tmux 窗格的边缘仍归 tmux 管，所以除非你在 tmux 一侧加一个绑定，否则没法用 `Ctrl+h/j/k/l` 从 nvim 跨到面板窗口；从面板里打开文件也不会把编辑器的窗口提到前面（从 nvim 往上的进程树通向的是 tmux server，而不是终端）。
- **在窗口管理器会话之前启动的 tmux server** 会在它的环境里一直保留那个会话旧的 `SWAYSOCK` 或 `HYPRLAND_INSTANCE_SIGNATURE`，从它里面的 nvim 启动的面板会继承这个过期的值：移动焦点和提到前面都不会有任何效果。重新登录后请重启 tmux server，或者从带有当前会话环境的 shell 里启动面板。
- **`foot --server` 的客户端共用同一个 pid**，所以按进程把编辑器提到前面时，可能提起同一个 server 的另一个 foot 窗口，而不是装着你那个 nvim 的窗口。
- **在 sway 的边缘，这个按键会被吞掉。** Eitri 必须先接管或放行 `Ctrl+h/j/k/l`，才能去问 sway 那个方向上有没有窗口，所以在边缘上这个按键什么也不做，和 tmux 自己的 `select-pane` 在它的边缘上一样。
- **GNOME 和 KDE：不移动焦点。** 在那里 Wayland 客户端没法抢到焦点；请用桌面自己的窗口键。

<a id="turn-review"></a>
## 回合审阅

agent 的一个回合结束后，在面板的 BROWSE 模式里按 `c`，会列出这个回合期间磁盘上发生变化的文件和它们的 hunk。这个版本里它是只读的：没有撤销（revert）、没有评论，也没有 nvim 里的覆盖层。

- **它会保存一份你项目文件的副本。** 为了知道一个回合改了什么，Eitri 在每个回合前后各拍一次快照，放进它自己的仓库，位置在 `~/.local/state/eitri/review/`（目录 0700，文件 0600）。凡是你的忽略规则没有排除的文件都会被复制，单个文件最大 8 MiB、最多 20,000 个文件，所以项目里有没写进 `.gitignore` 的大型生成文件时，这个目录会变大。删掉这个目录就清掉了全部内容，下一个回合重新开始。它按项目保留最近 100 个回合、最多 30 天。它不会往你的项目里写任何东西，也从不写你自己的 `.git`：Eitri 只向 git 询问你仓库的 git 目录在哪里，并读取其中的 `info/exclude`，让你在那里排除的文件同样不被复制。
- **“这个回合期间磁盘上变了”不等于“agent 改了”。** 你手动做的修改、`Bash` 或另一个标签页做的修改，会和 agent 自己的编辑（`✓`）并列显示为 `?`；与别的回合重叠的回合会被标出来。
- **限制：** 超过 8 MiB 的文件只列出“过大”，不给 patch；超过 2,000 行的 patch 只显示行数统计；文件名不是合法 UTF-8 的文件会被列出，但打不开它的 patch；位于更大仓库里面的项目（在仓库顶层之下打开），既不会遵守父目录在它自己根目录之上的 `.gitignore` 文件，也不会遵守该仓库的 `info/exclude`，所以只在那里被排除的文件会被复制进这个存储。

## 稳定性

0.2.0 是第一个公开版本。0.x 期间 `init.lua` 的 API 和默认按键还可能改动；0.3 的目标是稳定到能当我们自己日常的主力编辑器。

## 这里没有你遇到的问题？

[提交 bug 报告](https://github.com/HunterGrey-cyber/eitri/issues/new/choose)；如果不确定算不算 bug，可以先在 [Discussions](https://github.com/HunterGrey-cyber/eitri/discussions) 里问。
