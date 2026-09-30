#!/usr/bin/env python3
"""packaging/release_check.py -- the release build's content and layout checks, as pure functions
(Task 12, plan docs/superpowers/plans/2026-09-27-v1-dist.md and its lane-D split,
docs/superpowers/plans/2026-09-27-v1-dist-task12.md, Task 1). Spec
docs/superpowers/specs/2026-09-27-v1-dist-design.md sec 4 describes the release release.sh (Task 4)
assembles; this file is every check that release.sh runs against it, factored out so each one has a
failing fixture and a passing fixture in packaging/tests/test_release_layout.py with no container and
no network -- the pattern packaging/collect-licenses.py and packaging/test_collect_licenses.py follow.

Usage (release.sh calls these; Task 4 fixed the set):
    python3 release_check.py validate-release-file RELEASE [--rehearsal]
    python3 release_check.py asset-names 0.2.0-rc.1 a2f194a
    python3 release_check.py gh-release-command 0.2.0-rc.1 --repo OWNER/NAME --title T --notes-file F asset1 ...
    python3 release_check.py check-legacy-symbols NM_C_OUTPUT_FILE
    python3 release_check.py tree-equal EXTRACTED_DIR REPO_DIR REV [--allow ADDITION ...]
    python3 release_check.py build-executables BUILD_JSON
    python3 release_check.py build-script-out-dir BUILD_JSON PACKAGE
    python3 release_check.py check-skia-output OUTPUT_FILE EXPECTED_URL
    python3 release_check.py relink-recipe SOURCE
    python3 release_check.py rebuild-env SOURCE
    python3 release_check.py lock-package-sources CARGO_LOCK NAME
    python3 release_check.py make-scan-view SRC_DIR DEST_DIR
    python3 release_check.py check-node-pin BUILD_BINARY_MJS PINS_ENV
    python3 release_check.py check-assets OUT_DIR CHECK_DIR VERSION VERDANDI_REV SRC_REPO VERDANDI_REPO \
        SKIA_ARCHIVE SKIA_SHA256
    python3 release_check.py check-release-signers INSTALL_SH RELEASE_SIGNERS
    python3 release_check.py twins ROOT

Every check function below takes plain data (a dict, a string, a list of (relpath, path) pairs) and
raises ReleaseCheckError on a problem -- never touches the network, and touches the filesystem only
to read fixtures/extracted files the caller already produced. Extraction (SS5) is the one part that
shells out, to the same tools spec sec 4.2 step 9 names: `tar`, `dpkg-deb`, `bsdtar`.
"""

from __future__ import annotations

import fnmatch
import hashlib
import io
import json
import os
import re
import shlex
import shutil
import subprocess
import sys
import tarfile
from typing import Callable, Iterable, Sequence


class ReleaseCheckError(Exception):
    """Raised by every check in this module; release.sh turns one into a non-zero exit naming it."""


# --- 1. The RELEASE field set (spec sec 4.3) -----------------------------------------------------

_HEX40 = re.compile(r"^[0-9a-f]{40}$")
_HEX64 = re.compile(r"^[0-9a-f]{64}$")
# Mirrors packaging/install.sh's NV_VERSION_ERE ('[0-9]+\.[0-9]+\.[0-9]+(-rc\.[0-9]+)?') -- a test
# in test_release_layout.py reads that literal back out of install.sh and cross-checks a handful of
# version strings against both, so a drift in either place is caught rather than silently diverging.
VERSION_RE = re.compile(r"^[0-9]+\.[0-9]+\.[0-9]+(-rc\.[0-9]+)?$")
_NODE_VERSION_RE = re.compile(r"^v[0-9]+\.[0-9]+\.[0-9]+$")
_NVIM_VERSION_RE = re.compile(r"^[0-9]+\.[0-9]+\.[0-9]+$")
_GTK_FLOOR_RE = re.compile(r"^[0-9]+\.[0-9]+$")
# A Docker image ID: 12 hex (the short form the CLI prints) or 64 hex (the full digest), either one
# optionally carrying a "sha256:" prefix. A local build (BUILD_IMAGE, no RepoDigest) prints the
# short form; a registry pull would print the full one -- this release never pulls (never --pull).
_IMAGE_ID_RE = re.compile(r"^(sha256:)?[0-9a-f]{12}([0-9a-f]{52})?$")
_VERDANDI_SOURCE_RE = re.compile(r"^verdandi-[0-9a-f]{7}-source\.tar\.gz$")
_SKIA_ARCHIVE_RE = re.compile(r"^skia-binaries-\S+\.tar\.gz$")

# Exactly the spec sec 4.3 RELEASE keys, which already include VERDANDI_SOURCE_SHA256 and the Skia
# pair (SKIA_BINARIES_ARCHIVE/SKIA_BINARIES_SHA256) -- Task 3 adds those two to packaging/pins.env
# under the same names used here.
RELEASE_FIELDS: dict[str, re.Pattern] = {
    "EITRI_VERSION": VERSION_RE,
    "EITRI_COMMIT": _HEX40,
    "NEOVIDE_FORK_COMMIT": _HEX40,
    "VERDANDI_REV": _HEX40,
    "VERDANDI_SOURCE": _VERDANDI_SOURCE_RE,
    "VERDANDI_SOURCE_SHA256": _HEX64,
    "NODE_VERSION": _NODE_VERSION_RE,
    "NODE_SHA256_linux_x64": _HEX64,
    "NODE_SHA256_linux_arm64": _HEX64,
    "NVIM_VERSION": _NVIM_VERSION_RE,
    "NVIM_SHA256_linux_x86_64": _HEX64,
    "SKIA_BINARIES_ARCHIVE": _SKIA_ARCHIVE_RE,
    "SKIA_BINARIES_SHA256": _HEX64,
    "GTK_FLOOR": _GTK_FLOOR_RE,
    "BUILD_IMAGE": _IMAGE_ID_RE,
}


def parse_release_text(text: str) -> dict[str, str]:
    """KEY=value, one per line, blank lines and '#'-led lines skipped -- the shape spec sec 4.3
    describes and both release.sh and the installer read with a `case` loop, never `eval`/`.`. The
    value is taken verbatim to end of line: no quoting, no expansion, so this parser matches
    exactly what a shell `case` reader sees."""
    fields: dict[str, str] = {}
    for lineno, raw in enumerate(text.splitlines(), start=1):
        line = raw.strip()
        if not line or line.startswith("#"):
            continue
        if "=" not in line:
            raise ReleaseCheckError(f"RELEASE line {lineno}: no '=' in {raw!r}")
        key, _, value = line.partition("=")
        fields[key] = value
    return fields


# A rehearsal (release.sh --rehearsal, lane D Task 4) is the one RELEASE allowed to leave out the
# nvim pin -- plan Task 11 adds it to packaging/pins.env, and until then a rehearsal still has to be
# able to run -- and it must say what it is: REHEARSAL=1, a field no real RELEASE ever carries.
REHEARSAL_OPTIONAL_FIELDS = ("NVIM_VERSION", "NVIM_SHA256_linux_x86_64")
REHEARSAL_FIELD = "REHEARSAL"


def validate_release(fields: dict[str, str], rehearsal: bool = False) -> None:
    """Raises ReleaseCheckError naming every problem at once (missing/extra keys, and any value
    that fails its own field's pattern) -- not just the first one found, so a broken RELEASE is
    fixed in a single pass rather than one failure at a time.

    rehearsal=True: REHEARSAL=1 is required, and the two nvim fields may be absent -- both or
    neither, never one of them. A real release (the default) may carry no REHEARSAL field at all."""
    problems = []
    required = set(RELEASE_FIELDS)
    allowed = set(RELEASE_FIELDS)
    if rehearsal:
        allowed.add(REHEARSAL_FIELD)
        required.add(REHEARSAL_FIELD)
        present_optional = [k for k in REHEARSAL_OPTIONAL_FIELDS if k in fields]
        if present_optional and len(present_optional) != len(REHEARSAL_OPTIONAL_FIELDS):
            problems.append(
                f"a rehearsal carries both nvim fields or neither, not only {', '.join(present_optional)}")
        required -= set(REHEARSAL_OPTIONAL_FIELDS)
        if fields.get(REHEARSAL_FIELD, "1") != "1":
            problems.append(f"{REHEARSAL_FIELD}={fields[REHEARSAL_FIELD]!r} is not '1'")
    missing = sorted(required - fields.keys())
    extra = sorted(fields.keys() - allowed)
    if missing:
        problems.append(f"missing field(s): {', '.join(missing)}")
    if extra:
        problems.append(f"unexpected field(s): {', '.join(extra)}")
    for key, pattern in RELEASE_FIELDS.items():
        if key in fields and not pattern.match(fields[key]):
            problems.append(f"{key}={fields[key]!r} does not match {pattern.pattern!r}")
    if problems:
        raise ReleaseCheckError("; ".join(problems))


