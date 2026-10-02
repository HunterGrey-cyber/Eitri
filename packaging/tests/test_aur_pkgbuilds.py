"""The two AUR PKGBUILDs' package() functions, run for real over a stand-in source tree, and what they
install of the desktop entry and the icon (the app icon, 2026-10-01).

    python3 -m pytest packaging/tests/test_aur_pkgbuilds.py -q

No makepkg and no network: each PKGBUILD is sourced in bash (it only defines variables and functions)
and its package() is called with `srcdir` and `pkgdir` pointing at scratch trees built from this
repository's own committed desktop entry and icons. The binaries and the sidecar are empty stand-ins.
eitri-bin installs from the release tarball's layout (share/applications, share/icons), eitri-git from
the source tree's packaging/; both must end with the entry named by the application id, every icon
under /usr/share/icons/hicolor, and no eitri.desktop. Scratch lives under ~/.cache, never /tmp.

build() is run the same way, with stand-ins for the slow commands (cargo, and the eitri-setup/install.sh
that builds the sidecar) and a makepkg-style `msg2`: it must say ONE line, after anything that came before
it (cargo prints its own progress) and before the sidecar build, and nothing else of its own (the owner,
2026-10-01: print only the genuinely slow step, nothing for the quick ones).
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
PANEL_DESKTOP = "cn.huntergrey.eitri.Panel.desktop"
PLUGIN_ROOT = os.path.join(_REPO, "nvim", "eitri.nvim")
# What makepkg's msg2 prints for the one line (the test's own stand-in msg2 prints "  -> text").
SLOW_LINE = "  -> Installing the Claude Agent SDK and building the sidecar (1-3 min)"


def _icon_files():
    root = os.path.join(_PACKAGING, "icons")
    return sorted(
        os.path.relpath(os.path.join(d, f), root)
        for d, _dirs, files in os.walk(os.path.join(root, "hicolor"))
        for f in files
    )


def _plugin_files():
    return sorted(
        os.path.relpath(os.path.join(d, f), PLUGIN_ROOT) for d, _dirs, files in os.walk(PLUGIN_ROOT) for f in files
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

    def build(self, extra_path=""):
        """Run the PKGBUILD's build() with a makepkg-style msg2; return the CompletedProcess."""
        script = (
            f'set -euo pipefail; source "{self.pkgbuild}"; srcdir="{self.srcdir}"; pkgdir="{self.pkgdir}"; '
            'msg2() { printf "  -> %s\\n" "$1"; }; build'
        )
        path = f"{extra_path}:{os.environ['PATH']}" if extra_path else os.environ["PATH"]
        return subprocess.run(
            ["bash", "-c", script], capture_output=True, text=True, timeout=60, env=dict(os.environ, PATH=path)
        )

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
        # The panel's hidden entry, and the nvim plugin at the path the packages use.
        self.assertIn(f"usr/share/applications/{PANEL_DESKTOP}", got)
        self.assertEqual(
            _read(os.path.join(self.pkgdir, "usr/share/applications", PANEL_DESKTOP)),
            _read(os.path.join(_PACKAGING, PANEL_DESKTOP)),
        )
        self.assertEqual(len(_plugin_files()), 3)
        self.assertEqual(
            [p for p in got if p.startswith("usr/share/eitri/")],
            [f"usr/share/eitri/nvim/eitri.nvim/{rel}" for rel in _plugin_files()],
        )
        for rel in _plugin_files():
            path = os.path.join(self.pkgdir, "usr/share/eitri/nvim/eitri.nvim", rel)
            self.assertEqual(_read(path), _read(os.path.join(PLUGIN_ROOT, rel)), rel)
            self.assertEqual(os.stat(path).st_mode & 0o777, 0o644, rel)
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
        shutil.copy(os.path.join(_PACKAGING, PANEL_DESKTOP), os.path.join(top, "share", "applications", PANEL_DESKTOP))
        shutil.copytree(PLUGIN_ROOT, os.path.join(top, "share", "eitri", "eitri.nvim"))
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

    def test_build_says_one_line_before_the_sidecar_build_and_nothing_else(self):
        setup = os.path.join(self.srcdir, "eitri-0.2.0-x86_64-linux", "lib", "eitri", "eitri-setup")
        _touch(setup, b'echo "SETUP RAN $*"\n', mode=0o755)
        proc = self.build()
        self.assertEqual(proc.returncode, 0, proc.stderr)
        lines = proc.stdout.splitlines()
        self.assertEqual(len(lines), 2, proc.stdout)
        self.assertEqual(lines[0], SLOW_LINE)
        self.assertTrue(lines[1].startswith("SETUP RAN --build-sidecar-into "), lines[1])
        self.assertEqual(proc.stderr, "")

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
        shutil.copy(os.path.join(_PACKAGING, PANEL_DESKTOP), os.path.join(tree, "packaging", PANEL_DESKTOP))
        shutil.copytree(PLUGIN_ROOT, os.path.join(tree, "nvim", "eitri.nvim"))
        shutil.copytree(os.path.join(_PACKAGING, "icons"), os.path.join(tree, "packaging", "icons"))
        _touch(os.path.join(tree, "LICENSE"), b"MIT\n")
        _touch(os.path.join(self.srcdir, "release-for-sidecar.env"))

    def test_it_installs_the_desktop_entry_and_every_icon_from_the_source_tree(self):
        proc = self.package()
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assert_desktop_and_icons()

    def test_build_says_one_line_for_the_sidecar_and_none_for_cargo(self):
        tree = os.path.join(self.srcdir, "eitri-git")
        shutil.copy(os.path.join(_PACKAGING, "pins.env"), os.path.join(tree, "packaging", "pins.env"))
        _touch(os.path.join(tree, "packaging", "install.sh"), b'echo "SETUP RAN $*"\n', mode=0o755)
        bindir = os.path.join(self.scratch, "bin")
        _touch(os.path.join(bindir, "cargo"), b'#!/bin/sh\necho "CARGO RAN $*"\n', mode=0o755)
        proc = self.build(extra_path=bindir)
        self.assertEqual(proc.returncode, 0, proc.stderr)
        lines = proc.stdout.splitlines()
        # cargo prints its own progress and gets no line from this PKGBUILD; the one added line comes
        # after it, immediately before the sidecar build.
        self.assertEqual(len(lines), 3, proc.stdout)
        self.assertTrue(lines[0].startswith("CARGO RAN build "), lines[0])
        self.assertEqual(lines[1], SLOW_LINE)
        self.assertTrue(lines[2].startswith("SETUP RAN --build-sidecar-into "), lines[2])
        self.assertEqual(proc.stderr, "")

    def test_it_ships_the_logos_notice_beside_the_mit_licence(self):
        proc = self.package()
        self.assertEqual(proc.returncode, 0, proc.stderr)
        licences = os.path.join(self.pkgdir, "usr", "share", "licenses", "eitri-git")
        self.assertEqual(_read(os.path.join(licences, "LICENSE-icon")), _read(os.path.join(_PACKAGING, "icons", "LICENSE")))
        self.assertIn(b"CC BY 4.0", _read(os.path.join(licences, "LICENSE-icon")))
        self.assertEqual(_read(os.path.join(licences, "LICENSE")), b"MIT\n")


if __name__ == "__main__":
    unittest.main()
