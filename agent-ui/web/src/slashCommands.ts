import { isHiddenChar } from "./revealHidden";

/** Whether a typed slash command is actually sent, and what the panel says when it is not. The table
 *  below is transcribed by hand from a real-CLI classification of every command (sidecar backend,
 *  CLI 2.1.283, `ANTHROPIC_MODEL=haiku`); nothing here re-derives it.
 *
 *  Five classes: **works** (the command's own effect is observable), **sent-as-text**
 *  (the model answered the literal words), **no-op**, **error**, **hangs**. None of the 28 commands
 *  classified landed in the last three -- they exist here only as a holdback reason, for a future row
 *  this table might gain. */
export type SlashCommandClass = "works" | "sent-as-text" | "no-op" | "error" | "hangs";

export type SlashAction =
  | { kind: "keymap" }
  | { kind: "tab-info" }
  | { kind: "sessions" }
  | { kind: "tasks"; workflows: boolean }
  | { kind: "open-path"; path: string }
  | { kind: "pointer"; text: string }
  /** `/plan`: the person's own move into or out of plan mode, as `<leader>p`. */
  | { kind: "plan" };

export interface SlashCommandEntry {
  /** Without the leading "/", lower-case (the CLI's own commands are lower-case; anything else is
   *  simply not in this table and is sent as an unknown command, which the CLI answers itself). */
  name: string;
  class: SlashCommandClass;
  /** A local action takes precedence over the recorded CLI behavior for a bare word. */
  action?: SlashAction;
}

/** The classification's own row order, preserved here because `worksSlashCommands` below relies on
 *  it (the `?` overlay lists the **works** rows -- this order is what keeps that list stable). */
export const SLASH_COMMANDS: readonly SlashCommandEntry[] = [
  { name: "help", class: "sent-as-text", action: { kind: "keymap" } },
  { name: "clear", class: "works" },
  { name: "compact", class: "works" },
  { name: "cost", class: "works" },
  { name: "context", class: "works" },
  { name: "usage", class: "works" },
  { name: "status", class: "sent-as-text", action: { kind: "tab-info" } },
  { name: "model", class: "works" },
  { name: "config", class: "works" },
  { name: "memory", class: "sent-as-text", action: { kind: "open-path", path: "CLAUDE.md" } },
  { name: "init", class: "sent-as-text" },
  { name: "review", class: "sent-as-text" },
  { name: "security-review", class: "sent-as-text" },
  { name: "pr-comments", class: "sent-as-text" },
  { name: "todos", class: "sent-as-text" },
  { name: "export", class: "sent-as-text" },
  { name: "mcp", class: "works" },
  { name: "agents", class: "works" },
  { name: "hooks", class: "sent-as-text" },
  { name: "permissions", class: "sent-as-text" },
  { name: "add-dir", class: "sent-as-text" },
  { name: "resume", class: "sent-as-text", action: { kind: "sessions" } },
  {
    name: "rewind",
    class: "sent-as-text",
    action: { kind: "pointer", text: "/rewind is not in Eitri — c reviews and reverts this turn's edits" },
  },
  { name: "login", class: "sent-as-text" },
  { name: "logout", class: "sent-as-text" },
  { name: "doctor", class: "works" },
  { name: "release-notes", class: "sent-as-text" },
  { name: "output-style", class: "works" },
];

/** Local task controls postdate the CLI classification above. */
const TASK_COMMANDS: readonly SlashCommandEntry[] = [
  { name: "tasks", class: "sent-as-text", action: { kind: "tasks", workflows: false } },
  { name: "bashes", class: "sent-as-text", action: { kind: "tasks", workflows: false } },
  { name: "workflows", class: "sent-as-text", action: { kind: "tasks", workflows: true } },
];
/** `/plan` postdates the classification too. Sent to the CLI as a message it switches nothing (CLI 2.1.311 stays in
 *  its mode), so a bare `/plan` is the page's own switch, which works only on a Claude tab whose session offers
 *  plan mode. */
