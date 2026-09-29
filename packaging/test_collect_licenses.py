"""Unit tests for collect-licenses.py's handling of workspace members, the two profiles
(--sidecar/--no-sidecar), --repo/--binaries-dir, and the SOURCE notice (spec sec 11, Task 7).

    python3 packaging/test_collect_licenses.py

(`python3 -m unittest packaging/test_collect_licenses.py` fails from the repository root:
`ModuleNotFoundError: No module named 'packaging.test_collect_licenses'`, because the `packaging`
directory name collides with the installed PyPI `packaging` package -- engine review 2026-09-23,
minor 6.)

The collector's full run needs release binaries and the sidecar (publish.sh); these need neither --
they run the script's functions on fixtures (a scratch dir under ~/.cache, never /tmp), or against
this workspace's already-resolved Cargo.lock (offline: no network, nothing built).
"""

import glob
import hashlib
import importlib.util
import io
import os
import shutil
import subprocess
import tarfile
import tempfile
import unittest
from unittest import mock

_HERE = os.path.dirname(os.path.abspath(__file__))
_spec = importlib.util.spec_from_file_location("collect_licenses", os.path.join(_HERE, "collect-licenses.py"))
cl = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(cl)

# Scratch for fixtures this test file creates on the fly. Never /tmp: on this project's own dev
# machines that is a small shared tmpfs, and it is not this test's call to assume otherwise elsewhere.
_SCRATCH_ROOT = os.path.expanduser("~/.cache/neovibe-collect-licenses-tests")


def _scratch_dir():
    os.makedirs(_SCRATCH_ROOT, exist_ok=True)
    return tempfile.mkdtemp(dir=_SCRATCH_ROOT)


class WorkspaceMembers(unittest.TestCase):
    def test_neovibes_own_crates_are_left_to_license(self):
        self.assertTrue(cl.own_code("shell", None))
        self.assertTrue(cl.own_code("terminal-render", "MIT"))

    def test_terminal_input_is_listed_under_its_own_licence(self):
        self.assertFalse(cl.own_code("terminal-input", "Apache-2.0"))

    def test_a_member_with_an_unexpected_licence_fails_the_run(self):
        with self.assertRaises(cl.Fail):
            cl.own_code("terminal-frame", "Apache-2.0")

    def test_terminal_input_ships_its_licence_and_its_notice(self):
        pkg = {"eco": "cargo", "name": "terminal-input", "version": "0.1.0", "license": "Apache-2.0",
               "dir": os.path.join(cl.REPO, "terminal-input"), "source": "", "license_file": None}
        entry = cl.resolve(pkg, {}, {})
        self.assertEqual(sorted(name for name, _ in entry["files"]), ["LICENSE-APACHE", "NOTICE"])
        notice = dict(entry["files"])["NOTICE"]
        self.assertIn("94e7c8874e526b1e67b349d9ba30ddf81669119e", notice)


class ShippedTree(unittest.TestCase):
    """Needs `cargo` and the workspace's lockfile; no build."""

    def test_the_shipped_tree_lists_terminal_input_and_none_of_neovibes_own(self):
        crates, _ = cl.cargo_packages()
        names = {p["name"] for p in crates}
        self.assertIn("terminal-input", names)
        self.assertTrue(names.isdisjoint({"shell", "neovibe-terminal", "terminal-render", "terminal-frame",
                                          "terminal-sync"}), names)

    def test_shipped_binaries_has_four_entries_and_no_agent_hook(self):
        """After Task 4 (D16) no release build compiles the legacy backend, so agent-hook is not in
        any release, in either profile (spec sec 10)."""
        self.assertNotIn("agent-hook", cl.SHIPPED_BINARIES)
        self.assertEqual(len(cl.SHIPPED_BINARIES), 4)

    def test_nvim_rs_carries_lgpl_and_gpl_texts(self):
        """4(b): the output must contain nvim-rs's own LGPL text and a copy of the GPL-3.0 text.
        Read straight off this workspace's real, already-cached dependency tree -- no network, no
        build (LICENSE-LGPL and GPL-3.0.txt are both plain text files a real run would read)."""
        crates, by_nv = cl.cargo_packages()
        nvimrs = next(p for p in crates if p["name"] == "nvim-rs")
        entry = cl.resolve(nvimrs, by_nv, {})
        files = dict(entry["files"])
        lgpl_name = next(n for n in files if n.startswith("LICENSE-LGPL"))
        self.assertIn("GNU LESSER GENERAL PUBLIC LICENSE", files[lgpl_name])
        gpl_name = next(n for n in files if n.startswith("GPL-3.0"))
        self.assertIn("GNU GENERAL PUBLIC LICENSE", files[gpl_name])
        self.assertTrue(entry["copyleft"])

    def test_the_rendered_no_sidecar_output_actually_contains_both_texts(self):
        """The line above proves resolve()'s entry has both texts; this proves render_entries() --
        what a real --no-sidecar run actually writes into PART 1 -- does not drop or truncate either
        on the way into the file."""
        crates, by_nv = cl.cargo_packages()
        nvimrs = next(p for p in crates if p["name"] == "nvim-rs")
        entry = cl.resolve(nvimrs, by_nv, {})
        lines = []
        cl.render_entries([entry], {}, lines)
        rendered = "\n".join(lines)
        self.assertIn("GNU LESSER GENERAL PUBLIC LICENSE", rendered)
        self.assertIn("GNU GENERAL PUBLIC LICENSE", rendered)
        self.assertIn("nvim-rs", rendered)


class RepoAndBinariesDirParams(unittest.TestCase):
    """--repo and --binaries-dir (Task 13 needs both: the public clone's Cargo.lock, the public
    release's extracted binaries -- spec sec 8 step 6)."""

    def test_repo_param_changes_where_cargo_metadata_runs(self):
        empty = _scratch_dir()
        try:
            with self.assertRaises(cl.Fail) as ctx:
                cl.cargo_packages(repo=empty)
            self.assertIn("Cargo.toml", str(ctx.exception))
        finally:
            shutil.rmtree(empty, ignore_errors=True)

    def test_binaries_dir_param_is_where_the_shipped_binaries_are_looked_up(self):
        empty = _scratch_dir()
        try:
            with self.assertRaises(cl.Fail) as ctx:
                cl.collect(cl.REPO, empty, True, "https://example.invalid/src")
            # Proves binaries_dir (not the module-level REPO/target/release) was actually used.
            self.assertIn(os.path.join(empty, "shell"), str(ctx.exception))
        finally:
            shutil.rmtree(empty, ignore_errors=True)


