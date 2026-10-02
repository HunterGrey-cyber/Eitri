"""Tests for packaging/release.sh (lane D Task 4, plan docs/superpowers/plans/2026-09-27-v1-dist-task12.md):
every refusal it makes before anything is built, each with its own message, and a sign/verify round
trip through the same shell functions a release uses.

    python3 -m pytest packaging/tests/test_release_sh.py -q

No container and no network. The fixture is a small git clone (a fake workspace, the real release.sh
and pins.env, and a `neovide` submodule) built once per run and copied per test. `docker` is a stub
first on PATH that records its arguments and exits 97, so a run that passes every refusal stops at its
first container step -- which is the positive control: the same fixture, unbroken, reaches the image
build. Scratch lives under ~/.cache, never /tmp.
"""

import hashlib
import importlib.util
import os
import re
import shutil
import subprocess
import tempfile
import textwrap
import time
import unittest

_HERE = os.path.dirname(os.path.abspath(__file__))
_PACKAGING = os.path.dirname(_HERE)
_RELEASE_SH = os.path.join(_PACKAGING, "release.sh")
_RELEASE_CHECK_PY = os.path.join(_PACKAGING, "release_check.py")
_PINS = os.path.join(_PACKAGING, "pins.env")
_spec = importlib.util.spec_from_file_location("release_check", os.path.join(_PACKAGING, "release_check.py"))
rc = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(rc)

_SCRATCH_ROOT = os.path.expanduser("~/.cache/eitri-release-sh-tests")
_VERDANDI_REV = "0123456789abcdef0123456789abcdef01234567"
_NVIM_PIN = "NVIM_VERSION=0.11.4\nNVIM_SHA256_linux_x86_64=" + "ab" * 32 + "\n"
_DOCKER_EXIT = 97
# The identity every public commit carries: release.sh's RS_PUBLIC_NAME/RS_PUBLIC_EMAIL.
_PUBLIC_NAME = "Hunter Grey"
_PUBLIC_EMAIL = "71165939+HunterGrey-cyber@users.noreply.github.com"
# Task 1 (installer-claude-2): the fixture's packaging/release-signers, and the matching block a
# fixture packaging/install.sh embeds -- kept as one constant so a test that wants them to agree
# (the positive controls) and one that wants them to differ (Refusals) both start from the same
# text rather than two copies that could quietly drift from each other.
_SIGNERS_BLOCK = "# no key line in the fixture's committed signers file\n"


def _install_sh_fixture(block=_SIGNERS_BLOCK):
    """A minimal packaging/install.sh stand-in: release.sh's own preflight only reads this file's
    embedded_release_signers() heredoc (packaging/release_check.py's check-release-signers), never
    runs it, so the fixture need not be a working installer."""
    return ("#!/bin/sh\nembedded_release_signers() {\n\tcat <<'EITRI_RELEASE_SIGNERS'\n" + block
            + "EITRI_RELEASE_SIGNERS\n}\n")


# Task 5 (v1-dist, lane D): release_check.py's `twins` check (wired into preflight, below) requires
# a fresh README.md/README.zh-CN.md and INSTALL.md/INSTALL.zh-CN.md in every --source tree, so the
# fixture clone needs its own minimal, genuinely-fresh pair of each -- built here rather than copied
# from publish/files/, so this suite never depends on that lane's own docs landing first.
def _twin_zh(english_name, english_text):
    digest = hashlib.sha256(english_text.encode("utf-8")).hexdigest()
    return f"[English]({english_name}) | 简体中文\n<!-- translated-from: {english_name} sha256={digest} -->\n\n# 中文\n"


_TWIN_README_EN = "English | [简体中文](README.zh-CN.md)\n\n# fixture readme\n"
_TWIN_INSTALL_EN = "English | [简体中文](INSTALL.zh-CN.md)\n\n# fixture install guide\n"


def _twin_doc_files():
    """The fixture's own fresh README.md/INSTALL.md and their Chinese twins, as a files dict
    fragment to merge into _build_template's own (same shape: relpath -> text)."""
    return {
        "README.md": _TWIN_README_EN,
        "README.zh-CN.md": _twin_zh("README.md", _TWIN_README_EN),
        "INSTALL.md": _TWIN_INSTALL_EN,
        "INSTALL.zh-CN.md": _twin_zh("INSTALL.md", _TWIN_INSTALL_EN),
    }


def _scratch_dir():
    os.makedirs(_SCRATCH_ROOT, exist_ok=True)
    return tempfile.mkdtemp(dir=_SCRATCH_ROOT)


def _write(path, text, mode=None):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w", encoding="utf-8") as f:
        f.write(text)
    if mode is not None:
        os.chmod(path, mode)


class _Env:
    """The environment every run gets: a fixture HOME, a git config of its own, and the stubs."""

    def __init__(self, root):
        self.root = root
        self.home = os.path.join(root, "home")
        self.bin = os.path.join(root, "bin")
        self.marker = os.path.join(root, "docker-calls")
        os.makedirs(os.path.join(self.home, ".ssh"))
        gitconfig = os.path.join(root, "gitconfig")
        # Every fixture commit carries the public identity: preflight refuses a release commit that
        # does not (F7, PublicCommitsRule).
        _write(gitconfig, textwrap.dedent(f"""\
            [user]
            \tname = {_PUBLIC_NAME}
            \temail = {_PUBLIC_EMAIL}
            [commit]
            \tgpgsign = false
            [protocol "file"]
            \tallow = always
            [init]
            \tdefaultBranch = main
            """))
        _write(os.path.join(self.bin, "docker"),
               f'#!/bin/sh\nprintf "%s\\n" "$*" >> "{self.marker}"\nexit {_DOCKER_EXIT}\n', 0o755)
        # The public repo preflight reads its branches from (F7, fix round 1): a bare repo standing in
        # for GitHub, empty until a test pushes to it, so every fixture commit counts as new to it.
        self.public = os.path.join(root, "public.git")
        self.vars = {
            "PATH": self.bin + os.pathsep + os.environ.get("PATH", "/usr/bin:/bin"),
            "HOME": self.home,
            "GIT_CONFIG_GLOBAL": gitconfig,
            "GIT_CONFIG_NOSYSTEM": "1",
            "LANG": "C.UTF-8",
            "EITRI_PUBLIC_REPO_URL": self.public,
        }
        subprocess.run(["git", "init", "-q", "--bare", self.public], check=True, capture_output=True, env=self.vars)
        # Upstream Neovide, which preflight reads to tell the fork's own commits from upstream's: a bare
        # repo holding one commit by a contributor, unrelated to the fixture fork.
        self.upstream = os.path.join(root, "upstream.git")
        seed = os.path.join(root, "upstream-seed")
        self.vars["EITRI_UPSTREAM_NEOVIDE_URL"] = self.upstream
        subprocess.run(["git", "init", "-q", "--bare", self.upstream], check=True, capture_output=True, env=self.vars)
        subprocess.run(["git", "init", "-q", seed], check=True, capture_output=True, env=self.vars)
        _write(os.path.join(seed, "README"), "upstream\n")
        contributor = dict(self.vars, GIT_AUTHOR_NAME="Upstream Dev", GIT_AUTHOR_EMAIL="dev@example.com",
                           GIT_COMMITTER_NAME="Upstream Dev", GIT_COMMITTER_EMAIL="dev@example.com")
        for argv in (["add", "-A"], ["commit", "-q", "-m", "upstream"], ["push", "-q", self.upstream, "HEAD:refs/heads/main"]):
            subprocess.run(["git", "-C", seed, *argv], check=True, capture_output=True, env=contributor)

    def git(self, cwd, *args):
        subprocess.run(["git", "-C", cwd, *args], check=True, capture_output=True, env=self.vars)

    def fake_root(self):
        _write(os.path.join(self.bin, "id"),
               '#!/bin/sh\nif [ "$1" = -u ]; then echo 0; exit 0; fi\nexec /usr/bin/id "$@"\n', 0o755)

    def keypair(self, name):
        path = os.path.join(self.root, name)
        subprocess.run(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-C", "", "-f", path],
                       check=True, capture_output=True, env=self.vars)
        with open(path + ".pub", encoding="utf-8") as f:
            return path, f.read().strip()


def _build_template(env, dest):
    """The fixture clone: a workspace release.sh accepts as it stands."""
    fork = os.path.join(env.root, "fork")
    _write(os.path.join(fork, "Cargo.toml"), '[package]\nname = "neovide"\nversion = "0.0.0"\n')
    _write(os.path.join(fork, ".gitignore"), "target/\n")
    subprocess.run(["git", "init", "-q", fork], check=True, env=env.vars)
    env.git(fork, "add", "-A")
    env.git(fork, "commit", "-q", "-m", "fork")

    # The fixture's nvim pin is _NVIM_PIN alone: every NVIM_* line of the real pins.env (plan Task
    # 11's) is dropped first, so RehearsalRule's "no nvim pin" fixture really has none once Task 11
    # is merged, and the pin under test is never a second, shadowed copy of the real one.
    with open(_PINS, encoding="utf-8") as f:
        pins = "".join(line for line in f if not line.startswith("NVIM_"))
    files = {
        "Cargo.toml": textwrap.dedent("""\
            [workspace]
            resolver = "2"
            members = ["shell", "agent"]

            [workspace.package]
            version = "1.0.0-rc.1"
            """),
        "Cargo.lock": 'version = 4\n\n[[package]]\nname = "shell"\nversion = "1.0.0-rc.1"\n',
        ".gitignore": "dist/\n",
        "shell/Cargo.toml": textwrap.dedent("""\
            [package]
            name = "shell"
            version.workspace = true

            [features]
            default = []
            legacy-backend = ["agent/legacy-backend"]
            """),
        "agent/Cargo.toml": textwrap.dedent(f"""\
            [package]
            name = "agent"
            version.workspace = true

            [dependencies]
            claude-runtime-protocol = {{ git = "https://example.com/verdandi.git", rev = "{_VERDANDI_REV}" }}

            [features]
            legacy-backend = []
            """),
        "agent/src/providers/claude_sidecar/spawn.rs":
            f'pub const EXPECTED_VERDANDI_REVISION: &str = "{_VERDANDI_REV[:7]}";\n',
        "packaging/pins.env": pins + _NVIM_PIN,
        "packaging/release-signers": _SIGNERS_BLOCK,
        "packaging/install.sh": _install_sh_fixture(),
        **_twin_doc_files(),
    }
    for rel, text in files.items():
        _write(os.path.join(dest, rel), text)
    shutil.copy2(_RELEASE_SH, os.path.join(dest, "packaging", "release.sh"))
    # Task 1's preflight check (embedded release-signers agreement) shells out to this, exactly as
    # a real clone's release.sh would ("run the copy the clone carries", same as release.sh itself).
    shutil.copy2(_RELEASE_CHECK_PY, os.path.join(dest, "packaging", "release_check.py"))
    subprocess.run(["git", "init", "-q", dest], check=True, env=env.vars)
    env.git(dest, "submodule", "add", "-q", fork, "neovide")
    env.git(dest, "add", "-A")
    env.git(dest, "commit", "-q", "-m", "fixture")


