import { isPlainAnswerKey } from "./keymap";
import type { KeyLike } from "./keymap";
import type {
  ReviewComment,
  ReviewDiffEnvelope,
  ReviewDraft,
  ReviewDraftEnvelope,
  ReviewEnvelope,
  ReviewFile,
  ReviewHunk,
  ReviewNotOnDisk,
  ReviewRecoveryEntry,
  ReviewRevert,
  ReviewScope,
  ReviewSendPreviewEnvelope,
  ReviewTurn,
  TabId,
} from "./types";

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
  | { kind: "revert" }
  | { kind: "undo" }
  | { kind: "comment" }
  | { kind: "send" }
  /** A one-line prompt's answer: `y`/`n` on a question, or `Escape` on any prompt (`cancel`). Only
   *  `resolveReviewKey` with a prompt open produces it. */
  | { kind: "answer"; answer: "yes" | "no" | "cancel" }
  /** Enter in the comment input. The input is a real `<input>` that stops its own Enter, so this is
   *  not a key `resolveReviewKey` returns: the overlay hands it over. */
  | { kind: "accept" }
  /** The first `g` of `gg`: the caller remembers it and passes it back as `pendingG` for the next key. */
  | { kind: "pending" };

/** The overlay's key table, as a function of one key and whether a `g` is waiting. Tied both ways to
 *  `REVIEW_KEYS` (`./keymap`) by `review.test.ts`: a key that does something is listed, a listed key does
 *  something. A `g` followed by anything but `g` is `null` (the prefix ends), as in vim.
 *
 *  Only a plain key counts: Ctrl, Alt, Meta, Super/Hyper or AltGraph held make some other chord, which does
 *  nothing here -- except `Ctrl+d`/`Ctrl+u`, named first on their exact modifier set. */
export function resolveReviewKey(event: KeyLike, pendingG: boolean, prompt: ReviewPrompt | null = null): ReviewAction | null {
  if (event.isComposing || event.keyCode === 229) return null;
  // A prompt owns the keys: the overlay's own table is not read while one is open, so `x` then `u` cannot
  // run a second command under a question that has not been answered.
  if (prompt !== null) return resolvePromptKey(event, prompt);
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
    case "x":
      return { kind: "revert" };
    case "u":
      return { kind: "undo" };
    case "i":
      return { kind: "comment" };
    case "s":
      return { kind: "send" };
    case "q":
    case "c":
    case "Escape":
      return { kind: "close" };
    default:
      return null;
  }
}

/** What a key means while a prompt is open. A question takes `y`, `n` and `Escape` and nothing else; the
 *  comment input is a text box that takes its own keys, so only an `Escape` that reached the overlay (focus
 *  was somewhere else) means anything. A modified key is some other chord and does nothing. */
function resolvePromptKey(event: KeyLike, prompt: ReviewPrompt): ReviewAction | null {
  if (!isPlainAnswerKey(event)) return null;
  if (event.key === "Escape") return { kind: "answer", answer: "cancel" };
  if (prompt.kind === "comment") return null;
  if (event.key === "y") return { kind: "answer", answer: "yes" };
  if (event.key === "n") return { kind: "answer", answer: "no" };
  return null;
}

/** One thing a key can land on. The names are the overlay's own and are never `row`: the session's rows
 *  are `data-nav-stop="row"`, still in the DOM under the overlay, and a second set of `row`s would be
 *  counted by `rowIndexOf` and the cursor's `querySelectorAll`. `group` is the folded "changed outside
 *  this tab's edits" header, a stop so that `Enter` can reach it. */
export type ReviewStop =
  | { kind: "file"; path: string }
  | { kind: "hunk"; path: string; id: number }
  | { kind: "comment"; path: string; id: number }
  /** An interrupted revert the journal still holds; `id` is the journal entry's. */
  | { kind: "recovery"; id: string }
  | { kind: "group" };

export function stopKey(stop: ReviewStop): string {
  switch (stop.kind) {
    case "group":
      return "group";
    case "file":
      return `file:${stop.path}`;
    case "hunk":
      return `hunk:${stop.path}:${stop.id}`;
    case "comment":
      return `comment:${stop.path}:${stop.id}`;
    case "recovery":
      return `recovery:${stop.id}`;
  }
}

