[English](many-agents.md) | 简体中文
<!-- translated-from: many-agents.md sha256=f72f9ad4bfaa89501482c36e1cab98c901e7049493f121414a18fcd76323337c -->

# 同时用很多 agent

你在好几个标签页、好几个聊天视图、好几个窗口里同时跑 agent 时，要问的是：现在哪一个需要你。Eitri 用同一组行来回答：你机器上的每个 Eitri 进程，为它的每个 agent 标签页发布一行（它是谁、处于什么状态、从什么时候起、是不是在等你），下面讲的一切都读这些行。三个自带页面把它们显示出来，两个键带你走到在等你的 agent 那里，`eitri agents` 把它们打印到你的终端和状态栏里，你自己写的页面可以通过 `agents` 能力读到它们。

这里没有任何东西替你回答。一行只表明有一张卡片在等；卡片在聊天里回答，用那个聊天的按键。走到一张卡片，不会在里面按下任何键。

这一层默认是开着的。不想要的话，[一行配置就能关掉](#turn-it-off)。

<a id="the-three-roles"></a>
## 三种分工

这些页面各有各的活，可以同时开着，并且共用一个选中项和一轮（见下文），所以你在一个里做的事，会在所有窗口的其他页面里看到。

| 分工 | 页面 | 放在哪里 | 用来做什么 |
|---|---|---|---|
| 扫一眼 | Strips，`agents-strips` | 窗口左边的侧栏 | 谁需要你，始终看得见；行永远不重新排序，所以一行总在你上次看到的位置 |
| 动手 | The pass，`agents-pass` | 模块上方的一行 | 什么需要你，一次一个：在等的 agent，最久的在前，这一轮走到哪了，下一个和回退；只有在有东西等、或者一轮在进行时才出现 |
| 管理 | 表格，`agents-top` | 主区域里的一个模块，打开之前是隐藏的 | 所有东西：排序、过滤、新开一个 agent、请求停掉一个 |

三个都是随 Eitri 一起发布的插件，在 `init.lua` 加载它们之前是关着的（[插件](configuration.zh-CN.md#plugins)）：

```lua
eitri.plugin.load("agents-strips")
eitri.plugin.load("agents-pass")
eitri.plugin.load("agents-top", { key = "g" })    -- prefix then g shows or hides the table
```

Strips 有两个设置：`narrow`（窄条：给一个约 62 像素宽的模块画，只有一个图标、在等的状态和标签页编号，而不是完整的侧栏）和 `show_idle`（`false` 时空闲的行留在原位，但只画它们的图标）：

```lua
eitri.plugin.load("agents-strips", { narrow = true })
```

`narrow` 改变的是 Strips 画什么，而不是它那一列有多宽：这一列仍占窗口原来的份额（大约六分之一）。列宽是窗口的一个份额，从来不是像素，所以用你自己的一条[打开规则](configuration.zh-CN.md#where-a-new-module-opens)给窄条一个小份额；窗口约 1900 像素宽时，`0.035` 接近 62 像素：

```lua
eitri.layout.rule("lua:agents-strips", { at = "left", size = 0.035 })
```

规则在窗口打开这一列时决定它的位置；你保存过的布局保留原来的宽度，所以那一列要手动调窄一次。

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

- **回答仍然在聊天里。** 没有任何页面有回答卡片、提问或信任问题的键。当这一轮当前的那个 agent 不再需要你（你在它的聊天里回答了），这一轮会自己移到下一个，而下一个聊天的回答键在它到达后要等四分之一秒，所以你还在按的键，不会去回答一张你没读过的卡片。任何页面、`prefix u` 或 `prefix a` 把一个聊天带到前面时，同样要等这四分之一秒。
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
  if (w.length) eitriPlugin.call("agents.focus", { id: w[0].id, card_first: true });
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

点击就是 `agents.focus` 需要的那个手势。宿主在向这个文档发送任何事件之前，先回答 `agents.watch`，所以 `load` 不会覆盖更新的行。

带有 `agents` 能力的页面可以发出的调用：

| 调用 | 参数 | 作用 |
|---|---|---|
| `agents.watch` | 无 | 回答完整的视图；之后页面会收到下面的事件 |
| `agents.focus` | `{id, card_first?}` | 去到那一行的聊天；`card_first` 为 true 时，把它的卡片摆在前面 |
| `agents.new` | `{project}` | 在那个项目里新开一个 agent 标签页（视图 `projects` 里的项目键） |
| `agents.select` | `{key}` | 移动共用的选中项 |
| `agents.round` | `{op, key?}` | `op` 是 `start`、`next` 或 `leave` |
| `agents.stop` | `{id}` | 去到那一行的聊天，并在那里问 `Stop this turn? (y/n)` |

事件有 `agents.row`（一行变了）、`agents.gone`、`agents.reset`（页面没来得及听到变化之后给的完整视图）、`agents.selection` 和 `agents.round`。不在屏幕上的页面什么也听不到，回来时会收到一次 `agents.reset`。

声明了 `agents` 的页面，在它自己的脚本运行之前，还会得到 `window.eitriAgents`：一个按行键存放行、并应用这些事件的行仓库；每个状态一个图标和一个词（遇到更新的 Eitri 新增的状态，按它的类别回退）；以及通过按键工具包声明的[共用操作](#the-shared-grammar)的各个键，所以你的页面可以和自带的三个并排放。它还会得到 `window.eitriKeys`。插件格式的其余部分见[你自己做的页面](pages.zh-CN.md)。

<a id="the-rows"></a>
### 行

完整的视图和 `eitri agents --watch` 的每一行带有 `"v": 1`；单独的一行没有（下面的例子里没有 `v`），页面事件的内容也没有。读取方必须忽略它不认识的字段，并把不认识的状态按它的 `class` 来处理。一行如下：

```json
{
  "id": "a3f09c1d2e4b5f60.7",
  "key": "k-5c1e0b9a7d3f2e18",
  "agent": "claude",
  "project": { "key": "9b2c4e7a10f3d856", "name": "eitri" },
  "window": { "instance": "a3f09c1d2e4b5f60", "name": "1", "focused": true },
  "number": 3,
  "label": "close question",
  "label_from": "title",
  "shown": false,
  "state": "card",
  "class": "needs",
  "since": 1793865600123,
  "needs": { "cards": 1, "questions": 0, "trust": false },
  "waiting_since": 1793865600123,
  "unread": false,
  "tool": "Bash",
  "background": { "calls": 0, "shells": 0, "monitors": 0, "agents": 0, "other": 0 },
  "queued": 0,
  "mode": "manual",
  "ended_how": null,
  "moving": false
}
```

| 字段 | 含义 |
|---|---|
| `id` | `<instance>.<tab>`；这一行存在期间有效；是 `focus` 和 `stop` 的参数 |
| `key` | `k-` 加 16 位十六进制数，标签页自己的，标签页移到另一个窗口或被恢复时保持不变 |
| `agent` | `claude`、`codex` 或 `acp:<name>` |
| `project` | `key`（16 位十六进制，不含路径）和 `name`（文件夹名，截到 40 个字符） |
| `window` | `instance`（每次进程启动随机生成）、`name`（`1`、`2`，未配对的窗口是 `u`，合成器聊天窗口是 `c1`..`c9`，还有 `companion`）、`focused` |
| `number` | 那个窗口标签栏上的编号（不是全局的） |
| `label`、`label_from` | 最多 40 个字符；来自 `rename`、`title`、`session` 或 `new` |
| `shown` | 这个标签页是屏幕上某个聊天的当前标签页 |
| `state`、`class` | 见下 |
| `since` | 这一行进入当前状态的时间，毫秒，自 epoch 起，用写入进程的时钟 |
| `waiting_since` | 最早的卡片、提问或信任问题第一次被看到的时间，或 `null`；这一轮按它排序 |
| `needs` | 卡片数和提问数，以及有没有信任问题在等（一个标签页可以同时有卡片和提问） |
| `unread` | 一个回合在聊天不在屏幕上时结束了 |
| `tool` | 打开着的工具调用的名字（或最早那张卡片的），绝不含它的输入 |
| `background` | `calls` 是总数；按种类分的四个计数，在 `calls` 不为 0 时也可能全是 0 |
| `queued` | 打好了、等回合结束再发送的消息数 |
| `mode` | `manual`、`auto` 或 `bypass` |
| `ended_how` | `closed`、`stopped`、`unavailable`、`start_failed` 或 `tripped`；绝不含原因文字 |
| `moving` | 这个标签页正在移到另一个窗口的途中 |

状态，按决定一行是哪个状态的顺序排列（第一个成立的生效）：

| 状态 | 类别 | 图标 | 什么时候 |
|---|---|---|---|
| `card` | needs | `⚑` | 有权限卡片或工具卡片在等 |
| `question` | needs | `?` | 有一组提问在等 |
| `trust` | needs | `?` | 一次启动在等信任问题的回答 |
| `failed` | ended | `✕` | 标签页没能启动，或者它的会话未经要求就死了 |
| `ended` | ended | `✕` | 会话已关闭或被你停掉，而标签页仍然开着 |
| `running` | working | `●` | 一个回合在运行 |
| `background` | working | `◌` | 没有回合在运行，但有后台任务 |
| `starting` | working | `…` | 会话正在打开 |
| `done` | quiet | `•` | 空闲，并且有一个回合在聊天不在屏幕上时结束了 |
| `idle` | quiet | `·` | 一个活着的会话，没有东西在运行，也没有未读 |
| `new` | quiet | （空白） | 还没有会话的标签页 |

完整的视图，也就是 `eitri agents --json` 打印的、`agents.watch` 回答的：

```json
{ "v": 1, "at": 1793865600200, "enabled": true, "projects": [ ... ], "rows": [ ... ],
  "selection": { "key": "k-…", "by": "a3f09c1d2e4b5f60", "at": 1793865600100 } | null,
  "round": { "from": { "instance": "a3f09c1d2e4b5f60", "module": "…", "tab": null }, "current": "k-…",
             "visited": [ ... ], "started_at": 1793865500000, "at": 1793865600100 } | null }
```

`projects` 列出每个有窗口在运行的项目，`{key, name}`，包括窗口里没有任何标签页的项目，这样页面可以在那里提供 `new`。行按项目名和键、窗口名和实例、编号排序。`--watch` 的各行把每个变化包在一个带 `v` 和 `type` 的外壳里：

```json
{ "v": 1, "type": "row",  "row": { ... } }
{ "v": 1, "type": "gone", "id": "a3f09c1d2e4b5f60.7", "key": "k-5c1e0b9a7d3f2e18", "why": "closed" }
{ "v": 1, "type": "reset", "view": { ... } }
{ "v": 1, "type": "selection", "selection": { ... } | null }
{ "v": 1, "type": "round", "round": { ... } | null }
```

页面收不到这些外壳，它的回调只拿到里面的内容：`agents.row` 就是那一行本身（用 `event.id`，不是 `event.row.id`），`agents.gone` 是 `{id, key, why}`，`agents.reset` 是完整的视图（只有这个带 `v`），`agents.selection` 是选中项或 `null`，`agents.round` 是这一轮或 `null`。

`why` 是 `closed`（标签页关了）、`moved` 或 `window`（它的进程结束了，或者关掉了这一层）。一个进程最多发布 64 行、64 KiB；超过的话，按标签页编号发布前 64 行。

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
