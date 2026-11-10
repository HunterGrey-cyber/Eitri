/** The proof a pointer answered a permission card: what a card button hands the page with its answer. It lives apart
 *  from `./cardMode` (which re-exports it) and imports nothing, so the card's markup can make one without bringing the
 *  key machinery (`keymap.ts`) into a page that has no keys, the phone's transcript.
 *
 *  `pointerAnswerProof` is its only constructor. The type is branded with a symbol only this module declares, so
 *  nothing outside it can make one without a cast. */

declare const pointerBrand: unique symbol;
export type PointerAnswerProof = { readonly [pointerBrand]: true };

/** The proof that a pointer answered: a click a real press made (`detail` counts the presses). A click that Enter
 *  or Space synthesised on a focused button has `detail` 0 and proves nothing; so does one a script dispatched
 *  without a press. */
export function pointerAnswerProof(event: { detail: number }): PointerAnswerProof | null {
  if (!(event.detail > 0)) return null;
  // The brand is a type only: no value carries it at run time.
  return Object.freeze({}) as PointerAnswerProof;
}
