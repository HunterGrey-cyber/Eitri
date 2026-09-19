import { memo } from "react";
import type { ReactNode } from "react";
import type { ToolCallRecord } from "./types";

export type ToolRenderConfig = {
  label: string;
  /** Renders the INVOCATION only -- what the model asked the tool to do.
   *
   * This used to be `(input, result, isError)`, and all eight registered renderers ignored the last
   * two arguments: a finished Bash call and one still executing produced byte-identical markup, and
   * only an error was ever printed. The result is now rendered by `ToolResult` below, on one shared
   * path for every tool, so "still running" and "finished" cannot end up distinguishable for some
   * tools and not others. A per-tool result view can be added back as a separate optional field the
   * day one is genuinely wanted -- what must not come back is a declared parameter nothing uses. */
  renderInvocation: (input: unknown) => ReactNode;
};

/** How much of a tool result is rendered inline, head and tail respectively.
 *
 * A real `Bash` or `Read` result is unbounded -- a build log or a large file arrives in full -- and
 * dumping it into an append-only transcript costs the whole panel's scroll position and layout.
 * Head AND tail rather than a single head cap: a failing command explains itself on its LAST lines,
 * which is exactly what a head-only budget throws away. Anything within head+tail is shown
 * untouched; beyond it, the elided middle is replaced by a count of the code points it hid.
 *
 * These two are UTF-16 code-unit budgets, because that is what bounds the rendered size. The count
 * shown to the reader is deliberately NOT in the same unit -- see `countCodePoints`. */
export const RESULT_HEAD_CHARS = 2000;
export const RESULT_TAIL_CHARS = 500;

/** A tool result's content as displayable text.
 *
 * `content` is whatever the provider put on the wire (`serde_json::Value` on the Rust side), so it
 * is a bare string for the common Bash/Read case and an array of content blocks often enough that
 * `String(content)` would render "[object Object]" in real conversations. */
export function formatResultContent(content: unknown): string {
  if (typeof content === "string") return content;
  if (content === null || content === undefined) return "";
  // JSON.stringify returns undefined for a bare `undefined`, already handled above; the ?? guards
  // the remaining exotic cases (a function, a symbol) rather than rendering the string "undefined".
  return JSON.stringify(content, null, 2) ?? String(content);
}

/** Counts Unicode code points, which is what a reader means by "characters".
 *
 * `String.prototype.length` and `slice` are both UTF-16 code-unit based, and an astral character
 * (any emoji, a CJK extension-B ideograph, a mathematical alphanumeric) is two units. Reporting
 * `text.length` arithmetic as a character count therefore overstates by up to 2x for exactly the
 * output most likely to contain them, and the whole point of showing the number is to let a reader
 * judge whether going to find the real output is worth it.
 *
 * One pass rather than `Array.from(text).length`: a real build log is megabytes and this runs on
 * every truncation. A lone surrogate -- possible in genuinely broken tool output -- counts as one,
 * which is also how it renders. */
export function countCodePoints(text: string): number {
  let count = 0;
  for (let i = 0; i < text.length; i++) {
    const unit = text.charCodeAt(i);
    if (unit >= 0xd800 && unit <= 0xdbff && i + 1 < text.length) {
      const next = text.charCodeAt(i + 1);
      if (next >= 0xdc00 && next <= 0xdfff) i++;
    }
    count++;
  }
  return count;
}

const HIGH_SURROGATE_FIRST = 0xd800;
const HIGH_SURROGATE_LAST = 0xdbff;
const LOW_SURROGATE_FIRST = 0xdc00;
const LOW_SURROGATE_LAST = 0xdfff;

/** Where the head cut lands, moved back by one unit when the budget would fall between the halves
 * of a surrogate pair. Keeping the high half alone renders it as U+FFFD -- a character the tool
 * never emitted, at the seam this function is supposed to make legible. Back rather than forward:
 * the budget is a cap, so spending one more unit than it allows is the wrong direction. */
function headEnd(text: string, budget: number): number {
  const lastKept = text.charCodeAt(budget - 1);
  const firstDropped = text.charCodeAt(budget);
  const splitsAPair =
    lastKept >= HIGH_SURROGATE_FIRST &&
    lastKept <= HIGH_SURROGATE_LAST &&
    firstDropped >= LOW_SURROGATE_FIRST &&
    firstDropped <= LOW_SURROGATE_LAST;
  return splitsAPair ? budget - 1 : budget;
}

