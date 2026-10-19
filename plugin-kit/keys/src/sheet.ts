import type { Row } from "./rows";

let restoreFocus: HTMLElement | null = null;

export function closeSheet(): boolean {
  const sheet = document.getElementById("eitri-keys-sheet");
  if (!sheet) return false;
  sheet.remove();
  if (restoreFocus?.isConnected) restoreFocus.focus();
  restoreFocus = null;
  return true;
}

export function sheetIsOpen(): boolean {
  return document.getElementById("eitri-keys-sheet") !== null;
}

export function openSheet(rows: Row[]): void {
  closeSheet();
  restoreFocus = document.activeElement instanceof HTMLElement ? document.activeElement : null;
  const sheet = document.createElement("section");
  sheet.id = "eitri-keys-sheet";
  sheet.setAttribute("role", "dialog");
  sheet.setAttribute("aria-label", "按键帮助");
  sheet.tabIndex = -1;
  sheet.style.cssText = "position:fixed;inset:8%;z-index:2147483646;overflow:auto;padding:24px;" +
    "background:#20252b;color:#f4f4f4;border:1px solid #888;font:14px sans-serif";
  const title = document.createElement("h2");
  title.textContent = "按键帮助";
  sheet.append(title);
  for (const row of rows) {
    if (!row.keys.length) continue;
    const line = document.createElement("p");
    const keys = document.createElement("kbd");
    keys.textContent = row.keys.join(" / ");
    line.append(keys, document.createTextNode(`  ${row.context}  ${row.desc}`));
    sheet.append(line);
  }
  const footer = document.createElement("p");
  footer.textContent = "Esc 关闭 · Ctrl+h / Ctrl+l 切换模块 · 前缀键后按插件模块键返回本页";
  const close = document.createElement("button");
  close.textContent = "关闭";
  close.addEventListener("click", closeSheet);
  sheet.append(footer, close);
  document.body.append(sheet);
  sheet.focus();
}
