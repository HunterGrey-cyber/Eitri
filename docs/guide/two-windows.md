English | [简体中文](two-windows.zh-CN.md)

# Two windows

Eitri can open a second window, for a second monitor. It is a full Eitri window with its own top bar, workspaces,
floats, chats and terminals, and it has no editor: the editor, and the one Neovim behind it, stay in the first window.
You start it from the first window, move terminals, chats and pages between the two while they keep running, and each
window remembers the monitor it was on.

Nothing about this is on by default and no key is bound to it. Without a call in your `init.lua` or a command from the
panel, Eitri is one window, as before.

There is no second window on Hyprland, sway or niri unless you set `window.tiling` to `"eitri"`: there, chats and pages
are windows of your window manager by default, and its workspaces and outputs do this page's job. See
[Compositor windows](compositor-windows.md).

## Open the second window

Five verbs make up the feature. None has a default key, so bind the ones you want in
`$XDG_CONFIG_HOME/eitri/init.lua` (default `~/.config/eitri/init.lua`):

| verb | what it does |
|---|---|
| `window.new` | starts the second window, or brings it to the front when it is already open |
| `window.other` | brings the other window to the front and gives it the keys |
| `move.to-window` | moves a module to the other window; options `module` (default: the module that has the keys) and `follow` (default `false`: go to the other window too) |
| `workspace.to-window` | moves the whole current workspace to the other window; option `follow` |
| `workspace.swap-window` | swaps the current workspaces of the two windows |

```lua
local k = eitri.keymap
k.set("prefix", "F5", "window.new")
k.set("prefix", "F6", "window.other")
k.set("prefix", "F7", "move.to-window", { follow = true })
k.set("prefix", "F8", "workspace.to-window")
```

