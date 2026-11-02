import { isImeKey } from "./composerKeys";
import { TYPING_GUARD_MS } from "./typingGuard";
import type { PermissionModeChoice, TabInfo } from "./types";

/** Claude Code's mode key (Shift+Tab cycles the permission mode), claimed everywhere in the chat so it never
 *  becomes WebKit's backward focus navigation or GTK's `move-focus`: the mode must switch from anywhere in
 *  the agent pane, not only from the input. */
export function isModeCycleKey(e: {
  key: string;
  code?: string;
  shiftKey: boolean;
  ctrlKey: boolean;
  altKey: boolean;
  metaKey: boolean;
  isComposing: boolean;
  keyCode?: number;
}): boolean {
  if (isImeKey({ isComposing: e.isComposing, keyCode: e.keyCode ?? 0 })) return false;
  return isShiftTab(e) && !e.ctrlKey && !e.altKey && !e.metaKey;
}

/** Shift+Tab as WebKitGTK actually delivers it. A real keyboard's `us` layout (and any xkb layout)
 *  turns Shift+Tab into the keysym `ISO_Left_Tab`, which WebKitGTK 2.52.6 reports as
 *  `key: "Unidentified"`, `code: "Tab"`, `keyCode: 9` -- never `key: "Tab"`. Checking `key === "Tab"`
 *  alone misses a real keyboard and lets the key fall through to GTK's `move-focus`.
 *  `code` names the physical key, so it is what identifies Tab here; `key === "Tab"` is kept for a
 *  keymap (or a test) that does deliver it. */
export function isShiftTab(e: { key: string; code?: string; shiftKey: boolean }): boolean {
  if (!e.shiftKey) return false;
  return e.key === "Tab" || (e.key === "Unidentified" && e.code === "Tab");
}

export type ModeKeyState = {
  confirmOpen: boolean;
  chooserOpen: boolean;
  tabState: TabInfo["state"] | null;
  /** The tab's own mode (`TabInfo.mode`), read only to settle the `ended`/`failed` case: entering bypass there is refused, but a tab that is ALREADY in bypass can still leave it
   *  in every state, so `ended`/`failed` is not simply "fixed". `null` before any tab
   *  exists yet, the same as `tabState`. */
  tabMode: PermissionModeChoice | null;
};

/** `not_started`, `starting`
 *  and `live` all cycle -- there is no provider capability left to gate on, since every
 *  session is gated and Rust is the one authority for whether a cycle actually lands in bypass (it
 *  may reprompt with `confirm_bypass` instead of just applying it). `ended`/`failed` can only LEAVE
 *  bypass, never enter it. */
export function modeKeyRoute(s: ModeKeyState): "overlay" | "cycle" | "fixed" | "none" {
  if (s.confirmOpen || s.chooserOpen) return "overlay";
  if (s.tabState === null) return "none";
  if (
    s.tabState === "not_started" ||
    s.tabState === "starting" ||
    s.tabState === "awaiting_trust" ||
    s.tabState === "live"
  ) {
    return "cycle";
  }
  return s.tabMode === "bypass" ? "cycle" : "fixed";
}

/** The key that restarts an ended or failed session in place (`EmptyTab`'s own `r` while failed;
 *  the live conversation's `restart` case while ended) -- fixed, not a rebindable `panelTable`
 *  action, so there is nothing on the wire to read it from; unlike `newTabChord` it is never empty
 *  in practice, but the same optional shape is kept so a caller with none at all still gets a sensible message. */
export function modeFixedMessage(resetKey: string): string {
  return resetKey === ""
    ? "the session has ended — start a new one to change the mode"
    : `the session has ended — ${resetKey} to start again`;
}

/** How long a `confirm_bypass` prompt must have been on screen, and how long since the last
 *  non-modifier keydown ANYWHERE in the panel, before a `y`/`Y` counts as answering it -- typed text
 *  (even the very keys that triggered the prompt, like `<leader>m`) must never fall through into
 *  bypass. It IS `typingGuard.ts#TYPING_GUARD_MS`: one number for "typed text is still
 *  arriving". */
export const BYPASS_YES_GUARD_MS = TYPING_GUARD_MS;

/** No key waits this long in the queue of a page that keeps answering, so a lag past it may be another clock. */
export const OTHER_CLOCK_LAG_MS = 10_000;
/** How closely two keys' lags agree when the lag is the clocks' offset and not a wait: keys held up by one stall lag by
 *  different amounts, the time between their presses. */
export const LAGS_AGREE_MS = 20;

/** When each keydown was pressed, on `performance.now()`'s clock. A page that is busy queues keys, so the time a key is
 *  handled says nothing about what was on screen at the press; `event.timeStamp` does.
 *
 *  The handling time stands in where a stamp cannot be this clock: none at all, or one in the future (a test's faked
 *  `Date`, or a compositor's clock running ahead). It does for a repeat too: GDK stamps every repeat of a held key with
 *  the press, so the stamp stands still while the key is held. A clock running behind is told by the keys themselves:
 *  when no key so far has lagged less than `OTHER_CLOCK_LAG_MS` and a key lags within `LAGS_AGREE_MS` of the least lag
 *  so far, that least lag is the clocks' offset and is taken off. It is compared with the least and not the last, since
 *  some keys are always handled late and the least is the one that is near the offset; and it is always the least,
 *  since no key can lag less than the offset, so a press is never put after its own handling. Anything less proves
 *  nothing, so a key held up by a long stall keeps its own press. */
export class KeyClock {
  private leastLag = Infinity;
  private otherClock = false;

  pressedAt(event: { timeStamp: number; repeat?: boolean }, handledAt: number): number {
    const stamp = event.timeStamp;
    if (event.repeat === true || stamp <= 0 || stamp > handledAt) return handledAt;
    const lag = handledAt - stamp;
    if (Math.abs(lag - this.leastLag) <= LAGS_AGREE_MS) this.otherClock = true;
    this.leastLag = Math.min(this.leastLag, lag);
    if (this.leastLag <= OTHER_CLOCK_LAG_MS) this.otherClock = false;
    return this.otherClock ? stamp + this.leastLag : stamp;
  }
}

/** Both halves of that guard, pure and clock-injected. `now` is this key's press (`KeyClock`), so a key pressed
 *  before the question was painted never counts, however late it is handled. `openedAt` is when the question was
 *  painted, `Infinity` until then (`App.tsx`'s `answerableFromRef`). `lastKeyAt` is the press of the last non-modifier
 *  keydown strictly BEFORE this one -- see `App.tsx`'s document-capture listener, which records it for every keydown
 *  in the panel before any routing, so a key typed into any handler (the composer, a card's reason box, the leader
 *  engine) counts, not just ones this module itself sees. */
export function bypassYesCounts({ now, openedAt, lastKeyAt }: { now: number; openedAt: number; lastKeyAt: number }): boolean {
  return now - openedAt >= BYPASS_YES_GUARD_MS && now - lastKeyAt >= BYPASS_YES_GUARD_MS;
}
