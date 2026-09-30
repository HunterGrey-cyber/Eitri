/** R9 (idiom matrix E10): `Ctrl+[` is Esc, as in vim, nvim and the terminal. WebKitGTK 2.52.6 sends it
 *  as `key "["`, `code BracketLeft`, `ctrlKey` (kbux pass 2026-09-29, row P15). One capture listener
 *  re-dispatches it as a plain Escape keydown on the same target, so every handler reads the key it
 *  already reads; an input method's own key is left to it, as Esc is.
 *
 *  Plain means plain: Shift, Alt and Meta make it some other chord (`Ctrl+{` is not Esc), and a key an
 *  input method is composing with (`isComposing`, or the legacy keyCode 229 some WebKit builds still
 *  report -- the same pair `composerKeys.ts`'s `isImeKey` tests) belongs to it, exactly as Esc does. */
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
