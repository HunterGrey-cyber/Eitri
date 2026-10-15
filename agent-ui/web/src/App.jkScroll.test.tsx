// @vitest-environment jsdom
/**
 * `j`/`k` over rows of very different heights: where the view goes when the cursor lands on a row
 * (`jkScroll.ts`'s `decideLanding`), what a press does inside a tall row or a tool result's box
 * (`decidePress`), how a count spends itself, and the ease-out. The decisions have their own pure tests
 * in `jkScroll.test.ts`; these drive the real `App` handlers to prove the wiring, over a fake layout.
 *
 * jsdom implements no layout, so `fakeLayout` stands one in: the rows stacked at the given heights in a
 * list `viewport` px tall, every rect following the list's `scrollTop`, which clamps to
 * [0, scrollHeight - clientHeight] the way a browser's does. Each row's text line is 20px, so a margin is
 * 40px, a step 60px and a reading line a third of the view. None of this proves what a real WebKit draws:
 * `shell/tests/panel_jk_scroll.rs` does.
 */
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render } from "@testing-library/react";
import App from "./App";
import { initialState } from "./reducer";
import type { AgentUiState, Hello } from "./types";
import { decideLanding, READING_LINE, SCROLL_ANIMATION_MS } from "./jkScroll";
import { TYPING_GUARD_MS } from "./typingGuard";

afterEach(cleanup);

beforeAll(() => {
  // jsdom implements no layout, so MessageList's auto-scroll would throw on a missing method.
  Element.prototype.scrollIntoView = vi.fn();
});

beforeEach(() => {
  (window as unknown as { webkit: unknown }).webkit = {
    messageHandlers: { eitriAgent: { postMessage: () => {} } },
  };
});

function dispatch(payload: unknown) {
  act(() => {
    window.__eitriDispatch!(JSON.stringify(payload));
  });
}

const HELLO: Hello = {
  backend: "legacy",
  projectDir: "/home/user/project",
  permissionModes: ["manual", "auto", "bypass"],
  resumableSessions: [],
  expectedVerdandiRevision: null,
  account: null,
};

const LIVE_TAB = {
  id: 1, number: 1, label: "1 new", name: null, state: "live", mode: "auto",
  marker: null, pending: 0, resumable: false, failure: null, title: null,
} as const;

function started() {
  const rendered = render(<App />);
  dispatch({ kind: "hello", ...HELLO });
  dispatch({ kind: "tabs", active: 1, tabs: [LIVE_TAB] });
  dispatch({ kind: "snapshot", tab: 1, throughRevision: 1, state: { ...initialState(), status: { kind: "running" } } as AgentUiState });
  return rendered;
}

function events(...list: unknown[]) {
  dispatch({ kind: "events", tab: 1, fromRevision: 0, throughRevision: list.length, events: list });
}

function prompts(...texts: string[]) {
  events(...texts.map((text) => ({ type: "user_prompt_submitted", text })));
}

const root = (c: HTMLElement) => c.querySelector<HTMLElement>(".agent-ui-conversation")!;
const press = (key: string, over: { ctrlKey?: boolean; shiftKey?: boolean; repeat?: boolean } = {}) =>
  fireEvent.keyDown(document.activeElement ?? document.body, { key, ...over });
const current = (c: HTMLElement) => c.querySelector(".row-current .row-body")!.textContent;
const onToolRow = (c: HTMLElement) => c.querySelector(".row-current .tool-result-body") !== null;

const LINE = 20;
const STEP = 3 * LINE;

/** Rows stacked at `heights`, a `viewport`px list, every rect following `scrollTop`. */
function fakeLayout(container: HTMLElement, heights: number[], viewport = 400) {
  const list = container.querySelector(".message-list") as HTMLElement;
  const rows = Array.from(list.querySelectorAll<HTMLElement>('[data-nav-stop="row"]'));
  expect(rows.length).toBe(heights.length);
  const total = heights.reduce((a, b) => a + b, 0);
  let scrollTop = 0;
  Object.defineProperty(list, "clientHeight", { value: viewport, configurable: true });
  Object.defineProperty(list, "scrollHeight", { value: total, configurable: true });
  Object.defineProperty(list, "scrollTop", {
    configurable: true,
    get: () => scrollTop,
    set: (v: number) => {
      scrollTop = Math.max(0, Math.min(total - viewport, v));
    },
  });
  list.getBoundingClientRect = () => ({ top: 0, bottom: viewport }) as DOMRect;
  let y = 0;
  rows.forEach((row, i) => {
    const top = y;
    y += heights[i];
    row.getBoundingClientRect = () => ({ top: top - scrollTop, bottom: top + heights[i] - scrollTop }) as DOMRect;
    row.querySelector<HTMLElement>(".row-body")!.style.lineHeight = `${LINE}px`;
    // The box of a tool row sits 30px under the row's top.
    const box = row.querySelector<HTMLElement>(".tool-result-body");
    if (box !== null) {
      box.getBoundingClientRect = () => ({ top: top + 30 - scrollTop, bottom: top + 30 + 260 - scrollTop }) as DOMRect;
    }
  });
  act(() => root(container).focus());
  return list;
}

