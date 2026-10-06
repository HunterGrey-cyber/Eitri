English | [简体中文](getting-started.zh-CN.md)

# Getting started

This page takes you from the command to a first finished turn: opening a project, sending a prompt,
answering the cards Eitri shows, working with several sessions, and what the panel says when a turn does not
end well. It assumes Eitri is [installed](../../INSTALL.md) and the `claude` command is signed in to your
account.

Keys are written for the default setup: the prefix is `Ctrl+b`, and "the leader" is `Space` unless your
Neovim configuration sets another `mapleader`. The full list is in [Keys](keys.md); `?` in the panel shows it
too.

## Open a project

```sh
eitri [DIR]
```

`DIR` is the project directory. Without it Eitri uses `$EITRI_PROJECT_DIR` when that is set, and otherwise the
directory you ran the command in. A path that is not a directory, or an option Eitri does not know, stops the
launch with a message instead of opening some other directory; to open a directory whose name starts with
`-`, put `--` before it (`eitri -- -notes`).

Each launch is its own window with its own Neovim, its own sessions and its own project root, so you can have
several projects open at once.

The window holds your editor on one side and the agent panel on the other. The editor is your own Neovim, with
your configuration and plugins. The panel starts on an empty tab that shows the project directory, the Claude
account when one is configured, and a short menu:

| key | what it does |
|---|---|
| `i` | start a new session by typing |
| `s` | restore the tabs the last window on this project had open (shown only when there are some) |
| `r` | resume the most recent saved conversation (shown only when there is one) |
| `w` | choose among every saved session |
| `Shift+Tab` | switch between Auto and Bypass before you start |
| `?` | list all the keys |

`Ctrl+h` and `Ctrl+l` move the keys between the editor and the panel.

Two options are worth knowing. `eitri --version` prints the version and exits. `eitri --clean` starts the
editor's Neovim with `nvim --clean`, that is without your configuration and plugins, which is the quickest way
to tell whether a problem comes from your own setup.

## Your first turn

Press `i` (or `Ctrl+j`) in the panel and type your prompt. `Enter` sends it; `Shift+Enter` starts a new
line, and so do `Alt+Enter` and a backslash followed by `Enter`. The session starts when you send the first
message, not before. Each message also carries the name of the file open in the editor, or the lines you had
selected there in Visual mode. A selection is what points the agent at a particular piece of code, so select
the function in the editor first when you say "fix this function"; a cursor position alone is not sent.

While Claude answers, the reply appears as rendered markdown as it streams in. Each file edit is a diff row of
its own. Each tool the agent runs is a row too: a command, a file it read, a search.

- `Esc` leaves the box and goes back to browsing the conversation. In browsing, `j` and `k` step from row to
  row, `Enter` shows or hides a tool's result, `Ctrl+o` shows the detailed view (every result, with longer excerpts), `y` copies the row and `?`
  lists all the keys.
- `Ctrl+c` interrupts the turn that is running.
- `Enter` on a message you type while a turn is running queues it for when that turn ends. `Ctrl+Enter` sends it
  now: it interrupts the running turn, then sends the queue and this message.
- `Ctrl+g` in the box edits your draft in Neovim; `:wq` brings it back.

After a turn that edited files, `c` in browsing shows what changed on disk during it; see
[Turn review](turn-review.md).

## When Eitri asks

Some tool calls wait for you. Eitri draws a permission card in the conversation, next to the call it gates,
with an **Approve** and a **Deny** button, and for many commands a third button, "Always allow ... in this
project", which saves a rule so that call no longer asks.

In browsing:

- `a` approves the card under the cursor and `d` denies it. `]p` and `[p` jump to the next and the previous
  waiting card without answering anything.
- `D` denies and lets you type a reason first; `Enter` then sends the denial with that reason.
- `h`, `l` and `Enter` work the buttons for you if you prefer to walk to one.
- From the box, `Ctrl+y` approves the oldest card waiting in this tab. There is no key to deny from the box.

A key answers a card only when it is pressed on its own: `a`, `d` and `D` do nothing if another key was
pressed within about a quarter of a second before or after, and none of them takes a count. That is so the
first letters of a sentence you are typing cannot answer a card by accident.

**Auto and bypass.** Every tab is in one of two modes, and `Shift+Tab` switches (or `<leader>m`).

- **Auto** is the default. Eitri still sees every call. A call that one of your saved rules allows is approved
  at once. Everything else is left to Claude Code's own auto mode, which decides without a card; when it refuses
  a call the tool's row says `blocked by auto: <reason>`, and after repeated refusals Claude Code asks you
  itself, on a card. When your Claude Code cannot run its auto mode, Eitri's own rules decide which calls need
  a card instead.
- **Bypass** makes Eitri approve every tool call itself, without a card: its own checks and Claude Code's own
  questions alike, including those a `permissions.ask` rule of yours forces. Because bypass is a lot of trust,
  `Shift+Tab` into it asks first (a `y/n` question whose wording says what else changes, for instance how many
  waiting cards it would approve), and the `y` counts only when it is pressed alone and a moment after the
  question appears. Leaving bypass needs no question.

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

In the panel, `H` and `L` (or `gT` and `gt`) also go to the previous and the next tab. Tabs run side by side: a
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
it back in Auto and says so. The one exception is your own answer in advance: with `agent.default_mode` set to
`"bypass"` in your `init.lua`, saved bypass tabs come back in bypass without a question.

**Continuing in a real terminal.** Press `<leader>t` (or open `<prefix> i` and choose `Continue in a
terminal...`). Eitri explains that it will close this conversation in the panel first, and then, when you
confirm, shows the `claude --resume ...` command that continues it in your own terminal; `y` copies it. Eitri
does not start that terminal for you and takes no lock on the session, so do not keep it open in two places at
once. That terminal runs a plain `claude` with your own Claude Code settings, not Eitri's permission gate, so
its tool calls do not appear as cards. A session that has not had its first turn yet has nothing to continue, and neither does one with a
turn still running.

## When something ends

A turn that does not finish normally no longer looks like one that did. A muted `·` row appears where it
stopped, and the status line at the bottom of the panel says it in a few words until you send the next
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
it closed. Either way, press `r` in browsing to start a new session in that tab. A message you queued while a turn
ran is still sent when that turn ends, even when it ended at a limit.

Known rough edges are listed in [Known issues](../known-issues.md).
