/** A key that acts on the panel from BROWSE acts only when it stands alone, so typed prose that lands there (an
 *  arrival puts the page in BROWSE on a waiting card) never turns its letters into commands. This is a rule of
 *  Eitri's own: no tool the panel copies has an exact precedent; vim tells a sequence from a lone key by time
 *  (`:h 'timeoutlen'`). A deferred key (`defer`) needs a quiet `TYPING_GUARD_MS` before it and after it; another
 *  keydown cancels the wait. Both sides matter: the first letter has no predecessor, but a letter inside or ending
 *  a word has. A card's answer goes through this wait too, as the after-half of card mode's arming
 *  (`./cardMode`), the only way a key answers a card (a card button answers only a pointer's click).
 *
 *  Repeats never answer. A key that must also be let go (`deferUntilReleased`: `j` into the box) acts
 *  `TYPING_GUARD_MS` after the press if the key is up by then, else at its release, and any key, including its
 *  own repeat, cancels it. Compositor repeat begins after 500-600 ms by default, too late for the 250 ms window;
 *  with repeat off it never comes. A press longer than `RELEASE_WAIT_MAX_MS` is refused on release.
 *  Measure duration using the events' times: a timer would count a main-thread stall as holding and
 *  refuse a quick press during one.
 *
 *  The leader and Enter/Space on Stop accept a quiet key or an unbroken motion run beginning with
 *  one (`j`/`k`/`h`/`l`/`g`/`G`/Tab). Otherwise prose like `set up my` runs `<leader>m` (the mode
 *  flips to bypass, which on a switch-capable sidecar also approves every waiting card), `the boy`
 *  runs `<leader>bo` (close the other tabs) and answers its confirmation with its own `y`, and
 *  `just do it` reaches Stop and interrupts with Space, which also denies the card.
 *  `jj<Space>bd` and `j` then Enter remain quick motions. HINT's `f` and single-key table bindings
 *  such as H/L need the forward wait instead: as first letters they have no preceding motion run.
 *  Without it `fix the dashboard layout` starts HINT and `Looks good, now add tests` switches tabs,
 *  sending the remaining prose to the editor, terminal or another composer.
 *
 *  INPUT entry (`i`/`o`/`A`) requires a pure pause with no motion exception: `look at` would walk
 *  with `l` then enter with `o`, and `explain` or `follow-up` would lose their prefixes. A swallowed
 *  key says `TYPE_HINT_FLASH`. The one key it may follow at once is the Esc that just took this page
 *  from INPUT to BROWSE (`noteLeftInput`), as in vim's `Esc i`: no prose holds an Esc, and the key
 *  only returns the page to the INPUT it was in, so nothing becomes reachable that was not already.
 *  Any other Esc (BROWSE's, a region's) gets no exception. BROWSE's `y` copies immediately and opens a typing burst: following
 *  keys within the window are swallowed, so `y i w` cannot copy, enter INPUT and type `w`.
 *  The clipboard is still overwritten; text without `i` is still dropped. A lone `i` after a pause works.
 *
 *  `j`/Down with nowhere further to go walks on into the box (`walkIntoBox`), and asks the most of any key:
 *  quiet before it, so a fast `jjj` to the bottom stops there; quiet after it, because `j` is a first letter and
 *  "just do it" must not lose its `j` to the box; and let go, because a held key would otherwise repeat into the
 *  draft as `jjjj` from the moment the box took the keys.
 *
 *  An arrival is a key (`noteArrival`). The host reveals a tab here (`arrive`, `focus_permission`) from a key or
 *  click made somewhere else, a face in another window, `prefix a`, `prefix u`, and the person may still be
 *  typing. Recorded as a key at the moment the envelope came, it puts every rule above in force from there: a
 *  deferred key within `TYPING_GUARD_MS` after it is refused, a run that began inside that window is no
 *  motion run, and the keys it refused count as typing, so a run typed on through the arrival stays refused.
 *
 *  Pure: no DOM reads. Callers supply event timestamps; injectable timers default to globals looked
 *  up at call time so fake timers apply. */

/** The guard window, both sides. Typed prose at 60 wpm is ~200 ms a key; a
 *  deliberate single `a` waits well over this for the next key. A tunable, not a key. */
