#!/usr/bin/env python3
"""Exhaustive mutation sweep over SyncSpy's Handler forwards.

THE BAR IS EXHAUSTIVE MUTATION, NOT HAND-PICKED MUTANTS. The previous prototype's author
hand-picked six mutants and all six happened to land inside the 26 that die; a mechanical sweep
of the same 68 cleanly-mutable forwards found 42 that SURVIVED GREEN.

This script enumerates the mutation space MECHANICALLY by brace-matching the
`impl ... Handler for SyncSpy` block, so nothing is picked by hand and nothing is mangled by a
naive regex (the last method in the block carries the closing brace of the impl; the two
intercepted methods are multi-line).

Three operators per method:
  delete_method   -- remove the whole `fn` item. The real defect: the trait's silent no-op
                     default takes over and a forgotten forward compiles cleanly.
  delete_forward  -- remove only the `self.inner.<name>(..)` statement.
  delete_signal   -- remove only the barrier notification (for the two intercepted methods,
                     the whole `if .. SyncUpdate .. {} else {}` block).

KILLED  = the test command failed on the mutant (good).
SURVIVED= the test command still passed (a hole in the net).

Usage: python3 scripts/mutation_sweep.py [--test-args ...]
"""

import argparse
import pathlib
import re
import shutil
import subprocess
import sys
import time

CRATE = pathlib.Path(__file__).resolve().parent.parent
SPY = CRATE / "src" / "spy.rs"
IMPL_RE = re.compile(r"^impl<H: Handler \+ ZeroWidthTarget> Handler for SyncSpy<'_, H> \{$", re.M)


def find_impl_block(text):
    m = IMPL_RE.search(text)
    if not m:
        sys.exit("mutation_sweep: could not find the `impl .. Handler for SyncSpy` block")
    start = m.end()  # just after the opening brace
    depth = 1
    i = start
    while i < len(text):
        c = text[i]
        if c == "{":
            depth += 1
        elif c == "}":
            depth -= 1
            if depth == 0:
                return start, i
        i += 1
    sys.exit("mutation_sweep: unterminated impl block")


def find_methods(text, start, end):
    """Return [(name, item_start, item_end)] for every `fn` item in the impl block."""
    out = []
    i = start
    body = text
    while True:
        m = re.compile(r"^(\s*)fn (\w+)\(", re.M).search(body, i, end)
        if not m:
            return out
        name = m.group(2)
        # Find the method's opening brace, then brace-match to its end.
        j = body.index("{", m.end())
        depth = 1
        k = j + 1
        while depth:
            if body[k] == "{":
                depth += 1
            elif body[k] == "}":
                depth -= 1
            k += 1
        line_start = body.rfind("\n", 0, m.start()) + 1
        # Swallow the trailing newline so deletion leaves no blank line.
        item_end = k + 1 if k < len(body) and body[k] == "\n" else k
        out.append((name, line_start, item_end))
        i = k


def mutate(text, name, s, e, op):
    item = text[s:e]
    if op == "delete_method":
        return text[:s] + text[e:]
    if op == "delete_forward":
        pat = re.compile(r"[ \t]*self\.inner\.%s\([^;]*\);\n?" % re.escape(name))
        new_item, n = pat.subn("", item)
        if n != 1:
            return None
        return text[:s] + new_item + text[e:]
    if op == "delete_signal":
        pat = re.compile(r"[ \t]*self\.barrier\.note_dispatch\(\);\n?")
        new_item, n = pat.subn("", item)
        if n == 1:
            return text[:s] + new_item + text[e:]
        # Intercepted methods: drop the whole if/else that drives the barrier.
        pat = re.compile(
            r"[ \t]*if a0 == PrivateMode::Named\(NamedPrivateMode::SyncUpdate\) \{.*?\n[ \t]*\}\n",
            re.S,
        )
        new_item, n = pat.subn("", item)
        if n != 1:
            return None
        return text[:s] + new_item + text[e:]
    raise AssertionError(op)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--ops", default="delete_method,delete_forward,delete_signal")
    ap.add_argument(
        "--test",
        default="cargo test --offline --test forwarding",
        help="command whose FAILURE means the mutant was killed",
    )
    args = ap.parse_args()
    ops = args.ops.split(",")

    original = SPY.read_text()
    backup = SPY.with_suffix(".rs.sweep-backup")
    shutil.copy2(SPY, backup)

    s, e = find_impl_block(original)
    methods = find_methods(original, s, e)
    print(f"mutation space: {len(methods)} methods x {len(ops)} operators = "
          f"{len(methods) * len(ops)} mutants")
    print(f"test command  : {args.test}")

    # Baseline: the unmutated tree must PASS, or every 'killed' below is meaningless.
    base = subprocess.run(args.test, shell=True, cwd=CRATE, capture_output=True, text=True)
    if base.returncode != 0:
        SPY.write_text(original)
        sys.exit("mutation_sweep: BASELINE FAILS -- fix the tree before sweeping\n" + base.stdout[-2000:])
    print("baseline      : PASS\n")

    results = []
    t0 = time.time()
    try:
        for name, ms, me in methods:
            for op in ops:
                mutant = mutate(original, name, ms, me, op)
                if mutant is None:
                    results.append((name, op, "NOT_APPLICABLE"))
                    print(f"  {op:<15} {name:<28} NOT_APPLICABLE")
                    continue
                SPY.write_text(mutant)
                r = subprocess.run(args.test, shell=True, cwd=CRATE,
                                   capture_output=True, text=True)
                combined = r.stdout + r.stderr
                if r.returncode == 0:
                    verdict = "SURVIVED"
                elif "could not compile" in combined or re.search(r"^error\[E\d+\]", combined, re.M):
                    # NB: a KILLED mutant also prints `error: test failed, to rerun ..`, so the
                    # compile check must be narrower than /^error/.
                    verdict = "DID_NOT_COMPILE"
                else:
                    verdict = "KILLED"
                results.append((name, op, verdict))
                print(f"  {op:<15} {name:<28} {verdict}")
    finally:
        SPY.write_text(original)
        backup.unlink(missing_ok=True)

    elapsed = time.time() - t0
    print(f"\nswept {len(results)} mutants in {elapsed:.0f}s")
    for op in ops:
        sub = [r for r in results if r[1] == op]
        killed = [r for r in sub if r[2] == "KILLED"]
        survived = [r for r in sub if r[2] == "SURVIVED"]
        broken = [r for r in sub if r[2] == "DID_NOT_COMPILE"]
        na = [r for r in sub if r[2] == "NOT_APPLICABLE"]
        print(f"\n{op}: total={len(sub)} killed={len(killed)} survived={len(survived)} "
              f"did_not_compile={len(broken)} not_applicable={len(na)}")
        for r in survived:
            print(f"    SURVIVOR: {r[0]}")
        for r in broken:
            print(f"    DID_NOT_COMPILE: {r[0]}")
        for r in na:
            print(f"    NOT_APPLICABLE: {r[0]}")

    total_survived = sum(1 for r in results if r[2] == "SURVIVED")
    print(f"\nTOTAL: {len(results)} mutants, "
          f"{sum(1 for r in results if r[2] == 'KILLED')} killed, "
          f"{total_survived} survived")
    sys.exit(1 if total_survived else 0)


if __name__ == "__main__":
    main()