class SkiaLicenseLookup(unittest.TestCase):
    """The Skia licence text used to be read from <repo>/target/release/build/... unconditionally,
    ignoring --binaries-dir entirely: Task 12's container builds under a different CARGO_TARGET_DIR,
    and Task 13's public-clone-plus-extracted-tarball tree has no build/ subtree at all (spec sec 8
    step 6). skia_license_text() is now the standalone function collect()'s native_components() calls,
    testable on a fixture directory with no real binaries or a real build."""

    def _scratch(self):
        d = _scratch_dir()
        self.addCleanup(shutil.rmtree, d, ignore_errors=True)
        return d

    def test_found_under_binaries_dir_by_default(self):
        d = self._scratch()
        skia_dir = os.path.join(d, "build", "skia-bindings-deadbeef", "out", "skia")
        os.makedirs(skia_dir)
        with open(os.path.join(skia_dir, "LICENSE_SKIA"), "w") as f:
            f.write("fixture skia licence text\n")
        self.assertIn("fixture skia licence text", cl.skia_license_text(d))

    def test_override_directory_is_used_when_binaries_dir_has_no_build_subtree(self):
        d = self._scratch()  # binaries_dir itself: no build/ under it at all
        override_dir = os.path.join(self._scratch(), "extracted-skia-licence")
        skia_dir = os.path.join(override_dir, "skia-bindings-cafef00d", "out", "skia")
        os.makedirs(skia_dir)
        with open(os.path.join(skia_dir, "LICENSE_SKIA"), "w") as f:
            f.write("override fixture text\n")
        self.assertIn("override fixture text", cl.skia_license_text(d, override=override_dir))

    def test_fails_loudly_when_none_found(self):
        d = self._scratch()
        with self.assertRaises(cl.Fail):
            cl.skia_license_text(d)

    def test_fails_loudly_when_more_than_one_distinct_text_found(self):
        d = self._scratch()
        for i, text in enumerate(["one\n", "two\n"]):
            skia_dir = os.path.join(d, "build", f"skia-bindings-{i}", "out", "skia")
            os.makedirs(skia_dir)
            with open(os.path.join(skia_dir, "LICENSE_SKIA"), "w") as f:
                f.write(text)
        with self.assertRaises(cl.Fail):
            cl.skia_license_text(d)


class NvimRsSymbolCheck(unittest.TestCase):
    """Plan Task 7 checks by symbol which shipped binaries actually contain nvim-rs code (spec sec
    11.1), matching any DEMANGLED symbol containing "nvim_rs" -- not just a leading mangled prefix
    like `_ZN7nvim_rs`, which misses trait impls and monomorphised code and would undercount which
    binaries really carry it."""

    def test_demangled_defined_symbols_parses_real_nm_c_output_shape(self):
        fake_nm_output = (
            "0000000000012345 T main\n"
            "0000000000067890 t <nvim_rs::rpc::Client<T> as core::clone::Clone>::clone\n"
        )
        with mock.patch.object(cl, "run", return_value=fake_nm_output):
            syms = cl.demangled_defined_symbols("/fake/path")
        self.assertIn("main", syms)
        self.assertTrue(any("nvim_rs" in s for s in syms))

    def test_matches_the_crate_name_mid_symbol_not_only_as_a_leading_prefix(self):
        """The regression this check exists to close: a symbol where "nvim_rs" is nowhere near the
        start, which a `_ZN7nvim_rs` mangled-prefix match would never find."""
        mid_symbol = "<some_other_crate::Wrapper<neovide::editor::nvim_rs_bridge::Handle> as Trait>::call"
        fake_nm_output = f"0000000000012345 T {mid_symbol}\n"
        with mock.patch.object(cl, "run", return_value=fake_nm_output):
            hit = cl.binaries_containing_symbol_substring({"shell": "/fake/shell"}, "nvim_rs")
        self.assertEqual(hit, ["shell"])

    def test_binaries_containing_symbol_substring_only_names_the_real_hits(self):
        outputs = {
            "/fake/shell": "0000000000012345 T <nvim_rs::rpc::Client as core::clone::Clone>::clone\n",
            "/fake/other": "0000000000054321 T some_unrelated_symbol\n",
        }
        with mock.patch.object(cl, "run", side_effect=lambda cmd, cwd: outputs[cmd[-1]]):
            hit = cl.binaries_containing_symbol_substring(
                {"shell": "/fake/shell", "other": "/fake/other"}, "nvim_rs")
        self.assertEqual(hit, ["shell"])

    def test_nvim_rs_symbol_check_fails_loudly_if_shell_no_longer_carries_it(self):
        with mock.patch.object(cl, "run", return_value="0000000000012345 T main\n"):
            with self.assertRaises(cl.Fail):
                cl.nvim_rs_symbol_check({"shell": "/fake/shell"})

    def test_nvim_rs_symbol_check_returns_every_binary_that_carries_it(self):
        outputs = {
            "/fake/shell": "0000000000012345 T <nvim_rs::rpc::Client as core::clone::Clone>::clone\n",
            "/fake/other": "0000000000054321 T <nvim_rs::rpc::helper as core::fmt::Debug>::fmt\n",
        }
        with mock.patch.object(cl, "run", side_effect=lambda cmd, cwd: outputs[cmd[-1]]):
            hit = cl.nvim_rs_symbol_check({"shell": "/fake/shell", "other": "/fake/other"})
        self.assertEqual(hit, ["other", "shell"])

    def test_real_shell_binary_carries_nvim_rs_and_no_other_shipped_binary_does(self):
        """Against this workspace's actual release build, if one exists -- the same real measurement
        the review that asked for this check made by hand (245 hits in `shell`, 0 elsewhere)."""
        shipped = {b: os.path.join(cl.REPO, "target", "release", b) for b in cl.SHIPPED_BINARIES}
        if not all(os.path.isfile(p) for p in shipped.values()):
            self.skipTest("no real target/release build in this checkout")
        self.assertEqual(cl.nvim_rs_symbol_check(shipped), ["shell"])


class RustStdComponent(unittest.TestCase):
    """licences-claude-2: the note said "statically linked into all five binaries" -- stale since
    D16 dropped `agent-hook` and left four (the header a line above it already says so). Now
    derived from `len(SHIPPED_BINARIES)` rather than a literal number word, so the next binary
    count change fixes this note without anyone having to remember it exists."""

    def test_note_names_the_real_binary_count_not_a_stale_literal(self):
        scratch = _scratch_dir()
        doc_dir = os.path.join(scratch, "share", "doc", "rust")
        os.makedirs(doc_dir)
        with open(os.path.join(doc_dir, "COPYRIGHT-library.html"), "w") as f:
            f.write("<html><body>Notice text</body></html>")
        version = "1.90.0-fake (0123456789abcdef 2026-01-01)"
        readelf_out = f"String dump of section '.comment':\n  [     0]  rustc version {version}\x00\n"

        def fake_run(cmd, cwd):
            if cmd[0] == "readelf":
                return readelf_out
            if cmd[:2] == ["rustc", "--version"]:
                return f"rustc {version}\n"
            if cmd[:2] == ["rustc", "--print"]:
                return scratch + "\n"
            raise AssertionError(cmd)

        shipped = {b: os.path.join(scratch, f"fake-{b}") for b in cl.SHIPPED_BINARIES}
        with mock.patch.object(cl, "run", side_effect=fake_run):
            entry = cl.rust_std_component(shipped)
        self.assertEqual(len(cl.SHIPPED_BINARIES), 4, "this test's own expectation below assumes four")
        self.assertIn("all four binaries", entry["note"])
        self.assertNotIn("five", entry["note"])


