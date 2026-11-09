import type { PanelMode } from "./keymap";
import type { RemoteConnectedDevice } from "./bridge";
import type { BackgroundTaskRecord, EditorLink, TurnEnding, UsageInfo } from "./types";
import { turnEndingBandText } from "./turnEnding";
import { escapeText, isHiddenChar, revealedPathText, revealedText } from "./revealHidden";
import { SHORT_FLASH } from "./app/flashes";

/** One segment's identity, in priority order (used only for lookups here --
 *  `bandLayout`'s own construction order, not this list, decides degrade order and render order). */
export type SegId = "mode" | "pill" | "showcmd" | "message" | "prompt" | "warn" | "unread" | "cards" | "approve" | "queue" | "context" | "link" | "position" | "model" | "effort" | "usage" | "review" | "recovery" | "ending" | "background" | "following" | "phone";

/** One piece of the band: the text to show and which side of the gap it belongs on (left of the
 *  gap is mode, pill, `⚑N`, `⧗N`, message; right of it is showcmd, `⚠`, context,
 *  model, usage, position, `↓N`). `StatusBand` groups by `side` when it renders; `bandLayout` itself
 *  never reorders by side, only by priority. */
export type Seg = {
  id: SegId;
  text: string;
  side: "left" | "right";
  /** Set only on `"prompt"`, and on a `"message"` with a `messageLook`, when its full text does not fit the measured
   *  row. `StatusBand` then draws it whole on its own lines above the row: a y/n question says what `y` does (a move
   *  into bypass, the cards it approves), such a flash what the next click opens, and neither is cut. Every other
   *  flash is cut to the row (`cut`). Absent on every other segment, which degrades by priority. */
  wraps?: boolean;
  /** The width this segment keeps, in cells, when its text is narrower: a figure (`position`, `usage`, `unread`)
   *  given a `reserve` by `bandLayoutSteady`, so its text changing does not move the segments beside it. Counted
   *  in place of the text's own width when it is wider. Absent everywhere else. */
  cells?: number;
  /** Set only on `"message"` while it shows its short form (`SHORT_FLASH`) or is cut (`cut`): the whole
   *  sentence, which the shown text leaves words out of, for `StatusBand` to carry as the segment's title. */
  full?: string;
  /** Set only on a `"message"` too wide for the row even so: `StatusBand` draws it on the row, ending in an ellipsis
   *  where the row ends (`.band-message`); a flash ending in a path is already cut in its middle (`cutFlash`). */
  cut?: boolean;
};

/** What the band has to say this render, gathered by `App.tsx` from state that already exists
 *  elsewhere (this crate's own state, the tab's mode, the reducer's projection) -- `bandLayout` is
 *  a pure function over this plus the measured width, so the degrade rule is testable
 *  with no browser. */
export type BandFacts = {
  mode: PanelMode;
  pill: string;
  showcmd: string | null;
  message: string | null;
  /** The part of the flash that must be on screen whole before the action it names may follow: a link's host, shown
   *  before the click that opens it. The band never cuts such a flash: too wide for the row, it is drawn whole on its
   *  own lines above the row, as a long y/n prompt is, and `StatusBand` reports when this part is measured visible
   *  (`onLookShown`). Absent: an ordinary flash, cut to the row. */
  messageLook?: string;
  /** The path the flash ends with, as it is drawn (`revealedPathText`), so a cut keeps its file name: given where the
   *  flash is built from a path (`gf`'s file list). Without it, a flash's last word holding a `/` is taken for one. */
  messagePath?: string;
  prompt: string | null;
  warn: string | null;
  unread: string | null;
  cards: number;
  questions?: number;
  queued: number;
  context: { file: string; lines: [number, number] | null } | null;
  position: string | null;
  model: string | null;
  /** The effort the agent said the session runs at, beside the model. `null`, or absent, is not known, and
   *  says nothing: a level is never guessed. */
  effort?: string | null;
  /** The active tab's last reported usage, already worded (`usageSegment`) -- `text` is the
   *  segment, `title` its tooltip. `null` until a turn has reported one, and always `null` on the
   *  empty tab. */
  usage: UsageFact | null;
  /** The card INPUT's `Ctrl+y` would approve -- the active tab's
   *  oldest waiting card -- set by `App.tsx` only while one waits and the composer has the keys, so
   *  the band says which card that one key answers. Optional: absent (or `null`) is "nothing to
   *  name". */
  approve?: ApproveFact | null;
  /** Companion mode: where the panel stands with the editor beside it. `null`, or absent, is the
   *  one-window mode, which has no such thing and never sends one. While it is set and not
   *  `attached`, its text takes the place of the context segment: with no editor attached no file
   *  can be named. */
  link?: EditorLink | null;
  /** Turn review: a finished turn of this tab changed `files` files on disk, and `c` opens them. `null`,
   *  or absent, says nothing; `keyOnly` omits the count and offers just `c to review`. Never drawn over a
   *  prompt. A narrow band drops it right after the usage figure and the context's shortening, ahead
   *  of the model: it is a pointer, not a state. */
  review?: { files: number; keyOnly?: boolean } | null;
  /** A revert was interrupted and its journal entry is still there: the first one's `path`, and how many
   *  more there are. A half-written file is not a diff notice, so this is drawn whatever `review` says (the
   *  `review.hint` setting turns only the pointer off), and it outlasts every figure and pointer when the
   *  band narrows. `null`, or absent, says nothing. */
  recovery?: { path: string; more: number } | null;
  /** How this tab's latest turn ended when it did not complete (`latestTurnEnding`), until
   *  the next turn starts. `null`, or absent, says nothing: a completed turn, no turn yet, and the
   *  empty tab, which has no turns. On the left beside the state it describes, in the band's own colour like
   *  its neighbours; the muted row in the conversation carries the detail. */
  ending?: Pick<TurnEnding, "kind" | "reason" | "apiErrorStatus" | "message"> | null;
  /** How many commands this tab's agent left running after its turns ended. `0`, `null` or absent says
   *  nothing: optional things show nothing until there is something to show. */
  background?: number | null;
  backgroundTasks?: BackgroundTaskRecord[];
  following?: boolean;
  /** How many phones are connected to this desk right now (remote control). `0`, `null` or absent says nothing.
   *  Unlike every other segment the mark is never dropped, shortened or hidden by a prompt while it is above `0`:
   *  the person at the desk must always see that someone else can watch. */
  phones?: number | null;
  remoteSandbox?: string | null;
  /** The phones connected, for the mark's title: each named with its grant and the `Host` it came through. */
  phoneDevices?: RemoteConnectedDevice[];
  /** A turn of this tab is running. While it is, `StatusBand` redraws the figures at most once a second and keeps
   *  each at the widest it has been (`steadyBand.ts`). Absent is no turn. */
  streaming?: boolean;
  /** Whose band this is (the active tab): `StatusBand` carries nothing it held back or kept wide across a change. */
  scope?: string | number | null;
  /** Milliseconds from the panel's last key to this render; absent when there has been none. A position change
   *  within `OWN_MOVE_MS` of a key outside INSERT is the reader's own move and is drawn at once (`steadyBand.ts`). */
  keyAge?: number;
};

