"""Cargo.lock pairs bindgen with syn 2 by hand. skia-bindings needs bindgen 0.73, built with prettyplease, which Cargo
keeps at 0.2 (prost-build needs it) and which speaks syn 2; given the choice Cargo resolves bindgen's own syn to 3,
which does not compile. bindgen is a build dependency of skia-bindings, so with syn 3 back cargo fails before any Rust
test can run: this reads the lock with Python instead, and says how to set the entry again.
"""
import os
import re
import tomllib
import unittest

REPO = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
RULE = "`bindgen` is paired with syn 2 in `Cargo.lock` by hand"


def bindgen_syn_problems(lock_text):
    """Problems with every bindgen package's syn in a Cargo.lock text; empty when each depends on one syn 2.x."""
    lock = tomllib.loads(lock_text)
    packages = lock.get("package", [])
    syn_versions = [p["version"] for p in packages if p["name"] == "syn"]
    bindgens = [p for p in packages if p["name"] == "bindgen"]
    if not bindgens:
        return ["Cargo.lock has no bindgen package"]
    problems = []
    for b in bindgens:
        syns = []
        for dep in b.get("dependencies", []):
            name, _, version = dep.partition(" ")
            if name != "syn":
                continue
            if version:
                syns.append(version)
            elif len(syn_versions) == 1:
                # A lock with one syn names it bare.
                syns.append(syn_versions[0])
            else:
                syns.append("?")
        if len(syns) != 1 or not re.match(r"^2\.", syns[0]):
            problems.append(
                f"bindgen {b['version']} depends on syn {syns} in Cargo.lock; it must name the syn 2.x the lock "
                f"already has, set by hand: {RULE}"
            )
    return problems


class BindgenSynPairing(unittest.TestCase):
    def test_the_lock_pairs_bindgen_with_syn_2(self):
        with open(os.path.join(REPO, "Cargo.lock"), encoding="utf-8") as f:
            self.assertEqual(bindgen_syn_problems(f.read()), [])

    LOCK = (
        'version = 4\n\n[[package]]\nname = "bindgen"\nversion = "0.73.2"\ndependencies = [\n "prettyplease",\n'
        ' "syn 3.0.5",\n]\n\n[[package]]\nname = "syn"\nversion = "2.0.119"\n\n[[package]]\nname = "syn"\n'
        'version = "3.0.5"\n'
    )

    def test_a_lock_pairing_bindgen_with_syn_3_is_refused_naming_the_rule(self):
        (problem,) = bindgen_syn_problems(self.LOCK)
        self.assertIn("3.0.5", problem)
        self.assertIn(RULE, problem)
        self.assertEqual(bindgen_syn_problems(self.LOCK.replace('"syn 3.0.5",', '"syn 2.0.119",')), [])

    def test_a_bare_syn_is_resolved_against_the_locks_only_syn(self):
        one_syn_3 = (
            'version = 4\n\n[[package]]\nname = "bindgen"\nversion = "0.73.2"\ndependencies = [\n "syn",\n]\n\n'
            '[[package]]\nname = "syn"\nversion = "3.0.5"\n'
        )
        (problem,) = bindgen_syn_problems(one_syn_3)
        self.assertIn("3.0.5", problem)
        self.assertEqual(bindgen_syn_problems(one_syn_3.replace('version = "3.0.5"', 'version = "2.0.119"')), [])


if __name__ == "__main__":
    unittest.main()