class NpmPackages(unittest.TestCase):
    """licences-claude-3 (+licences-codex-2): THIRD-PARTY-LICENSES listed `@types/trusted-types`
    2.0.7 -- a types-only optional dependency of dompurify with no runtime code in the built
    bundle (`package-lock.json` has it under `node_modules/@types/trusted-types`,
    `optional: True`). Every `@types/*` package is DefinitelyTyped: `.d.ts` declarations only,
    never runtime JS, whatever depended on it or how -- so `npm_packages()` skips the whole scope
    rather than special-casing this one name."""

    def _fake_tree(self, root, names_and_licenses):
        os.makedirs(os.path.join(root, "node_modules"), exist_ok=True)
        with open(os.path.join(root, "package.json"), "w") as f:
            f.write('{"name": "root-project", "version": "0.0.0"}')
        dirs = [root]
        for name, version, license_ in names_and_licenses:
            pkg_dir = os.path.join(root, "node_modules", *name.split("/"))
            os.makedirs(pkg_dir, exist_ok=True)
            with open(os.path.join(pkg_dir, "package.json"), "w") as f:
                f.write(f'{{"name": "{name}", "version": "{version}", "license": "{license_}"}}')
            dirs.append(pkg_dir)
        return dirs

    def test_types_only_packages_are_skipped(self):
        root = _scratch_dir()
        dirs = self._fake_tree(root, [
            ("dompurify", "3.2.7", "Apache-2.0"),
            ("@types/trusted-types", "2.0.7", "MIT"),
        ])
        with mock.patch.object(cl, "run", return_value="\n".join(dirs)):
            packages = cl.npm_packages(root)
        names = {p["name"] for p in packages}
        self.assertIn("dompurify", names)
        self.assertNotIn("@types/trusted-types", names)
        self.assertFalse(any(n.startswith("@types/") for n in names))


class NoScratchDiskPathShipsInPinsEnv(unittest.TestCase):
    """leaks-claude-4 (+leaks-codex-3), packaging half: the owner's own scratch-disk mount point is
    not something a public reader needs -- packaging/pins.env's own comment used to name it
    literally. Deliberately not self-checking THIS file's own source: a test asserting a string's
    absence has to spell that string out to compare against, which would recreate the very leak
    it exists to catch. publish/tests/ scans the real shipped source (including this file) for the
    same marker without that paradox; see publish/scan.sh's private-path rule."""

    # Built from parts so this file's own source never contains the forbidden substring
    # contiguously (the paradox the class docstring names).
    _SCRATCH_DISK_MARKER = "/" + "srv" + "/" + "scratch"

    def test_pins_env_names_no_scratch_disk_path(self):
        path = os.path.join(cl.REPO, "packaging", "pins.env")
        with open(path, encoding="utf-8") as f:
            text = f.read()
        self.assertNotIn(self._SCRATCH_DISK_MARKER, text, path)


# The pinned public source-asset archive (Task 3's pin, verdict #7's own subject): tag 0.153.3,
# sha256 ef41a8ff...0caf10. rust-skia's build script caches it under a skia-bindings-* build
# directory's out/.cache/ -- never modified in place, only ever copied out of, by every test below
# that reads it.
_PINNED_SKIA_ARCHIVE_SHA256 = "ef41a8ff85bebeb06238df88e144eb04778a368fc1672a63155155d97f0caf10"


# Where a pinned archive is cached, at fixed depths: release.sh's own checked cache (Task 12 M11),
# then a skia-bindings build dir under some target/<profile>/build/. The recursive glob this
# replaced walked every build and release tree under the cargo target root -- millions of files
# once release.sh kept its work/, proof/ and target/ there -- and took more than ten minutes
# (Task 4 fix round 1). Two entries once named the owner's own scratch disk explicitly
# (leaks-claude-4/leaks-codex-3, 2026-09-28); dropped, not just rewritten at export, because
# `target/` (repo-relative, below) and a `~/.cache/neovibe-release` symlink already reach the same
# cache through paths this file never has to spell.
_SKIA_CACHE_GLOBS = (
    "~/.cache/neovibe-release/skia/skia-binaries-*.tar.gz",
    os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))),
                 "target/*/build/skia-bindings-*/out/.cache/skia-binaries-*.tar.gz"),
    "~/.cargo/target/*/build/skia-bindings-*/out/.cache/skia-binaries-*.tar.gz",
)


def _find_pinned_skia_archive():
    for path in (p for g in _SKIA_CACHE_GLOBS for p in sorted(glob.glob(os.path.expanduser(g)))):
        h = hashlib.sha256()
        with open(path, "rb") as f:
            for chunk in iter(lambda: f.read(1 << 20), b""):
                h.update(chunk)
        if h.hexdigest() == _PINNED_SKIA_ARCHIVE_SHA256:
            return path
    return None


class SkiaArchiveMemberClassification(unittest.TestCase):
    """_skia_archive_member_component(): pure, no I/O -- the same fail-closed shape as
    native_components()'s c_component(), one level earlier (over the archive itself, verdict #7)."""

    def test_known_third_party_stems(self):
        self.assertEqual(cl._skia_archive_member_component("libexpat"), "expat")
        self.assertEqual(cl._skia_archive_member_component("libfreetype2"), "FreeType")
        self.assertEqual(cl._skia_archive_member_component("libharfbuzz"), "HarfBuzz")
        self.assertEqual(cl._skia_archive_member_component("libjpeg"), "libjpeg-turbo")
        self.assertEqual(cl._skia_archive_member_component("libjpeg12"), "libjpeg-turbo")
        self.assertEqual(cl._skia_archive_member_component("libjpeg16"), "libjpeg-turbo")
        self.assertEqual(cl._skia_archive_member_component("libpng"), "libpng")
        self.assertEqual(cl._skia_archive_member_component("libwuffs"), "Wuffs")
        self.assertEqual(cl._skia_archive_member_component("libicu"), "ICU")
        self.assertEqual(cl._skia_archive_member_component("libzlib"), "zlib")

    def test_chromium_zlib_simd_stems(self):
        for stem in ("zlib_adler32_simd", "zlib_crc32_simd", "zlib_inflate_chunk_simd"):
            self.assertEqual(cl._skia_archive_member_component(stem), "Chromium zlib")

    def test_skias_own_codec_wrappers_are_skia_not_the_library_they_wrap(self):
        """jpeg_decode.SkJpegCodec.o and png_decode_libpng.SkPngCodec.o are Skia's OWN wrapper
        classes around libjpeg-turbo/libpng, not those libraries' own source -- checked against the
        real archive's actual defined symbols (all Sk*), not assumed from the name looking similar."""
        self.assertEqual(cl._skia_archive_member_component("jpeg_decode"), "Skia")
        self.assertEqual(cl._skia_archive_member_component("jpeg_encode"), "Skia")
        self.assertEqual(cl._skia_archive_member_component("png_decode_libpng"), "Skia")
        self.assertEqual(cl._skia_archive_member_component("png_encode_common"), "Skia")
        self.assertEqual(cl._skia_archive_member_component("wuffs"), "Skia")  # wuffs.SkWuffsCodec.o

    def test_skias_own_targets(self):
        for stem in ("core", "gpu", "gpu_shared", "pathops", "pdf", "xml", "libskia", "libskparagraph",
                     "libskshaper", "libskunicode_core", "libskunicode_icu", "libskcms",
                     "fontmgr_custom", "skcms_TransformHsw"):
            self.assertEqual(cl._skia_archive_member_component(stem), "Skia", stem)

    def test_skia_bindings_hash_prefixed_objects_are_skia(self):
        self.assertEqual(cl._skia_archive_member_component("0602fb52cb66f316-bindings"), "Skia")

    def test_an_unrecognised_stem_is_none(self):
        self.assertIsNone(cl._skia_archive_member_component("libtotally-new-thing"))


