"""packaging/install.sh's tests.

The tests themselves are shell functions in packaging/tests/install/test_*.sh, run by harness.sh in
two shells: bash on the host, and dash inside an Ubuntu 24.04 image (Dockerfile.dash) -- this host
has no dash and must not install one. This file runs both halves, shellcheck, and guards the real
HOME's installer-touched paths around the container run (inside it they are not visible; on the host
run-in-env.sh guards every single installer run, through the same real-home-state.sh).

Scratch lives under ~/.cache/nv-v1dist-t9/run-<random> (never /tmp) and is removed after a passing
half. The per-run directory matters: two runs from different worktrees at once used to share one
directory, and each one's fresh_scratch() deleted the other's stubs mid-run (run-in-env.sh then
refused, rightly). Its name is random rather than the process id, because two runs can have the same
id (separate PID namespaces) and then still share it. The Docker image is tagged by what it is built
from, so a run never re-points a tag another run is using.
"""

import hashlib
import os
import pathlib
import re
import shutil
import subprocess
import sys
import tempfile

import pytest

HERE = pathlib.Path(__file__).resolve().parent
INSTALLER = HERE / "install.sh"
TESTS = HERE / "tests" / "install"
HARNESS = TESTS / "harness.sh"
PLUGIN = HERE.parent / "nvim" / "eitri.nvim"
EXTENSION = HERE.parent / "gnome-extension"
REAL_HOME_STATE = TESTS / "real-home-state.sh"
RUN_IN_ENV = TESTS / "run-in-env.sh"
ROOT_WRAPPER_TEST = TESTS / "test_root_wrapper.sh"
ROOT_INSTALLER = HERE.parent / "install.sh"
SCRATCH_PARENT = pathlib.Path.home() / ".cache" / "nv-v1dist-t9"
IMAGE_NAME = "eitri-install-dash"

_run_root = None


def make_run_root():
    """A new, empty directory no other run can have: the name is random, never derived from a pid."""
    SCRATCH_PARENT.mkdir(parents=True, exist_ok=True)
    return pathlib.Path(tempfile.mkdtemp(prefix="run-", dir=SCRATCH_PARENT))


def scratch_root():
    """This process's own run directory, made on first use."""
    global _run_root
    if _run_root is None:
        _run_root = make_run_root()
    return _run_root


def real_home_state(home=None, env=None):
    """The listing real-home-state.sh gives for a HOME (default: the real one): what an installer can
    touch there, by name, type, size and mtime and never by content. Not the application's own
    runtime state or settings, which change whenever someone uses Eitri while the tests run."""
    proc = subprocess.run(
        ["sh", str(REAL_HOME_STATE), str(home if home is not None else pathlib.Path.home())],
        capture_output=True, text=True, env=env,
    )
    assert proc.returncode == 0, report(proc)
    return proc.stdout.splitlines()


def assert_real_home_unchanged(before):
    after = real_home_state()
    changed = sorted(set(before) ^ set(after))
    assert after == before, "a path the installer can touch in the real HOME changed:\n" + "\n".join(changed)


UNIT_SCRATCH_PREFIX = "guard-"


@pytest.fixture(scope="session", autouse=True)
def _remove_empty_scratch_root():
    """Leave no per-run directory behind unless a failed half keeps its own scratch inside it (the
    fake HOMEs of this file's own unit tests are always removed)."""
    yield
    if _run_root is not None:
        for leftover in _run_root.glob(UNIT_SCRATCH_PREFIX + "*"):
            shutil.rmtree(leftover, ignore_errors=True)
        try:
            _run_root.rmdir()
        except OSError:
            pass


def fresh_scratch(name):
    path = scratch_root() / name
    shutil.rmtree(path, ignore_errors=True)
    path.mkdir(parents=True)
    return path


def report(proc):
    return f"exit {proc.returncode}\n--- stdout\n{proc.stdout}\n--- stderr\n{proc.stderr}"


def test_installer_shellcheck_clean():
    if shutil.which("shellcheck") is None:
        pytest.fail("shellcheck is required: install.sh is held to `shellcheck -s sh -o check-set-e-suppressed`")
    proc = subprocess.run(
        ["shellcheck", "-s", "sh", "-o", "check-set-e-suppressed", str(INSTALLER)],
        capture_output=True, text=True,
    )
    assert proc.returncode == 0, report(proc)


def test_the_0_2_0_installer_fixture_is_the_file_0_2_0_shipped():
    """fixtures/install-0.2.0.sh is what the harness runs as 0.2.0's own installer (a user who saved it
    reruns it to upgrade, INSTALL.md): packaging/install.sh from the tree 0.2.0 is cut from.
    Pinned by hash, and against the commit itself where this checkout still has it."""
    fixture = TESTS / "fixtures" / "install-0.2.0.sh"
    assert hashlib.sha256(fixture.read_bytes()).hexdigest() == (
        "1730da3265672918b14cc79a0ee9dd7ee44903ca28aab1624265a44dd8b82013"
    )
    show = subprocess.run(["git", "-C", str(HERE), "show", "4ce433a0:packaging/install.sh"], capture_output=True)
    if show.returncode == 0:
        assert show.stdout == fixture.read_bytes()


