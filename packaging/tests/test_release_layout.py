"""Unit tests for packaging/release_check.py -- the release build's content and layout checks,
factored out as pure functions so each one is tested here with no container and no network
(Task 12 pre-think Section 5 step 1; plan docs/superpowers/plans/2026-09-27-v1-dist-task12.md,
Task 1).

    python3 -m pytest packaging -q
    python3 packaging/tests/test_release_layout.py

Fixtures are built at run time: real tarballs (`tarfile`), a real `.deb`-shaped archive (a small ar
writer below, then extracted with the host's real `dpkg-deb -x`), a real `.rpm` (the host's real
nfpm, then extracted with the host's real `bsdtar -xf`), and tiny throwaway git repositories (`git
init` in scratch). Nothing here needs Docker, a network connection, or a built Eitri release --
every fixture is planted by the test itself. Scratch lives under ~/.cache, never /tmp (a small
shared tmpfs on this project's own dev machines, not this test's call to assume otherwise
elsewhere).
"""

import atexit
import contextlib
import hashlib
import importlib.util
import io
import json
import os
import re
import shutil
import subprocess
import tarfile
import tempfile
import unittest

_HERE = os.path.dirname(os.path.abspath(__file__))
_PACKAGING = os.path.dirname(_HERE)
_spec = importlib.util.spec_from_file_location("release_check", os.path.join(_PACKAGING, "release_check.py"))
rc = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(rc)

_SCRATCH_ROOT = os.path.expanduser("~/.cache/eitri-release-check-tests")
_run_root = None


def _scratch_dir():
    """A fresh directory inside this process's own run directory, which is removed at exit.

    Many fixtures (the git repos, the directory an archive is written into before it is
    extracted) have no single owner that could clean them up, so none of them is cleaned up one by
    one: every scratch directory lives under one per-process root and goes with it."""
    global _run_root
    if _run_root is None:
        os.makedirs(_SCRATCH_ROOT, exist_ok=True)
        _run_root = tempfile.mkdtemp(dir=_SCRATCH_ROOT, prefix="run-")
        atexit.register(shutil.rmtree, _run_root, ignore_errors=True)
    return tempfile.mkdtemp(dir=_run_root)


def _make_tar_gz(dest_path, files):
    """files: [(relpath, bytes), ...]. Writes a real gzip-compressed tar to dest_path."""
    with tarfile.open(dest_path, mode="w:gz") as tf:
        for relpath, data in files:
            info = tarfile.TarInfo(name=relpath)
            info.size = len(data)
            tf.addfile(info, io.BytesIO(data))


def _tar_gz_bytes(files):
    buf = io.BytesIO()
    with tarfile.open(fileobj=buf, mode="w:gz") as tf:
        for relpath, data in files:
            info = tarfile.TarInfo(name=relpath)
            info.size = len(data)
            tf.addfile(info, io.BytesIO(data))
    return buf.getvalue()


def _ar_member_header(name, size, mtime=0, uid=0, gid=0, mode=0o100644):
    # The "common" ar header GNU ar, BSD ar and dpkg-deb all agree on: 16-byte name, 12-byte
    # decimal mtime, 6-byte decimal uid, 6-byte decimal gid, 8-byte octal mode, 10-byte decimal
    # size, then the two-byte end marker 0x60 0x0a. All fields left-justified, space-padded.
    return (
        name[:16].ljust(16)
        + str(mtime).ljust(12)
        + str(uid).ljust(6)
        + str(gid).ljust(6)
        + oct(mode)[2:].ljust(8)
        + str(size).ljust(10)
        + "`\n"
    ).encode("ascii")


def _write_ar(path, members):
    """members: [(name, bytes), ...]. A small ar (Unix archive) writer -- just enough of the
    format for dpkg-deb -x to read it back, which is how a real .deb is built without needing the
    host's own `ar`/`dpkg-deb` binary to construct the fixture (only to read it, which this test
    does need and the host has)."""
    with open(path, "wb") as f:
        f.write(b"!<arch>\n")
        for name, data in members:
            f.write(_ar_member_header(name, len(data)))
            f.write(data)
            if len(data) % 2 == 1:
                f.write(b"\n")


def _make_deb(dest_path, data_files, control_extra=""):
    """A real .deb: ar(debian-binary, control.tar.gz, data.tar.gz). `data_files` become the
    package payload (relpaths rooted at /), extracted by dpkg-deb -x the same way a real install
    would unpack it."""
    control = _tar_gz_bytes([(
        "./control",
        (
            "Package: fixture\nVersion: 1.0\nArchitecture: amd64\n"
            "Maintainer: Fixture <fixture@example.com>\nDescription: fixture\n" + control_extra
        ).encode("ascii"),
    )])
    data = _tar_gz_bytes([(f"./{relpath.lstrip('/')}", content) for relpath, content in data_files])
    _write_ar(dest_path, [
        ("debian-binary", b"2.0\n"),
        ("control.tar.gz", control),
        ("data.tar.gz", data),
    ])


_NFPM = shutil.which("nfpm")


def _make_rpm(dest_path, data_files):
    """A real .rpm via the host's own nfpm 2.47.0 (Task 12 pre-think Section 5 step 1; M8's tool
    list). Stages `data_files` (relpaths rooted at /) under a scratch dir and points a minimal
    nfpm.yaml's `contents` at them."""
    stage = _scratch_dir()
    for relpath, content in data_files:
        dst = os.path.join(stage, relpath.lstrip("/"))
        os.makedirs(os.path.dirname(dst), exist_ok=True)
        with open(dst, "wb") as f:
            f.write(content)
    contents_yaml = "\n".join(
        f"  - src: {os.path.join(stage, relpath.lstrip('/'))}\n    dst: {relpath}"
        for relpath, _ in data_files
    )
    yaml_path = os.path.join(stage, "fixture-nfpm.yaml")
    with open(yaml_path, "w", encoding="utf-8") as f:
        f.write(
            "name: fixturepkg\n"
            "arch: amd64\n"
            "platform: linux\n"
            "version: 1.0.0\n"
            "maintainer: Fixture <fixture@example.com>\n"
            "description: fixture\n"
            "homepage: https://example.com\n"
            "license: MIT\n"
            "contents:\n" + contents_yaml + "\n"
        )
    try:
        subprocess.run(
            ["nfpm", "pkg", "--packager", "rpm", "--config", yaml_path, "--target", dest_path],
            check=True, capture_output=True,
        )
    finally:
        shutil.rmtree(stage, ignore_errors=True)


def _git(repo, *args):
    return subprocess.run(
        ["git", "-C", repo, *args], check=True, capture_output=True, text=True,
        env={**os.environ, "GIT_AUTHOR_NAME": "fixture", "GIT_AUTHOR_EMAIL": "fixture@example.com",
             "GIT_COMMITTER_NAME": "fixture", "GIT_COMMITTER_EMAIL": "fixture@example.com"},
    ).stdout


def _init_git_repo(files):
    """files: {relpath: bytes}. `git init`s a scratch repo, commits `files`, returns (repo_dir,
    head_sha)."""
    repo = _scratch_dir()
    _git(repo, "init", "-q")
    for relpath, data in files.items():
        path = os.path.join(repo, relpath)
        if os.path.dirname(relpath):
            os.makedirs(os.path.dirname(path), exist_ok=True)
        with open(path, "wb") as f:
            f.write(data)
    _git(repo, "add", "-A")
    _git(repo, "commit", "-q", "-m", "fixture")
    head = _git(repo, "rev-parse", "HEAD").strip()
    return repo, head


# --- 1. RELEASE field set --------------------------------------------------------------------

_GOOD_RELEASE_FIELDS = {
    "EITRI_VERSION": "1.0.0-rc.1",
    "EITRI_COMMIT": "a" * 40,
    "NEOVIDE_FORK_COMMIT": "b" * 40,
    "VERDANDI_REV": "c" * 40,
    "VERDANDI_SOURCE": "verdandi-1234567-source.tar.gz",
    "VERDANDI_SOURCE_SHA256": "d" * 64,
    "NODE_VERSION": "v22.23.2",
    "NODE_SHA256_linux_x64": "e" * 64,
    "NODE_SHA256_linux_arm64": "f" * 64,
    "NVIM_VERSION": "0.11.2",
    "NVIM_SHA256_linux_x86_64": "0" * 64,
    "SKIA_BINARIES_ARCHIVE": "skia-binaries-0.153.3-x86_64-linux.tar.gz",
    "SKIA_BINARIES_SHA256": "1" * 64,
    "GTK_FLOOR": "4.14",
    "BUILD_IMAGE": "6232b387cafe",
}


class ReleaseFieldsTests(unittest.TestCase):
    def test_the_full_valid_set_passes(self):
        rc.validate_release(dict(_GOOD_RELEASE_FIELDS))  # must not raise

    def test_a_missing_field_fails(self):
        fields = dict(_GOOD_RELEASE_FIELDS)
        del fields["BUILD_IMAGE"]
        with self.assertRaisesRegex(rc.ReleaseCheckError, "missing field.*BUILD_IMAGE"):
            rc.validate_release(fields)

    def test_an_unexpected_field_fails(self):
        fields = dict(_GOOD_RELEASE_FIELDS)
        fields["EXTRA_FIELD"] = "x"
        with self.assertRaisesRegex(rc.ReleaseCheckError, "unexpected field.*EXTRA_FIELD"):
            rc.validate_release(fields)

    def test_a_non_hex40_commit_fails(self):
        fields = dict(_GOOD_RELEASE_FIELDS)
        fields["EITRI_COMMIT"] = "not-40-hex"
        with self.assertRaisesRegex(rc.ReleaseCheckError, "EITRI_COMMIT"):
            rc.validate_release(fields)

    def test_a_non_hex64_sha_fails(self):
        fields = dict(_GOOD_RELEASE_FIELDS)
        fields["VERDANDI_SOURCE_SHA256"] = "too-short"
        with self.assertRaisesRegex(rc.ReleaseCheckError, "VERDANDI_SOURCE_SHA256"):
            rc.validate_release(fields)

    def test_a_bad_version_fails(self):
        fields = dict(_GOOD_RELEASE_FIELDS)
        fields["EITRI_VERSION"] = "v1.0"
        with self.assertRaisesRegex(rc.ReleaseCheckError, "EITRI_VERSION"):
            rc.validate_release(fields)

    def test_a_bad_verdandi_source_name_fails(self):
        fields = dict(_GOOD_RELEASE_FIELDS)
        fields["VERDANDI_SOURCE"] = "verdandi-source.tar.gz"  # missing the rev7
        with self.assertRaisesRegex(rc.ReleaseCheckError, "VERDANDI_SOURCE\\b"):
            rc.validate_release(fields)

    def test_every_problem_is_reported_together(self):
        fields = {"EITRI_VERSION": "bad"}
        with self.assertRaises(rc.ReleaseCheckError) as ctx:
            rc.validate_release(fields)
        message = str(ctx.exception)
        self.assertIn("missing field", message)
        self.assertIn("EITRI_VERSION", message)

    def test_parse_release_text_round_trips_a_real_file_shape(self):
        text = "\n".join(f"{k}={v}" for k, v in _GOOD_RELEASE_FIELDS.items()) + "\n"
        fields = rc.parse_release_text(text)
        self.assertEqual(fields, _GOOD_RELEASE_FIELDS)
        rc.validate_release(fields)  # must not raise

    def test_parse_release_text_skips_blank_lines_and_comments(self):
        text = "# a comment\n\nEITRI_VERSION=1.0.0\n"
        self.assertEqual(rc.parse_release_text(text), {"EITRI_VERSION": "1.0.0"})

    def test_a_line_with_no_equals_fails_to_parse(self):
        with self.assertRaises(rc.ReleaseCheckError):
            rc.parse_release_text("NOT_A_KV_LINE\n")

    def test_version_regex_agrees_with_the_installers_own(self):
        """The installer's own version regex lives in packaging/install.sh's NV_VERSION_ERE
        (owned by another lane's Task 11, never edited here) -- read back, not retyped, so a
        change there that this file's VERSION_RE does not follow is caught rather than assumed
        away. Skips (does not fail the suite) if that line is not found in its current shape,
        since this lane must not depend on another lane's in-flight edits."""
        install_sh = os.path.join(_PACKAGING, "install.sh")
        if not os.path.isfile(install_sh):
            self.skipTest("packaging/install.sh not present in this checkout")
        with open(install_sh, encoding="utf-8") as f:
            text = f.read()
        import re
        m = re.search(r"^NV_VERSION_ERE='([^']*)'", text, re.M)
        if not m:
            self.skipTest("NV_VERSION_ERE not found in packaging/install.sh in its expected shape")
        installer_re = re.compile("^" + m.group(1) + "$")
        cases = ["1.0.0", "1.0.0-rc.1", "0.11.2", "v1.0.0", "1.0", "1.0.0-beta", "1.0.0-rc.1.2"]
        for case in cases:
            self.assertEqual(
                bool(installer_re.match(case)), bool(rc.VERSION_RE.match(case)),
                f"installer and release_check disagree on version string {case!r}",
            )


