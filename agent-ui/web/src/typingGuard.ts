/**
 * A permission card is answered
 * only by a key that stands alone. **Eitri's own rule** -- no tool this panel copies has an exact
 * precedent. Claude Code answers a prompt only by an explicit choice (Enter on the highlighted option,
 * or a digit); vim tells a sequence from a lone key by time (`:h 'timeoutlen'`). This keeps
 * `a`/`d` and still makes "arrive, then type" harmless: arrivals land BROWSE with the cursor on
 * a waiting card, and a Claude Code user types at once, so the first letters of a
 * sentence arrive as BROWSE keys.
 *
 * - `a`/`d`/`D` answer only with no other key within `TYPING_GUARD_MS` before them **and** none
 *   within it after: they are deferred, and any keydown before the wait ends cancels them. Both
 *   directions, because the first letter of a sentence has no key before it ("also add a test") and
 *   a letter inside or at the end of one has ("please add", "fix the data").
 * - Enter on a focused card button answers only with no key within the window before it, unless
 *   every key of the unbroken run before it (each within the window of the next) was `h`, `l`, Tab
 *   or Shift+Tab -- a walk onto Approve/Deny/Always that began with a key standing alone. **This
 *   narrows an earlier rule**, which let Enter through whenever the ONE key before it was a walk, and
 *   assumed the residual ("…al⏎" right after an arrival onto a card) "lands on Deny, the safe side".
 *   That was false: `l` from the card's row walks to its first control, which is Approve
 *   (`PermissionCard.tsx`'s `data-nav-order`), so "cancel⏎" or "deal⏎" typed at once after an
 *   arrival approved the card. What is left, stated rather than hidden: a walk run that itself
 *   began with a key standing alone -- a message of nothing but walk keys ("l⏎" approves, "ll⏎"
 *   denies), or prose paused for `TYPING_GUARD_MS` or more right before a final `l` whose Enter
 *   follows at once ("cance", a pause, "l⏎" approves, as it did under the earlier rule).
 *   **A second narrowing (2026-09-29)**: `:ls⏎` approved a card through
 *   Enter's native activation, since `l` walked onto Approve and `s` was a pause away. Enter on a
 *   card's own button is now claimed and waits like `a`/`d` (`defer`, told the key is Enter so the
 *   walk exception above still applies), and `App.tsx` presses the button only when focus was put
 *   on that very button by the key right before Enter -- an `h`/`l` that moved it, a Tab -- or by a
 *   HINT landing with no key since. That landing is recorded where focus moves, never guessed from
 *   a key's name: this guard decides no landing, it only counts keys (`keyCount`), so `App.tsx` can
 *   tie the landing it saw to the key that made it.
 * - A held key's repeat (`event.repeat`) never answers. An answer that must not come from a held key
 *   at all -- INPUT's `Ctrl+y` -- also waits for its key's release (`deferUntilReleased`): the wait
 *   ends `TYPING_GUARD_MS` after the press only if the key is up by then, else at its release, and
 *   the key's first repeat, like any key, cancels it. A key's repeat begins only after the
 *   compositor's delay (500-600 ms by default), well after the window, so the repeat alone cannot
 *   tell a held key from a pressed one in time.
 * - **A GUI pass (2026-09-27) found two more ways typed prose acts after an arrival**, and
 *   `mayActAfterMotion` closes both. The leader (Space by default) starts a sequence wherever the
 *   row cursor has the keys, so "set up my" ran `<leader>m` (the mode flipped to bypass, which on a
 *   switch-capable sidecar also approves every waiting card) and "the boy" ran
 *   `<leader>bo` and answered its own y/n with the `y`. And `j` from the last row lands on the
 *   activity line's Stop, so "just do it" interrupted the turn with its first Space (the card was
 *   denied with it). So the leader and Enter/Space on Stop act only on a key that stands alone, or
 *   at the end of an unbroken run of motion keys (`j`/`k`/`h`/`l`/`g`/`G`/Tab) that itself began
 *   with one: `jj<Space>bd` and `j`⏎ onto Stop stay one quick motion, a word does not.
 * - **Two more cases turned up,
 *   and neither can use `mayActAfterMotion`**: `f` (the global HINT trigger) and a single-key table
 *   binding that runs on its very first press (H/L's default tab.prev/tab.next, or an nvim-read
 *   binding on some other plain letter) are each always the FIRST key of whatever typed them, so
 *   they never have a motion run behind them to check -- "fix the dashboard layout⏎" started a HINT
 *   on its own `f` and "Looks good, now add tests⏎" switched tabs on its own `L`, sending the rest of
 *   the sentence into nvim, a shell or another session's composer. These use `defer` instead, the
 *   same forward-looking wait `a`/`d` already use (the "after" half of their rule): the key
 *   acts `TYPING_GUARD_MS` later, only if nothing else was typed meanwhile.
 * - **It extends to BROWSE's own entry into INPUT and to the keys after a copy (2026-09-29).** `i`/`o`/`A` enter
 *   INPUT only when `mayStartInput` allows it -- on a key that stands alone (a pure pause, NO motion exception: with
 *   the leader's `mayActAfterMotion` rule "look at" walked with `l` and opened INPUT with `o`) -- so "expla|in" no
 *   longer loses "expla" to the `i` inside the word, and "follow-up" no longer sends "llow-up" after its `o` opened
 *   the composer (a swallowed key says `TYPE_HINT_FLASH`). And `y` opens a burst (`noteCopy`/`afterCopy`): any key
 *   within `TYPING_GUARD_MS` of a BROWSE `y` is typing, so vim's `y i w` no longer copies the row, opens INPUT and
 *   types the `w`. The copy itself is frozen and still happens at once, the clipboard still overwritten; text typed
 *   without `i` is still dropped. A lone `i` after a pause is unchanged.
 *
 * Pure: it never reads the DOM. Timestamps are the caller's (`event.timeStamp`); the timers are
 * injectable and default to the global ones, looked up at call time so a test's fake timers apply.
 */

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