/** What a one-line prompt is asking, kept in the state so a key means what the question says. */
export type ReviewPrompt =
  /** `x` on a file row: whole-file reverts ask first. `turn` is the number the sentence names. */
  | { kind: "revert-file"; path: string; turn: number; scope: ReviewScope; shownTurn: number }
  | { kind: "recover"; entry: string; path: string }
  /** `i` on a hunk: the one-line input. `from`/`to` are the new-side lines it comments on. */
  | { kind: "comment"; path: string; turn: number; scope: ReviewScope; from: number; to: number; text: string }
  /** `s`: `preview` is `null` until the shell's `review_send_preview` for `requestId` arrives. */
  | { kind: "send"; requestId: string; preview: ReviewPreview | null };

export type ReviewPreview = { digest: string; text: string; notOnDisk: ReviewNotOnDisk[]; queued: boolean };

/** The words of a question prompt (the comment input draws itself, as a text box). */
export function promptText(prompt: ReviewPrompt): string {
  switch (prompt.kind) {
    case "revert-file":
      return `revert the whole file ${prompt.path} to before turn ${prompt.turn}? y/n`;
    case "recover":
      return `restore ${prompt.path} to its bytes from before the interrupted revert? the current bytes are kept in the review store. y restores · n forgets this`;
    case "comment":
      return `comment on ${prompt.path}:${prompt.from === prompt.to ? prompt.from : `${prompt.from}-${prompt.to}`}`;
    case "send":
      return prompt.preview === null ? "preparing what would be sent…" : prompt.preview.queued ? "y queues it behind the running turn · n cancels" : "y sends · n cancels";
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
  diffs: ReadonlyMap<string, DiffEntry>;
  /** A `g` is waiting for its second key. */
  pendingG: boolean;
  /** The tab's draft, from the `review` and `review_draft` envelopes only: nothing is kept across a reload. */
  draft: ReviewDraft;
  /** Interrupted reverts, from the journal (`review_recovery`); they are the project's, so they are held
   *  whether or not an overview has arrived. */
  recovery: ReviewRecoveryEntry[];
  /** The question or input the overlay is waiting on; it owns the keys. */
  prompt: ReviewPrompt | null;
  /** One line about what a key just did or why it did nothing (`nothing to undo`, a refusal's text). Any
   *  next key clears it. */
  status: string | null;
};

export const EMPTY_DRAFT: ReviewDraft = { comments: [], reverts: [], canUndo: false };

/** A fresh overlay asking for the tab's latest turn. */
export function openReview(tab: TabId, requestId: string, recovery: ReviewRecoveryEntry[] = []): ReviewState {
  return {
    tab,
    scope: "turn",
    turn: "latest",
    requestId,
    envelope: null,
    error: null,
    cursor: null,
    expanded: [],
    groupOpen: false,
    diffs: new Map(),
    pendingG: false,
    draft: EMPTY_DRAFT,
    recovery,
    prompt: null,
    status: null,
  };
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

/** The new-side line range a hunk covers, context included, or `null` for a hunk with no line on the new
 *  side (everything in it was deleted). */
export function hunkNewRange(hunk: ReviewHunk): [number, number] | null {
  let lo: number | null = null;
  let hi: number | null = null;
  for (const line of hunk.lines) {
    if (line.newNo === null) continue;
    lo = lo === null ? line.newNo : Math.min(lo, line.newNo);
    hi = hi === null ? line.newNo : Math.max(hi, line.newNo);
  }
  return lo === null || hi === null ? null : [lo, hi];
}

/** The new-side lines a comment on `hunk` is about: the added lines' first and last number. A hunk that only
 *  deletes has none, so it is the context line right after the deleted ones, or at the end of the file the
 *  one before them. `null` when the hunk has no line on the new side at all. */
export function commentLines(hunk: ReviewHunk): [number, number] | null {
  const added = hunk.lines.filter((l) => l.kind === "added" && l.newNo !== null).map((l) => l.newNo as number);
  if (added.length > 0) return [Math.min(...added), Math.max(...added)];
  const firstRemoved = hunk.lines.findIndex((l) => l.kind === "removed");
  const context = (l: ReviewHunk["lines"][number]) => l.kind === "context" && l.newNo !== null;
  for (let i = Math.max(firstRemoved, 0); i < hunk.lines.length; i++) if (context(hunk.lines[i])) return [hunk.lines[i].newNo as number, hunk.lines[i].newNo as number];
  for (let i = (firstRemoved < 0 ? hunk.lines.length : firstRemoved) - 1; i >= 0; i--) if (context(hunk.lines[i])) return [hunk.lines[i].newNo as number, hunk.lines[i].newNo as number];
  return null;
}

/** `diffs` with one file's entry replaced. A `Map`, because a path is the user's own text: a file named
 *  `constructor` or `__proto__` must not find a member of `Object.prototype` where its entry should be. */
function withDiff(diffs: ReadonlyMap<string, DiffEntry>, path: string, entry: DiffEntry): ReadonlyMap<string, DiffEntry> {
  const next = new Map(diffs);
  next.set(path, entry);
  return next;
}

/** The path whose patch request `requestId` is still waiting on, if any. */
function pathOfLoading(state: ReviewState, requestId: string): string | undefined {
  for (const [path, entry] of state.diffs) if (entry.status === "loading" && entry.requestId === requestId) return path;
  return undefined;
}

/** The loaded hunks of a file, or `null` while there are none to draw. */
function loadedHunks(state: ReviewState, path: string): ReviewHunk[] | null {
  const entry = state.diffs.get(path);
  return entry?.status === "ready" ? entry.diff.hunks : null;
}

/** The draft's comments that sit under `hunk`: this turn's, on this file, whose first line is on the
 *  hunk's new side. Ordered by line, then by when they were made. */
export function commentsUnder(state: ReviewState, path: string, hunk: ReviewHunk): ReviewComment[] {
  const envelope = state.envelope;
  const range = hunkNewRange(hunk);
  if (envelope === null || range === null) return [];
  return state.draft.comments
    .filter((c) => c.path === path && c.turn === envelope.current && c.from >= range[0] && c.from <= range[1])
    .sort((a, b) => a.from - b.from || a.id - b.id);
}

/** The draft's newest revert of `hunk` (matched by turn, path, hunk id and header), or of a whole file when
 *  `hunk` is `null`; `null` when there is none. The newest wins: a hunk reverted, undone and reverted again is
 *  reverted. */
export function revertOf(state: ReviewState, path: string, hunk: ReviewHunk | null): ReviewRevert | null {
  const envelope = state.envelope;
  if (envelope === null) return null;
  let found: ReviewRevert | null = null;
  for (const r of state.draft.reverts) {
    if (r.turn !== envelope.current || r.path !== path) continue;
    const same = hunk === null ? r.hunk === null : r.hunk === hunk.id && r.header === hunk.header;
    if (same && (found === null || r.id > found.id)) found = r;
  }
  return found;
}

/** `reverted`, or `reverted, undone` once the change is back; `null` for a hunk or file not reverted. */
export function revertMark(revert: ReviewRevert | null): string | null {
  return revert === null ? null : revert.undone ? "reverted, undone" : "reverted";
}

/** The footer line about the draft, or `null` when it holds nothing. */
export function draftLine(draft: ReviewDraft): string | null {
  const c = draft.comments.length;
  const r = draft.reverts.length;
  if (c === 0 && r === 0) return null;
  return `draft: ${c} ${c === 1 ? "comment" : "comments"}, ${r} ${r === 1 ? "revert" : "reverts"} · s sends them to the agent`;
}

/** The overlay's key line: what the keys are, in the order the design lists them. */
export const REVIEW_KEY_LINE = "j/k move · Enter open · x revert · u undo · i comment · s send · [ ] turn · o editor · q close";

/** What `x` on a hunk of a binary file says: nothing to revert piecemeal. */
export const BINARY_HUNK_NOTE = "binary files revert only whole; x on the file row";

/** The stops the cursor can be on, top to bottom: the interrupted reverts first (they are the project's, not
 *  the turn's); then each named file with its open hunks under it and each hunk's comments under that; then
 *  the group header (only when there is something in it) and, once opened, its files. */
export function reviewStops(state: ReviewState): ReviewStop[] {
  const envelope = state.envelope;
  const stops: ReviewStop[] = state.recovery.map((r) => ({ kind: "recovery", id: r.id }));
  if (envelope === null) return stops;
  const { named, unnamed } = splitFiles(envelope);
  const addFile = (file: ReviewFile) => {
    stops.push({ kind: "file", path: file.path });
    if (!state.expanded.includes(file.path)) return;
    const hunks = loadedHunks(state, file.path);
    if (hunks === null) return;
    for (const hunk of hunks) {
      stops.push({ kind: "hunk", path: file.path, id: hunk.id });
      for (const comment of commentsUnder(state, file.path, hunk)) stops.push({ kind: "comment", path: file.path, id: comment.id });
    }
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
    diffs: new Map(),
    pendingG: false,
    draft: envelope.draft ?? EMPTY_DRAFT,
    prompt: null,
    status: null,
  };
}

/** Keeps the cursor near where it was when the stop it named is gone (a comment deleted, a recovery entry
 *  resolved): the stop now at the old position, rather than the top of the overlay. */
function keepCursorNear(before: ReviewState, after: ReviewState): ReviewState {
  const was = reviewStops(before);
  const now = reviewStops(after);
  if (after.cursor === null || now.some((stop) => stopKey(stop) === after.cursor)) return after;
  const index = Math.min(cursorIndex(before, was), now.length - 1);
  return { ...after, cursor: index < 0 ? null : stopKey(now[index]) };
}

/** The tab's draft changed, asked for or not. A send preview on screen was made from the draft as it was, so
 *  it is dropped: confirming it would send something other than what the user read. */
export function receiveDraft(state: ReviewState, envelope: ReviewDraftEnvelope): ReviewState {
  if (envelope.tab !== state.tab) return state;
  const stale = state.prompt?.kind === "send";
  const next: ReviewState = {
    ...state,
    draft: envelope.draft,
    prompt: stale ? null : state.prompt,
    status: stale ? "the draft changed; s shows what would be sent now" : state.status,
  };
  return keepCursorNear(state, next);
}

/** The shell's preview of what `s` would send. Taken only for the question that asked for it and only while
 *  that question is still open; any other is stale. */
export function receivePreview(state: ReviewState, envelope: ReviewSendPreviewEnvelope): ReviewState {
  const prompt = state.prompt;
  if (envelope.tab !== state.tab || prompt?.kind !== "send" || prompt.requestId !== envelope.requestId || prompt.preview !== null) return state;
  const { digest, text, notOnDisk, queued } = envelope;
  return { ...state, prompt: { ...prompt, preview: { digest, text, notOnDisk, queued } } };
}

/** The interrupted reverts the journal holds, as the shell lists them now. A question about one that is gone
 *  is dropped. */
export function receiveRecovery(state: ReviewState, entries: ReviewRecoveryEntry[]): ReviewState {
  const prompt = state.prompt;
  const gone = prompt?.kind === "recover" && !entries.some((e) => e.id === prompt.entry);
  return keepCursorNear(state, { ...state, recovery: entries, prompt: gone ? null : state.prompt });
}

/** A command's outcome, in the shell's words, on the status line. `null` (an answer with nothing to say)
 *  leaves the line as it is. */
export function withStatus(state: ReviewState, text: string | null): ReviewState {
  return text === null ? state : { ...state, status: text };
}

/** The comment input's text, as typed. Only while the comment input is the open prompt. */
export function typeComment(state: ReviewState, text: string): ReviewState {
  return state.prompt?.kind === "comment" ? { ...state, prompt: { ...state.prompt, text } } : state;
}

/** A patch arrived. Matched to its file by the request id it answers, so a reply for a turn the overlay has
 *  since left finds no entry and is dropped. */
export function receiveDiff(state: ReviewState, diff: ReviewDiffEnvelope): ReviewState {
  const path = pathOfLoading(state, diff.requestId);
  if (path === undefined) return state;
  return { ...state, diffs: withDiff(state.diffs, path, { status: "ready", diff }) };
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
  const path = pathOfLoading(state, requestId);
  if (path === undefined) return state;
  return { ...state, diffs: withDiff(state.diffs, path, { status: "failed", error }) };
}

/** What carrying a key out needs from outside the reducer. */
export type ReviewEffect =
  | { kind: "close" }
  | { kind: "request"; requestId: string; turn: "latest" | number; scope: ReviewScope }
  | { kind: "request-diff"; requestId: string; turn: number; scope: ReviewScope; path: string }
  | { kind: "open"; path: string; line: number | null; turn: number; scope: ReviewScope }
  | { kind: "copy"; text: string }
  | { kind: "revert"; requestId: string; turn: number; scope: ReviewScope; path: string; target: "file" | { hunk: number; header: string } }
  | { kind: "undo"; requestId: string }
  | { kind: "comment-add"; requestId: string; turn: number; scope: ReviewScope; path: string; from: number; to: number; text: string }
  | { kind: "comment-remove"; requestId: string; id: number }
  /** `confirm: null` asks for the preview; a digest sends the draft that preview was made from. */
  | { kind: "send"; requestId: string; confirm: string | null }
  | { kind: "recover"; requestId: string; entry: string; answer: "restore" | "dismiss" };

/** The first line of a hunk on the file's new side: the number `:edit +N` should land on. */
export function hunkFirstNewLine(hunk: ReviewHunk): number | null {
  for (const line of hunk.lines) if (line.newNo !== null) return line.newNo;
  return null;
}

/** Where `o` and `y` point for the stop under the cursor: its file and, when a patch is loaded, its hunk's
 *  first new line (a file row: its first hunk's; a comment: its first line). `null` on the group header and
 *  on a recovery row, which name no line of the turn. */
export function stopTarget(state: ReviewState, stop: ReviewStop | null): { path: string; line: number | null } | null {
  if (stop === null || stop.kind === "group" || stop.kind === "recovery") return null;
  if (stop.kind === "comment") return { path: stop.path, line: state.draft.comments.find((c) => c.id === stop.id)?.from ?? null };
  const hunks = loadedHunks(state, stop.path);
  if (hunks === null) return { path: stop.path, line: null };
  const hunk = stop.kind === "hunk" ? hunks.find((h) => h.id === stop.id) : hunks[0];
  return { path: stop.path, line: hunk === undefined ? null : hunkFirstNewLine(hunk) };
}

/** One key's effect on the overlay: the next state and, when the key asks for something outside it, the
 *  effect for `App.tsx` to carry out. `nextId` mints the request ids a new request needs. */
export function applyReviewKey(state: ReviewState, action: ReviewAction, nextId: () => string): { state: ReviewState; effect: ReviewEffect | null } {
  const idle = { ...state, pendingG: false, status: null };
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
      const envelope = state.envelope;
      if (target === null || envelope === null) return none(idle);
      return { state: idle, effect: { kind: "open", ...target, turn: envelope.current, scope: envelope.scope } };
    }
    case "revert":
      return revert(idle, stops[at] ?? null, nextId);
    case "undo": {
      if (!state.draft.canUndo) return none({ ...idle, status: "nothing to undo" });
      return { state: idle, effect: { kind: "undo", requestId: nextId() } };
    }
    case "comment":
      return none(openComment(idle, stops[at] ?? null));
    case "send": {
      if (state.draft.comments.length === 0 && state.draft.reverts.length === 0) return none({ ...idle, status: "nothing to send" });
      const requestId = nextId();
      return { state: { ...idle, prompt: { kind: "send", requestId, preview: null } }, effect: { kind: "send", requestId, confirm: null } };
    }
    case "answer":
      return answer(idle, action.answer, nextId);
    case "accept":
      return accept(idle, nextId);
    case "copy": {
      const target = stopTarget(state, stops[at] ?? null);
      if (target === null) return none(idle);
      return { state: idle, effect: { kind: "copy", text: target.line === null ? target.path : `${target.path}:${target.line}` } };
    }
  }
}

