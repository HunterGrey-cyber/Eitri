"""Tests for the public docs under publish/files/ (v1-dist plan, docs/superpowers/plans/2026-09-28-v1-dist-task17-18.md,
Tasks 1-5) -- or, in the public tree this file also ships in, at the root, where publish/export.sh
copies them (public_docs_dir(); `PublicTreeLayoutTests` runs this whole suite in that layout): every
documented `curl ... install.sh | sh` command must carry the same HTTPS-safety flags
packaging/install.sh's own fetch() function uses for the identical download (verdict #6,
the private review notes #6: a GitHub `releases/latest/download/` URL
answers with a 302, and without `-L` (or `--proto-redir`) `curl -sSf | sh` silently pipes nothing to
`sh` and exits 0 -- no install happens and no error is shown).

Also (Task 2): INSTALL.md's "verify before running" recipe must actually hash the downloaded
`install.sh` against the verified `SHA256SUMS`, not just check `SHA256SUMS`'s own signature (verdict
#7, the private review notes #7: checking the manifest's signature
proves the manifest is authentic, never that the script you are about to run matches it -- a release
host or mirror that swaps `install.sh` while leaving `SHA256SUMS`/`.sig` alone passes every step of
the documented recipe otherwise). `VerifyRecipeRefusesTamperedInstallShTests` proves this by actually
running the recipe, extracted verbatim out of INSTALL.md, against a throwaway ssh key and a tiny
self-signed `SHA256SUMS` this test builds itself in pytest's own `tmp_path` -- never against
`/scratch/...` or the real rc.1 release, so it ships publicly and runs for any contributor with
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
under plain unittest, in a fresh directory under `~/.cache/neovibe-public-docs-tests` removed after
each test -- never a bare `tempfile.mkdtemp()`. The required flags are parsed out of the real
packaging/install.sh -- never hardcoded a second time here -- so a future change to fetch()'s own
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

import pytest

_HERE = os.path.dirname(os.path.abspath(__file__))
_SCRATCH_ROOT = os.path.expanduser("~/.cache/neovibe-public-docs-tests")
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
    "https://github.com/HunterGrey-cyber/neovibe/releases/latest/download/install.sh | sh"
)

# A line documenting the installer, in a public doc: curl ... install.sh ... | sh [-s -- ...].
_CURL_INSTALL_LINE_RE = re.compile(r"^\s*curl\b.*\binstall\.sh\b.*\|\s*sh\b")


def _fetch_https_branch(install_sh_text):
    """The body of packaging/install.sh's fetch() function, and within it the `else` branch's curl
    invocation -- the one used for every non-loopback (real) download, i.e. every URL a public doc
    also tells a user to fetch by hand."""
    m = re.search(r"\nfetch\(\) \{\n(.*?)\n\}\n", install_sh_text, re.DOTALL)
    if not m:
        raise AssertionError("packaging/install.sh: could not find a fetch() function to read flags from")
    body = m.group(1)
    https_line = None
    for line in body.splitlines():
        stripped = line.strip()
        if stripped.startswith("curl") and "'=https'" in stripped:
            https_line = stripped
            break
    if https_line is None:
        raise AssertionError("packaging/install.sh: fetch() has no https-branch curl invocation to read flags from")
    # Drop the trailing `|| die "..."`: only the curl invocation itself is parsed.
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
    packaging/install.sh's own fetch() -- not a second, hand-kept copy."""
    with open(_INSTALL_SH, encoding="utf-8") as f:
        install_sh_text = f.read()
    https_curl = _fetch_https_branch(install_sh_text)
    flags = curl_flags(https_curl)
    if flags["proto"] is None or flags["proto_redir"] is None or not flags["has_follow_redirects"]:
        raise AssertionError(
            f"packaging/install.sh's own fetch() no longer sets --proto/--proto-redir/-L "
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
    """required_fetch_flags() reads packaging/install.sh's own fetch(), rather than a hardcoded copy."""

    def test_the_https_branch_sets_proto_proto_redir_and_follow_redirects(self):
        required = required_fetch_flags()
        self.assertEqual(required["proto"], "=https")
        self.assertEqual(required["proto_redir"], "=https")
        self.assertTrue(required["has_follow_redirects"])

    def test_the_loopback_branch_is_not_what_this_test_reads(self):
        # The loopback branch (NV_LOOPBACK=1) uses --proto '=http' with no --proto-redir/-L -- it is
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
        # The exact regression verdict #6 found: -sSf with no L, so a 302 redirect pipes an empty
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
                "# neovibe",
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
    the root in the public tree) carries the flags packaging/install.sh's own fetch() requires."""

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
        self.assertIn(_README_ONE_LINER, lines, "README.md's curl one-liner does not match the exact literal (verdict #6)")


def extract_verify_recipe(doc_text):
    """INSTALL.md's "verify before running" recipe (verdict #7): the text of the fenced ```sh code
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
    the verified SHA256SUMS (verdict #7) -- checking SHA256SUMS's own signature alone proves
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
        self.assertIn("release@neovibe", self.recipe)
        self.assertIn("neovibe-release", self.recipe)


class VerifyRecipeRefusesTamperedInstallShTests(unittest.TestCase):
    """The recipe extracted verbatim from INSTALL.md, run for real against a throwaway ssh key and
    a tiny self-signed SHA256SUMS this test builds itself in a scratch directory (never against
    /scratch/... or the real rc.1 release -- this suite ships publicly and must run for any
    contributor). It must run install.sh when everything matches, and refuse -- never running
    install.sh -- when install.sh's bytes were swapped after signing while SHA256SUMS/.sig were
    left alone: exactly the attack verdict #7 names (a compromised release host or mirror)."""

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
            f.write(f'release@neovibe namespaces="neovibe-release" {key_type} {key_data}\n')

        install_sh = os.path.join(self.tmp, "install.sh")
        with open(install_sh, "w", encoding="utf-8") as f:
            f.write("#!/bin/sh\necho REAL_INSTALLER_RAN\n")
        os.chmod(install_sh, 0o755)

        sums_path = os.path.join(self.tmp, "SHA256SUMS")
        with open(sums_path, "w", encoding="utf-8") as f:
            f.write(f"{_sha256_file(install_sh)}  install.sh\n")

        subprocess.run(
            ["ssh-keygen", "-Y", "sign", "-f", key_path, "-n", "neovibe-release", sums_path],
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
        # them, the way a compromised release host or mirror would do it (verdict #7).
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
    """Task 5's private-side test (docs/superpowers/plans/2026-09-28-v1-dist-task17-18.md's own
    acceptance line): the twins actually tracked under public_docs_dir(_REPO_ROOT) -- publish/files/
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
                f"same change as its English original (Task 5's marker, Decisions section): {e}"
            )


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

        # A private tree that lost publish/files/ keeps pointing there (and fails loudly), rather
        # than quietly checking the private root's own README.md.
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
    unittest.main()