describe("j/k landing on a row: where the view goes", () => {
  it("j into a row taller than the view puts its head a third of the way down, not at the very bottom", () => {
    const { container } = started();
    prompts("a", "b", "c");
    const list = fakeLayout(container, [360, 990, 400]);
    press("g");
    press("g");

    press("j"); // "b" starts on the bottom margin line (360) of the 400px view
    expect(current(container)).toBe("b");
    expect(list.scrollTop).toBe(227); // 360 - 400/3
    expect(list.scrollTop).toBeLessThanOrEqual(Math.ceil((400 * 2) / 3));
  });

  it("k into a row taller than the view lands on its end, two thirds down -- never a whole view away", () => {
    const { container } = started();
    prompts("a", "b", "c", "d");
    const list = fakeLayout(container, [360, 990, 100, 300]);
    press("G", { shiftKey: true }); // "d", the view at the end of the list (1350)
    expect(list.scrollTop).toBe(1350);
    press("k"); // "c" (1350..1450) sits on the view's top edge: its top goes to the top margin line
    expect(current(container)).toBe("c");
    expect(list.scrollTop).toBe(1310);

    press("k"); // "b" ends on "c"'s top, 40px inside the view: its end goes two thirds down
    expect(current(container)).toBe("b");
    expect(list.scrollTop).toBe(1083); // 1350 - 400 * 2 / 3
    expect(1310 - list.scrollTop).toBeLessThanOrEqual(Math.ceil((400 * 2) / 3));
  });

  it("does not scroll for a row that is already visible, with its margin", () => {
    const { container } = started();
    prompts("a", "b", "c", "d");
    const list = fakeLayout(container, [100, 100, 100, 700]);
    press("g");
    press("g");
    press("j");
    expect(current(container)).toBe("b");
    expect(list.scrollTop).toBe(0);
  });

  it("scrolls a row that fits by the least amount that leaves it a two-line margin", () => {
    const { container } = started();
    prompts("a", "b", "c");
    const list = fakeLayout(container, [330, 100, 700]);
    press("g");
    press("g");
    press("j"); // "b" runs 330..430 in a 400px view: its bottom goes to the bottom margin line (360)
    expect(current(container)).toBe("b");
    expect(list.scrollTop).toBe(70);
  });

  it("never asks the engine's scrollIntoView to place a row while there is a layout", () => {
    const { container } = started();
    prompts("a", "b", "c");
    const list = fakeLayout(container, [400, 990, 400]);
    press("g");
    press("g");
    const scrollIntoView = Element.prototype.scrollIntoView as ReturnType<typeof vi.fn>;
    scrollIntoView.mockClear();
    press("j");
    press("j");
    press("k");
    expect(scrollIntoView).not.toHaveBeenCalled();
    expect(list.scrollTop).toBeGreaterThan(0);
  });

  it("long, short, long: the short row slides in by its own height and the next long row is entered at its head", () => {
    const { container } = started();
    prompts("a", "b", "c", "d");
    const list = fakeLayout(container, [990, 100, 990, 200]);
    press("g");
    press("g");
    // read "a" to its end: 11 presses leave its end on the bottom margin line (990 - 400 + 40 = 630)
    for (let i = 0; i < 11; i++) press("j");
    expect(current(container)).toBe("a");
    expect(list.scrollTop).toBe(630);
    press("j"); // "b" (100px) at 990..1090 slides in: its bottom to the bottom margin line
    expect(current(container)).toBe("b");
    expect(list.scrollTop).toBe(730);
    press("j"); // "c" starts at 1090, below the view's bottom (1130 - 40): head to the reading line
    expect(current(container)).toBe("c");
    expect(list.scrollTop).toBe(Math.round(1090 - 400 / 3));
    // and no press moved the view by more than two thirds of it
  });

  it("with a stale scroll the other way: a row above the view comes back by the least scroll too", () => {
    const { container } = started();
    prompts("a", "b", "c");
    const list = fakeLayout(container, [100, 100, 1000]);
    press("g");
    press("g");
    list.scrollTop = 600; // the reader mouse-scrolled well past "a" and "b"
    press("j");
    // the cursor row ("a") was out of view: this press brings it back (as far up as the list goes), and
    // moves nothing else
    expect(list.scrollTop).toBe(0);
    expect(current(container)).toBe("a");
  });
});