def test_harness_shellcheck_clean():
    if shutil.which("shellcheck") is None:
        pytest.skip("shellcheck is not installed")
    files = [str(p) for p in sorted(TESTS.glob("*.sh"))]
    files += [str(p) for p in sorted((TESTS / "fixtures").iterdir())
              if p.name not in ("old-install-sh-launcher", "install-0.2.0.sh")]
    proc = subprocess.run(["shellcheck", "-s", "sh", "-x", *files], capture_output=True, text=True, cwd=TESTS)
    assert proc.returncode == 0, report(proc)


def test_installer_header_curl_follows_redirects():
    """releases/latest/download/install.sh answers a first-time GET with a 302
    and no body, so the documented one-liner needs -L (or -fsSL) or `sh` silently runs nothing and
    exits 0 -- reproduced by hand against a real GitHub redirect."""
    header_lines = [
        line for line in INSTALLER.read_text().splitlines()[:10]
        if "curl" in line and "install.sh" in line
    ]
    assert len(header_lines) == 2, f"expected the two curl one-liners in the header, found {header_lines!r}"
    for line in header_lines:
        words = line.split()
        follows_redirects = any(
            w.startswith("-") and not w.startswith("--") and "L" in w for w in words
        )
        assert follows_redirects, f"header curl example lacks -L (or a combined -fsSL), so a redirect silently runs nothing: {line!r}"


def test_installer_checkbashisms_if_installed():
    if shutil.which("checkbashisms") is None:
        pytest.skip("checkbashisms is not installed (optional)")
    proc = subprocess.run(["checkbashisms", "--posix", str(INSTALLER)], capture_output=True, text=True)
    assert proc.returncode == 0, report(proc)


def test_harness_under_bash():
    scratch = fresh_scratch("bash")
    before = real_home_state()
    proc = subprocess.run(
        ["bash", str(HARNESS), "--sh", "bash", "--scratch", str(scratch), "--real-home", str(pathlib.Path.home())],
        capture_output=True, text=True,
    )
    assert_real_home_unchanged(before)
    assert proc.returncode == 0, report(proc)
    assert "# installer shell: bash" in proc.stdout
    assert " 0 failed" in proc.stdout, report(proc)
    shutil.rmtree(scratch, ignore_errors=True)


def docker_usable():
    if shutil.which("docker") is None:
        return False
    return subprocess.run(["docker", "info"], capture_output=True).returncode == 0


def dash_image_tag(uid, gid):
    """The image's tag, keyed by what it is built from (Dockerfile.dash and the uid and gid baked
    into it). One fixed tag was shared by every run on the host: a run from a worktree whose
    Dockerfile.dash differed re-pointed it under another run's `docker run`. Runs that build the same
    thing share a tag, and so one image."""
    key = hashlib.sha256((TESTS / "Dockerfile.dash").read_bytes() + f"\0{uid}\0{gid}".encode())
    return f"{IMAGE_NAME}:{key.hexdigest()[:12]}"


def build_dash_image(uid, gid):
    """Build (or find in Docker's cache) the Ubuntu 24.04 dash image and return its tag. The build is
    the only step that needs a network (apt-get, inside the image). A proxy the caller exports is
    passed on as Docker's predefined proxy build args, which do not enter the layer cache key.
    Two builds of the same tag at once are safe: Docker's builder shares the layers and tags the
    same image id."""
    tag = dash_image_tag(uid, gid)
    proxy_args = []
    for name in ("http_proxy", "https_proxy", "HTTP_PROXY", "HTTPS_PROXY", "no_proxy", "NO_PROXY"):
        if os.environ.get(name):
            proxy_args += ["--build-arg", f"{name}={os.environ[name]}"]
    build = subprocess.run(
        ["docker", "build", "-q", *proxy_args, "--build-arg", f"UID={uid}", "--build-arg", f"GID={gid}",
         "-t", tag, "-f", str(TESTS / "Dockerfile.dash"), str(TESTS)],
        capture_output=True, text=True,
    )
    assert build.returncode == 0, report(build)
    return tag


def test_harness_under_dash_in_ubuntu():
    if not docker_usable():
        pytest.skip("docker is not usable here; the dash half needs the Ubuntu 24.04 image")
    uid, gid = os.getuid(), os.getgid()
    image = build_dash_image(uid, gid)

    scratch = fresh_scratch("dash")
    before = real_home_state()
    # Container hygiene: bind mounts of existing host paths only (--mount errors on a
    # missing one), the host's uid, no runtime dir, Wayland socket or session bus -- and no network
    # at all: the fixture server runs inside, on the container's own 127.0.0.1.
    proc = subprocess.run(
        ["docker", "run", "--rm", "--user", f"{uid}:{gid}", "--network", "none",
         "--mount", f"type=bind,src={scratch},dst={scratch}",
         "--mount", f"type=bind,src={HERE},dst={HERE},readonly",
         # The nvim plugin the installer lays out: the harness builds its fixture releases from it.
         "--mount", f"type=bind,src={PLUGIN},dst={PLUGIN},readonly",
         # The GNOME Shell extension's files, which the fixture releases and the from-source checkout carry.
         "--mount", f"type=bind,src={EXTENSION},dst={EXTENSION},readonly",
         image, "/bin/sh", str(HARNESS), "--sh", "/bin/sh", "--scratch", str(scratch)],
        capture_output=True, text=True,
    )
    assert_real_home_unchanged(before)
    assert proc.returncode == 0, report(proc)
    assert "# installer shell: /bin/sh (/usr/bin/dash)" in proc.stdout, report(proc)
    assert " 0 failed" in proc.stdout, report(proc)
    shutil.rmtree(scratch, ignore_errors=True)


