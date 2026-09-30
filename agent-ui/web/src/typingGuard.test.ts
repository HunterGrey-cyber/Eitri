import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { isModifierKey, TYPING_GUARD_MS, TypingGuard } from "./typingGuard";

/* Spec §2.1 (S1), row by row. Each test drives the guard the way `App.tsx`'s `onKeyDown` does: every
   keydown goes through `onKey` first; `a`/`d`/`D` then ask `defer`, and Enter on a card button asks
   `mayAnswerNow` (since K02 it then also waits in `defer` and needs a landing, which these rows do
   not model: the `K02` describe below and `App.test.tsx` do). `answers` records what would have
   been answered, and when. Time is vitest's fake clock, so a gap in a test is exactly the gap
   between two keys. */

let guard: TypingGuard;
let answers: Array<{ key: string; at: number }>;

beforeEach(() => {
  vi.useFakeTimers();
  guard = new TypingGuard();
  answers = [];
});
afterEach(() => {
  vi.useRealTimers();
});

const ANSWER_KEYS = new Set(["a", "d", "D"]);

/** One keydown at the fake clock's current time, handled as the panel handles it. */
function key(k: string, options: { repeat?: boolean; onButton?: boolean } = {}) {
  const t = Date.now();
  guard.onKey(k, t);
  if (ANSWER_KEYS.has(k)) {
    guard.defer(t, options.repeat ?? false, () => answers.push({ key: k, at: Date.now() }), "cancelled");
  } else if (k === "Enter" && options.onButton === true) {
    if (guard.mayAnswerNow("Enter", t, options.repeat ?? false)) answers.push({ key: k, at: t });
  }
}

/** `text` typed a character at a time, `gap` ms apart (prose at 60 wpm is ~200 ms a key), then the
 *  clock runs on long enough for any deferred answer to fire. */
function type(text: string, gap: number, options: { enterOnButton?: boolean } = {}) {
  for (const ch of text) {
    key(ch === "\n" ? "Enter" : ch, { onButton: ch === "\n" && options.enterOnButton === true });
    vi.advanceTimersByTime(gap);
  }
  vi.advanceTimersByTime(TYPING_GUARD_MS * 4);
}

describe("TypingGuard: a lone a/d/D waits, then answers", () => {
  it("a lone a answers after TYPING_GUARD_MS, not before", () => {
    const start = Date.now();
    key("a");
    vi.advanceTimersByTime(TYPING_GUARD_MS - 1);
    expect(answers).toEqual([]);
    vi.advanceTimersByTime(1);
    expect(answers).toEqual([{ key: "a", at: start + TYPING_GUARD_MS }]);
  });

  it("d and D wait the same way", () => {
    key("d");
    vi.advanceTimersByTime(TYPING_GUARD_MS);
    vi.advanceTimersByTime(TYPING_GUARD_MS * 2);
    key("D");
    vi.advanceTimersByTime(TYPING_GUARD_MS);
    expect(answers.map((a) => a.key)).toEqual(["d", "D"]);
  });

  it("a key long enough before a does not stop it (a gap of exactly TYPING_GUARD_MS counts as alone)", () => {
    key("j");
    vi.advanceTimersByTime(TYPING_GUARD_MS);
    key("a");
    vi.advanceTimersByTime(TYPING_GUARD_MS);
    expect(answers.map((a) => a.key)).toEqual(["a"]);
  });

  it("a bare modifier is not a key: Shift before D, and Shift during the wait", () => {
    key("Shift");
    key("D");
    vi.advanceTimersByTime(100);
    key("Shift");
    vi.advanceTimersByTime(TYPING_GUARD_MS);
    expect(answers.map((a) => a.key)).toEqual(["D"]);
  });
});

