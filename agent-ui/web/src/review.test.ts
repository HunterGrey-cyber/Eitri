// @vitest-environment jsdom
import { describe, expect, it } from "vitest";
import { REVIEW_KEYS, resolveKey } from "./keymap";
import type { KeyLike } from "./keymap";
import {
  applyReviewKey,
  boxUnderCursor,
  cursorStop,
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
import type { ReviewAction, ReviewState } from "./review";
import type { ReviewDiffEnvelope, ReviewEnvelope, ReviewFile, ReviewTurn } from "./types";

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

  it("x, u, i and s are reserved: bound to nothing and not listed", () => {
    for (const k of ["x", "u", "i", "s"]) {
      expect(resolveReviewKey(key(k), false), k).toBeNull();
      expect(REVIEW_KEYS.flatMap((row) => row.keys.split(" / ")), k).not.toContain(k);
    }
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
    expect(press(state, { kind: "open" }).effect).toEqual({ kind: "open", path: "a.rs", line: 41 });
    expect(press(state, { kind: "copy" }).effect).toEqual({ kind: "copy", text: "a.rs:41" });
  });

  it("o on a file with its patch loaded opens its first hunk; with none loaded, the file alone", () => {
    expect(press(withDiff(), { kind: "open" }).effect).toEqual({ kind: "open", path: "a.rs", line: 10 });
    expect(press(loaded(), { kind: "open" }).effect).toEqual({ kind: "open", path: "a.rs", line: null });
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
