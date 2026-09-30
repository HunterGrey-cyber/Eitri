"""Unit tests for check-abi-floor.py's pure parsing/floor logic (plan Task 2, spec §2.2 layer 2).

    python3 packaging/test_check_abi_floor.py

(`python3 -m unittest packaging/test_check_abi_floor.py` fails from the repository root the same
way packaging/test_collect_licenses.py's own docstring explains: `packaging` collides with the
installed PyPI `packaging` package. `python3 -m pytest packaging` is unaffected.)

Fixtures live in packaging/tests/fixtures/abi/: a fake `-sys` `src/lib.rs` (never built; stands in
for gtk4-sys/gdk4-sys/gsk4-sys/webkit6-sys/javascriptcore6-sys, which all share this exact
`#[cfg(feature = "vX_Y")]`-before-`pub fn` shape) and captured `nm -D --undefined-only`/`objdump -T`
text. These tests feed that text straight into the pure parsing functions -- no subprocess, no real
binary -- except the last two classes, which run the real script as a subprocess over a real binary:
`RealShellBinary` over this host's own `target/debug/shell`, if one has been built (skipped
otherwise), and `ScratchCompliantBinary` over a copy of `/usr/bin/true` placed at a scratch path
that stands in for one (v1-dist verdict #3's probe), so a compliant build is exercised on every
host regardless of what -- if anything -- has been built here.
"""

import importlib.util
import os
import shutil
import subprocess
import sys
import tempfile
import unittest

_HERE = os.path.dirname(os.path.abspath(__file__))
_REPO = os.path.dirname(_HERE)
_FIXTURES = os.path.join(_HERE, "tests", "fixtures", "abi")

_spec = importlib.util.spec_from_file_location("check_abi_floor", os.path.join(_HERE, "check-abi-floor.py"))
abi = importlib.util.module_from_spec(_spec)
# Registered before exec: check-abi-floor.py's dataclasses (with `from __future__ import
# annotations`) resolve their field types against `sys.modules[cls.__module__]` at class-creation
# time, which is empty/absent otherwise -- collect-licenses.py never hit this because it has no
# dataclass.
sys.modules[_spec.name] = abi
_spec.loader.exec_module(abi)


def _read(name: str) -> str:
    with open(os.path.join(_FIXTURES, name), encoding="utf-8") as f:
        return f.read()


class SymbolGates(unittest.TestCase):
    """`parse_symbol_gates` over the fake `-sys` fixture."""

    def setUp(self):
        self.gates = abi.parse_symbol_gates("fake-sys", _read("fake_sys_lib.rs"))

    def test_an_unguarded_symbol_is_not_in_the_map(self):
        self.assertNotIn("gtk_fake_baseline_fn", self.gates)

    def test_a_v4_14_gated_symbol_is_mapped(self):
        self.assertEqual(self.gates["gtk_fake_v4_14_fn"], abi.Gate("fake-sys", 4, 14))

    def test_a_v4_16_gated_symbol_is_mapped(self):
        self.assertEqual(self.gates["gtk_fake_v4_16_fn"], abi.Gate("fake-sys", 4, 16))

    def test_the_cfg_any_docsrs_shape_is_still_parsed(self):
        self.assertEqual(self.gates["gtk_fake_v4_18_fn"], abi.Gate("fake-sys", 4, 18))


class GtkFloorViolations(unittest.TestCase):
    """`gate_floor_violations` against the fake gate map, over real `nm -D --undefined-only`
    fixture output, at the v1 floor (4.14/2.40)."""

    def setUp(self):
        self.gates = abi.parse_symbol_gates("fake-sys", _read("fake_sys_lib.rs"))
        self.gtk_floor = (4, 14)
        self.webkit_floor = (2, 40)

    def test_pass_case_has_no_violations(self):
        undefined = abi.parse_nm_undefined(_read("nm_pass.txt"))
        violations = abi.gate_floor_violations(self.gates, undefined, self.gtk_floor, self.webkit_floor)
        self.assertEqual(violations, [])

    def test_a_v4_16_symbol_fails_at_the_4_14_floor(self):
        undefined = abi.parse_nm_undefined(_read("nm_v4_16_violation.txt"))
        violations = abi.gate_floor_violations(self.gates, undefined, self.gtk_floor, self.webkit_floor)
        self.assertEqual(len(violations), 1)
        self.assertIn("gtk_fake_v4_16_fn", violations[0])
        self.assertIn("v4_16", violations[0])


class GlibcFloorReport(unittest.TestCase):
    """`glibc_floor_report` over real `objdump -T` fixture output, at the v1 floor (2.39)."""

    def test_a_weak_glibc_2_44_symbol_passes_with_a_note(self):
        refs = abi.parse_objdump_glibc_refs(_read("objdump_weak_2_44.txt"))
        failures, notes = abi.glibc_floor_report(refs, (2, 39))
        self.assertEqual(failures, [])
        self.assertEqual(len(notes), 1)
        self.assertIn("fake_weak_glibc_2_44_fn@GLIBC_2.44", notes[0])

    def test_a_strong_glibc_2_40_symbol_fails(self):
        refs = abi.parse_objdump_glibc_refs(_read("objdump_strong_2_40.txt"))
        failures, notes = abi.glibc_floor_report(refs, (2, 39))
        self.assertEqual(notes, [])
        self.assertEqual(len(failures), 1)
        self.assertIn("fake_strong_glibc_2_40_fn@GLIBC_2.40", failures[0])


