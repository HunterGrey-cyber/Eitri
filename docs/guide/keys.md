English | [简体中文](keys.zh-CN.md)

# Keys

Eitri's keys come from three places, and they do not collide: a **prefix** for the window itself (tabs,
modules, layout), a few keys that work **everywhere** in the window, and the **agent panel's own keys**,
which are vim's. Everything below is the default setup. `F1` in the panel lists the keys of your own setup,
including anything you changed in [your configuration](configuration.md) or took over from tmux.

> **First time with prefix keys?**
>
> - Press `Ctrl+b`, let go, then press one key. `Ctrl+b ?` lists every key, and `Esc` after `Ctrl+b` cancels it.
> - `Ctrl+b` belongs to the window, so to give it to whatever has the keys, press it twice: `Ctrl+b Ctrl+b` is
>   Neovim's page up, and the key Claude Code uses to move a running command to the background when it runs in the
>   bottom terminal.
> - `Ctrl+h`, `Ctrl+j`, `Ctrl+k` and `Ctrl+l` move between modules, so in the bottom terminal the shell's clear-screen
>   is `Ctrl+b Ctrl+l` (and readline's `Ctrl+k` is `Ctrl+b Ctrl+k`). In the agent panel `Ctrl+j` while browsing starts
>   typing and `Ctrl+k` while typing goes back to browsing; otherwise they leave the panel
>   ([Moving between panes](#moving-between-panes)).
> - To have Eitri list the next keys when you pause after the prefix, put
>   `eitri.config.set("keymap.which_key_delay", 200)` in your `init.lua` ([The hint panel](#the-hint-panel)).
> - If you use tmux, your prefix and prefix-table bindings come from your tmux configuration
>   ([Your tmux keys](#your-tmux-keys)), and `Ctrl+b ?` (or your own prefix and `?`) lists what did not come across.
> - For a `Ctrl+a` prefix, start from [`docs/keymap/tmux-ctrl-a.lua`](../keymap/tmux-ctrl-a.lua). On i3 or sway with
>   Alt as `$mod`, Eitri moves its Alt keys after the prefix to Ctrl+Alt by itself and says so once
>   ([below](#containers-and-terminals)); [`docs/keymap/i3-alt-mod.lua`](../keymap/i3-alt-mod.lua) is the same moves as a file.
> - On a first launch without an `init.lua`, Eitri offers to set the keys up and write one
>   ([First start](configuration.md#first-start)); `:Eitri setup` brings the screen back.
> - `:EitriTutor` in the editor walks you through these keys, step by step, in English or Simplified Chinese
>   (whichever your locale asks for, as Neovim's `:Tutor` picks). It says your own prefix wherever a lesson means
>   it, and teaches the stock keys after the prefix, so a tmux configuration that rebinds some of them is not
>   reflected; your prefix and `?` list yours.
> - **On Hyprland, sway and niri this page's layout keys are off by default,** because your window manager arranges
>   the windows: `Ctrl+b %` shows one hint and does nothing else. [Compositor windows](compositor-windows.md#keys) says
>   which keys still work there, and `window.tiling = "eitri"` brings the rest back. There `:EitriTutor` teaches that
>   mode's keys instead.

## The prefix

The prefix is stock tmux's: press `Ctrl+b`, let go, then press one key. It is read before the editor, the
panel and the bottom terminal see anything, so it works wherever the keys are. `Esc` cancels it, and a key the
table does not bind ends the wait without doing anything. To send `Ctrl+b` itself to whatever has the keys,
press it twice. `<prefix> ?` opens the full keys sheet at the "After <prefix>" table. While the prefix is armed,
the hint strip ends with `? keys`; it uses the effective key for `panel.keymap`, and leaves that run out if unbound.

The keys marked "repeats" can be pressed again within half a second without the prefix, as in tmux. If you
would rather see the keys than remember them, [The hint panel](#the-hint-panel) lists them after a pause.

**Tabs.** Each conversation in the agent panel is a tab.

| keys | what they do |
|---|---|
| `<prefix> c` | a new tab |
| `<prefix> n` / `<prefix> p` | the next / the previous tab |
| `<prefix> 1` ... `9` | that tab by number |
| `<prefix> l` | the tab you were on before |
| `<prefix> ,` | rename this tab |
| `<prefix> &` | close this tab, after a `y/n` question |
| `<prefix> w` | choose a session: `Restore last session` first while the last window's tabs are on offer (the way to take it from the editor, where `s` is nvim's), then the open tabs, then saved ones to resume |
| `<prefix> i` | this session's details |

**Agents that wait.** `<prefix> u` goes to the agent that has waited longest for you, in whichever window it is, and each
further press goes to the next one; `<prefix> B` leaves the walk and goes back to where it started. Neither answers
anything: the card is in front of you, and you answer it there, in card mode. With nothing waiting the
status line says `no agent waits for you`. They share their walk with the pages that show your agents, and they work
in a companion window too. With `agents.enabled` off they say `agents are off in this window`. See
[Many agents at once](many-agents.md). `<prefix> U` shows the Strips sidebar of your agents with the keys on it,
and hides it again from there, once `init.lua` loads it (`eitri.plugin.load("agents-strips")`).

**A second window.** `<prefix> W` opens a [second Eitri window](two-windows.md), for a second monitor, or brings it to the
front when it is open. It does nothing in [compositor windows](compositor-windows.md) and in a companion window,
where it says why.

**Projects.** `<prefix> O` (also `:Eitri project.choose`, a click on the project's name in the top bar, or `p` on an empty
chat tab) lays a list over the window: your other projects, the one you used last first, with a line to filter it or to
type a path. It works in every window, the chat and page windows of [compositor windows](compositor-windows.md) included,
and it changes nothing in the window: `Esc` or `q` closes it and the keys go back to where they were.

It opens on the list, with the keys of the session list (`<prefix> w`): the line is not taking text yet, and its
placeholder says `/` filters or types a path.

| keys in the list | what they do |
|---|---|
| `j`, `Down`, `Ctrl+n`, `Ctrl+j` / `k`, `Up`, `Ctrl+p`, `Ctrl+k` | move down / up the list (it does not wrap) |
| `gg` / `G` | the first / the last row (a `g` waits for the next key; any other key ends the wait and does nothing) |
| `Enter` | go to the selected row |
| `h`, `Left` | look around the folder that holds the selected project, with the project selected |
| `l`, `Right` | look inside the selected project |
| `-` | look around the folder above this window's project (in the small start window, your home folder) |
| `~` | look around your home folder |
| `/` | go to the line, empty, to filter |
| `Ctrl+o` | the system's folder dialog |
| `Esc`, `q` | close the list; nothing happened |

The last row, `Browse folders…`, does what `-` does. Any other plain key does nothing. In the line:

| keys in the line | what they do |
|---|---|
| typing | filters by name and path, ignoring case; text that starts with `/`, `~`, `./` or `../` is read as a path instead |
| `Down`, `Ctrl+n`, `Ctrl+j` / `Up`, `Ctrl+p`, `Ctrl+k` | move down / up the list (`j` and `k` are typed here) |
| `Enter` | go to the selected row |
| `Tab` | complete a typed path as far as the folders agree |
| `Ctrl+o` | the system's folder dialog |
| `Esc` | empty the line and go back to the list (a second `Esc` closes it) |

**Browsing folders.** A path typed in the line, or any of `h`, `l`, `-` and `~` in the list, shows the subfolders of
a folder as rows, so you can walk to a project Eitri has not opened before. The line holds the path of the folder you
are in, and the first row, `.`, is that folder itself. A folder that is a git repository is marked `git`, a link
`link`, a mount point `mount`, and a file you named exactly `file`. Nothing is read until you ask for a folder: opening the
list reads no folder, and typing letters after the last `/` only filters what is shown.

| keys while browsing | what they do |
|---|---|
| `j`, `k`, `Down`, `Up`, `Ctrl+n`, `Ctrl+p`, `Ctrl+j`, `Ctrl+k` | move |
| `gg` / `G` | the first / the last row |
| `l`, `Right`, `Enter` on a folder | go into it, with the cursor on `.` |
| `Enter` on `.` | open this folder as a project: the same as a project row, a new window and its trust question |
| `Enter` on the `file` row | open the file |
| `h`, `Left`, `-`, `Backspace` | go up one folder, with the cursor on the folder you left |
| `~`, `gh` | your home folder |
| `.` | show or hide folders that start with a dot |
| `/` | edit the path in the line: the text after the last `/` filters, a change before it reads another folder |
| `Tab` (in the line) | complete the folders' shared prefix; with one left, go into it |
| `Ctrl+w` (in the line) | delete back to the previous `/` |
| `Ctrl+o` | the system's folder dialog, starting at this folder |
| `Esc` | go back (from the line to browsing, from browsing to the list) |
| `q` | close |

`Enter` on a folder goes into it and `Enter` on `.` opens it, so a folder seen from its parent takes `l`, then `Enter`
(and a typed path ending in `/` opens with one `Enter`). The footer says what `Enter` will do to the selected row. A click
selects a row, and a double-click does what `Enter` does on it. Only a folder Eitri has read is ever opened. A folder on a
network share or other mount that does not answer says `not answering` within two seconds; the other keys keep working,
and `h` goes back. A folder with more than 2000 subfolders shows the first 2000 and says so, and `/` searches all of them.
Names are shown as text: a control character, a right-to-left override or a zero-width space appears as a `\u{...}`
escape, and `Enter` still opens the folder by its real name. Like the list of projects, what you browse is shown in
this chooser only and is never sent to the agent panel, an agent or Neovim.

A row shows the folder's name, its path (`~` for your home folder) and, on the right, `open` when a window already holds
the project, `gone` when the folder is no longer there, and otherwise how long ago you last used the project (`5 m`, `2 h`,
`3 d`, `4 w`). The window's own project is left out, so `<prefix> O` then `Enter` goes to the project you used before
this one, as `<prefix> L` goes to the workspace you were on before. Your home folder and `/` come after every other
project, and a project whose folder is gone comes last (at most 30 projects and 5 gone ones are listed). `Enter` on a
project that has a window brings that window forward, across workspaces where your window manager allows it, as it is and
on the account it started with; on any other project it opens a new window, on this window's account. A typed path that names a file opens it the way `eitri FILE` does; a name that does not
exist shows `no such folder: <path>` and nothing is created, and a `gone` row shows `<path> is not there` and stays.
Eitri builds the list from its own records of your projects (the layout and tabs it saves, and a note each window keeps
of its project and when you last used it) and reads nothing else on your disk; a project you opened and left alone is in
it. The list is never sent to the agent panel, an agent or Neovim, and there is no key to remove a row. While it is open
a running turn goes on, but its permission cards wait for the list to close: `Enter` in the list cannot answer one.
**Modules.** The window is made of modules: the editor, the agent panel and the bottom terminal.

| keys | what they do |
|---|---|
| `<prefix> e` / `a` / `t` | show and focus the editor / the agent / the terminal, or hide it when it already has the keys; once nvim has quit in the editor (`<prefix> x` on it, or a `:q` while the window stayed open), showing the editor starts a fresh nvim in its place |
| `<prefix> Left`, `Right`, `Up`, `Down` | move the keys to the module on that side (repeats) |
| `<prefix> ;` | move the keys back to the module that had them before |
| `<prefix> o` | move the keys to the next module on screen |
| `<prefix> z` | zoom this module to fill the window, or restore it. In this guide "zoom" only ever means this; changing the size of text is "text size" |
| `<prefix> %` or `<prefix> "`, then `e`, `a` or `t` | open that module to the right of / below this one, or move it there; with the keys in a terminal, `t` opens a new terminal there (see [Containers and terminals](#containers-and-terminals)) |
| `<prefix> d`, then `e`, `a` or `t` (or a panel's own key) | hide that module wherever the keys are; it keeps running. The keys stay where they are unless that module had them, and then go to its neighbour; `Esc` cancels |
| `<prefix> !` | with the keys in the agent panel: break the active tab out into a chat view of its own, to the right of the panel; with the keys in such a view, join its tab back (see [Chat views](#chat-views)) |
| `<prefix> {` / `<prefix> }` | swap this module with the previous / the next one on screen |
| `<prefix> Alt+1` / `Alt+2` | every module in one row / in one column, at equal sizes |
| `<prefix> Alt+3` / `Alt+4` / `Alt+5` | the first module big on top with the others in a row below / big on the left with the others in a column beside it / all modules in a grid (tmux's `main-horizontal`, `main-vertical` and `tiled`) |
| `<prefix> Space` | the next arrangement of the cycle: one row, one column, `Alt+3`, `Alt+4`, `Alt+5`, then one row again; it starts after the last one you chose, or at one row once the modules have been rearranged some other way, or one closed, since; a divider moved, a zoom, a mark, or a module shown or hidden keeps its place in the cycle |
| `<prefix> E` | spread the modules around this one evenly, along the direction they are split (tmux's `select-layout -E`) |
| `<prefix> H` / `J` / `K` | move this module to the far left / bottom / top edge of the window, full height or width (vim's `Ctrl+w H`, `J`, `K`); nothing happens when it is already there |
| `<prefix> Alt+e` | turn the split this module is in from side by side to stacked, or back (i3's `layout toggle split`) |
| `<prefix> Ctrl+o` / `Alt+o` | rotate every module on screen one place up / down through the same spaces (tmux's `rotate-window`) |
| `<prefix> m` / `M` | mark this module, or clear the mark when it is the marked one / clear the mark; one module is marked at a time, and the top bar shows `marked: <title>` while there is a mark |
| `<prefix> q` | draw a number on every module on screen; press that number (`1` to `9`, then `0`) to give that module the keys. `Esc` or any other key ends it, and so does one second without a key |
| `<prefix> Ctrl+Up`, `Ctrl+Down`, `Ctrl+Left`, `Ctrl+Right` | move the nearest divider one cell that way (repeats) |
| `<prefix> Alt+Up`, `Alt+Down`, `Alt+Left`, `Alt+Right` | the same, five cells (repeats) |
| `<prefix> x` | close this module and end what runs in it, after a `y/n` question; closing the last one on screen closes Eitri, and then the question, like closing the window, also names the chat turns running or queued and the terminals running a command (`kill-pane editor? closes Eitri, 2 terminal jobs (y/n)`); a job left in the background at a shell prompt is not counted. On the editor it quits nvim as `:confirm qall` does (nvim asks about unsaved buffers itself), and the editor comes back with a fresh nvim on `<prefix> e` |
| `<prefix> [` or `<prefix> PageUp` | scroll the bottom terminal back (copy mode); only while the terminal has the keys, elsewhere it flashes a refusal and the toast says `prefix [ scrolls the terminal` |
| `<prefix> F11` | immersive: fullscreen without the top bar |
| `<prefix> Ctrl+=` / `Ctrl+-` / `Ctrl+0` | text size larger / smaller / back in step, for the pane with the keys only: the editor, the chat, a plugin's page or the terminal. One prefix takes as many steps as you like: `=`, `-` and `0` stay on this pane for as long as Ctrl is down, and until half a second after the last step |

Each of these arrangements ends a zoom when it changes something and leaves it on when there was nothing to do. The
keys stay with the module that had them, and a hidden module stays hidden. With fewer than two modules on screen they
change nothing and the refusal flashes; `Alt+3`, `Alt+4`, `Alt+5` and `Space` also refuse more than 20 modules on
screen.

A refused key flashes Eitri's name in the top bar, and the toast at the top right says why (`rotate: fewer than two
modules on screen`, `no other module on screen`). A few refusals have nothing to say and only flash, such as a tab key
with no tab that way. While the toast is telling you that a hidden chat has a permission card waiting, a refused key
leaves that toast up and only flashes.

**Actions with no key.** These are reached by name: from a Lua command with `eitri.layout.run` (see
[Configuration](configuration.md#layout-from-lua)) and, once bound, in the list under `<prefix> ?`. To give one a key, add a line
to your `init.lua`; the keys here are examples that no default uses:

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

`layout.undo` keeps one step in memory and never saves it: it swaps the arrangement with the one before the last
change (a divider moved since then is undone with it), so pressing it again redoes. Showing or hiding a module is not
a change it sees. Closing the terminal or a Lua panel with `<prefix> x` puts its place back where it opens next, and
that becomes the last change: `layout.undo` right after it only moves the closed module's place back, so nothing on
screen changes, and the change before the close can no longer be undone.

**Setting a key that already has an action.** The keys in this section are defaults, and your `eitri.keymap.set`
wins over them: it takes the key, and Eitri says so once when it starts, naming the key and the action it replaced
(it says so again only when that list changes). Setting a key twice is the same: the last call wins, as with vim's
maps. Take a key and you lose the default action of that key, so a release that gives a default a key you already
use does not stop Eitri, and does not change what your key does; the new default is simply not on that key. The
message is also on stderr. A `del` before the `set` still works, and says nothing.

A call Eitri cannot apply at all (a key it cannot read, an action it does not know, a `del` of a key that nothing is
bound to) is left out, and Eitri lists it at startup with the call as you wrote it. The rest of your keys apply.

**The window and the panel.**

| keys | what they do |
|---|---|
| `<prefix> f` | HINT: jump anywhere in the window (see [HINT](#hint)) |
| `<prefix> r` | reload the agent panel; the session keeps running |
| `<prefix> ?` | the full list of keys, in the panel, starting at "After <prefix>" (from a plugin page, its own keys come first); the chat's own `F1` starts at the section of the mode you pressed it in ("This panel" from BROWSE). Closing it with `?`, `Esc`, `q` or a click outside gives the keys back to the module that had them, and puts back a zoom or a hidden chat that opening it changed |
| `<prefix> :` | the panel's `:` command line (see [The command line](#the-command-line)) |
| `<prefix> Ctrl+h`, `Ctrl+j`, `Ctrl+k`, `Ctrl+l` | send that chord itself to whatever has the keys; the bare chords below are Eitri's |

**Without the prefix**, these work anywhere in the window (and [Keys without the prefix](#keys-without-the-prefix)
adds yours):

| keys | what they do |
|---|---|
| `F11` | fullscreen |
| `Ctrl+=` / `Ctrl+-` / `Ctrl+0` | text size larger / smaller / reset, for every module of the window: the editor, the chat, every plugin page and every terminal (the keypad's `+`, `-` and `0` work too, and so does `Ctrl++` where `+` is a key of its own, as on a German keyboard) |
| `Ctrl+wheel` | text size of the pane under the pointer only |

**How text sizes add up.** The window has one text size, and each module has its own difference from it. `Ctrl+=` and
`Ctrl+-` move the window's size, so every module moves and keeps its difference. `<prefix> Ctrl+=`, `Ctrl+-` and
`Ctrl+wheel` change one difference: every chat view shares one, every terminal shares one, the editor has one and each
plugin page has its own. `<prefix> Ctrl+0` clears the difference of the pane with the keys, so that pane rejoins the
others wherever they are. `Ctrl+0` clears every difference and puts the window's size back to what your configuration
says: `g:neovide_scale_factor` if your `init.lua` sets it, otherwise 100%.

- **What moves.** Every module of this window, shown or hidden. The top bar and the hint panel keep their size, and
  with the top bar holding the keys the pane keys do nothing.
- **Other windows.** Each window of Eitri has its own text size. A second window starts at 100%. A window that holds
  one module (a [companion window](companion.md), a chat or page window in [compositor mode](compositor-windows.md))
  has one size, so there `<prefix> Ctrl+=` and `Ctrl+=` are the same action.
- **Nothing lasts past a restart.** For a size that does, set `guifont`, `agent.font_size`, `terminal.font` or
  `g:neovide_scale_factor` in your configuration.
- **Your own mappings.** Eitri takes `Ctrl+=`, `Ctrl+-`, `Ctrl+0`, `Ctrl++` and `Ctrl+wheel` before Neovim sees them, so
  a `<C-=>` mapping in Neovim does not run. `<C-+>` does on a keyboard that types `+` with Shift, where `Ctrl+Shift+=`
  goes on to Neovim as it always has. A mapping of your own reaches every module through `g:neovide_scale_factor`:
  Eitri follows any write to it, a `:let` included, and keeps each module's difference.
- **Keyboards that type these with Shift.** Where `=` or `0` needs Shift, hold it as you would to type the key: on a
  German keyboard `<prefix> Ctrl+Shift+0` is `<prefix> Ctrl+=`, and `Ctrl+Shift+0` alone sizes every module.

A plugin's page (the learning plugin, the Markdown preview) scales whole, its layout with its text, and follows the
window's text size: `Ctrl+=` grows every page with everything else, a page made larger or smaller on its own keeps that
difference until `Ctrl+0` or `<prefix> Ctrl+0` on it, and the page keeps its size when it reloads. The page never
sees these keys.

The terminal draws in the editor's font, the families and the size of your `guifont` (`terminal.font` in
`init.lua` changes that, see [Configuration](configuration.md#settings)), and follows the window's text size the same
way: `Ctrl+=` grows it with everything else, and `Ctrl+wheel` over a terminal or `<prefix> Ctrl+=` in one sizes it alone,
until `Ctrl+0` or `<prefix> Ctrl+0` there. Every terminal of the window is one size, as every chat view is. A size change is
a new grid for the shell: a larger font is fewer rows and columns, and the program running in it is told, as when the
pane itself is resized (`stty size` shows the new numbers). The program never sees these keys or `Ctrl+wheel`.

A companion window (see [Companion mode](companion.md)) has no layout of its own, so there the prefix keeps
the tab keys, `r`, `?`, `:`, `f`, the text size keys (`Ctrl+=`, `Ctrl+-`, `Ctrl+0`), sending the prefix itself
or a `Ctrl+h/j/k/l` chord, and the arrow keys, which move toward the neighbouring window; a layout or window key answers
`not here: your window manager arranges this window` the first time you press one in that window, and nothing after.

### Containers and terminals

Modules sit in containers. A container is **split** (its modules side by side, or one above the other: what `%`
and `"` make), **tabbed** (one module shown at a time, with a row of titles above it) or **stacked** (one module
shown at a time, with a row of title for each module above it). The keys below are i3's `$mod` chords, with Alt
as `$mod`, pressed after the prefix. A title is the module's name as the top bar's chips show it (`terminal 2`;
`agent ⚑1` when the agent has a card waiting), and it is drawn exactly as written, never as markup.

| keys | what they do |
|---|---|
| `<prefix> Alt+w` / `Alt+s` | make the container this module is in tabbed / stacked; pressed again, the container goes back to the split it was, with its sizes. "This module's container" is the nearest one around it that has another module on screen, so in the default window it is the whole window, not the editor's column with a hidden terminal. With none, nothing happens and the refusal flashes |
| `<prefix> Alt+H`, `Alt+J`, `Alt+K`, `Alt+L` | move this module one step left / down / up / right (repeats; i3's `move`). It changes places with the module on that side, or goes into the container there beside the module that container shows, or leaves its container for the next one out; at the window's edge nothing happens. `<prefix> H`, `J`, `K` go all the way to the edge instead |
| `<prefix> Alt+a` / `Alt+d` | select the parent container of what is selected / go back down to the child that has the keys (i3's `focus parent` and `focus child`) |
| `<prefix> Alt+Enter` | open a new terminal to the right of the module that has the keys; its shell starts in the project root |

If your i3 or sway `$mod` is Alt, the window manager takes those chords before Eitri sees them. Eitri finds that out at start (from the running window manager's configuration, else its config file) and, unless your `init.lua` already sets or deletes one of those keys, or your tmux config (read through `keymap.from_tmux`) binds or unbinds one, moves them for that window, as [`docs/keymap/i3-alt-mod.lua`](../keymap/i3-alt-mod.lua) does, saying so once: `sway's $mod is Alt, so Eitri moved its Alt keys after the prefix to Ctrl+Alt for this window`. `eitri.keymap.use("i3-alt")` in your `init.lua` makes the moves yours; setting or deleting any of those keys yourself stops it. The `?` sheet marks the moved rows. The file moves the rows it grabs: each keeps its key with Ctrl added (`<prefix> Ctrl+Alt+w`, `Ctrl+Alt+Enter`, and so on). The rows whose key has Shift in it drop the Shift instead of keeping it: Eitri does not read Ctrl+Shift+<character> as a chord, so the four moves (`Alt+H`, `Alt+J`, `Alt+K`, `Alt+L`) become `<prefix> Ctrl+Alt+h`, `Ctrl+Alt+j`, `Ctrl+Alt+k`, `Ctrl+Alt+l`, and the move-to-scratchpad row (`Alt+_`) is only deleted, since `<prefix> _` already does it; the float toggle (`Alt+Shift+Space`) becomes `<prefix> Ctrl+Alt+Space`, since Ctrl+Shift+Space switches the input method on many setups. Paste it into your `init.lua`.

**Tabs and stacks.** Click a title, or press `f` and type the title's label (HINT labels every title), to bring
that module forward and give it the keys. From the keyboard, `Ctrl+h` / `Ctrl+l` step to the tab on the left /
right in a tabbed container and `Ctrl+k` / `Ctrl+j` to the one above / below in a stacked one; when there is no
tab that way they go on as between modules. The module that has the keys is always the one shown, so hiding or
closing it gives the keys to the tab that shows next. A tab that is not shown keeps running: a terminal there goes
on printing and shows its lines when it comes back, and a chat keeps its session. A chat on a tab that is not
shown carries `agent ⚑N` on its title while it has cards waiting; with `modules.chat.on_permission` set to
`"reveal"` a new card brings its tab forward, except when the keys are on a sibling tab of the same container: then
you get the toast and nothing moves.

**Selecting a container.** `<prefix> Alt+a` selects the container around the module that has the keys, and the
top bar shows it as `selected: column (editor, terminal)`: the container's kind and the titles of the modules
shown in it. Pressing it again goes up one more container (the window's own is the last), and `<prefix> Alt+d`
goes back down. While a container is selected, the keys that arrange (`Alt+H`, `Alt+J`, `Alt+K`, `Alt+L`, `Alt+w`,
`Alt+s`, `Alt+e`, `z`, `H`, `J`, `K`, `E` and the flips) act on the whole container. The ones that need a single
module (`x`, hiding a module, the swaps and the marks) refuse and flash `acts on one module`. Giving the keys to
another module, opening a module with a split key, and `Esc` after the prefix end the selection. Nothing is drawn
around the selected container; the top bar's text is all there is.

**Terminals.** A window can hold up to nine terminals, `terminal` and `terminal 2` to `terminal 9`, each with its
own shell. A new one comes from `<prefix> Alt+Enter`, or from a split key with the keys in a terminal:
`<prefix> %` or `"` and then `t` opens a new terminal on that side, where from any other module `t` opens the one
terminal. The prefix's hint reads `t new terminal` then, and `t terminal ×3` when several are open. `<prefix> t`
on its own reaches the terminal that has the keys, or else the one you used last. A split binding with
`cwd = "pane"` starts the new shell in the directory the terminal you split is in (see
[Configuration](configuration.md#layout-from-lua)). `<prefix> x` on a numbered terminal ends its shell and removes
it, after the usual `y/n`; the `terminal` itself is only closed and reopens below the editor. A numbered terminal
whose shell ends (`exit`) is removed with no question. `<prefix> n` / `<prefix> p` with the keys in a terminal go to
the next / the previous tab when it is in a tabbed or stacked container, otherwise to the next / the previous
terminal on screen, and do nothing with only one. Terminals have no text-size keys. `Alt+Enter` opens terminals only; a new chat view
comes from a split key then `a` with the keys in a chat view, or from `<prefix> !` (see [Chat views](#chat-views)).

### Chat views

A **chat view** is another page of the agent panel, shown as a module of its own and holding one session tab. The
agent panel you start with is the **base view** (`agent`); the others are `agent#2` to `agent#9`, up to nine in all
(a tenth is refused with `at most 9 chat views`). Every view is a module like the terminals are: it sits in the
layout, can be moved, zoomed, floated or hidden, is in every workspace, and has its own chip (`agent 2`, or
`agent 2 ⚑1` while a card waits in it). Their tabs belong to the window, so they keep running when their view is
hidden, and every view spends the window's one [account](configuration.md#accounts).

- **`<prefix> !` breaks a tab out.** With the keys in the base view, the tab you are on moves into a new view to the
  right of the base view, and the view takes the keys. With only one tab in the base view it refuses (the toast reads
  `can't break with only one tab`): the base view always keeps a tab. With the keys in an extra view, `!` does the
  opposite: the view is shown no more, its tab returns to the base view as the tab you are on, and the keys go there.
  With the keys in the editor or a terminal it refuses (the toast reads `tab.break: the keys are in no chat view`). A `set` of `!` in your `init.lua` needs
  `k.del("prefix", "!")` before it, as for any default key (above).
- **A new view with an empty tab.** `eitri.layout.run("instance.new", { kind = "agent" })`, `eitri.layout.open("agent")`
  and a split key then `a` with the keys in a chat view each make a new view holding a new, empty tab of the default
  agent (its first message goes through the usual [trust question](permissions.md#trusting-a-project) in that view).
  Where it opens is up to your [open rules](configuration.md#where-a-new-module-opens); by default it is to the right
  of the module that has the keys.
- **`<prefix> a`** shows the chat you want: with the keys in a chat view it hides that view, as it hides the base view
  today; from anywhere else it goes to the view holding the oldest waiting card, else to the chat view you used last,
  else to the base view. A view has no key of its own by default; `eitri.keymap.set("prefix", "F4", "module.agent#2")`
  gives one the same behaviour for that view.
- **Closing.** `<prefix> x` in a view asks `kill-pane agent 2? closes tab 3 <name> (y/n)` and, on `y`, closes that
  tab and removes the view. A view that is left with no tab (you closed its tab, or joined it back) goes away by
  itself.

**Tab keys in an extra view.** A view holds one tab, so the tab keys read differently there:

| keys | what they do with the keys in an extra view |
|---|---|
| `,` rename, `i` details, `&` close, `Shift+Tab` mode | act on this view's tab; `&` also removes the now-empty view |
| `r`, `?`, `:` | reload this page only, show its keys, open its command line (a `:` command acts on this view's tab) |
| `c` | a new tab in the base view, which takes the keys |
| `n`, `p`, `l` | step through the base view's tabs, as from the editor, without moving the keys |
| `1` ... `9` | the tab with that number wherever it is: a tab in an extra view gives that view the keys (shown first if hidden), a base-view tab is selected in the base view, which takes them |
| `w` | the chooser, in this page, listing every open tab (a tab in an extra view reads ` · agent 2` after its name); a choice acts as its digit |
| `<leader>b o` | flashes: there is no other tab here to close |
| `!` | join the tab back (above) |

The base view's tab strip lists only its own tabs, so a tab shown in another view leaves a gap in its numbers, as a
closed tab does. A view with one tab draws no strip. A restored session comes back in the view it was in when the
layout still has that view, otherwise in the base view (see [Agents](agents.md#chat-views)).

### Floats, the scratchpad and workspaces

A **float** is one module drawn over the others at a size and place of its own, with a bar across its top edge
that holds its title (as the top bar's chips read it, drawn exactly as written) and a small grip at its bottom-right corner. The
editor never floats; the agent panel can. A **scratch** float is a float you can put away and call back: while
it is put away it takes no room, its chip in the top bar reads `<title> ⌁`, and it keeps running. A
**workspace** is a whole arrangement of the same modules under a name; the window can hold several and one is
shown at a time.

| keys | what they do |
|---|---|
| `<prefix> F` (or `Alt+Shift+Space`) | float this module at the size and place it last floated at, else centred at 60% of the window; with the keys on a float, put it back into the layout next to the tiled module you used last. The editor and the last tiled module in view are refused |
| `<prefix> _` (or `Alt+_`) | send this module to the scratchpad: it floats and is put away |
| `<prefix> S` (or `Alt+-`) | the scratchpad: put away the shown scratch float that has the keys; else raise a shown scratch float; else show the one put away longest. Repeated presses cycle through them. An empty scratchpad flashes and says how to fill it |
| `<prefix> Alt+H`, `Alt+J`, `Alt+K`, `Alt+L` | with the keys on a float: move it five cells that way, inside the window (repeats) |
| `<prefix> Ctrl+Left` ... `Ctrl+Down`, `Alt+Left` ... `Alt+Down` | with the keys on a float: resize it one / five cells with its centre staying put: `Right` and `Down` grow it, `Left` and `Up` shrink it, never below the module's minimum and never beyond the window (repeats) |
| `<prefix> H` / `J` / `K` | with the keys on a float: dock it as a tiled module at the far left / bottom / top edge |
| `Ctrl+h`, `Ctrl+j`, `Ctrl+k`, `Ctrl+l` | from a float: to the float on that side if one is nearest that way, else to the tiled module on that side, else to the tiled module under the float's middle. A float never leads to the top bar |
| `<prefix> (` / `<prefix> )` | the previous / the next workspace, wrapping |
| `<prefix> L` | the workspace you were on before (tmux's `switch-client -l`); it flashes when there is none, or when that one was deleted |
| `<prefix> s` | choose a workspace (below) |
| `<prefix> $` | rename this workspace |

Click or drag a float's bar to raise it and move it; drag its grip to resize it. Giving a float the keys (a click,
`Ctrl+h` and its kin, a HINT label, its chip) raises it above the other floats. The rectangle a float had is
remembered when it sinks, for the next time it floats, and kept in the layout file.

**What a float does not have.** It has no neighbours in a split, so the keys that arrange the tiled tree either
leave the floats alone or refuse. The presets (`Alt+1` ... `Alt+5`, `Space`), the flips, rotate and `layout.undo`
act on the tiled modules only. `E`, tabbed and stacked, `Alt+e`, `Alt+a`, `Alt+d` and the swaps refuse with the
keys on a float, and the toast reads `a float has no split: <prefix> F sinks it`. A split key with the
keys on a float opens its module beside the tiled module you used last; a split key that names a floating module
moves it out of the floats into the tree there. Zooming a tiled module takes every float off the screen (each one
keeps a chip in the top bar); zooming a float shows it alone.

**The scratchpad as a drop-down.** `scratchpad.show` takes a module, which makes it the one key for one module,
created on first use when it is a terminal:

```lua
local k = eitri.keymap
k.set("prefix", "F2", "scratchpad.show", { module = "terminal#2" })   -- a drop-down terminal on F2
```

The first press makes `terminal#2` as `<prefix> Alt+Enter` would, but straight into the scratchpad, shown and with
the keys; the next one puts it away; a third calls it back. For a module that exists and is not a scratch float,
the press sends it there and shows it. For a chat view that does not exist, `module = "agent#3"` makes it the same way: a float holding a new empty tab,
put into the scratchpad and shown; a number that is taken, or a tenth view, is refused.

**Workspaces.** `<prefix> s` (or a click on the workspace's name in the top bar, which shows only while there are
two or more) draws the chooser in the prefix's hint line and waits for one key:

```
1 main · 2 study · c new · x delete
```

`1` ... `9` go to that workspace in order, `0` to the tenth; `c` makes a copy of the arrangement you are in
under the next free number (`2`, `3`, ...) and goes to it; `x` then a digit deletes that workspace. `main` is never
deleted or renamed, and neither is the one you are in. `$` opens a field in the top bar holding the name: `Enter`
renames, `Esc` leaves it. A switch gives the keys to the module the workspace last had them on (when it is shown
there), and keeps the other workspace exactly as it was, zoom included. The agent panel is the same module in every
workspace, and so are the terminals: a new terminal comes into the other workspaces hidden below the editor.

**Actions with no key.** `focus.mode-toggle` gives the keys to the module you used last on the other layer (a float
from a tile, a tile from a float; `Alt+Space` is GNOME's, so it has no key). `workspace.new` makes and enters a copy under the next
free number, and `workspace.switch` takes a `name` (`eitri.keymap.set("prefix", "F3", "workspace.switch", { name =
"study" })`), making the workspace first when none has that name. Neither has a key; bind them as above.

**The `del` each new default needs.** `F`, `S`, `_`, `W`, `Alt+Shift+Space`, `Alt+-`, `Alt+_`, `(`, `)`, `L`, `s` and `$` are
defaults now, and so is `!` (`tab.break`, tmux's `break-pane`), so an `init.lua` that already sets any of them stops at
startup until it puts `k.del("prefix", "<key>")` before that `set` (the spelling is the one `eitri.keymap` takes:
`F`, `S`, `_`, `W`, `M-S-Space`, `M--`, `M-_`, `(`, `)`, `L`, `s`, `$`, `!`; for `!` that is `k.del("prefix", "!")`). The rule is the one under "Setting a key that already has an action" above,
tmux import included.

### The hint panel

`eitri.config.set("keymap.which_key_delay", 200)` makes Eitri show the keys that may come next once the prefix has
been armed for that many milliseconds without a second key. The value is a whole number from 0 to 5000, or `false`
or `"off"` (the default, which shows nothing). `0` shows the panel at once; with `200`, someone who presses `Ctrl+b z`
in one movement never sees it.

The panel appears near the bottom of the window, over the modules, centred, and takes at most 90% of the window's
width and 45% of its height. It groups the keys as Tabs, Modules, Layout, Workspaces (the floats are there too),
Windows and Other, with a short name for each (`zoom`, `split right…`, `next tab`), and a plugin panel's own module key gets a row with
its title. A key whose action this window refuses is left out, as it is in the keys sheet. Groups that do not fit
across are left out, and a group too tall for the height ends in a line like `… 5 more`; `<prefix> ?` lists every
key. After a split key (`%`
or `"`) it shows only the module keys that may follow, under the heading the hint strip already uses.

It never takes a key and never takes a pointer event: the next key goes to the prefix exactly as without the panel.
It goes away when the wait ends (any key, `Esc`, a click, or the window losing focus). A repeating key's wait, the
digit waits and the workspace chooser show their own displays and no panel. The hint strip stays as it was.

Two gaps: a [companion window](companion.md) does not draw the panel (with the setting on it logs that once, and
the strip and the keys sheet work as before), and neither does the Mac host. The panel's text is English, like the strip and the
sheet.

### Choosing Ctrl+Space as the prefix

The default stays `Ctrl+b`, which is stock tmux's, and what this guide and the README teach (`:EitriTutor` says whichever prefix you have). If you
want `Ctrl+Space`, set it like any other prefix, and move the "send it through" key with it:

```lua
local k = eitri.keymap
k.prefix("C-Space")
k.del("prefix", "C-b")
k.set("prefix", "C-Space", "send-prefix")
```

Check three things first. fcitx5's default trigger is `Ctrl+Space`, so with fcitx5 running the key may never reach
Eitri (the conflict warnings under [Keys without the prefix](#keys-without-the-prefix) cover root keys, not the
prefix: press it and see whether the strip appears). macOS switches input sources with it.
And nvim-cmp, blink.cmp and kickstart.nvim map `<C-Space>` in Insert mode for completion: with `Ctrl+Space` as the
prefix you reach that mapping by pressing the key twice, as `Ctrl+b Ctrl+b` does today. `send-prefix` (and
`send-keys`) deliver every chord to the bottom terminal too, `Ctrl+Space` included, where the shell sees it as
`Ctrl+@` (NUL).

The [first-start screen](configuration.md#first-start) offers `Ctrl+Space` among the prefixes and does the first
check for you: its press test asks for the key once and says whether it reached Eitri, and warns beforehand when an
input method is running. If it did not arrive, an input method or your desktop took it; choose another prefix, or
write it anyway if you mean to change that. A tmux configuration with `set -g prefix C-space` is read like any
other prefix.

## Moving between panes

`Ctrl+h`, `Ctrl+j`, `Ctrl+k` and `Ctrl+l` move the keys to the module on the left, below, above and right,
the way vim-tmux-navigator does. They have no prefix, and they are Eitri's in every module, the bottom
terminal included (the terminal never receives them; `<prefix> Ctrl+l` sends the chord to it if a program
needs it).

**From the editor.** Neovim goes first: the key moves between Neovim's own windows, and only when there is no
window left that way does it cross into the neighbouring module. This works with vim-tmux-navigator
installed. Without it Eitri gives these four keys the same behaviour itself, in Normal and Visual mode (in
Insert mode only `Ctrl+l`, since `Ctrl+h`, `Ctrl+j` and `Ctrl+k` keep their Insert meanings), but only on a key
your configuration has not mapped to something else: a mapping of your own is left alone.

**From the agent panel.** `Ctrl+h` and `Ctrl+l` leave the panel to the module on that side. `Ctrl+k` and
`Ctrl+j` depend on the panel's mode (below): `Ctrl+j` in BROWSE starts typing, in the box (INSERT) it leaves
the panel to the module below; `Ctrl+k` in INSERT goes back to BROWSE, and in BROWSE it leaves the panel to
the module above. From the topmost module, `Ctrl+k` goes on to the top bar: `h` and `l` move along it,
`Enter` activates an item, and `Ctrl+j` or `Esc` return the keys to where they were.

In BROWSE, vim's `Ctrl+w h`, `Ctrl+w j`, `Ctrl+w k` and `Ctrl+w l` do the same moves; `Ctrl+w j` is always the
module below, never the box.

**Navigator plugins in the editor.** What you need depends on the plugin. vim-tmux-navigator and Navigator.nvim work
as they are: Eitri answers the `select-pane` call they make, so `Ctrl+l` at Neovim's right edge reaches the terminal
or panel on that side. smart-splits.nvim (AstroNvim's window-navigation plugin) needs a hook. Without one it does not move the keys out of Neovim (it
switches its multiplexer support off when Neovim is a GUI, and the editor here is), so hand the move to Eitri from
`at_edge`:

```lua
require("smart-splits").setup({
  at_edge = function(ctx)
    require("eitri").edge(ctx.direction)
  end,
})
```

`require("eitri").edge` comes from [eitri.nvim](companion.md#the-eitrinvim-plugin), which has to be on Neovim's
`runtimepath`. It takes the direction words smart-splits passes (`left`, `down`, `up`, `right`), returns `true` when
Eitri took the move, and `false` for anything else (and when Eitri is not there), sending nothing. The same snippet
works in a [companion](companion.md#navigator-plugins-and-tmux) setup, where the window manager takes the move. The one
smart-splits case that needs no hook is version 3 with its separate tmux backend plugin, which moves the keys itself
(the hook is configured and never called). Eitri never edits your plugin configuration.

## From Neovim: `:Eitri`

In the Neovim that Eitri's own window embeds, `:Eitri` runs the window and layout actions, so a key you map in Neovim
can do what `<prefix> z` does. It exists only in that window's Neovim: not in a companion's Neovim, not on the Mac, and
not in a second window, which has no Neovim.

| command | what it does |
|---|---|
| `:Eitri plugin load <name>` | load a plugin page into this window for this run ([Plugins](configuration.md#plugins)) |
| `:Eitri <action> [key=value ...]` | any action `eitri.layout.run` takes, by its `eitri.keymap` name: `:Eitri zoom`, `:Eitri resize.left cells=5`, `:Eitri module.agent`, `:Eitri layout.even-horizontal`, `:Eitri float.toggle` |
| `:Eitri split right` or `below <module>` | open a module beside the editor, or move it there; `<module>` is `editor`, `agent`, `terminal`, `terminal#2`, `agent#3` or `lua:<panel id>` |
| `:Eitri focus`, `show` or `hide <module>` | as `eitri.layout.focus`, `show` and `hide` |
| `:Eitri open terminal` or `agent` | a new terminal or chat view, placed by the usual rules |
| `:Eitri workspace <name>` | switch to the workspace, making it when there is none |
| `:Eitri keys` | the keys sheet |
| `:Eitri setup` | the [first-start screen](configuration.md#first-start), to choose a key setup again |

With [compositor windows](compositor-windows.md#keys), `workspace`, `open terminal` and `split` of the editor or the
terminal are off, as their keys are.

Completion goes by position: the first word offers these words and the actions, `split` offers `right` and `below`,
`open` offers `terminal` and `agent`, and a module's place offers the module ids, every numbered instance among them
whether it is open or not. A first word that is neither these words nor an action is a Neovim error and nothing is
sent. Anything else the window refuses (an action without an option it needs, a module that is not open, a panel key
such as `tab.new`) shows as a toast in the window with the reason, and Neovim hears nothing back.

**What it leaves out.** The agent panel's own keys (`c`, `n`, `p`, `1`-`9`, `,` after the prefix, plus `r`, `:`, copy
mode and text size) are not layout actions, and from Neovim the module holding the keys is always the editor, so most
would do nothing. They stay on the prefix, and `:Eitri keys` lists them. The leader group below leaves them out too.

**Binding at config time.** The command is defined just after Neovim starts (never over a `:Eitri` of your own, and it
says in the log when it had to leave yours), so `vim.g.eitri` is not set while your config is being read. A mapping that
calls `<cmd>Eitri zoom<cr>` needs no check, since it resolves when you press it. To bind only where Eitri is, wait for
the `User EitriReady` event, which fires once after `:Eitri` exists and `vim.g.eitri` is `1`; outside Eitri neither
happens:

```lua
vim.keymap.set("n", "<leader>z", "<cmd>Eitri zoom<cr>", { desc = "Eitri: zoom" })

vim.api.nvim_create_autocmd("User", {
  pattern = "EitriReady",
  callback = function()
    vim.keymap.set("n", "<leader>ez", "<cmd>Eitri zoom<cr>", { desc = "Eitri: zoom" })
  end,
})
```

From Lua, `require("eitri").run("zoom")` (eitri.nvim) does the same and returns `false` when Eitri is not there.

**A leader group.** `eitri.config.set("keymap.nvim_prefix", "<leader>W")` maps, in Normal mode under that left-hand
side, the layout keys of your effective prefix table: `<leader>Wz` zooms, and each split key becomes a group
(`<leader>W%t` puts a terminal to the right, `<leader>W"e` an editor below). The descriptions read `Eitri: zoom`, so
which-key lists them under one group, "Eitri window". What you have learned after the prefix is what you press after
`<leader>W`, and tmux imports and `init.lua` changes carry over, because the group is built from that table. The default
is `"off"`. The value is a Neovim left-hand side of at most 16 characters, `<leader>` and `<localleader>` allowed.

If something is already mapped at that left-hand side or under it (LazyVim maps `<leader>w` as its window group), Eitri
installs none of the group and says so once, naming what is in the way, so choose another. Eitri looks again when
your plugin manager finishes loading. A plugin that maps one of Eitri's keys later wins that key, and Eitri does not
put its own back; the rest of the group stays.

### Editing the selection from Neovim

In Visual mode, `<leader>ae` (your `mapleader`; `\` when you have not set one) puts the keys in the
agent panel with the lines you selected attached and `Edit the selection: ` already in the box, the caret after it. Type what you
want changed and press `Enter`: that is an ordinary message, so the edit comes back as the agent's usual edit card, and turn
review shows it afterwards. Nothing is written into your buffer by Eitri. The selection stays selected in Neovim, and the box
keeps whatever you had already typed instead of the prompt. The message goes to the chat tab the panel shows (`prefix a` finds
the chat view that has the keys, else the one you used last, and brings it back if it was hidden), and the file and lines are
named in the status line under the box before you send. The key is put in only where nothing of yours maps it, and Neovim itself maps
nothing under `<leader>`. For a key of your own, `vim.keymap.set("x", "<leader>k", function() require("eitri").edit_selection() end)`
does the same and `edit_selection()` returns `false` when no Eitri window or panel is attached.

### Keep or undo live drawings

When `review.live` is on (see [Configuration](configuration.md)), a file the agent edits is drawn in your editor as it
changes, and these keys work in that buffer while a drawing is on it: `<localleader>a` (or `:EitriReviewKeep`) keeps all
the changes in this buffer, `<localleader>d` (or `:EitriReviewUndo`) undoes all of them in the buffer as one step that `u`
brings back, `]h` and `[h` move between hunks, `<localleader>r` reverts one, `<localleader>q` closes the drawing and
`:EitriReviewClear` takes every drawing off. `<localleader>` is your `maplocalleader` (`\` when you have not set one). A key
that something of yours already maps is left to it, and the commands still work. None of them writes a file. The full
account, including what is and is not drawn, is in [Live drawings](turn-review.md#live-drawings).

## Keys without the prefix

By default the only keys Eitri takes without the prefix are the ones in the table above. `eitri.keymap.set("root", ...)`
adds your own: Ctrl and Alt chords that run an action in any module, with no prefix in front. The table starts empty,
so nothing changes until you fill it.

```lua
local k = eitri.keymap
k.set("root", "M-z", "zoom")
k.set("root", "C-Space", "select.next")
-- k.del("root", "M-z") removes one again
```

- **The key must hold Ctrl or Alt** (`C-`, `M-`, `C-M-`, with `S-` allowed on a named key). A bare key, a bare function
  key or a Shift-only key would be taken from Neovim, your shells and the panel everywhere, so Eitri stops at startup
  and says so. Super chords are not offered.
- **Any action a prefix key can run**, except `send-prefix` and `send-keys`, and with no `repeatable` option (a held
  root key does not repeat; press it again). A split action waits for the module key, and `select.number` and the
  workspace actions wait for their digit or name, as after the prefix.
- **Collisions stop the launch, naming both sides:** the prefix itself; a built-in chord (`F11`, `Ctrl+h`, `Ctrl+j`,
  `Ctrl+k`, `Ctrl+l`); a key bound twice in the root table; an
  `eitri.command.register` keybinding; and a key that is also bound after the prefix, by default or in your
  `init.lua`. For the last one, `eitri.keymap.del("prefix", "<key>")` first, so that `<prefix> <key>` can send the key
  itself. A prefix binding that only came from your tmux import is dropped instead, as is a tmux prefix equal to a root
  key (the prefix falls back to `Ctrl+b`); Eitri logs one line for each. A prefix your `init.lua` set stays an error.
- **The text size chords are yours to take.** `C-=`, `C--`, `C-0` and `C-+` may be root keys, and they may stay bound
  after the prefix too, so the plain chord and the prefix before it can change places (the block below).
- **A held key runs once.** The auto-repeat is swallowed until you let go, and losing focus or a click clears it. A key
  bound to a text size action steps while held, as `Ctrl+=` does.
- **Where it works.** Wherever the prefix works: the window, a second window, the companion window and the Mac host.
  In the last two, a root key whose action that window refuses (a layout action in a companion window) is passed on to
  whatever has the keys instead of being eaten; a text size chord refused there keeps its built-in action.
- **The keys sheet lists them** under "Anywhere in the window", with ` (init.lua)` after the action. The hint panel
  does not, since they are not after the prefix.
- **tmux's `bind -n` is still skipped on import** (the sheet says so); set the key here instead.

**Swapping the two text size keys.** The actions are `text.larger`, `text.smaller` and `text.reset` for the pane with
the keys, and `text.all.larger`, `text.all.smaller` and `text.all.reset` for every module. To make the plain chord size
one pane and the prefix before it size everything:

```lua
local k = eitri.keymap
for key, change in pairs({ ["C-="] = "larger", ["C--"] = "smaller", ["C-0"] = "reset" }) do
  k.set("root", key, "text." .. change)
  k.del("prefix", key)
  k.set("prefix", key, "text.all." .. change, { repeatable = true })
end
```

`Ctrl++` and the keypad's `+` are `C-+`; add it to the table with `"larger"` to swap those too.

**Sending the chord through.** Neovim, programs in the terminal, the agent panel and plugin pages never see a root key.
When you want the module to have it, `<prefix> <key>` sends it: Neovim gets it as a key, the bottom terminal as the
bytes a typed key gives (`Ctrl+Space` is NUL, `Alt+z` is Escape then `z`). The agent panel and plugin pages cannot get
such a chord back: the panel reads only a literal `Ctrl+a`, and a page takes no literal key. The panel table you can
rebind holds no Ctrl or Alt chord, but the panel's fixed keys do, and no warning covers them: `Ctrl+a`, `Ctrl+c`,
`Ctrl+d`, `Ctrl+e`, `Ctrl+f`, `Ctrl+g`, `Ctrl+i`, `Ctrl+n`, `Ctrl+o`, `Ctrl+p`, `Ctrl+r`, `Ctrl+u`, `Ctrl+w`, `Ctrl+x`,
`Ctrl+y`, `Ctrl+Enter`, `Alt+Enter`, `Alt+n`, `Alt+p` and `F1` (the INSERT and BROWSE tables below). A root key on one of these stops it working in the
panel. Re-sending a chord to those pages is a later change.

**Warnings.** Something else may take a root key before Eitri sees it, so Eitri checks and warns, and never refuses
to start for it. It compares your root keys with:

- the default triggers of fcitx5 when it runs (it looks at `GTK_IM_MODULE`, `QT_IM_MODULE`, `XMODIFIERS` and the
  session bus, and never reads the input method's own configuration; IBus gives no warning, since its default trigger
  is `Super+Space`, which cannot be a root key);
- GNOME's live shortcut settings, only when the session is GNOME;
- the live binds of sway (its main configuration: binds in files it `include`s are not seen) and Hyprland.

Each conflict is one log line every launch, and a toast only when the set of conflicts differs from the last toast.
KDE and niri have no source here, and a conflict from those, from a sway include or from an input method you
reconfigured is invisible to Eitri: if a root key does nothing, look there. The check runs in Eitri's own windows: a
companion window and the Mac host do not run it.

## The panel's modes

The panel has two modes, shown at the left of its bottom band in the same block, colours and place as the
editor's own statusline shows its mode (lualine's mode section, when you use lualine).

- **INSERT** is the message box. Typing goes into it.
- **BROWSE** is the conversation. The keys are vim's: you move over rows, fold and copy them, and answer
  cards.

`Esc` leaves INSERT for BROWSE. `i` or `o` (the caret where you left it), `A` (at the end of the draft) or
`Ctrl+j` go from BROWSE to INSERT, and so does `j` or `Down` on the last row: where it can move no further, it
walks on into the box, with the caret where you left it. A long last row scrolls to its end first, and a held key
stops at the last row, so it never types `jjjj` into your draft. On the empty tab the box is the last stop, so the
same keys walk the rows and then enter it. A waiting card, a question, an open list and the search line keep `j`
for themselves. `i`, `o`, `A`, `f` and this `j` act only on a key that stands alone: if another key is pressed
within about a quarter of a second of it, it does nothing, so the first letters of a sentence typed right after a
reply arrives do not start HINT or open the box in the middle of a word. The one exception is vim's `Esc i`: right after
the `Esc` that left INSERT, `i`, `o` and `A` go back in at once. `Ctrl+[` is `Esc` everywhere in the panel.

A third state, **card mode**, takes the keys while you answer a waiting card. It is entered with `Alt-n` or `Enter` on the
card and left with `Esc` ([Answering a card](#answering-a-card)).

The band's right side names the agent's model, and the effort beside it when the agent has said what it is. On a
Claude tab a click on either opens the list of models or efforts, also while a turn runs: nothing is sent to Claude as
a message, and the band says when your choice takes effect. A Codex tab shows what Codex reports and offers no list.

### In INSERT

| keys | what they do |
|---|---|
| `Enter` | send; while a turn runs, put the message into that turn without stopping it (the agent reads it at its next step), or, when the tab's agent cannot take messages mid-turn, hold it until the turn ends |
| `Ctrl+Enter` | send now: interrupts a running turn, then sends what is held and this message |
| `Shift+Enter`, `Alt+Enter`, or a backslash then `Enter` | a new line (the backslash is replaced by it) |
| `Up` | from the first line: take the held messages back, then step through earlier prompts (a message already put into the turn stays) |
| `Down` | from the last line, once you are stepping through earlier prompts: one step forward again |
| `Ctrl+r` | search earlier prompts; `Enter` puts one in the box |
| `/` at the start | open the command menu while the box holds one line and the caret is in its first word: it lists the commands this tab accepts (Claude Code's own, or the ones an ACP agent reports; a Codex tab has none), typing narrows it, a space closes it |
| `Up` / `Down` / `Ctrl+n` / `Ctrl+p` | with the command menu open: move the selection (these do not step through earlier prompts while it shows) |
| `Tab` | with the command menu open: complete the selected command and a space; `Enter` does the same until the name is whole, then sends |
| `Ctrl+w` / `Ctrl+u` | delete a word / to the start of the line |
| `Ctrl+a` / `Ctrl+e` | move the caret to the start / end of the line it is on |
| `Ctrl+c` | interrupt a running turn; when idle, clear the box into the history |
| `Alt-n` / `Alt-p` | card mode for the next / previous card waiting in this tab, counting from the end of the conversation (see [Answering a card](#answering-a-card)) |
| `Ctrl+y` | nothing: cards are no longer answered from the box, and the band says `Ctrl+y: cards are on Alt-n now` |
| `Ctrl+x`, then `Enter` | send the draft after the turn (see below) |
| `Ctrl+g` | edit the draft in Neovim; `:wq` brings it back into the box in INSERT, caret at the end, ready to send (`Enter` sends), unless a card is waiting, which the keys land on as usual; `:q!` returns the same way with the draft unchanged unless you already saved it with `:w`, in which case the saved text comes back |
| `Ctrl+o` | the detailed view (in BROWSE the detailed view is `zi`, and `Ctrl+o` goes back through your jumps) |
| `Shift+Tab` | cycle the tab's mode: Manual, Auto, Bypass, then Manual again (an agent with no Manual mode: Auto and Bypass); entering Bypass asks first, only from Auto, and a `y` also approves the Claude Code questions already waiting in the tab (ones your ask rules caused too) and answers the later ones for you, see [When Claude Code itself asks](permissions.md#when-claude-code-itself-asks); it never answers a question Claude asks you ([Claude's questions](#claudes-questions)) |
| `Esc` | stop typing and go back to BROWSE; with the command menu open, it closes the menu first and keeps the text |
| `?` | the list of keys, from an empty box |
| `F1` | the list of keys, whatever the box holds |

**Send after the turn.** `Ctrl+x` with nothing selected starts a two-second chord, and the band shows `Ctrl+x -`. `Enter`
then sends the draft *after the turn*: the message is held above the box as `⧗ after the turn: <text>` and goes out
with the rest of the queue when the turn ends, or when `Ctrl+Enter` ends it. Nothing puts it into the running turn,
which is the difference from `Enter`. While it waits, messages you send with `Enter` still go into the turn, so this is
the one message that does not go out in the order you typed it. `Up` takes it back like any held message. With no turn
running it is sent like an ordinary message. Any other key after `Ctrl+x` ends the chord with `Ctrl+x: only Ctrl+x
Enter is bound (send after the turn)` and does nothing else; with text selected, `Ctrl+x` cuts it as in any text box.
The empty box says which keys do what while a turn runs: `Enter steers the running turn, Ctrl+x Enter waits for its
end…`. If your window prefix is `Ctrl+x`, the window takes the key first. The start screen's box has no chord.

On a Claude tab, seven slash words run Eitri's own actions when typed alone, in either the dashboard's
box or a conversation:

| word | what it does |
|---|---|
| `/help` | open the keys sheet (`F1`) |
| `/status` | show this tab's information (`tab.info`) |
| `/resume` | open the sessions chooser |
| `/memory` | open the project's `CLAUDE.md` in the editor; if missing, show `no such file: CLAUDE.md` |
| `/rewind` | keep the draft and show `/rewind is not in Eitri — c reviews and reverts this turn's edits` |
| `/tasks` (or `/bashes`) | open the [background tasks list](#the-background-tasks-list) |
| `/workflows` | open the background tasks list at its dynamic workflows |

The first four clear the draft. None of these sends a message to Claude. Other slash commands keep
their usual behavior: `/clear`, `/compact`, `/cost`, `/context`, `/usage`, `/model`, `/mcp`, `/agents`,
`/doctor` and `/output-style` show an effect; a bare `/model` or `/effort` opens a picker, also while a turn runs, and
`/model <name>` is sent as a message (in an ACP tab it sets the model the agent lists). `/login` and
`/config`, and `/resume` with arguments, need a full terminal and are refused with a message. Other
words, including prompt templates and your own commands, are sent as typed. `?` lists the working
CLI commands. In a Codex tab, all seven words are sent as text: there are no Claude command holdbacks or pickers. In a tab
of an agent you registered, `/model` and `/effort` choose from the options the agent lists (see
[agents](agents.md#model-and-effort)) and the other five are sent as text.

### In BROWSE

| keys | what they do |
|---|---|
| `j` / `k`, or `Down` / `Up` | the next / the previous row; a long row scrolls first; `j` on its own on the last row goes to the box |
| `h` / `l` | the previous / the next button in the row |
| `gg` / `G` | the first / the last row |
| `{` / `}` | the start of the previous / the next row; unlike `j` and `k`, they never scroll inside a long row first |
| `[[` / `]]` | the previous / the next prompt of yours |
| `]t` / `[t` | the next / the previous tool call |
| `]d` / `[d` | the next / the previous error |
| `]m` / `[m` | the next / the previous file edit that was applied |
| `]c` / `[c` | the next / the previous hunk of an edit's diff (opens the diff) |
| `Ctrl+o` / `Ctrl+i` | back / forward through the places you jumped from, in this tab |
| `Alt-n` / `Alt-p` | card mode for the next / the previous card waiting in this tab (see [Answering a card](#answering-a-card)) |
| `a`, `d`, `D`, `]p`, `[p` | nothing: `]p` and `[p` say that cards are on `Alt-n` / `Alt-p` now; `a`, `d` and `D` on a card's row (or a tool row a card waits on) say that cards are answered in card mode, and elsewhere that they are not a key |
| `Ctrl+d` / `Ctrl+u` | half a page down / up |
| `Ctrl+e` / `Ctrl+y` | one line down / up |
| `Ctrl+f` or `PageDown` / `PageUp` | a view down / up (`Ctrl+b` is the prefix, so `PageUp` is the way back) |
| `zt` / `zz` / `zb` | this row to the top / the middle / the bottom of the view |
| `Enter`, or `za` | show or hide a tool's result, or a collapsed run of calls; for the other effects of `Enter` see below |
| `zo` / `zc` | open / close that fold only |
| `zh` / `zl` | scroll this row's table left / right |
| `zi` | the detailed view: every result, longer excerpts, no collapsed runs (per tab); a folded output says `zi for all` (in the box, `Ctrl+o for all`) |
| `yy`, `Y` | copy the row (the message, command or path), or the code block HINT landed on |
| `y` and an object or motion, `gy` | copy part of a row, or several rows (see [Copying with `y`](#copying-with-y)) |
| `Ctrl+g` | open this row's whole text in a scratch buffer in Neovim; on a pending edit card for one file (a Claude `Edit`, `MultiEdit` or `Write`, a Codex change that adds one file, an ACP agent's edit of one file), the file as it is on disk beside the file as the edit would leave it, read-only, in a new Neovim tab page (`q` closes it; see [Reviewing a turn](turn-review.md#before-you-approve)); on a card of an agent's [proposal](editor-tools.md#proposals), the same view of one of its files, the next file at each press; any other card shows its text, as before |
| `gf` | open the file path on this row in the editor, including a card's or edit row's file headers. Several paths get HINT labels at the paths (two letters past 14); type a label to open, `Backspace` deletes a letter, `Esc` cancels. Paths not on screen are listed under the conversation. If the project root does not hold the path, look up the end of its name: one match opens, several offer labels, more than 14 lookup matches ask you to narrow it. A lookup that stops early never opens by itself: it offers the matches it found as labels and says the list may be incomplete; more than 14 matches still ask you to narrow it |
| `gd` | go to the definition of a name, or to the line a path cites (see [Go to a definition](#go-to-a-definition)) |
| `gx` | open this row's web link in your browser (several: pick by letter) |
| `/` | search the conversation forward; `Enter` keeps the match, `Esc` goes back |
| `?` | search the conversation backward |
| `n` / `N` | the next / the previous match in the direction you searched, wrapping |
| `c` | review what changed on disk during this tab's latest turn (see [Turn review](turn-review.md)) |
| `Ctrl+c` | interrupt the running turn (`Esc` never does) |
| `Shift+Tab` | cycle the tab's mode, as in the box and as `<leader>m` does; it works everywhere in the chat except the caret and Visual, the question box and card mode |
| `r` | start a new session in this tab, once its session has ended |
| `f` | HINT |
| `:` | the command line |
| `F1`, or `g?` | the list of keys; `?`, `Esc` or `q` closes it |

A count goes before many of these: `3j` is three rows, `3G` or `3gg` is row 3, `2]]` is two prompts, `3]t` is the third
tool call on, `2gt` is tab 2, `5 Ctrl+e` is five lines, and `3yy` copies three rows. `0` continues a count but does
not start one.

A key after `g`, `z`, `[`, `]`, `y` or `Ctrl+w` that completes none of the pairs above ends the sequence without
doing anything. So do `Esc`, `Shift+Tab` and entering card mode: they drop whatever was pending (a prefix, a count, a
`y`).

**`Enter`'s other effects.** On a waiting card's row, or on the tool row a card gates, a lone `Enter` (no key within
about a quarter of a second before it) enters card mode for that card. On the row of a waiting question it moves the
keys to the question box. On a tool row with a live background task it opens that task in the
[tasks list](#the-background-tasks-list), but not on a card row, where it enters card mode. Anywhere else it shows or
hides a result.

**Searching.** `/` searches forward and `?` backward from the cursor, as in vim: the match follows what you type, and the
search wraps. On a row taller than the view, the line holding the match is brought on screen, below the pinned prompt. `n` goes on in the direction you searched, `N` the other way. In the caret and in Visual, `/` and `?`
end the selection and search from the caret's row.

**Jumping by structure.** `]t` and `[t` step between tool calls (a collapsed run is one stop; the note names its first
call), `]d` and `[d` between errors (a tool call that failed or was denied, an error notice, a turn that ended in an
error), `]m` and `[m` between file edits that were applied (a Claude `Write`, `Edit`, `MultiEdit` or `NotebookEdit`, a
Codex file change, an ACP agent's edit, delete or move), and `]c` and `[c` between the hunks of an edit: `]c` opens the
inline diff of the edit under the cursor and scrolls to its next hunk, then goes on to the first hunk of the next edit
that has a diff. They stop at the ends of the conversation with a note such as `no more tool calls below`, and do not
wrap. `]p` and `[p`, which used to walk the waiting cards, now say that cards are on `Alt-n` and `Alt-p`.

**Jumps.** `gg`, `G`, accepting a search, `n`, `N`, `[[`, `]]`, the structure jumps, a HINT landing and `Alt-n` (from
the place card mode was entered) each remember the row you left and its position in it. `Ctrl+o` goes back through them
and `Ctrl+i` forward, as in vim: a tab keeps up to 100 while the panel runs, a new jump after going back drops the ones
ahead, and a row that is gone lands on the nearest one. Two places in one long row (its top, and its end after `G`) are
two jumps. With nothing older or newer the band says `no older jump` or
`no newer jump`. `Tab` is not `Ctrl+i` here. In BROWSE `Ctrl+o` no longer shows the detailed view: that is `zi`; the
box keeps `Ctrl+o` for it.

**The pinned prompt.** While you read back through a long turn, a dim line at the top of the conversation names the
prompt it answers: its text cut to one line, then `prompt 4/9`. It shows only while the conversation is not following
the newest row and the top row lies below a prompt of yours; it is hidden when that prompt is itself on screen, above
the first prompt, and when a turn the agent started on its own lies between the prompt and the top row (its prompt is
not on the page). A message you sent into a running turn belongs to that turn and is not counted. A click on the line
goes to that prompt's row. It takes no keys; `[[` from the top row walks the prompts, the ones read mid-turn too.

### Copying with `y`

`y` is an operator, as in vim: it copies nothing until you say what. After `y` a short list of what can follow appears
(as after `g`), and `Esc` cancels. Every copy goes to the clipboard and to the primary selection, so a middle-click pastes
it, and the band says `copied N chars`. Nothing waits after a copy: the key you press next is an ordinary key.

| keys | what is copied | where |
|---|---|---|
| `yy`, `Y` | the row, as `y` copied it before: the rendered text of a reply, the code block HINT landed on, else the command or path | any row |
| `yim` / `yam` | the message's Markdown source / with a first line `**You**` or `**Claude**` | a prompt or a reply |
| `yio` / `yao` | the tool's whole output, uncut / with the command line before it and `(ok)` or `(error)` after | a tool call |
| `yic` / `yac` | the tool's input: the command, path, query or URL / with the tool's name (`Bash: npm test`) | a tool call or a permission card |
| `` yi` `` / `` ya` ``, `gy` | the row's code block, code only / fenced with its language (`gy` is `` yi` ``) | a reply with a code block |
| `yif` / `yaf` | the path or URL / with `:line` | a row with a path or link |
| `yih` / `yah` | the edit's current hunk (the one `]c` last landed on) / with the file's path as a header line | an edit with a diff |
| `y` then a motion | the rows from the cursor's through the motion's target, each as `yy` gives it, with a blank line between; the cursor does not move. The motions are `j`, `k`, `{`, `}`, `G`, `gg`, `[[`, `]]`, with a count (`y2j` is three rows, `y3]]`; a count before `y` multiplies the one after it, so `2y3j` is `y6j`) | any |

With several code blocks, paths or hunks on the row, labels appear on them as for `gf`: type a label's letters,
`Backspace` deletes one, and any other key cancels. A copy on the wrong kind of row says what would work:
`io: the cursor is on a reply, not a tool call — try im`. Vim's own text objects (`iw`, `aw`, `ip`, `is` and so on) need a
caret: `iw: needs the caret — v, then move, then y`. In the caret and in Visual, `y` copies the selection as before.
The start screen keeps its single-key `y`.

### Go to a definition

`gd` goes to one place from where the cursor or the caret is. In BROWSE it works on the cursor's row (one path or
`path:line`; with several, pick by letter, as `gf`); with the caret on a name, or with a name selected in Visual, it uses
that name.

- **`path:line` or `path:line-line`** (`mesh.go:142`): the file opens at that line, as with `gf`. If the reply quotes a
  name beside the path and the line no longer holds it, the note says so (`mesh.go:142 no longer has serve_replay (the
  file changed since the reply)`), and the file still opens.
- **A path:** the file opens, as with `gf`.
- **A name** (`TestMeshFieldsOmittedWhenZero`): Eitri asks the language servers that are already attached to the project's
  open buffers. If the same reply cites a `path:line` whose line holds the name, it asks for the definition from
  there; otherwise it looks the name up in the workspace. One match jumps there; with several, the one in the cited file
  nearest the cited line wins, else you pick by letter. With no match, the cited `path:line` opens, else the band says
  why: `gd: no language server attached for this project (open one of its files)`, `gd: gopls has no symbol
  TestMeshFieldsOmittedWhenZero` or `gd: rust-analyzer is still indexing — try again`. A server that has just started may
  answer empty at first, so `gd` asks again for up to three seconds before it gives up. An answer that comes once you
  have moved on (you are typing in the box, card mode holds the keys, a float or a pick is open, another pane has the
  keys) opens nothing: the band names the place, `gd: src/lib.rs:12 — gd again to open`.

`gd` never opens a file to start a language server, so it needs one you have already used in this project. For
rust-analyzer only, a name it cannot find is searched as a definition (`fn`, `struct`, `enum`, `trait` and the like) in
the project's Rust code, comments and strings left out; a qualified name such as `Foo::new` takes only a `new` inside
`impl Foo`, `trait Foo` or module `Foo` (`Self::new`, `self::`, `super::` and `crate::` keep every `new`). A name may hold letters, digits and `. : _ $ -`, up to 200 bytes; anything else is not looked up.

### Tabs and the leader

`<leader>` is `Space`, unless your Neovim configuration sets `mapleader` to a single key the panel can use.
After it, a short list of what can follow appears. These keys work in BROWSE.

| keys | what they do |
|---|---|
| `H` / `L`, `[b` / `]b`, `gT` / `gt` | the previous / the next tab (`2gt` is tab 2) |
| `<leader>b b` | the tab you were on before |
| `<leader>b d` | close this tab, after a `y/n` question |
| `<leader>b o` | close every other tab, after a `y/n` question |
| `<leader>f n` | a new tab |
| `<leader>,` | switch tab |
| `<leader>/` | search the conversation |
| `<leader>i` | this tab's details |
| `<leader>t` | continue this conversation in a terminal (Claude tabs; a Codex or registered agent's tab says it is not available yet) |
| `<leader>m` | cycle the tab's mode (as `Shift+Tab`) |
| `<leader>M` | choose the model (as a click on the band's model), also while a turn runs: the running response finishes on the old model, and the next request uses the new one. Claude tabs, and a registered agent's tab from the models the agent lists; a Codex tab says it is not available. |
| `<leader>E` | choose the effort (as a click on the band's effort), also while a turn runs: it holds from the next request. Claude tabs, and a registered agent's tab from the effort levels the agent lists; a Codex tab says it is not available. |
| `<leader>k` | in a Claude tab, stop every background task after a `y/n` question that names them (`Stop 2 shells, 1 monitor? y/n`); the conversation goes on, and it works only while no turn runs. In a Codex tab, stop the commands it left running in the background, which ends its session (it can be resumed), after a `y/n` question |
| `Space e` (`<leader>e`) | toggle following the agent in the editor for this tab; off by default, never moves your cursor |
| `<leader>?` | the list of keys |

You can change them in [your configuration](configuration.md).

### The background tasks list

`/tasks`, `/bashes` or `/workflows` in a Claude tab opens the list of what Claude Code runs in the background: shells,
monitors, local agents and dynamic workflows, then the tasks that finished, and a folded row of Claude Code's own
system tasks. The list has the keys while it is open, and its last line names the main ones.

| keys | what they do |
|---|---|
| `j` / `k`, or `Down` / `Up` | the next / the previous task; in a task's detail, scroll |
| `Enter` | the task's detail: for a shell or a monitor its command and the end of its output, read again every second while it runs; for a workflow its phases and agents |
| `x` | stop the selected task at once, and only that one |
| `o` | open the whole output read-only in Neovim; past 1 MiB, its last 1 MiB, under a line saying how much comes before |
| `PageDown` or `Space` / `PageUp` | scroll a page down / up |
| `Home` / `End` | scroll to the top / the bottom |
| `q` or `Esc` | back from a task's detail; from the list, close it |

What the list shows is in [Codex and other agents](agents.md#background-tasks-and-tasks).

### Claude's questions

When Claude asks you a multiple-choice question, a question box replaces the message box until you answer it, and
these keys work there. In BROWSE the activity line reads `⚑ Claude has a question — i to answer`, and `i` goes back
to the box.

| keys | what they do |
|---|---|
| `1` ... `9` | pick that row and stay on the question; the rows after the options are `Type something` (your own answer) and `Chat about this`, whose number only moves the cursor to it |
| `j` / `k`, or `Down` / `Up` | move between the options |
| `Home` / `End` | the first / the last option |
| `Space` | pick the option under the cursor and stay on the question (in one that takes several answers, select or clear it); on `Chat about this`, `Submit answers` or `Cancel` it does nothing |
| `Enter` | in a question with one answer, pick the option under the cursor and move on: to the next question, to the review page, or, for a single question, send it; in one that takes several answers, select or clear the option, and on its `Next` or `Submit` move on or send; on `Type something`, start typing; on `Chat about this`, tell Claude you would rather talk it over; on the review page, `Submit answers` sends and `Cancel` cancels the question |
| `Tab` / `Shift+Tab`, `Right` / `Left`, or `l` / `h` | the next / the previous question; when there are several, a review page of your answers follows the last one |
| `Ctrl+Enter` | send every answer, or show which questions are still unanswered |
| `n` | add notes to an option that shows a preview |
| `Ctrl+g` | edit the answer you are typing under `Type something` in Neovim |
| `Ctrl+k` | back to the conversation; the question keeps waiting |
| `Up` | from the typed answer, back to the options |
| `Esc` | in a typed answer, back to the options; anywhere else, cancel the question, which stops the turn |
| `F1` | the list of keys |

A key held down before the box appeared does not pick anything, and a held key's repeat does not either. More on
questions in [Codex and other agents](agents.md#claudes-questions).

### The sessions list and the pickers

The sessions list (`<prefix> w`, or `/resume`) and the model and effort pickers (a bare `/model` or `/effort`,
`<leader>M`, `<leader>E`) own the keys while they are open.

| keys | what they do |
|---|---|
| `j` / `k`, `Down` / `Up`, or `Ctrl+n` / `Ctrl+p` | the next / the previous row |
| `gg` / `G` | the first / the last row (the sessions list only) |
| `Enter` | the sessions list: switch to the open tab, or resume the session; a picker: choose the row |
| `/` | the sessions list: filter by name or title. The arrows and `Ctrl+n` / `Ctrl+p` still move; `Enter` keeps the filter and goes back to the list, `Esc` clears it |
| `Ctrl+r` | the sessions list, on an open tab: rename it; `Enter` saves, `Esc` cancels |
| `x` | the sessions list, on an open tab: close it |
| `Shift+Tab` | the sessions list: cycle the mode of that tab, or of the session a resume starts |
| `Esc` or `q` | close it; a picker sends nothing |

### Selecting text

`v` in BROWSE starts a caret on the row under the cursor, `V` starts a line selection directly.

- In the caret: `h` `l` move by character, `j` `k` by screen line, `w` `b` `e` by word, `0` and `$` to the start
  and end of the line, `gg` and `G` to the first and last character of the conversation; a count goes in front of
  a motion. `v` or `V` starts a selection from there, and `Esc` goes back to BROWSE. `/` and `?` end the caret and search.
  `gd` looks up the name under the caret.
- In VISUAL (`v`, by character) and V-LINE (`V`, by line): the same motions extend the selection, `o` swaps which
  end moves, `v` and `V` switch between the two modes (a mode's own key goes back to the caret), `y` copies what
  is highlighted (to the clipboard and the primary selection) and returns to BROWSE, `>` quotes it into your message and starts typing, and `Esc` goes back
  to the caret.
- `gd` goes to the definition of the selected name. `Alt-n` ends the selection and enters card mode.
- `gf` opens the path under the caret, the VISUAL selection as one path, or the paths in V-LINE's selected
  lines. Several paths get HINT labels at their selected occurrences (two letters past 14); type a label to open,
  `Backspace` deletes a letter, and `Esc` cancels. Paths not on screen are listed under the conversation.
  Opening or choosing returns to BROWSE; text without a usable path keeps the current mode.
  As with `y`, a changed selection, or no selection left, ends the region with a flash.
- `Ctrl+e` and `Ctrl+y` scroll one line without moving the caret or the selection, and `Ctrl+c` still
  interrupts a running turn. Any other plain key ends the selection with a note naming it; other chords with
  `Ctrl` or `Alt` are ignored.

## Answering a card

A permission card waits for you in the conversation, next to the tool call it gates. You answer it in **card mode**:
the card holds the keys, and only a digit, or `Enter` on a button you chose, answers it. No letter answers a card, and
nothing in the box does.

**Entering card mode.** A card never takes it by itself. When one arrives the cursor goes to it, as before, and the
activity line above the box says `⚑ Bash needs approval — Enter or Alt-n: card mode, then 1-3` (`Alt-n: card mode,
then 1-3` when the cursor is elsewhere).

| from | keys | which card |
|---|---|---|
| BROWSE or the box | `Alt-n` / `Alt-p` | the next / the previous card waiting in this tab: the first after (before) the cursor, wrapping; from the box, counted from the end of the conversation. It also works from the caret and Visual (the selection ends first) and from the question box |
| BROWSE, the cursor on a waiting card or on the tool row it gates | `Enter`, on its own | that card |
| BROWSE | a HINT label on one of a card's buttons | that card |

Entering highlights no button. With no card waiting, `Alt-n` says `no card waiting`; with only a question waiting it points
there (`a question waits — j to it, Enter`), because questions have their own box and are not in the ring. `Alt-n` and
`Alt-p` cannot be rebound.

**In card mode** the card takes an accent ring, its buttons show their digits, and the band's mode block reads `CARD`
(`CD` when the panel is narrow). In a narrow panel a refused key's note takes the room of the mode's word and then of
`card 1/2`, so it is never cut away.

| keys | what they do |
|---|---|
| `1` `2` `3` | answer in Claude Code's order. Where the card offers a saved rule: **1 Approve, 2 Always allow, 3 Deny**; `2` is the keyboard way to save one ([Saved rules](permissions.md#saved-rules)). On every other card, an editor-tools, Codex or other agent's card included: **1 Approve, 2 Deny**. So Deny is always the last digit, as the buttons show it. A digit with no button says `4: this card has 2 answers` |
| `h` `l`, `Left` `Right` | move the highlight between the buttons; nothing is highlighted when you enter |
| `0` `^` / `$` | highlight the first / the last button |
| `Enter` | answer with the highlighted button, as its digit does. With nothing highlighted: `Enter: no button picked — 1-3, or h/l then Enter` |
| `Tab` | open the reason field (a Claude card only; the others say they take no reason, and which digit denies). `Enter` there denies with the reason; `Esc` or `Tab` closes it and keeps the text, which the Deny digit then sends too |
| `Esc`, `Ctrl+[`, `Ctrl+c` | leave card mode, the card still waiting. `Ctrl+c` interrupts nothing here |
| `Alt-n` / `Alt-p` | go to the next / the previous waiting card; this one keeps waiting |
| `j` `k`, `Ctrl+d` `Ctrl+u` | scroll a card taller than the view |
| `F1` | leave card mode and show the list of keys at this section |
| any other key | refused, and the band names the card's digits: `a: letters never answer a card — 1-3, or h/l then Enter; Esc leaves`. `Shift+Tab` says to answer or leave the card first; `Ctrl+g` says to leave card mode first |

**Armed, and the quarter second.** A digit or `Enter` answers only once card mode is *armed*: 300 ms after you entered,
after the card has been drawn, after an arrival in the card's tab, and after any key in card mode that did not answer.
The digits are dim until then and lit once armed. A digit pressed too early says `1: card arming — press again`. Even
then the answer waits a quarter of a second, and any other key that arrives in that time cancels it
(`answer cancelled: another key followed`) and re-arms card mode. So `1. do x` typed into card mode answers nothing.
A held key's repeat, a key replayed after the panel lost the keys, and a digit with a modifier held (`Shift+1` says
`digits answer without Shift`) never answer. The answer goes to the card that held card mode, and only while it is
still waiting; if what the card offers changes (a saved rule appears, goes or is replaced), card mode ends with `card
changed — Alt-n or Enter again`.

**Leaving.** After an answer you are back where you came from: the box, with the caret where it was, or BROWSE on the
row you left. It never moves on to the next card; when more wait, the band says `2 more waiting — Alt-n`. Card mode also
ends, the card unanswered, when the card is answered somewhere else (a click, your phone) or withdrawn, when you switch
tab, and when anything else takes the keys: a float over the panel, the list of keys, the review, a `y/n` question or a
pick. A `gd` or `gf` answer that arrives while card mode holds the keys opens nothing: the band says
`gd: press Esc to leave the card first` (or names the one place found), and after `Esc` you press `gd` or `gf` again.
A card of a session that has ended cannot be answered.

**The old keys.** `a`, `d` and `D` in BROWSE, `]p` and `[p`, and `Ctrl+y` in the box answer nothing. `]p`, `[p` and
`Ctrl+y` say where the card keys are now; `a`, `d` and `D` say so on a card's row or a tool row a card waits on, and
elsewhere say they are not a key. A card from an agent's [editor tools](editor-tools.md#cards) goes the same way. The mouse works as it
always did: a click on a button with the pointer answers; `Enter` or `Space` on a focused button does not. The `y/n`
questions Eitri asks (entering Bypass, stopping a turn, closing a tab) are not cards and keep `y` and `n`.

## HINT

`f` in BROWSE, or `<prefix> f` from anywhere, labels everything you can jump to: the top bar's items, the
editor, the terminal when it is shown, and in the panel every visible row, code block, link, button and the
message box. Type a label's letters (they come from `asdjklghweruio`) to go there.
`Backspace` deletes a letter and `Esc` cancels. Landing on a row moves the cursor to it; landing on a link
focuses it and `Enter` opens it, and landing on a code block lets `yy` copy it, and landing on one of a waiting card's buttons enters card mode for it. Pressing `f` again does
nothing, since `f` is never a label.

## The command line

`:` in BROWSE (or `<prefix> :` from anywhere) opens a command line at the bottom of the panel. It is not
vim's: it takes five commands, so that `:ls` or `:d` typed from habit land here and never on a card. `Enter`
runs it and `Esc` closes it; anything else shows a note saying it is not a command here and that `F1` lists the keys.

| command | what it does |
|---|---|
| `:tabnew` | open a new tab of the default agent, as `<prefix> c` does. `:tabnew codex`, or `:tabnew` and the name of an agent registered in `init.lua`, opens a tab that runs that agent. Any other word is refused with the list of accepted ones |
| `:trust` | check this project's own configuration for the agent of the tab on screen (for Claude its `.claude/`, `.mcp.json` and `CLAUDE.md`) and ask whether to trust it; sessions started afterwards load it. It says so when there is nothing to trust or it is already trusted |
| `:untrust` | forget that trust; sessions started afterwards leave the project's configuration out. Running sessions are unchanged |
| `:scratch` | the row under the cursor, whole, read-only in Neovim, as `Ctrl+g` shows it |
| `:%scratch` | the tab's whole loaded conversation as one Markdown text, read-only, in a new Neovim tab page (`q` closes it) |

`:%scratch` writes a snapshot: run it again for a fresh one. Each prompt is a `## ▍You · prompt 4/9` heading, replies are
as written, tool calls are fenced blocks (`Bash: npm test`, then the full output), paths read `path:line`, cards and
endings take a line each, and a subagent's work is quoted under its call. It is built from the whole conversation, not
the folded view, so nothing is collapsed. A tab that loaded only the newest part of a long history says in its first line
how many turns it did not load. The text holds at most 1,000,000 characters; over that, the oldest turns are left out
whole and the first line says how many. Agent text that begins with the prompt marker has it changed, so no reply can
pass as one of your prompts.

What trusting means is in [Permissions](permissions.md), and the other agents are in
[Codex and other agents](agents.md).

## Your tmux keys

If you use tmux, Eitri picks up your prefix and your prefix-table bindings from the same files tmux
reads (`/etc/tmux.conf`, `~/.tmux.conf`, `$XDG_CONFIG_HOME/tmux/tmux.conf`,
`~/.config/tmux/tmux.conf`, and what they `source-file`), wherever an Eitri action matches the tmux
command. It only reads them; it never starts or asks tmux. A notice says what was taken (it appears
once for each change to what is imported), and `<prefix> ?` lists each line that was not, with the reason.
Things Eitri has no equivalent of, and anything that would need tmux to run something, are skipped.

An `unbind -a` in your tmux configuration removes Eitri's tmux-like defaults too, but Eitri's own keys (HINT,
the module keys, `?`, the panel reload, immersive and the tab keys tmux has no command for) stay.

To turn the import off, put `eitri.config.set("keymap.from_tmux", "off")` in your [`init.lua`](configuration.md#initlua)
(or use Lua `false`; `true` and `"on"` enable it, the default; any other value stops the launch, naming the key). Your own `eitri.keymap` calls in
that file always win over what came from tmux; for instance, with the import off, a `Ctrl+a` prefix is
`eitri.keymap.prefix("C-a")`. The prefix must be a `Ctrl` or `Alt` chord or a function key. The calls
themselves are in [Configuration](configuration.md).
