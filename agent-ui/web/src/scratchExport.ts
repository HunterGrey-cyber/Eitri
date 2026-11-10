import { marked } from "marked";
import { outputText } from "./copyText";
import { fenced, pathsIn } from "./paths";
import { noticeText, wakeText, type TimelineItem } from "./timeline";
import { turnEndingText } from "./turnEnding";
import { toolInput } from "./yank";
import type { HistoryNotice, SubagentStream, ToolCallRecord } from "./types";

const MAX_CHARACTERS = 1_000_000;
type CardState = "waiting" | "approved" | "denied";
type ExportOptions = {
  /** What the tab's restore left out, if it said (`AgentUiState.history`). */
  history: HistoryNotice | null;
  /** Answered cards remain drawn until the host resolves them; the timeline itself holds only requests. */
  cardStates?: ReadonlyMap<string, CardState>;
};

/** Only headings written by the exporter carry a literal prompt marker in the editor buffer.
 *  Encode every occurrence in supplied text, regardless of its Markdown context. */
function provenance(text: string): string {
  return text.replace(/▍/g, "&#x258D;");
}

/** Ask the page's Markdown parser whether a following column-0 paragraph would be swallowed by a top-level
 *  block. List and blockquote containers end before that paragraph, so their fences need no extra closer.
 *  The patterns only read the opener of the block the parser found; they do not track Markdown structure. */