def validate_release_text(text: str, rehearsal: bool = False) -> dict[str, str]:
    """parse_release_text then validate_release; returns the parsed fields on success."""
    fields = parse_release_text(text)
    validate_release(fields, rehearsal=rehearsal)
    return fields


# --- 2. Asset names (M7) --------------------------------------------------------------------------

ASSET_NAME_RE = re.compile(r"^[A-Za-z0-9._+-]+$")
_VERDANDI_REV7_RE = re.compile(r"^[0-9a-f]{7}$")


def asset_names(version: str, verdandi_rev7: str) -> list[str]:
    """The nine asset names of spec sec 4.3's tarball layout, for one release `version` (e.g.
    "1.0.0" or "1.0.0-rc.1") and the public Verdandi's 7-hex-char revision prefix. nfpm's own
    semver schema would otherwise write `1.0.0~rc.1`-shaped defaults (Task 12 pre-think, M7), so
    release.sh passes these as explicit --target names rather than trusting nfpm's default."""
    if not VERSION_RE.match(version):
        raise ReleaseCheckError(f"not a release version (X.Y.Z or X.Y.Z-rc.N): {version!r}")
    if not _VERDANDI_REV7_RE.match(verdandi_rev7):
        raise ReleaseCheckError(f"not a 7-hex Verdandi revision: {verdandi_rev7!r}")
    return [
        f"eitri-{version}-x86_64-linux.tar.gz",
        f"eitri_{version}_amd64.deb",
        f"eitri-{version}-1.x86_64.rpm",
        f"eitri-{version}-source.tar.gz",
        f"verdandi-{verdandi_rev7}-source.tar.gz",
        "install.sh",
        "RELEASE",
        "SHA256SUMS",
        "SHA256SUMS.sig",
    ]


def validate_asset_name(name: str) -> None:
    if not ASSET_NAME_RE.match(name):
        raise ReleaseCheckError(f"asset name has characters outside [A-Za-z0-9._+-]: {name!r}")


# --- 3. The printed `gh release create` line (sec 4.2 step 11) -----------------------------------

_GH_REPO_RE = re.compile(r"^[A-Za-z0-9-]+/[A-Za-z0-9._-]+$")


def gh_release_command(version: str, assets: Sequence[str], title: str, notes_file: str, repo: str) -> list[str]:
    """The argv release.sh prints -- and never runs (I4: "by hand") -- for `gh release create`.
    --repo always: release.sh runs from the private checkout, whose remotes are not the public
    GitHub repository, and `gh` otherwise picks its repository from the current directory's remotes;
    --verify-tag always, so `gh` cannot create a missing tag from the default branch's current tip;
    --prerelease exactly when `version` contains a '-', so an rc build never becomes
    releases/latest."""
    if not VERSION_RE.match(version):
        raise ReleaseCheckError(f"not a release version (X.Y.Z or X.Y.Z-rc.N): {version!r}")
    if not _GH_REPO_RE.match(repo):
        raise ReleaseCheckError(f"not an OWNER/NAME GitHub repository: {repo!r}")
    argv = ["gh", "release", "create", f"v{version}", "--repo", repo, *assets, "--verify-tag"]
    if "-" in version:
        argv.append("--prerelease")
    argv += ["--title", title, "--notes-file", notes_file]
    return argv


# --- 4. Walking an extracted tree ------------------------------------------------------------------

def iter_files(root: str) -> list[tuple[str, str]]:
    """[(posix-style relpath, absolute path), ...] for every regular file under `root`, in a
    deterministic (sorted) order. A relpath is always '/'-joined regardless of host path
    separator, so fixtures and checks agree on what a path "is" independent of platform."""
    root = os.path.abspath(root)
    out: list[tuple[str, str]] = []
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames.sort()
        for name in sorted(filenames):
            abspath = os.path.join(dirpath, name)
            rel = os.path.relpath(abspath, root).replace(os.sep, "/")
            out.append((rel, abspath))
    out.sort()
    return out


# --- 5. Extraction (release.sh runs these on the real, built assets) -----------------------------
#
# Spec sec 4.2 step 9: "every .tar.gz with tar -xzf, the .deb with dpkg-deb -x, the .rpm with
# bsdtar -xf". These shell out to those exact tools rather than reimplementing .deb/.rpm reading,
# so a check never disagrees with what a real install would actually unpack.

def extract_tar_gz(archive_path: str, dest_dir: str) -> None:
    os.makedirs(dest_dir, exist_ok=True)
    subprocess.run(["tar", "-xzf", os.path.abspath(archive_path), "-C", dest_dir], check=True)


def extract_deb(archive_path: str, dest_dir: str) -> None:
    os.makedirs(dest_dir, exist_ok=True)
    subprocess.run(["dpkg-deb", "-x", os.path.abspath(archive_path), dest_dir], check=True)


def extract_rpm(archive_path: str, dest_dir: str) -> None:
    os.makedirs(dest_dir, exist_ok=True)
    subprocess.run(["bsdtar", "-xf", os.path.abspath(archive_path), "-C", dest_dir], check=True)


# --- 6. Content scans over an extracted asset (sec 4.2 step 9, M2/M3/M16) ------------------------

# Built from two halves so the joined literal never appears anywhere in this repository's own
# tracked source (M5): if it did, scanning the Eitri *source* asset -- which is `git archive
# HEAD`, i.e. this very file among everything else -- for the sentinel would find a hit on itself
# and fail every release. packaging/tests/test_release_layout.py asserts the joined string is
# absent from this file and its own file, so that stays true.
_SEA_SENTINEL_PREFIX = "NODE_SEA_FUSE_"
_SEA_SENTINEL_SUFFIX = "fce680ab2cc467b6e072b8b5df1996b2"
SEA_SENTINEL = _SEA_SENTINEL_PREFIX + _SEA_SENTINEL_SUFFIX


def find_sentinel_hits(files: Iterable[tuple[str, str]]) -> list[str]:
    """relpaths whose content contains the Node SEA fuse sentinel, whatever the file is named --
    this is what catches a sidecar binary renamed away from `verdandi-claude-sidecar*` (spec sec
    4.2 step 9's own parenthetical: "so a renamed sidecar is still caught")."""
    needle = SEA_SENTINEL.encode("ascii")
    hits = []
    for rel, path in files:
        with open(path, "rb") as f:
            if needle in f.read():
                hits.append(rel)
    return hits


_ANTHROPIC_NODE_MODULES_RE = re.compile(r"(^|/)node_modules/@anthropic-ai/")


def find_anthropic_node_modules(files: Iterable[tuple[str, str]]) -> list[str]:
    """relpaths under any node_modules/@anthropic-ai/ directory."""
    return [rel for rel, _ in files if _ANTHROPIC_NODE_MODULES_RE.search(rel)]


def find_node_modules_dirs(files: Iterable[tuple[str, str]]) -> list[str]:
    """relpaths under any node_modules/ directory at all -- in either source asset (Verdandi's or
    Eitri's), no node_modules/ of any kind may ship (sec 4.2 step 9)."""
    return [rel for rel, _ in files if re.search(r"(^|/)node_modules/", rel)]


def find_verdandi_sidecar_filenames(files: Iterable[tuple[str, str]]) -> list[str]:
    """relpaths whose basename matches `verdandi-claude-sidecar*` (sec 4.2 step 9) -- a second,
    name-based net beside find_sentinel_hits's content-based one."""
    return [rel for rel, _ in files if fnmatch.fnmatch(os.path.basename(rel), "verdandi-claude-sidecar*")]


def find_agent_hook_binaries(files: Iterable[tuple[str, str]]) -> list[str]:
    """relpaths whose basename is EXACTLY 'agent-hook' (M16) -- the compiled legacy-gate relay
    binary, which no release build compiles (D16, no --features anywhere) and no asset may carry.
    A source file such as agent/src/bin/agent-hook.rs, legitimately present in the Eitri source
    asset, has a different basename ('agent-hook.rs') and is not a hit."""
    return [rel for rel, _ in files if os.path.basename(rel) == "agent-hook"]


_ELF_MAGIC = b"\x7fELF"
_PRINTABLE_RUN_RE = re.compile(rb"[\x20-\x7e]{4,}")
# The Task 12 pre-think's M3 ruling: a generic pattern only, checked against extracted ELF files.
# Owner-specific identifiers (a home directory, a checkout layout, an account name) are
# publish/scan.sh's job on the host, not this release-wide one.
_HOME_PATH_RE = re.compile(r"(^|[^A-Za-z0-9_])/(home|Users|root)/[^/\s]+")


def is_elf(path: str) -> bool:
    with open(path, "rb") as f:
        return f.read(4) == _ELF_MAGIC


def strings_a(data: bytes) -> list[str]:
    """A pure-Python `strings -a`: every run of 4 or more printable ASCII bytes, decoded."""
    return [m.decode("ascii") for m in _PRINTABLE_RUN_RE.findall(data)]


