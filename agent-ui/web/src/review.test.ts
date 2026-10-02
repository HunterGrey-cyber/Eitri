// @vitest-environment jsdom
import { describe, expect, it } from "vitest";
import { REVIEW_KEYS, resolveKey } from "./keymap";
import type { KeyLike } from "./keymap";
import {
  EMPTY_DRAFT,
  applyReviewKey,
  boxUnderCursor,
  commentLines,
  commentsUnder,
  cursorStop,
  draftLine,
  promptText,
  receiveDraft,
  receivePreview,
  receiveRecovery,
  revertMark,
  revertOf,
  typeComment,
  failRequest,
  fileFlags,
  openReview,
  receiveDiff,
  receiveReview,
  resolveReviewKey,
  reviewNotes,
  reviewStops,
  reviewTitle,
  scrollBoxFirst,
  stopKey,
  stopTarget,
} from "./review";
import type { ReviewAction, ReviewPrompt, ReviewState } from "./review";
import type { ReviewDiffEnvelope, ReviewDraft, ReviewEnvelope, ReviewFile, ReviewHunk, ReviewTurn } from "./types";

const key = (k: string, over: Partial<KeyLike> = {}): KeyLike => ({ key: k, ctrlKey: false, shiftKey: false, isComposing: false, ...over });

/** One `REVIEW_KEYS[i].keys` token, as what a person presses; `g` waiting for its second key is `pending`. */
function parseToken(token: string): { ev: KeyLike; pendingG: boolean } {
  if (token === "gg") return { ev: key("g"), pendingG: true };
  if (token === "Esc") return { ev: key("Escape"), pendingG: false };
  if (token.startsWith("Ctrl+")) return { ev: key(token.slice(5), { ctrlKey: true }), pendingG: false };
  if (/^[A-Z]$/.test(token)) return { ev: key(token, { shiftKey: true }), pendingG: false };
  return { ev: key(token), pendingG: false };
}

describe("REVIEW_KEYS <-> resolveReviewKey", () => {
  it("every key REVIEW_KEYS lists does something", () => {
    for (const { keys } of REVIEW_KEYS) {
      for (const token of keys.split(" / ")) {
        const { ev, pendingG } = parseToken(token);
        const action = resolveReviewKey(ev, pendingG);
        expect(action, `"${token}" (from "${keys}")`).not.toBeNull();
        expect(action?.kind, `"${token}" only starts a prefix`).not.toBe("pending");
      }
    }
  });

  it("every key that does something is listed", () => {
    const listed = new Set(REVIEW_KEYS.flatMap((row) => row.keys.split(" / ")));
    const candidates: { token: string; ev: KeyLike; pendingG?: boolean }[] = [
      ..."abcdefghijklmnopqrstuvwxyz".split("").map((l) => ({ token: l, ev: key(l) })),
      ..."ABCDEFGHIJKLMNOPQRSTUVWXYZ".split("").map((l) => ({ token: l, ev: key(l, { shiftKey: true }) })),
      ..."0123456789[]?:/".split("").map((c) => ({ token: c, ev: key(c) })),
      { token: "Enter", ev: key("Enter") },
      { token: "Esc", ev: key("Escape") },
      { token: "Ctrl+d", ev: key("d", { ctrlKey: true }) },
      { token: "Ctrl+u", ev: key("u", { ctrlKey: true }) },
      { token: "gg", ev: key("g"), pendingG: true },
    ];
    for (const { token, ev, pendingG } of candidates) {
      const action = resolveReviewKey(ev, pendingG ?? false);
      if (action === null || action.kind === "pending") continue;
      expect(listed.has(token), `"${token}" resolves to ${JSON.stringify(action)} but no REVIEW_KEYS row spells it`).toBe(true);
    }
  });

  it("x, u, i and s are bound, and each has a row that spells it (the two-way reconciliation above holds them to it)", () => {
    const listed = REVIEW_KEYS.flatMap((row) => row.keys.split(" / "));
    const meant: Record<string, ReviewAction["kind"]> = { x: "revert", u: "undo", i: "comment", s: "send" };
    for (const [k, kind] of Object.entries(meant)) {
      expect(resolveReviewKey(key(k), false), k).toEqual({ kind });
      expect(listed, k).toContain(k);
    }
  });

  it("they are plain keys: a chord, a capital or a held modifier is some other key and does nothing", () => {
    for (const k of ["x", "u", "i", "s"]) {
      expect(resolveReviewKey(key(k, { altKey: true }), false), `Alt+${k}`).toBeNull();
      expect(resolveReviewKey(key(k, { metaKey: true }), false), `Meta+${k}`).toBeNull();
    }
    expect(resolveReviewKey(key("u", { ctrlKey: true }), false), "Ctrl+u is half a page").toEqual({ kind: "half-page", delta: -1 });
    expect(resolveReviewKey(key("X", { shiftKey: true }), false)).toBeNull();
    expect(resolveReviewKey(key("x"), true), "after a g").toBeNull();
  });

  it("with a prompt open the overlay's own table is not read: only y, n and Escape mean anything", () => {
    const question: ReviewPrompt = { kind: "revert-file", path: "a.rs", turn: 7, scope: "turn", shownTurn: 7 };
    expect(resolveReviewKey(key("y"), false, question)).toEqual({ kind: "answer", answer: "yes" });
    expect(resolveReviewKey(key("n"), false, question)).toEqual({ kind: "answer", answer: "no" });
    expect(resolveReviewKey(key("Escape"), false, question)).toEqual({ kind: "answer", answer: "cancel" });
    for (const k of ["j", "x", "u", "s", "i", "q", "c", "o", "a", "d"]) expect(resolveReviewKey(key(k), false, question), k).toBeNull();
    expect(resolveReviewKey(key("y", { ctrlKey: true }), false, question)).toBeNull();
    const input: ReviewPrompt = { kind: "comment", path: "a.rs", turn: 7, scope: "turn", from: 1, to: 1, text: "" };
    expect(resolveReviewKey(key("y"), false, input), "a letter for the text box, not an answer").toBeNull();
    expect(resolveReviewKey(key("Escape"), false, input)).toEqual({ kind: "answer", answer: "cancel" });
  });

  it("only a plain key counts: a chord, a held Alt or Meta, or a composing input method does nothing", () => {
    expect(resolveReviewKey(key("j", { ctrlKey: true }), false)).toBeNull();
    expect(resolveReviewKey(key("j", { altKey: true }), false)).toBeNull();
    expect(resolveReviewKey(key("q", { metaKey: true }), false)).toBeNull();
    expect(resolveReviewKey(key("j", { isComposing: true }), false)).toBeNull();
    expect(resolveReviewKey(key("d", { ctrlKey: true, shiftKey: true }), false)).toBeNull();
  });

  it("a g followed by anything but g ends the prefix and does nothing", () => {
    expect(resolveReviewKey(key("g"), false)).toEqual({ kind: "pending" });
    expect(resolveReviewKey(key("j"), true)).toBeNull();
    expect(resolveReviewKey(key("G", { shiftKey: true }), true)).toBeNull();
  });

  it("brackets work with AltGr or Option held, as on a German layout", () => {
    expect(resolveReviewKey(key("[", { altKey: true }), false)).toEqual({ kind: "turn", delta: -1 });
    expect(resolveReviewKey(key("]", { altKey: true }), false)).toEqual({ kind: "turn", delta: 1 });
  });
});

