"""Tests for the public docs under publish/files/ -- or, in the public tree this file also ships in, at the root, where publish/export.sh
copies them (public_docs_dir(); `PublicTreeLayoutTests` runs this whole suite in that layout): every
documented `curl ... install.sh | sh` command must carry the same HTTPS-safety flags
packaging/install.sh's own net_get() function uses for the identical download (a GitHub
`releases/latest/download/` URL answers with a 302, and without `-L` (or `--proto-redir`) `curl -sSf | sh` silently pipes nothing to
`sh` and exits 0 -- no install happens and no error is shown).

Also: INSTALL.md's "verify before running" recipe must actually hash the downloaded
`install.sh` against the verified `SHA256SUMS`, not just check `SHA256SUMS`'s own signature (checking the manifest's signature
proves the manifest is authentic, never that the script you are about to run matches it -- a release
host or mirror that swaps `install.sh` while leaving `SHA256SUMS`/`.sig` alone passes every step of
the documented recipe otherwise). `VerifyRecipeRefusesTamperedInstallShTests` proves this by actually
running the recipe, extracted verbatim out of INSTALL.md, against a throwaway ssh key and a tiny
self-signed `SHA256SUMS` this test builds itself in pytest's own `tmp_path` -- never against
a real release, so it ships publicly and runs for any contributor with
`ssh-keygen` on PATH (skipped, not failed, if it is absent). The same class also covers a tampered
`SHA256SUMS` (edited after it was signed) and a missing `SHA256SUMS.sig`, not only a tampered
`install.sh`: dropping or weakening the signature step's own `exit 1` fails one of these, not only
the install.sh-tampering test.

    python3 -m pytest packaging/tests/test_public_docs.py -q

(`cd packaging/tests && python3 -m unittest test_public_docs` runs the same tests without pytest's
runner; pytest itself must still be importable.)

No container, no network: `VerifyRecipeRefusesTamperedInstallShTests` and `PublicTreeLayoutTests`
build their fixtures under pytest's own `tmp_path` fixture (an autouse fixture, since they are
`unittest.TestCase`s; pytest puts it under `$TMPDIR`, else `/tmp`, unless `--basetemp` is given), or,
under plain unittest, in a fresh directory under `$XDG_CACHE_HOME/eitri-public-docs-tests`
(default `~/.cache/eitri-public-docs-tests`) removed after
each test -- never a bare `tempfile.mkdtemp()`. The required flags are parsed out of the real
packaging/install.sh -- never hardcoded a second time here -- so a future change to net_get()'s own
flags is what this test tracks, not a copy that could drift from it. The "catches a regression" tests
below construct their own in-memory command lines; they do not mutate any tracked file.
"""

import hashlib
import importlib.util
import os
import re
import shlex
import shutil
import subprocess
import sys
import tempfile
import unittest
import unicodedata
from urllib.parse import unquote, urlsplit

import pytest

_HERE = os.path.dirname(os.path.abspath(__file__))
_SCRATCH_ROOT = os.path.join(os.environ.get("XDG_CACHE_HOME") or os.path.expanduser("~/.cache"),
                             "eitri-public-docs-tests")
_PACKAGING = os.path.dirname(_HERE)
_REPO_ROOT = os.path.dirname(_PACKAGING)
_INSTALL_SH = os.path.join(_PACKAGING, "install.sh")

_spec = importlib.util.spec_from_file_location("release_check", os.path.join(_PACKAGING, "release_check.py"))
rc = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(rc)


def public_docs_dir(repo_root):
    """Where the public docs are in the tree this file runs in. In the private tree they are
    publish/files/; the public tree has no publish/ at all (publish/manifest.txt's `exclude
    publish/*`) because publish/export.sh copies publish/files/ over the tree's root, path for path,
    so there they are at the root. Only a tree with no publish/ directory counts as the public one: a
    private tree that lost publish/files/ still points there and fails loudly, rather than quietly
    checking the private root's own README.md instead."""
    private = os.path.join(repo_root, "publish", "files")
    if os.path.isdir(private) or os.path.lexists(os.path.join(repo_root, "publish")):
        return private
    return repo_root


_PUBLISH_FILES = public_docs_dir(_REPO_ROOT)
_INSTALL_MD = os.path.join(_PUBLISH_FILES, "INSTALL.md")

_VERIFY_RECIPE_START = "<!-- verify-recipe:start -->"
_VERIFY_RECIPE_END = "<!-- verify-recipe:end -->"

_README_ONE_LINER = (
    "curl --proto '=https' --proto-redir '=https' --tlsv1.2 -sSfL "
    "https://github.com/HunterGrey-cyber/eitri/releases/latest/download/install.sh | sh"
)

# A line documenting the installer, in a public doc: curl ... install.sh ... | sh [-s -- ...].
_CURL_INSTALL_LINE_RE = re.compile(r"^\s*curl\b.*\binstall\.sh\b.*\|\s*sh\b")


def _fetch_https_branch(install_sh_text):
    """The body of packaging/install.sh's net_get() function (every download goes through it), and
    within it the first https curl invocation -- the one used for every non-loopback (real)
    download, i.e. every URL a public doc also tells a user to fetch by hand."""
    m = re.search(r"\nnet_get\(\) \{\n(.*?)\n\}\n", install_sh_text, re.DOTALL)
    if not m:
        raise AssertionError("packaging/install.sh: could not find a net_get() function to read flags from")
    body = m.group(1)
    https_line = None
    for line in body.splitlines():
        stripped = line.strip()
        if stripped.startswith("curl") and "'=https'" in stripped:
            https_line = stripped
            break
    if https_line is None:
        raise AssertionError("packaging/install.sh: net_get() has no https curl invocation to read flags from")
    # Drop the trailing `|| printf ...`: only the curl invocation itself is parsed.
    return https_line.split(" || ", 1)[0].strip()


def curl_flags(curl_command_text):
    """Parse a `curl ...` shell command (optionally followed by `| sh ...`) into the three things
    this suite cares about: the value passed to --proto, the value passed to --proto-redir, and
    whether any short-option cluster (e.g. -sSfL) carries the -L (follow redirects) flag. Returns a
    dict; a flag that never appears is None (proto/proto_redir) or False (has_follow_redirects)."""
    curl_part = curl_command_text.split("|", 1)[0].strip()
    tokens = shlex.split(curl_part)
    if not tokens or tokens[0] != "curl":
        raise AssertionError(f"not a curl command: {curl_command_text!r}")
    result = {"proto": None, "proto_redir": None, "has_follow_redirects": False}
    i = 1
    while i < len(tokens):
        tok = tokens[i]
        if tok == "--proto" and i + 1 < len(tokens):
            result["proto"] = tokens[i + 1]
            i += 2
            continue
        if tok == "--proto-redir" and i + 1 < len(tokens):
            result["proto_redir"] = tokens[i + 1]
            i += 2
            continue
        if tok == "-L":
            result["has_follow_redirects"] = True
        elif tok.startswith("-") and not tok.startswith("--") and "L" in tok[1:]:
            # a bundled short-option cluster such as -sSfL or -fsSL
            result["has_follow_redirects"] = True
        i += 1
    return result


