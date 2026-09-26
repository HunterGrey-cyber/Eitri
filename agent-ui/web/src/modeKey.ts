import { isImeKey } from "./composerKeys";
import type { TabInfo } from "./types";

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
  /** Whether this sidecar supports changing a live session's permission mode
   *  (`capabilities.modeSwitch`). Without it, the mode is fixed after the first message. */
  canSwitch: boolean;
};

/** A mode can be chosen before the session starts, or while live if the sidecar supports switching.
 *  Starting tabs report that they are starting. Ended and failed sessions have a fixed mode. */
export function modeKeyRoute(s: ModeKeyState): "overlay" | "cycle" | "fixed" | "starting" | "none" {
  if (s.confirmOpen || s.chooserOpen) return "overlay";
  if (s.tabState === null) return "none";
  if (s.tabState === "not_started") return "cycle";
  if (s.tabState === "live" && s.canSwitch) return "cycle";
  if (s.tabState === "starting") return "starting";
  return "fixed";
}

export const MODE_STARTING_MESSAGE = "session is starting — try again once it is up";

export function modeFixedMessage(newTabChord: string): string {
  return newTabChord === ""
    ? "mode is fixed for this session — open a new tab to choose"
    : `mode is fixed for this session — ${newTabChord} for a new tab`;
}
