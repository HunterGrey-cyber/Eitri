// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render } from "@testing-library/react";
import { ReviewOverlay } from "./ReviewOverlay";
import { applyReviewKey, failRequest, openReview, receiveDiff, receiveReview } from "../review";
import type { ReviewAction, ReviewState } from "../review";
import type { ReviewDiffEnvelope, ReviewEnvelope, ReviewFile, ReviewTurn } from "../types";
/** What Rust really sends: `core/src/turn_review/lifecycle.rs` writes these envelopes from its own `plan`
 *  and serializer, and fails when this file is not what it would write. */
import notesFixture from "../fixtures/review-notes.json";

afterEach(cleanup);

const TURN: ReviewTurn = {
  n: 7,
  turnId: "t7",
  startedAt: Date.UTC(2026, 0, 1, 11, 0, 0),
  endedAt: Date.UTC(2026, 0, 1, 11, 2, 0),
  state: "ok",
  late: false,
  overlappedNext: false,
  overlappedTab: false,
  reason: null,
};
const file = (path: string, over: Partial<ReviewFile> = {}): ReviewFile => ({ path, added: 1, removed: 0, origin: "agent", binary: false, tooLarge: false, nested: false, ...over });

function envelope(over: Partial<ReviewEnvelope> = {}): ReviewEnvelope {
  return {
    requestId: "req-1",
    tab: 1,
    scope: "turn",
    current: 7,
    turns: [TURN],
    files: [file("a.rs", { added: 41, removed: 6 }), file("b.md", { origin: "agent_only", added: 0 }), file("Cargo.lock", { origin: "workspace", added: 7, removed: 7 })],
    compared: true,
    pendingNoResult: 0,
    notes: ["changed on disk during this turn"],
    ...over,
  };
}
const loaded = (over: Partial<ReviewEnvelope> = {}): ReviewState => receiveReview(openReview(1, "req-1"), envelope(over));
let n = 0;
const press = (state: ReviewState, action: ReviewAction) => applyReviewKey(state, action, () => `id-${++n}`).state;
const draw = (state: ReviewState) => render(<ReviewOverlay state={state} onClose={() => {}} />);

const HUNKS: ReviewDiffEnvelope["hunks"] = [
  {
    id: 0,
    header: "@@ -10,3 +10,4 @@",
    lines: [
      { kind: "context", text: "keep", oldNo: 10, newNo: 10 },
      { kind: "removed", text: "old", oldNo: 11, newNo: null },
      { kind: "added", text: "new", oldNo: null, newNo: 11 },
      { kind: "no_newline", text: "\\ No newline at end of file", oldNo: null, newNo: null },
    ],
  },
  { id: 1, header: "@@ -40,2 +41,2 @@", lines: [{ kind: "added", text: "later", oldNo: null, newNo: 41 }] },
];

/** `a.rs` opened and its patch arrived. */
function withPatch(hunks: ReviewDiffEnvelope["hunks"] = HUNKS): ReviewState {
  let state = press(loaded(), { kind: "toggle" });
  const entry = state.diffs["a.rs"];
  if (entry?.status !== "loading") throw new Error("no request");
  state = receiveDiff(state, { requestId: entry.requestId, tab: 1, turn: 7, path: "a.rs", added: 41, removed: 6, hunks });
  return state;
}