def test_root_wrapper_under_bash():
    """packaging/tests/install/test_root_wrapper.sh (the repository root install.sh's own tests,
    implemented in shell) is standalone -- it stubs packaging/install.sh itself and needs no fixture
    machinery, so unlike harness.sh it is not sourced by anything else. It used to be run only by
    hand; wired in here so both shell halves this file already holds every other installer test to
    actually cover it too. Its own scratch dir lives under a fresh HOME so its
    real-home guard elsewhere in this file has nothing to see either way.
    """
    scratch = fresh_scratch("root-wrapper-bash")
    proc = subprocess.run(
        ["bash", str(ROOT_WRAPPER_TEST)],
        capture_output=True, text=True,
        env={**os.environ, "HOME": str(scratch)},
    )
    assert proc.returncode == 0, report(proc)
    shutil.rmtree(scratch, ignore_errors=True)


def test_root_wrapper_under_dash_in_ubuntu():
    if not docker_usable():
        pytest.skip("docker is not usable here; the dash half needs the Ubuntu 24.04 image")
    uid, gid = os.getuid(), os.getgid()
    image = build_dash_image(uid, gid)

    scratch = fresh_scratch("root-wrapper-dash")
    # Two mounts: TESTS's own tree (the test script itself, under HERE) plus the repository root
    # install.sh it wraps (ROOT_INSTALLER, one level above HERE and so outside every other test's
    # own bind mount) -- test_root_wrapper.sh reads both by their real absolute paths, never
    # packaging/install.sh itself (it stubs that out).
    proc = subprocess.run(
        ["docker", "run", "--rm", "--user", f"{uid}:{gid}", "--network", "none",
         "--mount", f"type=bind,src={scratch},dst={scratch}",
         "--mount", f"type=bind,src={HERE},dst={HERE},readonly",
         "--mount", f"type=bind,src={ROOT_INSTALLER},dst={ROOT_INSTALLER},readonly",
         "-e", f"HOME={scratch}",
         image, "/bin/sh", str(ROOT_WRAPPER_TEST)],
        capture_output=True, text=True,
    )
    assert proc.returncode == 0, report(proc)
    shutil.rmtree(scratch, ignore_errors=True)


_RULING_CITATION_PATTERN = re.compile(
    r"spec §|spec \d{4}-\d{2}-\d{2}-[\w-]*\.md|\b[DRI]\d{1,3}(?:/[A-Z]?\d{0,3})?\b"
)


@pytest.mark.parametrize("existing_temp", [False, True])
def test_selected_is_private_under_group_writable_umask(tmp_path, existing_temp):
    source = INSTALLER.read_text()
    functions = source[source.index("selected_replaceable() {"):source.index("# sidecar_probe PATH:")]
    script = functions + r"""
set -e
die() { echo "$*" >&2; exit 1; }
NV_SIDECAR_ROOT=$1
NV_SIDECAR_SELECTED=$1/selected
umask 002
if [ "$2" = yes ]; then
    printf old >"$NV_SIDECAR_ROOT/.selected.tmp.$$"
fi
write_selected 0.3.2
"""
    proc = subprocess.run(["sh", "-c", script, "test", str(tmp_path), "yes" if existing_temp else "no"],
                          capture_output=True, text=True)
    assert proc.returncode == 0, report(proc)
    selected = tmp_path / "selected"
    assert selected.read_text() == "version=0.3.2\n"
    assert selected.stat().st_mode & 0o777 == 0o600


def test_help_cites_no_internal_ruling_or_spec_section():
    """`install.sh --help` is what a stranger
    running the curl installer actually reads. It used to cite a spec section and a ruling id
    -- markers that mean nothing outside this repo's own planning documents."""
    proc = subprocess.run(["sh", str(INSTALLER), "--help"], capture_output=True, text=True)
    assert proc.returncode == 0, report(proc)
    match = _RULING_CITATION_PATTERN.search(proc.stdout)
    assert match is None, (match and match.group(0), proc.stdout)


def test_desktop_exec_parses_back_with_glib():
    """The Exec line, read back by GLib (what GNOME's launcher uses), names the launcher exactly.

    GLib rejects a single `\\$` in a key file value outright, so this is the check that the
    string-value escaping layer (the Desktop Entry Specification) is really applied.
    """
    try:
        import gi

        gi.require_version("GLib", "2.0")
        from gi.repository import GLib
    except (ImportError, ValueError):
        pytest.skip("python3-gobject (GLib) is not installed")
    scratch = fresh_scratch("glib")
    proc = subprocess.run(
        ["bash", str(HARNESS), "--sh", "bash", "--scratch", str(scratch), "--real-home", str(pathlib.Path.home()),
         "--only", "t_desktop_exec_all_specials"],
        capture_output=True, text=True,
    )
    assert proc.returncode == 0, report(proc)
    t = scratch / "t" / "t_desktop_exec_all_specials"
    home = (t / "home-path").read_text().rstrip("\n")
    desktop = pathlib.Path(home) / ".local/share/applications/cn.huntergrey.eitri.desktop"
    keyfile = GLib.KeyFile()
    keyfile.load_from_file(str(desktop), GLib.KeyFileFlags.NONE)
    ok, argv = GLib.shell_parse_argv(keyfile.get_string("Desktop Entry", "Exec"))
    assert ok
    # Field codes are expanded after parsing; a literal % is %%.
    assert argv[0].replace("%%", "%") == home + "/.local/bin/eitri"
    assert argv[1:] == ["--quiet", "--from-desktop", "%F"]
    if shutil.which("desktop-file-validate"):
        check = subprocess.run(["desktop-file-validate", str(desktop)], capture_output=True, text=True)
        assert check.returncode == 0, report(check)
    shutil.rmtree(scratch, ignore_errors=True)


