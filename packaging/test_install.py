"""packaging/install.sh's tests (plan 2026-09-27-v1-dist, Task 9; spec §6).

The tests themselves are shell functions in packaging/tests/install/test_*.sh, run by harness.sh in
two shells: bash on the host, and dash inside an Ubuntu 24.04 image (Dockerfile.dash) -- this host
has no dash and must not install one. This file runs both halves, shellcheck, and guards the real
HOME's Eitri directories around the container run (inside it they are not visible; on the host
run-in-env.sh guards every single installer run).

Scratch lives under ~/.cache/nv-v1dist-t9/<pid> (never /tmp) and is removed after a passing half. The
per-run directory matters: two runs from different worktrees at once used to share one directory,
and each one's fresh_scratch() deleted the other's stubs mid-run (run-in-env.sh then refused, rightly).
"""

import os
import pathlib
import re
import shutil
import subprocess

import pytest

HERE = pathlib.Path(__file__).resolve().parent
INSTALLER = HERE / "install.sh"
TESTS = HERE / "tests" / "install"
HARNESS = TESTS / "harness.sh"
ROOT_WRAPPER_TEST = TESTS / "test_root_wrapper.sh"
ROOT_INSTALLER = HERE.parent / "install.sh"
SCRATCH_ROOT = pathlib.Path.home() / ".cache" / "nv-v1dist-t9" / str(os.getpid())
IMAGE = "eitri-install-dash"


def real_home_state():
    """Every path and mtime under the real HOME's Eitri directories (never file contents)."""
    home = pathlib.Path.home()
    state = []
    for rel in (".local/state/eitri", ".config/eitri", ".local/share/eitri"):
        root = home / rel
        if not os.path.lexists(root):
            state.append(("absent", str(root)))
            continue
        for dirpath, dirnames, filenames in os.walk(root):
            for name in [dirpath] + [os.path.join(dirpath, n) for n in dirnames + filenames]:
                st = os.lstat(name)
                state.append((name, st.st_mtime_ns, st.st_size))
    return sorted(state, key=repr)


@pytest.fixture(scope="session", autouse=True)
def _remove_empty_scratch_root():
    """Leave no empty per-run directory behind (a failed half keeps its own scratch inside it)."""
    yield
    try:
        SCRATCH_ROOT.rmdir()
    except OSError:
        pass


def fresh_scratch(name):
    path = SCRATCH_ROOT / name
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


def test_harness_shellcheck_clean():
    if shutil.which("shellcheck") is None:
        pytest.skip("shellcheck is not installed")
    files = [str(p) for p in sorted(TESTS.glob("*.sh"))]
    files += [str(p) for p in sorted((TESTS / "fixtures").iterdir()) if p.name != "old-install-sh-launcher"]
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
    assert real_home_state() == before, "the real HOME's Eitri directories changed"
    assert proc.returncode == 0, report(proc)
    assert "# installer shell: bash" in proc.stdout
    assert " 0 failed" in proc.stdout, report(proc)
    shutil.rmtree(scratch, ignore_errors=True)


def docker_usable():
    if shutil.which("docker") is None:
        return False
    return subprocess.run(["docker", "info"], capture_output=True).returncode == 0


def test_harness_under_dash_in_ubuntu():
    if not docker_usable():
        pytest.skip("docker is not usable here; the dash half needs the Ubuntu 24.04 image")
    uid, gid = os.getuid(), os.getgid()
    # The image build is the only step that needs a network (apt-get, inside the image). A proxy the
    # caller exports is passed on as Docker's predefined proxy build args, which do not enter the
    # layer cache key.
    proxy_args = []
    for name in ("http_proxy", "https_proxy", "HTTP_PROXY", "HTTPS_PROXY", "no_proxy", "NO_PROXY"):
        if os.environ.get(name):
            proxy_args += ["--build-arg", f"{name}={os.environ[name]}"]
    build = subprocess.run(
        ["docker", "build", "-q", *proxy_args, "--build-arg", f"UID={uid}", "--build-arg", f"GID={gid}",
         "-t", IMAGE, "-f", str(TESTS / "Dockerfile.dash"), str(TESTS)],
        capture_output=True, text=True,
    )
    assert build.returncode == 0, report(build)

    scratch = fresh_scratch("dash")
    before = real_home_state()
    # Container hygiene (spec §2.4): bind mounts of existing host paths only (--mount errors on a
    # missing one), the host's uid, no runtime dir, Wayland socket or session bus -- and no network
    # at all: the fixture server runs inside, on the container's own 127.0.0.1.
    proc = subprocess.run(
        ["docker", "run", "--rm", "--user", f"{uid}:{gid}", "--network", "none",
         "--mount", f"type=bind,src={scratch},dst={scratch}",
         "--mount", f"type=bind,src={HERE},dst={HERE},readonly",
         IMAGE, "/bin/sh", str(HARNESS), "--sh", "/bin/sh", "--scratch", str(scratch)],
        capture_output=True, text=True,
    )
    assert real_home_state() == before, "the real HOME's Eitri directories changed"
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
    proxy_args = []
    for name in ("http_proxy", "https_proxy", "HTTP_PROXY", "HTTPS_PROXY", "no_proxy", "NO_PROXY"):
        if os.environ.get(name):
            proxy_args += ["--build-arg", f"{name}={os.environ[name]}"]
    build = subprocess.run(
        ["docker", "build", "-q", *proxy_args, "--build-arg", f"UID={uid}", "--build-arg", f"GID={gid}",
         "-t", IMAGE, "-f", str(TESTS / "Dockerfile.dash"), str(TESTS)],
        capture_output=True, text=True,
    )
    assert build.returncode == 0, report(build)

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
         IMAGE, "/bin/sh", str(ROOT_WRAPPER_TEST)],
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
    desktop = pathlib.Path(home) / ".local/share/applications/eitri.desktop"
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
