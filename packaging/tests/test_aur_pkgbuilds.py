"""The two AUR PKGBUILDs' package() functions, run for real over a stand-in source tree, and what they
install of the desktop entry and the icon (the app icon, 2026-10-01).

    python3 -m pytest packaging/tests/test_aur_pkgbuilds.py -q

No makepkg and no network: each PKGBUILD is sourced in bash (it only defines variables and functions)
and its package() is called with `srcdir` and `pkgdir` pointing at scratch trees built from this
repository's own committed desktop entry and icons. The binaries and the sidecar are empty stand-ins.
eitri-bin installs from the release tarball's layout (share/applications, share/icons), eitri-git from
the source tree's packaging/; both must end with the entry named by the application id, every icon
under /usr/share/icons/hicolor, and no eitri.desktop. Scratch lives under ~/.cache, never /tmp.
"""

import os
import shutil
import subprocess
import tempfile
import unittest

_HERE = os.path.dirname(os.path.abspath(__file__))
_PACKAGING = os.path.dirname(_HERE)
_REPO = os.path.dirname(_PACKAGING)
_SCRATCH_ROOT = os.path.expanduser("~/.cache/eitri-aur-pkgbuild-tests")
DESKTOP = "cn.huntergrey.eitri.desktop"


def _icon_files():
    root = os.path.join(_PACKAGING, "icons")
    return sorted(
        os.path.relpath(os.path.join(d, f), root)
        for d, _dirs, files in os.walk(os.path.join(root, "hicolor"))
        for f in files
    )


def _touch(path, data=b"x\n", mode=0o644):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "wb") as f:
        f.write(data)
    os.chmod(path, mode)


def _read(path):
    with open(path, "rb") as f:
        return f.read()


class _PackageCase(unittest.TestCase):
    pkgbuild = ""
    pkgname = ""

    def setUp(self):
        os.makedirs(_SCRATCH_ROOT, exist_ok=True)
        self.scratch = tempfile.mkdtemp(dir=_SCRATCH_ROOT)
        self.addCleanup(shutil.rmtree, self.scratch, ignore_errors=True)
        self.srcdir = os.path.join(self.scratch, "src")
        self.pkgdir = os.path.join(self.scratch, "pkg")
        os.makedirs(self.srcdir)
        os.makedirs(self.pkgdir)
        _touch(os.path.join(self.srcdir, "sidecar", "verdandi-claude-sidecar"), mode=0o755)
        _touch(os.path.join(self.srcdir, "sidecar", "verdandi-claude-sidecar.rev"))

    def package(self, extra=""):
        """Run the PKGBUILD's package() with the scratch srcdir and pkgdir; return the CompletedProcess."""
        script = (
            f'set -euo pipefail; source "{self.pkgbuild}"; srcdir="{self.srcdir}"; pkgdir="{self.pkgdir}"; '
            f"{extra} package"
        )
        return subprocess.run(["bash", "-c", script], capture_output=True, text=True, timeout=60)

    def installed(self):
        return sorted(
            os.path.relpath(os.path.join(d, f), self.pkgdir) for d, _dirs, files in os.walk(self.pkgdir) for f in files
        )

    def assert_desktop_and_icons(self):
        got = self.installed()
        self.assertIn(f"usr/share/applications/{DESKTOP}", got)
        self.assertNotIn("usr/share/applications/eitri.desktop", got)
        self.assertEqual(
            [p for p in got if p.startswith("usr/share/icons/")],
            [f"usr/share/icons/{rel}" for rel in _icon_files()],
        )
        self.assertEqual(len(_icon_files()), 9)
        self.assertEqual(
            _read(os.path.join(self.pkgdir, "usr/share/applications", DESKTOP)),
            _read(os.path.join(_PACKAGING, DESKTOP)),
        )
        for rel in _icon_files():
            self.assertEqual(
                _read(os.path.join(self.pkgdir, "usr/share/icons", rel)), _read(os.path.join(_PACKAGING, "icons", rel)), rel
            )
            self.assertEqual(os.stat(os.path.join(self.pkgdir, "usr/share/icons", rel)).st_mode & 0o777, 0o644, rel)


