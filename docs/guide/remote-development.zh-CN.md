[English](remote-development.md) | 简体中文
<!-- translated-from: remote-development.md sha256=ec74ee9cb1903fdb568dd739aeb2423d34394291a1d8ef7429f6023b236604bb -->

# 远程开发

远程开发让你电脑上的一个 Eitri 窗口，去处理放在另一台机器上的项目。代码、agent 和它的工具留在服务器上，工具链和凭据本来就在那里；你的 Neovim、它的插件、面板和你的按键都留在本地。这一页讲的是同步这一侧：项目怎样到达你的电脑、哪些东西不会到，以及两份副本对不上时 Eitri 怎么办。之后讲语言服务器，和 Neovim 替你运行的命令（格式化工具、lint 工具、`make`），它们都在服务器上运行。

它是**实验性**的，默认关闭。`init.lua` 里要有下面这一行，这里讲的一切才会运行：

```lua
eitri.config.set("remote_dev.enabled", true)
```

关闭时，`eitri host open` 和打开远程项目都会停止，并提示相应的设置项。你仍可运行 `eitri host status`、`list`、`stop`、`uninstall`，以及 `eitri host sync status` 或 `conflicts`。要解决文件冲突，请启用远程开发，并保持项目窗口或 `eitri host sync watch` 运行。

<a id="how-it-works"></a>
## 它是怎么工作的

