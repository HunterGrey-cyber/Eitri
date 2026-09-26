import { isImeKey } from "./composerKeys";
import type { TabInfo } from "./types";

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
  /** Verdandi `SetPermissionMode` (D6) is live for this tab's sidecar (`capabilities.modeSwitch`), so a live
   *  session can switch mid-session rather than being fixed after the first message. Wave 5. */
  canSwitch: boolean;
};

/** See the table in the wave-4 plan, Task 1, extended by wave 5 (ruling W5). A mode is chosen before a session's
 *  first message (ruling 5) unless the sidecar can switch, in which case a live tab keeps cycling; a starting
 *  tab says so rather than flashing the (false, on a switch-capable sidecar) fixed-mode text; ended/failed
 *  sessions are always fixed. */
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
