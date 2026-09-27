/**
 * S1 (spec `docs/superpowers/specs/2026-09-27-v1-ui-design.md` §2.1): a permission card is answered
 * only by a key that stands alone. **neovibe's own rule** -- no tool this panel copies has an exact
 * precedent. Claude Code answers a prompt only by an explicit choice (Enter on the highlighted option,
 * or a digit); vim tells a sequence from a lone key by time (`:h 'timeoutlen'`). This keeps R04's
 * `a`/`d` and still makes "arrive, then type" harmless: arrivals land BROWSE (R02) with the cursor on
 * a waiting card (R32), and a Claude Code user types at once (F1, F4), so the first letters of a
 * sentence arrive as BROWSE keys.
 *
 * - `a`/`d`/`D` answer only with no other key within `TYPING_GUARD_MS` before them **and** none
 *   within it after: they are deferred, and any keydown before the wait ends cancels them. Both
 *   directions, because the first letter of a sentence has no key before it ("also add a test") and
 *   a letter inside or at the end of one has ("please add", "fix the data").
 * - Enter on a focused card button answers only with no key within the window before it, unless
 *   every key of the unbroken run before it (each within the window of the next) was `h`, `l`, Tab
 *   or Shift+Tab -- a walk onto Approve/Deny/Always that began with a key standing alone. **This
 *   narrows spec §2.1**, which let Enter through whenever the ONE key before it was a walk, and said
 *   the residual ("…al⏎" right after an arrival onto a card) "lands on Deny, the safe side"
 *   (§14.2). That was false: `l` from the card's row walks to its first control, which is Approve
 *   (`PermissionCard.tsx`'s `data-nav-order`), so "cancel⏎" or "deal⏎" typed at once after an
 *   arrival approved the card. What is left, stated rather than hidden: a walk run that itself
 *   began with a key standing alone -- a message of nothing but walk keys ("l⏎" approves, "ll⏎"
 *   denies), or prose paused for `TYPING_GUARD_MS` or more right before a final `l` whose Enter
 *   follows at once ("cance", a pause, "l⏎" approves, as it did under the spec's rule).
 * - A held key's repeat (`event.repeat`) never answers.
 * - **The v1-ui GUI pass (2026-09-27) found two more ways typed prose acts after an arrival**, and
 *   `mayActAfterMotion` closes both. The leader (Space by default) starts a sequence wherever the
 *   row cursor has the keys, so "set up my" ran `<leader>m` (the mode flipped to bypass, which on a
 *   switch-capable sidecar also approves every waiting card, wave 5's W4) and "the boy" ran
 *   `<leader>bo` and answered its own y/n with the `y`. And `j` from the last row lands on the
 *   activity line's Stop, so "just do it" interrupted the turn with its first Space (the card was
 *   denied with it). So the leader and Enter/Space on Stop act only on a key that stands alone, or
 *   at the end of an unbroken run of motion keys (`j`/`k`/`h`/`l`/`g`/`G`/Tab) that itself began
 *   with one: `jj<Space>bd` and `j`⏎ onto Stop stay one quick motion, a word does not.
 *
 * Pure: it never reads the DOM. Timestamps are the caller's (`event.timeStamp`); the timers are
 * injectable and default to the global ones, looked up at call time so a test's fake timers apply.
 */

/** The guard window, both sides (spec §2.1, §14.1). Typed prose at 60 wpm is ~200 ms a key; a
 *  deliberate single `a` waits well over this for the next key. A tunable, not a key (§13). */
export const TYPING_GUARD_MS = 250;

/** The keys that walk onto a card button, so Enter at the end of a run made only of them is not
 *  typing. Shift+Tab arrives as `Tab` (the caller normalizes WebKitGTK's `Unidentified`/
 *  `code: "Tab"` shape to it). */
const WALK_KEYS = new Set(["h", "l", "Tab"]);

/** The keys that move the cursor without typing anything (`keymap.ts`'s BROWSE motions, the empty
 *  tab's `j`/`k`, and Tab), so the leader or Enter/Space on Stop at the end of a run made only of
 *  them is a quick motion, not prose (`mayActAfterMotion`). Wider than `WALK_KEYS` on purpose: that
 *  set stays the whole-branch review's narrowing for a card's Approve/Deny, which is untouched. */
const MOTION_KEYS = new Set(["h", "j", "k", "l", "g", "G", "Tab", "ArrowUp", "ArrowDown"]);

/** Bare modifiers are not "a key" (spec §2.1: "anything but a bare modifier"). `key` values from the
 *  UI Events KeyboardEvent key list's modifier section. */
const MODIFIER_KEYS = new Set([
  "Alt",
  "AltGraph",
  "CapsLock",
  "Control",
  "Fn",
  "FnLock",
  "Hyper",
  "Meta",
  "NumLock",
  "OS",
  "ScrollLock",
  "Shift",
  "Super",
  "Symbol",
  "SymbolLock",
]);

/** What the band says when the leader did not start a sequence because it came in the middle of
 *  typing (`TypingGuard.mayActAfterMotion`; the v1-ui GUI pass, 2026-09-27: "set up my" ran
 *  `<leader>m`, "the boy" `<leader>bo` and answered its y/n with its own `y`). `leaderLabel` is the
 *  leader as a person reads it (`Space`, or nvim's `mapleader`). The live conversation and the empty
 *  tab's dashboard both say it. */