def find_home_paths_in_elves(files: Iterable[tuple[str, str]]) -> dict[str, list[str]]:
    """{relpath: [matched substrings]} for every extracted ELF file whose strings contain a
    /home/.../Users/.../root/... path (M3). A non-ELF file carrying the identical text is
    deliberately not reported here -- that is exactly M3's narrowing from the spec's "every
    extracted shipped file" to "every extracted ELF", since a text file (a doc, a licence, this
    project's own install.sh) legitimately mentions such paths in prose and examples."""
    hits: dict[str, list[str]] = {}
    for rel, path in files:
        if not is_elf(path):
            continue
        with open(path, "rb") as f:
            data = f.read()
        found = []
        for s in strings_a(data):
            for m in _HOME_PATH_RE.finditer(s):
                found.append(s[m.start():m.end()])
        if found:
            hits[rel] = found
    return hits


SDK_MENTION_MARKERS = ("claude-agent-sdk", "Anthropic PBC")


def find_sdk_mentions(
    files: Iterable[tuple[str, str]],
    is_allowed: Callable[[str], bool] = lambda rel: False,
) -> list[str]:
    """relpaths whose content mentions the Claude Agent SDK by package name or Anthropic's legal
    name, and are not exempted by `is_allowed(relpath)`. release.sh (Task 4) supplies the concrete
    allowlist per the Task 12 pre-think's M2 ruling: legitimate only in eitri-setup and the
    install.sh asset (which print the SDK line, I2/I2src), and possibly THIRD-PARTY-LICENSES; in
    the Verdandi source asset, its package.json files, package-lock.json, and the sentinel's own
    definition in apps/claude-sidecar/scripts/buildBinary.mjs."""
    hits = []
    for rel, path in files:
        if is_allowed(rel):
            continue
        with open(path, "rb") as f:
            data = f.read()
        if any(marker.encode("ascii") in data for marker in SDK_MENTION_MARKERS):
            hits.append(rel)
    return hits


# --- 7. Tree equality against `git archive` (M2) --------------------------------------------------

class TreeMismatch(ReleaseCheckError):
    def __init__(self, missing: list[str], mismatched: list[str], unexpected: list[str]):
        self.missing = missing
        self.mismatched = mismatched
        self.unexpected = unexpected
        parts = []
        if missing:
            parts.append(f"missing from the extracted tree: {', '.join(missing)}")
        if mismatched:
            parts.append(f"content differs from the archive: {', '.join(mismatched)}")
        if unexpected:
            parts.append(f"present but not in the archive or the allowed additions: {', '.join(unexpected)}")
        super().__init__("; ".join(parts))


def _sha256_file(path: str) -> str:
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 16), b""):
            h.update(chunk)
    return h.hexdigest()


def web_bundle_fingerprint(web_dir: str) -> str:
    """The same fingerprint `shell/build_web.rs`'s `compute_fingerprint` writes to
    `dist/.inputs-sha256` (v1-dist plan Task 6, P4-A1): a sha256 over every file under `src/`
    (depth-first, each directory's own entries sorted first) followed by `package.json`,
    `package-lock.json` and `index.html` if present, then every top-level `vite.config.*`/
    `tsconfig*.json` sorted -- each entry as its path relative to `web_dir` (forward slashes, as
    Rust's own `Path::join`/`strip_prefix` produce on this platform) plus a NUL, its content plus a
    NUL. Must stay byte-for-byte the same algorithm as the Rust side or this check would only prove
    a fingerprint file exists, not that it actually matches the shipped sources."""
    h = hashlib.sha256()

    def add(rel: str, path: str) -> None:
        h.update(rel.encode("utf-8"))
        h.update(b"\0")
        with open(path, "rb") as f:
            h.update(f.read())
        h.update(b"\0")

    def walk_sorted(dir_path: str, prefix: str) -> list[tuple[str, str]]:
        if not os.path.isdir(dir_path):
            return []
        out: list[tuple[str, str]] = []
        for name in sorted(os.listdir(dir_path)):
            path = os.path.join(dir_path, name)
            rel = f"{prefix}/{name}" if prefix else name
            if os.path.isdir(path):
                out.extend(walk_sorted(path, rel))
            else:
                out.append((rel, path))
        return out

    for rel, path in walk_sorted(os.path.join(web_dir, "src"), "src"):
        add(rel, path)
    for name in ("package.json", "package-lock.json", "index.html"):
        path = os.path.join(web_dir, name)
        if os.path.isfile(path):
            add(name, path)
    extra = sorted(
        name for name in os.listdir(web_dir)
        if os.path.isfile(os.path.join(web_dir, name))
        and (name.startswith("vite.config.") or (name.startswith("tsconfig") and name.endswith(".json")))
    )
    for name in extra:
        add(name, os.path.join(web_dir, name))
    return h.hexdigest()


def git_archive_hashes(repo_dir: str, rev: str) -> dict[str, str]:
    """{relpath: sha256} for every regular file `git archive <rev>` of `repo_dir` would produce."""
    proc = subprocess.run(
        ["git", "-C", repo_dir, "archive", "--format=tar", rev],
        check=True, capture_output=True,
    )
    hashes: dict[str, str] = {}
    with tarfile.open(fileobj=io.BytesIO(proc.stdout), mode="r:") as tf:
        for member in tf.getmembers():
            if not member.isfile():
                continue
            extracted = tf.extractfile(member)
            assert extracted is not None
            hashes[member.name] = hashlib.sha256(extracted.read()).hexdigest()
    return hashes


# The known additions the Task 12 pre-think's M2 ruling names for the Eitri source asset: every
# path in the extracted tree not covered by `git archive HEAD` must fall under one of these, or it
# is unexpected.
KNOWN_EITRI_SOURCE_ADDITIONS = (
    "vendor/",
    "skia/",
    "neovide/",
    "proto/",
    ".cargo/config.toml",
    "agent-ui/web/dist/index.html",
    # The fingerprint shell/build.rs's own web-bundle rebuild rule writes beside the bundle
    # (v1-dist plan Task 6, P4-A1) -- shipped alongside it so the source asset's offline rebuild can
    # skip npm by content match rather than an mtime rule.
    "agent-ui/web/dist/.inputs-sha256",
    "THIRD-PARTY-LICENSES",
    "SOURCE",
)


def _is_allowed_addition(relpath: str, allowed_additions: Sequence[str]) -> bool:
    for addition in allowed_additions:
        if addition.endswith("/"):
            prefix = addition.rstrip("/")
            if relpath == prefix or relpath.startswith(addition):
                return True
        elif relpath == addition:
            return True
    return False


def compare_tree_to_git_archive(
    extracted_dir: str,
    repo_dir: str,
    rev: str,
    allowed_additions: Sequence[str] = (),
) -> None:
    """Every path `git archive <rev>` of `repo_dir` produces must exist in `extracted_dir` with an
    identical sha256; every path in `extracted_dir` not in that archive must fall under one of
    `allowed_additions` (a directory prefix ending in '/', or an exact file path) -- otherwise it
    is reported as unexpected. Raises TreeMismatch listing every problem at once."""
    expected = git_archive_hashes(repo_dir, rev)
    actual = {rel: _sha256_file(path) for rel, path in iter_files(extracted_dir)}

    missing = sorted(p for p in expected if p not in actual)
    mismatched = sorted(p for p in expected if p in actual and expected[p] != actual[p])
    unexpected = sorted(
        p for p in actual if p not in expected and not _is_allowed_addition(p, allowed_additions)
    )
    if missing or mismatched or unexpected:
        raise TreeMismatch(missing, mismatched, unexpected)


# --- 8. The scan view publish/scan.sh runs over (M4) -----------------------------------------------

# vendor/ floods scan.sh's email rule with crate authors' own addresses, and skia/ and proto/ are
# third-party archives this project did not author -- all three are integrity-checked by hash
# instead (vendor/ by the offline --locked --offline proof, skia/proto by their pins), so they are
# left out of the leak-scan view entirely rather than scanned and then allowlisted file by file.
SCAN_VIEW_EXCLUDED_TOP_DIRS = ("vendor", "skia", "proto")


def scan_view(root: str, excluded_top_dirs: Sequence[str] = SCAN_VIEW_EXCLUDED_TOP_DIRS) -> list[str]:
    """relpaths under `root`, never descending into the excluded top-level directories at all (a
    same-named directory nested deeper, e.g. src/vendor/, is not excluded -- only the release
    tree's own root-level additions are)."""
    root = os.path.abspath(root)
    out: list[str] = []
    for dirpath, dirnames, filenames in os.walk(root):
        rel_dir = os.path.relpath(dirpath, root)
        if rel_dir == ".":
            dirnames[:] = [d for d in dirnames if d not in excluded_top_dirs]
        for name in filenames:
            relpath = os.path.normpath(os.path.join(rel_dir, name)).replace(os.sep, "/")
            out.append(relpath)
    out.sort()
    return out


