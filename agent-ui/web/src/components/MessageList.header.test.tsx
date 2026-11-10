// @vitest-environment jsdom
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render } from "@testing-library/react";
import { MessageList } from "./MessageList";
import { noteUserScroll } from "../follow";
import { initialState } from "../reducer";
import type { AgentUiState } from "../types";
import { conversationRows } from "../nav";
import css from "../index.css?raw";

/* The pinned prompt header (`../promptHeader`) as `MessageList` draws it: when it shows, what it says, where it sits
   in the page, and what a click on it does. jsdom lays nothing out, so the rows are stacked at planted heights in a
   300px list whose rects follow `scrollTop`; the header's own rect is planted as 24px at the list's top. Which rows
   it then counts as on screen is the `effectiveTop` rule, pinned per user in `rowScroll.test.ts`,
   `jkScroll.test.ts`, `viewAnchor.test.ts` and `HintLayer.test.tsx`. */

afterEach(cleanup);

beforeAll(() => {
  Element.prototype.scrollIntoView = vi.fn();
});

const realRect = HTMLElement.prototype.getBoundingClientRect;
beforeEach(() => {
  HTMLElement.prototype.getBoundingClientRect = function (this: HTMLElement) {
    return this.classList.contains("prompt-header") ? ({ top: 0, bottom: 24 } as DOMRect) : realRect.call(this);
  };
});
afterEach(() => {
  HTMLElement.prototype.getBoundingClientRect = realRect;
});

// Rows: prompt 0, two replies, prompt 3, four replies: eight rows of 100px.
const STATE: AgentUiState = {
  ...initialState(),
  userPrompts: [
    { seq: 0, text: "first question\n  over two lines" },
    { seq: 3, text: "second question" },
  ],
  transcript: [1, 2, 4, 5, 6, 7].map((seq) => ({ seq, text: `reply ${seq}` })),
};
const ROW = 100;
const VIEW = 300;

function layout(list: HTMLElement, opts: { offsetWidth?: number } = {}) {
  const rows = conversationRows(list);
  expect(rows).toHaveLength(8);
  const total = rows.length * ROW;
  let scrollTop = total - VIEW;
  Object.defineProperty(list, "clientHeight", { configurable: true, value: VIEW });
  Object.defineProperty(list, "scrollHeight", { configurable: true, value: total });
  Object.defineProperty(list, "clientWidth", { configurable: true, value: 400 });
  Object.defineProperty(list, "offsetWidth", { configurable: true, value: opts.offsetWidth ?? 400 });
  Object.defineProperty(list, "scrollTop", {
    configurable: true,
    get: () => scrollTop,
    set: (v: number) => {
      scrollTop = Math.max(0, Math.min(total - VIEW, v));
    },
  });
  list.getBoundingClientRect = () => ({ top: 0, bottom: VIEW }) as DOMRect;
  rows.forEach((row, i) => {
    row.getBoundingClientRect = () => ({ top: i * ROW - scrollTop, bottom: (i + 1) * ROW - scrollTop }) as DOMRect;
  });
  // At the end and following, as a fresh conversation is.
  fireEvent.scroll(list);
  return {
    scrollTo: (top: number) => {
      list.scrollTop = top;
      fireEvent.scroll(list);
    },
  };
}

function mounted(props: Partial<Parameters<typeof MessageList>[0]> = {}) {
  const onPromptJump = vi.fn();
  const view = render(
    <MessageList state={STATE} sessionEnded={false} expanded={{}} cursor={-1} onAnswerPermission={vi.fn()} onPromptJump={onPromptJump} {...props} />,
  );
  const list = view.container.querySelector<HTMLElement>(".message-list")!;
  const header = () => view.container.querySelector<HTMLElement>(".prompt-header");
  return { ...view, list, header, onPromptJump };
}

/** Scrolls up as the reader's own move: following stops outright, as `k`/`gg` do. */
function readUp(list: HTMLElement, scroll: (top: number) => void, top: number) {
  noteUserScroll(list, "up");
  scroll(top);
}

