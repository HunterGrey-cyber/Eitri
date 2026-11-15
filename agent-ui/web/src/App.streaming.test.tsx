// @vitest-environment jsdom
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, render } from "@testing-library/react";
import App from "./App";
import { MessageList, timelineRowRenderCount } from "./components/MessageList";
import { isMarkdownCached, markdownParseCount, renderMarkdown } from "./markdown";
import { initialState } from "./reducer";

/* A reply on its way to the screen in the real page: what the row holds after every batch, and what stays in place. */

afterEach(cleanup);
beforeAll(() => {
  Element.prototype.scrollIntoView = vi.fn();
});

const TAB = { id: 1, number: 1, label: "1 new", name: null, state: "live", mode: "auto", marker: null, pending: 0, resumable: false, failure: null };

function mount() {
  (window as unknown as { webkit: unknown }).webkit = { messageHandlers: { eitriAgent: { postMessage: () => {} } } };
  const { container } = render(<App />);
  const dispatch = (payload: unknown) => act(() => window.__eitriDispatch!(JSON.stringify(payload)));
  dispatch({ kind: "tabs", active: 1, tabs: [TAB] });
  const earlier = Array.from({ length: 30 }, (_, i) => ({ seq: i + 1, text: `earlier reply **${i}**` }));
  dispatch({
    kind: "snapshot", tab: 1, throughRevision: 40,
    state: { ...initialState(), status: { kind: "running" }, activeTurnId: "t1", transcript: earlier, nextSeq: 40 },
  });
  let revision = 40;
  const deliver = (events: unknown[]) => {
    dispatch({ kind: "events", tab: 1, fromRevision: revision, throughRevision: revision + events.length, events });
    revision += events.length;
  };
  const delta = (text: string) => deliver([{ type: "content_delta", kind: "text", turn_id: "t1", text }]);
  const body = () => {
    const rows = container.querySelectorAll<HTMLElement>(".row-assistant .row-body > div");
    return rows[rows.length - 1];
  };
  return { deliver, delta, body };
}

const PIECES = [
  "First paragraph with `code`.\n\n",
  "```rust\nfn main() {\n",
  "    println!(\"hi\");\n}\n```\n\n",
  "- one\n",
  "- two\n\n",
  "1. a\n\n2",
  ". b\n\n",
  "| a | b |\n|---|---|\n",
  "| 1 | 2 |\n\n",
  "Last paragraph, still being wri",
  "tten.\n",
];

describe("a reply whose open block is slow to draw", () => {
  afterEach(() => {
    vi.useRealTimers();
    vi.restoreAllMocks();
  });

  it("rests after a slow drawing, and draws the newest text when the rest is over", () => {
    vi.useFakeTimers();
    let clock = 1_000;
    vi.spyOn(performance, "now").mockImplementation(() => clock);
    const { deliver, delta, body } = mount();
    const realInsert = Element.prototype.insertAdjacentHTML;
    // Every drawing of the open end takes 20 ms on this clock.
    vi.spyOn(Element.prototype, "insertAdjacentHTML").mockImplementation(function (this: Element, where, html) {
      clock += 20;
      realInsert.call(this, where as InsertPosition, html);
    });
    delta("one");
    expect(body().textContent?.trim()).toBe("one");
    delta(" two");
    expect(body().textContent?.trim(), "inside the rest: not drawn yet").toBe("one");
    delta(" three");
    expect(body().textContent?.trim()).toBe("one");
    act(() => {
      clock += 60;
      vi.advanceTimersByTime(60);
    });
    expect(body().textContent?.trim(), "the rest is over: the newest text").toBe("one two three");
    delta(" four");
    act(() => {
      clock += 60;
      vi.advanceTimersByTime(60);
    });
    delta(" five");
    expect(body().textContent?.trim(), "inside a rest again").toBe("one two three four");
    deliver([{ type: "assistant_message_boundary" }]);
    expect(body().innerHTML, "the end draws what the rest held back").toBe(renderMarkdown("one two three four five"));
  });
});

describe("VISUAL over a reply being written", () => {
  afterEach(() => {
    vi.useRealTimers();
    vi.restoreAllMocks();
  });

  it("a drawing waiting out its rest does not touch the DOM under a selection, and is drawn when the mode ends", () => {
    vi.useFakeTimers();
    let clock = 1_000;
    vi.spyOn(performance, "now").mockImplementation(() => clock);
    const realInsert = Element.prototype.insertAdjacentHTML;
    vi.spyOn(Element.prototype, "insertAdjacentHTML").mockImplementation(function (this: Element, where, html) {
      clock += 20;
      realInsert.call(this, where as InsertPosition, html);
    });
    const list = (text: string, visual: "visual" | null) => (
      <MessageList
        visual={visual}
        state={{ ...initialState(), status: { kind: "running" }, activeTurnId: "t1", assistantMessageOpen: true, transcript: [{ seq: 1, text }] }}
        sessionEnded={false}
        expanded={{}}
        cursor={0}
        onAnswerPermission={() => {}}
      />
    );
    const { container, rerender } = render(list("one", null));
    const body = () => container.querySelector<HTMLElement>(".row-assistant .row-body > div")!;
    // A drawing took 20 ms, so the next text waits; then the selection starts, on the text as it arrived.
    rerender(list("one two", null));
    expect(body().textContent?.trim()).toBe("one");
    const drawn = body().firstElementChild;
    rerender(list("one two", "visual"));
    act(() => {
      clock += 120;
      vi.advanceTimersByTime(120);
    });
    expect(body().firstElementChild, "the selection's node stays").toBe(drawn);
    expect(body().textContent?.trim()).toBe("one");
    rerender(list("one two", null));
    expect(body().textContent?.trim(), "the mode is over: the newest text").toBe("one two");
  });
});