# ---------------------------------------------------------------------------------------------
# The real-HOME guard (real-home-state.sh) and the per-run names, tested on a fake HOME.


def _guard_env(**extra):
    """The environment real-home-state.sh reads: the caller's, without its XDG_* variables (so a test
    sees install.sh's defaults), plus the given ones."""
    env = {k: v for k, v in os.environ.items() if not k.startswith("XDG_")}
    env.update(extra)
    return env


def _write(path, text="x"):
    path = pathlib.Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text)
    return path


def _fake_home(name):
    """A HOME holding an installed Eitri and the application's own state and settings."""
    home = fresh_scratch(name)
    _write(home / ".local/lib/eitri/RELEASE")
    _write(home / ".local/bin/eitri")
    _write(home / ".local/share/applications/cn.huntergrey.eitri.desktop")
    _write(home / ".local/share/icons/hicolor/16x16/apps/cn.huntergrey.eitri.png")
    _write(home / ".local/share/licenses/eitri/LICENSE")
    _write(home / ".local/share/gnome-shell/extensions/eitri@huntergrey.cn/extension.js")
    _write(home / ".local/share/gnome-shell/extensions/other@example.org/metadata.json")
    _write(home / ".local/share/eitri/sidecar/abc1234/BUILD")
    _write(home / ".cache/eitri/download/tarball")
    _write(home / ".local/state/eitri/history/prompts", "one")
    _write(home / ".config/eitri/init.lua", "-- mine")
    return home


def test_real_home_guard_ignores_the_applications_own_state_and_settings():
    """The reason for the narrowing: Eitri rewrites its prompt history, layouts and permission rules
    under ~/.local/state/eitri while someone uses it, and that failed the main session's run."""
    home = _fake_home("guard-own-state")
    before = real_home_state(home, _guard_env())
    _write(home / ".local/state/eitri/history/prompts", "a longer line than before")
    os.utime(home / ".local/state/eitri/history/prompts", ns=(1, 1))
    _write(home / ".local/state/eitri/history/another")
    _write(home / ".local/state/eitri/layout/abc.json")
    _write(home / ".config/eitri/init.lua", "-- changed")
    _write(home / ".config/eitri/lua/more.lua")
    # Other applications' files beside the installer's own names are not its to touch either.
    _write(home / ".local/bin/other")
    _write(home / ".local/share/applications/other.desktop")
    _write(home / ".local/share/icons/hicolor/icon-theme.cache")
    _write(home / ".local/share/icons/hicolor/16x16/apps/other.png")
    _write(home / ".local/lib/other/file")
    _write(home / ".cache/other/file")
    _write(home / ".local/share/gnome-shell/extensions/other@example.org/metadata.json", "changed")
    _write(home / ".local/share/gnome-shell/extensions/another@example.org/extension.js")
    assert real_home_state(home, _guard_env()) == before


def _installer_changes():
    """One change per path the installer writes, replaces or removes, each as a function of the HOME."""
    def touch_later(rel):
        return lambda h: os.utime(h / rel, ns=(1, 1))

    def remove_tree(rel):
        return lambda h: shutil.rmtree(h / rel)

    def new_file(rel):
        return lambda h: _write(h / rel)

    return {
        "a file in the installed tree": new_file(".local/lib/eitri/shell"),
        "an installed file touched": touch_later(".local/lib/eitri/RELEASE"),
        "the installed tree removed": remove_tree(".local/lib/eitri"),
        "an upgrade's new tree": new_file(".local/lib/eitri.new/RELEASE"),
        "an upgrade's old tree": new_file(".local/lib/eitri.old/RELEASE"),
        "the launcher rewritten": lambda h: _write(h / ".local/bin/eitri", "a different launcher"),
        "the launcher's backup": new_file(".local/bin/eitri.bak-20261001-101010"),
        "the launcher's temporary name": new_file(".local/bin/.eitri.tmp.4242"),
        "the desktop entry rewritten": lambda h: _write(h / ".local/share/applications/cn.huntergrey.eitri.desktop", "[x]"),
        "the older desktop entry": new_file(".local/share/applications/eitri.desktop"),
        "the desktop entry's temporary name": new_file(".local/share/applications/.cn.huntergrey.eitri.desktop.tmp.4242"),
        "an icon": new_file(".local/share/icons/hicolor/48x48/apps/cn.huntergrey.eitri.png"),
        "an icon's temporary name": new_file(".local/share/icons/hicolor/16x16/apps/.cn.huntergrey.eitri.png.tmp.4242"),
        "the icon removed": lambda h: (h / ".local/share/icons/hicolor/16x16/apps/cn.huntergrey.eitri.png").unlink(),
        "an extension file": new_file(".local/share/gnome-shell/extensions/eitri@huntergrey.cn/extension.js"),
        "an extension file's temporary name": new_file(
            ".local/share/gnome-shell/extensions/eitri@huntergrey.cn/.extension.js.tmp.4242"),
        "the extension removed": remove_tree(".local/share/gnome-shell/extensions/eitri@huntergrey.cn"),
        "a licence file": new_file(".local/share/licenses/eitri/THIRD-PARTY-LICENSES"),
        "a sidecar": new_file(".local/share/eitri/sidecar/bbbbbbb/verdandi-claude-sidecar"),
        "the private nvim": new_file(".local/share/eitri/nvim/v0.11.4/bin/nvim"),
        "the AppArmor profile": new_file(".local/share/eitri/apparmor/eitri-user-1000"),
        "the download cache": new_file(".cache/eitri/unpack/x"),
        "the installer's lock": new_file(".cache/eitri/lock/pid"),
        "--purge removing the state directory": remove_tree(".local/state/eitri"),
        "--purge removing the settings directory": remove_tree(".config/eitri"),
    }