# --- 2. Asset names (M7) --------------------------------------------------------------------

class AssetNameTests(unittest.TestCase):
    def test_the_nine_names_of_spec_section_4_3(self):
        names = rc.asset_names("1.0.0-rc.1", "a2f194a")
        self.assertEqual(names, [
            "eitri-1.0.0-rc.1-x86_64-linux.tar.gz",
            "eitri_1.0.0-rc.1_amd64.deb",
            "eitri-1.0.0-rc.1-1.x86_64.rpm",
            "eitri-1.0.0-rc.1-source.tar.gz",
            "verdandi-a2f194a-source.tar.gz",
            "install.sh",
            "RELEASE",
            "SHA256SUMS",
            "SHA256SUMS.sig",
        ])
        self.assertEqual(len(names), 9)
        for name in names:
            rc.validate_asset_name(name)  # must not raise

    def test_a_bare_release_version_also_works(self):
        names = rc.asset_names("1.0.0", "0000000")
        self.assertIn("eitri-1.0.0-x86_64-linux.tar.gz", names)

    def test_a_bad_version_is_refused(self):
        with self.assertRaises(rc.ReleaseCheckError):
            rc.asset_names("v1.0.0", "a2f194a")

    def test_a_bad_verdandi_rev_is_refused(self):
        with self.assertRaises(rc.ReleaseCheckError):
            rc.asset_names("1.0.0", "a2f194")  # 6 hex, not 7

    def test_a_name_with_a_space_fails_validation(self):
        with self.assertRaises(rc.ReleaseCheckError):
            rc.validate_asset_name("eitri 1.0.0.tar.gz")

    def test_a_name_with_a_slash_fails_validation(self):
        with self.assertRaises(rc.ReleaseCheckError):
            rc.validate_asset_name("../escape.tar.gz")


# --- 3. The printed `gh release create` line --------------------------------------------------

class GhReleaseCommandTests(unittest.TestCase):
    REPO = "HunterGrey-cyber/eitri"

    def argv(self, version, assets=("a.tar.gz",)):
        return rc.gh_release_command(version, list(assets), "Title", "notes.md", self.REPO)

    def test_verify_tag_is_always_present(self):
        self.assertIn("--verify-tag", self.argv("1.0.0"))

    def test_prerelease_only_for_an_rc(self):
        self.assertIn("--prerelease", self.argv("1.0.0-rc.1"))
        self.assertNotIn("--prerelease", self.argv("1.0.0"))

    def test_the_tag_is_v_prefixed_and_assets_are_positional(self):
        argv = self.argv("1.0.0", ["a.tar.gz", "b.deb"])
        self.assertIn("v1.0.0", argv)
        self.assertIn("a.tar.gz", argv)
        self.assertIn("b.deb", argv)

    def test_the_repository_is_always_named(self):
        # gh otherwise takes its repository from the current directory's remotes, and release.sh
        # runs from the private checkout (Task 4 review).
        argv = self.argv("1.0.0-rc.1")
        self.assertEqual(argv[argv.index("--repo") + 1], self.REPO)

    def test_a_bad_version_or_repository_is_refused(self):
        with self.assertRaises(rc.ReleaseCheckError):
            rc.gh_release_command("not-a-version", [], "Title", "notes.md", self.REPO)
        for repo in ("", "eitri", "https://github.com/HunterGrey-cyber/eitri", "a/b c"):
            with self.assertRaises(rc.ReleaseCheckError):
                rc.gh_release_command("1.0.0", [], "Title", "notes.md", repo)

    def test_the_cli_needs_the_repository_and_prints_one_shell_line(self):
        script = os.path.join(_PACKAGING, "release_check.py")
        base = ["python3", script, "gh-release-command", "1.0.0-rc.1", "--title", "Eitri 1.0.0-rc.1",
                "--notes-file", "n.md", "a.tar.gz"]
        missing = subprocess.run(base, capture_output=True, text=True)
        self.assertEqual(missing.returncode, 2, missing.stderr)
        ok = subprocess.run(base + ["--repo", self.REPO], capture_output=True, text=True)
        self.assertEqual(ok.returncode, 0, ok.stderr)
        self.assertEqual(ok.stdout, "gh release create v1.0.0-rc.1 --repo HunterGrey-cyber/eitri a.tar.gz "
                                    "--verify-tag --prerelease --title 'Eitri 1.0.0-rc.1' --notes-file n.md\n")


# --- 4. Extraction plus content scans -----------------------------------------------------------

class TarGzScanTests(unittest.TestCase):
    """Every content-scan rule, over a real gzip-compressed tarball extracted with
    release_check.extract_tar_gz -- the same tool spec sec 4.2 step 9 names for that asset type."""

    def setUp(self):
        self.dest = _scratch_dir()
        self.addCleanup(shutil.rmtree, self.dest, ignore_errors=True)

    def _extract(self, files):
        archive = os.path.join(_scratch_dir(), "fixture.tar.gz")
        _make_tar_gz(archive, files)
        rc.extract_tar_gz(archive, self.dest)
        return rc.iter_files(self.dest)

    def test_the_sea_sentinel_is_found_by_content_whatever_the_file_is_named(self):
        payload = (rc.SEA_SENTINEL + " padding padding padding").encode("ascii")
        files = self._extract([
            ("lib/eitri/helper-utility", payload),  # a deliberately innocuous name (M5/step 9)
            ("lib/eitri/shell", b"not the sidecar at all"),
        ])
        hits = rc.find_sentinel_hits(files)
        self.assertEqual(hits, ["lib/eitri/helper-utility"])

    def test_a_file_with_no_sentinel_is_not_a_hit(self):
        files = self._extract([("lib/eitri/shell", b"an ordinary binary, no sentinel here")])
        self.assertEqual(rc.find_sentinel_hits(files), [])

    def test_anthropic_node_modules_path_is_found(self):
        files = self._extract([
            ("verdandi/node_modules/@anthropic-ai/claude-agent-sdk/index.js", b"x"),
            ("verdandi/node_modules/left-pad/index.js", b"x"),
        ])
        hits = rc.find_anthropic_node_modules(files)
        self.assertEqual(hits, ["verdandi/node_modules/@anthropic-ai/claude-agent-sdk/index.js"])

    def test_any_node_modules_at_all_is_found_in_a_source_asset(self):
        files = self._extract([("verdandi/node_modules/left-pad/index.js", b"x")])
        self.assertEqual(rc.find_node_modules_dirs(files), ["verdandi/node_modules/left-pad/index.js"])

    def test_a_verdandi_claude_sidecar_named_file_is_found(self):
        files = self._extract([
            ("lib/eitri/verdandi-claude-sidecar", b"x"),
            ("lib/eitri/verdandi-claude-sidecar-linux-x64", b"x"),
            ("lib/eitri/shell", b"x"),
        ])
        hits = rc.find_verdandi_sidecar_filenames(files)
        self.assertEqual(sorted(hits), [
            "lib/eitri/verdandi-claude-sidecar",
            "lib/eitri/verdandi-claude-sidecar-linux-x64",
        ])

    def test_an_exact_agent_hook_basename_is_found_but_the_source_file_is_not(self):
        files = self._extract([
            ("lib/eitri/agent-hook", b"a compiled legacy-gate relay binary"),
            ("src/agent/src/bin/agent-hook.rs", b"fn main() {}"),  # M16: source file is fine
        ])
        hits = rc.find_agent_hook_binaries(files)
        self.assertEqual(hits, ["lib/eitri/agent-hook"])

    def test_a_home_path_inside_an_elf_is_found(self):
        elf_payload = b"\x7fELF" + b"\x00" * 12 + b"/home/someuser/.cargo/registry/src/foo.rs\x00"
        files = self._extract([("lib/eitri/shell", elf_payload)])
        hits = rc.find_home_paths_in_elves(files)
        self.assertIn("lib/eitri/shell", hits)
        self.assertTrue(any("/home/someuser" in m for m in hits["lib/eitri/shell"]))

    def test_a_home_path_inside_a_non_elf_file_is_not_reported(self):
        """M3's own narrowing: the check runs over extracted ELF files, not every extracted
        shipped file -- a text file (a doc, install.sh itself) legitimately mentions such paths in
        prose and examples."""
        files = self._extract([("lib/eitri/README.txt", b"see /home/someuser/project for an example")])
        self.assertEqual(rc.find_home_paths_in_elves(files), {})

    def test_users_and_root_paths_are_also_matched(self):
        payload = b"\x7fELF" + b"\x00" * 12 + b"C:/Users/dev/build /root/.cargo\x00"
        files = self._extract([("lib/eitri/shell", payload)])
        hits = rc.find_home_paths_in_elves(files)
        self.assertIn("lib/eitri/shell", hits)

    def test_an_sdk_mention_is_found_unless_allowed(self):
        files = self._extract([
            ("lib/eitri/eitri-setup", b"this build fetches claude-agent-sdk on your own machine\n"),
            ("lib/eitri/shell", b"claude-agent-sdk should not be inside a compiled binary either"),
        ])
        all_hits = rc.find_sdk_mentions(files)
        self.assertEqual(sorted(all_hits), ["lib/eitri/eitri-setup", "lib/eitri/shell"])

        allowed_hits = rc.find_sdk_mentions(files, is_allowed=lambda rel: rel.endswith("eitri-setup"))
        self.assertEqual(allowed_hits, ["lib/eitri/shell"])

    def test_anthropic_pbc_is_also_a_marker(self):
        files = self._extract([("lib/eitri/shell", b"Copyright Anthropic PBC, embedded")])
        self.assertEqual(rc.find_sdk_mentions(files), ["lib/eitri/shell"])

    def test_install_sh_mentioning_the_sdk_passes_when_allowed(self):
        """The one allowed-fixture case Task 1's own brief names by name: eitri-setup/install.sh
        mentioning the SDK passes (M2)."""
        files = self._extract([
            ("install.sh", b"# fetches and builds claude-agent-sdk locally, per I2\n"),
            ("lib/eitri/eitri-setup", b"# fetches and builds claude-agent-sdk locally, per I2\n"),
        ])
        allow = {"install.sh", "lib/eitri/eitri-setup"}
        self.assertEqual(rc.find_sdk_mentions(files, is_allowed=lambda rel: rel in allow), [])


