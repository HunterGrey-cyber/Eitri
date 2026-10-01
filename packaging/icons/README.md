# The Eitri icon

The application icon, as installed under the icon theme's `hicolor` tree:

- `eitri.svg`: the mark, drawn on a 128 px grid. `hicolor/scalable/apps/cn.huntergrey.eitri.svg` is
  the same file.
- `eitri-16.svg`: a hand-placed 16 px drawing, the source of the 16 px PNG alone.
- `hicolor/<size>x<size>/apps/cn.huntergrey.eitri.png`: 16, 24, 32, 48, 64, 128, 256 and 512 px. The
  16 px file is `eitri-16.svg` and every other size is `eitri.svg`, each rendered with
  `rsvg-convert -w <size> -h <size>`. They are committed as files, so no build step needs an SVG
  renderer.

The icon name is `cn.huntergrey.eitri`, the application id: `packaging/cn.huntergrey.eitri.desktop`
names it (`Icon=`), and the window is matched to that entry by the same id.

Licence of the logo: see [`LICENSE`](LICENSE) in this directory. It is a notice, not code, and the
MIT licence of Eitri's own code does not cover it:

> The Eitri logo (c) Hunter Grey, licensed under CC BY 4.0 (https://creativecommons.org/licenses/by/4.0/). Its blue and green come from the Neovim logo by Jason Long (CC BY 3.0).

`packaging/collect-licenses.py` prints that notice into `THIRD-PARTY-LICENSES`, and fails when a file
here is not one it lists or the notice is missing, so the credit travels with every package.