@pytest.mark.parametrize("what", sorted(_installer_changes()))
def test_real_home_guard_sees_what_an_installer_changes(what):
    home = _fake_home("guard-installer")
    before = real_home_state(home, _guard_env())
    _installer_changes()[what](home)
    assert real_home_state(home, _guard_env()) != before


def test_real_home_guard_sees_an_installer_creating_what_was_absent():
    home = fresh_scratch("guard-absent")
    before = real_home_state(home, _guard_env())
    assert before and all(line.startswith("absent ") for line in before)
    _write(home / ".local/bin/eitri")
    assert real_home_state(home, _guard_env()) != before


def _xdg_env(home):
    return _guard_env(XDG_DATA_HOME=str(home / "data"), XDG_STATE_HOME=str(home / "st"),
                      XDG_CACHE_HOME=str(home / "cch"), XDG_CONFIG_HOME=str(home / "cfg"))


@pytest.mark.parametrize("rel", [
    "data/licenses/eitri/LICENSE",
    "data/eitri/nvim/x",
    "data/applications/cn.huntergrey.eitri.desktop",
    "data/icons/hicolor/16x16/apps/cn.huntergrey.eitri.png",
    "data/gnome-shell/extensions/eitri@huntergrey.cn/extension.js",
    "cch/eitri/lock/pid",
])
def test_real_home_guard_watches_the_xdg_directories_install_sh_would_write(rel):
    home = fresh_scratch("guard-xdg")
    before = real_home_state(home, _xdg_env(home))
    _write(home / rel)
    assert real_home_state(home, _xdg_env(home)) != before


def test_real_home_guard_sees_purge_remove_the_state_directory_the_xdg_variable_names():
    home = fresh_scratch("guard-xdg")
    _write(home / "st/eitri/history/prompts")
    before = real_home_state(home, _xdg_env(home))
    shutil.rmtree(home / "st/eitri")
    assert real_home_state(home, _xdg_env(home)) != before


def test_real_home_guard_sees_purge_remove_the_config_directory_the_xdg_variable_names():
    home = fresh_scratch("guard-xdg")
    _write(home / "cfg/eitri/init.lua")
    before = real_home_state(home, _xdg_env(home))
    shutil.rmtree(home / "cfg/eitri")
    assert real_home_state(home, _xdg_env(home)) != before


def test_real_home_guard_leaves_the_default_places_alone_when_xdg_names_others():
    home = fresh_scratch("guard-xdg")
    before = real_home_state(home, _xdg_env(home))
    _write(home / ".local/share/eitri/sidecar/ccccccc/BUILD")
    _write(home / ".local/share/licenses/eitri/NEW")
    _write(home / ".cache/eitri/NEW")
    _write(home / ".config/eitri/init.lua")
    assert real_home_state(home, _xdg_env(home)) == before


def test_real_home_guard_ignores_a_relative_or_empty_xdg_value_as_install_sh_does():
    home = _fake_home("guard-xdg-relative")
    env = _guard_env(XDG_DATA_HOME="data", XDG_CACHE_HOME="", XDG_STATE_HOME="state", XDG_CONFIG_HOME="cfg")
    before = real_home_state(home, env)
    _write(home / ".local/share/eitri/sidecar/ccccccc/BUILD")
    _write(home / ".cache/eitri/NEW")
    assert real_home_state(home, env) != before
    settings_present = real_home_state(home, env)
    shutil.rmtree(home / ".config/eitri")
    assert real_home_state(home, env) != settings_present
    assert real_home_state(home, env) == real_home_state(home, _guard_env())


def test_real_home_guard_lists_names_types_sizes_and_times_never_contents():
    home = _fake_home("guard-no-content")
    secret = "a-secret-that-must-not-be-read-back"
    _write(home / ".local/lib/eitri/RELEASE", secret)
    listing = "\n".join(real_home_state(home, _guard_env()))
    assert secret not in listing
    assert f"{home}/.local/lib/eitri/RELEASE f {len(secret)} " in listing