const PLAN_COMMANDS: readonly SlashCommandEntry[] = [
  { name: "plan", class: "sent-as-text", action: { kind: "plan" } },
];
const BY_NAME: ReadonlyMap<string, SlashCommandEntry> = new Map([...SLASH_COMMANDS, ...TASK_COMMANDS, ...PLAN_COMMANDS].map((entry) => [entry.name, entry]));

/** Held back regardless of the table's class above, because a real interactive terminal opens a
 *  picker/wizard for these that a headless turn cannot draw -- each degraded to a plain-text
 *  summary instead when it was sent anyway.
 *
 *  Bare `/model` and `/effort` print parseable headless replies (`Current model: ... Usage:
 *  /model <name>. Available: ...` and `Usage: /effort <low|medium|high|xhigh|max|auto>`), so the panel
 *  sends them and builds a picker from the reply (`SlashPicker`, `parseModelReply`/`parseEffortReply`).
 *  Neither belongs in this set. `/effort` has no `SLASH_COMMANDS` row; unknown words pass through. */
const INTERACTIVE_ONLY_UNCONDITIONAL: ReadonlySet<string> = new Set(["login", "config", "resume"]);

/** Classes that are always held back, regardless of name (no-op, error, hangs). No row
 *  in `SLASH_COMMANDS` carries one of these today -- kept as real classes rather than folded away so
 *  a future row classed this way is held back with no further code change. */
const HELD_BACK_CLASSES: ReadonlySet<SlashCommandClass> = new Set(["no-op", "error", "hangs"]);

/** A draft's first word, if it is a slash command: `/name` alone, or `/name` followed by something
 *  else non-whitespace anywhere after it (an "argument" -- what tells `barePickerCommand` below a
 *  bare `/model`/`/effort` apart from `/model sonnet`). Returns `null` for a draft that is not a
 *  slash command at all -- ordinary text, sent as always. */
function parseSlashCommand(text: string): { name: string; hasArgument: boolean } | null {
  const trimmed = text.trimStart();
  if (!trimmed.startsWith("/")) return null;
  const match = /^\/(\S+)/.exec(trimmed);
  if (match === null) return null;
  const name = match[1];
  const rest = trimmed.slice(match[0].length);
  return { name, hasArgument: rest.trim().length > 0 };
}

/** Only a whole draft names a local action; arguments remain the agent's text. */
export function slashAction(text: string): SlashAction | null {
  const parsed = parseSlashCommand(text);
  if (parsed === null || parsed.hasArgument) return null;
  return BY_NAME.get(parsed.name)?.action ?? null;
}

/** Whether Enter must refuse to send this draft. Returns the command name (without the
 *  leading "/", for the flash text below) when held back; `null` means send it as today -- a
 *  **works** or **sent-as-text** command, `/model`/`/effort` (bare or with an argument), and any
 *  other unknown `/word` all fall through to `null` here. */
export function heldBackSlashCommand(text: string): string | null {
  const parsed = parseSlashCommand(text);
  if (parsed === null || slashAction(text) !== null) return null;
  const { name } = parsed;
  if (INTERACTIVE_ONLY_UNCONDITIONAL.has(name)) return name;
  const entry = BY_NAME.get(name);
  if (entry !== undefined && HELD_BACK_CLASSES.has(entry.class)) return name;
  return null;
}

/** Whether `text` is exactly a bare `/model` or `/effort` -- no
 *  argument, nothing else typed -- the shape whose reply this panel now parses into a picker
 *  (`../components/SlashPicker`). Anything else (an argument present, a different command, plain
 *  text) is `null`: `App.tsx` arms the picker's "watch the next turn_completed" state only on this,
 *  never on `/model sonnet` (an ordinary, already-working send) or on the picker's own `onChoose`
 *  send, which always carries an argument and so is never mistaken for a fresh bare command. */
export function barePickerCommand(text: string): "model" | "effort" | null {
  const parsed = parseSlashCommand(text);
  if (parsed === null || parsed.hasArgument) return null;
  return parsed.name === "model" || parsed.name === "effort" ? parsed.name : null;
}

/** A typed `/model <name>` or `/effort <level>`, one word after the command: a setting the session can change
 *  with no turn, which `App.tsx` does during a turn, as Claude Code runs these at once. Anything else is `null`. */
