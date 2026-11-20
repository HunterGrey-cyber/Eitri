English | [简体中文](known-issues.zh-CN.md)

# Known issues and limits

What is unsupported, rough or not yet measured in the current 0.x release. Please read it before filing a
bug, and tell us if something here is wrong or has changed. The requirements are also on
[eitri.cc](https://eitri.cc/#requirements); installing is in [INSTALL.md](../INSTALL.md).

## Where it runs

- **Linux on x86_64, Wayland.** X11 has been tried in one session: GNOME on Xorg on Ubuntu 24.04, without GPU
  acceleration. The window, the editor, the agent panel, the bottom terminal and tiling worked there. On X11 the
  editor always draws the GTK way, and the GNOME Shell extension and companion mode's focus moves do not apply to
  X11 windows. Other X11 desktops and window managers, i3 among them, are untested (reports are welcome). macOS is
  in progress. Windows is not supported; WSL2 with WSLg may work, untested. There is no ARM build yet.
- **Tested on GNOME and sway, and companion mode on Hyprland.** Other desktops and compositors, KDE Plasma
  among them, have not been tested; reports from them are welcome. Hyprland: companion mode (focus moves and
  raises) was run on a live 0.57.0 and 0.56.2 (with `hyprland.conf` and with `hyprland.lua`); the second window's
  raises were not.
- **[Compositor windows](guide/compositor-windows.md)**, the default on Hyprland, sway and niri, are new, and some
  things do not work as they should yet: the guide's
  [known gaps](guide/compositor-windows.md#known-gaps) list them.
- **WebKitGTK 2.40 or newer** (the `webkitgtk-6.0` API) is required. The prebuilt binaries are built against
  glibc 2.39 and GTK 4.14; older versions are untested. Ubuntu 24.04, Debian 13, Fedora 40+, RHEL 10 and Arch
  meet this; Ubuntu 22.04 and Debian 12 do not.
- **Neovim 0.11 or newer**: your own on `PATH`, or a private copy (0.12.6) the installer offers to fetch when
  yours is older. A Neovim 0.11 on `PATH` is accepted and is not replaced, and three popular configurations still
  fail on it: the current kickstart.nvim does not start (it uses `vim.pack`, new in 0.12), LazyVim loses Eitri's
  `Ctrl+j` and `Ctrl+k` on the first start to its notification windows, and AstroNvim now and then keeps its own
  `Ctrl+j` (seen once on 0.12.6 as well, so not tied to the version). The same LazyVim and kickstart checks pass on 0.12.6. If you
  use one of them, keep 0.12 on `PATH` or point `EITRI_NVIM` at it.
- **Claude Code**, installed and set up, 2.1.252 or newer and below 3.0. Without it Eitri still opens and
  the editor works; the agent panel cannot run turns.
- **Codex and other agents are optional.** A Codex tab needs `codex` 0.160.0 or newer, installed and set up
  (tested with every release from 0.160.0 to 0.166.0); a tab for any other agent needs an agent that speaks ACP version 1 over standard
  input and output. See [Codex and other agents](guide/agents.md).

## Installing

- **The agent sidecar is built on your machine** during the install (network access and about 1.4 GiB of free
  disk space while it builds; it fetches Node.js and npm packages). The AUR package `eitri-bin` does the same inside its build, so
  it takes minutes, not seconds. Why: [INSTALL.md](../INSTALL.md#why-the-sidecar-is-built-on-your-machine).
- **`.deb` and `.rpm` need one `eitri setup`** afterwards, as your own user; neither package builds the sidecar.
- **Upgrading from 0.2.2 builds the sidecar again**, because 0.3.0 pins a newer one: the same download and disk
  space as a first install, and after a `.deb` or `.rpm` upgrade one more `eitri setup`.
- **A sidecar update can fail in two places** ([INSTALL.md](../INSTALL.md#updating-the-sidecar)).
  `eitri sidecar update` stops with a plain message and changes nothing: usually no network to nodejs.org, npm or
  grpc.io, too little free disk space, a signature that does not verify, a release that needs a newer Eitri, or
  another `eitri sidecar` command running at the same moment. An update that is built but will not start in a tab does
  not break the tab: it runs the sidecar that came with Eitri, its band shows `⚠`, and its details say why. `eitri
  sidecar use bundled` ends the attempts. An update reaches only tabs opened after it, and Eitri 0.3.0 cannot take
  updates at all.
- **Ubuntu 23.10 and later, 24.04 included:** AppArmor blocks WebKit's sandbox by default. Eitri checks at
  startup; the editor and the terminal still work, and the agent panel shows the one-time fix in its place. The
  fix needs `sudo` and has a cost, both spelled out in
  [INSTALL.md](../INSTALL.md#ubuntu-2310-and-later).
- **`v0.2.0` cannot be built from git for much longer.** The Neovide fork's public history was rewritten after
  0.2.0 (commit metadata only; the files are the same), so the `v0.2.0` tag and every earlier commit point at fork
  commits no branch holds. Build 0.2.1 or later. For 0.2.0 itself, 0.2.0's own `install.sh` with `--from-source`
  works only while GitHub still serves those old objects, and the `eitri-0.2.0-source.tar.gz` on its release page
  rebuilds it for good; a later release's `install.sh` cannot build 0.2.0 from source. Packages, tarballs and
  `install.sh` are not affected.

## Speed and drawing

These were measured on development builds shortly before 0.3.0, on an AMD desktop unless a bullet names another
machine. The items below distinguish real refresh-period feedback from the fallback clock.

- **Drawn the GTK way, the editor is one refresh slower on GTK 4.24.** By default it is not drawn that way there: on
  Wayland with GTK 4.24 or newer, unless the graphics driver is NVIDIA's, the editor hands its frames to the
  compositor on a surface of its own ([How the editor reaches the screen](guide/editor-presentation.md)), and a typed
  letter reached the screen 0.02 to 0.46 ms after native Neovide's at the median, on sway and Mutter at 60 and
  165 Hz. The GTK way is still used on X11, with NVIDIA's driver, with `editor.presentation = "gtk"`, and after a
  fallback (the editor prints why). There, with the compositor feeding GTK a real 165 Hz period, GTK 4.24.1 added
  5.97 ms at the median, where the same build with GTK 4.22.5 added 0.30 ms; with GTK's 60 Hz fallback clock the
  addition was about 16.5 ms. A 156-line GTK-only reproduction also shows the delay on GTK's development branch
  (4.25.0, commit c36202f6, 2026-10-25). The editor's own surface has not been tried on NVIDIA or KWin.
- **Drawn the GTK way on GTK 4.24, scrolling with a held `j` at a 165 Hz output drops frames.** Native Neovide drew
  165.5 changed frames a second, Eitri on GTK 4.24.1 140.6, and the same build on GTK 4.22.5 165.4.
  The 99th-percentile frame intervals were 6.1, 12.1 and 6.1 ms respectively. This separate scrolling run
  used stock presentation feedback, so GTK 4.24 still used its 60 Hz fallback clock; scrolling has not been
  repeated with real-period feedback. With the editor's own surface, a held-key test did no
  worse than the GTK way.
- **While an agent reply streams, typing in the editor is slower.** With WebKitGTK 2.54 a key's redraw took 11.2 ms
  at the median while a stand-in reply streamed at the default 5 updates a second, on GTK 4.22.5 as well; with
  WebKitGTK 2.52 it was about 2.3 ms. A real reply has not been measured. If you see lag while typing during a
  reply, say so in the "Testing feedback" form, with your GPU and monitor refresh rate.
- **The bottom terminal draws one output frame behind what you type.** Its echo reaches the screen about 18 ms after
  the key at a 60 Hz output (8.7 ms at 165 Hz), measured on sway on an AMD desktop. Ghostty
  (17.3 ms at the median), WezTerm (16.7) and the terminal in Zed (18.3) land in the same place; foot (1.0), kitty
  (0.7) and alacritty (0.7) draw at once. The cause is that GTK 4.24 makes two commits of the window for each key and
  the compositor shows the second one a refresh later; the terminal draws inside GTK's frame clock, which the editor
  does not. We tried to remove the first commit and typing got slower (about 20 ms at either rate), so nothing ships.
  If you want the lowest latency, run Neovim's own `:terminal` in the editor, which is drawn the way the editor is
  (typing in a buffer took 1.7 ms at the median in the same run). A `:terminal` buffer's own latency has not been
  measured.
- **Memory:** an idle window with one tab uses 232 MiB more than native Neovide when the panel is inside the
  window, and 245 MiB more in compositor mode, where the panel is a window of its own (the default on a tiling
  compositor). Both are under the 256 MiB we aim for. That was measured on a development build after 0.3.0, on an
  AMD Radeon RX 9070; on the integrated GPU beside it the window used 248 to 259 MiB more. The first
  Claude tab adds about 85 MiB, its sidecar included (79 MiB of it), and each further tab about 63 MiB (about 55 of it
  its own sidecar). Each Claude session also runs the `claude` CLI you installed. That is your program, not part of
  Eitri: it used about 109 MiB per session once idle, more during a turn, on top of these figures.
- **In 0.3.0 an idle session's sidecar keeps growing; the next release fixes it.** With seven sessions left idle,
  each sidecar grew 30 to 47 MB an hour, with no plateau in three hours. A forced garbage collection every five
  minutes did not slow it, so it is memory the sidecar still holds, not memory it has yet to collect. At that rate a
  window with five tabs left open for a week would reach about 30 GB (extrapolated, not run). Quitting the window
  gives it back. The release after 0.3.0 bundles a sidecar that stays flat at rest: in the same 25-minute test of one
  idle session, its memory held at 102.6 MB from the fifth minute, where 0.3.0's grew 22 MB an hour (its JavaScript
  heap 44 MB an hour, against 0.3 for the new one). In a window on a development build after 0.3.0, four idle
  sessions' sidecars held 248 MiB between them for 30 minutes, each within 0.1 MiB. 0.3.0 cannot take the fix as a
  sidecar update.
- **At high refresh rates under load the editor drops frames.** At 165 Hz on an Intel laptop, with other work
  keeping the machine 17-28 % busy, it drew 137-149 frames a second where native Neovide drew 160, and 111 at 34 %
  busy, with one 83.6 ms frame in four runs. That was measured before 0.2.0's typing-latency fix and not since.
- Below GTK 4.16 the editor draws another way, and its typing latency has not been measured there.

## The window

- **In immersive mode a card can wait unseen.** When a card arrives in a chat that is not on screen, Eitri shows a
  toast and `agent ⚑N` in the top bar. Immersive mode (`<prefix> F11`) hides the top bar, so once the toast has faded
  nothing on screen says a card waits; `Ctrl+k` shows the bar with its `agent ⚑N` again.
- **A second Eitri window on the same project keeps its own layout and saved tabs.** It starts from the first
  window's layout, then saves only its own, so the first window always opens with what you left in it.
- **Under a light colourscheme the window starts dark for a moment**, then turns light: the top bar, the panel,
  plugin pages, the terminal and the editor's placeholder.
- **`<prefix> ?` pressed in the editor** moves the keys to the panel without drawing the key sheet; `?` in the
  panel then shows it.
- **A relative link in a reply** is underlined like a link, but `gx` and HINT do not offer it.
- **On i3 or sway with `$mod = Alt`** the window manager takes the prefix's `Alt` rows before Eitri sees them;
  [`docs/keymap/i3-alt-mod.lua`](keymap/i3-alt-mod.lua) moves them.
- **On Neovim 0.12, `:restart` and `:connect` typed in the editor move it** instead of ending it: `:restart` to the new
  Neovim it starts (no stray `nvim --embed` is left behind), `:connect <address>` to that server. A `:connect` leaves
  the editor's own Neovim running with whatever it had open; closing the window names its address, and `:connect!`
  stops it. A `:connect` to an address that does not answer keeps the editor where it was. In a Neovim you
  [took over from a terminal](guide/takeover.md), `:connect` gives it back to the terminal and closes the window, and
  `:restart` ends like `:qa` for now (the window closes; the new server runs with no window, at the address its log names).

## Pages you build

Lua panels and plugin pages ([Pages you build](guide/pages.md)) are **experimental**: the plugin manifest and the
page bridge may change during 0.x.

- **Loading a plugin trusts it.** There is no consent screen yet: `eitri.plugin.load` in your `init.lua` gives the
  plugin every power its manifest lists, and one with tasks runs programs as you. Load only plugins you trust, as you
  would run only programs you trust. A change in a plugin's list of powers is logged at the next start, nothing more.
- **A Lua panel is an ordinary web view.** It loads the address you give it, runs its scripts and may reach the
  network; it gets no bridge to Eitri, no colours and no keys kit. Give it only pages you trust.
- **A page that handles a key itself** and moves the keys to the editor (`layout.focus`) lets the key reach the
  editor too, unless it calls `preventDefault()`. Keys handled through the keys kit are not affected.
- Plugin pages and Lua panels are not shown in the companion window. A plugin page can still open as a window of its
  own beside your Neovim (`eitri page <id>`, `:EitriPage <id>`; [the guide](guide/companion.md#pages-beside-your-nvim))
  when it uses only following the buffer, Neovim commands, opening files, keeping state and the network: the Markdown
  preview can, the agent pages cannot. Lua panels have no such window.

## Reaching eitri.cc and GitHub from mainland China

Users on some Chinese ISPs (reported so far: China Telecom in Fujian, Jiangsu and Henan) say their connections to
foreign sites that are not on the ISP's whitelist are reset, so eitri.cc and github.com may not open at all
there. The installer downloads from GitHub, so it can fail the same way. If you can get the release files another
way, `sh install.sh --tarball FILE --sums FILE --sig FILE` installs from them
([INSTALL.md](../INSTALL.md#offline-from-release-files)); the sidecar build still needs a network that reaches Node.js and npm.

## Companion mode

Companion mode is the agent panel as its own window beside your own nvim
([the guide](guide/companion.md)). It is new, and less of it has been tried than of the
one-window mode.

- **Tried so far:** sway with a terminal nvim, `eitri split` with Neovide 0.16.2 on sway (also started from a shell
  inside tmux), the GNOME Shell extension's moves in GNOME Shell 50, and companion mode with a terminal nvim on Hyprland 0.57.0
  and 0.56.2 (not the second window's raises). niri and other nvim GUIs have not been tried yet. On niri the edge behaviour is the window manager's own; Eitri does not check it.
- **nvim inside tmux has not been tried yet.** The edge of tmux's panes stays tmux's. With vim-tmux-navigator
  there is no crossing from nvim to the panel window with `Ctrl+h/j/k/l`; with smart-splits.nvim and its
  `at_edge` hook set as in [the guide](guide/companion.md#navigator-plugins-and-tmux), the move should reach the panel at
  tmux's own edge, but that is untested. Opening a file from the panel does not raise the editor's window (the
  process tree from nvim leads to the tmux server, not to the terminal).
- **A tmux server started before your window manager session** keeps that session's old `SWAYSOCK` or
  `HYPRLAND_INSTANCE_SIGNATURE` in its environment, and a panel started from an nvim in it inherits the stale value:
  focus moves and raising then do nothing. Restart the tmux server after logging in again, or start the panel from a
  shell that has the current session's environment.
- **A tmux server started before your desktop session** also keeps the old `WAYLAND_DISPLAY` (or none), and the old
  session's socket file can still be in `$XDG_RUNTIME_DIR`. Eitri checks which `wayland-*` socket has a compositor
  behind it before the window opens: a name that works is never changed, a dead or missing one is replaced by the one
  live socket (the newest, if several), and with none live both the window and the editor use X11 (one line on stderr
  says which). If the editor's half still shows a red page about a display, it says which display it tried; start Eitri
  with `WAYLAND_DISPLAY` set to your compositor's socket (`ls $XDG_RUNTIME_DIR/wayland-*`; in tmux,
  `tmux setenv -g WAYLAND_DISPLAY <name>`, or restart the tmux server).
- **`foot --server` clients share one pid**, so raising the editor by its process can bring up another foot window
  of the same server instead of the one that holds your nvim.
- **At sway's edge the key is consumed.** Eitri has to take or release `Ctrl+h/j/k/l` before it can ask sway
  whether a window lies that way, so at the edge the key does nothing, as tmux's own `select-pane` does at its edge.
- **KDE: no focus moves.** A Wayland client cannot take focus there; use the desktop's own window keys.
- **Pages beside your nvim** (`eitri page`, `:EitriPage`; [the guide](guide/companion.md#pages-beside-your-nvim)) passed their window checks on sway
  with nvim 0.12.6 in a terminal (the page's colours, the follow, the keys, closing, `kill -9`), and their slot and
  reload suites pass on nvim 0.11.2 and 0.12.6. Not tried yet: nvim inside tmux (where the reload on focus
  needs `set -g focus-events on`), Hyprland, niri, KDE and GNOME with the extension.
- **Taking over a terminal nvim** (`:EitriOpen`, `eitri open`; [the guide](guide/takeover.md)) passed all 17 rows of its
  window checks against Neovim 0.12.6 in tmux. Not tried yet: Hyprland, the GNOME session, a real lazy.nvim spec, and
  vim-tmux-navigator and smart-splits as installed plugins.
- **A page window closes slowly in two cases.** A graceful close leaves its slot in your Neovim until Neovim's next liveness
  check (up to a second), as a killed window does; and a Neovim that is blocked at a prompt when you run `:EitriPage` keeps
  `eitri page` waiting, with no window, until it answers.
- **GNOME needs the Eitri GNOME Shell extension** for focus moves, enabled by you and loaded at your next login
  ([the guide](guide/companion.md#gnome-the-extension)). With it: moves stay on the focused window's monitor; an X11 or
  XWayland window never counts; moving back within about a third of a second of the previous move is refused; a
  terminal that runs all its windows in one process (GNOME Terminal, Ptyxis) gets focus handed back to its most
  recent window, which may not be nvim's; and a forwarded `eitri split` does not bring the panel forward.

## What leaves your machine

- **Eitri sends nothing anywhere on its own.** It has no telemetry, no update check and no crash upload; a crash
  leaves a log under `$XDG_STATE_HOME/eitri/log/` (default `~/.local/state/eitri/log/`) that stays there. It uses the
  network when you run the installer, `eitri setup` or `eitri sidecar check` / `update`, and when you click a link,
  which your browser opens.
- **The Markdown preview loads remote images by default.** An `https` image in the document is fetched when the
  preview shows it, so the server hosting it sees your address and the time. `http` images are never loaded. To load
  none, `eitri.plugin.load("markdown", { remote_images = "off" })` ([Markdown preview](guide/markdown-preview.md#images)).
  The preview has no other network use.
- **Claude Code and Codex keep their own settings.** Eitri runs the `claude` and `codex` you installed and does not
  change their telemetry or update settings, so what they send follows your configuration for them.
- **A Lua panel or a plugin that declares hosts may reach them** ([Pages you build](#pages-you-build)); that is
  something you installed or configured.

## Security

- **Eitri asks before it loads a project's own Claude configuration, and the step has limits.** Until you trust a
  project, its agent sessions load only your own user settings, not the project's `.claude/settings.json`,
  `.claude/settings.local.json`, `.mcp.json` or `CLAUDE.md`, so a repository you did not write cannot start its own
  hooks or MCP servers, or pre-approve tool calls, at your first message. The question lists what it found, with the
  exact commands. Trust is remembered per project and is tied to what you saw: Eitri asks again after any change to
  `.claude/`, `.mcp.json` or `CLAUDE.md`, including one that terminal `claude` makes (its "don't ask again" rewrites
  `settings.local.json`). It never imports terminal `claude`'s own trust answer, so a project you already trusted in
  the terminal asks once more here. A file Eitri cannot check (larger than 4 MiB, unreadable, a pipe, a symlink
  Eitri cannot follow in place of a configuration file or below `.claude/`, or a settings file that is not valid
  JSON) is named in the question, and your answer then covers that one start only. On a Claude tab, a symlink below
  `.claude/` or in place of `CLAUDE.md` whose relative target stays inside the repository is followed and checked,
  and so is one inside `.codex/skills/` on a Codex tab; [the permissions guide](guide/permissions.md) lists the
  exceptions. Nothing in the question is cut, so a
  configuration whose question would pass 2 MiB cannot be shown and cannot be trusted at all: the project starts
  only without its configuration. A `CLAUDE.md` in a subdirectory of the project is not read by the check; Claude
  Code loads it once the agent reads a file there, and the question says so. A script a hook calls from outside
  `.claude/` is trusted only through the hook's command text, so trusting a project means trusting its code to run, as
  building it does. Continuing a conversation in a real terminal runs plain `claude`, whose own trust question applies.
  To forget every answer, delete `~/.local/state/eitri/trust/`.
- **In an Auto tab, Claude Code's own auto mode can let the agent write under `.git/` without asking**, a new
  git hook included. Turn review does not show changes under `.git/`, and the trust question watches `.claude/`,
  `.mcp.json` and `CLAUDE.md`, not `.git/`. After a turn in a repository you care about, `ls .git/hooks` is worth a
  look. An edit under `.claude/`, by contrast, makes Eitri ask the trust question again before the next session.
- **A shell command can reach Eitri's own configuration without naming it.** A write into `~/.config/eitri`, Eitri's
  data and state directories or the directory the program runs from is a card in Manual and Auto
  ([the permissions guide](guide/permissions.md#eitris-own-configuration)), but a shell command is judged by its text:
  one that gets there through a script, a `cd` first or a path it builds is not caught, and in an Auto tab Claude
  Code's auto mode decides it, as it decides a write to `~/.bashrc`.
- **A hook in your own Claude Code settings (or a trusted project's) can change a tool call after you approved
  it.** Claude Code applies a hook's rewritten input after Eitri has seen the original; when Claude Code then
  asks, its card shows the rewritten input and the tool's row the original. Hooks work this way in a terminal too.
- **A trusted project's `permissions.allow` rules reach an Auto tab only if you have also trusted that directory
  in terminal `claude`.** That is Claude Code's own rule; your user settings' rules always apply.
- **In a bypass tab your own ask rules stop nothing.** Since 0.2.2 bypass also approves the questions Claude Code
  raises itself, those your `permissions.ask` rules (or a trusted project's) force included, with no card; the
  tool's row says it was allowed in bypass. Use a Manual or Auto tab where an ask rule has to hold.
- **Plan mode supervises Claude; it is not a sandbox.** While Claude Code plans, Eitri approves no file edit or command
  by itself and shows each as a card, but your hooks (and a trusted project's) still run, MCP servers stay loaded, and
  other tools, an MCP server's included, run wherever the tab's mode lets them. Which calls count as made in plan mode
  rests on the order in which Claude Code reports them: a call Eitri sees start more than 2 seconds late is judged both
  ways, as made in plan mode and as made outside it, and is a card where the two answers differ. The plan is Claude's
  text, drawn with no scripts, with remote images as links and hidden characters shown.
- **A dynamic workflow's agents get cards in a Bypass tab.** Claude Code does not show Eitri those calls start, so Eitri
  cannot tell whether one was made in plan mode: it waits 2 seconds and shows a card wherever the two answers would
  differ, which in Bypass is every such call, also when nobody is planning. In Manual and Auto, their file edits,
  commands and Claude Code questions that a saved rule or Claude Code's auto mode would have handled are cards too.
- **Images are decoded under resource limits, not in a sandbox.** Eitri decodes a pasted or dropped image in a separate
  process limited to 768 MiB of memory beyond its start-up size, 30 seconds of processor time and 60 seconds in all. A
  file that is merely huge or broken ends that process when it reaches a limit, and the image's chip shows an error.
  The limits do not contain a file built to exploit a bug in the image decoders: the process runs as you, with your
  files, and has no system-call filter. It needs Linux 5.11 or newer; in a container or sandbox that forbids the calls it uses, no
  image can be decoded and the image's chip says why.
- **The bottom terminal can write to your clipboard, by design.** A program running there can set the clipboard
  or the primary selection with an OSC 52 escape sequence (the same default as Alacritty). Eitri asks nothing first, but
  the pane shows a notice for three seconds after the write (`Terminal 3: OSC 52 wrote clipboard: 9 bytes`), naming
  the terminal, the target and the size, never the text. Reading the clipboard is refused.
- **The agent panel's script policy is new.** The
  panel now allows only its own script by hash instead of any inline script. If the agent panel stays blank
  after an update, please report it with your WebKitGTK version.

## Claude Code conversations

- A Claude Code conversation started in Eitri before 0.3.0 **keeps** its earlier, shorter instructions when you
  continue it (measured with Claude Code 2.1.302); a new conversation gets the full ones, and `/compact` may
  refresh them too (not yet measured).
- A conversation started in Eitri and continued with `claude --resume` in a terminal **keeps** Eitri's short
  panel note (measured with Claude Code 2.1.302). The note says the session was started by Eitri, so it stays
  true there.
- A conversation started in a terminal and continued in Eitri **may keep** its terminal instructions without
  Eitri's panel note (not yet measured).
- A sidecar without `system_prompt_preset` still starts sessions with the Agent SDK's minimal prompt.
  The startup diagnostic says Claude Code's system prompt is unavailable and names the required backend revision;
  if `EITRI_VERDANDI_CHECKOUT` or `EITRI_SIDECAR_BINARY` selects an older backend, point that override at a backend
  built for the named revision. Otherwise, run `eitri setup`.
- **Closing a Claude tab ends its scheduled wake-ups and cron tasks without asking.** They fire while the tab stays
  open. Closing the tab, its view or the window asks first while background tasks run or a question waits, but Eitri
  has no list of scheduled jobs yet, so it neither counts them nor warns that they will not fire. (Claude Code keeps
  a cron task after its session ends only when it was told to keep it across sessions.)
- **A background subagent's permission card can be out of sight.** It is drawn at the top of the subagent's turn,
  above the subagent's row. While a question box is open the card may be scrolled out of view, and only the tab's
  `⚑` count shows that it waits.
- **Some reports about background tasks show only Claude Code's bare words.** When Claude Code reports on a task in a
  way Eitri does not recognise yet (a stopped shell, a workflow's agents), the row shows the words Claude Code sent,
  often just the task's description, without saying what happened to the task.
- **Right after `/clear`, an empty highlighted block can stay in the middle of the empty view.** It goes away when
  the next turn draws.
- **Claude Code keeps the images you send.** It writes each one into the session file it keeps on disk, as in a
  terminal, and Eitri does not remove them. Eitri's own record of a tab keeps only each image's type, size and checksum.
  After 16 MB of images a tab says once that the conversation is heavy and suggests `/compact` or a new tab.
- **Plan mode and images on screen were checked against a scripted model service, with Claude Code 2.1.311.** The
  plan box, its buttons and the image chips passed their checks in dark and light, at a normal and a narrow width.
  Against the real model service, Claude Code 2.1.322 on this release's backend planned, revised a plan and carried it
  out after an approval, held an edit for an answer after a plan approved into Manual, and read an attached image
  (2.1.311 did the same on earlier builds). Reports of what you see are welcome.
- **In a narrow chat pane (about 320 pixels) the status line is cut short.** The panel's mode shows as a single
  letter (`I`, `B`), the permission mode's name is cut off (`bypass permission`), and the activity line's spinner
  sits on a line of its own.

## Codex and ACP tabs

Tabs for Codex and for agents registered with `eitri.agent.register` are new ([the guide](guide/agents.md)). They
have been tried against stand-in models and stand-in agents, and, in a few runs, a real Codex and the `codex-acp`
adapter; they have not been tried on every setup.

- **What an agent reads from your editor in an ACP tab is drawn as that tab's other tool results are.** ACP names no
  server, so Eitri cannot tell an [editor tool's](guide/editor-tools.md) result from the agent's others there, and
  hidden characters in it are not made visible as they are in Claude and Codex tabs.
- **The first start with a `codex` Eitri has not checked runs a short self-check, and every Codex start is refused
  until it passes.** It takes a few seconds, is remembered per binary (path, size, modification time and version)
  and runs again after an update. It fails, naming `XDG_STATE_HOME`, when the state directory is under `/tmp` or `$TMPDIR`
  and neither `XDG_RUNTIME_DIR` nor the cache directory is elsewhere. A newer Codex than the tested range runs and
  is marked as untested.
- **A Codex tab's sandbox limits writes, network and sockets, not reads.** Codex can read any file you can read.
  Commands your own exec-policy `allow` rules cover run outside the sandbox, MCP tools follow the approval mode
  of their server, and a command approved to leave the sandbox runs with your full rights. Your own Codex
  configuration (`CODEX_HOME`, or `~/.codex`) always loads, and Eitri cannot turn it off.
- **Subagents cannot be turned off.** A Codex subagent inherits the tab's sandbox and trust answer; its own text is
  not shown.
- **A command the model leaves running in the background past its call outlives a turn that ends normally.** The
  turn is not held for it: the band says how many are running, that turn's review says later changes are not in
  it, and `<leader>k` stops them by stopping the Codex session (after a `y/n`; the conversation can be resumed).
  An interrupt after that, or closing the tab, stops them too.
- **A hook of yours can answer first.** A `PermissionRequest` hook in your own Codex configuration may answer a
  request before Eitri sees it. Not measured: a hook rewriting a command after approval, how each MCP approval
  mode behaves, a plugin from a marketplace listing loading in a trusted project, resuming a thread that a live
  Codex client holds, a subagent's file-change approval, and whether the sandbox can reach your session bus, tmux
  or nvim.
- **There is no Eitri-side classifier or saved rule for these agents.** Every request that reaches Eitri is a card
  (in Manual and Auto), or is approved by Eitri (in Bypass).
- **An ACP agent starts only after a `y`**, because Eitri cannot keep it from loading the project's own
  configuration; an `n` starts nothing and is not remembered. `project_files` is your list: Eitri hashes only
  what you name there. Eitri does not scrub the agent's environment, so an agent that reads a relative-path
  variable as its configuration directory would read the project's directory.
- **What an ACP agent runs without asking is the agent's own.** Eitri shows the requests it receives and sets
  only the mode the registration names. Eitri has no rate limits, context bar or slash-command list
  for these tabs (the model and the effort are chosen from what the agent lists), and `<leader>t` (continue in a
  terminal) is not available for them yet.
- **The last row of a tab's details, "Continue in a terminal…", is for Claude tabs only.** `prefix i` draws it on
  Codex and ACP tabs too. Choosing it there, on a tab with a session, ends that session in Eitri and prints a
  `claude --resume` command that cannot open it. Do not use it on these tabs; `<leader>t` already refuses them. The
  row goes away for them in the next release.

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
  that belongs to another user or has extended attributes), the next window with turn review on offers to restore the saved bytes.
- **A file with a second hard link is never reverted.** A write through one link would change the others, which may live outside the project, so the
  revert is refused with `the file has N hard links; writing it would change the other links too, so it was not written` and nothing is touched.
- **Limits:** a file over 8 MiB is listed as too large with no patch; a patch over 2,000 lines shows only its counts; a file name that is not valid
  UTF-8 is listed but its patch cannot be opened; a project inside a larger repository (opened below the repository's top level) honours neither
  the parent's `.gitignore` files above its own root nor the repository's `info/exclude`, so files excluded only there are copied into the store.

## Stability

0.4.0 is an early release and not yet stable: expect rough edges. The `init.lua` API and the default keys may change
during 0.x. Automated runs against the real Claude
Code cover 2.1.302, 2.1.311 and 2.1.322; other versions have not been tried. Reports are welcome.

## Not on this list?

[Open a bug report](https://github.com/HunterGrey-cyber/eitri/issues/new/choose), or ask in
[Discussions](https://github.com/HunterGrey-cyber/eitri/discussions) first if you are unsure it is one.
