# Vendored licence texts

`packaging/collect-licenses.py` reads every licence text it can from the live dependency trees
(the cargo registry, `node_modules`, the Verdandi build cache, the Skia build output). This
directory holds only the texts it **cannot** find there, each for a stated reason. The script maps
each file to an exact `(name, version)`, so a version bump fails the collector until someone
re-checks the text here -- a vendored file never silently covers a release it was not checked for.

| file | covers | where it came from (2026-09-19) | why it is vendored |
|---|---|---|---|
| `standard-webhooks.LICENSE` | npm `standardwebhooks@1.1.1` (sidecar) | `https://raw.githubusercontent.com/standard-webhooks/standard-webhooks/main/libraries/LICENSE` | the published package declares MIT and ships no LICENSE file. The repository root's `LICENSE` is Apache-2.0; the `libraries/` one (MIT, Svix) is the one covering `libraries/javascript`, which is what npm publishes |
| `gl-rs.LICENSE` | crate `gl 0.14.0` | `https://raw.githubusercontent.com/brendanzab/gl-rs/master/LICENSE` | the crate declares Apache-2.0 and ships no LICENSE file |
| `webkit6-rs.LICENSE` | crates `webkit6 0.6.1`, `webkit6-sys 0.6.0`, `javascriptcore6 0.6.0`, `javascriptcore6-sys 0.6.0` | `https://gitlab.gnome.org/World/Rust/webkit6-rs/-/raw/main/LICENSE` (no `0.6.1` tag exists to pin to) | the four crates declare MIT and ship no LICENSE file |
| `rust-skia.LICENSE` | crates `skia-bindings 0.153.3`, `skia-safe 0.153.3` | `https://raw.githubusercontent.com/rust-skia/rust-skia/0.153.3/LICENSE` (the release tag) | the crates declare MIT and ship no LICENSE file |
| `freetype-FTL.TXT` | FreeType, statically linked into `shell` inside the prebuilt Skia | `/usr/share/licenses/freetype2/FTL.TXT` from Arch `freetype2 2.14.3-1` | rust-skia's prebuilt `libskia.a` embeds FreeType but ships no FreeType licence. FreeType is FTL-or-GPLv2; this package takes the FTL |
| `libpng.LICENSE` | libpng, same route | `/usr/share/licenses/libpng/LICENSE` from Arch `libpng 1.6.58-2` | same reason |
| `zlib.NOTICE` | zlib inflate code, same route | the notice block of `/usr/include/zlib.h` (zlib 1.3.2) | same reason. The embedded copy's own version (and therefore its copyright years) was not determined |
| `Apache-2.0.txt` | Wuffs, same route | `/usr/share/licenses/spdx/Apache-2.0.txt` (Arch `licenses 20240728-1`) | same reason; Wuffs is Apache-2.0 and its notice line is in the script |
| `GPL-3.0.txt` | crate `nvim-rs 0.9.2` | `/usr/share/licenses/spdx/GPL-3.0-only.txt` | nvim-rs is LGPL-3.0, and LGPL-3.0 §4(b) requires a copy of the GNU GPL to accompany the object code; the crate ships only the LGPL text |

The versions of FreeType, libpng, zlib and Wuffs **inside** the prebuilt Skia were not determined;
the texts above are those libraries' current licences, which have been stable for years. That is
an inference, not a measurement.
