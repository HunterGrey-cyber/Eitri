"""Unit tests holding the two nfpm profiles -- packaging/nfpm.yaml (private mirror) and
packaging/nfpm-public.yaml (public, GitHub Releases) -- to spec sec 8's invariants (Task 8).

    python3 packaging/test_nfpm_profiles.py
    python3 -m pytest packaging

Parses both YAML files with PyYAML when it is importable (it is, on every machine this has been
run on), falling back to a hand-rolled parser covering exactly the subset of YAML these two files
use (block mappings, block sequences, list-of-maps, one level of nesting, and a `|` literal block
for `description`) when it is not. `AgreesWithPyYAML` below feeds both real files through both
parsers and asserts they produce the same structure whenever PyYAML is present, so the fallback
path is exercised even on a machine where it is dead code.

These tests read the two YAML files as data; they never invoke `nfpm` (no binary needed to build a
`.deb`/`.rpm`, and several `contents` entries here point at build outputs -- `./target/release/*`,
`./dist/*` -- that do not exist until `release.sh`/`publish.sh` run).
"""

import os
import re
import unittest

try:
    import yaml as _pyyaml
except ImportError:  # pragma: no cover -- not observed on any machine this has run on
    _pyyaml = None

_HERE = os.path.dirname(os.path.abspath(__file__))
PUBLIC_PATH = os.path.join(_HERE, "nfpm-public.yaml")
PRIVATE_PATH = os.path.join(_HERE, "nfpm.yaml")


# --- a minimal YAML-subset parser, for a host with no PyYAML -------------------------------------
#
# Not a general YAML parser: it understands exactly what packaging/nfpm*.yaml use -- 2-space-indented
# block mappings and block sequences, a sequence of one-line-per-item maps (`contents:`'s shape),
# and a `|` literal block scalar (`description:`). Quoted scalars, flow style (`{a: b}`, `[a, b]`),
# anchors and multi-document files are not needed here and are not handled.
def _indent_of(line):
    return len(line) - len(line.lstrip(" "))


def _parse_scalar(text):
    """A bare scalar, YAML-1.1-ish: octal `0NNN` and decimal integers become `int` (PyYAML does
    this too -- `mode: 0755` is the int 493, not the string "0755" -- so `file_info.mode`
    comparisons agree between the two parsers), an inline ` #comment` is dropped, and a quoted
    string is unwrapped. Everything else stays a string."""
    s = text.strip()
    if " #" in s:
        s = s.split(" #", 1)[0].rstrip()
    if len(s) >= 2 and s[0] == s[-1] and s[0] in "\"'":
        return s[1:-1]
    if re.fullmatch(r"0[0-7]+", s):
        return int(s, 8)
    if re.fullmatch(r"[+-]?[0-9]+", s):
        return int(s)
    return s


class _MinimalYamlParser:
    def __init__(self, text):
        self.lines = text.split("\n")

    def _next_significant(self, i):
        while i < len(self.lines):
            stripped = self.lines[i].strip()
            if stripped == "" or stripped.startswith("#"):
                i += 1
                continue
            return i
        return i

    def parse_document(self):
        value, i = self._parse_block(0, 0)
        return value if value is not None else {}

    def _parse_block(self, i, min_indent):
        i = self._next_significant(i)
        if i >= len(self.lines):
            return None, i
        indent = _indent_of(self.lines[i])
        if indent < min_indent:
            return None, i
        if self.lines[i].strip().startswith("- "):
            return self._parse_seq(i, indent)
        return self._parse_map(i, indent)

    def _parse_seq(self, i, indent):
        result = []
        while True:
            i = self._next_significant(i)
            if i >= len(self.lines):
                break
            line = self.lines[i]
            if _indent_of(line) != indent or not line.strip().startswith("- "):
                break
            item_text = line.strip()[2:]
            # Decide map-item vs scalar-item on the text with any inline comment already
            # stripped: `- noto-fonts # NotoSansSymbols2: the glyph` contains a `:` too, but only
            # inside its trailing comment, not as this item's own key.
            item_sans_comment = item_text.split(" #", 1)[0].rstrip() if " #" in item_text else item_text
            if re.match(r"^[A-Za-z0-9_.-]+:(\s|$)", item_sans_comment):
                # A list item that is itself a one-line-started map (`contents:`'s shape): splice
                # a synthetic line back into place at the column the item's own keys sit at (two
                # past the dash), then parse a map starting there.
                key_col = indent + 2
                self.lines[i] = (" " * key_col) + item_text
                item, i = self._parse_map(i, key_col)
                result.append(item)
            else:
                result.append(_parse_scalar(item_text))
                i += 1
        return result, i

    def _parse_map(self, i, indent):
        result = {}
        while True:
            i = self._next_significant(i)
            if i >= len(self.lines):
                break
            line = self.lines[i]
            if _indent_of(line) != indent:
                break
            content = line.strip()
            if content.startswith("- ") or ":" not in content:
                break
            key, _, rest = content.partition(":")
            key = key.strip()
            rest = rest.strip()
            i += 1
            if rest == "|":
                block, i = self._parse_literal_block(i, indent)
                result[key] = block
            elif rest == "" or rest.startswith("#"):
                child, i = self._parse_block(i, indent + 1)
                result[key] = child if child is not None else {}
            else:
                result[key] = _parse_scalar(rest)
        return result, i

    def _parse_literal_block(self, i, parent_indent):
        collected = []
        block_indent = None
        while i < len(self.lines):
            raw = self.lines[i]
            if raw.strip() == "":
                collected.append("")
                i += 1
                continue
            ind = _indent_of(raw)
            if block_indent is None:
                if ind <= parent_indent:
                    break
                block_indent = ind
            if ind < block_indent:
                break
            collected.append(raw[block_indent:])
            i += 1
        while collected and collected[-1] == "":
            collected.pop()
        return "\n".join(collected) + "\n", i