export const TYPING_GUARD_MS = 250;

/** How long a key that must be let go (`deferUntilReleased`) may stay down and still answer at its
 *  release. A held key is otherwise refused by its first repeat, but with key repeat off there is none,
 *  so the release cap must also refuse a key held for seconds. Below the compositor's repeat delay (500-600 ms by
 *  default), so the cap, not the repeat, is what refuses a held key in either case; well above a
 *  deliberate press, which is up within about 150 ms. */
export const RELEASE_WAIT_MAX_MS = 450;

/** What an arrival is recorded as: no key a keydown can name, so nothing is ever its release or its repeat. */
const ARRIVAL = "\u0000arrival";

/** The keys that move the cursor without typing anything (`keymap.ts`'s BROWSE motions, the empty
 *  tab's `j`/`k`, and Tab), so the leader or Enter/Space on Stop at the end of a run made only of
 *  them is a quick motion, not prose (`mayActAfterMotion`). Shift+Tab arrives as `Tab` (the caller
 *  normalizes WebKitGTK's `Unidentified`/`code: "Tab"` shape to it). */
export const DEFAULT_MOTION_KEYS: ReadonlySet<string> = new Set(["h", "j", "k", "l", "g", "G", "Tab", "ArrowUp", "ArrowDown"]);

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

/** What the band says when the leader arrived during typing (`TypingGuard.mayActAfterMotion`).
 *  `leaderLabel` is the human label (`Space` or nvim's `mapleader`); both the conversation and the
 *  empty tab's dashboard use it. */
export function leaderTypingFlash(leaderLabel: string): string {
  return `${leaderLabel} starts a sequence only on its own — i or Ctrl+j to type`;
}

/** What the band says when `f` did not ask `shell` for a HINT because another
 *  key came within `TYPING_GUARD_MS` of it (`TypingGuard.defer`, the same wait a card-mode answer uses -- `f` is
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
 *  ...), so the band names what did not happen rather than assuming every such key steps tabs: an
 *  nvim-read binding could delete instead. The empty tab's
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

/** Modifiers whose own keydown still counts as "another key" for the guard. The shell withholds
 *  any key pressed with Super or Hyper from this page (`shell/src/panel_super.rs`), so for
 *  `a` then `Super+x` the page sees only Super go down -- and a waiting `a` must be cancelled by
 *  that, as the `x` would have cancelled it. */
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
 *  `released` whether that release has come; `due` whether the window after the press has ended;
 *  `pressedAt` the press's own time, which the release's is compared with; `heldFlash` what the band
 *  says when the key was down longer than `RELEASE_WAIT_MAX_MS`. */
type Pending = {
  handle: unknown;
  flash: string;
  run: () => void;
  heldKey: string | null;
  released: boolean;
  due: boolean;
  pressedAt: number;
  heldFlash: string;
};

export class TypingGuard {
  private motionKeys: ReadonlySet<string> = DEFAULT_MOTION_KEYS;

  /** Change the effective motion keys without resetting the run, arming or waiting action. */
  setMotionKeys(keys: ReadonlySet<string>): void {
    this.motionKeys = new Set(keys);
  }

  /** The key `onKey` recorded last -- the one being handled right now. */
  private latest: Keydown | null = null;
  /** The key before it: what `mayAnswerNow`/`defer` judge the current key against. */
  private previous: Keydown | null = null;
  /** Whether every key of the unbroken run ending at `latest` -- going back while each key came
   *  within the window of the next -- is a motion key (`motionKeys`). */
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
  /** The key `noteLeftInput` was told of: the Esc that took this page from INPUT to BROWSE. Kept as the very
   *  record `onKey` made, so any key or arrival recorded after it is not it. */
  private leftInput: Keydown | null = null;
  /** When a BROWSE `y` last copied (its keydown's timestamp), for `afterCopy`. */
  private copiedAt: number | null = null;