class ReleaseShTestCase(unittest.TestCase):
    """Each test gets its own copy of the fixture clone, HOME and stubs."""

    def setUp(self):
        self.scratch = _scratch_dir()
        self.addCleanup(shutil.rmtree, self.scratch, ignore_errors=True)
        self.env = _Env(self.scratch)
        self.clone = os.path.join(self.scratch, "clone")
        _build_template(self.env, self.clone)
        self.out = os.path.join(self.scratch, "out")

    def commit(self, message="change"):
        self.env.git(self.clone, "add", "-A")
        self.env.git(self.clone, "commit", "-q", "-m", message)

    def edit(self, rel, old, new):
        path = os.path.join(self.clone, rel)
        with open(path, encoding="utf-8") as f:
            text = f.read()
        self.assertIn(old, text)
        _write(path, text.replace(old, new))

    def write_signers(self, block):
        """packaging/release-signers and the matching packaging/install.sh embedded block,
        together, so a test that changes what the fixture signs never leaves the two fixture
        files disagreeing by accident (Task 1's own drift check would refuse that)."""
        _write(os.path.join(self.clone, "packaging", "release-signers"), block)
        _write(os.path.join(self.clone, "packaging", "install.sh"), _install_sh_fixture(block))

    def run_release(self, *args, version="1.0.0-rc.1", source=None, script=_RELEASE_SH):
        argv = ["bash", script]
        if version is not None:
            argv.append(version)
        argv += ["--source", source or self.clone, "--out", self.out, *args]
        return subprocess.run(argv, capture_output=True, text=True, env=self.env.vars, timeout=120)

    def docker_calls(self):
        if not os.path.exists(self.env.marker):
            return []
        with open(self.env.marker, encoding="utf-8") as f:
            return f.read().splitlines()

    def assertRefused(self, proc, message):
        self.assertEqual(proc.returncode, 1, f"expected a refusal, got {proc.returncode}:\n{proc.stderr}")
        self.assertIn(message, proc.stderr)
        self.assertEqual(self.docker_calls(), [], "a refusal must come before any docker call")

    def assertReachedDocker(self, proc):
        self.assertEqual(proc.returncode, _DOCKER_EXIT, f"expected to reach docker:\n{proc.stderr}")
        calls = self.docker_calls()
        self.assertTrue(calls and calls[0].startswith("build "), calls)
        return calls


class PositiveControls(ReleaseShTestCase):
    def test_the_unbroken_fixture_reaches_the_image_build_without_pull(self):
        calls = self.assertReachedDocker(self.run_release("--unsigned"))
        build = calls[0].split()
        self.assertNotIn("--pull", build)
        self.assertIn(f"NODE_VERSION={rc.parse_release_text(open(_PINS).read())['NODE_VERSION']}", build)
        self.assertIn(os.path.join(self.clone, "packaging", "container", "build.Dockerfile"), build)
        self.assertTrue(os.path.isdir(os.path.join(self.out, "v1.0.0-rc.1")))

    def test_a_signed_candidate_with_a_test_signers_file_reaches_the_image_build(self):
        key, pub = self.env.keypair("rc-key")
        signers = os.path.join(self.scratch, "rc-signers")
        _write(signers, f'release@eitri namespaces="eitri-release" {" ".join(pub.split()[:2])}\n')
        proc = self.run_release("--sign", key, "--release-signers", signers)
        self.assertReachedDocker(proc)
        self.assertIn("--release-signers", proc.stderr)

    def test_a_final_version_signed_by_a_committed_signer_reaches_the_image_build(self):
        key, pub = self.env.keypair("release-key")
        self.edit("Cargo.toml", 'version = "1.0.0-rc.1"', 'version = "1.0.0"')
        self.write_signers(f'release@eitri namespaces="eitri-release" {" ".join(pub.split()[:2])}\n')
        self.commit()
        self.assertReachedDocker(self.run_release("--sign", key, version="1.0.0"))


class Refusals(ReleaseShTestCase):
    def test_running_as_root(self):
        self.env.fake_root()
        self.assertRefused(self.run_release("--unsigned"), "refusing to run as root")

    def test_a_source_without_git(self):
        bare = os.path.join(self.scratch, "export-tree")
        shutil.copytree(self.clone, bare, ignore=shutil.ignore_patterns(".git"))
        self.assertRefused(self.run_release("--unsigned", source=bare), "has no .git")

    def test_a_clone_whose_release_sh_differs_from_the_one_running(self):
        with open(os.path.join(self.clone, "packaging", "release.sh"), "a", encoding="utf-8") as f:
            f.write("# changed\n")
        self.commit()
        self.assertRefused(self.run_release("--unsigned"), "differs from")

    def test_an_untracked_file(self):
        _write(os.path.join(self.clone, "stray.txt"), "x\n")
        self.assertRefused(self.run_release("--unsigned"), "is not clean")

    def test_an_ignored_file_such_as_a_built_dist(self):
        _write(os.path.join(self.clone, "dist", "index.html"), "built on the host\n")
        self.assertRefused(self.run_release("--unsigned"), "is not clean")

    def test_a_stray_file_hidden_by_status_show_untracked_files_no(self):
        # A global or clone config can hide untracked and ignored files from a plain `git status`
        # (Task 4 review); the check must not follow it.
        self.env.git(self.clone, "config", "status.showUntrackedFiles", "no")
        for rel in ("stray.txt", "dist/index.html"):
            with self.subTest(rel):
                _write(os.path.join(self.clone, rel), "x\n")
                self.assertRefused(self.run_release("--unsigned"), "is not clean")
                os.remove(os.path.join(self.clone, rel))

    def test_an_uninitialised_neovide_submodule(self):
        self.env.git(self.clone, "submodule", "deinit", "-q", "-f", "neovide")
        self.assertRefused(self.run_release("--unsigned"), "is not initialised")

    def test_a_neovide_submodule_away_from_its_recorded_commit(self):
        sub = os.path.join(self.clone, "neovide")
        _write(os.path.join(sub, "extra.txt"), "x\n")
        self.env.git(sub, "add", "-A")
        self.env.git(sub, "commit", "-q", "-m", "moved")
        self.assertRefused(self.run_release("--unsigned"), "not at the commit HEAD records")

    def test_an_ignored_file_inside_neovide(self):
        _write(os.path.join(self.clone, "neovide", "target", "stale"), "x\n")
        self.assertRefused(self.run_release("--unsigned"), "neovide is not clean")

    def test_a_cargo_config_directory(self):
        _write(os.path.join(self.clone, ".cargo", "config.toml"), "[net]\noffline = true\n")
        self.commit()
        self.assertRefused(self.run_release("--unsigned"), "has a .cargo/")

    def test_an_ssh_source_in_a_manifest(self):
        self.edit("agent/Cargo.toml", 'git = "https://example.com/verdandi.git"',
                  'git = "ssh://git@example.com/verdandi.git"')
        self.commit()
        self.assertRefused(self.run_release("--unsigned"), "names an ssh source")

    def test_an_ssh_source_in_the_lockfile(self):
        with open(os.path.join(self.clone, "Cargo.lock"), "a", encoding="utf-8") as f:
            f.write('\n[[package]]\nname = "x"\nversion = "1.0.0"\nsource = "git+ssh://git@example.com/x.git#aa"\n')
        self.commit()
        self.assertRefused(self.run_release("--unsigned"), "Cargo.lock names an ssh source")

    def test_a_version_that_disagrees_with_the_workspace(self):
        self.assertRefused(self.run_release("--unsigned", version="1.0.0-rc.2"), "disagrees with")

    def test_a_legacy_backend_default(self):
        self.edit("shell/Cargo.toml", "default = []", 'default = ["legacy-backend"]')
        self.commit()
        self.assertRefused(self.run_release("--unsigned"), "enables legacy-backend")

    def test_a_malformed_version(self):
        self.assertRefused(self.run_release("--unsigned", version="v1.0"), "is not X.Y.Z")

    def test_a_final_version_without_sign(self):
        self.assertRefused(self.run_release(version="1.0.0"), "must be signed")

    def test_unsigned_on_a_final_version(self):
        self.assertRefused(self.run_release("--unsigned", version="1.0.0"), "--unsigned is for release candidates only")

    def test_release_signers_on_a_final_version(self):
        key, _ = self.env.keypair("k")
        signers = os.path.join(self.scratch, "signers")
        _write(signers, "# none\n")
        self.assertRefused(self.run_release("--sign", key, "--release-signers", signers, version="1.0.0"),
                           "--release-signers is for release candidates only")

    def test_a_candidate_with_neither_sign_nor_unsigned(self):
        self.assertRefused(self.run_release(), "give --sign KEY, or --unsigned")

    def test_sign_and_unsigned_together(self):
        key, _ = self.env.keypair("k")
        self.assertRefused(self.run_release("--sign", key, "--unsigned"), "not both")

    def test_a_sign_key_not_in_the_signers_file(self):
        key, _ = self.env.keypair("k")
        self.assertRefused(self.run_release("--sign", key), "is not listed in")

    def test_a_sign_key_that_is_a_login_key_compares_field_2_only(self):
        key, pub = self.env.keypair("k")
        kind, blob = pub.split()[:2]
        # The login key's own copy carries a comment the release key's does not: field 2 decides.
        _write(os.path.join(self.env.home, ".ssh", "id_ed25519.pub"), f"{kind} {blob} someone@laptop\n")
        signers = os.path.join(self.scratch, "signers")
        _write(signers, f'release@eitri namespaces="eitri-release" {kind} {blob}\n')
        self.assertRefused(self.run_release("--sign", key, "--release-signers", signers), "everyday login key")

    def test_a_sign_key_whose_pub_file_is_missing_is_read_with_ssh_keygen(self):
        key, _ = self.env.keypair("k")
        os.remove(key + ".pub")
        self.assertRefused(self.run_release("--sign", key), "is not listed in")

    def test_no_skia_pin(self):
        path = os.path.join(self.clone, "packaging", "pins.env")
        with open(path, encoding="utf-8") as f:
            lines = [line for line in f if not line.startswith("SKIA_BINARIES_SHA256=")]
        _write(path, "".join(lines))
        self.commit()
        self.assertRefused(self.run_release("--unsigned"), "no complete Skia pin")

    def test_a_verdandi_rev_that_is_not_40_hex(self):
        self.edit("agent/Cargo.toml", _VERDANDI_REV, _VERDANDI_REV[:7])
        self.commit()
        self.assertRefused(self.run_release("--unsigned"), "not a full 40-hex")

    def test_an_expected_verdandi_revision_that_disagrees(self):
        self.edit("agent/src/providers/claude_sidecar/spawn.rs", _VERDANDI_REV[:7], "fedcba9")
        self.commit()
        self.assertRefused(self.run_release("--unsigned"), "EXPECTED_VERDANDI_REVISION")

    def test_a_finished_release_is_never_rebuilt_in_place(self):
        _write(os.path.join(self.out, "v1.0.0-rc.1", "SHA256SUMS"), "x\n")
        proc = self.run_release("--unsigned")
        self.assertEqual(proc.returncode, 1, proc.stderr)
        self.assertIn("already holds a finished release", proc.stderr)
        self.assertEqual(self.docker_calls(), [])

    def test_verdandi_mirror_without_rehearsal(self):
        self.assertRefused(self.run_release("--unsigned", "--verdandi-mirror", self.scratch),
                           "--verdandi-mirror is for --rehearsal only")