export function leaderTypingFlash(leaderLabel: string): string {
  return `${leaderLabel} starts a sequence only on its own — i or Ctrl+j to type`;
}

export function isModifierKey(key: string): boolean {
  return MODIFIER_KEYS.has(key);
}

export type GuardTimers = {
  setTimeout: (run: () => void, ms: number) => unknown;
  clearTimeout: (handle: unknown) => void;
};

const GLOBAL_TIMERS: GuardTimers = {
  setTimeout: (run, ms) => globalThis.setTimeout(run, ms),
  clearTimeout: (handle) => globalThis.clearTimeout(handle as ReturnType<typeof setTimeout>),
};

type Keydown = { key: string; t: number };

export class TypingGuard {
  /** The key `onKey` recorded last -- the one being handled right now. */
  private latest: Keydown | null = null;
  /** The key before it: what `mayAnswerNow`/`defer` judge the current key against. */
  private previous: Keydown | null = null;
  /** Whether every key of the unbroken run ending at `latest` -- going back while each key came
   *  within the window of the next -- is a walk key. */
  private latestRunIsWalk = false;
  /** The same, for the run ending at `previous`: what Enter's walk exception asks. */
  private previousRunIsWalk = false;
  /** Whether every key of the unbroken run ending at `latest` is a motion key (`MOTION_KEYS`). */
  private latestRunIsMotion = false;
  /** The same, for the run ending at `previous`: what `mayActAfterMotion` asks. */
  private previousRunIsMotion = false;
  /** The waiting answer's timer, boxed so a timer handle that happens to be falsy still counts. */
  private pending: { handle: unknown } | null = null;

  constructor(private readonly timers: GuardTimers = GLOBAL_TIMERS) {}

  /** Every keydown the panel sees, FIRST, before anything acts on it. A bare modifier is ignored.
   *  Any other key cancels a waiting answer ("the key that cancelled it still does whatever it
   *  does"), and returns whether it did, so the caller can flash. */
  onKey(key: string, t: number): boolean {
    if (isModifierKey(key)) return false;
    const continuesRun = this.latest !== null && t - this.latest.t < TYPING_GUARD_MS;
    this.previousRunIsWalk = this.latestRunIsWalk;
    this.latestRunIsWalk = WALK_KEYS.has(key) && (!continuesRun || this.previousRunIsWalk);
    this.previousRunIsMotion = this.latestRunIsMotion;
    this.latestRunIsMotion = MOTION_KEYS.has(key) && (!continuesRun || this.previousRunIsMotion);
    this.previous = this.latest;
    this.latest = { key, t };
    return this.cancel();
  }

  /** Whether the key `onKey` just recorded (at `t`) may answer now: with nothing within the window
   *  before it, or, for Enter, when the whole unbroken run before it is a walk onto the button
   *  (`h`/`l`/Tab/Shift+Tab) back to a key that stood alone -- `l`⏎ and `l l`⏎ answer, `e`⏎,
   *  `al`⏎ and `jl`⏎ do not. A repeat never may. */
  mayAnswerNow(key: string, t: number, repeat: boolean): boolean {
    if (repeat) return false;
    const before = this.previous;
    if (before === null || t - before.t >= TYPING_GUARD_MS) return true;
    return key === "Enter" && this.previousRunIsWalk;
  }

  /** The leader, and Enter/Space on the activity line's Stop (the v1-ui GUI pass, 2026-09-27; this
   *  file's own doc comment): whether the key `onKey` just recorded (at `t`) may act -- with nothing
   *  within the window before it, or when the whole unbroken run before it is motion keys back to a
   *  key that stood alone (`j`⏎ and `jj<Space>` act; "just do it"'s Space and "the boy"'s do not).
   *  A repeat never may. */
  mayActAfterMotion(t: number, repeat: boolean): boolean {
    if (repeat) return false;
    const before = this.previous;
    if (before === null || t - before.t >= TYPING_GUARD_MS) return true;
    return this.previousRunIsMotion;
  }

  /** `a`/`d`/`D`: when the key `onKey` just recorded stands alone before, runs `run` after
   *  `TYPING_GUARD_MS` unless another key (`onKey`) or `cancel` comes first, and returns true.
   *  Otherwise runs nothing and returns false -- the caller flashes. */
  defer(t: number, repeat: boolean, run: () => void): boolean {
    this.cancel();
    if (!this.mayAnswerNow("", t, repeat)) return false;
    const waiting: { handle: unknown } = { handle: null };
    waiting.handle = this.timers.setTimeout(() => {
      if (this.pending !== waiting) return;
      this.pending = null;
      run();
    }, TYPING_GUARD_MS);
    this.pending = waiting;
    return true;
  }

  /** Drops a waiting answer, if any, and says whether there was one. Also called for the non-key
   *  cancellations (spec §2.1): `pane_focus`, a tab switch, an overlay opening. */
  cancel(): boolean {
    if (this.pending === null) return false;
    this.timers.clearTimeout(this.pending.handle);
    this.pending = null;
    return true;
  }
}