def _run_check_abi_floor(binary):
    return subprocess.run(
        [sys.executable, os.path.join(_HERE, "check-abi-floor.py"), str(binary), "--gtk", "4.14", "--webkit", "2.40",
         "--glibc", "2.39"],
        cwd=_REPO,
        capture_output=True,
        text=True,
        check=False,
    )


class RealShellBinary(unittest.TestCase):
    """Runs the real script over this host's real `target/debug/shell`, if one is built -- no
    fixture, no fake gate map. Skipped (not failed) when nothing has been built here, since a
    fresh checkout or a CI machine with no build must not fail `python3 -m pytest packaging`.

    The GTK/WebKit half is asserted unconditionally (spec §2.3: this binary is built at the v4_14
    floor, plan Task 2, on every host this test can run on). **The glibc half used to be asserted
    unconditionally too** -- `returncode == 1` and `FAIL glibc` in the output, hard-coding this
    Arch host's own glibc 2.44 link -- which fails on any compliant host (glibc <= 2.39, including
    Ubuntu 24.04, the declared build platform, and Debian 12; v1-dist verdict #3, reproduced with
    `/usr/bin/true` standing in for a compliant `target/debug/shell`). It is now computed instead,
    from this binary's own real `objdump -T` output, through the exact pair of pure functions
    check-abi-floor.py itself uses (`parse_objdump_glibc_refs`/`glibc_floor_report`) -- so this test
    agrees with the script under test by construction rather than by a second, independently
    maintained assumption about what this host's toolchain happens to link.
    """

    BINARY = os.path.join(_REPO, "target", "debug", "shell")
    GLIBC_FLOOR = (2, 39)

    @unittest.skipUnless(os.path.exists(BINARY), "target/debug/shell is not built on this machine")
    def test_gtk_webkit_pass_unconditionally_and_glibc_matches_this_binarys_own_refs(self):
        refs = abi.parse_objdump_glibc_refs(abi.run_objdump_dynsyms(self.BINARY))
        glibc_failures, _notes = abi.glibc_floor_report(refs, self.GLIBC_FLOOR)
        expect_glibc_fail = bool(glibc_failures)

        result = _run_check_abi_floor(self.BINARY)
        output = result.stdout + result.stderr

        # Unconditional: this binary is built at the v4_14/v2_40 floor (plan Task 2), on every host.
        self.assertNotIn("gtk4-sys", output)
        self.assertNotIn("gdk4-sys", output)
        self.assertNotIn("gsk4-sys", output)
        self.assertNotIn("webkit6-sys", output)

        # Conditional: only a host whose glibc link is itself above the floor -- this Arch host,
        # glibc 2.44 -- is expected to fail here; a compliant host (glibc <= 2.39) must pass.
        self.assertEqual(result.returncode, 1 if expect_glibc_fail else 0, output)
        if expect_glibc_fail:
            self.assertIn("FAIL glibc", output)
        else:
            self.assertNotIn("FAIL glibc", output)
            self.assertIn("OK (gtk<=v4_14, webkit<=v2_40, glibc<=2.39)", output)


class ScratchCompliantBinary(unittest.TestCase):
    """The probe from v1-dist verdict #3: a binary that *is* compliant (glibc <= 2.39, no
    GTK/WebKit symbols at all) must PASS this check, not be asserted to fail it -- the case the old
    hard-coded `RealShellBinary` assertion could not express because it only ever saw this Arch
    host's own non-compliant link. Runs the real script over a copy of `/usr/bin/true` -- max
    GLIBC_2.34 per the verdict's own `objdump` probe, well under the 2.39 floor, and no
    gtk_/gdk_/gsk_/webkit_/jsc_-prefixed symbol -- placed at a scratch path standing in for
    `target/debug/shell`, in a throwaway temporary directory (never inside the real workspace
    `target/`, and cleaned up whether or not the test passes), so this never depends on what -- if
    anything -- happens to be built on this machine.
    """

    def setUp(self):
        if not os.path.exists("/usr/bin/true"):
            self.skipTest("/usr/bin/true is not present on this host")
        self._scratch = tempfile.mkdtemp(prefix="eitri-abi-floor-compliant-stand-in-")
        self.addCleanup(shutil.rmtree, self._scratch, ignore_errors=True)
        self.stand_in = os.path.join(self._scratch, "shell")
        shutil.copyfile("/usr/bin/true", self.stand_in)
        shutil.copymode("/usr/bin/true", self.stand_in)

    def test_a_compliant_stand_in_binary_passes_rather_than_being_asserted_to_fail(self):
        result = _run_check_abi_floor(self.stand_in)
        output = result.stdout + result.stderr
        self.assertEqual(result.returncode, 0, output)
        self.assertNotIn("FAIL", output)
        self.assertIn("OK (gtk<=v4_14, webkit<=v2_40, glibc<=2.39)", output)


if __name__ == "__main__":
    unittest.main()