def _minimal_yaml_load(text):
    return _MinimalYamlParser(text).parse_document()


def _load_yaml(path):
    with open(path, encoding="utf-8") as f:
        text = f.read()
    if _pyyaml is not None:
        return _pyyaml.safe_load(text)
    return _minimal_yaml_load(text)


def _read(path):
    with open(path, encoding="utf-8") as f:
        return f.read()


def _contents_by_dst(doc):
    return {entry["dst"]: entry for entry in doc.get("contents", [])}


# --- the four binaries every release ships, D16 (no agent-hook anywhere) ------------------------

RELEASE_BINARY_DSTS = {
    "/usr/lib/neovibe/shell",
    "/usr/lib/neovibe/neovibe-supervisor",
    "/usr/lib/neovibe/neovibe-tmux-shim",
    "/usr/lib/neovibe/neovibe-claude-handoff",
}

# Contents entries expected ONLY in the private (mirror) profile: the sidecar itself and the
# rev file recording which Verdandi revision it was built from (spec sec 8's "extra files" row).
PRIVATE_ONLY_DSTS = {
    "/usr/lib/neovibe/verdandi-claude-sidecar",
    "/usr/lib/neovibe/verdandi-claude-sidecar.rev",
}

# Contents entries expected ONLY in the public profile: the installer, staged as `neovibe setup`
# (spec sec 8's "extra files" row -- the private profile has no installer entry of its own).
PUBLIC_ONLY_DSTS = {
    "/usr/lib/neovibe/neovibe-setup",
}


class BothProfilesLoad(unittest.TestCase):
    def test_both_files_parse_to_mappings(self):
        public = _load_yaml(PUBLIC_PATH)
        private = _load_yaml(PRIVATE_PATH)
        self.assertIsInstance(public, dict)
        self.assertIsInstance(private, dict)
        self.assertIn("contents", public)
        self.assertIn("contents", private)


class AgreesWithPyYAML(unittest.TestCase):
    """The fallback parser is dead code on any machine with PyYAML installed -- which is every
    machine this has been run on. Prove it agrees with the real parser on both real files, so a
    host without PyYAML is not the first place a parser bug would be found."""

    @unittest.skipIf(_pyyaml is None, "PyYAML is not importable here; nothing to compare against")
    def test_minimal_parser_agrees_with_pyyaml_on_both_files(self):
        for path in (PUBLIC_PATH, PRIVATE_PATH):
            text = _read(path)
            self.assertEqual(
                _minimal_yaml_load(text),
                _pyyaml.safe_load(text),
                f"the fallback parser disagrees with PyYAML on {path}",
            )


