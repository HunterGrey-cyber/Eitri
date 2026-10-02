import { isPlainAnswerKey } from "./keymap";
import type { KeyLike } from "./keymap";
import type { ReviewDiffEnvelope, ReviewEnvelope, ReviewFile, ReviewHunk, ReviewScope, ReviewTurn, TabId } from "./types";

/** The turn review overlay's state and keys, with no DOM and no React: a pure reducer over what the shell
 *  sent and what was typed, so every rule is checkable without rendering. `App.tsx` owns the one
 *  `ReviewState`, feeds it keys and envelopes, and carries out the `ReviewEffect` a key asks for -- this
 *  module never posts a message itself.
 *
 *  Everything the overlay says about a file or a turn was decided in Rust (origin, counts, state, flags);
 *  nothing here recomputes it. What this module decides is only where the cursor is and what is open. */

/** How many stops `Ctrl+d`/`Ctrl+u` move: the overlay's stops are a handful of files and hunks, not rows of
 *  a fixed height, so "half a page" is a count here, not a pixel distance. */
export const HALF_PAGE_STOPS = 5;

/** What a key means inside the overlay, or `null` for a key the overlay swallows. The overlay owns every
 *  key while it is open, so `null` still means "handled: nothing happens", never "pass it on". */
export type ReviewAction =
  | { kind: "close" }
  | { kind: "move"; delta: 1 | -1 }
  | { kind: "jump"; to: "first" | "last" }
  | { kind: "half-page"; delta: 1 | -1 }
  | { kind: "toggle" }
  | { kind: "turn"; delta: 1 | -1 }
  | { kind: "scope" }
  | { kind: "open" }
  | { kind: "copy" }
  /** The first `g` of `gg`: the caller remembers it and passes it back as `pendingG` for the next key. */
  | { kind: "pending" };

/** The overlay's key table, as a function of one key and whether a `g` is waiting. Tied both ways to
 *  `REVIEW_KEYS` (`./keymap`) by `review.test.ts`: a key that does something is listed, a listed key does
 *  something. A `g` followed by anything but `g` is `null` (the prefix ends), as in vim.
 *
 *  Only a plain key counts: Ctrl, Alt, Meta, Super/Hyper or AltGraph held make some other chord, which does
 *  nothing here -- except `Ctrl+d`/`Ctrl+u`, named first on their exact modifier set. */
export function resolveReviewKey(event: KeyLike, pendingG: boolean): ReviewAction | null {
  if (event.isComposing || event.keyCode === 229) return null;
  if (event.ctrlKey && !event.shiftKey && !event.altKey && !event.metaKey && (event.key === "d" || event.key === "u")) {
    return pendingG ? null : { kind: "half-page", delta: event.key === "d" ? 1 : -1 };
  }
  // A bracket is typed with AltGr or Option held on several layouts (German, French, Swiss, macOS), so it
  // is named ahead of the plain-key test, as BROWSE's own `[[`/`]]` are.
  if (!pendingG && !event.ctrlKey && (event.key === "[" || event.key === "]")) return { kind: "turn", delta: event.key === "]" ? 1 : -1 };
  if (!isPlainAnswerKey(event)) return null;
  if (pendingG) return event.key === "g" && !event.shiftKey ? { kind: "jump", to: "first" } : null;
  switch (event.key) {
    case "j":
      return { kind: "move", delta: 1 };
    case "k":
      return { kind: "move", delta: -1 };
    case "g":
      return event.shiftKey ? null : { kind: "pending" };
    case "G":
      return { kind: "jump", to: "last" };
    case "Enter":
      return { kind: "toggle" };
    case "S":
      return { kind: "scope" };
    case "o":
      return { kind: "open" };
    case "y":
      return { kind: "copy" };
    case "q":
    case "c":
    case "Escape":
      return { kind: "close" };
    default:
      return null;
  }
}

/** One thing a key can land on. The names are the overlay's own and are never `row`: the session's rows
 *  are `data-nav-stop="row"`, still in the DOM under the overlay, and a second set of `row`s would be
 *  counted by `rowIndexOf` and the cursor's `querySelectorAll`. `group` is the folded "changed outside
 *  this tab's edits" header, a stop so that `Enter` can reach it. */
