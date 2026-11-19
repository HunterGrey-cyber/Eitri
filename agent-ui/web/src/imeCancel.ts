/** The host's cancel of an input-method composition the composer has open when the keys leave the panel.
 *
 * WebKit composes in the web process: the host never sees the marked text, and when the web view gives up the keys it
 * confirms the composition, raw letters and all (`ni hao` for Pinyin's `nihao`). So before the macOS host moves the
 * keys from the panel to the editor it calls `window.__eitriImeCancel()`; a composer with a composition open arms its
 * cancel, and when that composition ends (the blur's confirm) it puts back the text it had before the composition
 * began. Nothing else calls the hook: a move of the keys the host makes for its own reasons, a blur of the page's own,
 * or a composition the person commits leaves the text as the input method made it.
 *
 * An armed cancel lapses after `ARMED_FOR_MS`, at the next key the person types, when another composition starts, or
 * when the host disarms it (`window.__eitriImeCancel(false)`) because AppKit refused the move: the composition stays
 * open then, and its later end is the person's own commit. And it puts the text back only while the text is still the
 * one the composition began in, with the composition in its middle: a draft something else rewrote meanwhile (an
 * unsent message given back) is left as it is. */

export const ARMED_FOR_MS = 1000;

export interface Snapshot {
  text: string;
  selectionStart: number;
  selectionEnd: number;
}

/** One composer's side of the cancel. */
export class CompositionGuard {
  private before: Snapshot | null = null;
  private armedAt: number | null = null;

  constructor(private readonly now: () => number = () => performance.now()) {}

  /** A composition began over `snapshot` (the text and selection just before it). */
  start(snapshot: Snapshot): void {
    this.before = snapshot;
    this.armedAt = null;
  }

  /** A key the person typed: an armed cancel was not followed by the blur it was for. */
  key(): void {
    this.armedAt = null;
  }

  /** The host's cancel: armed only while a composition is open. */
  arm(): boolean {
    if (this.before === null) return false;
    this.armedAt = this.now();
    return true;
  }

  /** The host's move was refused: the composition goes on as the person's own. */
  disarm(): void {
    this.armedAt = null;
  }

  /** The composition ended with the field holding `text`: what to put back, if a live cancel was armed for it and the
   * text is still the one the composition began in (what was before the selection, then the composed letters, then
   * what was after it). */
  end(text: string): Snapshot | null {
    const before = this.before;
    const armedAt = this.armedAt;
    this.before = null;
    this.armedAt = null;
    if (before === null || armedAt === null || this.now() - armedAt > ARMED_FOR_MS) return null;
    const head = before.text.slice(0, before.selectionStart);
    const tail = before.text.slice(before.selectionEnd);
    if (!text.startsWith(head) || !text.endsWith(tail) || text.length < head.length + tail.length) return null;
    return before;
  }
}

/** One composer's answer to the host: `arm` (true when a composition was open to arm) and `disarm`. */
export interface CancelTarget {
  arm: () => boolean;
  disarm: () => void;
}
const targets = new Set<CancelTarget>();

declare global {
  interface Window {
    __eitriImeCancel?: (arm?: boolean) => string;
  }
}

/** Makes `target` answer the host's cancel until the returned function is called. `__eitriImeCancel()` arms,
 * `__eitriImeCancel(false)` disarms what an earlier call armed. */
export function registerImeCancel(target: CancelTarget, w: Window = window): () => void {
  targets.add(target);
  w.__eitriImeCancel = (arm = true) => {
    if (!arm) {
      for (const each of targets) each.disarm();
      return "disarmed";
    }
    let armed = 0;
    for (const each of targets) if (each.arm()) armed += 1;
    return armed > 0 ? "armed" : "idle";
  };
  return () => {
    targets.delete(target);
  };
}
