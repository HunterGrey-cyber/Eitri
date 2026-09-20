import type { TimelineItem } from "./timeline";
import { permissionTarget } from "./nav";
import type { AnswerableItem } from "./nav";

/** One key the which-key strip offers for the row under the cursor: the key as typed, and the
 *  word it does. Rendered as a keycap plus this label (spec §2.2). */
export type StripEntry = { key: string; label: string };

/**
 * What the which-key strip lists for the row under `cursor` (spec §2.2), in the spec's order:
 * a pending permission first, then a tool result, then the end-of-session offer. `? keys` is not
 * included here -- it is always last and drawn fainter, which is `WhichKey`'s job, not this one's.
 *
 * Only the row's OWN keys: `j`/`k`/`y`/`f` and the rest that work everywhere are deliberately left
 * out (spec §2.2's last bullet), or every row would say the same thing and the strip would be
 * noise instead of a hint.
 */
export function stripEntries(
  timeline: TimelineItem[],
  answerable: AnswerableItem[],
  cursor: number,
  sessionEnded: boolean,
): StripEntry[] {
  const entries: StripEntry[] = [];
  // A dead session's cards are inert (`resolveKey`'s own `a`/`d` refusal) -- offering them here
  // would promise a key that does nothing.
  if (!sessionEnded && permissionTarget(answerable, cursor) !== null) {
    entries.push({ key: "a", label: "allow" }, { key: "d", label: "deny" }, { key: "l", label: "buttons" });
  }
  const here = timeline[cursor];
  if (here !== undefined && here.kind === "tool" && here.call.result !== null) {
    entries.push({ key: "Enter", label: "result" });
  }
  if (sessionEnded) {
    entries.push({ key: "r", label: "new session" });
  }
  return entries;
}