class DebExtractionTests(unittest.TestCase):
    """The ar-writer fixture + the host's real dpkg-deb -x (M8's tool list)."""

    def test_a_real_deb_shaped_archive_extracts_and_scans(self):
        dest = _scratch_dir()
        self.addCleanup(shutil.rmtree, dest, ignore_errors=True)
        archive = os.path.join(_scratch_dir(), "fixture.deb")
        payload = (rc.SEA_SENTINEL + " padding padding padding").encode("ascii")
        _make_deb(archive, [
            ("/usr/lib/eitri/helper-utility", payload),
            ("/usr/lib/eitri/eitri-setup", b"fetches claude-agent-sdk on your own machine\n"),
        ])
        rc.extract_deb(archive, dest)
        files = rc.iter_files(dest)
        self.assertEqual(rc.find_sentinel_hits(files), ["usr/lib/eitri/helper-utility"])
        sdk_hits = rc.find_sdk_mentions(files, is_allowed=lambda rel: rel.endswith("eitri-setup"))
        self.assertEqual(sdk_hits, [])

    def test_a_clean_deb_has_no_hits(self):
        dest = _scratch_dir()
        self.addCleanup(shutil.rmtree, dest, ignore_errors=True)
        archive = os.path.join(_scratch_dir(), "fixture.deb")
        _make_deb(archive, [("/usr/lib/eitri/shell", b"an ordinary compiled binary\n")])
        rc.extract_deb(archive, dest)
        files = rc.iter_files(dest)
        self.assertEqual(rc.find_sentinel_hits(files), [])
        self.assertEqual(rc.find_agent_hook_binaries(files), [])
        self.assertEqual(rc.find_verdandi_sidecar_filenames(files), [])


@unittest.skipUnless(_NFPM, "nfpm not on PATH")
class RpmExtractionTests(unittest.TestCase):
    """The host's real nfpm 2.47.0 building a real .rpm, then the host's real bsdtar -xf (M8's
    tool list, Task 12 pre-think Section 5 step 1)."""

    def test_a_real_rpm_extracts_and_scans(self):
        dest = _scratch_dir()
        self.addCleanup(shutil.rmtree, dest, ignore_errors=True)
        archive = os.path.join(_scratch_dir(), "fixture.rpm")
        _make_rpm(archive, [
            ("/usr/lib/eitri/verdandi-claude-sidecar", b"a renamed-in-fixture sidecar shape"),
            ("/usr/lib/eitri/shell", b"an ordinary compiled binary"),
        ])
        rc.extract_rpm(archive, dest)
        files = rc.iter_files(dest)
        self.assertEqual(rc.find_verdandi_sidecar_filenames(files), ["usr/lib/eitri/verdandi-claude-sidecar"])

    def test_a_clean_rpm_has_no_agent_hook(self):
        dest = _scratch_dir()
        self.addCleanup(shutil.rmtree, dest, ignore_errors=True)
        archive = os.path.join(_scratch_dir(), "fixture.rpm")
        _make_rpm(archive, [("/usr/lib/eitri/shell", b"an ordinary compiled binary")])
        rc.extract_rpm(archive, dest)
        files = rc.iter_files(dest)
        self.assertEqual(rc.find_agent_hook_binaries(files), [])


# --- 5. Tree equality against `git archive` (M2) -------------------------------------------------

class TreeEqualityTests(unittest.TestCase):
    def test_an_exact_extraction_matches_the_verdandi_style_asset(self):
        repo, head = _init_git_repo({"README.md": b"hello\n", "src/lib.rs": b"fn f() {}\n"})
        extracted = _scratch_dir()
        self.addCleanup(shutil.rmtree, extracted, ignore_errors=True)
        for relpath, data in [("README.md", b"hello\n"), ("src/lib.rs", b"fn f() {}\n")]:
            path = os.path.join(extracted, relpath)
            os.makedirs(os.path.dirname(path), exist_ok=True)
            with open(path, "wb") as f:
                f.write(data)
        rc.compare_tree_to_git_archive(extracted, repo, head)  # must not raise

    def test_a_missing_file_is_reported(self):
        repo, head = _init_git_repo({"README.md": b"hello\n", "src/lib.rs": b"fn f() {}\n"})
        extracted = _scratch_dir()
        self.addCleanup(shutil.rmtree, extracted, ignore_errors=True)
        with open(os.path.join(extracted, "README.md"), "wb") as f:
            f.write(b"hello\n")
        with self.assertRaises(rc.TreeMismatch) as ctx:
            rc.compare_tree_to_git_archive(extracted, repo, head)
        self.assertIn("src/lib.rs", ctx.exception.missing)

    def test_a_content_mismatch_is_reported(self):
        repo, head = _init_git_repo({"README.md": b"hello\n"})
        extracted = _scratch_dir()
        self.addCleanup(shutil.rmtree, extracted, ignore_errors=True)
        with open(os.path.join(extracted, "README.md"), "wb") as f:
            f.write(b"TAMPERED\n")
        with self.assertRaises(rc.TreeMismatch) as ctx:
            rc.compare_tree_to_git_archive(extracted, repo, head)
        self.assertIn("README.md", ctx.exception.mismatched)

    def test_an_unlisted_extra_file_is_reported_unexpected(self):
        repo, head = _init_git_repo({"README.md": b"hello\n"})
        extracted = _scratch_dir()
        self.addCleanup(shutil.rmtree, extracted, ignore_errors=True)
        with open(os.path.join(extracted, "README.md"), "wb") as f:
            f.write(b"hello\n")
        with open(os.path.join(extracted, "sneaked-in.txt"), "wb") as f:
            f.write(b"should not be here\n")
        with self.assertRaises(rc.TreeMismatch) as ctx:
            rc.compare_tree_to_git_archive(extracted, repo, head)
        self.assertIn("sneaked-in.txt", ctx.exception.unexpected)

    def test_the_eitri_source_assets_known_additions_are_allowed(self):
        """M2's ruling: the Eitri source asset equals `git archive HEAD` except exactly
        vendor/, skia/, neovide/, proto/, .cargo/config.toml, agent-ui/web/dist/index.html,
        agent-ui/web/dist/.inputs-sha256 (v1-dist plan Task 6, P4-A1), THIRD-PARTY-LICENSES and
        SOURCE."""
        repo, head = _init_git_repo({"core/lib.rs": b"fn f() {}\n"})
        extracted = _scratch_dir()
        self.addCleanup(shutil.rmtree, extracted, ignore_errors=True)
        additions = {
            "core/lib.rs": b"fn f() {}\n",
            "vendor/some-crate/Cargo.toml": b"[package]\n",
            "skia/skia-binaries-x.tar.gz": b"not really an archive",
            "neovide/Cargo.toml": b"[package]\n",
            "proto/verdandi/claude/runtime/v1/runtime.proto": b"syntax = \"proto3\";\n",
            ".cargo/config.toml": b"[source.crates-io]\n",
            "agent-ui/web/dist/index.html": b"<html></html>\n",
            "agent-ui/web/dist/.inputs-sha256": b"a" * 64 + b"\n",
            "THIRD-PARTY-LICENSES": b"...\n",
            "SOURCE": b"...\n",
        }
        for relpath, data in additions.items():
            path = os.path.join(extracted, relpath)
            os.makedirs(os.path.dirname(path), exist_ok=True)
            with open(path, "wb") as f:
                f.write(data)
        rc.compare_tree_to_git_archive(extracted, repo, head, rc.KNOWN_EITRI_SOURCE_ADDITIONS)  # not raise

    def test_an_addition_outside_the_known_list_still_fails(self):
        repo, head = _init_git_repo({"core/lib.rs": b"fn f() {}\n"})
        extracted = _scratch_dir()
        self.addCleanup(shutil.rmtree, extracted, ignore_errors=True)
        os.makedirs(os.path.join(extracted, "core"))
        with open(os.path.join(extracted, "core/lib.rs"), "wb") as f:
            f.write(b"fn f() {}\n")
        os.makedirs(os.path.join(extracted, "not-a-known-addition"))
        with open(os.path.join(extracted, "not-a-known-addition", "x"), "wb") as f:
            f.write(b"x\n")
        with self.assertRaises(rc.TreeMismatch) as ctx:
            rc.compare_tree_to_git_archive(extracted, repo, head, rc.KNOWN_EITRI_SOURCE_ADDITIONS)
        self.assertIn("not-a-known-addition/x", ctx.exception.unexpected)


# --- 6. The scan view (M4) ----------------------------------------------------------------------

class ScanViewTests(unittest.TestCase):
    def test_vendor_skia_and_proto_are_excluded_at_the_top_level(self):
        root = _scratch_dir()
        self.addCleanup(shutil.rmtree, root, ignore_errors=True)
        for relpath in ["vendor/some-crate/Cargo.toml", "skia/archive.tar.gz",
                         "proto/verdandi/runtime.proto", "core/lib.rs", "README.md"]:
            path = os.path.join(root, relpath)
            os.makedirs(os.path.dirname(path), exist_ok=True)
            with open(path, "wb") as f:
                f.write(b"x")
        view = rc.scan_view(root)
        self.assertEqual(view, ["README.md", "core/lib.rs"])

    def test_a_same_named_directory_nested_deeper_is_not_excluded(self):
        root = _scratch_dir()
        self.addCleanup(shutil.rmtree, root, ignore_errors=True)
        path = os.path.join(root, "src", "vendor", "notice.txt")
        os.makedirs(os.path.dirname(path), exist_ok=True)
        with open(path, "wb") as f:
            f.write(b"x")
        self.assertEqual(rc.scan_view(root), ["src/vendor/notice.txt"])


# --- 7. Legacy compiled out, by symbol (M9) -------------------------------------------------------

class LegacySymbolTests(unittest.TestCase):
    _GOOD_NM = (
        "0000000000012340 T agent::providers::claude_sidecar::spawn::hd3f2\n"
        "0000000000012350 T agent::providers::claude_sidecar::mod::h9a1c\n"
        "0000000000012360 T agent::agent_backend::choose::h1234\n"
    )

    def test_the_sidecar_only_build_passes(self):
        rc.check_no_legacy_symbols(self._GOOD_NM)  # must not raise

    def test_a_legacy_symbol_present_fails(self):
        nm = self._GOOD_NM + "0000000000012370 T agent::session::AgentSession::new::habcd\n"
        with self.assertRaisesRegex(rc.ReleaseCheckError, "agent::session::AgentSession"):
            rc.check_no_legacy_symbols(nm)

    def test_the_other_legacy_symbol_also_fails(self):
        nm = self._GOOD_NM + "0000000000012380 T agent::process::AgentProcess::spawn::hbeef\n"
        with self.assertRaisesRegex(rc.ReleaseCheckError, "agent::process::AgentProcess"):
            rc.check_no_legacy_symbols(nm)

    def test_no_sidecar_symbol_at_all_fails_even_with_no_legacy_symbols(self):
        with self.assertRaises(rc.ReleaseCheckError) as ctx:
            rc.check_no_legacy_symbols("0000000000012340 T some::other::thing::h1\n")
        self.assertIn(rc.SIDECAR_SYMBOL_MARKER, str(ctx.exception))

    def test_an_empty_nm_output_does_not_vacuously_pass(self):
        with self.assertRaises(rc.ReleaseCheckError):
            rc.check_no_legacy_symbols("")


