#!/usr/bin/env python3
"""Layer 2 of the GTK 4.14 / WebKit 2.40 / glibc 2.39 toolkit floor (spec
docs/superpowers/specs/2026-09-27-v1-dist-design.md §2.2-2.3, plan Task 2). Layer 1 is
shell/tests/toolkit_floor.rs (the Cargo feature graph); layer 3 is the release container itself
(release.sh, Task 12) linking against Ubuntu 24.04's real libraries. This layer maps every
gtk_/gdk_/gsk_/webkit_/jsc_-prefixed symbol a binary imports (`nm -D --undefined-only`) back to the
`#[cfg(feature = "vX_Y")]` gate that guards it in the resolved `-sys` crate's own `src/lib.rs`
(located through `cargo metadata --locked`, so it reads exactly what built the binary under test),
the same method the research that set these floors used by hand -- and fails on anything above the
floor. It separately checks every `GLIBC_x.y` symbol version the binary references (`objdump -T`):
a *strong* (non-weak) reference above the floor fails; a *weak* one is only reported (glibc's own
convention for a symbol with a fallback -- the research found `pidfd_spawnp` weak @2.39).

Usage:
    packaging/check-abi-floor.py BIN... [--gtk 4.14] [--webkit 2.40] [--glibc 2.39]

Exit 0: one summary line per binary. Exit 1: every violation, then a summary line per binary.
"""

from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path

# The five `-sys` crates whose FFI declarations this check reads. gtk4-rs's `v4_N` convention
# covers the first three; WebKitGTK-rs's `v2_N` convention covers the last two. Which floor a
# symbol is checked against follows its own prefix (below), not this list -- this is only where
# `cargo metadata` looks for the source that gates it.
SYS_CRATES = ["gtk4-sys", "gdk4-sys", "gsk4-sys", "webkit6-sys", "javascriptcore6-sys"]

SYMBOL_PREFIX_RE = re.compile(r"^(gtk_|gdk_|gsk_|webkit_|jsc_)")
_CFG_LINE_RE = re.compile(r"^\s*#\[cfg\(")
_CFG_ATTR_DOCSRS_RE = re.compile(r"^\s*#\[cfg_attr\(docsrs")
# Matches the version feature whether it is the whole `cfg(...)`'s condition or one alternative
# inside a `cfg(any(feature = "vX_Y", ...))` (order-independent: `.search`, not anchored).
_CFG_VERSION_RE = re.compile(r'feature\s*=\s*"v(\d)_(\d+)"')
_FN_RE = re.compile(r"^\s*pub fn ([A-Za-z_][A-Za-z0-9_]*)\s*\(")
_GLIBC_REF_RE = re.compile(r"\(GLIBC_(\d+)\.(\d+)\)\s+(\S+)$")


class AbiFloorError(RuntimeError):
    """Raised for a hard failure (a tool missing or erroring) -- never for "the check found a
    violation", which is reported instead, not raised."""


@dataclass(frozen=True)
class Gate:
    """`crate` gates its FFI declaration of a symbol behind version feature `vMAJOR_MINOR`."""

    crate: str
    major: int
    minor: int

    def __str__(self) -> str:
        return f"v{self.major}_{self.minor} (from {self.crate})"


@dataclass(frozen=True)
class GlibcRef:
    name: str
    major: int
    minor: int
    weak: bool


def parse_version(text: str) -> tuple[int, int]:
    m = re.fullmatch(r"(\d+)\.(\d+)", text.strip())
    if not m:
        raise AbiFloorError(f"not a MAJOR.MINOR version: {text!r}")
    return int(m.group(1)), int(m.group(2))


def locate_sys_crate_lib_rs(workspace_root: Path) -> dict[str, Path]:
    """`cargo metadata --locked` resolves each `-sys` crate's source directory exactly as
    Cargo.lock pins it -- the same sources that produced the binary under test, not whatever
    version happens to be newest in the local registry cache."""
    result = subprocess.run(
        ["cargo", "metadata", "--format-version", "1", "--locked"],
        cwd=workspace_root,
        capture_output=True,
        text=True,
        check=False,
    )
    if result.returncode != 0:
        raise AbiFloorError(f"`cargo metadata --locked` failed (is Cargo.lock in sync?):\n{result.stderr}")
    metadata = json.loads(result.stdout)
    found: dict[str, Path] = {}
    for pkg in metadata["packages"]:
        if pkg["name"] in SYS_CRATES:
            found[pkg["name"]] = Path(pkg["manifest_path"]).parent / "src" / "lib.rs"
    missing = set(SYS_CRATES) - found.keys()
    if missing:
        raise AbiFloorError(
            f"cargo metadata did not resolve: {', '.join(sorted(missing))} "
            "(not in this workspace's dependency graph?)"
        )
    return found


