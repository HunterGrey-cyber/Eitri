/**
 * Permission keys are guarded because typed prose can arrive in BROWSE with the cursor on a
 * waiting card. A command letter may be the first, middle or last letter of a sentence.
 *
 * `a`/`d`/`D` answer only when no other key occurs within `TYPING_GUARD_MS` before or after them.
 * They are deferred, and any keydown before the wait ends cancels them.
 * Enter on a focused card button requires a pause before it, unless the entire unbroken run
 * before it consists of `h`, `l`, Tab or Shift+Tab and began with a key standing alone.
 * Checking just the preceding key would let prose ending in `l` activate Approve: `l` from
 * its row moves to the card's first control, Approve (`PermissionCard.tsx`, `data-nav-order`).
 * A run made only of walk keys can still answer: `l` followed by Enter approves, and `ll`
 * followed by Enter denies. A pause of at least `TYPING_GUARD_MS` before a final `l` also
 * makes that `l` followed by Enter eligible.
 * A held key's repeat (`event.repeat`) never answers.
 *
 * `mayActAfterMotion` guards the leader and Enter/Space on Stop. A Space in prose can start
 * a leader sequence, and `j` from the last row can put the next Space on Stop. These keys act
 * only after a pause or an unbroken run of `j`/`k`/`h`/`l`/`g`/`G`/Tab that began with a key
 * standing alone, so quick navigation can still end in an action.
 *
 * `f` and single-key table bindings can be the first letter typed, with no motion run to
 * check. They use `defer` to wait `TYPING_GUARD_MS` and act only if no further key arrives.
 * This prevents a sentence's first letter from opening HINT or switching the input target.
 *
 * Pure: it never reads the DOM. Timestamps come from the caller (`event.timeStamp`). Timers
 * are injectable and otherwise looked up globally at call time so fake timers apply.
 */

/** The guard window before and after a key. Prose at 60 wpm is about 200 ms per key;
 *  a deliberate single `a` leaves a longer gap. This is a tunable timing value. */
export const TYPING_GUARD_MS = 250;

/** The keys that walk onto a card button, so Enter at the end of a run made only of them is not
 *  typing. Shift+Tab arrives as `Tab` (the caller normalizes WebKitGTK's `Unidentified`/
 *  `code: "Tab"` shape to it). */
const WALK_KEYS = new Set(["h", "l", "Tab"]);

/** The keys that move the cursor without typing (`keymap.ts`'s BROWSE motions, the empty
 *  tab's `j`/`k`, and Tab). `mayActAfterMotion` allows the leader or Enter/Space on Stop after
 *  a run of these keys. `WALK_KEYS` stays narrower for a card's Approve/Deny controls. */
const MOTION_KEYS = new Set(["h", "j", "k", "l", "g", "G", "Tab", "ArrowUp", "ArrowDown"]);

/** Bare modifiers are not "a key". `key` values from the
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

/** The band message when another key within `TYPING_GUARD_MS` cancels a deferred `f` HINT.
 *  `f` can be the first letter of prose, so `mayActAfterMotion`'s backward check cannot protect it. */
export function hintTypingFlash(): string {
  return "f starts HINT only on its own — i or Ctrl+j to type";
}

/** The band message when typing suppresses a single-key table binding, as in `hintTypingFlash`.
 *  `key` is the human-readable key; these bindings are never the leader or a space.
 *  `desc` is `PanelBinding.desc`, so the message names the actual action, such as closing a
 *  tab, instead of assuming every binding switches tabs. Empty-tab `r`/`w` use it too. */
export function tableKeyTypingFlash(key: string, desc: string): string {
  return `${key} (${desc}) runs only on its own — i or Ctrl+j to type`;
}

export function isModifierKey(key: string): boolean {
  return MODIFIER_KEYS.has(key);
}

/** Modifiers whose own keydown counts as another key for the guard. The shell withholds keys
 *  pressed with Super or Hyper (`shell/src/panel_super.rs`), so `a` then `Super+x` exposes only Super.
 *  That keydown must cancel the waiting `a` as the hidden `x` would. */
const CANCELLING_MODIFIERS = new Set(["Super", "Hyper", "OS"]);

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
  /** The waiting action's timer, boxed so a timer handle that happens to be falsy still counts, and
   *  what the band says should a key cancel it (`defer`'s `cancelledFlash`). */
  private pending: { handle: unknown; flash: string } | null = null;

  constructor(private readonly timers: GuardTimers = GLOBAL_TIMERS) {}

  /** Every keydown the panel sees, FIRST, before anything acts on it. A bare modifier is ignored.
   *  Any other key cancels a waiting action ("the key that cancelled it still does whatever it
   *  does"), and returns what the band should say about the one it cancelled -- the flash its own
   *  `defer` named, so a cancelled `f` or `L` says what did not happen instead of talking about
   *  `a`/`d` -- or `null` when nothing was waiting. */
  onKey(key: string, t: number): string | null {
    if (isModifierKey(key) && !CANCELLING_MODIFIERS.has(key)) return null;
    const continuesRun = this.latest !== null && t - this.latest.t < TYPING_GUARD_MS;
    this.previousRunIsWalk = this.latestRunIsWalk;
    this.latestRunIsWalk = WALK_KEYS.has(key) && (!continuesRun || this.previousRunIsWalk);
    this.previousRunIsMotion = this.latestRunIsMotion;
    this.latestRunIsMotion = MOTION_KEYS.has(key) && (!continuesRun || this.previousRunIsMotion);
    this.previous = this.latest;
    this.latest = { key, t };
    return this.drop();
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

  /** `a`/`d`/`D`, `f`, a single-key table binding: when the key `onKey` just recorded stands alone
   *  before, runs `run` after `TYPING_GUARD_MS` unless another key (`onKey`) or `cancel` comes
   *  first, and returns true. Otherwise runs nothing and returns false -- the caller flashes.
   *  `cancelledFlash` is what `onKey` hands back should a key cancel this wait. */
  defer(t: number, repeat: boolean, run: () => void, cancelledFlash: string): boolean {
    this.cancel();
    if (!this.mayAnswerNow("", t, repeat)) return false;
    const waiting: { handle: unknown; flash: string } = { handle: null, flash: cancelledFlash };
    waiting.handle = this.timers.setTimeout(() => {
      if (this.pending !== waiting) return;
      this.pending = null;
      run();
    }, TYPING_GUARD_MS);
    this.pending = waiting;
    return true;
  }

  /** Drops a waiting action, if any, and says whether there was one. Also called for the non-key
   *  cancellations: `pane_focus`, a tab switch, an overlay opening. */
  cancel(): boolean {
    return this.drop() !== null;
  }

  /** Drops a waiting action, returning its `cancelledFlash`, or `null` when none was waiting. */
  private drop(): string | null {
    if (this.pending === null) return null;
    const { flash } = this.pending;
    this.timers.clearTimeout(this.pending.handle);
    this.pending = null;
    return flash;
  }
}