# --- 8. The M3/M5 no-op: rewrites.sed is a no-op, and the sentinel never appears literally --------

# --- Task 4's additions: what release.sh itself needs from this module ---------------------------

def _load_collector():
    spec = importlib.util.spec_from_file_location("collect_licenses", os.path.join(_PACKAGING, "collect-licenses.py"))
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


class RehearsalReleaseTests(unittest.TestCase):
    def rehearsal_fields(self, **changes):
        fields = dict(_GOOD_RELEASE_FIELDS, REHEARSAL="1")
        for key in rc.REHEARSAL_OPTIONAL_FIELDS:
            del fields[key]
        fields.update(changes)
        return fields

    def test_a_rehearsal_without_the_nvim_pin_passes(self):
        rc.validate_release(self.rehearsal_fields(), rehearsal=True)

    def test_a_rehearsal_with_the_nvim_pin_passes(self):
        fields = dict(_GOOD_RELEASE_FIELDS, REHEARSAL="1")
        rc.validate_release(fields, rehearsal=True)

    def test_a_rehearsal_with_only_one_nvim_field_fails(self):
        fields = self.rehearsal_fields(NVIM_VERSION="0.11.2")
        with self.assertRaisesRegex(rc.ReleaseCheckError, "both nvim fields or neither"):
            rc.validate_release(fields, rehearsal=True)

    def test_a_rehearsal_must_say_so(self):
        fields = self.rehearsal_fields()
        del fields["REHEARSAL"]
        with self.assertRaisesRegex(rc.ReleaseCheckError, "REHEARSAL"):
            rc.validate_release(fields, rehearsal=True)
        with self.assertRaisesRegex(rc.ReleaseCheckError, "is not '1'"):
            rc.validate_release(self.rehearsal_fields(REHEARSAL="yes"), rehearsal=True)

    def test_a_real_release_never_carries_the_rehearsal_mark_or_leaves_out_nvim(self):
        with self.assertRaisesRegex(rc.ReleaseCheckError, "unexpected field"):
            rc.validate_release(dict(_GOOD_RELEASE_FIELDS, REHEARSAL="1"))
        with self.assertRaisesRegex(rc.ReleaseCheckError, "missing field"):
            rc.validate_release(self.rehearsal_fields(REHEARSAL="1") | {"REHEARSAL": "1"})


def _artifact(path, executable=True):
    return json.dumps({"reason": "compiler-artifact", "package_id": "x", "executable": path if executable else None,
                       "filenames": [path]})


class BuildOutputTests(unittest.TestCase):
    FOUR = [f"/build/target/release/{b}" for b in rc.RELEASE_BINARIES]

    def test_exactly_the_four_executables_in_release_order(self):
        lines = ["Compiling ...", _artifact("/build/target/release/libagent.rlib", executable=False)]
        lines += [_artifact(p) for p in reversed(self.FOUR)]
        self.assertEqual(rc.release_executables(lines), self.FOUR)

    def test_an_agent_hook_in_the_build_fails(self):
        lines = [_artifact(p) for p in self.FOUR] + [_artifact("/build/target/release/agent-hook")]
        with self.assertRaisesRegex(rc.ReleaseCheckError, "agent-hook"):
            rc.release_executables(lines)

    def test_a_missing_binary_fails(self):
        with self.assertRaisesRegex(rc.ReleaseCheckError, "did not build: eitri-tmux-shim"):
            rc.release_executables([_artifact(p) for p in self.FOUR if not p.endswith("tmux-shim")])

    def test_the_skia_bindings_out_dir_in_either_package_id_spelling(self):
        for pid in ("registry+https://github.com/rust-lang/crates.io-index#skia-bindings@0.153.3",
                    "skia-bindings 0.153.3 (registry+https://github.com/rust-lang/crates.io-index)"):
            lines = [json.dumps({"reason": "build-script-executed", "package_id": pid, "out_dir": "/o/skia/out"}),
                     json.dumps({"reason": "build-script-executed", "package_id": "a#skia-safe@0.1", "out_dir": "/o/x"})]
            self.assertEqual(rc.build_script_out_dir(lines, "skia-bindings"), "/o/skia/out")

    def test_no_or_two_out_dirs_fail(self):
        with self.assertRaises(rc.ReleaseCheckError):
            rc.build_script_out_dir([], "skia-bindings")
        lines = [json.dumps({"reason": "build-script-executed", "package_id": "a#skia-bindings@1", "out_dir": d})
                 for d in ("/a", "/b")]
        with self.assertRaises(rc.ReleaseCheckError):
            rc.build_script_out_dir(lines, "skia-bindings")

    URL = "file:///build/skia/skia-binaries-k.tar.gz"

    def test_the_skia_output_shows_the_pinned_url_and_success(self):
        text = f"cargo:rerun-if-env-changed=X\nTRYING TO DOWNLOAD AND INSTALL SKIA BINARIES: 0.1/k\n  FROM: {self.URL}\nDOWNLOAD AND INSTALL SUCCEEDED\n"
        rc.check_skia_build_output(text, self.URL)

    def test_another_url_or_a_failure_fails(self):
        for text in (f"  FROM: https://github.com/x.tar.gz\nDOWNLOAD AND INSTALL SUCCEEDED\n",
                     f"  FROM: {self.URL}\nDOWNLOAD AND INSTALL FAILED: nope\n",
                     "no download at all\n"):
            with self.assertRaises(rc.ReleaseCheckError):
                rc.check_skia_build_output(text, self.URL)


class RelinkRecipeTests(unittest.TestCase):
    def source_text(self):
        collector = _load_collector()
        d = _scratch_dir()
        self.addCleanup(shutil.rmtree, d, ignore_errors=True)
        path = os.path.join(d, "SOURCE")
        return collector.write_source_notice(path, "1.0.0-rc.1", "a" * 40, "https://example.com/src.tar.gz",
                                             "skia-binaries-k.tar.gz", "b" * 64, "0.9.2", "c" * 40)

    def test_the_recipe_the_collector_writes_parses(self):
        recipe = rc.parse_relink_recipe(self.source_text())
        self.assertEqual(recipe.modified_dir, "nvim-rs-modified")
        self.assertEqual(recipe.copy, "cp -r vendor/nvim-rs nvim-rs-modified")
        self.assertEqual(recipe.unlock, "rm nvim-rs-modified/.cargo-checksum.json")
        self.assertEqual(recipe.patch, '[patch.crates-io]\nnvim-rs = { path = "nvim-rs-modified" }')
        self.assertIn('SKIA_BINARIES_URL="file://$PWD/skia/skia-binaries-k.tar.gz" cargo build', recipe.build)
        self.assertNotIn("--locked", recipe.build)

    def test_a_recipe_that_pins_the_lockfile_fails(self):
        text = self.source_text().replace("cargo build --release --offline -p shell",
                                          "cargo build --release --offline --locked -p shell")
        with self.assertRaisesRegex(rc.ReleaseCheckError, "step 5"):
            rc.parse_relink_recipe(text)

    def test_a_recipe_that_edits_vendor_in_place_fails(self):
        text = self.source_text().replace('nvim-rs = { path = "nvim-rs-modified" }', 'nvim-rs = { path = "vendor/nvim-rs" }')
        with self.assertRaisesRegex(rc.ReleaseCheckError, "step 4"):
            rc.parse_relink_recipe(text)

    def test_no_recipe_fails(self):
        with self.assertRaises(rc.ReleaseCheckError):
            rc.parse_relink_recipe("nothing here\n")

    def test_the_rebuild_env_the_collector_writes_parses(self):
        # Whole-branch review (lane D): proof (a) sets exactly these before its offline rebuild, so
        # the rebuilt --version names the fork commit the shipped one does, not "unknown".
        self.assertEqual(rc.parse_rebuild_env(self.source_text()),
                         {"EITRI_BUILD_COMMIT": "a" * 40, "EITRI_BUILD_FORK_COMMIT": "c" * 40})

    def test_a_rebuild_env_missing_doubled_short_or_unknown_fails(self):
        text = self.source_text()
        fork_line = "    EITRI_BUILD_FORK_COMMIT=" + "c" * 40 + "\n"
        self.assertIn(fork_line, text)
        for broken, why in ((text.replace(fork_line, ""), "0 times"),
                            (text.replace(fork_line, fork_line * 2), "2 times"),
                            (text.replace(fork_line, "    EITRI_BUILD_FORK_COMMIT=ccccccc\n"), "not a full commit"),
                            (text.replace(fork_line, fork_line + "    EITRI_BUILD_OTHER=" + "d" * 40 + "\n"),
                             "unexpected EITRI_BUILD_OTHER")):
            with self.subTest(why=why), self.assertRaisesRegex(rc.ReleaseCheckError, why):
                rc.parse_rebuild_env(broken)

    def test_lock_sources(self):
        lock = ('version = 4\n\n[[package]]\nname = "nvim-rs"\nversion = "0.9.2"\n\n'
                '[[package]]\nname = "x"\nversion = "1.0.0"\nsource = "registry+https://example.com"\n')
        self.assertEqual(rc.lock_package_sources(lock, "nvim-rs"), [None])
        self.assertEqual(rc.lock_package_sources(lock, "x"), ["registry+https://example.com"])
        self.assertEqual(rc.lock_package_sources(lock, "absent"), [])


