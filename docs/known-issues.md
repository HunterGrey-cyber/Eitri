English | [简体中文](known-issues.zh-CN.md)

# Known issues and limits

What is unsupported, rough or not yet measured in the current 0.x release. Please read it before filing a
bug, and tell us if something here is wrong or has changed. The requirements are also on
[eitri.cc](https://eitri.cc/#requirements); installing is in [INSTALL.md](../INSTALL.md).

## Where it runs

- **Linux on x86_64, Wayland.** X11 is untested (a report from X11 is still welcome). macOS is in progress.
  Windows is not supported; WSL2 with WSLg may work, untested. There is no ARM build yet.
- **Tested on GNOME and sway.** Other desktops and compositors, KDE Plasma and Hyprland among them, have not
  been tested; reports from them are welcome.
- **WebKitGTK 2.40 or newer** (the `webkitgtk-6.0` API) is required. The prebuilt binaries are built against
  glibc 2.39 and GTK 4.14; older versions are untested. Ubuntu 24.04, Debian 13, Fedora 40+, RHEL 10 and Arch
  meet this; Ubuntu 22.04 and Debian 12 do not.
- **Neovim 0.10 or newer**: your own on `PATH`, or a private copy the installer offers to fetch.
- **Claude Code**, installed and logged in, 2.1.252 or newer and below 3.0. Without it Eitri still opens and
  the editor works; the agent panel cannot run turns.

## Installing

- **The agent sidecar is built on your machine** during the install (network access and about 600 MiB of free
  disk space; it fetches Node.js and npm packages). The AUR package `eitri-bin` does the same inside its build, so
  it takes minutes, not seconds. Why: [INSTALL.md](../INSTALL.md#why-the-sidecar-is-built-on-your-machine).
- **`.deb` and `.rpm` need one `eitri setup`** afterwards, as your own user; neither package builds the sidecar.
- **Ubuntu 23.10 and later, 24.04 included:** AppArmor blocks WebKit's sandbox by default. Eitri checks at
  startup; the editor and the terminal still work, and the agent panel shows the one-time fix in its place. The
  fix needs `sudo` and has a cost, both spelled out in
  [INSTALL.md](../INSTALL.md#ubuntu-2310-and-later).

## Speed and drawing

- Below GTK 4.16 the editor draws another way, and its typing latency has not been measured there.
- While an agent reply streams, about 6-9 % of key presses in the editor took two refreshes instead of one
  (Intel laptop, GTK 4.22.5, a stand-in stream at the default 5 updates a second, 165 Hz and 60 Hz). A real reply
  has not been measured. If you see lag while typing during a reply, say so in the "Testing feedback" form, with
  your GPU and monitor refresh rate.

## Reaching eitri.cc and GitHub from mainland China

Users on some Chinese ISPs (reported so far: China Telecom in Fujian, Jiangsu and Henan) say their connections to
foreign sites that are not on the ISP's whitelist are reset, so eitri.cc and github.com may not open at all
there. The installer downloads from GitHub, so it can fail the same way. If you can get the release files another
way, `sh install.sh --tarball FILE --sums FILE --sig FILE` installs from them
([INSTALL.md](../INSTALL.md#quick-install)); the sidecar build still needs a network that reaches Node.js and npm.

## Companion mode

Companion mode is the agent panel as its own window beside your own nvim
([INSTALL.md](../INSTALL.md#use-it-beside-your-own-nvim)). It is new, and less of it has been tried than of the
one-window mode.

- **Only sway with a terminal nvim has been tried.** Hyprland, niri, GNOME and upstream Neovide (or another nvim
  GUI) as the host are not yet seen on real hardware. On Hyprland and niri the edge behaviour is the window
  manager's own; Eitri does not check it.
- **Inside tmux** the edge of tmux's panes stays tmux's, so there is no crossing from nvim to the panel window with
  `Ctrl+h/j/k/l` unless you add a binding on the tmux side, and opening a file from the panel does not raise the
  editor's window (the process tree from nvim leads to the tmux server, not to the terminal).
- **A tmux server started before your window manager session** keeps that session's old `SWAYSOCK` or
  `HYPRLAND_INSTANCE_SIGNATURE` in its environment, and a panel started from an nvim in it inherits the stale value:
  focus moves and raising then do nothing. Restart the tmux server after logging in again, or start the panel from a
  shell that has the current session's environment.
- **`foot --server` clients share one pid**, so raising the editor by its process can bring up another foot window
  of the same server instead of the one that holds your nvim.
- **At sway's edge the key is consumed.** Eitri has to take or release `Ctrl+h/j/k/l` before it can ask sway
  whether a window lies that way, so at the edge the key does nothing, as tmux's own `select-pane` does at its edge.
- **KDE: no focus moves.** A Wayland client cannot take focus there; use the desktop's own window keys.
- **GNOME needs the Eitri GNOME Shell extension** for focus moves, enabled by you and loaded at your next login
  ([INSTALL](../INSTALL.md#gnome-the-extension)). With it: moves stay on the focused window's monitor; an X11 or
  XWayland window never counts; moving back within about a third of a second of the previous move is refused; a
  terminal that runs all its windows in one process (GNOME Terminal, Ptyxis) gets focus handed back to its most
  recent window, which may not be nvim's; a forwarded `eitri split` does not bring the panel forward; and the
  supervisor's "bring to front" does nothing.

## Security

- **Eitri asks before it loads a project's own Claude configuration, and the step has limits.** Until you trust a
  project, its agent sessions load only your own user settings, not the project's `.claude/settings.json`,
  `.claude/settings.local.json`, `.mcp.json` or `CLAUDE.md`, so a repository you did not write cannot start its own
  hooks or MCP servers, or pre-approve tool calls, at your first message. The question lists what it found, with the
  exact commands. Trust is remembered per project and is tied to what you saw: Eitri asks again after any change to
  `.claude/`, `.mcp.json` or `CLAUDE.md`, including one that terminal `claude` makes (its "don't ask again" rewrites
  `settings.local.json`). It never imports terminal `claude`'s own trust answer, so a project you already trusted in
  the terminal asks once more here. A file Eitri cannot check (larger than 4 MiB, unreadable, a pipe, or a symlink)
  is named in the question, and your answer then covers that one start only. A script a hook calls from outside
  `.claude/` is trusted only through the hook's command text, so trusting a project means trusting its code to run, as
  building it does. Continuing a conversation in a real terminal runs plain `claude`, whose own trust question applies.
  To forget every answer, delete `~/.local/state/eitri/trust/`.
- **The bottom terminal can write to your clipboard, by design.** A program running there can set the clipboard
  or the primary selection with an OSC 52 escape sequence (the same default as Alacritty); Eitri shows no notice
  when it happens, so what you paste next may not be what you copied. Reading the clipboard is refused.
- **The agent panel's script policy is new and was checked in tests, not yet on a screen on every setup.** The
  panel now allows only its own script by hash instead of any inline script. If the agent panel stays blank
  after an update, please report it with your WebKitGTK version.

## Turn review

After an agent turn, `c` in the panel's BROWSE mode lists the files that changed on disk during the turn and their hunks. From there `x` reverts a hunk
or a file to what it was before the turn and `u` undoes that, `i` and `s` send comments and your reverts back to the agent, and `o` draws the hunks over the
file in your nvim.

- **It keeps a copy of your project's files.** To tell what a turn changed, Eitri takes a snapshot before and after each turn into its own
  repository under `~/.local/state/eitri/review/` (directories 0700, files 0600). It copies every file your ignore rules do not exclude, up to 8 MiB
  per file and 20,000 files, so a project with large generated files outside `.gitignore` grows that store. Delete the directory to drop everything;
  the next turn starts afresh. It keeps the newest 100 turns per project for 30 days. Nothing is written into your project, and your own `.git` is
  never written: Eitri only asks git where your repository's git directory is and reads its `info/exclude`, so that what you exclude there is not
  copied either.
- **"Changed on disk during this turn" is not "the agent changed".** A change you made by hand, or one made by `Bash` or another tab, shows as `?`
  next to the agent's own edits (`✓`); a turn that overlaps another one is labelled.
- **A revert keeps the bytes it replaced** in that same store, under `~/.local/state/eitri/review/`, for 30 days, so `u` can bring them back.
- **Another program writing the same file at the same instant can lose its write.** A revert checks the file just before it writes, but no lock exists
  that other programs honour; `u` restores what was there.
- **A window with `review.enabled = false`, or one on the legacy backend, still blocks reverts from another window.** Every Eitri window on a project
  holds a small lock file while it is open, so one window cannot write under another window's unsaved buffers.
- **A revert whose file sits behind a symbolic link to a directory is refused,** whether the link leads inside the project or outside it.
- **An interrupted revert is offered back only in a window with turn review on.** If the window was killed in the middle of rewriting a file in place (one
  with a second hard link, another owner or extended attributes), the next window with turn review on offers to restore the saved bytes.
- **Limits:** a file over 8 MiB is listed as too large with no patch; a patch over 2,000 lines shows only its counts; a file name that is not valid
  UTF-8 is listed but its patch cannot be opened; a project inside a larger repository (opened below the repository's top level) honours neither
  the parent's `.gitignore` files above its own root nor the repository's `info/exclude`, so files excluded only there are copied into the store.

## Stability

0.2.0 was the first public release. The `init.lua` API and the default keys may change during 0.x; the goal for
0.3 is a version stable enough to be our own everyday editor.

## Not on this list?

[Open a bug report](https://github.com/HunterGrey-cyber/eitri/issues/new/choose), or ask in
[Discussions](https://github.com/HunterGrey-cyber/eitri/discussions) first if you are unsure it is one.
