// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render } from "@testing-library/react";
import { ReviewOverlay } from "./ReviewOverlay";
import { REVIEW_KEY_LINE, applyReviewKey, failRequest, openReview, receiveDiff, receivePreview, receiveRecovery, receiveReview, withStatus } from "../review";
import type { ReviewAction, ReviewState } from "../review";
import type { ReviewDiffEnvelope, ReviewDraft, ReviewEnvelope, ReviewFile, ReviewNotOnDisk, ReviewSendPreviewEnvelope, ReviewTurn } from "../types";
/** What Rust really sends: `core/src/turn_review/lifecycle.rs` writes these envelopes from its own `plan`
 *  and serializer, and fails when this file is not what it would write. */
import notesFixture from "../fixtures/review-notes.json";
/** A send preview leaving out a revert of every kind, as `core/src/agent_bridge.rs` serializes it. */
import previewFixture from "../fixtures/review-send-preview.json";

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
  const entry = state.diffs.get("a.rs");
  if (entry?.status !== "loading") throw new Error("no request");
  state = receiveDiff(state, { requestId: entry.requestId, tab: 1, turn: 7, path: "a.rs", added: 41, removed: 6, hunks });
  return state;
}

describe("ReviewOverlay with files named like Object.prototype members", () => {
  it("opens each one, shows its patch once it arrives, and breaks nothing", () => {
    for (const name of ["constructor", "toString", "__proto__", "hasOwnProperty"]) {
      let state = loaded({ files: [file(name), file("b.md")] });
      state = press(state, { kind: "toggle" });
      expect(draw(state).container.querySelector(".review-note")!.textContent, name).toBe("loading…");
      cleanup();
      const entry = state.diffs.get(name);
      if (entry?.status !== "loading") throw new Error(`${name}: no request`);
      state = receiveDiff(state, { requestId: entry.requestId, tab: 1, turn: 7, path: name, added: 41, removed: 6, hunks: HUNKS });
      const { container } = draw(state);
      expect(container.querySelectorAll(".review-hunk, [data-nav-stop='hunk']").length, name).toBeGreaterThan(0);
      cleanup();
    }
  });
});

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
    const entry = loading.diffs.get("a.rs");
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

const COMMENT = { id: 1, turn: 7, path: "a.rs", from: 11, to: 11, anchor: ["new"], text: "why is this here?" };
const REVERT = { id: 1, turn: 7, path: "a.rs", hunk: 1, header: "@@ -40,2 +41,2 @@", what: "hunk" as const, lines: [41, 41] as [number, number], source: "panel", undone: false };
const draft = (over: Partial<ReviewDraft> = {}): ReviewDraft => ({ comments: [], reverts: [], canUndo: false, ...over });

/** `a.rs` open, its patch arrived, and `d` as the draft the overview carried. */
function withDraft(d: ReviewDraft): ReviewState {
  let state = receiveReview(openReview(1, "req-1"), envelope({ draft: d }));
  state = press(state, { kind: "toggle" });
  const entry = state.diffs.get("a.rs");
  if (entry?.status !== "loading") throw new Error("no request");
  return receiveDiff(state, { requestId: entry.requestId, tab: 1, turn: 7, path: "a.rs", added: 41, removed: 6, hunks: HUNKS });
}