describe("BROWSE_KEYS <-> resolveKey, for c", () => {
  it("c opens the review in BROWSE and is a letter in INPUT; zc stays a fold; Alt+c is nothing", () => {
    const ctx = { sessionEnded: false };
    expect(resolveKey("browse", key("c"), ctx)).toEqual({ kind: "review" });
    expect(resolveKey("input", key("c"), ctx)).toBeNull();
    expect(resolveKey("browse", key("c"), { ...ctx, pending: "z" })).toMatchObject({ kind: expect.not.stringMatching(/^review$/) });
    expect(resolveKey("browse", key("c", { altKey: true }), ctx)).toBeNull();
    expect(resolveKey("browse", key("c", { metaKey: true }), ctx)).toBeNull();
    expect(resolveKey("browse", key("c", { ctrlKey: true }), { ...ctx, turnRunning: false })).toBeNull();
    expect(resolveKey("browse", key("C", { shiftKey: true }), ctx)).toBeNull();
  });

  it("is offered on an ended session too: what changed on disk is still there to read", () => {
    expect(resolveKey("browse", key("c"), { sessionEnded: true })).toEqual({ kind: "review" });
  });
});

const TURN6: ReviewTurn = { n: 6, turnId: "t6", startedAt: Date.UTC(2026, 0, 1, 10, 0, 0), endedAt: Date.UTC(2026, 0, 1, 10, 1, 0), state: "ok", late: false, overlappedNext: false, overlappedTab: false, reason: null };
const TURN7: ReviewTurn = { ...TURN6, n: 7, turnId: "t7", startedAt: Date.UTC(2026, 0, 1, 11, 0, 0), endedAt: Date.UTC(2026, 0, 1, 11, 2, 0) };

const file = (path: string, over: Partial<ReviewFile> = {}): ReviewFile => ({ path, added: 1, removed: 0, origin: "agent", binary: false, tooLarge: false, nested: false, ...over });

function envelope(over: Partial<ReviewEnvelope> = {}): ReviewEnvelope {
  return {
    requestId: "req-1",
    tab: 1,
    scope: "turn",
    current: 7,
    turns: [TURN6, TURN7],
    files: [file("a.rs", { added: 41, removed: 6 }), file("b.md", { origin: "agent_only", added: 0 }), file("Cargo.lock", { origin: "workspace" }), file("c.rs", { origin: "workspace" })],
    compared: true,
    pendingNoResult: 0,
    notes: ["changed on disk during this turn"],
    ...over,
  };
}

function diffOf(requestId: string, path: string, over: Partial<ReviewDiffEnvelope> = {}): ReviewDiffEnvelope {
  return {
    requestId,
    tab: 1,
    turn: 7,
    path,
    added: 2,
    removed: 1,
    hunks: [
      {
        id: 0,
        header: "@@ -10,3 +10,4 @@",
        lines: [
          { kind: "context", text: "keep", oldNo: 10, newNo: 10 },
          { kind: "removed", text: "old", oldNo: 11, newNo: null },
          { kind: "added", text: "new", oldNo: null, newNo: 11 },
        ],
      },
      { id: 1, header: "@@ -40,2 +41,2 @@", lines: [{ kind: "added", text: "later", oldNo: null, newNo: 41 }] },
    ],
    ...over,
  };
}

let counter = 0;
const nextId = () => `id-${++counter}`;
const loaded = (over: Partial<ReviewEnvelope> = {}): ReviewState => receiveReview(openReview(1, "req-1"), envelope(over));
const press = (state: ReviewState, action: ReviewAction) => applyReviewKey(state, action, nextId);

