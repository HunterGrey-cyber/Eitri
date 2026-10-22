import { onRemove } from "./bridge";
import type { KitGroup, KitLabels } from "./index";
import { isGenericAction, type Row } from "./rows";
import { currentSettings } from "./settings";
import { ensureStyle } from "./style";

let pendingG = false;
let restoreFocus: HTMLElement | null = null;

export function closeSheet(): boolean {
  pendingG = false;
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

export function sheetKey(key: string): boolean {
  const sheet = document.getElementById("eitri-keys-sheet");
  const wasG = pendingG;
  pendingG = false;
  if (!sheet) return false;
  switch (key) {
    case "q": case "?": closeSheet(); break;
    case "j": sheet.scrollTop += currentSettings().scrollStep; break;
    case "k": sheet.scrollTop -= currentSettings().scrollStep; break;
    case "d": sheet.scrollTop += Math.floor(sheet.clientHeight / 2); break;
    case "u": sheet.scrollTop -= Math.floor(sheet.clientHeight / 2); break;
    case "G": sheet.scrollTop = sheet.scrollHeight; break;
    case "g":
      if (wasG) sheet.scrollTop = 0;
      else pendingG = true;
      break;
    default: return false;
  }
  return true;
}

const GROUPS: KitGroup[] = ["scroll", "history", "link", "find", "hint", "sheet", "mode"];
const GROUP_LABELS: Record<KitGroup, string> = {
  scroll: "scroll", history: "history", link: "pages", find: "find", hint: "hints", sheet: "help", mode: "leave",
};

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
  const contexts = new Map<string, Row[]>();
  for (const row of rows) {
    if (!row.keys.length || isGenericAction(row.action)) continue;
    if (!contexts.has(row.context)) contexts.set(row.context, []);
    contexts.get(row.context)!.push(row);
  }
  for (const [context, contextRows] of contexts) {
    if (context) {
      const heading = document.createElement("h3");
      heading.className = "eitri-keys-sheet-context";
      heading.textContent = context;
      sheet.append(heading);
    }
    for (const row of contextRows) {
      const line = document.createElement("p");
      line.className = "eitri-keys-sheet-row";
      const keys = document.createElement("kbd");
      keys.textContent = row.keys.join(" / ");
      line.append(keys, document.createTextNode(`  ${row.desc}`));
      sheet.append(line);
    }
  }
  const folded = rows.filter((row) => row.keys.length && isGenericAction(row.action));
  if (folded.length) {
    const line = document.createElement("p");
    line.className = "eitri-keys-sheet-row eitri-keys-sheet-generic";
    line.textContent = GROUPS.flatMap((group) => {
      const grouped = folded.filter((row) => row.action.split(".")[0] === group);
      return grouped.length
        ? [`${labels?.groups?.[group] ?? GROUP_LABELS[group]} ${grouped.map((row) => row.keys.join(" / ")).join(" ")}`]
        : [];
    }).join(" · ");
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

onRemove(() => {
  closeSheet();
  restoreFocus = null;
});