class EmbeddedSignersRule(ReleaseShTestCase):
    """Task 1 (installer-claude-2): packaging/install.sh's embedded release-signers block must
    agree with the tracked packaging/release-signers, for every version; a final version also
    needs a real key in it. Each case refuses with its own message (the plan's acceptance)."""

    def test_a_drifted_embedded_block_refuses_on_a_release_candidate(self):
        # The tracked file gains a key; install.sh's fixture is left as it was -- the drift itself,
        # independent of whether this version needs a key at all.
        _write(os.path.join(self.clone, "packaging", "release-signers"),
               'release@eitri namespaces="eitri-release" ssh-ed25519 AAAAdrifted\n')
        self.commit()
        self.assertRefused(self.run_release("--unsigned"), "embedded release-signers block")

    def test_a_drifted_embedded_block_refuses_on_a_final_version(self):
        key, pub = self.env.keypair("release-key")
        self.edit("Cargo.toml", 'version = "1.0.0-rc.1"', 'version = "1.0.0"')
        _write(os.path.join(self.clone, "packaging", "release-signers"),
               f'release@eitri namespaces="eitri-release" {" ".join(pub.split()[:2])}\n')
        # install.sh's fixture is left holding the old (empty) block: drift, not a missing-key
        # question -- refused before release.sh ever looks at --sign's key.
        self.commit()
        self.assertRefused(self.run_release("--sign", key, version="1.0.0"), "embedded release-signers block")

    def test_a_final_version_with_no_key_anywhere_refuses_by_name(self):
        # Both sides agree (Task 1's drift check passes) and both are empty: the final-release-
        # specific rule, not the general "is not listed in" one below.
        key, _ = self.env.keypair("release-key")
        self.edit("Cargo.toml", 'version = "1.0.0-rc.1"', 'version = "1.0.0"')
        self.commit()
        self.assertRefused(self.run_release("--sign", key, version="1.0.0"),
                           "packaging/release-signers holds no key line")

    def test_a_final_version_whose_key_is_not_the_one_signing_refuses_as_a_foreign_key(self):
        # Both sides agree and both hold a key -- but not --sign's key: the existing
        # signers_list_blob refusal, which Task 1 leaves working (the plan's own words).
        key, _ = self.env.keypair("release-key")
        _, other_pub = self.env.keypair("other-key")
        self.edit("Cargo.toml", 'version = "1.0.0-rc.1"', 'version = "1.0.0"')
        self.write_signers(f'release@eitri namespaces="eitri-release" {" ".join(other_pub.split()[:2])}\n')
        self.commit()
        self.assertRefused(self.run_release("--sign", key, version="1.0.0"), "is not listed in")


class EmbeddedKeyBindsRcSigning(ReleaseShTestCase):
    """Fix round 1 (installer-claude-10 + [codex]): once packaging/release-signers holds a real
    key, a release candidate must actually be verifiable against it -- fill_notes' 'install.sh
    verifies it itself, with the release key built into it' claim describes a signature that must
    exist and must be by that key. Before this round, an rc with a key already embedded could still
    build --unsigned or against an unrelated --release-signers throwaway, so the shipped notes
    would lie. The version stays the fixture default (1.0.0-rc.1, an rc); every case here commits a
    real key into both packaging/release-signers and install.sh's embedded block first, via
    write_signers (so Task 1's own drift check never fires here -- these are about the *new* rule,
    layered on top of an already-agreeing pair)."""

    def commit_a_real_key(self):
        key, pub = self.env.keypair("release-key")
        self.write_signers(f'release@eitri namespaces="eitri-release" {" ".join(pub.split()[:2])}\n')
        self.commit()
        return key

    def test_unsigned_is_refused_once_a_key_is_embedded(self):
        self.commit_a_real_key()
        self.assertRefused(self.run_release("--unsigned"),
                            "packaging/release-signers already holds a release key")

    def test_a_release_signers_override_naming_a_different_key_is_refused(self):
        self.commit_a_real_key()
        throwaway, pub = self.env.keypair("throwaway")
        override = os.path.join(self.scratch, "throwaway-signers")
        _write(override, f'release@eitri namespaces="eitri-release" {" ".join(pub.split()[:2])}\n')
        proc = self.run_release("--sign", throwaway, "--release-signers", override)
        self.assertRefused(proc, "not listed there")
        self.assertIn("packaging/release-signers already holds a release key", proc.stderr)

    def test_a_release_signers_override_naming_the_same_key_is_not_refused(self):
        # The review's own wording: refuse an override "whose key is not in the embedded block" --
        # one that names the *same* key the embedded block already holds is not what this rule
        # exists to stop, and install.sh could verify it just as well as signing with no override.
        key = self.commit_a_real_key()
        same_key_signers = os.path.join(self.scratch, "same-key-signers")
        shutil.copy2(os.path.join(self.clone, "packaging", "release-signers"), same_key_signers)
        proc = self.run_release("--sign", key, "--release-signers", same_key_signers)
        self.assertReachedDocker(proc)

    def test_signing_directly_against_the_embedded_key_is_not_refused(self):
        # The ordinary, no-override case: still allowed once a key exists (the positive control for
        # the two refusals above).
        key = self.commit_a_real_key()
        self.assertReachedDocker(self.run_release("--sign", key))

    def test_rehearsal_may_still_run_unsigned_once_a_key_is_embedded(self):
        # --rehearsal is exempt (it always runs --unsigned and is never published); its own RELEASE
        # and notes already say REHEARSAL, and fill_notes renders the truthful "no key" text for it
        # because RS_SIGN_KEY is empty, not because release-signers is empty.
        self.commit_a_real_key()
        proc = self.run_release("--unsigned", "--rehearsal")
        self.assertReachedDocker(proc)


class TwinFreshnessRule(ReleaseShTestCase):
    """Task 5 (v1-dist, lane D): release_check.py's `twins` check is wired into preflight, over
    $RS_SOURCE's own root, before anything is built (docs/superpowers/plans/2026-09-28-v1-dist-
    task17-18.md's acceptance: 'editing an English guide without its twin fails ... release.sh').
    The fixture's own README.md/INSTALL.md and their Chinese twins (_twin_doc_files) start fresh --
    PositiveControls already proves that shape reaches docker; these prove each way of breaking it
    is refused first, by release_check's own message, with no docker call."""

    def test_editing_the_english_guide_without_its_twin_is_refused(self):
        # The exact scenario the plan's acceptance line names: README.md changes, README.zh-CN.md
        # (and its stale marker hash) does not follow.
        self.edit("README.md", "# fixture readme", "# fixture readme, edited")
        self.commit()
        proc = self.run_release("--unsigned")
        self.assertRefused(proc, "release_check twins")
        self.assertIn("stale", proc.stderr)

    def test_a_missing_required_twin_is_refused(self):
        os.remove(os.path.join(self.clone, "INSTALL.zh-CN.md"))
        self.commit()
        self.assertRefused(self.run_release("--unsigned"), "a missing twin of a required pair")

    def test_a_missing_required_english_file_is_refused(self):
        os.remove(os.path.join(self.clone, "README.md"))
        self.commit()
        self.assertRefused(self.run_release("--unsigned"), "a missing English file")

    def test_the_fresh_fixture_is_not_refused_by_the_twins_check(self):
        # The positive control: unmodified, the fixture's own twins reach docker (already implied
        # by PositiveControls, restated here so a regression in check_twins itself, not just in
        # this fixture's shape, is caught by this class on its own).
        self.assertReachedDocker(self.run_release("--unsigned"))


class LeakScanRule(ReleaseShTestCase):
    """publish/scan.sh is only in the private checkout. The public clone's own copy of release.sh
    passes the cmp refusal, so running it by mistake must not skip the host leak scan quietly
    (Task 4 review): without the scanner the run stops unless --no-leak-scan asks for that, and
    --no-leak-scan is refused wherever the scanner exists and for a final version."""

    def clone_copy(self):
        return os.path.join(self.clone, "packaging", "release.sh")

    def test_the_clone_copy_without_the_scanner_is_refused(self):
        self.assertFalse(os.path.exists(os.path.join(self.clone, "publish", "scan.sh")))
        self.assertRefused(self.run_release("--unsigned", script=self.clone_copy()),
                           "publish/scan.sh is not beside this release.sh")

    def test_no_leak_scan_lets_a_candidate_run_from_a_public_checkout_and_says_so(self):
        proc = self.run_release("--unsigned", "--no-leak-scan", script=self.clone_copy())
        self.assertReachedDocker(proc)
        self.assertIn("never publish them", proc.stderr)

    def test_no_leak_scan_where_the_scanner_exists(self):
        self.assertTrue(os.path.isfile(os.path.join(_PACKAGING, "..", "publish", "scan.sh")))
        self.assertRefused(self.run_release("--unsigned", "--no-leak-scan"), "where the scanner exists it always runs")

    def test_no_leak_scan_on_a_final_version(self):
        key, _ = self.env.keypair("k")
        self.assertRefused(self.run_release("--sign", key, "--no-leak-scan", version="1.0.0", script=self.clone_copy()),
                           "--no-leak-scan is for release candidates only")


