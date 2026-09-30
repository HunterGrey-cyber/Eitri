// @vitest-environment jsdom
import { afterEach, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render } from "@testing-library/react";
import { StatusBand } from "./StatusBand";
import type { BandFacts } from "../band";

afterEach(cleanup);

/** A `ResizeObserver` jsdom lacks, the same shape `MessageList.test.tsx` stubs with: records what
 *  it observes and delivers a synthetic entry on demand. */
type FakeObserver = { callback: ResizeObserverCallback; observed: Element[] };
function stubResizeObserver(): FakeObserver[] {
  const observers: FakeObserver[] = [];
  class FakeResizeObserver {
    private record: FakeObserver;
    constructor(callback: ResizeObserverCallback) {
      this.record = { callback, observed: [] };
      observers.push(this.record);
    }
    observe(el: Element) {
      this.record.observed.push(el);
    }
    unobserve() {}
    disconnect() {}
  }
  vi.stubGlobal("ResizeObserver", FakeResizeObserver);
  return observers;
}
const resize = (observer: FakeObserver, target: Element, width: number) =>
  observer.callback([{ target, contentRect: { width } } as unknown as ResizeObserverEntry], {} as ResizeObserver);

const RUNNING: BandFacts = {
  mode: "input",
  pill: "⏵⏵ auto",
  showcmd: "Space b…",
  message: null,
  prompt: null,
  warn: "Verdandi baseline drift: running 1234567.",
  unread: "↓3",
  cards: 1,
  queued: 1,
  context: { file: "neovibe.zsh", lines: [3, 9] },
  position: "14/30",
  model: "sonnet-5",
  usage: null,
};

/** Renders with a wide-enough band and a real character width, through the same fake observer
 *  `MessageList.test.tsx` uses -- jsdom never lays anything out, so without this every segment past
 *  mode/pill would be dropped by `bandLayout`'s own pre-measurement floor (Review Focus 3), and this
 *  suite would only ever see the two that never get dropped. */
function renderWide(facts: BandFacts, onOpenDetail = vi.fn(), onJump = vi.fn()) {
  const observers = stubResizeObserver();
  const rendered = render(<StatusBand facts={facts} paneFocused={true} onOpenDetail={onOpenDetail} onJump={onJump} />);
  const band = rendered.container.querySelector(".status-band")!;
  const measure = rendered.container.querySelector(".band-measure")!;
  act(() => {
    resize(observers[0], measure, 7.2);
    resize(observers[0], band, 900);
  });
  return rendered;
}

it("draws mode with data-mode and data-focused, dim when the pane lacks the keys", () => {
  const { container } = renderWide(RUNNING);
  const mode = container.querySelector<HTMLElement>('[data-testid="mode-block"]')!;
  expect(mode.dataset.mode).toBe("input");
  expect(mode.dataset.focused).toBe("true");
  expect(mode.textContent).toBe("INPUT");
});

it("draws the mode glyph in its own span, named for the permission mode", () => {
  const { container } = renderWide(RUNNING);
  const glyph = container.querySelector<HTMLElement>(".mode-glyph")!;
  expect(glyph.textContent).toBe("⏵⏵");
  expect(glyph.dataset.modeName).toBe("auto");
  expect(container.querySelector(".mode-pill")!.textContent).toBe("⏵⏵ auto");

  vi.unstubAllGlobals();
  const bypass = renderWide({ ...RUNNING, pill: "⏵⏵ bypass" });
  expect(bypass.container.querySelector<HTMLElement>(".mode-glyph")!.dataset.modeName).toBe("bypass");
});

it("the ↓N segment is a button that jumps and carries title G", () => {
  const onJump = vi.fn();
  const { container } = renderWide(RUNNING, vi.fn(), onJump);
  const unread = container.querySelector<HTMLButtonElement>(".band-unread")!;
  expect(unread.tagName).toBe("BUTTON");
  expect(unread.title).toBe("G");
  expect(unread.textContent).toBe("↓3");
  fireEvent.click(unread);
  expect(onJump).toHaveBeenCalledTimes(1);
});

it("a click elsewhere on the band opens the detail popover, not a click on ↓N", () => {
  const onOpenDetail = vi.fn();
  const onJump = vi.fn();
  const { container } = renderWide(RUNNING, onOpenDetail, onJump);
  fireEvent.click(container.querySelector(".band-unread")!);
  expect(onOpenDetail).not.toHaveBeenCalled();
  fireEvent.click(container.querySelector(".band-open")!);
  expect(onOpenDetail).toHaveBeenCalledTimes(1);
  expect(onJump).toHaveBeenCalledTimes(1); // from the previous click, unaffected by this one
});

it("⚠'s title holds the warning text, not just the glyph", () => {
  const { container } = renderWide(RUNNING);
  const warn = container.querySelector<HTMLElement>(".band-warn")!;
  expect(warn.textContent).toBe("⚠");
  expect(warn.title).toBe(RUNNING.warn);
});

it("the showcmd segment carries data-testid=showcmd", () => {
  const { container } = renderWide(RUNNING);
  const showcmd = container.querySelector<HTMLElement>('[data-testid="showcmd"]')!;
  expect(showcmd.textContent).toBe("Space b…");
});

/** R5 (v1 picks, Task 13): `bandLayout` decides whether the usage segment is drawn and where; the
 *  component's own job is the markup -- the text on the segment and the breakdown as its tooltip,
 *  the way `⚠` carries its warning in `title`. */
