English | [简体中文](remote-development.zh-CN.md)

# Remote development

Remote development lets one Eitri window on your computer work on a project that lives on another machine. The code,
the agent and its tools stay on the server, where the toolchains and credentials are. Your Neovim, its plugins, the panel and your
keys stay local. This page covers the sync side: how the project reaches your computer, what does not, and what
Eitri does when the two copies disagree. It then covers language servers and the commands Neovim runs for you
(formatters, linters, `make`), which run on the server.

It is **experimental** and off by default. Nothing here runs until `init.lua` has

```lua
eitri.config.set("remote_dev.enabled", true)
```

With it off, `eitri host open` and opening a remote project stop with one line naming the key. You can still use
`eitri host status`, `list`, `stop`, `uninstall`, and `eitri host sync status` or `conflicts`. To resolve a file
conflict, enable remote development and keep a project window or `eitri host sync watch` open.

## How it works

The server runs a small Eitri helper that Eitri installs there, with your consent, the first time you open a project
on that host. The agent for a remote tab runs on the server, next to the files it edits. Your computer keeps a mirror
of the project at the same path, made by [Mutagen](https://mutagen.io) (a file-sync tool Eitri drives), so that your
Neovim and its plugins read and write ordinary local files. When you save, the file travels to the
server; when the agent edits a file, it travels back.

The rest of the page uses "laptop" for the machine you sit at, and "server" for the other one.

## Server terminals

Closing a server terminal ends its shell and everything still in that shell's session; a program that has left the session (a daemon, or anything started with `setsid`), or that outlived a shell which had already exited, keeps running on the server, as it would after you log out of ssh.

If the laptop's clock jumps forward by more than a minute while a new server terminal is opening, Eitri can lose track of it: the terminal keeps running on the server and `eitri host list` still shows it.

## Before you start

- **Key-based ssh.** `ssh example-host` must log in without asking you anything. Eitri's background connections cannot
  answer a password or passphrase prompt, so a key with a passphrase should be loaded in your ssh agent. Eitri uses
  your `~/.ssh/config`, so a `Host` alias works wherever a host name does.
- **OpenSSH 8.5 or later** on the laptop. Eitri names the version it found if it is older.
- **Mutagen 0.18 or later** on the laptop, found on your `PATH` or at the absolute path in `remote_dev.mutagen`.
  Install it from [mutagen.io](https://mutagen.io), or build it yourself without the SSPL-licensed parts. **Eitri does
  not ship it, download it or build it for you**: Mutagen's official build contains code under the SSPL, and Eitri
  does not redistribute that. If it is missing or older than 0.18.0, `eitri host open` says so and stops. Eitri runs
  its own Mutagen daemon with a data directory of its own, so it never touches Mutagen sessions you made yourself.
  Mutagen puts its own small agent on the server over the same ssh connection.
- **A Linux server** with `git` on the login `PATH`, and the `claude` command installed and logged in there, as the
  account you connect with. The agent runs there with that account's own login.

## Settings

All of them go in `init.lua` with `eitri.config.set`. A wrong value is reported as a startup problem naming the key,
and the default is used.

| Key | Default | Meaning |
|---|---|---|
| `remote_dev.enabled` | `false` | turns remote development on |
| `remote_dev.mutagen` | unset | absolute path to `mutagen`; unset means look on `PATH` |
| `remote_dev.tripwire_mib` | `64` | the size [tripwire](#the-size-tripwire), 8 to 4096 MiB |
| `remote_dev.flush_cap` | `3` | seconds a send waits for your saves to reach the server, 0.5 to 30 |
| `remote_dev.send_waits_for_flush` | `true` | `false` makes a send not wait at all |
| `remote_dev.lsp_grace` | `1800` | seconds the server keeps a language server after Neovim's connection is lost, 60 to 86400 ([the grace](#the-grace)) |
| `remote_dev.lsp_local` | none | names of language servers that stay on the laptop, separated by commas or blanks ([what stays local](#what-stays-on-the-laptop)) |

The other settings are in [Configuration](configuration.md).

## Open a project

From a terminal:

```sh
eitri host open example-host:/srv/project
```

The part before the colon that starts the path is the host as you would give it to ssh: `example-host`,
`user@example-host`, or `ssh://user@example-host:2222`. The path must be absolute. The command goes through these steps, asking before each
one that changes something:

1. It connects. If ssh does not know the server's host key, Eitri shows the key type and fingerprint and asks. Answer
   `y` only if you can check the fingerprint; the key is then added to `~/.ssh/known_hosts`. A key that changed is
   ssh's refusal, shown as it is. Every later connection, including the ones Mutagen makes, must show that same
   key, or nothing runs.
2. It probes the server and tells you what is missing, then asks before it installs the helper there. It asks once per host;
   later versions of Eitri update the helper there without asking again.
3. It prepares the same path on your laptop (next section), checks both machines for another sync tool
   ([Syncthing and Dropbox](#syncthing-and-dropbox)), and measures how big the first sync is.
4. It follows the first sync in the terminal: files, MiB received and expected, the rate, then the history's progress.
   Press `Enter` to stop following it. Sync keeps going while an Eitri window has the project open, and pauses
   when nothing does (see [below](#sync-pauses-when-nobody-is-watching)).

Afterwards open the folder as you would any project:

```sh
eitri /srv/project
```

Eitri remembers that this folder is a remote project, so the window opens as one: the panel's status band shows the
host, the sync state and any conflicts, and agent tabs run on the server. `eitri --local /srv/project` opens the same
folder as a plain local copy, with no connection (agent tabs are refused there, since they run on the server).

`eitri host list` shows what the helper runs on every host you opened: agents, server terminals and language servers.
`eitri host stop example-host` stops them (and pauses sync for that host's projects); `eitri host uninstall example-host` removes the helper. They work
with `remote_dev.enabled` off.

### From the window

With remote development on, the project chooser offers **Open on a host…**. It lists the hosts in your
`~/.ssh/config` and the ones you have opened before, completes folders on the server, and asks the same questions as
`eitri host open`, in the same order. Running `eitri --ssh <host> <path>` for a folder never opened before starts the
same flow. It needs the chooser's web view; without it, use `eitri host open`.

## The same-path rule

The project has the **same absolute path on both machines**: `/srv/project` on the server is `/srv/project` on the
laptop. The agent reports paths as the server sees them, diagnostics and links carry them, and your editor opens
them. With one path they are already right on your side, and no translation layer sits between them to get one wrong.

So the laptop needs that directory, owned by you. If it cannot be created, Eitri prints the line to run once, with
your user name in place of `<user>`:

```sh
sudo mkdir -p /srv/project && sudo chown <user> /srv/project
```

If the directory already exists and is not empty, Eitri never syncs over it. It shows what the copy holds that the
server may not (uncommitted changes, stashes, commits no remote has; otherwise the file count and size) and offers
one action: move it aside to `/srv/project.eitri-local-<YYYYMMDD-HHMMSS>`. Eitri never deletes the moved copy. A folder
that is already this project's mirror (same host, same server key, and the very folder Eitri prepared) is reused with
its contents. If you move the mirror away and put another folder at that path (a fresh clone, a restore from a
backup), Eitri treats it as a new folder: sync pauses, and `eitri host open` asks before moving it aside and starting a
fresh copy. A folder inside another project's mirror, or one that holds a mirror, is refused.

## What syncs and what does not

The laptop's copy holds every file the server has, except what the ignore rules leave out. `.git` is handled
separately (see [Git on the laptop](#git-on-the-laptop)).

### Ignores come from git on the server

Mutagen does not read `.gitignore`, so Eitri reads git's rules on the server and hands them over as patterns: every
`.gitignore` in the tree, `.git/info/exclude` and the server's `core.excludesFile`, including nested repositories and
submodules with their own rules. Patterns, not a list of files that exist, so a `target/` that appears with your first
build is already excluded, and ignored secrets such as `.env` stay on the server.

- **Tracked files still sync**, even when an ignore rule matches them: git tracks them, so Eitri does too. If more
  than 10,000 tracked files are ignored, sync does not start and says the count.
- A `.gitignore` that is a symbolic link, is larger than 1 MiB or is not UTF-8 excludes its whole directory, with a
  line saying why. A path name sync cannot carry is excluded alone and listed; it never stops the rest.
- When a `.gitignore` changes, the ignore rules change with it, but only when it is safe: Eitri waits until no
  agent turn is running, no buffer under the project has unsaved changes and nothing has synced for two seconds,
  then re-creates the session. Until then the band says "ignore rules changed; applying when idle". A file that
  becomes ignored **keeps its copy on the laptop**: sync stops carrying it, and does not delete it.
- If a file existed on only one side when the session was re-created (you deleted one on the laptop while it was
  waiting), the other side's copy can come back. Such files are listed as "copied back"; see
  [below](#an-edit-beats-a-delete).

### A project without git

With no git repository, Eitri leaves out `target/`, `node_modules/`, `.venv/`, `__pycache__/` and `.cache/`, and
**warns that secrets such as `.env` are not excluded**: nothing says they should be, so they are copied to the
laptop. Exclude them yourself:

```sh
eitri host sync exclude .env
```

### `exclude` and `include`

`eitri host sync exclude <path>` keeps a path (relative to the project root) on the server only; `eitri host sync
include <path>` takes it back, undoing an `exclude` or pulling in a path an ignore rule would leave out. Both are stored
in Eitri's record of the project, not in your repository. The change is applied the way a `.gitignore` change is, when
things are quiet, and the laptop keeps the copies it already has. `include` refuses a path that sits under an excluded
directory, and says which rule hides it. Run them from inside the project: the project is the one Eitri has a record of
that holds your current directory.

### The size tripwire

A new directory that suddenly arrives from the server (a build output nobody ignored, a dataset the agent
downloaded) can fill your disk before you notice. When a batch that is coming to the laptop would pass
`remote_dev.tripwire_mib` (64 MiB), Eitri pauses sync for this project and says what it saw:

```text
sync paused: build/out.bin on example-host would send 900 MiB to this laptop; sync of /srv/project with example-host waits until you choose: sync it anyway (eitri host sync allow build/out.bin) or leave it on the server (eitri host sync exclude build/out.bin)
```

`allow <path>` lets that path through (and what is under it) and syncing resumes; `exclude <path>` keeps it on the
server. `allow` with no path allows the path the pause named, or, when it named none, the batch that tripped it. The first sync is exempt, because the terminal shows its
size when you open the project. The tripwire works from Mutagen's progress reports, so on a very fast link some of a
batch can arrive before the pause; when it only notices afterwards, it says how much the laptop's copy grew.

### Symlinks, special files, hard links, times and modes

- **Symbolic links** are synced when they point inside the project by a relative path. A link with an absolute target
  or one that points out of the project is not synced and shows up as a problem in the sync view ("not synced").
- **FIFOs, sockets and device files** are not synced. They are listed in the sync view too.
- **Hard links** do not survive: each name arrives as its own copy.
- **Modification times** are not kept in step between the two machines: a file's time on the laptop is when sync
  wrote it. A build tool that trusts times can rebuild more than you expect.
- **File modes.** Mutagen writes every file it brings as `0600` (`0700` if executable), which would turn a server
  file's `0644` into `0600` on your first save. A small keeper in the helper watches the project and puts the mode
  back: a file keeps the mode it had on the server, a script you made executable on the laptop gains the owner's
  execute bit, and a new file gets the server account's umask. Only the executable bit travels from laptop to server;
  directory modes are not kept. The keeper needs the folder above the project writable by the server account (Mutagen
  writes beside it), and Eitri tells you if a project's modes are not kept and why. On the laptop the files are `0600`
  and `0700`.

## Sync pauses when nobody is watching

Sync runs only while something is looking at the project: an Eitri window, a plain Neovim with `eitri.nvim` open on a
file inside it, or `eitri host sync watch`. When the last one goes away, Eitri pauses both sessions, so that no
conflict, ignore change or large transfer can pile up where you cannot see it. `eitri host sync status` then says "no
Eitri window or eitri host sync watch follows `<root>`; its sync is paused until one does". Opening a window resumes it.
Ctrl+C or SIGTERM releases a first-sync or watch command and pauses its sessions if it holds the project.
A process killed with SIGKILL leaves its sessions running until the next holder takes over.

When `eitri host sync status` shows a recorded pause, the next step depends on its reason.

| Why sync paused | What to do in the project |
|---|---|
| You ran `eitri host stop <host>`, or an uninstall paused the project and did not finish. | Run `eitri host sync resume` when you want to start again. |
| The server's host key changed. | Check the old and new keys shown. If the change was expected, run `eitri host sync resume` and confirm the new server. |
| The size tripwire caught a large incoming transfer. | Use `eitri host sync allow <path>` for the named path, `eitri host sync allow` for an unnamed batch, or `eitri host sync exclude <path>` to leave a path on the server. |
| The laptop folder at the project root was moved or replaced, or an older setup did not record its identity. | Run `eitri host open <host>:<root>` to check and set up the folder. The resume command refuses this pause. |
| There were too many files to list safely when re-creating sync. | Exclude large directories with `eitri host sync exclude <path>`, then run `eitri host sync resume` to retry both listings. |

`eitri host sync flush` waits for a sync cycle to finish. Check `eitri host sync status` for entries that cycle could
not carry.

## Git on the laptop

The agent and the real repository are on the server. Your laptop's `.git` is a **read-only copy of the server's
history**, so that gitsigns, fugitive's status and blame, a statusline's branch name and the like read it locally and
fast. The history keeps arriving after the tree is in; until it is complete the band shows its progress and git
features report that history is arriving.

Whoever can write the server's `.git` (the agent, or anyone with that account) chooses what the copy holds, and a
repository's config and hooks run programs. So the copy is built so that the server cannot run anything on your
laptop through it. It does **not** copy: `config`, `hooks`, `attributes`, `alternates` and lock files (branches and
tags with those names are copied); submodule and linked-worktree git directories; a rebase or similar operation in
progress; or any symbolic link inside `.git`. If the laptop folder already has a `.git` of your own, Eitri leaves it
alone and git commands run on the server until you move it out.
Eitri writes the laptop's `.git/config` itself from a short list of settings it reads from the server (remotes,
branch tracking, `user.name` and `user.email`), and every git command Eitri runs on the copy ignores hooks, fsmonitor,
external diff and textconv. If the project's `.git` is a file (a linked worktree or a submodule checkout) or uses
alternates, nothing is mirrored, and every git command is forwarded to the server; the band says
"git history is not mirrored (`<why>`); git commands run on `<host>`".

### The git shim

A commit on the copy would be undone by the next sync, and a checkout there would change the tree and then
travel to the server as if you had edited it. So in the window's Neovim, and in any Neovim running `eitri.nvim` on a
remote project, a `git` that Eitri provides comes first on `PATH`:

- **Reads run locally** on the copy: `status`, `diff`, `log`, `show`, `blame`, `grep`, `rev-parse`, `ls-files`,
  `cat-file`, `for-each-ref`, `merge-base`, `describe`, the read-only forms of `branch`, `tag`, `remote`, `stash` and
  `config`, and the like. They run with `--no-ext-diff --no-textconv`, so no helper program runs.
  The copy leaves out each remote's `HEAD` (`origin/HEAD`), so on the laptop Git cannot resolve `origin/HEAD`.
- **Writes go to the server.** `add`, `commit`, `checkout`, `switch`, `restore`, `reset`, `stash`, `merge`, `rebase`,
  `pull`, `push`, `fetch`, `cherry-pick`, `revert`, `apply`, `rm`, `mv` and the other listed forms run on the server,
  in the matching directory, with your input and output and the server's exit code. Before the command goes, Eitri
  flushes your saves and checks for conflicts and sync errors affecting the named paths. Whole-tree commands
  check every reported path. If the sync tool omitted conflicts or errors, Eitri refuses the command. It also
  refuses a write that names an excluded path or a path inside an excluded folder, and a write whose paths it cannot
  check (a glob pattern, or `--pathspec-from-file`). Only the paths you name are checked: if you name a folder that
  holds excluded files, or run a whole-tree command such as `git add .`, `git commit -a` or `git stash`, Git on the
  server acts on the server's copies of those files. To work with the server's copy, run Git in a terminal on the
  server. Forwarded reads can inspect excluded paths. After a command runs, both sessions
  are flushed, so a plugin that ran `git checkout` reads the result. If sync is not in a state to flush, the
  command is refused with "nothing was run". Nothing can answer a prompt there: no editor opens, and a command that
  asks for a password or passphrase fails instead.
- **Anything else is refused**, with one line: `git clean`, `gc`, `clone`, `init`, `submodule`, an alias and any form
  Eitri does not recognise print "git `<subcommand>` is not run from here; run it on the server". The shim never guesses.
- **Commit from the terminal, or with `-m`.** A form that needs an editor (`git commit` with no message, `-e`, `-p`,
  `rebase -i` and similar) cannot reach the server's editor, so it answers "commit from the terminal (it runs on
  example-host) or with -m". A commit-message buffer cannot work here: give the message with `-m`, or commit in the
  terminal. If a forwarded command's outcome is not known (the link broke), the shim says so and asks you to check `git status`
  on the server rather than repeating it.

A plugin that runs git by its full path (`/usr/bin/git`), or a terminal without the shim first on its `PATH`, runs
git directly on the laptop copy. Two laptop-local hooks protect that copy of the server's history. The `pre-commit` hook
stops an ordinary commit early. The `reference-transaction` hook refuses ref updates, including creating a branch or
tag and committing with `--no-verify`. Plain `git stash` stops before it resets your edited files. Plain `fetch`
and `push` are also blocked by the copy's configuration. Commands that edit the working tree can still change
files that sync to the server: `reset --hard`, `checkout <branch>`, `cherry-pick`, `revert`, `rebase` and `stash pop`
change files before their ref update is refused, so check `git status` on the server after one. Use the shim or a
terminal on the server for Git writes.

One remaining risk to know about: if a `.gitattributes` in the tree names a `filter` or `textconv` that **your own
global git config** defines, the laptop runs it, because your global git config is still read. A server cannot define
such a filter through the copy, but it can write the `.gitattributes`. Keep filters out of your global config if you
open projects from servers you do not fully trust.

## What the mirror holds, and what it trusts

The mirror is a full copy of the project's non-ignored files on your laptop's disk, so your backups, indexers and any
other tool that reads that folder see them too. What the ignore rules leave out stays on the server, which is why a
secret that is not ignored (or a `.env` in a project without git) is copied; [`exclude`](#exclude-and-include) keeps one
off. A file that becomes ignored later keeps its laptop copy until you delete it; the sync view lists such paths as
"no longer synced; this laptop keeps its copy".

The files also come from the server, and your own tools read them. Eitri closes the routes it controls (see the git
section). A project-local Neovim config the server writes into the tree, such as `.nvim.lua`, runs in your Neovim if you
turned `exrc` on; Eitri does not change that setting. Treat a server whose account you do not control the way you
treat any checkout from it.

## Conflicts

Sync is two-way and cautious: it never overwrites a file that both sides changed. If you and the agent each edit the
same file before it syncs, the copies diverge and both stay as they are. The panel's status band shows `⇅` and
the host with the conflict count in the error colour, an open buffer with a conflicting file gets a sign in the sign
column and a notice line, and `:EitriSync` opens the sync view for the current project. Any Neovim running `eitri.nvim`
has it, not only an Eitri window.

The view lists, for each conflict, what each side holds (a file, a missing file, a link...), plus the paths that could
not sync and why, special files, "copied back" entries, copies the laptop keeps and the ignore rules. On a conflict:

- **Keep the server's.** Removes the laptop's file; sync then brings the server's down.
- **Keep the laptop's.** Removes the server's file; the laptop's goes up.
- **Merge.** Opens both in a Neovim diff, the server's version read-only; `:w` of the result writes the same bytes on
  both sides, which clears the conflict. It is offered when both versions are 8 MiB or less. Wiping the server's buffer
  ends the merge, and later writes save only locally.
- **Dismiss** acknowledges a "copied back" entry; **dismiss all** names the project and keeps every file on both sides.

The same actions run from a terminal as `eitri host sync resolve <path> --keep server|laptop` and
`eitri host sync dismiss <path>|--all`; `eitri host sync conflicts` lists them.

Every action that removes or replaces a version first saves a copy of it under
`$XDG_STATE_HOME/eitri/remote/conflict-backups/<key>/<time>-<id>-<laptop|server>/<path>` (default
`~/.local/state/eitri/...`), readable by you only. The paths are printed when it happens, and copies are removed after
30 days.

Eitri will not act on a conflict when acting would be unsafe, and says why: while sync is paused or about to re-create
a session, while an ignore change is waiting, when sync is not running, when the conflict is no longer the one you
were shown, and while **an agent turn or background task is running in that project** or Eitri cannot tell (a choice
made now would land in the middle of the agent's changes). It also does not touch a conflict where either version is a
different kind of thing now (a directory, a link, a special file) or larger than 64 MiB; for those the view lists the
paths and you settle them by hand. If a decision is being made when you try, it says "sync is updating; look again".

### An edit beats a delete

Not every disagreement is a conflict. If one side deletes a file and the other edits it, the **edit wins** and the file
comes back on the side that deleted it, with no conflict and no sign. That is how the sync tool works, so Eitri cannot
show it as a conflict; the sync view's help says so. A file you deleted on the laptop that returns was probably edited
on the server.

The same applies when a session is re-created after an ignore change: a file you had deleted on one side, still present
on the other, is copied back to where you deleted it. Eitri lists those files ("`<n>` files were copied to the
other side when sync was re-created; if you had deleted one, delete it again") so you can delete them again.

## Before a prompt is sent

You save a file and send a prompt. The save reaches the server a moment later, and a prompt that leaves at once lets the
agent read the old file, and the edit would be charged to the agent's turn. So on a remote tab, **sending a prompt or a
steer first waits for your saves to reach the server**, for at most `remote_dev.flush_cap` seconds (3). The composer
shows "syncing…" meanwhile, and the wait never blocks the window. `remote_dev.send_waits_for_flush = false` turns the
wait off.

The prompt always goes. If the wait runs out, a saved path is in conflict, excluded or affected by a sync error, or sync is not running, a line
under your message names the files and the host, for example "your last save of `src/main.rs` has not reached
`example-host`", with the reason for an exclusion or an unavailable sync. **The prompt itself is never changed**: the
agent receives exactly what you typed, and the line stays with the message in the saved tab. When an agent's proposed edit
is applied through your editor, Eitri waits for the server to hold the file's new content even when send waits are
off; if it has not arrived Eitri says "the write to `<path>` has not reached `example-host` yet; it is saved on this
laptop". Files over 64 MiB are reported as too large to confirm.

## Permission cards on a remote tab

The agent of a remote tab works on the server, so the paths it names are the server's. Eitri's helper checks them
there, on the server's disk, for each request the agent makes: whether the path stays inside the project, whether a
link on the way leads out of it, and whether the project folder is a boundary at all (yours, and not writable by other
accounts). The panel then answers as it does for a local project of the same shape:

- **Manual**: reads inside the project and read-only commands go through; edits, and anything outside the project or
  through a link that leaves it, ask you.
- **Auto**: at a project folder the server calls a boundary, Eitri leaves the call to Claude Code's own auto mode, as
  it does locally. A model that cannot run auto mode turns the tab to Manual.
- **Bypass**: as on a local tab.

Before Eitri answers a request by itself, it asks the server once more (a fresh check, at most about three seconds).
If the answer changed in between, for example a file inside the project was swapped for a link to one outside, you
get a card instead. A request that arrives while the connection is down is answered the same way once it is back,
after its fresh check.

When the server cannot check paths for a tab, the tab says "this tab runs on `example-host`; path checks on the server
are not available yet" and asks you about every file and command. That is the case for an agent started by an older
Eitri whose helper still runs on the server, for a tab you just moved to another window, and for a tab that came back when
its window was reopened, until Eitri has checked the project on the server for it, which it does as the tab comes back. A project folder that is not a boundary says "No project boundary on `example-host` (...)" and asks about
every file and command, as a local one does.

## Sessions on the server

The session chooser (`Ctrl+b w`) of a remote window lists the sessions Claude Code recorded on the server for the
project, newest first, with their titles: those your remote tabs started and those you started yourself with `claude`
in a terminal on the server. Choosing one continues it, with its history read from Claude Code's own transcript on the
server. A session whose transcript is still being written (Claude Code is still working in it somewhere) is refused.

`@buffers`, `@quickfix` and `@diagnostics` on a remote tab follow the server's `Read` deny rules: the project's and
your account's Claude settings on the server, not the ones on your laptop. When those cannot be read, nothing from the
editor is attached and the message says why.

## Syncthing and Dropbox

Do not let another sync tool carry the same folder. Two tools writing one tree race each other: Syncthing's
`.sync-conflict` files and its `.stfolder` and `.stignore` land in your tree, where an agent's `git add -A` can commit
them.

At `eitri host open`, Eitri walks from the project up to `/` on the laptop, and does the same on the server, looking for
`.stfolder`, `.dropbox` and `.dropbox.cache`. When it finds one, it stops and says which path is affected, what would
happen on the other devices, and that you can use a path outside the synced folder on both machines.

For Syncthing there is one way to keep the folder where it is: an exclusion in that Syncthing folder's `.stignore`,
anchored and literal, for the project or one of its parents. If Syncthing syncs `/srv` and the project is
`/srv/project`, the line is

```text
/project
```

Eitri accepts the exclusion only when it can prove that nothing in `.stignore`, nor in a file it `#include`s,
re-includes anything of the project. An exception it cannot read or match with certainty counts against it. Dropbox
markers always need a different location or removal of the overlap. The check runs when you open a project with
`eitri host open`.

## Language servers and Neovim

The sections above keep your editing on the laptop. A language server, a formatter or a test runner is different: it
needs the project's toolchain, and the laptop may not have one. So in a remote project Eitri runs them on the server
and lets your Neovim talk to them from the laptop. You do not change your Neovim configuration for the language
servers: lspconfig, `vim.lsp.enable` and `vim.lsp.start` all go through the same path.

### Language servers on the server

Open a file in a remote project, and the language server your configuration would have started on the laptop starts
on the server instead, in the project's directory, with that account's login environment. Diagnostics, completion,
hover and the rest show up in Neovim as before. Your buffers go to the server as you type, so completion sees text
you have not saved.

- **The server needs the language server installed**, not the laptop. Eitri looks for the command on the account's
  login `PATH`, then in Mason's directory there (`~/.local/share/nvim/mason/bin`). If it is not found, Neovim's server
  fails to start with one message, for example `rust-analyzer is not installed on example-host (looked on the login
  PATH and in mason)`. Eitri's side of that server lets go of its connection at once and ends by itself about ten
  seconds later if Neovim does not stop it.
- **One server per Neovim.** Two Neovims on one project start two servers on the server.
- `cmd_env` and `cmd_cwd` in a server's configuration describe the laptop, so they are not applied on the server.
- **If the server dies**, Eitri starts a fresh one and sends it your open buffers. If it dies a second time within
  five minutes, Eitri stops trying and says so: `the language server rust-analyzer on example-host stopped twice`,
  followed by the end of its error output.
- If the helper on the server is an older one that is still running and does not know language servers, the language
  server runs on the laptop instead. The line saying so goes to Neovim's language-server log (the file
  `vim.lsp.log.get_filename()` names), not to the screen. It runs without the configuration's `cmd_env` and
  `cmd_cwd`, in Neovim's working directory. Opening the project's window again updates the helper.

### What stays on the laptop

A language server stays on the laptop when

- its root is not inside a remote project;
- its name is in `remote_dev.lsp_local`. The name is the `name` of its configuration, and a configuration with no
  name cannot be listed. `init.lua` stores no tables, so it is one string:

  ```lua
  eitri.config.set("remote_dev.lsp_local", "lua_ls, stylua")
  ```

- its `cmd` is a Lua function. A function builds its own connection, so Eitri cannot put the server on the other
  side of one. `:checkhealth eitri` warns about it (see [Plain Neovim](#plain-neovim));
- you set `vim.g.eitri_remote_lsp = false` (for the whole session) or `vim.b.eitri_remote_lsp = false` (for one
  buffer). These are Neovim variables, not `eitri.config` keys, so they go in your Neovim configuration.

The choice is made when a server starts, so changing it does not move a server that is already running. A plugin that
saved its own reference to `vim.lsp.start` before Eitri's setup ran, and `vim.lsp.start_client`, are not covered.

### The grace

A closed lid, a change of network or a dropped ssh connection does not stop the language server. The server keeps it
for `remote_dev.lsp_grace` seconds (60 to 86,400, default 1,800, so 30 minutes), and Neovim reconnects by itself.
When the connection is back within the grace, Neovim is talking to the same server: your open buffers and the saves
you made meanwhile are sent again, and what the server said while you were away (diagnostics, progress) arrives.
**Nothing is indexed again.** While the connection is down, requests such as completion and hover fail at once
instead of waiting.

If the grace runs out, the server stops; the next connection starts a fresh one, which indexes again. Closing Neovim
asks the server to stop its language server straight away when the connection is up; with the connection down, the
language server ends when the grace runs out. `eitri host stop` stops the language servers too. Neovim's clients for
them quit and do not start new ones until Neovim starts them again, for example when you open a file of the project.
A server kept by the grace keeps using the server's memory until it ends, so a
shorter grace suits a small machine:

```lua
eitri.config.set("remote_dev.lsp_grace", 600)
```

### Saves reach the server first

When you save, Eitri asks sync to carry the file to the server at once, and the language server is told once the saved
file has arrived, so a check that runs on save (rust-analyzer's `cargo check`, for one) reads what you saved. Usually
that takes about as long as one round trip to the server.
If the file has not arrived within about five seconds, the save goes to the language server anyway, and the check can
read the older file; the helper notes it in that language server's log on the server. A save is held for at most about
six seconds. `eitri host sync status` shows what sync is doing.

### `gd` into dependencies

A language server also names files that sync does not carry: the standard library, a crate under `~/.cargo`, a file
generated under an excluded directory. The laptop's file at such a path is some other file, or none. So Eitri opens
these as `eitri-remote://example-host/<path>` buffers, read from the server:

- The buffer says `reading from example-host…` and then shows the text. It is **read-only**: `:w` answers `this file
  is on example-host and read-only here`.
- The file type is detected from the path, and the language server you came from attaches, so hover and `gd` keep
  working inside it. That holds for `gd` and for a file you open yourself or from the panel: the servers of the buffer
  you were in attach if they handle the file's language, and if that buffer has none, a server of the same host that
  already runs for that language does. Only a language server of that same host attaches to such a buffer; one your
  configuration would start on the laptop for the file type does not.
- A file inside the project that an ignore rule excludes opens this way too, even if the laptop holds an old copy.
- A file larger than 16 MiB is refused, not shown in part. At most four are read at once; the rest wait their turn.
- To open one yourself, `:EitriRemote open /home/user/.cargo/registry/src/example/lib.rs` (an absolute path on the
  server of the current buffer's project; typed inside an `eitri-remote://` buffer, on that buffer's host). In a remote window, the panel's open-path does the same for an absolute
  path outside the project. `gf` stays Neovim's own.
- `vim.b.eitri_remote` is `{ host, path }` in these buffers, for a plugin that must not treat one as a local file.

From a terminal, `eitri host read example-host:/absolute/path` prints a file from the server.

### Commands on the server

Formatters, linters, test runners and `make` are started by Neovim on the laptop. `eitri host exec` runs one command
on the project's server instead:

```sh
eitri host exec --cwd /srv/project -- cargo fmt
```

The project is the recorded one that holds `--cwd` (default: the current directory), and the command runs in the same
path on its server. Eitri flushes your saves to the server before it starts and brings back what the command changed
when it ends, so a formatter that rewrote a file in place has its result on the laptop when the command returns (a
buffer's `:checktime` sees it). `--no-flush` skips both. If a flush fails the command still runs, with a line on
stderr saying your last save may not have reached the host (or that the command's changes may not have reached the
laptop). Standard input goes to the command unless you give `--no-stdin` or it is a terminal, so a formatter that
reads stdin and writes stdout works.

The command gets the login environment of the server account with `TERM=dumb`, and nothing from your laptop. Nothing
can ask it a question: an editor or a password prompt fails instead of opening. The program is a bare name found on
the login `PATH`, or an absolute path; otherwise you get the line "`<name>` is not on the login PATH of this account".

What you meet:

- **Exit code.** The command's own; `128 + n` when a signal `n` ended it. **125** means nothing was run: the
  directory is in no remote project, the server refused, or its helper is older than this Eitri (open the project's
  window to update it). **255** means the server could not be reached, or the outcome is unknown because the
  connection was lost after the start; look on the server before you run it again. Eitri's own lines go to stderr
  starting `eitri host exec:`. It uses 125 rather than 1 so that a linter's "issues found" exit never reads as Eitri
  failing.
- **A command is killed after 1 hour.** Interrupting `eitri host exec` on the laptop does not stop a command whose
  input was already closed (the Neovim wrappers close it at once); only that limit does.
- It works whether or not `remote_dev.enabled` is on, for a project you have opened.

### Exec wrappers for plugins

`require("eitri.remote").cmd(argv, opts)` takes an argument list and returns it wrapped in `eitri host exec` when the
directory is inside a remote project, and unchanged otherwise, so one configuration serves local and remote projects.
`opts` takes `cwd` (default: the current buffer's directory), `flush = false` for `--no-flush`, and `stdin = true`
for a command that reads standard input. A command with `stdin = true` reads the buffer and answers on standard
output, so it skips both flushes too (a round trip each); add `flush = true` if it also reads or writes files of the
project. The one-line recipes:

- **conform.nvim.** Make `command` and `args` functions of the wrapped list. For a formatter that reads the buffer
  from standard input, pass `stdin = true` to `cmd` and set it on the formatter too:

  ```lua
  local function on_server(argv)
    return require("eitri.remote").cmd(argv, { stdin = true })
  end
  local rustfmt = { "rustfmt", "--edition", "2021" }
  require("conform").formatters.rustfmt = {
    inherit = false,
    command = function() return on_server(rustfmt)[1] end,
    args = function() return vim.list_slice(on_server(rustfmt), 2) end,
    stdin = true,
  }
  ```

- **nvim-lint.** `linters.<name>.cmd` is a function that returns the first word of `cmd{ ... }`, and `args` is the
  rest. A linter with `stdin = true` reads the buffer from standard input, so build its list with
  `cmd(argv, { stdin = true })`: by default the wrapped command runs with `--no-stdin` and reads empty input.
- **neotest.** For an adapter option that takes a command, pass `cmd{ ... }`.
- **`:make`.** `:EitriRemote makeprg cargo build` sets this buffer's `makeprg` to run the command on the server in
  the buffer's directory (`make` if you give none); then `:make` fills the quickfix list as usual. `errorformat` is
  left alone: the paths in the output are the server's, and they equal the mirror's. Outside a remote project
  `:EitriRemote makeprg` says so and changes nothing.

Where a list is built before any buffer is current (nvim-lint's `args`, a neotest option), pass `cwd` yourself.

### Plain Neovim

None of this needs an Eitri window. A Neovim in an ordinary terminal needs three things:

1. The `eitri.nvim` plugin on its `runtimepath` (`vim.opt.runtimepath:append("/usr/share/eitri/nvim/eitri.nvim")`;
   `:help eitri-install` lists the other install paths).
2. Optional, for the two settings above that a window reads from `init.lua`:

   ```lua
   require("eitri.remote").setup({
     lsp = { grace = 1800, local_servers = { "lua_ls" } },
   })
   ```

   Without it the defaults apply: grace 1,800 seconds, nothing kept local. A grace outside 60 to 86,400 is clamped.
3. The project opened once, in a shell: `eitri host open example-host:/srv/project` (with `remote_dev.enabled` on in
   Eitri's `init.lua`).

After that, a file under the project's local copy gets its language servers on the server, `gd` into dependencies,
the exec wrappers and the sync watch, as in a window. The sync watch reads `remote_dev.enabled` from Eitri's `init.lua`
each time it starts: if this Neovim sees an `init.lua` without it (or none), the watch says "remote development is
off" and only the language servers and the exec wrappers work. `:help eitri-plain-remote` says the same.

`:checkhealth eitri` has a section for the buffer it was typed in. Outside a remote project it is one line. Inside one
it shows the project (host and root), the `eitri` program and its version, whether `remote_dev.enabled` is on, each
language server attached to the buffer as **remote**, **local and pinned**, **local because its `cmd` is a function**
(a warning) or plain local, and Mutagen with its version (a warning below 0.18.0 or when it is not found).

### A server shell in Neovim

`:EitriRemote term` opens a shell on the project's server in a terminal buffer of a new tab, in the current buffer's
directory (the same path there). It looks as it would over ssh: `vim`, job control and resizing work. Outside a remote
project nothing opens. It runs `eitri host term`, which you can also run in any terminal.

- Wiping the buffer asks the server to close the shell, **on a best-effort basis**: if the server does not answer
  within about a second and a half, the shell keeps running, and `eitri host list` shows it.
- If the connection is lost, the shell keeps running on the server and the terminal reattaches by itself for up to two
  minutes; after that it gives up and says the shell is still running.
- It is a server terminal like those in the window; [Server terminals](#server-terminals) says what closing one
  ends.

## What is not in this version

- **macOS laptops.** The mirror's exclusions match names exactly, and a macOS file system ignores case by default;
  until that is settled, use a Linux laptop.
- **Git for worktrees and submodule checkouts as the project root.** Not mirrored; git runs on the server.
- **Git inside a submodule of the project.** The copy holds no submodule history, so a git read in a submodule's
  folder describes the outer repository; run git for a submodule in a terminal on the server.
- **Git commands the shim lists as refused, and interactive editor flows** through git (see above).
- **Agent tabs on the local copy.** A window opened with `--local` shows the copy; agents run on the server.
- **Turn review on remote tabs.** A remote tab's turn review says it is not available yet.
- **Sparse editing and a remote Neovim.** The whole non-ignored tree is mirrored, and Neovim runs on your laptop.

For what else is unsupported or rough today, see [Known issues](../known-issues.md).
