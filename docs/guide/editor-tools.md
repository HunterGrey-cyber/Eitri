English | [简体中文](editor-tools.zh-CN.md)

# Your editor, from an agent's tab

An agent in any tab, Claude Code, Codex or one you registered, can show you a file, read what your editor holds, ask
your language servers about the code and propose changes through them. Eitri gives it these tools on a server called
`eitri`, the same in every kind of tab.

To see what your editor holds:

- `open_file` shows you a file at a line, with the lines highlighted, in a window beside the one you are in.
- `read_buffer` reads a file as your editor holds it, unsaved changes included. It reads only a file that is already
  open and never loads one.
- `editor_state` tells it which files are open and which have unsaved changes, where your cursor is, your mode and
  your selection. Files outside the project are only counted, never named, and your cursor in one is not told.
- `diagnostics` gives it the errors and warnings your editor shows, the most severe first. It can ask for only the ones
  that appeared since the turn began.

To ask your language servers, which are the ones already running in your editor:

- `usages` says where a symbol is defined, referenced and implemented.
- `hover` gives what the server says about a symbol, as the server's own text.
- `symbols` lists the symbols of a file, or searches the project's language servers by name.

To propose a change, which writes nothing until `apply_proposal` (see [Proposals](#proposals)):

- `lsp_rename` works out a rename across the project.
- `format` works out the formatting of a file.
- `lsp_code_actions` lists the code actions for a line, and `apply_code_action` works out what one of them would change.
- `apply_proposal` writes a proposal to disk.

One tool stays off until you turn it on: [`nvim_exec`](#nvim-exec) runs Ex commands or Lua in your editor.

## It never takes your cursor

Showing a file leaves your current window, your cursor, your mode and your text as they were. If you are typing in
insert mode when the agent opens a file, you stay in insert mode and your next key goes where it would have gone. The
file appears in a side window; the window Eitri opened for the agent earlier is reused, and your own window is never
the one that changes. The highlight goes after four seconds, or when you enter that window. The tab's row says what
happened, for example "showed `b.py:42` in the editor".

<a id="when-the-editor-is-out-of-sight"></a>
## When the editor is out of sight

Eitri never brings the editor forward for an agent, since that would take your place. If the editor is hidden, zoomed
away or on another workspace, the file still opens there, and the agent is told that you cannot see it. The row then
says "opened `b.py:42` in the editor, which is not on screen". The highlight may be gone by the time you bring the
editor back; the side window stays.

From [window 2](two-windows.md) the file opens in window 1, which stays where it is, and the row says "opened `b.py:42`
in window 1's editor". The agent is told that you may not see it. Eitri cannot tell whether a window is covered by
another one, or, from window 2, whether window 1's editor is hidden.

<a id="cards"></a>
## When Eitri asks first

| what the call does | Manual | Auto | Bypass |
|---|---|---|---|
| reads a file that is open, inside the project | no card | no card | no card |
| has to load a file that is not open | card | no card after a yes at the trust question, a card otherwise | no card |
| reads or asks about a file outside the project | card | card | no card |
| `apply_proposal` | card | card | no card |
| `nvim_exec` | card | card | card |

A file that is not open has to be loaded before it can be shown or asked about, and loading can start language servers
that run code from the project. That is why a load goes without a card only after a yes at the
[trust question](permissions.md#trusting-a-project). A project with nothing to ask about has no trust question: its
sessions get every tool, since nobody said no, and a load there asks, since nobody said yes. A card shows the path, the
change or the code as plain text.

This table is the only one that decides these tools, in every kind of tab. [Saved rules](permissions.md#saved-rules),
Claude Code's auto mode and a Codex tab's reviewer do not judge them.

These cards differ from the other cards in a tab:

- **Answer one on the card.** `a` approves it and `d` denies it, in browsing; `]p` and `[p` reach it, and its buttons
  work as usual. `Ctrl+y` does not answer one: from the box, press `Esc` first. If a provider's card waits behind it,
  `Ctrl+y` approves that one, and the band says which.
- **`D` takes no reason**, and a card never offers "always".
- **Entering Bypass does not answer one.** A card raised before you switched stays waiting in the Bypass tab, and it is
  not counted in the question `Shift+Tab` asks. A call made after you switched has no card, unless it is `nvim_exec`.
- **A card approves the place it names.** If the path is a link and it leads somewhere else by the time you answer,
  nothing is read or shown, and the agent is told so.
- **A card waits ten minutes**, then the call ends with "the person did not answer" and the card goes. An ACP agent has
  its own limit and may give up sooner, and a card it gave up on with no word stays until you answer it or the ten
  minutes pass. If you allow one then, what it asked for still runs, and the answer goes to nobody.

<a id="proposals"></a>
## Proposals

`lsp_rename`, `format` and `apply_code_action` change nothing, on disk or in a buffer. Each answers with a proposal: an
id, the files it would touch and the change to each. Only `apply_proposal` writes, and in a Manual or an Auto tab it
raises a card first, with the files and their changes. `a` writes them and `d` leaves them as they are; the proposal
stays and the agent is told.

In browsing, `Ctrl+g` on that card shows one file of the proposal as it is on disk beside the file as the proposal would
leave it, read-only, in a new Neovim tab page, and `q` closes it. Each press shows the next file, and with several the
band says which, for example `2 of 5: src/b.rs`. A long proposal's card leaves lines out and says how many; `Ctrl+g`
still shows each file whole.

A language server's edit becomes a proposal only if all of this holds:

- **Nothing in it has unsaved changes in your editor**, and nothing changed since the server answered. Eitri compares
  what the server worked from with the buffer and the disk, and tells the agent to ask again if they differ.
- **It touches at most 64 files, none over 4 MiB, all inside the project.**
- **It only edits files that exist.** An edit that creates, renames or deletes a file is refused whole.

A server's commands never run. A code action that does nothing but run a command is refused. One with an edit and a
command proposes the edit alone, and the agent is told that the command did not run.

The look at unsaved changes happens again when `apply_proposal` runs, since a card may wait for minutes, and a file
that changed since the proposal is not overwritten. Your editor reloads a written file that is in a window within about
a second. A tab keeps its eight newest proposals, and they stay behind when the tab moves to another window.

A session you started with `n` at the trust question has none of these tools.

<a id="following"></a>
## Following the agent's edits

Following is off by default. Press `Space e` (`<leader>e` if you changed the leader) in BROWSE to toggle it
for this tab; the band says `following` while it is on. After the agent edits a project file, Eitri shows it
in the agent's editor window at the first changed line, or the top when no change line is available. This
never moves your cursor, changes your mode or replaces unsaved text. Following loads an unopened file only
after a yes at the trust question; there, turning it on permits loading these files, which can start language
servers and filetype plugins. Turning it off stops what was still waiting to be shown.

Set `eitri.config.set("agent.editor.follow", true)` (or `"on"`) in `init.lua` to enable it for new tabs;
`false` (or `"off"`) keeps it off. A tab moved to another window keeps its own choice.

<a id="nvim-exec"></a>
## Running code in your editor

One tool stays off until you turn it on. `nvim_exec` lets an agent run Ex commands or Lua in your editor, and it goes in
your `init.lua`:

```lua
eitri.config.set("agent.editor.tool.nvim_exec", true)
```

Every call is then a card with the code in it, whole, in every permission mode, bypass included. There is no "always":
the next call asks again. Read the card before you say yes: the code runs with your user's full rights inside your
editor, and nothing sandboxes it. A session started with `n` at the trust question does not get the tool.

<a id="window-2"></a>
## From window 2

A tab in window 2 reaches the editor in window 1, but only through paths inside the project that follow no link. Every
other path answers "outside the project, or through a link; move the tab to the editor's window to be asked", with no
card: only a tab beside the editor can ask you.

The tools that show or ask (`open_file`, `read_buffer`, `editor_state`, `diagnostics`, `usages`, `hover` and `symbols`)
work this way. The tools that propose or apply a change, and `nvim_exec`, do not work from window 2 at all: each answers
"move the tab to the editor's window".

<a id="off-switches"></a>
## Two off switches

Both go in your `init.lua`, and are read when the window starts (a session already running keeps the tools it began
with):

```lua
eitri.config.set("agent.editor.tools", "off")           -- no editor tools in any tab
eitri.config.set("agent.editor.tool.open_file", "off")  -- one tool off; the others stay
```

A tool that is off is not declared, so the agent never sees it. A tool name Eitri does not know, or a value that is not
on or off, stops the start and names the key. Every tool is on unless you turn it off, except
[`nvim_exec`](#nvim-exec), which is off unless you turn it on. The names are in the
[settings reference](configuration.md).

<a id="when-a-codex-tab-has-none"></a>
## When a Codex tab has none

A Codex tab goes without the tools, and says so once, when Eitri cannot be sure they would stay Eitri's:

- **"a server named eitri is in your Codex configuration"**: your `config.toml` or the project's `.codex/config.toml`
  defines `[mcp_servers.eitri]`, switched off or not. Codex would merge the two, so rename it.
- **"Eitri could not read all of your Codex configuration"**: a file that could define that name went unread, so Eitri
  does not know. That is your `config.toml` when it is a link, is not valid TOML or is over 4 MiB, or when `CODEX_HOME`
  is relative; or the project's `.codex/config.toml` when it or its `.codex` is a link, Eitri's parser refused it, or the
  project has too many files to walk. Fix the file, or make it a plain file.
- **"this Codex would ask about the editor tools"**: Eitri's check of this `codex` saw it ask before a tool call, which
  would put a question of Codex's own beside Eitri's card. The check was measured on Codex 0.163.0; a newer Codex may
  keep the tools on.
