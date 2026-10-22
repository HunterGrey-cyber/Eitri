/** Guards commands against prose typed immediately after a card arrival in BROWSE mode.
 * A permission answer must stand alone so the first letters of a sentence cannot approve a card.
 * `a`/`d`/`D` require no other key within `TYPING_GUARD_MS` before or after them. They wait,
 * and any keydown before the timer ends cancels the answer. Repeated keydowns never answer.
 * Enter on a focused card button also requires a quiet window before it, except after an unbroken
 * run of `h`, `l`, Tab or Shift+Tab that began with a key standing alone.
 * Checking only the immediately preceding key would allow words ending in `l` to approve a card:
 * `l` from the card row focuses Approve. A run consisting only of walk keys still answers,
 * and a pause before a final walk key still allows the following Enter.
 * Enter is claimed and deferred to prevent native button activation. `App.tsx` activates only when
 * the immediately preceding key focused that exact button, or a HINT landed there with no key since.
 * Focus landings are recorded where focus moves. This guard only counts keys through `keyCount`.
 * The leader and Enter/Space on Stop require a lone key or an unbroken run of motion keys
 * (`j`/`k`/`h`/`l`/`g`/`G`/Tab) that began with one. This keeps words from opening leader sequences
 * or activating Stop after a motion letter moves focus there.
 * `f` and single-key table bindings can be the first letter of prose, so a backward check cannot
 * protect them. They use `defer` to wait for `TYPING_GUARD_MS` and act only if no other key arrives.
 * `i`/`o`/`A` enter INPUT only when `mayStartInput` allows a lone key, with no motion exception;
 * otherwise a word beginning with `lo` could walk and then open INPUT. Swallowed keys show `TYPE_HINT_FLASH`.
 * A BROWSE `y` copies immediately and opens a burst through `noteCopy`/`afterCopy`. Keys within
 * `TYPING_GUARD_MS` of the copy count as typing, preventing `y i w` from opening INPUT after copying.
 * The copy still overwrites the clipboard, and text typed without entering INPUT is still dropped.
 * A lone `i` after a pause enters INPUT normally.
 * Pure: this guard never reads the DOM. The caller supplies timestamps (`event.timeStamp`).
 * Timers are injectable and default to globals looked up at call time so fake timers apply. */

/** The guard window, both sides. Typed prose at 60 wpm is ~200 ms a key; a
 *  deliberate single `a` waits well over this for the next key. A tunable, not a key. */
export const TYPING_GUARD_MS = 250;

/** The keys that walk onto a card button, so Enter at the end of a run made only of them is not
 *  typing. Shift+Tab arrives as `Tab` (the caller normalizes WebKitGTK's `Unidentified`/
 *  `code: "Tab"` shape to it). */
const WALK_KEYS = new Set(["h", "l", "Tab"]);

/** The keys that move the cursor without typing anything (`keymap.ts`'s BROWSE motions, the empty
 *  tab's `j`/`k`, and Tab), so the leader or Enter/Space on Stop at the end of a run made only of
 *  them is a quick motion, not prose (`mayActAfterMotion`). Wider than `WALK_KEYS` on purpose: that
 *  set stays the narrower rule for a card's Approve/Deny, which is untouched. */
const MOTION_KEYS = new Set(["h", "j", "k", "l", "g", "G", "Tab", "ArrowUp", "ArrowDown"]);

/** Bare modifiers are not "a key": holding Shift or Ctrl is not typing. `key` values from the
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

/** The BROWSE placeholder text, also used by the band when the typing guard swallows `i`/`o`/`A`
 *  or a key immediately after a BROWSE `y`. */
export const TYPE_HINT_FLASH = "i or Ctrl+j to type";

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