class PublicProfileNeverCarriesTheSDK(unittest.TestCase):
    """Spec sec 8's "no sidecar in any public artifact" (Review Focus 5): checked here on the
    profile itself, so a future edit that adds the sidecar back to the public YAML fails a fast
    unit test rather than waiting for a release build's own content-and-leak check (spec sec 4.2
    step 9) to catch it."""

    def test_verdandi_claude_sidecar_is_named_nowhere_in_the_public_profile(self):
        text = _read(PUBLIC_PATH)
        self.assertNotIn("verdandi-claude-sidecar", text)

    def test_public_contents_has_no_sidecar_or_rev_entry(self):
        public_dsts = set(_contents_by_dst(_load_yaml(PUBLIC_PATH)))
        self.assertFalse(public_dsts & PRIVATE_ONLY_DSTS, public_dsts & PRIVATE_ONLY_DSTS)

    def test_no_neovim_relation_in_any_deb_depends(self):
        public = _load_yaml(PUBLIC_PATH)
        self.assertNotIn("neovim", public.get("depends") or [])
        deb = public.get("overrides", {}).get("deb", {})
        self.assertNotIn("neovim", deb.get("depends") or [])
        self.assertNotIn("neovim", deb.get("recommends") or [])

    def test_rpm_recommends_neovim_but_does_not_depend_on_it(self):
        rpm = _load_yaml(PUBLIC_PATH).get("overrides", {}).get("rpm", {})
        self.assertNotIn("neovim", rpm.get("depends") or [])
        self.assertIn("neovim", rpm.get("recommends") or [])


class NeitherProfileNamesAgentHook(unittest.TestCase):
    """D16: no release contains the legacy backend, the private mirror's one included, so
    `agent-hook` (legacy's PreToolUse relay binary) ships in neither profile."""

    def test_agent_hook_is_absent_from_both_files(self):
        # On the parsed `contents` entries, not the raw text: both files' own comments legitimately
        # explain, in prose, that agent-hook is absent and why (D16) -- a raw substring check would
        # fail on that very sentence.
        for path in (PUBLIC_PATH, PRIVATE_PATH):
            for entry in _load_yaml(path)["contents"]:
                self.assertNotIn("agent-hook", entry["src"], (path, entry))
                self.assertNotIn("agent-hook", entry["dst"], (path, entry))

    def test_the_four_binaries_are_at_the_same_dsts_in_both_profiles(self):
        public_dsts = set(_contents_by_dst(_load_yaml(PUBLIC_PATH)))
        private_dsts = set(_contents_by_dst(_load_yaml(PRIVATE_PATH)))
        self.assertTrue(RELEASE_BINARY_DSTS <= public_dsts, public_dsts)
        self.assertTrue(RELEASE_BINARY_DSTS <= private_dsts, private_dsts)


class ReleaseAndSourceAreInBoth(unittest.TestCase):
    def test_release_is_in_both_at_mode_0644(self):
        for path in (PUBLIC_PATH, PRIVATE_PATH):
            entry = _contents_by_dst(_load_yaml(path))["/usr/lib/neovibe/RELEASE"]
            self.assertEqual(entry.get("file_info", {}).get("mode"), 0o644, (path, entry))

    def test_source_is_in_both(self):
        for path in (PUBLIC_PATH, PRIVATE_PATH):
            self.assertIn("/usr/share/licenses/neovibe/SOURCE", _contents_by_dst(_load_yaml(path)))