/** The mirror of `headEnd` for the tail: moved forward by one unit when the budget would start on
 * the low half of a pair whose high half is being elided. */
function tailStart(text: string, budget: number): number {
  const start = text.length - budget;
  const firstKept = text.charCodeAt(start);
  const lastDropped = text.charCodeAt(start - 1);
  const splitsAPair =
    firstKept >= LOW_SURROGATE_FIRST &&
    firstKept <= LOW_SURROGATE_LAST &&
    lastDropped >= HIGH_SURROGATE_FIRST &&
    lastDropped <= HIGH_SURROGATE_LAST;
  return splitsAPair ? start + 1 : start;
}

/** `hiddenChars` is a code-point count (see `countCodePoints`); the head/tail budgets it is derived
 * from are code-unit caps. The two units are deliberately different and neither is a guess: one
 * bounds how much is drawn, the other is what gets reported to a human. */
export function truncateResult(text: string): { shown: string; hiddenChars: number } {
  if (text.length <= RESULT_HEAD_CHARS + RESULT_TAIL_CHARS) return { shown: text, hiddenChars: 0 };
  const head = text.slice(0, headEnd(text, RESULT_HEAD_CHARS));
  const tail = text.slice(tailStart(text, RESULT_TAIL_CHARS));
  const elided = text.slice(head.length, text.length - tail.length);
  const hiddenChars = countCodePoints(elided);
  // Singular is unreachable as this stands. The guard below refuses to print a marker that costs
  // more text than it saves, and the marker is 29 characters at a two-digit count -- so anything
  // it does print elided at least 30 code UNITS, i.e. at least 15 code points. The branch stays
  // anyway: one ternary, and the sentence stays correct if that guard is ever relaxed. It has no
  // test of its own on purpose, since a test for it would assert a case this cannot produce.
  const noun = hiddenChars === 1 ? "character" : "characters";
  const marker = `\n… ${hiddenChars.toLocaleString("en-US")} ${noun} not shown …\n`;
  // A marker longer than what it elides would make the panel show MORE text and tell the reader
  // less. Genuinely reachable rather than theoretical: the budget check above is a code-unit
  // comparison, so a string a few units past it can have only a handful of code points to hide.
  if (marker.length >= elided.length) return { shown: text, hiddenChars: 0 };
  return { shown: `${head}${marker}${tail}`, hiddenChars };
}

function commandOf(input: unknown): string {
  if (input && typeof input === "object" && "command" in input) {
    return String((input as { command: unknown }).command);
  }
  return JSON.stringify(input);
}

function pathOf(input: unknown, key: string): string {
  if (input && typeof input === "object" && key in input) {
    return String((input as Record<string, unknown>)[key]);
  }
  return JSON.stringify(input);
}

export const TOOL_REGISTRY: Record<string, ToolRenderConfig> = {
  Bash: {
    label: "Run command",
    renderInvocation: (input) => <pre className="tool-card tool-card-bash">$ {commandOf(input)}</pre>,
  },
  Read: {
    label: "Read file",
    renderInvocation: (input) => <div className="tool-card">Read: {pathOf(input, "file_path")}</div>,
  },
  Write: {
    label: "Write file",
    renderInvocation: (input) => <div className="tool-card">Write: {pathOf(input, "file_path")}</div>,
  },
  Edit: {
    label: "Edit file",
    renderInvocation: (input) => <div className="tool-card">Edit: {pathOf(input, "file_path")}</div>,
  },
  Grep: {
    label: "Search",
    renderInvocation: (input) => <div className="tool-card">Grep: {pathOf(input, "pattern")}</div>,
  },
  Glob: {
    label: "Find files",
    renderInvocation: (input) => <div className="tool-card">Glob: {pathOf(input, "pattern")}</div>,
  },
  WebFetch: {
    label: "Fetch URL",
    renderInvocation: (input) => <div className="tool-card">Fetch: {pathOf(input, "url")}</div>,
  },
  WebSearch: {
    label: "Web search",
    renderInvocation: (input) => <div className="tool-card">Search: {pathOf(input, "query")}</div>,
  },
};