class SkiaArchiveComponents(unittest.TestCase):
    """skia_archive_components(): real `ar t`/`nm --defined-only` plumbing over fixture .a files."""

    def setUp(self):
        self.dir = _scratch_dir()
        self.addCleanup(shutil.rmtree, self.dir, ignore_errors=True)

    def test_classifies_by_member_name_fixture_ar_output(self):
        """Mocked ar t/nm text (`run()`), as this file's other symbol-based tests already do --
        the classifier itself (parsing + lookup) is the code under test, not a real archive. The
        Skia members' nm output is empty: their own third-party-symbol corroboration
        (_fail_if_a_skia_member_defines_a_third_party_symbol) is exercised separately below."""
        for name in ("libskia.a", "libskshaper.a"):
            open(os.path.join(self.dir, name), "wb").close()
        ar_t = {
            os.path.join(self.dir, "libskia.a"): "libexpat.xmlparse.o\ncore.SkCanvas.o\n",
            os.path.join(self.dir, "libskshaper.a"): "libharfbuzz.hb-buffer.o\nlibskshaper.SkShaper.o\n",
        }

        def fake_run(cmd, cwd):
            if cmd[0] == "ar":
                return ar_t[cmd[2]]
            if cmd[:3] == ["nm", "--defined-only", "-A"]:
                return ""
            raise AssertionError(f"unexpected command {cmd}")

        with mock.patch.object(cl, "run", side_effect=fake_run):
            found = cl.skia_archive_components(self.dir)
        self.assertEqual(found["expat"], ["libskia.a(libexpat.xmlparse.o)"])
        self.assertEqual(found["Skia"],
                         ["libskia.a(core.SkCanvas.o)", "libskshaper.a(libskshaper.SkShaper.o)"])
        self.assertEqual(found["HarfBuzz"], ["libskshaper.a(libharfbuzz.hb-buffer.o)"])

    def test_a_skia_classified_member_defining_a_third_party_symbol_fails_the_run(self):
        """The corroborating check's own Fail path (verdict #7, review minor 4): a member this
        script's own name table would classify as Skia, but whose real defined symbols say
        otherwise, must not be silently trusted -- covers a hypothetical future archive that compiled
        third-party source into an existing Skia-named build target, which the name-only
        classification above cannot see by construction."""
        open(os.path.join(self.dir, "libskia.a"), "wb").close()

        def fake_run(cmd, cwd):
            if cmd[0] == "ar":
                return "core.SneakyThirdParty.o\n"  # "core" is a known Skia-own stem
            if cmd[:3] == ["nm", "--defined-only", "-A"]:
                path = cmd[3]
                return f"{path}:core.SneakyThirdParty.o:0000000000000000 T hb_shape\n"
            raise AssertionError(f"unexpected command {cmd}")

        with mock.patch.object(cl, "run", side_effect=fake_run):
            with self.assertRaises(cl.Fail) as ctx:
                cl.skia_archive_components(self.dir)
        self.assertIn("core.SneakyThirdParty.o", str(ctx.exception))
        self.assertIn("hb_shape", str(ctx.exception))
        self.assertIn("HarfBuzz", str(ctx.exception))

    def test_an_unrecognised_member_fails_the_run_naming_it(self):
        open(os.path.join(self.dir, "libweird.a"), "wb").close()
        with mock.patch.object(cl, "run", return_value="somenewlib.SomeFile.o\n"):
            with self.assertRaises(cl.Fail) as ctx:
                cl.skia_archive_components(self.dir)
        self.assertIn("somenewlib.SomeFile.o", str(ctx.exception))

    def test_no_a_files_at_all_fails_the_run(self):
        with self.assertRaises(cl.Fail):
            cl.skia_archive_components(self.dir)

    def test_real_ar_and_nm_over_a_tiny_compiled_archive(self):
        """A real, tiny .a built with cc -c/ar (not mocked), proving the actual subprocess-parsing
        path against real tool output -- one recognised member (a stand-in expat object, named the
        way rust-skia's build_support actually names one) and one this script has never seen."""
        if not (shutil.which("cc") and shutil.which("ar")):
            self.skipTest("no cc/ar on PATH")
        src_known = os.path.join(self.dir, "known.c")
        with open(src_known, "w") as f:
            f.write("void XML_ParserCreate(void) {}\n")
        obj_known = os.path.join(self.dir, "libexpat.xmlparse.o")
        subprocess.run(["cc", "-c", src_known, "-o", obj_known], check=True, capture_output=True)
        archive = os.path.join(self.dir, "libskia.a")
        subprocess.run(["ar", "rcs", archive, obj_known], check=True, capture_output=True)
        found = cl.skia_archive_components(self.dir)
        self.assertEqual(found, {"expat": ["libskia.a(libexpat.xmlparse.o)"]})

    def test_real_pinned_archive_matches_verdict_7s_evidence_table(self):
        """the private review notes #7's own table, against the real,
        pinned (sha256-checked) archive this release links -- skipped where that build cache is
        absent, the same shape as test_real_shell_binary_carries_nvim_rs... above. Every count
        matches exactly once skia_archive_symbol_counts() counts only "T" (global function) symbols,
        which is what the table's own numbers turn out to be: counting every global type (T/R/D/W/...)
        overcounts Wuffs 124 vs. 71 (its several R/D/W-typed internal data tables also start
        "wuffs_"), and libjpeg-turbo 125 vs. 123 (2 more jpeg_* land in the archive's separate
        12-/16-bit precision member groups, libjpeg12.*/libjpeg16.* -- both inside libskia.a, not
        separate .a files -- which the table did not count).

        Extracted into this test's own scratch directory, never beside the cached archive itself
        (review found the previous version of _skia_archive_root() doing exactly that, mutating
        another lane's shared cargo build cache in place on every run of this test), and removed
        again afterwards: the extraction is 63 MB, and a scratch directory with no cleanup piled one
        more copy into ~/.cache on every run (review, Task 2 fix round 2)."""
        archive = _find_pinned_skia_archive()
        if archive is None:
            self.skipTest("the pinned Skia prebuilt archive is not cached on this machine")
        extract_dir = _scratch_dir()
        self.addCleanup(shutil.rmtree, extract_dir, ignore_errors=True)
        root = cl._skia_archive_root(archive, extract_dir)
        self.assertEqual(root, os.path.join(extract_dir, cl.SKIA_ARCHIVE_EXTRACT_SUBDIR))
        self.assertFalse(os.path.exists(archive + ".extracted"),
                          "must not write beside the cached archive it was given")
        found = cl.skia_archive_components(root)
        for comp in ("expat", "libjpeg-turbo", "Wuffs", "HarfBuzz", "ICU", "Skia", "FreeType", "libpng", "zlib"):
            self.assertIn(comp, found, comp)
        counts = cl.skia_archive_symbol_counts(root)
        self.assertEqual(counts["expat"], 71)
        self.assertEqual(counts["libjpeg-turbo"], 89 + 34)
        self.assertEqual(counts["Wuffs"], 71)
        self.assertEqual(counts["HarfBuzz"], 547 + 547)
        self.assertEqual(counts["ICU"], 194)