describe("a streaming reply", () => {
  // A drawing that takes longer than the rest threshold makes the next text wait; these tests check
  // what is drawn, not when, so the clock stands still and every drawing takes no time. Under a
  // loaded machine the real clock made the code-block deltas wait and the comparison see half of them.
  beforeEach(() => {
    vi.spyOn(performance, "now").mockReturnValue(1_000);
  });
  afterEach(() => {
    vi.restoreAllMocks();
  });
  it("holds what one rendering of its text holds after every batch and after it ends", () => {
    const { deliver, delta, body } = mount();
    let text = "";
    for (const piece of PIECES) {
      text += piece;
      delta(piece);
      expect(body().innerHTML).toBe(renderMarkdown(text));
    }
    deliver([{ type: "assistant_message_boundary" }]);
    expect(body().innerHTML).toBe(renderMarkdown(text));
  });

  it("keeps its DOM when it ends: the end checks it against one rendering and changes nothing", () => {
    const { deliver, delta, body } = mount();
    delta("Block one.\n\nBlock two.\n\nBlock three, the open one.");
    const box = body();
    const first = box.firstElementChild;
    const last = box.lastElementChild;
    deliver([{ type: "assistant_message_boundary" }]);
    expect(body()).toBe(box);
    expect(box.firstElementChild).toBe(first);
    expect(box.lastElementChild).toBe(last);
    expect(box.innerHTML).toBe(renderMarkdown("Block one.\n\nBlock two.\n\nBlock three, the open one."));
    expect(isMarkdownCached("Block one.\n\nBlock two.\n\nBlock three, the open one."), "a switch back parses nothing").toBe(true);
  });

  it("leaves complete blocks in place: a batch touches neither them nor any other row", () => {
    const { delta, body } = mount();
    delta("Block one.\n\nBlock two.\n\n");
    delta("Block three.\n\nBlock ");
    const box = body();
    const first = box.firstElementChild;
    expect(first?.textContent).toBe("Block one.");
    const rows = timelineRowRenderCount();
    const parses = markdownParseCount();
    for (let i = 0; i < 10; i++) delta(`word${i} `);
    expect(body()).toBe(box);
    expect(box.firstElementChild, "the DOM of a complete block is not rebuilt").toBe(first);
    expect(timelineRowRenderCount() - rows, "one row per batch").toBe(10);
    expect(markdownParseCount() - parses, "the open end only").toBeLessThanOrEqual(10);
  });

  it("draws what an unclosed fence or a cut table holds, and a reply that goes on from a snapshot", () => {
    const { deliver, delta, body } = mount();
    delta("Intro.\n\n```ts\nconst a = 1;\n");
    expect(body().querySelector("pre.code-block")).not.toBeNull();
    delta("const b = 2;\n");
    expect(body().innerHTML).toBe(renderMarkdown("Intro.\n\n```ts\nconst a = 1;\nconst b = 2;\n"));
    deliver([{ type: "assistant_message_boundary" }]);
    delta("A new reply");
    expect(body().innerHTML).toBe(renderMarkdown("A new reply"));
  });
});

describe("streamed reply diagrams", () => {
  afterEach(() => { vi.useRealTimers(); vi.restoreAllMocks(); });
  it("attaches after the slow-tail timer, keeps cached images through deltas and stream end", () => {
    vi.useFakeTimers();
    let clock = 1000;
    vi.spyOn(performance, "now").mockImplementation(() => clock);
    const sent: { type: string; id?: string }[] = [];
    const { deliver, delta, body } = mount();
    window.webkit = { messageHandlers: { eitriAgent: { postMessage: (m) => sent.push(JSON.parse(m)) } } };
    act(() => window.__eitriDispatch!(JSON.stringify({ kind: "display", thinking: "collapsed", diagrams: true })));
    const real = Element.prototype.insertAdjacentHTML;
    vi.spyOn(Element.prototype, "insertAdjacentHTML").mockImplementation(function (this: Element, where, html) {
      clock += 20; real.call(this, where as InsertPosition, html);
    });
    const renders = () => sent.filter((m) => m.type === "render_diagram");
    delta("```mermaid\nflowchart LR; A-->B\n");
    expect(renders()).toHaveLength(0);
    delta("```");
    act(() => { clock += 200; vi.advanceTimersByTime(200); });
    expect(renders()).toHaveLength(0);
    delta("\n");
    act(() => { clock += 200; vi.advanceTimersByTime(200); });
    expect(renders()).toHaveLength(1);
    act(() => window.__eitriDispatch!(JSON.stringify({ kind: "diagram", id: renders()[0].id,
      svg: '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 200 100"><text>A</text></svg>' })));
    const url = body().querySelector("img")!.src;
    for (let i = 0; i < 10; i++) {
      delta("\nmore text " + i);
      act(() => { clock += 200; vi.advanceTimersByTime(200); });
      expect(body().querySelector("img"), body().innerHTML + " iteration " + i).not.toBeNull();
      expect(body().querySelector("img")!.src).toBe(url);
      expect(body().querySelector("pre")!.classList.contains("diagram-drawn")).toBe(true);
    }
    deliver([{ type: "assistant_message_boundary" }]);
    expect(body().querySelector("img")!.src).toBe(url);
    expect(renders()).toHaveLength(1);
  });
});
