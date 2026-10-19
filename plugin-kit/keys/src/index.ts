import { install as installPage, setCurrent as setPageCurrent } from "./install";
import { showMessage } from "./hud";
import { GENERIC_ROWS as genericRows } from "./rows";

// Keep the public types here so the emitted declaration travels with the single script.
export type Row = { action: string; keys: string[]; context: string; desc: string };
export type Mode = "normal" | "hint" | "insert";
export type KeyStroke = { key: string; ctrl: boolean; shift: boolean };
export type KeySeq = KeyStroke[];
export type HintTarget = { el: Element; rank?: number };
export type InstallOptions = {
  rows: Row[];
  run(action: string, ev: KeyboardEvent): boolean;
  targets?: () => HintTarget[];
  initial?: { timeoutlen: number; hasKeys: boolean };
};

export const GENERIC_ROWS: Row[] = genericRows;
export const install: (opts: InstallOptions) => Promise<() => void> = installPage;
export const setCurrent: (el: Element | null) => void = setPageCurrent;
export const message: (text: string) => void = showMessage;

export const eitriKeys = { version: 1 as const, install, setCurrent, message };

declare global {
  interface Window { eitriKeys: typeof eitriKeys }
}

window.eitriKeys = eitriKeys;
