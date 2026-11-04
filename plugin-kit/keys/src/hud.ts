import { onRemove } from "./bridge";
import type { Mode } from "./rows";
import { ensureStyle } from "./style";

let mode: Mode = "normal";
let pending = "";
let message = "";
let whichKey = "";
let status = "";
let dimmed = false;
const MODES: Record<Mode, string> = {
  normal: "NORMAL", hint: "HINT", insert: "INSERT", visual: "VISUAL", visual_line: "VISUAL LINE",
};
let modeWords = { ...MODES };
/** What the HUD last showed. A held key renders on every repeat, so a render that would show the same thing writes
 *  nothing: no style or layout work on a fixed bar thirty times a second. */
let shown: string | null = null;

onRemove(() => {
  mode = "normal";
  pending = "";
  message = "";
  whichKey = "";
  status = "";
  dimmed = false;
  modeWords = { ...MODES };
  shown = null;
  document.getElementById("eitri-keys-hud")?.remove();
});

function render(): void {
  let hud = document.getElementById("eitri-keys-hud");
  const next = JSON.stringify([mode, pending, whichKey, message, status, dimmed, modeWords[mode]]);
  if (hud && next === shown) {
    ensureStyle();
    return;
  }
  if (!hud) {
    hud = document.createElement("div");
    hud.id = "eitri-keys-hud";
    hud.setAttribute("role", "status");
    for (const name of ["mode", "pending", "whichkey", "message", "status"]) {
      const span = document.createElement("span");
      span.className = `eitri-keys-${name}`;
      hud.append(span);
    }
    document.body.append(hud);
  }
  ensureStyle();
  hud.dataset.mode = mode;
  hud.dataset.dimmed = String(dimmed);
  hud.hidden = dimmed;
  const modeSpan = hud.querySelector<HTMLElement>(".eitri-keys-mode")!;
  modeSpan.dataset.mode = mode;
  modeSpan.textContent = modeWords[mode];
  hud.querySelector(".eitri-keys-pending")!.textContent = pending;
  hud.querySelector(".eitri-keys-whichkey")!.textContent = whichKey;
  const messageSpan = hud.querySelector<HTMLElement>(".eitri-keys-message")!;
  messageSpan.textContent = message;
  // The continuations take the message's room while a prefix waits; the message comes back with the prefix's end.
  messageSpan.hidden = whichKey !== "";
  hud.querySelector(".eitri-keys-status")!.textContent = status;
  shown = next;
}

export function showMode(next: Mode, prefix: string): void {
  mode = next;
  pending = prefix;
  render();
}

export function showMessage(text: string): void {
  message = text;
  render();
}

/** What may follow the pending keys; `""` hides the line and brings the message back. */
export function showWhichKey(text: string): void {
  whichKey = text;
  render();
}

export function setDimmed(value: boolean): void {
  dimmed = value;
  render();
}

export function showStatus(text: string): void {
  status = text;
  render();
}

export function forgetStatus(): void {
  status = "";
}

export function setModeWords(words?: Partial<Record<Mode, string>>): void {
  modeWords = { ...MODES, ...words };
  render();
}
