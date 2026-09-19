#!/usr/bin/env python3
"""Regenerate dist/THIRD-PARTY-LICENSES from the live dependency trees of what the package ships.

    packaging/collect-licenses.py                 # writes dist/THIRD-PARTY-LICENSES
    packaging/collect-licenses.py --out FILE      # somewhere else

Why this exists: the .deb/.pkg ship statically linked Rust binaries, an embedded web bundle, and a
Node single-executable sidecar with ~135 npm packages inside it. MIT, BSD and ISC all require the
copyright notice itself to travel with a binary copy, so an SPDX id per package is not enough; this
emits each package's own licence TEXT. Decision and reasoning:
docs/superpowers/specs/2026-09-19-public-release-design.md, "Third-party licences in the package".

It FAILS -- exit 1, and no output file -- rather than warn, on:
  * a licence expression it cannot classify, or one that is copyleft and not acknowledged below;
  * a package whose licence text it cannot find, unless allowlisted below with a reason;
  * a vendored override that no longer matches (the package now ships its own text, or its version
    moved), so an override never silently covers a release it was not checked against;
  * native code found in a shipped binary that it has no licence text for, including any C source
    file that still owns code and belongs to no known component;
  * a shipped binary with no symbol table (a stripped binary would otherwise hide all native code);
  * binaries built by a different rustc from the one whose standard-library notice it reads;
  * a sidecar artifact that does not match the Verdandi checkout it is reading the npm tree from.

Deterministic by construction: every list is sorted, nothing reads the clock, and identical texts
are printed once and referred to afterwards. Running it twice gives byte-identical output.

Inputs it expects to exist (publish.sh produces all of them before calling it):
  target/release/{shell,agent-hook,neovibe-supervisor,neovibe-tmux-shim,neovibe-claude-handoff}
  dist/verdandi-claude-sidecar
  agent-ui/web/node_modules                       (shell/build.rs runs npm ci there)
  $NEOVIBE_VERDANDI_CHECKOUT (default ~/src/verdandi), with node_modules installed and
  apps/claude-sidecar/build/node-cache/ holding the Node the artifact was built from
"""

import glob
import hashlib
import html.parser
import json
import os
import re
import struct
import subprocess
import sys

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
TEXTS = os.path.join(REPO, "packaging", "license-texts")
TARGET = "x86_64-unknown-linux-gnu"

# The five binaries packaging/nfpm.yaml installs from target/release, and the cargo packages whose
# bin targets they are (publish.sh builds exactly `-p shell -p agent -p supervisor --bins`).
SHIPPED_BINARIES = ["shell", "agent-hook", "neovibe-supervisor", "neovibe-tmux-shim", "neovibe-claude-handoff"]
SHIPPED_CARGO_PACKAGES = ["shell", "agent", "supervisor"]

# ---------------------------------------------------------------------------------------------
# Classification
# ---------------------------------------------------------------------------------------------

PERMISSIVE = {
    "MIT", "MIT-0", "Apache-2.0", "BSD-2-Clause", "BSD-3-Clause", "ISC", "Zlib", "Unlicense",
    "0BSD", "CC0-1.0", "Unicode-3.0", "Unicode-DFS-2016", "BSL-1.0", "BlueOak-1.0.0",
}
EXCEPTIONS = {"LLVM-exception"}
# Known, classifiable, and NOT acceptable without a per-package acknowledgement below.
COPYLEFT = {
    "MPL-2.0", "LGPL-2.1", "LGPL-2.1-only", "LGPL-2.1-or-later", "LGPL-3.0", "LGPL-3.0-only",
    "LGPL-3.0-or-later", "GPL-2.0", "GPL-2.0-only", "GPL-2.0-or-later", "GPL-3.0", "GPL-3.0-only",
    "GPL-3.0-or-later", "AGPL-3.0", "AGPL-3.0-only", "AGPL-3.0-or-later", "EPL-2.0", "CDDL-1.0",
}

# Copyleft that ships, acknowledged one package at a time. A NEW copyleft dependency fails the
# collector until someone adds it here, which is the point: each one carries an obligation.
COPYLEFT_ACK = {
    ("cargo", "nvim-rs"): (
        "LGPL-3.0 only (its README: a fork of neovim-lib; new commits are also MIT/Apache, but the "
        "crate as published is LGPL-3.0). Pulled in by the Neovide fork and STATICALLY linked into "
        "`shell`. LGPL-3.0 section 4(d) then requires the recipient be able to relink against a "
        "modified nvim-rs, which for a Rust static link means offering neovibe's own source. See "
        "the spec's 'Third-party licences in the package' for whether that is met."),
    ("cargo", "option-ext"): (
        "MPL-2.0, file-level copyleft. Used unmodified (via `dirs-sys`); its source is published on "
        "crates.io. MPL-2.0 section 3.2 is met by stating where that source is, which the header does."),
}

# Packages with no licence field or no licence text, shipped anyway, each for a stated reason.
# Exactly these. Anything else without a classifiable licence and a findable text fails.
ALLOWLIST = {
    ("cargo", "claude-runtime-protocol"): (
        "Verdandi's own generated gRPC types, same owner as neovibe. The Verdandi repository states "
        "no licence yet; the owner has decided it will be MIT when verdandi-public is cut."),
    ("npm", "@verdandi/claude-sidecar"): (
        "Verdandi's own sidecar, same owner as neovibe; no licence stated yet (to be MIT)."),
    ("npm", "@verdandi/claude-runtime"): (
        "Verdandi's own runtime package, same owner as neovibe; no licence stated yet (to be MIT)."),
    ("npm", "@anthropic-ai/claude-agent-sdk"): (
        "NOT open source. Bundled into the sidecar knowingly (the owner's decision B, 2026-09-19). "
        "Its own LICENSE.md is reproduced verbatim; neovibe's MIT licence does not and cannot "
        "cover it."),
}

