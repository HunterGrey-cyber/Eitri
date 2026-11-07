English | [简体中文](compositor-windows.zh-CN.md)

# Compositor windows

On Hyprland, sway and niri, Eitri leaves the tiling to your window manager. Each chat view and each page is a window of
its own, and your compositor's keys, rules, workspaces and outputs arrange them as they arrange any other window. The
editor stays in the first window, with one optional terminal below it.

This is the default on those three compositors. Everywhere else Eitri stays [one window](getting-started.md) unless you
set otherwise, and [two windows](two-windows.md) and the [companion window](companion.md) work as they always did.

## The setting

```lua
eitri.config.set("window.tiling", "auto")
```

| value | what it does |
|---|---|
| `"auto"` | the default: separate windows when the three checks below pass, otherwise everything inside Eitri's window |
| `"eitri"` | everything tiles inside Eitri's window (and window 2, if you open one) |
| `"compositor"` | chats and pages are separate windows on any Wayland desktop |

`auto` picks separate windows when all three of these hold:

1. The session is Wayland.
2. The environment names Hyprland, sway or niri (`HYPRLAND_INSTANCE_SIGNATURE`, `SWAYSOCK`, `NIRI_SOCKET`).
3. That compositor's socket exists and it answers `hyprctl version -j`, `swaymsg -t get_version` or
   `niri msg --json version` within 500 ms.

The third check is there because the variables can outlive the compositor: a tmux server keeps the signature of the
session that started it, and a relaunched sway leaves an old `SWAYSOCK`. The window never waits for the answer. If it
has not come in when the mode is read, that run opens inside Eitri's window. `companion.wm` does not change what `auto`
checks; it still picks the adapter that moves focus.

The setting is read once, at the start, so a change applies to the next window you open. Two notes tell you what
happened:

