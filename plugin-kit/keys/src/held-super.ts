import { onRemove } from "./bridge";
/** WebKit can omit Super from other keys' modifier flags, so remember its own press and release.
 * A lost release only withholds actions; blur and visibility changes clear that stale state. */

const SUPER_KEYS = new Set(["Super", "Hyper", "OS"]);
const SUPER_CODES = new Set(["OSLeft", "OSRight", "MetaLeft", "MetaRight"]);

/** Track each physical key separately so releasing one Super cannot release the other. */
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

/** Capture before page handlers can hide a modifier's own event by stopping propagation. */
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

onRemove(reset);
