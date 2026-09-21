// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render } from "@testing-library/react";
import { HistoryNotice } from "./HistoryNotice";
import { rowIndexOf, stopsIn } from "../nav";
import type { HistoryNotice as HistoryNoticeData } from "../types";

// See ModeSelector.test.tsx: `globals` is off, so RTL's automatic cleanup is not registered.
afterEach(cleanup);

function notice(overrides: Partial<HistoryNoticeData> = {}): HistoryNoticeData {
  return {
    source: "claude_transcript",
    restoredItems: 42,
    omittedItems: 0,
    uptoSeq: 43,
    sourcePath: "/home/user/.claude/projects/-home-user-p/7432932d.jsonl",
    attemptedTranscriptPath: null,
    fallbackReason: null,
    writerVersion: "2.1.272",
    ...overrides,
  };
}

/** The notice inside a panel-shaped root, so `nav`'s own queries see what they would see for real:
 *  a row above it is what makes "is it a row?" a question with a wrong answer available. */
function renderInPanel(data: HistoryNoticeData = notice()) {
  const { container } = render(
    <div id="root">
      <HistoryNotice notice={data} />
      <div data-nav-stop="row">a conversation row</div>
    </div>,
  );
  const root = container.querySelector<HTMLElement>("#root")!;
  const el = root.querySelector<HTMLElement>(".history-notice")!;
  return { root, el, button: el.querySelector("button")! };
}

describe("HistoryNotice", () => {
  it("says which source the history came from", () => {
    const { el } = renderInPanel();
    expect(el.textContent).toContain("read from Claude's own transcript");
  });

  /* §5.5: the row appears whenever history was restored, not only when it was truncated. A user
     has to be able to tell "this is what we said last time" from "this is what we said just now",
     and after the 400-item cap only 8 of this machine's 44 sessions truncate at all -- so a
     truncation-only row would be absent almost every time it is needed. */
  it("is drawn for a complete history too, not only a truncated one", () => {
    const { el } = renderInPanel(notice({ omittedItems: 0 }));
    expect(el).not.toBeNull();
    expect(el.textContent).not.toContain("not loaded");
  });

  /* Invariant 15, all three clauses. */
  it("is reachable by j/k and is not a conversation row", () => {
    const { root, el } = renderInPanel();
    expect(stopsIn(root)).toContain(el);
    expect(rowIndexOf(root, el)).toBeNull();
    expect(Array.from(root.querySelectorAll('[data-nav-stop="row"]'))).not.toContain(el);
  });

  /* **The negative control**, and the reason the Copy path button is not decoration. `stopsIn`
     keeps a stop only when it is a row OR holds an enabled control (`nav.ts`), so a notice built by
     copying `.command-notice`'s attributes and leaving out its button would be filtered out and
     `j`/`k` could never land on it. The first draft of this row did exactly that. */
  it("would be unreachable without its button, which is why it has one", () => {
    const { root, el, button } = renderInPanel();
    expect(stopsIn(root)).toContain(el);
    button.remove();
    expect(stopsIn(root)).not.toContain(el);
  });

  it("copies the source path, and carries it as the button's tooltip", () => {
    const writeText = vi.fn();
    Object.defineProperty(navigator, "clipboard", { value: { writeText }, configurable: true });
    const { el, button } = renderInPanel();
    fireEvent.click(button);
    expect(writeText).toHaveBeenCalledWith("/home/user/.claude/projects/-home-user-p/7432932d.jsonl");
    expect(button.getAttribute("title")).toBe("/home/user/.claude/projects/-home-user-p/7432932d.jsonl");
    // The path itself is NOT in the line of prose: it is absolute, and it would wrap a narrow panel
    // several times (§5.5's own reason for moving it onto the button).
    expect(el.querySelector("span")!.textContent).not.toContain(".jsonl");
  });

  /* It must not be a `row`: row indices are counted across the whole panel (`App.tsx`), so one
     extra row would shift the cursor index of every conversation row under it and take
     `permissionTarget` and the scroll helpers with it. */
  it("does not change how many rows the panel has", () => {
    const { root } = renderInPanel();
    expect(root.querySelectorAll('[data-nav-stop="row"]').length).toBe(1);
  });
});
