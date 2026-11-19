English | [简体中文](remote-development.zh-CN.md)

# Remote development

Remote development lets one Eitri window on your computer work on a project that lives on another machine. The code,
the agent and its tools stay on the server, where the toolchains and credentials are. Your Neovim, its plugins, the panel and your
keys stay local. This page covers the sync side: how the project reaches your computer, what does not, and what
Eitri does when the two copies disagree.

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

`eitri host list` shows the helper's running agents on every host you opened; `eitri host stop example-host` stops
them (and pauses sync for that host's projects); `eitri host uninstall example-host` removes the helper. They work
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

## What is not in this version

- **macOS laptops.** The mirror's exclusions match names exactly, and a macOS file system ignores case by default;
  until that is settled, use a Linux laptop.
- **Git for worktrees and submodule checkouts as the project root.** Not mirrored; git runs on the server.
- **Git inside a submodule of the project.** The copy holds no submodule history, so a git read in a submodule's
  folder describes the outer repository; run git for a submodule in a terminal on the server.
- **Git commands the shim lists as refused, and interactive editor flows** through git (see above).
- **Agent tabs on the local copy.** A window opened with `--local` shows the copy; agents run on the server.
- **Sparse editing and a remote Neovim.** The whole non-ignored tree is mirrored, and Neovim runs on your laptop.

For what else is unsupported or rough today, see [Known issues](../known-issues.md).