class SkiaArchiveNoticeEntries(unittest.TestCase):
    """skia_archive_notice_entries(): the entries collect() renders into the archive-contents
    section, each with real vendored licence text attached."""

    def setUp(self):
        self.dir = _scratch_dir()
        self.addCleanup(shutil.rmtree, self.dir, ignore_errors=True)

    def test_every_detected_component_gets_a_real_vendored_text(self):
        open(os.path.join(self.dir, "libskia.a"), "wb").close()
        with open(os.path.join(self.dir, "LICENSE_SKIA"), "w") as f:
            f.write("fixture Skia licence text\n")
        ar_t = ("libexpat.xmlparse.o\nlibharfbuzz.hb-buffer.o\nlibicu.bocsu.o\nlibjpeg.jdapimin.o\n"
                "libwuffs.wuffs-v0.3.o\nlibfreetype2.ftinit.o\nlibpng.png.o\nzlib_adler32_simd.adler32_simd.o\n"
                "core.SkCanvas.o\n")
        with mock.patch.object(cl, "run", return_value=ar_t):
            entries = cl.skia_archive_notice_entries(self.dir, None)
        titles = {e["title"] for e in entries}
        self.assertEqual(titles, {"expat", "HarfBuzz", "ICU", "libjpeg-turbo", "Wuffs", "FreeType",
                                  "libpng", "Chromium zlib", "Skia"})
        by_title = {e["title"]: e for e in entries}
        self.assertIn("Expat maintainers", dict(by_title["expat"]["files"])["expat.COPYING"])
        self.assertIn("Old MIT", dict(by_title["HarfBuzz"]["files"])["harfbuzz.COPYING"])
        self.assertIn("UNICODE LICENSE", dict(by_title["ICU"]["files"])["icu.LICENSE"])
        self.assertEqual(dict(by_title["Skia"]["files"])["LICENSE_SKIA"], "fixture Skia licence text\n")
        self.assertIn("IJG", by_title["libjpeg-turbo"]["license"])
        # Review minor 6: only Skia's own text comes from the archive, not packaging/license-texts/.
        self.assertIn("its own LICENSE_SKIA inside the archive", by_title["Skia"]["note"])
        self.assertNotIn("vendored", by_title["Skia"]["note"])
        self.assertIn("text vendored in packaging/license-texts/", by_title["expat"]["note"])
        self.assertEqual(by_title["ICU"]["license"], "Unicode-3.0")

    def test_a_tar_gz_archive_is_extracted_once_and_reused(self):
        """_skia_archive_root() accepts the raw .tar.gz too (release.sh's own --skia-archive-path
        input), extracting it into a caller-supplied extract_dir -- never beside the archive itself
        (which may sit in a read-only mount, release.sh's own /build/skia in the offline build phase)
        and never into /tmp."""
        src_root = os.path.join(self.dir, "skia-binaries")
        os.makedirs(src_root)
        open(os.path.join(src_root, "libskia.a"), "wb").close()
        with open(os.path.join(src_root, "LICENSE_SKIA"), "w") as f:
            f.write("tarball fixture licence\n")
        archive = os.path.join(self.dir, "skia-binaries-fixture.tar.gz")
        with tarfile.open(archive, "w:gz") as tf:
            tf.add(src_root, arcname="skia-binaries")
        extract_dir = os.path.join(self.dir, "extracted")
        with mock.patch.object(cl, "run", return_value="libexpat.xmlparse.o\n"):
            entries = cl.skia_archive_notice_entries(archive, extract_dir)
        self.assertEqual([e["title"] for e in entries], ["expat"])
        owned = os.path.join(extract_dir, cl.SKIA_ARCHIVE_EXTRACT_SUBDIR)
        self.assertTrue(os.path.isfile(os.path.join(owned, "skia-binaries", "LICENSE_SKIA")))
        self.assertFalse(os.path.exists(archive + ".extracted"), "must not write beside the archive")
        # Re-running must not fail on the leftover extraction from the first call, and must not
        # serve anything the first call (or anyone else) left in the owned subdirectory.
        stale = os.path.join(owned, "stale-from-an-earlier-run")
        open(stale, "w").close()
        with mock.patch.object(cl, "run", return_value="libexpat.xmlparse.o\n"):
            cl.skia_archive_notice_entries(archive, extract_dir)
        self.assertFalse(os.path.exists(stale))

    def _fixture_tarball(self, directory):
        os.makedirs(directory, exist_ok=True)
        archive = os.path.join(directory, "skia-binaries-fixture.tar.gz")
        with tarfile.open(archive, "w:gz") as tf:
            info = tarfile.TarInfo("skia-binaries/LICENSE_SKIA")
            data = b"fixture\n"
            info.size = len(data)
            tf.addfile(info, io.BytesIO(data))
        return archive

    def test_only_its_own_subdirectory_of_extract_dir_is_replaced(self):
        """extract_dir is the caller's directory, not this script's: only the fixed
        SKIA_ARCHIVE_EXTRACT_SUBDIR inside it is removed and rebuilt (review, Task 2 fix round 2 --
        the previous version rmtree'd extract_dir itself, so `--skia-archive-extract-dir /build/out`
        would have wiped the release output)."""
        archive = self._fixture_tarball(os.path.join(self.dir, "input"))
        extract_dir = os.path.join(self.dir, "out")
        os.makedirs(extract_dir)
        keep = os.path.join(extract_dir, "neovibe-1.0.0-x86_64.tar.gz")
        with open(keep, "w") as f:
            f.write("a release asset\n")
        root = cl._skia_archive_root(archive, extract_dir)
        self.assertEqual(root, os.path.join(extract_dir, cl.SKIA_ARCHIVE_EXTRACT_SUBDIR))
        with open(keep) as f:
            self.assertEqual(f.read(), "a release asset\n")

    def test_an_archive_inside_extract_dir_itself_survives(self):
        """The directory holding the archive given as extract_dir: the input must not be deleted
        before tarfile.open() (the previous version died with FileNotFoundError here)."""
        archive = self._fixture_tarball(os.path.join(self.dir, "skia"))
        root = cl._skia_archive_root(archive, os.path.dirname(archive))
        self.assertTrue(os.path.isfile(archive))
        self.assertTrue(os.path.isfile(os.path.join(root, "skia-binaries", "LICENSE_SKIA")))

    def test_an_archive_inside_the_owned_subdirectory_fails_closed(self):
        """The one place replacing the owned subdirectory would delete the input: refuse, naming it,
        rather than lose the archive."""
        extract_dir = os.path.join(self.dir, "scratch")
        archive = self._fixture_tarball(os.path.join(extract_dir, cl.SKIA_ARCHIVE_EXTRACT_SUBDIR, "nested"))
        with self.assertRaises(cl.Fail) as ctx:
            cl._skia_archive_root(archive, extract_dir)
        self.assertIn(cl.SKIA_ARCHIVE_EXTRACT_SUBDIR, str(ctx.exception))
        self.assertTrue(os.path.isfile(archive))

    def test_extract_dir_is_required_for_a_tar_gz_archive(self):
        """Fail closed (review important 1/2): without an extract_dir, this must refuse rather than
        fall back to writing beside the archive (which is what the previous version did)."""
        archive = os.path.join(self.dir, "skia-binaries-fixture.tar.gz")
        with tarfile.open(archive, "w:gz"):
            pass
        with self.assertRaises(cl.Fail) as ctx:
            cl.skia_archive_notice_entries(archive, None)
        self.assertIn("extract_dir", str(ctx.exception))

    def test_expected_sha256_mismatch_fails_before_extraction(self):
        """--skia-sha256, when given, is checked against --skia-archive-path's own bytes before
        anything is extracted or trusted (review minor 5) -- SOURCE names one archive by hash, and
        collect() must not silently generate notices from a different file of the same name."""
        archive = os.path.join(self.dir, "skia-binaries-fixture.tar.gz")
        with tarfile.open(archive, "w:gz"):
            pass
        extract_dir = os.path.join(self.dir, "extracted")
        with self.assertRaises(cl.Fail) as ctx:
            cl._skia_archive_root(archive, extract_dir, expected_sha256="ab" * 32)
        self.assertIn("ab" * 32, str(ctx.exception))
        self.assertFalse(os.path.exists(extract_dir), "must not extract before the hash is checked")

    def test_expected_sha256_match_proceeds(self):
        archive = os.path.join(self.dir, "skia-binaries-fixture.tar.gz")
        with tarfile.open(archive, "w:gz") as tf:
            info = tarfile.TarInfo("skia-binaries/LICENSE_SKIA")
            data = b"fixture\n"
            info.size = len(data)
            tf.addfile(info, io.BytesIO(data))
        with open(archive, "rb") as f:
            real_sha256 = hashlib.sha256(f.read()).hexdigest()
        extract_dir = os.path.join(self.dir, "extracted")
        root = cl._skia_archive_root(archive, extract_dir, expected_sha256=real_sha256)
        self.assertEqual(root, os.path.join(extract_dir, cl.SKIA_ARCHIVE_EXTRACT_SUBDIR))