- The first start in separate windows under `auto` says so once:
  `Hyprland detected: chats and pages open as their own windows; window.tiling = "eitri" keeps them inside`. With the
  [first-start screen](configuration.md#first-start) up, the toast waits for it to close, and a file written with a
  `window.tiling` line drops it.
- A start that opens differently from the last one says why, once: `opened as tiles this time: Hyprland did not answer
  (window.tiling = "auto")` or `opened as separate windows this time (window.tiling = "auto")`. Your chats are in the same
  file either way, so a change of mode loses none of them.

`"compositor"` also works on GNOME and KDE, where `auto` stays inside Eitri's window. Their window managers place the
windows. On GNOME, focus moves need the [Eitri Shell extension](companion.md#gnome-the-extension); on KDE the edge keys
stop with the notice they give today. Neither has been tried. Outside Wayland the setting has no effect, and macOS has
no compositor mode.

## When it stays in one window

Three cases run inside Eitri's window whatever the setting says. Each writes one log line, and shows one toast where
the mode was in play (`"compositor"`, or `"auto"` on Hyprland, sway or niri), which reads
`Chats and pages stay inside this window: ` and then the reason:

| case | the reason it gives | why |
|---|---|---|
| a plugin's own window | `this project's page has a window of its own` | the page keeps its tile and the powers that window gives it |
| window 2 is still open, with no window 1 | `window 2 was left without its partner` | window 1 links up to it, and its workspaces need Eitri's tiling |
| the project is already open in another Eitri window | `this project is open in another Eitri window` | the project's windows belong to that one |

The reverse also happens: chat or page windows left behind by a window 1 that crashed make the next start use separate
windows for that run, whatever `window.tiling` says, and it links up to every one of them (`opened as separate windows
this time: chat windows were waiting to be relinked`).

## What opens

- **Window 1** opens with the editor alone. Its one bottom terminal stays hidden until its key shows it, and the same
  key hides it again. It has one workspace, and its top bar adds up the other windows' waiting cards and running turns (`chats: ⚑1 ●2`).
- **A chat window** holds one chat view with its own tab strip. There are up to nine, `c1` to `c9`.
- **A page window** holds one plugin page or Lua panel. A panel gets a window only if its id uses letters, digits, `.`,
  `_` and `-`, is at most 40 bytes long, and starts and ends with a letter or digit. Any other id opens nothing, and a
  toast names the rule.

Your start layout decides which windows open. Every chat and page that `eitri.layout.default` shows becomes a window, in
order, and a `terminal` it shows is shown below the editor: that is the one way window 1 starts with its terminal
open. With no layout of your own, the first launch opens `c1`. A chat
or page the layout hides stays closed until you press its key. Windows the start opens leave the keys in the editor; a
window you open with a key comes forward.

A new window opens wherever your compositor opens a new window, normally beside the one that has the focus. `eitri.layout`
calls in `init.lua` that place a chat or page (open rules, `split`, `float`) still succeed: the module opens in its own
window, and the placement is ignored with one log line.

Each window is its own process and loads your `init.lua`. `eitri.window` tells them apart: `"1"` in window 1, `"c3"` in a
chat window, `"p:markdown"` in a page window. A chat window costs 51.4 MiB more than the same chat as a tile with four
projects open, each with its window 1 and one chat window, and 62.6 MiB with one project (release build, medians).

Window 2 does not exist in this mode, since your compositor's workspaces and outputs do its job.

## App ids and titles

| window | app id | title |
|---|---|---|
| window 1 | `cn.huntergrey.eitri` | `<project> — Eitri` |
| chat window | `cn.huntergrey.eitri.Panel` | `<project> — Eitri chat`, and `<project> — Eitri chat 2` from `c2` on |
| page window | `cn.huntergrey.eitri.Page` | `<project> — <plugin name> · Eitri` |

Chat windows share the [companion panel's](companion.md) app id, so one rule covers both. Titles are set before a window
first maps and never change, because compositors read a rule's match when a window opens.

## Window rules

Rules go in your compositor's configuration, and Eitri never writes there. Each example matches one kind of window.

Hyprland with a Lua config (0.55 or later):

```lua
hl.window_rule({ match = { class = "^cn.huntergrey.eitri.Panel$" }, workspace = "2 silent" })
```

Hyprland with `hyprland.conf` (run on 0.56.2):

```
windowrule = match:class ^cn.huntergrey.eitri.Panel$, workspace 2 silent
```

sway, `for_window` for what to do with a window and `assign` for where it goes:

```
for_window [app_id="cn.huntergrey.eitri.Panel"] mark eitri-chat
assign [app_id="cn.huntergrey.eitri.Page"] workspace 2
```

niri:

```kdl
window-rule {
    match app-id=r#"^cn.huntergrey.eitri.Panel$"#
    default-column-width { fixed 700; }
}
```

**A placement from your rule wins.** Eitri cannot read which rule applied, so it looks at the result: a new window that
mapped on a different output or workspace from the window that had the focus was sent there by a rule. Eitri then neither
moves it nor focuses it, at that start or later when a remembered output comes back.

## Keys

The layout keys are off in every window, window 1 included. Nothing is passed on to the compositor, so `<prefix> z` does
not become a fullscreen. The first press of an off key in a window shows one toast, and later presses in that window
only write a log line:

`Hyprland arranges windows here: use its own keys. window.tiling = "eitri" brings Eitri's layout keys back.`

Off in every window: splits, moving and swapping modules, containers, layouts, zoom, resizing, marks, floats, the
scratchpad, workspaces, the `window.*` verbs of [two windows](two-windows.md) and `move.to-window`. No key is new in this
mode. What stands for such a key is off the same way, with the same toast: `:Eitri workspace <name>`, `:Eitri open
terminal`, `:Eitri split` of the editor or the terminal, and the matching `eitri.layout` calls.

In window 1:

| key | what it does |
|---|---|
| `Ctrl+h/j/k/l` | through nvim's splits, then the bottom terminal, then your compositor's focus move at the edge |
| `<prefix> e` | gives the editor the keys |
| `<prefix> t` | shows the bottom terminal and gives it the keys, or hides it, as in any window |
| `<prefix> a`, a page's key | raises that window, or starts it; `<prefix> a` goes first to a chat window waiting on you |
| the tab keys, `<prefix> !`, `r`, `?`, `:` | run in the chat window that last had the keys, which comes forward; with no chat window, `c1` starts and they run there |
| `<prefix> x` | quits the editor, or ends the terminal after a y/n |
| `<prefix> d` | hides the terminal; on a chat or page it only writes a log line, since the compositor closes those |
| `f`, the text keys, sending the prefix, copy mode, immersive | as usual |

In a chat or page window:

| key | what it does |
|---|---|
| `Ctrl+h`, `Ctrl+l` | your compositor's focus move |
| `Ctrl+k`, `Ctrl+j` | as in a [companion window](companion.md): `Ctrl+k` leaves from BROWSE and `Ctrl+j` from INSERT |
| `<prefix> e` | raises window 1 |
| `<prefix> a`, `<prefix> t`, a page's key | asks window 1 to raise or start that window |
| `<prefix> !` | in `c1`, sends the active tab to a new chat window; in any other chat window, sends it back to `c1` |
| `<prefix> x` | closes this window, as the compositor's close key does |
| the tab keys, `r`, `?`, `:`, `f`, the text keys, sending the prefix | as usual, in a chat window; a page has no chat, so its chat keys run in the last chat window |

In a page window the same keys leave at the edge the page reports. A chat or page window has no top bar, so the prefix strip floats
over its top right corner while the prefix waits. Copy mode, immersive and `<prefix> d` have nothing to act on there and
only write a log line.

**A new, empty chat window:** `<prefix> c` makes a new tab, and `<prefix> !` on a tab with no session yet opens the next
free chat window. The tab stays where it was. When all nine are open, the toast says so.

**Sending a tab to another window** ends it in one process and resumes it in the other, as a restart does. A tab in the
middle of a turn moves when the turn ends (`the tab moves to its own window when this turn ends`), and a second `<prefix> !`
calls that off. The tab arrives without the last turn's review. A chat window that loses its last tab closes; `c1` stays
open and empty.

## Closing

| what happens | its tabs and its place |
|---|---|
| you close window 1 (and answer `y` if it asks) | **kept**: every chat window comes back at the next start |
| you close a chat or page window with the compositor's key, or `<prefix> x` | **forgotten** after two seconds: the tabs and the place are dropped |
| every window closes at once, as a logout does | **kept**: each window comes back at the next start |
| a chat window with tabs still on offer closes before you answered the offer | **kept**: it reopens empty, offering them |
| an end signal (`SIGTERM`) closes a window | **kept** |
| a window crashes | **kept**, and its key starts it again |

Closing window 1 closes the whole group. It asks only when that would cut something short, a turn running, a message
queued or background work in any window, or a command running in the bottom terminal:
`close Eitri and its 2 windows? 3 turns are running, 1 queued (y/n)`. With every window idle it closes at once, since
the windows come back at the next start anyway. Text you typed in a chat window and did not send counts too when
it would not come back with its tab: in a new tab that has not started a conversation yet, a very long text, or any
text with `agent.restore = "off"`, since then no tab comes back. While the
question is up, closing the windows it names is an answer as well: once what is left is idle, window 1 and the idle
windows close with no further question.

A compositor that exits or crashes takes every window with it at once, with no time to ask, so a turn running then stops
where it was. At the next start the chat window's offer says so under its rows (`a turn was cut off when this window last
ended`), and so does the message when the tab comes back, with `agent.restore = "auto"` too.

When nvim exits in window 1 (`:qa` or `:wqa`, say), window 1 closes the same way: at once when everything is idle,
and with that same question when something would be cut short. Answer `n` and the group stays: the bottom terminal
takes the window and the keys, and `<prefix> e` starts nvim again. The next start opens window 1 with the editor alone,
as every start does.

Closing a chat window asks only while a turn runs (`close window? 1 running (y/n)`), and when it is idle it closes with no
question. **This differs from `eitri` mode,** where closing a window always kept its tabs: here a chat window you close
leaves the tab list, as closing a tab does. The conversation is still in the chooser.

A crash of window 1 leaves the chat and page windows up, without an editor, and a turn in one keeps running. The next
Eitri window on the project links up to all of them.

## Coming back

`window.restore` decides whether the group comes back: which windows open, which output each goes to, and how tall the
bottom terminal is when you next show it. The terminal itself starts hidden either way, unless your
`eitri.layout.default` shows it. The keys end in window 1. With `false`, the start layout decides. Either
way every chat window that has saved tabs opens too, unless `agent.restore` is `"off"`, and `agent.restore` decides
whether the tabs return at once or wait for your answer.

The tabs are in one file in both modes. A tab saved in `c3` opens as view 3 when you go back to `eitri` mode, in a tile
below the chat, and back in `c3` when you return. The layout you had in `eitri` mode waits for you, unchanged.

## Known gaps

- `<prefix> ?` in a page window shows the chat sheet of the last chat window, not the page's keys. The page's own `?`
  lists them. In window 1 it opens the same sheet, which leaves out keys that work only there, such as `<prefix> d`, `[`,
  `o` and `;`.
- `:EitriTutor` follows the mode: in a window that opened in compositor mode its lessons teach this mode's keys (chat
  windows, the bottom terminal, moving out to your compositor) instead of Eitri's own layout.
- `:EitriMarkdownToggle` opens the page's window and leaves the keys in the editor, but it cannot close it. Only the
  compositor can.
- On Hyprland, `Ctrl+h/j/k/l` stops at a floating window's edge when the next float sits diagonally, sharing no row or
  column with it, though Hyprland's own focus keys reach it. Floats that overlap are left to Hyprland.
- Nothing tells Eitri who closed a window, so it goes by what follows: a window closed within two seconds of window 1
  being asked to close stays for the next start, whatever you answer, and so does one that goes while window 1's question
  is up. Closes less than two seconds apart count as one, so that holds for all of them. Close a chat window and then
  window 1 straight away, and that chat window comes back. A tool that closes windows more than two seconds apart reads as
  you closing them.

## What is not supported

- Terminals as windows. There is one bottom terminal in window 1, and your compositor can open as many terminals as you
  like.
- i3 and X11.
- Your compositor's own focus keys stepping through nvim's splits first (`SUPER+h` the way `Ctrl+h` does). `Ctrl+h/j/k/l`
  inside Eitri's windows do.

For one chat beside any editor, [`eitri split` and `eitri panel`](companion.md) remain the lighter choice on these
compositors too.

## What has been tried

All of this ran with a stand-in model and scripted keys, never on real hardware:

- **sway 1.12**, in an isolated headless session: every row of the pass, including an output that returns after the start.
- **Hyprland 0.57** with a Lua config, and **0.56.2** with `hyprland.conf` and with a Lua config, in a virtual machine:
  windows, rules, restart on two outputs, the off keys and the edge keys pass.
- **niri 26.04**, nested in another compositor in a virtual machine: windows, rules and focus moves pass. Its outputs,
  restart and the off keys were not run.

GNOME, KDE and a real `claude` or `codex` in these windows have not been tried. Reports are welcome. The
[known issues](../known-issues.md) list the rest.
