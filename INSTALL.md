English | [简体中文](INSTALL.zh-CN.md)

# Installing Eitri

**You need:** x86_64 Linux with GTK ≥ 4.14, WebKitGTK 6.0 and glibc ≥ 2.39 (Ubuntu 24.04, Debian 13,
Fedora 40+, RHEL 10, Arch); **nvim ≥ 0.10** (the installer can fetch a private copy for Eitri if yours is
older); and **Claude Code**, installed and logged in.

## Quick install

```sh
curl --proto '=https' --proto-redir '=https' --tlsv1.2 -sSfL https://github.com/HunterGrey-cyber/eitri/releases/latest/download/install.sh | sh
```

This is the recommended way, on every distribution; on Arch, the [AUR package](#aur-arch) fits better.
It installs into `~/.local`, without `sudo`, and builds the agent sidecar on your machine
([why](#why-the-sidecar-is-built-on-your-machine)), which takes a few minutes, network access and about
600 MiB of disk. Options go after `sh -s --`, for example `… | sh -s -- --version 0.2.1 --yes`;
`sh install.sh --help` lists them all.

To check the download's signature before running it, see [Verify before running](#verify-before-running).

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
is older than 0.10. `eitri setup --help` lists its other modes. There is no package repository, so each
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
contains it.

## Updating

Run the same route again. `sh install.sh` does nothing if you already have the latest release. Otherwise
it builds the new sidecar first and switches only when that worked, so a failed update leaves your install
as it was. Restart open Eitri windows afterwards.

For a `.deb` or `.rpm`, upgrade with your package manager, then run `eitri setup` again (it does nothing
when the sidecar is already built).

Coming from 0.2.0: the desktop entry was renamed, so pin Eitri to your dock again once.

## Uninstalling

```sh
sh install.sh --uninstall            # keeps ~/.config/eitri and $XDG_STATE_HOME/eitri
sh install.sh --uninstall --purge    # removes those too
```

This removes everything the installer put in place: the program, the launcher, the desktop entry, the
icons, the GNOME Shell extension, the sidecars, the private nvim and the download cache. It never touches
an `nvim` of your own. Disable the GNOME Shell extension first if you turned it on.

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
| private nvim, if you accepted it | `$XDG_DATA_HOME/eitri/nvim/<version>/` | the same |

For every route, your config is `~/.config/eitri/init.lua`, and per-project state (layout, open tabs,
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
- **`checksum mismatch for …`**: the download is corrupt. Run the installer again.
- **`the signature on SHA256SUMS does not verify`**: stop, and do not install this release.
- **`ssh-keygen was not found, so the release signature cannot be checked`**: install the OpenSSH client
  (`openssh-client` on Debian and Ubuntu, `openssh-clients` on Fedora, `openssh` on Arch).
  `--insecure-skip-signature` installs without checking the signature; only the checksums are checked then.
- **`claude` was not found on `PATH`**: a warning only. The panel needs Claude Code installed and logged in
  to run turns.
- **`~/.local/bin` is not on your `PATH`**, or **`eitri` on this PATH runs /usr/bin/eitri**: put
  `~/.local/bin` first on your `PATH`.
- **`… is writable by its group or by anyone, and not sticky`** or **`… is a symlink owned by another
  user`**: the installer keeps its downloads in `~/.cache/eitri` and refuses a cache that other users could
  write to. Run `chmod g-w ~/.cache`, or set `XDG_CACHE_HOME` to a directory of your own.

<!-- ubuntu-userns: revisit if the owner chooses the automatic sandbox-off option -->
### Ubuntu 23.10 and later

Ubuntu (24.04 included) lets a program create user namespaces only under an AppArmor profile that allows
it, and WebKitGTK needs them for its sandbox. Eitri checks at startup: the editor and the terminal still
work, and the agent panel shows the fix instead of crashing. The installer prints the same fix.

- **`.deb`**: the package ships the profile `/etc/apparmor.d/eitri`. Load it once with
  `sudo apparmor_parser -r /etc/apparmor.d/eitri`, or restart.
- **Every other route**: the installer (or Eitri, at startup) writes a profile for your install and prints
  the two `sudo` commands that put it in place. Run them once.

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
