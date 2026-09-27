# neovibe

**Your whole Neovim, unmodified, with a real rich agent panel beside it. Both halves are real, and
the shell owns neither one's internals.**

There are two ordinary ways to put an AI agent next to code, and each charges you for the other
half. Run the agent in a terminal and your editor is untouched, but everything it says arrives as
text in a PTY — markdown that isn't rendered, diffs you read as plus and minus lines. Use one of the
AI-editor forks and the panel is genuinely good, but the built-in "Vim mode" is an emulation of
whichever parts someone had time to reimplement.

neovibe refuses the trade. The editor pane is Neovide's own pipeline — keyboard and mouse → Neovim →
Neovide's renderer → Skia → GPU — driving a real `nvim --embed` that loads your actual
configuration, with your plugins, your LSP, your Treesitter and your keymaps. The agent panel is a
React app in WebKitGTK, because that is still the sane way to render markdown, diffs and tool
output. Neither half is a reimplementation of the other.

![A pending permission card beside real Neovim](demo/permission-card.png)

## The one rule

**Component-ize Neovide, don't virtualize it.** No terminal-emulator relay, no WebView relay, no
framebuffer compositing, and no second text model synchronised against Neovim's buffer. Neovide was
forked and minimally restructured into an embeddable surface; the fork's changes stay in surface,
geometry, input-adapter and renderer-viewport code so it keeps merging with upstream.

```
GTK4 shell (gtk4-rs)
├── chrome          top bar, tray, dividers — GTK's own appearance suppressed
├── editor pane     GtkGLArea → Skia → Neovide renderer → real `nvim --embed`
├── agent panel     WebKitGTK → React/TypeScript — markdown, tool calls, diffs, permission cards
└── terminal        an in-process PTY → terminal state → Skia (a shell, not a relay for the editor)
```

- **GTK** provides windowing, layout, focus, IME and the WebKitGTK container. It is not the visual
  style: the chrome is drawn by neovibe and follows your Neovim colorscheme.
- **Neovim** keeps owning buffers, motions, LSP, completion, Treesitter and rendering. The shell
  never reimplements any of it.
- **Agent state lives in the Rust host, not the WebView.** The panel is a pure view over a
  projection held in Rust, so reloading or crashing the WebView loses nothing.

## What it does

- **Editor and agent know about each other.** Your current file, cursor and visual selection go
  into the turn as context; an edit the agent proposes is shown on its permission card as a line
  diff; a buffer the agent wrote reloads in the editor.
- **Permissions that ask only when a human is needed.** In Auto mode the panel answers what Claude
  Code's own interactive default would allow without asking — reads inside the project, a shell
  command that only reads — and shows a card for everything else. Every uncertainty resolves toward
  the card: there is no shell parser, so a redirect, a pipe or a substitution asks; an unknown tool
  asks; a path that resolves outside the project asks, `..` and symlinks included.
- **Keyboard first.** `Ctrl-h`/`Ctrl-l` move between panes without breaking your splits (the embedded
  Neovim believes it is inside tmux, so `vim-tmux-navigator` decides the boundary). The panel is a
  document with a `BROWSE` mode: `j`/`k`, `h`/`l`, `a`/`d` to approve or deny, `G`/`gg`, and your
  Neovim `mapleader` with a which-key box. `f` puts jump labels on every target in the window.
- **Session tabs.** One conversation per tab, all running at once, driven by tmux's window keys.
- **The layout is data.** Editor, panel and terminal are modules in a tree: hide, kill, split, swap
  and zoom with tmux's keys; a project reopens the way you left it.
- **A bottom terminal.** Its own PTY painted with Skia, with IME input, bracketed paste and OSC 52
  copy, scrollback (`Shift-PageUp`/`PageDown`, the wheel), mouse selection with middle-click paste,
  and mouse reporting to programs that ask for it (`nvim`, `htop`, `less`).
- **It inherits, it does not impose.** One `:colorscheme` in Neovim and the chrome, editor and panel
  all follow. Window keys default to stock tmux (prefix `Ctrl-b`) and are rebound in
  `~/.config/neovibe/init.lua`, a real Lua kernel (`neovibe.keymap`, `neovibe.command`,
  `neovibe.panel`, `neovibe.config`). A binding that collides with another is a startup error
  naming both. An example `Ctrl-a` setup is in [`docs/keymap/`](docs/keymap/).

![neovibe follows your colorscheme](demo/colorscheme.gif)

## Status — read this before trying it

- **Linux on Wayland is the primary and only verified platform.** Developed on GNOME and a headless
  sway; X11 has not been tried. A macOS port of the GTK-free crates is in progress; the application
  itself does not run there.
- **Much of it has been verified only in an isolated GUI sandbox**, with synthetic input on a
  headless compositor. Everything listed above exists and has tests; much less of it has been
  checked on real hardware under a real hand. Expect rough edges.
- The feasibility questions (Neovide's renderer outside its own window, a browser engine beside it
  without taxing the editor, IME, fractional scaling, fork mergeability) were each tested against a
  Go/No-Go criterion written beforehand and came back Go, several with written conditions. The two
  design documents that preceded the work are in [`docs/`](docs/), Chinese originals under
  [`docs/zh/`](docs/zh/).
- The agent side speaks to Claude Code only. It is not a multi-provider client.

## The workspace

One Cargo workspace at the repository root.