class PublicCommitsRule(ReleaseShTestCase):
    """F7 (whole-branch review, claude-release-1). The `git push origin HEAD:main` a run prints
    publishes the released commit with every commit behind it the public repo lacks, and nothing looked
    at their metadata: a hand commit in the clone carrying a private identity, or a private path in its
    message, was built, signed and pushed. Preflight now holds HEAD to publish/commit.sh's public
    identity and scans HEAD and every commit reachable from HEAD or main and from no branch the public
    repo has, raw object and all, with publish/scan.sh. Fix round 1: the public repo's branches are
    read from it (git ls-remote; here the fixture's bare repo, through EITRI_PUBLIC_REPO_URL), never
    from the clone's own refs/remotes/origin/*, and HEAD is scanned even when the public repo has it."""

    # Something publish/scan.sh refuses in a commit message: an invented address (its email rule), in two
    # pieces so this file passes that rule itself. Never a real private value, whole or pieced -- scan.sh
    # joins pieced literals and runs its private rules on them.
    LEAK = "someone@" + "mail.co.uk"
    SOMEONE = ("Someone Else", "someone@example.com")

    def commit_as(self, message, author=None, committer=None):
        env = dict(self.env.vars)
        if author:
            env.update(GIT_AUTHOR_NAME=author[0], GIT_AUTHOR_EMAIL=author[1])
        if committer:
            env.update(GIT_COMMITTER_NAME=committer[0], GIT_COMMITTER_EMAIL=committer[1])
        with open(os.path.join(self.clone, "CHANGES"), "a", encoding="utf-8") as f:
            f.write(message + "\n")
        for argv in (["add", "-A"], ["commit", "-q", "-m", message]):
            subprocess.run(["git", "-C", self.clone, *argv], check=True, capture_output=True, env=env)

    def publish(self):
        """The public repo's main becomes the clone's HEAD, as a push would leave it."""
        self.env.git(self.clone, "push", "-q", self.env.public, "HEAD:refs/heads/main")
        self.env.git(self.clone, "update-ref", "refs/remotes/origin/main", "HEAD")

    def test_a_release_commit_with_a_foreign_author_is_refused(self):
        self.commit_as("Release candidate 1.0.0-rc.1", author=self.SOMEONE)
        self.assertRefused(self.run_release("--unsigned"), "Someone Else <someone@example.com>")

    def test_a_release_commit_with_a_foreign_committer_is_refused(self):
        self.commit_as("Release candidate 1.0.0-rc.1", committer=self.SOMEONE)
        self.assertRefused(self.run_release("--unsigned"), "not the public identity")

    def test_a_private_path_in_the_release_commits_message_is_refused(self):
        self.commit_as(f"Release candidate 1.0.0-rc.1, built in {self.LEAK}")
        self.assertRefused(self.run_release("--unsigned"), "does not pass publish/scan.sh")

    def test_a_private_path_in_an_earlier_commit_the_push_publishes_is_refused(self):
        self.commit_as(f"Tidy up {self.LEAK}")
        self.commit_as("Release candidate 1.0.0-rc.1")
        self.assertRefused(self.run_release("--unsigned"), "does not pass publish/scan.sh")

    def test_a_commit_the_public_repo_already_has_is_not_this_pushs_to_scan(self):
        self.commit_as(f"An old commit naming {self.LEAK}")
        self.publish()
        self.commit_as("Release candidate 1.0.0-rc.1")
        proc = self.run_release("--unsigned")
        self.assertReachedDocker(proc)
        # It says which base it used and how many commits it read.
        tip = subprocess.run(["git", "-C", self.clone, "rev-parse", "--short=12", "refs/remotes/origin/main"],
                             check=True, capture_output=True, text=True, env=self.env.vars).stdout.strip()
        self.assertIn(f"main at {tip}", proc.stderr)
        self.assertIn("1 commit(s)", proc.stderr)

    def test_the_clones_own_origin_refs_do_not_say_what_is_public(self):
        # A clone of a local clone (the plan's own rc.1 recipe): its origin/main is the release commit
        # itself, while the public repo has none of these commits. The old range was empty.
        self.commit_as(f"Tidy up {self.LEAK}")
        self.commit_as("Release candidate 1.0.0-rc.1")
        self.env.git(self.clone, "update-ref", "refs/remotes/origin/main", "HEAD")
        self.assertRefused(self.run_release("--unsigned"), "does not pass publish/scan.sh")

    def test_the_release_commit_is_scanned_even_when_the_public_repo_has_it(self):
        self.commit_as(f"Release candidate 1.0.0-rc.1, built in {self.LEAK}")
        self.publish()
        self.assertRefused(self.run_release("--unsigned"), "does not pass publish/scan.sh")

    def test_main_ahead_of_a_detached_head_is_scanned(self):
        # `git push origin main`, typed by habit instead of the printed HEAD:main, publishes main.
        self.commit_as("Release candidate 1.0.0-rc.1")
        self.commit_as(f"Follow-up in {self.LEAK}")
        self.env.git(self.clone, "checkout", "-q", "--detach", "HEAD~1")
        self.assertRefused(self.run_release("--unsigned"), "does not pass publish/scan.sh")

    def test_a_public_repo_that_cannot_be_read_is_refused(self):
        self.commit_as("Release candidate 1.0.0-rc.1")
        self.env.vars["EITRI_PUBLIC_REPO_URL"] = os.path.join(self.scratch, "no-such-repo.git")
        self.assertRefused(self.run_release("--unsigned"), "cannot read the branches of")

    def stand_in(self, rev, message, ref_base="refs/replace/"):
        """A sanitized stand-in for REV, as `git replace --edit` makes one -- the same tree and
        parents, the public identity, MESSAGE -- recorded under REF_BASE<REV's sha>."""
        def git_out(*argv, stdin=None):
            return subprocess.run(["git", "-C", self.clone, *argv], check=True, capture_output=True, text=True,
                                  input=stdin, env=self.env.vars).stdout
        sha = git_out("rev-parse", rev).strip()
        headers = git_out("cat-file", "commit", sha).partition("\n\n")[0].splitlines()
        ident = f"{_PUBLIC_NAME} <{_PUBLIC_EMAIL}> 1790000000 +0000"
        text = "\n".join([l for l in headers if l.startswith(("tree ", "parent "))]
                         + [f"author {ident}", f"committer {ident}"]) + f"\n\n{message}\n"
        new = git_out("hash-object", "-t", "commit", "-w", "--stdin", stdin=text).strip()
        self.env.git(self.clone, "update-ref", ref_base + sha, new)

    # Fix round 2 ([codex]): a replacement changes what git log, rev-list and cat-file read, never what
    # a push sends, so a commit "fixed" with `git replace --edit` was scanned as its stand-in and pushed
    # as the original.
    def test_a_replacement_hiding_a_leaky_commit_is_refused(self):
        self.commit_as(f"Tidy up {self.LEAK}")
        self.stand_in("HEAD", "Tidy up")
        self.commit_as("Release candidate 1.0.0-rc.1")
        self.assertRefused(self.run_release("--unsigned"), "replacement refs")

    def test_a_replacement_under_another_ref_base_is_read_past(self):
        # GIT_REPLACE_REF_BASE moves where git looks for replacements, so no refs/replace/ ref exists:
        # the reads themselves must ignore replacements.
        self.env.vars["GIT_REPLACE_REF_BASE"] = "refs/elsewhere/"
        self.commit_as(f"Tidy up {self.LEAK}")
        self.stand_in("HEAD", "Tidy up", ref_base="refs/elsewhere/")
        self.commit_as("Release candidate 1.0.0-rc.1")
        self.assertRefused(self.run_release("--unsigned"), "does not pass publish/scan.sh")

    def test_a_replaced_release_commit_is_held_to_its_own_identity(self):
        self.env.vars["GIT_REPLACE_REF_BASE"] = "refs/elsewhere/"
        self.commit_as("Release candidate 1.0.0-rc.1", author=self.SOMEONE)
        self.stand_in("HEAD", "Release candidate 1.0.0-rc.1", ref_base="refs/elsewhere/")
        self.assertRefused(self.run_release("--unsigned"), "Someone Else <someone@example.com>")

    def test_no_leak_scan_skips_it_with_the_rest_of_the_leak_scan_and_says_so(self):
        self.commit_as("Release candidate 1.0.0-rc.1", author=self.SOMEONE)
        proc = self.run_release("--unsigned", "--no-leak-scan",
                                script=os.path.join(self.clone, "packaging", "release.sh"))
        self.assertReachedDocker(proc)
        self.assertIn("not checked for a private identity or path", proc.stderr)


class ForkHistoryRule(ReleaseShTestCase):
    """The neovide submodule is published with every commit behind the commit the clone records for it,
    and nothing read those commits' metadata: a fork commit made from a checkout with a private git
    identity, or with an assistant trailer, shipped unseen. Preflight now hands the recorded commit to
    publish/fork-guard.sh (upstream Neovide here is the fixture's bare repo, through
    EITRI_UPSTREAM_NEOVIDE_URL). Nothing is rewritten: a refusal is the end of it."""

    def fork_commit(self, message, author=None):
        """A new commit in the submodule, recorded in the clone's own commit the way a real update is."""
        sub = os.path.join(self.clone, "neovide")
        env = dict(self.env.vars)
        if author:
            env.update(GIT_AUTHOR_NAME=author[0], GIT_AUTHOR_EMAIL=author[1])
        with open(os.path.join(sub, "CHANGES"), "a", encoding="utf-8") as f:
            f.write(message + "\n")
        for argv in (["add", "-A"], ["commit", "-q", "-m", message]):
            subprocess.run(["git", "-C", sub, *argv], check=True, capture_output=True, env=env)
        self.commit("Update the fork")

    def test_a_fork_commit_by_a_foreign_identity_is_refused(self):
        self.fork_commit("fork change", author=("Someone Else", "someone@example.com"))
        self.assertRefused(self.run_release("--unsigned"), "neither the public identity nor an upstream contributor")

    def test_a_claude_trailer_on_a_fork_commit_is_refused(self):
        # The header is joined at run time: this file ships in the public tree, and the leak scan reads a
        # literal trailer, or one written in pieces, as a real one.
        trailer = "-".join(("Co", "Authored", "By"))
        self.fork_commit(f"fork change\n\n{trailer}: Claude <noreply@example.com>")
        self.assertRefused(self.run_release("--unsigned"), "attribution trailer")

    def test_a_fork_commit_by_the_public_identity_passes(self):
        self.fork_commit("fork change")
        self.assertReachedDocker(self.run_release("--unsigned"))

    def test_an_upstream_that_cannot_be_read_is_refused(self):
        self.env.vars["EITRI_UPSTREAM_NEOVIDE_URL"] = os.path.join(self.scratch, "nowhere.git")
        self.assertRefused(self.run_release("--unsigned"), "cannot fetch main")

    def test_a_shallow_submodule_checkout_is_refused(self):
        shallow = os.path.join(self.clone, "neovide", ".git")
        with open(shallow, encoding="utf-8") as f:
            gitdir = f.read().split("gitdir:")[1].strip()
        gitdir = os.path.normpath(os.path.join(os.path.dirname(shallow), gitdir))
        with open(os.path.join(gitdir, "shallow"), "w", encoding="utf-8") as f:
            f.write(subprocess.run(["git", "-C", os.path.join(self.clone, "neovide"), "rev-parse", "HEAD"],
                                   capture_output=True, text=True, env=self.env.vars).stdout)
        self.assertRefused(self.run_release("--unsigned"), "is a shallow checkout")

    def test_no_leak_scan_skips_it_with_the_rest_of_the_leak_scan(self):
        self.fork_commit("fork change", author=("Someone Else", "someone@example.com"))
        proc = self.run_release("--unsigned", "--no-leak-scan",
                                script=os.path.join(self.clone, "packaging", "release.sh"))
        self.assertReachedDocker(proc)
        self.assertIn("the fork's commits are not checked", proc.stderr)