describe("the stops", () => {
  it("are the named files, then a folded group header; the unnamed files appear once the group is open", () => {
    let state = loaded();
    expect(reviewStops(state).map(stopKey)).toEqual(["file:a.rs", "file:b.md", "group"]);
    state = { ...state, groupOpen: true };
    expect(reviewStops(state).map(stopKey)).toEqual(["file:a.rs", "file:b.md", "group", "file:Cargo.lock", "file:c.rs"]);
  });

  it("are called file, hunk and group, never row", () => {
    const state = { ...loaded(), expanded: ["a.rs"], groupOpen: true };
    const open = receiveDiff({ ...state, diffs: { "a.rs": { status: "loading", requestId: "d1" } } }, diffOf("d1", "a.rs"));
    const kinds = new Set(reviewStops(open).map((s) => s.kind));
    expect(kinds).toEqual(new Set(["file", "hunk", "group"]));
  });

  it("have no group when nothing is unnamed", () => {
    const state = loaded({ files: [file("a.rs")] });
    expect(reviewStops(state).map(stopKey)).toEqual(["file:a.rs"]);
  });

  it("show no files when nothing was compared", () => {
    const state = loaded({ turns: [TURN6, { ...TURN7, state: "no_baseline", reason: "git is not installed" }], compared: false });
    expect(reviewStops(state)).toEqual([]);
    // A session scope with no baseline anywhere, or a turn whose baseline is still being taken, too.
    expect(reviewStops(loaded({ scope: "session", compared: false }))).toEqual([]);
    expect(reviewStops(loaded({ turns: [TURN6, { ...TURN7, state: "pending" }], compared: false }))).toEqual([]);
  });
});

describe("the cursor", () => {
  it("moves over the stops with j/k, clamped at both ends, and jumps with gg/G and Ctrl+d/Ctrl+u", () => {
    let state = loaded();
    state = press(state, { kind: "move", delta: -1 }).state;
    expect(state.cursor).toBe("file:a.rs");
    state = press(state, { kind: "move", delta: 1 }).state;
    expect(state.cursor).toBe("file:b.md");
    state = press(state, { kind: "jump", to: "last" }).state;
    expect(state.cursor).toBe("group");
    state = press(state, { kind: "move", delta: 1 }).state;
    expect(state.cursor).toBe("group");
    state = press(state, { kind: "jump", to: "first" }).state;
    expect(state.cursor).toBe("file:a.rs");
    state = press(state, { kind: "half-page", delta: 1 }).state;
    expect(state.cursor).toBe("group");
    state = press(state, { kind: "half-page", delta: -1 }).state;
    expect(state.cursor).toBe("file:a.rs");
  });

  it("stays on its stop when a patch arrives above it", () => {
    let state = loaded();
    state = press(state, { kind: "toggle" }).state; // opens a.rs, asks for it
    state = press(state, { kind: "move", delta: 1 }).state; // on b.md while the patch loads
    expect(state.cursor).toBe("file:b.md");
    const requestId = (Object.values(state.diffs)[0] as { requestId: string }).requestId;
    state = receiveDiff(state, diffOf(requestId, "a.rs"));
    expect(cursorStop(state)).toEqual({ kind: "file", path: "b.md" });
  });
});

describe("Enter", () => {
  it("opens a file and asks for its patch under the overlay's own scope, once", () => {
    const { state, effect } = press(loaded(), { kind: "toggle" });
    expect(effect).toEqual({ kind: "request-diff", requestId: expect.any(String), turn: 7, scope: "turn", path: "a.rs" });
    expect(state.expanded).toEqual(["a.rs"]);
    const closed = press(state, { kind: "toggle" });
    expect(closed.state.expanded).toEqual([]);
    expect(closed.effect).toBeNull();
    const again = press(closed.state, { kind: "toggle" });
    expect(again.effect, "the loading entry is kept, so no second request").toBeNull();
  });

  it("asks under the session scope when that is what is shown", () => {
    const { effect } = press(loaded({ scope: "session" }), { kind: "toggle" });
    expect(effect).toMatchObject({ kind: "request-diff", scope: "session", path: "a.rs" });
  });

  it("asks again for a patch that failed", () => {
    let state = press(loaded(), { kind: "toggle" }).state;
    const requestId = (Object.values(state.diffs)[0] as { requestId: string }).requestId;
    state = failRequest(state, requestId, "git unavailable");
    expect(state.diffs["a.rs"]).toEqual({ status: "failed", error: "git unavailable" });
    state = press(state, { kind: "toggle" }).state; // closes
    const again = press(state, { kind: "toggle" });
    expect(again.effect).toMatchObject({ kind: "request-diff", path: "a.rs" });
  });

  it("does nothing on a binary file, one too large to snapshot and a nested repository", () => {
    for (const flags of [{ binary: true }, { tooLarge: true }, { nested: true }]) {
      const state = loaded({ files: [file("x", flags)] });
      const next = press(state, { kind: "toggle" });
      expect(next.effect, JSON.stringify(flags)).toBeNull();
      expect(next.state.expanded).toEqual([]);
    }
  });

  it("opens the folded group, and on a hunk closes its file and lands on the file's row", () => {
    let state = loaded();
    state = press(state, { kind: "jump", to: "last" }).state;
    state = press(state, { kind: "toggle" }).state;
    expect(state.groupOpen).toBe(true);
    state = loaded();
    state = press(state, { kind: "toggle" }).state;
    const requestId = (Object.values(state.diffs)[0] as { requestId: string }).requestId;
    state = receiveDiff(state, diffOf(requestId, "a.rs"));
    state = press(state, { kind: "move", delta: 1 }).state;
    expect(state.cursor).toBe("hunk:a.rs:0");
    state = press(state, { kind: "toggle" }).state;
    expect(state.expanded).toEqual([]);
    expect(state.cursor).toBe("file:a.rs");
  });
});

describe("replies", () => {
  it("an overview for any request but the newest is dropped", () => {
    const state = openReview(1, "req-2");
    expect(receiveReview(state, envelope({ requestId: "req-1" }))).toBe(state);
  });

  it("a patch nobody is waiting for is dropped", () => {
    const state = loaded();
    expect(receiveDiff(state, diffOf("nobody", "a.rs"))).toBe(state);
  });

  it("a refusal is the overlay's error, and the overlay goes back to the scope it shows", () => {
    let state = loaded();
    state = press(state, { kind: "scope" }).state;
    expect(state.scope).toBe("session");
    state = failRequest(state, state.requestId, "git unavailable");
    expect(state.error).toBe("git unavailable");
    expect(state.scope).toBe("turn");
  });

  it("a refusal for a replaced request is ignored", () => {
    const state = loaded();
    expect(failRequest(state, "old", "x")).toBe(state);
  });

  it("a new overview starts the cursor, the open files and the patches over", () => {
    let state = loaded();
    state = press(state, { kind: "toggle" }).state;
    const moved = press(state, { kind: "turn", delta: -1 });
    const next = receiveReview(moved.state, envelope({ requestId: moved.state.requestId, current: 6 }));
    expect(next.expanded).toEqual([]);
    expect(next.diffs).toEqual({});
    expect(next.cursor).toBeNull();
    expect(next.turn).toBe(6);
  });
});