class CFileClassification(unittest.TestCase):
    """classify_c_file(): the closed set of C source files native_components() accepts in a shipped
    binary. GCC's crtstuff.c joined it on 2026-09-28, when the release container's Ubuntu 24.04 GCC
    (which keeps crtbeginS.o's STT_FILE entry) built `shell` for the first time."""

    def test_known_files_map_to_their_components(self):
        for f, comp in (("lapi.c", "Lua"), ("ftbase.c", "FreeType"), ("sfnt.c", "FreeType"),
                        ("pngread.c", "libpng"), ("inflate.c", "zlib"), ("crc32_simd.c", "Chromium zlib"),
                        ("crtstuff.c", "GCC runtime")):
            self.assertEqual(cl.classify_c_file(f, {"lapi.c"}), comp, f)

    def test_an_unknown_file_has_no_component(self):
        for f in ("crtstuff.cc", "start.c", "libgcc2.c", "hb-blob.c"):
            self.assertIsNone(cl.classify_c_file(f, set()), f)

    def test_the_gcc_runtime_entry_carries_the_pinned_exception_text(self):
        entry = cl.gcc_runtime_component({"shell", "neovibe-supervisor"})
        self.assertEqual(entry["title"], cl.GCC_RUNTIME_TITLE)
        self.assertEqual(entry["license"], "GPL-3.0-or-later WITH GCC-exception-3.1")
        self.assertIn("neovibe-supervisor, shell", entry["note"])
        (name, text), = entry["files"]
        self.assertEqual(name, "COPYING.RUNTIME")
        self.assertTrue(text.startswith("GCC RUNTIME LIBRARY EXCEPTION\n\nVersion 3.1, 31 March 2009"))
        with open(os.path.join(cl.TEXTS, "gcc-COPYING.RUNTIME"), "rb") as f:
            vendored = f.read()
        # GCC's releases/gcc-13.3.0 COPYING.RUNTIME, byte for byte (packaging/license-texts/README.md).
        self.assertEqual(hashlib.sha256(vendored).hexdigest(),
                         "9d6b43ce4d8de0c878bf16b54d8e7a10d9bd42b75178153e3af6a815bdc90f74")
        self.assertEqual(text, cl.read_text(os.path.join(cl.TEXTS, "gcc-COPYING.RUNTIME")))


class BuildHeaderProfiles(unittest.TestCase):
    """build_header() is the prose block before PART 1, split out so its two profile shapes can be
    checked without a real build (Review Focus 4/5)."""

    FAKE_COPYLEFT = [{"title": "nvim-rs 0.9.2", "license": "LGPL-3.0", "name": "nvim-rs", "version": "0.9.2"}]

    def test_no_sidecar_header_starts_with_the_lgpl_notice_and_has_no_sdk_paragraph(self):
        lines = cl.build_header(True, "https://example.invalid/release-asset.tar.gz", self.FAKE_COPYLEFT,
                                 3, 2, 1, None, 0, {"LGPL-3.0": 1, "MIT": 2}, {}, {}, skia_archive_count=9)
        text = "\n".join(lines)
        # RULE, title, RULE, "" precede the notice: it is the header's first substantive line.
        self.assertEqual(lines[4], cl.nvim_rs_lgpl_notice("0.9.2"))
        self.assertIn("neovibe's own code is MIT-licensed: see LICENSE, installed beside this file.", text)
        self.assertNotIn("NOT OPEN SOURCE", text)
        self.assertNotIn("/usr/lib/neovibe/verdandi-claude-sidecar", text)
        self.assertIn("neovibe's own source is at https://example.invalid/release-asset.tar.gz.", text)
        self.assertNotIn("part 4", text)
        # Review minor 6: the "Contents:" list used to stop at part 3 and never mention this section.
        self.assertIn("components inside the Skia prebuilt archive in the source asset      (9)", text)

    def test_sidecar_header_never_mentions_the_skia_archive_section(self):
        """The private/--sidecar profile does not ship the archive (it has no such section to list),
        so build_header() must not claim it does even if a stray skia_archive_count were passed."""
        sc = {"sdk": "1.2.3", "node": "v22.23.2"}
        lines = cl.build_header(False, cl.DEFAULT_SOURCE_URL, self.FAKE_COPYLEFT, 3, 2, 1, sc, 5,
                                 {"LGPL-3.0": 1, "MIT": 2}, {}, {"MIT": 5}, skia_archive_count=9)
        self.assertNotIn("Skia prebuilt archive", "\n".join(lines))

    def test_sidecar_header_also_carries_the_4a_notice_first(self):
        """Both profiles ship the same statically-linked `shell`, so 4(a)'s notice is owed in both
        (spec sec 11.3: "the same sentence is at the top of THIRD-PARTY-LICENSES") -- this is the one
        way the two profiles' headers deliberately do NOT match; everything else about the sidecar
        profile's shape (the NOT OPEN SOURCE paragraph, part 4) is unaffected."""
        sc = {"sdk": "1.2.3", "node": "v22.23.2"}
        lines = cl.build_header(False, cl.DEFAULT_SOURCE_URL, self.FAKE_COPYLEFT, 3, 2, 1, sc, 5,
                                 {"LGPL-3.0": 1, "MIT": 2}, {}, {"MIT": 5})
        text = "\n".join(lines)
        self.assertEqual(lines[4], cl.nvim_rs_lgpl_notice("0.9.2"))
        self.assertIn("neovibe's own code is MIT-licensed: see LICENSE, installed beside this file.", text)
        self.assertIn("ONE PART IS NOT OPEN SOURCE", text)
        self.assertIn("/usr/lib/neovibe/verdandi-claude-sidecar", text)
        self.assertIn("part 4  the sidecar: Node.js v22.23.2 and its npm packages", text)

    def test_the_notices_version_is_the_resolved_crate_version_not_a_hardcoded_one(self):
        copyleft = [{"title": "nvim-rs 9.9.9", "license": "LGPL-3.0", "name": "nvim-rs", "version": "9.9.9"}]
        lines = cl.build_header(True, "https://example.invalid/x", copyleft, 1, 1, 1, None, 0, {}, {}, {})
        self.assertEqual(lines[4], cl.nvim_rs_lgpl_notice("9.9.9"))
        self.assertIn("nvim-rs 9.9.9", lines[4])

    def test_fails_loudly_with_no_nvim_rs_in_copyleft(self):
        with self.assertRaises(cl.Fail):
            cl.build_header(True, "https://example.invalid/x", [], 1, 1, 1, None, 0, {}, {}, {})