describe("the pinned prompt header", () => {
  it("names the prompt above the top row, cut to one line, with its place among the prompts", () => {
    const { list, header } = mounted();
    const { scrollTo } = layout(list);
    expect(header()).toBeNull();
    readUp(list, scrollTo, 150); // row 1 (a reply under the first prompt) is the top row
    expect(header()?.querySelector(".prompt-header-text")?.textContent).toBe("first question over two lines");
    expect(header()?.querySelector(".prompt-header-count")?.textContent).toBe("prompt 1/2");
  });

  it("goes while the prompt's own row is on screen, and names the next prompt past it", () => {
    const { list, header } = mounted();
    const { scrollTo } = layout(list);
    readUp(list, scrollTo, 150);
    expect(header()).not.toBeNull();
    scrollTo(320); // the second prompt's row shows 80px at the top
    expect(header()).toBeNull();
    scrollTo(410); // past it
    expect(header()?.querySelector(".prompt-header-count")?.textContent).toBe("prompt 2/2");
    expect(header()?.querySelector(".prompt-header-text")?.textContent).toBe("second question");
  });

  it("a prompt row wholly under the header does not count as on screen", () => {
    const { list, header } = mounted();
    const { scrollTo } = layout(list);
    readUp(list, scrollTo, 410);
    scrollTo(390); // the second prompt's last 10px are back, under the 24px header
    expect(header()?.querySelector(".prompt-header-count")?.textContent).toBe("prompt 2/2");
    scrollTo(370); // 30px of it: below the header
    expect(header()).toBeNull();
  });

  it("is not drawn while the view follows the newest row", () => {
    const { list, header } = mounted();
    const { scrollTo } = layout(list);
    readUp(list, scrollTo, 410);
    expect(header()).not.toBeNull();
    scrollTo(500); // the true end: following again
    expect(header()).toBeNull();
  });

  it("is not drawn with no layout to judge, nor on the phone's page", () => {
    const bare = mounted();
    noteUserScroll(bare.list, "up");
    fireEvent.scroll(bare.list);
    expect(bare.header()).toBeNull();
    cleanup();
    const phone = mounted({ touch: true });
    const { scrollTo } = layout(phone.list);
    readUp(phone.list, scrollTo, 410);
    expect(phone.header()).toBeNull();
  });

  it("is the list's sibling, never in its flow: the list keeps its children and rows when it appears", () => {
    const { list, header } = mounted();
    const { scrollTo } = layout(list);
    const children = Array.from(list.children);
    const rows = conversationRows(list).length;
    readUp(list, scrollTo, 410);
    const shown = header()!;
    expect(list.contains(shown)).toBe(false);
    expect(shown.parentElement).toBe(list.parentElement);
    expect(Array.from(list.children)).toEqual(children);
    expect(conversationRows(list)).toHaveLength(rows);
    // It takes no focus and is not a stop: keys never land on it.
    expect(shown.tabIndex).toBe(-1);
    expect(shown.hasAttribute("data-nav-stop")).toBe(false);
    expect(shown.getAttribute("aria-hidden")).toBe("true");
  });

  it("stops short of the list's scrollbar", () => {
    const { list, header } = mounted();
    const { scrollTo } = layout(list, { offsetWidth: 410 });
    readUp(list, scrollTo, 410);
    expect(header()?.style.right).toBe("10px");
  });

  it("a click asks for that prompt's row by its key; the mousedown moves no focus", () => {
    const { list, header, onPromptJump } = mounted();
    const { scrollTo } = layout(list);
    readUp(list, scrollTo, 410);
    expect(fireEvent.mouseDown(header()!)).toBe(false); // default prevented
    fireEvent.click(header()!);
    expect(onPromptJump).toHaveBeenCalledWith("u-3");
  });
});

