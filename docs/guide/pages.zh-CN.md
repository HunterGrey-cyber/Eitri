[English](pages.md) | 简体中文
<!-- translated-from: pages.md sha256=c09854aca253fe496e84e71ab6dab52cf184c263eb2d8b0cc9c94ed93fba582a -->

# 你自己做的页面

Eitri 可以把你自己的网页作为窗口里的模块显示出来，和编辑器、聊天、终端放在一起。在 Hyprland、sway 和 niri 上，每个页面默认是一个独立的窗口，由你的窗口管理器排布（[合成器窗口](compositor-windows.zh-CN.md)）。有两种：

- **Lua 面板**是 `init.lua` 里的一条调用，显示一个 HTML 文件或一个地址，比如你的开发服务器。它是一个普通的网页视图，对 Eitri 一无所知。
- **插件**是一个带 `eitri-plugin.toml` 的目录，加上一个小网站。它的页面拿到 Eitri 的配色、Eitri 的页面按键，以及一座通往一组封闭的宿主能力的桥，具体哪些由清单点名：保存它自己的数据、把按键交给编辑器、运行插件自己的程序、跟随你正在编辑的缓冲区。Eitri 随附的 [Markdown 预览](markdown-preview.zh-CN.md)就是一个插件。

**两种都是实验性的。** 0.x 期间清单格式、桥接和按键工具包都可能改变，改变时发布说明会写出来。两种都不能在 companion 窗口里使用。

<a id="trust"></a>
## 信任

目前还没有同意界面。**加载插件就是授权**：你自己的 `init.lua` 里的一条 `eitri.plugin.load`，会把清单里列出的每一项能力都交给这个插件；带任务的插件会以你的身份、用你的文件和你的网络运行它的程序。对待插件就像对待你运行的任何程序：只加载你自己写的或读过的。Eitri 会把每个插件的能力清单记在 `~/.local/state/eitri/plugins/<id>/` 下，下次启动时如果变了就记一行日志；它什么也不会问。

Lua 面板会运行你指给它的任何页面，拥有任何网页都有的网络访问。只把它指向你信任的页面。

<a id="a-lua-panel"></a>
## Lua 面板

把一个 HTML 文件放进 `<config>/panels/`，然后在 `init.lua` 里注册它。`<config>` 就是读取 `init.lua` 的目录：`$EITRI_CONFIG_DIR`；没有设置时，如果 `$XDG_CONFIG_HOME` 是绝对路径，就是 `$XDG_CONFIG_HOME/eitri`；否则是 `~/.config/eitri`。

```lua
eitri.panel.register{
  id = "status",                                  -- the module is lua:status
  title = "Status",                               -- the name on its chip and in the tray
  position = "side",                              -- "side", "bottom" or "main", where it goes the first time
  key = "u",                                      -- optional: <prefix> u shows it and gives it the keys
  content = { type = "webview", url = "status.html" },
}
```

```html
<!doctype html>
<meta charset="utf-8">
<title>Status</title>
<h1>Today</h1>
<ul>
  <li>Review the parser change</li>
  <li>Write the release notes</li>
</ul>
```