class EitriBinPackage(_PackageCase):
    pkgbuild = os.path.join(_PACKAGING, "aur", "eitri-bin", "PKGBUILD")

    def setUp(self):
        super().setUp()
        top = os.path.join(self.srcdir, "eitri-0.2.0-x86_64-linux")
        for name in ("shell", "eitri-supervisor", "eitri-tmux-shim", "eitri-claude-handoff", "eitri-setup"):
            _touch(os.path.join(top, "lib", "eitri", name), mode=0o755)
        _touch(os.path.join(top, "lib", "eitri", "RELEASE"))
        _touch(os.path.join(top, "bin", "eitri"), mode=0o755)
        os.makedirs(os.path.join(top, "share", "applications"))
        shutil.copy(os.path.join(_PACKAGING, DESKTOP), os.path.join(top, "share", "applications", DESKTOP))
        # The release tarball also carries 0.2.0's own entry, for 0.2.0's installer alone: the package
        # must not install it (assert_desktop_and_icons checks eitri.desktop is absent).
        shutil.copy(os.path.join(_PACKAGING, "legacy", "eitri.desktop"), os.path.join(top, "share", "applications", "eitri.desktop"))
        shutil.copytree(os.path.join(_PACKAGING, "icons", "hicolor"), os.path.join(top, "share", "icons", "hicolor"))
        for name in ("LICENSE", "THIRD-PARTY-LICENSES", "SOURCE"):
            _touch(os.path.join(top, "share", "licenses", "eitri", name))

    def test_it_installs_the_desktop_entry_and_every_icon_from_the_tarball_layout(self):
        proc = self.package()
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assert_desktop_and_icons()

    def test_a_tarball_from_before_the_icon_does_not_build_a_package_with_the_old_entry(self):
        top = os.path.join(self.srcdir, "eitri-0.2.0-x86_64-linux")
        os.rename(os.path.join(top, "share", "applications", DESKTOP), os.path.join(top, "share", "applications", "eitri.desktop"))
        shutil.rmtree(os.path.join(top, "share", "icons"))
        proc = self.package()
        self.assertNotEqual(proc.returncode, 0, "package() must fail, not ship an entry with the old name")
        self.assertNotIn("usr/share/applications/eitri.desktop", self.installed())


class EitriGitPackage(_PackageCase):
    pkgbuild = os.path.join(_PACKAGING, "aur", "eitri-git", "PKGBUILD")

    def setUp(self):
        super().setUp()
        tree = os.path.join(self.srcdir, "eitri-git")
        for name in ("shell", "eitri-supervisor", "eitri-tmux-shim", "eitri-claude-handoff"):
            _touch(os.path.join(tree, "target", "release", name), mode=0o755)
        for name in ("install.sh", "eitri.launcher.sh"):
            _touch(os.path.join(tree, "packaging", name), mode=0o755)
        # The committed entry and icons, exactly as the repository holds them (and the notice).
        shutil.copy(os.path.join(_PACKAGING, DESKTOP), os.path.join(tree, "packaging", DESKTOP))
        shutil.copytree(os.path.join(_PACKAGING, "icons"), os.path.join(tree, "packaging", "icons"))
        _touch(os.path.join(tree, "LICENSE"), b"MIT\n")
        _touch(os.path.join(self.srcdir, "release-for-sidecar.env"))

    def test_it_installs_the_desktop_entry_and_every_icon_from_the_source_tree(self):
        proc = self.package()
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assert_desktop_and_icons()

    def test_it_ships_the_logos_notice_beside_the_mit_licence(self):
        proc = self.package()
        self.assertEqual(proc.returncode, 0, proc.stderr)
        licences = os.path.join(self.pkgdir, "usr", "share", "licenses", "eitri-git")
        self.assertEqual(_read(os.path.join(licences, "LICENSE-icon")), _read(os.path.join(_PACKAGING, "icons", "LICENSE")))
        self.assertIn(b"CC BY 4.0", _read(os.path.join(licences, "LICENSE-icon")))
        self.assertEqual(_read(os.path.join(licences, "LICENSE")), b"MIT\n")


if __name__ == "__main__":
    unittest.main()