服务器上跑着一个很小的 Eitri 辅助程序，第一次在那台主机上打开项目时，Eitri 会在你同意之后把它装上去。远程标签页里的 agent 就跑在服务器上，紧挨着它要改的文件。你的电脑上放着这个项目的一份镜像，路径和服务器上相同，由 [Mutagen](https://mutagen.io)（Eitri 驱动的一个文件同步工具）维护，这样你的 Neovim 和它的插件读写的都是普通的本地文件。你保存时文件送往服务器；agent 改了文件，文件就送回来。

后文把你坐着的这台机器叫「笔记本」，另一台叫「服务器」。

<a id="server-terminals"></a>
## 服务器终端

关闭服务器终端会结束它的 shell 以及仍在该会话中的所有程序；已经离开该会话的程序（守护进程，或用 `setsid` 启动的程序），以及在 shell 先行退出后仍在运行的程序，会继续在服务器上运行，就像退出 ssh 登录后一样。

如果在新的服务器终端打开期间笔记本的时钟向前跳了一分钟以上，Eitri 可能会丢失对它的记录：这个终端仍在服务器上运行，`eitri host list` 仍能看到它。

<a id="before-you-start"></a>
## 开始之前

- **用密钥登录的 ssh。** `ssh example-host` 必须能直接登录，不问你任何问题。Eitri 在后台建立的连接没法回答密码或口令提示，所以带口令的密钥应该先加载进你的 ssh agent。Eitri 会用你的 `~/.ssh/config`，所以凡是能写主机名的地方，`Host` 别名也能用。
- 笔记本上的 **OpenSSH 8.5 或更新**。版本更旧时，Eitri 会说出它找到的版本。
- 笔记本上的 **Mutagen 0.18 或更新**，放在 `PATH` 里，或者放在 `remote_dev.mutagen` 给出的绝对路径上。从 [mutagen.io](https://mutagen.io) 安装，或者自己编译一个不含 SSPL 许可部分的版本。**Eitri 不会替你附带它、下载它或编译它**：Mutagen 的官方构建里含有 SSPL 许可的代码，Eitri 不分发这部分。找不到，或者版本低于 0.18.0 时，`eitri host open` 会这么说，然后停下。Eitri 运行它自己的 Mutagen 守护进程，数据目录也是它自己的，所以绝不会碰你自己建的 Mutagen 会话。Mutagen 会通过同一条 ssh 连接，在服务器上放一个它自己的小 agent。
- **一台 Linux 服务器**，登录时的 `PATH` 里要有 `git`，并且用你连接时的那个账户，在上面装好 `claude` 命令、登录好。agent 就在那里用这个账户自己的登录运行。

<a id="settings"></a>
## 设置

都写在 `init.lua` 里，用 `eitri.config.set`。值不对会作为启动问题报出来，并指出是哪个设置项，同时使用默认值。

| 设置项 | 默认值 | 含义 |
|---|---|---|
| `remote_dev.enabled` | `false` | 打开远程开发 |
| `remote_dev.mutagen` | 未设置 | `mutagen` 的绝对路径；未设置就到 `PATH` 里找 |
| `remote_dev.tripwire_mib` | `64` | 体积[绊线](#the-size-tripwire)，8 到 4096 MiB |
| `remote_dev.flush_cap` | `3` | 发送时最多等多少秒，让你的保存先到达服务器，0.5 到 30 |
| `remote_dev.send_waits_for_flush` | `true` | 设成 `false` 则发送完全不等 |
| `remote_dev.lsp_grace` | `1800` | Neovim 的连接断开后，服务器还把语言服务器留多少秒，60 到 86400（[宽限期](#the-grace)） |
| `remote_dev.lsp_local` | 无 | 留在笔记本上的语言服务器名字，用逗号或空白分隔（[哪些留在笔记本上](#what-stays-on-the-laptop)） |

其余设置见[配置](configuration.zh-CN.md)。

<a id="open-a-project"></a>
## 打开一个项目

在终端里：

```sh
eitri host open example-host:/srv/project
```

路径开头那个冒号之前的部分，就是你交给 ssh 的主机写法：`example-host`、`user@example-host`，或者 `ssh://user@example-host:2222`。路径必须是绝对路径。命令按下面几步走，每一步要改动什么之前都会先问你：

1. 连接。如果 ssh 不认识服务器的主机密钥，Eitri 会显示密钥类型和指纹并询问。只有你能核对指纹时才回答 `y`；密钥随后会加进 `~/.ssh/known_hosts`。密钥变了，就是 ssh 的拒绝，原样显示。之后的每一次连接，包括 Mutagen 建立的，都必须显示同一把密钥，否则什么都不运行。
2. 探测服务器，告诉你缺什么，然后在往上装辅助程序之前先问你。每台主机只问一次；以后 Eitri 升级了，辅助程序会在那里直接更新，不再问。
3. 在笔记本上准备同一个路径（见下一节），检查两台机器上有没有别的同步工具（[Syncthing 和 Dropbox](#syncthing-and-dropbox)），并估算第一次同步有多大。
4. 在终端里跟着第一次同步：文件数、已收到和预计的 MiB、速率，然后是历史记录的进度。按 `Enter` 不再跟着看。只要有 Eitri 窗口开着这个项目，同步就会继续；一个都没有时它会暂停（见[下文](#sync-pauses-when-nobody-is-watching)）。

之后像打开任何项目一样打开这个文件夹：

```sh
eitri /srv/project
```

Eitri 记得这个文件夹是远程项目，所以窗口会以远程项目的方式打开：面板的状态栏显示主机、同步状态和冲突，agent 标签页跑在服务器上。`eitri --local /srv/project` 把同一个文件夹当作普通的本地副本打开，不建立连接（那里拒绝 agent 标签页，因为 agent 跑在服务器上）。

`eitri host list` 列出你打开过的每台主机上辅助程序在运行的东西：agent、服务器终端和语言服务器；`eitri host stop example-host` 停掉它们（并暂停那台主机上各项目的同步）；`eitri host uninstall example-host` 删除辅助程序。这几个命令在 `remote_dev.enabled` 关闭时也能用。

<a id="from-the-window"></a>
### 从窗口打开

打开远程开发后，项目选择器里有 **Open on a host…**。它列出你 `~/.ssh/config` 里的主机和以前打开过的主机，在服务器上补全文件夹，并按相同顺序问与 `eitri host open` 相同的问题。对从未打开过的文件夹运行 `eitri --ssh <host> <path>` 也会进入这个流程。它需要选择器的网页视图；没有的话，请用 `eitri host open`。

<a id="the-same-path-rule"></a>
## 同一路径规则

项目在两台机器上有**相同的绝对路径**：服务器上的 `/srv/project`，在笔记本上也是 `/srv/project`。agent 按服务器看到的样子报告路径，诊断信息和链接里带着这些路径，你的编辑器也据此打开文件。路径只有一个，到你这边就已经是对的，中间没有哪一层翻译可以把它翻错。

所以笔记本上要有这个目录，并且归你所有。如果它建不出来，Eitri 会打印一行只需运行一次的命令，`<user>` 处换成你的用户名：

```sh
sudo mkdir -p /srv/project && sudo chown <user> /srv/project
```

如果这个目录已经存在而且不是空的，Eitri 绝不会往上面同步。它会列出这份副本里有、服务器上可能没有的东西（未提交的改动、stash、没有任何远程仓库有的提交；此外还有文件数量和大小），并只提供一个操作：把它挪到 `/srv/project.eitri-local-<YYYYMMDD-HHMMSS>`。Eitri 从不删除挪开的那份。已经是这个项目镜像的文件夹（同一台主机、同一把服务器密钥，而且就是 Eitri 当初准备的那个文件夹）会连同里面的内容一起沿用。如果你把镜像挪走，又在同一路径放了另一个文件夹（重新克隆的、从备份恢复的），Eitri 会把它当作新文件夹：同步暂停，`eitri host open` 会先问你，再把它挪开、重新建一份副本。位于另一个项目镜像之内、或者包含一个镜像的文件夹会被拒绝。

<a id="what-syncs-and-what-does-not"></a>
## 哪些会同步，哪些不会

笔记本上的副本包含服务器上的每个文件，忽略规则排除的除外。`.git` 另外处理（见[笔记本上的 git](#git-on-the-laptop)）。

<a id="ignores-come-from-git-on-the-server"></a>
### 忽略规则取自服务器上的 git

Mutagen 不读 `.gitignore`，所以 Eitri 在服务器上读 git 的规则，再把它们转成模式交给 Mutagen：树里的每一个 `.gitignore`、`.git/info/exclude` 和服务器的 `core.excludesFile`，包括各自带规则的嵌套仓库和子模块。给的是模式，而不是现存文件的清单，所以第一次构建时才冒出来的 `target/` 一开始就已经被排除了，`.env` 之类被忽略的机密留在服务器上。

- **被跟踪的文件照样同步**，即使某条忽略规则匹配了它们：git 跟踪它们，Eitri 也就跟着同步。如果被忽略的已跟踪文件超过 10,000 个，同步不会开始，并会说出这个数量。
- 是符号链接、大于 1 MiB 或者不是 UTF-8 的 `.gitignore`，会让它所在的整个目录被排除，并给出一行原因。同步带不了的路径名只排除它自己，并列出来，不会拖住其余部分。
- `.gitignore` 变了，忽略规则也跟着变，但要等到安全的时候：Eitri 等到没有 agent 回合在运行、项目下没有未保存的缓冲区、而且两秒内没有任何同步，然后重建会话。在那之前，状态栏显示「ignore rules changed; applying when idle」。一个后来被忽略的文件**在笔记本上保留它的副本**：同步不再传它，但也不删除它。
- 重建会话时，如果某个文件只存在于一侧（你在它等待期间在笔记本上删了一个），另一侧的那份可能会回来。这样的文件会列为「copied back」（复制回来的）；见[下文](#an-edit-beats-a-delete)。

<a id="a-project-without-git"></a>
### 没有 git 的项目

没有 git 仓库时，Eitri 排除 `target/`、`node_modules/`、`.venv/`、`__pycache__/` 和 `.cache/`，并且**警告说 `.env` 之类的机密没有被排除**：没有任何东西说它们该被排除，所以它们会被复制到笔记本上。请自己把它们排除：

```sh
eitri host sync exclude .env
```

<a id="exclude-and-include"></a>
### `exclude` 和 `include`

`eitri host sync exclude <path>` 让一个路径（相对于项目根目录）只留在服务器上；`eitri host sync include <path>` 把它收回来，撤销一次 `exclude`，或者把某条忽略规则会排除的路径纳入同步。两者都存在 Eitri 为这个项目留的记录里，不在你的仓库里。变更的生效方式和 `.gitignore` 变更一样，等到安静的时候才做，笔记本上已有的副本会保留。如果路径在一个被排除的目录之下，`include` 会拒绝，并说出是哪条规则把它藏起来的。请在项目里面运行它们：项目就是 Eitri 有记录、并且包含你当前目录的那一个。

<a id="the-size-tripwire"></a>
### 体积绊线

服务器上突然冒出来一个新目录（没人忽略的构建产物，agent 下载的一个数据集），可能在你发觉之前就把磁盘填满。当一批要到笔记本上的内容将超过 `remote_dev.tripwire_mib`（64 MiB）时，Eitri 会暂停这个项目与该主机的同步，并说出它看到了什么：

```text
sync paused: build/out.bin on example-host would send 900 MiB to this laptop; sync of /srv/project with example-host waits until you choose: sync it anyway (eitri host sync allow build/out.bin) or leave it on the server (eitri host sync exclude build/out.bin)
```

`allow <path>` 放行这个路径（以及它下面的内容），同步随即恢复；`exclude <path>` 让它留在服务器上。不带路径的 `allow`，放行的是暂停时点名的那个路径；没有点名时，放行的是触发绊线的那一批。第一次同步不受限，因为你打开项目时终端已经显示了它的大小。绊线依据的是 Mutagen 的进度报告，所以在很快的链路上，一批内容可能在暂停之前已经到了一部分；它事后才发现的话，会说笔记本上的副本涨了多少。

<a id="symlinks-special-files-hard-links-times-and-modes"></a>
### 符号链接、特殊文件、硬链接、时间和权限

- **符号链接**：用相对路径指向项目内部的会同步。目标是绝对路径的，或者指到项目外面的，不会同步，在同步视图里显示为一个问题（「not synced」）。
- **FIFO、套接字和设备文件**不同步。同步视图里同样会列出它们。
- **硬链接**留不下来：每个名字都会作为各自独立的副本到达。
- **修改时间**在两台机器之间不保持一致：文件在笔记本上的时间就是同步写入它的时间。信任时间戳的构建工具可能比你预想的重新构建得更多。
- **文件权限。** Mutagen 带来的每个文件都以 `0600`（可执行的是 `0700`）写入，这会让服务器上一个 `0644` 的文件在你第一次保存时变成 `0600`。辅助程序里有一个很小的守护器盯着项目，把权限改回去：文件保持它在服务器上原来的权限，你在笔记本上设成可执行的脚本会获得属主的执行位，新文件取服务器账户的 umask。从笔记本传到服务器的只有可执行位；目录的权限不保留。守护器要求项目上一级的目录对服务器账户可写（Mutagen 在它旁边写入），如果某个项目的权限没能保留，Eitri 会告诉你，并说明原因。笔记本上的文件是 `0600` 和 `0700`。

<a id="sync-pauses-when-nobody-is-watching"></a>
## 没人看着时同步会暂停

只有当有东西在看着这个项目时，同步才会运行：一个 Eitri 窗口，一个在项目内某个文件上打开了 `eitri.nvim` 的普通 Neovim，或者 `eitri host sync watch`。最后一个离开时，Eitri 会暂停两个会话，这样冲突、忽略规则变更或大文件传输都不会在你看不到的地方堆积起来。此时 `eitri host sync status` 会说「no Eitri window or eitri host sync watch follows `<root>`; its sync is paused until one does」。打开一个窗口就恢复。按 Ctrl+C 或发送 SIGTERM 会结束首次同步或 watch 命令；如果该命令持有项目锁，它会暂停两个会话。用 SIGKILL 强制结束进程时，会话会继续运行，直到下一个持有者接手。

如果 `eitri host sync status` 显示项目已暂停，请根据原因在项目目录里操作。

| 暂停原因 | 怎么处理 |
|---|---|
| 你运行了 `eitri host stop <host>`，或者卸载助手时暂停了同步，但卸载没有完成。 | 准备重新开始时，运行 `eitri host sync resume`。 |
| 服务器的主机密钥变了。 | 核对提示里的新旧密钥。确认是预期的变更后，运行 `eitri host sync resume` 并确认新的服务器。 |
| 体积绊线拦下了即将传到笔记本的大批数据。 | 有具体路径时用 `eitri host sync allow <path>` 放行，没有路径时用 `eitri host sync allow` 放行这一批。要把某个路径留在服务器上，用 `eitri host sync exclude <path>`。 |
| 笔记本上的项目根目录被移走或替换了，或者旧版 Eitri 建立副本时没有记录目录身份。 | 运行 `eitri host open <host>:<root>` 核对并设置目录。这种情况下，`resume` 会拒绝恢复同步。 |
| 重新建立同步时，文件太多，无法安全地列出两边的内容。 | 用 `eitri host sync exclude <path>` 排除大目录，再运行 `eitri host sync resume` 重新列出两边的文件。 |

`eitri host sync flush` 会等一个同步周期结束。这一轮没能同步哪些条目，可以用 `eitri host sync status` 查看。

<a id="git-on-the-laptop"></a>
## 笔记本上的 git

agent 和真正的仓库都在服务器上。你笔记本上的 `.git` 是**服务器历史记录的只读副本**，这样 gitsigns、fugitive 的 status 和 blame、状态栏里的分支名之类，都可以在本地很快地读到它。历史记录在树到齐之后才继续到达；在它完整之前，状态栏显示进度，git 功能会说历史记录正在到达。

能写服务器 `.git` 的人（agent，或任何持有那个账户的人）决定副本里有什么，而仓库的配置和钩子是会运行程序的。所以这份副本被做成：服务器无法借它在你的笔记本上运行任何东西。它**不会**复制：`config`、`hooks`、`attributes`、`alternates` 和锁文件（叫这些名字的分支和标签照样复制）；子模块和链接工作树的 git 目录；正在进行中的 rebase 之类的操作；以及 `.git` 里的任何符号链接。如果笔记本上的文件夹里已经有你自己的 `.git`，Eitri 不会动它，在你把它挪出去之前，git 命令都在服务器上运行。Eitri 自己写笔记本上的 `.git/config`，内容来自它从服务器读到的一小份设置清单（远程仓库、分支跟踪、`user.name` 和 `user.email`），Eitri 在这份副本上运行的每一条 git 命令都会忽略钩子、fsmonitor、外部 diff 和 textconv。如果项目的 `.git` 是个文件（链接工作树或子模块的检出），或者用了 alternates，就什么都不镜像，每条 git 命令都转发到服务器；状态栏会说「git history is not mirrored (`<why>`); git commands run on `<host>`」。

<a id="the-git-shim"></a>
### git 垫片

在副本上提交，会被下一次同步撤销；在副本上 checkout，会改动工作树，而这个改动随后会当作你的编辑传到服务器。所以在窗口的 Neovim 里，以及在任何对远程项目运行了 `eitri.nvim` 的 Neovim 里，`PATH` 的最前面是 Eitri 提供的一个 `git`：

- **读操作在本地运行**，对着这份副本：`status`、`diff`、`log`、`show`、`blame`、`grep`、`rev-parse`、`ls-files`、`cat-file`、`for-each-ref`、`merge-base`、`describe`，以及 `branch`、`tag`、`remote`、`stash` 和 `config` 的只读形式，等等。它们带着 `--no-ext-diff --no-textconv` 运行，所以不会运行任何辅助程序。副本不含各远端的 `HEAD`（`origin/HEAD`），所以在笔记本上 Git 解析不了 `origin/HEAD`。
- **写操作送到服务器上。** `add`、`commit`、`checkout`、`switch`、`restore`、`reset`、`stash`、`merge`、`rebase`、`pull`、`push`、`fetch`、`cherry-pick`、`revert`、`apply`、`rm`、`mv` 以及清单里其他的形式，在服务器上对应的目录里运行，输入输出照旧，退出码是服务器的。命令送出之前，Eitri 先刷新同步，再检查指定路径是否有冲突或同步错误。整树命令会检查所有已报告的路径。如果同步工具省略了部分冲突或错误，Eitri 会拒绝执行命令。如果写操作指定的路径已被排除，或位于被排除的文件夹里，或 Eitri 无法核对它的路径（通配模式，或 `--pathspec-from-file`），也会拒绝执行。Eitri 只检查你指定的路径：如果你指定的文件夹里有被排除的文件，或者运行整树命令（如 `git add .`、`git commit -a`、`git stash`），服务器上的 Git 会作用于这些文件在服务器上的副本。要处理服务器自己的副本，请在服务器终端里运行 Git。转发到服务器的读操作可以查看已排除的路径。命令之后，两个会话都会再刷一次，所以运行了 `git checkout` 的插件读到的是结果。如果同步当前不在能刷新的状态，命令会被拒绝，并说「nothing was run」。那里没法回答任何提示：不会打开编辑器，要密码或口令的命令会直接失败。
- **其他一律拒绝**，只回一行话：`git clean`、`gc`、`clone`、`init`、`submodule`、别名，以及 Eitri 不认识的任何形式，都会打印「git `<subcommand>` is not run from here; run it on the server」。垫片从不猜。
- **从终端提交，或者带 `-m`。** 需要编辑器的形式（不带消息的 `git commit`、`-e`、`-p`、`rebase -i` 等）够不到服务器上的编辑器，所以它回答「commit from the terminal (it runs on example-host) or with -m」。提交信息缓冲区在这里没法工作：用 `-m` 给出消息，或者到终端里提交。如果转发出去的某条命令结果不明（链路断了），垫片会这么说，并请你到服务器上看 `git status`，而不是重复执行。

用完整路径（`/usr/bin/git`）调用 git 的插件，或者没有把垫片放在 `PATH` 最前面的终端，会直接在笔记本副本上运行 git。笔记本上的两个钩子保护这份历史副本。`pre-commit` 会提前拦住普通提交；`reference-transaction` 会拒绝更新引用，包括创建分支、打标签和用 `--no-verify` 提交。直接运行 `git stash` 会在重置文件之前停下，保留你在工作树里的改动。副本的配置也会拦住普通的 `fetch` 和 `push`。修改工作树的命令仍然可能改动文件，这些改动会同步到服务器：`reset --hard`、`checkout <branch>`、`cherry-pick`、`revert`、`rebase` 和 `stash pop` 会在引用更新被拒绝之前就改动文件，所以执行过之后请在服务器上看一下 `git status`。请通过垫片或在服务器终端里执行 Git 写操作。

还有一个要知道的剩余风险：如果树里的某个 `.gitattributes` 点名了**你自己的全局 git 配置**里定义的某个 `filter` 或 `textconv`，笔记本就会运行它，因为你的全局 git 配置仍然会被读取。服务器没法通过这份副本定义这样的过滤器，但它能写那个 `.gitattributes`。如果你要打开来自不完全可信的服务器的项目，请不要在全局配置里放过滤器。

<a id="what-the-mirror-holds-and-what-it-trusts"></a>
## 镜像里有什么，它信任什么

镜像是项目里未被忽略的文件在你笔记本磁盘上的完整副本，所以你的备份、索引器和其他读取那个文件夹的工具也都看得到它们。被忽略规则排除的东西留在服务器上，这就是为什么没被忽略的机密（或者没有 git 的项目里的 `.env`）会被复制；[`exclude`](#exclude-and-include) 可以让某一个路径留在服务器上。一个后来才被忽略的文件会在笔记本上保留它的副本，直到你自己删掉；同步视图会把这样的路径列为「no longer synced; this laptop keeps its copy」。

这些文件也来自服务器，而你自己的工具会去读它们。Eitri 把它能控制的路径都堵上了（见 git 一节）。服务器写进树里的项目本地 Neovim 配置，比如 `.nvim.lua`，如果你打开了 `exrc`，就会在你的 Neovim 里运行；Eitri 不会改这个设置。对一台你不掌控其账户的服务器，请像对待来自它的任何检出一样对待。

<a id="conflicts"></a>
## 冲突

同步是双向的，而且很谨慎：两边都改过的文件，它绝不覆盖。如果你和 agent 在同一个文件同步之前各改了一次，两份副本就分叉了，各自保持原样。面板的状态栏显示 `⇅` 和主机，冲突数量用错误色标出；有冲突的文件打开的缓冲区，符号栏里会有一个标记，并带一行提示；`:EitriSync` 打开当前项目的同步视图。任何运行了 `eitri.nvim` 的 Neovim 都有它，不只是 Eitri 窗口。

视图对每个冲突列出两侧各是什么（文件、不存在的文件、链接……），此外还有没能同步的路径及原因、特殊文件、「copied back」条目、笔记本保留的副本和忽略规则。对一个冲突可以：

- **保留服务器的。** 删掉笔记本上的文件；同步随后把服务器的带下来。
- **保留笔记本的。** 删掉服务器上的文件；笔记本的随之传上去。
- **合并。** 用 Neovim 的 diff 同时打开两份，服务器的那份是只读的；对结果执行 `:w`，会把相同的字节写到两侧，冲突随之消除。两个版本都不超过 8 MiB 时才提供。清掉服务器那个缓冲区会结束这次合并，之后的写入只保存在本地。
- **忽略（dismiss）**确认一个「copied back」条目；**dismiss all** 会写出项目名，并保留两侧的每个文件。

同样的操作可以在终端里运行：`eitri host sync resolve <path> --keep server|laptop` 和 `eitri host sync dismiss <path>|--all`；`eitri host sync conflicts` 列出冲突。

每个会删除或替换某个版本的操作，都会先把那个版本存一份到 `$XDG_STATE_HOME/eitri/remote/conflict-backups/<key>/<time>-<id>-<laptop|server>/<path>`（默认是 `~/.local/state/eitri/...`），只有你能读。操作发生时会打印这些路径，副本在 30 天后清除。

当操作不安全时，Eitri 不会对一个冲突动手，并说明原因：同步暂停时、或者即将重建会话时，忽略规则变更在等待时，同步没在运行时，这个冲突已经不是展示给你的那一个时，以及**这个项目里有 agent 回合或后台任务在运行**、或者 Eitri 判断不了的时候（此刻做出的选择会落在 agent 改动的中途）。如果某一个版本现在是另一种东西（目录、链接、特殊文件），或者大于 64 MiB，它也不会碰那个冲突；这种情况视图会列出路径，由你手工处理。你操作的时候如果正有一个决定在执行，它会说「sync is updating; look again」。

<a id="an-edit-beats-a-delete"></a>
### 编辑胜过删除

并非每一次分歧都是冲突。如果一侧删了一个文件，另一侧编辑了它，**编辑胜出**，文件会回到删除它的那一侧，没有冲突，也没有标记。同步工具就是这样工作的，所以 Eitri 没法把它显示成冲突；同步视图的帮助里说了这一点。你在笔记本上删掉、又回来的文件，多半是在服务器上被编辑过。

在忽略规则变更后重建会话时也是如此：一个你在一侧删掉、而另一侧仍然有的文件，会被复制回你删它的地方。Eitri 会把这些文件列出来（「`<n>` files were copied to the other side when sync was re-created; if you had deleted one, delete it again」），让你再删一次。

<a id="before-a-prompt-is-sent"></a>
## 提示词发送之前

你保存了一个文件，然后发出提示词。保存稍后才会到达服务器，而立刻发出去的提示词会让 agent 读到旧文件，你的这次编辑还会被算到 agent 那个回合的账上。所以在远程标签页上，**发送提示词或引导（steer）之前，会先等你的保存到达服务器**，最多等 `remote_dev.flush_cap` 秒（3）。等待期间输入框显示「syncing…」，这个等待不会卡住窗口。`remote_dev.send_waits_for_flush = false` 可以关掉等待。

提示词总是会发出去。如果等待超时、你保存的某个文件有冲突、被排除或遇到同步错误、或者同步没在运行，你的消息下面会有一行话，写出文件和主机，例如「your last save of `src/main.rs` has not reached `example-host`」，被排除或同步不可用时还会带上原因。**提示词本身从不被改动**：agent 收到的恰好就是你输入的内容，这一行会和消息一起留在保存的标签页里。agent 提出的改动经由你的编辑器应用时，即使发送等待已关闭，Eitri 也会等到服务器上有了文件的新内容；如果还没到，Eitri 会说「the write to `<path>` has not reached `example-host` yet; it is saved on this laptop」。如果文件超过 64 MiB，Eitri 会提示文件太大，无法确认。

<a id="permission-cards-on-a-remote-tab"></a>
## 远程标签页上的权限卡片

远程标签页的 agent 在服务器上干活，它说的路径是服务器上的路径。Eitri 的助手程序在服务器上、对着服务器的磁盘逐个检查 agent 的请求：路径是否留在项目里，路上的链接是否通到项目外面，以及项目文件夹本身算不算边界（归你所有，别的账户不能写）。然后面板就像对待一个同样情况的本地项目那样作答：

- **Manual**：项目内的读取和只读命令直接放行；编辑，以及项目外面的、或经由通出项目的链接的操作，都会问你。
- **Auto**：服务器认定项目文件夹是边界时，Eitri 把这次调用交给 Claude Code 自己的 auto 模式，和本地一样。跑不了 auto 模式的模型会把这个标签页转成 Manual。
- **Bypass**：和本地标签页一样。

Eitri 自己作答之前，会再问服务器一次（一次新的检查，最多大约三秒）。如果这期间答案变了，比如项目里的一个文件被换成了指向项目外面的链接，你会看到一张卡片。连接断开时到达的请求，等连接回来、做完新的检查之后，照样这样作答。

服务器没法为某个标签页检查路径时，这个标签页会显示「this tab runs on `example-host`; path checks on the server are not available yet」，每个文件和命令都会问你。这种情况包括：旧版 Eitri 启动的 agent，它的助手程序还在服务器上运行；你刚把标签页挪到了另一个窗口；以及窗口重新打开时回来的标签页，在 Eitri 为它检查完服务器上的项目之前（标签页回来时就会检查）。项目文件夹不是边界时会显示「No project boundary on `example-host` (...)」，每个文件和命令都会问你，本地也是这样。

<a id="sessions-on-the-server"></a>
## 服务器上的会话

远程窗口的会话选择器（`Ctrl+b w`）列出 Claude Code 在服务器上为这个项目记录的会话，最新的在前，带着标题：既有你的远程标签页启动的，也有你自己在服务器终端里用 `claude` 启动的。选中一个就接着它继续，历史从服务器上 Claude Code 自己的记录里读出来。记录还在被写入的会话（Claude Code 还在某处处理它）会被拒绝。

远程标签页上的 `@buffers`、`@quickfix` 和 `@diagnostics` 遵循服务器上的 `Read` 拒绝规则：也就是服务器上这个项目的和你账户的 Claude 设置，而不是你笔记本上的。这些设置读不到时，编辑器里的内容一样都不附上，消息里会说明原因。

<a id="syncthing-and-dropbox"></a>
## Syncthing 和 Dropbox

不要让另一个同步工具再去同步同一个文件夹。两个工具写同一棵树会互相抢：Syncthing 的 `.sync-conflict` 文件、它的 `.stfolder` 和 `.stignore` 会落进你的树里，被 agent 的 `git add -A` 提交上去。

`eitri host open` 时，Eitri 在笔记本上从项目往上走到 `/`，在服务器上也一样，找 `.stfolder`、`.dropbox` 和 `.dropbox.cache`。找到时它会停下，说出受影响的路径、其他设备上会发生什么，以及你可以在两台机器上都改用同步文件夹之外的路径。

对 Syncthing，有一种办法能让文件夹留在原处：在那个 Syncthing 文件夹的 `.stignore` 里，为这个项目或它的某一级父目录写一条锚定的、字面的排除。如果 Syncthing 同步的是 `/srv`，项目是 `/srv/project`，这一行就是

```text
/project
```

只有当 Eitri 能证明 `.stignore` 里、以及它 `#include` 的文件里，没有任何内容把项目的任何部分重新纳入时，才会接受这条排除。它读不懂、或没法确定是否匹配的例外，都算作不利。Dropbox 的标记总是需要换个位置，或者去掉重叠。这项检查在你用 `eitri host open` 打开项目时运行。

<a id="language-servers-and-neovim"></a>
## 语言服务器和 Neovim

前面几节都让你的编辑留在笔记本上。语言服务器、格式化工具、测试运行器就不一样：它们需要项目的工具链，而笔记本上未必有。所以在远程项目里，Eitri 让它们在服务器上运行，你的 Neovim 从笔记本上去和它们通信。语言服务器这部分不用改你的 Neovim 配置：lspconfig、`vim.lsp.enable` 和 `vim.lsp.start` 走的是同一条路。

<a id="language-servers-on-the-server"></a>
### 服务器上的语言服务器

在远程项目里打开一个文件，你的配置本来会在笔记本上启动的语言服务器，改成在服务器上启动：工作目录是项目目录，环境是那个账户的登录环境。诊断、补全、悬停提示等在 Neovim 里的样子和以前一样。你的缓冲区会随着你输入送到服务器，所以补全看得到你还没保存的文字。

- **语言服务器要装在服务器上**，笔记本上不用装。Eitri 在那个账户的登录 `PATH` 里找这个命令，再到那里 Mason 的目录（`~/.local/share/nvim/mason/bin`）里找。找不到时，Neovim 里的这个服务器会启动失败，并给出一条消息，例如 `rust-analyzer is not installed on example-host (looked on the login PATH and in mason)`。Eitri 这一端会马上放掉连接；Neovim 不去停它的话，大约十秒后它自己退出。
- **每个 Neovim 一个服务器。** 同一个项目开两个 Neovim，服务器上就启动两个语言服务器。
- 服务器配置里的 `cmd_env` 和 `cmd_cwd` 说的是笔记本，所以不会在服务器上生效。
- **服务器进程退出了**，Eitri 会启动一个新的，并把你打开的缓冲区重新发给它。五分钟内第二次退出，Eitri 就不再重试，并告诉你：`the language server rust-analyzer on example-host stopped twice`，后面跟着它错误输出的末尾。
- 如果服务器上还在运行的辅助程序是不认识语言服务器的旧版本，语言服务器会改在笔记本上运行。说明这一点的那一行写进 Neovim 的语言服务器日志（`vim.lsp.log.get_filename()` 给出的文件），不显示在屏幕上。它运行时不带配置里的 `cmd_env` 和 `cmd_cwd`，工作目录是 Neovim 的工作目录。重新打开这个项目的窗口，辅助程序就会更新。

<a id="what-stays-on-the-laptop"></a>
### 哪些留在笔记本上

下面这些情况，语言服务器留在笔记本上：

- 它的根目录不在任何远程项目里；
- 它的名字在 `remote_dev.lsp_local` 里。名字指它配置里的 `name`，没有 `name` 的配置没法列进去。`init.lua` 不存表，所以写成一个字符串：

  ```lua
  eitri.config.set("remote_dev.lsp_local", "lua_ls, stylua")
  ```

- 它的 `cmd` 是一个 Lua 函数。函数自己建立连接，Eitri 没法把服务器放到连接的另一头。`:checkhealth eitri` 会就此给出警告（见[普通 Neovim](#plain-neovim)）；
- 你设了 `vim.g.eitri_remote_lsp = false`（整个会话）或 `vim.b.eitri_remote_lsp = false`（单个缓冲区）。这两个是 Neovim 变量，不是 `eitri.config` 的设置项，要写在你的 Neovim 配置里。

选择是在服务器启动的那一刻做出的，所以改了设置不会把已经在运行的服务器搬走。在 Eitri 的 setup 运行之前就保存了自己那份 `vim.lsp.start` 引用的插件，以及 `vim.lsp.start_client`，都不在覆盖范围内。

<a id="the-grace"></a>
### 宽限期

合上盖子、换了网络、ssh 连接断了，都不会让语言服务器停下。服务器会把它留 `remote_dev.lsp_grace` 秒（60 到 86,400，默认 1,800，也就是 30 分钟），Neovim 自己会重新连接。宽限期内连接恢复时，Neovim 接上的是同一个服务器：你打开的缓冲区和你这段时间里做的保存会重新发过去，你不在时服务器发出的内容（诊断、进度）也会送到。**不会重新建索引。** 连接断着的时候，补全、悬停这类请求会立刻失败，不会一直等。

宽限期用完，服务器就停掉它；下一次连接会启动一个新的，重新建索引。连接还在时，关闭 Neovim 会让服务器立刻把它的语言服务器停掉；连接断着时，语言服务器到宽限期用完才停。`eitri host stop` 也会停掉语言服务器。Neovim 里对应的客户端随之退出，不会自己启动新的，直到 Neovim 再次启动它们，比如你打开项目里的一个文件。宽限期里保留的服务器会一直占用服务器的内存，直到它结束，所以内存小的机器适合设短一点：

```lua
eitri.config.set("remote_dev.lsp_grace", 600)
```

<a id="saves-reach-the-server-first"></a>
### 保存先到服务器

你保存时，Eitri 会让同步马上把这个文件送到服务器，语言服务器要等文件到了之后才会得到通知，所以在保存时运行的检查（比如 rust-analyzer 的 `cargo check`）读到的是你保存的内容。通常这大约是到服务器一个来回的时间。如果文件在大约五秒内还没到，这次保存会照样送给语言服务器，检查就可能读到旧文件；辅助程序会在服务器上那个语言服务器的日志里记一行。一次保存最多被扣住大约六秒。`eitri host sync status` 能看到同步在做什么。

<a id="gd-into-dependencies"></a>
### `gd` 跳进依赖

语言服务器还会提到同步不携带的文件：标准库、`~/.cargo` 下的某个 crate、排除目录下生成的文件。笔记本上同一路径的文件，是别的文件，或者根本没有。所以 Eitri 把这些文件打开成 `eitri-remote://example-host/<路径>` 缓冲区，内容从服务器读取：

- 缓冲区先显示 `reading from example-host…`，然后是文本。它是**只读**的：`:w` 会回答 `this file is on example-host and read-only here`。
- 文件类型由路径判断，你跳过来时用的语言服务器会接上去，所以在里面悬停、`gd` 还能继续用。`gd` 是这样，你自己打开或从面板打开的文件也是这样：你原先所在缓冲区的语言服务器如果处理这种语言，就会接上去；那个缓冲区没有的话，同一台主机上已经在为这种语言运行的服务器会接上去。只有同一台主机的语言服务器才会接到这样的缓冲区上；你的配置在笔记本上按文件类型启动的服务器不会。
- 项目里被某条忽略规则排除的文件，也这样打开，哪怕笔记本上还留着一份旧的。
- 超过 16 MiB 的文件会被拒绝，不会只显示一部分。同时最多读四个，其余的排队等。
- 想自己打开一个：`:EitriRemote open /home/user/.cargo/registry/src/example/lib.rs`（当前缓冲区所属项目的服务器上的绝对路径；在 `eitri-remote://` 缓冲区里输入时，是那个缓冲区所在主机上的路径）。在远程窗口里，面板的打开路径对项目之外的绝对路径也是这样处理。`gf` 仍是 Neovim 自己的。
- 这样的缓冲区里 `vim.b.eitri_remote` 是 `{ host, path }`，给不该把它当本地文件的插件用。

在终端里，`eitri host read example-host:/absolute/path` 会把服务器上的一个文件打印出来。

<a id="commands-on-the-server"></a>
### 服务器上的命令

格式化工具、lint 工具、测试运行器和 `make` 本来是 Neovim 在笔记本上启动的。`eitri host exec` 改成在项目的服务器上运行一条命令：

```sh
eitri host exec --cwd /srv/project -- cargo fmt
```

项目是记录里包含 `--cwd`（默认是当前目录）的那一个，命令在它的服务器上的同一个路径里运行。Eitri 在启动前把你的保存刷到服务器，结束时把命令改动的内容取回来，所以原地改写文件的格式化工具，命令返回时结果已经在笔记本上了（缓冲区的 `:checktime` 看得到）。`--no-flush` 两头都不做。如果刷新失败，命令照样运行，并在 stderr 上给出一行，说明你最近的保存可能还没到主机（或命令的改动可能还没回到笔记本）。标准输入会交给命令，除非你给了 `--no-stdin`，或者它是个终端，所以从 stdin 读、往 stdout 写的格式化工具能用。

命令得到的是服务器账户的登录环境，加上 `TERM=dumb`，笔记本上的东西一概没有。没有谁能向它提问：要开编辑器或者要输入密码时，它会直接失败。程序要么是登录 `PATH` 里找得到的裸名字，要么是绝对路径；否则你会看到这一行：“`<name>` is not on the login PATH of this account”。

你会遇到的：

- **退出码。** 命令自己的退出码；信号 `n` 结束它时是 `128 + n`。**125** 表示什么都没运行：目录不在任何远程项目里、服务器拒绝了，或者它的辅助程序比这个 Eitri 旧（打开项目的窗口来更新它）。**255** 表示连不上服务器，或者结果未知，因为命令启动后连接丢了；再运行之前先到服务器上看看。Eitri 自己的行写到 stderr，以 `eitri host exec:` 开头。它用 125 而不是 1，这样 lint 工具“发现问题”的退出码不会被当成 Eitri 出了错。
- **命令运行 1 小时后会被杀掉。** 在笔记本上中断 `eitri host exec`，停不了一条输入已经关闭的命令（Neovim 的包装会立刻关闭输入）；只有这个时限能停它。
- 不管 `remote_dev.enabled` 开没开，只要是你打开过的项目，它都能用。

<a id="exec-wrappers-for-plugins"></a>
### 给插件用的 exec 包装

`require("eitri.remote").cmd(argv, opts)` 接收一个参数列表；目录在远程项目里时，返回包在 `eitri host exec` 里的版本，否则原样返回，所以同一份配置本地项目和远程项目都能用。`opts` 里有 `cwd`（默认是当前缓冲区的目录）、`flush = false`（相当于 `--no-flush`）和 `stdin = true`（给要读标准输入的命令）。设了 `stdin = true` 的命令读的是缓冲区、答案写到标准输出，所以两次 flush 也一并跳过（每次都要到服务器走一个来回）；如果它还要读写项目里的文件，再加上 `flush = true`。各插件的一行写法：

- **conform.nvim。** 把 `command` 和 `args` 写成包装后列表的函数。格式化工具如果从标准输入读缓冲区，就给 `cmd` 传 `stdin = true`，格式化工具上也设 `stdin = true`：

  ```lua
  local function on_server(argv)
    return require("eitri.remote").cmd(argv, { stdin = true })
  end
  local rustfmt = { "rustfmt", "--edition", "2021" }
  require("conform").formatters.rustfmt = {
    inherit = false,
    command = function() return on_server(rustfmt)[1] end,
    args = function() return vim.list_slice(on_server(rustfmt), 2) end,
    stdin = true,
  }
  ```

- **nvim-lint。** `linters.<name>.cmd` 是一个函数，返回 `cmd{ ... }` 的第一个词，`args` 是其余的部分。设了 `stdin = true` 的 linter 从标准输入读缓冲区，所以要用 `cmd(argv, { stdin = true })` 生成列表：默认包装出来的命令带 `--no-stdin`，读到的是空输入。
- **neotest。** 适配器里接收命令的选项，传 `cmd{ ... }`。
- **`:make`。** `:EitriRemote makeprg cargo build` 把这个缓冲区的 `makeprg` 设成在服务器上、在缓冲区所在目录里运行这条命令（不给命令就是 `make`）；之后 `:make` 照常填充 quickfix 列表。`errorformat` 不动：输出里的路径是服务器上的路径，和镜像的路径一致。在远程项目之外，`:EitriRemote makeprg` 会说明这一点，什么都不改。

如果列表是在没有当前缓冲区的时候建好的（nvim-lint 的 `args`、neotest 的某个选项），请自己传 `cwd`。

<a id="plain-neovim"></a>
### 普通 Neovim

这一切都不需要 Eitri 窗口。在普通终端里的 Neovim，需要三样东西：

1. `eitri.nvim` 插件在它的 `runtimepath` 上（`vim.opt.runtimepath:append("/usr/share/eitri/nvim/eitri.nvim")`；其他安装路径见 `:help eitri-install`）。
2. 可选，用于上面那两个窗口会从 `init.lua` 里读的设置：

   ```lua
   require("eitri.remote").setup({
     lsp = { grace = 1800, local_servers = { "lua_ls" } },
   })
   ```

   不写就用默认值：宽限期 1,800 秒，没有任何服务器留在本地。超出 60 到 86,400 的宽限期会被收到这个范围内。
3. 在 shell 里把项目打开一次：`eitri host open example-host:/srv/project`（要求 Eitri 的 `init.lua` 里 `remote_dev.enabled` 是开的）。

之后，项目本地副本下的文件，和在窗口里一样：语言服务器在服务器上，可以 `gd` 进依赖，也有 exec 包装和同步监视。同步监视每次启动都会从 Eitri 的 `init.lua` 读 `remote_dev.enabled`：如果这个 Neovim 看到的 `init.lua` 里没开（或者根本没有 `init.lua`），监视会说 "remote development is off"，这时只有语言服务器和 exec 包装能用。`:help eitri-plain-remote` 说的是同样的内容。

`:checkhealth eitri` 里有一节针对你输入它时所在的缓冲区。在远程项目之外，它只有一行。在项目里，它会显示项目（主机和根目录）、`eitri` 程序及其版本、`remote_dev.enabled` 开没开、附在这个缓冲区上的每个语言服务器是**远程**、**本地且已固定**、**因为 `cmd` 是函数而在本地**（警告）还是普通的本地服务器，以及 Mutagen 及其版本（低于 0.18.0 或找不到时给出警告）。

<a id="a-server-shell-in-neovim"></a>
### Neovim 里的服务器 shell

`:EitriRemote term` 在新标签页的终端缓冲区里，打开项目服务器上的一个 shell，目录是当前缓冲区的目录（服务器上的同一路径）。它的样子和用 ssh 时一样：`vim`、作业控制、调整大小都能用。在远程项目之外不会打开任何东西。它运行的是 `eitri host term`，你也可以在任何终端里运行它。

- 清除这个缓冲区时，会请服务器关掉 shell，但**只是尽力而为**：如果服务器在大约一秒半内没有回应，shell 会继续运行，`eitri host list` 能看到它。
- 连接丢了，shell 会继续在服务器上运行，终端会自己重新连接，最长两分钟；之后它放弃，并说明 shell 仍在运行。
- 它和窗口里的那些终端一样，是一个服务器终端；关闭一个会结束什么，见[服务器终端](#server-terminals)。

<a id="what-is-not-in-this-version"></a>
## 这个版本里没有的

- **macOS 笔记本。** 镜像的排除规则按名字精确匹配，而 macOS 的文件系统默认不区分大小写；在这一点解决之前，请使用 Linux 笔记本。
- **以工作树或子模块检出作为项目根目录时的 git。** 不做镜像；git 在服务器上运行。
- **项目里某个子模块之内的 git。** 副本里没有子模块的历史，所以在子模块文件夹里运行的 git 读操作描述的是外层仓库；子模块的 git 请在服务器的终端里运行。
- **垫片列为拒绝的 git 命令，以及经由 git 的交互式编辑器流程**（见上文）。
- **本地副本上的 agent 标签页。** 用 `--local` 打开的窗口显示的是副本；agent 在服务器上运行。
- **远程标签页上的回合审阅。** 远程标签页的回合审阅会说明它还不可用。
- **稀疏编辑和远程 Neovim。** 镜像的是整棵未被忽略的树，Neovim 在你的笔记本上运行。

其他今天不支持或比较粗糙的地方，见[已知问题](../known-issues.zh-CN.md)。
