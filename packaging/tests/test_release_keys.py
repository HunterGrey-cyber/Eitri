"""The three sidecar keys in a RELEASE: SIDECAR_VERSION, SIDECAR_PROTOCOL_MAJOR, SIDECAR_REQUIRED_CAPABILITIES.

    python3 -m pytest packaging/tests/test_release_keys.py -q

Three writers copy them from packaging/sidecar-contract.env: release.sh's write_release, install.sh's
synthesize_release_from_checkout and the eitri-git PKGBUILD's block. The values below are never the real
file's, so a writer that hard-codes or reads another file fails. An old tree has no contract file; it
writes none of the keys and must still install, because install.sh --version reads older RELEASE files
the same way. The tag gate and release_check.py's field list have their tests in test_release_sh.py and
test_release_layout.py.

No container, no network. install.sh runs its work at the end of the file, so the functions under test
are cut out of it by name and run in sh with the helpers they call stubbed. Scratch is under ~/.cache.
"""

import importlib.util
import os
import re
import shutil
import subprocess
import tempfile
import unittest

_HERE = os.path.dirname(os.path.abspath(__file__))
_PACKAGING = os.path.dirname(_HERE)
_RELEASE_SH = os.path.join(_PACKAGING, "release.sh")
_INSTALL_SH = os.path.join(_PACKAGING, "install.sh")
_PINS = os.path.join(_PACKAGING, "pins.env")
_PKGBUILD = os.path.join(_PACKAGING, "aur", "eitri-git", "PKGBUILD")
_REAL_CONTRACT = os.path.join(_PACKAGING, "sidecar-contract.env")
_spec = importlib.util.spec_from_file_location("release_check", os.path.join(_PACKAGING, "release_check.py"))
rc = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(rc)

_SCRATCH_ROOT = os.path.expanduser("~/.cache/eitri-release-keys-tests")
KEYS = ("SIDECAR_VERSION", "SIDECAR_PROTOCOL_MAJOR", "SIDECAR_REQUIRED_CAPABILITIES")
CONTRACT = {
    "SIDECAR_VERSION": "7.1.4",
    "SIDECAR_PROTOCOL_MAJOR": "8",
    "SIDECAR_REQUIRED_CAPABILITIES": "alpha_cap beta_cap gamma_cap",
}
CONTRACT_TEXT = "# a comment the reader skips\n" + "".join(f"{k}={v}\n" for k, v in CONTRACT.items())
_VERDANDI_REV = "0123456789abcdef0123456789abcdef01234567"


def _write(path, text):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w", encoding="utf-8") as f:
        f.write(text)


def _function_text(path, name):
    """One shell function's source, from `name() {` to the closing brace in column 0."""
    with open(path, encoding="utf-8") as f:
        text = f.read()
    m = re.search(rf"^{re.escape(name)}\(\) \{{\n.*?\n\}}\n", text, re.S | re.M)
    assert m, f"{path} has no function {name}"
    return m.group(0)


def _release_keys(text):
    return {k: v for k, _, v in (line.partition("=") for line in text.splitlines()) if k in KEYS}


class _Scratch(unittest.TestCase):
    def setUp(self):
        os.makedirs(_SCRATCH_ROOT, exist_ok=True)
        self.scratch = tempfile.mkdtemp(dir=_SCRATCH_ROOT)
        self.addCleanup(shutil.rmtree, self.scratch, ignore_errors=True)


class WriteRelease(_Scratch):
    """release.sh's write_release as phase B runs it: cwd is the source copy, read_pins has run."""

    VALUES = {
        "RS_VERSION": "1.0.0-rc.1",
        "RS_COMMIT": "1" * 40,
        "RS_FORK_COMMIT": "2" * 40,
        "RS_VERDANDI_REV": _VERDANDI_REV,
        "RS_VERDANDI_SOURCE_SHA256": "3" * 64,
        "RS_GRPC_TOOLS_VERSION": "1.13.1",
        "RS_GRPC_TOOLS_SHA256_X64": "8" * 64,
        "RS_GRPC_TOOLS_SHA256_ARM64": "9" * 64,
        "RS_GTK_FLOOR": "4.14",
        "RS_BUILD_IMAGE": "sha256:" + "7" * 64,
        "RS_REHEARSAL": "0",
    }

    def write(self, contract_text):
        tree = os.path.join(self.scratch, "src")
        os.makedirs(os.path.join(tree, "packaging"), exist_ok=True)
        shutil.copy2(_PINS, os.path.join(tree, "packaging", "pins.env"))
        if contract_text is not None:
            _write(os.path.join(tree, "packaging", "sidecar-contract.env"), contract_text)
        out = os.path.join(self.scratch, "RELEASE")
        proc = subprocess.run(
            ["bash", "-c", f'set -euo pipefail; source "{_RELEASE_SH}"; read_pins packaging/pins.env; write_release "{out}"'],
            capture_output=True, text=True, cwd=tree, env=dict(os.environ, **self.VALUES), timeout=60)
        self.assertEqual(proc.returncode, 0, proc.stderr)
        with open(out, encoding="utf-8") as f:
            return f.read()

    def test_the_three_keys_come_from_the_contract_file(self):
        text = self.write(CONTRACT_TEXT)
        self.assertEqual(_release_keys(text), CONTRACT)
        self.assertEqual(sum(1 for line in text.splitlines() if line.split("=")[0] in KEYS), 3, text)

    def test_the_repository_contract_file_is_what_a_release_carries(self):
        with open(_REAL_CONTRACT, encoding="utf-8") as f:
            real = f.read()
        text = self.write(real)
        want = {k: v for k, _, v in (line.partition("=") for line in real.splitlines()) if k in KEYS}
        self.assertEqual(set(want), set(KEYS))
        self.assertEqual(_release_keys(text), want)
        rc.validate_release_text(text)  # the whole RELEASE passes release_check, the keys included

    def test_a_tree_with_no_contract_file_writes_none(self):
        text = self.write(None)
        self.assertEqual(_release_keys(text), {})
        self.assertIn("EITRI_VERSION=1.0.0-rc.1\n", text)