/** The BROWSE placeholder's own words (`Composer`'s hint line; the flash when `j` cannot move),
 *  which is also what the band says when the guard swallowed an `i`/`o`/`A` (or any key right after a
 *  `y`) because it came in the middle of typing. */
export const TYPE_HINT_FLASH = "i or Ctrl+j to type";

/** What the band says when the leader did not start a sequence because it came in the middle of
 *  typing (`TypingGuard.mayActAfterMotion`; a GUI pass, 2026-09-27: "set up my" ran
 *  `<leader>m`, "the boy" `<leader>bo` and answered its y/n with its own `y`). `leaderLabel` is the
 *  leader as a person reads it (`Space`, or nvim's `mapleader`). The live conversation and the empty
 *  tab's dashboard both say it. */
export function leaderTypingFlash(leaderLabel: string): string {
  return `${leaderLabel} starts a sequence only on its own — i or Ctrl+j to type`;
}

/** What the band says when `f` did not ask `shell` for a HINT because another
 *  key came within `TYPING_GUARD_MS` of it (`TypingGuard.defer`, the same wait `a`/`d` use -- `f` is
 *  always the first key of whatever typed it, so `mayActAfterMotion`'s backward-only check cannot
 *  cover it). */
export function hintTypingFlash(): string {
  return "f starts HINT only on its own — i or Ctrl+j to type";
}

/** What the band says when a single-key table binding that runs on its very
 *  first press -- H/L's default tab.prev/tab.next, or an nvim-read binding on some other plain
 *  letter -- did not run, for the same reason `hintTypingFlash` does not. `key` is the key as a
 *  person reads it (already human: a table's single-key bindings are never the leader or a space);
 *  `desc` is the binding's own which-key description (`PanelBinding.desc`: "next tab", "close tab",
 *  ...), so the band names what did not happen rather than assuming every such key steps tabs (an
 *  nvim-read delete binding was once flashed as "switches tabs"). The empty tab's
 *  own single letters (`r`, `w`) say theirs the same way. */
export function tableKeyTypingFlash(key: string, desc: string): string {
  return `${key} (${desc}) runs only on its own — i or Ctrl+j to type`;
}

/** How a press and its release name the same key for `deferUntilReleased`/`onKeyUp`: its `code` (the
 *  physical key, the same on its keydown and keyup whatever modifiers changed meanwhile), or, where
 *  there is none, its `key` in lower case (Shift or Caps Lock let go first changes the case). */