def parse_symbol_gates(crate: str, lib_rs_text: str) -> dict[str, Gate]:
    """Maps every `pub fn <symbol>` in `lib_rs_text` that is directly preceded by a
    `#[cfg(feature = "vX_Y")]` (or `#[cfg(any(feature = "vX_Y", ...))]`) to that gate. A
    `#[cfg_attr(docsrs, doc(cfg(...)))]` line (rustdoc-only, always paired with a real `cfg` line
    in these crates) and a blank line are both skipped without losing the pending gate; any other
    line clears it. An *unguarded* `pub fn` -- this crate's baseline -- is deliberately left out of
    the map: absence here means "not known to need more than the floor", not "definitely fine"."""
    gates: dict[str, Gate] = {}
    pending: tuple[int, int] | None = None
    for line in lib_rs_text.splitlines():
        if _CFG_ATTR_DOCSRS_RE.match(line):
            continue
        if _CFG_LINE_RE.match(line):
            m = _CFG_VERSION_RE.search(line)
            if m:
                pending = (int(m.group(1)), int(m.group(2)))
            continue
        if not line.strip():
            continue
        m = _FN_RE.match(line)
        if m and pending is not None:
            major, minor = pending
            gates[m.group(1)] = Gate(crate, major, minor)
        pending = None
    return gates


def build_gate_map(workspace_root: Path) -> dict[str, Gate]:
    gates: dict[str, Gate] = {}
    for crate, lib_rs in locate_sys_crate_lib_rs(workspace_root).items():
        text = lib_rs.read_text(encoding="utf-8", errors="replace")
        gates.update(parse_symbol_gates(crate, text))
    return gates


def parse_nm_undefined(nm_output: str) -> list[str]:
    """`nm -D --undefined-only` prints `<blank-or-address> <type> <name>` per line; a versioned
    libc symbol carries `@GLIBC_x.y` on the name, which this check does not need (the glibc check
    reads `objdump -T` instead, which carries the version in its own dedicated field)."""
    symbols = []
    for raw in nm_output.splitlines():
        parts = raw.split()
        if len(parts) < 2:
            continue
        symbols.append(parts[-1].split("@", 1)[0])
    return symbols


def parse_objdump_glibc_refs(objdump_output: str) -> list[GlibcRef]:
    """`objdump -T` prints two tab-separated fields per symbol; the first carries the flags
    (weak is an isolated `w` token) and `*UND*` for an imported (undefined) reference, the second
    the `(GLIBC_x.y)` version and the symbol name. Only undefined references are imports this
    binary depends on; a defined, versioned export (this binary's own symbol) is not one."""
    refs = []
    for line in objdump_output.splitlines():
        if "GLIBC_" not in line or "*UND*" not in line:
            continue
        parts = line.split("\t", 1)
        if len(parts) != 2:
            continue
        flags_field, rest = parts
        m = _GLIBC_REF_RE.search(rest)
        if not m:
            continue
        weak = bool(re.search(r"\bw\b", flags_field))
        refs.append(GlibcRef(m.group(3), int(m.group(1)), int(m.group(2)), weak))
    return refs


def run_nm_undefined(binary: Path) -> str:
    result = subprocess.run(["nm", "-D", "--undefined-only", str(binary)], capture_output=True, text=True, check=False)
    if result.returncode != 0:
        raise AbiFloorError(f"`nm -D --undefined-only {binary}` failed:\n{result.stderr}")
    return result.stdout


def run_objdump_dynsyms(binary: Path) -> str:
    result = subprocess.run(["objdump", "-T", str(binary)], capture_output=True, text=True, check=False)
    if result.returncode != 0:
        raise AbiFloorError(f"`objdump -T {binary}` failed:\n{result.stderr}")
    return result.stdout


