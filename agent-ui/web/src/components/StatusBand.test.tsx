// @vitest-environment jsdom
import { afterEach, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render } from "@testing-library/react";
import { StatusBand } from "./StatusBand";
import { bandLayout, type BandFacts } from "../band";
import { UNREAD_SHOW_MS } from "../steadyBand";
import { SHORT_FLASH, cardKeyGoneFlash } from "../app/flashes";

const A_ON_CARD_FLASH = cardKeyGoneFlash("a");

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
  context: { file: "eitri.zsh", lines: [3, 9] },
  position: "14/30",
  model: "sonnet-5",
  usage: null,
};

/** Renders with a wide-enough band and a real character width, through the same fake observer
 *  `MessageList.test.tsx` uses -- jsdom never lays anything out, so without this every segment past
 *  mode/pill would be dropped by `bandLayout`'s own pre-measurement floor, and this
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
  expect(mode.textContent).toBe("INSERT");
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

/** `renderWide`, then the half second `↓N` waits before it is drawn (`UNREAD_SHOW_MS`). */
function renderWideUnread(facts: BandFacts, onOpenDetail = vi.fn(), onJump = vi.fn()) {
  vi.useFakeTimers();
  const rendered = renderWide(facts, onOpenDetail, onJump);
  act(() => {
    vi.advanceTimersByTime(UNREAD_SHOW_MS);
  });
  vi.useRealTimers();
  return rendered;
}

