/* Test support, never bundled: jsdom lays nothing out. Each tagged leaf block gets 20 px per source line (100 more
   for one whose text says TALL) and a 10 px gap, stacked in document order; the end sentinel gets 1 px. A block's
   descendants share its box. Scrolling moves `window.scrollY` and fires `scroll`, as a browser would. */
export interface FakeLayout { restore(): void; setScroll(y: number): void; boxOf(el: Element): { top: number; height: number } | undefined }

export function installFakeLayout(root: HTMLElement): FakeLayout {
  const layOut = () => {
    const pos = new Map<Element, { top: number; height: number }>();
    let y = 0;
    for (const el of root.querySelectorAll<HTMLElement>(".code-line")) {
      if (el.querySelector(".code-line")) continue;
      const line = Number(el.dataset.line);
      const height = el.dataset.lineEnd === undefined ? 1
        : (Number(el.dataset.lineEnd) - line) * 20 + (el.textContent?.includes("TALL") ? 100 : 0);
      pos.set(el, { top: y, height });
      y += height + 10;
    }
    return pos;
  };
  const boxOf = (el: Element) => {
    const pos = layOut();
    for (let e: Element | null = el; e; e = e.parentElement) { const p = pos.get(e); if (p) return p; }
    return undefined;
  };
  const setScroll = (y: number) => {
    Object.defineProperty(window, "scrollY", { value: y, configurable: true, writable: true });
    window.dispatchEvent(new Event("scroll"));
  };
  const original = Element.prototype.getBoundingClientRect;
  const originalTo = window.scrollTo, originalBy = window.scrollBy;
  Element.prototype.getBoundingClientRect = function (this: Element) {
    const p = boxOf(this);
    const top = (p?.top ?? 0) - window.scrollY;
    return { top, height: p?.height ?? 0, bottom: top + (p?.height ?? 0), left: 0, right: 600, width: 600, x: 0, y: top, toJSON: () => ({}) } as DOMRect;
  };
  Object.defineProperty(document.documentElement, "scrollHeight", {
    configurable: true, get: () => [...layOut().values()].reduce((m, p) => Math.max(m, p.top + p.height), 0) + window.innerHeight,
  });
  window.scrollTo = ((opts: ScrollToOptions) => setScroll(opts.top ?? 0)) as typeof window.scrollTo;
  window.scrollBy = ((opts: ScrollToOptions) => setScroll(window.scrollY + (opts.top ?? 0))) as typeof window.scrollBy;
  Object.defineProperty(window, "scrollY", { value: 0, configurable: true, writable: true });
  return {
    setScroll,
    boxOf,
    restore() { Element.prototype.getBoundingClientRect = original; window.scrollTo = originalTo; window.scrollBy = originalBy; },
  };
}
