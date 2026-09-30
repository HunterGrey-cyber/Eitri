/**
 * The page's half of the agent panel's typing cadence (owner decision #37, revised 2026-09-29;
 * `core/src/panel_cadence.rs` is the Rust half). While the user types in the editor, Rust pushes the
 * stream on an even cadence so the panel causes few, evenly spaced GTK frame-clock cycles -- each one
 * can make the editor's next frame land a refresh late. The page's own motion must not undo that:
 * the turn meter repaints on its own, so while the user types it is slowed to no faster than the
 * cadence (never paused: the stream is meant to stay smooth, and a still meter reads as a hang).
 *
 * The only self-driven motion in the panel, checked 2026-09-29 by reading every `setInterval`,
 * `requestAnimationFrame`, `animation` and `transition` in `src/`:
 *
 * - the meter (`index.css`, `.turn-activity .meter-fill`): 4 visible states in 1200 ms, one repaint
 *   per {@link NATURAL_METER_STEP_MS}. Slowed here, through `--meter-step`.
 * - the elapsed clock (`components/TurnActivity.tsx`): one repaint a second. The slowest cadence
 *   Rust accepts is one push a second, so this is never faster than the cadence and is left alone.
 * - everything else is a one-shot after a user action (a flash, a which-key delay, the draft mirror)
 *   or the native caret in a composer that has the keys -- and typing in the editor means the panel
 *   does not.
 *
 * Rust sends `editor_typing` only for a cadence slower than {@link NATURAL_METER_STEP_MS}
 * (`panel_cadence::SELF_DRIVEN_STEP_MS`, the same number); at faster ones the meter is already
 * within the cadence and telling the page would cost it two repaints per burst for nothing.
 */

/** One visible state of the meter with nothing slowing it: `index.css` animates 4 states over
 *  1200 ms. `indexCss.test.ts` holds this equal to the stylesheet and to the Rust constant. */
export const NATURAL_METER_STEP_MS = 300;

/** How long the meter holds each state while the user types, given the gap between the panel's
 *  pushes: the natural step, or the gap if that is longer. Never shorter than the natural step --
 *  typing must not speed the meter up -- and a gap that is not a usable number falls back to it. */
export function typingMeterStepMs(periodMs: number): number {
  if (!Number.isFinite(periodMs) || periodMs <= 0) return NATURAL_METER_STEP_MS;
  return Math.max(NATURAL_METER_STEP_MS, Math.round(periodMs));
}

/** Applies Rust's `editor_typing` to the document's root. While `typing`, `data-editor-typing` is
 *  set and `--meter-step` slows the meter; otherwise both are gone. A period that is not a usable
 *  number is "not typing": a malformed message must not leave the meter slowed. */
export function applyEditorTyping(root: HTMLElement, typing: boolean, periodMs: number): void {
  if (typing && Number.isFinite(periodMs) && periodMs > 0) {
    root.setAttribute("data-editor-typing", "");
    root.style.setProperty("--meter-step", `${typingMeterStepMs(periodMs)}ms`);
  } else {
    root.removeAttribute("data-editor-typing");
    root.style.removeProperty("--meter-step");
  }
}