it("the usage segment carries the text and the breakdown as its title, right of the model", () => {
  const usage = { text: "1.2M tok $0.42", title: "input 10 · output 5,000 · cache write 200,000 · cache read 1,000,000 · $0.4213 — since this tab started or resumed" };
  const { container } = renderWide({ ...RUNNING, usage });
  const seg = container.querySelector<HTMLElement>(".band-usage")!;
  expect(seg).not.toBeNull();
  expect(seg.textContent).toBe("1.2M tok $0.42");
  expect(seg.title).toBe(usage.title);
  expect(seg.classList.contains("band-seg")).toBe(true);
  const right = Array.from(container.querySelectorAll(".band-right .band-seg")).map((el) => el.className.replace("band-seg ", ""));
  expect(right.indexOf("band-usage")).toBe(right.indexOf("band-model") + 1);
});

it("no usage fact draws no usage segment, at any width", () => {
  const { container } = renderWide(RUNNING);
  expect(container.querySelector(".band-usage")).toBeNull();
});

it("a narrow band drops the usage segment first, keeping the model and the position", () => {
  const usage = { text: "1.2M tok $0.42", title: "t" };
  const observers = stubResizeObserver();
  // No showcmd and no warning here, so that 72 columns (520 / 7.2) hold everything but the usage
  // segment: 64 columns without it, 80 with it.
  const { container } = render(<StatusBand facts={{ ...RUNNING, showcmd: null, warn: null, usage }} paneFocused={true} />);
  const band = container.querySelector(".status-band")!;
  const measure = container.querySelector(".band-measure")!;
  act(() => {
    resize(observers[0], measure, 7.2);
    resize(observers[0], band, 520);
  });
  expect(container.querySelector(".band-usage")).toBeNull();
  expect(container.querySelector(".band-model")).not.toBeNull();
  expect(container.querySelector(".band-position")).not.toBeNull();
});

/** C1c (spec §3.4): the band stops being a `j`/`k` stop -- its own details stay reachable on
 *  `<leader>i`, `prefix i` and a click (`onOpenDetail`, exercised above), none of which go through
 *  `nav.ts`'s `data-nav-stop` list. */
it("carries no data-nav-stop -- j/k never land here, only click/leader/prefix reach its details", () => {
  const { container } = renderWide(RUNNING);
  expect(container.querySelector(".status-band")!.hasAttribute("data-nav-stop")).toBe(false);
});

/** Defect 1 (2026-09-27 sandbox GUI pass): the owner's own WebKit zoom 1.5 measured the panel at
 *  520 logical px = 346 CSS px -- a 47-column budget at this suite's own 7.2px character width
 *  (47 * 7.2 = 338.4). `bandLayout` no longer truncates a prompt/flash that does not fit there; this
 *  is the render-level half of that fix: `StatusBand` must add `.status-band--wrap` exactly when
 *  that happens, and leave it off when the text already fits (`ids`/`bandLayout`'s own case in
 *  `band.test.ts` covers the pure logic; this covers what the component does with it). */
function renderNarrow(facts: BandFacts) {
  const observers = stubResizeObserver();
  const rendered = render(<StatusBand facts={facts} paneFocused={true} />);
  const band = rendered.container.querySelector(".status-band")!;
  const measure = rendered.container.querySelector(".band-measure")!;
  act(() => {
    resize(observers[0], measure, 7.2);
    resize(observers[0], band, 340); // floor(340 / 7.2) === 47 columns
  });
  return rendered;
}

it("defect 1: a bypass prompt too wide for the band wraps instead of truncating, (y/n) intact", () => {
  const R06_PROMPT = "切到 bypass 并批准 1 张等待中的卡片？(y/n)";
  const { container } = renderNarrow({ ...RUNNING, prompt: R06_PROMPT, message: null });
  expect(container.querySelector(".status-band")!.classList.contains("status-band--wrap")).toBe(true);
  const prompt = container.querySelector(".band-prompt")!;
  expect(prompt.textContent).toBe(R06_PROMPT);
  expect(prompt.textContent).not.toContain("…");
});

it("defect 1: the D11 flash is not truncated either, and wraps the band", () => {
  const D11_FLASH = "y must be pressed on its own to enter bypass — Shift+Tab to ask again";
  const { container } = renderNarrow({ ...RUNNING, prompt: null, message: D11_FLASH });
  expect(container.querySelector(".status-band")!.classList.contains("status-band--wrap")).toBe(true);
  const message = container.querySelector(".band-message")!;
  expect(message.textContent).toBe(D11_FLASH);
  expect(message.textContent).not.toContain("…");
});

it("defect 1: a short prompt does not switch the band into wrap mode -- it still fits one row", () => {
  const { container } = renderNarrow({ ...RUNNING, prompt: 'close 1 "docs"? (y/n)', message: null });
  expect(container.querySelector(".status-band")!.classList.contains("status-band--wrap")).toBe(false);
  expect(container.querySelector(".band-prompt")!.textContent).toBe('close 1 "docs"? (y/n)');
});

it("before any measurement (Review Focus 3), only mode and pill show -- no crash on an unmounted ref", () => {
  const { container } = render(<StatusBand facts={RUNNING} paneFocused={false} />);
  const mode = container.querySelector<HTMLElement>('[data-testid="mode-block"]')!;
  // v1 polish F24: no mode word while the pane lacks the keys; the block itself stays, dim.
  expect(mode.textContent).toBe("");
  expect(mode.dataset.mode).toBe("input");
  expect(mode.dataset.focused).toBe("false");
  expect(container.querySelector(".mode-pill")).not.toBeNull();
  expect(container.querySelector(".band-warn")).toBeNull();
  expect(container.querySelector(".band-unread")).toBeNull();
});
