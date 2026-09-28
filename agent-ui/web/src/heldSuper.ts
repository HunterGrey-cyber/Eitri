/** Tracks held Super/Hyper keys from their own keydown and keyup events. WebKitGTK 2.52.6
 *  reports neither metaKey nor getModifierState("Super"/"Hyper") on another key's event while
 *  Super is held. It does deliver the Super key's own events, so remembering them prevents
 *  Super+a from answering a permission card. A missed keyup can leave a key marked as held and
 *  cause answer keys to be refused. Resetting on blur and visibilitychange clears that state
 *  when focus moves away. */

const SUPER_KEYS = new Set(["Super", "Hyper", "OS"]);
const SUPER_CODES = new Set(["OSLeft", "OSRight", "MetaLeft", "MetaRight"]);

/** One entry per physical key held (its `code`, or its `key` when a build reports no code), so
 *  releasing right Super while left Super stays down leaves it held. */
const held = new Set<string>();

type KeyEventLike = { key: string; code?: string };

function isSuperKey(event: KeyEventLike): boolean {
  return SUPER_KEYS.has(event.key) || (event.code !== undefined && SUPER_CODES.has(event.code));
}

/** Feed every keydown and keyup here (capture phase, before any handler reads `superHeld`). */
export function noteKey(type: "keydown" | "keyup", event: KeyEventLike): void {
  if (!isSuperKey(event)) return;
  const id = event.code !== undefined && event.code !== "" ? event.code : event.key;
  if (type === "keydown") held.add(id);
  else held.delete(id);
}

/** Forget a held Super: focus left the page, so its keyup may never arrive here. */
export function reset(): void {
  held.clear();
}

export function superHeld(): boolean {
  return held.size > 0;
}

/** Installs the listeners and returns their removal. `App.tsx` registers these at window
 *  capture in its first effect, before HINT's `swallowWhilePending` can hide Super's keydown.
 *  The shell filter in `shell/src/panel_super.rs` covers presses GTK claims before the page
 *  and Super still held when focus returns, wherever GDK reports it in the modifier state. */
export function installHeldSuperTracking(doc: Document, win: Window): () => void {
  const down = (event: KeyboardEvent) => noteKey("keydown", event);
  const up = (event: KeyboardEvent) => noteKey("keyup", event);
  const onVisibility = () => reset();
  win.addEventListener("keydown", down, true);
  win.addEventListener("keyup", up, true);
  win.addEventListener("blur", reset);
  doc.addEventListener("visibilitychange", onVisibility);
  return () => {
    win.removeEventListener("keydown", down, true);
    win.removeEventListener("keyup", up, true);
    win.removeEventListener("blur", reset);
    doc.removeEventListener("visibilitychange", onVisibility);
    reset();
  };
}