class RehearsalRule(ReleaseShTestCase):
    def drop_nvim_pin(self):
        path = os.path.join(self.clone, "packaging", "pins.env")
        with open(path, encoding="utf-8") as f:
            text = f.read()
        self.assertIn(_NVIM_PIN, text)
        text = text.replace(_NVIM_PIN, "")
        self.assertFalse([l for l in text.splitlines() if l.startswith("NVIM_")], "the fixture kept an nvim pin")
        _write(path, text)
        self.commit()

    def test_a_normal_run_needs_the_nvim_pin(self):
        self.drop_nvim_pin()
        self.assertRefused(self.run_release("--unsigned"), "only a --rehearsal may run without it")

    def test_a_rehearsal_runs_without_the_nvim_pin_and_says_so(self):
        self.drop_nvim_pin()
        proc = self.run_release("--unsigned", "--rehearsal")
        self.assertReachedDocker(proc)
        self.assertIn("carries no NVIM_* fields", proc.stderr)
        self.assertTrue(os.path.isdir(os.path.join(self.out, "v1.0.0-rc.1-rehearsal")))
        self.assertFalse(os.path.exists(os.path.join(self.out, "v1.0.0-rc.1")))

    def test_a_rehearsal_only_for_a_release_candidate(self):
        key, _ = self.env.keypair("k")
        self.assertRefused(self.run_release("--sign", key, "--rehearsal", version="1.0.0"),
                           "--rehearsal is for release candidates only")

    def test_a_rehearsal_only_unsigned(self):
        key, pub = self.env.keypair("k")
        signers = os.path.join(self.scratch, "signers")
        _write(signers, f'release@eitri namespaces="eitri-release" {" ".join(pub.split()[:2])}\n')
        self.assertRefused(self.run_release("--sign", key, "--release-signers", signers, "--rehearsal"),
                           "--rehearsal runs --unsigned only")


def _bash(script, env, cwd):
    """Run `script` in bash with release.sh's functions loaded (sourcing it runs no release)."""
    return subprocess.run(["bash", "-c", f'set -euo pipefail; source "{_RELEASE_SH}"; {script}'],
                          capture_output=True, text=True, env=env, cwd=cwd, timeout=60)


class PhaseFCopyRebuilds(unittest.TestCase):
    """F6 (whole-branch review, claude-release-0). Phase F copies the clone into /build/src, and phase
    B builds there into the persistent $RS_ROOT/target. `cp -a` keeps the clone's own mtimes (its
    checkout time), and cargo takes a path crate as fresh when none of its sources is newer than its
    last build in that target -- so a tree checked out before some later build in the same --out root
    shipped that build's code under this commit's RELEASE. ctr_copy_source, phase F's copy, must make
    every path crate rebuild. A real two-crate workspace, built twice at one path into one target, as
    phases F and B do; no network (no dependencies)."""

    def setUp(self):
        if shutil.which("cargo") is None:
            self.skipTest("cargo is not installed")
        self.scratch = _scratch_dir()
        self.addCleanup(shutil.rmtree, self.scratch, ignore_errors=True)
        self.dest = os.path.join(self.scratch, "build", "src")
        self.target = os.path.join(self.scratch, "target")
        # The caller's own HOME stays: cargo may be a rustup proxy that finds its toolchain there.
        self.vars = dict(os.environ, CARGO_TARGET_DIR=self.target, CARGO_HOME=os.path.join(self.scratch, "cargo-home"),
                         CARGO_NET_OFFLINE="true")

    def tree(self, name, message, mtime=None):
        root = os.path.join(self.scratch, name)
        files = {
            "Cargo.toml": '[workspace]\nresolver = "2"\nmembers = ["app", "lib"]\n',
            "app/Cargo.toml": '[package]\nname = "app"\nversion = "0.1.0"\nedition = "2021"\n\n'
                              '[dependencies]\nlib = { path = "../lib" }\n',
            "app/src/main.rs": 'fn main() {\n    println!("{}", lib::message());\n}\n',
            "lib/Cargo.toml": '[package]\nname = "lib"\nversion = "0.1.0"\nedition = "2021"\n',
            "lib/src/lib.rs": f'pub fn message() -> &\'static str {{\n    "{message}"\n}}\n',
        }
        for rel, text in files.items():
            _write(os.path.join(root, rel), text)
            if mtime is not None:
                os.utime(os.path.join(root, rel), (mtime, mtime))
        return root

    def build_from(self, tree, copy="ctr_copy_source"):
        shutil.rmtree(self.dest, ignore_errors=True)
        os.makedirs(self.dest)
        proc = _bash(f'{copy} "{tree}" "{self.dest}"', self.vars, self.scratch)
        self.assertEqual(proc.returncode, 0, proc.stderr)
        subprocess.run(["cargo", "build", "--offline", "-q"], cwd=self.dest, env=self.vars, check=True,
                       capture_output=True, timeout=300)
        return subprocess.run([os.path.join(self.target, "debug", "app")], capture_output=True, text=True,
                              check=True).stdout.strip()

    def test_a_tree_older_than_the_last_build_in_the_target_ships_its_own_code(self):
        self.assertEqual(self.build_from(self.tree("newer", "built first")), "built first")
        older = self.tree("older", "checked out earlier", mtime=time.time() - 30 * 86400)
        self.assertEqual(self.build_from(older), "checked out earlier")

    def test_a_plain_cp_a_ships_the_earlier_build_on_this_cargo(self):
        # The control: the scenario is real here, so the test above would fail without the fix.
        plain = 'plain_copy() { cp -a "$1/." "$2/"; }; plain_copy'
        self.assertEqual(self.build_from(self.tree("newer", "built first"), copy=plain), "built first")
        older = self.tree("older", "checked out earlier", mtime=time.time() - 30 * 86400)
        self.assertEqual(self.build_from(older, copy=plain), "built first")

    def test_phase_f_copies_the_clone_with_it(self):
        proc = _bash("declare -f ctr_phase_f", self.vars, self.scratch)
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertIn("ctr_copy_source /src /build/src", proc.stdout)
        self.assertNotIn("cp -a", proc.stdout)


class StagesTheDesktopEntryAndIcons(unittest.TestCase):
    """Phase B's ctr_stage_art and ctr_tarball_art (the app icon, 2026-10-01), run for real over this
    repository's own packaging/ files: the staging dir holds every source nfpm-public.yaml names for the
    desktop entry and the icons, and the tarball tree holds exactly the paths release_check.py's role
    table expects -- no more (a stray file there fails check-assets), no fewer -- with the committed
    bytes."""

    def setUp(self):
        self.scratch = _scratch_dir()
        self.addCleanup(shutil.rmtree, self.scratch, ignore_errors=True)
        self.clone = os.path.join(self.scratch, "clone")
        os.makedirs(os.path.join(self.clone, "packaging"))
        shutil.copytree(os.path.join(_PACKAGING, "icons"), os.path.join(self.clone, "packaging", "icons"))
        shutil.copy(os.path.join(_PACKAGING, "cn.huntergrey.eitri.desktop"), os.path.join(self.clone, "packaging"))
        shutil.copy(os.path.join(_PACKAGING, "cn.huntergrey.eitri.Panel.desktop"), os.path.join(self.clone, "packaging"))
        shutil.copytree(os.path.join(_PACKAGING, "..", "nvim", "eitri.nvim"),
                        os.path.join(self.clone, "nvim", "eitri.nvim"))
        # The whole extension directory, test code and notes included: only its four shipped files may be staged.
        shutil.copytree(os.path.join(_PACKAGING, "..", "gnome-extension"), os.path.join(self.clone, "gnome-extension"))
        shutil.copytree(os.path.join(_PACKAGING, "legacy"), os.path.join(self.clone, "packaging", "legacy"))
        self.st = os.path.join(self.scratch, "stage")
        self.tree = os.path.join(self.scratch, "tarball", "top")
        os.makedirs(os.path.join(self.st, "packaging"))
        proc = _bash(f'ctr_stage_art "{self.st}"; ctr_tarball_art "{self.st}" "{self.tree}"', dict(os.environ),
                     self.clone)
        self.assertEqual(proc.returncode, 0, proc.stderr)

    @staticmethod
    def files(root):
        return {os.path.relpath(os.path.join(d, f), root) for d, _, names in os.walk(root) for f in names}

    def test_the_staging_dir_holds_every_source_the_public_profile_names(self):
        with open(os.path.join(_PACKAGING, "nfpm-public.yaml"), encoding="utf-8") as f:
            srcs = re.findall(rf"^\s*- src: \./((?:packaging/(?:icons/|{re.escape(rc.APP_ID)}(?:\.Panel)?\.desktop)|nvim/eitri\.nvim/|gnome-extension/)\S*)\s*$",
                              f.read(), re.M)
        # The desktop entry, the panel's entry, nine icons, the plugin's three files and the extension's four.
        self.assertEqual(len(srcs), 18, srcs)
        for src in srcs:
            self.assertTrue(os.path.isfile(os.path.join(self.st, src)), src)
        # The staging dir also holds the legacy entry (for the tarball's own copy of it), which no
        # profile names: a package never ships it.
        self.assertEqual({p for p in self.files(self.st)}, set(srcs) | {"packaging/legacy/eitri.desktop"})
        for profile in ("nfpm.yaml", "nfpm-public.yaml"):
            with open(os.path.join(_PACKAGING, profile), encoding="utf-8") as f:
                entries = [ln for ln in f.read().splitlines() if re.match(r"\s*(- )?(src|dst):", ln)]
            self.assertEqual([ln for ln in entries if "packaging/legacy" in ln or "/eitri.desktop" in ln], [], profile)

    def test_the_tarball_tree_is_exactly_the_roles_release_check_expects(self):
        top = "eitri-1.0.0-x86_64-linux/"
        roles = rc.tarball_roles("1.0.0")
        want = {path[len(top):] for role, path in roles.items()
                if role in ("desktop", "panel-desktop", "legacy-desktop") or role.startswith(("icon-", "plugin-", "gnome-ext-"))}
        self.assertIn("share/applications/eitri.desktop", want)
        # Nothing of the extension's test code or notes is in the tarball tree.
        for name in ("testing.js", "README.md"):
            self.assertFalse([p for p in self.files(self.tree) if p.endswith("/" + name)], name)
        self.assertFalse([p for p in self.files(self.tree) if "/test/" in p])
        self.assertEqual(self.files(self.tree), want)
        for role, tracked in rc.TRACKED_ART.items():
            with open(os.path.join(_PACKAGING, "..", tracked), "rb") as a, \
                    open(os.path.join(self.tree, roles[role][len(top):]), "rb") as b:
                self.assertEqual(a.read(), b.read(), role)
        # Modes: read, never executed (the nfpm packages and install.sh rely on a plain file).
        for rel in self.files(self.tree):
            self.assertEqual(os.stat(os.path.join(self.tree, rel)).st_mode & 0o777, 0o644, rel)