describe("j/k inside a row taller than the view", () => {
  it("steps three lines a press, the last one leaving the row's end on the bottom margin line", () => {
    const { container } = started();
    prompts("a", "b", "c");
    const list = fakeLayout(container, [100, 990, 100]);
    press("g");
    press("g");
    press("j");
    expect(current(container)).toBe("b");
    expect(list.scrollTop).toBe(0); // its head (at 100) is above the reading line: nothing moves

    const seen: number[] = [];
    for (let i = 0; i < 13; i++) {
      press("j");
      seen.push(list.scrollTop);
    }
    // 1090 (the row's bottom) - 400 + 40 = 730: eleven 60px steps, a 60px one to 720, then the last 10px
    expect(seen).toEqual([60, 120, 180, 240, 300, 360, 420, 480, 540, 600, 660, 720, 730]);
    expect(current(container)).toBe("b");

    press("j");
    expect(current(container)).toBe("c");
  });

  it("k is the mirror: the last step leaves the row's top on the top margin line", () => {
    const { container } = started();
    prompts("a", "b", "c");
    const list = fakeLayout(container, [100, 990, 100]);
    press("G", { shiftKey: true });
    expect(current(container)).toBe("c");
    press("k"); // onto "b", whose bottom (1090) is on the view's bottom
    expect(current(container)).toBe("b");
    const seen: number[] = [];
    for (let i = 0; i < 14; i++) {
      press("k");
      if (current(container) !== "b") break;
      seen.push(list.scrollTop);
    }
    // top margin line: the row's top (100) - 40 = 60
    expect(seen[seen.length - 1]).toBe(60);
    expect(Math.min(...seen)).toBe(60);
    for (let i = 1; i < seen.length; i++) expect(seen[i - 1] - seen[i]).toBeLessThanOrEqual(STEP);
  });
});

describe("a count over a row's own steps", () => {
  it("5j inside a tall row is five steps, not one", () => {
    const { container } = started();
    prompts("a", "b", "c");
    const list = fakeLayout(container, [100, 990, 100]);
    press("g");
    press("g");
    press("j"); // into "b"
    press("5");
    press("j");
    expect(current(container)).toBe("b");
    expect(list.scrollTop).toBe(5 * STEP);
  });

  it("5k scrolls back five steps", () => {
    const { container } = started();
    prompts("a", "b", "c");
    const list = fakeLayout(container, [100, 990, 100]);
    press("g");
    press("g");
    press("j");
    press("9");
    press("j"); // nine steps down: 540
    expect(list.scrollTop).toBe(9 * STEP);
    press("5");
    press("k");
    expect(current(container)).toBe("b");
    expect(list.scrollTop).toBe(4 * STEP);
  });

  it("a count past what is left of the row spends the rest moving rows", () => {
    const { container } = started();
    prompts("a", "b", "c");
    const list = fakeLayout(container, [100, 990, 100]);
    press("g");
    press("g");
    press("j");
    // thirteen steps finish "b" (730); the other seven are row moves, of which there is one
    press("2");
    press("0");
    press("j");
    expect(current(container)).toBe("c");
    expect(list.scrollTop).toBeGreaterThanOrEqual(730);
  });

  it("a count over short rows still walks rows, as before", () => {
    const { container } = started();
    prompts("r0", "r1", "r2", "r3", "r4");
    fakeLayout(container, [60, 60, 60, 60, 60]);
    press("g");
    press("g");
    press("3");
    press("j");
    expect(current(container)).toBe("r3");
  });
});

