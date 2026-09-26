/** C4 (spec §4.2): an IME's own Enter/Esc/`?`. `isComposing`, or the legacy 229 some WebKit builds
 *  still report -- the same pair `TabBar`'s rename and `Chooser`'s filter already test. */
export function isImeKey(e: { isComposing: boolean; keyCode: number }): boolean {
  return e.isComposing || e.keyCode === 229;
}

/** C6: readline's `Ctrl+w` (a word back, spaces first) and `Ctrl+u` (to the line's start). A
 *  selection is deleted whole, as readline does with a region. `null` when nothing would change. */
export function readlineEdit(
  value: string,
  start: number,
  end: number,
  key: "w" | "u",
): { value: string; caret: number } | null {
  if (start !== end) return { value: value.slice(0, start) + value.slice(end), caret: start };
  if (start === 0) return null;
  let from: number;
  if (key === "u") {
    from = value.lastIndexOf("\n", start - 1) + 1;
    if (from === start) from = start - 1; // at a line's start, delete the newline, as readline would the char
  } else {
    from = start;
    while (from > 0 && /\s/.test(value[from - 1])) from -= 1;
    while (from > 0 && !/\s/.test(value[from - 1])) from -= 1;
  }
  return { value: value.slice(0, from) + value.slice(start), caret: from };
}

export function caretOnFirstLine(value: string, caret: number): boolean {
  return !value.slice(0, caret).includes("\n");
}

export function caretOnLastLine(value: string, caret: number): boolean {
  return !value.slice(caret).includes("\n");
}

/** The box's floor (the old fixed 44px) and its ceiling (40% of the panel, C3). */
export const COMPOSER_MIN_PX = 44;
export function growHeight(scrollHeight: number, panelHeight: number): number {
  return Math.max(COMPOSER_MIN_PX, Math.min(scrollHeight, Math.floor(panelHeight * 0.4)));
}

/** Ruling 7: the queue comes back first, then whatever is in the box. */
export function mergeTaken(texts: string[], current: string): string {
  const parts = current.trim() === "" ? texts : [...texts, current];
  return parts.join("\n\n");
}

/** Every chord `Composer`'s `onKeyDown` (and `App`'s INPUT branch, for `Esc` and, since R3,
 *  `Ctrl+o`, which `Composer` leaves unclaimed so it bubbles to the root's `resolveKey("input", …)`)
 *  implements. Two tests hold the chain: `composerKeys.test.ts` ties `keymap.ts`'s `INPUT_KEYS` to
 *  this list both ways, and `Composer.test.tsx` ("every chord COMPOSER_CHORDS names …") exercises
 *  each entry against the real `Composer` (or `resolveKey("input", …)` for `Esc`/`Ctrl+o`) and
 *  requires its table's keys to equal this list. So a chord listed here with no handler behind it
 *  fails, and so does a new entry with no behaviour test. A handler added to `onKeyDown` without an
 *  entry here is NOT caught: nothing derives this list from the handler's source. */
export const COMPOSER_CHORDS: string[] = [
  "Enter",
  "Ctrl+Enter",
  "Shift+Enter",
  "↑ / ↓",
  "Ctrl+r",
  "Ctrl+w / Ctrl+u",
  "Ctrl+c",
  "Ctrl+g",
  "Ctrl+o",
  "Shift+Tab",
  "Esc",
  "?",
];
