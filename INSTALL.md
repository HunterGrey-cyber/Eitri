English | [简体中文](INSTALL.zh-CN.md)

# Installing Eitri

Every route ends the same way: a per-user install under `~/.local` (or a system one from a
`.deb`/`.rpm`), and an `eitri` launcher. `install.sh` builds the agent sidecar for your machine as
part of installing — except the `.deb`/`.rpm` route, which runs no maintainer script and needs one
explicit `eitri setup` afterwards (below). The `eitri` launcher itself never builds a sidecar: run
before one exists, it still opens with no agent backend and a one-line hint to run `eitri setup`. See
[Why the sidecar is built on your machine](#why-the-sidecar-is-built-on-your-machine) below. Full
option lists are `sh install.sh --help` and `eitri setup --help`; that output, not this file, is the
source of truth once you've read this once.

**Requirements**: GTK ≥ 4.14, WebKitGTK 6.0, glibc ≥ 2.39, x86_64 Linux. **0.2.0 is x86_64 only** —
building from source (below) needs the same architecture and refuses on anything else, so there is
no ARM route yet. **nvim ≥ 0.10** — your own, on `PATH`, or let the installer fetch a
private copy for Eitri alone (below). Claude Code, installed and logged in (a warning, not a
refusal, if it is missing — Eitri just cannot run turns until you add it).

## Quick install

Downloads and runs the installer over HTTPS, into `~/.local`, no `sudo`:

```sh
curl --proto '=https' --proto-redir '=https' --tlsv1.2 -sSfL https://github.com/HunterGrey-cyber/eitri/releases/latest/download/install.sh | sh
```

`--proto '=https' --proto-redir '=https'` refuses a plain-HTTP redirect, and `-L` follows GitHub's
own `releases/latest/download/…` → `releases/download/v<X>/…` redirect — without `-L` a bare `curl -sSf`
gets an empty body from the redirect response and pipes nothing to `sh`, so nothing installs and
nothing is said either way (`curl -f` only trips on a 4xx/5xx status, never a 302). These are exactly
`install.sh`'s own `fetch()` flags for every non-loopback download (`packaging/install.sh`'s `fetch()`
function).

Pass options after `--`: `curl … install.sh | sh -s -- --version 0.2.0 --yes`.

**If you'd rather read it before running it**, download it first instead of piping it:

```sh
curl --proto '=https' --proto-redir '=https' --tlsv1.2 -fsSL -o install.sh https://github.com/HunterGrey-cyber/eitri/releases/latest/download/install.sh
less install.sh      # or your editor
sh install.sh
```