export type ReviewStop = { kind: "file"; path: string } | { kind: "hunk"; path: string; id: number } | { kind: "group" };

export function stopKey(stop: ReviewStop): string {
  switch (stop.kind) {
    case "group":
      return "group";
    case "file":
      return `file:${stop.path}`;
    case "hunk":
      return `hunk:${stop.path}:${stop.id}`;
  }
}

/** A file's patch, as far as it has got. Keyed by path in `ReviewState.diffs`; reset when a new overview
 *  arrives, because a diff belongs to one turn and one scope. */
export type DiffEntry =
  | { status: "loading"; requestId: string }
  | { status: "ready"; diff: ReviewDiffEnvelope }
  | { status: "failed"; error: string };

export type ReviewState = {
  tab: TabId;
  /** The scope the latest request asked for. What is drawn follows `envelope.scope`. */
  scope: ReviewScope;
  /** The turn a turn-scope request names: `"latest"` until an envelope says which one that is. */
  turn: "latest" | number;
  /** The newest overview request. An envelope or failure carrying any other id is stale and ignored. */
  requestId: string;
  envelope: ReviewEnvelope | null;
  /** Why the newest overview request was refused, or `null`. */
  error: string | null;
  /** The stop the cursor is on, by `stopKey`; `null` is the first stop. Held as an identity rather than an
   *  index so a patch arriving above the cursor does not move it onto another stop. */
  cursor: string | null;
  /** Files whose hunks are open. */
  expanded: string[];
  /** Whether the "changed outside this tab's edits" group is open. */
  groupOpen: boolean;
  diffs: Record<string, DiffEntry>;
  /** A `g` is waiting for its second key. */
  pendingG: boolean;
};

/** A fresh overlay asking for the tab's latest turn. */
export function openReview(tab: TabId, requestId: string): ReviewState {
  return { tab, scope: "turn", turn: "latest", requestId, envelope: null, error: null, cursor: null, expanded: [], groupOpen: false, diffs: {}, pendingG: false };
}

/** The turn record the overview is about (turn scope), or `null`. */
export function currentTurn(envelope: ReviewEnvelope): ReviewTurn | null {
  return envelope.turns.find((t) => t.n === envelope.current) ?? null;
}

/** A review that compared nothing (no baseline, or one still being taken) has no file list to show: an
 *  empty one would read as "nothing changed". The notes say why. */
export function hidesFiles(envelope: ReviewEnvelope): boolean {
  return !envelope.compared;
}

/** Files that were named by a call of this tab (`✓`, `·`), then the ones nothing named (`?`). */
export function splitFiles(envelope: ReviewEnvelope): { named: ReviewFile[]; unnamed: ReviewFile[] } {
  if (hidesFiles(envelope)) return { named: [], unnamed: [] };
  return {
    named: envelope.files.filter((f) => f.origin !== "workspace"),
    unnamed: envelope.files.filter((f) => f.origin === "workspace"),
  };
}

/** Whether `Enter` has a patch to ask for: a binary file, one too large to snapshot and a nested
 *  repository have none, and the row says why. */
export function hasPatch(file: ReviewFile): boolean {
  return !file.binary && !file.tooLarge && !file.nested;
}

/** The stops the cursor can be on, top to bottom: each named file, its open hunks under it; then the group
 *  header (only when there is something in it) and, once opened, its files. */
export function reviewStops(state: ReviewState): ReviewStop[] {
  const envelope = state.envelope;
  if (envelope === null) return [];
  const { named, unnamed } = splitFiles(envelope);
  const stops: ReviewStop[] = [];
  const addFile = (file: ReviewFile) => {
    stops.push({ kind: "file", path: file.path });
    if (!state.expanded.includes(file.path)) return;
    const entry = state.diffs[file.path];
    if (entry?.status !== "ready" || entry.diff.hunks === null) return;
    for (const hunk of entry.diff.hunks) stops.push({ kind: "hunk", path: file.path, id: hunk.id });
  };
  named.forEach(addFile);
  if (unnamed.length > 0) {
    stops.push({ kind: "group" });
    if (state.groupOpen) unnamed.forEach(addFile);
  }
  return stops;
}

