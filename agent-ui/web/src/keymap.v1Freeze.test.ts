/// <reference types="vite/client" />
/** Freeze existing key meanings while allowing new bindings.
 *  `resolveKey` cases cover the fixed BROWSE/INPUT table and state-dependent actions.
 *  Events are hand-picked; only expected actions are snapshots, so help-text edits cannot
 *  silently change coverage.
 *
 *  The leader table shares `docs/keymap/v1-frozen-panel-keys.json` with the Rust test.
 *  It is exercised through `startSequence`/`advanceSequence`, or through `resolveKey`
 *  for reserved-prefix sequences [b, ]b, gt and gT.
 *
 *  Regenerate only the resolve-key fixture here with
 *  `REGENERATE_V1_RESOLVE_KEY_FIXTURE=1 npx vitest run -u src/keymap.v1Freeze.test.ts`.
 *  The variable selects regeneration and -u allows the fixture to be written.
 *  The Rust test regenerates the panel fixture. Inspect changes by hand because snapshots
 *  cannot distinguish intended changes from regressions. */
import { describe, expect, it } from "vitest";
// Read through Vite's `?raw` like the other tests here, not `node:fs`: the panel's `tsc -b` has no
// Node types, and shell/build.rs runs that build.
import resolveKeyFixtureText from "./fixtures/v1-frozen-resolve-key.json?raw";
// Shared with core/tests/keymap_v1_freeze.rs -- one file, one freeze, for both languages.
import panelFixtureText from "../../../docs/keymap/v1-frozen-panel-keys.json?raw";
import { CARET_KEYS, resolveKey, VISUAL_KEYS } from "./keymap";
import type { KeyLike, PanelAction, PanelBinding, PanelTable, PendingPrefix } from "./keymap";
import { advanceSequence, startSequence } from "./leader";


const key = (k: string, over: Partial<{ ctrlKey: boolean; shiftKey: boolean }> = {}): KeyLike => ({
  key: k,
  ctrlKey: false,
  shiftKey: false,
  isComposing: false,
  ...over,
});

/** `CARET_KEYS`/`VISUAL_KEYS`' own token spelling -> a `KeyLike`, the same rule `keymap.test.ts`'s
 *  `parseVisualToken` uses: `"Esc"` is the display spelling of the real `key: "Escape"`, `"1-9"` a
 *  representative digit, `"gg"` (added for 3a) the FIRST of its two presses (a non-`pending` case
 *  below covers the second), and every bare uppercase letter or `?` carries Shift. */
function visualToken(token: string): KeyLike {
  if (token === "Esc") return key("Escape");
  if (token === "1-9") return key("3");
  if (token === "gg") return key("g");
  if (/^[A-Z?]$/.test(token)) return key(token, { shiftKey: true });
  return key(token);
}

type ResolveKeyCase = {
  label: string;
  mode: "browse" | "input" | "caret" | "visual" | "vline";
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
  // Append new binding cases so regenerated fixture diffs show only additions.
  { label: "Ctrl+e", mode: "browse", event: key("e", { ctrlKey: true }), ctx: { sessionEnded: false } },
  { label: "Ctrl+y", mode: "browse", event: key("y", { ctrlKey: true }), ctx: { sessionEnded: false } },
  // Add cases for BROWSE's visual entry keys, the gv reservation, and each VISUAL_KEYS row
  // in VISUAL and V-LINE. Also pin keys that must never answer a card and Ctrl+c's interrupt.
  { label: "v", mode: "browse", event: key("v"), ctx: { sessionEnded: false } },
  { label: "Shift+V", mode: "browse", event: key("V", { shiftKey: true }), ctx: { sessionEnded: false } },
  { label: "gv", mode: "browse", event: key("v"), ctx: { sessionEnded: false, pending: "g" } },
  ...(["visual", "vline"] as const).flatMap((mode) =>
    VISUAL_KEYS.flatMap(({ keys }) =>
      keys.split(" / ").map((token) => ({
        label: `${mode}: ${token}`,
        mode,
        event: visualToken(token),
        ctx: { sessionEnded: false },
      })),
    ),
  ),
  { label: "visual: d", mode: "visual", event: key("d"), ctx: { sessionEnded: false } },
  { label: "visual: a", mode: "visual", event: key("a"), ctx: { sessionEnded: false } },
  { label: "visual: Shift+D", mode: "visual", event: key("D", { shiftKey: true }), ctx: { sessionEnded: false } },
  { label: "visual: Ctrl+c, a turn is running", mode: "visual", event: key("c", { ctrlKey: true }), ctx: { sessionEnded: false, turnRunning: true } },
  // Cover every CARET_KEYS row, three keys that must never answer, and running/idle Ctrl+c.
  // Idle Ctrl+c is claimed in CARET, unlike the VISUAL case above.
  ...CARET_KEYS.flatMap(({ keys }) =>
    keys.split(" / ").map((token) => ({
      label: `caret: ${token}`,
      mode: "caret" as const,
      event: visualToken(token),
      ctx: { sessionEnded: false },
    })),
  ),
  { label: "caret: d", mode: "caret", event: key("d"), ctx: { sessionEnded: false } },
  { label: "caret: a", mode: "caret", event: key("a"), ctx: { sessionEnded: false } },
  { label: "caret: Shift+D", mode: "caret", event: key("D", { shiftKey: true }), ctx: { sessionEnded: false } },
  { label: "caret: Enter", mode: "caret", event: key("Enter"), ctx: { sessionEnded: false } },
  { label: "caret: Ctrl+c, idle", mode: "caret", event: key("c", { ctrlKey: true }), ctx: { sessionEnded: false, turnRunning: false } },
  {
    label: "caret: Ctrl+c, a turn is running",
    mode: "caret",
    event: key("c", { ctrlKey: true }),
    ctx: { sessionEnded: false, turnRunning: true },
  },
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
        expect(actual, "\"keys are only ever added, never changed in meaning\"").toEqual(
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
   *  start a sequence on them by design, so the leader engine alone cannot reach them. */
  function resolveRow(row: { keys: string[]; action: string }): PanelAction | null {
    const first = row.keys[0];
    if (first === "g" || first === "z" || first === "[" || first === "]") {
      expect(row.keys.length, `${row.keys.join(" ")}: this test only knows the two-key reserved-prefix shape`).toBe(2);
      const pendingStep = resolveKey("browse", key(first), { sessionEnded: false });
      expect(pendingStep, `${row.keys.join(" ")}: the first key should arm a pending prefix`).toEqual({ kind: "pending", prefix: first });
      // A real keypress for an uppercase second key (`gT`) carries `shiftKey: true` alongside the
      // already-uppercase `.key`, redundant as that is -- match it, so this synthetic event is not
      // more permissive than what a browser actually sends.
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
        `"keys are only ever added, never changed in meaning"`,
      ).toEqual({ kind: "panel", binding: expect.objectContaining({ keys: row.keys, action: row.action }) });
    });
  }
});
