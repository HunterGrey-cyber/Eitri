[English](compositor-windows.md) | 简体中文
<!-- translated-from: compositor-windows.md sha256=28f554d0c601dcbc76a95f9cb182cfcdbc7dfbf37e51b31375aaa7cae6e5006c -->

# 合成器窗口

在 Hyprland、sway 和 niri 上，Eitri 把平铺交给你的窗口管理器。每个聊天视图和每个页面都是一个独立的窗口，由合成器的按键、规则、工作区和输出按对待其他窗口的方式来排布。编辑器留在第一个窗口里，下面可以有一个终端。

在这三个合成器上这是默认行为。在别的地方，只要你不另行设置，Eitri 就还是[一个窗口](getting-started.zh-CN.md)，[两个窗口](two-windows.zh-CN.md)和 [companion 窗口](companion.zh-CN.md)也和以前一样。

<a id="the-setting"></a>
## 设置

```lua
eitri.config.set("window.tiling", "auto")
```

| 值 | 作用 |
|---|---|
| `"auto"` | 默认值：下面三项检查都通过就用独立窗口，否则一切留在 Eitri 的窗口里 |
| `"eitri"` | 一切都在 Eitri 的窗口里平铺（打开了窗口 2 的话，也包括窗口 2） |
| `"compositor"` | 在任何 Wayland 桌面上，聊天和页面都是独立窗口 |

下面三条全部成立时，`auto` 才选独立窗口：

1. 会话是 Wayland。
2. 环境变量指明了 Hyprland、sway 或 niri（`HYPRLAND_INSTANCE_SIGNATURE`、`SWAYSOCK`、`NIRI_SOCKET`）。
3. 那个合成器的 socket 存在，并且在 500 毫秒内回答了 `hyprctl version -j`、`swaymsg -t get_version` 或 `niri msg --json version`。

有第三条是因为这些变量可能比合成器活得久：tmux server 会一直留着启动它的那个会话的签名，重新启动的 sway 会留下旧的 `SWAYSOCK`。窗口从不等这个回答。读取模式时回答还没到，这次启动就留在 Eitri 的窗口里。`companion.wm` 不影响 `auto` 检查什么；它仍然决定用哪个适配器移动焦点。

这个设置只在启动时读一次，改动从下一个打开的窗口起生效。有两条提示告诉你发生了什么：

