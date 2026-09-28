/// <reference types="vite/client" />
/** Fitness function F10 (2026-09-27 Codex audit P6, `the private review notes`
 *  appendix, verdict SUPPORTED in `the private review notes`):
 *  "After v1, keys are only ever added, never changed in meaning"
 *  (`docs/superpowers/specs/2026-09-27-v1-decisions.md`, top paragraph). Two halves, matching
 *  `core/tests/keymap_v1_freeze.rs` on the Rust side:
 *
 *  1. `resolveKey` (`./keymap`) -- the panel's fixed single/two-key BROWSE and INPUT table. The
 *     candidate keyboard events below are hand-picked, one per semantically distinct row of
 *     `BROWSE_KEYS`/`INPUT_KEYS` (plus the session/turn-state branches that change what a row does),
 *     and are themselves part of the test, not generated data -- only the *expected action* each one
 *     resolves to is a frozen snapshot, so a text-only edit to `BROWSE_KEYS`'s descriptions can never
 *     silently change what this file checks.
 *  2. The panel's leader/which-key table -- `core::keymap::panel::default_bindings()`, shared with
 *     the Rust test via ONE fixture, `docs/keymap/v1-frozen-panel-keys.json` (its `keys: string[]`
 *     shape already matches `PanelBinding.keys` on the wire), walked through `./leader`'s
 *     `startSequence`/`advanceSequence` (or, for the four sequences that start with one of
 *     `resolveKey`'s own reserved two-key prefixes -- `[b`, `]b`, `gt`, `gT` -- through `resolveKey`
 *     itself, the same two-step a real keypress takes).
 *
 *  Regenerate fixture 1 only (fixture 2 is Rust's to regenerate, see `keymap_v1_freeze.rs`'s own
 *  header -- this file only reads it): `REGENERATE_V1_RESOLVE_KEY_FIXTURE=1 npx vitest run -u
 *  src/keymap.v1Freeze.test.ts` (both are needed: the variable selects the regenerating case, `-u` lets
 *  vitest write the file), then read the diff by hand against
 *  `docs/superpowers/specs/2026-09-27-v1-decisions.md` before committing -- this test cannot tell an
 *  intended change from a regression it should have caught.
 */
import { describe, expect, it } from "vitest";
// Read through Vite's `?raw` like the other tests here, not `node:fs`: the panel's `tsc -b` has no
// Node types, and shell/build.rs runs that build.
import resolveKeyFixtureText from "./fixtures/v1-frozen-resolve-key.json?raw";
// Shared with core/tests/keymap_v1_freeze.rs -- one file, one freeze, for both languages.
import panelFixtureText from "../../../docs/keymap/v1-frozen-panel-keys.json?raw";
import { resolveKey } from "./keymap";
import type { KeyLike, PanelAction, PanelBinding, PanelTable, PendingPrefix } from "./keymap";
import { advanceSequence, startSequence } from "./leader";


const key = (k: string, over: Partial<{ ctrlKey: boolean; shiftKey: boolean }> = {}): KeyLike => ({
  key: k,
  ctrlKey: false,
  shiftKey: false,
  isComposing: false,
  ...over,
});

type ResolveKeyCase = {
  label: string;
  mode: "browse" | "input";
  event: KeyLike;
  ctx: { sessionEnded: boolean; turnRunning?: boolean; pending?: PendingPrefix };
};

/** One candidate per semantically distinct `resolveKey` row (`BROWSE_KEYS`/`INPUT_KEYS`), plus the
 *  session/turn-state branch for every row whose result depends on one. This list is the test, not
 *  generated output -- see this file's own header. */
