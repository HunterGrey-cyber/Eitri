import { onRemove } from "./bridge";
import type { KeySeq, KeyStroke } from "./index";
export type { KeySeq, KeyStroke } from "./index";

const NAMED: Record<string, string> = {
  Esc: "Escape", CR: "Enter", BS: "Backspace", Left: "ArrowLeft", Right: "ArrowRight",
  Up: "ArrowUp", Down: "ArrowDown", Space: " ", Tab: "Tab", Home: "Home", End: "End", Del: "Delete", lt: "<",
};

function names(): Map<string, string> {
  const names = new Map(Object.entries(NAMED).map(([name, key]) => [name.toLowerCase(), key]));
  names.set("enter", "Enter");
  names.set("return", "Enter");
  names.set("bslash", "\\");
  names.set("bar", "|");
  return names;
}

let NAMES = names();
onRemove(() => { NAMES = names(); });

function shifted(key: string): boolean {
  return /^[A-Z~!@#$%^&*()_+{}|:"<>?]$/.test(key);
}

export function parseNotation(text: string): KeySeq | null {
  const result: KeySeq = [];
  while (text.length > 0) {
    let key: string;
    let ctrl = false;
    let shift = false;
    if (text.startsWith("<") && text.includes(">")) {
      const end = text.indexOf(">");
      let token = text.slice(1, end);
      while (/^[cs]-/i.test(token)) {
        if (/^c-/i.test(token)) { if (ctrl) return null; ctrl = true; }
        else { if (shift) return null; shift = true; }
        token = token.slice(2);
      }
      const named = NAMES.get(token.toLowerCase());
      key = named ?? token;
      if (!named && !/^[\x21-\x7e]$/.test(token)) return null;
      if (ctrl && /^[A-Z]$/.test(key)) { shift = true; key = key.toLowerCase(); }
      if (shift && !ctrl && /^[a-z]$/.test(key)) key = key.toUpperCase();
      text = text.slice(end + 1);
    } else {
      key = [...text][0];
      if (/\s|[\x00-\x1f\x7f]/u.test(key)) return null;
      text = text.slice(key.length);
    }
    result.push({ key, ctrl, shift: shift || shifted(key) });
    if (result.length > 10) return null;
  }
  return result.length > 0 ? result : null;
}

export function strokeOf(ev: KeyboardEvent): KeyStroke {
  const key = ev.ctrlKey && /^[A-Z]$/i.test(ev.key) ? ev.key.toLowerCase() : ev.key;
  return { key, ctrl: ev.ctrlKey, shift: ev.ctrlKey ? ev.shiftKey : shifted(key) || ev.shiftKey };
}

export function notationOf(stroke: KeyStroke): string {
  const named = Object.entries(NAMED).find(([, key]) => key === stroke.key)?.[0];
  const modifiers = `${stroke.ctrl ? "C-" : ""}${stroke.shift && (stroke.ctrl || !shifted(stroke.key)) ? "S-" : ""}`;
  return named || modifiers ? `<${modifiers}${named ?? stroke.key}>` : stroke.key;
}

/** One spelling for aliases and modifier names, so declarations and bindings compare the same keys. */
export function canonicalOf(notation: string): string | null {
  return parseNotation(notation)?.map(notationOf).join("") ?? null;
}
