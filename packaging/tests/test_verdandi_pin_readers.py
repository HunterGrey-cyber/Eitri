"""Every script that reads the Verdandi pin reads it where it lives: the root Cargo.toml's one
[workspace.dependencies] line, which agent and server take with `workspace = true`, and agent/Cargo.toml in a tree
from before the pin moved there (an older release's source, an older public tree). Each reader's own shell function
runs here against both shapes."""

import pathlib
import re
import subprocess

ROOT = pathlib.Path(__file__).resolve().parents[2]
REV = "c" * 40
OLD = "d" * 40
URL = "https://github.com/HunterGrey-cyber/verdandi.git"


def function(path, name):
    """The text of shell function `name` in `path`, from its `name() {` line to the first `}` alone on a line."""
    text = (ROOT / path).read_text()
    found = re.search(rf"^{name}\(\) \{{\n.*?^\}}\n", text, re.S | re.M)
    assert found, f"{path} has no function {name}"
    return found.group(0)


def tree(at, shape):
    (at / "agent").mkdir(parents=True)
    line = f'claude-runtime-protocol = {{ git = "{URL}", rev = "%s" }}\n'
    if shape == "workspace":
        (at / "Cargo.toml").write_text('[workspace]\nmembers = ["agent"]\n\n[workspace.dependencies]\n' + line % REV)
        (at / "agent/Cargo.toml").write_text("[dependencies]\nclaude-runtime-protocol = { workspace = true }\n")
    else:
        (at / "Cargo.toml").write_text('[workspace]\nmembers = ["agent"]\n')
        (at / "agent/Cargo.toml").write_text("[dependencies]\n" + line % OLD)
    return at


def run(shell, script, cwd, *args):
    done = subprocess.run([shell, "-c", script, shell, *args], cwd=cwd, capture_output=True, text=True, timeout=30)
    assert done.returncode == 0, done.stderr
    return done.stdout.split()


def test_the_installer_reads_the_workspace_pin_and_an_older_checkouts(tmp_path):
    reader = function("packaging/install.sh", "agent_toml_verdandi")
    script = 'die() { echo "$*" >&2; exit 1; }\n' + reader + 'agent_toml_verdandi "$1"; echo "$AT_GIT_URL" "$AT_REV"\n'
    for shape, rev in [("workspace", REV), ("older", OLD)]:
        at = tree(tmp_path / shape, shape)
        assert run("sh", script, at, str(at)) == [URL, rev], shape


def test_the_release_reads_the_workspace_pin_and_an_older_sources(tmp_path):
    reader = function("packaging/release.sh", "verdandi_dep") + function("packaging/release.sh", "pinned_verdandi")
    for shape, rev, manifest in [("workspace", REV, "Cargo.toml"), ("older", OLD, "agent/Cargo.toml")]:
        at = tree(tmp_path / shape, shape)
        got = run("bash", reader + f'pinned_verdandi {at}; echo "$RS_VERDANDI_URL" "$RS_VERDANDI_REV" "$RS_PIN_TOML"\n', at)
        assert got == [URL, rev, manifest], shape


def test_the_export_rewrites_the_pin_where_it_lives(tmp_path):
    reader = function("publish/export.sh", "pin_manifest")
    for shape, manifest in [("workspace", "Cargo.toml"), ("older", "agent/Cargo.toml")]:
        at = tree(tmp_path / shape, shape)
        assert run("bash", reader + "pin_manifest\n", at) == [manifest], shape


def test_the_aur_build_reads_the_workspace_pin_and_an_older_tree(tmp_path):
    reader = function("packaging/aur/eitri-git/PKGBUILD", "_verdandi_pin")
    for shape, rev in [("workspace", REV), ("older", OLD)]:
        at = tree(tmp_path / shape, shape)
        assert run("bash", reader + '_verdandi_pin; echo "$_verdandi_rev"\n', at) == [rev]


def test_the_aur_preflight_reads_the_workspace_pin_and_an_older_commit(tmp_path):
    reader = function("packaging/aur/test-in-container.sh", "verdandi_pin_at")
    # Only git's show operation is needed; read its requested manifest from the fixture tree.
    git = 'git() { cat "${4#*:}"; }\n'
    for shape, rev in [("workspace", REV), ("older", OLD)]:
        at = tree(tmp_path / shape, shape)
        assert run("bash", git + reader + 'verdandi_pin_at unused HEAD\n', at) == [rev]
