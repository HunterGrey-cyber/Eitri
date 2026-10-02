"""packaging/install.sh's tests (plan 2026-09-27-v1-dist, Task 9; spec §6).

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
import tempfile

import pytest

HERE = pathlib.Path(__file__).resolve().parent
INSTALLER = HERE / "install.sh"
TESTS = HERE / "tests" / "install"
HARNESS = TESTS / "harness.sh"
PLUGIN = HERE.parent / "nvim" / "eitri.nvim"
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
        pytest.fail("shellcheck is required: spec §6.1 holds install.sh to `shellcheck -s sh -o check-set-e-suppressed`")
    proc = subprocess.run(
        ["shellcheck", "-s", "sh", "-o", "check-set-e-suppressed", str(INSTALLER)],
        capture_output=True, text=True,
    )
    assert proc.returncode == 0, report(proc)


def test_the_0_2_0_installer_fixture_is_the_file_0_2_0_shipped():
    """fixtures/install-0.2.0.sh is what the harness runs as 0.2.0's own installer (a user who saved it
    reruns it to upgrade, INSTALL.md): packaging/install.sh at main 4ce433a0, the tree 0.2.0 is cut from.
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
    """installer-claude-3: releases/latest/download/install.sh answers a first-time GET with a 302
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
    # Container hygiene (spec §2.4): bind mounts of existing host paths only (--mount errors on a
    # missing one), the host's uid, no runtime dir, Wayland socket or session bus -- and no network
    # at all: the fixture server runs inside, on the container's own 127.0.0.1.
    proc = subprocess.run(
        ["docker", "run", "--rm", "--user", f"{uid}:{gid}", "--network", "none",
         "--mount", f"type=bind,src={scratch},dst={scratch}",
         "--mount", f"type=bind,src={HERE},dst={HERE},readonly",
         # The nvim plugin the installer lays out: the harness builds its fixture releases from it.
         "--mount", f"type=bind,src={PLUGIN},dst={PLUGIN},readonly",
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
    docs-codex-3) is standalone -- it stubs packaging/install.sh itself and needs no fixture
    machinery, so unlike harness.sh it is not sourced by anything else. It used to be run only by
    hand; wired in here so both shell halves this file already holds every other installer test to
    actually cover it too (review-2 finding). Its own scratch dir lives under a fresh HOME so its
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


def test_help_cites_no_internal_ruling_or_spec_section():
    """leaks-claude-1 (installer half, review2 Task 4): `install.sh --help` is what a stranger
    running the curl installer actually reads. It used to cite "spec §7" and "the owner's dev
    loop, D11" -- ruling ids and spec section markers that mean nothing outside this repo's own
    planning docs."""
    proc = subprocess.run(["sh", str(INSTALLER), "--help"], capture_output=True, text=True)
    assert proc.returncode == 0, report(proc)
    match = _RULING_CITATION_PATTERN.search(proc.stdout)
    assert match is None, (match and match.group(0), proc.stdout)


def test_desktop_exec_parses_back_with_glib():
    """The Exec line, read back by GLib (what GNOME's launcher uses), names the launcher exactly.

    GLib rejects a single `\\$` in a key file value outright, so this is the check that the
    string-value escaping layer (spec §6.5, the Desktop Entry Specification) is really applied.
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
    assert argv[1:] == ["--quiet", "%f"]
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
                      XDG_CACHE_HOME=str(home / "cch"))


@pytest.mark.parametrize("rel", [
    "data/licenses/eitri/LICENSE",
    "data/eitri/nvim/x",
    "data/applications/cn.huntergrey.eitri.desktop",
    "data/icons/hicolor/16x16/apps/cn.huntergrey.eitri.png",
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


def test_real_home_guard_leaves_the_default_places_alone_when_xdg_names_others():
    home = fresh_scratch("guard-xdg")
    before = real_home_state(home, _xdg_env(home))
    _write(home / ".local/share/eitri/sidecar/ccccccc/BUILD")
    _write(home / ".local/share/licenses/eitri/NEW")
    _write(home / ".cache/eitri/NEW")
    assert real_home_state(home, _xdg_env(home)) == before


def test_real_home_guard_ignores_a_relative_or_empty_xdg_value_as_install_sh_does():
    home = _fake_home("guard-xdg-relative")
    env = _guard_env(XDG_DATA_HOME="data", XDG_CACHE_HOME="", XDG_STATE_HOME="state")
    before = real_home_state(home, env)
    _write(home / ".local/share/eitri/sidecar/ccccccc/BUILD")
    _write(home / ".cache/eitri/NEW")
    assert real_home_state(home, env) != before
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
