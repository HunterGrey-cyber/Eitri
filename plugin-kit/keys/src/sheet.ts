import type { KitLabels } from "./index";
import { DEFAULT_PAGE_LABEL, isGenericAction, type Row } from "./rows";
import { ensureStyle } from "./style";

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

export function openSheet(rows: Row[], labels?: KitLabels): void {
  ensureStyle();
  closeSheet();
  restoreFocus = document.activeElement instanceof HTMLElement ? document.activeElement : null;
  const sheet = document.createElement("section");
  sheet.id = "eitri-keys-sheet";
  sheet.setAttribute("role", "dialog");
  sheet.setAttribute("aria-label", labels?.sheetTitle ?? "Keys");
  sheet.tabIndex = -1;
  const title = document.createElement("h2");
  title.textContent = labels?.sheetTitle ?? "Keys";
  sheet.append(title);
  for (const row of rows) {
    if (!row.keys.length || isGenericAction(row.action)) continue;
    const line = document.createElement("p");
    line.className = "eitri-keys-sheet-row";
    const keys = document.createElement("kbd");
    keys.textContent = row.keys.join(" / ");
    line.append(keys, document.createTextNode(`  ${row.context}  ${row.desc}`));
    sheet.append(line);
  }
  const folded = rows.filter((row) => row.keys.length && isGenericAction(row.action));
  if (folded.length) {
    const context = folded[0].context;
    const label = context && folded.every((row) => row.context === context)
      ? context : labels?.page ?? DEFAULT_PAGE_LABEL;
    const line = document.createElement("p");
    line.className = "eitri-keys-sheet-row";
    const keys = document.createElement("kbd");
    keys.textContent = folded.map((row) => row.keys.join(" / ")).join(" ");
    line.append(keys, document.createTextNode(`  ${label}`));
    sheet.append(line);
  }
  const footer = document.createElement("p");
  footer.className = "eitri-keys-sheet-muted";
  footer.textContent = labels?.sheetFooter ?? "Esc closes · Ctrl+h / Ctrl+l move between modules";
  const close = document.createElement("button");
  close.textContent = labels?.sheetClose ?? "Close";
  close.addEventListener("click", closeSheet);
  sheet.append(footer, close);
  document.body.append(sheet);
  sheet.focus();
}
