const NATIVE_CONTROL = "button, a, summary, input, select, textarea, [role=button], [role=tab], [role=option], [role=checkbox], [role=switch]";

export function blockedEvent(ev: KeyboardEvent, heldSuper: boolean): boolean {
  const focused = ev.target instanceof Element ? ev.target : document.activeElement;
  return ev.defaultPrevented || ev.isComposing || ev.keyCode === 229 || heldSuper ||
    ev.getModifierState("Super") || ev.getModifierState("Hyper") ||
    ((ev.key === "Enter" || ev.key === " ") && !!focused?.closest(NATIVE_CONTROL));
}

export function shouldSkip(ev: KeyboardEvent, heldSuper: boolean): boolean {
  return blockedEvent(ev, heldSuper) || ev.ctrlKey || ev.altKey || ev.metaKey;
}

export function textField(el: Element | null): HTMLElement | null {
  if (!(el instanceof HTMLElement)) return null;
  if (el.matches("input, textarea, select")) return el;
  const editable = el.closest<HTMLElement>("[contenteditable]");
  return el.isContentEditable || (editable && editable.getAttribute("contenteditable") !== "false") ? el : null;
}
