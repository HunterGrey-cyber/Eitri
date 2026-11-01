[English](keys.md) | 简体中文
<!-- translated-from: keys.md sha256=5bfdde564d2402df9a0e3e8eb83b31ce3731ebacc7171e880bb99634380bb3bc -->

# 按键

Eitri 的按键来自三个地方，它们互不冲突：用于窗口本身（标签页、模块、布局）的**前缀**；在窗口里**任何地方**都有效的几个键；以及**agent 面板自己的按键**，那些是 vim 的。下面写的都是默认设置。在面板里按 `?` 会列出你自己这套设置下的按键，包括你在[自己的配置](configuration.zh-CN.md)里改过的，或者从 tmux 拿过来的。

> **第一次用前缀键？**
>
> - 按下 `Ctrl+b`，松开，再按一个键。`Ctrl+b ?` 列出所有按键，`Ctrl+b` 之后按 `Esc` 取消。
> - `Ctrl+b` 归窗口所有，所以要把它交给当前拿着按键的东西，就连按两次：`Ctrl+b Ctrl+b` 是 Neovim 的向上翻页，
>   在底部终端里运行 Claude Code 时，也是它把正在运行的命令移到后台的那个键。
> - `Ctrl+h`、`Ctrl+j`、`Ctrl+k` 和 `Ctrl+l` 在模块之间移动，所以在底部终端里，shell 的清屏是 `Ctrl+b Ctrl+l`
>   （readline 的 `Ctrl+k` 是 `Ctrl+b Ctrl+k`）。
> - 想让 Eitri 在你按下前缀后停一下就列出接下来的键，在 `init.lua` 里写 `eitri.config.set("keymap.which_key_delay", 200)`（[提示面板](#the-hint-panel)）。
> - 如果你用 tmux，你的前缀和前缀表里的绑定来自你的 tmux 配置（[你的 tmux 按键](#your-tmux-keys)），
>   `Ctrl+b ?`（或者你自己的前缀加 `?`）会列出没拿过来的那些。
> - 想用 `Ctrl+a` 当前缀，从 [`docs/keymap/tmux-ctrl-a.lua`](../keymap/tmux-ctrl-a.lua) 开始；i3 或 sway 把 Alt
>   当 `$mod` 的话，从 [`docs/keymap/i3-alt-mod.lua`](../keymap/i3-alt-mod.lua) 开始。
> - 没有 `init.lua` 的第一次启动，Eitri 会提议设置按键并写入一个（[首次启动](configuration.zh-CN.md#first-start)）；
>   `:Eitri setup` 可以把这个界面叫回来。
> - 在编辑器里运行 `:EitriTutor`，会一步步带你把这些键走一遍，用英文或简体中文（按你的语言环境来选，和
>   Neovim 的 `:Tutor` 一样）。课文里凡是指“你的前缀”的地方都写你自己的前缀，教的是前缀之后的默认键，所以被
>   tmux 配置改过的键不会体现在里面；你的前缀加 `?` 会列出你的键。
> - **在 Hyprland、sway 和 niri 上，这一页的布局键默认关闭**，因为窗口由你的窗口管理器排布：`Ctrl+b %` 只显示一条提示，不做别的。
>   [合成器窗口](compositor-windows.zh-CN.md#keys)说明哪些键在那里仍然有效，`window.tiling = "eitri"` 把其余的键带回来。教程不管怎样都会教这些键。

## 前缀

前缀就是原版 tmux 的：按下 `Ctrl+b`，松开，再按一个键。它在编辑器、面板和底部终端看到任何东西之前就被读走，所以不管按键焦点在哪里它都有效。`Esc` 取消它，按下表里没绑定的键会结束等待而不做任何事。要把 `Ctrl+b` 本身发给当前拿着按键的东西，连按两次。`<prefix> ?` 打开完整按键表，并滚到“After <prefix>”一节。等待前缀后续按键时，提示条以 `? keys` 结尾；这里使用 `panel.keymap` 的实际绑定，未绑定时不显示这一项。

标着“repeats”的键，可以在半秒内不再按前缀而直接再按一次，和 tmux 里一样。如果你宁愿看见这些键而不是背它们，[提示面板](#the-hint-panel)会在停顿之后列出来。

**标签页。** agent 面板里的每个对话是一个标签页。

| 按键 | 作用 |
|---|---|
| `<prefix> c` | 新建标签页 |
| `<prefix> n` / `<prefix> p` | 下一个 / 上一个标签页 |
| `<prefix> 1` ... `9` | 按编号切到那个标签页 |
| `<prefix> l` | 切回你之前所在的标签页 |
| `<prefix> ,` | 重命名这个标签页 |
| `<prefix> &` | 关闭这个标签页，会先问一个 `y/n` |
| `<prefix> w` | 选一个会话：先是已打开的标签页，然后是可以继续的已保存会话 |
| `<prefix> i` | 这个会话的详情 |

**模块。** 窗口由模块组成：编辑器、agent 面板和底部终端。

| 按键 | 作用 |
|---|---|
| `<prefix> e` / `a` / `t` | 显示并聚焦编辑器 / agent / 终端；如果它已经拿着按键，就把它隐藏；编辑器里的 nvim 退出之后（对它按了 `<prefix> x`，或者 `:q` 之后窗口仍然开着），显示编辑器会在原处启动一个新的 nvim |
| `<prefix> Left`、`Right`、`Up`、`Down` | 把按键移到那一侧的模块（repeats） |
| `<prefix> ;` | 把按键移回之前拿着它的模块 |
| `<prefix> o` | 把按键移到屏幕上的下一个模块 |
| `<prefix> z` | 放大这个模块，占满窗口，或恢复。本指南里“放大”只指这件事；改文字的大小叫“文字大小” |
| `<prefix> %` 或 `<prefix> "`，然后 `e`、`a` 或 `t` | 在这个模块的右边 / 下面打开那个模块，或者把它移到那里；按键在终端里时，`t` 会在那里打开一个新的终端（见[容器和终端](#containers-and-terminals)） |
| `<prefix> d`，然后 `e`、`a` 或 `t`（或者面板自己的键） | 隐藏那个模块，不管按键在哪里；它照常运行。按键留在原处，除非隐藏的正是拿着按键的那个模块，那时按键交给它旁边的模块；`Esc` 取消 |
| `<prefix> !` | 按键在 agent 面板里时：把正在用的标签页拆出来，放进面板右边它自己的聊天视图；按键在这样的视图里时：把它的标签页并回去（见[聊天视图](#chat-views)） |
| `<prefix> {` / `<prefix> }` | 把这个模块和屏幕上的上一个 / 下一个模块交换 |
| `<prefix> Alt+1` / `Alt+2` | 所有模块排成一行 / 一列，大小相等 |
| `<prefix> Alt+3` / `Alt+4` / `Alt+5` | 第一个模块在上面占大头、其余排成一行在下面 / 第一个模块在左边占大头、其余排成一列在旁边 / 所有模块排成网格（tmux 的 `main-horizontal`、`main-vertical` 和 `tiled`） |
| `<prefix> Space` | 切到循环里的下一种排布：一行、一列、`Alt+3`、`Alt+4`、`Alt+5`，然后回到一行；从你上一次选的那种之后开始；如果此后模块被别的方式重新排布过，或者有模块被关闭，则从一行开始；移动分隔线、放大、标记，以及显示或隐藏模块，都不会改变它在循环里的位置 |
| `<prefix> E` | 把这个模块周围的模块沿它们被分割的方向均匀摊开（tmux 的 `select-layout -E`） |
| `<prefix> H` / `J` / `K` | 把这个模块移到窗口的最左边 / 最下边 / 最上边，占满整个高度或宽度（vim 的 `Ctrl+w H`、`J`、`K`）；它已经在那里时什么都不发生 |
| `<prefix> Alt+e` | 把这个模块所在的分割从左右并排改成上下叠放，或者改回来（i3 的 `layout toggle split`） |
| `<prefix> Ctrl+o` / `Alt+o` | 把屏幕上的每个模块在原有的空间里向前 / 向后轮转一位（tmux 的 `rotate-window`） |
| `<prefix> m` / `M` | 标记这个模块，如果它就是被标记的那个则取消标记 / 清除标记；同一时刻只标记一个模块，有标记时顶栏会显示 `marked: <标题>` |
| `<prefix> q` | 在屏幕上的每个模块上画一个数字；按下那个数字（`1` 到 `9`，然后是 `0`）就让那个模块拿到按键。`Esc` 或任何别的键会结束它，一秒钟没有按键也会结束 |
| `<prefix> Ctrl+Up`、`Ctrl+Down`、`Ctrl+Left`、`Ctrl+Right` | 把最近的分隔线朝那个方向移一格（repeats） |
| `<prefix> Alt+Up`、`Alt+Down`、`Alt+Left`、`Alt+Right` | 同上，五格（repeats） |
| `<prefix> x` | 关闭这个模块并结束它里面运行的东西，会先问一个 `y/n`；关掉屏幕上的最后一个模块就关闭 Eitri，这时的问题和关闭窗口时一样，还会列出正在运行或排队的聊天回合，以及正在运行命令的终端（`kill-pane editor? closes Eitri, 2 terminal jobs (y/n)`）；在 shell 提示符下放到后台的任务不算在内。对编辑器按时，它像 `:confirm qall` 那样退出 nvim（有未保存的缓冲区时 nvim 自己会问），之后按 `<prefix> e` 编辑器就带着一个新的 nvim 回来 |
| `<prefix> [` 或 `<prefix> PageUp` | 把底部终端往回滚（复制模式）；只在终端拿着按键时有效，在别处会闪一下拒绝提示，提示框写着 `prefix [ scrolls the terminal` |
| `<prefix> F11` | 沉浸模式：不带顶栏的全屏 |
| `<prefix> Ctrl+=` / `Ctrl+-` / `Ctrl+0` | 只把拿着按键的那个窗格的文字调大 / 调小 / 回到和其他模块一致：编辑器、聊天、某个插件页面或终端。一次前缀可以调任意多步：只要 Ctrl 还按着，以及最后一步之后的半秒内，`=`、`-` 和 `0` 都留在这个窗格上 |

上面这些排布在真的改变了东西时会结束放大，什么都没做时则保持放大。按键仍留在原来拿着它的模块上，被隐藏的模块仍然隐藏。屏幕上少于两个模块时它们什么都不改，只闪一下拒绝提示；`Alt+3`、`Alt+4`、`Alt+5` 和 `Space` 在屏幕上超过 20 个模块时也会拒绝。

按键被拒绝时，顶栏里 Eitri 的名字会闪一下，右上角的提示框会说明原因（`rotate: fewer than two modules on screen`、`no other module on screen`）。少数拒绝没有可说的，只闪一下，比如那个方向上没有标签页时按的标签页键。提示框正在告诉你某个隐藏的对话有权限卡片在等待时，被拒绝的按键不会盖掉这条提示，只闪一下。

**没有按键的动作。** 这些按名字使用：在 Lua 命令里用 `eitri.layout.run`（见[配置](configuration.zh-CN.md#layout-from-lua)），绑上键之后才会出现在 `<prefix> ?` 的列表里。要给其中某个绑上键，在你的 `init.lua` 里加一行；这里的键只是示例，没有任何默认键用到它们：

```lua
local k = eitri.keymap
k.set("prefix", "S", "swap.marked")              -- swap this module with the marked one (the mark stays)
k.set("prefix", "U", "layout.undo")              -- undo the last change of the arrangement; again, redo
k.set("prefix", "M-r", "move.far-right")         -- move this module to the far right edge
k.set("prefix", "Q", "layout.previous")          -- the previous arrangement of the cycle
k.set("prefix", "M-f", "layout.flip-horizontal") -- mirror left and right
k.set("prefix", "M-g", "layout.flip-vertical")   -- mirror top and bottom; either one twice is the identity
for key, dir in pairs({ ["M-h"] = "left", ["M-j"] = "down", ["M-k"] = "up", ["M-l"] = "right" }) do
  k.set("prefix", key, "swap." .. dir)           -- swap with the module on that side
end
```

`layout.undo` 在内存里保留一步，从不保存：它把当前排布和上一次改动之前的排布对调（之后移动过的分隔线会一起被撤销），所以再按一次就是重做。显示或隐藏模块不算它看得到的改动。用 `<prefix> x` 关闭终端或 Lua 面板会把它的位置放回下次打开的地方，这本身就成了上一次改动：紧接着执行 `layout.undo` 只会把被关闭模块的位置挪回去，屏幕上什么都不变，而关闭之前的那次改动再也撤销不了。

**给已经有动作的键再 `set`。** 本节的这些键是默认键，对一个已有动作的键调用 `eitri.keymap.set` 会让 Eitri 在启动时停下，信息里点名那次调用，并说要先执行 `eitri.keymap.del("prefix", "<key>")`。如果你的 `init.lua` 设置了 `m`、`M`、`q`、`E`、`H`、`J`、`K`、`Space`、`C-o`、`M-o`、`M-e`、`M-3`、`M-4`、`M-5`、`M-H`、`M-J`、`M-K`、`M-L`、`M-w`、`M-s`、`M-a`、`M-d`、`M-Enter`、`M-S-Space`、`M--`、`M-_`、`(`、`)`、`L`、`s`、`$`、`!`、`C-=`、`C-+`、`C--` 或 `C-0`（`eitri.keymap` 接受的写法），就在那个 `set` 之前放一行 `k.del("prefix", "<key>")`。tmux 导入开着、并且你的 tmux 配置绑定了这个键时，这个键已经算是你的，你的 `set` 直接替换它，不需要 `del`。对一个没有绑定任何东西的键执行 `del` 同样会让启动停下，所以 `del` 这一行只有在这个版本及之后才能用，或者导入开着、并且你的 tmux 配置绑定了那个键。

**窗口和面板。**

| 按键 | 作用 |
|---|---|
| `<prefix> f` | HINT：跳到窗口里的任何地方（见 [HINT](#hint)） |
| `<prefix> r` | 重新加载 agent 面板；会话继续运行 |
| `<prefix> ?` | 完整的按键列表，显示在面板里，从“After <prefix>”一节开始（从插件页面打开时，该页面的按键仍排在最前）；聊天自己的 `?` 从“This panel”开始。用 `?`、`Esc`、`q` 或点击面板外关闭它，会把按键还给原来拿着它们的模块，并恢复因打开它而改变的缩放或被隐藏的聊天面板 |
| `<prefix> :` | 面板的 `:` 命令行（见[命令行](#the-command-line)） |
| `<prefix> Ctrl+h`、`Ctrl+j`、`Ctrl+k`、`Ctrl+l` | 把那个组合键本身发给当前拿着按键的东西；下面不带前缀的那几个组合键是 Eitri 的 |

**不带前缀**时，这些在窗口里的任何地方都有效（[不带前缀的键](#keys-without-the-prefix)里还能加上你自己的）：

| 按键 | 作用 |
|---|---|
| `F11` | 全屏 |
| `Ctrl+=` / `Ctrl+-` / `Ctrl+0` | 把窗口里每个模块的文字调大 / 调小 / 还原：编辑器、聊天、所有插件页面和所有终端（小键盘的 `+`、`-` 和 `0` 也行；`+` 单独占一个键的键盘，比如德语键盘，`Ctrl++` 也行） |
| `Ctrl+wheel` | 只改指针所在那个窗格的文字大小 |

**文字大小怎么算。** 窗口有一个文字大小，每个模块各有一个和它的差别。`Ctrl+=` 和 `Ctrl+-` 改窗口的大小，所以每个模块都跟着动，各自的差别不变。`<prefix> Ctrl+=`、`Ctrl+-` 和 `Ctrl+wheel` 改一个差别：所有聊天视图共用一个，所有终端共用一个，编辑器一个，每个插件页面各一个。`<prefix> Ctrl+0` 清掉拿着按键的那个窗格的差别，它就回到和其他模块一致的大小，不管那时其他模块是多大。`Ctrl+0` 清掉所有差别，并把窗口的大小还原成你的配置写的值：`init.lua` 设了 `g:neovide_scale_factor` 就是它，否则是 100%。

- **哪些会动。** 这个窗口的每个模块，显示着的和隐藏的都算。顶栏和提示面板的大小不变；按键在顶栏上时，窗格键不起作用。
- **其他窗口。** Eitri 的每个窗口有自己的文字大小。第二个窗口从 100% 开始。只有一个模块的窗口（[companion 窗口](companion.zh-CN.md)、[合成器窗口](compositor-windows.zh-CN.md)下的聊天或页面窗口）只有一个大小，所以那里 `<prefix> Ctrl+=` 和 `Ctrl+=` 是同一个动作。
- **重启后什么都不保留。** 想长期保留，在配置里设 `guifont`、`agent.font_size`、`terminal.font` 或 `g:neovide_scale_factor`。
- **你自己的映射。** `Ctrl+=`、`Ctrl+-`、`Ctrl+0`、`Ctrl++` 和 `Ctrl+wheel` 在 Neovim 看到之前就被 Eitri 拿走了，所以 Neovim 里的 `<C-=>` 映射不会运行。`<C-+>` 在要按 Shift 才能打出 `+` 的键盘上仍然有效：那里的 `Ctrl+Shift+=` 照旧传给 Neovim。你自己的映射要影响每个模块，就写 `g:neovide_scale_factor`：Eitri 跟随对它的任何写入，包括 `:let`，并保留每个模块的差别。
- **要按 Shift 才能打出这些键的键盘。** 如果 `=` 或 `0` 要按 Shift，就像打这个字符时一样按住它：德语键盘上 `<prefix> Ctrl+Shift+0` 就是 `<prefix> Ctrl+=`，单独的 `Ctrl+Shift+0` 调整每个模块。

插件页面（学习插件、Markdown 预览）整页一起变大变小，版面随文字一起变，并跟着窗口的文字大小走：`Ctrl+=` 让所有页面和其他模块一起变大；单独调大或调小过的页面会一直保持这个差别，直到按 `Ctrl+0`，或在它上面按 `<prefix> Ctrl+0`；页面重新加载后大小不变。这些键页面本身收不到。

终端用编辑器的字体绘制，也就是你的 `guifont` 里的字体和字号（在 `init.lua` 里设 `terminal.font` 可以改，见[配置](configuration.zh-CN.md#settings)），并以同样的方式跟着窗口的文字大小走：`Ctrl+=` 让它和其他模块一起变大；在终端上 `Ctrl+wheel`，或在终端里按 `<prefix> Ctrl+=`，只改终端，直到在那里按 `Ctrl+0` 或 `<prefix> Ctrl+0`。窗口里所有终端是同一个大小，就像所有聊天视图是同一个大小。改字号就是给 shell 换一个网格：字越大，行数和列数越少，里面运行的程序会收到通知，和窗格本身改变大小时一样（`stty size` 会显示新的数字）。这些键和 `Ctrl+wheel` 程序本身收不到。

companion 窗口（见 [Companion 模式](companion.zh-CN.md)）没有自己的布局，所以那里的前缀只保留标签页键、`r`、`?`、`:`、`f`、文字大小键（`Ctrl+=`、`Ctrl+-`、`Ctrl+0`）、发送前缀本身或某个 `Ctrl+h/j/k/l` 组合键，以及方向键（它们朝相邻的窗口移动）；每个布局键都回答 `not in a companion window`。

<a id="containers-and-terminals"></a>
### 容器和终端

模块放在容器里。容器可以是**分割**的（里面的模块左右并排，或者上下叠放：`%` 和 `"` 做出来的就是这种）、**标签页式**的（同一时刻只显示一个模块，上面有一行标题）或者**堆叠式**的（同一时刻只显示一个模块，每个模块在上面各有一行标题）。下面的键是 i3 的 `$mod` 组合键，把 Alt 当作 `$mod`，在前缀之后按。标题就是顶栏里该模块的芯片所显示的名字（`terminal 2`；agent 有卡片在等时是 `agent ⚑1`），原样画出，从不当作标记语言解析。

| 按键 | 作用 |
|---|---|
| `<prefix> Alt+w` / `Alt+s` | 把这个模块所在的容器改成标签页式 / 堆叠式；再按一次，容器回到原来的分割，大小也一并恢复。“这个模块的容器”是它周围最近的、屏幕上还有别的模块的那一个，所以在默认窗口里它是整个窗口，而不是编辑器那一列加上一个隐藏的终端。找不到时什么都不发生，并闪一下拒绝提示 |
| `<prefix> Alt+H`、`Alt+J`、`Alt+K`、`Alt+L` | 把这个模块向左 / 向下 / 向上 / 向右移一步（repeats；i3 的 `move`）。它会和那一侧的模块互换位置，或者进入那一侧的容器、放到该容器所显示模块的旁边，或者离开自己的容器去往外面一层；到了窗口边缘什么都不发生。`<prefix> H`、`J`、`K` 则是一直移到边缘 |
| `<prefix> Alt+a` / `Alt+d` | 选中当前所选之物的父容器 / 回到拿着按键的那个子项（i3 的 `focus parent` 和 `focus child`） |
| `<prefix> Alt+Enter` | 在拿着按键的模块右边打开一个新的终端；它的 shell 从项目根目录启动 |

如果你的 i3 或 sway 的 `$mod` 是 Alt，这些组合键会先被窗口管理器拿走，Eitri 看不到，这些行就永远不会运行。[`docs/keymap/i3-alt-mod.lua`](../keymap/i3-alt-mod.lua) 会把被拿走的那些行移开：每一行保留原来的键并加上 Ctrl（`<prefix> Ctrl+Alt+w`、`Ctrl+Alt+Enter`，等等）。键里带 Shift 的那几行则去掉 Shift，而不是保留它：Eitri 不把 Ctrl+Shift+<字符> 当作组合键来读，所以四个移动键（`Alt+H`、`Alt+J`、`Alt+K`、`Alt+L`）变成 `<prefix> Ctrl+Alt+h`、`Ctrl+Alt+j`、`Ctrl+Alt+k`、`Ctrl+Alt+l`，移入便笺板那一行（`Alt+_`）变成 `<prefix> _`；浮动切换（`Alt+Shift+Space`）变成 `<prefix> Ctrl+Alt+Space`，因为在很多环境里 Ctrl+Shift+Space 是切换输入法的键。把它粘贴进你的 `init.lua`。

**标签页和堆叠。** 点一下标题，或者按 `f` 再输入标题的标签（HINT 会给每个标题标上标签），就能把那个模块带到前面并让它拿到按键。用键盘的话，在标签页式容器里 `Ctrl+h` / `Ctrl+l` 移到左边 / 右边的标签页，在堆叠式容器里 `Ctrl+k` / `Ctrl+j` 移到上面 / 下面那一个；那个方向没有标签页时，它们就像在模块之间一样继续往外走。拿着按键的模块永远是显示出来的那个，所以隐藏或关闭它会把按键交给接着显示的标签页。没有显示的标签页照常运行：那里的终端继续输出，回来时能看到这些行；聊天保持着它的会话。不在显示中的标签页上的聊天，在有卡片等待时，标题上带着 `agent ⚑N`；`modules.chat.on_permission` 设为 `"reveal"` 时，新卡片会把它的标签页带到前面，除非按键正在同一容器的另一个标签页上：那时你只会看到提示，什么都不动。

**选中一个容器。** `<prefix> Alt+a` 选中拿着按键的模块周围的容器，顶栏会这样显示：`selected: column (editor, terminal)`，即容器的种类和它里面显示着的模块的标题。再按一次会再往上一层（最外层是窗口自己的容器），`<prefix> Alt+d` 往下回去。选中一个容器时，用来排布的那些键（`Alt+H`、`Alt+J`、`Alt+K`、`Alt+L`、`Alt+w`、`Alt+s`、`Alt+e`、`z`、`H`、`J`、`K`、`E` 和翻转）作用于整个容器。必须针对单个模块的那些（`x`、隐藏模块、交换和标记）会拒绝，并闪一下 `acts on one module`。把按键交给别的模块、用分割键打开一个模块，以及前缀之后的 `Esc`，都会结束选中。选中的容器周围不画任何东西；顶栏的文字就是全部。

**终端。** 一个窗口里最多可以有九个终端，`terminal` 和 `terminal 2` 到 `terminal 9`，各有各的 shell。新的终端来自 `<prefix> Alt+Enter`，或者按键在终端里时的分割键：`<prefix> %` 或 `"` 再按 `t`，会在那一侧打开一个新的终端，而在别的模块里 `t` 打开的是那唯一的终端。这时前缀的提示读作 `t new terminal`，开着好几个时读作 `t terminal ×3`。单独的 `<prefix> t` 到达拿着按键的那个终端，否则到你最近用过的那个。带 `cwd = "pane"` 的分割绑定会让新的 shell 在你分割的那个终端所在的目录里启动（见[配置](configuration.zh-CN.md#layout-from-lua)）。对编号的终端按 `<prefix> x` 会结束它的 shell 并把它移除，先问通常的 `y/n`；`terminal` 本身只是关闭，会在编辑器下面重新打开。编号终端的 shell 结束（`exit`）时，它不问任何问题就被移除。按键在终端里时，`<prefix> n` / `<prefix> p` 在它处于标签页式或堆叠式容器中时去下一个 / 上一个标签页，否则去屏幕上的下一个 / 上一个终端，只有一个时什么都不做。终端没有调整文字大小的键。`Alt+Enter` 只打开终端；新的聊天视图来自按键在聊天视图里时的分割键再按 `a`，或者 `<prefix> !`（见[聊天视图](#chat-views)）。

<a id="chat-views"></a>
### 聊天视图

**聊天视图**是 agent 面板的另一个页面，作为一个单独的模块显示，里面只放一个会话标签页。你一开始用的 agent 面板是**基础视图**（`agent`）；其余的是 `agent#2` 到 `agent#9`，一共最多九个（第十个会被拒绝，提示 `at most 9 chat views`）。每个视图都和终端一样是一个模块：它在布局里，可以移动、放大、浮动或隐藏，在每个工作区里都有，并且有自己的芯片（`agent 2`，有卡片等着时读作 `agent 2 ⚑1`）。它们的标签页属于窗口，所以视图被隐藏时标签页照常运行，所有视图用的都是窗口那一个[账号](configuration.zh-CN.md#accounts)。

- **`<prefix> !` 把标签页拆出来。** 按键在基础视图里时，你正在用的标签页移进基础视图右边的一个新视图，并且这个视图拿到按键。基础视图只有一个标签页时它会拒绝（提示 `can't break with only one tab`）：基础视图永远留着一个标签页。按键在额外的视图里时，`!` 做相反的事：这个视图不再显示，它的标签页回到基础视图成为正在用的标签页，按键也回到那里。按键在编辑器或终端里时它会拒绝（提示 `tab.break: the keys are in no chat view`）。你的 `init.lua` 里要 `set` `!` 的话，要像其他默认键一样，在前面放 `k.del("prefix", "!")`（见上面）。
- **带一个空标签页的新视图。** `eitri.layout.run("instance.new", { kind = "agent" })`、`eitri.layout.open("agent")`，以及按键在聊天视图里时的分割键再按 `a`，都会建出一个新视图，里面是默认 agent 的一个新的空标签页（它的第一条消息要先在那个视图里过一遍通常的[信任问题](permissions.zh-CN.md#trusting-a-project)）。它开在哪里由你的[打开规则](configuration.zh-CN.md#where-a-new-module-opens)决定；默认是在拿着按键的模块右边。
- **`<prefix> a`** 去你想要的那个聊天：按键在某个聊天视图里时，它隐藏这个视图，和现在隐藏基础视图一样；在别处时，它去有最早一张等待中的卡片的视图，否则去你最近用过的聊天视图，再否则去基础视图。视图默认没有自己的键；`eitri.keymap.set("prefix", "F4", "module.agent#2")` 能给那个视图配一个同样行为的键。
- **关闭。** 在视图里按 `<prefix> x` 会问 `kill-pane agent 2? closes tab 3 <name> (y/n)`，按 `y` 就关闭那个标签页并移除这个视图。标签页被关掉或并回去之后，没有标签页的视图会自己消失。

**额外视图里的标签页键。** 一个视图只放一个标签页，所以标签页的键在那里读法不同：

| 按键 | 按键在额外视图里时它们做什么 |
|---|---|
| `,` 重命名、`i` 详情、`&` 关闭、`Shift+Tab` 模式 | 作用于这个视图的标签页；`&` 还会移除变空的视图 |
| `r`、`?`、`:` | 只重新加载这个页面、显示它的按键、打开它的命令行（`:` 命令作用于这个视图的标签页） |
| `c` | 在基础视图里新建一个标签页，并让基础视图拿到按键 |
| `n`、`p`、`l` | 像从编辑器里按一样，在基础视图的标签页之间移动，按键不动 |
| `1` ... `9` | 那个编号的标签页，不管它在哪里：在额外视图里的标签页让那个视图拿到按键（隐藏着就先显示），基础视图的标签页在基础视图里被选中，基础视图拿到按键 |
| `w` | 在这个页面里打开选择器，列出每个打开的标签页（在额外视图里的标签页，名字后面读作 ` · agent 2`）；选一个等于按它的数字 |
| `<leader>b o` | 闪一下：这里没有别的标签页可关 |
| `!` | 把标签页并回去（见上） |

基础视图的标签条只列出它自己的标签页，所以在别的视图里显示的标签页会在它的编号里留下空缺，和被关掉的标签页一样。只有一个标签页的视图不画标签条。恢复的会话，在布局里还有它原来的视图时回到那个视图，否则回到基础视图（见 [agent](agents.zh-CN.md#chat-views)）。

<a id="floats-the-scratchpad-and-workspaces"></a>
### 浮动窗口、便笺板和工作区

**浮动窗口**（float）是画在其他模块之上的一个模块，有自己的大小和位置；它的上沿有一条栏，写着它的标题（读法和顶栏的芯片一样，原样画出），右下角有一个小握柄。编辑器从不浮动；agent 面板可以。**便笺**（scratch）浮动窗口是可以收起再叫回来的浮动窗口：收起时它不占地方，顶栏里它的芯片读作 `<title> ⌁`，它照常运行。**工作区**是同一批模块的一整套排布，带一个名字；窗口可以有好几个，同一时刻显示一个。

| 按键 | 作用 |
|---|---|
| `<prefix> Alt+Shift+Space` | 把这个模块浮起来，大小和位置用它上次浮动时的，没有就居中、占窗口的 60%；按键在浮动窗口上时，把它放回布局，放在你上次用过的平铺模块旁边。编辑器和视野里最后一个平铺模块会被拒绝 |
| `<prefix> Alt+_` | 把这个模块送进便笺板：它浮起来并被收起 |
| `<prefix> Alt+-` | 便笺板：收起拿着按键的那个正在显示的便笺浮动窗口；否则抬起一个正在显示的便笺浮动窗口；否则显示收起得最久的那一个。反复按会轮着来。便笺板是空的时，会闪一下并说明怎样往里放 |
| `<prefix> Alt+H`、`Alt+J`、`Alt+K`、`Alt+L` | 按键在浮动窗口上时：把它向那个方向移五格，不出窗口（repeats） |
| `<prefix> Ctrl+Left` ... `Ctrl+Down`、`Alt+Left` ... `Alt+Down` | 按键在浮动窗口上时：调整它的大小，一格 / 五格，中心不动：`Right` 和 `Down` 变大，`Left` 和 `Up` 变小，不会小于模块的最小尺寸，也不会超出窗口（repeats） |
| `<prefix> H` / `J` / `K` | 按键在浮动窗口上时：把它作为平铺模块停靠到最左 / 最下 / 最上边缘 |
| `Ctrl+h`、`Ctrl+j`、`Ctrl+k`、`Ctrl+l` | 从浮动窗口出发：如果那个方向上最近的是一个浮动窗口，就去它；否则去那一侧的平铺模块；再否则去浮动窗口中心下面的平铺模块。浮动窗口永远不会通向顶栏 |
| `<prefix> (` / `<prefix> )` | 上一个 / 下一个工作区，首尾相接 |
| `<prefix> L` | 你之前所在的工作区（tmux 的 `switch-client -l`）；没有这样的工作区，或者那个已经被删除时，会闪一下 |
| `<prefix> s` | 选择一个工作区（见下） |
| `<prefix> $` | 重命名这个工作区 |

点击或拖动浮动窗口的栏可以抬起并移动它；拖它的握柄可以调整大小。让一个浮动窗口拿到按键（点击、`Ctrl+h` 之类、HINT 标签、它的芯片）会把它抬到其他浮动窗口之上。浮动窗口沉回去时，它的矩形会被记住，供下次浮动时使用，并保存在布局文件里。

**浮动窗口没有的东西。** 它在分割里没有邻居，所以排布平铺树的那些键要么不碰浮动窗口，要么拒绝。预设（`Alt+1` ... `Alt+5`、`Space`）、翻转、旋转和 `layout.undo` 只作用于平铺的模块。按键在浮动窗口上时，`E`、标签页式和堆叠式、`Alt+e`、`Alt+a`、`Alt+d` 和各种交换会被拒绝，提示读作 `a float has no split: <prefix> Alt+Shift+Space sinks it`。按键在浮动窗口上时的分割键，会把它的模块打开在你上次用过的平铺模块旁边；点名一个浮动模块的分割键，会把它从浮动窗口里移进那里的树。缩放一个平铺模块会让所有浮动窗口离开屏幕（每个在顶栏里留一个芯片）；缩放一个浮动窗口则只显示它自己。

**把便笺板当成下拉窗口。** `scratchpad.show` 可以带一个模块，于是一个键对应一个模块，是终端时第一次使用会把它建出来：

```lua
local k = eitri.keymap
k.set("prefix", "F2", "scratchpad.show", { module = "terminal#2" })   -- F2 上的下拉终端
```

第一次按下，像 `<prefix> Alt+Enter` 一样建出 `terminal#2`，但直接放进便笺板、显示并拿到按键；下一次把它收起；第三次把它叫回来。对于一个已存在、却不是便笺浮动窗口的模块，这一按会把它送进便笺板并显示。对一个不存在的聊天视图，`module = "agent#3"` 以同样的方式建出它：一个带新的空标签页的浮动窗口，放进便笺板并显示；已被占用的编号，或者第十个视图，会被拒绝。

**工作区。** `<prefix> s`（或者点顶栏上工作区的名字；只有两个或更多工作区时才显示）会在前缀的提示行里画出选择器，并等待一个键：

```
1 main · 2 study · c new · x delete
```

`1` ... `9` 依次去那个工作区，`0` 去第十个；`c` 把你现在的排布复制成下一个空闲编号（`2`、`3`、……）的新工作区并去那里；`x` 再按一个数字删除那个工作区。`main` 永远不会被删除或重命名，你现在所在的那个也一样。`$` 在顶栏里打开一个带着名字的输入框：`Enter` 重命名，`Esc` 离开。切换时，按键交给该工作区上次拿着按键的模块（它在那里显示着的话），另一个工作区原样保留，缩放状态也一并保留。agent 面板在每个工作区里是同一个模块，终端也是：新终端会以隐藏的方式、放在编辑器下面，进入其他工作区。

**没有键的动作。** `focus.mode-toggle` 把按键交给另一层上你上次用过的模块（从平铺到浮动窗口，或者从浮动窗口到平铺；`Alt+Space` 是 GNOME 的，所以它没有键）。`workspace.new` 用下一个空闲编号建出并进入一份复制，`workspace.switch` 带一个 `name`（`eitri.keymap.set("prefix", "F3", "workspace.switch", { name = "study" })`），没有这个名字的工作区时先建出来。两者都没有默认键；像上面那样绑定即可。

**每个新默认键需要的 `del`。** `Alt+Shift+Space`、`Alt+-`、`Alt+_`、`(`、`)`、`L`、`s` 和 `$` 现在是默认键，`!`（`tab.break`，tmux 的 `break-pane`）也是，所以已经对其中任何一个调用 `set` 的 `init.lua`，要在那个 `set` 之前放一行 `k.del("prefix", "<key>")`，否则启动会停下（写法是 `eitri.keymap` 接受的那种：`M-S-Space`、`M--`、`M-_`、`(`、`)`、`L`、`s`、`$`、`!`；`!` 就是 `k.del("prefix", "!")`）。规则和上面“给已经有动作的键再 `set`”一样，tmux 导入也包括在内。

<a id="the-hint-panel"></a>
### 提示面板

`eitri.config.set("keymap.which_key_delay", 200)` 让 Eitri 在前缀已经按下、过了这么多毫秒还没等到第二个键时，列出接下来可以按的键。值是 0 到 5000 之间的整数，或者 `false`、`"off"`（默认，什么都不显示）。`0` 立刻显示；设成 `200` 时，如果你一气呵成地按 `Ctrl+b z`，永远看不到它。

面板出现在窗口靠下的位置，盖在模块上面，居中，宽度最多是窗口的 90%，高度最多 45%。键按 Tabs、Modules、Layout、Workspaces（浮动窗口也在这里）、Windows、Other 几组排列，每个键有一个简短的英文名（`zoom`、`split right…`、`next tab`）；插件面板自己的模块键单独一行，写它的标题。这个窗口拒绝执行的动作，和按键表里一样不列出来。横向放不下的组不显示，太高的组末尾变成 `… 5 more` 这样的一行；`<prefix> ?` 列出全部的键。按下分割键（`%` 或 `"`）之后，面板只列出接下来可以按的模块键，标题用提示条已经在用的那一句。

它从不抢键，也不接收指针事件：下一个键照常交给前缀，就像没有面板一样。等待结束时它就消失（任何键、`Esc`、一次点击，或者窗口失去焦点）。会重复的键的等待、数字等待和工作区选择器显示它们自己的内容，不出面板。提示条保持原样。

有两处没做到：[companion 窗口](companion.zh-CN.md)不画这个面板（设置打开时它会在日志里说一次，提示条和按键表照常工作），Mac 主机也不画。面板上的文字是英文，和提示条、按键表一样。

<a id="choosing-ctrlspace-as-the-prefix"></a>
### 选 Ctrl+Space 当前缀

默认仍是 `Ctrl+b`：它是原版 tmux 的，本指南和 README 教的也是它（`:EitriTutor` 讲的是你实际用的前缀）。想用 `Ctrl+Space` 的话，像设置别的前缀一样设，并把“原样发出去”的那个键一起换掉：

```lua
local k = eitri.keymap
k.prefix("C-Space")
k.del("prefix", "C-b")
k.set("prefix", "C-Space", "send-prefix")
```

先确认三件事。fcitx5 的默认触发键就是 `Ctrl+Space`，fcitx5 在运行时，这个键可能根本到不了 Eitri（[不带前缀的键](#keys-without-the-prefix)里的冲突警告只管根键，不管前缀：按一下，看提示条出不出来）。macOS 用它切换输入源。nvim-cmp、blink.cmp 和 kickstart.nvim 在 Insert 模式里把 `<C-Space>` 映射成补全：前缀设成 `Ctrl+Space` 后，要连按两次才能碰到那个映射，就像现在的 `Ctrl+b Ctrl+b`。`send-prefix`（和 `send-keys`）对底部终端也能发出每一个组合键，`Ctrl+Space` 也不例外，shell 里它是 `Ctrl+@`（NUL）。

[首次启动界面](configuration.zh-CN.md#first-start)把 `Ctrl+Space` 列在可选前缀里，也替你做第一次检查：它的按键测试请你按一下这个键，并说明它到没到 Eitri；有输入法在运行时还会事先提醒。没到的话，是输入法或桌面把它拿走了；换一个前缀，或者你就是要改这一点的话，仍然写入。tmux 配置里的 `set -g prefix C-space` 和别的前缀一样会被读取。

<a id="moving-between-panes"></a>
## 在窗格之间移动

`Ctrl+h`、`Ctrl+j`、`Ctrl+k` 和 `Ctrl+l` 把按键焦点移到左边、下面、上面和右边的模块，和 vim-tmux-navigator 一样。它们不带前缀，而且在每个模块里都是 Eitri 的，底部终端也包括在内（终端永远收不到它们；程序真的需要时，`<prefix> Ctrl+l` 把这个组合键发给它）。

**从编辑器出发。** Neovim 优先：这个键先在 Neovim 自己的窗口之间移动，只有那个方向上再没有窗口时，才越过去进入相邻的模块。装了 vim-tmux-navigator 时这样工作。没装的话，Eitri 自己在 Normal 和 Visual 模式里给这四个键同样的行为（在 Insert 模式里只有 `Ctrl+l`，因为 `Ctrl+h`、`Ctrl+j` 和 `Ctrl+k` 保留它们在 Insert 里的含义），但只对你的配置没有映射成别的东西的键生效：你自己的映射不会被动。

**从 agent 面板出发。** `Ctrl+h` 和 `Ctrl+l` 离开面板，去那一侧的模块。`Ctrl+k` 和 `Ctrl+j` 取决于面板的模式（见下）：BROWSE 里的 `Ctrl+j` 开始输入，在输入框（INSERT）里它离开面板去下面的模块；INSERT 里的 `Ctrl+k` 回到 BROWSE，在 BROWSE 里它离开面板去上面的模块。从最上面的模块出发，`Ctrl+k` 会继续走到顶栏：`h` 和 `l` 在上面移动，`Enter` 激活一项，`Ctrl+j` 或 `Esc` 把按键还给原来的地方。

在 BROWSE 里，vim 的 `Ctrl+w h`、`Ctrl+w j`、`Ctrl+w k` 和 `Ctrl+w l` 做同样的移动；`Ctrl+w j` 永远是下面的模块，绝不是输入框。

<a id="navigator-plugins-in-the-editor"></a>
**编辑器里的导航插件。** 要不要做点什么，看插件。vim-tmux-navigator 和 Navigator.nvim 照原样就能用：它们发的 `select-pane` 由 Eitri 回答，所以在 Neovim 右边缘按 `Ctrl+l`，按键会到达那一侧的终端或面板。smart-splits.nvim 需要一个钩子。没有钩子时它不会把按键移出 Neovim（Neovim 是 GUI 时它会关掉自己的多路复用器支持，而这里的编辑器就是），所以在 `at_edge` 里把这次移动交给 Eitri：

```lua
require("smart-splits").setup({
  at_edge = function(ctx)
    require("eitri").edge(ctx.direction)
  end,
})
```

`require("eitri").edge` 来自 [eitri.nvim](companion.zh-CN.md#the-eitrinvim-plugin)，它要在 Neovim 的 `runtimepath` 上。它接受 smart-splits 传来的方向词（`left`、`down`、`up`、`right`），Eitri 接下了这次移动时返回 `true`，其他情况（包括 Eitri 不在）返回 `false`，什么也不发送。同一段配置在 [companion](companion.zh-CN.md#navigator-plugins-and-tmux) 的用法里也能用，那里是窗口管理器来接这次移动。唯一不需要钩子的 smart-splits 情形，是第 3 版加上它单独的 tmux 后端插件：它自己移动按键（钩子配了也从不被调用）。Eitri 从不修改你的插件配置。

<a id="from-neovim-eitri"></a>
## 从 Neovim 用：`:Eitri`

在 Eitri 自己的窗口所内嵌的那个 Neovim 里，`:Eitri` 运行窗口和布局动作，所以你在 Neovim 里映射的一个键，可以做 `<prefix> z` 做的事。它只存在于那个窗口的 Neovim 里：companion 的 Neovim 里没有，Mac 上没有，第二个窗口里也没有（它没有 Neovim）。

| 命令 | 作用 |
|---|---|
| `:Eitri <action> [key=value ...]` | `eitri.layout.run` 接受的任何动作，用它在 `eitri.keymap` 里的名字：`:Eitri zoom`、`:Eitri resize.left cells=5`、`:Eitri module.agent`、`:Eitri layout.even-horizontal`、`:Eitri float.toggle` |
| `:Eitri split right` 或 `below <module>` | 在编辑器旁边打开一个模块，或者把它移到那里；`<module>` 是 `editor`、`agent`、`terminal`、`terminal#2`、`agent#3` 或 `lua:<panel id>` |
| `:Eitri focus`、`show` 或 `hide <module>` | 同 `eitri.layout.focus`、`show`、`hide` |
| `:Eitri open terminal` 或 `agent` | 新开一个终端或聊天视图，按通常的规则放置 |
| `:Eitri workspace <name>` | 切换到那个工作区，没有就先建出来 |
| `:Eitri keys` | 按键表 |
| `:Eitri setup` | [首次启动界面](configuration.zh-CN.md#first-start)，用来重新选一套按键 |

使用[合成器窗口](compositor-windows.zh-CN.md#keys)时，`workspace`、`open terminal` 以及对编辑器或终端的 `split` 都关闭，和它们的键一样。

补全按位置来：第一个词列出这些词和动作名，`split` 之后列出 `right` 和 `below`，`open` 之后列出 `terminal` 和 `agent`，该写模块的地方列出模块 id，其中每个带编号的实例不管开没开都在。第一个词既不是这些词也不是动作时，是一个 Neovim 错误，什么都不会发出去。窗口拒绝的其他情况（动作缺少必需的选项、模块没有打开、`tab.new` 这样的面板键）在窗口里弹一条提示说明原因，Neovim 这边听不到回音。

**它没有包括的。** agent 面板自己的键（前缀后的 `c`、`n`、`p`、`1`-`9`、`,`，以及 `r`、`:`、复制模式和文字大小）不是布局动作，而从 Neovim 出发时拿着按键的模块永远是编辑器，所以它们多半什么也做不了。它们仍然走前缀，`:Eitri keys` 会列出。下面的 leader 组也不包括它们。

**在配置里绑定。** 这个命令在 Neovim 刚启动之后才定义（绝不会盖掉你自己的 `:Eitri`；不得不让开时会在日志里说一声），所以读你的配置时 `vim.g.eitri` 还没有设。映射到 `<cmd>Eitri zoom<cr>` 不用检查，因为它在你按下时才解析。想只在 Eitri 里绑定，就等 `User EitriReady` 事件：`:Eitri` 存在、`vim.g.eitri` 为 `1` 之后它触发一次；不在 Eitri 里两者都不会发生：

```lua
vim.keymap.set("n", "<leader>z", "<cmd>Eitri zoom<cr>", { desc = "Eitri: zoom" })

vim.api.nvim_create_autocmd("User", {
  pattern = "EitriReady",
  callback = function()
    vim.keymap.set("n", "<leader>ez", "<cmd>Eitri zoom<cr>", { desc = "Eitri: zoom" })
  end,
})
```

在 Lua 里，`require("eitri").run("zoom")`（eitri.nvim）做同样的事，Eitri 不在时返回 `false`。

**一个 leader 组。** `eitri.config.set("keymap.nvim_prefix", "<leader>W")` 在 Normal 模式里，把你生效的前缀表里的布局键映射到这个左端之下：`<leader>Wz` 放大，每个分割键成为一个组（`<leader>W%t` 在右边放一个终端，`<leader>W"e` 在下面放一个编辑器）。描述写成 `Eitri: zoom`，所以 which-key 把它们列在一个组 “Eitri window” 下。你在前缀之后学到的，就是在 `<leader>W` 之后要按的；tmux 导入和 `init.lua` 里的改动也一并带过来，因为这个组就是从那张表生成的。默认是 `"off"`。值是一个 Neovim 的左端，最长 16 个字符，可以用 `<leader>` 和 `<localleader>`。

如果那个左端本身或它之下已经有映射（LazyVim 把 `<leader>w` 当作它的窗口组），Eitri 一个都不装，并说一次是什么挡了路，所以请换一个。插件管理器加载完之后，Eitri 会再看一遍。之后有插件映射了 Eitri 的某个键，这个键以插件的为准，Eitri 不会把自己的放回去；组里其余的键不变。

<a id="keys-without-the-prefix"></a>
## 不带前缀的键

默认情况下，Eitri 不带前缀就接管的键，只有上面表里的那几个。`eitri.keymap.set("root", ...)` 加上你自己的：在任何模块里都不带前缀地运行一个动作的 Ctrl 和 Alt 组合键。这张表一开始是空的，你不填，什么都不变。

```lua
local k = eitri.keymap
k.set("root", "M-z", "zoom")
k.set("root", "C-Space", "select.next")
-- k.del("root", "M-z") removes one again
```

- **键必须带 Ctrl 或 Alt**（`C-`、`M-`、`C-M-`，命名键可以再加 `S-`）。光秃秃的键、光秃秃的功能键和只带 Shift 的键，会在 Neovim、你的 shell 和面板里到处被拿走，所以 Eitri 在启动时停下并说明。不提供 Super 组合键。
- **前缀键能运行的动作都可以**，除了 `send-prefix` 和 `send-keys`，也没有 `repeatable` 选项（按住的根键不会重复；再按一次）。分割动作会等模块键，`select.number` 和工作区动作会等它们的数字或名字，和前缀之后一样。
- **冲突会让启动停下，并点出双方：**前缀本身；内置的组合键（`F11`、`Ctrl+h`、`Ctrl+j`、`Ctrl+k`、`Ctrl+l`）；在根表里绑了两次的键；`eitri.command.register` 的键位；以及同时绑在前缀之后的键（默认的或你的 `init.lua` 里的）。对最后一种，先 `eitri.keymap.del("prefix", "<key>")`，这样 `<prefix> <key>` 才能把这个键本身发出去。只来自 tmux 导入的前缀绑定会被丢掉，tmux 设的前缀等于某个根键时也一样（前缀退回 `Ctrl+b`）；每一条 Eitri 都会记一行日志。你的 `init.lua` 自己设的前缀则仍是错误。
- **文字大小的组合键可以由你占用。** `C-=`、`C--`、`C-0` 和 `C-+` 可以做根键，同时也可以继续绑在前缀之后，所以单按组合键和先按前缀的两种作用可以互换（见下面）。
- **按住的键只运行一次。** 自动重复被吞掉，直到你松手；失去焦点或一次点击也会清掉它。绑到文字大小动作的键按住时会连续调整，和 `Ctrl+=` 一样。
- **在哪里有效。** 前缀有效的地方：窗口、第二个窗口、companion 窗口和 Mac 主机。在最后两处，动作被那个窗口拒绝的根键（比如 companion 窗口里的布局动作）会原样交给拿着按键的东西，而不是被吃掉；在那里被拒绝的文字大小组合键保持内置的作用。
- **按键表会列出它们**，在“Anywhere in the window”下，动作后面跟着 ` (init.lua)`。提示面板不列，因为它们不在前缀之后。
- **tmux 的 `bind -n` 导入时仍然跳过**（按键表里会这么说）；改在这里设。

**互换两种文字大小键。** 动作名是：拿着按键的窗格用 `text.larger`、`text.smaller`、`text.reset`，每个模块用 `text.all.larger`、`text.all.smaller`、`text.all.reset`。要让单按组合键只调一个窗格、先按前缀调整所有模块：

```lua
local k = eitri.keymap
for key, change in pairs({ ["C-="] = "larger", ["C--"] = "smaller", ["C-0"] = "reset" }) do
  k.set("root", key, "text." .. change)
  k.del("prefix", key)
  k.set("prefix", key, "text.all." .. change, { repeatable = true })
end
```

`Ctrl++` 和小键盘的 `+` 都是 `C-+`；把它以 `"larger"` 加进这张表，它们也一起互换。

**把组合键发过去。** Neovim、终端里的程序、agent 面板和插件页面都看不到根键。想让模块拿到它，就按 `<prefix> <key>`：Neovim 收到的是一个键，底部终端收到的是敲这个键会产生的字节（`Ctrl+Space` 是 NUL，`Alt+z` 是 Escape 加 `z`）。agent 面板和插件页面拿不回这样的组合键：面板只认字面的 `Ctrl+a`，页面不接受字面键。你能重新绑定的面板键表里没有 Ctrl 或 Alt 组合键，但面板固定的键里有，而且没有警告覆盖它们：`Ctrl+a`、`Ctrl+c`、`Ctrl+d`、`Ctrl+e`、`Ctrl+f`、`Ctrl+g`、`Ctrl+n`、`Ctrl+o`、`Ctrl+p`、`Ctrl+r`、`Ctrl+u`、`Ctrl+w`、`Ctrl+y`、`Ctrl+Enter` 和 `Alt+Enter`（见下面的 INSERT 和 BROWSE 两张表）。根键占了其中一个，它在面板里就不再起作用。把组合键重新发给这些页面，是以后的改动。

**警告。** 别的东西可能在 Eitri 看到之前就拿走一个根键，所以 Eitri 会检查并警告，从不因此拒绝启动。它把你的根键和这些比较：

- fcitx5 运行时的默认触发键（它看 `GTK_IM_MODULE`、`QT_IM_MODULE`、`XMODIFIERS` 和会话总线，从不读输入法自己的配置；IBus 不会触发警告，因为它的默认触发键是 `Super+Space`，而根键不能是它）；
- GNOME 当前生效的快捷键设置，仅当会话是 GNOME 时；
- sway（它的主配置：`include` 进来的文件里的绑定看不到）和 Hyprland 当前生效的绑定。

每个冲突每次启动记一行日志，只有冲突的集合和上次弹出时不同才弹一次提示。KDE 和 niri 没有来源，来自它们的冲突、来自 sway 的 include、或来自你重新配置过的输入法的冲突，Eitri 都看不见：根键没反应时，去那里找。这项检查只在 Eitri 自己的窗口里运行：companion 窗口和 Mac 主机不运行它。

## 面板的模式

面板有两种模式，显示在它底部状态条的左边，样式、颜色和位置都和编辑器自己的状态栏显示模式的那一块一样（用 lualine 时就是 lualine 的模式块）。

- **INSERT** 是消息输入框。输入的内容进到里面。
- **BROWSE** 是对话。按键是 vim 的：你在各行之间移动、折叠和复制它们，并回答卡片。

`Esc` 从 INSERT 回到 BROWSE。`i` 或 `o`（光标在你离开时的位置）、`A`（在草稿末尾）或 `Ctrl+j` 从 BROWSE 进入 INSERT；在最后一行按 `j` 或 `Down` 也一样：走到头再也不能往下时，它继续走进输入框，光标在你离开时的位置。很长的最后一行会先滚到底；按住不放的键停在最后一行，所以绝不会往草稿里打出 `jjjj`。空标签页上输入框是最后一站，所以同样的键先走过各行，再进入输入框。等待回答的卡片、问题、打开的列表和搜索行会把 `j` 留给自己。`i`、`o`、`A`、`f` 和这里的 `j` 只对单独按下的键起作用：如果在它之后大约四分之一秒内又按了别的键，它什么也不做，所以回复刚到时打出的句子开头几个字母不会启动 HINT，也不会在一个词中间打开输入框。

底部状态条右侧显示 agent 的模型；agent 说明了 effort 时，旁边还会显示 effort。在 Claude 标签页上，点击其中任何一个会打开 `/model` 或 `/effort` 列表。Claude Code 只有被问到时才说出自己的 effort，所以要在这个标签页里运行过 `/model` 或 `/effort <level>` 之后才会显示，点一下模型就是一种问法。Codex 标签页显示 Codex 报告的内容，不提供列表。

### 在 INSERT 里

| 按键 | 作用 |
|---|---|
| `Enter` | 发送；回合运行期间，把消息插进这个回合而不打断它（agent 在下一步读到它）；标签页的 agent 不能在回合中途接收消息时，就先留着，等回合结束再发 |
| `Ctrl+Enter` | 立刻发送：中断正在运行的回合，然后发送留着的消息和这条消息 |
| `Shift+Enter`、`Alt+Enter`，或者反斜杠再加 `Enter` | 换行（反斜杠会被它替换掉） |
| `Up` | 在第一行时：先把留着的消息取回，然后逐条翻看之前的提示（已经插进回合的消息不会取回） |
| `Down` | 在最后一行时，并且已经在翻看之前的提示：再往前走一条 |
| `Ctrl+r` | 搜索之前的提示；`Enter` 把其中一条放进输入框 |
| 开头的 `/` | 输入框是单独一行、光标还在第一个词里时，打开命令菜单：列出这个标签页接受的命令（Claude Code 自己的，或 ACP agent 报告的；Codex 标签页没有），继续输入会缩小范围，输入空格就关闭 |
| `Up` / `Down` / `Ctrl+n` / `Ctrl+p` | 命令菜单打开时：移动选中项（菜单显示期间这些键不会去翻之前的提示） |
| `Tab` | 命令菜单打开时：补全选中的命令并加一个空格；`Enter` 在名字还没写全时也一样，写全之后才发送 |
| `Ctrl+w` / `Ctrl+u` | 删除一个词 / 删到行首 |
| `Ctrl+a` / `Ctrl+e` | 把光标移到当前行的行首 / 行尾 |
| `Ctrl+c` | 中断正在运行的回合；空闲时，把输入框清空并存入历史 |
| `Ctrl+y` | 批准这个标签页里等得最久的卡片，agent 的[编辑器工具](editor-tools.zh-CN.md#cards)的卡片除外（见[回答一张卡片](#answering-a-card)） |
| `Ctrl+g` | 在 Neovim 里编辑草稿；`:wq` 把它带回来；`:q!` 什么都不改，除非你已经用 `:w` 保存过草稿，这时保存的文字会回来 |
| `Ctrl+o` | 详细视图（BROWSE 里也有） |
| `Shift+Tab` | 轮换标签页的模式：Manual、Auto、Bypass，然后再回到 Manual（没有 Manual 模式的 agent：在 Auto 和 Bypass 之间）；进入 Bypass 会先问，并且只能从 Auto 进入，回答 `y` 还会批准这个标签页里已经在等的 Claude Code 提问（你的 ask 规则引起的也算），并替你回答之后的提问，见[Claude Code 自己提问时](permissions.zh-CN.md#when-claude-code-itself-asks)；Claude 问你的选择题它永远不会回答（[Claude 的提问](#claudes-questions)） |
| `Esc` | 停止输入，回到 BROWSE；命令菜单打开时，先关闭菜单并保留文字 |
| `?` | 按键列表，在输入框为空时 |

在 Claude 标签页中，单独输入以下七个斜杠词会运行 Eitri 自己的操作；仪表板的输入框和对话中的输入框都支持：

| 词 | 操作 |
|---|---|
| `/help` | 打开 `?` 按键表 |
| `/status` | 显示当前标签页的信息（`tab.info`） |
| `/resume` | 打开会话选择器 |
| `/memory` | 在编辑器中打开项目的 `CLAUDE.md`；文件不存在时显示 `no such file: CLAUDE.md` |
| `/rewind` | 保留草稿并显示 `/rewind is not in Eitri — c reviews and reverts this turn's edits`（Eitri 没有 `/rewind`；用 `c` 审阅并撤销本轮的修改） |
| `/tasks`（或 `/bashes`） | 打开[后台任务列表](#the-background-tasks-list) |
| `/workflows` | 打开后台任务列表，并定位到动态工作流 |

前四个操作会清空草稿。这些词都不会给 Claude 发送消息。其他斜杠命令的行为不变：`/clear`、`/compact`、`/cost`、`/context`、`/usage`、`/model`、`/mcp`、`/agents`、`/doctor` 和 `/output-style` 会产生效果；单独的 `/model` 或 `/effort` 会打开选择器。`/login` 和 `/config`，以及带参数的 `/resume`，需要完整终端，会被拒绝并显示消息。其他词，包括提示词模板和你自己的命令，按输入的原样发出。`?` 列出能用的 CLI 命令。在 Codex 标签页或你注册的 agent 的标签页中，这七个词都作为文字发出，没有 Claude 命令的拦截或选择器。

### 在 BROWSE 里

| 按键 | 作用 |
|---|---|
| `j` / `k`，或 `Down` / `Up` | 下一行 / 上一行；很长的一行会先滚动；在最后一行单独按 `j` 进入输入框 |
| `h` / `l` | 这一行里上一个 / 下一个按钮 |
| `gg` / `G` | 第一行 / 最后一行 |
| `[[` / `]]` | 你自己的上一条 / 下一条提示 |
| `]p` / `[p` | 下一张 / 上一张等待回答的卡片（会循环；移动光标，不回答任何东西） |
| `Ctrl+d` / `Ctrl+u` | 向下 / 向上半页 |
| `Ctrl+e` / `Ctrl+y` | 向下 / 向上一行 |
| `Ctrl+f` 或 `PageDown` / `PageUp` | 向下 / 向上一屏（`Ctrl+b` 是前缀，所以往回翻用 `PageUp`） |
| `zt` / `zz` / `zb` | 把这一行放到视图的顶部 / 中间 / 底部 |
| `Enter`，或 `za` | 显示或隐藏一个工具的结果，或一段折叠起来的连续调用 |
| `zo` / `zc` | 只展开 / 折叠那个折叠项 |
| `zh` / `zl` | 把这一行里的表格向左 / 向右滚动 |
| `Ctrl+o` | 详细视图：每个结果、更长的摘录、没有折叠的连续调用 |
| `y` | 复制这一行（消息、命令或路径），或 HINT 落到的那个代码块；复制的内容也会进入主选区（primary selection），所以鼠标中键也能粘贴 |
| `Y` | 复制一个工具行的全部输出 |
| `Ctrl+g` | 在 Neovim 的一个临时缓冲区里打开这一行的全部文字；在一张只改一个文件、还在等待的编辑卡片上（Claude 的 `Edit`、`MultiEdit` 或 `Write`，Codex 新增一个文件的改动，ACP 智能体对一个文件的编辑），则在 Neovim 的一个新标签页里把磁盘上的文件和这次编辑之后的文件并排对比，两边都只读（`q` 关闭；见[审阅一个回合](turn-review.zh-CN.md#before-you-approve)）；在 agent 的[提议](editor-tools.zh-CN.md#proposals)的卡片上，对其中一个文件显示同样的对比，每按一次换下一个文件；其他卡片照旧显示它的文字 |
| `gf` | 在编辑器里打开这一行上的文件路径，包括权限卡片或编辑行的文件标题里的路径（有多个时按字母选） |
| `gx` | 在浏览器里打开这一行的网页链接（有多个时按字母选） |
| `/` | 在对话里搜索；`Enter` 保留匹配，`Esc` 返回 |
| `n` / `N` | 下一个 / 上一个匹配，循环 |
| `c` | 审阅这个标签页最近一个回合期间磁盘上改了什么（见[回合审阅](turn-review.zh-CN.md)） |
| `Ctrl+c` | 中断正在运行的回合（`Esc` 永远不会） |
| `r` | 这个标签页的会话结束后，在其中开始一个新会话 |
| `f` | HINT |
| `:` | 命令行 |
| `?` | 按键列表；`?`、`Esc` 或 `q` 关闭它 |

很多键前面可以加数字：`3j` 是三行，`3G` 或 `3gg` 是第 3 行，`2]]` 是两条提示，`2gt` 是第 2 个标签页，`5 Ctrl+e` 是五行，`]p` 和 `[p` 则往前走那么多张卡片。`0` 可以接在数字后面，但不能用来开始一个数字。

在 `g`、`z`、`[`、`]` 或 `Ctrl+w` 之后，如果按的键不能和它们组成上面任何一对，这个序列就会什么都不做地结束，所以 `g` 后面跟 `d` 不会拒绝一张卡片。

<a id="tabs-and-the-leader"></a>
### 标签页和 leader

`<leader>` 是 `Space`，除非你的 Neovim 配置把 `mapleader` 设成了面板可以使用的单个键。按下它之后，会出现一张简短的列表，列出后面可以接什么。这些键在 BROWSE 里有效。

| 按键 | 作用 |
|---|---|
| `H` / `L`、`[b` / `]b`、`gT` / `gt` | 上一个 / 下一个标签页（`2gt` 是第 2 个标签页） |
| `<leader>b b` | 你之前所在的标签页 |
| `<leader>b d` | 关闭这个标签页，会先问一个 `y/n` |
| `<leader>b o` | 关闭其他所有标签页，会先问一个 `y/n` |
| `<leader>f n` | 新建标签页 |
| `<leader>,` | 切换标签页 |
| `<leader>/` | 在对话里搜索 |
| `<leader>i` | 这个标签页的详情 |
| `<leader>t` | 在终端里继续这个对话（Claude 标签页；Codex 或已注册 agent 的标签页会提示这还不可用） |
| `<leader>m` | 轮换标签页的模式（和 `Shift+Tab` 一样） |
| `<leader>M` | 选择模型（和点击状态带的模型一样）。仅限 Claude 标签页；Codex 或已注册代理的标签页会提示此功能不可用。回合运行时会提示等回合结束后再选择。 |
| `<leader>E` | 选择推理力度（和点击状态带的推理力度一样）。仅限 Claude 标签页；Codex 或已注册代理的标签页会提示此功能不可用。回合运行时会提示等回合结束后再选择。 |
| `<leader>k` | 在 Claude 标签页里，先问一个列出任务的 `y/n`（`Stop 2 shells, 1 monitor? y/n`），然后停掉所有后台任务，对话继续；只在没有回合运行时可用。在 Codex 标签页里，停止它留在后台运行的命令，这会结束它的会话（之后可以恢复），会先问一个 `y/n` |
| `Space e`（`<leader>e`） | 为当前标签页开关编辑器跟随；默认关闭，不会移动你的光标 |
| `<leader>?` | 按键列表 |

你可以在[自己的配置](configuration.zh-CN.md)里修改它们。

<a id="the-background-tasks-list"></a>
### 后台任务列表

在 Claude 标签页里输入 `/tasks`、`/bashes` 或 `/workflows`，会打开 Claude Code 在后台运行的东西的列表：shell、monitor、local agent 和动态工作流，然后是已经结束的任务，以及折叠起来的一行 Claude Code 自己的系统任务。列表打开期间，按键都归它，最下面一行写着主要的几个键。

| 按键 | 作用 |
|---|---|
| `j` / `k`，或 `Down` / `Up` | 下一个 / 上一个任务；在任务详情里则是滚动 |
| `Enter` | 任务详情：shell 或 monitor 显示它的命令和输出的末尾，运行期间每秒重新读取一次；工作流显示它的阶段和 agent |
| `x` | 立即停掉选中的任务，只停这一个 |
| `o` | 在 Neovim 里以只读方式打开详情里显示的输出（最后 8 KiB） |
| `PageDown` 或空格 / `PageUp` | 向下 / 向上滚动一页 |
| `Home` / `End` | 滚到顶部 / 底部 |
| `q` 或 `Esc` | 从任务详情返回；在列表里则关闭列表 |

列表显示什么，见[Codex 与其他 agent](agents.zh-CN.md#background-tasks-and-tasks)。

<a id="claudes-questions"></a>
### Claude 的提问

Claude 用选择题问你时，提问框会取代输入框，直到你回答为止，下面这些键在提问框里起作用。在 BROWSE 里，活动行显示 `⚑ Claude has a question — i to answer`，按 `i` 回到提问框。

| 按键 | 作用 |
|---|---|
| `1` ... `9` | 选这一行，停在当前问题上；选项之后的两行是 `Type something`（你自己写的回答）和 `Chat about this`，按 `Chat about this` 的数字只会把光标移过去 |
| `j` / `k`，或 `Down` / `Up` | 在选项之间移动 |
| 空格 | 选定光标所在的选项，停在当前问题上（在可以多选的问题里是选上或取消）；在 `Chat about this`、`Submit answers` 或 `Cancel` 上不起作用 |
| `Enter` | 在只能选一个的问题里，选定光标所在的选项并进入下一步：下一个问题、汇总页，或者只有这一个问题时直接发出；在可以多选的问题里，选上或取消这个选项，在它的 `Next` 或 `Submit` 上则进入下一步或发出；在 `Type something` 上开始输入；在 `Chat about this` 上告诉 Claude 你想先聊一聊；在汇总页上，`Submit answers` 发出回答，`Cancel` 取消提问 |
| `Tab` / `Shift+Tab`，或 `l` / `h` | 下一个 / 上一个问题；问题不止一个时，最后一个之后是汇总回答的一页 |
| `Ctrl+Enter` | 发出所有回答，或者显示哪些问题还没回答 |
| `n` | 给带预览的选项加备注 |
| `Ctrl+g` | 在 Neovim 里编辑你在 `Type something` 下写的回答 |
| `Ctrl+k` | 回到对话，提问继续等着 |
| `Esc` | 在自己写的回答里：回到选项；其他地方：取消提问，这会停掉当前回合 |

提问框出现之前就按住的键不会选中任何东西，按住不放产生的重复按键也不会。关于提问的更多内容，见[Codex 与其他 agent](agents.zh-CN.md#claudes-questions)。

<a id="selecting-text"></a>
### 选择文字

BROWSE 里的 `v` 在光标所在的那一行上放一个插入符，`V` 直接开始行选择。

- 在插入符模式里：`h` `l` 按字符移动，`j` `k` 按屏幕行移动，`w` `b` `e` 按词移动，`0` 和 `$` 到行首和行尾，`gg` 和 `G` 到对话的第一个和最后一个字符；数字加在动作前面。`v` 或 `V` 从那里开始选择，`Esc` 回到 BROWSE。
- 在 VISUAL（`v`，按字符）和 V-LINE（`V`，按行）里：同样的动作扩展选区，`o` 交换移动的是哪一端，`v` 和 `V` 在两种模式之间切换（本模式自己的键回到插入符），`y` 复制高亮的内容（到剪贴板和主选区）并回到 BROWSE，`>` 把它引用到你的消息里并开始输入，`Esc` 回到插入符。
- `Ctrl+e` 和 `Ctrl+y` 滚动一行，不移动插入符或选区，`Ctrl+c` 仍然中断正在运行的回合。其他任何普通的键都会结束选择，并给出一条点出这个键的提示；带 `Ctrl` 或 `Alt` 的其他组合键会被忽略。

<a id="answering-a-card"></a>
## 回答一张卡片

权限卡片在对话里等着你，紧挨着它所拦住的那次工具调用。在 BROWSE 里：

| 按键 | 作用 |
|---|---|
| `a` | 批准光标下的卡片，或者拦着光标下那次工具调用的卡片 |
| `d` | 拒绝它 |
| `D` | 带理由拒绝：按键转到卡片的理由输入框，在那里按 `Enter` 发出拒绝 |
| `h` / `l`，然后 `Enter` | 走到卡片的某个按钮上并按下它 |
| `]p` / `[p` | 把光标移到下一张 / 上一张等待中的卡片 |

在 INSERT 里，`Ctrl+y` 批准这个标签页里等得最久的卡片，状态条会点出它将要批准的那张卡片。输入框里没有用来拒绝的键，因为拒绝可以带一个你想输入的理由。agent 的[编辑器工具](editor-tools.zh-CN.md#cards)的卡片不归 `Ctrl+y` 管：先按 `Esc`，再在卡片上按 `a` 或 `d`。在这种卡片上按 `D` 不带理由。

这些键只有单独按下时才回答。如果在之前或之后大约四分之一秒内按过别的键，`a`、`d`、`D` 和按钮上的 `Enter` 什么都不做；`Ctrl+y` 以同样的方式被拒绝，在 readline 的删除键（`Ctrl+u`、`Ctrl+w`、`Ctrl+k`）之后也是，在它指向的那张卡片变化后的四分之一秒内也是。按键被拒绝时，状态条会说明原因。它们都不接受数字，按住不放的重复键绝不会回答，按住 `Ctrl`、`Alt`、`Super` 或 `AltGr` 的键也不会回答（`D` 是用 `Shift` 打出来的）。按钮上的 `Enter` 只有在 `h`、`l` 或 `Tab` 刚把焦点移到它上面之后，或者 HINT 刚落到它上面时才回答。已经结束的会话的卡片无法回答。

<a id="hint"></a>
## HINT

BROWSE 里的 `f`，或者任何地方的 `<prefix> f`，会给所有你能跳到的东西标上标签：顶栏的各项、编辑器、显示着的终端，以及面板里每个可见的行、代码块、链接、按钮和消息输入框。输入某个标签的字母（它们取自 `asdjklghweruio`）就跳到那里。`Backspace` 删掉一个字母，`Esc` 取消。落到一行上，光标就移到那一行；落到一个链接上，它就获得焦点，`Enter` 打开它；落到一个代码块上，就可以用 `y` 复制它。再按一次 `f` 什么都不做，因为 `f` 永远不是标签。

<a id="the-command-line"></a>
## 命令行

BROWSE 里的 `:`（或任何地方的 `<prefix> :`）在面板底部打开一个命令行。它不是 vim 的：只接受三个命令，所以出于习惯敲出来的 `:ls` 或 `:d` 落在这里，绝不会落到一张卡片上。`Enter` 运行它，`Esc` 关闭它；其他任何东西都会显示一条说没有别的命令的提示。

| 命令 | 作用 |
|---|---|
| `:tabnew` | 打开默认 agent 的新标签页，和 `<prefix> c` 一样。`:tabnew codex`，或 `:tabnew` 加上在 `init.lua` 里注册的 agent 的名字，会打开一个运行该 agent 的标签页。任何其他词都会被拒绝，并给出可接受的词的列表 |
| `:trust` | 检查这个项目自己为屏幕上那个标签页的 agent 准备的配置（对 Claude 是它的 `.claude/`、`.mcp.json` 和 `CLAUDE.md`），并问是否信任它；之后启动的会话会加载它。没有什么可信任的，或者已经信任过时，它会说明 |
| `:untrust` | 忘掉这个信任；之后启动的会话把项目的配置排除在外。正在运行的会话不变 |

信任意味着什么，见[权限](permissions.zh-CN.md)。

<a id="your-tmux-keys"></a>
## 你的 tmux 键位

如果你用 tmux，Eitri 会从 tmux 自己读的那几个文件（`/etc/tmux.conf`、`~/.tmux.conf`、`$XDG_CONFIG_HOME/tmux/tmux.conf`、`~/.config/tmux/tmux.conf`，以及它们 `source-file` 进来的文件）里读出你的前缀键和前缀表绑定，凡是有对应 Eitri 动作的 tmux 命令都会照搬过来。它只读这些文件，从不启动或询问 tmux。一条提示会说明拿到了什么（每次导入内容有变化时出现一次），`<prefix> ?` 会逐行列出没能拿过来的那些行和原因。Eitri 没有对应物的东西，以及任何需要 tmux 去运行什么的东西，都会被跳过。

你 tmux 配置里的 `unbind -a` 也会去掉 Eitri 那些类似 tmux 的默认键，但 Eitri 自己的键（HINT、模块键、`?`、面板重新加载、沉浸模式，以及 tmux 里没有对应命令的标签页键）会保留。

要关掉这个导入，在你的 [`init.lua`](configuration.zh-CN.md#initlua) 里写上 `eitri.config.set("keymap.from_tmux", "off")`（也可用 Lua `false`；`true` 和 `"on"` 表示开启，也是默认值；任何其他值都会让启动停下，并点出这个设置的名字）。你在那个文件里写的 `eitri.keymap` 调用永远优先于从 tmux 拿来的东西；例如，导入关掉时，`Ctrl+a` 前缀就是 `eitri.keymap.prefix("C-a")`。前缀必须是 `Ctrl` 或 `Alt` 组合键，或者功能键。这些调用本身见[配置](configuration.zh-CN.md)。