type Applied = { state: ReviewState; effect: ReviewEffect | null };

/** The turn number a whole-file revert's question names: the turn shown, or in session scope the first turn
 *  of the session, since that is what the file goes back to. */
function revertsBackTo(envelope: ReviewEnvelope): number {
  return envelope.scope === "turn" ? envelope.current : envelope.turns.reduce((min, t) => Math.min(min, t.n), envelope.current);
}

/** `x`: revert the hunk under the cursor, ask about the whole file on a file row, delete a comment. */
function revert(state: ReviewState, stop: ReviewStop | null, nextId: () => string): Applied {
  const envelope = state.envelope;
  if (stop === null || envelope === null) return { state, effect: null };
  if (stop.kind === "comment") return { state, effect: { kind: "comment-remove", requestId: nextId(), id: stop.id } };
  if (stop.kind === "file") {
    const prompt: ReviewPrompt = { kind: "revert-file", path: stop.path, turn: revertsBackTo(envelope), scope: envelope.scope, shownTurn: envelope.current };
    return { state: { ...state, prompt }, effect: null };
  }
  if (stop.kind !== "hunk") return { state, effect: null };
  const entry = state.diffs.get(stop.path);
  const binary = (entry?.status === "ready" && entry.diff.binary === true) || envelope.files.find((f) => f.path === stop.path)?.binary === true;
  if (binary) return { state: { ...state, status: BINARY_HUNK_NOTE }, effect: null };
  const hunk = loadedHunks(state, stop.path)?.find((h) => h.id === stop.id);
  if (hunk === undefined) return { state, effect: null };
  const target = { hunk: hunk.id, header: hunk.header };
  return { state, effect: { kind: "revert", requestId: nextId(), turn: envelope.current, scope: envelope.scope, path: stop.path, target } };
}

