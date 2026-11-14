"""The examples the public guide prints: the `eitri agents` scripts in examples/agents/, run against the recorded rows
in examples/agents/fixtures/ (a stand-in `eitri` prints them; stand-ins for notify-send and curl record their
arguments, so no notification is ever shown or sent), and the working-environment recipes' Chinese twins.

The recorded rows themselves are pinned to the real wire by `the_recipe_fixtures_are_lines_eitri_agents_prints`
(core/tests/agents_wire.rs); the recipes' init.lua files are run by `examples_environments` (core/tests/)."""

import hashlib
import os
import re
import shutil
import subprocess
import tempfile
import unittest

_HERE = os.path.dirname(os.path.abspath(__file__))
_REPO_ROOT = os.path.dirname(os.path.dirname(_HERE))
_AGENTS = os.path.join(_REPO_ROOT, "examples", "agents")
_ENVIRONMENTS = os.path.join(_REPO_ROOT, "examples", "environments")


def _guide_dir():
    # The private tree keeps the public docs under publish/files/; the export copies them over the public tree's root.
    if os.path.lexists(os.path.join(_REPO_ROOT, "publish")):
        return os.path.join(_REPO_ROOT, "publish", "files", "docs", "guide")
    return os.path.join(_REPO_ROOT, "docs", "guide")


def _read(path):
    with open(path, encoding="utf-8") as f:
        return f.read()


@unittest.skipUnless(shutil.which("jq"), "the scripts need jq")
class AgentsScriptsTests(unittest.TestCase):
    def setUp(self):
        self.bin = tempfile.mkdtemp(prefix="eitri-examples-")
        self.addCleanup(shutil.rmtree, self.bin, ignore_errors=True)
        self.calls = os.path.join(self.bin, "calls")
        fixtures = os.path.join(_AGENTS, "fixtures")
        self._stub("eitri", f"""#!/bin/sh
case "$*" in
  "agents --json") cat '{fixtures}/view.json' ;;
  "agents --watch") cat '{fixtures}/watch.jsonl' ;;
  *) echo "unexpected eitri $*" >&2; exit 64 ;;
esac
""")
        # Each call is one line: the program, then its arguments separated by a tab.
        for name in ("notify-send", "curl"):
            self._stub(name, f"""#!/bin/sh
{{ printf '%s' {name}; for a in "$@"; do printf '\\t%s' "$a"; done; printf '\\n'; }} >> '{self.calls}'
""")

    def _stub(self, name, text):
        path = os.path.join(self.bin, name)
        with open(path, "w", encoding="utf-8") as f:
            f.write(text)
        os.chmod(path, 0o755)

    def _run(self, script, **env):
        environment = {
            "PATH": os.pathsep.join([self.bin, _AGENTS, os.environ.get("PATH", "/usr/bin:/bin")]),
            "HOME": self.bin,
            "LC_ALL": "C.UTF-8",
        }
        environment.update(env)
        return subprocess.run([os.path.join(_AGENTS, script)], env=environment, capture_output=True, text=True,
                              timeout=30)

    def _calls(self):
        if not os.path.exists(self.calls):
            return []
        return [line.split("\t") for line in _read(self.calls).splitlines()]

    def test_the_tmux_counter_counts_the_waiting_rows(self):
        out = self._run("eitri-waiting")
        self.assertEqual((out.returncode, out.stdout, out.stderr), (0, "⚑ 2\n", ""))

    def test_the_waybar_module_writes_one_line_per_watch_line(self):
        out = self._run("eitri-waybar")
        self.assertEqual(out.returncode, 0, out.stderr)
        lines = out.stdout.splitlines()
        watched = len(_read(os.path.join(_AGENTS, "fixtures", "watch.jsonl")).splitlines())
        self.assertEqual(len(lines), watched)
        self.assertEqual(lines[0], '{"text":"⚑ 2","class":"waiting",'
                                   '"tooltip":"eitri 1·1 close question\\neitri 2·3 release notes"}')

    def test_on_wait_prints_each_new_wait_once(self):
        out = self._run("eitri-on-wait")
        self.assertEqual(out.returncode, 0, out.stderr)
        # Already waiting at the start: 1·1 and 2·3, not printed. 1·2 starts waiting (its second card is no new
        # wait), 1·1 is answered and waits again, then moves to window 2 while it waits (a new id, then the old id's
        # departure, then a change: no new wait), 2·4 has no key and is named by its id, and the reset brings 2·5.
        self.assertEqual(out.stdout.splitlines(),
                         ["eitri 1·2 card", "eitri 1·1 card", "eitri 2·4 trust", "eitri 2·5 question"])

    def test_notify_sends_one_desktop_notification_per_new_wait(self):
        out = self._run("eitri-notify")
        self.assertEqual(out.returncode, 0, out.stderr)
        self.assertEqual(self._calls(), [
            ["notify-send", "--app-name=Eitri", "An agent waits for you", line]
            for line in ("eitri 1·2 card", "eitri 1·1 card", "eitri 2·4 trust", "eitri 2·5 question")
        ])

    def test_ntfy_posts_to_the_topic_it_is_given(self):
        url = "https://ntfy.example.com/my-agents"
        out = self._run("eitri-ntfy", NTFY_URL=url)
        self.assertEqual(out.returncode, 0, out.stderr)
        self.assertEqual(self._calls(), [
            ["curl", "-fsS", "-H", "Title: An agent waits for you", "-d", line, url]
            for line in ("eitri 1·2 card", "eitri 1·1 card", "eitri 2·4 trust", "eitri 2·5 question")
        ])

    def test_ntfy_without_a_topic_refuses_and_posts_nothing(self):
        out = self._run("eitri-ntfy")
        self.assertNotEqual(out.returncode, 0)
        self.assertIn("NTFY_URL", out.stderr)
        self.assertEqual(self._calls(), [])


