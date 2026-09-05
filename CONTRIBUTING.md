# Contributing to Eitri

Bug reports, small fixes and larger changes are all welcome. This file gets you from a clean clone
to a green test suite, and says how the project is laid out; [README.md](README.md) says what it is
and [INSTALL.md](INSTALL.md) how to install it.

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
  style: the chrome is drawn by Eitri and follows your Neovim colorscheme.
- **Neovim** keeps owning buffers, motions, LSP, completion, Treesitter and rendering. The shell
  never reimplements any of it.
- **Agent state lives in the Rust host, not the WebView.** The panel is a pure view over a
  projection held in Rust, so reloading or crashing the WebView loses nothing.

## The workspace

One Cargo workspace at the repository root.

| crate | what it is |
|---|---|
| `shell/` | **the application binary.** Window, chrome, focus, the module grid, the Lua kernel, theme following |
| `neovide-editor/` | library: `NeovideEditorPane`, the forked Neovide renderer, input and runtime on a `GtkGLArea` |
| `agent/` | library: Claude session backends (no GTK). Also builds `agent-hook` and `eitri-claude-handoff` |
| `core/` | library, package **`eitri-core`**: the GTK-free half of the shell — backend selection, the Lua kernel, theme tokens, the per-window sockets, layout, keymap, session tabs |
| `agent-ui/web/` | the panel's React/TypeScript frontend. Not a crate: `shell/build.rs` builds it with npm and embeds the single-file result |
| `supervisor/` | `eitri-supervisor`, a read-only cross-window agent-status dashboard |
| `terminal/` | package **`eitri-terminal`**: the bottom terminal's engine — PTY, session thread, terminal state, Skia paint |
| `terminal-render/`, `terminal-frame/`, `terminal-sync/`, `terminal-input/` | the terminal's paint list, frame projection, synchronized-update barrier and key encoder. `terminal-input` is Apache-2.0 (derived from Alacritty); see its `NOTICE` |
| `neovide/` | submodule: the Neovide fork ([HunterGrey-cyber/neovide](https://github.com/HunterGrey-cyber/neovide), branch `neovibe-integration`) |
| `poc/` | frozen proof-of-concept crates from the feasibility stage. Not the product; they deliberately keep bugs fixed only in the product crates |

## Prerequisites

All of these are hard requirements — a missing one fails the build, usually from inside a
third-party build script with an error that never names this project:

- **Rust 1.96 or newer** for the [Neovide fork](https://github.com/HunterGrey-cyber/neovide)'s 2024
  edition; `install.sh` enforces this floor (`NV_MIN_RUSTC`), and this tree builds and tests clean
  on 1.96.0.
- **GTK 4.14+ and WebKitGTK 6.0 development packages**, plus `pkg-config`
  (Arch: `gtk4 webkitgtk-6.0`; Debian/Ubuntu: `libgtk-4-dev libwebkitgtk-6.0-dev`).
- **Node.js and npm**: `shell/build.rs` runs `npm ci` and `npm run build` in `agent-ui/web/`.
- **`protoc`** (`protobuf`/`protobuf-compiler`): the Verdandi protocol crate below generates its
  types with it.
- **A C toolchain**: Lua 5.4 is built from source and statically linked.
- **Network access on the first build**: `skia-safe` downloads a prebuilt Skia, and Cargo fetches
  the Verdandi protocol crate from GitHub.
- At run time: **`nvim` ≥ 0.10** on `PATH`, a working OpenGL driver, a Wayland session, and Claude
  Code installed and logged in — Eitri spends whatever account it runs under.

## Building

```sh
git clone --recurse-submodules https://github.com/HunterGrey-cyber/eitri.git
cd eitri
# cloned without --recurse-submodules?  git submodule update --init
cargo build --locked
```

`cargo build --locked` — **not** `--workspace` — is the product: the workspace's `default-members`
(`shell`, `neovide-editor`, `agent`, `core`, `supervisor` and the terminal crates) at the GTK 4.14
floor. `cargo build --workspace` also builds the frozen `poc/` crates below, which need GTK 4.18 and
are not part of what ships — don't reach for it just to build Eitri itself.

## Tests

```sh
cargo test                                                                    # GTK 4.14 floor
cargo test -p agent -p eitri-core -p shell --features shell/legacy-backend  # the legacy-backend pass
(cd agent-ui/web && npm ci && npx vitest run && npx tsc -b)
python3 -m pytest packaging
```

Both `cargo test` invocations must stay green: the default build's shape, and the same three crates
built with the legacy backend compiled in (below) — a change to `agent`/`core`/`shell` can compile
and pass on one and not the other. Point `XDG_STATE_HOME` at a scratch directory first; the tests
write project state there. `cargo test --workspace` additionally builds and tests the frozen `poc/`
crates and needs GTK 4.18+ for them. Tests marked `#[ignore]` need a display, a real `nvim`, or a
real Claude account (and bill it) — each says which in its own doc comment; they're not part of the
pass above.

## Formatting

`rustfmt.toml` is the whole rule — one line, `max_width = 120`; everything else is rustfmt's
default. Run `cargo fmt --all` before you commit; a reviewer runs `cargo fmt --all --check`. A
couple of tests (the socket-path scanners in `agent/` and `core/`) read this crate's own source
files as literal text and are sensitive to how a line wraps — if one turns red after a change that
looks unrelated, run `cargo fmt --all` again rather than hand-editing around it.

## The Verdandi dependency

Eitri's agent talks to Claude Code through a sidecar, a Node service from the sibling project
[Verdandi](https://github.com/HunterGrey-cyber/verdandi). `agent` depends on Verdandi's
`claude-runtime-protocol` crate, pinned by revision in `agent/Cargo.toml` and fetched from GitHub by
Cargo — *building* Eitri never needs a Verdandi checkout. *Running* the sidecar backend is a
separate question with its own search order:

To *run* the sidecar backend, Eitri needs the sidecar itself, found in this order: first
`EITRI_SIDECAR_BINARY` (a built executable), then — if that is unset — `EITRI_VERDANDI_CHECKOUT`
(a development override pointing at a Verdandi checkout); then a `verdandi-claude-sidecar` beside
the `shell` binary (what the packages and the AUR builds produce); then the artifact `eitri setup`
builds for your user, under `$XDG_DATA_HOME/eitri/sidecar/<pinned revision>/` (or
`~/.local/share/…` if unset), keyed to the exact revision this build pins so an old release's build
is never picked up by a newer one; and last, a Verdandi checkout at the default
`~/src/verdandi`, but only if one is already sitting there with something built.
Nothing here ever builds anything on its own — see INSTALL.md's
[Why the sidecar is built on your machine](INSTALL.md#why-the-sidecar-is-built-on-your-machine) for why
that build has to happen on your machine, and run through `eitri setup`.

If you need to change the protocol crate itself, don't re-pin the revision for a work-in-progress
change: point Cargo at your own Verdandi checkout instead, in a `.cargo/config.toml` you keep out of
your commits:

```toml
[patch."https://github.com/HunterGrey-cyber/verdandi.git"]
claude-runtime-protocol = { path = "/path/to/your/verdandi/checkout/crates/claude-runtime-protocol" }
```

Land the protocol change on the Verdandi side first, then open the Eitri side of it pointing at a
real, pinned revision.

## Running a source build

```sh
cargo run -p shell -- /path/to/project     # or: EITRI_PROJECT_DIR=..., or the current directory
cargo run -p shell -- --clean /path        # start the embedded nvim with --clean
```

The project directory is resolved once and shared by the editor, the agent and the terminal. A
path that is not a directory, or an unknown option, is a startup error rather than silently ignored
(`--` ends option parsing). Each launch is its own process with its own `nvim`.

If you installed Eitri (see [INSTALL.md](INSTALL.md)), run it as `eitri [project directory]`.
`./try-eitri.sh` in this repository launches a source build with the backend and account stated up
front. Configuration lives in `~/.config/eitri/init.lua` (`EITRI_CONFIG_DIR` overrides it);
per-project state (layout, history, saved permission rules) under `$XDG_STATE_HOME/eitri/`.

The editor component alone, without the shell: `cargo run -p neovide-editor --example standalone`.

## The `poc/` crates

`poc/` holds the frozen proof-of-concept crates from Eitri's feasibility stage. They are not the
product — `shell` is — and they deliberately keep bugs that were already fixed in the product
crates, so a fix that makes `poc/` match `shell`/`neovide-editor` again is not what's wanted; fix
the product instead. They pin GTK's `v4_18` feature (one point above the product's 4.14 floor),
which is why they sit outside the workspace's `default-members` and are only reached by
`--workspace`.

## The legacy backend

Before the sidecar, Eitri's agent panel spawned a `claude` process on `PATH` directly. That code
is still in the tree behind a cargo feature, `legacy-backend`, off by default: plain `cargo
build`/`cargo test` never compile it, and no release build can select it (`eitri --legacy` on a
release exits 1, naming why). Build it in for local testing with `cargo build --features
shell/legacy-backend` (or `cargo run -p shell --features shell/legacy-backend -- ...`), then select
it with `--legacy` or `EITRI_AGENT_BACKEND=legacy`. It needs Claude Code installed and logged in,
same as the sidecar.

## Submitting a change

Fork, branch off `main`, and open a pull request against it. Keep the diff scoped to one change, and
say why in the commit message, not just what — the "why" is what a reviewer can't get back from the
diff alone. Run whichever of the commands above cover what you touched before opening the PR;
`agent-ui/web`'s tests only need running when you touched `agent-ui/web/`.

## Reporting a security issue

Please don't open a public issue for a security vulnerability. Use GitHub's private vulnerability
reporting on this repository instead — the **Security** tab → **Report a vulnerability** — so a
report, and any proof of concept, doesn't sit in public before a fix ships.

## Licence

Contributions are made under the licence that already covers the file you're editing: MIT for
everything except `terminal-input/`, which is Apache-2.0 (derived from Alacritty; see its own
`NOTICE`). See [README.md's Licence section](README.md#licence) for the full picture, including what
Eitri statically links and why the agent sidecar isn't in any release artifact.