# Allowlisted for its licence, NOT for its text: the header promises this one's own licence file
# is reproduced, so a release of it without one must fail rather than print an empty entry.
ALLOWLIST_TEXT_REQUIRED = {("npm", "@anthropic-ai/claude-agent-sdk")}

# In the npm tree, and deliberately NOT in the shipped artifact. Verified on every run: the
# artifact must not contain the package name at all (the SDK's resolver would name it if the
# lookup code were bundled; measured 0 hits on 2026-09-19, with the artifact at ~106 MB against
# this package's 205 MB).
NOT_SHIPPED = {
    ("npm", "@anthropic-ai/claude-agent-sdk-linux-x64"): (
        "the 205 MB native Claude Code CLI. The packaged sidecar serves host_cli only: the user "
        "runs the `claude` they installed. It is not redistributed."),
}

# Texts the package does not ship itself, keyed by EXACT version so a bump fails until re-checked.
# Provenance for each file: packaging/license-texts/README.md.
TEXT_OVERRIDES = {
    ("npm", "standardwebhooks", "1.1.1"): ["standard-webhooks.LICENSE"],
    ("cargo", "gl", "0.14.0"): ["gl-rs.LICENSE"],
    ("cargo", "webkit6", "0.6.1"): ["webkit6-rs.LICENSE"],
    ("cargo", "webkit6-sys", "0.6.0"): ["webkit6-rs.LICENSE"],
    ("cargo", "javascriptcore6", "0.6.0"): ["webkit6-rs.LICENSE"],
    ("cargo", "javascriptcore6-sys", "0.6.0"): ["webkit6-rs.LICENSE"],
    ("cargo", "skia-bindings", "0.153.3"): ["rust-skia.LICENSE"],
    ("cargo", "skia-safe", "0.153.3"): ["rust-skia.LICENSE"],
}
# Same repository, same release train, and the sibling crate DOES ship the file: read it live.
SIBLING_TEXT = {
    ("cargo", "mlua-sys", "0.12.0"): ("mlua", "0.12.1"),
    ("cargo", "tonic-prost", "0.14.6"): ("tonic", "0.14.6"),
}
# Texts a licence requires beyond what the package ships.
EXTRA_TEXTS = {
    ("cargo", "nvim-rs", "0.9.2"): ["GPL-3.0.txt"],  # LGPL-3.0 s4(b): a copy of the GNU GPL too
}

LICENSE_FILE = re.compile(r"^(licen[cs]e|copying|notice|unlicense|copyright)([-._].*)?$", re.I)


class Fail(Exception):
    pass


def tokenize(expr):
    expr = re.sub(r"\s*/\s*", " OR ", expr.strip())
    return re.findall(r"\(|\)|[A-Za-z0-9.+-]+", expr)


def acceptable(expr):
    """True if the SPDX expression lets us use permissive terms. Raises Fail on an unknown id."""
    toks = tokenize(expr)
    pos = [0]

    def peek():
        return toks[pos[0]] if pos[0] < len(toks) else None

    def take():
        pos[0] += 1
        return toks[pos[0] - 1]

    def factor():
        t = take()
        if t == "(":
            v = alt()
            if take() != ")":
                raise Fail(f"unbalanced licence expression {expr!r}")
            return v
        if t in PERMISSIVE:
            v = True
        elif t in COPYLEFT:
            v = False
        else:
            raise Fail(f"unclassifiable licence id {t!r} in {expr!r}")
        if peek() == "WITH":
            take()
            exc = take()
            if exc not in EXCEPTIONS:
                raise Fail(f"unknown licence exception {exc!r} in {expr!r}")
        return v

    def conj():
        v = factor()
        while peek() == "AND":
            take()
            v = factor() and v
        return v

    def alt():
        v = conj()
        while peek() == "OR":
            take()
            v = conj() or v
        return v

    if not toks:
        raise Fail("empty licence expression")
    v = alt()
    if pos[0] != len(toks):
        raise Fail(f"trailing tokens in licence expression {expr!r}")
    return v


def read_text(path):
    with open(path, "rb") as f:
        raw = f.read()
    text = raw.decode("utf-8", errors="replace").replace("\r\n", "\n").replace("\r", "\n")
    return text.rstrip() + "\n"


def license_files(d):
    return sorted(f for f in os.listdir(d) if LICENSE_FILE.match(f) and os.path.isfile(os.path.join(d, f)))


def run(cmd, cwd):
    p = subprocess.run(cmd, cwd=cwd, capture_output=True, text=True)
    if p.returncode != 0:
        raise Fail(f"`{' '.join(cmd)}` in {cwd} failed ({p.returncode}):\n{p.stderr.strip()}")
    return p.stdout


# ---------------------------------------------------------------------------------------------
# Rust: the crates the shipped binaries link
# ---------------------------------------------------------------------------------------------

