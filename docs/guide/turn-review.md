English | [简体中文](turn-review.zh-CN.md)

# Reviewing a turn

After the agent has worked on your project, turn review answers one question: what changed on disk during
that turn? It lists the files, shows each file's hunks, lets you put a hunk or a whole file back the way it
was before the turn, and lets you send comments on the changes back to the agent as your next message.

It works from snapshots Eitri takes of the project's files, not from your git history, so it behaves the same
in a directory that is not a git repository. Nothing is ever written into your project's `.git`.

With [compositor windows](compositor-windows.md) the review lives in the chat window, and what it opens goes to the
editor in window 1.

Keys below are for the default setup. In the panel, "browsing" is the mode you reach with `Esc` from the
message box; see [Getting started](getting-started.md).

## What it shows

Press `c` in browsing, in a tab that has a session, to open the review of the tab's latest turn. It is drawn over
the conversation, and the status band stays visible. The header names the turn
("turn 3 of 5"), when it started and ended, and says `changed on disk during this turn`.

Each changed file is a row with its added and removed line counts and a sign in front of it:

| sign | what it means |
|---|---|
| `✓` | one of the agent's own successful `Write`, `Edit`, `MultiEdit` or `NotebookEdit` calls in this tab named the file, and it changed |
| `·` | one of those calls named the file, but the comparison shows no change in it: the file did not change, or your ignore rules keep it out of the snapshots, so a change to it cannot be seen |
| `?` | the file changed, and none of those calls named it |