/** The phone mark's tooltip: each connected phone with its grant and the `Host` it came through (a phone's and the
 *  network's words, with their hidden characters written as escapes), and the host's sandbox report. */
export function phoneTitle(sandbox?: string | null, devices: RemoteConnectedDevice[] = []): string {
  const named = devices.map((d) => `${revealedText(d.name)} (${d.grant}, through ${revealedText(d.host)})`).join(", ");
  return `Remote control: connected phones${named === "" ? "" : `: ${named}`}${sandbox && sandbox !== "full" ? "; sandbox: partial" : ""}`;
}

/** The band's phone mark: how many phones are connected. `?` names them. */
export function phoneSegment(count: number): string {
  return `☎${count}`;
}

/** The band's count of the commands an agent left running after its turns ended. */
export function backgroundSegment(running: number): string {
  return `${running} running in background`;
}

/** Live work excludes ambient housekeeping and finished roster entries. */
export function liveTasks(rows: BackgroundTaskRecord[]): BackgroundTaskRecord[] {
  return rows.filter((r) => r.live && !r.ambient && ["running", "paused", "stopping"].includes(r.status));
}

export function backgroundFallback(count: number): string {
  return `${count} background ${count === 1 ? "task" : "tasks"}`;
}

/** Shells and monitors share one CLI type; other mixtures use its generic count. */
export function taskWords(tasks: BackgroundTaskRecord[]): string {
  if (tasks.length === 0) return "";
  const group = (r: BackgroundTaskRecord) => r.task_type || (r.kind === "monitor" ? "shell" : r.kind);
  const kind = tasks[0].kind;
  if (!tasks.every((r) => group(r) === group(tasks[0]))) return backgroundFallback(tasks.length);
  const word = (n: number, singular: string, plural = `${singular}s`) => `${n} ${n === 1 ? singular : plural}`;
  if (kind === "shell" || kind === "monitor") {
    const monitors = tasks.filter((r) => r.kind === "monitor").length;
    return [tasks.length - monitors > 0 ? word(tasks.length - monitors, "shell") : "", monitors > 0 ? word(monitors, "monitor") : ""].filter(Boolean).join(", ");
  }
  if (kind === "local_agent") return word(tasks.length, "local agent");
  if (kind === "workflow") return word(tasks.length, "background dynamic workflow");
  return backgroundFallback(tasks.length);
}

export function elapsedWords(ms: number): string {
  const seconds = Math.max(0, Math.floor(ms / 1000));
  return `${Math.floor(seconds / 60)}m ${String(seconds % 60).padStart(2, "0")}s`;
}

