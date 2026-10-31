import { memo } from "react";
import type { ReactNode } from "react";
import type { InputChange, ToolCallDenial, ToolCallRecord } from "./types";
import { codexChangePreview, editPreview, lineDiff, splitLines } from "./diff";
import { EditDiff } from "./components/EditDiff";
import { HiddenWarning, Revealed } from "./components/Revealed";
import { subagentOf } from "./externalTools";
import { countEscapes, revealHidden } from "./revealHidden";
import { toolCardLine } from "./display";
import { useProjectRelative } from "./projectPath";

export type ToolRenderConfig = {
  label: string;
  /** A tool whose invocation carries its own result inline (`ToolSearch`'s one muted line) and
   *  is never followed by a separate result row. */
  quiet?: boolean;
  /** Renders the INVOCATION only -- what the model asked the tool to do.
   *
   * `ToolResult` renders results on one shared path so running and finished calls remain
   * distinguishable for every tool. A per-tool result view belongs in a separate optional field;
   * invocation renderers must not declare result parameters they do not use.
   *
   * `opts.expanded`: a renderer that draws diffs (`editConfig`, Codex's file changes, an ACP call's
   * diffs) folds them to a summary when it is `false` and shows the whole thing when it is `true`. */
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

/** The detailed view (`Ctrl+o`): the same head+tail scheme, wider, for a reader who asked to see
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
 * `head`/`tail` default to the ordinary fold's budgets; the detailed view (`Ctrl+o`) passes
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

/** A path rendered where `gf`/a click can reach it (`MessageList`'s `onClick`, `paths.ts`'s
 *  `pathsIn`). `data-path` carries the raw string a click reads back, separately from the visible
 *  label, which is project-relative under the project root, so transforming the label cannot
 *  change what a click opens. */
function PathLink({ path }: { path: string }) {
  return (
    <span className="path-link" data-path={path}>
      {useProjectRelative(path)}
    </span>
  );
}

/** One line for a tool this registry has no special view for: the first telling string field, else
 *  compact JSON, cut to 120 characters. Never "Unrecognized": it reads as an error. */
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

/** A request Codex made that Eitri answered itself, drawn as what was asked for: the row's note says how it
 *  was answered and why, and the input names nothing else. */
function answeredRequest(label: string): ToolRenderConfig {
  return { label, renderInvocation: () => <div className="tool-card">{label}</div> };
}

/** A tool this registry recognizes by name only: one muted line, its own summary, nothing more.
 *  Every name in `TOOLS_OFFERED_BY_CLI_2_1_311` this file has no richer view for gets one of these,
 *  so adding a tool to the CLI's list and forgetting to register it here is caught by
 *  `toolRegistry.test.tsx`'s own coverage check, not left to be noticed on a screen. */
function oneLine(label: string): ToolRenderConfig {
  return { label, renderInvocation: (input) => <div className="tool-card">{label}: {summarizeInput(input)}</div> };
}

/** `Edit`/`Write`/`NotebookEdit`: a real diff (`EditDiff`). Folded to a one-line path+counts
 *  summary (`maxLines: 0`) everywhere except the detailed view and an expanded row. Each edit
 *  draws its own row rather than folding into a count-only run, so its default must be compact.
 *  `toolName` names the real tool this input belongs to (`editPreview`'s own first
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
      return <EditDiff preview={preview} maxLines={expanded ? undefined : 0} createsFile={createsFile} tool={toolName} />;
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

/** Whether an input says nothing beyond the subagent that made the call. */
function saysNothing(input: unknown): boolean {
  if (input === null || typeof input !== "object") return input === undefined || input === null;
  return Object.keys(input).every((key) => key === "subagent");
}

/** The host and protocol of a network request's row, when the input names them. */
function networkOf(input: unknown): { host: string; protocol: string } | null {
  const network = input && typeof input === "object" ? (input as Record<string, unknown>).network : undefined;
  if (network === null || typeof network !== "object") return null;
  const { host, protocol } = network as Record<string, unknown>;
  return typeof host === "string" && typeof protocol === "string" ? { host, protocol } : null;
}

/** Codex's `commandExecution`: the exact command, shell wrapper included, like a `Bash` row. A network request
 *  names a host and no command, and a request Eitri answered itself may name neither. */
const codexCommand: ToolRenderConfig = {
  label: "Run command",
  renderInvocation: (input) => {
    const command = textField(input, "command");
    const network = networkOf(input);
    if (command === "" && network !== null)
      return (
        <div className="tool-card">
          network access to <Shown text={network.host} /> (<Shown text={network.protocol} />)
        </div>
      );
    if (command === "" && saysNothing(input)) return <div className="tool-card">command</div>;
    return (
      <pre className="tool-card tool-card-bash">
        $ <Shown text={command || (JSON.stringify(input) ?? "")} />
      </pre>
    );
  },
};

/** Codex's `fileChange`: each path with its diff, folded to a path and counts like `Edit` until the row is
 *  expanded. */
const codexFileChange: ToolRenderConfig = {
  label: "Change files",
  previewSuccess: false,
  renderInvocation: (input, { expanded }) => {
    const changes =
      input && typeof input === "object" && Array.isArray((input as { changes?: unknown }).changes)
        ? (input as { changes: unknown[] }).changes.filter(
            (c): c is { path: string; kind: string; diff: string } => typeof c === "object" && c !== null,
          )
        : [];
    if (changes.length === 0)
      return saysNothing(input) ? (
        <div className="tool-card">Change files</div>
      ) : (
        <div className="tool-card">Change files: {summarizeInput(input)}</div>
      );
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
    if (saysNothing(input)) return <div className="tool-card">tool call</div>;
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
            const count = (k: "added" | "removed") =>
              shown?.filter((l) => l.kind === k).length ?? splitLines(k === "added" ? diff.newText ?? "" : diff.oldText ?? "").length;
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

/** The tools Claude Code 2.1.311 actually offers, matching `agent/src/permission_policy.rs`'s own
 *  `TOOLS_OFFERED_BY_CLI_2_1_311` (`toolRegistry.test.tsx` reads that file as text and checks every
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
  // Deferred on this CLI, so nearly every action reaches this first: keep it to one muted line,
  // with no result row or card of its own worth expanding.
  ToolSearch: {
    label: "Load a tool",
    quiet: true,
    renderInvocation: (input) => <div className="tool-card tool-card-muted">ToolSearch {summarizeInput(input)}</div>,
  },
  // A real diff (its `new_source` against nothing -- the tool's own schema has no
  // "before" field, see `editPreview`'s own doc), not just a one-line summary of the raw input.
  NotebookEdit: editConfig("NotebookEdit", "NotebookEdit", "notebook_path"),
  // Ordinary `tool_use` blocks named "Skill" -- no distinct wire event -- so this stays a
  // rendering-layer special case (kept out of `renderToolCall` itself, which stays tool-agnostic).
  Skill: {
    label: "Skill",
    renderInvocation: (input) => <div className="tool-card tool-card-skill">Used skill: {commandOf(input)}</div>,
  },
  Workflow: oneLine("Workflow"),
  RunWorkflow: oneLine("RunWorkflow"),
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
  // Requests Codex made that Eitri answered itself; their row's note says how.
  permissions: answeredRequest("extra permissions"),
  mcpElicitation: answeredRequest("tool server form"),
  requestUserInput: answeredRequest("question"),
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
    // A `null` result the panel knows can never arrive -- the call predates the
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
    <div className="tool-result" data-state={result.isError ? "error" : "done"}>
      <pre className="tool-result-body">{shown === "" ? "(no output)" : result.isError ? withFailureWords(shown) : shown}</pre>
      {hiddenChars > 0 && (
        <div className="tool-result-truncated">
          {hiddenChars.toLocaleString("en-US")} characters not shown
        </div>
      )}
    </div>
  );
});

/** The words a failed command's text opens with: `Exit code 1` or `Error: Exit code 1` from the agent, or
 *  a background command's own `failed · exit code 1` after the card line's `⎿`. */
const FAILURE_WORDS = /^(⎿ )?((?:Error: )?[Ee]xit code[ :]+-?\d+|failed(?: · exit code -?\d+)?)(?!\w)/;

/** `text` with the words that say a command failed coloured and everything else left as it is; `draw` draws each
 *  part. Text that does not open with those words comes back whole: the row's `✗` says it failed. */
function withFailureWords(text: string, draw: (part: string) => ReactNode = (part) => part): ReactNode {
  const found = FAILURE_WORDS.exec(text);
  if (found === null) return draw(text);
  const lead = found[1] ?? "";
  return (
    <>
      {lead !== "" && draw(lead)}
      <span className="tool-failure-words">{draw(found[2])}</span>
      {draw(text.slice(found[0].length))}
    </>
  );
}

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
 *  a muted count of the lines not shown and how to see them. A failure shows its first line only, in the same
 *  text as any other result, with only the words that say it failed coloured (`withFailureWords`). Nothing when
 *  the result has no lines at all. It never scrolls and never holds focus: to `j`/`k` it is part of the row.
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
      className={`tool-result-preview${wrapped ? " tool-result-preview-wrapped" : ""}`}
      data-state={result.isError ? "error" : "done"}
    >
      {shown.map((line, i) => (
        <div key={i} className="tool-result-preview-line">
          {line === "" ? "\u00a0" : result.isError ? withFailureWords(line) : line}
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

/** What decided a call, in a `ToolCallDenial.reasonType` that is not the Claude CLI's: Eitri itself
 *  answered another agent's request, or Codex's own reviewer passed on it. */
export const ANSWERED_BY_EITRI = "eitri";
export const AUTO_REVIEW_DENIED = "auto_review";
export const AUTO_REVIEW_APPROVED = "auto_review_approved";

/** The row note for a call someone decided without a card. The Claude CLI's refusal reads "blocked by auto:
 *  [Git Destructive]", with the kind of refusal appended when it was not the classifier's ("… (rule)").
 *  A request Eitri answered itself reads "answered by Eitri: <why>", and Codex's reviewer's verdict "blocked
 *  by auto review: <why>" or "allowed by auto review: <why>". */
export function deniedNoteText(denied: ToolCallDenial): string {
  const reason = denied.reason === null || denied.reason.trim() === "" ? "" : `: ${denied.reason}`;
  if (denied.reasonType === ANSWERED_BY_EITRI) return `answered by Eitri${reason}`;
  if (denied.reasonType === AUTO_REVIEW_DENIED) return `blocked by auto review${reason}`;
  if (denied.reasonType === AUTO_REVIEW_APPROVED) return `allowed by auto review${reason}`;
  const kind = denied.reasonType === null || denied.reasonType === "" || denied.reasonType === "classifier" ? "" : ` (${denied.reasonType})`;
  return `blocked by auto${reason}${kind}`;
}

/** The most characters the row shows of a change; Rust already bounds what it sends, this is the
 *  page's own limit for a payload that is not Rust's (a stored record from another build). */
const MAX_CHANGED_NOTE_CHARS = 4000;

/** What the row shows of an input the CLI's own prompt carried instead of the gate's: a `Bash`
 *  command as the shell reads it when that is the whole difference, otherwise only the fields that
 *  differ as compact JSON, then the fields that went away. Cut at `MAX_CHANGED_NOTE_CHARS`. */
export function changedInputText(name: string, change: InputChange): string {
  const fields = change.fields ?? {};
  const removed = change.removed ?? [];
  const keys = Object.keys(fields);
  let text: string;
  // Only a lone changed `command` reads as the command: any other field (a flag a hook added, a
  // dropped one) is part of what changed, so the fields are shown as they are.
  if (name === "Bash" && removed.length === 0 && keys.length === 1 && typeof fields.command === "string") text = fields.command;
  else {
    text = keys.length === 0 ? "" : (JSON.stringify(fields) ?? "");
    if (removed.length > 0) text += `${text === "" ? "" : " "}(no longer present: ${removed.join(", ")})`;
  }
  const chars = Array.from(text);
  if (chars.length > MAX_CHANGED_NOTE_CHARS) text = `${chars.slice(0, MAX_CHANGED_NOTE_CHARS).join("")}… (${chars.length - MAX_CHANGED_NOTE_CHARS} more)`;
  return change.clipped === true ? `${text} (cut)` : text;
}

/** The muted row note for a call whose input differs from what the gate was asked about. It says what
 *  the CLI's own prompt showed, with hidden characters drawn as escapes like every card, and claims
 *  nothing about whether it was then allowed to run. "Differs", not "was changed": only the two
 *  inputs are compared, and why they differ is not on any wire. */
function ChangedInputNote({ name, change }: { name: string; change: InputChange }) {
  const pieces = revealHidden(changedInputText(name, change));
  return (
    <div className="tool-rule-note tool-changed-input-note">
      input differs from what the gate was asked; Claude Code asked about <code><Revealed pieces={pieces} /></code>
      <HiddenWarning count={countEscapes(pieces)} what="input" />
    </div>
  );
}

/** `showResult` folded by default, so a long result does not push the conversation away. A parameter rather than a
 *  second exported renderer: `ToolResult` owns the head/tail truncation and nothing else should grow a copy.
 *
 * `opts.gated`: a card is waiting on this exact call, so the invocation repeats nothing the
 * card already shows -- a `Bash` command or an `Edit`'s diff sitting twice on screen, once above a
 * card asking whether to run it, read as approved already. `opts.detailed` (`Ctrl+o`) widens
 * the result's own cut and never folds it. `opts.expanded` is `Enter`'s own per-row toggle,
 * read by `editConfig` to show a folded diff's remaining lines; it defaults to `showResult` so a
 * caller passing only two arguments gets the same expansion for invocation and result. `opts.abandoned`
 * tells `ToolResult` a `null` result is never coming, rather than still running --
 * `MessageList.tsx`'s `isAbandonedCall` is what decides it, from state this function does not see. */
export function renderToolCall(
  call: ToolCallRecord,
  showResult = true,
  opts: { gated?: boolean; detailed?: boolean; expanded?: boolean; abandoned?: boolean; cardLine?: string | null } = {},
): ReactNode {
  const config = lookupTool(call.name);
  const cardLine = opts.cardLine === undefined ? toolCardLine(call) : opts.cardLine;
  const specialInvocation = cardLine !== null && !["Bash", "Monitor", "Agent", "Task"].includes(call.name);
  // Ctrl+o must widen the invocation diff as well as the result's head/tail cut, even when the
  // row's own expansion flag is false.
  const invocation = opts.gated ? (
    <div className="tool-card tool-card-gated">{call.name} · waiting for approval</div>
  ) : specialInvocation ? (
    <div className="tool-card tool-card-muted">{withFailureWords(cardLine!, (part) => <Shown text={part} />)}</div>
  ) : config ? (
    config.renderInvocation(call.input, {
      expanded: (opts.expanded ?? showResult) || opts.detailed === true,
      createsFile: call.createsFile,
    })
  ) : (
    // Generic fallback for unrecognized tools -- never "Unrecognized": a name and a one-line
    // summary of its input is honest, where that word reads as an error the panel hit.
    <details className="tool-card tool-card-generic">
      <summary>
        {call.name} {summarizeInput(call.input)}
      </summary>
      <pre>{JSON.stringify(call.input, null, 2)}</pre>
    </details>
  );

  const showOrdinaryResult = cardLine === null || (call.result?.isError === true && !["Workflow", "RunWorkflow"].includes(call.name));

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
  // finished call. The detailed view never folds, whatever the row's own expansion says.
  // A folded result shows its first lines and how many more there are (`ToolResultPreview`),
  // as Claude Code's fold does. No separate marker line: `data-folded` supports Enter and tests,
  // and the row itself is the handle.
  // A call a saved prefix rule answered says so, muted, under its invocation -- it
  // otherwise looks exactly like one the user approved. Claude Code names the rule the same way
  // (`Bash(git log *)`, its own permission-rule syntax).
  const ruleNote =
    call.allowedByRule === undefined ? null : (
      <div className="tool-rule-note">
        allowed by rule <code>{call.allowedByRule}</code>
      </div>
    );
  // The CLI's own prompt for this call was answered without
  // a card -- in bypass, or on the user's earlier approval of the same call. Rust writes the words;
  // muted like the rule note, which it sits beside.
  const promptNote =
    call.promptNote === undefined ? null : <div className="tool-rule-note tool-prompt-note">{call.promptNote}</div>;
  // A `Write`/`Edit`/`NotebookEdit` the acceptEdits fast path answered with no
  // card says so, muted, the same way a rule-answered row does -- the two never both apply (no rule
  // ever fires for these three tools, `agent::permission_policy`'s module doc), so this and
  // `ruleNote` are never both non-null for the same call.
  const autoNote = call.allowedByAuto === true ? <div className="tool-rule-note">allowed by auto</div> : null;
  // The CLI refused this call on its own -- its auto-mode classifier, most often -- and the model was
  // told so in the call's error result. Muted like the notes above; the CLI's reason verbatim, and
  // the kind of refusal only when it was not the classifier's (a settings deny rule, the mode).
  const deniedNote = call.denied === undefined ? null : <div className="tool-rule-note tool-denied-note">{deniedNoteText(call.denied)}</div>;
  const changedNote = call.changedInput === undefined ? null : <ChangedInputNote name={call.name} change={call.changedInput} />;
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
      {changedNote}
      {!opts.gated && cardLine !== null && !specialInvocation && <div className="tool-rule-note">{withFailureWords(cardLine, (part) => <Shown text={part} />)}</div>}
      {showOrdinaryResult && shown && <ToolResult result={call.result} detailed={opts.detailed === true} abandoned={opts.abandoned === true} />}
      {showOrdinaryResult && !shown && call.result !== null && (config?.previewSuccess !== false || call.result.isError) && (
        <ToolResultPreview result={call.result} wrapped={call.denied !== undefined && call.result.isError} />
      )}
    </div>
  );
}