The keys are only examples; use any key that has no action yet ([Keys](keys.md) explains what setting a key that
already has an action needs, and [Configuration](configuration.md#keys) the `eitri.keymap` calls). `eitri.window` is
`"1"` in the first window and `"2"` in the second, if your `init.lua` should behave differently in one of them.

A project has one pair of windows. A second full Eitri window opened on the same project runs on its own, unlinked, and
`window.new` there says that the project's second window belongs to another Eitri window. The companion window
(`eitri panel`, `eitri split`) has none of these verbs.

## What the second window is

- **No editor.** Window 2 has no Neovim and no editor module. Everything that touches the editor goes to window 1,
  which raises itself and does it with its own Neovim: opening a file from a path click or `gf`, `Ctrl+g`, turn review,
  the context a prompt carries, the Markdown preview following the buffer, the colour scheme. They work in window 2 as
  they do in window 1. An agent's [editor tools](editor-tools.md#window-2) are the exception: they do not raise
  window 1, and from window 2 they reach only paths inside the project that follow no link.
- **A file from the command line.** [`eitri FILE`](getting-started.md#open-a-file), typed in either window's bottom
  terminal, opens the file in window 1's editor and raises window 1, since that is where the editor is. Reuse
  was tried with one window; it has not been run with the pair.
- **The editor key** in window 2 raises window 1 and gives its editor the keys. When window 1 is gone it asks
  `start the editor? (y/n)`; `y` starts a new Eitri window for the project, which links up to the waiting window 2.
- **Everything else is its own.** Each window has its own workspaces, layout, open rules, HINT, tray and toasts. A
  terminal or chat view you make in one window takes the lowest number neither window is using, so `terminal#2` is
  never in both. A module the other window holds is not in your layout; its key or its tray chip raises that window and
  gives it the keys instead of showing a second copy.
- **Each window shows what the other is doing.** Its top bar counts the other window's waiting permission cards,
  unseen turns, running turns and queued messages, whether or not a card is waiting. `workspace.choose` lists both
  windows' workspaces (the other one's prefixed `1:` or `2:`), and picking one raises that window. The chat chooser
  marks a session the other window holds as `in window 2`.
- **Moving between the windows with the keyboard.** `Ctrl+h`, `Ctrl+j` and `Ctrl+l` at the edge of a window cross to
  the other one when its monitor lies in that direction; `Ctrl+k` never crosses, because it reaches the top bar first.
  The key arrives on the opposite edge of the other window.
- **Turn review spans both.** A revert in either window waits for a turn running in the other, as it would for a turn
  in its own, so two windows never write to the same files at once.

Window 2 costs about 195 MiB of memory beside window 1 when it is empty (measured; each chat, terminal and page costs
what it costs in window 1).

## Moving things between the windows

`move.to-window` takes the module that has the keys, or the `module` you name (`"terminal#2"`, `"agent#3"`). The move
makes the new place first and lets go of the old one only then, so a move that cannot finish leaves the module where it
was.

- **A terminal** keeps its shell, its screen, its scrollback and whatever is running in it; a program writing a counter
  loses no line and repeats none. A float keeps its place relative to the window.
- **A chat** moves one tab, idle or in the middle of a turn, for Claude, Codex and other agents alike: the session, the
  running turn, a card still waiting for your answer, the queued message, the draft and the place you had scrolled to go
  with it. Its first frame in the other window is the moved view. `agent#N` moves its tab and the view; the base chat
  view moves its active tab into the other window's base view and stays where it is. The chooser's `Enter` on a session
  the other window holds moves it to you.
- **A plugin page** that declares it can be handed over (the Markdown preview does: its reading position and search go
  with it) keeps that, and the tasks it runs under Eitri's task guard move with it and keep running. A page that does
  not declare it, and a Lua panel, is opened again in the other window and starts from its own saved state.
- **A workspace** (`workspace.to-window`) moves every module it shows, in one go, except the editor, which stays; the
  other window switches to it once every module has arrived or been refused. A module that cannot move stays where it
  was and the toast says why. The source keeps a workspace that only partly moved, and a workspace that moved whole is
  gone from it (`main` always stays). `workspace.swap-window` does this both ways at once.
- **The editor never moves** in this version.

A page needs a moment to be drawn in the window it arrives in. It is shown there once it says it is ready, or after one
second at the latest; in that time neither window shows it, and you may see the place where it was stay empty for a
fraction of a second (about 0.06 to 0.3 s when measured in an isolated session). Its window will not let you move it
again until it has appeared.

A move is refused, with the reason in a toast, when there is no other window (`this window has no second window yet`),
when a terminal is in the middle of a long escape sequence, when a chat has a steer message the agent has not read yet,
is being handed to a terminal or is waiting on a restore, or runs on the legacy backend, and for the editor. A move
whose other window does not answer for 10 seconds says so once, and closing the window that does not answer gives the
module back. A moving module cannot be closed or moved again until it has settled.

Honest limits: a move freezes the thing for a moment (the aim is 50 ms for a terminal and 150 ms for a chat), and an
input-method composition or a visual selection in progress is dropped. A shell or an agent whose window ended
reports its exit without a status when it later ends. A turn an agent starts on its own, with no prompt from Eitri, is
unseen by the other window's revert check until Eitri has read its first event.

## When a window ends

- **Closing a window** asks the usual question and, with two windows, names the other window's running and queued
  work. `m` offers to move the running chats, the terminals with a job in the foreground and the pages with running
  tasks to the other window first, so closing a window does not end them; `y` ends them with the window.
- **Window 2 closes or crashes.** Window 1 stays alone and takes its placeholders back. Plugin pages and Lua panels
  window 2 held return **hidden** and load at their next show, so a window that ends does not rearrange the one that
  stays; the window's terminals and chat views are gone, and its chats are in its saved tabs, which the chooser offers.
  What a crash held ends with it.
- **Window 1 closes or crashes.** Window 2 stays, linked to nothing, without an editor: its editor calls answer
  `the editor window is closed`, and a standing note says how to start it again. What window 1 started and window
  2 holds (a shell, an agent) keeps running. A new Eitri window for the same project links up to it, and so does `y` on
  the editor key's question. After an upgrade of Eitri an old window 2 cannot link up to the new window 1; close it and
  start it again, moving its chats with `m` first.
- **A restore that fails** (window 2 does not start, or never answers within 30 seconds) brings everything it would have
  held back to window 1 exactly as it was.

## Monitors and restart

- Each window remembers its monitor. At the next start window 2 comes back, when it was open when the pair last ended,
  and both windows go back to their monitors; a window 1 that starts alone stays where the window manager puts it. A
  window 2 that `window.new` starts goes to the monitor it was last on; the first time, the window manager places it.
- On sway and Hyprland, a restored window 2 returns the keys to window 1 after mapping and placement, including when
  both windows use the same monitor. A window 2 opened with `window.new` receives the keys after moving. On GNOME,
  restoring window 2 can take the keys; returning them to window 1 automatically is not implemented. On niri, a
  restored window 2 is put back on its monitor, but no refocus is sent to window 1, so the keys are not handed back;
  niri has not been tried with the pair.
- **Set `eitri.config.set("window.restore", false)`** if window 2 should not come back by itself. Window 1 then takes
  back everything window 2 held and offers its saved chat tabs after its own. The default is `true`.
- Moving a window to its remembered monitor works on sway, Hyprland and niri, and on GNOME through the Eitri GNOME Shell
  extension, version 2 (see [Companion mode](companion.md#gnome-the-extension)). On GNOME with version 1 of the
  extension, or with none, window 2 opens wherever GNOME puts it.

## What has been tried

Two windows have been run in an isolated, headless sway session with two outputs, scripted keys and a stand-in model: the
moves, the keys across the edge, a window ending and coming back, a restart on remembered monitors and the memory
figure above. The Markdown preview and a moved chat were seen arriving. GNOME's token raise was tried in a probe, not
with the pair. On Hyprland 0.57 and 0.56.2 in a virtual machine only the keys at the edge were tried, on one output and
on two. niri, KDE and a real desktop with two physical monitors have not been tried; reports are welcome. The [known issues](../known-issues.md) list the rest.