export function settingCommand(text: string): { kind: "model" | "effort"; value: string } | null {
  const match = /^\s*\/(model|effort)\s+(\S+)\s*$/.exec(text);
  return match === null ? null : { kind: match[1] as "model" | "effort", value: match[2] };
}

/** What the band flashes: "`/<name> does not work in Eitri — ? lists the ones that do`", pointing
 *  at the overlay that answers the next question. */
export function slashCommandFlashText(name: string): string {
  return `/${name} does not work in Eitri — ? lists the ones that do`;
}

/** The `?` overlay's "Slash commands" section, listing the commands that work --
 *  exactly the rows classed **works** in `SLASH_COMMANDS`' own order, `/model` and `/config`
 *  included even though both are held back from being sent (the table's class and the holdback
 *  above are deliberately independent: a command can work and still need a picker to send). */
export function worksSlashCommands(): string[] {
  return SLASH_COMMANDS.filter((entry) => entry.class === "works").map((entry) => entry.name);
}

/** What the `?` overlay lists: the **works** rows that Enter would actually send. Listing `/config`
 *  there while Enter refused it with the flash "does not work in <the product> — ? lists the ones
 *  that do" would make the two contradict each other on one screen. A command
 *  held back unconditionally (`/config`, the only one left since a bare `/model` opens a picker instead of being
 *  held back) is left out; `worksSlashCommands` above stays the table's own class, unchanged. */
export function listedSlashCommands(): string[] {
  return worksSlashCommands().flatMap((name) => (heldBackSlashCommand(`/${name}`) === null ? [name] : []));
}

/** One row of the composer's `/` menu: what a user may type after the slash, and one plain line saying
 *  what it does. Text from an agent (`hint` is an ACP command's argument hint) is untrusted, so it is
 *  only ever drawn as text, and `cleanCommands` bounds it on the way in. */
export interface CompletionCommand {
  /** Without the leading "/". */
  name: string;
  description: string;
  hint?: string;
}

/** Which Claude commands the menu lists, besides the **works** rows: `sent-as-text` rows whose reply
 *  is the model acting on the request -- `/init` explored the project, `/review` asked what to review,
 *  `/security-review` ran its own precondition check -- rather than the uniform "isn't available in this
 *  environment" the rest of that class answered. Those
 *  rest-of-the-class rows, and `/pr-comments` and `/todos` (the model said they are not installed), would
 *  complete into a message that does nothing, so the menu leaves them out. */
const LISTED_SENT_AS_TEXT: ReadonlySet<string> = new Set(["init", "review", "security-review"]);

/** One line each, shortened from the CLI's own command reference (code.claude.com/docs/en/commands).
 *  `/effort` has no row in `SLASH_COMMANDS` -- it postdates that table -- but a bare `/effort` is sent and
 *  answered with its usage line (`barePickerCommand`), so it is listed beside `/model`. */
const CLAUDE_DESCRIPTIONS: Readonly<Record<string, string>> = {
  tasks: "Show background tasks",
  bashes: "Show background tasks",
  workflows: "Show dynamic workflows",
  help: "Show Eitri's keys",
  status: "Show this tab's information",
  resume: "Open the sessions chooser",
  memory: "Open the project's CLAUDE.md in the editor",
  rewind: "Show where to review and revert this turn's edits",
  clear: "Start a new conversation with empty context",
  compact: "Free up context by summarizing the conversation so far",
  cost: "Show session cost and plan usage limits",
  context: "Show how full the context window is",
  usage: "Show session cost, plan usage limits and activity",
  model: "Switch the AI model and save it as your default for new sessions",
  effort: "Set the effort level",
  mcp: "Manage MCP server connections",
  agents: "Show how to create and manage subagents",
  doctor: "Check the installation and settings, and propose fixes",
  "output-style": "List output styles or switch to one",
  init: "Initialize the project with a CLAUDE.md guide",
  review: "Review the current diff or a pull request",
  "security-review": "Analyze the branch's changes for security vulnerabilities",
};