| crate | what it is |
|---|---|
| `shell/` | **the application binary.** Window, chrome, focus, the module grid, the Lua kernel, theme following |
| `neovide-editor/` | library: `NeovideEditorPane`, the forked Neovide renderer, input and runtime on a `GtkGLArea` |
| `agent/` | library: Claude session backends (no GTK). Also builds `agent-hook` and `neovibe-claude-handoff` |
| `core/` | library, package **`neovibe-core`**: the GTK-free half of the shell — backend selection, the Lua kernel, theme tokens, the per-window sockets, layout, keymap, session tabs |
| `agent-ui/web/` | the panel's React/TypeScript frontend. Not a crate: `shell/build.rs` builds it with npm and embeds the single-file result |
| `supervisor/` | `neovibe-supervisor`, a read-only cross-window agent-status dashboard |
| `terminal/` | package **`neovibe-terminal`**: the bottom terminal's engine — PTY, session thread, terminal state, Skia paint |
| `terminal-render/`, `terminal-frame/`, `terminal-sync/`, `terminal-input/` | the terminal's paint list, frame projection, synchronized-update barrier and key encoder. `terminal-input` is Apache-2.0 (derived from Alacritty); see its `NOTICE` |
| `neovide/` | submodule: the Neovide fork ([HunterGrey-cyber/neovide](https://github.com/HunterGrey-cyber/neovide), branch `neovibe-integration`) |
| `poc/` | frozen proof-of-concept crates from the feasibility stage. Not the product; they deliberately keep bugs fixed only in the product crates |

### The Verdandi dependency

The agent talks to Claude Code through a **sidecar**, a Node service from the sibling project
[Verdandi](https://github.com/HunterGrey-cyber/verdandi) (`apps/claude-sidecar`), over gRPC on a
per-window Unix socket. `agent` depends on Verdandi's `claude-runtime-protocol` crate, pinned by
revision in `agent/Cargo.toml` and fetched from GitHub by Cargo; nothing else of Verdandi is needed
to *build* neovibe.

To *run* the sidecar backend, neovibe needs the sidecar itself, found in this order:
`NEOVIBE_SIDECAR_BINARY` (a built executable), a `verdandi-claude-sidecar` beside the `shell`
binary, or a Verdandi checkout at `NEOVIBE_VERDANDI_CHECKOUT` (default `~/src/verdandi`). Without
any of them neovibe uses its **legacy** backend, which spawns the `claude` on your `PATH` directly.
`NEOVIBE_AGENT_BACKEND=legacy|sidecar` forces one; the choice is printed on stderr at startup.
Either way you need Claude Code installed and logged in; neovibe spends whatever account it runs.

## Building

```sh
git clone --recurse-submodules https://github.com/HunterGrey-cyber/neovibe.git
cd neovibe
# cloned without --recurse-submodules?  git submodule update --init
cargo build --workspace --locked
```

Prerequisites, all of them hard — a missing one fails the build, often inside a third-party build
script with an error that never names this project:

- **Rust** recent enough for the fork's 2024 edition (developed on 1.96).
- **GTK 4.18+ and WebKitGTK 6.0 development packages**, and `pkg-config`
  (Arch: `gtk4 webkitgtk-6.0`; Debian/Ubuntu: `libgtk-4-dev libwebkitgtk-6.0-dev`).
- **Node.js and npm**: `shell/build.rs` runs `npm ci` and `npm run build` in `agent-ui/web/`.
- **`protoc`** (`protobuf`/`protobuf-compiler`): Verdandi's protocol crate generates its types with it.
- **A C toolchain**: Lua 5.4 is built from source and statically linked.
- **Network access on the first build**: `skia-safe` downloads a prebuilt Skia, and Cargo fetches
  the Verdandi protocol crate from GitHub.
- At run time: **`nvim`** on `PATH`, a working OpenGL driver, a Wayland session, and Claude Code.

## Running

```sh
cargo run -p shell -- /path/to/project     # or: NEOVIBE_PROJECT_DIR=..., or the current directory
cargo run -p shell -- --clean /path        # start the embedded nvim with --clean
```

The project directory is resolved once and shared by the editor, the agent and the terminal. A
path that is not a directory, or an unknown option, is a startup error rather than silently ignored
(`--` ends option parsing). Each launch is its own process with its own `nvim`.

A few keys to start with (prefix is `Ctrl-b` unless you change it): `Ctrl-h`/`Ctrl-l` switch
panes; `prefix ?` shows every key; `prefix t` opens the terminal; `prefix c` opens a session tab;
`prefix z` zooms the pane with the keys; `F11` toggles fullscreen.

`./install.sh` installs a release build for your own user (`~/.local`) with a desktop entry, and
`./try-neovibe.sh` launches with the backend and account stated up front. Configuration lives in
`~/.config/neovibe/init.lua` (`NEOVIBE_CONFIG_DIR` overrides it); per-project state (layout,
history, saved permission rules) under `$XDG_STATE_HOME/neovibe/`.

The editor component alone, without the shell: `cargo run -p neovide-editor --example standalone`.

## Tests

```sh
cargo test --workspace                      # non-ignored tests; set XDG_STATE_HOME to a scratch dir
cd agent-ui/web && npm ci && npx vitest run
```

Tests marked `#[ignore]` need a display, a real `nvim`, or a real Claude account (and bill it);
each says in its own doc comment what it needs.

## Licence

MIT — see [`LICENSE`](LICENSE), except `terminal-input/`, which is Apache-2.0. Built on a fork of
[Neovide](https://github.com/neovide/neovide) (MIT), whose architecture and code directly shaped the
editor component. `packaging/collect-licenses.py` generates the third-party notices a package ships.
