/** An IME's own Enter/Esc/`?` belong to the IME, not to the panel. `isComposing`, or the legacy 229 some WebKit builds
 *  still report -- the same pair `TabBar`'s rename and `Chooser`'s filter already test. */
export function isImeKey(e: { isComposing: boolean; keyCode: number }): boolean {
  return e.isComposing || e.keyCode === 229;
}

/** Readline's `Ctrl+w` (a word back, spaces first) and `Ctrl+u` (to the line's start). A
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

/** Where readline's `Ctrl+a` / `Ctrl+e` put the caret: the start or the end of the line it is on.
 *  WebKitGTK's text box binds neither under GTK's default key theme (`Ctrl+a` selects everything,
 *  `Ctrl+e` does nothing), so the composer moves the caret itself. */
export function lineEdge(value: string, caret: number, edge: "start" | "end"): number {
  if (edge === "start") return value.lastIndexOf("\n", caret - 1) + 1;
  const next = value.indexOf("\n", caret);
  return next === -1 ? value.length : next;
}

/** Claude Code's two newline keys. `Alt+Enter`
 *  inserts a newline in place of the selection; a plain Enter with a backslash right before the caret
 *  (and no selection) removes that backslash and inserts a newline in its place. `null` for every other
 *  Enter -- Ctrl+Enter and Meta+Enter keep sending, a plain Enter with no such backslash sends,
 *  and Shift+Enter never gets here (it is the textarea's own newline). */
export function enterNewline(
  value: string,
  start: number,
  end: number,
  mods: { altKey: boolean; ctrlKey: boolean; metaKey: boolean },
): { value: string; caret: number } | null {
  if (mods.ctrlKey || mods.metaKey) return null;
  if (mods.altKey) return { value: value.slice(0, start) + "\n" + value.slice(end), caret: start + 1 };
  if (start === end && start > 0 && value[start - 1] === "\\") {
    return { value: value.slice(0, start - 1) + "\n" + value.slice(end), caret: start };
  }
  return null;
}

export function caretOnFirstLine(value: string, caret: number): boolean {
  return !value.slice(0, caret).includes("\n");
}

export function caretOnLastLine(value: string, caret: number): boolean {
  return !value.slice(caret).includes("\n");
}

/** The textarea's height: its content, at most 40% of the panel. There is no floor here: an empty box is
 *  one line tall (`rows={1}` and the stylesheet's `min-height`), the height BROWSE's stand-in has inside the
 *  same card, so entering INPUT does not grow the card. */
export function growHeight(scrollHeight: number, panelHeight: number): number {
  return Math.min(scrollHeight, Math.floor(panelHeight * 0.4));
}

/** The queue comes back first, then whatever is in the box: the queued messages were written first. */
export function mergeTaken(texts: string[], current: string): string {
  const parts = current.trim() === "" ? texts : [...texts, current];
  return parts.join("\n\n");
}

/** Every chord `Composer`'s `onKeyDown` (and `App`'s INPUT branch, for `Esc` and
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
  // Claude Code's own two newline keys.
  "Alt+Enter / \\ Enter",
  "↑ / ↓",
  "Ctrl+r",
  // The `/` command menu's keys (`Composer`'s open-menu branch, ahead of everything else).
  "/ command",
  "Ctrl+n / Ctrl+p",
  "Tab",
  "Ctrl+w / Ctrl+u",
  "Ctrl+a / Ctrl+e",
  "Ctrl+c",
  // Like `Ctrl+o`, left to bubble to `App`'s INPUT branch, which approves the
  // oldest waiting card on it.
  "Ctrl+y",
  "Ctrl+g",
  "Ctrl+o",
  "Shift+Tab",
  "Esc",
  "?",
];
