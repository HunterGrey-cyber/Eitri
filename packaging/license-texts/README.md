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
| `chromium.LICENSE` | Chromium's additions to its zlib fork (`Cr_z_adler32_simd_`, `Cr_z_crc32_sse42_simd_`, `Cr_z_inflate_fast_chunk_`, `Cr_z_cpu_check_features` defined in `shell`) | `https://chromium.googlesource.com/chromium/src/+/main/LICENSE?format=TEXT` (base64-decoded), fetched 2026-09-19 | those files (`adler32_simd.c`, `crc32_simd.c`, `contrib/optimizations/chunkcopy.h`, ...) say "governed by a BSD-style license that can be found in the Chromium source repository LICENSE file"; zlib's own notice does not cover them. Added by the 2026-09-19 adversarial review |
| `Apache-2.0.txt` | Wuffs, same route -- **used again as of 2026-09-27**: `shell` still carries no Wuffs code (see below), but `libwuffs.wuffs-v0.3.o` inside the prebuilt Skia archive does, and verdict #7 (below) means the archive-contents section notices it even though the shipped binary does not | `/usr/share/licenses/spdx/Apache-2.0.txt` (Arch `licenses 20240728-1`) | same reason; Wuffs is Apache-2.0 and its notice line is in the script |
| `GPL-3.0.txt` | crate `nvim-rs 0.9.2` | `/usr/share/licenses/spdx/GPL-3.0-only.txt` | nvim-rs is LGPL-3.0, and LGPL-3.0 §4(b) requires a copy of the GNU GPL to accompany the object code; the crate ships only the LGPL text |
| `expat.COPYING` | expat, inside the prebuilt Skia archive (`libskia.a`'s `libexpat.*` members) | `https://raw.githubusercontent.com/libexpat/libexpat/6154446fccefbf3ca644894f598969113b0c7bcd/expat/COPYING`, fetched 2026-09-27 | verdict #7 (Codex, 2026-09-27): the whole prebuilt archive ships in the public source asset (spec D9), and expat is compiled into `libskia.a` even though `--gc-sections` drops it from the shipped `shell` binary, so `native_components()` never sees it. The commit is Skia's own `DEPS` pin for `third_party/externals/expat`, read at the exact Skia revision (`61e7ca4e99062cdd0ab69445d5963fb3365778f6`) rust-skia tag `0.153.3`'s `.gitmodules` submodule points at |
| `harfbuzz.COPYING` | HarfBuzz, inside the prebuilt Skia archive (`libskia.a`'s and `libskshaper.a`'s `libharfbuzz.*` members) | `https://raw.githubusercontent.com/harfbuzz/harfbuzz/9cb1fee51069b206effb4736e443b038d230789d/COPYING`, fetched 2026-09-27 | same reason and same Skia `DEPS` pin route, `third_party/externals/harfbuzz` |
| `icu.LICENSE` | ICU, inside the prebuilt Skia archive (`libskunicode_icu.a`'s `libicu.*` members) | `https://chromium.googlesource.com/chromium/deps/icu.git/+/d578f2e8b7bd5938e21cfb6bf15c079e0aa5b738/LICENSE?format=TEXT`, fetched 2026-09-27 | same reason and same Skia `DEPS` pin route, `third_party/externals/icu` (Unicode-3.0) |
| `libjpeg-turbo.LICENSE.md` | libjpeg-turbo, inside the prebuilt Skia archive (`libskia.a`'s `libjpeg.*`/`libjpeg12.*`/`libjpeg16.*` members) | `https://chromium.googlesource.com/chromium/deps/libjpeg_turbo.git/+/e14cbfaa85529d47f9f55b0f104a579c1061f9ad/LICENSE.md?format=TEXT`, fetched 2026-09-27 | same reason and same Skia `DEPS` pin route, `third_party/externals/libjpeg-turbo`. This file states the IJG License and reproduces the Modified (3-clause) BSD License in full; it explains that the zlib-licensed SIMD sources are subsumed by the IJG License and that condition 1.3 of the IJG License requires the README.ijg file itself (below) to accompany any source redistribution |
| `libjpeg-turbo-ijg.README` | the IJG License's own text (condition 1.3 of `libjpeg-turbo.LICENSE.md`, above) | same commit, `README.ijg`, fetched 2026-09-27 | an **excerpt**: only the "LEGAL ISSUES" section (the actual licence terms) is vendored; the rest of the upstream README is JPEG-algorithm documentation, not part of the licence |
| `gcc-COPYING.RUNTIME` | GCC's C runtime startup code (`crtstuff.c`, via `crtbeginS.o`/`crtendS.o`), linked by the system C compiler into every shipped binary | `https://raw.githubusercontent.com/gcc-mirror/gcc/releases/gcc-13.3.0/COPYING.RUNTIME` (GCC 13.3.0, the release container's `gcc-13 13.3.0-6ubuntu2~24.04.1`), fetched 2026-09-28; word for word the exception section of that image's own `/usr/share/doc/gcc-13-base/copyright`, and byte-identical to GCC `master`'s | Ubuntu 24.04's GCC keeps `crtstuff.c`'s STT_FILE entry, so `native_components()`'s closed set of C files found it in the first container build (lane D Task 4 rehearsal, 2026-09-28); an Arch host build never showed it. The GCC Runtime Library Exception 3.1 requires no notice for a program GCC compiled; the text is vendored so the file still maps to a component with a text |

The versions of FreeType, libpng, zlib and Wuffs **inside** the prebuilt Skia were not determined;
the texts above are those libraries' current licences, which have been stable for years. That is
an inference, not a measurement. **Correction (2026-09-27):** the four rows added that day (expat,
HarfBuzz, ICU, libjpeg-turbo) are not an inference -- they are read at the exact commit Skia's own
`DEPS` file pins for `tag.txt`'s `0.153.3` (rust-skia's own release tag), found by walking rust-skia's
`.gitmodules` submodule pin to its `skia` fork, then that revision's `DEPS`. Whether that exact
version match extends to FreeType/libpng/zlib/Wuffs above was not re-checked here.

**Correction (2026-09-19, adversarial review).** Two of those versions can be read out of `shell`:
libpng is **1.6.56** (its version string sits in `.rodata`), and `pnggroup/libpng`'s `LICENSE` at tag
`v1.6.56` is byte-identical to the vendored 1.6.58 text; zlib is Chromium's fork, **"1.3.0.1-motley"**,
so `zlib.NOTICE` (from zlib 1.3.2) may carry later copyright years than the embedded copy -- the
permission text is the same. FreeType's version was not found as a string; its `docs/FTL.TXT` at
`VER-2-14-2` is byte-identical to the vendored one anyway. **Wuffs is not in `shell` at all:** the
`wuffs-v0.3.c` string the first version detected is an `STT_FILE` symbol-table entry that survives
`--gc-sections` after every byte of Wuffs was discarded (no `wuffs_` symbol, no Wuffs status string),
so it is no longer listed. The same holds for HarfBuzz, ICU, libjpeg-turbo and expat, whose
`FILE` entries are also in `shell` with no code behind them.