class SignAndVerify(unittest.TestCase):
    """The same sign_sums/verify_sums/write_sums release.sh runs, with a throwaway key."""

    def setUp(self):
        self.scratch = _scratch_dir()
        self.addCleanup(shutil.rmtree, self.scratch, ignore_errors=True)
        self.env = _Env(self.scratch)
        self.rel = os.path.join(self.scratch, "rel")
        _write(os.path.join(self.rel, "eitri-1.0.0-rc.1-x86_64-linux.tar.gz"), "tarball\n")
        _write(os.path.join(self.rel, "install.sh"), "installer\n")
        self.key, pub = self.env.keypair("rc-key")
        self.signers = os.path.join(self.scratch, "rc-signers")
        _write(self.signers, f'release@eitri namespaces="eitri-release" {" ".join(pub.split()[:2])}\n')

    def sign(self):
        return _bash(f'write_sums "{self.rel}" eitri-1.0.0-rc.1-x86_64-linux.tar.gz install.sh; '
                     f'sign_sums "{self.key}" "{self.rel}"', self.env.vars, self.scratch)

    def test_sums_are_plain_two_space_lines_in_the_order_given(self):
        proc = self.sign()
        self.assertEqual(proc.returncode, 0, proc.stderr)
        with open(os.path.join(self.rel, "SHA256SUMS"), encoding="utf-8") as f:
            lines = f.read().splitlines()
        self.assertEqual([line.split("  ")[1] for line in lines],
                         ["eitri-1.0.0-rc.1-x86_64-linux.tar.gz", "install.sh"])
        for line in lines:
            self.assertRegex(line, r"^[0-9a-f]{64}  [A-Za-z0-9._+-]+$")

    def test_a_signature_verifies_against_the_signers_file_used(self):
        self.assertEqual(self.sign().returncode, 0)
        proc = _bash(f'verify_sums "{self.signers}" "{self.rel}"', self.env.vars, self.scratch)
        self.assertEqual(proc.returncode, 0, proc.stderr)
        # The installer's own command, verbatim (spec sec 6.4), agrees.
        with open(os.path.join(self.rel, "SHA256SUMS"), "rb") as sums:
            direct = subprocess.run(["ssh-keygen", "-Y", "verify", "-f", self.signers, "-I", "release@eitri",
                                     "-n", "eitri-release", "-s", os.path.join(self.rel, "SHA256SUMS.sig")],
                                    stdin=sums, capture_output=True, env=self.env.vars)
        self.assertEqual(direct.returncode, 0, direct.stderr)

    def test_a_changed_sums_file_does_not_verify(self):
        self.assertEqual(self.sign().returncode, 0)
        with open(os.path.join(self.rel, "SHA256SUMS"), "a", encoding="utf-8") as f:
            f.write("0" * 64 + "  extra\n")
        proc = _bash(f'verify_sums "{self.signers}" "{self.rel}"', self.env.vars, self.scratch)
        self.assertNotEqual(proc.returncode, 0)

    def test_a_signers_file_naming_another_key_does_not_verify(self):
        self.assertEqual(self.sign().returncode, 0)
        _, other = self.env.keypair("other")
        other_signers = os.path.join(self.scratch, "other-signers")
        _write(other_signers, f'release@eitri namespaces="eitri-release" {" ".join(other.split()[:2])}\n')
        proc = _bash(f'verify_sums "{other_signers}" "{self.rel}"', self.env.vars, self.scratch)
        self.assertNotEqual(proc.returncode, 0)


class PassphraseKeySigning(unittest.TestCase):
    """The release key is passphrase-protected: sign_sums must work with the passphrase typed on a
    terminal, and survive one mistyped attempt. ssh-keygen -Y sign asks once and then dies, which at
    the end of a whole build would leave SHA256SUMS written and prepare_dirs refusing to rebuild."""

    def setUp(self):
        self.scratch = _scratch_dir()
        self.addCleanup(shutil.rmtree, self.scratch, ignore_errors=True)
        self.env = _Env(self.scratch)
        self.rel = os.path.join(self.scratch, "rel")
        _write(os.path.join(self.rel, "SHA256SUMS"), "0" * 64 + "  install.sh\n")
        self.key = os.path.join(self.scratch, "eitri-release")
        subprocess.run(["ssh-keygen", "-q", "-t", "ed25519", "-N", "testpass", "-C", "", "-f", self.key],
                       check=True, capture_output=True, env=self.env.vars)
        with open(self.key + ".pub", encoding="utf-8") as f:
            pub = " ".join(f.read().split()[:2])
        self.signers = os.path.join(self.scratch, "signers")
        _write(self.signers, f'release@eitri namespaces="eitri-release" {pub}\n')
        # No agent of the caller's may answer for the key, and no askpass program may stand in for a tty.
        self.vars = {k: v for k, v in self.env.vars.items() if k not in ("SSH_AUTH_SOCK", "SSH_ASKPASS", "DISPLAY")}

    def sign_on_a_tty(self, passphrases):
        """sign_sums under a pseudo-terminal, typing each of `passphrases` at a prompt in turn.
        Returns (exit status, everything the terminal showed)."""
        import pty
        import select
        pid, fd = pty.fork()
        if pid == 0:
            os.chdir(self.scratch)
            os.execvpe("bash", ["bash", "-c", f'set -euo pipefail; source "{_RELEASE_SH}"; '
                                              f'sign_sums "{self.key}" "{self.rel}"'], self.vars)
        shown = b""
        typed = 0
        deadline = time.time() + 30
        try:
            while time.time() < deadline:
                ready, _, _ = select.select([fd], [], [], 0.2)
                if not ready:
                    continue
                try:
                    data = os.read(fd, 4096)
                except OSError:
                    break
                if not data:
                    break
                shown += data
                # Count prompts, not the word "passphrase": ssh-keygen's error says it too.
                prompts = shown.count(b"Enter passphrase for")
                if typed < len(passphrases) and prompts > typed and shown.rstrip().endswith(b":"):
                    time.sleep(0.2)
                    os.write(fd, (passphrases[typed] + "\n").encode())
                    typed += 1
        finally:
            try:
                os.kill(pid, 9)
            except ProcessLookupError:
                pass
            _, status = os.waitpid(pid, 0)
        # A kill after a clean exit still reports the exit status, not the signal.
        return os.waitstatus_to_exitcode(status), shown.decode(errors="replace")

    def verify(self):
        return _bash(f'verify_sums "{self.signers}" "{self.rel}"', self.env.vars, self.scratch)

    def test_the_passphrase_typed_on_a_terminal_signs_and_the_signature_verifies(self):
        code, shown = self.sign_on_a_tty(["testpass"])
        self.assertEqual(code, 0, shown)
        self.assertIn('Enter passphrase for "%s": ' % self.key, shown)
        self.assertEqual(self.verify().returncode, 0)

    def test_a_mistyped_passphrase_is_asked_for_again(self):
        code, shown = self.sign_on_a_tty(["wrong", "testpass"])
        self.assertEqual(code, 0, shown)
        self.assertIn("incorrect passphrase", shown)
        self.assertIn("trying again (attempt 2 of 3)", shown)
        self.assertEqual(self.verify().returncode, 0)

    def test_three_wrong_passphrases_end_the_run_naming_the_key(self):
        code, shown = self.sign_on_a_tty(["a", "b", "c"])
        self.assertNotEqual(code, 0, shown)
        self.assertIn("after 3 attempts", shown)
        self.assertFalse(os.path.exists(os.path.join(self.rel, "SHA256SUMS.sig")))

    def test_no_terminal_and_no_agent_fails_naming_both_ways_out(self):
        proc = subprocess.run(["setsid", "-w", "bash", "-c", f'set -euo pipefail; source "{_RELEASE_SH}"; '
                                                            f'sign_sums "{self.key}" "{self.rel}"'],
                              capture_output=True, text=True, env=self.vars, cwd=self.scratch,
                              stdin=subprocess.DEVNULL, timeout=60)
        self.assertNotEqual(proc.returncode, 0)
        self.assertIn("a terminal on stdin, or the key loaded into ssh-agent", proc.stderr)

    def test_a_key_loaded_into_an_agent_signs_without_any_prompt(self):
        sock = os.path.join(self.scratch, "agent.sock")
        started = subprocess.run(["ssh-agent", "-a", sock, "-s"], capture_output=True, text=True, env=self.vars)
        self.assertEqual(started.returncode, 0, started.stderr)
        agent_pid = int(started.stdout.split("SSH_AGENT_PID=")[1].split(";")[0])
        self.addCleanup(os.kill, agent_pid, 15)
        # ssh-add needs the passphrase once, on a terminal; the release then needs no terminal at all.
        import pty
        pid, fd = pty.fork()
        if pid == 0:
            os.execvpe("ssh-add", ["ssh-add", self.key], dict(self.vars, SSH_AUTH_SOCK=sock))
        seen = b""
        deadline = time.time() + 30
        sent = False
        while time.time() < deadline:
            try:
                data = os.read(fd, 4096)
            except OSError:
                break
            if not data:
                break
            seen += data
            if not sent and b"passphrase" in seen:
                time.sleep(0.2)
                os.write(fd, b"testpass\n")
                sent = True
        _, status = os.waitpid(pid, 0)
        self.assertEqual(os.waitstatus_to_exitcode(status), 0, seen)
        proc = subprocess.run(["setsid", "-w", "bash", "-c", f'set -euo pipefail; source "{_RELEASE_SH}"; '
                                                            f'sign_sums "{self.key}" "{self.rel}"'],
                              capture_output=True, text=True, env=dict(self.vars, SSH_AUTH_SOCK=sock),
                              cwd=self.scratch, stdin=subprocess.DEVNULL, timeout=60)
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertEqual(self.verify().returncode, 0)


