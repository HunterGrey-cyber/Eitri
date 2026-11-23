[English](many-agents.md) | 简体中文
<!-- translated-from: many-agents.md sha256=e31440e6195db79cd262592eac07c05dc8809f3c3bdfe5c0f81c490c265cadce -->

# 同时用很多 agent

你在好几个标签页、好几个聊天视图、好几个窗口里同时跑 agent 时，要问的是：现在哪一个需要你。Eitri 用同一组行来回答：你机器上的每个 Eitri 进程，为它的每个 agent 标签页发布一行（它是谁、处于什么状态、从什么时候起、是不是在等你），下面讲的一切都读这些行。三个自带页面把它们显示出来，两个键带你走到在等你的 agent 那里，`eitri agents` 把它们打印到你的终端和状态栏里，你自己写的页面可以通过 `agents` 能力读到它们。

这里没有任何东西替你回答。一行只表明有一张卡片在等；卡片在聊天里回答，用那个聊天的按键。走到一张卡片，不会在里面按下任何键。

这一层默认是开着的。不想要的话，[一行配置就能关掉](#turn-it-off)。

<a id="the-three-roles"></a>
## 三种分工

这些页面各有各的活，可以同时开着，并且共用一个选中项和一轮（见下文），所以你在一个里做的事，会在所有窗口的其他页面里看到。

| 分工 | 页面 | 放在哪里 | 用来做什么 |
|---|---|---|---|
| 扫一眼 | Strips，`agents-strips` | 窗口左边的侧栏，用 `<prefix> U` 打开之前是隐藏的 | 谁需要你，一个键就能看到；行永远不重新排序，所以一行总在你上次看到的位置 |
| 动手 | The pass，`agents-pass` | 编辑器上方的一行 | 什么需要你，一次一个：在等的 agent，最久的在前，这一轮走到哪了，下一个和回退；只有在有东西等、或者一轮在进行时才出现 |
| 管理 | 表格，`agents-top` | 主区域里的一个模块，打开之前是隐藏的 | 所有东西：排序、过滤、新开一个 agent、请求停掉一个 |

它们和别的模块一样用键拿到焦点：Strips 打开后，从编辑器按 `<prefix> Left` 到 Strips，`<prefix> Up` 到 The pass，`<prefix> o` 依次走过所有模块；页面自己的键打开它并让它拿到焦点，焦点在它上面时再按一次就把它隐藏。Strips 自带一个键 `<prefix> U`（挨着下文那一轮用的 `u` 和 `B`）；表格只有在你给它一个键时才有，见下面。

三个都是随 Eitri 一起发布的插件，在 `init.lua` 加载它们之前是关着的（[插件](configuration.zh-CN.md#plugins)）；想在当前窗口里试一个，在空标签页的 Pages 下面它的那一行上按 `Enter`，或者运行 `:Eitri plugin load agents-top`（只在这次运行里有效）：

```lua
eitri.plugin.load("agents-strips")                -- prefix then U shows or hides the sidebar
eitri.plugin.load("agents-pass")
eitri.plugin.load("agents-top", { key = "g" })    -- prefix then g shows or hides the table
```

`eitri.plugin.load("agents-strips", { key = "y" })` 给 Strips 换一个键，`{ key = false }` 让它没有键。如果 `U` 已经被别的用上了（你 `init.lua` 或 tmux 配置里的一个绑定，或者另一个你写了 `key = "U"` 的页面），就归那边：Strips 加载时没有键，启动 Eitri 的那个终端里会说明原因。打开过 Strips 之后，你保存的布局会让它在下次启动时还开着；以前启动时就显示 Strips 的 Eitri 保存的布局也让它保持显示，在那里按 `<prefix> U` 就能把它隐藏。

Strips 有两个设置：`narrow`（窄条：给一个约 62 像素宽的模块画，只有一个图标、在等的状态和标签页编号，而不是完整的侧栏）和 `show_idle`（`false` 时空闲的行留在原位，但只画它们的图标）：

```lua
eitri.plugin.load("agents-strips", { narrow = true })
```

`narrow` 改变的是 Strips 画什么，而不是它那一列有多宽：这一列仍占窗口原来的份额（五分之一）。用你自己的一条[打开规则](configuration.zh-CN.md#where-a-new-module-opens)以像素给窄条定宽；以像素给的宽度不随窗口宽窄而变：

```lua
eitri.layout.rule("lua:agents-strips", { at = "left-edge", size = "62px" })
```

规则在这一列第一次打开时决定它的位置和宽度；你保存过的布局保持这一列原来的样子。关掉再显示，窄条仍是原来的宽度。拖动它的边缘，或者在按键在它上面时用 `<prefix> Ctrl+Left`/`Ctrl+Right`，可以按像素改宽度（16 到 400）。除了像素宽度，这一列还保留一个占窗口的份额，起初是最小的份额（二十分之一），随你拖动宽度而增减；只有在像素宽度不起作用的地方才会看到这个份额：较旧版本的 Eitri 读到的布局里，或者窄条被移进上下排列的一列模块时。

列窄的时候，完整的侧栏保留标签，也就是让你分清各个 agent 的那部分：先省掉工具名，再省掉时长。鼠标停在一行上会显示它的全名。

这些页面在完整的 Eitri 窗口里和[合成器窗口](compositor-windows.zh-CN.md)的页面窗口里运行。它们不在[伴随窗口](companion.zh-CN.md)里、也不在 Mac 上运行，那里不加载插件；伴随面板仍然会发布它的行，所以另一个窗口里的页面，以及它旁边终端里的 `eitri agents`，都能显示这些行。

用[合成器窗口](compositor-windows.zh-CN.md)时，每个页面都是一个独立的窗口，由你的合成器决定位置和大小：Strips 是一个普通的平铺窗口，而不是左边的一列；The pass 也不会自己出现和消失，因为页面窗口只有你关掉它才会关。在那里给 The pass 一个键（`eitri.plugin.load("agents-pass", { key = "P" })`），想看时再打开它；没有它，`<prefix> u` 和 `<prefix> B` 也照样能用。

<a id="the-shared-grammar"></a>
## 共用的一套操作

每个页面都用 agent 标签页自己的键来称呼一行，所以它们对"选中的是哪一行"意见一致。

- **一个选中项。** 在任何一个页面里移动光标，所有显示这一行的窗口里的所有页面里，光标都跟着移动。
- **一轮。** 一轮就是依次走过在等你的 agent，等得最久的先走。开始它、推进它、结束它，在三个页面里、在你所在的任何窗口里都看得见。

页面里的键：

| 键 | 作用 |
|---|---|
| `j` / `k` | 按页面自己的顺序移动选中项 |
| `Enter` | 在需要你的行上：从这里开始这一轮，或者把正在进行的一轮移到这里，并去到它的聊天，卡片摆在前面。在其他行上：去到它的聊天 |
| `c` | 在选中行所在的项目里新开一个 agent 标签页 |
| `x` | 请求停掉选中的 agent（见下） |
| `/` | 过滤：Strips 把不匹配的行变暗，表格把它们隐藏 |
| `Esc` | 把按键交给编辑器。在 The pass 里，如果有一轮正在进行，`Esc` 改为结束这一轮，回到这一轮开始的地方 |

Strips 另有 `m` 标记一行。表格另有 `s` 循环切换排序（状态、时间、项目、标签），`S` 反向，并且它是唯一会重新排序的页面，因为你是打开它、读、再关上它，就像用 `top` 一样。它自己没有关闭键：再按它的模块键（上面的 `<prefix> g`）就会把它隐藏。

- **回答仍然在聊天里。** 没有任何页面有回答卡片、提问或信任问题的键。当这一轮当前的那个 agent 不再需要你（你在它的聊天里回答了），这一轮会自己移到下一个，而下一个聊天里的卡片只能在卡片模式里回答，卡片模式要你自己进入（`Alt-n`），所以你还在按的键，不会去回答一张你没读过的卡片。任何页面、`prefix u` 或 `prefix a` 把一个聊天带到前面时，同样要等这四分之一秒。
- **停止在聊天里问。** `x` 把聊天带到前面，它自己画出 `Stop this turn? (y/n)`（Claude Code 在做聊天里显示不出来的事、行显示为 `running` 时也是这样），只有后台任务在运行时画出 `Stop the background work? (y/n)`。`y` 停止；`n` 或 `Esc` 什么也不做。什么都没在运行的 agent 会被拒绝（`idle`），在伴随面板里也一样，而且什么都不会被带到前面。

<a id="keys-without-a-page"></a>
## 不用页面的键

| 键 | 作用 |
|---|---|
| `<prefix> u` | 从等得最久的 agent 开始这一轮，或者去到下一个还没去过的；一轮走到哪个窗口，那个窗口就被带到前面 |
| `<prefix> B` | 结束这一轮，回到它开始的地方 |

没有谁在等时，`<prefix> u` 会在状态行显示两秒 `no agent waits for you`，并结束正在进行的一轮。一轮结束之后，`<prefix> B` 仍然能回到它开始的地方，直到一轮空闲满十分钟而过期。同一个窗口里的几个[信任问题](permissions.zh-CN.md#trusting-a-project)算一站，因为一次回答就能解决它们。这两个键在伴随窗口里也能用，在合成器窗口的页面窗口里也能用：从那里的页面开始的一轮，会回到那个页面。

在 GNOME 上，被带到前面的窗口由发出请求的那次按键来提升；在 Hyprland、sway 和 niri 上，由窗口管理器按进程来聚焦它。

<a id="turn-it-off"></a>
## 关掉它

```lua
eitri.config.set("agents.enabled", false)
```

一个窗口里 `agents.enabled` 为 false 时，这个窗口不发布行，不启动读取器，也不启动控制套接字，`<prefix> u` 和 `<prefix> B` 只会在状态行显示 `agents are off in this window`；它的行立刻从其他窗口的页面里消失。它自己的页面显示 `agents are off in this window`。这个键在窗口启动时读取。

<a id="eitri-agents"></a>
## `eitri agents`：在终端和状态栏里

```sh
eitri agents                 # a table: PROJECT WIN # AGENT STATE AGE LABEL
eitri agents --json          # one line: the whole view
eitri agents --watch         # the view, then one line per change
eitri agents --schema 1      # refuse to run unless the rows are this version
```

`WIN` 是窗口在它的项目里的名字（`1`、`2`……，名字见下文），所以每个项目的第一个窗口都显示 `1`：`PROJECT` 和 `WIN` 合起来才指明一个窗口。

它读的是和页面相同的行，不需要任何窗口来回答，所以一个卡住的窗口拖不住它。没有 Eitri 窗口在运行时，表格显示 `no window shares agents`，JSON 里没有行；退出码是 0。不认识的参数或不支持的模式版本，退出码是 64；读取出错是 1。管道被关闭时，`--watch` 安静地结束。

**tmux。** `eitri-waiting` 是一个小脚本，`~/.local/bin/eitri-waiting`：

```sh
#!/bin/sh
eitri agents --json | jq -r '[.rows[] | select(.class == "needs")] | length | if . > 0 then "⚑ \(.)" else "" end'
```

然后 `~/.tmux.conf` 显示它：

```
set -g status-interval 5
set -g status-right '#(~/.local/bin/eitri-waiting) %H:%M'
```

**Waybar。** `--watch` 在有变化时写一行；把它当作触发器，每次都重新要完整的视图。`~/.local/bin/eitri-waybar`：

```sh
#!/bin/sh
show() {
  eitri agents --json | jq -c '[.rows[] | select(.class == "needs")] as $w | {
    text: (if ($w | length) > 0 then "⚑ \($w | length)" else "" end),
    class: (if ($w | length) > 0 then "waiting" else "quiet" end),
    tooltip: ($w | map("\(.project.name) \(.window.name)·\(.number) \(.label)") | join("\n"))
  }'
}
eitri agents --watch | while read -r _; do show; done
```

```json
"custom/agents": { "exec": "~/.local/bin/eitri-waybar", "return-type": "json" }
```

**每次有新的等待就一行。** 接下来的两个脚本要知道的是 agent 什么时候开始等，而不是有几个在等。`eitri-on-wait` 每发生一次就打印一行，写出 agent 所在的项目、窗口、标签页和状态（`eitri 1·2 card`）。它启动时已经在等的 agent 不会打印；停止等待后又重新等的会打印。`~/.local/bin/eitri-on-wait`：

```sh
#!/bin/sh
# One line each time an agent starts waiting for you: "<project> <window>·<tab> <state>".
# Agents already waiting when it starts are not printed; an agent that stops waiting and waits again is.
# A tab that moves to another window keeps its key and gets a new id, so a wait is kept by key with the id it has,
# and a departure counts only for the id that left.
eitri agents --watch | jq -n --unbuffered -r '
  def slot: if (.key // "") == "" then .id else .key end;
  def line: "\(.project.name) \(.window.name)·\(.number) \(.state)";
  def waits($rows): [$rows[] | select(.class == "needs") | {key: slot, value: {id, line: line}}] | from_entries;
  foreach inputs as $e ({waiting: {}, say: []};
    if $e.type == null then {waiting: waits($e.rows), say: []}
    elif $e.type == "reset" then .waiting as $old | waits($e.view.rows) as $now
      | {waiting: $now, say: [$now | to_entries[] | select(.key as $k | $old | has($k) | not) | .value.line]}
    elif $e.type == "row" then ($e.row | slot) as $k
      | if $e.row.class != "needs" then {waiting: (.waiting | del(.[$k])), say: []}
        elif .waiting[$k] then {waiting: (.waiting + {($k): {id: $e.row.id, line: ($e.row | line)}}), say: []}
        else ($e.row | line) as $l | {waiting: (.waiting + {($k): {id: $e.row.id, line: $l}}), say: [$l]} end
    elif $e.type == "gone" then ($e | slot) as $k
      | if .waiting[$k].id == $e.id then {waiting: (.waiting | del(.[$k])), say: []} else .say = [] end
    else .say = [] end;
    .say[])'
```

**桌面通知。** `~/.local/bin/eitri-notify`，从你桌面的自启动项或一个 `systemd --user` 服务启动：

```sh
#!/bin/sh
# A desktop notification each time an agent starts waiting.
eitri-on-wait | while IFS= read -r line; do
  notify-send --app-name=Eitri "An agent waits for you" "$line"
done
```

**你的手机。** [ntfy](https://ntfy.sh) 会把发到你服务器上某个主题的消息推送到你手机上的 ntfy 应用。`~/.local/bin/eitri-ntfy`：

```sh
#!/bin/sh
# A push to your phone through your own ntfy server each time an agent starts waiting.
# NTFY_URL is the topic's address on your server, for example https://ntfy.example.com/my-agents.
: "${NTFY_URL:?set NTFY_URL to your ntfy topic address}"
eitri-on-wait | while IFS= read -r line; do
  curl -fsS -H "Title: An agent waits for you" -d "$line" "$NTFY_URL" > /dev/null
done
```

它发出去的那一行里有项目的文件夹名，这个名字因此会离开这台机器，发到你的服务器；标签来自你的提示词，不在其中。运行方式是 `NTFY_URL=https://ntfy.example.com/my-agents eitri-ntfy`，换成你服务器的地址和一个别人猜不到的主题名。

本页的五个脚本在 Eitri 源码树的 `examples/agents/` 下，录制好的示例行在 `examples/agents/fixtures/` 里。

<a id="write-your-own-page"></a>
## 写你自己的页面

一个在编辑器下方显示有几个 agent 在等的页面。它是 `~/.local/share/eitri/plugins/waiting/` 里的三个文件。

`eitri-plugin.toml`：

```toml
id = "waiting"
title = "Waiting agents"
protocol = 1
site = "site"
entry = "index.html"
capabilities = ["agents"]

[module]
at = "below"
size = 0.08
focus = false
```

`site/index.html`：

```html
<!doctype html>
<meta charset="utf-8">
<button id="n">…</button>
<script src="main.js"></script>
```

`site/main.js`：

```js
const rows = new Map();
const el = document.getElementById("n");
const waiting = () => [...rows.values()].filter((r) => r.class === "needs");
const draw = () => {
  const w = waiting();
  el.textContent = w.length ? `⚑ ${w.length} waiting` : "no one waits";
};
el.onclick = () => {
  const w = waiting().sort((a, b) => a.waiting_since - b.waiting_since);
  if (w.length) eitriPlugin.call("agents.round", { op: "start", key: w[0].key });
};
const load = (view) => { rows.clear(); view.rows.forEach((r) => rows.set(r.id, r)); draw(); };
eitriPlugin.on("agents.row", (r) => { rows.set(r.id, r); draw(); });
eitriPlugin.on("agents.gone", (g) => { rows.delete(g.id); draw(); });
eitriPlugin.on("agents.reset", load);
eitriPlugin.call("host.hello", {})
  .then(() => eitriPlugin.call("agents.watch"))
  .then(load)
  .catch((e) => { el.textContent = e.code === "disabled" ? "agents are off" : String(e); });
```

然后在 `init.lua` 里：`eitri.plugin.load("waiting")`。

点击就是这次调用需要的那个手势。它在等得最久的 agent 那里开始一轮（或者把正在进行的一轮移到那里），那个 agent 的聊天来到前面，卡片摆在最前，和在内置页面里对一行在等的行按 `Enter` 一样。`agents.focus` 去到一行的聊天，但不开始一轮。宿主在向这个文档发送任何事件之前，先回答 `agents.watch`，所以 `load` 不会覆盖更新的行。

带有 `agents` 能力的页面可以发出的调用：

| 调用 | 参数 | 作用 |
|---|---|---|
| `agents.watch` | 无 | 回答完整的视图；之后页面会收到下面的事件 |
| `agents.focus` | `{id, card_first?}` | 去到那一行的聊天；`card_first` 为 true 时，把它的卡片摆在前面 |
| `agents.new` | `{project}` | 在那个项目里新开一个 agent 标签页（视图 `projects` 里的项目键） |
| `agents.select` | `{key}` | 移动共用的选中项 |
| `agents.round` | `{op, key?}` | `op` 是 `start`、`next` 或 `leave`；`start` 带上一行在等的行的 `key`，在那里开始一轮（或者把正在进行的一轮移到那里），并去到它的聊天，卡片摆在最前 |
| `agents.stop` | `{id}` | 去到那一行的聊天，并在那里问 `Stop this turn? (y/n)` |

事件有 `agents.row`（一行变了）、`agents.gone`、`agents.reset`（页面没来得及听到变化之后给的完整视图）、`agents.selection` 和 `agents.round`。不在屏幕上的页面什么也听不到，回来时会收到一次 `agents.reset`。

声明了 `agents` 的页面，在它自己的脚本运行之前，还会得到 `window.eitriAgents`：一个按行键存放行、并应用这些事件的行仓库；每个状态一个图标和一个词（遇到更新的 Eitri 新增的状态，按它的类别回退）；以及通过按键工具包声明的[共用操作](#the-shared-grammar)的各个键，所以你的页面可以和自带的三个并排放。它还会得到 `window.eitriKeys`。[写一个插件](write-a-plugin.zh-CN.md)是给插件作者的完整参考，[你自己做的页面](pages.zh-CN.md)是简短的介绍。

<a id="the-rows"></a>
### 行

一行的每个字段、状态及其判定、完整的视图和 `--watch` 的各行，都在[写一个插件](write-a-plugin.zh-CN.md#the-rows)里，还有每个调用的参数、结果和拒绝的情况。

<a id="limits"></a>
## 限制

- **行里永远没有对话文字。** 没有提示词，没有回复，没有工具输入，没有路径，没有失败原因。标签（label）是唯一来自提示词的内容（聊天给标签页起的标题，截到 40 个字符），它会传给每个页面，也传给任何能读到这些行文件的、你这个用户的进程。Eitri 自己的记录和 agent 的会话记录，对同一个用户而言包含的东西要多得多，所以这些文件没有给这样的进程多添任何它本来读不到的东西。
- **`agents` 和 `net` 不能同时声明。** 能读到你的 agent 的页面，不能同时访问网络；加载时会以 `agents and net cannot be declared together` 拒绝这一对。
- **除了 `agents.watch`，每个调用都需要一个手势。** 页面的模块必须拿着按键，并且在调用之前不到一秒内，页面里发生过一次按键、按钮或触摸。页面自己的脚本伪造不了。`agents.new` 在上一次之后一秒内也会被拒绝。
- **没有任何东西替你回答。** 没有行、调用或事件会回答卡片、提问或信任问题。
- **在你自己的 Neovim 旁边的页面窗口** 只加载五种能力的插件，所以那里会拒绝 `agents`。
- **只列出在运行的窗口。** 没有开着窗口的项目不在视图里，对它调用 `agents.new` 会回答 `no_window`；启动一个窗口不是这些调用做的事。
- **`eitri agents` 只读。** 它不能带你去到某个 agent：在 GNOME 上提升一个窗口，需要发出请求的那次按键或点击，而脚本没有。
- **同时运行两个版本的 Eitri**（升级时留着一个窗口没关）都会显示在较新的页面里。较旧的窗口会忽略较新的文件，并记一行日志。
- **运行时目录不能用时**（`XDG_RUNTIME_DIR` 没设置、也没有可用的临时目录，或者路径对套接字来说太长），这个进程不带这一层运行，并记一行日志；其他一切不变。
- **关闭和重命名不是调用，停止也只是询问。** `agents.stop` 把你带到那一行的聊天，并在那里提问；你在聊天里回答 `y` 之前，什么都不会停止。关闭和重命名留在聊天里。不停止而只中断，以及回答，都不提供。