describe("the header never flickers on its own choice of top row", () => {
  const cause = (): import("../types").TurnCause => ({
    kind: "peer", taskId: null, taskKind: null, taskStatus: null, description: null, summary: null,
    peerName: "worker", text: "ready", wakeupSource: null, scheduledForMs: null, scheduleReason: null, cronId: null, prompt: null,
  });
  // Rows: prompt 0, reply 1, a wake the CLI started (seq 2), then three replies; heights below.
  const WAKE_STATE: AgentUiState = {
    ...initialState(),
    userPrompts: [{ seq: 0, text: "the question" }],
    transcript: [1, 3, 4, 5].map((seq) => ({ seq, text: `reply ${seq}` })),
    turnStarts: [{ seq: 2, turnId: "w", origin: "cli", cause: cause() }],
  };
  const HEIGHTS = [100, 100, 50, 100, 100, 100];

  function layoutHeights(list: HTMLElement) {
    const rows = conversationRows(list);
    expect(rows).toHaveLength(HEIGHTS.length);
    const tops = HEIGHTS.map((_, i) => HEIGHTS.slice(0, i).reduce((a, b) => a + b, 0));
    const total = HEIGHTS.reduce((a, b) => a + b, 0);
    let scrollTop = total - VIEW;
    Object.defineProperty(list, "clientHeight", { configurable: true, value: VIEW });
    Object.defineProperty(list, "scrollHeight", { configurable: true, value: total });
    Object.defineProperty(list, "clientWidth", { configurable: true, value: 400 });
    Object.defineProperty(list, "offsetWidth", { configurable: true, value: 400 });
    Object.defineProperty(list, "scrollTop", {
      configurable: true,
      get: () => scrollTop,
      set: (v: number) => {
        scrollTop = Math.max(0, Math.min(total - VIEW, v));
      },
    });
    list.getBoundingClientRect = () => ({ top: 0, bottom: VIEW }) as DOMRect;
    rows.forEach((row, i) => {
      row.getBoundingClientRect = () => ({ top: tops[i] - scrollTop, bottom: tops[i] + HEIGHTS[i] - scrollTop }) as DOMRect;
    });
    fireEvent.scroll(list);
    return (top: number) => {
      list.scrollTop = top;
      fireEvent.scroll(list);
    };
  }

  it("a reply ending 10px below the list's top with a wake right under it settles on one answer", () => {
    const { list, header } = mounted({ state: WAKE_STATE });
    const scrollTo = layoutHeights(list);
    noteUserScroll(list, "up");
    scrollTo(190); // reply 1 ends at 10px, the wake spans 10..60: the header's 24px band covers the reply's end
    const seen: boolean[] = [];
    for (let i = 0; i < 6; i++) {
      seen.push(header() !== null);
      fireEvent.scroll(list);
    }
    // The first row past the band is the wake (a turn no prompt row names): no header, however often it is asked.
    expect(seen).toEqual([false, false, false, false, false, false]);
  });
});

describe("the header's look (index.css)", () => {
  const block = (selector: string) => {
    const at = css.indexOf(`\n${selector} {`);
    expect(at, selector).toBeGreaterThan(-1);
    return css.slice(at, css.indexOf("}", at));
  };

  it("is one dim line, an editor row high, with a hairline and no shadow, over the list", () => {
    const header = block(".prompt-header");
    expect(header).toContain("position: absolute;");
    expect(header).toContain("height: var(--nv-editor-row, 24px);");
    expect(header).toContain("box-sizing: border-box;");
    expect(header).toContain("color: var(--nv-muted);");
    expect(header).toContain("background: var(--nv-bg);");
    expect(header).toContain("border-bottom: 1px solid var(--rule-soft);");
    expect(header).toContain("white-space: nowrap;");
    expect(header).not.toMatch(/box-shadow|border-radius|z-index|opacity/);
    expect(block(".prompt-header-text")).toContain("text-overflow: ellipsis;");
  });

  it("uses only variables both looks share, with main's 24px fallback for the row", () => {
    const rules = [".prompt-header", ".prompt-header:hover", ".prompt-header-text", ".prompt-header-count"].map(block).join("\n");
    const used = new Set([...rules.matchAll(/var\((--[\w-]+)/g)].map((m) => m[1]));
    expect([...used].sort()).toEqual(["--fs-sm", "--nv-bg", "--nv-editor-row", "--nv-fg", "--nv-muted", "--page-pad", "--rule-soft"]);
    for (const m of rules.matchAll(/var\(--nv-editor-row(, [^)]*)?\)/g)) expect(m[1]).toBe(", 24px");
    // The page's own tokens are declared at :root.
    for (const token of ["--fs-sm", "--page-pad", "--rule-soft"]) expect(css).toMatch(new RegExp(`:root \\{[^}]*${token}:`));
  });
});