class SignSumsRetries(unittest.TestCase):
    """sign_sums's retry loop alone, with an ssh-keygen that fails a set number of times first."""

    def setUp(self):
        self.scratch = _scratch_dir()
        self.addCleanup(shutil.rmtree, self.scratch, ignore_errors=True)
        self.env = _Env(self.scratch)
        self.rel = os.path.join(self.scratch, "rel")
        _write(os.path.join(self.rel, "SHA256SUMS"), "0" * 64 + "  install.sh\n")
        self.key, _ = self.env.keypair("k")
        self.count = os.path.join(self.scratch, "count")
        real = shutil.which("ssh-keygen")
        _write(os.path.join(self.scratch, "fakebin", "ssh-keygen"),
               '#!/bin/sh\n'
               f'n=$(cat "{self.count}" 2>/dev/null || echo 0); n=$((n + 1)); echo "$n" > "{self.count}"\n'
               'if [ "$1" = -Y ] && [ "$2" = sign ] && [ "$n" -le "$FAILS" ]; then\n'
               '\techo "Load key: incorrect passphrase supplied to decrypt private key" >&2; exit 255\n'
               'fi\n'
               f'exec "{real}" "$@"\n', 0o755)

    def sign(self, fails):
        env = dict(self.env.vars, FAILS=str(fails),
                   PATH=os.path.join(self.scratch, "fakebin") + os.pathsep + self.env.vars["PATH"])
        return _bash(f'sign_sums "{self.key}" "{self.rel}"', env, self.scratch)

    def calls(self):
        with open(self.count, encoding="utf-8") as f:
            return int(f.read())

    def test_a_signature_that_succeeds_first_time_is_not_repeated(self):
        proc = self.sign(0)
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertEqual(self.calls(), 1)
        self.assertNotIn("trying again", proc.stderr)

    def test_two_failures_then_a_success(self):
        proc = self.sign(2)
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertEqual(self.calls(), 3)
        self.assertIn("trying again (attempt 2 of 3)", proc.stderr)
        self.assertIn("trying again (attempt 3 of 3)", proc.stderr)
        self.assertTrue(os.path.getsize(os.path.join(self.rel, "SHA256SUMS.sig")) > 0)

    def test_three_failures_stop_after_exactly_three_attempts(self):
        proc = self.sign(99)
        self.assertNotEqual(proc.returncode, 0)
        self.assertEqual(self.calls(), 3)
        self.assertIn("ssh-keygen could not sign", proc.stderr)
        self.assertIn("after 3 attempts", proc.stderr)
        self.assertFalse(os.path.exists(os.path.join(self.rel, "SHA256SUMS.sig")))


class VerifyReleaseSignature(unittest.TestCase):
    """verify_release_signature, which host_main runs right after sign_sums. Review of fix round 1:
    preflight's signers_list_blob matches a --release-signers override against packaging/
    release-signers by the key blob alone, and the signature used to be verified against the
    override only. An override listing the embedded key's blob under the right principal and
    namespace, while packaging/release-signers lists it under another, passed at release time --
    and the shipped install.sh, which verifies against packaging/release-signers (preflight holds
    it byte-equal to the embedded block), refused the release. Now the signature is also verified
    against packaging/release-signers whenever that file holds a key: exactly install.sh's check."""

    def setUp(self):
        self.scratch = _scratch_dir()
        self.addCleanup(shutil.rmtree, self.scratch, ignore_errors=True)
        self.env = _Env(self.scratch)
        self.rel = os.path.join(self.scratch, "rel")
        _write(os.path.join(self.rel, "install.sh"), "installer\n")
        self.key, pub = self.env.keypair("release-key")
        self.pubkey = " ".join(pub.split()[:2])
        self.override = os.path.join(self.scratch, "override-signers")
        _write(self.override, f'release@eitri namespaces="eitri-release" {self.pubkey}\n')
        self.source = os.path.join(self.scratch, "clone")
        self.tracked = os.path.join(self.source, "packaging", "release-signers")
        proc = _bash(f'write_sums "{self.rel}" install.sh; sign_sums "{self.key}" "{self.rel}"',
                     self.env.vars, self.scratch)
        self.assertEqual(proc.returncode, 0, proc.stderr)

    def verify(self, signers):
        env = dict(self.env.vars, RS_SOURCE=self.source, RS_SIGNERS=signers)
        return _bash(f'verify_release_signature "{self.rel}"', env, self.scratch)

    def test_an_override_whose_blob_the_tracked_file_lists_under_another_principal_is_refused(self):
        _write(self.tracked, f'someone@else namespaces="eitri-release" {self.pubkey}\n')
        # Both checks preflight and the old host_main made pass: the blob is listed in the tracked
        # file, and the signature verifies against the override.
        self.assertEqual(subprocess.run(["bash", "-c", f'source "{_RELEASE_SH}"; '
                                         f'signers_list_blob "{self.tracked}" "{self.pubkey.split()[1]}"'],
                                        env=self.env.vars).returncode, 0)
        self.assertEqual(_bash(f'verify_sums "{self.override}" "{self.rel}"', self.env.vars,
                               self.scratch).returncode, 0)
        proc = self.verify(self.override)
        self.assertNotEqual(proc.returncode, 0)
        self.assertIn("its own installer would refuse", proc.stderr)
        self.assertIn(self.tracked, proc.stderr)

    def test_an_override_whose_blob_the_tracked_file_lists_under_another_namespace_is_refused(self):
        _write(self.tracked, f'release@eitri namespaces="something-else" {self.pubkey}\n')
        proc = self.verify(self.override)
        self.assertNotEqual(proc.returncode, 0)
        self.assertIn("its own installer would refuse", proc.stderr)

    def test_an_override_agreeing_with_the_tracked_key_verifies(self):
        _write(self.tracked, f'release@eitri namespaces="eitri-release" {self.pubkey}\n')
        proc = self.verify(self.override)
        self.assertEqual(proc.returncode, 0, proc.stderr)

    def test_a_throwaway_rc_with_no_key_in_the_tracked_file_is_checked_against_the_override_only(self):
        # rc.1's shape: nothing embedded yet, a throwaway key named by --release-signers.
        _write(self.tracked, "# no key line yet\n")
        proc = self.verify(self.override)
        self.assertEqual(proc.returncode, 0, proc.stderr)

    def test_a_signature_the_signers_file_used_does_not_accept_still_refuses(self):
        _, other = self.env.keypair("other-key")
        _write(self.tracked, "# no key line yet\n")
        other_signers = os.path.join(self.scratch, "other-signers")
        _write(other_signers, f'release@eitri namespaces="eitri-release" {" ".join(other.split()[:2])}\n')
        proc = self.verify(other_signers)
        self.assertNotEqual(proc.returncode, 0)
        self.assertIn("does not verify against " + other_signers, proc.stderr)


class VerdandiPublicRevision(unittest.TestCase):
    """verdandi_fetch_public, which phase F runs against the persistent verdandi.git cache. A
    rehearsal's --verdandi-mirror fetches into the same cache, so the commit object alone proves
    nothing (Task 4 review): the revision must be reachable from a branch the URL has now."""

    def setUp(self):
        self.scratch = _scratch_dir()
        self.addCleanup(shutil.rmtree, self.scratch, ignore_errors=True)
        self.env = _Env(self.scratch)
        self.public = os.path.join(self.scratch, "public")
        subprocess.run(["git", "init", "-q", self.public], check=True, env=self.env.vars)
        _write(os.path.join(self.public, "README"), "published\n")
        self.env.git(self.public, "add", "-A")
        self.env.git(self.public, "commit", "-q", "-m", "published")
        self.published = self.rev(self.public)
        # The mirror holds one commit more on the same branch name: pushed nowhere yet.
        self.mirror = os.path.join(self.scratch, "mirror")
        subprocess.run(["git", "clone", "-q", self.public, self.mirror], check=True, env=self.env.vars)
        _write(os.path.join(self.mirror, "README"), "not published yet\n")
        self.env.git(self.mirror, "commit", "-q", "-am", "unpublished")
        self.unpublished = self.rev(self.mirror)
        self.cache = os.path.join(self.scratch, "verdandi.git")

    def rev(self, repo):
        return subprocess.run(["git", "-C", repo, "rev-parse", "HEAD"], check=True, capture_output=True,
                              text=True, env=self.env.vars).stdout.strip()

    def fetch(self, url, rev):
        return _bash(f'verdandi_fetch_public "{self.cache}" "{url}" "{rev}"', self.env.vars, self.scratch).returncode

    def test_a_published_revision_passes_from_an_empty_cache(self):
        self.assertEqual(self.fetch("file://" + self.public, self.published), 0)

    def test_a_revision_only_a_mirror_had_fails_although_the_cache_still_holds_it(self):
        self.assertEqual(self.fetch("file://" + self.mirror, self.unpublished), 0)
        self.assertEqual(self.fetch("file://" + self.public, self.unpublished), 1)
        # The old check -- the object is present -- would have passed here.
        present = subprocess.run(["git", "-C", self.cache, "cat-file", "-e", self.unpublished + "^{commit}"],
                                 env=self.env.vars)
        self.assertEqual(present.returncode, 0)
        self.assertEqual(self.fetch("file://" + self.public, self.published), 0)

    def test_an_unknown_revision_fails_and_an_unreachable_url_is_a_fetch_failure(self):
        self.assertEqual(self.fetch("file://" + self.public, "f" * 40), 1)
        self.assertEqual(self.fetch("file://" + os.path.join(self.scratch, "nowhere"), self.published), 2)