/** `i` on a hunk: opens the comment input on the lines the hunk added. */
function openComment(state: ReviewState, stop: ReviewStop | null): ReviewState {
  const envelope = state.envelope;
  if (stop === null || stop.kind !== "hunk" || envelope === null) return state;
  const hunk = loadedHunks(state, stop.path)?.find((h) => h.id === stop.id);
  if (hunk === undefined) return state;
  const lines = commentLines(hunk);
  if (lines === null) return { ...state, status: "this hunk has no line left in the file to comment on" };
  return { ...state, prompt: { kind: "comment", path: stop.path, turn: envelope.current, scope: envelope.scope, from: lines[0], to: lines[1], text: "" } };
}

/** The answer to the open question. A question answered ends; `y` on a preview still being made waits. */
function answer(state: ReviewState, which: "yes" | "no" | "cancel", nextId: () => string): Applied {
  const prompt = state.prompt;
  if (prompt === null) return { state, effect: null };
  const cleared = { ...state, prompt: null };
  if (which === "cancel") return { state: cleared, effect: null };
  switch (prompt.kind) {
    case "revert-file":
      return which === "no"
        ? { state: cleared, effect: null }
        : { state: cleared, effect: { kind: "revert", requestId: nextId(), turn: prompt.shownTurn, scope: prompt.scope, path: prompt.path, target: "file" } };
    case "recover":
      return { state: cleared, effect: { kind: "recover", requestId: nextId(), entry: prompt.entry, answer: which === "yes" ? "restore" : "dismiss" } };
    case "send":
      if (which === "no") return { state: cleared, effect: null };
      if (prompt.preview === null) return { state, effect: null };
      return { state: cleared, effect: { kind: "send", requestId: nextId(), confirm: prompt.preview.digest } };
    case "comment":
      return { state, effect: null };
  }
}

