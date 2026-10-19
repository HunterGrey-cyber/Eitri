import type { Mode } from "./rows";

let mode: Mode = "normal";
let pending = "";
let message = "";
let dimmed = false;
const MODES: Record<Mode, string> = { normal: "普通", hint: "提示", insert: "输入" };

function render(): void {
  let hud = document.getElementById("eitri-keys-hud");
  if (!hud) {
    hud = document.createElement("div");
    hud.id = "eitri-keys-hud";
    hud.setAttribute("role", "status");
    hud.style.cssText = "position:fixed;bottom:0;left:0;right:0;z-index:2147483647;padding:4px 12px;" +
      "background:#20252b;color:#f4f4f4;font:13px monospace;pointer-events:none;white-space:pre-wrap";
    document.body.append(hud);
  }
  if (!document.getElementById("eitri-keys-style")) {
    const style = document.createElement("style");
    style.id = "eitri-keys-style";
    style.textContent = ".eitri-keys-current{outline:2px solid #cfa85d;outline-offset:3px}" +
      ".eitri-keys-current.eitri-keys-dimmed{outline-color:#8888}";
    document.head.append(style);
  }
  hud.dataset.mode = mode;
  hud.dataset.dimmed = String(dimmed);
  hud.hidden = dimmed;
  hud.textContent = [MODES[mode], pending, message].filter(Boolean).join("  ");
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