const RESOLVE_KEY_CASES: ResolveKeyCase[] = [
  { label: "j", mode: "browse", event: key("j"), ctx: { sessionEnded: false } },
  { label: "k", mode: "browse", event: key("k"), ctx: { sessionEnded: false } },
  { label: "h", mode: "browse", event: key("h"), ctx: { sessionEnded: false } },
  { label: "l", mode: "browse", event: key("l"), ctx: { sessionEnded: false } },
  { label: "gg", mode: "browse", event: key("g"), ctx: { sessionEnded: false, pending: "g" } },
  { label: "Shift+G", mode: "browse", event: key("G", { shiftKey: true }), ctx: { sessionEnded: false } },
  { label: "gf", mode: "browse", event: key("f"), ctx: { sessionEnded: false, pending: "g" } },
  { label: "1-9 (a digit)", mode: "browse", event: key("3"), ctx: { sessionEnded: false } },
  { label: "[[", mode: "browse", event: key("["), ctx: { sessionEnded: false, pending: "[" } },
  { label: "]]", mode: "browse", event: key("]"), ctx: { sessionEnded: false, pending: "]" } },
  { label: "Ctrl+d", mode: "browse", event: key("d", { ctrlKey: true }), ctx: { sessionEnded: false } },
  { label: "Ctrl+u", mode: "browse", event: key("u", { ctrlKey: true }), ctx: { sessionEnded: false } },
  { label: "Ctrl+c, idle", mode: "browse", event: key("c", { ctrlKey: true }), ctx: { sessionEnded: false, turnRunning: false } },
  { label: "Ctrl+c, a turn is running", mode: "browse", event: key("c", { ctrlKey: true }), ctx: { sessionEnded: false, turnRunning: true } },
  { label: "a, a live session", mode: "browse", event: key("a"), ctx: { sessionEnded: false } },
  { label: "a, a session that ended", mode: "browse", event: key("a"), ctx: { sessionEnded: true } },
  { label: "d, a live session", mode: "browse", event: key("d"), ctx: { sessionEnded: false } },
  { label: "d, a session that ended", mode: "browse", event: key("d"), ctx: { sessionEnded: true } },
  { label: "Enter", mode: "browse", event: key("Enter"), ctx: { sessionEnded: false } },
  { label: "y", mode: "browse", event: key("y"), ctx: { sessionEnded: false } },
  { label: "Shift+Y", mode: "browse", event: key("Y", { shiftKey: true }), ctx: { sessionEnded: false } },
  { label: "Shift+D, a live session", mode: "browse", event: key("D", { shiftKey: true }), ctx: { sessionEnded: false } },
  { label: "Shift+D, a session that ended", mode: "browse", event: key("D", { shiftKey: true }), ctx: { sessionEnded: true } },
  { label: "i, a live session", mode: "browse", event: key("i"), ctx: { sessionEnded: false } },
  { label: "i, a session that ended", mode: "browse", event: key("i"), ctx: { sessionEnded: true } },
  { label: "o, a live session", mode: "browse", event: key("o"), ctx: { sessionEnded: false } },
  { label: "o, a session that ended", mode: "browse", event: key("o"), ctx: { sessionEnded: true } },
  { label: "Shift+A, a live session", mode: "browse", event: key("A", { shiftKey: true }), ctx: { sessionEnded: false } },
  { label: "Shift+A, a session that ended", mode: "browse", event: key("A", { shiftKey: true }), ctx: { sessionEnded: true } },
  { label: "f", mode: "browse", event: key("f"), ctx: { sessionEnded: false } },
  { label: "r, a live session", mode: "browse", event: key("r"), ctx: { sessionEnded: false } },
  { label: "r, a session that ended", mode: "browse", event: key("r"), ctx: { sessionEnded: true } },
  { label: "/", mode: "browse", event: key("/"), ctx: { sessionEnded: false } },
  { label: "n", mode: "browse", event: key("n"), ctx: { sessionEnded: false } },
  { label: "Shift+N", mode: "browse", event: key("N", { shiftKey: true }), ctx: { sessionEnded: false } },
  { label: "Ctrl+o, browse", mode: "browse", event: key("o", { ctrlKey: true }), ctx: { sessionEnded: false } },
  { label: "Ctrl+o, input", mode: "input", event: key("o", { ctrlKey: true }), ctx: { sessionEnded: false } },
  { label: "zh", mode: "browse", event: key("h"), ctx: { sessionEnded: false, pending: "z" } },
  { label: "zl", mode: "browse", event: key("l"), ctx: { sessionEnded: false, pending: "z" } },
  { label: "Ctrl+g, browse", mode: "browse", event: key("g", { ctrlKey: true }), ctx: { sessionEnded: false } },
  { label: "?", mode: "browse", event: key("?", { shiftKey: true }), ctx: { sessionEnded: false } },
  { label: "Escape, browse, idle", mode: "browse", event: key("Escape"), ctx: { sessionEnded: false, turnRunning: false } },
  { label: "Escape, browse, a turn is running", mode: "browse", event: key("Escape"), ctx: { sessionEnded: false, turnRunning: true } },
  { label: "Escape, input", mode: "input", event: key("Escape"), ctx: { sessionEnded: false } },
];

function readJson<T>(text: string): T {
  return JSON.parse(text) as T;
}

