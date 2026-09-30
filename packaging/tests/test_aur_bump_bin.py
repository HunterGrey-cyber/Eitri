"""Tests for packaging/aur/bump-bin.sh: what it writes into a copy of eitri-bin's PKGBUILD from a
release directory's RELEASE and SHA256SUMS.

    python3 -m pytest packaging/tests/test_aur_bump_bin.py -q

No container and no network. Every run works on a scratch copy of the PKGBUILD (--pkgbuild-dir), never
the tracked one; `makepkg` is kept off PATH so the result does not depend on the host being Arch. Scratch
lives under ~/.cache, never /tmp.
"""

import os
import re
import shutil
import subprocess
import tempfile
import unittest

_HERE = os.path.dirname(os.path.abspath(__file__))
_AUR = os.path.join(os.path.dirname(_HERE), "aur")
_BUMP = os.path.join(_AUR, "bump-bin.sh")
_PKGBUILD = os.path.join(_AUR, "eitri-bin", "PKGBUILD")
_SCRATCH_ROOT = os.path.expanduser("~/.cache/eitri-bump-bin-tests")

_TARBALL_SHA = "11" * 32
_VERDANDI_SHA = "22" * 32
_NODE_SHA = "33" * 32


def _release(root, version="1.0.0", node_version="v24.1.0", node_sha=_NODE_SHA):
    rel = os.path.join(root, "release")
    os.makedirs(rel)
    with open(os.path.join(rel, "RELEASE"), "w", encoding="utf-8") as f:
        f.write(f"EITRI_VERSION={version}\n"
                "VERDANDI_SOURCE=verdandi-abcdef0-source.tar.gz\n"
                f"NODE_VERSION={node_version}\n"
                f"NODE_SHA256_linux_x64={node_sha}\n")
    with open(os.path.join(rel, "SHA256SUMS"), "w", encoding="utf-8") as f:
        f.write(f"{_TARBALL_SHA}  eitri-{version}-x86_64-linux.tar.gz\n"
                f"{_VERDANDI_SHA}  verdandi-abcdef0-source.tar.gz\n")
    return rel


class BumpBinTest(unittest.TestCase):
    def setUp(self):
        os.makedirs(_SCRATCH_ROOT, exist_ok=True)
        self.root = tempfile.mkdtemp(dir=_SCRATCH_ROOT)
        self.addCleanup(shutil.rmtree, self.root, True)
        self.pkgdir = os.path.join(self.root, "eitri-bin")
        os.makedirs(self.pkgdir)
        shutil.copy(_PKGBUILD, os.path.join(self.pkgdir, "PKGBUILD"))
        # A PATH without makepkg: bump-bin.sh then stops after writing the PKGBUILD, with a message.
        self.bin = os.path.join(self.root, "bin")
        os.makedirs(self.bin)
        for tool in ("bash", "sed", "awk", "grep", "python3", "dirname", "cat"):
            path = shutil.which(tool)
            self.assertIsNotNone(path, tool)
            os.symlink(path, os.path.join(self.bin, tool))

    def run_bump(self, rel, *extra):
        return subprocess.run(["bash", _BUMP, rel, "--pkgbuild-dir", self.pkgdir, *extra],
                              env={"PATH": self.bin, "LANG": "C.UTF-8"},
                              capture_output=True, text=True, check=False)

    def pkgbuild(self):
        with open(os.path.join(self.pkgdir, "PKGBUILD"), encoding="utf-8") as f:
            return f.read()

    def test_node_version_and_hash_come_from_release_together(self):
        r = self.run_bump(_release(self.root))
        self.assertEqual(r.returncode, 0, r.stderr)
        text = self.pkgbuild()
        self.assertRegex(text, r"(?m)^_nodever=v24\.1\.0$")
        # No second, hard-coded Node version left anywhere for the URL or build() to use.
        self.assertNotIn("v22.", text)
        self.assertIn('"node-${_nodever}-linux-x64.tar.xz::https://nodejs.org/dist/${_nodever}/'
                      'node-${_nodever}-linux-x64.tar.xz"', text)
        self.assertIn('--node "$srcdir/node-${_nodever}-linux-x64.tar.xz"', text)
        sums = re.search(r"sha256sums=\(([^)]*)\)", text).group(1)
        self.assertEqual(re.findall(r"'([0-9a-f]{64})'", sums), [_TARBALL_SHA, _VERDANDI_SHA, _NODE_SHA])
        self.assertRegex(text, r"(?m)^pkgver=1\.0\.0$")
        self.assertRegex(text, r"(?m)^_verdandi_source=verdandi-abcdef0-source\.tar\.gz$")

    def test_a_pkgbuild_without_nodever_is_refused(self):
        path = os.path.join(self.pkgdir, "PKGBUILD")
        with open(path, encoding="utf-8") as f:
            text = f.read()
        with open(path, "w", encoding="utf-8") as f:
            f.write(re.sub(r"(?m)^_nodever=.*\n", "", text))
        r = self.run_bump(_release(self.root))
        self.assertNotEqual(r.returncode, 0)
        self.assertIn("no _nodever= line", r.stderr)

    def test_a_malformed_node_version_is_refused_before_writing(self):
        before = self.pkgbuild()
        r = self.run_bump(_release(self.root, node_version="v24.1.0/evil"))
        self.assertNotEqual(r.returncode, 0)
        self.assertIn("NODE_VERSION is not a vX.Y.Z version", r.stderr)
        self.assertEqual(self.pkgbuild(), before)

    def test_a_prerelease_is_refused_without_allow_prerelease(self):
        before = self.pkgbuild()
        r = self.run_bump(_release(self.root, version="1.0.0-rc.1"))
        self.assertNotEqual(r.returncode, 0)
        self.assertIn("is a prerelease", r.stderr)
        self.assertEqual(self.pkgbuild(), before)


if __name__ == "__main__":
    unittest.main()