/** Enter in the comment input: a comment with text is saved; an empty one is the same as Escape. */
function accept(state: ReviewState, nextId: () => string): Applied {
  const prompt = state.prompt;
  if (prompt?.kind !== "comment") return { state, effect: null };
  const cleared = { ...state, prompt: null };
  const text = prompt.text.trim();
  if (text === "") return { state: cleared, effect: null };
  const { turn, scope, path, from, to } = prompt;
  return { state: cleared, effect: { kind: "comment-add", requestId: nextId(), turn, scope, path, from, to, text } };
}

function toggle(state: ReviewState, stops: ReviewStop[], at: number, nextId: () => string): { state: ReviewState; effect: ReviewEffect | null } {
  const stop = stops[at];
  const envelope = state.envelope;
  if (stop?.kind === "recovery") {
    const entry = state.recovery.find((r) => r.id === stop.id);
    return entry === undefined ? { state, effect: null } : { state: { ...state, prompt: { kind: "recover", entry: entry.id, path: entry.path } }, effect: null };
  }
  if (stop === undefined || envelope === null || stop.kind === "comment") return { state, effect: null };
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
  const entry = state.diffs.get(path);
  if (entry !== undefined && entry.status !== "failed") return { state: open, effect: null };
  const requestId = nextId();
  return {
    state: { ...open, diffs: withDiff(state.diffs, path, { status: "loading", requestId }) },
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
