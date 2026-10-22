import { GUARD_MS } from "./sync";

/** Tells the scroll events a programmatic scroll causes from the user's own: a sync scroll must not be taken for the
 *  reader moving, or the preview would send it back to nvim. The guard ends on the first frame after the position
 *  reaches its target, or after GUARD_MS. */
export class ScrollGuard {
  private until = 0;
  private target = 0;
  private armed = 0;

  constructor(
    private readonly now: () => number = Date.now,
    private readonly frame: (fn: () => void) => void = (fn) => {
      if (typeof window.requestAnimationFrame === "function") window.requestAnimationFrame(() => fn());
      else setTimeout(fn, 16);
    },
  ) {}

  arm(target: number): void {
    this.target = target;
    this.until = this.now() + GUARD_MS;
    this.armed++;
  }

  /** True when this scroll event belongs to the last programmatic scroll. */
  onScroll(scrollY: number): boolean {
    if (this.now() >= this.until) return false;
    if (Math.abs(scrollY - this.target) < 1) {
      const which = this.armed;
      this.frame(() => {
        if (this.armed === which) this.until = 0;
      });
    }
    return true;
  }
}
