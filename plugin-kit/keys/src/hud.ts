import type { Mode } from "./rows";
import { ensureStyle } from "./style";

let mode: Mode = "normal";
let pending = "";
let message = "";
let status = "";
let dimmed = false;
const MODES: Record<Mode, string> = { normal: "NORMAL", hint: "HINT", insert: "INSERT" };
let modeWords = { ...MODES };

function render(): void {
  let hud = document.getElementById("eitri-keys-hud");
  if (!hud) {
    hud = document.createElement("div");
    hud.id = "eitri-keys-hud";
    hud.setAttribute("role", "status");
    for (const name of ["mode", "pending", "message", "status"]) {
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
  hud.querySelector(".eitri-keys-message")!.textContent = message;
  hud.querySelector(".eitri-keys-status")!.textContent = status;
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
