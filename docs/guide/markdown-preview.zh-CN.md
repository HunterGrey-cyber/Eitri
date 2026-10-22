[English](markdown-preview.md) | 简体中文
<!-- translated-from: markdown-preview.md sha256=77f71d4a7658692b1809f7c36e520581a5e3098588c00069536b1cde73afd4c0 -->

# Markdown 预览

Eitri 可以把你正在编辑的 Markdown 文件渲染出来，显示在编辑器旁边：GitHub 风格的表格、任务列表、脚注、数学公式、图表和提示块，配色跟随你的配色方案。它跟着你的输入更新，和 Neovim 双向同步滚动，还可以用它自己的 Vim 风格按键来阅读。它是 Eitri 自带的一个插件，在你打开之前一直是关闭的。

预览只负责显示文件。编辑只在 Neovim 里进行：在预览里按 `Enter`，会把 Neovim 的光标移到你正在读的那一块，并把按键交给 Neovim。

<a id="turn-it-on"></a>
## 打开这个插件

在 `$XDG_CONFIG_HOME/eitri/init.lua`（默认 `~/.config/eitri/init.lua`）里加一行，然后开一个新窗口：

```lua
eitri.plugin.load("markdown")
```

在你打开预览之前，屏幕上什么都不会变。没有这一行，窗口里就没有预览的任何东西：没有模块，没有 Neovim 命令，也没有任何东西在运行。

<a id="open-and-close-it"></a>
## 打开和关闭预览

- `:MarkdownPreview` 在编辑器右边显示预览；`:MarkdownPreviewToggle` 显示或隐藏它；`:MarkdownPreviewStop` 隐藏它。从 Neovim 打开时，预览把按键留在 Neovim 里，你可以继续输入。
- 你自己的按键：`eitri.plugin.load("markdown", { key = "b" })` 让前缀再按 `b`（默认前缀下是 `Ctrl+b b`）显示预览并把按键交给它，在它拿着按键时再按一次就隐藏它。没有默认按键。
- 如果你的 Neovim 配置已经定义了 `:MarkdownPreview`（另一个预览插件，或者延迟加载它的占位命令），Eitri 不动那个命令，只定义另外两个，并在你启动 Eitri 的终端里打印 `[plugin markdown] :MarkdownPreview is already defined; not replacing it`。要用 Eitri 的命令，就删掉或改名你自己的那个。