if (import.meta.env.REGENERATE_V1_RESOLVE_KEY_FIXTURE) {
  it("regenerates the resolveKey fixture (REGENERATE_V1_RESOLVE_KEY_FIXTURE=1 only)", async () => {
    const snapshot = RESOLVE_KEY_CASES.map((c) => ({
      label: c.label,
      expected: resolveKey(c.mode, c.event, c.ctx),
    }));
    await expect(`${JSON.stringify(snapshot, null, 2)}\n`).toMatchFileSnapshot("./fixtures/v1-frozen-resolve-key.json");
    expect(snapshot.length).toBe(RESOLVE_KEY_CASES.length);
  });
} else {
  describe("resolveKey keeps every pinned v1 key (F10)", () => {
    const fixture = readJson<{ label: string; expected: PanelAction }[]>(resolveKeyFixtureText);
    const byLabel = new Map(fixture.map((row) => [row.label, row.expected]));

    it("the fixture covers exactly today's candidate cases (no silent addition/removal)", () => {
      expect(new Set(fixture.map((r) => r.label))).toEqual(new Set(RESOLVE_KEY_CASES.map((c) => c.label)));
    });

    for (const c of RESOLVE_KEY_CASES) {
      it(`${c.label} still resolves the same as it did at the v1 freeze`, () => {
        expect(byLabel.has(c.label), `no fixture row for ${JSON.stringify(c.label)}`).toBe(true);
        const actual = resolveKey(c.mode, c.event, c.ctx);
        expect(actual, "docs/superpowers/specs/2026-09-27-v1-decisions.md: \"keys are only ever added, never changed in meaning\"").toEqual(
          byLabel.get(c.label),
        );
      });
    }
  });
}

describe("the panel/leader default table keeps every pinned v1 binding (F10)", () => {
  const fixture = readJson<{ keys: string[]; action: string }[]>(panelFixtureText);

  // A synthetic PanelTable built from the shared fixture: `desc`/`source` are irrelevant to what
  // `startSequence`/`advanceSequence`/`resolveKey`'s tableHit lookup do, so they are filled with
  // placeholders rather than duplicated data.
  const bindings: PanelBinding[] = fixture.map((row) => ({
    keys: row.keys,
    action: row.action as PanelBinding["action"],
    desc: row.action,
    source: "default",
  }));
  const table: PanelTable = {
    leader: " ",
    leaderLabel: "Space",
    leaderSource: "default",
    timeoutlen: 300,
    timeout: true,
    bindings,
    groups: [],
  };

  /** Walks one fixture row's `keys` through whichever engine a real keypress would use for it: the
   *  which-key engine (`./leader`) for everything else, or `resolveKey`'s own reserved two-key
   *  prefix machinery (`resolveKey`'s own `g`/`z`/`[`/`]` pending switch, then its `tableHit` lookup)
   *  for the four sequences that start with one of those reserved keys -- `startSequence` refuses to
   *  start a sequence on them by design (spec §2.4), so the leader engine alone cannot reach them. */
  function resolveRow(row: { keys: string[]; action: string }): PanelAction | null {
    const first = row.keys[0];
    if (first === "g" || first === "z" || first === "[" || first === "]") {
      expect(row.keys.length, `${row.keys.join(" ")}: this test only knows the two-key reserved-prefix shape`).toBe(2);
      const pendingStep = resolveKey("browse", key(first), { sessionEnded: false });
      expect(pendingStep, `${row.keys.join(" ")}: the first key should arm a pending prefix`).toEqual({ kind: "pending", prefix: first });
      // A real keypress for an uppercase second key (`gT`) carries `shiftKey: true` alongside the
      // already-uppercase `.key`, redundant as that is -- match it, so this synthetic event is not
      // more permissive than what a browser actually sends (minor finding, 2026-09-28 fix round).
      const secondKey = row.keys[1];
      const second = resolveKey("browse", key(secondKey, { shiftKey: secondKey !== secondKey.toLowerCase() }), {
        sessionEnded: false,
        pending: first as PendingPrefix,
        table,
      });
      return second;
    }
    const onControl = false;
    let step = startSequence(table, first === "<leader>" ? table.leader : first, onControl);
    for (const nextKey of row.keys.slice(1)) {
      if (step.kind !== "pending") break;
      step = advanceSequence(table, step.typed, nextKey);
    }
    return step.kind === "run" ? { kind: "panel", binding: step.binding } : null;
  }

  it("covers a non-empty, duplicate-free set of bindings", () => {
    expect(fixture.length).toBeGreaterThan(0);
    expect(new Set(fixture.map((r) => r.keys.join(" "))).size).toBe(fixture.length);
  });

  for (const row of fixture) {
    const human = row.keys.join(" ");
    it(`${human} still resolves to ${row.action}`, () => {
      const result = resolveRow(row);
      expect(
        result,
        `docs/superpowers/specs/2026-09-27-v1-decisions.md: "keys are only ever added, never changed in meaning"`,
      ).toEqual({ kind: "panel", binding: expect.objectContaining({ keys: row.keys, action: row.action }) });
    });
  }
});
