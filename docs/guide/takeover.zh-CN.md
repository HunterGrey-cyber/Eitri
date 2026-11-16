[English](takeover.md) | 简体中文
<!-- translated-from: takeover.md sha256=0050743dc56bebda2d5572cefe8d60f6eae2eb87de04e9d5b3b3cd0cc074e4f1 -->

# 接管终端里的 nvim

如果你有一个 Neovim 正在终端里运行，里面开着文件、有没保存的修改、跑着任务或者开着 `:terminal`，Eitri 可以把这个 Neovim 原样搬进它自己的窗口，窗口结束时再还给终端。什么都不会重启。你接管的这个 Neovim 就是窗口里的编辑器，旁边是 agent 面板，下面是底部终端；它原来所在的终端停在一个小小的等候画面上，直到 Neovim 被还回来。如果你现在在 tmux 里工作，[从 tmux 迁过来](migrate-from-tmux.zh-CN.md)讲了整个迁移过程，也包括这种用法。

## 面板放在 nvim 旁边，还是把 nvim 搬进来

两种方式都把 agent 放到你已经在用的 Neovim 旁边，适合不同的日子。

- **[面板放在 nvim 旁边](companion.zh-CN.md)**：你的 Neovim 留在你的终端里，原地不动，面板是附着到它上面的第二个窗口。你喜欢用自己的终端、tmux 或窗口管理器来排布时，用这种。
- **把 nvim 搬进来**：你的 Neovim 进到 Eitri 的窗口里当编辑器，由 Eitri 的渲染器绘制（平滑滚动、浮动窗口效果、窗口的网格），和面板、终端一起平铺在一个窗口里。你想要整个窗口，而且想要的是你正在用的这个 Neovim、不是新开一个时，用这种。

两种可以来回换：窗口结束时你的 Neovim 回到终端，你可以再运行 `:EitriOpen`。

## 怎样开始

在你想搬的那个 Neovim 里输入 `:EitriOpen`。这个 Neovim 需要是 **0.12 或更新**；低于这个版本，它只打印一行话，接管的任何部分都不会加载。Eitri 的其他部分不受影响，Eitri 自己启动的编辑器仍然用它自己更低的版本下限。

```vim
:EitriOpen
:EitriOpen ~/some/project
```

`:EitriOpen` 可以带一个项目目录。不带时，项目就是这个 Neovim 自己的工作目录（`getcwd(-1, -1)`，即 `:pwd` 为整个会话显示的那个）；之后的 `:cd` 不会移动窗口。在 shell 里同样的事是 `eitri open --nvim <地址> [DIR]`，其中 `--nvim` 是这个 Neovim 的服务器地址（`:echo v:servername`）。`eitri open` 总是打开它自己的窗口，也从不读取 `$NVIM`。

