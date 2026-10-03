import { isImeKey } from "./composerKeys";
import { TYPING_GUARD_MS } from "./typingGuard";
import type { PermissionModeChoice, TabInfo } from "./types";

/** Claude Code's mode key (Shift+Tab cycles the permission mode), claimed everywhere in the chat so it never
 *  becomes WebKit's backward focus navigation or GTK's `move-focus` (owner, 2026-09-26: "在agent pane都要可以直接切换"). */
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
 *  `key: "Unidentified"`, `code: "Tab"`, `keyCode: 9` -- never `key: "Tab"` (seen in the wave-4 GUI
 *  pass, 2026-09-26, with a virtual keyboard carrying the real `us` keymap). Checking `key === "Tab"`
 *  alone therefore never matched on a real keyboard, and the key fell through to GTK's `move-focus`.
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
  /** The tab's own mode (`TabInfo.mode`), read only to settle the `ended`/`failed` case (v1 spec
   *  D6): entering bypass there is refused, but a tab that is ALREADY in bypass can still leave it
   *  in every state, so `ended`/`failed` is not simply "fixed" any more. `null` before any tab
   *  exists yet, the same as `tabState`. */
  tabMode: PermissionModeChoice | null;
};

/** v1 (spec `docs/superpowers/specs/2026-09-27-v1-mode-design.md`, D6, replacing wave 5's
 *  `canSwitch`/`SetPermissionMode` gate, which Task 1 removes entirely): `not_started`, `starting`
 *  and `live` all always cycle now -- there is no provider capability left to gate on, since every
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
 *  action, so there is nothing on the wire to read it from; unlike `newTabChord` (which this
 *  parameter replaces at both call sites) it is never actually empty in practice, but the same
 *  optional shape is kept so a caller with none at all still gets a sensible message. */
export function modeFixedMessage(resetKey: string): string {
  return resetKey === ""
    ? "the session has ended — start a new one to change the mode"
    : `the session has ended — ${resetKey} to start again`;
}

/** D11: how long a `confirm_bypass` prompt must have been on screen, and how long since the last
 *  non-modifier keydown ANYWHERE in the panel, before a `y`/`Y` counts as answering it -- typed text
 *  (even the very keys that triggered the prompt, like `<leader>m`) must never fall through into
 *  bypass. It IS `typingGuard.ts#TYPING_GUARD_MS` (v1 S1): one number for "typed text is still
 *  arriving", unified when v1-mode merged onto v1-ui. */
export const BYPASS_YES_GUARD_MS = TYPING_GUARD_MS;

/** Both halves of D11's guard, pure and clock-injected (`performance.now()` at the call site, never
 *  `event.timeStamp`, so tests can control it). `lastKeyAt` is the last non-modifier keydown
 *  strictly BEFORE this one -- see `App.tsx`'s document-capture listener, which records it for every
 *  keydown in the panel before any routing, so a key typed into any handler (the composer, a card's
 *  reason box, the leader engine) counts, not just ones this module itself sees. */
export function bypassYesCounts({ now, openedAt, lastKeyAt }: { now: number; openedAt: number; lastKeyAt: number }): boolean {
  return now - openedAt >= BYPASS_YES_GUARD_MS && now - lastKeyAt >= BYPASS_YES_GUARD_MS;
}
