/** What the preview hands to its successor in the other window: where the reader is (the top source line and how
 *  far into it) and the search being typed. Anything that must survive a restart lives in the plugin's store, not
 *  here. */
export interface MarkdownState {
  v: 1;
  topLine: number;
  fraction: number;
  search: string | null;
  /** Set when the page showed no document: there is no reading position to go back to, so the successor waits for
   *  no render before it is ready (with nvim on another kind of buffer, none may ever come). */
  noDocument?: true;
}

export const MAX_STATE_BYTES = 256 * 1024;

type Post = (verb: string, payload: unknown) => void;

/** A state from the host, or null for anything that is not one: it crossed a process boundary and is never trusted. */
export function decodeState(raw: unknown): MarkdownState | null {
  if (typeof raw !== "object" || raw === null || Array.isArray(raw)) return null;
  const r = raw as Record<string, unknown>;
  if (r.v !== 1) return null;
  if (typeof r.topLine !== "number" || !Number.isInteger(r.topLine) || r.topLine < 0) return null;
  if (typeof r.fraction !== "number" || !(r.fraction >= 0 && r.fraction < 1)) return null;
  if (r.search !== null && typeof r.search !== "string") return null;
  if (r.noDocument !== undefined && r.noDocument !== true) return null;
  const state: MarkdownState = { v: 1, topLine: r.topLine, fraction: r.fraction, search: r.search as string | null };
  if (r.noDocument === true) state.noDocument = true;
  return state;
}

/** Whether a restored state has a reading position the successor shows before it says ready. */
export function hasPosition(state: MarkdownState): boolean {
  return state.noDocument !== true;
}

/** Answers `host.save`: the state as it is now, or no state when it would be over the host's cap. */
export function onSave(snapshot: () => MarkdownState, post: Post): void {
  const state = snapshot();
  const size = new TextEncoder().encode(JSON.stringify(state)).length;
  post("page.saved", { state: size > MAX_STATE_BYTES ? null : state });
}

/** After `host.hello`'s reply: applies a restored state (if any), waits one frame so it is drawn, then tells the host
 *  the page is ready to be swapped in. */
export function onRestored(
  raw: unknown,
  apply: (s: MarkdownState) => void,
  nextFrame: (cb: () => void) => void,
  post: Post,
): void {
  const state = decodeState(raw);
  if (state) apply(state);
  nextFrame(() => post("page.ready", {}));
}
