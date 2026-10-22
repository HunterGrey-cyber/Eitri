// The shapes of the keyboard kit's public interface, as the kit publishes them on `window.eitriKeys`.
export type Row = { action: string; keys: string[]; context: string; desc: string };
export type Mode = "normal" | "hint" | "insert";
export type HintTarget = { el: Element; rank?: number };
export type GenericAction =
  | "scroll.down" | "scroll.up" | "scroll.half_down" | "scroll.half_up" | "scroll.page_down" | "scroll.top"
  | "scroll.bottom" | "history.back" | "history.forward" | "link.prev" | "link.next" | "find.start" | "find.next"
  | "find.prev" | "hint.start" | "sheet.open" | "mode.escape";
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
  find?: { root?: () => Element | null; exclude?: string };
};

export interface EitriKeys {
  version: number;
  install(opts: InstallOptions): Promise<() => void>;
  setCurrent(el: Element | null): void;
  message(text: string): void;
  status(text: string): void;
  keys(action: string): string[];
  findRefresh(): void;
  findSeed(query: string): void;
}
/** The kit the host serves at /.eitri/keys.js, or null when the page runs without it (a test, a browser). */
export function kit(): EitriKeys | null {
  return (window as unknown as { eitriKeys?: EitriKeys }).eitriKeys ?? null;
}
