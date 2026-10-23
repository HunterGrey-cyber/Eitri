English | [简体中文](keys.zh-CN.md)

# Keys

Eitri's keys come from three places, and they do not collide: a **prefix** for the window itself (tabs,
modules, layout), a few keys that work **everywhere** in the window, and the **agent panel's own keys**,
which are vim's. Everything below is the default setup. `?` in the panel lists the keys of your own setup,
including anything you changed in [your configuration](configuration.md) or took over from tmux.

## The prefix

The prefix is stock tmux's: press `Ctrl+b`, let go, then press one key. It is read before the editor, the
panel and the bottom terminal see anything, so it works wherever the keys are. `Esc` cancels it, and a key the
table does not bind ends the wait without doing anything. To send `Ctrl+b` itself to whatever has the keys,
press it twice. `<prefix> ?` opens the full keys sheet at the "After <prefix>" table. While the prefix is armed,
the hint strip ends with `? keys`; it uses the effective key for `panel.keymap`, and leaves that run out if unbound.

The keys marked "repeats" can be pressed again within half a second without the prefix, as in tmux.

**Tabs.** Each conversation in the agent panel is a tab.

| keys | what they do |
|---|---|
| `<prefix> c` | a new tab |
| `<prefix> n` / `<prefix> p` | the next / the previous tab |
| `<prefix> 1` ... `9` | that tab by number |
| `<prefix> l` | the tab you were on before |
| `<prefix> ,` | rename this tab |
| `<prefix> &` | close this tab, after a `y/n` question |
| `<prefix> w` | choose a session: the open tabs first, then saved ones to resume |
| `<prefix> i` | this session's details |

**Modules.** The window is made of modules: the editor, the agent panel and the bottom terminal.

