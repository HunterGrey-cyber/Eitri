import { isPlainAnswerKey } from "./keymap";
import { isShiftTab } from "./modeKey";

/** Where the plan dock's keys land: its row of actions (the plan scrolls from there), or the "keep planning"
 *  feedback field. */
export type PlanKeyZone = "actions" | "feedback";
export type PlanKeyIntent =
  /** Not the dock's: the key goes on to the panel (`Ctrl+y`, the prefix, a Super chord) or to the field. */
  | { type: "none" }
  /** The dock's, and it does nothing: a letter or digit never decides a plan, nor does a held key. */
  | { type: "swallow" }
  | { type: "focus"; step: 1 | -1 }
  | { type: "scroll"; by: "line" | "half" | "edge"; step: 1 | -1 }
  /** Enter on the focused action; the dock still waits out its paint-and-key guard. */
  | { type: "activate" }
  /** Ctrl+Enter in the feedback field: keep planning with what it says. */
  | { type: "send" }
  /** Esc in the feedback field: back to the actions, the text kept. */
  | { type: "back" }
  /** Esc on the actions: back to BROWSE, the plan still waiting. */
  | { type: "browse" }
  | { type: "help" }
  /** `g`: the first half of `gg`, which scrolls to the top. */
  | { type: "g" };

type Key = Pick<KeyboardEvent, "key" | "keyCode" | "repeat" | "metaKey" | "ctrlKey" | "altKey" | "shiftKey" | "isComposing"> & {
  code?: string;
  getModifierState?: (key: string) => boolean;
};

/** The plan dock's keys, vim's and Claude Code's: Tab/Shift+Tab and h/l move between the actions, j/k, Ctrl-d/Ctrl-u
 *  and gg/G scroll the plan, Enter activates the focused action. No other key decides anything, and nothing reaches an
 *  action under Super, Hyper, AltGraph, Alt or Meta, while an input method composes, or from a held key. */
export function planKey(event: Key, zone: PlanKeyZone): PlanKeyIntent {
  const plain = isPlainAnswerKey({ key: event.key, ctrlKey: false, shiftKey: event.shiftKey, altKey: event.altKey,
    metaKey: event.metaKey, isComposing: event.isComposing, keyCode: event.keyCode,
    getModifierState: event.getModifierState?.bind(event) });
  if (!plain) return { type: "none" };
  if (zone === "feedback") {
    if (event.ctrlKey && event.key === "Enter" && !event.repeat) return { type: "send" };
    if (!event.ctrlKey && !event.repeat && (event.key === "Escape" || event.key === "Tab" || isShiftTab(event))) return { type: "back" };
    return { type: "none" };
  }
  if (event.ctrlKey) {
    const letter = event.key.toLowerCase();
    if (letter === "d" || letter === "u") return { type: "scroll", by: "half", step: letter === "d" ? 1 : -1 };
    return { type: "none" };
  }
  // Scrolling may repeat; nothing else does: a key held from before the plan arrived never reaches an action.
  if (["j", "ArrowDown", "k", "ArrowUp"].includes(event.key) && !event.shiftKey) {
    return { type: "scroll", by: "line", step: event.key === "j" || event.key === "ArrowDown" ? 1 : -1 };
  }
  if (event.repeat) return { type: "swallow" };
  if (event.key === "Tab" || isShiftTab(event)) return { type: "focus", step: event.shiftKey ? -1 : 1 };
  if (!event.shiftKey && ["h", "ArrowLeft", "l", "ArrowRight"].includes(event.key)) {
    return { type: "focus", step: event.key === "h" || event.key === "ArrowLeft" ? -1 : 1 };
  }
  if (event.key === "g" && !event.shiftKey) return { type: "g" };
  if (event.key === "G" || event.key === "Home" || event.key === "End") {
    return { type: "scroll", by: "edge", step: event.key === "Home" ? -1 : 1 };
  }
  if (event.key === "Enter" && !event.shiftKey) return { type: "activate" };
  if (event.key === "Escape") return { type: "browse" };
  if (event.key === "F1" && !event.shiftKey) return { type: "help" };
  return { type: "swallow" };
}