def test_real_home_guard_refuses_a_relative_home():
    proc = subprocess.run(["sh", str(REAL_HOME_STATE), "relative/home"], capture_output=True, text=True)
    assert proc.returncode == 2 and "ABSOLUTE-HOME" in proc.stderr, report(proc)


def _run_in_env_guarded(real_home, command):
    """run-in-env.sh with its guard on a fake real HOME, running one shell command in a run HOME of
    its own (the command gets the guarded HOME as $1)."""
    run_home = fresh_scratch("guard-run-home")
    stubs = fresh_scratch("guard-stubs")
    _write(stubs / "claude", "#!/bin/sh\nexit 0\n")
    (stubs / "claude").chmod(0o755)
    return subprocess.run(
        ["sh", str(RUN_IN_ENV), "--home", str(run_home), "--stubs", str(stubs),
         "--guard-real-home", str(real_home), "--", "sh", "-c", command, "sh", str(real_home)],
        capture_output=True, text=True, env=_guard_env(),
    )


def test_run_in_env_guard_passes_while_only_the_applications_own_state_changes():
    home = _fake_home("guard-run-in-env-quiet")
    proc = _run_in_env_guarded(home, 'echo more >>"$1/.local/state/eitri/history/prompts"; echo y >"$1/.config/eitri/new"')
    assert proc.returncode == 0, report(proc)


def test_run_in_env_guard_fires_when_the_installed_tree_changes():
    home = _fake_home("guard-run-in-env-loud")
    proc = _run_in_env_guarded(home, 'echo changed >"$1/.local/lib/eitri/RELEASE"')
    assert proc.returncode == 97, report(proc)
    assert "installer can touch" in proc.stderr, report(proc)


def test_the_scratch_root_is_not_named_by_the_process_id(monkeypatch):
    """Two runs can have the same process id (separate PID namespaces), so a name made from it is
    shared by exactly the runs that must not share one."""
    monkeypatch.setattr(os, "getpid", lambda: 4242)
    first, second = make_run_root(), make_run_root()
    try:
        assert first != second
        assert "4242" not in first.name and "4242" not in second.name
        assert first.parent == SCRATCH_PARENT
    finally:
        first.rmdir()
        second.rmdir()


def test_the_dash_image_tag_follows_what_the_image_is_built_from():
    assert dash_image_tag(1000, 1000) == dash_image_tag(1000, 1000)
    assert dash_image_tag(1000, 1000) != dash_image_tag(1001, 1000)
    assert dash_image_tag(1000, 1000) != dash_image_tag(1000, 1001)
    assert re.fullmatch(rf"{IMAGE_NAME}:[0-9a-f]{{12}}", dash_image_tag(1000, 1000))


@pytest.mark.parametrize("reader", ["installer", "launcher"])
@pytest.mark.parametrize("mode,linked,foreign,accepted", [
    (0o600, False, False, True), (0o664, False, False, True),
    (0o602, False, False, False), (0o666, False, False, False),
    (0o600, False, True, False), (0o644, True, False, True),
    (0o664, True, False, False), (0o602, True, False, False),
])
def test_selection_readers_check_owner_and_write_bits(tmp_path, reader, mode, linked, foreign, accepted):
    root = tmp_path / "eitri" / "sidecar"
    root.mkdir(parents=True)
    selected = root / "selected"
    target = root / "target" if linked else selected
    target.write_text("version=0.3.2\n")
    target.chmod(mode)
    if linked:
        selected.symlink_to(target)
    # A different effective uid exercises the foreign-owner branch without needing chown privileges.
    uid = os.geteuid() + 1 if foreign else os.geteuid()
    prelude = f'id() {{ printf "%s\\n" {uid}; }}\n'
    if reader == "installer":
        source = INSTALLER.read_text()
        functions = source[source.index("version_name() {"):source.index("# update_intact VERSION:")]
        functions += source[source.index("status_selected() {"):source.index("# status_named sets")]
        script = prelude + functions + r'''
NV_NL='
'
NV_SIDECAR_VERSION_ERE='[0-9]+\.[0-9]+\.[0-9]+'
NV_SIDECAR_SELECTED=$1/eitri/sidecar/selected
read_selected
if [ -z "$NV_SELECTED_VERSION" ]; then
    status_selected
    printf '%s\n' "$SS_NOTE"
fi
[ "$NV_SELECTED_VERSION" = 0.3.2 ]
'''
        shell = "sh"
    else:
        source = (HERE / "eitri.launcher.sh").read_text()
        functions = source[source.index("# Read only an owned regular selection"):source.index("sidecar_found() {")]
        (tmp_path / "eitri-setup").touch()
        (tmp_path / "RELEASE").write_text("SIDECAR_VERSION=0.3.1\n")
        update = root / "updates" / "0.3.2"
        update.mkdir(parents=True)
        for name in ("BUILD", "verdandi-claude-sidecar"):
            (update / name).touch()
        script = prelude + functions + r'''
LIBDIR=$1
xdg_data_home() { printf '%s\n' "$LIBDIR"; }
version_newer() { return 0; }
selected_update_found
'''
        shell = "bash"
    proc = subprocess.run([shell, "-c", script, "test", str(tmp_path)], capture_output=True, text=True)
    assert proc.returncode == (0 if accepted else 1), report(proc)
    if reader == "installer" and not accepted:
        reason = "not yours" if foreign else "writable by others"
        assert proc.stdout.strip() == f"refused: {reason}", report(proc)
    assert target.stat().st_mode & 0o777 == mode
