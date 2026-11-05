English | [简体中文](markdown-preview.zh-CN.md)

# Markdown preview

Eitri can show the Markdown file you are editing rendered beside the editor: GitHub-style tables, task lists,
footnotes, math, diagrams and alerts, in your colourscheme's colours. It follows what you type, scrolls with Neovim in
both directions, and can be read with Vim-style keys of its own. It is a plugin that ships with Eitri, off until you
turn it on.

The preview only shows the file. You edit in Neovim, and only there: `Enter` in the preview moves Neovim's cursor to
where the preview's cursor is and gives Neovim the keys.

## Turn it on

Add one line to `$XDG_CONFIG_HOME/eitri/init.lua` (default `~/.config/eitri/init.lua`) and open a new window:

```lua
eitri.plugin.load("markdown")
```

Nothing changes on screen until you open the preview. Without that line nothing of the preview exists in the window:
no module, no Neovim command, nothing running.

## Open and close it

- `:EitriMarkdown` shows the preview to the right of the editor; `:EitriMarkdownToggle` shows or hides it;
  `:EitriMarkdownHide` hides it. Opened from Neovim, the preview leaves the keys in Neovim, so you keep typing.
  With [compositor windows](compositor-windows.md) the preview is a window of its own: these commands open it where your
  window manager puts it, and only your window manager closes it.
- A key of your own: `eitri.plugin.load("markdown", { key = "b" })` makes the prefix then `b` (`Ctrl+b b` with the
  default prefix) show the preview and give it the keys, and hide it again while it has them. There is no default key.
- The command names start with `Eitri` so they do not clash with the commands of other preview plugins. If your
  Neovim configuration already defines a command of the same name (`:EitriMarkdown`, say), Eitri leaves that command
  alone, defines the others, and prints `[plugin markdown] :EitriMarkdown is already defined; not replacing it` in
  the terminal you started Eitri from. Remove or rename your own command to use Eitri's.

- A key in Neovim, VS Code style. Eitri does not bind it; add it to your Neovim configuration if you want it:

  ```lua
  vim.keymap.set("n", "<C-S-v>", "<Cmd>EitriMarkdownToggle<CR>")
  ```

  On desktops where an input method such as fcitx5 owns `Ctrl+Shift` combinations, that chord never reaches Neovim;
  pick another key, for example `<leader>mp`.

## Preview automatically

```lua
eitri.plugin.load("markdown", { auto = true })
```

With `auto`, the preview appears, without taking the keys, whenever a Markdown file becomes the current buffer. It
never hides itself. If you hide it, it stays hidden until a different Markdown file becomes current.

## What it shows

- The current Markdown buffer, as it is in Neovim, not the file on disk: changes appear within about a fifth of a
  second, and the part you are reading stays in place while text above or below it changes.
- When you move to a buffer that is not Markdown (code, help, a terminal), the preview keeps the last Markdown file it
  showed and the status line at its bottom says `[not current]`.
- CommonMark and GitHub-flavoured Markdown: tables, task lists (shown, not clickable), strikethrough, autolinks,
  footnotes, and heading anchors with GitHub's names.
- Code blocks highlighted in your colourscheme's colours.
- Math with KaTeX in GitHub's forms: `$...$` and `` $`...`$ `` inline, `$$...$$` and ```` ```math ```` blocks. A
  formula with an error shows its source underlined, and the rest of the document still renders.