# --- 9. Legacy compiled out, by symbol (M9) --------------------------------------------------------

LEGACY_SYMBOL_MARKERS = ("agent::session::AgentSession", "agent::process::AgentProcess")
SIDECAR_SYMBOL_MARKER = "agent::providers::claude_sidecar"


def check_no_legacy_symbols(nm_c_text: str) -> None:
    """`nm_c_text` is `nm -C`'s demangled output for one release binary. Passes only when neither
    legacy marker appears at all AND the sidecar marker appears at least once -- an empty or
    unrelated `nm` output (a stripped binary, or the wrong file) must not pass by vacuously
    finding no legacy symbols; it has to positively show the sidecar backend is linked."""
    present_legacy = [m for m in LEGACY_SYMBOL_MARKERS if m in nm_c_text]
    if present_legacy:
        raise ReleaseCheckError(f"legacy symbol(s) present: {', '.join(present_legacy)}")
    if SIDECAR_SYMBOL_MARKER not in nm_c_text:
        raise ReleaseCheckError(f"no {SIDECAR_SYMBOL_MARKER} symbol found -- the sidecar backend is not linked")


# --- 10. What the release build produced (M15) -----------------------------------------------------
#
# release.sh runs `cargo build --release --locked -p shell -p agent -p supervisor --bins
# --message-format=json-render-diagnostics` and takes the executables from that JSON rather than
# from a listing of a reused target directory, where a binary left behind by an earlier build (an
# `agent-hook` from a development build, say) would otherwise look like part of this one.

RELEASE_BINARIES = ("shell", "eitri-supervisor", "eitri-tmux-shim", "eitri-claude-handoff")


def _json_messages(lines: Iterable[str]) -> list[dict]:
    out = []
    for raw in lines:
        line = raw.strip()
        if line.startswith("{"):
            out.append(json.loads(line))
    return out


def release_executables(lines: Iterable[str]) -> list[str]:
    """The executable paths cargo reported building, in RELEASE_BINARIES order. Exactly those four
    basenames, each once, or ReleaseCheckError naming what differs."""
    execs = [m["executable"] for m in _json_messages(lines)
             if m.get("reason") == "compiler-artifact" and m.get("executable")]
    by_name: dict[str, list[str]] = {}
    for path in execs:
        by_name.setdefault(os.path.basename(path), []).append(path)
    problems = []
    unexpected = sorted(set(by_name) - set(RELEASE_BINARIES))
    missing = [b for b in RELEASE_BINARIES if b not in by_name]
    doubled = sorted(b for b, paths in by_name.items() if len(paths) > 1)
    if unexpected:
        problems.append(f"built executables that no release ships: {', '.join(unexpected)}")
    if missing:
        problems.append(f"did not build: {', '.join(missing)}")
    if doubled:
        problems.append(f"built more than once: {', '.join(doubled)}")
    if problems:
        raise ReleaseCheckError("; ".join(problems))
    return [by_name[b][0] for b in RELEASE_BINARIES]


def build_script_out_dir(lines: Iterable[str], package: str) -> str:
    """The OUT_DIR of `package`'s build script, from its build-script-executed message (both the
    `name@version` and the older `name version (source)` package-id spellings). Exactly one."""
    pid_re = re.compile(rf"(^|[#/ ]){re.escape(package)}[@ ]")
    dirs = sorted({m["out_dir"] for m in _json_messages(lines)
                   if m.get("reason") == "build-script-executed" and pid_re.search(m.get("package_id", ""))})
    if len(dirs) != 1:
        raise ReleaseCheckError(f"expected one build-script-executed OUT_DIR for {package}, found {len(dirs)}")
    return dirs[0]


def check_skia_build_output(output_text: str, expected_url: str) -> None:
    """skia-bindings' build script output (the `output` file beside its OUT_DIR) must show the
    pinned file:// URL as its FROM line and a successful install (pre-think sec 3). A download that
    failed would otherwise fall back to a full Skia build from source; offline that fails loudly,
    and this makes the success explicit rather than inferred."""
    lines = [line.rstrip("\r") for line in output_text.splitlines()]
    froms = [line.strip()[len("FROM: "):] for line in lines if line.strip().startswith("FROM: ")]
    problems = []
    if froms != [expected_url]:
        problems.append(f"FROM line(s) {froms!r}, expected exactly [{expected_url!r}]")
    if "DOWNLOAD AND INSTALL SUCCEEDED" not in lines:
        problems.append("no 'DOWNLOAD AND INSTALL SUCCEEDED' line")
    if any(line.startswith("DOWNLOAD AND INSTALL FAILED") for line in lines):
        problems.append("a 'DOWNLOAD AND INSTALL FAILED' line")
    if problems:
        raise ReleaseCheckError("skia-bindings did not install the pinned archive: " + "; ".join(problems))


# --- 11. The relink recipe, read out of the generated SOURCE (pre-think sec 4) ----------------------
#
# The proof runs the recipe SOURCE prints, never a retyped copy of it, so a SOURCE whose recipe
# stops working fails the release rather than a user.

class RelinkRecipe:
    def __init__(self, copy: str, unlock: str, patch: str, build: str, modified_dir: str):
        self.copy = copy
        self.unlock = unlock
        self.patch = patch
        self.build = build
        self.modified_dir = modified_dir


_RECIPE_HEADING = "Relinking against a MODIFIED nvim-rs"
_STEP_RE = re.compile(r"^    ([1-9])\. (.*)$")


def parse_relink_recipe(source_text: str) -> RelinkRecipe:
    """Steps 1, 2, 4 and 5 of SOURCE's relink section (step 3 is the user's own change). Step 4's
    TOML is the indented block under it, dedented."""
    lines = source_text.splitlines()
    try:
        start = lines.index(_RECIPE_HEADING)
    except ValueError:
        raise ReleaseCheckError(f"SOURCE has no {_RECIPE_HEADING!r} section") from None
    steps: dict[str, list[str]] = {}
    current = None
    for line in lines[start + 1:]:
        m = _STEP_RE.match(line)
        if m:
            current = m.group(1)
            steps[current] = [m.group(2)]
            continue
        if current is None:
            continue
        if line.startswith("           ") and line.strip():
            steps[current].append(line.strip())
            continue
        if not line.strip() and current == "5":
            break
    missing = [n for n in ("1", "2", "4", "5") if n not in steps]
    if missing:
        raise ReleaseCheckError(f"SOURCE's relink recipe has no step(s) {', '.join(missing)}")
    copy, unlock, build = steps["1"][0], steps["2"][0], steps["5"][0]
    patch_lines = steps["4"][1:]
    problems = []
    if not copy.startswith("cp -r vendor/nvim-rs "):
        problems.append(f"step 1 is not a copy of vendor/nvim-rs: {copy!r}")
    modified_dir = copy.split()[-1] if copy.split() else ""
    if unlock != f"rm {modified_dir}/.cargo-checksum.json":
        problems.append(f"step 2 does not remove {modified_dir}'s checksum file: {unlock!r}")
    patch_re = re.compile(rf'^nvim-rs = \{{ path = "{re.escape(modified_dir)}" \}}$')
    if patch_lines[:1] != ["[patch.crates-io]"] or len(patch_lines) != 2 or not patch_re.match(patch_lines[1]):
        problems.append(f"step 4 is not a [patch.crates-io] entry for {modified_dir}: {patch_lines!r}")
    if " cargo build " not in f" {build} " or "--offline" not in build.split() or "--locked" in build.split():
        problems.append(f"step 5 is not an offline, unlocked cargo build: {build!r}")
    if problems:
        raise ReleaseCheckError("; ".join(problems))
    return RelinkRecipe(copy, unlock, "\n".join(patch_lines), build, modified_dir)


REBUILD_ENV_NAMES = ("EITRI_BUILD_COMMIT", "EITRI_BUILD_FORK_COMMIT")
_REBUILD_ENV_RE = re.compile(r"^    (EITRI_BUILD_[A-Z_]+)=(.*)$")


def parse_rebuild_env(source_text: str) -> dict[str, str]:
    """The environment SOURCE tells a rebuild from the source asset to set, so the rebuilt
    `shell --version` names the same Eitri and Neovide fork commits as the shipped one: each of
    REBUILD_ENV_NAMES exactly once, as a full 40-hex commit, nothing else (whole-branch review,
    lane D: before, SOURCE named only the first, and a rebuild said "neovide fork unknown")."""
    found: dict[str, list[str]] = {}
    for line in source_text.splitlines():
        m = _REBUILD_ENV_RE.match(line)
        if m:
            found.setdefault(m.group(1), []).append(m.group(2))
    problems = []
    for name in REBUILD_ENV_NAMES:
        values = found.get(name, [])
        if len(values) != 1:
            problems.append(f"SOURCE names {name} {len(values)} times, not once")
        elif not _HEX40.match(values[0]):
            problems.append(f"SOURCE's {name} is not a full commit: {values[0]!r}")
    problems += [f"SOURCE names an unexpected {name}" for name in sorted(found) if name not in REBUILD_ENV_NAMES]
    if problems:
        raise ReleaseCheckError("; ".join(problems))
    return {name: found[name][0] for name in REBUILD_ENV_NAMES}