describe("ReviewOverlay: the draft", () => {
  it("draws a comment under the hunk that holds its line, as a stop of its own", () => {
    const { container } = draw(withDraft(draft({ comments: [COMMENT] })));
    const hunks = Array.from(container.querySelectorAll(".review-hunk"));
    expect(hunks[0].querySelector('[data-nav-stop="comment"]')!.textContent).toContain("11");
    expect(hunks[0].querySelector('[data-nav-stop="comment"]')!.textContent).toContain("why is this here?");
    expect(hunks[1].querySelector('[data-nav-stop="comment"]')).toBeNull();
    const names = new Set(Array.from(container.querySelectorAll("[data-nav-stop]")).map((e) => e.getAttribute("data-nav-stop")));
    expect(names).toEqual(new Set(["file", "hunk", "comment", "group"]));
    expect(container.querySelector('[data-nav-stop="row"]')).toBeNull();
  });

  it("puts the cursor on a comment when it is there", () => {
    const state = { ...withDraft(draft({ comments: [COMMENT] })), cursor: "comment:a.rs:1" };
    const current = draw(state).container.querySelector('[aria-current="true"]')!;
    expect(current.getAttribute("data-nav-stop")).toBe("comment");
  });

  it("marks a reverted hunk, and says when the revert was undone", () => {
    const marks = (d: ReviewDraft) => {
      const rows = Array.from(draw(withDraft(d)).container.querySelectorAll('[data-nav-stop="hunk"]'));
      cleanup();
      return rows.map((r) => r.querySelector(".review-file-flag")?.textContent ?? null);
    };
    expect(marks(draft())).toEqual([null, null]);
    expect(marks(draft({ reverts: [REVERT] }))).toEqual([null, "reverted"]);
    expect(marks(draft({ reverts: [{ ...REVERT, undone: true }] }))).toEqual([null, "reverted, undone"]);
  });

  it("marks a file whose whole revert is recorded", () => {
    const state = receiveReview(openReview(1, "req-1"), envelope({ draft: draft({ reverts: [{ ...REVERT, hunk: null, header: null, what: "file", lines: null }] }) }));
    const rows = Array.from(draw(state).container.querySelectorAll('[data-nav-stop="file"]'));
    expect(rows[0].textContent).toContain("reverted");
    expect(rows[1].textContent).not.toContain("reverted");
  });

  it("says what the draft holds on a line of its own, only when it holds something", () => {
    expect(draw(withDraft(draft())).container.querySelector(".review-draft")).toBeNull();
    cleanup();
    const line = draw(withDraft(draft({ comments: [COMMENT], reverts: [REVERT] }))).container.querySelector(".review-draft")!;
    expect(line.textContent).toBe("draft: 1 comment, 1 revert · s sends them to the agent");
  });

  it("is drawn again from the review envelope after a reload, with nothing carried by the page", () => {
    const d = draft({ comments: [COMMENT], reverts: [REVERT], canUndo: true });
    const first = draw(receiveReview(openReview(1, "req-1"), envelope({ draft: d }))).container.querySelector(".review-draft")!.textContent;
    cleanup();
    // A reload starts the overlay over from `openReview`; nothing but the envelope Rust sends again fills it.
    const fresh = openReview(1, "req-9");
    expect(draw(fresh).container.querySelector(".review-draft")).toBeNull();
    cleanup();
    const again = draw(receiveReview(fresh, envelope({ requestId: "req-9", draft: d }))).container.querySelector(".review-draft")!.textContent;
    expect(again).toBe(first);
  });

  it("draws an overview recorded before the draft existed, as an empty one", () => {
    const old = notesFixture.envelopes.ok as unknown as ReviewEnvelope;
    expect(draw(receiveReview(openReview(3, old.requestId), old)).container.querySelector(".review-draft")).toBeNull();
  });
});

describe("ReviewOverlay: the line under the box", () => {
  it("shows the key line the design gives, and the one-line status above it", () => {
    const { container } = draw(withStatus(loaded(), "nothing to undo"));
    expect(container.querySelector(".review-keys")!.textContent).toBe(REVIEW_KEY_LINE);
    expect(REVIEW_KEY_LINE).toBe("j/k move · Enter open · x revert · u undo · i comment · s send · [ ] turn · o editor · q close");
    expect(container.querySelector(".review-status")!.textContent).toBe("nothing to undo");
    cleanup();
    expect(draw(loaded()).container.querySelector(".review-status")).toBeNull();
  });

  it("asks the whole-file question on its own line", () => {
    const state = press(loaded(), { kind: "revert" });
    expect(draw(state).container.querySelector(".review-question")!.textContent).toBe("revert the whole file a.rs to before turn 7? y/n");
  });
});

describe("ReviewOverlay: the send preview", () => {
  const preview = (over: { queued?: boolean; notOnDisk?: ReviewNotOnDisk[] } = {}) => {
    const asked = applyReviewKey(withDraft(draft({ comments: [COMMENT], reverts: [REVERT] })), { kind: "send" }, () => "send-1");
    return receivePreview(asked.state, {
      requestId: "send-1",
      tab: 1,
      digest: "9f2c4e1a0b7d3c55",
      text: "Review of your last turn: 1 comment, 1 revert.\n\nComments:\n1. a.rs:11",
      notOnDisk: over.notOnDisk ?? [],
      queued: over.queued ?? false,
    });
  };

  it("shows the message, a line per revert that is not on disk, and the question", () => {
    const { container } = draw(
      preview({
        notOnDisk: [
          { id: 2, path: "core/src/y.rs", what: "hunk", lines: [10, 14], why: "only in the editor, not saved" },
          { id: 3, path: "core/src/z.rs", what: "hunk", lines: [7, 7], why: "undone" },
        ],
      }),
    );
    const box = container.querySelector(".review-preview")!;
    expect(box.querySelector(".review-preview-text")!.textContent).toContain("Review of your last turn: 1 comment, 1 revert.");
    expect(Array.from(box.querySelectorAll(".review-note")).map((n) => n.textContent)).toEqual([
      "core/src/y.rs lines 10-14: only in the editor, not saved",
      "core/src/z.rs line 7: undone",
    ]);
    expect(container.querySelector(".review-question")!.textContent).toBe("y sends · n cancels");
  });

  it("words a whole-file revert by what it did, with no line range, as Rust sends it", () => {
    const sent = previewFixture.envelope as unknown as ReviewSendPreviewEnvelope & { kind: string };
    const asked = applyReviewKey(withDraft(draft({ comments: [COMMENT], reverts: [REVERT] })), { kind: "send" }, () => sent.requestId);
    const { container } = draw(receivePreview(asked.state, sent));
    const box = container.querySelector(".review-preview")!;
    expect(box.querySelector(".review-preview-text")!.textContent).toBe(sent.text);
    expect(Array.from(box.querySelectorAll(".review-note")).map((n) => n.textContent)).toEqual([
      "core/src/foo.rs lines 120-127: changed since",
      "new.rs (deleted): changed since",
      "old.rs (restored): only in the editor, not saved",
      "all.rs (whole file): undone",
    ]);
  });

  it("says it queues behind a running turn when it will", () => {
    expect(draw(preview({ queued: true })).container.querySelector(".review-question")!.textContent).toBe("y queues it behind the running turn · n cancels");
  });

  it("says it is preparing while the preview has not arrived, and shows no box", () => {
    const asked = applyReviewKey(withDraft(draft({ comments: [COMMENT] })), { kind: "send" }, () => "send-1").state;
    const { container } = draw(asked);
    expect(container.querySelector(".review-preview")).toBeNull();
    expect(container.querySelector(".review-question")!.textContent).toBe("preparing what would be sent…");
  });
});