class ScanViewOnDiskTests(unittest.TestCase):
    def test_the_view_leaves_out_the_top_level_hash_checked_trees_only(self):
        src = _scratch_dir()
        dest_parent = _scratch_dir()
        self.addCleanup(shutil.rmtree, src, ignore_errors=True)
        self.addCleanup(shutil.rmtree, dest_parent, ignore_errors=True)
        for rel in ("vendor/a/lib.rs", "skia/s.tar.gz", "proto/p.proto", "src/vendor/keep.rs", "README.md"):
            path = os.path.join(src, rel)
            os.makedirs(os.path.dirname(path), exist_ok=True)
            with open(path, "w") as f:
                f.write(rel)
        os.symlink("README.md", os.path.join(src, "link"))
        dest = os.path.join(dest_parent, "view")
        self.assertEqual(rc.make_scan_view(src, dest), 3)
        self.assertEqual(sorted(rel for rel, _ in rc.iter_files(dest)), ["README.md", "link", "src/vendor/keep.rs"])
        self.assertTrue(os.path.islink(os.path.join(dest, "link")))
        with self.assertRaises(rc.ReleaseCheckError):
            rc.make_scan_view(src, dest)

    def test_a_directory_scan_sh_skips_by_name_is_renamed_so_it_is_scanned(self):
        # The source asset's built web bundle sits in agent-ui/web/dist/, and publish/scan.sh skips
        # every dist/ (Task 4 review).
        src = _scratch_dir()
        dest_parent = _scratch_dir()
        self.addCleanup(shutil.rmtree, src, ignore_errors=True)
        self.addCleanup(shutil.rmtree, dest_parent, ignore_errors=True)
        _plant(src, {"agent-ui/web/dist/index.html": b"<script>/* /home/someone/x */</script>\n",
                     "a/target/b/node_modules/c.js": b"c\n", "dist": b"a file, not a directory\n"})
        dest = os.path.join(dest_parent, "view")
        self.assertEqual(rc.make_scan_view(src, dest), 3)
        self.assertEqual(sorted(rel for rel, _ in rc.iter_files(dest)),
                         ["a/target.scanned/b/node_modules.scanned/c.js",
                          "agent-ui/web/dist.scanned/index.html", "dist"])
        scan = os.path.join(os.path.dirname(_PACKAGING), "publish", "scan.sh")
        if not os.path.isfile(scan):
            self.skipTest("publish/scan.sh is not in this checkout")
        ident = _private_identifiers(scan)[0]
        _plant(src, {"agent-ui/web/dist/index.html": f"<script>/* {ident}/x */</script>\n".encode()})
        leak = os.path.join(dest_parent, "leak")
        rc.make_scan_view(src, leak)
        proc = subprocess.run(["bash", scan, leak], capture_output=True, text=True)
        self.assertEqual(proc.returncode, 1, proc.stderr)
        self.assertIn("agent-ui/web/dist.scanned/index.html", proc.stdout)

    def test_the_fork_checkout_scan_sh_skips_at_the_top_is_renamed_so_it_is_scanned(self):
        # publish/scan.sh skips a top-level neovide/ (a public tree's submodule checkout); in the
        # source asset it is the pinned fork's source, shipped by this release (whole-branch review,
        # codex). Only the top level: scan.sh reads a nested neovide/ already.
        src = _scratch_dir()
        dest_parent = _scratch_dir()
        _plant(src, {"neovide/src/lib.rs": b"// fork\n", "src/neovide/keep.rs": b"// nested\n",
                     "neovide/assets/README.md": b"![bg](neovide-dmg-background@2x.png)\n"})
        dest = os.path.join(dest_parent, "view")
        self.assertEqual(rc.make_scan_view(src, dest), 3)
        self.assertEqual(sorted(rel for rel, _ in rc.iter_files(dest)),
                         ["neovide.scanned/assets/README.md", "neovide.scanned/src/lib.rs", "src/neovide/keep.rs"])
        scan = os.path.join(os.path.dirname(_PACKAGING), "publish", "scan.sh")
        if not os.path.isfile(scan):
            self.skipTest("publish/scan.sh is not in this checkout")
        # The fork's retina image names (name@2x.png) are not addresses: the clean view passes.
        proc = subprocess.run(["bash", scan, dest], capture_output=True, text=True)
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        ident = _private_identifiers(scan)[0]
        for planted, hit in ((f"// {ident}/x\n".encode(), ident),
                             (b"// someone@" + b"mail.co.uk\n", "[email] someone@mail.co.uk")):
            with self.subTest(hit=hit):
                _plant(src, {"neovide/src/lib.rs": planted})
                leak = _scratch_dir()
                rc.make_scan_view(src, os.path.join(leak, "view"))
                proc = subprocess.run(["bash", scan, os.path.join(leak, "view")], capture_output=True, text=True)
                self.assertEqual(proc.returncode, 1, proc.stdout + proc.stderr)
                self.assertIn("neovide.scanned/src/lib.rs", proc.stdout)
                self.assertIn(hit, proc.stdout)


class ThirdPartyLicensesScanAllowTests(unittest.TestCase):
    """publish/scan-allow.txt's THIRD-PARTY-LICENSES entry removes third-party addresses before the
    other rules run; an address carrying one of scan.sh's own private identifiers must still hit
    (Task 4 review). The identifiers come from scan-allow.txt itself (_private_identifiers), never
    from this file, which ships: every one is checked, and nothing private is spelled here, whole or
    in pieces. The third-party addresses are invented, and pieced so the email rule passes this file."""

    def scan(self, text):
        scan = os.path.join(os.path.dirname(_PACKAGING), "publish", "scan.sh")
        if not os.path.isfile(scan):
            self.skipTest("publish/scan.sh is not in this checkout")
        root = _scratch_dir()
        self.addCleanup(shutil.rmtree, root, ignore_errors=True)
        _plant(root, {"share/licenses/eitri/THIRD-PARTY-LICENSES": text.encode()})
        return subprocess.run(["bash", scan, root], capture_output=True, text=True)

    def test_third_party_addresses_pass(self):
        proc = self.scan("Copyright Jane Doe <jane.doe@" + "mail.co.uk>, <a+b@" + "gzip.org>\n")
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)

    def test_an_address_carrying_a_private_identifier_still_hits(self):
        scan = os.path.join(os.path.dirname(_PACKAGING), "publish", "scan.sh")
        if not os.path.isfile(scan):
            self.skipTest("publish/scan.sh is not in this checkout")
        addresses = [shape.format(ident) for ident in _private_identifiers(scan)
                     for shape in ("someone@{}.example", "{}@mail.example", "x.{}@mail.example")]
        for address in addresses:
            with self.subTest(address=address):
                proc = self.scan(f"Author: <{address}>, <jane@example.org>\n")
                self.assertEqual(proc.returncode, 1, proc.stdout + proc.stderr)
                self.assertIn(f"[email] {address}", proc.stdout)


def _private_identifiers(scan):
    """The private identifiers publish/scan-allow.txt's THIRD-PARTY-LICENSES entry never lets pass
    (its `(?i:...)` lookahead), plain words only: the tests below plant them, read at run time from
    beside scan.sh, so this shipped file never carries one."""
    with open(os.path.join(os.path.dirname(scan), "scan-allow.txt"), encoding="utf-8") as f:
        line = next(l for l in f if l.startswith("*THIRD-PARTY-LICENSES\t"))
    group = re.search(r"\(\?i:([^)]*)\)", line).group(1)
    idents = [w for w in group.split("|") if re.fullmatch(r"[a-z0-9-]+", w)]
    assert idents, line
    return idents


def _plant(root, files):
    for rel, data in files.items():
        path = os.path.join(root, rel)
        os.makedirs(os.path.dirname(path), exist_ok=True)
        with open(path, "wb") as f:
            f.write(data)


def _fake_elf(text=b""):
    return b"\x7fELF" + b"\0" * 12 + text


class BinaryAssetTreeTests(unittest.TestCase):
    def tree(self, roles, extra=None, overrides=None):
        root = _scratch_dir()
        self.addCleanup(shutil.rmtree, root, ignore_errors=True)
        files = {rel: (_fake_elf(role.encode()) if role in rc.RELEASE_BINARIES else role.encode())
                 for role, rel in roles.items()}
        files[roles["setup"]] = b"#!/bin/sh\n# downloads claude-agent-sdk on your machine\n"
        files.update(overrides or {})
        files.update(extra or {})
        _plant(root, files)
        return root

    def test_the_tarball_and_package_layouts_pass(self):
        for roles in (rc.tarball_roles("1.0.0-rc.1"), rc.package_roles(), rc.deb_roles()):
            self.assertEqual(rc.check_binary_asset("x", self.tree(roles), roles), [])

    def test_only_the_deb_carries_the_apparmor_profile(self):
        """docs/superpowers/plans/2026-09-28-v1-dist-ubuntu-userns.md: the .deb's layout is the
        .rpm's plus /etc/apparmor.d/eitri; a .deb without it, or an .rpm with it, fails."""
        self.assertEqual(rc.deb_roles(), {**rc.package_roles(), "apparmor": "etc/apparmor.d/eitri"})
        deb, rpm = rc.deb_roles(), rc.package_roles()
        root = self.tree(deb)
        os.remove(os.path.join(root, deb["apparmor"]))
        self.assertTrue(any("missing: etc/apparmor.d/eitri" in p for p in rc.check_binary_asset("x", root, deb)))
        problems = rc.check_binary_asset("x", self.tree(rpm, {"etc/apparmor.d/eitri": b"profile\n"}), rpm)
        self.assertTrue(any("files no release ships: etc/apparmor.d/eitri" in p for p in problems), problems)

    def test_an_extra_file_such_as_agent_hook_fails(self):
        roles = rc.package_roles()
        problems = rc.check_binary_asset("x", self.tree(roles, {"usr/lib/eitri/agent-hook": _fake_elf()}), roles)
        self.assertTrue(any("files no release ships" in p for p in problems), problems)
        self.assertTrue(any("agent-hook binary" in p for p in problems), problems)

    def test_a_missing_file_fails(self):
        roles = rc.package_roles()
        root = self.tree(roles)
        os.remove(os.path.join(root, roles["SOURCE"]))
        self.assertTrue(any("missing" in p for p in rc.check_binary_asset("x", root, roles)))

    def test_the_sdk_name_outside_eitri_setup_fails(self):
        roles = rc.package_roles()
        root = self.tree(roles, overrides={roles["shell"]: _fake_elf(b"claude-agent-sdk")})
        problems = rc.check_binary_asset("x", root, roles)
        self.assertEqual(len(problems), 1, problems)
        self.assertIn("usr/lib/eitri/shell", problems[0])

    def test_a_home_path_in_a_binary_fails(self):
        roles = rc.package_roles()
        root = self.tree(roles, overrides={roles["shell"]: _fake_elf(b"\0/home/someone/.cargo/registry/x.rs\0")})
        self.assertTrue(any("home-directory" in p for p in rc.check_binary_asset("x", root, roles)))

    def test_same_bytes(self):
        d = _scratch_dir()
        self.addCleanup(shutil.rmtree, d, ignore_errors=True)
        _plant(d, {"a": b"1", "b": b"1", "c": b"2"})
        j = lambda n: os.path.join(d, n)
        self.assertEqual(rc.check_same_bytes({"a": j("a"), "b": j("b")}), [])
        self.assertTrue(rc.check_same_bytes({"a": j("a"), "c": j("c")}))
        self.assertTrue(rc.check_same_bytes({"a": j("a"), "gone": j("gone")}))


_BUILD_BINARY = b"""const NODE_VERSION = 'v22.23.2';
const NODE_TARBALL_SHA256 = {
  'linux-x64': '""" + b"a" * 64 + b"""',
  'linux-arm64': '""" + b"b" * 64 + b"""',
};
const SEA_FUSE = '""" + rc.SEA_SENTINEL.encode() + b"""';
"""


class NodePinTests(unittest.TestCase):
    PINS = {"NODE_VERSION": "v22.23.2", "NODE_SHA256_linux_x64": "a" * 64, "NODE_SHA256_linux_arm64": "b" * 64}

    def test_equal_pins_pass(self):
        rc.check_node_pin(_BUILD_BINARY.decode(), self.PINS)

    def test_a_different_version_or_hash_fails(self):
        for change in ({"NODE_VERSION": "v22.23.3"}, {"NODE_SHA256_linux_x64": "c" * 64}):
            with self.assertRaises(rc.ReleaseCheckError):
                rc.check_node_pin(_BUILD_BINARY.decode(), dict(self.PINS, **change))


def _extract_archive_of(repo, rev, dest, paths=()):
    archive = subprocess.run(["git", "-C", repo, "archive", "--format=tar", rev, *paths],
                             check=True, capture_output=True).stdout
    with tarfile.open(fileobj=io.BytesIO(archive)) as tf:
        tf.extractall(dest, filter="tar")


