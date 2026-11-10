export const JUMPLIST_MAX = 100;
/** offset is the number of pixels of this row above the viewport's top. */
export type JumpPos = { key: string; index: number; offset: number };
export type Jumplist = { entries: JumpPos[]; at: number };

export function emptyJumplist(): Jumplist {
  return { entries: [], at: 0 };
}

/** How far apart, in pixels, two places in one row may be and still count as one place. */
export const SAME_PLACE_PX = 4;

/** Whether two places are one: the same row, scrolled to within `SAME_PLACE_PX`. Two places in one long row (the top
 *  and the end of a reply, two hunks of one edit) are two entries. */
function samePlace(a: JumpPos, b: JumpPos): boolean {
  return a.key === b.key && Math.abs(a.offset - b.offset) <= SAME_PLACE_PX;
}

export function recordJump(list: Jumplist, from: JumpPos): Jumplist {
  const entries = list.entries.slice(0, list.at + 1).filter((entry) => !samePlace(entry, from));
  entries.push({ ...from });
  const kept = entries.slice(-JUMPLIST_MAX);
  return { entries: kept, at: kept.length };
}

export function back(list: Jumplist, here: JumpPos): { list: Jumplist; to: JumpPos } | null {
  if (list.entries.length === 0) return null;
  const newest = list.at === list.entries.length;
  const saved = newest ? recordJump(list, here) : list;
  const at = newest ? saved.entries.length - 2 : saved.at - 1;
  if (at < 0) return null;
  return { list: { entries: saved.entries, at }, to: saved.entries[at] };
}

export function forward(list: Jumplist): { list: Jumplist; to: JumpPos } | null {
  const at = list.at + 1;
  if (at >= list.entries.length) return null;
  return { list: { entries: list.entries, at }, to: list.entries[at] };
}

export function landingIndex(pos: JumpPos, keys: readonly string[]): number {
  const found = keys.indexOf(pos.key);
  return found >= 0 ? found : Math.max(0, Math.min(pos.index, keys.length - 1));
}