def lock_package_sources(lock_text: str, name: str) -> list[str | None]:
    """The `source` of every [[package]] named `name` in a Cargo.lock (None for a path package)."""
    out: list[str | None] = []
    for block in re.split(r"^\[\[package\]\]\s*$", lock_text, flags=re.M)[1:]:
        fields = dict(re.findall(r'^(name|source) = "([^"]*)"\s*$', block, flags=re.M))
        if fields.get("name") == name:
            out.append(fields.get("source"))
    return out


# --- 12. The scan view on disk (M4), for publish/scan.sh -------------------------------------------

# publish/scan.sh skips every directory with one of these names (its SKIP_DIRS, less .git): in a
# checkout they hold build output nobody publishes. In an extracted release asset they hold shipped
# bytes -- the source asset's agent-ui/web/dist/index.html is the one built text file any asset
# carries -- so the view renames them and the scan reads them (Task 4 review).
SCAN_VIEW_RENAMED_DIRS = ("dist", "target", "node_modules")
# publish/scan.sh also skips a top-level neovide/ -- at the top level only -- because in a public
# tree it is the submodule checkout of a separate repository. In the source asset it is the pinned
# fork's source, which this release ships and redistributes, so the view renames it too; M4 leaves
# out only vendor/, skia/ and proto/ (whole-branch review, codex).
SCAN_VIEW_RENAMED_TOP_DIRS = ("neovide",)
SCAN_VIEW_RENAME_SUFFIX = ".scanned"


def scan_view_path(rel: str, renamed_dirs: Sequence[str] = SCAN_VIEW_RENAMED_DIRS,
                   renamed_top_dirs: Sequence[str] = SCAN_VIEW_RENAMED_TOP_DIRS) -> str:
    """`rel` with every directory component in `renamed_dirs`, and a first component in
    `renamed_top_dirs`, given SCAN_VIEW_RENAME_SUFFIX."""
    parts = rel.split("/")
    dirs = [p + SCAN_VIEW_RENAME_SUFFIX if p in renamed_dirs or (i == 0 and p in renamed_top_dirs) else p
            for i, p in enumerate(parts[:-1])]
    return "/".join(dirs + parts[-1:])


def make_scan_view(src_root: str, dest_root: str,
                   excluded_top_dirs: Sequence[str] = SCAN_VIEW_EXCLUDED_TOP_DIRS) -> int:
    """Hard-link (or copy, across filesystems) every scan_view() file of src_root into dest_root,
    which must not exist yet, at scan_view_path(): a directory publish/scan.sh would skip by name is
    renamed, so it is scanned. Symlinks are recreated as symlinks. Returns the file count."""
    if os.path.lexists(dest_root):
        raise ReleaseCheckError(f"{dest_root} already exists")
    count = 0
    for rel in scan_view(src_root, excluded_top_dirs):
        src = os.path.join(src_root, rel)
        dst = os.path.join(dest_root, scan_view_path(rel))
        os.makedirs(os.path.dirname(dst), exist_ok=True)
        if os.path.islink(src):
            os.symlink(os.readlink(src), dst)
        else:
            try:
                os.link(src, dst)
            except OSError:
                shutil.copy2(src, dst)
        count += 1
    os.makedirs(dest_root, exist_ok=True)
    return count


# --- 13. Every asset, extracted, checked as a whole (sec 4.2 step 9, M2, M6) ----------------------

def tarball_top(version: str) -> str:
    return f"eitri-{version}-x86_64-linux"


def source_top(version: str) -> str:
    return f"eitri-{version}-source"


# role -> path, for the tarball (under its top directory) and for the .deb/.rpm (from /). The same
# role must hold the same bytes in all three (M6: one staging dir, packaged three ways).
_TARBALL_ROLES = {
    "launcher": "bin/eitri",
    **{b: f"lib/eitri/{b}" for b in RELEASE_BINARIES},
    "setup": "lib/eitri/eitri-setup",
    "RELEASE": "lib/eitri/RELEASE",
    "desktop": "share/applications/eitri.desktop",
    "LICENSE": "share/licenses/eitri/LICENSE",
    "THIRD-PARTY-LICENSES": "share/licenses/eitri/THIRD-PARTY-LICENSES",
    "SOURCE": "share/licenses/eitri/SOURCE",
}
_PACKAGE_ROLES = {
    "launcher": "usr/bin/eitri",
    **{b: f"usr/lib/eitri/{b}" for b in RELEASE_BINARIES},
    "setup": "usr/lib/eitri/eitri-setup",
    "RELEASE": "usr/lib/eitri/RELEASE",
    "desktop": "usr/share/applications/eitri.desktop",
    "LICENSE": "usr/share/licenses/eitri/LICENSE",
    "THIRD-PARTY-LICENSES": "usr/share/licenses/eitri/THIRD-PARTY-LICENSES",
    "SOURCE": "usr/share/licenses/eitri/SOURCE",
}


def tarball_roles(version: str) -> dict[str, str]:
    top = tarball_top(version)
    return {role: f"{top}/{rel}" for role, rel in _TARBALL_ROLES.items()}


# The .deb alone also carries the AppArmor profile Ubuntu 23.10+'s user-namespace restriction needs
# before WebKit's sandbox can start (packaging/nfpm-public.yaml's `packager: deb` entry;
# docs/superpowers/plans/2026-09-28-v1-dist-ubuntu-userns.md): packaging/apparmor/eitri, byte for
# byte (check_release_assets compares it with the source asset's copy). The .rpm carries none.
_DEB_ONLY_ROLES = {
    "apparmor": "etc/apparmor.d/eitri",
}


def package_roles() -> dict[str, str]:
    """The files both packages ship (the .rpm's whole layout)."""
    return dict(_PACKAGE_ROLES)


def deb_roles() -> dict[str, str]:
    """The .deb's layout: package_roles() plus the AppArmor profile."""
    return {**_PACKAGE_ROLES, **_DEB_ONLY_ROLES}


def _content_problems(label: str, files: list[tuple[str, str]], sdk_allowed: Callable[[str], bool]) -> list[str]:
    problems = []
    for name, hits in (
        ("the SEA fuse sentinel", find_sentinel_hits(files)),
        ("a node_modules/ path", find_node_modules_dirs(files)),
        ("a verdandi-claude-sidecar* file", find_verdandi_sidecar_filenames(files)),
        ("an agent-hook binary", find_agent_hook_binaries(files)),
        ("the SDK's name outside the allowlist", find_sdk_mentions(files, sdk_allowed)),
    ):
        if hits:
            problems.append(f"{label}: {name}: {', '.join(hits)}")
    homes = find_home_paths_in_elves(files)
    if homes:
        problems.append(f"{label}: a home-directory path in an ELF file: "
                        + "; ".join(f"{rel} ({', '.join(sorted(set(v))[:3])})" for rel, v in sorted(homes.items())))
    return problems


def check_binary_asset(label: str, root: str, roles: dict[str, str]) -> list[str]:
    """One extracted tarball/.deb/.rpm: exactly the files `roles` names (M6's layout; anything else
    -- an agent-hook, a sidecar, a stray build product -- is a problem), and the content rules. The
    SDK's name is allowed only in eitri-setup (the installer, which prints the SDK line)."""
    files = iter_files(root)
    present = {rel for rel, _ in files}
    expected = set(roles.values())
    problems = []
    if present - expected:
        problems.append(f"{label}: files no release ships: {', '.join(sorted(present - expected))}")
    if expected - present:
        problems.append(f"{label}: missing: {', '.join(sorted(expected - present))}")
    setup = roles["setup"]
    problems += _content_problems(label, files, lambda rel: rel == setup)
    return problems


def check_same_bytes(named_paths: dict[str, str]) -> list[str]:
    """{label: path}: every path must exist and hold identical bytes."""
    digests = {}
    problems = []
    for label, path in named_paths.items():
        if not os.path.isfile(path):
            problems.append(f"{label}: missing ({path})")
            continue
        digests[label] = _sha256_file(path)
    if len(set(digests.values())) > 1:
        problems.append("not byte-identical: " + ", ".join(f"{k}={v[:12]}" for k, v in sorted(digests.items())))
    return problems