describe("a tool result's own box", () => {
  function withBox() {
    const rendered = started();
    events(
      { type: "user_prompt_submitted", text: "before" },
      { type: "tool_call_started", turn_id: "t1", tool_use_id: "toolu_1", name: "Bash", input: { command: "long" } },
      { type: "tool_call_completed", turn_id: "t1", tool_use_id: "toolu_1", content: "a lot of output", is_error: false },
      { type: "user_prompt_submitted", text: "after" },
    );
    const r = root(rendered.container);
    fireEvent.keyDown(r, { key: "g" });
    fireEvent.keyDown(r, { key: "g" });
    fireEvent.keyDown(r, { key: "j" }); // onto the tool row
    fireEvent.keyDown(r, { key: "Enter" }); // unfold its result: `.tool-result-body` now exists
    const box = rendered.container.querySelector<HTMLElement>(".row-current .tool-result-body")!;
    expect(box).not.toBeNull();
    let boxTop = 0;
    Object.defineProperty(box, "scrollHeight", { value: 1000, configurable: true });
    Object.defineProperty(box, "clientHeight", { value: 260, configurable: true });
    Object.defineProperty(box, "scrollTop", {
      configurable: true,
      get: () => boxTop,
      set: (v: number) => {
        boxTop = Math.max(0, Math.min(740, v));
      },
    });
    box.style.lineHeight = "18px";
    const list = fakeLayout(rendered.container, [100, 341, 100]);
    return { ...rendered, box, list };
  }

  it("steps three of the box's own lines a press (54px for an 18px line), not a fixed 40", () => {
    const { container, box } = withBox();
    expect(onToolRow(container)).toBe(true);
    press("j");
    expect(box.scrollTop).toBe(54);
    press("j");
    expect(box.scrollTop).toBe(108);
    press("k");
    expect(box.scrollTop).toBe(54);
  });

  it("a count steps the box that many times", () => {
    const { box } = withBox();
    press("5");
    press("j");
    expect(box.scrollTop).toBe(5 * 54);
  });

  it("is entered at the near end: its start going down, its end going up", () => {
    const { container, box } = withBox();
    for (let i = 0; i < 20; i++) press("j"); // read the whole box, then move on
    expect(current(container)).toBe("after");
    expect(box.scrollTop).toBe(740);

    press("k"); // back onto the tool row from below: its box is entered at its end, not left where it was read to
    expect(onToolRow(container)).toBe(true);
    expect(box.scrollTop).toBe(740);
    press("k");
    expect(box.scrollTop).toBe(740 - 54);

    // and from above, going down: back to its start, not wherever it stood
    for (let i = 0; i < 20; i++) press("k");
    expect(current(container)).toBe("before");
    expect(box.scrollTop).toBe(0);
    press("j");
    expect(onToolRow(container)).toBe(true);
    expect(box.scrollTop).toBe(0);
  });

  it("does not carry a stale position: a box left halfway reads from its start when entered going down", () => {
    const { container, box } = withBox();
    press("j");
    press("j");
    expect(box.scrollTop).toBe(108);
    press("k");
    press("k");
    press("k"); // out of the box (it is at its start), onto "before"
    expect(current(container)).toBe("before");
    box.scrollTop = 400; // left somewhere in the middle
    press("j");
    expect(onToolRow(container)).toBe(true);
    expect(box.scrollTop).toBe(0);
  });
});

describe("the ease-out", () => {
  let now = 0;
  let nextId = 1;
  let queue = new Map<number, FrameRequestCallback>();

  function advance(ms: number) {
    const end = now + ms;
    while (now < end) {
      now = Math.min(end, now + 16);
      const due = [...queue.entries()];
      queue = new Map();
      for (const [, callback] of due) act(() => callback(now));
    }
  }

  beforeEach(() => {
    now = 5000;
    nextId = 1;
    queue = new Map();
    vi.stubGlobal("requestAnimationFrame", (cb: FrameRequestCallback) => {
      const id = nextId++;
      queue.set(id, cb);
      return id;
    });
    vi.stubGlobal("cancelAnimationFrame", (id: number) => {
      queue.delete(id);
    });
    vi.spyOn(performance, "now").mockImplementation(() => now);
    (window as unknown as { matchMedia: unknown }).matchMedia = () => ({ matches: false });
  });
  afterEach(() => {
    vi.unstubAllGlobals();
    vi.restoreAllMocks();
    delete (window as unknown as { matchMedia?: unknown }).matchMedia;
  });

  it("a single j eases the view to its target over about 150ms, and the cursor moves at once", () => {
    const { container } = started();
    prompts("a", "b", "c");
    const list = fakeLayout(container, [360, 990, 400]);
    press("g");
    press("g");
    queue.clear();
    press("j");
    expect(current(container)).toBe("b");
    // on its way at once (the first frame's share is written in the key's own task), not yet there
    expect(list.scrollTop).toBeGreaterThan(0);
    expect(list.scrollTop).toBeLessThan(227);
    advance(SCROLL_ANIMATION_MS + 40);
    expect(list.scrollTop).toBe(227);
  });

  it("a count scrolls at once", () => {
    const { container } = started();
    prompts("a", "b", "c");
    const list = fakeLayout(container, [100, 990, 100]);
    press("g");
    press("g");
    press("j");
    advance(SCROLL_ANIMATION_MS + 40);
    queue.clear();
    press("3");
    press("j");
    expect(list.scrollTop).toBe(3 * STEP);
    expect(queue.size).toBe(0);
  });

  it("a press during an animation starts from that animation's target", () => {
    const { container } = started();
    prompts("a", "b", "c");
    const list = fakeLayout(container, [360, 990, 400]);
    press("g");
    press("g");
    press("j"); // target 227, animating
    advance(48);
    expect(list.scrollTop).toBeLessThan(227);
    press("j"); // settles to 227, then steps three lines from there
    advance(SCROLL_ANIMATION_MS + 40);
    expect(list.scrollTop).toBe(227 + STEP);
  });

  it("a wheel during the ease is the reader's: the cursor is re-homed to a visible row at once, as for any other scroll", () => {
    const { container } = started();
    prompts("a", "b", "c");
    const list = fakeLayout(container, [360, 990, 400]);
    press("g");
    press("g");
    press("j"); // "b", the view easing toward 227
    expect(current(container)).toBe("b");
    list.scrollTop = 1500; // a wheel: the end of the list, "b" now wholly above the view
    fireEvent.scroll(list);
    expect(current(container)).toBe("c");
    advance(SCROLL_ANIMATION_MS + 40);
    expect(list.scrollTop).toBe(1350); // the ease let go; it never pulled the list back
    expect(current(container)).toBe("c");
  });

  it("the list's own scroll events, while the ease runs untouched, do not re-home the cursor", () => {
    const { container } = started();
    prompts("a", "b", "c");
    const list = fakeLayout(container, [360, 990, 400]);
    press("g");
    press("g");
    press("j");
    expect(current(container)).toBe("b");
    for (let i = 0; i < 4; i++) {
      advance(16);
      fireEvent.scroll(list); // what the engine dispatches for each of the ease's own writes
      expect(current(container)).toBe("b");
    }
    advance(SCROLL_ANIMATION_MS);
    expect(list.scrollTop).toBe(227);
  });

  it("a tab switch ends a running ease: the new tab is never carried on toward the old tab's target", () => {
    const { container } = started();
    prompts("a", "b", "c");
    const list = fakeLayout(container, [360, 990, 400]);
    press("g");
    press("g");
    press("j"); // easing toward 227
    const two = [LIVE_TAB, { ...LIVE_TAB, id: 2, number: 2, label: "2 new" }];
    dispatch({ kind: "tabs", active: 2, tabs: two });
    dispatch({ kind: "snapshot", tab: 2, throughRevision: 1, state: { ...initialState(), status: { kind: "running" } } as AgentUiState });
    const atSwitch = list.scrollTop;
    advance(SCROLL_ANIMATION_MS + 60);
    expect(list.scrollTop).toBe(atSwitch);
  });

  it("without an animation (reduced motion, or no way to ask) a held key's repeats step at once", () => {
    (window as unknown as { matchMedia: unknown }).matchMedia = () => ({ matches: true });
    const { container } = started();
    prompts("a", "b", "c");
    const list = fakeLayout(container, [100, 990, 100]);
    press("g");
    press("g");
    press("j");
    queue.clear();
    press("j", { repeat: true });
    expect(list.scrollTop).toBe(STEP);
    expect(queue.size).toBe(0);
  });

  it("without an animation (reduced motion, or no way to ask) the same press lands at once", () => {
    (window as unknown as { matchMedia: unknown }).matchMedia = () => ({ matches: true });
    const { container } = started();
    prompts("a", "b", "c");
    const list = fakeLayout(container, [360, 990, 400]);
    press("g");
    press("g");
    press("j");
    expect(list.scrollTop).toBe(227);
  });
});

