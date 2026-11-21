// The shapes of the keyboard kit's public interface, as the kit publishes them on `window.eitriKeys`.
export type Row = { action: string; keys: string[]; context: string; desc: string };
export type Mode = "normal" | "hint" | "insert" | "visual" | "visual_line";
export type VisualKind = "char" | "line";
export type HintTarget = { el: Element; rank?: number };
export type GenericAction =
  | "scroll.down" | "scroll.up" | "scroll.half_down" | "scroll.half_up" | "scroll.page_down" | "scroll.page_up"
  | "scroll.top"
  | "scroll.bottom" | "history.back" | "history.forward" | "link.prev" | "link.next" | "find.start" | "find.next"
  | "find.prev" | "hint.start" | "sheet.open" | "mode.escape" | "visual.char" | "visual.line";
export type KitMessage = "find.notFound" | "find.none" | "find.wrapBottom" | "find.wrapTop" | "find.count"
  | "visual.yanked" | "visual.yankedOne" | "visual.copyFailed" | "visual.noText";
export type KitGroup = "scroll" | "history" | "link" | "find" | "hint" | "sheet" | "mode" | "visual";
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
  /** Version 4 and later: where `G` takes the window. */
  bottom?: () => number | null;
  /** Version 4 and later: called around the kit's own jumps (`gg`, `G`, a find move). */
  onJump?: (when: "before" | "after") => void;
  /** Version 5 and later: visual mode's start at the page's cursor, its cursor followed, and the page's own keys in it.
   *  `follow` gets the cursor character's box and, from a kit 6 build that has it, the cursor itself as a place with
   *  `motion`'s contract (just before the cursor's character, or a line's end), for `v` and `V` and at the start. */
  visual?: {
    start?: () => Range | null;
    follow?: (focus: DOMRect, cursor?: { node: Node; offset: number }) => void;
    keys?: Record<string, (kind: VisualKind) => boolean>;
  };
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
  /** Version 4 and later: the window's glide to `top`, where it is heading, and ending it at once there. */
  scrollTo?(top: number): void;
  destination?(): number;
  settle?(): void;
  /** Version 5 and later: visual mode's kind while it runs, and ending it. */
  visualMode?(): VisualKind | null;
  endVisual?(): void;
  /** Version 6 and later: where a Normal-mode motion from `at` lands, by visual mode's own walker, the document's
   *  selection left as it was; null for a place or a result outside the page. */
  motion?(at: { node: Node; offset: number }, motion: "h" | "l" | "w" | "b" | "e" | "0" | "$"):
    { node: Node; offset: number } | null;
}
/** The kit the host serves at /.eitri/keys.js, or null when the page runs without it (a test, a browser). */
export function kit(): EitriKeys | null {
  return (window as unknown as { eitriKeys?: EitriKeys }).eitriKeys ?? null;
}