/** Where the cursor is in `stops`; the first stop when it names none that exists any more. */
export function cursorIndex(state: ReviewState, stops: ReviewStop[]): number {
  if (state.cursor === null) return 0;
  const index = stops.findIndex((stop) => stopKey(stop) === state.cursor);
  return index === -1 ? 0 : index;
}

export function cursorStop(state: ReviewState): ReviewStop | null {
  const stops = reviewStops(state);
  return stops[cursorIndex(state, stops)] ?? null;
}

function withCursor(state: ReviewState, stops: ReviewStop[], index: number): ReviewState {
  const clamped = Math.max(0, Math.min(index, stops.length - 1));
  const stop = stops[clamped];
  return { ...state, cursor: stop === undefined ? null : stopKey(stop) };
}

/** An overview arrived. Taken only for the newest request; it replaces what was drawn and starts the cursor,
 *  the open files and the patches over, because they all belonged to another turn or scope. */
export function receiveReview(state: ReviewState, envelope: ReviewEnvelope): ReviewState {
  if (envelope.requestId !== state.requestId) return state;
  return {
    ...state,
    scope: envelope.scope,
    turn: envelope.scope === "turn" ? envelope.current : state.turn,
    envelope,
    error: null,
    cursor: null,
    expanded: [],
    groupOpen: false,
    diffs: {},
    pendingG: false,
  };
}

/** A patch arrived. Matched to its file by the request id it answers, so a reply for a turn the overlay has
 *  since left finds no entry and is dropped. */
export function receiveDiff(state: ReviewState, diff: ReviewDiffEnvelope): ReviewState {
  const path = Object.keys(state.diffs).find((p) => {
    const entry = state.diffs[p];
    return entry.status === "loading" && entry.requestId === diff.requestId;
  });
  if (path === undefined) return state;
  return { ...state, diffs: { ...state.diffs, [path]: { status: "ready", diff } } };
}

/** The shell refused a request this overlay made. The newest overview request's refusal is the overlay's
 *  error (and it goes back to the scope it is actually showing); a patch request's refusal marks that file.
 *  Any other id is stale. */
export function failRequest(state: ReviewState, requestId: string, error: string): ReviewState {
  if (requestId === state.requestId) {
    const shown = state.envelope;
    return {
      ...state,
      error,
      scope: shown?.scope ?? state.scope,
      turn: shown !== null && shown.scope === "turn" ? shown.current : state.turn,
    };
  }
  const path = Object.keys(state.diffs).find((p) => {
    const entry = state.diffs[p];
    return entry.status === "loading" && entry.requestId === requestId;
  });
  if (path === undefined) return state;
  return { ...state, diffs: { ...state.diffs, [path]: { status: "failed", error } } };
}

/** What carrying a key out needs from outside the reducer. */
export type ReviewEffect =
  | { kind: "close" }
  | { kind: "request"; requestId: string; turn: "latest" | number; scope: ReviewScope }
  | { kind: "request-diff"; requestId: string; turn: number; scope: ReviewScope; path: string }
  | { kind: "open"; path: string; line: number | null }
  | { kind: "copy"; text: string };

/** The first line of a hunk on the file's new side: the number `:edit +N` should land on. */
export function hunkFirstNewLine(hunk: ReviewHunk): number | null {
  for (const line of hunk.lines) if (line.newNo !== null) return line.newNo;
  return null;
}

/** Where `o` and `y` point for the stop under the cursor: its file and, when a patch is loaded, its hunk's
 *  first new line (a file row: its first hunk's). `null` on the group header, which is no file. */
