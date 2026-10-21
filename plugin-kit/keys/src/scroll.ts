import { glideAction } from "./glide";

const PREVIOUS = /\b(?:prev|previous|back|older)\b|[<‹←«]|上一/i;
const NEXT = /\b(?:next|forward|newer)\b|[>›→»]|下一/i;

function follow(direction: "prev" | "next"): boolean {
  const rel = document.querySelector<HTMLAnchorElement>(`a[rel~="${direction}"][href]`);
  const pattern = direction === "prev" ? PREVIOUS : NEXT;
  const link = rel ?? [...document.querySelectorAll<HTMLAnchorElement>("a[href]")]
    .find((candidate) => pattern.test(candidate.textContent ?? ""));
  if (!link) return false;
  link.click();
  return true;
}

/** Runs a generic scrolling or navigation action for the key event that completed it (`null`: no key). Scrolling
 * goes to the glide, which needs the event to tell a held key's repeats from presses. */
export function runScroll(action: string, ev: KeyboardEvent | null = null): boolean {
  if (action.startsWith("scroll.")) return glideAction(action, ev);
  switch (action) {
    case "history.back": window.history.back(); return true;
    case "history.forward": window.history.forward(); return true;
    case "link.prev": return follow("prev");
    case "link.next": return follow("next");
    default: return false;
  }
}