def cargo_packages():
    """Normal (not dev, not build) dependencies of the shipped packages, not descending into
    proc-macros: a proc-macro and its own dependencies run in the compiler and link into nothing.
    Build dependencies are excluded for the same reason -- EXCEPT the C they compile into the
    binary (vendored Lua; Skia's prebuilt archive), which native_components() covers by looking
    at the binaries themselves."""
    meta = json.loads(run(["cargo", "metadata", "--format-version", "1", "--locked",
                           "--filter-platform", TARGET], REPO))
    workspace = set(meta["workspace_members"])
    by_nv = {}
    for p in meta["packages"]:
        by_nv.setdefault((p["name"], p["version"]), []).append(p)
    cmd = ["cargo", "tree", "--locked", "-e", "normal,no-proc-macro", "--target", TARGET,
           "--prefix", "none", "--format", "{p}"]
    for pkg in SHIPPED_CARGO_PACKAGES:
        cmd += ["-p", pkg]
    seen = set()
    for line in run(cmd, REPO).splitlines():
        m = re.match(r"^(\S+) v(\S+)", line.strip())
        if m:
            seen.add((m.group(1), m.group(2)))
    out = []
    for nv in sorted(seen):
        cands = by_nv.get(nv, [])
        if len(cands) != 1:
            raise Fail(f"cargo tree names {nv[0]} {nv[1]}, which matches {len(cands)} packages in cargo metadata")
        p = cands[0]
        if p["id"] in workspace:
            continue  # neovibe's own code: covered by LICENSE, not by this file
        out.append({
            "eco": "cargo", "name": p["name"], "version": p["version"], "license": p["license"],
            "dir": os.path.dirname(p["manifest_path"]), "source": p["source"] or "",
            "license_file": p.get("license_file"),
        })
    return out, by_nv


def cargo_texts(pkg, by_nv):
    d = pkg["dir"]
    files = [os.path.join(d, f) for f in license_files(d)]
    if pkg["license_file"]:
        lf = os.path.normpath(os.path.join(d, pkg["license_file"]))
        if lf not in files and os.path.isfile(lf):
            files.append(lf)
    if not files and pkg["source"].startswith("git+"):
        # A crate in a subdirectory of a git dependency: the licence is usually at the checkout root.
        cur = d
        while not files and os.path.basename(os.path.dirname(os.path.dirname(cur))) != "checkouts" and cur != "/":
            cur = os.path.dirname(cur)
            files = [os.path.join(cur, f) for f in license_files(cur)]
    return files


# ---------------------------------------------------------------------------------------------
# npm
# ---------------------------------------------------------------------------------------------

def npm_packages(root, workspace=None):
    cmd = ["npm", "ls", "--omit=dev", "--all", "--parseable"]
    if workspace:
        cmd += ["-w", workspace]
    dirs = sorted(set(l for l in run(cmd, root).splitlines() if l.strip()))
    out = {}
    for d in dirs:
        if os.path.realpath(d) == os.path.realpath(root):
            continue  # the project root itself, not a dependency
        with open(os.path.join(d, "package.json")) as f:
            pj = json.load(f)
        lic = pj.get("license")
        if isinstance(lic, dict):
            lic = lic.get("type")
        if lic is None and isinstance(pj.get("licenses"), list):
            lic = " OR ".join(x.get("type", "?") for x in pj["licenses"])
        key = (pj["name"], pj["version"])
        # The same name@version installed at several paths (npm nests copies): one entry.
        if key not in out:
            out[key] = {"eco": "npm", "name": pj["name"], "version": pj["version"], "license": lic,
                        "dir": d, "source": "", "license_file": None}
    return [out[k] for k in sorted(out)]


# ---------------------------------------------------------------------------------------------
# Resolving one package to its texts
# ---------------------------------------------------------------------------------------------

def resolve(pkg, by_nv, counts):
    eco, name, ver = pkg["eco"], pkg["name"], pkg["version"]
    key, keyv = (eco, name), (eco, name, ver)
    found = cargo_texts(pkg, by_nv) if eco == "cargo" else [os.path.join(pkg["dir"], f) for f in license_files(pkg["dir"])]
    note = None

    if keyv in TEXT_OVERRIDES or keyv in SIBLING_TEXT:
        if found:
            raise Fail(f"{eco} {name} {ver} now ships its own licence file ({', '.join(os.path.basename(f) for f in found)}); remove its override")
        if keyv in TEXT_OVERRIDES:
            found = [os.path.join(TEXTS, f) for f in TEXT_OVERRIDES[keyv]]
            note = "the package ships no licence file; text vendored in packaging/license-texts/"
        else:
            sname, sver = SIBLING_TEXT[keyv]
            sib = by_nv.get((sname, sver))
            if not sib:
                raise Fail(f"{name} {ver}'s licence is read from sibling {sname} {sver}, which is not in cargo metadata")
            sdir = os.path.dirname(sib[0]["manifest_path"])
            found = [os.path.join(sdir, f) for f in license_files(sdir)]
            note = f"the crate ships no licence file; text read from its sibling crate {sname} {sver} (same repository and release)"
    for (e, n, v), extra in EXTRA_TEXTS.items():
        if (e, n) == key and v != ver:
            raise Fail(f"EXTRA_TEXTS names {n} {v} but the tree has {ver}; re-check it")
    found = found + [os.path.join(TEXTS, f) for f in EXTRA_TEXTS.get(keyv, [])]
    for (e, n, v) in list(TEXT_OVERRIDES) + list(SIBLING_TEXT):
        if (e, n) == key and v != ver:
            raise Fail(f"an override names {n} {v} but the tree has {ver}; re-check the text and update it")

    lic = pkg["license"]
    if key in ALLOWLIST:
        status = "allowlisted: " + ALLOWLIST[key]
        label = lic or "(no licence declared)"
        if key in ALLOWLIST_TEXT_REQUIRED and not found:
            raise Fail(f"{eco} {name} {ver} is allowlisted on the grounds that its own licence file is "
                       "reproduced, and it now ships none")
    else:
        if not lic:
            raise Fail(f"{eco} {name} {ver} declares no licence and is not allowlisted")
        if not acceptable(lic):
            if key not in COPYLEFT_ACK:
                raise Fail(f"{eco} {name} {ver} is {lic!r}, which is copyleft and not acknowledged in COPYLEFT_ACK")
            status = "copyleft, acknowledged: " + COPYLEFT_ACK[key]
        else:
            status = None
        label = lic
        if not found:
            raise Fail(f"{eco} {name} {ver} ({lic}) ships no licence text and has no vendored one")
    for f in found:
        if not os.path.isfile(f):
            raise Fail(f"licence text {f} for {name} {ver} does not exist")
    counts[label] = counts.get(label, 0) + 1
    return {"title": f"{name} {ver}", "name": name, "version": ver, "license": label, "status": status, "note": note,
            "copyleft": key in COPYLEFT_ACK,
            "files": [(os.path.basename(f), read_text(f)) for f in found]}