export function stopTarget(state: ReviewState, stop: ReviewStop | null): { path: string; line: number | null } | null {
  if (stop === null || stop.kind === "group") return null;
  const entry = state.diffs[stop.path];
  const hunks = entry?.status === "ready" ? entry.diff.hunks : null;
  if (hunks === null || hunks === undefined) return { path: stop.path, line: null };
  const hunk = stop.kind === "hunk" ? hunks.find((h) => h.id === stop.id) : hunks[0];
  return { path: stop.path, line: hunk === undefined ? null : hunkFirstNewLine(hunk) };
}

/** One key's effect on the overlay: the next state and, when the key asks for something outside it, the
 *  effect for `App.tsx` to carry out. `nextId` mints the request ids a new request needs. */
export function applyReviewKey(state: ReviewState, action: ReviewAction, nextId: () => string): { state: ReviewState; effect: ReviewEffect | null } {
  const idle = { ...state, pendingG: false };
  const none = (next: ReviewState) => ({ state: next, effect: null });
  const stops = reviewStops(state);
  const at = cursorIndex(state, stops);
  switch (action.kind) {
    case "pending":
      return none({ ...state, pendingG: true });
    case "close":
      return { state: idle, effect: { kind: "close" } };
    case "move":
      return none(withCursor(idle, stops, at + action.delta));
    case "half-page":
      return none(withCursor(idle, stops, at + action.delta * HALF_PAGE_STOPS));
    case "jump":
      return none(withCursor(idle, stops, action.to === "first" ? 0 : stops.length - 1));
    case "toggle":
      return toggle(idle, stops, at, nextId);
    case "turn": {
      const envelope = state.envelope;
      // Turns are stepped through in turn scope only: the session scope is every turn at once.
      if (envelope === null || envelope.scope !== "turn" || state.requestId !== envelope.requestId) return none(idle);
      const numbers = envelope.turns.map((t) => t.n).sort((a, b) => a - b);
      const target = numbers[numbers.indexOf(envelope.current) + action.delta];
      if (target === undefined) return none(idle);
      const requestId = nextId();
      return { state: { ...idle, requestId, turn: target }, effect: { kind: "request", requestId, turn: target, scope: "turn" } };
    }
    case "scope": {
      const envelope = state.envelope;
      if (envelope === null || state.requestId !== envelope.requestId) return none(idle);
      const requestId = nextId();
      const scope: ReviewScope = envelope.scope === "turn" ? "session" : "turn";
      const turn = scope === "turn" ? state.turn : envelope.current;
      return { state: { ...idle, requestId, scope, turn }, effect: { kind: "request", requestId, turn, scope } };
    }
    case "open": {
      const target = stopTarget(state, stops[at] ?? null);
      return { state: idle, effect: target === null ? null : { kind: "open", ...target } };
    }
    case "copy": {
      const target = stopTarget(state, stops[at] ?? null);
      if (target === null) return none(idle);
      return { state: idle, effect: { kind: "copy", text: target.line === null ? target.path : `${target.path}:${target.line}` } };
    }
  }
}

function toggle(state: ReviewState, stops: ReviewStop[], at: number, nextId: () => string): { state: ReviewState; effect: ReviewEffect | null } {
  const stop = stops[at];
  const envelope = state.envelope;
  if (stop === undefined || envelope === null) return { state, effect: null };
  if (stop.kind === "group") return { state: { ...state, groupOpen: !state.groupOpen }, effect: null };
  const { path } = stop;
  if (state.expanded.includes(path) || stop.kind === "hunk") {
    // On a hunk, "the file under the cursor" is its file: close it and land on its row.
    const closed = { ...state, expanded: state.expanded.filter((p) => p !== path), cursor: stopKey({ kind: "file", path }) };
    return { state: closed, effect: null };
  }
  const file = envelope.files.find((f) => f.path === path);
  if (file === undefined || !hasPatch(file)) return { state, effect: null };
  const open = { ...state, expanded: [...state.expanded, path] };
  const entry = state.diffs[path];
  if (entry !== undefined && entry.status !== "failed") return { state: open, effect: null };
  const requestId = nextId();
  return {
    state: { ...open, diffs: { ...state.diffs, [path]: { status: "loading", requestId } } },
    effect: { kind: "request-diff", requestId, turn: envelope.current, scope: envelope.scope, path },
  };
}