/** The argument hints the command reference shows beside these two (`/compact [instructions]`,
 *  `/model [model]`). */
const CLAUDE_HINTS: Readonly<Record<string, string>> = {
  compact: "[instructions]",
  model: "[model]",
};

/** Until the agent reports its list, offer local actions and commands with a useful headless effect.
 *  Terminal-only commands are omitted so completion never leads into a refusal. */
export function builtInCompletions(): CompletionCommand[] {
  const rows = [...SLASH_COMMANDS, ...TASK_COMMANDS].filter(
    (entry) =>
      (entry.action !== undefined || entry.class === "works" || LISTED_SENT_AS_TEXT.has(entry.name)) &&
      heldBackSlashCommand(`/${entry.name}`) === null,
  ).map((entry) => entry.name);
  rows.splice(rows.indexOf("model") + 1, 0, "effort");
  return rows.flatMap((name) =>
    name in CLAUDE_DESCRIPTIONS
      ? [{ name, description: CLAUDE_DESCRIPTIONS[name], ...(name in CLAUDE_HINTS ? { hint: CLAUDE_HINTS[name] } : {}) }]
      : [],
  );
}

/** The bounds an agent's list is held to on the page too (Rust bounds it first): a page that trusted
 *  the producer alone would draw whatever a future producer let through. */
export const MAX_COMPLETIONS = 200;
export const MAX_COMPLETION_NAME = 64;
export const MAX_COMPLETION_TEXT = 200;
/** A hint is drawn beside the name on one line, so it is held shorter than a description. */
export const MAX_COMPLETION_HINT = 60;

function oneLine(text: string, max: number): string {
  const folded = text.replace(/[\p{Cc}\s]+/gu, " ").trim();
  const chars = Array.from(folded);
  return chars.length <= max ? folded : `${chars.slice(0, max - 1).join("")}…`;
}

/** An agent's command list as the page keeps it: only entries with a usable name (non-empty, no
 *  whitespace, control, format or other invisible character -- `revealHidden`'s set, which Rust's
 *  `parse_commands` drops by the same table -- at most `MAX_COMPLETION_NAME` characters, no repeat), their
 *  texts folded to one line and cut (a hint at `MAX_COMPLETION_HINT`, a description at
 *  `MAX_COMPLETION_TEXT`), at most `MAX_COMPLETIONS`. One leading `/` is stripped, and a name still
 *  starting with `/` after that is dropped: Rust has already stripped one, so a second strip here would
 *  read `//x` differently from there. Anything that is not a list is no list. */
export function cleanCommands(raw: unknown): CompletionCommand[] | null {
  if (!Array.isArray(raw)) return null;
  const out: CompletionCommand[] = [];
  for (const item of raw) {
    if (out.length >= MAX_COMPLETIONS) break;
    if (typeof item !== "object" || item === null) continue;
    const { name, description, hint } = item as Record<string, unknown>;
    if (typeof name !== "string") continue;
    const bare = name.startsWith("/") ? name.slice(1) : name;
    if (bare === "" || bare.startsWith("/") || Array.from(bare).length > MAX_COMPLETION_NAME) continue;
    if (/\s/u.test(bare) || Array.from(bare).some(isHiddenChar)) continue;
    if (out.some((c) => c.name === bare)) continue;
    const entry: CompletionCommand = {
      name: bare,
      description: typeof description === "string" ? oneLine(description, MAX_COMPLETION_TEXT) : "",
    };
    if (typeof hint === "string" && oneLine(hint, MAX_COMPLETION_HINT) !== "") entry.hint = oneLine(hint, MAX_COMPLETION_HINT);
    out.push(entry);
  }
  return out;
}

/** Where a tab's `/` menu gets its rows. A list the tab's agent reported (`availableCommands`) wins over
 *  the built-in table: that is how an ACP agent's commands appear, and how a Claude tab will show the
 *  full list -- skills and custom commands included -- once the sidecar forwards the SDK's. A Claude
 *  tab's reported list is still filtered by `heldBackSlashCommand`, because Enter refuses those commands
 *  there and a row that completes into a refused send would be a trap; another agent's list is not
 *  (it sends every draft as typed). Without a list, a Claude tab offers the built-in table; any other
 *  agent (Codex reports none, an ACP agent before its first list) gets no menu, since its commands are
 *  not Claude's. `null` is "no menu". */
