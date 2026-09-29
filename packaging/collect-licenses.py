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
  * a sidecar artifact that does not match the Verdandi checkout it is reading the npm tree from;
  * an object member inside the Skia prebuilt archive (--no-sidecar's --skia-archive-path) that
    belongs to no component it has a licence text for.

Deterministic by construction: every list is sorted, nothing reads the clock, and identical texts
are printed once and referred to afterwards. Running it twice gives byte-identical output.

Inputs it expects to exist (publish.sh / release.sh produce all of them before calling it):
  <repo>/target/release/{shell,neovibe-supervisor,neovibe-tmux-shim,neovibe-claude-handoff}
    (or --binaries-dir DIR; no agent-hook -- it is not in any release, spec sec 10, D16)
  dist/verdandi-claude-sidecar                    (sidecar profile only; not read with --no-sidecar)
  agent-ui/web/node_modules                       (shell/build.rs runs npm ci there)
  $NEOVIBE_VERDANDI_CHECKOUT (default ~/src/verdandi), with node_modules installed and
  apps/claude-sidecar/build/node-cache/ holding the Node the artifact was built from (sidecar profile
  only)

Two profiles (spec sec 11, sec 8):
  --sidecar ARTIFACT [--repo DIR] [--binaries-dir DIR] [--source-url URL]
      the private/dev profile: parts 1-4, sidecar included. --source-url defaults to the bare repo
      URL (DEFAULT_SOURCE_URL) when omitted, since a dev run has no versioned release asset.
  --no-sidecar --source-url URL --skia-archive-path PATH [--repo DIR] [--binaries-dir DIR]
      the public profile (release.sh, spec sec 4.2 step 6): parts 1-3 only, no sidecar, plus the
      archive-contents section below. --source-url is required and is the exact "clear directions"
      URL GPL-3.0 sec 6(d) requires (the release's source asset download link); it is never derived
      or guessed here. --skia-archive-path is required too (verdict #7): the public source asset
      redistributes the WHOLE Skia prebuilt archive (spec D9), and every component it contains --
      not just what the shipped binaries end up linking -- needs a notice.
  --source-notice FILE --skia-archive NAME --skia-sha256 HEX --source-url URL
      also write share/licenses/neovibe/SOURCE (spec sec 11.3) to FILE, in either profile. version
      comes from cargo metadata's `shell` package; commit from $NEOVIBE_BUILD_COMMIT or
      `git rev-parse HEAD` in --repo; the Neovide fork's commit from $NEOVIBE_BUILD_FORK_COMMIT or
      `git rev-parse HEAD` in --repo's own neovide/ checkout. --source-url is required here too,
      even under --sidecar (whose own default is the bare repo, which SOURCE must never name as the
      asset).

--skia-license-dir DIR overrides where the Skia licence lookup looks (default: <binaries-dir>/build,
a sibling of the shipped binaries); needed when --binaries-dir has no build/ subtree of its own, e.g.
a public clone plus an extracted release tarball (Task 13, spec sec 8 step 6).

--skia-archive-path PATH (--no-sidecar only, verdict #7): the prebuilt Skia archive this build
linked (the same one --skia-archive names, before rewriting -- either its .tar.gz or an already-
extracted directory). THIRD-PARTY-LICENSES gains a section covering every component the archive's
own .a files contain: expat, libjpeg-turbo, Wuffs, HarfBuzz and ICU (which native_components() above
never sees, because --gc-sections drops them from the shipped binaries), plus Skia itself, FreeType,
libpng and zlib (which it usually does see, so the section's copy is typically a "printed above"
pointer, not a second full text). A member of a .a file this script cannot classify fails the run
naming it, the same fail-closed shape as native_components()'s c_component(). When --skia-sha256 is
also given, PATH's own bytes are verified against it before anything is extracted or trusted.

--skia-archive-extract-dir DIR: required whenever --skia-archive-path names a .tar.gz/.tgz (not an
already-extracted directory) -- a writable scratch location. The archive is extracted into
DIR/skia-archive, which this script replaces on every run; nothing else in DIR is touched. Never
derived from --skia-archive-path itself: release.sh's own copy of that path lives inside
/build/skia, which is read-only during the offline build phase, and on a dev machine
--skia-archive-path can point straight at another checkout's shared cargo build cache, which must
never be written into.
"""

import glob
import hashlib
import html.parser
import json
import os
import re
import shutil
import struct
import subprocess
import sys
import tarfile

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
TEXTS = os.path.join(REPO, "packaging", "license-texts")
TARGET = "x86_64-unknown-linux-gnu"

# The four binaries every release (public or private) installs from target/release, and the cargo
# packages whose bin targets they are (publish.sh / release.sh build exactly
# `-p shell -p agent -p supervisor --bins`). No `agent-hook`: it is the legacy backend's permission
# hook, and after Task 4 (D16) no release build compiles the legacy backend at all, in either
# profile (spec sec 10).
SHIPPED_BINARIES = ["shell", "neovibe-supervisor", "neovibe-tmux-shim", "neovibe-claude-handoff"]
SHIPPED_CARGO_PACKAGES = ["shell", "agent", "supervisor"]

# Small counts, spelled as words in prose (never digits) -- licences-claude-2: a note once read
# "all five binaries" as a literal, stale since D16 dropped `agent-hook` and left four. Anything
# past this table falls back to the digit rather than failing: nothing here needs a word past six.
_COUNT_WORDS = {1: "one", 2: "two", 3: "three", 4: "four", 5: "five", 6: "six"}


def _count_word(n):
    return _COUNT_WORDS.get(n, str(n))


# The URL used when neither profile names one explicitly (only the --sidecar/dev profile may omit
# --source-url; --no-sidecar requires it). A dev run has no versioned release asset to point at, so
# this is just the repository, which is at least still true.
DEFAULT_SOURCE_URL = "https://github.com/HunterGrey-cyber/neovibe"

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
    # "{binaries}" is filled in from a real symbol scan (nvim_rs_symbol_check), not assumed: a plain
    # `_ZN7nvim_rs` mangled-prefix match finds only 49 of the 245 demangled hits in today's `shell`,
    # missing trait impls and monomorphised code.
    ("cargo", "nvim-rs"): (
        "LGPL-3.0 only (its README: a fork of neovim-lib; new commits are also MIT/Apache, but the "
        "crate as published is LGPL-3.0). Pulled in by the Neovide fork and STATICALLY linked into "
        "{binaries}, checked by symbol against the shipped binaries rather than assumed. LGPL-3.0 "
        "section 4(d) then requires the recipient be able to relink against a modified nvim-rs; "
        "this project meets that under section 4(d)(0) with the release's source asset and the "
        "accompanying SOURCE notice installed beside this file."),
    ("cargo", "option-ext"): (
        "MPL-2.0, file-level copyleft. Used unmodified (via `dirs-sys`); its unmodified source is "
        "published on crates.io, and also travels inside this release's own source asset, under "
        "vendor/option-ext. MPL-2.0 section 3.2 is met by stating where that source is."),
}

# Packages with no licence field or no licence text, shipped anyway, each for a stated reason.
# Exactly these. Anything else without a classifiable licence and a findable text fails.
ALLOWLIST = {
    ("cargo", "claude-runtime-protocol"): (
        "Verdandi's own generated gRPC types, same owner as neovibe. Its own Cargo.toml declares no "
        "`license` field. cargo_texts()'s git-checkout walk falls back to the checkout's own root "
        "LICENSE when the crate's own directory ships none -- which is present (MIT) on the public "
        "Verdandi mirror and absent on at least one private checkout, so whether a licence text is "
        "found at all depends on which checkout this ran against. Keep this entry regardless of "
        "that: the check above is `if not lic`, reading the crate's own declared licence, which is "
        "empty either way -- dropping the entry fails the run on \"declares no licence and is not "
        "allowlisted\" even where a root LICENSE happens to be found. Re-check only once "
        "claude-runtime-protocol's own Cargo.toml declares `license = \"MIT\"`."),
    ("npm", "@verdandi/claude-sidecar"): (
        "Verdandi's own sidecar, same owner as neovibe; no licence stated yet (to be MIT)."),
    ("npm", "@verdandi/claude-runtime"): (
        "Verdandi's own runtime package, same owner as neovibe; no licence stated yet (to be MIT)."),
    ("npm", "@anthropic-ai/claude-agent-sdk"): (
        "NOT open source. Bundled into the sidecar knowingly (the owner's decision B, 2026-09-19). "
        "Its own LICENSE.md is reproduced verbatim; neovibe's MIT licence does not and cannot "
        "cover it."),
}

# Workspace members that are NOT neovibe's own MIT code. Every other workspace member is covered by
# LICENSE and left out of this file; these were moved in with a licence of their own, which travels
# with the binary exactly as a crates.io dependency's does (bottom-terminal spec, decision 4.1).
WORKSPACE_THIRD_PARTY = {
    "terminal-input": (
        "derived from Alacritty (commit 94e7c8874e526b1e67b349d9ba30ddf81669119e) and Apache-2.0, "
        "which cannot be relicensed. Moved into neovibe from Verdandi with its LICENSE-APACHE and its "
        "NOTICE, both reproduced as Apache-2.0 section 4 requires."),
}


def own_code(name, license):
    """Whether workspace member `name` is neovibe's own code, covered by LICENSE and not listed here.
    A member declaring anything but MIT (or nothing) must be in WORKSPACE_THIRD_PARTY -- a moved-in
    crate that kept its own licence must never be skipped as if it were neovibe's."""
    if name in WORKSPACE_THIRD_PARTY:
        return False
    if license in (None, "MIT"):
        return True
    raise Fail(f"workspace member {name} declares {license!r}: not neovibe's MIT, and not in "
               "WORKSPACE_THIRD_PARTY")


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

def load_metadata(repo):
    return json.loads(run(["cargo", "metadata", "--format-version", "1", "--locked",
                           "--filter-platform", TARGET], repo))


def workspace_package_version(meta, name):
    """The version cargo metadata reports for workspace member `name` (e.g. "shell") -- one
    version, one source of truth, Task 1's own design (spec sec 3)."""
    workspace = set(meta["workspace_members"])
    for p in meta["packages"]:
        if p["name"] == name and p["id"] in workspace:
            return p["version"]
    raise Fail(f"no workspace member named {name!r} in cargo metadata")


def cargo_packages(repo=REPO):
    """Normal (not dev, not build) dependencies of the shipped packages, not descending into
    proc-macros: a proc-macro and its own dependencies run in the compiler and link into nothing.
    Build dependencies are excluded for the same reason -- EXCEPT the C they compile into the
    binary (vendored Lua; Skia's prebuilt archive), which native_components() covers by looking
    at the binaries themselves. `repo` is where cargo metadata/tree run (--repo; default this
    checkout), so the same script can describe a different clone's dependency tree (Task 13, spec
    sec 8 step 6: the public clone at the release tag)."""
    meta = load_metadata(repo)
    workspace = set(meta["workspace_members"])
    by_nv = {}
    for p in meta["packages"]:
        by_nv.setdefault((p["name"], p["version"]), []).append(p)
    cmd = ["cargo", "tree", "--locked", "-e", "normal,no-proc-macro", "--target", TARGET,
           "--prefix", "none", "--format", "{p}"]
    for pkg in SHIPPED_CARGO_PACKAGES:
        cmd += ["-p", pkg]
    seen = set()
    for line in run(cmd, repo).splitlines():
        m = re.match(r"^(\S+) v(\S+)", line.strip())
        if m:
            seen.add((m.group(1), m.group(2)))
    out = []
    for nv in sorted(seen):
        cands = by_nv.get(nv, [])
        if len(cands) != 1:
            raise Fail(f"cargo tree names {nv[0]} {nv[1]}, which matches {len(cands)} packages in cargo metadata")
        p = cands[0]
        if p["id"] in workspace and own_code(p["name"], p["license"]):
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
        if pj["name"].startswith("@types/"):
            # DefinitelyTyped packages ship only .d.ts type declarations, never runtime code, so
            # they can never reach a built bundle regardless of how `npm ls` found them --
            # licences-claude-3/licences-codex-2 (2026-09-28), against @types/trusted-types, an
            # optional dependency of dompurify that `--omit=dev` does not exclude.
            continue
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

    if eco == "cargo" and name in WORKSPACE_THIRD_PARTY:
        note = "in neovibe's own repository, under its own licence: " + WORKSPACE_THIRD_PARTY[name]

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


def demangled_defined_symbols(path):
    """Every defined symbol name in `path`, demangled (`nm -C --defined-only`). Unlike
    defined_symbols() above (raw/mangled, used for exact-prefix matches like `_ZN8SkCanvas`), this is
    for substring checks against a crate name: a mangled symbol's crate-name prefix (`_ZN7nvim_rs...`)
    only ever appears once per symbol and misses trait impls implemented FOR nvim-rs types elsewhere
    and monomorphised generic code, where the crate name shows up mid-symbol in the demangled form."""
    out = run(["nm", "-C", "--defined-only", path], REPO)
    return [l.split(None, 2)[-1] for l in out.splitlines() if len(l.split(None, 2)) >= 3]


def binaries_containing_symbol_substring(shipped, substring):
    """Sorted names of `shipped` (a {binary name: path} map) whose demangled defined symbols contain
    `substring` anywhere, not just as a leading crate-name prefix (spec sec 11.1)."""
    return sorted(b for b, path in shipped.items() if any(substring in s for s in demangled_defined_symbols(path)))


def nvim_rs_symbol_check(shipped):
    """Which shipped binaries actually statically link nvim-rs code, checked by symbol rather than
    assumed (spec sec 11.1; plan Task 7). Returns the sorted hit list so COPYLEFT_ACK's nvim-rs note
    can name exactly them -- including any binary other than `shell` that turns out to carry it,
    rather than a hand-kept claim silently going stale."""
    hit = binaries_containing_symbol_substring(shipped, "nvim_rs")
    if "shell" not in hit:
        raise Fail("no shipped binary's symbols contain nvim_rs any more (nm -C --defined-only found "
                   f"it in {hit or 'none'}); the LGPL-3.0 sec 4(d)(0) route recorded in COPYLEFT_ACK "
                   "assumes `shell` statically links it and needs re-checking, not silently updating")
    return hit


# Symbols the linker itself synthesizes: they sort after the last input file's FILE entry in the
# symbol table, so they must not be attributed to that file.
_FREETYPE_MODULES = {"autofit.c", "bdf.c", "cff.c", "gxvalid.c", "otvalid.c", "pcf.c", "pfr.c",
                     "psaux.c", "pshinter.c", "psnames.c", "raster.c", "sdf.c", "sfnt.c", "smooth.c",
                     "svg.c", "truetype.c", "type1.c", "type1cid.c", "type42.c", "winfnt.c"}
_ZLIB_FILES = {"adler32.c", "compress.c", "crc32.c", "deflate.c", "gzclose.c", "gzlib.c", "gzread.c",
               "gzwrite.c", "infback.c", "inffast.c", "inflate.c", "inftrees.c", "trees.c",
               "uncompr.c", "zutil.c"}
_CHROMIUM_ZLIB_FILES = {"adler32_simd.c", "crc32_simd.c", "cpu_features.c", "inffast_chunk.c",
                        "crc_folding.c"}
_LIBPNG_EXTRA = {"intel_init.c", "filter_sse2_intrinsics.c"}

LINKER_LOCALS = {"__FRAME_END__", "__TMC_END__", "_GLOBAL_OFFSET_TABLE_", "_DYNAMIC", "_init",
                 "_fini", "__dso_handle", "__GNU_EH_FRAME_HDR"}


# GCC's own C runtime startup code: crtbeginS.o/crtendS.o, compiled from libgcc's crtstuff.c, which
# the system C compiler links into every executable (deregister_tm_clones, register_tm_clones,
# __do_global_dtors_aux, frame_dummy). Ubuntu 24.04's GCC 13 keeps crtstuff.c's STT_FILE entry, so
# the release container's `shell` shows it; Arch's crtbeginS.o carries no FILE entry at all, which is
# why a host build never did. GPL-3.0-or-later WITH the GCC Runtime Library Exception 3.1, whose
# section 1 lets a program compiled by GCC be conveyed under terms of its own choice: no notice is
# required. It gets an entry anyway, with the exception's own text, so every C file that owns code
# in a shipped binary still maps to a component with a text (the closed set below).
GCC_RUNTIME_C_FILES = {"crtstuff.c"}
GCC_RUNTIME_TITLE = "GCC runtime startup code (crtstuff.c)"


def classify_c_file(f, lua_files):
    """The component a C source file (an STT_FILE name) belongs to, or None when this script has no
    licence text for it -- which fails the run (native_components())."""
    if f in lua_files:
        return "Lua"
    if f in _FREETYPE_MODULES or re.match(r"^ft\w*\.c$", f):
        return "FreeType"
    if f in _LIBPNG_EXTRA or re.match(r"^png\w*\.c$", f):
        return "libpng"
    if f in _ZLIB_FILES:
        return "zlib"
    if f in _CHROMIUM_ZLIB_FILES:
        return "Chromium zlib"
    if f in GCC_RUNTIME_C_FILES:
        return "GCC runtime"
    return None


def gcc_runtime_component(bins):
    return {"title": GCC_RUNTIME_TITLE, "license": "GPL-3.0-or-later WITH GCC-exception-3.1", "status": None,
            "note": f"GCC's crtbeginS.o/crtendS.o, linked by the system C compiler into {', '.join(sorted(bins))}; "
                    "the GCC Runtime Library Exception lets the compiled program be conveyed under its own terms, "
                    "so no notice is required -- its text is here so every C file with code has a component",
            "files": [("COPYING.RUNTIME", read_text(os.path.join(TEXTS, "gcc-COPYING.RUNTIME")))]}


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


def skia_license_text(binaries_dir, override=None):
    """The prebuilt Skia archive's own LICENSE_SKIA, found beside the binaries rather than hardcoded
    to this checkout's own build tree. By default that is <binaries_dir>/build/skia-bindings-*/out/
    skia/LICENSE_SKIA -- a sibling of the shipped binaries themselves under the SAME cargo target dir
    that produced them, so this follows --binaries-dir like everything else here (Task 12's container
    builds under CARGO_TARGET_DIR=/build/target, not this repo's own target/). `override` (--skia-
    license-dir) is for a tree with no build subtree at all -- Task 13: a public clone plus an
    extracted release tarball, which ships no target/ directory to look under."""
    base = override or os.path.join(binaries_dir, "build")
    lic = sorted(glob.glob(os.path.join(base, "skia-bindings-*", "out", "skia", "LICENSE_SKIA")))
    texts = sorted(set(read_text(f) for f in lic))
    if len(texts) != 1:
        raise Fail(f"expected one Skia licence under {base}/skia-bindings-*/out/skia, found {len(texts)} distinct")
    return texts[0]


def native_components(by_nv, shipped, binaries_dir, skia_license_dir=None):
    """C/C++ code and data assets inside the binaries, found by looking at the binaries.
    `binaries_dir`/`skia_license_dir` are only needed for the Skia licence lookup; see
    skia_license_text()."""
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
    c_seen = {}
    for b in SHIPPED_BINARIES:
        for f in c_files_with_code(shipped[b]):
            comp = classify_c_file(f, lua_files)
            if comp is None:
                raise Fail(f"{b} contains code compiled from {f}, which belongs to no component this "
                           "script has a licence text for. Find which library it is and add it.")
            c_seen.setdefault(comp, set()).add(b)
    if "GCC runtime" in c_seen:
        comps.append(gcc_runtime_component(c_seen["GCC runtime"]))

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
        text = skia_license_text(binaries_dir, skia_license_dir)
        comps.append({"title": "Skia", "license": "BSD-3-Clause", "status": None,
                      "note": f"prebuilt static library (rust-skia's skia-binaries), linked into {', '.join(skia_hit)}",
                      "files": [("LICENSE_SKIA", text)]})

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
            "Chromium zlib": lambda t: t.startswith("Chromium zlib"),
            "GCC runtime": lambda t: t == GCC_RUNTIME_TITLE}
    for comp, bins in sorted(c_seen.items()):
        if not any(need[comp](t) for t in titles):
            raise Fail(f"{', '.join(sorted(bins))} contains {comp} code (by its C source files) but "
                       f"no {comp} entry was emitted: its symbol detector missed it")

    comps.insert(0, rust_std_component(shipped))
    return comps


# ---------------------------------------------------------------------------------------------
# The prebuilt Skia archive's own contents (verdict #7 of the
# v1-dist code review): the public source asset redistributes the WHOLE archive (spec D9), not just
# whatever a shipped binary ends up linking after --gc-sections discards the rest. That is a
# different question from native_components() above, which only ever sees what `shell` itself
# actually links -- measured (2026-09-27) at 0 for every one of expat/libjpeg-turbo/Wuffs/HarfBuzz/
# ICU despite all five being compiled into the archive's .a files.
# ---------------------------------------------------------------------------------------------

# Every object member's name is "<rust-skia build_support target>.<source file>.o", except
# skia-bindings' own generated C++ bridge objects, named "<16 hex content hash>-<name>.o". The part
# before the first '.' is a Skia (or skia-bindings) GN/build_support target name, which is what this
# table matches on -- stable across an ordinary Skia bump because it is not renamed per release, the
# same assumption c_component() makes about a C source file's own name.
_SKIA_ARCHIVE_THIRD_PARTY_STEMS = {
    "libexpat": "expat",
    "libfreetype2": "FreeType",
    "libharfbuzz": "HarfBuzz",
    "libjpeg": "libjpeg-turbo",
    "libjpeg12": "libjpeg-turbo",
    "libjpeg16": "libjpeg-turbo",
    "libpng": "libpng",
    "libwuffs": "Wuffs",
    "libicu": "ICU",
    "libzlib": "zlib",
}
# Chromium's zlib-fork additions (the same SIMD/chunked-inflate code native_components() already
# knows by symbol as "Chromium zlib" when it reaches a shipped binary).
_SKIA_ARCHIVE_CHROMIUM_ZLIB_STEMS = {"zlib_adler32_simd", "zlib_crc32_simd", "zlib_inflate_chunk_simd"}

# Every other target this project has found inside the pinned archive (skia-binaries-b7f043e0b1e2a-
# 850e702-x86_64-unknown-linux-gnu-ftembed-ganesh-gl-jpegd-jpege-pdf-textlayout.tar.gz, tag 0.153.3,
# sha256 ef41a8ff...0caf10): all Skia's own code -- font-manager backends, its own codec wrappers
# around the libraries above (e.g. "jpeg_decode.SkJpegCodec.o" is Skia's wrapper, not libjpeg itself;
# checked by reading its actual defined symbols, not assumed from the name), skcms, SkSL, gpu
# backends, and so on. A target not in this set and not matching a known third-party stem above (or
# skia-bindings' hash-prefixed pattern below) fails the run naming it -- deliberately: a Skia bump
# that starts bundling something new must be looked at, not silently folded into "Skia" or dropped.
_SKIA_ARCHIVE_OWN_STEMS = {
    "clipstack_utils", "core", "gpu", "gpu_shared", "jpeg_decode", "jpeg_encode", "libskcms",
    "libskia", "libskparagraph", "libskshaper", "libskunicode_core", "libskunicode_icu", "ml3",
    "ml4", "pathops", "pdf", "typeface_freetype", "typeface_proxy", "wuffs", "xml",
    "fontmgr_android", "fontmgr_android_parser", "fontmgr_custom", "fontmgr_custom_directory",
    "fontmgr_custom_embedded", "fontmgr_custom_empty", "fontmgr_fontconfig",
    "fontmgr_FontConfigInterface", "png_decode_common", "png_decode_libpng", "png_encode_common",
    "png_encode_libpng", "skcms_TransformHsw", "skcms_TransformSkx",
}
# skia-bindings' own generated C++ bridge (rust-skia's crate, already licensed in part 1 above as a
# cargo dependency -- this is not a second, separate third party).
_SKIA_BINDINGS_STEM = re.compile(r"^[0-9a-f]{8,}-\w+$")


def _skia_archive_member_component(stem):
    """The component a `.a` member's name stem (everything up to its first '.') belongs to, or None
    if this script has never seen it."""
    if stem in _SKIA_ARCHIVE_THIRD_PARTY_STEMS:
        return _SKIA_ARCHIVE_THIRD_PARTY_STEMS[stem]
    if stem in _SKIA_ARCHIVE_CHROMIUM_ZLIB_STEMS:
        return "Chromium zlib"
    if stem in _SKIA_ARCHIVE_OWN_STEMS or _SKIA_BINDINGS_STEM.match(stem):
        return "Skia"
    return None


# A defined-symbol prefix expected somewhere among a component's own members: not how members are
# classified (their own name already does that, above, unambiguously against this pinned archive --
# see the module-level comment), but independent corroborating evidence, and the source of the
# counts this script's own commit history records against verdict #7's table.
_SKIA_ARCHIVE_SYMBOL_PREFIXES = {
    "expat": re.compile(r"^XML_"),
    "libjpeg-turbo": re.compile(r"^(jpeg_|jinit_)"),
    "Wuffs": re.compile(r"^wuffs_"),
    "HarfBuzz": re.compile(r"^hb_"),
    "ICU": re.compile(r"^(u_|ubrk_)"),
}


# The one directory _skia_archive_root() owns inside a caller's --skia-archive-extract-dir.
SKIA_ARCHIVE_EXTRACT_SUBDIR = "skia-archive"


def _skia_archive_root(archive_path, extract_dir, expected_sha256=None):
    """archive_path is either an already-extracted directory or the .tar.gz/.tgz rust-skia's build
    script downloads (e.g. skia-binaries-<key>.tar.gz, extracting to a top-level skia-binaries/
    directory) -- collect-licenses.py accepts either, per its own --skia-archive-path.

    extract_dir is where a .tar.gz is extracted; it is ignored when archive_path already names a
    directory. The extraction goes into extract_dir/SKIA_ARCHIVE_EXTRACT_SUBDIR, the one directory
    this function owns: that subdirectory alone is removed and rebuilt on every run (so this is
    idempotent and never silently serves a stale extraction), and nothing else in extract_dir is
    touched. It used to remove and rebuild extract_dir itself, so a caller naming a directory it also
    uses -- /build/out, the release output, or the directory holding the archive -- would have lost
    it (review, Task 2 fix round 2). An archive inside that owned subdirectory is refused rather than
    deleted. extract_dir is a REQUIRED, caller-owned scratch location -- deliberately never derived
    from archive_path itself (e.g. a sibling "<archive_path>.extracted", which this function used to
    write). That broke two ways, found in review: release.sh's own --skia-archive-path names a file
    under /build/skia, which is read-only during the offline build phase (the Task 12 pre-think's
    cache-mount table), so writing beside it raised EROFS there; and on a dev machine --skia-archive-
    path can point straight at another checkout's shared cargo build cache (rust-skia's own
    out/.cache/), where it deleted and rebuilt a 63 MB tree in place on every run. This also
    deliberately never falls back to a bare tempfile.mkdtemp(), unlike extract_dir's own callers may
    reasonably use internally -- release.sh's own scratch dir, or a test's own scratch helper, never
    /tmp itself on this project's dev machines, where it is a small shared tmpfs.

    expected_sha256, when given, is verified against archive_path's own bytes before anything is
    extracted or otherwise trusted: SOURCE's own --skia-archive/--skia-sha256 name one pinned archive
    by hash, and this function's caller must not silently generate notices from a different file that
    happens to share --skia-archive-path's name. Not checked when archive_path is already a directory
    (there is no longer a single set of file bytes to hash).

    The tarball itself is otherwise treated as this project's own pinned, sha256-verified build
    cache, not untrusted input, so no path-filtering beyond Python 3.14's own default ("data")
    extractall() behaviour is added here."""
    if os.path.isdir(archive_path):
        return archive_path
    if not archive_path.endswith((".tar.gz", ".tgz")):
        raise Fail(f"{archive_path} is neither a directory nor a .tar.gz/.tgz Skia archive")
    if expected_sha256:
        h = hashlib.sha256()
        with open(archive_path, "rb") as f:
            for chunk in iter(lambda: f.read(1 << 20), b""):
                h.update(chunk)
        got = h.hexdigest()
        if got.lower() != expected_sha256.lower():
            raise Fail(f"{archive_path} sha256 is {got}, not the pinned {expected_sha256} "
                       "(--skia-sha256): refusing to trust it")
    if not extract_dir:
        raise Fail(f"{archive_path} is a .tar.gz/.tgz archive: an extract_dir is required to unpack "
                   "it into (never beside the archive itself, which may be read-only or a shared "
                   "cache this must not write into) -- pass --skia-archive-extract-dir")
    owned = os.path.join(extract_dir, SKIA_ARCHIVE_EXTRACT_SUBDIR)
    owned_real = os.path.realpath(owned)
    if os.path.commonpath([owned_real, os.path.realpath(archive_path)]) == owned_real:
        raise Fail(f"{archive_path} lies inside {owned}, which is replaced on every run: put the "
                   "archive somewhere else, or pass a different --skia-archive-extract-dir")
    if os.path.islink(owned) or (os.path.exists(owned) and not os.path.isdir(owned)):
        raise Fail(f"{owned} is a symlink or not a directory: refusing to replace it")
    if os.path.isdir(owned):
        shutil.rmtree(owned)
    os.makedirs(owned)
    with tarfile.open(archive_path) as tf:
        tf.extractall(owned, filter="data")
    return owned


def _fail_if_a_skia_member_defines_a_third_party_symbol(path, base, member_comp):
    """Corroborates the name-based classification above the other way (verdict #7, review minor 4):
    _skia_archive_member_component() classifies a member by its own filename, which is unambiguous
    against this pinned archive (every member checked once, by hand, against its real defined
    symbols) but says nothing about a FUTURE archive that compiled third-party source into an
    existing Skia-named build target. A member already classified as "Skia" that also DEFINES a
    global (T-type) symbol matching one of _SKIA_ARCHIVE_SYMBOL_PREFIXES would be exactly that --
    third-party code hiding inside a name this script's own table already claims for Skia -- so it
    fails the run the same fail-closed way an unrecognised member name does, rather than silently
    keeping that code's licence unacknowledged. This never fires against the pinned archive: every
    global symbol matching one of those five prefixes lives only in members already classified as
    that same component (checked with `nm -C`, since a C++ symbol's own *mangled* name never
    collides with a plain C-linkage prefix like `hb_` or `wuffs_`)."""
    skia_members = {m for m, c in member_comp.items() if c == "Skia"}
    if not skia_members:
        return
    for line in run(["nm", "--defined-only", "-A", path], REPO).splitlines():
        parts = line.split(":", 2)
        if len(parts) != 3:
            continue
        _archive, member, rest = parts
        if member not in skia_members:
            continue
        fields = rest.split()
        if len(fields) != 3 or fields[1] != "T":
            continue
        sym = fields[2]
        for comp, pat in _SKIA_ARCHIVE_SYMBOL_PREFIXES.items():
            if pat.match(sym):
                raise Fail(f"{base}({member}) inside the Skia prebuilt archive is classified as Skia "
                           f"by name, but defines {sym!r}, which matches {comp}'s own symbol prefix "
                           f"({pat.pattern}) -- third-party source may have been compiled into a "
                           "Skia build target under a name this script already treats as Skia's own; "
                           "reclassify it or investigate before trusting this notice")


def skia_archive_components(archive_dir):
    """The components every object member of every `.a` file under archive_dir (searched
    recursively, since the real archive nests its libraries one level inside a top-level
    skia-binaries/ directory) belongs to: {component: sorted(["<archive.a>(<member>.o)", ...])}.

    Classification is by each member's own name (_skia_archive_member_component); a member whose
    stem this script has never seen fails the run naming it -- an object member that maps to no
    known component is exactly what must not silently pass, the same fail-closed shape
    native_components()'s c_component() already uses for a shipped binary's own C files. A member
    classified as Skia that also defines a symbol one of the OTHER five components' own prefix
    matches fails the same way (_fail_if_a_skia_member_defines_a_third_party_symbol)."""
    archives = sorted(glob.glob(os.path.join(archive_dir, "**", "*.a"), recursive=True))
    if not archives:
        raise Fail(f"no .a files found under {archive_dir}: not a Skia prebuilt archive?")
    by_comp = {}
    for path in archives:
        base = os.path.basename(path)
        members = [l.strip() for l in run(["ar", "t", path], REPO).splitlines() if l.strip()]
        member_comp = {}
        for m in members:
            stem = m.split(".", 1)[0]
            comp = _skia_archive_member_component(stem)
            if comp is None:
                raise Fail(f"{base}({m}) inside the Skia prebuilt archive belongs to no component "
                           "this script knows about; find which library it is and add it")
            member_comp[m] = comp
            by_comp.setdefault(comp, []).append(f"{base}({m})")
        _fail_if_a_skia_member_defines_a_third_party_symbol(path, base, member_comp)
    return {c: sorted(ms) for c, ms in by_comp.items()}


def skia_archive_symbol_counts(archive_dir):
    """How many of each _SKIA_ARCHIVE_SYMBOL_PREFIXES-matching symbol the archive's `.a` files
    define, keyed the same as that table -- the corroborating counts verdict #7's own evidence table
    used (71 XML_*, 89 jpeg_*  and 34 jinit_* separately, 71 wuffs_*, 547 hb_* per library, 194
    u_*/ubrk_*), read via `nm --defined-only`, independent of skia_archive_components()'s own
    member-name classification above."""
    archives = sorted(glob.glob(os.path.join(archive_dir, "**", "*.a"), recursive=True))
    counts = {comp: 0 for comp in _SKIA_ARCHIVE_SYMBOL_PREFIXES}
    for path in archives:
        for l in run(["nm", "--defined-only", path], REPO).splitlines():
            parts = l.split()
            # A 3-field line is "<addr> <type> <name>"; "T" (global function/text) is what verdict
            # #7's own table counted -- other global types (R/D/W/...) and local ones alike can share
            # a component's prefix without being one of its own named functions (e.g. a `d
            # XML_GetFeatureList.features` local data symbol in expat, or the several R/D/W-typed
            # wuffs_base__*_TABLE data symbols wuffs generates).
            if len(parts) != 3 or parts[1] != "T":
                continue
            sym = parts[-1]
            for comp, pat in _SKIA_ARCHIVE_SYMBOL_PREFIXES.items():
                if pat.match(sym):
                    counts[comp] += 1
    return counts


def _license_skia_from_archive(archive_dir):
    """LICENSE_SKIA as shipped inside the archive itself (its own root, beside the .a files) --
    unlike skia_license_text() above, this needs no cargo build tree: --skia-archive-path names the
    archive directly, so its own copy is simpler and more robust than re-deriving a build/ path."""
    found = sorted(set(read_text(f) for f in glob.glob(os.path.join(archive_dir, "**", "LICENSE_SKIA"),
                                                        recursive=True)))
    if len(found) != 1:
        raise Fail(f"expected one LICENSE_SKIA under {archive_dir}, found {len(found)} distinct")
    return found[0]


def skia_archive_notice_entries(archive_path, extract_dir, expected_sha256=None):
    """Licence entries for every component skia_archive_components() finds inside the release's
    Skia prebuilt archive (verdict #7). Skia itself, FreeType, libpng and zlib are included even
    though their text is very likely already printed by native_components() (what `shell` actually
    links) -- render_entries()'s own dedup-by-hash collapses an identical repeat into a one-line
    pointer, so it costs a line, not a second full licence body, and skia_archive_components() must
    classify every member regardless of which of them ends up in a shipped binary.

    extract_dir and expected_sha256 are passed straight through to _skia_archive_root() -- see its
    own docstring."""
    root = _skia_archive_root(archive_path, extract_dir, expected_sha256)
    by_comp = skia_archive_components(root)

    def text(comp):
        if comp == "Skia":
            return "BSD-3-Clause", [("LICENSE_SKIA", _license_skia_from_archive(root))]
        if comp == "FreeType":
            return "FTL", [("freetype-FTL.TXT", read_text(os.path.join(TEXTS, "freetype-FTL.TXT")))]
        if comp == "libpng":
            return "libpng-2.0", [("libpng.LICENSE", read_text(os.path.join(TEXTS, "libpng.LICENSE")))]
        if comp == "zlib":
            return "Zlib", [("zlib.NOTICE", read_text(os.path.join(TEXTS, "zlib.NOTICE")))]
        if comp == "Chromium zlib":
            return "BSD-3-Clause", [("notice", "Copyright 2017 The Chromium Authors. Copyright (C) 2017 ARM, Inc.\n"),
                                     ("chromium.LICENSE", read_text(os.path.join(TEXTS, "chromium.LICENSE")))]
        if comp == "Wuffs":
            return "Apache-2.0", [("notice", "Copyright 2017 The Wuffs Authors.\n"),
                                   ("Apache-2.0.txt", read_text(os.path.join(TEXTS, "Apache-2.0.txt")))]
        if comp == "expat":
            return "MIT", [("expat.COPYING", read_text(os.path.join(TEXTS, "expat.COPYING")))]
        if comp == "HarfBuzz":
            return "MIT", [("harfbuzz.COPYING", read_text(os.path.join(TEXTS, "harfbuzz.COPYING")))]
        if comp == "ICU":
            return "Unicode-3.0", [("icu.LICENSE", read_text(os.path.join(TEXTS, "icu.LICENSE")))]
        if comp == "libjpeg-turbo":
            return "IJG AND BSD-3-Clause AND Zlib", [
                ("libjpeg-turbo.LICENSE.md", read_text(os.path.join(TEXTS, "libjpeg-turbo.LICENSE.md"))),
                ("libjpeg-turbo-ijg.README", read_text(os.path.join(TEXTS, "libjpeg-turbo-ijg.README"))),
            ]
        raise Fail(f"skia_archive_components() reported the unhandled component {comp!r}: "
                   "add its licence text here")

    entries = []
    for comp in sorted(by_comp):
        members = by_comp[comp]
        archives_here = sorted({m.split("(", 1)[0] for m in members})
        license_id, files = text(comp)
        # Every component's text is vendored under packaging/license-texts/ EXCEPT Skia's own, which
        # is its own LICENSE_SKIA read straight out of the archive (_license_skia_from_archive) --
        # review minor 6: the note used to say "vendored" for every entry alike, which was wrong for
        # the one component whose text this archive itself supplies.
        source = "its own LICENSE_SKIA inside the archive" if comp == "Skia" \
            else "text vendored in packaging/license-texts/"
        entries.append({
            "title": comp, "license": license_id, "status": None,
            "note": f"{len(members)} object file(s) inside the Skia prebuilt archive shipped in the "
                    f"source asset ({', '.join(archives_here)}); {source}",
            "files": files,
        })
    return entries


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


def rust_std_component(shipped):
    """The Rust standard library (std, core, alloc, compiler_builtins and the crates.io crates std
    itself depends on) is statically linked into every Rust binary, and no `cargo tree` lists it.
    The toolchain ships the notice file meant for exactly this, COPYRIGHT-library.html; it is
    reproduced as text. The compiler that built the binaries must be the one whose notice is read.
    Reads `shipped` (the same --binaries-dir paths native_components() already resolved) rather
    than a hardcoded target/release, so this follows --binaries-dir like everything else here."""
    built = set()
    for b in SHIPPED_BINARIES:
        out = run(["readelf", "-p", ".comment", shipped[b]], REPO)
        m = re.search(r"rustc version (\S+ \([0-9a-f]+ [0-9-]+\))", out)
        if not m:
            raise Fail(f"{shipped[b]} names no rustc in its .comment section")
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
            "note": f"statically linked into all {_count_word(len(SHIPPED_BINARIES))} binaries; text is the "
                     "toolchain's share/doc/rust/COPYRIGHT-library.html, tags removed",
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


def vite_runtime(shipped, counts, repo=REPO):
    """Code the bundler itself writes into the web bundle, which `npm ls --omit=dev` cannot list
    because Vite is a devDependency: its module-preload polyfill (measured present in `shell` on
    2026-09-19). Vite's own MIT notice, without the licences of Vite's bundled build-time
    dependencies, none of which reach the output."""
    with open(shipped["shell"], "rb") as f:
        if b'supports("modulepreload")' not in f.read():
            return []
    vite = os.path.join(repo, "agent-ui", "web", "node_modules", "vite")
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


# The exact LGPL-3.0 section 4(a) "prominent notice" sentence (spec sec 11.3's 4(a) row). Shared
# between THIRD-PARTY-LICENSES (build_header) and SOURCE (write_source_notice) so the two files
# never say it differently, and takes the real resolved version rather than a hardcoded one nothing
# re-checks: a bump trips EXTRA_TEXTS's own version-mismatch guard elsewhere in this file, but that
# guard has nothing to do with this sentence, and a stale version here could otherwise survive it.
def nvim_rs_lgpl_notice(version):
    return (f"This program statically links nvim-rs {version}, which is licensed under the GNU "
            "LGPL-3.0; its use is covered by that licence.")


def build_header(no_sidecar, source_url, copyleft, rust_count, native_count, web_count, sc, sc_pkgs_count,
                  counts_rust, counts_web, counts_sc, skia_archive_count=0):
    """The prose block before PART 1, as its own function so a unit test can check the two profiles'
    shapes (spec sec 11.3 4(a); Review Focus 4/5) without a real build. `sc` is the sidecar_facts()
    dict, or None when no_sidecar. `skia_archive_count` (no_sidecar only) is the number of entries
    skia_archive_notice_entries() found, for the "Contents:" list below -- review minor 6: that list
    used to stop at part 3/4 and never mentioned the archive-contents section at all."""
    nvim_rs = next((e for e in copyleft if e["name"] == "nvim-rs"), None)
    if nvim_rs is None:
        raise Fail("no nvim-rs entry in copyleft: the LGPL-3.0 sec 4(a) notice has no version to name")
    # LGPL-3.0 4(a): a "prominent notice ... that the Library is used" -- first, in BOTH profiles
    # (spec sec 11.3: "the same sentence is at the top of THIRD-PARTY-LICENSES"; the private/sidecar
    # profile ships the identical statically-linked `shell`, so 4(a) is owed there too).
    L = [RULE, "neovibe -- third-party licences", RULE, "", nvim_rs_lgpl_notice(nvim_rs["version"]), ""]
    L += [
        "neovibe's own code is MIT-licensed: see LICENSE, installed beside this file.",
        "",
        "This package also contains other people's code, each part under its own licence,",
        "reproduced below. MIT covers neovibe's code only; it does not relicense any of this.",
    ]
    if not no_sidecar:
        L += [
            "",
            "ONE PART IS NOT OPEN SOURCE. /usr/lib/neovibe/verdandi-claude-sidecar bundles",
            f"@anthropic-ai/claude-agent-sdk {sc['sdk']}, which is (c) Anthropic PBC, all rights",
            "reserved, and whose use is subject to Anthropic's legal agreements. Its licence file is",
            "reproduced verbatim in part 4. The Claude Code CLI itself is NOT included: the sidecar",
            "runs the `claude` you install yourself.",
        ]
    L += [
        "",
        "Copyleft parts, each acknowledged individually in part 1, and where their",
        "unmodified source is published:",
    ]
    for e in copyleft:
        L.append(f"  {e['title']} [{e['license']}]  https://crates.io/crates/{e['name']}/{e['version']}")
    L += [
        f"neovibe's own source is at {source_url}.",
        "",
        "Libraries this package links dynamically from your system (GTK 4, WebKitGTK, GLib and",
        "their dependencies) are not redistributed here and are not listed.",
        "",
        "Generated by packaging/collect-licenses.py from the dependency trees of exactly what the",
        "package installs. Contents:",
        f"  part 1  Rust crates linked into the four binaries this package installs ({rust_count})",
        f"  part 2  the Rust standard library, native code and fonts in those binaries ({native_count})",
        f"  part 3  npm packages in the agent panel's web bundle, inside `shell`      ({web_count})",
    ]
    if not no_sidecar:
        L.append(f"  part 4  the sidecar: Node.js {sc['node']} and its npm packages              ({sc_pkgs_count})")
    if no_sidecar:
        L.append(f"  also    components inside the Skia prebuilt archive in the source asset      ({skia_archive_count})")
    L.append("")
    parts = [("part 1", counts_rust), ("part 3", counts_web)]
    if not no_sidecar:
        parts.append(("part 4", counts_sc))
    for title, counts in parts:
        L.append(f"Licence expressions, {title}:")
        for k in sorted(counts, key=lambda k: (-counts[k], k)):
            L.append(f"  {counts[k]:4d}  {k}")
        L.append("")
    return L


def collect(repo, binaries_dir, no_sidecar, source_url, sidecar_artifact=None, verdandi=None,
            skia_license_dir=None, skia_archive_path=None, skia_archive_extract_dir=None,
            skia_archive_sha256=None):
    """Build THIRD-PARTY-LICENSES. `no_sidecar` selects the public profile (parts 1-3 only,
    sidecar_artifact/verdandi ignored, plus the archive-contents section below); otherwise the
    private/dev profile (parts 1-4, sidecar_artifact and verdandi required, no archive-contents
    section -- the private profile does not ship the archive). `repo` and `binaries_dir` let this
    describe a different tree's dependencies and a different directory's binaries (Task 13, spec sec
    8 step 6: the public clone's Cargo.lock, the public release's extracted binaries). `skia_license_dir`
    (--skia-license-dir) overrides where the Skia licence lookup looks, for a `binaries_dir` with no
    build/ subtree of its own -- see skia_license_text(). `skia_archive_path` (--skia-archive-path,
    required when `no_sidecar`) is the Skia prebuilt archive this build linked -- see
    skia_archive_notice_entries(). `skia_archive_extract_dir` (--skia-archive-extract-dir) and
    `skia_archive_sha256` (--skia-sha256) are passed straight through to it: the former is required
    whenever `skia_archive_path` names a .tar.gz/.tgz rather than an already-extracted directory."""
    shipped = {b: os.path.join(binaries_dir, b) for b in SHIPPED_BINARIES}
    for b, p in shipped.items():
        if not os.path.isfile(p):
            raise Fail(f"{p} does not exist: build the release binaries first (publish.sh/release.sh does)")
    if no_sidecar and not skia_archive_path:
        raise Fail("--skia-archive-path is required with --no-sidecar (verdict #7): the public "
                   "source asset ships the whole Skia prebuilt archive, and its own components need "
                   "a notice regardless of what the shipped binaries link")

    counts_rust, counts_web, counts_sc = {}, {}, {}
    crates, by_nv = cargo_packages(repo)
    rust = [resolve(p, by_nv, counts_rust) for p in crates]
    native = native_components(by_nv, shipped, binaries_dir, skia_license_dir)
    web = [resolve(p, by_nv, counts_web) for p in npm_packages(os.path.join(repo, "agent-ui", "web"))]
    web += vite_runtime(shipped, counts_web, repo)

    sc, sc_pkgs, excluded, archive_entries = None, [], [], []
    if not no_sidecar:
        sc = sidecar_facts(sidecar_artifact, verdandi)
        for p in npm_packages(verdandi, "@verdandi/claude-sidecar"):
            if (p["eco"], p["name"]) in NOT_SHIPPED:
                excluded.append(p)
                continue
            sc_pkgs.append(resolve(p, by_nv, counts_sc))
    if no_sidecar:
        # Computed here, before build_header(), so the header's own "Contents:" list (review minor 6)
        # can show this section's count the same way it shows every other part's.
        archive_entries = skia_archive_notice_entries(skia_archive_path, skia_archive_extract_dir,
                                                        skia_archive_sha256)

    copyleft = [e for e in rust if e["copyleft"]]
    if sorted((e["name"] for e in copyleft)) != sorted(n for (eco, n) in COPYLEFT_ACK if eco == "cargo"):
        raise Fail(f"COPYLEFT_ACK names {sorted(n for _, n in COPYLEFT_ACK)} but the tree ships "
                   f"{sorted(e['name'] for e in copyleft)}: drop the stale acknowledgement")

    # Which shipped binaries actually carry nvim-rs code, checked by symbol (spec sec 11.1). Patches
    # the nvim-rs entry's already-resolved status text in place, naming the real hit list rather than
    # a hand-kept "`shell`" that a future binary picking it up would leave silently stale.
    nvim_rs_binaries = nvim_rs_symbol_check(shipped)
    for e in rust:
        if e["name"] == "nvim-rs" and e["status"] and "{binaries}" in e["status"]:
            e["status"] = e["status"].replace(
                "{binaries}", ", ".join(f"`{b}`" for b in nvim_rs_binaries))

    L = build_header(no_sidecar, source_url, copyleft, len(rust), len(native), len(web), sc,
                      len(sc_pkgs), counts_rust, counts_web, counts_sc, len(archive_entries))

    printed = {}
    L += [RULE, "PART 1 -- Rust crates linked into the shipped binaries", RULE, ""]
    render_entries(rust, printed, L)
    L += ["", RULE, "PART 2 -- the Rust standard library, native code and fonts in the shipped binaries", RULE, ""]
    render_entries(native, printed, L)
    L += ["", RULE, "PART 3 -- the agent panel's web bundle (embedded in shell)", RULE, ""]
    render_entries(web, printed, L)
    if not no_sidecar:
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
    if no_sidecar:
        L += ["", RULE, "Components inside the Skia prebuilt archive in the source asset", RULE, ""]
        L += ["The public source asset (named above) redistributes the WHOLE Skia prebuilt archive",
              "this build linked, not just the parts the binaries above actually reference -- so every",
              "component it contains needs a notice, whether or not it appears in PART 2.", ""]
        render_entries(archive_entries, printed, L)
    nvim_rs = next((e for e in copyleft if e["name"] == "nvim-rs"), None)
    if nvim_rs is None:
        raise Fail("no nvim-rs entry in the resolved copyleft crates: the LGPL-3.0 sec 4(a) notice "
                   "has no version to name")
    return "\n".join(L).rstrip("\n") + "\n", {"rust": counts_rust, "native": [c["title"] for c in native],
                                               "web": counts_web, "sidecar": counts_sc,
                                               "excluded": [p["name"] for p in excluded],
                                               "skia_archive": [c["title"] for c in archive_entries],
                                               "nvim_rs_version": nvim_rs["version"]}


def neovide_checkout_head(repo):
    """`git rev-parse HEAD` in <repo>/neovide when that directory is the root of its own checkout
    (the public tree's submodule) -- never an enclosing repository's HEAD, which `git` would climb
    to from a plain directory. Fail otherwise."""
    d = os.path.join(repo, "neovide")
    try:
        top = subprocess.run(["git", "-C", d, "rev-parse", "--show-toplevel"],
                             capture_output=True, text=True, check=True).stdout.strip()
        if os.path.realpath(top) == os.path.realpath(d):
            return subprocess.run(["git", "-C", d, "rev-parse", "HEAD"],
                                  capture_output=True, text=True, check=True).stdout.strip()
    except (OSError, subprocess.CalledProcessError):
        pass
    raise Fail("cannot tell the Neovide fork's commit for SOURCE: set NEOVIBE_BUILD_FORK_COMMIT, or run "
               f"where {d} is its own git checkout")


def write_source_notice(path, version, commit, url, skia_archive, skia_sha256, nvim_rs_version, fork_commit):
    """Write share/licenses/neovibe/SOURCE (spec sec 11.3): the LGPL-3.0 section 4(d)(0) route, with
    the modified-nvim-rs relink recipe release.sh proves offline (spec sec 4.2 step 8). `url` is
    printed exactly as given -- this function never derives or guesses a download URL; the caller
    (release.sh, via --source-url) is the one that knows the release's real asset link. `version` is
    neovibe's own (the `shell` package's) version; `nvim_rs_version` is nvim-rs's, resolved from the
    same dependency tree collect() already read rather than assumed. `commit` and `fork_commit` are
    what the shipped `--version` names; a rebuild from the asset (no .git anywhere in it) gets them
    back only through the two NEOVIBE_BUILD_* lines printed here, which release.sh's proof (a)
    reads out of this file and checks against the shipped binary."""
    lines = [
        nvim_rs_lgpl_notice(nvim_rs_version),
        "A copy of the GNU LGPL-3.0 (nvim-rs's own licence) and the GNU GPL-3.0 (LGPL-3.0 section",
        "4(b)) accompany this program in THIRD-PARTY-LICENSES, installed beside this file.",
        "",
        "Nothing in this program displays copyright notices at run time -- `--version` deliberately",
        "prints none, so LGPL-3.0 section 4(c) is not triggered today. If an About view or a notices",
        "view is ever added, it must include nvim-rs's own copyright notice among what it shows,",
        "plus a reference to the GNU GPL and GNU LGPL texts named above (LGPL-3.0 section 4(c)).",
        "",
        "This program's own terms are MIT, which does not restrict modification of nvim-rs or",
        "reverse engineering for debugging such modifications -- the precondition LGPL-3.0 section 4",
        "sets for everything below. No artifact, README or installer this project produces may add a",
        "term restricting modification or reverse engineering: that would break the precondition",
        "every route in section 4 depends on.",
        "",
        "Corresponding Source and Corresponding Application Code (LGPL-3.0 section 4(d)(0))",
        "-" * 78,
        "This program's Minimal Corresponding Source, and the Corresponding Application Code needed",
        "to recombine or relink it against a modified nvim-rs, are published at:",
        "",
        f"    {url}",
        "",
        f"for neovibe {version}, built from commit {commit}. That asset contains:",
        "  - this project's source tree at that commit (`git archive`);",
        f"  - the Neovide fork submodule, at its pinned commit {fork_commit};",
        "  - this project's Rust dependencies, vendored (`cargo vendor --locked`), nvim-rs's own",
        "    source among them;",
        "  - the built agent panel web bundle, agent-ui/web/dist/index.html, in object form (its own",
        "    source is under agent-ui/web/src/ in the same asset). Rebuild it with:",
        "        npm ci && npm run build",
        "    which needs network access to the npm registry;",
        f"  - the Skia prebuilt static archive this build linked: {skia_archive}, sha256",
        f"    {skia_sha256};",
        "  - Verdandi's proto/ tree (MIT) at the asset's root, from the pinned public Verdandi",
        "    revision: claude-runtime-protocol's build script compiles",
        "    ../../proto/verdandi/claude/runtime/v1/runtime.proto relative to its own vendored",
        "    crate directory, which is exactly vendor/claude-runtime-protocol/../../proto once this",
        "    asset is unpacked -- so an offline rebuild needs proto/ at the root to resolve that path;",
        "  - LICENSE, THIRD-PARTY-LICENSES and this file, at the asset's root.",
        "",
        "To rebuild the unmodified tree without a git checkout (no .git directory to read a commit",
        "from, in this project or in neovide/), set these before building, so `shell --version`",
        "names the same two commits the released binary does:",
        "",
        f"    NEOVIBE_BUILD_COMMIT={commit}",
        f"    NEOVIBE_BUILD_FORK_COMMIT={fork_commit}",
        "",
        "Relinking against a MODIFIED nvim-rs",
        "-" * 78,
        "This is the exact recipe LGPL-3.0 section 4(d)(0) requires this project to offer.",
        "",
        "    1. cp -r vendor/nvim-rs nvim-rs-modified",
        "    2. rm nvim-rs-modified/.cargo-checksum.json",
        "    3. make your changes inside nvim-rs-modified/, not inside vendor/nvim-rs",
        "    4. add this to the root Cargo.toml:",
        "           [patch.crates-io]",
        '           nvim-rs = { path = "nvim-rs-modified" }',
        f'    5. SKIA_BINARIES_URL="file://$PWD/skia/{skia_archive}" cargo build --release --offline -p shell',
        "",
        "Step 3 must go into nvim-rs-modified/, never into vendor/nvim-rs itself: `cargo vendor`",
        "records a per-file checksum for a directory source, so an in-place edit of vendor/nvim-rs",
        "fails that check before the modified code is even reached. Step 5 does not pin the",
        "lockfile, on purpose: step 4's patch rewrites nvim-rs's Cargo.lock entry, and a pinned build",
        "would refuse exactly that change. skia-bindings also needs that SKIA_BINARIES_URL to be an",
        "absolute file:// URL, which is why $PWD appears rather than a relative path.",
        "",
        "option-ext (pulled in via dirs-sys, used unmodified) is MPL-2.0, file-level copyleft. Its",
        "Source Code Form is published in vendor/option-ext inside the same asset named above, which",
        "is where MPL-2.0 section 3.2 requires it to be findable.",
    ]
    text = "\n".join(lines).rstrip("\n") + "\n"
    tmp = path + ".tmp"
    with open(tmp, "w", encoding="utf-8", newline="\n") as f:
        f.write(text)
    os.replace(tmp, path)
    return text


def main(argv):
    out = os.path.join(REPO, "dist", "THIRD-PARTY-LICENSES")
    artifact = None
    repo = REPO
    binaries_dir = None
    no_sidecar = False
    sidecar_given = False
    source_url = None
    source_notice = None
    skia_archive = None
    skia_sha256 = None
    skia_license_dir = None
    skia_archive_path = None
    skia_archive_extract_dir = None
    args = list(argv)
    while args:
        a = args.pop(0)
        if a == "--out":
            out = args.pop(0)
        elif a == "--sidecar":
            artifact = args.pop(0)
            sidecar_given = True
        elif a == "--no-sidecar":
            no_sidecar = True
        elif a == "--source-url":
            source_url = args.pop(0)
        elif a == "--source-notice":
            source_notice = args.pop(0)
        elif a == "--repo":
            repo = args.pop(0)
        elif a == "--binaries-dir":
            binaries_dir = args.pop(0)
        elif a == "--skia-archive":
            skia_archive = args.pop(0)
        elif a == "--skia-sha256":
            skia_sha256 = args.pop(0)
        elif a == "--skia-license-dir":
            skia_license_dir = args.pop(0)
        elif a == "--skia-archive-path":
            skia_archive_path = args.pop(0)
        elif a == "--skia-archive-extract-dir":
            skia_archive_extract_dir = args.pop(0)
        else:
            print(f"collect-licenses: unknown argument {a}", file=sys.stderr)
            return 2

    if no_sidecar and sidecar_given:
        print("collect-licenses: give --sidecar or --no-sidecar, not both", file=sys.stderr)
        return 2
    if no_sidecar and not source_url:
        print("collect-licenses: --source-url is required with --no-sidecar", file=sys.stderr)
        return 2
    if no_sidecar and not skia_archive_path:
        print("collect-licenses: --skia-archive-path is required with --no-sidecar", file=sys.stderr)
        return 2
    if source_notice and not (skia_archive and skia_sha256):
        print("collect-licenses: --source-notice needs --skia-archive and --skia-sha256", file=sys.stderr)
        return 2
    if source_notice and source_url is None:
        # Without this, --sidecar's own default (DEFAULT_SOURCE_URL, the bare repo) would end up
        # named as "that asset" in SOURCE's text, which does not contain a vendored source tree, a
        # web bundle or a Skia archive -- only a real release asset does.
        print("collect-licenses: --source-notice needs an explicit --source-url naming the real "
              "release asset (never the bare-repo default)", file=sys.stderr)
        return 2
    if source_url is None:
        source_url = DEFAULT_SOURCE_URL
    if binaries_dir is None:
        binaries_dir = os.path.join(repo, "target", "release")
    if artifact is None:
        artifact = os.path.join(REPO, "dist", "verdandi-claude-sidecar")

    verdandi = os.environ.get("NEOVIBE_VERDANDI_CHECKOUT") or os.path.expanduser("~/src/verdandi")
    try:
        text, summary = collect(repo, binaries_dir, no_sidecar, source_url,
                                 sidecar_artifact=None if no_sidecar else artifact,
                                 verdandi=None if no_sidecar else verdandi,
                                 skia_license_dir=skia_license_dir,
                                 skia_archive_path=skia_archive_path,
                                 skia_archive_extract_dir=skia_archive_extract_dir,
                                 skia_archive_sha256=skia_sha256)
        if source_notice:
            meta = load_metadata(repo)
            version = workspace_package_version(meta, "shell")
            commit = os.environ.get("NEOVIBE_BUILD_COMMIT") or run(["git", "rev-parse", "HEAD"], repo).strip()
            fork_commit = os.environ.get("NEOVIBE_BUILD_FORK_COMMIT") or neovide_checkout_head(repo)
            write_source_notice(source_notice, version, commit, source_url, skia_archive, skia_sha256,
                                 summary["nvim_rs_version"], fork_commit)
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
    if summary["skia_archive"]:
        print(f"  Skia archive contents: {', '.join(summary['skia_archive'])}", file=sys.stderr)
    if source_notice:
        print(f"collect-licenses: wrote {source_notice}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
