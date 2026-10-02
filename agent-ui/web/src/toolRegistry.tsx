import { memo } from "react";
import type { ReactNode } from "react";
import type { ToolCallDenial, ToolCallRecord } from "./types";
import { editPreview } from "./diff";
import { EditDiff } from "./components/EditDiff";
import { useProjectRelative } from "./projectPath";

export type ToolRenderConfig = {
  label: string;
  /** P2: a tool whose invocation carries its own result inline (`ToolSearch`'s one muted line) and
   *  is never followed by a separate result row. */
  quiet?: boolean;
  /** Renders the INVOCATION only -- what the model asked the tool to do.
   *
   * This used to be `(input, result, isError)`, and all eight registered renderers ignored the last
   * two arguments: a finished Bash call and one still executing produced byte-identical markup, and
   * only an error was ever printed. The result is now rendered by `ToolResult` below, on one shared
   * path for every tool, so "still running" and "finished" cannot end up distinguishable for some
   * tools and not others. A per-tool result view can be added back as a separate optional field the
   * day one is genuinely wanted -- what must not come back is a declared parameter nothing uses.
   *
   * `opts.expanded` is P3's own addition: `editConfig` folds a diff to 6 lines when it is `false`
   * and shows the whole thing when it is `true`, which is the only renderer today that reads it. */
  renderInvocation: (input: unknown, opts: { expanded: boolean; createsFile?: boolean }) => ReactNode;
  /** `false` for a tool whose own row already says what it did (`Edit`/`Write`/`NotebookEdit`: the diff),
   *  so a folded success draws no result preview under it. A failure is previewed whatever this says. */
  previewSuccess?: boolean;
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

/** R3's detailed view (`Ctrl+o`): the same head+tail scheme, wider, for a reader who asked to see
 *  more rather than go find the real output elsewhere. */
export const DETAILED_HEAD_CHARS = 20000;
export const DETAILED_TAIL_CHARS = 5000;

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
 * bounds how much is drawn, the other is what gets reported to a human.
 *
 * `head`/`tail` default to the ordinary fold's budgets; R3's detailed view (`Ctrl+o`) passes
 * `DETAILED_HEAD_CHARS`/`DETAILED_TAIL_CHARS` instead -- same scheme, wider. */
export function truncateResult(
  text: string,
  head = RESULT_HEAD_CHARS,
  tail = RESULT_TAIL_CHARS,
): { shown: string; hiddenChars: number } {
  if (text.length <= head + tail) return { shown: text, hiddenChars: 0 };
  const headPart = text.slice(0, headEnd(text, head));
  const tailPart = text.slice(tailStart(text, tail));
  const elided = text.slice(headPart.length, text.length - tailPart.length);
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
  return { shown: `${headPart}${marker}${tailPart}`, hiddenChars };
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

/** N2: a path rendered where `gf`/a click can reach it (`MessageList`'s `onClick`, `paths.ts`'s
 *  `pathsIn`). `data-path` carries the raw string a click reads back, separately from the visible
 *  label, which is project-relative under the project root since v1 polish F21 -- the two were kept
 *  apart on purpose so that transform could not break what a click opens. */
function PathLink({ path }: { path: string }) {
  return (
    <span className="path-link" data-path={path}>
      {useProjectRelative(path)}
    </span>
  );
}

/** One line for a tool this registry has no special view for: the first telling string field, else
 *  compact JSON, cut to 120 characters. Never "Unrecognized" (P2): it reads as an error. */
export function summarizeInput(input: unknown): string {
  if (input && typeof input === "object") {
    const fields = input as Record<string, unknown>;
    for (const key of ["description", "prompt", "query", "command", "file_path", "path", "url", "pattern", "name", "task_id", "id"]) {
      if (typeof fields[key] === "string") return cut(fields[key] as string);
    }
  }
  return cut(JSON.stringify(input) ?? "");
}

function cut(text: string): string {
  const line = text.replace(/\s+/g, " ").trim();
  return line.length > 120 ? `${line.slice(0, 119)}…` : line;
}

/** A tool this registry recognizes by name only: one muted line, its own summary, nothing more.
 *  Every name in `TOOLS_OFFERED_BY_CLI_2_1_272` this file has no richer view for gets one of these,
 *  so "not in the table" (the 2026-09-19 defect: 18 of 26 real tools carded as unknown) cannot
 *  recur -- adding a tool to the CLI's own list and forgetting to register it here is caught by
 *  `toolRegistry.test.tsx`'s own coverage check, not left to be noticed on a screen. */
function oneLine(label: string): ToolRenderConfig {
  return { label, renderInvocation: (input) => <div className="tool-card">{label}: {summarizeInput(input)}</div> };
}

/** `Edit`/`Write`/`NotebookEdit`: a real diff (`EditDiff`). Folded to a one-line path+counts
 *  summary (`maxLines: 0`) everywhere except the detailed view and an expanded row (P3; v1 trial
 *  item 7 lowered this from a 6-line fold once these three tools stopped ever folding into a
 *  count-only run -- every one of them now draws its own row, so each has to be this compact by
 *  default). `toolName` names the real tool this input belongs to (`editPreview`'s own first
 *  argument), since `Write file` and `NotebookEdit` are display labels, not tool names. Falls back
 *  to a plain path line when the input carries no reviewable change (neither `old_string` nor
 *  `new_string`, `Write` with neither `content` nor a path, or `NotebookEdit` with neither
 *  `new_source` nor `notebook_path`) -- `editPreview` returning `null` is what decides that, not
 *  this function. */
function editConfig(label: string, toolName: string, pathField = "file_path"): ToolRenderConfig {
  return {
    label,
    previewSuccess: false,
    renderInvocation: (input, { expanded, createsFile }) => {
      const preview = editPreview(toolName, input);
      if (preview === null)
        return (
          <div className="tool-card">
            {label}: <PathLink path={pathOf(input, pathField)} />
          </div>
        );
      return <EditDiff preview={preview} maxLines={expanded ? undefined : 0} createsFile={createsFile} />;
    },
  };
}

/** The tools Claude Code 2.1.27x actually offers, matching `agent/src/permission_policy.rs`'s own
 *  `TOOLS_OFFERED_BY_CLI_2_1_272` (`toolRegistry.test.tsx` reads that file as text and checks every
 *  name here). `Grep`/`Glob`/`WebFetch`/`WebSearch` predate that list and are kept for a build that
 *  does carry them; `ToolSearch`, `Edit`, `Write`, `NotebookEdit` and `Skill` get their own view
 *  below; everything else gets `oneLine` -- a real tool the model called always says its name and a
 *  one-line summary, never "Unrecognized". */
export const TOOL_REGISTRY: Record<string, ToolRenderConfig> = {
  Bash: {
    label: "Run command",
    renderInvocation: (input) => <pre className="tool-card tool-card-bash">$ {commandOf(input)}</pre>,
  },
  Read: {
    label: "Read file",
    renderInvocation: (input) => (
      <div className="tool-card">
        Read: <PathLink path={pathOf(input, "file_path")} />
      </div>
    ),
  },
  Write: editConfig("Write file", "Write"),
  Edit: editConfig("Edit file", "Edit"),
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
  // P2: deferred on this CLI, so nearly every action the agent takes reaches this one first (the
  // 2026-09-19 defect that made "auto mode 还是一堆弹窗" once the policy started asking about it) --
  // one muted line, no result row, never a card of its own worth expanding.
  ToolSearch: {
    label: "Load a tool",
    quiet: true,
    renderInvocation: (input) => <div className="tool-card tool-card-muted">ToolSearch {summarizeInput(input)}</div>,
  },
  // v1 trial item 7: a real diff (its `new_source` against nothing -- the tool's own schema has no
  // "before" field, see `editPreview`'s own doc), not just a one-line summary of the raw input.
  NotebookEdit: editConfig("NotebookEdit", "NotebookEdit", "notebook_path"),
  // Ordinary `tool_use` blocks named "Skill" -- no distinct wire event -- so this stays a
  // rendering-layer special case (kept out of `renderToolCall` itself since Task 13, review draft).
  Skill: {
    label: "Skill",
    renderInvocation: (input) => <div className="tool-card tool-card-skill">Used skill: {commandOf(input)}</div>,
  },
  Task: oneLine("Task"),
  // CLI 2.1.283+ spells the subagent tool `Agent` (the same tool, renamed -- see
  // `agent/src/permission_policy.rs`'s `NEVER_ASK_TOOLS`); `Task` stays for an older CLI.
  Agent: oneLine("Agent"),
  CronCreate: oneLine("CronCreate"),
  CronDelete: oneLine("CronDelete"),
  CronList: oneLine("CronList"),
  DesignSync: oneLine("DesignSync"),
  EnterWorktree: oneLine("EnterWorktree"),
  ExitWorktree: oneLine("ExitWorktree"),
  ListAgents: oneLine("ListAgents"),
  LSP: oneLine("LSP"),
  Monitor: oneLine("Monitor"),
  PushNotification: oneLine("PushNotification"),
  RemoteTrigger: oneLine("RemoteTrigger"),
  ReportFindings: oneLine("ReportFindings"),
  ScheduleWakeup: oneLine("ScheduleWakeup"),
  SendMessage: oneLine("SendMessage"),
  TaskOutput: oneLine("TaskOutput"),
  TaskStop: oneLine("TaskStop"),
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
const ToolResult = memo(function ToolResult({
  result,
  detailed = false,
  abandoned = false,
}: {
  result: ToolCallRecord["result"];
  detailed?: boolean;
  abandoned?: boolean;
}) {
  if (result === null) {
    // sw-panel-render-6: a `null` result the panel knows can never arrive -- the call predates the
    // restored-history boundary, or the session has ended -- draws a static notice instead of the
    // spinner below. `data-state="none"` (not "running") and no `aria-busy`: nothing is in flight.
    if (abandoned) {
      return (
        <div className="tool-result tool-result-abandoned" data-state="none">
          no result recorded
        </div>
      );
    }
    return (
      <div className="tool-result tool-result-pending" data-state="running" aria-busy="true">
        Running…
      </div>
    );
  }

  const { shown, hiddenChars } = detailed
    ? truncateResult(formatResultContent(result.content), DETAILED_HEAD_CHARS, DETAILED_TAIL_CHARS)
    : truncateResult(formatResultContent(result.content));
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

/** How many lines of a folded result its row shows (Claude Code's own fold shows three). */
export const PREVIEW_LINES = 3;

/** The longest a previewed line is drawn, in UTF-16 units: a runaway line (a minified file, a base64 blob)
 *  is cut here with an ellipsis, not put into the page whole for CSS to clip. */
const PREVIEW_LINE_CHARS = 400;

/** The first `limit` lines of `text` and how many lines it has in all, without splitting the whole
 *  string: a build log can be megabytes and this runs for every finished call that is folded.
 *
 *  A line is what a person counts: a final newline ends the last line rather than starting another, a
 *  Windows line ending is one break, and a result with nothing but whitespace has no lines at all. */
export function previewLines(text: string, limit: number): { shown: string[]; total: number } {
  if (!/\S/.test(text)) return { shown: [], total: 0 };
  const shown: string[] = [];
  let newlines = 0;
  let start = 0;
  for (let at = text.indexOf("\n"); at !== -1; at = text.indexOf("\n", start)) {
    if (shown.length < limit) shown.push(cutLine(text.slice(start, at)));
    newlines++;
    start = at + 1;
  }
  if (start < text.length) {
    if (shown.length < limit) shown.push(cutLine(text.slice(start)));
    newlines++;
  }
  return { shown, total: newlines };
}

function cutLine(raw: string): string {
  const line = raw.endsWith("\r") ? raw.slice(0, -1) : raw;
  return line.length > PREVIEW_LINE_CHARS ? `${line.slice(0, headEnd(line, PREVIEW_LINE_CHARS - 1))}…` : line;
}

/** What a folded, finished call shows under its invocation: the first `PREVIEW_LINES` of its result, then
 *  a muted count of the lines not shown and how to see them. A failure shows its first line only, in the
 *  error treatment (`.tool-result-error`: the colour on the rule beside it, the text in the body colour --
 *  a signal colour is never a text colour here). Nothing when the result has no lines at all. It never
 *  scrolls and never holds focus: to `j`/`k` it is part of the row.
 *
 *  `memo` for the reason `ToolResult` is: every finished call would otherwise be split again on each
 *  33ms pump while a turn streams. */
const ToolResultPreview = memo(function ToolResultPreview({ result }: { result: NonNullable<ToolCallRecord["result"]> }) {
  const { shown, total } = previewLines(formatResultContent(result.content), result.isError ? 1 : PREVIEW_LINES);
  if (total === 0) return null;
  const hidden = total - shown.length;
  return (
    <div
      className={`tool-result-preview${result.isError ? " tool-result-error" : ""}`}
      data-state={result.isError ? "error" : "done"}
    >
      {shown.map((line, i) => (
        <div key={i} className="tool-result-preview-line">
          {line === "" ? "\u00a0" : line}
        </div>
      ))}
      {hidden > 0 && (
        <div className="tool-result-preview-more">
          {`… +${hidden.toLocaleString("en-US")} ${hidden === 1 ? "line" : "lines"} (Enter to expand, Ctrl+o for all)`}
        </div>
      )}
    </div>
  );
});

/** The row note for a call the CLI refused: "blocked by auto: [Git Destructive]", with the kind of
 *  refusal appended when it was not the classifier's ("… (rule)"). */
export function deniedNoteText(denied: ToolCallDenial): string {
  const reason = denied.reason === null || denied.reason.trim() === "" ? "" : `: ${denied.reason}`;
  const kind = denied.reasonType === null || denied.reasonType === "" || denied.reasonType === "classifier" ? "" : ` (${denied.reasonType})`;
  return `blocked by auto${reason}${kind}`;
}

/** `showResult` folded by default, per spec §3.2. A parameter rather than a second exported
 *  renderer: `ToolResult` owns the head/tail truncation and nothing else should grow a copy.
 *
 * `opts.gated` (P4): a card is waiting on this exact call, so the invocation repeats nothing the
 * card already shows -- a `Bash` command or an `Edit`'s diff sitting twice on screen, once above a
 * card asking whether to run it, read as approved already. `opts.detailed` (R3, `Ctrl+o`) widens
 * the result's own cut and never folds it. `opts.expanded` (P3) is `Enter`'s own per-row toggle,
 * read by `editConfig` to show a folded diff's remaining lines; it defaults to `showResult` so a
 * caller that only ever passed the old two arguments keeps the old behaviour exactly. `opts.abandoned`
 * (sw-panel-render-6) tells `ToolResult` a `null` result is never coming, rather than still running --
 * `MessageList.tsx`'s `isAbandonedCall` is what decides it, from state this function does not see. */
export function renderToolCall(
  call: ToolCallRecord,
  showResult = true,
  opts: { gated?: boolean; detailed?: boolean; expanded?: boolean; abandoned?: boolean } = {},
): ReactNode {
  const config = lookupTool(call.name);
  // Fix round finding 4: `opts.detailed` (Ctrl+o) used to reach only `ToolResult`'s own head/tail
  // cut, never `editConfig`'s `maxLines`, so a folded (unexpanded) Edit/Write/NotebookEdit row
  // stayed folded even in the detailed view -- silently, since `EditDiff`'s own doc already
  // promised "everywhere except the detailed view and an expanded row" (`editConfig`'s comment
  // above). `Ctrl+o` now widens a diff exactly as it widens a `Bash`/`Read` result.
  const invocation = opts.gated ? (
    <div className="tool-card tool-card-gated">{call.name} · waiting for approval</div>
  ) : config ? (
    config.renderInvocation(call.input, {
      expanded: (opts.expanded ?? showResult) || opts.detailed === true,
      createsFile: call.createsFile,
    })
  ) : (
    // Generic fallback for unrecognized tools -- never "Unrecognized" (P2): a name and a one-line
    // summary of its input is honest, where that word reads as an error the panel hit.
    <details className="tool-card tool-card-generic">
      <summary>
        {call.name} {summarizeInput(call.input)}
      </summary>
      <pre>{JSON.stringify(call.input, null, 2)}</pre>
    </details>
  );

  // A quiet tool (`ToolSearch`) never gets a separate result row: its own line is the whole story.
  if (config?.quiet) {
    return (
      <div className="tool-call" data-tool-name={call.name}>
        {invocation}
      </div>
    );
  }

  // A call still running has no result to fold -- `result === null` always goes through
  // `ToolResult`, which is what renders the "Running…" indicator. Folding only ever applies to a
  // FINISHED call, which is also why the default `showResult = true` reproduces the pre-fold
  // behaviour exactly: `call.result === null || showResult` is then always true. The detailed view
  // (R3) never folds either, whatever the row's own expansion says.
  // A folded result draws no marker line of its own (v1 polish F21: it used to be a line holding only `▸`,
  // under every finished call). It shows its first lines and how many more there are instead
  // (`ToolResultPreview`), as Claude Code's fold does. `data-folded` says it is folded, for `Enter` and
  // for tests; the row itself is the handle.
  // v1 polish F18: a call a saved prefix rule answered says so, muted, under its invocation -- it
  // otherwise looked exactly like one the user approved. Claude Code names the rule the same way
  // (`Bash(git log *)`, its own permission-rule syntax).
  const ruleNote =
    call.allowedByRule === undefined ? null : (
      <div className="tool-rule-note">
        allowed by rule <code>{call.allowedByRule}</code>
      </div>
    );
  // O3 review item 7 (the spike's row note): the CLI's own prompt for this call was answered without
  // a card -- in bypass, or on the user's earlier approval of the same call. Rust writes the words;
  // muted like the rule note, which it sits beside.
  const promptNote =
    call.promptNote === undefined ? null : <div className="tool-rule-note tool-prompt-note">{call.promptNote}</div>;
  // v1 trial item 7: a `Write`/`Edit`/`NotebookEdit` the acceptEdits fast path answered with no
  // card says so, muted, the same way a rule-answered row does -- the two never both apply (no rule
  // ever fires for these three tools, `agent::permission_policy`'s module doc), so this and
  // `ruleNote` are never both non-null for the same call.
  const autoNote = call.allowedByAuto === true ? <div className="tool-rule-note">allowed by auto</div> : null;
  // The CLI refused this call on its own -- its auto-mode classifier, most often -- and the model was
  // told so in the call's error result. Muted like the notes above; the CLI's reason verbatim, and
  // the kind of refusal only when it was not the classifier's (a settings deny rule, the mode).
  const deniedNote = call.denied === undefined ? null : <div className="tool-rule-note tool-denied-note">{deniedNoteText(call.denied)}</div>;
  const shown = call.result === null || showResult || opts.detailed === true;
  return (
    <div className="tool-call" data-tool-name={call.name} data-folded={shown ? undefined : "true"}>
      {invocation}
      {ruleNote}
      {autoNote}
      {promptNote}
      {deniedNote}
      {shown && <ToolResult result={call.result} detailed={opts.detailed === true} abandoned={opts.abandoned === true} />}
      {!shown && call.result !== null && (config?.previewSuccess !== false || call.result.isError) && (
        <ToolResultPreview result={call.result} />
      )}
    </div>
  );
}