class ReleaseNotes(unittest.TestCase):
    """fill_notes, over the real release-notes.md.in, with the real publish/scan.sh. Task 1
    (installer-claude-10): the Verify section must describe what THIS build's shipped install.sh
    can actually check, which takes two facts, never one: whether packaging/release-signers (the
    block install.sh embeds) holds a key, and whether this run signed (RS_SIGN_KEY non-empty).

    - no key embedded, signed or not: 'carries no release key' (rc.1's shape: a throwaway key);
    - a key embedded and this run signed: 'built into it' and the manual ssh-keygen recipe;
    - a key embedded and this run unsigned (only --rehearsal gets here): no signature, and this
      build's own installer refuses it -- neither of the other two texts is true of it.

    Each of the three is pinned by a test that renders the *other* fact both ways, so a condition
    reading only one of them fails here (fix rounds 1 and 2)."""

    def render(self, signers_text=None, **env_overrides):
        scan = os.path.join(_PACKAGING, "..", "publish", "scan.sh")
        if not os.path.isfile(scan):
            self.skipTest("publish/scan.sh is not in this checkout")
        scratch = _scratch_dir()
        self.addCleanup(shutil.rmtree, scratch, ignore_errors=True)
        source = os.path.join(scratch, "clone")
        _write(os.path.join(source, "Cargo.lock"), 'version = 4\n\n[[package]]\nname = "nvim-rs"\nversion = "0.9.2"\n')
        os.makedirs(os.path.join(source, "packaging"))
        shutil.copy2(os.path.join(_PACKAGING, "release-notes.md.in"), os.path.join(source, "packaging"))
        if signers_text is not None:
            _write(os.path.join(source, "packaging", "release-signers"), signers_text)
        logs = os.path.join(scratch, "logs")
        os.makedirs(logs)
        env = dict(os.environ, RS_SOURCE=source, RS_LOGS=logs, RS_VERSION="1.0.0-rc.1", RS_COMMIT="1" * 40,
                   RS_VERDANDI_REV=_VERDANDI_REV, RS_SIGN_KEY="", RS_NO_LEAK_SCAN="0", RS_SCAN=scan)
        env.update(env_overrides)
        proc = _bash("fill_notes", env, scratch)
        self.assertEqual(proc.returncode, 0, proc.stderr)
        with open(os.path.join(logs, "release-notes.md"), encoding="utf-8") as f:
            return f.read()

    def test_the_notes_install_this_release_name_the_floors_and_pass_the_scan(self):
        notes = self.render()
        self.assertIn("/releases/download/v1.0.0-rc.1/install.sh | sh -s -- --version 1.0.0-rc.1\n", notes)
        self.assertIn("WebKitGTK 2.40", notes)
        self.assertIn("nvim-rs 0.9.2", notes)
        self.assertIn("nvim` >= 0.10", notes)
        self.assertIn("--nvim-only", notes)
        self.assertNotIn("@", notes.replace("release@eitri", ""))

    def test_no_release_signers_file_at_all_renders_the_no_key_variant(self):
        # No packaging/release-signers in the fixture (the default rc.1 shape before Task 1's own
        # drift check would even let a real build reach this far): treated the same as an empty one.
        notes = self.render()
        self.assertIn("carries no release key", notes)
        self.assertNotIn("built into it", notes)

    def test_an_empty_release_signers_file_never_claims_a_built_in_key(self):
        # An --unsigned rc (RS_SIGN_KEY empty) with no key embedded: nothing signed, nothing to
        # verify with. The signed half of installer-claude-10 is the next test.
        notes = self.render(signers_text="# no key line yet\n")
        self.assertIn("carries no release key", notes)
        self.assertNotIn("built into it", notes)
        self.assertNotIn("ssh-keygen -Y verify -f release-signers", notes)

    def test_a_throwaway_signed_rc_with_no_embedded_key_never_claims_a_built_in_key(self):
        # installer-claude-10's own reproduction, exactly rc.1's shape: SHA256SUMS.sig exists
        # (--sign with a throwaway key, checked against --release-signers), but the block
        # install.sh embeds holds no key. rc.1's notes said "install.sh does both itself, with the
        # release key built into it" -- a condition reading only RS_SIGN_KEY renders that again,
        # and this is the test that says so (review of fix round 1: no other test did).
        for signers_text in ("# no key line yet\n", None):
            with self.subTest(release_signers="comment only" if signers_text else "absent"):
                notes = self.render(signers_text=signers_text, RS_SIGN_KEY="/a/throwaway/key")
                self.assertIn("carries no release key", notes)
                self.assertIn("throwaway key", notes)
                self.assertNotIn("built into it", notes)
                self.assertNotIn("ssh-keygen -Y verify -f release-signers", notes)

    def test_a_key_in_release_signers_renders_the_manual_verify_recipe(self):
        # A real signed build: release-signers holds a key AND this run actually signed against it
        # (RS_SIGN_KEY non-empty) -- preflight's refusals (EmbeddedKeyBindsRcSigning, above) and
        # verify_release_signature (VerifyReleaseSignature, above) are what guarantee the signing
        # key is the embedded one by the time fill_notes runs for real; fill_notes
        # itself does not re-open RS_SIGN_KEY, so any non-empty path stands in for it here.
        notes = self.render(signers_text='release@eitri namespaces="eitri-release" ssh-ed25519 AAAAkeydata\n',
                             RS_SIGN_KEY="/does/not/matter/to/fill_notes")
        self.assertIn("built into it", notes)
        self.assertIn("ssh-keygen -Y verify -f release-signers -I release@eitri -n eitri-release "
                       "-s SHA256SUMS.sig < SHA256SUMS", notes)
        self.assertNotIn("carries no release key", notes)

    def test_a_key_in_release_signers_but_this_run_unsigned_says_its_own_installer_refuses_it(self):
        # docs-codex-6 / installer-claude-10 (fix round 1's reproduction): release-signers already
        # holds a key, but THIS run is --unsigned (RS_SIGN_KEY empty) -- which preflight allows
        # only under --rehearsal (EmbeddedKeyBindsRcSigning). It must not claim "install.sh
        # verifies it itself, with the release key built into it" for a build with no
        # SHA256SUMS.sig. Nor (fix round 2) "carries no release key ... checks SHA256SUMS only":
        # this build's install.sh embeds the key and refuses a release whose signature is missing.
        notes = self.render(signers_text='release@eitri namespaces="eitri-release" ssh-ed25519 AAAAkeydata\n',
                             RS_SIGN_KEY="")
        self.assertIn("has no `SHA256SUMS.sig`", notes)
        self.assertIn("refuses", notes)
        self.assertNotIn("carries no release key", notes)
        self.assertNotIn("SHA256SUMS only", notes.replace("\n", " "))
        self.assertNotIn("built into it", notes)
        self.assertNotIn("ssh-keygen -Y verify -f release-signers", notes)


class ReleaseNotesBilingual(unittest.TestCase):
    """Task 5 (v1-dist, lane D): one page, the English notes, a `---` rule, then the Chinese notes
    under `## 简体中文` (the owner's language decision). Reuses ReleaseNotes.render() (called
    unbound, `ReleaseNotes.render(self, ...)`, so its addCleanup/skipTest register against *this*
    instance rather than a throwaway one) against the exact same three release-signers/RS_SIGN_KEY
    combinations ReleaseNotes already covers for the English half."""

    render = ReleaseNotes.render

    def test_the_page_has_both_halves_in_order_and_no_leftover_placeholder(self):
        notes = self.render()
        self.assertIn("Eitri 1.0.0-rc.1 -- Your Neovim, with Claude Code beside it", notes)
        self.assertIn("\n---\n\n## 简体中文\n", notes)
        self.assertLess(notes.index("## 简体中文"), notes.index("Eitri 1.0.0-rc.1 -- 你的 Neovim，与 Claude Code 并肩"))
        self.assertNotIn("@", notes.replace("release@eitri", ""))

    def test_the_chinese_half_names_the_same_release_facts_as_the_english_one(self):
        notes = self.render()
        self.assertIn("WebKitGTK 2.40", notes)
        self.assertIn("nvim-rs 0.9.2", notes)
        self.assertIn("nvim` >= 0.10", notes)
        self.assertIn("--nvim-only", notes)  # command names stay in English/code form in the Chinese half too
        zh = notes.split("## 简体中文", 1)[1]
        self.assertIn("nvim-rs 0.9.2", zh)
        self.assertIn(">= 0.10", zh)
        self.assertIn("--nvim-only", zh)
        self.assertIn("XDG_DATA_HOME/eitri/nvim", zh)

    def test_the_chinese_verify_section_renders_the_no_key_variant(self):
        notes = self.render()
        zh = notes.split("## 简体中文", 1)[1]
        self.assertIn("没有发布密钥", zh)
        self.assertNotIn("内置的发布密钥", zh)

    def test_the_chinese_verify_section_renders_the_keyed_and_signed_variant(self):
        notes = self.render(signers_text='release@eitri namespaces="eitri-release" ssh-ed25519 AAAAkeydata\n',
                             RS_SIGN_KEY="/does/not/matter/to/fill_notes")
        zh = notes.split("## 简体中文", 1)[1]
        self.assertIn("已签名", zh)
        self.assertIn("内置的发布密钥", zh)
        self.assertIn("ssh-keygen -Y verify -f release-signers -I release@eitri -n eitri-release "
                       "-s SHA256SUMS.sig < SHA256SUMS", zh)
        self.assertNotIn("没有发布密钥", zh)

    def test_the_chinese_verify_section_renders_the_keyed_but_unsigned_variant(self):
        notes = self.render(signers_text='release@eitri namespaces="eitri-release" ssh-ed25519 AAAAkeydata\n',
                             RS_SIGN_KEY="")
        zh = notes.split("## 简体中文", 1)[1]
        self.assertIn("没有 `SHA256SUMS.sig`", zh)
        self.assertIn("拒绝", zh)
        self.assertNotIn("没有发布密钥", zh)
        self.assertNotIn("内置的发布密钥。", zh)


class WriteRelease(unittest.TestCase):
    """write_release, the function phase B runs, against release_check's own validator."""

    VALUES = {
        "RS_VERSION": "1.0.0-rc.1",
        "RS_COMMIT": "1" * 40,
        "RS_FORK_COMMIT": "2" * 40,
        "RS_VERDANDI_REV": _VERDANDI_REV,
        "RS_VERDANDI_SOURCE_SHA256": "3" * 64,
        "RS_NODE_VERSION": "v22.23.2",
        "RS_NODE_SHA256_X64": "4" * 64,
        "RS_NODE_SHA256_ARM64": "5" * 64,
        "RS_SKIA_ARCHIVE": "skia-binaries-abc-x86_64-unknown-linux-gnu.tar.gz",
        "RS_SKIA_SHA256": "6" * 64,
        "RS_BUILD_IMAGE": "sha256:" + "7" * 64,
    }

    def setUp(self):
        self.scratch = _scratch_dir()
        self.addCleanup(shutil.rmtree, self.scratch, ignore_errors=True)

    def write(self, **overrides):
        values = dict(self.VALUES, **overrides)
        env = dict(os.environ, **values)
        out = os.path.join(self.scratch, "RELEASE")
        proc = _bash(f'write_release "{out}"', env, self.scratch)
        self.assertEqual(proc.returncode, 0, proc.stderr)
        with open(out, encoding="utf-8") as f:
            return f.read()

    def test_a_release_carries_every_field_and_no_rehearsal_mark(self):
        text = self.write(RS_NVIM_VERSION="0.11.4", RS_NVIM_SHA256="8" * 64, RS_REHEARSAL="0")
        rc.validate_release_text(text)
        self.assertNotIn("REHEARSAL", text)
        self.assertIn("VERDANDI_SOURCE=verdandi-0123456-source.tar.gz\n", text)

    def test_a_rehearsal_without_the_nvim_pin_is_marked_and_leaves_the_nvim_fields_out(self):
        text = self.write(RS_NVIM_VERSION="", RS_NVIM_SHA256="", RS_REHEARSAL="1")
        self.assertIn("REHEARSAL=1\n", text)
        self.assertNotIn("NVIM_", text)
        rc.validate_release_text(text, rehearsal=True)
        with self.assertRaises(rc.ReleaseCheckError):
            rc.validate_release_text(text)


if __name__ == "__main__":
    unittest.main()