def required_fetch_flags():
    """The flags this test requires of every documented install one-liner, read live from
    packaging/install.sh's own net_get() -- not a second, hand-kept copy."""
    with open(_INSTALL_SH, encoding="utf-8") as f:
        install_sh_text = f.read()
    https_curl = _fetch_https_branch(install_sh_text)
    flags = curl_flags(https_curl)
    if flags["proto"] is None or flags["proto_redir"] is None or not flags["has_follow_redirects"]:
        raise AssertionError(
            f"packaging/install.sh's own net_get() no longer sets --proto/--proto-redir/-L "
            f"(parsed {flags!r} from {https_curl!r}); this test's own baseline changed, look there first"
        )
    return flags


def missing_flags(curl_command_text, required):
    """Which of `required`'s flags (as returned by required_fetch_flags()) `curl_command_text` is
    missing, as a list of human-readable names -- empty when the command is fully safe."""
    got = curl_flags(curl_command_text)
    missing = []
    if got["proto"] != required["proto"]:
        missing.append(f"--proto {required['proto']!r}")
    if got["proto_redir"] != required["proto_redir"]:
        missing.append(f"--proto-redir {required['proto_redir']!r}")
    if required["has_follow_redirects"] and not got["has_follow_redirects"]:
        missing.append("-L (follow redirects)")
    return missing


def find_curl_install_lines(doc_text):
    """Every line in a public doc that documents piping install.sh into sh, in source order."""
    return [line for line in doc_text.splitlines() if _CURL_INSTALL_LINE_RE.match(line.strip())]


def public_doc_paths():
    if not os.path.isdir(_PUBLISH_FILES):
        raise AssertionError(f"the public docs directory was not found at {_PUBLISH_FILES}")
    paths = sorted(
        os.path.join(_PUBLISH_FILES, name) for name in os.listdir(_PUBLISH_FILES) if name.endswith(".md")
    )
    if not paths:
        raise AssertionError(f"no .md files found under {_PUBLISH_FILES}")
    return paths


class FetchFlagsFromInstallShTests(unittest.TestCase):
    """required_fetch_flags() reads packaging/install.sh's own net_get(), rather than a hardcoded copy."""

    def test_the_https_branch_sets_proto_proto_redir_and_follow_redirects(self):
        required = required_fetch_flags()
        self.assertEqual(required["proto"], "=https")
        self.assertEqual(required["proto_redir"], "=https")
        self.assertTrue(required["has_follow_redirects"])

    def test_the_loopback_branch_is_not_what_this_test_reads(self):
        # The loopback branch (a loopback http URL) uses --proto '=http' with no --proto-redir/-L -- it is
        # a local-testing-only path and must never be mistaken for the public download's requirements.
        with open(_INSTALL_SH, encoding="utf-8") as f:
            install_sh_text = f.read()
        https_curl = _fetch_https_branch(install_sh_text)
        self.assertNotIn("'=http'", https_curl)
        self.assertIn("'=https'", https_curl)


class CurlFlagsParsingTests(unittest.TestCase):
    """curl_flags()/missing_flags() themselves, independent of install.sh or any doc -- these are the
    functions the acceptance criterion ("fails on a README without -L") is proven against."""

    def setUp(self):
        self.required = {"proto": "=https", "proto_redir": "=https", "has_follow_redirects": True}

    def test_a_fully_safe_line_has_nothing_missing(self):
        line = (
            "curl --proto '=https' --proto-redir '=https' --tlsv1.2 -sSfL "
            "https://example.invalid/releases/latest/download/install.sh | sh"
        )
        self.assertEqual(missing_flags(line, self.required), [])

    def test_a_line_missing_dash_L_is_caught(self):
        # The exact regression found in review: -sSf with no L, so a 302 redirect pipes an empty
        # body to `sh` -- silently installs nothing, exit 0.
        line = (
            "curl --proto '=https' --proto-redir '=https' --tlsv1.2 -sSf "
            "https://example.invalid/releases/latest/download/install.sh | sh"
        )
        missing = missing_flags(line, self.required)
        self.assertIn("-L (follow redirects)", missing)

    def test_a_line_missing_proto_redir_is_caught(self):
        line = "curl --proto '=https' --tlsv1.2 -sSfL https://example.invalid/install.sh | sh"
        missing = missing_flags(line, self.required)
        self.assertTrue(any("--proto-redir" in m for m in missing))

    def test_a_line_with_bare_dash_capital_L_is_recognised_too(self):
        line = "curl --proto '=https' --proto-redir '=https' -sSf -L https://example.invalid/install.sh | sh"
        self.assertEqual(missing_flags(line, self.required), [])

    def test_lowercase_l_does_not_count_as_follow_redirects(self):
        # -l is a different curl flag (list-only FTP); this must not be mistaken for -L.
        line = "curl --proto '=https' --proto-redir '=https' -sSfl https://example.invalid/install.sh | sh"
        missing = missing_flags(line, self.required)
        self.assertIn("-L (follow redirects)", missing)


class FindCurlInstallLinesTests(unittest.TestCase):
    def test_finds_a_fenced_one_liner_and_ignores_unrelated_lines(self):
        doc = "\n".join(
            [
                "# eitri",
                "",
                "Some prose about install.sh, not a command.",
                "```sh",
                _README_ONE_LINER,
                "```",
                "",
                "curl -fsSL https://example.invalid/unrelated.sh | sh",
            ]
        )
        found = find_curl_install_lines(doc)
        self.assertEqual(found, [_README_ONE_LINER])


class PublicDocsCurlLinesTests(unittest.TestCase):
    """The real check: every curl ... install.sh | sh line in every public doc (publish/files/ here,
    the root in the public tree) carries the flags packaging/install.sh's own net_get() requires."""

    def test_every_public_doc_curl_install_line_has_the_required_flags(self):
        required = required_fetch_flags()
        checked_at_least_one = False
        for path in public_doc_paths():
            with open(path, encoding="utf-8") as f:
                text = f.read()
            for line in find_curl_install_lines(text):
                checked_at_least_one = True
                missing = missing_flags(line, required)
                self.assertEqual(
                    missing,
                    [],
                    f"{os.path.relpath(path, _REPO_ROOT)}: curl line missing {missing}: {line!r}",
                )
        self.assertTrue(
            checked_at_least_one,
            f"no `curl ... install.sh | sh` line was found under {_PUBLISH_FILES} -- "
            "this test would otherwise pass vacuously",
        )

    def test_the_readme_one_liner_is_byte_exact(self):
        readme = os.path.join(_PUBLISH_FILES, "README.md")
        with open(readme, encoding="utf-8") as f:
            text = f.read()
        lines = find_curl_install_lines(text)
        self.assertIn(_README_ONE_LINER, lines, "README.md's curl one-liner does not match the exact literal")


def extract_verify_recipe(doc_text):
    """INSTALL.md's "verify before running" recipe: the text of the fenced ```sh code
    block between the <!-- verify-recipe:start/end --> marker comments. Raises AssertionError, never
    returns something wrong, if the markers or the fenced block are missing -- a doc edit that drops
    or renames them fails a test here rather than silently stopping being checked at all."""
    try:
        start = doc_text.index(_VERIFY_RECIPE_START) + len(_VERIFY_RECIPE_START)
        end = doc_text.index(_VERIFY_RECIPE_END, start)
    except ValueError as exc:
        raise AssertionError(
            f"INSTALL.md: could not find {_VERIFY_RECIPE_START!r} .. {_VERIFY_RECIPE_END!r}"
        ) from exc
    span = doc_text[start:end]
    m = re.search(r"```sh\n(.*?)```", span, re.DOTALL)
    if not m:
        raise AssertionError("INSTALL.md: the verify-recipe marker span holds no ```sh fenced code block")
    return m.group(1)