describe("a held j or k glides", () => {
  /** The key-repeat interval these tests hold keys at: 31 a second, two whole frames. */
  const REPEAT_MS = 32;
  let now = 0;
  let nextId = 1;
  let queue = new Map<number, FrameRequestCallback>();
  let posted: Record<string, unknown>[] = [];

  /** Runs animation frames every 16ms for `ms`. */
  function advance(ms: number) {
    const end = now + ms;
    while (now < end) {
      now = Math.min(end, now + 16);
      const due = [...queue.entries()];
      queue = new Map();
      for (const [, callback] of due) act(() => callback(now));
    }
  }

  /** Holds `key` for `ms` after its first press: a repeat every `every` ms, frames in between. Returns
   *  the list's `scrollTop` and the cursor's row text after each frame. */
  function holdFor(container: HTMLElement, list: HTMLElement, key: string, ms: number, every = REPEAT_MS) {
    const seen: { scrollTop: number; cursor: string | null }[] = [];
    let held = 0;
    while (held < ms) {
      press(key, { repeat: true });
      for (let t = 0; t < every; t += 16) {
        advance(16);
        seen.push({ scrollTop: list.scrollTop, cursor: current(container) });
      }
      held += every;
    }
    return seen;
  }

  const letGo = (key: string) => fireEvent.keyUp(document.activeElement ?? document.body, { key });

  beforeEach(() => {
    now = 5000;
    nextId = 1;
    queue = new Map();
    posted = [];
    (window as unknown as { webkit: unknown }).webkit = {
      messageHandlers: { eitriAgent: { postMessage: (m: string) => posted.push(JSON.parse(m)) } },
    };
    vi.stubGlobal("requestAnimationFrame", (cb: FrameRequestCallback) => {
      const id = nextId++;
      queue.set(id, cb);
      return id;
    });
    vi.stubGlobal("cancelAnimationFrame", (id: number) => {
      queue.delete(id);
    });
    // One clock for the frames, `performance.now()` and the keydowns' own timestamps (jsdom takes those
    // from `Date.now()`), as a browser has.
    vi.spyOn(performance, "now").mockImplementation(() => now);
    vi.spyOn(Date, "now").mockImplementation(() => 1_000_000 + now);
    (window as unknown as { matchMedia: unknown }).matchMedia = () => ({ matches: false });
  });
  afterEach(() => {
    vi.useRealTimers();
    vi.unstubAllGlobals();
    vi.restoreAllMocks();
    delete (window as unknown as { matchMedia?: unknown }).matchMedia;
  });

  /** Short rows and long ones, one after another. */
  const HEIGHTS = [100, 990, 40, 40, 600, 30, 1500, 60, 800, 40, 1200, 50];
  const TEXTS = HEIGHTS.map((_, i) => `r${i}`);
  /** 3 lines per repeat interval (the measured one, from the second repeat on), 16ms a frame. */
  const PER_FRAME = ((3 * LINE) / REPEAT_MS) * 16;

  function heldConversation() {
    const rendered = started();
    prompts(...TEXTS);
    const list = fakeLayout(rendered.container, HEIGHTS);
    press("g");
    press("g");
    advance(200);
    return { ...rendered, list };
  }

  it("moves the view a little every frame at one steady speed, over short rows and long ones alike", () => {
    const { container, list } = heldConversation();
    press("j");
    advance(SCROLL_ANIMATION_MS + 40);
    const seen = holdFor(container, list, "j", 2500);
    const steps = seen.slice(1).map((f, i) => f.scrollTop - seen[i].scrollTop);
    // Never a jump: no frame moves more than a frame's worth at speed (plus the drift to the reading
    // line and rounding), and once the line is on its reading line every frame moves the same.
    for (const step of steps) {
      expect(step).toBeGreaterThanOrEqual(0);
      expect(step).toBeLessThanOrEqual(PER_FRAME * 1.6 + 1);
    }
    const settled = steps.slice(-40);
    for (const step of settled) expect(Math.abs(step - PER_FRAME)).toBeLessThanOrEqual(1.5);
    // It passed rows of 30px and of 1,500px on the way.
    expect(new Set(seen.map((f) => f.cursor)).size).toBeGreaterThan(5);
  });

  it("keeps the cursor on the row under the reading line, which is always on screen", () => {
    const { container, list } = heldConversation();
    press("j");
    advance(SCROLL_ANIMATION_MS + 40);
    const seen = holdFor(container, list, "j", 2500);
    let y = 0;
    const spans = HEIGHTS.map((h) => {
      const span = { top: y, bottom: y + h };
      y += h;
      return span;
    });
    for (const frame of seen.slice(-40)) {
      const row = spans[TEXTS.indexOf(frame.cursor!)];
      const reading = frame.scrollTop + 400 * READING_LINE;
      // On the reading line, give or take a frame's travel and the line's drift.
      expect(row.top).toBeLessThanOrEqual(reading + PER_FRAME + 2);
      expect(row.bottom).toBeGreaterThanOrEqual(reading - PER_FRAME - 2);
      expect(row.bottom).toBeGreaterThan(frame.scrollTop);
      expect(row.top).toBeLessThan(frame.scrollTop + 400);
    }
  });

  it("letting go stops it within a short distance, the cursor where it was, the row where a press would leave it", () => {
    const { container, list } = heldConversation();
    press("j");
    advance(SCROLL_ANIMATION_MS + 40);
    holdFor(container, list, "j", 1200);
    const cursorAtRelease = current(container);
    const atRelease = list.scrollTop;
    letGo("j");
    advance(400);
    expect(queue.size).toBe(0);
    expect(current(container)).toBe(cursorAtRelease);
    // A short coast at most, plus what the cursor's row needs to land.
    expect(list.scrollTop).toBeGreaterThanOrEqual(atRelease);
    let y = 0;
    const spans = HEIGHTS.map((h) => {
      const span = { top: y, bottom: y + h };
      y += h;
      return span;
    });
    const row = spans[TEXTS.indexOf(cursorAtRelease!)];
    const landing = decideLanding({
      direction: 1,
      view: { scrollTop: list.scrollTop, height: 400, maxScrollTop: y - 400 },
      row,
      line: LINE,
    });
    expect(landing.scrollTop).toBeLessThanOrEqual(list.scrollTop);
    // A press from there is an ordinary press again.
    const before = current(container);
    press("j");
    expect(current(container) !== before || list.scrollTop !== landing.scrollTop).toBe(true);
  });

  it("k is the mirror: up at the same speed, the cursor on the row under the line", () => {
    const { container, list } = heldConversation();
    press("G", { shiftKey: true });
    advance(SCROLL_ANIMATION_MS + 40);
    const bottom = list.scrollTop;
    press("k");
    advance(SCROLL_ANIMATION_MS + 40);
    const seen = holdFor(container, list, "k", 2500);
    const steps = seen.slice(1).map((f, i) => seen[i].scrollTop - f.scrollTop);
    for (const step of steps) expect(step).toBeGreaterThanOrEqual(0);
    for (const step of steps.slice(-40)) expect(Math.abs(step - PER_FRAME)).toBeLessThanOrEqual(1.5);
    expect(list.scrollTop).toBeLessThan(bottom - 1500);
    letGo("k");
  });

  it("stops at the last row with the view at the end, and the next repeat is an ordinary press there", () => {
    const { container, list } = heldConversation();
    press("j");
    advance(SCROLL_ANIMATION_MS + 40);
    holdFor(container, list, "j", 6000);
    expect(current(container)).toBe(TEXTS[TEXTS.length - 1]);
    const total = HEIGHTS.reduce((a, b) => a + b, 0);
    expect(list.scrollTop).toBe(total - 400);
    advance(300);
    expect(queue.size).toBe(0);
    // Held on: nothing more to glide to, and nothing moves.
    press("j", { repeat: true });
    advance(100);
    expect(queue.size).toBe(0);
    expect(current(container)).toBe(TEXTS[TEXTS.length - 1]);
    letGo("j");
  });

  it("a missing repeat ends it as letting go would", () => {
    const { container, list } = heldConversation();
    press("j");
    advance(SCROLL_ANIMATION_MS + 40);
    holdFor(container, list, "j", 600);
    advance(600); // no repeat, no keyup: the release was lost
    expect(queue.size).toBe(0);
    const at = list.scrollTop;
    advance(200);
    expect(list.scrollTop).toBe(at);
  });

  it("a frame the page was too busy to draw neither ends the hold nor leaps to make up the time", () => {
    const { container, list } = heldConversation();
    press("j");
    advance(SCROLL_ANIMATION_MS + 40);
    holdFor(container, list, "j", 1200);
    // 150ms with no frame and no repeat delivered: the page was busy; the repeats queued meanwhile come
    // after the late frame.
    now += 150;
    const before = list.scrollTop;
    const due = [...queue.values()];
    queue = new Map();
    for (const callback of due) act(() => callback(now));
    expect(queue.size).toBe(1);
    expect(list.scrollTop - before).toBeLessThanOrEqual(PER_FRAME * (34 / 16) + 2);
    // The queued repeats arrive, and the glide goes on at its speed rather than gathering again from rest.
    press("j", { repeat: true });
    const seen = holdFor(container, list, "j", 192);
    const steps = seen.slice(1).map((f, i) => f.scrollTop - seen[i].scrollTop);
    for (const step of steps) expect(Math.abs(step - PER_FRAME)).toBeLessThanOrEqual(3);
    letGo("j");
  });

  it("a key that ends a hold is judged from the release, even when the repeats stopped arriving well before it", () => {
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] });
    const { container } = started();
    prompts("a", "b");
    events(
      { type: "turn_started", turn_id: "t1" },
      { type: "tool_call_started", turn_id: "t1", tool_use_id: "toolu_1", name: "Bash", input: { command: "ls" } },
      { type: "permission_requested", permission_id: "perm-1", tool_use_id: "toolu_1", tool_name: "Bash", input: { command: "ls" } },
    );
    prompts("c", "d", "e");
    const rows = Array.from(container.querySelectorAll('.message-list [data-nav-stop="row"]'));
    const list = fakeLayout(container, rows.map((row) => (row.classList.contains("row-permission") ? 300 : 120)));
    press("g");
    press("g");
    advance(300);
    press("j");
    advance(SCROLL_ANIMATION_MS + 40);
    let guard = 0;
    while (!container.querySelector(".row-current.row-permission") && guard++ < 100) holdFor(container, list, "j", REPEAT_MS);
    expect(container.querySelector(".row-current.row-permission")).not.toBeNull();
    // The page stalls: no frame and no repeat for 300ms, and the next key is `a`. Measured from the last
    // repeat it stands alone; but the key was held until `a` came.
    now += 300;
    fireEvent.keyDown(document.activeElement ?? document.body, { key: "a" });
    act(() => vi.advanceTimersByTime(TYPING_GUARD_MS + 50));
    expect(posted.filter((m) => m.type === "permission_response")).toHaveLength(0);
  });

  it("an overlay that opens while a key is held stops the glide, and the repeats go to the overlay", () => {
    const { container, list } = heldConversation();
    press("j");
    advance(SCROLL_ANIMATION_MS + 40);
    holdFor(container, list, "j", 300);
    dispatch({ kind: "open_keymap" });
    const at = list.scrollTop;
    const cursorAt = current(container);
    holdFor(container, list, "j", 200);
    expect(list.scrollTop).toBe(at);
    expect(current(container)).toBe(cursorAt);
  });

  it("an arrival stops it where it is", () => {
    const { container, list } = heldConversation();
    press("j");
    advance(SCROLL_ANIMATION_MS + 40);
    holdFor(container, list, "j", 600);
    dispatch({ kind: "pane_focus", focused: false });
    const at = list.scrollTop;
    advance(300);
    expect(list.scrollTop).toBe(at);
    expect(queue.size).toBe(0);
  });

  it("the cursor's band glides from the row it left to the row it reached, on a single press too", () => {
    const { container } = heldConversation();
    const rows = Array.from(container.querySelectorAll<HTMLElement>('.message-list [data-nav-stop="row"]'));
    let y = 0;
    rows.forEach((row, i) => {
      const at = y;
      y += HEIGHTS[i];
      Object.defineProperty(row, "offsetTop", { value: at, configurable: true });
      Object.defineProperty(row, "offsetHeight", { value: HEIGHTS[i], configurable: true });
      Object.defineProperty(row, "offsetWidth", { value: 380, configurable: true });
    });
    const band = container.querySelector<HTMLElement>(".message-list > .cursor-band")!;
    expect(band.getAttribute("aria-hidden")).toBe("true");
    const bandTop = () => Number(/translate\([^,]+, ([-\d.]+)px\)/.exec(band.style.transform)![1]);
    press("j");
    advance(16);
    expect(bandTop()).toBeGreaterThan(0);
    expect(bandTop()).toBeLessThan(HEIGHTS[0]);
    advance(200);
    expect(bandTop()).toBe(HEIGHTS[0]);
    expect(band.style.height).toBe(`${HEIGHTS[1]}px`);
    // The row itself no longer carries the fill's class alone: the band is the fill.
    expect(container.querySelector(".row-current .row-body")!.textContent).toBe("r1");
  });

  it("a count keeps its immediate steps, even on a repeat", () => {
    const { container, list } = heldConversation();
    press("j");
    advance(SCROLL_ANIMATION_MS + 40);
    queue.clear();
    press("2");
    press("j", { repeat: true });
    expect(queue.size).toBe(0);
    expect(list.scrollTop).toBeGreaterThan(0);
    expect(current(container)).toBe("r1");
  });

  it("a/d wait the typing guard through a hold and after letting go, then answer the card the cursor is on", () => {
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] });
    const rendered = started();
    const { container } = rendered;
    events(
      { type: "user_prompt_submitted", text: "go" },
      { type: "turn_started", turn_id: "t1" },
      { type: "assistant_message", turn_id: "t1", text: "x".repeat(10) },
      { type: "tool_call_started", turn_id: "t1", tool_use_id: "toolu_1", name: "Bash", input: { command: "ls" } },
      { type: "permission_requested", permission_id: "perm-1", tool_use_id: "toolu_1", tool_name: "Bash", input: { command: "ls" } },
    );
    const rows = container.querySelectorAll('.message-list [data-nav-stop="row"]');
    const heights = Array.from(rows, (row) => (row.classList.contains("row-permission") ? 300 : 700));
    const list = fakeLayout(container, heights);
    press("g");
    press("g");
    advance(300);
    act(() => vi.advanceTimersByTime(300));
    press("j");
    advance(SCROLL_ANIMATION_MS + 40);
    // Hold until the card is the cursor's row.
    let guard = 0;
    while (!container.querySelector(".row-current.row-permission") && guard++ < 200) holdFor(container, list, "j", 33);
    expect(container.querySelector(".row-current.row-permission")).not.toBeNull();
    // An `a` while held answers nothing, and ends the hold.
    fireEvent.keyDown(document.activeElement ?? document.body, { key: "a" });
    act(() => vi.advanceTimersByTime(TYPING_GUARD_MS + 50));
    expect(posted.filter((m) => m.type === "permission_response")).toHaveLength(0);

    // Hold again; let go 30ms after the last repeat; an `a` 240ms after letting go -- 270ms after the last
    // repeat -- is still inside the window, which runs from the release.
    advance(400);
    act(() => vi.advanceTimersByTime(400));
    press("k");
    advance(SCROLL_ANIMATION_MS + 40);
    holdFor(container, list, "k", 96);
    advance(30);
    letGo("k");
    advance(240);
    fireEvent.keyDown(document.activeElement ?? document.body, { key: "a" });
    act(() => vi.advanceTimersByTime(TYPING_GUARD_MS + 50));
    expect(posted.filter((m) => m.type === "permission_response")).toHaveLength(0);

    // Back on the card by single presses; a lone `a` past the window answers the card the cursor shows.
    advance(400);
    act(() => vi.advanceTimersByTime(400));
    while (!container.querySelector(".row-current.row-permission") && guard++ < 400) {
      press("j");
      advance(SCROLL_ANIMATION_MS + 40);
    }
    advance(400);
    act(() => vi.advanceTimersByTime(400));
    fireEvent.keyDown(document.activeElement ?? document.body, { key: "a" });
    act(() => vi.advanceTimersByTime(TYPING_GUARD_MS + 50));
    const answers = posted.filter((m) => m.type === "permission_response");
    expect(answers).toHaveLength(1);
    expect(answers[0]).toMatchObject({ permission_id: "perm-1", decision: "allow" });
  });
});
