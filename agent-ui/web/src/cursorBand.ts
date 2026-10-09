/** The conversation cursor's fill, drawn as one band behind the rows that glides from row to row, as
 *  Neovide's own cursor glides from cell to cell, instead of a fill on each row that snaps.
 *
 *  The band is an absolutely positioned element inside the list (`MessageList`), in the list's content
 *  space, so it scrolls with the rows; it sits under every row's content (`index.css`, `.cursor-band`) and
 *  takes no pointer events, so it hides no text, no selection and no button. Only the fill moves: the
 *  row's sign cell, its border and a card's own look stay on the row. */
import { reducedMotion } from "./jkScroll";

/** How long the band takes to reach a new row: quick, so a held key's run of rows reads as one movement
 *  that keeps up, and a single press still shows where the cursor came from. */
export const CURSOR_GLIDE_MS = 100;

export type BandBox = { left: number; top: number; width: number; height: number };

const easeOut = (t: number) => 1 - (1 - t) ** 3;

function mix(from: BandBox, to: BandBox, t: number): BandBox {
  const at = (a: number, b: number) => a + (b - a) * t;
  return { left: at(from.left, to.left), top: at(from.top, to.top), width: at(from.width, to.width), height: at(from.height, to.height) };
}

function sameBox(a: BandBox, b: BandBox): boolean {
  return a.left === b.left && a.top === b.top && a.width === b.width && a.height === b.height;
}

export class CursorBand {
  /** What the band shows now (mid-glide, where it is this frame); `null` while hidden. */
  private shown: BandBox | null = null;
  private glide: { from: BandBox; to: BandBox; startedAt: number; frame: number } | null = null;

  constructor(private readonly el: HTMLElement) {}

  /** Puts the band on `box` (`null`: no row, hidden). With `glide` it moves there from where it is over
   *  `CURSOR_GLIDE_MS`, picking up from mid-glide when one is running; without it, or with reduced motion,
   *  or when it was hidden, it is there at once. */
  place(box: BandBox | null, glide: boolean): void {
    if (box === null) {
      this.stop();
      this.shown = null;
      this.el.dataset.row = "none";
      return;
    }
    delete this.el.dataset.row;
    const from = this.shown;
    if (from !== null && this.glide !== null && sameBox(this.glide.to, box)) return;
    this.stop();
    if (!glide || from === null || sameBox(from, box) || reducedMotion()) {
      this.write(box);
      return;
    }
    const run = { from, to: box, startedAt: performance.now(), frame: 0 };
    const step = (now: number) => {
      if (this.glide !== run) return;
      const t = Math.min(1, Math.max(0, now - run.startedAt) / CURSOR_GLIDE_MS);
      this.write(t >= 1 ? run.to : mix(run.from, run.to, easeOut(t)));
      if (t >= 1) this.glide = null;
      else run.frame = requestAnimationFrame(step);
    };
    this.glide = run;
    run.frame = requestAnimationFrame(step);
  }

  /** Where the band is heading (or is), for tests. */
  get target(): BandBox | null {
    return this.glide?.to ?? this.shown;
  }

  /** Stops a running glide where it is. */
  stop(): void {
    if (this.glide === null) return;
    cancelAnimationFrame(this.glide.frame);
    this.glide = null;
  }

  private write(box: BandBox): void {
    this.shown = box;
    const s = this.el.style;
    s.transform = `translate(${box.left}px, ${box.top}px)`;
    s.width = `${box.width}px`;
    s.height = `${box.height}px`;
  }
}