describe("[ ] and S", () => {
  it("[ and ] ask for the previous and next turn in turn scope, and stop at the ends", () => {
    const prev = press(loaded(), { kind: "turn", delta: -1 });
    expect(prev.effect).toEqual({ kind: "request", requestId: expect.any(String), turn: 6, scope: "turn" });
    expect(prev.state.requestId).toBe((prev.effect as { requestId: string }).requestId);
    expect(press(loaded(), { kind: "turn", delta: 1 }).effect, "7 is the newest").toBeNull();
    expect(press(loaded({ current: 6 }), { kind: "turn", delta: -1 }).effect, "6 is the oldest").toBeNull();
  });

  it("do nothing in session scope, where every turn is shown at once", () => {
    expect(press(loaded({ scope: "session" }), { kind: "turn", delta: -1 }).effect).toBeNull();
  });

  it("S asks for the session, and back for the turn that was shown", () => {
    const toSession = press(loaded({ current: 6 }), { kind: "scope" });
    expect(toSession.effect).toMatchObject({ kind: "request", scope: "session" });
    const session = receiveReview(toSession.state, envelope({ requestId: toSession.state.requestId, scope: "session", current: 7 }));
    const back = press(session, { kind: "scope" });
    expect(back.effect).toMatchObject({ kind: "request", scope: "turn", turn: 6 });
  });

  it("do nothing while the first overview has not arrived", () => {
    const fresh = openReview(1, "req-1");
    expect(press(fresh, { kind: "scope" }).effect).toBeNull();
    expect(press(fresh, { kind: "turn", delta: -1 }).effect).toBeNull();
  });
});

describe("o and y", () => {
  const withDiff = () => {
    let state = loaded();
    state = press(state, { kind: "toggle" }).state;
    const requestId = (Object.values(state.diffs)[0] as { requestId: string }).requestId;
    return receiveDiff(state, diffOf(requestId, "a.rs"));
  };

  it("o on a hunk opens the file at the hunk's first new line", () => {
    let state = withDiff();
    state = press(state, { kind: "move", delta: 1 }).state;
    state = press(state, { kind: "move", delta: 1 }).state;
    expect(state.cursor).toBe("hunk:a.rs:1");
    expect(press(state, { kind: "open" }).effect).toEqual({ kind: "open", path: "a.rs", line: 41, turn: 7, scope: "turn" });
    expect(press(state, { kind: "copy" }).effect).toEqual({ kind: "copy", text: "a.rs:41" });
  });

  it("o on a file with its patch loaded opens its first hunk; with none loaded, the file alone", () => {
    expect(press(withDiff(), { kind: "open" }).effect).toEqual({ kind: "open", path: "a.rs", line: 10, turn: 7, scope: "turn" });
    expect(press(loaded(), { kind: "open" }).effect).toEqual({ kind: "open", path: "a.rs", line: null, turn: 7, scope: "turn" });
    expect(press(loaded(), { kind: "copy" }).effect).toEqual({ kind: "copy", text: "a.rs" });
  });

  it("neither does anything on the group header", () => {
    const state = press(loaded(), { kind: "jump", to: "last" }).state;
    expect(press(state, { kind: "open" }).effect).toBeNull();
    expect(press(state, { kind: "copy" }).effect).toBeNull();
    expect(stopTarget(state, cursorStop(state))).toBeNull();
  });
});

describe("what the header says", () => {
  it("names the turn, its times and what happened to the disk, never who did it", () => {
    const title = reviewTitle(envelope());
    expect(title).toMatch(/^review · turn 7 of 7 · \d\d:\d\d:\d\d → \d\d:\d\d:\d\d · changed on disk during this turn$/);
    expect(title).not.toMatch(/agent/i);
  });

  it("says session scope in its own words", () => {
    expect(reviewTitle(envelope({ scope: "session" }))).toMatch(/^review · session · 2 turns · \d\d:\d\d:\d\d → \d\d:\d\d:\d\d · changed on disk during this session$/);
  });

  it("is quiet for a turn that is fine: the heading note is the title's own words", () => {
    expect(reviewNotes(envelope())).toEqual([]);
  });

  it("says only what Rust's notes say, never a line of its own from the turn flags", () => {
    const flagged = envelope({ turns: [TURN6, { ...TURN7, late: true, overlappedNext: true, overlappedTab: true, state: "unfinished" }] });
    expect(reviewNotes(flagged)).toEqual([]);
    const notes = ["changed on disk during this turn", "may include the next turn's first changes"];
    expect(reviewNotes({ ...flagged, notes })).toEqual(["may include the next turn's first changes"]);
  });

  it("says why a file has nothing to open", () => {
    expect(fileFlags(file("x", { binary: true }))).toEqual(["binary"]);
    expect(fileFlags(file("x", { tooLarge: true }))).toEqual(["too large to snapshot"]);
    expect(fileFlags(file("x", { nested: true }))).toEqual(["nested repository, not reviewed"]);
    expect(fileFlags(file("x"))).toEqual([]);
  });
});