/** The band message when typing cancels a deferred single-key table binding.
 *  `key` is the human-readable key label. `desc` is the binding's `PanelBinding.desc`, so the message
 *  names the blocked action instead of assuming all such keys switch tabs.
 *  The empty tab's single-letter actions (`r`, `w`) use the same format. */
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
  /** The count of keys recorded by `onKey`, excluding bare modifiers but including cancelling modifiers.
   *  `App.tsx` uses it to match a recorded focus landing to the key that made it. */
  private recorded = 0;
  /** #26: when a BROWSE `y` last copied (its keydown's timestamp), for `afterCopy`. */
  private copiedAt: number | null = null;

  constructor(private readonly timers: GuardTimers = GLOBAL_TIMERS) {}

  /** Every keydown the panel sees, FIRST, before anything acts on it. A bare modifier is ignored.
   *  Any other key cancels a waiting action ("the key that cancelled it still does whatever it
   *  does"), and returns what the band should say about the one it cancelled -- the flash its own
   *  `defer` named, so a cancelled `f` or `L` says what did not happen instead of talking about
   *  `a`/`d` -- or `null` when nothing was waiting. */
  onKey(key: string, t: number): string | null {
    if (isModifierKey(key) && !CANCELLING_MODIFIERS.has(key)) return null;
    this.recorded += 1;
    const continuesRun = this.latest !== null && t - this.latest.t < TYPING_GUARD_MS;
    this.previousRunIsWalk = this.latestRunIsWalk;
    this.latestRunIsWalk = WALK_KEYS.has(key) && (!continuesRun || this.previousRunIsWalk);
    this.previousRunIsMotion = this.latestRunIsMotion;
    this.latestRunIsMotion = MOTION_KEYS.has(key) && (!continuesRun || this.previousRunIsMotion);
    this.previous = this.latest;
    this.latest = { key, t };
    return this.drop();
  }

  /** A held key let go at `t` (a `j`/`k` whose repeats were moving the view, `./jkHold`): while it was
   *  held it stood for a key pressed again and again, so the window a following key is judged by starts
   *  at the release, not at its last repeat -- an `a` right after letting go of `j` waits like one right
   *  after a press. Only when nothing else was pressed since that key; it records no key of its own. */
  onRelease(key: string, t: number): void {
    if (this.latest !== null && this.latest.key === key && t > this.latest.t) this.latest = { key, t };
  }

  /** The number of keys recorded by `onKey`, including the key currently being handled.
   *  A landing recorded with `keyCount()` belongs to the preceding key exactly when the next key
   *  reads that landing's count as `keyCount() - 1`. */
  keyCount(): number {
    return this.recorded;
  }

  /** Records a BROWSE copy at its keydown timestamp `t` and opens the burst checked by `afterCopy`.
   *  Called where copying happens, never for a `y` typed in the composer or a text box. */
  noteCopy(t: number): void {
    this.copiedAt = t;
  }

  /** Whether the key just recorded at `t` falls within `TYPING_GUARD_MS` after `noteCopy`.
   *  The caller treats it as typing, swallows it and shows `TYPE_HINT_FLASH`.
   *  A timestamp before the copy is outside the burst. */
  afterCopy(t: number): boolean {
    return this.copiedAt !== null && t >= this.copiedAt && t - this.copiedAt < TYPING_GUARD_MS;
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

  /** BROWSE's `i`/`o`/`A` enter INPUT only on a pure
   *  pause -- nothing pressed within `TYPING_GUARD_MS` before the key `onKey` just recorded, exactly
   *  `mayAnswerNow`'s rule with none of its Enter exception, and **none of `mayActAfterMotion`'s motion
   *  exception either**. A lone `h`/`j`/`k`/`l`/`G` is as much the first letter of a word as a motion, so
   *  with that exception "look at" walked with `l` and opened INPUT with `o` ("ok at"), and "how", "hi",
   *  "like", "join" and "Good" did the same. The price, stated: a fast `ji` chord now needs a pause. A
   *  repeat never may. */
  mayStartInput(t: number, repeat: boolean): boolean {
    return this.mayAnswerNow("", t, repeat);
  }

  /** Defers `a`/`d`/`D`, `f`, single-key table bindings and Enter on a card button.
   *  If `mayAnswerNow` allows the recorded key, schedules `run` after `TYPING_GUARD_MS` and returns true.
   *  Another `onKey` or `cancel` cancels the wait. Otherwise schedules nothing and returns false for a flash.
   *  `cancelledFlash` is returned by `onKey` on cancellation. Only `key: "Enter"` allows the walk exception;
   *  other callers omit `key`. */
  defer(t: number, repeat: boolean, run: () => void, cancelledFlash: string, key = ""): boolean {
    this.cancel();
    if (!this.mayAnswerNow(key, t, repeat)) return false;
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