# ---------------------------------------------------------------------------------------------
# Native code and assets compiled into the binaries
# ---------------------------------------------------------------------------------------------

def defined_symbols(path):
    out = run(["nm", "--defined-only", path], REPO)
    syms = set(l.split()[-1] for l in out.splitlines() if len(l.split()) >= 3)
    # `nm` on a stripped binary prints "no symbols" and exits 0. Every detector below would then see
    # an empty set and report nothing -- a silent pass that drops Lua, Skia and the rest from the
    # notices. Every Rust binary defines `main`, so its absence means there is no symbol table.
    if "main" not in syms:
        raise Fail(f"{path} has no symbol table (stripped?): the native-code detectors cannot see "
                   "inside it. Build without stripping, or teach this script another way in.")
    return syms


# Symbols the linker itself synthesizes: they sort after the last input file's FILE entry in the
# symbol table, so they must not be attributed to that file.
LINKER_LOCALS = {"__FRAME_END__", "__TMC_END__", "_GLOBAL_OFFSET_TABLE_", "_DYNAMIC", "_init",
                 "_fini", "__dso_handle", "__GNU_EH_FRAME_HDR"}


def c_files_with_code(path):
    """Names of the C source files (STT_FILE entries ending in .c) that still own at least one
    local symbol in a real section after --gc-sections.

    A FILE entry alone is not evidence: the prebuilt Skia archive leaves FILE entries for whole
    libraries whose code the linker then discarded (on 2026-09-19 `shell` carried FILE entries for
    HarfBuzz, ICU, libjpeg-turbo, expat and Wuffs, and not one symbol or runtime string of any of
    them). A C file that kept a local symbol did link code in."""
    out = run(["readelf", "-sW", path], REPO)
    cur, found = None, set()
    for l in out.splitlines():
        p = l.split()
        if len(p) < 8:
            continue
        if p[3] == "FILE":
            cur = p[7]
            continue
        if cur and p[4] == "LOCAL" and p[6] not in ("ABS", "UND") and p[7] not in LINKER_LOCALS:
            if cur.endswith(".c"):
                found.add(cur)
    return found


def font_copyright(path):
    """nameID 0 (copyright) from a TrueType/OpenType 'name' table, preferring Windows English."""
    with open(path, "rb") as f:
        data = f.read()
    num = struct.unpack(">H", data[4:6])[0]
    for i in range(num):
        tag, _, off, _ = struct.unpack(">4sIII", data[12 + 16 * i: 28 + 16 * i])
        if tag == b"name":
            _, count, soff = struct.unpack(">HHH", data[off:off + 6])
            best = None
            for j in range(count):
                pid, eid, lid, nid, ln, so = struct.unpack(">HHHHHH", data[off + 6 + 12 * j: off + 18 + 12 * j])
                if nid != 0:
                    continue
                raw = data[off + soff + so: off + soff + so + ln]
                s = raw.decode("utf-16-be") if pid in (0, 3) else raw.decode("latin-1")
                rank = (pid == 3 and lid == 0x409, pid == 3)
                if best is None or rank > best[0]:
                    best = (rank, s)
            if best:
                return best[1].strip()
    raise Fail(f"no copyright name record in {path}")