describe("the box under the cursor", () => {
  const box = (scrollHeight: number, clientHeight: number, scrollTop = 0) => {
    const el = document.createElement("div");
    Object.defineProperty(el, "scrollHeight", { value: scrollHeight });
    Object.defineProperty(el, "clientHeight", { value: clientHeight });
    el.scrollTop = scrollTop;
    return el;
  };

  it("scrolls first while it has room in that direction, then lets the cursor move", () => {
    const el = box(500, 260);
    expect(scrollBoxFirst(el, 1, 40)).toBe(true);
    expect(el.scrollTop).toBe(40);
    el.scrollTop = 240;
    expect(scrollBoxFirst(el, 1, 40), "at the bottom").toBe(false);
    expect(scrollBoxFirst(el, -1, 40)).toBe(true);
    el.scrollTop = 0;
    expect(scrollBoxFirst(el, -1, 40), "at the top").toBe(false);
  });

  it("leaves a box that fits, and no box at all, to the cursor", () => {
    expect(scrollBoxFirst(box(100, 260), 1, 40)).toBe(false);
    expect(scrollBoxFirst(null, 1, 40)).toBe(false);
  });

  it("is found from the hunk row the cursor is on, and only there", () => {
    const root = document.createElement("div");
    root.innerHTML =
      '<div class="review-hunk"><div data-nav-stop="hunk" aria-current="true"></div><div class="review-hunk-lines" id="mine"></div></div>' +
      '<div class="review-hunk"><div data-nav-stop="hunk"></div><div class="review-hunk-lines" id="other"></div></div>';
    expect(boxUnderCursor(root)?.id).toBe("mine");
    expect(boxUnderCursor(document.createElement("div"))).toBeNull();
    expect(boxUnderCursor(null)).toBeNull();
  });
});

// ---------------------------------------------------------------------------------------------------------
// x, u, i, s and the recovery rows

const COMMENT = { id: 1, turn: 7, path: "a.rs", from: 11, to: 11, anchor: ["new"], text: "why?" };
const REVERT = { id: 1, turn: 7, path: "a.rs", hunk: 1, header: "@@ -40,2 +41,2 @@", what: "hunk" as const, lines: [41, 41] as [number, number], source: "panel", undone: false };
const draftOf = (over: Partial<ReviewDraft> = {}): ReviewDraft => ({ ...EMPTY_DRAFT, ...over });

/** `a.rs` open with its two hunks, the cursor on the file row, and `draft` shown. */
function opened(draft: ReviewDraft = EMPTY_DRAFT, diff: Partial<ReviewDiffEnvelope> = {}, over: Partial<ReviewEnvelope> = {}): ReviewState {
  let state = loaded({ draft, ...over });
  state = press(state, { kind: "toggle" }).state;
  const entry = state.diffs["a.rs"];
  if (entry?.status !== "loading") throw new Error("no request");
  return receiveDiff(state, diffOf(entry.requestId, "a.rs", diff));
}
const moveTo = (state: ReviewState, stop: string): ReviewState => {
  for (let i = 0; i < 12 && state.cursor !== stop; i++) state = press(state, { kind: "move", delta: 1 }).state;
  expect(state.cursor).toBe(stop);
  return state;
};

describe("x", () => {
  it("on a hunk reverts it, naming it by id and header and the turn and scope shown", () => {
    const state = moveTo(opened(), "hunk:a.rs:1");
    const { effect, state: next } = press(state, { kind: "revert" });
    expect(effect).toEqual({ kind: "revert", requestId: expect.any(String), turn: 7, scope: "turn", path: "a.rs", target: { hunk: 1, header: "@@ -40,2 +41,2 @@" } });
    expect(next.prompt, "a hunk revert asks nothing").toBeNull();
  });

  it("on a file row asks first, and y reverts the whole file", () => {
    const asked = press(loaded(), { kind: "revert" });
    expect(asked.effect).toBeNull();
    expect(asked.state.prompt).toMatchObject({ kind: "revert-file", path: "a.rs" });
    expect(promptText(asked.state.prompt!)).toBe("revert the whole file a.rs to before turn 7? y/n");
    const yes = press(asked.state, { kind: "answer", answer: "yes" });
    expect(yes.effect).toEqual({ kind: "revert", requestId: expect.any(String), turn: 7, scope: "turn", path: "a.rs", target: "file" });
    expect(yes.state.prompt).toBeNull();
  });

  it("on a file row, n and Escape revert nothing", () => {
    const asked = press(loaded(), { kind: "revert" }).state;
    for (const answer of ["no", "cancel"] as const) {
      const next = press(asked, { kind: "answer", answer });
      expect(next.effect, answer).toBeNull();
      expect(next.state.prompt, answer).toBeNull();
    }
  });

  it("names the first turn of the session when the session is shown, since that is what the file goes back to", () => {
    const asked = press(loaded({ scope: "session" }), { kind: "revert" }).state;
    expect(promptText(asked.prompt!)).toBe("revert the whole file a.rs to before turn 6? y/n");
  });

  it("on a comment deletes it", () => {
    const state = moveTo(opened(draftOf({ comments: [COMMENT] })), "comment:a.rs:1");
    expect(press(state, { kind: "revert" }).effect).toEqual({ kind: "comment-remove", requestId: expect.any(String), id: 1 });
  });

  it("on a hunk of a binary file says it reverts only whole, and asks for nothing", () => {
    const state = moveTo(opened(EMPTY_DRAFT, { binary: true }), "hunk:a.rs:0");
    const next = press(state, { kind: "revert" });
    expect(next.effect).toBeNull();
    expect(next.state.status).toBe("binary files revert only whole; x on the file row");
    expect(next.state.prompt).toBeNull();
    // The file row of the same file still offers the whole-file revert.
    expect(press(loaded({ files: [file("a.rs", { binary: true })] }), { kind: "revert" }).state.prompt).toMatchObject({ kind: "revert-file" });
  });

  it("does nothing on the group header", () => {
    const state = press(loaded(), { kind: "jump", to: "last" }).state;
    expect(press(state, { kind: "revert" })).toEqual({ state: { ...state, status: null }, effect: null });
  });

  it("is not a way to revert a hunk whose patch has not arrived", () => {
    let state = press(loaded(), { kind: "toggle" }).state;
    expect(press(state, { kind: "revert" }).state.prompt, "the cursor is on the file row, which asks").toMatchObject({ kind: "revert-file" });
    state = { ...state, cursor: "hunk:a.rs:0" };
    expect(press(state, { kind: "revert" }).effect).toBeNull();
  });
});