- `url` 是 `panels/` 下的一个文件名；以 `/` 开头或带有协议（`file://`、`https://`、开发服务器的 `http://localhost:5173`、`data:`）时，原样使用。
- `position` 只是第一次启动时的提示：之后窗口会记住你把面板放在哪里，而针对 `lua:status` 的[打开规则](configuration.zh-CN.md#where-a-new-module-opens)比它优先。
- `key` 必须是一个字符，并且前缀后面没有别的东西在用：不能是默认键，不能是 stock tmux 在它的前缀后面绑定的键，不能是 `v`，也不能是 `E`、`A`、`T` 或 `V`。默认情况下 `b`、`g`、`h`、`j`、`k`、`u` 和 `y` 是空着的。冲突会让这个页面没有键，Eitri 在启动时把它列出来，并点出这个键和占着它的东西。
- id 不能是 `terminal#2` 到 `terminal#9` 或 `agent#2` 到 `agent#9`；再次注册同一个 id 会替换之前的面板。
- 这个页面是一个普通的网页视图：它的脚本会运行，它的链接在它自己里面打开，它可以访问网络。它没有通往 Eitri 的桥，拿不到配色，也没有按键工具包，所以需要这些的页面应该写成插件。Eitri 自己的按键（前缀、`Ctrl+h/j/k/l`）在那里照常有效，所以 `<prefix> e` 会带你回到编辑器。

<a id="a-plugin"></a>
## 插件

插件是一个目录。把它放进 `~/.local/share/eitri/plugins/<id>/` 并按名字加载，或者放在任何地方并按路径加载（[名字是怎样找到的](configuration.zh-CN.md#plugins)）：

```lua
eitri.plugin.load("scratch", { key = "y" })
```

`eitri.plugin.load` 只在 `init.lua` 运行期间被接受。它的选项是插件自己的设置、`key`（一个模块键，和 Lua 面板一样）以及 `position`。每次加载都会在你启动 Eitri 的那个终端里打印它用的是哪个目录。清单里有错误，这个页面就不加载，Eitri 在启动时把它列出来并点出出错的那个键。

<a id="a-complete-example"></a>
### 一个完整的例子

一条在两次启动之间保留下来的笔记。四个文件：

```text
~/.local/share/eitri/plugins/scratch/
├── eitri-plugin.toml
└── site/
    ├── index.html
    ├── page.js
    └── style.css
```

`eitri-plugin.toml`：

```toml
id = "scratch"
title = "Scratch"
protocol = 1
site = "site"
entry = "index.html"
capabilities = ["state", "layout.focus"]
```

`site/index.html`：

```html
<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>Scratch</title>
<link rel="stylesheet" href="style.css">
<script src="/.eitri/keys.js"></script>
<script src="page.js" defer></script>
</head>
<body>
<textarea id="note" aria-label="Note"></textarea>
</body>
</html>
```

`site/style.css`：

```css
body { margin: 0; background: var(--nv-bg); color: var(--nv-fg); font-family: var(--nv-font-prose); }
textarea {
  box-sizing: border-box; width: 100%; height: 100vh; padding: 12px; border: 0;
  background: var(--nv-surface); color: var(--nv-surface-fg); font-family: var(--nv-font-mono);
}
textarea:focus { outline: 2px solid var(--nv-focus-ring); }
```

`site/page.js`：

```js
const host = window.eitriPlugin;
const note = document.getElementById("note");

async function start() {
  const hello = await host.call("host.hello");
  const saved = await host.call("state.read", { key: "note" });
  note.value = saved.value ?? "";

  let timer = 0;
  note.addEventListener("input", () => {
    clearTimeout(timer);
    timer = setTimeout(() => host.call("state.write", { key: "note", value: note.value }), 300);
  });

  await window.eitriKeys.install({
    rows: [
      { action: "note.edit", keys: ["i"], context: "note", desc: "type in the note" },
      { action: "note.back", keys: ["q"], context: "note", desc: "back to the editor" },
    ],
    run(action) {
      if (action === "note.edit") { note.focus(); return true; }
      if (action === "note.back") { host.call("layout.focus", { target: "editor" }); return true; }
      return false;
    },
    initial: { timeoutlen: hello.timeoutlen, hasKeys: true, prefix: hello.prefix, settings: hello.keys_settings },
  });
}

start();
```

重启 Eitri。`<prefix> y` 会显示这个页面并把按键交给它（在还没有保存过布局的项目里，它已经开在右边了）。`i` 在笔记里打字，`Esc` 停止打字，`q` 把按键交还给编辑器，`?` 列出页面的按键，`j`、`k`、`/` 和 `f` 和在每个插件页面里一样。笔记保存在 `~/.local/state/eitri/plugins/scratch/state/`。

<a id="the-manifest"></a>
### 清单

| 键 | 内容 |
|---|---|
| `id` | 小写字母、数字和 `-`，最多 40 个字符，两端不能是 `-`；也就是加载它时用的名字 |
| `title` | 模块芯片上的名字 |
| `protocol` | `1` |
| `site`、`entry` | 页面文件所在的目录（相对于插件），以及其中的页面 |
| `capabilities` | 页面可以使用的宿主能力，从下表里选；用到别的能力的调用会以 `not_declared` 被拒绝 |
| `[settings.<name>]` | `eitri.plugin.load` 的一个选项：`kind = "bool"`、`"choice"`（带 `values`，一到八个词）、`"int"`（一个整数，带 `min` 和 `max`；`default` 可以省略，省略时这个选项在 `init.lua` 给它值之前是未设置的）或 `"dir"`（一个路径，不存在时会创建），以及一个 `default` |
| `[module]` | `start`（第一次启动时是 `"shown"` 还是 `"hidden"`）、`at`（一个[打开规则](configuration.zh-CN.md#where-a-new-module-opens)的位置）、`size`、`focus` |
| `[roots]` | `editor.open`、`editor.assist` 和任务可以触及的目录，写成 `${plugin}` 或一个 `dir` 设置 |
| `[tasks.<name>]` | `argv`（第一个词是 `${plugin}/<路径>`，也就是插件里面的一个程序）、`cwd`（在某个根目录里面）和 `timeout_secs`（`0` 表示不限） |
| `[vars.<name>]` | 页面传给任务的一个值：`chars`（`"lower-digit-dash"`、`"lower-digit"` 或 `"digit"`）和 `max_len`（1 到 256） |
| `[net]` | `origins`（页面可以从中获取内容的 `https://host[:port]`）和 `images`（`"https"` 或 `"off"`，或者一个取这两个值的 choice 设置） |
| `[follow]` | 页面在编辑器里跟随的 `filetypes`，以及 `auto_show`（一个 bool 设置） |
| `[commands.<Name>]` | 一个显示、切换或隐藏页面的 Neovim 命令（`does = "show"`、`"toggle"` 或 `"hide"`） |
| `[page]` | 页面要把自己的状态交给另一个窗口里替换它的页面时，写 `handoff = true` |

每张表都会拒绝它不认识的键，并点出那个键。任务不经过 shell 运行：每个参数是一个词，里面可以有 `${plugin}`、一个设置或一个声明过的变量；程序的输出一行一行地回到页面。

能力：

| 能力 | 页面因此可以做什么 |
|---|---|
| `state` | 按键保存 JSON 值（`state.read`、`state.write`），并追加日志（`log.append`、`log.read`），都放在 `~/.local/state/eitri/plugins/<id>/` 下 |
| `layout.focus` | 把按键交给编辑器或拿回来（`layout.focus`，`target` 为 `"editor"` 或 `"self"`） |
| `editor.open` | 在编辑器里打开某个根目录里面的文件，可以指定行和列 |
| `editor.assist` | 为某个根目录下的文件打开或关闭编辑器的诊断和内嵌提示 |
| `tasks` | 启动和取消插件自己的 `[tasks]`（`task.start`、`task.cancel`；输出以 `task.output` 和 `task.exit` 事件到达） |
| `net` | 从 `[net]` 的 origin 获取内容；除此之外什么都出不了页面 |
| `editor.follow` | 跟随你正在编辑的缓冲区（`editor.view`、`editor.text` 事件），滚动它、移动它的光标 |
| `editor.commands` | 在 Neovim 里定义 `[commands]` |

<a id="what-the-page-runs-in"></a>
### 页面运行在什么里面

- 页面从 `eitri-plugin://<id>/` 提供，只提供 `site` 下的文件（HTML、CSS、JavaScript、JSON、图片和字体），内容安全策略很严：脚本只能来自站点，没有内联脚本，没有 `eval`；样式表来自站点，另外允许内联样式（`<style>` 元素和 `style` 属性）；图片来自站点或 `data:`，`[net]` 的 `images` 为（或者它的设置取值为）`"https"` 时也可以来自任何 `https` 地址；网络请求只能发往 `[net]` 的 origin。站点之外的 `https`、`http` 或 `mailto` 链接，在你按下它时会在 Eitri 之外打开；脚本没法打开它。
- **桥**是 `window.eitriPlugin`：`call(verb, args)` 返回一个结果的 promise，或者以一个错误拒绝，错误的 `code` 说明原因（`not_declared`、`bad_request`、`outside_roots`、`too_large` 等等）；`on(event, fn)` 用来监听。先调用 `host.hello`：它的结果里有页面的设置、主题、窗口的前缀和 `timeoutlen`，以及它还在运行的任务。一条消息最大 256 KiB，一个保存的值最大 192 KiB，一个插件最多保存 64 个键。
- **配色和字体**以 `--nv-*` CSS 变量的形式出现在页面的根元素上（`--nv-bg`、`--nv-fg`、`--nv-surface`、`--nv-border`、`--nv-link`、`--nv-font-mono`、`--nv-font-prose`、`--nv-syn-*` 语法配色等等），并随你的配色实时变化。`prefers-color-scheme` 跟随 Eitri 的浅色或深色。除了开在你自己的 Neovim 旁边的窗口，Eitri 的每个窗口还会发送 `--nv-text-size`，即编辑器的文字大小，以页面的像素为单位（例如 `18.67px`），随 `guifont` 变化；想让文字和编辑器一样大的页面读 `var(--nv-text-size, 14px)`。开在你自己的 Neovim 旁边的窗口不发送它。
- **按键工具包**在 `/.eitri/keys.js`，给页面所有插件页面共用的按键：`j`/`k` 和 `d`/`u` 滚动，`gg`/`G`，`/` 查找，`f` 给能按的东西标上标签，`?` 列出按键，`H`/`L` 后退和前进。`window.eitriKeys.install({rows, run, initial})` 加上你自己的行；`run(action)` 处理了这个键就返回 `true`。文本框拿着按键时，除了 `Esc`，每个键都交给它。你可以在 `init.lua` 里用 `eitri.keymap.set("plugin:<id>", "<key>", "<action>")` 改一个页面的按键。
- **Eitri 的按键优先**：前缀和它后面的键、`Ctrl+h/j/k/l`、`Ctrl+=`、`Ctrl+-`、`Ctrl+0` 和 `F11` 永远不会到达页面，一行按键也不能用它们、`Ctrl+Shift` 组合键或 `Super` 组合键。不经过工具包、自己处理按键并把按键交给编辑器的页面，应该调用 `preventDefault()`，否则这个键也会传到编辑器。
- 文字大小跟随窗口（`Ctrl+=`），`<prefix> Ctrl+=` 只调整这个页面。

<a id="a-brief-for-your-own-agent"></a>
## 给你自己的 agent 的说明

想让一个 agent 替你写插件，把这一页和类似下面的说明交给它：

```text
Write an Eitri plugin: a directory with eitri-plugin.toml (id, title, protocol = 1, site, entry,
capabilities) and a static site under `site`. Follow docs/guide/pages.md in the Eitri repository.

- The page runs in WebKitGTK at eitri-plugin://<id>/ under a strict CSP: no inline script, no eval,
  scripts and styles only from the site, network only to origins listed under [net].
- It reaches Eitri only through window.eitriPlugin.call(verb, args) and .on(event, fn); call
  host.hello first. Declare only the capabilities the page uses.
- Load /.eitri/keys.js and call window.eitriKeys.install({rows, run, initial}) after host.hello, so
  every action has a key. Never bind the prefix, Ctrl+h/j/k/l, Ctrl+= / Ctrl+- / Ctrl+0, F11, or a
  Ctrl+Shift or Super chord.
- Use the --nv-* CSS variables for every colour and font.
- Tasks run only programs inside the plugin directory, without a shell.

What the page should do: <describe it here>.
```

然后在加载之前读一遍它写的东西：加载它就是信任它。