class SynthesizeReleaseFromCheckout(_Scratch):
    """install.sh's synthesize_release_from_checkout over a real git checkout, with the helpers that
    read Cargo files and fetch Verdandi stubbed. kv_get, kv_value, first7 and parse_release are the
    installer's own, so a RELEASE the function writes is read back the way an install reads it."""

    FUNCTIONS = ("kv_get", "kv_value", "first7", "parse_release", "parse_release_text",
                 "synthesize_release_from_checkout")

    def checkout(self, contract_text):
        tree = os.path.join(self.scratch, "checkout")
        _write(os.path.join(tree, "packaging", "pins.env"), open(_PINS, encoding="utf-8").read())
        if contract_text is not None:
            _write(os.path.join(tree, "packaging", "sidecar-contract.env"), contract_text)
        env = dict(os.environ, GIT_CONFIG_GLOBAL="/dev/null", GIT_CONFIG_NOSYSTEM="1")
        for argv in (["init", "-q"], ["add", "-A"],
                     ["-c", "user.name=t", "-c", "user.email=t@example.invalid", "-c", "commit.gpgsign=false",
                      "commit", "-q", "-m", "fixture"]):
            subprocess.run(["git", "-C", tree, *argv], check=True, capture_output=True, env=env)
        return tree

    def run_function(self, shell, tree):
        body = "\n".join(_function_text(_INSTALL_SH, name) for name in self.FUNCTIONS)
        script = f"""set -eu
NV_NL='
'
NV_VERSION_ERE='[0-9]+\\.[0-9]+\\.[0-9]+(-rc\\.[0-9]+)?'
NV_ISSUES=https://example.invalid/issues
NV_CACHE_NV="$1/cache"
OPT_VERDANDI_CHECKOUT=
die() {{ echo "die: $*" >&2; exit 1; }}
checkout_version() {{ CV_VERSION=1.0.0; }}
checkout_fork_rev() {{ CFR_REV={'b' * 40}; }}
agent_toml_verdandi() {{ AT_GIT_URL=https://github.com/HunterGrey-cyber/verdandi.git; AT_REV={_VERDANDI_REV}; }}
resolve_verdandi_source() {{ NV_VERDANDI_SOURCE_TARBALL_OVERRIDE=/dev/null; REL_VERDANDI_SOURCE_SHA256={'c' * 64}; }}
{body}
synthesize_release_from_checkout "$2"
printf 'PARSED version=[%s] major=[%s] caps=[%s] release=%s\\n' "$REL_SIDECAR_VERSION" \\
	"$REL_SIDECAR_PROTOCOL_MAJOR" "$REL_SIDECAR_REQUIRED_CAPABILITIES" "$NV_RELEASE_PATH"
"""
        return subprocess.run([shell, "-c", script, "sh", self.scratch, tree], capture_output=True, text=True,
                              timeout=60)

    def shells(self):
        return [s for s in ("dash", "bash") if shutil.which(s)]

    def test_the_three_keys_come_from_the_contract_file_and_read_back(self):
        tree = self.checkout(CONTRACT_TEXT)
        for shell in self.shells():
            with self.subTest(shell=shell):
                proc = self.run_function(shell, tree)
                self.assertEqual(proc.returncode, 0, proc.stderr)
                self.assertIn(f"PARSED version=[{CONTRACT['SIDECAR_VERSION']}] major=[{CONTRACT['SIDECAR_PROTOCOL_MAJOR']}] "
                              f"caps=[{CONTRACT['SIDECAR_REQUIRED_CAPABILITIES']}]", proc.stdout)
                path = proc.stdout.split("release=")[1].strip()
                with open(path, encoding="utf-8") as f:
                    text = f.read()
                self.assertEqual(_release_keys(text), CONTRACT)
                self.assertIn("EITRI_BUILT_FROM_CHECKOUT=1\n", text)

    def test_an_old_checkout_with_no_contract_file_writes_none_and_still_reads_back(self):
        tree = self.checkout(None)
        for shell in self.shells():
            with self.subTest(shell=shell):
                proc = self.run_function(shell, tree)
                self.assertEqual(proc.returncode, 0, proc.stderr)
                self.assertIn("PARSED version=[] major=[] caps=[]", proc.stdout)
                path = proc.stdout.split("release=")[1].strip()
                with open(path, encoding="utf-8") as f:
                    text = f.read()
                self.assertEqual(_release_keys(text), {})
                self.assertIn("EITRI_VERSION=1.0.0\n", text)