def native_components(by_nv, shipped):
    """C/C++ code and data assets inside the binaries, found by looking at the binaries."""
    syms, blobs = {}, {}
    for b in SHIPPED_BINARIES:
        syms[b] = defined_symbols(shipped[b])
        with open(shipped[b], "rb") as f:
            blobs[b] = f.read()

    def pkg_dir(name):
        c = [p for (n, _), ps in by_nv.items() if n == name for p in ps]
        if len(c) != 1:
            raise Fail(f"expected exactly one {name} in cargo metadata, found {len(c)}")
        return os.path.dirname(c[0]["manifest_path"])

    def any_sym(names):
        return sorted(b for b in SHIPPED_BINARIES if syms[b] & set(names))

    comps = []

    # Every detector below is a symbol (or string) the library defines. A component detected with
    # no text source fails; one with a text source and no detection is left out, so this section
    # follows the binaries rather than a hand-kept list.
    detectors = [
        ("HarfBuzz", {"hb_blob_create"}), ("expat", {"XML_ParserCreate"}),
        ("libjpeg", {"jpeg_CreateDecompress", "jpeg_std_error"}), ("libwebp", {"WebPDecode"}),
    ]
    for comp, names in detectors:
        hit = any_sym(names)
        if hit:
            raise Fail(f"{comp} is statically linked into {', '.join(hit)} and this script has no licence text for it")
    if any(re.search(rb"^u_\w+_\d+$", s.encode()) for b in SHIPPED_BINARIES for s in syms[b] if s.startswith("u_")):
        raise Fail("ICU appears statically linked into a shipped binary and this script has no licence text for it")

    # The named detectors above only catch libraries someone thought of. This closes the set for C:
    # every C source file that still owns code in a shipped binary must belong to a component this
    # script has a text for, or the run fails naming the file. (C++ is NOT closed this way: Skia's
    # own sources have no common naming scheme to tell them from a third party's.)
    lua_files = set()
    for d in glob.glob(os.path.join(pkg_dir("lua-src"), "lua-*")):
        lua_files |= {f for f in os.listdir(d) if f.endswith(".c")}
    freetype_modules = {"autofit.c", "bdf.c", "cff.c", "gxvalid.c", "otvalid.c", "pcf.c", "pfr.c",
                        "psaux.c", "pshinter.c", "psnames.c", "raster.c", "sdf.c", "sfnt.c", "smooth.c",
                        "svg.c", "truetype.c", "type1.c", "type1cid.c", "type42.c", "winfnt.c"}
    zlib_files = {"adler32.c", "compress.c", "crc32.c", "deflate.c", "gzclose.c", "gzlib.c", "gzread.c",
                  "gzwrite.c", "infback.c", "inffast.c", "inflate.c", "inftrees.c", "trees.c",
                  "uncompr.c", "zutil.c"}
    chromium_zlib_files = {"adler32_simd.c", "crc32_simd.c", "cpu_features.c", "inffast_chunk.c",
                           "crc_folding.c"}
    libpng_extra = {"intel_init.c", "filter_sse2_intrinsics.c"}

    def c_component(f):
        if f in lua_files:
            return "Lua"
        if f in freetype_modules or re.match(r"^ft\w*\.c$", f):
            return "FreeType"
        if f in libpng_extra or re.match(r"^png\w*\.c$", f):
            return "libpng"
        if f in zlib_files:
            return "zlib"
        if f in chromium_zlib_files:
            return "Chromium zlib"
        return None

    c_seen = {}
    for b in SHIPPED_BINARIES:
        for f in c_files_with_code(shipped[b]):
            comp = c_component(f)
            if comp is None:
                raise Fail(f"{b} contains code compiled from {f}, which belongs to no component this "
                           "script has a licence text for. Find which library it is and add it.")
            c_seen.setdefault(comp, set()).add(b)

    # Lua, compiled from lua-src's vendored C by mlua-sys's build script.
    hit = any_sym({"lua_newstate"})
    if hit:
        # The binary carries only "Lua 5.4" (LUA_VERSION); lua-src vendors one release per minor.
        minors = sorted(set(m.group(1).decode() for b in hit for m in re.finditer(rb"\x00Lua (\d+\.\d+)\x00", blobs[b])))
        if len(minors) != 1:
            raise Fail(f"Lua is linked into {hit} but its version string was found {len(minors)} times")
        dirs = sorted(glob.glob(os.path.join(pkg_dir("lua-src"), f"lua-{minors[0]}.*")))
        if len(dirs) != 1:
            raise Fail(f"expected one lua-{minors[0]}.* in lua-src, found {len(dirs)}")
        vers = [os.path.basename(dirs[0])[len("lua-"):]]
        header = os.path.join(dirs[0], "lua.h")
        text = read_text(header)
        m = re.search(r"/\*{10,}\n(\* Copyright.*?)\*{10,}/", text, re.S)
        if not m:
            raise Fail(f"no licence block at the end of {header}")
        notice = "\n".join(re.sub(r"^\* ?", "", l) for l in m.group(1).splitlines()) + "\n"
        comps.append({"title": f"Lua {vers[0]}", "license": "MIT", "status": None,
                      "note": f"compiled from the lua-src crate's vendored C into {', '.join(hit)}; notice from lua-{vers[0]}/lua.h",
                      "files": [("lua.h (notice)", notice)]})

    # Skia: rust-skia's build script downloads a prebuilt static archive; its licence is in the
    # build output next to it, and in no crate.
    skia_hit = sorted(b for b in SHIPPED_BINARIES if any(s.startswith("_ZN8SkCanvas") for s in syms[b]))
    if skia_hit:
        lic = sorted(glob.glob(os.path.join(REPO, "target", "release", "build", "skia-bindings-*", "out", "skia", "LICENSE_SKIA")))
        texts = sorted(set(read_text(f) for f in lic))
        if len(texts) != 1:
            raise Fail(f"expected one Skia licence under target/release/build/skia-bindings-*/out/skia, found {len(texts)} distinct")
        comps.append({"title": "Skia", "license": "BSD-3-Clause", "status": None,
                      "note": f"prebuilt static library (rust-skia's skia-binaries), linked into {', '.join(skia_hit)}",
                      "files": [("LICENSE_SKIA", texts[0])]})

    # Libraries the prebuilt Skia archive carries inside it.
    embedded = [
        ("FreeType", {"FT_Init_FreeType"}, None, "FTL (FreeType is FTL-or-GPLv2; this package takes the FTL)",
         ["freetype-FTL.TXT"], "Portions of this software are copyright (c) The FreeType Project (www.freetype.org). All rights reserved."),
        ("libpng", {"png_create_read_struct"}, None, "libpng-2.0", ["libpng.LICENSE"], None),
        ("zlib (inflate)", {"inflateInit2_", "Cr_z_inflateInit2_"}, None, "Zlib", ["zlib.NOTICE"], None),
        # Chromium's zlib fork (symbols prefixed Cr_z_, version string "1.3.0.1-motley" in `shell`)
        # adds SIMD adler32/crc32 and a chunked inflate_fast, whose files say "Copyright 2017 The
        # Chromium Authors ... governed by a BSD-style license that can be found in the Chromium
        # source repository LICENSE file" (chunkcopy.h adds ARM, Inc.). zlib's own notice does not
        # cover those.
        ("Chromium zlib (SIMD and chunked-inflate additions)",
         {"Cr_z_adler32_simd_", "Cr_z_crc32_sse42_simd_", "Cr_z_inflate_fast_chunk_", "Cr_z_cpu_check_features"},
         None, "BSD-3-Clause", ["chromium.LICENSE"],
         "Copyright 2017 The Chromium Authors. Copyright (C) 2017 ARM, Inc."),
        # Detected by code, NOT by the "wuffs-v0.3.c" string: that string is an STT_FILE entry the
        # prebuilt archive leaves behind even when --gc-sections discards every byte of Wuffs, which
        # is exactly what `shell` looked like on 2026-09-19 (no wuffs_ symbol, no Wuffs status
        # string). The first version of this script matched the FILE entry and listed Wuffs wrongly.
        ("Wuffs", set(), b"#base: ", "Apache-2.0", ["Apache-2.0.txt"], "Copyright 2017 The Wuffs Authors."),
    ]
    for comp, names, marker, lic, files, credit in embedded:
        if comp == "Wuffs":
            hit = sorted(b for b in SHIPPED_BINARIES
                         if any("wuffs_" in s for s in syms[b]) or marker in blobs[b])
        else:
            hit = any_sym(names)
        if not hit:
            continue
        fl = [(f, read_text(os.path.join(TEXTS, f))) for f in files]
        if credit:
            fl.insert(0, ("notice", credit + "\n"))
        comps.append({"title": comp, "license": lic, "status": None,
                      "note": f"inside the prebuilt Skia archive, statically linked into {', '.join(hit)}; text vendored in packaging/license-texts/",
                      "files": fl})

    # Fonts the Neovide fork embeds with include_bytes!: detect by the font bytes themselves.
    nv = pkg_dir("neovide")
    fonts_dir = os.path.join(nv, "assets", "fonts")
    for font in sorted(os.listdir(fonts_dir)):
        if not font.endswith((".ttf", ".otf")):
            continue
        with open(os.path.join(fonts_dir, font), "rb") as f:
            head = f.read(4096)
        hit = sorted(b for b in SHIPPED_BINARIES if head in blobs[b])
        if not hit:
            continue
        comps.append({"title": f"font {font}", "license": "OFL-1.1", "status": None,
                      "note": f"embedded by the Neovide fork into {', '.join(hit)}; copyright from the font's own name table",
                      "files": [("copyright", font_copyright(os.path.join(fonts_dir, font)) + "\n"),
                                ("assets/fonts/LICENSE", read_text(os.path.join(fonts_dir, "LICENSE")))]})

    # Every C file that kept code must have produced an entry above; a classified file whose
    # component then was not emitted would be a gap this function reports as coverage.
    titles = {c["title"] for c in comps}
    need = {"Lua": lambda t: t.startswith("Lua "), "FreeType": lambda t: t == "FreeType",
            "libpng": lambda t: t == "libpng", "zlib": lambda t: t == "zlib (inflate)",
            "Chromium zlib": lambda t: t.startswith("Chromium zlib")}
    for comp, bins in sorted(c_seen.items()):
        if not any(need[comp](t) for t in titles):
            raise Fail(f"{', '.join(sorted(bins))} contains {comp} code (by its C source files) but "
                       f"no {comp} entry was emitted: its symbol detector missed it")

    comps.insert(0, rust_std_component())
    return comps