def _sha256_file(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        h.update(f.read())
    return h.hexdigest()


class VerifyRecipeHashStepTests(unittest.TestCase):
    """INSTALL.md's "verify before running" recipe must hash the *downloaded* install.sh against
    the verified SHA256SUMS -- checking SHA256SUMS's own signature alone proves
    SHA256SUMS is authentic, never that the install.sh sitting on disk matches it."""

    def setUp(self):
        with open(_INSTALL_MD, encoding="utf-8") as f:
            self.doc_text = f.read()
        self.recipe = extract_verify_recipe(self.doc_text)

    def test_the_recipe_hashes_install_sh_against_the_verified_sums(self):
        self.assertIn("sha256sum install.sh", self.recipe)
        self.assertIn('$2=="install.sh"', self.recipe)
        self.assertIn("SHA256SUMS", self.recipe)

    def test_the_hash_step_runs_after_the_signature_check_and_before_running_it(self):
        sig_pos = self.recipe.index("ssh-keygen -Y verify")
        hash_pos = self.recipe.index("sha256sum install.sh")
        run_pos = self.recipe.rindex("sh install.sh")
        self.assertLess(sig_pos, hash_pos, "the signature check must come before the hash check")
        self.assertLess(hash_pos, run_pos, "the hash check must come before install.sh is run")

    def test_the_recipe_also_verifies_the_signature(self):
        self.assertIn("ssh-keygen -Y verify", self.recipe)
        self.assertIn("release@eitri", self.recipe)
        self.assertIn("eitri-release", self.recipe)


class VerifyRecipeRefusesTamperedInstallShTests(unittest.TestCase):
    """The recipe extracted verbatim from INSTALL.md, run for real against a throwaway ssh key and
    a tiny self-signed SHA256SUMS this test builds itself in a scratch directory (never against
    a build machine's own files or the real rc.1 release -- this suite ships publicly and must run for any
    contributor). It must run install.sh when everything matches, and refuse -- never running
    install.sh -- when install.sh's bytes were swapped after signing while SHA256SUMS/.sig were
    left alone: exactly the attack the recipe must stop (a compromised release host or mirror)."""

    @classmethod
    def setUpClass(cls):
        if shutil.which("ssh-keygen") is None:
            raise unittest.SkipTest("ssh-keygen not on PATH")
        with open(_INSTALL_MD, encoding="utf-8") as f:
            cls.recipe = extract_verify_recipe(f.read())

    # Under pytest (the documented runner) the scratch directory is pytest's own tmp_path, handed
    # over by this autouse fixture -- pytest's documented pattern for a unittest.TestCase, and it
    # runs before setUp(). Plain unittest (`python3 -m unittest test_public_docs`, or this file's
    # own unittest.main()) runs no pytest fixture, so setUp() falls back to a fresh directory under
    # _SCRATCH_ROOT, the same ~/.cache convention this directory's other suites use. tmp_path itself
    # lives under $TMPDIR (else /tmp) unless pytest is given --basetemp.
    @pytest.fixture(autouse=True)
    def _tmp_path_fixture(self, tmp_path):
        self.tmp = str(tmp_path)

    def setUp(self):
        if getattr(self, "tmp", None) is None:
            os.makedirs(_SCRATCH_ROOT, exist_ok=True)
            self.tmp = tempfile.mkdtemp(dir=_SCRATCH_ROOT)
            self.addCleanup(shutil.rmtree, self.tmp, ignore_errors=True)
        self._make_fixture()

    def _make_fixture(self):
        key_path = os.path.join(self.tmp, "key")
        subprocess.run(
            ["ssh-keygen", "-t", "ed25519", "-f", key_path, "-N", "", "-q"],
            check=True,
            capture_output=True,
        )
        with open(key_path + ".pub", encoding="utf-8") as f:
            key_type, key_data = f.read().split()[:2]
        signers_path = os.path.join(self.tmp, "release-signers")
        with open(signers_path, "w", encoding="utf-8") as f:
            f.write(f'release@eitri namespaces="eitri-release" {key_type} {key_data}\n')

        install_sh = os.path.join(self.tmp, "install.sh")
        with open(install_sh, "w", encoding="utf-8") as f:
            f.write("#!/bin/sh\necho REAL_INSTALLER_RAN\n")
        os.chmod(install_sh, 0o755)

        sums_path = os.path.join(self.tmp, "SHA256SUMS")
        with open(sums_path, "w", encoding="utf-8") as f:
            f.write(f"{_sha256_file(install_sh)}  install.sh\n")

        subprocess.run(
            ["ssh-keygen", "-Y", "sign", "-f", key_path, "-n", "eitri-release", sums_path],
            check=True,
            capture_output=True,
        )
        # ssh-keygen -Y sign writes <sums_path>.sig, i.e. exactly SHA256SUMS.sig beside it (proven
        # against an absolute path, not just a relative one, before relying on it here).
        self.assertTrue(os.path.isfile(sums_path + ".sig"), "ssh-keygen -Y sign did not write SHA256SUMS.sig")

    def _run_recipe(self):
        return subprocess.run(
            ["sh", "-c", self.recipe],
            cwd=self.tmp,
            capture_output=True,
            text=True,
        )

    def test_a_clean_fixture_runs_install_sh(self):
        result = self._run_recipe()
        self.assertEqual(result.returncode, 0, f"stdout={result.stdout!r} stderr={result.stderr!r}")
        self.assertIn("REAL_INSTALLER_RAN", result.stdout)

    def test_a_tampered_install_sh_is_refused_and_never_run(self):
        # SHA256SUMS/.sig are left exactly as signed; only install.sh's bytes change underneath
        # them, the way a compromised release host or mirror would do it.
        install_sh = os.path.join(self.tmp, "install.sh")
        with open(install_sh, "w", encoding="utf-8") as f:
            f.write("#!/bin/sh\necho TAMPERED_RAN\n")
        os.chmod(install_sh, 0o755)

        result = self._run_recipe()
        self.assertNotEqual(result.returncode, 0, "the recipe must refuse a tampered install.sh, not exit 0")
        self.assertNotIn("TAMPERED_RAN", result.stdout, "the tampered install.sh must never actually run")
        self.assertIn("does not match the verified SHA256SUMS", result.stdout + result.stderr)

    def test_a_tampered_sha256sums_is_refused_and_never_run(self):
        # install.sh and SHA256SUMS.sig are left exactly as signed; only SHA256SUMS's own bytes
        # change underneath its signature -- an attacker (or a corrupted mirror) that edits the
        # manifest but cannot re-sign it. The signature step must catch this before the recipe
        # ever reaches the hash-comparison step, let alone install.sh.
        sums_path = os.path.join(self.tmp, "SHA256SUMS")
        with open(sums_path, "a", encoding="utf-8") as f:
            f.write("0000000000000000000000000000000000000000000000000000000000000000  extra-file\n")

        result = self._run_recipe()
        self.assertNotEqual(result.returncode, 0, "the recipe must refuse a tampered SHA256SUMS, not exit 0")
        self.assertNotIn(
            "REAL_INSTALLER_RAN", result.stdout, "install.sh must never run against an unverified SHA256SUMS"
        )
        self.assertIn("does not carry a valid signature", result.stdout + result.stderr)

    def test_a_missing_signature_is_refused_and_never_run(self):
        # SHA256SUMS.sig absent altogether (a release host that dropped it, or a fetch that
        # failed silently) must refuse the same way a bad signature does, not skip the check.
        os.remove(os.path.join(self.tmp, "SHA256SUMS.sig"))

        result = self._run_recipe()
        self.assertNotEqual(result.returncode, 0, "the recipe must refuse a missing SHA256SUMS.sig, not exit 0")
        self.assertNotIn(
            "REAL_INSTALLER_RAN", result.stdout, "install.sh must never run with no signature to verify"
        )


class PublicDocsTwinsAreFresh(unittest.TestCase):
    """The private-side test: the twins actually tracked under public_docs_dir(_REPO_ROOT) -- publish/files/
    in this tree, the root in the public one -- must pass release_check's own `twins` check right
    now, so editing README.md or INSTALL.md without updating its Chinese twin fails
    `pytest packaging` long before anyone runs a release. This never builds a fixture: it is the
    real tracked files, the same ones release.sh's preflight checks with the same function."""

    def test_the_tracked_public_docs_twins_are_fresh(self):
        try:
            rc.check_twins(_PUBLISH_FILES)
        except rc.ReleaseCheckError as e:
            self.fail(
                f"{_PUBLISH_FILES}'s Chinese twins are stale or incomplete -- update the twin in the "
                f"same change as its English original (its translated-from marker): {e}"
            )


_GUIDE_DIR = os.path.join(_PUBLISH_FILES, "docs", "guide")
# The guide's pages, so a page may link a sibling written in another branch before both are merged; the
# check that every one of them (and its twin) exists runs on the merged tree (GuideCompleteTests).
_GUIDE_PAGE_NAMES = ("README", "getting-started", "migrate-from-tmux", "keys", "configuration", "companion", "turn-review", "permissions", "markdown-preview",
                     "agents", "editor-tools", "pages", "compositor-windows", "write-a-plugin", "environments",
                     "remote-development", "performance")
_FENCE_RE = re.compile(r"^\s*(```|~~~)")
_LINK_RE = re.compile(r"\]\(([^)\s]+)(?:\s+\"[^\"]*\")?\)")


def _guide_pages():
    if not os.path.isdir(_GUIDE_DIR):
        return []
    return sorted(n for n in os.listdir(_GUIDE_DIR) if n.endswith(".md") and not n.endswith(".zh-CN.md"))


def _outside_fences(text):
    """The page's lines outside fenced code blocks, as (line number, line)."""
    inside = False
    for number, line in enumerate(text.splitlines(), 1):
        if _FENCE_RE.match(line):
            inside = not inside
            continue
        if not inside:
            yield number, line


def _resolves(page_dir_rel, target):
    """A relative link from a guide page resolves to a file of the public tree: a public-only file under
    publish/files/, or an exported repo file at the same path in the private tree."""
    path = target.split("#", 1)[0]
    if not path:
        return True
    rel = os.path.normpath(os.path.join(page_dir_rel, path))
    if rel.startswith(".."):
        return False
    if os.path.dirname(rel) == "docs/guide" and os.path.basename(rel) in {f"{n}.md" for n in _GUIDE_PAGE_NAMES}:
        return True
    return os.path.isfile(os.path.join(_PUBLISH_FILES, rel)) or os.path.isfile(os.path.join(_REPO_ROOT, rel))


class GuidePagesTests(unittest.TestCase):
    """Every English guide page under docs/guide/: the language switch on line 1, one H1, and every
    relative link landing on a file the public tree has (the site's importer refuses the same cases)."""

    def test_the_guide_index_exists(self):
        self.assertIn("README.md", _guide_pages())

    def test_each_page_starts_with_its_language_switch(self):
        for name in _guide_pages():
            with open(os.path.join(_GUIDE_DIR, name), encoding="utf-8") as f:
                first = f.readline().rstrip("\n")
            self.assertEqual(first, f"English | [简体中文]({name[:-3]}.zh-CN.md)", name)

    def test_each_page_has_exactly_one_h1(self):
        for name in _guide_pages():
            with open(os.path.join(_GUIDE_DIR, name), encoding="utf-8") as f:
                h1 = [n for n, line in _outside_fences(f.read()) if line.startswith("# ")]
            self.assertEqual(len(h1), 1, f"{name}: H1 on lines {h1}")

    def test_every_relative_link_resolves(self):
        for name in _guide_pages():
            with open(os.path.join(_GUIDE_DIR, name), encoding="utf-8") as f:
                text = f.read()
            for number, line in _outside_fences(text):
                if number == 1:
                    continue  # the language switch, checked on its own; the twin is checked once the twins exist
                for target in _LINK_RE.findall(line):
                    if re.match(r"^[a-z][a-z0-9+.-]*:|^//|^#", target):
                        continue
                    self.assertTrue(_resolves("docs/guide", target), f"{name}:{number}: {target}")


class GuideCompleteTests(unittest.TestCase):
    """On the merged tree: every guide page the checks accept a link to exists, and has its Chinese twin."""

    def test_every_guide_page_and_its_twin_exist(self):
        for name in _GUIDE_PAGE_NAMES:
            for file in (f"{name}.md", f"{name}.zh-CN.md"):
                self.assertTrue(os.path.isfile(os.path.join(_GUIDE_DIR, file)), file)


def _heading_slug(heading):
    heading = re.sub(r"<[^>]*>", "", heading).lower()
    return "".join(
        char for char in heading
        if char in "-_ " or unicodedata.category(char)[0] not in "PS"
    ).replace(" ", "-")


def _markdown_anchors(text):
    anchors = set()
    counts = {}
    for _, line in _outside_fences(text):
        anchors.update(re.findall(r'<a\s+(?:id|name)=[\"\']([^\"\']+)[\"\']', line))
        heading = re.match(r"^ {0,3}#{1,6}\s+(.+?)(?:\s+#+)?\s*$", line)
        if heading:
            slug = _heading_slug(heading.group(1))
            count = counts.get(slug, 0)
            counts[slug] = count + 1
            anchors.add(f"{slug}-{count}" if count else slug)
    return anchors


def _chinese_link_errors(page, text):
    errors = []
    for number, line in _outside_fences(text):
        for target in _LINK_RE.findall(line):
            url = urlsplit(target)
            if url.scheme or url.netloc or target.startswith("/") or not url.path.endswith(".md"):
                continue
            path = os.path.normpath(os.path.join(os.path.dirname(page), unquote(url.path)))
            # The language switch deliberately links to the English original.
            language_switch = number == 1 and line.startswith("[English](")
            twin = path[:-3] + ".zh-CN.md"
            if not language_switch and not path.endswith(".zh-CN.md") and os.path.isfile(twin):
                errors.append(f"{page}:{number}: {target}: use the Chinese twin")
            if url.fragment:
                if not os.path.isfile(path):
                    errors.append(f"{page}:{number}: {target}: missing target")
                    continue
                with open(path, encoding="utf-8") as stream:
                    anchors = _markdown_anchors(stream.read())
                if unquote(url.fragment) not in anchors:
                    errors.append(f"{page}:{number}: {target}: unknown anchor")
    return errors


_RELEASES_DIR = os.path.join(_PUBLISH_FILES, "docs", "releases")
_RELEASE_NOTES_IN = os.path.join(_REPO_ROOT, "packaging", "release-notes.md.in")
# The documentation site replaces this comment, alone on its line, with the named section of the file: the heading's
# line is left out and the section runs to the next "## " heading. GitHub shows nothing for it.
_FULL_LIST_RE = re.compile(r'^<!-- full-list: (?P<file>\S+) "(?P<heading>[^"]+)" -->$')


def _release_pages():
    return sorted(n for n in os.listdir(_RELEASES_DIR) if n.endswith(".md") and not n.endswith(".zh-CN.md"))


def _full_list_markers(text):
    return [(number, m) for number, line in _outside_fences(text) if (m := _FULL_LIST_RE.match(line))]


class ReleasePagesTests(unittest.TestCase):
    """docs/releases/: one page per minor version and one for the versions before 0.4, read on GitHub and imported by
    the documentation site. The full list of changes is not copied into a page: a full-list comment names the section
    of packaging/release-notes.md.in (the GitHub release body) that the site puts there, so the two cannot disagree."""

    def test_each_page_has_its_switch_one_h1_and_resolving_links(self):
        pages = _release_pages()
        self.assertIn("before-0.4.md", pages)
        for name in pages:
            with open(os.path.join(_RELEASES_DIR, name), encoding="utf-8") as f:
                text = f.read()
            self.assertEqual(text.split("\n", 1)[0], f"English | [简体中文]({name[:-3]}.zh-CN.md)", name)
            h1 = [n for n, line in _outside_fences(text) if line.startswith("# ")]
            self.assertEqual(len(h1), 1, f"{name}: H1 on lines {h1}")
            for number, line in _outside_fences(text):
                if number == 1:
                    continue
                for target in _LINK_RE.findall(line):
                    if re.match(r"^[a-z][a-z0-9+.-]*:|^//|^#", target):
                        continue
                    self.assertTrue(_resolves("docs/releases", target), f"{name}:{number}: {target}")

    def test_every_full_list_names_a_section_of_the_release_notes(self):
        with open(_RELEASE_NOTES_IN, encoding="utf-8") as f:
            headings = {line[3:].rstrip("\n") for line in f if line.startswith("## ")}
        seen = 0
        for name in sorted(os.listdir(_RELEASES_DIR)):
            if not name.endswith(".md"):
                continue
            with open(os.path.join(_RELEASES_DIR, name), encoding="utf-8") as f:
                text = f.read()
            stray = [n for n, line in _outside_fences(text) if "full-list:" in line and not _FULL_LIST_RE.match(line)]
            self.assertEqual(stray, [], f"{name}: a full-list comment must be alone on its line, as the pattern says")
            for number, m in _full_list_markers(text):
                seen += 1
                self.assertEqual(m["file"], "packaging/release-notes.md.in", f"{name}:{number}")
                self.assertIn(m["heading"], headings, f"{name}:{number}: no '## {m['heading']}' in release-notes.md.in")
        self.assertGreater(seen, 0, "no release page imports a full list")

    def test_a_twin_imports_the_same_full_list_as_its_english_page(self):
        for name in _release_pages():
            zh = os.path.join(_RELEASES_DIR, name[:-3] + ".zh-CN.md")
            with open(os.path.join(_RELEASES_DIR, name), encoding="utf-8") as f:
                en_markers = [m.group(0) for _, m in _full_list_markers(f.read())]
            with open(zh, encoding="utf-8") as f:
                zh_markers = [m.group(0) for _, m in _full_list_markers(f.read())]
            self.assertEqual(en_markers, zh_markers, name)

    def test_the_marker_pattern_rejects_what_the_site_would_not_replace(self):
        self.assertIsNotNone(_FULL_LIST_RE.match('<!-- full-list: packaging/release-notes.md.in "What\'s new since 0.3.0" -->'))
        for bad in ('<!-- full-list: packaging/release-notes.md.in What\'s new -->',
                    ' <!-- full-list: packaging/release-notes.md.in "x" -->',
                    '<!-- full-list: packaging/release-notes.md.in "x" --> trailing'):
            self.assertIsNone(_FULL_LIST_RE.match(bad), bad)


class ChineseDocsLinksTests(unittest.TestCase):
    def test_every_chinese_page_uses_twins_and_existing_anchors(self):
        pages = []
        for directory, _, names in os.walk(_PUBLISH_FILES):
            pages.extend(os.path.join(directory, name) for name in sorted(names) if name.endswith(".zh-CN.md"))
        self.assertTrue(pages)
        errors = []
        for page in sorted(pages):
            with open(page, encoding="utf-8") as stream:
                errors.extend(_chinese_link_errors(page, stream.read()))
        self.assertEqual(errors, [], "\n" + "\n".join(errors))

    def test_heading_slugs_keep_chinese_and_drop_punctuation(self):
        for heading, slug in (("打开一个项目", "打开一个项目"), ("前缀", "前缀"), ("设置", "设置"),
                              ("Eitri 自己崩溃时", "eitri-自己崩溃时"),
                              ("浮动窗口、便笺板和工作区", "浮动窗口便笺板和工作区")):
            self.assertEqual(_heading_slug(heading), slug)
        self.assertEqual(_markdown_anchors("## 设置\n## 设置\n<a id=\"stable\"></a>\n```md\n## hidden\n```"),
                         {"设置", "设置-1", "stable"})
        for name, slug in (("getting-started", "打开一个项目"), ("keys", "前缀"), ("configuration", "设置")):
            with open(os.path.join(_GUIDE_DIR, name + ".zh-CN.md"), encoding="utf-8") as stream:
                self.assertIn(slug, _markdown_anchors(stream.read()))

    def test_external_links_no_twin_and_fenced_examples_are_ignored(self):
        page = os.path.join(_GUIDE_DIR, "getting-started.zh-CN.md")
        text = ('[English](getting-started.md) | 简体中文\n'
                '[external](https://example.invalid/keys.md#missing)\n'
                '[external](//example.invalid/keys.md#missing)\n'
                '[no twin](../../CONTRIBUTING.md)\n'
                '```md\n[example](keys.md#missing)\n```\n'
                '~~~md\n[example](keys.md#missing)\n~~~\n')
        self.assertEqual(_chinese_link_errors(page, text), [])


class KnownIssuesAnchorsTests(unittest.TestCase):
    def test_chinese_agent_links_land_on_the_corresponding_sections(self):
        with open(os.path.join(_PUBLISH_FILES, "docs/known-issues.zh-CN.md"), encoding="utf-8") as stream:
            text = stream.read()
        for anchor, heading in (("claude-code-conversations", "Claude Code 对话"),
                                ("codex-and-acp-tabs", "Codex 与 ACP 标签页")):
            self.assertRegex(text, rf'<a id="{anchor}"></a>\s*\n## {re.escape(heading)}\n')
        with open(os.path.join(_GUIDE_DIR, "agents.zh-CN.md"), encoding="utf-8") as stream:
            self.assertIn("../known-issues.zh-CN.md#codex-and-acp-tabs", stream.read())


#
# What readers see says what has been tried and what has not, never how: no word about headless or automated runs,
# scripted or stand-in models, harnesses, rows or suites that passed, or a test account. Product features that use
# such words (the Codex self-check's scripted conversation, a tmux stand-in, scripted launches) are the allow list,
# phrase by phrase and page by page. Text is matched with its line breaks folded into spaces, so a phrase wrapped
# across two lines is still found.
_METHOD_WORDING = [re.compile(p, re.IGNORECASE) for p in (
    r"\bhead-?less\b",
    r"\bautomat(?:ed|ion)\b",
    r"\bscripted\b",
    r"\bstand-?ins?\b",
    r"\bxvfb\b",
    r"\bharness(?:es)?\b",
    r"\btest accounts?\b",
    r"\bsandbox(?:ed)? (?:pass|run)(?:es)?\b",
    r"\b(?:machine|screen|window) checks?\b",
    r"\bfake (?:language servers?|models?|clients?|agents?)\b",
    r"\bsuites?\b",
    r"\b(?:all|every) (?:\d+ )?rows? of\b",
    r"\b\d+ of \d+ (?:rows|checks)\b",
    r"\bpass(?:ed|es)? (?:all |every |their |its )?(?:\d+ )?(?:of \d+ )?(?:rows|checks)\b",
    r"无头", r"自动化", r"脚本化", r"替身", r"测试账号", r"测试套件", r"窗口检查", r"通过了[^。；]{0,8}检查",
    r"假的?语言服务器",
)]
_METHOD_WORDING_ALLOWED = {
    "CONTRIBUTING.md": ("a green test suite",),
    "docs/guide/agents.md": ("runs a short scripted conversation", "against a stand-in model on your own machine"),
    "docs/guide/agents.zh-CN.md": ("对一个替身模型跑一段几秒钟的脚本化对话",),
    "docs/guide/configuration.md": ("scripted launches",),
    "docs/guide/configuration.zh-CN.md": ("脚本化的启动",),
    "docs/guide/takeover.md": ("its own stand-in", "reaches that stand-in"),
    "docs/guide/takeover.zh-CN.md": ("它自己的替身", "到达的是那个替身"),
}


def _fold(text):
    return re.sub(r"\s+", " ", text)


def method_wording_hits(text, allowed=()):
    """Every phrase in `text` that tells how Eitri was tested, with the `allowed` phrases taken out first."""
    folded = _fold(text)
    for phrase in allowed:
        folded = folded.replace(_fold(phrase), " ")
    hits = []
    for pattern in _METHOD_WORDING:
        for m in pattern.finditer(folded):
            hits.append(folded[max(0, m.start() - 40):m.end() + 40].strip())
    return hits


def _reader_pages():
    """The pages readers see, as paths relative to public_docs_dir(): the top level, docs/, docs/guide/ and
    docs/releases/ (in the public tree, other directories under docs/ are exported repository files)."""
    pages = []
    for sub in ("", "docs", "docs/guide", "docs/releases"):
        directory = os.path.join(_PUBLISH_FILES, sub)
        pages.extend(os.path.join(sub, n) if sub else n for n in sorted(os.listdir(directory)) if n.endswith(".md"))
    return pages


class NoTestingMethodInPublicTextTests(unittest.TestCase):
    """The public pages and the release body (packaging/release-notes.md.in) carry no testing-method wording."""

    def test_the_public_pages_say_nothing_about_how_eitri_was_tested(self):
        problems = []
        pages = _reader_pages()
        self.assertIn("docs/known-issues.md", pages)
        for page in pages:
            with open(os.path.join(_PUBLISH_FILES, page), encoding="utf-8") as f:
                text = f.read()
            problems.extend(f"{page}: ...{hit}..." for hit in method_wording_hits(text, _METHOD_WORDING_ALLOWED.get(page, ())))
        self.assertEqual(problems, [], "\n" + "\n".join(problems))

    def test_the_release_body_says_nothing_about_how_eitri_was_tested(self):
        with open(_RELEASE_NOTES_IN, encoding="utf-8") as f:
            hits = method_wording_hits(f.read())
        self.assertEqual(hits, [], "\npackaging/release-notes.md.in: " + "\n".join(hits))

    def test_every_allowed_phrase_is_still_on_its_page(self):
        for page, phrases in _METHOD_WORDING_ALLOWED.items():
            with open(os.path.join(_PUBLISH_FILES, page), encoding="utf-8") as f:
                folded = _fold(f.read())
            for phrase in phrases:
                self.assertIn(_fold(phrase), folded, f"{page}: the allowed phrase {phrase!r} is gone; drop it here")

    def test_the_patterns_catch_testing_method_and_spare_product_text(self):
        for text in ("tests ran headless", "Everything new has automated tests.", "passed their scripted\nsuites",
                     "against a local stand-in", "the test account reached the model service's rate limit",
                     "passed every row of their checks", "passed 60 of 60 checks", "passed all 17 rows of its",
                     "ran against a fake language server", "Live review passed the machine checks",
                     "对真实 Claude Code 的自动化运行", "用脚本化的模型服务检查的", "拿替身模型试过", "通过了全部 17 行窗口检查"):
            self.assertTrue(method_wording_hits(text), text)
        for text in ("Not tried yet: GNOME's raise.", "the checks below pass", "an automatic restore",
                     "It probes the server", "a row of titles above it", "A newer Codex runs and is marked untested."):
            self.assertEqual(method_wording_hits(text), [], text)
        self.assertEqual(method_wording_hits("it runs a short\nscripted conversation", ("runs a short scripted conversation",)), [])


#
# The prose counter the budgets below use: the prose size of a public Markdown text.
#
# - Prose only. Fenced code blocks (``` or ~~~), HTML comments, HTML tags and their attributes, link targets, image
#   markup (including a linked badge) and the language-switch line do not count.
# - Link text counts. Inline code counts as one word each, however long it is.
# - A text has two sizes, counted separately because a Chinese twin has its own budget for each: words (runs of Latin
#   letters or digits, so `v0.4.0` and `init.lua` are one word each) and CJK ideographs. Punctuation, Chinese or
#   Latin, counts for neither.
#
# Where a budget measures: prose_lead (between the H1 and the first `## `; for a README, which has no plain H1 line,
# the text above the first `## `), prose_section (the body of one `## ` section), prose_before (everything above one
# `## ` heading) and prose_bullets (the list items of a body, each with its continuation lines).
#
#     python3 packaging/tests/test_public_docs.py --count FILE...   # words and CJK characters, whole file and lead
_CJK = "\u3400-\u9fff"
_WORD_RE = re.compile(r"[A-Za-z0-9][A-Za-z0-9'’.+-]*")


def prose_text(text):
    """`text` with everything that does not count taken out (inline code left as the single word `X`)."""
    text = "\n".join(line for _, line in _outside_fences(text))
    text = re.sub(r"<!--.*?-->", " ", text, flags=re.S)
    text = re.sub(r"<(script|style|pre|code)\b[^>]*>.*?</\1>", " ", text, flags=re.S)
    text = re.sub(r"\[!\[[^\]]*\]\([^)]*\)\]\([^)]*\)", " ", text)  # a linked badge
    text = re.sub(r"!\[[^\]]*\]\([^)]*\)", " ", text)
    text = re.sub(r"\[([^\]]*)\]\([^)]*\)", r"\1", text)
    text = re.sub(r"<[^>]+>", " ", text)
    text = re.sub(r"^(English \| .*|\[English\]\(.*)$", " ", text, flags=re.M)
    text = text.replace("&amp;", "&").replace("&nbsp;", " ").replace("&middot;", " ")
    return re.sub(r"`[^`\n]*`", " X ", text)


def prose_size(text):
    """(words, cjk characters) of the prose in `text`."""
    plain = prose_text(text)
    cjk = len(re.findall(f"[{_CJK}]", plain))
    return len(_WORD_RE.findall(re.sub(f"[{_CJK}]", " ", plain))), cjk


def _prose_h2_positions(text):
    inside = False
    pos = 0
    for line in text.splitlines(keepends=True):
        if _FENCE_RE.match(line):
            inside = not inside
        elif not inside and line.startswith("## "):
            yield pos, line[3:].strip()
        pos += len(line)


def prose_headings(text):
    """The `## ` headings of `text`, outside code blocks, in order."""
    return [h for _, h in _prose_h2_positions(text)]


def prose_before(text, heading=None):
    """Everything above the `## <heading>` line, or above the first `## ` line when `heading` is None."""
    for pos, h in _prose_h2_positions(text):
        if heading is None or h == heading:
            return text[:pos]
    return text


def prose_lead(text):
    """The text between the H1 and the first `## ` heading (the whole top of a page with no H1 line)."""
    top = prose_before(text)
    m = re.search(r"^# .*$", top, flags=re.M)
    return top[m.end():] if m else top


def prose_section(text, heading):
    """The body of the `## <heading>` section, or None when there is none."""
    spans = list(_prose_h2_positions(text))
    for i, (pos, h) in enumerate(spans):
        if h == heading:
            start = text.index("\n", pos) + 1 if "\n" in text[pos:] else len(text)
            end = spans[i + 1][0] if i + 1 < len(spans) else len(text)
            return text[start:end]
    return None


def prose_bullets(body):
    """The list items of `body` (lines starting `- `), each with its continuation lines, outside code blocks."""
    items = []
    for _, line in _outside_fences(body):
        if re.match(r"^[-*] ", line):
            items.append(line)
        elif items and line.startswith("  ") and line.strip():
            items[-1] += "\n" + line
        elif not line.strip():
            continue
        else:
            items.append(None)
    return [i for i in items if i]


def _count_main(paths):
    for path in paths:
        with open(path, encoding="utf-8") as stream:
            text = stream.read()
        words, cjk = prose_size(text)
        lead_words, lead_cjk = prose_size(prose_before(text) if "README." in path else prose_lead(text))
        print(f"{path}\twords={words}\tcjk={cjk}\tlead_words={lead_words}\tlead_cjk={lead_cjk}")
    return 0


#
# Budgets: how long each layer of a public text may be, counted by the prose counter above. A Chinese twin gets two CJK
# characters for each word of its English budget, and its own small budget for the Latin words (keys, commands) that
# stay in it. A budget goes up only together with a written reason for the change.
_README_FIRST_SCREEN_WORDS = 100
_README_WORDS = 520
_README_L1_WORDS = 220
_L1_BULLET_WORDS = 30
_README_ZH_FIRST_SCREEN_LATIN = 40
_GUIDE_LEAD_WORDS = 120
_GUIDE_LEAD_ZH_LATIN = 60
_RELEASE_PAGE_WORDS = 450
# The guide pages that conform to the layers, with the budget for their task part (the text above `## Limits`).
# Each stage's commits add the pages they rewrite; a page is never taken off.
_DISCLOSURE_PAGES = {"getting-started": 1500, "performance": 900}
_LIMITS_HEADING = {"": "Limits", ".zh-CN": "限制"}
_README_L1_HEADING = {"": "Why try it", ".zh-CN": "为什么试试"}
_FULL_LIST_HEADING = {"": "The full list", ".zh-CN": "完整清单"}


def _public_text(rel):
    with open(os.path.join(_PUBLISH_FILES, rel), encoding="utf-8") as f:
        return f.read()


def _heading_shape(text):
    """The levels of the headings outside code blocks, in order: what a twin must repeat."""
    return [len(m.group(1)) for _, line in _outside_fences(text) if (m := re.match(r"^(#{1,6}) ", line))]


class DisclosureBudgets(unittest.TestCase):
    def test_the_counter_counts_prose_and_nothing_else(self):
        text = ("English | [简体中文](x.zh-CN.md)\n# T\n<!-- a long comment here -->\n"
                "Use [the guide](../some/long/target.md) and `a --long --flag` now.\n"
                "```sh\nfenced code is not prose\n```\n"
                "<img src=\"a.png\" alt=\"a picture of words\">![alt words](b.png)\n"
                "[![Badge](https://x/y.svg)](https://x)\n中文四个字。\n")
        self.assertEqual(prose_size(text), (7, 5))  # T; Use the guide and X now; five CJK characters
        self.assertEqual(prose_size("v0.4.0 and init.lua"), (3, 0))

    def test_the_counter_finds_the_leads_sections_and_bullets(self):
        text = "# T\nlead words\n## A\n- one two\n  three\n- four\n```\n## not a heading\n```\n## B\nend\n"
        self.assertEqual(prose_headings(text), ["A", "B"])
        self.assertEqual(prose_size(prose_lead(text)), (2, 0))
        self.assertEqual(prose_size(prose_before(text, "B"))[0], 8)
        self.assertEqual([prose_size(b)[0] for b in prose_bullets(prose_section(text, "A"))], [3, 1])
        self.assertIsNone(prose_section(text, "C"))

    def test_the_readme_first_screen_is_short(self):
        words, _ = prose_size(prose_before(_public_text("README.md")))
        self.assertLessEqual(words, _README_FIRST_SCREEN_WORDS, "README.md: prose above the first ## heading")
        cjk, latin = prose_size(prose_before(_public_text("README.zh-CN.md")))[1], prose_size(prose_before(_public_text("README.zh-CN.md")))[0]
        self.assertLessEqual(cjk, 2 * _README_FIRST_SCREEN_WORDS, "README.zh-CN.md: CJK characters above the first ##")
        self.assertLessEqual(latin, _README_ZH_FIRST_SCREEN_LATIN, "README.zh-CN.md: Latin words above the first ##")

    def test_the_readme_is_short(self):
        self.assertLessEqual(prose_size(_public_text("README.md"))[0], _README_WORDS, "README.md: words")
        self.assertLessEqual(prose_size(_public_text("README.zh-CN.md"))[1], 2 * _README_WORDS, "README.zh-CN.md: CJK characters")

    def test_each_readme_point_is_one_short_sentence_without_a_parenthesis(self):
        for suffix, limit in (("", _L1_BULLET_WORDS), (".zh-CN", 2 * _L1_BULLET_WORDS)):
            text = _public_text(f"README{suffix}.md")
            body = prose_section(text, _README_L1_HEADING[suffix])
            self.assertIsNotNone(body, f"README{suffix}.md: no '## {_README_L1_HEADING[suffix]}' section")
            items = prose_bullets(body)
            self.assertTrue(5 <= len(items) <= 7, f"README{suffix}.md: {len(items)} points, want 5 to 7")
            for item in items:
                size = prose_size(item)[1 if suffix else 0]
                self.assertLessEqual(size, limit, f"README{suffix}.md: {item!r}")
                self.assertNotRegex(item, r"[(（]", f"README{suffix}.md: a parenthesis in {item!r}")
            if suffix == "":
                self.assertLessEqual(prose_size(body)[0], _README_L1_WORDS, "README.md: the points together")

    def test_the_readme_carries_no_maintainer_comment_and_no_how_it_differs(self):
        en = _public_text("README.md")
        self.assertNotIn("<!--", en, "README.md: an HTML comment ships in the raw file")
        self.assertEqual(_public_text("README.zh-CN.md").count("<!--"), 1, "README.zh-CN.md: only the translated-from marker")
        self.assertNotIn("How it differs", prose_headings(en))
        self.assertIn("https://eitri.cc/#differs", en)
        self.assertIn("https://eitri.cc/zh/#differs", _public_text("README.zh-CN.md"))

    def test_a_readme_twin_repeats_its_original_structure(self):
        self.assertEqual(_heading_shape(_public_text("README.md")), _heading_shape(_public_text("README.zh-CN.md")))

    def test_each_conforming_guide_page_has_a_short_lead_one_limits_and_a_short_task_part(self):
        for name, budget in sorted(_DISCLOSURE_PAGES.items()):
            for suffix in ("", ".zh-CN"):
                rel = f"docs/guide/{name}{suffix}.md"
                text = _public_text(rel)
                limits = prose_headings(text).count(_LIMITS_HEADING[suffix])
                self.assertEqual(limits, 1, f"{rel}: {limits} '## {_LIMITS_HEADING[suffix]}' sections, want exactly one")
                lead_words, lead_cjk = prose_size(prose_lead(text))
                task_words, task_cjk = prose_size(prose_before(text, _LIMITS_HEADING[suffix]))
                if suffix:
                    self.assertLessEqual(lead_cjk, 2 * _GUIDE_LEAD_WORDS, f"{rel}: lead, CJK characters")
                    self.assertLessEqual(lead_words, _GUIDE_LEAD_ZH_LATIN, f"{rel}: lead, Latin words")
                    self.assertLessEqual(task_cjk, 2 * budget, f"{rel}: the part above Limits, CJK characters")
                else:
                    self.assertLessEqual(lead_words, _GUIDE_LEAD_WORDS, f"{rel}: lead")
                    self.assertLessEqual(task_words, budget, f"{rel}: the part above Limits")

    def test_a_conforming_page_and_its_twin_have_the_same_headings_in_the_same_order(self):
        for name in sorted(_DISCLOSURE_PAGES):
            en = _public_text(f"docs/guide/{name}.md")
            zh = _public_text(f"docs/guide/{name}.zh-CN.md")
            self.assertEqual(_heading_shape(en), _heading_shape(zh), name)

    def test_a_conforming_page_names_no_other_page_in_its_first_sentence_as_a_table_of_contents(self):
        for name in sorted(_DISCLOSURE_PAGES):
            lead = prose_lead(_public_text(f"docs/guide/{name}.md")).strip()
            self.assertNotRegex(lead.split("\n", 1)[0], r"(?i)^this page (takes|covers|shows|walks)", name)

    def test_each_release_page_is_short_above_its_full_list(self):
        seen = 0
        for name in _release_pages():
            for suffix in ("", ".zh-CN"):
                text = _public_text(f"docs/releases/{name[:-3]}{suffix}.md")
                heading = _FULL_LIST_HEADING[suffix]
                if heading not in prose_headings(text):
                    continue
                seen += 1
                words, cjk = prose_size(prose_before(text, heading))
                if suffix:
                    self.assertLessEqual(cjk, 2 * _RELEASE_PAGE_WORDS, f"{name[:-3]}{suffix}.md: CJK characters above the full list")
                else:
                    self.assertLessEqual(words, _RELEASE_PAGE_WORDS, f"{name}: words above the full list")
        self.assertGreater(seen, 0)


class PublicTreeLayoutTests(unittest.TestCase):
    """This suite ships in the public tree (publish/manifest.txt's `include packaging/*`) and must pass
    there too, where there is no publish/ at all and publish/files/ has been copied over the root by
    publish/export.sh. Reading publish/files/ unconditionally made 9 of its 17 tests fail or error in a
    real export while every one passed here, so this runs the whole suite in that layout: packaging/ as
    it is, publish/files/ copied over the root path for path, and no publish/."""

    @pytest.fixture(autouse=True)
    def _tmp_path_fixture(self, tmp_path):
        self.tmp = str(tmp_path)

    def setUp(self):
        if getattr(self, "tmp", None) is None:
            os.makedirs(_SCRATCH_ROOT, exist_ok=True)
            self.tmp = tempfile.mkdtemp(dir=_SCRATCH_ROOT)
            self.addCleanup(shutil.rmtree, self.tmp, ignore_errors=True)

    def test_public_docs_dir_is_publish_files_in_a_private_tree_and_the_root_in_the_public_one(self):
        private = os.path.join(self.tmp, "private")
        os.makedirs(os.path.join(private, "publish", "files"))
        self.assertEqual(public_docs_dir(private), os.path.join(private, "publish", "files"))

        public = os.path.join(self.tmp, "public")
        os.makedirs(os.path.join(public, "packaging"))
        self.assertEqual(public_docs_dir(public), public)

        # If publish/ exists but publish/files/ is missing, keep that missing directory as the docs path.
        # Falling back to the root README.md would check the wrong documentation.
        lost = os.path.join(self.tmp, "lost")
        os.makedirs(os.path.join(lost, "publish"))
        self.assertEqual(public_docs_dir(lost), os.path.join(lost, "publish", "files"))

    def test_the_whole_suite_passes_in_the_public_tree_layout(self):
        if public_docs_dir(_REPO_ROOT) == _REPO_ROOT:
            self.skipTest("already running in the public tree's layout")
        root = os.path.join(self.tmp, "public")
        shutil.copytree(
            _PACKAGING,
            os.path.join(root, "packaging"),
            ignore=shutil.ignore_patterns("__pycache__", ".pytest_cache"),
        )
        shutil.copytree(_PUBLISH_FILES, root, dirs_exist_ok=True)
        # The public tree also carries docs/keymap/, which guide pages link to.
        shutil.copytree(os.path.join(_REPO_ROOT, "docs", "keymap"), os.path.join(root, "docs", "keymap"))
        # And examples/ (the recipes and the example plugin), which the environments page links to.
        shutil.copytree(os.path.join(_REPO_ROOT, "examples"), os.path.join(root, "examples"))
        self.assertFalse(os.path.lexists(os.path.join(root, "publish")))

        result = subprocess.run(
            [
                sys.executable,
                "-m",
                "pytest",
                "-q",
                "-p",
                "no:cacheprovider",
                "--basetemp",
                os.path.join(self.tmp, "basetemp"),
                os.path.join("packaging", "tests", os.path.basename(__file__)),
            ],
            cwd=root,
            capture_output=True,
            text=True,
            env={**os.environ, "PYTHONDONTWRITEBYTECODE": "1"},
        )
        output = result.stdout + result.stderr
        self.assertEqual(result.returncode, 0, output)

        # Every test ran there, bar this one (it skips itself in that layout) and the recipe tests
        # when ssh-keygen is absent (they skip here too): the suite did not pass by skipping.
        total = unittest.defaultTestLoader.loadTestsFromModule(sys.modules[__name__]).countTestCases()
        skipped = 1
        if shutil.which("ssh-keygen") is None:
            skipped += len(unittest.defaultTestLoader.getTestCaseNames(VerifyRecipeRefusesTamperedInstallShTests))
        passed = re.search(r"(\d+) passed", output)
        self.assertIsNotNone(passed, output)
        self.assertEqual(int(passed.group(1)), total - skipped, output)


if __name__ == "__main__":
    if sys.argv[1:2] == ["--count"]:
        sys.exit(_count_main(sys.argv[2:]))
    unittest.main()
