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

export function runScroll(action: string): boolean {
  const steps: Record<string, number> = {
    "scroll.down": 60, "scroll.up": -60,
    "scroll.half_down": window.innerHeight / 2, "scroll.half_up": -window.innerHeight / 2,
  };
  if (Object.hasOwnProperty.call(steps, action)) {
    window.scrollBy({ top: steps[action], behavior: "smooth" });
    return true;
  }
  switch (action) {
    case "scroll.top": window.scrollTo({ top: 0, behavior: "smooth" }); return true;
    case "scroll.bottom":
      window.scrollTo({ top: Math.max(document.documentElement.scrollHeight, document.body.scrollHeight), behavior: "smooth" });
      return true;
    case "history.back": window.history.back(); return true;
    case "history.forward": window.history.forward(); return true;
    case "link.prev": return follow("prev");
    case "link.next": return follow("next");
    default: return false;
  }
}