命令由 [eitri.nvim](companion.zh-CN.md#the-eitrinvim-plugin) 提供。用 lazy.nvim 的话，把 `"EitriOpen"` 加进配置里 `cmd` 列表，放在 `"EitriPanel"` 和 `"EitriPage"` 旁边。

下面这些情况接管会被拒绝：`:EitriOpen` 显示一行话，终端保持原样：

- Neovim 比 0.12 旧；
- 它显示的 UI 不止一个，或者那个 UI 不是终端（Neovide 窗口、nvim-qt、别的 `--remote-ui` 客户端）；
- 它在等一个按键，或者忙得半秒内答不上来（按 `Esc` 再试一次）；
- 它已经在一个 Eitri 窗口里了；
- 你的 `init.lua` 里 `editor.takeover` 是关的（见[关掉它](#turning-it-off)）；
- 没有显示可以开窗口，或者服务器地址不是你自己的 socket（不支持通过 SSH 或 TCP 连的远程服务器）。

在脚本里，`eitri open` 在窗口关闭后退出码为 0，在上面这类、什么都还没动之前的拒绝时为 64，在终端的 UI 已经搬走之后出了问题时为 1（此时终端会拿回它的 Neovim）。

## 你会看到什么

终端里显示：

```
This nvim is open in Eitri. It comes back here when Eitri lets go of it. <Enter> takes it back now.
If the screen is blank when it comes back, nvim is waiting for a key: press Esc.
```

窗口的标题和顶栏写作 `<项目> (from terminal)`，让你能分清它和你自己启动的 Eitri 窗口。它有窗口该有的一切：带标签页的 agent 面板、回合审阅和权限，底部终端，浮动窗口，工作区。在那个终端里输入 `eitri FILE` 会在这个 Neovim 里打开文件。Visual 模式下的 `<leader>ae` 把选区送到面板，和[放在 nvim 旁边的面板](companion.zh-CN.md)一样。编辑器不会重新打开上一次的文件，也不会写会话记录：它是你的 Neovim，带着你交给它的会话。

## nvim 怎样回来

不管窗口怎样结束，同一个 Neovim 都会回到终端，带着它所有的缓冲区、任务和 `:terminal`：

- 你关闭窗口，或者在编辑器上按 `prefix x`（它会先问：`give nvim back to the terminal and close this window? (y/n)`），或者 Eitri 退出，或者最后一个模块被关掉；
- 你在终端的等候画面上按 `<Enter>`；
- 会话因为注销或 `kill` 而结束（Eitri 先把 Neovim 还回去再退出），或者 Eitri 崩溃或被杀（终端大约一秒内就会发现，把它的 Neovim 拿回来）；
- 另一个 UI 附着到了这个 Neovim（手动执行的 `nvim --remote-ui`）：Eitri 把 Neovim 还回去并关闭，另一个 UI 留着；
- 终端的 UI 搬走之后附着失败：Eitri 带着一行话退出，终端拿回它的 Neovim。

关闭窗口时只问必须问的：

- 如果编辑器里有带未保存修改的草稿（面板的 `Ctrl+g`），或者有打开着的 agent 提议，它会说出来（`the draft in the editor has unsaved changes`、`<N> agent proposals are open`），然后问 `close anyway? (y/n)`，因为这些属于窗口，会随窗口一起结束；
- 如果 Neovim 在等一个按键，或者很忙，它会问 `nvim is waiting for a key (or busy): press Esc in the editor, or close anyway? (y/n)`；
- 关于还在运行的 agent 回合的问题，在 Neovim 已经还回去之后才问，所以终端从不为它们等着。

**Eitri 从不向你的 Neovim 发信号，也从不主动退出它。** 还回去是一次 detach：Eitri 拿掉它添加的一切，终端的 UI 回来。在窗口里输入 `:qa` 或 `ZZ` 是你自己的退出：Neovim 退出，窗口关闭，终端停在 shell 提示符上，就像你在终端里退出了它一样。

### 空白画面

如果还回去的时候 Neovim 正在等一个按键（你按了 `f`，或者开着一个提示），终端会显示一个空白画面和 Neovim 的开场文字，直到第一个按键。那个按键会交给 Neovim，所以请按 `Esc`。上面的关闭提问就是为了提醒你这一点；Eitri 不会自己发一个按键去清掉它，因为那个按键会回答 Neovim 正在问的任何问题。

## 终端被关掉了怎么办

如果你在 Neovim 还在 Eitri 里的时候关掉了终端，Neovim 会带着所有缓冲区继续在 Eitri 里运行。窗口会显示一个条幅 `the terminal this nvim came from is closed`，关闭时它会问：

```
closing leaves nvim running with no window. To see it again: nvim --remote-ui --server <地址>. Close? (y/n)
```

回答 `y` 之后，这个 Neovim 会没有窗口地继续运行，带着它没保存的缓冲区和交换文件，显示的那条命令可以在新的终端里把它找回来。Eitri 替你的终端守着的那个等候室如果被从外面杀掉了（`the terminal's way back is gone`），情形相同。如果你想结束那个 Neovim，就在终端里用显示的命令连上它再 `:qa`。

## Eitri 放进你的 Neovim 的东西，和它拿走的东西

窗口拥有你的 Neovim 时，Eitri 会往里面加几样东西，并在 Neovim 回到终端时把它们全部拿掉：窗口的命令和按键（`:Eitri`、`<leader>ae`，设置了 [`keymap.nvim_prefix`](configuration.zh-CN.md#settings) 时还有它下面的按键）、agent 的编辑器工具和审阅标记、一个按磁盘变化重新加载缓冲区的定时器，以及编辑器自己的设置（比如光标轮廓）。你打开的文件、你做的修改、agent 应你的请求编辑的文件会留下，就像在终端里一样。只被 agent 的读取工具加载、又没有窗口显示的缓冲区会被卸载。Eitri 不会往你的 Neovim 配置里写任何东西，也从不在你自己定义过的命令上覆盖 `:Eitri`。那里的 `:Eitri` 补全你 `init.lua` 键位表里的词，但不包括插件页面在加载时设的按键；在你接管的 Neovim 里 `:EitriTutor` 不会运行。

如果你有自己的 `:Eitri`、`<leader>ae` 或 `WatchGlobal`，它们在接管和拆除之后原样保留。

## 你的配置

你的 Neovim 是在终端里启动的，所以你的 `init.lua` 里为图形前端做的事从没运行过。`if vim.g.neovide then ... end` 这样的分支在启动时是假的，也没有 `ginit.vim`：Eitri 在附着到的 Neovim 里不会加载它，因为它的效果分不清是不是你自己的、也撤销不了，而且会跟着 Neovim 回到终端（在那里设置的 `termguicolors` 会改变终端的颜色）。

想在 Neovim 处在 Eitri 里时调整它，用一个检查到达的是哪个 UI 的 `UIEnter` 自动命令，再在 `UILeave` 里把东西放回去：

```lua
vim.api.nvim_create_autocmd("UIEnter", {
  callback = function()
    local info = vim.api.nvim_get_chan_info(vim.v.event.chan)
    if info.client and info.client.name == "neovide" then
      -- settings for the Eitri window only
    end
  end,
})
```

用这种办法设置的东西要由你自己撤销；Eitri 只放回它自己设置的。

## 剪贴板、tmux 插件和 smart-splits

- **剪贴板。** 你的 Neovim 保留它自己的剪贴板 provider。`tmux` provider 在窗口拥有 Neovim 时到达 Eitri 的剪贴板，之后又回到真正的 tmux。`wl-copy` 和 `xclip` 照常工作。OSC 52 写给终端，而窗口的 UI 没有终端。
- **在 tmux 里**，终端的 tmux 窗格藏在 Eitri 后面，所以交给 tmux 的移动不会让你看到任何变化。Neovim 在窗口里时，Eitri 会把 `TMUX`、`TMUX_PANE` 和 `PATH` 的开头指向它自己的替身，所以 vim-tmux-navigator 在 Neovim 边缘的 `Ctrl+h/j/k/l` 会在 Eitri 的模块之间移动焦点；之后会放回去。其他运行 `tmux` 的插件（vimux、vim-slime）在窗口拥有 Neovim 期间到达的是那个替身，而不是你的 tmux；这段时间里启动的 `:terminal` 会保留被改过的变量，所以它里面的 `tmux` 调用会失败，直到那里启动一个新的 shell。
- **smart-splits.nvim** 只在启动时选一次多路复用器，并保持用真正的 tmux。用它的 `at_edge` 钩子加 `require("eitri").edge`，就像[导航插件和 tmux](companion.zh-CN.md#navigator-plugins-and-tmux)里那样。

在已经开在 Eitri 窗口里的 Neovim 里执行 `:EitriPanel`，会显示 `this nvim is open in an Eitri window`，`eitri page` 则说它的页面就开在那里。已经在这个 Neovim 旁边的页面窗口，会在接管开始时关闭。

## `:restart` 和 `:connect`

在你接管的窗口里，`:connect <地址>` 会把你的 Neovim 还给终端，窗口随之关闭（编辑器的 UI 去了另一个服务器，那不是 Eitri 该跟着去的）。`:restart` 暂时和 `:qa` 一样结束：窗口关闭，终端回到 shell 提示符，新服务器没有窗口地运行。它的地址在接管的日志里，即 `~/.local/state/eitri/companion/`（设了 `$XDG_STATE_HOME` 就在它下面）里最新的 `takeover-<16 位十六进制>.log`，那一行是 `nvim restarted at <地址>: nvim --remote-ui --server <地址>`；在终端里运行这条命令就能再看到这个 Neovim，最好在对同样的文件再开新的 `nvim`、碰上它的交换文件之前。

在 Eitri 自己启动的编辑器里，两者都会被跟随：`:restart` 把编辑器搬到新的 Neovim，`:connect` 把它搬到你指定的服务器，编辑器原来的 Neovim 继续运行。从那以后，窗口把那个服务器当作不是自己启动的：在编辑器上按 `prefix x` 会问 `let go of the nvim :connect reached and close this window? (y/n)`，因为编辑器回不到它自己的 Neovim。关闭窗口，或者你连上的服务器结束（在那里输入 `:qa`），都会带着编辑器原来那个 Neovim 的地址问上面那句 `closing leaves nvim running with no window`；不能提问的关闭（注销、`kill`）会把这一行写进窗口的日志。

<a id="turning-it-off"></a>
## 关掉它

在你的 `init.lua` 里写 `eitri.config.set("editor.takeover", false)` 会拒绝所有接管：这时 `:EitriOpen` 会说 `takeover is off (editor.takeover in init.lua)`，终端从不离开。默认是开，但你不运行 `:EitriOpen` 就什么都不会发生。见[配置](configuration.zh-CN.md#settings)。

## 不包括的

- 同一个 Neovim 同时在终端和 Eitri 里显示；
- 另一台机器上的 Neovim（SSH 或 TCP），以及 UI 是图形前端的 Neovim；
- 比 0.12 旧的 Neovim；
- Mac。

最初的几处毛边列在[已知问题](../known-issues.zh-CN.md#companion-mode)里。
