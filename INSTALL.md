English | [简体中文](INSTALL.zh-CN.md)

# Installing Eitri

Check these first:

- **System:** x86_64 Linux with GTK ≥ 4.14, WebKitGTK 6.0 and glibc ≥ 2.39. Ubuntu 24.04, Debian 13, Fedora 40+,
  RHEL 10 and Arch qualify; Ubuntu 22.04 and Debian 12 are too old.
- **Neovim ≥ 0.11.** If yours is older, the installer can fetch a private copy for Eitri and leave your own nvim alone.
- **Claude Code** (the `claude` command), installed and set up.

## Quick install

```sh
curl --proto '=https' --proto-redir '=https' --tlsv1.2 -sSfL https://github.com/HunterGrey-cyber/eitri/releases/latest/download/install.sh | sh
```

This is the recommended way on every distribution; on Arch, the [AUR package](#aur-arch) fits better.

- It installs into `~/.local`, without `sudo`.
- It builds the agent sidecar on your machine ([why](#why-the-sidecar-is-built-on-your-machine)), which takes a few
  minutes, network access and about 1.4 GiB of free disk space while it builds.
- Options go after `sh -s --`, for example `… | sh -s -- --version 0.4.0 --yes`; `sh install.sh --help` lists them all.
- This installer installs 0.4.0 and later; it refuses 0.3.0 and older. To install an older release, use that release's
  own `install.sh` (its release asset, for example `https://github.com/HunterGrey-cyber/eitri/releases/download/v0.3.0/install.sh`).

To check the download's signature before running it, see [Verify before running](#verify-before-running).

### On a slow or filtered network

Every file the installer downloads (the release files, Node, the npm packages, the compiler for the sidecar and
the private nvim) is tried again when the connection breaks or stalls, and a file download picks up where it stopped.
Each file is still checked against the release's signature, its pinned hash or npm's lockfile afterwards, so a
retried or resumed file that is not the right one is refused. When a download still fails, the message names the
file, the host and what was tried.

If those hosts are reachable only through a proxy, set it before running the line: curl, npm and the installer all
read `https_proxy`.

```sh
export https_proxy=http://127.0.0.1:7890    # your proxy's address
```

`--base-url URL` takes Eitri's own release files from a mirror laid out like the GitHub release page; Node, npm,
the compiler and nvim still come from their usual hosts.

**If `install.sh` itself does not arrive in full**, `sh` runs none of it: a cut-off script ends in a syntax error and
a non-zero exit status. A download that breaks before its first byte, though, hands `sh` an empty script, and the
line then exits 0 with only curl's `curl: (NN) …` line to show for it. In a script, or whenever the exit status
matters, download first and then run it:

```sh
curl --proto '=https' --proto-redir '=https' --tlsv1.2 --retry 5 --retry-all-errors -sSfL -o eitri-install.sh https://github.com/HunterGrey-cyber/eitri/releases/latest/download/install.sh && sh eitri-install.sh
```

In bash or zsh, `set -o pipefail` before the quick-install line also makes it fail with curl's status. Do not add
`--retry` to the piped line: curl cannot take back what it already passed to `sh`, so a retry sends the script
again after the part that already went.

## AUR (Arch)

```sh
yay -S eitri-bin    # prebuilt binaries; the sidecar is built while the package builds
yay -S eitri-git    # everything built from source
```

Updates come with the rest of your system (`yay -Syu`). nvim and Claude Code are optional dependencies, so
a self-built or `bob`-managed nvim is left alone.

## Other ways to install

### `.deb` / `.rpm`

Download the package from the [releases page](https://github.com/HunterGrey-cyber/eitri/releases), install it,
then run `eitri setup` once as your own user:

```sh
sudo apt install ./eitri_<version>_amd64.deb       # Debian, Ubuntu
sudo dnf install ./eitri-<version>-1.x86_64.rpm    # Fedora, RHEL
eitri setup
```

The packages run no install scripts, so `eitri setup` builds the sidecar and offers a private nvim if yours
is older than 0.11. `eitri setup --help` lists its other modes. There is no package repository, so each
update is a new download.

### From source

```sh
sh install.sh --from-source
```

This builds the release's own tag and installs it like the prebuilt route. It needs Rust 1.96 or newer, the
GTK 4.14 and WebKitGTK 6.0 development packages, `pkg-config`, Node.js and npm, `protoc`, a C toolchain and
network access; [CONTRIBUTING](CONTRIBUTING.md#prerequisites) has the package names per distribution.
`--checkout DIR` builds a local working tree instead. It builds 0.2.1 and later
(for 0.2.0, see the [known issues](docs/known-issues.md#installing)).

### Offline, from release files

```sh
sh install.sh --tarball FILE --sums FILE --sig FILE
```

Same checks as the other routes. Building the sidecar still needs network access.

## Why the sidecar is built on your machine

The sidecar bundles Anthropic's `@anthropic-ai/claude-agent-sdk` ("© Anthropic PBC. All rights reserved"),
which this project does not redistribute. The installer fetches it from npm onto your own machine, under
Anthropic's terms, and builds it from the Verdandi revision the release pins. No Eitri release file
contains it. A sidecar update ([below](#updating-the-sidecar)) is built the same way, on your machine from signed
source, so it puts nothing of Anthropic's into any release file either.

## Updating

Run the same route again.

- `sh install.sh` does nothing if you already have the latest release. Otherwise it builds the new sidecar first and
  switches only when that worked, so a failed update leaves your install as it was. Restart open Eitri windows
  afterwards.
- For a `.deb` or `.rpm`, upgrade with your package manager, then run `eitri setup` again (it does nothing when the
  sidecar is already built).

Coming from 0.2.2: the update builds the agent sidecar again, since 0.3.0 pins a newer one, and Eitri now reads
its settings from `$XDG_CONFIG_HOME/eitri` when `XDG_CONFIG_HOME` is set to an absolute path (0.2.2 always read
`~/.config/eitri`), so move them there if the two differ.

Coming from 0.2.0: the desktop entry was renamed, so pin Eitri to your dock again once.

## Updating the sidecar

Claude Code updates itself often, and now and then a new version needs a newer sidecar. Sidecar updates come out on
their own, between Eitri releases, and you take one when you choose to. Eitri 0.3.0 cannot take them, and has no
`eitri sidecar` command: on 0.3.0, `eitri sidecar status` answers `eitri: no such directory`. Install the release after
it first (see [Updating](#updating) above); after that the commands below exist.

```sh
eitri sidecar status        # what new tabs run, what came with Eitri, what is kept; no network
eitri sidecar check         # asks whether a newer sidecar release exists; changes nothing
eitri sidecar update        # downloads it, checks the signature, builds it here and selects it
eitri sidecar use bundled   # new tabs go back to the sidecar that came with Eitri
eitri sidecar use 0.3.3     # select an update that is already built
```

- **Nothing runs by itself.** Eitri does not look for a sidecar release in the background and shows no notice that
  one exists. `check` is the only way it asks, and only when you run it.
- **An update is built on your machine**, from signed source, the way `eitri setup` builds the first sidecar. It needs
  network access to nodejs.org, the npm registry and grpc.io, and about 1.2 GiB of free disk space while it runs (one
  build we measured peaked at 1.15 GiB and left about 106 MiB behind). It took half a minute on a fast connection, so
  allow a few minutes.
- **The signature must verify, or nothing is installed.** A sidecar release is signed with the same key as Eitri's own
  releases, and there is no option to skip the check. Without `ssh-keygen` (the OpenSSH client), `eitri sidecar update`
  stops and names the package to install.
- **Open Claude tabs keep the sidecar they started with.** New tabs use the update.
- **The sidecar that came with Eitri stays as the fallback.** If an update will not start, or lacks something this Eitri
  needs, the new tab runs the bundled sidecar instead, its band shows `⚠`, and the tab's details (`Space i`, or `/status`)
  say why. `eitri sidecar use bundled` stops the attempts, and the update stays on disk until you select it again.
- **An update older than the sidecar that came with Eitri is never used.** After an Eitri upgrade that brings a newer
  sidecar, the old update stays on disk, unused, and `eitri sidecar status` says so. After a `.deb` or `.rpm` upgrade,
  run `eitri setup` first: an update needs the bundled sidecar built behind it.
- **`EITRI_SIDECAR_BINARY` and `EITRI_VERDANDI_CHECKOUT` win** over an update when you set them.
- **Where the command exists.** It runs from the `eitri-setup` that sits beside Eitri's binary: curl and tarball
  installs, the `.deb` and `.rpm`, and the AUR packages. A package that carries its own sidecar and no `eitri-setup`
  answers `this package updates its sidecar itself`.

## Uninstalling

```sh
sh install.sh --uninstall            # keeps your settings and $XDG_STATE_HOME/eitri
sh install.sh --uninstall --purge    # removes those too
```

Your settings are the directory Eitri reads them from: `$XDG_CONFIG_HOME/eitri` when `XDG_CONFIG_HOME`
is non-empty and absolute, else `~/.config/eitri`. `--purge` never removes a directory `EITRI_CONFIG_DIR`
names, since that may hold another program's settings too; it says so, and you remove it yourself.

This removes what the installer put in place under your home: the program, the launcher, the desktop entry, the
icons, the GNOME Shell extension, the sidecars (updates you took included), the private nvim and the download cache. It never touches
an `nvim` of your own. Disable the GNOME Shell extension first if you turned it on.

The one thing it leaves is the AppArmor profile of a per-user install on Ubuntu (`/etc/apparmor.d/eitri-user-<uid>`,
see [Ubuntu 23.10 and later](#ubuntu-2310-and-later)): that file belongs to root and the uninstall never runs `sudo`.
It prints the two `sudo` commands that unload and delete it; run them if you want the profile gone.

For a `.deb` or `.rpm`: remove the package first, then run `sh install.sh --uninstall` to remove the
per-user sidecar and private nvim, which the package does not own (this also removes a curl install under
`~/.local`, if you have one). Do it in this order: while the package is installed, its sidecar is kept.

## Where things go

| | curl or tarball install | `.deb` / `.rpm` / AUR |
|---|---|---|
| program | `~/.local/lib/eitri/` | `/usr/lib/eitri/` |
| `eitri` launcher | `~/.local/bin/eitri` | `/usr/bin/eitri` |
| desktop entry, icons, licences | under `~/.local/share/` | under `/usr/share/` |
| the `:EitriPanel` plugin | `~/.local/share/eitri/eitri.nvim/` | `/usr/share/eitri/nvim/eitri.nvim/` |
| GNOME Shell extension | `~/.local/share/gnome-shell/extensions/eitri@huntergrey.cn/` | `/usr/share/gnome-shell/extensions/eitri@huntergrey.cn/` |
| agent sidecar | `$XDG_DATA_HOME/eitri/sidecar/<rev>/` | the same, per user (AUR: `/usr/lib/eitri/`) |
| sidecar updates, and which one new tabs use | `$XDG_DATA_HOME/eitri/sidecar/updates/<version>/` and `.../sidecar/selected` | the same, per user |
| private nvim, if you accepted it | `$XDG_DATA_HOME/eitri/nvim/<version>/` | the same |

For every route on Linux, your config is `$XDG_CONFIG_HOME/eitri/init.lua` when `XDG_CONFIG_HOME`
is non-empty and absolute, else `~/.config/eitri/init.lua`. The Mac uses `~/.config/eitri/init.lua`.
A non-empty `EITRI_CONFIG_DIR` overrides the directory and must be absolute; a relative value refuses
to start. Per-project state (layout, open tabs,
prompt history, permission rules, trust answers, turn review snapshots) is in `$XDG_STATE_HOME/eitri/`.
`$XDG_DATA_HOME` and `$XDG_STATE_HOME` default to `~/.local/share` and `~/.local/state`. The private nvim
is never put on your `PATH`.

## Troubleshooting

What the installer says, abridged:

- **`run this as the user who will run Eitri`**: do not run it as root; the sidecar is built per user.
  `--allow-root` is only for containers.
- **`this system has GTK 4.N, and Eitri needs GTK 4.14 or newer`**, **`WebKitGTK 6.0 … was not found`** or
  **`Eitri's prebuilt binaries need glibc 2.39 or newer`**: the distribution is too old (Ubuntu 22.04 and
  Debian 12 are) or a runtime package is missing; the message names the package for your distribution.
- **`could not download … from …: curl: (NN) …; N attempts over N s to <host>`**: the network kept breaking that
  download. Set `https_proxy` if that host is reachable only through a proxy, or use `--base-url` for Eitri's own
  release files ([On a slow or filtered network](#on-a-slow-or-filtered-network)).
- **`checksum mismatch for …`**: the download is corrupt. Run the installer again.
- **`the signature on SHA256SUMS does not verify`**: stop, and do not install this release.
- **`ssh-keygen was not found, so the release signature cannot be checked`**: install the OpenSSH client
  (`openssh-client` on Debian and Ubuntu, `openssh-clients` on Fedora, `openssh` on Arch).
  `--insecure-skip-signature` installs without checking the signature; only the checksums are checked then.
- **`claude` was not found on `PATH`**: a warning only. The panel needs the `claude` you installed and set up
  to run turns. Get it at <https://code.claude.com/docs/en/quickstart>. Until Eitri finds `claude`, the first-start
  screen and a Claude tab's failure row say so and give the same address.
- **`~/.local/bin` is not on your `PATH`**, or **`eitri` on this PATH runs /usr/bin/eitri**: put
  `~/.local/bin` first on your `PATH`.
- **`… is writable by its group or by anyone, and not sticky`** or **`… is a symlink owned by another
  user`**: the installer keeps its downloads in `~/.cache/eitri` and refuses a cache that other users could
  write to. Run `chmod g-w ~/.cache`, or set `XDG_CACHE_HOME` to a directory of your own.

<!-- ubuntu-userns: revisit if the maintainer chooses the automatic sandbox-off option -->
### Ubuntu 23.10 and later

Ubuntu (24.04 included) lets a program create user namespaces only under an AppArmor profile that allows
it, and WebKitGTK needs them for its sandbox. Eitri checks at startup: the editor and the terminal still
work, and the agent panel shows the fix instead of crashing. The installer prints the same fix.

- **`.deb`**: the package ships the profile `/etc/apparmor.d/eitri`. Load it once with
  `sudo apparmor_parser -r /etc/apparmor.d/eitri`, or restart.
- **Every other route**: the installer (or Eitri, at startup) writes a profile for your install and ends by
  saying that the agent panel stays off until you run the two `sudo` commands it prints. Run them once, then start
  (or restart) Eitri. The installer never runs `sudo` itself.

**The cost:** the profile lets every process Eitri starts create user namespaces, including the editor,
the terminal's shell and the commands the agent runs. For a per-user install the profile is tied to a path
in your home directory, so it covers whatever program is put there.

**Without the fix:** `WEBKIT_DISABLE_SANDBOX_THIS_IS_DANGEROUS=1 eitri`, from a terminal. This turns off
WebKit's sandbox for the window that renders the model's output.
<!-- /ubuntu-userns -->

## Verify before running

The release also publishes `SHA256SUMS` and its signature `SHA256SUMS.sig`. Download all three, and get
the release key, `packaging/release-signers`, from this repository at the release's
tag:

```sh
base=https://github.com/HunterGrey-cyber/eitri/releases/latest/download
for f in install.sh SHA256SUMS SHA256SUMS.sig; do
  curl --proto '=https' --proto-redir '=https' --tlsv1.2 -fsSL -o "$f" "$base/$f"
done
git clone --branch v<version> --depth 1 https://github.com/HunterGrey-cyber/eitri.git eitri-signers
cp eitri-signers/packaging/release-signers .
```

Then check the signature, check `install.sh` against the signed sums, and only then run it:

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

`install.sh` checks the same signature on everything it downloads after that. The key and the release
both come from GitHub, so this catches a corrupt or swapped file, not a compromised GitHub account.
