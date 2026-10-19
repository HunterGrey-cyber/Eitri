import { install as installPage, setCurrent as setPageCurrent, keysOf } from "./install";
import { showMessage, showStatus } from "./hud";
import { GENERIC_ROWS as genericRows } from "./rows";

// Keep the public types here so the emitted declaration travels with the single script.
export type Row = { action: string; keys: string[]; context: string; desc: string };
export type Mode = "normal" | "hint" | "insert";
export type KeyStroke = { key: string; ctrl: boolean; shift: boolean };
export type KeySeq = KeyStroke[];
export type HintTarget = { el: Element; rank?: number };
export type GenericAction =
  | "scroll.down" | "scroll.up" | "scroll.half_down" | "scroll.half_up" | "scroll.top" | "scroll.bottom"
  | "history.back" | "history.forward" | "link.prev" | "link.next" | "hint.start" | "sheet.open" | "mode.escape";
export type KitLabels = {
  modes?: Partial<Record<Mode, string>>;
  page?: string;
  generic?: Partial<Record<GenericAction, string>>;
  sheetTitle?: string;
  sheetFooter?: string;
  sheetClose?: string;
};
export type InstallOptions = {
  rows: Row[];
  run(action: string, ev: KeyboardEvent): boolean;
  targets?: () => HintTarget[];
  initial?: { timeoutlen: number; hasKeys: boolean };
  labels?: KitLabels;
};

export const GENERIC_ROWS: Row[] = genericRows;
export const install: (opts: InstallOptions) => Promise<() => void> = installPage;
export const setCurrent: (el: Element | null) => void = setPageCurrent;
export const message: (text: string) => void = showMessage;

export const status: (text: string) => void = showStatus;
export const keys: (action: string) => string[] = keysOf;

export const eitriKeys = { version: 2 as const, install, setCurrent, message, status, keys };

declare global {
  interface Window { eitriKeys: typeof eitriKeys }
}

window.eitriKeys = eitriKeys;
