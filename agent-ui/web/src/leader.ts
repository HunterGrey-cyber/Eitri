/** The panel's which-key sequence engine: leader-driven multi-key bindings over `PanelTable`
 *.
 *
 * `resolveKey` (`./keymap`) still owns every fixed single-key row and the four reserved two-key
 * pairs (`gg`/`gf`/`zh`/`zl`/`[[`/`]]`/`[]`/`][`, Global Constraint #4); this module owns the
 * leader and any longer sequence the table itself defines. `App.tsx` is expected to try
 * `startSequence`/`advanceSequence` first on a key `resolveKey` did not claim (a `{kind:"none"}`
 * result falls through to `resolveKey`, never the other way around), since the table's own
 * reserved-pair rule means the two can never both claim the same key.
 */
import type { PanelBinding, PanelTable } from "./keymap";

/** which-key's default `delay` (which-key.nvim lua/which-key/config.lua:13-15). */
export const WHICH_KEY_DELAY_MS = 200;

/** The two-key prefixes `resolveKey` already reserves for its own fixed pairs (Global Constraint
 *  #4); a table sequence may still start with one of these keys (`[b`), but the bare key itself is
 *  never a sequence start on its own -- `resolveKey`'s `{kind:"pending"}` machinery owns it. */
const PENDING_FIRST = new Set(["g", "z", "[", "]"]);

/** The result of one key against the sequence engine, at a node identified by `typed` (the keys
 *  typed so far, `"<leader>"` standing for the leader itself). */
export type SeqStep =
  | { kind: "none" } // not a sequence start: fall through to resolveKey
  | { kind: "pending"; typed: string[]; ambiguous: PanelBinding | null }
  | { kind: "run"; binding: PanelBinding }
  | { kind: "cancel" }; // swallowed, nothing runs

/** The bindings whose `keys` start with `typed`, split into the one that matches it exactly (if
 *  any) and whether some binding continues past it (making the node a prefix, not just a leaf). */
function matching(table: PanelTable, typed: string[]) {
  const starts = table.bindings.filter((b) => b.keys.length >= typed.length && typed.every((k, i) => b.keys[i] === k));
  return { exact: starts.find((b) => b.keys.length === typed.length) ?? null, longer: starts.some((b) => b.keys.length > typed.length) };
}

/** One step of the walk: `atStart` is whether `typed` is the very first key pressed (the caller's
 *  `startSequence`), which decides what an unmatched node means -- not a sequence at all (fall
 *  through) versus a sequence that ran out (cancel, having already swallowed at least one key). */
function step(table: PanelTable, typed: string[], atStart: boolean): SeqStep {
  const { exact, longer } = matching(table, typed);
  if (exact && !longer) return { kind: "run", binding: exact };
  if (longer) return { kind: "pending", typed, ambiguous: exact };
  return atStart ? { kind: "none" } : { kind: "cancel" };
}

/** Start a possible sequence. The leader cannot start while `onControl` says a focused control
 *  has the keys. Reserved `g`/`z`/`[`/`]` prefixes belong to `resolveKey`, even when the table
 *  defines a sequence such as `[b`; its pending-pair lookup resolves those bindings. */
export function startSequence(table: PanelTable, key: string, onControl: boolean): SeqStep {
  if (PENDING_FIRST.has(key)) return { kind: "none" }; // resolveKey's own two-key prefixes
  if (key === table.leader) return onControl ? { kind: "none" } : step(table, ["<leader>"], true);
  return step(table, [key], true);
}

/** Whether `key` is one of `resolveKey`'s reserved two-key prefixes (`g`/`z`/`[`/`]`). For a
 *  screen that has no `resolveKey` of its own (the empty tab's dashboard) but must still reach a
 *  table pair such as `[b`. */
export function isPendingFirst(key: string): boolean {
  return PENDING_FIRST.has(key);
}

/** The table binding a reserved prefix plus one more key completes (`[` then `b`), the same lookup
 *  `resolveKey`'s own `tableHit` does; `null` when the pair is not in the table. */
export function pendingPairBinding(table: PanelTable, prefix: string, key: string): PanelBinding | null {
  return table.bindings.find((b) => b.keys.length === 2 && b.keys[0] === prefix && b.keys[1] === key) ?? null;
}

