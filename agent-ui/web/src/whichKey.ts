import type { TimelineItem } from "./timeline";
import { permissionTarget } from "./nav";
import type { AnswerableItem } from "./nav";

/** One key the which-key strip offers for the row under the cursor: the key as typed, and the
 *  word it does. Rendered as a keycap plus this label. */
export type StripEntry = { key: string; label: string };

/**
 * List the row's keys under `cursor`: pending permission, tool result, then end-of-session offer.
 * `WhichKey` adds `? keys` last and draws it fainter.
 *
 * Omit global keys such as `j`/`k`/`y`/`f`; repeating them for every row would obscure the
 * row-specific actions.
 */
export function stripEntries(
  timeline: TimelineItem[],
  answerable: AnswerableItem[],
  cursor: number,
  sessionEnded: boolean,
  /** The keys are on a banner, the status row or another control outside the rows: `onKeyDown`'s
   *  row arms (`a`/`d`, `Enter` on a row) are gated on it, so the row's own keys resolve to nothing
   *  there. It says nothing about the session, so it never turns on `r`. */
  atEdge = false,
): StripEntry[] {
  const entries: StripEntry[] = [];
  // A dead session's cards are inert (`resolveKey`'s own `a`/`d` refusal) -- offering them here
  // would promise a key that does nothing.
  if (!sessionEnded && !atEdge && permissionTarget(answerable, cursor) !== null) {
    entries.push({ key: "a", label: "allow" }, { key: "d", label: "deny" }, { key: "l", label: "buttons" });
  }
  const here = timeline[cursor];
  if (!atEdge && here !== undefined && here.kind === "tool" && here.call.result !== null) {
    entries.push({ key: "Enter", label: "result" });
  }
  if (sessionEnded) {
    entries.push({ key: "r", label: "new session" });
  }
  return entries;
}
