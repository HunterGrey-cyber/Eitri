import { isImeKey } from "./composerKeys";
import { TYPING_GUARD_MS } from "./typingGuard";
import type { PermissionModeChoice, TabInfo } from "./types";

/** Claude Code's mode key (Shift+Tab cycles the permission mode), claimed everywhere in the chat so it never
 *  becomes WebKit's backward focus navigation or GTK's `move-focus`. */
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

/** Recognize Shift+Tab when WebKitGTK reports the `ISO_Left_Tab` keysym as `key:
 *  "Unidentified"`, `code: "Tab"`, `keyCode: 9`. Checking `key === "Tab"` alone lets that event
 *  fall through to GTK focus movement. Keep the `key === "Tab"` case for keymaps and tests that
 *  deliver it. */
export function isShiftTab(e: { key: string; code?: string; shiftKey: boolean }): boolean {
  if (!e.shiftKey) return false;
  return e.key === "Tab" || (e.key === "Unidentified" && e.code === "Tab");
}

export type ModeKeyState = {
  confirmOpen: boolean;
  chooserOpen: boolean;
  tabState: TabInfo["state"] | null;
  /** The tab mode (`TabInfo.mode`) distinguishes ended or failed tabs that can leave bypass from
   *  those that cannot enter it. A tab already in bypass can leave it in every state. `null`
   *  before a tab exists, like `tabState`. */
  tabMode: PermissionModeChoice | null;
};

/** `not_started`, `starting` and `live` tabs can cycle modes without a provider capability check.
 *  Rust decides whether a cycle enters bypass and may request `confirm_bypass` instead.
 *  Ended and failed tabs can leave bypass but cannot enter it. */
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

/** Minimum prompt age and time since the last non-modifier keydown anywhere in the panel before
 *  `y` or `Y` can confirm `confirm_bypass`. Typed text, including keys that opened the prompt,
 *  must not accidentally enable bypass. */
export const BYPASS_YES_GUARD_MS = TYPING_GUARD_MS;

/** Check both the prompt age and the time since the previous non-modifier keydown. The caller
 *  supplies `performance.now()` rather than `event.timeStamp` so tests control the clock.
 *  `lastKeyAt` excludes this keydown and includes keys handled anywhere in the panel, as
 *  captured by `App.tsx` before routing. */
export function bypassYesCounts({ now, openedAt, lastKeyAt }: { now: number; openedAt: number; lastKeyAt: number }): boolean {
  return now - openedAt >= BYPASS_YES_GUARD_MS && now - lastKeyAt >= BYPASS_YES_GUARD_MS;
}