class _HtmlText(html.parser.HTMLParser):
    BLOCK = {"p", "br", "li", "h1", "h2", "h3", "h4", "pre", "div", "tr", "dt", "dd"}

    def __init__(self):
        super().__init__(convert_charrefs=True)
        self.out, self.skip = [], 0

    def handle_starttag(self, tag, attrs):
        if tag in ("style", "script", "head"):
            self.skip += 1
        if tag in self.BLOCK:
            self.out.append("\n")

    def handle_endtag(self, tag):
        if tag in ("style", "script", "head"):
            self.skip -= 1
        if tag in self.BLOCK:
            self.out.append("\n")

    def handle_data(self, data):
        if not self.skip:
            self.out.append(data)


def rust_std_component():
    """The Rust standard library (std, core, alloc, compiler_builtins and the crates.io crates std
    itself depends on) is statically linked into every Rust binary, and no `cargo tree` lists it.
    The toolchain ships the notice file meant for exactly this, COPYRIGHT-library.html; it is
    reproduced as text. The compiler that built the binaries must be the one whose notice is read."""
    built = set()
    for b in SHIPPED_BINARIES:
        out = run(["readelf", "-p", ".comment", os.path.join(REPO, "target", "release", b)], REPO)
        m = re.search(r"rustc version (\S+ \([0-9a-f]+ [0-9-]+\))", out)
        if not m:
            raise Fail(f"target/release/{b} names no rustc in its .comment section")
        built.add(m.group(1))
    here = run(["rustc", "--version"], REPO).strip()
    if len(built) != 1 or f"rustc {next(iter(built))}" != here:
        raise Fail(f"the shipped binaries were built by rustc {sorted(built)}, but `rustc` here is "
                   f"{here!r}: its notice file would describe a different standard library")
    sysroot = run(["rustc", "--print", "sysroot"], REPO).strip()
    src = os.path.join(sysroot, "share", "doc", "rust", "COPYRIGHT-library.html")
    if not os.path.isfile(src):
        raise Fail(f"no {src}: the toolchain's standard-library notice is missing")
    p = _HtmlText()
    with open(src, encoding="utf-8") as f:
        p.feed(f.read())
    lines = [l.rstrip() for l in "".join(p.out).splitlines()]
    text = re.sub(r"\n{3,}", "\n\n", "\n".join(lines)).strip() + "\n"
    return {"title": f"Rust standard library ({here})", "license": "MIT OR Apache-2.0, and the licences it lists",
            "status": None,
            "note": "statically linked into all five binaries; text is the toolchain's share/doc/rust/COPYRIGHT-library.html, tags removed",
            "files": [("COPYRIGHT-library", text)]}