export function tasksLine(tasks: BackgroundTaskRecord[], now: number): string {
  const workflows = tasks.filter((r) => r.kind === "workflow").sort((a, b) => b.started_at - a.started_at);
  const workflow = workflows[0];
  if (!workflow) return `⏵ ${taskWords(tasks)} · /tasks`;
  const agents = workflow.workflow_progress?.agents;
  const done = agents?.filter((a) => ["done", "completed", "failed", "stopped"].includes(a.state)).length;
  const status = workflow.status === "paused" ? "paused" : agents ? `${done}/${agents.length} agents` : "running";
  const tokens = workflow.usage?.total_tokens;
  const usage = tokens == null ? "" : ` · ${tokens >= 1000 ? `${Math.round(tokens / 1000)}k` : tokens} tokens`;
  return `⏵ ${revealedText(workflow.workflow_name ?? workflow.description)} · ${status} · ${elapsedWords(now - workflow.started_at)}${usage} · /tasks${tasks.length > 1 ? ` · +${tasks.length - 1} more` : ""}`;
}

/** The band's effort segment: the agent's own word, read after the model as Claude Code's footer reads
 *  (`opus-5-5 · xhigh`). The word is drawn as text with its hidden characters escaped. */
export function effortSegment(effort: string): string {
  return `· ${revealedText(effort)}`;
}

/** The band's turn-review segment: how many files changed on disk during the last turn, and the key that
 *  shows them, or just the key when `keyOnly`. Says "changed", not who changed them -- the overlay
 *  carries the attribution. */
export function reviewSegment(files: number, keyOnly = false): string {
  if (keyOnly) return "c to review";
  return `${files} ${files === 1 ? "file" : "files"} changed · c to review`;
}

/** The band's notice for an interrupted revert: which file, and that `c` opens the overlay that restores it. */
export function recoverySegment(path: string, more: number): string {
  return `an interrupted revert left ${path}${more > 0 ? ` and ${more} more` : ""} · c to review`;
}

/** The `Ctrl+y` segment: the card's tool name and a one-line summary of its input (`cardSummary`). */
export type ApproveFact = { tool: string; summary: string };

/** How long `cardSummary` lets a card's summary run on the band, the `…` included. */
const CARD_SUMMARY_MAX = 40;

/** A card in one short line for the band -- what the call acts on (its command,
 *  file, path or URL, the fields a person checks before approving), else `summarizeInput`'s own
 *  order's remaining fields, else compact JSON; spaces, tabs and line breaks collapsed, and every
 *  character the card escapes drawn as its escape; cut to `CARD_SUMMARY_MAX` characters in the
 *  middle, head … tail, never inside an escape. A `Bash` card's `description` is the
 *  model's own words about the command, so the command itself comes first. An ACP agent's card is
 *  read inside its `rawInput` by the fields that name the call alone, then as that `rawInput` in compact
 *  JSON, then by its first location, and by its title only when it sent none of those. */
export function cardSummary(input: unknown): string {
  let text: string | null = null;
  if (input !== null && typeof input === "object") {
    const fields = input as Record<string, unknown>;
    text = actsOn(fields);
    // An ACP agent's card (`AcpCardInput`) wraps the call: what it acts on is in the agent's own
    // `rawInput`, read only by the fields that name the call, else the raw input itself as compact JSON,
    // else its first location. A `description`, `prompt` or `name` in an ACP raw input is the agent's
    // account of the call, which could stand in front of the key that does the work (`{description:
    // "Read notes", script: "rm -rf ~"}`), so it is shown only inside the JSON. The `title` and the
    // locations are the agent's words about the call too, so they are named only when it sent no input.
    if (text === null && "rawInput" in fields) {
      const raw = fields.rawInput;
      const locations = Array.isArray(fields.locations) ? fields.locations : [];
      const first = locations[0] as { path?: unknown } | undefined;
      text =
        (raw !== null && typeof raw === "object" && !Array.isArray(raw) ? actsOn(raw as Record<string, unknown>, CALL_KEYS) : null) ??
        rawAsJson(raw) ??
        (typeof first?.path === "string" ? first.path : null) ??
        (typeof fields.title === "string" && fields.title !== "" ? fields.title : null);
    }
  }
  // Only the whitespace the card keeps as text collapses: a no-break or other unusual space is not one the
  // shell splits at, and is drawn as its escape below, as the card draws it.
  const line = (text ?? JSON.stringify(input) ?? "").replace(/[ \t\n]+/g, " ").replace(/^ | $/g, "");
  // One unit per character, a character the card escapes as its whole escape: the band says what the card
  // says (a direction override drawn raw reorders the command after it), and a cut never splits an escape.
  const units = Array.from(line, (ch) => (isHiddenChar(ch) ? escapeText(ch) : ch));
  const size = (unit: string) => Array.from(unit).length;
  if (units.reduce((n, unit) => n + size(unit), 0) <= CARD_SUMMARY_MAX) return units.join("");
  // Cut in the middle, not at the end -- the tail of a long command is often
  // the part that matters (`| sudo bash`, `--force`), and it is what a person approving must see.
  const tailRoom = Math.floor((CARD_SUMMARY_MAX - 1) / 2) + 4;
  const headRoom = CARD_SUMMARY_MAX - 1 - tailRoom;
  const head: string[] = [];
  for (let i = 0, used = 0; i < units.length && used + size(units[i]) <= headRoom; i++) {
    head.push(units[i]);
    used += size(units[i]);
  }
  const tail: string[] = [];
  for (let i = units.length - 1, used = 0; i >= head.length && used + size(units[i]) <= tailRoom; i--) {
    tail.unshift(units[i]);
    used += size(units[i]);
  }
  return `${head.join("")}…${tail.join("")}`;
}