function closeOpenBlocks(text: string): string {
  const boundary = "eitri_export_boundary";
  const last = marked.lexer(`${text}\n\n${boundary}`).at(-1);
  if (last === undefined || !last.raw.endsWith(boundary)) return text;
  if (last.type === "code") {
    const fence = /^( {0,3})(`{3,}|~{3,})/.exec(last.raw);
    if (fence !== null) return `${text}\n${fence[1]}${fence[2]}`;
  } else if (last.type === "html") {
    const tag = /^ {0,3}<(pre|script|style|textarea)\b/i.exec(last.raw);
    const closer = tag !== null ? `</${tag[1].toLowerCase()}>`
      : /^ {0,3}<!--/.test(last.raw) ? "-->"
      : /^ {0,3}<\?/.test(last.raw) ? "?>"
      : /^ {0,3}<!\[CDATA\[/.test(last.raw) ? "]]>"
      : /^ {0,3}<![A-Za-z]/.test(last.raw) ? ">" : null;
    if (closer !== null) return `${text}\n${closer}`;
  }
  return text;
}

const quote = (text: string): string => text.split("\n").map((line) => `> ${line}`).join("\n");
const oneLine = (text: string): string => text.replace(/[\r\n]+/g, " ");

function children(stream: SubagentStream, options: ExportOptions): string {
  const items: TimelineItem[] = [
    ...stream.transcript.map((message): TimelineItem => ({ kind: "message", key: `m-${message.seq}`, ...message })),
    ...stream.toolCalls.map((call): TimelineItem => ({ kind: "tool", key: `t-${call.seq}`, seq: call.seq, call })),
  ];
  items.sort((a, b) => a.seq - b.seq);
  const text = items.map((item) => renderItem(item, options)).filter(Boolean).join("\n\n");
  return text ? quote(text) : "";
}

function tool(call: ToolCallRecord, options: ExportOptions): string {
  const item: TimelineItem = { kind: "tool", seq: call.seq, key: `t-${call.seq}`, call };
  const output = outputText(item);
  const block = fenced(provenance(`${call.name}: ${toolInput(call.name, call.input)}${output === null ? "" : `\n${output}`}`));
  const paths = pathsIn(item).map(({ path, line }) => closeOpenBlocks(provenance(`${path}${line === null ? "" : `:${line}`}`)));
  const nested = call.children ? children(call.children, options) : "";
  return [block, ...paths, nested].filter(Boolean).join("\n\n");
}

function renderItem(item: TimelineItem, options: ExportOptions): string {
  switch (item.kind) {
    case "prompt":
    case "message":
      return closeOpenBlocks(provenance(item.text));
    case "tool":
      return tool(item.call, options);
    case "run":
      return item.calls.map((call) => tool(call, options)).join("\n\n");
    case "permission": {
      const card = item.request;
      const state = options.cardStates?.get(card.permissionId) ?? "waiting";
      return provenance(`> card: ${oneLine(card.toolName)}: ${oneLine(toolInput(card.toolName, card.input))} — ${state}`);
    }
    case "ending":
      return quote(provenance(turnEndingText(item.ending, item.ending.resetsAt ?? null)));
    case "notice":
      return quote(provenance(noticeText(item.notice)));
    case "wake":
      return quote(provenance(wakeText(item)));
    case "thinking":
      return "";
  }
}

/** The first line when the tab's restore left something out: turns, records whose count is unknown, or items. */
function notLoadedLine(history: HistoryNotice | null): string {
  if (history === null) return "";
  const turns = history.omittedTurns ?? 0;
  if (turns > 0) return `(${turns} earlier ${turns === 1 ? "turn was" : "turns were"} not loaded into this tab)`;
  // An agent's replay counts turns only: it has no items to leave out.
  if (history.source === "acp_replay") return "";
  const items = history.omittedItems;
  if (items === null) return "(earlier records were not loaded into this tab; count unknown)";
  if (items > 0) return `(${items} earlier ${items === 1 ? "item was" : "items were"} not restored into this tab)`;
  return "";
}

const counted = (n: number, one: string, many: string): string => `${n} ${n === 1 ? one : many}`;

/** A Markdown snapshot of the loaded timeline, retaining whole newest turns within the transport's cap. */
export function exportConversation(timeline: readonly TimelineItem[], options: ExportOptions): string {
  const total = timeline.filter((item) => item.kind === "prompt" && !item.delivered).length;
  // A turn runs from a turn-opening prompt to the next; rows before the first such prompt are a group of their own.
  const turns: { parts: string[]; prompts: number }[] = [];
  let ordinal = 0;
  for (const item of timeline) {
    const startsTurn = item.kind === "prompt" && !item.delivered;
    const last = turns[turns.length - 1];
    if (last === undefined || (startsTurn && (last.prompts > 0 || last.parts.length > 0))) {
      turns.push({ parts: [], prompts: 0 });
    }
    const turn = turns[turns.length - 1];
    let text = renderItem(item, options);
    if (item.kind === "prompt") {
      if (startsTurn) { ordinal++; turn.prompts++; }
      text = `## ▍You · ${item.delivered ? "read during the turn" : `prompt ${ordinal}/${total}`}\n\n${text}`;
    }
    if (text) turn.parts.push(text);
  }
  const bodies = turns.map((turn) => turn.parts.join("\n\n"));
  let size = bodies.reduce((sum, body) => sum + body.length + 2, 0);
  let first = 0;
  let omitted = 0;
  let rows = 0;
  const restored = notLoadedLine(options.history);
  const capLine = () => {
    if (omitted === 0 && rows === 0) return "";
    const turnsOut = counted(omitted, "earlier turn", "earlier turns");
    const rowsOut = counted(rows, "earlier row", "earlier rows");
    const what = rows === 0 ? turnsOut : omitted === 0 ? rowsOut : `${rowsOut} and ${turnsOut}`;
    return `(${what} left out: this view holds at most 1,000,000 characters)`;
  };
  const prefix = () => [restored, capLine()].filter(Boolean).join("\n");
  const length = () => {
    const note = prefix();
    return Math.max(0, size - 2) + note.length + (note && first < bodies.length ? 2 : 0);
  };
  while (first < bodies.length && length() > MAX_CHARACTERS) {
    size -= bodies[first].length + 2;
    if (turns[first].prompts > 0) omitted += turns[first].prompts;
    else rows += turns[first].parts.length;
    first++;
  }
  return [prefix(), bodies.slice(first).join("\n\n")].filter(Boolean).join("\n\n");
}
