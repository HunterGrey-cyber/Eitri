import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { isModifierKey, RELEASE_WAIT_MAX_MS, TYPING_GUARD_MS, TypingGuard } from "./typingGuard";

/* Every keydown reaches `onKey` first. `a`/`d`/`D` ask `defer`; Enter on a card button asks
   `mayAnswerNow`, then waits in `defer` and requires a focus landing. These rows model only the guard;
   button landing is covered by the key-count tests below and `App.test.tsx`.
   `answers` records the answer and its time. Fake timers make test gaps match the time between keys. */

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

describe("TypingGuard: typed prose never answers", () => {
  it('"also add a test" at 60 wpm answers nothing (the first letter has no key before it)', () => {
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

  /* The shell withholds a key pressed with Super or Hyper, so for
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

  /* The walk exception once asked only about the key right before Enter, so a sentence
     ending in `l` walked onto Approve (the first control, `PermissionCard.tsx`'s `data-nav-order`)
     and its Enter approved. The expectation that "…al⏎" lands on Deny was false against the code. Now every
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

  /** Every deferred key shared one slot and the caller flashed the
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

/* "set up my" ran `<leader>m` and "the boy" `<leader>bo` (its `y`
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

/* Enter on a card's own button now
   answers the way `a`/`d` do -- deferred through `defer`, and only when `App.tsx` saw focus put on
   that very button by the key right before it. The guard itself decides no landing: it counts the
   keys it recorded, so `App.tsx` can tie the landing it saw to the key that made it. */
describe("keyCount", () => {
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

/* A BROWSE `y` opens a burst in which any other key is typing,
   not a command. `noteCopy` records the `y`; `afterCopy` asks about the key `onKey` just recorded. */
describe("TypingGuard: the burst after a copy", () => {
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

/* `i`/`o`/`A` enter INPUT on a pure pause, with NO motion
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

describe("TypingGuard.onRelease: a held key counts until it is let go", () => {
  it("an a right after letting go of a held j waits, however long ago its last repeat was", () => {
    const guard = new TypingGuard();
    guard.onKey("j", 0);
    guard.onKey("j", 30);
    guard.onRelease("j", 200);
    guard.onKey("a", 400);
    expect(guard.mayAnswerNow("a", 400, false)).toBe(false);
    guard.onKey("a", 700);
    expect(guard.mayAnswerNow("a", 700, false)).toBe(true);
  });

  it("changes nothing when another key came after the held one, or for an earlier time", () => {
    const guard = new TypingGuard();
    guard.onKey("j", 0);
    guard.onKey("x", 10);
    guard.onRelease("j", 500);
    guard.onKey("a", 300);
    expect(guard.mayAnswerNow("a", 300, false)).toBe(true);
    const other = new TypingGuard();
    other.onKey("j", 100);
    other.onRelease("j", 50);
    other.onKey("a", 360);
    expect(other.mayAnswerNow("a", 360, false)).toBe(true);
  });

  it("records no key of its own", () => {
    const guard = new TypingGuard();
    guard.onKey("j", 0);
    guard.onRelease("j", 100);
    expect(guard.keyCount()).toBe(1);
  });
});

describe("TypingGuard: an answer that waits for its key's release (deferUntilReleased)", () => {
  /** A press of `held` at the fake clock's current time, answered through `deferUntilReleased`. */
  function press(held: string, flash = "held") {
    const t = Date.now();
    guard.onKey(held, t);
    return guard.deferUntilReleased(t, false, () => answers.push({ key: held, at: Date.now() }), flash, held);
  }

  it("a tap let go inside the window answers when the window ends, as defer does", () => {
    const start = Date.now();
    expect(press("y")).toBe(true);
    vi.advanceTimersByTime(90);
    guard.onKeyUp("y", Date.now());
    vi.advanceTimersByTime(TYPING_GUARD_MS - 91);
    expect(answers).toEqual([]);
    vi.advanceTimersByTime(1);
    expect(answers).toEqual([{ key: "y", at: start + TYPING_GUARD_MS }]);
  });

  it("a key still down when the window ends answers nothing until it is let go", () => {
    const start = Date.now();
    press("y");
    // Past the window, still inside RELEASE_WAIT_MAX_MS.
    const held = TYPING_GUARD_MS + 100;
    vi.advanceTimersByTime(held);
    expect(answers).toEqual([]);
    expect(guard.onKeyUp("y", Date.now())).toBeNull();
    expect(answers).toEqual([{ key: "y", at: start + held }]);
  });

  it("a key down longer than RELEASE_WAIT_MAX_MS, with no repeat to cancel it, is refused at its release with the wait's flash", () => {
    press("y", "Ctrl+y approves only on its own");
    vi.advanceTimersByTime(RELEASE_WAIT_MAX_MS - 1);
    expect(answers).toEqual([]);
    vi.advanceTimersByTime(5000);
    expect(guard.onKeyUp("y", Date.now())).toBe("Ctrl+y approves only on its own");
    vi.advanceTimersByTime(TYPING_GUARD_MS * 4);
    expect(answers).toEqual([]);
    // Nothing is left waiting: the next press is judged afresh.
    vi.advanceTimersByTime(TYPING_GUARD_MS * 2);
    press("y");
    guard.onKeyUp("y", Date.now());
    vi.advanceTimersByTime(TYPING_GUARD_MS);
    expect(answers.length).toBe(1);
  });

  it("a held key is refused with its own held flash when the wait names one", () => {
    const t = Date.now();
    guard.onKey("y", t);
    guard.deferUntilReleased(t, false, () => answers.push({ key: "y", at: Date.now() }), "typing", "y", "let go sooner");
    vi.advanceTimersByTime(RELEASE_WAIT_MAX_MS + 1);
    expect(guard.onKeyUp("y", Date.now())).toBe("let go sooner");
    expect(answers).toEqual([]);
  });

  it("the hold is judged by the press's and the release's own times, not by how late the release is handled", () => {
    // A quick press whose keyup is handled only after a stall of the page's main thread.
    const t = Date.now();
    press("y");
    vi.advanceTimersByTime(RELEASE_WAIT_MAX_MS * 3);
    expect(answers).toEqual([]);
    expect(guard.onKeyUp("y", t + 100)).toBeNull();
    expect(answers).toEqual([{ key: "y", at: t + RELEASE_WAIT_MAX_MS * 3 }]);
    // And a long hold handled at once is refused, though no timer has run past the cap.
    vi.advanceTimersByTime(TYPING_GUARD_MS * 2);
    const u = Date.now();
    press("y", "held");
    vi.advanceTimersByTime(TYPING_GUARD_MS + 1);
    expect(guard.onKeyUp("y", u + RELEASE_WAIT_MAX_MS + 1)).toBe("held");
    expect(answers.length).toBe(1);
  });

  it("a key let go one millisecond inside RELEASE_WAIT_MAX_MS still answers at its release", () => {
    press("y");
    vi.advanceTimersByTime(RELEASE_WAIT_MAX_MS - 1);
    expect(guard.onKeyUp("y", Date.now())).toBeNull();
    expect(answers.length).toBe(1);
  });

  it("the cap is a release wait's alone: defer, which waits for no release, is unchanged", () => {
    const t = Date.now();
    guard.onKey("a", t);
    expect(guard.defer(t, false, () => answers.push({ key: "a", at: Date.now() }), "f")).toBe(true);
    vi.advanceTimersByTime(RELEASE_WAIT_MAX_MS * 2);
    expect(answers.length).toBe(1);
  });

  it("a held key's first repeat, before any release, cancels it and says the wait's own flash", () => {
    press("y", "Ctrl+y approves only on its own");
    vi.advanceTimersByTime(600);
    expect(guard.onKey("y", Date.now())).toBe("Ctrl+y approves only on its own");
    guard.onKeyUp("y", Date.now());
    vi.advanceTimersByTime(TYPING_GUARD_MS * 4);
    expect(answers).toEqual([]);
  });

  it("another key's release does not end the wait, and another key's press cancels it", () => {
    press("y");
    vi.advanceTimersByTime(TYPING_GUARD_MS * 2);
    guard.onKeyUp("Control", Date.now());
    expect(answers).toEqual([]);
    expect(guard.onKey("x", Date.now())).toBe("held");
    guard.onKeyUp("y", Date.now());
    expect(answers).toEqual([]);
  });

  it("cancel() drops a wait that is waiting for the release", () => {
    press("y");
    vi.advanceTimersByTime(TYPING_GUARD_MS * 2);
    expect(guard.cancel()).toBe(true);
    guard.onKeyUp("y", Date.now());
    expect(answers).toEqual([]);
  });

  it("a release seen before a later press of the same key is not that press's release", () => {
    press("y");
    guard.onKeyUp("y", Date.now());
    vi.advanceTimersByTime(TYPING_GUARD_MS * 2);
    expect(answers.length).toBe(1);
    press("y");
    vi.advanceTimersByTime(TYPING_GUARD_MS + 100);
    expect(answers.length).toBe(1);
    guard.onKeyUp("y", Date.now());
    expect(answers.length).toBe(2);
  });

  it("refused like defer: a key inside the window before it, or a repeat", () => {
    const t = Date.now();
    guard.onKey("u", t);
    guard.onKey("y", t + 100);
    expect(guard.deferUntilReleased(t + 100, false, () => answers.push({ key: "y", at: 0 }), "f", "y")).toBe(false);
    guard.onKey("y", t + 1000);
    expect(guard.deferUntilReleased(t + 1000, true, () => answers.push({ key: "y", at: 0 }), "f", "y")).toBe(false);
  });
});

describe("TypingGuard.timeOf: when a keydown happened", () => {
  it("a fresh press is its own stamp", () => {
    expect(guard.timeOf({ repeat: false, timeStamp: 1000 }, 50)).toBe(1000);
    expect(guard.timeOf({ repeat: false, timeStamp: 0 }, 70)).toBe(70);
  });

  it("a repeat stamped with the press's time, as GDK's are, is dated by the time since that press", () => {
    guard.timeOf({ repeat: false, timeStamp: 1000 }, 50);
    expect(guard.timeOf({ repeat: true, timeStamp: 1000 }, 650)).toBe(1600);
    expect(guard.timeOf({ repeat: true, timeStamp: 1000 }, 1050)).toBe(2000);
  });

  it("a repeat with a later stamp of its own keeps it", () => {
    guard.timeOf({ repeat: false, timeStamp: 1000 }, 50);
    expect(guard.timeOf({ repeat: true, timeStamp: 1700 }, 650)).toBe(1700);
  });

  it("a held motion key's first repeat, dated by timeOf, starts a new run, so the leader right after it may act", () => {
    // Prose `x`, then `j` at once: one run that is not all motion keys.
    guard.onKey("x", guard.timeOf({ repeat: false, timeStamp: 1000 }, 1000));
    guard.onKey("j", guard.timeOf({ repeat: false, timeStamp: 1100 }, 1100));
    // `j` held: GDK stamps its first repeat with the press's time; it comes after the repeat delay.
    const repeatAt = guard.timeOf({ repeat: true, timeStamp: 1100 }, 1600);
    expect(repeatAt).toBe(1600);
    guard.onKey("j", repeatAt);
    // More than the window after the press, the repeat does not continue the `x j` run: it begins a
    // run of motion keys, and a Space right after it is a quick motion's leader, not prose.
    const space = guard.timeOf({ repeat: false, timeStamp: 1650 }, 1650);
    guard.onKey(" ", space);
    expect(guard.mayActAfterMotion(space, false)).toBe(true);

    // Dated by the stamp alone, the same repeat continued the run that began with `x`.
    const stamps = new TypingGuard();
    stamps.onKey("x", 1000);
    stamps.onKey("j", 1100);
    stamps.onKey("j", 1100);
    stamps.onKey(" ", 1150);
    expect(stamps.mayActAfterMotion(1150, false)).toBe(false);
  });
});

/* The host revealed a tab here (`arrive`, `focus_permission`) while the person may still be typing
   wherever they asked for it: the arrival is a key, at the moment it came, for every rule above. */
describe("TypingGuard: an arrival counts as a key", () => {
  /** The keys and a tab arrive now, on the fake clock the keys are dated by. */
  function arrive() {
    guard.noteArrival(Date.now());
  }
  const enterOnButton = () => key("Enter", { onButton: true });
  /** INPUT's `Ctrl+y`: the guard's before-half, then a wait for the window and the release. */
  function ctrlY() {
    const t = Date.now();
    guard.onKey("y", t);
    if (!guard.mayAnswerNow("", t, false)) return;
    guard.deferUntilReleased(t, false, () => answers.push({ key: "Ctrl+y", at: Date.now() }), "cancelled", "KeyY");
    guard.onKeyUp("KeyY", t + 50);
  }
  /** The leader: acts at once when the guard lets it. */
  function leader() {
    const t = Date.now();
    guard.onKey(" ", t);
    if (guard.mayActAfterMotion(t, false)) answers.push({ key: "leader", at: t });
  }
  function startInput() {
    const t = Date.now();
    guard.onKey("i", t);
    if (guard.mayStartInput(t, false)) answers.push({ key: "i", at: t });
  }

  it("an_answer_key_within_250ms_of_an_arrival_is_refused", () => {
    const cases: Array<[string, () => void]> = [
      ["a", () => key("a")],
      ["Enter on a card button", enterOnButton],
      ["Ctrl+y", ctrlY],
      ["the leader", leader],
      ["i", startInput],
    ];
    for (const [name, press] of cases) {
      guard = new TypingGuard();
      answers = [];
      key("k");
      vi.advanceTimersByTime(TYPING_GUARD_MS * 4);
      arrive();
      vi.advanceTimersByTime(TYPING_GUARD_MS - 1);
      press();
      vi.advanceTimersByTime(TYPING_GUARD_MS * 4);
      expect(answers, name).toEqual([]);
    }
  });

  it("an_answer_key_after_250ms_is_taken", () => {
    const cases: Array<[string, () => void]> = [
      ["a", () => key("a")],
      ["Enter on a card button", enterOnButton],
      ["Ctrl+y", ctrlY],
      ["the leader", leader],
      ["i", startInput],
    ];
    for (const [name, press] of cases) {
      guard = new TypingGuard();
      answers = [];
      arrive();
      vi.advanceTimersByTime(TYPING_GUARD_MS);
      press();
      vi.advanceTimersByTime(TYPING_GUARD_MS * 4);
      expect(answers.length, name).toBe(1);
    }
  });

  it("a_refused_key_extends_the_run", () => {
    arrive();
    vi.advanceTimersByTime(200);
    key("a");
    vi.advanceTimersByTime(200);
    key("a");
    vi.advanceTimersByTime(200);
    enterOnButton();
    vi.advanceTimersByTime(TYPING_GUARD_MS * 4);
    expect(answers).toEqual([]);
    key("a");
    vi.advanceTimersByTime(TYPING_GUARD_MS);
    expect(answers.map((a) => a.key)).toEqual(["a"]);
  });

  it("a walk or a motion run that starts inside the arrival's window is no exception", () => {
    arrive();
    vi.advanceTimersByTime(100);
    key("l");
    vi.advanceTimersByTime(100);
    enterOnButton();
    vi.advanceTimersByTime(TYPING_GUARD_MS * 4);
    arrive();
    vi.advanceTimersByTime(100);
    key("j");
    vi.advanceTimersByTime(100);
    leader();
    expect(answers).toEqual([]);
  });

  it("an arrival cancels a waiting a, as another key would", () => {
    key("a");
    vi.advanceTimersByTime(100);
    arrive();
    vi.advanceTimersByTime(TYPING_GUARD_MS * 4);
    expect(answers).toEqual([]);
  });

  it("an arrival is not a key the page saw: keyCount does not move", () => {
    key("k");
    const before = guard.keyCount();
    arrive();
    expect(guard.keyCount()).toBe(before);
  });

  it("a held j let go after an arrival does not move the arrival", () => {
    key("j");
    vi.advanceTimersByTime(TYPING_GUARD_MS * 4);
    arrive();
    vi.advanceTimersByTime(100);
    guard.onRelease("j", Date.now());
    vi.advanceTimersByTime(TYPING_GUARD_MS - 100);
    key("a");
    vi.advanceTimersByTime(TYPING_GUARD_MS);
    expect(answers.map((a) => a.key)).toEqual(["a"]);
  });

  it("an arrival never comes before the last key recorded", () => {
    key("x");
    guard.noteArrival(Date.now() - 1000);
    vi.advanceTimersByTime(TYPING_GUARD_MS - 1);
    key("a");
    vi.advanceTimersByTime(TYPING_GUARD_MS * 4);
    expect(answers).toEqual([]);
  });
});

describe("TypingGuard.clockNow: the present on the keydowns' clock", () => {
  it("with no key seen yet, it is performance.now()'s", () => {
    expect(guard.clockNow(42)).toBe(42);
  });

  it("it follows the keys' clock by the least lag seen, so a key handled late does not pull it back", () => {
    // Keys stamped on a clock 4900 ms ahead of performance.now()'s, the first handled at once.
    guard.timeOf({ repeat: false, timeStamp: 5000 }, 100);
    expect(guard.clockNow(300)).toBe(5200);
    // A key handled 400 ms after its press, in a stall: the present is still read from the first.
    guard.timeOf({ repeat: false, timeStamp: 6000 }, 1500);
    expect(guard.clockNow(1600)).toBe(6500);
    // A repeat's stamp is the press's own, so it says nothing about the offset.
    guard.timeOf({ repeat: true, timeStamp: 6000 }, 1700);
    expect(guard.clockNow(1800)).toBe(6700);
  });

  it("on performance.now()'s own clock a key handled late does not date an arrival before its moment", () => {
    // The page's first key is pressed at 100 and handled at 600, in a stall; the clocks are one.
    guard.onKey("k", guard.timeOf({ repeat: false, timeStamp: 100 }, 600));
    guard.noteArrival(guard.clockNow(1000));
    // 100 ms after the arrival, handled at once.
    const t = guard.timeOf({ repeat: false, timeStamp: 1100 }, 1100);
    guard.onKey("a", t);
    expect(guard.mayAnswerNow("a", t, false)).toBe(false);
    const alone = guard.timeOf({ repeat: false, timeStamp: 1600 }, 1600);
    guard.onKey("a", alone);
    expect(guard.mayAnswerNow("a", alone, false)).toBe(true);
  });

  it("a stamp never ahead of its handling reads as performance.now()'s clock, however late the keys were handled", () => {
    guard.timeOf({ repeat: false, timeStamp: 100 }, 600);
    expect(guard.clockNow(1000)).toBe(1000);
  });

  it("an arrival dated by it refuses a key pressed just after on the keys' clock", () => {
    guard.onKey("k", guard.timeOf({ repeat: false, timeStamp: 5000 }, 100));
    guard.noteArrival(guard.clockNow(2000));
    const t = guard.timeOf({ repeat: false, timeStamp: 7000 }, 2100);
    guard.onKey("a", t);
    expect(guard.mayAnswerNow("a", t, false)).toBe(false);
    const later = guard.timeOf({ repeat: false, timeStamp: 7200 }, 2300);
    guard.onKey("a", later);
    expect(guard.mayAnswerNow("a", later, false)).toBe(false);
    const alone = guard.timeOf({ repeat: false, timeStamp: 7500 }, 2600);
    guard.onKey("a", alone);
    expect(guard.mayAnswerNow("a", alone, false)).toBe(true);
  });
});
