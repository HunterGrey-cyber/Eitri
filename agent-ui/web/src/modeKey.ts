import { isImeKey } from "./composerKeys";
import type { TabInfo } from "./types";

/** Claude Code's mode key (Shift+Tab cycles the permission mode), claimed everywhere in the chat so it never
 *  becomes WebKit's backward focus navigation or GTK's `move-focus`. */
export function isModeCycleKey(e: {
  key: string;
  shiftKey: boolean;
  ctrlKey: boolean;
  altKey: boolean;
  metaKey: boolean;
  isComposing: boolean;
  keyCode?: number;
}): boolean {
  if (isImeKey({ isComposing: e.isComposing, keyCode: e.keyCode ?? 0 })) return false;
  return e.key === "Tab" && e.shiftKey && !e.ctrlKey && !e.altKey && !e.metaKey;
}

export type ModeKeyState = { confirmOpen: boolean; chooserOpen: boolean; tabState: TabInfo["state"] | null };

/** A mode can be chosen before the session starts. After that it is fixed for the session. */
export function modeKeyRoute(s: ModeKeyState): "overlay" | "cycle" | "fixed" | "none" {
  if (s.confirmOpen || s.chooserOpen) return "overlay";
  if (s.tabState === null) return "none";
  return s.tabState === "not_started" ? "cycle" : "fixed";
}

export function modeFixedMessage(newTabChord: string): string {
  return newTabChord === ""
    ? "mode is fixed for this session — open a new tab to choose"
    : `mode is fixed for this session — ${newTabChord} for a new tab`;
}
