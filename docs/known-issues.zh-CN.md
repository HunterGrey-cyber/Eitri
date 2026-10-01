[English](known-issues.md) | 简体中文
<!-- translated-from: known-issues.md sha256=f5877ae22f130abf6228654fe2b0d49876cdaf06adf1f0ac767e06d94ae22588 -->

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

## 稳定性

0.2.0 是第一个公开版本。0.x 期间 `init.lua` 的 API 和默认按键还可能改动；0.3 的目标是稳定到能当我们自己日常的主力编辑器。

## 这里没有你遇到的问题？

[提交 bug 报告](https://github.com/HunterGrey-cyber/eitri/issues/new/choose)；如果不确定算不算 bug，可以先在 [Discussions](https://github.com/HunterGrey-cyber/eitri/discussions) 里问。
