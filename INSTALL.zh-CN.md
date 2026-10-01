[English](INSTALL.md) | 简体中文
<!-- translated-from: INSTALL.md sha256=7af6b10284821b93ebf1fe183b348b4ed0b308adf9aaada04110d54da69c968a -->

# 安装 Eitri

不管走哪条路径，结果都一样：一份按用户安装在 `~/.local` 下的安装（或者从 `.deb`/`.rpm` 装出的系统级安装），以及一个 `eitri` 启动器。`install.sh` 会在安装过程中顺带为你的机器构建 agent sidecar——除了 `.deb`/`.rpm` 这条路径：它不运行 maintainer script，需要你之后自己显式运行一次 `eitri setup`（见下文）。`eitri` 启动器本身从来不会构建 sidecar：在 sidecar 还没构建出来之前运行它，它照样会打开窗口，只是没有 agent 后端，并打印一行提示，让你运行 `eitri setup`。见下面的[为什么 sidecar 要在你自己的机器上构建](#why-the-sidecar-is-built-on-your-machine)。完整的选项列表见 `sh install.sh --help` 和 `eitri setup --help`；读完这一篇之后，权威来源就是那些输出，而不是这份文档。

**环境要求**：GTK ≥ 4.14、WebKitGTK 6.0、glibc ≥ 2.39、x86_64 Linux。**0.2.0 只支持 x86_64**——从源码构建（见下文）也需要同样的架构，在其他任何架构上都会拒绝运行，所以目前还没有 ARM 路径。**nvim ≥ 0.10**——可以是你自己 `PATH` 上的那份，也可以让安装脚本为 Eitri 单独获取一份私有副本（见下文）。还需要安装并登录 Claude Code（缺失时只是警告，不会拒绝安装——只是在你补上之前，Eitri 没法运行对话轮次）。

## 快速安装

通过 HTTPS 下载并运行安装脚本，安装到 `~/.local`，不需要 `sudo`：

```sh
curl --proto '=https' --proto-redir '=https' --tlsv1.2 -sSfL https://github.com/HunterGrey-cyber/eitri/releases/latest/download/install.sh | sh
```

`--proto '=https' --proto-redir '=https'` 会拒绝纯 HTTP 的重定向，`-L` 则会跟随 GitHub 自己的 `releases/latest/download/…` → `releases/download/v<X>/…` 重定向——没有 `-L` 的话，单纯的 `curl -sSf` 会从重定向响应里拿到一个空响应体，什么都不会传给 `sh`，结果是什么都没装上，也什么都没提示（`curl -f` 只会在 4xx/5xx 状态码上出错，从不会因为 302 出错）。这几个正是 `install.sh` 自己的 `fetch()` 函数在每一次非本地环回（也就是真实）下载时使用的旗标（`packaging/install.sh` 的 `fetch()` 函数）。

在 `--` 之后传选项：`curl … install.sh | sh -s -- --version 0.2.0 --yes`。

**如果你想在运行之前先读一遍**，可以先把它下载下来，而不是直接接进管道：

```sh
curl --proto '=https' --proto-redir '=https' --tlsv1.2 -fsSL -o install.sh https://github.com/HunterGrey-cyber/eitri/releases/latest/download/install.sh
less install.sh      # 或者用你自己的编辑器
sh install.sh
```

这样做只是能让你先读一遍脚本；并不能证明你下载到的就是真品——要做到这一点，见[运行前先验证](#verify-before-running)。

已经有发行文件了（用别的方式拿到的，所以这一步本身不需要联网）？`sh install.sh --tarball FILE --sums FILE --sig FILE` 会直接用它们安装，检查方式和上面的路径完全一样。之后构建 sidecar 仍然需要联网，去拉取 Node.js 和它所依赖的那些 npm 包。

<a id="verify-before-running"></a>
## 运行前先验证

`SHA256SUMS` 和 `SHA256SUMS.sig` 也会一并发布。验证签名能证明 `SHA256SUMS` 本身是真实的；但这**不能**单独证明你下载到的 `install.sh` 和它是对应的——那还需要额外、明确的一步，这一步很容易被漏掉，而下面这份流程要补上的正是它。

```sh
curl --proto '=https' --proto-redir '=https' --tlsv1.2 -fsSL -o install.sh    https://github.com/HunterGrey-cyber/eitri/releases/latest/download/install.sh
curl --proto '=https' --proto-redir '=https' --tlsv1.2 -fsSL -o SHA256SUMS    https://github.com/HunterGrey-cyber/eitri/releases/latest/download/SHA256SUMS
curl --proto '=https' --proto-redir '=https' --tlsv1.2 -fsSL -o SHA256SUMS.sig https://github.com/HunterGrey-cyber/eitri/releases/latest/download/SHA256SUMS.sig
```

从你要安装的那个 tag 所在的本仓库里，取得信任锚点 `packaging/release-signers`（ssh-keygen 的 `allowed_signers` 格式）——`install.sh` 自身内嵌了完全相同的字节（本仓库有一个测试保证两者一致），所以在对应 tag 上的一份 checkout 就是这个锚点：

```sh
git clone --branch v<version> --depth 1 https://github.com/HunterGrey-cyber/eitri.git nv-signers
cp nv-signers/packaging/release-signers .
```

接下来，按顺序：先检查签名，**再把下载到的 `install.sh` 本身对照刚刚验证过的 `SHA256SUMS` 做哈希校验**（上面那一步只能证明 `SHA256SUMS` 是真实的，不能证明 `install.sh` 和它对得上），最后才运行它。整段命令用 `( … )` 包了起来：直接粘贴进一个交互式 shell 时，某一步校验失败触发的 `exit 1` 只会结束这个子 shell，不会把你的终端会话关掉。

<!-- verify-recipe:start -->
```sh
(
ssh-keygen -Y verify -f release-signers -I release@eitri -n eitri-release -s SHA256SUMS.sig < SHA256SUMS \
  || { echo "SHA256SUMS does not carry a valid signature: do not run install.sh" >&2; exit 1; }

sha256sum install.sh | awk '{print $1}' | grep -qxF "$(awk '$2=="install.sh"{print $1}' SHA256SUMS)" \
  || { echo "install.sh does not match the verified SHA256SUMS: do not run it" >&2; exit 1; }

sh install.sh
)
```
<!-- verify-recipe:end -->

这份流程针对一个真实签名过的发行版本跑过一遍：把 `install.sh` 在签名完成后换成另一个文件（`SHA256SUMS`/`.sig` 原封不动，正是一个被攻陷的发布服务器或镜像会采取的手法），它会打印出 `install.sh does not match the verified SHA256SUMS: do not run it`，并且什么都不运行就退出。`packaging/tests/test_public_docs.py` 用它自己的一次性密钥，把这份流程钉死在这个行为上（还包括拒绝一份被篡改过的 `SHA256SUMS`），所以以后如果有改动删掉或削弱了某一步校验，挂掉的是一个测试，而不只是一个发行版本。

**直白地说明一下，这套机制能保护什么、不能保护什么。** 签名能保护 `--base-url` 指向的镜像、`eitri setup`，以及从一份已经装好的副本上做的重新运行（它内嵌的密钥早于之后任何一次可能的入侵），也能保护那些手动拿 `install.sh` 去对照 git 里 `packaging/release-signers` 检查的人。但它**不能**保护第一次 `curl … | sh` 免受发布页面本身或 GitHub 账号被攻陷的影响：验证器和它用的密钥，和 `SHA256SUMS` 来自完全相同的地方。**对于一个正式编号的发行版本**，`install.sh` 内嵌了签名密钥，签名缺失或不对时会直接拒绝运行——但如果你自己机器上没有 `ssh-keygen`，那就只会给个警告，退回到只做校验和检查，因为它没法自己跑这项校验。**对于一个候选发行版本**（比如 `rc.1`，它发布时发布密钥还没有列入），安装脚本根本不内嵌任何密钥——它的校验和只能检测出损坏，永远无法证明是谁发布的——而且上面这份流程会拒绝它：这样的 rc 是用一把一次性密钥签名的，从来不是正式发布密钥（`rc.1` 那个 tag 上的 `packaging/release-signers` 里一行密钥都没有；从第一个正式编号的发行版本起，它里面就是发布密钥）。如果你是通过带外方式拿到了那把一次性密钥的签名者文件，用 `--release-signers FILE` 传进去（或者在上面的流程里把它当作 `release-signers` 使用）；但那只能证明这些文件彼此吻合，不能证明它们来自真正的维护者——这些候选发行版本没有已发布的信任锚点。

## `.deb` / `.rpm`，然后 `eitri setup`

从[发布页面](https://github.com/HunterGrey-cyber/eitri/releases)下载 `eitri_<version>_amd64.deb` 或 `eitri-<version>-1.x86_64.rpm`，然后用你的包管理器安装：`sudo apt install ./eitri_<version>_amd64.deb`，或者 `sudo dnf install ./eitri-<version>-1.x86_64.rpm`（`.rpm` 这条命令已经在一台 Fedora 44 虚拟机上，针对 `rc.1` 自己的软件包实际跑过）。两种包都不会运行 maintainer script：它们只解包四个二进制文件、启动器、desktop 条目、图标和许可证（见[文件都装到哪里去了](#where-things-go)）——如果以 root 身份构建 sidecar，会让 `npm ci` 的每一个依赖安装脚本都以 root 身份运行，所以这里完全不这么做。两种包都没有对发行版自带的 `neovim` 声明硬依赖（`.deb` 完全没有依赖；`.rpm` 只有一个软性的 `Recommends`）：Ubuntu 自带的 `neovim` 太旧，达不到 ≥ 0.10 这条底线，如果依赖它，反而会解析、安装成功，却在第一次启动时才失败，还给不出有用的错误信息。

然后，以你自己的用户身份，运行一次：

```sh
eitri setup
```

这会为这台机器和这个锁定的 Verdandi revision 构建 sidecar（构建到 `$XDG_DATA_HOME/eitri/sidecar/<rev>/` 下，绝不以 root 身份——见[为什么 sidecar 要在你自己的机器上构建](#why-the-sidecar-is-built-on-your-machine)），然后运行和普通安装一样的 nvim 提议流程：如果你 `PATH` 上已经有 `>= 0.10` 的 `nvim`，跳过；如果这个发行版本的私有副本已经装好了，也跳过；否则就交互式地询问你（或者用 `--yes`/`--no-nvim`/`--with-nvim` 给出非交互式的答案）。`eitri setup --nvim-only`、`--uninstall` 和 `--sidecar-only` 则分别只运行其中一项——完整的模式列表见 `eitri setup --help`。

<a id="aur-arch"></a>
## AUR（Arch）

```sh
yay -S eitri-bin    # 预构建二进制；sidecar 仍然会在安装时在你自己的机器上构建
yay -S eitri-git    # 包括 eitri 本身在内的一切都从源码构建
```

（这两条命令就是 `packaging/aur/eitri-bin/PKGBUILD` 和 `packaging/aur/eitri-git/PKGBUILD` 各自的 `pkgname`，配合某个 AUR 助手工具运行；这一轮没有针对一个真实的 AUR 条目跑过它们。）两者都声明了 `provides`/`conflicts: eitri`，所以同一时间最多只能装一个叫 `eitri` 的软件包——不管是这两者中的哪一个，还是别的什么——而且 `eitri-bin` 自己的 `build()` 会把 sidecar 构建在 `/usr/lib/eitri/` 里，和几个二进制文件放在一起，而不是按用户各装一份。`eitri-bin` 的 `depends` 是 `gtk4 webkitgtk-6.0 glibc gcc-libs`；nvim 和 Claude Code 是 `optdepends`，不是硬依赖——不少 Arch 用户用的是自己编译的、或者由 `bob` 管理的 nvim，这样就不会和那种做法打架。

## 从源码构建（`--from-source`）

```sh
sh install.sh --from-source
```

**0.2.0 只支持 x86_64**：在除 x86_64 之外的任何 `uname -m` 上都会立即拒绝——目前锁定的唯一 Skia 预构建包是 x86_64 版本，还没有为 ARM 锁定对应的包。

在发行版本自己的 tag 上克隆公开仓库，如果它的 `HEAD` 和这个发行版本记录的 commit 对不上就拒绝继续，然后完全按照预构建路径的方式构建并安装。需要完整的工具链：**Rust 1.96 或更新版本**（2024 edition；安装脚本会拒绝更旧的 `rustc`）、**GTK 4.14+ 和 WebKitGTK 6.0 的开发包**外加 `pkg-config`、**Node.js 和 npm**、**`protoc`**、一套 **C 工具链**，以及第一次构建时需要的**联网**。`--checkout DIR` 会直接构建一份本地工作树，而不是重新克隆（这是作者自己的开发方式）——完整的前置条件列表和各发行版对应的包名，见 CONTRIBUTING.md 里的[前置条件](CONTRIBUTING.md#prerequisites)一节（仅有英文版）；`--verdandi-checkout` 和其他 `--from-source` 子选项见 `sh install.sh --help`。

<a id="why-the-sidecar-is-built-on-your-machine"></a>
## 为什么 sidecar 要在你自己的机器上构建

它打包了 Anthropic 的 `@anthropic-ai/claude-agent-sdk`（"© Anthropic PBC. All rights reserved"），本项目不会对它进行再分发。`eitri setup`（以及上面提到的每一条路径）会把它从 npm 仓库拉到你自己的机器上，遵循 Anthropic 自己的条款，并按这个发行版本指定的锁定 Verdandi revision 构建出来。sidecar 本身永远不会出现在 Eitri 的发行产物里。

<a id="updating"></a>
## 更新

重新走一遍你当初用的那条安装路径就行。`sh install.sh`（不带 `--version`）会把你装好的版本和 sidecar revision 拿去和最新发行版比较：如果两者都已经是最新的，它会打印 "up to date" 然后什么都不动就退出。否则它会解包新版本，并且**在切换任何东西之前先把新版本的 sidecar 构建好**，这样一次失败的构建不会动到旧的安装；只有构建成功之后，才会原子性地把两者互换。你之前那个 sidecar revision 会被保留，而不是删掉——这样即便新的那个后来出了问题，这次切换也是可以信赖的；它（连同这台机器上任何一个不再被任何安装引用的 revision）要等到*下一次*更新时才会被移除。之后要重启已经打开的 Eitri 窗口——它们会继续用旧的安装跑下去，但新开的标签页或 agent 交接需要新版本才行。`.deb`/`.rpm` 的升级是你的包管理器自己的事；只有在锁定的 sidecar revision 变了的时候，才需要之后再运行一次 `eitri setup`（如果当前 revision 已经构建好了，这一步就是空操作）。

从 0.2.1 起 Eitri 有了图标，它的 desktop 条目也改用应用 id 命名，叫 `cn.huntergrey.eitri.desktop`（原来是 `eitri.desktop`）；桌面环境正是靠这个名字把窗口和它的启动器对上。在旧版本上更新时会移除旧条目——tarball 方式由安装脚本来做，而且只有当那个文件与 0.2.0 的安装脚本写下的逐字节一致时才会删（你改过的条目会保留，并被指出来），软件包升级则是因为新包不再列出那个文件——所以**如果你之前把 Eitri 固定在了 dash 或 dock 上，更新之后需要重新固定一次**。

如果你存了一份 0.2.0 的 `install.sh`，并且重新运行*它*来升级，也是可以的：发行版的 tarball 里仍然带着 0.2.0 自己的 desktop 条目，路径是 `share/applications/eitri.desktop`，那个安装脚本要求它必须存在（这个发行版的安装脚本会忽略它，也没有任何软件包会安装它）。那个旧安装脚本铺不出图标，所以在你再运行一次这个发行版的 `install.sh` 之前，你得到的是 0.2.0 的条目、没有图标；再运行一次会替换掉旧条目，并装上新条目和图标文件。反方向，也就是从这个发行版退回 0.2.0，要先运行 `eitri setup --uninstall`——它会删除程序、启动器、desktop 条目和图标、许可证、私有 nvim 和 sidecar，保留 `~/.config/eitri` 和 `$XDG_STATE_HOME/eitri`——然后再运行 0.2.0 自己的 `install.sh`；这个发行版的 `install.sh --version 0.2.0` 会拒绝，并给出同样的步骤，因为在它上面再装 0.2.0，会让新条目和图标与旧条目并存。

## 卸载

```sh
sh install.sh --uninstall            # 保留 ~/.config/eitri 和各项目自己的状态
sh install.sh --uninstall --purge    # 同时删除 ~/.config/eitri 和 $XDG_STATE_HOME/eitri
```

这会删除 `~/.local/lib/eitri`、`~/.local/bin/eitri` 启动器（只有当它带着 Eitri 自己的标记行时才会删；同名但无关的文件会被原样保留并被指出来）、desktop 条目（以及 0.2.0 的 `eitri.desktop`，只有当它与 0.2.0 的安装脚本写下的逐字节一致时才删）、[文件都装到哪里去了](#where-things-go)里列出的那九个图标文件，一个不多（你图标主题目录里你自己的文件都保留，即使它的名字和 Eitri 的一样、尺寸又是 Eitri 不装的）和许可证、`$XDG_DATA_HOME/eitri/nvim`（私有 nvim 副本）、下载缓存，以及除了某个已安装的 `.deb`/`.rpm` 在它的 `/usr/lib/eitri/RELEASE` 里仍然引用的那一个之外的所有 sidecar revision——所以卸载一份 tarball 安装，永远不会连带删掉软件包安装的 sidecar。除非你加上 `--purge`，否则它总是会保留 `~/.config/eitri`（你的 `init.lua`）和 `$XDG_STATE_HOME/eitri`（各项目的布局、已打开的标签页、提示词历史、已保存的权限规则）；`$XDG_DATA_HOME/eitri/nvim` 之外任何叫 `nvim`/`vim`/`vi` 的东西都不会被动到。对于 `.deb`/`.rpm` 安装，软件包本身从来不管那份按用户的 sidecar 或私有 nvim，所以移除软件包（`sudo apt remove eitri` / `sudo dnf remove eitri`）会把两者都留下——**而且清理它们的顺序很重要**。`eitri setup --uninstall` 对这种情况没用：`eitri setup` 就是 `/usr/lib/eitri/eitri-setup`，会随着 `/usr/bin/eitri` 一起被软件包删掉；而如果你在移除软件包*之前*运行它，它会保留下所有那些还在被这份已安装软件包自己的 `/usr/lib/eitri/RELEASE` 引用的 sidecar revision——这个文件正是它用来区分"某个已安装的 Eitri 还需要这个"和"没有谁需要它"的依据（同一份文件也决定了一次普通的[更新](#updating)）。所以：先用你的包管理器移除软件包，然后重新下载一份 `install.sh`（或者用你提前留好的一份副本），运行 `sh install.sh --uninstall`——软件包自己的 `RELEASE` 已经没了，这一次就没有什么能阻止它把 sidecar 和私有 nvim 一起删掉。这和上面是同一个 `--uninstall`，所以如果你在 `~/.local` 下还有一份 tarball 安装，它也会把那份安装连同它的 sidecar 一起删掉。想保留那份安装，就跳过这一步：它自己的更新会在某个 sidecar revision 不再被任何安装使用时把它删掉，见[更新](#updating)。

<a id="where-things-go"></a>
## 文件都装到哪里去了

```
~/.local/lib/eitri/                                              四个二进制文件、eitri-setup、RELEASE             (tarball 方式)
~/.local/bin/eitri                                               启动器，带有标记 `# eitri-launcher v1`           (tarball 方式)
~/.local/share/applications/cn.huntergrey.eitri.desktop          Exec = 启动器的绝对路径                          (tarball 方式)
~/.local/share/icons/hicolor/<size>/apps/cn.huntergrey.eitri.png 图标，16 到 512 px，另有 scalable/…/….svg       (tarball 方式)
~/.local/share/licenses/eitri/                                   LICENSE、THIRD-PARTY-LICENSES、SOURCE            (tarball 方式)
/usr/lib/eitri/                                                  同样的四个二进制文件、eitri-setup、RELEASE       (.deb/.rpm)
/usr/bin/eitri                                                   同样的启动器                                     (.deb/.rpm)
/usr/share/applications/cn.huntergrey.eitri.desktop                                                               (.deb/.rpm)
/usr/share/icons/hicolor/<size>/apps/cn.huntergrey.eitri.png     同样的图标文件                                   (.deb/.rpm)
/usr/share/licenses/eitri/                                                                                        (.deb/.rpm)
$XDG_DATA_HOME/eitri/sidecar/<rev>/                              eitri setup 构建出的 sidecar，按用户的各条路径
$XDG_DATA_HOME/eitri/nvim/<X.Y.Z>/                               私有 nvim 副本，仅在你接受该提议时才有
~/.config/eitri/init.lua                                         你自己的配置（EITRI_CONFIG_DIR 可覆盖该目录）
$XDG_STATE_HOME/eitri/                                           各项目的布局、已打开的标签页、提示词历史、权限规则
```

除了 **AUR**（`eitri-bin`/`eitri-git`）之外，上面这些安装路径的 sidecar 都是按用户的——AUR 的 `build()` 会把它构建在 `/usr/lib/eitri/` 里（见 [AUR](#aur-arch)）。

`$XDG_DATA_HOME`/`$XDG_CACHE_HOME`/`$XDG_STATE_HOME` 遵循通常的规则：未设置、为空或者是相对路径时，分别回退到 `~/.local/share`、`~/.cache` 和 `~/.local/state`——这和 Eitri 自己用的规则完全一样，所以安装脚本和运行中的程序永远不会在该去哪里找这件事上产生分歧。私有 nvim 副本永远不会被放上 `PATH`，也永远不会替换、链接或删除你系统里任何其他叫 `nvim`/`vim`/`vi` 的东西。

## 一次启动怎样开始：`init.lua` 里的两个设置

两个都写在 `~/.config/eitri/init.lua` 里；设成这里没列出的值，Eitri 会在启动时直接停下，并指出是哪个设置。

```lua
eitri.config.set("agent.restore", "offer")        -- "offer"（默认）、"auto" 或 "off"
eitri.config.set("agent.default_mode", "auto")    -- "auto"（默认）或 "bypass"
```

- **`agent.restore`** 决定上一个打开这个项目的窗口里开着的标签页怎么办。Eitri 会随时记下那些有 Claude 会话的标签页（它们的顺序、名字、模式，以及当时在屏幕上的是哪一个），而且不会因为你关了窗口就记成"没有标签页"。设为 `"offer"` 时，只要窗口里还没有任何会话开始，空标签页的仪表盘就会显示一行 `Restore last session`，按 `s` 即可；`"auto"` 在启动时不用按任何键就把它们带回来；`"off"` 既不提议也不记录。每个标签页都会被恢复（在你输入之前什么都不会发送），上次在屏幕上的那个仍然显示在屏幕上，并且会有一条消息告诉你恢复了几个。某个标签页如果保存的记录已经没了，或者它的会话被另一个窗口占着，就会被跳过并点名。上次处于 bypass 的标签页，没有你的一句"是"，绝不会以 bypass 回来：`s` 会先问，回答 `n`（或使用 `"auto"`）就让它以 auto 回来。
- **`agent.default_mode`** 是新标签页开始时所处的模式，针对你还没有用 `Shift+Tab` 离开过 bypass 的项目（那个选择按项目记住，并且始终优先）。设为 `"bypass"` 是唯一一种窗口不经询问就以 bypass 开始的方式，因为你已经在自己的文件里这么说了；它同时也让保存下来的 bypass 标签页不必回答那个问题就以 bypass 回来。

## 疑难排解

安装时几种常见的拒绝情形，用安装脚本自己的原话（有所删节）：

- **`run this as the user who will run Eitri`**——安装脚本拒绝以 root 身份运行（sidecar 是按用户构建的；以 root 身份执行 `npm ci` 会让每一个依赖的安装脚本都以 root 身份运行）。用你的普通用户身份运行；`--allow-root` 只是为容器环境准备的。
- **`this system has GTK 4.N, and Eitri needs GTK 4.14 or newer`** / **`WebKitGTK 6.0 … was not found`**——你发行版的 GTK4/WebKitGTK 太旧，或者缺少运行时库；错误信息里会给出你这个发行版系列对应的确切包名。Ubuntu 22.04 和 Debian 12 都在这条底线之下；在那上面从源码构建也一样行得通，只是前提是你另外装了足够新的开发包。Ubuntu 24.04、Debian 13、Fedora 40+、RHEL 10 和 Arch 都已经满足要求。
- **`Eitri's prebuilt binaries need glibc 2.39 or newer`**——和上面情况类似；预构建二进制需要一个不早于上面列出的那些发行版，否则就用 `--from-source`。
- **`checksum mismatch for …`**——下载损坏了，或者在传输过程中被改动过。不管是哪种情况，安装脚本都会拒绝安装，并且如果这个问题反复出现，会给出报告的地方；直接重新运行一次就好。
- **`the signature on SHA256SUMS does not verify`**——直接拒绝；在一个正式发行版本上遇到这个，不要继续往下走。见[运行前先验证](#verify-before-running)。
- **在 `PATH` 上找不到 `claude` CLI，或者版本不受支持**——这只是警告，不会拒绝安装：Eitri 照样会装上，但 agent 面板需要一个能用、已登录的 Claude Code 才能运行对话轮次。警告信息里会给出 Anthropic 自己的安装方式。
- **`~/.local/bin` is not on your `PATH`**，或者 **`\`eitri\` on this PATH runs <something else>`**——安装成功之后会打印出来；把 `~/.local/bin` 加到你 shell 的 `PATH` 里，或者把它排到当前其他响应 `eitri` 这个名字的东西前面（往往是早先某次 `.deb` 安装留下的 `/usr/bin/eitri`）。

- **`… is writable by its group or by anyone, and not sticky`** — 安装器把下载的文件放在
  `$XDG_CACHE_HOME/eitri`（默认是 `~/.cache/eitri`）。如果其他用户能写这个缓存目录，他们就可能在
  校验之后、使用之前替换下载的文件，所以安装器会拒绝。权限为 `0775` 的 `~/.cache`，只要它的组是你自己
  的私有组（Ubuntu、Debian 和 Fedora 的常见设置），就会被接受；但如果某条 ACL 或另一个 GID 相同的组让
  别人也能写，仍会拒绝。安装器只能看到系统列得出来的账号：机器加入了不列举用户的目录服务时，它无法排除
  有目录账号共用这个组，这时最稳妥的做法是 `chmod g-w ~/.cache`，或把 `XDG_CACHE_HOME` 指向一个只属于
  你的目录。

<!-- ubuntu-userns: revisit if the owner chooses the automatic sandbox-off option -->
<a id="ubuntu-2310-and-later"></a>
### Ubuntu 23.10 及以后版本

Ubuntu 自带的 `apparmor` 软件包设置了 `kernel.apparmor_restrict_unprivileged_userns=1`（含 24.04），所以一个进程只有在被某个授予了 `userns` 权限的 AppArmor profile 限制住的情况下，才能创建 Linux 用户命名空间；WebKitGTK 6.0 总是把它的 web 进程和网络进程用 `bwrap` 沙盒起来，需要这样一份 profile，而它的 6.0 API 根本没有开关能关掉这一点。

**Eitri 会在启动时检查一次这个限制是否存在。** 如果存在，编辑器和底部终端依然可用，agent 面板的位置会显示发生了什么以及具体的修复方法，而不是直接崩溃。curl 安装脚本在这种情况适用时，也会在安装成功的最后打印同样的修复步骤。

- **`.deb`**：把这份 profile 装成 `/etc/apparmor.d/eitri`，只授予 `userns` 权限——和 Ubuntu 自己的 `apparmor` 软件包为 `epiphany` 提供的 profile 形状一样。它没有 maintainer script，所以装包这一步不会加载它：运行一次 `sudo apparmor_parser -r /etc/apparmor.d/eitri`，或者重启电脑。
- **curl / tarball / `--from-source` / AUR，以及万一装到了受影响系统上的 `.rpm`**（默认没有任何基于 `.rpm` 的发行版会有这个限制）：安装脚本——或者如果这一步被跳过了，又或者限制是在安装之后才出现的，就由 Eitri 自己——会把属于你这次安装的 profile 写到你自己的数据目录下，并打印出两条命令，命令里就是你自己的路径：`sudo install -m 0644 <写出来的 profile> /etc/apparmor.d/eitri-user-<uid>`，然后 `sudo apparmor_parser -r /etc/apparmor.d/eitri-user-<uid>`。照着打印出来的运行一次就好。

**代价。** 除了 `userns` 之外，这份 profile 是不设限的：Eitri 启动的每一个进程——编辑器的 `nvim`、底部终端的 shell，还有 agent 的 Bash 工具运行的任何命令——都会继承它，所以它们全都可以创建用户命名空间，不只是 WebKit 自己的沙盒 helper。对按用户安装来说，这份 profile 是按你自己 `shell` 二进制实际运行的那个绝对路径附着的，这个路径就在你自己的 `$HOME` 下面，你（或者任何已经以你的身份在跑的东西）都能覆写它——授权跟着放在那里的东西走。

**逃生舱，代替应用修复：** 从终端启动 Eitri，加上 `WEBKIT_DISABLE_SANDBOX_THIS_IS_DANGEROUS=1 eitri`。这会把操作系统层面的沙盒，从渲染模型输出内容的整个进程上拿掉，而不只是针对这一个限制，而且只在终端里启动才有效——从应用菜单或 dock 启动不会带上这个变量。
<!-- /ubuntu-userns -->