class WriteSourceNotice(unittest.TestCase):
    def setUp(self):
        self.dir = _scratch_dir()
        self.addCleanup(shutil.rmtree, self.dir, ignore_errors=True)
        self.path = os.path.join(self.dir, "SOURCE")
        self.text = cl.write_source_notice(
            self.path, "1.0.0-rc.1", "cafef00dcafef00dcafef00dcafef00dcafef00d",
            "https://github.com/HunterGrey-cyber/neovibe/releases/download/v1.0.0-rc.1/neovibe-1.0.0-rc.1-source.tar.gz",
            "skia-binaries-abcdef0.tar.gz", "d34db33f" * 8, "0.9.2", "f0e1d2c3" * 5,
        )

    def test_writes_the_file_and_returns_the_same_text(self):
        with open(self.path, encoding="utf-8") as f:
            self.assertEqual(f.read(), self.text)

    def test_contains_the_4a_notice_with_the_given_nvim_rs_version_and_the_url_verbatim(self):
        self.assertTrue(self.text.startswith(cl.nvim_rs_lgpl_notice("0.9.2")))
        self.assertIn(
            "https://github.com/HunterGrey-cyber/neovibe/releases/download/v1.0.0-rc.1/neovibe-1.0.0-rc.1-source.tar.gz",
            self.text)

    def test_names_the_version_it_was_built_for(self):
        self.assertIn("neovibe 1.0.0-rc.1", self.text)

    def test_the_nvim_rs_version_in_the_notice_is_the_given_one_not_a_hardcoded_default(self):
        d = _scratch_dir()
        self.addCleanup(shutil.rmtree, d, ignore_errors=True)
        text = cl.write_source_notice(
            os.path.join(d, "SOURCE"), "1.0.0-rc.1", "cafef00dcafef00dcafef00dcafef00dcafef00d",
            "https://example.invalid/asset.tar.gz", "skia-binaries-abcdef0.tar.gz", "d34db33f" * 8,
            "9.9.9", "f0e1d2c3" * 5,
        )
        self.assertTrue(text.startswith("This program statically links nvim-rs 9.9.9,"))

    def test_names_web_bundle_rebuild_and_the_commit_pin(self):
        self.assertIn("npm ci && npm run build", self.text)
        self.assertIn("NEOVIBE_BUILD_COMMIT=cafef00dcafef00dcafef00dcafef00dcafef00d", self.text)

    def test_names_the_fork_commit_and_how_a_rebuild_gets_it_back(self):
        """Whole-branch review (lane D): the asset has no .git in neovide/, so a rebuild from it
        said "neovide fork unknown" unless SOURCE says which commit to set."""
        self.assertIn("    NEOVIBE_BUILD_FORK_COMMIT=" + "f0e1d2c3" * 5 + "\n", self.text)
        self.assertIn("at its pinned commit " + "f0e1d2c3" * 5, self.text)

    def test_names_the_skia_archive_and_its_sha256(self):
        self.assertIn("skia-binaries-abcdef0.tar.gz", self.text)
        self.assertIn("d34db33f" * 8, self.text)

    def test_names_verdandis_proto_tree_and_why_the_offline_rebuild_needs_it(self):
        """M1 (pre-think sec 2, verdict #6): claude-runtime-protocol's build script compiles
        ../../proto/verdandi/claude/runtime/v1/runtime.proto, outside the crate `cargo vendor`
        copies, so the asset must carry proto/ at its root for the offline rebuild to resolve it."""
        self.assertIn("proto/", self.text)
        self.assertIn("claude-runtime-protocol", self.text)
        self.assertIn("../../proto", self.text)

    def test_states_the_no_runtime_notices_and_no_restrictive_terms_rules(self):
        self.assertIn("copyright notices at run time", self.text)
        self.assertIn("About view", self.text)
        self.assertIn("restricting modification or reverse engineering", self.text)

    def test_names_option_exts_mpl_obligation(self):
        self.assertIn("option-ext", self.text)
        self.assertIn("MPL-2.0", self.text)

    def test_relink_recipe_never_pins_the_lockfile_and_never_suggests_editing_vendor_in_place(self):
        recipe = self.text[self.text.index("Relinking against a MODIFIED nvim-rs"):]
        self.assertNotIn("--locked", recipe)
        self.assertIn("nvim-rs-modified", recipe)
        self.assertIn(".cargo-checksum.json", recipe)
        self.assertIn("[patch.crates-io]", recipe)
        self.assertIn('SKIA_BINARIES_URL="file://$PWD/skia/skia-binaries-abcdef0.tar.gz"', recipe)
        self.assertIn("cargo build --release --offline -p shell", recipe)
        self.assertNotIn("edit vendor/nvim-rs", recipe.lower())