class TheTwoProfilesDifferOnlyInTheAllowedKeys(unittest.TestCase):
    """Spec sec 8: "A test ... parses both files and fails unless they differ only in: the sidecar
    entry and its `.rev`; `neovibe-setup`; the dependency blocks; the licence and description
    fields; maintainer/homepage." `version_schema` is a `nfpm-public.yaml`-only addition from this
    task's own brief ("plus whatever Task 1 found about prerelease ordering") that Task 1 (not yet
    landed on this branch) may still revisit for the private profile; until then it is an allowed
    top-level difference too, same as the fields spec sec 8 already names."""

    ALLOWED_DIFFERING_TOP_LEVEL_KEYS = {
        "maintainer",
        "homepage",
        "license",
        "description",
        "depends",
        "recommends",
        "overrides",
        "version_schema",
    }

    def test_top_level_keys_outside_the_allowed_set_are_identical(self):
        public = _load_yaml(PUBLIC_PATH)
        private = _load_yaml(PRIVATE_PATH)
        checked_keys = (set(public) | set(private)) - self.ALLOWED_DIFFERING_TOP_LEVEL_KEYS - {"contents"}
        for key in checked_keys:
            self.assertEqual(public.get(key), private.get(key), f"top-level key {key!r} differs")

    def test_contents_differ_only_in_the_sidecar_rev_and_setup_entries(self):
        public_by_dst = _contents_by_dst(_load_yaml(PUBLIC_PATH))
        private_by_dst = _contents_by_dst(_load_yaml(PRIVATE_PATH))
        self.assertEqual(set(private_by_dst) - set(public_by_dst), PRIVATE_ONLY_DSTS)
        self.assertEqual(set(public_by_dst) - set(private_by_dst), PUBLIC_ONLY_DSTS)
        for dst in set(public_by_dst) & set(private_by_dst):
            self.assertEqual(public_by_dst[dst], private_by_dst[dst], f"contents entry {dst!r} differs")

    def test_private_licence_is_the_public_one_plus_the_sdk_licenseref(self):
        public_license = _load_yaml(PUBLIC_PATH)["license"]
        private_license = _load_yaml(PRIVATE_PATH)["license"]
        self.assertEqual(private_license, f"{public_license} AND LicenseRef-Anthropic-Claude-Agent-SDK")

    def test_public_licence_is_the_spdx_expression_spec_names(self):
        self.assertEqual(_load_yaml(PUBLIC_PATH)["license"], "MIT AND Apache-2.0 AND LGPL-3.0-only AND MPL-2.0")

    def test_public_maintainer_and_homepage_are_the_public_identity(self):
        public = _load_yaml(PUBLIC_PATH)
        self.assertEqual(
            public["maintainer"],
            "Hunter Grey <71165939+HunterGrey-cyber@users.noreply.github.com>",
        )
        self.assertEqual(public["homepage"], "https://github.com/HunterGrey-cyber/neovibe")

    def test_both_descriptions_end_with_the_source_url_placeholder(self):
        for path in (PUBLIC_PATH, PRIVATE_PATH):
            description = _load_yaml(path)["description"]
            last_line = [line for line in description.splitlines() if line.strip()][-1]
            self.assertIn("${NEOVIBE_SOURCE_URL}", last_line, (path, last_line))


# leaks-claude-1 (packaging half, review2 Task 4): a package Description is read by `apt show`/
# `dnf info`/`pacman -Si`, by anyone, not just this project's own agents -- an internal ruling id
# (D11, R07/S2, I2/I7) or an unpublished spec filename means nothing to a stranger reading it.
# `rpm -qip`/`.deb control` showed nfpm-public.yaml's description citing "I2/I7, spec
# 2026-09-27-v1-dist-design.md sec 5, sec 8)".
RULING_CITATION_PATTERN = re.compile(
    r"spec §|spec \d{4}-\d{2}-\d{2}-[\w-]*\.md|\b[DRI]\d{1,3}(?:/[A-Z]?\d{0,3})?\b"
)


class DescriptionsCiteNoInternalRulingOrSpec(unittest.TestCase):
    def test_neither_description_cites_a_ruling_id_or_spec_file(self):
        for path in (PUBLIC_PATH, PRIVATE_PATH):
            description = _load_yaml(path)["description"]
            match = RULING_CITATION_PATTERN.search(description)
            self.assertIsNone(match, (path, match and match.group(0), description))


# --- the AppArmor profile, .deb only (docs/superpowers/plans/2026-09-28-v1-dist-ubuntu-userns.md) ---

APPARMOR_DST = "/etc/apparmor.d/neovibe"
APPARMOR_SRC = "./packaging/apparmor/neovibe"


class TheDebCarriesTheAppArmorProfile(unittest.TestCase):
    """Ubuntu 23.10+ lets WebKit's bwrap sandbox start only under an AppArmor profile granting
    `userns`. Both profiles ship packaging/apparmor/neovibe to the .deb alone, as a conffile, and
    nothing loads it (no maintainer script: spec sec 8's rule, REL-2)."""

    def test_both_profiles_ship_it_to_the_deb_only_as_a_conffile(self):
        for path in (PUBLIC_PATH, PRIVATE_PATH):
            entry = _contents_by_dst(_load_yaml(path)).get(APPARMOR_DST)
            self.assertIsNotNone(entry, path)
            self.assertEqual(entry["src"], APPARMOR_SRC, path)
            self.assertEqual(entry.get("packager"), "deb", path)
            self.assertEqual(entry.get("type"), "config|noreplace", path)
            self.assertEqual(entry.get("file_info", {}).get("mode"), 0o644, path)

    def test_neither_profile_has_a_maintainer_script(self):
        for path in (PUBLIC_PATH, PRIVATE_PATH):
            doc = _load_yaml(path)
            self.assertNotIn("scripts", doc, path)
            for packager in ("deb", "rpm", "archlinux"):
                self.assertNotIn("scripts", (doc.get("overrides") or {}).get(packager) or {}, (path, packager))