- 在 Neovim 里用 VS Code 风格的按键。Eitri 不绑定它；想要的话，加到你的 Neovim 配置里：

  ```lua
  vim.keymap.set("n", "<C-S-v>", "<Cmd>MarkdownPreviewToggle<CR>")
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

Neovim 的窗口和预览的顶部显示同一行源码，经过折叠、很长的自动换行、很长的代码块和稍后才加载的图片时也是如此。哪一边拿着按键，哪一边带头：

- 按键在 Neovim 里（或者在预览以外的任何地方）时，在 Neovim 里滚动和跳转会带动预览，预览的边栏里有一条竖线标出 Neovim 光标所在的行。
- 按键在预览里时，滚动预览会带动 Neovim 的窗口，所以离开预览时，Neovim 就停在同一个位置。Neovim 会把光标保持在窗口里，所以窗口滚动时光标可能会跟着移动。
- 用鼠标滚轮滚动预览，即使按键在 Neovim 里，也会带动 Neovim 的窗口。Neovim 自己的下一次移动会把带头的位置拿回来。

<a id="reading-with-the-keyboard"></a>
## 用键盘阅读

用你的按键、在编辑器里按 `Ctrl+l`，或者点击预览，把按键交给预览。然后：

| 按键 | 作用 |
|---|---|
| `j` 或 `Ctrl+e` / `k` 或 `Ctrl+y` | 向下 / 向上滚动一步（按住不放会一直滚动） |
| `d` 或 `Ctrl+d` / `u` 或 `Ctrl+u` | 向下 / 向上半页 |
| `Ctrl+f` | 向下一页 |
| `gg` / `G` | 顶部 / 底部 |
| `]]` / `[[` | 下一个 / 上一个标题，放到顶部 |
| `zh` / `zl` | 横向滚动当前块（很宽的表格、代码块、公式或图表） |
| `/` | 在渲染后的文字里查找；`Enter` 执行搜索（空的搜索重复上一次），`Esc` 取消 |
| `n` / `N` | 下一个 / 上一个匹配，到头后从另一头接着找 |
| `f` | 在链接、脚注链接和折叠开关上显示标签；输入标签就跟随它 |
| `yy` | 复制当前块的 Markdown 源码（代码块只复制代码，不含围栏那两行） |
| `Enter` | 在这里编辑：把 Neovim 的光标移到这个位置，并把按键交给 Neovim |
| `H` 或 `Ctrl+o` / `L` 或 `Ctrl+i` | 在文档内部的跳转（标题链接、脚注）之间后退 / 前进 |
| `?` | 列出这些按键 |
| `Esc` | 退出当前模式；同时清除搜索高亮 |

- 当前块是视图顶部往下一行处的那一块；预览拿着按键时，边栏里有一条竖线标出它。点击一个块会让它成为当前块，直到你滚动为止。双击一个块，等于在那里按 `Enter`。
- 默认情况下，搜索不区分大小写，除非搜索里有大写字母，和 Vim 的 `smartcase` 一样（`plugins.keys.find_case` 可以改）。状态行显示 `[3/12]`，到头接着找时显示 `search hit BOTTOM, continuing at TOP`。
- 按下一个较长按键的第一个键（`g`、`z`、`y`、`[`、`]`）之后，状态行会列出接下来可以按的键。
- 如果你的 Eitri 前缀是这些 Ctrl 键中的一个，它仍然是前缀，预览里就没有这个键。
- 按 `yy` 之后，状态行显示 `yanked 3 lines`。
- 指向项目里另一个文件的链接会在 Neovim 里打开它（按键留在预览里，如果那个文件是 Markdown，预览接着显示它）；`src/main.rs#L10` 会打开到第 10 行。网页链接在你的浏览器里打开。指向项目以外的链接，或者指向不存在的文件的链接，会显示 `outside the project` 或 `no such file`，此外什么也不做。

和任何插件一样，在 `init.lua` 里用 `plugin:markdown` 这张表修改这些按键：

```lua
eitri.keymap.set("plugin:markdown", "e", "edit.here")
```

焦点在预览上时，`<prefix> ?` 会在 Eitri 自己的按键列表里先列出它的按键。动作名是 `scroll.down`、`scroll.up`、`scroll.half_down`、`scroll.half_up`、`scroll.page_down`、`scroll.top`、`scroll.bottom`、`heading.next`、`heading.prev`、`scroll.left`、`scroll.right`、`find.start`、`find.next`、`find.prev`、`hint.start`、`block.yank`、`edit.here`、`history.back`、`history.forward`、`sheet.open` 和 `mode.escape`。

一步滚动多远、滚动是否带动画、`g` 这样的键等多久、什么时候列出接下来的键、提示标签用哪些字母、搜索怎样处理大小写，这些是所有插件页面共用的设置：见[插件页面里的按键](configuration.zh-CN.md#keys-in-a-plugin-page)。

<a id="images"></a>
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
## 设置

| 选项 | 取值 | 默认 | 作用 |
|---|---|---|---|
| `key` | 一个键 | 无 | 前缀再按这个键：显示预览并把焦点给它，在它拿着按键时隐藏它 |
| `auto` | `true` 或 `false` | `false` | 每当一个 Markdown 缓冲区成为当前缓冲区时显示预览 |
| `remote_images` | `"https"` 或 `"off"` | `"https"` | 加载 `https` 图片，或者一张远程图片都不加载 |
| `sync` | `"both"`、`"editor"` 或 `"off"` | `"both"` | 双向同步滚动；只由 Neovim 带动预览；或者完全不同步（文字仍然跟着更新） |
| `position` | `"side"`、`"bottom"` 或 `"main"` | 编辑器右边 | 预览第一次打开的位置 |

```lua
eitri.plugin.load("markdown", { key = "b", auto = false, remote_images = "https", sync = "both" })
eitri.layout.rule("lua:markdown", { at = "right", size = 0.45, focus = false })
```

和任何模块一样，`eitri.layout.rule` 决定预览在哪里打开（[新模块开在哪里](configuration.zh-CN.md#where-a-new-module-opens)）。选项不接受的值会让 Eitri 在启动时停下，并点出它的名字。

<a id="limits"></a>
## 限制

- 超过 4 MB 或 50,000 行的文档不会被渲染；预览显示 `This document is too large to preview`，附上文档的大小和限制。Neovim 不受影响。
- 超过 512 KB 时，你输入期间预览最多每秒更新一次。
- 在预览里编辑、导出为 PDF 或 HTML、wiki 链接和幻灯片都不属于预览的功能。companion 窗口里没有插件页面。

<a id="your-own-copy"></a>
## 你自己的副本

预览是一个普通的插件目录。Eitri 先在你的数据文件夹里找名为 `markdown` 的插件（`~/.local/share/eitri/plugins/markdown/`，或 `$XDG_DATA_HOME/eitri/plugins/markdown/`），然后才找随 Eitri 一起安装的那份，所以你放在那里的副本优先。要修改预览，把 Eitri 启动时终端里点出的目录（例如 `[plugin markdown] loaded /usr/share/eitri/plugins/markdown (system data directory)`）复制到那个位置再编辑；下次启动时会显示 `(user data directory)`。删掉你的副本就回到 Eitri 自带的那份。升级永远不会动你的副本。[插件](configuration.zh-CN.md#plugins)说明了按名字查找的方式。
