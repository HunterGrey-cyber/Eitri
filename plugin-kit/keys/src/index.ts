import { install as installPage, setCurrent as setPageCurrent, keysOf } from "./install";
import { showMessage, showStatus } from "./hud";
import { GENERIC_ROWS as genericRows } from "./rows";
import { refreshFind, seedFind } from "./find";

// Keep the public types here so the emitted declaration travels with the single script.
export type Row = { action: string; keys: string[]; context: string; desc: string };
export type Mode = "normal" | "hint" | "insert";
export type KeyStroke = { key: string; ctrl: boolean; shift: boolean };
export type KeySeq = KeyStroke[];
export type HintTarget = { el: Element; rank?: number };
export type GenericAction =
  | "scroll.down" | "scroll.up" | "scroll.half_down" | "scroll.half_up" | "scroll.page_down" | "scroll.top" | "scroll.bottom"
  | "history.back" | "history.forward" | "link.prev" | "link.next" | "find.start" | "find.next" | "find.prev"
  | "hint.start" | "sheet.open" | "mode.escape";
/** The HUD messages find shows; `{query}`, `{index}` and `{total}` are filled in. */
export type KitMessage = "find.notFound" | "find.none" | "find.wrapBottom" | "find.wrapTop" | "find.count";
export type KitGroup = "scroll" | "history" | "link" | "find" | "hint" | "sheet" | "mode";
export type KitLabels = {
  groups?: Partial<Record<KitGroup, string>>;
  modes?: Partial<Record<Mode, string>>;
  page?: string;
  generic?: Partial<Record<GenericAction, string>>;
  sheetTitle?: string;
  sheetFooter?: string;
  sheetClose?: string;
  messages?: Partial<Record<KitMessage, string>>;
};
export type InstallOptions = {
  rows: Row[];
  run(action: string, ev: KeyboardEvent): boolean;
  targets?: () => HintTarget[];
  initial?: { timeoutlen: number; hasKeys: boolean; prefix?: string | null; settings?: unknown };
  labels?: KitLabels;
  /** What `/` searches: the root's rendered text (default `document.body`), less the elements `exclude` matches. */
  find?: { root?: () => Element | null; exclude?: string };
};

export const GENERIC_ROWS: Row[] = genericRows;
export const install: (opts: InstallOptions) => Promise<() => void> = installPage;
export const setCurrent: (el: Element | null) => void = setPageCurrent;
export const message: (text: string) => void = showMessage;

export const status: (text: string) => void = showStatus;
export const keys: (action: string) => string[] = keysOf;
/** Runs the find again after the page changed its text; the current match stays the one nearest where it was. */
export const findRefresh: () => void = refreshFind;
/** Sets the search `n` and `N` walk without moving or highlighting, for a search the page already knows. */
export const findSeed: (query: string) => void = seedFind;

export const eitriKeys = { version: 3 as const, install, setCurrent, message, status, keys, findRefresh, findSeed };

declare global {
  interface Window { eitriKeys: typeof eitriKeys }
}

window.eitriKeys = eitriKeys;