describe("TypingGuard: typed prose never answers (spec §2.1)", () => {
  it('"also add a test" at 60 wpm answers nothing (F1: the first letter has no key before it)', () => {
    type("also add a test", 200);
    expect(answers).toEqual([]);
  });

  it('"also" at 80 ms a key answers nothing', () => {
    type("also", 80);
    expect(answers).toEqual([]);
  });

  it('"please add" answers nothing: the a in the middle and the one after a space each have a key before them', () => {
    type("please add", 200);
    expect(answers).toEqual([]);
  });

  it('"fix the data" and its Enter on a card button answer nothing', () => {
    type("fix the data\n", 200, { enterOnButton: true });
    expect(answers).toEqual([]);
  });

  it("a then j at 200 ms: the j cancels the waiting a", () => {
    key("a");
    vi.advanceTimersByTime(200);
    expect(guard.onKey("j", Date.now())).toBe("cancelled");
    vi.advanceTimersByTime(TYPING_GUARD_MS * 4);
    expect(answers).toEqual([]);
  });

  /* Codex re-review (2026-09-28): the shell withholds a key pressed with Super or Hyper, so for
     `a` then `Super+x` the page sees only Super's own keydown -- which must cancel as the x would. */
  it("a then Super at 100 ms: Super's own keydown cancels the waiting a (the chord after it never arrives)", () => {
    key("a");
    vi.advanceTimersByTime(100);
    expect(guard.onKey("Super", Date.now())).toBe("cancelled");
    vi.advanceTimersByTime(TYPING_GUARD_MS * 4);
    expect(answers).toEqual([]);
  });

  it("a key 1 ms before the wait ends still cancels it", () => {
    key("a");
    vi.advanceTimersByTime(TYPING_GUARD_MS - 1);
    key("x");
    vi.advanceTimersByTime(TYPING_GUARD_MS * 4);
    expect(answers).toEqual([]);
  });

  it("a held key's repeat never answers, even with nothing before it", () => {
    key("a", { repeat: true });
    vi.advanceTimersByTime(TYPING_GUARD_MS * 4);
    expect(answers).toEqual([]);
    expect(guard.mayAnswerNow("Enter", Date.now(), true)).toBe(false);
  });

  it("defer says whether it waits: false when a key came inside the window before", () => {
    const t = Date.now();
    guard.onKey("e", t);
    guard.onKey("a", t + 100);
    expect(guard.defer(t + 100, false, () => answers.push({ key: "a", at: 0 }), "cancelled")).toBe(false);
    guard.onKey("a", t + 1000);
    expect(guard.defer(t + 1000, false, () => answers.push({ key: "a", at: 0 }), "cancelled")).toBe(true);
  });
});