class VerdandiSourceTreeTests(unittest.TestCase):
    def setUp(self):
        self.repo, self.head = _init_git_repo({
            "package.json": b"{}\n",
            "apps/claude-sidecar/package.json": b'{"dependencies": {"@anthropic-ai/claude-agent-sdk": "1"}}\n',
            rc.VERDANDI_SENTINEL_HOME: _BUILD_BINARY,
            "proto/v1/runtime.proto": b'syntax = "proto3";\n',
        })
        self.addCleanup(shutil.rmtree, self.repo, ignore_errors=True)
        self.root = _scratch_dir()
        self.addCleanup(shutil.rmtree, self.root, ignore_errors=True)
        _extract_archive_of(self.repo, self.head, self.root)

    def test_the_archive_itself_passes_sdk_mentions_and_all(self):
        self.assertEqual(rc.check_verdandi_source_tree(self.root, self.repo, self.head), [])

    def test_an_elf_a_node_modules_or_a_stray_sentinel_fails(self):
        _plant(self.root, {"bin/tool": _fake_elf(), "node_modules/x/index.js": b"x",
                           "apps/other.js": rc.SEA_SENTINEL.encode()})
        problems = "\n".join(rc.check_verdandi_source_tree(self.root, self.repo, self.head))
        for expected in ("an ELF file", "node_modules", "outside " + rc.VERDANDI_SENTINEL_HOME, "not in the archive"):
            self.assertIn(expected, problems)


class WebBundleFingerprintTests(unittest.TestCase):
    """Pins one literal sha256 for `web_bundle_fingerprint` over a minimal, fixed fixture (a
    single `src/a.ts` holding `x\\n`, nothing else) -- the same fixture and the same literal hash
    `shell/tests/web_bundle_freshness.rs`'s own `fingerprint_matches_the_pinned_value_the_python_
    side_also_asserts` asserts against `shell/build_web.rs`'s independent `compute_fingerprint`.
    The two sides are separate implementations of the same algorithm (see both functions' own doc
    comments), kept in step only by those doc comments and by whatever exercises each one --
    nothing else compares them against each other directly, so a change to either one that
    silently drifted from the other would otherwise only surface as every real release's source
    asset failing `check_eitri_source_tree` (v1-dist Task 12 fix round 1, finding #3)."""

    PINNED_SHA256 = "ae53f67ca66e61114b0f5453e451fa06c6ed8d9f8d44bee54b35d51b23fe3408"

    def test_matches_the_pinned_hash_the_rust_side_also_asserts(self):
        root = _scratch_dir()
        self.addCleanup(shutil.rmtree, root, ignore_errors=True)
        _plant(root, {"src/a.ts": b"x\n"})
        self.assertEqual(rc.web_bundle_fingerprint(root), self.PINNED_SHA256)


class EitriSourceTreeTests(unittest.TestCase):
    SKIA = "skia-binaries-k.tar.gz"
    SKIA_BYTES = b"skia archive"

    def setUp(self):
        self.verdandi, self.vhead = _init_git_repo({"proto/v1/runtime.proto": b'syntax = "proto3";\n',
                                                    "package.json": b"{}\n"})
        self.src, _ = _init_git_repo({"Cargo.toml": b"[workspace]\n", "agent-ui/web/src/a.ts": b"x\n",
                                      "packaging/install.sh": b"# names claude-agent-sdk\n"})
        fork, _ = _init_git_repo({"Cargo.toml": b"[package]\n"})
        shutil.copytree(fork, os.path.join(self.src, "neovide"))
        for d in (self.verdandi, self.src, fork):
            self.addCleanup(shutil.rmtree, d, ignore_errors=True)
        self.root = _scratch_dir()
        self.addCleanup(shutil.rmtree, self.root, ignore_errors=True)
        _extract_archive_of(self.src, "HEAD", self.root)
        _extract_archive_of(os.path.join(self.src, "neovide"), "HEAD", os.path.join(self.root, "neovide"))
        _extract_archive_of(self.verdandi, self.vhead, self.root, ["proto"])
        _plant(self.root, {
            "vendor/nvim-rs/src/lib.rs": b"pub fn f() {}\n",
            ".cargo/config.toml": b'[source.vendored-sources]\ndirectory = "vendor"\n',
            "skia/" + self.SKIA: self.SKIA_BYTES,
            "THIRD-PARTY-LICENSES": b"notices\n",
            "SOURCE": b"source\n",
            "agent-ui/web/dist/index.html": b"<html></html>\n",
        })
        # shell/build_web.rs writes this beside the bundle after a successful build (v1-dist plan
        # Task 6, P4-A1) -- a fresh fingerprint over the fixture's own web sources, exactly as a real
        # release build would compute it, is what check_eitri_source_tree now requires matches.
        self.web_dir = os.path.join(self.root, "agent-ui/web")
        _plant(self.root, {"agent-ui/web/dist/.inputs-sha256": rc.web_bundle_fingerprint(self.web_dir).encode()})

    def check(self):
        sha = hashlib.sha256(self.SKIA_BYTES).hexdigest()
        return rc.check_eitri_source_tree(self.root, self.src, self.verdandi, self.vhead, self.SKIA, sha)

    def test_the_assembled_tree_passes(self):
        self.assertEqual(self.check(), [])

    def test_an_added_file_naming_the_sdk_fails_but_tracked_source_may(self):
        _plant(self.root, {"vendor/x/README.md": b"see claude-agent-sdk\n"})
        problems = self.check()
        self.assertEqual(len(problems), 1, problems)
        self.assertIn("vendor/x/README.md", problems[0])

    def test_a_missing_fingerprint_file_fails(self):
        os.remove(os.path.join(self.root, "agent-ui/web/dist/.inputs-sha256"))
        self.assertTrue(any("no agent-ui/web/dist/.inputs-sha256" in p for p in self.check()))

    def test_a_fingerprint_that_does_not_match_the_shipped_sources_fails(self):
        # A stale or mismatched fingerprint (a source edited after it was written, or one copied
        # from a different build) -- an offline rebuild from this asset would wrongly skip npm and
        # embed a bundle that does not match its own shipped sources. Overwriting only the
        # fingerprint file itself (an allowed addition regardless of its content, so this cannot
        # also trip the git-archive comparison above) isolates this one check.
        _plant(self.root, {"agent-ui/web/dist/.inputs-sha256": b"0" * 64})
        problems = self.check()
        self.assertEqual(len(problems), 1, problems)
        self.assertIn("does not match a fresh fingerprint", problems[0])


    def test_an_unexpected_file_a_changed_proto_or_another_skia_fails(self):
        _plant(self.root, {"stray.txt": b"x", "proto/v1/runtime.proto": b"changed\n", "skia/" + self.SKIA: b"other"})
        problems = "\n".join(self.check())
        for expected in ("stray.txt", "proto/ differs", "pinned sha256"):
            self.assertIn(expected, problems)


def _archive_files(repo, rev, prefix="", paths=()):
    """{prefix + relpath: bytes} for every regular file `git archive rev [paths]` of repo holds."""
    archive = subprocess.run(["git", "-C", repo, "archive", "--format=tar", rev, *paths],
                             check=True, capture_output=True).stdout
    out = {}
    with tarfile.open(fileobj=io.BytesIO(archive)) as tf:
        for member in tf.getmembers():
            if member.isfile():
                out[prefix + member.name] = tf.extractfile(member).read()
    return out


def _pack_tar_gz(dest, files, mtime_of=lambda rel: 1000):
    """A real .tar.gz of {relpath: bytes}, each entry carrying mtime_of(relpath)."""
    with tarfile.open(dest, mode="w:gz") as tf:
        for rel, data in sorted(files.items()):
            info = tarfile.TarInfo(name=rel)
            info.size = len(data)
            info.mtime = mtime_of(rel)
            info.mode = 0o755 if data.startswith(b"\x7fELF") else 0o644
            tf.addfile(info, io.BytesIO(data))


@unittest.skipUnless(_NFPM and shutil.which("dpkg-deb") and shutil.which("bsdtar"),
                     "needs nfpm, dpkg-deb and bsdtar, as the release container has")