describe("ReviewOverlay", () => {
  it("shows the header: the turn, its times, and what happened to the disk", () => {
    const { container } = draw(loaded());
    const title = container.querySelector(".review-title")!.textContent!;
    expect(title).toMatch(/^review · turn 7 of 7 · \d\d:\d\d:\d\d → \d\d:\d\d:\d\d · changed on disk during this turn$/);
    // Rust's own note says the same thing; it is not repeated under the header.
    expect(container.querySelectorAll(".review-flag")).toHaveLength(0);
  });

  it("draws the turn flags only through Rust's notes, never a second line of its own", () => {
    const { container } = draw(loaded({ turns: [{ ...TURN, late: true, overlappedNext: true, overlappedTab: true }] }));
    expect(container.querySelectorAll(".review-flag")).toHaveLength(0);
  });

  it("shows no file list, and never says nothing changed, when nothing was compared", () => {
    const { container } = draw(loaded({ files: [], compared: false, notes: ["changed on disk during this turn", "the baseline snapshot is still being taken"] }));
    expect(container.querySelector(".review-header")!.textContent).toContain("the baseline snapshot is still being taken");
    expect(container.textContent).not.toContain("no files changed on disk");
    expect(container.querySelector(".review-file")).toBeNull();
    expect(container.querySelector('[data-nav-stop="group"]')).toBeNull();
  });

  it("says no files changed when two snapshots were compared and none differ", () => {
    expect(draw(loaded({ files: [] })).container.textContent).toContain("no files changed on disk");
  });

  it("carries a note Rust adds beyond the header's own words", () => {
    const { container } = draw(loaded({ notes: ["changed on disk during this turn", "2 files are in .gitignore and were not copied"] }));
    const lines = Array.from(container.querySelectorAll(".review-flag")).map((el) => el.textContent);
    expect(lines).toEqual(["2 files are in .gitignore and were not copied"]);
  });

  it("says it is loading, then why a request was refused", () => {
    const fresh = openReview(1, "req-1");
    expect(draw(fresh).container.textContent).toContain("loading…");
    cleanup();
    const refused = failRequest(fresh, "req-1", "no session");
    const { container } = draw(refused);
    expect(container.querySelector(".review-header")!.textContent).toContain("no session");
    expect(container.textContent).not.toContain("loading…");
  });

  it("draws a file row with its sign, path and counts, on the document's own row", () => {
    const { container } = draw(loaded());
    const rows = Array.from(container.querySelectorAll<HTMLElement>('[data-nav-stop="file"]'));
    expect(rows.map((r) => r.getAttribute("data-sign"))).toEqual(["✓", "·"]);
    expect(rows[0].textContent).toContain("a.rs");
    expect(rows[0].textContent).toContain("+41 −6");
    expect(rows[0].classList.contains("row")).toBe(true);
    expect(rows[0].classList.contains("row-current")).toBe(true);
  });

  it("names the reason a file cannot be opened", () => {
    const { container } = draw(loaded({ files: [file("x.bin", { binary: true }), file("big", { tooLarge: true }), file("vendor", { nested: true })] }));
    const text = container.textContent!;
    expect(text).toContain("binary");
    expect(text).toContain("too large to snapshot");
    expect(text).toContain("nested repository, not reviewed");
  });

  it("folds the files nothing named under one group, opened with Enter", () => {
    const state = loaded();
    const closed = draw(state).container;
    expect(closed.textContent).toContain("changed outside this tab's edits (1)");
    expect(closed.textContent).not.toContain("Cargo.lock");
    const group = closed.querySelector('[data-nav-stop="group"]')!;
    expect(group.getAttribute("data-sign")).toBe("▸");
    cleanup();
    const open = draw({ ...state, groupOpen: true }).container;
    expect(open.textContent).toContain("Cargo.lock");
    expect(open.querySelector('[data-nav-stop="group"]')!.getAttribute("data-sign")).toBe("▾");
    const unnamed = Array.from(open.querySelectorAll('[data-nav-stop="file"]')).map((r) => r.getAttribute("data-sign"));
    expect(unnamed).toEqual(["✓", "·", "?"]);
  });

  it("says how many changing calls have no result, only when some do", () => {
    expect(draw(loaded()).container.textContent).not.toContain("no result");
    cleanup();
    expect(draw(loaded({ pendingNoResult: 1 })).container.textContent).toContain("1 file-changing call has no result");
    cleanup();
    expect(draw(loaded({ pendingNoResult: 3 })).container.textContent).toContain("3 file-changing calls have no result");
  });

  it("says nothing changed when nothing did", () => {
    expect(draw(loaded({ files: [] })).container.textContent).toContain("no files changed on disk");
  });

  it("draws the hunks with the diff classes and both line numbers", () => {
    const { container } = draw(withPatch());
    const lines = Array.from(container.querySelectorAll(".review-hunk-lines")[0].querySelectorAll(".diff-line"));
    expect(lines.map((l) => l.className)).toEqual([
      "diff-line diff-context",
      "diff-line diff-removed",
      "diff-line diff-added",
      "diff-line diff-context",
    ]);
    expect(Array.from(lines[1].querySelectorAll(".review-lineno")).map((e) => e.textContent)).toEqual(["11", ""]);
    expect(Array.from(lines[2].querySelectorAll(".review-lineno")).map((e) => e.textContent)).toEqual(["", "11"]);
    expect(lines[2].querySelector(".diff-gutter")!.textContent).toBe("+");
    expect(lines[1].querySelector(".diff-gutter")!.textContent).toBe("-");
    expect(lines[3].textContent).toContain("No newline at end of file");
  });

  it("stops are named file, hunk and group, never row", () => {
    const { container } = draw(withPatch());
    const names = new Set(Array.from(container.querySelectorAll("[data-nav-stop]")).map((e) => e.getAttribute("data-nav-stop")));
    expect(names).toEqual(new Set(["file", "hunk", "group"]));
    expect(container.querySelector('[data-nav-stop="row"]')).toBeNull();
    expect(container.querySelectorAll('[data-nav-stop="hunk"]')).toHaveLength(2);
  });

  it("refuses a patch over the cap, with its counts, and draws no hunk to land on", () => {
    const { container } = draw(withPatch(null));
    expect(container.textContent).toContain("too large (+41 −6 lines); o opens it in the editor");
    expect(container.querySelector(".review-hunk")).toBeNull();
    expect(container.querySelector(".diff-line")).toBeNull();
  });

  it("says a patch is loading, and that one could not be loaded", () => {
    const loading = press(loaded(), { kind: "toggle" });
    expect(draw(loading).container.querySelector(".review-patch")!.textContent).toBe("loading…");
    cleanup();
    const entry = loading.diffs["a.rs"];
    if (entry?.status !== "loading") throw new Error("no request");
    const failed = failRequest(loading, entry.requestId, "git unavailable");
    expect(draw(failed).container.querySelector(".review-patch")!.textContent).toContain("could not load this file's changes: git unavailable");
  });

  it("puts the cursor on the hunk it is on", () => {
    const state = press(withPatch(), { kind: "move", delta: 1 });
    const { container } = draw(state);
    const current = container.querySelector('[aria-current="true"]')!;
    expect(current.getAttribute("data-nav-stop")).toBe("hunk");
    expect(current.textContent).toContain("@@ -10,3 +10,4 @@");
    expect(container.querySelectorAll('[aria-current="true"]')).toHaveLength(1);
  });

  it("adds no colour of its own: no inline style anywhere", () => {
    const { container } = draw(withPatch());
    expect(container.querySelectorAll("[style]")).toHaveLength(0);
  });

  it("closes on a click on its own backdrop and not on a click inside", () => {
    const onClose = vi.fn();
    const { container } = render(<ReviewOverlay state={loaded()} onClose={onClose} />);
    fireEvent.click(container.querySelector(".review-path")!);
    expect(onClose).not.toHaveBeenCalled();
    fireEvent.click(container.querySelector(".review-overlay")!);
    expect(onClose).toHaveBeenCalledTimes(1);
  });

  it("never says who changed a file", () => {
    const state = withPatch();
    const text = draw({ ...state, groupOpen: true }).container.textContent!;
    expect(text).not.toMatch(/agent changed|the agent|claude changed/i);
  });
});