def test_dead_takeover_lock_requires_manual_removal():
    """Exercise lock recovery without downloads or a fixture server."""
    root = fresh_scratch("guard-dead-takeover")
    lock = root / "lock"
    takeover = root / "lock.takeover"
    holder = subprocess.Popen(["sh", "-c", "exit 0"])
    holder.wait()
    for directory in (lock, takeover):
        directory.mkdir()
        (directory / "pid").write_text(str(holder.pid) + "\n")
    (lock / "sentinel").write_text("untouched\n")
    before = {p.name: (p.read_bytes(), p.stat().st_ino) for p in lock.iterdir()}
    text = INSTALLER.read_text()
    functions = "\n".join(re.search(r"^" + name + r"\(\) \{\n.*?^\}", text, re.M | re.S)[0]
                          for name in ("pid_alive", "acquire_lock"))
    script = ("set -eu\n" + functions + '\ncheck_cache_root() { :; }\n'
              'die() { echo "$*" >&2; exit 1; }\nwarn() { :; }\n'
              'OPT_DRY_RUN=0; NV_LOCK=$1; NV_CACHE_NV=$2; acquire_lock\n')
    proc = subprocess.run(["sh", "-c", script, "_", str(lock), str(root)],
                          capture_output=True, text=True)
    assert proc.returncode != 0, report(proc)
    assert "no installer is using it" in proc.stderr, report(proc)
    assert f"if none is running, remove {takeover} by hand and re-run" in proc.stderr, report(proc)
    assert {p.name: (p.read_bytes(), p.stat().st_ino) for p in lock.iterdir()} == before
    assert (takeover / "pid").read_text() == str(holder.pid) + "\n"


@pytest.mark.parametrize("failure", ["pid-write", "remove-main", "INT", "TERM"])
def test_failed_takeover_removes_its_sublock(failure):
    """Inject failures at the write, removal and wait boundaries without a fixture server."""
    root = fresh_scratch("guard-takeover-" + failure)
    lock = root / "lock"
    takeover = root / "lock.takeover"
    lock.mkdir()
    (lock / "sentinel").write_text("untouched\n")
    if failure == "remove-main":
        holder = subprocess.Popen(["sh", "-c", "exit 0"])
        holder.wait()
        (lock / "pid").write_text(str(holder.pid) + "\n")
    before = {p.name: (p.read_bytes(), p.stat().st_ino) for p in lock.iterdir()}
    text = INSTALLER.read_text()
    functions = "\n".join(re.search(r"^" + name + r"\(\) \{\n.*?^\}", text, re.M | re.S)[0]
                          for name in ("pid_alive", "acquire_lock", "on_exit"))
    traps = "\n".join(re.findall(r"^\ttrap .*", text, re.M))
    inject = {
        "pid-write": "printf() { return 1; }\n",
        "remove-main": 'rm() { [ "$3" != "$NV_LOCK" ] || return 1; command rm "$@"; }\n',
        "INT": 'sleep() { kill -s INT "$$"; }\n',
        "TERM": 'sleep() { kill -s TERM "$$"; }\n',
    }[failure]
    script = ("set -eu\n" + functions + "\n" + traps + '\ncheck_cache_root() { :; }\n'
              'die() { echo "$*" >&2; exit 1; }\nwarn() { :; }\n'
              'OPT_DRY_RUN=0; LOCK_HELD=0; TAKEOVER_HELD=0; NV_SIDECAR_CHECK_DIR=\n'
              'NV_LOCK=$1; NV_CACHE_NV=$2\n' + inject + 'acquire_lock\n')
    # A background runner may ignore SIGINT. Noninteractive shells cannot trap a signal
    # ignored on entry, so restore the child's dispositions before exec, leaving pytest's alone.
    launch = ("import os, signal, sys; "
              "signal.signal(signal.SIGINT, signal.SIG_DFL); "
              "signal.signal(signal.SIGTERM, signal.SIG_DFL); "
              "os.execvp(sys.argv[1], sys.argv[1:])")
    proc = subprocess.run([sys.executable, "-c", launch, "sh", "-c", script, "_", str(lock), str(root)],
                          capture_output=True, text=True, timeout=10)
    assert proc.returncode == {"INT": 130, "TERM": 143}.get(failure, 1), report(proc)
    assert not takeover.exists(), report(proc)
    assert {p.name: (p.read_bytes(), p.stat().st_ino) for p in lock.iterdir()} == before
    if failure == "pid-write":
        assert f"cannot write {takeover}/pid" in proc.stderr, report(proc)
        assert "another Eitri installer" not in proc.stderr, report(proc)
    elif failure == "remove-main":
        assert "cannot remove the stale lock" in proc.stderr, report(proc)


