import { onRemove } from "./bridge";
const NATIVE_CONTROL = "button, a, summary, input, select, textarea, [role=button], [role=tab], [role=option], [role=checkbox], [role=switch]";

/** `spaceToKeys`: the page binds a sequence that starts with Space, so Space on a focused control belongs to the
 *  keys; Enter still activates the control, so every control stays reachable. */
export function blockedEvent(ev: KeyboardEvent, heldSuper: boolean, spaceToKeys = false): boolean {
  const focused = ev.target instanceof Element ? ev.target : document.activeElement;
  return ev.defaultPrevented || ev.isComposing || ev.keyCode === 229 || heldSuper ||
    ev.getModifierState("Super") || ev.getModifierState("Hyper") ||
    ((ev.key === "Enter" || (ev.key === " " && !spaceToKeys)) && !!focused?.closest(NATIVE_CONTROL));
}

export function shouldSkip(ev: KeyboardEvent, heldSuper: boolean, spaceToKeys = false): boolean {
  return blockedEvent(ev, heldSuper, spaceToKeys) || ev.ctrlKey || ev.altKey || ev.metaKey;
}

/** Esc, or vim's other spelling of it, Ctrl+[, which a keyboard without a reachable Esc key relies on. */
export function isEscape(ev: KeyboardEvent): boolean {
  if (ev.altKey || ev.metaKey) return false;
  return ev.key === "Escape" ? !ev.ctrlKey : ev.key === "[" && ev.ctrlKey && !ev.shiftKey;
}

// Input types that take typed text. A checkbox, radio, button, range, colour or file input takes no text, so focusing
// one (a label click focuses it) must not turn the page's keys off.
function textInputs(): Set<string> {
  return new Set([
    "text", "search", "email", "url", "tel", "password", "number", "date", "datetime-local", "month", "time", "week",
  ]);
}

let TEXT_INPUTS = textInputs();
onRemove(() => { TEXT_INPUTS = textInputs(); });

export function textField(el: Element | null): HTMLElement | null {
  if (!(el instanceof HTMLElement)) return null;
  if (el instanceof HTMLInputElement) return TEXT_INPUTS.has(el.type) ? el : null;
  if (el.matches("textarea, select")) return el;
  const editable = el.closest<HTMLElement>("[contenteditable]");
  return el.isContentEditable || (editable && editable.getAttribute("contenteditable") !== "false") ? el : null;
}