This only lets you read the script; it does not check that what you downloaded is the real thing —
see [Verify before running](#verify-before-running) for that.

Already have the release files (fetched by other means, so this step itself needs no network)?
`sh install.sh --tarball FILE --sums FILE --sig FILE` installs from them directly, with the same
checks as the routes above. Building the sidecar afterwards still needs network access, to fetch
Node.js and the npm packages it is built from.

## Verify before running

`SHA256SUMS` and `SHA256SUMS.sig` are also published. Verifying the signature proves `SHA256SUMS` is
authentic; it does **not** by itself prove the `install.sh` you downloaded matches it — that needs one
more, explicit step, which is easy to leave out and is exactly what the recipe below adds.

```sh
curl --proto '=https' --proto-redir '=https' --tlsv1.2 -fsSL -o install.sh    https://github.com/HunterGrey-cyber/eitri/releases/latest/download/install.sh
curl --proto '=https' --proto-redir '=https' --tlsv1.2 -fsSL -o SHA256SUMS    https://github.com/HunterGrey-cyber/eitri/releases/latest/download/SHA256SUMS
curl --proto '=https' --proto-redir '=https' --tlsv1.2 -fsSL -o SHA256SUMS.sig https://github.com/HunterGrey-cyber/eitri/releases/latest/download/SHA256SUMS.sig
```

Get the trust anchor, `packaging/release-signers` (ssh-keygen's `allowed_signers` format), from this
repository at the tag you are installing — `install.sh` itself embeds the same bytes verbatim (a test
in this repo holds the two equal), so a checkout at the matching tag is the anchor:

```sh
git clone --branch v<version> --depth 1 https://github.com/HunterGrey-cyber/eitri.git nv-signers
cp nv-signers/packaging/release-signers .
```

Then, in order — check the signature, **then hash the downloaded `install.sh` itself against the now-verified
`SHA256SUMS`** (the step above alone only proves `SHA256SUMS` is authentic, not that `install.sh` matches
it), and only then run it. The whole thing is wrapped in `( … )`: pasted straight into an interactive
shell, a failed check's `exit 1` ends only that subshell, not your terminal session.

<!-- verify-recipe:start -->
```sh
(
ssh-keygen -Y verify -f release-signers -I release@eitri -n eitri-release -s SHA256SUMS.sig < SHA256SUMS \
  || { echo "SHA256SUMS does not carry a valid signature: do not run install.sh" >&2; exit 1; }

sha256sum install.sh | awk '{print $1}' | grep -qxF "$(awk '$2=="install.sh"{print $1}' SHA256SUMS)" \
  || { echo "install.sh does not match the verified SHA256SUMS: do not run it" >&2; exit 1; }

sh install.sh
)
```
<!-- verify-recipe:end -->

This recipe has been tested against a real signed release: with `install.sh` swapped for a different
file after signing (`SHA256SUMS`/`.sig` left exactly as signed — the way a compromised release host or
mirror would do it), it prints `install.sh does not match the verified SHA256SUMS: do not run it` and
exits without running anything. `packaging/tests/test_public_docs.py` holds this recipe to that
behaviour (and to refusing a tampered `SHA256SUMS` too) with its own throwaway key, so a future edit
that drops or weakens a check fails a test, not just a release.

**What this does and does not protect, plainly said.** The signature protects a `--base-url` mirror,
`eitri setup` and a re-run from an already-installed copy (whose embedded key predates any later
compromise), and anyone who checks `install.sh` by hand against `packaging/release-signers` in git. It
does **not** protect a first `curl … | sh` against a compromised release page or GitHub account itself:
the verifier and its key come from the same place `SHA256SUMS` does. **For a numbered release**,
`install.sh` embeds the signing key and refuses a missing or bad signature outright, and it refuses to
run at all on a machine without `ssh-keygen` (OpenSSH's client), since it could not check the
signature there. `--insecure-skip-signature` lets it go on without `ssh-keygen`, checking only the
checksums, which catch a corrupt download but not a release someone else built; where `ssh-keygen` is
installed the option changes nothing and the signature is checked anyway. **For a release candidate** (like `rc.1`, published before the release key was listed), the
installer embeds no key at all — its checksums only detect corruption, never who published them — and
the recipe above refuses it: such an rc is signed with a throwaway key, never the release key
(`packaging/release-signers` at `rc.1`'s tag has no key line at all; from the first numbered release
on, it holds the release key). If you were handed that throwaway key's signers file out of band, pass it
with `--release-signers FILE` (or use it as `release-signers` in the recipe); that only proves the files
agree with each other, not that they came from the real maintainer — there is no published trust
anchor for those release candidates.

## `.deb` / `.rpm`, then `eitri setup`

Download `eitri_<version>_amd64.deb` or `eitri-<version>-1.x86_64.rpm` from the
[releases page](https://github.com/HunterGrey-cyber/eitri/releases) and install it with your package
manager: `sudo apt install ./eitri_<version>_amd64.deb`, or `sudo dnf install
./eitri-<version>-1.x86_64.rpm` (the `.rpm` command has been run for real, against `rc.1`'s own
package, on a Fedora 44 VM). Neither package runs a maintainer script: it only unpacks the four binaries, the launcher,
the desktop entry, the icon and the licences (see [Where things go](#where-things-go)) — building the sidecar as
root would run every one of `npm ci`'s dependency install scripts as root, so nothing here does that.
Neither package declares a hard dependency on a distro `neovim` (the `.deb` has none at all; the
`.rpm` gets a soft `Recommends`): Ubuntu's own `neovim` is too old to meet the ≥ 0.10 floor, so
depending on it would resolve, install, and only fail at first launch with no useful error.

Then, once, as your own user:

```sh
eitri setup
```

This builds the sidecar for this machine and this pinned Verdandi revision (into
`$XDG_DATA_HOME/eitri/sidecar/<rev>/`, never as root — see [Why the sidecar is built on your
machine](#why-the-sidecar-is-built-on-your-machine)), then runs the same nvim offer a normal install
runs: skipped if the `nvim` already on your `PATH` is `>= 0.10`, skipped if this release's private
copy is already installed, and otherwise offered interactively (or `--yes`/`--no-nvim`/`--with-nvim`
for a non-interactive answer). `eitri setup --nvim-only`, `--uninstall` and `--sidecar-only` run
exactly one of those alone instead — `eitri setup --help` lists every mode.

## AUR (Arch)

```sh
yay -S eitri-bin    # prebuilt binaries; the sidecar is still built at install time, on your machine
yay -S eitri-git    # everything, including eitri itself, built from source
```

(These commands are exactly `packaging/aur/eitri-bin/PKGBUILD` and `packaging/aur/eitri-git/PKGBUILD`'s
own `pkgname`, run with an AUR helper; they were not run against a live AUR entry by this pass.) Both
declare `provides`/`conflicts: eitri`, so at most one package named `eitri` — either of these, or
any other — can be installed at a time, and `eitri-bin`'s own `build()` builds the sidecar as a
sibling of the binaries in `/usr/lib/eitri/` rather than per-user. `eitri-bin`'s `depends` are `gtk4
webkitgtk-6.0 glibc gcc-libs`; nvim and Claude Code are `optdepends`, not hard dependencies — many Arch
users run a self-built or `bob`-managed nvim, so this never fights that.

## Building from source (`--from-source`)

```sh
sh install.sh --from-source
```

**x86_64 only in this release** — this refuses immediately on any other `uname -m`: the only pinned Skia
archive is x86_64, and no ARM archive is pinned yet.

Clones the public repository at the release's own tag, refuses unless its `HEAD` matches that
release's recorded commit, then builds and installs exactly like the prebuilt route. Needs the full
toolchain: **Rust 1.96 or newer** (edition 2024; the installer refuses an older `rustc`), **GTK
4.14+ and WebKitGTK 6.0 development packages** plus `pkg-config`, **Node.js and npm**, **`protoc`**,
a **C toolchain**, and **network access** on the first build. `--checkout DIR` builds a local
working tree instead of cloning (the owner's own dev loop) — see [Prerequisites](CONTRIBUTING.md#prerequisites) in
CONTRIBUTING.md for the full prerequisite list and package names per distro, and `sh install.sh --help`
for `--verdandi-checkout` and the other `--from-source` sub-options.

## Why the sidecar is built on your machine

It bundles Anthropic's `@anthropic-ai/claude-agent-sdk` ("© Anthropic PBC. All rights reserved"),
which this project does not redistribute. `eitri setup` (and every route above) fetches it from the
npm registry to your own machine, under Anthropic's own terms, and builds it from the pinned Verdandi
revision this release names. The sidecar itself never ships inside an Eitri release artifact.

## Updating

Re-run the same route you installed with. `sh install.sh` (with no `--version`) compares your
installed version and sidecar revision against the latest release: if both already match, it prints
"up to date" and exits without touching anything. Otherwise it unpacks the new version and **builds
its sidecar before switching anything**, so a failed build leaves the old install untouched; only then
does it swap the two atomically. Your previous sidecar revision is kept, not removed, so the swap can
be trusted even if the new one turns out to be bad — it (and any other revision no longer named by an
install on this machine) is removed the *next* time you update. Restart open Eitri windows afterwards — they keep running against the old
install, but a new tab or the agent handoff needs the new one. A `.deb`/`.rpm` upgrade is your package
manager's own job; run `eitri setup` again afterwards only if the pinned sidecar revision changed
(it is a no-op if the one already built for the current revision is still present).

Eitri has an icon from 0.2.1 on, and its desktop entry is named after its application id,
`cn.huntergrey.eitri.desktop` (it was `eitri.desktop`), which is how the desktop matches the window to
its launcher. Updating over an older release removes the old entry — the tarball route's installer
does it, and only when the file is byte for byte the one 0.2.0's installer wrote (an entry you edited
stays, and is named); a package upgrade does it by no longer listing the file — so **if you had pinned
Eitri to the dash or the dock, pin it again once** after updating.

If you saved 0.2.0's `install.sh` and rerun *that* to upgrade, it still works: the release tarball also
carries 0.2.0's own desktop entry as `share/applications/eitri.desktop`, which that installer insists on
(this release's installer ignores it, and no package installs it). That old installer cannot lay out the
icon, so you keep 0.2.0's entry and no icon until you run this release's `install.sh` once more, which
replaces the old entry and installs the new entry and the icon files. Going the other way, back to 0.2.0
from this release, is `eitri setup --uninstall` first — it removes the program, the launcher, the
desktop entries and icons, the licences, the private nvim and the sidecars, and keeps `~/.config/eitri`
and `$XDG_STATE_HOME/eitri` — and then 0.2.0's own `install.sh`; this release's `install.sh --version 0.2.0`
refuses, with those same steps, because installing 0.2.0 over it would leave the new entry and the icons
beside the old entry.

## Uninstalling

```sh
sh install.sh --uninstall            # keep ~/.config/eitri and per-project state
sh install.sh --uninstall --purge    # also remove ~/.config/eitri and $XDG_STATE_HOME/eitri
```

This removes `~/.local/lib/eitri`, the `~/.local/bin/eitri` launcher (only if it carries
Eitri's own marker line — an unrelated file of the same name is left alone and named), the desktop
entry (and 0.2.0's `eitri.desktop`, only when it is byte for byte what 0.2.0's installer wrote), exactly
the nine icon files listed under [Where things go](#where-things-go) (a file of your own in your icon
theme directory stays, even one named like Eitri's in a size Eitri does not install) and licences,
`$XDG_DATA_HOME/eitri/nvim` (a private nvim copy), the download cache, and every
sidecar revision except the one an installed `.deb`/`.rpm` still names in its `/usr/lib/eitri/RELEASE`
— so uninstalling a tarball install never takes a package install's sidecar with it. It always keeps
`~/.config/eitri` (your `init.lua`) and `$XDG_STATE_HOME/eitri` (per-project layout, open tabs,
prompt history, saved permission rules) unless you pass `--purge`; nothing named `nvim`/`vim`/`vi` outside
`$XDG_DATA_HOME/eitri/nvim` is ever touched. For a `.deb`/`.rpm` install, the package itself never
owns the per-user sidecar or private nvim, so removing it (`sudo apt remove eitri` / `sudo dnf remove
eitri`) leaves both behind — **and order matters for cleaning them up**. `eitri setup --uninstall`
does not work for this: `eitri setup` is `/usr/lib/eitri/eitri-setup`, which the package removed
along with `/usr/bin/eitri`; and running it *before* you remove the package keeps every sidecar revision the
still-installed package's own `/usr/lib/eitri/RELEASE` names, since that file is how it tells "an
installed Eitri still needs this one" from "nothing does" (the same file governs an ordinary
[update](#updating)). So: first remove the package with your package manager, then download
`install.sh` again (or use a copy you saved beforehand) and run `sh install.sh --uninstall` — with the
package's own `RELEASE` gone, nothing stops it from removing the sidecar and private nvim this time.
It is the same `--uninstall` as above, so if you also have a tarball install under `~/.local`, it
removes that install and its sidecar too. To keep that install, skip this step: its own updates remove
a sidecar revision once no install uses it, as [Updating](#updating) describes.

## Where things go

```
~/.local/lib/eitri/                                              the four binaries, eitri-setup, RELEASE         (tarball route)
~/.local/bin/eitri                                               the launcher, marked `# eitri-launcher v1`      (tarball route)
~/.local/share/applications/cn.huntergrey.eitri.desktop          Exec = the launcher's absolute path             (tarball route)
~/.local/share/icons/hicolor/<size>/apps/cn.huntergrey.eitri.png the icon, 16 to 512 px, and scalable/…/….svg    (tarball route)
~/.local/share/licenses/eitri/                                   LICENSE, THIRD-PARTY-LICENSES, SOURCE           (tarball route)
~/.local/share/eitri/eitri.nvim/                                 the :EitriPanel plugin                          (tarball route)
/usr/lib/eitri/                                                  the same four binaries, eitri-setup, RELEASE    (.deb/.rpm)
/usr/bin/eitri                                                   the same launcher                               (.deb/.rpm)
/usr/share/applications/cn.huntergrey.eitri.desktop                                                              (.deb/.rpm)
/usr/share/icons/hicolor/<size>/apps/cn.huntergrey.eitri.png     the same icon files                             (.deb/.rpm)
/usr/share/licenses/eitri/                                                                                       (.deb/.rpm)
/usr/share/eitri/nvim/eitri.nvim/                                the :EitriPanel plugin                          (.deb/.rpm, AUR)
$XDG_DATA_HOME/eitri/sidecar/<rev>/                              the sidecar eitri setup built, per-user routes
$XDG_DATA_HOME/eitri/nvim/<X.Y.Z>/                               a private nvim copy, only if you accepted the offer
~/.config/eitri/init.lua                                         your own config (EITRI_CONFIG_DIR overrides the dir)
$XDG_STATE_HOME/eitri/                                           per-project layout, open tabs, prompt history, permission rules
```

The sidecar row is per-user for every route above — **except AUR** (`eitri-bin`/`eitri-git`),
whose `build()` builds it as a sibling of the binaries in `/usr/lib/eitri/` instead ([AUR](#aur-arch)).

`$XDG_DATA_HOME`/`$XDG_CACHE_HOME`/`$XDG_STATE_HOME` follow the usual rule: unset, empty or relative
falls back to `~/.local/share`, `~/.cache` and `~/.local/state` respectively — the same rule Eitri
itself uses, so the installer and the running program never disagree about where to look. The private
nvim copy is never placed on `PATH` and never replaces, links or removes anything else named
`nvim`/`vim`/`vi` on your system.

## How a launch starts: two `init.lua` settings

Both go in `~/.config/eitri/init.lua`, and a value other than the ones named here stops Eitri at
startup with a message naming the setting.

```lua
eitri.config.set("agent.restore", "offer")        -- "offer" (the default), "auto" or "off"
eitri.config.set("agent.default_mode", "auto")    -- "auto" (the default) or "bypass"
```

- **`agent.restore`** is what happens to the tabs the last window on this project had open. Eitri
  keeps the tabs that have a Claude session (their order, names, modes and which one was on screen)
  as it goes, and never records "no tabs" because you closed the window. With `"offer"` the empty
  tab's dashboard shows a `Restore last session` line, `s`, while nothing in the window has started;
  `"auto"` brings the tabs back at launch with no key pressed; `"off"` neither offers nor remembers.
  Each tab is resumed (nothing is sent until you type), the one on screen last time is on screen
  again, and one message says how many came back. A tab is skipped, and named, when its saved
  record is gone or another window holds its session. A tab that was in bypass is never put back in
  bypass without a yes: `s` asks first, and `n` (or `"auto"`) brings it back in auto.
- **`agent.default_mode`** is the mode a new tab starts in, for a project where you have not left
  bypass with `Shift+Tab` (that choice is remembered per project and keeps winning). Setting it to
  `"bypass"` is the one way a window starts in bypass without asking, because you have said so in
  your own file; it also lets saved bypass tabs come back in bypass without the question.

## Your tmux keys

If you use tmux, Eitri picks up your prefix and your prefix-table bindings from the same files tmux
reads (`/etc/tmux.conf`, `~/.tmux.conf`, `$XDG_CONFIG_HOME/tmux/tmux.conf`,
`~/.config/tmux/tmux.conf`, and what they `source-file`), wherever an Eitri action matches the tmux
command. It only reads them; it never starts or asks tmux. A one-time notice says what was taken,
and `<prefix> ?` lists each line that was not, with the reason. To turn it off, put
`eitri.config.set("keymap.from_tmux", "off")` in `~/.config/eitri/init.lua`; your own
`eitri.keymap` calls there always win over what came from tmux.

## Use it beside your own nvim

Eitri can also run as the agent panel alone: a separate window next to the nvim you already use in
a terminal (inside tmux or not), in upstream Neovide, or in any other nvim GUI. The panel attaches
to that nvim over its RPC socket, installs a little glue inside it (the file you have open and your
Visual selection go to the agent; the panel takes your colorscheme and shows your which-key keys;
buffers reload after the agent edits them; a file opens at a line from the panel; `Ctrl+g` edits a
draft in nvim), and removes all of it again when the panel goes away. Nothing is written to your
nvim configuration. Your editor keeps its own speed and keys, and your window manager arranges the
two windows.

**1. Add the plugin.** `eitri.nvim` is a thin launcher; everything the panel needs inside nvim, the
panel installs itself, so the plugin and the installed Eitri never have to match versions. The
packages (`.deb`, `.rpm`, AUR) put it in `/usr/share/eitri/nvim/eitri.nvim`; the tarball installer
puts it in `~/.local/share/eitri/eitri.nvim` (under `$XDG_DATA_HOME/eitri/` when that is set). With
lazy.nvim, point a spec at the directory.

```lua
-- .deb, .rpm, AUR
{ dir = "/usr/share/eitri/nvim/eitri.nvim", cmd = "EitriPanel" },

-- the tarball installer (install.sh)
{ dir = vim.fn.expand("~/.local/share/eitri/eitri.nvim"), cmd = "EitriPanel" },
```

Without a plugin manager, add the directory to `runtimepath`:
`set runtimepath+=/usr/share/eitri/nvim/eitri.nvim`. Calling `require("eitri").setup({ ... })` is
optional: `mapping = "<leader>ep"` binds a key to the command, and `cmd = "/path/to/eitri"` names the
launcher when it is not on nvim's `PATH`.

**2. Open the panel.** `:EitriPanel` in nvim opens the panel for the current working directory,
`:EitriPanel ~/some/project` for another. If nvim has no server address yet, the plugin starts one.
The panel's band says `attaching…` until the glue is in, and nothing times out: nvim answers the
request once you finish a pending key or a prompt.

**3. Or start it from a shell.**

```sh
eitri panel [--nvim <addr>] [DIR]
```

`--nvim` is the address of the nvim to attach to (`:echo v:servername`), a path to a Unix socket
that you own. It defaults to `$NVIM`, which nvim sets for its `:terminal` and `jobstart()` children;
TCP addresses (`host:port`) are refused. With no address the panel starts unattached and says so in
its band; running `:EitriPanel` in nvim attaches it later. `DIR` is the project, resolved exactly
as for `eitri DIR`.

There is one panel per project. A second `:EitriPanel`, from the same nvim or another one in the
same project, attaches the running panel to it and raises its window instead of opening a second
one; the first nvim loses the glue. If the nvim quits, the panel keeps its sessions and its band
says `editor detached: run :EitriPanel to attach again`.

The panel window has the application id `cn.huntergrey.eitri.Panel` and the title
`Eitri · <project directory name>`, so a window rule can pick it out. It reads the same
`~/.config/eitri/init.lua` as the one-window mode (`agent.account` applies); Lua panels and commands
registered there are not shown, and one line on stderr says how many were left out. It has no
editor, no bottom terminal and no layout of its own: tab keys, `?`, `:` and the text size keys work
under the prefix, and the layout keys answer `not in a companion window`.

**4. Moving between the two windows.** In nvim, `Ctrl+h/j/k/l` at the edge of nvim's own windows
hands the move to the panel, which asks your window manager to focus the neighbouring window. In the
panel, `Ctrl+h` and `Ctrl+l` always leave the window; `Ctrl+k` leaves from BROWSE (in INPUT it
switches to BROWSE), and `Ctrl+j` leaves from INPUT (in BROWSE it switches to INPUT). The prefix's
`Select` keys do the same. Opening a file from the panel raises the editor's window, and a second
`:EitriPanel` raises the panel.

Which window manager is used is detected from the session; to force one or turn it off, put this in
`~/.config/eitri/init.lua` (any other value stops the panel at startup, naming the key):

```lua
eitri.config.set("companion.wm", "auto")   -- "auto" (the default), "hyprland", "sway", "niri" or "none"
```

| desktop | detected by | what Eitri does |
|---|---|---|
| sway | `SWAYSOCK` | moves focus with `swaymsg`, after checking that a visible window really lies in that direction (on any output), so sway's default focus wrapping does not carry you to the far side. At the edge the key is consumed |
| Hyprland | `HYPRLAND_INSTANCE_SIGNATURE` | moves focus with `hyprctl dispatch movefocus`; what happens at the edge is Hyprland's own |
| niri | `NIRI_SOCKET` | moves focus with `niri msg action`; what happens at the edge is niri's own |
| GNOME, KDE, anything else | none of the above | no focus moves: a Wayland client cannot take focus there. The band says once that your desktop does not let Eitri move focus; use the desktop's own window keys |

**5. Inside tmux.** When nvim runs inside tmux, nothing in nvim's environment changes, its
navigator maps are left alone, and your tmux setup keeps moving between tmux panes as it does today.
The edge of tmux's panes is tmux's: a key that tmux handles never reaches the panel window, so
crossing to it needs a binding on the tmux side (or your window manager's keys). The editor's window
is not raised when you open a file from the panel, because from inside tmux the process tree leads
to the tmux server, not to the terminal.

**6. Navigator plugins.** With nvim outside tmux, vim-tmux-navigator needs nothing: while the panel
is attached, its `TmuxNavigate` maps are treated as plain window moves. With smart-splits.nvim, hand
a move off the edge to the panel from its `at_edge` hook; `edge` returns `false` when no panel is
attached:

```lua
require("smart-splits").setup({
  at_edge = function(ctx)
    if not require("eitri").edge(ctx.direction) then
      -- no panel attached: your own fallback, or nothing
    end
  end,
})
```

`:help eitri.nvim` has the same in nvim. What has not been tried on real hardware yet is on the
[known issues](docs/known-issues.md#companion-mode) page.

## Troubleshooting

A few common install-time refusals, in the installer's own words (abridged):

- **`run this as the user who will run Eitri`** — the installer refuses to run as root (the sidecar
  is built per user; `npm ci` as root would run every dependency's install scripts as root). Run it as
  your normal user; `--allow-root` exists only for containers.
- **`this system has GTK 4.N, and Eitri needs GTK 4.14 or newer`** / **`WebKitGTK 6.0 … was not
  found`** — your distro's GTK4/WebKitGTK are too old or missing the runtime libraries; the message
  names the exact package for your distro family. Ubuntu 22.04 and Debian 12 are below the floor;
  build from source works there too only once you separately have new-enough dev packages.
  Ubuntu 24.04, Debian 13, Fedora 40+, RHEL 10 and Arch all have it.
- **`Eitri's prebuilt binaries need glibc 2.39 or newer`** — same shape as above; the prebuilt
  binaries need a distro no older than the ones just listed, or `--from-source`.
- **`checksum mismatch for …`** — a download is corrupt or was altered in transit. The installer
  refuses to install it either way and names where to report it if it keeps happening; just re-run.
- **`the signature on SHA256SUMS does not verify`** — refuses outright; do not proceed past this on a
  real release. See [Verify before running](#verify-before-running).
- **`ssh-keygen was not found, so the release signature cannot be checked`** — install OpenSSH's
  client (`openssh-client` on Debian and Ubuntu, `openssh-clients` on Fedora, `openssh` on Arch) and
  re-run. `--insecure-skip-signature` installs without the check; see above for what that gives up.
- **the `claude` CLI was not found on `PATH`, or an unsupported version** — a warning, not a refusal:
  Eitri installs regardless, but the agent panel needs a working, logged-in Claude Code to run
  turns. The warning names Anthropic's own installer.
- **`~/.local/bin` is not on your `PATH`**, or **`\`eitri\` on this PATH runs <something else>`** —
  printed at the end of a successful install; add `~/.local/bin` to your shell's `PATH`, or put it
  before whatever else answers to `eitri` today (often `/usr/bin/eitri` from an earlier `.deb`).

- **`… is writable by its group or by anyone, and not sticky`** — the installer keeps its downloads
  under `$XDG_CACHE_HOME/eitri` (by default `~/.cache/eitri`) and refuses a cache directory other
  users could write to, since they could swap a checked download before it is used. A `0775`
  `~/.cache` is accepted when its group is your own private group (the usual setup on Ubuntu, Debian
  and Fedora), unless an ACL or a second group with the same GID gives someone else write access. The
  installer can only see the accounts your system lists: on a machine joined to a directory service
  that does not enumerate its users, it cannot rule out a directory account sharing that group, so
  there `chmod g-w ~/.cache`, or pointing `XDG_CACHE_HOME` at a directory of your own, is the safe
  choice. The same holds for every directory above the cache, up to `/`: each must be yours or
  root's, and writable by no one else unless it has the sticky bit (as `/tmp` does).
- **`… is a symlink owned by another user`** — the cache directory itself (`$XDG_CACHE_HOME`, by
  default `~/.cache`) may be a symbolic link of your own (or root's), such as a cache moved to another
  disk and linked back: the installer resolves the path once, checks every directory on the resolved
  path, and uses only that. A link that belongs to another user is refused, since its owner could point
  it at a directory of theirs; set `XDG_CACHE_HOME` to a directory of your own.

<!-- ubuntu-userns: revisit if the owner chooses the automatic sandbox-off option -->
### Ubuntu 23.10 and later

Ubuntu's `apparmor` package sets `kernel.apparmor_restrict_unprivileged_userns=1` (24.04 included),
so a process may create a Linux user namespace only while an AppArmor profile granting `userns`
confines it; WebKitGTK 6.0 always sandboxes its web and network processes with `bwrap`, which needs
one, and its 6.0 API has no switch to turn that off.

**Eitri checks for the restriction once at startup.** Where it applies, the editor and the bottom
terminal still work, and the agent panel's place shows what happened and the exact fix instead of
crashing. The curl installer prints the same fix at the end of a successful install, where it
applies.

- **`.deb`**: ships the profile as `/etc/apparmor.d/eitri`, granting exactly `userns` — the same shape
  Ubuntu's own `apparmor` package ships for `epiphany`. It has no maintainer script, so installing the
  package does not load it: run `sudo apparmor_parser -r /etc/apparmor.d/eitri` once, or restart.
- **curl / tarball / `--from-source` / AUR, and a `.rpm` if it ends up on an affected system** (no
  `.rpm`-based distribution restricts this by default): the installer — or Eitri itself, if that step
  was skipped or the restriction appeared after installing — writes a profile for your own install under
  your data directory and prints two commands naming its own paths:
  `sudo install -m 0644 <the written profile> /etc/apparmor.d/eitri-user-<uid>` then
  `sudo apparmor_parser -r /etc/apparmor.d/eitri-user-<uid>`. Run them once, where they are printed.

**The cost.** Past `userns`, the profile is otherwise unconfined: every process Eitri starts — the
editor's `nvim`, the bottom terminal's shell, and any command the agent's Bash tool runs — inherits it,
so all of them may create user namespaces, not only WebKit's own sandbox helper. For a per-user install,
the profile is attached by the literal path your own `shell` binary runs from, a path under your `$HOME`
that you (or anything already running as you) can overwrite — the grant follows whatever sits there.

**Escape hatch, instead of applying the fix:** start Eitri from a terminal with
`WEBKIT_DISABLE_SANDBOX_THIS_IS_DANGEROUS=1 eitri`. This removes the operating system's sandbox from
the whole process that renders the model's output, not only from this restriction, and only works from
a terminal — a launch from the app menu or dock does not carry the variable.
<!-- /ubuntu-userns -->
