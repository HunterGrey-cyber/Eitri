import type { Row, GenericAction, KitLabels } from "./index";
export type { Row, Mode } from "./index";

export const DEFAULT_PAGE_LABEL = "page";

const DEFAULT_ROWS: (Row & { action: GenericAction })[] = [
  { action: "scroll.down", keys: ["j", "<C-e>"], context: DEFAULT_PAGE_LABEL, desc: "scroll down" },
  { action: "scroll.up", keys: ["k", "<C-y>"], context: DEFAULT_PAGE_LABEL, desc: "scroll up" },
  { action: "scroll.half_down", keys: ["d", "<C-d>"], context: DEFAULT_PAGE_LABEL, desc: "half a page down" },
  { action: "scroll.half_up", keys: ["u", "<C-u>"], context: DEFAULT_PAGE_LABEL, desc: "half a page up" },
  { action: "scroll.page_down", keys: ["<C-f>"], context: DEFAULT_PAGE_LABEL, desc: "a page down" },
  { action: "scroll.page_up", keys: ["<C-b>"], context: DEFAULT_PAGE_LABEL, desc: "a page up" },
  { action: "scroll.top", keys: ["gg"], context: DEFAULT_PAGE_LABEL, desc: "top of the page" },
  { action: "scroll.bottom", keys: ["G"], context: DEFAULT_PAGE_LABEL, desc: "bottom of the page" },
  { action: "history.back", keys: ["H", "<C-o>"], context: DEFAULT_PAGE_LABEL, desc: "back" },
  { action: "history.forward", keys: ["L", "<C-i>"], context: DEFAULT_PAGE_LABEL, desc: "forward" },
  { action: "link.prev", keys: ["[["], context: DEFAULT_PAGE_LABEL, desc: "previous page" },
  { action: "link.next", keys: ["]]"], context: DEFAULT_PAGE_LABEL, desc: "next page" },
  { action: "find.start", keys: ["/"], context: DEFAULT_PAGE_LABEL, desc: "find in the page" },
  { action: "find.next", keys: ["n"], context: DEFAULT_PAGE_LABEL, desc: "next match" },
  { action: "find.prev", keys: ["N"], context: DEFAULT_PAGE_LABEL, desc: "previous match" },
  { action: "hint.start", keys: ["f"], context: DEFAULT_PAGE_LABEL, desc: "hint mode" },
  { action: "sheet.open", keys: ["?"], context: DEFAULT_PAGE_LABEL, desc: "this sheet" },
  { action: "mode.escape", keys: ["<Esc>"], context: DEFAULT_PAGE_LABEL, desc: "leave a mode" },
  { action: "visual.char", keys: ["v"], context: DEFAULT_PAGE_LABEL, desc: "visual mode" },
  { action: "visual.line", keys: ["V"], context: DEFAULT_PAGE_LABEL, desc: "visual line mode" },
];

export function genericRows(labels?: KitLabels): Row[] {
  return DEFAULT_ROWS.map((row) => ({
    ...row,
    keys: [...row.keys],
    context: labels?.page ?? DEFAULT_PAGE_LABEL,
    desc: labels?.generic?.[row.action] ?? row.desc,
  }));
}

export const GENERIC_ROWS: Row[] = genericRows();

export function isGenericAction(action: string): action is GenericAction {
  return DEFAULT_ROWS.some((row) => row.action === action);
}