class ReleaseAssetsTests(unittest.TestCase):
    """check_release_assets, the `check-assets` entry point release.sh calls, over a whole fake
    release directory: every asset a real one has, built with the same formats (a real .tar.gz, .deb
    and .rpm, git archives of real repositories), extracted with the real tools. The unbroken set
    passes; then one violation per asset is planted, and each must be reported (Task 4 review: the
    parts were tested, the whole was not)."""

    VERSION = "1.0.0-rc.1"
    SKIA = "skia-binaries-k.tar.gz"
    SKIA_BYTES = b"skia archive"
    INSTALL = b"#!/bin/sh\n# builds the sidecar, which downloads claude-agent-sdk on your machine\n"
    APPARMOR = b"abi <abi/4.0>,\nprofile eitri \"/usr/lib/eitri/shell\" flags=(unconfined) {\n  userns,\n}\n"
    RELEASE = b"EITRI_VERSION=1.0.0-rc.1\n"
    NOTICES = {"THIRD-PARTY-LICENSES": b"notices\n", "SOURCE": b"the source is the source asset\n"}

    @classmethod
    def setUpClass(cls):
        cls.verdandi, cls.vrev = _init_git_repo({
            "package.json": b"{}\n",
            "apps/claude-sidecar/package.json": b'{"dependencies": {"@anthropic-ai/claude-agent-sdk": "1"}}\n',
            rc.VERDANDI_SENTINEL_HOME: _BUILD_BINARY,
            "proto/v1/runtime.proto": b'syntax = "proto3";\n',
        })
        cls.src, _ = _init_git_repo({"Cargo.toml": b"[workspace]\n", "agent-ui/web/src/a.ts": b"x\n",
                                     "packaging/install.sh": cls.INSTALL, "LICENSE": b"MIT\n",
                                     "packaging/apparmor/eitri": cls.APPARMOR})
        fork, _ = _init_git_repo({"Cargo.toml": b"[package]\n"})
        shutil.copytree(fork, os.path.join(cls.src, "neovide"))
        shutil.rmtree(fork, ignore_errors=True)

    @classmethod
    def tearDownClass(cls):
        for d in (cls.verdandi, cls.src):
            shutil.rmtree(d, ignore_errors=True)

    def contents(self):
        """{asset key: {relpath: bytes}} for the unbroken release, before packing."""
        v = self.VERSION
        role_bytes = {role: (_fake_elf(role.encode()) if role in rc.RELEASE_BINARIES else role.encode())
                      for role in rc.tarball_roles(v)}
        role_bytes.update(setup=self.INSTALL, RELEASE=self.RELEASE, **self.NOTICES)
        troles, proles = rc.tarball_roles(v), rc.package_roles()
        top = rc.source_top(v) + "/"
        source = _archive_files(self.src, "HEAD", top)
        source.update(_archive_files(os.path.join(self.src, "neovide"), "HEAD", top + "neovide/"))
        source.update(_archive_files(self.verdandi, self.vrev, top, ["proto"]))
        # shell/build_web.rs's own fingerprint (v1-dist plan Task 6, P4-A1), over the working tree
        # `git archive HEAD` of cls.src also reads agent-ui/web/src/a.ts from -- the two agree
        # because that tree is clean (nothing planted here changes agent-ui/web).
        fingerprint = rc.web_bundle_fingerprint(os.path.join(self.src, "agent-ui/web"))
        source.update({top + rel: data for rel, data in {
            "vendor/nvim-rs/src/lib.rs": b"pub fn f() {}\n",
            ".cargo/config.toml": b'[source.vendored-sources]\ndirectory = "vendor"\n',
            "skia/" + self.SKIA: self.SKIA_BYTES,
            "agent-ui/web/dist/index.html": b"<html></html>\n",
            "agent-ui/web/dist/.inputs-sha256": fingerprint.encode(),
            **self.NOTICES,
        }.items()})
        return {
            "tarball": {troles[r]: b for r, b in role_bytes.items()},
            "deb": {**{proles[r]: b for r, b in role_bytes.items()}, rc.deb_roles()["apparmor"]: self.APPARMOR},
            "rpm": {proles[r]: b for r, b in role_bytes.items()},
            "source": source,
            "verdandi": _archive_files(self.verdandi, self.vrev),
            "install.sh": self.INSTALL,
            "RELEASE": self.RELEASE,
        }

    def names(self):
        return dict(zip(("tarball", "deb", "rpm", "source", "verdandi", "install.sh", "RELEASE"),
                        rc.asset_names(self.VERSION, self.vrev[:7])))

    def check(self, mutate=lambda c: None):
        c = self.contents()
        mutate(c)
        out = _scratch_dir()
        self.addCleanup(shutil.rmtree, out, ignore_errors=True)
        names = self.names()
        for key, files in c.items():
            path = os.path.join(out, names[key])
            if files is None:
                continue
            if key in ("install.sh", "RELEASE"):
                with open(path, "wb") as f:
                    f.write(files)
            elif key == "deb":
                _make_deb(path, sorted(files.items()))
            elif key == "rpm":
                _make_rpm(path, sorted(files.items()))
            else:
                # Uniform mtimes: the source asset's bundle is validated by a content fingerprint,
                # not by mtime order (v1-dist plan Task 6, P4-A1), so no special-casing is needed here.
                _pack_tar_gz(path, files, lambda rel: 1000)
        sha = hashlib.sha256(self.SKIA_BYTES).hexdigest()
        return rc.check_release_assets(out, os.path.join(out, "check"), self.VERSION, self.vrev, self.src,
                                       self.verdandi, self.SKIA, sha)

    def test_the_unbroken_release_passes(self):
        self.assertEqual(self.check(), [f"extracted and checked: {', '.join(self.names().values())}"])

    def test_every_asset_is_checked(self):
        top = rc.source_top(self.VERSION) + "/"
        shell = rc.package_roles()["shell"]

        def plant(key, rel, data):
            return lambda c: c[key].__setitem__(rel, data)

        def replace(key, data):
            return lambda c: c.__setitem__(key, data)

        cases = [
            ("tarball", plant("tarball", rc.tarball_top(self.VERSION) + "/lib/eitri/agent-hook", _fake_elf()),
             ["{tarball}: files no release ships", "agent-hook"]),
            ("deb", plant("deb", "usr/lib/eitri/verdandi-claude-sidecar", _fake_elf()),
             ["{deb}: files no release ships", "verdandi-claude-sidecar"]),
            ("rpm", plant("rpm", "usr/lib/eitri/agent-hook", _fake_elf()),
             ["{rpm}: files no release ships"]),
            ("rpm apparmor", plant("rpm", "etc/apparmor.d/eitri", self.APPARMOR),
             ["{rpm}: files no release ships", "etc/apparmor.d/eitri"]),
            ("deb apparmor bytes", plant("deb", "etc/apparmor.d/eitri", b"another profile\n"),
             ["not byte-identical", "{deb}:etc/apparmor.d/eitri"]),
            ("rpm bytes", plant("rpm", shell, _fake_elf(b"another build")),
             ["not byte-identical", "{rpm}:" + shell]),
            ("source", plant("source", top + "stray.txt", b"x\n"), ["source asset vs git archive HEAD", "stray.txt"]),
            ("source sdk", plant("source", top + "vendor/x/README.md", b"see claude-agent-sdk\n"),
             ["the SDK's name in an added file", "vendor/x/README.md"]),
            ("source notices", plant("source", top + "SOURCE", b"another notice\n"),
             ["not byte-identical", "{source}:SOURCE"]),
            ("source top", plant("source", "second-top/README", b"x\n"), ["{source}: top level is"]),
            ("verdandi", plant("verdandi", "bin/tool", _fake_elf()), ["Verdandi asset: an ELF file", "bin/tool"]),
            ("install.sh", replace("install.sh", b"#!/bin/sh\n# another installer\n"),
             ["not byte-identical", "install.sh="]),
            ("RELEASE", replace("RELEASE", b"EITRI_VERSION=1.0.0\n"), ["not byte-identical", "{tarball}:RELEASE"]),
            ("missing", replace("verdandi", None), ["asset missing: {verdandi}"]),
        ]
        names = {k: v for k, v in self.names().items() if "." not in k and k != "RELEASE"}
        for label, mutate, expected in cases:
            with self.subTest(label):
                with self.assertRaises(rc.ReleaseCheckError) as caught:
                    self.check(mutate)
                for text in expected:
                    self.assertIn(text.format(**names), str(caught.exception))


def _fresh_zh_twin(english_name, english_text):
    """A twin that check_twins accepts for english_text: its own switch line, a translated-from
    marker naming english_name and its real sha256, and a body line -- used only to build fixtures
    for TwinsCheckTests, which then breaks one property of the pair at a time."""
    stem = os.path.splitext(english_name)[0]
    zh_name = f"{stem}.zh-CN.md"
    digest = hashlib.sha256(english_text.encode("utf-8")).hexdigest()
    zh_text = f"[English]({english_name}) | 简体中文\n<!-- translated-from: {english_name} sha256={digest} -->\n\n正文\n"
    return zh_name, zh_text


class TwinsCheckTests(unittest.TestCase):
    """release_check.check_twins (Task 5, v1-dist plan lane D,
    docs/superpowers/plans/2026-09-28-v1-dist-task17-18.md): every Chinese guide anywhere under a
    tree, at any depth, must still match the English original it was translated from. README.md/
    README.zh-CN.md and INSTALL.md/INSTALL.zh-CN.md are required pairs at the tree's own top level;
    any other discovered <name>.zh-CN.md is paired with its own <name>.md in the same directory the
    same way (recursion, not just the top level, added in Task 5's own fix round 1). One property
    broken per test, so each failure mode has its own fixture and its own message to check."""

    def setUp(self):
        self.root = _scratch_dir()
        self.addCleanup(shutil.rmtree, self.root, ignore_errors=True)

    def plant_text(self, files):
        _plant(self.root, {rel: text.encode("utf-8") for rel, text in files.items()})

    def fresh_readme(self, english="English | [简体中文](README.zh-CN.md)\n\n# fixture\n"):
        zh_name, zh_text = _fresh_zh_twin("README.md", english)
        return {"README.md": english, zh_name: zh_text}

    def fresh_install(self, english="English | [简体中文](INSTALL.zh-CN.md)\n\n# fixture install\n"):
        zh_name, zh_text = _fresh_zh_twin("INSTALL.md", english)
        return {"INSTALL.md": english, zh_name: zh_text}

    def test_a_fresh_required_pair_passes(self):
        self.plant_text({**self.fresh_readme(), **self.fresh_install()})
        rc.check_twins(self.root)  # must not raise

    def test_a_stale_twin_is_reported_with_both_hashes(self):
        files = self.fresh_readme()
        files["README.md"] += "more\n"
        self.plant_text({**files, **self.fresh_install()})
        with self.assertRaises(rc.ReleaseCheckError) as caught:
            rc.check_twins(self.root)
        message = str(caught.exception)
        self.assertIn("README.zh-CN.md", message)
        self.assertIn("stale", message)
        self.assertEqual(message.count("sha256="), 2)

    def test_a_twin_with_no_marker_is_reported(self):
        files = self.fresh_readme()
        lines = files["README.zh-CN.md"].splitlines(keepends=True)
        lines[1] = "not a marker\n"
        files["README.zh-CN.md"] = "".join(lines)
        self.plant_text({**files, **self.fresh_install()})
        with self.assertRaisesRegex(rc.ReleaseCheckError, "not a translated-from marker"):
            rc.check_twins(self.root)

    def test_a_marker_naming_a_different_file_is_reported(self):
        files = self.fresh_readme()
        files["README.zh-CN.md"] = files["README.zh-CN.md"].replace(
            "translated-from: README.md", "translated-from: OTHER.md"
        )
        self.plant_text({**files, **self.fresh_install()})
        with self.assertRaisesRegex(rc.ReleaseCheckError, r"its marker names 'OTHER\.md'"):
            rc.check_twins(self.root)

    def test_an_english_original_without_its_switch_line_is_reported(self):
        files = self.fresh_readme(english="Not the switch line\n\n# fixture\n")
        self.plant_text({**files, **self.fresh_install()})
        with self.assertRaisesRegex(rc.ReleaseCheckError, "not the switch line"):
            rc.check_twins(self.root)

    def test_a_missing_english_file_for_a_discovered_twin_is_reported(self):
        _, notes_zh = _fresh_zh_twin("NOTES.md", "English | [简体中文](NOTES.zh-CN.md)\n\nbody\n")
        self.plant_text({**self.fresh_readme(), **self.fresh_install(), "NOTES.zh-CN.md": notes_zh})
        with self.assertRaisesRegex(rc.ReleaseCheckError, "NOTES.zh-CN.md: no NOTES.md"):
            rc.check_twins(self.root)

    def test_a_missing_twin_of_a_required_pair_is_reported(self):
        files = self.fresh_readme()
        del files[_fresh_zh_twin("README.md", files["README.md"])[0]]
        self.plant_text({**files, **self.fresh_install()})
        with self.assertRaisesRegex(rc.ReleaseCheckError, "README.md: no README.zh-CN.md"):
            rc.check_twins(self.root)

    def test_a_missing_required_english_file_is_reported(self):
        # No README.md and no README.zh-CN.md planted at all: the required pair's English side is
        # entirely absent, distinct from "present but its twin is missing" above. Fix round 1 (a
        # plain finding, verified real): this used to read "README.zh-CN.md: no README.md beside
        # it (a missing English file)", which implies README.zh-CN.md exists when it does not --
        # the message must say neither file exists.
        self.plant_text(self.fresh_install())
        with self.assertRaisesRegex(rc.ReleaseCheckError, r"README\.md: neither it nor its twin README\.zh-CN\.md exists"):
            rc.check_twins(self.root)

    def test_a_file_with_no_twin_at_all_is_never_checked(self):
        # CONTRIBUTING.md, by design (Task 3), is English-only: it has no *.zh-CN.md, so it is
        # never part of any pair and cannot fail this check.
        self.plant_text({**self.fresh_readme(), **self.fresh_install(),
                          "CONTRIBUTING.md": "English only, no twin\n"})
        rc.check_twins(self.root)  # must not raise

    # Fix round 1 ([codex] finding, verified real): publish/export.sh mirrors publish/files/ into
    # the exported tree path for path ("Files that exist only in the public tree"), so a future
    # guide nested under a subdirectory (e.g. docs/USAGE.md) needs the same freshness gate as a
    # top-level one. These four pin that check_twins actually recurses, rather than only ever
    # looking at `root` itself.

    def test_a_fresh_nested_guide_pair_passes(self):
        # _fresh_zh_twin's own switch-line/marker convention names the bare filename ("USAGE.md",
        # not "docs/USAGE.md") -- the same convention the top-level required pairs use, since a
        # pair is always adjacent (same directory), nested or not. "docs/" is only where the pair
        # is planted.
        usage_en = "English | [简体中文](USAGE.zh-CN.md)\n\n# fixture usage guide\n"
        usage_zh_name, usage_zh = _fresh_zh_twin("USAGE.md", usage_en)
        self.plant_text({**self.fresh_readme(), **self.fresh_install(),
                          "docs/USAGE.md": usage_en, f"docs/{usage_zh_name}": usage_zh})
        rc.check_twins(self.root)  # must not raise

    def test_a_stale_nested_guide_is_reported(self):
        usage_en = "English | [简体中文](USAGE.zh-CN.md)\n\n# fixture usage guide\n"
        usage_zh_name, usage_zh = _fresh_zh_twin("USAGE.md", usage_en)
        self.plant_text({**self.fresh_readme(), **self.fresh_install(),
                          "docs/USAGE.md": usage_en + "more\n", f"docs/{usage_zh_name}": usage_zh})
        with self.assertRaises(rc.ReleaseCheckError) as caught:
            rc.check_twins(self.root)
        message = str(caught.exception)
        self.assertIn(os.path.join("docs", "USAGE.zh-CN.md"), message)
        self.assertIn("stale", message)

    def test_a_nested_guide_missing_its_english_original_is_reported(self):
        _, usage_zh = _fresh_zh_twin("USAGE.md", "English | [简体中文](USAGE.zh-CN.md)\n\nbody\n")
        self.plant_text({**self.fresh_readme(), **self.fresh_install(), "docs/USAGE.zh-CN.md": usage_zh})
        with self.assertRaisesRegex(
            rc.ReleaseCheckError, re.escape(os.path.join("docs", "USAGE.zh-CN.md")) + r": no "
            + re.escape(os.path.join("docs", "USAGE.md"))
        ):
            rc.check_twins(self.root)

    def test_a_git_directory_is_never_scanned(self):
        # A lone .zh-CN.md with no adjacent .md would fail the check if it were ever looked at;
        # planting one inside .git/ and asserting no exception proves it genuinely is not.
        self.plant_text({**self.fresh_readme(), **self.fresh_install(),
                          ".git/refs/ORPHAN.zh-CN.md": "not a real guide\n"})
        rc.check_twins(self.root)  # must not raise

    def test_several_problems_are_reported_together_not_just_the_first(self):
        files = self.fresh_readme()
        del files[_fresh_zh_twin("README.md", files["README.md"])[0]]
        install = self.fresh_install()
        install["INSTALL.md"] += "more\n"
        self.plant_text({**files, **install})
        with self.assertRaises(rc.ReleaseCheckError) as caught:
            rc.check_twins(self.root)
        message = str(caught.exception)
        self.assertIn("README.md: no README.zh-CN.md", message)
        self.assertIn("INSTALL.zh-CN.md", message)
        self.assertIn("stale", message)

    def test_not_a_directory_is_reported(self):
        not_a_dir = os.path.join(self.root, "not-a-directory")
        _plant(self.root, {"not-a-directory": b"a file, not a directory\n"})
        with self.assertRaisesRegex(rc.ReleaseCheckError, "is not a directory"):
            rc.check_twins(not_a_dir)


