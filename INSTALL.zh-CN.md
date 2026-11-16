[English](INSTALL.md) | 简体中文
<!-- translated-from: INSTALL.md sha256=3a12682e916e37fe51097f1e8c72974842359a11d161654282b265bad899747a -->

# 安装 Eitri

**你需要：** x86_64 Linux，带 GTK ≥ 4.14、WebKitGTK 6.0 和 glibc ≥ 2.39（Ubuntu 24.04、Debian 13、Fedora 40+、RHEL 10、Arch 都满足）；**nvim ≥ 0.11**（你的版本更旧时，安装脚本可以给 Eitri 单独下载一份私有副本）；以及已经安装并配置好的 **Claude Code**（`claude` 命令）。

<a id="quick-install"></a>
## 快速安装

```sh
curl --proto '=https' --proto-redir '=https' --tlsv1.2 -sSfL https://github.com/HunterGrey-cyber/eitri/releases/latest/download/install.sh | sh
```

这是推荐的安装方式，适用于所有发行版；Arch 上用 [AUR 包](#aur-arch)更合适。装到 `~/.local`，不需要 `sudo`。安装时还会在你的机器上构建 agent sidecar（[原因](#why-the-sidecar-is-built-on-your-machine)），要几分钟，需要联网，构建时需要大约 1.4 GiB 的可用磁盘空间。选项写在 `sh -s --` 后面，例如 `… | sh -s -- --version 0.4.0 --yes`；全部选项见 `sh install.sh --help`。这个安装脚本安装 0.3.1 及以后的版本；要安装更早的版本，请用那个版本自己的 `install.sh`（它的发布文件，例如 `https://github.com/HunterGrey-cyber/eitri/releases/download/v0.3.0/install.sh`）。

想在运行前先校验下载的签名，见[运行前先验证](#verify-before-running)。

<a id="on-a-slow-or-filtered-network"></a>
### 网络慢或受限时

安装脚本下载的每个文件（发布文件、Node、npm 包、sidecar 用的编译器和私有 nvim）在连接断开或卡住时都会重试，文件下载会从断开的地方接着下。每个文件下载完仍然要对照发布签名、固定的哈希或 npm 的 lockfile 校验，所以重试或续传得到的文件如果不对，会被拒绝。重试之后仍然失败时，错误信息会写明是哪个文件、哪台主机，以及试过什么。

如果这些主机只能通过代理访问，运行前先设好代理：curl、npm 和安装脚本都读 `https_proxy`。

```sh
export https_proxy=http://127.0.0.1:7890    # 换成你的代理地址
```

`--base-url URL` 让 Eitri 自己的发布文件改从一个和 GitHub 发布页面结构相同的镜像下载；Node、npm、编译器和 nvim 仍然从原来的主机下载。

**如果 `install.sh` 本身没有完整下载下来**，`sh` 一行也不会执行：被截断的脚本会以语法错误结束，退出状态不为 0。但如果下载在收到第一个字节之前就断了，`sh` 拿到的是一个空脚本，这一行的退出状态就是 0，只有 curl 打印的 `curl: (NN) …` 那一行能看出出了问题。在脚本里，或者需要靠退出状态判断时，先下载再运行：

```sh
curl --proto '=https' --proto-redir '=https' --tlsv1.2 --retry 5 --retry-all-errors -sSfL -o eitri-install.sh https://github.com/HunterGrey-cyber/eitri/releases/latest/download/install.sh && sh eitri-install.sh
```

在 bash 或 zsh 里，先执行 `set -o pipefail` 再运行快速安装那一行，也能让它带着 curl 的状态失败。不要给管道那一行加 `--retry`：curl 收不回已经交给 `sh` 的内容，重试会把脚本接在已经发出去的那部分后面再发一遍。

<a id="aur-arch"></a>
## AUR（Arch）

```sh
yay -S eitri-bin    # 预构建二进制；sidecar 在打包过程中构建
yay -S eitri-git    # 全部从源码构建
```

更新随系统一起（`yay -Syu`）。nvim 和 Claude Code 都是可选依赖，所以不会动你自己编译的或者由 `bob` 管理的 nvim。

<a id="other-ways-to-install"></a>
## 其他安装方式

### `.deb` / `.rpm`

从[发布页面](https://github.com/HunterGrey-cyber/eitri/releases)下载软件包并安装，然后用你自己的用户身份运行一次 `eitri setup`：

```sh
sudo apt install ./eitri_<version>_amd64.deb       # Debian、Ubuntu
sudo dnf install ./eitri-<version>-1.x86_64.rpm    # Fedora、RHEL
eitri setup
```

这两种包不运行任何安装脚本，所以由 `eitri setup` 来构建 sidecar；你的 nvim 低于 0.11 时，它还会提议装一份私有副本。它的其他模式见 `eitri setup --help`。没有软件源，所以每次更新都要重新下载软件包。

<a id="from-source"></a>
### 从源码构建

```sh
sh install.sh --from-source
```

构建这个发行版本自己的 tag，然后和预构建方式一样安装。需要 Rust 1.96 或更新版本、GTK 4.14 和 WebKitGTK 6.0 的开发包、`pkg-config`、Node.js 和 npm、`protoc`、一套 C 工具链，以及联网；各发行版对应的包名见 [CONTRIBUTING](CONTRIBUTING.md#prerequisites)（仅有英文版）。`--checkout DIR` 改为构建一份本地工作树。能构建 0.2.1 及以后的版本（0.2.0 见[已知问题](docs/known-issues.zh-CN.md#安装)）。

<a id="offline-from-release-files"></a>
### 离线安装：用发布文件

```sh
sh install.sh --tarball FILE --sums FILE --sig FILE
```

校验和其他方式完全一样。构建 sidecar 时仍然需要联网。

<a id="why-the-sidecar-is-built-on-your-machine"></a>
## 为什么 sidecar 要在你自己的机器上构建

sidecar 打包了 Anthropic 的 `@anthropic-ai/claude-agent-sdk`（"© Anthropic PBC. All rights reserved"），本项目不对它再分发。安装脚本按 Anthropic 的条款，把它从 npm 下载到你自己的机器上，再按发行版本锁定的 Verdandi revision 构建出来。Eitri 的发布文件里都不包含它。sidecar 更新（见[下文](#updating-the-sidecar)）也是同样的做法：在你的机器上，用签过名的源码构建，所以它也不会把 Anthropic 的任何东西放进发布文件。

<a id="updating"></a>
## 更新

再走一遍同样的安装方式就行。如果已经是最新版本，`sh install.sh` 什么都不做；否则它先构建新的 sidecar，成功了才切换，所以更新失败不会动到你现有的安装。更新之后，重启已经打开的 Eitri 窗口。

`.deb` 或 `.rpm` 用包管理器升级，然后再运行一次 `eitri setup`（sidecar 已经构建好时，它什么都不做）。

从 0.2.2 升级上来：这次更新会重新构建 agent sidecar，因为 0.3.0 固定了一个更新的版本；另外，`XDG_CONFIG_HOME` 设为绝对路径时，Eitri 现在从 `$XDG_CONFIG_HOME/eitri` 读取设置（0.2.2 总是读 `~/.config/eitri`），两者不同时请把设置挪过去。

从 0.2.0 升级上来：desktop 条目改了名字，所以要把 Eitri 重新固定到 dock 上一次。

<a id="updating-the-sidecar"></a>
## 更新 sidecar

Claude Code 经常自己更新，有时新版本需要一个更新的 sidecar。sidecar 更新在两次 Eitri 发布之间单独发出，由你决定什么时候装。Eitri 0.3.0 不能装 sidecar 更新，也没有 `eitri sidecar` 这个命令：在 0.3.0 上运行 `eitri sidecar status` 得到的是 `eitri: no such directory`。先装它之后的那个版本（见上面的[更新](#updating)），之后下面这些命令才存在。

```sh
eitri sidecar status        # 新标签页用哪个、Eitri 自带的是哪个、还留着哪些；不联网
eitri sidecar check         # 问有没有更新的 sidecar 发布；什么都不改
eitri sidecar update        # 下载、验签、在本机构建并选用
eitri sidecar use bundled   # 新标签页回到 Eitri 自带的 sidecar
eitri sidecar use 0.3.3     # 选用一个已经构建好的更新
```

- **不会自己运行。** Eitri 不会在后台去找 sidecar 发布，也不会提示有新版本。只有你运行 `check` 的时候，它才会去问。
- **更新在你的机器上构建**，用签过名的源码，和 `eitri setup` 构建第一个 sidecar 的方式一样。需要能访问 nodejs.org、npm 仓库和 grpc.io，构建时需要大约 1.2 GiB 的可用磁盘空间（我们测过的一次构建，峰值约 1.15 GiB，结束后留下约 106 MiB）。网速快的时候用了半分钟，请留出几分钟。
- **签名必须验证通过，否则什么都不装。** sidecar 发布和 Eitri 自己的发布用同一把密钥签名，也没有跳过检查的选项。没有 `ssh-keygen`（OpenSSH 客户端）时，`eitri sidecar update` 会停下，并说出要装哪个软件包。
- **已经打开的 Claude 标签页继续用它启动时的 sidecar。** 新标签页用更新。
- **Eitri 自带的 sidecar 一直留作后备。** 如果更新启动不了，或缺少这个 Eitri 需要的东西，新标签页就改用自带的 sidecar，它的状态栏显示 `⚠`，标签页的详情（`Space i`，或 `/status`）会写明原因。`eitri sidecar use bundled` 会让它不再尝试，更新留在磁盘上，直到你再次选用。
- **比 Eitri 自带的 sidecar 更旧的更新永远不会被使用。** Eitri 升级后带来了更新的 sidecar，旧的更新会留在磁盘上不用，`eitri sidecar status` 会说明。`.deb` 或 `.rpm` 升级之后，先运行 `eitri setup`：更新的背后需要一个已经构建好的自带 sidecar。
- **设置了 `EITRI_SIDECAR_BINARY` 或 `EITRI_VERDANDI_CHECKOUT` 时，它们优先于更新。**
- **哪里有这个命令。** 它从 Eitri 程序旁边的 `eitri-setup` 运行：curl 和 tarball 安装、`.deb` 和 `.rpm`、AUR 软件包都有。自带 sidecar、没有 `eitri-setup` 的软件包会回答 `this package updates its sidecar itself`。

<a id="uninstalling"></a>
## 卸载

```sh
sh install.sh --uninstall            # 保留你的设置和 $XDG_STATE_HOME/eitri
sh install.sh --uninstall --purge    # 这两个也删掉
```

你的设置就是 Eitri 读取设置的那个目录：`XDG_CONFIG_HOME` 非空且是绝对路径时是 `$XDG_CONFIG_HOME/eitri`，否则是 `~/.config/eitri`。`--purge` 从不删除 `EITRI_CONFIG_DIR` 指向的目录，因为那里可能还放着别的程序的设置；它会提示这一点，需要的话你自己删。

这会删除安装脚本放在你主目录下的东西：程序、启动器、desktop 条目、图标、GNOME Shell 扩展、sidecar（包括你装过的更新）、私有 nvim 和下载缓存。你自己的 `nvim` 永远不会被动到。如果你启用过 GNOME Shell 扩展，先把它停用。

它唯一留下的是 Ubuntu 上按用户安装的 AppArmor profile（`/etc/apparmor.d/eitri-user-<uid>`，见 [Ubuntu 23.10 及更新版本](#ubuntu-2310-and-later)）：那个文件属于 root，而卸载从不运行 `sudo`。卸载会打印卸载并删除它的两条 `sudo` 命令；想把 profile 清掉就运行它们。

`.deb` 或 `.rpm`：先移除软件包，再运行 `sh install.sh --uninstall`，删掉不归软件包管的那份按用户的 sidecar 和私有 nvim（如果你在 `~/.local` 下还有一份 curl 安装，也会一起删掉）。顺序不能反：软件包还在的时候，它的 sidecar 会被保留。

<a id="where-things-go"></a>
## 文件都装到哪里去了

| | curl 或 tarball 安装 | `.deb` / `.rpm` / AUR |
|---|---|---|
| 程序 | `~/.local/lib/eitri/` | `/usr/lib/eitri/` |
| `eitri` 启动器 | `~/.local/bin/eitri` | `/usr/bin/eitri` |
| desktop 条目、图标、许可证 | `~/.local/share/` 下 | `/usr/share/` 下 |
| `:EitriPanel` 插件 | `~/.local/share/eitri/eitri.nvim/` | `/usr/share/eitri/nvim/eitri.nvim/` |
| GNOME Shell 扩展 | `~/.local/share/gnome-shell/extensions/eitri@huntergrey.cn/` | `/usr/share/gnome-shell/extensions/eitri@huntergrey.cn/` |
| agent sidecar | `$XDG_DATA_HOME/eitri/sidecar/<rev>/` | 同左，按用户（AUR：`/usr/lib/eitri/`） |
| sidecar 更新，以及新标签页用哪个 | `$XDG_DATA_HOME/eitri/sidecar/updates/<version>/` 和 `.../sidecar/selected` | 同左，按用户 |
| 私有 nvim（仅在你接受时） | `$XDG_DATA_HOME/eitri/nvim/<version>/` | 同左 |

Linux 上，不管哪种方式，若 `XDG_CONFIG_HOME` 非空且为绝对路径，你的配置是 `$XDG_CONFIG_HOME/eitri/init.lua`，否则是 `~/.config/eitri/init.lua`。Mac 使用 `~/.config/eitri/init.lua`。非空的 `EITRI_CONFIG_DIR` 会覆盖配置目录，且必须是绝对路径；相对路径会拒绝启动。各项目的状态（布局、已打开的标签页、提示词历史、权限规则、信任回答、回合审阅快照）都在 `$XDG_STATE_HOME/eitri/`。`$XDG_DATA_HOME` 和 `$XDG_STATE_HOME` 默认是 `~/.local/share` 和 `~/.local/state`。私有 nvim 永远不会被放上你的 `PATH`。

<a id="troubleshooting"></a>
## 疑难排解

安装脚本的原话（有删节）：

- **`run this as the user who will run Eitri`**：不要以 root 身份运行；sidecar 是按用户构建的。`--allow-root` 只是给容器用的。
- **`this system has GTK 4.N, and Eitri needs GTK 4.14 or newer`**、**`WebKitGTK 6.0 … was not found`** 或 **`Eitri's prebuilt binaries need glibc 2.39 or newer`**：发行版太旧（Ubuntu 22.04 和 Debian 12 就是），或者缺运行时的包；错误信息里会给出你这个发行版对应的包名。
- **`could not download … from …: curl: (NN) …; N attempts over N s to <host>`**：网络反复中断了这个下载。如果那台主机只能通过代理访问，设好 `https_proxy`；Eitri 自己的发布文件也可以用 `--base-url`（见[网络慢或受限时](#on-a-slow-or-filtered-network)）。
- **`checksum mismatch for …`**：下载损坏了。重新运行一次安装脚本。
- **`the signature on SHA256SUMS does not verify`**：停下，不要安装这个版本。
- **`ssh-keygen was not found, so the release signature cannot be checked`**：装上 OpenSSH 客户端（Debian 和 Ubuntu 上是 `openssh-client`，Fedora 上是 `openssh-clients`，Arch 上是 `openssh`）。`--insecure-skip-signature` 不校验签名直接安装，这时只校验 checksum。
- **在 `PATH` 上找不到 `claude`**：只是警告。面板要运行对话轮次，需要你已经安装并配置好的 `claude`。获取地址：<https://code.claude.com/docs/en/quickstart>。Eitri 找不到 `claude` 期间，首次启动界面和 Claude 标签页的失败提示都会这么说，并给出同一个地址。
- **`~/.local/bin` is not on your `PATH`**，或者 **`eitri` on this PATH runs /usr/bin/eitri**：把 `~/.local/bin` 放到 `PATH` 的最前面。
- **`… is writable by its group or by anyone, and not sticky`** 或 **`… is a symlink owned by another user`**：安装脚本把下载的文件放在 `~/.cache/eitri`，如果别的用户能写这个缓存，它就拒绝。运行 `chmod g-w ~/.cache`，或者把 `XDG_CACHE_HOME` 设成一个只属于你的目录。

<!-- ubuntu-userns: revisit if the maintainer chooses the automatic sandbox-off option -->
<a id="ubuntu-2310-and-later"></a>
### Ubuntu 23.10 及以后版本

Ubuntu（包括 24.04）只允许在授权了的 AppArmor profile 下创建用户命名空间，而 WebKitGTK 的沙盒需要它。Eitri 启动时会检查：编辑器和终端照常工作，agent 面板会显示修复方法，而不是崩溃。安装脚本也会打印同样的修复方法。

- **`.deb`**：软件包里带有 profile `/etc/apparmor.d/eitri`。运行一次 `sudo apparmor_parser -r /etc/apparmor.d/eitri` 加载它，或者重启。
- **其他所有方式**：安装脚本（或者 Eitri 启动时）会为你的安装写一份 profile，最后说明：在你运行它打印的两条 `sudo` 命令之前，agent 面板一直不可用。运行一次，然后启动（或重新启动）Eitri。安装脚本自己从不运行 `sudo`。

**代价：** 这份 profile 让 Eitri 启动的每个进程都能创建用户命名空间，包括编辑器、终端里的 shell 和 agent 运行的命令。按用户安装时，profile 绑定的是你 home 目录下的一个路径，所以放在那里的任何程序都能用上它。

**不做修复的办法：** 从终端运行 `WEBKIT_DISABLE_SANDBOX_THIS_IS_DANGEROUS=1 eitri`。这会关掉渲染模型输出的那个窗口的 WebKit 沙盒。
<!-- /ubuntu-userns -->

<a id="verify-before-running"></a>
## 运行前先验证

发布页面上还有 `SHA256SUMS` 和它的签名 `SHA256SUMS.sig`。把这三个文件都下载下来，再从本仓库这个发行版本的 tag 上取得发布密钥 `packaging/release-signers`：

```sh
base=https://github.com/HunterGrey-cyber/eitri/releases/latest/download
for f in install.sh SHA256SUMS SHA256SUMS.sig; do
  curl --proto '=https' --proto-redir '=https' --tlsv1.2 -fsSL -o "$f" "$base/$f"
done
git clone --branch v<version> --depth 1 https://github.com/HunterGrey-cyber/eitri.git eitri-signers
cp eitri-signers/packaging/release-signers .
```

然后先校验签名，再用签过名的 checksum 校验 `install.sh`，都通过了才运行它：

```sh
(
ssh-keygen -Y verify -f release-signers -I release@eitri -n eitri-release -s SHA256SUMS.sig < SHA256SUMS \
  || { echo "SHA256SUMS does not carry a valid signature: do not run install.sh" >&2; exit 1; }

sha256sum install.sh | awk '{print $1}' | grep -qxF "$(awk '$2=="install.sh"{print $1}' SHA256SUMS)" \
  || { echo "install.sh does not match the verified SHA256SUMS: do not run it" >&2; exit 1; }

sh install.sh
)
```

之后 `install.sh` 下载的每样东西，都会用同一个签名校验。密钥和发布文件都来自 GitHub，所以这能发现损坏或被换掉的文件，防不了 GitHub 账号本身被盗。