# ---------------------------------------------------------------------------------------------
# The sidecar
# ---------------------------------------------------------------------------------------------

def sidecar_facts(artifact, verdandi):
    if not os.access(artifact, os.X_OK):
        raise Fail(f"{artifact} is not an executable sidecar artifact")
    ver = run([artifact, "--version"], REPO)
    m_node = re.search(r"node (v\d+\.\d+\.\d+)", ver)
    m_sdk = re.search(r"claude-agent-sdk (\S+)", ver)
    m_self = re.search(r"verdandi-claude-sidecar (\S+)", ver)
    if not (m_node and m_sdk and m_self):
        raise Fail(f"cannot read node/sdk/sidecar versions from `{artifact} --version`:\n{ver}")
    sdk_pj = os.path.join(verdandi, "node_modules", "@anthropic-ai", "claude-agent-sdk", "package.json")
    with open(sdk_pj) as f:
        sdk_ver = json.load(f)["version"]
    if sdk_ver != m_sdk.group(1):
        raise Fail(f"the artifact bundles claude-agent-sdk {m_sdk.group(1)} but the Verdandi checkout has {sdk_ver}: "
                   "its npm tree does not describe this artifact")
    with open(os.path.join(verdandi, "apps", "claude-sidecar", "package.json")) as f:
        sc_ver = json.load(f)["version"]
    if sc_ver != m_self.group(1):
        raise Fail(f"the artifact is sidecar {m_self.group(1)} but the checkout is {sc_ver}")
    node_license = os.path.join(verdandi, "apps", "claude-sidecar", "build", "node-cache",
                                f"node-{m_node.group(1)}-linux-x64", "LICENSE")
    if not os.path.isfile(node_license):
        raise Fail(f"no Node.js LICENSE at {node_license}: the artifact embeds Node {m_node.group(1)} and that is "
                   "the official tarball its build extracts there. Run Verdandi's `npm run build:binary "
                   "-w @verdandi/claude-sidecar` once to populate it.")
    with open(artifact, "rb") as f:
        blob = f.read()
    for (_, name), _reason in NOT_SHIPPED.items():
        if name.encode() in blob:
            raise Fail(f"{name} is declared NOT shipped, but its name occurs in {artifact}")
    return {"node": m_node.group(1), "sdk": sdk_ver, "sidecar": sc_ver, "node_license": node_license,
            "version_text": ver.strip()}


# ---------------------------------------------------------------------------------------------
# Output
# ---------------------------------------------------------------------------------------------

RULE = "=" * 78


def render_entries(entries, printed, lines):
    for e in entries:
        lines.append("-" * 78)
        lines.append(f"{e['title']}    [{e['license']}]")
        if e["status"]:
            lines.append(f"  {e['status']}")
        if e["note"]:
            lines.append(f"  ({e['note']})")
        for fname, text in e["files"]:
            h = hashlib.sha256(text.encode()).hexdigest()
            if h in printed:
                lines.append(f"  {fname}: text identical to {printed[h]}, printed above")
            else:
                printed[h] = f"{e['title']} ({fname})"
                lines.append(f"  --- {fname} ---")
                lines.append("")
                lines.append(text.rstrip("\n"))
                lines.append("")


def vite_runtime(shipped, counts):
    """Code the bundler itself writes into the web bundle, which `npm ls --omit=dev` cannot list
    because Vite is a devDependency: its module-preload polyfill (measured present in `shell` on
    2026-09-19). Vite's own MIT notice, without the licences of Vite's bundled build-time
    dependencies, none of which reach the output."""
    with open(shipped["shell"], "rb") as f:
        if b'supports("modulepreload")' not in f.read():
            return []
    vite = os.path.join(REPO, "agent-ui", "web", "node_modules", "vite")
    with open(os.path.join(vite, "package.json")) as f:
        ver = json.load(f)["version"]
    text = read_text(os.path.join(vite, "LICENSE.md"))
    cut = text.find("# Licenses of bundled dependencies")
    if cut <= 0 or "MIT License" not in text[:cut]:
        raise Fail(f"{vite}/LICENSE.md no longer starts with Vite's own MIT notice; re-check it")
    counts["MIT"] = counts.get("MIT", 0) + 1
    return [{"title": f"vite {ver} (runtime code it injects: the module-preload polyfill)", "name": "vite",
             "version": ver, "license": "MIT", "status": None, "copyleft": False,
             "note": "a devDependency, so not in the npm tree above; its polyfill is in the built bundle",
             "files": [("LICENSE.md (Vite core licence section)", text[:cut])]}]