  /** The last keydown that was not a repeat: its own time and `performance.now()` when it was handled,
   *  for `timeOf`. */
  private fresh: { typed: number; perf: number } | null = null;
  /** The least lag seen from a fresh press to its handling (`performance.now()` less the press's own time), for
   *  `clockNow`. The least, since no key is handled before its press, so it is the nearest to the clocks' offset;
   *  a key held up by a stall lags more and never moves it. */
  private leastLag = Infinity;

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
      if (!event.repeat) this.leastLag = Math.min(this.leastLag, perfNow - stamp);
      return stamp;
    }
    return Math.max(stamp, this.fresh.typed + Math.max(0, perfNow - this.fresh.perf));
  }

  /** The present moment on the keydowns' clock, for an event that is not a key (`noteArrival`). A stamp is
   *  `performance.now()`'s clock (both count from the page's time origin), and a key is never handled before its
   *  press, so while no fresh press has lagged less than nothing the present is `perfNow` itself. A lag says how
   *  long a key waited, not where the clock is: taking even the least of them off would date the present early by
   *  the wait of every key seen so far, and an arrival dated early lets a key typed just after it answer. Stamps
   *  ahead of `performance.now()` (jsdom's `Date`, a compositor's own clock) are another clock, read as `perfNow`
   *  less the least lag: early by the least wait any fresh press has had, which for a key handled at once is
   *  about nothing. With no key seen yet it is `perfNow`, the clock `timeOf` takes for a keydown with no stamp. */
  clockNow(perfNow: number = performance.now()): number {
    return this.leastLag === Infinity ? perfNow : perfNow - Math.min(0, this.leastLag);
  }

  /** The host revealed a tab in this page at `at` on the keydowns' clock (`clockNow`): recorded as a key that
   *  is not a motion, so every deferred key, the leader and INPUT entry wait `TYPING_GUARD_MS`
   *  after it, a run begun inside that window is typing, and a waiting action is cancelled as by any key. Never
   *  before the last key recorded. It is not counted in `keyCount`: that ties a landing to the key the page saw
   *  make it, and the arrival's own landing is tied to the keys after it. */
  noteArrival(at: number): void {
    const t = this.latest === null ? at : Math.max(at, this.latest.t);
    this.previousRunIsMotion = this.latestRunIsMotion;
    this.latestRunIsMotion = false;
    this.previous = this.latest;
    this.latest = { key: ARRIVAL, t };
    this.drop();
  }

  /** When a keyup happened, on the keydowns' clock (`event.timeStamp`, or `perfNow` where a page gives
   *  none), for `onKeyUp`. A release is a real event of its own, never a synthesized repeat, so its
   *  stamp is its own time; it changes nothing `timeOf` keeps. */
  timeOfRelease(event: { timeStamp: number }, perfNow: number = performance.now()): number {
    return event.timeStamp > 0 ? event.timeStamp : perfNow;
  }

  /** Every keydown the panel sees, FIRST, before anything acts on it. A bare modifier is ignored.
   *  Any other key cancels a waiting action ("the key that cancelled it still does whatever it
   *  does"), and returns what the band should say about the one it cancelled -- the flash its own
   *  `defer` named, so a cancelled `f` or `L` says what did not happen, in its
   *  own words -- or `null` when nothing was waiting. */
  onKey(key: string, t: number): string | null {
    if (isModifierKey(key) && !CANCELLING_MODIFIERS.has(key)) return null;
    this.recorded += 1;
    const continuesRun = this.latest !== null && t - this.latest.t < TYPING_GUARD_MS;
    this.previousRunIsMotion = this.latestRunIsMotion;
    this.latestRunIsMotion = this.motionKeys.has(key) && (!continuesRun || this.previousRunIsMotion);
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

  /** The key `onKey` just recorded is an Esc that took this page from INPUT to BROWSE. The next key, if it is
   *  `i`/`o`/`A` and nothing (no other key, no arrival) came between, may enter INPUT again at once
   *  (`mayStartInput`). Called where the mode changes, never guessed from a key's name. */
  noteLeftInput(): void {
    this.leftInput = this.latest;
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

  /** Whether the key `onKey` just recorded (at `t`) stands alone: nothing within the window before it, and not a
   *  repeat. */
  mayAnswerNow(t: number, repeat: boolean): boolean {
    if (repeat) return false;
    const before = this.previous;
    return before === null || t - before.t >= TYPING_GUARD_MS;
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
   *  `mayAnswerNow`'s rule, and **none of `mayActAfterMotion`'s motion exception**. A lone `h`/`j`/`k`/`l`/`G` is as much the first letter of a word as a motion, so
   *  with that exception "look at" walked with `l` and opened INPUT with `o` ("ok at"), and "how", "hi",
   *  "like", "join" and "Good" did the same. The price, stated: a fast `ji` chord now needs a pause. A
   *  repeat never may.
   *
   *  The exception is the key right after the Esc that left INPUT (`noteLeftInput`; vim's `Esc i`, `Esc A`),
   *  however fast and wherever that Esc fell, an arrival's window included: no word holds an Esc, and the key only
   *  takes the page back to the INPUT it was just in, where every key that answers (the question dock's) was
   *  already live and keeps its own guard. A key or an arrival between the Esc and this key
   *  ends the exception. Any other Esc, BROWSE's own or a region's, gets none, so no sequence that starts in
   *  BROWSE reaches INPUT sooner than before. */
  mayStartInput(t: number, repeat: boolean): boolean {
    if (repeat) return false;
    if (this.previous !== null && this.previous === this.leftInput) return true;
    return this.mayAnswerNow(t, repeat);
  }

  /** `f`, a single-key table binding, and a card-mode answer's after-half: when the key `onKey` just recorded
   *  stands alone before, runs `run` after `TYPING_GUARD_MS` unless another key (`onKey`) or `cancel` comes first,
   *  and returns true. Otherwise runs nothing and returns false -- the caller flashes. `cancelledFlash` is what
   *  `onKey` hands back should a key cancel this wait. */
  defer(t: number, repeat: boolean, run: () => void, cancelledFlash: string): boolean {
    return this.wait(t, repeat, run, cancelledFlash, null);
  }

  /** `defer`, for a key that must also have been let go: `run` runs `TYPING_GUARD_MS` after the
   *  press if `onKeyUp(heldKey)` came by then, else at that release -- unless another key (`onKey`,
   *  the held key's own repeat included) or `cancel` comes first, or the release came more than
   *  `RELEASE_WAIT_MAX_MS` after the press `t`, which it then refuses with `heldFlash`. `heldKey` is
   *  the key as the caller names it to `onKeyUp` (its `code`). */
  deferUntilReleased(
    t: number,
    repeat: boolean,
    run: () => void,
    cancelledFlash: string,
    heldKey: string,
    heldFlash: string = cancelledFlash,
  ): boolean {
    return this.wait(t, repeat, run, cancelledFlash, heldKey, heldFlash);
  }

  /** `j`/Down that can move no further: `enter` runs as for `deferUntilReleased`, so only a press that stood alone
   *  and was let go reaches the box. Returns whether it waits; otherwise the caller says how to type. */
  walkIntoBox(event: { repeat: boolean; code?: string; key: string }, t: number, enter: () => void): boolean {
    return this.deferUntilReleased(t, event.repeat, enter, TYPE_HINT_FLASH, heldKeyName(event));
  }

  /** A key let go at `t` (`timeOfRelease`), named as `deferUntilReleased` was given it. Ends a wait
   *  that is waiting for this key's release: it runs, or, when the key was down longer than
   *  `RELEASE_WAIT_MAX_MS`, is dropped and this returns its `heldFlash` for the band. Any other
   *  release changes nothing and returns `null`. */
  onKeyUp(heldKey: string, t: number): string | null {
    const waiting = this.pending;
    if (waiting === null || waiting.heldKey !== heldKey || waiting.released) return null;
    waiting.released = true;
    if (t - waiting.pressedAt > RELEASE_WAIT_MAX_MS) {
      this.drop();
      return waiting.heldFlash;
    }
    if (waiting.due) this.fire(waiting);
    return null;
  }

  private wait(
    t: number,
    repeat: boolean,
    run: () => void,
    cancelledFlash: string,
    heldKey: string | null,
    heldFlash: string = cancelledFlash,
  ): boolean {
    this.cancel();
    if (!this.mayAnswerNow(t, repeat)) return false;
    const waiting: Pending = {
      handle: null,
      flash: cancelledFlash,
      run,
      heldKey,
      released: heldKey === null,
      due: false,
      pressedAt: t,
      heldFlash,
    };
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
