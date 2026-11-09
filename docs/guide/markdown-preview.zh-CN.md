[English](markdown-preview.md) | 简体中文
<!-- translated-from: markdown-preview.md sha256=066b28284497dc739ce1d3165a8a8744c212c371f102357adce922f70587ce11 -->

# Markdown 预览

Eitri 可以把你正在编辑的 Markdown 文件渲染出来，显示在编辑器旁边：GitHub 风格的表格、任务列表、脚注、数学公式、图表和提示块，配色跟随你的配色方案。它跟着你的输入更新，和 Neovim 双向同步滚动，还可以用它自己的 Vim 风格按键来阅读。它是 Eitri 自带的一个插件，在你打开之前一直是关闭的。

预览只负责显示文件。编辑只在 Neovim 里进行：在预览里按 `Enter`，会把 Neovim 的光标移到预览光标所在的位置，并把按键交给 Neovim。

<a id="turn-it-on"></a>
## 打开这个插件

在 `$XDG_CONFIG_HOME/eitri/init.lua`（默认 `~/.config/eitri/init.lua`）里加一行，然后开一个新窗口：

```lua
eitri.plugin.load("markdown")
```

在你打开预览之前，屏幕上什么都不会变。没有这一行，窗口里就没有预览的任何东西：没有模块，没有 Neovim 命令，也没有任何东西在运行。想先试一下的话，在空标签页的 Pages 下面的 `Markdown preview` 上按 `Enter`，或者运行 `:Eitri plugin load markdown`，就会把它加载进当前窗口，只在这次运行里有效（[插件](configuration.zh-CN.md#plugins)）。

<a id="open-and-close-it"></a>
## 打开和关闭预览

- `:EitriMarkdown` 在编辑器右边显示预览；`:EitriMarkdownToggle` 显示或隐藏它；`:EitriMarkdownHide` 隐藏它。从 Neovim 打开时，预览把按键留在 Neovim 里，你可以继续输入。使用[合成器窗口](compositor-windows.zh-CN.md)时，预览是一个独立的窗口：这些命令在你的窗口管理器放置它的地方打开它，也只有窗口管理器能关闭它。
- 你自己的按键：`eitri.plugin.load("markdown", { key = "b" })` 让前缀再按 `b`（默认前缀下是 `Ctrl+b b`）显示预览并把按键交给它，在它拿着按键时再按一次就隐藏它。没有默认按键。
- 命令名都以 `Eitri` 开头，这样不会和其他预览插件的命令撞名。如果你的 Neovim 配置已经定义了同名的命令（比如 `:EitriMarkdown`），Eitri 不动那个命令，只定义其余的，并在你启动 Eitri 的终端里打印 `[plugin markdown] :EitriMarkdown is already defined; not replacing it`。要用 Eitri 的命令，就删掉或改名你自己的那个。

- 在 Neovim 里用 VS Code 风格的按键。Eitri 不绑定它；想要的话，加到你的 Neovim 配置里：

  ```lua
  vim.keymap.set("n", "<C-S-v>", "<Cmd>EitriMarkdownToggle<CR>")
  ```

  在 fcitx5 这类输入法占用 `Ctrl+Shift` 组合键的桌面上，这个组合键到不了 Neovim；换一个键，比如 `<leader>mp`。

<a id="preview-automatically"></a>
## 自动预览

```lua
eitri.plugin.load("markdown", { auto = true })
```

打开 `auto` 后，每当一个 Markdown 文件成为当前缓冲区，预览就会出现，但不拿走按键。它从不自己隐藏。你把它隐藏后，要等到另一个 Markdown 文件成为当前缓冲区，它才会再出现。

<a id="what-it-shows"></a>
## 它显示什么

- 当前的 Markdown 缓冲区，也就是 Neovim 里的内容，而不是磁盘上的文件：改动大约五分之一秒内就会出现，上方或下方的文字变化时，你正在读的部分保持不动。
- 当你切到不是 Markdown 的缓冲区（代码、帮助、终端）时，预览保留它上次显示的 Markdown 文件，底部的状态行显示 `[not current]`。
- CommonMark 和 GitHub 风格的 Markdown：表格、任务列表（只显示，不能点）、删除线、自动链接、脚注，以及按 GitHub 规则命名的标题锚点。
- 代码块按你的配色方案高亮。
- 用 KaTeX 渲染 GitHub 写法的数学公式：行内的 `$...$` 和 `` $`...`$ ``，块级的 `$$...$$` 和 ```` ```math ````。有错误的公式会显示它的源码并加下划线，文档的其余部分照常渲染。
- 来自 ```` ```mermaid ```` 代码块的 Mermaid 图表，使用你的配色方案的颜色。只有带图表的文档才会加载图表支持。
- GitHub 提示块（`> [!NOTE]`、`[!TIP]`、`[!IMPORTANT]`、`[!WARNING]`、`[!CAUTION]`）和 Obsidian 的 callout：`> [!info]`、标记后面的标题（`> [!note] Read this first`），以及 `[!note]-`（折叠）和 `[!note]+`（展开）。
- Front matter（文件开头两行 `---` 之间的 YAML）显示为一个折叠的 “Front matter” 部分。
- 文档里的 HTML 按 GitHub 的方式显示，但去掉脚本、样式、框架和表单。

如果你希望 Markdown 在 Neovim 里面就显示成渲染后的样子，render-markdown.nvim 和 markview.nvim 这类插件可以在你自己的 Neovim 配置里做到；Eitri 不替你配置它们。

<a id="scrolling-together"></a>
## 同步滚动

预览让你所在的那一行和 Neovim 里保持在同一高度：Neovim 光标所在的行在预览里往下的比例，和它在 Neovim 屏幕上往下的比例相同（markdown-preview.nvim 把这种方式叫 `relative` 滚动），经过折叠、很长的自动换行、很长的代码块和稍后才加载的图片时也是如此。在 Eitri 的窗口里左右并排时，两边在屏幕上就是同一高度；在终端里的 Neovim 旁边，预览窗口和终端顶边对齐、一样高时也是。哪一边拿着按键，哪一边带头：

- 按键在 Neovim 里（或者在预览以外的任何地方）时，在 Neovim 里移动和滚动会带动预览，没有让 Neovim 滚动的光标移动也会，预览的边栏里有一条竖线标出 Neovim 光标所在的行。到了文档的开头和结尾，预览停在它的两端。
- 按键在预览里时，预览有它自己的光标，画法和 Neovim 的 `cursorline` 一样：边栏里一条竖线，加上一条用你配色方案里光标行颜色的横带。Neovim 跟着它：Neovim 的光标停在同一行、同一高度，只有预览滚动时 Neovim 的窗口才滚动，所以离开预览时，Neovim 停在同一个位置，光标就在那一行上。如果 Neovim 的 `scrolloff` 不允许光标停在那一行而不滚动，光标会停在最近的、不用滚动的那一行。离开时还在进行的滚动会立即结束，并把结束的位置告诉 Neovim。如果离开前没有移动过，就什么也不告诉 Neovim，它的光标留在原处。
- 光标在视图的顶部和底部各留一段边距，和 Neovim 的 `scrolloff` 一样：至少是预览里五行文字的高度（Vim 自己的默认设置留五行；Neovim 默认的 `scrolloff` 是 0），你的 Neovim 的 `scrolloff` 留的行更多时就留得更多，最多是视图的四分之一。一次移动如果会越过边距，预览就只滚动刚好够的距离。到了文档的开头和结尾，边距让开，光标能到达第一行和最后一行。
- 在文档开头附近，预览的光标可能比 Neovim 的低：渲染后的 Markdown 比源码高，两边都不能滚到第一行之上。从第一次滚动开始，两边就在同一高度。
- 回到 Neovim 后，在你在 Neovim 里移动之前，什么都不会动。
- 进入预览时，光标从 Neovim 光标所在的那一行开始，就在预览当前显示它的位置，所以什么都不会动。光标在任何块之外的一行上（比如段落之间的空行）时，预览的光标从离它最近的块开始，`Enter` 仍然让 Neovim 的光标留在它自己那一行。
- 用鼠标滚轮滚动预览时，页面移动，光标留在它那一行上，除非边距把它推开，和 Neovim 一样。按键在 Neovim 里时，滚轮也会带动 Neovim 的窗口；这时 Neovim 的光标只按它的 `scrolloff` 需要的那样移动，Neovim 自己的下一次移动会把带头的位置拿回来。

<a id="reading-with-the-keyboard"></a>
## 用键盘阅读

用你的按键、在编辑器里按 `Ctrl+l`，或者点击预览，把按键交给预览。然后：

| 按键 | 作用 |
|---|---|
| `j` / `k` | 光标向下 / 向上移动一个显示行（一个长段落占好几行，和 Vim 的 `gj` 一样） |
| `Ctrl+e` / `Ctrl+y` | 向下 / 向上滚动一步；光标不动，除非边距把它推开 |
| `d` 或 `Ctrl+d` / `u` 或 `Ctrl+u` | 向下 / 向上半页，光标跟着走 |
| `Ctrl+f` / `Ctrl+b` | 向下 / 向上一页 |
| `gg` / `G` | 光标到第一行 / 最后一行（文档最后一行在视图底部，和 Vim 一样） |
| `}` / `{` | 光标到下一块（一段、一个列表项、一行表格、整个代码块）/ 到所在块的开头，已经在开头时到上一块的开头 |
| `]]` / `[[` | 光标到下一个 / 上一个标题 |
| `zh` / `zl` | 横向滚动当前块（很宽的表格、代码块、公式或图表） |
| `/` | 在渲染后的文字里查找；`Enter` 执行搜索（空的搜索重复上一次），`Esc` 取消 |
| `n` / `N` | 下一个 / 上一个匹配，到头后从另一头接着找 |
| `f` | 在链接、脚注链接和折叠开关上显示标签；输入标签就跟随它 |
| `yy` | 复制当前块的 Markdown 源码（代码块只复制代码，不含围栏那两行） |
| `Enter` | 在这里编辑：把 Neovim 的光标移到这个位置，并把按键交给 Neovim |
| `o` | 在查看器里打开光标处的图表或图片（见[近看一张图表](#近看一张图表)） |
| `v` / `V` | 从光标处开始选中文字，按字符 / 按整行（见[选中文字](#选中文字)） |
| `H` 或 `Ctrl+o` / `L` 或 `Ctrl+i` | 在文档内部的跳转之间后退 / 前进：`gg`、`G`、`}`、`{`、`]]`、`[[`、搜索和 `n`/`N`、标题链接、脚注 |
| `?` | 列出这些按键 |
| `Esc` | 退出当前模式；同时清除搜索高亮 |

- 当前块是光标所在的那一块；边栏里另有一条竖线标出它。点击会把光标放到你点的那一行。双击一个块，等于在那里按 `Enter`。
- 如果 Neovim 的光标已经在预览光标所在的那一行上（移动时放到那里的，或者进入预览时带进来的那一行，这时连所在的列也不变），`Enter` 让它留在那一行，所以 Neovim 的窗口不会滚动。
- `}` 和 `{` 一块一块走过的，就是边栏竖线标出的那些块。
- 跳转类的移动（`gg`、`G`、`}`、`{`、`]]`、`[[`、搜索和 `n`/`N`、文档内的链接、`H` 和 `L`）把光标移到目标上，只有目标在边距之外时才滚动：离得近就只滚动需要的那一点，离得远就把它放到中间，和 Neovim 一样。滚动是平滑的；关掉平滑滚动，或者桌面要求减少动画时，就立即跳过去。不支持计数（`5j`），图表查看器里除外。
- 默认情况下，搜索不区分大小写，除非搜索里有大写字母，和 Vim 的 `smartcase` 一样（`plugins.keys.find_case` 可以改）。状态行显示 `[3/12]`，到头接着找时显示 `search hit BOTTOM, continuing at TOP`。
- 按下一个较长按键的第一个键（`g`、`z`、`y`、`[`、`]`）之后，状态行会列出接下来可以按的键。
- 如果你的 Eitri 前缀是这些 Ctrl 键中的一个，它仍然是前缀，预览里就没有这个键：用默认前缀 `Ctrl+b` 时，没有 `Ctrl+b` 向上翻页。
- 按 `yy` 之后，状态行显示 `yanked 3 lines`。
- 指向项目里另一个文件的链接会在 Neovim 里打开它（按键留在预览里，如果那个文件是 Markdown，预览接着显示它）；`src/main.rs#L10` 会打开到第 10 行。网页链接在你的浏览器里打开。指向项目以外的链接，或者指向不存在的文件的链接，会显示 `outside the project` 或 `no such file`，此外什么也不做。

### 选中文字

`v` 从光标处开始选中，和 Vim 一样；`V` 按整行选中。状态行显示 `VISUAL` 或 `VISUAL LINE`。然后：

| 按键 | 作用 |
|---|---|
| `h` / `l`、`w` / `b` / `e`、`0` / `$` | 把选区光标那一端移动一个字符、一个词，或者移到行首、行尾 |
| `j` / `k` | 向下 / 向上一个显示行 |
| `}` / `{` | 到所在块的末尾 / 开头，再按就到下一块的末尾 / 上一块的开头：选区一次多一整块 |
| `gg` / `G` | 到第一段 / 最后一段文字 |
| `n` / `N` | 到上一次搜索的下一个 / 上一个匹配 |
| `o` | 到选区的另一端 |
| `y` | 按你看到的样子复制选中的文字，然后结束选中 |
| `Y` | 复制选区覆盖的那些行的 Markdown 源码 |
| `Enter` | 在 Neovim 里编辑选区：按键交给 Neovim，同样的文字在它自己的 visual 模式里选中 |
| `v` / `V` | 在按字符和按整行之间切换；再按一次同一个键就结束选中 |
| `Esc` | 结束选中 |

- 预览的光标跟着你移动的那一端走，所以选区始终在边距以内，Neovim 也像按 `j` 时那样跟着走。
- 预览从不修改你的文字。要修改选中的内容，按 `Enter`，在 Neovim 里改（`c`、`d`、`gq` 等等）。
- 选区在一行源码之内，而且它的文字在那一行里只出现一次时，`Enter` 在 Neovim 里选中同样的字符；否则 Neovim 选中它覆盖的整行源码：渲染后的 Markdown 对不回源码的列（`**bold**` 显示为 **bold**，链接只显示文字）。如果 Neovim 正停在提示或命令行上，就什么也不选中，状态行会说明。
- 点击鼠标会结束选中，把按键交给别的窗格也会。选中时用的按键不能在 `init.lua` 里修改；`v` 和 `V` 本身可以（`visual.char`、`visual.line`）。

和任何插件一样，在 `init.lua` 里用 `plugin:markdown` 这张表修改这些按键：

```lua
eitri.keymap.set("plugin:markdown", "e", "edit.here")
```

焦点在预览上时，`<prefix> ?` 会在 Eitri 自己的按键列表里先列出它的按键。动作名是 `cursor.down`、`cursor.up`、`cursor.half_down`、`cursor.half_up`、`cursor.page_down`、`cursor.page_up`、`cursor.first`、`cursor.last`、`block.next`、`block.prev`、`scroll.down`、`scroll.up`、`heading.next`、`heading.prev`、`scroll.left`、`scroll.right`、`find.start`、`find.next`、`find.prev`、`hint.start`、`block.yank`、`edit.here`、`history.back`、`history.forward`、`sheet.open`、`mode.escape`、`visual.char`、`visual.line`，以及查看器的 `viewer.open`、`viewer.close`、`viewer.left`、`viewer.right`、`viewer.left_edge`、`viewer.right_edge`、`viewer.zoom_in`、`viewer.zoom_out`、`viewer.actual`、`viewer.fit`、`viewer.fit_width` 和 `count`（数字 `1` 到 `9`）。如果你在候选版本里改过这些按键，把对 `scroll.half_down`、`scroll.half_up`、`scroll.page_down`、`scroll.page_up`、`scroll.top` 或 `scroll.bottom` 的修改改成对应的 `cursor.` 动作（`cursor.half_down`……`cursor.first`、`cursor.last`）：预览里已经没有这几个 `scroll.` 动作了。

一步滚动多远、滚动是否带动画、`g` 这样的键等多久、什么时候列出接下来的键、提示标签用哪些字母、搜索怎样处理大小写，这些是所有插件页面共用的设置：见[插件页面里的按键](configuration.zh-CN.md#keys-in-a-plugin-page)。

<a id="images"></a>
## 近看一张图表

光标在一张图表（或一张图片）上时，按 `o` 会在一个盖住整个预览的查看器里打开它，先让整张图表正好放得下（图片比查看器小时，按它自己的大小打开）。图表无论调到多大都是清晰的。在查看器里：

| 按键 | 作用 |
|---|---|
| `h` / `j` / `k` / `l` | 向左 / 下 / 上 / 右移动视图的五分之一；加计数移得更远（`5l`） |
| `d` 或 `Ctrl+d` / `u` 或 `Ctrl+u` | 向下 / 上移动半个视图 |
| `Ctrl+f` / `Ctrl+b` | 向下 / 上移动一整个视图 |
| `0` / `$` | 左 / 右边缘 |
| `gg` / `G` | 上 / 下边缘 |
| `+` / `-` | 以视图中间为中心变大 / 变小；加计数多走几步（`3+`） |
| `=` | 原始大小（100%）；加计数就是那个百分比（`200=`） |
| `a` | 让整张图表正好放得下 |
| `s` | 让它的宽度正好放得下 |
| `Enter` | 关掉查看器，在 Neovim 里编辑这张图表，和在页面上按 `Enter` 一样 |
| `q` 或 `Esc` | 关掉查看器，光标回到这张图表上（如果你开始输入计数，`Esc` 先清掉计数） |

- 这些是 zathura 和 nsxiv 的按键。查看器的角上列出主要的几个按键、当前大小的百分比和你正在输入的计数。
- 用鼠标：滚轮移动，按住 `Ctrl` 滚动滚轮（或在触摸板上捏合）以指针为中心改变大小，拖动来移动，双击在打开时的样子和它的两倍之间切换。
- 查看器开着时，页面的其他按键、搜索、`H`/`L` 和选中文字都不起作用；`?` 仍然列出按键。
- 查看器开着时在 Neovim 里编辑这张图表，它会在查看器里重新画出来，位置和大小不变（原来是“正好放得下”的，新图也正好放得下）。新版本画不出来（有错误）时，查看器保留上一次的图，状态行显示 `the diagram does not draw now; showing its last drawing`。如果你删掉了这张图表，或者预览换到了别的文件，查看器会关掉，并显示 `the diagram is no longer in the document`。
- 在别的东西上按 `o` 会显示 `no diagram or image at the cursor`；在还没画好的图表上显示 `the diagram is not drawn yet`。
- 图片比查看器大时，打开时正好放得下；比查看器小时按 100% 打开，和 nsxiv 一样；按 `a` 仍然可以让它正好放得下。图片按它自己的分辨率显示：超过 100% 时，看到的是变大的像素。
- 查看器盖住的是预览，不是整个窗口。想让预览先占满窗口，用 `<prefix> z` 放大它所在的模块（见[按键](keys.zh-CN.md)）。

## 图片

- 项目里的图片会显示：相对路径从文档所在的文件夹算起，以 `/` 开头的路径从项目的顶层文件夹算起。文档在 Eitri 启动时所在的文件夹里面时，项目就是那个文件夹；否则是文档上方最近的一个含有 `.git` 的文件夹；再否则是文档自己的文件夹；你的主目录和 `/` 永远不算项目。项目以外的图片显示为一个写着 `outside the project` 的方框，未保存的缓冲区里的相对路径图片显示为 `save the file to show local images`。
- 远程图片按 VS Code 的默认方式处理：`https` 图片会加载，`http` 图片不会（方框里写着 `http image not loaded`）。状态行会统计没有加载的图片数量。
- **隐私：** 加载一张远程图片，会让托管它的服务器知道你的地址和你打开文档的时间。要一张远程图片都不加载：

  ```lua
  eitri.plugin.load("markdown", { remote_images = "off" })
  ```

  这时远程图片显示为写着 `remote images are off` 的方框，预览不发出任何网络请求。预览里别的东西都不使用网络：不下载字体、脚本或图表，链接也从不预取。
- 图片文件改动后，要在你切换文档或重新打开预览时才会显示；预览不监视文件。

<a id="settings"></a>
## 文字大小

预览的文字和编辑器的一样大：就是 Neovim 的 `guifont` 的大小，在屏幕上画出来也一样大。它在你工作时跟着编辑器变：改 `guifont`，或者用 `Ctrl+=` 调整窗口的大小，预览就跟着变。标题、代码和表格与正文保持原来的比例。

要单独调整预览的大小，给它 `font_size`，一个 9 到 32 的整数（单位是页面的像素，和 [`agent.font_size`](configuration.zh-CN.md) 一样）：

```lua
eitri.plugin.load("markdown", { font_size = 16 })
```

在[你自己的 Neovim 旁边](#beside-your-own-neovim)的窗口里，Eitri 不知道你终端的字体，所以预览用它自己的大小，14 像素，除非你给了 `font_size`。

## 设置

| 选项 | 取值 | 默认 | 作用 |
|---|---|---|---|
| `key` | 一个键 | 无 | 前缀再按这个键：显示预览并把焦点给它，在它拿着按键时隐藏它 |
| `auto` | `true` 或 `false` | `false` | 每当一个 Markdown 缓冲区成为当前缓冲区时显示预览 |
| `remote_images` | `"https"` 或 `"off"` | `"https"` | 加载 `https` 图片，或者一张远程图片都不加载 |
| `sync` | `"both"`、`"editor"` 或 `"off"` | `"both"` | 双向同步滚动；只由 Neovim 带动预览；或者完全不同步（文字仍然跟着更新） |
| `font_size` | 一个整数，9 到 32 | 编辑器的文字大小 | 预览文字的大小，单位是像素，不再跟随编辑器的大小 |
| `position` | `"side"`、`"bottom"` 或 `"main"` | 编辑器右边 | 预览第一次打开的位置 |

```lua
eitri.plugin.load("markdown", { key = "b", auto = false, remote_images = "https", sync = "both" })
eitri.layout.rule("lua:markdown", { at = "right", size = 0.45, focus = false })
```

和任何模块一样，`eitri.layout.rule` 决定预览在哪里打开（[新模块开在哪里](configuration.zh-CN.md#where-a-new-module-opens)）。选项不接受的值会被拒绝，Eitri 在启动时把它列出来并点出它的名字。

<a id="beside-your-own-neovim"></a>
## 开在你自己的 Neovim 旁边

如果你编辑用的是自己的 Neovim，而不是 Eitri 的窗口，预览可以单独成为一个窗口，开在它旁边。装好 [eitri.nvim](companion.zh-CN.md#the-eitrinvim-plugin) 后，在 Neovim 里输入 `:EitriPage markdown`；从 shell 里则是 `eitri page markdown --nvim <地址>`（在 Neovim 自己的终端里，地址默认取 `$NVIM`）。不需要 `eitri.plugin.load` 那一行：输入这条命令就是允许用默认设置加载这个插件，如果你的 `init.lua` 确实带选项加载了它，用的就是那些选项。

这个窗口跟随启动它的那个 Neovim 的缓冲区，用你的配色方案的颜色；从那个 Neovim 再输入一次 `:EitriPage markdown` 会提起它，而不是再开一个。在某个块上按 `Enter`，仍然会把 Neovim 的光标移到那里，并把 Neovim 的窗口带到前面。窗口由窗口管理器摆放，Eitri 不移动任何东西。

这里插件的命令只在窗口存在期间存在。`:EitriMarkdown` 把已经打开的窗口提到前面，`:EitriMarkdownHide` 和 `:EitriMarkdownToggle` 会关掉它并带走这些命令，所以之后 `:EitriMarkdown` 会以 Neovim 自己的 `E492: Not an editor command` 失败。`:EitriPage markdown` 会再次打开窗口。设了 `auto = true` 时，预览跟随你进入的 Markdown 缓冲区，不会跑到前面，也不会拿走按键。窗口怎样启动、存在和结束，见[开在你 nvim 旁边的页面](companion.zh-CN.md#pages-beside-your-nvim)。

<a id="limits"></a>
## 限制

- 超过 4 MB 或 50,000 行的文档不会被渲染；预览显示 `This document is too large to preview`，附上文档的大小和限制。Neovim 不受影响。
- 超过 512 KB 时，你输入期间预览最多每秒更新一次。
- 在预览里编辑、导出为 PDF 或 HTML、wiki 链接和幻灯片都不属于预览的功能。你自己的 Neovim 旁边的 agent 面板（[companion 窗口](companion.zh-CN.md)）自己不绘制插件页面；预览在那里是它自己的窗口，见上面一节。

<a id="your-own-copy"></a>
## 你自己的副本

预览是一个普通的插件目录。Eitri 先在你的数据文件夹里找名为 `markdown` 的插件（`~/.local/share/eitri/plugins/markdown/`，或 `$XDG_DATA_HOME/eitri/plugins/markdown/`），然后才找随 Eitri 一起安装的那份，所以你放在那里的副本优先。要修改预览，把 Eitri 启动时终端里点出的目录（例如 `[plugin markdown] loaded /usr/share/eitri/plugins/markdown (system data directory)`）复制到那个位置再编辑；下次启动时会显示 `(user data directory)`。删掉你的副本就回到 Eitri 自带的那份。升级永远不会动你的副本。[插件](configuration.zh-CN.md#plugins)说明了按名字查找的方式。