def gate_floor_violations(
    gates: dict[str, Gate], undefined_symbols: list[str], gtk_floor: tuple[int, int], webkit_floor: tuple[int, int]
) -> list[str]:
    """The floor a symbol is checked against follows the *symbol's own* prefix (gtk_/gdk_/gsk_ vs
    webkit_/jsc_), not which `-sys` crate happens to have declared it -- so this stays correct even
    for a symbol resolved through a crate name this script doesn't otherwise special-case."""
    violations = []
    for sym in undefined_symbols:
        m = SYMBOL_PREFIX_RE.match(sym)
        if not m:
            continue
        gate = gates.get(sym)
        if gate is None:
            continue
        floor = gtk_floor if m.group(1) in ("gtk_", "gdk_", "gsk_") else webkit_floor
        if (gate.major, gate.minor) > floor:
            violations.append(f"{sym} needs {gate}, floor is v{floor[0]}_{floor[1]}")
    return violations


def glibc_floor_report(refs: list[GlibcRef], floor: tuple[int, int]) -> tuple[list[str], list[str]]:
    """`(failures, notes)`: a strong reference above `floor` fails; a weak one above `floor` is
    only noted."""
    failures = []
    notes = []
    for ref in refs:
        if (ref.major, ref.minor) <= floor:
            continue
        label = f"{ref.name}@GLIBC_{ref.major}.{ref.minor}"
        (notes if ref.weak else failures).append(label)
    return failures, notes


def check_binary(
    binary: Path,
    gates: dict[str, Gate],
    gtk_floor: tuple[int, int],
    webkit_floor: tuple[int, int],
    glibc_floor: tuple[int, int],
) -> tuple[bool, str]:
    undefined = parse_nm_undefined(run_nm_undefined(binary))
    gate_violations = gate_floor_violations(gates, undefined, gtk_floor, webkit_floor)
    glibc_refs = parse_objdump_glibc_refs(run_objdump_dynsyms(binary))
    glibc_failures, glibc_notes = glibc_floor_report(glibc_refs, glibc_floor)

    lines = [f"{binary}: FAIL {v}" for v in gate_violations]
    lines += [f"{binary}: FAIL glibc {f}, floor is {glibc_floor[0]}.{glibc_floor[1]}" for f in glibc_failures]

    ok = not gate_violations and not glibc_failures
    summary = (
        f"{binary}: OK (gtk<=v4_{gtk_floor[1]}, webkit<=v2_{webkit_floor[1]}, "
        f"glibc<={glibc_floor[0]}.{glibc_floor[1]})"
        if ok
        else f"{binary}: FAIL ({len(gate_violations)} gtk/webkit, {len(glibc_failures)} glibc violation(s))"
    )
    if glibc_notes:
        summary += f"; weak glibc above floor, not failed: {', '.join(glibc_notes)}"
    lines.append(summary)
    return ok, "\n".join(lines)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("binaries", nargs="+", type=Path)
    parser.add_argument("--gtk", default="4.14", help="floor for gtk4/gdk4/gsk4 (default: 4.14)")
    parser.add_argument("--webkit", default="2.40", help="floor for webkit6/javascriptcore6 (default: 2.40)")
    parser.add_argument("--glibc", default="2.39", help="floor for GLIBC symbol versions (default: 2.39)")
    parser.add_argument(
        "--workspace-root",
        type=Path,
        default=Path(__file__).resolve().parent.parent,
        help="passed to `cargo metadata` to locate the -sys crates (default: this script's own workspace)",
    )
    args = parser.parse_args(argv)

    try:
        gtk_floor = parse_version(args.gtk)
        webkit_floor = parse_version(args.webkit)
        glibc_floor = parse_version(args.glibc)
        gates = build_gate_map(args.workspace_root)

        overall_ok = True
        for binary in args.binaries:
            if not binary.exists():
                print(f"{binary}: FAIL no such file", file=sys.stderr)
                overall_ok = False
                continue
            ok, report = check_binary(binary, gates, gtk_floor, webkit_floor, glibc_floor)
            print(report)
            overall_ok = overall_ok and ok
    except AbiFloorError as e:
        print(f"check-abi-floor: {e}", file=sys.stderr)
        return 2

    return 0 if overall_ok else 1


if __name__ == "__main__":
    sys.exit(main())