/** Looks a tool up without consulting `Object.prototype`.
 *
 * `TOOL_REGISTRY[name]` alone is not safe here: a tool named `constructor`, `toString`, `valueOf`
 * or `hasOwnProperty` resolves to an inherited member, which is truthy and has no
 * `renderInvocation` -- so `?.renderInvocation(...)` would not short-circuit, it would throw
 * mid-render, and `App.tsx` has no error boundary, so the whole panel would unmount. Tool names
 * arrive off the provider wire and an MCP server may name its tools anything it likes. */
export function lookupTool(name: string): ToolRenderConfig | undefined {
  return Object.prototype.hasOwnProperty.call(TOOL_REGISTRY, name) ? TOOL_REGISTRY[name] : undefined;
}

/** The one place a tool call's outcome is drawn, for every tool including the unregistered ones.
 *
 * `data-state` carries the three genuinely different situations -- running / done / error -- as a
 * single attribute rather than as a colour a stylesheet could quietly collapse. `result === null`
 * means the provider has not sent a `tool_call_completed` for this `tool_use_id` yet; it is never a
 * completed call with nothing to say, which is why an empty finished result says so in words.
 *
 * `memo` because the panel re-renders on a 33ms pump while a turn streams: without it every
 * finished tool call would re-stringify and re-truncate its whole result thirty times a second for
 * the rest of the conversation. On the EVENT path the skip is real rather than hoped for: the
 * reducer replaces a `result` object exactly once, on `tool_call_completed`, and its `.map`
 * preserves the identity of every entry it does not match. The one documented exception is
 * `applySnapshot`, which replaces the whole state object graph -- so every result re-stringifies
 * and re-truncates once per `UiDelivery::Resync` and once per panel reload. Those are rare and
 * bounded, which is why this is recorded rather than worked around. */
const ToolResult = memo(function ToolResult({ result }: { result: ToolCallRecord["result"] }) {
  if (result === null) {
    return (
      <div className="tool-result tool-result-pending" data-state="running" aria-busy="true">
        Running…
      </div>
    );
  }

  const { shown, hiddenChars } = truncateResult(formatResultContent(result.content));
  return (
    <div className={`tool-result${result.isError ? " tool-result-error" : ""}`} data-state={result.isError ? "error" : "done"}>
      <pre className="tool-result-body">{shown === "" ? "(no output)" : shown}</pre>
      {hiddenChars > 0 && (
        <div className="tool-result-truncated">
          {hiddenChars.toLocaleString("en-US")} characters not shown
        </div>
      )}
    </div>
  );
});

/** `showResult` folded by default, per spec §3.2. A parameter rather than a second exported
 *  renderer: `ToolResult` owns the head/tail truncation and nothing else should grow a copy. */
export function renderToolCall(call: ToolCallRecord, showResult = true): ReactNode {
  // Skill calls are ordinary tool_use blocks with name === "Skill" -- there is no distinct wire
  // event for this (confirmed real behavior, see the agent-v2 spec) -- so this is purely a
  // rendering-layer special case, not something the reducer or Rust side needs to know about.
  const invocation =
    call.name === "Skill" ? (
      <div className="tool-card tool-card-skill">Used skill: {commandOf(call.input)}</div>
    ) : (
      lookupTool(call.name)?.renderInvocation(call.input) ?? (
        // Generic fallback for unrecognized tools.
        <details className="tool-card tool-card-generic">
          <summary>Unrecognized tool: {call.name}</summary>
          <pre>{JSON.stringify(call.input, null, 2)}</pre>
        </details>
      )
    );

  // A call still running has no result to fold -- `result === null` always goes through
  // `ToolResult`, which is what renders the "Running…" indicator. Folding only ever applies to a
  // FINISHED call, which is also why the default `showResult = true` reproduces the pre-fold
  // behaviour exactly: `call.result === null || showResult` is then always true.
  return (
    <div className="tool-call" data-tool-name={call.name}>
      {invocation}
      {call.result === null || showResult ? (
        <ToolResult result={call.result} />
      ) : (
        <div className="tool-result-folded">result folded — Enter to expand</div>
      )}
    </div>
  );
}