export function heldKeyName(event: { code?: string; key: string }): string {
  return event.code || event.key.toLowerCase();
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

/** A waiting action. `heldKey` is the key whose release it also waits for (`deferUntilReleased`), and
 *  `released` whether that release has come; `due` whether the window after the press has ended. */
type Pending = { handle: unknown; flash: string; run: () => void; heldKey: string | null; released: boolean; due: boolean };

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
  private pending: Pending | null = null;
  /** How many keys `onKey` has recorded (bare modifiers excepted, the cancelling ones counted),
   *  so `App.tsx` can tie a landing it saw to the key that made it -- never a guess from a key's
   *  name. */
  private recorded = 0;
  /** When a BROWSE `y` last copied (its keydown's timestamp), for `afterCopy`. */
  private copiedAt: number | null = null;

  /** The last keydown that was not a repeat: its own time and `performance.now()` when it was handled,
   *  for `timeOf`. */
  private fresh: { typed: number; perf: number } | null = null;

  constructor(private readonly timers: GuardTimers = GLOBAL_TIMERS) {}

  /** When a keydown happened, on the keydowns' own clock (`event.timeStamp`, or `perfNow` where a
   *  page gives none): what `onKey` and the rest are handed. A repeat is not dated by its own stamp
   *  alone. GDK synthesizes a held key's repeats itself and stamps every one with the press's time,
   *  which WebKitGTK passes on as `timeStamp`, so that clock stands still for as long as a key is
   *  held, and a key pressed just after letting go of one looked as if it came long after it. A
   *  repeat is at least as late as the last fresh press plus the time since, on `performance.now()`. */
  timeOf(event: { repeat: boolean; timeStamp: number }, perfNow: number = performance.now()): number {
    const stamp = event.timeStamp > 0 ? event.timeStamp : perfNow;
    if (!event.repeat || this.fresh === null) {
      this.fresh = { typed: stamp, perf: perfNow };
      return stamp;
    }
    return Math.max(stamp, this.fresh.typed + Math.max(0, perfNow - this.fresh.perf));
  }

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

  /** The number of keys `onKey` has recorded so far. The key being handled right now is the
   *  last of them, so a landing recorded as `keyCount()` by one key is "the key right before" the
   *  next one exactly when that next key reads `keyCount() - 1` for it. */
  keyCount(): number {
    return this.recorded;
  }

  /** A BROWSE `y` copied, at `t` (its keydown's timestamp). Opens the burst `afterCopy` asks
   *  about. Called where the copy happens, never for a `y` typed into the composer or a text box. */
  noteCopy(t: number): void {
    this.copiedAt = t;
  }

  /** Whether the key `onKey` just recorded (at `t`) came within `TYPING_GUARD_MS` after the
   *  last `noteCopy` -- typing, not a command, so the caller swallows it and says `TYPE_HINT_FLASH`.
   *  A key stamped before the copy is never inside its burst. */
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

  /** The leader, and Enter/Space on the activity line's Stop (see this
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

  /** `a`/`d`/`D`, `f`, a single-key table binding, and Enter on a card's own button: when the
   *  key `onKey` just recorded stands alone before, runs `run` after `TYPING_GUARD_MS` unless another
   *  key (`onKey`) or `cancel` comes first, and returns true. Otherwise runs nothing and returns
   *  false -- the caller flashes. `cancelledFlash` is what `onKey` hands back should a key cancel
   *  this wait. `key` is what `mayAnswerNow` judges: only `"Enter"` takes its walk exception, so
   *  every caller but Enter's leaves it out. */
  defer(t: number, repeat: boolean, run: () => void, cancelledFlash: string, key = ""): boolean {
    return this.wait(t, repeat, run, cancelledFlash, key, null);
  }

  /** `defer`, for a key that must also have been let go: `run` runs `TYPING_GUARD_MS` after the
   *  press if `onKeyUp(heldKey)` came by then, else at that release -- unless another key (`onKey`,
   *  the held key's own repeat included) or `cancel` comes first. `heldKey` is the key as the
   *  caller names it to `onKeyUp` (its `code`). */
  deferUntilReleased(t: number, repeat: boolean, run: () => void, cancelledFlash: string, heldKey: string): boolean {
    return this.wait(t, repeat, run, cancelledFlash, "", heldKey);
  }

  /** A key let go, named as `deferUntilReleased` was given it. Ends a wait that is waiting for this
   *  key's release; any other release changes nothing. */
  onKeyUp(heldKey: string): void {
    const waiting = this.pending;
    if (waiting === null || waiting.heldKey !== heldKey || waiting.released) return;
    waiting.released = true;
    if (waiting.due) this.fire(waiting);
  }

  private wait(
    t: number,
    repeat: boolean,
    run: () => void,
    cancelledFlash: string,
    key: string,
    heldKey: string | null,
  ): boolean {
    this.cancel();
    if (!this.mayAnswerNow(key, t, repeat)) return false;
    const waiting: Pending = { handle: null, flash: cancelledFlash, run, heldKey, released: heldKey === null, due: false };
    waiting.handle = this.timers.setTimeout(() => {
      if (this.pending !== waiting) return;
      waiting.handle = null;
      waiting.due = true;
      if (waiting.released) this.fire(waiting);
    }, TYPING_GUARD_MS);
    this.pending = waiting;
    return true;
  }

  private fire(waiting: Pending): void {
    if (this.pending !== waiting) return;
    this.pending = null;
    waiting.run();
  }

  /** Drops a waiting action, if any, and says whether there was one. Also called for the non-key
   *  cancellations: `pane_focus`, a tab switch, an overlay opening. */
  cancel(): boolean {
    return this.drop() !== null;
  }

  /** Drops a waiting action, returning its `cancelledFlash`, or `null` when none was waiting. */
  private drop(): string | null {
    if (this.pending === null) return null;
    const { flash, handle } = this.pending;
    if (handle !== null) this.timers.clearTimeout(handle);
    this.pending = null;
    return flash;
  }
}
