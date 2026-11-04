[English](companion.md) | 简体中文
<!-- translated-from: companion.md sha256=df74caddb4fa5948ff82bda101e6da822d93209292a71b6636bf27cb8db19896 -->

# 配合你自己的 Neovim 使用

Eitri 也可以只运行 agent 面板：一个独立的窗口，放在你平时用的 Neovim 旁边，可以是在终端里（在 tmux 里或不在都行），也可以是上游 Neovide 或其他任何 Neovim 图形前端。这一页讲怎样启动它、焦点怎样在两个窗口之间移动，Markdown 预览这样的插件页面怎样单独开在你的 Neovim 旁边，以及每种桌面需要什么。直接运行 `eitri` 得到的窗口在[快速上手](getting-started.zh-CN.md)里讲；面板自己的按键在两种方式下是一样的，[按键](keys.zh-CN.md)里列出了它们。

面板通过 RPC socket 附着到你的 Neovim 上，并在里面装一小段胶水代码：你打开的文件和 Visual 选区会送给 agent；面板取用你的配色方案并显示你的 which-key 按键；agent 工作期间缓冲区会重新加载（见[缓冲区重新加载](#buffers-reload)）；可以从面板里按行号打开文件；`Ctrl+g` 在 Neovim 里编辑草稿；Visual 模式里按 `<leader>ae`，带着选区把你送到面板的输入框，里面已有 `Edit the selection: ` 等你接着写（窗口管理器能被要求把面板带到前面的地方才行；其它地方用桌面自己的窗口键）。面板一消失就把这些全部撤掉。你的 Neovim 配置里不会被写入任何东西。编辑器保持它自己的速度和按键，两个窗口怎么摆由你的窗口管理器决定。

在 Hyprland、sway 或 niri 上，普通的 `eitri` 也会把它的聊天和页面放进各自的窗口，不过围绕的是 Eitri 的编辑器，而不是你的：见[合成器窗口](compositor-windows.zh-CN.md)。

<a id="the-eitrinvim-plugin"></a>
## eitri.nvim 插件

`eitri.nvim` 是一个很薄的启动器：面板在 Neovim 里需要的一切都由面板自己安装，所以插件和已安装的 Eitri 的版本永远不必一致。软件包（`.deb`、`.rpm`、AUR）把它放在 `/usr/share/eitri/nvim/eitri.nvim`；tarball 安装器把它放在 `~/.local/share/eitri/eitri.nvim`（设置了 `$XDG_DATA_HOME/eitri/` 时就在它下面）。已安装文件的其余部分见[文件都装到哪里去了](../../INSTALL.zh-CN.md#where-things-go)。用 lazy.nvim 的话，把一个 spec 指向这个目录：

```lua
-- .deb, .rpm, AUR
{ dir = "/usr/share/eitri/nvim/eitri.nvim", cmd = { "EitriPanel", "EitriPage" } },

-- the tarball installer (install.sh)
{ dir = vim.fn.expand("~/.local/share/eitri/eitri.nvim"), cmd = { "EitriPanel", "EitriPage" } },
```

不用插件管理器的话，把这个目录加进 `runtimepath`：`set runtimepath+=/usr/share/eitri/nvim/eitri.nvim`。

调用 `require("eitri").setup({ ... })` 是可选的。`mapping = "<leader>ep"` 把一个 normal 模式的键绑定到这个命令上，`page_mappings = { ["<leader>em"] = "markdown" }` 把 normal 模式的键绑定到[开在你 nvim 旁边的页面](#pages-beside-your-nvim)（默认一个也没有），`cmd = "/path/to/eitri"` 在启动器不在 Neovim 的 `PATH` 上时指明它。Neovim 里的 `:help eitri.nvim` 有同样的内容，`:checkhealth eitri` 会找到程序、打印它的版本并列出页面 id。你用上某个命令之前，Eitri 的任何东西都不会在你的 Neovim 里运行。lazy.nvim 的 spec 如果只写了 `EitriPanel`（这一页以前就是这样写的），在你打开过一次面板之前 `:EitriPage` 都不存在：像上面那样把两个命令都写上。

`:EitriPanel` 为当前工作目录打开面板，`:EitriPanel ~/some/project` 为另一个目录打开。如果 Neovim 还没有服务器地址，插件会启动一个。在胶水代码装好之前，面板的状态条会写 `attaching…`，而且没有任何超时：等你完成一个未完成的按键或提示之后，Neovim 才会回应这个请求，状态条随后会说明这一点（`attaching… (nvim is waiting for a key)`）。

<a id="eitri-panel"></a>
## eitri panel

你也可以从 shell 启动面板：

```sh
eitri panel [--nvim <addr>] [DIR]
```

`--nvim` 是要附着的 Neovim 的地址（`:echo v:servername` 会显示它）：一个由你自己拥有的 Unix socket 的路径。它默认取 `$NVIM`，Neovim 会为它的 `:terminal` 和 `jobstart()` 子进程设置这个变量；TCP 地址（`host:port`）会被拒绝。没有地址时，面板以未附着的状态启动，并在状态条里说明；之后在 Neovim 里运行 `:EitriPanel` 就会附着上。`DIR` 是项目，解析方式和 `eitri DIR` 完全一样。`--account` 和 `--quiet` 的作用和对 `eitri` 一样；`--clean` 和 `--legacy` 不适用，会被拒绝。

每个项目只有一个面板。第二次 `:EitriPanel` 会把正在运行的面板窗口带到前面，而不是再开一个。如果是从面板已经附着的那个 Neovim 里运行，它就只做这个；如果是从同一个项目里的另一个 Neovim 里运行，面板就改为附着到那一个，第一个 Neovim 里的胶水代码会被撤掉。如果 Neovim 退出了，面板会保留它的会话，状态条写 `editor detached: run :EitriPanel to attach again`。状态条的其他文字是 `no editor attached: run :EitriPanel in nvim`、`editor detached: another Eitri panel attached to it` 和 `could not attach: <reason>`。

面板窗口的应用 id 是 `cn.huntergrey.eitri.Panel`，标题是 `Eitri · <项目目录名>`，所以窗口规则可以把它挑出来。它读取和单窗口模式相同的 `$XDG_CONFIG_HOME/eitri/init.lua`（默认 `~/.config/eitri/init.lua`）（`agent.account` 适用）；在那里注册的 Lua 面板和命令不会显示，面板的输出里会有一行说有多少个被略去。它没有编辑器、没有底部终端，也没有自己的布局：标签页键、`r`、`?`、`:`、文字大小键、`f`、发送前缀本身，以及方向键（朝相邻的窗口）在前缀下可用，布局键或窗口键在一个窗口里第一次按下时回答 `not here: your window manager arranges this window`（之后再按就不说话了）。你在 `init.lua` 里用 `eitri.plugin.load("markdown", { key = "b" })` 给某个插件页面设的键，会为已附着的 Neovim 打开或提起[那个页面自己的窗口](#pages-beside-your-nvim)；没有附着的 Neovim 时，它会说 `no editor attached: run :EitriPanel in nvim`。

### 在两个窗口之间移动

在 Neovim 的普通和可视模式里，处在 Neovim 自己窗口的边缘时，`Ctrl+h/j/k/l` 会把这次移动交给面板，面板再请求你的窗口管理器去聚焦相邻的窗口。只有在你的配置把这些键留空、或者留在 Neovim 自己的默认（或者像 LazyVim 那样的普通窗口移动）时，Eitri 才会绑定其中的某个键，所以你自己映射过的键仍然是你的。插入模式里只绑定了 `Ctrl+l`（它先退出插入模式）；`Ctrl+h`、`Ctrl+j`、`Ctrl+k` 在那里保持插入模式原有的含义。

在面板里：

| 按键 | 发生什么 |
|---|---|
| `Ctrl+h`、`Ctrl+l` | 总是离开窗口：由窗口管理器移动焦点 |
| `Ctrl+k` | 在 BROWSE 里离开窗口；在 INSERT 里切换到 BROWSE |
| `Ctrl+j` | 在 INSERT 里离开窗口；在 BROWSE 里切换到 INSERT |
| INSERT 里的 `Ctrl+g` | 在你的 Neovim 的一个临时缓冲区里编辑草稿（没有附着的编辑器时，它会说编辑器没有连接） |
| 前缀里的 `Select` 键 | 和 `Ctrl+h/j/k/l` 一样的由窗口管理器移动焦点 |

从面板里打开一个文件，会在你的 Neovim 里打开它，并把编辑器的窗口带到前面（在 tmux 里除外，见下）。第二次 `:EitriPanel` 会把面板带到前面。

**右边有分屏时的 `Ctrl+l`。** 这个键先在 Neovim 里面移动：右边还有别的 Neovim 窗口时就去那里，只有到了最右边的边缘才去面板（另外三个方向同理）。这在 Normal 和 Visual 模式里成立，Insert 模式里只对 `Ctrl+l` 成立。Eitri 接管了 Neovim 自己默认的 `Ctrl+l` 的地方，这个键不再清除高亮并重绘，和 vim-tmux-navigator 一样；想把它找回来，就把 `:nohlsearch | diffupdate | redraw` 映射到你自己的某个键上。

**自己手工映射的键。** 如果你不通过导航插件、而是自己映射一个键，先执行窗口移动，只有它没移动时才把这次移动交给面板，因为 `require("eitri").edge` 不看 Neovim 的窗口：

```lua
vim.keymap.set("n", "<C-l>", function()
  local before = vim.fn.winnr()
  vim.cmd.wincmd("l")
  if vim.fn.winnr() == before then
    require("eitri").edge("right")
  end
end)
```

<a id="navigator-plugins-and-tmux"></a>
### 导航插件和 tmux

Neovim 在 tmux 外面时，vim-tmux-navigator 什么都不用做：面板附着期间，它的 `TmuxNavigate` 映射被当作普通的窗口移动，所以在 Neovim 的边缘，`Ctrl+h/j/k/l` 会越过去到面板。

用 smart-splits.nvim 的话，在它的 `at_edge` 钩子里把越过边缘的移动交给面板。`require("eitri").edge` 在面板接下了这次移动时返回 `true`，没有附着的面板时返回 `false`：

```lua
require("smart-splits").setup({
  at_edge = function(ctx)
    if not require("eitri").edge(ctx.direction) then
      -- no panel attached: your own fallback, or nothing
    end
  end,
})
```

同一段 `at_edge` 配置在 Eitri 自己的窗口里也能用，那里 `require("eitri").edge` 把移动交给相邻的模块（见[按键](keys.zh-CN.md#moving-between-panes)），所以一套配置两边都管用。

Neovim 在 tmux 里运行时，Neovim 的环境里什么都不会变，它的导航映射不会被动，你的 tmux 设置还是照常在 tmux 窗格之间移动。tmux 窗格的边缘归 tmux。所以用 vim-tmux-navigator 时，没有越过去到面板窗口的办法（用 tmux 一侧的绑定，或你的窗口管理器的按键）；用 smart-splits.nvim 时，上面的 `at_edge` 钩子会把 tmux 自己的边缘处的移动交给面板。这两种都还没有在真实硬件上试过（见[已知问题](../known-issues.zh-CN.md#companion-mode)）。从面板里打开文件时，编辑器的窗口不会被带到前面，因为在 tmux 里，进程树通向的是 tmux 服务器，而不是终端。tmux 里的 Neovim 也只有在你的 `tmux.conf` 里写了 `set -g focus-events on` 时才收得到焦点变化；没有它，下面讲的聚焦时重新加载就不会发生。

<a id="buffers-reload"></a>
### 缓冲区重新加载

agent 工作期间（一个回合正在运行，或者它启动的后台任务还在跑），Neovim 每秒检查一次磁盘上的文件有没有变，并重新加载你没有动过的缓冲区。每个回合结束时、面板分离或关闭时（所以回合中途被杀掉的面板写下的最后一笔也会被重新加载）、以及 Neovim 重新得到焦点时，也各检查一次。除此之外不做任何轮询。你自己改过的缓冲区不会被动，只会出现 Neovim 自己的警告。有两种情况没有覆盖：你在回合中途用另一个面板换掉了前一个面板时，前一个面板的回合之后写下的文件，要等新面板下一次忙起来、聚焦时或者手动才会重新加载；面板被杀掉之后，agent 启动的、仍在继续写文件的程序，同样不会被重新加载。

<a id="pages-beside-your-nvim"></a>
## 开在你 nvim 旁边的页面

Markdown 预览这样的插件页面，也可以单独成为一个窗口，开在你的 Neovim 旁边，不带 agent 面板。它跟随的是启动它的那个 Neovim 的缓冲区。从 shell 或从 Neovim 里启动：

```sh
eitri page <id> [--nvim <addr>] [DIR]
eitri page --list
```

```vim
:EitriPage markdown
:EitriPage markdown ~/some/project
```

- `--nvim` 和 `DIR` 与 `eitri panel` 一样：`--nvim` 默认取 `$NVIM`，没有设置时必须给出，是一个由你拥有的 Unix socket 的路径；`DIR` 是项目。`--account`、`--quiet`、`--clean`、`--legacy` 和 `--new-window` 会被拒绝。`:EitriPage {id} [dir]` 先补全插件 id，再补全目录；`require("eitri").page("markdown")` 是给你自己的某个键调用的。`eitri page --list` 每行打印一个可以这样打开的 id。
- 输入这条命令就是允许加载那个插件：按 id 加载，用它的默认设置，除非你的 `init.lua` 带选项加载了它（`eitri.plugin.load("markdown", { ... })`），那时用的就是这些选项。`init.lua` 加载的其他插件不会在这个窗口里构建（它的日志里有一行会说有几个）。它没有 Lua 面板和命令。你给另一个页面设的键，会打开或提起那个页面的窗口。
- 只有要求的能力不超出下面这些的插件才能这样打开：跟随编辑器的缓冲区（`editor.follow`）、定义 Neovim 命令（`editor.commands`）、打开文件（`editor.open`）、保存状态，以及访问网络。需要更多的插件会说明这一点，并且什么都不打开：`eitri page: <id> needs Eitri's own window (it declares <capability>)`，退出码 64。Markdown 预览符合条件。
- 只有插件在插件搜索路径上时，页面窗口才会启动（`eitri page: no plugin "<id>" on the plugin search path`，退出码 1）。`eitri page: could not attach: <reason>`（退出码 1）表示没能连上你的 Neovim。
- 每个 Neovim 和每个插件只有一个页面窗口：同一个 Neovim 里第二次 `:EitriPage markdown` 会提起正在运行的那个窗口。从另一个 Neovim 运行同一条命令，会打开第二个窗口，跟随那一个 Neovim。
- 你也可以从 agent 窗口打开页面：前缀再加上页面的键。在页面窗口里，前缀加 `a` 会提起项目的 agent 窗口，没有在运行时就为这个 Neovim 启动一个；附着在另一个 Neovim 上的 agent 窗口会保持它原来的那个。前缀加 `e` 会提起装着你的 Neovim 的窗口。

**它在你的 Neovim 里放了什么。** 一个它自己的小槽位（`_G.__eitri_pages`）：页面的配色跟随你的配色方案，插件自己的命令只在窗口存在期间存在（Markdown 的是 `:EitriMarkdown`、`:EitriMarkdownToggle`、`:EitriMarkdownHide`）。它不定义按键，不映射 `Ctrl+h/j/k/l`，不创建临时缓冲区，不做重新加载，也从不替换你自己的同名命令。agent 窗口的胶水代码和每个页面的槽位互相独立：关掉或杀掉其中一个，只会撤掉那一个，别的不动；agent 的 `Ctrl+g` 草稿也从不会显示在预览里。

**它怎样结束。**
- 用窗口管理器、前缀加 `x`，或者 `:EitriMarkdownHide` / `:EitriMarkdownToggle` 关掉它：它不问就关，它的槽位也从你的 Neovim 里消失。
- 在 Neovim 里 `:qa` 会关掉页面窗口（退出码 0，日志里有 `[page] the editor went away; closing`）；agent 窗口留着，显示 `editor detached`。
- 窗口被杀掉时，它的槽位会在一两秒内从你的 Neovim 里消失；卡在提示上的 Neovim 会在再次响应时把它撤掉。

**在这里隐藏和切换是什么意思。** 插件的命令住在槽位里，槽位随窗口一起走。所以 `:EitriMarkdownHide` 或 `:EitriMarkdownToggle` 之后窗口就关了，`:EitriMarkdownToggle` 没法把它带回来：`:EitriMarkdown` 会以 Neovim 自己的 `E492: Not an editor command` 失败。`:EitriPage markdown` 会再次打开它。窗口开着时，`:EitriMarkdown` 把它提到前面；设了 `auto = true`，预览跟随你进入的缓冲区，不会抢焦点，也不会跑到前面。

**页面窗口里的按键。** 页面自己的按键和在任何窗口里一样（页面里的 `?` 会列出它们；`<prefix> ?` 会说 `the page's own ? lists its keys`）。在标题上按 `Enter`，会把你的 Neovim 的光标移到那里，并把编辑器的窗口带到前面。

| 按键 | 发生什么 |
|---|---|
| 布局、窗口、工作区或浮动键 | 第一次是 `not here: your window manager arranges this window`，之后什么都不说 |
| 聊天键（标签页键、`r`、`:`） | 第一次是 `no chat here: press the prefix and a to open the agent` |
| 前缀加 `e` | 提起你的 Neovim 所在的窗口；在 tmux 里会说一次 `your nvim runs inside tmux: use your window manager's keys to reach it` |
| 前缀加 `a` | 提起 agent 窗口，或者启动一个 |
| 前缀加 `x`、文字大小、`f` | 和任何模块窗口里一样 |
| 越过页面边缘的 `Ctrl+h/j/k/l` | 由你的窗口管理器移动焦点 |

**哪里会被拒绝。** 在 Eitri 自己的 Neovim 里，它自己打开页面：`this window opens pages itself: press the prefix and the page's key`。在被 Eitri 接管的 Neovim 里：`this nvim is open in an Eitri window: its pages open there`（退出码 64，没有窗口）。

窗口的应用 id 是 `cn.huntergrey.eitri.Page`，标题是 `<项目> — <插件标题> · Eitri`，所以一条窗口规则就能覆盖它。Eitri 不移动任何东西：窗口由窗口管理器摆放。从 Neovim 启动时，它的输出写到 `$XDG_STATE_HOME/eitri/companion/page-<16 位十六进制>.log`（默认 `~/.local/state/eitri/companion/`）。

<a id="two-windows-from-one-command-eitri-split"></a>
## 一条命令开两个窗口：eitri split

```sh
eitri split [DIR]
```

打开上游 Neovide 作为编辑器，并把 agent 面板作为第二个窗口，两者互相附着，不需要任何别的设置。它接受项目（`DIR`，解析方式和 `eitri DIR` 一样）以及 `--account`/`--quiet`，没有其他选项。**它需要 Neovide**，Eitri 不捆绑它：要么是 `PATH` 上的 `neovide`，要么是 `EITRI_NEOVIDE` 指向的那个文件（名字不存在是错误，不会回退）。它自己运行 Neovide，并让 Neovim 监听一个私有 socket，所以你的 `init.lua` 和插件照常加载；它不是单窗口的 `eitri` 用来绘制的那个 Neovide fork。

关闭由 `eitri split` 启动的那个 Neovide，面板也会一起关闭（有回合还在运行时，和任何一次关闭一样会先问你）；只关面板则 Neovide 保持打开，因为那是你的编辑器，在里面运行 `:EitriPanel` 就能把面板找回来。两种情况下会话都会保留（除非 `agent.restore` 是 `"off"`），下一次对该项目运行 `eitri split` 时，会在空标签页上把它们作为 `Restore last session`（`s`）提供出来；设了 `agent.restore = "auto"` 的话，它们在启动时不用按任何键就回来。上面讲的一切，包括焦点键，都适用于它打开的面板。

Neovide 在你启动 `eitri split` 的那个 shell 的前台运行，和 Neovide 默认的行为一样：在那里按 `Ctrl+C`，或者关掉那个终端，会结束 Neovide 和面板。如果这个项目已经有面板在运行，这一条不成立：`eitri split` 会把那个面板附着过来，立即返回，Neovide 留在你的提示符后面继续运行，所以在提示符处按 `Ctrl+C` 什么也结束不了。想让它们比终端活得更久，就让它脱离终端启动（`setsid eitri split DIR`，或者从启动器里启动）。只有 `eitri split` 附着的那个面板会随它的 Neovide 一起关闭：你之后用 `:EitriPanel` 带回来的面板不会，而在另一个 Neovim 里运行 `:EitriPanel` 会把面板移到那里，并结束这层关联。

如果这个项目已经有一个面板在运行，`eitri split` 会把它附着到新的 Neovide 上，而不是再开一个。

## 窗口管理器

由哪个窗口管理器来移动焦点，是从会话里检测出来的；要强制指定一个，或者把它关掉，把下面这个写进 `$XDG_CONFIG_HOME/eitri/init.lua`（默认 `~/.config/eitri/init.lua`；其他任何值都会让面板在启动时停下，并点出这个键）：

```lua
eitri.config.set("companion.wm", "auto")   -- "auto" (the default), "hyprland", "sway", "niri", "gnome" or "none"
```

`"none"` 从不向窗口管理器请求任何东西。别的 `init.lua` 设置见[配置](configuration.zh-CN.md)。

| 桌面 | 通过什么检测 | Eitri 做什么 |
|---|---|---|
| Hyprland | `HYPRLAND_INSTANCE_SIGNATURE` | 用 `hyprctl dispatch` 移动焦点，之前先确认那个方向确实有可见的窗口，因为 Hyprland 自己的移动在那个方向什么都没有时也会挑一个窗口：到了真正的边缘，按键就停在那里。互相重叠的窗口（比如浮动窗口）交给 Hyprland 决定。Eitri 先问一次 Hyprland 用的是 Lua 配置（`hyprland.lua`）还是 `hyprland.conf`，再按那种配置的写法发命令 |
| sway | `SWAYSOCK` | 用 `swaymsg` 移动焦点，会先确认那个方向上确实有一个可见的窗口（在任何输出上），所以 sway 默认的焦点环绕不会把你带到远端。到了边缘，这个键被吞掉 |
| niri | `NIRI_SOCKET` | 用 `niri msg action` 移动焦点；到了边缘会怎样，由 niri 自己决定 |
| GNOME | `XDG_CURRENT_DESKTOP` 含有 `GNOME`，且是 Wayland 会话 | 通过 [Eitri GNOME Shell 扩展](#gnome-the-extension)移动焦点，前提是你已经启用了它。没有它就不移动焦点（在 GNOME 上 Wayland 客户端没法自己抢到焦点），状态条会提示一次：你的桌面不允许 Eitri 移动焦点，并指出这个扩展 |
| KDE，其他任何桌面 | 以上都不是 | 不移动焦点：Wayland 客户端在那里没法抢到焦点。状态条会提示一次你的桌面不允许 Eitri 移动焦点；用桌面自己的窗口按键 |

检查按这个顺序进行：如果同时设了 `SWAYSOCK` 和 GNOME 桌面，sway 优先。Xorg 上的 GNOME 会话不会被检测为 GNOME；`init.lua` 里的 `"gnome"` 可以强制指定，不过这个扩展从不在 X11 窗口之间移动焦点。

不在屏幕上的窗口（另一个工作区、一个隐藏的标签）永远不会成为方向性移动的目标邻居。

<a id="gnome-the-extension"></a>
## GNOME：扩展

在 GNOME 上，程序不能自己取得焦点，所以要用 `Ctrl+h/j/k/l` 在面板和它的编辑器之间移动，需要一个小的 GNOME Shell 扩展 `eitri@huntergrey.cn`（GNOME Shell 45 到 51）。`.deb`、`.rpm`、AUR 软件包和 tarball 安装器会把它的四个文件放到位（[文件都装到哪里去了](../../INSTALL.zh-CN.md#where-things-go)）；**启用它是你的一步**，安装器从不替你做：

```sh
gnome-extensions enable eitri@huntergrey.cn
```

在 Wayland 上，shell 只读取它在登录时找到的扩展，所以在你已登录时装上的扩展，要到你下次登录才开始起作用。在那之前，以及没有启用它时，面板的表现和任何没有焦点支持的桌面一样：不移动焦点，状态条会说明一次。

它做什么，不做什么。只有在你刚在拥有焦点的窗口里按了键或点了鼠标之后，它才移动键盘焦点，而且只有当那个窗口属于发出请求的程序，或者属于那个程序指名的作为它伙伴的编辑器时才行。面板就是这样把焦点从自己移到邻居，或者移回它的编辑器，以及从它的编辑器移到它自己。你的会话总线上的任何程序都可以按同样的规则请求它，所以后台的一个程序最多能在你刚在它指名的窗口里打过字之后，把焦点取到它自己的窗口（面板做的就是这个），并且在拿到焦点之后，把它交给邻居，或交回你原来所在的窗口。它不报告标题、几何信息或进程 id。

值得知道的限制：

- 焦点键（`Ctrl+h/j/k/l`）只移到和当前聚焦窗口在同一个显示器上的窗口；从面板里打开一个审阅文件，会把编辑器带到前面，不管它在哪里；
- X11 会话里的窗口或 XWayland 程序的窗口永远不算数，因为 X11 程序自己写自己的按键时间：请把编辑器当作 Wayland 窗口运行；
- 所有窗口都跑在同一个进程里的终端（GNOME Terminal、Ptyxis）算作一个伙伴，所以把焦点交回编辑器时，会落到它最近使用的那个窗口；
- 在上一次移动之后大约三分之一秒内再移回去，会按设计被拒绝；
- 它会把你最近一次亲手把焦点放进面板（点击、Alt+Tab）的时间记住 30 分钟；用焦点键或 `:EitriPanel` 移进面板，或者面板打开时自己拿到焦点，都不算。所以在面板刚打开时、超过这 30 分钟后，以及每次锁屏之后，从编辑器移进面板的焦点键和 `:EitriPanel` 都不起作用，直到你点一下面板（如果焦点跟随鼠标，就改用 Alt+Tab 或概览切换到它：只移动指针不算）；
- 只有当面板的焦点来自你或来自编辑器（焦点键、`:EitriPanel`）时，草稿、`gf` 或打开的审阅文件才会把编辑器带到前面；面板自己拿到焦点时（刚打开、它自己的某个窗口关闭之后、锁屏之后），这一步不起作用，直到你亲手把焦点放进面板：先点别的窗口再点面板，或者用 Alt+Tab 切走再切回来（面板已经有焦点时再点它，什么也不会改变）；
- 被转发的 `eitri split` 会让正在运行的面板附着上，但不会把面板带到前面（它新开的 Neovide 还没收到过按键）。