it("the ↓N segment is a button that jumps and carries title G", () => {
  const onJump = vi.fn();
  const { container } = renderWideUnread(RUNNING, vi.fn(), onJump);
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
  const { container } = renderWideUnread(RUNNING, onOpenDetail, onJump);
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

it("a link segment carries data-testid=band-link and its text", () => {
  const link = { state: "none" as const, text: "no editor attached: run :EitriPanel in nvim" };
  const { container } = renderWide({ ...RUNNING, context: null, link });
  expect(container.querySelector<HTMLElement>('[data-testid="band-link"]')!.textContent).toBe(link.text);
});

/** `bandLayout` decides whether the usage segment is drawn and where; the
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

/** The band stops being a `j`/`k` stop -- its own details stay reachable on
 *  `<leader>i`, `prefix i` and a click (`onOpenDetail`, exercised above), none of which go through
 *  `nav.ts`'s `data-nav-stop` list. */
it("carries no data-nav-stop -- j/k never land here, only click/leader/prefix reach its details", () => {
  const { container } = renderWide(RUNNING);
  expect(container.querySelector(".status-band")!.hasAttribute("data-nav-stop")).toBe(false);
});

/** A WebKit zoom of 1.5 measured the panel at
 *  520 logical px = 346 CSS px -- a 47-column budget at this suite's own 7.2px character width
 *  (47 * 7.2 = 338.4). `bandLayout` no longer truncates a prompt/flash that does not fit there; this
 *  is the render-level half of that fix: `StatusBand` must draw the prompt above the row exactly when
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

it("a bypass prompt too wide for the band goes whole onto its own lines above the row, (y/n) intact", () => {
  const BYPASS_CARD_PROMPT = "Switch to bypass and approve the 1 waiting card? (y/n)";
  const { container } = renderNarrow({ ...RUNNING, prompt: BYPASS_CARD_PROMPT, message: null });
  expect(container.querySelector(".status-band .band-prompt")).toBeNull();
  expect(container.querySelector(".status-band .band-mode")).not.toBeNull();
  const prompt = container.querySelector(".status-band-above .band-prompt")!;
  expect(prompt.textContent).toBe(BYPASS_CARD_PROMPT);
  expect(prompt.textContent).not.toContain("…");
});

it("a flash too wide for the band never wraps it: one row, cut, the whole sentence as its title", () => {
  const BYPASS_ENTER_FLASH = "y must be pressed on its own to enter bypass — Shift+Tab to ask again";
  const { container } = renderNarrow({ ...RUNNING, prompt: null, message: BYPASS_ENTER_FLASH });
  expect(container.querySelector(".status-band-above")).toBeNull();
  const message = container.querySelector<HTMLElement>(".status-band .band-message")!;
  // Whole in the markup: `.band-message`'s ellipsis ends it where the row ends.
  expect(message.textContent).toBe(BYPASS_ENTER_FLASH);
  expect(message.title).toBe(BYPASS_ENTER_FLASH);
});

it("a short prompt does not switch the band into wrap mode -- it still fits one row", () => {
  const { container } = renderNarrow({ ...RUNNING, prompt: 'close 1 "docs"? (y/n)', message: null });
  expect(container.querySelector(".status-band-above")).toBeNull();
  expect(container.querySelector(".status-band .band-prompt")!.textContent).toBe('close 1 "docs"? (y/n)');
});

it("before any measurement, only mode and pill show -- no crash on an unmounted ref", () => {
  const { container } = render(<StatusBand facts={RUNNING} paneFocused={false} />);
  const mode = container.querySelector<HTMLElement>('[data-testid="mode-block"]')!;
  // No mode word while the pane lacks the keys; the block itself stays, dim.
  expect(mode.textContent).toBe("");
  expect(mode.dataset.mode).toBe("input");
  expect(mode.dataset.focused).toBe("false");
  expect(container.querySelector(".mode-pill")).not.toBeNull();
  expect(container.querySelector(".band-warn")).toBeNull();
  expect(container.querySelector(".band-unread")).toBeNull();
});

/** How the latest turn ended: a few words in the left group, drawn only while the fact is set. */
it("a turn ending is a plain segment on the left, and no fact draws none", () => {
  const { container } = renderWide({ ...RUNNING, ending: { kind: "limit_reached", reason: "budget_exhausted", apiErrorStatus: null, message: null } });
  const seg = container.querySelector<HTMLElement>(".band-open .band-ending")!;
  expect(seg.textContent).toBe("spending limit");
  expect(seg.classList.contains("band-seg")).toBe(true);
  cleanup();
  expect(renderWide({ ...RUNNING, ending: null }).container.querySelector(".band-ending")).toBeNull();
});

it("draws the interrupted-revert notice with the review pointer off, in the band's own colour", () => {
  const { container } = renderWide({ ...RUNNING, review: null, recovery: { path: "core/src/x.rs", more: 0 } });
  const notice = container.querySelector<HTMLElement>(".band-recovery")!;
  expect(notice.textContent).toBe("an interrupted revert left core/src/x.rs · c to review");
  expect(container.querySelector(".band-review")).toBeNull();
  expect(container.querySelectorAll(".band-recovery[style]")).toHaveLength(0);
});

/** The dim mode block says which key brings the keys here, and the key is the window's own (`panelEntryKey`),
 *  not a literal: remapped it must read the remapped one, and with none known the clause is left out. */
it("names the panel-entry key in the dim mode block's title, and leaves the clause out where there is none", () => {
  const title = (props: { entryKey?: string | null; paneFocused?: boolean }) => {
    const { container } = render(<StatusBand facts={RUNNING} paneFocused={props.paneFocused ?? false} entryKey={props.entryKey} />);
    const text = container.querySelector<HTMLElement>('[data-testid="mode-block"]')!.getAttribute("title");
    cleanup();
    return text;
  };
  expect(title({ entryKey: "Ctrl+l" })).toBe("This pane does not have keyboard focus (Ctrl+l to focus it)");
  expect(title({ entryKey: "Alt+l" })).toBe("This pane does not have keyboard focus (Alt+l to focus it)");
  expect(title({ entryKey: null })).toBe("This pane does not have keyboard focus");
  expect(title({})).toBe("This pane does not have keyboard focus");
  // With the keys there is no title at all.
  expect(title({ entryKey: "Ctrl+l", paneFocused: true })).toBeNull();
});

it("draws no Ctrl+y segment while a card waits in INPUT", () => {
  const { container } = renderWide({ ...RUNNING, mode: "input", warn: null, cards: 1 });
  expect(container.querySelector(".band-approve")).toBeNull();
  expect(container.querySelector(".status-band")!.textContent).not.toContain("Ctrl+y");
});

function renderPicks(facts: BandFacts, picks: { onPickModel?: () => void; onPickEffort?: () => void }, onOpenDetail = vi.fn()) {
  const observers = stubResizeObserver();
  const rendered = render(<StatusBand facts={facts} paneFocused={true} onOpenDetail={onOpenDetail} {...picks} />);
  act(() => {
    resize(observers[0], rendered.container.querySelector(".band-measure")!, 7.2);
    resize(observers[0], rendered.container.querySelector(".status-band")!, 900);
  });
  return rendered;
}

it("the model and the effort open their pickers on a click, and nothing else opens with them", () => {
  const onPickModel = vi.fn();
  const onPickEffort = vi.fn();
  const onOpenDetail = vi.fn();
  const { container } = renderPicks({ ...RUNNING, effort: "xhigh" }, { onPickModel, onPickEffort }, onOpenDetail);
  fireEvent.click(container.querySelector<HTMLElement>("button.band-model")!);
  expect(onPickModel).toHaveBeenCalledTimes(1);
  expect(onPickEffort).not.toHaveBeenCalled();
  const effort = container.querySelector<HTMLElement>("button.band-effort")!;
  expect(effort.textContent).toBe("· xhigh");
  fireEvent.click(effort);
  expect(onPickEffort).toHaveBeenCalledTimes(1);
  expect(onOpenDetail).not.toHaveBeenCalled();
});

it("an agent with nothing to choose shows what it reports as plain text", () => {
  const { container } = renderPicks({ ...RUNNING, effort: "high" }, {});
  expect(container.querySelector("button.band-model")).toBeNull();
  expect(container.querySelector("button.band-effort")).toBeNull();
  expect(container.querySelector("span.band-effort")!.textContent).toBe("· high");
  expect(container.querySelector("span.band-model")!.textContent).toBe("sonnet-5");
});

it("an effort nobody reported draws nothing, not even a place for it", () => {
  const { container } = renderPicks(RUNNING, { onPickModel: vi.fn(), onPickEffort: vi.fn() });
  expect(container.querySelector("button.band-model")).not.toBeNull();
  expect(container.querySelector(".band-effort")).toBeNull();
});

it("shows the host's partial sandbox in the phone mark title", () => {
  stubResizeObserver();
  const { container, rerender } = render(<StatusBand facts={{ ...RUNNING, phones: 1, remoteSandbox: "partial" }} paneFocused />);
  expect(container.querySelector(".band-phone")?.getAttribute("title")).toContain("sandbox: partial");
  rerender(<StatusBand facts={{ ...RUNNING, phones: 1, remoteSandbox: "full" }} paneFocused />);
  expect(container.querySelector(".band-phone")?.getAttribute("title")).not.toContain("partial");
  rerender(<StatusBand facts={{ ...RUNNING, phones: 0, remoteSandbox: "partial" }} paneFocused />);
  expect(container.querySelector(".band-phone")).toBeNull();
});

/** The band as a turn streams: `facts` re-rendered every 100 ms, as the panel renders on every item. */
function streamBand(make: (i: number) => BandFacts, n: number) {
  vi.useFakeTimers();
  vi.setSystemTime(100_000);
  const observers = stubResizeObserver();
  const rendered = render(<StatusBand facts={make(0)} paneFocused={true} onOpenDetail={vi.fn()} onJump={vi.fn()} />);
  act(() => {
    resize(observers[0], rendered.container.querySelector(".band-measure")!, 7.2);
    resize(observers[0], rendered.container.querySelector(".status-band")!, 900);
  });
  const drawn: (string | null)[][] = [];
  const read = () => [".band-position", ".band-unread", ".band-usage"].map((sel) => rendered.container.querySelector(sel)?.textContent ?? null);
  for (let i = 1; i < n; i++) {
    act(() => {
      vi.advanceTimersByTime(100);
    });
    rendered.rerender(<StatusBand facts={make(i)} paneFocused={true} onOpenDetail={vi.fn()} onJump={vi.fn()} />);
    drawn.push(read());
  }
  return { ...rendered, drawn, read };
}
const redraws = (drawn: (string | null)[][], k: number) => drawn.filter((d, i) => i > 0 && d[k] !== drawn[i - 1][k]).length;

it("while a turn streams the position is redrawn at most about once a second, then lands on the newest", () => {
  const band = streamBand((i) => ({ ...RUNNING, unread: null, streaming: true, position: `${i + 1}/${i + 1}` }), 40);
  expect(redraws(band.drawn, 0)).toBeLessThanOrEqual(4);
  act(() => {
    vi.advanceTimersByTime(1000);
  });
  expect(band.read()[0]).toBe("40/40");
  vi.useRealTimers();
});

it("↓N is not drawn for a passing moment of unseen rows, and stays once it has been up", () => {
  const band = streamBand((i) => ({ ...RUNNING, streaming: true, unread: i < 3 ? null : i < 5 ? "↓1" : i < 7 ? null : `↓${i - 6}` }), 30);
  // i = 3..4: unseen for 200 ms, then gone -- never drawn.
  expect(band.drawn.slice(0, 7).every((d) => d[1] === null)).toBe(true);
  // From i = 7 on it is there for good: drawn after half a second, and never taken away again.
  const first = band.drawn.findIndex((d) => d[1] !== null);
  expect(first).toBeGreaterThanOrEqual(10);
  expect(band.drawn.slice(first).every((d) => d[1] !== null)).toBe(true);
  expect(redraws(band.drawn.slice(first), 1)).toBeLessThanOrEqual(2);
  vi.useRealTimers();
});

it("a figure keeps its reserved width, so a count gaining a digit moves nothing beside it", () => {
  const band = streamBand((i) => ({ ...RUNNING, unread: null, streaming: true, position: `${i + 1}/120` }), 3);
  const seg = band.container.querySelector<HTMLElement>(".band-position")!;
  expect(seg.style.minWidth).toBe("7ch");
  vi.useRealTimers();
});

it("a tab switch starts the band's layout afresh: nothing dropped on the last tab stays dropped on this one", () => {
  vi.useFakeTimers();
  vi.setSystemTime(100_000);
  const observers = stubResizeObserver();
  const narrowA: BandFacts = { ...RUNNING, unread: null, showcmd: null, warn: null, cards: 0, queued: 0, context: null, position: "100/100", scope: 1 };
  const rendered = render(<StatusBand facts={narrowA} paneFocused={true} onOpenDetail={vi.fn()} onJump={vi.fn()} />);
  // Wide enough for every segment of tab B, too narrow for A's model beside its wide position.
  const cells = (() => {
    for (let n = 20; n < 80; n++) {
      const room = n * 7.2;
      const a = bandLayout(narrowA, room, 7.2).map((x) => x.id);
      const b = bandLayout({ ...narrowA, position: "1/1" }, room + 3 * 7.2, 7.2).map((x) => x.id);
      if (!a.includes("model") && b.includes("model") && bandLayout({ ...narrowA, position: "1/1" }, room, 7.2).some((x) => x.id === "model")) return n;
    }
    throw new Error("no such width");
  })();
  act(() => {
    resize(observers[0], rendered.container.querySelector(".band-measure")!, 7.2);
    resize(observers[0], rendered.container.querySelector(".status-band")!, cells * 7.2);
  });
  expect(rendered.container.querySelector(".band-model")).toBeNull();
  act(() => {
    vi.advanceTimersByTime(50);
  });
  rendered.rerender(<StatusBand facts={{ ...narrowA, position: "1/1", scope: 2 }} paneFocused={true} onOpenDetail={vi.fn()} onJump={vi.fn()} />);
  expect(rendered.container.querySelector(".band-model")).not.toBeNull();
  vi.useRealTimers();
});

it("a flash drawn in its short form carries the whole sentence as its title", () => {
  const observers = stubResizeObserver();
  const facts: BandFacts = { ...RUNNING, mode: "browse", unread: null, showcmd: null, warn: null, cards: 3, queued: 0, context: null, pill: "⏵⏵ manual", message: A_ON_CARD_FLASH };
  const rendered = render(<StatusBand facts={facts} paneFocused={true} onOpenDetail={vi.fn()} onJump={vi.fn()} />);
  act(() => {
    resize(observers[0], rendered.container.querySelector(".band-measure")!, 7.2);
    resize(observers[0], rendered.container.querySelector(".status-band")!, 37 * 7.2);
  });
  const message = rendered.container.querySelector<HTMLElement>(".band-message")!;
  expect(message.textContent).toBe(SHORT_FLASH.get(A_ON_CARD_FLASH));
  expect(message.title).toBe(A_ON_CARD_FLASH);
  vi.unstubAllGlobals();
});
