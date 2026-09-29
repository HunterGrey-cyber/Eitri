/** Treat plain `Ctrl+[` as Esc. WebKitGTK 2.52.6 sends `key "["`, `code BracketLeft`, with
 *  `ctrlKey` set. A capture listener re-dispatches Escape on the same target so existing handlers
 *  can use their normal Escape path.
 *
 *  Shift, Alt or Meta makes this a different chord. Leave composing keys to the input method,
 *  using both `isComposing` and the legacy keyCode 229 to detect them. */
export function isCtrlBracket(e: KeyboardEvent): boolean {
  return e.key === "[" && e.ctrlKey && !e.shiftKey && !e.altKey && !e.metaKey && !e.isComposing && e.keyCode !== 229;
}

/** Installs the listener and returns its removal. `App.tsx` calls this in its FIRST effect, so its
 *  document-capture listener is registered ahead of `onModeKey`'s (the Shift+Tab router, which also
 *  keeps the bypass prompt's last-key clock), which then sees the Escape this dispatches instead of
 *  the Ctrl+[ it replaced.
 *
 *  The original is stopped, so no handler ever sees a Ctrl+[ next to the Escape; the Escape keeps the
 *  original's `repeat`, so a held Ctrl+[ is a held Esc (the region's and the y/n prompt's "a repeat of
 *  the key that ended it does nothing" rules read it). It is an untrusted event, so the browser runs no
 *  default action of its own for it: the panel's handlers are what act on it, as they do for a real Esc.
 *
 *  What it does not cover: window-capture listeners run before a document-capture one, so while a
 *  panel HINT request is pending, `swallowWhilePending` (App.tsx) drops every key, this one included;
 *  and GTK claims a chord in the shell before the WebView ever sees it (its prefix, the global HINT).
 *  Only the keydown is translated, so a Ctrl+[ leaves no Escape keyup behind. */
export function installCtrlBracketAsEscape(doc: Document): () => void {
  const onKeyDown = (event: KeyboardEvent) => {
    if (!isCtrlBracket(event) || event.target === null) return;
    event.preventDefault();
    event.stopImmediatePropagation();
    event.target.dispatchEvent(
      new KeyboardEvent("keydown", {
        key: "Escape",
        code: "Escape",
        bubbles: true,
        cancelable: true,
        composed: true,
        repeat: event.repeat,
      }),
    );
  };
  doc.addEventListener("keydown", onKeyDown, true);
  return () => doc.removeEventListener("keydown", onKeyDown, true);
}