@pytest.mark.parametrize("shell", ["bash", "dash"])
@pytest.mark.parametrize("version,bridge,server", [
    ("0.2.2", 0, 0), ("0.3.0-rc.1", 0, 0), ("0.3.0", 0, 0),
    ("0.3.1-rc.0", 1, 0), ("0.3.1-rc.1", 1, 0), ("0.3.1", 1, 0),
    ("0.3.2-rc.0", 1, 1), ("0.3.2-rc.1", 1, 1), ("0.3.2", 1, 1),
    ("0.10.0", 1, 1),
])
def test_companion_boundaries(shell, version, bridge, server):
    if shutil.which(shell) is None:
        pytest.skip(f"{shell} is not installed")
    script = INSTALLER.read_text().replace('main "$@" </dev/null', r'''
NV_VERSION=$1
for binary in eitri-editor-bridge eitri-server shell; do
    companion_required "$binary"
    printf '%s=%s\n' "$binary" "$NV_COMPANION_REQUIRED"
done
''')
    proc = subprocess.run([shell, "-s", "--", version], input=script,
                          capture_output=True, text=True, timeout=10)
    assert proc.returncode == 0, report(proc)
    assert proc.stdout.splitlines() == [f"eitri-editor-bridge={bridge}", f"eitri-server={server}", "shell=1"]


@pytest.mark.parametrize("version", [
    "0.2.2", "0.3.0-rc.1", "0.3.0", "0.3.1-rc.1", "0.3.1",
    "0.3.2-rc.0", "0.3.2-rc.1", "0.3.2", "0.10.0",
])
@pytest.mark.parametrize("companion", ["eitri-server", "eitri-editor-bridge"])
@pytest.mark.parametrize("mode", ["absent", "executable", "not-executable"])
def test_archive_companion_compatibility(version, companion, mode):
    """Exercise unpack_new's real layout checks without the HTTP fixture server."""
    with tempfile.TemporaryDirectory(prefix=UNIT_SCRATCH_PREFIX, dir=scratch_root()) as scratch:
        script = INSTALLER.read_text().replace('main "$@" </dev/null', r'''
NV_VERSION=$1
missing=$2
NV_HOME=$3
mode=$4
NV_STAGE=$NV_HOME/stage
NV_LIBROOT=$NV_HOME/lib
NV_LIB=$NV_LIBROOT/eitri
NV_TARBALL_NAME=eitri-$NV_VERSION-x86_64-linux.tar.gz
NV_TARBALL=$NV_HOME/$NV_TARBALL_NAME
fixture=$NV_HOME/fixture/eitri-$NV_VERSION-x86_64-linux
mkdir -p "$fixture/lib/eitri/plugins"
for plugin in $NV_SHIPPED_PLUGINS; do
    mkdir -p "$fixture/lib/eitri/plugins/$plugin"
    touch "$fixture/lib/eitri/plugins/$plugin/eitri-plugin.toml"
done
for path in bin/eitri lib/eitri/RELEASE; do
    mkdir -p "$fixture/${path%/*}"
    printf 'fixture\n' >"$fixture/$path"
done
for path in $NV_DESKTOP; do
    mkdir -p "$fixture/share/applications"
    touch "$fixture/share/applications/$path"
done
for path in $NV_PLUGIN_FILES; do
    mkdir -p "$fixture/share/eitri/eitri.nvim/${path%/*}"
    touch "$fixture/share/eitri/eitri.nvim/$path"
done
for path in $NV_EXT_FILES; do
    mkdir -p "$fixture/share/gnome-shell/extensions/$NV_EXT_ID"
    touch "$fixture/share/gnome-shell/extensions/$NV_EXT_ID/$path"
done
for path in $NV_ICON_FILES; do
    mkdir -p "$fixture/share/icons/${path%/*}"
    touch "$fixture/share/icons/$path"
done
for path in $NV_LICENCE_FILES; do
    mkdir -p "$fixture/share/licenses/eitri"
    touch "$fixture/share/licenses/eitri/$path"
done
# Later layouts also require NOTICE, independently of the companion under test.
case $NV_VERSION in 0.3.1* | 0.3.2* | 0.10.0) touch "$fixture/share/licenses/eitri/NOTICE" ;; esac
for path in $NV_BINARIES eitri-setup; do
    if [ "$path" = "$missing" ] && [ "$mode" = absent ]; then continue; fi
    printf '#!/bin/sh\nexit 0\n' >"$fixture/lib/eitri/$path"
    chmod 0755 "$fixture/lib/eitri/$path"
    if [ "$path" = "$missing" ] && [ "$mode" = not-executable ]; then
        chmod 0644 "$fixture/lib/eitri/$path"
    fi
done
# Only metadata parsing is outside this layout test; archive and path checks run unchanged.
parse_release() { REL_VERSION=$NV_VERSION; REL_REV7=aaaaaaa; }
tar -C "$NV_HOME/fixture" -czf "$NV_TARBALL" "eitri-$NV_VERSION-x86_64-linux"
unpack_new
if [ "$mode" = absent ]; then [ ! -e "$NV_LIB.new/$missing" ];
else [ -x "$NV_LIB.new/$missing" ]; fi
''')
        proc = subprocess.run(["bash", "-s", "--", version, companion, scratch, mode], input=script,
                              capture_output=True, text=True, timeout=30)
        legacy = version in ("0.2.2", "0.3.0-rc.1", "0.3.0") or (
            companion == "eitri-server" and version in ("0.3.1-rc.1", "0.3.1"))
        if mode == "executable" or (legacy and mode == "absent"):
            assert proc.returncode == 0, report(proc)
        else:
            assert proc.returncode != 0, report(proc)
            assert f"has no executable lib/eitri/{companion}" in proc.stderr, report(proc)