class TwinsCliTests(unittest.TestCase):
    """The `twins` CLI verb (release.sh's own call site, wired into preflight)."""

    def setUp(self):
        self.root = _scratch_dir()
        self.addCleanup(shutil.rmtree, self.root, ignore_errors=True)

    def run_cli(self, *args):
        buf_out, buf_err = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(buf_out), contextlib.redirect_stderr(buf_err):
            code = rc.main(["twins", *args])
        return code, buf_out.getvalue(), buf_err.getvalue()

    def test_a_fresh_tree_exits_zero(self):
        readme_en = "English | [简体中文](README.zh-CN.md)\n\nx\n"
        install_en = "English | [简体中文](INSTALL.zh-CN.md)\n\nx\n"
        readme_zh_name, readme_zh = _fresh_zh_twin("README.md", readme_en)
        install_zh_name, install_zh = _fresh_zh_twin("INSTALL.md", install_en)
        _plant(self.root, {
            "README.md": readme_en.encode("utf-8"),
            readme_zh_name: readme_zh.encode("utf-8"),
            "INSTALL.md": install_en.encode("utf-8"),
            install_zh_name: install_zh.encode("utf-8"),
        })
        code, _out, err = self.run_cli(self.root)
        self.assertEqual(code, 0, err)
        self.assertIn("fresh", err)

    def test_a_stale_tree_exits_one_and_names_it(self):
        code, _out, err = self.run_cli(self.root)  # nothing planted: both required pairs missing
        self.assertEqual(code, 1, err)
        self.assertIn("release_check: FAILED:", err)
        self.assertIn("README.md", err)


class ReleaseSignersAgreementTests(unittest.TestCase):
    """Task 1 (installer-claude-2, docs-superpowers/plans/2026-09-28-v1-dist-review2-fixes.md):
    packaging/install.sh's embedded_release_signers() heredoc must stay byte-identical to the
    tracked packaging/release-signers -- a hand-kept copy that drifts is the defect the report
    found (spec sec 4.4, D5/D13). These tests plant a minimal install.sh shape carrying the real
    heredoc marker (`EITRI_RELEASE_SIGNERS`) rather than reading the real 130KB file, since only
    the marker shape and the block between the two marker lines matter here."""

    @staticmethod
    def install_sh(block):
        return "#!/bin/sh\nembedded_release_signers() {\n\tcat <<'EITRI_RELEASE_SIGNERS'\n" + block \
            + "EITRI_RELEASE_SIGNERS\n}\n"

    def test_extract_embedded_signers_returns_exactly_the_heredoc_body(self):
        block = '# a comment\nrelease@eitri namespaces="eitri-release" ssh-ed25519 AAAAkeydata\n'
        self.assertEqual(rc.extract_embedded_signers(self.install_sh(block)), block)

    def test_equal_and_empty_passes(self):
        block = "# no key line yet\n"
        rc.check_release_signers_agree(self.install_sh(block), block)  # must not raise

    def test_equal_with_a_key_passes(self):
        block = '# a comment\nrelease@eitri namespaces="eitri-release" ssh-ed25519 AAAAkeydata\n'
        rc.check_release_signers_agree(self.install_sh(block), block)  # must not raise

    def test_a_difference_is_reported_with_each_sides_key_lines(self):
        embedded = "# no key line yet\n"
        tracked = '# a comment\nrelease@eitri namespaces="eitri-release" ssh-ed25519 AAAAkeydata\n'
        with self.assertRaises(rc.ReleaseCheckError) as caught:
            rc.check_release_signers_agree(self.install_sh(embedded), tracked)
        message = str(caught.exception)
        self.assertIn("differ", message)
        self.assertIn("ssh-ed25519 AAAAkeydata", message)
        self.assertIn("no key line", message)

    def test_missing_markers_is_reported(self):
        with self.assertRaisesRegex(rc.ReleaseCheckError, "EITRI_RELEASE_SIGNERS"):
            rc.check_release_signers_agree("#!/bin/sh\necho hi\n", "# no key\n")

    def test_only_an_opening_marker_is_reported(self):
        broken = "#!/bin/sh\n\tcat <<'EITRI_RELEASE_SIGNERS'\n# no key\n"
        with self.assertRaisesRegex(rc.ReleaseCheckError, "EITRI_RELEASE_SIGNERS"):
            rc.check_release_signers_agree(broken, "# no key\n")


class CheckReleaseSignersCliIsByteExact(unittest.TestCase):
    """Fix round 1 (minor finding): check_release_signers_agree's docstring and its own error
    message both promise a byte-identical comparison, but the `check-release-signers` CLI verb used
    to read both files with plain text-mode _read, which silently translates '\\r\\n' to '\\n'
    (Python's universal-newline handling). A CRLF copy of packaging/release-signers used to pass
    against install.sh's LF heredoc even though the files are not byte-identical -- reproduced here
    against the real files on disk, through rc.main's own CLI dispatch (not the pure function, which
    the tests above already exercise directly with in-memory strings and never touched a file)."""

    def setUp(self):
        self.scratch = _scratch_dir()

    def write(self, rel, text):
        path = os.path.join(self.scratch, rel)
        with open(path, "w", encoding="utf-8", newline="") as f:
            f.write(text)
        return path

    def run_cli(self, install_sh, signers):
        buf_out, buf_err = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(buf_out), contextlib.redirect_stderr(buf_err):
            code = rc.main(["check-release-signers", install_sh, signers])
        return code, buf_out.getvalue(), buf_err.getvalue()

    def test_lf_install_sh_against_lf_signers_matches(self):
        block = 'release@eitri namespaces="eitri-release" ssh-ed25519 AAAAkeydata\n'
        install_sh = self.write("install.sh",
                                 "#!/bin/sh\nembedded_release_signers() {\n\tcat <<'EITRI_RELEASE_SIGNERS'\n"
                                 + block + "EITRI_RELEASE_SIGNERS\n}\n")
        signers = self.write("release-signers", block)
        code, _, err = self.run_cli(install_sh, signers)
        self.assertEqual(code, 0, err)

    def test_a_crlf_copy_of_release_signers_is_not_treated_as_matching_an_lf_install_sh(self):
        block = 'release@eitri namespaces="eitri-release" ssh-ed25519 AAAAkeydata\n'
        install_sh = self.write("install.sh",
                                 "#!/bin/sh\nembedded_release_signers() {\n\tcat <<'EITRI_RELEASE_SIGNERS'\n"
                                 + block + "EITRI_RELEASE_SIGNERS\n}\n")
        # The exact same key line, but the tracked file on disk is CRLF -- not byte-identical to
        # what install.sh embeds, even though a universal-newline read would collapse the two.
        signers = self.write("release-signers", block.replace("\n", "\r\n"))
        code, out, err = self.run_cli(install_sh, signers)
        self.assertEqual(code, 1, f"stdout={out!r} stderr={err!r}")
        self.assertIn("differ", err)


def _this_tasks_files():
    """Files this task (and, later, Tasks 3-4) own under packaging/: only the ones that already
    exist are checked, so this test does not depend on another lane's in-flight work landing
    first."""
    candidates = [
        os.path.join(_PACKAGING, "release_check.py"),
        os.path.join(_HERE, "test_release_layout.py"),
        os.path.join(_PACKAGING, "release.sh"),
        os.path.join(_PACKAGING, "release-notes.md.in"),
        os.path.join(_HERE, "test_release_sh.py"),
    ]
    container_dir = os.path.join(_PACKAGING, "container")
    if os.path.isdir(container_dir):
        for dirpath, _dirnames, filenames in os.walk(container_dir):
            for name in filenames:
                candidates.append(os.path.join(dirpath, name))
    return [p for p in candidates if os.path.isfile(p)]


class RewritesNoOpTests(unittest.TestCase):
    def test_at_least_this_tasks_own_two_files_are_present(self):
        files = _this_tasks_files()
        basenames = {os.path.basename(p) for p in files}
        self.assertIn("release_check.py", basenames)
        self.assertIn("test_release_layout.py", basenames)

    def test_publish_rewrites_sed_is_a_no_op_on_every_one_of_these_files(self):
        rewrites = os.path.join(os.path.dirname(_PACKAGING), "publish", "rewrites.sed")
        if not os.path.isfile(rewrites):
            self.skipTest("publish/rewrites.sed not present in this checkout")
        for path in _this_tasks_files():
            with open(path, "rb") as f:
                original = f.read()
            out = subprocess.run(["sed", "-f", rewrites, path], check=True, capture_output=True)
            self.assertEqual(
                out.stdout, original,
                f"publish/rewrites.sed is not a no-op on {os.path.relpath(path, _PACKAGING)}",
            )

    def test_the_joined_sea_sentinel_literal_appears_in_neither_file(self):
        for path in _this_tasks_files():
            with open(path, "rb") as f:
                content = f.read()
            self.assertNotIn(
                rc.SEA_SENTINEL.encode("ascii"), content,
                f"the joined SEA sentinel literal must not appear in {os.path.relpath(path, _PACKAGING)} "
                "(M5) -- it would flag the Eitri source asset's own copy of this file",
            )


if __name__ == "__main__":
    unittest.main()