/** The fields that name what a call acts on: its command, file, path, URL or search. */
const CALL_KEYS = ["command", "file_path", "notebook_path", "path", "url", "pattern", "query"] as const;

/** `CALL_KEYS`, then the fields a Claude tool's own input names a call by when it has none of those (a
 *  `Task`'s `description` and `prompt`, an MCP tool's `name`). */
const SUMMARY_KEYS = [...CALL_KEYS, "description", "prompt", "name"] as const;

/** The first of `keys` a call's fields hold, a person checks before approving it, or `null`. A command
 *  given as an argument list (ACP and Codex agents send one) reads as its words joined by spaces, a word
 *  that holds a space quoted (`argvWord`). */
function actsOn(fields: Record<string, unknown>, keys: readonly string[] = SUMMARY_KEYS): string | null {
  for (const key of keys) {
    const value = fields[key];
    if (typeof value === "string") return value;
    if (key === "command" && Array.isArray(value) && value.length > 0 && value.every((v) => typeof v === "string")) {
      return value.map(argvWord).join(" ");
    }
  }
  return null;
}

/** One element of an argument list as a shell would have to be given it: as it is, or in single quotes
 *  when it is empty or holds whitespace or a quote. Joined bare, `["rm", "a b"]` and `["rm", "a", "b"]`
 *  both read `rm a b`, which are a different number of files. */
