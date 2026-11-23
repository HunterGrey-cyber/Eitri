[English](known-issues.md) | 简体中文
<!-- translated-from: known-issues.md sha256=2ab7a515516e6a9184dc403a57b64c922ddbff6648e7a761091148e3db2afae6 -->

# 已知问题与限制

当前 0.x 版本里不支持的、还比较粗糙的、以及还没有测过的部分。提 bug 之前请先看一遍；如果这里写的有错，或者已经变了，也请告诉我们。环境要求在 [eitri.cc](https://eitri.cc/zh/#requirements) 上也有；安装见 [INSTALL.zh-CN.md](../INSTALL.zh-CN.md)。

## 能在哪里运行

- **x86_64 的 Linux，Wayland。** X11 只试过一种会话：Ubuntu 24.04 上的 GNOME on Xorg，没有 GPU 加速。窗口、编辑器、agent 面板、底部终端和平铺在那里都能用。X11 上编辑器始终用 GTK 方式绘制，GNOME Shell 扩展和 companion 模式的焦点移动对 X11 窗口不起作用。其他 X11 桌面和窗口管理器（包括 i3）没有测试过（欢迎反馈）。macOS：移植中。Windows：不支持，WSL2 配合 WSLg 也许能用，但没有测试过。目前还没有 ARM 版本。
- **在 GNOME 和 sway 上测试过，companion 模式还在 Hyprland 上测试过。** 其他桌面和合成器，包括 KDE Plasma，没有测试过，欢迎反馈。Hyprland：companion 模式（焦点移动和置前）在真实运行的 0.57.0 和 0.56.2 上跑过（0.56.2 分别用 `hyprland.conf` 和 `hyprland.lua`）；第二个窗口的置前没有跑过。
- **[合成器窗口](guide/compositor-windows.zh-CN.md)**是 Hyprland、sway 和 niri 上的默认行为，还很新，有一些地方不符合预期：见指南里的[已知缺口](guide/compositor-windows.zh-CN.md#known-gaps)。
- **[远程开发](guide/remote-development.zh-CN.md)是实验性功能，默认关闭**（`remote_dev.enabled`）。笔记本需要是 Linux（macOS 默认不区分大小写的文件系统还没有处理），并且要自己装好 Mutagen。远程标签页还不能用回合审阅。其余限制见指南里的[这个版本里没有的](guide/remote-development.zh-CN.md#what-is-not-in-this-version)。
- **需要 WebKitGTK 2.40 或更新**（`webkitgtk-6.0` API）。预构建二进制按 glibc 2.39 和 GTK 4.14 构建，更旧的版本没有测试过。Ubuntu 24.04、Debian 13、Fedora 40+、RHEL 10 和 Arch 满足要求；Ubuntu 22.04 和 Debian 12 不满足。
- **Neovim 0.11 或更新**：用你自己 `PATH` 里的，或者让安装脚本在你的版本更旧时为 Eitri 单独下载一份私有副本（0.12.6）。`PATH` 里的 Neovim 0.11 会被接受，也不会被替换，但有三个常见配置在它上面仍有问题：当前的 kickstart.nvim 无法启动（它用了 0.12 才有的 `vim.pack`）；LazyVim 在第一次启动时，`Ctrl+j` 和 `Ctrl+k` 会被它的通知浮窗抢走，Eitri 收不到；AstroNvim 偶尔会保留它自己的 `Ctrl+j`（在 0.12.6 上也出现过一次，所以和版本无关）。在 0.12.6 上，LazyVim 和 kickstart.nvim 都没有这两个问题。如果你用其中之一，请让 `PATH` 里是 0.12，或者用 `EITRI_NVIM` 指向它。
- **Claude Code**，已安装并配置好，2.1.252 或更新，低于 3.0。没有它时 Eitri 照样能打开，编辑器可以用；agent 面板没法运行对话。
- **Codex 和其他 agent 是可选的。** Codex 标签页需要 0.160.0 或更新的 `codex`，已安装并配置好（测试过从 0.160.0 到 0.166.0 的每个版本）；其他 agent 的标签页需要一个通过标准输入输出说 ACP 第 1 版的 agent。见 [Codex 与其他 agent](guide/agents.zh-CN.md)。

## 安装

- **agent sidecar 在你的机器上构建**，在安装的过程中完成（需要联网，以及构建时约 1.4 GiB 可用磁盘空间；要下载 Node.js 和 npm 包）。AUR 的 `eitri-bin` 在它自己的构建过程里做同样的事，所以要几分钟，而不是几秒。原因见 [INSTALL.zh-CN.md](../INSTALL.zh-CN.md#为什么-sidecar-要在你自己的机器上构建)。
- **`.deb` 和 `.rpm` 装完后需要运行一次 `eitri setup`**，用你自己的用户身份；这两种包都不会构建 sidecar。
- **从 0.2.2 升级会重新构建一次 sidecar**，因为 0.3.0 固定了一个更新的版本：下载量和磁盘空间与第一次安装相同；`.deb` 或 `.rpm` 升级之后还要再运行一次 `eitri setup`。
- **sidecar 更新可能在两个地方失败**（[INSTALL.zh-CN.md](../INSTALL.zh-CN.md#updating-the-sidecar)）。`eitri sidecar update` 会用一句直白的话停下，并且什么都不改：常见原因是连不上 nodejs.org、npm 或 grpc.io，可用磁盘空间太少，签名验证不过，发布需要更新的 Eitri，或者同时有另一个 `eitri sidecar` 命令在运行。更新已经构建好、但在某个标签页里启动不了，不会弄坏这个标签页：它改用 Eitri 自带的 sidecar，状态栏显示 `⚠`，详情里写明原因。`eitri sidecar use bundled` 让它不再尝试。更新只对它之后打开的标签页生效，Eitri 0.3.0 完全不能装更新。
- **Ubuntu 23.10 及以后，包括 24.04：** AppArmor 默认会拦住 WebKit 的沙盒。Eitri 启动时会检查；编辑器和终端照常工作，agent 面板的位置会显示只需做一次的修复步骤。修复需要 `sudo`，也有代价，两者都写在 [INSTALL.zh-CN.md](../INSTALL.zh-CN.md#ubuntu-2310-及以后版本) 里。
- **如果你的网络连不上 GitHub、Node.js 或 npm**，安装脚本的每一项下载都可以改从你自己或你所在机构运行的镜像获取，每个文件照样按签名的发布来校验（[下载地址](../INSTALL.zh-CN.md#download-addresses)）。用别的办法拿到的发布文件，可以用 `sh install.sh --tarball FILE --sums FILE --sig FILE` 安装（[INSTALL.zh-CN.md](../INSTALL.zh-CN.md#offline-from-release-files)）。

## 速度与绘制

下面这些是在 0.3.0 发布前不久的开发构建上，于一台 AMD 台式机上测得的，条目里写了别的机器的除外。下面区分了真实的刷新周期反馈和默认回退时钟。

- **用 GTK 的方式绘制时，编辑器在 GTK 4.24 上慢一个刷新周期。** 默认情况下那里不用这种方式：在 Wayland 上、GTK 4.24 或更新、显卡驱动不是 NVIDIA 的时候，编辑器把它的帧放在自己的一个表面上直接交给合成器（[编辑器怎样到达屏幕](guide/editor-presentation.zh-CN.md)），在 sway 和 Mutter 上、60 和 165 Hz 下，打出的字母到达屏幕的时间中位数只比原生 Neovide 晚 0.02 到 0.46 ms。X11、NVIDIA 的驱动、`editor.presentation = "gtk"`，以及回退之后（编辑器会打印原因），仍然用 GTK 的方式。在那种情况下，合成器向 GTK 提供真实的 165 Hz 周期时，GTK 4.24.1 的打字中位数多 5.97 ms，同一个构建用 GTK 4.22.5 时只多 0.30 ms；GTK 使用 60 Hz 回退时钟时，约多 16.5 ms。一个 156 行的纯 GTK 复现程序在 GTK 开发分支（4.25.0，提交 c36202f6，2026-10-25）上也测到了这个延迟。编辑器自己的表面还没有在 NVIDIA 或 KWin 上试过。
- **用 GTK 的方式绘制时，在 GTK 4.24、165 Hz 的输出上按住 `j` 滚动会掉帧。** 原生 Neovide 每秒画出 165.5 个有变化的帧，Eitri 用 GTK 4.24.1 时为 140.6，同一个构建用 GTK 4.22.5 时为 165.4。第 99 百分位的帧间隔依次为 6.1、12.1 和 6.1 ms。这是另一次滚动测量，使用原样的呈现反馈，因此 GTK 4.24 仍使用 60 Hz 回退时钟；滚动还没有用真实周期反馈重测。用编辑器自己的表面时，一项按住键的测试不比 GTK 的方式差。
- **agent 回复流式输出时，在编辑器里打字会变慢。** 在 WebKitGTK 2.54 上，回复按默认每秒 5 次更新输出时，一次按键的重绘中位数是 11.2 ms，GTK 4.22.5 上也一样；在 WebKitGTK 2.52 上约为 2.3 ms。如果你在回复期间打字时觉得卡，请在「Testing feedback」表单里告诉我们，并附上你的 GPU 和显示器刷新率。
- **底部终端比你的按键晚画一个输出帧。** 在 60 Hz 的输出上，回显要在按键后约 18 ms 才出现在屏幕上（165 Hz 时是 8.7 ms），这是在 AMD 台式机上的 sway 里测的。Ghostty（中位数 17.3 ms）、WezTerm（16.7）和 Zed 里的终端（18.3）落在同一个位置；foot（1.0）、kitty（0.7）和 alacritty（0.7）则立刻画出来。原因是 GTK 4.24 每按一个键会对窗口提交两次，合成器要晚一个刷新周期才显示第二次；终端在 GTK 的帧时钟里绘制，而编辑器不是。我们试过去掉第一次提交，结果打字更慢了（两种刷新率下都约 20 ms），所以没有发布。想要最低的延迟，就在编辑器里运行 Neovim 自己的 `:terminal`，它和编辑器用同一种方式绘制（同一轮测试里，在缓冲区里打字的中位数是 1.7 ms）。`:terminal` 缓冲区本身的延迟没有测过。
- **内存：** 只开一个标签页的闲置窗口，面板在窗口里面时比原生 Neovide 多用 232 MiB；在合成器模式下（面板是一个单独的窗口，平铺式合成器上的默认方式）多用 245 MiB。两者都在我们定的 256 MiB 以内。这是 0.3.0 之后的开发版本在一块 AMD Radeon RX 9070 上测的；换成与它同机的集成显卡，窗口多用 248 到 259 MiB。第一个 Claude 标签页约多用 85 MiB，其中包括它的 sidecar（占 79 MiB）；之后每多一个标签页约多 63 MiB（其中约 55 MiB 是它自己的 sidecar）。每个 Claude 会话还会运行你自己安装的 `claude` CLI。那是你的程序，不是 Eitri 的一部分：闲置时每个会话约 109 MiB，回合进行时更多，不算在上面这些数字里。
- **0.3.0 里，闲置会话的 sidecar 会一直涨；0.4.0 修好了。** 七个会话闲置时，每个 sidecar 每小时涨 30 到 47 MB，三小时内没有停下来的迹象。每五分钟强制一次垃圾回收也没有让它变慢，所以这是 sidecar 仍然握着的内存，不是还没来得及回收的内存。按这个速度，开着五个标签页放一周的窗口会涨到约 30 GB（外推得出，没有实际跑过）。退出窗口就会还回去。0.4.0 自带 sidecar 0.5.0，其中包含 sidecar 0.3.0 首次做的修复：在同一个 25 分钟、单个闲置会话的测试里，sidecar 0.3.0 的内存从第五分钟起一直是 102.6 MB，而 Eitri 0.3.0 的每小时涨 22 MB（其中 JavaScript 堆每小时涨 44 MB，sidecar 0.3.0 只有 0.3 MB）。在 0.3.0 之后的开发版本上，一个窗口里四个闲置会话的 sidecar 30 分钟里合计一直是 248 MiB，每个的变化都在 0.1 MiB 以内。0.3.0 无法通过 sidecar 更新拿到这个修复。
- **高刷新率下机器有负载时，编辑器会掉帧。** 在一台 Intel 笔记本上，165 Hz、有其他工作让机器保持 17-28 % 繁忙时，它每秒画 137-149 帧，而原生 Neovide 是 160 帧；繁忙度 34 % 时是 111 帧，四次测量里出现过一帧 83.6 ms。这是在 0.2.0 的打字延迟修复之前测的，之后没有重测。
- GTK 低于 4.16 时，编辑器换一种方式绘制，那里的打字延迟还没有测过。

<a id="the-window"></a>
## 窗口

- **在沉浸模式下，一张卡片可能无人看见地等着。** 卡片出现在一个不在屏幕上的对话里时，Eitri 会显示一条提示，并在顶栏显示 `agent ⚑N`。沉浸模式（`<prefix> F11`）会隐藏顶栏，所以提示淡出之后，屏幕上就没有任何东西说有卡片在等；`Ctrl+k` 会把顶栏连同它的 `agent ⚑N` 再显示出来。
- **同一个项目上的第二个 Eitri 窗口保存它自己的布局和标签页。** 它从第一个窗口的布局开始，之后只保存自己的，所以第一个窗口打开时总是你在它里面留下的样子。
- **用浅色配色时，窗口启动时会先暗一下**，然后才变亮：顶栏、面板、插件页面、终端和编辑器的占位区都是这样。
- **在编辑器里按 `<prefix> ?`** 会把按键交给面板，但不画出按键表；之后在面板里按 `?` 就会显示。
- **回复里的相对链接**画成带下划线的链接样子，但 `gx` 和 HINT 不会提供它。
- **在 `$mod = Alt` 的 i3 或 sway 上**，前缀后面的那些 `Alt` 行会先被窗口管理器拿走，Eitri 看不到；[`docs/keymap/i3-alt-mod.lua`](keymap/i3-alt-mod.lua) 会把它们移开。
- **在 Neovim 0.12 上，在编辑器里执行 `:restart` 和 `:connect` 会让编辑器跟着走**，而不是结束它：`:restart` 转到它新启动的那个 Neovim（不会再留下多余的 `nvim --embed`），`:connect <地址>` 转到那个服务器。`:connect` 会让编辑器原来的 Neovim 继续运行，里面还开着它原来打开的东西；关闭窗口时会告诉你它的地址，`:connect!` 则会把它结束。`:connect` 到一个没有回应的地址，编辑器留在原处。在你从终端[接管](guide/takeover.zh-CN.md)的 Neovim 里，`:connect` 会把它还给终端并关闭窗口，`:restart` 暂时和 `:qa` 一样结束（窗口关闭；新的服务器没有窗口，运行在日志里写明的地址上）。

<a id="pages-you-build"></a>
## 你自己做的页面

Lua 面板和插件页面（[你自己做的页面](guide/pages.zh-CN.md)）是**实验性的**：0.x 期间插件清单格式和页面桥接都可能改变。

- **加载一个插件就是信任它。** 还没有同意界面：`init.lua` 里的 `eitri.plugin.load` 把清单里列出的每一项能力都交给这个插件，带任务的插件会以你的身份运行程序。只加载你信任的插件，就像只运行你信任的程序一样。插件的能力清单变了，下次启动时只会记一行日志。
- **Lua 面板是一个普通的网页视图。** 它加载你给它的地址，运行页面里的脚本，可以访问网络；它没有通往 Eitri 的桥接，拿不到配色，也没有按键工具包。只给它你信任的页面。
- **自己处理按键的页面**如果把按键交回编辑器（`layout.focus`），这个按键也会传到编辑器，除非页面调用了 `preventDefault()`。通过按键工具包处理的按键不受影响。
- 插件页面和 Lua 面板不会显示在 companion 窗口里。插件页面仍然可以作为单独的窗口开在你的 Neovim 旁边（`eitri page <id>`、`:EitriPage <id>`；[指南](guide/companion.zh-CN.md#pages-beside-your-nvim)），只要它只用到跟随缓冲区、Neovim 命令、打开文件、保存状态和网络：Markdown 预览可以，agent 页面不行。Lua 面板没有这样的窗口。

<a id="companion-mode"></a>
## Companion 模式

Companion 模式是把 agent 面板作为一个独立窗口，开在你自己的 nvim 旁边（见[使用指南](guide/companion.zh-CN.md)）。它比较新，试过的部分比单窗口模式少。

- **到目前为止试过的：** sway 配合终端里的 nvim；sway 上配合 Neovide 0.16.2 的 `eitri split`（也试过从 tmux 里的 shell 启动）；GNOME Shell 50 里 GNOME Shell 扩展的焦点移动；Hyprland 0.57.0 和 0.56.2 上配合终端 nvim 的 companion 模式（没跑第二个窗口的置前）。niri 和别的 nvim 图形前端还没有试过。niri 上，边缘上的行为是窗口管理器自己的，Eitri 不去检查它。
- **nvim 运行在 tmux 里的情况还没有试过。** tmux 窗格的边缘仍归 tmux 管。用 vim-tmux-navigator 时，没法用 `Ctrl+h/j/k/l` 从 nvim 跨到面板窗口；用 smart-splits.nvim 并按[使用指南](guide/companion.zh-CN.md#navigator-plugins-and-tmux)设好它的 `at_edge` 钩子时，在 tmux 自己的边缘上这次移动应该能到达面板，但还没有测试过。从面板里打开文件也不会把编辑器的窗口提到前面（从 nvim 往上的进程树通向的是 tmux server，而不是终端）。
- **在窗口管理器会话之前启动的 tmux server** 会在它的环境里一直保留那个会话旧的 `SWAYSOCK` 或 `HYPRLAND_INSTANCE_SIGNATURE`，从它里面的 nvim 启动的面板会继承这个过期的值：移动焦点和提到前面都不会有任何效果。重新登录后请重启 tmux server，或者从带有当前会话环境的 shell 里启动面板。
- **在桌面会话之前启动的 tmux server** 同样会一直保留旧的 `WAYLAND_DISPLAY`（或者根本没有），而旧会话留下的 socket 文件也可能还在 `$XDG_RUNTIME_DIR` 里。窗口打开之前，Eitri 会先检查哪个 `wayland-*` socket 背后真有合成器：能用的名字绝不改动，失效或不存在的名字换成唯一一个活着的 socket（有几个就取最新的），一个都没有时，窗口和编辑器一起用 X11（stderr 上有一行说明用了哪个）。如果编辑器那一半仍然显示关于显示的红色页面，页面会说明它试了哪个显示；请带着指向你合成器 socket 的 `WAYLAND_DISPLAY` 重新启动 Eitri（`ls $XDG_RUNTIME_DIR/wayland-*`；在 tmux 里用 `tmux setenv -g WAYLAND_DISPLAY <名字>`，或重启 tmux server）。
- **`foot --server` 的客户端共用同一个 pid**，所以按进程把编辑器提到前面时，可能提起同一个 server 的另一个 foot 窗口，而不是装着你那个 nvim 的窗口。
- **在 sway 的边缘，这个按键会被吞掉。** Eitri 必须先接管或放行 `Ctrl+h/j/k/l`，才能去问 sway 那个方向上有没有窗口，所以在边缘上这个按键什么也不做，和 tmux 自己的 `select-pane` 在它的边缘上一样。
- **KDE：不移动焦点。** 在那里 Wayland 客户端没法抢到焦点；请用桌面自己的窗口键。
- **开在你 nvim 旁边的页面**（`eitri page`、`:EitriPage`；[使用指南](guide/companion.zh-CN.md#pages-beside-your-nvim)）在 sway 上配合终端里的 nvim 0.12.6 试过（页面的配色、跟随、按键、关闭、`kill -9`），它的槽位和重新加载在 nvim 0.11.2 和 0.12.6 上也试过。还没有试过：tmux 里的 nvim（那里聚焦时的重新加载需要 `set -g focus-events on`）、Hyprland、niri、KDE 和启用了扩展的 GNOME。
- **接管终端里的 nvim**（`:EitriOpen`、`eitri open`；[指南](guide/takeover.zh-CN.md)）在 tmux 里的 Neovim 0.12.6 上试过，关掉窗口的五种方式都在内。还没有试过：Hyprland 和 GNOME 会话、真实的 lazy.nvim 配置，以及作为已安装插件的 vim-tmux-navigator 和 smart-splits。
- **页面窗口在两种情况下关得慢。** 正常关闭会把它的槽位留在你的 Neovim 里，直到 Neovim 下一次存活检查（最多一秒），被杀掉的窗口也一样；你运行 `:EitriPage` 时如果 Neovim 正卡在一个提示上，`eitri page` 会一直等着、不开窗口，直到它响应。
- **GNOME 需要 Eitri GNOME Shell 扩展**才能移动焦点：由你自己启用，下次登录时生效（见[使用指南](guide/companion.zh-CN.md#gnome-the-extension)）。有了它：移动只在拥有焦点的窗口所在的显示器上进行；X11 或 XWayland 窗口永远不算数；在上一次移动之后大约三分之一秒内再移回去会被拒绝；所有窗口都跑在同一个进程里的终端（GNOME Terminal、Ptyxis）在交回焦点时会落到它最近使用的窗口，那未必是 nvim 所在的窗口；被转发的 `eitri split` 不会把面板带到前面。

## 什么会离开你的机器

- **Eitri 自己不会向任何地方发送任何东西。** 它没有遥测，不检查更新，也不上传崩溃报告；崩溃时会在 `$XDG_STATE_HOME/eitri/log/`（默认是 `~/.local/state/eitri/log/`）下留一份日志，日志只留在本地。你运行安装脚本、`eitri setup`、`eitri sidecar check` / `update` 时，以及点开链接时（由浏览器打开），才会用到网络。
- **Markdown 预览默认加载远程图片。** 预览显示文档时，会去取里面的 `https` 图片，所以托管图片的服务器能看到你的地址和时间。`http` 图片从不加载。要一张都不加载：`eitri.plugin.load("markdown", { remote_images = "off" })`（见 [Markdown 预览](guide/markdown-preview.zh-CN.md#图片)）。预览没有别的网络用途。
- **Claude Code 和 Codex 用它们自己的设置。** Eitri 运行你装的 `claude` 和 `codex`，不改它们的遥测或更新设置，所以它们发送什么取决于你对它们的配置。
- **Lua 面板，或声明了主机的插件，可以访问这些主机**（见[你自己做的页面](#你自己做的页面)）；它们是你自己装的或配置的。

<a id="security"></a>
## 安全

- **Eitri 会先问一句，再加载项目自己的 Claude 配置，这一步有它的限度。** 在你信任一个项目之前，它的 agent 会话只加载你自己的用户设置，不加载项目里的 `.claude/settings.json`、`.claude/settings.local.json`、`.mcp.json` 和 `CLAUDE.md`，所以一个不是你写的仓库，没法在你发出第一条消息时启动它自己的 hook 和 MCP server，也没法预先批准工具调用。这个提问会列出找到的内容和确切的命令。信任按项目记住，并且和你看到的内容绑定：`.claude/`、`.mcp.json` 或 `CLAUDE.md` 有任何改动，Eitri 都会再问一次，包括终端里的 `claude` 做的改动（它的「不再询问」会改写 `settings.local.json`）。Eitri 不会导入终端 `claude` 自己的信任答案，所以你在终端里已经信任过的项目，在这里还会问一次。Eitri 检查不了的文件（超过 4 MiB、读不出来、管道、Eitri 跟不过去的符号链接（顶替了某个配置文件，或在 `.claude/` 下），或者不是合法 JSON 的设置文件）会在提问里点名，你的回答只管这一次启动。在 Claude 标签页里，`.claude/` 下或顶替 `CLAUDE.md` 的符号链接，如果用相对路径指向仓库内部，会被跟过去并检查，Codex 标签页里 `.codex/skills/` 内的符号链接也一样；例外情况见[权限指南](guide/permissions.zh-CN.md)。提问里的任何内容都不会被截断，所以提问会超过 2 MiB 的配置没法显示，也根本没法被信任：这样的项目只能不带它的配置启动。检查不会读项目子目录里的 `CLAUDE.md`；Claude Code 在 agent 读那里的文件时才会加载它，提问里也会这么说。hook 调用的、在 `.claude/` 之外的脚本，只通过 hook 的命令文本得到信任，所以信任一个项目，就是信任它的代码可以运行，和构建它一样。在真正的终端里继续一个对话，运行的是普通的 `claude`，适用它自己的信任提问。想忘掉所有回答，删掉 `~/.local/state/eitri/trust/`。
- **在 Auto 标签页里，Claude Code 自己的 auto 模式可能不经询问就让 agent 在 `.git/` 下写文件**，包括新建一个 git hook。回合审阅不显示 `.git/` 下的改动，信任提问盯的是 `.claude/`、`.mcp.json` 和 `CLAUDE.md`，不是 `.git/`。在你在意的仓库里跑完一个回合后，值得看一眼 `ls .git/hooks`。相反，`.claude/` 下的改动会让 Eitri 在下一个会话之前重新提出信任提问。
- **shell 命令可以不点名就到达 Eitri 自己的配置。** 往 `~/.config/eitri`、Eitri 的数据目录和状态目录或程序所在目录写入，在 Manual 和 Auto 下都是一张卡片（[权限指南](guide/permissions.zh-CN.md#eitris-own-configuration)），但 shell 命令按它的文字来判断：通过脚本、先 `cd`，或者自己拼出路径到达那里的命令不会被拦下；在 Auto 标签页里由 Claude Code 的 auto 模式来决定它，就像它决定一次对 `~/.bashrc` 的写入一样。
- **你自己 Claude Code 设置里的 hook（或者一个已信任项目的 hook）可以在你批准之后改掉工具调用。** Claude Code 在 Eitri 看过原始输入之后才应用 hook 改写过的输入；如果 Claude Code 随后再询问，它的卡片显示改写后的输入，而工具那一行显示原始的。在终端里 hook 也是这样工作的。
- **已信任项目的 `permissions.allow` 规则，只有在你也在终端 `claude` 里信任过那个目录时，才会作用到 Auto 标签页。** 这是 Claude Code 自己的规则；你用户设置里的规则始终有效。
- **在 bypass 标签页里，你自己的 ask 规则什么也拦不住。** 从 0.2.2 起，bypass 也会批准 Claude Code 自己提出的问题，包括你的 `permissions.ask` 规则（或被信任项目的）强制它提出的问题，不出卡片；工具那一行会注明它是在 bypass 下放行的。需要 ask 规则起作用的地方，请用 Manual 或 Auto 标签页。
- **plan 模式是在监督 Claude，不是沙箱。** Claude Code 做计划时，Eitri 不会自己批准任何文件编辑或命令，每一次都显示成卡片；但你的 hook（以及被信任项目的 hook）照样运行，MCP server 保持加载，其他工具（包括 MCP server 的工具）在标签页的模式允许的地方照样运行。哪些调用算作在 plan 模式里发出，取决于 Claude Code 报告它们的先后顺序：Eitri 晚了 2 秒以上才看到开始的调用，会按两种情况各判断一次（当作在 plan 模式里发出，和当作在它之外发出），两种回答不同时就是卡片。计划是 Claude 写的文字，显示时不运行脚本，远程图片显示成链接，隐藏字符会显示出来。
- **在 Bypass 标签页里，动态工作流里各个 agent 的调用会出卡片。** Claude Code 不会让 Eitri 看到这些调用开始，所以 Eitri 分不清某次调用是不是在 plan 模式里发出的：它等 2 秒，然后在两种回答会不同的地方显示卡片；在 Bypass 下，这就是每一次这样的调用，没有人在做计划时也一样。在 Manual 和 Auto 下，它们的文件编辑、命令和 Claude Code 自己的提问，凡是本来会由保存的规则或 Claude Code 的 auto 模式处理的，也都是卡片。
- **图片在资源限制下解码，不是在沙箱里。** Eitri 在一个单独的进程里解码粘贴或拖进来的图片，这个进程的内存限制为启动时的大小再加 768 MiB，处理器时间 30 秒，总时间 60 秒。一个只是很大或损坏的文件，碰到限制时结束的是这个进程，图片的小块上会显示错误。但这些限制挡不住专门利用图片解码器漏洞构造的文件：这个进程以你的身份运行，能访问你的文件，没有系统调用过滤。它需要 Linux 5.11 或更新的内核；在禁止它所用系统调用的容器或沙箱里，任何图片都无法解码，图片的小块上会说明原因。
- **底部终端可以写你的剪贴板，这是有意为之。** 在那里运行的程序可以用 OSC 52 转义序列设置剪贴板或主选区（和 Alacritty 的默认行为一样）；Eitri 事先不会询问，但写入之后窗格会显示三秒钟的提示（`Terminal 3: OSC 52 wrote clipboard: 9 bytes`），写明是哪个终端、哪个目标和多少字节，不显示内容。读取剪贴板则会被拒绝。
- **agent 面板的脚本策略是新的。** 面板现在只允许自己的那段脚本（按哈希），不再允许任意内联脚本。如果更新后 agent 面板一直是空白，请连同你的 WebKitGTK 版本一起报告。

<a id="claude-code-conversations"></a>
## Claude Code 对话

- 在 0.3.0 之前于 Eitri 里开始的 Claude Code 对话，继续时**会保留**原来较短的指令（用 Claude Code 2.1.302 测过）；新对话会得到完整的指令，`/compact` 也可能刷新它们（还没有测过）。
- 在 Eitri 里开始、之后在终端用 `claude --resume` 继续的对话，**会保留** Eitri 那一小段面板说明（用 Claude Code 2.1.302 测过）。这段说明讲的是会话由 Eitri 开始，所以在终端里也依然成立。
- 在终端里开始、之后在 Eitri 里继续的对话，**可能保留**终端的指令，而没有 Eitri 的面板说明（还没有测过）。
- 不提供 `system_prompt_preset` 的 sidecar 仍然会启动会话，使用 Agent SDK 的最小提示词。
  启动诊断会说明 Claude Code 的系统提示词不可用，并写出需要的后端版本；如果 `EITRI_VERDANDI_CHECKOUT` 或
  `EITRI_SIDECAR_BINARY` 选择了旧后端，请将该覆盖项指向为诊断中版本构建的后端。否则，运行 `eitri setup`。
- **关闭 Claude 标签页会直接取消它的定时唤醒和 cron 任务，不会先问。** 标签页开着时它们照常触发。后台任务在运行或有提问在等时，关闭标签页、它所在的视图或整个窗口会先问；但 Eitri 还没有定时任务的列表，所以既不会把它们算进去，也不会提醒你它们不会再触发。（只有被要求跨会话保留的 cron 任务，Claude Code 才会在会话结束后留着。）
- **后台子 agent 的权限卡片可能看不到。** 它画在这个子 agent 所在回合的最上面、子 agent 那一行的上方。提问框打开时，卡片可能滚出了视野，只有标签页上的 `⚑` 计数表明它在等。
- **有些关于后台任务的报告只显示 Claude Code 的原话。** Claude Code 用 Eitri 还不认识的方式报告一个任务时（停掉的 shell、工作流里的 agent），那一行只显示 Claude Code 发来的文字，往往只是任务的描述，不说这个任务怎么样了。
- **刚执行完 `/clear` 时，空白的视图中间可能留着一块空的高亮区域。** 下一个回合画出来后它就消失了。
- **Claude Code 会留着你发的图片。** 它把每张图片都写进它存在磁盘上的会话文件，和在终端里一样，Eitri 不会删除它们。Eitri 自己对标签页的记录只保留每张图片的类型、尺寸和校验和。一个标签页发出的图片超过 16 MB 之后，它会提醒一次对话已经很重，并建议 `/compact` 或开一个新标签页。
- **plan 模式和图片在 Claude Code 2.1.311 和 2.1.322 上试过。** 计划框、它的按钮和图片小块在深色和浅色、正常宽度和窄宽度下都试过。在本版本的后端上，Claude Code 2.1.322 做了计划、改了一版计划并在批准后执行，在计划批准进 Manual 之后让编辑停下来等回答，也读出了附上的图片（2.1.311 在更早的构建上也一样）。欢迎报告你看到的情况。
- **聊天面板很窄时（大约 320 像素），状态行会被截断。** 面板的模式只显示一个字母（`I`、`B`），权限模式的名字被截掉（`bypass permission`），活动行的转圈图标单独占一行。

<a id="codex-and-acp-tabs"></a>
## Codex 与 ACP 标签页

Codex 的标签页，以及用 `eitri.agent.register` 注册的 agent 的标签页都是新的（[指南](guide/agents.zh-CN.md)）。它们只和 Codex 本身以及 `codex-acp` 适配器一起试过几次，也没有在每一种环境上试过。

- **ACP 标签页里 agent 从你的编辑器读到的内容，和这个标签页别的工具结果一样绘制。** ACP 不说明工具属于哪个服务，所以 Eitri 分不清那里哪个结果来自[编辑器工具](guide/editor-tools.zh-CN.md)，其中的隐藏字符不会像在 Claude 和 Codex 标签页里那样显示出来。
- **第一次用一个 Eitri 没检查过的 `codex` 启动时，会运行一次简短的自检，通过之前每一次 Codex 启动都会被拒绝。** 它要几秒钟，按程序（路径、大小、修改时间和版本）记住，更新之后会再运行。如果状态目录在 `/tmp` 或 `$TMPDIR` 之下，而 `XDG_RUNTIME_DIR` 和缓存目录又都不在别处，它会失败，并点出 `XDG_STATE_HOME`。比试过的范围更新的 Codex 能运行，并被标为未测试。
- **Codex 标签页的沙箱限制写入、网络和套接字，不限制读取。** Codex 能读取你能读取的任何文件。你自己的 exec-policy `allow` 规则覆盖的命令在沙箱之外运行，MCP 工具遵循它们所在服务器的批准模式，被批准离开沙箱的命令以你的全部权限运行。你自己的 Codex 配置（`CODEX_HOME`，或 `~/.codex`）总会加载，Eitri 没办法关掉它。
- **子 agent 关不掉。** Codex 的子 agent 继承标签页的沙箱和信任回答；它们自己的文字不会显示。
- **模型在自己那次调用之后留在后台继续运行的命令，会比正常结束的回合活得更久。** 回合不会为它等着：状态栏会显示有几个在运行，那个回合的审查会说明之后的改动不在其中，`<leader>k` 会通过停掉 Codex 会话来停掉它们（先问一个 `y/n`；对话之后可以恢复）。在那之后的中断，或者关闭标签页，也会停掉它们。
- **你的钩子可能抢先回答。** 你自己的 Codex 配置里的 `PermissionRequest` 钩子，可能在 Eitri 看到请求之前就回答了它。没有测过的有：钩子在批准之后改写命令、每种 MCP 批准模式的行为、来自市场清单的插件在已信任的项目里加载、恢复一个有真实 Codex 客户端占着的线程、子 agent 的文件改动批准，以及沙箱能不能够到你的会话总线、tmux 或 nvim。
- **这些 agent 没有 Eitri 一侧的分类器或保存的规则。** 到达 Eitri 的每个请求，在 Manual 和 Auto 下是一张卡片，在 Bypass 下由 Eitri 批准。
- **ACP agent 只在 `y` 之后才启动**，因为 Eitri 没法不让它加载项目自己的配置；`n` 什么也不启动，也不会被记住。`project_files` 是你的清单：Eitri 只对你在那里点名的东西做哈希。Eitri 不清理 agent 的环境，所以把环境变量里的相对路径当作配置目录的 agent，会去读项目的目录。
- **ACP agent 不经询问运行什么，是 agent 自己的事。** Eitri 显示它收到的请求，只设置注册里点名的那个模式。这类标签页没有速率限制、上下文条或斜杠命令列表（模型和推理力度从 agent 自己列出的选项里选），`<leader>t`（在终端里继续）对它们也还不可用。
- **标签页详情的最后一行「Continue in a terminal…」只适用于 Claude 标签页。** `prefix i` 在 Codex 和 ACP 标签页上也会显示它。在这类已有会话的标签页上选它，会在 Eitri 里结束这个会话，并打印一条打不开它的 `claude --resume` 命令。请不要在这类标签页上用它；`<leader>t` 已经会拒绝它们。下一个版本里，这一行不再对它们显示。

<a id="turn-review"></a>
## 回合审阅

agent 的一个回合结束后，在面板的 BROWSE 模式里按 `c`，会列出这个回合期间磁盘上发生变化的文件和它们的 hunk。从那里可以用 `x` 把一个 hunk 或一个文件撤销（revert）回回合之前的样子，用 `u` 撤销这次撤销，用 `i` 和 `s` 把评论和你的撤销发回给 agent，用 `o` 在你的 nvim 里把这些 hunk 画在文件上。

- **它会保存一份你项目文件的副本。** 为了知道一个回合改了什么，Eitri 在每个回合前后各拍一次快照，放进它自己的仓库，位置在 `~/.local/state/eitri/review/`（目录 0700，文件 0600）。凡是你的忽略规则没有排除的文件都会被复制，单个文件最大 8 MiB、最多 20,000 个文件，所以项目里有没写进 `.gitignore` 的大型生成文件时，这个目录会变大。删掉这个目录就清掉了全部内容，下一个回合重新开始。它按项目保留最近 100 个回合、最多 30 天。它不会往你的项目里写任何东西，也从不写你自己的 `.git`：Eitri 只向 git 询问你仓库的 git 目录在哪里，并读取其中的 `info/exclude`，让你在那里排除的文件同样不被复制。
- **“这个回合期间磁盘上变了”不等于“agent 改了”。** 你手动做的修改、`Bash` 或另一个标签页做的修改，会和 agent 自己的编辑（`✓`）并列显示为 `?`；与别的回合重叠的回合会被标出来。
- **撤销会保留被它替换掉的字节**，同样放在 `~/.local/state/eitri/review/` 下，保留 30 天，所以 `u` 可以把它们找回来。
- **另一个程序在同一时刻写同一个文件，可能丢掉它的那次写入。** 撤销在写入前会再检查一次文件，但并不存在其他程序都遵守的锁；`u` 会还原当时在那里的内容。
- **`review.enabled = false` 的窗口，或者使用旧后端（legacy）的窗口，仍然会挡住另一个窗口的撤销。** 同一个项目的每个 Eitri 窗口在打开期间都持有一个很小的锁文件，这样一个窗口不会在另一个窗口未保存的缓冲区下面写文件。
- **文件位于指向某个目录的符号链接之后时，撤销会被拒绝**，不管这个链接指向项目里面还是外面。
- **被中断的撤销只会在开着回合审阅的窗口里提示恢复。** 如果窗口在原地改写一个文件（属于别的用户或带扩展属性的文件）的中途被杀掉，下一个开着回合审阅的窗口会提示恢复保存下来的字节。
- **有第二个硬链接的文件从不被还原。** 通过其中一个链接写入会改动其他链接，而它们可能在项目之外，所以这次还原会被拒绝，并给出 `the file has N hard links; writing it would change the other links too, so it was not written`，什么都不会被动。
- **限制：** 超过 8 MiB 的文件只列出“过大”，不给 patch；超过 2,000 行的 patch 只显示行数统计；文件名不是合法 UTF-8 的文件会被列出，但打不开它的 patch；位于更大仓库里面的项目（在仓库顶层之下打开），既不会遵守父目录在它自己根目录之上的 `.gitignore` 文件，也不会遵守该仓库的 `info/exclude`，所以只在那里被排除的文件会被复制进这个存储。

## 稳定性

0.4.0 是早期版本，还不稳定：会有粗糙的地方。0.x 期间 `init.lua` 的 API 和默认按键还可能改动。Eitri 在 Claude Code 2.1.302、2.1.311 和 2.1.322 上试过，其他版本还没有试过。欢迎反馈。

## 这里没有你遇到的问题？

[提交 bug 报告](https://github.com/HunterGrey-cyber/eitri/issues/new/choose)；如果不确定算不算 bug，可以先在 [Discussions](https://github.com/HunterGrey-cyber/eitri/discussions) 里问。
