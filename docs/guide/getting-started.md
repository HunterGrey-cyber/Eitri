English | [简体中文](getting-started.zh-CN.md)

# Getting started

Open a project, send a prompt, answer the card when the agent asks, and press `c` after the turn to see what it changed.

[Install](../../INSTALL.md) Eitri and set up `claude` first: Eitri runs the Claude Code you installed, and
[Where Eitri looks for `claude`](#where-eitri-looks-for-claude) says what to do when it cannot find it. Keys here are
the defaults. The prefix is `Ctrl+b`, so `<prefix> x` means `Ctrl+b`, let go, then `x`, as in tmux. The leader is
`Space`. [Keys](keys.md) has the full list, and `F1` in the panel shows it. If you have never used tmux, start with
[these 6 keys](tmux-6-keys.md).

## The first launch

With no `init.lua` yet, the first window opens a setup screen. It lists what it found on your computer, such as your tmux
prefix and Neovim mappings that could collide with Eitri's keys, and asks how the window's keys should work. On Hyprland, sway and niri it also offers, and selects, a preset in which your window manager arranges chats and pages ([Compositor windows](compositor-windows.md)).

- `j` and `k`, or a preset's number, choose; `Enter` shows the lines it would write.
- `Enter` on that review writes `~/.config/eitri/init.lua`, never over an existing file; `Backspace` goes back. The new keys, and any `window.tiling` line, apply from the next start. After writing, `t` opens `:EitriTutor`.
- `Esc` means not now, and the screen asks once more at the next start. `d` stops it for good, and `:Eitri setup`
  brings it back.

To write the file yourself, press `Esc` twice or `d`. [Configuration](configuration.md#first-start) lists what the screen offers.

## Open a project

```sh
eitri [DIR]
```

`DIR` is the project directory. Without it Eitri uses `$EITRI_PROJECT_DIR`, then the directory you ran the command in.
From a terminal, `eitri` gives the prompt back once the window is up; the window runs on its own, and
[Starting from a terminal](#starting-from-a-terminal) covers its output and closing the terminal.

Each directory is its own window with its own Neovim, sessions and project root, so several projects can be open at
once. A window holds your editor on one side and the agent panel on the other, and `Ctrl+h` and `Ctrl+l` move the keys
between them. The panel starts on an empty tab with a menu:

| key | what it does |
|---|---|
| `i` | start a new session by typing |
| `s` | restore the tabs the last window on this project had open, when there are some |
| `r` | resume the most recent saved conversation, when there is one |
| `w` | choose among every saved session |
| `p` | open another project |
| `Shift+Tab` | cycle the mode (Manual, Auto, Bypass) before you start |
| `?` | list all the keys |



`<prefix> O` in any window lists the projects you opened before, newest first. One that has a window comes forward,
and any other opens in a new window ([More on projects](#more-on-projects)). Started from the desktop icon with no folder, Eitri shows this list in a small window first; [`window.desktop_start`](configuration.md#settings) changes that.

To open a file instead of a directory, see [Open a file](#open-a-file); for odd names, [Directory arguments](#directory-arguments). `eitri --clean` starts the editor's Neovim without your configuration, which shows whether a problem is yours.

## Your first turn

Press `Ctrl+l` to focus the panel, then `i` to type. `Enter` sends and `Shift+Enter` starts a new line. The session
starts with your first message. In a project that ships its own Claude Code configuration (`.claude/`, `.mcp.json` or
`CLAUDE.md`), Eitri first asks whether to trust it: `y` loads it, `n` starts without it, and `Esc` takes your text back
([Trusting a project](permissions.md#trusting-a-project)).

Each message carries the name of the open file, or the lines you selected in Visual mode. Select the function before you
say "fix this function"; a cursor position alone is not sent. With lines selected, `<leader>ae` in the editor moves the
keys to the panel with `Edit the selection: ` in the box ([Keys](keys.md#editing-the-selection-from-neovim)).

The reply renders as Markdown while it streams. Each tool call is a quiet line marked `✓` when it finished, `✗` when it
failed and `◐` while it runs. Each file edit is a card with its diff.

- `Esc` leaves the box and browses the conversation: `j` and `k` step by row, `Enter` shows a tool's result, `yy` copies the row, and `F1` lists all the keys.
- `Ctrl+c` interrupts the turn.
- `Enter` on a message typed while a turn runs puts it into that turn. It shows above the box as `↪ <message>` until
  the agent reads it at its next step. A tab whose agent cannot take messages mid-turn, such as an ACP tab, holds it as
  `⧗ <message>` and sends it when the turn ends. `Ctrl+Enter` sends now, interrupting the turn.
- `Ctrl+g` edits your draft in Neovim, and `:wq` brings it back.

After a turn that edited files, `c` in browsing shows what changed on disk: [Turn review](turn-review.md).

## Arranging the window

Try a few layout keys after your first turn. If Eitri picked up your tmux bindings, see
[Your tmux keys](keys.md#your-tmux-keys) and [Coming from tmux](migrate-from-tmux.md).

1. In the panel, `<prefix> "` then `t` opens the bottom terminal below it and moves the keys there. `<prefix> %`
   splits to the right instead.
2. `<prefix> o` moves the keys to the next module, and `<prefix>` with an arrow key goes to the module on that side.
3. In the terminal, `<prefix> Alt+Enter` opens a second terminal to its right, in the project root.
4. `<prefix> F` floats the focused module, `<prefix> _` stows it in the scratchpad, and `<prefix> S` calls it back.
5. `<prefix> s` then `c` copies this arrangement into a new workspace and switches to it; `<prefix> L` returns to the
   previous workspace.
6. In the base panel, `<prefix> c` adds a chat tab and `<prefix> !` breaks it into its own view. `<prefix> !` in that view joins it back. The base view keeps at least one tab.

To start every project from one layout, put `eitri.layout.default(...)` in `~/.config/eitri/init.lua`;
[Configuration](configuration.md#layout-from-lua) shows how to export the layout you have. The other layout keys are in [Keys](keys.md#the-prefix).

## When Eitri asks

Some tool calls wait for you. Eitri draws a permission card beside the call, with **Approve** and **Deny**. For many
commands a button between them, "Always allow ... in this project", saves a rule so such commands pass without a card.
You answer in card mode:

- `Alt-n`, from the box too, takes the next waiting card in this tab. `Enter` on a card's row does the same.
- `1` approves and `2` denies. On a card with the "Always allow" button, `2` approves and saves the rule, and `3` denies.
- `h`, `l` and `Enter` walk to a button, and `Tab` opens a box for a reason on a Claude card.
- `Esc` leaves card mode with the card still waiting.

No letter answers a card, so the first letters of a sentence you are typing cannot answer one by accident
([Keys](keys.md#answering-a-card)).

Every tab is in one of three modes, and `Shift+Tab` (or `<leader>m`) cycles them: manual, auto, bypass. Manual turns everything except reads inside the project, read-only commands and your saved rules into a card. Auto, the default, leaves the rest to
Claude Code's own auto mode. Bypass approves every call without a card and asks before you enter it.
[Permissions](permissions.md) says what each mode lets through, and covers saved rules and trusting a project.

## Tabs and sessions

Each conversation is a tab, and the tab bar appears once there are two or more.

| keys | what they do |
|---|---|
| `<prefix> c` | a new, empty tab |
| `<prefix> n` / `<prefix> p` | the next / the previous tab |
| `<prefix> 1` ... `9` | that tab by number |
| `<prefix> l` | the tab you were on before |
| `<prefix> ,` | rename the tab |
| `<prefix> &` | close the tab; it asks first |
| `<prefix> w` | a list of the open tabs and the saved sessions, to switch or resume |
| `<prefix> i` | details of this tab: mode, model, session ids and project trust |

`H` and `L` in the panel also move between tabs. A tab runs one agent, Claude by default.
`:tabnew codex` opens a Codex tab, and an agent you register can have tabs too ([Codex and other agents](agents.md)).
Background tabs keep streaming, and the tab bar marks what they need: `⚑` for a waiting card, `•` for a turn that
finished while you were elsewhere, `✕` for a session that ended badly.

Closing a tab keeps its conversation. `r` on an empty tab resumes the most recent session, `<prefix> w` lists them all,
and `s` brings back the tabs the last window had open. To carry a conversation into a real terminal: [Resume, restore and continue](#resume-restore-and-continue).

## When you start again

Close Eitri, or lose it to a crash, and the next start in the same project reopens the editor as you left it: the files
in their splits and tab pages, each window at its line and scroll position. When everything came back, Eitri says nothing. More: [What comes back, and what does not](#what-comes-back-and-what-does-not).

## When something goes wrong

A turn that stops early leaves a muted row saying why, and the status line repeats it until your next prompt.
[When something ends](#when-something-ends) lists the rows. [When Eitri itself crashes](#when-eitri-itself-crashes)
says where the crash record goes and what it holds.

## Limits

- Folds, window and buffer options, working directories, terminal buffers, help, quickfix and location lists, floating windows, unnamed buffers, files never written, and a plugin's diff tab or a turn review's buffers do not reopen. Marks, registers, the jump list, command history and undo stay Neovim's own.
- A session plugin that Eitri does not recognise restores beside Eitri and opens your files twice. Set
  [`editor.restore`](configuration.md#settings) to `false`.
- A bare `autocmd!` at the top of your configuration removes Eitri's reopening for that run. Put `autocmd!` inside the
  `augroup` it belongs to.
- Unsaved text is as safe as in plain Neovim: a swap file keeps it, and `noswapfile` does not. If Eitri is killed
  outright, the last 30 seconds of window moves are lost, more if the editor was busy.
- A crash inside a native library (GTK, WebKit, a graphics driver) writes no record. The window log has what it
  printed.
- On GNOME, `eitri FILE` typed in a terminal may show a "window is ready" notice instead of raising the window.

More in [Known issues](../known-issues.md).

---

## Details



### Where Eitri looks for `claude`

Eitri runs the `claude` on its `PATH`. A window started from a desktop launcher or a menu rather than a terminal often
has none there, so Eitri then looks where Claude Code's installers put it, in this order:

1. `~/.local/bin/claude` (the installer from the quickstart page)
2. `~/.claude/local/claude` (older local installs)
3. the `bin` directory of npm's global prefix, when you set one yourself (`NPM_CONFIG_PREFIX`, or `prefix` in `~/.npmrc`)

It takes the first that is a regular file you may run, owned by you or by root, which neither it nor any directory on
the way to it (links followed) can be written by other users; anything else there is skipped and the window's log says
why. Setting `VERDANDI_CLAUDE_CLI_PATH` to a path chooses the program yourself, and Eitri then looks nowhere else.

If Eitri cannot find `claude`, the first-start screen and a Claude tab's failure row say so, name the places it looked,
and give the address to get it from, <https://code.claude.com/docs/en/quickstart>; install it, then press `r` in the tab.

### The PATH of a window started from the desktop

A window started from a desktop launcher, a menu or your compositor's key binding has the `PATH` your session gave
it, which often lacks what your shell configuration adds (`~/.local/bin`, `~/.cargo/bin`, a version manager's
directory). So such a start asks your login shell once (`$SHELL -i -l -c`, at most 2 seconds) for its `PATH` and
uses that for the window, its Neovim, the bottom terminal and the agents; with a heavy shell configuration that
adds its run time to the start (about 0.3 s for a zsh with a banner). Your configuration sees
`EITRI_RESOLVING_ENVIRONMENT=1` then, if it wants to skip something. If the shell takes too long or fails, the
window keeps the `PATH` it was given and its log says why. To skip this, set `EITRI_LOGIN_PATH=off`; the other way
to give every program you start from the desktop the same `PATH` is a file in `~/.config/environment.d/` (for
example `PATH=$HOME/.local/bin:$PATH` in `10-path.conf`), which systemd's user session reads at login.

### Starting from a terminal

Started from a terminal, `eitri` gives you the prompt back as soon as the window is up and prints nothing. The window
runs on its own, so closing that terminal, `exit`, a tmux pane you kill or an ssh session that drops leave it running,
and its output goes to its log (`~/.local/state/eitri/log/`). If the window ends before it is up, the command says so,
shows what it printed and exits with its code. `eitri --no-fork DIR` keeps the window in the foreground, with its
output in the terminal, and it ends with the terminal. Commands that are not a window (`eitri agents`,
`eitri sidecar ...`, `eitri setup`, `eitri panel`, `eitri page`, `eitri split`, `eitri open`) always stay in the
terminal.

The window's title names the project (`myproject — Eitri`, and with two workspaces the one on screen:
`myproject — Eitri · docs`), which is what a window switcher or a compositor's window rule reads.

### Directory arguments

A path in a folder that does not exist, or an option Eitri does not know, stops the launch with a message instead of
opening some other directory; to open a directory whose name starts with `-`, put `--` before it (`eitri -- -notes`).
A name that does not exist in a folder that does is a new file, so a mistyped directory name opens an empty buffer of
that name.

### Open a file

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

### More on projects

To look for a project that is not in the list, press `h` on one to see the folder that holds it, and its sibling folders,
with the project selected. `l` or `Enter` goes into a folder, `h` goes back up, and `Enter` on the `.` row opens the
folder you are in as a project. A path typed in the line shows that folder's subfolders the same way, and a folder on a
network share that does not answer says so instead of making the window wait. The keys are in
[Keys](keys.md#the-prefix). A window that comes forward stays as it was, on the Claude account it started with, even when
you chose it from a window or a desktop entry on another account; `eitri --account <name> --new-window <dir>` gives the
project a second window on that account. When a new window cannot start, a short message says the folder is not there, or
sends you to that project's window log, described in [When Eitri itself crashes](#when-eitri-itself-crashes).

### Resume, restore and continue

**Resuming a conversation.** Closing a tab does not delete its conversation. `r` on an empty tab resumes the
most recent saved session, and `<prefix> w` (or `w` on an empty tab) lists them, newest first, each with its name or
title; a session another window currently holds is shown but cannot be chosen. A project keeps its 200 newest
sessions: once it holds that many the list says `only the newest 200 are kept`, and the next new session drops the
oldest one from it (its conversation stays in Claude Code's own `claude --resume`).

**Restoring the last tabs.** Eitri remembers which tabs the last window on a project had open: their order,
names and modes, and which one was on screen. By default the empty tab offers them as `Restore last session`
(`s` in the panel). The keys start in the editor, where `s` is nvim's, and while they are there the empty tab says
which key brings them to the panel (`Ctrl+l to use these keys`); from the editor `<prefix> w` also opens the session list
with the cursor on a `Restore last session` row, and `Enter` takes it. Setting `agent.restore` to `"auto"` in your `init.lua` brings them back at launch with no key pressed,
and `"off"` neither offers nor remembers; see [Configuration](configuration.md). A tab is skipped, and named
in a short message, when its saved conversation is gone or another window holds it. A tab that was in bypass
does not come back in bypass unasked: `s` asks (`n` brings it back in Auto), and an automatic restore brings
it back in Auto and says so. A tab that was in manual comes back in manual. The one exception is your own answer in advance: with `agent.default_mode` set to
`"bypass"` in your `init.lua`, saved bypass tabs come back in bypass without a question.

A tab's unsent text is saved with it and comes back in its composer with `s`, at most 256 KiB of it. Text that would
not come back, in a tab with no session yet or too long to keep, is counted by the window's close question:
`close window? 1 unsent draft (y/n)`. Starting something new in place of `s` replaces what was saved, the unsent text
with it.

**Continuing in a real terminal.** Press `<leader>t` (or open `<prefix> i` and choose `Continue in a
terminal...`). Eitri explains that it will close this conversation in the panel first, and then, when you
confirm, shows the `claude --resume ...` command that continues it in your own terminal; `y` copies it. Eitri
does not start that terminal for you and takes no lock on the session, so do not keep it open in two places at
once. That terminal runs a plain `claude` with your own Claude Code settings, not Eitri's permission gate, so
its tool calls do not appear as cards. A session that has not had its first turn yet has nothing to continue, and neither does one with a
turn still running.

### What comes back, and what does not

Close Eitri (`:qa` in the editor closes it as it closes Neovim, and so does closing the window; either asks first only
when something would be interrupted, such as a running turn), or lose it to a crash or a reboot, and the next start in
the same project opens the editor where you left it. The files you had in windows come back split the way they were, in the same tab pages, each window at the line and
scroll position it had, with the same window holding the cursor. Files you had listed but not on screen are back in
`:ls`. A split keeps its proportions, so a 30/70 split is still 30/70 on a narrower pane. When everything came back,
Eitri says nothing.

Eitri keeps a small file of what is open in its state directory, updated every 30 seconds, when the window loses focus
and when the editor exits (a `:qa`, a window close, SIGTERM). If Eitri is killed outright, the next start has the
record from the last update. The record holds file names and positions, never text. Neovim writes a swap file by default
after 200 typed characters or 4 seconds without typing.

What comes back and what does not (folds, window options, terminal buffers and the rest) is under Limits above.
When something could not come back, one short line says what: `2 files from last time are gone: src/old.rs and 1 more`,
`could not reopen src/x.rs`, or `3 files from last time did not fit on screen; they are in :ls`. A file that has unsaved
changes in a swap file from an editor that died opens read-only, and the usual line about unsaved changes follows (`:e`
the file, then `r` recovers them). If the last restore never finished, because Neovim crashed or hung while opening
files, the next start opens nothing, says so once, and tries again after a record is written.

**When Eitri stands aside.** If you use a session plugin, Eitri leaves reopening to it: it restores nothing, writes
nothing, and says nothing. It recognises `persistence.nvim` (LazyVim's), `auto-session`, `resession.nvim` (AstroNvim's),
`possession.nvim`, `nvim-possession`, `neovim-session-manager`, `persisted.nvim`, `mini.sessions`, vim-startify with
`g:startify_session_autoload` set, vim-prosession, and a session your configuration has already loaded. It only looks at
what is loaded or declared and never loads or sets up anything of yours. Eitri also does nothing when you start it with
files (`eitri FILE`), when your configuration has already opened a file before the editor shows, with `eitri --clean`,
in window 2, and in a [companion](companion.md) window, where the Neovim is yours. A second window on the same project
that is not paired with the first keeps its own record and never opens the first window's.

### When something ends

A turn that does not finish normally leaves a muted row in italics where
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
tab switch, and can be copied with `yy` like any other row.

When the session itself is gone, the conversation says so. A red `This session was lost` banner means the
connection broke and what is shown above may be incomplete; a quiet `This session has ended (<reason>)` means
it closed. Either way, press `r` in browsing to start a new session in that tab. A message held while a turn
ran is still sent when that turn ends, even when it ended at a limit.

If nvim itself goes without being asked, killed or crashed, the window stays and says so:
`nvim stopped unexpectedly — <prefix> e starts it again and says what can be recovered`. `:q` of the last buffer still
closes the window, as it does in plain Neovim. The exception is a quit from a start screen (the buffer is a
LazyVim dashboard, `alpha`, `dashboard`, `ministarter` or `starter`) in a window that also holds a chat, the terminal or
a page: that asks `close Eitri? (y/n)` first, because a second `q` pressed in a plugin's float (Lazy's update screen,
say) quits through the dashboard behind it. `n` ends the editor in place, the window stays, and the editor's key starts
nvim again; `y` closes. `:qa`, `:wqa` and `ZZ` from a real buffer still close at once. When a start finds this project's files with unsaved changes in swap
files, it says so once: `unsaved changes kept for long.txt — :e the file, then r recovers them or d deletes them`.
Nothing is recovered unless you do that, and the line comes back at each start until the swap file is gone.

If what Eitri keeps can no longer be written, the window says so once per kind until it works again, with what is not
being kept: `could not save the open tabs`, `could not record this session` or `could not store the history`, usually
`the disk is full`. Turns go on running. Once there is room the saves catch up by themselves.

When Claude Code has updated itself and the panel misbehaves, the sidecar behind it may need an update of its own.
`eitri sidecar check` asks whether one exists, `eitri sidecar update` takes it, and `eitri sidecar use bundled` goes
back. Nothing checks by itself, and open tabs keep the sidecar they started with.
[Updating the sidecar](../../INSTALL.md#updating-the-sidecar) has the details.

### When Eitri itself crashes

A crash writes a record to `~/.local/state/eitri/log/crash-<time>-<pid>.log` (under `$XDG_STATE_HOME` if you set
it), readable only by you. The next start says so once, in one line with the file's path. To report it, attach
that file to a [bug report](https://github.com/HunterGrey-cyber/eitri/issues/new?template=bug_report.yml). Nothing
leaves your machine unless you attach it: Eitri sends nothing anywhere.

The record is plain text: the version, the build, the kernel, the thread, where the code stopped, a backtrace and
the last lines the window printed. It holds no prompt, conversation or tool output, and a crash's own message is
left out when it could quote one. Paths are there, with your home shown as `~`, so read it before you attach it.

A window started from the desktop, from the project list, or normally from a terminal keeps what it prints in
`main-<16 hex>.log` in the same directory, one name per project; at 2 MiB the file moves to `.log.1`, and so does
the last run's when a new one starts. Only `eitri --no-fork` keeps the window in the foreground, printing to that terminal instead of keeping a
window log. When the disk is full the lines are dropped and the window goes on.
`EITRI_WINDOW_LOG=off` turns the log and the records off.

Known rough edges are listed in [Known issues](../known-issues.md).