describe("TypingGuard: Enter on a card button", () => {
  it("Enter with nothing before it answers at once", () => {
    key("Enter", { onButton: true });
    expect(answers.map((a) => a.key)).toEqual(["Enter"]);
  });

  it("l then Enter at 100 ms answers: l is the walk onto Approve", () => {
    key("l");
    vi.advanceTimersByTime(100);
    key("Enter", { onButton: true });
    expect(answers.map((a) => a.key)).toEqual(["Enter"]);
  });

  it("h, Tab and Shift+Tab (key Tab) are walks too", () => {
    for (const walk of ["h", "Tab"]) {
      vi.advanceTimersByTime(TYPING_GUARD_MS * 4);
      answers = [];
      key(walk);
      vi.advanceTimersByTime(50);
      key("Enter", { onButton: true });
      expect(answers.map((a) => a.key), walk).toEqual(["Enter"]);
    }
  });

  it("e then Enter at 100 ms does not answer: a sentence ending on a card button", () => {
    key("e");
    vi.advanceTimersByTime(100);
    key("Enter", { onButton: true });
    expect(answers).toEqual([]);
  });

  it("the walk exception is only the key right before Enter: l, then e, then Enter", () => {
    key("l");
    vi.advanceTimersByTime(50);
    key("e");
    vi.advanceTimersByTime(50);
    key("Enter", { onButton: true });
    expect(answers).toEqual([]);
  });

  /* Fix round 1: the walk exception once asked only about the key right before Enter, so a sentence
     ending in `l` walked onto Approve (the first control, `PermissionCard.tsx`'s `data-nav-order`)
     and its Enter approved. The spec's "…al⏎ lands on Deny" was false against the code. Now every
     key of the unbroken run before Enter must be a walk. */
  it('"cancel" then Enter at 80 ms a key does not answer: the l walked onto Approve, but the run began with typing', () => {
    type("cancel\n", 80, { enterOnButton: true });
    expect(answers).toEqual([]);
  });

  it('"al" then Enter (a sentence ending "…al⏎" right after an arrival) does not answer', () => {
    type("al\n", 80, { enterOnButton: true });
    expect(answers).toEqual([]);
    answers = [];
    type("deal\n", 80, { enterOnButton: true });
    expect(answers).toEqual([]);
  });

  it("l l then Enter, each inside the window, still answers: a run of walks back to a key that stood alone", () => {
    type("ll\n", 80, { enterOnButton: true });
    expect(answers.map((a) => a.key)).toEqual(["Enter"]);
    answers = [];
    vi.advanceTimersByTime(TYPING_GUARD_MS * 4);
    for (const k of ["l", "Tab", "h", "l"]) {
      key(k);
      vi.advanceTimersByTime(60);
    }
    key("Enter", { onButton: true });
    expect(answers.map((a) => a.key)).toEqual(["Enter"]);
  });

  it("a key that stood alone ends the run: e, a pause, then l and Enter fast, answers", () => {
    key("e");
    vi.advanceTimersByTime(TYPING_GUARD_MS);
    key("l");
    vi.advanceTimersByTime(100);
    key("Enter", { onButton: true });
    expect(answers.map((a) => a.key)).toEqual(["Enter"]);
  });

  it("the stated residual: prose paused for the window right before a final l, then Enter at once, answers", () => {
    type("cance", 80);
    key("l");
    vi.advanceTimersByTime(80);
    key("Enter", { onButton: true });
    expect(answers.map((a) => a.key)).toEqual(["Enter"]);
  });

  it("j then l then Enter, all fast, does not answer: only h/l/Tab walk onto a button", () => {
    type("jl\n", 80, { enterOnButton: true });
    expect(answers).toEqual([]);
  });

  it("a then Enter: the Enter cancels the waiting a and is itself refused", () => {
    key("a");
    vi.advanceTimersByTime(100);
    key("Enter", { onButton: true });
    vi.advanceTimersByTime(TYPING_GUARD_MS * 4);
    expect(answers).toEqual([]);
  });
});

describe("TypingGuard: cancel", () => {
  it("cancel drops a waiting answer and says whether there was one", () => {
    key("a");
    expect(guard.cancel()).toBe(true);
    expect(guard.cancel()).toBe(false);
    vi.advanceTimersByTime(TYPING_GUARD_MS * 4);
    expect(answers).toEqual([]);
  });

  it("onKey says null when nothing was waiting", () => {
    expect(guard.onKey("j", Date.now())).toBeNull();
  });

  /** The whole-branch review: every deferred key shared one slot and the caller flashed the
   *  `a`/`d` text for all of them. The flash that comes back is the cancelled wait's own. */
  it("onKey hands back the flash the cancelled wait named, not another's", () => {
    guard.onKey("f", 1000);
    expect(guard.defer(1000, false, () => {}, "f starts HINT only on its own")).toBe(true);
    expect(guard.onKey("i", 1080)).toBe("f starts HINT only on its own");
    guard.onKey("L", 5000);
    expect(guard.defer(5000, false, () => {}, "L (next tab) runs only on its own")).toBe(true);
    expect(guard.onKey("o", 5080)).toBe("L (next tab) runs only on its own");
    expect(guard.onKey("o", 5160)).toBeNull();
  });

  it("an injected clock is used instead of the global timers", () => {
    vi.useRealTimers();
    const scheduled: Array<() => void> = [];
    const manual = new TypingGuard({
      setTimeout: (run) => {
        scheduled.push(run);
        return scheduled.length;
      },
      clearTimeout: (handle) => {
        scheduled[(handle as number) - 1] = () => {};
      },
    });
    let ran = 0;
    manual.onKey("a", 1000);
    manual.defer(1000, false, () => ran++, "cancelled");
    expect(ran).toBe(0);
    scheduled[0]();
    expect(ran).toBe(1);
  });
});