def check_eitri_source_tree(root: str, src_repo: str, verdandi_repo: str, verdandi_rev: str,
                              skia_archive: str, skia_sha256: str) -> list[str]:
    """The extracted Eitri source asset's top directory (M2): `git archive HEAD` of src_repo byte
    for byte plus only the known additions; neovide/ equal to the submodule's own `git archive HEAD`;
    proto/ equal to the public Verdandi's; skia/ exactly the pinned archive; .cargo/config.toml
    pointing at vendor/; the shipped `dist/.inputs-sha256` matches a fresh fingerprint over the
    shipped web sources, the same content-based rule `shell/build_web.rs` uses (v1-dist plan Task 6,
    P4-A1) to decide the offline rebuild can skip npm -- replaces an earlier mtime rule (`M2`'s
    original wording), which a tar extraction or `SOURCE_DATE_EPOCH` normalization could satisfy or
    fail independently of whether the bundle's content ever matched its sources. The content rules
    run over the whole tree for the sentinel and node_modules, and over the additions only for the
    SDK's name (Eitri's own tracked source legitimately names it)."""
    problems = []
    try:
        compare_tree_to_git_archive(root, src_repo, "HEAD", KNOWN_EITRI_SOURCE_ADDITIONS)
    except TreeMismatch as e:
        problems.append(f"source asset vs git archive HEAD: {e}")
    for sub, repo, rev, subpath in (("neovide", os.path.join(src_repo, "neovide"), "HEAD", None),
                                    ("proto", verdandi_repo, verdandi_rev, "proto")):
        subroot = os.path.join(root, sub)
        if not os.path.isdir(subroot):
            problems.append(f"source asset: no {sub}/")
            continue
        expected = git_archive_hashes(repo, rev) if subpath is None else {
            rel[len(subpath) + 1:]: h for rel, h in git_archive_hashes(repo, rev).items()
            if rel.startswith(subpath + "/")}
        actual = {rel: _sha256_file(p) for rel, p in iter_files(subroot)}
        if expected != actual:
            diff = sorted(set(expected.items()) ^ set(actual.items()))
            problems.append(f"source asset {sub}/ differs from git archive {rev}: "
                            f"{', '.join(sorted({rel for rel, _ in diff})[:10])}")
    skia_files = [rel for rel, _ in iter_files(os.path.join(root, "skia"))]
    if skia_files != [skia_archive]:
        problems.append(f"source asset skia/ holds {skia_files}, expected exactly [{skia_archive!r}]")
    elif _sha256_file(os.path.join(root, "skia", skia_archive)) != skia_sha256:
        problems.append(f"source asset skia/{skia_archive} does not match the pinned sha256")
    config = os.path.join(root, ".cargo", "config.toml")
    if not os.path.isfile(config) or 'directory = "vendor"' not in open(config, encoding="utf-8").read():
        problems.append('source asset .cargo/config.toml does not name directory = "vendor"')
    bundle = os.path.join(root, "agent-ui", "web", "dist", "index.html")
    fingerprint_file = os.path.join(root, "agent-ui", "web", "dist", ".inputs-sha256")
    web_dir = os.path.join(root, "agent-ui", "web")
    if not os.path.isfile(bundle):
        problems.append("source asset has no agent-ui/web/dist/index.html")
    if not os.path.isfile(fingerprint_file):
        problems.append("source asset has no agent-ui/web/dist/.inputs-sha256")
    elif os.path.isfile(bundle):
        shipped = open(fingerprint_file, encoding="utf-8").read().strip()
        recomputed = web_bundle_fingerprint(web_dir)
        if shipped != recomputed:
            problems.append(
                "source asset agent-ui/web/dist/.inputs-sha256 does not match a fresh fingerprint of "
                f"its own web sources ({shipped[:12]} shipped vs {recomputed[:12]} recomputed) -- "
                "an offline rebuild from this asset would reach npm")
    files = iter_files(root)
    for name, hits in (("the SEA fuse sentinel", find_sentinel_hits(files)),
                       ("a node_modules/ path", find_node_modules_dirs(files)),
                       ("a verdandi-claude-sidecar* file", find_verdandi_sidecar_filenames(files)),
                       ("an agent-hook binary", find_agent_hook_binaries(files))):
        if hits:
            problems.append(f"source asset: {name}: {', '.join(hits)}")
    tracked = git_archive_hashes(src_repo, "HEAD")
    added = [(rel, p) for rel, p in files if rel not in tracked]
    sdk = find_sdk_mentions(added)
    if sdk:
        problems.append(f"source asset: the SDK's name in an added file: {', '.join(sdk)}")
    homes = find_home_paths_in_elves(files)
    if homes:
        problems.append(f"source asset: a home-directory path in an ELF file: {', '.join(sorted(homes))}")
    return problems


def check_verdandi_source_tree(root: str, verdandi_repo: str, verdandi_rev: str) -> list[str]:
    """The extracted Verdandi asset (M2, and plan Task 10's unpack: no prefix, package.json at the
    root): `git archive <rev>` of the public Verdandi byte for byte, no ELF, no node_modules/, no
    verdandi-claude-sidecar* file, and the sentinel only where buildBinary.mjs defines it. The SDK's
    name is not checked here: tree equality already proves this is the public repo's content."""
    problems = []
    try:
        compare_tree_to_git_archive(root, verdandi_repo, verdandi_rev)
    except TreeMismatch as e:
        problems.append(f"Verdandi asset vs git archive {verdandi_rev}: {e}")
    files = iter_files(root)
    if not os.path.isfile(os.path.join(root, "package.json")):
        problems.append("Verdandi asset has no package.json at its root")
    elves = [rel for rel, p in files if is_elf(p)]
    for name, hits in (("an ELF file", elves),
                       ("a node_modules/ path", find_node_modules_dirs(files)),
                       ("a verdandi-claude-sidecar* file", find_verdandi_sidecar_filenames(files))):
        if hits:
            problems.append(f"Verdandi asset: {name}: {', '.join(hits)}")
    sentinel = [rel for rel in find_sentinel_hits(files) if rel != VERDANDI_SENTINEL_HOME]
    if sentinel:
        problems.append(f"Verdandi asset: the SEA fuse sentinel outside {VERDANDI_SENTINEL_HOME}: {', '.join(sentinel)}")
    return problems


VERDANDI_SENTINEL_HOME = "apps/claude-sidecar/scripts/buildBinary.mjs"
_NODE_VERSION_JS_RE = re.compile(r"^const NODE_VERSION = '(v[0-9]+\.[0-9]+\.[0-9]+)';$", re.M)
_NODE_SHA_JS_RE = re.compile(r"^\s*'(linux-x64|linux-arm64)': '([0-9a-f]{64})',$", re.M)


def check_node_pin(build_binary_text: str, pins: dict[str, str]) -> None:
    """buildBinary.mjs's NODE_VERSION and linux tarball sha256s equal pins.env's (spec sec 4.2 step
    1): the release's RELEASE hands users pins.env's pair, and the sidecar build re-checks against
    buildBinary.mjs's own, so the two must agree."""
    version = _NODE_VERSION_JS_RE.findall(build_binary_text)
    shas = dict(_NODE_SHA_JS_RE.findall(build_binary_text))
    want = {"linux-x64": pins.get("NODE_SHA256_linux_x64"), "linux-arm64": pins.get("NODE_SHA256_linux_arm64")}
    problems = []
    if version != [pins.get("NODE_VERSION")]:
        problems.append(f"buildBinary.mjs pins Node {version}, pins.env {pins.get('NODE_VERSION')!r}")
    for platform, sha in want.items():
        if shas.get(platform) != sha:
            problems.append(f"buildBinary.mjs's {platform} sha256 {shas.get(platform)!r} != pins.env's {sha!r}")
    if problems:
        raise ReleaseCheckError("; ".join(problems))