def collect(sidecar_artifact, verdandi):
    shipped = {b: os.path.join(REPO, "target", "release", b) for b in SHIPPED_BINARIES}
    for b, p in shipped.items():
        if not os.path.isfile(p):
            raise Fail(f"{p} does not exist: build the release binaries first (publish.sh does)")

    counts_rust, counts_web, counts_sc = {}, {}, {}
    crates, by_nv = cargo_packages()
    rust = [resolve(p, by_nv, counts_rust) for p in crates]
    native = native_components(by_nv, shipped)
    web = [resolve(p, by_nv, counts_web) for p in npm_packages(os.path.join(REPO, "agent-ui", "web"))]
    web += vite_runtime(shipped, counts_web)
    sc = sidecar_facts(sidecar_artifact, verdandi)
    sc_pkgs = []
    excluded = []
    for p in npm_packages(verdandi, "@verdandi/claude-sidecar"):
        if (p["eco"], p["name"]) in NOT_SHIPPED:
            excluded.append(p)
            continue
        sc_pkgs.append(resolve(p, by_nv, counts_sc))

    L = []
    L += [RULE, "neovibe -- third-party licences", RULE, ""]
    L += [
        "neovibe's own code is MIT-licensed: see LICENSE, installed beside this file.",
        "",
        "This package also contains other people's code, each part under its own licence,",
        "reproduced below. MIT covers neovibe's code only; it does not relicense any of this.",
        "",
        "ONE PART IS NOT OPEN SOURCE. /usr/lib/neovibe/verdandi-claude-sidecar bundles",
        f"@anthropic-ai/claude-agent-sdk {sc['sdk']}, which is (c) Anthropic PBC, all rights",
        "reserved, and whose use is subject to Anthropic's legal agreements. Its licence file is",
        "reproduced verbatim in part 4. The Claude Code CLI itself is NOT included: the sidecar",
        "runs the `claude` you install yourself.",
        "",
        "Copyleft parts, each acknowledged individually in part 1, and where their",
        "unmodified source is published:",
    ]
    copyleft = [e for e in rust if e["copyleft"]]
    if sorted((e["name"] for e in copyleft)) != sorted(n for (eco, n) in COPYLEFT_ACK if eco == "cargo"):
        raise Fail(f"COPYLEFT_ACK names {sorted(n for _, n in COPYLEFT_ACK)} but the tree ships "
                   f"{sorted(e['name'] for e in copyleft)}: drop the stale acknowledgement")
    for e in copyleft:
        L.append(f"  {e['title']} [{e['license']}]  https://crates.io/crates/{e['name']}/{e['version']}")
    L += [
        "neovibe's own source is at https://github.com/HunterGrey-cyber/eitri.",
        "",
        "Libraries this package links dynamically from your system (GTK 4, WebKitGTK, GLib and",
        "their dependencies) are not redistributed here and are not listed.",
        "",
        "Generated by packaging/collect-licenses.py from the dependency trees of exactly what the",
        "package installs. Contents:",
        f"  part 1  Rust crates linked into the five binaries in /usr/lib/neovibe   ({len(rust)})",
        f"  part 2  the Rust standard library, native code and fonts in those binaries ({len(native)})",
        f"  part 3  npm packages in the agent panel's web bundle, inside `shell`      ({len(web)})",
        f"  part 4  the sidecar: Node.js {sc['node']} and its npm packages              ({len(sc_pkgs)})",
        "",
    ]
    for title, counts in (("part 1", counts_rust), ("part 3", counts_web), ("part 4", counts_sc)):
        L.append(f"Licence expressions, {title}:")
        for k in sorted(counts, key=lambda k: (-counts[k], k)):
            L.append(f"  {counts[k]:4d}  {k}")
        L.append("")

    printed = {}
    L += [RULE, "PART 1 -- Rust crates linked into the shipped binaries", RULE, ""]
    render_entries(rust, printed, L)
    L += ["", RULE, "PART 2 -- the Rust standard library, native code and fonts in the shipped binaries", RULE, ""]
    render_entries(native, printed, L)
    L += ["", RULE, "PART 3 -- the agent panel's web bundle (embedded in shell)", RULE, ""]
    render_entries(web, printed, L)
    L += ["", RULE, "PART 4 -- /usr/lib/neovibe/verdandi-claude-sidecar", RULE, ""]
    L += [f"A Node.js single-executable application. Its own --version reports:", ""]
    L += ["  " + l for l in sc["version_text"].splitlines()]
    L += [""]
    for p in excluded:
        L += [f"NOT included, though its npm tree names it: {p['name']} {p['version']} -- "
              + NOT_SHIPPED[(p['eco'], p['name'])], ""]
    render_entries([{"title": f"Node.js {sc['node']}", "license": "MIT, and the licences of its embedded components below",
                     "status": None, "note": "the official nodejs.org linux-x64 build the artifact was made from",
                     "files": [("LICENSE", read_text(sc["node_license"]))]}], printed, L)
    render_entries(sc_pkgs, printed, L)
    return "\n".join(L).rstrip("\n") + "\n", {"rust": counts_rust, "native": [c["title"] for c in native],
                                               "web": counts_web, "sidecar": counts_sc,
                                               "excluded": [p["name"] for p in excluded]}


def main(argv):
    out = os.path.join(REPO, "dist", "THIRD-PARTY-LICENSES")
    artifact = os.path.join(REPO, "dist", "verdandi-claude-sidecar")
    args = list(argv)
    while args:
        a = args.pop(0)
        if a == "--out":
            out = args.pop(0)
        elif a == "--sidecar":
            artifact = args.pop(0)
        else:
            print(f"collect-licenses: unknown argument {a}", file=sys.stderr)
            return 2
    verdandi = os.environ.get("NEOVIBE_VERDANDI_CHECKOUT") or os.path.expanduser("~/src/verdandi")
    try:
        text, summary = collect(artifact, verdandi)
    except (Fail, OSError, json.JSONDecodeError) as e:
        print(f"collect-licenses: FAILED: {e}", file=sys.stderr)
        return 1
    os.makedirs(os.path.dirname(out), exist_ok=True)
    tmp = out + ".tmp"
    with open(tmp, "w", encoding="utf-8", newline="\n") as f:
        f.write(text)
    os.replace(tmp, out)
    total = lambda c: sum(c.values())
    print(f"collect-licenses: wrote {out} ({text.count(chr(10))} lines)", file=sys.stderr)
    print(f"  rust crates {total(summary['rust'])}, native {len(summary['native'])} ({', '.join(summary['native'])}), "
          f"web npm {total(summary['web'])}, sidecar npm {total(summary['sidecar'])}; "
          f"excluded as not shipped: {', '.join(summary['excluded']) or 'none'}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