def _nfpm_tools_missing():
    import shutil

    return [t for t in ("nfpm", "dpkg-deb", "rpm", "bsdtar") if shutil.which(t) is None]


@unittest.skipIf(_nfpm_tools_missing(), f"needs nfpm, dpkg-deb, rpm and bsdtar: missing {_nfpm_tools_missing()}")
class RealPackagesCarryTheProfileWhereTheySay(unittest.TestCase):
    """Built for real with this host's nfpm from both profiles, over a stand-in for every other
    file and the real packaging/apparmor/neovibe, then read back with the tools a user has: the
    .deb carries the profile, byte for byte, listed as a conffile, with no maintainer script; the
    .rpm (public) and the pacman package (private) carry none."""

    @classmethod
    def setUpClass(cls):
        import shutil
        import subprocess
        import tempfile

        root = os.path.join(os.path.expanduser("~"), ".cache", "neovibe-test-nfpm-profiles")
        os.makedirs(root, exist_ok=True)
        cls.work = tempfile.mkdtemp(dir=root)
        cls.addClassCleanup(shutil.rmtree, cls.work, ignore_errors=True)
        cls.sh = staticmethod(lambda args, **kw: subprocess.run(args, check=True, capture_output=True, **kw))
        cls.built = {}
        for label, path, packagers in (
            ("public", PUBLIC_PATH, ("deb", "rpm")),
            ("private", PRIVATE_PATH, ("deb", "archlinux")),
        ):
            stage = os.path.join(cls.work, label)
            for entry in _load_yaml(path)["contents"]:
                src = os.path.join(stage, entry["src"])
                os.makedirs(os.path.dirname(src), exist_ok=True)
                if entry["src"] == APPARMOR_SRC:
                    shutil.copyfile(os.path.join(_HERE, "apparmor", "neovibe"), src)
                else:
                    with open(src, "w") as f:
                        f.write(f"stand-in for {entry['src']}\n")
            env = {**os.environ, "VERSION": "1.0.0-rc.1", "NEOVIBE_SOURCE_URL": "https://example.com/source"}
            for packager in packagers:
                out = os.path.join(cls.work, f"{label}.{packager}")
                cls.sh(["nfpm", "pkg", "--config", path, "--packager", packager, "--target", out],
                        cwd=stage, env=env)
                cls.built[(label, packager)] = out

    def _deb(self, label):
        out = self.built[(label, "deb")]
        x = out + ".x"
        self.sh(["dpkg-deb", "-x", out, x])
        control = out + ".control"
        self.sh(["dpkg-deb", "-e", out, control])
        return x, control

    def test_the_debs_carry_the_profile_as_a_conffile_and_no_maintainer_script(self):
        with open(os.path.join(_HERE, "apparmor", "neovibe"), "rb") as f:
            shipped = f.read()
        for label in ("public", "private"):
            x, control = self._deb(label)
            with open(os.path.join(x, "etc", "apparmor.d", "neovibe"), "rb") as f:
                self.assertEqual(f.read(), shipped, label)
            with open(os.path.join(control, "conffiles")) as f:
                self.assertIn(APPARMOR_DST, f.read().split(), label)
            for script in ("preinst", "postinst", "prerm", "postrm", "config", "triggers"):
                self.assertFalse(os.path.exists(os.path.join(control, script)), (label, script))

    def test_the_rpm_and_the_pacman_package_carry_none(self):
        rpm_files = self.sh(["rpm", "-qlp", self.built[("public", "rpm")]], text=True).stdout.split()
        self.assertNotIn(APPARMOR_DST, rpm_files)
        self.assertIn("/usr/lib/neovibe/shell", rpm_files)
        pacman_files = self.sh(["bsdtar", "-tf", self.built[("private", "archlinux")]], text=True).stdout.split()
        self.assertNotIn(APPARMOR_DST.lstrip("/"), pacman_files)
        self.assertIn("usr/lib/neovibe/shell", pacman_files)


if __name__ == "__main__":
    unittest.main()