class MainArgParsing(unittest.TestCase):
    """main()'s validation and wiring, with collect() mocked out so these stay fast and need no
    real build, network or Verdandi checkout."""

    def setUp(self):
        self.dir = _scratch_dir()
        self.addCleanup(shutil.rmtree, self.dir, ignore_errors=True)
        self.out = os.path.join(self.dir, "THIRD-PARTY-LICENSES")

    @staticmethod
    def _fake_collect(calls):
        def fake(repo, binaries_dir, no_sidecar, source_url, sidecar_artifact=None, verdandi=None,
                  skia_license_dir=None, skia_archive_path=None, skia_archive_extract_dir=None,
                  skia_archive_sha256=None):
            calls.update(repo=repo, binaries_dir=binaries_dir, no_sidecar=no_sidecar, source_url=source_url,
                         sidecar_artifact=sidecar_artifact, verdandi=verdandi, skia_license_dir=skia_license_dir,
                         skia_archive_path=skia_archive_path, skia_archive_extract_dir=skia_archive_extract_dir,
                         skia_archive_sha256=skia_archive_sha256)
            return "text\n", {"rust": {}, "native": [], "web": {}, "sidecar": {}, "excluded": [],
                              "skia_archive": [], "nvim_rs_version": "0.9.2"}
        return fake

    def test_no_sidecar_and_sidecar_together_is_an_error(self):
        rc = cl.main(["--sidecar", "/x", "--no-sidecar", "--source-url", "http://y", "--out", self.out])
        self.assertEqual(rc, 2)

    def test_no_sidecar_without_source_url_is_an_error(self):
        rc = cl.main(["--no-sidecar", "--out", self.out])
        self.assertEqual(rc, 2)

    def test_no_sidecar_without_skia_archive_path_is_an_error(self):
        rc = cl.main(["--no-sidecar", "--source-url", "http://y", "--out", self.out])
        self.assertEqual(rc, 2)

    def test_source_notice_requires_skia_archive_and_sha256(self):
        rc = cl.main(["--source-notice", os.path.join(self.dir, "SOURCE"), "--out", self.out])
        self.assertEqual(rc, 2)

    def test_source_notice_requires_an_explicit_source_url_even_under_sidecar(self):
        """--sidecar's own default (DEFAULT_SOURCE_URL, the bare repo) must never end up named as
        "that asset" in SOURCE's text -- the bare repo contains none of what SOURCE promises."""
        rc = cl.main(["--source-notice", os.path.join(self.dir, "SOURCE"), "--skia-archive", "x.tar.gz",
                      "--skia-sha256", "ab" * 32, "--out", self.out])
        self.assertEqual(rc, 2)

    def test_skia_license_dir_is_passed_through_to_collect(self):
        calls = {}
        with mock.patch.object(cl, "collect", self._fake_collect(calls)):
            rc = cl.main(["--skia-license-dir", "/some/extracted/skia", "--out", self.out])
        self.assertEqual(rc, 0)
        self.assertEqual(calls["skia_license_dir"], "/some/extracted/skia")

    def test_default_source_url_is_the_bare_repo_for_the_sidecar_profile(self):
        calls = {}
        with mock.patch.object(cl, "collect", self._fake_collect(calls)):
            rc = cl.main(["--out", self.out])
        self.assertEqual(rc, 0)
        self.assertEqual(calls["source_url"], cl.DEFAULT_SOURCE_URL)
        self.assertFalse(calls["no_sidecar"])

    def test_binaries_dir_defaults_to_repo_target_release(self):
        calls = {}
        with mock.patch.object(cl, "collect", self._fake_collect(calls)):
            cl.main(["--repo", "/some/where", "--out", self.out])
        self.assertEqual(calls["repo"], "/some/where")
        self.assertEqual(calls["binaries_dir"], os.path.join("/some/where", "target", "release"))

    def test_no_sidecar_passes_no_sidecar_artifact_or_verdandi_to_collect(self):
        calls = {}
        with mock.patch.object(cl, "collect", self._fake_collect(calls)):
            rc = cl.main(["--no-sidecar", "--source-url", "https://example.invalid/asset.tar.gz",
                          "--skia-archive-path", "/some/skia-binaries.tar.gz", "--out", self.out])
        self.assertEqual(rc, 0)
        self.assertTrue(calls["no_sidecar"])
        self.assertIsNone(calls["sidecar_artifact"])
        self.assertIsNone(calls["verdandi"])
        self.assertEqual(calls["source_url"], "https://example.invalid/asset.tar.gz")
        self.assertEqual(calls["skia_archive_path"], "/some/skia-binaries.tar.gz")
        self.assertIsNone(calls["skia_archive_extract_dir"])
        self.assertIsNone(calls["skia_archive_sha256"])

    def test_skia_archive_extract_dir_and_sha256_are_passed_through_to_collect(self):
        """Review important 1/2 (the extract_dir plumbing) and minor 5 (the sha256 cross-check):
        both new inputs reach collect() unchanged, alongside the existing --skia-archive-path."""
        calls = {}
        with mock.patch.object(cl, "collect", self._fake_collect(calls)):
            rc = cl.main(["--no-sidecar", "--source-url", "https://example.invalid/asset.tar.gz",
                          "--skia-archive-path", "/some/skia-binaries.tar.gz",
                          "--skia-archive-extract-dir", "/some/scratch",
                          "--skia-sha256", "ab" * 32, "--out", self.out])
        self.assertEqual(rc, 0)
        self.assertEqual(calls["skia_archive_extract_dir"], "/some/scratch")
        self.assertEqual(calls["skia_archive_sha256"], "ab" * 32)

    def test_main_writes_source_notice_when_asked(self):
        calls = {}
        notice_path = os.path.join(self.dir, "SOURCE")
        fake_meta = {"packages": [{"name": "shell", "version": "1.0.0-rc.1", "id": "shell 1.0.0-rc.1 (path+file:///x)"}],
                     "workspace_members": ["shell 1.0.0-rc.1 (path+file:///x)"]}
        os.environ["NEOVIBE_BUILD_COMMIT"] = "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef"
        self.addCleanup(os.environ.pop, "NEOVIBE_BUILD_COMMIT", None)
        os.environ["NEOVIBE_BUILD_FORK_COMMIT"] = "f00d" * 10
        self.addCleanup(os.environ.pop, "NEOVIBE_BUILD_FORK_COMMIT", None)
        with mock.patch.object(cl, "collect", self._fake_collect(calls)), \
             mock.patch.object(cl, "load_metadata", lambda repo: fake_meta):
            rc = cl.main(["--out", self.out, "--source-notice", notice_path,
                          "--skia-archive", "skia-binaries-abc.tar.gz", "--skia-sha256", "ab" * 32,
                          "--source-url", "https://example.invalid/asset.tar.gz"])
        self.assertEqual(rc, 0)
        with open(notice_path, encoding="utf-8") as f:
            text = f.read()
        self.assertIn("deadbeefdeadbeefdeadbeefdeadbeefdeadbeef", text)
        self.assertIn("NEOVIBE_BUILD_FORK_COMMIT=" + "f00d" * 10, text)
        self.assertIn("skia-binaries-abc.tar.gz", text)
        self.assertIn("1.0.0-rc.1", cl.workspace_package_version(fake_meta, "shell"))

    def test_the_fork_commit_comes_from_neovide_s_own_checkout_never_an_enclosing_one(self):
        """With no NEOVIBE_BUILD_FORK_COMMIT, SOURCE's fork commit is `git rev-parse HEAD` in
        --repo's neovide/ -- and only when that directory is its own checkout: a plain neovide/
        inside a repository must fail rather than name the enclosing repository's HEAD."""
        fake_meta = {"packages": [{"name": "shell", "version": "1.0.0-rc.1", "id": "shell 1.0.0-rc.1 (path+file:///x)"}],
                     "workspace_members": ["shell 1.0.0-rc.1 (path+file:///x)"]}
        os.environ["NEOVIBE_BUILD_COMMIT"] = "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef"
        self.addCleanup(os.environ.pop, "NEOVIBE_BUILD_COMMIT", None)
        os.environ.pop("NEOVIBE_BUILD_FORK_COMMIT", None)
        git_env = {**os.environ, "GIT_AUTHOR_NAME": "f", "GIT_AUTHOR_EMAIL": "f@example.com",
                   "GIT_COMMITTER_NAME": "f", "GIT_COMMITTER_EMAIL": "f@example.com"}
        repo = os.path.join(self.dir, "repo")
        os.makedirs(os.path.join(repo, "neovide"))
        subprocess.run(["git", "init", "-q", repo], check=True, env=git_env)
        with open(os.path.join(repo, "neovide", "README"), "w") as f:
            f.write("plain directory\n")
        subprocess.run(["git", "-C", repo, "add", "-A"], check=True, env=git_env)
        subprocess.run(["git", "-C", repo, "commit", "-q", "-m", "outer"], check=True, env=git_env)

        def main(name):
            path = os.path.join(self.dir, name)
            with mock.patch.object(cl, "collect", self._fake_collect({})), \
                 mock.patch.object(cl, "load_metadata", lambda repo: fake_meta):
                rc = cl.main(["--repo", repo, "--out", self.out, "--source-notice", path,
                              "--skia-archive", "skia-binaries-abc.tar.gz", "--skia-sha256", "ab" * 32,
                              "--source-url", "https://example.invalid/asset.tar.gz"])
            return rc, path

        rc, _ = main("SOURCE-plain")
        self.assertEqual(rc, 1)

        subprocess.run(["git", "init", "-q", os.path.join(repo, "neovide")], check=True, env=git_env)
        subprocess.run(["git", "-C", os.path.join(repo, "neovide"), "add", "-A"], check=True, env=git_env)
        subprocess.run(["git", "-C", os.path.join(repo, "neovide"), "commit", "-q", "-m", "fork"],
                       check=True, env=git_env)
        head = subprocess.run(["git", "-C", os.path.join(repo, "neovide"), "rev-parse", "HEAD"],
                              check=True, capture_output=True, text=True).stdout.strip()
        rc, path = main("SOURCE-checkout")
        self.assertEqual(rc, 0)
        with open(path, encoding="utf-8") as f:
            self.assertIn(f"NEOVIBE_BUILD_FORK_COMMIT={head}", f.read())


if __name__ == "__main__":
    unittest.main()