export function completionSource(
  agent: string | undefined,
  reported: readonly CompletionCommand[] | null,
): readonly CompletionCommand[] | null {
  const claude = agent === undefined || agent === "claude";
  if (reported !== null) return claude ? reported.filter((c) => heldBackSlashCommand(`/${c.name}`) === null).map((c) =>
    slashAction(`/${c.name}`)?.kind === "tasks" ? { name: c.name, description: CLAUDE_DESCRIPTIONS[c.name] } : c,
  ) : reported;
  if (claude) return builtInCompletions();
  return null;
}

/** The rows for what is typed after the slash: a name equal to it first, then names that start with
 *  it, then names with a word that starts with it (separators ignored, `startsMatch`), then names that
 *  merely contain it -- each group in the source's own order. Case-insensitive. An empty query lists
 *  everything. */
export function matchCompletions(commands: readonly CompletionCommand[], query: string): CompletionCommand[] {
  const needle = query.toLowerCase();
  if (needle === "") return [...commands];
  const exact: CompletionCommand[] = [];
  const prefix: CompletionCommand[] = [];
  const word: CompletionCommand[] = [];
  const inside: CompletionCommand[] = [];
  for (const command of commands) {
    const name = command.name.toLowerCase();
    if (name === needle) exact.push(command);
    else if (name.startsWith(needle)) prefix.push(command);
    else if (startsMatch(command, query)) word.push(command);
    else if (name.includes(needle)) inside.push(command);
  }
  return [...exact, ...prefix, ...word, ...inside];
}

/** Whether Enter may take `command` for what is typed: the typed letters (`query`, the word after the
 *  slash) match the start of its name, or the start of a word within it, ignoring the `:`, `_` and `-`
 *  separators -- Claude Code's own rule for highlighting the top suggestion (code.claude.com/docs/en/commands,
 *  "How the command menu matches what you type"). A name that only contains the letters further in
 *  (`/view` for `review`) stays in the list for Tab and the arrows, but Enter then sends the text as typed,
 *  so a skill the table does not know is never rewritten into another command. An empty query matches
 *  nothing: a bare `/` then Enter is a send. */
export function startsMatch(command: CompletionCommand, query: string): boolean {
  const squash = (text: string) => text.toLowerCase().replace(/[:_-]/g, "");
  const needle = squash(query);
  if (needle === "") return false;
  const name = command.name;
  for (let at = 0; at <= name.length; at += 1) {
    if (at > 0 && !/[:_-]/.test(name[at - 1])) continue;
    if (squash(name.slice(at)).startsWith(needle)) return true;
  }
  return false;
}

/** The part of a draft the `/` menu is about: `{ query, end }` when the draft is one line that starts
 *  with `/` and the caret is inside (or at the end of) its first word, with `end` the index just after
 *  that word. `null` otherwise -- a second line, no slash, or the caret past the word (a space typed,
 *  or moved out), where the menu has nothing to complete. */
export function menuToken(draft: string, caret: number): { query: string; end: number } | null {
  if (!draft.startsWith("/") || draft.includes("\n")) return null;
  const word = /^\/(\S*)/.exec(draft);
  if (word === null) return null;
  const end = word[0].length;
  if (caret < 1 || caret > end) return null;
  return { query: word[1], end };
}

/** How many rows the `/` menu draws at once; a longer list scrolls with the selection. */
export const SLASH_MENU_ROWS = 8;

/** The index of the first row to draw: `top` kept where it is when the selected row is already inside
 *  the window, else moved the least that brings it in, and never past the last full window. */
export function slashMenuWindow(selected: number, top: number, total: number): number {
  const last = Math.max(0, total - SLASH_MENU_ROWS);
  let from = top;
  if (selected < from) from = selected;
  else if (selected >= from + SLASH_MENU_ROWS) from = selected - SLASH_MENU_ROWS + 1;
  return Math.max(0, Math.min(from, last));
}