describe("isModifierKey", () => {
  it("names the bare modifiers and nothing else", () => {
    for (const k of ["Shift", "Control", "Alt", "Meta", "AltGraph", "CapsLock"]) expect(isModifierKey(k), k).toBe(true);
    for (const k of ["a", "Enter", " ", "Tab", "Escape", "Unidentified"]) expect(isModifierKey(k), k).toBe(false);
  });
});

/* The v1-ui GUI pass (2026-09-27): "set up my" ran `<leader>m` and "the boy" `<leader>bo` (its `y`
   then answered the y/n), and "just do it" reached Stop with its `j` and interrupted the turn with
   its first Space. The leader and Enter/Space on Stop now ask `mayActAfterMotion`. */
describe("TypingGuard.mayActAfterMotion: the leader and Stop", () => {
  /** `text` a key at a time, `gap` ms apart; returns whether the LAST key may act. */
  function lastMayAct(text: string, gap: number, repeat = false): boolean {
    const keys = [...text].map((ch) => (ch === "\n" ? "Enter" : ch));
    let may = false;
    keys.forEach((k, i) => {
      if (i > 0) vi.advanceTimersByTime(gap);
      const t = Date.now();
      guard.onKey(k, t);
      may = guard.mayActAfterMotion(t, i === keys.length - 1 && repeat);
    });
    return may;
  }

  it("a key standing alone may act (a leader pressed on its own)", () => {
    expect(lastMayAct(" ", 0)).toBe(true);
  });

  it.each([["set up "], ["the "], ["just "], ["at "]])("the Space in %j typed at 180 ms a key may not", (text) => {
    expect(lastMayAct(text, 180)).toBe(false);
  });

  it("a key TYPING_GUARD_MS after the one before it stands alone again", () => {
    expect(lastMayAct("set ", TYPING_GUARD_MS)).toBe(true);
  });

  it.each([["jj "], ["j\n"], ["gg "], ["G "], ["kkj "]])("%j at 80 ms a key: a quick motion, so its last key may act", (text) => {
    expect(lastMayAct(text, 80)).toBe(true);
  });

  it("a motion run that began inside a word does not count", () => {
    vi.advanceTimersByTime(1000);
    expect(lastMayAct("ej ", 80)).toBe(false);
  });

  it("a held key's repeat never may", () => {
    expect(lastMayAct(" ", 0, true)).toBe(false);
  });

  it("leaves a card's Enter exactly as it was: j then Enter at 80 ms is still refused there", () => {
    vi.advanceTimersByTime(1000);
    guard.onKey("j", Date.now());
    vi.advanceTimersByTime(80);
    guard.onKey("Enter", Date.now());
    expect(guard.mayAnswerNow("Enter", Date.now(), false)).toBe(false);
  });
});

/* K02 (kbux 2026-09-29: `:ls⏎` approved `rm -rf important`): Enter on a card's own button now
   answers the way `a`/`d` do -- deferred through `defer`, and only when `App.tsx` saw focus put on
   that very button by the key right before it. The guard itself decides no landing: it counts the
   keys it recorded, so `App.tsx` can tie the landing it saw to the key that made it. */