def check_release_assets(out_dir: str, check_dir: str, version: str, verdandi_rev: str, src_repo: str,
                         verdandi_repo: str, skia_archive: str, skia_sha256: str) -> list[str]:
    """Extract every asset in out_dir into a fresh check_dir with the tools a user would use (sec 4.2
    step 9) and run every check above over the result. Returns the report lines; raises
    ReleaseCheckError listing every problem found."""
    if os.path.lexists(check_dir):
        raise ReleaseCheckError(f"{check_dir} already exists; the checks extract into a fresh directory")
    names = asset_names(version, verdandi_rev[:7])
    tarball, deb, rpm, source, verdandi, install_sh, release = names[:7]
    problems = []
    for name in names[:7]:
        try:
            validate_asset_name(name)
        except ReleaseCheckError as e:
            problems.append(str(e))
        if not os.path.isfile(os.path.join(out_dir, name)):
            problems.append(f"asset missing: {name}")
    if problems:
        raise ReleaseCheckError("; ".join(problems))
    x = {k: os.path.join(check_dir, k) for k in ("tarball", "deb", "rpm", "source", "verdandi")}
    extract_tar_gz(os.path.join(out_dir, tarball), x["tarball"])
    extract_deb(os.path.join(out_dir, deb), x["deb"])
    extract_rpm(os.path.join(out_dir, rpm), x["rpm"])
    extract_tar_gz(os.path.join(out_dir, source), x["source"])
    extract_tar_gz(os.path.join(out_dir, verdandi), x["verdandi"])

    troles, proles = tarball_roles(version), package_roles()
    problems += check_binary_asset(tarball, x["tarball"], troles)
    problems += check_binary_asset(deb, x["deb"], deb_roles())
    problems += check_binary_asset(rpm, x["rpm"], proles)
    for role in troles:
        problems += check_same_bytes({
            f"{tarball}:{troles[role]}": os.path.join(x["tarball"], troles[role]),
            f"{deb}:{proles[role]}": os.path.join(x["deb"], proles[role]),
            f"{rpm}:{proles[role]}": os.path.join(x["rpm"], proles[role]),
        })
    src_root = os.path.join(x["source"], source_top(version))
    top_level = sorted(os.listdir(x["source"]))
    if top_level != [source_top(version)]:
        problems.append(f"{source}: top level is {top_level}, expected [{source_top(version)!r}]")
    else:
        problems += check_eitri_source_tree(src_root, src_repo, verdandi_repo, verdandi_rev,
                                              skia_archive, skia_sha256)
    problems += check_verdandi_source_tree(x["verdandi"], verdandi_repo, verdandi_rev)
    # The .deb's AppArmor profile is the tracked file, not a copy of its own.
    problems += check_same_bytes({
        f"{deb}:{_DEB_ONLY_ROLES['apparmor']}": os.path.join(x["deb"], _DEB_ONLY_ROLES["apparmor"]),
        f"{source}:packaging/apparmor/eitri": os.path.join(src_root, "packaging", "apparmor", "eitri"),
    })
    # The installer, four copies (spec sec 6.4): the asset, the tarball's and both packages'
    # eitri-setup, and the source asset's packaging/install.sh.
    problems += check_same_bytes({
        install_sh: os.path.join(out_dir, install_sh),
        f"{tarball}:eitri-setup": os.path.join(x["tarball"], troles["setup"]),
        f"{deb}:eitri-setup": os.path.join(x["deb"], proles["setup"]),
        f"{source}:packaging/install.sh": os.path.join(src_root, "packaging", "install.sh"),
    })
    problems += check_same_bytes({
        release: os.path.join(out_dir, release),
        f"{tarball}:RELEASE": os.path.join(x["tarball"], troles["RELEASE"]),
    })
    for notice in ("THIRD-PARTY-LICENSES", "SOURCE"):
        problems += check_same_bytes({
            f"{tarball}:{notice}": os.path.join(x["tarball"], troles[notice]),
            f"{source}:{notice}": os.path.join(src_root, notice),
        })
    if problems:
        raise ReleaseCheckError("\n  " + "\n  ".join(problems))
    return [f"extracted and checked: {', '.join(names[:7])}"]


# --- 14. The embedded release-signers block (Task 1, installer-claude-2, spec sec 4.4/6.4) --------
#
# packaging/install.sh's embedded_release_signers() carries packaging/release-signers verbatim
# inside a heredoc, between its own two marker lines (`cat <<'EITRI_RELEASE_SIGNERS'` and a bare
# `EITRI_RELEASE_SIGNERS` line). A hand-kept copy that drifts from the tracked file is exactly
# the defect installer-claude-2 found: release.sh checks `--sign`'s key against release-signers,
# never against what install.sh actually ships, so the two can disagree with no error.

_SIGNERS_HEREDOC_MARKER = "EITRI_RELEASE_SIGNERS"
_SIGNERS_HEREDOC_OPEN = f"<<'{_SIGNERS_HEREDOC_MARKER}'"


def extract_embedded_signers(install_sh_text: str) -> str:
    """The exact text between packaging/install.sh's own two `EITRI_RELEASE_SIGNERS` heredoc
    marker lines (the opening line carries `<<'EITRI_RELEASE_SIGNERS'` -- with whatever heredoc
    command and indentation precede it -- and the closing line is that marker alone, which is what
    a real, non-`<<-` heredoc requires). Raises ReleaseCheckError if the markers are not found, in
    order, exactly one of each."""
    lines = install_sh_text.splitlines(keepends=True)
    start = end = None
    for i, line in enumerate(lines):
        if _SIGNERS_HEREDOC_OPEN in line:
            if start is not None:
                raise ReleaseCheckError(
                    f"packaging/install.sh has more than one {_SIGNERS_HEREDOC_MARKER!r} heredoc opening marker"
                )
            start = i
        elif line.rstrip("\n") == _SIGNERS_HEREDOC_MARKER:
            if start is None:
                raise ReleaseCheckError(
                    f"packaging/install.sh has a {_SIGNERS_HEREDOC_MARKER!r} closing marker with no opening one"
                )
            if end is not None:
                raise ReleaseCheckError(
                    f"packaging/install.sh has more than one {_SIGNERS_HEREDOC_MARKER!r} closing marker"
                )
            end = i
    if start is None:
        raise ReleaseCheckError(
            f"packaging/install.sh has no {_SIGNERS_HEREDOC_MARKER!r} heredoc: embedded_release_signers() "
            "is missing or was renamed"
        )
    if end is None:
        raise ReleaseCheckError(
            f"packaging/install.sh's {_SIGNERS_HEREDOC_MARKER!r} heredoc has an opening marker but no closing one"
        )
    return "".join(lines[start + 1:end])


def signers_key_lines(text: str) -> list[str]:
    """The non-comment, non-blank lines of an allowed_signers-format block (ssh-keygen's format: a
    comment line starts with '#'). Used only to report which key lines each side holds when they
    differ, and to say whether a block holds any key at all."""
    return [line for line in text.splitlines() if line.strip() and not line.lstrip().startswith("#")]


def check_release_signers_agree(install_sh_text: str, signers_text: str) -> None:
    """Task 1: packaging/install.sh's embedded release-signers block must be byte-identical to
    packaging/release-signers, for every version release.sh builds -- an rc's embedded block may
    still be empty (it signs with a throwaway --release-signers file instead), as long as the
    tracked file agrees. Raises ReleaseCheckError naming the key lines each side holds when they
    differ, so the drift is fixed by looking at the message rather than re-deriving it."""
    embedded = extract_embedded_signers(install_sh_text)
    if embedded == signers_text:
        return
    embedded_keys = signers_key_lines(embedded)
    file_keys = signers_key_lines(signers_text)
    raise ReleaseCheckError(
        "packaging/install.sh's embedded release-signers block and packaging/release-signers differ "
        "(a hand-kept copy that drifts is the defect, spec sec 4.4): "
        f"install.sh holds {embedded_keys or '[no key line]'}; "
        f"release-signers holds {file_keys or '[no key line]'}. Keep them byte-identical."
    )


# Task 5 (v1-dist plan lane D, docs/superpowers/plans/2026-09-28-v1-dist-task17-18.md): the
# required English/Chinese doc pairs -- every other X.zh-CN.md found at any depth under the tree
# (recursion added in Task 5's own fix round 1) is paired with the X.md its own name implies, in
# the same directory (check_twins, below).
TWIN_REQUIRED_PAIRS: tuple[tuple[str, str], ...] = (
    ("README.md", "README.zh-CN.md"),
    ("INSTALL.md", "INSTALL.zh-CN.md"),
)
_TWIN_ZH_SUFFIX = ".zh-CN.md"
_TWIN_MARKER_RE = re.compile(r"^<!-- translated-from: (?P<name>\S+) sha256=(?P<hash>[0-9a-f]{64}) -->$")


def _twin_switch_line(zh_name: str) -> str:
    """The exact first line an English original carries (Decisions, Task 5's plan section):
    'English | [简体中文](<its twin's filename>)'."""
    return f"English | [简体中文]({zh_name})"