- Mermaid diagrams from ```` ```mermaid ```` blocks, in your colourscheme's colours. Diagram support is loaded only
  for a document that has one.
- GitHub alerts (`> [!NOTE]`, `[!TIP]`, `[!IMPORTANT]`, `[!WARNING]`, `[!CAUTION]`) and Obsidian's callouts:
  `> [!info]`, a title after the marker (`> [!note] Read this first`), and `[!note]-` (folded) or `[!note]+` (open).
- Front matter (a YAML block between `---` lines at the top) as a closed "Front matter" section.
- HTML in the document as GitHub shows it, without scripts, styles, frames or forms.

If you want Markdown to look rendered inside Neovim itself, plugins such as render-markdown.nvim and markview.nvim do
that in your own Neovim configuration; Eitri does not configure them.

## Scrolling together

The preview keeps the line you are on at the same height as Neovim does: the line Neovim's cursor is on sits as far
down the preview as it is down Neovim's screen (what markdown-preview.nvim calls `relative` scrolling), through folds,
long wrapped lines, long code blocks and images that load late. Side by side in Eitri's window that is the same height
on the screen; beside Neovim in a terminal it is when the preview's window is level with the terminal and as tall.
Whichever side has the keys leads:

- With the keys in Neovim (or anywhere outside the preview), moving and scrolling in Neovim move the preview, a
  cursor move that does not scroll Neovim included, and a bar in the preview's margin marks Neovim's cursor line. At
  the top and the bottom of the document the preview stops at its ends.
- With the keys in the preview, the preview has a cursor of its own, drawn like Neovim's `cursorline`: a bar in the
  margin and a band in your colourscheme's cursor-line colour. Neovim follows it: Neovim's cursor stands on the same
  line, at the same height, and Neovim's window scrolls only when the preview does, so leaving the preview finds
  Neovim at the same place with the cursor on that line. Where Neovim's `scrolloff` does not let the cursor sit on
  that line without scrolling, the cursor goes to the nearest line that does. A scroll still running when you leave
  finishes at once, and Neovim is told where it ended. If you leave without having moved, Neovim is told nothing and
  its cursor stays put.
- The cursor keeps a margin at the top and the bottom of the view, as Neovim's `scrolloff` does: at least five lines
  of the preview's text (what Vim's own defaults keep; Neovim's default `scrolloff` is 0), more when your Neovim's
  `scrolloff` keeps more rows, at most a quarter of the view. A move that would cross a margin scrolls the preview by
  just enough instead. At the top and the bottom of the document the margins give way, so the cursor reaches the
  first and the last line.
- Near the top of a document the preview's cursor can sit lower than Neovim's: rendered Markdown is taller than its
  source, and neither side scrolls above its first line. From the first scroll on they are level.
- Coming back to Neovim, nothing moves until you move in Neovim.
- Going into the preview, the cursor starts on the line Neovim's cursor is on, where the preview shows it, so nothing
  moves. On a line outside any block (a blank line between paragraphs) it starts on the nearest block, and `Enter`
  still leaves Neovim's cursor on its own line.
- Scrolling the preview with the mouse wheel moves the page, and the cursor stays on its line unless a margin pushes
  it, as in Neovim. While Neovim has the keys, the wheel moves Neovim's window too; Neovim's cursor then moves only
  as far as its `scrolloff` needs, and Neovim's next own move takes the lead back.

## Reading with the keyboard

Give the preview the keys with your key, with `Ctrl+l` from the editor, or by clicking it. Then:

| keys | what it does |
|---|---|
| `j` / `k` | the cursor down / up a line as shown (a long paragraph is several lines, as with Vim's `gj`) |
| `Ctrl+e` / `Ctrl+y` | scroll down / up a step; the cursor stays unless a margin pushes it |
| `d` or `Ctrl+d` / `u` or `Ctrl+u` | half a page down / up, the cursor with it |
| `Ctrl+f` / `Ctrl+b` | a page down / up |
| `gg` / `G` | the cursor to the first / last line (the document's last line at the bottom of the view, as in Vim) |
| `}` / `{` | the cursor to the next block (a paragraph, a list item, a table row, a whole code block) / to the start of its block, or of the one before when it is there already |
| `]]` / `[[` | the cursor to the next / previous heading |
| `zh` / `zl` | scroll the current block sideways (a wide table, code block, formula or diagram) |
| `/` | find in the rendered text; `Enter` runs the search (an empty one repeats the last), `Esc` cancels |
| `n` / `N` | next / previous match, wrapping round |
| `f` | labels on links, footnote links and fold toggles; type a label to follow it |
| `yy` | copy the current block's Markdown source (for a code block, the code without the fence lines) |
| `Enter` | edit here: move Neovim's cursor to this place and give Neovim the keys |
| `v` / `V` | select text from the cursor, by characters / by whole lines ([Selecting text](#selecting-text)) |
| `H` or `Ctrl+o` / `L` or `Ctrl+i` | back / forward between jumps inside the document: `gg`, `G`, `}`, `{`, `]]`, `[[`, a search and `n`/`N`, a heading link, a footnote |
| `?` | the list of these keys |
| `Esc` | leave a mode; also clears the search highlights |

- The current block is the one the cursor is in; a second bar in the margin marks it. A click puts the cursor on the
  line you clicked. Double-clicking a block does what `Enter` does there.
- `Enter` leaves Neovim's cursor on its line when it is already on the cursor's line (put there as you moved, or the
  line you came in with, where the column stays too), so Neovim's window does not scroll.
- The blocks `}` and `{` step through are the ones the bar in the margin marks.
- Moves that jump (`gg`, `G`, `}`, `{`, `]]`, `[[`, a search and `n`/`N`, a link inside the document, `H` and `L`) take the
  cursor to the target and scroll only when it is outside the margins: as little as needed when it is near, centred
  when it is far, as in Neovim. Scrolling glides, or jumps at once when smooth scrolling is off or your desktop asks
  for reduced motion. A count (`5j`) is not read.
- By default the search ignores case unless it has a capital letter, as in Vim's `smartcase`
  (`plugins.keys.find_case` changes that). The status line shows `[3/12]`, and `search hit BOTTOM, continuing at TOP`
  when it wraps.
- After the first key of a longer one (`g`, `z`, `y`, `[`, `]`), the status line lists the keys that may follow.
- If your Eitri prefix is one of these Ctrl keys, it stays the prefix and the preview goes without that key: with the
  default prefix, `Ctrl+b`, there is no `Ctrl+b` page up.
- After `yy` the status line says `yanked 3 lines`.
- A link to another file in the project opens it in Neovim (the keys stay in the preview, which then shows that file
  when it is Markdown); `src/main.rs#L10` opens at line 10. A web link opens in your browser. A link outside the
  project, or to a file that does not exist, shows `outside the project` or `no such file` and does nothing else.

### Selecting text

`v` starts a selection at the cursor, as in Vim, and `V` selects whole lines; the status line says `VISUAL` or
`VISUAL LINE`. Then:

| keys | what it does |
|---|---|
| `h` / `l`, `w` / `b` / `e`, `0` / `$` | move the cursor end of the selection by a character, a word, to the line's start or end |
| `j` / `k` | a line down / up, as shown |
| `}` / `{` | to the end of the block / to its start, then on to the end of the next block / the start of the one before: the selection grows a block at a time |
| `gg` / `G` | to the first / last text |
| `n` / `N` | to the next / previous match of the last search |
| `o` | to the other end of the selection |
| `y` | copy the selected text as you see it, and stop selecting |
| `Y` | copy the Markdown source of the lines the selection covers |
| `Enter` | edit the selection in Neovim: Neovim gets the keys, with the same text selected in its own visual mode |
| `v` / `V` | switch between characters and whole lines; the same key again stops selecting |
| `Esc` | stop selecting |

- The preview's cursor follows the end you move, so the selection stays inside the margins and Neovim follows as it
  does for `j`.
- The preview never changes your text. To change what you selected, press `Enter` and change it in Neovim (`c`, `d`,
  `gq`, ...).
- `Enter` selects the same characters in Neovim when your selection lies inside one source line and its text appears
  in that line exactly once. Otherwise Neovim selects the whole source lines it covers: rendered Markdown does not
  map back to source columns (`**bold**` shows as **bold**, a link shows only its text). If Neovim is at a prompt or
  on its command line, nothing is selected and the status line says so.
- A mouse click stops selecting, and so does giving the keys to another pane. The keys used while selecting can't be
  changed in `init.lua`; `v` and `V` themselves can (`visual.char`, `visual.line`).

Change any of these keys in `init.lua`, as for any plugin, in the `plugin:markdown` table:

```lua
eitri.keymap.set("plugin:markdown", "e", "edit.here")
```

`<prefix> ?` with the preview focused lists its keys first in Eitri's own key list. The actions are `cursor.down`,
`cursor.up`, `cursor.half_down`, `cursor.half_up`, `cursor.page_down`, `cursor.page_up`, `cursor.first`,
`cursor.last`, `block.next`, `block.prev`, `scroll.down`, `scroll.up`, `heading.next`, `heading.prev`, `scroll.left`, `scroll.right`, `find.start`,
`find.next`, `find.prev`, `hint.start`, `block.yank`, `edit.here`, `history.back`, `history.forward`, `sheet.open`,
`mode.escape`, `visual.char` and `visual.line`. If you changed these keys for a release candidate, rename an override
of `scroll.half_down`, `scroll.half_up`, `scroll.page_down`, `scroll.page_up`, `scroll.top` or `scroll.bottom` to the
matching `cursor.` action (`cursor.half_down`, ..., `cursor.first`, `cursor.last`): the preview no longer has those `scroll.` actions.

How far a step scrolls, whether scrolling is animated, how long a key such as `g` waits, when the list of following
keys appears, the letters hint labels use and how the search treats case are settings shared by every plugin page:
see [Keys in a plugin page](configuration.md#keys-in-a-plugin-page).

## Images

- Images in your project show: a relative path is read from the document's folder, a path starting with `/` from the
  project's top folder. The project is the folder Eitri was started in when the document is inside it, else the
  nearest folder above the document that holds `.git`, else the document's own folder; your home folder and `/` never
  count as a project. An image outside the project shows as a box saying `outside the project`, and an unsaved buffer's
  relative images as `save the file to show local images`.
- Remote images follow VS Code's default: `https` images load, `http` images do not (a box says `http image not
  loaded`). The status line counts the images that were not loaded.
- **Privacy:** loading a remote image tells the server that hosts it your address and the time you opened the
  document. To load no remote image at all:

  ```lua
  eitri.plugin.load("markdown", { remote_images = "off" })
  ```

  Remote images then show as a box saying `remote images are off`, and the preview makes no network request. Nothing
  else in the preview uses the network: no fonts, scripts or diagrams are fetched, and links are never prefetched.
- A changed image file shows when you switch documents or reopen the preview; the preview does not watch files.

## Text size

The preview's text is the size of the editor's: Neovim's `guifont` size, drawn at the same size on screen. It follows the editor while you work. Change `guifont`, or size the window with `Ctrl+=`,
and the preview changes with it. Headings, code and tables keep their proportions to the text.

To size the preview on its own, give it `font_size`, a whole number from 9 to 32 (in the page's pixels, as
[`agent.font_size`](configuration.md) is):

```lua
eitri.plugin.load("markdown", { font_size = 16 })
```

In a window [beside your own Neovim](#beside-your-own-neovim) Eitri does not know your terminal's font, so the preview
keeps a size of its own, 14 pixels, unless you give `font_size`.

## Settings

| option | values | default | what it does |
|---|---|---|---|
| `key` | one key | none | the prefix then this key shows and focuses the preview, or hides it while it has the keys |
| `auto` | `true` or `false` | `false` | show the preview whenever a Markdown buffer becomes current |
| `remote_images` | `"https"` or `"off"` | `"https"` | load `https` images, or no remote image at all |
| `sync` | `"both"`, `"editor"` or `"off"` | `"both"` | scroll together both ways; only Neovim moves the preview; or not at all (the text still follows) |
| `font_size` | a whole number, 9 to 32 | the editor's text size | the preview's text size in pixels, instead of following the editor's |
| `position` | `"side"`, `"bottom"` or `"main"` | right of the editor | where the preview first opens |

```lua
eitri.plugin.load("markdown", { key = "b", auto = false, remote_images = "https", sync = "both" })
eitri.layout.rule("lua:markdown", { at = "right", size = 0.45, focus = false })
```

`eitri.layout.rule` decides where the preview opens, as for any module ([Where a new module
opens](configuration.md#where-a-new-module-opens)). A value an option does not accept is refused and listed when
Eitri starts, naming it.

## Beside your own Neovim

If you edit in your own Neovim rather than in Eitri's window, the preview can open as a window of its own beside it. From
Neovim, with [eitri.nvim](companion.md#the-eitrinvim-plugin) installed, type `:EitriPage markdown`; from a shell,
`eitri page markdown --nvim <address>` (the address defaults to `$NVIM` inside Neovim's own terminal). No `eitri.plugin.load`
line is needed: typing the command is the permission to load the plugin with its default settings, and if your `init.lua`
does load it with options, those are used.

The window follows the buffer of the Neovim it started with, in your colourscheme's colours, and a second `:EitriPage markdown`
from that Neovim raises it instead of opening another. `Enter` on a block still moves Neovim's cursor there and brings the
Neovim window forward. A window manager places the window; Eitri moves nothing.

Here the plugin's commands exist only while the window does. `:EitriMarkdown` brings an open window forward, and
`:EitriMarkdownHide` and `:EitriMarkdownToggle` close it and take the commands with them, so afterwards `:EitriMarkdown` fails with
Neovim's own `E492: Not an editor command`. `:EitriPage markdown` opens the window again. With `auto = true` the preview follows
the Markdown buffers you enter without coming forward or taking the keys. The window starts, lives and ends as
[Pages beside your nvim](companion.md#pages-beside-your-nvim) describes.

## Limits

- A document over 4 MB or 50,000 lines is not rendered; the preview says `This document is too large to preview`, with
  the document's size and the limits. Neovim is not affected.
- Over 512 KB the preview updates at most once a second while you type.
- Editing in the preview, exporting to PDF or HTML, wiki links and slides are not part of the preview. The agent panel
  beside your own Neovim (the [companion window](companion.md)) does not draw plugin pages in itself; the preview opens
  there as its own window, as the section above describes.

## Your own copy

The preview is an ordinary plugin directory. Eitri looks for a plugin named `markdown` in your data folder first
(`~/.local/share/eitri/plugins/markdown/`, or `$XDG_DATA_HOME/eitri/plugins/markdown/`), then in the copy that came
with Eitri, so a copy you put there wins. To change the preview, copy the directory the terminal names when Eitri starts
(`[plugin markdown] loaded /usr/share/eitri/plugins/markdown (system data directory)`, for example) to that place and
edit it; the next start says `(user data directory)`. Delete your copy to go back to Eitri's. An upgrade never touches
your copy. [Plugins](configuration.md#plugins) says how a name is looked up.
