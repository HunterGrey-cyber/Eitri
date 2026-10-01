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

## Stability

0.2.0 was the first public release. The `init.lua` API and the default keys may change during 0.x; the goal for
0.3 is a version stable enough to be our own everyday editor.

## Not on this list?

[Open a bug report](https://github.com/HunterGrey-cyber/eitri/issues/new/choose), or ask in
[Discussions](https://github.com/HunterGrey-cyber/eitri/discussions) first if you are unsure it is one.
