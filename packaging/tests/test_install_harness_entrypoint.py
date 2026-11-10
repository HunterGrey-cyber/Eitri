"""The no-argument harness runner preserves isolation and the child result."""

import os
from pathlib import Path
import subprocess

import pytest


HARNESS = Path(__file__).parent / "install/harness.sh"


@pytest.mark.parametrize("result", [0, 23])
def test_bare_harness_runs_with_private_scratch_and_real_home_guard(tmp_path, result):
    home = tmp_path / "home with spaces"
    home.mkdir()
    stubs = tmp_path / "stubs"
    stubs.mkdir()
    child = stubs / "sh"
    child.write_text(
        '#!/bin/sh\n'
        'printf "%s\\n" "$@" > "$HOME/arguments"\n'
        'test -d "$5" || exit 90\n'
        'test "$(stat -c %a "$5")" = 700 || exit 91\n'
        'touch "$5/evidence"\n'
        f'exit {result}\n'
    )
    child.chmod(0o755)
    proc = subprocess.run(
        ["/bin/sh", str(HARNESS.resolve())],
        env={**os.environ, "HOME": str(home), "PATH": f"{stubs}:{os.environ['PATH']}"},
        capture_output=True, text=True, timeout=30,
    )
    assert proc.returncode == result, proc.stdout + proc.stderr
    args = (home / "arguments").read_text().splitlines()
    assert args[:4] == [str(HARNESS.resolve()), "--sh", "bash", "--scratch"]
    scratch = Path(args[4])
    assert scratch.parent == home / ".cache/nv-v1dist-t9"
    assert args[5:] == ["--real-home", str(home)]
    assert scratch.exists() == (result != 0)
    if result:
        assert (scratch / "evidence").exists()
        assert str(scratch) in proc.stderr


def test_partial_arguments_still_require_explicit_scratch():
    proc = subprocess.run(
        ["sh", str(HARNESS), "--sh", "bash"],
        capture_output=True, text=True, timeout=30,
    )
    assert proc.returncode == 2
    assert "--sh and --scratch are required" in proc.stderr