| keys | what they do |
|---|---|
| `<prefix> e` / `a` / `t` | show and focus the editor / the agent / the terminal, or hide it when it already has the keys |
| `<prefix> Left`, `Right`, `Up`, `Down` | move the keys to the module on that side (repeats) |
| `<prefix> ;` | move the keys back to the module that had them before |
| `<prefix> o` | move the keys to the next module on screen |
| `<prefix> z` | zoom this module, or restore it |
| `<prefix> %` or `<prefix> "`, then `e`, `a` or `t` | open that module to the right of / below this one, or move it there; with the keys in a terminal, `t` opens a new terminal there (see [Containers and terminals](#containers-and-terminals)) |
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
| `<prefix> x` | close this module and end what runs in it, after a `y/n` question; closing the last one on screen closes Eitri, and then the question, like closing the window, also names the chat turns running or queued and the terminals running a command (`kill-pane editor? closes Eitri, 2 terminal jobs (y/n)`); a job left in the background at a shell prompt is not counted |
| `<prefix> [` or `<prefix> PageUp` | scroll the bottom terminal back (copy mode); only while the terminal has the keys, elsewhere it flashes a refusal and the toast says `prefix [ scrolls the terminal` |
| `<prefix> F11` | immersive: fullscreen without the top bar |
| `<prefix> Ctrl+=` / `Ctrl+-` / `Ctrl+0` | text size larger / smaller / back in step, for the pane with the keys only: the editor, the chat or a plugin's page (repeats: keep Ctrl down and press `=` or `-` again) |

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

**Setting a key that already has an action.** The keys in this section are defaults, and `eitri.keymap.set` on a key
that has an action stops Eitri at startup with a message that names the call and says to run
`eitri.keymap.del("prefix", "<key>")` first. If your `init.lua` sets `m`, `M`, `q`, `E`, `H`, `J`, `K`, `Space`, `C-o`, `M-o`, `M-e`, `M-3`, `M-4`, `M-5`, `M-H`,
`M-J`, `M-K`, `M-L`, `M-w`, `M-s`, `M-a`, `M-d`, `M-Enter`, `M-S-Space`, `M--`, `M-_`, `(`, `)`, `L`, `s`, `$`, `!`, `C-=`, `C-+`, `C--` or `C-0` (the spelling `eitri.keymap` takes), put `k.del("prefix", "<key>")` before that `set`. When the tmux import is on and your
tmux configuration binds the key, the key already counts as yours and your `set` replaces it with no `del`. A `del`
of a key that nothing is bound to also stops startup, so a `del` line works only from this version on, or when the
import and your tmux configuration bind that key.

**The window and the panel.**

| keys | what they do |
|---|---|
| `<prefix> f` | HINT: jump anywhere in the window (see [HINT](#hint)) |
| `<prefix> r` | reload the agent panel; the session keeps running |
| `<prefix> ?` | the full list of keys, in the panel, starting at "After <prefix>" (from a plugin page, its own keys come first); the chat's own `?` starts at "This panel". Closing it with `?`, `Esc`, `q` or a click outside gives the keys back to the module that had them, and puts back a zoom or a hidden chat that opening it changed |
| `<prefix> :` | the panel's `:` command line (see [The command line](#the-command-line)) |
| `<prefix> Ctrl+h`, `Ctrl+j`, `Ctrl+k`, `Ctrl+l` | send that chord itself to whatever has the keys; the bare chords below are Eitri's |

**Without the prefix**, these work anywhere in the window:

| keys | what they do |
|---|---|
| `F11` | fullscreen |
| `Ctrl+=` / `Ctrl+-` / `Ctrl+0` | text size larger / smaller / reset, for the editor, the panel and every plugin page together (the keypad's `+`, `-` and `0` work too) |
| `Ctrl+wheel` | text size of the pane under the pointer only |

A plugin's page (the learning plugin, the Markdown preview) is zoomed whole, its layout with its text, and follows the
editor's size: `Ctrl+=` grows the editor and every page together, a page made larger or smaller on its own keeps that
difference until `Ctrl+0` or `<prefix> Ctrl+0` on it, and the page keeps its size when it reloads. The page never
sees these keys.

A companion window (see [Companion mode](companion.md)) has no layout of its own, so there the prefix keeps
the tab keys, `r`, `?`, `:`, `f`, the text size keys (`Ctrl+=`, `Ctrl+-`, `Ctrl+0`), sending the prefix itself
or a `Ctrl+h/j/k/l` chord, and the arrow keys, which move toward the neighbouring window; every layout key answers
`not in a companion window`.

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

If your i3 or sway `$mod` is Alt, the window manager takes those chords before Eitri sees them, and the rows never run. [`docs/keymap/i3-alt-mod.lua`](../keymap/i3-alt-mod.lua) moves the rows it grabs: each keeps its key with Ctrl added (`<prefix> Ctrl+Alt+w`, `Ctrl+Alt+Enter`, and so on). The rows whose key has Shift in it drop the Shift instead of keeping it: Eitri does not read Ctrl+Shift+<character> as a chord, so the four moves (`Alt+H`, `Alt+J`, `Alt+K`, `Alt+L`) become `<prefix> Ctrl+Alt+h`, `Ctrl+Alt+j`, `Ctrl+Alt+k`, `Ctrl+Alt+l`, and the move-to-scratchpad row (`Alt+_`) becomes `<prefix> _`; the float toggle (`Alt+Shift+Space`) becomes `<prefix> Ctrl+Alt+Space`, since Ctrl+Shift+Space switches the input method on many setups. Paste it into your `init.lua`.

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
| `<prefix> Alt+Shift+Space` | float this module at the size and place it last floated at, else centred at 60% of the window; with the keys on a float, put it back into the layout next to the tiled module you used last. The editor and the last tiled module in view are refused |
| `<prefix> Alt+_` | send this module to the scratchpad: it floats and is put away |
| `<prefix> Alt+-` | the scratchpad: put away the shown scratch float that has the keys; else raise a shown scratch float; else show the one put away longest. Repeated presses cycle through them. An empty scratchpad flashes and says how to fill it |
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
keys on a float, and the toast reads `a float has no split: <prefix> Alt+Shift+Space sinks it`. A split key with the
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

**The `del` each new default needs.** `Alt+Shift+Space`, `Alt+-`, `Alt+_`, `(`, `)`, `L`, `s` and `$` are
defaults now, and so is `!` (`tab.break`, tmux's `break-pane`), so an `init.lua` that already sets any of them stops at
startup until it puts `k.del("prefix", "<key>")` before that `set` (the spelling is the one `eitri.keymap` takes:
`M-S-Space`, `M--`, `M-_`, `(`, `)`, `L`, `s`, `$`, `!`; for `!` that is `k.del("prefix", "!")`). The rule is the one under "Setting a key that already has an action" above,
tmux import included.

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

## The panel's modes

The panel has two modes, shown at the left of its bottom band in the same block, colours and place as the
editor's own statusline shows its mode (lualine's mode section, when you use lualine).

- **INSERT** is the message box. Typing goes into it.
- **BROWSE** is the conversation. The keys are vim's: you move over rows, fold and copy them, and answer
  cards.

`Esc` leaves INSERT for BROWSE. `i` or `o` (the caret where you left it), `A` (at the end of the draft) or
`Ctrl+j` go from BROWSE to INSERT. `i`, `o`, `A` and `f` act only on a key that stands alone: if another key
is pressed within about a quarter of a second of it, it does nothing, so the first letters of a sentence typed
right after a reply arrives do not start HINT or open the box in the middle of a word.

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
| `Ctrl+y` | approve the oldest card waiting in this tab (see [Answering a card](#answering-a-card)) |
| `Ctrl+g` | edit the draft in Neovim; `:wq` brings it back; `:q!` changes nothing unless you already saved the draft with `:w`, in which case the saved text comes back |
| `Ctrl+o` | the detailed view (also in BROWSE) |
| `Shift+Tab` | cycle the tab's mode: Manual, Auto, Bypass, then Manual again (an agent with no Manual mode: Auto and Bypass); entering Bypass asks first, only from Auto, and a `y` also approves the Claude Code questions already waiting in the tab (ones your ask rules caused too) and answers the later ones for you, see [When Claude Code itself asks](permissions.md#when-claude-code-itself-asks) |
| `Esc` | stop typing and go back to BROWSE; with the command menu open, it closes the menu first and keeps the text |
| `?` | the list of keys, from an empty box |

On a Claude tab, five slash words run Eitri's own actions when typed alone, in either the dashboard's
box or a conversation:

| word | what it does |
|---|---|
| `/help` | open the `?` keys sheet |
| `/status` | show this tab's information (`tab.info`) |
| `/resume` | open the sessions chooser |
| `/memory` | open the project's `CLAUDE.md` in the editor; if missing, show `no such file: CLAUDE.md` |
| `/rewind` | keep the draft and show `/rewind is not in Eitri — c reviews and reverts this turn's edits` |

The first four clear the draft. None of these sends a message to Claude. Other slash commands keep
their usual behavior: `/clear`, `/compact`, `/cost`, `/context`, `/usage`, `/model`, `/mcp`, `/agents`,
`/doctor` and `/output-style` show an effect; a bare `/model` or `/effort` opens a picker. `/login` and
`/config`, and `/resume` with arguments, need a full terminal and are refused with a message. Other
words, including prompt templates and your own commands, are sent as typed. `?` lists the working
CLI commands. In a Codex tab or a tab of an agent you registered, all five words are sent as text:
there are no Claude command holdbacks or pickers.

### In BROWSE

| keys | what they do |
|---|---|
| `j` / `k`, or `Down` / `Up` | the next / the previous row; a long row scrolls first |
| `h` / `l` | the previous / the next button in the row |
| `gg` / `G` | the first / the last row |
| `[[` / `]]` | the previous / the next prompt of yours |
| `]p` / `[p` | the next / the previous card waiting for an answer (wraps; moves the cursor, answers nothing) |
| `Ctrl+d` / `Ctrl+u` | half a page down / up |
| `Ctrl+e` / `Ctrl+y` | one line down / up |
| `Ctrl+f` or `PageDown` / `PageUp` | a view down / up (`Ctrl+b` is the prefix, so `PageUp` is the way back) |
| `zt` / `zz` / `zb` | this row to the top / the middle / the bottom of the view |
| `Enter`, or `za` | show or hide a tool's result, or a collapsed run of calls |
| `zo` / `zc` | open / close that fold only |
| `zh` / `zl` | scroll this row's table left / right |
| `Ctrl+o` | the detailed view: every result, longer excerpts, no collapsed runs |
| `y` | copy the row (the message, command or path), or the code block HINT landed on; the copy also goes to the primary selection, so a middle-click pastes it too |
| `Y` | copy a tool row's whole output |
| `Ctrl+g` | open this row's whole text in a scratch buffer in Neovim; on a pending edit card for one file (a Claude `Edit`, `MultiEdit` or `Write`, a Codex change that adds one file, an ACP agent's edit of one file), the file as it is on disk beside the file as the edit would leave it, read-only, in a new Neovim tab page (`q` closes it; see [Reviewing a turn](turn-review.md#before-you-approve)); any other card shows its text, as before |
| `gf` | open the file path on this row in the editor, including a card's or edit row's file headers (several: pick by letter) |
| `gx` | open this row's web link in your browser (several: pick by letter) |
| `/` | search the conversation; `Enter` keeps the match, `Esc` goes back |
| `n` / `N` | the next / the previous match, wrapping |
| `c` | review what changed on disk during this tab's latest turn (see [Turn review](turn-review.md)) |
| `Ctrl+c` | interrupt the running turn (`Esc` never does) |
| `r` | start a new session in this tab, once its session has ended |
| `f` | HINT |
| `:` | the command line |
| `?` | the list of keys; `?`, `Esc` or `q` closes it |

A count goes before many of these: `3j` is three rows, `3G` or `3gg` is row 3, `2]]` is two prompts, `2gt`
is tab 2, `5 Ctrl+e` is five lines, and `]p` and `[p` go that many cards on. `0` continues a count but does
not start one.

A key after `g`, `z`, `[`, `]` or `Ctrl+w` that completes none of the pairs above ends the sequence without
doing anything, so `g` followed by `d` cannot deny a card.

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
| `<leader>k` | stop the commands a Codex tab left running in the background, which ends its session (it can be resumed), after a `y/n` question |
| `<leader>?` | the list of keys |

You can change them in [your configuration](configuration.md).

### Selecting text

`v` in BROWSE starts a caret on the row under the cursor, `V` starts a line selection directly.

- In the caret: `h` `l` move by character, `j` `k` by screen line, `w` `b` `e` by word, `0` and `$` to the start
  and end of the line, `gg` and `G` to the first and last character of the conversation; a count goes in front of
  a motion. `v` or `V` starts a selection from there, and `Esc` goes back to BROWSE.
- In VISUAL (`v`, by character) and V-LINE (`V`, by line): the same motions extend the selection, `o` swaps which
  end moves, `v` and `V` switch between the two modes (a mode's own key goes back to the caret), `y` copies what
  is highlighted (to the clipboard and the primary selection) and returns to BROWSE, `>` quotes it into your message and starts typing, and `Esc` goes back
  to the caret.
- `Ctrl+e` and `Ctrl+y` scroll one line without moving the caret or the selection, and `Ctrl+c` still
  interrupts a running turn. Any other plain key ends the selection with a note naming it; other chords with
  `Ctrl` or `Alt` are ignored.

## Answering a card

A permission card waits for you in the conversation, next to the tool call it gates. In BROWSE:

| keys | what they do |
|---|---|
| `a` | approve the card under the cursor, or the card that gates the tool call under it |
| `d` | deny it |
| `D` | deny with a reason: the keys go to the card's reason box, and `Enter` there sends the denial |
| `h` / `l`, then `Enter` | walk to one of the card's buttons and press it |
| `]p` / `[p` | move the cursor to the next / the previous waiting card |

From INSERT, `Ctrl+y` approves the oldest card waiting in this tab, and the band names the card it would
approve. There is no key that denies from the box, since a denial can carry a reason you want to type.

These keys answer only when they stand alone. `a`, `d`, `D` and `Enter` on a button do nothing if another key
was pressed within about a quarter of a second before or after; `Ctrl+y` is refused in the same way, right
after a readline kill key (`Ctrl+u`, `Ctrl+w`, `Ctrl+k`), and for a quarter of a second after the card it names
changed. The band says why when a key was refused. None of them takes a count, a held key's repeat never
answers, and a key with `Ctrl`, `Alt`, `Super` or `AltGr` held does not answer either (`D` is typed with
`Shift`). `Enter` on a button answers only right after `h`, `l` or `Tab` walked the focus onto it, or a HINT
landed on it. A card of a session that has ended cannot be answered.

## HINT

`f` in BROWSE, or `<prefix> f` from anywhere, labels everything you can jump to: the top bar's items, the
editor, the terminal when it is shown, and in the panel every visible row, code block, link, button and the
message box. Type a label's letters (they come from `asdjklghweruio`) to go there.
`Backspace` deletes a letter and `Esc` cancels. Landing on a row moves the cursor to it; landing on a link
focuses it and `Enter` opens it, and landing on a code block lets `y` copy it. Pressing `f` again does
nothing, since `f` is never a label.

## The command line

`:` in BROWSE (or `<prefix> :` from anywhere) opens a command line at the bottom of the panel. It is not
vim's: it takes three commands, so that `:ls` or `:d` typed from habit land here and never on a card. `Enter`
runs it and `Esc` closes it; anything else shows a note saying there are no other commands.

| command | what it does |
|---|---|
| `:tabnew` | open a new tab of the default agent, as `<prefix> c` does. `:tabnew codex`, or `:tabnew` and the name of an agent registered in `init.lua`, opens a tab that runs that agent. Any other word is refused with the list of accepted ones |
| `:trust` | check this project's own configuration for the agent of the tab on screen (for Claude its `.claude/`, `.mcp.json` and `CLAUDE.md`) and ask whether to trust it; sessions started afterwards load it. It says so when there is nothing to trust or it is already trusted |
| `:untrust` | forget that trust; sessions started afterwards leave the project's configuration out. Running sessions are unchanged |

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