def check_twins(root: str) -> None:
    """Every Chinese guide doc anywhere under `root` must still match the English original it was
    translated from (Task 5). README.md/README.zh-CN.md and INSTALL.md/INSTALL.zh-CN.md
    (TWIN_REQUIRED_PAIRS) are required pairs at `root`'s own top level; every other
    `<name>.zh-CN.md` file found **at any depth under `root`** is paired with the `<name>.md` in
    the SAME directory -- its adjacent English original -- the same way. A file with no twin at
    all (e.g. CONTRIBUTING.md, English-only by design) is never checked.

    Recurses (fix round 1, a [codex] finding, verified real): `publish/export.sh` mirrors
    `publish/files/` into the exported tree path for path ("Files that exist only in the public
    tree", export.sh), so a future guide nested under a subdirectory (e.g.
    `docs/USAGE.md`/`docs/USAGE.zh-CN.md`) would otherwise never be discovered -- this used to
    `os.listdir` only `root` itself, and the owner's own language decision ("any install/usage
    guide likewise") does not stop at one directory level. `.git` directories are skipped (never a
    source of guides, and can be large).

    Raises ReleaseCheckError, naming every offending file relative to `root` (and, for a stale
    hash, both hashes), collecting every problem found rather than stopping at the first one:
      - a required pair's English file is missing, or its twin is missing, or both are missing
        (each gets its own message: a lone twin naming a missing English file reads differently
        from neither existing at all -- fix round 1, a plain finding, verified real);
      - a discovered `<name>.zh-CN.md` has no `<name>.md` beside it (a missing English file);
      - a twin's second line is not a `<!-- translated-from: NAME sha256=HASH -->` marker;
      - that marker names a different English file than the one it sits beside;
      - the marker's hash no longer matches the English file's current sha256 (a stale
        translation -- the hash covers the whole English file, its own switch line included,
        matching `sha256sum`);
      - the English original's own first line is not its switch line pointing at that twin."""
    if not os.path.isdir(root):
        raise ReleaseCheckError(f"twins: {root} is not a directory")

    problems: list[str] = []

    def check_pair(directory: str, english: str, zh: str, required: bool) -> None:
        english_path = os.path.join(directory, english)
        zh_path = os.path.join(directory, zh)
        english_rel = os.path.relpath(english_path, root)
        zh_rel = os.path.relpath(zh_path, root)
        english_exists = os.path.isfile(english_path)
        zh_exists = os.path.isfile(zh_path)

        if not english_exists:
            if zh_exists:
                problems.append(f"{zh_rel}: no {english_rel} beside it (a missing English file)")
            else:
                problems.append(f"{english_rel}: neither it nor its twin {zh_rel} exists")
            return
        if not zh_exists:
            if required:
                problems.append(f"{english_rel}: no {zh_rel} (a missing twin of a required pair)")
            return

        english_text = _read(english_path)
        english_lines = english_text.splitlines()
        expected_switch = _twin_switch_line(zh)
        if not english_lines or english_lines[0] != expected_switch:
            problems.append(f"{english_rel}: its first line is not the switch line {expected_switch!r}")

        zh_lines = _read(zh_path).splitlines()
        marker_line = zh_lines[1].strip() if len(zh_lines) > 1 else ""
        m = _TWIN_MARKER_RE.match(marker_line)
        if m is None:
            problems.append(f"{zh_rel}: its second line is not a translated-from marker (got {marker_line!r})")
            return
        if m.group("name") != english:
            problems.append(f"{zh_rel}: its marker names {m.group('name')!r}, not {english!r}")
            return
        actual_hash = _sha256_file(english_path)
        marker_hash = m.group("hash")
        if marker_hash != actual_hash:
            problems.append(
                f"{zh_rel}: stale -- its marker sha256={marker_hash} no longer matches {english_rel}'s "
                f"current sha256={actual_hash}"
            )

    for english, zh in TWIN_REQUIRED_PAIRS:
        check_pair(root, english, zh, required=True)

    top_level_required_zh = {zh for _, zh in TWIN_REQUIRED_PAIRS}
    discovered: list[tuple[str, str]] = []  # (directory, zh filename), one entry per discovered twin
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames[:] = sorted(d for d in dirnames if d != ".git")
        for name in sorted(filenames):
            if not name.endswith(_TWIN_ZH_SUFFIX):
                continue
            if dirpath == root and name in top_level_required_zh:
                continue  # already handled by the required-pairs loop above
            discovered.append((dirpath, name))

    for directory, zh in discovered:
        stem = zh[: -len(_TWIN_ZH_SUFFIX)]
        check_pair(directory, f"{stem}.md", zh, required=False)

    if problems:
        raise ReleaseCheckError("release_check twins: " + "; ".join(problems))


# --- CLI ---------------------------------------------------------------------------------------

def _read(path: str) -> str:
    with open(path, encoding="utf-8") as f:
        return f.read()


def _read_exact(path: str) -> str:
    """Like _read, but with universal-newline translation off (fix round 1, minor finding): a
    CRLF copy of either file must not compare equal to an LF one just because Python's default
    text mode silently rewrites '\\r\\n' to '\\n' on read. check_release_signers_agree's docstring
    and its own error message both promise a byte-identical comparison; plain _read did not keep
    that promise."""
    with open(path, encoding="utf-8", newline="") as f:
        return f.read()


def main(argv: list[str]) -> int:
    if not argv:
        print("release_check: no subcommand (see the module docstring)", file=sys.stderr)
        return 2
    cmd, rest = argv[0], argv[1:]
    try:
        if cmd == "validate-release-file":
            fields = validate_release_text(_read(rest[0]), rehearsal="--rehearsal" in rest[1:])
            print(f"release_check: {rest[0]} is a valid RELEASE ({len(fields)} fields)", file=sys.stderr)
            return 0
        if cmd == "asset-names":
            for name in asset_names(rest[0], rest[1]):
                print(name)
            return 0
        if cmd == "gh-release-command":
            version = rest[0]
            args = rest[1:]
            title = notes_file = repo = None
            assets = []
            i = 0
            while i < len(args):
                if args[i] == "--title":
                    title, i = args[i + 1], i + 2
                elif args[i] == "--notes-file":
                    notes_file, i = args[i + 1], i + 2
                elif args[i] == "--repo":
                    repo, i = args[i + 1], i + 2
                else:
                    assets.append(args[i])
                    i += 1
            if title is None or notes_file is None or repo is None:
                print("release_check: gh-release-command needs --repo, --title and --notes-file", file=sys.stderr)
                return 2
            print(shlex.join(gh_release_command(version, assets, title, notes_file, repo)))
            return 0
        if cmd == "check-legacy-symbols":
            check_no_legacy_symbols(_read(rest[0]))
            print("release_check: no legacy symbols, sidecar backend linked", file=sys.stderr)
            return 0
        if cmd == "tree-equal":
            extracted_dir, repo_dir, rev = rest[0], rest[1], rest[2]
            allowed = []
            i = 3
            while i < len(rest):
                if rest[i] == "--allow":
                    allowed.append(rest[i + 1])
                    i += 2
                else:
                    i += 1
            compare_tree_to_git_archive(extracted_dir, repo_dir, rev, allowed)
            print(f"release_check: {extracted_dir} matches `git archive {rev}` of {repo_dir}", file=sys.stderr)
            return 0
        if cmd == "build-executables":
            for path in release_executables(_read(rest[0]).splitlines()):
                print(path)
            return 0
        if cmd == "build-script-out-dir":
            print(build_script_out_dir(_read(rest[0]).splitlines(), rest[1]))
            return 0
        if cmd == "check-skia-output":
            check_skia_build_output(_read(rest[0]), rest[1])
            print(f"release_check: skia-bindings installed {rest[1]}", file=sys.stderr)
            return 0
        if cmd == "relink-recipe":
            recipe = parse_relink_recipe(_read(rest[0]))
            print(f"MODIFIED_DIR={recipe.modified_dir}")
            print(f"COPY={recipe.copy}")
            print(f"UNLOCK={recipe.unlock}")
            for line in recipe.patch.splitlines():
                print(f"PATCH={line}")
            print(f"BUILD={recipe.build}")
            return 0
        if cmd == "rebuild-env":
            for name, value in parse_rebuild_env(_read(rest[0])).items():
                print(f"{name}={value}")
            return 0
        if cmd == "lock-package-sources":
            sources = lock_package_sources(_read(rest[0]), rest[1])
            if not sources:
                raise ReleaseCheckError(f"{rest[0]} has no package named {rest[1]}")
            for source in sources:
                print(source if source is not None else "<path>")
            return 0
        if cmd == "make-scan-view":
            count = make_scan_view(rest[0], rest[1])
            print(f"release_check: {count} files linked into {rest[1]}", file=sys.stderr)
            return 0
        if cmd == "check-node-pin":
            check_node_pin(_read(rest[0]), parse_release_text(_read(rest[1])))
            print("release_check: buildBinary.mjs pins the same Node as pins.env", file=sys.stderr)
            return 0
        if cmd == "check-assets":
            for line in check_release_assets(*rest[:8]):
                print(f"release_check: {line}", file=sys.stderr)
            return 0
        if cmd == "check-release-signers":
            check_release_signers_agree(_read_exact(rest[0]), _read_exact(rest[1]))
            print("release_check: packaging/install.sh's embedded release-signers block matches "
                  "packaging/release-signers", file=sys.stderr)
            return 0
        if cmd == "twins":
            check_twins(rest[0])
            print(f"release_check: {rest[0]}'s Chinese twins are fresh", file=sys.stderr)
            return 0
        print(f"release_check: unknown subcommand {cmd!r}", file=sys.stderr)
        return 2
    except (ReleaseCheckError, OSError, subprocess.CalledProcessError, IndexError, TypeError,
            json.JSONDecodeError) as e:
        print(f"release_check: FAILED: {e}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