In a Codex tab a file change Codex applied counts as such a call (a subagent's included), and in a tab of an
agent registered over ACP an edit, delete or move call that finished without an error does; see [Codex and other agents](agents.md).

The wording is deliberately about the disk, not about who acted. A change made by a shell command the agent
ran, or by you in your editor while the turn was running, has no edit call to name it, so it shows as `?`;
those files are gathered in a folded group, `changed outside this tab's edits`, that `Enter` opens. Only the
signs say anything about attribution, and `?` means "not named", not "not the agent".

Some files have no hunks to open, and their row says why: `binary`, `too large to snapshot` (a file over
8 MiB is left out of the snapshots) or `nested repository, not reviewed` (a directory that is a git
repository of its own). A patch longer than 2000 lines is not cut short: the row says it is too large, gives
the counts and offers `o` to open the file in the editor instead.

The header can carry notes about how exact the comparison is:

- `baseline late: changes made before <time> may be missing`: the first snapshot of the turn finished after the
  agent had already started working.
- `this turn overlapped another tab's turn`: two tabs of the same project were running at once, so a change
  may belong to either.
- `may include the next turn's first changes`: the next turn began before this turn's closing snapshot was done.
- `this turn is still running: compared with the files on disk now`, and the same for a turn whose closing
  snapshot is still being taken.
- `this turn did not finish while Eitri was running`, or a note that the turn has no closing snapshot with
  its reason: the files are compared with the disk as it is now.
- `the baseline snapshot is still being taken`: the list is empty until it is done. An empty list is never
  shown as "no change" while the comparison is not possible.

A turn that changed nothing says `no files changed on disk`.

### Before you approve

The review shows what changed on disk, so an edit still waiting on its permission card is not in it yet. When
some of the turn's file-changing calls have no result, the last line counts them and adds
`Ctrl+g on its card shows the proposal`.

In browsing, `Ctrl+g` on a pending edit card opens a new Neovim tab page with the file as it is on disk on the
left and the file as the edit would leave it on the right, in diff mode, both read-only; `q` closes the tab page.
Nothing is written to the file, and your own buffers are not touched. This works for a Claude `Edit`, `MultiEdit`
or `Write`, a Codex change that adds one file, and an ACP agent's edit of one file. A Codex change to an existing
file, a change to several files, a delete or a move shows the card's text instead, as `Ctrl+g` does on any other
row.

If the file changed after the card appeared and the edit no longer fits it, the band says
`the proposal no longer applies to <path> as it is on disk` rather than showing a diff against the wrong text. A
path that is not a regular file, a file or a proposed text over 4 MiB, and a file that is not UTF-8 text are refused
the same way, with the reason. A change of line ends alone still shows: when one side has CR-LF line ends and the
other LF, the CRs show as `^M`, and a final newline added or removed shows as an empty last line.

### Moving around

| key | what it does |
|---|---|
| `j` / `k` | next / previous file or hunk (inside an open hunk's box they scroll it first) |
| `gg` / `G` | first / last |
| `Ctrl+d` / `Ctrl+u` | five stops down / up |
| `Enter` | open or close a file's hunks; on a hunk, close its file; on the folded group, open it |
| `[` / `]` | the previous / next turn |
| `S` | switch between this turn and the whole session (everything changed since the session's first snapshot) |
| `y` | copy `path:line` of the row under the cursor; a file row whose patch is not open yet, and a binary or oversized file, copies the path alone |
| `o` | open the file in your editor at the hunk's first line (see [In your editor](#in-your-editor)) |
| `q`, `Esc` or `c` | close |

While the review is open it takes every key: the keys that answer permission cards, for example, do nothing
until you close it. Patches are drawn with the diff colours only, in two line-number columns.

By default nothing tells you a review is waiting. If you want a hint after each turn that changed files, the
status band can say `2 files changed · c to review`; while the turn runs, a line above the input box then
counts what its edits have changed so far (`2 files changed +14 −1`). See [Turning it off](#turning-it-off).

## Reverting and undoing

`x` on a hunk puts that hunk's lines back the way they were before the turn (in the session view, before the
session's first turn). `x` on a file row reverts the whole file and asks first:
`revert the whole file <path> to before turn N? y/n`. A file the turn created is deleted, a file the turn
deleted comes back with the mode it had, and a link comes back as a link. A binary file, a file the turn created
or deleted, and a symbolic link can only be reverted whole: `x` on one of their hunks is refused and says so.

A reverted hunk or file is marked `reverted`. `u` undoes the last revert made here and puts the file back
byte for byte, mode included; the mark then reads `reverted, undone`. Neither key writes anything unless
every one of these holds, and when one does not, the footer says which:

- **No agent turn is running in any Eitri window on this project.** Otherwise: `an agent turn is running`. A
  turn in a window on another project does not count.
- **No other Eitri window has this project open.** Otherwise: `another Eitri window has this project open;
  revert from one window at a time`.
- **Your editor holds no unsaved changes to that file.** Otherwise: `<path> has unsaved changes in the editor;
  write or discard them first`. With no editor attached (for instance a [companion](companion.md) panel that
  is detached), Eitri cannot tell and refuses: `no editor is connected, so Eitri cannot tell whether <path> has
  unsaved changes`.
- **The lines being reverted are still what the turn left.** For a whole file that means every byte of it; for a
  hunk, only that hunk's lines and the context around them, so your own edits elsewhere in the file stay. If they
  differ, the revert is refused with `changed since the turn ended; open it in the editor (o)`, so your own work
  is never overwritten by a stale revert.
- **The path is inside the project.** A parent directory that is a symbolic link is refused, even when it
  leads somewhere harmless.
- **The turn was finished and its snapshots are still there.** A turn without a closing snapshot has nothing
  fixed to revert to, and a turn old enough to have been cleaned up says its snapshots were removed.

If Eitri is killed in the middle of rewriting a file that cannot be replaced atomically (one with a second
hard link, say), the bytes it was about to overwrite are kept. The next time you open the project the status
band says `an interrupted revert left <path>`, even with the hint turned off. Once a session is open in the window, open the review (`c`); the row
is at the top, and `Enter` on it asks whether to restore the file to its bytes from before the interrupted
revert (`y`) or to forget the entry (`n`).

## Sending comments back

`i` on a hunk opens a one-line comment on the lines the hunk added. `Enter` saves it, `Esc` cancels. The
comment shows under its hunk with the line range; `x` on a comment deletes it. `i` does nothing on a file row.

Your comments and the reverts you made form a draft that is kept per tab while Eitri runs; the footer says
`draft: 2 comments, 1 revert · s sends them to the agent`. Reloading the panel does not lose it.

`s` shows exactly what would be sent, built from the disk at that moment, and asks. The message looks like
this:

```
Review of your last turn: 2 comments, 1 revert.

I reverted these changes; the files no longer contain them:
- src/main.rs lines 12-20 (back to how they were before your turn)

Comments:
1. src/lib.rs:40-44
   > the lines you commented on, quoted
   Why does this not return an error?
```

A revert you undid, one that only exists in an unsaved editor buffer, or one whose lines have changed since
is not reported as being in the file: the preview lists it under the message with the reason, and leaves it
out. `y` sends the message as one message from you (when a turn is running it is queued for the end of that
turn, and the question says so); `n` or `Esc` cancels and keeps the draft. After a send the draft is emptied.
Nothing is sent without this preview.

## In your editor

`o` on a file or a hunk opens the file in your editor at the hunk's first line and draws the turn's hunks over
the buffer: added lines highlighted and the removed lines shown above or below them as text that is not in the buffer. In the full
window the editor gets the keys; in a companion window the editor's window is raised. A hunk whose lines no
longer match the buffer is skipped and the notice says how many; the hunks that still match are drawn. Reloading the buffer (`:e!` or a change from outside) clears the overlay
with a notice.

In that buffer:

| key | what it does |
|---|---|
| `]h` / `[h` | next / previous hunk (a count moves several) |
| `<localleader>r` | revert the hunk under the cursor in the buffer only: it is not saved, and `u` undoes it like any edit |
| `<localleader>q` | close the overlay |

`<localleader>` is your Neovim `maplocalleader` (`\` by default). The keys exist only in buffers that show an
overlay: an `]h` of another plugin (gitsigns, for example) is put aside there and comes back when the overlay
closes. A revert made this way is added to the draft; undoing it in the buffer takes it out again. Saving
the file is up to you.

## Turning it off

Two settings in your `init.lua` (see [Configuration](configuration.md)):

```lua
eitri.config.set("review.enabled", false) -- take no snapshots at all; `c` says turn review is off
eitri.config.set("review.hint", true)     -- say "N files changed · c to review" in the status band after a turn
```

`review.enabled` is `true` and `review.hint` is `false` by default. Anything but `true` or `false` is a
startup error that names the key. With review off, no snapshot is taken and no review can be opened, and
neither the status band nor the line above the input box says anything about a turn's changes, whatever
`review.hint` is.

## Where the snapshots live

Under `$XDG_STATE_HOME/eitri/review/<project key>/` (`~/.local/state/eitri/review/` when that variable is not
set), in a private git repository of Eitri's own, one directory per project. Eitri keeps the newest 100 turns of
a project for up to 30 days, and each snapshot takes at most 20,000 files and 10 seconds. Files your
`.gitignore`, `.git/info/exclude` (when the project is a repository's top level) or your global git excludes
list are not snapshotted; ignore files above the project's own directory are not read when the project is a
subdirectory of a larger repository. Deleting the directory loses the review history: the next turn starts a new one. Do not delete it while the
status band offers to recover an interrupted revert (see above): the saved bytes that offer restores from live
in that directory too.