describe("ReviewOverlay: the comment input", () => {
  const typing = () => {
    let state = withDraft(draft());
    state = { ...state, cursor: "hunk:a.rs:0" };
    return press(state, { kind: "comment" });
  };

  it("is a real text box in the overlay, naming the lines it comments on", () => {
    const { container } = draw(typing());
    const input = container.querySelector<HTMLInputElement>(".review-prompt input")!;
    expect(input.getAttribute("aria-label")).toBe("Comment");
    expect(container.querySelector(".review-question")!.textContent).toBe("comment on a.rs:11");
  });

  it("hands its text, its Enter and its Escape to the caller, and no other key", () => {
    const onChange = vi.fn();
    const onAccept = vi.fn();
    const onCancel = vi.fn();
    const { container } = render(<ReviewOverlay state={typing()} onClose={() => {}} onCommentChange={onChange} onCommentAccept={onAccept} onCommentCancel={onCancel} />);
    const input = container.querySelector<HTMLInputElement>(".review-prompt input")!;
    fireEvent.change(input, { target: { value: "ad" } });
    expect(onChange).toHaveBeenCalledWith("ad");
    fireEvent.keyDown(input, { key: "a" });
    fireEvent.keyDown(input, { key: "d" });
    expect(onAccept).not.toHaveBeenCalled();
    expect(onCancel).not.toHaveBeenCalled();
    fireEvent.keyDown(input, { key: "Enter" });
    expect(onAccept).toHaveBeenCalledTimes(1);
    fireEvent.keyDown(input, { key: "Escape" });
    expect(onCancel).toHaveBeenCalledTimes(1);
  });

  it("has no input when no comment is being written", () => {
    expect(draw(withDraft(draft())).container.querySelector(".review-prompt")).toBeNull();
  });
});

describe("ReviewOverlay: the recovery rows", () => {
  const entries = [{ id: "e-1", path: "core/src/x.rs", at: Date.UTC(2026, 0, 1, 11, 0, 0) }];

  it("are stops of kind recovery at the top, above the files", () => {
    const state = receiveReview(openReview(1, "req-1", entries), envelope());
    const { container } = draw(state);
    const stops = Array.from(container.querySelectorAll("[data-nav-stop]")).map((e) => e.getAttribute("data-nav-stop"));
    expect(stops.slice(0, 2)).toEqual(["recovery", "file"]);
    const row = container.querySelector('[data-nav-stop="recovery"]')!;
    expect(row.textContent).toContain("an interrupted revert left core/src/x.rs");
    expect(row.classList.contains("row-current")).toBe(true);
  });

  it("are drawn before an overview has arrived, and when the review compared nothing", () => {
    expect(draw(openReview(1, "req-1", entries)).container.querySelector('[data-nav-stop="recovery"]')).not.toBeNull();
    cleanup();
    const nothing = receiveReview(openReview(1, "req-1", entries), envelope({ files: [], compared: false }));
    expect(draw(nothing).container.querySelector('[data-nav-stop="recovery"]')).not.toBeNull();
  });

  it("go when the shell says they are resolved, and the question about one with them", () => {
    let state = receiveReview(openReview(1, "req-1", entries), envelope());
    state = press(state, { kind: "toggle" });
    expect(draw(state).container.querySelector(".review-question")!.textContent).toContain("restore core/src/x.rs to its bytes from before the interrupted revert?");
    cleanup();
    const { container } = draw(receiveRecovery(state, []));
    expect(container.querySelector('[data-nav-stop="recovery"]')).toBeNull();
    expect(container.querySelector(".review-question")).toBeNull();
  });

  it("add no inline style: no colour of their own", () => {
    const { container } = draw(receiveReview(openReview(1, "req-1", entries), envelope()));
    expect(container.querySelectorAll("[style]")).toHaveLength(0);
  });
});