class PkgbuildKeyList(unittest.TestCase):
    """The eitri-git PKGBUILD builds its RELEASE inline; its key list is held to install.sh's."""

    @staticmethod
    def printed_keys(text):
        return re.findall(r"printf '([A-Za-z0-9_]+)=%s\\n'", text)

    def pkgbuild_block(self):
        with open(_PKGBUILD, encoding="utf-8") as f:
            text = f.read()
        start = text.index("printf 'EITRI_VERSION=%s")
        end = text.index("release-for-sidecar.env", start)
        return text[start:end]

    def test_the_pkgbuild_writes_the_same_keys_as_the_installer(self):
        mine = self.printed_keys(self.pkgbuild_block())
        theirs = self.printed_keys(_function_text(_INSTALL_SH, "synthesize_release_from_checkout"))
        # The installer marks a checkout build; a package build is not one.
        theirs = [k for k in theirs if k != "EITRI_BUILT_FROM_CHECKOUT"]
        self.assertEqual(sorted(mine), sorted(theirs))
        for key in KEYS:
            self.assertIn(key, mine)

    def test_the_pkgbuild_takes_the_values_from_the_contract_file(self):
        with open(_PKGBUILD, encoding="utf-8") as f:
            text = f.read()
        self.assertIn("sidecar-contract.env", text)
        block = self.pkgbuild_block()
        for key in KEYS:
            self.assertRegex(block, rf"printf '{key}=%s\\n' \"\$\{{?\w+", f"{key} is not written from a variable")

    def run_block(self, scratch, contract_text):
        """Run the PKGBUILD's RELEASE-building lines in bash, in a source copy with the given contract file."""
        with open(_PKGBUILD, encoding="utf-8") as f:
            text = f.read()
        start = text.index("_pins=packaging/pins.env")
        end_marker = '} >"$srcdir/release-for-sidecar.env"'
        end = text.index(end_marker, start) + len(end_marker)
        tree = os.path.join(scratch, "src")
        os.makedirs(os.path.join(tree, "packaging"), exist_ok=True)
        shutil.copy2(_PINS, os.path.join(tree, "packaging", "pins.env"))
        if contract_text is not None:
            _write(os.path.join(tree, "packaging", "sidecar-contract.env"), contract_text)
        preamble = (
            "_eitri_version=1.0.0-rc.1; _eitri_commit=" + "1" * 40 + "; _fork_commit=" + "2" * 40 + "; "
            f"_verdandi_rev={_VERDANDI_REV}; _verdandi_rev7=0123456; _verdandi_sha=" + "3" * 64 + "; "
            f'srcdir="{scratch}"\n'
        )
        proc = subprocess.run(["bash", "-c", "set -euo pipefail\n" + preamble + text[start:end]],
                              capture_output=True, text=True, cwd=tree, timeout=60)
        self.assertEqual(proc.returncode, 0, proc.stderr)
        with open(os.path.join(scratch, "release-for-sidecar.env"), encoding="utf-8") as f:
            return f.read()

    def test_the_pkgbuild_writes_the_contract_files_values(self):
        os.makedirs(_SCRATCH_ROOT, exist_ok=True)
        scratch = tempfile.mkdtemp(dir=_SCRATCH_ROOT)
        self.addCleanup(shutil.rmtree, scratch, ignore_errors=True)
        out = self.run_block(scratch, CONTRACT_TEXT)
        self.assertEqual(_release_keys(out), CONTRACT)
        self.assertIn("EITRI_VERSION=1.0.0-rc.1\n", out)

    def test_the_pkgbuild_writes_none_of_the_keys_without_a_contract_file(self):
        os.makedirs(_SCRATCH_ROOT, exist_ok=True)
        scratch = tempfile.mkdtemp(dir=_SCRATCH_ROOT)
        self.addCleanup(shutil.rmtree, scratch, ignore_errors=True)
        out = self.run_block(scratch, None)
        self.assertEqual(_release_keys(out), {})
        self.assertIn("EITRI_VERSION=1.0.0-rc.1\n", out)


if __name__ == "__main__":
    unittest.main()