describe("u", () => {
  it("undoes the last revert when the draft says it can", () => {
    const { effect } = press(loaded({ draft: draftOf({ canUndo: true, reverts: [REVERT] }) }), { kind: "undo" });
    expect(effect).toEqual({ kind: "undo", requestId: expect.any(String) });
  });

  it("says nothing to undo when it cannot, and the line is gone at the next key", () => {
    const next = press(loaded(), { kind: "undo" });
    expect(next.effect).toBeNull();
    expect(next.state.status).toBe("nothing to undo");
    expect(press(next.state, { kind: "move", delta: 1 }).state.status).toBeNull();
  });
});

describe("i", () => {
  it("opens a one-line input on a hunk, over the lines it added", () => {
    const state = moveTo(opened(), "hunk:a.rs:0");
    const next = press(state, { kind: "comment" });
    expect(next.effect).toBeNull();
    expect(next.state.prompt).toEqual({ kind: "comment", path: "a.rs", turn: 7, scope: "turn", from: 11, to: 11, text: "" });
  });

  it("Enter saves it: the effect carries the min and max new line of the added lines and the text", () => {
    const wide: ReviewHunk = {
      id: 0,
      header: "@@ -5,2 +5,5 @@",
      lines: [
        { kind: "context", text: "c", oldNo: 5, newNo: 5 },
        { kind: "added", text: "x", oldNo: null, newNo: 6 },
        { kind: "removed", text: "y", oldNo: 6, newNo: null },
        { kind: "added", text: "z", oldNo: null, newNo: 7 },
        { kind: "added", text: "w", oldNo: null, newNo: 9 },
      ],
    };
    let state = moveTo(opened(EMPTY_DRAFT, { hunks: [wide] }), "hunk:a.rs:0");
    state = press(state, { kind: "comment" }).state;
    state = typeComment(state, "  why is this here?  ");
    const saved = press(state, { kind: "accept" });
    expect(saved.effect).toEqual({ kind: "comment-add", requestId: expect.any(String), turn: 7, scope: "turn", path: "a.rs", from: 6, to: 9, text: "why is this here?" });
    expect(saved.state.prompt).toBeNull();
  });

  it("Escape cancels, and an empty comment is saved as nothing", () => {
    let state = press(moveTo(opened(), "hunk:a.rs:0"), { kind: "comment" }).state;
    expect(press(state, { kind: "answer", answer: "cancel" })).toMatchObject({ effect: null, state: { prompt: null } });
    expect(press(state, { kind: "accept" })).toMatchObject({ effect: null, state: { prompt: null } });
    state = typeComment(state, "   ");
    expect(press(state, { kind: "accept" }).effect).toBeNull();
  });

  it("on a pure deletion takes the context line after it, or at the end of the file the one before", () => {
    const mid: ReviewHunk = {
      id: 0,
      header: "@@ -4,3 +4,1 @@",
      lines: [
        { kind: "context", text: "a", oldNo: 4, newNo: 4 },
        { kind: "removed", text: "b", oldNo: 5, newNo: null },
        { kind: "removed", text: "c", oldNo: 6, newNo: null },
        { kind: "context", text: "d", oldNo: 7, newNo: 5 },
      ],
    };
    const eof: ReviewHunk = {
      id: 0,
      header: "@@ -8,2 +8,1 @@",
      lines: [
        { kind: "context", text: "a", oldNo: 8, newNo: 8 },
        { kind: "removed", text: "b", oldNo: 9, newNo: null },
      ],
    };
    expect(commentLines(mid)).toEqual([5, 5]);
    expect(commentLines(eof)).toEqual([8, 8]);
    expect(commentLines({ id: 0, header: "@@ -1,1 +0,0 @@", lines: [{ kind: "removed", text: "x", oldNo: 1, newNo: null }] })).toBeNull();
    const state = moveTo(opened(EMPTY_DRAFT, { hunks: [mid] }), "hunk:a.rs:0");
    expect(press(state, { kind: "comment" }).state.prompt).toMatchObject({ from: 5, to: 5 });
  });

  it("says so when the hunk has no line left in the file, and opens no input", () => {
    const gone: ReviewHunk = { id: 0, header: "@@ -1,1 +0,0 @@", lines: [{ kind: "removed", text: "x", oldNo: 1, newNo: null }] };
    const next = press(moveTo(opened(EMPTY_DRAFT, { hunks: [gone] }), "hunk:a.rs:0"), { kind: "comment" });
    expect(next.state.prompt).toBeNull();
    expect(next.state.status).toBe("this hunk has no line left in the file to comment on");
  });

  it("does nothing on a file row, a group or a comment", () => {
    expect(press(loaded(), { kind: "comment" }).state.prompt).toBeNull();
    const onComment = moveTo(opened(draftOf({ comments: [COMMENT] })), "comment:a.rs:1");
    expect(press(onComment, { kind: "comment" }).state.prompt).toBeNull();
  });
});