describe("K02: keyCount", () => {
  it("counts every recorded key, a cancelling Super included, and no bare Shift", () => {
    const g = new TypingGuard();
    g.onKey("l", 0);
    g.onKey("Shift", 10);
    expect(g.keyCount()).toBe(1);
    g.onKey("Enter", 400);
    g.onKey("Super", 500);
    expect(g.keyCount()).toBe(3);
  });

  it("defer takes Enter's walk exception only when told the key", () => {
    let ran = 0;
    guard.onKey("l", 0);
    guard.onKey("Enter", 100);
    // `a`/`d`'s own call, with no key named: `l` 100 ms before is typing.
    expect(guard.defer(100, false, () => ran++, "x")).toBe(false);
    // Enter's: `l` is the walk onto the button, so it waits and then runs once.
    expect(guard.defer(100, false, () => ran++, "x", "Enter")).toBe(true);
    vi.advanceTimersByTime(TYPING_GUARD_MS - 1);
    expect(ran).toBe(0);
    vi.advanceTimersByTime(1);
    expect(ran).toBe(1);
    vi.advanceTimersByTime(TYPING_GUARD_MS * 4);
    expect(ran).toBe(1);
  });
});

/* Owner decision #26 (K12, K13, K15): a BROWSE `y` opens a burst in which any other key is typing,
   not a command. `noteCopy` records the `y`; `afterCopy` asks about the key `onKey` just recorded. */
describe("TypingGuard: the burst after a copy (#26, K15)", () => {
  it("is false before any copy", () => {
    expect(guard.afterCopy(Date.now())).toBe(false);
  });

  it("is true for a key inside the window after the copy, false from the window on", () => {
    guard.noteCopy(Date.now());
    vi.advanceTimersByTime(TYPING_GUARD_MS - 1);
    expect(guard.afterCopy(Date.now())).toBe(true);
    vi.advanceTimersByTime(1);
    expect(guard.afterCopy(Date.now())).toBe(false);
  });

  it("a later copy restarts the burst", () => {
    guard.noteCopy(Date.now());
    vi.advanceTimersByTime(TYPING_GUARD_MS + 50);
    guard.noteCopy(Date.now());
    vi.advanceTimersByTime(100);
    expect(guard.afterCopy(Date.now())).toBe(true);
  });

  it("never reads a key stamped before the copy as inside its burst", () => {
    guard.noteCopy(1000);
    expect(guard.afterCopy(900)).toBe(false);
  });
});

/* Fix round (Codex + Claude review, #26 finding 1): `i`/`o`/`A` enter INPUT on a pure pause, with NO motion
   exception -- a lone `l`, `h` or `G` is a word's first letter as much as a motion, so "look at" walked
   with `l` and opened INPUT with `o`. */
describe("TypingGuard.mayStartInput: i/o/A need a pause, never a motion run", () => {
  function lastMayStart(text: string, gap: number, repeat = false): boolean {
    const keys = [...text];
    let may = false;
    keys.forEach((k, i) => {
      if (i > 0) vi.advanceTimersByTime(gap);
      const t = Date.now();
      guard.onKey(k, t);
      may = guard.mayStartInput(t, i === keys.length - 1 && repeat);
    });
    return may;
  }

  it("a key standing alone may", () => {
    expect(lastMayStart("i", 0)).toBe(true);
  });

  it("a key TYPING_GUARD_MS after the one before it stands alone again", () => {
    expect(lastMayStart("ji", TYPING_GUARD_MS)).toBe(true);
  });

  it.each([["lo"], ["ho"], ["Go"], ["hi"], ["li"], ["ji"], ["jji"], ["ggo"], ["kkA"]])(
    "%j at 80 ms a key: a motion run is no exception, so the last key may not",
    (text) => {
      expect(lastMayStart(text, 80)).toBe(false);
    },
  );

  it("a held key's repeat never may", () => {
    expect(lastMayStart("i", 0, true)).toBe(false);
  });
});