- 在 `auto` 下第一次以独立窗口启动时，会说一次：`Hyprland detected: chats and pages open as their own windows; window.tiling = "eitri" keeps them inside`。[首次启动界面](configuration.zh-CN.md#first-start)开着时，这条提示等界面关闭再出现；写入的文件里有 `window.tiling` 这一行时，它就不再出现。
- 这次启动的方式和上次不同时，会说一次原因：`opened as tiles this time: Hyprland did not answer (window.tiling = "auto")`，或者 `opened as separate windows this time (window.tiling = "auto")`。两种方式下聊天都在同一个文件里，所以换模式不会丢聊天。

`"compositor"` 在 GNOME 和 KDE 上也能用，那里 `auto` 留在 Eitri 的窗口里。窗口由它们的窗口管理器摆放。在 GNOME 上移动焦点需要 [Eitri Shell 扩展](companion.zh-CN.md#gnome-the-extension)；在 KDE 上，边缘键停下并给出现有的提示。这两者都没有试过。Wayland 之外这个设置不起作用，macOS 没有合成器模式。

<a id="when-it-stays-in-one-window"></a>
## 什么时候留在一个窗口里

有三种情况，不管设置怎么写都在 Eitri 的窗口里运行。每种情况写一行日志，并在这个模式确实有可能生效时（`"compositor"`，或 Hyprland、sway、niri 上的 `"auto"`）显示一条提示，提示的开头是 `Chats and pages stay inside this window: `，后面是原因：

| 情况 | 提示里给出的原因 | 为什么 |
|---|---|---|
| 插件自己的窗口 | `this project's page has a window of its own` | 页面保留它的平铺位置和那个窗口给它的权限 |
| 窗口 2 还开着，但没有窗口 1 | `window 2 was left without its partner` | 窗口 1 和它重新连接，而它的工作区需要 Eitri 的平铺 |
| 这个项目已经在另一个 Eitri 窗口里打开 | `this project is open in another Eitri window` | 这个项目的窗口都属于那一个 |

反过来的情况也有：崩溃的窗口 1 留下的聊天或页面窗口，会让下一次启动在这一次里使用独立窗口，不管 `window.tiling` 怎么写，并和它们全部重新连接（`opened as separate windows this time: chat windows were waiting to be relinked`）。

<a id="what-opens"></a>
## 会打开什么

- **窗口 1** 有编辑器和一个底部终端，终端用它的键显示和隐藏。它只有一个工作区，顶栏把其他窗口等待中的卡片和正在运行的回合加在一起显示（`chats: ⚑1 ●2`）。
- **聊天窗口**有一个聊天视图，带自己的标签栏。最多九个，`c1` 到 `c9`。
- **页面窗口**有一个插件页面或 Lua 面板。面板的 id 只能用字母、数字、`.`、`_` 和 `-`，最长 40 字节，开头和结尾都是字母或数字，才有窗口。其他 id 什么也不打开，并用一条提示说明这条规则。

启动布局决定打开哪些窗口。`eitri.layout.default` 显示的每个聊天和页面都按顺序成为一个窗口，其中的 `terminal` 成为底部终端。没有自己的布局时，第一次启动打开 `c1`。布局里隐藏的聊天或页面保持关闭，直到你按它的键。启动时打开的窗口把按键留在编辑器里；你用键打开的窗口会到前面来。

新窗口开在合成器开新窗口的地方，通常在有焦点的窗口旁边。`init.lua` 里摆放聊天或页面的 `eitri.layout` 调用（打开规则、`split`、`float`）仍然成功：模块在自己的窗口里打开，摆放部分被忽略，写一行日志。

每个窗口是一个独立的进程，都会加载你的 `init.lua`。`eitri.window` 用来区分它们：窗口 1 里是 `"1"`，聊天窗口里是 `"c3"` 这样的值，页面窗口里是 `"p:markdown"` 这样的值。开着四个项目（各有自己的窗口 1 和一个聊天窗口）时，每个聊天窗口比同样的聊天放在平铺里多占 51.4 MiB，只开一个项目时多占 62.6 MiB（release 构建，取中位数）。

这个模式下没有窗口 2，合成器的工作区和输出做了它的事。

<a id="app-ids-and-titles"></a>
## 应用 id 和标题

| 窗口 | 应用 id | 标题 |
|---|---|---|
| 窗口 1 | `cn.huntergrey.eitri` | `<project> — Eitri` |
| 聊天窗口 | `cn.huntergrey.eitri.Panel` | `<project> — Eitri chat`，从 `c2` 起是 `<project> — Eitri chat 2` 这样 |
| 页面窗口 | `cn.huntergrey.eitri.Page` | `<project> — <plugin name> · Eitri` |

聊天窗口和 [companion 面板](companion.zh-CN.md)共用一个应用 id，所以一条规则对两者都管用。标题在窗口第一次映射之前设好，之后不再变，因为合成器在窗口打开时读取规则的匹配条件。

<a id="window-rules"></a>
## 窗口规则

规则写在合成器的配置里，Eitri 从不往那里写。每个例子匹配一种窗口。

使用 Lua 配置的 Hyprland（0.55 或更新）：

```lua
hl.window_rule({ match = { class = "^cn.huntergrey.eitri.Panel$" }, workspace = "2 silent" })
```

使用 `hyprland.conf` 的 Hyprland（在 0.56.2 上跑过）：

```
windowrule = match:class ^cn.huntergrey.eitri.Panel$, workspace 2 silent
```

sway，`for_window` 决定对窗口做什么，`assign` 决定它去哪里：

```
for_window [app_id="cn.huntergrey.eitri.Panel"] mark eitri-chat
assign [app_id="cn.huntergrey.eitri.Page"] workspace 2
```

niri：

```kdl
window-rule {
    match app-id=r#"^cn.huntergrey.eitri.Panel$"#
    default-column-width { fixed 700; }
}
```

**你的规则摆放的位置优先。** Eitri 读不到用了哪条规则，所以看结果：新窗口映射到的输出或工作区和有焦点的窗口不同，就是规则把它送过去的。这时 Eitri 既不移动它，也不聚焦它，这次启动如此，之后记住的输出回来时也如此。

<a id="keys"></a>
## 按键

布局键在每个窗口里都关闭，窗口 1 也一样。什么都不会转交给合成器，所以 `<prefix> z` 不会变成全屏。在一个窗口里第一次按关闭的键时显示一条提示，之后在这个窗口里再按只写一行日志：

`Hyprland arranges windows here: use its own keys. window.tiling = "eitri" brings Eitri's layout keys back.`

在每个窗口里都关闭的有：分割、移动和交换模块、容器、布局、放大、调整大小、标记、浮动、scratchpad、工作区、[两个窗口](two-windows.zh-CN.md)的 `window.*` 动词和 `move.to-window`。这个模式没有新增按键。代表这些键的命令也同样关闭，提示也相同：`:Eitri workspace <name>`、`:Eitri open terminal`、对编辑器或终端的 `:Eitri split`，以及对应的 `eitri.layout` 调用。

窗口 1 里：

| 键 | 作用 |
|---|---|
| `Ctrl+h/j/k/l` | 先走 nvim 的分屏，再到底部终端，到边缘后交给合成器移动焦点 |
| `<prefix> e` | 把按键交给编辑器 |
| `<prefix> t` | 显示底部终端并把按键交给它，或者隐藏它，和任何窗口里一样 |
| `<prefix> a`、页面的键 | 把那个窗口提到前面，或者启动它；`<prefix> a` 先去等你回应的聊天窗口 |
| 标签键、`<prefix> !`、`r`、`?`、`:` | 在上一个拿到按键的聊天窗口里运行，那个窗口到前面来；没有聊天窗口时启动 `c1`，在那里运行 |
| `<prefix> x` | 退出编辑器，或者在 y/n 之后结束终端 |
| `<prefix> d` | 隐藏终端；对聊天或页面只写一行日志，因为它们由合成器关闭 |
| `f`、文字大小键、发送前缀、复制模式、沉浸模式 | 和平常一样 |

聊天窗口或页面窗口里：

| 键 | 作用 |
|---|---|
| `Ctrl+h`、`Ctrl+l` | 交给合成器移动焦点 |
| `Ctrl+k`、`Ctrl+j` | 和 [companion 窗口](companion.zh-CN.md)里一样：`Ctrl+k` 从 BROWSE 离开，`Ctrl+j` 从 INSERT 离开 |
| `<prefix> e` | 把窗口 1 提到前面 |
| `<prefix> a`、`<prefix> t`、页面的键 | 请窗口 1 把那个窗口提到前面或启动它 |
| `<prefix> !` | 在 `c1` 里把当前标签页送到一个新的聊天窗口；在其他聊天窗口里把它送回 `c1` |
| `<prefix> x` | 关闭这个窗口，和合成器的关闭键一样 |
| 标签键、`r`、`?`、`:`、`f`、文字大小键、发送前缀 | 在聊天窗口里和平常一样；页面没有聊天，所以它的聊天键在上一个聊天窗口里运行 |

在页面窗口里，同样的键在页面报告的边缘离开。聊天窗口和页面窗口没有顶栏，所以前缀等待时，前缀条浮在窗口右上角。复制模式、沉浸模式和 `<prefix> d` 在那里没有可操作的对象，只写一行日志。

**新开一个空的聊天窗口：** `<prefix> c` 新建一个标签页，再在这个还没有会话的标签页上按 `<prefix> !`，就打开下一个空闲的聊天窗口。标签页留在原处。九个都开着时，提示会说明。

**把标签页送到另一个窗口**是在一个进程里结束它，在另一个进程里恢复它，和重启时一样。正在运行回合的标签页等回合结束再走（`the tab moves to its own window when this turn ends`），再按一次 `<prefix> !` 取消。标签页到达后没有上一回合的审阅。聊天窗口失去最后一个标签页时就关闭；`c1` 保持打开，内容为空。

<a id="closing"></a>
## 关闭

| 发生了什么 | 它的标签页和位置 |
|---|---|
| 你关闭窗口 1 并回答 `y` | **保留**：每个聊天窗口下次启动时都回来 |
| 你用合成器的键或 `<prefix> x` 关闭聊天或页面窗口 | **忘掉**：标签页和位置都丢弃 |
| 聊天窗口里还有标签页在等你回答，它就关闭了 | **保留**：它重新打开时是空的，继续提供那些标签页 |
| 结束信号（`SIGTERM`）关闭窗口 | **保留** |
| 窗口崩溃 | **保留**，按它的键就能再次启动 |

关闭窗口 1 会关闭整组窗口，只要还有窗口开着就一定会问：`close Eitri and its 2 windows? 3 turns are running, 1 queued (y/n)`。

nvim 在窗口 1 里退出时（比如 `:qa`），窗口 1 要关闭，所以会问同样的问题。回答 `n`，这组窗口就留下：底部终端占据窗口并拿到按键，`<prefix> e` 重新启动 nvim。

关闭聊天窗口只在回合运行时才问（`close window? 1 running (y/n)`），空闲时不问就关闭。**这和 `eitri` 模式不同，** 那里关闭窗口总会保留标签页：这里你关闭的聊天窗口会离开标签页列表，和关闭一个标签页一样。对话还在选择器里。

窗口 1 崩溃时，聊天和页面窗口都留着，只是没有编辑器，里面正在运行的回合继续运行。这个项目下一个 Eitri 窗口会和它们全部重新连接。

<a id="coming-back"></a>
## 回来

`window.restore` 决定这组窗口是否回来：打开哪些窗口，每个窗口去哪个输出，底部终端是否显示以及多高。按键最后留在窗口 1。设为 `false` 时由启动布局决定。无论哪种情况，有保存标签页的聊天窗口也都会打开，除非 `agent.restore` 是 `"off"`；标签页是立刻恢复还是等你回答，由 `agent.restore` 决定。

两种模式下标签页都在同一个文件里。保存在 `c3` 的标签页，回到 `eitri` 模式时作为视图 3 打开，在聊天下面的一个平铺位置里，再切回来时又回到 `c3`。你在 `eitri` 模式里的布局原样等着你。

<a id="known-gaps"></a>
## 已知缺口

- 页面窗口里的 `<prefix> ?` 显示上一个聊天窗口的聊天按键表，而不是页面的键。页面自己的 `?` 列出那些键。在窗口 1 里它打开的是同一张表，其中没有只在窗口 1 起作用的键，比如 `<prefix> d`、`[`、`o` 和 `;`。
- `:EitriTutor` 教的是 Eitri 自身布局的按键，其中讲分屏、放大、浮窗、scratchpad 和工作区的课文，在这里碰到的键都是关闭的。它不随模式改变。
- `:EitriMarkdownToggle` 能打开页面的窗口并把按键留在编辑器里，但关不掉它。只有合成器能关。
- 在 Hyprland 上，如果下一个浮动窗口在斜对角，和当前窗口没有共同的行或列，`Ctrl+h/j/k/l` 会停在边缘，不过 Hyprland 自己的焦点键能到达它。互相重叠的浮动窗口交给 Hyprland 决定。
- 逐个关闭窗口的注销，看起来就像你在关闭它们，所以它们的标签页和位置都被忘掉。`hyprshutdown` 就是这样。合成器直接退出则会保留。

<a id="what-is-not-supported"></a>
## 不支持什么

- 把终端作为窗口。窗口 1 里有一个底部终端，别的终端你的合成器想开多少开多少。
- i3 和 X11。
- 让合成器自己的焦点键先走 nvim 的分屏（`SUPER+h` 像 `Ctrl+h` 那样）。Eitri 窗口里的 `Ctrl+h/j/k/l` 是这样的。

只想在任意编辑器旁边放一个聊天的话，在这些合成器上 [`eitri split` 和 `eitri panel`](companion.zh-CN.md) 仍然是更轻的选择。

<a id="what-has-been-tried"></a>
## 试过什么

以下都用替身模型和脚本按键运行，没有在真实硬件上跑过：

- **sway 1.12**，在隔离的无界面会话里：通过了每一行，包括启动之后才回来的输出。
- **Hyprland 0.57**（Lua 配置）和 **0.56.2**（`hyprland.conf` 和 Lua 配置），在虚拟机里：窗口、规则、两个输出上的重启、关闭的键和边缘键都通过。
- **niri 26.04**，在虚拟机里嵌套在另一个合成器中：窗口、规则和焦点移动通过。它的输出、重启和关闭的键没有运行。

GNOME、KDE，以及在这些窗口里运行真实的 `claude` 或 `codex`，都没有试过。欢迎反馈。[已知问题](../known-issues.zh-CN.md)列出了其余的内容。