describe("s", () => {
  const withDraft = () => loaded({ draft: draftOf({ comments: [COMMENT], reverts: [REVERT] }) });
  const PREVIEW = { tab: 1, digest: "9f2c4e1a0b7d3c55", text: "Review of your last turn: 1 comment, 1 revert.", notOnDisk: [], queued: false };

  it("says there is nothing to send for an empty draft, and asks for nothing", () => {
    const next = press(loaded(), { kind: "send" });
    expect(next.effect).toBeNull();
    expect(next.state.status).toBe("nothing to send");
    expect(next.state.prompt).toBeNull();
  });

  it("asks for the preview with no confirmation, and waits for it", () => {
    const next = press(withDraft(), { kind: "send" });
    expect(next.effect).toEqual({ kind: "send", requestId: expect.any(String), confirm: null });
    expect(next.state.prompt).toEqual({ kind: "send", requestId: (next.effect as { requestId: string }).requestId, preview: null });
    // `y` before the preview is there sends nothing and keeps the question.
    const early = press(next.state, { kind: "answer", answer: "yes" });
    expect(early.effect).toBeNull();
    expect(early.state.prompt).not.toBeNull();
  });

  it("y on the preview sends with its digest, once", () => {
    const asked = press(withDraft(), { kind: "send" });
    const requestId = (asked.effect as { requestId: string }).requestId;
    const shown = receivePreview(asked.state, { ...PREVIEW, requestId });
    expect(shown.prompt).toMatchObject({ kind: "send", preview: { digest: "9f2c4e1a0b7d3c55", queued: false } });
    expect(promptText(shown.prompt!)).toBe("y sends · n cancels");
    const sent = press(shown, { kind: "answer", answer: "yes" });
    expect(sent.effect).toEqual({ kind: "send", requestId: expect.any(String), confirm: "9f2c4e1a0b7d3c55" });
    expect(sent.state.prompt).toBeNull();
    expect(press(sent.state, { kind: "answer", answer: "yes" }).effect, "no question is open any more").toBeNull();
  });

  it("words the question differently when the message will queue behind a running turn", () => {
    const asked = press(withDraft(), { kind: "send" });
    const shown = receivePreview(asked.state, { ...PREVIEW, requestId: (asked.effect as { requestId: string }).requestId, queued: true });
    expect(promptText(shown.prompt!)).toBe("y queues it behind the running turn · n cancels");
  });

  it("n and Escape send nothing, and a preview for a question that is gone is dropped", () => {
    const asked = press(withDraft(), { kind: "send" });
    const requestId = (asked.effect as { requestId: string }).requestId;
    const shown = receivePreview(asked.state, { ...PREVIEW, requestId });
    for (const answer of ["no", "cancel"] as const) expect(press(shown, { kind: "answer", answer })).toMatchObject({ effect: null, state: { prompt: null } });
    const cancelled = press(asked.state, { kind: "answer", answer: "cancel" }).state;
    expect(receivePreview(cancelled, { ...PREVIEW, requestId })).toBe(cancelled);
    expect(receivePreview(asked.state, { ...PREVIEW, requestId: "someone-else" })).toBe(asked.state);
    expect(receivePreview(asked.state, { ...PREVIEW, requestId, tab: 2 })).toBe(asked.state);
  });

  it("a preview made from a draft that has since changed is dropped, so its digest cannot be confirmed", () => {
    const asked = press(withDraft(), { kind: "send" });
    const shown = receivePreview(asked.state, { ...PREVIEW, requestId: (asked.effect as { requestId: string }).requestId });
    const changed = receiveDraft(shown, { requestId: null, tab: 1, draft: draftOf({ comments: [COMMENT] }) });
    expect(changed.prompt).toBeNull();
    expect(changed.status).toBe("the draft changed; s shows what would be sent now");
    expect(press(changed, { kind: "answer", answer: "yes" }).effect).toBeNull();
  });
});

describe("the draft", () => {
  it("comes from the review envelope and from review_draft, and from nothing else", () => {
    const draft = draftOf({ comments: [COMMENT], canUndo: true });
    expect(loaded({ draft }).draft).toEqual(draft);
    expect(loaded().draft, "an envelope with none draws an empty draft").toEqual(EMPTY_DRAFT);
    const next = receiveDraft(loaded(), { requestId: "r1", tab: 1, draft });
    expect(next.draft).toEqual(draft);
    expect(receiveDraft(loaded(), { requestId: null, tab: 2, draft })).toEqual(loaded());
  });

  it("is the footer line, counted, and nothing for an empty one", () => {
    expect(draftLine(EMPTY_DRAFT)).toBeNull();
    expect(draftLine(draftOf({ comments: [COMMENT, { ...COMMENT, id: 2 }], reverts: [REVERT] }))).toBe("draft: 2 comments, 1 revert · s sends them to the agent");
    expect(draftLine(draftOf({ comments: [COMMENT] }))).toBe("draft: 1 comment, 0 reverts · s sends them to the agent");
  });

  it("puts a comment under the hunk whose new lines hold its first line, in this turn only", () => {
    const state = opened(draftOf({ comments: [COMMENT, { ...COMMENT, id: 2, from: 41, to: 41 }, { ...COMMENT, id: 3, turn: 6 }, { ...COMMENT, id: 4, path: "b.md" }] }));
    expect(reviewStops(state).map(stopKey)).toEqual(["file:a.rs", "hunk:a.rs:0", "comment:a.rs:1", "hunk:a.rs:1", "comment:a.rs:2", "file:b.md", "group"]);
    const hunks = (state.diffs["a.rs"] as { diff: ReviewDiffEnvelope }).diff.hunks!;
    expect(commentsUnder(state, "a.rs", hunks[0]).map((c) => c.id)).toEqual([1]);
  });

  it("marks a hunk reverted by turn, path, hunk id and header, and says when it was undone", () => {
    const state = opened(draftOf({ reverts: [REVERT] }));
    const hunks = (state.diffs["a.rs"] as { diff: ReviewDiffEnvelope }).diff.hunks!;
    expect(revertMark(revertOf(state, "a.rs", hunks[1]))).toBe("reverted");
    expect(revertOf(state, "a.rs", hunks[0]), "another hunk").toBeNull();
    expect(revertOf(state, "a.rs", { ...hunks[1], header: "@@ -40,2 +45,2 @@" }), "the same id with another header is another hunk").toBeNull();
    expect(revertOf(state, "b.md", hunks[1])).toBeNull();
    expect(revertOf(opened(draftOf({ reverts: [{ ...REVERT, turn: 6 }] })), "a.rs", hunks[1]), "another turn").toBeNull();
    const undone = opened(draftOf({ reverts: [{ ...REVERT, undone: true }] }));
    expect(revertMark(revertOf(undone, "a.rs", hunks[1]))).toBe("reverted, undone");
    const again = opened(draftOf({ reverts: [{ ...REVERT, undone: true }, { ...REVERT, id: 2 }] }));
    expect(revertMark(revertOf(again, "a.rs", hunks[1])), "the newest revert of it wins").toBe("reverted");
    expect(revertMark(null)).toBeNull();
  });

  it("marks a whole-file revert on the file", () => {
    const state = loaded({ draft: draftOf({ reverts: [{ ...REVERT, hunk: null, header: null, what: "file", lines: null }] }) });
    expect(revertMark(revertOf(state, "a.rs", null))).toBe("reverted");
  });

  it("deleting a comment leaves the cursor where it was, not at the top", () => {
    const state = moveTo(opened(draftOf({ comments: [COMMENT, { ...COMMENT, id: 2, from: 41, to: 41 }] })), "comment:a.rs:1");
    const next = receiveDraft(state, { requestId: "r", tab: 1, draft: draftOf({ comments: [{ ...COMMENT, id: 2, from: 41, to: 41 }] }) });
    expect(next.cursor).toBe("hunk:a.rs:1");
  });
});