class AgentsScriptsInTheGuideTests(unittest.TestCase):
    def test_the_guide_prints_each_script_word_for_word(self):
        guide = _read(os.path.join(_guide_dir(), "many-agents.md"))
        for name in ("eitri-waiting", "eitri-waybar", "eitri-on-wait", "eitri-notify", "eitri-ntfy"):
            self.assertIn(_read(os.path.join(_AGENTS, name)).rstrip("\n"), guide, name)


class EnvironmentTwinsTests(unittest.TestCase):
    """Each recipe's README.zh-CN.md follows the README.md beside it, as release_check.py's `twins` check reads a
    twin: line 1 the language switch, line 2 the translated-from marker with the English file's sha256."""

    def test_every_recipe_has_a_fresh_chinese_twin(self):
        recipes = sorted(d for d in os.listdir(_ENVIRONMENTS) if os.path.isdir(os.path.join(_ENVIRONMENTS, d)))
        self.assertGreaterEqual(len(recipes), 4)
        for name in recipes:
            english = os.path.join(_ENVIRONMENTS, name, "README.md")
            twin = os.path.join(_ENVIRONMENTS, name, "README.zh-CN.md")
            self.assertTrue(os.path.isfile(os.path.join(_ENVIRONMENTS, name, "init.lua")), name)
            with open(english, "rb") as f:
                digest = hashlib.sha256(f.read()).hexdigest()
            self.assertEqual(_read(english).splitlines()[0], "English | [简体中文](README.zh-CN.md)", name)
            first, second = _read(twin).splitlines()[:2]
            self.assertEqual(first, "[English](README.md) | 简体中文", name)
            marker = re.fullmatch(r"<!-- translated-from: README\.md sha256=([0-9a-f]{64}) -->", second)
            self.assertIsNotNone(marker, f"{name}: {second}")
            self.assertEqual(marker.group(1), digest, f"{name}: the twin is stale")


class RecipeLinksTests(unittest.TestCase):
    """Every relative link in a recipe's README (and its twin) lands on a file of the public tree, where examples/ and
    docs/guide/ are both at the root; in this tree docs/guide/ is under publish/files/."""

    def test_every_relative_link_in_the_recipes_resolves(self):
        checked = 0
        for name in sorted(os.listdir(_ENVIRONMENTS)):
            for readme in ("README.md", "README.zh-CN.md"):
                path = os.path.join(_ENVIRONMENTS, name, readme)
                for target in re.findall(r"\]\(([^)\s]+)\)", _read(path)):
                    if re.match(r"^[a-z][a-z0-9+.-]*:|^#", target):
                        continue
                    rel = os.path.normpath(os.path.join("examples", "environments", name, target.split("#", 1)[0]))
                    self.assertFalse(rel.startswith(".."), f"{name}/{readme}: {target}")
                    here = os.path.join(_REPO_ROOT, rel)
                    guide = os.path.join(_guide_dir(), os.path.relpath(rel, os.path.join("docs", "guide")))
                    in_guide = rel.startswith(os.path.join("docs", "guide") + os.sep)
                    self.assertTrue(os.path.isfile(guide if in_guide else here), f"{name}/{readme}: {target} -> {rel}")
                    checked += 1
        self.assertGreater(checked, 10)


if __name__ == "__main__":
    unittest.main()
