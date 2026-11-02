English | [简体中文](getting-started.zh-CN.md)

# Getting started

This page takes you from the command to a first finished turn: opening a project, sending a prompt,
answering the cards Eitri shows, working with several sessions, and what the panel says when a turn does not
end well. It assumes Eitri is [installed](../../INSTALL.md) and the `claude` command is signed in to your
account. If Eitri cannot find `claude`, the first-start screen and a Claude tab's failure row say so and give
the address to get it from, <https://code.claude.com/docs/en/quickstart>; install it, make sure `claude` runs in
a terminal, then press `r` in the tab.

Keys are written for the default setup: the prefix is `Ctrl+b`, and "the leader" is `Space` unless your
Neovim configuration sets another `mapleader` of a single key the panel can use (a longer or reserved one falls
back to `Space`). The full list is in [Keys](keys.md); `?` in the panel shows it
too.

## The first launch

If you have no `init.lua` yet, the first window opens a short setup screen before anything else. It lists what it
found on your computer (a tmux configuration and its prefix, your desktop, the Neovim mappings that could collide
with Eitri's keys, a running input method) and asks how the window's keys should work. On Hyprland, sway or niri it
also offers, and selects, a preset where your window manager arranges chats and pages
([Compositor windows](compositor-windows.md)):

- `j` and `k`, or the number of a preset, choose; `Enter` goes on to a review that shows the lines it would write.
- `Enter` on the review writes `~/.config/eitri/init.lua` (once, and never over a file that exists); `Backspace`
  goes back. The new keys, and any `window.tiling` line, apply from the next start. After writing, `t` opens `:EitriTutor` and `Enter` closes.
- `Esc` is "not now": nothing is written, and the screen asks once more at the next start. `d` stops it for good.
  `:Eitri setup` brings it back later.

If you would rather write the file yourself, press `Esc` twice or `d`; everything the screen offers is in
[Keys](keys.md) and [Configuration](configuration.md#first-start).

## Open a project

```sh
eitri [DIR]
```

`DIR` is the project directory. Without it Eitri uses `$EITRI_PROJECT_DIR` when that is set, and otherwise the
directory you ran the command in. A path in a folder that does not exist, or an option Eitri does not know, stops
the launch with a message instead of opening some other directory; to open a directory whose name starts with
`-`, put `--` before it (`eitri -- -notes`). A name that does not exist in a folder that does is a new file, so a
mistyped directory name opens an empty buffer of that name. To open a file instead of a directory, see
[Open a file](#open-a-file).

Each directory you open is its own window with its own Neovim, its own sessions and its own project root, so you can
have several projects open at once. The window's title names the project (`myproject — Eitri`, and with two workspaces
the one on screen: `myproject — Eitri · docs`), which is what a window switcher or a compositor's window rule reads.

The window holds your editor on one side and the agent panel on the other. The editor is your own Neovim, with
your configuration and plugins. The panel starts on an empty tab that shows the project directory, the Claude
account when one is configured, and a short menu:

| key | what it does |
|---|---|
| `i` | start a new session by typing |
| `s` | restore the tabs the last window on this project had open (shown only when there are some) |
| `r` | resume the most recent saved conversation (shown only when there is one) |
| `w` | choose among every saved session |
| `Shift+Tab` | cycle the mode (Manual, Auto, Bypass) before you start (the mode is shown at the bottom of the panel) |
| `?` | list all the keys |

`Ctrl+h` and `Ctrl+l` move the keys between the editor and the panel.

Two options are worth knowing. `eitri --version` prints the version and exits. `eitri --clean` starts the
editor's Neovim with `nvim --clean`, that is without your configuration and plugins, which is the quickest way
to tell whether a problem comes from your own setup.

## Open a file

```sh
eitri src/lib.rs
eitri +42 src/lib.rs
eitri src/lib.rs:42:7
eitri a.rs b.rs
eitri DIR FILE
```

`eitri FILE` opens the file in the editor and finds the project from it: the project is the git repository the file
sits in, or the file's own folder when there is none, except that your current directory is used instead when it
holds the file, is itself a usable folder and, with a repository, lies inside it (`cd crate; eitri src/x.rs` opens
the project `crate`, as `eitri .` there would). A repository counts only when it is a usable folder too (see
[A folder that is no boundary](permissions.md#a-folder-that-is-no-boundary)); one that is not is skipped, and the
search goes on above it. From your home directory, `eitri code/scratch/a.py` with no repository opens the project
`~/code/scratch`, not `~`. A project found this way behaves like one named with `eitri DIR`, including the
[trust question](permissions.md#trusting-a-project) at the first session. The file does not have to exist yet: its
folder must, and `:w` creates the file.

- With several files, the first one is shown and all of them are open, as in `nvim a b c`. A file outside the
  project prints a note and still opens.
- `+N` before the first file starts on line N, and a bare `+` on its last line. `FILE:LINE` and `FILE:LINE:COL` do
  the same, but only when no file with that literal name exists. A line applies to the first file only. A word
  after `+` that is not a number, such as `+page.svelte` or `+/pattern`, is refused because it would be an nvim
  command; for a file with that name write `./+page.svelte` or `eitri -- +page.svelte`.
- After `--` every word is a path.
- `eitri DIR FILE...` keeps `DIR` as the project and shows the files first.
- A folder that does not exist, a pipe, a socket or a device, and a name that is not valid UTF-8 are refused with a
  message. `eitri panel` and `eitri split` still take a directory only, and `EITRI_PROJECT_DIR` stays a directory.

**A window for that project is already open.** Then `eitri FILE` does not start another window: the running editor
opens the file, the window comes to the front with the keys on the editor, and the command returns at once. If that
window's editor was hidden or has not started yet, it is shown and the file opens when it is ready. Typed in an Eitri
window's own bottom terminal, `eitri FILE` opens in that window, even after a `cd` into a subfolder, as long as the
file is inside the window's project. `eitri DIR` and a bare `eitri` always start a new window, and so does
`eitri --new-window FILE` (`--clean` and `--account` imply it). An account set in `VERDANDI_CLAUDE_ACCOUNT` does not
imply it: the running window keeps the account it has. Outside a bottom terminal a window on a folder above the
project worked out here also counts, up to the file's repository, nearest first: `cd repo/crate; eitri src/x.rs` works
out `crate`, and opens in a window on `repo` when none is open on `crate`. A window that cannot hold every file you
named is passed over, and with no repository only the project itself is asked. With no such window a new one starts,
and so it does when the window cannot take the file just now, for instance one sitting in a quit prompt with an
unsaved buffer.
When the buffer in the editor has unsaved changes, the file opens in a split and that buffer stays.

On GNOME a window can be raised only by a launch that carries an activation token, which a command typed in a
terminal does not have: there GNOME may show a "window is ready" notice instead of raising the window. Sway, Hyprland
and niri are expected to focus the window through the compositor. Only sway has been tried.

**A file with no project of its own.** `eitri ~/notes.md`, `eitri /etc/hosts` and `eitri /tmp/x.py` open too, in the
file's folder. When that folder cannot serve as a boundary for the agent (your home directory, a folder you do not
own, a folder other users can write to, the filesystem root), the panel shows a note on every tab page:
`No project boundary here (<why>): a Claude tab asks before every file and command, and a Codex tab starts read-only,
without the folder's configuration. Other agents keep their own rules.` It stays until you dismiss it. What that means for each mode is in
[Permissions](permissions.md#a-folder-that-is-no-boundary).

**From a file manager.** Eitri's desktop entry lists plain text, Markdown, C, C++, Python, Rust, JSON, TOML and shell
scripts, so it appears under Open With for them (it does not become the default). Such a launch usually starts in your
home directory, so the project comes from the file as described above. Several files selected together open as
`eitri a.rs b.rs` does, and several folders open one window each.

## Your first turn

Press `Ctrl+l` to focus the panel, then `i` (or `Ctrl+j`) to type your prompt. If your module-focus
key is remapped, use that key instead of `Ctrl+l`. `Enter` sends it; `Shift+Enter` starts a new line, and
so do `Alt+Enter` and a backslash followed by `Enter`. The session starts when you send the first
message, not before, and in a project that ships its own Claude Code configuration (`.claude/`, `.mcp.json` or
`CLAUDE.md`) not before you have answered a question: Eitri holds the start and asks whether to trust that
configuration. `y` loads it, `n` starts the session without it, and `Esc` puts the start off and gives your
text back. Until you answer, the panel says "Waiting for your answer to the trust question — nothing has
started". What the question shows and what each answer means is in
[Trusting a project](permissions.md#trusting-a-project). Each message also carries the name of the file open in the editor, or the lines you had
selected there in Visual mode. A selection is what points the agent at a particular piece of code, so select
the function in the editor first when you say "fix this function"; a cursor position alone is not sent. With lines selected,
`<leader>ae` in the editor does the first step for you: the keys go to the panel with the box open and `Edit the selection: `
in it, and what you type after it is an ordinary message ([Keys](keys.md#editing-the-selection-from-neovim) has the details).

While Claude answers, the reply appears as rendered markdown as it streams in. Each tool the agent runs is a
quiet line of its own, with a mark in front saying whether it finished (`✓`), failed (`✗`) or is still running
(`◐`): a command, a file it read, a search. Each file edit is a small card naming the file and the lines it adds
and removes, with the diff inside it.

- `Esc` leaves the box and goes back to browsing the conversation. In browsing, `j` and `k` step from row to
  row, `Enter` shows or hides a tool's result, `Ctrl+o` shows the detailed view (every result, with longer excerpts), `y` copies the row (to the
  clipboard and the primary selection, so a middle-click pastes it too) and `?` lists all the keys.
- `Ctrl+c` interrupts the turn that is running.
- `Enter` on a message you type while a turn is running puts it into that turn without stopping it, as in
  Claude Code and Codex: the agent reads it at its next step, and it shows above the box as `↪ <message>` until
  then, when it appears in the conversation where the agent read it. A tab whose agent cannot take messages
  mid-turn holds it instead, as `⧗ <message>`, and sends it when the turn ends; the empty box says which one
  Enter does in that tab. Claude and Codex tabs take messages mid-turn; ACP tabs hold them. A message the
  agent did not read before the turn ended (an interrupt, for one) is held again with a line saying why.
  `Ctrl+Enter` sends now: it interrupts the running turn, then sends what is held and this message.
- `Ctrl+g` in the box edits your draft in Neovim; `:wq` brings it back.

After a turn that edited files, `c` in browsing shows what changed on disk during it; see
[Turn review](turn-review.md).

## Arranging the window

Try arranging the modules after your first turn. Here `<prefix>` means press `Ctrl+b`, let go, then press
the next key or chord, as in tmux. These are the defaults; if Eitri has picked up your tmux bindings, see
[Your tmux keys](keys.md#your-tmux-keys).

1. With the keys in the agent panel, press `<prefix> "`, then `t`: the bottom terminal opens below it and
   takes the keys. `<prefix> %` splits to the right instead; either split waits for a module to open or move there.
2. Press `<prefix> o` to move the keys to the next module on screen, and again until they are back in the
   terminal. `<prefix> Left`, `Right`, `Up` and `Down` go to the module on that side.
3. In the terminal, press `<prefix> Alt+Enter` to open a second terminal to its right, with its own shell
   in the project root.
4. With the second terminal focused, press `<prefix> Alt+Shift+Space` to float it. Then `<prefix> Alt+_`
   stows it in the scratchpad. `<prefix> Alt+-` calls it back; press that sequence again to stow it again.
5. Press `<prefix> s`, then `c` to create a second workspace as a copy of this arrangement and switch to it.
   `<prefix> L` returns to the previous workspace. The modules are shared; each workspace keeps its own arrangement.
6. Move to the base agent panel. Press `<prefix> c` to add a second chat tab, then `<prefix> !` to break that
   tab into its own view beside the panel. The base view must keep at least one tab. In the new view,
   `<prefix> !` joins the tab back.
7. To make a starting layout for projects without a saved one, put `eitri.layout.default(...)` in
   `$XDG_CONFIG_HOME/eitri/init.lua` (default `~/.config/eitri/init.lua`). Once the window is running,
   `eitri.layout.export()` returns the layout calls to paste there, including workspaces and floats;
   [Configuration](configuration.md#layout-from-lua) shows how to save that text from a Lua command. Numbered chat
   views are left out of the export: they need a saved tab to survive startup, so use [open
   rules](configuration.md#where-a-new-module-opens) to place new views.

See [Keys](keys.md#the-prefix) for tabbed and stacked containers, resizing and the rest of the layout keys.

## When Eitri asks

Some tool calls wait for you. Eitri draws a permission card in the conversation, next to the call it gates,
with an **Approve** and a **Deny** button, and for many commands a third button, "Always allow ... in this
project", which saves a rule that approves such commands without a card from then on. A rule does not answer
the questions Claude Code raises itself, so a command it still asks about gets a card even then; see
[When Claude Code itself asks](permissions.md#when-claude-code-itself-asks).

In browsing:

- `a` approves the card under the cursor and `d` denies it. `]p` and `[p` jump to the next and the previous
  waiting card without answering anything.
- `D` denies and lets you type a reason first; `Enter` then sends the denial with that reason.
- `h`, `l` and `Enter` work the buttons for you if you prefer to walk to one.
- From the box, `Ctrl+y` approves the oldest card waiting in this tab, leaving out a card from an agent's
  [editor tools](editor-tools.md#cards), which takes `a` or `d`. There is no key to deny from the box.

A key answers a card only when it is pressed on its own: `a`, `d` and `D` do nothing if another key was
pressed within about a quarter of a second before or after, and none of them takes a count. That is so the
first letters of a sentence you are typing cannot answer a card by accident.

**Manual, auto and bypass.** Every tab is in one of three modes, and `Shift+Tab` (or `<leader>m`) cycles them
in Claude Code's order: manual, auto, bypass, then manual again.

- **Manual** is the strictest. Eitri's own rules answer every call: reading inside the project and read-only
  commands run, your saved rules still apply, and everything else is a card, every file edit and write
  included. Nothing is left to Claude Code's own auto mode. Set `agent.default_mode` to `"manual"` in your
  `init.lua` to start every tab in it.

- **Auto** is the default. Eitri still sees every call. A call that one of your saved rules allows is approved
  at once. Everything else is left to Claude Code's own auto mode, which decides without a card; when it refuses
  a call the tool's row says `blocked by auto: <reason>`, and after repeated refusals Claude Code asks you
  itself, on a card. When your Claude Code cannot run its auto mode, Eitri's own rules decide which calls need
  a card instead.
- **Bypass** makes Eitri approve every tool call itself, without a card: its own checks and Claude Code's own
  questions alike, including those a `permissions.ask` rule of yours forces. Because bypass is a lot of trust,
  `Shift+Tab` into it asks first (a `y/n` question whose wording says what else changes, for instance how many
  waiting cards it would approve), and the `y` counts only when it is pressed alone and a moment after the
  question appears. Bypass is entered only from auto; leaving it needs no question and lands in manual.

Exactly what each mode lets through, the saved rules, and how a project's own Claude Code configuration is
trusted are in [Permissions](permissions.md).

## Tabs and sessions

Each conversation is a tab. The tab bar appears once there are two or more.

| keys | what they do |
|---|---|
| `<prefix> c` | a new tab (an empty one, with the menu above) |
| `<prefix> n` / `<prefix> p` | the next / the previous tab |
| `<prefix> 1` ... `9` | that tab by number |
| `<prefix> l` | the tab you were on before |
| `<prefix> ,` | rename the tab |
| `<prefix> &` | close the tab (asks first, and says so when a turn is running and will be interrupted) |
| `<prefix> w` | a list of the open tabs and the saved sessions, to switch or resume |
| `<prefix> i` | details of this tab: name, mode, model, session ids, the settings it loaded and whether its project is trusted |

In the panel, `H` and `L` (or `gT` and `gt`) also go to the previous and the next tab. Each tab runs one agent, Claude by default; `:tabnew codex` opens a tab for Codex, and an agent you register
can have tabs too. See [Codex and other agents](agents.md). Tabs run side by side: a
background tab keeps streaming, and its label in the tab bar shows what it needs (`⚑` for a waiting card, `•`
for a turn that finished while you were elsewhere, `✕` for a session that ended badly).

**Resuming a conversation.** Closing a tab does not delete its conversation. `r` on an empty tab resumes the
most recent saved session, and `<prefix> w` (or `w` on an empty tab) lists them all, each with its name or
title; a session another window currently holds is shown but cannot be chosen.

**Restoring the last tabs.** Eitri remembers which tabs the last window on a project had open: their order,
names and modes, and which one was on screen. By default the empty tab offers them as `Restore last session`
(`s`). Setting `agent.restore` to `"auto"` in your `init.lua` brings them back at launch with no key pressed,
and `"off"` neither offers nor remembers; see [Configuration](configuration.md). A tab is skipped, and named
in a short message, when its saved conversation is gone or another window holds it. A tab that was in bypass
does not come back in bypass unasked: `s` asks (`n` brings it back in Auto), and an automatic restore brings
it back in Auto and says so. A tab that was in manual comes back in manual. The one exception is your own answer in advance: with `agent.default_mode` set to
`"bypass"` in your `init.lua`, saved bypass tabs come back in bypass without a question.

**Continuing in a real terminal.** Press `<leader>t` (or open `<prefix> i` and choose `Continue in a
terminal...`). Eitri explains that it will close this conversation in the panel first, and then, when you
confirm, shows the `claude --resume ...` command that continues it in your own terminal; `y` copies it. Eitri
does not start that terminal for you and takes no lock on the session, so do not keep it open in two places at
once. That terminal runs a plain `claude` with your own Claude Code settings, not Eitri's permission gate, so
its tool calls do not appear as cards. A session that has not had its first turn yet has nothing to continue, and neither does one with a
turn still running.

## When something ends

A turn that does not finish normally no longer looks like one that did. A muted row in italics appears where
it stopped, and the status line at the bottom of the panel says it in a few words until you send the next
prompt:

| what happened | the row says | the status line says |
|---|---|---|
| you pressed `Ctrl+c` | `interrupted` | `interrupted` |
| a failure | `the turn ended with an error (...)`, then Claude Code's own words | `turn failed` |
| a rate or usage limit (HTTP 429) | `rate or usage limit reached (HTTP 429)`, then Claude Code's words | `rate limited` |
| the turn limit or a spending limit | `stopped at the turn limit` or `stopped at the spending limit` | `turn limit` or `spending limit` |
| the conversation no longer fits | `stopped: the context is full` | `context full` |
| a hook of yours stopped it | `stopped by a hook` | `stopped by a hook` |
| the session ended under it | `the turn did not finish: the session ended` | `turn did not finish` |

The text after a colon on a row is Claude Code's own message, shown as it came, so a limit's reset time appears
only if Claude Code wrote one. A row stays in the conversation, survives a panel reload (`<prefix> r`) and a
tab switch, and can be copied with `y` like any other row.

When the session itself is gone, the conversation says so. A red `This session was lost` banner means the
connection broke and what is shown above may be incomplete; a quiet `This session has ended (<reason>)` means
it closed. Either way, press `r` in browsing to start a new session in that tab. A message held while a turn
ran is still sent when that turn ends, even when it ended at a limit.

When Claude Code has updated itself and the panel misbehaves, the sidecar behind it may need an update of its own.
`eitri sidecar check` asks whether one exists, `eitri sidecar update` takes it, and `eitri sidecar use bundled` goes
back. Nothing checks by itself, and open tabs keep the sidecar they started with. Eitri 0.3.0 has no `eitri sidecar`
command (it answers `eitri: no such directory`); take the release after it first.
[Updating the sidecar](../../INSTALL.md#updating-the-sidecar) has the details.

## When Eitri itself crashes

A crash writes a record to `~/.local/state/eitri/log/crash-<time>-<pid>.log` (under `$XDG_STATE_HOME` if you set
it), readable only by you. The next start says so once, in one line with the file's path. To report it, attach
that file to a [bug report](https://github.com/HunterGrey-cyber/eitri/issues/new?template=bug_report.yml). Nothing
leaves your machine unless you attach it: Eitri sends nothing anywhere.

The record is plain text: the version, the build, the kernel, the thread, where the code stopped, a backtrace and
the last lines the window printed. It holds no prompt, conversation or tool output, and a crash's own message is
left out when it could quote one. Paths are there, with your home shown as `~`, so read it before you attach it.

A window started from the desktop keeps what it prints in `main-<16 hex>.log` in the same directory; at 2 MiB the
file moves to `.log.1`, and so does the last run's when a new one starts. A window started from a terminal prints
to that terminal instead, and keeps no log. When the disk is full the lines are dropped and the window goes on.
`EITRI_WINDOW_LOG=off` turns the log and the records off.

A crash inside a native library (GTK, WebKit, a graphics driver) writes no record. What the window printed up to
that point is in the log.

Known rough edges are listed in [Known issues](../known-issues.md).