describe("recovery rows", () => {
  const ENTRIES = [
    { id: "e-1", path: "core/src/x.rs", at: 1790000000000 },
    { id: "e-2", path: "Cargo.lock", at: 1790000001000 },
  ];
  const withRecovery = (over: Partial<ReviewEnvelope> = {}) => receiveReview(openReview(1, "req-1", ENTRIES), envelope(over));

  it("are the first stops, ahead of the files, and named recovery", () => {
    expect(reviewStops(withRecovery()).map(stopKey)).toEqual(["recovery:e-1", "recovery:e-2", "file:a.rs", "file:b.md", "group"]);
  });

  it("are there while nothing was compared and before any overview has arrived", () => {
    expect(reviewStops(withRecovery({ compared: false, files: [] })).map(stopKey)).toEqual(["recovery:e-1", "recovery:e-2"]);
    expect(reviewStops(openReview(1, "req-1", ENTRIES)).map(stopKey)).toEqual(["recovery:e-1", "recovery:e-2"]);
  });

  it("Enter asks, in the words that say what is kept; y restores and n forgets", () => {
    const asked = press(withRecovery(), { kind: "toggle" });
    expect(asked.effect).toBeNull();
    expect(asked.state.prompt).toEqual({ kind: "recover", entry: "e-1", path: "core/src/x.rs" });
    expect(promptText(asked.state.prompt!)).toBe(
      "restore core/src/x.rs to its bytes from before the interrupted revert? the current bytes are kept in the review store. y restores · n forgets this",
    );
    expect(press(asked.state, { kind: "answer", answer: "yes" }).effect).toEqual({ kind: "recover", requestId: expect.any(String), entry: "e-1", answer: "restore" });
    expect(press(asked.state, { kind: "answer", answer: "no" }).effect).toEqual({ kind: "recover", requestId: expect.any(String), entry: "e-1", answer: "dismiss" });
    expect(press(asked.state, { kind: "answer", answer: "cancel" })).toMatchObject({ effect: null, state: { prompt: null } });
  });

  it("have no stops of the other kinds' keys: x, i and o do nothing on one", () => {
    const state = withRecovery();
    for (const kind of ["revert", "comment", "open", "copy"] as const) expect(press(state, { kind }).effect, kind).toBeNull();
    expect(press(state, { kind: "revert" }).state.prompt).toBeNull();
  });

  it("follow the shell's list, and a question about one that is gone goes with it", () => {
    const asked = press(withRecovery(), { kind: "toggle" }).state;
    expect(receiveRecovery(asked, ENTRIES).prompt).not.toBeNull();
    const one = receiveRecovery(asked, [ENTRIES[1]]);
    expect(one.prompt).toBeNull();
    expect(reviewStops(one).map(stopKey)[0]).toBe("recovery:e-2");
    expect(reviewStops(receiveRecovery(asked, [])).map(stopKey)).toEqual(["file:a.rs", "file:b.md", "group"]);
  });

  it("survive a new overview, which is about another turn", () => {
    const state = withRecovery();
    const moved = press(state, { kind: "turn", delta: -1 });
    expect(receiveReview(moved.state, envelope({ requestId: moved.state.requestId, current: 6 })).recovery).toEqual(ENTRIES);
  });
});

describe("stops are never called row", () => {
  it("every kind the overlay can show is one of file, hunk, comment, recovery, group", () => {
    const state = opened(draftOf({ comments: [COMMENT] }), {}, { files: [file("a.rs"), file("c.rs", { origin: "workspace" })] });
    const withAll = receiveRecovery({ ...state, groupOpen: true }, [{ id: "e-1", path: "p", at: 0 }]);
    const kinds = new Set(reviewStops(withAll).map((s) => s.kind));
    expect(kinds).toEqual(new Set(["recovery", "file", "hunk", "comment", "group"]));
    expect(kinds.has("row" as never)).toBe(false);
  });
});

describe("o with the turn named", () => {
  it("carries the turn and scope shown, in session scope the newest turn", () => {
    expect(press(loaded({ scope: "session" }), { kind: "open" }).effect).toEqual({ kind: "open", path: "a.rs", line: null, turn: 7, scope: "session" });
  });

  it("on a comment points at its first line", () => {
    const state = moveTo(opened(draftOf({ comments: [{ ...COMMENT, from: 11 }] })), "comment:a.rs:1");
    expect(press(state, { kind: "open" }).effect).toMatchObject({ path: "a.rs", line: 11 });
    expect(press(state, { kind: "copy" }).effect).toEqual({ kind: "copy", text: "a.rs:11" });
  });
});