/** `HH:MM:SS` in the local clock; `…` for a time that is not known yet (a turn still running). */
export function clock(ms: number | null | undefined): string {
  if (ms === null || ms === undefined) return "…";
  const d = new Date(ms);
  const two = (n: number) => String(n).padStart(2, "0");
  return `${two(d.getHours())}:${two(d.getMinutes())}:${two(d.getSeconds())}`;
}

/** The header line. It says what happened to the disk, never who did it: the signs on the file rows carry
 *  the attribution (`✓` named by a call of this tab, `·` named and unchanged, `?` not named). */
export function reviewTitle(envelope: ReviewEnvelope): string {
  if (envelope.scope === "session") {
    const turns = [...envelope.turns].sort((a, b) => a.n - b.n);
    const first = turns[0];
    const last = turns[turns.length - 1];
    const span = first === undefined || last === undefined ? "" : ` · ${clock(first.startedAt)} → ${clock(last.endedAt)}`;
    return `review · session · ${turns.length} ${turns.length === 1 ? "turn" : "turns"}${span} · changed on disk during this session`;
  }
  const turn = currentTurn(envelope);
  const total = envelope.turns.reduce((max, t) => Math.max(max, t.n), envelope.current);
  const span = turn === null ? "" : ` · ${clock(turn.startedAt)} → ${clock(turn.endedAt)}`;
  return `review · turn ${envelope.current} of ${total}${span} · changed on disk during this turn`;
}

/** The lines the header shows under its title: the review's own notes, as Rust worded them, less the one
 *  the title already says. Rust is the only source of these, so a mark is never said twice in two
 *  wordings, and a turn's state is described as it really is (an end snapshot that failed in this run is
 *  not "did not finish"). */
export function reviewNotes(envelope: ReviewEnvelope): string[] {
  const title = reviewTitle(envelope);
  return envelope.notes.filter((note) => !title.includes(note));
}

/** Why a file row has nothing to open, in the words the row shows. */
export function fileFlags(file: ReviewFile): string[] {
  return [file.binary ? "binary" : null, file.tooLarge ? "too large to snapshot" : null, file.nested ? "nested repository, not reviewed" : null].filter(
    (f): f is string => f !== null,
  );
}

/** The sign column: attribution as Rust decided it. */
export function originSign(file: ReviewFile): string {
  return file.origin === "agent" ? "✓" : file.origin === "agent_only" ? "·" : "?";
}

/** The shown counts, with a real minus sign as the permission card's diff head uses. */
export function counts(added: number, removed: number): string {
  return `+${added} −${removed}`;
}

/** What a patch the shell would not send says instead (`hunks: null`): refused with its counts, never cut
 *  short -- a diff missing lines reads as the whole diff. */
export function tooLargeNote(added: number, removed: number): string {
  return `too large (+${added} −${removed} lines); o opens it in the editor`;
}

/** The box under the cursor that `j`/`k` should scroll before they move: the open hunk's own lines, found
 *  from the cursor's row (`.row-current`), or `null` on a stop with no box. */
export function boxUnderCursor(overlay: HTMLElement | null): HTMLElement | null {
  const current = overlay?.querySelector<HTMLElement>('[data-nav-stop="hunk"][aria-current="true"]');
  return current?.parentElement?.querySelector<HTMLElement>(".review-hunk-lines") ?? null;
}

/** Scrolls `box` by `step` pixels toward `delta` and says so, or says it could not: no box, a box that
 *  does not overflow, or one already at that end -- then `j`/`k` move the cursor instead. */
export function scrollBoxFirst(box: HTMLElement | null, delta: 1 | -1, step: number): boolean {
  if (box === null) return false;
  const room = box.scrollHeight - box.clientHeight;
  if (room <= 1) return false;
  if (delta > 0 ? box.scrollTop >= room - 1 : box.scrollTop <= 0) return false;
  box.scrollTop += delta * step;
  return true;
}