/** Advance a sequence pending at `typed`. Escape cancels; Backspace goes up one level or
 *  cancels at the top. Other keys continue, run a binding, or cancel if unmatched.
 *  An unmatched pending key is swallowed, unlike an unmatched first key. */
export function advanceSequence(table: PanelTable, typed: string[], key: string): SeqStep {
  if (key === "Escape") return { kind: "cancel" };
  if (key === "Backspace") {
    return typed.length <= 1
      ? { kind: "cancel" }
      : { kind: "pending", typed: typed.slice(0, -1), ambiguous: matching(table, typed.slice(0, -1)).exact };
  }
  return step(table, [...typed, key], false);
}

/** One row of the which-key popup box: a real binding's own key and description, or a group label
 *  (`+tab`) standing in for everything under it. `disabled` greys out `mode.cycle` once a session
 *  has started. */
export type BoxEntry = { key: string; label: string; group: boolean; disabled: boolean };

/** How a table key reads to a person: the leader as its label (`Space`), anything else as itself. */
function humanKey(table: PanelTable, k: string): string {
  return k === "<leader>" ? table.leaderLabel : k === " " ? "Space" : k;
}

/** Distinct next keys reachable from `typed`: direct bindings in table order, then groups
 *  in `table.groups` order. A key is a group if a binding continues beyond it.
 *  Group labels come from `table.groups`, falling back to `+<key>`.
 *
 *  `modeFixed` means `mode.cycle` would flash instead of posting a change, so its entry is disabled.
 *  A live tab that supports switching can still cycle; `modeKeyRoute` determines the route. */
export function boxEntries(table: PanelTable, typed: string[], modeFixed: boolean): BoxEntry[] {
  const depth = typed.length;
  const atDepth = table.bindings.filter((b) => b.keys.length > depth && typed.every((k, i) => b.keys[i] === k));
  const nextKeys: string[] = [];
  for (const b of atDepth) {
    const nextKey = b.keys[depth];
    if (!nextKeys.includes(nextKey)) nextKeys.push(nextKey);
  }
  const leaves: BoxEntry[] = [];
  const groups: BoxEntry[] = [];
  for (const nextKey of nextKeys) {
    const isGroup = atDepth.some((b) => b.keys.length > depth + 1 && b.keys[depth] === nextKey);
    const label = isGroup
      ? (table.groups.find((g) => g.keys.length === depth + 1 && g.keys[depth] === nextKey && typed.every((k, i) => g.keys[i] === k))
          ?.label ?? `+${humanKey(table, nextKey)}`)
      : (atDepth.find((b) => b.keys.length === depth + 1 && b.keys[depth] === nextKey)?.desc ?? "");
    const binding = atDepth.find((b) => b.keys.length === depth + 1 && b.keys[depth] === nextKey);
    const disabled = !isGroup && binding?.action === "mode.cycle" && modeFixed;
    const entry: BoxEntry = { key: humanKey(table, nextKey), label, group: isGroup, disabled: Boolean(disabled) };
    (isGroup ? groups : leaves).push(entry);
  }
  return [...leaves, ...groups];
}

/** The box's title: "Space b", "Space" alone at the leader's own node. Joins `typed`'s human
 *  spellings with a space, which-key's own convention (`lua/which-key/config.lua`'s default popup). */
export function sequenceTitle(table: PanelTable, typed: string[]): string {
  return typed.map((k) => humanKey(table, k)).join(" ");
}

/** `resolveKey`'s four reserved two-key prefixes have no `PanelBinding` of their own (they are
 *  fixed rows, not table entries), so a which-key box opened on one of them
 *  (`g`/`z`/`[`/`]`, held past `WHICH_KEY_DELAY_MS`) is filled in by hand rather than
 *  from `table.bindings`. */
export const FIXED_PENDING_ENTRIES: Record<"g" | "z" | "[" | "]", BoxEntry[]> = {
  g: [
    { key: "g", label: "first row", group: false, disabled: false },
    { key: "f", label: "open path", group: false, disabled: false },
  ],
  z: [
    { key: "h", label: "scroll table left", group: false, disabled: false },
    { key: "l", label: "scroll table right", group: false, disabled: false },
  ],
  "[": [{ key: "[", label: "previous prompt", group: false, disabled: false }],
  "]": [{ key: "]", label: "next prompt", group: false, disabled: false }],
};
