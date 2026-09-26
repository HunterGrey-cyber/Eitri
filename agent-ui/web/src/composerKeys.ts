/** C4: an IME's own Enter/Esc/`?`. `isComposing`, or the legacy 229 some WebKit builds
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

/** Restore queued text before the current box contents. */
export function mergeTaken(texts: string[], current: string): string {
  const parts = current.trim() === "" ? texts : [...texts, current];
  return parts.join("\n\n");
}

/** Chords handled by `Composer`'s `onKeyDown` or `App`'s INPUT branch. `Esc` and `Ctrl+o`
 *  bubble to the root's `resolveKey("input", ...)` because `Composer` leaves them unclaimed.
 *  `composerKeys.test.ts` checks this list against `INPUT_KEYS` in both directions.
 *  `Composer.test.tsx` exercises each entry and requires its behavior table to match this list.
 *  A listed chord without a handler or behavior test fails. A handler added without a list
 *  entry is not caught, since nothing derives this list from the handler's source. */
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
