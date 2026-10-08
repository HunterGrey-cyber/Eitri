import { memo } from "react";
import type { ReactNode } from "react";
import type { ToolCallDenial, ToolCallRecord } from "./types";
import { codexChangePreview, editPreview, lineDiff } from "./diff";
import { EditDiff } from "./components/EditDiff";
import { Revealed } from "./components/Revealed";
import { subagentOf } from "./externalTools";
import { revealHidden } from "./revealHidden";
import { useProjectRelative } from "./projectPath";

export type ToolRenderConfig = {
  label: string;
  /** A tool whose invocation includes its result inline, such as `ToolSearch`'s muted line,
   *  with no separate result row. */
  quiet?: boolean;
  /** Renders only the invocation: what the model asked the tool to do.
   * `ToolResult` renders results on one shared path so running and finished calls are distinguished
   * consistently across tools. Keep result handling out of unused invocation parameters.
   * `editConfig` uses `opts.expanded` to show the full diff when true and fold it to 6 lines when false. */
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

/** The detailed view (`Ctrl+o`) uses larger head and tail budgets to show more output. */
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

/** `hiddenChars` counts code points (`countCodePoints`); head and tail budgets cap UTF-16 code units.
 * The budgets bound rendered size, while the hidden count reports characters to the reader.
 * `head`/`tail` default to the ordinary fold's budgets. The detailed view (`Ctrl+o`) passes
 * `DETAILED_HEAD_CHARS`/`DETAILED_TAIL_CHARS` for the same truncation with larger limits. */
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

/** A path reachable by `gf` or a click (`MessageList`'s `onClick`, `paths.ts`'s `pathsIn`).
 *  `data-path` carries the raw click target separately from the visible label, which is relative
 *  under the project root. This keeps the display transform from changing which path opens. */
function PathLink({ path }: { path: string }) {
  return (
    <span className="path-link" data-path={path}>
      {useProjectRelative(path)}
    </span>
  );
}

/** One line for a tool without a special view: the first useful string field, or compact JSON,
 *  cut to 120 characters. Avoid "Unrecognized", which reads as an error. */
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

/** `Edit`/`Write`/`NotebookEdit`: a diff (`EditDiff`), folded to a path and counts (`maxLines: 0`)
 *  except in the detailed view or an expanded row. Each tool has its own row, so the default stays compact.
 *  `toolName` is passed to `editPreview` separately from the display label.
 *  Falls back to a path line when `editPreview` returns `null`: no `old_string` or `new_string`,
 *  a `Write` with neither `content` nor a path, or a `NotebookEdit` with neither `new_source` nor `notebook_path`. */
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

/** A string field of a tool input, or `""`. */
function textField(input: unknown, key: string): string {
  if (input && typeof input === "object") {
    const value = (input as Record<string, unknown>)[key];
    if (typeof value === "string") return value;
  }
  return "";
}

/** Text another agent wrote, as a person would read it: hidden and direction-changing characters are drawn as
 *  escapes, the same as on the card that asked about it. */
function Shown({ text }: { text: string }) {
  return <Revealed pieces={revealHidden(text)} />;
}

/** Codex's `commandExecution`: the exact command, shell wrapper included, like a `Bash` row. */
const codexCommand: ToolRenderConfig = {
  label: "Run command",
  renderInvocation: (input) => (
    <pre className="tool-card tool-card-bash">
      $ <Shown text={textField(input, "command") || (JSON.stringify(input) ?? "")} />
    </pre>
  ),
};

/** Codex's `fileChange`: each path with its diff, folded to a path and counts like `Edit` until the row is
 *  expanded. */
const codexFileChange: ToolRenderConfig = {
  label: "Change files",
  previewSuccess: false,
  renderInvocation: (input, { expanded }) => {
    const changes =
      input && typeof input === "object" && Array.isArray((input as { changes?: unknown }).changes)
        ? ((input as { changes: Array<{ path: string; kind: string; diff: string }> }).changes)
        : [];
    if (changes.length === 0) return <div className="tool-card">Change files: {summarizeInput(input)}</div>;
    return (
      <>
        {changes.map((change, i) => (
          <EditDiff key={i} preview={codexChangePreview(change)} maxLines={expanded ? undefined : 0} />
        ))}
      </>
    );
  },
};

const codexMcp: ToolRenderConfig = {
  label: "Tool call",
  renderInvocation: (input) => {
    const args = input && typeof input === "object" ? (input as Record<string, unknown>).arguments : undefined;
    return (
      <div className="tool-card">
        <Shown text={`${textField(input, "server")} · ${textField(input, "tool")}`} />{" "}
        <Shown text={cut(JSON.stringify(args ?? null) ?? "")} />
      </div>
    );
  },
};

const codexWebSearch: ToolRenderConfig = {
  label: "Web search",
  renderInvocation: (input) => (
    <div className="tool-card">
      Search: <Shown text={textField(input, "query") || summarizeInput(input)} />
    </div>
  ),
};

const codexSubagentCall: ToolRenderConfig = {
  label: "Subagent",
  renderInvocation: (input) => (
    <div className="tool-card">
      Subagent {textField(input, "tool")}: <Shown text={cut(textField(input, "prompt"))} />
    </div>
  ),
};

/** An ACP agent's call, named `acp:<kind>`: the title the agent gave it, and a diff for each file it changes. */
function acpConfig(name: string): ToolRenderConfig {
  const kind = name.slice("acp:".length);
  return {
    label: kind,
    renderInvocation: (input, { expanded }) => {
      const title = textField(input, "title");
      const diffs =
        input && typeof input === "object" && Array.isArray((input as { diffs?: unknown }).diffs)
          ? ((input as { diffs: Array<{ path: string; oldText?: string | null; newText?: string }> }).diffs)
          : [];
      return (
        <>
          <div className="tool-card">
            {kind}: <Shown text={title || summarizeInput(input)} />
          </div>
          {diffs.map((diff, i) => {
            const shown = lineDiff(diff.oldText ?? "", diff.newText ?? "");
            const count = (k: "added" | "removed") => shown?.filter((l) => l.kind === k).length ?? 0;
            return (
              <EditDiff
                key={i}
                maxLines={expanded ? undefined : 0}
                preview={{ filePath: diff.path, diff: shown, added: count("added"), removed: count("removed"), replaceAll: false, wholeFile: false }}
              />
            );
          })}
        </>
      );
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
  // Tools are deferred on this CLI, so nearly every action starts with ToolSearch.
  // Show one muted line with no result row or expandable card.
  ToolSearch: {
    label: "Load a tool",
    quiet: true,
    renderInvocation: (input) => <div className="tool-card tool-card-muted">ToolSearch {summarizeInput(input)}</div>,
  },
  // Show a diff of `new_source` against empty text because the tool schema has no "before" field,
  // as handled by `editPreview`.
  NotebookEdit: editConfig("NotebookEdit", "NotebookEdit", "notebook_path"),
  // Ordinary `tool_use` blocks named "Skill" -- no distinct wire event -- so this stays a
  // rendering-layer special case.
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
  // Another agent's calls: Codex's item types, which its provider names its tool calls by.
  commandExecution: codexCommand,
  fileChange: codexFileChange,
  mcpToolCall: codexMcp,
  webSearch: codexWebSearch,
  collabAgentToolCall: codexSubagentCall,
};

/** Looks a tool up without consulting `Object.prototype`.
 *
 * `TOOL_REGISTRY[name]` alone is not safe here: a tool named `constructor`, `toString`, `valueOf`
 * or `hasOwnProperty` resolves to an inherited member, which is truthy and has no
 * `renderInvocation` -- so `?.renderInvocation(...)` would not short-circuit, it would throw
 * mid-render, and `App.tsx` has no error boundary, so the whole panel would unmount. Tool names
 * arrive off the provider wire and an MCP server may name its tools anything it likes. */
export function lookupTool(name: string): ToolRenderConfig | undefined {
  if (name.startsWith("acp:")) return acpConfig(name);
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

/** The same bound for a line drawn wrapped (a refusal's wording, see `ToolResultPreview`): it still ends
 *  somewhere, but a sentence of the CLI's own is never what it cuts. */
const WRAPPED_LINE_CHARS = 4000;

/** The first `limit` lines of `text` and how many lines it has in all, without splitting the whole
 *  string: a build log can be megabytes and this runs for every finished call that is folded.
 *
 *  A line is what a person counts: a final newline ends the last line rather than starting another, a
 *  Windows line ending is one break, and a result with nothing but whitespace has no lines at all. */
export function previewLines(text: string, limit: number, lineChars = PREVIEW_LINE_CHARS): { shown: string[]; total: number } {
  if (!/\S/.test(text)) return { shown: [], total: 0 };
  const shown: string[] = [];
  let newlines = 0;
  let start = 0;
  for (let at = text.indexOf("\n"); at !== -1; at = text.indexOf("\n", start)) {
    if (shown.length < limit) shown.push(cutLine(text.slice(start, at), lineChars));
    newlines++;
    start = at + 1;
  }
  if (start < text.length) {
    if (shown.length < limit) shown.push(cutLine(text.slice(start), lineChars));
    newlines++;
  }
  return { shown, total: newlines };
}

function cutLine(raw: string, chars: number): string {
  const line = raw.endsWith("\r") ? raw.slice(0, -1) : raw;
  return line.length > chars ? `${line.slice(0, headEnd(line, chars - 1))}…` : line;
}

/** What a folded, finished call shows under its invocation: the first `PREVIEW_LINES` of its result, then
 *  a muted count of the lines not shown and how to see them. A failure shows its first line only, in the
 *  error treatment (`.tool-result-error`: the colour on the rule beside it, the text in the body colour --
 *  a signal colour is never a text colour here). Nothing when the result has no lines at all. It never
 *  scrolls and never holds focus: to `j`/`k` it is part of the row.
 *
 *  `wrapped` is for a call the CLI itself refused: the one line is the CLI's own reason for the refusal,
 *  which is the whole point of the row, so it wraps (muted, as the note above it is) instead of being
 *  cut at the track's edge.
 *
 *  `memo` for the reason `ToolResult` is: every finished call would otherwise be split again on each
 *  33ms pump while a turn streams. */
const ToolResultPreview = memo(function ToolResultPreview({
  result,
  wrapped = false,
}: {
  result: NonNullable<ToolCallRecord["result"]>;
  wrapped?: boolean;
}) {
  const { shown, total } = previewLines(formatResultContent(result.content), result.isError ? 1 : PREVIEW_LINES, wrapped ? WRAPPED_LINE_CHARS : PREVIEW_LINE_CHARS);
  if (total === 0) return null;
  const hidden = total - shown.length;
  return (
    <div
      className={`tool-result-preview${result.isError ? " tool-result-error" : ""}${wrapped ? " tool-result-preview-wrapped" : ""}`}
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

/** `ToolResult` owns head/tail truncation so renderers do not duplicate it.
 * `opts.gated` hides invocation details already shown by the pending permission card.
 * Repeating a command or diff above a card asking to run it can make it look approved.
 * `opts.detailed` (`Ctrl+o`) widens the result's cut and never folds it.
 * `opts.expanded` is Enter's per-row toggle, used by `editConfig` to show the rest of a diff.
 * It defaults to `showResult` to preserve behaviour for callers passing only two arguments.
 * `opts.abandoned` tells `ToolResult` that a null result will never arrive.
 * `MessageList.tsx`'s `isAbandonedCall` determines this from state unavailable here. */
export function renderToolCall(
  call: ToolCallRecord,
  showResult = true,
  opts: { gated?: boolean; detailed?: boolean; expanded?: boolean; abandoned?: boolean } = {},
): ReactNode {
  const config = lookupTool(call.name);
  // Pass `opts.detailed` through to `editConfig` so Ctrl+o widens Edit/Write/NotebookEdit diffs
  // as well as `ToolResult`'s head/tail cut, even when the row is not expanded.
  const invocation = opts.gated ? (
    <div className="tool-card tool-card-gated">{call.name} · waiting for approval</div>
  ) : config ? (
    config.renderInvocation(call.input, {
      expanded: (opts.expanded ?? showResult) || opts.detailed === true,
      createsFile: call.createsFile,
    })
  ) : (
    // For tools without a special view, show the name and a one-line input summary.
    // Avoid "Unrecognized", which reads as a panel error.
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

  // A running call (`result === null`) always reaches `ToolResult` to show the running indicator.
  // Only finished results fold. With the default `showResult = true`, all results stay visible.
  // The detailed view also keeps results open regardless of the row's expansion state.
  // A folded result shows its first lines and a remaining-line count through `ToolResultPreview`.
  // It has no separate marker line; `data-folded` exposes the state to `Enter` and tests,
  // and the row itself is the expansion handle. Calls answered by saved prefix rules show a muted
  // note under the invocation so they can be distinguished from calls approved by the user.
  // The note includes the rule in permission-rule syntax, such as `Bash(git log *)`.
  const ruleNote =
    call.allowedByRule === undefined ? null : (
      <div className="tool-rule-note">
        allowed by rule <code>{call.allowedByRule}</code>
      </div>
    );
  // The CLI prompt was answered without a card, either in bypass or through earlier user approval
  // of the same call. Rust supplies the text, shown muted beside the saved-rule note.
  const promptNote =
    call.promptNote === undefined ? null : <div className="tool-rule-note tool-prompt-note">{call.promptNote}</div>;
  // A `Write`/`Edit`/`NotebookEdit` answered by the acceptEdits fast path shows a muted note.
  // Saved rules never apply to these tools, so this note and `ruleNote` cannot both be present.
  const autoNote = call.allowedByAuto === true ? <div className="tool-rule-note">allowed by auto</div> : null;
  // The CLI refused this call on its own -- its auto-mode classifier, most often -- and the model was
  // told so in the call's error result. Muted like the notes above; the CLI's reason verbatim, and
  // the kind of refusal only when it was not the classifier's (a settings deny rule, the mode).
  const deniedNote = call.denied === undefined ? null : <div className="tool-rule-note tool-denied-note">{deniedNoteText(call.denied)}</div>;
  const shown = call.result === null || showResult || opts.detailed === true;
  const subagent = subagentOf(call.name, call.input);
  return (
    <div className="tool-call" data-tool-name={call.name} data-folded={shown ? undefined : "true"}>
      {subagent !== null && (
        <div className="tool-rule-note tool-subagent-note">
          subagent <Shown text={subagent} />
        </div>
      )}
      {invocation}
      {ruleNote}
      {autoNote}
      {promptNote}
      {deniedNote}
      {shown && <ToolResult result={call.result} detailed={opts.detailed === true} abandoned={opts.abandoned === true} />}
      {!shown && call.result !== null && (config?.previewSuccess !== false || call.result.isError) && (
        <ToolResultPreview result={call.result} wrapped={call.denied !== undefined && call.result.isError} />
      )}
    </div>
  );
}
