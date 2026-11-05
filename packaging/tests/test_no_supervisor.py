"""The retired dashboard is absent from current sources and package contents."""

from pathlib import Path
import subprocess

import pytest

ROOT = Path(__file__).resolve().parents[2]


def no_source_names_the_supervisor():
    name = "eitri-" + "supervisor"
    if subprocess.run(
        ["git", "rev-parse", "--git-dir"], cwd=ROOT, capture_output=True
    ).returncode:
        pytest.skip("not a git checkout: git grep has nothing to read")
    result = subprocess.run(
        ["git", "grep", "-n", name], cwd=ROOT, capture_output=True, text=True
    )
    assert result.returncode in (0, 1), result.stderr
    hits = []
    for hit in result.stdout.splitlines():
        path, _, line = hit.split(":", 2)
        if path.startswith(("docs/canonical/", "docs/superpowers/")):
            continue
        if path in (
            "packaging/tests/install/fixtures/install-0.2.0.sh",
            "packaging/release-notes.md.in",
            # The placeholder 0.2.0's installer wants in the tarball (packaging/legacy/README.md).
            "packaging/legacy/README.md",
            "docs/rules/distribution.md",
            "packaging/release.sh",
            "packaging/release_check.py",
            "packaging/tests/install/harness.sh",
            "packaging/tests/test_release_layout.py",
            "packaging/tests/test_release_sh.py",
        ):
            continue
        if path == "packaging/install.sh" and line.startswith("NV_OLD_BINARIES="):
            continue
        hits.append(hit)
    assert not hits, "Retired dashboard references remain:\n" + "\n".join(hits)


test_no_source_names_the_supervisor = no_source_names_the_supervisor
