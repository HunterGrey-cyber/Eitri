/** Classifies slash commands and controls whether they can be sent.
 *  `works` means the command has an observable effect; `sent-as-text` means the model answered
 *  the literal words. `no-op`, `error` and `hangs` are held back if added to the table, although
 *  no current entry has one of those classes. */
export type SlashCommandClass = "works" | "sent-as-text" | "no-op" | "error" | "hangs";

export interface SlashCommandEntry {
  /** Command name without the leading slash, in lower case as used by the CLI.
   *  Other spellings miss this table and are sent as unknown commands. */
  name: string;
  class: SlashCommandClass;
}

/** Preserve this order so `worksSlashCommands` gives the `?` overlay a stable command list. */
export const SLASH_COMMANDS: readonly SlashCommandEntry[] = [
  { name: "help", class: "sent-as-text" },
  { name: "clear", class: "works" },
  { name: "compact", class: "works" },
  { name: "cost", class: "works" },
  { name: "context", class: "works" },
  { name: "usage", class: "works" },
  { name: "status", class: "sent-as-text" },
  { name: "model", class: "works" },
  { name: "config", class: "works" },
  { name: "memory", class: "sent-as-text" },
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
  { name: "resume", class: "sent-as-text" },
  { name: "rewind", class: "sent-as-text" },
  { name: "login", class: "sent-as-text" },
  { name: "logout", class: "sent-as-text" },
  { name: "doctor", class: "works" },
  { name: "release-notes", class: "sent-as-text" },
  { name: "output-style", class: "works" },
];

const BY_NAME: ReadonlyMap<string, SlashCommandEntry> = new Map(SLASH_COMMANDS.map((entry) => [entry.name, entry]));

/** Hold these commands back because their terminal pickers cannot be drawn in a headless turn.
 *  `/model` and `/effort` are allowed: their headless replies contain parseable usage text, and
 *  `parseModelReply` and `parseEffortReply` use it to build the panel's `SlashPicker`. Neither
 *  command belongs in this set. */
const INTERACTIVE_ONLY_UNCONDITIONAL: ReadonlySet<string> = new Set(["login", "config", "resume"]);

/** Classes that are always held back, regardless of name. No row
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

/** Returns the blocked command's name without the slash for the flash message, or `null` to send.
 *  Named interactive-only commands and held-back classes are blocked. `/model`, `/effort` and
 *  unknown commands pass, with or without arguments. */
export function heldBackSlashCommand(text: string): string | null {
  const parsed = parseSlashCommand(text);
  if (parsed === null) return null;
  const { name } = parsed;
  if (INTERACTIVE_ONLY_UNCONDITIONAL.has(name)) return name;
  const entry = BY_NAME.get(name);
  if (entry !== undefined && HELD_BACK_CLASSES.has(entry.class)) return name;
  return null;
}

/** Recognize a bare `/model` or `/effort` with no argument so the panel can parse its reply into
 *  `SlashPicker` options. Return `null` for other commands, plain text or commands with
 *  arguments. `App.tsx` watches the next `turn_completed` only for bare commands; the picker's
 *  own choice always sends an argument and must not reopen a picker. */
export function barePickerCommand(text: string): "model" | "effort" | null {
  const parsed = parseSlashCommand(text);
  if (parsed === null || parsed.hasArgument) return null;
  return parsed.name === "model" || parsed.name === "effort" ? parsed.name : null;
}

/** Flash text for a blocked slash command, pointing to `?` for commands that work. */
export function slashCommandFlashText(name: string): string {
  return `/${name} does not work in neovibe — ? lists the ones that do`;
}

/** Lists `works` entries in `SLASH_COMMANDS` order for the `?` overlay.
 *  Classification and holdback are independent: `/model` and `/config` remain in this list
 *  even though sending them is restricted. */
export function worksSlashCommands(): string[] {
  return SLASH_COMMANDS.filter((entry) => entry.class === "works").map((entry) => entry.name);
}

/** Commands shown by the `?` overlay: rows classed **works** that Enter would actually send.
 *  Exclude unconditionally held commands such as `/config` so the list agrees with sending
 *  behavior. `worksSlashCommands` still returns the table's class without filtering holdbacks. */
export function listedSlashCommands(): string[] {
  return worksSlashCommands().flatMap((name) => (heldBackSlashCommand(`/${name}`) === null ? [name] : []));
}
