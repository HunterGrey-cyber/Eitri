import { install as installPage, setCurrent as setPageCurrent, keysOf } from "./install";
import { showMessage, showStatus } from "./hud";
import { GENERIC_ROWS as genericRows } from "./rows";
import { refreshFind, seedFind } from "./find";
import { scrollToAnimated, settleGlide, windowDestination } from "./glide";
import { endVisual as endVisualMode, visualKind } from "./visual";

// Keep the public types here so the emitted declaration travels with the single script.
export type Row = { action: string; keys: string[]; context: string; desc: string };
export type Mode = "normal" | "hint" | "insert" | "visual" | "visual_line";
/** Visual mode's kind: charwise (`v`) or linewise (`V`). */
export type VisualKind = "char" | "line";
export type KeyStroke = { key: string; ctrl: boolean; shift: boolean };
export type KeySeq = KeyStroke[];
export type HintTarget = { el: Element; rank?: number };
export type GenericAction =
  | "scroll.down" | "scroll.up" | "scroll.half_down" | "scroll.half_up" | "scroll.page_down" | "scroll.page_up"
  | "scroll.top" | "scroll.bottom"
  | "history.back" | "history.forward" | "link.prev" | "link.next" | "find.start" | "find.next" | "find.prev"
  | "hint.start" | "sheet.open" | "mode.escape" | "visual.char" | "visual.line";
/** The HUD messages find shows; `{query}`, `{index}` and `{total}` are filled in. */
export type KitMessage = "find.notFound" | "find.none" | "find.wrapBottom" | "find.wrapTop" | "find.count"
  | "visual.yanked" | "visual.yankedOne" | "visual.copyFailed" | "visual.noText";
export type KitGroup = "scroll" | "history" | "link" | "find" | "hint" | "sheet" | "mode" | "visual";
/** What a page tells the kit about visual mode (kit 5). */
export type VisualOptions = {
  /** Where `v` and `V` start: a range at the page's own cursor (its start is used). `null`, a throw or no function:
   *  a selection the reader made that is in view, else the first text in the view. */
  start?: () => Range | null;
  /** Called with the box (window coordinates) of the selection's moving end after every move, so the page keeps it
   *  inside its own margins (and its cursor on it); without it the kit keeps that end in view itself. */
  follow?: (focus: DOMRect) => void;
  /** The page's own keys in visual mode, in key notation (`<CR>`, `Y`), each run with the mode's kind while the
   *  selection is still there; one returning true ends visual mode, as `y` does. A key here replaces the kit's own
   *  visual key of the same notation. */
  keys?: Record<string, (kind: VisualKind) => boolean>;
};
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
  /** Where `G` takes the window (`null`, or no function: the end of the scroll range). */
  bottom?: () => number | null;
  /** Called just before and just after the kit's own jumps (`gg`, `G`, a move to a match of `/`, `n` or `N`), so the
   *  page can record them as history. */
  onJump?: (when: "before" | "after") => void;
  /** Visual mode's page hooks (kit 5); every page has visual mode without them. */
  visual?: VisualOptions;
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

/** Moves the window to `top` as the scroll keys do: an eased glide, or a jump under reduced motion or `smooth` off. */
export const scrollTo: (top: number) => void = (top) => scrollToAnimated(null, top);
/** Where the window is heading: the running glide's destination, else where it is. */
export const destination: () => number = windowDestination;
/** Ends a running glide at once where it was heading. */
export const settle: () => void = settleGlide;

/** Ends visual mode, if it is running; the selection goes. */
export const endVisual: () => void = endVisualMode;
/** Visual mode's kind while it runs, else null; a mode whose text the page replaced is ended first. */
export const visualMode: () => VisualKind | null = visualKind;

export const eitriKeys = {
  version: 5 as const, install, setCurrent, message, status, keys, findRefresh, findSeed, scrollTo, destination, settle,
  endVisual, visualMode,
};

declare global {
  interface Window { eitriKeys: typeof eitriKeys }
}

window.eitriKeys = eitriKeys;
