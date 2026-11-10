import type { SlashPickerKind } from "../components/SlashPicker";
import type { AppliedSettings, EffortChoice, ModelOption, TabId } from "../types";
import type { Caret } from "../visual";

/** Whether `el` is an ordinary editable control -- an `<input>`, a `<textarea>`, or anything
 *  `contenteditable`. Used to decide what the panel's own keydown handlers must stay out of: this
 *  panel does not own every keystroke inside its own subtree, only the ones outside a text field
 *  someone is actually typing into. Not scoped to any one component on purpose -- a permission
 *  card's reason box is the one that exists today, but the rule is general and must hold for
 *  whatever text field a later card adds too. */
export function isEditableElement(el: EventTarget | null): el is HTMLElement {
  if (!(el instanceof HTMLElement)) return false;
  return el.tagName === "INPUT" || el.tagName === "TEXTAREA" || el.isContentEditable;
}

/** What an open `/model`/`/effort` picker shows. */
export type SlashPickerState = { kind: SlashPickerKind; options: string[]; current: string | null; labels?: Record<string, string>; direct?: boolean };
/** A request of the direct route for the model and the effort, until its answer: the list a picker opens from, or a
 *  change, with what the words about it need from the moment it was made. */
export type SettingsRequest =
  | { what: "list"; tab: TabId; kind: SlashPickerKind }
  | { what: "typed-effort"; tab: TabId; level: string; text: string }
  | { what: "change"; tab: TabId; kind: SlashPickerKind; chosen: string; running: { model: string | null } | null };

/** What answers a `SettingsRequest`. */
export type SettingsAnswer =
  | { kind: "models_result"; models: ModelOption[]; applied: AppliedSettings; efforts?: EffortChoice[] }
  | { kind: "settings_result"; applied: AppliedSettings }
  | { kind: "command_result"; ok: true }
  | { kind: "command_result"; ok: false; error: string };

/** Envelopes that move the keys, or put something over the conversation, with no keydown this panel
 *  sees: each drops a waiting `a`/`d`/`D` (the typing guard's cancel list -- `pane_focus`, an arrival,
 *  an overlay). `nav_key` is `Ctrl+j`/`Ctrl+k`, which GTK claims before the page sees a key; a Set of
 *  strings, so naming it needs no type from elsewhere.
 *  **The mode cycle key (Shift+Tab) is on that cancel list too, but is NOT a member of this Set**: it is a real
 *  keydown, not an inbound envelope with no keydown of its own, so it cannot be named here -- `onModeKey`'s
 *  document-capture handler calls `typingGuard.onKey` directly instead (not a bare `cancel()`, which would drop a
 *  pending answer but never record the key itself, leaving Shift+Tab invisible to the guard's own "before" half),
 *  since its own `stopPropagation` (needed to keep Shift+Tab from WebKit's backward focus navigation) means the
 *  bubble-phase `onKeyDown` that feeds every OTHER key to `typingGuard.onKey` never runs for it. */
export const CANCELS_WAITING_ANSWER = new Set(["pane_focus", "arrive", "enter_input", "focus_permission", "hint_collect", "nav_key"]);

/** Whether `caret` has no usable client rect at all (jsdom, or a node with nothing laid out) --
 *  `scrollCaretIntoView`'s cue to fall back to the element's own `scrollIntoView`. */
export function caretRectMissing(caret: Caret): boolean {
  if (typeof Range === "undefined" || typeof Range.prototype.getBoundingClientRect !== "function") return true;
  const range = document.createRange();
  try {
    range.setStart(caret.node, caret.offset);
  } catch {
    return true;
  }
  range.collapse(true);
  const rect = range.getBoundingClientRect();
  return rect.width === 0 && rect.height === 0 && rect.top === 0 && rect.left === 0;
}