function argvWord(word: string): string {
  if (word !== "" && !/[\s'"\\]/u.test(word)) return word;
  return `'${word.replace(/'/g, "'\\''")}'`;
}

/** An ACP agent's raw input as compact JSON, or `null` when it carries nothing (absent, `null`, an empty
 *  object or list). */
function rawAsJson(raw: unknown): string | null {
  if (raw === undefined || raw === null) return null;
  if (typeof raw === "object" && Object.keys(raw as object).length === 0) return null;
  return JSON.stringify(raw) ?? null;
}

/** The band's usage segment as `usageSegment` words it: the text drawn, and the tooltip carrying
 *  the breakdown the text has no room for. */
export type UsageFact = { text: string; title: string };

/** vim's East Asian Wide/Fullwidth ranges, counted twice: a CJK file
 *  name or model name must not silently overrun the band's monospace budget. Not a full Unicode
 *  East-Asian-Width table -- just the ranges the fixtures and a long CJK file name such as
 *  (`说明文档非常长的文件名称.md`) actually exercise: CJK Unified Ideographs and their extension
 *  blocks, Hangul syllables and jamo, and the fullwidth forms block. */
const WIDE = /[ᄀ-ᅟ⺀-꓏가-힣豈-﫿︰-﹏＀-｠￠-￦]/u;

/** Character count with East Asian wide characters counted twice, as a terminal draws them. Iterates by code
 *  point, not UTF-16 unit, so a codepoint outside the BMP is never split into two "wide" halves. */
export function textWidth(text: string): number {
  let n = 0;
  for (const ch of text) n += WIDE.test(ch) ? 2 : 1;
  return n;
}

/** A y/n prompt never truncates: at WebKit zoom 1.5 a 47-column band must still show what `y` does and the prompt's
 *  `(y/n)`. Too wide for the row, it is marked `wraps` and `StatusBand` draws it whole on its own lines above the band's
 *  row (`.status-band-above`) until the question is answered; so is a flash with a `messageLook`. The band's own row is
 *  one editor row always: every other flash that does not fit it is cut (`Seg.cut`), so a flash coming or going never
 *  moves the conversation or the composer. Ordinary segments degrade by priority. */

/** Where a flash's closing path starts and the flash with that path made visible: `path` when the caller names it
 *  (the text ends with it, already drawn visible), else the flash's last run of non-space text holding a `/`, with the
 *  spaces after it, which `revealedPathText` writes as escapes. `null` when it ends in no path. */
function closingPath(text: string, path?: string): { text: string; start: number } | null {
  if (path !== undefined && path !== "" && text.endsWith(path)) return { text, start: text.length - path.length };
  const m = /\S*\/\S*\s*$/u.exec(text);
  if (m === null || !m[0].includes("/")) return null;
  const start = m.index;
  return { text: text.slice(0, start) + revealedPathText(m[0]), start };
}

/** A flash that ends in a path (`path`, or its last word holding a `/`), with the spaces that end the path made visible,
 *  and cut to at most `cells` cells so that the path's end -- the whole file name, spaces and all, where that leaves
 *  4 cells of the start -- stays and its middle goes: `gf: 2 of 5: src/co…ry/StatusBand.tsx`. A file name too long for
 *  that keeps as much of its end as leaves those 4 cells. Any other flash comes back unchanged, left for the engine to
 *  end in an ellipsis at the row's edge (`.band-message`, `text-overflow`), which uses the room to the pixel where this
 *  cell count is conservative; so does one that fits, or a room under 8 cells. */
export function cutFlash(text: string, cells: number, path?: string): string {
  const closing = closingPath(text, path);
  if (closing === null) return text;
  const shown = closing.text;
  if (textWidth(shown) <= cells) return shown;
  const room = cells - 1;
  if (room < 8) return shown;
  const chars = [...shown];
  // The longest prefix (by code point) at most `n` cells wide; and the longest suffix.
  const head = (n: number) => {
    let w = 0;
    let i = 0;
    while (i < chars.length && w + textWidth(chars[i]) <= n) w += textWidth(chars[i++]);
    return chars.slice(0, i).join("");
  };
  const tail = (n: number) => {
    let w = 0;
    let i = chars.length;
    while (i > 0 && w + textWidth(chars[i - 1]) <= n) w += textWidth(chars[--i]);
    return chars.slice(i).join("");
  };
  const pathText = shown.slice(closing.start);
  // The file name with its `/` (the whole path when it has none: a file at the project's root), measured on the path
  // as shown: its spaces, end ones written as escapes, included.
  const slash = pathText.lastIndexOf("/");
  const name = textWidth(slash < 0 ? pathText : pathText.slice(slash));
  const half = Math.min(textWidth(pathText), Math.floor(room / 2));
  // Neither side keeps part of an escape (`⟨U+0020⟩`): a half escape reads as neither the character nor the text.
  let end = tail(name + 4 <= room ? Math.max(name, half) : room - 4);
  const open = end.indexOf("⟨");
  const close = end.indexOf("⟩");
  if (close >= 0 && (open < 0 || close < open)) end = end.slice(close + 1);
  let start = head(room - textWidth(end));
  if (start.lastIndexOf("⟨") > start.lastIndexOf("⟩")) start = start.slice(0, start.lastIndexOf("⟨"));
  return `${start.trimEnd()}…${end}`;
}

const PAD = 2; // one character of padding each side
/** The mode block's separator arrow, one cell after it (`index.css`, `.band-mode::after`). */
const MODE_ARROW = 1;
// CARET / VISUAL / V-LINE (V-LINE's own word from lualine: utils/mode.lua:17,19, what a LazyVim
// statusline says; CARET has no lualine precedent -- Eitri-only, qutebrowser's own caret mode is the
// source), narrow C / V / VL.
// The typing mode reads INSERT: the band's mode block sits beside the editor's and is drawn like it, so
// the mode where typing lands is called what vim calls it there.
const MODE_TEXT: Record<PanelMode, string> = { input: "INSERT", browse: "BROWSE", hint: "HINT", caret: "CARET", visual: "VISUAL", vline: "V-LINE" };
const MODE_LETTER: Record<PanelMode, string> = { input: "I", browse: "B", hint: "H", caret: "C", visual: "V", vline: "VL" };

/** The bottom band's degrade rule, a pure function of the facts and the measured
 *  width so it is testable without a browser. `widthPx <= 0` (before the first measurement)
 *  or `charPx <= 0` (the hidden measuring span not yet laid out) shows only mode
 *  and pill -- priorities 1 and 2's own floor, never dropped once a real width is known either. */
export function bandLayout(f: BandFacts, widthPx: number, charPx: number): Seg[] {
  return bandLayoutSteady(f, widthPx, charPx, { hold: null, now: 0 }).segs;
}

/** Room, in cells, beyond the bare fit a segment dropped for room needs before it comes back. */
export const RETURN_MARGIN = 3;
/** How long, in milliseconds, after the band last dropped something for room, before anything comes back. */
export const RETURN_COOLDOWN_MS = 1000;

/** What the band dropped or shortened for room on its last layout -- each such segment's id and the cells it was
 *  drawn in then, `0` for gone -- and when it last took a segment narrower than before. Kept by `StatusBand` between
 *  renders, and let go on a tab switch. */
export type LayoutHold = { dropped: Partial<Record<SegId, number>>; droppedAt: number };

/** `bandLayout` with memory, so nothing on the band oscillates. A segment dropped for room comes back only when it
 *  fits with `RETURN_MARGIN` cells to spare, and no sooner than `RETURN_COOLDOWN_MS` after the band last dropped
 *  something; one that no longer fits still goes at once. `reserve` gives figures the width they keep (`cells` on
 *  the segment, counted toward the fit). `due` is when something held out may come back, `null` if nothing waits. */
export function bandLayoutSteady(
  f: BandFacts,
  widthPx: number,
  charPx: number,
  opts: { hold: LayoutHold | null; now: number; reserve?: Partial<Record<SegId, number>> },
): { segs: Seg[]; hold: LayoutHold | null; due: number | null } {
  const { hold, now } = opts;
  const unchanged = (segs: Seg[]) => ({ segs, hold, due: null });
  const mode: Seg = { id: "mode", text: MODE_TEXT[f.mode], side: "left" };
  const pill: Seg = { id: "pill", text: f.pill, side: "left" };
  const phone: Seg[] = f.phones && f.phones > 0 ? [{ id: "phone", text: phoneSegment(f.phones), side: "left" }] : [];
  if (widthPx <= 0 || charPx <= 0) return unchanged([mode, pill, ...phone]);
  const budget = Math.floor(widthPx / charPx);
  if (f.prompt !== null) {
    // The question is the band's whole business; a flash that comes while it is up (a notification) is not part of
    // it, and has the row's one-row place beside it: cut when it does not fit, never joining the question's lines.
    const taken = phone.reduce((n, x) => n + textWidth(x.text) + PAD, 0);
    const roomForPrompt = budget - textWidth(mode.text) - MODE_ARROW - 2 * PAD - taken;
    const prompt: Seg = { id: "prompt", text: f.prompt, side: "left", wraps: textWidth(f.prompt) > roomForPrompt };
    if (f.message === null) return unchanged([mode, prompt, ...phone]);
    const look = f.messageLook !== undefined;
    const onRow = (m: Seg) => [mode, ...(prompt.wraps ? [] : [prompt]), m, ...phone];
    const rowWidth = (s: Seg[]) => s.reduce((n, x) => n + textWidth(x.text) + PAD + (x.id === "mode" ? MODE_ARROW : 0), 0);
    let message: Seg = { id: "message", text: f.message, side: "left" };
    const over = rowWidth(onRow(message)) - budget;
    if (over > 0 && look) message = { ...message, wraps: true };
    else if (over > 0) {
      const room = Math.max(1, textWidth(message.text) - over);
      message = { ...message, text: cutFlash(message.text, room, f.messagePath), full: message.text, cut: true };
    } else {
      message = { ...message, text: closingPath(message.text, f.messagePath)?.text ?? message.text };
    }
    return unchanged([mode, prompt, message, ...phone]);
  }
  const ctxFull = f.context && (f.context.lines ? `⧉ ${f.context.file}:${f.context.lines[0]}-${f.context.lines[1]}` : `⧉ ${f.context.file}`);
  const link = f.link && f.link.state !== "attached" && f.link.text !== "" ? f.link : null;
  const tasks = f.backgroundTasks === undefined ? null : liveTasks(f.backgroundTasks);
  const background = tasks === null ? (f.background ? backgroundSegment(f.background) : "") : taskWords(tasks);
  let segs: Seg[] = [
    mode,
    pill,
    ...(f.cards + (f.questions ?? 0) > 0 ? [{ id: "cards", text: `⚑${f.cards + (f.questions ?? 0)}`, side: "left" } as Seg] : []),
    ...(f.approve ? [{ id: "approve", text: `Ctrl+y approves ${revealedText(f.approve.tool)}: ${f.approve.summary}`, side: "left" } as Seg] : []),
    ...(f.queued > 0 ? [{ id: "queue", text: `⧗${f.queued}`, side: "left" } as Seg] : []),
    ...phone,
    ...(background ? [{ id: "background", text: background, side: "left" } as Seg] : []),
    ...(f.following ? [{ id: "following", text: "following", side: "left" } as Seg] : []),
    ...(f.ending ? [{ id: "ending", text: turnEndingBandText(f.ending), side: "left" } as Seg] : []),
    ...(f.recovery ? [{ id: "recovery", text: recoverySegment(f.recovery.path, f.recovery.more), side: "left" } as Seg] : []),
    ...(f.message ? [{ id: "message", text: f.message, side: "left" } as Seg] : []),
    ...(f.showcmd ? [{ id: "showcmd", text: f.showcmd, side: "right" } as Seg] : []),
    ...(f.warn ? [{ id: "warn", text: "⚠", side: "right" } as Seg] : []),
    ...(link ? [{ id: "link", text: link.text, side: "right" } as Seg] : []),
    ...(ctxFull && !link ? [{ id: "context", text: ctxFull, side: "right" } as Seg] : []),
    ...(f.review && f.review.files > 0 ? [{ id: "review", text: reviewSegment(f.review.files, f.review.keyOnly), side: "right" } as Seg] : []),
    ...(f.model ? [{ id: "model", text: f.model, side: "right" } as Seg] : []),
    ...(f.model && f.effort ? [{ id: "effort", text: effortSegment(f.effort), side: "right" } as Seg] : []),
    ...(f.usage ? [{ id: "usage", text: f.usage.text, side: "right" } as Seg] : []),
    ...(f.position ? [{ id: "position", text: f.position, side: "right" } as Seg] : []),
    ...(f.unread ? [{ id: "unread", text: f.unread, side: "right" } as Seg] : []),
  ];
  const reserve = opts.reserve ?? {};
  segs = segs.map((x) => (reserve[x.id] !== undefined && reserve[x.id]! > textWidth(x.text) ? { ...x, cells: reserve[x.id] } : x));
  const cellsOf = (x: Seg) => Math.max(textWidth(x.text), x.cells ?? 0);
  const width = (s: Seg[]) => s.reduce((n, x) => n + cellsOf(x) + PAD + (x.id === "mode" ? MODE_ARROW : 0), 0);
  const steps: ((s: Seg[]) => Seg[])[] = [
    (s) => tasks === null ? s : s.map((x) => x.id === "background" ? { ...x, text: backgroundFallback(tasks.length) } : x),
    // The usage figure is the least urgent thing on the band -- it goes before even the context
    // is shortened, so a narrow panel never trades the file it is showing for a running total.
    (s) => s.filter((x) => x.id !== "usage"),
    (s) => s.map((x) => (x.id === "context" && f.context ? { ...x, text: `⧉ ${f.context.file}` } : x)),
    // The review pointer is the only place the band offers `c`, so it gives up its count before any figure goes,
    // and outlasts the effort, the model and the position: they say what the user chose or where they are, and
    // come back by themselves; the pointer is gone with the next turn.
    (s) => s.map((x) => (x.id === "review" && f.review ? { ...x, text: reviewSegment(f.review.files, true) } : x)),
    // The effort qualifies the model, so it goes first, and never stands without it.
    (s) => s.filter((x) => x.id !== "effort"),
    (s) => s.filter((x) => x.id !== "model"),
    (s) => s.filter((x) => x.id !== "position"),
    (s) => s.filter((x) => x.id !== "review"),
    (s) => s.filter((x) => x.id !== "context" && x.id !== "link"),
    (s) => s.filter((x) => x.id !== "queue"),
    (s) => s.filter((x) => x.id !== "following"),
    // The Ctrl+y segment keeps the tool it names before it goes, and goes before the card
    // count -- `⚑N` is shorter, and still says a card waits.
    (s) => s.map((x) => (x.id === "approve" && f.approve ? { ...x, text: `Ctrl+y approves ${revealedText(f.approve.tool)}` } : x)),
    (s) => s.filter((x) => x.id !== "approve"),
    (s) => s.filter((x) => x.id !== "cards"),
    // Something still runs and may change files: it outlasts the figures and pointers, and the card count.
    (s) => s.filter((x) => x.id !== "background"),
    // A turn that ended badly is short and it explains why the panel went quiet, so it outlasts every
    // figure and pointer above and the card count too, and goes only just before the mode shrinks to
    // its letter: the row in the conversation says the same thing in full.
    (s) => s.filter((x) => x.id !== "ending"),
    // Not a figure or a pointer but a file left half-written: it is shortened to the file's name before it
    // goes, and goes last, with only the mode left.
    (s) => s.map((x) => (x.id === "recovery" && f.recovery ? { ...x, text: recoverySegment(f.recovery.path.split("/").pop() ?? f.recovery.path, f.recovery.more) } : x)),
    (s) => s.filter((x) => x.id !== "recovery"),
  ];
  // How many steps it takes to fit `room`; then the band at that many steps.
  const all = segs;
  const levelFor = (room: number) => {
    let s = all;
    let n = 0;
    for (const apply of steps) {
      if (width(s) <= room) break;
      s = apply(s);
      n++;
    }
    return n;
  };
  // The band after each number of steps, and which of its segments that many steps dropped or shortened. The steps
  // only ever take more away, so a later step's set holds every earlier one's.
  const levels: Seg[][] = [all];
  for (const apply of steps) levels.push(apply(levels[levels.length - 1]));
  const shownCells = (s: Seg[], id: SegId) => {
    const x = s.find((y) => y.id === id);
    return x === undefined ? 0 : cellsOf(x);
  };
  const affected = (s: Seg[]) => {
    const out: Partial<Record<SegId, number>> = {};
    for (const x of all) if (!s.some((y) => y.id === x.id && y.text === x.text)) out[x.id] = shownCells(s, x.id);
    return out;
  };
  const fit = levelFor(budget);
  let level = fit;
  let due: number | null = null;
  if (hold !== null) {
    // Only a segment the band really dropped or shortened last time is held -- drawn no wider than it was then; one
    // that has appeared since is laid out on its own merits. A held segment comes back (or back to its full text)
    // only with room to spare, and not within a second of the band's last drop.
    const held = (Object.entries(hold.dropped) as [SegId, number][]).filter(([id]) => all.some((x) => x.id === id));
    const keeps = (n: number) => held.every(([id, cells]) => shownCells(levels[n], id) <= cells);
    let keep = fit;
    while (keep < steps.length && !keeps(keep)) keep++;
    if (keep > fit) {
      const roomy = Math.max(fit, levelFor(budget - RETURN_MARGIN));
      if (now - hold.droppedAt < RETURN_COOLDOWN_MS) {
        level = keep;
        due = hold.droppedAt + RETURN_COOLDOWN_MS;
      } else {
        level = Math.min(keep, roomy);
      }
    }
  }
  const dropped = affected(levels[level]);
  const fresh = (Object.entries(dropped) as [SegId, number][]).some(
    ([id, cells]) => hold === null || hold.dropped[id] === undefined || cells < hold.dropped[id]!,
  );
  const droppedAt = fresh ? now : hold === null ? -Infinity : hold.droppedAt;
  segs = levels[level];
  // A flash too wide for the row gives way to its short form, if it has one (`SHORT_FLASH`), before it is cut: the
  // short form keeps what to do, where a cut keeps only the sentence's start. Only where it helps: the whole sentence
  // beside the mode's letter comes first.
  const short = f.message === null ? undefined : SHORT_FLASH.get(f.message);
  const letter = (s: Seg[]) => s.map((x) => (x.id === "mode" ? { ...x, text: MODE_LETTER[f.mode] } : x));
  if (width(segs) > budget && short !== undefined && width(letter(segs)) > budget) {
    segs = segs.map((x) => (x.id === "message" ? { ...x, text: short, full: x.text } : x));
  }
  // The mode shrinks to its letter last, and beside a flash only when the letter is what lets everything fit: a flash
  // cut anyway keeps the mode its word, so the mode block does not change width with every long flash.
  if (width(segs) > budget) {
    const shortened = letter(segs);
    if (!segs.some((x) => x.id === "message") || width(shortened) <= budget) segs = shortened;
  }
  // Still too wide: the flash is cut to the row, so the band stays one editor row and nothing above it moves -- by the
  // engine's ellipsis at the row's edge, or, for a flash ending in a path, here in its middle (`cutFlash`). The whole
  // sentence stays the segment's title.
  const over = width(segs) - budget;
  if (over > 0 && f.messageLook !== undefined) {
    segs = segs.map((x) => (x.id === "message" ? { ...x, wraps: true } : x));
  } else if (over > 0) {
    segs = segs.map((x) => {
      if (x.id !== "message") return x;
      const room = Math.max(1, textWidth(x.text) - over);
      return { ...x, text: cutFlash(x.text, room, f.messagePath), full: x.full ?? x.text, cut: true };
    });
  } else {
    // It fits: a closing path's end spaces are still drawn visible.
    segs = segs.map((x) => (x.id === "message" ? { ...x, text: closingPath(x.text, f.messagePath)?.text ?? x.text } : x));
  }
  return { segs, hold: { dropped, droppedAt }, due };
}

/** The model without its leading `claude-`, which every Claude model carries: `"claude-sonnet-5"` ->
 *  `"sonnet-5"`. A model that never carried the prefix (or `null`, before a session reports one)
 *  passes through unchanged. */
export function shortModel(model: string | null): string | null {
  if (model === null) return null;
  return model.startsWith("claude-") ? model.slice("claude-".length) : model;
}

/** `1234` -> `1.2k`, `1_200_000` -> `1.2M`: one decimal, and no trailing `.0`. A figure that would
 *  round up to `1000k` reads `1M` instead (`999_950` is the first such). */
function tokens(n: number): string {
  const scaled = (value: number, unit: string) => `${value.toFixed(1).replace(/\.0$/, "")}${unit}`;
  if (n < 1000) return `${n}`;
  return n < 999_950 ? scaled(n / 1e3, "k") : scaled(n / 1e6, "M");
}

/** Cents, except that a real cost below half a cent reads `<$0.01` rather than `$0.00`: a session that
 *  spent something never looks like one that spent nothing. */
function dollars(c: number): string {
  return c > 0 && c < 0.005 ? "<$0.01" : `$${c.toFixed(2)}`;
}

/** One formatter for the tooltip: `toLocaleString` builds a new `Intl.NumberFormat` on every call,
 *  and this runs on every render of the panel while a reply streams. */
const GROUPED = new Intl.NumberFormat("en-US");

/** Every token the provider counted (the prompt's three parts and the output) and the cost -- the
 *  latest report, never a sum; `null` until one arrives, never `0 tok $0.00` for unknown. A backend that
 *  reports a cost and no tokens (legacy) shows the cost alone. A cost that is not a finite number is no
 *  measurement either (Rust drops such a report whole, `usage_from_proto`; this keeps one that slipped
 *  through from throwing inside a render). A report with `cost_reported: false` shows its tokens and no
 *  cost, never `$0.00`. */
export function usageSegment(u: UsageInfo | null): UsageFact | null {
  if (u === null) return null;
  // `cost_reported: false` (Codex): the provider counted tokens and priced nothing, so `total_cost_usd`
  // is a placeholder. Show the tokens alone, and nothing at all when there are none either.
  const priced = u.cost_reported !== false;
  if (priced && !Number.isFinite(u.total_cost_usd)) return null;
  const t = u.tokens;
  const total = t === null ? null : t.input + t.output + t.cache_creation + t.cache_read;
  if (!priced && total === null) return null;
  const parts =
    t === null
      ? []
      : [
          `input ${GROUPED.format(t.input)}`,
          `output ${GROUPED.format(t.output)}`,
          `cache write ${GROUPED.format(t.cache_creation)}`,
          `cache read ${GROUPED.format(t.cache_read)}`,
        ];
  if (!priced) {
    return { text: `${tokens(total!)} tok`, title: `${parts.join(" · ")} — since this tab started or resumed; no cost reported` };
  }
  const text = total === null ? dollars(u.total_cost_usd) : `${tokens(total)} tok ${dollars(u.total_cost_usd)}`;
  const what = "since this tab started or resumed; /clear starts it over; Claude Code's own estimate, not a bill";
  const title = `${[...parts, `$${u.total_cost_usd.toFixed(4)}`].join(" · ")} — ${what}`;
  return { text, title };
}
