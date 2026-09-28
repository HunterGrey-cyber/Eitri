/** Whether a Super/Hyper key is physically held, tracked from that key's OWN keydown and keyup.
 *
 *  The 2026-09-28 sandbox pass of the v1 audit fixes (`the private review notes`,
 *  Task 1) answered the question `keymap.ts`'s `KeyLike` doc left open: WebKitGTK 2.52.6 never reports a
 *  held Super on ANOTHER key's event -- `metaKey` is false and `getModifierState("Super"/"Hyper")` reads
 *  false while Super is down, so `hasSuperOrHyper` alone let `Super+a` answer a permission card. What
 *  WebKitGTK does deliver is the Super key's own keydown (`key: "Super"`, `code: "OSLeft"`) and keyup.
 *  So this module remembers it, the way a terminal tracks a held modifier it cannot query.
 *
 *  A missed keyup (focus moved away with Super still down, as GNOME's own Super tap does when it opens
 *  the overview) would leave this true, which only ever makes an answer key refuse -- the safe side --
 *  and `reset` runs on every `blur` and `visibilitychange`, which that focus change also fires. */

const SUPER_KEYS = new Set(["Super", "Hyper", "OS"]);
const SUPER_CODES = new Set(["OSLeft", "OSRight", "MetaLeft", "MetaRight"]);

/** One entry per physical key held (its `code`, or its `key` when a build reports no code), so
 *  releasing right Super while left Super stays down leaves it held (Codex review, 2026-09-28). */
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

/** Installs the listeners and returns their removal. App.tsx calls this in its FIRST effect: capture
 *  phase on the window, registered before HINT's `swallowWhilePending`, which stops a key's
 *  propagation on that same tier and would otherwise hide Super's keydown from this (Codex review,
 *  2026-09-28). What this cannot see -- a Super press GTK claims before the page gets it, or Super
 *  still held when focus comes back -- is what the shell's own filter in front of the WebView covers
 *  (`shell/src/panel_super.rs`), wherever GDK reports Super in its modifier state. */
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
