"""The repository's own LICENSE and NOTICE.

License detectors compare LICENSE against the MIT template and give up when anything else is in the file, so
LICENSE holds that text alone and the acknowledgement of Neovide lives in NOTICE. Every place a release carries
LICENSE carries NOTICE too: test_nfpm_profiles.py, test_aur_pkgbuilds.py, test_release_layout.py and the
installer tests hold those.

    python3 -m pytest packaging/tests/test_root_licence.py -q
"""
import fnmatch
import os
import unittest

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
MANIFEST = os.path.join(ROOT, "publish", "manifest.txt")

MIT_TEMPLATE = """MIT License

Copyright (c) 2026 Hunter Grey

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
"""


def _read(name):
    with open(os.path.join(ROOT, name), encoding="utf-8", newline="") as f:
        return f.read()


class RootLicence(unittest.TestCase):
    def test_licence_is_the_mit_text_alone(self):
        self.assertEqual(_read("LICENSE"), MIT_TEMPLATE)

    def test_notice_acknowledges_neovide_and_its_licence(self):
        text = _read("NOTICE")
        self.assertIn("https://github.com/neovide/neovide", text)
        self.assertIn("Copyright (c) 2023 Neovide Contributors", text)
        self.assertIn("neovide-editor/", text)

    @unittest.skipUnless(os.path.exists(MANIFEST), "publish/ does not ship in the public tree")
    def test_the_export_carries_notice(self):
        with open(MANIFEST, encoding="utf-8") as f:
            rules = [line.split() for line in f if line.strip() and not line.startswith(("#", "option"))]
        verdict = next(kind for kind, glob, *_ in rules if fnmatch.fnmatchcase("NOTICE", glob))
        self.assertEqual(verdict, "include")


if __name__ == "__main__":
    unittest.main()