describe("ReviewOverlay over the envelopes Rust sends", () => {
  const envelopes = notesFixture.envelopes as unknown as Record<string, ReviewEnvelope>;
  const shown = (name: string) => {
    const envelope = envelopes[name];
    const { container } = draw(receiveReview(openReview(3, envelope.requestId), envelope));
    const lines = Array.from(container.querySelectorAll(".review-flag")).map((el) => el.textContent ?? "");
    const text = container.textContent ?? "";
    cleanup();
    return { envelope, lines, text };
  };

  it.each(Object.keys(envelopes))("%s: draws each of Rust's notes once and nothing else", (name) => {
    const { envelope, lines, text } = shown(name);
    expect(new Set(lines).size).toBe(lines.length);
    const title = envelope.scope === "turn" ? "changed on disk during this turn" : "changed on disk during this session";
    expect(lines).toEqual(envelope.notes.filter((note) => note !== title));
    if (!envelope.compared) expect(text).not.toContain("no files changed on disk");
  });

  it("dates a late baseline once, in Rust's words", () => {
    const { lines } = shown("flagged");
    expect(lines).toEqual([
      "baseline late: changes made before HH:MM:SS may be missing",
      "may include the next turn's first changes",
      "this turn overlapped another tab's turn",
    ]);
  });

  it("does not say a turn whose end snapshot failed in this run did not finish", () => {
    const { lines } = shown("endFailed");
    expect(lines.join("\n")).not.toContain("did not finish");
    expect(lines.some((line) => line.startsWith("no end snapshot (too many files"))).toBe(true);
  });

  it("shows no file list for a review that compared nothing", () => {
    for (const name of ["noBaseline", "basePending", "sessionNoBaseline"]) {
      expect(envelopes[name].compared, name).toBe(false);
      const { text } = shown(name);
      expect(text, name).not.toContain("no files changed on disk");
    }
    expect(shown("ok").text).toContain("no files changed on disk");
  });
});
