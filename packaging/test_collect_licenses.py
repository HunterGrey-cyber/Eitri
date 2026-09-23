"""Unit tests for collect-licenses.py's handling of workspace members.

    python3 packaging/test_collect_licenses.py

(`python3 -m unittest packaging/test_collect_licenses.py` fails from the repository root:
`ModuleNotFoundError: No module named 'packaging.test_collect_licenses'`, because the `packaging`
directory name collides with the installed PyPI `packaging` package -- engine review 2026-09-23,
minor 6.)

The collector's full run needs release binaries and the sidecar (publish.sh); these need neither.
"""

import importlib.util
import os
import unittest

_HERE = os.path.dirname(os.path.abspath(__file__))
_spec = importlib.util.spec_from_file_location("collect_licenses", os.path.join(_HERE, "collect-licenses.py"))
cl = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(cl)


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


if __name__ == "__main__":
    unittest.main()
